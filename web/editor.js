// An Obsidian-style "live preview" markdown editor on CodeMirror 6.
//
// The text stays markdown — what is saved is exactly what you typed — but the
// syntax is drawn the way it reads: `#` marks, `**`, `[[` `]]` and link URLs
// fold away unless the cursor is touching them, list dashes become bullets,
// `- [ ]` becomes a checkbox you can click, and headings get their size.
//
// Exports one function, createEditor(); the workspace (tabs, saving) owns
// everything about which note is in it.

import {
  EditorState, EditorSelection, RangeSetBuilder, Prec, Compartment,
  EditorView, Decoration, ViewPlugin, WidgetType, keymap, drawSelection,
  dropCursor, rectangularSelection,
  defaultKeymap, history, historyKeymap, indentMore, indentLess,
  markdown, markdownLanguage, insertNewlineContinueMarkup, deleteMarkupBackward,
  syntaxTree, HighlightStyle, syntaxHighlighting, indentUnit,
  autocompletion, completionKeymap, closeBrackets, closeBracketsKeymap,
  search, searchKeymap, highlightSelectionMatches,
  tags, GFM,
} from './vendor/codemirror.js';

// ───────────────────────────── list toggling ────────────────────────────

// indent, list marker (`-`, `*`, `+`, `1.`), optional task box, the rest.
const LIST_RE = /^(\s*)(?:([-*+]|\d+[.)])\s+)?(?:\[([ xX])\]\s+)?(.*)$/;

/**
 * Cmd/Ctrl-Enter: cycle each selected line through
 *   text → `- [ ] text` → `- [x] text` → `- text` → text …
 * so a plain markdown file can serve as a to-do list, and one more press
 * past a bullet takes the line back to plain text. Indentation and numbered
 * markers are kept.
 */
export function cycleListItem(view) {
  const { state } = view;
  if (state.readOnly) return true;
  const changes = [];
  const seen = new Set();
  for (const range of state.selection.ranges) {
    for (let pos = range.from; pos <= range.to;) {
      const line = state.doc.lineAt(pos);
      if (!seen.has(line.number)) {
        seen.add(line.number);
        const next = nextListState(line.text);
        if (next !== line.text) changes.push({ from: line.from, to: line.to, insert: next });
      }
      pos = line.to + 1;
    }
  }
  if (!changes.length) return true;
  // Keep the cursor at the same spot in the item's text, not the line.
  const tx = state.update({ changes, scrollIntoView: true, userEvent: 'input.list' });
  const sel = EditorSelection.create(state.selection.ranges.map(r => {
    const line = state.doc.lineAt(r.head);
    const before = line.text.length - LIST_RE.exec(line.text)[4].length;
    const offset = Math.max(0, r.head - line.from - before);
    const newLine = tx.state.doc.lineAt(tx.changes.mapPos(line.from));
    const nb = newLine.text.length - LIST_RE.exec(newLine.text)[4].length;
    const head = newLine.from + Math.min(nb + offset, newLine.length);
    return r.empty ? EditorSelection.cursor(head) : EditorSelection.range(
      tx.changes.mapPos(r.anchor), tx.changes.mapPos(r.head));
  }), state.selection.mainIndex);
  view.dispatch({ changes, selection: sel, scrollIntoView: true, userEvent: 'input.list' });
  return true;
}

function nextListState(text) {
  const [, indent, marker, box, rest] = LIST_RE.exec(text);
  if (!marker) return `${indent}- [ ] ${rest}`;                 // text → open task
  if (box === ' ') return `${indent}${marker} [x] ${rest}`;     // open → done
  if (box !== undefined) return `${indent}${marker} ${rest}`;   // done → bullet
  return `${indent}${rest}`;                                    // bullet → text
}

function toggleTaskAt(view, pos) {
  if (view.state.readOnly) return false;
  const line = view.state.doc.lineAt(pos);
  const m = /^(\s*(?:[-*+]|\d+[.)])\s+\[)([ xX])\]/.exec(line.text);
  if (!m) return false;
  const at = line.from + m[1].length;
  view.dispatch({
    changes: { from: at, to: at + 1, insert: m[2] === ' ' ? 'x' : ' ' },
    userEvent: 'input.toggle',
  });
  return true;
}

/** Wrap the selection in `mark` (or unwrap it when it already is). */
function wrapWith(mark) {
  return view => {
    const { state } = view;
    if (state.readOnly) return true;
    view.dispatch(state.changeByRange(range => {
      const len = mark.length;
      const before = state.sliceDoc(range.from - len, range.from);
      const after = state.sliceDoc(range.to, range.to + len);
      if (before === mark && after === mark) {
        return {
          changes: [{ from: range.from - len, to: range.from }, { from: range.to, to: range.to + len }],
          range: EditorSelection.range(range.from - len, range.to - len),
        };
      }
      return {
        changes: [{ from: range.from, insert: mark }, { from: range.to, insert: mark }],
        range: EditorSelection.range(range.from + len, range.to + len),
      };
    }), { userEvent: 'input' });
    return true;
  };
}

// ───────────────────────────── widgets ──────────────────────────────────

class CheckboxWidget extends WidgetType {
  constructor(checked) { super(); this.checked = checked; }
  eq(other) { return other.checked === this.checked; }
  toDOM() {
    const box = document.createElement('span');
    box.className = 'cm-task' + (this.checked ? ' done' : '');
    box.setAttribute('role', 'checkbox');
    box.setAttribute('aria-checked', String(this.checked));
    return box;
  }
  ignoreEvent() { return false; }
}

class BulletWidget extends WidgetType {
  constructor(depth) { super(); this.depth = depth; }
  eq(other) { return other.depth === this.depth; }
  toDOM() {
    const dot = document.createElement('span');
    dot.className = 'cm-bullet';
    dot.textContent = ['•', '◦', '▪'][this.depth % 3];
    return dot;
  }
}

class RuleWidget extends WidgetType {
  eq() { return true; }
  toDOM() {
    const hr = document.createElement('span');
    hr.className = 'cm-hr';
    return hr;
  }
}

const hide = Decoration.replace({});
const mark = cls => Decoration.mark({ class: cls });
const lineCls = cls => Decoration.line({ class: cls });

// ───────────────────────────── live preview ─────────────────────────────

const FRONTMATTER_RE = /^---\n[\s\S]*?\n(?:---|\.\.\.)(?:\n|$)/;
const WIKI_RE = /(!?)\[\[([^\[\]\n]+?)\]\]/g;
const TAG_RE = /(^|[\s(])(#[\p{L}\p{N}_\-/]*[\p{L}_\-/][\p{L}\p{N}_\-/]*)/gu;
const HILITE_RE = /==([^=\n](?:[^\n]*?[^=\n])?)==/g;

/** Does any selection range touch [from, to]? Touching counts, as in
 *  Obsidian: the cursor parked just after `**bold**` reveals the stars. */
function touched(state, from, to) {
  for (const r of state.selection.ranges) if (r.from <= to && r.to >= from) return true;
  return false;
}

function lineTouched(state, pos) {
  const line = state.doc.lineAt(pos);
  return touched(state, line.from, line.to);
}

function buildDecorations(view) {
  const { state } = view;
  const doc = state.doc;
  const fm = FRONTMATTER_RE.exec(doc.sliceString(0, Math.min(doc.length, 20000)));
  const fmEnd = fm ? fm[0].length : 0;
  // Collected unsorted, sorted once at the end — RangeSetBuilder wants order.
  const out = [];
  const add = (from, to, deco) => { if (to >= from) out.push({ from, to, deco }); };
  const focused = view.hasFocus;
  // With the editor blurred everything renders, the way Obsidian looks
  // when you are reading rather than typing.
  const near = (from, to) => focused && touched(state, from, to);
  const nearLine = pos => focused && lineTouched(state, pos);
  const codeRanges = [];

  if (fmEnd) {
    for (let pos = 0; pos < fmEnd;) {
      const line = doc.lineAt(pos);
      add(line.from, line.from, lineCls('cm-frontmatter'));
      pos = line.to + 1;
    }
  }

  for (const { from, to } of view.visibleRanges) {
    syntaxTree(state).iterate({
      from, to,
      enter: node => {
        if (node.to <= fmEnd) return false;
        const name = node.name;
        const heading = /^ATXHeading(\d)$/.exec(name);
        if (heading) {
          add(node.from, node.from, lineCls(`cm-h cm-h${heading[1]}`));
          return;
        }
        if (/^SetextHeading(\d)$/.test(name)) {
          add(node.from, node.from, lineCls(`cm-h cm-h${name.slice(-1)}`));
          return;
        }
        switch (name) {
          case 'HeaderMark': {
            const parent = node.node.parent?.name || '';
            if (parent.startsWith('ATX') && !nearLine(node.from)) {
              // the marks and the one space after them
              const end = doc.sliceString(node.to, node.to + 1) === ' ' ? node.to + 1 : node.to;
              add(node.from, end, hide);
            } else {
              add(node.from, node.to, mark('cm-md-mark'));
            }
            return;
          }
          case 'Emphasis': add(node.from, node.to, mark('cm-em')); break;
          case 'StrongEmphasis': add(node.from, node.to, mark('cm-strong')); break;
          case 'Strikethrough': add(node.from, node.to, mark('cm-strike')); break;
          case 'InlineCode': add(node.from, node.to, mark('cm-inline-code')); break;
          case 'EmphasisMark':
          case 'StrikethroughMark':
          case 'CodeMark': {
            const parent = node.node.parent;
            if (parent?.name === 'FencedCode') { add(node.from, node.to, mark('cm-md-mark')); return; }
            if (parent && !near(parent.from, parent.to)) add(node.from, node.to, hide);
            else add(node.from, node.to, mark('cm-md-mark'));
            return;
          }
          case 'Link': {
            // [text](url): show only the text, styled as a link.
            const marks = [];
            let url = null;
            for (let c = node.node.firstChild; c; c = c.nextSibling) {
              if (c.name === 'LinkMark') marks.push(c);
              if (c.name === 'URL') url = c;
            }
            if (marks.length >= 2) {
              const textFrom = marks[0].to, textTo = marks[1].from;
              const href = url ? doc.sliceString(url.from, url.to) : '';
              add(textFrom, textTo, Decoration.mark({
                class: 'cm-link', attributes: { 'data-href': href },
              }));
              if (!near(node.from, node.to)) {
                add(node.from, textFrom, hide);
                add(textTo, node.to, hide);
              } else {
                add(node.from, textFrom, mark('cm-md-mark'));
                add(textTo, node.to, mark('cm-md-mark cm-url'));
              }
            }
            return false;
          }
          case 'URL':
            if (node.node.parent?.name !== 'Link') add(node.from, node.to, Decoration.mark({
              class: 'cm-link', attributes: { 'data-href': doc.sliceString(node.from, node.to) },
            }));
            return;
          case 'Blockquote':
            for (let pos = node.from; pos <= node.to;) {
              const line = doc.lineAt(pos);
              add(line.from, line.from, lineCls('cm-quote'));
              pos = line.to + 1;
            }
            break;
          case 'QuoteMark':
            if (!nearLine(node.from)) {
              const end = doc.sliceString(node.to, node.to + 1) === ' ' ? node.to + 1 : node.to;
              add(node.from, end, hide);
            } else add(node.from, node.to, mark('cm-md-mark'));
            return;
          case 'HorizontalRule':
            if (!nearLine(node.from)) add(node.from, node.to, Decoration.replace({ widget: new RuleWidget() }));
            else add(node.from, node.to, mark('cm-md-mark'));
            return;
          case 'FencedCode':
          case 'CodeBlock': {
            codeRanges.push([node.from, node.to]);
            const first = doc.lineAt(node.from).number, last = doc.lineAt(node.to).number;
            for (let n = first; n <= last; n++) {
              const line = doc.line(n);
              const edge = n === first ? ' cm-codeblock-first' : n === last ? ' cm-codeblock-last' : '';
              add(line.from, line.from, lineCls('cm-codeblock' + edge));
            }
            return false;
          }
          case 'ListItem': {
            const markNode = node.node.getChild('ListMark');
            if (!markNode) return;
            const task = node.node.getChild('Task');
            const taskMarker = task?.getChild('TaskMarker');
            const depth = depthOf(node.node);
            if (taskMarker && doc.lineAt(taskMarker.from).number === doc.lineAt(markNode.from).number) {
              const checked = /x/i.test(doc.sliceString(taskMarker.from, taskMarker.to));
              const line = doc.lineAt(markNode.from);
              add(line.from, line.from, lineCls('cm-task-line' + (checked ? ' cm-task-done' : '')));
              const end = doc.sliceString(taskMarker.to, taskMarker.to + 1) === ' ' ? taskMarker.to + 1 : taskMarker.to;
              if (!near(markNode.from, taskMarker.to)) {
                add(markNode.from, end, Decoration.replace({ widget: new CheckboxWidget(checked) }));
              } else {
                add(markNode.from, taskMarker.to, mark('cm-md-mark'));
              }
            } else {
              const ordered = node.node.parent?.name === 'OrderedList';
              if (ordered) add(markNode.from, markNode.to, mark('cm-ol-mark'));
              else if (!near(markNode.from, markNode.to)) {
                add(markNode.from, markNode.to, Decoration.replace({ widget: new BulletWidget(depth) }));
              } else add(markNode.from, markNode.to, mark('cm-md-mark'));
            }
            return;
          }
        }
      },
    });

    // Things the markdown grammar does not know: wikilinks, #tags, ==marks==.
    const text = doc.sliceString(from, to);
    const inCode = pos => codeRanges.some(([a, b]) => pos >= a && pos <= b) || inInlineCode(state, pos);
    for (const m of text.matchAll(WIKI_RE)) {
      const start = from + m.index, end = start + m[0].length;
      if (start < fmEnd || inCode(start)) continue;
      const inner = m[2];
      const pipe = inner.indexOf('|');
      const target = (pipe >= 0 ? inner.slice(0, pipe) : inner).trim();
      const embed = m[1] === '!';
      const open = start + m[1].length + 2;              // after `[[`
      const shownFrom = pipe >= 0 ? open + pipe + 1 : open;
      const shownTo = end - 2;
      const raw = near(start, end);
      add(shownFrom, shownTo, Decoration.mark({
        class: embed ? 'cm-wikilink cm-embed' : 'cm-wikilink',
        attributes: raw ? { 'data-target': target, 'data-raw': '1' } : { 'data-target': target },
      }));
      if (!raw) {
        add(start, shownFrom, hide);
        add(shownTo, end, hide);
      } else {
        add(start, shownFrom, mark('cm-md-mark'));
        add(shownTo, end, mark('cm-md-mark'));
      }
    }
    for (const m of text.matchAll(TAG_RE)) {
      const start = from + m.index + m[1].length;
      if (start < fmEnd || inCode(start)) continue;
      add(start, start + m[2].length, mark('cm-tag'));
    }
    for (const m of text.matchAll(HILITE_RE)) {
      const start = from + m.index, end = start + m[0].length;
      if (start < fmEnd || inCode(start)) continue;
      add(start, end, mark('cm-hilite'));
      if (!near(start, end)) { add(start, start + 2, hide); add(end - 2, end, hide); }
    }
  }

  // RangeSetBuilder wants ranges by start, then by side (line decorations
  // sort first). Replacements may not overlap one another; drop any that
  // would rather than let a pathological note break the editor.
  out.sort((a, b) => a.from - b.from || a.deco.startSide - b.deco.startSide);
  const builder = new RangeSetBuilder();
  let lastReplaceEnd = -1;
  for (const { from, to, deco } of out) {
    if (deco.spec.widget !== undefined || deco === hide) {
      if (from < lastReplaceEnd) continue;
      lastReplaceEnd = to;
    }
    try { builder.add(from, to, deco); } catch { /* out of order: skip */ }
  }
  return builder.finish();
}

function depthOf(node) {
  let depth = 0;
  for (let p = node.parent; p; p = p.parent) if (p.name === 'BulletList' || p.name === 'OrderedList') depth++;
  return Math.max(0, depth - 1);
}

function inInlineCode(state, pos) {
  const node = syntaxTree(state).resolveInner(pos, 1);
  for (let n = node; n; n = n.parent) if (n.name === 'InlineCode' || n.name === 'FencedCode') return true;
  return false;
}

const livePreview = ViewPlugin.fromClass(class {
  constructor(view) { this.decorations = buildDecorations(view); }
  update(u) {
    if (u.docChanged || u.viewportChanged || u.selectionSet || u.focusChanged
        || syntaxTree(u.startState) !== syntaxTree(u.state)) {
      this.decorations = buildDecorations(u.view);
    }
  }
}, { decorations: v => v.decorations });

// ───────────────────────────── theme ────────────────────────────────────

const highlight = HighlightStyle.define([
  { tag: tags.monospace, fontFamily: "'JetBrains Mono', ui-monospace, monospace" },
  { tag: tags.processingInstruction, color: 'var(--fg-mute)' },
  { tag: tags.meta, color: 'var(--fg-mute)' },
  { tag: tags.comment, color: 'var(--fg-mute)' },
  { tag: tags.keyword, color: '#b48cff' },
  { tag: tags.string, color: '#9fd8a8' },
  { tag: tags.number, color: '#f0c070' },
]);

const theme = EditorView.theme({
  '&': { color: '#cfd9e5', backgroundColor: 'transparent', fontSize: '14px' },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller': { fontFamily: 'Manrope, system-ui, sans-serif', lineHeight: '1.7', overflow: 'visible' },
  '.cm-content': { padding: '0 0 40vh', caretColor: 'var(--accent)' },
  '.cm-line': { padding: '0 2px' },
  '.cm-cursor': { borderLeftColor: 'var(--accent)', borderLeftWidth: '2px' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection':
    { backgroundColor: 'rgba(127, 216, 232, .22) !important' },
  '.cm-tooltip': { background: '#0b111b', border: '1px solid var(--line-mid)', borderRadius: '8px' },
  '.cm-tooltip-autocomplete > ul > li': { padding: '4px 10px !important', color: 'var(--fg-dim)' },
  '.cm-tooltip-autocomplete > ul > li[aria-selected]': { background: 'rgba(127,216,232,.14)', color: '#fff' },
  '.cm-completionDetail': { color: 'var(--fg-mute)', fontStyle: 'normal', marginLeft: '8px', fontSize: '11px' },
  '.cm-panels': { background: '#0b111b', color: 'var(--fg-dim)', borderBottom: '1px solid var(--line)' },
  '.cm-searchMatch': { backgroundColor: 'rgba(240,192,48,.25)' },
}, { dark: true });

// ───────────────────────────── wikilink completion ──────────────────────

function wikiCompletion(searchTitles) {
  return async context => {
    const before = context.matchBefore(/\[\[[^\[\]|#\n]*$/);
    if (!before) return null;
    const query = before.text.slice(2);
    const from = before.from + 2;
    let titles = [];
    try { titles = await searchTitles(query); } catch { return null; }
    if (context.aborted) return null;
    const closes = context.state.sliceDoc(context.pos, context.pos + 2) === ']]';
    return {
      from,
      filter: false,
      options: titles.map(t => ({
        label: t.title,
        detail: t.detail || '',
        apply: (view, _c, f, to) => {
          const insert = closes ? t.title : `${t.title}]]`;
          view.dispatch({
            changes: { from: f, to, insert },
            selection: { anchor: f + t.title.length + 2 },
            userEvent: 'input.complete',
          });
        },
      })),
    };
  };
}

// ───────────────────────────── the editor ───────────────────────────────

/**
 * @param parent      element to mount into
 * @param opts.onChange(state)           every doc change
 * @param opts.onOpenLink(target, how)   a [[wikilink]] was clicked; how = {newTab}
 * @param opts.onOpenUrl(href)           an external link was clicked
 * @param opts.onSave()                  Mod-s
 * @param opts.searchTitles(query) → Promise<[{title, detail}]>
 */
// Reading vs editing. Reconfigured on the live view, not baked into each
// note's state, so one switch covers whichever note is showing.
const editability = new Compartment();
const editable = on => [EditorState.readOnly.of(!on), EditorView.editable.of(on)];

export function createEditor(parent, opts) {
  const extensions = [editability.of(editable(false)), makeExtensions(opts)];
  let canEdit = false;
  const view = new EditorView({
    parent,
    state: EditorState.create({ doc: '', extensions }),
  });
  return {
    view,
    /** A fresh state for a newly opened note; the workspace caches these per tab. */
    newState(doc) { return EditorState.create({ doc, extensions }); },
    /** Swap in a note's state, in the current reading/editing mode. */
    setState(state) {
      view.setState(state);
      view.dispatch({ effects: editability.reconfigure(editable(canEdit)) });
    },
    /** Reading (false) or editing (true). Leaving editing drops the cursor,
     *  so the note renders fully, as Obsidian's reading view does. */
    setEditable(on) {
      canEdit = !!on;
      if (!canEdit) view.contentDOM.blur();
      view.dispatch({ effects: editability.reconfigure(editable(canEdit)) });
    },
    get state() { return view.state; },
    focus() { view.focus(); },
    /** Replace the text (an edit on disk), keeping the cursor about where it was. */
    replaceDoc(text) {
      const head = Math.min(view.state.selection.main.head, text.length);
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: text },
        selection: { anchor: head },
        userEvent: 'external',
        annotations: [],
      });
    },
  };
}

function makeExtensions(opts) {
  const linkClicks = EditorView.domEventHandlers({
    mousedown(event, view) {
      if (event.button !== 0) return false;
      const box = event.target.closest?.('.cm-task');
      if (box) {
        const pos = view.posAtDOM(box);
        event.preventDefault();
        return toggleTaskAt(view, pos);
      }
      const wiki = event.target.closest?.('.cm-wikilink');
      if (wiki && !wiki.classList.contains('cm-embed')) {
        // A plain click follows the link, as in Obsidian's live preview,
        // unless the raw `[[…]]` is showing — then you are editing it.
        if (!wiki.dataset.raw || event.metaKey || event.ctrlKey) {
          event.preventDefault();
          opts.onOpenLink?.(wiki.dataset.target, { newTab: event.metaKey || event.ctrlKey });
          return true;
        }
      }
      const link = event.target.closest?.('.cm-link');
      if (link && (event.metaKey || event.ctrlKey || !view.hasFocus)) {
        event.preventDefault();
        opts.onOpenUrl?.(link.dataset.href);
        return true;
      }
      return false;
    },
    auxclick(event) {
      // middle-click a wikilink: open it in a new tab
      const wiki = event.button === 1 && event.target.closest?.('.cm-wikilink');
      if (wiki) {
        event.preventDefault();
        opts.onOpenLink?.(wiki.dataset.target, { newTab: true });
        return true;
      }
      return false;
    },
  });

  return [
    history(),
    drawSelection(),
    dropCursor(),
    rectangularSelection(),
    indentUnit.of('    '),
    EditorState.tabSize.of(4),
    EditorView.lineWrapping,
    closeBrackets(),
    highlightSelectionMatches(),
    search({ top: true }),
    autocompletion({
      override: [wikiCompletion(opts.searchTitles || (async () => []))],
      icons: false,
      activateOnTyping: true,
    }),
    markdown({ base: markdownLanguage, extensions: [GFM], addKeymap: false }),
    syntaxHighlighting(highlight),
    livePreview,
    theme,
    linkClicks,
    EditorView.contentAttributes.of({ spellcheck: 'true', autocorrect: 'on', autocapitalize: 'sentences' }),
    EditorView.updateListener.of(u => { if (u.docChanged) opts.onChange?.(u.state, u); }),
    Prec.high(keymap.of([
      { key: 'Mod-Enter', run: cycleListItem },
      { key: 'Enter', run: insertNewlineContinueMarkup },
      { key: 'Backspace', run: deleteMarkupBackward },
      { key: 'Tab', run: indentMore },
      { key: 'Shift-Tab', run: indentLess },
      { key: 'Mod-b', run: wrapWith('**') },
      { key: 'Mod-i', run: wrapWith('*') },
      { key: 'Mod-s', run: () => { opts.onSave?.(); return true; } },
      { key: 'Escape', run: view => { view.contentDOM.blur(); return true; } },
    ])),
    keymap.of([
      ...closeBracketsKeymap,
      ...completionKeymap,
      ...searchKeymap,
      ...historyKeymap,
      ...defaultKeymap,
    ]),
  ];
}
