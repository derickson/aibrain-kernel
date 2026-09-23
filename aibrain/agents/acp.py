"""Agent Client Protocol over stdio.

ACP is how editors talk to coding agents: newline-delimited JSON-RPC 2.0 on the
agent's stdin/stdout. We are the *client* here, so besides sending prompts we
have to answer the agent's own requests — reading and writing files, and
approving tool calls.

Handshake: initialize → (authenticate) → session/new → session/prompt, with the
answer arriving as a stream of session/update notifications rather than in the
prompt's result.
"""

from __future__ import annotations

import json
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any, Iterator

from ..config import AgentConfig
from ..corpus import Corpus
from .base import Agent, Event

PROTOCOL_VERSION = 1
# The directory `aibrain/` lives in, so `python3 -m aibrain.mcp_todo` resolves
# from a subprocess the agent starts in someone else's project.
PACKAGE_ROOT = Path(__file__).resolve().parent.parent.parent
START_TIMEOUT = 45.0
PROMPT_TIMEOUT = 600.0

# Shell commands that read a file's contents. Deliberately excludes ls, find,
# stat and friends, which name a path without ever opening it.
READ_COMMANDS = {
    "cat", "head", "tail", "bat", "less", "more", "sed", "awk", "grep", "rg",
    "ag", "nl", "od", "strings", "jq", "wc", "diff", "open", "pbcopy",
}


class ACPError(RuntimeError):
    pass


class ACPConnection:
    """One live subprocess speaking ACP, with a reader thread behind it."""

    def __init__(self, command: list[str], cwd: str, env: dict[str, str],
                 log: Any = None, roots: list[Path] | None = None,
                 core_url: str | None = None):
        self.command = command
        self.cwd = cwd or os.getcwd()
        self.env = env
        # Where our own MCP server should look for the corpus. Held here
        # because `session/new` is what hands it to the agent.
        self.core_url = core_url or ""
        # Directories the agent may read and write. The session directory is
        # always one; the vaults are added because reading the notes behind a
        # brain is the whole point of connecting a coding agent to it.
        self.roots = [Path(self.cwd).resolve()] + [
            Path(r).expanduser().resolve() for r in (roots or [])
        ]
        self.log = log or (lambda _m: None)
        self.proc: subprocess.Popen | None = None
        self.session_id: str | None = None
        self._next_id = 1
        self._pending: dict[int, queue.Queue] = {}
        self._updates: queue.Queue = queue.Queue()
        self._lock = threading.Lock()
        self._alive = False
        self._stderr_tail: list[str] = []
        self.capabilities: dict = {}
        # Files the agent touched this turn, path -> (tier, why). Two tiers,
        # because agents differ in how much they let us see:
        #   "read"   we served the bytes, or the agent told us the location
        #   "opened" a command that reads a file ran against it and completed
        # Claude Code's adapter reads through its own terminal and never calls
        # fs/read_text_file, so without the second tier we would observe
        # nothing at all for the agent people actually use.
        self.opened: dict[str, tuple[str, str]] = {}
        # Command text accumulated per tool call, banked when it completes.
        self._tool_cmds: dict[str, str] = {}

    # ---- lifecycle -------------------------------------------------------
    def start(self) -> None:
        exe = self.command[0]
        if shutil.which(exe) is None and not Path(exe).exists():
            raise ACPError(
                f"{exe!r} is not on PATH. Install the agent's ACP adapter, or point "
                f"the connection at its full path."
            )
        self.proc = subprocess.Popen(
            self.command,
            cwd=self.cwd,
            env={**os.environ, **self.env},
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            start_new_session=True,
        )
        self._alive = True
        threading.Thread(target=self._read_loop, daemon=True, name="acp-read").start()
        threading.Thread(target=self._stderr_loop, daemon=True, name="acp-err").start()

    def close(self) -> None:
        self._alive = False
        proc, self.proc = self.proc, None
        if proc and proc.poll() is None:
            try:
                proc.terminate()
                proc.wait(timeout=3)
            except (subprocess.TimeoutExpired, OSError):
                proc.kill()

    @property
    def running(self) -> bool:
        return bool(self.proc and self.proc.poll() is None)

    # ---- wire ------------------------------------------------------------
    def _send(self, payload: dict) -> None:
        if not self.proc or not self.proc.stdin:
            raise ACPError("agent is not running")
        line = json.dumps(payload, separators=(",", ":")) + "\n"
        try:
            self.proc.stdin.write(line)
            self.proc.stdin.flush()
        except (BrokenPipeError, ValueError) as exc:
            raise ACPError(f"agent closed its input: {exc}") from exc

    def request(self, method: str, params: dict | None = None,
                timeout: float = 60.0) -> Any:
        with self._lock:
            rid = self._next_id
            self._next_id += 1
            inbox: queue.Queue = queue.Queue(maxsize=1)
            self._pending[rid] = inbox
        self._send({"jsonrpc": "2.0", "id": rid, "method": method,
                    "params": params or {}})
        try:
            message = inbox.get(timeout=timeout)
        except queue.Empty:
            raise ACPError(f"{method} timed out after {timeout:.0f}s{self._why()}") from None
        finally:
            self._pending.pop(rid, None)
        if "error" in message:
            err = message["error"]
            raise ACPError(f"{method}: {err.get('message', err)}")
        return message.get("result")

    def notify(self, method: str, params: dict | None = None) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def _respond(self, rid: Any, result: Any = None, error: dict | None = None) -> None:
        msg: dict = {"jsonrpc": "2.0", "id": rid}
        if error is not None:
            msg["error"] = error
        else:
            msg["result"] = result
        try:
            self._send(msg)
        except ACPError:
            pass

    def _why(self) -> str:
        if self._stderr_tail:
            return " — stderr: " + " / ".join(self._stderr_tail[-3:])
        if not self.running:
            return " — the process exited"
        return ""

    def _read_loop(self) -> None:
        proc = self.proc
        if not proc or not proc.stdout:
            return
        for line in proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                self.log(f"non-JSON from agent: {line[:200]}")
                continue
            if "id" in message and "method" in message:
                self._handle_request(message)
            elif "id" in message:
                inbox = self._pending.get(message["id"])
                if inbox:
                    try:
                        inbox.put_nowait(message)
                    except queue.Full:
                        pass
            elif message.get("method") == "session/update":
                self._updates.put(message.get("params", {}))
            else:
                self.log(f"notification {message.get('method')}")
        self._alive = False
        self._updates.put({"__closed__": True})

    def _stderr_loop(self) -> None:
        proc = self.proc
        if not proc or not proc.stderr:
            return
        for line in proc.stderr:
            line = line.rstrip()
            if line:
                self._stderr_tail.append(line)
                del self._stderr_tail[:-20]
                self.log(f"stderr: {line}")

    # ---- agent-initiated requests ---------------------------------------
    def _handle_request(self, message: dict) -> None:
        method, params, rid = message["method"], message.get("params") or {}, message["id"]
        try:
            if method == "fs/read_text_file":
                self._respond(rid, {"content": self._read_file(params)})
            elif method == "fs/write_text_file":
                self._write_file(params)
                self._respond(rid, None)
            elif method == "session/request_permission":
                self._respond(rid, {"outcome": self._permission(params)})
            else:
                self._respond(rid, error={"code": -32601,
                                          "message": f"{method} not supported"})
        except Exception as exc:
            self._respond(rid, error={"code": -32000, "message": str(exc)})

    def _confine(self, raw: str) -> Path:
        """Keep the agent inside the session directory or one of the vaults."""
        path = Path(raw).expanduser().resolve()
        for root in self.roots:
            if path == root or root in path.parents:
                return path
        allowed = ", ".join(str(r) for r in self.roots)
        raise ACPError(f"refusing access to {path}; allowed roots are {allowed}")

    def note_location(self, raw: str) -> None:
        """Record a path an ACP tool call reported touching."""
        self._record(raw, "read", "the agent reported this as a tool location")

    def _record(self, raw: str, tier: str, why: str) -> None:
        """Bank one path, keeping the strongest tier seen for it."""
        try:
            path = str(self._confine(raw))
        except (ACPError, OSError, ValueError):
            return
        prior = self.opened.get(path)
        if prior is None or (prior[0] == "opened" and tier == "read"):
            self.opened[path] = (tier, why)

    def tool_command(self, call_id: str, text: str) -> None:
        """Remember what a tool call is running, to judge when it finishes."""
        if call_id and text:
            self._tool_cmds[call_id] = text

    def tool_finished(self, call_id: str) -> None:
        """A tool call completed — bank any note it demonstrably read.

        Only commands that actually read a file's contents count. `ls` and
        `find` name paths without reading them, and treating those as evidence
        is exactly the kind of guess that made citations untrustworthy.
        """
        command = self._tool_cmds.pop(call_id, "")
        if not command:
            return
        words = set(re.findall(r"[A-Za-z0-9_.-]+", command))
        if not (words & READ_COMMANDS):
            return
        for path in re.findall(r'["\']([^"\']+\.md)["\']|(\S+\.md)', command):
            candidate = path[0] or path[1]
            if candidate:
                self._record(candidate, "opened",
                             f"a command that reads this file ran: {_one_line(command, 80)}")

    def _read_file(self, params: dict) -> str:
        path = self._confine(params["path"])
        text = path.read_text(encoding="utf-8", errors="replace")
        # `opened` became a dict of path -> (tier, why) when the second tier
        # was added; this line was still calling `.add`, so every fs/read_text
        # request came back to the agent as an error instead of a file.
        self._record(str(path), "read", "we served this file to the agent")
        line = params.get("line")
        limit = params.get("limit")
        if line is None and limit is None:
            return text
        lines = text.splitlines(keepends=True)
        try:
            start = max(0, int(line or 1) - 1)
            end = start + max(0, int(limit)) if limit is not None else len(lines)
        except (TypeError, ValueError):
            raise ACPError("line and limit must be numbers")
        return "".join(lines[start:end])

    def _write_file(self, params: dict) -> None:
        path = self._confine(params["path"])
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(params.get("content", ""), encoding="utf-8")
        self.log(f"agent wrote {path}")

    def _within_roots(self, raw: str) -> bool:
        try:
            self._confine(raw)
            return True
        except (ACPError, OSError, ValueError):
            return False

    def _permission(self, params: dict) -> dict:
        """Auto-approve the least surprising option — but never outside our roots.

        The UI has no modal yet, so a blocking prompt would hang the stream.
        We take the allow-once option when the agent offers one, and otherwise
        refuse, which agents handle as a declined tool call.

        Claude Code edits files by running its own tools directly rather than
        asking us to write them through `fs/write_text_file` (that path is
        already confined by `_confine`) — it only tells us what a tool call
        touched, via `locations`, when it asks permission to run it. So this
        is the one place we can refuse a write outside the session directory
        or a vault before it happens rather than merely notice it afterwards.
        A tool call with no declared locations — most shell commands — cannot
        be checked this way; this narrows the gap, it does not close it the
        way an OS-level sandbox would.
        """
        options = params.get("options") or []
        tool_call = params.get("toolCall") or {}
        tool = tool_call.get("title", "a tool call")

        outside = [
            loc["path"] for loc in (tool_call.get("locations") or [])
            if isinstance(loc, dict) and loc.get("path")
            and not self._within_roots(loc["path"])
        ]
        if outside:
            self.log(f"refused {tool}: touches {', '.join(outside)}, "
                     f"outside the session directory and its vaults")
            return {"outcome": "cancelled"}

        for want in ("allow_once", "allow_always"):
            for opt in options:
                if opt.get("kind") == want:
                    self.log(f"approved {tool} ({opt.get('name')})")
                    return {"outcome": "selected", "optionId": opt["optionId"]}
        self.log(f"no allow option offered for {tool}; cancelling")
        return {"outcome": "cancelled"}

    # ---- protocol steps --------------------------------------------------
    def initialize(self) -> dict:
        result = self.request("initialize", {
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": True, "writeTextFile": True},
                "terminal": False,
            },
        }, timeout=START_TIMEOUT) or {}
        self.capabilities = result.get("agentCapabilities", {}) or {}
        methods = result.get("authMethods") or []
        if methods:
            # Try the first method; agents that are already logged in answer ok.
            try:
                self.request("authenticate", {"methodId": methods[0].get("id")},
                             timeout=START_TIMEOUT)
            except ACPError as exc:
                self.log(f"authenticate skipped: {exc}")
        return result

    def mcp_servers(self) -> list[dict]:
        """The MCP servers the agent gets for this session.

        One, for now: the day's list. It travels through the same `session/new`
        field `additionalDirectories` does, so the adapter is already known to
        accept it. The child is launched by the agent, not by us, so it
        inherits none of our environment — PYTHONPATH is spelled out because
        the agent's cwd is the user's project, not this repo.
        """
        env = [{"name": "PYTHONPATH", "value": str(PACKAGE_ROOT)}]
        if self.core_url:
            env.append({"name": "AIBRAIN_CORE_URL", "value": self.core_url})
        return [{
            "type": "stdio",
            "name": "aibrain-todo",
            "command": sys.executable or "python3",
            "args": ["-m", "aibrain.mcp_todo"],
            "env": env,
        }]

    def new_session(self, cwd: str) -> str:
        # Tell the agent which directories are legitimately part of this
        # workspace. The vaults live outside the session cwd, so without this
        # the agent has to guess where the notes are — which is exactly what it
        # did, by listing the whole of ~/Documents/ObsidianVaults and finding
        # vaults that are not linked. Naming the roots is not a sandbox, but it
        # removes the reason to go looking.
        roots = [str(r) for r in self.roots if str(r) != cwd]
        params = {
            "cwd": cwd,
            "mcpServers": self.mcp_servers(),
            "additionalDirectories": roots,
        }
        try:
            result = self.request("session/new", params, timeout=START_TIMEOUT) or {}
        except ACPError as exc:
            # Adapters validate this field strictly and they do not all spell
            # a stdio server the same way. A chat that cannot start is a much
            # worse outcome than a chat without the to-do tools, so drop them
            # and say so rather than failing.
            self.log(f"session/new refused our MCP servers ({exc}); retrying without them")
            params["mcpServers"] = []
            result = self.request("session/new", params, timeout=START_TIMEOUT) or {}
        session = result.get("sessionId")
        if not session:
            raise ACPError("agent did not return a sessionId")
        self.session_id = session
        return session

    def prompt(self, text: str) -> Iterator[dict]:
        """Send a prompt; yield session/update payloads until the turn ends."""
        if not self.session_id:
            raise ACPError("no session")
        while not self._updates.empty():          # drop anything stale
            self._updates.get_nowait()

        done: queue.Queue = queue.Queue(maxsize=1)

        def run() -> None:
            try:
                result = self.request("session/prompt", {
                    "sessionId": self.session_id,
                    "prompt": [{"type": "text", "text": text}],
                }, timeout=PROMPT_TIMEOUT)
                done.put(("ok", result))
            except Exception as exc:
                done.put(("error", exc))

        threading.Thread(target=run, daemon=True, name="acp-prompt").start()

        deadline = time.time() + PROMPT_TIMEOUT
        while True:
            try:
                status, payload = done.get_nowait()
                # Drain whatever arrived in the same instant as the result.
                while True:
                    try:
                        yield self._updates.get_nowait()
                    except queue.Empty:
                        break
                if status == "error":
                    raise payload
                yield {"__stop__": (payload or {}).get("stopReason", "end_turn")}
                return
            except queue.Empty:
                pass
            try:
                update = self._updates.get(timeout=0.25)
            except queue.Empty:
                if time.time() > deadline:
                    raise ACPError("prompt timed out")
                continue
            if update.get("__closed__"):
                raise ACPError(f"agent exited mid-turn{self._why()}")
            yield update

    def cancel(self) -> None:
        if self.session_id:
            try:
                self.notify("session/cancel", {"sessionId": self.session_id})
            except ACPError:
                pass


class ACPAgent(Agent):
    """An ACP connection, kept warm between questions."""

    kind = "acp"

    def __init__(self, cfg: AgentConfig, corpus: Corpus, brain_names: dict[str, str],
                 colors: dict[str, str] | None = None):
        super().__init__(cfg, corpus, brain_names, colors)
        self.conn: ACPConnection | None = None
        self.vault_roots: list[Path] = []
        self.brain_roots: dict[str, str] = {}
        self._log: list[str] = []
        self._start_lock = threading.Lock()

    def _note(self, message: str) -> None:
        self._log.append(message)
        del self._log[:-100]

    def _ensure(self) -> ACPConnection:
        with self._start_lock:
            if self.conn and self.conn.running and self.conn.session_id:
                return self.conn
            if self.conn:
                self.conn.close()
            if not self.cfg.command:
                raise ACPError("no command configured for this ACP connection")
            cwd = self.cfg.cwd or str(Path.cwd())
            conn = ACPConnection(self.cfg.command, cwd, self.cfg.env, self._note,
                                 roots=self.vault_roots,
                                 core_url=self.corpus.base_url)
            conn.start()
            conn.initialize()
            conn.new_session(cwd)
            self.conn = conn
            return conn

    def ask(self, question: str, history: list[dict]) -> Iterator[Event]:
        yield Event("status", "retrieving context from your brains…")
        hits = self.retrieve(question)

        try:
            yield Event("status", f"connecting over ACP ({self.cfg.command[0]})…")
            conn = self._ensure()
        except (ACPError, OSError) as exc:
            yield Event("error", str(exc))
            yield Event("done")
            return

        prompt = _build_prompt(question, self.context_block(hits), self.brain_names)
        answer: list[str] = []
        conn.opened.clear()   # evidence is per-turn, not per-session

        try:
            for update in conn.prompt(prompt):
                if "__stop__" in update:
                    break
                kind = (update.get("update") or {}).get("sessionUpdate")
                payload = update.get("update") or {}
                if kind == "agent_message_chunk":
                    text = _content_text(payload.get("content"))
                    if text:
                        answer.append(text)
                        yield Event("delta", text)
                elif kind == "agent_thought_chunk":
                    text = _content_text(payload.get("content"))
                    if text:
                        yield Event("thought", text)
                elif kind in ("tool_call", "tool_call_update"):
                    for loc in (payload.get("locations") or []):
                        raw_path = loc.get("path") if isinstance(loc, dict) else None
                        if raw_path:
                            conn.note_location(raw_path)
                    call_id = payload.get("toolCallId") or payload.get("id") or ""
                    # The command arrives on one update and its completion on a
                    # later one, keyed by the same id.
                    if payload.get("title") and payload.get("kind") == "execute":
                        conn.tool_command(call_id, payload["title"])
                    if payload.get("status") == "completed":
                        conn.tool_finished(call_id)
                    # One line per tool call, updated in place. Without the id
                    # the UI would print a fresh line for every status change
                    # and bury the answer under a wall of shell commands.
                    title = payload.get("title") or payload.get("kind") or "tool"
                    yield Event(
                        "tool",
                        _one_line(title, 110),
                        data={"id": call_id, "status": payload.get("status", "")},
                    )
                elif kind == "plan":
                    entries = payload.get("entries") or []
                    if entries:
                        yield Event(
                            "tool",
                            _one_line("plan: " + "; ".join(
                                e.get("content", "") for e in entries[:4]), 140),
                            data={"id": "plan", "status": ""},
                        )
        except ACPError as exc:
            yield Event("error", str(exc))
            if self.conn:
                self.conn.close()
                self.conn = None
            yield Event("done")
            return

        text = "".join(answer)
        if not text.strip():
            yield Event("delta", "(the agent finished without sending any text)")

        cites = self.cites_from_text(text, hits, self._read_note_ids(conn.opened))

        yield Event("cites", cites=cites,
                    data={"context": [c.to_dict() for c in
                                      self.context_citations(hits, cites)],
                          "html": self.render_html(text, cites)})
        yield Event("done")

    def _read_note_ids(self, opened: dict[str, tuple[str, str]]) -> dict[int, tuple[str, str]]:
        """Turn paths the agent touched into note ids with their evidence.

        A path only resolves if it is a note we have indexed; anything else the
        agent read — source files, its own scratch — is correctly ignored.
        """
        out: dict[int, tuple[str, str]] = {}
        for brain_id, root in self.brain_roots.items():
            root_path = Path(root).resolve()
            for path, evidence in opened.items():
                try:
                    rel = Path(path).resolve().relative_to(root_path)
                except ValueError:
                    continue
                note = self.corpus.note_by_path(brain_id, rel.as_posix())
                if note is not None:
                    nid = note["nid"]
                    prior = out.get(nid)
                    if prior is None or (prior[0] == "opened" and evidence[0] == "read"):
                        out[nid] = evidence
        return out

    def probe(self) -> dict:
        exe = self.cfg.command[0] if self.cfg.command else ""
        if not exe:
            return {"ok": False, "detail": "no command configured"}
        found = shutil.which(exe) or (exe if Path(exe).exists() else None)
        if not found:
            return {"ok": False, "detail": f"{exe} not found on PATH"}
        if self.conn and self.conn.running:
            return {"ok": True, "detail": f"session {self.conn.session_id or '—'} live"}
        return {"ok": True, "detail": f"{found} ready (connects on first message)"}

    def close(self) -> None:
        if self.conn:
            self.conn.close()
            self.conn = None


def _one_line(text: str, limit: int) -> str:
    """Collapse a tool title to something that fits on one line in the trace."""
    flat = " ".join(str(text).split())
    return flat if len(flat) <= limit else flat[: limit - 1] + "…"


def _content_text(content: Any) -> str:
    """ACP content blocks are either a dict or a list of them."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, dict):
        if content.get("type") in (None, "text"):
            return content.get("text", "")
        return ""
    if isinstance(content, list):
        return "".join(_content_text(c) for c in content)
    return ""


def _build_prompt(question: str, context: str, brain_names: dict[str, str]) -> str:
    brains = ", ".join(brain_names.values()) or "the vault"
    if not context:
        return (
            f"{question}\n\n"
            f"(Context: this question is about the user's Obsidian knowledge bases "
            f"({brains}). Nothing in the local index matched, so answer from the "
            f"files themselves if you can reach them.)"
        )
    return (
        f"{question}\n\n"
        f"---\n"
        f"Passages retrieved from the user's Obsidian brains ({brains}). "
        f"Ground your answer in these, and cite a note by writing its title as "
        f"[[Note title]] so the UI can link it:\n\n"
        f"{context}\n"
    )
