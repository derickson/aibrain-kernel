"""An MCP server over the note index, so a coding agent can search the vault
on its own initiative instead of being handed passages it never asked for.

Run it as `python3 -m aibrain.mcp_search`. Same stdio/JSON-RPC shape as
`mcp_todo.py`: newline-delimited JSON-RPC 2.0, no framing.

Standard library only. `AIBRAIN_CORE_URL` says where the corpus service is;
`AIBRAIN_SEARCH_BRAINS`, if set, is a comma-separated list of brain ids to
scope every search to — the same scoping the agent's own config already
applies, kept in sync because the ACP adapter passes it through when it
launches this.
"""

from __future__ import annotations

import json
import os
import sys
import traceback
from typing import Any, Callable, Iterator

from .corpus import Corpus, CorpusError

PROTOCOL_VERSION = "2025-06-18"
SERVER_NAME = "aibrain-search"
SERVER_VERSION = "1.0.0"

PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

DEFAULT_LIMIT = 8
MAX_LIMIT = 24
SNIPPET_CHARS = 500


# ---------------------------------------------------------------------------
# the tool
# ---------------------------------------------------------------------------

TOOLS: list[dict] = [
    {
        "name": "search_notes",
        "description": (
            "Hybrid search over the user's Obsidian vaults. Call this when a "
            "question needs something specific from their notes. It returns "
            "ranked titles, brains, paths and a short snippet — not the "
            "notes' full text — so read one with your own file tools once "
            "you know which it is. Cite a note by writing its title as "
            "[[Note title]]."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Words to search for."},
                "limit": {"type": "integer",
                          "description": f"At most this many hits (default {DEFAULT_LIMIT})."},
            },
            "required": ["query"],
        },
    },
]


class SearchTools:
    def __init__(self, corpus: Corpus, brain_ids: list[str] | None = None):
        self.corpus = corpus
        self.brain_ids = brain_ids or None
        self._brain_names: dict[str, str] | None = None

    def call(self, name: str, args: dict) -> str:
        handler: Callable[[dict], str] | None = getattr(self, f"_{name}", None)
        if handler is None:
            raise KeyError(name)
        return handler(args)

    def _brain_name(self, brain_id: str) -> str:
        # Fetched lazily, once: most sessions run several searches and none
        # of them need the brain list to change mid-session.
        if self._brain_names is None:
            status = self.corpus.status()
            self._brain_names = {b.get("id"): b.get("name") or b.get("id")
                                 for b in status.get("brains", [])}
        return self._brain_names.get(brain_id, brain_id)

    def _search_notes(self, args: dict) -> str:
        query = str(args.get("query", "")).strip()[:2000]
        if not query:
            return "Nothing to search for: query was empty."
        limit = max(1, min(MAX_LIMIT, int(args.get("limit") or DEFAULT_LIMIT)))
        hits = self.corpus.search(query, limit=limit, brain_ids=self.brain_ids)
        if not hits:
            return f'No notes match "{query}".'
        lines = [f'{len(hits)} match(es) for "{query}":']
        for hit in hits:
            brain = self._brain_name(hit.brain_id)
            snippet = " ".join((hit.snippet or "")[:SNIPPET_CHARS].split())
            lines.append(f"- [[{hit.title}]] — {brain} / {hit.source} ({hit.rel_path})")
            if snippet:
                lines.append(f"    {snippet}")
        return "\n".join(lines)


# ---------------------------------------------------------------------------
# the protocol
# ---------------------------------------------------------------------------

class Server:
    """One MCP session. See `mcp_todo.Server` — same shape, different tools."""

    def __init__(self, corpus: Corpus | None = None, brain_ids: list[str] | None = None):
        self.tools = SearchTools(corpus or Corpus(), brain_ids)
        self.initialized = False

    def handle(self, message: dict) -> dict | None:
        if message.get("jsonrpc") != "2.0":
            return self._error(message.get("id"), INVALID_REQUEST,
                               "not a JSON-RPC 2.0 message")
        method = message.get("method")
        mid = message.get("id")
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
            return self._tool_error(mid, f"bad arguments: {exc}")
        except CorpusError as exc:
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
    brains_env = os.environ.get("AIBRAIN_SEARCH_BRAINS") or ""
    brain_ids = [b for b in brains_env.split(",") if b] or None
    print(f"{SERVER_NAME} talking to {corpus.base_url}", file=sys.stderr)
    Server(corpus, brain_ids).run(sys.stdin, sys.stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
