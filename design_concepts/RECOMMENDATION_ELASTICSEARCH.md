# Recommendation — Elasticsearch index lifecycle

Written 2026-09-23. A proposal, not yet built. It covers how a brain's
Elasticsearch index is named, created, written to, and removed, and what
should change so that removing a vault is immediate, cheap, and cannot be
undone by a late write.

## Where things stand

- **Naming.** `Es::index_for(brain_name)` (`rust/aibrain-core/src/es/mod.rs:85`)
  derives the index from the vault's *display name* through
  `sanitize_index_name` (`es/mod.rs:220`). Two names that fold to the same
  string ("My Notes", "my-notes") share one index. Renaming the symlink moves
  the brain to a new index and orphans the old one. Queue rows carry
  `brain_name` rather than an index name, so the index a row targets is decided
  when it is claimed, not when it is enqueued.
- **Removal.** `remove_brain` (`aibrain/server.py:1011`) unlinks the symlink
  and saves the config. Nothing else happens until someone runs a reindex.
  Then `retain_brains` (`db/queries.rs:96`) queues one `delete` per note and
  deletes the `brain` row, and the worker sends those deletes in bulk batches
  of 50. The index itself is never dropped: `Es::delete_index` is only called
  from the integration-test teardown. An empty `aibrain-<name>` is left behind,
  and unscoped searches (`aibrain-*`, `es/search.rs:84`) still hit it.
- **Creation.** `ensure_index` (`es/mod.rs:104`) caches "exists" in a
  process-local `known` set. If the index is deleted by anyone else, the cache
  still says it exists, and the next `_bulk` makes Elasticsearch
  **auto-create** the index with a dynamic mapping: no `semantic_text` and no
  inference endpoint. Every queued upsert then lands there. This is the
  "recreated at large scale" failure, and nothing in the code guards against
  it today.
- **Outages.** On a transport failure the worker calls `fail_queue` on every
  row in the batch (`es/worker.rs:104`). Each row backs off independently at
  2^attempts seconds, capped at one hour. After a long outage the backlog can
  sit for up to an hour after the cluster recovers, and `attempts` measures
  the outage rather than how bad the document is.
- **Drift.** `worker::reconcile` (`es/worker.rs:198`) runs once at startup,
  only looks at brains that are still in Postgres, and compares counts. If
  Elasticsearch is unreachable at startup it is skipped for the whole run. An
  index whose brain is gone is never noticed.

## 1. Name indices by a stable, unique id

Add an immutable, never-reused identifier to each brain and record the index
name instead of deriving it.

```sql
-- 0005_index_identity.sql
ALTER TABLE brain ADD COLUMN IF NOT EXISTS uid        TEXT;  -- e.g. UUIDv7 / 12-char base32
ALTER TABLE brain ADD COLUMN IF NOT EXISTS index_name TEXT;  -- recorded, never recomputed
CREATE UNIQUE INDEX IF NOT EXISTS brain_uid_idx ON brain (uid);
CREATE UNIQUE INDEX IF NOT EXISTS brain_index_name_idx ON brain (index_name);
```

- **Format.** Use `aibrain-<uid>` for the concrete index and
  `aibrain-w-<uid>` for its write alias (see §2). A human-friendly alias,
  `aibrain-name-<slug>`, is optional, for Kibana/Discover only; nothing in the
  code should depend on it.
- **Why not `brain.id`?** `brain.id` is `slugify(link name)`
  (`aibrain/config.py:314`). It is stable across a rescan but **is reused**
  when a vault is removed and re-added under the same name. If the drop of the
  old index is still queued when the new brain arrives, a name built from
  `brain.id` would let the drop delete the *new* brain's index. A fresh `uid`
  per brain lifetime makes each drop target exactly one generation.
- **Stamp ownership into the index** via the mapping's `_meta`:
  `{ "aibrain": { "brain_id", "uid", "created_at", "schema": 1 } }`.
  Reconcile (§5) can then tell our indices from strays such as
  `aibrain-test`, and a rebuilt Postgres can re-adopt an existing index
  instead of paying to re-embed every note.
- **Queue rows carry `index_name`**, captured at enqueue time, in place of
  `brain_name`. The target of every write is then decided once, in the same
  transaction that decided to write it.
- **Search targets recorded names.** `target_indices` maps ids to
  `brain.index_name` for *active* brains. An unscoped search uses a read alias,
  `aibrain-search`, that every live index joins at creation and leaves at
  retirement, instead of the `aibrain-*` wildcard, which also picks up
  orphans and strays.

### Migrating what exists

The existing indices hold paid-for embeddings, so don't move them:

1. **Adopt in place.** For each existing brain, set
   `index_name = aibrain-<sanitized name>` (today's name), generate a `uid`,
   add the write and search aliases, and PUT `_meta` onto the mapping. No
   documents move.
2. New brains get `aibrain-<uid>` from the start.
3. Optionally, later: move adopted brains to uid names with `_reindex` and an
   alias swap. Before relying on this, confirm on the Serverless cluster that
   `_reindex` carries the stored `semantic_text` inference results rather than
   re-running inference.

## 2. Make it impossible for a write to recreate an index

This is the most important change, because it closes every race below at the
cluster level rather than relying on our own locking being perfect.

- **Write only through the per-brain write alias**, and send every `_bulk`
  with `?require_alias=true`. When the alias is gone (because the index was
  dropped), Elasticsearch rejects the action instead of auto-creating an
  index. Serverless doesn't let us set `action.auto_create_index`, and
  `require_alias` is a per-request guard that works there.
- **Create indices in exactly one place.** Creation happens only in the
  retirement-aware "provision" step (brain becomes active → PUT the index with
  mapping, `_meta` and aliases). The worker never creates indices. If a write
  finds its alias missing, the worker checks the brain's state: an active
  brain gets a provision task, a retired brain's rows are discarded.
- **Remove the `known` cache,** or scope it to "provisioned this process and
  not since dropped" and clear it on any `index_not_found` or
  `alias required` response. With `require_alias` a stale cache is harmless,
  but it shouldn't claim an index exists when it doesn't.

## 3. Vault removal as a durable, ordered operation

Removal becomes an explicit operation in Rust, called by Python's
`remove_brain` right after it unlinks the symlink, instead of a side effect of
the next reindex.

### New state

```sql
-- One row per index we have decided to destroy. This is the outbox: it
-- survives restarts and outages, and is the only thing that ever deletes an index.
CREATE TABLE IF NOT EXISTS index_retirement (
    index_name   TEXT PRIMARY KEY,
    brain_id     TEXT        NOT NULL,
    uid          TEXT        NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts     INTEGER     NOT NULL DEFAULT 0,
    last_error   TEXT,
    not_before   TIMESTAMPTZ NOT NULL DEFAULT now(),
    done_at      TIMESTAMPTZ
);

-- Queue rows now belong to a brain. Deleting the brain purges its pending work
-- in the same statement, so nothing is left for the worker to find later.
ALTER TABLE search_queue
  ADD CONSTRAINT search_queue_brain_fk
  FOREIGN KEY (brain_id) REFERENCES brain(id) ON DELETE CASCADE;
```

The existing comment on `search_queue` explains why there's deliberately no
foreign key to `note`: a per-note delete must outlive its note row. That still
holds. The foreign key here is to `brain`. Once dropping the index replaces
per-note deletes for a removed brain, no queue row needs to outlive its brain.

### Serialising lifecycle changes

Brain lifecycle changes and bulk ingest take a Postgres advisory lock keyed on
the brain id:

| Operation | Lock |
|---|---|
| `retire_brain`, `add/provision_brain` | exclusive, `pg_advisory_xact_lock(hashtext('brain:'||id))` |
| `ingest::reindex` per brain, watcher `ingest_one` | shared, `pg_advisory_xact_lock_shared(...)` |

`reindex` also **re-reads the config after taking its locks**, and
`upsert_brain` refuses to resurrect a brain whose `uid` appears in
`index_retirement`. A reindex that started with a stale config (still listing
the removed vault) then can't re-create the brain row and re-queue thousands
of upserts behind the removal's back.

### The removal transaction

`POST /brains/<id>/retire` in Rust. One transaction, holding the exclusive lock:

1. `SELECT … FROM brain WHERE id = $1 FOR UPDATE`. If the row is missing,
   return OK (idempotent).
2. `INSERT INTO index_retirement (index_name, brain_id, uid) …`.
3. `DELETE FROM brain WHERE id = $1`. This cascades to `note`, `link`,
   `link_target`, `graph_cache` and, with the new foreign key, **every pending
   `search_queue` row for the brain**. `todo_ref.note_id` becomes NULL as it
   does today.
4. Commit, then publish a `Change::brain_removed(id)` on the event bus so the
   UI drops it at once.

The remaining steps run in the background:

5. **Detach first.** Remove the index from the `aibrain-search` alias and
   delete its write alias `aibrain-w-<uid>`. From this point:
   - searches no longer see it, and
   - any write already in flight, from a batch the worker claimed before step
     3, fails `require_alias` instead of landing or recreating the index. Its
     rows were already deleted by the cascade, and `finish_queue` / `fail_queue`
     against missing ids are no-ops.
6. **Drop.** `DELETE /aibrain-<uid>`. Treat `404 index_not_found` as success.
7. Mark `done_at`. Keep the row for about 30 days as an audit trail, then
   delete it.

The worker loop processes retirements before document batches. Each step is
idempotent, so a crash between steps 5 and 7 simply repeats the work.

### Blocking new work for a retiring brain

Everything that enqueues does so in SQL that joins `brain`:

```sql
INSERT INTO search_queue (brain_id, index_name, note_id, rel_path, op)
SELECT b.id, b.index_name, $3, $4, $5
  FROM brain b
 WHERE b.id = $1
ON CONFLICT …
```

`enqueue_note` (`db/queries.rs:790`) currently takes `brain_name` from the
caller and inserts unconditionally. With the join, a missing brain enqueues
nothing, and the foreign key backs that up. `enqueue_all` and `delete_notes`
already join `brain` and need only the column swap.

### When Elasticsearch is unreachable

Retirement is recorded in Postgres (steps 1–4) whether or not the cluster is
reachable, so the UI and Postgres reflect the removal at once. Steps 5–6 wait
in `index_retirement` and run when the cluster returns (see §4). Nothing can
recreate the index in the meantime:

- no queue rows for the brain exist,
- no enqueue can create one,
- `upsert_brain` refuses the retired `uid`, and
- a write aimed at the old alias would fail `require_alias` anyway.

### Re-adding a vault with the same name

This produces a new `brain` row with a new `uid`, a new index and new aliases.
A drop still queued for the old generation targets `aibrain-<old uid>` only,
so the two never touch.

### Python / UI

- `remove_brain` calls the Rust retire endpoint after unlinking. If Rust is
  down, the next reindex's `retain_brains` runs the same retire routine for
  every brain that disappeared, so the reindex becomes the fallback rather
  than the primary path. `retain_brains` stops queuing per-note deletes.
- The Brains panel shows "Removing… (search cleanup pending)" while an
  `index_retirement` row is not done, and surfaces `last_error`.
- The Remove confirmation should mention that the search index is deleted.

## 4. A cluster-health gate in the worker

Replace per-row backoff for outages with one breaker for the whole worker:

- **Classify errors.** Connection refused, timeout, 5xx, or 429 on the bulk
  request means *the cluster is unavailable*. Per-item 4xx errors inside a 200
  bulk response mean *this document is bad*.
- **When the cluster is unavailable,** release the leases without touching
  `attempts` (`UPDATE … SET locked_until = NULL`). Then back off globally
  (1s → 2s → … capped at about 60s, with jitter), probe with `GET /` (or
  `_cluster/health` if Serverless allows it), and don't claim anything until a
  probe succeeds. The backlog resumes within one probe interval of recovery
  rather than waiting out up to an hour of per-row backoff.
- **When a document is bad,** keep today's exponential backoff, but after N
  attempts (e.g. 10) move the row to `search_dead_letter` (same columns plus
  the last response) instead of retrying hourly forever. Report it in
  `/api/search/status`.
- **Retirements** use the same gate: they wait while the breaker is open and
  are the first work done when it closes.
- **On breaker close,** run reconcile (§5), because an outage is exactly when
  drift builds up.

## 5. Continuous reconcile

Turn `worker::reconcile` from a one-shot startup check into a periodic task
(e.g. every 15 minutes, plus whenever the breaker closes), comparing both
directions:

| Found | Action |
|---|---|
| Active brain, index missing | queue provision + `enqueue_all(brain)` |
| Active brain, count differs, nothing queued | `enqueue_all(brain)` (today's behaviour) |
| Index with our `_meta`, brain gone, no retirement row | insert `index_retirement` (orphan) |
| Index matching `aibrain-*` **without** our `_meta` | log only; never delete what we can't prove we own |
| Index with our `_meta.uid` matching a brain being re-provisioned after a DB rebuild | adopt it: set `brain.index_name`, re-add aliases, skip re-embedding |

A count mismatch is a weak signal. Later, compare a per-brain checksum
(e.g. the max `note.updated_at` against a `max(indexed_at)` field stored in
the documents) so that an edit lost in transit is caught, not only an added or
missing document.

## 6. Other recommendations

- **Watcher follows config changes.** `watch::run` (`rust/aibrain-core/src/watch.rs:45`)
  picks its watch roots once at startup. A newly added vault isn't watched
  until a restart, and a removed one keeps producing FSEvents that are only
  filtered out later by `locate`. Subscribe the watcher to the lifecycle
  events from §3 and call `watch`/`unwatch` per brain.
- **Carry `indexed_at` in documents** so reconcile can use it (see §5) and
  searches can show how fresh a result is.
- **`/api/search/status`** should report: breaker state and last probe,
  queue depth per brain, oldest queued age, dead-letter count, pending
  retirements with their `last_error`, and orphaned indices found by
  reconcile.
- **Mapping version in `_meta.schema`.** When `mapping()` changes, reconcile
  can find indices on an old schema and move them to a new generation with an
  alias swap, rather than leaving them mismatched.
- **Integration tests** (in `es/integration.rs`, against a scratch prefix,
  never the live `aibrain-` indices):
  1. Retire while an upsert batch is leased. The late bulk is rejected with
     `require_alias`, and the index stays gone.
  2. Retire while Elasticsearch is unreachable (point the URL at a closed
     port). Postgres is clean at once, and the index drops when the URL is
     restored.
  3. A reindex holding a stale config (vault still listed) cannot resurrect
     the brain.
  4. Remove then re-add under the same name before the drop runs. The new
     index survives.
  5. A folded-name collision ("My Notes" / "my-notes") gives two indices.
  6. A breaker outage doesn't increment `attempts`.

## Suggested order

1. §2 `require_alias` + write aliases + remove the `known` cache. Small,
   closes the recreation hole on its own.
2. §1 `uid` / `index_name` columns, adopting existing indices in place.
3. §3 retirement outbox, the brain foreign key on `search_queue`, advisory
   locks, and the retire endpoint wired to Python's `remove_brain`.
4. §4 health gate and dead-letter.
5. §5 continuous reconcile with `_meta` ownership checks.
6. §6 watcher follow-up, status endpoint, `indexed_at`, schema versioning.
