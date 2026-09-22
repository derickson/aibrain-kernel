"""The searchable index.

One SQLite file holds every note, every resolved link, and an FTS5 table over
the text. Reindexing is incremental: a note is only re-read when its mtime or
size changed, so a rescan of a few thousand notes costs a stat() each.

Link resolution happens here rather than in the scanner because a wikilink is
only resolvable once every note in the vault is known. Obsidian resolves a
bare `[[Name]]` against the whole vault by basename, falling back to a path
match, so that is what we do — and we allow it to fall through to other vaults,
which is what produces the cross-brain arcs in the universe.
"""

from __future__ import annotations

import re
import sqlite3
import threading
import time
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable

from .config import BrainConfig
from .vault import Note, normalize, scan, strip_markup

SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS notes (
  id        INTEGER PRIMARY KEY,
  brain_id  TEXT NOT NULL,
  rel_path  TEXT NOT NULL,
  title     TEXT NOT NULL,
  source    TEXT NOT NULL,
  excerpt   TEXT NOT NULL DEFAULT '',
  tags      TEXT NOT NULL DEFAULT '',
  mtime     REAL NOT NULL DEFAULT 0,
  size      INTEGER NOT NULL DEFAULT 0,
  degree    INTEGER NOT NULL DEFAULT 0,
  UNIQUE(brain_id, rel_path)
);
CREATE INDEX IF NOT EXISTS notes_brain ON notes(brain_id);
CREATE INDEX IF NOT EXISTS notes_mtime ON notes(mtime DESC);

CREATE TABLE IF NOT EXISTS links (
  src_id INTEGER NOT NULL,
  dst_id INTEGER NOT NULL,
  PRIMARY KEY (src_id, dst_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS links_dst ON links(dst_id);

-- Every link target a note writes, resolved or not. Keeping the unresolved
-- ones is what lets the UI show that a note points at something that does not
-- exist yet, which in a vault usually means a note you meant to write.
CREATE TABLE IF NOT EXISTS dangling (
  src_id   INTEGER NOT NULL,
  target   TEXT NOT NULL,
  resolved INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS dangling_src ON dangling(src_id);

-- A plain (self-storing) FTS5 table: contentless tables cannot produce
-- snippet(), and the matched passage is what makes a citation readable.
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
  title, body, tags, source,
  tokenize='unicode61 remove_diacritics 2'
);

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
"""


@dataclass
class Hit:
    note_id: int
    brain_id: str
    rel_path: str
    title: str
    source: str
    snippet: str
    score: float


@dataclass
class NoteRow:
    id: int
    brain_id: str
    rel_path: str
    title: str
    source: str
    excerpt: str
    tags: str
    mtime: float
    size: int
    degree: int


def _row_to_note(row: sqlite3.Row) -> NoteRow:
    return NoteRow(
        id=row["id"], brain_id=row["brain_id"], rel_path=row["rel_path"],
        title=row["title"], source=row["source"], excerpt=row["excerpt"],
        tags=row["tags"], mtime=row["mtime"], size=row["size"], degree=row["degree"],
    )


# FTS5 treats these as syntax; a person typing them into a search box does not.
_FTS_SPECIAL = re.compile(r'["*():^]')


def fts_query(text: str, match: str = "all") -> str:
    """Turn a human query into a safe FTS5 MATCH expression.

    Every bare term becomes a prefix term so search feels live as you type.
    A quoted phrase is preserved as a phrase.

    `match` decides how the terms combine. FTS5 ANDs them by default, which is
    right for a search box — you type more to narrow down. It is wrong for a
    question, where "describe" or "tell" appears in none of your notes and one
    absent word takes the whole query to zero results. `match="any"` ORs them
    so a question still finds the notes it is about, leaving bm25 to rank.
    """
    text = text.strip()
    if not text:
        return ""
    phrases = re.findall(r'"([^"]+)"', text)
    rest = re.sub(r'"[^"]*"', " ", text)
    terms: list[str] = [f'"{_FTS_SPECIAL.sub(" ", p).strip()}"' for p in phrases if p.strip()]
    for word in _FTS_SPECIAL.sub(" ", rest).split():
        word = word.strip("-+")
        if not word:
            continue
        terms.append(f'"{word}"*' if not word.isdigit() else f'"{word}"')
    if not terms:
        return ""
    return (" OR " if match == "any" else " ").join(terms)


class Index:
    """Thread-safe-enough wrapper around the SQLite index.

    The HTTP server is threaded, so every connection is per-thread and writes
    are serialised behind one lock. That is plenty for a single-user local app.
    """

    def __init__(self, db_path: Path):
        self.db_path = db_path
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._local = threading.local()
        self._write_lock = threading.Lock()
        with self._connect() as conn:
            conn.executescript(SCHEMA)
            self._migrate(conn)

    def _migrate(self, conn: sqlite3.Connection) -> None:
        """Bring an index written by an older version up to the current schema."""
        columns = {r["name"] for r in conn.execute("PRAGMA table_info(dangling)")}
        if "resolved" not in columns:
            conn.execute("ALTER TABLE dangling ADD COLUMN resolved INTEGER NOT NULL DEFAULT 0")
            conn.commit()

    def _connect(self) -> sqlite3.Connection:
        conn = getattr(self._local, "conn", None)
        if conn is None:
            conn = sqlite3.connect(self.db_path, timeout=30, check_same_thread=False)
            conn.row_factory = sqlite3.Row
            conn.execute("PRAGMA foreign_keys=ON")
            self._local.conn = conn
        return conn

    @property
    def conn(self) -> sqlite3.Connection:
        return self._connect()

    # ---- metadata --------------------------------------------------------
    def get_meta(self, key: str, default: str = "") -> str:
        row = self.conn.execute("SELECT value FROM meta WHERE key=?", (key,)).fetchone()
        return row["value"] if row else default

    def set_meta(self, key: str, value: str) -> None:
        with self._write_lock:
            self.conn.execute(
                "INSERT INTO meta(key,value) VALUES(?,?) "
                "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                (key, str(value)),
            )
            self.conn.commit()

    # ---- indexing --------------------------------------------------------
    def reindex(
        self,
        brains: Iterable[BrainConfig],
        progress: Callable[[str], None] | None = None,
        force: bool = False,
    ) -> dict[str, int]:
        """Rescan every enabled vault and rebuild links.

        Returns counts so the caller can report what changed.
        """
        emit = progress or (lambda _msg: None)
        started = time.time()
        stats = {"scanned": 0, "added": 0, "updated": 0, "removed": 0, "links": 0}

        with self._write_lock:
            conn = self.conn
            brain_list = list(brains)
            keep_brains = {b.id for b in brain_list}

            # Drop brains that are gone or disabled.
            existing = {r["brain_id"] for r in conn.execute("SELECT DISTINCT brain_id FROM notes")}
            for gone in existing - keep_brains:
                emit(f"removing brain {gone}")
                ids = [r["id"] for r in conn.execute(
                    "SELECT id FROM notes WHERE brain_id=?", (gone,))]
                self._delete_ids(conn, ids)
                stats["removed"] += len(ids)

            for brain in brain_list:
                root = brain.resolved_path()
                if not root.is_dir():
                    emit(f"skipping {brain.name}: {root} is not a directory")
                    continue
                emit(f"scanning {brain.name} ({root})")

                known = {
                    r["rel_path"]: (r["id"], r["mtime"], r["size"])
                    for r in conn.execute(
                        "SELECT id, rel_path, mtime, size FROM notes WHERE brain_id=?",
                        (brain.id,),
                    )
                }
                seen: set[str] = set()
                count = 0
                for note in scan(root, brain.id, brain.exclude):
                    seen.add(note.rel_path)
                    count += 1
                    stats["scanned"] += 1
                    prior = known.get(note.rel_path)
                    if prior and not force and abs(prior[1] - note.mtime) < 0.001 \
                            and prior[2] == note.size:
                        continue
                    if prior:
                        self._update_note(conn, prior[0], note)
                        stats["updated"] += 1
                    else:
                        self._insert_note(conn, note)
                        stats["added"] += 1
                    if count % 500 == 0:
                        emit(f"  {brain.name}: {count} notes")

                stale = [known[p][0] for p in set(known) - seen]
                if stale:
                    self._delete_ids(conn, stale)
                    stats["removed"] += len(stale)
                emit(f"  {brain.name}: {count} notes on disk")

            conn.commit()
            emit("resolving links…")
            stats["links"] = self._rebuild_links(conn)
            conn.commit()

        elapsed = time.time() - started
        self.set_meta("last_index", str(time.time()))
        self.set_meta("last_index_stats", repr(stats))
        emit(
            f"done in {elapsed:.1f}s — {stats['scanned']} notes, "
            f"+{stats['added']} ~{stats['updated']} -{stats['removed']}, "
            f"{stats['links']} links"
        )
        return stats

    def _note_row(self, note: Note) -> tuple:
        return (
            note.brain_id, note.rel_path, note.title, note.source,
            note.excerpt(), ",".join(note.tags), note.mtime, note.size,
        )

    def _fts_row(self, note: Note) -> tuple:
        body = strip_markup(note.body)
        return (note.title, body[:200_000], " ".join(note.tags), note.source)

    def _insert_note(self, conn: sqlite3.Connection, note: Note) -> int:
        cur = conn.execute(
            "INSERT INTO notes(brain_id,rel_path,title,source,excerpt,tags,mtime,size)"
            " VALUES(?,?,?,?,?,?,?,?)",
            self._note_row(note),
        )
        note_id = int(cur.lastrowid)
        conn.execute(
            "INSERT INTO notes_fts(rowid,title,body,tags,source) VALUES(?,?,?,?,?)",
            (note_id, *self._fts_row(note)),
        )
        self._stage_targets(conn, note_id, note.links)
        return note_id

    def _update_note(self, conn: sqlite3.Connection, note_id: int, note: Note) -> None:
        conn.execute(
            "UPDATE notes SET brain_id=?,rel_path=?,title=?,source=?,excerpt=?,"
            "tags=?,mtime=?,size=? WHERE id=?",
            (*self._note_row(note), note_id),
        )
        conn.execute("DELETE FROM notes_fts WHERE rowid=?", (note_id,))
        conn.execute(
            "INSERT INTO notes_fts(rowid,title,body,tags,source) VALUES(?,?,?,?,?)",
            (note_id, *self._fts_row(note)),
        )
        conn.execute("DELETE FROM dangling WHERE src_id=?", (note_id,))
        conn.execute("DELETE FROM links WHERE src_id=?", (note_id,))
        self._stage_targets(conn, note_id, note.links)

    def _stage_targets(self, conn: sqlite3.Connection, note_id: int, targets: list[str]) -> None:
        if targets:
            conn.executemany(
                "INSERT INTO dangling(src_id,target) VALUES(?,?)",
                [(note_id, t) for t in targets[:200]],
            )

    def _delete_ids(self, conn: sqlite3.Connection, ids: list[int]) -> None:
        for chunk in (ids[i:i + 500] for i in range(0, len(ids), 500)):
            marks = ",".join("?" * len(chunk))
            conn.execute(f"DELETE FROM notes WHERE id IN ({marks})", chunk)
            conn.execute(f"DELETE FROM notes_fts WHERE rowid IN ({marks})", chunk)
            conn.execute(f"DELETE FROM links WHERE src_id IN ({marks})", chunk)
            conn.execute(f"DELETE FROM links WHERE dst_id IN ({marks})", chunk)
            conn.execute(f"DELETE FROM dangling WHERE src_id IN ({marks})", chunk)

    def _rebuild_links(self, conn: sqlite3.Connection) -> int:
        """Resolve staged targets into note ids.

        Preference order matches Obsidian: same vault by basename, same vault by
        path, then any vault by basename. The last one is what makes brains talk
        to each other.
        """
        by_base: dict[tuple[str, str], int] = {}
        by_path: dict[tuple[str, str], int] = {}
        global_base: dict[str, list[int]] = defaultdict(list)
        for row in conn.execute("SELECT id, brain_id, rel_path, title FROM notes"):
            nid, brain = row["id"], row["brain_id"]
            stem = row["rel_path"].rsplit("/", 1)[-1]
            if stem.endswith(".md"):
                stem = stem[:-3]
            for name in {normalize(stem), normalize(row["title"])}:
                by_base.setdefault((brain, name), nid)
                global_base[name].append(nid)
            path_key = normalize(row["rel_path"].removesuffix(".md"))
            by_path.setdefault((brain, path_key), nid)

        conn.execute("DELETE FROM links")
        conn.execute("UPDATE dangling SET resolved = 0")
        pairs: set[tuple[int, int]] = set()
        resolved_rows: list[int] = []
        src_brain = {r["id"]: r["brain_id"] for r in conn.execute("SELECT id, brain_id FROM notes")}

        rows = conn.execute("SELECT rowid, src_id, target FROM dangling").fetchall()
        for row in rows:
            src, target = row["src_id"], row["target"]
            brain = src_brain.get(src)
            if brain is None:
                continue
            key = normalize(target)
            dst = (
                by_base.get((brain, key))
                or by_path.get((brain, key))
                or by_base.get((brain, normalize(target.rsplit("/", 1)[-1])))
            )
            if dst is None:
                candidates = global_base.get(key) or []
                dst = candidates[0] if len(candidates) == 1 else None
            if dst is None:
                continue
            # A self-link resolves — it just does not become an edge.
            resolved_rows.append(row["rowid"])
            if dst != src:
                pairs.add((src, dst))

        conn.executemany("INSERT OR IGNORE INTO links(src_id,dst_id) VALUES(?,?)", pairs)
        conn.executemany("UPDATE dangling SET resolved = 1 WHERE rowid = ?",
                         [(r,) for r in resolved_rows])
        conn.execute(
            "UPDATE notes SET degree = ("
            "  SELECT COUNT(*) FROM links WHERE links.src_id = notes.id"
            "     OR links.dst_id = notes.id)"
        )
        return len(pairs)

    # ---- queries ---------------------------------------------------------
    def count(self, brain_id: str | None = None) -> int:
        if brain_id:
            row = self.conn.execute(
                "SELECT COUNT(*) c FROM notes WHERE brain_id=?", (brain_id,)).fetchone()
        else:
            row = self.conn.execute("SELECT COUNT(*) c FROM notes").fetchone()
        return int(row["c"])

    def brain_stats(self) -> list[dict]:
        rows = self.conn.execute(
            "SELECT brain_id, COUNT(*) notes, SUM(degree) deg, MAX(mtime) newest"
            " FROM notes GROUP BY brain_id"
        ).fetchall()
        return [
            {
                "brain_id": r["brain_id"],
                "notes": r["notes"],
                "degree": r["deg"] or 0,
                "newest": r["newest"] or 0,
            }
            for r in rows
        ]

    def note(self, note_id: int) -> NoteRow | None:
        row = self.conn.execute("SELECT * FROM notes WHERE id=?", (note_id,)).fetchone()
        return _row_to_note(row) if row else None

    def note_by_path(self, brain_id: str, rel_path: str) -> NoteRow | None:
        row = self.conn.execute(
            "SELECT * FROM notes WHERE brain_id=? AND rel_path=?", (brain_id, rel_path)
        ).fetchone()
        return _row_to_note(row) if row else None

    def neighbours(self, note_id: int, limit: int = 40) -> list[NoteRow]:
        rows = self.conn.execute(
            "SELECT n.* FROM notes n WHERE n.id IN ("
            "  SELECT dst_id FROM links WHERE src_id=?"
            "  UNION SELECT src_id FROM links WHERE dst_id=?"
            ") ORDER BY n.degree DESC LIMIT ?",
            (note_id, note_id, limit),
        ).fetchall()
        return [_row_to_note(r) for r in rows]

    def outgoing_unresolved(self, note_id: int) -> list[str]:
        """Targets this note links to that match nothing in any vault."""
        rows = self.conn.execute(
            "SELECT DISTINCT target FROM dangling WHERE src_id=? AND resolved=0",
            (note_id,),
        ).fetchall()
        return [r["target"] for r in rows]

    def search(
        self,
        query: str,
        limit: int = 60,
        brain_ids: list[str] | None = None,
        snippets: bool = True,
        match: str = "all",
    ) -> list[Hit]:
        expr = fts_query(query, match)
        if not expr:
            return []
        sql = [
            "SELECT n.id, n.brain_id, n.rel_path, n.title, n.source, n.degree,",
            "  bm25(notes_fts, 8.0, 1.0, 3.0, 2.0) AS score,",
            ("  snippet(notes_fts, 1, '<mark>', '</mark>', '…', 18) AS snip"
             if snippets else "  '' AS snip"),
            " FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid",
            " WHERE notes_fts MATCH ?",
        ]
        params: list = [expr]
        if brain_ids:
            sql.append(f" AND n.brain_id IN ({','.join('?' * len(brain_ids))})")
            params.extend(brain_ids)
        # bm25 returns negative numbers, lower is better; nudge hubs up a little
        # so a well-connected note beats an identical orphan.
        sql.append(" ORDER BY score + (-0.05 * MIN(n.degree, 20)) LIMIT ?")
        params.append(limit)
        try:
            rows = self.conn.execute("".join(sql), params).fetchall()
        except sqlite3.OperationalError:
            return []
        return [
            Hit(
                note_id=r["id"], brain_id=r["brain_id"], rel_path=r["rel_path"],
                title=r["title"], source=r["source"], snippet=r["snip"] or "",
                score=-float(r["score"]),
            )
            for r in rows
        ]

    def recent(self, limit: int = 20, brain_ids: list[str] | None = None) -> list[NoteRow]:
        sql = "SELECT * FROM notes"
        params: list = []
        if brain_ids:
            sql += f" WHERE brain_id IN ({','.join('?' * len(brain_ids))})"
            params.extend(brain_ids)
        sql += " ORDER BY mtime DESC LIMIT ?"
        params.append(limit)
        return [_row_to_note(r) for r in self.conn.execute(sql, params)]

    def all_for_graph(self, brain_id: str, limit: int) -> list[sqlite3.Row]:
        """Notes for one brain, most connected and most recent first.

        The cap keeps the universe at a frame rate a laptop can hold; the
        ordering means what gets dropped is the least connected tail.
        """
        return self.conn.execute(
            "SELECT id, rel_path, title, source, degree, mtime FROM notes"
            " WHERE brain_id=? ORDER BY degree DESC, mtime DESC LIMIT ?",
            (brain_id, limit),
        ).fetchall()

    def links_among(self, ids: set[int]) -> list[tuple[int, int]]:
        if not ids:
            return []
        out: list[tuple[int, int]] = []
        id_list = list(ids)
        for chunk in (id_list[i:i + 900] for i in range(0, len(id_list), 900)):
            marks = ",".join("?" * len(chunk))
            rows = self.conn.execute(
                f"SELECT src_id, dst_id FROM links WHERE src_id IN ({marks})", chunk
            ).fetchall()
            out.extend((r["src_id"], r["dst_id"]) for r in rows if r["dst_id"] in ids)
        return out
