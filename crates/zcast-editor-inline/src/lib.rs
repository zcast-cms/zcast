//! In-place TipTap/ProseMirror editor for zcast.
//!
//! The editor is deliberately shipped as a pre-bundled static asset so
//! `cargo build` (and `zcast module install`) never needs Node.js (DEC-17).
//! Rust only contributes the small UI slots which tell the browser where the
//! editor lives; all editing behaviour is implemented in `assets/editor.bundle.js`.

use maud::{Markup, html};
use rust_embed::RustEmbed;
use zcast_interfaces::{
    Asset, CardView, EditorProvider, Html, IFACE_EDITOR, InterfaceSpec, ModuleManifest,
};

const PROVIDES: &[InterfaceSpec] = &[InterfaceSpec {
    name: IFACE_EDITOR,
    version: "1.0.0",
}];

const JS_MIME: &str = "application/javascript; charset=utf-8";
const CSS_MIME: &str = "text/css; charset=utf-8";

#[derive(RustEmbed)]
#[folder = "assets/"]
struct EditorAssets;

/// Default in-place editor module (`zcast:ui/editor@1.0`).
#[derive(Debug, Clone, Copy, Default)]
pub struct InlineEditor;

impl InlineEditor {
    pub fn new() -> Self {
        Self
    }
}

impl EditorProvider for InlineEditor {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: "zcast-editor-inline",
            name: "Inline Editor",
            version: env!("CARGO_PKG_VERSION"),
            description: "In-place rich Markdown editor with [[wikilink]] autocomplete",
            provides: PROVIDES,
            requires: &[],
            recommends: &[],
        }
    }

    fn head_slot(&self) -> Html {
        html! {
            link rel="stylesheet" href="/_zcast/editor/editor.css";
        }
        .into_string()
        .into()
    }

    fn card_toolbar_slot(&self, card: &CardView) -> Html {
        let private = card.private;
        let private_label = if private { "Private" } else { "Public" };
        let private_title = if private {
            "Private note - click to make public"
        } else {
            "Public note - click to make private"
        };

        toolbar_markup(card.id.as_str(), private, private_label, private_title)
            .into_string()
            .into()
    }

    fn body_end_slot(&self) -> Html {
        html! {
            script type="module" src="/_zcast/editor/editor.bundle.js" {}
        }
        .into_string()
        .into()
    }

    fn get_asset(&self, path: &str) -> Option<Asset> {
        let path = normalise_asset_path(path);
        let content_type = match path {
            "editor.bundle.js" => JS_MIME,
            "editor.css" => CSS_MIME,
            _ => return None,
        };

        EditorAssets::get(path).map(|file| Asset {
            content_type,
            bytes: file.data,
        })
    }
}

/// Render the per-card toolbar. Kept separate so the draft-card JS can mirror
/// the exact same markup when it appends a new pane client-side.
fn toolbar_markup<'a>(
    card_id: &'a str,
    private: bool,
    private_label: &'a str,
    private_title: &'a str,
) -> Markup {
    let private_attr = if private { "true" } else { "false" };
    let pressed = if private { "true" } else { "false" };

    html! {
        button
            type="button"
            class="zcast-btn zcast-btn-edit"
            data-zcast-edit-toggle
            data-zcast-card-id=(card_id)
            aria-pressed="false"
            title="Edit note"
        { "Edit" }
        button
            type="button"
            class="zcast-btn zcast-btn-private"
            data-zcast-private-toggle
            data-zcast-card-id=(card_id)
            data-private=(private_attr)
            aria-pressed=(pressed)
            title=(private_title)
        { (private_label) }
        span
            class="zcast-save-status"
            data-zcast-save-status
            role="status"
            aria-live="polite"
        {}
    }
}

fn normalise_asset_path(path: &str) -> &str {
    let mut path = path.trim().trim_start_matches('/');
    for prefix in ["_zcast/editor/assets/", "_zcast/editor/", "assets/"] {
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
    use chrono::Utc;
    use zcast_interfaces::{BacklinkRef, NoteId};

    fn card(private: bool) -> CardView {
        CardView {
            id: NoteId::from("01JTESTNOTE0000000000000"),
            slug: "hello-world".to_owned(),
            title: "Hello World".to_owned(),
            markdown_body: "Hello **world**".to_owned(),
            rendered_html: "<p>Hello <strong>world</strong></p>".to_owned(),
            private,
            backlinks: Vec::<BacklinkRef>::new(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn manifest_declares_editor_interface() {
        let manifest = InlineEditor::new().manifest();

        assert_eq!(manifest.id, "zcast-editor-inline");
        assert_eq!(manifest.provides.len(), 1);
        assert_eq!(manifest.provides[0].name, IFACE_EDITOR);
        assert_eq!(manifest.provides[0].version, "1.0.0");
        assert!(manifest.requires.is_empty());
    }

    #[test]
    fn slots_emit_stylesheet_and_module_script() {
        let editor = InlineEditor::new();
        let head = editor.head_slot().into_string();
        let body_end = editor.body_end_slot().into_string();

        assert_eq!(
            head,
            r#"<link rel="stylesheet" href="/_zcast/editor/editor.css">"#
        );
        assert_eq!(
            body_end,
            r#"<script type="module" src="/_zcast/editor/editor.bundle.js"></script>"#
        );
    }

    #[test]
    fn card_toolbar_has_edit_private_and_save_controls() {
        let editor = InlineEditor::new();

        let private = editor.card_toolbar_slot(&card(true)).into_string();
        assert!(private.contains("data-zcast-edit-toggle"));
        assert!(private.contains("data-zcast-private-toggle"));
        assert!(private.contains(r#"data-private="true""#));
        assert!(private.contains("data-zcast-save-status"));
        assert!(private.contains(">Private</button>"));

        let public = editor.card_toolbar_slot(&card(false)).into_string();
        assert!(public.contains(r#"data-private="false""#));
        assert!(public.contains(">Public</button>"));
    }

    #[test]
    fn embedded_assets_are_served_with_correct_mime_types() {
        let editor = InlineEditor::new();

        let js = editor.get_asset("editor.bundle.js").expect("embedded JS");
        assert_eq!(js.content_type, JS_MIME);
        assert!(!js.bytes.is_empty());

        let css = editor.get_asset("editor.css").expect("embedded CSS");
        assert_eq!(css.content_type, CSS_MIME);
        assert!(!css.bytes.is_empty());

        // Asset URLs may arrive with the public mount prefix.
        assert!(editor.get_asset("assets/editor.bundle.js").is_some());
        assert!(editor.get_asset("_zcast/editor/editor.css").is_some());
        assert!(
            editor
                .get_asset("/_zcast/editor/editor.bundle.js")
                .is_some()
        );
        assert!(editor.get_asset("/editor.css").is_some());
        assert!(editor.get_asset("missing.js").is_none());
    }
}
