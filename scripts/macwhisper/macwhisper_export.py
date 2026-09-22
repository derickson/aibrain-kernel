#!/usr/bin/env python3
"""Export MacWhisper transcripts to Markdown, with schema drift detection.

Flow:
  1. Quit MacWhisper gracefully and wait for its process to exit.
  2. Verify nothing is touching the database: no MacWhisper process exists
     and no process has main.sqlite / -wal / -shm open (lsof). Abort otherwise.
  3. Open the live main.sqlite with mode=ro inside a single read transaction.

     Why not immutable=1? MacWhisper (GRDB) keeps a persistent WAL and does
     not checkpoint on quit, so the whole schema lives in the -wal file.
     immutable=1 disables WAL handling and reports "no such table". mode=ro
     reads the WAL, never writes database content, and only takes a shared
     lock; the only file it may touch is the -shm scratch index.

  4. VALIDATE before reading any transcript (see "Drift detection" below).
     Every difference from the committed baseline is printed to stdout.
     Differences inside the tables this script reads stop the run (exit 3,
     nothing written); differences elsewhere are informational and the
     export proceeds.
  5. Write one Markdown file per new session, named after the recording date.
     A small YAML frontmatter block carries stable identifiers; the body is
     byte-identical to MacWhisper's own Markdown export:

         ---
         macwhisper_session_id: EA072EF6-8135-4E84-83BA-76B3E1F5C04C
         macwhisper_meeting_id: 643FDF4C-5A7C-49EB-8661-EBDE9DCA7415
         recorded_at: 2026-09-21T17:39:02-04:00
         source_app: Zoom
         title: "Meeting - Zoom"
         script_exported_at: 2026-09-21T20:15:03-04:00
         ---
         **Speaker**
         *MM:SS*
         text

     script_exported_at is the last time THIS SCRIPT wrote the file. It is the
     one volatile field, so the refresh pass ignores that line when comparing
     old and new content; everything else in the file must be stable.

  6. Close the DB, report whether any DB file changed (none should), then
     relaunch MacWhisper if it was running before.

Tailing: the output folder is the source of truth for what has been
exported. The newest timestamp found in existing filenames
("YYYY-MM-DD HH-MM-SS ...md") is the high-water mark. Sessions are walked
newest-first; each one newer than the mark is written, and the walk stops at
the first session that is not newer. If the newest session is already
exported the run is a no-op (apart from the quit/relaunch).

An empty output folder exports everything.

Refresh (change data capture): transcripts can be edited inside MacWhisper
after the fact, and the most common edit, renaming a speaker, leaves no
timestamp anywhere in the database (verified empirically; text edits do bump
session.dateUpdated, speaker renames do not). So after the tail, every
session whose recording time falls inside --lookback-days (default 14) is
re-rendered and compared byte-for-byte with its existing file, matched by
the macwhisper_session_id in its frontmatter (falling back to the
"YYYY-MM-DD HH-MM-SS" filename prefix for files written before frontmatter
existed). If the title changed the file is renamed;
if the content changed the file is rewritten. Files below the high-water
mark that are missing are NOT recreated (a deliberate deletion stays
deleted). NOTE: hand edits to files inside the window will be overwritten;
the target folder is treated as a raw mirror of MacWhisper.

Drift detection:
  MacWhisper's developer can change the database at any release without
  notice. Garbled transcripts in the vault are worse than no transcripts, so
  the script refuses to export if the parts of the database it reads have
  changed, and reports everything else so drift is never silent. Three layers:

    a. Baseline diff. scripts/macwhisper/macwhisper_schema_baseline.json records every
       table's CREATE statement and columns, the full GRDB migration list,
       and the MacWhisper version. Any added/removed/changed table, column,
       type, or migration is reported. BLOCKING only when the change is in a
       tracked table (the keys of REQUIRED); otherwise informational.
    b. Required-field contract (BLOCKING). The specific tables and columns
       this script reads must exist with the expected declared types,
       independent of the baseline.
    c. Data-shape checks on live rows (BLOCKING): datetimes parse, timestamps
       are integer milliseconds with start <= end, ids are 16-byte blobs,
       transcript spans roughly match recorded meeting durations.

  A missing baseline file is informational: layers b and c still guard the
  export, and the report tells you to run --write-baseline.

  When validation fails, review the report, fix this script if needed, then
  accept the new shape with:  macwhisper_export.py --write-baseline

Usage:
  macwhisper_export.py                       # tail into the default vault folder
  macwhisper_export.py TARGET_FOLDER         # tail into TARGET_FOLDER
  macwhisper_export.py TARGET_FOLDER --dry-run
  macwhisper_export.py --lookback-days 30    # widen the refresh window (0 disables)
  macwhisper_export.py --validate-only       # run drift detection, export nothing
  macwhisper_export.py --write-baseline      # (re)record the schema baseline
  macwhisper_export.py --db /path/to/main.sqlite TARGET_FOLDER

Examples:
  python3 scripts/macwhisper/macwhisper_export.py ~/Documents/ObsidianVaults/AgenticTest/raw_transcript
  python3 scripts/macwhisper/macwhisper_export.py ~/Notes/meetings --dry-run
  MACWHISPER_EXPORT_DIR=~/Notes/meetings python3 scripts/macwhisper/macwhisper_export.py

The target folder is resolved in this order: positional argument,
--out-dir, $MACWHISPER_EXPORT_DIR, then the built-in default.

Exit codes: 0 ok, 1 error, 2 refused (MacWhisper running), 3 validation failed.
"""

from __future__ import annotations

import argparse
import json
import os
import plistlib
import re
import sqlite3
import subprocess
import sys
import time
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

APP_NAME = "MacWhisper"
APP_BUNDLE = Path("/Applications/MacWhisper.app")
DEFAULT_DB = Path.home() / "Library/Application Support/MacWhisper/Database/main.sqlite"
DEFAULT_OUT = Path(
    os.environ.get("MACWHISPER_EXPORT_DIR")
    or Path.home() / "Documents/ObsidianVaults/AgenticTest/raw_transcript"
).expanduser()
BASELINE_PATH = Path(__file__).with_name("macwhisper_schema_baseline.json")
QUIT_TIMEOUT_S = 30

EXIT_OK, EXIT_ERROR, EXIT_REFUSED, EXIT_VALIDATION = 0, 1, 2, 3


# --------------------------------------------------------------------------- app control
def app_pids() -> list[int]:
    r = subprocess.run(["pgrep", "-x", APP_NAME], capture_output=True, text=True)
    return [int(p) for p in r.stdout.split()]


def app_version() -> str | None:
    try:
        with open(APP_BUNDLE / "Contents/Info.plist", "rb") as f:
            info = plistlib.load(f)
        return f"{info.get('CFBundleShortVersionString')} ({info.get('CFBundleVersion')})"
    except Exception:
        return None


def db_files(db: Path) -> list[Path]:
    return [db.with_name(db.name + sfx) for sfx in ("", "-wal", "-shm")]


def db_file_holders(db: Path) -> list[str]:
    """PIDs/commands of any process with a DB file open (via lsof)."""
    existing = [str(f) for f in db_files(db) if f.exists()]
    try:
        r = subprocess.run(["lsof", "-Fpc", "--", *existing], capture_output=True, text=True)
    except FileNotFoundError:
        print("warning: lsof not found; cannot verify DB files are unheld.", file=sys.stderr)
        return []
    holders, pid = [], None
    for line in r.stdout.splitlines():
        if line.startswith("p"):
            pid = line[1:]
        elif line.startswith("c") and pid is not None:
            holders.append(f"{line[1:]}({pid})")
            pid = None
    return sorted(set(holders))


def ensure_not_running(db: Path) -> None:
    """Hard gate before opening the DB. Raises SystemExit if anything is unsafe."""
    pids = app_pids()
    if pids:
        raise SystemExit(f"refusing to open DB: {APP_NAME} is running (pids {pids}).")
    holders = db_file_holders(db)
    if holders:
        raise SystemExit(f"refusing to open DB: files are held open by {', '.join(holders)}.")


def fingerprint(db: Path) -> dict[str, tuple[int, int]]:
    return {f.name: (f.stat().st_mtime_ns, f.stat().st_size) for f in db_files(db) if f.exists()}


def quit_app() -> None:
    subprocess.run(
        ["osascript", "-e", f'tell application "{APP_NAME}" to quit'],
        check=False, capture_output=True, text=True,
    )
    deadline = time.time() + QUIT_TIMEOUT_S
    while time.time() < deadline:
        if not app_pids():
            return
        time.sleep(0.25)
    raise SystemExit(f"{APP_NAME} did not quit within {QUIT_TIMEOUT_S}s; aborting without touching the DB.")


def launch_app(attempts: int = 8, delay_s: float = 1.0) -> None:
    """Relaunch the app. LaunchServices can return -600 (procNotFound) if asked
    too soon after a quit, so retry until a process appears."""
    for i in range(attempts):
        # -g: don't bring it to the foreground
        r = subprocess.run(["open", "-g", "-a", APP_NAME], capture_output=True, text=True)
        time.sleep(delay_s)
        if app_pids():
            return
        if r.stderr.strip():
            print(f"  launch attempt {i + 1} failed: {r.stderr.strip()}", file=sys.stderr)
    print(f"warning: could not confirm {APP_NAME} relaunched; start it manually.", file=sys.stderr)


# --------------------------------------------------------------------------- helpers
def parse_db_datetime(s: str) -> datetime:
    """MacWhisper stores 'YYYY-MM-DD HH:MM:SS.fff' in UTC. Return local time."""
    for fmt in ("%Y-%m-%d %H:%M:%S.%f", "%Y-%m-%d %H:%M:%S"):
        try:
            return datetime.strptime(s, fmt).replace(tzinfo=timezone.utc).astimezone()
        except ValueError:
            continue
    raise ValueError(f"unrecognised datetime: {s!r}")


def fmt_timestamp(ms: int) -> str:
    total = ms // 1000
    h, rem = divmod(total, 3600)
    m, s = divmod(rem, 60)
    return f"{h}:{m:02d}:{s:02d}" if h else f"{m:02d}:{s:02d}"


FILENAME_TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2} \d{2}-\d{2}-\d{2}) .*\.md$")
FILENAME_TS_FMT = "%Y-%m-%d %H-%M-%S"


def latest_exported(out_dir: Path) -> datetime | None:
    """Newest recording timestamp already present in out_dir, from filenames."""
    latest = None
    if not out_dir.is_dir():
        return None
    for f in out_dir.iterdir():
        m = FILENAME_TS_RE.match(f.name)
        if not m:
            continue
        ts = datetime.strptime(m.group(1), FILENAME_TS_FMT).astimezone()
        if latest is None or ts > latest:
            latest = ts
    return latest


def blob_uuid(b: bytes | None) -> str | None:
    """MacWhisper stores UUIDs as 16 raw bytes; present them the way it does in filenames."""
    return str(uuid.UUID(bytes=bytes(b))).upper() if b else None


def yaml_str(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


FRONTMATTER_ID_RE = re.compile(r"^macwhisper_session_id:\s*([0-9A-Fa-f-]{36})\s*$", re.M)


def index_by_session_id(out_dir: Path) -> dict[str, list[Path]]:
    """Map session UUID -> files whose frontmatter carries it (reads only the head of each file)."""
    idx: dict[str, list[Path]] = {}
    if not out_dir.is_dir():
        return idx
    for f in out_dir.glob("*.md"):
        try:
            with f.open("r", encoding="utf-8") as fh:
                head = fh.read(2048)
        except OSError:
            continue
        if not head.startswith("---"):
            continue
        m = FRONTMATTER_ID_RE.search(head.split("\n---", 1)[0])
        if m:
            idx.setdefault(m.group(1).upper(), []).append(f)
    return idx


def files_with_stamp(out_dir: Path, stamp: str) -> list[Path]:
    """Existing exports for a recording, matched by timestamp prefix only."""
    return sorted(f for f in out_dir.glob(f"{stamp} *.md") if f.is_file())


def safe_filename(s: str) -> str:
    s = re.sub(r'[\\/:*?"<>|]+', "-", s).strip().strip(".")
    return s or "Transcript"


# --------------------------------------------------------------------------- db
SESSIONS_SQL = """
SELECT s.id,
       s.dateCreated,
       s.userChosenTitle, s.aiTitle, s.originalFilename,
       m.date AS meetingDate,
       m.id   AS meetingID,
       m.appName
FROM session s
LEFT JOIN recordedmeeting m ON m.id = s.recordedMeetingID
WHERE s.dateDeleted IS NULL
  AND s.isTransient = 0
  AND s.transcriptionDidSucceed = 1
ORDER BY s.dateCreated DESC
"""

LINES_SQL = """
SELECT tl.start, tl.text, sp.name
FROM transcriptline tl
LEFT JOIN speaker sp ON sp.id = tl.speakerID
WHERE tl.sessionId = ?
ORDER BY tl.orderIndex, tl.start
"""


def open_readonly(db: Path) -> sqlite3.Connection:
    """Open the live DB read-only, belt-and-braces:
    - mode=ro: connection cannot write
    - query_only: any write statement is rejected at the SQL layer too
    - a single deferred read transaction: consistent snapshot for the run
    - schema check: fail loudly if the WAL contents are not visible
    """
    uri = f"file:{db.resolve()}?mode=ro"
    conn = sqlite3.connect(uri, uri=True, timeout=5, isolation_level=None)
    try:
        conn.execute("PRAGMA query_only = 1")
        conn.execute("BEGIN")
        have = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if not have:
            raise SystemExit("DB opened but contains no tables; WAL not visible? aborting.")
    except Exception:
        conn.close()
        raise
    return conn


VOLATILE_FM_RE = re.compile(r"^script_exported_at:.*\n", re.M)


def strip_volatile(text: str) -> str:
    """Remove frontmatter lines that legitimately change on every write."""
    return VOLATILE_FM_RE.sub("", text, count=1)


def render_frontmatter(meta: dict) -> str:
    fm = ["---", f"macwhisper_session_id: {meta['session_id']}"]
    if meta.get("meeting_id"):
        fm.append(f"macwhisper_meeting_id: {meta['meeting_id']}")
    fm.append(f"recorded_at: {meta['recorded_at']}")
    if meta.get("source_app"):
        fm.append(f"source_app: {yaml_str(meta['source_app'])}")
    fm.append(f"title: {yaml_str(meta['title'])}")
    fm.append(f"script_exported_at: {datetime.now().astimezone().replace(microsecond=0).isoformat()}")
    fm.append("---")
    return "\n".join(fm) + "\n"


def render_body(lines) -> str:
    out = []
    for start, text, speaker in lines:
        out.append(f"**{speaker or 'Unknown'}**\n*{fmt_timestamp(start)}*\n{text}\n")
    # MacWhisper's own export ends with a blank line, so keep the trailing "\n".
    return "\n".join(out) + "\n"


def render_markdown(meta: dict, lines) -> str:
    return render_frontmatter(meta) + render_body(lines)


# --------------------------------------------------------------------------- drift detection
# (b) Required-field contract: table -> {column: declared type}. Every column the
# SQL above or the render code depends on must be listed here.
REQUIRED: dict[str, dict[str, str]] = {
    "session": {
        "id": "BLOB", "dateCreated": "DATETIME", "userChosenTitle": "TEXT", "aiTitle": "TEXT",
        "originalFilename": "TEXT", "recordedMeetingID": "BLOB", "dateDeleted": "DOUBLE",
        "isTransient": "BOOLEAN", "transcriptionDidSucceed": "BOOLEAN",
    },
    "transcriptline": {
        "id": "BLOB", "sessionId": "BLOB", "text": "TEXT", "start": "INTEGER", "end": "INTEGER",
        "orderIndex": "INTEGER", "speakerID": "BLOB",
    },
    "speaker": {"id": "BLOB", "name": "TEXT"},
    "recordedmeeting": {"id": "BLOB", "date": "DATETIME", "duration": "DOUBLE", "appName": "TEXT"},
    "grdb_migrations": {"identifier": "TEXT"},
}


def capture_schema(conn: sqlite3.Connection) -> dict:
    """Everything we compare against the baseline. Deterministic ordering."""
    tables = {}
    for name, sql in conn.execute(
        "SELECT name, sql FROM sqlite_master WHERE type='table' ORDER BY name"
    ):
        cols = [
            {"name": c[1], "type": c[2], "notnull": c[3], "pk": c[5]}
            for c in conn.execute(f'PRAGMA table_info("{name}")')
        ]
        tables[name] = {"sql": sql, "columns": cols}
    migrations = [r[0] for r in conn.execute("SELECT identifier FROM grdb_migrations ORDER BY rowid")] \
        if "grdb_migrations" in tables else []
    return {
        "app_version": app_version(),
        "user_version": conn.execute("PRAGMA user_version").fetchone()[0],
        "tables": tables,
        "migrations": migrations,
    }


TRACKED_TABLES = frozenset(REQUIRED) - {"grdb_migrations"}


def diff_baseline(base: dict, live: dict) -> list[tuple[bool, str]]:
    """(a) Differences between baseline and live schema as (blocking, message).
    Blocking = the change touches a table this script reads."""
    out: list[tuple[bool, str]] = []
    info = lambda m: out.append((False, m))

    if base.get("app_version") != live.get("app_version"):
        info(f"MacWhisper version: baseline {base.get('app_version')!r} -> live {live.get('app_version')!r}")
    if base.get("user_version") != live.get("user_version"):
        info(f"PRAGMA user_version: {base.get('user_version')} -> {live.get('user_version')}")

    bm, lm = base.get("migrations", []), live.get("migrations", [])
    if bm != lm:
        added = [m for m in lm if m not in bm]
        removed = [m for m in bm if m not in lm]
        info(f"migrations: baseline {len(bm)} -> live {len(lm)}")
        for m in added:
            info(f"  + new migration: {m!r}")
        for m in removed:
            info(f"  - missing migration: {m!r}")
        if not added and not removed:
            info("  (same set, different order)")

    bt, lt = base.get("tables", {}), live.get("tables", {})
    for t in sorted(set(bt) | set(lt)):
        blocking = t in TRACKED_TABLES
        tag = " [tracked]" if blocking else ""
        if t not in lt:
            out.append((blocking, f"table removed: {t}{tag}"))
            continue
        if t not in bt:
            out.append((blocking, f"table added: {t}{tag}"))
            out.append((blocking, f"  {lt[t]['sql']}"))
            continue
        bc = {c["name"]: c for c in bt[t]["columns"]}
        lc = {c["name"]: c for c in lt[t]["columns"]}
        for c in sorted(set(bc) | set(lc)):
            if c not in lc:
                out.append((blocking, f"column removed: {t}.{c} (was {bc[c]['type']}){tag}"))
            elif c not in bc:
                out.append((blocking, f"column added: {t}.{c} {lc[c]['type']}{tag}"))
            elif bc[c] != lc[c]:
                out.append((blocking, f"column changed: {t}.{c} {bc[c]} -> {lc[c]}{tag}"))
        if bt[t]["sql"] != lt[t]["sql"] and bc == lc:
            out.append((blocking, f"table DDL changed (constraints/defaults) for {t}{tag}:"))
            out.append((blocking, f"  baseline: {bt[t]['sql']}"))
            out.append((blocking, f"  live:     {lt[t]['sql']}"))
    return out


def check_required(live: dict) -> list[str]:
    """(b) The contract this script's queries depend on."""
    out = []
    for t, cols in REQUIRED.items():
        if t not in live["tables"]:
            out.append(f"REQUIRED table missing: {t}")
            continue
        have = {c["name"]: c["type"] for c in live["tables"][t]["columns"]}
        for c, typ in cols.items():
            if c not in have:
                out.append(f"REQUIRED column missing: {t}.{c}")
            elif have[c].upper() != typ:
                out.append(f"REQUIRED column type changed: {t}.{c} expected {typ}, got {have[c]}")
    return out


def check_data_shapes(conn: sqlite3.Connection) -> list[str]:
    """(c) Semantic checks on live rows. Cheap, and they catch changes that
    leave the schema intact (e.g. seconds instead of milliseconds)."""
    out = []
    q = lambda sql, *a: conn.execute(sql, a).fetchall()

    # ids are 16-byte UUID blobs
    for t in ("session", "transcriptline", "speaker", "recordedmeeting"):
        bad = q(f"SELECT count(*) FROM {t} WHERE typeof(id)!='blob' OR length(id)!=16")[0][0]
        if bad:
            out.append(f"{t}.id: {bad} row(s) are not 16-byte blobs")
    bad = q("SELECT count(*) FROM transcriptline WHERE typeof(sessionId)!='blob' OR length(sessionId)!=16")[0][0]
    if bad:
        out.append(f"transcriptline.sessionId: {bad} row(s) are not 16-byte blobs")

    # datetimes parse with our format
    for t, c in (("session", "dateCreated"), ("recordedmeeting", "date")):
        for (v,) in q(f"SELECT {c} FROM {t} WHERE {c} IS NOT NULL LIMIT 200"):
            try:
                parse_db_datetime(v)
            except Exception:
                out.append(f"{t}.{c}: unparseable value {v!r}")
                break
    for (v,) in q("SELECT typeof(dateCreated) FROM session GROUP BY 1"):
        if v != "text":
            out.append(f"session.dateCreated stored as {v}, expected text")

    # transcript timing/text types
    bad = q("SELECT count(*) FROM transcriptline WHERE typeof(start)!='integer' OR typeof(\"end\")!='integer'")[0][0]
    if bad:
        out.append(f"transcriptline.start/end: {bad} row(s) are not integers")
    bad = q("SELECT count(*) FROM transcriptline WHERE start > \"end\"")[0][0]
    if bad:
        out.append(f"transcriptline: {bad} row(s) have start > end")
    bad = q("SELECT count(*) FROM transcriptline WHERE typeof(text)!='text'")[0][0]
    if bad:
        out.append(f"transcriptline.text: {bad} row(s) are not text")
    bad = q("SELECT count(*) FROM speaker WHERE typeof(name)!='text'")[0][0]
    if bad:
        out.append(f"speaker.name: {bad} row(s) are not text")

    # millisecond sanity: transcript span vs recorded meeting duration (seconds)
    rows = q("""
        SELECT hex(s.id), m.duration, max(tl."end")
        FROM session s
        JOIN recordedmeeting m ON m.id = s.recordedMeetingID
        JOIN transcriptline tl ON tl.sessionId = s.id
        WHERE m.duration IS NOT NULL AND m.duration > 0
        GROUP BY s.id
    """)
    for sid, dur, max_end in rows:
        ratio = max_end / (dur * 1000.0)
        if not (0.2 <= ratio <= 1.5):
            out.append(f"session {sid[:8]}: transcript ends at {max_end} ms but meeting lasted {dur:.1f} s "
                       f"(ratio {ratio:.2f}); start/end may no longer be milliseconds")

    # exportable sessions should have lines and speaker joins should resolve
    orphan_sessions = q("""
        SELECT count(*) FROM session s
        WHERE s.dateDeleted IS NULL AND s.isTransient = 0 AND s.transcriptionDidSucceed = 1
          AND NOT EXISTS (SELECT 1 FROM transcriptline tl WHERE tl.sessionId = s.id)
    """)[0][0]
    if orphan_sessions:
        out.append(f"{orphan_sessions} exportable session(s) have no transcriptline rows; "
                   f"transcript storage may have moved")
    dangling = q("""
        SELECT count(*) FROM transcriptline tl
        WHERE tl.speakerID IS NOT NULL
          AND NOT EXISTS (SELECT 1 FROM speaker sp WHERE sp.id = tl.speakerID)
    """)[0][0]
    if dangling:
        out.append(f"{dangling} transcriptline row(s) reference a speakerID with no speaker row")
    return out


def validate(conn: sqlite3.Connection, baseline_path: Path) -> tuple[bool, dict]:
    """Run all three layers. Print a report to STDOUT. Return (ok, live_schema).

    ok is False only for BLOCKING findings: required-field contract failures,
    data-shape failures, or baseline drift inside a tracked table. Everything
    else is printed as informational and the export proceeds."""
    live = capture_schema(conn)
    required = check_required(live)
    shapes = check_data_shapes(conn) if not required else ["(skipped: required-field contract failed)"]

    if baseline_path.exists():
        base = json.loads(baseline_path.read_text())
        drift = diff_baseline(base, live)
        baseline_note = f"baseline: {baseline_path} (recorded against MacWhisper {base.get('app_version')})"
    else:
        drift = [(False, f"no baseline file at {baseline_path}; run --write-baseline after reviewing the schema")]
        baseline_note = "baseline: MISSING (required-field and data-shape checks still enforced)"

    blocking_drift = [m for b, m in drift if b]
    info_drift = [m for b, m in drift if not b]
    ok = not (required or blocking_drift or shapes)

    print(f"validation: live MacWhisper {live['app_version']}, {len(live['tables'])} tables, "
          f"{len(live['migrations'])} migrations")
    print(baseline_note)

    if ok:
        if info_drift:
            print(f"validation: OK, with {len(info_drift)} informational change(s) outside the tables this "
                  f"script reads (export continues; run --write-baseline to silence):")
            for line in info_drift:
                print(f"  ~ {line}")
        else:
            print("validation: OK (schema, required fields, and data shapes all match)")
        return True, live

    print()
    print("=" * 78)
    print("VALIDATION FAILED - a part of the MacWhisper database this script reads has")
    print("changed. No transcripts were exported.")
    print("=" * 78)
    for title, items in (("Required-field contract", required),
                         ("Schema drift in tracked tables", blocking_drift),
                         ("Data-shape checks", shapes)):
        print(f"\n[{title}] BLOCKING")
        if items:
            for line in items:
                print(f"  {line}")
        else:
            print("  ok")
    print("\n[Other drift] informational")
    if info_drift:
        for line in info_drift:
            print(f"  ~ {line}")
    else:
        print("  none")
    print("\nNext step: review the differences above, update scripts/macwhisper/macwhisper_export.py")
    print("(SESSIONS_SQL / LINES_SQL / REQUIRED / render_markdown) if needed, verify the")
    print("output against a manual MacWhisper export, then run --write-baseline.")
    return False, live


# --------------------------------------------------------------------------- main
def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("target", nargs="?", type=Path, metavar="TARGET_FOLDER",
                    help=f"folder to write transcripts into (default: {DEFAULT_OUT})")
    ap.add_argument("--out-dir", type=Path, default=None, help="same as TARGET_FOLDER; kept for compatibility")
    ap.add_argument("--db", type=Path, default=DEFAULT_DB, help=f"path to main.sqlite (default: {DEFAULT_DB})")
    ap.add_argument("--baseline", type=Path, default=BASELINE_PATH,
                    help=f"schema baseline JSON (default: {BASELINE_PATH})")
    ap.add_argument("--lookback-days", type=float, default=14,
                    help="re-render sessions recorded within this many days and update changed files (0 disables)")
    ap.add_argument("--dry-run", action="store_true", help="report what would be written without writing")
    ap.add_argument("--validate-only", action="store_true", help="run drift detection and exit; export nothing")
    ap.add_argument("--write-baseline", action="store_true",
                    help="record the live schema as the new baseline (after you have reviewed it); export nothing")
    ap.add_argument("--no-app-control", action="store_true",
                    help="never quit/relaunch MacWhisper; the run is refused if it is running")
    args = ap.parse_args()
    args.out_dir = (args.target or args.out_dir or DEFAULT_OUT).expanduser()
    if not (args.validate_only or args.write_baseline):
        print(f"target folder: {args.out_dir}")

    if not args.db.exists():
        print(f"database not found: {args.db}", file=sys.stderr)
        return EXIT_ERROR

    was_running = bool(app_pids())
    if was_running and args.no_app_control:
        print(f"{APP_NAME} is running and --no-app-control was given; refusing to read its DB.", file=sys.stderr)
        return EXIT_REFUSED
    if was_running:
        print(f"quitting {APP_NAME}...")
        quit_app()

    written = 0
    updated = 0
    rc = EXIT_OK
    try:
        # Hard gate: nothing may be running or holding the DB files, regardless of flags.
        # Inside the try so a refusal still relaunches the app in `finally`.
        ensure_not_running(args.db)
        before = fingerprint(args.db)
        conn = open_readonly(args.db)
        try:
            if args.write_baseline:
                live = capture_schema(conn)
                problems = check_required(live) + check_data_shapes(conn)
                if problems:
                    print("refusing to write baseline: required-field/data checks fail:")
                    for p in problems:
                        print(f"  {p}")
                    return EXIT_VALIDATION
                args.baseline.write_text(json.dumps(live, indent=2, sort_keys=True) + "\n")
                print(f"baseline written: {args.baseline} (MacWhisper {live['app_version']}, "
                      f"{len(live['tables'])} tables, {len(live['migrations'])} migrations)")
                return EXIT_OK

            ok, _ = validate(conn, args.baseline)
            if not ok:
                return EXIT_VALIDATION
            if args.validate_only:
                return EXIT_OK

            mark = latest_exported(args.out_dir)
            print(f"high-water mark: {mark.strftime(FILENAME_TS_FMT) if mark else 'none (empty folder)'}")

            # Build (recording_time, row) and walk newest-first by recording time.
            rows = []
            for sid, created, user_title, ai_title, orig_name, meeting_date, meeting_id, app_name \
                    in conn.execute(SESSIONS_SQL):
                # Truncate to whole seconds so it compares cleanly with filename stamps.
                when = parse_db_datetime(meeting_date or created).replace(microsecond=0)
                title = user_title or ai_title or orig_name or "Transcript"
                meta = {
                    "session_id": blob_uuid(sid),
                    "meeting_id": blob_uuid(meeting_id),
                    "recorded_at": when.isoformat(),
                    "source_app": app_name,
                    "title": title,
                }
                rows.append((when, sid, title, meta))
            rows.sort(key=lambda r: r[0], reverse=True)
            by_id = index_by_session_id(args.out_dir)

            if not rows:
                print("no transcripts in database.")
            args.out_dir.mkdir(parents=True, exist_ok=True)

            # ---- pass 1: tail. Newest-first; stop at the first already-exported session.
            for when, sid, title, meta in rows:
                stamp = when.strftime(FILENAME_TS_FMT)
                if mark is not None and when <= mark:
                    print(f"reached already-exported session {stamp}; stopping tail.")
                    break
                if meta["session_id"] in by_id:
                    print(f"skip (session already exported as {by_id[meta['session_id']][0].name})")
                    continue
                path = args.out_dir / f"{stamp} {safe_filename(title)}.md"
                lines = conn.execute(LINES_SQL, (sid,)).fetchall()
                if not lines:
                    print(f"skip (no transcript lines): {stamp} {title}")
                    continue
                if args.dry_run:
                    print(f"would write: {path}  ({len(lines)} lines)")
                    written += 1
                    continue
                path.write_text(render_markdown(meta, lines), encoding="utf-8")
                written += 1
                print(f"wrote: {path}  ({len(lines)} lines)")

            # ---- pass 2: refresh. Re-render already-exported sessions inside the lookback
            # window and reconcile filename (title) and content (text/speaker edits).
            if mark is not None and args.lookback_days > 0:
                cutoff = datetime.now().astimezone() - timedelta(days=args.lookback_days)
                print(f"refresh window: sessions since {cutoff.strftime(FILENAME_TS_FMT)} "
                      f"({args.lookback_days:g} days)")
                for when, sid, title, meta in rows:
                    if when > mark or when < cutoff:
                        continue
                    stamp = when.strftime(FILENAME_TS_FMT)
                    # Match by session id in frontmatter first; legacy files by filename stamp.
                    existing = by_id.get(meta["session_id"]) or files_with_stamp(args.out_dir, stamp)
                    expected = args.out_dir / f"{stamp} {safe_filename(title)}.md"
                    if not existing:
                        print(f"refresh: {stamp} has no file (deleted?); not recreated")
                        continue
                    if len(existing) > 1:
                        print(f"refresh: {stamp} matches {len(existing)} files; ambiguous, skipped: "
                              f"{[f.name for f in existing]}")
                        continue
                    current = existing[0]
                    lines = conn.execute(LINES_SQL, (sid,)).fetchall()
                    if not lines:
                        print(f"refresh: {stamp} now has no transcript lines in DB; file left as is")
                        continue
                    new_text = render_markdown(meta, lines)
                    old_text = current.read_text(encoding="utf-8")
                    rename = current != expected
                    # Compare with the volatile line removed; a file that lacks it entirely
                    # (written by an older version of this script) is rewritten once to gain it.
                    rewrite = (strip_volatile(old_text) != strip_volatile(new_text)
                               or not VOLATILE_FM_RE.search(old_text))
                    if not (rename or rewrite):
                        continue
                    what = " + ".join(w for w, on in (("rename", rename), ("content", rewrite)) if on)
                    if args.dry_run:
                        print(f"would update ({what}): {current.name}"
                              + (f" -> {expected.name}" if rename else ""))
                        updated += 1
                        continue
                    if rename:
                        if expected.exists():
                            print(f"refresh: cannot rename {current.name} -> {expected.name}: target exists; "
                                  f"updating content in place")
                            expected = current
                        else:
                            current.rename(expected)
                    if rewrite or rename:
                        expected.write_text(new_text, encoding="utf-8")
                    updated += 1
                    print(f"updated ({what}): {expected.name}")
        finally:
            try:
                conn.execute("ROLLBACK")
            except sqlite3.Error:
                pass
            conn.close()
            after = fingerprint(args.db)
            changed = [n for n in after if before.get(n) != after[n]]
            content_changed = [n for n in changed if not n.endswith("-shm")]
            if content_changed:
                print(f"WARNING: database content files changed during the read: {content_changed}", file=sys.stderr)
            else:
                shm_note = " (-shm scratch index refreshed, expected)" if changed else ""
                print(f"verified: main.sqlite and -wal unchanged{shm_note}.")
    finally:
        if was_running:
            print(f"relaunching {APP_NAME}...")
            launch_app()

    if not (args.validate_only or args.write_baseline):
        if written or updated:
            verb = "would be" if args.dry_run else ""
            print(f"done: {written} new file(s) {verb} written, {updated} existing file(s) {verb} updated."
                  .replace("  ", " "))
        else:
            print("done: nothing new, nothing changed.")
    return rc


if __name__ == "__main__":
    sys.exit(main())
