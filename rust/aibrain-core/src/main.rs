//! aibrain-core — watches the vaults, keeps Postgres in step, serves the corpus.

mod api;
mod config;
mod db;
mod graph;
mod ingest;
mod layout;
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
  status              what the database currently holds

Environment:
  AIBRAIN_DATABASE_URL   default {DEFAULT_DATABASE_URL}
  AIBRAIN_HOME           where config.json lives (default ~/.aibrain)
"
    );
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AIBRAIN_LOG")
                .unwrap_or_else(|_| "aibrain_core=info,sqlx=warn".into()),
        )
        .with_target(false)
        .init();

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
            let ctx = std::sync::Arc::new(api::Ctx {
                pool: pool.clone(),
                config_path: config_path.clone(),
                revision: std::sync::atomic::AtomicI64::new(0),
            });

            // A cold database is useless to the UI, so fill it before listening
            // rather than serving an empty universe and hoping.
            if db::count_notes(&pool).await? == 0 && !cfg.brains.is_empty() {
                tracing::info!("index is empty — scanning first");
                ingest::reindex(&pool, &cfg.brains, false, |l| tracing::info!("{l}")).await?;
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
        "status" => {
            let brains = db::list_brains(&pool).await?;
            println!("{} notes, {} links", db::count_notes(&pool).await?,
                     db::count_links(&pool).await?);
            for brain in brains {
                println!("  {:20} {:>6} notes  rev {}  {}",
                         brain.name, brain.note_count, brain.revision, brain.root);
            }
        }
        _ => usage(),
    }
    Ok(())
}
