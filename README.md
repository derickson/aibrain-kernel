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
UI, in that order, and opens <http://127.0.0.1:8760/>. `make dev` does the
same; `make help` lists the other targets (`stop`, `docker-up`,
`docker-down`, `docker-redeploy`, `test`).

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
- **Node/npm**, only if you want the Claude Code agent: install the ACP
  adapter it talks over (see [Agents](#agents)):

  ```sh
  npm install -g @agentclientprotocol/claude-agent-acp
  ```

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

### Running on more than one machine

Everything that differs between machines — which vaults exist, where they
live, secrets — is meant to live outside git, so two checkouts never fight
over each other's local paths in a commit:

| What | Where | Committed? |
|---|---|---|
| Which vaults are linked, and to where | `obsidian_vaults/<name>` symlinks | No — gitignored |
| Read-deny rules derived from those vaults | `.claude/settings.local.json` | No — gitignored; regenerated automatically on every `Config.load()`, never hand-edit it |
| Brain list, agents, scripts, UI layout | `$AIBRAIN_HOME/config.json` (default `~/.aibrain/config.json`) | No — lives outside the repo entirely |
| Secrets and endpoints (Elasticsearch, etc.) | `.env` | No — gitignored; copy from `.env.example` |
| MacWhisper staging + pull ledger (macOS only) | `raw_transcripts/`, `scripts/macwhisper/export_state.sqlite3` | No — gitignored |

`.claude/settings.json` (no `.local`) is the one file in this list that
*is* committed — it is meant only for rules you want every checkout to
share, never for anything derived from a specific machine's vaults. See
[the note in `SECURITY.md`](SECURITY.md) about why the deny rules moved out
of it.

Setting up an additional machine is then just:

```sh
git clone <this repo>   # or open the same checkout from another OS/user
cd aibrain-kernel
ln -s /path/to/this/machines/vault obsidian_vaults/Home
cp .env.example .env && $EDITOR .env   # only if you want Elasticsearch here too
./dev.sh
```

The first `Config.load()` on that machine discovers `obsidian_vaults/`,
creates `~/.aibrain/config.json` and `.claude/settings.local.json` for it,
and none of that touches what any other machine has configured.

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
| `AIBRAIN_TODO_TEST_CLOCK` | unset | Rust | `1` lets every `/todos` route take a `?now=` override instead of the wall clock, so tests can say what day it is. Never set this in normal use. |
| `ELASTICSEARCH_URL` | unset | Rust | Elasticsearch endpoint. Unset means search stays on Postgres. |
| `ELASTICSEARCH_API_KEY` | unset | Rust | Sent as `Authorization: ApiKey ...`. |
| `AIBRAIN_ES_INFERENCE_ID` | `.jina-embeddings-v5-omni-small` | Rust | The EIS inference endpoint used for the `semantic_text` field. |
| `AIBRAIN_ES_INDEX_PREFIX` | `aibrain-` | Rust | Prefix for the per-brain index names (`aibrain-<uid>`) and aliases (`aibrain-_w-<uid>`, `aibrain-_search`). |
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
error recorded. Each brain gets its own index, `aibrain-<uid>`, named by an
id the brain row is given once and never reuses (brains that predate this
keep their old `aibrain-<vault-name>` index, adopted in place). Writes go
through a per-brain alias with `require_alias=true`, so a write can never
recreate a dropped index; searches across every brain go through the
`aibrain-_search` alias. Each index gets a `semantic_text` field on the
configured inference endpoint plus plain lexical fields (`title`, `headings`, `tags`, `source`,
`excerpt`, `body`). `/search` queries both with a `retriever.linear`
(`minmax`-normalised lexical `multi_match` leg weighted 1.0, semantic leg
weighted 1.5) and resolves every hit back to its current Postgres row before
answering, so a note deleted seconds ago cannot appear in the results. If
Elasticsearch errors, or a non-empty query comes back with zero hits (the
semantic leg matches every indexed note, so empty usually means the note is
not indexed yet), `/search` falls back to Postgres for that request.

Removing a vault (the Remove button, or a rescan that no longer finds its
link) retires the brain in one Postgres transaction: its notes and every
queued write for it go at once, and its index is recorded in
`index_retirement`. The worker detaches the aliases and deletes the index —
now, or whenever Elasticsearch is next reachable. While the cluster is down
the worker pauses as a whole rather than charging the outage to individual
documents; a document refused ten times moves to `search_dead_letter`.

The worker also reconciles at startup, every fifteen minutes, and after an
outage: a brain whose document count differs from Postgres is requeued, a
missing index is reprovisioned, and an index stamped as ours (`_meta.aibrain`)
with no brain behind it is retired. An index under the prefix without that
stamp is reported and never touched. `GET /search/status` (and the `search`
block of `/api/status`) shows all of this. The design is in
`design_concepts/RECOMMENDATION_ELASTICSEARCH.md`.

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
`rust/aibrain-core/src/db/migrations/0003_todo.sql` and `0005_todo_due.sql`),
never materialised into Obsidian. It is one list, not a calendar: there is no
day to page to and nothing rolls over. An item may carry an optional due date,
shown beside it as a chip (due today, tomorrow, a weekday, or overdue); most
items have none. Folders sit below the list, and a filed item shows under its
folder instead. A completed item stays on the list, struck through, until the
day it was completed is over, then only the history has it; nothing is
deleted, and `todo_event` keeps every change. A day does not start at
midnight: `todo.day_start_hour` in `config.json` defaults to 04:00, so at
01:00 "today" and "tomorrow" still mean the day you were working and the one
after. The routes live in `rust/aibrain-core/src/todo.rs` and are covered in
the API table below.

### MCP server

`python3 -m aibrain.mcp_todo` is a stdio JSON-RPC 2.0 server (standard
library only) exposing six tools: `list_todos`, `add_todo`, `complete_todo`,
`set_due_date`, `link_todo_to_note`, `search_history`. It talks to the
to-do list the same way the browser does, over HTTP through
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

## The Absorbitron

An intake tray for the links and files people send you, so they land
somewhere searchable before you have decided where they belong.

**Where it writes.** Into an `Absorbitron/` folder at the top of the brain
marked as the meeting recording target (☰ → **Brains** → "Meeting recording
target · Absorbitron intake"). With no target set the + button explains that
and creates nothing. The folder name is fixed: `FOLDER` in
`aibrain/absorbitron.py` and `ABSORBITRON_FOLDER` in
`rust/aibrain-core/src/layout.rs` must agree.

**Making one.** The gold **+** beside the brand at the top left opens a
yellow sticky over the universe. Type a title and press Enter, or move to the
body, and the note is created as
`Absorbitron/<YYYY-MM-DD HHmm> <title>.md`. If a note with that name already
exists this minute, seconds are added to the stamp (`HHmmss`). The title is
tidied rather than refused: characters that would break a file name or a
`[[wikilink]]` (`\ / : * ? " < > | # ^ [ ]`) become spaces, leading dots go,
and it is capped at 160 characters. Once the note exists the title is fixed.

The body is a CodeMirror editor with the same live preview and `[[`
autocompletion as the notes panel. It saves itself a second after you stop
typing, and on close or page unload. Saves are hash-checked like any other
edit: if the file changed on disk underneath you, the sticky offers **Use
theirs** or **Keep mine** instead of overwriting, and if it has been moved
or deleted the sticky says so and stops saving. Drag the header to move the
sticky; ⤢ opens the note in the notes panel; × closes it.

**The file.** What you type, then the sections the AI step will fill in:

```markdown
Sam sent this: https://example.com/paper

---
# AI Notes

_Placeholder — AI expansion and research on the links and attachments above will be added here._

# Attachments

- [[Absorbitron/Attachments/2026-11-10 1432 report.pdf]]
```

The sticky shows and edits only the part above the rule; everything from
`---` / `# AI Notes` down is kept as it is on disk and written back
untouched, so the AI sections and the attachment list are never lost to an
edit.

**Links and attachments.** Every URL in the body gets an unlit lightbulb
glyph under the editor (click it to open the link); it is meant to light up
once that link has been researched. Drop files onto the sticky, or use its
file button, and each is copied to
`Absorbitron/Attachments/<YYYY-MM-DD HHmm> <original name>`, linked under
`# Attachments`, and shown as a black paperclip (meant to glow once
processed). Uploads are capped at 200 MB each and never overwrite: a name
already taken gets seconds, then a counter. Neither links nor attachments
are drawn in the universe.

**In the universe.** Intake notes are not stars in the galaxy swirl. Each is
a glowing yellow cube, larger than a star, orbiting its brain. Hovering one
stops it and shows its title; clicking reopens its sticky. Rust excludes
`Absorbitron/` paths from the layout (`notes_for_layout`) and sends them
separately as each brain's `absorbitron` list in `/universe`
(`absorbitron_notes`), so cubes come and go without moving any star.

**Search and agents.** Intake notes are ordinary notes to everything else:
they are indexed, show up in search (words inside a URL are not split into
search terms by the Postgres search), and are reachable by ACP agents like
the rest of the vault.

**Filing one.** Move the file out of `Absorbitron/` (in Obsidian, Finder,
anywhere) and it becomes a normal note: on the next rescan it leaves its
orbit and joins the galaxy as a star. Being an intake note is decided by the
path alone, so nothing else needs updating. It keeps its `# AI Notes` and
its attachment links, which are full vault paths and still resolve.

## The pieces

```
aibrain-kernel/
├── aibrain/                   the UI server (Python 3, standard library only)
│   ├── __main__.py            entry point; python3 -m aibrain
│   ├── absorbitron.py         intake note names, template, attachment writes
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
│   ├── src/todo.rs            the to-do list
│   ├── src/es/                Elasticsearch client, queue worker, search
│   └── src/db/                sqlx queries and migrations
├── web/                       the UI (no framework, no build step)
│   ├── universe.js            the three.js visualisation
│   ├── edges.js               edge brightness math, testable without a GPU
│   ├── app.js                 panels, search, chat, the control drawer
│   ├── absorbitron.js         the Absorbitron sticky editor
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
`--dry-run` and `--verbose` flags as toggles; it writes into
`raw_transcripts/` at the repo root, a staging folder outside any vault, so
it does not trigger a reindex on its own — a transcript only becomes part of
a brain once it is moved into a linked vault. See
[`scripts/macwhisper/README.md`](scripts/macwhisper/README.md) for what it
does to the MacWhisper database.

## Controls

| | |
|---|---|
| Drag | orbit |
| Scroll | zoom |
| Click a star | open that note |
| Double-click a star | open it and fly to it |
| Click a yellow cube | reopen that Absorbitron sticky |
| + (top left) | new Absorbitron sticky |
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
| GET | `/todos` | `?now=` — the list: open items plus anything closed today, the folders, and the service's `today`. |
| POST | `/todos` | `{body, due_on?, refs?}` — add an item to the end of the list. |
| GET | `/todos/history` | `?q=&limit=` — every to-do ever written, with its event history. |
| PATCH | `/todos/:id` | `{body?, sort_order?}` — edit an item in place. |
| POST | `/todos/:id/complete` | Mark done; stays visible, struck through, for the rest of the day. |
| POST | `/todos/:id/uncomplete` | Undo a completion (an event, not an erasure). |
| POST | `/todos/:id/cancel` | Mark cancelled. |
| POST | `/todos/:id/due` | `{due_on}` — `YYYY-MM-DD`, `today`, `tomorrow`, or null to clear. |
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
| GET | `/api/absorbitron` | `{folder, brainId, brainName, maxAttachmentBytes}` — where intake notes go; `brainId` is null with no meeting target. |
| POST | `/api/absorbitron/create` | `{title, text}` — a new timestamped intake note; `{nid, brainId, name, relPath}`. 409 with no target. |
| POST | `/api/absorbitron/attach` | `?nid=&name=`, raw file bytes as the body — copied into `Absorbitron/Attachments/`; `{path, name, size}`. Linking it in the note is the client's job. |
| GET/POST | `/api/todos` | Proxy to Rust's `/todos`. |
| GET | `/api/todos/history` | Proxy to Rust's `/todos/history`. |
| PATCH | `/api/todos/<tid>` | Proxy to Rust's `PATCH /todos/:id`. |
| POST | `/api/todos/<tid>/{complete,uncomplete,cancel,due,link,file}` | Proxies to the matching Rust route. |
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

One command runs everything:

```sh
scripts/test.sh
```

It runs four stages in order, each with its own header, and prints a summary
at the end naming every stage that passed, failed or was skipped (and why):

1. **Rust unit tests** — `cargo test` with Postgres and Elasticsearch both
   switched off, so every gated test takes its own "no credentials" early
   return.
2. **Rust integration tests (Postgres)** — the same `cargo test`, this time
   against a real database. The script looks for the `aibrain-db` container,
   creates the `aibrain_test` database on it if missing, and skips this stage
   with a printed reason if no container answers.
3. **Python suite** — every `tests/test_*.py`, discovered
   (`python3 -m unittest discover -s tests -p 'test_*.py'`). Its
   corpus-backed test classes build the `rust/aibrain-core` debug binary (if
   not already built) and start it against `aibrain_test` on an ephemeral
   port, skipping with a message rather than failing if Postgres is
   unreachable. `tests/test_edges.mjs`, a plain Node script with no test
   runner of its own, runs inside this stage (`EdgeShadingTests`) and skips if
   `node` is not installed.
4. **Rust integration tests (Elasticsearch)** — `cargo test es::integration`,
   which additionally needs `ELASTICSEARCH_URL` and `ELASTICSEARCH_API_KEY`.
   The script reads them from the environment, or from a repo-root `.env` if
   the environment does not already have them. Each test indexes under its
   own `aibrain-test-<pid>-<tag>-` prefix and deletes those indices itself
   when it finishes, so a real `aibrain-*` index is never touched. These
   tests run one at a time and clear the brain table of the test database
   first, because the worker acts on every brain it finds there. Skipped with a
   reason when no credentials are found.

The script exits non-zero if any stage that ran failed. It never rewrites
`.claude/settings.local.json`: every corpus-backed test class points
`aibrain.config` at a private temp file, and the one path that does not (the
`StartupTests` subprocess, which runs the real `python3 -m aibrain` entry
point) sets `AIBRAIN_MANAGE_DENY_RULES=0` in its own environment rather than
the script's, so `DenyRuleTests` — which tests that rewrite — still runs with
the real behaviour on.

Run a stage by hand with the same environment variables `scripts/test.sh`
looks for:

```sh
AIBRAIN_TEST_DATABASE_URL=postgres://aibrain:aibrain@127.0.0.1:5433/aibrain_test \
  cargo test                                  # stage 2, by hand
python3 -m unittest discover -s tests -p 'test_*.py'   # stage 3, by hand
node tests/test_edges.mjs                     # just the edge-brightness math
```

`tests/test_kernel.py` covers the seam Python still owns — the `/api/*` HTTP
surface, citation tiers, vault discovery, the job runner — since parsing,
layout and rendering all moved to Rust and are tested there. Set
`AIBRAIN_TEST_ELASTICSEARCH=1` to let its corpus-backed tests talk to the real
Elasticsearch cluster instead of running with search forced off; leave it
unset on a laptop with no cluster (this is separate from, and off by default
even when, `scripts/test.sh`'s stage 4 runs the Rust Elasticsearch tests).

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
