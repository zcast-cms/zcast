# `zcast`

A fast, self-hosted, single-author Zettelkasten CMS built on a **contract-oriented Rust microkernel**.

Every major capability—storage, `[[wikilink]]` graph, theme/reader, inline editor, hover popovers, full-text search, `⌘K` spotlight bar, and authentication—lives behind versioned Rust traits in [`zcast-interfaces`](https://github.com/zcast-cms/zcast-interfaces).

## Highlights (`v0.4.0`)

- **One-line installer & Express setup:** `curl -sSL https://raw.githubusercontent.com/zcast-cms/zcast/main/installer/install.sh | sh` installs a prebuilt static binary, verifies SHA-256, and runs `zcast setup` (with automatic Caddy / Nginx / Apache detection & `systemd` unit generation).
- **Sliding-pane reader:** Click any `[[wikilink]]` to open the linked card to the right, trace your trail via breadcrumbs, close non-root panes, and share exact stacks via `/<root>?stack=a,b,c`.
- **In-place TipTap editor:** Edit titles and Markdown bodies directly on any card with a floating formatting toolbar, `[[` live autocomplete, `Cmd/Ctrl+Click` + `Open ↗` link navigation while editing, debounced autosave + `Cmd+S`, and instant cross-pane backlink & rename updates.
- **Hover preview popovers (`zcast-popover`):** Subtle hover cards on `[[wikilinks]]` with 150ms hover-intent delay, client caching, and automatic touch-device suppression.
- **Swappable instant `⌘K` search (`zcast-search-fts` & `zcast-search-onnx` + `zcast-cmd-k`):** Choose pure-Rust full-text + graph-boosted search (`--search fts`) or local dense-vector semantic hybrid search (`--search onnx`).
- **Swappable auth (`zcast-auth-basic` & `zcast-auth-magic-link`):** Argon2id password login OR scanner-safe `GET` → `POST` one-time magic links with `zcast auth login-link` SSH recovery—both backed by DB-agnostic `ModuleStore`.
- **SQLite & MySQL storage (`zcast-storage-sqlite` & `zcast-storage-mysql`):** Pure-Rust backends (`rustls` for MySQL) supporting WAL mode, atomic renames, and `ModuleStore`.
- **Markdown export & import (`zcast-io-markdown`):** Full directory and `.zip` export/import preserving YAML frontmatter and `[[wikilinks]]` (via CLI `zcast export`/`import` or Admin UI).
- **Admin dashboard & `xcaddy`-style module CLI:** Inspect the live Module Dependency Graph & edit site settings live at `/_zcast/admin`, manage modules via `zcast module list | graph | bind | install | update | remove`, and upgrade safely via `zcast update` (automatic `VACUUM INTO` SQLite snapshot + SHA-256 verification + auto-rollback).

## Quick Start

```bash
# Option 1: One-line installer (downloads prebuilt release binary + runs `zcast setup`)
curl -sSL https://raw.githubusercontent.com/zcast-cms/zcast/main/installer/install.sh | sh

# Option 2: Run directly from source (local no-auth mode on 127.0.0.1:3000)
cargo run -p zcast-cli -- serve

# Option 3: Guided Express / Advanced setup wizard
cargo run -p zcast-cli -- setup

# Inspect the module dependency graph
cargo run -p zcast-cli -- module graph
```

Then open [http://127.0.0.1:3000](http://127.0.0.1:3000) (or `/_zcast/admin` for the Module Dependency Graph & live settings). See the full **[Quick Start & User Guide](docs/quick-start.md)** and **[Configuration & Reference](docs/configuration.md)**.

## Bundled Modules (`v0.4.0`)

| Crate | Interface | Role |
|-------|-----------|------|
| `zcast-storage-sqlite` | `zcast:storage/repository@1.0.0` | Default SQLite (WAL) note persistence, aliases, and `ModuleStore` |
| `zcast-storage-mysql` | `zcast:storage/repository@1.0.0` | Optional MySQL (`rustls`) persistence & `ModuleStore` |
| `zcast-links-wikilink` | `zcast:links/parser@1.0.0` | `[[wikilink]]` parsing, GFM rendering, backlinks, atomic rename rewriting |
| `zcast-theme-davidhewitson` | `zcast:ui/theme@1.0.0` | Dark/light/auto editorial theme, sliding panes, breadcrumbs |
| `zcast-editor-inline` | `zcast:ui/editor@1.0.0` | Pre-bundled TipTap/ProseMirror inline editor (zero Node needed at Rust build time) |
| `zcast-auth-basic` | `zcast:auth/provider@1.0.0` | Optional Argon2id username/password login & cookie sessions |
| `zcast-auth-magic-link` | `zcast:auth/provider@1.0.0` | Optional scanner-safe one-time magic links + `zcast auth login-link` |
| `zcast-popover` | `zcast:ui/popover@1.0.0` | Hover link preview popovers (`/_zcast/popover/:slug`) |
| `zcast-search-fts` | `zcast:search/provider@1.0.0` | Default pure-Rust instant full-text + graph-boosted search |
| `zcast-search-onnx` | `zcast:search/provider@1.0.0` | Optional local dense-vector semantic + lexical hybrid search |
| `zcast-cmd-k` | `zcast:ui/cmd-k@1.0.0` | Floating `⌘K` spotlight search modal (requires `zcast:search/provider@^1.0`) |
| `zcast-io-markdown` | `zcast:io/markdown@1.0.0` | Markdown directory & `.zip` export/import preserving `[[wikilinks]]` |

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
