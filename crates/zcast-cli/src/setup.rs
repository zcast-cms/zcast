//! Interactive `zcast setup` wizard, web-server detection, and generated
//! production artefacts (`zcast.toml`, vhost snippets, hardened systemd unit).
//!
//! The wizard is Express-first: production is the default, and a single
//! confirmation screen covers the common path. Advanced setup walks through the
//! database, auth, theme, search provider and bind address.
#![allow(dead_code)] // Wired into `main.rs` by the coordinator.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use zcast_interfaces::ThemeColorMode;

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
