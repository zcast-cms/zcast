# `zcast`

A fast, self-hosted, single-author Zettelkasten CMS built on a **contract-oriented Rust microkernel**.

Every major capability—storage, `[[wikilink]]` graph, theme/reader, inline editor, hover popovers, full-text search, `⌘K` spotlight bar, and authentication—lives behind versioned Rust traits in [`zcast-interfaces`](https://github.com/zcast-cms/zcast-interfaces).

## Highlights (`v0.2.0`)

- **Sliding-pane reader:** Click any `[[wikilink]]` to open the linked card to the right, trace your trail via breadcrumbs, close non-root panes, and share exact stacks via `/<root>?stack=a,b,c`.
- **In-place TipTap editor:** Edit titles and Markdown bodies directly on any card with a floating formatting toolbar, `[[` live autocomplete, `Cmd/Ctrl+Click` + `Open ↗` link navigation while editing, debounced autosave + `Cmd+S`, and instant cross-pane backlink & rename updates.
- **Hover preview popovers (`zcast-popover`):** Subtle hover cards on `[[wikilinks]]` with 150ms hover-intent delay, client caching, and automatic touch-device suppression.
- **Instant `⌘K` / `Ctrl+K` search (`zcast-search-fts` + `zcast-cmd-k`):** Pure-Rust prefix + BM25 + link-graph-boosted search with live index updates and admin quick-draft creation.
- **Basic production auth (`zcast-auth-basic`):** Argon2id password hashing and SHA-256-hashed session cookies stored in DB-agnostic `ModuleStore` (zero raw SQL), unlocking safe public VPS hosting.
- **Strict privacy gate:** Marking a note **Private** hides it across pages, rendered `[[wikilinks]]`, backlinks, hover popovers, search hits, and API responses for unauthenticated visitors (test anytime with `?preview=public`).
- **Editorial dark/light/auto theme:** Self-hosted `Libre Franklin` typography, zero-flash colour-scheme locking, `--zcast-*` design tokens, and optional `custom.css` overrides.

## Quick Start

```bash
# 1. Local personal Zettelkasten (no auth, binds to 127.0.0.1:3000)
cargo run -p zcast-cli -- serve

# 2. Authenticated mode (prints a one-time generated admin password on first boot)
cargo run -p zcast-cli -- serve --auth basic

# 3. Inspect compiled modules and active interface bindings
cargo run -p zcast-cli -- module list
```

Then open [http://127.0.0.1:3000](http://127.0.0.1:3000). See the full **[Quick Start & User Guide](docs/quick-start.md)** and **[Configuration & Reference](docs/configuration.md)** for deployment, shortcuts, `zcast.toml`, and reverse-proxy setup.

## Bundled Modules (`v0.2.0`)

| Crate | Interface | Role |
|-------|-----------|------|
| `zcast-storage-sqlite` | `zcast:storage/repository@1.0.0` | SQLite (WAL) note persistence, aliases, and `ModuleStore` |
| `zcast-links-wikilink` | `zcast:links/parser@1.0.0` | `[[wikilink]]` parsing, GFM rendering, backlinks, atomic rename rewriting |
| `zcast-theme-davidhewitson` | `zcast:ui/theme@1.0.0` | Dark/light/auto editorial theme, sliding panes, breadcrumbs |
| `zcast-editor-inline` | `zcast:ui/editor@1.0.0` | Pre-bundled TipTap/ProseMirror inline editor (zero Node needed at Rust build time) |
| `zcast-auth-basic` | `zcast:auth/provider@1.0.0` | Optional Argon2id username/password login & cookie sessions |
| `zcast-popover` | `zcast:ui/popover@1.0.0` | Hover link preview popovers (`/_zcast/popover/:slug`) |
| `zcast-search-fts` | `zcast:search/provider@1.0.0` | Pure-Rust instant full-text + graph-boosted search |
| `zcast-cmd-k` | `zcast:ui/cmd-k@1.0.0` | Floating `⌘K` spotlight search modal (requires `zcast:search/provider@^1.0`) |

## Repositories in `zcast-cms`

- [`zcast-interfaces`](https://github.com/zcast-cms/zcast-interfaces) — Versioned Rust contracts and the minimal `Note` primitive.
- [`zcast`](https://github.com/zcast-cms/zcast) — Microkernel, CLI, docs, and first-party standard modules.
- [`zcast-theme-davidhewitson`](https://github.com/zcast-cms/zcast-theme-davidhewitson) — Editorial dark/light/auto sliding-panes theme inspired by `davidhewitson.co.uk`.

## Documentation

- [Quick Start & User Guide](docs/quick-start.md) — Running locally, authoring workflow, keyboard shortcuts, and basic VPS deployment.
- [Configuration & Reference](docs/configuration.md) — `zcast.toml`, `ZCAST_*` env vars, CLI flags, `custom.css`, security model, and HTTP routes.
- [Architecture Blueprint & Decision Log](docs/architecture-planning.md) — Microkernel architecture, DEC-01..DEC-20, and phased roadmap.
- [Theme System Design](docs/themes-design.md) — Three-layer CSS tokens, `ThemeProvider`, UI slots, and `data-zcast-*` DOM contracts.
- [Contract Sketches](docs/contracts-sketch.md) — Trait design notes for `Storage`, `ModuleStore`, and `SearchProvider`.
- [Installer & Updater UX](docs/installer-ux.md) — Planned `v0.4` one-line installer, `zcast setup` TUI, and rollback updater spec.
- [Feature Wishlist](docs/wishlist.md) — Future ideas (full-screen editor, revisions, orphans, note index, REST/MCP/CLI agent access).
