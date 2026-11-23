/*
 * zcast-popover: dependency-free hover previews for
 * <a data-zcast-note="slug"> links.
 *
 * The kernel serves preview JSON from GET /_zcast/popover/:slug. This script
 * only handles hover intent, caching, positioning and accessibility; it never
 * changes the underlying link or navigation behaviour.
 */
(function () {
    'use strict';

    var SELECTOR = 'a[data-zcast-note]';
    var POPOVER_CLASS = 'zcast-popover';
    var HOVER_DELAY_MS = 150;
    var LEAVE_DELAY_MS = 180;
    var CACHE_TTL_MS = 30 * 1000;
    var VIEWPORT_MARGIN = 8;
    var LINK_GAP = 8;

    var root = null;
    var titleEl = null;
    var bodyEl = null;
    var metaEl = null;

    var activeLink = null;
    var activeSlug = null;
    var showTimer = 0;
    var hideTimer = 0;
    var requestSeq = 0;
    var visible = false;

    var cache = new Map();
    var hoverMedia = typeof window.matchMedia === 'function'
        ? window.matchMedia('(hover: none)')
        : null;

    function canHover() {
        if (typeof window.matchMedia === 'function' && window.matchMedia('(hover: none)').matches) {
            return false;
        }

        return !hoverMedia || !hoverMedia.matches;
    }

    function isNode(value) {
        return !!value && typeof value.nodeType === 'number';
    }

    function buildRoot() {
        if (root) {
            return;
        }

        root = document.createElement('div');
        root.className = POPOVER_CLASS;
        root.setAttribute('role', 'tooltip');
        root.setAttribute('aria-hidden', 'true');
        root.setAttribute('data-zcast-popover', '');
        root.innerHTML =
            '<div class="zcast-popover__header">' +
                '<span class="zcast-popover__title"></span>' +
            '</div>' +
            '<div class="zcast-popover__body-wrap">' +
                '<div class="zcast-popover__body"></div>' +
            '</div>' +
            '<div class="zcast-popover__meta" hidden></div>';

        titleEl = root.querySelector('.zcast-popover__title');
        bodyEl = root.querySelector('.zcast-popover__body');
        metaEl = root.querySelector('.zcast-popover__meta');

        document.body.appendChild(root);
    }

    function linkFromTarget(target) {
        if (!target || typeof target.closest !== 'function') {
            return null;
        }

        var link = target.closest(SELECTOR);
        if (!link) {
            return null;
        }

        if (link.hasAttribute('data-zcast-unresolved')) {
            return null;
        }

        if (link.closest('.zcast-editor-surface, [contenteditable="true"], .zcast-popover')) {
            return null;
        }

        if (!link.getAttribute('data-zcast-note')) {
            return null;
        }

        return link;
    }

    function isSelfLink(link, slug) {
        var card = link.closest('[data-zcast-card]');
        if (!card) {
            return false;
        }

        var cardSlug = card.getAttribute('data-zcast-card-slug') ||
            card.getAttribute('data-zcast-note');

        return !!cardSlug && cardSlug === slug;
    }

    function cancelShow() {
        if (showTimer) {
            window.clearTimeout(showTimer);
            showTimer = 0;
        }
    }

    function cancelHide() {
        if (hideTimer) {
            window.clearTimeout(hideTimer);
            hideTimer = 0;
        }
    }

    function hideNow() {
        cancelShow();
        cancelHide();
        requestSeq += 1;
        activeLink = null;
        activeSlug = null;
        visible = false;

        if (root) {
            root.classList.remove('is-visible');
            root.setAttribute('aria-hidden', 'true');
        }
    }

    function scheduleShow(link) {
        var slug = link.getAttribute('data-zcast-note');

        if (!slug || !canHover() || isSelfLink(link, slug)) {
            return;
        }

        cancelShow();
        cancelHide();

        if (activeLink === link && visible) {
            return;
        }

        showTimer = window.setTimeout(function () {
            showTimer = 0;
            openFor(link, slug);
        }, HOVER_DELAY_MS);
    }

    function scheduleHide() {
        cancelShow();
        cancelHide();

        hideTimer = window.setTimeout(function () {
            hideTimer = 0;

            if (root && typeof root.matches === 'function' && root.matches(':hover')) {
                return;
            }

            hideNow();
        }, LEAVE_DELAY_MS);
    }

    function openFor(link, slug) {
        if (!canHover() || !link.isConnected) {
            return;
        }

        activeLink = link;
        activeSlug = slug;

        var cached = cache.get(slug);
        if (cached && (Date.now() - cached.at) < CACHE_TTL_MS) {
            render(link, cached.data);
            return;
        }

        var seq = ++requestSeq;

        fetchPopover(slug).then(function (data) {
            if (seq !== requestSeq || activeLink !== link || !data) {
                return;
            }

            var now = Date.now();
            cache.set(slug, { data: data, at: now });

            if (data.canonical_slug && data.canonical_slug !== slug) {
                cache.set(data.canonical_slug, { data: data, at: now });
            }

            render(link, data);
        }).catch(function () {
            if (seq === requestSeq) {
                hideNow();
            }
        });
    }

    function fetchPopover(slug) {
        var url = '/_zcast/popover/' + encodeURIComponent(slug);

        if (typeof window.URLSearchParams === 'function') {
            var params = new window.URLSearchParams(window.location.search);
            if (params.get('preview') === 'public') {
                url += '?preview=public';
            }
        }

        return fetch(url, {
            method: 'GET',
            headers: {
                'Accept': 'application/json'
            },
            credentials: 'same-origin'
        }).then(function (response) {
            if (!response.ok) {
                throw new Error('Popover request failed: ' + response.status);
            }

            return response.json();
        });
    }

    function render(link, data) {
        if (!root || !titleEl || !bodyEl || !metaEl) {
            return;
        }

        activeLink = link;
        activeSlug = data.canonical_slug || data.slug || link.getAttribute('data-zcast-note');

        titleEl.textContent = data.title || '';
        bodyEl.innerHTML = data.excerpt_html || '';

        var backlinks = Number(data.backlink_count) || 0;
        if (backlinks > 0) {
            metaEl.hidden = false;
            metaEl.textContent = backlinks === 1 ? '1 backlink' : backlinks + ' backlinks';
        } else {
            metaEl.hidden = true;
            metaEl.textContent = '';
        }

        if (data.updated_at) {
            root.setAttribute('data-updated-at', data.updated_at);
        } else {
            root.removeAttribute('data-updated-at');
        }

        root.classList.add('is-visible');
        root.setAttribute('aria-hidden', 'false');
        visible = true;

        position(link);
    }

    function position(link) {
        if (!root) {
            return;
        }

        var linkRect = link.getBoundingClientRect();
        var popRect = root.getBoundingClientRect();
        var viewportWidth = document.documentElement.clientWidth || window.innerWidth || 0;
        var viewportHeight = document.documentElement.clientHeight || window.innerHeight || 0;

        var top = linkRect.bottom + LINK_GAP;
        var placement = 'below';

        if (top + popRect.height > viewportHeight - VIEWPORT_MARGIN) {
            var above = linkRect.top - LINK_GAP - popRect.height;

            if (above >= VIEWPORT_MARGIN) {
                top = above;
                placement = 'above';
            } else {
                top = Math.max(VIEWPORT_MARGIN, viewportHeight - VIEWPORT_MARGIN - popRect.height);
            }
        }

        if (top < VIEWPORT_MARGIN) {
            top = VIEWPORT_MARGIN;
        }

        var left = linkRect.left + ((linkRect.width - popRect.width) / 2);
        var maxLeft = viewportWidth - VIEWPORT_MARGIN - popRect.width;

        if (left < VIEWPORT_MARGIN) {
            left = VIEWPORT_MARGIN;
        } else if (left > maxLeft) {
            left = Math.max(VIEWPORT_MARGIN, maxLeft);
        }

        root.style.top = Math.round(top) + 'px';
        root.style.left = Math.round(left) + 'px';
        root.setAttribute('data-placement', placement);
    }

    function onPointerOver(event) {
        if (event.pointerType === 'touch' || !canHover()) {
            hideNow();
            return;
        }

        if (root && root.contains(event.target)) {
            cancelHide();
            return;
        }

        var link = linkFromTarget(event.target);
        if (link) {
            if (event.relatedTarget && link.contains(event.relatedTarget)) {
                return;
            }

            cancelHide();
            scheduleShow(link);
            return;
        }

        if (visible) {
            scheduleHide();
        }
    }

    function onPointerOut(event) {
        if (event.pointerType === 'touch') {
            hideNow();
            return;
        }

        var related = event.relatedTarget;

        if (root && root.contains(event.target)) {
            if (!related || !root.contains(related)) {
                scheduleHide();
            }
            return;
        }

        var link = linkFromTarget(event.target);
        if (!link) {
            return;
        }

        if (related && link.contains(related)) {
            return;
        }

        cancelShow();

        if (root && related && root.contains(related)) {
            cancelHide();
        } else {
            scheduleHide();
        }
    }

    function onFocusIn(event) {
        if (!canHover()) {
            return;
        }

        var link = linkFromTarget(event.target);
        if (link) {
            cancelHide();
            scheduleShow(link);
        }
    }

    function onFocusOut(event) {
        var link = linkFromTarget(event.target);
        if (!link) {
            return;
        }

        if (!event.relatedTarget || !link.contains(event.relatedTarget)) {
            cancelShow();
            scheduleHide();
        }
    }

    function onKeyDown(event) {
        if (event.key === 'Escape' || event.key === 'Esc') {
            hideNow();
        }
    }

    function onScroll(event) {
        if (!visible) {
            return;
        }

        if (root && isNode(event.target) && root.contains(event.target)) {
            return;
        }

        hideNow();
    }

    function onNoteSaved() {
        cache.clear();
    }

    function bind() {
        document.addEventListener('pointerover', onPointerOver, true);
        document.addEventListener('pointerout', onPointerOut, true);
        document.addEventListener('focusin', onFocusIn, true);
        document.addEventListener('focusout', onFocusOut, true);
        document.addEventListener('keydown', onKeyDown, true);
        document.addEventListener('click', hideNow, true);
        document.addEventListener('scroll', onScroll, true);
        document.addEventListener('zcast:note-saved', onNoteSaved);
        window.addEventListener('blur', hideNow);
        window.addEventListener('resize', hideNow);

        if (hoverMedia) {
            if (typeof hoverMedia.addEventListener === 'function') {
                hoverMedia.addEventListener('change', function (event) {
                    if (event.matches) {
                        hideNow();
                    }
                });
            } else if (typeof hoverMedia.addListener === 'function') {
                hoverMedia.addListener(function (event) {
                    if (event.matches) {
                        hideNow();
                    }
                });
            }
        }
    }

    function init() {
        if (!canHover() || !document.body) {
            return;
        }

        buildRoot();
        bind();
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init, { once: true });
    } else {
        init();
    }
})();
