#!/usr/bin/env python3
"""Search the user's Obsidian vaults through aibrain's own hybrid search —
the same engine and ranking the aibrain UI uses — instead of grepping
markdown files directly.

Usage:
    python3 search.py "query text" [--limit N] [--brains id1,id2]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.parse
import urllib.request

SEARCH_URL = os.environ.get("AIBRAIN_SEARCH_URL", "http://127.0.0.1:8760")


def main() -> None:
    parser = argparse.ArgumentParser(description="Search aibrain's indexed vault notes")
    parser.add_argument("query")
    parser.add_argument("--limit", type=int, default=15)
    parser.add_argument("--brains", default="", help="comma-separated brain ids to scope the search")
    args = parser.parse_args()

    params = {"q": args.query, "limit": args.limit}
    if args.brains:
        params["brains"] = args.brains
    url = f"{SEARCH_URL}/api/search?{urllib.parse.urlencode(params)}"

    try:
        with urllib.request.urlopen(url, timeout=15) as resp:
            data = json.load(resp)
    except Exception as exc:
        print(f"could not reach aibrain search at {SEARCH_URL} ({exc})", file=sys.stderr)
        print("is the aibrain server running? (dev.sh / make run)", file=sys.stderr)
        sys.exit(1)

    results = data.get("results", [])
    print(f"# {data.get('count', len(results))} result(s) via {data.get('engine', '?')} for: {args.query}\n")
    for row in results:
        title = row.get("name") or row.get("rel_path") or "(untitled)"
        brain = row.get("brain") or row.get("brain_id", "")
        path = row.get("rel_path", "")
        snippet = (row.get("snippet") or "").strip().replace("\n", " ")
        print(f"- [{brain}] {title}  ({path})")
        if snippet:
            print(f"    {snippet[:280]}")
    if not results:
        print("(no matches — try broader terms, or check the brain is enabled)")


if __name__ == "__main__":
    main()
