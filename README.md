# aibrain-kernel

Tooling that feeds and maintains a personal knowledge vault in Obsidian.
Source systems on this Mac (starting with MacWhisper meeting transcripts) are
mirrored into the vault as plain Markdown with stable identifiers, so that
downstream notes and agents can build on them and trace back to the original
record.

## Project structure

```
aibrain-kernel/
├── README.md                  this file
├── .gitignore                 ignores Python caches and the vault symlink
├── scripts/                   one subfolder per source system or tool
│   └── macwhisper/
│       ├── README.md                         usage, tailing, refresh, drift detection
│       ├── macwhisper_export.py              exporter (stdlib only, Python 3)
│       └── macwhisper_schema_baseline.json   MacWhisper DB schema the script was verified against
├── obsidian_vaults/           local symlinks to vaults (git-ignored)
│   └── AgenticTest -> ~/Documents/ObsidianVaults/AgenticTest
└── graft/                     local cache for the graft code-index tool (not source)
```

### `scripts/`

Each source system gets its own folder containing the script, its
documentation, and any fixtures it needs to detect upstream change. The folder
README is the authoritative usage doc; this file only describes how the pieces
fit together.

**`scripts/macwhisper/`**: tails MacWhisper's local SQLite database and writes
each transcript into the vault's `raw_transcript/` folder. Key behaviors:

- Quits MacWhisper, verifies nothing holds the database open, reads it in
  read-only mode, then relaunches the app.
- Validates the live schema against `macwhisper_schema_baseline.json` before
  reading anything. Changes to the tables it depends on stop the run; changes
  elsewhere are reported and the run continues.
- Tails new sessions newest-first, stopping at the first already-exported one.
- Refreshes sessions from the last 14 days by content comparison, because
  MacWhisper does not timestamp its most common edit (speaker renames).
- Each file carries YAML frontmatter with MacWhisper's stable session and
  meeting UUIDs, the recording time, and the last export time.

```sh
python3 scripts/macwhisper/macwhisper_export.py            # export anything new
python3 scripts/macwhisper/macwhisper_export.py --dry-run  # preview
```

See [`scripts/macwhisper/README.md`](scripts/macwhisper/README.md) for flags,
exit codes, and the drift-detection workflow.

### `obsidian_vaults/`

Symlinks into the real vault locations under `~/Documents/ObsidianVaults/`,
so scripts and agents can address vault paths relative to the repo. The
symlinks are machine-specific and git-ignored; recreate one with:

```sh
ln -s ../../Documents/ObsidianVaults/AgenticTest obsidian_vaults/AgenticTest
```

Current vault layout that this repo writes to:

```
AgenticTest/
└── raw_transcript/     one Markdown file per MacWhisper session,
                        named "YYYY-MM-DD HH-MM-SS <title>.md"
```

Files in `raw_transcript/` are a raw mirror of MacWhisper and are overwritten
on refresh. Curated notes should live elsewhere in the vault and link to them.

### `graft/`

Cache written by the graft code-indexing tool used during development. It is
not part of the project and can be deleted at any time.

## Conventions

- Scripts are Python 3 standard library only, so they run with the system
  interpreter and no virtualenv.
- Every exporter treats its source as read-only and verifies the source files
  are unchanged after a run.
- Upstream schema assumptions are recorded as a checked-in baseline and
  validated on every run, so a silent upstream change becomes a visible report
  instead of corrupted vault content.
- Exported Markdown carries the source system's own stable ids in frontmatter.

## Status

Initial commit plus the MacWhisper exporter. Nothing else is wired up yet.
