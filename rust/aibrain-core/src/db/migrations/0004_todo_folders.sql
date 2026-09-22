-- Folders for the to-do list: a persistent backlog that sits below the daily
-- list, unaffected by which day is on screen or by rollover. Filing an item
-- into a folder means "I've organized this, stop carrying it forward" — see
-- `for_day` and `roll_over` in db/todo.rs, both of which now skip anything
-- with a folder_id.

CREATE TABLE IF NOT EXISTS todo_folder (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT        NOT NULL,
    collapsed  BOOLEAN     NOT NULL DEFAULT FALSE,
    sort_order DOUBLE PRECISION NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- SET NULL, not CASCADE: deleting a folder un-files its members rather than
-- touching todo's append-only history. An unfiled item with a stale
-- scheduled_on is picked up by the very next roll_over, so nothing else has
-- to notice a folder went away.
ALTER TABLE todo ADD COLUMN IF NOT EXISTS folder_id BIGINT
    REFERENCES todo_folder(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS todo_folder_idx ON todo (folder_id);
