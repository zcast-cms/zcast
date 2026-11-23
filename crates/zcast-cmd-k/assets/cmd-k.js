/* ==========================================================================
 * zcast-cmd-k — instant Cmd+K spotlight search
 *
 * Zero dependencies. The modal DOM is created lazily on first open, then all
 * search traffic goes to the kernel's `/_zcast/search` endpoint which is
 * backed by the versioned `search@^1.0` provider contract (DEC-01).
 *
 * Expected response shape:
 *   { query, capabilities, hits: [{ note_id, slug, title, snippet, score,
 *     match_kind }] }
 * ========================================================================== */

(function () {
  'use strict';

  var SEARCH_ENDPOINT = '/_zcast/search';
  var DEBOUNCE_MS = 80;
  var RESULT_ID = 'zcast-cmdk-results';

  var isMac = /Mac|iPhone|iPad|iPod/.test(
    (navigator.platform || '') + ' ' + (navigator.userAgent || '')
  );
  var keycapLabel = isMac ? '⌘K' : 'Ctrl K';

  var state = {
    open: false,
    ready: false,
    lastFocused: null,
    hits: [],
    activeIndex: -1,
    createTitle: '',
    query: '',
    controller: null,
    debounce: null,
    seq: 0,
    loading: false,
    error: false
  };

  var els = {
    overlay: null,
    backdrop: null,
    dialog: null,
    input: null,
    results: null
  };

  /* ---------------------------------------------------------------------- */
  /* Small helpers                                                          */
  /* ---------------------------------------------------------------------- */

  function escapeHtml(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function escapeRegExp(value) {
    return String(value).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  }

  function queryTokens(query) {
    return String(query || '')
      .split(/\s+/)
      .filter(function (token) {
        return token.length > 0;
      });
  }

  function highlight(text, query) {
    var raw = String(text == null ? '' : text);
    var tokens = queryTokens(query);
    if (tokens.length === 0) {
      return escapeHtml(raw);
    }

    var pattern = new RegExp(
      '(' +
        tokens
          .map(escapeRegExp)
          .join('|') +
        ')',
      'gi'
    );
    var output = '';
    var lastIndex = 0;
    var match;

    while ((match = pattern.exec(raw)) !== null) {
      if (match.index > lastIndex) {
        output += escapeHtml(raw.slice(lastIndex, match.index));
      }
      output += '<mark>' + escapeHtml(match[0]) + '</mark>';
      lastIndex = match.index + match[0].length;
      if (match[0].length === 0) {
        pattern.lastIndex += 1;
      }
    }

    output += escapeHtml(raw.slice(lastIndex));
    return output;
  }

  function rightmostCard() {
    var cards = document.querySelectorAll('.zcast-card');
    return cards.length > 0 ? cards[cards.length - 1] : null;
  }

  function isAdminBody() {
    return !!(document.body && document.body.classList.contains('zcast-body--admin'));
  }

  function updateShortcutHints() {
    var nodes = document.querySelectorAll('[data-zcast-cmdk-kbd]');
    for (var i = 0; i < nodes.length; i += 1) {
      nodes[i].textContent = keycapLabel;
    }
  }

  function itemCount() {
    return state.hits.length + (state.createTitle ? 1 : 0);
  }

  /* ---------------------------------------------------------------------- */
  /* Lazy modal construction                                                */
  /* ---------------------------------------------------------------------- */

  function make(tag, className) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    return node;
  }

  function makeHint(keys, label) {
    var hint = make('span', 'zcast-cmdk__hint');
    hint.setAttribute('aria-hidden', 'true');
    for (var i = 0; i < keys.length; i += 1) {
      var kbd = make('kbd');
      kbd.textContent = keys[i];
      hint.appendChild(kbd);
    }
    hint.appendChild(document.createTextNode(' ' + label));
    return hint;
  }

  function ensureDom() {
    if (state.ready) {
      return;
    }

    var overlay = make('div', 'zcast-cmdk');
    overlay.setAttribute('data-zcast-cmdk', '');
    overlay.hidden = true;

    var backdrop = make('div', 'zcast-cmdk__backdrop');
    backdrop.setAttribute('data-zcast-cmdk-backdrop', '');

    var dialog = make('div', 'zcast-cmdk__dialog');
    dialog.setAttribute('role', 'dialog');
    dialog.setAttribute('aria-modal', 'true');
    dialog.setAttribute('aria-label', 'Search notes');

    var search = make('div', 'zcast-cmdk__search');

    var icon = make('span', 'zcast-cmdk__icon');
    icon.setAttribute('aria-hidden', 'true');
    icon.innerHTML =
      '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" ' +
      'stroke-linecap="round" stroke-linejoin="round" focusable="false">' +
      '<circle cx="8.5" cy="8.5" r="5.25"></circle>' +
      '<path d="m12.4 12.4 4.1 4.1"></path>' +
      '</svg>';

    var input = make('input', 'zcast-cmdk__input');
    input.type = 'text';
    input.placeholder = 'Search notes…';
    input.autocomplete = 'off';
    input.spellcheck = false;
    input.setAttribute('role', 'combobox');
    input.setAttribute('aria-expanded', 'true');
    input.setAttribute('aria-controls', RESULT_ID);
    input.setAttribute('aria-autocomplete', 'list');
    input.setAttribute('aria-label', 'Search notes');

    var esc = make('kbd', 'zcast-cmdk__esc');
    esc.textContent = 'esc';

    search.appendChild(icon);
    search.appendChild(input);
    search.appendChild(esc);

    var results = make('div', 'zcast-cmdk__results');
    results.id = RESULT_ID;
    results.setAttribute('role', 'listbox');
    results.setAttribute('aria-label', 'Search results');

    var footer = make('div', 'zcast-cmdk__footer');
    footer.setAttribute('aria-hidden', 'true');
    footer.appendChild(makeHint(['↑', '↓'], 'navigate'));
    footer.appendChild(makeHint(['↵'], 'open'));
    footer.appendChild(makeHint(['esc'], 'close'));

    dialog.appendChild(search);
    dialog.appendChild(results);
    dialog.appendChild(footer);
    overlay.appendChild(backdrop);
    overlay.appendChild(dialog);
    document.body.appendChild(overlay);

    els.overlay = overlay;
    els.backdrop = backdrop;
    els.dialog = dialog;
    els.input = input;
    els.results = results;
    state.ready = true;

    backdrop.addEventListener('click', function () {
      close(true);
    });

    input.addEventListener('input', function () {
      scheduleSearch(input.value);
    });

    results.addEventListener('click', function (event) {
      var item = closestItem(event.target);
      if (!item) {
        return;
      }
      activateItem(item);
    });

    results.addEventListener('mousemove', function (event) {
      var item = closestItem(event.target);
      if (!item) {
        return;
      }
      var index = itemIndex(item);
      if (index >= 0 && index !== state.activeIndex) {
        state.activeIndex = index;
        syncActive(false);
      }
    });
  }

  function closestItem(target) {
    if (!target || typeof target.closest !== 'function') {
      return null;
    }
    var item = target.closest('.zcast-cmdk__item');
    return item && els.results && els.results.contains(item) ? item : null;
  }

  function itemIndex(item) {
    if (item.getAttribute('data-zcast-cmdk-create') === 'true') {
      return state.createTitle ? state.hits.length : -1;
    }
    var raw = item.getAttribute('data-zcast-cmdk-index');
    if (raw == null) {
      return -1;
    }
    var parsed = parseInt(raw, 10);
    return isNaN(parsed) ? -1 : parsed;
  }

  function activateItem(item) {
    var index = itemIndex(item);
    if (index < 0) {
      return;
    }
    state.activeIndex = index;
    activateActive();
  }

  /* ---------------------------------------------------------------------- */
  /* Open / close lifecycle                                                 */
  /* ---------------------------------------------------------------------- */

  function toggle() {
    if (state.open) {
      close(true);
    } else {
      open();
    }
  }

  function open() {
    ensureDom();
    updateShortcutHints();

    if (state.open) {
      return;
    }

    state.open = true;
    state.lastFocused = document.activeElement;
    state.hits = [];
    state.activeIndex = -1;
    state.createTitle = '';
    state.query = '';
    state.error = false;
    state.loading = true;

    els.input.value = '';
    els.input.removeAttribute('aria-activedescendant');
    els.overlay.hidden = false;
    document.body.classList.add('zcast-cmdk-open');

    renderResults();
    els.input.focus();

    // Immediately show recent/default notes for an empty query.
    runSearch('');
  }

  function close(restoreFocus) {
    if (!state.open) {
      return;
    }

    state.open = false;
    state.loading = false;

    if (state.debounce !== null) {
      window.clearTimeout(state.debounce);
      state.debounce = null;
    }
    if (state.controller) {
      state.controller.abort();
      state.controller = null;
    }
    state.seq += 1;

    els.overlay.hidden = true;
    document.body.classList.remove('zcast-cmdk-open');

    var target = state.lastFocused;
    state.lastFocused = null;

    if (restoreFocus && target && typeof target.focus === 'function' && document.contains(target)) {
      try {
        target.focus();
      } catch (error) {
        /* Focus can fail while navigating away — nothing to recover from. */
      }
    }
  }

  /* ---------------------------------------------------------------------- */
  /* Search                                                                 */
  /* ---------------------------------------------------------------------- */

  function scheduleSearch(value) {
    if (state.debounce !== null) {
      window.clearTimeout(state.debounce);
    }
    state.debounce = window.setTimeout(function () {
      state.debounce = null;
      runSearch(value);
    }, DEBOUNCE_MS);
  }

  function buildSearchUrl(query) {
    var parts = ['q=' + encodeURIComponent(query), 'limit=12'];

    var anchor = rightmostCard();
    if (anchor) {
      var contextId = anchor.getAttribute('data-zcast-card-id');
      if (contextId) {
        parts.push('context=' + encodeURIComponent(contextId));
      }
    }

    var search = window.location.search || '';
    if (search.length > 1) {
      var params = new URLSearchParams(search);
      if (params.get('preview') === 'public') {
        parts.push('preview=public');
      }
    }

    return SEARCH_ENDPOINT + '?' + parts.join('&');
  }

  function runSearch(value) {
    var query = String(value == null ? '' : value).trim();

    state.query = query;
    state.error = false;
    state.loading = true;
    state.seq += 1;
    var seq = state.seq;

    if (state.controller) {
      state.controller.abort();
    }

    var controller = null;
    if (typeof AbortController !== 'undefined') {
      controller = new AbortController();
    }
    state.controller = controller;

    var options = {
      method: 'GET',
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
      cache: 'no-store'
    };
    if (controller) {
      options.signal = controller.signal;
    }

    renderResults();

    fetch(buildSearchUrl(query), options)
      .then(function (response) {
        if (!response.ok) {
          throw new Error('search request failed');
        }
        return response.json();
      })
      .then(function (payload) {
        if (seq !== state.seq) {
          return;
        }
        state.hits = payload && Array.isArray(payload.hits) ? payload.hits : [];
        state.loading = false;
        state.error = false;
        if (state.controller === controller) {
          state.controller = null;
        }
        renderResults();
      })
      .catch(function (error) {
        if (error && error.name === 'AbortError') {
          return;
        }
        if (seq !== state.seq) {
          return;
        }
        state.hits = [];
        state.loading = false;
        state.error = true;
        if (state.controller === controller) {
          state.controller = null;
        }
        renderResults();
      });
  }

  /* ---------------------------------------------------------------------- */
  /* Rendering                                                              */
  /* ---------------------------------------------------------------------- */

  function renderResults() {
    if (!state.ready) {
      return;
    }

    var results = els.results;
    results.textContent = '';

    var query = state.query;

    if (state.loading && state.hits.length === 0) {
      appendStatus('Searching…');
    }

    for (var i = 0; i < state.hits.length; i += 1) {
      results.appendChild(buildHitItem(state.hits[i], i, query));
    }

    if (!state.loading && state.error) {
      appendStatus('Search is unavailable right now');
    } else if (!state.loading && state.hits.length === 0) {
      appendStatus(query ? 'No notes found' : 'No recent notes');
    }

    state.createTitle = state.loading ? '' : maybeCreateTitle(query);
    if (state.createTitle) {
      results.appendChild(buildCreateItem(state.createTitle));
    }

    var total = itemCount();
    if (state.activeIndex < 0 && total > 0) {
      state.activeIndex = 0;
    }
    if (state.activeIndex >= total) {
      state.activeIndex = total > 0 ? total - 1 : -1;
    }

    syncActive(false);
  }

  function appendStatus(message) {
    var status = make('div', 'zcast-cmdk__status');
    status.setAttribute('role', 'status');
    status.setAttribute('aria-live', 'polite');
    status.textContent = message;
    els.results.appendChild(status);
  }

  function buildHitItem(hit, index, query) {
    var item = make('div', 'zcast-cmdk__item');
    item.setAttribute('role', 'option');
    item.id = 'zcast-cmdk-opt-' + index;
    item.setAttribute('data-zcast-cmdk-index', String(index));
    item.setAttribute('aria-selected', 'false');

    var title = make('span', 'zcast-cmdk__title');
    title.innerHTML = highlight(hit && hit.title, query);

    var snippet = make('span', 'zcast-cmdk__snippet');
    snippet.innerHTML = highlight(hit && hit.snippet, query);

    item.appendChild(title);
    item.appendChild(snippet);
    return item;
  }

  function maybeCreateTitle(query) {
    if (!query || !isAdminBody()) {
      return '';
    }

    var needle = query.toLowerCase();
    for (var i = 0; i < state.hits.length; i += 1) {
      var hit = state.hits[i];
      var title = hit && typeof hit.title === 'string' ? hit.title.trim().toLowerCase() : '';
      if (title === needle) {
        return '';
      }
    }
    return query;
  }

  function buildCreateItem(title) {
    var item = make('div', 'zcast-cmdk__item zcast-cmdk__item--create');
    item.setAttribute('role', 'option');
    item.id = 'zcast-cmdk-opt-create';
    item.setAttribute('data-zcast-cmdk-create', 'true');
    item.setAttribute('aria-selected', 'false');
    item.textContent = '+ Create "' + title + '"';
    return item;
  }

  function syncActive(scroll) {
    if (!state.ready) {
      return;
    }

    var items = els.results.querySelectorAll('.zcast-cmdk__item');
    var activeNode = null;

    for (var i = 0; i < items.length; i += 1) {
      var item = items[i];
      var isActive = itemIndex(item) === state.activeIndex;
      item.setAttribute('aria-selected', isActive ? 'true' : 'false');
      if (isActive) {
        activeNode = item;
      }
    }

    if (activeNode) {
      els.input.setAttribute('aria-activedescendant', activeNode.id);
      if (scroll !== false && typeof activeNode.scrollIntoView === 'function') {
        activeNode.scrollIntoView({ block: 'nearest' });
      }
    } else {
      els.input.removeAttribute('aria-activedescendant');
    }
  }

  function moveActive(delta) {
    var total = itemCount();
    if (total === 0) {
      return;
    }

    var next;
    if (state.activeIndex < 0) {
      next = delta > 0 ? 0 : total - 1;
    } else {
      next = (state.activeIndex + delta + total) % total;
    }

    state.activeIndex = next;
    syncActive(true);
  }

  function activateActive() {
    if (state.activeIndex < 0) {
      return;
    }
    if (state.activeIndex >= state.hits.length) {
      selectCreate();
      return;
    }
    selectHit(state.hits[state.activeIndex]);
  }

  function selectHit(hit) {
    if (!hit || !hit.slug) {
      return;
    }

    var slug = String(hit.slug);
    var anchor = rightmostCard();
    close(false);

    if (
      window.ZcastPanes &&
      typeof window.ZcastPanes.openSlug === 'function' &&
      anchor &&
      anchor.isConnected
    ) {
      window.ZcastPanes.openSlug(slug, anchor);
      return;
    }

    window.location.assign('/' + encodeURIComponent(slug));
  }

  function selectCreate() {
    var title = state.createTitle || state.query;
    if (!title) {
      return;
    }

    close(false);
    document.dispatchEvent(
      new CustomEvent('zcast:open-draft', {
        detail: { title: title }
      })
    );
  }

  /* ---------------------------------------------------------------------- */
  /* Global event wiring                                                    */
  /* ---------------------------------------------------------------------- */

  function onKeyDown(event) {
    var key = event.key || '';

    if ((event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey && (key === 'k' || key === 'K')) {
      event.preventDefault();
      if (!event.repeat) {
        toggle();
      }
      return;
    }

    if (!state.open) {
      return;
    }

    if (key === 'Escape' || key === 'Esc') {
      event.preventDefault();
      event.stopPropagation();
      close(true);
      return;
    }

    if (key === 'ArrowDown') {
      event.preventDefault();
      moveActive(1);
      return;
    }

    if (key === 'ArrowUp') {
      event.preventDefault();
      moveActive(-1);
      return;
    }

    if (key === 'Enter') {
      event.preventDefault();
      activateActive();
    }
  }

  function onClick(event) {
    var target = event.target;
    if (!target || typeof target.closest !== 'function') {
      return;
    }

    var trigger = target.closest('[data-zcast-cmdk-open]');
    if (!trigger) {
      return;
    }

    event.preventDefault();
    toggle();
  }

  function init() {
    updateShortcutHints();
    document.addEventListener('keydown', onKeyDown);
    document.addEventListener('click', onClick);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
