"""The Absorbitron: an intake folder for links and files people send you.

Each intake note lives in `<vault>/Absorbitron/` of the brain that catches
meeting transcripts, named `<YYYY-MM-DD HHmm> <title>.md`. The file is the
raw input you typed, then a rule and an `# AI Notes` section (a placeholder
until the research step exists), then `# Attachments`, listing wikilinks to
files copied into `Absorbitron/Attachments/`.

Moving a note out of `Absorbitron/` files it into the knowledge base: it stops
being an intake note (Rust decides that from the path alone) and keeps its AI
notes and its attachment links, which are full vault paths and so still
resolve.

Pure helpers only; the routes that use them are in server.py.
"""

from __future__ import annotations

import os
import re
from datetime import datetime
from pathlib import Path

# Must match ABSORBITRON_FOLDER in rust/aibrain-core/src/layout.rs.
FOLDER = "Absorbitron"
ATTACHMENTS = "Attachments"
# Big enough for a PDF deck or a phone video, small enough to hold in memory.
MAX_ATTACHMENT_BYTES = 200 * 1024 * 1024
MAX_TITLE_CHARS = 160

AI_NOTES_PLACEHOLDER = (
    "_Placeholder — AI expansion and research on the links and attachments "
    "above will be added here._"
)

# Characters a file name may not carry and still be a working [[wikilink]]
# in Obsidian, plus the ones macOS, Windows or a shell make awkward.
_UNSAFE = re.compile(r'[\\/:*?"<>|#^\[\]\x00-\x1f\x7f]')


def stamp(now: datetime | None = None, seconds: bool = False) -> str:
    """`2026-11-10 1432`, or `2026-11-10 143205` to break a tie."""
    return (now or datetime.now()).strftime("%Y-%m-%d %H%M%S" if seconds else "%Y-%m-%d %H%M")


def clean_title(raw: str) -> str:
    """A title fit to be part of a file name, or "" if nothing is left.

    Unlike a note created from a [[link]] — where a bad name is refused so a
    link never quietly points somewhere else — this name is new and only
    ever typed into the sticky, so it is tidied rather than refused.
    """
    title = _UNSAFE.sub(" ", raw or "")
    title = re.sub(r"\s+", " ", title).strip().lstrip(".").strip()
    title = re.sub(r"\.{2,}", ".", title)
    return title[:MAX_TITLE_CHARS].rstrip()


def note_name(title: str, now: datetime | None = None, seconds: bool = False) -> str:
    """The file stem for a new intake note: the timestamp, then the title."""
    return f"{stamp(now, seconds)} {title}".strip()


def template(raw_input: str = "") -> str:
    """A new intake note: your words, then the sections the AI fills later.

    The blank line before the rule matters: a `---` straight under a line of
    text would turn that line into a heading instead of drawing a rule.
    """
    body = (raw_input or "").rstrip()
    return (
        f"{body}\n\n"
        "---\n"
        "# AI Notes\n\n"
        f"{AI_NOTES_PLACEHOLDER}\n\n"
        "# Attachments\n\n"
    )


def attachment_name(original: str, now: datetime | None = None, seconds: bool = False) -> str:
    """`2026-11-10 1432 report.pdf` from `report.pdf`, made link-safe."""
    base = os.path.basename((original or "").replace("\\", "/"))
    stem, ext = os.path.splitext(base)
    stem = clean_title(stem) or "attachment"
    ext = re.sub(r"[^A-Za-z0-9.]", "", ext)[:16]
    return f"{stamp(now, seconds)} {stem}{ext}"


def attachments_dir(vault: Path) -> Path:
    """`<vault>/Absorbitron/Attachments`, created, and confined to the vault.

    Resolved after creation so a symlinked folder pointing out of the vault
    is refused instead of written through.
    """
    root = vault.resolve()
    target = root / FOLDER / ATTACHMENTS
    target.mkdir(parents=True, exist_ok=True)
    real = target.resolve()
    if root not in real.parents:
        raise PermissionError("the attachments folder resolves outside its vault")
    return real


def write_attachment(vault: Path, original: str, data: bytes,
                     now: datetime | None = None) -> str:
    """Copy one upload into the attachments folder; the vault-relative path.

    Never overwrites: a name taken this minute gets seconds, then a counter.
    """
    folder = attachments_dir(vault)
    candidates = [attachment_name(original, now), attachment_name(original, now, seconds=True)]
    stem, ext = os.path.splitext(candidates[1])
    candidates += [f"{stem} {n}{ext}" for n in range(2, 100)]
    for name in candidates:
        try:
            # O_EXCL: refuses an existing file and will not follow a symlink
            # planted at the name.
            fd = os.open(folder / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        except FileExistsError:
            continue
        with os.fdopen(fd, "wb") as out:
            out.write(data)
        return f"{FOLDER}/{ATTACHMENTS}/{name}"
    raise FileExistsError("could not find a free name for that attachment")
