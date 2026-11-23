# `zcast` — Architecture Blueprint & Decision Record (`notes.davidhewitson.co.uk`)

> **Status:** Architecture Decisions Locked (Ready for Detailed Design / Implementation Planning)
> **Last Updated:** 2026-11-23

---

## 1. Product Vision & Experience

**`zcast`** is a fast, self-hosted, single-author Zettelkasten CMS (first deployed at `notes.davidhewitson.co.uk`), architected as a **contract-oriented Rust microkernel** with compile-time crate modularity (`xcaddy`-style).

- **Public Reading Experience:**
  - Default theme matched to the editorial typography of `davidhewitson.co.uk`.
  - Pinned **"Index / Start Here"** note at `/` (`notes.davidhewitson.co.uk/`).
  - Hovering over a `[[link]]` shows a cached preview popover (title + excerpt).
  - Clicking a `[[link]]` opens the linked card to the right and scrolls horizontally (stacked/sliding panes), synced to the URL (`/card-a?stack=card-b,card-c`) with breadcrumb navigation, right-truncation when branching from a middle pane, and scroll-to-existing deduplication.
  - Each card displays **Backlinks ("Linked from")** at the bottom via the `link-parser` graph contract.
  - Public reader mode treats links to non-existent or `private = true` notes identically: rendered as plain text with zero preview, backlink, search, or API leakage.
- **In-Place Authoring Experience:**
  - Painless toggle into an in-place ProseMirror/TipTap rich editor (`zcast-editor-inline`) with a floating selection toolbar (bold, italic, underline as `<u>...</u>`, lists, headings, `[[link]]` autocomplete).
  - Debounced autosave + `Cmd+S` with clear saved/saving state.
  - Clicking an unresolved `[[new card]]` link while editing opens a fresh draft pane to the right with that title pre-filled (persisted once authored).
  - Canonical Markdown storage with full export to a directory of Markdown files preserving inter-note links.
- **Modular Discovery & Ecosystem (`zcast` CLI):**
  - `Cmd+K` spotlight bar (`zcast-cmd-k`) is itself a module declaring a dependency on `search@^1.0` (optionally recommending a semantic provider).
  - Admin can inspect installed modules, view the interface dependency graph, and switch active bindings among compiled-in providers.
  - Adding/removing third-party modules happens via `zcast module install | update | remove`, which regenerates a thin shim crate, compiles a native optimized binary, verifies interface bindings, and atomically swaps + restarts the server.
  - Initial `curl | sh` installation downloads a **prebuilt default binary** (zero Rust toolchain required until compiling custom modules).

---

## 2. Locked Architecture Decision Log

| ID | Topic | Status | Locked Decision |
|----|-------|--------|-----------------|
| DEC-01 | Search & Topical Matching | **Decided** | `Cmd+K` is a module requiring versioned `search@^1.0` (with optional `recommends` hints). Multiple swappable search providers (keyword FTS, local ONNX embeddings, external APIs) implement the `search` contract. |
| DEC-02 | Note Identity & `[[link]]` Resolution | **Decided** | Title/slug-based (`[[Card Title]]`) with automatic Markdown link rewriting + slug redirect aliases on rename. |
| DEC-03 | Core vs. Extension Scope | **Decided** | `Note` (with `private: bool`) is the minimal core primitive. Two-way Markdown sync, revision history, and image/media uploads are extensions. |
| DEC-04 | Database Architecture | **Decided** | Storage is a required singleton `storage` contract implemented by modules (`zcast-storage-sqlite` default, `zcast-storage-mysql` optional). |
| DEC-05 | Module Runtime & Build Model | **Decided** | `xcaddy`-style Rust crate compilation via `zcast` CLI. `curl \| sh` fetches a prebuilt default binary; custom module installs recompile natively on-server (with warm `target/` cache) or locally/in CI. |
| DEC-06 | Core `Note` Primitive Schema | **Decided** | `id` (ULID), `slug`, `title`, `markdown_body`, `private`, `aliases`, namespaced `ext` JSON metadata map, `created_at`, `updated_at`. |
| DEC-07 | Frontend UI Extensibility | **Decided** | Crate-embedded static assets (`rust-embed`) registering into declared kernel frontend UI slots (`command-bar`, `editor-toolbar`, `card-footer`, `admin-panel`, etc.). |
| DEC-08 | Kernel vs. Swappable Modules | **Decided** | True microkernel: `auth`, `io/exporter`, `theme/reader`, `editor`, and `link-parser` are all swappable modules behind `zcast-interfaces`, with default implementations shipped in the standard bundle. |
| DEC-09 | Module Persistence | **Decided** | **Hybrid DB-agnostic persistence:** `storage` exposes a namespaced KV/document `ModuleStore` (for sessions, tokens, link graph, revisions) + kernel grants each module an isolated disk data directory (for search indexes, media uploads). Modules never write DB-dialect SQL. |
| DEC-10 | Hosting, TLS & Auth Optionality | **Decided** | Hosted behind an existing reverse proxy (auto-detected Caddy/Nginx/Apache vhost setup) as a `systemd` service. `auth` is an **optional** slot so `zcast` runs locally with zero auth; Express production setup defaults to `zcast-auth-basic` (generated login shown on completion), with `zcast-auth-magic-link` selectable in Advanced setup or swapped in via `zcast module`. |
| DEC-11 | Reader UX & Sliding Panes | **Decided** | URL-synced stack (`/card-a?stack=card-b,card-c`), right-truncation on middle-pane clicks, scroll-to-existing dedup, card Backlinks via `link-parser`, pinned Index/Start Here homepage. Hover popovers live in their own `zcast-popover` module. |
| DEC-12 | Default In-Place Editor | **Decided** | ProseMirror/TipTap inline editor (`zcast-editor-inline`) with strict Markdown + `<u>` + `[[wikilink]]` schema, debounced autosave + `Cmd+S`, and unresolved-link clicks opening a fresh right-hand draft pane. |
| DEC-13 | Installer Target Platforms | **Decided** | Prebuilt static Linux `musl` (`x86_64`, `aarch64`) for VPS production + macOS Apple Silicon (`aarch64-apple-darwin`) for local authoring. (See Section 6.) |
| DEC-14 | Theme System Design | **Decided** | Three-layer theme architecture: CSS Design Tokens (`--zcast-*`, supporting **`dark`-only, `light`-only, or `auto`** via SSR `data-theme`), `ThemeProvider` returning plain `Html(String)` (first-party uses `Maud`), UI Slots & `data-zcast-*` conventions, plus optional `custom.css`. See [themes-design.md](./themes-design.md). |
| DEC-15 | Configuration & Secrets | **Decided** | Split config: boot/infra & secrets in `zcast.toml` + `ZCAST_*` env vars (bind addr, DB URL, SMTP); content/app settings (site title, pinned homepage, active bindings) in `ModuleStore` so Admin UI can edit them live. |
| DEC-16 | Hook Order, URL Namespace & Title Uniqueness | **Decided** | Stage/priority buckets (`Early`, `Normal`, `Late` + registration order) for hooks; reserved `/_zcast/*` URL prefix for kernel/module routes; case-insensitive unique note titles to prevent ambiguous `[[links]]`. |
| DEC-17 | Frontend Asset Bundling | **Decided** | Pre-build & package minified JS/CSS bundles inside crates like `zcast-editor-inline` (`rust-embed`); Node/esbuild is only needed when developing editor JS source, keeping `cargo build` and `zcast module install` 100% pure Rust with zero Node dependency. |
| DEC-18 | HTML Render Caching | **Decided** | In-memory render cache in `zcast-kernel` keyed by `(note_id, is_admin)`, invalidated on note writes, renames, and visibility changes (ensuring `[[link]]` rendering updates immediately when targets appear or change privacy). |
| DEC-19 | Prototype Scope (`v0.1`) | **Decided** | **v0.1 Bare Minimum Reader/Writer:** `zcast-interfaces`, `zcast-kernel` (local no-auth mode), `zcast-storage-sqlite`, `zcast-links-wikilink`, `zcast-theme-davidhewitson`, and `zcast-editor-inline`. Defer `zcast-popover`, `zcast-search-*`, `zcast-cmd-k`, `zcast-auth-*`, `zcast-io-markdown`, MySQL, and CLI recompiler/installer to `v0.2+`. |
| DEC-20 | Contract & Version Pinning | **Decided** | Modules pin to SemVer-versioned contracts in `zcast-interfaces` (never to `zcast-kernel`), with additive minor versions and optional `v1`/`v2` submodules for non-breaking transitions. Site admins pin core + module versions in `zcast.toml` backed by a persisted `Cargo.lock`. |

---

## 3. System Architecture & Contracts

```text
┌────────────────────────────────────────────────────────────────────┐
│  `zcast` CLI (`xcaddy`-style)                                      │
│  • `curl | sh` -> prebuilt standard binary (no Rust required)      │
│  • `zcast setup` / `zcast serve` / `zcast update`                  │
│  • `zcast module list | graph | install | update | remove | bind`  │
│  • `zcast auth login-link` (provider CLI hooks)                    │
└─────────────────────────────────┬──────────────────────────────────┘
                                  │
┌─────────────────────────────────▼──────────────────────────────────┐
│                        `zcast-kernel`                              │
│  • Core `Note` Primitive & `private` Visibility Gate               │
│  • Module Registry, SemVer Interface Resolver & Slot Bindings      │
│  • Hook (Filter) & Action (Event) Bus                              │
│  • Axum HTTP Router, `rust-embed` Asset Server & Frontend UI Slots │
├──────────────┬──────────────┬───────────────┬──────────────────────┤
│ storage (1)  │ auth (0..1)  │ link-parser   │ search (0..1)        │
│ • sqlite     │ • magic-link │ • wikilink    │ • fts / onnx / api   │
│ • mysql      │ • basic      │   (+ graph &  ├──────────────────────┤
│ (+ ModuleKV) │ • (none=loc) │   backlinks)  │ ui & io (0..n)       │
├──────────────┴──────────────┴───────────────┤ • cmd-k (req search) │
│ editor (0..1)          theme/reader (1)     │ • io-markdown        │
│ • inline-prosemirror   • davidhewitson      │ • (revisions, media, │
│   (lazy-loaded)          (sliding panes)    │    2way-sync, ...)   │
└─────────────────────────────────────────────┴──────────────────────┘
```

### Core `Note` Primitive (`zcast-interfaces`)

```rust
pub struct Note {
    pub id: NoteId,                               // ULID (time-sortable, URL-safe, immutable)
    pub slug: String,                             // Current canonical URL slug
    pub title: String,                            // Human-readable title
    pub markdown_body: String,                    // Canonical Markdown source
    pub private: bool,                            // Enforced at kernel read boundary
    pub aliases: Vec<NoteAlias>,                  // Historical slugs/titles for redirects & link resolution
    pub ext: BTreeMap<String, serde_json::Value>, // Namespaced per-module metadata (e.g. ext["zcast-tags"])
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

### Core Interface Contracts (`zcast-interfaces`)

| Interface | Slot Cardinality | Core Responsibilities |
|-----------|------------------|-----------------------|
| `Storage` | Required (`1`) | CRUD/queries for `Note` & `aliases`, transactional rename updates, plus a DB-agnostic namespaced `ModuleStore` (KV/JSON documents with TTL & prefix queries). |
| `AuthProvider` | Optional (`0..1`) | Request authentication, session validation, login/logout routes, optional CLI hooks (e.g. `login-link`). |
| `LinkParser` | Required (`1`) | Parse outgoing `[[links]]` from Markdown, maintain forward/backlink edges (in `ModuleStore`), resolve targets respecting `private`, rewrite links on rename, return backlinks. |
| `SearchProvider` | Optional (`0..1`) | Index note create/update/delete events, execute instant keyword/topical queries respecting `private` visibility. |
| `EditorProvider` | Optional (`0..1`) | Provide lazy-loaded edit-mode JS/CSS assets + save/autocomplete endpoints; expose `editor-toolbar` UI slot for other modules. |
| `ThemeProvider` | Required (`1`) | Render SSR public HTML shells, sliding-pane reader chrome, hover popovers, breadcrumbs, backlinks, and host declared UI slots. |
| `Exporter` / `Importer` | Multi (`0..n`) | Export notes (and links/frontmatter) to external formats (default: directory/zip of `.md` files) and import back. |
| `Hook` / `Action` | Multi (`0..n`) | Pipeline filters (`before_note_save`, `on_html_render`, `on_popover_render`) and lifecycle events (`after_note_save`, `after_note_rename`, `after_note_delete`). |

---

## 4. Key Architectural Guardrails

1. **Unauthenticated Local-Only Safety Guard:** Because `auth` is optional (allowing frictionless local use without an auth module), if no `auth` provider is bound, `zcast serve` binds strictly to `127.0.0.1` and refuses to bind to `0.0.0.0` (public interfaces) unless an explicit override flag is passed.
2. **Visibility Gate at the Kernel Boundary:** Every public read path (HTML render, hover popover, backlinks, search queries, API responses) passes through a kernel `ReadContext { is_admin: bool }` so `private = true` notes never leak through any module.
3. **Magic-Link Scanner Protection:** `zcast-auth-magic-link` must never consume one-time tokens on `GET` (preventing email security scanners from burning links); `GET` renders a confirmation step that submits `POST` to consume the hashed, short-lived token.
4. **Hot-Path Performance & Cache Consistency:** In-memory render cache in `zcast-kernel` keyed by `(note_id, is_admin)`, invalidated on note writes/renames/visibility changes so `[[link]]` resolution never goes stale; public pages ship SSR HTML + minimal reader JS while `zcast-editor-inline` (pre-bundled JS/CSS via `rust-embed`, zero Node dependency at Rust build time) is lazy-loaded only for edit sessions.
5. **Deterministic Hooks & Clean Namespacing:** Hooks execute in deterministic stage/priority buckets (`Early` → `Normal` → `Late`, tie-broken by registration order); `/_zcast/*` is reserved for kernel/module APIs and static assets so note slugs never collide; note titles are case-insensitively unique to keep `[[Title]]` unambiguous.
6. **Safe `zcast` Rebuilds & Updates:** Pre-migration automatic SQLite snapshots, warm `target/` cache with configurable `CARGO_BUILD_JOBS`, optional local/CI cross-builds for low-RAM VPSes, and atomic binary swap only after build + interface validation pass.

---

## 5. Planned Workspace Structure

```text
dwh-zettelkasten/
├── docs/
│   ├── architecture-planning.md      # Master architecture blueprint & decision log
│   ├── themes-design.md              # Detailed theme, token, slot & layout specification
│   ├── contracts-sketch.md           # Rust contract sketches (Storage, SearchProvider, etc.)
│   └── installer-ux.md               # 30-second one-line installer & updater UX spec
├── crates/
│   ├── zcast-interfaces/             # Versioned traits, Note primitive, Hook/Action types
│   ├── zcast-kernel/                 # Core runtime, module registry, router, UI slot host
│   ├── zcast-cli/                    # `zcast` CLI (setup, serve, module install/build, update)
│   ├── zcast-storage-sqlite/         # Default Storage provider (WAL mode + ModuleStore)
│   ├── zcast-storage-mysql/          # Optional MySQL Storage provider (+ ModuleStore)
│   ├── zcast-links-wikilink/         # Default [[link]] parser, graph, backlinks, rename rewriter
│   ├── zcast-popover/                # Hover link preview popover module (uses data-zcast-note)
│   ├── zcast-auth-magic-link/        # One-time magic-link auth provider (+ CLI login-link)
│   ├── zcast-auth-basic/             # Optional simple password/basic auth provider
│   ├── zcast-editor-inline/          # ProseMirror/TipTap in-place editor + floating toolbar
│   ├── zcast-theme-davidhewitson/    # Editorial typography skin + sliding panes
│   ├── zcast-search-fts/             # Lightweight pure-Rust FTS + graph-boosted search
│   ├── zcast-search-onnx/            # Optional local semantic embeddings hybrid search
│   ├── zcast-cmd-k/                  # Spotlight command bar module (requires search@^1.0)
│   └── zcast-io-markdown/            # Markdown directory/zip exporter & importer
└── installer/
    └── install.sh                    # Minimal curl|sh downloader -> hands off to `zcast setup`
```

---

## 6. Installer Target Matrix (DEC-13)

| Platform | Rust Target Triple | Purpose | Service Integration |
|----------|--------------------|---------|---------------------|
| Linux x86_64 (VPS) | `x86_64-unknown-linux-musl` | Primary production servers (Ubuntu, Debian, Alpine, Fedora — zero glibc mismatch) | Optional `systemd` unit via `zcast setup` |
| Linux ARM64 (VPS/Pi) | `aarch64-unknown-linux-musl` | ARM VPSes (Hetzner CAX, Graviton, Raspberry Pi) | Optional `systemd` unit via `zcast setup` |
| macOS Apple Silicon | `aarch64-apple-darwin` | Local personal Zettelkasten & development | Foreground `zcast serve` (optional `launchd`) |
| macOS Intel *(optional)* | `x86_64-apple-darwin` | Older Macs if needed | Foreground `zcast serve` |

> **Build-chain note:** Static `musl` binaries work cleanly for pure-Rust modules (including `zcast-storage-sqlite`, `zcast-storage-mysql`, and `zcast-search-fts`). Modules wrapping C++ native runtimes like `zcast-search-onnx` (`ort`) either target `gnu` Linux or load `libonnxruntime.so` dynamically—so keeping the default prebuilt standard binary pure-Rust (`zcast-search-fts`) guarantees rock-solid static `musl` portability.

---

## 7. Phased Roadmap (Locked in DEC-19)

1. **`v0.1` — Bare Minimum Reader/Writer Prototype (Local-Only):**
   - `zcast-interfaces` + `zcast-kernel` (module registry, slot bindings, hook buckets, `ReadContext` gate, in-memory render cache, local-only no-auth guard, split config).
   - `zcast-storage-sqlite` (`Note` CRUD, atomic rename, DB-agnostic `ModuleStore`).
   - `zcast-links-wikilink` (`[[link]]` parsing, unresolved/private hiding, backlinks, rename rewriter).
   - `zcast-theme-davidhewitson` (`Maud` SSR templates, light/dark `--zcast-*` tokens matched to `davidhewitson.co.uk`, pinned Index homepage, URL-synced sliding panes, breadcrumbs, optional `custom.css`).
   - `zcast-editor-inline` (pre-bundled ProseMirror/TipTap floating toolbar, strict Markdown + `<u>` + `[[wikilink]]` round-trip, debounced autosave + `Cmd+S`, right-hand stub pane creation).
2. **`v0.2` — Reader/Discovery Modules (Validates Pure Slot Extensibility):**
   - `zcast-popover` (hover preview popovers via `data-zcast-note`).
   - `zcast-search-fts` + `zcast-cmd-k` (instant spotlight command bar).
   - `zcast-io-markdown` (full Markdown directory/zip export & import).
3. **`v0.3` — Production Auth, Semantic Search & MySQL:**
   - `zcast-auth-magic-link` (POST-confirmed tokens + `zcast auth login-link` SSH recovery) & `zcast-auth-basic`.
   - `zcast-search-onnx` (local semantic topical search) & `zcast-storage-mysql`.
4. **`v0.4` — `zcast` Ops, CLI Recompiler & Distribution:**
   - `zcast module list | graph | install | update | remove | bind` shim generator & native rebuilder.
   - Admin Module Dependency Graph UI.
   - `zcast setup` TUI, `zcast update`, and `installer/install.sh` (static Linux `musl` x86_64/ARM64 + macOS Apple Silicon releases).
