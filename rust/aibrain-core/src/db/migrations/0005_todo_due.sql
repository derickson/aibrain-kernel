-- The list stops being a calendar. There is one list, not one per day, and a
-- date on an item is an optional due date rather than the page it lives on —
-- so no rollover either: nothing needs carrying forward when nothing is
-- filed under a day.
--
-- In one DO block because it must run exactly once and every file here is
-- re-applied on each start: the rename is the marker that it already has.
--
-- Open items keep a date only if someone chose it (a `rescheduled` event
-- landed them on it). Every other date was the day view's bookkeeping — the
-- day it was typed, or the day it was rolled onto — and would otherwise show
-- up as a wall of false "overdue" badges. Closed items are history and are
-- left as they were.
--
-- first_scheduled_on goes: the `created` event's to_day holds the same fact.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.columns
                WHERE table_name = 'todo' AND column_name = 'scheduled_on') THEN
        ALTER TABLE todo RENAME COLUMN scheduled_on TO due_on;
        ALTER TABLE todo ALTER COLUMN due_on DROP NOT NULL;
        UPDATE todo t SET due_on = NULL
         WHERE t.completed_at IS NULL AND t.cancelled_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM todo_event e
                            WHERE e.todo_id = t.id AND e.kind = 'rescheduled'
                              AND e.to_day = t.due_on);
        ALTER TABLE todo DROP COLUMN IF EXISTS first_scheduled_on;
    END IF;
END
$$;

-- Both served the day view (and rollover); nothing reads by date any more.
DROP INDEX IF EXISTS todo_scheduled_idx;
DROP INDEX IF EXISTS todo_open_idx;
