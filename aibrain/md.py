"""A small markdown renderer for the note reader.

Not a general implementation — it covers what Obsidian notes actually contain:
headings, lists, quotes, tables, fenced code, emphasis, links, and wikilinks.
Wikilinks are the point: each one is resolved against the index so clicking it
opens the linked note and flies the camera to it.
"""

from __future__ import annotations

import html
import re

from .index import Index
from .vault import normalize

FENCE_RE = re.compile(r"^(```|~~~)(.*)$")
WIKILINK_RE = re.compile(r"(!?)\[\[([^\[\]]+?)\]\]")
MD_LINK_RE = re.compile(r"\[([^\]]*)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
IMG_RE = re.compile(r"!\[([^\]]*)\]\(([^)\s]+)\)")
BOLD_RE = re.compile(r"\*\*([^*]+)\*\*")
ITALIC_RE = re.compile(r"(?<![\w*])\*([^*\n]+)\*(?![\w*])")
CODE_RE = re.compile(r"`([^`\n]+)`")
STRIKE_RE = re.compile(r"~~([^~]+)~~")
TAG_RE = re.compile(r"(?<![\w&#/])#([A-Za-z][\w/-]*)")
URL_RE = re.compile(r"(?<![\"'=(>])\bhttps?://[^\s<>\"')]+")
TASK_RE = re.compile(r"^\s*[-*]\s+\[([ xX])\]\s+(.*)$")
LIST_RE = re.compile(r"^(\s*)([-*+]|\d+[.)])\s+(.*)$")
HEADING_RE = re.compile(r"^(#{1,6})\s+(.*)$")
TABLE_SEP_RE = re.compile(r"^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$")


class LinkResolver:
    """Resolves `[[targets]]` to note ids, preferring the current vault."""

    def __init__(self, index: Index, brain_id: str):
        self.index = index
        self.brain_id = brain_id
        self._cache: dict[str, int | None] = {}

    def resolve(self, target: str) -> int | None:
        key = normalize(target.split("#")[0].split("|")[0])
        if key in self._cache:
            return self._cache[key]
        hits = self.index.search(f'"{key}"', limit=8, snippets=False)
        best: int | None = None
        for hit in hits:
            stem = hit.rel_path.rsplit("/", 1)[-1].removesuffix(".md")
            if normalize(stem) == key or normalize(hit.title) == key:
                if hit.brain_id == self.brain_id:
                    best = hit.note_id
                    break
                if best is None:
                    best = hit.note_id
        self._cache[key] = best
        return best


def _inline(text: str, links: LinkResolver | None) -> str:
    """Inline formatting. Escapes first, so nothing in a note can inject HTML."""
    placeholders: list[str] = []

    def stash(fragment: str) -> str:
        placeholders.append(fragment)
        return f"\x00{len(placeholders) - 1}\x00"

    def wikilink(m: re.Match) -> str:
        embed, inner = m.group(1), m.group(2)
        target, _, alias = inner.partition("|")
        label = alias or target.split("#")[-1] if "#" in target and not alias else (alias or target)
        label = html.escape(label.strip())
        clean = target.split("#")[0].strip()
        if embed:
            return stash(f'<span class="embed">⧉ {label}</span>')
        nid = links.resolve(clean) if links else None
        if nid is None:
            return stash(f'<span class="wiki wiki-missing" title="not in the index">{label}</span>')
        return stash(f'<a class="wiki" data-note="{nid}" href="#note-{nid}">{label}</a>')

    def image(m: re.Match) -> str:
        return stash(f'<span class="embed">⧉ {html.escape(m.group(1) or m.group(2))}</span>')

    def link(m: re.Match) -> str:
        label, href = html.escape(m.group(1)), m.group(2)
        if href.startswith(("http://", "https://")):
            safe = html.escape(href, quote=True)
            return stash(f'<a class="ext" href="{safe}" target="_blank" rel="noreferrer">{label}</a>')
        return stash(f'<span class="wiki wiki-missing">{label}</span>')

    def code(m: re.Match) -> str:
        return stash(f"<code>{html.escape(m.group(1))}</code>")

    def url(m: re.Match) -> str:
        safe = html.escape(m.group(0), quote=True)
        return stash(f'<a class="ext" href="{safe}" target="_blank" rel="noreferrer">{safe}</a>')

    text = CODE_RE.sub(code, text)
    text = WIKILINK_RE.sub(wikilink, text)
    text = IMG_RE.sub(image, text)
    text = MD_LINK_RE.sub(link, text)
    text = URL_RE.sub(url, text)
    text = html.escape(text)
    text = BOLD_RE.sub(r"<strong>\1</strong>", text)
    text = ITALIC_RE.sub(r"<em>\1</em>", text)
    text = STRIKE_RE.sub(r"<del>\1</del>", text)
    text = TAG_RE.sub(lambda m: f'<span class="tag">#{m.group(1)}</span>', text)

    return re.sub(r"\x00(\d+)\x00", lambda m: placeholders[int(m.group(1))], text)


def render(body: str, index: Index | None = None, brain_id: str = "") -> str:
    """Markdown to HTML. Wikilinks resolve when an index is supplied."""
    links = LinkResolver(index, brain_id) if index is not None else None
    out: list[str] = []
    lines = body.replace("\r\n", "\n").split("\n")
    i = 0
    list_stack: list[str] = []

    def close_lists(to: int = 0) -> None:
        while len(list_stack) > to:
            out.append(f"</{list_stack.pop()}>")

    while i < len(lines):
        line = lines[i]

        fence = FENCE_RE.match(line.strip())
        if fence:
            close_lists()
            lang = fence.group(2).strip().split()[0] if fence.group(2).strip() else ""
            marker = fence.group(1)
            i += 1
            block: list[str] = []
            while i < len(lines) and not lines[i].strip().startswith(marker):
                block.append(lines[i])
                i += 1
            i += 1
            cls = f' class="lang-{html.escape(lang, quote=True)}"' if lang else ""
            out.append(f"<pre><code{cls}>{html.escape(chr(10).join(block))}</code></pre>")
            continue

        if not line.strip():
            close_lists()
            i += 1
            continue

        heading = HEADING_RE.match(line)
        if heading:
            close_lists()
            level = min(len(heading.group(1)), 6)
            out.append(f"<h{level}>{_inline(heading.group(2), links)}</h{level}>")
            i += 1
            continue

        if re.match(r"^\s*(-{3,}|\*{3,}|_{3,})\s*$", line):
            close_lists()
            out.append("<hr>")
            i += 1
            continue

        if line.lstrip().startswith(">"):
            close_lists()
            quote: list[str] = []
            while i < len(lines) and lines[i].lstrip().startswith(">"):
                quote.append(lines[i].lstrip()[1:].lstrip())
                i += 1
            out.append(f"<blockquote>{_inline(' '.join(quote), links)}</blockquote>")
            continue

        # Tables: a header row followed by a separator row.
        if "|" in line and i + 1 < len(lines) and TABLE_SEP_RE.match(lines[i + 1]):
            close_lists()
            def cells(row: str) -> list[str]:
                return [c.strip() for c in row.strip().strip("|").split("|")]
            head = cells(line)
            i += 2
            rows: list[list[str]] = []
            while i < len(lines) and "|" in lines[i] and lines[i].strip():
                rows.append(cells(lines[i]))
                i += 1
            th = "".join(f"<th>{_inline(c, links)}</th>" for c in head)
            body_html = "".join(
                "<tr>" + "".join(f"<td>{_inline(c, links)}</td>" for c in r) + "</tr>"
                for r in rows
            )
            out.append(f"<table><thead><tr>{th}</tr></thead><tbody>{body_html}</tbody></table>")
            continue

        task = TASK_RE.match(line)
        item = LIST_RE.match(line)
        if task or item:
            m = task or item
            indent = len(m.group(1)) // 2 if item else 0
            ordered = bool(item and not task and item.group(2)[0].isdigit())
            want = indent + 1
            tag = "ol" if ordered else "ul"
            while len(list_stack) > want:
                out.append(f"</{list_stack.pop()}>")
            while len(list_stack) < want:
                out.append(f"<{tag}>")
                list_stack.append(tag)
            if task:
                done = task.group(1).lower() == "x"
                box = "☑" if done else "☐"
                cls = ' class="task done"' if done else ' class="task"'
                out.append(f"<li{cls}><span class=\"box\">{box}</span>"
                           f"{_inline(task.group(2), links)}</li>")
            else:
                out.append(f"<li>{_inline(item.group(3), links)}</li>")
            i += 1
            continue

        close_lists()
        para: list[str] = []
        while i < len(lines) and lines[i].strip() and not HEADING_RE.match(lines[i]) \
                and not LIST_RE.match(lines[i]) and not lines[i].lstrip().startswith(">") \
                and not FENCE_RE.match(lines[i].strip()):
            para.append(lines[i].strip())
            i += 1
        out.append(f"<p>{_inline(' '.join(para), links)}</p>")

    close_lists()
    return "\n".join(out)
