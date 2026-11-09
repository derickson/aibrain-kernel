// Everything the note editor needs from CodeMirror 6, re-exported from one
// module so the browser loads a single vendored file (no CDN, no bundler at
// runtime). Rebuild with ./build.sh.
export { EditorState, StateField, StateEffect, RangeSetBuilder, Prec, Compartment, Annotation, Transaction, EditorSelection } from '@codemirror/state';
export { EditorView, Decoration, ViewPlugin, WidgetType, keymap, drawSelection, highlightActiveLine, dropCursor, rectangularSelection, placeholder } from '@codemirror/view';
export { defaultKeymap, history, historyKeymap, indentWithTab, indentMore, indentLess } from '@codemirror/commands';
export { markdown, markdownLanguage, markdownKeymap, insertNewlineContinueMarkup, deleteMarkupBackward } from '@codemirror/lang-markdown';
export { syntaxTree, HighlightStyle, syntaxHighlighting, indentUnit, bracketMatching } from '@codemirror/language';
export { autocompletion, completionKeymap, closeBrackets, closeBracketsKeymap, startCompletion } from '@codemirror/autocomplete';
export { search, searchKeymap, highlightSelectionMatches } from '@codemirror/search';
export { tags } from '@lezer/highlight';
export { GFM, Strikethrough, TaskList, Table } from '@lezer/markdown';
