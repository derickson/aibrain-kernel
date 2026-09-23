// What an edge's brightness should be, kept out of universe.js so it can run
// without a GPU or a DOM — and so it can be tested.
//
// The renderer holds two brightness snapshots per edge vertex and eases from
// one to the other in the vertex shader, which is why nothing here runs per
// frame: a frame only moves the mix uniform. This module decides what the two
// snapshots hold, and that only changes when the selection does.

/** Past this many active nodes the traced overlay reads as noise, not a path. */
export const MAX_TRACED = 240;
/** An edge sitting inside the tight first ring — both ends within one hop of
 *  the origin. Above 1, so the shader stops fading it by depth and it reads
 *  through the shell; also the overlay-traced brightness. */
export const EDGE_LIT = 1.5;
/** An edge that reaches into the second ring but does not touch the first —
 *  still lit, just fainter, so attention decays with distance instead of
 *  cutting off sharply at one hop. */
export const EDGE_LIT_FAR = 0.75;
/** Neither end is within two hops. Not zero: the galaxy keeps its shape while
 *  you look. */
export const EDGE_DIM = 0.06;

/**
 * Write one brightness per edge *vertex* — two per edge, the shader needs both
 * ends — into `out`, and collect into `highlight` the edges worth drawing in
 * the bright overlay on top.
 *
 * `active` is every node within two hops of the origin (null when nothing is
 * selected, the common case); `near` is the tighter one-hop ring (origin plus
 * its direct links). `dim` is the whole-galaxy multiplier applied when
 * another galaxy has the focus. Returns `highlight`, emptied first.
 */
export function edgeBrightness(edges, offset, active, near, dim, out, highlight) {
  highlight.length = 0;
  const trace = !!active && active.size < MAX_TRACED;
  for (let i = 0; i < edges.length; i++) {
    if (!active) { out[i * 2] = out[i * 2 + 1] = dim; continue; }
    const e = edges[i], ga = offset + e[0], gc = offset + e[1];
    const both = active.has(ga) && active.has(gc);
    const tight = both && near.has(ga) && near.has(gc);
    out[i * 2] = out[i * 2 + 1] = (tight ? EDGE_LIT : both ? EDGE_LIT_FAR : EDGE_DIM) * dim;
    if (tight && trace) highlight.push(i);
  }
  return highlight;
}

/**
 * Fold an ease that is still running into its own starting point, so a new
 * target can be eased from wherever the old one had reached rather than
 * snapping back. `mix` is the progress of the ease being replaced, 0 to 1.
 */
export function freezeEase(from, to, mix) {
  if (mix <= 0) return from;
  for (let i = 0; i < from.length; i++) {
    from[i] = mix >= 1 ? to[i] : from[i] + (to[i] - from[i]) * mix;
  }
  return from;
}

/** How fast the ease approaches its target, per second. */
export const EASE_RATE = 6.3;
/** Where the ease has got to, `seconds` after the target changed.
 *
 * The CPU version was `x += (target - x) * 0.1` once per frame, which is this
 * curve at exactly 60 fps and a slower one at anything else. Driving it from
 * elapsed time instead means a heavy frame does not also slow the fade. */
export function easeMix(seconds) {
  return seconds >= 1.4 ? 1 : 1 - Math.exp(-EASE_RATE * seconds);
}
