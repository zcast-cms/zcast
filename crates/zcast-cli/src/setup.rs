//! Interactive `zcast setup` wizard, web-server detection, and generated
//! production artefacts (`zcast.toml`, vhost snippets, hardened systemd unit).
//!
//! The wizard is Express-first: production is the default, and a single
//! confirmation screen covers the common path. Advanced setup walks through the
//! database, auth, theme, search provider and bind address.
#![allow(dead_code)] // Wired into `main.rs` by the coordinator.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use zcast_interfaces::ThemeColorMode;

/// True when the process is running as root on Linux.
///
/// Used to gate automatic, host-mutating production provisioning: writing
/// `/etc/systemd/system`, vhosts and TLS certificates requires root, and we
/// never want to attempt it on macOS or as an unprivileged user.
pub fn is_linux_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        Command::new("id").arg("-u").output().is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "0"
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Run a command, mapping a non-zero exit into a readable error.
fn run_checked(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!("{program} {} failed", args.join(" ")))
    } else {
        Err(format!("{program} {} failed: {stderr}", args.join(" ")))
    }
}

/// True when an executable is available on `PATH`.
fn command_on_path(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program} >/dev/null 2>&1")])
        .status()
        .is_ok_and(|status| status.success())
}

/// Create a symlink (copying instead on platforms without symlinks).
fn symlink_file(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dst)
    }
    #[cfg(not(unix))]
    {
        fs::copy(src, dst).map(|_| ())
    }
}

/// Reverse proxy detected on the host (DEC-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebServer {
    Caddy,
    Nginx,
    Apache,
    None,
}

impl WebServer {
    /// Human readable name used in the wizard and generated files.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caddy => "Caddy",
            Self::Nginx => "Nginx",
            Self::Apache => "Apache",
            Self::None => "none",
        }
    }

    /// Config location hint for the detected server.
    pub fn config_hint(self) -> &'static str {
        match self {
            Self::Caddy => "/etc/caddy/Caddyfile",
            Self::Nginx => "/etc/nginx/sites-available",
            Self::Apache => "/etc/apache2/sites-available",
            Self::None => "no supported web server found",
        }
    }

    /// File name for a generated vhost snippet.
    pub fn vhost_filename(self, domain: &str) -> String {
        match self {
            Self::Caddy => "Caddyfile".to_owned(),
            Self::Nginx => format!("zcast-{domain}"),
            Self::Apache => format!("zcast-{domain}.conf"),
            Self::None => "zcast-vhost.txt".to_owned(),
        }
    }
}

/// Detect a reverse proxy from an arbitrary `/etc`-style root.
pub fn detect_web_server_in(etc_root: &Path) -> WebServer {
    if etc_root.join("caddy").join("Caddyfile").exists() {
        return WebServer::Caddy;
    }

    let nginx = etc_root.join("nginx");
    if nginx.join("sites-available").exists() || nginx.join("nginx.conf").exists() {
        return WebServer::Nginx;
    }

    if etc_root.join("apache2").join("sites-available").exists()
        || etc_root.join("httpd").join("conf.d").exists()
    {
        return WebServer::Apache;
    }

    WebServer::None
}

/// Detect a reverse proxy from the host's `/etc`.
pub fn detect_web_server() -> WebServer {
    detect_web_server_in(Path::new("/etc"))
}

/// Answers collected by `zcast setup`.
#[derive(Debug, Clone)]
pub struct SetupOptions {
    /// Production website (`true`) or local-only (`false`).
    pub production: bool,
    pub domain: Option<String>,
    pub site_title: String,
    pub bind_addr: String,
    pub database_url: String,
    pub color_mode: ThemeColorMode,
    /// `none`, `basic`, or `magic-link`.
    pub auth_mode: String,
    pub admin_user: String,
    pub admin_password: Option<String>,
    pub admin_email: Option<String>,
    /// `fts`, `onnx`, or `none`.
    pub search_provider: String,
    pub config_path: PathBuf,
}

impl Default for SetupOptions {
    fn default() -> Self {
        Self {
            production: false,
            domain: None,
            site_title: "Zcast".to_owned(),
            bind_addr: "127.0.0.1:3000".to_owned(),
            database_url: "sqlite://zcast.db?mode=rwc".to_owned(),
            color_mode: ThemeColorMode::Auto,
            auth_mode: "none".to_owned(),
            admin_user: "admin".to_owned(),
            admin_password: None,
            admin_email: None,
            search_provider: "fts".to_owned(),
            config_path: PathBuf::from("/etc/zcast/zcast.toml"),
        }
    }
}

fn escape_toml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn normalized_auth_mode(mode: &str) -> String {
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "local" | "local-only" => "none".to_owned(),
        "basic" => "basic".to_owned(),
        "magic-link" | "magic_link" | "magiclink" | "magic link" => "magic-link".to_owned(),
        other => other.to_owned(),
    }
}

fn normalized_search_provider(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "" | "fts" | "keyword" | "zcast-search-fts" => "fts".to_owned(),
        "onnx" | "semantic" | "embeddings" | "zcast-search-onnx" => "onnx".to_owned(),
        "none" | "off" | "disabled" => "none".to_owned(),
        other => other.to_owned(),
    }
}

/// Render a well-commented `zcast.toml` matching `KernelConfig`.
pub fn render_zcast_toml(opts: &SetupOptions) -> String {
    let auth_mode = normalized_auth_mode(&opts.auth_mode);
    let mut out = String::new();

    out.push_str("# zcast configuration\n");
    out.push_str("# Generated by `zcast setup` — edit freely, then restart zcast.\n");
    out.push_str("# Secrets may also be supplied via ZCAST_* environment variables, which win.\n");
    if opts.production {
        match opts.domain.as_deref() {
            Some(domain) if !domain.is_empty() => {
                out.push_str(&format!(
                    "# Public URL: https://{domain} (reverse proxied)\n"
                ));
            }
            _ => out.push_str("# Public URL: <domain not set — configure your reverse proxy>\n"),
        }
    } else {
        out.push_str("# Mode: local only (bound to loopback, no auth)\n");
    }
    if let Some(email) = opts.admin_email.as_deref().filter(|e| !e.is_empty()) {
        out.push_str(&format!("# Admin email (magic-link login): {email}\n"));
    }
    out.push_str(&format!(
        "# Search provider: {} (install via `zcast module install`)\n",
        normalized_search_provider(&opts.search_provider)
    ));
    out.push('\n');

    out.push_str("[zcast]\n");
    out.push_str(&format!(
        "site_title = \"{}\"\n",
        escape_toml(&opts.site_title)
    ));
    out.push_str(&format!(
        "bind_addr = \"{}\"\n",
        escape_toml(&opts.bind_addr)
    ));
    out.push_str(&format!(
        "database_url = \"{}\"\n",
        escape_toml(&opts.database_url)
    ));
    out.push_str(&format!("color_mode = \"{}\"\n", opts.color_mode.as_str()));
    out.push_str("pinned_home_slug = \"index\"\n");
    out.push_str("# Keep this false unless you really mean to expose an unauthenticated site.\n");
    out.push_str("allow_unauthenticated_public = false\n");
    out.push_str("# Auth provider: `none`, `basic`, or `magic-link`.\n");
    out.push_str(&format!("auth = \"{}\"\n", escape_toml(&auth_mode)));
    out.push_str(&format!(
        "admin_user = \"{}\"\n",
        escape_toml(&opts.admin_user)
    ));
    match opts.admin_password.as_deref() {
        Some(password) if !password.is_empty() => out.push_str(&format!(
            "# Prefer ZCAST_ADMIN_PASSWORD over storing the password here.\nadmin_password = \"{}\"\n",
            escape_toml(password)
        )),
        _ => {
            out.push_str("# admin_password is read from ZCAST_ADMIN_PASSWORD or generated at first run.\n")
        }
    }
    out.push_str("# custom_css_path = \"/var/lib/zcast/custom.css\"\n");

    out
}

/// Render a copy-pasteable reverse proxy config for the detected server.
pub fn render_vhost_snippet(server: WebServer, domain: &str, bind_addr: &str) -> String {
    let domain = domain.trim();
    let bind_addr = bind_addr.trim();

    match server {
        WebServer::Caddy => format!(
            "# {hint}\n\
             #\n\
             # Append this block, then: systemctl reload caddy\n\
             {domain} {{\n\
             \x20   encode zstd gzip\n\
             \x20   reverse_proxy {bind_addr}\n\
             }}\n",
            hint = server.config_hint()
        ),
        WebServer::Nginx => format!(
            "# {hint}/zcast-{domain}\n\
             #\n\
             # Enable with:\n\
             #   ln -s {hint}/zcast-{domain} /etc/nginx/sites-enabled/zcast-{domain}\n\
             #   nginx -t && systemctl reload nginx\n\
             server {{\n\
             \x20   listen 80;\n\
             \x20   listen [::]:80;\n\
             \x20   server_name {domain};\n\
             \n\
             \x20   location / {{\n\
             \x20       proxy_pass http://{bind_addr};\n\
             \x20       proxy_http_version 1.1;\n\
             \x20       proxy_set_header Host $host;\n\
             \x20       proxy_set_header X-Real-IP $remote_addr;\n\
             \x20       proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n\
             \x20       proxy_set_header X-Forwarded-Proto $scheme;\n\
             \x20       proxy_set_header Upgrade $http_upgrade;\n\
             \x20       proxy_set_header Connection \"upgrade\";\n\
             \x20       proxy_read_timeout 300s;\n\
             \x20   }}\n\
             }}\n",
            hint = server.config_hint()
        ),
        WebServer::Apache => format!(
            "# {hint}/zcast-{domain}.conf\n\
             #\n\
             # Enable with:\n\
             #   a2ensite zcast-{domain} && apachectl configtest && systemctl reload apache2\n\
             <VirtualHost *:80>\n\
             \x20   ServerName {domain}\n\
             \x20   ProxyPreserveHost On\n\
             \x20   ProxyRequests Off\n\
             \n\
             \x20   ProxyPass / http://{bind_addr}/\n\
             \x20   ProxyPassReverse / http://{bind_addr}/\n\
             \n\
             \x20   RequestHeader set X-Forwarded-Proto \"http\"\n\
             \x20   RequestHeader set X-Forwarded-Host \"{domain}\"\n\
             \n\
             \x20   ErrorLog ${{APACHE_LOG_DIR}}/zcast-{domain}-error.log\n\
             \x20   CustomLog ${{APACHE_LOG_DIR}}/zcast-{domain}-access.log combined\n\
             </VirtualHost>\n",
            hint = server.config_hint()
        ),
        WebServer::None => format!(
            "# No supported web server detected on this host.\n\
             # zcast.service will still run on {bind_addr}; point any reverse proxy at it.\n\
             #\n\
             # Caddy (recommended — automatic HTTPS):\n\
             #   {domain} {{\n\
             #       reverse_proxy {bind_addr}\n\
             #   }}\n\
             #\n\
             # Nginx:\n\
             #   server {{\n\
             #       listen 80;\n\
             #       server_name {domain};\n\
             #       location / {{ proxy_pass http://{bind_addr}; }}\n\
             #   }}\n"
        ),
    }
}

/// Render a plain-HTTP Nginx vhost for `domain`, proxying to `bind_addr`.
///
/// When `include_acme_webroot` is true the HTTP server block also serves
/// `/.well-known/acme-challenge/` from `/var/www/letsencrypt`, so Certbot can
/// complete an HTTP-01 challenge even before the proxy config is live.
pub fn render_nginx_vhost_config(
    domain: &str,
    bind_addr: &str,
    include_acme_webroot: bool,
) -> String {
    let domain = domain.trim();
    let bind_addr = bind_addr.trim();
    let acme = if include_acme_webroot {
        "    location /.well-known/acme-challenge/ { root /var/www/letsencrypt; }\n\n"
    } else {
        ""
    };

    format!(
        "server {{\n\
         \x20   listen 80;\n\
         \x20   listen [::]:80;\n\
         \x20   server_name {domain};\n\
         \n\
         {acme}\
         \x20   location / {{\n\
         \x20       proxy_pass http://{bind_addr};\n\
         \x20       proxy_http_version 1.1;\n\
         \x20       proxy_set_header Host $host;\n\
         \x20       proxy_set_header X-Real-IP $remote_addr;\n\
         \x20       proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n\
         \x20       proxy_set_header X-Forwarded-Proto $scheme;\n\
         \x20       proxy_set_header Upgrade $http_upgrade;\n\
         \x20       proxy_set_header Connection \"upgrade\";\n\
         \x20       proxy_read_timeout 300s;\n\
         \x20   }}\n\
         }}\n"
    )
}

/// Render an HTTP-only Apache vhost for `domain`, proxying to `bind_addr`.
fn render_apache_vhost_config(domain: &str, bind_addr: &str) -> String {
    let domain = domain.trim();
    let bind_addr = bind_addr.trim();
    format!(
        "<VirtualHost *:80>\n\
         \x20   ServerName {domain}\n\
         \x20   ProxyPreserveHost On\n\
         \x20   ProxyRequests Off\n\
         \n\
         \x20   ProxyPass / http://{bind_addr}/\n\
         \x20   ProxyPassReverse / http://{bind_addr}/\n\
         \n\
         \x20   RequestHeader set X-Forwarded-Proto \"http\"\n\
         \x20   RequestHeader set X-Forwarded-Host \"{domain}\"\n\
         \n\
         \x20   ErrorLog ${{APACHE_LOG_DIR}}/zcast-{domain}-error.log\n\
         \x20   CustomLog ${{APACHE_LOG_DIR}}/zcast-{domain}-access.log combined\n\
         </VirtualHost>\n"
    )
}

/// Idempotently append a Caddy site block for `domain`.
///
/// Returns `None` when a non-comment line already opens a block containing
/// `domain` (so repeated `zcast setup --express` runs never duplicate config).
pub fn append_caddy_block_if_missing(
    existing: &str,
    domain: &str,
    bind_addr: &str,
) -> Option<String> {
    let domain = domain.trim();
    let bind_addr = bind_addr.trim();
    if domain.is_empty() {
        return None;
    }

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((head, _)) = trimmed.split_once('{') else {
            continue;
        };
        if head
            .split_whitespace()
            .any(|token| token.trim_end_matches(',') == domain)
        {
            return None;
        }
    }

    let mut out = existing.to_owned();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "{domain} {{\n\
         \x20   encode zstd gzip\n\
         \x20   reverse_proxy {bind_addr}\n\
         }}\n"
    ));

    Some(out)
}

/// Outcome of [`apply_linux_production_setup`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionReport {
    /// Human-readable actions that completed successfully.
    pub steps: Vec<String>,
    /// Non-fatal problems the operator should look at (e.g. DNS not ready).
    pub warnings: Vec<String>,
    pub vhost_configured: bool,
    pub tls_configured: bool,
    pub service_started: bool,
    pub health_ok: bool,
}

/// Apply a complete, idempotent Linux production deployment as root.
///
/// Creates the unprivileged `zcast` system user, a hardened work directory and
/// config permissions, a systemd unit, and (when a domain is set) an idempotent
/// Nginx/Caddy/Apache vhost plus a best-effort Certbot certificate.
///
/// Anything TLS-related that fails (usually DNS not pointing at the host yet)
/// is reported as a warning rather than aborting the whole setup.
pub fn apply_linux_production_setup(
    opts: &SetupOptions,
    detected: WebServer,
    bin_path: &str,
    work_dir: &Path,
) -> Result<ProvisionReport, String> {
    let mut report = ProvisionReport::default();
    let work_dir_str = work_dir.display().to_string();
    let config_path = opts.config_path.display().to_string();

    // 1. Dedicated unprivileged system user.
    let user_exists = Command::new("id")
        .args(["-u", "zcast"])
        .output()
        .is_ok_and(|out| out.status.success());
    if user_exists {
        report
            .steps
            .push("System user 'zcast' already exists".to_owned());
    } else {
        run_checked(
            "useradd",
            &[
                "--system",
                "--home-dir",
                &work_dir_str,
                "--shell",
                "/usr/sbin/nologin",
                "zcast",
            ],
        )?;
        report.steps.push("Created system user 'zcast'".to_owned());
    }

    // 2. Work directory and permissions.
    fs::create_dir_all(work_dir)
        .map_err(|e| format!("failed to create {}: {e}", work_dir.display()))?;
    let backups = work_dir.join("backups");
    fs::create_dir_all(&backups)
        .map_err(|e| format!("failed to create {}: {e}", backups.display()))?;
    run_checked("chown", &["-R", "zcast:zcast", &work_dir_str])?;
    run_checked("chmod", &["0750", &work_dir_str])?;
    run_checked("chown", &["root:zcast", &config_path])?;
    run_checked("chmod", &["0640", &config_path])?;
    report.steps.push(format!(
        "Prepared {work_dir_str} (zcast:zcast, 0750) and {config_path} (root:zcast, 0640)"
    ));

    // 3. Hardened systemd unit.
    let unit_path = Path::new("/etc/systemd/system/zcast.service");
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    fs::write(
        unit_path,
        render_systemd_unit(bin_path, &config_path, &work_dir_str),
    )
    .map_err(|e| format!("failed to write {}: {e}", unit_path.display()))?;
    report.steps.push(format!("Wrote {}", unit_path.display()));

    run_checked("systemctl", &["daemon-reload"])?;
    run_checked("systemctl", &["enable", "zcast.service"])?;
    run_checked("systemctl", &["restart", "zcast.service"])?;
    report.service_started = true;
    report
        .steps
        .push("Enabled and started zcast.service".to_owned());

    // 4. Wait for the service to answer its health endpoint.
    report.health_ok = crate::update::check_health_endpoint(&opts.bind_addr, 15, 500);
    if report.health_ok {
        report.steps.push(format!(
            "Health check passed at http://{}/_zcast/health",
            opts.bind_addr.trim()
        ));
    } else {
        report.warnings.push(format!(
            "zcast.service did not answer http://{}/_zcast/health yet — check `systemctl status zcast` and `journalctl -u zcast`",
            opts.bind_addr.trim()
        ));
    }

    // 5. Reverse proxy + TLS.
    if let Some(domain) = opts
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        match detected {
            WebServer::Caddy => {
                let caddyfile = Path::new("/etc/caddy/Caddyfile");
                let existing = fs::read_to_string(caddyfile).unwrap_or_default();
                match append_caddy_block_if_missing(&existing, domain, &opts.bind_addr) {
                    Some(updated) => {
                        fs::write(caddyfile, updated)
                            .map_err(|e| format!("failed to write {}: {e}", caddyfile.display()))?;
                        report.steps.push(format!(
                            "Added {domain} to {} (automatic HTTPS)",
                            caddyfile.display()
                        ));
                    }
                    None => report.steps.push(format!(
                        "{domain} is already configured in {}",
                        caddyfile.display()
                    )),
                }
                report.vhost_configured = true;
                report.tls_configured = true;
                if let Err(e) = run_checked("systemctl", &["reload", "caddy"]) {
                    report
                        .warnings
                        .push(format!("could not reload caddy: {e}"));
                } else {
                    report.steps.push("Reloaded caddy".to_owned());
                }
            }
            WebServer::Nginx => {
                let existing_path = PathBuf::from(format!("/etc/nginx/sites-available/{domain}"));
                if existing_path.exists() {
                    report.warnings.push(format!(
                        "existing Nginx config {} was left untouched; add `proxy_pass http://{}` manually if needed",
                        existing_path.display(),
                        opts.bind_addr.trim()
                    ));
                } else {
                    let available = format!("/etc/nginx/sites-available/zcast-{domain}");
                    let acme_webroot = Path::new("/var/www/letsencrypt").is_dir();
                    fs::write(
                        &available,
                        render_nginx_vhost_config(domain, &opts.bind_addr, acme_webroot),
                    )
                    .map_err(|e| format!("failed to write {available}: {e}"))?;

                    let enabled = format!("/etc/nginx/sites-enabled/zcast-{domain}");
                    if !Path::new(&enabled).exists() {
                        symlink_file(Path::new(&available), Path::new(&enabled))
                            .map_err(|e| format!("failed to symlink {enabled}: {e}"))?;
                    }

                    run_checked("nginx", &["-t"])?;
                    run_checked("systemctl", &["reload", "nginx"])?;
                    report.vhost_configured = true;
                    report
                        .steps
                        .push(format!("Wrote and enabled {available}"));
                }

                let certbot_command = certbot_command_for("nginx", domain, opts.admin_email.as_deref());
                if command_on_path("certbot") {
                    let mut args = vec![
                        "--nginx",
                        "--non-interactive",
                        "--agree-tos",
                        "--redirect",
                        "-d",
                        domain,
                    ];
                    match opts.admin_email.as_deref().filter(|e| !e.trim().is_empty()) {
                        Some(email) => args.extend(["--email", email]),
                        None => args.push("--register-unsafely-without-email"),
                    }
                    match run_checked("certbot", &args) {
                        Ok(()) => {
                            report.tls_configured = true;
                            report
                                .steps
                                .push(format!("Obtained a TLS certificate for {domain}"));
                        }
                        Err(e) => report.warnings.push(format!(
                            "could not obtain a TLS certificate for {domain} yet ({e}). Once DNS points at this host, run:\n       {certbot_command}"
                        )),
                    }
                } else {
                    report.warnings.push(format!(
                        "certbot is not installed — skipping TLS. Install it and run:\n       {certbot_command}"
                    ));
                }
            }
            WebServer::Apache => {
                let available = format!("/etc/apache2/sites-available/zcast-{domain}.conf");
                fs::write(&available, render_apache_vhost_config(domain, &opts.bind_addr))
                    .map_err(|e| format!("failed to write {available}: {e}"))?;
                run_checked("a2ensite", &[&format!("zcast-{domain}")])?;
                run_checked("apachectl", &["configtest"])?;
                run_checked("systemctl", &["reload", "apache2"])?;
                report.vhost_configured = true;
                report
                    .steps
                    .push(format!("Wrote and enabled {available}"));

                let certbot_command = certbot_command_for("apache", domain, opts.admin_email.as_deref());
                if command_on_path("certbot") {
                    let mut args = vec![
                        "--apache",
                        "--non-interactive",
                        "--agree-tos",
                        "--redirect",
                        "-d",
                        domain,
                    ];
                    match opts.admin_email.as_deref().filter(|e| !e.trim().is_empty()) {
                        Some(email) => args.extend(["--email", email]),
                        None => args.push("--register-unsafely-without-email"),
                    }
                    match run_checked("certbot", &args) {
                        Ok(()) => {
                            report.tls_configured = true;
                            report
                                .steps
                                .push(format!("Obtained a TLS certificate for {domain}"));
                        }
                        Err(e) => report.warnings.push(format!(
                            "could not obtain a TLS certificate for {domain} yet ({e}). Once DNS points at this host, run:\n       {certbot_command}"
                        )),
                    }
                } else {
                    report.warnings.push(format!(
                        "certbot is not installed — skipping TLS. Install it and run:\n       {certbot_command}"
                    ));
                }
            }
            WebServer::None => report.warnings.push(format!(
                "no supported web server detected — zcast.service is serving http://{}; point a reverse proxy at it for {domain}",
                opts.bind_addr.trim()
            )),
        }
    }

    Ok(report)
}

/// The exact Certbot command an operator should re-run later on TLS failure.
fn certbot_command_for(plugin: &str, domain: &str, admin_email: Option<&str>) -> String {
    match admin_email.map(str::trim).filter(|e| !e.is_empty()) {
        Some(email) => {
            format!(
                "certbot --{plugin} --non-interactive --agree-tos --redirect -d {domain} --email {email}"
            )
        }
        None => format!(
            "certbot --{plugin} --non-interactive --agree-tos --redirect -d {domain} --register-unsafely-without-email"
        ),
    }
}

/// Render a hardened, unprivileged systemd unit for production.
pub fn render_systemd_unit(bin_path: &str, config_path: &str, work_dir: &str) -> String {
    format!(
        "[Unit]\n\
         Description=zcast — fast modular Zettelkasten CMS\n\
         Documentation=https://github.com/zcast-cms/zcast\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User=zcast\n\
         Group=zcast\n\
         WorkingDirectory={work_dir}\n\
         ExecStart={bin_path} --config {config_path} serve\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         TimeoutStopSec=20\n\
         \n\
         # Hardening\n\
         NoNewPrivileges=true\n\
         PrivateTmp=true\n\
         PrivateDevices=true\n\
         ProtectSystem=strict\n\
         ProtectHome=true\n\
         ProtectKernelTunables=true\n\
         ProtectKernelModules=true\n\
         ProtectControlGroups=true\n\
         RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n\
         RestrictNamespaces=true\n\
         LockPersonality=true\n\
         SystemCallArchitectures=native\n\
         ReadWritePaths={work_dir}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

fn ask_line<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
    default: &str,
) -> io::Result<String> {
    write!(output, "  {prompt}")?;
    if default.is_empty() {
        write!(output, ": ")?;
    } else {
        write!(output, " [{default}]: ")?;
    }
    output.flush()?;

    let mut line = String::new();
    let read = input.read_line(&mut line)?;
    if read == 0 {
        writeln!(output)?;
        return Ok(default.to_owned());
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

/// Express-first interactive wizard.
pub fn prompt_interactive_setup(
    defaults: SetupOptions,
    detected: WebServer,
) -> Result<SetupOptions, io::Error> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    prompt_interactive_with(defaults, detected, &mut input, &mut output)
}

/// Testable wizard core over any reader/writer pair.
pub fn prompt_interactive_with<R: BufRead, W: Write>(
    defaults: SetupOptions,
    detected: WebServer,
    input: &mut R,
    output: &mut W,
) -> Result<SetupOptions, io::Error> {
    writeln!(output)?;
    writeln!(output, "  zcast setup")?;
    writeln!(output, "  -----------")?;
    writeln!(output)?;
    writeln!(output, "  How are you using zcast?")?;
    writeln!(output, "    1) Production website (recommended)")?;
    writeln!(output, "    2) Local only (no auth)")?;
    let usage = ask_line(input, output, "Choice", "1")?;
    let production = !matches!(
        usage.to_ascii_lowercase().as_str(),
        "2" | "local" | "local only" | "local-only"
    );

    let mut opts = defaults;
    opts.production = production;
    opts.auth_mode = normalized_auth_mode(&opts.auth_mode);
    opts.search_provider = normalized_search_provider(&opts.search_provider);

    if !production {
        // Local mode is frictionless: loopback only, no auth, no vhost work.
        opts.bind_addr = "127.0.0.1:3000".to_owned();
        opts.auth_mode = "none".to_owned();
        writeln!(output)?;
        writeln!(output, "  Local-only setup:")?;
        writeln!(output, "    Site title:    {}", opts.site_title)?;
        writeln!(output, "    Bind address:  http://{}", opts.bind_addr)?;
        writeln!(output, "    Database:      {}", opts.database_url)?;
        writeln!(output, "    Admin auth:    none (loopback only)")?;
        writeln!(output, "    Theme mode:    {}", opts.color_mode.as_str())?;
        writeln!(output, "    Config file:   {}", opts.config_path.display())?;
        writeln!(output)?;
        writeln!(
            output,
            "  Run `zcast serve` when ready — your starter \"Index\" card is pinned at /."
        )?;
        return Ok(opts);
    }

    let domain_default = opts.domain.clone().unwrap_or_default();
    let domain = ask_line(
        input,
        output,
        "Domain name (e.g. notes.davidhewitson.co.uk)",
        &domain_default,
    )?;
    if domain.is_empty() {
        opts.domain = None;
        writeln!(
            output,
            "  ! No domain given — skipping automatic vhost generation (snippet will be printed)."
        )?;
    } else {
        opts.domain = Some(domain.trim().to_owned());
    }

    match detected {
        WebServer::None => {
            writeln!(output, "\n  Detected web server: none")?;
        }
        server => {
            writeln!(
                output,
                "\n  Detected web server: {} ({})",
                server.as_str(),
                server.config_hint()
            )?;
        }
    }

    // Production Express defaults.
    if opts.auth_mode == "none" {
        opts.auth_mode = "basic".to_owned();
    }

    let vhost_line = match (opts.domain.as_deref(), detected) {
        (Some(domain), WebServer::None) => {
            format!("Print a Caddy/Nginx snippet for {domain}")
        }
        (Some(domain), server) => {
            format!(
                "Add {domain} to {} ({}), then reload",
                server.as_str(),
                server.vhost_filename(domain)
            )
        }
        (None, _) => "Skipped (no domain supplied)".to_owned(),
    };

    writeln!(output)?;
    writeln!(output, "  Express setup plan:")?;
    writeln!(output, "    Site title:         {}", opts.site_title)?;
    writeln!(output, "    Bind address:       {}", opts.bind_addr)?;
    writeln!(output, "    Database:           {}", opts.database_url)?;
    writeln!(output, "    Admin auth:         {}", opts.auth_mode)?;
    writeln!(output, "    Admin user:         {}", opts.admin_user)?;
    writeln!(
        output,
        "    Theme colour mode:  {}",
        opts.color_mode.as_str()
    )?;
    writeln!(output, "    Search provider:    {}", opts.search_provider)?;
    writeln!(
        output,
        "    Default modules:    wikilinks, popover, search, cmd-k, inline-editor,"
    )?;
    writeln!(output, "                        davidhewitson theme")?;
    writeln!(output, "    Virtual host:       {vhost_line}")?;
    writeln!(
        output,
        "    Config file:        {}",
        opts.config_path.display()
    )?;

    writeln!(output)?;
    writeln!(output, "  How would you like to proceed?")?;
    writeln!(output, "    1) Express setup (use defaults above)")?;
    writeln!(
        output,
        "    2) Advanced setup (customize database, auth, theme, modules & server)"
    )?;
    let proceed = ask_line(input, output, "Choice", "1")?;
    let advanced = matches!(
        proceed.to_ascii_lowercase().as_str(),
        "2" | "advanced" | "a"
    );

    if !advanced {
        return Ok(opts);
    }

    writeln!(output)?;
    writeln!(output, "  Advanced setup")?;
    writeln!(output, "  ---------------")?;

    opts.site_title = ask_line(input, output, "Site title", &opts.site_title)?;
    opts.database_url = ask_line(input, output, "Database URL", &opts.database_url)?;

    let auth = ask_line(
        input,
        output,
        "Auth mode (none/basic/magic-link)",
        &opts.auth_mode,
    )?;
    opts.auth_mode = normalized_auth_mode(&auth);

    let theme = ask_line(
        input,
        output,
        "Theme colour mode (auto/dark/light)",
        opts.color_mode.as_str(),
    )?;
    opts.color_mode = ThemeColorMode::parse(&theme);

    let search = ask_line(
        input,
        output,
        "Search provider (fts/onnx/none)",
        &opts.search_provider,
    )?;
    opts.search_provider = normalized_search_provider(&search);

    opts.bind_addr = ask_line(input, output, "Bind address", &opts.bind_addr)?;

    if opts.auth_mode == "magic-link" && opts.admin_email.is_none() {
        let email = ask_line(input, output, "Admin email (for magic links)", "")?;
        if !email.is_empty() {
            opts.admin_email = Some(email);
        }
    }

    writeln!(output)?;
    writeln!(output, "  Advanced setup ready:")?;
    writeln!(output, "    Site title:    {}", opts.site_title)?;
    writeln!(output, "    Bind address:  {}", opts.bind_addr)?;
    writeln!(output, "    Database:      {}", opts.database_url)?;
    writeln!(output, "    Admin auth:    {}", opts.auth_mode)?;
    writeln!(output, "    Theme mode:    {}", opts.color_mode.as_str())?;
    writeln!(output, "    Search:        {}", opts.search_provider)?;

    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zcast-cli-setup-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_caddy() {
        let dir = temp_dir("caddy");
        fs::create_dir_all(dir.join("caddy")).expect("mkdir");
        fs::write(dir.join("caddy").join("Caddyfile"), "{}").expect("write");
        assert_eq!(detect_web_server_in(&dir), WebServer::Caddy);
        cleanup(&dir);
    }

    #[test]
    fn detects_nginx_sites_available() {
        let dir = temp_dir("nginx-sites");
        fs::create_dir_all(dir.join("nginx").join("sites-available")).expect("mkdir");
        assert_eq!(detect_web_server_in(&dir), WebServer::Nginx);
        cleanup(&dir);
    }

    #[test]
    fn detects_nginx_conf() {
        let dir = temp_dir("nginx-conf");
        fs::create_dir_all(dir.join("nginx")).expect("mkdir");
        fs::write(dir.join("nginx").join("nginx.conf"), "events {}").expect("write");
        assert_eq!(detect_web_server_in(&dir), WebServer::Nginx);
        cleanup(&dir);
    }

    #[test]
    fn detects_apache_debian_layout() {
        let dir = temp_dir("apache2");
        fs::create_dir_all(dir.join("apache2").join("sites-available")).expect("mkdir");
        assert_eq!(detect_web_server_in(&dir), WebServer::Apache);
        cleanup(&dir);
    }

    #[test]
    fn detects_apache_redhat_layout() {
        let dir = temp_dir("httpd");
        fs::create_dir_all(dir.join("httpd").join("conf.d")).expect("mkdir");
        assert_eq!(detect_web_server_in(&dir), WebServer::Apache);
        cleanup(&dir);
    }

    #[test]
    fn detects_none() {
        let dir = temp_dir("none");
        assert_eq!(detect_web_server_in(&dir), WebServer::None);
        cleanup(&dir);
    }

    #[test]
    fn renders_toml_that_parses_as_kernel_config() {
        use zcast_kernel::KernelConfig;

        let opts = SetupOptions {
            production: true,
            domain: Some("notes.example.com".to_owned()),
            site_title: "David's \"Notes\"".to_owned(),
            bind_addr: "127.0.0.1:8080".to_owned(),
            database_url: "sqlite:///var/lib/zcast/zcast.db?mode=rwc".to_owned(),
            color_mode: ThemeColorMode::Dark,
            auth_mode: "basic".to_owned(),
            admin_user: "david".to_owned(),
            admin_password: Some("hunter2".to_owned()),
            admin_email: Some("david@example.com".to_owned()),
            search_provider: "fts".to_owned(),
            config_path: PathBuf::from("/etc/zcast/zcast.toml"),
        };

        let rendered = render_zcast_toml(&opts);
        assert!(rendered.contains("[zcast]"));
        assert!(rendered.contains("site_title = \"David's \\\"Notes\\\"\""));
        assert!(rendered.contains("color_mode = \"dark\""));
        assert!(rendered.contains("auth = \"basic\""));
        assert!(rendered.contains("admin_password = \"hunter2\""));
        assert!(rendered.contains("notes.example.com"));
        assert!(rendered.contains("pinned_home_slug = \"index\""));

        let root: toml::Value = toml::from_str(&rendered).expect("valid TOML");
        assert!(root.get("zcast").is_some());

        #[derive(serde::Deserialize)]
        struct Root {
            zcast: KernelConfig,
        }
        let parsed: Root = toml::from_str(&rendered).expect("KernelConfig");
        let parsed = parsed.zcast;
        assert_eq!(parsed.site_title, "David's \"Notes\"");
        assert_eq!(parsed.bind_addr, "127.0.0.1:8080");
        assert_eq!(
            parsed.database_url,
            "sqlite:///var/lib/zcast/zcast.db?mode=rwc"
        );
        assert_eq!(parsed.color_mode, ThemeColorMode::Dark);
        assert_eq!(parsed.auth.as_deref(), Some("basic"));
        assert_eq!(parsed.admin_user, "david");
        assert_eq!(parsed.admin_password.as_deref(), Some("hunter2"));
        assert_eq!(parsed.pinned_home_slug, "index");
        assert!(!parsed.allow_unauthenticated_public);
    }

    #[test]
    fn local_toml_has_no_auth() {
        let opts = SetupOptions {
            production: false,
            domain: None,
            auth_mode: "none".to_owned(),
            admin_password: None,
            ..SetupOptions::default()
        };
        let rendered = render_zcast_toml(&opts);
        assert!(rendered.contains("auth = \"none\""));
        assert!(!rendered.contains("admin_password ="));
        let root: toml::Value = toml::from_str(&rendered).expect("valid TOML");
        assert!(root.get("zcast").is_some());
    }

    #[test]
    fn renders_caddy_snippet() {
        let snippet = render_vhost_snippet(WebServer::Caddy, "notes.example.com", "127.0.0.1:8080");
        assert!(snippet.contains("notes.example.com {"));
        assert!(snippet.contains("reverse_proxy 127.0.0.1:8080"));
        assert!(snippet.contains("systemctl reload caddy"));
    }

    #[test]
    fn renders_nginx_snippet_with_forwarded_proto() {
        let snippet = render_vhost_snippet(WebServer::Nginx, "notes.example.com", "127.0.0.1:8080");
        assert!(snippet.contains("server_name notes.example.com;"));
        assert!(snippet.contains("proxy_pass http://127.0.0.1:8080;"));
        assert!(snippet.contains("X-Forwarded-Proto $scheme"));
        assert!(snippet.contains("sites-available/zcast-notes.example.com"));
    }

    #[test]
    fn renders_apache_snippet() {
        let snippet =
            render_vhost_snippet(WebServer::Apache, "notes.example.com", "127.0.0.1:8080");
        assert!(snippet.contains("<VirtualHost *:80>"));
        assert!(snippet.contains("ServerName notes.example.com"));
        assert!(snippet.contains("ProxyPass / http://127.0.0.1:8080/"));
        assert!(snippet.contains("ProxyPassReverse / http://127.0.0.1:8080/"));
        assert!(snippet.contains("X-Forwarded-Proto"));
    }

    #[test]
    fn renders_none_snippet_with_instructions() {
        let snippet = render_vhost_snippet(WebServer::None, "notes.example.com", "127.0.0.1:8080");
        assert!(snippet.contains("No supported web server detected"));
        assert!(snippet.contains("127.0.0.1:8080"));
        assert!(snippet.contains("Caddy"));
        assert!(snippet.contains("Nginx"));
    }

    #[test]
    fn nginx_vhost_config_proxies_and_optionally_serves_acme() {
        let without = render_nginx_vhost_config("notes.example.com", "127.0.0.1:8080", false);
        assert!(without.contains("listen 80;"));
        assert!(without.contains("server_name notes.example.com;"));
        assert!(without.contains("proxy_pass http://127.0.0.1:8080;"));
        assert!(without.contains("X-Forwarded-Proto $scheme"));
        assert!(!without.contains(".well-known/acme-challenge"));

        let with = render_nginx_vhost_config("notes.example.com", "127.0.0.1:8080", true);
        assert!(
            with.contains("location /.well-known/acme-challenge/ { root /var/www/letsencrypt; }")
        );
        assert!(with.contains("proxy_pass http://127.0.0.1:8080;"));
        // Both `server {` and `location / {` open/close cleanly.
        assert_eq!(with.matches('{').count(), with.matches('}').count());
    }

    #[test]
    fn caddy_block_is_appended_once_and_skips_comments() {
        let existing = "# global options\n{\n    admin off\n}\n";
        let appended =
            append_caddy_block_if_missing(existing, "notes.example.com", "127.0.0.1:8080")
                .expect("block appended");
        assert!(appended.contains("notes.example.com {"));
        assert!(appended.contains("reverse_proxy 127.0.0.1:8080"));
        assert!(appended.contains("encode zstd gzip"));
        assert!(appended.starts_with(existing));

        // Re-running against its own output is a no-op.
        assert!(
            append_caddy_block_if_missing(&appended, "notes.example.com", "127.0.0.1:8080")
                .is_none()
        );

        // Commented-out blocks do not count as configured.
        let commented = "# notes.example.com {\n#     reverse_proxy 127.0.0.1:8080\n# }\n";
        assert!(
            append_caddy_block_if_missing(commented, "notes.example.com", "127.0.0.1:8080")
                .is_some()
        );

        // A different domain is preserved and the new block is appended.
        let other = "other.example.com {\n    reverse_proxy 127.0.0.1:9000\n}\n";
        let both = append_caddy_block_if_missing(other, "notes.example.com", "127.0.0.1:8080")
            .expect("appended alongside other domains");
        assert!(both.contains("other.example.com {"));
        assert!(both.contains("notes.example.com {"));

        // Comma-separated site addresses are recognised as already configured.
        let multi =
            "notes.example.com, www.notes.example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n";
        assert!(
            append_caddy_block_if_missing(multi, "notes.example.com", "127.0.0.1:8080").is_none()
        );

        // An empty domain never produces a block.
        assert!(append_caddy_block_if_missing("", "  ", "127.0.0.1:8080").is_none());
    }

    #[test]
    fn provision_report_defaults_to_nothing_done() {
        let report = ProvisionReport::default();
        assert!(report.steps.is_empty());
        assert!(report.warnings.is_empty());
        assert!(!report.vhost_configured);
        assert!(!report.tls_configured);
        assert!(!report.service_started);
        assert!(!report.health_ok);
    }

    #[test]
    fn certbot_recovery_command_includes_email_or_flag() {
        assert_eq!(
            certbot_command_for("nginx", "notes.example.com", Some("admin@example.com")),
            "certbot --nginx --non-interactive --agree-tos --redirect -d notes.example.com --email admin@example.com"
        );
        assert_eq!(
            certbot_command_for("apache", "notes.example.com", None),
            "certbot --apache --non-interactive --agree-tos --redirect -d notes.example.com --register-unsafely-without-email"
        );
    }

    #[test]
    fn renders_hardened_systemd_unit() {
        let unit = render_systemd_unit(
            "/usr/local/bin/zcast",
            "/etc/zcast/zcast.toml",
            "/var/lib/zcast",
        );
        assert!(
            unit.contains("ExecStart=/usr/local/bin/zcast --config /etc/zcast/zcast.toml serve")
        );
        assert!(unit.contains("User=zcast"));
        assert!(unit.contains("Group=zcast"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("NoNewPrivileges=true"));
        assert!(unit.contains("WorkingDirectory=/var/lib/zcast"));
        assert!(unit.contains("ReadWritePaths=/var/lib/zcast"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn express_wizard_accepts_defaults() {
        let defaults = SetupOptions {
            production: false,
            domain: Some("notes.example.com".to_owned()),
            config_path: PathBuf::from("/tmp/zcast.toml"),
            ..SetupOptions::default()
        };
        // Usage: production; domain: accept default; proceed: express.
        let input = b"1\n\n1\n";
        let mut reader = &input[..];
        let mut output: Vec<u8> = Vec::new();

        let opts = prompt_interactive_with(defaults, WebServer::Caddy, &mut reader, &mut output)
            .expect("wizard runs");
        let text = String::from_utf8(output).expect("utf-8 output");

        assert!(opts.production);
        assert_eq!(opts.domain.as_deref(), Some("notes.example.com"));
        assert_eq!(opts.auth_mode, "basic");
        assert_eq!(opts.bind_addr, "127.0.0.1:3000");
        assert!(text.contains("Express setup plan:"));
        assert!(text.contains("Detected web server: Caddy"));
        assert!(text.contains("Use defaults above") || text.contains("Express setup"));
    }

    #[test]
    fn local_wizard_skips_domain_and_auth() {
        let defaults = SetupOptions {
            domain: Some("ignored.example.com".to_owned()),
            ..SetupOptions::default()
        };
        let input = b"2\n";
        let mut reader = &input[..];
        let mut output: Vec<u8> = Vec::new();

        let opts = prompt_interactive_with(defaults, WebServer::Nginx, &mut reader, &mut output)
            .expect("wizard runs");
        let text = String::from_utf8(output).expect("utf-8 output");

        assert!(!opts.production);
        assert_eq!(opts.bind_addr, "127.0.0.1:3000");
        assert_eq!(opts.auth_mode, "none");
        assert!(!text.contains("Domain name"));
    }

    #[test]
    fn advanced_wizard_collects_custom_values() {
        let defaults = SetupOptions {
            domain: Some("notes.example.com".to_owned()),
            config_path: PathBuf::from("/tmp/zcast.toml"),
            ..SetupOptions::default()
        };
        // production, domain default, advanced, title, db, auth, theme, search, bind.
        let input = b"1\n\n2\nMy Notes\nsqlite:///srv/zcast.db?mode=rwc\nbasic\ndark\nfts\n127.0.0.1:9090\n";
        let mut reader = &input[..];
        let mut output: Vec<u8> = Vec::new();

        let opts = prompt_interactive_with(defaults, WebServer::Apache, &mut reader, &mut output)
            .expect("wizard runs");

        assert!(opts.production);
        assert_eq!(opts.site_title, "My Notes");
        assert_eq!(opts.database_url, "sqlite:///srv/zcast.db?mode=rwc");
        assert_eq!(opts.auth_mode, "basic");
        assert_eq!(opts.color_mode, ThemeColorMode::Dark);
        assert_eq!(opts.search_provider, "fts");
        assert_eq!(opts.bind_addr, "127.0.0.1:9090");
    }

    #[test]
    fn wizard_handles_eof_by_accepting_defaults() {
        let defaults = SetupOptions::default();
        let input = b"";
        let mut reader = &input[..];
        let mut output: Vec<u8> = Vec::new();

        let opts = prompt_interactive_with(defaults, WebServer::None, &mut reader, &mut output)
            .expect("wizard runs");
        assert!(opts.production);
        assert_eq!(opts.auth_mode, "basic");
    }
}
