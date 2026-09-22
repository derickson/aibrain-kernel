//! aibrain-core — watches the vaults, keeps Postgres in step, serves the corpus.

mod api;
mod config;
mod db;
mod es;
mod graph;
mod ingest;
mod layout;
mod todo;
mod vault;
mod watch;

use anyhow::Result;
use std::path::PathBuf;

const DEFAULT_DATABASE_URL: &str = "postgres://aibrain:aibrain@127.0.0.1:5433/aibrain";

fn usage() -> ! {
    eprintln!(
        "aibrain-core

  serve [--watch]     serve the corpus on AIBRAIN_BIND (default 127.0.0.1:8781)
  reindex [--force]   rescan every linked vault into Postgres
  resync-search       queue every note for Elasticsearch (backfill)
  status              what the database currently holds

Environment:
  AIBRAIN_DATABASE_URL     default {DEFAULT_DATABASE_URL}
  AIBRAIN_HOME             where config.json lives (default ~/.aibrain)
  ELASTICSEARCH_URL        unset means search stays on Postgres
  ELASTICSEARCH_API_KEY    sent as `Authorization: ApiKey ...`
  AIBRAIN_ES_INFERENCE_ID  default {es_default_inference}
  AIBRAIN_ES_INDEX_PREFIX  default {es_default_prefix}
  AIBRAIN_ES_BATCH         documents per bulk (default {es_default_batch})
  AIBRAIN_ES_POLL_MS       idle poll interval (default {es_default_poll})
  AIBRAIN_TODO_TEST_CLOCK  1 lets every /todos route take `?now=<timestamp>`
                           instead of the wall clock, so a test can roll the
                           day forward. Unset in normal use.

A .env in this directory or any parent is loaded first; real environment
variables win over it.
",
        es_default_inference = es::DEFAULT_INFERENCE_ID,
        es_default_prefix = es::DEFAULT_INDEX_PREFIX,
        es_default_batch = es::DEFAULT_BATCH,
        es_default_poll = es::DEFAULT_POLL_MS,
    );
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Before anything reads the environment. dotenvy searches upward, so a
    // repo-root .env is found whether cargo was run from the root or rust/.
    // It never overwrites a variable that is already set.
    let dotenv = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AIBRAIN_LOG")
                .unwrap_or_else(|_| "aibrain_core=info,sqlx=warn".into()),
        )
        .with_target(false)
        .init();

    match dotenv {
        Ok(path) => tracing::debug!("loaded {}", path.display()),
        Err(err) if err.not_found() => {}
        Err(err) => tracing::warn!("could not read .env: {err}"),
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("status");
    let url = std::env::var("AIBRAIN_DATABASE_URL")
        .unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());
    let config_path: PathBuf = config::default_path();

    let pool = db::connect(&url).await?;

    match command {
        "reindex" => {
            let cfg = config::load(&config_path)?;
            if cfg.brains.is_empty() {
                println!("no vaults are linked in obsidian_vaults/ — nothing to index");
                return Ok(());
            }
            let force = args.iter().any(|a| a == "--force");
            let started = std::time::Instant::now();
            let stats = ingest::reindex(&pool, &cfg.brains, force, |line| println!("{line}")).await?;
            println!(
                "done in {:.1}s — {} notes (+{} ~{} -{} ={}), {} links",
                started.elapsed().as_secs_f32(),
                stats.scanned,
                stats.added,
                stats.updated,
                stats.removed,
                stats.unchanged,
                stats.links
            );
        }
        "serve" => {
            let cfg = config::load(&config_path)?;
            let search = match es::EsConfig::from_env() {
                None => {
                    tracing::info!("ELASTICSEARCH_URL is unset — search stays on postgres");
                    None
                }
                Some(es_cfg) => {
                    tracing::info!(
                        "elasticsearch at {} — indices {}*, inference {}",
                        db::redact(&es_cfg.url),
                        es_cfg.index_prefix,
                        es_cfg.inference_id
                    );
                    Some(std::sync::Arc::new(es::Es::new(es_cfg)?))
                }
            };
            let ctx = std::sync::Arc::new(api::Ctx {
                pool: pool.clone(),
                config_path: config_path.clone(),
                revision: std::sync::atomic::AtomicI64::new(0),
                es: search.clone(),
            });

            // A cold database is useless to the UI, so fill it before listening
            // rather than serving an empty universe and hoping.
            if db::count_notes(&pool).await? == 0 && !cfg.brains.is_empty() {
                tracing::info!("index is empty — scanning first");
                ingest::reindex(&pool, &cfg.brains, false, |l| tracing::info!("{l}")).await?;
            }

            if let Some(search) = &search {
                if let Err(err) = es::worker::reconcile(&pool, search).await {
                    tracing::warn!("could not reconcile with elasticsearch: {err:#}");
                }
                es::worker::spawn(pool.clone(), search.clone());
            }

            if args.iter().any(|a| a == "--watch") {
                watch::spawn(ctx.clone());
            }

            let bind = std::env::var("AIBRAIN_BIND")
                .unwrap_or_else(|_| "127.0.0.1:8781".to_string());
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!("serving {} notes on http://{bind}", db::count_notes(&pool).await?);
            axum::serve(listener, api::router(ctx)).await?;
        }
        "resync-search" => {
            let enqueued = db::enqueue_all(&pool, None).await?;
            println!("queued {enqueued} note(s) for search indexing");
            if es::EsConfig::from_env().is_none() {
                println!("ELASTICSEARCH_URL is unset — the queue will wait until it is set");
            }
        }
        "status" => {
            let brains = db::list_brains(&pool).await?;
            println!("{} notes, {} links", db::count_notes(&pool).await?,
                     db::count_links(&pool).await?);
            for brain in brains {
                println!("  {:20} {:>6} notes  rev {}  {}",
                         brain.name, brain.note_count, brain.revision, brain.root);
            }
            let queue = db::queue_stats(&pool).await?;
            match es::EsConfig::from_env() {
                Some(cfg) => println!(
                    "search: elasticsearch ({}), queue {} pending / {} failing",
                    cfg.inference_id, queue.pending, queue.failing
                ),
                None => println!("search: postgres, {} queued for later", queue.pending),
            }
        }
        _ => usage(),
    }
    Ok(())
}
