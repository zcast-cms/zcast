# `zcast` Configuration & Reference (`v0.2.0`)

---

## 1. Configuration Precedence

`zcast` loads configuration in the following order (later sources override earlier ones):

1. Built-in defaults
2. Optional `zcast.toml` file passed via `--config /path/to/zcast.toml`
3. `ZCAST_*` environment variables
4. CLI flags on `zcast serve`

---

## 2. `zcast.toml` Reference

```toml
[zcast]
# Displayed in the header, page <title>, and login screen
site_title = "David's Notes"

# Address and port to listen on
bind_addr = "127.0.0.1:3000"

# SQLite connection string (?mode=rwc creates the file if missing)
database_url = "sqlite://zcast.db?mode=rwc"

# Theme colour scheme: "auto" (system + toggle), "dark" (locked), or "light" (locked)
color_mode = "auto"

# Slug of the note rendered at "/"
pinned_home_slug = "index"

# Optional path to a custom CSS override file served at /_zcast/custom.css
# custom_css_path = "/var/lib/zcast/custom.css"

# Auth provider: "none" (local-only) or "basic"
# auth = "basic"

# Admin username when auth = "basic"
admin_user = "admin"

# Optional plaintext password (prefer ZCAST_ADMIN_PASSWORD env var in production)
# admin_password = "change-me"

# Dangerous escape hatch: allow binding non-loopback without auth (default: false)
allow_unauthenticated_public = false
```

Run with a config file:

```bash
zcast --config ./zcast.toml serve
```

---

## 3. Environment Variables & CLI Flags

| Setting | `zcast.toml` key | Environment Variable | CLI Flag (`zcast serve`) | Default |
|---------|------------------|----------------------|--------------------------|---------|
| Config file | — | — | `--config <PATH>` (global) | *(none)* |
| Site title | `site_title` | `ZCAST_SITE_TITLE` | `--title <TITLE>` | `"Zcast"` |
| Bind address | `bind_addr` | `ZCAST_BIND_ADDR` | `--bind <HOST:PORT>` | `"127.0.0.1:3000"` |
| Database URL | `database_url` | `ZCAST_DATABASE_URL` | `--db <URL>` | `"sqlite://zcast.db?mode=rwc"` |
| Theme mode | `color_mode` | `ZCAST_THEME_MODE` | `--theme-mode <auto\|dark\|light>` | `"auto"` |
| Pinned home slug | `pinned_home_slug` | — | — | `"index"` |
| Custom CSS | `custom_css_path` | `ZCAST_CUSTOM_CSS` | — | *(none)* |
| Auth provider | `auth` | `ZCAST_AUTH` | `--auth <none\|basic>` | `"none"` *(or `"basic"` if password set)* |
| Admin username | `admin_user` | `ZCAST_ADMIN_USER` | `--admin-user <USER>` | `"admin"` |
| Admin password | `admin_password` | `ZCAST_ADMIN_PASSWORD` | `--admin-password <PASS>` | *(generated once on first `--auth basic` boot)* |
| Allow public no-auth | `allow_unauthenticated_public` | — | `--allow-unauthenticated-public` | `false` |

> **Logging verbosity:** Set `RUST_LOG` (e.g. `RUST_LOG=debug` or `RUST_LOG=tower_http=debug,zcast_kernel=info`). Defaults to `info`.

---

## 4. Theming & `custom.css` Overrides

`zcast-theme-davidhewitson` supports three colour modes:
- **`dark`** — Server-renders `<html data-theme="dark">` with zero flash-of-wrong-theme and hides the theme toggle.
- **`light`** — Server-renders `<html data-theme="light">` with zero flash and hides the theme toggle.
- **`auto`** — Resolves `localStorage` or `prefers-color-scheme` in an inline `<head>` script before first paint, and displays a sun/moon toggle in the header.

### Customising without forking a theme
Point `custom_css_path` (or `ZCAST_CUSTOM_CSS`) at any `.css` file. `zcast` serves it at `/_zcast/custom.css` and loads it last in `<head>` so you can override any `--zcast-*` design token across the theme, editor, popovers, and `⌘K` bar:

```css
/* Example custom.css */
:root {
  --zcast-pane-width: 680px;
  --zcast-text-base: 1.125rem;
  --zcast-measure: 66ch;
}

:root[data-theme="dark"] {
  --zcast-bg: #16171a;
  --zcast-surface: #1f2126;
}
```

See [themes-design.md](themes-design.md) for the complete token and UI-slot contract.

---

## 5. Security & Privacy Model

- **Mandatory `ReadContext` gate:** Every storage query, link render, backlink lookup, hover popover, and search query receives `ReadContext { include_private }`. Unauthenticated visitors (and `?preview=public`) always execute with `include_private: false`, ensuring private notes never leak in HTML, backlinks, popovers, search hits, or link graph boosts.
- **Unauthenticated bind guard:** Refuses to bind to non-loopback interfaces (`0.0.0.0`, public IP) unless an `AuthProvider` is bound or `--allow-unauthenticated-public` is explicitly passed.
- **Password & session hardening (`zcast-auth-basic`):**
  - Passwords are stored exclusively as **Argon2id** PHC hashes in `ModuleStore`.
  - Session tokens are 32 random bytes from `OsRng`; only their **SHA-256** digest (`session:<hex>`) is persisted with a 7-day TTL.
  - Cookies are issued with `HttpOnly; SameSite=Lax; Path=/` (plus `Secure` when `X-Forwarded-Proto: https` is detected).
  - Login `next` redirect targets are strictly validated against open redirects (same-origin `/...` paths only).
- **Reserved namespace:** All system and module endpoints live under `/_zcast/*`, and `_zcast` is disallowed as a note slug.

---

## 6. HTTP Endpoints Reference (`v0.2.0`)

### Public Reader Pages
| Method & Path | Description |
|---------------|-------------|
| `GET /` | Render pinned home card (plus optional `?stack=slug1,slug2` & `?preview=public`) |
| `GET /{slug}` | Render note by canonical slug, or `308 Permanent Redirect` if matched via historical alias |

### System, Auth & Discovery (`/_zcast/*`)
| Method & Path | Auth | Description |
|---------------|------|-------------|
| `GET /_zcast/health` | Public | Health check JSON `{ "status": "ok", "version": "0.2.0" }` |
| `GET /_zcast/login` | Public | Render login form (when an `AuthProvider` is bound) |
| `POST /_zcast/login` | Public | Verify form credentials, set session cookie, redirect to sanitized `next` |
| `POST /_zcast/logout` | Public | Revoke session token in `ModuleStore`, clear cookie, redirect to `/` |
| `GET /_zcast/cards/{slug}` | Public / Admin* | JSON card fragment `{ card, html, canonical_slug }` (`Cache-Control: no-store`) |
| `GET /_zcast/popover/{slug}` | Public / Admin* | JSON preview `{ slug, canonical_slug, title, excerpt_html, backlink_count, updated_at }` (`Cache-Control: no-store`) |
| `GET /_zcast/search?q=&limit=&context=` | Public / Admin* | JSON search hits `{ query, capabilities, hits }` (`Cache-Control: no-store`) |
| `GET /_zcast/theme/{*path}` | Public | Embedded theme assets (`theme.css`, `panes.js`, `fonts/*.woff2`) |
| `GET /_zcast/editor/{*path}` | Public | Embedded inline editor assets (`editor.css`, `editor.bundle.js`) |
| `GET /_zcast/ext/{prefix}/{*path}` | Public | Embedded `UiExtension` assets (`popover/*`, `cmd-k/*`) |
| `GET /_zcast/custom.css` | Public | Optional admin CSS override file |

*\*Respects `ReadContext`: public callers only see public notes; authenticated admins see all notes (unless `?preview=public` is passed).*

### Admin JSON API (`/_zcast/api/*` — Admin Only)
| Method & Path | Description |
|---------------|-------------|
| `GET /_zcast/modules` | Active module manifests and interface slot bindings |
| `GET /_zcast/api/notes?q=` | List/filter notes (up to 25) for `[[wikilink]]` autocomplete |
| `POST /_zcast/api/notes` | Create a note (`{ title, markdown_body, private }`) |
| `GET /_zcast/api/notes/{id}` | Fetch raw note JSON including canonical Markdown |
| `PUT /_zcast/api/notes/{id}` | Update or atomically rename a note (rewriting citing `[[links]]`) |
| `DELETE /_zcast/api/notes/{id}` | Delete a note and clean up link graph + search index |

---

## 7. Developing & Testing

```bash
# Run all unit and integration tests across the workspace
cargo test --workspace

# Run Clippy with warnings denied
cargo clippy --workspace --all-targets -- -D warnings
```

### Rebuilding the inline editor bundle (only when editing editor JS source)
Normal Rust builds never need Node.js because `crates/zcast-editor-inline/assets/editor.bundle.js` is committed. If you modify `crates/zcast-editor-inline/frontend/src/editor.js`:

```bash
cd crates/zcast-editor-inline/frontend
npm install
npm run build
```
