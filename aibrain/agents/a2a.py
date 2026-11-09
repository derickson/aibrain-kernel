"""Agent2Agent (A2A) over HTTP.

A2A is JSON-RPC 2.0 posted to a single endpoint. The server publishes an agent
card describing itself, then accepts `message/send` for a single response or
`message/stream` for an SSE stream of task updates.

Field names moved between drafts of the spec (`kind` vs `type` on parts,
`agent-card.json` vs `agent.json`), so the parsing here accepts both shapes
rather than pinning one.
"""

from __future__ import annotations

import http.client
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


# How a server says "I don't do that" rather than "that went wrong": the
# JSON-RPC codes for unsupported operation / method not found, and the HTTP
# statuses that mean the same thing one layer down. Only these trigger the
# fall back from message/stream to message/send — anything else is a real
# failure, and re-sending after one could run the remote task twice.
UNSUPPORTED_CODES = {-32004, -32601}
UNSUPPORTED_STATUSES = {404, 405, 415, 501}
FAILED_STATES = {"failed", "rejected", "canceled"}


class _Unsupported(A2AError):
    """The server refused message/stream outright; message/send may still work."""


def error_message(error: Any) -> str:
    if isinstance(error, dict):
        return str(error.get("message") or error)
    return str(error)


def is_unsupported(error: Any) -> bool:
    return isinstance(error, dict) and error.get("code") in UNSUPPORTED_CODES


class A2AClient:
    def __init__(self, url: str, headers: dict[str, str] | None = None):
        self.url = url.rstrip("/")
        self.headers = headers or {}
        self.card: dict | None = None
        # How the most recent turn was carried ("stream" or "send") and, when
        # it was not streamed, why. The agent reads these to tell the user.
        self.mode: str | None = None
        self.fallback_reason = ""
        # Set once the server has refused message/stream, so later turns go
        # straight to message/send instead of failing the same way first.
        self._no_stream = ""

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
                    card = json.loads(response.read().decode("utf-8"))
            except (urllib.error.URLError, OSError, ValueError) as exc:
                errors.append(f"{candidate}: {exc}")
                continue
            if not isinstance(card, dict):
                errors.append(f"{candidate}: not an agent card")
                continue
            self.card = card
            self._no_stream = ""  # a fresh card may describe an upgraded server
            return card
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

    def streaming_declared(self) -> bool | None:
        """What the card says about streaming: True, False, or None if silent."""
        capabilities = self.card.get("capabilities") if self.card else None
        value = capabilities.get("streaming") if isinstance(capabilities, dict) else None
        return value if isinstance(value, bool) else None

    def plan(self) -> tuple[str, str]:
        """("stream" | "send", why not streaming). A card that says nothing
        about streaming is tried with it and falls back if the server refuses."""
        if self._no_stream:
            return "send", self._no_stream
        if self.streaming_declared() is False:
            return "send", "the agent card says it does not support streaming"
        return "stream", ""

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

    def _http_error(self, exc: urllib.error.HTTPError) -> A2AError:
        hint = " — check the API key in this agent's headers" if exc.code in (401, 403) else ""
        return A2AError(f"HTTP {exc.code} from {self.endpoint()}{hint}")

    def _read_json(self, response) -> dict:
        try:
            payload = json.loads(response.read().decode("utf-8"))
        except ValueError as exc:  # bad JSON and bad UTF-8 alike
            raise A2AError(f"{self.endpoint()} did not answer with JSON") from exc
        if not isinstance(payload, dict):
            raise A2AError(f"{self.endpoint()} answered with an unexpected shape")
        return payload

    def send(self, text: str, context_id: str | None = None) -> dict:
        body = json.dumps(self._envelope("message/send", text, context_id)).encode()
        try:
            with self._open(self.endpoint(), body) as response:
                payload = self._read_json(response)
        except urllib.error.HTTPError as exc:
            raise self._http_error(exc) from exc
        except (urllib.error.URLError, OSError, ValueError) as exc:
            raise A2AError(f"cannot reach {self.endpoint()}: {exc}") from exc
        if "error" in payload:
            raise A2AError(error_message(payload["error"]))
        result = payload.get("result")
        if not isinstance(result, dict) or not result:
            raise A2AError("the agent's reply carried no result")
        return result

    def _send_instead(self, text: str, context_id: str | None, reason: str) -> dict:
        self.mode, self.fallback_reason = "send", reason
        return self.send(text, context_id)

    def stream(self, text: str, context_id: str | None = None) -> Iterator[dict]:
        """One turn's results: message/stream where the server has it,
        otherwise one message/send. `mode` and `fallback_reason` say which."""
        mode, reason = self.plan()
        if mode == "send":
            yield self._send_instead(text, context_id, reason)
            return
        try:
            yield from self._stream_events(text, context_id)
        except _Unsupported as exc:
            # Refused before anything was delivered, so nothing has run yet
            # and a plain send is safe. Remember it for the next turn.
            self._no_stream = str(exc)
            yield self._send_instead(text, context_id, self._no_stream)

    def _stream_events(self, text: str, context_id: str | None) -> Iterator[dict]:
        body = json.dumps(self._envelope("message/stream", text, context_id)).encode()
        try:
            response = self._open(self.endpoint(), body, accept="text/event-stream")
        except urllib.error.HTTPError as exc:
            if exc.code in UNSUPPORTED_STATUSES:
                raise _Unsupported(f"the server answered HTTP {exc.code} to message/stream") from exc
            raise self._http_error(exc) from exc
        except (urllib.error.URLError, OSError, ValueError) as exc:
            raise A2AError(f"cannot reach {self.endpoint()}: {exc}") from exc

        with response:
            if "text/event-stream" not in response.headers.get("Content-Type", ""):
                # Not a stream: either a JSON-RPC error (some servers send
                # "streaming unsupported" as a 200) or a one-shot reply.
                payload = self._read_json(response)
                if "error" in payload:
                    if is_unsupported(payload["error"]):
                        raise _Unsupported(error_message(payload["error"]))
                    raise A2AError(error_message(payload["error"]))
                result = payload.get("result")
                if not isinstance(result, dict) or not result:
                    raise A2AError("the agent's reply carried no result")
                self.mode = "send"
                self.fallback_reason = "the server replied once instead of streaming"
                yield result
                return

            self.mode, self.fallback_reason = "stream", ""
            delivered = False
            data_lines: list[str] = []

            def flush() -> dict | None:
                nonlocal data_lines
                blob, data_lines = "\n".join(data_lines), []
                try:
                    payload = json.loads(blob)
                except ValueError:
                    return None
                if not isinstance(payload, dict):
                    return None
                if "error" in payload:
                    raise A2AError(error_message(payload["error"]))
                result = payload.get("result") or payload
                return result if isinstance(result, dict) else None

            try:
                for raw in response:
                    line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
                    if line.startswith("data:"):
                        data_lines.append(line[5:].lstrip())
                    elif line == "" and data_lines:
                        result = flush()
                        if result is not None:
                            delivered = True
                            yield result
                if data_lines:  # a last event the server never terminated
                    result = flush()
                    if result is not None:
                        delivered = True
                        yield result
            except (OSError, http.client.HTTPException) as exc:
                raise A2AError(f"the stream from {self.endpoint()} was interrupted: {exc}") from exc

            if not delivered:
                raise A2AError("the stream ended without a reply")


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
        text = container.get("text", "")
        return text if isinstance(text, str) else ""
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
        status = _as_dict(result.get("status"))
        return parts_text(status.get("message")), _state(status)
    if kind in ("artifact-update", "task-artifact-update"):
        return parts_text(result.get("artifact")), ""
    if kind == "task" or "status" in result or "artifacts" in result:
        status = _as_dict(result.get("status"))
        text = parts_text(status.get("message"))
        if not text:
            for artifact in _as_list(result.get("artifacts")):
                text += parts_text(artifact)
        if not text:
            for message in reversed(_as_list(result.get("history"))):
                if isinstance(message, dict) and message.get("role") == "agent":
                    text = parts_text(message)
                    break
        return text, _state(status)
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
                skills = ", ".join(str(s.get("name", "")) for s in _as_list(card.get("skills"))[:3]
                                   if isinstance(s, dict))
                yield Event("status", f"connected to {name}{f' · {skills}' if skills else ''}")
        except A2AError as exc:
            yield Event("error", str(exc))
            yield Event("done")
            return

        # Say how the reply will arrive before the wait starts, so a missing
        # stream reads as a property of this agent rather than a hang. A
        # server that only reveals it lacks streaming when refused is
        # announced from the loop below instead.
        announced = False
        mode, reason = self.client.plan()
        if mode == "send":
            announced = True
            yield _no_stream_notice(reason)

        prompt = _build_prompt(question, self.context_block(hits))
        answer: list[str] = []
        seen_state = ""
        failed = False
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
                if not isinstance(result, dict):
                    continue
                if not announced and self.client.mode == "send":
                    announced = True
                    yield _no_stream_notice(self.client.fallback_reason)
                result_status = _as_dict(result.get("status"))
                cid = result.get("contextId") or result_status.get("contextId")
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
                    status = result_status
                    state = _state(status)
                    if state in FAILED_STATES:
                        failed = True
                        yield Event("error", parts_text(status.get("message"))
                                    or f"the remote task {state}")
                        continue
                    if state and state != seen_state:
                        seen_state = state
                        if state != "completed":
                            yield Event("status", f"task {state}")
                    for evt in progress_events(status.get("message")):
                        yield evt
                    continue

                if kind in ("artifact-update", "task-artifact-update"):
                    aid = _as_dict(result.get("artifact")).get("artifactId")
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
                text, state = result_text(result)
                if state in FAILED_STATES:
                    failed = True
                    # A non-streamed reply is a whole Task, so a failure
                    # arrives here rather than as a status-update.
                    yield Event("error", text or f"the remote task {state}")
                    continue
                if state and state != seen_state:
                    seen_state = state
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
        except (AttributeError, TypeError, KeyError, ValueError) as exc:
            # A reply shaped unlike anything above. Report it rather than let
            # the exception cut the chat stream off mid-answer.
            yield Event("error", f"unexpected reply from the agent ({type(exc).__name__}: {exc})")
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
        if not text.strip() and not failed:
            unfinished = seen_state and seen_state not in ("completed", *FAILED_STATES)
            yield Event("delta", f"(the agent returned no text — its task is still {seen_state})"
                        if unfinished else "(the agent returned no text)")

        # A remote agent reads nothing of ours, so the only evidence available
        # is what it wrote. Anything it did not name stays in the context list.
        cites = self.cites_from_text(text, hits)
        yield Event("cites", cites=cites,
                    data={"context": [c.to_dict() for c in
                                      self.context_citations(hits, cites)],
                          "html": self.render_html(text, cites),
                          "usage": usage,
                          **({"transport": self.client.mode} if self.client.mode else {})})
        yield Event("done")

    def probe(self) -> dict:
        if not self.cfg.url:
            return {"ok": False, "detail": "no URL configured"}
        try:
            card = self.client.fetch_card()
        except A2AError as exc:
            return {"ok": False, "detail": str(exc)}
        skills = len(_as_list(card.get("skills")))
        streaming = self.client.streaming_declared()
        return {
            "ok": True,
            "detail": f"{card.get('name', 'agent')} · {card.get('version', '?')}"
                      f"{f' · {skills} skills' if skills else ''}"
                      f" · {'streaming' if streaming else 'no streaming (replies arrive at once)' if streaming is False else 'streaming not declared'}",
        }


def _state(status: dict) -> str:
    state = status.get("state", "")
    return state if isinstance(state, str) else ""


def _as_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def _as_list(value: Any) -> list:
    return value if isinstance(value, list) else []


def _no_stream_notice(reason: str) -> Event:
    return Event("status", f"streaming unavailable ({reason}) — the reply will arrive all at once",
                 data={"transport": "send"})


def _build_prompt(question: str, context: str) -> str:
    if not context:
        return question
    return (
        f"{question}\n\n---\nPassages from the user's knowledge base. Ground the "
        f"answer in these and cite a note by writing its title as [[Note title]]:\n\n"
        f"{context}\n"
    )
