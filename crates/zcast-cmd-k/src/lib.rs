//! `Cmd+K` spotlight command bar UI module for zcast (`zcast:ui/cmd-k@1.0.0`).
//!
//! The command bar is a pure frontend extension: it declares a dependency on
//! the versioned `search` contract (DEC-01) and talks to the kernel's
//! `/_zcast/search` endpoint from vanilla JS. All markup and behaviour ships
//! as crate-embedded assets (DEC-07 / DEC-17), so `cargo build` never needs
//! Node.js.

use rust_embed::RustEmbed;
use zcast_interfaces::{
    Asset, Html, IFACE_CMD_K, IFACE_SEARCH, InterfaceReq, InterfaceSpec, ModuleManifest,
    UiExtension,
};

const PROVIDES: &[InterfaceSpec] = &[InterfaceSpec {
    name: IFACE_CMD_K,
    version: "1.0.0",
}];

const REQUIRES: &[InterfaceReq] = &[InterfaceReq {
    name: IFACE_SEARCH,
    version_req: "^1.0",
    optional: false,
}];

const RECOMMENDS: &[&str] = &["zcast-search-onnx"];

const CSS_MIME: &str = "text/css; charset=utf-8";
const JS_MIME: &str = "application/javascript; charset=utf-8";

const HEAD_SLOT: &str = r#"<link rel="stylesheet" href="/_zcast/ext/cmd-k/cmd-k.css">"#;

const HEADER_ACTIONS_SLOT: &str = r#"<button type="button" class="zcast-header-btn zcast-cmdk-trigger" data-zcast-cmdk-open aria-label="Search notes" title="Search notes (Cmd+K)"><span>Search</span><kbd class="zcast-cmdk-kbd" data-zcast-cmdk-kbd>⌘K</kbd></button>"#;

const BODY_END_SLOT: &str = r#"<script src="/_zcast/ext/cmd-k/cmd-k.js" defer></script>"#;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct CmdKAssets;

/// Spotlight command bar module (`zcast:ui/cmd-k@1.0.0`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandBar;

impl CommandBar {
    pub fn new() -> Self {
        Self
    }
}

impl UiExtension for CommandBar {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: "zcast-cmd-k",
            name: "Spotlight Command Bar",
            version: env!("CARGO_PKG_VERSION"),
            description: "Instant Cmd+K spotlight search bar powered by SearchProvider",
            provides: PROVIDES,
            requires: REQUIRES,
            recommends: RECOMMENDS,
        }
    }

    fn asset_prefix(&self) -> &'static str {
        "cmd-k"
    }

    fn head_slot(&self, _is_admin: bool) -> Html {
        Html::from(HEAD_SLOT)
    }

    fn header_actions_slot(&self, _is_admin: bool) -> Html {
        Html::from(HEADER_ACTIONS_SLOT)
    }

    fn body_end_slot(&self, _is_admin: bool) -> Html {
        Html::from(BODY_END_SLOT)
    }

    fn get_asset(&self, path: &str) -> Option<Asset> {
        let path = normalise_asset_path(path);
        let content_type = match path {
            "cmd-k.css" => CSS_MIME,
            "cmd-k.js" => JS_MIME,
            _ => return None,
        };

        CmdKAssets::get(path).map(|file| Asset {
            content_type,
            bytes: file.data,
        })
    }
}

/// Accept asset paths as bare filenames, root-relative URLs, or public mount
/// URLs (`/_zcast/ext/cmd-k/...`).
fn normalise_asset_path(path: &str) -> &str {
    let mut path = path.trim().trim_start_matches('/');
    for prefix in ["_zcast/ext/cmd-k/", "assets/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            path = rest;
            break;
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_declares_cmd_k_interface_and_search_requirement() {
        let manifest = CommandBar::new().manifest();

        assert_eq!(manifest.id, "zcast-cmd-k");
        assert_eq!(manifest.name, "Spotlight Command Bar");
        assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            manifest.description,
            "Instant Cmd+K spotlight search bar powered by SearchProvider"
        );

        assert_eq!(manifest.provides.len(), 1);
        assert_eq!(manifest.provides[0].name, IFACE_CMD_K);
        assert_eq!(manifest.provides[0].version, "1.0.0");

        assert_eq!(manifest.requires.len(), 1);
        assert_eq!(manifest.requires[0].name, IFACE_SEARCH);
        assert_eq!(manifest.requires[0].version_req, "^1.0");
        assert!(!manifest.requires[0].optional);

        assert!(manifest.recommends.contains(&"zcast-search-onnx"));
    }

    #[test]
    fn manifest_uses_the_expected_interface_names() {
        assert_eq!(IFACE_CMD_K, "zcast:ui/cmd-k");
        assert_eq!(IFACE_SEARCH, "zcast:search/provider");
    }

    #[test]
    fn asset_prefix_matches_the_public_mount_point() {
        assert_eq!(CommandBar::new().asset_prefix(), "cmd-k");
    }

    #[test]
    fn head_slot_loads_the_extension_stylesheet() {
        let bar = CommandBar::new();

        assert_eq!(
            bar.head_slot(false).into_string(),
            r#"<link rel="stylesheet" href="/_zcast/ext/cmd-k/cmd-k.css">"#
        );
        assert_eq!(
            bar.head_slot(true).into_string(),
            r#"<link rel="stylesheet" href="/_zcast/ext/cmd-k/cmd-k.css">"#
        );
    }

    #[test]
    fn header_actions_slot_renders_an_editorial_trigger() {
        let bar = CommandBar::new();
        let header = bar.header_actions_slot(false).into_string();

        assert_eq!(header, HEADER_ACTIONS_SLOT);
        assert!(header.contains(r#"class="zcast-header-btn zcast-cmdk-trigger""#));
        assert!(header.contains("data-zcast-cmdk-open"));
        assert!(header.contains(r#"aria-label="Search notes""#));
        assert!(header.contains(r#"title="Search notes (Cmd+K)""#));
        assert!(header.contains(r#"<kbd class="zcast-cmdk-kbd" data-zcast-cmdk-kbd>⌘K</kbd>"#));

        assert_eq!(
            bar.header_actions_slot(true).into_string(),
            HEADER_ACTIONS_SLOT
        );
    }

    #[test]
    fn body_end_slot_loads_a_deferred_script() {
        let bar = CommandBar::new();

        assert_eq!(
            bar.body_end_slot(false).into_string(),
            r#"<script src="/_zcast/ext/cmd-k/cmd-k.js" defer></script>"#
        );
        assert_eq!(
            bar.body_end_slot(true).into_string(),
            r#"<script src="/_zcast/ext/cmd-k/cmd-k.js" defer></script>"#
        );
    }

    #[test]
    fn embedded_assets_are_served_with_correct_mime_types() {
        let bar = CommandBar::new();

        let css = bar.get_asset("cmd-k.css").expect("embedded CSS");
        assert_eq!(css.content_type, CSS_MIME);
        assert!(!css.bytes.is_empty());

        let js = bar.get_asset("cmd-k.js").expect("embedded JS");
        assert_eq!(js.content_type, JS_MIME);
        assert!(!js.bytes.is_empty());
    }

    #[test]
    fn get_asset_normalises_mount_prefixes() {
        let bar = CommandBar::new();

        for path in [
            "/cmd-k.css",
            "_zcast/ext/cmd-k/cmd-k.css",
            "/_zcast/ext/cmd-k/cmd-k.css",
            "assets/cmd-k.css",
            "  /_zcast/ext/cmd-k/cmd-k.css  ",
        ] {
            let asset = bar
                .get_asset(path)
                .unwrap_or_else(|| panic!("missing {path}"));
            assert_eq!(asset.content_type, CSS_MIME);
        }

        for path in [
            "/cmd-k.js",
            "_zcast/ext/cmd-k/cmd-k.js",
            "/_zcast/ext/cmd-k/cmd-k.js",
            "assets/cmd-k.js",
        ] {
            let asset = bar
                .get_asset(path)
                .unwrap_or_else(|| panic!("missing {path}"));
            assert_eq!(asset.content_type, JS_MIME);
        }

        assert!(bar.get_asset("missing.js").is_none());
        assert!(bar.get_asset("").is_none());
    }

    #[test]
    fn css_uses_the_shared_design_tokens() {
        let css = String::from_utf8(
            CommandBar::new()
                .get_asset("cmd-k.css")
                .expect("embedded CSS")
                .bytes
                .into_owned(),
        )
        .expect("CSS is UTF-8");

        assert!(css.contains("--zcast-z-modal"));
        assert!(css.contains("var(--zcast-surface-elevated, var(--zcast-surface))"));
        assert!(css.contains("var(--zcast-shadow-popover)"));
        assert!(css.contains("var(--zcast-accent-subtle)"));
        assert!(css.contains("backdrop-filter: blur(4px)"));
        assert!(css.contains("max-width: 560px"));
        assert!(css.contains("min(14vh, 110px)"));
        assert!(css.contains("prefers-reduced-motion: reduce"));
        assert!(css.contains("@media (max-width: 768px)"));
    }

    #[test]
    fn js_contains_the_expected_search_and_interaction_contract() {
        let js = String::from_utf8(
            CommandBar::new()
                .get_asset("cmd-k.js")
                .expect("embedded JS")
                .bytes
                .into_owned(),
        )
        .expect("JS is UTF-8");

        assert!(js.contains("/_zcast/search"));
        assert!(js.contains("limit=12"));
        assert!(js.contains("preview=public"));
        assert!(js.contains("ZcastPanes"));
        assert!(js.contains("openSlug"));
        assert!(js.contains("zcast:open-draft"));
        assert!(js.contains("AbortController"));
        assert!(js.contains("data-zcast-cmdk-open"));
        assert!(js.contains("data-zcast-cmdk-kbd"));
        assert!(js.contains("zcast-body--admin"));
    }
}
