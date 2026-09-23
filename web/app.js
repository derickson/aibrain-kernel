// The UI around the universe: agent chat on the left, the galaxies in the
// middle, notes on the right, and a drawer for the things that configure them.
//
// No framework and no build step — the server hands this file straight to the
// browser. State lives in one object and every change goes through render().

import { createUniverse } from './universe.js';

// ───────────────────────────── helpers ──────────────────────────────────

const $ = sel => document.querySelector(sel);
const el = (tag, props = {}, ...kids) => {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === 'class') node.className = v;
    else if (k === 'html') node.innerHTML = v;
    else if (k.startsWith('on')) node.addEventListener(k.slice(2).toLowerCase(), v);
    else if (v === true) node.setAttribute(k, '');
    else if (v !== false && v != null) node.setAttribute(k, v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    node.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return node;
};
const esc = s => String(s ?? '').replace(/[&<>"]/g, c =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
// Snippets arrive with <mark> already in them; keep those and escape the rest.
const safeSnippet = s => esc(s).replace(/&lt;mark&gt;/g, '<mark>').replace(/&lt;\/mark&gt;/g, '</mark>');
const debounce = (fn, ms) => {
  let t;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
};

async function api(path, options) {
  const response = await fetch(path, {
    headers: { 'Content-Type': 'application/json' },
    ...options,
    body: options?.body ? JSON.stringify(options.body) : undefined,
  });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || `${response.status} ${path}`);
  return data;
}

function toast(message, bad = false) {
  const node = el('div', { class: `toast${bad ? ' bad' : ''}` }, message);
  $('#toasts').append(node);
  setTimeout(() => {
    node.style.transition = 'opacity .3s';
    node.style.opacity = '0';
    setTimeout(() => node.remove(), 320);
  }, bad ? 6000 : 3200);
}

function timeAgo(seconds) {
  if (!seconds) return '';
  const delta = Date.now() / 1000 - seconds;
  const steps = [[60, 's'], [3600, 'm'], [86400, 'h'], [604800, 'd'], [2629800, 'w']];
  if (delta < 60) return 'just now';
  for (let i = 1; i < steps.length; i++) {
    if (delta < steps[i][0]) return `${Math.floor(delta / steps[i - 1][0])}${steps[i - 1][1]} ago`;
  }
  return `${Math.floor(delta / 2629800)}mo ago`;
}

// ───────────────────────────── state ────────────────────────────────────

const S = {
  universe: null,      // the three.js handle
  data: null,          // /api/universe payload
  status: null,        // /api/status payload
  leftOpen: false,
  rightOpen: false,
  agentId: null,
  chats: {},           // agentId -> [{role, text, cites, trace}]
  streaming: null,     // AbortController while an answer is in flight
  note: null,
  noteFrom: null,
  results: [],
  searchMeta: null,    // {engine, ms} for the last /api/search response
  query: '',
  brainFocus: null,
  hover: null,
  agentHover: null,
  statusText: null,
  drawerTab: 'brains',
  jobStreams: new Map(),
  etag: '',            // what /api/universe last handed us
  live: null,          // the /api/events subscription, while it is open
};

// ───────────────────────────── boot ─────────────────────────────────────

async function boot() {
  wireStaticHandlers();
  try {
    S.status = await api('/api/status');
    $('#brand').textContent = S.status.title || 'AI Brains';
    document.title = S.status.title || 'AI Brains';
  } catch (err) {
    bootText('CANNOT REACH THE SERVER', String(err));
    return;
  }

  if (!S.status.notes) {
    // First run: the server is indexing. Follow whichever job is doing it.
    bootText('READING YOUR BRAINS', 'Indexing your vaults for the first time.');
    await followIndexJob();
  }
  await loadUniverse();
  subscribeEvents();
}

function bootText(title, sub) {
  $('#boot-text').textContent = title;
  if (sub != null) $('#boot-sub').textContent = sub;
}

async function followIndexJob() {
  for (let attempt = 0; attempt < 40; attempt++) {
    const { jobs } = await api('/api/jobs');
    const job = jobs.find(j => j.name === 'Reindex');
    if (job && job.status === 'running') {
      await new Promise(resolve => {
        streamSSE(`/api/stream/job/${job.id}`, event => {
          if (event.type === 'line' && event.text) bootText(null, event.text.trim().slice(0, 90));
          if (event.type === 'done') resolve();
        }, resolve);
      });
      return;
    }
    if (job && job.status !== 'running') return;
    await new Promise(r => setTimeout(r, 400));
  }
}

/** GET /api/universe, conditionally. `null` means the server said 304. */
async function fetchUniverse(rebuild = false) {
  const headers = {};
  if (!rebuild && S.etag) headers['If-None-Match'] = S.etag;
  const response = await fetch(`/api/universe${rebuild ? '?rebuild=1' : ''}`, { headers });
  if (response.status === 304) return null;
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || `${response.status} /api/universe`);
  S.etag = response.headers.get('ETag') || '';
  return data;
}

async function loadUniverse(rebuild = false) {
  S.etag = '';
  const data = await fetchUniverse(rebuild);
  S.data = data;

  if (S.universe) S.universe.dispose();
  const container = $('#canvas');
  container.innerHTML = '';

  if (!data.brains.length) {
    $('#boot').classList.remove('fading');
    $('#boot').hidden = false;
    $('.spinner').style.display = 'none';
    bootText('NO BRAINS CONFIGURED',
      'Open the menu (☰) and point this at an Obsidian vault.');
    renderChips();
    renderDrawer();
    return;
  }

  S.universe = createUniverse(container, {
    ...data,
    onHover: node => { S.hover = node; renderStatus(); },
    onAgentHover: agent => { S.agentHover = agent; renderStatus(); },
    onNodeClick: node => openNote(node.nid, null),
    onNodeOpen: node => { openNote(node.nid, null); S.universe.flyTo(node.gid); },
    onAgentOpen: agent => openChat(agent.id),
    onAgentClick: agent => {
      S.statusText = `${agent.name} · double-click to chat, drag to move`;
      renderStatus();
    },
    onAgentMove: (agent, pos) => {
      api(`/api/agent/${agent.id}/move`, { method: 'POST', body: { pos } }).catch(() => {});
    },
    onEmptyClick: () => {
      S.universe.focusNode(-1);
      S.statusText = null;
      renderStatus();
    },
  });

  $('#boot').classList.add('fading');
  setTimeout(() => { $('#boot').hidden = true; }, 520);

  renderChips();
  renderAgentRail();
  renderStatus();
  loadRecent();
  if (S.agentId) renderChat();
}

// ───────────────────────────── live updates ─────────────────────────────
//
// The server knows the moment a note on disk changes; before this the browser
// found out by being reloaded. /api/events carries one frame per change, and
// the response to it is not a rebuild but a conditional GET: an unchanged
// corpus is a 304, and a changed one patches only the vaults whose revision
// moved, leaving the camera, the selection and the open note exactly as they
// were.

/** Longest we wait between reconnection attempts. */
const LIVE_MAX_BACKOFF = 15000;
/** A burst of saves is one refetch, not one per file. */
const LIVE_SETTLE = 250;

let liveBackoff = 500;
let liveTimer = null;
let livePending = null;
let liveBurst = null;
let liveGeneration = 0;

function subscribeEvents() {
  // Aborting the old stream fires its close handler asynchronously, after the
  // new one is already in hand — so a stale handler has to know to say nothing
  // rather than clear the subscription that replaced it.
  const mine = ++liveGeneration;
  S.live?.();
  S.live = streamSSE('/api/events', onLiveEvent, () => {
    if (mine !== liveGeneration) return;
    S.live = null;
    // The stream ends on a server restart, a proxy timeout, or a laptop
    // waking up. Keep trying, but back off so a service that is down does
    // not get hammered.
    clearTimeout(liveTimer);
    liveTimer = setTimeout(subscribeEvents, liveBackoff);
    liveBackoff = Math.min(liveBackoff * 2, LIVE_MAX_BACKOFF);
  });
}

function onLiveEvent(event) {
  // A ping is the stream saying it is still there; an error frame is the
  // reader's own, and the close handler is about to reconnect anyway.
  if (!event || event.type === 'ping') { liveBackoff = 500; return; }
  if (event.type === 'error') return;
  liveBackoff = 500;
  // A save arrives as one `note` frame then a `brain` frame; the first is the
  // one that can say which note, so it is the one worth keeping.
  if (!liveBurst || (event.kind === 'note' && liveBurst.kind !== 'note')) liveBurst = event;
  clearTimeout(livePending);
  livePending = setTimeout(() => {
    const burst = liveBurst;
    liveBurst = null;
    refreshUniverse(burst).catch(() => {});
  }, LIVE_SETTLE);
}

async function refreshUniverse(event) {
  const data = await fetchUniverse();
  if (!data) return;                       // 304: nothing moved after all
  const previous = S.data;
  S.data = data;

  // `update` patches the galaxies whose revision moved and keeps everything
  // else; it declines when a vault was added, removed or changed size, and
  // then there is no way around building the scene again.
  if (!S.universe || !S.universe.update(data)) {
    S.data = previous;
    await loadUniverse();
    return;
  }

  renderChips();
  renderStatus();
  loadRecent();
  refreshStatus().then(renderStatus).catch(() => {});
  // The results list holds titles and snippets that may have just changed.
  if (S.query.trim()) runSearch(S.query);
  if (S.note && event?.note_id === S.note.nid) openNote(S.note.nid, S.noteFrom);
  pulseStatus(event);
}

/** A two-second note in the status bar. Quieter than a toast on purpose:
 *  a vault being watched should feel like weather, not like an alert. */
let pulseTimer = null;
function pulseStatus(event) {
  const brain = S.data?.brains?.find(b => b.id === event?.brain_id);
  S.statusText = event?.kind === 'note' && brain
    ? `${brain.name} · a note just changed`
    : 'updated';
  renderStatus();
  clearTimeout(pulseTimer);
  pulseTimer = setTimeout(() => {
    if (S.statusText === 'updated' || S.statusText?.endsWith('a note just changed')) {
      S.statusText = null;
      renderStatus();
    }
  }, 2000);
}

// ───────────────────────────── universe chrome ──────────────────────────

function renderChips() {
  const chips = $('#chips');
  chips.innerHTML = '';
  for (const brain of (S.data?.brains || [])) {
    const on = S.brainFocus === brain.id;
    const color = brain.color || 'var(--accent)';
    chips.append(el('button', {
      class: 'chip',
      'aria-pressed': on ? 'true' : 'false',
      title: `${brain.total ?? 0} notes · ${brain.path}`,
      onclick: () => {
        S.brainFocus = on ? null : brain.id;
        S.universe?.focusBrain(S.brainFocus);
        renderChips();
        renderStatus();
        // The scope changed under an open query, so the results must follow.
        if (S.query.trim()) runSearch(S.query);
      },
    },
      el('span', { class: 'pip', style: `background:${color};box-shadow:0 0 8px ${color}` }),
      el('span', {}, brain.name),
      el('span', { class: 'count' }, String(brain.total ?? 0))
    ));
  }
}

function renderAgentRail() {
  const rail = $('#agent-rail');
  rail.innerHTML = '';
  for (const agent of (S.data?.agents || [])) {
    rail.append(el('button', {
      class: `rail-btn${S.streaming?.agentId === agent.id ? ' busy' : ''}`,
      id: `rail-${agent.id}`,
      onclick: () => openChat(agent.id),
      title: `Chat with ${agent.name}`,
    },
      el('span', { class: 'pip', style: `background:${agent.color};color:${agent.color}` }),
      el('span', {}, agent.name),
      el('span', { class: 'proto' }, agent.protocol)
    ));
  }
}

function renderStatus() {
  const dot = $('#status-dot'), label = $('#status-label');
  const stats = S.data?.stats;
  let text, color = 'var(--accent)';

  if (S.agentHover) {
    text = `${S.agentHover.name} · ${S.agentHover.protocol} · double-click to chat · drag to move`;
    color = S.agentHover.color;
  } else if (S.hover) {
    text = `${S.hover.name} · ${S.hover.brain} · ${S.hover.source} · ${S.hover.links.length} links`;
    color = S.hover.color;
  } else if (S.statusText) {
    text = S.statusText;
  } else if (stats) {
    // Every note is drawn now, so there is no "shown of total" to report.
    text = `${stats.brains} brains · ${stats.notes.toLocaleString()} notes · `
      + `${stats.cross} cross-links · ${stats.agents} agents`;
  } else {
    text = 'starting…';
  }
  label.textContent = text;
  dot.style.background = color;
  dot.style.color = color;
}

// ───────────────────────────── notes panel ──────────────────────────────

function openRight() {
  S.rightOpen = true;
  $('#right').dataset.open = 'true';
}

function closeRight() {
  S.rightOpen = false;
  $('#right').dataset.open = 'false';
  S.note = null;
  S.universe?.focusNode(-1);
}

async function openNote(nid, from) {
  if (nid == null || nid < 0) return;
  openRight();
  try {
    const note = await api(`/api/note/${nid}`);
    S.note = note;
    S.noteFrom = from ?? (S.results.length ? 'results' : null);
    renderNote();
    if (note.gid != null && note.gid >= 0) S.universe?.focusNode(note.gid);
  } catch (err) {
    toast(String(err.message || err), true);
  }
}

function renderNote() {
  const note = S.note;
  $('#empty-view').hidden = true;
  $('#results-view').hidden = true;
  $('#note-view').hidden = !note;
  if (!note) return;

  const color = colorForBrain(note.brainId);
  $('#note-dot').style.background = color;
  $('#note-dot').style.color = color;
  $('#note-crumb').textContent = `${note.brain} · ${note.source}`;
  $('#back-results').hidden = !(S.noteFrom === 'results' && S.results.length);

  const bits = [
    `${note.words.toLocaleString()} words`,
    `${note.degree} links`,
    timeAgo(note.mtime),
    note.relPath,
  ].filter(Boolean);
  $('#note-meta').textContent = bits.join('  ·  ');

  const body = $('#note-body');
  body.innerHTML = `<h1>${esc(note.name)}</h1>` + note.html;
  body.onclick = event => {
    const link = event.target.closest('a.wiki[data-note]');
    if (link) {
      event.preventDefault();
      openNote(Number(link.dataset.note), S.noteFrom);
    }
  };

  $('#linked-label').textContent =
    `LINKED NOTES · ${note.linked.length}${note.unresolved.length ? ` · ${note.unresolved.length} UNRESOLVED` : ''}`;
  const linked = $('#linked');
  linked.innerHTML = '';
  for (const item of note.linked) {
    linked.append(noteRow(item, () => openNote(item.nid, S.noteFrom)));
  }
  for (const target of note.unresolved) {
    linked.append(el('div', { class: 'row', style: 'opacity:.5;cursor:default' },
      el('span', { class: 'pip', style: 'background:var(--fg-mute)' }),
      el('span', { class: 'col' },
        el('div', { class: 'nm' }, target),
        el('div', { class: 'mt' }, 'NOT IN THE INDEX'))
    ));
  }
  $('#right-body').scrollTop = 0;
}

function noteRow(item, onClick) {
  const color = item.color || colorForBrain(item.brainId) || 'var(--accent)';
  const row = el('button', { class: 'row', onclick: onClick },
    el('span', { class: 'pip', style: `background:${color}` }),
    el('span', { class: 'col' },
      el('div', { class: 'nm', title: item.name }, item.name),
      el('div', { class: 'mt' },
        [item.brain, item.source, item.degree ? `${item.degree} links` : null]
          .filter(Boolean).join(' · '))
    )
  );
  if (item.snippet) {
    row.querySelector('.col').append(
      el('div', { class: 'sn', html: safeSnippet(item.snippet) })
    );
  }
  // Hovering a result lights its star, its links, and their titles up in the
  // universe — the same treatment a real pointer hover over the node gets.
  if (item.gid != null && item.gid >= 0) {
    row.addEventListener('mouseenter', () => S.universe?.hoverNode(item.gid));
    row.addEventListener('mouseleave', () => S.universe?.hoverNode(-1));
  }
  return row;
}

function colorForBrain(brainId) {
  const brain = S.data?.brains.find(b => b.id === brainId);
  return brain?.color || '#7fd8e8';
}

const runSearch = debounce(async text => {
  if (!text.trim()) {
    S.results = [];
    S.searchMeta = null;
    S.universe?.highlight(null);
    renderResults();
    return;
  }
  try {
    // Focusing a brain scopes the search to it, so the results and the lit
    // stars agree about which galaxy you are looking at.
    const scope = S.brainFocus ? `&brains=${encodeURIComponent(S.brainFocus)}` : '';
    const started = performance.now();
    const data = await api(`/api/search?q=${encodeURIComponent(text)}&limit=80${scope}`);
    if (data.query !== $('#query').value) return;   // a newer keystroke won
    S.results = data.results;
    S.searchMeta = { engine: data.engine || '', ms: Math.round(performance.now() - started) };
    S.universe?.highlight(data.results.map(r => r.gid).filter(g => g != null));
    renderResults();
  } catch (err) {
    toast(String(err.message || err), true);
  }
}, 160);

function renderResults() {
  S.note = null;
  $('#note-view').hidden = true;
  const any = S.results.length > 0, searching = !!S.query.trim();
  $('#results-view').hidden = !searching;
  $('#empty-view').hidden = searching;
  if (!searching) return;

  const brain = S.data?.brains.find(b => b.id === S.brainFocus);
  const scope = brain ? ` IN ${brain.name.toUpperCase()}` : '';
  $('#results-label').textContent = any
    ? `${S.results.length} RESULT${S.results.length === 1 ? '' : 'S'} FOR “${S.query.toUpperCase()}”${scope}`
    : `NOTHING MATCHES “${S.query.toUpperCase()}”${scope}`;
  const meta = $('#results-meta');
  if (S.searchMeta?.engine) {
    meta.hidden = false;
    meta.dataset.engine = S.searchMeta.engine;
    meta.innerHTML = `<span class="engine">${esc(S.searchMeta.engine.toUpperCase())}</span> · ${S.searchMeta.ms}ms`;
  } else {
    meta.hidden = true;
  }
  const list = $('#results');
  list.innerHTML = '';
  for (const item of S.results) {
    list.append(noteRow(item, () => openNote(item.nid, 'results')));
  }
}

async function loadRecent() {
  try {
    const { results } = await api('/api/recent?limit=14');
    const list = $('#recent');
    list.innerHTML = '';
    for (const item of results) list.append(noteRow(item, () => openNote(item.nid, null)));
  } catch { /* the panel is still usable without it */ }
}

// ───────────────────────────── chat ─────────────────────────────────────

function agentById(id) {
  return (S.data?.agents || []).find(a => a.id === id) || null;
}

function openChat(agentId) {
  S.agentId = agentId;
  S.leftOpen = true;
  $('#left').dataset.open = 'true';
  S.universe?.setAgentActive(agentId);
  renderChat();
  setTimeout(() => $('#draft').focus(), 360);
}

function closeChat() {
  S.leftOpen = false;
  $('#left').dataset.open = 'false';
  S.universe?.setAgentActive(null);
}

function renderChat() {
  const agent = agentById(S.agentId) || (S.data?.agents || [])[0];
  if (!agent) return;
  S.agentId = agent.id;
  const messages = S.chats[agent.id] || [];

  $('#agent-dot').style.background = agent.color;
  $('#agent-dot').style.color = agent.color;
  $('#agent-name').textContent = agent.name;
  $('#agent-meta').textContent =
    `${agent.protocol} · ${messages.length ? `${messages.length} MESSAGES` : 'READY'}`;
  $('#agent-intro').textContent = agent.intro || '';
  $('#draft').placeholder = `Message ${agent.name}…`;

  const suggestions = $('#suggestions');
  suggestions.innerHTML = '';
  for (const text of (agent.suggestions || [])) {
    suggestions.append(el('button', { onclick: () => send(text) }, text));
  }

  const log = $('#chat-log');
  const empty = $('#chat-empty');
  log.innerHTML = '';
  empty.hidden = messages.length > 0;
  log.append(empty);

  for (const message of messages) log.append(renderMessage(message, agent));
  log.scrollTop = log.scrollHeight;
}

function renderMessage(message, agent) {
  const wrap = el('div', { class: `msg ${message.role}` });

  for (const line of (message.trace || [])) {
    wrap.append(el('div', {
      class: `trace${line.live ? ' live' : ''}${line.kind === 'error' ? ' err' : ''}`,
    }, el('span', {}, line.text)));
  }
  if (message.thought) {
    wrap.append(el('div', { class: 'bubble thought' }, message.thought));
  }

  if (message.html && !message.streaming) {
    // Rendered server-side through the same sanitizing markdown pipeline
    // notes use, so `[[Title]]` came back as `a.wiki[data-note]` — the same
    // markup a note's own body uses, styled by `.note` and driven the same
    // way a citation pill is: open the note, fly the camera to it.
    const bubble = el('div', { class: 'bubble rendered note', html: message.html });
    bubble.addEventListener('click', event => {
      const link = event.target.closest('a.wiki[data-note]');
      if (!link) return;
      event.preventDefault();
      goToCitedNote(Number(link.dataset.note));
    });
    wrap.append(bubble);
  } else if (message.text || message.role === 'user' || message.streaming) {
    const bubble = el('div', { class: 'bubble' }, message.text || '');
    if (message.streaming) {
      bubble.append(el('span', {
        class: 'cursor', style: `background:${agent.color}`,
      }));
    }
    wrap.append(bubble);
  }

  if (message.cites?.length) {
    wrap.append(el('div', { class: 'cites-label mono' }, 'CITED BY THE AGENT'));
    wrap.append(citePills(message.cites));
  }

  // Passages we supplied that the agent never referred to. Collapsed, and
  // visibly not a citation — the previous version mixed these in with the real
  // ones, which is what made every pill untrustworthy.
  if (message.context?.length) {
    const list = citePills(message.context);
    list.hidden = true;
    const toggle = el('button', {
      class: 'context-toggle mono',
      onclick: () => {
        list.hidden = !list.hidden;
        toggle.textContent = pillLabel(list.hidden);
      },
    }, pillLabel(true));
    function pillLabel(closed) {
      return `${closed ? '▸' : '▾'} ${message.context.length} PASSAGE`
        + `${message.context.length === 1 ? '' : 'S'} SUPPLIED AS CONTEXT, NOT CITED`;
    }
    wrap.append(toggle, list);
  }
  return wrap;
}

// What we can actually back up about each citation. `read` is the only tier
// that means we watched the agent open the file.
const EVIDENCE = {
  read:    { mark: '✓', label: 'we served this file to the agent' },
  opened:  { mark: '◆', label: 'a command that reads this file ran' },
  grounded:{ mark: '◇', label: 'we supplied this passage and the agent cited it' },
  matched: { mark: '✓', label: 'this is the search hit quoted above' },
  named:   { mark: '·', label: 'named only — we neither supplied it nor saw it opened' },
  context: { mark: '○', label: 'supplied to the agent; it never referred to this' },
};

// Open a cited note and fly the universe camera to it — what a citation pill
// does, and, for a rendered chat answer, what clicking its inline `[[Title]]`
// link does too. One place, so the two can never drift apart.
function goToCitedNote(nid) {
  openNote(nid, 'chat');
  const gid = S.universe?.gidForNote(nid);
  if (gid != null && gid >= 0) S.universe.flyTo(gid);
}

function citePills(cites) {
  const box = el('div', { class: 'cites' });
  for (const cite of cites) {
    const ev = EVIDENCE[cite.evidence] || EVIDENCE.named;
    const why = [cite.why || ev.label, cite.ambiguousWith
      ? `${cite.ambiguousWith} other note${cite.ambiguousWith === 1 ? '' : 's'} share this title`
      : null, cite.snippet].filter(Boolean).join(' — ');
    box.append(el('button', {
      class: `cite ev-${cite.evidence || 'named'}`,
      title: why,
      onclick: () => goToCitedNote(cite.nid),
    },
      el('span', { class: 'pip', style: `background:${cite.color};box-shadow:0 0 6px ${cite.color}` }),
      el('span', { class: 'nm' }, cite.name),
      cite.ambiguousWith ? el('span', { class: 'amb', title: why }, '?') : null,
      el('span', { class: 'br' }, cite.brain),
      el('span', { class: 'ev' }, ev.mark)
    ));
  }
  return box;
}

function send(text) {
  const agent = agentById(S.agentId);
  const draft = $('#draft');
  const question = (text ?? draft.value).trim();
  if (!agent || !question || S.streaming) return;

  draft.value = '';
  draft.style.height = 'auto';

  const chat = (S.chats[agent.id] ||= []);
  chat.push({ role: 'user', text: question });
  const reply = { role: 'agent', text: '', cites: [], context: [], trace: [], streaming: true };
  chat.push(reply);
  renderChat();

  const log = $('#chat-log');
  const stick = () => { log.scrollTop = log.scrollHeight; };

  S.streaming = { agentId: agent.id, close: null };
  S.universe?.setAgentBusy(agent.id, true);
  renderAgentRail();

  const redraw = () => {
    const nodes = log.querySelectorAll('.msg');
    const last = nodes[nodes.length - 1];
    const fresh = renderMessage(reply, agent);
    if (last) last.replaceWith(fresh); else log.append(fresh);
    stick();
  };

  const url = `/api/stream/chat?agent=${encodeURIComponent(agent.id)}`
    + `&q=${encodeURIComponent(question)}`;

  const finish = () => {
    reply.streaming = false;
    reply.trace = reply.trace.map(t => ({ ...t, live: false }));
    S.streaming = null;
    S.universe?.setAgentBusy(agent.id, false);
    renderAgentRail();
    $('#agent-meta').textContent = `${agent.protocol} · ${chat.length} MESSAGES`;
    redraw();
  };

  S.streaming.close = streamSSE(url, event => {
    switch (event.type) {
      case 'status':
        reply.trace = [...reply.trace.map(t => ({ ...t, live: false })),
          { text: event.text, live: true }].slice(-5);
        break;
      case 'tool': {
        // Tool calls report repeatedly as they progress. Key the line by the
        // call's id so it updates in place instead of stacking up, and keep
        // only the last few so the trace stays a status line, not a log.
        const key = event.id || event.text;
        const line = {
          key, live: true,
          text: `⚙ ${event.text}${event.status ? ` · ${event.status}` : ''}`,
        };
        const rest = reply.trace
          .filter(t => t.key !== key)
          .map(t => ({ ...t, live: false }));
        reply.trace = [...rest, line].slice(-5);
        break;
      }
      case 'thought':
        reply.thought = (reply.thought || '') + event.text;
        break;
      case 'delta':
        reply.text += event.text;
        reply.trace = reply.trace.map(t => ({ ...t, live: false }));
        break;
      case 'cites':
        reply.cites = event.cites || [];
        reply.context = event.context || [];
        reply.html = event.html || '';
        // Only real citations pulse in the universe. Lighting up the supplied
        // context too would re-create the impression we are trying to remove.
        if (reply.cites.length) {
          S.universe?.signal(agent.id,
            reply.cites.map(c => S.universe.gidForNote(c.nid)).filter(g => g >= 0));
        }
        break;
      case 'error':
        reply.trace = [...reply.trace.map(t => ({ ...t, live: false })),
          { text: event.text, kind: 'error' }];
        break;
      case 'done':
        finish();
        return;
    }
    redraw();
  }, () => finish());
}

/** Minimal SSE reader. EventSource cannot be aborted cleanly, fetch can. */
function streamSSE(url, onEvent, onClose) {
  const controller = new AbortController();
  (async () => {
    try {
      const response = await fetch(url, { signal: controller.signal });
      if (!response.ok || !response.body) throw new Error(`${response.status}`);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let cut;
        while ((cut = buffer.indexOf('\n\n')) >= 0) {
          const frame = buffer.slice(0, cut);
          buffer = buffer.slice(cut + 2);
          for (const line of frame.split('\n')) {
            if (!line.startsWith('data:')) continue;
            try { onEvent(JSON.parse(line.slice(5).trim())); }
            catch { /* a partial frame; the next read completes it */ }
          }
        }
      }
    } catch (err) {
      if (err.name !== 'AbortError') onEvent({ type: 'error', text: String(err.message || err) });
    } finally {
      onClose?.();
    }
  })();
  return () => controller.abort();
}

// ───────────────────────────── drawer ───────────────────────────────────

function openDrawer(tab) {
  if (tab) S.drawerTab = tab;
  $('#drawer').hidden = false;
  $('#drawer-scrim').hidden = false;
  renderDrawer();
}

function closeDrawer() {
  $('#drawer').hidden = true;
  $('#drawer-scrim').hidden = true;
  for (const close of S.jobStreams.values()) close();
  S.jobStreams.clear();
}

async function refreshStatus() {
  S.status = await api('/api/status');
  return S.status;
}

async function renderDrawer() {
  for (const button of $('#drawer-tabs').children) {
    button.classList.toggle('on', button.dataset.tab === S.drawerTab);
  }
  for (const panel of document.querySelectorAll('.tab-panel')) {
    panel.hidden = panel.dataset.panel !== S.drawerTab;
  }
  if ($('#drawer').hidden) return;
  try { await refreshStatus(); } catch { /* render what we have */ }
  ({ brains: renderBrainsPanel, agents: renderAgentsPanel,
     tools: renderToolsPanel, meetings: renderMeetingsPanel,
     view: renderViewPanel }[S.drawerTab])();
}

function renderBrainsPanel() {
  const panel = $('#panel-brains');
  panel.innerHTML = '';

  panel.append(el('div', { class: 'card' },
    el('div', { class: 'card-head' },
      el('span', { class: 'pip', style: 'background:var(--accent);color:var(--accent)' }),
      el('span', { class: 'nm' }, 'Index'),
      el('span', { class: 'badge' }, `${(S.status?.notes || 0).toLocaleString()} NOTES`)),
    el('div', { class: 'note-line' },
      S.status?.indexedAt
        ? `Last built ${timeAgo(Number(S.status.indexedAt))}. Rescanning only re-reads files whose timestamp changed.`
        : 'Not built yet.'),
    el('div', { class: 'card-actions' },
      el('button', { class: 'btn primary', onclick: () => runReindex(false) }, 'Rescan vaults'),
      el('button', { class: 'btn ghost', onclick: () => runReindex(true) }, 'Rebuild from scratch'))
  ));

  for (const brain of (S.status?.brains || [])) {
    const card = el('div', { class: 'card' },
      el('div', { class: 'card-head' },
        el('input', {
          type: 'color', class: 'color-pick', title: 'Brain color',
          value: colorForBrain(brain.id),
          onchange: async event => {
            try {
              await api(`/api/brain/${brain.id}`, {
                method: 'POST', body: { color: event.target.value },
              });
              // Cosmetic only — no rescan needed, just a fresh universe read
              // so the chip and the wireframe core pick up the new color.
              await loadUniverse(true);
              renderDrawer();
            } catch (err) { toast(String(err.message || err), true); }
          },
        }),
        el('span', { class: 'nm' }, brain.name),
        el('span', { class: 'badge' }, `${brain.notes.toLocaleString()} NOTES`)),
      el('div', { class: 'path' }, brain.path + (brain.exists ? '' : '  (missing)')),
      el('div', { class: 'card-actions' },
        el('label', { class: 'switch' },
          el('input', {
            type: 'checkbox', ...(brain.enabled ? { checked: true } : {}),
            onchange: async event => {
              await api(`/api/brain/${brain.id}`, {
                method: 'POST', body: { enabled: event.target.checked },
              });
              toast(`${brain.name} ${event.target.checked ? 'enabled' : 'disabled'} — rescan to apply`);
              renderDrawer();
            },
          }),
          el('span', {}, brain.enabled ? 'Shown in the universe' : 'Hidden')),
        el('span', { style: 'flex:1' }),
        el('button', {
          class: 'btn danger',
          onclick: async () => {
            if (!confirm(`Remove ${brain.name} from the app? The vault itself is not touched.`)) return;
            await api(`/api/brain/${brain.id}/remove`, { method: 'POST' });
            toast(`${brain.name} removed`);
            renderDrawer();
          },
        }, 'Remove')),
      // Starts off for every brain — only one vault may catch raw_transcripts/
      // at a time, so turning this on for one turns it off for the others.
      el('div', { class: 'card-actions' },
        el('label', { class: 'switch' },
          el('input', {
            type: 'checkbox', ...(brain.meetingTarget ? { checked: true } : {}),
            onchange: async event => {
              try {
                await api(`/api/brain/${brain.id}`, {
                  method: 'POST', body: { meetingTarget: event.target.checked },
                });
                toast(event.target.checked
                  ? `${brain.name} is now the meeting recording target`
                  : `${brain.name} is no longer the meeting recording target`);
                renderDrawer();
              } catch (err) { toast(String(err.message || err), true); }
            },
          }),
          el('span', {}, 'Meeting recording target'))),
      brain.meetingTarget ? el('div', { class: 'field' },
        el('label', {}, 'RAW TRANSCRIPTS FOLDER'),
        el('input', {
          type: 'text', value: brain.meetingFolder || 'Meetings', placeholder: 'Meetings',
          onchange: async event => {
            const value = event.target.value.trim() || 'Meetings';
            event.target.value = value;
            try {
              await api(`/api/brain/${brain.id}`, {
                method: 'POST', body: { meetingFolder: value },
              });
              toast(`Meeting transcripts will land in ${brain.name}/${value}`);
            } catch (err) { toast(String(err.message || err), true); }
          },
        }),
        el('div', { class: 'hint' }, 'Path inside this vault, relative to its root.')) : null
    );
    panel.append(card);
  }

  const discovered = el('div', { class: 'card' },
    el('div', { class: 'section-title' }, 'ADD A VAULT'),
    el('div', { class: 'field' },
      el('label', {}, 'PATH'),
      el('input', { type: 'text', id: 'new-vault', placeholder: '~/Documents/ObsidianVaults/…' })),
    el('div', { class: 'card-actions' },
      el('button', {
        class: 'btn',
        onclick: async () => {
          const path = $('#new-vault').value.trim();
          if (!path) return;
          try {
            await api('/api/brains/add', { method: 'POST', body: { path } });
            toast('Vault added — rescanning');
            runReindex(false);
          } catch (err) { toast(String(err.message || err), true); }
        },
      }, 'Add'),
      el('button', { class: 'btn ghost', onclick: () => findVaults(discovered) }, 'Find vaults on this Mac'))
  );
  panel.append(discovered);
}

async function findVaults(card) {
  try {
    const { vaults } = await api('/api/brains/discover');
    card.querySelectorAll('.found').forEach(node => node.remove());
    if (!vaults.length) {
      card.append(el('div', { class: 'muted found' }, 'No unconfigured vaults found.'));
      return;
    }
    for (const vault of vaults) {
      card.append(el('div', { class: 'card-actions found' },
        el('span', { class: 'path', style: 'flex:1' }, `${vault.name} — ${vault.notes} notes`),
        el('button', {
          class: 'btn',
          onclick: async () => {
            await api('/api/brains/add', { method: 'POST', body: { path: vault.path } });
            toast(`${vault.name} added — rescanning`);
            runReindex(false);
          },
        }, 'Add')));
    }
  } catch (err) { toast(String(err.message || err), true); }
}

function renderAgentsPanel() {
  const panel = $('#panel-agents');
  panel.innerHTML = '';

  panel.append(el('p', { class: 'muted' },
    'An agent is a star in the universe. Double-click it to talk to it; whatever it '
    + 'answers is grounded in passages retrieved from your brains, and every citation '
    + 'opens the note it came from.'));

  for (const agent of (S.status?.agents || [])) {
    panel.append(agentCard(agent));
  }

  panel.append(el('div', { class: 'card' },
    el('div', { class: 'section-title' }, 'ADD A CONNECTION'),
    el('div', { class: 'card-actions' },
      ...['local', 'acp', 'a2a'].map(kind => el('button', {
        class: 'btn',
        onclick: async () => {
          const name = prompt(`Name for the new ${kind.toUpperCase()} agent?`,
            kind === 'acp' ? 'Claude Code' : kind === 'a2a' ? 'Remote agent' : 'Kernel');
          if (!name) return;
          await api('/api/agents/add', { method: 'POST', body: { name, kind } });
          toast(`${name} added`);
          await loadUniverse(true);
          renderDrawer();
        },
      }, `New ${kind.toUpperCase()}`)))
  ));
}

function agentCard(agent) {
  const fields = el('div', { class: 'card', id: `agent-card-${agent.id}` });
  const probe = el('div', { class: 'probe wait' }, 'not checked');

  const input = (label, key, value, placeholder = '') => el('div', { class: 'field' },
    el('label', {}, label),
    el('input', { type: 'text', 'data-key': key, value: value ?? '', placeholder })
  );

  fields.append(
    el('div', { class: 'card-head' },
      el('span', { class: 'pip', style: `background:${agent.color};color:${agent.color}` }),
      el('span', { class: 'nm' }, agent.name),
      el('span', { class: 'badge' }, agent.kind.toUpperCase())),
    el('div', { class: 'grid2' },
      input('NAME', 'name', agent.name),
      input('PROTOCOL LABEL', 'protocol', agent.protocol)),
  );

  if (agent.kind === 'acp') {
    fields.append(
      input('COMMAND', 'command', agent.command, 'npx -y @zed-industries/claude-code-acp'),
      input('WORKING DIRECTORY', 'cwd', agent.cwd, '~/dev/aibrain-kernel'),
      el('div', { class: 'field' }, el('div', { class: 'hint' },
        'Spoken over stdio. The agent may read and write files under the working '
        + 'directory; tool calls it asks permission for are approved automatically.')),
    );
  } else if (agent.kind === 'a2a') {
    fields.append(
      input('BASE URL', 'url', agent.url, 'http://localhost:9000'),
      el('div', { class: 'field' }, el('div', { class: 'hint' },
        'The agent card is read from /.well-known/agent-card.json, then messages go '
        + 'to the endpoint it advertises.')),
    );
  } else {
    fields.append(el('div', { class: 'field' }, el('div', { class: 'hint' },
      'Answers straight from the local index — no model, no network.')));
  }

  fields.append(
    el('div', { class: 'field' },
      el('label', {}, 'INTRO'),
      el('textarea', { 'data-key': 'intro' }, agent.intro || '')),
    el('div', { class: 'field' },
      el('label', {}, 'SUGGESTED QUESTIONS (ONE PER LINE)'),
      el('textarea', { 'data-key': 'suggestions' }, (agent.suggestions || []).join('\n'))),
    probe,
    el('div', { class: 'card-actions' },
      el('label', { class: 'switch' },
        el('input', {
          type: 'checkbox', 'data-key': 'enabled',
          ...(agent.enabled ? { checked: true } : {}),
        }),
        el('span', {}, 'Visible in the universe')),
      el('span', { style: 'flex:1' }),
      el('button', {
        class: 'btn ghost',
        onclick: async () => {
          probe.className = 'probe wait';
          probe.textContent = 'checking…';
          try {
            const result = await api(`/api/agent/${agent.id}/probe`);
            probe.className = `probe ${result.ok ? 'ok' : 'bad'}`;
            probe.textContent = `${result.ok ? '✓' : '✕'} ${result.detail}`;
          } catch (err) {
            probe.className = 'probe bad';
            probe.textContent = String(err.message || err);
          }
        },
      }, 'Test'),
      el('button', {
        class: 'btn danger',
        onclick: async () => {
          if (!confirm(`Remove ${agent.name}?`)) return;
          await api(`/api/agent/${agent.id}/remove`, { method: 'POST' });
          await loadUniverse(true);
          renderDrawer();
        },
      }, 'Remove'),
      el('button', {
        class: 'btn primary',
        onclick: async () => {
          const body = {};
          for (const node of fields.querySelectorAll('[data-key]')) {
            body[node.dataset.key] = node.type === 'checkbox' ? node.checked : node.value;
          }
          try {
            await api(`/api/agent/${agent.id}`, { method: 'POST', body });
            toast(`${body.name || agent.name} saved`);
            await loadUniverse(true);
            renderDrawer();
          } catch (err) { toast(String(err.message || err), true); }
        },
      }, 'Save'))
  );
  return fields;
}

function renderToolsPanel() {
  const panel = $('#panel-tools');
  panel.innerHTML = '';

  panel.append(el('p', { class: 'muted' },
    'Scripts that feed the brains. Output streams here as they run.'));

  for (const script of (S.status?.scripts || [])) {
    const chosen = new Set();
    const console_ = el('div', { class: 'console' }, 'not run yet');
    const running = (S.status?.jobs || []).find(j => j.name === script.name && j.status === 'running');

    const runButton = el('button', {
      class: 'btn primary',
      onclick: async () => {
        runButton.disabled = true;
        try {
          const { job } = await api(`/api/script/${script.id}/run`, {
            method: 'POST', body: { options: [...chosen] },
          });
          attachConsole(console_, job.id, () => { runButton.disabled = false; });
        } catch (err) {
          toast(String(err.message || err), true);
          runButton.disabled = false;
        }
      },
    }, running ? 'Running…' : 'Run');
    if (running) {
      runButton.disabled = true;
      attachConsole(console_, running.id, () => {
        runButton.disabled = false;
        runButton.textContent = 'Run';
      });
    }

    panel.append(el('div', { class: 'card' },
      el('div', { class: 'card-head' },
        el('span', { class: 'pip', style: 'background:#f2952d;color:#f2952d' }),
        el('span', { class: 'nm' }, script.name)),
      el('div', { class: 'note-line' }, script.description),
      el('div', { class: 'path' }, script.command.join(' ')),
      el('div', { class: 'card-actions' },
        ...Object.keys(script.options || {}).map(name => el('label', { class: 'switch' },
          el('input', {
            type: 'checkbox',
            onchange: event => {
              if (event.target.checked) chosen.add(name); else chosen.delete(name);
            },
          }),
          el('span', {}, name))),
        el('span', { style: 'flex:1' }),
        runButton),
      console_
    ));
  }

  const history = (S.status?.jobs || []).filter(j => j.status !== 'running');
  if (history.length) {
    panel.append(el('div', { class: 'card' },
      el('div', { class: 'section-title' }, 'RECENT RUNS'),
      ...history.slice(0, 6).map(job => el('div', { class: 'card-actions' },
        el('span', {
          class: 'pip',
          style: `background:${job.status === 'done' ? '#3ecf9a' : '#ff7a59'}`,
        }),
        el('span', { class: 'path', style: 'flex:1' },
          `${job.name} · ${job.status} · ${timeAgo(job.finished || job.started)}`),
        el('button', {
          class: 'btn ghost',
          onclick: async () => {
            const full = await api(`/api/job/${job.id}`);
            const box = el('div', { class: 'console' }, full.lines.join('\n'));
            panel.append(box);
            box.scrollTop = box.scrollHeight;
          },
        }, 'Log')))));
  }
}

function renderMeetingsPanel() {
  const panel = $('#panel-meetings');
  panel.innerHTML = '';

  const target = (S.status?.brains || []).find(b => b.meetingTarget);
  if (!target) {
    panel.append(el('p', { class: 'muted' },
      'No brain is set as the meeting recording target. Open the Brains tab ' +
      'and turn on “Meeting recording target” for one vault.'));
    return;
  }

  panel.append(el('p', { class: 'muted' },
    `Raw transcripts land in raw_transcripts/, staged to absorb into ` +
    `${target.name} → ${target.meetingFolder || 'Meetings'}. Pull, review, ` +
    `then choose what to copy in — nothing moves until you import.`));

  const script = (S.status?.scripts || []).find(s => s.id === 'macwhisper');
  const pullConsole = el('div', { class: 'console' }, 'not run yet');
  const card = el('div', { class: 'card' },
    el('div', { class: 'card-head' },
      el('span', { class: 'pip', style: 'background:#f2952d;color:#f2952d' }),
      el('span', { class: 'nm' }, 'MacWhisper import')));

  const preview = el('div', {});
  const importConsole = el('div', { class: 'console', hidden: true });
  const importButton = el('button', {
    class: 'btn primary', disabled: true,
    onclick: async () => {
      const files = [...preview.querySelectorAll('input[type=checkbox]:checked')]
        .map(cb => cb.dataset.name);
      if (!files.length) return;
      if (!confirm(
        `Copy ${files.length} file${files.length === 1 ? '' : 's'} into ` +
        `${target.name} → ${target.meetingFolder || 'Meetings'}? Files with ` +
        `the same name already there will be overwritten.`
      )) return;
      importButton.disabled = true;
      importConsole.hidden = false;
      try {
        const { job } = await api('/api/meetings/import', { method: 'POST', body: { files } });
        attachConsole(importConsole, job.id, () => {
          loadMeetingPreview(preview, importButton);
        });
      } catch (err) {
        toast(String(err.message || err), true);
        importButton.disabled = false;
      }
    },
  }, 'Import selected');

  if (script) {
    const running = (S.status?.jobs || []).find(j => j.name === script.name && j.status === 'running');
    const runButton = el('button', {
      class: 'btn primary',
      onclick: async () => {
        if (!confirm('Pulling transcripts will quit MacWhisper (it relaunches automatically when done). Continue?')) return;
        runButton.disabled = true;
        try {
          const { job } = await api(`/api/script/${script.id}/run`, { method: 'POST', body: { options: [] } });
          attachConsole(pullConsole, job.id, () => { runButton.disabled = false; loadMeetingPreview(preview, importButton); });
        } catch (err) {
          toast(String(err.message || err), true);
          runButton.disabled = false;
        }
      },
    }, running ? 'Running…' : 'Pull from MacWhisper');
    if (running) {
      runButton.disabled = true;
      attachConsole(pullConsole, running.id, () => {
        runButton.disabled = false;
        runButton.textContent = 'Pull from MacWhisper';
        loadMeetingPreview(preview, importButton);
      });
    }
    card.append(el('div', { class: 'card-actions' }, runButton), pullConsole);
  }

  card.append(
    el('div', { class: 'label mono', style: 'margin-top:4px' }, 'PREVIEW — SELECT WHAT TO IMPORT'),
    preview,
    el('div', { class: 'card-actions' }, importButton),
    importConsole);
  panel.append(card);
  loadMeetingPreview(preview, importButton);
}

async function loadMeetingPreview(node, importButton) {
  node.innerHTML = '';
  try {
    const { files } = await api('/api/meetings/preview');
    if (!files.length) {
      node.append(el('div', { class: 'muted' }, 'raw_transcripts/ is empty — nothing staged.'));
      if (importButton) importButton.disabled = true;
      return;
    }
    for (const file of files) {
      node.append(el('label', { class: 'file-row' },
        el('span', { style: 'display:flex;align-items:center;gap:8px;min-width:0;overflow:hidden' },
          el('input', { type: 'checkbox', checked: true, 'data-name': file.name }),
          el('span', { style: 'overflow:hidden;text-overflow:ellipsis;white-space:nowrap' }, file.name)),
        el('span', { class: `badge ${file.status}` }, file.status.toUpperCase())));
    }
    if (importButton) importButton.disabled = false;
  } catch (err) {
    node.append(el('div', { class: 'muted' }, String(err.message || err)));
    if (importButton) importButton.disabled = true;
  }
}

function attachConsole(node, jobId, onDone) {
  node.textContent = '';
  const existing = S.jobStreams.get(jobId);
  if (existing) existing();
  const close = streamSSE(`/api/stream/job/${jobId}`, event => {
    if (event.type === 'line') {
      const bad = /error|failed|traceback|refus/i.test(event.text);
      node.append(el('div', { class: bad ? 'err' : '' }, event.text), '\n');
      node.scrollTop = node.scrollHeight;
    }
    if (event.type === 'done') {
      node.append(el('div', {
        class: event.status === 'done' ? 'ok' : 'err',
      }, `— ${event.status} —`));
      node.scrollTop = node.scrollHeight;
      S.jobStreams.delete(jobId);
      onDone?.();
      if (event.status === 'done') refreshAfterJob();
    }
  });
  S.jobStreams.set(jobId, close);
}

async function refreshAfterJob() {
  try {
    await loadUniverse(true);
    loadRecent();
    toast('Universe rebuilt');
  } catch (err) { toast(String(err.message || err), true); }
}

async function runReindex(force) {
  try {
    const { job } = await api('/api/reindex', { method: 'POST', body: { force } });
    toast(force ? 'Rebuilding the index…' : 'Rescanning your vaults…');
    const panel = $('#panel-brains');
    const box = el('div', { class: 'console' }, '');
    panel.prepend(box);
    attachConsole(box, job.id, () => renderDrawer());
  } catch (err) { toast(String(err.message || err), true); }
}

function renderViewPanel() {
  const panel = $('#panel-view');
  panel.innerHTML = '';
  const view = S.status?.view || {};

  const slider = (label, key, min, max, step, value, format) => {
    const out = el('span', { class: 'val' }, format(value));
    return el('div', { class: 'field' },
      el('label', {}, label),
      el('div', { class: 'range' },
        el('input', {
          type: 'range', min, max, step, value,
          oninput: event => {
            out.textContent = format(Number(event.target.value));
            S.universe?.setOptions({ [key]: Number(event.target.value) });
          },
          onchange: event => saveView({ [key]: Number(event.target.value) }),
        }),
        out));
  };

  panel.append(
    el('div', { class: 'card' },
      el('div', { class: 'section-title' }, 'MOTION AND CONNECTIONS'),
      slider('ROTATION SPEED', 'rotationSpeed', 0, 1, 0.05,
        view.rotation_speed ?? 0.35, v => v.toFixed(2)),
      slider('LINK OPACITY', 'linkOpacity', 0.05, 0.6, 0.01,
        view.link_opacity ?? 0.24, v => v.toFixed(2)),
      slider('RIBBON TWIST', 'ribbonTwist', 0, 0.8, 0.05,
        view.ribbon_twist ?? 0.25, v => v.toFixed(2)),
      el('label', { class: 'switch' },
        el('input', {
          type: 'checkbox', ...(view.show_all_labels ? { checked: true } : {}),
          onchange: event => {
            S.universe?.setOptions({ showAllLabels: event.target.checked });
            saveView({ showAllLabels: event.target.checked });
          },
        }),
        el('span', {}, 'Label every hub note')),
      el('div', { class: 'hint' },
        'Ribbon twist only takes effect the next time the universe is rebuilt.')),

    el('div', { class: 'card' },
      el('div', { class: 'section-title' }, 'WHERE THINGS LIVE'),
      el('div', { class: 'path' }, 'config  ~/.aibrain/config.json'),
      el('div', { class: 'path' }, 'corpus  aibrain-core on postgres'))
  );
}

async function saveView(patch) {
  try { await api('/api/view', { method: 'POST', body: patch }); }
  catch (err) { toast(String(err.message || err), true); }
}

// ───────────────────────────── wiring ───────────────────────────────────

function wireStaticHandlers() {
  $('#chat-close').onclick = closeChat;
  $('#chat-clear').onclick = async () => {
    if (!S.agentId) return;
    S.chats[S.agentId] = [];
    await api(`/api/chat/${S.agentId}/clear`, { method: 'POST' }).catch(() => {});
    renderChat();
    toast('Conversation cleared');
  };
  $('#send').onclick = () => send();

  const draft = $('#draft');
  draft.addEventListener('input', () => {
    draft.style.height = 'auto';
    draft.style.height = Math.min(draft.scrollHeight, 160) + 'px';
  });
  draft.addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      send();
    }
  });

  $('#open-search').onclick = openSearch;
  $('#right-close').onclick = closeRight;
  $('#reset-view').onclick = async () => {
    S.brainFocus = null;
    S.statusText = null;
    renderChips();
    renderStatus();
    try {
      // Snap any dragged agents back to their default slots too — a rebuild
      // if something actually moved, a cheap camera reset otherwise.
      const { changed } = await api('/api/agents/reset-positions', { method: 'POST' });
      if (changed) await loadUniverse(true);
      else S.universe?.resetView();
    } catch (err) {
      S.universe?.resetView();
      toast(String(err.message || err), true);
    }
    if (S.query.trim()) runSearch(S.query);
  };
  $('#import-meetings').onclick = async () => {
    try { await refreshStatus(); } catch (err) { toast(String(err.message || err), true); return; }
    const target = (S.status.brains || []).find(b => b.meetingTarget);
    if (!target) {
      toast('Set a brain as the meeting recording target first (Brains tab).', true);
      openDrawer('brains');
      return;
    }
    const script = (S.status.scripts || []).find(s => s.id === 'macwhisper');
    if (!script) {
      toast('The macwhisper script is not configured.', true);
      return;
    }
    if (!confirm('Pulling meeting transcripts will quit MacWhisper (it relaunches automatically when done). Continue?')) return;
    try {
      await api(`/api/script/${script.id}/run`, { method: 'POST', body: { options: [] } });
      openDrawer('meetings');
    } catch (err) { toast(String(err.message || err), true); }
  };
  wireShelf();              // the day's list — see the section at the bottom
  $('#open-menu').onclick = () => openDrawer();
  $('#drawer-close').onclick = closeDrawer;
  $('#drawer-scrim').onclick = closeDrawer;
  for (const button of $('#drawer-tabs').children) {
    button.onclick = () => { S.drawerTab = button.dataset.tab; renderDrawer(); };
  }

  const query = $('#query');
  query.addEventListener('input', () => {
    S.query = query.value;
    $('#clear-q').hidden = !S.query;
    runSearch(S.query);
    if (S.query.trim()) renderResults();
  });
  $('#clear-q').onclick = () => {
    query.value = '';
    S.query = '';
    S.results = [];
    $('#clear-q').hidden = true;
    S.universe?.highlight(null);
    renderResults();
    query.focus();
  };
  $('#back-results').onclick = () => {
    S.note = null;
    S.universe?.focusNode(-1);
    renderResults();
  };

  window.addEventListener('keydown', event => {
    const typing = /INPUT|TEXTAREA/.test(document.activeElement?.tagName || '');
    if (event.key === '/' && !typing) { event.preventDefault(); openSearch(); }
    if (event.key === 'Escape') {
      if (!$('#drawer').hidden) closeDrawer();
      else if (document.activeElement === $('#query')) { $('#query').blur(); }
      else if (S.rightOpen) closeRight();
      else if (S.leftOpen) closeChat();
      else if (TODO.open) closeShelf();
    }
    if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && !S.leftOpen) {
      const first = S.data?.agents?.[0];
      if (first) openChat(first.id);
    }
  });
}

function openSearch() {
  openRight();
  S.note = null;
  renderResults();
  setTimeout(() => $('#query').focus(), 360);
}

// ───────────────────────── the day's list (shelf) ───────────────────────
//
// A flex sibling ahead of the chat panel, so opening it pushes the
// conversation right rather than covering it — the point is to see the day
// and the answer at once.
//
// Everything here is additive: its own state object, its own functions, and
// one line in wireStaticHandlers that binds the toggle. The server owns which
// day an item belongs to and when a rollover happens; this only asks.

const TODO = {
  day: null,        // the day being shown, YYYY-MM-DD
  today: null,      // what the server calls today, by its start hour
  items: [],
  folders: [],      // the persistent backlog, always rendered last
  open: false,
  picking: null,    // the id whose note picker is showing
  editingFolder: null,  // the folder id whose name is a text field right now
  addingFolder: false,  // whether the "new folder" input is showing
  loading: false,
};

// The id of whatever `.todo` is being dragged, so any drop target — a folder
// header, a folder's body, another row to reorder against — can read it
// without smuggling state through dataTransfer.
let dragTodoId = null;

function openShelf() {
  TODO.open = true;
  $('#shelf').dataset.open = 'true';
  loadTodos(TODO.day);
  setTimeout(() => $('#todo-draft')?.focus(), 360);
}

function closeShelf() {
  TODO.open = false;
  TODO.picking = null;
  $('#shelf').dataset.open = 'false';
}

function toggleShelf() {
  if (TODO.open) closeShelf(); else openShelf();
}

async function loadTodos(day) {
  TODO.loading = true;
  try {
    const query = day ? `?day=${encodeURIComponent(day)}` : '';
    const data = await api(`/api/todos${query}`);
    TODO.day = data.day;
    TODO.today = data.today;
    TODO.items = data.todos || [];
    TODO.folders = data.folders || [];
    renderShelf();
    if (data.rolled) {
      toast(`${data.rolled} item${data.rolled === 1 ? '' : 's'} carried over`);
    }
  } catch (err) {
    toast(String(err.message || err), true);
  } finally {
    TODO.loading = false;
  }
}

function shiftDay(iso, days) {
  const [y, m, d] = iso.split('-').map(Number);
  const date = new Date(Date.UTC(y, m - 1, d));
  date.setUTCDate(date.getUTCDate() + days);
  return date.toISOString().slice(0, 10);
}

/// "Today", "Yesterday", or the weekday — the calendar date is underneath it.
function dayLabel(day, today) {
  if (!day || !today) return 'Today';
  if (day === today) return 'Today';
  if (day === shiftDay(today, 1)) return 'Tomorrow';
  if (day === shiftDay(today, -1)) return 'Yesterday';
  const [y, m, d] = day.split('-').map(Number);
  return new Date(Date.UTC(y, m - 1, d))
    .toLocaleDateString(undefined, { weekday: 'long', timeZone: 'UTC' });
}

function dayStamp(day) {
  if (!day) return '—';
  const [y, m, d] = day.split('-').map(Number);
  return new Date(Date.UTC(y, m - 1, d))
    .toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric', timeZone: 'UTC' })
    .toUpperCase();
}

function renderShelf() {
  $('#shelf-day').textContent = dayLabel(TODO.day, TODO.today);
  $('#shelf-date').textContent = dayStamp(TODO.day);
  $('#shelf-today').hidden = false;

  const list = $('#shelf-list');
  list.innerHTML = '';
  // The whole list is the drop target for taking an item back out of a
  // folder; each folder's own header/body stops the event before it bubbles
  // here, so this only fires for a drop that landed outside every folder.
  list.ondragover = event => { event.preventDefault(); };
  list.ondrop = event => {
    event.preventDefault();
    if (dragTodoId != null) fileTodo(dragTodoId, null);
  };

  if (!TODO.items.length) {
    list.append(el('p', { class: 'shelf-empty' },
      TODO.day === TODO.today
        ? 'Nothing on today yet. Add the first thing below.'
        : 'Nothing on this day.'));
  } else {
    for (const item of TODO.items) {
      list.append(todoRow(item));
      if (TODO.picking === item.id) list.append(todoPicker(item));
    }
    const carried = TODO.items.filter(i => i.state === 'open'
      && i.first_scheduled_on && i.first_scheduled_on !== i.scheduled_on).length;
    if (carried) {
      list.append(el('div', { class: 'shelf-note' },
        `${carried} CARRIED FROM AN EARLIER DAY`));
    }
  }

  if (TODO.folders.length || TODO.addingFolder) {
    const wrap = el('div', { class: 'shelf-folders' });
    for (const folder of TODO.folders) wrap.append(folderSection(folder));
    wrap.append(folderAddRow());
    list.append(wrap);
  } else {
    list.append(el('button', {
      class: 'shelf-folder-new', title: 'Group tasks into a folder',
      onclick: () => { TODO.addingFolder = true; renderShelf(); },
    }, '+ New folder'));
  }
}

function folderSection(folder) {
  const open = !folder.collapsed;
  const body = el('div', { class: 'shelf-folder-body' });
  if (!folder.todos.length) {
    body.append(el('p', { class: 'shelf-empty' }, 'Drag tasks in.'));
  } else {
    for (const item of folder.todos) body.append(todoRow(item, folder));
  }
  body.hidden = !open;
  dropZone(body, folder);

  const nameNode = TODO.editingFolder === folder.id
    ? folderNameInput(folder)
    : el('span', {
        class: 'nm',
        title: 'Click to rename',
        onclick: event => { event.stopPropagation(); TODO.editingFolder = folder.id; renderShelf(); },
      }, folder.name);

  const head = el('div', { class: 'shelf-folder-head' },
    el('button', {
      class: 'chevron', title: open ? 'Collapse' : 'Expand',
      onclick: () => patchFolder(folder.id, { collapsed: open }),
    }, open ? '▾' : '▸'),
    nameNode,
    el('span', { class: 'count mono' }, String(folder.todos.length)),
    el('span', { style: 'flex:1' }),
    el('button', {
      class: 'act', title: 'Delete this folder',
      onclick: () => {
        if (folder.todos.length && !confirm(`Delete “${folder.name}”? Its ${folder.todos.length} task${folder.todos.length === 1 ? '' : 's'} will land back on today's list.`)) return;
        deleteFolder(folder.id);
      },
    }, '×'),
  );
  dropZone(head, folder);

  return el('div', { class: 'shelf-folder' }, head, body);
}

function folderNameInput(folder) {
  const input = el('input', {
    class: 'shelf-folder-name-input', value: folder.name,
    onclick: event => event.stopPropagation(),
    onkeydown: event => {
      if (event.key === 'Enter') { event.preventDefault(); input.blur(); }
      if (event.key === 'Escape') { TODO.editingFolder = null; renderShelf(); }
    },
    onblur: () => {
      const name = input.value.trim();
      TODO.editingFolder = null;
      if (name && name !== folder.name) renameFolder(folder.id, name);
      else renderShelf();
    },
  });
  setTimeout(() => { input.focus(); input.select(); }, 0);
  return input;
}

function folderAddRow() {
  if (!TODO.addingFolder) {
    return el('button', {
      class: 'shelf-folder-new', title: 'Group tasks into a folder',
      onclick: () => { TODO.addingFolder = true; renderShelf(); },
    }, '+ New folder');
  }
  const input = el('input', {
    class: 'shelf-folder-name-input', placeholder: 'Folder name…',
    onkeydown: event => {
      if (event.key === 'Enter') { event.preventDefault(); input.blur(); }
      if (event.key === 'Escape') { TODO.addingFolder = false; renderShelf(); }
    },
    onblur: () => {
      const name = input.value.trim();
      TODO.addingFolder = false;
      if (name) createFolder(name);
      else renderShelf();
    },
  });
  setTimeout(() => input.focus(), 0);
  return input;
}

/// Wires a folder header or body as a drop target: dropping a dragged task
/// files it into this folder, at the end unless it lands on a specific row
/// (see `todoRow`'s own drop handler, which stops the event here).
function dropZone(node, folder) {
  node.addEventListener('dragover', event => {
    event.preventDefault();
    node.classList.add('drag-over');
  });
  node.addEventListener('dragleave', () => node.classList.remove('drag-over'));
  node.addEventListener('drop', event => {
    event.preventDefault();
    event.stopPropagation();
    node.classList.remove('drag-over');
    if (dragTodoId != null) fileTodo(dragTodoId, folder.id);
  });
}

function todoRow(item, folder) {
  const done = item.state === 'completed';
  const gone = item.state === 'cancelled';

  const box = el('button', {
    class: 'box',
    title: done ? 'Mark it not done' : 'Mark it done',
    onclick: () => todoAction(item.id, done ? 'uncomplete' : 'complete'),
  }, done ? '✓' : gone ? '–' : '✓');

  const col = el('div', { class: 'col' },
    el('div', { class: 'body' }, item.body));

  const carried = item.first_scheduled_on && item.first_scheduled_on !== item.scheduled_on;
  if (carried && !done && !gone) {
    col.append(el('div', { class: 'mt' }, `carried since ${dayStamp(item.first_scheduled_on)}`));
  }
  if (item.refs?.length) {
    const refs = el('div', { class: 'refs' });
    for (const ref of item.refs) {
      refs.append(el('button', {
        class: 'ref',
        title: `${ref.brain_id} · ${ref.rel_path}`,
        onclick: () => ref.note_id
          ? openNote(ref.note_id, null)
          : toast('That note is not in the index right now'),
      }, ref.rel_path.split('/').pop().replace(/\.md$/, '')));
    }
    col.append(refs);
  }

  const acts = el('div', { class: 'acts' },
    folder
      ? el('button', {
          class: 'act', title: 'Take out of the folder',
          onclick: () => fileTodo(item.id, null),
        }, '⇤')
      : el('button', {
          class: 'act', title: 'Move to tomorrow',
          onclick: () => todoAction(item.id, 'reschedule', { to_day: 'tomorrow' }),
        }, '»'),
    el('button', {
      class: 'act', title: 'Pick a day, or link a note',
      onclick: () => { TODO.picking = TODO.picking === item.id ? null : item.id; renderShelf(); },
    }, '⊞'),
    el('button', {
      class: 'act', title: 'Drop it',
      onclick: () => todoAction(item.id, 'cancel'),
    }, '⊘'),
  );

  const row = el('div', {
    // Not `true` — el()'s generic boolean handling would set draggable="",
    // and unlike other boolean attributes, an empty value is invalid for
    // `draggable` and falls back to "auto" (not draggable for a <div>). The
    // spec requires the literal string "true".
    class: 'todo', 'data-state': item.state, draggable: 'true',
    ondragstart: event => {
      dragTodoId = item.id;
      event.dataTransfer.effectAllowed = 'move';
    },
    ondragend: () => { dragTodoId = null; },
  }, box, col, acts);

  // Dropping on another row inside the same folder reorders against it;
  // outside a folder, or crossing folders, the folder/list drop zones above
  // handle it instead (this one only wins when both sides share `folder`).
  if (folder) {
    row.addEventListener('dragover', event => {
      event.preventDefault();
      event.stopPropagation();
      row.classList.add('drag-over');
    });
    row.addEventListener('dragleave', () => row.classList.remove('drag-over'));
    row.addEventListener('drop', event => {
      event.preventDefault();
      event.stopPropagation();
      row.classList.remove('drag-over');
      if (dragTodoId != null && dragTodoId !== item.id) {
        reorderInFolder(dragTodoId, folder, item.id);
      }
    });
  }

  return row;
}

/// Pick a day, or attach a note. The note search is the app's own search, so
/// what you can cite is what you can link.
function todoPicker(item) {
  const hits = el('div', { class: 'hits' });
  const field = el('input', {
    type: 'search', placeholder: 'Search notes to link…', autocomplete: 'off',
  });

  const search = debounce(async text => {
    hits.innerHTML = '';
    if (!text.trim()) return;
    try {
      const data = await api(`/api/search?q=${encodeURIComponent(text)}&limit=8`);
      for (const row of data.results) {
        hits.append(el('button', {
          class: 'hit',
          onclick: () => linkNote(item.id, row.brainId || row.brain_id, row.relPath || row.rel_path),
        }, row.name, el('small', {}, `${row.brain || ''} · ${row.source || ''}`)));
      }
      if (!data.results.length) hits.append(el('div', { class: 'shelf-note' }, 'NOTHING MATCHES'));
    } catch (err) {
      toast(String(err.message || err), true);
    }
  }, 180);
  field.addEventListener('input', () => search(field.value));

  const date = el('input', { type: 'date', value: item.scheduled_on || '' });
  date.addEventListener('change', () => {
    if (date.value) todoAction(item.id, 'reschedule', { to_day: date.value });
  });

  const picker = el('div', { class: 'todo-picker' }, date, field, hits);
  setTimeout(() => field.focus(), 0);
  return picker;
}

async function linkNote(id, brainId, relPath) {
  if (!brainId || !relPath) {
    toast('That result has no path to link', true);
    return;
  }
  await todoAction(id, 'link', { brain_id: brainId, rel_path: relPath });
}

async function todoAction(id, action, body) {
  try {
    await api(`/api/todos/${id}/${action}`, { method: 'POST', body: body || {} });
    TODO.picking = null;
    await loadTodos(TODO.day);
  } catch (err) {
    toast(String(err.message || err), true);
  }
}

/// File a task into a folder, or (`folderId: null`) take it back out onto
/// the day's list — the service handles both through the same endpoint.
async function fileTodo(id, folderId) {
  await todoAction(id, 'file', { folder_id: folderId });
}

/// Drop `id` just above `beforeId` within `folder`, filing it in first if it
/// isn't already a member. `sort_order` has no meaning outside its folder or
/// day, so a plain midpoint between neighbours is all reordering needs.
async function reorderInFolder(id, folder, beforeId) {
  const items = folder.todos;
  const at = items.findIndex(t => t.id === beforeId);
  const prev = items[at - 1];
  const order = prev && prev.id !== id ? (prev.sort_order + items[at].sort_order) / 2
                                        : items[at].sort_order - 1;
  try {
    if (!items.some(t => t.id === id)) {
      await api(`/api/todos/${id}/file`, { method: 'POST', body: { folder_id: folder.id } });
    }
    await api(`/api/todos/${id}`, { method: 'PATCH', body: { sort_order: order } });
    await loadTodos(TODO.day);
  } catch (err) {
    toast(String(err.message || err), true);
  }
}

async function createFolder(name) {
  try {
    await api('/api/todos/folders', { method: 'POST', body: { name } });
    await loadTodos(TODO.day);
  } catch (err) {
    toast(String(err.message || err), true);
  }
}

async function renameFolder(id, name) {
  await patchFolder(id, { name });
}

async function patchFolder(id, patch) {
  try {
    await api(`/api/todos/folders/${id}`, { method: 'PATCH', body: patch });
    await loadTodos(TODO.day);
  } catch (err) {
    toast(String(err.message || err), true);
  }
}

async function deleteFolder(id) {
  try {
    await api(`/api/todos/folders/${id}/delete`, { method: 'POST', body: {} });
    await loadTodos(TODO.day);
  } catch (err) {
    toast(String(err.message || err), true);
  }
}

async function addTodo() {
  const draft = $('#todo-draft');
  const text = draft.value.trim();
  if (!text) return;
  draft.value = '';
  draft.style.height = 'auto';
  try {
    await api('/api/todos', {
      method: 'POST',
      // Whichever day is on screen, not whichever day it is.
      body: { body: text, scheduled_on: TODO.day },
    });
    await loadTodos(TODO.day);
  } catch (err) {
    draft.value = text;
    toast(String(err.message || err), true);
  }
}

function wireShelf() {
  $('#open-todo').onclick = toggleShelf;
  $('#shelf-close').onclick = closeShelf;
  $('#shelf-prev').onclick = () => loadTodos(shiftDay(TODO.day || todayIso(), -1));
  $('#shelf-next').onclick = () => loadTodos(shiftDay(TODO.day || todayIso(), 1));
  $('#shelf-today').onclick = () => loadTodos(null);
  $('#todo-send').onclick = addTodo;

  const draft = $('#todo-draft');
  draft.addEventListener('input', () => {
    draft.style.height = 'auto';
    draft.style.height = Math.min(draft.scrollHeight, 120) + 'px';
  });
  draft.addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      addTodo();
    }
  });
}

/// Only a fallback for the very first navigation, before the server has told
/// us what it considers today — its start hour is the authority, not ours.
function todayIso() {
  const now = new Date();
  return new Date(now.getTime() - now.getTimezoneOffset() * 60000)
    .toISOString().slice(0, 10);
}

boot().catch(err => {
  console.error(err);
  bootText('SOMETHING WENT WRONG', String(err.message || err));
});
