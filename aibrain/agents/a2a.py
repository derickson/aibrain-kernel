"""Agent2Agent (A2A) over HTTP.

A2A is JSON-RPC 2.0 posted to a single endpoint. The server publishes an agent
card describing itself, then accepts `message/send` for a single response or
`message/stream` for an SSE stream of task updates.

Field names moved between drafts of the spec (`kind` vs `type` on parts,
`agent-card.json` vs `agent.json`), so the parsing here accepts both shapes
rather than pinning one.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
import uuid
from typing import Any, Iterator

from ..config import AgentConfig
from ..corpus import Corpus
from .base import Agent, Event

CARD_PATHS = [
    "/.well-known/agent-card.json",
    "/.well-known/agent.json",
    "/agent-card.json",
]
TIMEOUT = 120.0


class A2AError(RuntimeError):
    pass


def same_origin(candidate: str, configured: str) -> bool:
    """Same scheme, host and port as the URL the user configured."""
    a = urllib.parse.urlparse(candidate)
    b = urllib.parse.urlparse(configured)
    if a.scheme not in ("http", "https") or not a.hostname:
        return False
    return (a.scheme, a.hostname, a.port or _default_port(a.scheme)) == (
        b.scheme, b.hostname, b.port or _default_port(b.scheme))


def _default_port(scheme: str) -> int:
    return 443 if scheme == "https" else 80


class A2AClient:
    def __init__(self, url: str, headers: dict[str, str] | None = None):
        self.url = url.rstrip("/")
        self.headers = headers or {}
        self.card: dict | None = None

    def _open(self, url: str, data: bytes | None = None, accept: str = "application/json"):
        request = urllib.request.Request(url, data=data, method="POST" if data else "GET")
        request.add_header("Accept", accept)
        if data:
            request.add_header("Content-Type", "application/json")
        for key, value in self.headers.items():
            request.add_header(key, value)
        return urllib.request.urlopen(request, timeout=TIMEOUT)

    def fetch_card(self) -> dict:
        errors = []
        for path in CARD_PATHS:
            try:
                with self._open(self.url + path) as response:
                    self.card = json.loads(response.read().decode("utf-8"))
                    return self.card
            except (urllib.error.URLError, OSError, json.JSONDecodeError) as exc:
                errors.append(f"{path}: {exc}")
        raise A2AError("no agent card at " + self.url + " (" + "; ".join(errors[:2]) + ")")

    def endpoint(self) -> str:
        """Where to post, from the agent card — but only on the same origin.

        The card is written by the remote agent, and `headers` may carry a
        static API key the user configured for it. A card naming
        `http://someone-else/` would have sent that key to someone else, so a
        card may move the path, not the host.
        """
        if self.card:
            for key in ("url", "endpoint", "serviceEndpoint"):
                value = self.card.get(key)
                if isinstance(value, str) and same_origin(value, self.url):
                    return value.rstrip("/")
                if isinstance(value, str) and value.startswith("http"):
                    raise A2AError(
                        f"the agent card at {self.url} points at a different "
                        f"host; refusing to send credentials there"
                    )
        return self.url

    def _envelope(self, method: str, text: str, context_id: str | None) -> dict:
        message: dict[str, Any] = {
            "role": "user",
            "parts": [{"kind": "text", "type": "text", "text": text}],
            "messageId": uuid.uuid4().hex,
            "kind": "message",
        }
        if context_id:
            message["contextId"] = context_id
        return {
            "jsonrpc": "2.0",
            "id": uuid.uuid4().hex,
            "method": method,
            "params": {"message": message},
        }

    def send(self, text: str, context_id: str | None = None) -> dict:
        body = json.dumps(self._envelope("message/send", text, context_id)).encode()
        try:
            with self._open(self.endpoint(), body) as response:
                payload = json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            raise A2AError(f"HTTP {exc.code} from {self.endpoint()}") from exc
        except (urllib.error.URLError, OSError) as exc:
            raise A2AError(f"cannot reach {self.endpoint()}: {exc}") from exc
        if "error" in payload:
            raise A2AError(str(payload["error"].get("message", payload["error"])))
        return payload.get("result") or {}

    def stream(self, text: str, context_id: str | None = None) -> Iterator[dict]:
        """message/stream as server-sent events; falls back to message/send."""
        body = json.dumps(self._envelope("message/stream", text, context_id)).encode()
        try:
            response = self._open(self.endpoint(), body, accept="text/event-stream")
        except urllib.error.HTTPError as exc:
            if exc.code in (404, 405, 501):
                yield self.send(text, context_id)
                return
            raise A2AError(f"HTTP {exc.code} from {self.endpoint()}") from exc
        except (urllib.error.URLError, OSError) as exc:
            raise A2AError(f"cannot reach {self.endpoint()}: {exc}") from exc

        ctype = response.headers.get("Content-Type", "")
        if "text/event-stream" not in ctype:
            payload = json.loads(response.read().decode("utf-8"))
            yield payload.get("result") or {}
            return

        data_lines: list[str] = []
        for raw in response:
            line = raw.decode("utf-8", errors="replace").rstrip("\n")
            if line.startswith("data:"):
                data_lines.append(line[5:].lstrip())
                continue
            if line == "" and data_lines:
                blob = "\n".join(data_lines)
                data_lines = []
                try:
                    payload = json.loads(blob)
                except json.JSONDecodeError:
                    continue
                if "error" in payload:
                    raise A2AError(str(payload["error"].get("message", payload["error"])))
                yield payload.get("result") or payload


def parts_text(container: Any) -> str:
    """Text out of a Message, Artifact, or bare parts list."""
    if container is None:
        return ""
    if isinstance(container, str):
        return container
    if isinstance(container, list):
        return "".join(parts_text(c) for c in container)
    if not isinstance(container, dict):
        return ""
    if "parts" in container:
        return parts_text(container["parts"])
    kind = container.get("kind") or container.get("type")
    if kind == "text" or "text" in container:
        return container.get("text", "")
    if kind == "data":
        data = container.get("data")
        return json.dumps(data, indent=2) if data is not None else ""
    return ""


def result_text(result: dict) -> tuple[str, str]:
    """Pull the assistant text and a status label out of any A2A result shape."""
    if not isinstance(result, dict):
        return "", ""
    kind = result.get("kind") or result.get("type") or ""

    if kind == "message" or result.get("role") == "agent":
        return parts_text(result), ""
    if kind in ("status-update", "task-status-update"):
        status = result.get("status") or {}
        return parts_text(status.get("message")), status.get("state", "")
    if kind in ("artifact-update", "task-artifact-update"):
        return parts_text(result.get("artifact")), ""
    if kind == "task" or "status" in result or "artifacts" in result:
        status = result.get("status") or {}
        text = parts_text(status.get("message"))
        if not text:
            for artifact in result.get("artifacts") or []:
                text += parts_text(artifact)
        if not text:
            for message in reversed(result.get("history") or []):
                if message.get("role") == "agent":
                    text = parts_text(message)
                    break
        return text, status.get("state", "")
    return parts_text(result), ""


class A2AAgent(Agent):
    kind = "a2a"

    def __init__(self, cfg: AgentConfig, corpus: Corpus, brain_names: dict[str, str],
                 colors: dict[str, str] | None = None):
        super().__init__(cfg, corpus, brain_names, colors)
        self.client = A2AClient(cfg.url, cfg.headers)
        self.context_id: str | None = None

    def ask(self, question: str, history: list[dict]) -> Iterator[Event]:
        if not self.cfg.url:
            yield Event("error", "no URL configured for this A2A connection")
            yield Event("done")
            return

        yield Event("status", "retrieving context from your brains…")
        hits = self.retrieve(question)

        try:
            if self.client.card is None:
                yield Event("status", f"fetching agent card from {self.cfg.url}…")
                card = self.client.fetch_card()
                name = card.get("name", "agent")
                skills = ", ".join(s.get("name", "") for s in (card.get("skills") or [])[:3])
                yield Event("status", f"connected to {name}{f' · {skills}' if skills else ''}")
        except A2AError as exc:
            yield Event("error", str(exc))
            yield Event("done")
            return

        prompt = _build_prompt(question, self.context_block(hits))
        answer: list[str] = []
        seen_state = ""

        try:
            for result in self.client.stream(prompt, self.context_id):
                cid = result.get("contextId") or (result.get("status") or {}).get("contextId")
                if cid:
                    self.context_id = cid
                text, state = result_text(result)
                if state and state != seen_state:
                    seen_state = state
                    if state not in ("completed", "failed"):
                        yield Event("status", f"task {state}")
                if text:
                    # Streamed chunks are usually cumulative in A2A; only send
                    # what is genuinely new.
                    joined = "".join(answer)
                    delta = text[len(joined):] if text.startswith(joined) else text
                    if delta:
                        answer.append(delta)
                        yield Event("delta", delta)
                if state == "failed":
                    yield Event("error", text or "the remote task failed")
        except A2AError as exc:
            yield Event("error", str(exc))
            yield Event("done")
            return

        text = "".join(answer)
        if not text.strip():
            yield Event("delta", "(the agent returned no text)")

        # A remote agent reads nothing of ours, so the only evidence available
        # is what it wrote. Anything it did not name stays in the context list.
        cites = self.cites_from_text(text, hits)
        yield Event("cites", cites=cites,
                    data={"context": [c.to_dict() for c in
                                      self.context_citations(hits, cites)]})
        yield Event("done")

    def probe(self) -> dict:
        if not self.cfg.url:
            return {"ok": False, "detail": "no URL configured"}
        try:
            card = self.client.fetch_card()
        except A2AError as exc:
            return {"ok": False, "detail": str(exc)}
        skills = len(card.get("skills") or [])
        return {
            "ok": True,
            "detail": f"{card.get('name', 'agent')} · {card.get('version', '?')}"
                      f"{f' · {skills} skills' if skills else ''}",
        }


def _build_prompt(question: str, context: str) -> str:
    if not context:
        return question
    return (
        f"{question}\n\n---\nPassages from the user's knowledge base. Ground the "
        f"answer in these and cite a note by writing its title as [[Note title]]:\n\n"
        f"{context}\n"
    )
