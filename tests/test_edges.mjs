// The one piece of edge-shading logic that is not a shader.
//
// There is no JS test runner in this repo, so this file is a plain node
// script: it exits non-zero on the first failure, and `tests/test_kernel.py`
// runs it (and skips if node is missing). Run it directly with:
//
//     node tests/test_edges.mjs

import assert from 'node:assert/strict';
import {
  EASE_RATE, EDGE_DIM, EDGE_LIT, MAX_TRACED,
  easeMix, edgeBrightness, freezeEase,
} from '../web/edges.js';

// A square with a diagonal: 0-1, 1-2, 2-3, 3-0, 0-2.
const EDGES = [[0, 1], [1, 2], [2, 3], [3, 0], [0, 2]];
const out = () => new Float32Array(EDGES.length * 2);

// Float32Array rounds, so brightness is compared with a tolerance.
function both(buf, i, want) {
  assert.equal(buf[i * 2], buf[i * 2 + 1], `edge ${i} ends disagree`);
  assert.ok(Math.abs(buf[i * 2] - want) < 1e-6,
    `edge ${i} is ${buf[i * 2]}, wanted ${want}`);
}

// Nothing selected: every edge sits at the galaxy's own dimming, and no edge
// is worth tracing.
{
  const buf = out(), hl = [];
  edgeBrightness(EDGES, 0, null, new Set(), 1, buf, hl);
  for (let i = 0; i < EDGES.length; i++) both(buf, i, 1);
  assert.deepEqual(hl, []);

  edgeBrightness(EDGES, 0, null, new Set(), 0.3, buf, hl);
  for (let i = 0; i < EDGES.length; i++) both(buf, i, 0.3);
}

// Hovering node 0: its edges light, the rest fall back.
{
  const buf = out(), hl = [];
  const active = new Set([0, 1, 2]), strong = new Set([0]);
  edgeBrightness(EDGES, 0, active, strong, 1, buf, hl);
  both(buf, 0, EDGE_LIT);   // 0-1, both active
  both(buf, 1, EDGE_LIT);   // 1-2, both active
  both(buf, 2, EDGE_DIM);   // 2-3, 3 is not
  both(buf, 3, EDGE_DIM);   // 3-0, 3 is not
  both(buf, 4, EDGE_LIT);   // 0-2, both active
  // Traced: lit, and touching the node actually under the cursor.
  assert.deepEqual(hl, [0, 4]);
}

// Global ids are offset by the brain's base, local indices are not.
{
  const buf = out(), hl = [];
  const active = new Set([100, 101]), strong = new Set([100]);
  edgeBrightness(EDGES, 100, active, strong, 1, buf, hl);
  both(buf, 0, EDGE_LIT);
  both(buf, 1, EDGE_DIM);
  assert.deepEqual(hl, [0]);
}

// A wide selection is dimmed like any other, but nothing is traced: past
// MAX_TRACED the overlay is a haze, not a path.
{
  const buf = out(), hl = [];
  const active = new Set([0, 1, 2, 3]), strong = new Set([0]);
  for (let g = 10; g < 10 + MAX_TRACED; g++) active.add(g);
  edgeBrightness(EDGES, 0, active, strong, 1, buf, hl);
  both(buf, 0, EDGE_LIT);
  assert.deepEqual(hl, []);
}

// The highlight array is reused between calls, so it must be emptied first.
{
  const buf = out(), hl = [7, 7, 7];
  edgeBrightness(EDGES, 0, null, new Set(), 1, buf, hl);
  assert.deepEqual(hl, []);
}

// freezeEase folds a running ease into its own start, so a new target is
// approached from where the last one got to rather than from a snap back.
{
  const from = new Float32Array([0, 2]), to = new Float32Array([1, 1]);
  freezeEase(from, to, 0);
  assert.deepEqual([...from], [0, 2]);
  freezeEase(from, to, 0.5);
  assert.deepEqual([...from], [0.5, 1.5]);
  freezeEase(from, to, 1);
  assert.deepEqual([...from], [1, 1]);
}

// The ease the shader applies: starts at nothing, finishes, never overshoots.
{
  assert.equal(easeMix(0), 0);
  assert.equal(easeMix(1.4), 1);
  assert.equal(easeMix(99), 1);
  let last = -1;
  for (let t = 0; t <= 1.4; t += 0.05) {
    const m = easeMix(t);
    assert.ok(m >= last, `ease went backwards at ${t}`);
    assert.ok(m >= 0 && m <= 1, `ease left [0,1] at ${t}`);
    last = m;
  }
  // It has to match the old per-frame `x += (target - x) * 0.1` at 60 fps, or
  // the highlight fade changes speed.
  const frames = 30;
  let cpu = 0;
  for (let i = 0; i < frames; i++) cpu += (1 - cpu) * 0.1;
  assert.ok(Math.abs(easeMix(frames / 60) - cpu) < 0.02,
    `ease drifted from the CPU curve: ${easeMix(frames / 60)} vs ${cpu}`);
  assert.ok(Math.abs(EASE_RATE - 60 * -Math.log(0.9)) < 0.1);
}

console.log('web/edges.js ok');
