"""Reading an Obsidian vault into plain records.

Everything here is deliberately tolerant: a vault is somebody's pile of
markdown, not a schema. Anything that fails to parse still becomes a note with
a title and a body, because losing a note is worse than losing its metadata.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterator

MD_SUFFIXES = {".md", ".markdown"}

# [[Target]] / [[Target|Alias]] / [[Target#Heading]] / ![[Embed]]
WIKILINK_RE = re.compile(r"!?\[\[([^\[\]]+?)\]\]")
MD_LINK_RE = re.compile(r"\[[^\]]*\]\(([^)\s]+\.md)\)")
TAG_RE = re.compile(r"(?:^|\s)#([A-Za-z0-9][\w/-]*)")
FRONTMATTER_RE = re.compile(r"\A---\r?\n(.*?)\r?\n---\r?\n?", re.S)
HEADING_RE = re.compile(r"^#{1,6}\s+(.*)$", re.M)
CODE_FENCE_RE = re.compile(r"```.*?```", re.S)


@dataclass
class Note:
    """One markdown file in a vault."""

    brain_id: str
    rel_path: str          # posix path relative to the vault root
    title: str
    source: str            # top-level folder, the ribbon this note sits in
    body: str
    frontmatter: dict[str, str]
    links: list[str]       # raw wikilink targets, unresolved
    tags: list[str]
    mtime: float
    size: int
    headings: list[str] = field(default_factory=list)

    @property
    def key(self) -> str:
        return f"{self.brain_id}::{self.rel_path}"

    def excerpt(self, limit: int = 400) -> str:
        text = strip_markup(self.body).strip()
        if len(text) <= limit:
            return text
        cut = text[:limit]
        space = cut.rfind(" ")
        return (cut[:space] if space > limit * 0.6 else cut).rstrip() + "…"


def normalize(text: str) -> str:
    """Fold a link target or title into a comparable key."""
    text = unicodedata.normalize("NFKD", text)
    text = "".join(c for c in text if not unicodedata.combining(c))
    return " ".join(text.lower().replace("_", " ").replace("-", " ").split())


def parse_frontmatter(text: str) -> tuple[dict[str, str], str]:
    """Pull YAML-ish frontmatter off the top.

    We only need scalars and simple lists; a real YAML parser is not in the
    stdlib and the values we care about (title, aliases, tags, ids) are flat.
    """
    m = FRONTMATTER_RE.match(text)
    if not m:
        return {}, text
    body = text[m.end():]
    data: dict[str, str] = {}
    key = None
    for line in m.group(1).splitlines():
        if not line.strip() or line.strip() in {"{}", "---"}:
            continue
        if line.lstrip().startswith("- ") and key:
            item = line.lstrip()[2:].strip().strip("\"'")
            data[key] = f"{data[key]}, {item}" if data.get(key) else item
            continue
        if ":" in line and not line.startswith(" "):
            key, _, value = line.partition(":")
            key = key.strip()
            data[key] = value.strip().strip("\"'")
    return data, body


def strip_markup(text: str) -> str:
    """A readable plain-text rendering, for excerpts and search snippets."""
    text = CODE_FENCE_RE.sub(" ", text)
    text = WIKILINK_RE.sub(lambda m: m.group(1).split("|")[-1].split("#")[0], text)
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", " ", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"[*_`>#]+", " ", text)
    text = re.sub(r"^\s*[-+*]\s+", "", text, flags=re.M)
    return re.sub(r"[ \t]+", " ", text)


def link_targets(text: str) -> list[str]:
    """Wikilink and relative-markdown-link targets, in document order."""
    out: list[str] = []
    seen: set[str] = set()
    for m in WIKILINK_RE.finditer(text):
        raw = m.group(1).split("|")[0].split("#")[0].strip()
        if raw and raw not in seen:
            seen.add(raw)
            out.append(raw)
    for m in MD_LINK_RE.finditer(text):
        raw = m.group(1)
        if raw.startswith(("http://", "https://")):
            continue
        raw = raw.rsplit("/", 1)[-1]
        if raw.endswith(".md"):
            raw = raw[:-3]
        if raw and raw not in seen:
            seen.add(raw)
            out.append(raw)
    return out


def note_tags(text: str, frontmatter: dict[str, str]) -> list[str]:
    tags = {t.strip() for t in frontmatter.get("tags", "").split(",") if t.strip()}
    body = CODE_FENCE_RE.sub(" ", text)
    for m in TAG_RE.finditer(body):
        tags.add(m.group(1))
    return sorted(tags)


def _is_excluded(rel: Path, exclude: list[str]) -> bool:
    parts = set(rel.parts)
    for pattern in exclude:
        if pattern in parts:
            return True
        if pattern.endswith("/") and pattern.rstrip("/") in parts:
            return True
    return False


def walk_vault(root: Path, exclude: list[str]) -> Iterator[Path]:
    """Markdown files in a vault, skipping excluded folders and dotdirs."""
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            entries = list(current.iterdir())
        except (OSError, PermissionError):
            continue
        for entry in entries:
            name = entry.name
            if name.startswith("."):
                continue
            try:
                if entry.is_dir():
                    if name in exclude:
                        continue
                    stack.append(entry)
                elif entry.suffix.lower() in MD_SUFFIXES:
                    rel = entry.relative_to(root)
                    if not _is_excluded(rel, exclude):
                        yield entry
            except (OSError, PermissionError):
                continue


def read_note(path: Path, root: Path, brain_id: str) -> Note | None:
    try:
        stat = path.stat()
        raw = path.read_text(encoding="utf-8", errors="replace")
    except (OSError, PermissionError):
        return None

    frontmatter, body = parse_frontmatter(raw)
    rel = path.relative_to(root)
    parts = rel.parts
    source = parts[0] if len(parts) > 1 else "Root"
    title = frontmatter.get("title") or path.stem

    return Note(
        brain_id=brain_id,
        rel_path=rel.as_posix(),
        title=title,
        source=source,
        body=body,
        frontmatter=frontmatter,
        links=link_targets(body),
        tags=note_tags(body, frontmatter),
        mtime=stat.st_mtime,
        size=stat.st_size,
        headings=HEADING_RE.findall(body)[:12],
    )


def scan(root: Path, brain_id: str, exclude: list[str]) -> Iterator[Note]:
    for path in walk_vault(root, exclude):
        note = read_note(path, root, brain_id)
        if note is not None:
            yield note
