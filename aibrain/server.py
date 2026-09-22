"""The local web server.

Standard library only, on purpose: the rest of this repo runs on the system
interpreter with no virtualenv, and a single-user local UI does not need more
than a threaded HTTP server with server-sent events for the streaming bits.

Everything the browser needs is under /api; everything it renders comes out of
web/. There is no build step.
"""

from __future__ import annotations

import hashlib
import json
import math
import mimetypes
import re
import shutil
import threading
import time
import traceback
import urllib.parse
from dataclasses import asdict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable, Iterator

from .agents import Registry
from .config import (AGENT_COLORS, REPO_ROOT, VAULT_LINK_DIR, Config,
                     ScriptConfig, AgentConfig, BrainConfig, discover_vaults,
                     link_problems, reconcile_brains, slugify)
from .corpus import Corpus, CorpusError
from .jobs import JobRunner

WEB_ROOT = Path(__file__).resolve().parent.parent / "web"
# Where scripts/macwhisper/macwhisper_export.py stages raw transcripts —
# see that script's DEFAULT_OUT for the source of truth.
RAW_TRANSCRIPTS_DIR = REPO_ROOT / "raw_transcripts"

mimetypes.add_type("text/javascript", ".js")
mimetypes.add_type("font/woff2", ".woff2")

# This server binds to loopback and has no authentication, because the only
# person it answers to is the one sitting at the machine. Two things break
# that assumption, and both come from a web page the user did not write:
#
#   DNS rebinding  evil.com resolves to 127.0.0.1, so the browser treats
#                  http://evil.com:8760 as same-origin with the page and can
#                  read every note. The `Host` header still says evil.com,
#                  which is how we catch it.
#   CSRF           a cross-site form post reaches /api/agent/<id> and edits the
#                  command an ACP agent runs. `Origin` and `Sec-Fetch-Site`
#                  say where it came from, which is how we catch that.
#
# Neither header can be forged by a page, and neither is sent by a non-browser
# client such as curl or urllib, so the checks cost the CLI nothing.
LOOPBACK_HOSTS = frozenset({"127.0.0.1", "localhost", "::1", "0.0.0.0"})

# Nothing this API accepts is large: the biggest body is a to-do with a handful
# of note references. A cap stops an unbounded read into memory.
MAX_BODY_BYTES = 1 << 20
MAX_TODO_BODY = 4000


def _hostname(netloc: str) -> str:
    """The host out of `host:port`, with IPv6 brackets stripped.

    Fussier than it looks: only a port may follow a `]`, or `[::1].evil.com`
    reads as the loopback address, and a bare `::1` has no port to strip.
    """
    netloc = netloc.strip().lower()
    if netloc.startswith("["):
        inner, sep, tail = netloc[1:].partition("]")
        if not sep or (tail and not (tail[0] == ":" and tail[1:].isdigit())):
            return ""
        return inner
    if netloc.count(":") > 1:
        return netloc
    return netloc.rsplit(":", 1)[0] if ":" in netloc else netloc


class State:
    """Everything the handlers share. One instance per process.

    The corpus itself lives in the Rust service; what is held here is the one
    thing the browser needs that the service does not provide — the mapping
    between a note id and its position in the universe payload, which is just
    the order `noteIds` came back in.
    """

    def __init__(self, cfg: Config, corpus: Corpus | None = None):
        self.cfg = cfg
        self.corpus = corpus or Corpus()
        self.jobs = JobRunner()
        self.agents = Registry(cfg, self.corpus)
        self._universe: dict | None = None
        self._etag = ""
        self._note_ids: list[int] = []
        self._gid_of: dict[int, int] = {}
        self._graph_lock = threading.Lock()
        self.chat_history: dict[str, list[dict]] = {}
        self.active_streams: dict[str, threading.Event] = {}

    # ---- universe --------------------------------------------------------
    def universe(self, rebuild: bool = False) -> dict:
        """The Rust payload plus the things only Python knows about.

        Agents are Python's: they are configured here, dragged here, and the
        Rust service has never heard of them. Everything else is passed through
        untouched, buffers included.
        """
        with self._graph_lock:
            if self._universe is None or rebuild:
                payload, etag = self.corpus.universe_with_etag()
                self._adopt(payload or {}, etag)
            return self._universe

    def universe_with_etag(self, rebuild: bool = False) -> tuple[dict, str]:
        """The payload and the tag that identifies it, asking Rust first.

        `universe()` answers from memory because a dozen call sites use it to
        map a note id to a star. This one always checks, because it answers the
        browser and the browser is the thing that has to notice an edit. The
        check is one small query on the Rust side when nothing has moved.
        """
        with self._graph_lock:
            if self._universe is None or rebuild:
                payload, etag = self.corpus.universe_with_etag()
                self._adopt(payload or {}, etag)
            else:
                payload, etag = self.corpus.universe_with_etag(self._etag)
                if payload is not None:
                    self._adopt(payload, etag)
            return self._universe, self._local_etag()

    def _adopt(self, payload: dict, etag: str) -> None:
        """Fold a fresh Rust payload into the things only Python knows."""
        payload = dict(payload)
        self._note_ids = [int(n) for n in payload.get("noteIds", [])]
        self._gid_of = {nid: gid for gid, nid in enumerate(self._note_ids)}
        payload["agents"] = self._agent_payload(payload.get("brains", []))
        payload["options"] = {
            "rotationSpeed": self.cfg.view.rotation_speed,
            "linkOpacity": self.cfg.view.link_opacity,
            "showAllLabels": self.cfg.view.show_all_labels,
            "ribbonTwist": self.cfg.view.ribbon_twist,
        }
        stats = dict(payload.get("stats", {}))
        stats["agents"] = len(payload["agents"])
        payload["stats"] = stats
        self._fit_camera(payload)
        self._universe = payload
        self._etag = etag

    def _local_etag(self) -> str:
        """Rust's tag, plus what Python added to the payload.

        The agents and the view options are Python's: moving an agent changes
        what the browser must draw without changing a single note, so the tag
        the browser holds has to move too.
        """
        if not self._etag:
            return ""
        mine = json.dumps(
            [self._universe.get("agents", []), self._universe.get("options", {})],
            sort_keys=True, default=_jsonable,
        )
        salt = hashlib.blake2b(mine.encode("utf-8"), digest_size=6).hexdigest()
        return '"%s.%s"' % (self._etag.strip('"'), salt)

    def _agent_payload(self, brains: list[dict]) -> list[dict]:
        agents = self.cfg.enabled_agents()
        slots = agent_slots(len(agents), brains)
        return [
            {
                "id": a.id, "name": a.name, "protocol": a.label(), "kind": a.kind,
                "color": a.color, "pos": a.pos or slots[i], "intro": a.intro,
                "suggestions": a.suggestions,
            }
            for i, a in enumerate(agents)
        ]

    def _fit_camera(self, payload: dict) -> None:
        """Widen Rust's framing to include the agent stars it cannot see."""
        positions = [a["pos"] for a in payload.get("agents", [])]
        if not positions:
            return
        payload["fitWidth"] = round(max(
            float(payload.get("fitWidth", 0.0)),
            max(abs(p[0]) + 4.0 for p in positions) * 2 + 20.0), 1)
        payload["fitHeight"] = round(max(
            float(payload.get("fitHeight", 0.0)),
            max(abs(p[1]) + 4.0 for p in positions) * 2 + 20.0), 1)

    def note_id_for_gid(self, gid: int) -> int | None:
        self.universe()
        if 0 <= gid < len(self._note_ids):
            return self._note_ids[gid]
        return None

    def gid_for_note(self, note_id: int) -> int | None:
        self.universe()
        return self._gid_of.get(int(note_id))

    def brain_names(self) -> dict[str, str]:
        return {b.id: b.name for b in self.cfg.brains}

    def decorate(self, row: dict) -> dict:
        """Add the two things the browser needs that Rust does not send."""
        brain_id = row.get("brain_id", "")
        return {
            **row,
            "gid": self.gid_for_note(row.get("nid", -1)),
            "brain": self.brain_names().get(brain_id, brain_id),
            "brainId": brain_id,
        }

    # ---- jobs ------------------------------------------------------------
    def reindex(self, emit: Callable[[str], None], force: bool = False) -> None:
        emit("asking aibrain-core to rescan the vaults…")
        stats = self.corpus.reindex(force=force)
        emit(
            f"{stats.get('scanned', 0)} notes scanned — "
            f"+{stats.get('added', 0)} added, ~{stats.get('updated', 0)} updated, "
            f"-{stats.get('removed', 0)} removed, ={stats.get('unchanged', 0)} unchanged"
        )
        emit(f"{stats.get('links', 0)} links resolved")
        emit("rebuilding the universe…")
        self.universe(rebuild=True)
        emit("universe ready")

    def close(self) -> None:
        self.agents.close()


def agent_slots(count: int, placed: list[dict]) -> list[list[float]]:
    """Find empty sky for the agent stars.

    Stars parked past the edge of the field make the camera pull back until
    every galaxy is a speck, so prefer space the galaxies already leave: the
    band between stacked rows, and only otherwise the sky above them. Either
    way the stars sit forward of the galaxies in z, so they read as nearer.

    This is the last piece of layout still in Python, because it is the only
    one that depends on something the Rust service does not know exists.
    """
    if count <= 0:
        return []

    centres = [(b["center"][1], b["radius"]) for b in placed] or [(0.0, 10.0)]
    mid_free = all(abs(y) - r > 2.0 for y, r in centres)
    lift = 0.0 if mid_free else max(y + r for y, r in centres) + 7.0

    columns = sorted({round(b["center"][0], 1) for b in placed}) or [0.0]
    reach = max((abs(b["center"][0]) + b["radius"] for b in placed), default=20.0)
    lanes = [(columns[i] + columns[i + 1]) / 2 for i in range(len(columns) - 1)]
    lanes += [-(reach + 6.0), reach + 6.0]
    lanes.sort(key=abs)

    out: list[list[float]] = []
    for i in range(count):
        out.append([
            round(lanes[i % len(lanes)] + (i // len(lanes)) * 3.0, 2),
            round(lift + (2.5 if i % 2 else -2.5), 2),
            round(14.0 + (i % 3) * 2.0, 2),
        ])
    return out


# ---------------------------------------------------------------------------
# request plumbing
# ---------------------------------------------------------------------------

class BadRequest(ValueError):
    """Something the caller sent is wrong, phrased for whoever sent it."""


class Router:
    def __init__(self) -> None:
        self.routes: list[tuple[str, re.Pattern, Callable]] = []

    def add(self, method: str, pattern: str, fn: Callable) -> None:
        regex = re.compile("^" + re.sub(r"<(\w+)>", r"(?P<\1>[^/]+)", pattern) + "$")
        self.routes.append((method, regex, fn))

    def get(self, path: str, fn: Callable) -> None:
        self.add("GET", path, fn)

    def post(self, path: str, fn: Callable) -> None:
        self.add("POST", path, fn)

    def patch(self, path: str, fn: Callable) -> None:
        self.add("PATCH", path, fn)

    def match(self, method: str, path: str):
        for verb, regex, fn in self.routes:
            m = regex.match(path)
            if m and verb == method:
                return fn, m.groupdict()
        return None, {}


class Handler(BaseHTTPRequestHandler):
    server_version = "aibrain"
    protocol_version = "HTTP/1.1"
    state: State
    router: Router

    # ---- helpers ---------------------------------------------------------
    def log_message(self, fmt: str, *args) -> None:  # quieter default logging
        if self.path.startswith("/api") and not self.path.startswith("/api/stream"):
            print(f"  {self.command} {self.path}")

    def _send(self, code: int, body: bytes, ctype: str, extra: dict | None = None) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def json(self, payload: Any, code: int = 200) -> None:
        self._send(code, json.dumps(payload, default=_jsonable).encode("utf-8"),
                   "application/json; charset=utf-8")

    def fail(self, message: str, code: int = 400) -> None:
        self.json({"error": message}, code)

    def not_modified(self, etag: str) -> None:
        """A 304 carries no body, only the tag that is still good."""
        self.send_response(304)
        self.send_header("ETag", etag)
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def matches_etag(self, etag: str) -> bool:
        """Does the browser already hold this exact payload?"""
        header = self.headers.get("If-None-Match") or ""
        want = etag.removeprefix("W/").strip()
        return any(
            candidate.strip() == "*"
            or candidate.strip().removeprefix("W/").strip() == want
            for candidate in header.split(",") if candidate.strip()
        )

    def body(self) -> dict:
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            raise BadRequest("Content-Length is not a number")
        if length < 0 or length > MAX_BODY_BYTES:
            # Refused without reading it, so the bytes are still in the socket;
            # the connection has to go with them or the next request on it
            # would start mid-body.
            self.close_connection = True
            raise BadRequest("request body is too large")
        if not length:
            return {}
        raw = self.rfile.read(length)
        try:
            parsed = json.loads(raw.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            return {}
        # Every caller indexes into this; a bare list or string would be an
        # AttributeError deep inside a handler rather than a 400 here.
        return parsed if isinstance(parsed, dict) else {}

    def query(self) -> dict[str, str]:
        parsed = urllib.parse.urlparse(self.path)
        return {k: v[0] for k, v in urllib.parse.parse_qs(parsed.query).items()}

    def int_query(self, name: str, default: int, low: int, high: int) -> int:
        """A bounded integer from the query string, or a 400 if it is not one.

        Unbounded and unchecked were both real: `limit=abc` was a traceback and
        `limit=-1` travelled all the way to Postgres.
        """
        raw = self.query().get(name)
        if raw is None or raw == "":
            return default
        try:
            value = int(raw)
        except ValueError:
            raise BadRequest(f"{name} must be a whole number")
        return max(low, min(high, value))

    # ---- who is asking ---------------------------------------------------
    def host_is_local(self) -> bool:
        """Reject a `Host` this server was never reached at — DNS rebinding."""
        host = self.headers.get("Host")
        if host is None:      # HTTP/1.0; no browser sends a request without one
            return True
        return _hostname(host) in LOOPBACK_HOSTS

    def origin_is_same(self) -> bool:
        """Same-origin, for anything that changes state or starts work.

        `Sec-Fetch-Site` is set by every browser on every request and cannot be
        set by script; `none` is the user typing the URL. `Origin` is the
        fallback for the one case that predates it, a cross-site form post.
        """
        site = self.headers.get("Sec-Fetch-Site")
        if site is not None and site not in ("same-origin", "none"):
            return False
        origin = self.headers.get("Origin")
        if origin is None:
            return True       # curl, urllib, the MCP server — not a browser
        if origin == "null":
            return False
        parsed = urllib.parse.urlparse(origin)
        host = (self.headers.get("Host") or "").lower()
        return bool(parsed.netloc) and parsed.netloc.lower() == host

    def sse(self, events: Iterator[dict]) -> None:
        """Stream JSON events. Each chunk is one SSE `data:` frame."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache, no-transform")
        self.send_header("Connection", "close")
        self.send_header("X-Accel-Buffering", "no")
        self.end_headers()
        self.close_connection = True
        try:
            for event in events:
                payload = json.dumps(event, default=_jsonable)
                self.wfile.write(f"data: {payload}\n\n".encode("utf-8"))
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception:
            traceback.print_exc()
            try:
                error = json.dumps({"type": "error", "text": "stream failed"})
                self.wfile.write(f"data: {error}\n\n".encode("utf-8"))
                self.wfile.flush()
            except OSError:
                pass

    # ---- dispatch --------------------------------------------------------
    def do_GET(self) -> None:
        self._dispatch("GET")

    def do_POST(self) -> None:
        self._dispatch("POST")

    def do_PATCH(self) -> None:
        self._dispatch("PATCH")

    def _dispatch(self, method: str) -> None:
        if not self.host_is_local():
            self.close_connection = True
            self.fail("this server only answers on localhost", 403)
            return
        # Reads are safe: a cross-site page cannot see the response without
        # CORS headers, and a GET is how a bookmark, a link from anywhere, or
        # the browser's own address bar reaches the page. Only a request that
        # changes state or starts work has to come from the page itself.
        if method not in ("GET", "HEAD") and not self.origin_is_same():
            self.close_connection = True
            self.fail("cross-origin requests are not accepted", 403)
            return
        path = urllib.parse.urlparse(self.path).path
        fn, params = self.router.match(method, path)
        if fn is not None:
            try:
                fn(self, **params)
            except BrokenPipeError:
                pass
            except (BadRequest, CorpusError) as exc:
                # Both carry text written for a person to read, so both are
                # safe to hand back.
                self.fail(str(exc), 400 if isinstance(exc, BadRequest) else 502)
            except Exception:
                # Anything else is a bug, and its message tends to carry
                # absolute paths or query text. The traceback goes to the
                # console the user started the server in; the browser gets
                # nothing it could leak onward.
                traceback.print_exc()
                self.fail("internal error — see the server log", 500)
            return
        if method == "GET":
            self._static(path)
            return
        # A POST/PATCH with no matching route may still have a body sitting
        # unread on the socket — every branch above this point that responds
        # without calling h.body() has to close rather than keep the
        # connection alive, or those bytes get parsed as the start of the
        # next request line and corrupt it too (garbled "Bad request syntax"
        # errors on whatever request happens to reuse the connection next).
        self.close_connection = True
        self.fail("not found", 404)

    def _static(self, path: str) -> None:
        if path == "/":
            path = "/index.html"
        # Percent escapes are decoded first: without this `%2e%2e` never became
        # `..`, but nor did `%20` ever become a space, and a future file with
        # one in its name would have been unreachable for the wrong reason.
        decoded = urllib.parse.unquote(path)
        if "\x00" in decoded:
            self.fail("not found", 404)
            return
        target = (WEB_ROOT / decoded.lstrip("/")).resolve()
        root = WEB_ROOT.resolve()
        # `resolve()` has already collapsed `..` and followed any symlink, so
        # this one check covers traversal and a link pointing out of web/.
        if root not in target.parents or not target.is_file():
            self.fail("not found", 404)
            return
        ctype = mimetypes.guess_type(target.name)[0] or "application/octet-stream"
        if ctype.startswith("text/") or ctype in ("application/javascript",
                                                  "application/json"):
            ctype += "; charset=utf-8"
        data = target.read_bytes()
        cache = "public, max-age=86400" if target.suffix in (".woff2", ".js") \
            and "vendor" in target.parts else "no-store"
        self._send(200, data, ctype, {"Cache-Control": cache})


_HEX_COLOR = re.compile(r"^#[0-9a-fA-F]{3,8}$")


def _is_hex_color(value: Any) -> bool:
    return isinstance(value, str) and bool(_HEX_COLOR.match(value))


def link_name(raw: str) -> str | None:
    """One safe filename for the symlink, or None if it cannot be one.

    Deliberately strict rather than sanitising: silently turning `../evil` into
    `evil` links a vault under a name nobody asked for, and the person typing
    it is standing at the machine and can retype it.
    """
    name = str(raw).strip()
    if not name or name in (".", "..") or len(name) > 128:
        return None
    if name.startswith(".") or any(c in name for c in "/\\\x00") or "\n" in name:
        return None
    return name


def _int_id(raw: str, what: str) -> int:
    """A path segment that has to be an id. The router only promised `[^/]+`."""
    try:
        return int(raw)
    except (TypeError, ValueError):
        raise BadRequest(f"{what} must be a whole number")


def _jsonable(obj: Any) -> Any:
    if hasattr(obj, "to_dict"):
        return obj.to_dict()
    if hasattr(obj, "__dataclass_fields__"):
        return asdict(obj)
    if isinstance(obj, Path):
        return str(obj)
    if isinstance(obj, set):
        return sorted(obj)
    return str(obj)


# ---------------------------------------------------------------------------
# API
# ---------------------------------------------------------------------------

def build_router(state: State) -> Router:
    router = Router()

    # ---- universe --------------------------------------------------------
    def universe(h: Handler) -> None:
        rebuild = h.query().get("rebuild") == "1"
        payload, etag = state.universe_with_etag(rebuild=rebuild)
        # An explicit rebuild is the browser saying "I know it moved", so the
        # conditional is skipped rather than answered.
        if etag and not rebuild and h.matches_etag(etag):
            h.not_modified(etag)
            return
        body = json.dumps(payload, default=_jsonable).encode("utf-8")
        extra = {"ETag": etag} if etag else None
        h._send(200, body, "application/json; charset=utf-8", extra)

    def events(h: Handler) -> None:
        """The Rust change feed, forwarded frame by frame.

        Nothing is collected on the way through: `sse()` writes and flushes
        each event as the generator produces it, and a browser that closes the
        tab breaks the pipe, which ends the generator, which closes the socket
        to Rust.
        """
        def proxied() -> Iterator[dict]:
            try:
                yield from state.corpus.stream_events()
            except CorpusError as exc:
                yield {"type": "error", "text": str(exc)}

        h.sse(proxied())

    def status(h: Handler) -> None:
        core = state.corpus.status()
        rows = {b.get("id"): b for b in core.get("brains", [])}
        # Rust records when it last scanned each brain; the UI wants one date.
        scanned = [b.get("scanned_at") for b in core.get("brains", [])
                   if b.get("scanned_at")]
        payload = {
            "title": state.cfg.title,
            "notes": core.get("notes", 0),
            "links": core.get("links", 0),
            "revision": core.get("revision", 0),
            "indexedAt": max(scanned) if scanned else "",
            "brains": [
                {
                    "id": b.id, "name": b.name, "path": str(b.resolved_path()),
                    "enabled": b.enabled, "exists": b.resolved_path().is_dir(),
                    "notes": rows.get(b.id, {}).get("note_count", 0),
                    # What the layout cache and the change feed both key off.
                    "revision": rows.get(b.id, {}).get("revision", 0),
                    "meetingTarget": b.meeting_target, "meetingFolder": b.meeting_folder,
                }
                for b in state.cfg.brains
            ],
            "agents": [
                {
                    "id": a.id, "name": a.name, "kind": a.kind, "color": a.color,
                    "protocol": a.label(), "enabled": a.enabled,
                    "command": " ".join(a.command), "url": a.url, "cwd": a.cwd,
                    "intro": a.intro, "suggestions": a.suggestions,
                }
                for a in state.cfg.agents
            ],
            "scripts": [asdict(s) for s in state.cfg.scripts],
            "view": asdict(state.cfg.view),
            "todo": asdict(state.cfg.todo),
            "jobs": state.jobs.list()[:8],
        }
        # Whatever else the service reports about itself — the search engine
        # block, for one — travels through rather than being enumerated here.
        for key in ("search",):
            if key in core:
                payload[key] = core[key]
        h.json(payload)

    # ---- search & notes --------------------------------------------------
    def search(h: Handler) -> None:
        q = h.query()
        text = q.get("q", "")[:2000]
        limit = h.int_query("limit", 60, 1, 200)
        brain_ids = [b for b in q.get("brains", "").split(",") if b][:32]
        page = state.corpus.search_page(text, limit=limit,
                                        brain_ids=brain_ids or None)
        results = page.get("results", [])
        h.json({
            "query": text,
            "count": len(results),
            "engine": page.get("engine", ""),
            "results": [state.decorate(row) for row in results],
        })

    def note(h: Handler, nid: str) -> None:
        page = state.corpus.note(_int_id(nid, "note id"))
        if page is None:
            h.fail("no such note", 404)
            return
        h.json({
            **state.decorate(page),
            # The reader shows the path under the title, and the browser has
            # always spelled it in camel case.
            "relPath": page.get("rel_path", ""),
            "linked": [state.decorate(row) for row in page.get("linked", [])],
        })

    def node(h: Handler, gid: str) -> None:
        """Universe node id → note id, so a click in 3D opens the right note."""
        note_id = state.note_id_for_gid(_int_id(gid, "node id"))
        if note_id is None:
            h.fail("no such node", 404)
            return
        h.json({"nid": note_id})

    def recent(h: Handler) -> None:
        rows = state.corpus.recent(limit=h.int_query("limit", 24, 1, 200))
        h.json({"results": [state.decorate(row) for row in rows]})

    # ---- the day's list --------------------------------------------------
    # Pure proxies. The rollover, the day boundary and the history all belong
    # to the service; adding a second opinion here is how the two would drift.
    def todos(h: Handler) -> None:
        q = h.query()
        h.json(state.corpus.todos(day=q.get("day"), now=q.get("now")))

    def todo_add(h: Handler) -> None:
        payload = h.body()
        body = str(payload.get("body", "")).strip()[:MAX_TODO_BODY]
        if not body:
            h.fail("a to-do needs some text")
            return
        h.json(state.corpus.add_todo(
            body,
            scheduled_on=payload.get("scheduled_on"),
            refs=payload.get("refs") or [],
            now=h.query().get("now"),
        ))

    def todo_action(action: str) -> Callable:
        def run(h: Handler, tid: str) -> None:
            now = h.query().get("now")
            payload = h.body()
            todo_id = _int_id(tid, "to-do id")
            if action == "reschedule":
                result = state.corpus.reschedule_todo(
                    todo_id, str(payload.get("to_day", "tomorrow"))[:32], now=now)
            elif action == "link":
                result = state.corpus.link_todo(
                    todo_id, str(payload.get("brain_id", ""))[:200],
                    str(payload.get("rel_path", ""))[:1024], now=now)
            elif action == "file":
                folder_id = payload.get("folder_id")
                result = state.corpus.file_todo(
                    todo_id, int(folder_id) if folder_id is not None else None, now=now)
            else:
                result = getattr(state.corpus, f"{action}_todo")(todo_id, now=now)
            if not result:
                h.fail("no such to-do", 404)
                return
            h.json(result)
        return run

    def todo_patch(h: Handler, tid: str) -> None:
        payload = h.body()
        result = state.corpus.update_todo(
            _int_id(tid, "to-do id"),
            body=payload.get("body"),
            sort_order=payload.get("sort_order"),
            now=h.query().get("now"),
        )
        if not result:
            h.fail("no such to-do", 404)
            return
        h.json(result)

    def todo_history(h: Handler) -> None:
        q = h.query()
        h.json(state.corpus.todo_history(q.get("q", "")[:2000],
                                         limit=h.int_query("limit", 50, 1, 200)))

    # ---- to-do folders -----------------------------------------------------
    def folder_add(h: Handler) -> None:
        name = str(h.body().get("name", "")).strip()[:200]
        if not name:
            h.fail("a folder needs a name")
            return
        h.json(state.corpus.create_folder(name))

    def folder_patch(h: Handler, fid: str) -> None:
        payload = h.body()
        name = payload.get("name")
        ok = state.corpus.update_folder(
            _int_id(fid, "folder id"),
            name=str(name).strip()[:200] if name is not None else None,
            collapsed=payload.get("collapsed"),
            sort_order=payload.get("sort_order"),
        )
        if not ok:
            h.fail("no such folder", 404)
            return
        h.json({"ok": True})

    def folder_delete(h: Handler, fid: str) -> None:
        if not state.corpus.delete_folder(_int_id(fid, "folder id")):
            h.fail("no such folder", 404)
            return
        h.json({"ok": True})

    # ---- chat ------------------------------------------------------------
    def chat(h: Handler) -> None:
        q = h.query()
        agent_id = q.get("agent", "")
        question = q.get("q", "").strip()
        agent = state.agents.get(agent_id)
        if agent is None:
            h.sse(iter([{"type": "error", "text": f"unknown agent {agent_id!r}"},
                        {"type": "done"}]))
            return
        if not question:
            h.sse(iter([{"type": "error", "text": "empty question"},
                        {"type": "done"}]))
            return

        history = state.chat_history.setdefault(agent_id, [])
        history.append({"role": "user", "text": question})
        del history[:-40]

        def events() -> Iterator[dict]:
            answer: list[str] = []
            cites: list[dict] = []
            yield {"type": "open", "agent": agent_id}
            try:
                for event in agent.ask(question, history[:-1]):
                    payload = event.to_dict()
                    if event.type == "delta":
                        answer.append(event.text)
                    if event.type == "cites":
                        cites = payload.get("cites", [])
                    yield payload
            except Exception as exc:
                traceback.print_exc()
                yield {"type": "error", "text": f"{type(exc).__name__}: {exc}"}
                yield {"type": "done"}
            history.append({"role": "agent", "text": "".join(answer), "cites": cites})

        h.sse(events())

    def chat_history(h: Handler, agent_id: str) -> None:
        h.json({"messages": state.chat_history.get(agent_id, [])})

    def chat_clear(h: Handler, agent_id: str) -> None:
        state.chat_history.pop(agent_id, None)
        state.agents.invalidate(agent_id)
        h.json({"ok": True})

    def agent_probe(h: Handler, agent_id: str) -> None:
        agent = state.agents.get(agent_id)
        if agent is None:
            h.fail("unknown agent", 404)
            return
        try:
            h.json(agent.probe())
        except Exception as exc:
            h.json({"ok": False, "detail": f"{type(exc).__name__}: {exc}"})

    # ---- jobs ------------------------------------------------------------
    def run_script(h: Handler, script_id: str) -> None:
        script = state.cfg.script(script_id)
        if script is None:
            h.fail("unknown script", 404)
            return
        payload = h.body()
        # Only the toggles this script declares. The route used to append a
        # free-form `args` list from the request body straight onto the
        # command line: no shell was involved, but "which flags does the
        # exporter run with" is not the browser's decision to make, and the UI
        # never sent one. Options are looked up by name in the script's own
        # table, so the request can only choose between arguments the config
        # already contains.
        names = payload.get("options", [])
        if not isinstance(names, list):
            h.fail("options must be a list of names")
            return
        extra: list[str] = []
        for name in names[:16]:
            extra.extend(script.options.get(str(name), []))
        existing = state.jobs.running(script.name)
        if existing:
            h.json({"job": existing.to_dict(), "alreadyRunning": True})
            return

        def after(_job) -> None:
            if script.reindex_after:
                state.reindex(_job.emit)

        job = state.jobs.run_command(
            script.name, [*script.command, *extra], script.cwd,
            on_success=after if script.reindex_after else None,
        )
        h.json({"job": job.to_dict()})

    def reindex(h: Handler) -> None:
        force = bool(h.body().get("force"))
        existing = state.jobs.running("Reindex")
        if existing:
            h.json({"job": existing.to_dict(), "alreadyRunning": True})
            return
        job = state.jobs.run_task("Reindex", lambda emit: state.reindex(emit, force=force))
        h.json({"job": job.to_dict()})

    def job_list(h: Handler) -> None:
        h.json({"jobs": state.jobs.list()})

    def job_get(h: Handler, job_id: str) -> None:
        job = state.jobs.get(job_id)
        if job is None:
            h.fail("unknown job", 404)
            return
        h.json({**job.to_dict(), "lines": job.lines})

    def job_cancel(h: Handler, job_id: str) -> None:
        job = state.jobs.get(job_id)
        if job is None:
            h.fail("unknown job", 404)
            return
        h.json({"cancelled": job.cancel()})

    def job_stream(h: Handler, job_id: str) -> None:
        job = state.jobs.get(job_id)
        if job is None:
            h.sse(iter([{"type": "error", "text": "unknown job"}, {"type": "done"}]))
            return

        def events() -> Iterator[dict]:
            for line in state.jobs.stream(job):
                if line == "\x00keepalive":
                    yield {"type": "ping"}
                else:
                    yield {"type": "line", "text": line}
            yield {"type": "done", "status": job.status, "exitCode": job.exit_code}

        h.sse(events())

    # ---- config ----------------------------------------------------------
    def save_view(h: Handler) -> None:
        payload = h.body()
        view = state.cfg.view
        # Coerced rather than stored as sent: these end up in config.json and
        # then back in the browser, and a string where a number belongs is a
        # bug that only shows up in the renderer.
        for key, attr, cast in (("rotationSpeed", "rotation_speed", float),
                                ("linkOpacity", "link_opacity", float),
                                ("showAllLabels", "show_all_labels", bool),
                                ("ribbonTwist", "ribbon_twist", float)):
            if key in payload:
                try:
                    value = cast(payload[key])
                except (TypeError, ValueError):
                    raise BadRequest(f"{key} is not a {cast.__name__}")
                if cast is float:
                    value = max(0.0, min(4.0, value))
                setattr(view, attr, value)
        state.cfg.save()
        h.json({"ok": True, "view": asdict(view)})

    def save_brain(h: Handler, brain_id: str) -> None:
        payload = h.body()
        brain = state.cfg.brain(brain_id)
        if brain is None:
            h.fail("unknown brain", 404)
            return
        if "color" in payload and not _is_hex_color(payload["color"]):
            # Ends up inside a `style="…"` on the chip and the universe
            # wireframe; anything but a hex colour belongs somewhere else.
            raise BadRequest("color must be a hex value like #4db3f0")
        if "meetingFolder" in payload and not str(payload["meetingFolder"]).strip():
            raise BadRequest("meetingFolder must not be empty")
        for key in ("name", "enabled", "color"):
            if key in payload:
                setattr(brain, key, payload[key])
        if "meetingFolder" in payload:
            brain.meeting_folder = str(payload["meetingFolder"]).strip()
        if "meetingTarget" in payload:
            brain.meeting_target = bool(payload["meetingTarget"])
            if brain.meeting_target:
                # Only one vault can catch raw_transcripts/ at a time.
                for other in state.cfg.brains:
                    if other.id != brain.id:
                        other.meeting_target = False
        state.cfg.save()
        # A colour-only change is cosmetic — the universe already has every
        # note placed, so there is nothing for a rescan to fix.
        needs_reindex = any(k in payload for k in ("name", "enabled"))
        h.json({"ok": True, "needsReindex": needs_reindex})

    def add_brain(h: Handler) -> None:
        """Link a vault into `obsidian_vaults/`, which is what makes it a brain."""
        payload = h.body()
        raw = str(payload.get("path", "")).strip()
        path = Path(raw).expanduser()
        if not path.is_dir():
            h.fail(f"{path} is not a directory")
            return
        target = path.resolve()

        for existing in discover_vaults():
            if existing.resolve() == target:
                h.fail(f"{target.name} is already linked as {existing.name}")
                return

        # The name becomes a filename inside obsidian_vaults/. Unchecked it was
        # also a path: `{"name": "../../.ssh/authorized_keys"}` put the symlink
        # anywhere the user could write.
        name = link_name(payload.get("name") or target.name)
        if name is None:
            h.fail("that name cannot be used as a folder name")
            return
        VAULT_LINK_DIR.mkdir(parents=True, exist_ok=True)
        link = VAULT_LINK_DIR / name
        if link.parent.resolve() != VAULT_LINK_DIR.resolve():
            h.fail("that name cannot be used as a folder name")
            return
        if link.exists() or link.is_symlink():
            h.fail(f"{link.name} already exists in obsidian_vaults/")
            return
        try:
            # Absolute, because the relative form is easy to get wrong — the
            # repo's own AgenticTest link was broken for exactly that reason.
            link.symlink_to(target, target_is_directory=True)
        except OSError as exc:
            h.fail(f"could not create the link: {exc}")
            return

        reconcile_brains(state.cfg)
        state.cfg.save()
        h.json({"ok": True, "id": slugify(link.name), "needsReindex": True})

    def remove_brain(h: Handler, brain_id: str) -> None:
        """Remove the symlink. The vault itself is never touched."""
        brain = state.cfg.brain(brain_id)
        if brain is None:
            h.fail("unknown brain", 404)
            return
        link = Path(brain.path)
        if link.parent.resolve() != VAULT_LINK_DIR.resolve():
            h.fail("this brain is not linked from obsidian_vaults/")
            return
        if not link.is_symlink():
            h.fail(f"{link} is a real directory, not a link — refusing to delete it")
            return
        try:
            link.unlink()
        except OSError as exc:
            h.fail(f"could not remove the link: {exc}")
            return

        reconcile_brains(state.cfg)
        state.cfg.save()
        h.json({"ok": True, "needsReindex": True})

    def discover(h: Handler) -> None:
        """What is linked, and what is linked but broken."""
        known = {str(b.resolved_path().resolve()) for b in state.cfg.brains}
        found = []
        for vault in discover_vaults():
            resolved = str(vault.resolve())
            if resolved in known:
                continue
            count = sum(1 for _ in vault.rglob("*.md"))
            found.append({"path": str(vault), "name": vault.name, "notes": count})
        h.json({
            "vaults": found,
            "linkDir": str(VAULT_LINK_DIR),
            "problems": [{"name": n, "reason": r} for n, r in link_problems()],
        })

    def meeting_preview(h: Handler) -> None:
        """What absorbing raw_transcripts/ into the target vault would do.

        Preview only — nothing is copied here; see import_meetings for that.
        """
        target = next((b for b in state.cfg.brains if b.meeting_target), None)
        if target is None:
            h.fail("no brain is set as the meeting recording target", 409)
            return
        dest = target.resolved_meeting_folder()
        files = []
        if RAW_TRANSCRIPTS_DIR.is_dir():
            for path in sorted(RAW_TRANSCRIPTS_DIR.glob("*.md")):
                files.append({
                    "name": path.name,
                    "status": "overwrite" if (dest / path.name).exists() else "new",
                    "size": path.stat().st_size,
                })
        h.json({
            "brainId": target.id, "brainName": target.name,
            "folder": str(dest), "files": files,
        })

    def import_meetings(h: Handler) -> None:
        """Copy the chosen raw_transcripts/ files into the target vault, then rescan.

        Runs as a job (like any script) so the console shows exactly what
        happened to each file. Copies rather than moves raw_transcripts/, so
        re-running the exporter and the import later is always safe — the
        staging folder is a mirror of MacWhisper, not a one-shot queue.
        """
        payload = h.body()
        target = next((b for b in state.cfg.brains if b.meeting_target), None)
        if target is None:
            h.fail("no brain is set as the meeting recording target", 409)
            return
        staged = {p.name: p for p in RAW_TRANSCRIPTS_DIR.glob("*.md")} if RAW_TRANSCRIPTS_DIR.is_dir() else {}
        names = payload.get("files")
        if names is None:
            selected = list(staged)
        elif isinstance(names, list):
            # Only filenames actually staged right now — never trust a path
            # the browser merely claims exists.
            selected = [n for n in names if isinstance(n, str) and n in staged]
        else:
            h.fail("files must be a list of names")
            return
        if not selected:
            h.fail("nothing to import")
            return

        job_name = "Import meeting transcripts"
        existing = state.jobs.running(job_name)
        if existing:
            h.json({"job": existing.to_dict(), "alreadyRunning": True})
            return

        def run(emit: Callable[[str], None]) -> None:
            dest = target.resolved_meeting_folder()
            dest.mkdir(parents=True, exist_ok=True)
            added = overwritten = 0
            for name in selected:
                dest_path = dest / name
                was_there = dest_path.exists()
                shutil.copy2(staged[name], dest_path)
                if was_there:
                    overwritten += 1
                    emit(f"overwrote {name}")
                else:
                    added += 1
                    emit(f"added {name}")
            emit(f"{added} added, {overwritten} overwritten in {dest}")
            state.reindex(emit)

        job = state.jobs.run_task(job_name, run)
        h.json({"job": job.to_dict()})

    def save_agent(h: Handler, agent_id: str) -> None:
        payload = h.body()
        agent = state.cfg.agent(agent_id)
        if agent is None:
            h.fail("unknown agent", 404)
            return
        _apply_agent(agent, payload)
        state.cfg.save()
        state.agents.invalidate(agent_id)
        state.universe(rebuild=True)
        h.json({"ok": True})

    def add_agent(h: Handler) -> None:
        payload = h.body()
        name = payload.get("name") or "New agent"
        aid = slugify(name)
        if state.cfg.agent(aid):
            aid = f"{aid}-{len(state.cfg.agents)}"
        index = len(state.cfg.agents)
        agent = AgentConfig(
            id=aid, name=name, kind=payload.get("kind", "local"),
            color=AGENT_COLORS[index % len(AGENT_COLORS)],
        )
        _apply_agent(agent, payload)
        state.cfg.agents.append(agent)
        state.cfg.save()
        state.universe(rebuild=True)
        h.json({"ok": True, "id": aid})

    def remove_agent(h: Handler, agent_id: str) -> None:
        state.agents.invalidate(agent_id)
        state.cfg.agents = [a for a in state.cfg.agents if a.id != agent_id]
        state.cfg.save()
        state.universe(rebuild=True)
        h.json({"ok": True})

    def move_agent(h: Handler, agent_id: str) -> None:
        payload = h.body()
        agent = state.cfg.agent(agent_id)
        if agent is None:
            h.fail("unknown agent", 404)
            return
        pos = payload.get("pos")
        if isinstance(pos, list) and len(pos) == 3:
            try:
                agent.pos = [round(float(v), 3) for v in pos]
            except (TypeError, ValueError):
                raise BadRequest("pos must be three numbers")
            if any(not math.isfinite(v) for v in agent.pos):
                raise BadRequest("pos must be three numbers")
            state.cfg.save()
        h.json({"ok": True})

    def reset_agent_positions(h: Handler) -> None:
        """Drop every dragged-to position so agents fall back to their slots."""
        moved = [a for a in state.cfg.agents if a.pos is not None]
        for agent in moved:
            agent.pos = None
        if moved:
            state.cfg.save()
            state.universe(rebuild=True)
        h.json({"ok": True, "changed": bool(moved)})

    # ---- routes ----------------------------------------------------------
    router.get("/api/universe", universe)
    router.get("/api/events", events)
    router.get("/api/status", status)
    router.get("/api/search", search)
    router.get("/api/recent", recent)
    router.get("/api/note/<nid>", note)
    router.get("/api/node/<gid>", node)
    # The history route is registered before /api/todos/<tid> so "history" is
    # never read as an id.
    router.get("/api/todos", todos)
    router.post("/api/todos", todo_add)
    router.get("/api/todos/history", todo_history)
    router.patch("/api/todos/<tid>", todo_patch)
    for _action in ("complete", "uncomplete", "cancel", "reschedule", "link", "file"):
        router.post(f"/api/todos/<tid>/{_action}", todo_action(_action))
    router.post("/api/todos/folders", folder_add)
    router.patch("/api/todos/folders/<fid>", folder_patch)
    router.post("/api/todos/folders/<fid>/delete", folder_delete)
    router.get("/api/stream/chat", chat)
    router.get("/api/chat/<agent_id>/history", chat_history)
    router.post("/api/chat/<agent_id>/clear", chat_clear)
    router.get("/api/agent/<agent_id>/probe", agent_probe)
    router.post("/api/script/<script_id>/run", run_script)
    router.post("/api/reindex", reindex)
    router.get("/api/jobs", job_list)
    router.get("/api/job/<job_id>", job_get)
    router.post("/api/job/<job_id>/cancel", job_cancel)
    router.get("/api/stream/job/<job_id>", job_stream)
    router.post("/api/view", save_view)
    router.post("/api/brain/<brain_id>", save_brain)
    router.post("/api/brain/<brain_id>/remove", remove_brain)
    router.post("/api/brains/add", add_brain)
    router.get("/api/brains/discover", discover)
    router.get("/api/meetings/preview", meeting_preview)
    router.post("/api/meetings/import", import_meetings)
    router.post("/api/agent/<agent_id>", save_agent)
    router.post("/api/agent/<agent_id>/remove", remove_agent)
    router.post("/api/agent/<agent_id>/move", move_agent)
    router.post("/api/agents/reset-positions", reset_agent_positions)
    router.post("/api/agents/add", add_agent)
    return router


def _apply_agent(agent: AgentConfig, payload: dict) -> None:
    import shlex
    simple = {
        "name": "name", "kind": "kind", "color": "color", "protocol": "protocol",
        "intro": "intro", "url": "url", "cwd": "cwd", "enabled": "enabled",
    }
    for camel, attr in simple.items():
        if camel in payload:
            value = payload[camel]
            if attr == "color" and not _is_hex_color(value):
                # This ends up inside a `style="…"` on a label in the
                # universe; anything but a hex colour belongs somewhere else.
                raise BadRequest("color must be a hex value like #b48cff")
            if attr in ("name", "kind", "protocol", "intro", "url", "cwd") \
                    and not isinstance(value, str):
                raise BadRequest(f"{camel} must be text")
            setattr(agent, attr, value)
    if "command" in payload:
        raw = payload["command"]
        agent.command = raw if isinstance(raw, list) else shlex.split(raw or "")
    if "suggestions" in payload:
        raw = payload["suggestions"]
        agent.suggestions = raw if isinstance(raw, list) else \
            [s.strip() for s in str(raw).splitlines() if s.strip()]
    if "headers" in payload and isinstance(payload["headers"], dict):
        agent.headers = payload["headers"]
    if "brains" in payload and isinstance(payload["brains"], list):
        agent.brains = payload["brains"]
    if "contextNotes" in payload:
        try:
            agent.context_notes = max(1, min(24, int(payload["contextNotes"])))
        except (TypeError, ValueError):
            raise BadRequest("contextNotes is not a number")
    if not agent.protocol:
        agent.protocol = agent.label()


def serve(cfg: Config, open_browser: bool = True) -> None:
    corpus = Corpus()
    state = State(cfg, corpus)
    router = build_router(state)

    handler = type("BoundHandler", (Handler,), {"state": state, "router": router})
    httpd = ThreadingHTTPServer((cfg.host, cfg.port), handler)
    httpd.daemon_threads = True

    url = f"http://{cfg.host}:{cfg.port}/"
    print(f"\n  {cfg.title} — {url}")
    print(f"  config  {cfg.path}")
    print(f"  corpus  {corpus.base_url}")
    brains = cfg.enabled_brains()
    print(f"  brains  {', '.join(b.name for b in brains) or '(none configured)'}")

    # The universe payload is the expensive read; warm it so the first page
    # load is not waiting on a cold Postgres.
    threading.Thread(target=_warm, args=(state,), daemon=True).start()

    if open_browser:
        def launch() -> None:
            time.sleep(0.6)
            import webbrowser
            webbrowser.open(url)
        threading.Thread(target=launch, daemon=True).start()

    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\n  shutting down")
    finally:
        state.close()
        httpd.server_close()


def _warm(state: State) -> None:
    try:
        state.universe(rebuild=True)
    except CorpusError as exc:
        print(f"  could not load the universe: {exc}")
