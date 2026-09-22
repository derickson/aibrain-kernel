"""Smoke tests for the AI Brain kernel.

Standard library only, like the rest of the repo — run them with:

    python3 tests/test_kernel.py

They build a small vault in a temp directory, index it, and drive the HTTP API
the way the browser does, so a passing run means the whole stack is wired up:
scanning, link resolution, search, markdown, the universe payload, the local
agent's SSE stream, and the job runner.
"""

from __future__ import annotations

import json
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
from http.server import ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from aibrain import graph, md                                      # noqa: E402
from aibrain.agents import LocalAgent                               # noqa: E402
from aibrain.agents.base import EVIDENCE_ORDER                      # noqa: E402
from aibrain.config import (AgentConfig, BrainConfig, Config,       # noqa: E402
                            ScriptConfig, default_agents)
from aibrain.index import Index, fts_query                         # noqa: E402
from aibrain.jobs import JobRunner                                 # noqa: E402
from aibrain.server import Handler, State, build_router            # noqa: E402
from aibrain.vault import link_targets, normalize, parse_frontmatter, scan  # noqa: E402

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


def make_vault(root: Path) -> None:
    for rel, body in VAULT.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")


class VaultTests(unittest.TestCase):
    def test_frontmatter_and_links(self):
        data, body = parse_frontmatter(VAULT["Index.md"])
        self.assertEqual(data["title"], "Index")
        self.assertNotIn("---", body.splitlines()[:1])
        self.assertEqual(
            link_targets(body), ["Protocols", "Recipes/Sourdough", "Nothing Here"]
        )

    def test_normalize_folds_separators(self):
        self.assertEqual(normalize("Agent-Context_Protocol"), "agent context protocol")
        self.assertEqual(normalize("Café"), "cafe")

    def test_scan_finds_every_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            make_vault(root)
            (root / ".obsidian").mkdir()
            (root / ".obsidian" / "config.md").write_text("ignored", encoding="utf-8")
            notes = list(scan(root, "t", [".obsidian"]))
            self.assertEqual(len(notes), len(VAULT))
            sources = {n.source for n in notes}
            self.assertEqual(sources, {"Root", "Recipes", "Journal"})


class IndexTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.vault = self.root / "vault"
        make_vault(self.vault)
        self.brain = BrainConfig(id="t", name="Test", path=str(self.vault))
        self.index = Index(self.root / "index.sqlite3")
        self.index.reindex([self.brain])

    def tearDown(self):
        self._tmp.cleanup()

    def test_counts_and_links(self):
        self.assertEqual(self.index.count(), len(VAULT))
        index_note = self.index.note_by_path("t", "Index.md")
        self.assertIsNotNone(index_note)
        names = {n.title for n in self.index.neighbours(index_note.id)}
        self.assertIn("Protocols", names)
        self.assertIn("Sourdough", names)          # resolved through a path link
        self.assertIn("Nothing Here", self.index.outgoing_unresolved(index_note.id))

    def test_backlinks_are_symmetric(self):
        protocols = self.index.note_by_path("t", "Protocols.md")
        agents = self.index.note_by_path("t", "Agents.md")
        # Agents links to Protocols; Protocols must show Agents as a neighbour.
        self.assertIn(agents.id, [n.id for n in self.index.neighbours(protocols.id)])

    def test_search_ranks_title_matches(self):
        hits = self.index.search("sourdough")
        self.assertTrue(hits)
        self.assertEqual(hits[0].title, "Sourdough")
        self.assertIn("<mark>", self.index.search("miso")[0].snippet.lower())

    def test_search_is_prefix_and_injection_safe(self):
        self.assertTrue(self.index.search("proto"))            # prefix
        self.assertEqual(self.index.search('" OR "'), [])      # no crash, no match
        self.assertEqual(fts_query(""), "")

    def test_incremental_reindex_is_cheap_and_correct(self):
        stats = self.index.reindex([self.brain])
        self.assertEqual(stats["added"], 0)
        self.assertEqual(stats["updated"], 0)

        (self.vault / "Ramen.md").write_text("# Ramen\n\nNow about [[Agents]].\n",
                                             encoding="utf-8")
        time.sleep(0.01)
        (self.vault / "Recipes" / "Ramen.md").write_text(
            "# Ramen\n\nShoyu now. [[Protocols]]\n", encoding="utf-8")
        stats = self.index.reindex([self.brain])
        self.assertEqual(stats["added"], 1)
        self.assertEqual(stats["updated"], 1)
        self.assertTrue(self.index.search("shoyu"))
        self.assertFalse(self.index.search("miso"))            # old text is gone

    def test_deleted_notes_leave_the_index(self):
        (self.vault / "Recipes" / "Ramen.md").unlink()
        stats = self.index.reindex([self.brain])
        self.assertEqual(stats["removed"], 1)
        self.assertEqual(self.index.count(), len(VAULT) - 1)

    def test_markdown_renders_and_resolves_wikilinks(self):
        note = self.index.note_by_path("t", "Protocols.md")
        body = (self.vault / "Protocols.md").read_text(encoding="utf-8")
        html = md.render(body, self.index, "t")
        self.assertIn("<strong>ACP</strong>", html)
        self.assertIn("<code>A2A</code>", html)
        self.assertIn("<table>", html)
        self.assertIn('class="wiki" data-note=', html)
        # A task list keeps its state.
        tasks = md.render((self.vault / "Agents.md").read_text(encoding="utf-8"),
                          self.index, "t")
        self.assertIn('class="task done"', tasks)

    def test_markdown_escapes_injected_html(self):
        html = md.render("<img src=x onerror=alert(1)>\n\n[[Index]]", self.index, "t")
        self.assertNotIn("<img", html)
        self.assertIn("&lt;img", html)

    def test_graph_payload_maps_ids_back_to_notes(self):
        cfg = Config(brains=[self.brain], agents=default_agents())
        result = graph.build(cfg, self.index)
        self.assertEqual(len(result.node_ids), len(VAULT))
        self.assertEqual(result.payload["stats"]["notes"], len(VAULT))

        # Every edge index must land inside its own brain's node list.
        brain = result.payload["brains"][0]
        size = sum(s["count"] for s in brain["sources"])
        for a, b in brain["edges"]:
            self.assertTrue(0 <= a < size and 0 <= b < size)

        # Node order is the contract between server and renderer.
        flat = [n["nid"] for s in brain["sources"] for n in s["notes"]]
        self.assertEqual(flat, result.node_ids)

    def test_layout_keeps_galaxies_apart(self):
        radii = [17.0, 5.0, 11.0, 9.0, 17.0, 6.0]
        slots = graph.brain_slots(radii)
        self.assertEqual(len(slots), len(radii))
        for i in range(len(radii)):
            for j in range(i + 1, len(radii)):
                gap = sum((slots[i][k] - slots[j][k]) ** 2 for k in range(3)) ** 0.5
                self.assertGreater(gap, radii[i] + radii[j],
                                   f"galaxies {i} and {j} overlap")

    def test_hub_spreading_is_a_permutation(self):
        items = list(range(97))
        spread = graph._spread_hubs(items)
        self.assertEqual(sorted(spread), items)
        self.assertNotEqual(spread[:5], items[:5])


class ServerTests(unittest.TestCase):
    """Drives the real HTTP server the way the browser does."""

    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        root = Path(cls._tmp.name)
        vault = root / "vault"
        make_vault(vault)

        cfg = Config(
            brains=[BrainConfig(id="t", name="Test", path=str(vault))],
            agents=[a for a in default_agents() if a.kind == "local"],
            scripts=[ScriptConfig(
                id="echo", name="Echo", description="test script",
                command=[sys.executable, "-c", "print('hello from a job')"],
            )],
            port=0,
        )
        cfg.path = root / "config.json"
        cfg.save()

        cls.state = State(cfg)
        cls.state.index.reindex(cfg.enabled_brains())
        cls.state.universe(rebuild=True)

        handler = type("H", (Handler,),
                       {"state": cls.state, "router": build_router(cls.state)})
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        cls.httpd.daemon_threads = True
        cls.base = f"http://127.0.0.1:{cls.httpd.server_address[1]}"
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.state.close()
        cls._tmp.cleanup()

    # ---- helpers ---------------------------------------------------------
    def get(self, path):
        with urllib.request.urlopen(self.base + path, timeout=20) as r:
            return json.loads(r.read().decode())

    def post(self, path, body=None):
        data = json.dumps(body or {}).encode()
        req = urllib.request.Request(self.base + path, data=data, method="POST")
        req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, timeout=20) as r:
            return json.loads(r.read().decode())

    def sse(self, path, limit=400):
        events = []
        with urllib.request.urlopen(self.base + path, timeout=60) as r:
            for raw in r:
                line = raw.decode()
                if line.startswith("data:"):
                    events.append(json.loads(line[5:]))
                    if events[-1].get("type") == "done" or len(events) >= limit:
                        break
        return events

    # ---- tests -----------------------------------------------------------
    def test_status_and_universe(self):
        status = self.get("/api/status")
        self.assertEqual(status["notes"], len(VAULT))
        self.assertEqual(len(status["brains"]), 1)

        universe = self.get("/api/universe")
        self.assertEqual(universe["stats"]["notes"], len(VAULT))
        self.assertTrue(universe["brains"][0]["sources"])
        self.assertTrue(universe["agents"])

    def test_static_files_are_served_and_confined(self):
        with urllib.request.urlopen(self.base + "/", timeout=10) as r:
            self.assertIn(b"<title>AI Brains</title>", r.read())
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(self.base + "/../aibrain/config.py", timeout=10)
        self.assertEqual(caught.exception.code, 404)

    def test_search_then_open_a_note(self):
        found = self.get("/api/search?q=protocols")
        self.assertTrue(found["results"])
        top = found["results"][0]
        self.assertEqual(top["brain"], "Test")

        note = self.get(f"/api/note/{top['nid']}")
        self.assertEqual(note["name"], "Protocols")
        self.assertIn("<table>", note["html"])
        self.assertTrue(note["linked"])
        self.assertGreater(note["words"], 0)

        # The universe id round-trips back to the same note.
        self.assertEqual(self.get(f"/api/node/{note['gid']}")["nid"], note["nid"])

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

    def test_unknown_agent_reports_an_error_rather_than_hanging(self):
        events = self.sse("/api/stream/chat?agent=nope&q=hi")
        self.assertEqual(events[0]["type"], "error")
        self.assertEqual(events[-1]["type"], "done")

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

    def test_reindex_endpoint_runs_a_job(self):
        job = self.post("/api/reindex", {"force": False})["job"]
        events = self.sse(f"/api/stream/job/{job['id']}")
        self.assertEqual(events[-1]["status"], "done")

    def test_view_settings_round_trip(self):
        saved = self.post("/api/view", {"linkOpacity": 0.42})
        self.assertAlmostEqual(saved["view"]["link_opacity"], 0.42)
        self.assertAlmostEqual(self.get("/api/status")["view"]["link_opacity"], 0.42)


class CitationTests(unittest.TestCase):
    """A citation must mean the agent used the note. Nothing else earns a pill.

    The bug these lock down: `cites_from_text` used to top every answer up to
    six pills with our own search results, so an answer that cited one note
    looked identical to one that cited six, and none of them could be trusted.
    """

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        root = Path(self._tmp.name)
        self.vault = root / "vault"
        make_vault(self.vault)
        self.brain = BrainConfig(id="t", name="Test", path=str(self.vault))
        self.index = Index(root / "index.sqlite3")
        self.index.reindex([self.brain])
        self.agent = LocalAgent(
            AgentConfig(id="a", name="A", kind="local"), self.index, {"t": "Test"})
        self.supplied = self.index.search("protocols", limit=6)
        self.assertTrue(self.supplied, "fixture must retrieve something")

    def tearDown(self):
        self._tmp.cleanup()

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
        protocols = self.index.note_by_path("t", "Protocols.md")
        touched = {protocols.id: ("opened", "cat Protocols.md")}
        cites = self.agent.cites_from_text("See [[Protocols]].", self.supplied, touched)
        self.assertEqual(cites[0].evidence, "opened")
        self.assertIn("cat", cites[0].why)

    def test_a_file_read_but_never_named_still_counts(self):
        agents = self.index.note_by_path("t", "Agents.md")
        cites = self.agent.cites_from_text(
            "Nothing to report.", self.supplied,
            {agents.id: ("read", "served via fs/read_text_file")})
        self.assertEqual([c.title for c in cites], ["Agents"])
        self.assertEqual(cites[0].evidence, "read")

    def test_citations_are_ordered_by_how_much_we_can_prove(self):
        protocols = self.index.note_by_path("t", "Protocols.md")
        cites = self.agent.cites_from_text(
            "[[Index]] and [[Protocols]] and [[Agents]].", self.supplied,
            {protocols.id: ("read", "served")})
        self.assertEqual(cites[0].title, "Protocols")
        ranks = [EVIDENCE_ORDER[c.evidence] for c in cites]
        self.assertEqual(ranks, sorted(ranks))

    def test_a_title_in_two_vaults_is_flagged_rather_than_guessed(self):
        other = Path(self._tmp.name) / "vault2"
        (other / "Recipes").mkdir(parents=True)
        (other / "Recipes" / "Ramen.md").write_text("# Ramen\n\nA copy.\n",
                                                    encoding="utf-8")
        second = BrainConfig(id="t2", name="Other", path=str(other))
        self.index.reindex([self.brain, second])
        agent = LocalAgent(AgentConfig(id="a", name="A", kind="local"),
                           self.index, {"t": "Test", "t2": "Other"})
        cites = agent.cites_from_text("As in [[Ramen]].", [])
        self.assertEqual(len(cites), 1, "one pill, not one per copy")
        self.assertEqual(cites[0].ambiguous_with, 1)

    def test_a_question_still_retrieves_when_a_word_is_absent(self):
        # "describe" appears in no note. Requiring every term returns nothing,
        # which used to leave remote agents with no context at all.
        self.assertEqual(self.index.search("describe the protocols"), [])
        self.assertTrue(self.index.search("describe the protocols", match="any"))
        self.assertTrue(self.agent.retrieve("describe the protocols for me"))


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
            prior = config.VAULT_LINK_DIR
            config.VAULT_LINK_DIR = links
            try:
                found = config.discover_vaults()
                self.assertEqual([p.name for p in found], ["Real"])
                problems = dict(config.link_problems())
                self.assertIn("Broken", problems)

                cfg = Config(brains=[BrainConfig(id="stale", name="Stale",
                                                 path=str(unlinked))])
                config.reconcile_brains(cfg)
                self.assertEqual([b.id for b in cfg.brains], ["real"])
            finally:
                config.VAULT_LINK_DIR = prior

    def test_settings_survive_an_unlink_and_relink(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            real = root / "Kept"
            make_vault(real)
            links = root / "obsidian_vaults"
            links.mkdir()
            (links / "Kept").symlink_to(real, target_is_directory=True)

            import aibrain.config as config
            prior = config.VAULT_LINK_DIR
            config.VAULT_LINK_DIR = links
            try:
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
            finally:
                config.VAULT_LINK_DIR = prior


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


if __name__ == "__main__":
    unittest.main(verbosity=2)
