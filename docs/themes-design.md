# `zcast` Theme System Design (`DEC-14`)

> **Status:** Design Proposal (Draft for review)
> **Goal:** Make themes easy to author and swap, match `davidhewitson.co.uk`'s typography cleanly, and guarantee that independent UI modules (`zcast-popover`, `zcast-cmd-k`, `zcast-editor-inline`) look native inside *any* theme without tight coupling.

---

## 1. Core Idea: Three Clear Layers

A `zcast` theme is a Rust crate implementing the `ThemeProvider` interface. To prevent themes and feature modules from breaking each other, theming is split into **three explicit contracts**:

1. **Design Tokens (CSS Custom Properties)** — how every module inherits the theme's typography, colors, spacing, and rhythm automatically.
2. **Templates & Layout (`ThemeProvider` trait)** — how pages, cards, and navigation chrome (such as sliding panes) are rendered server-side.
3. **UI Slots & `data-zcast-*` DOM Conventions** — guaranteed mount points and HTML attributes so modules like `zcast-popover`, `zcast-cmd-k`, and `zcast-editor-inline` attach cleanly regardless of theme markup.

---

## 2. Layer 1: Standard CSS Design Tokens (`--zcast-*`)

Every theme MUST define a canonical set of CSS custom properties on `:root` (and optionally `@media (prefers-color-scheme: dark)` or a `[data-theme="dark"]` override).

```css
:root {
  /* Typography (matched to davidhewitson.co.uk in the default theme) */
  --zcast-font-body: /* editorial body face, fallback serif/sans */;
  --zcast-font-heading: /* heading face */;
  --zcast-font-mono: ui-monospace, SFMono-Regular, Menlo, monospace;
  --zcast-text-base: 1.0625rem;
  --zcast-leading-body: 1.7;
  --zcast-measure: 62ch;           /* Comfortable reading line length per pane */

  /* Color Palette */
  --zcast-bg: #ffffff;
  --zcast-surface: #f9f9f8;        /* Popovers, command bar, floating toolbar */
  --zcast-text: #1a1a1a;
  --zcast-text-muted: #666666;
  --zcast-border: #e5e5e3;
  --zcast-accent: #111111;         /* Links, focus rings, active breadcrumb */
  --zcast-accent-subtle: rgba(17, 17, 17, 0.06);

  /* Chrome & Elevation */
  --zcast-radius: 6px;
  --zcast-shadow-popover: 0 8px 28px rgba(0, 0, 0, 0.12);
  --zcast-pane-width: 620px;       /* Width of each sliding pane on desktop */
  --zcast-z-popover: 40;
  --zcast-z-toolbar: 50;
  --zcast-z-modal: 60;             /* Cmd+K spotlight overlay */
}
```

### Why this matters
- **`zcast-popover`** styles its preview card with `var(--zcast-surface)`, `var(--zcast-font-body)`, `var(--zcast-shadow-popover)`, and `var(--zcast-border)`.
- **`zcast-cmd-k`** styles its floating command bar with the exact same tokens.
- **`zcast-editor-inline`** styles its floating formatting toolbar with the exact same tokens.
- Result: **zero visual jarring** when installing new modules, and swapping themes instantly restyles the entire UI.
- Theme crates embed self-hosted `.woff2` font files via `rust-embed` (served under `/_zcast/theme/assets/...` with immutable cache headers) so typography loads instantly with zero external CDN dependencies.

---

## 3. Layer 2: Templates, Layouts & Theme Overrides

### Compile-time SSR Templates
- Themes render HTML via compiled Rust templates (e.g., `Askama` or `Maud`)—sub-millisecond rendering and compile-time safety.
- A theme implements a small set of well-defined view methods:
  1. **`render_page(&self, ctx: PageContext) -> Html`** — Full SSR HTML document (`<!doctype html>`, `<head>`, theme CSS/fonts, breadcrumbs, initial card stack, and all UI slots).
  2. **`render_card(&self, card: CardView, ctx: RenderContext) -> Html`** — Renders a single card fragment (title, rendered body HTML, backlinks, per-card slots). Reused both inside `render_page` and by the sliding-panes endpoint (`GET /_zcast/cards/:slug`) when opening a new pane to the right!
  3. **`render_error(&self, status: u16, message: &str, ctx: RenderContext) -> Html`** — Branded 404/500 page.

### Sliding Panes: Theme-Owned Layout with a Shared Helper
- Different themes might want completely different reading layouts (e.g., `zcast-theme-davidhewitson` uses horizontal sliding panes + breadcrumbs, whereas another theme might be a classic single-column wiki or a book-style layout).
- Therefore, **layout belongs to the `ThemeProvider`**, while the sliding-pane stack/URL-sync JS can live in a small reusable crate (e.g., `zcast-layout-panes`) that `zcast-theme-davidhewitson` (and any future sliding-pane theme) embeds directly.

### User CSS Overrides (Without Forking a Theme!)
- Even with compiled theme crates, an admin often wants small tweaks (e.g., slightly larger body font or a custom accent color) without writing a new Rust crate.
- The kernel supports an optional `custom.css` file in the site's data directory (e.g., `/var/lib/zcast/custom.css`) injected last in the `head` slot so CSS variables or rules can be tweaked without recompiling.

---

## 4. Layer 3: UI Slots & DOM Conventions (How Modules Plug In)

### Required UI Slots
Every theme template renders kernel-supplied slot HTML at five standard locations:

| Slot Name | Placement | Typical Module Consumers |
|-----------|-----------|--------------------------|
| `head` | Inside `<head>` (after theme CSS) | Module stylesheets (`zcast-popover`, `zcast-cmd-k`), OpenGraph/SEO meta modules, `custom.css` |
| `header-actions` | Top bar / nav area | `zcast-cmd-k` search button (`⌘K`), `zcast-auth-*` login/logout link, admin dashboard link |
| `card-toolbar` | Top of each card (visible when `is_admin`) | `zcast-editor-inline` ("Edit" toggle), Private/Public toggle, future `zcast-revisions` ("History") |
| `card-footer` | Bottom of each card | Backlinks ("Linked from"), future `zcast-tags` pills, Webmentions |
| `body-end` | Just before `</body>` | Module JS (`zcast-popover`, `zcast-cmd-k`), floating modal roots, lazy editor bootstrap (admin only) |

### Standard `data-zcast-*` HTML Attributes
To decouple frontend behavior from theme CSS classes, themes and link parsers emit standard `data-zcast-*` attributes:

- **`<a href="/slug" data-zcast-note="slug">...</a>`**
  - `zcast-popover` listens via delegated `pointerenter`/`focusin` on `[data-zcast-note]` → fetches `/_zcast/popover/:slug` (cached in-memory with ~150ms hover-intent delay) and displays the preview popover.
  - Sliding-panes JS intercepts clicks on `[data-zcast-note]` inside a pane → fetches `/_zcast/cards/:slug` and opens/scrolls to the right-hand pane while updating `?stack=...`.
- **`<a data-zcast-unresolved="New Card Title">...</a>`** (emitted **only** when `is_admin = true`; for public readers, unresolved/private links render as plain `<span>` text with no attribute!)
  - In edit/admin mode, clicking opens a blank right-hand draft card pane pre-titled `"New Card Title"`.
- **`<article data-zcast-card data-zcast-card-id="<ulid>" data-zcast-card-slug="<slug>">`**
  - Contains `<div data-zcast-card-body>` so `zcast-editor-inline` knows the exact DOM region to swap into an active ProseMirror/TipTap surface on edit toggle.

---

## 5. How `zcast-popover` Works End-to-End (Concrete Example)

1. **`zcast-popover`** registers:
   - An HTTP route: `GET /_zcast/popover/:slug` → asks `zcast-kernel` for the note using `ReadContext` (so private notes return `404` for public visitors!), runs any `on_popover_render` hooks, and returns `{ title, excerpt_html, slug, updated_at }`.
   - Frontend assets (tiny ~2 KB vanilla JS + CSS using `--zcast-*` tokens) injected into the `head` and `body-end` slots.
2. When a reader hovers any `<a data-zcast-note="some-card">`:
   - After a 150ms hover-intent delay (cancels if mouse leaves early), JS checks its local `Map` cache, fetches `/_zcast/popover/some-card` if needed, positions the floating card smartly within the viewport (flipping above/below/left/right near edges), and fades it in.
   - Completely disabled on touch-only pointer devices (`@media (hover: none)`).
3. If `zcast-popover` is uninstalled or disabled via `zcast`, links still work seamlessly as normal sliding-pane links—zero broken code in the theme.

---

## 6. Locked Theme Decisions
1. **Color scheme mode (`dark` / `light` / `auto`) (Decided):** `zcast-theme-davidhewitson` ships both light and dark `--zcast-*` token sets and supports **locking the site to dark-only or light-only** (as well as `auto` following system preference + toggle). Server-rendered `<html data-theme="dark">` guarantees zero flash-of-wrong-theme—allowing `notes.davidhewitson.co.uk` to stay strictly in dark mode just like `davidhewitson.co.uk`.
2. **Template engine (Decided):** First-party theme & modules use **`Maud`** internally for terseness and agent ergonomics. **Not enforced on third-party crates:** `ThemeProvider` and UI slot contracts in `zcast-interfaces` return a plain `Html(pub String)` newtype (no `maud` dependency in `zcast-interfaces`), so third-party themes/modules can use `Maud`, `Askama`, `Tera`, or any HTML generator they prefer.
3. **Custom CSS escape hatch (Decided):** Optional `/var/lib/zcast/custom.css` override loaded last in the `head` slot for quick tweaks without recompiling.
