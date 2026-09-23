-- Elasticsearch index lifecycle: stable names, durable retirement, dead letters.
--
-- See design_concepts/RECOMMENDATION_ELASTICSEARCH.md. The short version: an
-- index used to be named after the vault's display name, recomputed on every
-- write, and was never dropped. Now each brain row carries a `uid` that is
-- never reused, the index name is recorded once, and removing a brain writes a
-- row to `index_retirement` that the worker turns into an index delete.
--
-- migrate() runs every statement on every start, so each one is idempotent.
-- The splitter breaks on a line ending in `;`, which is why the DO block below
-- is written on one line.

ALTER TABLE brain ADD COLUMN IF NOT EXISTS uid            TEXT;
-- Recorded, never recomputed. NULL until the worker has provisioned it.
ALTER TABLE brain ADD COLUMN IF NOT EXISTS index_name     TEXT;
-- Set once the index exists with its aliases. The worker claims queue rows
-- only for brains that have this, so it never writes before provisioning.
ALTER TABLE brain ADD COLUMN IF NOT EXISTS index_ready_at TIMESTAMPTZ;
-- True for brains that existed before this migration: their documents already
-- live in the old name-derived index, which is adopted rather than rebuilt.
ALTER TABLE brain ADD COLUMN IF NOT EXISTS adopt_legacy   BOOLEAN NOT NULL DEFAULT FALSE;

-- Order matters: mark the pre-existing rows before giving them a uid, because
-- "no uid yet" is how they are recognised. On every later start both are no-ops.
UPDATE brain SET adopt_legacy = TRUE WHERE uid IS NULL;
UPDATE brain SET uid = substr(replace(gen_random_uuid()::text, '-', ''), 1, 20) WHERE uid IS NULL;
ALTER TABLE brain ALTER COLUMN uid SET DEFAULT substr(replace(gen_random_uuid()::text, '-', ''), 1, 20);
ALTER TABLE brain ALTER COLUMN uid SET NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS brain_uid_idx        ON brain (uid);
CREATE UNIQUE INDEX IF NOT EXISTS brain_index_name_idx ON brain (index_name);

-- One row per index we have decided to destroy. The outbox: it survives
-- restarts and outages, and it is the only thing that ever deletes an index.
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
CREATE INDEX IF NOT EXISTS index_retirement_due_idx
    ON index_retirement (not_before) WHERE done_at IS NULL;

-- Documents Elasticsearch refused too many times. Kept rather than retried
-- hourly forever, and counted in /status so someone notices.
CREATE TABLE IF NOT EXISTS search_dead_letter (
    id           BIGINT      PRIMARY KEY,
    brain_id     TEXT        NOT NULL,
    brain_name   TEXT        NOT NULL DEFAULT '',
    note_id      BIGINT,
    rel_path     TEXT        NOT NULL,
    op           TEXT        NOT NULL,
    enqueued_at  TIMESTAMPTZ NOT NULL,
    attempts     INTEGER     NOT NULL,
    last_error   TEXT,
    dead_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Queue rows now belong to a brain: deleting the brain purges its pending work
-- in the same statement. Per-note deletes still outlive their note (there is
-- still no key to `note`); a removed brain's documents go with its index.
-- Rows left by a brain dropped before this existed would violate the key.
DELETE FROM search_queue q WHERE NOT EXISTS (SELECT 1 FROM brain b WHERE b.id = q.brain_id);
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'search_queue_brain_fk') THEN ALTER TABLE search_queue ADD CONSTRAINT search_queue_brain_fk FOREIGN KEY (brain_id) REFERENCES brain(id) ON DELETE CASCADE; END IF; END $$;
