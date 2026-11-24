mod modules;
mod setup;
mod update;

use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use zcast_auth_basic::BasicAuthProvider;
use zcast_auth_magic_link::MagicLinkAuthProvider;
use zcast_cmd_k::CommandBar;
use zcast_editor_inline::InlineEditor;
use zcast_interfaces::{
    LinkParser, NoteExporter, NoteImporter, NoteListFilter, ReadContext, SearchProvider, Storage,
    ThemeColorMode,
};
use zcast_io_markdown::MarkdownIo;
use zcast_kernel::{KernelConfig, ZcastKernelBuilder};
use zcast_links_wikilink::WikilinkParser;
use zcast_popover::HoverPopover;
use zcast_search_fts::FtsSearchProvider;
use zcast_search_onnx::OnnxSearchProvider;
use zcast_storage_mysql::MysqlStorage;
use zcast_storage_sqlite::SqliteStorage;
use zcast_theme_davidhewitson::DavidHewitsonTheme;

#[derive(Parser, Debug)]
#[command(name = "zcast", version, about = "Fast, modular Rust Zettelkasten CMS")]
struct Cli {
    /// Optional path to zcast.toml config file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the zcast HTTP server
    Serve {
        /// Bind address (default: 127.0.0.1:3000)
        #[arg(long)]
        bind: Option<String>,
        /// Database URL: sqlite://... or mysql://... (default: sqlite://zcast.db?mode=rwc)
        #[arg(long)]
        db: Option<String>,
        /// Site title
        #[arg(long)]
        title: Option<String>,
        /// Theme colour mode: auto, dark, or light
        #[arg(long)]
        theme_mode: Option<String>,
        /// Auth provider: none (local-only), basic, or magic-link
        #[arg(long)]
        auth: Option<String>,
        /// Admin username for basic auth (default: admin)
        #[arg(long)]
        admin_user: Option<String>,
        /// Admin password for basic auth (or set ZCAST_ADMIN_PASSWORD)
        #[arg(long)]
        admin_password: Option<String>,
        /// Admin email for magic-link auth (or set ZCAST_ADMIN_EMAIL)
        #[arg(long)]
        admin_email: Option<String>,
        /// Public base URL for magic-link callbacks (or set ZCAST_PUBLIC_URL)
        #[arg(long)]
        public_url: Option<String>,
        /// Search provider: fts (default), onnx (semantic hybrid), or none
        #[arg(long)]
        search: Option<String>,
        /// Explicitly allow binding to a non-loopback address without an auth module
        #[arg(long)]
        allow_unauthenticated_public: bool,
    },
    /// Guided Express / Advanced setup wizard for production or local use
    Setup {
        /// Run non-interactive Express production setup
        #[arg(long)]
        express: bool,
        /// Run non-interactive local-only setup (127.0.0.1:3000, no auth)
        #[arg(long)]
        local: bool,
        /// Public domain name (e.g. notes.davidhewitson.co.uk)
        #[arg(long)]
        domain: Option<String>,
        /// Site title
        #[arg(long)]
        title: Option<String>,
        /// Bind address
        #[arg(long)]
        bind: Option<String>,
        /// Database URL (sqlite://... or mysql://...)
        #[arg(long)]
        db: Option<String>,
        /// Theme colour mode: auto, dark, or light
        #[arg(long)]
        theme_mode: Option<String>,
        /// Auth provider: none, basic, or magic-link
        #[arg(long)]
        auth: Option<String>,
        /// Admin username for basic auth
        #[arg(long)]
        admin_user: Option<String>,
        /// Admin password for basic auth
        #[arg(long)]
        admin_password: Option<String>,
        /// Admin email for magic-link auth
        #[arg(long)]
        admin_email: Option<String>,
        /// Search provider: fts, onnx, or none
        #[arg(long)]
        search: Option<String>,
        /// Where to write zcast.toml (default: ./zcast.toml)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print systemd unit file for production deployment
        #[arg(long)]
        print_systemd: bool,
    },
    /// Safely back up SQLite and update zcast (with SHA-256 verification & auto-rollback)
    Update {
        /// Check / display update plan without downloading or replacing binaries
        #[arg(long)]
        check: bool,
        /// Target release version tag (e.g. v0.4.1; defaults to latest release)
        #[arg(long)]
        version: Option<String>,
        /// GitHub repository for releases
        #[arg(long, default_value = update::DEFAULT_RELEASE_REPO)]
        repo: String,
        /// Directory for automatic SQLite backups
        #[arg(long)]
        backup_dir: Option<PathBuf>,
        /// Optional CARGO_BUILD_JOBS for custom-module rebuilds
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Inspect, graph, bind, install, update, or remove zcast modules
    Module {
        #[command(subcommand)]
        action: ModuleAction,
    },
    /// Authentication provider CLI helpers (e.g. SSH magic-link recovery)
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Export notes to a Markdown directory or .zip archive preserving [[wikilinks]]
    Export {
        /// Target directory to write <slug>.md files into
        #[arg(long, conflicts_with = "zip")]
        out: Option<PathBuf>,
        /// Target .zip file path
        #[arg(long, conflicts_with = "out")]
        zip: Option<PathBuf>,
        /// Exclude private notes from the export
        #[arg(long)]
        public_only: bool,
        /// Database URL override
        #[arg(long)]
        db: Option<String>,
    },
    /// Import Markdown notes from a directory or .zip archive
    Import {
        /// Path to a directory of .md files or a .zip archive
        path: PathBuf,
        /// Database URL override
        #[arg(long)]
        db: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ModuleAction {
    /// List compiled modules and active interface bindings
    List,
    /// Print the interface dependency graph (ASCII or JSON)
    Graph {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Bind an interface slot to a module in zcast-modules.toml
    Bind {
        /// Interface name (e.g. search, auth, storage, or full zcast:search/provider)
        interface: String,
        /// Target module ID (e.g. zcast-search-onnx, zcast-auth-magic-link)
        module_id: String,
    },
    /// Add a module crate to zcast-modules.toml and rebuild a custom zcast binary
    Install {
        /// Crate spec: name, name@version, name=git+url, or name=path:../crate
        spec: String,
        /// Parallel Cargo build jobs
        #[arg(long)]
        jobs: Option<usize>,
        /// Generate shim crate and print plan without invoking cargo build
        #[arg(long)]
        dry_run: bool,
    },
    /// Re-enable a previously deactivated bundled module (no rebuild needed)
    Activate {
        /// Module ID (e.g. zcast-search-onnx, zcast-popover)
        name: String,
    },
    /// Deactivate a bundled module without removing it from the lock file
    Deactivate {
        /// Module ID (e.g. zcast-search-fts, zcast-popover)
        name: String,
    },
    /// Remove an extra module crate from zcast-modules.toml and rebuild
    Remove {
        /// Crate/module name to remove
        name: String,
        /// Parallel Cargo build jobs
        #[arg(long)]
        jobs: Option<usize>,
        /// Generate shim crate and print plan without invoking cargo build
        #[arg(long)]
        dry_run: bool,
    },
    /// Rebuild the custom binary with updated module crates
    Update {
        /// Parallel Cargo build jobs
        #[arg(long)]
        jobs: Option<usize>,
        /// Generate shim crate and print plan without invoking cargo build
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AuthAction {
    /// Generate a one-time SSH recovery magic login URL (zcast-auth-magic-link)
    LoginLink {
        /// Base URL prefix (e.g. https://notes.davidhewitson.co.uk)
        #[arg(long)]
        base_url: Option<String>,
        /// Admin email (falls back to ZCAST_ADMIN_EMAIL / config / admin@localhost)
        #[arg(long)]
        email: Option<String>,
        /// Database URL override
        #[arg(long)]
        db: Option<String>,
    },
}

async fn connect_storage(
    database_url: &str,
) -> Result<Arc<dyn Storage>, Box<dyn std::error::Error>> {
    if database_url.trim().starts_with("mysql://") {
        let mysql = MysqlStorage::connect(database_url).await?;
        Ok(Arc::new(mysql))
    } else {
        let sqlite = SqliteStorage::connect(database_url).await?;
        Ok(Arc::new(sqlite))
    }
}

fn sqlite_path_from_url(url: &str) -> Option<PathBuf> {
    let trimmed = url.trim();
    if trimmed.contains(":memory:") || trimmed.contains("mode=memory") {
        return None;
    }
    let rest = trimmed.strip_prefix("sqlite://").unwrap_or(trimmed);
    let path_part = rest.split('?').next().unwrap_or(rest).trim();
    if path_part.is_empty() || path_part.starts_with("mysql://") {
        None
    } else {
        Some(PathBuf::from(path_part))
    }
}

fn module_lock_path(config_path: Option<&Path>) -> PathBuf {
    match config_path.and_then(Path::parent) {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(modules::MODULE_LOCK_FILE),
        _ => PathBuf::from(modules::MODULE_LOCK_FILE),
    }
}

fn load_or_default_module_lock(path: &Path) -> modules::ModuleLockConfig {
    if path.exists()
        && let Ok(cfg) = modules::ModuleLockConfig::load(path)
    {
        return cfg;
    }
    modules::ModuleLockConfig {
        core_version: env!("CARGO_PKG_VERSION").to_owned(),
        bindings: BTreeMap::new(),
        extra_crates: Vec::new(),
        disabled: Vec::new(),
    }
}

fn build_search_provider(choice: &str) -> Result<Option<Arc<dyn SearchProvider>>, String> {
    match choice.trim().to_ascii_lowercase().as_str() {
        "" | "fts" | "zcast-search-fts" => Ok(Some(Arc::new(FtsSearchProvider::new()))),
        "onnx" | "semantic" | "zcast-search-onnx" => Ok(Some(Arc::new(OnnxSearchProvider::new()))),
        "none" | "off" => Ok(None),
        other => Err(format!(
            "unknown search provider '{other}' (expected 'fts', 'onnx', or 'none')"
        )),
    }
}

/// Resolve the effective search choice, honouring deactivated modules.
///
/// A CLI `--search` flag always wins (explicit intent). Otherwise the configured
/// value is used unless that provider has been deactivated, in which case the
/// lock file's active search binding (if any) takes over.
fn resolve_search_choice(
    explicit: bool,
    configured: Option<&str>,
    lock_cfg: &modules::ModuleLockConfig,
) -> String {
    let configured = configured.map(str::trim).filter(|s| !s.is_empty());
    if explicit {
        return configured.unwrap_or("fts").to_owned();
    }
    if let Some(choice) = configured {
        if matches!(choice.to_ascii_lowercase().as_str(), "none" | "off") {
            return "none".to_owned();
        }
        if !lock_cfg.is_disabled(modules::normalize_search_module_id(choice)) {
            return choice.to_owned();
        }
    }
    lock_cfg
        .active_search_module()
        .map(str::to_owned)
        .unwrap_or_else(|| "none".to_owned())
}

fn canonical_interface_name(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "storage" | "zcast:storage/repository" => zcast_interfaces::IFACE_STORAGE,
        "links" | "zcast:links/parser" => zcast_interfaces::IFACE_LINKS,
        "theme" | "zcast:ui/theme" => zcast_interfaces::IFACE_THEME,
        "editor" | "zcast:ui/editor" => zcast_interfaces::IFACE_EDITOR,
        "search" | "zcast:search/provider" => zcast_interfaces::IFACE_SEARCH,
        "auth" | "zcast:auth/provider" => zcast_interfaces::IFACE_AUTH,
        "popover" | "zcast:ui/popover" => zcast_interfaces::IFACE_POPOVER,
        "cmd-k" | "zcast:ui/cmd-k" => zcast_interfaces::IFACE_CMD_K,
        "io" | "zcast:io/markdown" => zcast_interfaces::IFACE_IO,
        _ => "",
    }
}

/// Print the deactivated-module section for `module list` / `module graph`.
fn print_deactivated_modules(lock_cfg: &modules::ModuleLockConfig) {
    if lock_cfg.disabled.is_empty() {
        return;
    }
    println!("\nDeactivated Modules:");
    for name in &lock_cfg.disabled {
        println!("  - {name}");
        println!("    (re-enable with: zcast module activate {name})");
    }
}

async fn build_introspection_kernel(
    config: KernelConfig,
    lock_cfg: &modules::ModuleLockConfig,
) -> Result<Arc<zcast_kernel::ZcastKernel>, Box<dyn std::error::Error>> {
    let storage = Arc::new(SqliteStorage::connect("sqlite::memory:").await?);
    let links = Arc::new(WikilinkParser::new());
    let theme = Arc::new(DavidHewitsonTheme::new());

    let mut builder = ZcastKernelBuilder::new(config.clone(), storage.clone(), links, theme);

    if !lock_cfg.is_disabled("zcast-editor-inline") {
        builder = builder.with_editor(Arc::new(InlineEditor::new()));
    }
    if !lock_cfg.is_disabled("zcast-popover") {
        builder = builder.with_ui_extension(Arc::new(HoverPopover::new()));
    }
    if !lock_cfg.is_disabled("zcast-io-markdown") {
        let io = Arc::new(MarkdownIo::new());
        builder = builder.with_exporter(io.clone()).with_importer(io);
    }

    let search_choice = resolve_search_choice(false, config.search.as_deref(), lock_cfg);
    if let Some(search) = build_search_provider(&search_choice)? {
        builder = builder.with_search(search);
        if !lock_cfg.is_disabled("zcast-cmd-k") {
            builder = builder.with_ui_extension(Arc::new(CommandBar::new()));
        }
    }

    let auth_mode = config.auth.as_deref().unwrap_or("basic");
    let auth_disabled = modules::normalize_auth_module_id(auth_mode)
        .is_some_and(|module| lock_cfg.is_disabled(module));
    if !auth_disabled {
        match auth_mode {
            "magic-link" | "zcast-auth-magic-link" => {
                let email = config
                    .admin_email
                    .clone()
                    .unwrap_or_else(|| "admin@localhost".to_owned());
                builder = builder.with_auth(Arc::new(MagicLinkAuthProvider::new(storage, email)));
            }
            "none" => {}
            _ => {
                builder = builder.with_auth(Arc::new(BasicAuthProvider::new(storage)));
            }
        }
    }

    Ok(builder.build().await?)
}

fn fetch_latest_release_tag(repo: &str) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: zcast-updater",
            &url,
        ])
        .output()
        .map_err(|e| format!("failed to run curl to check releases: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "failed to query latest release from {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("invalid GitHub release JSON: {e}"))?;
    json["tag_name"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| "latest release response had no tag_name".to_owned())
}

fn curl_download(url: &str, dest: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .args(["-fsSL", url, "-o"])
        .arg(dest)
        .status()
        .map_err(|e| format!("failed to run curl for {url}: {e}"))?;
    if !status.success() {
        return Err(format!("download failed for {url}"));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mut config = KernelConfig::load(cli.config.as_deref())?;
    let lock_path = module_lock_path(cli.config.as_deref());
    let lock_cfg = load_or_default_module_lock(&lock_path);

    // Apply persisted module slot bindings from zcast-modules.toml if CLI/env didn't override.
    if config.search.is_none()
        && let Some(bound) = lock_cfg.bindings.get(zcast_interfaces::IFACE_SEARCH)
    {
        config.search = Some(bound.clone());
    }
    if config.auth.is_none()
        && let Some(bound) = lock_cfg.bindings.get(zcast_interfaces::IFACE_AUTH)
    {
        config.auth = Some(bound.clone());
    }

    match cli.command {
        Commands::Serve {
            bind,
            db,
            title,
            theme_mode,
            auth,
            admin_user,
            admin_password,
            admin_email,
            public_url,
            search,
            allow_unauthenticated_public,
        } => {
            let search_explicit = search.is_some();
            let auth_explicit = auth.is_some();

            if let Some(b) = bind {
                config.bind_addr = b;
            }
            if let Some(d) = db {
                config.database_url = d;
            }
            if let Some(t) = title {
                config.site_title = t;
            }
            if let Some(m) = theme_mode {
                config.color_mode = ThemeColorMode::parse(&m);
            }
            if let Some(a) = auth {
                config.auth = Some(a.trim().to_ascii_lowercase());
            }
            if let Some(u) = admin_user {
                config.admin_user = u.trim().to_owned();
            }
            if let Some(p) = admin_password {
                config.admin_password = Some(p);
            }
            if let Some(e) = admin_email {
                config.admin_email = Some(e.trim().to_owned());
            }
            if let Some(u) = public_url {
                config.public_url = Some(u.trim().to_owned());
            }
            if let Some(s) = search {
                config.search = Some(s.trim().to_ascii_lowercase());
            }
            if allow_unauthenticated_public {
                config.allow_unauthenticated_public = true;
            }

            let auth_mode = config
                .auth
                .as_deref()
                .map(str::to_ascii_lowercase)
                .unwrap_or_else(|| {
                    if config.admin_email.is_some() {
                        "magic-link".to_owned()
                    } else if config.admin_password.is_some() {
                        "basic".to_owned()
                    } else {
                        "none".to_owned()
                    }
                });
            // A deactivated auth module is skipped unless the CLI asked for it.
            let auth_mode = match modules::normalize_auth_module_id(&auth_mode) {
                Some(module) if !auth_explicit && lock_cfg.is_disabled(module) => "none".to_owned(),
                _ => auth_mode,
            };

            let search_mode =
                resolve_search_choice(search_explicit, config.search.as_deref(), &lock_cfg);

            let storage = connect_storage(&config.database_url).await?;
            let links = Arc::new(WikilinkParser::new());
            let theme = Arc::new(DavidHewitsonTheme::new());

            let mut builder =
                ZcastKernelBuilder::new(config.clone(), storage.clone(), links, theme);

            if !lock_cfg.is_disabled("zcast-editor-inline") {
                builder = builder.with_editor(Arc::new(InlineEditor::new()));
            }
            if !lock_cfg.is_disabled("zcast-popover") {
                builder = builder.with_ui_extension(Arc::new(HoverPopover::new()));
            }
            if !lock_cfg.is_disabled("zcast-io-markdown") {
                let io = Arc::new(MarkdownIo::new());
                builder = builder.with_exporter(io.clone()).with_importer(io);
            }

            if let Some(search_provider) = build_search_provider(&search_mode)? {
                builder = builder.with_search(search_provider);
                if !lock_cfg.is_disabled("zcast-cmd-k") {
                    builder = builder.with_ui_extension(Arc::new(CommandBar::new()));
                }
            }

            let mut basic_auth: Option<Arc<BasicAuthProvider>> = None;
            let auth_label: &str;
            match auth_mode.as_str() {
                "none" | "" => {
                    auth_label = "local-only (none)";
                }
                "basic" | "zcast-auth-basic" => {
                    let provider = Arc::new(BasicAuthProvider::new(storage.clone()));
                    basic_auth = Some(provider.clone());
                    builder = builder.with_auth(provider);
                    auth_label = "basic";
                }
                "magic-link" | "zcast-auth-magic-link" => {
                    let email = config
                        .admin_email
                        .clone()
                        .unwrap_or_else(|| "admin@localhost".to_owned());
                    let provider = Arc::new(MagicLinkAuthProvider::new(storage.clone(), email));
                    builder = builder.with_auth(provider);
                    auth_label = "magic-link";
                }
                other => {
                    return Err(format!(
                        "unknown auth mode '{other}' (expected 'none', 'basic', or 'magic-link')"
                    )
                    .into());
                }
            }

            let kernel = builder.build().await?;

            if let Some(provider) = basic_auth
                && let Some(generated) = provider
                    .ensure_admin_credentials(
                        &kernel.config.admin_user,
                        kernel.config.admin_password.as_deref(),
                    )
                    .await?
            {
                println!("\n  Admin credentials initialized (shown once):");
                println!(
                    "    Login URL: http://{}/_zcast/login",
                    kernel.config.bind_addr
                );
                println!("    Username:  {}", kernel.config.admin_user);
                println!("    Password:  {generated}\n");
            }

            let addr = kernel.config.validate_bind_safety(kernel.has_auth())?;
            let app = zcast_kernel::http::router(kernel.clone());
            let settings = kernel.app_settings();

            tracing::info!(
                "zcast v{} listening on http://{} (title: '{}', theme: {}, auth: {}, search: {})",
                env!("CARGO_PKG_VERSION"),
                addr,
                settings.site_title,
                settings.color_mode.as_str(),
                auth_label,
                search_mode,
            );

            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
        }

        Commands::Setup {
            express,
            local,
            domain,
            title,
            bind,
            db,
            theme_mode,
            auth,
            admin_user,
            admin_password,
            admin_email,
            search,
            out,
            print_systemd,
        } => {
            let detected = setup::detect_web_server();
            let config_path = out
                .or(cli.config)
                .unwrap_or_else(|| PathBuf::from("zcast.toml"));

            let non_interactive = express || local;
            let defaults = setup::SetupOptions {
                production: !local,
                domain,
                site_title: title.unwrap_or_else(|| "Zcast".to_owned()),
                bind_addr: bind.unwrap_or_else(|| {
                    if local {
                        "127.0.0.1:3000".to_owned()
                    } else {
                        "127.0.0.1:8080".to_owned()
                    }
                }),
                database_url: db.unwrap_or_else(|| "sqlite://zcast.db?mode=rwc".to_owned()),
                color_mode: theme_mode
                    .as_deref()
                    .map(ThemeColorMode::parse)
                    .unwrap_or(ThemeColorMode::Auto),
                auth_mode: auth.unwrap_or_else(|| {
                    if local {
                        "none".to_owned()
                    } else {
                        "basic".to_owned()
                    }
                }),
                admin_user: admin_user.unwrap_or_else(|| "admin".to_owned()),
                admin_password,
                admin_email,
                search_provider: search.unwrap_or_else(|| "fts".to_owned()),
                config_path: config_path.clone(),
            };

            let opts = if non_interactive {
                defaults
            } else {
                setup::prompt_interactive_setup(defaults, detected)?
            };

            if let Some(parent) = opts.config_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            let rendered_toml = setup::render_zcast_toml(&opts);
            std::fs::write(&opts.config_path, &rendered_toml)?;
            println!("◇  Wrote configuration to {}", opts.config_path.display());

            // Initialize storage and seed starter card immediately.
            let storage = connect_storage(&opts.database_url).await?;
            let kernel_cfg = KernelConfig::load(Some(&opts.config_path))?;
            let links = Arc::new(WikilinkParser::new());
            let theme = Arc::new(DavidHewitsonTheme::new());
            let _kernel = ZcastKernelBuilder::new(kernel_cfg, storage.clone(), links, theme)
                .build()
                .await?;
            println!(
                "◇  Initialized database ({}) & seeded starter card \"Index\"",
                opts.database_url
            );

            let mut generated_pw = None;
            if opts.auth_mode.eq_ignore_ascii_case("basic") {
                let auth = BasicAuthProvider::new(storage.clone());
                generated_pw = auth
                    .ensure_admin_credentials(&opts.admin_user, opts.admin_password.as_deref())
                    .await?;
            }

            println!("\n└  Setup complete!\n");
            if let Some(domain) = opts.domain.as_deref().filter(|d| !d.is_empty()) {
                println!("   Site:      https://{domain}");
                println!("   Admin:     https://{domain}/_zcast/login");
            } else {
                println!("   Site:      http://{}", opts.bind_addr);
                if opts.auth_mode != "none" {
                    println!("   Admin:     http://{}/_zcast/login", opts.bind_addr);
                }
            }
            if opts.auth_mode.eq_ignore_ascii_case("basic") {
                println!("   Username:  {}", opts.admin_user);
                if let Some(pw) = generated_pw {
                    println!("   Password:  {pw}   (save this — shown once)");
                }
            } else if opts.auth_mode.eq_ignore_ascii_case("magic-link") {
                println!(
                    "   Auth:      magic-link ({})",
                    opts.admin_email.as_deref().unwrap_or("admin@localhost")
                );
                println!(
                    "   Recovery:  zcast --config {} auth login-link",
                    opts.config_path.display()
                );
            }

            if opts.production
                && let Some(domain) = opts.domain.as_deref().filter(|d| !d.is_empty())
            {
                println!(
                    "\n   Reverse proxy snippet (detected: {}):\n{}",
                    detected.as_str(),
                    setup::render_vhost_snippet(detected, domain, &opts.bind_addr)
                );
            }

            if print_systemd {
                let bin = std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "/usr/local/bin/zcast".to_owned());
                println!(
                    "\n   systemd unit (/etc/systemd/system/zcast.service):\n{}",
                    setup::render_systemd_unit(
                        &bin,
                        &opts.config_path.display().to_string(),
                        "/var/lib/zcast",
                    )
                );
            }

            println!(
                "\n   Start zcast now:\n     zcast --config {} serve\n",
                opts.config_path.display()
            );
        }

        Commands::Update {
            check,
            version,
            repo,
            backup_dir,
            jobs,
        } => {
            let current = env!("CARGO_PKG_VERSION");
            let has_custom_modules = !lock_cfg.extra_crates.is_empty();

            let target_version = match version {
                Some(v) => v,
                None => fetch_latest_release_tag(&repo).unwrap_or_else(|_| format!("v{current}")),
            };

            let plan =
                update::build_update_plan(&repo, current, &target_version, has_custom_modules);

            println!("zcast update plan:");
            println!("  Current version: v{}", plan.current_version);
            println!("  Target version:  v{}", plan.target_version);
            println!("  Target triple:   {}", plan.target_triple);
            println!(
                "  Update strategy: {}",
                if plan.is_custom_module_build {
                    "custom-module incremental rebuild (warm target/)"
                } else {
                    "prebuilt static binary download + SHA-256 verification"
                }
            );
            if !plan.is_custom_module_build {
                println!("  Download URL:    {}", plan.download_url);
                println!("  Checksums URL:   {}", plan.checksums_url);
            }

            if check {
                if update::is_newer_version(&plan.current_version, &plan.target_version) {
                    println!(
                        "\nUpdate available: v{} -> v{}",
                        plan.current_version, plan.target_version
                    );
                } else {
                    println!("\nzcast is up to date (v{}).", plan.current_version);
                }
                return Ok(());
            }

            // Step 1: Automatic hot SQLite snapshot before touching binaries.
            if let Some(db_path) = sqlite_path_from_url(&config.database_url)
                && db_path.exists()
            {
                let bdir = backup_dir.unwrap_or_else(|| PathBuf::from("backups"));
                let snapshot = update::backup_sqlite_db(&db_path, &bdir).await?;
                println!("◇  Created SQLite backup at {}", snapshot.display());
            }

            let current_exe = std::env::current_exe()?;

            // Step 2: Either rebuild custom shim binary or download & verify prebuilt archive.
            if plan.is_custom_module_build {
                let build_dir = config
                    .data_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(".zcast-build"));
                let mut updated_lock = lock_cfg.clone();
                updated_lock.core_version = plan.target_version.clone();
                let new_bin = modules::build_custom_binary(&build_dir, &updated_lock, jobs, false)?;
                let prev = modules::atomic_swap_binary(&new_bin, &current_exe)?;
                updated_lock.save(&lock_path)?;
                println!(
                    "◇  Swapped custom binary atomically (previous kept at {})",
                    prev.display()
                );
            } else {
                let tmpdir = std::env::temp_dir().join(format!(
                    "zcast-update-{}",
                    chrono::Utc::now().timestamp_millis()
                ));
                std::fs::create_dir_all(&tmpdir)?;
                let archive_name = format!("zcast-{}.tar.gz", plan.target_triple);
                let archive_path = tmpdir.join(&archive_name);
                let sums_path = tmpdir.join(update::CHECKSUMS_ASSET);

                curl_download(&plan.download_url, &archive_path)?;
                curl_download(&plan.checksums_url, &sums_path)?;

                let sums = std::fs::read_to_string(&sums_path)?;
                let expected = update::parse_sha256sums(&sums, &archive_name)
                    .ok_or_else(|| format!("missing SHA256SUMS entry for {archive_name}"))?;
                let archive_bytes = std::fs::read(&archive_path)?;
                if !update::verify_sha256_bytes(&archive_bytes, &expected) {
                    let _ = std::fs::remove_dir_all(&tmpdir);
                    return Err("SHA-256 checksum verification failed; aborting update".into());
                }
                println!("◇  Verified SHA-256 checksum");

                let extract_status = Command::new("tar")
                    .args(["-xzf"])
                    .arg(&archive_path)
                    .arg("-C")
                    .arg(&tmpdir)
                    .status()?;
                if !extract_status.success() {
                    let _ = std::fs::remove_dir_all(&tmpdir);
                    return Err("failed to extract release archive".into());
                }

                let new_bin = tmpdir.join("zcast");
                let prev = modules::atomic_swap_binary(&new_bin, &current_exe)?;
                let _ = std::fs::remove_dir_all(&tmpdir);

                // Smoke-verify the swapped binary; roll back immediately on failure!
                let ok = Command::new(&current_exe)
                    .arg("--version")
                    .status()
                    .is_ok_and(|s| s.success());
                if !ok {
                    modules::rollback_binary(&prev, &current_exe)?;
                    return Err(
                        "updated binary failed verification; rolled back to previous binary".into(),
                    );
                }

                println!(
                    "◇  Updated zcast to v{} (previous kept at {})",
                    plan.target_version,
                    prev.display()
                );
            }
        }

        Commands::Module { action } => match action {
            ModuleAction::List => {
                let kernel = build_introspection_kernel(config, &lock_cfg).await?;
                let snap = kernel.registry_snapshot();

                println!("Compiled Modules:");
                for m in &snap.modules {
                    let provides: Vec<String> = m
                        .provides
                        .iter()
                        .map(|p| format!("{}@{}", p.name, p.version))
                        .collect();
                    let requires: Vec<String> = m
                        .requires
                        .iter()
                        .map(|r| format!("{}@{}", r.name, r.version_req))
                        .collect();
                    if requires.is_empty() {
                        println!(
                            "  - {} v{}  provides [{}]",
                            m.id,
                            m.version,
                            provides.join(", ")
                        );
                    } else {
                        println!(
                            "  - {} v{}  provides [{}]  requires [{}]",
                            m.id,
                            m.version,
                            provides.join(", "),
                            requires.join(", ")
                        );
                    }
                }
                println!("\nActive Interface Bindings:");
                for b in &snap.bindings {
                    println!(
                        "  - {} -> {} ({})",
                        b.interface, b.module_id, b.module_version
                    );
                }
                print_deactivated_modules(&lock_cfg);
            }
            ModuleAction::Graph { json } => {
                let kernel = build_introspection_kernel(config, &lock_cfg).await?;
                let snap = kernel.registry_snapshot();
                if json {
                    println!("{}", serde_json::to_string_pretty(&snap)?);
                } else {
                    print!("{}", modules::render_module_graph_ascii(&snap));
                    print_deactivated_modules(&lock_cfg);
                }
            }
            ModuleAction::Bind {
                interface,
                module_id,
            } => {
                let iface = canonical_interface_name(&interface);
                if iface.is_empty() {
                    return Err(format!(
                        "unknown interface '{interface}' (expected storage, links, theme, editor, search, auth, popover, cmd-k, or io)"
                    )
                    .into());
                }
                let mut updated = lock_cfg.clone();
                updated.bindings.insert(iface.to_owned(), module_id.clone());
                updated.save(&lock_path)?;
                println!("Bound {iface} -> {module_id} in {}", lock_path.display());
            }
            ModuleAction::Install {
                spec,
                jobs,
                dry_run,
            } => {
                let parsed = modules::parse_module_spec(&spec)?;

                // Bundled modules are already compiled in: just re-enable them.
                if parsed.git.is_none()
                    && parsed.path.is_none()
                    && parsed.version.is_none()
                    && modules::is_bundled_module(&parsed.name)
                {
                    let mut updated = lock_cfg.clone();
                    let msg = updated.activate_module(&parsed.name)?;
                    if dry_run {
                        println!(
                            "Dry-run complete: {msg} (no recompile needed; {} unchanged)",
                            lock_path.display()
                        );
                    } else {
                        updated.save(&lock_path)?;
                        println!(
                            "Activated bundled module '{}' in {}.",
                            parsed.name,
                            lock_path.display()
                        );
                    }
                    return Ok(());
                }

                let mut updated = lock_cfg.clone();
                updated.extra_crates.retain(|c| c.name != parsed.name);
                updated.extra_crates.push(parsed.clone());
                updated.disabled.retain(|c| c != &parsed.name);

                let build_dir = config
                    .data_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(".zcast-build"));
                let bin = modules::build_custom_binary(&build_dir, &updated, jobs, dry_run)?;
                updated.save(&lock_path)?;

                if dry_run {
                    println!(
                        "Dry-run complete: generated shim crate in {} and recorded '{}' in {}",
                        build_dir.display(),
                        parsed.name,
                        lock_path.display()
                    );
                    println!("Expected output binary: {}", bin.display());
                } else {
                    let current_exe = std::env::current_exe()?;
                    let prev = modules::atomic_swap_binary(&bin, &current_exe)?;
                    println!(
                        "Installed '{}' and swapped binary atomically (previous kept at {})",
                        parsed.name,
                        prev.display()
                    );
                }
            }
            ModuleAction::Activate { name } => {
                let mut updated = lock_cfg.clone();
                let msg = updated.activate_module(&name)?;
                updated.save(&lock_path)?;
                println!("{msg}");
            }
            ModuleAction::Deactivate { name } => {
                let mut updated = lock_cfg.clone();
                let msg = updated.deactivate_module(&name)?;
                updated.save(&lock_path)?;
                println!("{msg}");
            }
            ModuleAction::Remove {
                name,
                jobs,
                dry_run,
            } => {
                let mut updated = lock_cfg.clone();

                if updated.extra_crates.iter().any(|c| c.name == name) {
                    updated.extra_crates.retain(|c| c.name != name);
                    updated.disabled.retain(|c| c != &name);

                    let build_dir = config
                        .data_dir
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(".zcast-build"));
                    let bin = modules::build_custom_binary(&build_dir, &updated, jobs, dry_run)?;
                    updated.save(&lock_path)?;

                    if dry_run {
                        println!(
                            "Dry-run complete: removed '{name}' from {} and refreshed shim in {}",
                            lock_path.display(),
                            build_dir.display()
                        );
                    } else {
                        let current_exe = std::env::current_exe()?;
                        let prev = modules::atomic_swap_binary(&bin, &current_exe)?;
                        println!(
                            "Removed '{name}' and swapped binary atomically (previous kept at {})",
                            prev.display()
                        );
                    }
                } else if modules::is_mandatory_module(&name) {
                    return Err(format!(
                        "module '{name}' is mandatory and cannot be removed (swap it instead via 'zcast module bind <interface> <replacement>')"
                    )
                    .into());
                } else if modules::is_optional_bundled_module(&name) {
                    let msg = updated.deactivate_module(&name)?;
                    if dry_run {
                        println!("Dry-run complete: {msg} (no recompile needed)");
                    } else {
                        updated.save(&lock_path)?;
                        println!(
                            "Removed (deactivated) bundled module '{name}' in {}. Restart zcast to apply, or re-enable anytime with 'zcast module activate {name}'.",
                            lock_path.display()
                        );
                    }
                } else {
                    return Err(format!(
                        "unknown module '{name}' (run 'zcast module list' to see installed modules)"
                    )
                    .into());
                }
            }
            ModuleAction::Update { jobs, dry_run } => {
                let build_dir = config
                    .data_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(".zcast-build"));
                let bin = modules::build_custom_binary(&build_dir, &lock_cfg, jobs, dry_run)?;
                if dry_run {
                    println!(
                        "Dry-run complete: refreshed shim crate in {} (expected binary: {})",
                        build_dir.display(),
                        bin.display()
                    );
                } else {
                    let current_exe = std::env::current_exe()?;
                    let prev = modules::atomic_swap_binary(&bin, &current_exe)?;
                    println!(
                        "Rebuilt and swapped custom binary atomically (previous kept at {})",
                        prev.display()
                    );
                }
            }
        },

        Commands::Auth {
            action:
                AuthAction::LoginLink {
                    base_url,
                    email,
                    db,
                },
        } => {
            if let Some(d) = db {
                config.database_url = d;
            }
            let storage = connect_storage(&config.database_url).await?;
            storage.initialize().await?;

            let admin_email = email
                .or(config.admin_email)
                .unwrap_or_else(|| "admin@localhost".to_owned());
            let base = base_url
                .or(config.public_url)
                .unwrap_or_else(|| format!("http://{}", config.bind_addr));

            let provider = MagicLinkAuthProvider::new(storage, admin_email);
            let url = provider.create_cli_login_link(&base).await?;
            println!("{url}");
        }

        Commands::Export {
            out,
            zip,
            public_only,
            db,
        } => {
            if let Some(d) = db {
                config.database_url = d;
            }
            let storage = connect_storage(&config.database_url).await?;
            storage.initialize().await?;

            let ctx = if public_only {
                ReadContext::PUBLIC
            } else {
                ReadContext::ADMIN
            };
            let exporter = MarkdownIo::new();

            if let Some(zip_path) = zip {
                let (bytes, summary) = exporter.export_to_zip_bytes(storage.as_ref(), ctx).await?;
                if let Some(parent) = zip_path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&zip_path, bytes)?;
                println!(
                    "Exported {} note(s) to {}",
                    summary.exported,
                    zip_path.display()
                );
            } else {
                let dir = out.unwrap_or_else(|| PathBuf::from("zcast-export"));
                let summary = exporter.export_to_dir(storage.as_ref(), &dir, ctx).await?;
                println!("Exported {} note(s) to {}", summary.exported, dir.display());
            }
        }

        Commands::Import { path, db } => {
            if let Some(d) = db {
                config.database_url = d;
            }
            let storage = connect_storage(&config.database_url).await?;
            storage.initialize().await?;

            let importer = MarkdownIo::new();
            let summary = if path.is_file() {
                let bytes = std::fs::read(&path)?;
                importer
                    .import_from_zip_bytes(storage.as_ref(), &bytes)
                    .await?
            } else {
                importer.import_from_dir(storage.as_ref(), &path).await?
            };

            // Re-sync wikilink graph edges for all notes after import.
            let links = WikilinkParser::new();
            let notes = storage
                .list_notes(NoteListFilter::default(), ReadContext::ADMIN)
                .await?;
            for note in &notes {
                links.sync_note_links(note, storage.as_ref()).await?;
            }

            println!(
                "Import complete: {} created, {} updated, {} skipped (link graph synced for {} notes)",
                summary.created,
                summary.updated,
                summary.skipped,
                notes.len()
            );
        }
    }

    Ok(())
}
