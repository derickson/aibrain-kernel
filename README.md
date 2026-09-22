# aibrain-kernel

A local app for the knowledge you already have. Your Obsidian vaults are read
into a corpus and drawn as galaxies — one per vault, each note a star, each
wikilink a chord between them. Agents float in the same space: double-click
one and ask it something, and the notes it answers from light up and open.

Nothing leaves the machine unless you connect an agent that goes somewhere
else, or you turn on Elasticsearch search.

```sh
./dev.sh
```

That starts Postgres, the Rust service that owns the corpus, and the Python
UI, in that order, and opens <http://127.0.0.1:8760/>.

<!-- TODO: a screenshot of the universe belongs here -->

## Architecture

Three processes, one owner per job:

```
browser ──► python (aibrain/, :8760) ──► rust (aibrain-core, :8781) ──► postgres (:5433)
                                                                     └─► elasticsearch (optional)
```

- **Postgres** (`db` in `docker-compose.yml`, port 5433) holds every note,
  link, the to-do shelf, and a queue of documents waiting to be indexed for
  search. It is the one schema owner.
- **`rust/aibrain-core`** scans the linked vaults into Postgres, watches them
  with FSEvents for live changes, computes the galaxy layout, renders
  markdown, and serves it all over HTTP on `AIBRAIN_BIND` (default
  `127.0.0.1:8781`). A background worker drains the search queue into
  Elasticsearch when one is configured. It never writes `config.json`.
- **`aibrain/`** (Python 3, standard library only — no pip installs) serves
  `web/` and a thin `/api/*` proxy in front of the Rust service. It owns
  `config.json`, the agents, the job runner, and nothing about the corpus
  itself: `aibrain/corpus.py` is the only place that talks to Rust, over
  `urllib`.

Python refuses to start if Rust is not answering `/health` — there is no
local fallback to degrade to.

## Install

- **Rust toolchain** — `cargo` and `rustc` 1.82 or newer (edition 2021).
  Install via [rustup](https://rustup.rs).
- **Docker or Podman**, for Postgres. `docker compose` (or `docker-compose`)
  must be on `PATH`.
- **Python 3**, standard library only — no virtualenv, no pip installs.

```sh
git clone <this repo>
cd aibrain-kernel
./dev.sh
```

`dev.sh` runs `docker compose up -d db`, waits for Postgres to answer,
then starts `cargo run -- serve --watch` and `python3 -m aibrain` and stops
the two it started on Ctrl-C (the database keeps running — it is cheap, and
stopping it would throw away the page cache on every restart).

Postgres is the only thing containerised in dev, on purpose: the watcher has
to see FSEvents for files on your Mac, and neither podman nor Docker Desktop
forwards filesystem events from a host mount into the VM. A watcher running
in a container would sit silent while you edited notes. The `full`
docker-compose profile exists for a host where the vault lives inside the
container's own filesystem, where that limitation does not apply — see
`docker-compose.yml`.

### Brains

A brain is an Obsidian vault, and a vault becomes a brain by being symlinked
into `obsidian_vaults/`. That folder is the allowlist — Rust only scans a
vault because its `path` field in `config.json` points there, and the config
`path` only gets there because the UI (or you) put a symlink in
`obsidian_vaults/`.

```sh
ln -s ~/Documents/ObsidianVaults/Home obsidian_vaults/Home
```

Use an absolute path. A relative one is resolved from inside
`obsidian_vaults/`, which is two levels below the repo, and getting that
wrong produces a dangling link that silently yields no vault. The **Brains**
tab creates and removes these links for you, and reports any that do not
resolve. Removing a brain removes only the link; the vault itself is never
touched.

## Environment variables

Rust reads these directly from the environment or from a `.env` file (see
below). Real environment variables always win over `.env`.

| Variable | Default | Read by | What it does |
|---|---|---|---|
| `AIBRAIN_DATABASE_URL` | `postgres://aibrain:aibrain@127.0.0.1:5433/aibrain` | Rust | Postgres connection string. |
| `AIBRAIN_HOME` | `~/.aibrain` | Rust and Python | Where `config.json` lives. |
| `AIBRAIN_BIND` | `127.0.0.1:8781` | Rust | Address `aibrain-core serve` listens on. |
| `AIBRAIN_LOG` | `aibrain_core=info,sqlx=warn` | Rust | `tracing_subscriber` env filter. |
| `AIBRAIN_CORE_URL` | `http://127.0.0.1:8781` | Python (`aibrain/corpus.py`) and the MCP server | Where the Rust service is, for anything that talks to it over HTTP. |
| `AIBRAIN_TODO_TEST_CLOCK` | unset | Rust | `1` lets every `/todos` route take a `?now=` override instead of the wall clock, so tests can roll the day forward. Never set this in normal use. |
| `ELASTICSEARCH_URL` | unset | Rust | Elasticsearch endpoint. Unset means search stays on Postgres. |
| `ELASTICSEARCH_API_KEY` | unset | Rust | Sent as `Authorization: ApiKey ...`. |
| `AIBRAIN_ES_INFERENCE_ID` | `.jina-embeddings-v5-omni-small` | Rust | The EIS inference endpoint used for the `semantic_text` field. |
| `AIBRAIN_ES_INDEX_PREFIX` | `aibrain-` | Rust | Prefix for the per-brain index names (`aibrain-<vault>`). |
| `AIBRAIN_ES_BATCH` | `50` | Rust | Documents per bulk request when draining the search queue. |
| `AIBRAIN_ES_POLL_MS` | `2000` | Rust | How often the idle worker checks the queue for new rows. |

`aibrain-core` with no arguments (or `aibrain-core --help`) prints this same
list with the current defaults baked in.

Two more are read only by the test suites and are covered in
[Tests](#tests): `AIBRAIN_TEST_DATABASE_URL` and `AIBRAIN_TEST_ELASTICSEARCH`.

### The `.env` file

`.env` at the repo root (or any parent of wherever `cargo` is run from — the
loader searches upward) is loaded by `dotenvy` before Rust reads any
environment variable, and never overwrites one that is already set. It holds
the Elasticsearch API key, which is a secret and must never be committed —
keep it out of version control (see `SECURITY.md`).

Copy `.env.example` to `.env` and fill in the two Elasticsearch variables to
turn search on:

```sh
cp .env.example .env
$EDITOR .env
```

With no `.env` and no `ELASTICSEARCH_URL` in the environment, everything
still works — search just runs on Postgres full-text search instead.

## How search works

Every note write queues a row in Postgres (`search_queue`, one row per
`(brain_id, rel_path)` — a note saved five times before the worker wakes is
one document to send, carrying the latest operation). If `ELASTICSEARCH_URL`
is unset, the queue just sits there: `/search` answers from Postgres
full-text search and nothing drains the queue.

If it is set, a background worker (`rust/aibrain-core/src/es/worker.rs`)
claims batches of `AIBRAIN_ES_BATCH` rows, sends one `_bulk` request per
batch (the embedding model runs inside the write, so one round trip per
batch beats one per note), and marks them done or retries them with the
error recorded. Each per-brain index (`aibrain-<vault-name>`, sanitized to
`[a-z0-9._-]`) gets a `semantic_text` field on the configured inference
endpoint plus plain lexical fields (`title`, `headings`, `tags`, `source`,
`excerpt`, `body`). `/search` queries both with a `retriever.linear`
(`minmax`-normalised lexical `multi_match` leg weighted 1.0, semantic leg
weighted 1.5) and resolves every hit back to its current Postgres row before
answering, so a note deleted seconds ago cannot appear in the results. If
Elasticsearch errors, or a non-empty query comes back with zero hits (the
semantic leg matches every indexed note, so empty usually means the note is
not indexed yet), `/search` falls back to Postgres for that request.

At startup, `aibrain-core serve` reconciles: for each brain, it compares the
note count in Postgres against the document count in that brain's
Elasticsearch index, and if they differ (and nothing is already queued for
that brain) it queues the whole brain.

### Backfilling

A database that had notes ingested before Elasticsearch was configured has
no queue rows for them, and nothing else creates them retroactively. Queue
everything explicitly:

```sh
cargo run --manifest-path rust/Cargo.toml -- resync-search
```

or, with the service already running, `POST /reindex`'s sibling
`POST /search/resync`. Both call the same `enqueue_all`. `aibrain-core
status` reports how many rows are pending or failing.

## The to-do shelf

Postgres-backed (`todo`, `todo_ref`, `todo_event` — see
`rust/aibrain-core/src/db/migrations/0003_todo.sql`), never materialised into
Obsidian. A day does not start at midnight: `todo.day_start_hour` in
`config.json` defaults to 04:00, so something finished at 01:00 still counts
as the evening before. Rollover is lazy — opening a day migrates anything
still open from before it onto it, logging the move, so a laptop asleep at
midnight never loses a day. A completed item stays visible, struck through,
on the day it was completed, and is simply absent the next day; nothing is
deleted, `todo_event` keeps the full history. The routes live in
`rust/aibrain-core/src/todo.rs` and are covered in the API table below.

### MCP server

`python3 -m aibrain.mcp_todo` is a stdio JSON-RPC 2.0 server (standard
library only) exposing six tools: `list_todos`, `add_todo`, `complete_todo`,
`reschedule_todo`, `link_todo_to_note`, `search_history`. It talks to the
day's list the same way the browser does, over HTTP through
`aibrain/corpus.py` and never straight to Postgres, so there is exactly one
owner of the schema. It reads `AIBRAIN_CORE_URL` for where the service is.

An ACP agent (Claude Code, Codex) gets this server automatically: it is
registered through the `session/new` `mcpServers` parameter alongside
`additionalDirectories`, so no extra configuration is needed to let a coding
agent read and edit the list. Run it by hand to point another MCP client at
it:

```sh
python3 -m aibrain.mcp_todo
```

## The pieces

```
aibrain-kernel/
├── aibrain/                   the UI server (Python 3, standard library only)
│   ├── __main__.py            entry point; python3 -m aibrain
│   ├── config.py              ~/.aibrain/config.json, and first-run discovery
│   ├── corpus.py              the only thing that talks to aibrain-core, over HTTP
│   ├── jobs.py                running scripts, streaming their output
│   ├── mcp_todo.py            the to-do shelf's MCP server
│   ├── server.py              threaded HTTP server, /api/* proxy, SSE
│   └── agents/                local retrieval, ACP over stdio, A2A over HTTP
├── rust/aibrain-core/         the corpus service (Rust, axum + sqlx)
│   ├── src/main.rs            CLI: serve, reindex, resync-search, status
│   ├── src/api.rs             the HTTP surface Python talks to
│   ├── src/config.rs          reads the same config.json Python writes
│   ├── src/vault/             parsing, wikilinks, frontmatter
│   ├── src/ingest.rs          scanning a vault into Postgres
│   ├── src/watch.rs           FSEvents, incremental rescans
│   ├── src/layout.rs          the galaxy layout
│   ├── src/graph.rs           layout → the universe payload, graph_cache, ETags
│   ├── src/events.rs          the /events change feed
│   ├── src/todo.rs            the day's list
│   ├── src/es/                Elasticsearch client, queue worker, search
│   └── src/db/                sqlx queries and migrations
├── web/                       the UI (no framework, no build step)
│   ├── universe.js            the three.js visualisation
│   ├── edges.js               edge brightness math, testable without a GPU
│   ├── app.js                 panels, search, chat, the control drawer
│   └── vendor/three.module.js three.js r160, vendored so it works offline
├── scripts/                   one folder per source system
│   └── macwhisper/            MacWhisper transcript exporter
├── tests/                     test_kernel.py (Python) and test_edges.mjs (Node)
├── docker-compose.yml         Postgres, plus the optional `full` profile
├── dev.sh                     starts all three processes
└── design_concepts/           the design this was built from, and the build plan
```

### Agents

Three kinds, all configured in the **Agents** tab, all rendered the same way:

| Kind | Transport | What it is |
|---|---|---|
| `local` | none | Answers from the corpus itself. No model, no network. |
| `acp` | [Agent Client Protocol](https://agentclientprotocol.com) over stdio | A coding agent — Claude Code, Codex — spoken to the way an editor speaks to it. |
| `a2a` | [Agent2Agent](https://a2a-protocol.org) over HTTP | A remote agent that publishes an agent card, such as Elasticsearch Agent Builder. |

Every agent, whatever the transport, is handed the same thing: the top
passages retrieved from your brains for that question, with a note asking it
to cite notes as `[[Title]]`. Those titles are resolved back to note ids,
which is how a remote agent's answer ends up with citations that open your
files.

ACP adapters found on `PATH` at first run are enabled automatically:

```sh
npm install -g @agentclientprotocol/claude-agent-acp   # Claude Code
npm install -g @agentclientprotocol/codex-acp          # Codex
```

An ACP agent may read and write files under its working directory and inside
your vaults, and nowhere else, and gets the to-do MCP server for free (see
above). Tool calls it asks permission for are approved automatically — there
is no modal yet, and a blocking prompt would stall the stream. Point it at a
vault you have backed up.

### Tools

The **Tools** tab runs the scripts in `scripts/`, streaming stdout and
stderr into the panel. The MacWhisper exporter is wired up with its
`--dry-run` and `--verbose` flags as toggles; when it finishes, `POST
/reindex` is called and the universe rebuilds. See
[`scripts/macwhisper/README.md`](scripts/macwhisper/README.md) for what it
does to the MacWhisper database.

## Controls

| | |
|---|---|
| Drag | orbit |
| Scroll | zoom |
| Click a star | open that note |
| Double-click a star | open it and fly to it |
| Double-click an agent | chat with it |
| Drag an agent | move it; the position is saved |
| `/` | search |
| `Esc` | close the topmost panel |
| Brain chip | focus one galaxy |
| ⊕ | reset the view |
| ☰ | brains, agents, tools, view |

## Configuration

`config.json` is written by the UI and read by both processes:

```
~/.aibrain/config.json      brains, agents, scripts, view and to-do settings
```

`AIBRAIN_HOME` moves it, and `--config` points Python at a specific file.
Rust only ever reads it. There is no local index file any more — the corpus
lives in Postgres.

```sh
python3 -m aibrain --port 9000 --no-open   # somewhere else, no browser
python3 -m aibrain --reindex               # ask aibrain-core to rescan, and exit
python3 -m aibrain --reindex --force       # re-read every note, ignoring mtimes
```

`aibrain-core` has its own CLI for working on the corpus directly:

```sh
cargo run --manifest-path rust/Cargo.toml -- serve --watch   # serve, and watch for edits
cargo run --manifest-path rust/Cargo.toml -- reindex --force # rescan every linked vault
cargo run --manifest-path rust/Cargo.toml -- resync-search   # queue everything for Elasticsearch
cargo run --manifest-path rust/Cargo.toml -- status          # what the database currently holds
```

## API surface

### Rust (`aibrain-core`, `AIBRAIN_BIND`, default `127.0.0.1:8781`)

| Method | Route | What it does |
|---|---|---|
| GET | `/health` | `{ok, notes}` — touches Postgres, not just the process. |
| GET | `/status` | Title, note/link counts, revision, per-brain stats, search engine state. |
| GET | `/universe` | The whole corpus as packed geometry. Supports `If-None-Match`; a 304 when nothing moved. |
| GET | `/events` | SSE change feed: `{kind, brain_id, revision}` as ingest changes things. |
| GET | `/search` | `?q=&brains=&limit=&any=` — Elasticsearch when configured, else Postgres FTS. |
| GET | `/note/:id` | A note and its rendered HTML, plus its linked neighbours. |
| GET | `/notes/by-path` | `?brain=&path=` — a note looked up by where it lives on disk. |
| GET | `/notes/recent` | `?limit=` — most recently touched notes. |
| POST | `/reindex` | `{force}` — rescan every linked vault. |
| POST | `/search/resync` | Queue every note for Elasticsearch (backfill). |
| GET | `/todos` | `?day=&now=` — one day's list; opening it also rolls stale items onto it. |
| POST | `/todos` | `{body, scheduled_on?, refs?}` — add an item. |
| GET | `/todos/history` | `?q=&limit=` — every to-do ever written, with its event history. |
| PATCH | `/todos/:id` | `{body?, sort_order?}` — edit an item in place. |
| POST | `/todos/:id/complete` | Mark done; stays visible, struck through, the day it was completed. |
| POST | `/todos/:id/uncomplete` | Undo a completion (an event, not an erasure). |
| POST | `/todos/:id/cancel` | Mark cancelled. |
| POST | `/todos/:id/reschedule` | `{to_day}` — `YYYY-MM-DD`, `today`, or `tomorrow`. |
| POST | `/todos/:id/link` | `{brain_id, rel_path}` — attach a note, kept by path so a rename can be repaired. |

### Python (`aibrain`, default `127.0.0.1:8760`)

Thin proxies over the Rust routes above, plus what only Python knows about
(agents, jobs, config):

| Method | Route | What it does |
|---|---|---|
| GET | `/api/universe` | Rust's `/universe`, plus agent positions and view options. |
| GET | `/api/events` | Rust's `/events`, forwarded frame by frame. |
| GET | `/api/status` | Rust's `/status`, plus agents, scripts, view, and running jobs. |
| GET | `/api/search` | Rust's `/search`, decorated with `gid` and brain name. |
| GET | `/api/recent` | Rust's `/notes/recent`, decorated the same way. |
| GET | `/api/note/<nid>` | Rust's `/note/:id`, decorated. |
| GET | `/api/node/<gid>` | Universe node id → note id, for a click in 3D. |
| GET/POST | `/api/todos` | Proxy to Rust's `/todos`. |
| GET | `/api/todos/history` | Proxy to Rust's `/todos/history`. |
| PATCH | `/api/todos/<tid>` | Proxy to Rust's `PATCH /todos/:id`. |
| POST | `/api/todos/<tid>/{complete,uncomplete,cancel,reschedule,link}` | Proxies to the matching Rust route. |
| GET | `/api/stream/chat` | `?agent=&q=` — SSE: ask an agent a question. |
| GET | `/api/chat/<agent_id>/history` | This session's chat history with one agent. |
| POST | `/api/chat/<agent_id>/clear` | Clear it. |
| GET | `/api/agent/<agent_id>/probe` | Whether an agent is reachable. |
| POST | `/api/script/<script_id>/run` | Run one of `scripts/`, streaming output. |
| POST | `/api/reindex` | Ask Rust to rescan, as a trackable job. |
| GET | `/api/jobs` / `/api/job/<id>` | Job list / one job's status and output. |
| POST | `/api/job/<id>/cancel` | Cancel a running job. |
| GET | `/api/stream/job/<id>` | SSE: a job's output as it runs. |
| POST | `/api/view` | Save view settings (rotation speed, link opacity, ribbon twist, ...). |
| POST | `/api/brain/<id>` / `/api/brain/<id>/remove` | Enable/rename, or unlink, a brain. |
| POST | `/api/brains/add` | Symlink a new vault into `obsidian_vaults/`. |
| GET | `/api/brains/discover` | Vaults found but not linked, and broken links. |
| POST | `/api/agent/<id>` / `/api/agent/<id>/remove` / `/api/agents/add` | Configure agents. |
| POST | `/api/agent/<id>/move` | Save a dragged agent's position. |

## Tests

Three kinds:

```sh
cd rust && cargo test                       # Rust: 108 tests
python3 -m unittest tests.test_kernel       # Python: 61 tests
node tests/test_edges.mjs                   # edge-brightness math
```

`scripts/test.sh` runs all three in one go.

Most Rust tests are pure unit tests and always run. A few are gated behind a
real Postgres and skip with a printed reason, rather than failing, when it is
unreachable:

```sh
AIBRAIN_TEST_DATABASE_URL=postgres://aibrain:aibrain@127.0.0.1:5433/aibrain_test \
  cargo test
```

Elasticsearch-gated tests additionally need `ELASTICSEARCH_URL` and
`ELASTICSEARCH_API_KEY` set (the repo's `.env`, or the environment); without
them those tests skip too.

`tests/test_kernel.py` covers the seam Python still owns — the `/api/*` HTTP
surface, citation tiers, vault discovery, the job runner — since parsing,
layout and rendering all moved to Rust and are tested there. Its
corpus-backed test classes build the `rust/aibrain-core` debug binary (if it
is not already built) and start it against the `aibrain_test` database on an
ephemeral port; if Postgres is not reachable there, they skip with a
message rather than failing. Set `AIBRAIN_TEST_ELASTICSEARCH=1` to let those
tests talk to the real Elasticsearch cluster instead of running with search
forced off; leave it unset on a laptop with no cluster.

`tests/test_edges.mjs` is a plain Node script (no test runner) that
`test_kernel.py` also runs, skipping if `node` is not installed.

## Troubleshooting

**"aibrain-core is not answering" / Python refuses to start.** Rust is not
reachable on `AIBRAIN_CORE_URL` (default `http://127.0.0.1:8781`). Start it:

```sh
docker compose up -d db
cargo run --manifest-path rust/Cargo.toml -- serve --watch
```

or just run `./dev.sh`, which starts all three. This is the exact message
`aibrain/corpus.py`'s `unreachable_message` prints.

**Postgres is down.** `aibrain-core` fails to connect at startup with a
connection error from `sqlx`. `docker compose up -d db` and wait for
`pg_isready`; `dev.sh` does this for you and polls up to 60 seconds. Check
the container directly with `docker exec aibrain-db pg_isready -U aibrain`.

**The Elasticsearch queue is failing.** `aibrain-core status` reports
`queue N pending / M failing`. Common causes: `ELASTICSEARCH_API_KEY` wrong
or expired (check `.env`), the inference endpoint named by
`AIBRAIN_ES_INFERENCE_ID` does not exist on the cluster, or the cluster is
unreachable from this network. The worker logs the rejection reason (via
`tracing::warn!`) and retries on its own poll interval
(`AIBRAIN_ES_POLL_MS`) rather than blocking ingest — notes keep saving and
searching on Postgres while it backs off. Once fixed, `aibrain-core
resync-search` re-queues anything that needs it.

**Search looks stale right after a big import.** The queue drains in the
background; `aibrain-core status` shows how much is left. `/search` falls
back to Postgres automatically whenever Elasticsearch returns zero hits for
a non-empty query, which is the common symptom of notes that have not been
indexed yet.

See `SECURITY.md` for how secrets and permissions are handled.

## Conventions

- Python 3 standard library only, so it runs on the system interpreter with
  no virtualenv. No build step for the frontend either: `web/` is served
  as-is.
- Rust owns the schema; Python only ever reads `config.json` back from disk,
  never Postgres directly.
- Every exporter under `scripts/` treats its source as read-only and
  verifies the source files are unchanged after a run.
- Upstream schema assumptions are checked in as a baseline and validated on
  every run, so a silent upstream change becomes a visible report rather
  than corrupted vault content.

## Status

The UI, the corpus service, live updates over `/events`, Elasticsearch
search with a Postgres fallback, the to-do shelf, and its MCP server are all
working. See `design_concepts/build-plan.md` for the roadmap and what has
landed. Known gaps:

- Tool-call permissions for ACP agents are auto-approved; there is no
  prompt.
- A2A authentication is limited to static headers.
