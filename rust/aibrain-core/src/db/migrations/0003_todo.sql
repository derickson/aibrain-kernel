-- The day's list. Unlike everything in 0001, this IS a source of truth: no
-- markdown file behind it, nothing to rebuild it from. So nothing is ever
-- deleted — an item that is done or abandoned keeps its row, and todo_event
-- keeps the story of how it got there.

CREATE TABLE IF NOT EXISTS todo (
    id                 BIGSERIAL PRIMARY KEY,
    body               TEXT        NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The day it was first asked for, kept apart from scheduled_on so a task
    -- that has rolled over nine times can still say how long it has been open.
    first_scheduled_on DATE        NOT NULL,
    scheduled_on       DATE        NOT NULL,
    completed_at       TIMESTAMPTZ,
    cancelled_at       TIMESTAMPTZ,
    sort_order         DOUBLE PRECISION NOT NULL DEFAULT 0
);

-- The day list reads by scheduled_on; the same day's completions read by
-- completed_at, as an instant range rather than a date, because which day an
-- instant belongs to depends on the configured start hour.
CREATE INDEX IF NOT EXISTS todo_scheduled_idx ON todo (scheduled_on);
CREATE INDEX IF NOT EXISTS todo_completed_idx ON todo (completed_at);
CREATE INDEX IF NOT EXISTS todo_cancelled_idx ON todo (cancelled_at);
-- Rollover asks one question on every day view: what is still open and stale.
CREATE INDEX IF NOT EXISTS todo_open_idx ON todo (scheduled_on)
    WHERE completed_at IS NULL AND cancelled_at IS NULL;

-- History is searched, not scrolled. Spelled with the explicit regconfig for
-- the same reason note.tsv is: the one-argument form is not immutable.
ALTER TABLE todo ADD COLUMN IF NOT EXISTS tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english'::regconfig, coalesce(body, ''))) STORED;
CREATE INDEX IF NOT EXISTS todo_tsv_idx ON todo USING GIN (tsv);

-- A to-do's notes, held by path AND by id. The id is the fast join; the path
-- is what survives a reindex that gave the note a new row, so a rename can be
-- repaired rather than silently dropping the link.
CREATE TABLE IF NOT EXISTS todo_ref (
    id       BIGSERIAL PRIMARY KEY,
    todo_id  BIGINT NOT NULL REFERENCES todo(id) ON DELETE CASCADE,
    brain_id TEXT   NOT NULL,
    rel_path TEXT   NOT NULL,
    note_id  BIGINT REFERENCES note(id) ON DELETE SET NULL,
    UNIQUE (todo_id, brain_id, rel_path)
);
CREATE INDEX IF NOT EXISTS todo_ref_todo_idx ON todo_ref (todo_id);
CREATE INDEX IF NOT EXISTS todo_ref_note_idx ON todo_ref (note_id);

-- Every mutation lands here: created, rolled, completed, uncompleted,
-- cancelled, rescheduled, linked, edited. from_day/to_day are filled only by
-- the two kinds that move an item between days.
CREATE TABLE IF NOT EXISTS todo_event (
    id       BIGSERIAL PRIMARY KEY,
    todo_id  BIGINT      NOT NULL REFERENCES todo(id) ON DELETE CASCADE,
    at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    kind     TEXT        NOT NULL,
    from_day DATE,
    to_day   DATE
);
CREATE INDEX IF NOT EXISTS todo_event_todo_idx ON todo_event (todo_id, at);
