"""Smoke tests for the AI Brain kernel.

Standard library only, like the rest of the repo — run them with:

    python3 -m unittest tests.test_kernel

Parsing, link resolution, layout and markdown rendering all live in Rust now
and are tested there. What is left here is the seam: the HTTP surface the
browser consumes, the citation tiers, vault discovery and the job runner.

The tests that need a corpus start a real `aibrain-core` against a temp vault
on an ephemeral port and point the Python side at it. If Postgres is not
reachable they skip with a message rather than failing, because a missing
container is an environment problem, not a broken build.
"""

from __future__ import annotations

import json
import math
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
import uuid
from http.server import ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from aibrain.agents import LocalAgent                               # noqa: E402
from aibrain.agents.base import EVIDENCE_ORDER, normalize           # noqa: E402
from aibrain.config import (AgentConfig, BrainConfig, Config,       # noqa: E402
                            ScriptConfig, default_agents)
from aibrain.corpus import Corpus, CorpusError, Hit, unreachable_message  # noqa: E402
from aibrain.jobs import JobRunner                                  # noqa: E402
from aibrain.server import (Handler, State, build_router,           # noqa: E402
                            link_name)

REPO_ROOT = Path(__file__).resolve().parent.parent

VAULT = {
    "Index.md": (
        "---\ntitle: Index\ntags: root\n---\n\n"
        "# Index\n\nEntry point. See [[Protocols]], [[Recipes/Sourdough]] and "
        "[[Nothing Here]].\n"
    ),
    "Protocols.md": (
        "# Protocols\n\nNotes on **ACP** and `A2A`.\n\n"
        "- [[Index]] is the parent\n- [[Agents]] use these\n\n"
        "| Name | Transport |\n|---|---|\n| ACP | stdio |\n| A2A | http |\n"
    ),
    "Agents.md": "# Agents\n\nAgents speak [[Protocols]].\n\n- [ ] wire it up\n- [x] read it\n",
    "Recipes/Sourdough.md": "# Sourdough\n\nStarter notes. Related: [[Index]].\n",
    "Recipes/Ramen.md": "# Ramen\n\nMiso broth. #cooking\n",
    "Journal/2026-01-01.md": "# New year\n\nThought about [[Agents]] today.\n",
}


def every_key(value) -> set[str]:
    """Every key name anywhere in a JSON payload, however deeply nested."""
    found: set[str] = set()
    stack = [value]
    while stack:
        node = stack.pop()
        if isinstance(node, dict):
            found.update(node.keys())
            stack.extend(node.values())
        elif isinstance(node, list):
            stack.extend(node)
    return found


def make_vault(root: Path) -> None:
    for rel, body in VAULT.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")


# ---------------------------------------------------------------------------
# the corpus fixture
# ---------------------------------------------------------------------------

TEST_DATABASE_URL = os.environ.get(
    "AIBRAIN_TEST_DATABASE_URL",
    "postgres://aibrain:aibrain@127.0.0.1:5433/aibrain_test",
)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def core_binary() -> Path | None:
    """The debug build, built on demand. None if cargo cannot produce one."""
    binary = REPO_ROOT / "rust" / "target" / "debug" / "aibrain-core"
    if binary.exists():
        return binary
    if shutil.which("cargo") is None:
        return None
    result = subprocess.run(
        ["cargo", "build", "--manifest-path", str(REPO_ROOT / "rust" / "Cargo.toml")],
        capture_output=True, text=True,
    )
    return binary if result.returncode == 0 and binary.exists() else None


class CorpusFixture:
    """A temp vault, a temp config, and an `aibrain-core` serving them.

    Each instance gets its own brain ids, so two of these can share the test
    database without seeing each other's notes.
    """

    def __init__(self, extra_vaults: dict[str, dict] | None = None):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.home = root / "home"
        self.home.mkdir()
        self.proc: subprocess.Popen | None = None

        tag = uuid.uuid4().hex[:8]
        self.vault = root / "vault"
        make_vault(self.vault)
        self.brain_id = f"t-{tag}"
        brains = [BrainConfig(id=self.brain_id, name="Test", path=str(self.vault),
                              seed=7)]
        self.extra_ids: dict[str, str] = {}
        for i, (name, files) in enumerate((extra_vaults or {}).items()):
            path = root / name
            for rel, body in files.items():
                target = path / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(body, encoding="utf-8")
            bid = f"{name}-{tag}"
            self.extra_ids[name] = bid
            brains.append(BrainConfig(id=bid, name=name, path=str(path),
                                      seed=20 + i * 13))

        self.cfg = Config(
            brains=brains,
            agents=[a for a in default_agents() if a.kind == "local"],
            scripts=[ScriptConfig(
                id="echo", name="Echo", description="test script",
                command=[sys.executable, "-c", "print('hello from a job')"],
            )],
            port=0,
        )
        self.cfg.path = self.home / "config.json"
        self.cfg.save()

        self.port = free_port()
        self.base_url = f"http://127.0.0.1:{self.port}"
        self.corpus = Corpus(self.base_url)

    def start(self, binary: Path) -> None:
        env = {
            **os.environ,
            "AIBRAIN_HOME": str(self.home),
            "AIBRAIN_DATABASE_URL": TEST_DATABASE_URL,
            "AIBRAIN_BIND": f"127.0.0.1:{self.port}",
            "AIBRAIN_LOG": "aibrain_core=warn",
            # Lets the to-do tests say what time it is. The service refuses
            # `?now=` unless this is set, so production cannot time travel.
            "AIBRAIN_TODO_TEST_CLOCK": "1",
        }
        # The binary loads the repo `.env`, which carries live Elasticsearch
        # credentials. A test must never index a throwaway vault into the real
        # cluster, so Elasticsearch is switched off unless a run opts in.
        if not os.environ.get("AIBRAIN_TEST_ELASTICSEARCH"):
            env["ELASTICSEARCH_URL"] = ""
            env["ELASTICSEARCH_API_KEY"] = ""
        self.proc = subprocess.Popen(
            [str(binary), "serve"], env=env,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        deadline = time.time() + 40
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(
                    "aibrain-core exited: " + (self.proc.stdout.read() or "")[-800:])
            try:
                self.corpus.health(timeout=1.0)
                return
            except CorpusError:
                time.sleep(0.25)
        raise RuntimeError(f"aibrain-core never answered on {self.base_url}")

    def reindex(self, force: bool = False) -> dict:
        return self.corpus.reindex(force=force)

    def stop(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.tmp.cleanup()


_SKIP_REASON: str | None = None


def start_corpus(extra_vaults: dict[str, dict] | None = None) -> CorpusFixture:
    """Bring a corpus up, or raise `unittest.SkipTest` with the reason."""
    global _SKIP_REASON
    if _SKIP_REASON:
        raise unittest.SkipTest(_SKIP_REASON)
    binary = core_binary()
    if binary is None:
        _SKIP_REASON = ("aibrain-core is not built and cargo could not build it — "
                        "run `cargo build --manifest-path rust/Cargo.toml`")
        raise unittest.SkipTest(_SKIP_REASON)
    fixture = CorpusFixture(extra_vaults)
    try:
        fixture.start(binary)
    except Exception as exc:
        fixture.stop()
        _SKIP_REASON = (
            f"could not start aibrain-core against {TEST_DATABASE_URL}: {exc} — "
            "start Postgres with `docker compose up -d db` and create the "
            "aibrain_test database, or set AIBRAIN_TEST_DATABASE_URL"
        )
        raise unittest.SkipTest(_SKIP_REASON) from exc
    fixture.reindex(force=True)
    return fixture


# ---------------------------------------------------------------------------
# the HTTP surface
# ---------------------------------------------------------------------------

class ServerTests(unittest.TestCase):
    """Drives the real HTTP server the way the browser does."""

    @classmethod
    def setUpClass(cls):
        # Registered as each piece comes up, not gathered into tearDownClass,
        # so a failure partway through setUpClass (a bad rebuild, a port
        # already taken) still stops what did start rather than leaking the
        # aibrain-core process or the HTTP server past this test.
        cls.fixture = start_corpus()
        cls.addClassCleanup(cls.fixture.stop)
        cls.state = State(cls.fixture.cfg, cls.fixture.corpus)
        cls.addClassCleanup(cls.state.close)
        cls.state.universe(rebuild=True)

        handler = type("H", (Handler,),
                       {"state": cls.state, "router": build_router(cls.state)})
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        cls.addClassCleanup(cls.httpd.shutdown)
        cls.httpd.daemon_threads = True
        cls.base = f"http://127.0.0.1:{cls.httpd.server_address[1]}"
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()

    # ---- helpers ---------------------------------------------------------
    def get(self, path):
        with urllib.request.urlopen(self.base + path, timeout=30) as r:
            return json.loads(r.read().decode())

    def post(self, path, body=None):
        data = json.dumps(body or {}).encode()
        req = urllib.request.Request(self.base + path, data=data, method="POST")
        req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read().decode())

    def sse(self, path, limit=400):
        events = []
        with urllib.request.urlopen(self.base + path, timeout=120) as r:
            for raw in r:
                line = raw.decode()
                if line.startswith("data:"):
                    events.append(json.loads(line[5:]))
                    if events[-1].get("type") == "done" or len(events) >= limit:
                        break
        return events

    # ---- tests -----------------------------------------------------------
    def test_status_reports_the_corpus_without_a_node_cap(self):
        status = self.get("/api/status")
        self.assertEqual(status["notes"], len(VAULT))
        self.assertEqual(len(status["brains"]), 1)
        self.assertEqual(status["brains"][0]["notes"], len(VAULT))
        # The cap is gone, so nothing may report one.
        self.assertNotIn("maxNodes", status["brains"][0])

    def test_universe_carries_the_buffers_the_renderer_reads(self):
        universe = self.get("/api/universe")
        self.assertEqual(universe["stats"]["notes"], len(VAULT))
        self.assertTrue(universe["agents"])
        brain = universe["brains"][0]
        # Geometry comes from Rust as flat arrays, one entry per note.
        self.assertEqual(len(brain["positions"]), len(VAULT) * 3)
        for key in ("sizes", "sourceIndex", "degrees", "names"):
            self.assertEqual(len(brain[key]), len(VAULT), key)
        self.assertEqual(len(universe["noteIds"]), len(VAULT))
        # Edge indices are local to their brain.
        for a, b in brain["edges"]:
            self.assertTrue(0 <= a < len(VAULT) and 0 <= b < len(VAULT))

    def test_universe_places_every_note_it_names(self):
        """Every id in `noteIds` has a position, because nothing is capped."""
        universe = self.get("/api/universe")
        placed = 0
        for brain in universe["brains"]:
            self.assertEqual(len(brain["positions"]) % 3, 0)
            self.assertTrue(all(math.isfinite(v) for v in brain["positions"]))
            placed += len(brain["positions"]) // 3
        self.assertEqual(placed, len(universe["noteIds"]))
        self.assertEqual(placed, universe["stats"]["notes"])

    def test_nothing_reports_a_cap_or_a_shown_count(self):
        """1c deleted the cap, so no payload may still describe one."""
        for path in ("/api/status", "/api/universe"):
            keys = every_key(self.get(path))
            for gone in ("maxNodes", "max_nodes", "shown"):
                self.assertNotIn(gone, keys, f"{gone} survives in {path}")

    def test_static_files_are_served_and_confined(self):
        with urllib.request.urlopen(self.base + "/", timeout=10) as r:
            self.assertIn(b"<title>AI Brains</title>", r.read())
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(self.base + "/../aibrain/config.py", timeout=10)
        self.assertEqual(caught.exception.code, 404)

    def test_search_then_open_a_note(self):
        found = self.get("/api/search?q=protocols")
        self.assertTrue(found["results"])
        # Which engine answered travels through the proxy; the fixture runs
        # without Elasticsearch, so this is the lexical one.
        self.assertEqual(found["engine"], "postgres")
        top = found["results"][0]
        self.assertEqual(top["brain"], "Test")
        self.assertEqual(top["brainId"], self.fixture.brain_id)
        self.assertIsNotNone(top["gid"])

        note = self.get(f"/api/note/{top['nid']}")
        self.assertEqual(note["name"], "Protocols")
        self.assertIn("<table>", note["html"])
        self.assertTrue(note["linked"])
        self.assertGreater(note["words"], 0)
        self.assertEqual(note["brain"], "Test")
        # Neighbours are decorated too, or hovering one lights nothing up.
        self.assertIsNotNone(note["linked"][0]["gid"])

        # The universe id round-trips back to the same note.
        self.assertEqual(self.get(f"/api/node/{note['gid']}")["nid"], note["nid"])

    def test_search_open_note_and_chat_citation_agree_on_the_same_note(self):
        """The full user journey: find it, read it, then watch an answer cite it.

        Search, the note reader and the citation matcher are three separate
        code paths — Postgres full-text search, `db::note_page`, and
        `cites_from_text` matching a wikilink back to a supplied hit. They are
        only trustworthy together if all three agree on which note "Protocols"
        actually is.
        """
        found = self.get("/api/search?q=protocols")
        top = next(r for r in found["results"] if r["brain"] == "Test")

        note = self.get(f"/api/note/{top['nid']}")
        self.assertEqual(note["name"], "Protocols")

        events = self.sse("/api/stream/chat?agent=kernel&q=protocols")
        cites = next(e for e in events if e["type"] == "cites")["cites"]
        self.assertTrue(cites)
        self.assertEqual(
            cites[0]["nid"], top["nid"],
            "the note the agent cited is the same note search and /api/note agree on")

    def test_search_can_be_scoped_to_one_brain(self):
        mine = self.get(f"/api/search?q=protocols&brains={self.fixture.brain_id}")
        self.assertTrue(mine["results"])
        elsewhere = self.get("/api/search?q=protocols&brains=no-such-brain")
        self.assertEqual(elsewhere["results"], [])

    def test_recent_is_decorated_for_the_browser(self):
        rows = self.get("/api/recent?limit=5")["results"]
        self.assertTrue(rows)
        self.assertEqual(rows[0]["brain"], "Test")
        self.assertIn("gid", rows[0])

    def test_missing_note_is_a_404(self):
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(self.base + "/api/note/999999", timeout=10)
        self.assertEqual(caught.exception.code, 404)

    def test_local_agent_streams_an_answer_with_citations(self):
        events = self.sse("/api/stream/chat?agent=kernel&q=protocols")
        kinds = [e["type"] for e in events]
        self.assertIn("delta", kinds)
        self.assertEqual(kinds[-1], "done")
        answer = "".join(e.get("text", "") for e in events if e["type"] == "delta")
        self.assertIn("Protocols", answer)
        cites = next(e for e in events if e["type"] == "cites")["cites"]
        self.assertTrue(cites)
        self.assertIn("nid", cites[0])
        # The answer is markdown, rendered server-side, and every hit in it is
        # a link the browser can open — the same note its citation pill opens.
        html = next(e for e in events if e["type"] == "cites")["html"]
        for cite in cites:
            self.assertIn(f'data-note="{cite["nid"]}"', html)
        self.assertNotIn("wiki-missing", html)

    def test_unknown_agent_reports_an_error_rather_than_hanging(self):
        events = self.sse("/api/stream/chat?agent=nope&q=hi")
        self.assertEqual(events[0]["type"], "error")
        self.assertEqual(events[-1]["type"], "done")

    def test_the_done_event_reports_how_long_the_turn_took(self):
        # However an agent measures itself (an a2a agent may report token
        # usage on its own), wall-clock elapsed time is the server's to know
        # regardless of which agent kind answered.
        events = self.sse("/api/stream/chat?agent=kernel&q=protocols")
        done = events[-1]
        self.assertEqual(done["type"], "done")
        self.assertIsInstance(done["elapsedMs"], int)
        self.assertGreaterEqual(done["elapsedMs"], 0)

    def test_chat_history_is_kept_and_clearable(self):
        self.sse("/api/stream/chat?agent=kernel&q=ramen")
        history = self.get("/api/chat/kernel/history")["messages"]
        self.assertTrue(any(m["role"] == "user" for m in history))
        self.post("/api/chat/kernel/clear")
        self.assertEqual(self.get("/api/chat/kernel/history")["messages"], [])

    def test_script_runs_and_streams_its_output(self):
        job = self.post("/api/script/echo/run")["job"]
        events = self.sse(f"/api/stream/job/{job['id']}")
        lines = [e["text"] for e in events if e["type"] == "line"]
        self.assertTrue(any("hello from a job" in line for line in lines))
        self.assertEqual(events[-1]["status"], "done")
        self.assertEqual(self.get(f"/api/job/{job['id']}")["exitCode"], 0)

    def test_reindex_endpoint_reports_the_rust_stats(self):
        job = self.post("/api/reindex", {"force": False})["job"]
        events = self.sse(f"/api/stream/job/{job['id']}")
        self.assertEqual(events[-1]["status"], "done")
        lines = " ".join(e.get("text", "") for e in events if e["type"] == "line")
        self.assertIn("notes scanned", lines)
        self.assertIn("links resolved", lines)

    def test_view_settings_round_trip(self):
        saved = self.post("/api/view", {"linkOpacity": 0.42})
        self.assertAlmostEqual(saved["view"]["link_opacity"], 0.42)
        self.assertAlmostEqual(self.get("/api/status")["view"]["link_opacity"], 0.42)

    # ---- who is allowed to ask -------------------------------------------
    def raw(self, method, path, headers=None, body=None):
        """One request with exactly the headers given. Returns (code, body)."""
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method)
        for key, value in (headers or {}).items():
            req.add_header(key, value)
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read()

    def test_a_host_header_we_were_not_reached_at_is_refused(self):
        """DNS rebinding: evil.com resolving to 127.0.0.1 still says evil.com."""
        for host in ("evil.com", "evil.com:8760", "127.0.0.1.evil.com",
                     "[::1].evil.com"):
            code, body = self.raw("GET", "/api/status", {"Host": host})
            self.assertEqual(code, 403, f"{host} was allowed: {body!r}")
        # The real thing still works.
        self.assertEqual(self.raw("GET", "/api/status")[0], 200)

    def test_a_cross_site_request_is_refused(self):
        """CSRF: a page on another origin must not drive this API."""
        host = self.base.removeprefix("http://")
        blocked = [
            {"Origin": "http://evil.com"},
            {"Origin": "null"},
            {"Sec-Fetch-Site": "cross-site"},
            # Another local app on a different port is a different origin,
            # and browsers call that "same-site" — it is still not us.
            {"Sec-Fetch-Site": "same-site"},
        ]
        for headers in blocked:
            code, _ = self.raw("POST", "/api/view", headers, {"linkOpacity": 0.9})
            self.assertEqual(code, 403, headers)
            # A read is not a CSRF vector, and a navigation from a link on
            # any site is exactly how someone opens the app.
            code, _ = self.raw("GET", "/api/status", headers)
            self.assertEqual(code, 200, headers)
            code, _ = self.raw("GET", "/", headers)
            self.assertEqual(code, 200, headers)
        # The page itself, and a non-browser client, both get through.
        for headers in ({"Origin": f"http://{host}", "Sec-Fetch-Site": "same-origin"},
                        {"Sec-Fetch-Site": "none"},
                        {}):
            code, _ = self.raw("POST", "/api/view", headers, {"linkOpacity": 0.24})
            self.assertEqual(code, 200, headers)

    def test_static_serving_refuses_every_spelling_of_traversal(self):
        for path in ("/../aibrain/config.py",
                     "/%2e%2e/aibrain/config.py",
                     "/..%2faibrain%2fconfig.py",
                     "/subdir/../../aibrain/config.py",
                     "/../.env"):
            code, _ = self.raw("GET", path)
            self.assertEqual(code, 404, path)
        self.assertEqual(self.raw("GET", "/app.js")[0], 200)

    def test_a_body_larger_than_the_cap_is_refused(self):
        """Refused on the header, before a byte of it is read into memory."""
        host, port = self.httpd.server_address[0], self.httpd.server_address[1]
        request = (
            "POST /api/todos HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Content-Type: application/json\r\n"
            f"Content-Length: {4 << 20}\r\n"
            "\r\n"
        ).encode()
        with socket.create_connection((host, port), timeout=10) as sock:
            sock.sendall(request)
            status = sock.recv(64).decode("utf-8", "replace")
        self.assertIn("400", status.splitlines()[0])

    def test_a_query_parameter_that_is_not_a_number_is_a_400(self):
        for path in ("/api/search?q=a&limit=abc", "/api/recent?limit=nope",
                     "/api/note/not-a-number", "/api/node/x"):
            code, body = self.raw("GET", path)
            self.assertEqual(code, 400, path)
            self.assertIn("error", json.loads(body))
        # And a negative or enormous one is clamped rather than passed on.
        self.assertLessEqual(len(self.get("/api/recent?limit=99999")["results"]), 200)
        self.get("/api/search?q=protocols&limit=-5")

    def test_a_script_cannot_be_handed_arguments_by_the_caller(self):
        """Only the toggles the script declares reach its command line."""
        job = self.post("/api/script/echo/run",
                        {"args": ["--wipe", "/"], "options": ["Nope"]})["job"]
        self.assertNotIn("--wipe", job["command"])
        events = self.sse(f"/api/stream/job/{job['id']}")
        self.assertEqual(events[-1]["status"], "done")

    def test_an_agent_colour_that_is_not_a_colour_is_refused(self):
        """It lands in a `style="…"` the universe builds with innerHTML."""
        code, _ = self.raw("POST", "/api/agent/kernel", None,
                           {"color": '#000" onmouseover="alert(1)'})
        self.assertEqual(code, 400)
        self.assertEqual(
            self.raw("POST", "/api/agent/kernel", None, {"color": "#b48cff"})[0], 200)

    def test_an_internal_failure_does_not_hand_back_its_detail(self):
        state = self.state

        def boom(*_args, **_kwargs):
            raise RuntimeError("/Users/someone/Vaults/Private/secret.md is missing")

        state.corpus.recent = boom
        try:
            code, body = self.raw("GET", "/api/recent")
        finally:
            del state.corpus.recent
        self.assertEqual(code, 500)
        payload = json.loads(body)
        self.assertNotIn("secret.md", payload["error"])
        self.assertNotIn("RuntimeError", payload["error"])


class LiveUpdateTests(unittest.TestCase):
    """The change feed and the conditional universe.

    Both halves of the same promise: the browser is told the moment something
    changes, and asking again when nothing did costs a 304 rather than a
    whole corpus of geometry.
    """

    @classmethod
    def setUpClass(cls):
        cls.fixture = start_corpus()
        cls.addClassCleanup(cls.fixture.stop)
        cls.state = State(cls.fixture.cfg, cls.fixture.corpus)
        cls.addClassCleanup(cls.state.close)
        cls.state.universe(rebuild=True)
        handler = type("H", (Handler,),
                       {"state": cls.state, "router": build_router(cls.state)})
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        cls.addClassCleanup(cls.httpd.shutdown)
        cls.httpd.daemon_threads = True
        cls.base = f"http://127.0.0.1:{cls.httpd.server_address[1]}"
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()

    # ---- helpers ---------------------------------------------------------
    def universe(self, if_none_match: str | None = None, rebuild: bool = False):
        """`(status, etag, payload)` — payload is None on a 304."""
        path = "/api/universe" + ("?rebuild=1" if rebuild else "")
        request = urllib.request.Request(self.base + path)
        if if_none_match:
            request.add_header("If-None-Match", if_none_match)
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return (response.status, response.headers.get("ETag"),
                        json.loads(response.read().decode()))
        except urllib.error.HTTPError as exc:
            return exc.code, exc.headers.get("ETag"), None

    def open_stream(self):
        return urllib.request.urlopen(self.base + "/api/events", timeout=30)

    @staticmethod
    def read_event(stream, deadline: float = 30.0):
        """The next `data:` frame, decoded. None if the stream went quiet."""
        stop = time.time() + deadline
        while time.time() < stop:
            line = stream.readline()
            if not line:
                return None
            text = line.decode("utf-8", "replace")
            if text.startswith("data:"):
                return json.loads(text[5:])
        return None

    # ---- tests -----------------------------------------------------------
    def test_the_stream_opens_with_a_ping(self):
        # The ping is what proves the connection is open before anything has
        # happened — without it a browser cannot tell "quiet" from "broken".
        with self.open_stream() as stream:
            self.assertEqual(self.read_event(stream, 20), {"type": "ping"})

    def test_an_unchanged_universe_is_a_304(self):
        status, etag, payload = self.universe()
        self.assertEqual(status, 200)
        self.assertTrue(etag, "the universe must carry a tag")
        self.assertTrue(payload["brains"])

        status, again, payload = self.universe(if_none_match=etag)
        self.assertEqual(status, 304)
        self.assertIsNone(payload)
        self.assertEqual(again, etag)

    def test_status_reports_a_revision_per_brain(self):
        with urllib.request.urlopen(self.base + "/api/status", timeout=30) as r:
            status = json.loads(r.read().decode())
        brain = status["brains"][0]
        self.assertGreater(brain["revision"], 0, "ingest set it")
        _, _, universe = self.universe()
        self.assertEqual(universe["brains"][0]["revision"], brain["revision"])

    def test_a_tag_we_never_issued_is_not_honoured(self):
        status, _, payload = self.universe(if_none_match='"not-a-real-tag"')
        self.assertEqual(status, 200)
        self.assertIsNotNone(payload)

    def test_an_explicit_rebuild_ignores_the_conditional(self):
        _, etag, _ = self.universe()
        status, _, payload = self.universe(if_none_match=etag, rebuild=True)
        self.assertEqual(status, 200, "?rebuild=1 means send it anyway")
        self.assertIsNotNone(payload)

    def test_a_rescan_reaches_the_stream_and_moves_the_tag(self):
        _, etag, before = self.universe()
        revision_before = before["brains"][0]["revision"]

        with self.open_stream() as stream:
            self.assertEqual(self.read_event(stream, 20), {"type": "ping"})

            note = self.fixture.vault / "Agents.md"
            note.write_text(
                note.read_text(encoding="utf-8") + "\nAnd a new line.\n",
                encoding="utf-8")
            self.fixture.reindex()

            change = None
            for _ in range(8):
                event = self.read_event(stream, 20)
                if event is None:
                    break
                if event.get("type") != "ping":
                    change = event
                    break
            self.assertIsNotNone(change, "the edit should have been announced")
            self.assertEqual(change["brain_id"], self.fixture.brain_id)
            self.assertIn(change["kind"], ("note", "brain", "reindex"))

        status, fresh, after = self.universe(if_none_match=etag)
        self.assertEqual(status, 200, "the old tag is stale now")
        self.assertNotEqual(fresh, etag)
        self.assertGreater(after["brains"][0]["revision"], revision_before)

    def test_a_second_rescan_that_found_nothing_leaves_the_tag_alone(self):
        self.fixture.reindex()
        _, etag, _ = self.universe()
        self.fixture.reindex()
        status, again, _ = self.universe(if_none_match=etag)
        self.assertEqual(status, 304, "nothing changed, so nothing moved")
        self.assertEqual(again, etag)

    def test_moving_an_agent_moves_the_tag_too(self):
        # The agents are Python's own addition to the payload, so a tag that
        # only described Rust's half would hand the browser a stale one.
        _, etag, payload = self.universe()
        agent_id = payload["agents"][0]["id"]
        agent = self.state.cfg.agent(agent_id)
        agent.pos = [round(agent.pos[0] + 3.0, 3) if agent.pos else 3.0, 1.0, 2.0]
        _, moved, _ = self.universe(rebuild=True)
        self.assertNotEqual(moved, etag)


class CitationTests(unittest.TestCase):
    """A citation must mean the agent used the note. Nothing else earns a pill.

    The bug these lock down: `cites_from_text` used to top every answer up to
    six pills with our own search results, so an answer that cited one note
    looked identical to one that cited six, and none of them could be trusted.
    """

    @classmethod
    def setUpClass(cls):
        # A second vault holding a duplicate title, so the ambiguity case has
        # something to be ambiguous about.
        cls.fixture = start_corpus({"Other": {
            "Recipes/Ramen.md": "# Ramen\n\nA copy.\n"}})
        cls.addClassCleanup(cls.fixture.stop)
        cls.corpus = cls.fixture.corpus
        cls.brain_id = cls.fixture.brain_id
        cls.other_id = cls.fixture.extra_ids["Other"]

    def setUp(self):
        self.agent = LocalAgent(
            AgentConfig(id="a", name="A", kind="local", brains=[self.brain_id]),
            self.corpus, {self.brain_id: "Test", self.other_id: "Other"})
        self.supplied = self.corpus.search(
            "protocols", limit=6, brain_ids=[self.brain_id])
        self.assertTrue(self.supplied, "fixture must retrieve something")

    def note_id(self, rel_path: str, brain_id: str | None = None) -> int:
        row = self.corpus.note_by_path(brain_id or self.brain_id, rel_path)
        self.assertIsNotNone(row, rel_path)
        return row["nid"]

    def test_normalize_folds_separators(self):
        self.assertEqual(normalize("Agent-Context_Protocol"), "agent context protocol")
        self.assertEqual(normalize("Café"), "cafe")

    def test_an_answer_citing_nothing_produces_no_citations(self):
        cites = self.agent.cites_from_text("I could not find anything.", self.supplied)
        self.assertEqual(cites, [])

    def test_only_notes_the_agent_wrote_are_cited(self):
        cites = self.agent.cites_from_text(
            "See [[Protocols]] for the details.", self.supplied)
        self.assertEqual([c.title for c in cites], ["Protocols"])

    def test_citing_a_supplied_note_outranks_citing_one_from_nowhere(self):
        """The difference between 'it had the text' and 'it just said the name'.

        An agent repeating a wikilink it saw inside another note produces a
        title it never read. That is the citation worth doubting, so it must
        not look like one backed by a passage we actually handed over.
        """
        supplied = [h for h in self.supplied if h.title == "Protocols"]
        self.assertTrue(supplied)
        cites = self.agent.cites_from_text(
            "See [[Protocols]], which mentions [[Sourdough]].", supplied)
        by_title = {c.title: c.evidence for c in cites}
        self.assertEqual(by_title["Protocols"], "grounded")
        self.assertEqual(by_title["Sourdough"], "named")
        self.assertLess(EVIDENCE_ORDER["grounded"], EVIDENCE_ORDER["named"])

    def test_supplied_but_uncited_passages_are_kept_separate(self):
        cites = self.agent.cites_from_text("See [[Protocols]].", self.supplied)
        context = self.agent.context_citations(self.supplied, cites)
        cited_ids = {c.note_id for c in cites}
        self.assertTrue(all(c.note_id not in cited_ids for c in context))
        self.assertTrue(all(c.evidence == "context" for c in context))
        # Between them they account for everything, and nothing is double-counted.
        self.assertEqual(len(cited_ids | {c.note_id for c in context}),
                         len(cited_ids) + len(context))

    def test_an_observed_read_outranks_a_bare_mention(self):
        touched = {self.note_id("Protocols.md"): ("opened", "cat Protocols.md")}
        cites = self.agent.cites_from_text("See [[Protocols]].", self.supplied, touched)
        self.assertEqual(cites[0].evidence, "opened")
        self.assertIn("cat", cites[0].why)

    def test_a_file_read_but_never_named_still_counts(self):
        cites = self.agent.cites_from_text(
            "Nothing to report.", self.supplied,
            {self.note_id("Agents.md"): ("read", "served via fs/read_text_file")})
        self.assertEqual([c.title for c in cites], ["Agents"])
        self.assertEqual(cites[0].evidence, "read")

    def test_citations_are_ordered_by_how_much_we_can_prove(self):
        cites = self.agent.cites_from_text(
            "[[Index]] and [[Protocols]] and [[Agents]].", self.supplied,
            {self.note_id("Protocols.md"): ("read", "served")})
        self.assertEqual(cites[0].title, "Protocols")
        ranks = [EVIDENCE_ORDER[c.evidence] for c in cites]
        self.assertEqual(ranks, sorted(ranks))

    def test_a_title_in_two_vaults_is_flagged_rather_than_guessed(self):
        # This agent sees both vaults, so [[Ramen]] is genuinely ambiguous.
        agent = LocalAgent(
            AgentConfig(id="a", name="A", kind="local",
                        brains=[self.brain_id, self.other_id]),
            self.corpus, {self.brain_id: "Test", self.other_id: "Other"})
        cites = agent.cites_from_text("As in [[Ramen]].", [])
        self.assertEqual(len(cites), 1, "one pill, not one per copy")
        self.assertEqual(cites[0].ambiguous_with, 1)

    def test_each_local_hit_links_to_its_own_copy_of_a_shared_title(self):
        # Ramen lives in both vaults; a title-keyed link would send both
        # entries to whichever copy won.
        agent = LocalAgent(
            AgentConfig(id="a", name="A", kind="local",
                        brains=[self.brain_id, self.other_id]),
            self.corpus, {self.brain_id: "Test", self.other_id: "Other"})
        events = list(agent.ask("ramen", []))
        done = next(e for e in events if e.type == "cites")
        ramen = [c.note_id for c in done.cites if c.title == "Ramen"]
        self.assertEqual(len(ramen), 2, "fixture has Ramen in both vaults")
        html = done.data["html"]
        for nid in ramen:
            self.assertIn(f'data-note="{nid}"', html)

    def test_a_question_still_retrieves_when_a_word_is_absent(self):
        # "describe" appears in no note. Rust widens once server-side when the
        # strict pass is empty, and the word-filtered pass here widens again.
        self.assertTrue(self.agent.retrieve("describe the protocols for me"))


class BrainRetirementTests(unittest.TestCase):
    """Removing a vault retires its brain in aibrain-core at once.

    See design_concepts/RECOMMENDATION_ELASTICSEARCH.md. Elasticsearch is off
    in these tests; the index side is covered by the Rust suite.
    """

    @classmethod
    def setUpClass(cls):
        cls.fixture = start_corpus({"Doomed": {"Gone.md": "# Gone\n\nSoon.\n"}})
        cls.addClassCleanup(cls.fixture.stop)
        cls.doomed = cls.fixture.extra_ids["Doomed"]

    def brain_ids(self):
        return {b["id"] for b in self.fixture.corpus.status().get("brains", [])}

    def test_a_linked_vault_cannot_be_retired_then_an_unlinked_one_is(self):
        corpus = self.fixture.corpus
        self.assertIn(self.doomed, self.brain_ids())

        # Still on disk and in the config: refused, because the next rescan
        # would only put it back after re-embedding every note.
        with self.assertRaises(CorpusError) as caught:
            corpus.retire_brain(self.doomed)
        self.assertIn("409", str(caught.exception))
        self.assertIn(self.doomed, self.brain_ids())

        # What remove_brain does first: the link goes, the config forgets it.
        shutil.rmtree(Path(self.fixture.cfg.brain(self.doomed).path))
        self.fixture.cfg.brains = [b for b in self.fixture.cfg.brains
                                   if b.id != self.doomed]
        self.fixture.cfg.save()

        result = corpus.retire_brain(self.doomed)
        self.assertTrue(result["retired"])
        self.assertIsNone(result["index"], "no cluster, so no index to drop")
        self.assertNotIn(self.doomed, self.brain_ids())
        self.assertIn(self.fixture.brain_id, self.brain_ids(), "the other vault is untouched")
        # Idempotent.
        self.assertFalse(corpus.retire_brain(self.doomed)["retired"])


class StartupTests(unittest.TestCase):
    """No Rust service means no app, and the message has to say how to fix it."""

    def test_the_failure_message_names_both_commands(self):
        message = unreachable_message("http://127.0.0.1:8781")
        self.assertIn("docker compose up -d db", message)
        self.assertIn("cargo run --manifest-path rust/Cargo.toml -- serve --watch",
                      message)
        self.assertIn("http://127.0.0.1:8781", message)

    def test_an_unreachable_corpus_exits_non_zero_without_a_traceback(self):
        from aibrain import __main__ as entry

        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "config.json"
            env = {**os.environ,
                   "AIBRAIN_CORE_URL": f"http://127.0.0.1:{free_port()}",
                   # Config.load() reconciles brains against whatever real
                   # obsidian_vaults/ this checkout has, which — unlike the
                   # rest of this file — this subprocess never gets a chance
                   # to patch away. Left on, a run against a checkout with
                   # linked vaults would rewrite its real .claude/settings.local.json.
                   "AIBRAIN_MANAGE_DENY_RULES": "0"}
            result = subprocess.run(
                [sys.executable, "-m", "aibrain", "--config", str(config),
                 "--no-browser"],
                cwd=str(REPO_ROOT), env=env, capture_output=True, text=True,
                timeout=120,
            )
            self.assertEqual(result.returncode, 1)
            self.assertNotIn("Traceback", result.stderr)
            self.assertIn("docker compose up -d db", result.stderr)
        self.assertTrue(hasattr(entry, "main"))


# ---------------------------------------------------------------------------
# configuration
# ---------------------------------------------------------------------------

class DiscoveryTests(unittest.TestCase):
    """Only symlinked vaults count, and a broken link says so."""

    def test_only_linked_vaults_are_found(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            real, unlinked = root / "real", root / "unlinked"
            make_vault(real)
            make_vault(unlinked)
            links = root / "obsidian_vaults"
            links.mkdir()
            (links / "Real").symlink_to(real, target_is_directory=True)
            (links / "Broken").symlink_to(root / "nope", target_is_directory=True)

            import aibrain.config as config
            with patched(config, VAULT_LINK_DIR=links,
                         SETTINGS_PATH=root / ".claude" / "settings.json"):
                found = config.discover_vaults()
                self.assertEqual([p.name for p in found], ["Real"])
                problems = dict(config.link_problems())
                self.assertIn("Broken", problems)

                cfg = Config(brains=[BrainConfig(id="stale", name="Stale",
                                                 path=str(unlinked))])
                config.reconcile_brains(cfg)
                self.assertEqual([b.id for b in cfg.brains], ["real"])

    def test_settings_survive_an_unlink_and_relink(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            real = root / "Kept"
            make_vault(real)
            links = root / "obsidian_vaults"
            links.mkdir()
            (links / "Kept").symlink_to(real, target_is_directory=True)

            import aibrain.config as config
            with patched(config, VAULT_LINK_DIR=links,
                         SETTINGS_PATH=root / ".claude" / "settings.json"):
                cfg = Config()
                config.reconcile_brains(cfg)
                cfg.brains[0].enabled = False
                cfg.brains[0].seed = 4242

                (links / "Kept").unlink()
                config.reconcile_brains(cfg)
                self.assertEqual(cfg.brains, [])

                (links / "Kept").symlink_to(real, target_is_directory=True)
                config.reconcile_brains(cfg)
                self.assertEqual(len(cfg.brains), 1)
                # A relink restores the brain, not its settings, because the
                # record was dropped with it. Documented here so the behaviour
                # is a decision rather than a surprise.
                self.assertTrue(cfg.brains[0].enabled)

    def test_an_old_config_with_a_node_cap_still_loads(self):
        # `max_nodes` was removed with the cap; a config written before that
        # must not become unreadable.
        raw = {"brains": [{"id": "t", "name": "T", "path": "/tmp/nope",
                           "max_nodes": 2200}]}
        cfg = Config.from_dict(raw)
        self.assertEqual([b.id for b in cfg.brains], ["t"])
        self.assertFalse(hasattr(cfg.brains[0], "max_nodes"))


class A2AEndpointTests(unittest.TestCase):
    """The agent card is written by the remote agent, so it is not trusted."""

    def client(self, url="https://agent.example.com:8443/a2a"):
        from aibrain.agents.a2a import A2AClient
        return A2AClient(url, {"Authorization": "Bearer secret"})

    def test_no_card_means_the_configured_url(self):
        client = self.client()
        self.assertEqual(client.endpoint(), "https://agent.example.com:8443/a2a")

    def test_a_card_may_move_the_path(self):
        client = self.client()
        client.card = {"url": "https://agent.example.com:8443/other/path/"}
        self.assertEqual(client.endpoint(),
                         "https://agent.example.com:8443/other/path")

    def test_a_card_may_not_move_the_host(self):
        from aibrain.agents.a2a import A2AError
        for elsewhere in ("http://evil.example.net/a2a",
                          "https://agent.example.com:9999/a2a",
                          "http://agent.example.com:8443/a2a"):
            client = self.client()
            client.card = {"url": elsewhere}
            with self.assertRaises(A2AError, msg=elsewhere):
                client.endpoint()

    def test_fetch_card_tries_the_configured_url_before_well_known_paths(self):
        # Elastic Agent Builder publishes the card at a per-agent URL the
        # user pastes verbatim (/api/agent_builder/a2a/<id>.json), not under
        # /.well-known/. That literal URL must be tried first.
        client = self.client(url="https://kibana.example.com/api/agent_builder/a2a/my-agent.json")
        seen = []

        class FakeResponse:
            def __init__(self, body: bytes):
                self._body = body

            def read(self):
                return self._body

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

        def fake_open(url, data=None, accept="application/json"):
            seen.append(url)
            if url == client.url:
                return FakeResponse(json.dumps({"name": "kibana-agent"}).encode())
            raise urllib.error.URLError("not found")

        client._open = fake_open
        card = client.fetch_card()
        self.assertEqual(card["name"], "kibana-agent")
        self.assertEqual(seen[0], client.url)


class A2AGroundingTests(unittest.TestCase):
    """Whether the outgoing message carries Obsidian passages at all.

    Most A2A servers — Elastic Agent Builder among them — run their own
    retrieval over their own index; handing them our passages too just mixes
    in an unrelated corpus. `ground_with_context` gates that, off by default
    for new a2a connections.
    """

    class NoCallCorpus:
        def search(self, *args, **kwargs):
            raise AssertionError("must not retrieve local context")

        def render(self, text, resolved):
            return text

    def make_agent(self, ground_with_context: bool):
        from aibrain.agents.a2a import A2AAgent
        cfg = AgentConfig(id="a", name="A", kind="a2a",
                           url="https://agent.example.com/card.json",
                           ground_with_context=ground_with_context)
        agent = A2AAgent(cfg, self.NoCallCorpus(), {})
        agent.client.card = {"name": "Test agent"}  # skip fetch_card
        sent = {}

        def fake_stream(text, context_id=None):
            sent["text"] = text
            yield {"kind": "message", "role": "agent",
                   "parts": [{"kind": "text", "text": "ok"}]}

        agent.client.stream = fake_stream
        return agent, sent

    def test_grounding_off_sends_the_bare_question(self):
        agent, sent = self.make_agent(ground_with_context=False)
        list(agent.ask("what is the speed of an A-Wing starfighter?", []))
        self.assertEqual(sent["text"], "what is the speed of an A-Wing starfighter?")

    def test_grounding_on_retrieves_local_context_first(self):
        agent, _ = self.make_agent(ground_with_context=True)
        with self.assertRaises(AssertionError):
            list(agent.ask("what is the speed of an A-Wing starfighter?", []))


class A2AStreamSplitTests(unittest.TestCase):
    """Progress narration — reasoning, tool calls, "still working" — must
    never leak into the answer text as raw JSON; each becomes its own event.

    This is the bug behind "the streamed answer has an ugly thinking/tool
    section mixed into it": every text-bearing status-update used to be
    concatenated straight into the answer alongside the real message.
    """

    def test_a_reasoning_step_becomes_a_thought(self):
        from aibrain.agents.a2a import progress_events
        events = progress_events({"parts": [
            {"kind": "data", "data": {"type": "reasoning",
                                      "reasoning": "Checking the ship registry…"}},
        ]})
        self.assertEqual([(e.type, e.text) for e in events],
                         [("thought", "Checking the ship registry…")])

    def test_a_tool_call_and_its_result_share_one_id(self):
        from aibrain.agents.a2a import progress_events
        call = progress_events({"parts": [
            {"kind": "data", "data": {"type": "tool_call", "tool_id": "search_notes",
                                      "tool_call_id": "call-1", "params": {"query": "a-wing"}}},
        ]})
        result = progress_events({"parts": [
            {"kind": "data", "data": {"type": "tool_result", "tool_id": "search_notes",
                                      "tool_call_id": "call-1"}},
        ]})
        self.assertEqual(call[0].type, "tool")
        self.assertEqual(call[0].data["status"], "running")
        self.assertIn("search_notes", call[0].text)
        self.assertEqual(result[0].data, {"id": "call-1", "status": "completed"})
        self.assertEqual(call[0].data["id"], result[0].data["id"])

    def test_a_status_updates_plain_text_is_not_re_emitted(self):
        # Captured traffic: Agent Builder's status-update re-announces, as
        # plain text, exactly what the artifact stream already delivered —
        # so this is a duplicate, not a new event.
        from aibrain.agents.a2a import progress_events
        events = progress_events({"parts": [{"kind": "text", "text": "Searching the index…"}]})
        self.assertEqual(events, [])

    def test_full_stream_keeps_progress_out_of_the_answer(self):
        from aibrain.agents.a2a import A2AAgent
        cfg = AgentConfig(id="a", name="A", kind="a2a", url="https://agent.example.com/card.json")
        agent = A2AAgent(cfg, A2AGroundingTests.NoCallCorpus(), {})
        agent.client.card = {"name": "Test agent"}

        def fake_stream(text, context_id=None):
            yield {"kind": "status-update", "status": {"state": "working", "message": {
                "parts": [{"kind": "data", "data": {
                    "type": "reasoning", "reasoning": "Thinking about A-Wings"}}]}}}
            yield {"kind": "status-update", "status": {"state": "working", "message": {
                "parts": [{"kind": "data", "data": {
                    "type": "tool_call", "tool_id": "search_notes",
                    "tool_call_id": "c1", "params": {"query": "a-wing"}}}]}}}
            yield {"kind": "message", "role": "agent",
                   "parts": [{"kind": "text", "text": "An A-Wing does Mach 1,300."}]}

        agent.client.stream = fake_stream
        events = list(agent.ask("what is the speed of an A-Wing?", []))
        deltas = "".join(e.text for e in events if e.type == "delta")
        self.assertEqual(deltas, "An A-Wing does Mach 1,300.")
        self.assertTrue(any(e.type == "thought" for e in events))
        self.assertTrue(any(e.type == "tool" for e in events))

    def test_artifact_streams_are_grouped_by_id_and_the_last_open_one_wins(self):
        # Captured from the real "RPG Expert" (Elastic Agent Builder) agent:
        # every tool-use round's narration AND the eventual real synthesis
        # all stream as `artifact-update` chunks, indistinguishable in shape.
        # A fresh artifactId (or an explicit append:false) starts a new
        # block; true per-token deltas within one block never "look" like
        # they extend each other textually, which is exactly why grouping
        # has to go by id/append and not by string prefix-matching.
        from aibrain.agents.a2a import A2AAgent

        def artifact(aid, text, append):
            return {"kind": "artifact-update", "append": append, "lastChunk": False,
                    "artifact": {"artifactId": aid, "parts": [{"kind": "text", "text": text}]}}

        cfg = AgentConfig(id="a", name="A", kind="a2a", url="https://agent.example.com/card.json")
        agent = A2AAgent(cfg, A2AGroundingTests.NoCallCorpus(), {})
        agent.client.card = {"name": "Test agent"}

        def fake_stream(text, context_id=None):
            # First block: narration, several true (non-cumulative) deltas.
            yield artifact("art-1", "I'll", False)
            yield artifact("art-1", " start by", True)
            yield artifact("art-1", " searching the rules.", True)
            # Second block: the real answer, also many small true deltas.
            yield artifact("art-2", "An", False)
            yield artifact("art-2", " A-Wing", True)
            yield artifact("art-2", " does Mach 1,300.", True)
            # Both blocks' closing pings arrive bunched up at the very end,
            # well after each one's own text was already fully sent.
            yield artifact("art-1", "", True)
            yield artifact("art-2", "", True)

        agent.client.stream = fake_stream
        events = list(agent.ask("what is the speed of an A-Wing?", []))

        deltas = "".join(e.text for e in events if e.type == "delta")
        self.assertEqual(deltas, "An A-Wing does Mach 1,300.")

        thoughts = "".join(e.text for e in events if e.type == "thought" and e.data.get("key") == "a1")
        self.assertEqual(thoughts, "I'll start by searching the rules.")
        # The second block streamed live as a thought too — until it turned
        # out to be the answer, at which point it must be retracted so the
        # UI doesn't show it twice.
        self.assertTrue(any(e.type == "thought" and e.data.get("key") == "a2" for e in events))
        self.assertTrue(any(e.type == "retract" and e.data.get("key") == "a2" for e in events))

    def test_a_single_artifact_block_streams_live_as_it_grows(self):
        # No second block ever arrives — the one open block simply is the
        # answer, and its individual delta chunks should still have gone
        # out live as thought events with a stable key (for merging) before
        # being retracted and promoted at the end.
        from aibrain.agents.a2a import A2AAgent

        def artifact(text, append):
            return {"kind": "artifact-update", "append": append, "lastChunk": False,
                    "artifact": {"artifactId": "only", "parts": [{"kind": "text", "text": text}]}}

        cfg = AgentConfig(id="a", name="A", kind="a2a", url="https://agent.example.com/card.json")
        agent = A2AAgent(cfg, A2AGroundingTests.NoCallCorpus(), {})
        agent.client.card = {"name": "Test agent"}

        def fake_stream(text, context_id=None):
            yield artifact("Mach", False)
            yield artifact(" 1,300", True)
            yield artifact(".", True)

        agent.client.stream = fake_stream
        events = list(agent.ask("how fast?", []))
        thought_keys = {e.data.get("key") for e in events if e.type == "thought"}
        self.assertEqual(thought_keys, {"a1"})
        self.assertTrue(any(e.type == "retract" and e.data.get("key") == "a1" for e in events))
        deltas = "".join(e.text for e in events if e.type == "delta")
        self.assertEqual(deltas, "Mach 1,300.")


class AcpGroundingTests(unittest.TestCase):
    """The clean question goes out; a search tool is offered instead, not a
    prompt we pre-stuffed with our own keyword search on every turn."""

    class NoCallCorpus:
        def search(self, *args, **kwargs):
            raise AssertionError("must not retrieve local context")

        def render(self, text, resolved):
            return text

    class FakeConnection:
        def __init__(self):
            self.opened: dict = {}
            self.sent: str | None = None

        def prompt(self, text):
            self.sent = text
            yield {"update": {"sessionUpdate": "agent_message_chunk",
                              "content": {"type": "text", "text": "ok"}}}

    def make_agent(self, ground_with_context: bool):
        from aibrain.agents.acp import ACPAgent
        cfg = AgentConfig(id="a", name="A", kind="acp", command=["true"],
                           ground_with_context=ground_with_context)
        agent = ACPAgent(cfg, self.NoCallCorpus(), {})
        conn = self.FakeConnection()
        agent._ensure = lambda: conn
        return agent, conn

    def test_grounding_off_sends_the_bare_question(self):
        agent, conn = self.make_agent(ground_with_context=False)
        list(agent.ask("what is the speed of an A-Wing starfighter?", []))
        self.assertEqual(conn.sent, "what is the speed of an A-Wing starfighter?")

    def test_grounding_on_retrieves_local_context_first(self):
        agent, _ = self.make_agent(ground_with_context=True)
        with self.assertRaises(AssertionError):
            list(agent.ask("what is the speed of an A-Wing starfighter?", []))


class LinkNameTests(unittest.TestCase):
    """`add_brain` turns a supplied name into a file inside obsidian_vaults/."""

    def test_a_plain_name_is_kept(self):
        for name in ("Grognard", "Obsidian Cloud Home", "notes.v2", "a-b_c"):
            self.assertEqual(link_name(name), name)
        self.assertEqual(link_name("  Spaced  "), "Spaced")

    def test_a_name_that_is_really_a_path_is_refused(self):
        for name in ("../../.ssh/authorized_keys", "a/b", "a\\b", "..", ".",
                     "", "   ", ".hidden", "with\x00null", "line\nbreak",
                     "x" * 200):
            self.assertIsNone(link_name(name), name)


class DenyRuleTests(unittest.TestCase):
    """The deny list is generated, so it cannot go stale when a vault moves."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.links = self.root / "obsidian_vaults"
        self.links.mkdir()
        self.settings = self.root / ".claude" / "settings.json"
        self.settings.parent.mkdir()

    def tearDown(self):
        self._tmp.cleanup()

    def link(self, name: str) -> Path:
        real = self.root / "vaults" / name
        real.mkdir(parents=True)
        (self.links / name).symlink_to(real, target_is_directory=True)
        return real.resolve()

    def write(self, payload: dict) -> None:
        self.settings.write_text(json.dumps(payload, indent=2), encoding="utf-8")

    def read(self) -> dict:
        return json.loads(self.settings.read_text(encoding="utf-8"))

    def test_every_linked_vault_gets_a_rule_and_no_rule_is_ever_dropped(self):
        import aibrain.config as config
        first, second = self.link("Alpha"), self.link("Beta")
        self.write({"permissions": {"deny": [
            "Read(//Users/nobody/GoneVault/**)"]}})

        with patched(config, VAULT_LINK_DIR=self.links, SETTINGS_PATH=self.settings):
            config.reconcile_brains(Config())
            deny = self.read()["permissions"]["deny"]
            self.assertIn(f"Read(//{str(first).lstrip('/')}/**)", deny)
            self.assertIn(f"Read(//{str(second).lstrip('/')}/**)", deny)
            # A rule nothing here generated is somebody's deliberate denial;
            # regenerating the list must not quietly delete it.
            self.assertIn("Read(//Users/nobody/GoneVault/**)", deny)
            # The fixed non-vault entries survive.
            self.assertTrue(any(r.endswith("/.ssh/**)") for r in deny))
            self.assertTrue(any("Keychains" in r for r in deny))

            # Unlinking a vault stops it being a brain, but the vault is still
            # on disk and an agent still runs with the repo as its cwd, so the
            # denial stays until the user takes it out themselves.
            (self.links / "Beta").unlink()
            config.reconcile_brains(Config())
            deny = self.read()["permissions"]["deny"]
            self.assertIn(f"Read(//{str(first).lstrip('/')}/**)", deny)
            self.assertIn(f"Read(//{str(second).lstrip('/')}/**)", deny)
            self.assertEqual(len(deny), len(set(deny)), "no rule is duplicated")

    def test_the_rewrite_can_be_switched_off(self):
        """Loading a config should not be able to edit a shared file blind."""
        import aibrain.config as config
        self.link("Zeta")
        original = {"permissions": {"deny": []}}
        self.write(original)
        with patched(config, VAULT_LINK_DIR=self.links, SETTINGS_PATH=self.settings):
            prior = os.environ.get("AIBRAIN_MANAGE_DENY_RULES")
            os.environ["AIBRAIN_MANAGE_DENY_RULES"] = "0"
            try:
                self.assertFalse(config.write_deny_rules(config.discover_vaults()))
            finally:
                if prior is None:
                    del os.environ["AIBRAIN_MANAGE_DENY_RULES"]
                else:
                    os.environ["AIBRAIN_MANAGE_DENY_RULES"] = prior
        self.assertEqual(self.read(), original)

    def test_a_symlink_is_resolved_to_the_real_directory(self):
        import aibrain.config as config
        real = self.link("Gamma")
        with patched(config, VAULT_LINK_DIR=self.links, SETTINGS_PATH=self.settings):
            config.reconcile_brains(Config())
        deny = self.read()["permissions"]["deny"]
        # The rule names what the link points at, not the link itself.
        self.assertIn(f"Read(//{str(real).lstrip('/')}/**)", deny)
        self.assertNotIn(f"Read(//{str(self.links / 'Gamma').lstrip('/')}/**)", deny)

    def test_everything_else_in_the_file_is_preserved(self):
        import aibrain.config as config
        self.link("Delta")
        self.write({
            "model": "opus",
            "permissions": {"allow": ["Bash(ls:*)"],
                            "deny": ["Bash(rm:*)", "Read(//stale/**)"]},
        })
        with patched(config, VAULT_LINK_DIR=self.links, SETTINGS_PATH=self.settings):
            config.reconcile_brains(Config())
        raw = self.read()
        self.assertEqual(raw["model"], "opus")
        self.assertEqual(raw["permissions"]["allow"], ["Bash(ls:*)"])
        self.assertIn("Bash(rm:*)", raw["permissions"]["deny"])
        # Including a Read rule somebody wrote by hand.
        self.assertIn("Read(//stale/**)", raw["permissions"]["deny"])

    def test_a_checkout_with_no_link_folder_is_left_alone(self):
        """A fresh clone or a worktree must not strip the committed rules.

        No `obsidian_vaults/` at all is not the same as an empty one: it means
        we have no idea which vaults the user has, so rewriting a shared file
        to drop every vault rule would remove protection rather than refresh it.
        """
        import aibrain.config as config
        original = {"permissions": {"deny": [
            "Read(//Users/somebody/Vaults/Private/**)"]}}
        self.write(original)
        with patched(config, VAULT_LINK_DIR=self.root / "not-there",
                     SETTINGS_PATH=self.settings):
            self.assertFalse(config.write_deny_rules([]))
        self.assertEqual(self.read(), original)

    def test_writing_is_atomic_and_leaves_no_temp_file(self):
        import aibrain.config as config
        self.link("Epsilon")
        with patched(config, VAULT_LINK_DIR=self.links, SETTINGS_PATH=self.settings):
            self.assertTrue(config.write_deny_rules(config.discover_vaults()))
            # A second call with the same vaults changes nothing.
            self.assertFalse(config.write_deny_rules(config.discover_vaults()))
        self.assertEqual(
            [p.name for p in self.settings.parent.iterdir()], ["settings.json"])


class patched:
    """Swap module attributes for the duration of a block, then put them back."""

    def __init__(self, module, **values):
        self.module = module
        self.values = values
        self.prior: dict = {}

    def __enter__(self):
        for key, value in self.values.items():
            self.prior[key] = getattr(self.module, key)
            setattr(self.module, key, value)
        return self.module

    def __exit__(self, *exc):
        for key, value in self.prior.items():
            setattr(self.module, key, value)
        return False


class JobTests(unittest.TestCase):
    def test_failed_command_is_reported_not_raised(self):
        runner = JobRunner()
        job = runner.run_command("fail", [sys.executable, "-c", "raise SystemExit(3)"])
        for _ in range(200):
            if job.status != "running":
                break
            time.sleep(0.05)
        self.assertEqual(job.status, "failed")
        self.assertEqual(job.exit_code, 3)

    def test_missing_binary_does_not_crash_the_runner(self):
        runner = JobRunner()
        job = runner.run_command("missing", ["definitely-not-a-real-binary-xyz"])
        for _ in range(100):
            if job.status != "running":
                break
            time.sleep(0.05)
        self.assertEqual(job.status, "failed")
        self.assertTrue(any("failed to start" in line for line in job.lines))

    def test_a_reindex_task_reports_what_rust_did(self):
        """The job's output lines are the stats, not a Python scan log."""
        class FakeCorpus:
            base_url = "http://example.invalid"

            def reindex(self, force=False):
                return {"scanned": 6, "added": 1, "updated": 2, "removed": 0,
                        "unchanged": 3, "links": 7}

            def universe(self):
                return {"noteIds": [], "brains": [], "stats": {}}

            def universe_with_etag(self, if_none_match=None):
                return self.universe(), '"fake"'

        state = State(Config(), FakeCorpus())
        lines: list[str] = []
        state.reindex(lines.append)
        joined = " ".join(lines)
        self.assertIn("6 notes scanned", joined)
        self.assertIn("+1 added", joined)
        self.assertIn("7 links resolved", joined)


# ---------------------------------------------------------------------------
# the day's list
# ---------------------------------------------------------------------------

class TodoTests(unittest.TestCase):
    """The /api/todos proxies, driven the way the shelf drives them.

    The service is started with AIBRAIN_TODO_TEST_CLOCK=1 by the fixture, so
    these can say what day it is instead of waiting for tomorrow.
    """

    # Far enough out that nothing else in the shared test database is here.
    MONDAY = "2031-05-05"
    TUESDAY = "2031-05-06"

    @classmethod
    def setUpClass(cls):
        cls.fixture = start_corpus()
        cls.addClassCleanup(cls.fixture.stop)
        cls.state = State(cls.fixture.cfg, cls.fixture.corpus)
        cls.addClassCleanup(cls.state.close)
        handler = type("H", (Handler,),
                       {"state": cls.state, "router": build_router(cls.state)})
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        cls.addClassCleanup(cls.httpd.shutdown)
        cls.httpd.daemon_threads = True
        cls.base = f"http://127.0.0.1:{cls.httpd.server_address[1]}"
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()
        # Nothing is deleted, by design — there is no route that would. Every
        # row these tests write carries a unique marker instead, so a shared
        # test database stays usable.

    def setUp(self):
        # Per test, not per class: the list is one shared thing, so a test
        # that counted the class's rows would count its siblings' too.
        self.marker = f"zzpy{uuid.uuid4().hex[:8]}"

    # ---- helpers ---------------------------------------------------------
    def call(self, path, body=None, method=None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data,
                                     method=method or ("POST" if data else "GET"))
        req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read().decode())

    def add(self, text, due=None, now=None):
        query = f"?now={now}" if now else ""
        body = {"body": f"{text} {self.marker}"}
        if due:
            body["due_on"] = due
        return self.call(f"/api/todos{query}", body)["todo"]

    def listed(self, now):
        return self.call(f"/api/todos?now={now}")

    def mine(self, payload):
        return [t for t in payload["todos"] if self.marker in t["body"]]

    # ---- tests -----------------------------------------------------------
    def test_one_list_where_a_finished_item_lingers_for_the_rest_of_its_day(self):
        plants = self.add("water the plants", now=f"{self.MONDAY}T09:00:00")
        letter = self.add("post the letter", now=f"{self.MONDAY}T09:00:00")
        self.assertIsNone(plants["due_on"], "no date unless one is asked for")

        listed = self.listed(f"{self.MONDAY}T09:00:00")
        self.assertEqual(listed["today"], self.MONDAY)
        self.assertNotIn("day", listed, "there is no day being viewed")
        rows = self.mine(listed)
        # sort_order, not insertion order by accident.
        self.assertEqual([t["id"] for t in rows], [plants["id"], letter["id"]])
        self.assertTrue(all(t["state"] == "open" for t in rows))

        done = self.call(f"/api/todos/{letter['id']}/complete"
                         f"?now={self.TUESDAY}T01:00:00", {})
        self.assertEqual(done["todo"]["state"], "completed")

        # One in the morning is still Monday, so it is still shown, struck.
        states = {t["id"]: t["state"] for t in self.mine(self.listed(f"{self.TUESDAY}T01:30:00"))}
        self.assertEqual(states, {plants["id"]: "open", letter["id"]: "completed"})

        # Tuesday proper: the finished one is gone, the open one is untouched.
        rows = self.mine(self.listed(f"{self.TUESDAY}T09:00:00"))
        self.assertEqual([t["id"] for t in rows], [plants["id"]])
        self.assertIsNone(rows[0]["due_on"], "nothing rolled it onto a day")

    def test_history_keeps_what_the_list_no_longer_shows(self):
        todo = self.add("buy stamps")
        self.call(f"/api/todos/{todo['id']}/complete?now={self.MONDAY}T10:00:00", {})
        found = self.call(f"/api/todos/history?q=stamps+{self.marker}")
        rows = [t for t in found["todos"] if t["id"] == todo["id"]]
        self.assertEqual(len(rows), 1, "the completed item is still searchable")
        kinds = [e["kind"] for e in rows[0]["events"]]
        self.assertIn("created", kinds)
        self.assertIn("completed", kinds)

    def test_a_note_can_be_linked_and_resolves_to_its_id(self):
        todo = self.add("reread the protocols note")
        linked = self.call(f"/api/todos/{todo['id']}/link",
                           {"brain_id": self.fixture.brain_id,
                            "rel_path": "Protocols.md"})
        refs = linked["todo"]["refs"]
        self.assertEqual(len(refs), 1)
        self.assertEqual(refs[0]["rel_path"], "Protocols.md")
        self.assertIsNotNone(refs[0]["note_id"], "the note exists, so it resolves")

    def test_a_due_date_is_set_moved_cleared_and_recorded(self):
        todo = self.add("call the plumber", due="tomorrow", now=f"{self.MONDAY}T09:00:00")
        self.assertEqual(todo["due_on"], self.TUESDAY)

        # 01:00 on Tuesday is still Monday, so "tomorrow" is still Tuesday.
        moved = self.call(f"/api/todos/{todo['id']}/due?now={self.TUESDAY}T01:00:00",
                          {"due_on": "tomorrow"})
        self.assertEqual(moved["todo"]["due_on"], self.TUESDAY)
        moved = self.call(f"/api/todos/{todo['id']}/due?now={self.TUESDAY}T09:00:00",
                          {"due_on": "2031-06-01"})
        self.assertEqual(moved["todo"]["due_on"], "2031-06-01")
        cleared = self.call(f"/api/todos/{todo['id']}/due?now={self.TUESDAY}T09:30:00",
                            {"due_on": None})
        self.assertIsNone(cleared["todo"]["due_on"])

        # Refused by the service; the proxy relays its reason, not a traceback.
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.call(f"/api/todos/{todo['id']}/due", {"due_on": "someday"})
        self.assertIn("a due date must be", caught.exception.read().decode())

        edited = self.call(f"/api/todos/{todo['id']}?now={self.TUESDAY}T10:00:00",
                           {"body": f"call the roofer {self.marker}"},
                           method="PATCH")
        self.assertIn("roofer", edited["todo"]["body"])

        found = self.call(f"/api/todos/history?q=roofer+{self.marker}")
        rows = [t for t in found["todos"] if t["id"] == todo["id"]]
        events = rows[0]["events"]
        self.assertEqual([e["kind"] for e in events],
                         ["created", "rescheduled", "rescheduled", "edited"],
                         "setting the same date again logs nothing")
        self.assertEqual(events[0]["to_day"], self.TUESDAY)
        self.assertEqual((events[2]["from_day"], events[2]["to_day"]), ("2031-06-01", None))

    def test_an_empty_body_is_refused_and_a_missing_item_is_a_404(self):
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.call("/api/todos", {"body": "   "})
        self.assertEqual(caught.exception.code, 400)
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.call("/api/todos/999999999/complete", {})
        self.assertEqual(caught.exception.code, 404)

    def test_the_configured_start_hour_reaches_the_browser(self):
        status = self.get_status()
        self.assertEqual(status["todo"]["day_start_hour"], 4)

    def get_status(self):
        with urllib.request.urlopen(self.base + "/api/status", timeout=30) as r:
            return json.loads(r.read().decode())


# ---------------------------------------------------------------------------
# the MCP server
# ---------------------------------------------------------------------------

class McpTodoTests(unittest.TestCase):
    """Drives `python3 -m aibrain.mcp_todo` over a pipe, as an ACP client does.

    Nothing is mocked: the server talks HTTP to the same aibrain-core the
    shelf does, which is the property worth testing — an agent and the browser
    must be looking at one list.
    """

    @classmethod
    def setUpClass(cls):
        cls.fixture = start_corpus()
        cls.addClassCleanup(cls.fixture.stop)
        cls.marker = f"zzmcp{uuid.uuid4().hex[:8]}"
        cls.proc = subprocess.Popen(
            [sys.executable, "-m", "aibrain.mcp_todo"],
            cwd=str(REPO_ROOT),
            env={**os.environ, "AIBRAIN_CORE_URL": cls.fixture.base_url},
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        cls.addClassCleanup(cls._stop_proc)
        cls._id = 0

    @classmethod
    def _stop_proc(cls):
        if cls.proc.poll() is None:
            cls.proc.stdin.close()
            try:
                cls.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                cls.proc.kill()

    def rpc(self, method, params=None):
        type(self)._id += 1
        message = {"jsonrpc": "2.0", "id": self._id, "method": method}
        if params is not None:
            message["params"] = params
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        self.assertTrue(line, "the MCP server closed its stdout")
        return json.loads(line)

    def notify(self, method, params=None):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def text_of(self, reply):
        self.assertNotIn("error", reply, f"unexpected JSON-RPC error: {reply}")
        result = reply["result"]
        self.assertFalse(result.get("isError"), result)
        return "\n".join(part["text"] for part in result["content"])

    def test_the_handshake_then_a_round_trip_through_the_list(self):
        hello = self.rpc("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"},
        })
        self.assertIn("protocolVersion", hello["result"])
        self.assertEqual(hello["result"]["serverInfo"]["name"], "aibrain-todo")
        self.assertIn("tools", hello["result"]["capabilities"])

        # A notification is never answered, so the next read must be the ping.
        self.notify("notifications/initialized")
        self.assertEqual(self.rpc("ping")["result"], {})

        listed = self.rpc("tools/list")["result"]["tools"]
        names = {tool["name"] for tool in listed}
        self.assertEqual(names, {
            "list_todos", "add_todo", "complete_todo", "set_due_date",
            "link_todo_to_note", "search_history",
        })
        for tool in listed:
            self.assertIn("inputSchema", tool)
            self.assertTrue(tool["description"])

        body = f"file the {self.marker} receipts"
        added = self.text_of(self.rpc("tools/call", {
            "name": "add_todo", "arguments": {"body": body, "due": "2031-07-07"},
        }))
        self.assertIn(self.marker, added)
        self.assertIn("due 2031-07-07", added)
        todo_id = int(added.split("#")[1].split()[0])

        shown = self.text_of(self.rpc("tools/call", {
            "name": "list_todos", "arguments": {},
        }))
        self.assertIn(self.marker, shown)
        self.assertIn("[ ]", shown)

        # And the browser's own route sees the very same row.
        listed = self.fixture.corpus.todos()
        self.assertIn(todo_id, [t["id"] for t in listed["todos"]])

        cleared = self.text_of(self.rpc("tools/call", {
            "name": "set_due_date", "arguments": {"id": todo_id, "due": ""},
        }))
        self.assertNotIn("due ", cleared)

        done = self.text_of(self.rpc("tools/call", {
            "name": "complete_todo", "arguments": {"id": todo_id},
        }))
        self.assertIn("[x]", done)

        history = self.text_of(self.rpc("tools/call", {
            "name": "search_history", "arguments": {"q": self.marker},
        }))
        self.assertIn(self.marker, history)
        self.assertIn("created", history)
        self.assertIn("completed", history)

    def test_bad_input_comes_back_as_a_result_not_a_broken_pipe(self):
        reply = self.rpc("tools/call", {"name": "no_such_tool", "arguments": {}})
        self.assertIn("error", reply)
        self.assertEqual(reply["error"]["code"], -32602)

        reply = self.rpc("tools/call", {"name": "complete_todo", "arguments": {}})
        self.assertTrue(reply["result"]["isError"], "a missing id is a tool error")

        self.assertIn("error", self.rpc("no/such/method"))
        # Still alive after all of that.
        self.assertEqual(self.rpc("ping")["result"], {})


class McpProtocolTests(unittest.TestCase):
    """The parts of the protocol that need no service behind them."""

    def setUp(self):
        from aibrain.mcp_todo import Server
        self.server = Server(corpus=_NoCorpus())

    def test_a_notification_is_never_answered(self):
        self.assertIsNone(self.server.handle(
            {"jsonrpc": "2.0", "method": "notifications/initialized"}))
        self.assertTrue(self.server.initialized)

    def test_a_message_that_is_not_jsonrpc_two_is_refused(self):
        reply = self.server.handle({"id": 1, "method": "ping"})
        self.assertEqual(reply["error"]["code"], -32600)

    def test_garbage_on_the_wire_does_not_stop_the_loop(self):
        import io
        out = io.StringIO()
        self.server.run(iter(["{not json", "", '{"jsonrpc":"2.0","id":9,"method":"ping"}']),
                        out)
        replies = [json.loads(line) for line in out.getvalue().splitlines()]
        self.assertEqual(replies[0]["error"]["code"], -32700)
        self.assertEqual(replies[1]["id"], 9)


class McpSearchTests(unittest.TestCase):
    """`search_notes`: what an ACP agent gets back when it decides to look
    something up, instead of us handing it passages unasked."""

    class FakeSearchCorpus:
        def __init__(self, hits: list[Hit]):
            self.hits = hits
            self.seen_brain_ids: list[str] | None = "not called"
            self.seen_limit: int | None = None

        def search(self, query, limit=60, brain_ids=None, any_terms=False):
            self.seen_brain_ids = brain_ids
            self.seen_limit = limit
            return self.hits[:limit]

        def status(self):
            return {"brains": [{"id": "grognard", "name": "Grognard"}]}

    def test_a_match_lists_title_brain_path_and_a_snippet(self):
        from aibrain.mcp_search import SearchTools
        hits = [Hit(note_id=1, title="Interceptor", brain_id="grognard",
                    source="grognard", rel_path="wiki/is/wiki/Interceptor.md",
                    snippet="takes a beating and gives one back")]
        tools = SearchTools(self.FakeSearchCorpus(hits))
        text = tools.call("search_notes", {"query": "interceptor"})
        self.assertIn("[[Interceptor]]", text)
        self.assertIn("Grognard", text)
        self.assertIn("wiki/is/wiki/Interceptor.md", text)
        self.assertIn("takes a beating", text)

    def test_no_matches_says_so_plainly(self):
        from aibrain.mcp_search import SearchTools
        tools = SearchTools(self.FakeSearchCorpus([]))
        text = tools.call("search_notes", {"query": "nothing like this exists"})
        self.assertIn("No notes match", text)

    def test_brain_scoping_reaches_the_search_call(self):
        from aibrain.mcp_search import SearchTools
        corpus = self.FakeSearchCorpus([])
        SearchTools(corpus, brain_ids=["grognard"]).call(
            "search_notes", {"query": "anything"})
        self.assertEqual(corpus.seen_brain_ids, ["grognard"])

    def test_the_limit_is_clamped_not_trusted(self):
        from aibrain.mcp_search import MAX_LIMIT, SearchTools
        corpus = self.FakeSearchCorpus([])
        SearchTools(corpus).call("search_notes", {"query": "n", "limit": 9999})
        self.assertEqual(corpus.seen_limit, MAX_LIMIT)

    def test_an_empty_query_is_not_sent_to_the_corpus(self):
        from aibrain.mcp_search import SearchTools

        class RefusingCorpus:
            def search(self, *a, **k):
                raise AssertionError("must not search for an empty query")

        text = SearchTools(RefusingCorpus()).call("search_notes", {"query": "  "})
        self.assertIn("Nothing to search for", text)


class AcpRegistrationTests(unittest.TestCase):
    """Both MCP servers have to reach the agent through `session/new`."""

    def connection(self, brains=()):
        from aibrain.agents.acp import ACPConnection
        return ACPConnection(["true"], "/tmp", {},
                             core_url="http://127.0.0.1:8781", brains=list(brains))

    def servers_by_name(self, brains=()):
        return {s["name"]: s for s in self.connection(brains).mcp_servers()}

    def test_the_todo_and_search_servers_are_offered_as_stdio_servers(self):
        servers = self.servers_by_name()
        self.assertEqual(set(servers), {"aibrain-todo", "aibrain-search"})
        for name, args in (("aibrain-todo", ["-m", "aibrain.mcp_todo"]),
                           ("aibrain-search", ["-m", "aibrain.mcp_search"])):
            self.assertEqual(servers[name]["type"], "stdio")
            self.assertEqual(servers[name]["args"], args)
            self.assertTrue(servers[name]["command"])

    def test_the_child_is_told_where_the_corpus_and_the_package_are(self):
        for server in self.servers_by_name().values():
            env = {e["name"]: e["value"] for e in server["env"]}
            self.assertEqual(env["AIBRAIN_CORE_URL"], "http://127.0.0.1:8781")
            # The agent starts the child in the user's project, so the import
            # path has to be spelled out or `-m aibrain.mcp_*` finds nothing.
            module = server["args"][1].rsplit(".", 1)[1]
            self.assertTrue((Path(env["PYTHONPATH"]) / "aibrain" / f"{module}.py").is_file())

    def test_the_search_server_is_scoped_to_the_agents_own_brains(self):
        servers = self.servers_by_name()
        env = {e["name"]: e["value"] for e in servers["aibrain-search"]["env"]}
        self.assertNotIn("AIBRAIN_SEARCH_BRAINS", env)

        scoped = self.servers_by_name(brains=["alpha", "beta"])
        env = {e["name"]: e["value"] for e in scoped["aibrain-search"]["env"]}
        self.assertEqual(env["AIBRAIN_SEARCH_BRAINS"], "alpha,beta")
        # The to-do server sees the same corpus regardless; brain scoping is
        # a search-tool concern, not a to-do-list one.
        self.assertNotIn("AIBRAIN_SEARCH_BRAINS",
                         {e["name"] for e in scoped["aibrain-todo"]["env"]})


class AcpPermissionTests(unittest.TestCase):
    """Claude Code edits files itself rather than asking us to; the only
    place we can refuse one outside the session directory or a vault is the
    permission request it sends first — see `ACPConnection._permission`."""

    def connection(self, roots=()):
        from aibrain.agents.acp import ACPConnection
        return ACPConnection(["true"], "/tmp", {}, roots=roots,
                             core_url="http://127.0.0.1:8781")

    @staticmethod
    def _request(path=None, kind="allow_once"):
        tool_call = {"title": "a tool call"}
        if path is not None:
            tool_call["locations"] = [{"path": path}]
        return {
            "toolCall": tool_call,
            "options": [{"kind": kind, "optionId": "opt1", "name": "Allow"}],
        }

    def test_a_location_inside_the_session_directory_is_approved(self):
        outcome = self.connection()._permission(self._request("/tmp/note.md"))
        self.assertEqual(outcome, {"outcome": "selected", "optionId": "opt1"})

    def test_a_location_inside_a_vault_is_approved(self):
        conn = self.connection(roots=["/tmp/vault"])
        outcome = conn._permission(self._request("/tmp/vault/note.md"))
        self.assertEqual(outcome, {"outcome": "selected", "optionId": "opt1"})

    def test_a_location_outside_every_root_is_refused(self):
        outcome = self.connection()._permission(self._request("/etc/passwd"))
        self.assertEqual(outcome, {"outcome": "cancelled"})

    def test_one_bad_location_among_several_refuses_the_whole_call(self):
        conn = self.connection()
        request = self._request()
        request["toolCall"]["locations"] = [
            {"path": "/tmp/fine.md"}, {"path": "/etc/passwd"},
        ]
        self.assertEqual(conn._permission(request), {"outcome": "cancelled"})

    def test_a_tool_call_with_no_declared_locations_falls_back_to_allow_once(self):
        # Most shell commands report no locations at all; that is the gap
        # this check narrows rather than closes, not a case to refuse.
        outcome = self.connection()._permission(self._request(None))
        self.assertEqual(outcome, {"outcome": "selected", "optionId": "opt1"})

    def test_no_allow_option_offered_is_still_cancelled(self):
        outcome = self.connection()._permission(
            self._request("/tmp/note.md", kind="reject_once"))
        self.assertEqual(outcome, {"outcome": "cancelled"})


class _NoCorpus:
    """Stands in for the HTTP client in tests that never reach the service."""

    base_url = "http://example.invalid"


# ---------------------------------------------------------------------------
# the renderer's one piece of testable logic
# ---------------------------------------------------------------------------

class EdgeShadingTests(unittest.TestCase):
    """Edge brightness is a shader attribute now; what feeds it is JS.

    `web/edges.js` is plain ES modules with no DOM and no three.js, so node
    can run its assertions. There is no JS test runner in the repo, so this
    shells out and skips when node is absent.
    """

    def test_edge_brightness_follows_the_highlight_set(self):
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is not installed, so web/edges.js has no runner")
        script = REPO_ROOT / "tests" / "test_edges.mjs"
        result = subprocess.run([node, str(script)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
