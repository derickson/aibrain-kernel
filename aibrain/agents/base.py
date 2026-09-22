"""What every agent connection looks like from the server's point of view.

An agent takes a question and yields events. The transport underneath may be a
local search, a subprocess speaking ACP, or an HTTP service speaking A2A; the
UI only ever sees this event stream.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Iterator

from ..config import AgentConfig
from ..index import Index
from ..vault import normalize


# How much we actually know about an agent's use of a note, strongest first.
# The distinction is the whole point: a citation we cannot back up should not
# look like one we can.
#
#   read      we served the file's bytes, or the agent told us it opened it
#   opened    a command that reads a file ran against it and completed
#   grounded  the agent cited a note whose passage we handed it, so it had the
#             real text in front of it even though it never opened the file
#   named     the agent wrote [[Title]] for a note we neither supplied nor saw
#             it open — the weakest claim, and the one worth doubting
#   matched   this is the search hit whose passage is quoted in the answer
#             (the local agent, where retrieval *is* the answer)
#   context   we handed it the passage; it never referred to it
EVIDENCE_ORDER = {"read": 0, "opened": 1, "grounded": 2, "matched": 3,
                  "named": 4, "context": 5}


@dataclass
class Citation:
    note_id: int
    title: str
    brain: str
    source: str
    color: str = "#7fd8e8"
    snippet: str = ""
    evidence: str = "named"
    # Free text explaining the evidence, shown on hover.
    why: str = ""
    # Set when a title matched several notes and we could not tell which the
    # agent meant — usually the same note duplicated across two vaults.
    ambiguous_with: int = 0

    def to_dict(self) -> dict:
        out = {
            "nid": self.note_id,
            "name": self.title,
            "brain": self.brain,
            "source": self.source,
            "color": self.color,
            "snippet": self.snippet,
            "evidence": self.evidence,
            "why": self.why,
        }
        if self.ambiguous_with:
            out["ambiguousWith"] = self.ambiguous_with
        return out


@dataclass
class Event:
    """One thing that happened while the agent was answering."""

    type: str                      # status | delta | thought | tool | cites | error | done
    text: str = ""
    cites: list[Citation] = field(default_factory=list)
    data: dict = field(default_factory=dict)

    def to_dict(self) -> dict:
        out: dict = {"type": self.type}
        if self.text:
            out["text"] = self.text
        if self.cites:
            out["cites"] = [c.to_dict() for c in self.cites]
        if self.data:
            out.update(self.data)
        return out


class Agent:
    """Base class. Subclasses implement `ask`."""

    def __init__(self, cfg: AgentConfig, index: Index, brain_names: dict[str, str],
                 colors: dict[str, str] | None = None):
        self.cfg = cfg
        self.index = index
        self.brain_names = brain_names
        self.colors = colors or {}

    # ---- shared retrieval ------------------------------------------------
    def retrieve(self, question: str, limit: int | None = None) -> list:
        """Top hits for a question, scoped to whatever brains this agent sees."""
        limit = limit or self.cfg.context_notes
        scope = self.cfg.brains or None

        # A question is not a search box query. Requiring every word to appear
        # means one absent verb — "describe", "summarise" — returns nothing at
        # all, which quietly left remote agents with no context whatsoever.
        # Try the strict reading first, then widen.
        hits = self.index.search(question, limit=limit * 2, brain_ids=scope)
        if len(hits) < limit:
            words = [w for w in re.findall(r"[A-Za-z][\w'-]{3,}", question)
                     if w.lower() not in STOPWORDS]
            if words:
                hits += self.index.search(" ".join(words[:8]), limit=limit * 2,
                                          brain_ids=scope, match="any")
        seen: set[int] = set()
        unique = []
        for hit in hits:
            if hit.note_id in seen:
                continue
            seen.add(hit.note_id)
            unique.append(hit)
        return unique[:limit]

    def cite(self, hit, evidence: str = "named", why: str = "") -> Citation:
        return Citation(
            note_id=hit.note_id,
            title=hit.title,
            brain=self.brain_names.get(hit.brain_id, hit.brain_id),
            source=hit.source,
            color=self.colors.get(hit.brain_id, self.cfg.color),
            snippet=_plain(hit.snippet),
            evidence=evidence,
            why=why,
        )

    def cite_note(self, note_id: int, evidence: str, why: str = "") -> Citation | None:
        """A citation for a note we already know the id of."""
        note = self.index.note(note_id)
        if note is None:
            return None
        return Citation(
            note_id=note.id,
            title=note.title,
            brain=self.brain_names.get(note.brain_id, note.brain_id),
            source=note.source,
            color=self.colors.get(note.brain_id, self.cfg.color),
            snippet=note.excerpt[:200],
            evidence=evidence,
            why=why,
        )

    def context_block(self, hits) -> str:
        """The retrieved passages, formatted for a remote agent's prompt."""
        if not hits:
            return ""
        chunks = []
        for i, hit in enumerate(hits, 1):
            note = self.index.note(hit.note_id)
            body = _plain(hit.snippet) or (note.excerpt if note else "")
            brain = self.brain_names.get(hit.brain_id, hit.brain_id)
            chunks.append(
                f"[{i}] {hit.title}  ({brain} / {hit.source})\n"
                f"    path: {hit.rel_path}\n"
                f"    {body[: self.cfg.context_chars]}"
            )
        return "\n\n".join(chunks)

    def cites_from_text(self, text: str, supplied: list,
                        touched: dict[int, tuple[str, str]] | None = None
                        ) -> list[Citation]:
        """Only what the agent actually referred to — never the retrieved set.

        An earlier version padded every answer out to six pills using our own
        search results, which made a two-note answer look like a six-note one
        and made every citation worth exactly as much as the weakest. Now a
        pill exists only if the agent wrote `[[Title]]`, or if we watched it
        open the file.

        `supplied` is what we handed the agent, used to disambiguate a title
        that matches several notes — not as a source of citations. `touched`
        maps note id to (tier, why) for files we observed it open.
        """
        touched = touched or {}
        read_ids = set(touched)
        supplied_ids = {h.note_id for h in supplied}
        found: dict[int, Citation] = {}

        for raw in re.findall(r"\[\[([^\]]+)\]\]", text):
            target = normalize(raw.split("|")[0].split("#")[0])
            if not target:
                continue
            matches = [
                hit for hit in self.index.search(
                    f'"{target}"', limit=8, brain_ids=self.cfg.brains or None)
                if normalize(hit.title) == target
            ]
            if not matches:
                continue
            chosen = self._disambiguate(matches, supplied_ids, read_ids)
            if chosen.note_id in found:
                continue
            if chosen.note_id in touched:
                tier, why = touched[chosen.note_id]
            elif chosen.note_id in supplied_ids:
                tier = "grounded"
                why = "we supplied this note's passage and the agent cited it"
            else:
                tier = "named"
                why = ("the agent named this note; we neither supplied it nor "
                       "saw it open the file")
            citation = self.cite(chosen, tier, why)
            # Duplicated notes across vaults are common; say so rather than
            # picking one and pretending it was obvious.
            citation.ambiguous_with = max(0, len(matches) - 1)
            found[chosen.note_id] = citation

        # A file we watched it open is evidence even when it never named it.
        for note_id, (tier, why) in touched.items():
            if note_id in found:
                continue
            citation = self.cite_note(note_id, tier, why)
            if citation is not None:
                found[note_id] = citation

        return sorted(found.values(),
                      key=lambda c: (EVIDENCE_ORDER.get(c.evidence, 9), c.title))

    def _disambiguate(self, matches: list, supplied_ids: set[int],
                      read_ids: set[int]):
        """Pick which note a title meant, preferring evidence over order."""
        for hit in matches:
            if hit.note_id in read_ids:
                return hit
        for hit in matches:
            if hit.note_id in supplied_ids:
                return hit
        return matches[0]

    def context_citations(self, supplied: list, cited: list[Citation]) -> list[Citation]:
        """Passages we gave the agent that it never referred to."""
        used = {c.note_id for c in cited}
        return [self.cite(hit, "context",
                          "we supplied this passage; the agent never referred to it")
                for hit in supplied if hit.note_id not in used]

    # ---- interface -------------------------------------------------------
    def ask(self, question: str, history: list[dict]) -> Iterator[Event]:
        raise NotImplementedError

    def probe(self) -> dict:
        """Cheap reachability check for the connections panel."""
        return {"ok": True, "detail": "ready"}

    def close(self) -> None:
        pass


STOPWORDS = {
    "what", "which", "about", "every", "note", "notes", "show", "find", "from",
    "that", "with", "connect", "summarize", "search", "have", "does", "this",
    "your", "mine", "tell", "give", "when", "where", "there", "their", "should",
    "would", "could", "know", "into", "over", "them", "they", "been", "were",
    "anything", "everything", "something", "please", "across", "between",
}


def _plain(text: str) -> str:
    return re.sub(r"</?mark>", "", text or "").replace("\n", " ").strip()
