---
name: vault-search
description: Search the user's Obsidian vaults (brains) through aibrain's own
  hybrid search engine instead of grepping markdown files directly. Use this
  first for any question about note content, "what do I know about X",
  finding notes on a topic, or locating a specific note before reading it.
---

# vault-search

The vaults reachable from this session are indexed by aibrain's own search
(Elasticsearch hybrid search when configured, Postgres full-text fallback
otherwise) — the same engine and ranking the aibrain UI uses. Prefer it over
`grep`/`find` across the vault directories: it ranks by relevance across all
enabled vaults, not just literal substring matches, and it's what "what do I
know about X" questions in the app itself resolve to.

## Usage

```
python3 .claude/skills/vault-search/search.py "query text" [--limit N] [--brains id1,id2]
```

Run it with the Bash tool. Each result line is `[vault] Title (relative/path.md)`
followed by an indented snippet. The path is relative to that vault's root —
resolve it against the vault's directory (visible under the session's
additional directories) before opening the file with Read.

Iterate on the query like a search engine, not a question: short, keyword-y
phrasings beat full sentences. If nothing useful comes back, broaden the terms
before concluding the vaults don't cover the topic.

Requires the aibrain server to be running locally (`http://127.0.0.1:8760` by
default; override with `AIBRAIN_SEARCH_URL`). If it's unreachable, say so
rather than falling back to a blind directory walk.
