#!/usr/bin/env python3
"""Build a throwaway vault big enough to feel.

The real corpus is 6,061 notes across five vaults, which is not something a
test can carry around. This writes a vault of the same shape — folders that
become ribbons, a degree distribution with a few hubs and a long tail — so the
renderer can be measured against a realistic load without touching anyone's
notes.

Deterministic: the same seed writes the same vault, so a frame-rate number
from one run is comparable with the next.

    python3 scripts/synthetic_vault.py /tmp/big-vault --notes 6000 --links 15000

Standard library only, like the rest of the package.
"""

from __future__ import annotations

import argparse
import random
import shutil
from pathlib import Path

# Top-level folders become the ribbons of the galaxy, so the count and the
# lopsidedness matter more than the names.
FOLDERS = [
    ("Projects", 26), ("Notes", 22), ("Journal", 18), ("Reference", 14),
    ("People", 8), ("Recipes", 6), ("Archive", 6),
]

WORDS = (
    "signal orbit lattice ember drift quorum vellum harbor cinder ledger "
    "cobalt tundra marrow prism thicket gable fathom quarry sable tinder "
    "vessel warden yarrow zephyr alcove bramble current dovetail"
).split()


def note_names(count: int, rng: random.Random) -> list[str]:
    """`Folder/Title` for every note, folders weighted as FOLDERS says."""
    weights = [w for _, w in FOLDERS]
    names: list[str] = []
    seen: set[str] = set()
    while len(names) < count:
        folder = rng.choices([f for f, _ in FOLDERS], weights=weights)[0]
        title = " ".join(rng.sample(WORDS, 3)).title()
        rel = f"{folder}/{title}"
        if rel in seen:
            continue
        seen.add(rel)
        names.append(rel)
    return names


def link_targets(count: int, links: int, rng: random.Random) -> list[list[int]]:
    """Who links to whom, with a few hubs and a long tail of leaves.

    Targets are drawn from a triangular distribution over the note list, so a
    small prefix collects most of the inbound links — the degree spread real
    vaults have, and the one the layout sizes its stars by.
    """
    out: list[list[int]] = [[] for _ in range(count)]
    for _ in range(links):
        source = rng.randrange(count)
        target = int(rng.triangular(0, count - 1, 0))
        if target == source:
            target = (target + 1) % count
        out[source].append(target)
    return out


def write_vault(root: Path, count: int, links: int, seed: int) -> None:
    rng = random.Random(seed)
    names = note_names(count, rng)
    targets = link_targets(count, links, rng)

    if root.exists():
        shutil.rmtree(root)
    for folder, _ in FOLDERS:
        (root / folder).mkdir(parents=True, exist_ok=True)

    for i, rel in enumerate(names):
        title = rel.split("/", 1)[1]
        body = [f"# {title}", ""]
        body.append(" ".join(rng.sample(WORDS, 12)).capitalize() + ".")
        body.append("")
        for t in targets[i]:
            body.append(f"- see [[{names[t].split('/', 1)[1]}]]")
        body.append("")
        body.append(f"#tag-{i % 40}")
        (root / f"{rel}.md").write_text("\n".join(body) + "\n", encoding="utf-8")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("root", type=Path, help="directory to write (replaced if it exists)")
    ap.add_argument("--notes", type=int, default=6000)
    ap.add_argument("--links", type=int, default=15000)
    ap.add_argument("--seed", type=int, default=11)
    args = ap.parse_args()
    write_vault(args.root, args.notes, args.links, args.seed)
    print(f"{args.notes} notes and about {args.links} wikilinks in {args.root}")


if __name__ == "__main__":
    main()
