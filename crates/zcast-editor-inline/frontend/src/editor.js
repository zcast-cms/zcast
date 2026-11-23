/* zcast-editor-inline — pre-bundled TipTap editor (DEC-17).
 *
 * This source is only needed while developing the editor. The committed
 * ../assets/editor.bundle.js is what Rust embeds, so `cargo build` stays
 * 100% pure Rust with no Node.js dependency.
 */

import { Editor, Extension } from '@tiptap/core';
import StarterKit from '@tiptap/starter-kit';
import Link from '@tiptap/extension-link';
import Underline from '@tiptap/extension-underline';
import Placeholder from '@tiptap/extension-placeholder';
import { Plugin, PluginKey } from '@tiptap/pm/state';
import { Decoration, DecorationSet } from '@tiptap/pm/view';
import MarkdownIt from 'markdown-it';

const API_NOTES = '/_zcast/api/notes';
const SAVE_DELAY_MS = 800;
const AUTOCOMPLETE_DELAY_MS = 120;
const SAVED_STATUS_MS = 2200;
const HINT_STATUS_MS = 3200;

const markdown = new MarkdownIt({
  html: true,
  linkify: false,
  typographer: false,
  breaks: false,
});

/** card element -> editor state */
const editorStates = new WeakMap();
/** editing states, so Cmd+S can find the active editor */
const liveStates = new Set();
/** status element -> timeout */
const statusTimers = new WeakMap();

let floatingToolbar = null;
let toolbarOwner = null;
let autocompleteMenu = null;

const autocomplete = {
  open: false,
  state: null,
  items: [],
  activeIndex: 0,
  start: 0,
  from: 0,
  query: '',
  dismissedKey: '',
  timer: null,
  controller: null,
  sequence: 0,
};

/* -------------------------------------------------------------------------- */
/* TipTap extensions                                                          */
/* -------------------------------------------------------------------------- */

function wikilinkDecorations(doc) {
  const decorations = [];
  const pattern = /\[\[[^\[\]\n]+\]\]/g;

  doc.descendants((node, pos) => {
    if (!node.isText || !node.text) return;
    pattern.lastIndex = 0;
    let match = pattern.exec(node.text);
    while (match) {
      decorations.push(
        Decoration.inline(pos + match.index, pos + match.index + match[0].length, {
          class: 'zcast-wikilink-text',
          title: 'Cmd+Click (or Ctrl+Click) to open',
        }),
      );
      match = pattern.exec(node.text);
    }
  });

  return DecorationSet.create(doc, decorations);
}

const WikilinkDecorations = Extension.create({
  name: 'zcastWikilinkDecorations',
  addProseMirrorPlugins() {
    return [
      new Plugin({
        key: new PluginKey('zcast-wikilink-decorations'),
        state: {
          init: (_config, { doc }) => wikilinkDecorations(doc),
          apply: (tr, old) => (tr.docChanged ? wikilinkDecorations(tr.doc) : old),
        },
        props: {
          decorations(state) {
            return this.getState(state);
          },
        },
      }),
    ];
  },
});

function editorExtensions() {
  return [
    StarterKit.configure({
      heading: { levels: [1, 2, 3, 4, 5, 6] },
      codeBlock: { HTMLAttributes: { class: 'zcast-code-block' } },
    }),
    Link.configure({
      openOnClick: false,
      autolink: true,
      linkOnPaste: true,
      HTMLAttributes: { rel: 'noopener noreferrer' },
    }),
    Underline,
    Placeholder.configure({
      placeholder: 'Start writing… Use [[Title]] to link another note.',
    }),
    WikilinkDecorations,
  ];
}

/* -------------------------------------------------------------------------- */
/* Markdown <-> TipTap                                                        */
/* -------------------------------------------------------------------------- */

function markdownToHtml(source) {
  return markdown.render(source || '');
}

function serializeDoc(doc) {
  if (!doc) return '';
  return serializeBlocks(doc.content || []);
}

function serializeBlocks(nodes) {
  return nodes
    .map((node) => serializeBlock(node))
    .filter((block) => block !== null && block !== undefined && block !== '')
    .join('\n\n')
    .replace(/\n{3,}/g, '\n\n')
    .replace(/^\n+|\n+$/g, '');
}

function serializeBlock(node) {
  if (!node) return '';

  switch (node.type) {
    case 'paragraph':
      return protectLineStarts(serializeInline(node.content || []));
    case 'heading': {
      const level = Math.min(6, Math.max(1, Number(node.attrs?.level) || 1));
      return `${'#'.repeat(level)} ${serializeInline(node.content || [])}`;
    }
    case 'blockquote': {
      const inner = serializeBlocks(node.content || []);
      return inner
        .split('\n')
        .map((line) => (line ? `> ${line}` : '>'))
        .join('\n');
    }
    case 'bulletList':
      return serializeList(node, '');
    case 'orderedList':
      return serializeList(node, '');
    case 'codeBlock':
      return serializeCodeBlock(node);
    case 'horizontalRule':
      return '---';
    case 'hardBreak':
      return '  \n';
    case 'image': {
      const src = markdownUrl(node.attrs?.src || '');
      const alt = escapePlainInline(node.attrs?.alt || '');
      const title = node.attrs?.title ? ` "${String(node.attrs.title).replace(/"/g, '\\"')}"` : '';
      return `![${alt}](${src}${title})`;
    }
    default:
      if (Array.isArray(node.content) && node.content.length > 0) {
        if (node.content.some((child) => child.type === 'text')) {
          return serializeInline(node.content);
        }
        return serializeBlocks(node.content);
      }
      return node.text ? escapeInlineText(node.text) : '';
  }
}

function serializeInline(nodes) {
  return (nodes || []).map(serializeInlineNode).join('');
}

function serializeInlineNode(node) {
  if (!node) return '';
  if (node.type === 'text') return serializeText(node);
  if (node.type === 'hardBreak') return '  \n';
  if (node.type === 'image') return serializeBlock(node);
  if (Array.isArray(node.content)) return serializeInline(node.content);
  return node.text ? escapeInlineText(node.text) : '';
}

function serializeText(node) {
  const text = node.text || '';
  const marks = new Map((node.marks || []).map((mark) => [mark.type, mark]));

  if (marks.has('code')) {
    return codeSpan(text);
  }

  let out = escapeInlineText(text);

  if (marks.has('bold')) out = `**${out}**`;
  if (marks.has('italic')) out = `*${out}*`;
  if (marks.has('strike')) out = `~~${out}~~`;
  if (marks.has('underline')) out = `<u>${out}</u>`;

  const link = marks.get('link');
  if (link?.attrs?.href) {
    out = `[${out}](${markdownUrl(link.attrs.href)})`;
  }

  return out;
}

function serializeList(node, indent) {
  const ordered = node.type === 'orderedList';
  const start = ordered ? Math.max(1, Number(node.attrs?.start) || 1) : 1;

  return (node.content || [])
    .map((item, index) => {
      const marker = ordered ? `${start + index}. ` : '- ';
      return serializeListItem(item, marker, indent);
    })
    .join('\n');
}

function serializeListItem(item, marker, indent) {
  const lines = [];
  let firstBlock = true;

  for (const child of item.content || []) {
    if (child.type === 'bulletList' || child.type === 'orderedList') {
      if (firstBlock) {
        lines.push(indent + marker.trimEnd());
        firstBlock = false;
      }
      lines.push(serializeList(child, `${indent}  `));
      continue;
    }

    const block = serializeBlock(child);
    const blockLines = block.split('\n');

    if (firstBlock) {
      lines.push(indent + marker + (blockLines.shift() ?? ''));
      firstBlock = false;
    } else {
      const firstLine = blockLines.shift();
      if (firstLine !== undefined) {
        lines.push(firstLine ? `${indent}  ${firstLine}` : '');
      }
    }

    for (const line of blockLines) {
      lines.push(line ? `${indent}  ${line}` : '');
    }
  }

  if (lines.length === 0) lines.push(indent + marker.trimEnd());
  return lines.join('\n');
}

function serializeCodeBlock(node) {
  const text = (node.content || []).map((child) => child.text || '').join('');
  const fence = codeFence(text, 3);
  const language = String(node.attrs?.language || '').trim();
  return `${fence}${language}\n${text.replace(/\n$/, '')}\n${fence}`;
}

function codeFence(text, minimum) {
  const runs = text.match(/`+/g) || [];
  const longest = runs.reduce((max, run) => Math.max(max, run.length), 0);
  return '`'.repeat(Math.max(minimum, longest + 1));
}

function codeSpan(text) {
  const fence = codeFence(text, 1);
  const needsPadding = /^\s|\s$|^`|`$/.test(text);
  const pad = needsPadding ? ' ' : '';
  return `${fence}${pad}${text}${pad}${fence}`;
}

function escapeInlineText(text) {
  const pattern = /\[\[[^\[\]\n]+\]\]/g;
  let out = '';
  let last = 0;

  for (const match of text.matchAll(pattern)) {
    out += escapePlainInline(text.slice(last, match.index)) + match[0];
    last = match.index + match[0].length;
  }

  return out + escapePlainInline(text.slice(last));
}

function escapePlainInline(text) {
  return text.replace(/([\\`*_\[\]<>~|])/g, '\\$1');
}

function markdownUrl(url) {
  const value = String(url || '');
  if (/\s|[()<>]/.test(value)) return `<${value.replace(/>/g, '%3E')}>`;
  return value.replace(/\)/g, '\\)');
}

function protectLineStarts(text) {
  return text
    .split('\n')
    .map((line) => {
      if (/^\s*([-*_]\s*){3,}$/.test(line)) {
        return line.replace(/^(\s*)(.)/, '$1\\$2');
      }
      if (/^\s*=+\s*$/.test(line)) {
        return line.replace(/^(\s*)=/, '$1\\=');
      }
      if (/^\s*(#{1,6}\s|>|[-+*]\s|\d+\.\s)/.test(line)) {
        return line.replace(/^(\s*)(#{1,6}|>|[-+*]|\d+\.)(\s)/, '$1\\$2$3');
      }
      return line;
    })
    .join('\n');
}

/* -------------------------------------------------------------------------- */
/* DOM / card helpers                                                         */
/* -------------------------------------------------------------------------- */

function eventElement(event) {
  const target = event.target;
  return target instanceof Element ? target : target?.parentElement || null;
}

function cardFor(element) {
  return element?.closest?.('[data-zcast-card]') || null;
}

function cardId(card) {
  return card?.dataset?.zcastCardId || '';
}

function cardSlug(card) {
  return card?.dataset?.zcastCardSlug || '';
}

function cardParts(card) {
  return {
    titleElement: card.querySelector('[data-zcast-card-title]'),
    bodyElement: card.querySelector('[data-zcast-card-body]'),
    editButton: card.querySelector('[data-zcast-edit-toggle]'),
    privateButton: card.querySelector('[data-zcast-private-toggle]'),
    statusElement: card.querySelector('[data-zcast-save-status]'),
  };
}

function normaliseTitle(value) {
  return String(value ?? '')
    .replace(/\s+/g, ' ')
    .trim();
}

function titleKey(value) {
  return normaliseTitle(value).toLowerCase();
}

function normaliseSlug(value) {
  return String(value ?? '')
    .replace(/^\/+/, '')
    .trim();
}

/** Every unsaved draft pane currently in the DOM, oldest first. */
function openDraftCards() {
  return Array.from(
    document.querySelectorAll('[data-zcast-card][data-zcast-draft="true"]'),
  ).filter((card) => !cardId(card));
}

function findOpenDraftCard() {
  return openDraftCards()[0] || null;
}

function draftTitle(card) {
  const state = editorStates.get(card);
  const element = state?.titleElement || card.querySelector('[data-zcast-card-title]');
  if (element) return normaliseTitle(element.textContent);
  return normaliseTitle(state?.note?.title);
}

/** True when an unsaved draft has neither a title nor any markdown. */
function draftIsBlank(card) {
  const state = editorStates.get(card);
  if (state) return draftIsEmptyState(state);

  const title = normaliseTitle(card.querySelector('[data-zcast-card-title]')?.textContent);
  const body = normaliseTitle(card.querySelector('[data-zcast-card-body]')?.textContent);
  return !title && !body;
}

/** True when an unsaved draft has neither a title nor any markdown. */
function draftIsEmptyState(state) {
  if (!state) return false;
  return (
    !normaliseTitle(state.titleElement?.textContent) &&
    !normaliseTitle(readMarkdown(state))
  );
}

function syncDraftActiveState() {
  const active = Boolean(findOpenDraftCard());
  const root = document.documentElement;

  if (active) root.dataset.zcastDraftActive = 'true';
  else delete root.dataset.zcastDraftActive;

  document.querySelectorAll('[data-zcast-new-note]').forEach((button) => {
    if (active) {
      button.dataset.draftActive = 'true';
      button.setAttribute('title', 'Save or close current draft first');
    } else {
      delete button.dataset.draftActive;
      button.removeAttribute('title');
    }
  });
}

function scrollPaneIntoView(card) {
  if (!card) return;

  const panes = window.ZcastPanes;
  if (panes && typeof panes.scrollToPane === 'function') {
    try {
      panes.scrollToPane(card);
      return;
    } catch {
      // Fall through to the plain DOM fallback below.
    }
  }

  if (typeof card.scrollIntoView === 'function') {
    card.scrollIntoView({ behavior: 'smooth', block: 'nearest', inline: 'end' });
  }
}

function pulseCard(card) {
  if (!card) return;

  card.classList.remove('zcast-card--highlight');
  // Force a reflow so the pulse restarts when the same pane is targeted twice.
  void card.offsetWidth;
  card.classList.add('zcast-card--highlight');
  window.setTimeout(() => card.classList.remove('zcast-card--highlight'), 1400);
}

function focusDraftPane(card, message = '') {
  if (!card) return;

  const state = editorStates.get(card);
  scrollPaneIntoView(card);
  pulseCard(card);

  if (state?.editing) {
    const title = normaliseTitle(state.titleElement?.textContent);
    if (!title) focusContentEditable(state.titleElement, true);
    else if (state.editor) state.editor.commands.focus('end');
  } else {
    const title = draftTitle(card);
    void enterEdit(card, { draft: true, focus: title ? 'body' : 'title' });
  }

  if (message) setStatus(card, message, 'hint');
  syncDraftActiveState();
}

function ensureDiscardButton(card) {
  if (!card || card.querySelector('[data-zcast-discard-draft]')) return;

  const toolbar = card.querySelector('[data-zcast-card-toolbar]');
  if (!toolbar) return;

  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'zcast-btn zcast-btn-discard';
  button.dataset.zcastDiscardDraft = '';
  button.title = 'Discard this unsaved draft';
  button.textContent = 'Discard';
  toolbar.appendChild(button);
}

function removeDiscardButton(card) {
  card?.querySelector?.('[data-zcast-discard-draft]')?.remove();
}

/**
 * Multi-card SSR pages (/?stack=…) reuse a single toolbar slot, so every
 * card can end up showing the first card's ID and Public/Private state.
 * Re-sync each toolbar from its own card attributes on boot.
 */
function hydrateCardToolbar(card) {
  if (!card) return;

  const id = cardId(card);
  const state = editorStates.get(card);
  const editButton = card.querySelector('[data-zcast-edit-toggle]');

  if (editButton) {
    if (id) editButton.dataset.zcastCardId = id;
    else editButton.removeAttribute('data-zcast-card-id');
    if (!state?.editing) applyEditButton(card, false);
  }

  const privateButton = card.querySelector('[data-zcast-private-toggle]');
  if (privateButton) {
    if (id) privateButton.dataset.zcastCardId = id;
    else privateButton.removeAttribute('data-zcast-card-id');
  }

  const privateValue = state
    ? state.private
      ? 'true'
      : 'false'
    : (card.dataset.zcastCardPrivate ?? privateButton?.dataset.private);

  if (privateValue === 'true' || privateValue === 'false') {
    applyPrivateButton(card, privateValue === 'true');
  }
}

function hydrateCardToolbars() {
  document.querySelectorAll('[data-zcast-card]').forEach(hydrateCardToolbar);
}

function escapeHtml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function setStatus(card, text, kind = '') {
  const element = card?.querySelector?.('[data-zcast-save-status]');
  if (!element) return;

  const previous = statusTimers.get(element);
  if (previous) clearTimeout(previous);
  statusTimers.delete(element);

  element.textContent = text || '';
  if (kind) element.dataset.state = kind;
  else delete element.dataset.state;

  if (text && (kind === 'saved' || kind === 'hint')) {
    const timer = setTimeout(() => {
      if (element.textContent === text) {
        element.textContent = '';
        delete element.dataset.state;
      }
    }, kind === 'hint' ? HINT_STATUS_MS : SAVED_STATUS_MS);
    statusTimers.set(element, timer);
  }
}

function applyPrivateButton(card, isPrivate) {
  const button = card.querySelector('[data-zcast-private-toggle]');
  if (!button) return;

  const value = isPrivate ? 'true' : 'false';
  button.dataset.private = value;
  button.setAttribute('aria-pressed', value);
  button.textContent = isPrivate ? 'Private' : 'Public';
  button.title = isPrivate
    ? 'Private note - click to make public'
    : 'Public note - click to make private';
  card.dataset.zcastCardPrivate = value;
  card.dataset.zcastPrivate = value;
}

function applyEditButton(card, isEditing) {
  const button = card.querySelector('[data-zcast-edit-toggle]');
  if (!button) return;

  button.textContent = isEditing ? 'Done' : 'Edit';
  button.setAttribute('aria-pressed', isEditing ? 'true' : 'false');
  button.disabled = false;
}

function focusContentEditable(element, selectAll = false) {
  if (!element) return;
  element.focus();

  const selection = window.getSelection();
  if (!selection) return;

  const range = document.createRange();
  range.selectNodeContents(element);
  range.collapse(!selectAll);
  selection.removeAllRanges();
  selection.addRange(range);
}

function readTitle(state) {
  const raw = state?.titleElement?.textContent || '';
  const title = raw.replace(/\s+/g, ' ').trim();
  return title || 'Untitled';
}

function readMarkdown(state) {
  if (state?.editor) return serializeDoc(state.editor.getJSON());
  return state?.markdown ?? state?.note?.markdown_body ?? '';
}

/** Every `[data-zcast-card]` pane currently in the DOM, left to right. */
function allCardPanes() {
  return Array.from(document.querySelectorAll('[data-zcast-card]'));
}

/* -------------------------------------------------------------------------- */
/* [[wikilink]] targets                                                       */
/* -------------------------------------------------------------------------- */

const WIKILINK_PATTERN = /\[\[[^\[\]\n]+\]\]/g;

/** Split `[[Title]]` / `[[Title|Display]]` into its target and display text. */
function parseWikilinkTarget(raw) {
  let text = String(raw ?? '').trim();
  if (!text) return null;

  if (text.startsWith('[[')) text = text.slice(2);
  if (text.endsWith(']]')) text = text.slice(0, -2);

  const pipe = text.indexOf('|');
  const target = normaliseTitle(pipe >= 0 ? text.slice(0, pipe) : text);
  if (!target) return null;

  const display = normaliseTitle(pipe >= 0 ? text.slice(pipe + 1) : target);
  return { target, display: display || target };
}

/**
 * All closed `[[wikilinks]]` in a ProseMirror document with their absolute
 * positions, skipping anything inside inline code or a fenced code block.
 */
function collectWikilinks(doc) {
  const links = [];
  if (!doc) return links;

  doc.descendants((node, nodePos, parent) => {
    if (!node.isText || !node.text) return;
    if (parent?.type?.name === 'codeBlock') return false;
    if (node.marks?.some((mark) => mark.type.name === 'code')) return;

    WIKILINK_PATTERN.lastIndex = 0;
    let match = WIKILINK_PATTERN.exec(node.text);
    while (match) {
      const parsed = parseWikilinkTarget(match[0]);
      if (parsed) {
        const from = nodePos + match.index;
        links.push({ ...parsed, raw: match[0], from, to: from + match[0].length });
      }
      match = WIKILINK_PATTERN.exec(node.text);
    }
  });

  return links;
}

/** The closed `[[wikilink]]` a cursor/position sits inside (or null). */
function findWikilinkAtPos(doc, pos) {
  if (typeof pos !== 'number' || pos < 0) return null;
  return collectWikilinks(doc).find((link) => pos >= link.from && pos <= link.to) || null;
}

/** The closed `[[wikilink]]` a selection (or collapsed caret) touches, if any. */
function findWikilinkInRange(doc, from, to) {
  const links = collectWikilinks(doc);
  if (from === to) return links.find((link) => from >= link.from && from <= link.to) || null;
  return links.find((link) => link.from < to && link.to > from) || null;
}

/** Mirror of the kernel's `slugify` so unresolved titles can be probed. */
function slugifyTarget(input) {
  let out = '';
  let prevDash = false;

  for (const char of String(input ?? '').trim()) {
    if (/[A-Za-z0-9]/.test(char)) {
      out += char.toLowerCase();
      prevDash = false;
    } else if (char === '-' || char === '_' || char === '/' || /\s/.test(char)) {
      if (!prevDash && out) {
        out += '-';
        prevDash = true;
      }
    }
  }

  out = out.replace(/-+$/, '');
  return out || 'untitled';
}

/* -------------------------------------------------------------------------- */
/* Network                                                                    */
/* -------------------------------------------------------------------------- */

async function requestJson(url, options = {}) {
  const response = await fetch(url, {
    credentials: 'same-origin',
    ...options,
    headers: {
      Accept: 'application/json',
      ...(options.body ? { 'Content-Type': 'application/json' } : {}),
      ...(options.headers || {}),
    },
  });

  const text = await response.text();
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = { message: text };
    }
  }

  if (!response.ok) {
    const message =
      data?.error || data?.message || `Request failed with status ${response.status}`;
    throw new Error(message);
  }

  return data || {};
}

async function fetchNote(id) {
  const data = await requestJson(`${API_NOTES}/${encodeURIComponent(id)}`);
  return data.note || data;
}

/**
 * Resolve a `[[wikilink]]` title to a slug: exact case-insensitive title match
 * or exact slug match from the (admin-only, 25 hit) notes search, then an
 * alias-safe probe of the public card fragment endpoint.
 */
async function resolveWikilinkSlug(title) {
  const wanted = normaliseTitle(title);
  if (!wanted) return '';

  const wantedKey = wanted.toLowerCase();
  const wantedSlug = slugifyTarget(wanted);

  try {
    const params = new URLSearchParams({ q: wanted });
    const data = await requestJson(`${API_NOTES}?${params.toString()}`);
    const notes = Array.isArray(data) ? data : data?.notes || data?.results || [];

    const byTitle = notes.find((note) => titleKey(note?.title) === wantedKey);
    if (byTitle?.slug) return normaliseSlug(byTitle.slug);

    const bySlug = notes.find(
      (note) => normaliseSlug(note?.slug).toLowerCase() === wantedSlug,
    );
    if (bySlug?.slug) return normaliseSlug(bySlug.slug);
  } catch {
    // Search is admin-only: fall through to the fragment probe below.
  }

  try {
    const response = await fetch(`/_zcast/cards/${encodeURIComponent(wantedSlug)}`, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
      cache: 'no-store',
    });

    if (!response.ok) return '';

    const data = await response.json().catch(() => null);
    return normaliseSlug(data?.canonical_slug || data?.card?.slug || wantedSlug);
  } catch {
    return '';
  }
}

/** Open a note slug in a pane to the right (never leaving edit mode). */
function openSlugInPane(slug, anchorCard = null) {
  const value = normaliseSlug(slug);
  if (!value) return false;

  const panes = window.ZcastPanes;

  if (panes && typeof panes.openSlug === 'function') {
    try {
      panes.openSlug(value, anchorCard || null);
      return true;
    } catch {
      // Fall through to the fragment append / navigation fallbacks below.
    }
  }

  // Themes that only expose the append helper still get a real pane instead of
  // a full page load, so the anchor card keeps its edit session.
  if (panes && typeof panes.appendCardHtml === 'function') {
    void appendSlugPane(value, anchorCard, panes).then((card) => {
      if (!card) navigateToSlug(value, anchorCard);
    });
    return true;
  }

  navigateToSlug(value, anchorCard);
  return false;
}

/** Fetch a card fragment and append it as a pane after `anchorCard`. */
async function appendSlugPane(slug, anchorCard, panes) {
  const wanted = slug.toLowerCase();
  const existing = allCardPanes().find(
    (pane) => normaliseSlug(cardSlug(pane)).toLowerCase() === wanted,
  );

  if (existing) {
    scrollPaneIntoView(existing);
    pulseCard(existing);
    return existing;
  }

  try {
    const response = await fetch(`/_zcast/cards/${encodeURIComponent(slug)}`, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
      cache: 'no-store',
    });

    if (!response.ok) throw new Error(`Could not open /${slug}`);

    const data = await response.json();
    if (typeof data?.html !== 'string') throw new Error('Missing card HTML');

    const canonical = normaliseSlug(data.canonical_slug || data.card?.slug || slug);
    const card = panes.appendCardHtml(
      anchorCard,
      data.html,
      canonical,
      data.card?.title || '',
    );

    if (card) {
      hydrateCardToolbar(card);
      pulseCard(card);
    }

    return card || null;
  } catch {
    return null;
  }
}

/** Last-resort fallback: flush any pending edits, then navigate. */
function navigateToSlug(slug, anchorCard = null) {
  const state = anchorCard ? editorStates.get(anchorCard) : null;
  const go = () => window.location.assign(`/${encodeURIComponent(slug)}`);

  if (state?.editing) void flushSave(state).then(go, go);
  else go();
}

/**
 * Follow a `[[wikilink]]` while staying in edit mode: resolved notes slide open
 * in a pane to the right, unresolved titles open a pre-titled draft pane.
 */
async function navigateWikilinkTarget(targetTitle, anchorCard = null) {
  const clean = normaliseTitle(targetTitle);
  if (!clean) return null;

  const slug = await resolveWikilinkSlug(clean);
  if (!slug) return openDraftForTitle(clean, anchorCard);

  openSlugInPane(slug, anchorCard);
  return slug;
}

/* -------------------------------------------------------------------------- */
/* Editor lifecycle                                                           */
/* -------------------------------------------------------------------------- */

async function toggleEdit(card) {
  const state = editorStates.get(card);
  if (state?.editing) {
    await exitEdit(state);
  } else {
    await enterEdit(card);
  }
}

async function enterEdit(card, options = {}) {
  const existing = editorStates.get(card);
  if (existing?.editing) return;

  const { titleElement, bodyElement, editButton } = cardParts(card);
  if (!titleElement || !bodyElement) return;

  const draft =
    Boolean(options.draft) ||
    Boolean(card.dataset.zcastDraft === 'true') ||
    !cardId(card);

  let note = options.note || null;
  const originalHtml = bodyElement.innerHTML;

  if (!note) {
    if (draft) {
      note = existing?.note
        ? { ...existing.note }
        : {
            id: '',
            slug: '',
            title: options.title ?? titleElement.textContent.trim(),
            markdown_body: options.markdown ?? '',
            private: false,
          };
      if (options.title !== undefined) note.title = options.title;
    } else {
      if (editButton) editButton.disabled = true;
      setStatus(card, 'Loading...', 'saving');
      try {
        note = await fetchNote(cardId(card));
      } catch (error) {
        setStatus(card, errorMessage(error), 'error');
        if (editButton) editButton.disabled = false;
        return;
      }
      setStatus(card, '');
    }
  }

  const markdown = draft
    ? existing?.markdown ?? note.markdown_body ?? ''
    : note.markdown_body ?? '';

  // A brand-new "+ New Note" pane starts clean: exiting it without typing
  // anything must not create an empty "Untitled" note (issue #4).
  const blankDraft =
    draft && !note.id && !normaliseTitle(note.title) && !normaliseTitle(markdown);

  const state = {
    card,
    note,
    draft,
    editing: true,
    editor: null,
    titleElement,
    bodyElement,
    editButton,
    statusElement: card.querySelector('[data-zcast-save-status]'),
    private: Boolean(note.private),
    markdown,
    renderedHtml: existing?.renderedHtml ?? originalHtml,
    originalHtml,
    dirty: blankDraft ? false : Boolean(options.draft || draft),
    saving: false,
    savePromise: null,
    saveTimer: null,
  };

  editorStates.set(card, state);
  liveStates.add(state);

  if (draft) ensureDiscardButton(card);

  mountEditor(state, options.focus || 'body', {
    deferFocus: Boolean(options.deferFocus),
  });
}

function mountEditor(state, focusMode, options = {}) {
  const { card, bodyElement, titleElement, note } = state;

  titleElement.textContent = note.title || '';
  titleElement.setAttribute('contenteditable', 'true');
  titleElement.setAttribute('spellcheck', 'true');
  titleElement.dataset.zcastPlaceholder = 'Untitled';
  titleElement.classList.add('zcast-card-title-editing');

  bodyElement.innerHTML = '';
  bodyElement.classList.add('zcast-editing');
  bodyElement.setAttribute('aria-busy', 'false');

  const surface = document.createElement('div');
  surface.className = 'zcast-editor-surface';
  bodyElement.appendChild(surface);

  applyPrivateButton(card, state.private);
  applyEditButton(card, true);
  setStatus(card, '');

  const editor = new Editor({
    element: surface,
    extensions: editorExtensions(),
    content: markdownToHtml(state.markdown),
    autofocus: false,
    editorProps: {
      attributes: {
        class: 'zcast-editor-prose',
        spellcheck: 'true',
        autocapitalize: 'off',
      },
      handleKeyDown: (_view, event) => handleEditorKeyDown(state, event),
      handleClick: (view, pos, event) =>
        handleEditorWikilinkClick(state, view, pos, event),
    },
    onUpdate: ({ editor: current }) => {
      if (!state.editing) return;
      state.markdown = serializeDoc(current.getJSON());
      state.dirty = true;
      scheduleSave(state);
      refreshAutocomplete(state);
    },
    onSelectionUpdate: () => {
      if (!state.editing) return;
      updateFloatingToolbar(state);
      refreshAutocomplete(state);
    },
    onBlur: () => {
      window.setTimeout(() => {
        if (toolbarOwner === state && !autocomplete.open) hideFloatingToolbar();
      }, 140);
    },
  });

  state.editor = editor;

  const applyFocus = () => {
    if (!state.editing) return;
    if (focusMode === 'title') {
      focusContentEditable(titleElement, true);
    } else {
      editor.commands.focus('end');
    }
  };

  if (options.deferFocus) {
    // Let the horizontal slide-in paint before focus-scroll can jump it.
    window.requestAnimationFrame(() => window.requestAnimationFrame(applyFocus));
  } else {
    applyFocus();
  }
}

async function exitEdit(state, options = {}) {
  if (!state?.editing) return;
  if (state.editButton) state.editButton.disabled = true;

  hideFloatingToolbar();
  hideAutocomplete(state);

  const unsavedDraft = Boolean(state.draft) && !state.note?.id;
  const blankDraft = unsavedDraft && draftIsEmptyState(state);
  const discard = Boolean(options.discard) || blankDraft;

  if (discard && state.saving && state.savePromise) await state.savePromise;

  // An untouched draft never hits POST /_zcast/api/notes (issue #4).
  if (!discard && !options.skipSave) await flushSave(state);

  state.editing = false;
  liveStates.delete(state);

  if (state.saveTimer) {
    clearTimeout(state.saveTimer);
    state.saveTimer = null;
  }

  if (state.editor) {
    state.editor.destroy();
    state.editor = null;
  }

  const { card, bodyElement, titleElement } = state;

  if (discard) {
    editorStates.delete(card);
    closeCardPane(card);
    return;
  }

  bodyElement.classList.remove('zcast-editing');
  bodyElement.innerHTML = state.renderedHtml ?? state.originalHtml ?? '';

  titleElement.textContent = state.note?.title || titleElement.textContent;
  titleElement.setAttribute('contenteditable', 'false');
  titleElement.classList.remove('zcast-card-title-editing');
  delete titleElement.dataset.zcastPlaceholder;

  applyEditButton(card, false);
  applyPrivateButton(card, state.private);
  card.removeAttribute('data-zcast-busy');
  if (state.draft) card.dataset.zcastDraft = 'true';
  syncDraftActiveState();
}

function scheduleSave(state, delay = SAVE_DELAY_MS) {
  if (!state?.editing) return;
  if (state.saveTimer) clearTimeout(state.saveTimer);
  state.saveTimer = window.setTimeout(() => {
    state.saveTimer = null;
    void saveState(state);
  }, delay);
}

function saveState(state, options = {}) {
  if (!state) return Promise.resolve(false);

  if (state.saving) {
    return state.savePromise || Promise.resolve(false);
  }

  // Never let autosave/Cmd+S create an empty "Untitled" note (issue #4).
  if (state.draft && !state.note?.id && draftIsEmptyState(state)) {
    state.dirty = false;
    return Promise.resolve(true);
  }

  if (!state.dirty && !options.force) return Promise.resolve(true);

  if (state.saveTimer) {
    clearTimeout(state.saveTimer);
    state.saveTimer = null;
  }

  state.dirty = false;
  state.saving = true;
  setStatus(state.card, 'Saving...', 'saving');

  state.savePromise = performSave(state)
    .then((saved) => {
      state.saving = false;
      state.savePromise = null;

      if (!saved) {
        state.dirty = true;
        return false;
      }

      setStatus(state.card, 'Saved', 'saved');
      if (state.dirty) scheduleSave(state, 250);
      return true;
    })
    .catch((error) => {
      state.saving = false;
      state.savePromise = null;
      state.dirty = true;
      setStatus(state.card, `Save failed: ${errorMessage(error)}`, 'error');
      return false;
    });

  return state.savePromise;
}

function mutateHtmlString(html, mutate) {
  if (!html) return html;

  const template = document.createElement('template');
  template.innerHTML = html;
  return mutate(template.content) ? template.innerHTML : html;
}

/** Apply an in-place link mutation to every open pane and its stored HTML. */
function syncPaneLinks(mutate) {
  document.querySelectorAll('[data-zcast-card]').forEach((card) => {
    mutate(card);

    const state = editorStates.get(card);
    if (!state) return;

    state.renderedHtml = mutateHtmlString(state.renderedHtml, mutate);
    state.originalHtml = mutateHtmlString(state.originalHtml, mutate);
  });
}

/** Upgrade admin `[[unresolved]]` anchors once the missing note is saved. */
function upgradeUnresolvedLinks(title, slug) {
  const wanted = normaliseSlug(slug);
  const wantedTitle = titleKey(title);
  if (!wanted || !wantedTitle) return;

  syncPaneLinks((root) => {
    let changed = false;

    root.querySelectorAll('a[data-zcast-unresolved]').forEach((anchor) => {
      const current = anchor.dataset.zcastUnresolved || anchor.textContent;
      if (titleKey(current) !== wantedTitle) return;

      anchor.dataset.zcastNote = wanted;
      anchor.setAttribute('href', `/${wanted}`);
      anchor.removeAttribute('data-zcast-unresolved');
      anchor.classList.remove('zcast-unresolved');
      anchor.classList.remove('zcast-unresolved-public');
      if (!anchor.classList.length) anchor.removeAttribute('class');
      changed = true;
    });

    return changed;
  });
}

/** Keep renamed `[[links]]` pointing at the new slug across open panes. */
function rewriteNoteLinks(previousSlug, nextSlug) {
  const from = normaliseSlug(previousSlug);
  const to = normaliseSlug(nextSlug);
  if (!from || !to || from === to) return;

  syncPaneLinks((root) => {
    let changed = false;

    root.querySelectorAll('a[data-zcast-note]').forEach((anchor) => {
      if (normaliseSlug(anchor.dataset.zcastNote) !== from) return;
      anchor.dataset.zcastNote = to;
      anchor.setAttribute('href', `/${to}`);
      changed = true;
    });

    return changed;
  });
}

async function performSave(state) {
  const previousSlug = state.note?.slug || cardSlug(state.card);
  const wasDraft = state.draft || !state.note?.id;

  const payload = {
    title: readTitle(state),
    markdown_body: readMarkdown(state),
    private: Boolean(state.private),
  };

  const endpoint = wasDraft
    ? API_NOTES
    : `${API_NOTES}/${encodeURIComponent(state.note.id)}`;
  const method = wasDraft ? 'POST' : 'PUT';

  const data = await requestJson(endpoint, {
    method,
    body: JSON.stringify(payload),
  });

  const note = data.note || data;
  if (!note || typeof note !== 'object') {
    throw new Error('The server did not return the saved note');
  }

  if (wasDraft) {
    if (!note.id) throw new Error('The server did not return a note id');
    state.draft = false;
    state.card.dataset.zcastDraft = 'false';
    state.card.dataset.zcastCardId = note.id;
    state.card.classList.remove('zcast-card-draft');
    removeDiscardButton(state.card);
  }

  state.note = { ...(state.note || {}), ...note };
  if (typeof state.note.private === 'boolean') state.private = state.note.private;
  state.markdown = state.note.markdown_body ?? payload.markdown_body;

  if (typeof data.rendered_html === 'string') {
    state.renderedHtml = data.rendered_html;
  }

  const nextSlug = state.note.slug || previousSlug;
  const renamed =
    Boolean(data.renamed_from_slug) ||
    Boolean(previousSlug && nextSlug && previousSlug !== nextSlug);

  if (nextSlug) state.card.dataset.zcastCardSlug = nextSlug;
  if (state.note.id) state.card.dataset.zcastCardId = state.note.id;

  if (wasDraft && nextSlug) {
    upgradeUnresolvedLinks(state.note.title || payload.title, nextSlug);
  }

  if (renamed && previousSlug && nextSlug && previousSlug !== nextSlug) {
    rewriteNoteLinks(previousSlug, nextSlug);
  }

  if (renamed || wasDraft) {
    const panes = window.ZcastPanes;
    if (panes && typeof panes.updateCardMeta === 'function') {
      const metaTitle = renamed ? state.note.title || payload.title : '';
      panes.updateCardMeta(state.card, nextSlug, metaTitle);
    }
  }

  applyPrivateButton(state.card, state.private);
  syncDraftActiveState();

  // Hot-swap this card's own footer so a brand-new note immediately shows its
  // own "Linked from" backlinks, then refresh every other open pane.
  if (typeof data.card_html === 'string' && data.card_html) {
    applyFreshCardParts(state.card, data.card_html, {
      rendered_html: typeof data.rendered_html === 'string' ? data.rendered_html : undefined,
    });
  }

  void refreshOpenPanes(state.card);
  return true;
}

/* -------------------------------------------------------------------------- */
/* Real-time cross-pane refresh                                               */
/* -------------------------------------------------------------------------- */

/**
 * Swap fresh SSR fragments into an open pane. The footer ("Linked from") is
 * always refreshed; the body only when the pane is not mid-edit, and an active
 * editor only has its cached HTML updated so typing is never clobbered.
 */
function applyFreshCardParts(card, cardHtml, cardData = null, options = {}) {
  if (!card || typeof cardHtml !== 'string' || !cardHtml) return;

  const replaceBody = Boolean(options.replaceBody);

  const template = document.createElement('template');
  template.innerHTML = cardHtml;

  const freshFooter = template.content.querySelector('[data-zcast-card-footer]');
  const freshBody = template.content.querySelector('[data-zcast-card-body]');
  const freshHtml = freshBody ? freshBody.innerHTML : null;
  const cachedHtml =
    typeof cardData?.rendered_html === 'string' ? cardData.rendered_html : freshHtml;

  const footer = card.querySelector('[data-zcast-card-footer]');
  if (footer && freshFooter) footer.innerHTML = freshFooter.innerHTML;

  const state = editorStates.get(card);

  if (state?.editing && !replaceBody) {
    // Never touch the live TipTap DOM while the user is typing: just keep the
    // cached HTML fresh so exiting edit mode renders the new content.
    if (typeof cachedHtml === 'string') {
      state.renderedHtml = cachedHtml;
      state.originalHtml = cachedHtml;
    }
    return;
  }

  if (!replaceBody) return;

  const body = card.querySelector('[data-zcast-card-body]');
  if (body && typeof freshHtml === 'string') body.innerHTML = freshHtml;

  if (state && !state.editing && typeof cachedHtml === 'string') {
    state.renderedHtml = cachedHtml;
    state.originalHtml = cachedHtml;
  }
}

/**
 * After a save, re-fetch every other open pane (in parallel) so renamed
 * titles, freshly resolved `[[links]]` and "Linked from" backlinks update
 * without a page refresh.
 */
async function refreshOpenPanes(excludeCard = null) {
  const panes = allCardPanes().filter((pane) => {
    if (pane === excludeCard) return false;
    // A pane with no id is an unsaved draft: it has nothing to refresh.
    if (pane.dataset.zcastDraft === 'true' && !cardId(pane)) return false;
    return Boolean(cardSlug(pane));
  });

  if (panes.length === 0) return;

  const results = await Promise.allSettled(
    panes.map(async (pane) => {
      const slug = cardSlug(pane);
      const response = await fetch(`/_zcast/cards/${encodeURIComponent(slug)}`, {
        method: 'GET',
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
        cache: 'no-store',
      });

      if (!response.ok) throw new Error(`Could not refresh /${slug}`);

      const data = await response.json();
      return { pane, slug, data };
    }),
  );

  let slugChanged = false;

  for (const result of results) {
    if (result.status !== 'fulfilled') continue;

    const { pane, slug, data } = result.value;
    if (!pane.isConnected || !data || typeof data.html !== 'string') continue;

    const canonical = normaliseSlug(data.canonical_slug || data.card?.slug || slug);
    if (canonical && canonical !== slug) {
      pane.dataset.zcastCardSlug = canonical;
      slugChanged = true;
    }

    applyFreshCardParts(pane, data.html, data.card, {
      replaceBody: !editorStates.get(pane)?.editing,
    });
  }

  if (slugChanged && typeof window.ZcastPanes?.rebuildBreadcrumbs === 'function') {
    window.ZcastPanes.rebuildBreadcrumbs();
  }
}

/**
 * Delete a note (future `[data-zcast-delete-note]` hook) and re-sync the panes
 * that were linking to it.
 */
async function deleteNote(card) {
  if (!card) return false;

  const state = editorStates.get(card);
  const id = state?.note?.id || cardId(card);

  if (!id) {
    // Never saved, so there is nothing to DELETE: just drop the draft pane.
    return discardDraft(card);
  }

  try {
    const response = await fetch(`${API_NOTES}/${encodeURIComponent(id)}`, {
      method: 'DELETE',
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
    });

    if (!response.ok && response.status !== 404) {
      throw new Error(`Delete failed with status ${response.status}`);
    }
  } catch (error) {
    setStatus(card, `Delete failed: ${errorMessage(error)}`, 'error');
    return false;
  }

  await refreshOpenPanes(card);

  if (state?.editing) {
    state.dirty = false;
    await exitEdit(state, { discard: true });
  } else {
    closeCardPane(card);
  }

  return true;
}

async function flushSave(state) {
  if (!state) return false;
  if (state.saveTimer) {
    clearTimeout(state.saveTimer);
    state.saveTimer = null;
  }

  for (let attempt = 0; attempt < 4; attempt += 1) {
    if (state.saving) {
      await state.savePromise;
      continue;
    }
    if (!state.dirty) return true;
    const saved = await saveState(state, { force: true });
    if (!saved) return false;
  }

  return !state.dirty;
}

/* -------------------------------------------------------------------------- */
/* Public/private toggle                                                      */
/* -------------------------------------------------------------------------- */

async function togglePrivate(card) {
  let state = editorStates.get(card);

  if (!state?.note) {
    const button = card.querySelector('[data-zcast-private-toggle]');
    const current = button?.dataset.private === 'true';
    const titleElement = card.querySelector('[data-zcast-card-title]');
    const bodyElement = card.querySelector('[data-zcast-card-body]');

    if (cardId(card)) {
      setStatus(card, 'Loading...', 'saving');
      try {
        const note = await fetchNote(cardId(card));
        state = {
          card,
          note,
          draft: false,
          editing: false,
          editor: null,
          titleElement,
          bodyElement,
          editButton: card.querySelector('[data-zcast-edit-toggle]'),
          private: Boolean(note.private),
          markdown: note.markdown_body || '',
          renderedHtml: bodyElement?.innerHTML || '',
          dirty: false,
          saving: false,
          savePromise: null,
          saveTimer: null,
        };
        editorStates.set(card, state);
      } catch (error) {
        setStatus(card, errorMessage(error), 'error');
        return;
      }
    } else {
      state = {
        card,
        note: { id: '', slug: '', title: titleElement?.textContent || 'Untitled', markdown_body: '' },
        draft: true,
        editing: false,
        editor: null,
        titleElement,
        bodyElement,
        editButton: card.querySelector('[data-zcast-edit-toggle]'),
        private: current,
        markdown: '',
        renderedHtml: bodyElement?.innerHTML || '',
        dirty: false,
        saving: false,
        savePromise: null,
        saveTimer: null,
      };
      editorStates.set(card, state);
    }
  }

  const next = !state.private;
  state.private = next;
  applyPrivateButton(card, next);

  if (state.draft || !state.note?.id) {
    state.dirty = true;
    await saveState(state, { force: true });
    return;
  }

  state.dirty = true;
  await saveState(state, { force: true });
}

/* -------------------------------------------------------------------------- */
/* Save status / keyboard shortcuts                                           */
/* -------------------------------------------------------------------------- */

function errorMessage(error) {
  return error?.message ? String(error.message) : String(error);
}

function activeStateForEvent(event) {
  const element = eventElement(event);
  const card = cardFor(element);
  if (card && editorStates.get(card)?.editing) return editorStates.get(card);

  for (const state of liveStates) return state;
  return null;
}

function handleEditorKeyDown(state, event) {
  if (handleAutocompleteKey(event)) return true;

  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') {
    event.preventDefault();
    void flushSave(state);
    return true;
  }

  return false;
}

/** `Cmd+Click` / `Ctrl+Click` on a `[[wikilink]]` follows it in-pane. */
function handleEditorWikilinkClick(state, view, pos, event) {
  if (!(event.metaKey || event.ctrlKey)) return false;

  const element = eventElement(event);
  const decoration = element?.closest?.('.zcast-wikilink-text') || null;

  let link = findWikilinkAtPos(view.state.doc, pos);

  if (!link && decoration) {
    // ProseMirror occasionally reports a position just outside the decoration.
    try {
      const from = view.posAtDOM(decoration, 0);
      link =
        findWikilinkAtPos(view.state.doc, from) ||
        findWikilinkAtPos(view.state.doc, from + 1);
    } catch {
      link = null;
    }
  }

  if (!link) return false;

  event.preventDefault();
  event.stopPropagation();
  void navigateWikilinkTarget(link.target, state.card);
  return true;
}

/* -------------------------------------------------------------------------- */
/* Closing panes                                                              */
/* -------------------------------------------------------------------------- */

/** Scroll back to the new rightmost pane, then re-sync breadcrumbs/`?stack=`. */
function syncPanesAfterClose() {
  const panes = window.ZcastPanes;
  const cards = allCardPanes();
  const last = cards[cards.length - 1] || null;

  if (last) scrollPaneIntoView(last);

  if (last && panes && typeof panes.updateCardMeta === 'function') {
    try {
      // Empty slug/title still re-syncs breadcrumbs, <title> and the URL stack.
      panes.updateCardMeta(last, '', '');
      return;
    } catch {
      // Fall through to breadcrumbs only.
    }
  }

  if (typeof panes?.rebuildBreadcrumbs === 'function') panes.rebuildBreadcrumbs();
}

/** Remove one pane, preferring the theme helper and falling back to the DOM. */
function closeCardPane(card) {
  if (!card) return false;

  const panes = window.ZcastPanes;
  if (panes && typeof panes.closePane === 'function') {
    try {
      panes.closePane(card);
      editorStates.delete(card);
      syncDraftActiveState();
      return true;
    } catch {
      // Fall through to the DOM fallback below.
    }
  }

  card.remove();
  editorStates.delete(card);
  syncDraftActiveState();
  syncPanesAfterClose();
  return true;
}

/**
 * Close a pane from its × chrome. Editor panes flush their pending autosave
 * first, so a stray click can never lose typing; blank drafts are dropped.
 */
async function closeEditorPane(card) {
  if (!card) return false;

  // The root/leftmost card is the reading anchor: never close it.
  if (allCardPanes().indexOf(card) <= 0) return false;

  const state = editorStates.get(card);

  if (state?.editing) {
    const unsavedDraft = Boolean(state.draft) && !state.note?.id;

    if (unsavedDraft && draftIsEmptyState(state)) {
      await exitEdit(state, { discard: true });
      return true;
    }

    const saved = await flushSave(state);
    if (!saved) {
      setStatus(card, 'Save failed - keeping pane open', 'error');
      return false;
    }

    state.dirty = false;
    await exitEdit(state, { skipSave: true });
    closeCardPane(card);
    return true;
  }

  if (card.dataset.zcastDraft === 'true' && !cardId(card)) {
    return discardDraft(card);
  }

  return false;
}

/* -------------------------------------------------------------------------- */
/* Floating selection toolbar                                                 */
/* -------------------------------------------------------------------------- */

const TOOLBAR_ACTIONS = [
  ['bold', 'B', 'Bold', 'zcast-toolbar-bold'],
  ['italic', 'I', 'Italic', 'zcast-toolbar-italic'],
  ['underline', 'U', 'Underline', 'zcast-toolbar-underline'],
  ['sep'],
  ['heading2', 'H2', 'Heading 2', 'zcast-toolbar-heading'],
  ['heading3', 'H3', 'Heading 3', 'zcast-toolbar-heading'],
  ['sep'],
  ['bulletList', '• List', 'Bullet list', 'zcast-toolbar-list'],
  ['orderedList', '1. List', 'Ordered list', 'zcast-toolbar-list'],
  ['blockquote', 'Quote', 'Blockquote', 'zcast-toolbar-quote'],
  ['code', 'Code', 'Inline code', 'zcast-toolbar-code'],
  ['sep'],
  ['wikilink', '[[Link]]', 'Wrap selection in a wikilink', 'zcast-toolbar-wikilink'],
  ['sep', '', '', 'zcast-toolbar-sep-open'],
  ['openWikilink', 'Open ↗', 'Open the linked note in a pane to the right', 'zcast-toolbar-open-link'],
];

function ensureFloatingToolbar() {
  if (floatingToolbar) return floatingToolbar;

  const toolbar = document.createElement('div');
  toolbar.className = 'zcast-editor-toolbar';
  toolbar.hidden = true;
  toolbar.dataset.mode = 'selection';
  toolbar.setAttribute('role', 'toolbar');
  toolbar.setAttribute('aria-label', 'Text formatting');
  toolbar.innerHTML = TOOLBAR_ACTIONS.map(([action, label, title, className]) => {
    if (action === 'sep') {
      const extra = className ? ` ${className}` : '';
      return `<span class="zcast-toolbar-sep${extra}" aria-hidden="true"></span>`;
    }
    return `<button type="button" class="zcast-toolbar-btn ${className}" data-zcast-action="${action}" title="${escapeHtml(title)}" aria-label="${escapeHtml(title)}">${escapeHtml(label)}</button>`;
  }).join('');

  toolbar.addEventListener('mousedown', (event) => event.preventDefault());
  toolbar.addEventListener('click', (event) => {
    const button = eventElement(event)?.closest('[data-zcast-action]');
    if (!button || !toolbarOwner?.editor) return;
    runToolbarAction(toolbarOwner, button.dataset.zcastAction || '');
  });

  document.body.appendChild(toolbar);
  floatingToolbar = toolbar;
  return toolbar;
}

function runToolbarAction(state, action) {
  const editor = state.editor;
  if (!editor) return;

  // Following a link must not refocus the editor first: that would collapse a
  // multi-character selection before we know which target we are opening.
  if (action === 'openWikilink') {
    const link = toolbarWikilink(state);
    if (link) void navigateWikilinkTarget(link.target, state.card);
    return;
  }

  const chain = editor.chain().focus();

  switch (action) {
    case 'bold':
      chain.toggleBold().run();
      break;
    case 'italic':
      chain.toggleItalic().run();
      break;
    case 'underline':
      chain.toggleUnderline().run();
      break;
    case 'heading2':
      chain.toggleHeading({ level: 2 }).run();
      break;
    case 'heading3':
      chain.toggleHeading({ level: 3 }).run();
      break;
    case 'bulletList':
      chain.toggleBulletList().run();
      break;
    case 'orderedList':
      chain.toggleOrderedList().run();
      break;
    case 'blockquote':
      chain.toggleBlockquote().run();
      break;
    case 'code':
      chain.toggleCode().run();
      break;
    case 'wikilink':
      wrapSelectionInWikilink(editor);
      break;
    default:
      break;
  }

  window.requestAnimationFrame(() => updateFloatingToolbar(state));
}

function wrapSelectionInWikilink(editor) {
  const { from, to } = editor.state.selection;
  if (from === to) {
    editor.chain().focus().insertContent('[[').run();
    const state = [...liveStates].find((candidate) => candidate.editor === editor);
    if (state) refreshAutocomplete(state);
    return;
  }

  const selected = editor.state.doc.textBetween(from, to, ' ');
  editor
    .chain()
    .focus()
    .insertContentAt({ from, to }, { type: 'text', text: `[[${selected}]]` })
    .run();
}

function updateFloatingToolbar(state) {
  if (!state?.editing || !state.editor) {
    hideFloatingToolbar();
    return;
  }

  // The [[wikilink]] autocomplete owns the caret while it is open.
  if (autocomplete.open && autocomplete.state === state) {
    hideFloatingToolbar();
    return;
  }

  const editor = state.editor;
  const selection = window.getSelection();
  if (!selection || selection.rangeCount === 0) {
    hideFloatingToolbar();
    return;
  }

  const range = selection.getRangeAt(0);
  const editorDom = editor.view.dom;
  if (!editorDom.contains(range.commonAncestorContainer)) {
    hideFloatingToolbar();
    return;
  }

  const collapsed = editor.state.selection.empty;
  const link = toolbarWikilink(state);

  // A bare caret only earns a toolbar while it sits inside a [[wikilink]].
  if (collapsed && !link) {
    hideFloatingToolbar();
    return;
  }

  const rect = collapsed ? wikilinkRect(editor, link) : range.getBoundingClientRect();
  if (!rect || (rect.width === 0 && rect.height === 0)) {
    hideFloatingToolbar();
    return;
  }

  const toolbar = ensureFloatingToolbar();
  toolbarOwner = state;
  toolbar.hidden = false;
  syncToolbarVisibility({ link, collapsed });
  syncToolbarState(state);
  positionToolbar(toolbar, rect);
}

/** The `[[wikilink]]` the current editor selection covers or sits inside. */
function toolbarWikilink(state) {
  const editor = state?.editor;
  if (!editor) return null;

  const { from, to } = editor.state.selection;
  return findWikilinkInRange(editor.state.doc, from, to);
}

/** Collapsed-caret mode: only `Open ↗`, nothing else. */
function syncToolbarVisibility({ link = null, collapsed = false } = {}) {
  if (!floatingToolbar) return;

  floatingToolbar.dataset.mode = collapsed ? 'wikilink-only' : 'selection';

  if (link) floatingToolbar.dataset.zcastWikilinkTarget = link.target;
  else delete floatingToolbar.dataset.zcastWikilinkTarget;

  const showOpen = Boolean(link);

  floatingToolbar.querySelectorAll('[data-zcast-action]').forEach((button) => {
    if (button.dataset.zcastAction === 'openWikilink') {
      button.hidden = !showOpen;
      return;
    }
    button.hidden = collapsed;
  });

  floatingToolbar.querySelectorAll('.zcast-toolbar-sep').forEach((separator) => {
    const openSeparator = separator.classList.contains('zcast-toolbar-sep-open');
    separator.hidden = collapsed || (openSeparator && !showOpen);
  });
}

/** Bounding rect (viewport coordinates) of a wikilink's text range. */
function wikilinkRect(editor, link) {
  if (!link) return null;

  try {
    const start = editor.view.coordsAtPos(link.from);
    const end = editor.view.coordsAtPos(link.to);
    const top = Math.min(start.top, end.top);
    const bottom = Math.max(start.bottom, end.bottom);
    const left = Math.min(start.left, end.left);
    const right = Math.max(start.right, end.right);

    return {
      top,
      bottom,
      left,
      right,
      width: Math.max(0, right - left),
      height: Math.max(0, bottom - top),
    };
  } catch {
    return null;
  }
}

function positionToolbar(toolbar, rect) {
  const gap = 8;
  const margin = 8;
  const width = toolbar.offsetWidth || 320;
  const height = toolbar.offsetHeight || 38;

  let top = rect.top - height - gap;
  if (top < margin) top = rect.bottom + gap;

  let left = rect.left + rect.width / 2 - width / 2;
  left = Math.max(margin, Math.min(window.innerWidth - width - margin, left));

  toolbar.style.top = `${Math.round(top)}px`;
  toolbar.style.left = `${Math.round(left)}px`;
}

function syncToolbarState(state) {
  if (!floatingToolbar || !state?.editor) return;
  const editor = state.editor;

  const active = {
    bold: editor.isActive('bold'),
    italic: editor.isActive('italic'),
    underline: editor.isActive('underline'),
    heading2: editor.isActive('heading', { level: 2 }),
    heading3: editor.isActive('heading', { level: 3 }),
    bulletList: editor.isActive('bulletList'),
    orderedList: editor.isActive('orderedList'),
    blockquote: editor.isActive('blockquote'),
    code: editor.isActive('code'),
  };

  floatingToolbar.querySelectorAll('[data-zcast-action]').forEach((button) => {
    const action = button.dataset.zcastAction;
    if (active[action]) button.dataset.active = 'true';
    else delete button.dataset.active;
  });
}

function hideFloatingToolbar() {
  if (floatingToolbar) floatingToolbar.hidden = true;
  toolbarOwner = null;
}

function repositionFloatingToolbar() {
  if (!toolbarOwner || !floatingToolbar || floatingToolbar.hidden) return;
  // Recompute rather than just reposition: scroll can move a collapsed caret
  // out of its [[wikilink]], and the compact pill must follow it cleanly.
  updateFloatingToolbar(toolbarOwner);
}

/* -------------------------------------------------------------------------- */
/* [[wikilink]] autocomplete                                                  */
/* -------------------------------------------------------------------------- */

function ensureAutocompleteMenu() {
  if (autocompleteMenu) return autocompleteMenu;

  const menu = document.createElement('div');
  menu.className = 'zcast-wikilink-menu';
  menu.hidden = true;
  menu.setAttribute('role', 'listbox');
  menu.setAttribute('aria-label', 'Link a note');

  menu.addEventListener('mousedown', (event) => event.preventDefault());
  menu.addEventListener('click', (event) => {
    const option = eventElement(event)?.closest('[data-zcast-wikilink-index]');
    if (!option) return;
    selectWikilink(Number(option.dataset.zcastWikilinkIndex));
  });

  document.body.appendChild(menu);
  autocompleteMenu = menu;
  return menu;
}

function refreshAutocomplete(state) {
  if (!state?.editing || !state.editor) {
    hideAutocomplete(state);
    return;
  }

  if (state.editor.isActive('code') || state.editor.isActive('codeBlock')) {
    hideAutocomplete(state);
    return;
  }

  // Inside a closed [[wikilink]] the autocomplete must not fight the "Open ↗"
  // affordance: only offer suggestions for a fresh, still-unclosed link.
  if (findWikilinkAtPos(state.editor.state.doc, state.editor.state.selection.from)) {
    hideAutocomplete(state);
    return;
  }

  const { from } = state.editor.state.selection;
  const doc = state.editor.state.doc;
  const lookBehind = Math.max(0, from - 140);

  let before;
  try {
    before = doc.textBetween(lookBehind, from, '\n', '\n');
  } catch {
    hideAutocomplete(state);
    return;
  }

  const openIndex = before.lastIndexOf('[[');
  if (openIndex < 0) {
    hideAutocomplete(state);
    return;
  }

  const query = before.slice(openIndex + 2);
  if (/[\[\]\n]/.test(query)) {
    hideAutocomplete(state);
    return;
  }

  const start = from - (before.length - openIndex);
  const key = `${start}:${query}`;

  if (autocomplete.dismissedKey === key) {
    hideAutocomplete(state);
    return;
  }

  if (
    autocomplete.open &&
    autocomplete.state === state &&
    autocomplete.start === start &&
    autocomplete.query === query
  ) {
    positionAutocomplete();
    return;
  }

  autocomplete.open = true;
  autocomplete.state = state;
  autocomplete.start = start;
  autocomplete.from = from;
  autocomplete.query = query;
  autocomplete.activeIndex = 0;
  autocomplete.dismissedKey = '';

  const menu = ensureAutocompleteMenu();
  menu.hidden = false;
  menu.innerHTML = '<div class="zcast-wikilink-empty">Searching...</div>';
  positionAutocomplete();

  if (autocomplete.timer) clearTimeout(autocomplete.timer);
  autocomplete.timer = window.setTimeout(() => {
    autocomplete.timer = null;
    void fetchWikilinkSuggestions(state, query, start);
  }, query ? AUTOCOMPLETE_DELAY_MS : 0);
}

async function fetchWikilinkSuggestions(state, query, start) {
  const sequence = ++autocomplete.sequence;

  if (autocomplete.controller) autocomplete.controller.abort();
  const controller = new AbortController();
  autocomplete.controller = controller;

  try {
    const params = new URLSearchParams({ q: query });
    const data = await requestJson(`${API_NOTES}?${params.toString()}`, {
      signal: controller.signal,
    });

    if (
      sequence !== autocomplete.sequence ||
      !autocomplete.open ||
      autocomplete.state !== state ||
      autocomplete.start !== start
    ) {
      return;
    }

    const items = Array.isArray(data)
      ? data
      : Array.isArray(data.notes)
        ? data.notes
        : Array.isArray(data.results)
          ? data.results
          : [];

    autocomplete.items = items;
    autocomplete.activeIndex = 0;
    renderAutocomplete();
  } catch (error) {
    if (error?.name === 'AbortError') return;
    if (sequence !== autocomplete.sequence || !autocomplete.open) return;
    autocomplete.items = [];
    renderAutocomplete(errorMessage(error));
  }
}

function renderAutocomplete(error = '') {
  const menu = ensureAutocompleteMenu();
  if (!autocomplete.open) {
    menu.hidden = true;
    return;
  }

  if (error) {
    menu.innerHTML = `<div class="zcast-wikilink-empty">${escapeHtml(error)}</div>`;
  } else if (autocomplete.items.length === 0) {
    menu.innerHTML = '<div class="zcast-wikilink-empty">No notes found</div>';
  } else {
    menu.innerHTML = autocomplete.items
      .map((note, index) => {
        const active = index === autocomplete.activeIndex ? ' is-active' : '';
        const selected = index === autocomplete.activeIndex ? 'true' : 'false';
        const lock = note.private
          ? '<span class="zcast-wikilink-private">Private</span>'
          : '';
        return `<button type="button" class="zcast-wikilink-option${active}" role="option" aria-selected="${selected}" data-zcast-wikilink-index="${index}">
          <span class="zcast-wikilink-title">${escapeHtml(note.title || 'Untitled')}</span>
          <span class="zcast-wikilink-slug">${escapeHtml(note.slug || '')}</span>
          ${lock}
        </button>`;
      })
      .join('');
  }

  menu.hidden = false;
  positionAutocomplete();
}

function positionAutocomplete() {
  if (!autocomplete.open || !autocomplete.state?.editor) return;

  const menu = ensureAutocompleteMenu();
  const editor = autocomplete.state.editor;
  const position = autocomplete.state.editor.state.selection.from;

  let coords;
  try {
    coords = editor.view.coordsAtPos(position);
  } catch {
    return;
  }

  const gap = 6;
  const margin = 8;
  const width = menu.offsetWidth || 300;
  const height = menu.offsetHeight || 180;

  let left = coords.left;
  if (left + width > window.innerWidth - margin) {
    left = window.innerWidth - width - margin;
  }
  left = Math.max(margin, left);

  let top = coords.bottom + gap;
  if (top + height > window.innerHeight - margin) {
    top = Math.max(margin, coords.top - height - gap);
  }

  menu.style.left = `${Math.round(left)}px`;
  menu.style.top = `${Math.round(top)}px`;
}

function moveAutocomplete(delta) {
  if (!autocomplete.open || autocomplete.items.length === 0) return;
  const count = autocomplete.items.length;
  autocomplete.activeIndex = (autocomplete.activeIndex + delta + count) % count;
  renderAutocomplete();
}

function handleAutocompleteKey(event) {
  if (!autocomplete.open) return false;

  switch (event.key) {
    case 'ArrowDown':
      event.preventDefault();
      moveAutocomplete(1);
      return true;
    case 'ArrowUp':
      event.preventDefault();
      moveAutocomplete(-1);
      return true;
    case 'Enter':
    case 'Tab':
      event.preventDefault();
      if (autocomplete.items.length > 0) {
        selectWikilink(autocomplete.activeIndex);
      } else {
        dismissAutocomplete();
      }
      return true;
    case 'Escape':
      event.preventDefault();
      dismissAutocomplete();
      return true;
    default:
      return false;
  }
}

function selectWikilink(index) {
  const item = autocomplete.items[index];
  const state = autocomplete.state;
  if (!item || !state?.editor) return;

  const editor = state.editor;
  const start = autocomplete.start;
  const current = editor.state.selection.from;
  hideAutocomplete(state);

  editor
    .chain()
    .focus()
    .insertContentAt(
      { from: start, to: Math.max(start, current) },
      { type: 'text', text: `[[${item.title || 'Untitled'}]]` },
    )
    .run();
}

function dismissAutocomplete() {
  autocomplete.dismissedKey = `${autocomplete.start}:${autocomplete.query}`;
  hideAutocomplete();
}

function hideAutocomplete(state = null) {
  if (state && autocomplete.state && autocomplete.state !== state) return;

  if (autocomplete.timer) {
    clearTimeout(autocomplete.timer);
    autocomplete.timer = null;
  }
  if (autocomplete.controller) {
    autocomplete.controller.abort();
    autocomplete.controller = null;
  }
  autocomplete.sequence += 1;
  autocomplete.open = false;
  autocomplete.state = null;
  autocomplete.items = [];
  autocomplete.activeIndex = 0;
  if (autocompleteMenu) autocompleteMenu.hidden = true;
}

/* -------------------------------------------------------------------------- */
/* Draft panes                                                                */
/* -------------------------------------------------------------------------- */

function draftCardHtml(title, token) {
  const safeTitle = escapeHtml(title || '');
  return `<article class="zcast-card zcast-card-draft zcast-card--entering" data-zcast-card data-zcast-card-id="" data-zcast-card-slug="" data-zcast-card-private="false" data-zcast-card-admin="true" data-zcast-draft="true" data-zcast-draft-token="${token}">
    <button type="button" class="zcast-card__close" data-zcast-close-pane aria-label="Close pane" title="Close pane">&times;</button>
    <div class="zcast-card__toolbar" data-zcast-card-toolbar>
      <button type="button" class="zcast-btn zcast-btn-edit" data-zcast-edit-toggle data-zcast-card-id="" aria-pressed="false" title="Edit note">Edit</button>
      <button type="button" class="zcast-btn zcast-btn-private" data-zcast-private-toggle data-zcast-card-id="" data-private="false" aria-pressed="false" title="Public note - click to make private">Public</button>
      <button type="button" class="zcast-btn zcast-btn-discard" data-zcast-discard-draft title="Discard this unsaved draft">Discard</button>
      <span class="zcast-save-status" data-zcast-save-status role="status" aria-live="polite"></span>
    </div>
    <header class="zcast-card__header"><h1 class="zcast-card-title" data-zcast-card-title contenteditable="false">${safeTitle}</h1></header>
    <div class="zcast-card__body"><div class="zcast-card-body" data-zcast-card-body></div></div>
    <footer class="zcast-card__footer" data-zcast-card-footer></footer>
  </article>`;
}

async function reuseBlankDraft(card, title) {
  const clean = normaliseTitle(title);
  if (!clean) {
    focusDraftPane(card);
    return;
  }

  let state = editorStates.get(card);
  if (!state?.editing) {
    await enterEdit(card, { draft: true, title: clean, focus: 'body' });
    state = editorStates.get(card);
  }

  if (state) {
    state.note = { ...(state.note || {}), title: clean };
    if (state.titleElement) state.titleElement.textContent = clean;
    state.dirty = true;
    if (state.editor) state.editor.commands.focus('end');
    scheduleSave(state);
  }

  scrollPaneIntoView(card);
  pulseCard(card);
  setStatus(card, '');
  syncDraftActiveState();
}

/**
 * Opening an unresolved `[[Title]]` never spawns a duplicate draft: reuse the
 * matching or untouched draft, otherwise jump to the unsaved one.
 */
async function openDraftForTitle(title, anchor = null) {
  const clean = normaliseTitle(title);
  const drafts = openDraftCards();

  const matching = drafts.find((card) => titleKey(draftTitle(card)) === titleKey(clean));
  if (matching) {
    focusDraftPane(matching);
    return matching;
  }

  const blank = drafts.find((card) => draftIsBlank(card));
  if (blank) {
    await reuseBlankDraft(blank, clean);
    return blank;
  }

  if (drafts.length > 0) {
    focusDraftPane(drafts[0], 'Save current draft first');
    return drafts[0];
  }

  return openDraftPane(clean, anchor);
}

async function openDraftPane(title, anchor = null) {
  const clean = normaliseTitle(title);

  const existing = findOpenDraftCard();
  if (existing) {
    // Never spawn a second unsaved draft pane (issue #2).
    if (clean && draftIsBlank(existing)) {
      await reuseBlankDraft(existing, clean);
    } else if (clean && titleKey(draftTitle(existing)) === titleKey(clean)) {
      focusDraftPane(existing);
    } else {
      focusDraftPane(existing, 'Save or close current draft first');
    }
    return existing;
  }

  const token = `draft-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  const html = draftCardHtml(clean, token);
  let card = null;
  const existingCards = document.querySelectorAll('[data-zcast-card]');
  const anchorCard = anchor || existingCards[existingCards.length - 1] || null;

  const panes = window.ZcastPanes;
  if (panes && typeof panes.appendCardHtml === 'function') {
    try {
      const result = await panes.appendCardHtml(anchorCard, html);
      if (result instanceof Element) {
        card = result.matches('[data-zcast-card]') ? result : result.closest('[data-zcast-card]');
      }
    } catch {
      card = null;
    }
  }

  if (!card) {
    card = document.querySelector(`[data-zcast-draft-token="${token}"]`);
  }

  if (!card) {
    const container =
      document.querySelector('[data-zcast-panes]') ||
      document.querySelector('.zcast-panes') ||
      document.querySelector('main') ||
      document.body;
    if (anchor && anchor.parentElement === container) {
      anchor.insertAdjacentHTML('afterend', html);
    } else {
      container.insertAdjacentHTML('beforeend', html);
    }
    card = container.querySelector(`[data-zcast-draft-token="${token}"]`);
  }

  if (!card) return null;

  card.removeAttribute('data-zcast-draft-token');
  card.dataset.zcastDraft = 'true';
  ensureDiscardButton(card);

  window.setTimeout(() => card.classList.remove('zcast-card--entering'), 260);

  const focusMode = clean ? 'body' : 'title';
  await enterEdit(card, {
    draft: true,
    title: clean,
    focus: focusMode,
    deferFocus: true,
  });

  scrollPaneIntoView(card);
  syncDraftActiveState();
  return card;
}

async function discardDraft(card) {
  if (!card) return false;

  const state = editorStates.get(card);
  if (state?.editing) {
    await exitEdit(state, { discard: true });
    return true;
  }

  editorStates.delete(card);
  closeCardPane(card);
  return true;
}

/* -------------------------------------------------------------------------- */
/* Delegated listeners                                                        */
/* -------------------------------------------------------------------------- */

function handleDocumentClick(event) {
  const element = eventElement(event);
  if (!element) return;

  const unresolved = element.closest('a[data-zcast-unresolved]');
  if (unresolved) {
    // The link parser emits these anchors only for admin sessions, so their
    // presence is the admin-mode signal. Never intercept links in the editor.
    if (element.closest('.zcast-editor-surface')) return;
    event.preventDefault();
    event.stopPropagation();
    const title = unresolved.dataset.zcastUnresolved || unresolved.textContent.trim();
    void openDraftForTitle(title, cardFor(unresolved));
    return;
  }

  const closeButton = element.closest('[data-zcast-close-pane]');
  if (closeButton) {
    const card = cardFor(closeButton);
    const state = card ? editorStates.get(card) : null;
    const unsavedDraft = Boolean(card) && card.dataset.zcastDraft === 'true' && !cardId(card);

    // Saved, non-editing cards fall through cleanly to panes.js below; only
    // editor panes and drafts need the flush-before-close dance.
    if (card && (state?.editing || unsavedDraft)) {
      if (allCardPanes().indexOf(card) <= 0) return;
      event.preventDefault();
      event.stopPropagation();
      void closeEditorPane(card);
      return;
    }
  }

  const discardButton = element.closest('[data-zcast-discard-draft]');
  if (discardButton) {
    const card = cardFor(discardButton);
    if (card) {
      event.preventDefault();
      event.stopPropagation();
      void discardDraft(card);
    }
    return;
  }

  const deleteButton = element.closest('[data-zcast-delete-note]');
  if (deleteButton) {
    const card = cardFor(deleteButton);
    if (card) {
      event.preventDefault();
      event.stopPropagation();
      void deleteNote(card);
    }
    return;
  }

  const newNote = element.closest('[data-zcast-new-note]');
  if (newNote) {
    event.preventDefault();
    event.stopPropagation();
    const existingDraft = findOpenDraftCard();
    if (existingDraft) {
      // Jump to (and pulse) the open draft instead of spawning duplicates.
      focusDraftPane(existingDraft, 'Save or close current draft first');
      return;
    }
    void openDraftPane('');
    return;
  }

  const editToggle = element.closest('[data-zcast-edit-toggle]');
  if (editToggle) {
    const card = cardFor(editToggle);
    if (card) {
      event.preventDefault();
      void toggleEdit(card);
    }
    return;
  }

  const privateToggle = element.closest('[data-zcast-private-toggle]');
  if (privateToggle) {
    const card = cardFor(privateToggle);
    if (card) {
      event.preventDefault();
      void togglePrivate(card);
    }
  }
}

function handleDocumentInput(event) {
  const target = eventElement(event);
  const titleElement = target?.closest?.('[data-zcast-card-title][contenteditable="true"]');
  if (!titleElement) return;

  const card = cardFor(titleElement);
  const state = card ? editorStates.get(card) : null;
  if (!state?.editing) return;

  state.dirty = true;
  scheduleSave(state);
}

function handleDocumentKeyDown(event) {
  const element = eventElement(event);

  const titleElement = element?.closest?.('[data-zcast-card-title][contenteditable="true"]');
  if (titleElement && event.key === 'Enter') {
    event.preventDefault();
    titleElement.blur();
    return;
  }

  if (!(event.metaKey || event.ctrlKey) || event.key.toLowerCase() !== 's') return;

  const state = activeStateForEvent(event);
  if (!state) return;

  event.preventDefault();
  event.stopPropagation();
  void flushSave(state);
}

function handleDocumentPaste(event) {
  const element = eventElement(event);
  const titleElement = element?.closest?.('[data-zcast-card-title][contenteditable="true"]');
  if (!titleElement) return;

  event.preventDefault();
  const text = (event.clipboardData?.getData('text/plain') || '').replace(/\s+/g, ' ');
  document.execCommand('insertText', false, text);
}

function handleDocumentPointerDown(event) {
  if (!autocomplete.open) return;

  const element = eventElement(event);
  if (autocompleteMenu && element && autocompleteMenu.contains(element)) return;
  if (autocomplete.state?.editor?.view.dom.contains(element)) return;
  hideAutocomplete();
}

function handleViewportChange() {
  repositionFloatingToolbar();
  if (autocomplete.open) positionAutocomplete();
}

/** Keep the "+ New Note" affordance honest when panes.js truncates panes. */
function observeDraftPanes() {
  if (typeof MutationObserver !== 'function') return;

  const container =
    document.querySelector('[data-zcast-panes]') || document.querySelector('.zcast-panes');
  if (!container) return;

  new MutationObserver(() => syncDraftActiveState()).observe(container, { childList: true });
}

function init() {
  hydrateCardToolbars();
  syncDraftActiveState();
  observeDraftPanes();

  document.addEventListener('click', handleDocumentClick, true);
  document.addEventListener('input', handleDocumentInput);
  document.addEventListener('keydown', handleDocumentKeyDown, true);
  document.addEventListener('paste', handleDocumentPaste, true);
  document.addEventListener('pointerdown', handleDocumentPointerDown, true);
  window.addEventListener('resize', handleViewportChange);
  window.addEventListener('scroll', handleViewportChange, true);
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', init, { once: true });
} else {
  init();
}
