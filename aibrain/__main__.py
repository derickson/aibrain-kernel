"""Entry point: python3 -m aibrain"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .config import Config, DEFAULT_CONFIG_DIR
from .corpus import Corpus, CorpusError, unreachable_message
from .server import serve


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python3 -m aibrain",
        description="Serve the AI Brain UI over your Obsidian vaults.",
    )
    parser.add_argument("--config", type=Path,
                        default=DEFAULT_CONFIG_DIR / "config.json",
                        help="config file (created on first run)")
    parser.add_argument("--host", default=None, help="bind address")
    parser.add_argument("--port", type=int, default=None, help="port")
    parser.add_argument("--no-open", "--no-browser", dest="no_open",
                        action="store_true",
                        help="do not open a browser window")
    parser.add_argument("--reindex", action="store_true",
                        help="rebuild the index and exit")
    parser.add_argument("--force", action="store_true",
                        help="with --reindex, re-read every note")
    args = parser.parse_args(argv)

    cfg = Config.load(args.config)
    if args.host:
        cfg.host = args.host
    if args.port:
        cfg.port = args.port

    # Postgres and the Rust service are required now — there is no local
    # fallback to quietly degrade to, so say so plainly instead of throwing a
    # traceback at the first request.
    corpus = Corpus()
    try:
        corpus.health()
    except CorpusError:
        print(unreachable_message(corpus.base_url), file=sys.stderr)
        return 1

    if args.reindex:
        stats = corpus.reindex(force=args.force)
        print(
            f"{stats.get('scanned', 0)} notes scanned — "
            f"+{stats.get('added', 0)} added, ~{stats.get('updated', 0)} updated, "
            f"-{stats.get('removed', 0)} removed, ={stats.get('unchanged', 0)} unchanged, "
            f"{stats.get('links', 0)} links"
        )
        return 0

    serve(cfg, open_browser=not args.no_open)
    return 0


if __name__ == "__main__":
    sys.exit(main())
