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
# The service pings every 15 s, so anything longer than that without a byte
# means the connection is gone rather than merely quiet.
EVENT_TIMEOUT = 45.0


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

    # ---- live updates ----------------------------------------------------
    # The service keeps a revision per brain and a packed layout keyed by it,
    # so an unchanged corpus is a 304 rather than four seconds of geometry.
    # Both of these stay raw: the ETag has to survive the trip to the browser
    # unchanged, and the event stream has to arrive event by event.

    def universe_with_etag(self, if_none_match: str | None = None
                           ) -> tuple[dict | None, str]:
        """`(payload, etag)`. The payload is None when the service said 304."""
        request = urllib.request.Request(self.base_url + "/universe")
        if if_none_match:
            request.add_header("If-None-Match", if_none_match)
        try:
            with urllib.request.urlopen(request, timeout=120.0) as response:
                etag = response.headers.get("ETag", "")
                return json.loads(response.read().decode("utf-8") or "null"), etag
        except urllib.error.HTTPError as exc:
            if exc.code == 304:
                return None, exc.headers.get("ETag", "") or (if_none_match or "")
            raise CorpusError(self._explain("/universe", exc)) from exc
        except (urllib.error.URLError, OSError, TimeoutError) as exc:
            raise CorpusError(
                f"could not reach aibrain-core at {self.base_url} ({exc}) — "
                f"is it running?"
            ) from exc

    def stream_events(self, timeout: float = EVENT_TIMEOUT):
        """The service's change feed, one decoded event at a time.

        A `data:` frame is yielded as the dict it carries; the service's
        keep-alive comment becomes `{"type": "ping"}` so the proxy downstream
        has something to write and the browser's connection stays warm too.
        Ends quietly when either side hangs up.
        """
        request = urllib.request.Request(self.base_url + "/events")
        request.add_header("Accept", "text/event-stream")
        try:
            response = urllib.request.urlopen(request, timeout=timeout)
        except (urllib.error.HTTPError, urllib.error.URLError, OSError) as exc:
            raise CorpusError(
                f"could not open the event stream at {self.base_url} ({exc})"
            ) from exc
        try:
            for raw in response:
                line = raw.decode("utf-8", "replace").rstrip("\r\n")
                if line.startswith(":"):
                    yield {"type": "ping"}
                elif line.startswith("data:"):
                    try:
                        yield json.loads(line[5:].strip())
                    except json.JSONDecodeError:
                        continue
        except (TimeoutError, OSError):
            # A dead socket ends the stream; the browser reconnects.
            return
        finally:
            response.close()

    def render(self, text: str, resolved: dict[str, int] | None = None) -> str:
        """Markdown to sanitized HTML, through the same pipeline notes use.

        `resolved` maps a normalized title (`agents.base.normalize`) to the
        note id a `[[Title]]` naming it should link to — callers pass in
        whatever they already resolved (a citation set) rather than letting
        this re-resolve titles on its own, so an inline link always points at
        the same note its citation pill does.
        """
        if not text.strip():
            return ""
        result = self._request("/render", body={
            "text": text, "resolved": resolved or {},
        }) or {}
        return result.get("html", "")

    def search_page(self, query: str, limit: int = 60,
                    brain_ids: list[str] | None = None,
                    any_terms: bool = False) -> dict:
        """The whole `/search` answer: `results` plus `engine` and `count`.

        The browser route wants every field, including ones added after this
        client was written; the agents want the `Hit` shape they already use.
        """
        if not query.strip():
            return {"results": [], "count": 0, "engine": ""}
        return self._request("/search", params={
            "q": query,
            "limit": limit,
            "brains": ",".join(brain_ids) if brain_ids else None,
            "any": "1" if any_terms else None,
        }) or {"results": []}

    def search_raw(self, query: str, limit: int = 60,
                   brain_ids: list[str] | None = None,
                   any_terms: bool = False) -> list[dict]:
        """Result rows exactly as the service sent them."""
        return self.search_page(query, limit, brain_ids, any_terms).get("results", [])

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

    def recent(self, limit: int = 24, brain_ids: list[str] | None = None) -> list[dict]:
        params: dict = {"limit": limit}
        if brain_ids:
            params["brains"] = ",".join(brain_ids)
        payload = self._request("/notes/recent", params=params) or {}
        return payload.get("results", [])

    def retire_brain(self, brain_id: str) -> dict:
        """Forget a brain whose link is gone, and schedule its index for
        deletion. Waits out a rescan of that brain if one is running."""
        return self._request(f"/brains/{urllib.parse.quote(brain_id, safe='')}/retire",
                             body={}, timeout=REINDEX_TIMEOUT) or {}

    def reindex(self, force: bool = False) -> dict:
        return self._request("/reindex", body={"force": bool(force)},
                             timeout=REINDEX_TIMEOUT) or {}

    # ---- the to-do list -------------------------------------------------
    # Thin on purpose. What "today" and "tomorrow" mean for a due date, how
    # long a finished item lingers and what gets logged are all decisions the
    # service owns; Python only carries the question there and the answer back.
    #
    # `now` is honoured by the service only when it was started with
    # AIBRAIN_TODO_TEST_CLOCK=1. Passing it otherwise is harmless.

    def todos(self, now: str | None = None) -> dict:
        return self._request("/todos", params={"now": now}) or {}

    def add_todo(self, body: str, due_on: str | None = None,
                 refs: list[dict] | None = None,
                 now: str | None = None) -> dict:
        payload: dict[str, Any] = {"body": body, "refs": refs or []}
        if due_on:
            payload["due_on"] = due_on
        return self._request("/todos", body=payload, params={"now": now}) or {}

    def complete_todo(self, todo_id: int, now: str | None = None) -> dict:
        return self._todo_post(todo_id, "complete", now=now)

    def uncomplete_todo(self, todo_id: int, now: str | None = None) -> dict:
        return self._todo_post(todo_id, "uncomplete", now=now)

    def cancel_todo(self, todo_id: int, now: str | None = None) -> dict:
        return self._todo_post(todo_id, "cancel", now=now)

    def set_due_todo(self, todo_id: int, due_on: str | None,
                     now: str | None = None) -> dict:
        """`YYYY-MM-DD`, `today`, `tomorrow`, or None/"" to clear the date."""
        return self._todo_post(todo_id, "due", {"due_on": due_on or None}, now)

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

    def file_todo(self, todo_id: int, folder_id: int | None,
                  now: str | None = None) -> dict:
        return self._todo_post(todo_id, "file", {"folder_id": folder_id}, now)

    # ---- to-do folders -----------------------------------------------------
    # Groups below the list; a filed item shows under its folder instead.

    def create_folder(self, name: str) -> dict:
        return self._request("/todos/folders", body={"name": name}) or {}

    def update_folder(self, folder_id: int, name: str | None = None,
                      collapsed: bool | None = None,
                      sort_order: float | None = None) -> bool:
        payload: dict[str, Any] = {}
        if name is not None:
            payload["name"] = name
        if collapsed is not None:
            payload["collapsed"] = collapsed
        if sort_order is not None:
            payload["sort_order"] = sort_order
        return bool(self._request(f"/todos/folders/{int(folder_id)}", body=payload,
                                  method="PATCH", allow_404=True))

    def delete_folder(self, folder_id: int) -> bool:
        return bool(self._request(f"/todos/folders/{int(folder_id)}/delete",
                                  body={}, allow_404=True))

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
