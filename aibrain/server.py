"""The local web server.

Standard library only, on purpose: the rest of this repo runs on the system
interpreter with no virtualenv, and a single-user local UI does not need more
than a threaded HTTP server with server-sent events for the streaming bits.

Everything the browser needs is under /api; everything it renders comes out of
web/. There is no build step.
"""

from __future__ import annotations

import json
import mimetypes
import re
import threading
import time
import traceback
import urllib.parse
from dataclasses import asdict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable, Iterator

from .agents import Registry
from .config import (AGENT_COLORS, VAULT_LINK_DIR, Config, ScriptConfig,
                     AgentConfig, BrainConfig, discover_vaults, link_problems,
                     reconcile_brains, slugify)
from .corpus import Corpus, CorpusError
from .jobs import JobRunner

WEB_ROOT = Path(__file__).resolve().parent.parent / "web"

mimetypes.add_type("text/javascript", ".js")
mimetypes.add_type("font/woff2", ".woff2")


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
                payload = dict(self.corpus.universe())
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
            return self._universe

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

    def body(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        if not length:
            return {}
        raw = self.rfile.read(length)
        try:
            return json.loads(raw.decode("utf-8"))
        except json.JSONDecodeError:
            return {}

    def query(self) -> dict[str, str]:
        parsed = urllib.parse.urlparse(self.path)
        return {k: v[0] for k, v in urllib.parse.parse_qs(parsed.query).items()}

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

    def _dispatch(self, method: str) -> None:
        path = urllib.parse.urlparse(self.path).path
        fn, params = self.router.match(method, path)
        if fn is not None:
            try:
                fn(self, **params)
            except BrokenPipeError:
                pass
            except Exception as exc:
                traceback.print_exc()
                self.fail(f"{type(exc).__name__}: {exc}", 500)
            return
        if method == "GET":
            self._static(path)
            return
        self.fail("not found", 404)

    def _static(self, path: str) -> None:
        if path == "/":
            path = "/index.html"
        target = (WEB_ROOT / path.lstrip("/")).resolve()
        if WEB_ROOT.resolve() not in target.parents or not target.is_file():
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
        h.json(state.universe(rebuild=rebuild))

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
        text = q.get("q", "")
        limit = min(int(q.get("limit", 60)), 200)
        brain_ids = [b for b in q.get("brains", "").split(",") if b]
        results = state.corpus.search_raw(text, limit=limit,
                                          brain_ids=brain_ids or None)
        h.json({
            "query": text,
            "count": len(results),
            "results": [state.decorate(row) for row in results],
        })

    def note(h: Handler, nid: str) -> None:
        page = state.corpus.note(int(nid))
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
        note_id = state.note_id_for_gid(int(gid))
        if note_id is None:
            h.fail("no such node", 404)
            return
        h.json({"nid": note_id})

    def recent(h: Handler) -> None:
        rows = state.corpus.recent(limit=int(h.query().get("limit", 24)))
        h.json({"results": [state.decorate(row) for row in rows]})

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
        extra: list[str] = []
        for name in payload.get("options", []):
            extra.extend(script.options.get(name, []))
        extra.extend(str(a) for a in payload.get("args", []))
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
        for key, attr in (("rotationSpeed", "rotation_speed"),
                          ("linkOpacity", "link_opacity"),
                          ("showAllLabels", "show_all_labels"),
                          ("ribbonTwist", "ribbon_twist")):
            if key in payload:
                setattr(view, attr, payload[key])
        state.cfg.save()
        h.json({"ok": True, "view": asdict(view)})

    def save_brain(h: Handler, brain_id: str) -> None:
        payload = h.body()
        brain = state.cfg.brain(brain_id)
        if brain is None:
            h.fail("unknown brain", 404)
            return
        for key in ("name", "enabled"):
            if key in payload:
                setattr(brain, key, payload[key])
        state.cfg.save()
        h.json({"ok": True, "needsReindex": True})

    def add_brain(h: Handler) -> None:
        """Link a vault into `obsidian_vaults/`, which is what makes it a brain."""
        payload = h.body()
        raw = payload.get("path", "").strip()
        path = Path(raw).expanduser()
        if not path.is_dir():
            h.fail(f"{path} is not a directory")
            return
        target = path.resolve()

        for existing in discover_vaults():
            if existing.resolve() == target:
                h.fail(f"{target.name} is already linked as {existing.name}")
                return

        VAULT_LINK_DIR.mkdir(parents=True, exist_ok=True)
        link = VAULT_LINK_DIR / (payload.get("name") or target.name)
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
            agent.pos = [round(float(v), 3) for v in pos]
            state.cfg.save()
        h.json({"ok": True})

    # ---- routes ----------------------------------------------------------
    router.get("/api/universe", universe)
    router.get("/api/status", status)
    router.get("/api/search", search)
    router.get("/api/recent", recent)
    router.get("/api/note/<nid>", note)
    router.get("/api/node/<gid>", node)
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
    router.post("/api/agent/<agent_id>", save_agent)
    router.post("/api/agent/<agent_id>/remove", remove_agent)
    router.post("/api/agent/<agent_id>/move", move_agent)
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
            setattr(agent, attr, payload[camel])
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
        agent.context_notes = max(1, min(24, int(payload["contextNotes"])))
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
