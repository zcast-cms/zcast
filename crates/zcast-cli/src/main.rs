use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use zcast_auth_basic::BasicAuthProvider;
use zcast_cmd_k::CommandBar;
use zcast_editor_inline::InlineEditor;
use zcast_interfaces::ThemeColorMode;
use zcast_kernel::{KernelConfig, ZcastKernelBuilder};
use zcast_links_wikilink::WikilinkParser;
use zcast_popover::HoverPopover;
use zcast_search_fts::FtsSearchProvider;
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
        /// SQLite database URL (default: sqlite://zcast.db?mode=rwc)
        #[arg(long)]
        db: Option<String>,
        /// Site title
        #[arg(long)]
        title: Option<String>,
        /// Theme colour mode: auto, dark, or light
        #[arg(long)]
        theme_mode: Option<String>,
        /// Auth provider: none (local-only) or basic
        #[arg(long)]
        auth: Option<String>,
        /// Admin username for basic auth (default: admin)
        #[arg(long)]
        admin_user: Option<String>,
        /// Admin password for basic auth (or set ZCAST_ADMIN_PASSWORD)
        #[arg(long)]
        admin_password: Option<String>,
        /// Explicitly allow binding to a non-loopback address without an auth module
        #[arg(long)]
        allow_unauthenticated_public: bool,
    },
    /// Inspect compiled modules and active interface slot bindings
    Module {
        #[command(subcommand)]
        action: ModuleAction,
    },
}

#[derive(Subcommand, Debug)]
enum ModuleAction {
    /// List compiled modules and active interface bindings
    List,
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

    match cli.command {
        Commands::Serve {
            bind,
            db,
            title,
            theme_mode,
            auth,
            admin_user,
            admin_password,
            allow_unauthenticated_public,
        } => {
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
            if allow_unauthenticated_public {
                config.allow_unauthenticated_public = true;
            }

            let auth_mode = config
                .auth
                .as_deref()
                .map(str::to_ascii_lowercase)
                .unwrap_or_else(|| {
                    if config.admin_password.is_some() {
                        "basic".to_owned()
                    } else {
                        "none".to_owned()
                    }
                });

            let storage = Arc::new(SqliteStorage::connect(&config.database_url).await?);
            let links = Arc::new(WikilinkParser::new());
            let theme = Arc::new(DavidHewitsonTheme::new());
            let editor = Arc::new(InlineEditor::new());
            let search = Arc::new(FtsSearchProvider::new());
            let popover = Arc::new(HoverPopover::new());
            let cmd_k = Arc::new(CommandBar::new());

            let mut builder = ZcastKernelBuilder::new(config.clone(), storage.clone(), links, theme)
                .with_editor(editor)
                .with_search(search)
                .with_ui_extension(popover)
                .with_ui_extension(cmd_k);

            let mut basic_auth: Option<Arc<BasicAuthProvider>> = None;
            match auth_mode.as_str() {
                "none" | "" => {}
                "basic" => {
                    let provider = Arc::new(BasicAuthProvider::new(storage.clone()));
                    basic_auth = Some(provider.clone());
                    builder = builder.with_auth(provider);
                }
                other => {
                    return Err(
                        format!("unknown auth mode '{other}' (expected 'none' or 'basic')").into(),
                    );
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
                println!("    Login URL: http://{}/_zcast/login", kernel.config.bind_addr);
                println!("    Username:  {}", kernel.config.admin_user);
                println!("    Password:  {generated}\n");
            }

            let addr = kernel.config.validate_bind_safety(kernel.has_auth())?;
            let app = zcast_kernel::http::router(kernel.clone());

            tracing::info!(
                "zcast v{} listening on http://{} (theme mode: {}, auth: {})",
                env!("CARGO_PKG_VERSION"),
                addr,
                kernel.config.color_mode.as_str(),
                if kernel.has_auth() {
                    "basic"
                } else {
                    "local-only (none)"
                }
            );

            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
        }
        Commands::Module {
            action: ModuleAction::List,
        } => {
            let storage = Arc::new(SqliteStorage::connect("sqlite::memory:").await?);
            let links = Arc::new(WikilinkParser::new());
            let theme = Arc::new(DavidHewitsonTheme::new());
            let editor = Arc::new(InlineEditor::new());
            let search = Arc::new(FtsSearchProvider::new());
            let popover = Arc::new(HoverPopover::new());
            let cmd_k = Arc::new(CommandBar::new());
            let auth = Arc::new(BasicAuthProvider::new(storage.clone()));

            let kernel = ZcastKernelBuilder::new(config, storage, links, theme)
                .with_editor(editor)
                .with_auth(auth)
                .with_search(search)
                .with_ui_extension(popover)
                .with_ui_extension(cmd_k)
                .build()
                .await?;
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
                    println!("  - {} v{}  provides [{}]", m.id, m.version, provides.join(", "));
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
                println!("  - {} -> {} ({})", b.interface, b.module_id, b.module_version);
            }
        }
    }

    Ok(())
}
