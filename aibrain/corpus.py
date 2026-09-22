"""The client for aibrain-core, the Rust service that owns the corpus.

Python used to own an SQLite index and fourteen fine-grained queries over it.
It now owns none of that: Rust scans the vaults, resolves the links, computes
the layout and renders the markdown, and this module is the only place that
knows how to ask it. Every route here answers a whole question, because the
old habit of one query per wikilink does not survive a socket.

Standard library only, like the rest of the package — `urllib` and `json`.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any

DEFAULT_URL = "http://127.0.0.1:8781"

# Health has to answer now or the startup check is pointless; a reindex of a
# large vault legitimately takes minutes.
HEALTH_TIMEOUT = 2.0
READ_TIMEOUT = 30.0
REINDEX_TIMEOUT = 1800.0


class CorpusError(RuntimeError):
    """Anything that stopped us getting an answer, phrased for a human."""


@dataclass
class Hit:
    """One search result, with the field names the agents already use."""

    note_id: int
    title: str
    brain_id: str
    source: str
    rel_path: str
    snippet: str = ""
    score: float = 0.0
    degree: int = 0
    mtime: float = 0.0
    # Which engine answered. Rust adds this; older builds do not.
    engine: str = ""

    @classmethod
    def from_json(cls, row: dict) -> "Hit":
        return cls(
            note_id=int(row.get("nid", 0)),
            title=row.get("name", ""),
            brain_id=row.get("brain_id", ""),
            source=row.get("source", ""),
            rel_path=row.get("rel_path", ""),
            snippet=row.get("snippet", "") or "",
            score=float(row.get("score") or 0.0),
            degree=int(row.get("degree") or 0),
            mtime=float(row.get("mtime") or 0.0),
            engine=row.get("engine", "") or "",
        )


class Corpus:
    """Everything Python needs from the corpus, over HTTP.

    One instance is shared by every request thread. `urllib` opens a fresh
    connection per call, so there is no shared state to guard.
    """

    def __init__(self, base_url: str | None = None):
        raw = base_url or os.environ.get("AIBRAIN_CORE_URL") or DEFAULT_URL
        self.base_url = raw.rstrip("/")

    # ---- plumbing --------------------------------------------------------
    def _request(self, path: str, *, params: dict | None = None,
                 body: dict | None = None, timeout: float = READ_TIMEOUT,
                 allow_404: bool = False, method: str | None = None) -> Any:
        url = self.base_url + path
        if params:
            clean = {k: v for k, v in params.items() if v not in (None, "")}
            if clean:
                url += "?" + urllib.parse.urlencode(clean)
        data = json.dumps(body).encode("utf-8") if body is not None else None
        request = urllib.request.Request(
            url, data=data, method=method or ("POST" if data else "GET"))
        if data:
            request.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return json.loads(response.read().decode("utf-8") or "null")
        except urllib.error.HTTPError as exc:
            if exc.code == 404 and allow_404:
                return None
            raise CorpusError(self._explain(path, exc)) from exc
        except (urllib.error.URLError, OSError, TimeoutError) as exc:
            raise CorpusError(
                f"could not reach aibrain-core at {self.base_url} ({exc}) — "
                f"is it running?"
            ) from exc
        except json.JSONDecodeError as exc:
            raise CorpusError(f"{path} did not return JSON: {exc}") from exc

    def _explain(self, path: str, exc: urllib.error.HTTPError) -> str:
        """Prefer the service's own error text over an HTTP status code."""
        detail = ""
        try:
            payload = json.loads(exc.read().decode("utf-8"))
            detail = payload.get("error", "")
        except Exception:
            pass
        return f"{path} failed ({exc.code}){': ' + detail if detail else ''}"

    # ---- endpoints -------------------------------------------------------
    def health(self, timeout: float = HEALTH_TIMEOUT) -> dict:
        return self._request("/health", timeout=timeout) or {}

    def reachable(self) -> bool:
        try:
            self.health()
            return True
        except CorpusError:
            return False

    def status(self) -> dict:
        return self._request("/status") or {}

    def universe(self) -> dict:
        # A big corpus makes this the slowest read, so it gets its own budget.
        return self._request("/universe", timeout=120.0) or {}

    def search_raw(self, query: str, limit: int = 60,
                   brain_ids: list[str] | None = None,
                   any_terms: bool = False) -> list[dict]:
        """Results exactly as the service sent them.

        The browser route wants every field, including ones added after this
        client was written; the agents want the `Hit` shape they already use.
        """
        if not query.strip():
            return []
        payload = self._request("/search", params={
            "q": query,
            "limit": limit,
            "brains": ",".join(brain_ids) if brain_ids else None,
            "any": "1" if any_terms else None,
        }) or {}
        return payload.get("results", [])

    def search(self, query: str, limit: int = 60,
               brain_ids: list[str] | None = None,
               any_terms: bool = False) -> list[Hit]:
        return [Hit.from_json(row)
                for row in self.search_raw(query, limit, brain_ids, any_terms)]

    def note(self, note_id: int) -> dict | None:
        return self._request(f"/note/{int(note_id)}", allow_404=True)

    def note_by_path(self, brain_id: str, rel_path: str) -> dict | None:
        return self._request("/notes/by-path", allow_404=True,
                             params={"brain": brain_id, "path": rel_path})

    def recent(self, limit: int = 24) -> list[dict]:
        payload = self._request("/notes/recent", params={"limit": limit}) or {}
        return payload.get("results", [])

    def reindex(self, force: bool = False) -> dict:
        return self._request("/reindex", body={"force": bool(force)},
                             timeout=REINDEX_TIMEOUT) or {}

    # ---- the day's list --------------------------------------------------
    # Thin on purpose. Which day an item belongs to, when a rollover happens
    # and what gets logged are all decisions the service owns; Python only
    # carries the question there and the answer back.
    #
    # `now` is honoured by the service only when it was started with
    # AIBRAIN_TODO_TEST_CLOCK=1. Passing it otherwise is harmless.

    def todos(self, day: str | None = None, now: str | None = None) -> dict:
        return self._request("/todos", params={"day": day, "now": now}) or {}

    def add_todo(self, body: str, scheduled_on: str | None = None,
                 refs: list[dict] | None = None,
                 now: str | None = None) -> dict:
        payload: dict[str, Any] = {"body": body, "refs": refs or []}
        if scheduled_on:
            payload["scheduled_on"] = scheduled_on
        return self._request("/todos", body=payload, params={"now": now}) or {}

    def complete_todo(self, todo_id: int, now: str | None = None) -> dict:
        return self._todo_post(todo_id, "complete", now=now)

    def uncomplete_todo(self, todo_id: int, now: str | None = None) -> dict:
        return self._todo_post(todo_id, "uncomplete", now=now)

    def cancel_todo(self, todo_id: int, now: str | None = None) -> dict:
        return self._todo_post(todo_id, "cancel", now=now)

    def reschedule_todo(self, todo_id: int, to_day: str,
                        now: str | None = None) -> dict:
        return self._todo_post(todo_id, "reschedule", {"to_day": to_day}, now)

    def link_todo(self, todo_id: int, brain_id: str, rel_path: str,
                  now: str | None = None) -> dict:
        return self._todo_post(todo_id, "link",
                               {"brain_id": brain_id, "rel_path": rel_path}, now)

    def update_todo(self, todo_id: int, body: str | None = None,
                    sort_order: float | None = None,
                    now: str | None = None) -> dict:
        payload: dict[str, Any] = {}
        if body is not None:
            payload["body"] = body
        if sort_order is not None:
            payload["sort_order"] = sort_order
        return self._request(f"/todos/{int(todo_id)}", body=payload,
                             params={"now": now}, method="PATCH",
                             allow_404=True) or {}

    def todo_history(self, query: str = "", limit: int = 50) -> dict:
        return self._request("/todos/history",
                             params={"q": query, "limit": limit}) or {}

    def _todo_post(self, todo_id: int, action: str, body: dict | None = None,
                   now: str | None = None) -> dict:
        # An empty body still has to be a POST, so it is `{}` not None.
        return self._request(f"/todos/{int(todo_id)}/{action}", body=body or {},
                             params={"now": now}, allow_404=True) or {}


# The message a person can act on, rather than a traceback. Named here because
# both `__main__` and the tests assert on it.
def unreachable_message(base_url: str) -> str:
    return (
        f"aibrain-core is not answering at {base_url}.\n"
        "\n"
        "  Postgres and the Rust service are both required. Start them with:\n"
        "\n"
        "    docker compose up -d db\n"
        "    cargo run --manifest-path rust/Cargo.toml -- serve --watch\n"
        "\n"
        "  Or run ./dev.sh, which starts all three."
    )
