// Note tabs for the right panel, after Obsidian's: several notes open at
// once, pinned tabs that keep their note (a link followed from a pinned tab
// opens beside it), back/forward per tab, and every tab a live editor that
// saves itself a moment after you stop typing.
//
// app.js owns the panel around this; the workspace owns which notes are open
// and their unsaved text. Tabs (not their text) survive a reload through
// localStorage.

import { createEditor } from './editor.js';

const STORE_KEY = 'aibrain.workspace.v1';
const SAVE_DELAY = 1200;
// Browsers cap the bodies of all in-flight keepalive requests at 64 KiB
// together, so keepalive is kept for the page-closing save, and only small.
const KEEPALIVE_MAX = 60 * 1024;

/** The separator a file uses, so saving it does not rewrite every line. */
const lineSeparatorOf = text => (text.includes('\r\n') ? '\r\n' : null);
/** What goes to disk: CodeMirror splits on any line break and joins with
 *  `\n`, so a CRLF file is joined back with CRLF — every line, pasted ones
 *  included — instead of being rewritten as LF. */
const textOf = (state, sep) => {
  const text = state.doc.toString();
  return sep ? text.replace(/\n/g, sep) : text;
};

export function createWorkspace({ api, toast, el, onActivate, onOpenUrl, brainName }) {
  const W = {
    tabs: [],          // {key, nid, title, brainId, pinned, back: [nid], fwd: [nid]}
    active: null,      // key of the showing tab
    docs: new Map(),   // nid -> {state, hash, savedText, dirty, saving, timer, conflict}
  };
  let seq = 0;
  const newKey = () => `t${Date.now().toString(36)}${(seq++).toString(36)}`;

  const strip = el('div', { class: 'ws-tabs', role: 'tablist' });
  const nav = el('div', { class: 'ws-nav' });
  const backBtn = el('button', { class: 'icon-btn sm', title: 'Back (⌘⌥←)', onclick: () => go(-1) }, '←');
  const fwdBtn = el('button', { class: 'icon-btn sm', title: 'Forward (⌘⌥→)', onclick: () => go(1) }, '→');
  const saveState = el('span', { class: 'ws-save mono' });
  const pinBtn = el('button', { class: 'icon-btn sm', title: 'Pin tab', onclick: () => togglePin(W.active) }, '⌖');
  nav.append(backBtn, fwdBtn, el('span', { class: 'ws-spacer' }), saveState, pinBtn);
  const banner = el('div', { class: 'ws-banner', hidden: true });
  const title = el('h1', { class: 'ws-title' });
  const host = el('div', { class: 'ws-editor' });
  // Shown instead of the editor while a note loads, or when it cannot be
  // edited here (too large, not UTF-8, gone): then a read-only render.
  const fallback = el('div', { class: 'ws-fallback', hidden: true });
  fallback.addEventListener('click', event => {
    const link = event.target.closest?.('a.wiki[data-note]');
    if (!link) return;
    event.preventDefault();
    open(Number(link.dataset.note), { newTab: event.metaKey || event.ctrlKey });
  });
  // The note whose state is in the editor right now. Edits are credited to
  // this, never to "the active tab", which may still be loading.
  let shownNid = null;

  const editor = createEditor(host, {
    onChange: state => {
      const nid = shownNid;
      const doc = nid != null && W.docs.get(nid);
      if (!doc) return;
      doc.state = state;
      doc.dirty = textOf(state, doc.sep) !== doc.savedText;
      renderSave();
      clearTimeout(doc.timer);
      if (doc.dirty && !doc.conflict) doc.timer = setTimeout(() => save(nid), SAVE_DELAY);
    },
    onSave: () => { if (shownNid != null) save(shownNid); },
    onOpenLink: (target, how) => followLink(target, how),
    onOpenUrl: href => onOpenUrl?.(href),
    searchTitles: async query => {
      const q = query.trim();
      const data = await api(q ? `/api/search?q=${encodeURIComponent(q)}&limit=12`
                                : '/api/recent?limit=12').catch(() => ({ results: [] }));
      const seen = new Set();
      return (data.results || []).filter(r => {
        const name = r.name || r.title;
        if (!name || seen.has(name)) return false;
        seen.add(name);
        return true;
      }).map(r => ({ title: r.name || r.title, detail: r.brain || brainName?.(r.brainId) || '' }));
    },
  });

  // ─────────────────────────── persistence ───────────────────────────

  function persist() {
    try {
      localStorage.setItem(STORE_KEY, JSON.stringify({
        active: W.active,
        tabs: W.tabs.map(({ key, nid, title, brainId, pinned, back, fwd }) =>
          ({ key, nid, title, brainId, pinned, back: back.slice(-30), fwd: fwd.slice(0, 30) })),
      }));
    } catch { /* private window: tabs just won't come back */ }
  }

  function restore() {
    try {
      const saved = JSON.parse(localStorage.getItem(STORE_KEY) || 'null');
      if (!saved?.tabs?.length) return false;
      W.tabs = saved.tabs.map(t => ({ back: [], fwd: [], ...t }));
      W.active = W.tabs.some(t => t.key === saved.active) ? saved.active : W.tabs[0].key;
      renderTabs();
      return true;
    } catch { return false; }
  }

  // ─────────────────────────── documents ─────────────────────────────

  async function loadDoc(nid, { force = false } = {}) {
    let doc = W.docs.get(nid);
    if (doc && !force) return doc;
    const src = await api(`/api/note/${nid}/source`);
    const sep = lineSeparatorOf(src.text);
    if (doc && sep === doc.sep) {
      // reload over an open doc: keep its undo history by editing in place
      doc.hash = src.hash;
      doc.dirty = false;
      doc.conflict = null;
      if (textOf(doc.state, doc.sep) !== src.text) {
        if (shownNid === nid) {
          editor.replaceDoc(src.text);
          doc.state = editor.state;
        } else {
          doc.state = doc.state.update({ changes: { from: 0, to: doc.state.doc.length, insert: src.text } }).state;
        }
      }
      doc.savedText = textOf(doc.state, doc.sep);
    } else {
      const state = editor.newState(src.text);
      // `savedText` is the file as the editor would write it back. For a
      // file with consistent line endings that is the file itself; for one
      // with mixed endings it differs, but only an actual edit saves it.
      const fresh = { name: src.name, state, sep, hash: src.hash, savedText: textOf(state, sep),
                      dirty: false, saving: null, timer: null, conflict: null };
      if (doc) {
        Object.assign(doc, fresh);
        if (shownNid === nid) editor.setState(doc.state);
      } else {
        doc = fresh;
        W.docs.set(nid, doc);
      }
    }
    for (const tab of W.tabs) if (tab.nid === nid) { tab.title = src.name || tab.title; tab.brainId = src.brainId; }
    return doc;
  }

  /**
   * Write a note. Saves of one note run strictly one after another: each
   * waits for any save in flight, then re-reads the text and the hash that
   * save left behind, so a queued save never sends a stale `baseHash`.
   */
  async function save(nid, { overwrite = false, unloading = false } = {}) {
    const doc = W.docs.get(nid);
    if (!doc) return;
    clearTimeout(doc.timer);
    while (doc.saving) await doc.saving.catch(() => {});
    const text = textOf(doc.state, doc.sep);
    if (text === doc.savedText && !overwrite) { doc.dirty = false; renderSave(); return; }
    if (doc.conflict && !overwrite) return;
    const run = (async () => {
      try {
        const base = overwrite && doc.conflict ? doc.conflict.hash : doc.hash;
        const body = JSON.stringify({ text, baseHash: base });
        const out = await fetch(`/api/note/${nid}/source`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body,
          keepalive: unloading && new TextEncoder().encode(body).length < KEEPALIVE_MAX,
        });
        const data = await out.json().catch(() => ({}));
        if (out.status === 409) {
          doc.conflict = { text: data.text, hash: data.hash };
          renderBanner();
          return;
        }
        if (!out.ok) throw new Error(data.error || `save failed (${out.status})`);
        doc.hash = data.hash;
        doc.savedText = text;
        doc.conflict = null;
        doc.dirty = textOf(doc.state, doc.sep) !== text;
        renderBanner();
      } catch (err) {
        toast(`Not saved: ${err.message || err}`, true);
      }
    })();
    doc.saving = run;
    renderSave();
    try { await run; } finally { if (doc.saving === run) doc.saving = null; }
    renderSave();
  }

  function saveAll() { for (const nid of W.docs.keys()) save(nid); }

  /** The file changed on disk (live event). Pick it up unless we hold edits. */
  async function externalChange(nid) {
    const doc = W.docs.get(nid);
    if (!doc || doc.saving) return;
    try {
      const src = await api(`/api/note/${nid}/source`);
      if (src.hash === doc.hash) return;       // our own save coming back
      if (doc.dirty) {
        doc.conflict = { text: src.text, hash: src.hash };
        renderBanner();
      } else {
        await loadDoc(nid, { force: true });
        renderBanner();
      }
    } catch { /* deleted or unreachable; leave the text alone */ }
  }

  // ─────────────────────────── tabs ──────────────────────────────────

  const activeTab = () => W.tabs.find(t => t.key === W.active) || null;

  /**
   * Show note `nid`. Like Obsidian: it replaces the current tab's note
   * (pushing it on that tab's back stack) unless `newTab` is asked for or
   * the current tab is pinned; a note already open in another tab, when we
   * would otherwise make a new one, focuses that tab instead.
   */
  async function open(nid, { newTab = false, title = '' } = {}) {
    if (nid == null || nid < 0) return;
    let tab = activeTab();
    if (tab && tab.nid === nid) { await activate(tab.key); return; }
    if (!tab || newTab || tab.pinned) {
      const existing = !newTab && W.tabs.find(t => t.nid === nid);
      if (existing) { await activate(existing.key); return; }
      tab = { key: newKey(), nid, title, brainId: null, pinned: false, back: [], fwd: [] };
      const at = W.tabs.indexOf(activeTab());
      W.tabs.splice(at < 0 ? W.tabs.length : at + 1, 0, tab);
    } else {
      leave(tab);
      tab.back.push(tab.nid);
      tab.fwd = [];
      tab.nid = nid;
      tab.title = title;
    }
    await activate(tab.key);
  }

  /** Save what the tab being left holds before it stops being shown. */
  function leave(tab) {
    const doc = tab && W.docs.get(tab.nid);
    if (doc?.dirty) save(tab.nid);
  }

  async function activate(key) {
    const prev = activeTab();
    if (prev && prev.key !== key) leave(prev);
    W.active = key;
    const tab = activeTab();
    renderTabs();
    persist();
    // Nothing typed from here until the new note is in the editor can be
    // credited to the old one; hide the editor so nothing can be typed.
    shownNid = null;
    if (!tab) { showFallback(null); onActivate?.(null); return; }
    title.textContent = tab.title || '';
    if (!W.docs.has(tab.nid)) showFallback('Loading…');
    try {
      const doc = await loadDoc(tab.nid);
      if (W.active !== key) return;           // another click beat us
      editor.setState(doc.state);
      shownNid = tab.nid;
      showFallback(null);
      tab.title = doc.name || tab.title;
      title.textContent = tab.title;
      renderTabs();
      renderSave();
      renderBanner();
      persist();
      onActivate?.(tab);
    } catch (err) {
      if (W.active !== key) return;
      renderSave();
      renderBanner();
      showReadOnly(tab, err);
      onActivate?.(tab);
    }
  }

  /** Editor hidden behind a message, or (message = null) the editor back. */
  function showFallback(message) {
    host.hidden = message != null;
    fallback.hidden = message == null;
    fallback.className = 'ws-fallback';
    fallback.textContent = message ?? '';
  }

  /** A note the editor cannot open (too large, not UTF-8, deleted, its brain
   *  disabled) still reads: the rendered page, without editing. */
  async function showReadOnly(tab, err) {
    showFallback(null);
    host.hidden = true;
    fallback.hidden = false;
    fallback.className = 'ws-fallback';
    fallback.innerHTML = '';
    fallback.append(el('div', { class: 'ws-fallback-why mono' },
      `CAN'T EDIT HERE — ${String(err?.message || err || 'unavailable').toUpperCase()}`));
    try {
      const page = await api(`/api/note/${tab.nid}`);
      if (W.active !== tab.key || shownNid != null) return;
      tab.title = page.name || tab.title;
      title.textContent = tab.title;
      renderTabs();
      // `html` is the server's sanitized render, the same the old reader used.
      fallback.append(el('article', { class: 'note', html: page.html || '' }));
    } catch { /* the message above is all there is */ }
  }

  async function go(step) {
    const tab = activeTab();
    if (!tab) return;
    const from = step < 0 ? tab.back : tab.fwd;
    const to = step < 0 ? tab.fwd : tab.back;
    if (!from.length) return;
    leave(tab);
    const nid = step < 0 ? from.pop() : from.shift();
    if (step < 0) to.unshift(tab.nid); else to.push(tab.nid);
    tab.nid = nid;
    tab.title = '';
    await activate(tab.key);
  }

  async function close(key) {
    const i = W.tabs.findIndex(t => t.key === key);
    if (i < 0) return;
    const [tab] = W.tabs.splice(i, 1);
    leave(tab);
    if (!W.tabs.some(t => t.nid === tab.nid)) {
      const doc = W.docs.get(tab.nid);
      // Keep the doc until its save lands, then forget it.
      (doc?.saving || Promise.resolve()).finally(() => {
        if (!W.tabs.some(t => t.nid === tab.nid) && !W.docs.get(tab.nid)?.dirty) W.docs.delete(tab.nid);
      });
    }
    if (W.active === key) {
      const next = W.tabs[i] || W.tabs[i - 1];
      if (next) await activate(next.key);
      else { W.active = null; renderTabs(); persist(); onActivate?.(null); }
    } else { renderTabs(); persist(); }
  }

  function togglePin(key) {
    const tab = W.tabs.find(t => t.key === key);
    if (!tab) return;
    tab.pinned = !tab.pinned;
    renderTabs();
    persist();
  }

  function cycle(step) {
    if (W.tabs.length < 2) return;
    const i = W.tabs.findIndex(t => t.key === W.active);
    activate(W.tabs[(i + step + W.tabs.length) % W.tabs.length].key);
  }

  async function followLink(target, { newTab = false } = {}) {
    const tab = activeTab();
    const brain = tab?.brainId || '';
    const name = target.split('#')[0].trim();
    if (!name) return;                          // [[#heading]] — same note
    try {
      const hit = await api(`/api/resolve?brain=${encodeURIComponent(brain)}&title=${encodeURIComponent(name)}`);
      await open(hit.nid, { newTab, title: name });
    } catch {
      // Unresolved: Obsidian makes the note. Ask first; it writes a file.
      if (!brain) return;
      const make = await confirmCreate(name);
      if (!make) return;
      try {
        const made = await api('/api/note/create', { method: 'POST', body: { brain, title: name } });
        await open(made.nid, { newTab, title: name });
      } catch (err) { toast(`Could not create note: ${err.message || err}`, true); }
    }
  }

  function confirmCreate(name) {
    return new Promise(resolve => {
      const done = v => { scrim.remove(); resolve(v); };
      const scrim = el('div', { class: 'modal-scrim' },
        el('div', { class: 'modal' },
          el('p', { class: 'modal-message' }, `“${name}” doesn't exist yet. Create it in ${brainName?.(activeTab()?.brainId) || 'this brain'}?`),
          el('div', { class: 'modal-actions' },
            el('button', { class: 'btn ghost', onclick: () => done(false) }, 'Cancel'),
            el('button', { class: 'btn primary', onclick: () => done(true) }, 'Create note'))));
      scrim.addEventListener('keydown', e => { if (e.key === 'Escape') done(false); });
      document.body.append(scrim);
      scrim.querySelector('.btn.primary').focus();
    });
  }

  // ─────────────────────────── rendering ─────────────────────────────

  let dragKey = null;
  function renderTabs() {
    strip.innerHTML = '';
    for (const tab of W.tabs) {
      const doc = W.docs.get(tab.nid);
      const node = el('div', {
        class: `ws-tab${tab.key === W.active ? ' on' : ''}${tab.pinned ? ' pinned' : ''}`,
        role: 'tab', draggable: 'true', title: tab.title,
        onclick: () => activate(tab.key),
        onauxclick: e => { if (e.button === 1 && !tab.pinned) { e.preventDefault(); close(tab.key); } },
        ondblclick: () => togglePin(tab.key),
        oncontextmenu: e => { e.preventDefault(); tabMenu(tab, e); },
        ondragstart: e => { dragKey = tab.key; e.dataTransfer.effectAllowed = 'move'; },
        ondragover: e => { if (dragKey) { e.preventDefault(); node.classList.add('drop'); } },
        ondragleave: () => node.classList.remove('drop'),
        ondrop: e => {
          e.preventDefault();
          node.classList.remove('drop');
          const from = W.tabs.findIndex(t => t.key === dragKey);
          const to = W.tabs.indexOf(tab);
          if (from >= 0 && to >= 0 && from !== to) {
            const [moved] = W.tabs.splice(from, 1);
            W.tabs.splice(to, 0, moved);
            renderTabs();
            persist();
          }
          dragKey = null;
        },
      },
        tab.pinned ? el('span', { class: 'ws-pin', title: 'Pinned — double-click to unpin' }, '⌖') : null,
        el('span', { class: 'ws-name' }, tab.title || '…'),
        doc?.dirty ? el('span', { class: 'ws-dirty', title: 'Unsaved' }, '●') : null,
        tab.pinned ? null : el('button', {
          class: 'ws-x', title: 'Close (⌘⌥W)',
          onclick: e => { e.stopPropagation(); close(tab.key); },
        }, '×'),
      );
      strip.append(node);
    }
    const tab = activeTab();
    backBtn.disabled = !tab?.back.length;
    fwdBtn.disabled = !tab?.fwd.length;
    pinBtn.classList.toggle('on', !!tab?.pinned);
    pinBtn.title = tab?.pinned ? 'Unpin tab' : 'Pin tab';
    strip.hidden = !W.tabs.length;
  }

  let menu = null;
  function tabMenu(tab, event) {
    menu?.remove();
    const item = (label, fn) => el('button', { onclick: () => { menu.remove(); menu = null; fn(); } }, label);
    menu = el('div', { class: 'ws-menu', style: `left:${event.clientX}px;top:${event.clientY}px` },
      item(tab.pinned ? 'Unpin' : 'Pin', () => togglePin(tab.key)),
      item('Close', () => close(tab.key)),
      item('Close others', () => { for (const t of [...W.tabs]) if (t !== tab && !t.pinned) close(t.key); }),
      item('Close all unpinned', () => { for (const t of [...W.tabs]) if (!t.pinned) close(t.key); }),
    );
    document.body.append(menu);
    setTimeout(() => document.addEventListener('mousedown', function off(e) {
      if (!menu?.contains(e.target)) { menu?.remove(); menu = null; document.removeEventListener('mousedown', off); }
    }), 0);
  }

  function renderSave() {
    const tab = activeTab();
    const doc = tab && W.docs.get(tab.nid);
    saveState.textContent = !doc ? '' : doc.conflict ? 'CONFLICT' : doc.saving ? 'SAVING…'
      : doc.dirty ? 'EDITED' : 'SAVED';
    saveState.dataset.state = !doc ? '' : doc.conflict ? 'conflict' : doc.dirty || doc.saving ? 'dirty' : 'saved';
    // the dirty dot on the tab
    const node = strip.querySelector('.ws-tab.on');
    if (node) {
      const dot = node.querySelector('.ws-dirty');
      if (doc?.dirty && !dot) node.querySelector('.ws-name').after(el('span', { class: 'ws-dirty', title: 'Unsaved' }, '●'));
      if (!doc?.dirty && dot) dot.remove();
    }
  }

  function renderBanner() {
    const tab = activeTab();
    const doc = tab && W.docs.get(tab.nid);
    banner.innerHTML = '';
    banner.hidden = !doc?.conflict;
    if (!doc?.conflict) { renderSave(); return; }
    banner.append(
      el('span', {}, 'This note changed on disk while you were editing.'),
      el('button', { class: 'btn ghost sm', onclick: async () => {
        doc.conflict = null;
        await loadDoc(tab.nid, { force: true });
        if (activeTab()?.nid === tab.nid) editor.setState(W.docs.get(tab.nid).state);
        renderBanner();
      } }, 'Use disk version'),
      el('button', { class: 'btn primary sm', onclick: () => save(tab.nid, { overwrite: true }) }, 'Keep mine'),
    );
    renderSave();
  }

  // ─────────────────────────── keys ──────────────────────────────────

  function onKey(event) {
    const mod = event.metaKey || event.ctrlKey;
    if (mod && event.altKey && event.key === 'ArrowLeft') { event.preventDefault(); go(-1); return true; }
    if (mod && event.altKey && event.key === 'ArrowRight') { event.preventDefault(); go(1); return true; }
    if (mod && event.altKey && (event.key === 'w' || event.key === '∑')) {
      event.preventDefault();
      const t = activeTab();
      if (t && !t.pinned) close(t.key);
      return true;
    }
    if (event.ctrlKey && event.key === 'Tab') { event.preventDefault(); cycle(event.shiftKey ? -1 : 1); return true; }
    return false;
  }

  window.addEventListener('beforeunload', event => {
    let pending = false;
    for (const [nid, doc] of W.docs) if (doc.dirty) { pending = true; save(nid, { unloading: true }); }
    if (pending) event.preventDefault();
  });
  window.addEventListener('blur', saveAll);

  return {
    strip, nav, banner, title, host, fallback,
    open, close, activate, restore, externalChange, onKey, saveAll,
    get activeTab() { return activeTab(); },
    get hasTabs() { return W.tabs.length > 0; },
    hasNid: nid => W.docs.has(nid),
    focus: () => editor.focus(),
  };
}
