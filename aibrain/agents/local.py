"""The built-in agent.

No model, no network: it answers by searching the index and reporting what it
found, with the matched passage under each citation. That makes the app useful
before a single external agent is configured, and it is the thing the ACP and
A2A agents are measured against — if the local agent cannot find it, neither
will they, because they are handed the same retrieval.
"""

from __future__ import annotations

import re
import time
from collections import Counter
from typing import Iterator

from .base import Agent, Event, STOPWORDS, _plain


class LocalAgent(Agent):
    kind = "local"

    def ask(self, question: str, history: list[dict]) -> Iterator[Event]:
        yield Event("status", "searching the index…")
        hits = self.retrieve(question, limit=max(6, self.cfg.context_notes))

        if not hits:
            yield Event("delta", (
                f"Nothing in the index matches “{question.strip()}”.\n\n"
                "Either the notes are not there yet, or the words in them are "
                "different from the words in the question. Try a narrower phrase, "
                "or a term you know appears verbatim in a note."
            ))
            yield Event("done")
            return

        brains = Counter(self.brain_names.get(h.brain_id, h.brain_id) for h in hits)
        sources = Counter(h.source for h in hits)
        total = self.index.count()
        spread = ", ".join(f"{name}" for name, _ in brains.most_common(3))

        lead = (
            f"{len(hits)} notes out of {total:,} match, spanning {spread}. "
            f"Most of them sit under {sources.most_common(1)[0][0]}.\n"
        )
        yield from _stream(lead)

        for i, hit in enumerate(hits, 1):
            note = self.index.note(hit.note_id)
            brain = self.brain_names.get(hit.brain_id, hit.brain_id)
            degree = note.degree if note else 0
            passage = _plain(hit.snippet) or (note.excerpt[:220] if note else "")
            block = (
                f"\n{i}. {hit.title} — {brain} / {hit.source}"
                f"{f', {degree} links' if degree else ''}\n"
                f"   “{passage}”\n"
            )
            yield from _stream(block)

        overlap = self._shared_links(hits)
        if overlap:
            names = ", ".join(n for n in overlap[:3])
            yield from _stream(
                f"\nThese notes converge on {names} — that is where the thread runs.\n"
            )

        yield from _stream("\nOpen a citation to read the note and fly the camera to it.")
        # Here retrieval *is* the answer: every note listed above is quoted from,
        # so 'matched' is an honest claim in a way that padding never was.
        yield Event("cites", cites=[
            self.cite(h, "matched", "this is the search hit quoted above")
            for h in hits])
        yield Event("done")

    def _shared_links(self, hits) -> list[str]:
        """Notes that more than one of the hits links to — the connective tissue."""
        counter: Counter[int] = Counter()
        ids = {h.note_id for h in hits}
        for hit in hits:
            for neighbour in self.index.neighbours(hit.note_id, limit=25):
                if neighbour.id not in ids:
                    counter[neighbour.id] += 1
        shared = [nid for nid, count in counter.most_common(6) if count > 1]
        out = []
        for nid in shared:
            note = self.index.note(nid)
            if note:
                out.append(note.title)
        return out

    def probe(self) -> dict:
        return {"ok": True, "detail": f"{self.index.count():,} notes indexed"}


def _stream(text: str, chunk: int = 4) -> Iterator[Event]:
    """Emit text in small pieces so the browser types it out."""
    for i in range(0, len(text), chunk):
        yield Event("delta", text[i:i + chunk])
        time.sleep(0.004)
