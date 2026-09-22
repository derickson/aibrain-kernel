# MacWhisper transcript export

`macwhisper_export.py` (run from the repo root as `python3 scripts/macwhisper/macwhisper_export.py`) tails MacWhisper's local database and writes any
new transcripts as Markdown into a target folder, in the same format as
MacWhisper's own Markdown export. It quits MacWhisper, refuses to proceed if
any process still has the database open, reads the live SQLite files in
read-only mode, writes new files, verifies the database files are unchanged,
and relaunches MacWhisper.

By default it writes into `raw_transcripts/` at the repo root — a staging
folder, not a vault. Files land there and are expected to get moved
("absorbed") into the right spot in an actual Obsidian vault by a later,
separate step; this script's job ends at getting them out of MacWhisper.

```sh
# Tail into the default staging folder (raw_transcripts/ at the repo root)
python3 scripts/macwhisper/macwhisper_export.py

# Tail into a specific target folder instead
python3 scripts/macwhisper/macwhisper_export.py ~/Notes/meetings

# See what would be written without writing anything
python3 scripts/macwhisper/macwhisper_export.py ~/Notes/meetings --dry-run

# Set the target folder once via environment
export MACWHISPER_EXPORT_DIR=~/Notes/meetings
python3 scripts/macwhisper/macwhisper_export.py

# Read a different database; --no-app-control never quits MacWhisper and refuses to run if it is open
python3 scripts/macwhisper/macwhisper_export.py --db /path/to/main.sqlite ~/Notes/meetings --no-app-control
```

Files are named `YYYY-MM-DD HH-MM-SS <title>.md` using the recording's local
start time.

## Tracking what has already been pulled

The staging folder's contents are not trusted as a record of what has been
exported — once a transcript is absorbed into a vault it leaves the folder,
so a missing file doesn't mean "not yet exported." Instead
`scripts/macwhisper/export_state.sqlite3` (local to this folder, gitignored,
independent of the target folder) records every session this script has
ever pulled, keyed by its stable `macwhisper_session_id`. The newest
recorded session time in that db is the high-water mark; only sessions newer
than the mark and not already recorded are written, and a run is a no-op
when nothing is new. Point at a different ledger with `--state-db`.

Each file starts with YAML frontmatter carrying MacWhisper's stable ids, and
the body below it is byte-identical to MacWhisper's own Markdown export:

```yaml
---
macwhisper_session_id: EA072EF6-8135-4E84-83BA-76B3E1F5C04C   # session.id, stable across edits
macwhisper_meeting_id: 643FDF4C-5A7C-49EB-8661-EBDE9DCA7415   # recordedmeeting.id, when present
recorded_at: 2026-09-21T17:39:02-04:00
source_app: "Zoom"
title: "Meeting - Zoom"
script_exported_at: 2026-09-21T20:15:03-04:00                 # last time this script wrote the file
---
```

## Refresh of recent transcripts (change data capture)

Transcripts can be edited inside MacWhisper after the fact. Text edits bump
`session.dateUpdated`, but renaming a speaker, the most common edit, leaves no
timestamp anywhere in the database. So the script does not trust timestamps:
after the tail it looks up every session recorded within the last
`--lookback-days` (default 14, `0` disables) in the state db and, if its file
is still sitting in the staging folder, re-renders it, renaming it if the
title changed and rewriting it if the content differs byte-for-byte
(ignoring only the `script_exported_at` line). A session whose file is no
longer in the staging folder is assumed already absorbed into a vault and is
left alone — the script does not go looking for it elsewhere.

Until absorbed, the staging folder is treated as a raw mirror of MacWhisper:
hand edits to a file still there, inside the window, will be overwritten on
the next run.

```sh
python3 scripts/macwhisper/macwhisper_export.py --lookback-days 30   # widen the window
python3 scripts/macwhisper/macwhisper_export.py --lookback-days 0    # tail only
```

## Schema drift detection

MacWhisper can change its database layout in any release without notice.
Before reading a single transcript the script validates the live database
against `scripts/macwhisper/macwhisper_schema_baseline.json` on three layers:

1. **Baseline diff**: every table's DDL and columns, the full GRDB migration
   list, and the MacWhisper version.
2. **Required-field contract**: the exact tables and columns the export
   queries depend on, with their declared types.
3. **Data-shape checks**: ids are 16-byte blobs, datetimes parse, transcript
   timestamps are integer milliseconds consistent with meeting durations,
   exportable sessions have lines, speaker references resolve.

Every difference is printed to stdout. A difference inside the tables the
script reads (`session`, `transcriptline`, `speaker`, `recordedmeeting`), a
contract failure, or a data-shape failure is **blocking**: nothing is
exported and the exit code is 3. Changes elsewhere (other tables, new
migrations, version bumps) are **informational**: they are listed and the
export proceeds. A blocking report is the signal to review, update the
script, verify its output against a manual MacWhisper export, and then
accept the new shape:

```sh
python3 scripts/macwhisper/macwhisper_export.py --validate-only    # just run the checks
python3 scripts/macwhisper/macwhisper_export.py --write-baseline   # accept the current schema
```

Exit codes: 0 ok, 1 error, 2 refused because MacWhisper is running with
`--no-app-control`, 3 validation failed.
