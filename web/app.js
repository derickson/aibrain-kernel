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
  query: '',
  brainFocus: null,
  hover: null,
  agentHover: null,
  statusText: null,
  drawerTab: 'brains',
  jobStreams: new Map(),
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

async function loadUniverse(rebuild = false) {
  const data = await api(`/api/universe${rebuild ? '?rebuild=1' : ''}`);
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

// ───────────────────────────── universe chrome ──────────────────────────

function renderChips() {
  const chips = $('#chips');
  chips.innerHTML = '';
  for (const brain of (S.data?.brains || [])) {
    const on = S.brainFocus === brain.id;
    const color = brain.sources?.[0]?.color || 'var(--accent)';
    chips.append(el('button', {
      class: 'chip',
      'aria-pressed': on ? 'true' : 'false',
      title: `${brain.total ?? brain.shown} notes · ${brain.path}`,
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
      el('span', { class: 'count' }, String(brain.shown))
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
  // Hovering a result lights its star up in the universe.
  if (item.gid != null && item.gid >= 0) {
    row.addEventListener('mouseenter', () => S.universe?.highlight([item.gid]));
    row.addEventListener('mouseleave', () => {
      S.universe?.highlight(S.results.map(r => r.gid).filter(g => g != null));
    });
  }
  return row;
}

function colorForBrain(brainId) {
  const brain = S.data?.brains.find(b => b.id === brainId);
  return brain?.sources?.[0]?.color || '#7fd8e8';
}

const runSearch = debounce(async text => {
  if (!text.trim()) {
    S.results = [];
    S.universe?.highlight(null);
    renderResults();
    return;
  }
  try {
    // Focusing a brain scopes the search to it, so the results and the lit
    // stars agree about which galaxy you are looking at.
    const scope = S.brainFocus ? `&brains=${encodeURIComponent(S.brainFocus)}` : '';
    const data = await api(`/api/search?q=${encodeURIComponent(text)}&limit=80${scope}`);
    if (data.query !== $('#query').value) return;   // a newer keystroke won
    S.results = data.results;
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

  if (message.text || message.role === 'user' || message.streaming) {
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
      onclick: () => {
        openNote(cite.nid, 'chat');
        const gid = S.universe?.gidForNote(cite.nid);
        if (gid != null && gid >= 0) S.universe.flyTo(gid);
      },
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
     tools: renderToolsPanel, view: renderViewPanel }[S.drawerTab])();
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
        el('span', {
          class: 'pip',
          style: `background:${colorForBrain(brain.id)};color:${colorForBrain(brain.id)}`,
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
        }, 'Remove'))
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
  $('#reset-view').onclick = () => {
    S.brainFocus = null;
    S.statusText = null;
    S.universe?.resetView();
    renderChips();
    renderStatus();
    if (S.query.trim()) runSearch(S.query);
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
  open: false,
  picking: null,    // the id whose note picker is showing
  loading: false,
};

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
  if (!TODO.items.length) {
    list.append(el('p', { class: 'shelf-empty' },
      TODO.day === TODO.today
        ? 'Nothing on today yet. Add the first thing below.'
        : 'Nothing on this day.'));
    return;
  }
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

function todoRow(item) {
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
    el('button', {
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

  return el('div', { class: 'todo', 'data-state': item.state }, box, col, acts);
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
