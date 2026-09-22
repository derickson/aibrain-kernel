"""Turning the index into the universe the browser renders.

The visualisation wants brains, each holding source ribbons, each holding an
ordered list of nodes, plus the edges between them. Node order matters: the
renderer assigns every node a global id by counting, and the browser maps that
id straight back to a note id, so this module is the single place where that
ordering is decided.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

from .config import SOURCE_COLORS, BrainConfig, Config
from .index import Index

# Folders that are containers rather than subject areas get folded together so
# a vault does not turn into thirty near-empty ribbons.
MIN_SOURCE_SHARE = 0.012
MAX_SOURCES = 8
OTHER_LABEL = "Other"


def brain_radius(note_count: int) -> float:
    """Radius that keeps surface density roughly constant across vaults.

    Nodes sit on a shell, so area — and therefore radius squared — has to grow
    with the note count or a big vault renders as one white blob. The square
    root does that; the clamp stops a four-note vault from being a dot and a
    ten-thousand-note one from filling the sky.
    """
    return round(max(4.5, min(17.0, 0.34 * math.sqrt(max(note_count, 1)) + 2.6)), 2)


def brain_slots(radii: list[float]) -> list[list[float]]:
    """Place the galaxies so they all fit one screen without touching.

    Up to three vaults read best as a row, the way the design concept has it.
    Past that a row gets so wide the camera has to pull back until every galaxy
    is a speck, so they stack into rows instead. Spacing comes from the radii
    themselves, so enabling a big vault pushes its neighbours apart rather than
    swallowing them.
    """
    count = len(radii)
    if count == 0:
        return []
    if count == 1:
        return [[0.0, 0.0, 0.0]]

    rows = 1 if count <= 3 else 2 if count <= 8 else 3
    cols = math.ceil(count / rows)

    # Which cell each galaxy lands in, row-major.
    cells = [(i // cols, i % cols) for i in range(count)]
    col_r = [max((radii[i] for i in range(count) if cells[i][1] == c), default=5.0)
             for c in range(cols)]
    row_r = [max((radii[i] for i in range(count) if cells[i][0] == r), default=5.0)
             for r in range(rows)]

    def axis(sizes: list[float], pad: float) -> list[float]:
        pos, cursor = [], 0.0
        for i, size in enumerate(sizes):
            if i:
                cursor += (sizes[i - 1] + size) * 1.1 + pad
            pos.append(cursor)
        centre = (pos[0] + pos[-1]) / 2 if pos else 0.0
        return [p - centre for p in pos]

    xs = axis(col_r, 4.0)
    ys = axis(row_r, 2.5)

    out: list[list[float]] = []
    for i, (r, c) in enumerate(cells):
        # Stagger the depth so a grid does not read as a flat wall.
        z = 0.5 * radii[i] * math.cos(i * 1.7)
        out.append([round(xs[c], 2), round(-ys[r], 2), round(z, 2)])
    return out


def agent_slots(count: int, placed: list[dict]) -> list[list[float]]:
    """Find empty sky for the agent stars.

    Stars parked past the edge of the field make the camera pull back until
    every galaxy is a speck, so prefer space the galaxies already leave: the
    band between stacked rows, and only otherwise the sky above them. Either
    way the stars sit forward of the galaxies in z, so they read as nearer.
    """
    if count <= 0:
        return []

    centres = [(b["center"][1], b["radius"]) for b in placed] or [(0.0, 10.0)]

    # Does any galaxy cross the horizontal mid-line? If not, that band is free.
    mid_free = all(abs(y) - r > 2.0 for y, r in centres)
    lift = 0.0 if mid_free else max(y + r for y, r in centres) + 7.0

    # Horizontally, aim for the gaps between columns rather than the columns
    # themselves, so a star does not sit on top of a galaxy's label.
    columns = sorted({round(b["center"][0], 1) for b in placed}) or [0.0]
    reach = max((abs(b["center"][0]) + b["radius"] for b in placed), default=20.0)
    lanes = [(columns[i] + columns[i + 1]) / 2 for i in range(len(columns) - 1)]
    # Once the gaps are used up, go outside the field rather than back on top
    # of a galaxy.
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


@dataclass
class GraphResult:
    payload: dict
    node_ids: list[int]  # global viz id -> note id


def _source_buckets(rows, brain_name: str) -> list[tuple[str, list]]:
    """Group rows by top-level folder, merging the long tail into 'Other'."""
    buckets: dict[str, list] = {}
    for row in rows:
        buckets.setdefault(row["source"], []).append(row)

    total = len(rows)
    ranked = sorted(buckets.items(), key=lambda kv: -len(kv[1]))
    keep: list[tuple[str, list]] = []
    tail: list = []
    for name, items in ranked:
        if len(keep) < MAX_SOURCES and len(items) >= max(2, total * MIN_SOURCE_SHARE):
            keep.append((name, items))
        else:
            tail.extend(items)
    if tail:
        keep.append((OTHER_LABEL, tail))
    if not keep:
        keep = [(brain_name, list(rows))]
    return keep


def build(cfg: Config, index: Index) -> GraphResult:
    """Assemble the full universe payload for the enabled brains."""
    brains_cfg = cfg.enabled_brains()

    node_ids: list[int] = []          # viz gid -> note id
    gid_of: dict[int, int] = {}       # note id -> viz gid
    brains_out: list[dict] = []

    per_brain_rows: list[tuple[BrainConfig, list]] = []
    for brain in brains_cfg:
        rows = index.all_for_graph(brain.id, brain.max_nodes)
        per_brain_rows.append((brain, rows))

    radii = [b.radius or brain_radius(len(rows)) for b, rows in per_brain_rows]
    slots = brain_slots(radii)

    for bi, (brain, rows) in enumerate(per_brain_rows):
        total_in_vault = index.count(brain.id)
        buckets = _source_buckets(rows, brain.name)
        sources_out: list[dict] = []
        local_start = len(node_ids)

        for si, (name, items) in enumerate(buckets):
            items = _spread_hubs(sorted(items, key=lambda r: (-r["degree"], -r["mtime"])))
            nodes = []
            for row in items:
                gid = len(node_ids)
                gid_of[row["id"]] = gid
                node_ids.append(row["id"])
                nodes.append({
                    "nid": row["id"],
                    "name": row["title"],
                    "deg": row["degree"],
                    "mtime": row["mtime"],
                })
            sources_out.append({
                "id": f"{brain.id}:{_slug(name)}",
                "name": name,
                "color": SOURCE_COLORS[(si + bi * 3) % len(SOURCE_COLORS)],
                "count": len(nodes),
                "notes": nodes,
            })

        local_ids = {r["id"] for r in rows}
        edges = index.links_among(local_ids)
        local_edges = [
            [gid_of[a] - local_start, gid_of[b] - local_start]
            for a, b in edges
            if a in gid_of and b in gid_of
        ]

        brains_out.append({
            "id": brain.id,
            "name": brain.name,
            "center": brain.center or slots[bi],
            "radius": radii[bi],
            "seed": brain.seed if brain.seed is not None else 7 + bi * 13,
            "sources": sources_out,
            "edges": local_edges,
            "shown": len(rows),
            "total": total_in_vault,
            "path": str(brain.resolved_path()),
        })

    # Cross-brain arcs: real links whose two ends live in different vaults.
    cross: list[list[int]] = []
    all_ids = set(gid_of)
    seen_pairs: set[tuple[int, int]] = set()
    note_brain = {}
    for brain, rows in per_brain_rows:
        for row in rows:
            note_brain[row["id"]] = brain.id
    for a, b in index.links_among(all_ids):
        if note_brain.get(a) == note_brain.get(b):
            continue
        key = (min(a, b), max(a, b))
        if key in seen_pairs:
            continue
        seen_pairs.add(key)
        cross.append([gid_of[a], gid_of[b]])
        if len(cross) >= 400:
            break

    agents_cfg = cfg.enabled_agents()
    slots = agent_slots(len(agents_cfg), brains_out)
    agent_pos = [a.pos or slots[i] for i, a in enumerate(agents_cfg)]

    # Everything the camera has to frame: the galaxies plus wherever the agent
    # stars ended up, including positions the user dragged them to.
    extent_x = max(
        [abs(b["center"][0]) + b["radius"] for b in brains_out]
        + [abs(p[0]) + 4.0 for p in agent_pos] or [20.0]
    )
    extent_y = max(
        [abs(b["center"][1]) + b["radius"] for b in brains_out]
        + [abs(p[1]) + 4.0 for p in agent_pos] or [14.0]
    )

    payload = {
        "title": cfg.title,
        "brains": brains_out,
        "cross": cross,
        "agents": [
            {
                "id": a.id,
                "name": a.name,
                "protocol": a.label(),
                "kind": a.kind,
                "color": a.color,
                "pos": agent_pos[i],
                "intro": a.intro,
                "suggestions": a.suggestions,
            }
            for i, a in enumerate(agents_cfg)
        ],
        "options": {
            "rotationSpeed": cfg.view.rotation_speed,
            "linkOpacity": cfg.view.link_opacity,
            "showAllLabels": cfg.view.show_all_labels,
            "ribbonTwist": cfg.view.ribbon_twist,
        },
        "fitWidth": round(extent_x * 2 + 20.0, 1),
        "fitHeight": round(extent_y * 2 + 20.0, 1),
        "stats": {
            "notes": index.count(),
            "shown": len(node_ids),
            "brains": len(brains_out),
            "agents": len(cfg.enabled_agents()),
            "cross": len(cross),
        },
    }
    return GraphResult(payload=payload, node_ids=node_ids)


def _spread_hubs(ranked: list) -> list:
    """Deal a degree-ranked list around the ribbon instead of front-loading it.

    Position along a ribbon is just the list index, so handing the renderer a
    sorted list piles every bright hub into one arc and blows the galaxy out
    into a white smear. Stepping by a stride coprime with the length walks the
    whole ribbon exactly once, so the hubs end up evenly spaced and the bright
    points read as individual stars.
    """
    n = len(ranked)
    if n < 8:
        return ranked
    stride = max(2, int(n * 0.6180339887))
    while math.gcd(stride, n) != 1:
        stride += 1
        if stride >= n:
            return ranked
    out = [None] * n
    for i, item in enumerate(ranked):
        out[(i * stride) % n] = item
    return out


def _slug(name: str) -> str:
    out = "".join(c.lower() if c.isalnum() else "-" for c in name).strip("-")
    return out or "source"
