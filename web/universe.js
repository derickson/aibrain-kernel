// The brain universe.
//
// Each vault is a galaxy: its notes are laid out in ribbons on a sphere, one
// ribbon per top-level folder, and the wikilinks between them are drawn as
// chords. Node ids are assigned by counting in the order the server sent them,
// so a global id here is the same global id the server uses to name a note.
//
// Grown from the design concept in design_concepts/, with the synthetic data
// replaced by the real index: node size is link degree, edges are real links,
// and the cross-galaxy arcs are links that actually cross vaults.

import * as THREE from './vendor/three.module.js';
import { edgeBrightness, easeMix, freezeEase } from './edges.js';

function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (s + 0x6d2b79f5) | 0;
    let t = Math.imul(s ^ (s >>> 15), 1 | s);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
const ease = p => (p < 0.5 ? 2 * p * p : 1 - Math.pow(-2 * p + 2, 2) / 2);

function glowTex() {
  const c = document.createElement('canvas'); c.width = c.height = 128;
  const g = c.getContext('2d');
  const gr = g.createRadialGradient(64, 64, 0, 64, 64, 64);
  gr.addColorStop(0, 'rgba(255,255,255,1)');
  gr.addColorStop(0.25, 'rgba(255,255,255,0.45)');
  gr.addColorStop(1, 'rgba(255,255,255,0)');
  g.fillStyle = gr; g.fillRect(0, 0, 128, 128);
  return new THREE.CanvasTexture(c);
}
function ringTex() {
  const c = document.createElement('canvas'); c.width = c.height = 128;
  const g = c.getContext('2d');
  g.strokeStyle = '#fff'; g.lineWidth = 5;
  g.beginPath(); g.arc(64, 64, 56, 0, Math.PI * 2); g.stroke();
  return new THREE.CanvasTexture(c);
}
function starTex() {
  const c = document.createElement('canvas'); c.width = c.height = 256;
  const g = c.getContext('2d');
  const gr = g.createRadialGradient(128, 128, 0, 128, 128, 128);
  gr.addColorStop(0, 'rgba(255,255,255,1)');
  gr.addColorStop(0.08, 'rgba(255,255,255,0.9)');
  gr.addColorStop(0.3, 'rgba(255,255,255,0.18)');
  gr.addColorStop(1, 'rgba(255,255,255,0)');
  g.fillStyle = gr; g.fillRect(0, 0, 256, 256);
  for (const rot of [0, Math.PI / 2, Math.PI / 4, -Math.PI / 4]) {
    g.save(); g.translate(128, 128); g.rotate(rot);
    const lg = g.createLinearGradient(-128, 0, 128, 0);
    const a = rot === 0 || rot === Math.PI / 2 ? 0.9 : 0.35;
    lg.addColorStop(0, 'rgba(255,255,255,0)');
    lg.addColorStop(0.5, `rgba(255,255,255,${a})`);
    lg.addColorStop(1, 'rgba(255,255,255,0)');
    g.fillStyle = lg; g.fillRect(-128, -1.2, 256, 2.4); g.restore();
  }
  return new THREE.CanvasTexture(c);
}

const VERT = `
uniform float uTime; uniform float uScale; uniform float uR;
attribute float aSize; attribute vec3 aColor; attribute float aBright;
varying vec3 vColor; varying float vBright; varying float vDepth;
void main(){
  vColor = aColor; vBright = aBright;
  vec4 mv = modelViewMatrix * vec4(position, 1.0);
  vec4 c = viewMatrix * modelMatrix * vec4(0.0, 0.0, 0.0, 1.0);
  vDepth = clamp((mv.z - c.z) / uR, -1.0, 1.0);
  float pulse = aBright > 1.3 ? 1.0 + 0.3 * sin(uTime * 5.0 + position.x * 3.0) : 1.0;
  gl_PointSize = aSize * SIZEMUL * pulse * uScale / -mv.z;
  gl_Position = projectionMatrix * mv;
}`;
const FRAG_CORE = `
uniform float uGain;
varying vec3 vColor; varying float vBright; varying float vDepth;
void main(){
  float d = length(gl_PointCoord - 0.5) * 2.0; if (d > 1.0) discard;
  float core = 1.0 - smoothstep(0.6, 0.95, d);
  float lift = max(vBright - 1.0, 0.0);
  float depthFade = mix(0.28, 1.0, smoothstep(-1.0, 0.6, vDepth));
  gl_FragColor = vec4(vColor * (1.0 + lift * 1.6),
                      core * clamp(vBright, 0.0, 1.0) * depthFade * mix(0.55, 1.0, uGain));
}`;
// Edges used to be shaded on the CPU: a loop over every edge, every frame,
// writing six floats each. At fifteen thousand links that loop was the frame
// budget, not the draw call. Brightness is a vertex attribute now — two
// snapshots and a mix uniform — so the CPU only touches it when the selection
// changes, and the depth fade is recomputed here instead of uploaded.
const EDGE_VERT = `
uniform float uR; uniform float uMix;
attribute vec3 aBase; attribute float aFrom; attribute float aTo;
varying vec3 vCol;
void main(){
  vec4 mv = modelViewMatrix * vec4(position, 1.0);
  vec4 c = viewMatrix * modelMatrix * vec4(0.0, 0.0, 0.0, 1.0);
  float d = clamp((mv.z - c.z) / uR, -1.0, 1.0);
  float depth = 0.22 + 0.78 * clamp((d + 0.8) / 1.4, 0.0, 1.0);
  float f = mix(aFrom, aTo, uMix);
  vCol = aBase * (f * (f > 1.0 ? 1.0 : depth));
  gl_Position = projectionMatrix * mv;
}`;
// The chunks keep this identical to the LineBasicMaterial it replaces, which
// converts to the output colour space on the way out.
const EDGE_FRAG = `
uniform float uOpacity;
varying vec3 vCol;
void main(){
  gl_FragColor = vec4(vCol, uOpacity);
  #include <tonemapping_fragment>
  #include <colorspace_fragment>
}`;

const FRAG_GLOW = `
uniform float uGain;
varying vec3 vColor; varying float vBright; varying float vDepth;
void main(){
  float d = length(gl_PointCoord - 0.5) * 2.0; if (d > 1.0) discard;
  float g = pow(1.0 - d, 3.0);
  float lift = max(vBright - 1.0, 0.0);
  float depthFade = smoothstep(-0.6, 0.8, vDepth);
  gl_FragColor = vec4(vColor * (0.16 + lift * 1.2),
                      g * clamp(vBright, 0.0, 1.0) * depthFade * uGain);
}`;

export function createUniverse(container, cfg) {
  const opt = Object.assign(
    { rotationSpeed: 0.35, linkOpacity: 0.24, ribbonTwist: 0.25, showAllLabels: false },
    cfg.options
  );
  const glow = glowTex(), ringT = ringTex(), starT = starTex();

  // ---------- renderer / scene
  const W = () => container.clientWidth || 800;
  const H = () => container.clientHeight || 600;
  const renderer = new THREE.WebGLRenderer({
    antialias: true, alpha: true, powerPreference: 'high-performance',
  });
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  renderer.setClearColor(0, 0);
  const el = renderer.domElement;
  Object.assign(el.style, {
    position: 'absolute', inset: '0', width: '100%', height: '100%',
    display: 'block', cursor: 'grab', touchAction: 'none',
  });
  container.appendChild(el);
  const overlay = document.createElement('div');
  Object.assign(overlay.style, {
    position: 'absolute', inset: '0', pointerEvents: 'none', overflow: 'hidden',
  });
  container.appendChild(overlay);
  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(40, 1, 0.1, 900);
  const uniforms = { uTime: { value: 0 }, uScale: { value: 800 } };

  // ---------- brains
  const allNodes = [], gadj = [], brains = [];

  function buildBrain(b, bi) {
    const rand = rng(b.seed ?? bi + 1);
    const R = b.radius ?? 7, offset = allNodes.length;

    // The positions are the server's now. Rust derives each one from a hash of
    // (brain_id, rel_path) instead of from an index in a sorted array, which is
    // what stops the galaxy reshuffling every time a note is edited — something
    // the layout that used to run here could not do, because position *was* the
    // array index. Node i in these buffers is global id offset + i, and that is
    // the same id the server names a note by.
    //
    // Reading them happens in a function rather than inline because it happens
    // twice: once here, and again whenever that vault's revision moves and
    // `repack` refills these same buffers without rebuilding the galaxy.
    let local = [], n = 0;
    let pos = new Float32Array(0), col = new Float32Array(0), siz = new Float32Array(0);

    function readNodes(src, noteIds) {
      const positions = src.positions || [];
      const sizes = src.sizes || [];
      const sourceIndex = src.sourceIndex || [];
      const degrees = src.degrees || [];
      const names = src.names || [];
      const count = sizes.length || Math.floor(positions.length / 3);

      // Degree still decides which stars earn a label; size arrives computed.
      // The 96.5th percentile is the hub cut, which keeps labels sparse.
      const ranked = Array.from(degrees).sort((x, y) => x - y);
      const pct = p => (ranked.length
        ? ranked[Math.min(ranked.length - 1, Math.floor(ranked.length * p))] : 0);
      const hubCut = Math.max(3, pct(0.965));
      const lastSource = Math.max(0, (src.sources || []).length - 1);

      local = [];
      for (let i = 0; i < count; i++) {
        const gid = offset + i;
        const deg = degrees[i] || 0;
        const node = allNodes[gid] || (allNodes[gid] = { gid, bi, li: i });
        node.nid = noteIds[gid] ?? -1;
        node.si = Math.min(sourceIndex[i] || 0, lastSource);
        node.pos = new THREE.Vector3(positions[i * 3], positions[i * 3 + 1],
                                     positions[i * 3 + 2]);
        node.size = sizes[i] ?? 0.2;
        node.hub = deg >= hubCut;
        node.deg = deg;
        node.name = names[i] || 'Note';
        node.mtime = 0;
        if (!gadj[gid]) gadj[gid] = [];
        local.push(node);
      }
      n = count;

      if (pos.length !== n * 3) {
        pos = new Float32Array(n * 3);
        col = new Float32Array(n * 3);
        siz = new Float32Array(n);
      }
      local.forEach((x, i) => {
        pos.set([x.pos.x, x.pos.y, x.pos.z], i * 3);
        const c = new THREE.Color(src.sources[x.si].color);
        col.set([c.r, c.g, c.b], i * 3);
        siz[i] = x.size;
      });
    }

    readNodes(b, cfg.noteIds || []);

    // Local edge list, deduplicated. Rebuilt wholesale on a repack: a note
    // gaining a link changes how many there are, so these cannot be patched.
    let edges = [], E = 0;
    let epos = new Float32Array(0), ebase = new Float32Array(0);
    // Two brightness snapshots per vertex — where the last ease had got to and
    // where it is heading. The shader mixes between them.
    let eFrom = new Float32Array(0), eTo = new Float32Array(0);
    let eFromAttr = null, eToAttr = null;

    function readEdges(src) {
      const edgeSet = new Set();
      edges = [];
      for (const [a, c] of (src.edges || [])) {
        if (a === c || a < 0 || c < 0 || a >= n || c >= n) continue;
        const key = a < c ? a * n + c : c * n + a;
        if (edgeSet.has(key)) continue;
        edgeSet.add(key);
        edges.push([a, c]);
      }
      E = edges.length;
      epos = new Float32Array(E * 6);
      ebase = new Float32Array(E * 6);
      eFrom = new Float32Array(E * 2).fill(1);
      eTo = new Float32Array(E * 2).fill(1);
      edges.forEach(([a, c], i) => {
        epos.set(pos.subarray(a * 3, a * 3 + 3), i * 6);
        epos.set(pos.subarray(c * 3, c * 3 + 3), i * 6 + 3);
        const m = local[a].si === local[c].si ? 0.7 : 0.5;
        for (let k = 0; k < 3; k++) {
          ebase[i * 6 + k] = col[a * 3 + k] * m;
          ebase[i * 6 + 3 + k] = col[c * 3 + k] * m;
        }
      });
      eFromAttr = new THREE.BufferAttribute(eFrom, 1);
      eToAttr = new THREE.BufferAttribute(eTo, 1);
    }

    readEdges(b);

    const group = new THREE.Group();
    group.position.fromArray(b.center);
    group.rotation.y = rand() * Math.PI * 2;
    scene.add(group);

    const bri = new Float32Array(n).fill(1), briT = new Float32Array(n).fill(1);

    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
    geo.setAttribute('aColor', new THREE.BufferAttribute(col, 3));
    geo.setAttribute('aSize', new THREE.BufferAttribute(siz, 1));
    const briAttr = new THREE.BufferAttribute(bri, 1);
    geo.setAttribute('aBright', briAttr);

    // Additive blending sums every overlapping sprite, so a dense shell washes
    // out to white. Fade the glow as density rises and the galaxy keeps its
    // colour and its individual stars.
    const density = n / Math.max(1, R * R);
    const glowGain = clamp(1.15 - density * 0.105, 0.2, 1.0);
    const uni = {
      uTime: uniforms.uTime, uScale: uniforms.uScale,
      uR: { value: R }, uGain: { value: glowGain },
    };
    const points = new THREE.Points(geo, new THREE.ShaderMaterial({
      uniforms: uni, vertexShader: VERT.replace('SIZEMUL', '1.0'),
      fragmentShader: FRAG_CORE, transparent: true, depthWrite: false,
    }));
    points.userData.offset = offset;
    group.add(new THREE.Points(geo, new THREE.ShaderMaterial({
      uniforms: uni, vertexShader: VERT.replace('SIZEMUL', '2.8'),
      fragmentShader: FRAG_GLOW, transparent: true, depthWrite: false,
      blending: THREE.AdditiveBlending, opacity: glowGain,
    })), points);

    const egeo = new THREE.BufferGeometry();
    // Chords cut through the middle of the shell, so a well-linked vault piles
    // thousands of them into the same few pixels and the core burns out. Thin
    // the lines as their count grows; the structure survives, the smear does not.
    const eMix = { value: 1 };
    const emat = new THREE.ShaderMaterial({
      uniforms: {
        uR: { value: R }, uMix: eMix,
        uOpacity: { value: opt.linkOpacity },
      },
      vertexShader: EDGE_VERT, fragmentShader: EDGE_FRAG,
      transparent: true, blending: THREE.AdditiveBlending, depthWrite: false,
    });

    /** Hand the current edge arrays to the GPU. Called again on every repack,
     *  because a changed link count means new buffers, not new contents. */
    function uploadEdges() {
      egeo.setAttribute('position', new THREE.BufferAttribute(epos, 3));
      egeo.setAttribute('aBase', new THREE.BufferAttribute(ebase, 3));
      egeo.setAttribute('aFrom', eFromAttr);
      egeo.setAttribute('aTo', eToAttr);
      const linkGain = clamp(Math.sqrt(1200 / Math.max(1, E)), 0.2, 1.0);
      emat.userData.gain = linkGain;
      emat.uniforms.uOpacity.value = opt.linkOpacity * linkGain;
    }
    uploadEdges();
    group.add(new THREE.LineSegments(egeo, emat));

    const hgeo = new THREE.BufferGeometry();
    hgeo.setAttribute('position', new THREE.BufferAttribute(new Float32Array(0), 3));
    hgeo.setAttribute('color', new THREE.BufferAttribute(new Float32Array(0), 3));
    group.add(new THREE.LineSegments(hgeo, new THREE.LineBasicMaterial({
      vertexColors: true, transparent: true, opacity: 0.9,
      blending: THREE.AdditiveBlending, depthWrite: false,
    })));
    const setHighlight = list => {
      const p = new Float32Array(list.length * 6), c = new Float32Array(list.length * 6);
      list.forEach((ei, i) => {
        p.set(epos.subarray(ei * 6, ei * 6 + 6), i * 6);
        const [a, b2] = edges[ei];
        for (let k = 0; k < 3; k++) {
          c[i * 6 + k] = Math.min(1, col[a * 3 + k] * 0.6 + 0.4);
          c[i * 6 + 3 + k] = Math.min(1, col[b2 * 3 + k] * 0.6 + 0.4);
        }
      });
      hgeo.setAttribute('position', new THREE.BufferAttribute(p, 3));
      hgeo.setAttribute('color', new THREE.BufferAttribute(c, 3));
    };

    const ringPts = new THREE.EllipseCurve(0, 0, R * 1.25, R * 1.12, 0, Math.PI * 2)
      .getPoints(160).map(p => new THREE.Vector3(p.x, p.y, 0));
    const ring = new THREE.LineLoop(
      new THREE.BufferGeometry().setFromPoints(ringPts),
      new THREE.LineBasicMaterial({ color: 0x35566a, transparent: true, opacity: 0.4 })
    );
    ring.rotation.set(rand() * Math.PI, rand() * Math.PI, 0);
    group.add(ring);
    // The core haze is sized off the shell, so on a big galaxy it covers a lot
    // of screen; fade it back or it reads as a grey ball instead of a glow.
    const core = new THREE.Sprite(new THREE.SpriteMaterial({
      map: glow, color: 0x2a4c66, transparent: true,
      opacity: 0.55 * clamp(8 / R, 0.28, 1),
      blending: THREE.AdditiveBlending, depthWrite: false,
    }));
    core.scale.setScalar(R * 1.4 * clamp(9 / R, 0.5, 1));
    group.add(core);

    const ripples = [];
    const ripple = li => {
      const s = new THREE.Sprite(new THREE.SpriteMaterial({
        map: ringT, color: b.sources[local[li].si].color, transparent: true,
        opacity: 0.9, blending: THREE.AdditiveBlending, depthWrite: false,
      }));
      s.position.fromArray(pos, li * 3);
      s.scale.setScalar(0.5);
      s.userData.t = 0;
      group.add(s); ripples.push(s);
    };

    // One label per ribbon: its most connected note.
    function pickLabels() {
      const labelIds = [];
      b.sources.forEach((_, si) => {
        const top = local.filter(x => x.si === si).sort((p, q) => q.deg - p.deg)[0];
        if (top) labelIds.push(top.gid);
      });
      const hubs = local.filter(x => x.hub).map(x => offset + x.li);
      return {
        labelIds,
        hubs: hubs.length ? hubs : local.slice(0, 4).map(x => offset + x.li),
      };
    }

    const handle = {
      cfg: b, bi, R, offset, n, local, edges, E, group, points, pos, col, bri, briT,
      briAttr, epos, ebase, eFrom, eTo, eFromAttr, eToAttr, eMix, eT: 0,
      emat, setHighlight, ripple,
      ripples, core, coreOpacity: core.material.opacity,
      spin: 0.7 + rand() * 0.6,
      ...pickLabels(),

      /** Refill this galaxy from a newer payload, in place.
       *
       * Only legal when the note count has not moved: the global ids the rest
       * of this module indexes by are positions in one flat array, so a vault
       * that gained a note shifts every vault after it. `update` checks that
       * before calling, and falls back to a rebuild when it cannot hold.
       *
       * Nothing about the view is touched — the group keeps its rotation, the
       * camera its target, the selection its brightness — so an edit lands as
       * a star changing size rather than the sky blinking.
       */
      repack(next, noteIds) {
        b = next;
        readNodes(next, noteIds);
        readEdges(next);
        uploadEdges();
        geo.attributes.position.needsUpdate = true;
        geo.attributes.aColor.needsUpdate = true;
        geo.attributes.aSize.needsUpdate = true;
        // Picking raycasts against the cached bounding sphere, which would
        // otherwise still describe where the notes used to be.
        geo.computeBoundingSphere();
        // The ease has nothing to ease from any more; recomputeTargets will
        // aim it again the moment the caller asks.
        eFrom.fill(1);
        eTo.fill(1);
        setHighlight([]);
        Object.assign(handle, {
          cfg: next, local, edges, E, pos, col, epos, ebase, eFrom, eTo,
          eFromAttr, eToAttr, ...pickLabels(),
        });
      },
    };
    return handle;
  }

  cfg.brains.forEach((b, bi) => brains.push(buildBrain(b, bi)));
  const N = allNodes.length;
  const nidToGid = new Map();
  allNodes.forEach(n => { if (n.nid >= 0) nidToGid.set(n.nid, n.gid); });

  const nodeWorld = (gid, out) => {
    const nd = allNodes[gid], b = brains[nd.bi];
    return out.set(b.pos[nd.li * 3], b.pos[nd.li * 3 + 1], b.pos[nd.li * 3 + 2])
      .applyMatrix4(b.group.matrixWorld);
  };

  // ---------- cross-brain links (real links that leave their vault)
  const cross = [];
  for (const [a, c] of (cfg.cross || [])) {
    if (a < 0 || c < 0 || a >= N || c >= N) continue;
    cross.push([a, c]);
    gadj[a].push(c); gadj[c].push(a);
  }
  let cpos = new Float32Array(cross.length * 6), ccol = new Float32Array(cross.length * 6);
  let cbri = new Float32Array(cross.length).fill(1);
  let cbriT = new Float32Array(cross.length).fill(1);
  const cgeo = new THREE.BufferGeometry();
  cgeo.setAttribute('position', new THREE.BufferAttribute(cpos, 3));
  cgeo.setAttribute('color', new THREE.BufferAttribute(ccol, 3));
  scene.add(new THREE.LineSegments(cgeo, new THREE.LineBasicMaterial({
    vertexColors: true, transparent: true, opacity: 0.18,
    blending: THREE.AdditiveBlending, depthWrite: false,
  })));

  /** Rebuild every adjacency and every arc between galaxies.
   *
   * A link is global — resolving one in a vault can attach it to a note in
   * another — so after a repack there is no safe way to patch a subset. One
   * pass over the whole edge list is a few milliseconds and cannot go wrong.
   */
  function relink(pairs) {
    for (let i = 0; i < N; i++) gadj[i].length = 0;
    for (const b of brains) {
      for (const [a, c] of b.edges) {
        gadj[b.offset + a].push(b.offset + c);
        gadj[b.offset + c].push(b.offset + a);
      }
    }
    cross.length = 0;
    for (const [a, c] of (pairs || [])) {
      if (a < 0 || c < 0 || a >= N || c >= N) continue;
      cross.push([a, c]);
      gadj[a].push(c); gadj[c].push(a);
    }
    if (cpos.length !== cross.length * 6) {
      cpos = new Float32Array(cross.length * 6);
      ccol = new Float32Array(cross.length * 6);
      cbri = new Float32Array(cross.length).fill(1);
      cbriT = new Float32Array(cross.length).fill(1);
      cgeo.setAttribute('position', new THREE.BufferAttribute(cpos, 3));
      cgeo.setAttribute('color', new THREE.BufferAttribute(ccol, 3));
    }
  }

  // ---------- stars
  {
    const rand = rng(5), sp = new Float32Array(1400 * 3);
    for (let i = 0; i < 1400; i++) {
      const v = new THREE.Vector3(rand() - 0.5, rand() - 0.5, rand() - 0.5)
        .normalize().multiplyScalar(160 + rand() * 200);
      sp.set([v.x, v.y, v.z], i * 3);
    }
    const sg = new THREE.BufferGeometry();
    sg.setAttribute('position', new THREE.BufferAttribute(sp, 3));
    scene.add(new THREE.Points(sg, new THREE.PointsMaterial({
      color: 0x9fb4cc, size: 0.7, transparent: true, opacity: 0.55, depthWrite: false,
    })));
  }

  // ---------- agents: stars in plasma clouds
  const agents = (cfg.agents || []).map((a, i) => {
    const rand = rng(1000 + i), color = new THREE.Color(a.color);
    const group = new THREE.Group();
    group.position.fromArray(a.pos);
    scene.add(group);
    const plasma = [];
    for (let k = 0; k < 14; k++) {
      const s = new THREE.Sprite(new THREE.SpriteMaterial({
        map: glow, color, transparent: true, opacity: 0.55 + rand() * 0.3,
        blending: THREE.AdditiveBlending, depthWrite: false,
      }));
      const base = new THREE.Vector3(rand() - 0.5, rand() - 0.5, rand() - 0.5)
        .multiplyScalar(3.6);
      const sc = 4 + rand() * 5;
      s.position.copy(base); s.scale.setScalar(sc);
      s.userData = { base, sc, f: 0.4 + rand() * 0.8, ph: rand() * 6.28, op: s.material.opacity };
      group.add(s); plasma.push(s);
    }
    const tint = new THREE.Sprite(new THREE.SpriteMaterial({
      map: glow, color, transparent: true, opacity: 0.85,
      blending: THREE.AdditiveBlending, depthWrite: false,
    }));
    tint.scale.setScalar(6);
    const core = new THREE.Sprite(new THREE.SpriteMaterial({
      map: starT, color: 0xffffff, transparent: true, opacity: 1,
      blending: THREE.AdditiveBlending, depthWrite: false,
    }));
    core.scale.setScalar(3.6);
    const hit = new THREE.Mesh(new THREE.SphereGeometry(2.4, 12, 8),
      new THREE.MeshBasicMaterial({ visible: false }));
    hit.userData.agent = i;
    group.add(tint, core, hit);
    const sparks = new Float32Array(24 * 3), sgeo = new THREE.BufferGeometry();
    sgeo.setAttribute('position', new THREE.BufferAttribute(sparks, 3));
    group.add(new THREE.Points(sgeo, new THREE.PointsMaterial({
      color, size: 0.35, transparent: true, opacity: 0.9,
      blending: THREE.AdditiveBlending, depthWrite: false,
    })));
    const orbits = Array.from({ length: 24 }, () => ({
      r: 1.4 + rand() * 1.6, sp: (0.4 + rand()) * (rand() < 0.5 ? 1 : -1),
      ph: rand() * 6.28, tilt: rand() * 3.14,
    }));
    return {
      cfg: a, i, group, plasma, tint, core, hit, sparks, sgeo, orbits,
      active: 0, activeT: 0, hover: false, bob: rand() * 6.28, burst: 0, busy: 0,
    };
  });

  // ---------- focus state
  let hover = -1, focusNode = -1, searchSet = null, brainFocus = null, hoverAgent = -1;
  const signalSet = new Map();

  const info = gid => {
    const nd = allNodes[gid], b = brains[nd.bi];
    return {
      gid, nid: nd.nid, name: nd.name, deg: nd.deg,
      brain: b.cfg.name, brainId: b.cfg.id,
      source: b.cfg.sources[nd.si].name, sourceId: b.cfg.sources[nd.si].id,
      color: b.cfg.sources[nd.si].color, links: gadj[gid].slice(),
    };
  };

  function recomputeTargets() {
    let active = null;
    const strong = new Set();
    if (hover >= 0) {
      active = new Set([hover, ...gadj[hover]]);
      strong.add(hover);
    } else if (focusNode >= 0 || signalSet.size || searchSet) {
      active = new Set();
      if (focusNode >= 0) {
        active.add(focusNode);
        gadj[focusNode].forEach(g => active.add(g));
        strong.add(focusNode);
      }
      for (const g of signalSet.keys()) { active.add(g); strong.add(g); }
      if (searchSet) for (const g of searchSet) {
        active.add(g);
        if (searchSet.size <= 80) strong.add(g);
      }
    }
    for (const b of brains) {
      const dimB = brainFocus != null && brainFocus !== b.bi ? 0.3 : 1;
      for (let i = 0; i < b.n; i++) {
        const g = b.offset + i;
        b.briT[i] = (!active ? 1 : strong.has(g) ? 1.7 : active.has(g) ? 1.1 : 0.1) * dimB;
      }
      // Fold however far the running ease got into the "from" snapshot, then
      // aim at the new one. This is the only pass over a brain's edges, and it
      // happens when the selection changes, not when a frame is drawn.
      const hl = [];
      freezeEase(b.eFrom, b.eTo, b.eMix.value);
      edgeBrightness(b.edges, b.offset, active, strong, dimB, b.eTo, hl);
      b.eFromAttr.needsUpdate = true;
      b.eToAttr.needsUpdate = true;
      b.eT = 0;
      b.eMix.value = 0;
      b.setHighlight(hl);
    }
    cross.forEach(([a, c], i) => {
      cbriT[i] = !active ? 1 : (active.has(a) && active.has(c) ? 1.7 : 0.08);
    });
  }

  // ---------- labels
  const labelEls = new Map();
  const mkLabel = (font = "500 12px Manrope, system-ui, sans-serif") => {
    const d = document.createElement('div');
    Object.assign(d.style, {
      position: 'absolute', left: '0', top: '0', font, color: '#e9f0f7',
      textShadow: '0 0 6px rgba(0,0,0,.95), 0 0 14px rgba(0,0,0,.8)',
      whiteSpace: 'nowrap', opacity: '0', transition: 'opacity .25s',
      willChange: 'transform', pointerEvents: 'none',
    });
    overlay.appendChild(d);
    return d;
  };
  const labelFor = gid => {
    let d = labelEls.get(gid);
    if (!d) {
      d = mkLabel();
      d.textContent = trim(allNodes[gid].name, 34);
      labelEls.set(gid, d);
    }
    return d;
  };
  const trim = (s, n) => (s && s.length > n ? s.slice(0, n - 1) + '…' : s || '');

  const hoverEl = mkLabel("600 13px Manrope, system-ui, sans-serif");
  const hoverName = document.createElement('div'), hoverSub = document.createElement('div');
  Object.assign(hoverSub.style, {
    font: '500 10px "JetBrains Mono", monospace', letterSpacing: '.12em',
    textTransform: 'uppercase', marginTop: '2px',
  });
  hoverEl.append(hoverName, hoverSub);

  // No cap any more, so the second line is just how many notes the vault holds.
  const brainLabel = (name, total) =>
    `<div>${escapeHtml(name)}</div>` +
    `<div style="font:500 10px 'JetBrains Mono',monospace;letter-spacing:.18em;color:#8fb3c4;margin-top:3px">${total} NOTES</div>`;
  const brainEls = brains.map(b => {
    const d = mkLabel("600 15px Manrope, system-ui, sans-serif");
    d.style.textAlign = 'center';
    d.style.letterSpacing = '-0.01em';
    d.innerHTML = brainLabel(b.cfg.name, b.cfg.total ?? b.n);
    return d;
  });
  const agentEls = agents.map(a => {
    const d = mkLabel("600 13px Manrope, system-ui, sans-serif");
    d.style.textAlign = 'center';
    d.innerHTML = `<div>${escapeHtml(a.cfg.name)}</div>` +
      `<div style="font:500 10px 'JetBrains Mono',monospace;letter-spacing:.16em;color:${a.cfg.color};margin-top:3px">${escapeHtml(a.cfg.protocol)}</div>`;
    return d;
  });

  const tv = new THREE.Vector3(), tv2 = new THREE.Vector3(), tmpVec = new THREE.Vector3();

  // Labels are placed in priority order and anything that would land on top of
  // one already placed is dropped for this frame. Without it, dense galaxies
  // stack three note titles into the same few pixels and none of them read.
  let taken = [];
  const PAD = 3;

  function placeWorld(elm, w, h, opacity, dy = -150, avoid = false) {
    tv.project(camera);
    const x = (tv.x * 0.5 + 0.5) * w, y = (-tv.y * 0.5 + 0.5) * h;

    if (avoid) {
      // offsetWidth is only non-zero once the element has been laid out, so a
      // label that has never been shown is let through on its first frame.
      const ew = elm.offsetWidth, eh = elm.offsetHeight;
      if (ew && eh) {
        const left = x - ew / 2;
        const top = y + (dy / 100) * eh;
        const box = [left - PAD, top - PAD, left + ew + PAD, top + eh + PAD];
        for (const other of taken) {
          if (box[0] < other[2] && box[2] > other[0] &&
              box[1] < other[3] && box[3] > other[1]) {
            elm.style.opacity = '0';
            return false;
          }
        }
        taken.push(box);
      }
    }

    elm.style.transform = `translate(${x}px, ${y}px) translate(-50%, ${dy}%)`;
    elm.style.opacity = opacity;
    return true;
  }

  function updateLabels() {
    const w = W(), h = H();
    taken = [];

    // Whatever names a place — the galaxies and the agents — is placed first
    // and always wins; note titles fill in around them.
    brains.forEach((b, i) => {
      tv.copy(b.group.position); tv.y += b.R + 1.2;
      placeWorld(brainEls[i], w, h,
        brainFocus != null && brainFocus !== i ? '0.3' : '0.9', -100, true);
    });
    agents.forEach((a, i) => {
      tv.copy(a.group.position); tv.y -= 2.6;
      placeWorld(agentEls[i], w, h, a.hover || a.active > 0.5 ? '1' : '0.7', 0, true);
    });

    if (hover >= 0) {
      const nd = allNodes[hover], b = brains[nd.bi];
      hoverName.textContent = trim(nd.name, 46);
      hoverSub.textContent =
        `${b.cfg.name} · ${b.cfg.sources[nd.si].name} · ${gadj[hover].length} links`;
      hoverSub.style.color = b.cfg.sources[nd.si].color;
      nodeWorld(hover, tv);
      placeWorld(hoverEl, w, h, '1', -150, true);
    } else hoverEl.style.opacity = '0';

    // Deliberate picks — the note in focus, notes an agent just cited, a small
    // enough set of search hits — before the ambient per-ribbon hub labels.
    const picked = [], ambient = [];
    if (focusNode >= 0) picked.push(focusNode);
    for (const g of signalSet.keys()) picked.push(g);
    if (searchSet && searchSet.size <= 30) searchSet.forEach(g => picked.push(g));
    for (const b of brains) {
      for (const g of (opt.showAllLabels ? b.hubs : b.labelIds)) ambient.push(g);
    }

    const show = new Set([...picked, ...ambient]);
    show.delete(hover);
    for (const [g, d] of labelEls) if (!show.has(g)) d.style.opacity = '0';

    const seen = new Set([hover]);
    for (const g of [...picked, ...ambient]) {
      if (seen.has(g)) continue;
      seen.add(g);
      const nd = allNodes[g], b = brains[nd.bi];
      nodeWorld(g, tv);
      tv2.copy(tv).applyMatrix4(camera.matrixWorldInverse);
      tv2.sub(tmpVec.copy(b.group.position).applyMatrix4(camera.matrixWorldInverse));
      const behind = tv2.z < -1.5, dim = b.bri[nd.li] < 0.5;
      placeWorld(labelFor(g), w, h, behind ? '0.12' : dim ? '0.25' : '1', -150, true);
    }
  }

  // ---------- camera
  const cam = {
    yaw: 0.12, pitch: 0.1, dist: 70, tyaw: 0.12, tpitch: 0.1,
    zoom: 1, base: 70, target: new THREE.Vector3(), tTarget: new THREE.Vector3(),
    pinned: null,
  };
  const fitDist = () => {
    const f = Math.tan(THREE.MathUtils.degToRad(camera.fov / 2));
    return Math.max((cfg.fitWidth ?? 60) / 2 / (f * camera.aspect), (cfg.fitHeight ?? 34) / 2 / f);
  };
  function placeCamera() {
    if (cam.pinned != null) nodeWorld(cam.pinned, cam.tTarget);
    cam.target.lerp(cam.tTarget, 0.06);
    // A focused galaxy needs room for the label above it, not just the shell.
    const want = (cam.pinned != null ? 19
      : brainFocus != null ? brains[brainFocus].R * 3.9
      : cam.base) * cam.zoom;
    cam.dist += (want - cam.dist) * 0.06;
    cam.yaw += (cam.tyaw - cam.yaw) * 0.08;
    cam.pitch += (cam.tpitch - cam.pitch) * 0.08;
    camera.position.set(
      Math.sin(cam.yaw) * Math.cos(cam.pitch) * cam.dist,
      Math.sin(cam.pitch) * cam.dist,
      Math.cos(cam.yaw) * Math.cos(cam.pitch) * cam.dist
    ).add(cam.target);
    camera.lookAt(cam.target);
  }

  // ---------- input
  const raycaster = new THREE.Raycaster();
  // Picking radius is in world units, so a fixed value that feels right up
  // close becomes sub-pixel once the camera pulls back to frame six galaxies.
  // Scale it with distance and a star stays clickable at any zoom.
  const pickThreshold = () => clamp(cam.dist / 190, 0.22, 1.3);
  const mouse = new THREE.Vector2();
  let pendingHover = false, dragging = false, moved = false, lx = 0, ly = 0, dragAgent = -1;
  const dragPlane = new THREE.Plane(), hitPt = new THREE.Vector3(), camDir = new THREE.Vector3();

  const setMouse = e => {
    const r = el.getBoundingClientRect();
    mouse.set(((e.clientX - r.left) / r.width) * 2 - 1,
      -((e.clientY - r.top) / r.height) * 2 + 1);
  };
  const agentAt = () => {
    raycaster.params.Points.threshold = pickThreshold();
    raycaster.setFromCamera(mouse, camera);
    const hits = raycaster.intersectObjects(agents.map(a => a.hit));
    return hits.length ? hits[0].object.userData.agent : -1;
  };
  const nodeAt = () => {
    raycaster.params.Points.threshold = pickThreshold();
    raycaster.setFromCamera(mouse, camera);
    const hits = raycaster.intersectObjects(brains.map(b => b.points));
    return hits.length ? hits[0].object.userData.offset + hits[0].index : -1;
  };
  const setHover = gid => {
    if (gid === hover) return;
    hover = gid;
    recomputeTargets();
    cfg.onHover?.(gid >= 0 ? info(gid) : null);
  };
  const setHoverAgent = i => {
    if (i === hoverAgent) return;
    if (hoverAgent >= 0) agents[hoverAgent].hover = false;
    hoverAgent = i;
    if (i >= 0) agents[i].hover = true;
    cfg.onAgentHover?.(i >= 0 ? agents[i].cfg : null);
  };

  const onDown = e => {
    setMouse(e);
    dragging = true; moved = false; lx = e.clientX; ly = e.clientY;
    const ai = agentAt();
    if (ai >= 0) {
      dragAgent = ai;
      camera.getWorldDirection(camDir);
      dragPlane.setFromNormalAndCoplanarPoint(camDir, agents[ai].group.position);
      el.style.cursor = 'grabbing';
      el.setPointerCapture?.(e.pointerId);
    }
  };
  const onUp = e => {
    if (dragging && !moved && e.target === el) {
      if (dragAgent >= 0) cfg.onAgentClick?.(agents[dragAgent].cfg);
      else if (hover >= 0) cfg.onNodeClick?.(info(hover));
      else cfg.onEmptyClick?.();
    }
    if (dragAgent >= 0 && moved) {
      cfg.onAgentMove?.(agents[dragAgent].cfg, agents[dragAgent].group.position.toArray());
    }
    dragging = false; dragAgent = -1;
    el.style.cursor = hover >= 0 || hoverAgent >= 0 ? 'pointer' : 'grab';
  };
  const onMove = e => {
    setMouse(e);
    if (dragging) {
      const dx = e.clientX - lx, dy = e.clientY - ly;
      lx = e.clientX; ly = e.clientY;
      if (Math.abs(dx) + Math.abs(dy) > 1) moved = true;
      if (dragAgent >= 0) {
        raycaster.setFromCamera(mouse, camera);
        if (raycaster.ray.intersectPlane(dragPlane, hitPt)) {
          agents[dragAgent].group.position.copy(hitPt);
        }
        return;
      }
      cam.tyaw -= dx * 0.005;
      cam.tpitch = clamp(cam.tpitch + dy * 0.005, -1.2, 1.2);
      return;
    }
    pendingHover = true;
  };
  const onLeave = () => { setHover(-1); setHoverAgent(-1); };
  const onWheel = e => {
    e.preventDefault();
    cam.zoom = clamp(cam.zoom * (1 + e.deltaY * 0.001), 0.25, 2.2);
  };
  const onDbl = e => {
    setMouse(e);
    const ai = agentAt();
    if (ai >= 0) cfg.onAgentOpen?.(agents[ai].cfg);
    else if (hover >= 0) cfg.onNodeOpen?.(info(hover));
  };
  el.addEventListener('pointerdown', onDown);
  window.addEventListener('pointerup', onUp);
  el.addEventListener('pointermove', onMove);
  el.addEventListener('pointerleave', onLeave);
  el.addEventListener('wheel', onWheel, { passive: false });
  el.addEventListener('dblclick', onDbl);

  // ---------- signals (agent → notes)
  const travelers = [], tA = new THREE.Vector3(), tB = new THREE.Vector3();
  function signal(agentId, gids) {
    const a = agents.find(x => x.cfg.id === agentId);
    if (!a) return;
    a.burst = 1;
    gids.filter(g => g >= 0 && g < N).forEach((g, i) => {
      const s = new THREE.Sprite(new THREE.SpriteMaterial({
        map: glow, color: a.cfg.color, transparent: true,
        blending: THREE.AdditiveBlending, depthWrite: false,
      }));
      s.scale.setScalar(1.3);
      scene.add(s);
      travelers.push({ s, gid: g, from: a.group.position.clone(), t: -i * 0.18, dur: 1.3 });
    });
  }

  // ---------- loop
  const clock = new THREE.Clock();
  let time = 0, raf = 0;

  function tick() {
    raf = requestAnimationFrame(tick);
    const dt = Math.min(clock.getDelta(), 0.05);
    time += dt;
    uniforms.uTime.value = time;
    placeCamera();
    camera.updateMatrixWorld();

    for (const b of brains) {
      b.group.rotation.y += opt.rotationSpeed * 0.1 * b.spin * dt;
      b.group.rotation.x = 0.1 * Math.sin(time * 0.06 + b.bi);
      b.group.updateMatrixWorld();

      let dirty = false;
      for (let i = 0; i < b.n; i++) {
        const d = b.briT[i] - b.bri[i];
        if (Math.abs(d) > 0.001) { b.bri[i] += d * 0.1; dirty = true; }
      }
      if (dirty) b.briAttr.needsUpdate = true;

      // All a frame owes the edges is how far along the ease is; the shader
      // does the shading and the depth fade.
      if (b.eMix.value < 1) {
        b.eT += dt;
        b.eMix.value = easeMix(b.eT);
      }
      b.core.material.opacity = b.coreOpacity * (0.85 + 0.17 * Math.sin(time * 0.7 + b.bi));

      for (let i = b.ripples.length - 1; i >= 0; i--) {
        const s = b.ripples[i];
        s.userData.t += dt;
        const t = s.userData.t;
        s.scale.setScalar(0.5 + t * 4.5);
        s.material.opacity = 0.9 * Math.max(0, 1 - t / 1.1);
        if (t > 1.1) { b.group.remove(s); s.material.dispose(); b.ripples.splice(i, 1); }
      }
    }

    cross.forEach(([a, c], i) => {
      nodeWorld(a, tA); nodeWorld(c, tB);
      cpos.set([tA.x, tA.y, tA.z, tB.x, tB.y, tB.z], i * 6);
      cbri[i] += (cbriT[i] - cbri[i]) * 0.1;
      const f = cbri[i] * 0.5;
      for (let k = 0; k < 6; k++) ccol[i * 6 + k] = 0.5 * f + (k % 3 === 2 ? 0.2 * f : 0);
    });
    if (cross.length) {
      cgeo.attributes.position.needsUpdate = true;
      cgeo.attributes.color.needsUpdate = true;
    }

    for (const a of agents) {
      a.active += ((a.activeT || a.hover ? 1 : 0) - a.active) * 0.05;
      a.burst = Math.max(0, a.burst - dt * 0.8);
      const think = a.busy ? 0.35 + 0.35 * Math.sin(time * 4) : 0;
      const boost = 1 + a.active * 0.4 + a.burst * 0.6 + think;
      a.group.position.y += Math.sin(time * 0.5 + a.bob) * 0.0015;
      for (const p of a.plasma) {
        const u = p.userData;
        p.position.set(
          u.base.x + 0.5 * Math.sin(time * u.f + u.ph),
          u.base.y + 0.5 * Math.cos(time * u.f * 0.8 + u.ph),
          u.base.z + 0.4 * Math.sin(time * u.f * 1.3 + u.ph * 2)
        );
        p.scale.setScalar(u.sc * boost * (1 + 0.12 * Math.sin(time * u.f * 2 + u.ph)));
        p.material.opacity = u.op * (0.8 + 0.3 * Math.sin(time * u.f * 3 + u.ph))
          * (1 + a.active * 0.6 + a.burst);
      }
      a.core.scale.setScalar(3.6 * boost * (1 + 0.08 * Math.sin(time * (3 + a.active * 3))));
      a.core.material.rotation = time * 0.15;
      a.tint.scale.setScalar(6 * boost);
      a.orbits.forEach((o, k) => {
        const ang = time * o.sp * (1 + a.busy * 1.6) + o.ph;
        a.sparks.set([
          Math.cos(ang) * o.r,
          Math.sin(ang) * o.r * Math.cos(o.tilt),
          Math.sin(ang) * o.r * Math.sin(o.tilt),
        ], k * 3);
      });
      a.sgeo.attributes.position.needsUpdate = true;
    }

    for (let i = travelers.length - 1; i >= 0; i--) {
      const t = travelers[i];
      t.t += dt;
      if (t.t < 0) { t.s.visible = false; continue; }
      t.s.visible = true;
      const p = Math.min(1, t.t / t.dur), e = ease(p);
      nodeWorld(t.gid, tB);
      t.s.position.lerpVectors(t.from, tB, e);
      t.s.position.y += Math.sin(p * Math.PI) * 2.5;
      t.s.scale.setScalar(0.9 + 0.8 * Math.sin(p * Math.PI));
      if (p >= 1) {
        const nd = allNodes[t.gid];
        brains[nd.bi].ripple(nd.li);
        signalSet.set(t.gid, time + 10);
        scene.remove(t.s); t.s.material.dispose();
        travelers.splice(i, 1);
        recomputeTargets();
      }
    }

    let expired = false;
    for (const [g, until] of signalSet) if (time > until) { signalSet.delete(g); expired = true; }
    if (expired) recomputeTargets();

    if (pendingHover) {
      pendingHover = false;
      const ai = agentAt();
      setHoverAgent(ai);
      setHover(ai >= 0 ? -1 : nodeAt());
      if (!dragging) el.style.cursor = hover >= 0 || ai >= 0 ? 'pointer' : 'grab';
    }
    updateLabels();
    renderer.render(scene, camera);
  }

  function resize() {
    const w = W(), h = H();
    renderer.setSize(w, h, false);
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
    uniforms.uScale.value = h * renderer.getPixelRatio() * 0.95;
    cam.base = fitDist();
  }
  const ro = new ResizeObserver(resize);
  ro.observe(container);
  resize();
  cam.dist = cam.base;
  recomputeTargets();
  tick();

  return {
    nodeCount: N,
    nodes: () => allNodes.map(n => info(n.gid)),
    gidForNote: nid => (nidToGid.has(nid) ? nidToGid.get(nid) : -1),
    noteForGid: gid => (gid >= 0 && gid < N ? allNodes[gid].nid : -1),
    infoFor: gid => (gid >= 0 && gid < N ? info(gid) : null),
    brains: brains.map(b => ({
      id: b.cfg.id, name: b.cfg.name, notes: b.n,
      total: b.cfg.total ?? b.n, links: b.E, color: b.cfg.sources[0]?.color,
    })),

    /** Fold a newer `/api/universe` payload in without rebuilding the scene.
     *
     * Returns true when it was applied. False means the change is structural
     * — a vault added, removed, reordered, or one whose note count moved —
     * and the caller has to build a fresh universe, because the global ids
     * everything here indexes by are about to shift.
     *
     * The view is deliberately untouched: camera, focus, hover, search
     * highlight and each galaxy's rotation all survive, so an edit shows up
     * as one star changing and nothing else moving.
     */
    update(next) {
      const incoming = next?.brains || [];
      if (incoming.length !== brains.length) return false;
      const moved = [];
      for (let i = 0; i < brains.length; i++) {
        const b = brains[i], nb = incoming[i];
        const count = nb.sizes?.length ?? Math.floor((nb.positions?.length || 0) / 3);
        if (nb.id !== b.cfg.id || count !== b.n) return false;
        if ((nb.revision ?? 0) !== (b.cfg.revision ?? 0)) moved.push(i);
      }
      if (!moved.length) return true;

      const noteIds = next.noteIds || [];
      for (const i of moved) brains[i].repack(incoming[i], noteIds);
      relink(next.cross);

      nidToGid.clear();
      for (const nd of allNodes) if (nd.nid >= 0) nidToGid.set(nd.nid, nd.gid);
      // A renamed note keeps its label element, so the text has to follow it.
      for (const [gid, d] of labelEls) d.textContent = trim(allNodes[gid].name, 34);
      for (const i of moved) {
        const b = brains[i];
        brainEls[i].innerHTML = brainLabel(b.cfg.name, b.cfg.total ?? b.n);
      }
      recomputeTargets();
      return true;
    },

    /** Light up an explicit set of nodes — used for server-side search results. */
    highlight(gids) {
      searchSet = gids && gids.length ? new Set(gids.filter(g => g >= 0 && g < N)) : null;
      recomputeTargets();
      return searchSet ? searchSet.size : 0;
    },

    /** Client-side title match, for instant feedback while the server catches up. */
    quickSearch(q) {
      q = (q || '').trim().toLowerCase();
      if (!q) { searchSet = null; recomputeTargets(); return []; }
      const out = [];
      for (const nd of allNodes) if (nd.name.toLowerCase().includes(q)) out.push(nd.gid);
      searchSet = new Set(out);
      recomputeTargets();
      return out;
    },

    focusNode(gid) {
      focusNode = gid ?? -1;
      if (gid >= 0 && gid < N) {
        const nd = allNodes[gid];
        brains[nd.bi].ripple(nd.li);
      } else {
        // Clearing the focus also releases a camera pinned by flyTo, so
        // clicking empty space always gets you back out.
        cam.pinned = null;
      }
      recomputeTargets();
    },

    /** Pin the camera on one note and pull in close. */
    flyTo(gid) {
      if (gid < 0 || gid >= N) return;
      cam.pinned = gid;
      cam.zoom = 1;
      brainFocus = null;
      focusNode = gid;
      brains[allNodes[gid].bi].ripple(allNodes[gid].li);
      recomputeTargets();
    },

    focusBrain(id) {
      const b = brains.find(x => x.cfg.id === id);
      brainFocus = b ? b.bi : null;
      cam.pinned = null;
      cam.tTarget.copy(b ? b.group.position : new THREE.Vector3());
      recomputeTargets();
    },

    setAgentActive(id) { agents.forEach(a => { a.activeT = a.cfg.id === id ? 1 : 0; }); },
    setAgentBusy(id, busy) { agents.forEach(a => { if (a.cfg.id === id) a.busy = busy ? 1 : 0; }); },
    agentPosition(id) {
      const a = agents.find(x => x.cfg.id === id);
      return a ? a.group.position.toArray() : null;
    },
    signal,

    resetView() {
      cam.tyaw = 0.12; cam.tpitch = 0.1; cam.zoom = 1; cam.pinned = null;
      brainFocus = null; focusNode = -1; searchSet = null;
      cam.tTarget.set(0, 0, 0);
      signalSet.clear();
      recomputeTargets();
    },

    setOptions(o) {
      Object.assign(opt, o);
      brains.forEach(b => {
        b.emat.uniforms.uOpacity.value =
          opt.linkOpacity * (b.emat.userData.gain ?? 1);
      });
      recomputeTargets();
    },

    dispose() {
      cancelAnimationFrame(raf);
      ro.disconnect();
      el.removeEventListener('pointerdown', onDown);
      window.removeEventListener('pointerup', onUp);
      el.removeEventListener('pointermove', onMove);
      el.removeEventListener('pointerleave', onLeave);
      el.removeEventListener('wheel', onWheel);
      el.removeEventListener('dblclick', onDbl);
      renderer.dispose();
      el.remove();
      overlay.remove();
    },
  };
}

function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"]/g, c =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}
