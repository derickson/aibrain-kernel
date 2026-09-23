"""An MCP server over the to-do list, so a coding agent can read and edit it.

Run it as `python3 -m aibrain.mcp_todo`. It speaks JSON-RPC 2.0 over stdio,
one message per line, which is what the MCP stdio transport is: no framing
headers, no length prefixes, just newline-delimited JSON.

It reaches the list the same way the browser does — over HTTP through
`corpus.py`, never straight to Postgres. One owner of the schema was the point
of moving the corpus to Rust, and a to-do written behind the service's back
would be a to-do with no `todo_event` behind it.

Standard library only. `AIBRAIN_CORE_URL` says where the service is; the ACP
adapter passes it through when it launches this.
"""

from __future__ import annotations

import json
import os
import sys
import traceback
from typing import Any, Callable, Iterator

from .corpus import Corpus, CorpusError

PROTOCOL_VERSION = "2025-06-18"
SERVER_NAME = "aibrain-todo"
SERVER_VERSION = "1.0.0"

# JSON-RPC's own codes, plus the one MCP adds for a tool that does not exist.
PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603


# ---------------------------------------------------------------------------
# the tools
# ---------------------------------------------------------------------------

def _string(name: str, description: str) -> dict:
    return {"type": "string", "description": description}


TOOLS: list[dict] = [
    {
        "name": "list_todos",
        "description": (
            "The to-do list: every open item, in order, with its due date if it "
            "has one, plus anything finished or cancelled today. There is one "
            "list, not one per day."
        ),
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "add_todo",
        "description": "Add an item to the end of the list, optionally with a due date.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "body": _string("body", "What needs doing."),
                "due": _string("due", "YYYY-MM-DD, 'today' or 'tomorrow'. Omit for no due date."),
            },
            "required": ["body"],
        },
    },
    {
        "name": "complete_todo",
        "description": (
            "Mark an item done. It stays on the list, struck through, for the "
            "rest of the day, then only the history has it."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "The to-do id."}},
            "required": ["id"],
        },
    },
    {
        "name": "set_due_date",
        "description": "Give an item a due date, change it, or clear it.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "The to-do id."},
                "due": _string("due", "YYYY-MM-DD, 'today' or 'tomorrow'. Empty clears it."),
            },
            "required": ["id"],
        },
    },
    {
        "name": "link_todo_to_note",
        "description": (
            "Attach a note in one of the brains to a to-do. The note is held "
            "by path as well as by id, so a rename can be repaired."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "The to-do id."},
                "brain_id": _string("brain_id", "Which brain the note is in."),
                "rel_path": _string("rel_path", "Path within the vault, e.g. Recipes/Ramen.md."),
            },
            "required": ["id", "brain_id", "rel_path"],
        },
    },
    {
        "name": "search_history",
        "description": (
            "Search every to-do ever written, done or not, with the history "
            "of every change to it, due-date moves included."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "q": _string("q", "Words to look for. Empty lists the most recent."),
                "limit": {"type": "integer", "description": "At most this many (default 20)."},
            },
        },
    },
]


class TodoTools:
    """Each tool is one call into the service, formatted for a reader."""

    def __init__(self, corpus: Corpus):
        self.corpus = corpus

    def call(self, name: str, args: dict) -> str:
        handler: Callable[[dict], str] | None = getattr(self, f"_{name}", None)
        if handler is None:
            raise KeyError(name)
        return handler(args)

    # ---- the six ---------------------------------------------------------
    def _list_todos(self, args: dict) -> str:
        payload = self.corpus.todos()
        today = payload.get("today", "")
        rows = payload.get("todos", [])
        if not rows:
            return "Nothing on the list."
        lines = [f"{len(rows)} item(s) (today is {today}):"]
        lines += [self._line(row, today) for row in rows]
        return "\n".join(lines)

    def _add_todo(self, args: dict) -> str:
        body = str(args.get("body", "")).strip()[:4000]
        if not body:
            return "Nothing to add: body was empty."
        row = self.corpus.add_todo(body, due_on=args.get("due") or None)
        return "Added " + self._line(row.get("todo", {}))

    def _complete_todo(self, args: dict) -> str:
        row = self.corpus.complete_todo(int(args["id"]))
        if not row:
            return f"There is no to-do {args['id']}."
        return "Done: " + self._line(row.get("todo", {}))

    def _set_due_date(self, args: dict) -> str:
        row = self.corpus.set_due_todo(int(args["id"]), str(args.get("due") or "") or None)
        if not row:
            return f"There is no to-do {args['id']}."
        return "Updated: " + self._line(row.get("todo", {}))

    def _link_todo_to_note(self, args: dict) -> str:
        row = self.corpus.link_todo(int(args["id"]), str(args["brain_id"]),
                                    str(args["rel_path"]))
        if not row:
            return f"There is no to-do {args['id']}."
        return "Linked: " + self._line(row.get("todo", {}))

    def _search_history(self, args: dict) -> str:
        # A model will happily ask for a million rows. Clamp rather than fail:
        # the tool result should be the list, not a lecture about the limit.
        limit = max(1, min(200, int(args.get("limit") or 20)))
        payload = self.corpus.todo_history(str(args.get("q") or "")[:2000],
                                           limit=limit)
        rows = payload.get("todos", [])
        if not rows:
            return "Nothing in the history matches that."
        lines = [f"{len(rows)} match(es):"]
        for row in rows:
            lines.append(self._line(row))
            for event in row.get("events", []):
                span = ""
                if event.get("from_day") or event.get("to_day"):
                    span = f" {event.get('from_day') or '—'} → {event.get('to_day') or '—'}"
                lines.append(f"    · {event.get('kind', '?')}{span}")
        return "\n".join(lines)

    # ---- formatting ------------------------------------------------------
    @staticmethod
    def _line(row: dict, today: str = "") -> str:
        if not row:
            return "(nothing)"
        state = row.get("state", "")
        mark = {"completed": "[x]", "cancelled": "[-]"}.get(state, "[ ]")
        bits = [f"#{row.get('id')}", mark, str(row.get("body", ""))]
        due = row.get("due_on")
        if due:
            # ISO dates compare as strings, so no parsing needed to flag it.
            late = state == "open" and today and due < today
            bits.append(f"(due {due}{', OVERDUE' if late else ''})")
        for ref in row.get("refs", []):
            bits.append(f"→ {ref.get('brain_id')}/{ref.get('rel_path')}")
        return " ".join(bits)


# ---------------------------------------------------------------------------
# the protocol
# ---------------------------------------------------------------------------

class Server:
    """One MCP session, reading requests from a stream and writing replies.

    Kept free of stdin/stdout so a test can drive it over a pipe, or in
    process, without a subprocess.
    """

    def __init__(self, corpus: Corpus | None = None):
        self.tools = TodoTools(corpus or Corpus())
        self.initialized = False

    def handle(self, message: dict) -> dict | None:
        """One request in, one response out. `None` for a notification."""
        if message.get("jsonrpc") != "2.0":
            return self._error(message.get("id"), INVALID_REQUEST,
                               "not a JSON-RPC 2.0 message")
        method = message.get("method")
        mid = message.get("id")
        # A notification has no id and is never answered, however it went.
        if mid is None:
            if method == "notifications/initialized":
                self.initialized = True
            return None

        try:
            if method == "initialize":
                return self._ok(mid, {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                })
            if method == "ping":
                return self._ok(mid, {})
            if method == "tools/list":
                return self._ok(mid, {"tools": TOOLS})
            if method == "tools/call":
                return self._call(mid, message.get("params") or {})
            return self._error(mid, METHOD_NOT_FOUND, f"unknown method {method!r}")
        except Exception as exc:                        # never take the pipe down
            traceback.print_exc(file=sys.stderr)
            return self._error(mid, INTERNAL_ERROR, f"{type(exc).__name__}: {exc}")

    def _call(self, mid: Any, params: dict) -> dict:
        name = params.get("name", "")
        args = params.get("arguments") or {}
        if not isinstance(args, dict):
            return self._error(mid, INVALID_PARAMS, "arguments must be an object")
        if name not in {tool["name"] for tool in TOOLS}:
            return self._error(mid, INVALID_PARAMS, f"no such tool {name!r}")
        try:
            text = self.tools.call(name, args)
        except (KeyError, ValueError, TypeError) as exc:
            # A missing or unusable argument is the model's mistake to fix, so
            # it comes back as a tool result rather than a protocol error.
            return self._tool_error(mid, f"bad arguments: {exc}")
        except CorpusError as exc:
            # A tool failing is a result the model should see and react to,
            # not a protocol error that kills the turn.
            return self._tool_error(mid, str(exc))
        return self._ok(mid, {"content": [{"type": "text", "text": text}],
                              "isError": False})

    def _tool_error(self, mid: Any, text: str) -> dict:
        return self._ok(mid, {"content": [{"type": "text", "text": text}],
                              "isError": True})

    @staticmethod
    def _ok(mid: Any, result: dict) -> dict:
        return {"jsonrpc": "2.0", "id": mid, "result": result}

    @staticmethod
    def _error(mid: Any, code: int, message: str) -> dict:
        return {"jsonrpc": "2.0", "id": mid, "error": {"code": code, "message": message}}

    def run(self, source: Iterator[str], sink) -> None:
        for line in source:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError as exc:
                self._write(sink, self._error(None, PARSE_ERROR, str(exc)))
                continue
            if isinstance(message, list):
                # Batches are legal JSON-RPC; MCP does not use them, but
                # answering one is cheaper than explaining why we did not.
                for item in message:
                    reply = self.handle(item) if isinstance(item, dict) else None
                    if reply is not None:
                        self._write(sink, reply)
                continue
            if not isinstance(message, dict):
                self._write(sink, self._error(None, INVALID_REQUEST, "expected an object"))
                continue
            reply = self.handle(message)
            if reply is not None:
                self._write(sink, reply)

    @staticmethod
    def _write(sink, payload: dict) -> None:
        sink.write(json.dumps(payload) + "\n")
        sink.flush()


def main(argv: list[str] | None = None) -> int:
    argv = argv if argv is not None else sys.argv[1:]
    if argv and argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    corpus = Corpus(os.environ.get("AIBRAIN_CORE_URL") or None)
    # Anything printed to stdout that is not a JSON-RPC message corrupts the
    # stream, so every diagnostic goes to stderr.
    print(f"{SERVER_NAME} talking to {corpus.base_url}", file=sys.stderr)
    Server(corpus).run(sys.stdin, sys.stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
