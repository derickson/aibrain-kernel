# Build plan — what is left

Companion to `AI Brains.html`, which is the visual design this implements.
Written 2026-09-22, after Phase 1a and 1b landed.

## Status (updated 2026-09-22)

Everything below has landed. The two stacks described in "Where things
stand" no longer both exist: the browser now consumes Rust's corpus, and the
SQLite path is gone.

| Item | State |
|---|---|
| 1e — Point Python at Rust | done — `aibrain/corpus.py` replaces `Index`; `index.py`, `md.py`, `vault.py` and most of `graph.py` are deleted |
| 1c — Remove the node cap | done — `web/universe.js` consumes Rust's `positions`/`sizes`/`sourceIndex`/`degrees`/`names`/`edges` buffers, edge brightness is a vertex attribute (`web/edges.js`), the cap and its slider are gone |
| `/events` (SSE) | done — `rust/aibrain-core/src/events.rs`, proxied by Python's `/api/events` |
| `graph_cache` | done — `rust/aibrain-core/src/graph.rs`, keyed by `(brain_id, revision)`, served with an ETag (304 when unchanged) |
| Phase 2 — the to-do shelf + MCP server | done — `rust/aibrain-core/src/todo.rs`, `aibrain/mcp_todo.py` |
| `dev.sh` as three processes | done |
| `.claude/settings.json` deny rules regenerated in `reconcile_brains()` | done — `aibrain/config.py` |
| README rewrite for the new architecture | done — see `README.md` |

The rest of this document is left as it was written, as history of the plan
that produced the above — including the "two parallel stacks" framing below,
which was accurate when this was written and is not any more.

## Where things stand

There are **two parallel stacks**, and only one of them is wired to the browser.

```
LIVE   browser ──► python ──► sqlite            ~/.aibrain/index.sqlite3
DARK            rust ──► postgres :5433         built, 47 tests, nothing consumes it
```

| Piece | State |
|---|---|
| Symlink-only vault scoping | done |
| Citation evidence tiers | done |
| Retrieval any-term fallback | done |
| ACP `additionalDirectories` + `.claude/settings.json` deny rules | done |
| Postgres in compose, Rust ingest, watcher, HTTP API | done, dormant |
| Everything below | not started |

The Rust service holds the same 465 notes and 1,454 links the Python index
does, ingests in 1.2 s, rescans incrementally in 0.1 s, and updates within a
second of a file changing on disk. None of that reaches the UI yet.

## Open decision, needed before 1e starts

The approved plan deletes the SQLite path outright, so the app stops working
when the Postgres container is down. That was a deliberate choice — one schema
owner, no dual read paths — and it is still the right one. But 1e is the point
of no return for `python3 -m aibrain` being self-sufficient, so it is worth
confirming rather than discovering later.

**Assumed unless told otherwise: no fallback. Postgres becomes required.**

---

## 1e — Point Python at Rust

The load-bearing step. Everything else is dormant until this lands.

**New** `aibrain/corpus.py` — a stdlib `urllib` client for the seven endpoints
that already exist (`/health`, `/status`, `/universe`, `/search`, `/note/:id`,
`/notes/recent`, `/reindex`). It replaces `Index`, but not method-for-method:
`Index` had fourteen fine-grained methods and 23 live call sites, and the point
of the move is that those become a handful of composed calls.

**Changed**
- `aibrain/server.py:45` — `Index(cfg.db_path)` becomes the corpus client;
  routes become thin proxies
- `aibrain/agents/base.py` — `retrieve()` goes through `corpus.search`, keeping
  the strict-then-widen behaviour that is currently two `Index.search` calls
- `aibrain/jobs.py` — the Reindex job calls `POST /reindex` instead of scanning

**Deleted** — about 1,100 lines
- `aibrain/index.py` (513), `aibrain/md.py` (222), `aibrain/vault.py` (198)
- most of `aibrain/graph.py` (305) — layout now lives in Rust

**Kept:** `aibrain/vault.py`'s `normalize()` may still be needed by the citation
resolver; check before deleting the file wholesale.

**Startup:** if Rust is unreachable, fail with a message naming the two commands
that fix it, not a traceback.

**Tests:** `tests/test_kernel.py` currently builds a temp vault and a temp
SQLite file. It needs a Postgres fixture, or the data-layer tests move to Rust
and the Python suite keeps only the HTTP-surface tests. Prefer the latter — the
Rust side already has 47 tests covering parsing, layout and rendering.

**Verify:** the app looks and behaves identically, on Postgres. Same note count,
same search results, citations still tiered, MacWhisper script still runs.

---

## 1c — Remove the node cap

Three changes that only work together; doing any one alone achieves nothing.

**1. Stop computing the layout twice.** `web/universe.js` still runs its own
`buildBrain()` and ignores the positions Rust computes. Rust already returns
`positions`, `sizes`, `sourceIndex`, `degrees`, `names` and `edges` per brain.
Collapse the duplication: consume the buffers, delete the JS layout.

This also fixes something the JS version cannot: positions in Rust are derived
from a hash of `(brain_id, rel_path)` and quantised counts, so editing one note
moves nothing else. The JS version derives position from array index, so it
reshuffles the galaxy on every change — which is why live updates would look
broken until this lands.

**2. Move edge shading to the GPU.** `web/universe.js` loops over every edge
every frame to recompute colour. That loop, not the renderer, is what makes a
large corpus expensive. Brightness becomes a vertex attribute updated only when
the highlight set changes.

**3. Delete the cap.** `aibrain/config.py:58`, `aibrain/graph.py:156`, the
`maxNodes` field in `/api/status`, and the View-tab slider at
`web/app.js:1106-1115`. The `2200 OF 2445 NOTES` badge becomes a plain count.

**Verify:** all 6,061 notes render when the other four vaults are relinked;
check frame rate with the full corpus, and confirm a note edit moves only its
own star.

---

## Two pieces planned but never built

**`/events` (SSE).** Rust maintains a `revision` counter and bumps it when
ingest changes anything, but nothing streams it. The watcher works and nothing
downstream hears it. Without this, live updates need a poll or a reload.

Shape: `GET /events` emits `{kind, brain_id, revision}`; the browser patches the
changed notes rather than rebuilding. Depends on 1c — patching a galaxy that
reshuffles on every change is pointless.

**`graph_cache`.** The table exists and is never written. Layout is recomputed
on every `/universe` call: 57 ms at 465 notes, roughly 10× that at 6,061, on
every page load. Cache the packed geometry keyed by `(brain_id, revision)` and
serve with an ETag so an unchanged brain is a 304.

This is the "pre-render subsections, invalidate as Rust notices changes" idea
from the original brief. It is only worth building once the corpus is large
enough to feel it — after 1c, not before.

---

## Smaller

- `dev.sh` — dev is now three processes (`docker compose up -d db`,
  `cargo run -- serve --watch`, `python3 -m aibrain`)
- README rewrite for the new architecture
- Regenerate `.claude/settings.json` deny rules during `reconcile_brains()`, so
  the list cannot go stale when a vault is linked or unlinked. Today it is a
  static file that silently stops protecting a newly-unlinked vault.

---

## Phase 2 — the to-do app

Postgres-backed, never materialised into Obsidian.

**Schema**
```
todo        id, body, created_at, first_scheduled_on, scheduled_on,
            completed_at, cancelled_at, sort_order
todo_ref    todo_id, brain_id, rel_path, note_id   -- by path AND id, so a
                                                      rename can be repaired
todo_event  todo_id, at, kind, from_day, to_day    -- the history
```

**Rollover, lazily on read.** Opening day *D* migrates anything with
`scheduled_on < D` that is still open, and logs the migration. No cron: a
missed midnight — laptop asleep, app not running — must not lose a day.

Completed items stay visible on the day they were completed, struck through,
and are simply absent the next day. Nothing is deleted; `todo_event` keeps the
history. Day boundary is a configurable start hour, defaulting to 04:00 so late
work counts as the previous day.

**UI.** A shelf sliding from the left that pushes the agent panel right rather
than covering it — so a conversation and the day's list are visible together.
Reuses the existing panel transition in `web/style.css`.

**MCP server.** A stdio server exposing `list_todos`, `add_todo`,
`complete_todo`, `reschedule_todo`, `link_todo_to_note`, `search_history`.
Registered through the ACP `session/new` `mcpServers` parameter — the same field
`additionalDirectories` goes through, so the adapter is known to accept it.

It should talk to the Rust service over HTTP rather than to Postgres directly,
keeping one owner of the schema.

**Verify:** add a to-do through the shelf, ask Claude Code to list it, have it
add one, confirm it appears in the shelf. Roll the clock forward a day and check
that completed items vanish from today while staying in history, and that open
ones migrate.

---

## Order

1. **1e** — riskiest; proves the Rust layer or doesn't
2. **1c** — mostly mechanical once the payload flows
3. **`/events`** — needs 1c to be worth anything
4. **`graph_cache`** — only once the corpus is big enough to feel the cost
5. **Phase 2** — needs Postgres reachable from Python, i.e. needs 1e
