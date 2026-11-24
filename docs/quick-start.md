# `zcast` Quick Start & User Guide (`v0.4.1`)

Get a working Zettelkasten running in seconds via the one-line installer—or build from source for local authoring or VPS deployment.

---

## 0. One-Line Install (Prebuilt Static Binaries)

```bash
curl -sSL https://raw.githubusercontent.com/zcast-cms/zcast/main/installer/install.sh | sh
```

The bootstrapper detects Linux (`x86_64`/`aarch64` static `musl`) or macOS (`aarch64`/`x86_64`), verifies `SHA256SUMS`, installs `zcast`, and launches the interactive **`zcast setup`** wizard (or runs `zcast update` automatically if `zcast` is already configured).

---

## 1. Building From Source (Prerequisites)

- **Rust toolchain** (edition 2024 / Rust 1.85+ via [rustup](https://rustup.rs))
- **No Node.js required:** `zcast-editor-inline` ships its TipTap bundle pre-compiled inside the crate, so `cargo build` and `cargo run` are 100% pure Rust.

Clone and enter the repository:

```bash
git clone https://github.com/zcast-cms/zcast.git
cd zcast
```

---

## 2. Run Locally (Personal / No-Auth Mode)

Start `zcast` with defaults:

```bash
cargo run -p zcast-cli -- serve
```

What happens on first launch:
1. Creates `./zcast.db` (SQLite in WAL mode) if it does not exist.
2. Seeds three starter notes so the site works immediately:
   - **`Index`** (pinned at `/`)
   - **`How Sliding Panes Work`**
   - **`Private Scratchpad`** (marked `private = true` so you can test visibility gating)
3. Indexes all notes in `zcast-search-fts`.
4. Starts the HTTP server on **`http://127.0.0.1:3000`** with implicit local admin access.

> **Local-only safety guard:** When no auth module is enabled, `zcast` refuses to bind to non-loopback addresses (such as `0.0.0.0`) unless you explicitly enable `--auth basic` or pass `--allow-unauthenticated-public`.

### Previewing what public visitors see
Append `?preview=public` to any URL—for example:

```text
http://127.0.0.1:3000/?preview=public
```

In public preview mode:
- Edit controls, `+ New Note`, and the TipTap bundle are omitted.
- Links to non-existent or `private` notes render as plain non-clickable text.
- Private notes return `404` on page loads and hover popovers, and are excluded from `⌘K` search and backlinks.

### Useful local flags

```bash
# Lock to dark mode with a custom site title and database path
cargo run -p zcast-cli -- serve \
  --title "David's Notes" \
  --theme-mode dark \
  --db "sqlite://my-notes.db?mode=rwc" \
  --bind 127.0.0.1:4000
```

To stop a backgrounded local instance on port `3000` (or another port):

```bash
./kill.sh        # stops listener on port 3000
./kill.sh 4000   # stops listener on port 4000
```

---

## 3. Run With Authentication (Public / VPS Mode)

Enable `zcast-auth-basic` to allow public reading while protecting editing and private notes behind a login screen:

```bash
# Option A: Let zcast generate a strong one-time admin password on first boot
cargo run -p zcast-cli -- serve --auth basic

# Option B: Supply credentials explicitly (or via ZCAST_ADMIN_USER / ZCAST_ADMIN_PASSWORD)
ZCAST_ADMIN_PASSWORD="choose-a-strong-password" \
  cargo run -p zcast-cli -- serve --auth basic --bind 127.0.0.1:8080
```

- Passwords are hashed with **Argon2id** and stored in SQLite via `ModuleStore`.
- On subsequent restarts without `--admin-password` / `ZCAST_ADMIN_PASSWORD`, `zcast` keeps your existing stored credentials intact.
- Visit **`/_zcast/login`** (or click **Sign in** in the header) to start a 7-day `HttpOnly; SameSite=Lax` session cookie. Click **Log out** to revoke the session immediately.

### Deploying behind a reverse proxy

On a fresh Linux VPS, running `zcast setup` **as root** now provisions the whole
host automatically:

1. Creates the unprivileged `zcast` system user and `/var/lib/zcast` (mode `0750`, owned `zcast:zcast`).
2. Secures `/etc/zcast/zcast.toml` as `root:zcast` mode `0640`.
3. Writes and starts a hardened `/etc/systemd/system/zcast.service` (`ProtectSystem=strict`, `NoNewPrivileges`, writable only under `/var/lib/zcast`).
4. Waits for `/_zcast/health` before reporting success.
5. Configures the detected reverse proxy idempotently:
   - **Caddy** — appends a site block to `/etc/caddy/Caddyfile` (never duplicated) and reloads Caddy; HTTPS is automatic.
   - **Nginx** — writes `/etc/nginx/sites-available/zcast-<domain>`, enables it, runs `nginx -t`, reloads Nginx, then runs Certbot (`--nginx --redirect`) when available.
   - **Apache** — writes `/etc/apache2/sites-available/zcast-<domain>.conf`, runs `a2ensite`, `apachectl configtest`, reloads Apache, then runs Certbot (`--apache`) when available.

If Certbot fails because DNS is not pointing at the host yet, setup still
succeeds and prints the exact command to re-run later. Use `zcast setup --no-apply`
to write the config and database only, and configure the service/proxy yourself.

To do the same steps by hand:

Build an optimised release binary:

```bash
cargo build --release -p zcast-cli
# Binary at: ./target/release/zcast
```

Run `zcast` bound to localhost (`127.0.0.1:8080`) with `--auth basic`, and terminate HTTPS in front of it:

**Caddy (`/etc/caddy/Caddyfile`):**
```caddyfile
notes.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

**Nginx:**
```nginx
server {
    server_name notes.example.com;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

> When `X-Forwarded-Proto: https` is present (sent automatically by Caddy and the Nginx snippet above), `zcast` automatically adds the `Secure` attribute to session cookies.

---

## 4. Reading & Authoring Cheat Sheet

### Sliding Panes & Navigation
- **Open a linked note:** Click any `[[wikilink]]` to slide the target card open to the right.
- **No duplicates:** Clicking a link to an already-open card scrolls to it and highlights it instead of opening a second copy.
- **Branching:** Clicking a link inside a middle pane replaces panes to its right and updates the URL (`/<root>?stack=second,third`).
- **Breadcrumbs:** Click any breadcrumb in the top bar to jump straight back to that pane.
- **Close a pane:** Click **`×`** in the top-right corner of any non-root card. (The leftmost root card stays anchored.)
- **Hover previews:** Pause your cursor over any resolved `[[wikilink]]` for 150ms to see a preview card (disabled automatically on touch devices).

### Instant Spotlight Search (`⌘K` / `Ctrl+K`)
- Press **`⌘K`** (macOS) or **`Ctrl+K`** (Linux/Windows)—or click **Search** in the header—from anywhere on the site.
- Start typing for instant prefix + full-text + link-graph-boosted matches.
- Use **`↑` / `↓`** and **`Enter`** to open a match in a sliding pane to the right, or **`Esc`** to close.
- In admin mode, if no note matches your exact query, select **`+ Create "<query>"`** at the bottom of the list to open a pre-titled draft pane immediately.

### In-Place Editing
- Click **Edit** in a card's toolbar to edit both the **title** and **body** directly on the card.
- **Formatting:** Select text to reveal the floating formatting toolbar (**B**, *I*, <u>U</u>, H2, H3, bullet/ordered lists, blockquote, inline code, `[[Link]]`).
- **Link autocomplete:** Type `[[` to search existing notes live (`↑`/`↓` + `Enter`/`Tab` to insert).
- **Navigate links while editing:**
  - **`Cmd+Click`** (macOS) or **`Ctrl+Click`** (Linux/Windows) any `[[wikilink]]` inside the editor to open it in a pane to the right (or open a new draft pane if it doesn't exist yet) without leaving edit mode.
  - Place your cursor inside a `[[wikilink]]` to reveal the compact **`Open ↗`** pill for single-click navigation.
- **Saving:** Edits autosave 800ms after you pause typing, or press **`Cmd+S` / `Ctrl+S`** to save immediately. Click **Done** to exit edit mode.
- **Creating new notes:** Click **`+ New Note`** in the header, or click any dotted unresolved `[[New Card]]` link while signed in. Clicking **Discard** (or closing an untouched blank draft) removes it cleanly without creating an empty note.
- **Renaming notes:** Edit a card's title and save. `zcast` atomically rewrites `[[Old Title]]` → `[[New Title]]` across citing notes, records a permanent `308` redirect alias from the old slug, and refreshes all other open panes in real time.
- **Live backlinks:** Adding or removing `[[links]]` immediately updates the **Linked from** section on every other open pane without refreshing the page.
- **Public / Private toggle:** Click **Public** / **Private** in the card toolbar to toggle note visibility immediately.

---

## 5. Operations, Modules, Export/Import & Updates (`v0.4.1`)

```bash
# Guided Express / Advanced setup wizard (or non-interactive --express / --local)
# As root on Linux this also installs systemd + vhost + TLS automatically.
zcast setup
zcast setup --express --domain notes.example.com
zcast setup --express --domain notes.example.com --no-apply   # config/db only
zcast setup --express --domain notes.example.com --print-systemd

# Inspect modules & dependency graph
zcast module list
zcast module graph
zcast module bind search zcast-search-onnx

# Temporarily disable / re-enable a bundled module (no rebuild needed)
zcast module deactivate zcast-popover
zcast module activate zcast-popover

# Install / remove / update third-party Rust module crates (xcaddy-style rebuild)
zcast module install zcast-tags@0.1.0 --dry-run

# Password rotation (revokes every active session unless --keep-sessions)
zcast auth set-password --generate
zcast auth set-password --user admin --password 's3cret' --keep-sessions

# Magic-link auth & SSH recovery
zcast serve --auth magic-link --admin-email you@example.com --public-url https://notes.example.com
zcast auth login-link --base-url https://notes.example.com

# Full Markdown backup / restore (preserving frontmatter & [[wikilinks]])
zcast export --out ./notes-backup
zcast export --zip ./notes-backup.zip
zcast import ./notes-backup.zip

# Safe update with automatic SQLite VACUUM INTO snapshot & rollback
zcast update --check
zcast update
```

> When `zcast` runs under systemd and you run `zcast update` as root, the updater
> restarts `zcast.service` after the binary swap, polls `/_zcast/health`, and
> automatically rolls back to the previous binary if the new one is unhealthy.

Visit **`/_zcast/admin`** while signed in to view the visual **Module Dependency Graph**, edit site title / pinned homepage / theme colour mode live (zero restart), or download a `.zip` backup.

See [configuration.md](configuration.md) for full `zcast.toml`, environment variable, theming, and HTTP route documentation.
