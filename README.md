# aibrain-kernel

A local app for the knowledge you already have. Your Obsidian vaults are read
into a searchable index and drawn as galaxies — one per vault, each note a star,
each wikilink a chord between them. Agents float in the same space: double-click
one and ask it something, and the notes it answers from light up and open.

Nothing leaves the machine unless you connect an agent that goes somewhere else.

```sh
python3 -m aibrain
```

That is the whole install. It finds your vaults, builds the index, opens
<http://127.0.0.1:8760/>, and starts rendering.

<!-- TODO: a screenshot of the universe belongs here -->

## What it does

**Search.** Every note goes into a SQLite FTS5 index. Search is prefix-matching
as you type, ranked by BM25 with a nudge for well-connected notes, and every
result carries the passage that matched. Hits light up in the universe as you
hover them.

**Read.** Clicking a star opens the note, rendered from the file on disk.
Wikilinks resolve against the index and are clickable, so you can walk the graph
from the reader as well as from the sky. Links that point at nothing are listed
separately — in a vault those are usually notes you meant to write.

**Ask.** Agents are stars. Each question is answered against passages retrieved
from your brains, and whatever the agent cites becomes a pill you can click:
the note opens on the right and the camera flies to its star, with a pulse
travelling from the agent to the note it used.

**Feed.** The exporters under `scripts/` are runnable from the UI, with their
output streaming live, and the index rebuilds when they finish.

## The pieces

```
aibrain-kernel/
├── aibrain/                   the app (Python 3, standard library only)
│   ├── __main__.py            entry point; python3 -m aibrain
│   ├── config.py              ~/.aibrain/config.json, and first-run discovery
│   ├── vault.py               reading a vault: frontmatter, wikilinks, tags
│   ├── index.py               SQLite + FTS5, incremental reindex, search
│   ├── graph.py               index → the universe payload, and its layout
│   ├── md.py                  markdown → HTML, with wikilinks resolved
│   ├── jobs.py                running scripts, streaming their output
│   ├── server.py              threaded HTTP server, JSON API, SSE
│   └── agents/                local retrieval, ACP over stdio, A2A over HTTP
├── web/                       the UI (no framework, no build step)
│   ├── universe.js            the three.js visualisation
│   ├── app.js                 panels, search, chat, the control drawer
│   └── vendor/three.module.js three.js r160, vendored so it works offline
├── scripts/                   one folder per source system
│   └── macwhisper/            MacWhisper transcript exporter
├── tests/test_kernel.py       smoke tests over the whole stack
└── design_concepts/           the design this was built from
```

### Brains

A brain is an Obsidian vault, and a vault becomes a brain by being symlinked
into `obsidian_vaults/`. That folder is the allowlist — nothing is scanned
because it happened to be somewhere findable, so the app can never volunteer a
vault you did not ask for.

```sh
ln -s ~/Documents/ObsidianVaults/Home obsidian_vaults/Home
```

Use an absolute path. A relative one is resolved from inside
`obsidian_vaults/`, which is two levels below the repo, and getting that wrong
produces a dangling link that silently yields no vault. The **Brains** tab
creates and removes these links for you, and reports any that do not resolve.
Removing a brain removes only the link; the vault itself is never touched.
Per-brain settings are kept by name, so unlinking and relinking restores them.

Inside a galaxy, each top-level folder becomes a ribbon with its own colour, and
a note's size is its link count — so a vault's hubs are literally its brightest
stars, and they carry the labels. Galaxies are sized so that note density stays
roughly constant, which is why a 2,500-note vault is a sphere and a 4-note vault
is a speck.

Large vaults draw their most connected notes first, up to a per-brain cap
(**View** tab) that trades frame rate for completeness. The header tells you
when you are seeing a subset: `2200 OF 2445 NOTES`.

### Agents

Three kinds, all configured in the **Agents** tab, all rendered the same way:

| Kind | Transport | What it is |
|---|---|---|
| `local` | none | Answers from the index itself. No model, no network. |
| `acp` | [Agent Client Protocol](https://agentclientprotocol.com) over stdio | A coding agent — Claude Code, Codex — spoken to the way an editor speaks to it. |
| `a2a` | [Agent2Agent](https://a2a-protocol.org) over HTTP | A remote agent that publishes an agent card, such as Elasticsearch Agent Builder. |

Every agent, whatever the transport, is handed the same thing: the top passages
retrieved from your brains for that question, with a note asking it to cite
notes as `[[Title]]`. Those titles are resolved back to note ids, which is how a
remote agent's answer ends up with citations that open your files.

ACP adapters found on `PATH` at first run are enabled automatically:

```sh
npm install -g @agentclientprotocol/claude-agent-acp   # Claude Code
npm install -g @agentclientprotocol/codex-acp          # Codex
```

An ACP agent may read and write files under its working directory and inside
your vaults, and nowhere else. Tool calls it asks permission for are approved
automatically — there is no modal yet, and a blocking prompt would stall the
stream. Point it at a vault you have backed up.

### Tools

The **Tools** tab runs the scripts in `scripts/`, streaming stdout and stderr
into the panel. The MacWhisper exporter is wired up with its `--dry-run` and
`--verbose` flags as toggles; when it finishes, the index rescans and the
universe rebuilds. See [`scripts/macwhisper/README.md`](scripts/macwhisper/README.md)
for what it does to the MacWhisper database.

## Controls

| | |
|---|---|
| Drag | orbit |
| Scroll | zoom |
| Click a star | open that note |
| Double-click a star | open it and fly to it |
| Double-click an agent | chat with it |
| Drag an agent | move it; the position is saved |
| `/` | search |
| `Esc` | close the topmost panel |
| Brain chip | focus one galaxy |
| ⊕ | reset the view |
| ☰ | brains, agents, tools, view |

## Configuration

Everything the UI writes lands in one file:

```
~/.aibrain/config.json      brains, agents, scripts, view settings
~/.aibrain/index.sqlite3    the index (rebuildable; delete it freely)
```

Edit it by hand if you prefer — it is read on start. `AIBRAIN_HOME` moves both,
and `--config` points at a specific file.

```sh
python3 -m aibrain --port 9000 --no-open   # somewhere else, no browser
python3 -m aibrain --reindex               # rebuild the index and exit
python3 -m aibrain --reindex --force       # re-read every note, ignoring mtimes
```

## Conventions

- Python 3 standard library only, so it runs on the system interpreter with no
  virtualenv. No build step for the frontend either: `web/` is served as-is.
- Reindexing is incremental — a note is only re-read when its size or timestamp
  changed, so a rescan of several thousand notes costs a `stat()` each.
- Every exporter treats its source as read-only and verifies the source files
  are unchanged after a run.
- Upstream schema assumptions are checked in as a baseline and validated on
  every run, so a silent upstream change becomes a visible report rather than
  corrupted vault content.

## Tests

```sh
python3 tests/test_kernel.py
```

Builds a small vault in a temp directory, indexes it, and drives the HTTP API
the way the browser does — scanning, link resolution, search, markdown, the
universe payload, the agent stream, and the job runner.

## Status

The UI, the index, the local agent, ACP and A2A are working. Known gaps:

- Tool-call permissions are auto-approved; there is no prompt.
- The universe rebuilds wholesale after a reindex rather than updating in place.
- A2A authentication is limited to static headers.
