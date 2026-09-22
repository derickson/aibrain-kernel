-- The work queue between ingest and Elasticsearch.
--
-- Ingest is synchronous and must stay fast; an embedding call per save would
-- make every keystroke wait on a network round trip. So ingest writes a row
-- here and a background worker drains it in batches.
--
-- There is deliberately no foreign key to `note`: a delete has to survive the
-- note row disappearing, which is the whole point of recording it.

CREATE TABLE IF NOT EXISTS search_queue (
    id           BIGSERIAL PRIMARY KEY,
    brain_id     TEXT        NOT NULL,
    brain_name   TEXT        NOT NULL DEFAULT '',
    note_id      BIGINT,
    rel_path     TEXT        NOT NULL,
    op           TEXT        NOT NULL CHECK (op IN ('upsert', 'delete')),
    enqueued_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts     INTEGER     NOT NULL DEFAULT 0,
    last_error   TEXT,
    locked_until TIMESTAMPTZ,
    -- One row per note, not one per edit. A note saved five times before the
    -- worker wakes is one document to send, carrying the latest operation.
    UNIQUE (brain_id, rel_path)
);

CREATE INDEX IF NOT EXISTS search_queue_ready_idx ON search_queue (enqueued_at, id);
CREATE INDEX IF NOT EXISTS search_queue_brain_idx ON search_queue (brain_id);
