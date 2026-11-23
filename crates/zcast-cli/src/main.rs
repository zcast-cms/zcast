use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use zcast_editor_inline::InlineEditor;
use zcast_interfaces::ThemeColorMode;
use zcast_kernel::{KernelConfig, ZcastKernelBuilder};
use zcast_links_wikilink::WikilinkParser;
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
            if allow_unauthenticated_public {
                config.allow_unauthenticated_public = true;
            }

            let storage = Arc::new(SqliteStorage::connect(&config.database_url).await?);
            let links = Arc::new(WikilinkParser::new());
            let theme = Arc::new(DavidHewitsonTheme::new());
            let editor = Arc::new(InlineEditor::new());

            let kernel = ZcastKernelBuilder::new(config.clone(), storage, links, theme)
                .with_editor(editor)
                .build()
                .await?;

            let addr = kernel.config.validate_bind_safety(kernel.has_auth())?;
            let app = zcast_kernel::http::router(kernel.clone());

            tracing::info!(
                "zcast v{} listening on http://{} (theme mode: {}, auth: {})",
                env!("CARGO_PKG_VERSION"),
                addr,
                kernel.config.color_mode.as_str(),
                if kernel.has_auth() {
                    "enabled"
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

            let kernel = ZcastKernelBuilder::new(config, storage, links, theme)
                .with_editor(editor)
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
                println!("  - {} v{}  [{}]", m.id, m.version, provides.join(", "));
            }
            println!("\nActive Interface Bindings:");
            for b in &snap.bindings {
                println!("  - {} -> {} ({})", b.interface, b.module_id, b.module_version);
            }
        }
    }

    Ok(())
}
