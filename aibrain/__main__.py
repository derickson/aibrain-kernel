"""Entry point: python3 -m aibrain"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .config import Config, DEFAULT_CONFIG_DIR
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
    parser.add_argument("--no-open", action="store_true",
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

    if args.reindex:
        from .index import Index
        index = Index(cfg.db_path)
        index.reindex(cfg.enabled_brains(), print, force=args.force)
        return 0

    serve(cfg, open_browser=not args.no_open)
    return 0


if __name__ == "__main__":
    sys.exit(main())
