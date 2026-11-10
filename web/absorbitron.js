// The Absorbitron: sticky notes for links and files people send you.
//
// Each sticky is one intake note in `<vault>/Absorbitron/` of the brain that
// catches meeting transcripts. You type a title (it becomes part of the file
// name, after the creation time) and a free-text markdown body with the same
// live preview and [[wikilink]] completion as the notes panel.
//
// On disk the note is your raw input, then `---` and `# AI Notes` (a
// placeholder the research step will fill), then `# Attachments`. The sticky
// edits only the raw input; it keeps the sections below as they are, adding
// a link under `# Attachments` for every file you drop on it. If those
// markers are missing — the note was rewritten by hand — the sticky edits the
// whole file instead, so nothing is ever hidden from you or lost.
//
// Along the bottom: an unlit bulb for every URL in the raw input (each one a
// research target for later) and a black paperclip for every attachment.
// Both have a "done" state for when research and processing exist.

import { createEditor } from './editor.js';

const SAVE_DELAY = 1000;
const MARKER = /(^|\n)[ \t]*---[ \t]*\n+[ \t]*# AI Notes[ \t]*(?=\n|$)/;
const ATTACH_HEAD = /(^|\n)# Attachments[ \t]*(?=\n|$)/;
const URL_RE = /\bhttps?:\/\/[^\s<>"'`)\]]+/gi;
const WIKI_RE = /!?\[\[([^\]|#\n]+)(?:[|#][^\]\n]*)?\]\]/g;
const STAMP_RE = /^(\d{4}-\d{2}-\d{2}) (\d{2})(\d{2})(\d{2})?\s*/;

const SVG = {
  bulb: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 18h6M10 21h4M12 3a6 6 0 0 0-3.6 10.8c.6.5 1.1 1.3 1.1 2.2h5c0-.9.5-1.7 1.1-2.2A6 6 0 0 0 12 3z"/></svg>',
  clip: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M21 11.5l-8.5 8.5a5 5 0 0 1-7-7l9-9a3.5 3.5 0 0 1 5 5l-9 9a2 2 0 0 1-3-3l8-8"/></svg>',
};

/** `2026-11-10 1432 Title` → {title: 'Title', when: '2026-11-10 14:32'}. */
export function splitName(stem) {
  const m = STAMP_RE.exec(stem || '');
  if (!m) return { title: stem || '', when: '' };
  return { title: stem.slice(m[0].length), when: `${m[1]} ${m[2]}:${m[3]}` };
}

/** The note's text → {raw, rest}; rest is null when the markers are missing. */
export function splitDoc(text) {
  const m = MARKER.exec(text);
  if (!m) return { raw: text, rest: null };
  const at = m.index + m[1].length;
  return { raw: text.slice(0, at).replace(/\s+$/, ''), rest: text.slice(at) };
}

/** The inverse. The blank line keeps `---` a rule rather than a heading
 *  underline, and keeps an empty note from opening with frontmatter. */
export function joinDoc(raw, rest) {
  if (rest == null) return raw;
  return `${raw.replace(/\s+$/, '')}\n\n${rest.replace(/^\s+/, '')}`;
}

/** Every distinct URL in a piece of markdown, in order. */
export function findUrls(text) {
  const seen = new Set();
  for (const m of text.matchAll(URL_RE)) seen.add(m[0].replace(/[.,;:!?*_~]+$/, ''));
  return [...seen];
}

/** The wikilink targets listed under `# Attachments`. */
export function findAttachments(text) {
  const m = ATTACH_HEAD.exec(text);
  if (!m) return [];
  const body = text.slice(m.index + m[0].length);
  const next = body.search(/\n# /);
  const section = next < 0 ? body : body.slice(0, next);
  return [...section.matchAll(WIKI_RE)].map(x => x[1].trim());
}

/** `text` with `- [[path]]` added at the end of its `# Attachments` section
 *  (made if missing). */
export function addAttachment(text, path) {
  const line = `- [[${path}]]`;
  const m = ATTACH_HEAD.exec(text);
  if (!m) return `${text.replace(/\s+$/, '')}\n\n# Attachments\n\n${line}\n`;
  const start = m.index + m[0].length;
  const body = text.slice(start);
  const next = body.search(/\n# /);
  const end = next < 0 ? text.length : start + next;
  const section = text.slice(start, end).replace(/\s+$/, '');
  const head = text.slice(0, start);
  const tail = text.slice(end);
  const items = section ? `${section}\n${line}` : `\n\n${line}`;
  return `${head}${items}\n${tail.replace(/^\n+/, tail ? '\n' : '')}`;
}

export function createAbsorbitron({ api, toast, el, host, confirm, onOpenUrl, onOpenNote }) {
  const open = new Map();     // key → sticky; key is the nid, or a draft id
  let z = 30, drafts = 0, info = null;

  async function loadInfo() {
    try { info = await api('/api/absorbitron'); } catch { info = null; }
    return info;
  }

  async function searchTitles(query) {
    const q = query.trim();
    const data = await api(q ? `/api/search?q=${encodeURIComponent(q)}&limit=12`
                              : '/api/recent?limit=12').catch(() => ({ results: [] }));
    const seen = new Set();
    return (data.results || []).filter(r => {
      const name = r.name || r.title;
      if (!name || seen.has(name)) return false;
      seen.add(name);
      return true;
    }).map(r => ({ title: r.name || r.title, detail: r.brain || '' }));
  }

  async function call(path, body) {
    const out = await fetch(path, body === undefined ? {} : {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
    });
    return { status: out.status, data: await out.json().catch(() => ({})) };
  }

  // ───────────────────────────── one sticky ─────────────────────────────

  function makeSticky(key) {
    const st = {
      key, nid: null, name: '', brainId: '', rest: null, hash: '', savedText: '',
      sep: null, saving: false, again: false, timer: null, creating: false,
      conflict: null, missing: false, uploads: 0,
    };

    const title = el('input', {
      class: 'sticky-title', placeholder: 'Title — what is this?', spellcheck: 'true',
      maxlength: '160', autocomplete: 'off',
    });
    const when = el('span', { class: 'sticky-when mono' }, 'NEW');
    const state = el('span', { class: 'sticky-state mono' });
    const panelBtn = el('button', { class: 'sticky-btn', title: 'Open in the notes panel', hidden: true }, '⤢');
    const closeBtn = el('button', { class: 'sticky-btn', title: 'Close' }, '×');
    const head = el('header', { class: 'sticky-head' },
      el('span', { class: 'sticky-mark' }), el('span', { class: 'sticky-label mono' }, 'ABSORBITRON'),
      when, el('span', { class: 'sticky-spacer' }), state, panelBtn, closeBtn);
    const banner = el('div', { class: 'sticky-banner', hidden: true });
    const body = el('div', { class: 'sticky-body' });
    const fileInput = el('input', { type: 'file', multiple: true, hidden: true });
    const drop = el('div', { class: 'sticky-drop', hidden: true },
      el('div', { class: 'sticky-drop-art' }, '⇩'),
      el('div', {}, 'Drop files to attach'),
      el('button', { class: 'sticky-browse', onclick: () => fileInput.click() }, 'or browse…'));
    const glyphs = el('div', { class: 'sticky-glyphs' });
    const attachBtn = el('button', { class: 'sticky-attach', title: 'Attach a file' },
      el('span', { class: 'glyph clip', html: SVG.clip }), '+');
    const ai = el('div', { class: 'sticky-ai mono', title: 'Filled in by the AI research step (coming later)' },
      'AI NOTES · PENDING');
    const foot = el('footer', { class: 'sticky-foot' }, glyphs, attachBtn);
    const root = el('section', { class: 'sticky', role: 'dialog', 'aria-label': 'Absorbitron note' },
      head, title, banner, body, drop, ai, foot, fileInput);

    const editor = createEditor(body, {
      onChange: () => { renderGlyphs(); schedule(); },
      onSave: () => save(),
      onOpenLink: target => onOpenNote?.(target, st.brainId),
      onOpenUrl: href => onOpenUrl?.(href),
      searchTitles,
    });
    editor.setEditable(true);

    // ── text ──
    const raw = () => editor.state.doc.toString();
    const compose = () => joinDoc(raw(), st.rest);
    const toDisk = text => (st.sep ? text.replace(/\n/g, st.sep) : text);
    const dirty = () => st.nid != null && compose() !== st.savedText;

    function setRaw(text) {
      if (text === raw()) return;
      editor.replaceDoc(text);
    }

    /** Take in a note's text from disk as the new baseline. */
    function adopt(src) {
      st.sep = src.text.includes('\r\n') ? '\r\n' : null;
      const text = src.text.replace(/\r\n/g, '\n');
      const parts = splitDoc(text);
      st.rest = parts.rest;
      st.hash = src.hash;
      st.savedText = joinDoc(parts.raw, parts.rest);
      st.conflict = null;
      setRaw(parts.raw);
      // A file with no markers is shown whole; "saved" is then just the file.
      if (parts.rest == null) st.savedText = text;
      renderBanner();
      renderGlyphs();
      renderState();
    }

    function setIdentity(nid, stem, brainId) {
      if (st.key !== nid) { open.delete(st.key); st.key = nid; open.set(nid, st); }
      st.nid = nid;
      st.name = stem;
      st.brainId = brainId || st.brainId;
      const parts = splitName(stem);
      title.value = parts.title || stem;
      title.readOnly = true;
      title.title = `${stem}.md — the title is part of the file name; rename it in Obsidian`;
      when.textContent = parts.when || '';
      panelBtn.hidden = false;
    }

    // ── saving ──
    function schedule(delay = SAVE_DELAY) {
      clearTimeout(st.timer);
      renderState();
      if (!st.nid || st.conflict || st.missing) return;
      if (dirty()) st.timer = setTimeout(save, delay);
    }

    async function save() {
      clearTimeout(st.timer);
      if (!st.nid || st.conflict || st.missing) return;
      if (st.saving) { st.again = true; return; }
      const text = compose();
      if (text === st.savedText) { renderState(); return; }
      st.saving = true;
      renderState();
      let result;
      try {
        result = await call(`/api/note/${st.nid}/source`, { text: toDisk(text), baseHash: st.hash });
      } catch (err) {
        result = { status: 0, data: { error: String(err) } };
      }
      st.saving = false;
      if (result.status === 200) {
        st.hash = result.data.hash;
        st.savedText = text;
      } else if (result.status === 409) {
        st.conflict = { text: result.data.text || '', hash: result.data.hash || '' };
        renderBanner();
      } else if (result.status === 404) {
        st.missing = true;
        renderBanner();
      } else {
        toast(`Could not save the sticky: ${result.data.error || result.status}`, true);
      }
      renderState();
      if (st.again) { st.again = false; save(); } else if (dirty()) schedule();
    }

    async function create() {
      if (st.nid || st.creating) return st.nid;
      const name = title.value.trim();
      if (!name) {
        title.classList.add('want');
        title.focus();
        return null;
      }
      st.creating = true;
      renderState();
      try {
        const made = await api('/api/absorbitron/create', { method: 'POST', body: { title: name, text: raw() } });
        const src = await api(`/api/note/${made.nid}/source`);
        // Whatever was typed while the file was being made stays in the
        // editor and goes out with the next save; only the sections and the
        // baseline come from disk.
        const typed = raw();
        setIdentity(made.nid, made.name, made.brainId);
        adopt(src);
        setRaw(typed);
        schedule(200);
        toast(`Absorbed “${name}”`);
        return st.nid;
      } catch (err) {
        toast(`Could not create the note: ${err.message || err}`, true);
        return null;
      } finally {
        st.creating = false;
        renderState();
      }
    }

    // ── attachments ──
    async function attach(files) {
      files = [...(files || [])].filter(f => f && f.size >= 0);
      if (!files.length) return;
      if (!st.nid && !(await create())) {
        toast('Give the note a title first — attachments are filed under it', true);
        return;
      }
      if (st.missing) return;
      const max = info?.maxAttachmentBytes || 200 * 1024 * 1024;
      for (const file of files) {
        if (file.size > max) { toast(`${file.name} is too large to attach`, true); continue; }
        if (!file.size) { toast(`${file.name} is empty`, true); continue; }
        st.uploads++;
        renderGlyphs();
        try {
          const res = await fetch(`/api/absorbitron/attach?nid=${st.nid}&name=${encodeURIComponent(file.name)}`, {
            method: 'POST', headers: { 'Content-Type': 'application/octet-stream' }, body: file,
          });
          const out = await res.json().catch(() => ({}));
          if (!res.ok) throw new Error(out.error || res.status);
          if (st.rest == null) {
            // No sections to keep apart: the link goes into the text itself.
            editor.view.dispatch({ changes: { from: 0, to: editor.state.doc.length,
              insert: addAttachment(raw(), out.path) } });
          } else {
            st.rest = addAttachment(st.rest, out.path);
          }
          schedule(0);
        } catch (err) {
          toast(`Could not attach ${file.name}: ${err.message || err}`, true);
        } finally {
          st.uploads--;
          renderGlyphs();
        }
      }
      drop.hidden = true;
    }

    // ── rendering ──
    function renderGlyphs() {
      const text = raw();
      const urls = findUrls(text);
      const files = findAttachments(st.rest == null ? text : st.rest);
      glyphs.innerHTML = '';
      for (const url of urls) {
        glyphs.append(el('button', {
          class: 'glyph bulb', title: `Research target (not yet researched)\n${url}`,
          'aria-label': `Research target: ${url}`, html: SVG.bulb,
          onclick: () => onOpenUrl?.(url),
        }));
      }
      for (const path of files) {
        glyphs.append(el('span', {
          class: 'glyph clip', title: `Attachment (not yet processed)\n${path}`,
          'aria-label': `Attachment: ${path}`, html: SVG.clip,
        }));
      }
      for (let i = 0; i < st.uploads; i++) {
        glyphs.append(el('span', { class: 'glyph clip pending', title: 'Uploading…', html: SVG.clip }));
      }
      if (!urls.length && !files.length && !st.uploads) {
        glyphs.append(el('span', { class: 'sticky-empty mono' }, 'PASTE A LINK · DROP A FILE'));
      }
      ai.hidden = st.nid == null;
    }

    function renderState() {
      let text = '';
      if (st.creating) text = 'CREATING…';
      else if (!st.nid) text = title.value.trim() ? '↵ TO SAVE' : raw().trim() ? 'NAME IT TO SAVE' : '';
      else if (st.missing) text = 'MOVED';
      else if (st.conflict) text = 'CONFLICT';
      else if (st.saving) text = 'SAVING…';
      else if (dirty()) text = 'EDITED';
      else text = 'SAVED';
      state.textContent = text;
      state.dataset.kind = text.toLowerCase().replace(/[^a-z]/g, '');
    }

    function renderBanner() {
      banner.innerHTML = '';
      if (st.missing) {
        banner.hidden = false;
        banner.append(
          el('span', {}, 'This note was moved or deleted outside the sticky — it may have been filed. '),
          el('button', { class: 'sticky-link', onclick: () => navigator.clipboard?.writeText(compose())
            .then(() => toast('Copied the sticky’s text')) }, 'Copy text'));
        editor.setEditable(false);
        return;
      }
      if (st.conflict) {
        banner.hidden = false;
        banner.append(
          el('span', {}, 'Changed on disk while you were typing. '),
          el('button', { class: 'sticky-link', onclick: () => adopt(st.conflict) }, 'Use theirs'),
          el('span', {}, ' · '),
          el('button', { class: 'sticky-link', onclick: () => {
            st.hash = st.conflict.hash;
            // Keep the sections as they now stand on disk; only the raw
            // input is yours to force through.
            const theirs = splitDoc(st.conflict.text.replace(/\r\n/g, '\n'));
            if (st.rest != null && theirs.rest != null) st.rest = theirs.rest;
            st.conflict = null;
            renderBanner();
            save();
          } }, 'Keep mine'));
        return;
      }
      banner.hidden = true;
    }

    // ── wiring ──
    title.addEventListener('keydown', event => {
      if (event.key === 'Enter') { event.preventDefault(); create().then(n => { if (n) editor.focus(); }); }
      if (event.key === 'Tab' && !event.shiftKey) { event.preventDefault(); editor.focus(); }
    });
    title.addEventListener('input', () => { title.classList.remove('want'); renderState(); });
    title.addEventListener('blur', () => { if (title.value.trim() && !st.nid) create(); });
    closeBtn.addEventListener('click', () => close(st));
    panelBtn.addEventListener('click', async () => { await save(); if (st.nid) onOpenNote?.(null, st.brainId, st.nid); });
    attachBtn.addEventListener('click', () => { drop.hidden = !drop.hidden; });
    fileInput.addEventListener('change', () => { attach(fileInput.files); fileInput.value = ''; });

    // Keys typed here belong to the sticky: the app's shortcuts (`/` opens
    // search, Escape closes panels, the notes panel's ⌘ keys) must not fire.
    root.addEventListener('keydown', event => {
      if (event.key === 'Escape' && document.activeElement === title) title.blur();
      event.stopPropagation();
    });
    root.addEventListener('pointerdown', () => raise(st), true);

    // Files dropped anywhere on the sticky are attachments. Caught on the way
    // down, before CodeMirror's own drop handler reads them in as text.
    const hasFiles = e => [...(e.dataTransfer?.types || [])].includes('Files');
    let dragDepth = 0;
    root.addEventListener('dragenter', e => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      dragDepth++;
      root.classList.add('dropping');
      drop.hidden = false;
    }, true);
    root.addEventListener('dragover', e => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      e.stopPropagation();
      e.dataTransfer.dropEffect = 'copy';
    }, true);
    root.addEventListener('dragleave', e => {
      if (!hasFiles(e)) return;
      if (--dragDepth <= 0) { dragDepth = 0; root.classList.remove('dropping'); }
    }, true);
    root.addEventListener('drop', e => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      e.stopPropagation();
      dragDepth = 0;
      root.classList.remove('dropping');
      attach(e.dataTransfer.files);
    }, true);

    // Drag the sticky by its header.
    head.addEventListener('pointerdown', event => {
      if (event.target.closest('button')) return;
      event.preventDefault();
      const box = root.getBoundingClientRect(), frame = host.getBoundingClientRect();
      const dx = event.clientX - box.left, dy = event.clientY - box.top;
      head.setPointerCapture(event.pointerId);
      const move = e => {
        const x = Math.min(Math.max(0, e.clientX - frame.left - dx), frame.width - 80);
        const y = Math.min(Math.max(0, e.clientY - frame.top - dy), frame.height - 40);
        root.style.left = `${x}px`;
        root.style.top = `${y}px`;
      };
      const up = () => {
        head.removeEventListener('pointermove', move);
        head.removeEventListener('pointerup', up);
      };
      head.addEventListener('pointermove', move);
      head.addEventListener('pointerup', up);
    });

    Object.assign(st, { root, title, editor, save, adopt, setIdentity, renderGlyphs, renderState,
                        renderBanner, dirty, compose });
    renderGlyphs();
    renderState();
    return st;
  }

  // ─────────────────────────── the layer ────────────────────────────────

  function place(st) {
    const n = open.size - 1;
    st.root.style.left = `${24 + (n % 6) * 30}px`;
    st.root.style.top = `${72 + (n % 6) * 30}px`;
    host.append(st.root);
    raise(st);
  }

  function raise(st) {
    st.root.style.zIndex = String(++z);
  }

  async function close(st) {
    if (!st.nid && (st.title.value.trim() || st.editor.state.doc.length)) {
      const ok = await (confirm ? confirm('Discard this unsaved sticky?') : Promise.resolve(true));
      if (!ok) return;
    }
    if (st.nid && st.dirty()) await st.save();
    if (st.nid && st.dirty()) {
      const ok = await (confirm ? confirm('This sticky has unsaved changes. Close it anyway?') : Promise.resolve(true));
      if (!ok) return;
    }
    clearTimeout(st.timer);
    st.editor.view.destroy();
    st.root.remove();
    open.delete(st.key);
  }

  /** A blank sticky, ready for a title. */
  async function create() {
    if (!info) await loadInfo();
    if (!info?.brainId) {
      toast('Pick a brain as the transcript target first (☰ → Brains) — the Absorbitron writes there', true);
      return null;
    }
    const st = makeSticky(`draft-${++drafts}`);
    open.set(st.key, st);
    place(st);
    st.title.focus();
    return st;
  }

  /** Open an existing intake note, or bring it forward if it is open. */
  async function openNote(nid) {
    const have = open.get(nid);
    if (have) { raise(have); have.editor.focus(); return have; }
    let src;
    try { src = await api(`/api/note/${nid}/source`); } catch (err) {
      toast(`Could not open that note: ${err.message || err}`, true);
      return null;
    }
    if (open.has(nid)) return open.get(nid);
    const st = makeSticky(nid);
    open.set(nid, st);
    const stem = (src.relPath || src.name || '').split('/').pop().replace(/\.md$/i, '');
    st.setIdentity(nid, stem, src.brainId);
    st.adopt(src);
    place(st);
    return st;
  }

  /** A live event said this note changed on disk. */
  async function externalChange(nid) {
    const st = open.get(nid);
    if (!st || st.saving || st.creating) return;
    const { status, data } = await call(`/api/note/${nid}/source`).catch(() => ({ status: 0 }));
    if (status === 404) { st.missing = true; st.renderBanner(); st.renderState(); return; }
    if (status !== 200 || data.hash === st.hash) return;
    if (st.dirty()) { st.conflict = { text: data.text, hash: data.hash }; st.renderBanner(); st.renderState(); }
    else st.adopt(data);
  }

  /** Save everything; used when the page is going away. */
  function saveAll() { for (const st of open.values()) if (st.dirty()) st.save(); }

  loadInfo();
  return {
    create, open: openNote, externalChange, saveAll, refreshInfo: loadInfo,
    isOpen: nid => open.has(nid),
    get info() { return info; },
  };
}
