"""The built-in agent.

No model, no network: it answers by searching the index and reporting what it
found, with the matched passage under each citation. That makes the app useful
before a single external agent is configured, and it is the thing the ACP and
A2A agents are measured against — if the local agent cannot find it, neither
will they, because they are handed the same retrieval.
"""

from __future__ import annotations

import re
from collections import Counter
from typing import Iterator

from .base import Agent, Event, STOPWORDS, _plain, normalize


class LocalAgent(Agent):
    kind = "local"

    def ask(self, question: str, history: list[dict]) -> Iterator[Event]:
        yield Event("status", "searching the index…")
        hits = self.retrieve(question, limit=max(6, self.cfg.context_notes))

        if not hits:
            text = (
                f"Nothing in the index matches “{question.strip()}”.\n\n"
                "Either the notes are not there yet, or the words in them are "
                "different from the words in the question. Try a narrower phrase, "
                "or a term you know appears verbatim in a note."
            )
            yield Event("delta", text)
            yield Event("cites", data={"html": self.corpus.render(text)})
            yield Event("done")
            return

        brains = Counter(self.brain_names.get(h.brain_id, h.brain_id) for h in hits)
        sources = Counter(h.source for h in hits)
        total = self._total_notes()
        spread = ", ".join(f"**{name}**" for name, _ in brains.most_common(3))

        # Every link is keyed by note id, not title: duplicate titles across
        # vaults are common, and a title-keyed map would send both links to
        # whichever copy won. `render` resolves `[[note-12|Title]]` through
        # `resolved`, so each link opens exactly the note it quotes.
        resolved: dict[str, int] = {}

        def link(note_id: int, title: str) -> str:
            key = f"note-{note_id}"
            resolved[normalize(key)] = note_id
            label = _LINK_UNSAFE.sub(" ", title).strip() or key
            return f"[[{key}|{label}]]"

        # One `/note/:id` per hit, shared by the passage links and the overlap.
        pages = {h.note_id: self.corpus.note(h.note_id) or {} for h in hits}

        def relink(passage: str, page: dict) -> str:
            """The passage's own `[[wikilinks]]`, pointed at the notes they
            resolve to. The quote is lifted out of its note, so the note's own
            neighbours are what its links meant; one that is not among them
            stays as written and renders as unresolved, same as in the note."""
            targets: dict[str, int] = {}
            for n in page.get("linked", []):
                if n.get("nid") is None:
                    continue
                path = re.sub(r"\.md$", "", n.get("rel_path", ""))
                for key in (n.get("name", ""), path, path.rsplit("/", 1)[-1]):
                    if key:
                        targets.setdefault(normalize(key), n["nid"])

            def swap(match: re.Match) -> str:
                target, _, alias = match.group(2).partition("|")
                nid = targets.get(normalize(target.split("#")[0]))
                if match.group(1) or nid is None:
                    return match.group(0)
                return link(nid, alias or target)
            return _WIKILINK.sub(swap, passage)

        parts = [
            f"**{len(hits)}** notes out of {total:,} match, spanning {spread}. "
            f"Most of them sit under *{sources.most_common(1)[0][0]}*.\n"
        ]
        for i, hit in enumerate(hits, 1):
            brain = self.brain_names.get(hit.brain_id, hit.brain_id)
            passage = _plain(hit.snippet)
            if not passage:
                passage = pages[hit.note_id].get("excerpt", "")[:220]
            passage = relink(" ".join(passage.split()), pages[hit.note_id])
            links = f" · {hit.degree} links" if hit.degree else ""
            parts.append(
                f"{i}. **{link(hit.note_id, hit.title)}** — {brain} / {hit.source}{links}\n"
                + (f"   > {passage}\n" if passage else "")
            )

        overlap = self._shared_links(hits, pages)
        if overlap:
            names = ", ".join(link(nid, name) for nid, name in overlap[:3])
            parts.append(f"These notes converge on {names} — that is where the thread runs.\n")

        parts.append("Open a link or a citation to read the note and fly the camera to it.")
        text = "\n".join(parts)

        # The whole answer at once. It is a search result, not a model writing,
        # so typing it out character by character would be pretending — and it
        # would show raw `[[note-…]]` markup until the rendered HTML arrived.
        yield Event("delta", text)
        # Here retrieval *is* the answer: every note listed above is quoted from,
        # so 'matched' is an honest claim in a way that padding never was.
        yield Event("cites", cites=[
            self.cite(h, "matched", "this is the search hit quoted above")
            for h in hits], data={"html": self.corpus.render(text, resolved)})
        yield Event("done")

    def _shared_links(self, hits, pages: dict[int, dict]) -> list[tuple[int, str]]:
        """Notes that more than one of the hits links to — the connective tissue.

        `/note/:id` already carries the neighbours, so this reuses the one
        request per hit `ask` made rather than a query per edge.
        """
        counter: Counter[int] = Counter()
        titles: dict[int, str] = {}
        ids = {h.note_id for h in hits}
        for hit in hits:
            page = pages.get(hit.note_id) or {}
            for neighbour in page.get("linked", []):
                nid = neighbour.get("nid")
                if nid is None or nid in ids:
                    continue
                counter[nid] += 1
                titles[nid] = neighbour.get("name", "")
        return [(nid, titles[nid]) for nid, count in counter.most_common(6)
                if count > 1 and titles.get(nid)]

    def _total_notes(self) -> int:
        try:
            return int(self.corpus.health().get("notes", 0))
        except Exception:
            return 0

    def probe(self) -> dict:
        return {"ok": True, "detail": f"{self._total_notes():,} notes indexed"}


# Brackets would end the wikilink early; `|` would split the label again.
_LINK_UNSAFE = re.compile(r"[\[\]|]")
# Same shape the Rust renderer matches, embeds included so they are left alone.
_WIKILINK = re.compile(r"(!?)\[\[([^\[\]]+?)\]\]")
