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
        # Most A2A servers publish the card at a well-known path under the
        # origin. Elastic Agent Builder does not: its card lives at
        # /api/agent_builder/a2a/<agentId>.json, a per-agent URL the user
        # pastes as-is into "Base URL". Try that literal URL first, then fall
        # back to the well-known conventions relative to it.
        candidates = [self.url] + [self.url + path for path in CARD_PATHS]
        errors = []
        for candidate in candidates:
            try:
                with self._open(candidate) as response:
                    self.card = json.loads(response.read().decode("utf-8"))
                    return self.card
            except (urllib.error.URLError, OSError, json.JSONDecodeError) as exc:
                errors.append(f"{candidate}: {exc}")
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


def progress_events(message: Any) -> list[Event]:
    """A status-update's structured step data, as thought/tool events.

    Captured traffic from Elastic Agent Builder shows its status-updates
    re-announcing, as plain text, exactly what the artifact stream already
    delivered live — so plain text here is a duplicate and is not turned
    into an event. A `data` part shaped like one of Agent Builder's own step
    types (reasoning, tool_call, tool_result) is not duplicated anywhere
    else, so that part of the split still applies.
    """
    if not isinstance(message, dict):
        return []
    parts = message.get("parts")
    if not isinstance(parts, list):
        return []
    events: list[Event] = []
    for part in parts:
        if not isinstance(part, dict):
            continue
        if (part.get("kind") or part.get("type")) == "data" and isinstance(part.get("data"), dict):
            events.extend(_step_event(part["data"]))
    return events


def _step_event(data: dict) -> list[Event]:
    """One Agent Builder step (reasoning / tool_call / tool_result) as an Event."""
    step = data.get("type") or data.get("step") or ""
    if step == "reasoning":
        text = str(data.get("reasoning") or data.get("text") or "").strip()
        return [Event("thought", text)] if text else []
    if step in ("tool_call", "tool_result"):
        tool = data.get("tool_id") or data.get("toolId") or data.get("tool_name") or "tool"
        call_id = str(data.get("tool_call_id") or data.get("toolCallId") or tool)
        if step == "tool_call":
            params = data.get("params") or data.get("arguments") or {}
            args = ", ".join(f"{k}={v!r}" for k, v in params.items()) if isinstance(params, dict) else ""
            return [Event("tool", f"{tool}({args})" if args else f"{tool}(…)",
                          data={"id": call_id, "status": "running"})]
        return [Event("tool", str(tool), data={"id": call_id, "status": "completed"})]
    return []


def extract_usage(result: dict) -> dict | None:
    """Token counts, if the server reports them.

    Field names vary by implementation and are not yet pinned down for
    Elastic Agent Builder specifically, so this checks the shapes seen
    across agent frameworks rather than one exact one; the summary line
    just omits token counts when none of them match.
    """
    if not isinstance(result, dict):
        return None
    for holder in (result, result.get("status") or {}, result.get("metadata") or {}):
        if not isinstance(holder, dict):
            continue
        usage = holder.get("usage") or holder.get("tokenUsage")
        if isinstance(usage, dict) and usage:
            return usage
    return None


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

        hits = []
        if self.cfg.ground_with_context:
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
        usage: dict | None = None

        # Elastic Agent Builder streams *everything* — every tool-use round's
        # narration, and the eventual real synthesis — as `artifact-update`
        # chunks, all shaped identically. The only way to tell them apart is
        # `artifactId` plus the spec's own `append` flag: a fresh id, or
        # `append: false`, starts a new block; nothing in the wire format
        # says which block is the final one, since even the closing
        # `lastChunk: true` pings for every block arrive bunched up at the
        # very end, long after each block's own text finished. So every
        # block streams live as a "thought" as it's built; whichever one is
        # still open when the stream ends was never superseded by a later
        # block, which makes it the answer by elimination — at that point
        # its thought step is retracted and its text becomes the answer.
        artifact_id: str | None = None
        artifact_key = 0
        artifact_text = ""

        try:
            for result in self.client.stream(prompt, self.context_id):
                cid = result.get("contextId") or (result.get("status") or {}).get("contextId")
                if cid:
                    self.context_id = cid
                usage = usage or extract_usage(result)

                kind = result.get("kind") or result.get("type") or ""
                if kind in ("status-update", "task-status-update"):
                    # Progress narration lives in the artifact stream below;
                    # Agent Builder's status-updates just re-announce the
                    # same text again once a block finishes, so their text is
                    # not re-emitted here — only the state transition, and
                    # any step data no other channel carries.
                    status = result.get("status") or {}
                    state = status.get("state", "")
                    if state == "failed":
                        yield Event("error", parts_text(status.get("message"))
                                    or "the remote task failed")
                        continue
                    if state and state != seen_state:
                        seen_state = state
                        if state != "completed":
                            yield Event("status", f"task {state}")
                    for evt in progress_events(status.get("message")):
                        yield evt
                    continue

                if kind in ("artifact-update", "task-artifact-update"):
                    aid = (result.get("artifact") or {}).get("artifactId")
                    text = parts_text(result.get("artifact"))
                    if not text:
                        continue
                    if aid != artifact_id or result.get("append") is False:
                        artifact_id = aid
                        artifact_key += 1
                        artifact_text = ""
                    artifact_text += text
                    yield Event("thought", text, data={"key": f"a{artifact_key}"})
                    continue

                # A plain message (or the non-streaming send() fallback) is
                # answer content on its own, independent of the artifact
                # bookkeeping above — most A2A agents that don't use
                # artifacts at all send their whole reply this way.
                text, _ = result_text(result)
                if text:
                    # Streamed chunks are usually cumulative in A2A; only send
                    # what is genuinely new. A server that sends true deltas
                    # instead (never resending the cumulative text) falls
                    # through to appending the chunk as-is, which is still
                    # correct — it just means every chunk is "new".
                    joined = "".join(answer)
                    delta = text[len(joined):] if text.startswith(joined) else text
                    if delta:
                        answer.append(delta)
                        yield Event("delta", delta)
        except A2AError as exc:
            yield Event("error", str(exc))
            yield Event("done")
            return

        if artifact_text:
            # The last artifact block was still open at the end, so nothing
            # ever superseded it — promote it: drop its thought step, and use
            # what it was holding as the answer instead.
            yield Event("retract", "", data={"key": f"a{artifact_key}"})
            answer.append(artifact_text)
            yield Event("delta", artifact_text)

        text = "".join(answer)
        if not text.strip():
            yield Event("delta", "(the agent returned no text)")

        # A remote agent reads nothing of ours, so the only evidence available
        # is what it wrote. Anything it did not name stays in the context list.
        cites = self.cites_from_text(text, hits)
        yield Event("cites", cites=cites,
                    data={"context": [c.to_dict() for c in
                                      self.context_citations(hits, cites)],
                          "html": self.render_html(text, cites),
                          "usage": usage})
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
