# `zcast` Installer & Updater UX Specification

> **Goal:** Faster and simpler than WordPress's famous 5-minute install. Paste one `curl` command, hit `Enter` a couple of times through **Express Setup**, and you're live on HTTPS—with an **Advanced** path when you want full control, and a friendly nudge to tinker with modules afterward.

---

## 1. End-to-End ~15-Second Express Journey

```bash
curl -sSL https://raw.githubusercontent.com/<user>/zcast/main/installer/install.sh | sh
```

### Stage 1: Minimal POSIX Bootstrapper (`install.sh` — ~3–5 seconds, zero prompts)
1. Detects OS (`linux` / `darwin`) and CPU arch (`x86_64` / `aarch64`).
2. **Idempotent:** If `zcast` is already configured (`/etc/zcast/zcast.toml` or local config exists), automatically runs **`zcast update`** instead of overwriting your installation.
3. Downloads the latest prebuilt static binary + `SHA256SUMS` from GitHub Releases and verifies SHA-256.
4. Installs `zcast` to `/usr/local/bin/zcast` (using `sudo` only if needed on Linux, or `~/.local/bin/zcast` for non-root installs).
5. Hands off cleanly to the interactive Rust TUI via `/dev/tty` (avoiding the `curl | sh` piped-stdin EOF trap):
   ```sh
   if [ -e /dev/tty ]; then
     exec "$ZCAST_BIN" setup < /dev/tty
   else
     echo "zcast installed. Run 'zcast setup' to configure."
   fi
   ```

---

### Stage 2: Express-First `zcast setup` TUI Wizard

**Production website is ALWAYS the default.** The wizard detects any existing web server (Caddy, Nginx, Apache) automatically and presents one clear **Express Setup** summary before doing anything:

```text
┌  zcast setup
│
◆  How are you using zcast?
│  ● Production website
│  ○ Local only (no auth)
│
◆  Domain name (e.g. notes.davidhewitson.co.uk)
│  └  notes.davidhewitson.co.uk
│
│  Detected web server: Caddy (/etc/caddy/Caddyfile)
│
│  Express setup plan:
│    Site title:         Zcast
│    Database:           SQLite (/var/lib/zcast/zcast.db)
│    Admin auth:         Basic (generated login shown after install)
│    Theme colour mode:  Auto
│    Default modules:    wikilinks, popover, search, cmd-k, inline-editor,
│                        davidhewitson theme
│    Virtual host:       Add notes.davidhewitson.co.uk to Caddy & reload
│
◆  How would you like to proceed?
│  ● Express setup (use defaults above)
│  ○ Advanced setup (customize database, auth, theme, modules & server)
│
◇  Created unprivileged system user `zcast`
◇  Initialized SQLite database & seeded starter card "Index"
◇  Started systemd service `zcast.service` (health check OK)
◇  Configured Caddy vhost for notes.davidhewitson.co.uk & reloaded
│
└  You're live!

   Site:      https://notes.davidhewitson.co.uk
   Admin:     https://notes.davidhewitson.co.uk/_zcast/login
   Username:  admin
   Password:  k9M4-vP2x-Q7rL-8nTz   (save this — shown once)

   Make it yours & tinker with modules:
     zcast module list                        See what's installed & how modules connect
     zcast module install zcast-io-markdown   Add Markdown folder import/export
     zcast module install zcast-auth-magic-link  Switch to passwordless magic-link login
     zcast config set theme.mode dark         Lock your site to dark mode
     zcast update                             Safely back up & update zcast
```

*(In **Local only (no auth)** mode, the domain & vhost prompts are skipped entirely—Express setup binds strictly to `127.0.0.1:3000` with no auth, seeds the starter "Index" card, and prints `http://127.0.0.1:3000` immediately.)*

---

## 2. Advanced Setup Path

Choosing **Advanced setup** walks cleanly through overriding any of the Express defaults:
1. **Site title** (default: `Zcast`)
2. **Database (`storage` module):** SQLite (default) vs. MySQL (with live connection test right inside the wizard before continuing)
3. **Admin authentication (`auth` module):**
   - Basic Login (auto-generated or custom password)
   - Magic Link (prompts for admin email + SMTP / Resend / Postmark API, or *"skip email for now & use `zcast auth login-link`"*)
   - None *(only permitted when bound to `127.0.0.1`)*
4. **Theme colour mode:** `Auto` / `Dark only` / `Light only`
5. **Default modules checklist:** Toggle bundled modules (`zcast-popover`, `zcast-cmd-k`, `zcast-search-fts`, `zcast-editor-inline`, `zcast-io-markdown`, etc.)
6. **Port & Web Server Vhost:** Custom local bind port (default `127.0.0.1:8080`), plus choose whether to auto-configure detected **Caddy / Nginx / Apache** or simply print a copy-pasteable vhost snippet.

---

## 3. Intelligent Web Server Detection (Zero-Pain Vhost Setup)

Because `zcast` binds to `127.0.0.1:8080`, `zcast setup` inspects the host before proposing the Express plan:
- **Caddy detected:** Proposes appending a clean 3-line site block (`notes.davidhewitson.co.uk { reverse_proxy 127.0.0.1:8080 }`) to `/etc/caddy/Caddyfile` (or `/etc/caddy/conf.d/` if imported) and reloading Caddy for automatic HTTPS.
- **Nginx detected:** Proposes writing `/etc/nginx/sites-available/zcast-<domain>`, symlinking into `sites-enabled`, running `nginx -t` + reload, and offering to run `certbot --nginx -d <domain>` if `certbot` is installed (or printing the exact one-liner if not).
- **Apache detected:** Proposes writing an enabled vhost reverse-proxy config (`ProxyPass`), verifying with `apachectl configtest`, and reloading.
- **None detected:** Starts `zcast.service` cleanly and prints ready-to-paste Caddy/Nginx snippets so nothing fails silently.

---

## 4. Security & Updater Safeguards

### Unprivileged Defaults (Linux Production)
- Runs under a dedicated unprivileged `zcast` system user (never `root`).
- Config stored at `/etc/zcast/zcast.toml` (`0640 root:zcast`).
- Data, SQLite DB, backups, `custom.css`, and module directories live in `/var/lib/zcast/` (`0750 zcast:zcast`).
- Starter **"Index"** card is seeded and pinned at `/` with an example `[[My First Note]]` link so the live site works immediately out of the box.

### Fearless `zcast update`
1. **Standard vs. Custom-Module Aware:** If running only bundled modules, downloads the prebuilt static binary (~3s); if custom modules were installed via `zcast module install`, updates core and runs an incremental rebuild using the warm `target/` cache (never overwriting custom modules).
2. **Automatic Backup:** Snapshots SQLite (`VACUUM INTO /var/lib/zcast/backups/...`) and keeps the prior binary as `zcast.prev`.
3. **Atomic Swap + Auto-Rollback:** Swaps binary atomically, restarts `zcast.service`, and verifies `/_zcast/health`; automatically rolls back to `zcast.prev` if the health check fails.

### Scriptable / Non-Interactive Mode
All options remain accessible via flags or a pre-supplied config file (`zcast setup --express --domain ...` or `zcast setup --config ./zcast.toml`) for automated provisioning.
