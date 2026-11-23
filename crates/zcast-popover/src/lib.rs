//! Hover preview popovers for zcast wikilinks (`zcast:ui/popover@1.0.0`).
//!
//! The module contributes a stylesheet and a tiny dependency-free script to
//! the kernel's standard UI slots. The browser code fetches preview data from
//! `/_zcast/popover/:slug` and renders it as a floating, theme-token-driven
//! card. Rust also offers [`build_excerpt_html`], a small, allocation-light
//! HTML excerpt builder used by the kernel when it composes preview responses.

use rust_embed::RustEmbed;
use zcast_interfaces::{Asset, Html, IFACE_POPOVER, InterfaceSpec, ModuleManifest, UiExtension};

const PROVIDES: &[InterfaceSpec] = &[InterfaceSpec {
    name: IFACE_POPOVER,
    version: "1.0.0",
}];

const CSS_MIME: &str = "text/css; charset=utf-8";
const JS_MIME: &str = "application/javascript; charset=utf-8";

#[derive(RustEmbed)]
#[folder = "assets/"]
struct PopoverAssets;

/// Default hover-preview popover extension (`zcast:ui/popover@1.0.0`).
#[derive(Debug, Clone, Copy, Default)]
pub struct HoverPopover;

impl HoverPopover {
    pub fn new() -> Self {
        Self
    }
}

impl UiExtension for HoverPopover {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: "zcast-popover",
            name: "Hover Popover",
            version: env!("CARGO_PKG_VERSION"),
            description: "Hover preview popovers for [[wikilinks]]",
            provides: PROVIDES,
            requires: &[],
            recommends: &[],
        }
    }

    fn asset_prefix(&self) -> &'static str {
        "popover"
    }

    fn head_slot(&self, _is_admin: bool) -> Html {
        Html::from(r#"<link rel="stylesheet" href="/_zcast/ext/popover/popover.css">"#)
    }

    fn body_end_slot(&self, _is_admin: bool) -> Html {
        Html::from(r#"<script src="/_zcast/ext/popover/popover.js" defer></script>"#)
    }

    fn get_asset(&self, path: &str) -> Option<Asset> {
        let path = normalise_asset_path(path);
        let content_type = match path {
            "popover.css" => CSS_MIME,
            "popover.js" => JS_MIME,
            _ => return None,
        };

        PopoverAssets::get(path).map(|file| Asset {
            content_type,
            bytes: file.data,
        })
    }
}

fn normalise_asset_path(path: &str) -> &str {
    let mut path = path.trim().trim_start_matches('/');

    for prefix in ["_zcast/ext/popover/", "assets/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            path = rest.trim_start_matches('/');
            break;
        }
    }

    path
}

/// Build a clean, balanced HTML excerpt from an already-sanitized rendered card.
///
/// Headings (and their contents) are removed so the preview stays compact.
/// Paragraphs and inline formatting are preserved. When the visible-character
/// budget is exceeded, the excerpt is cut on a character/entity boundary and
/// an ellipsis is appended. Every opened tag is closed, so the result is always
/// safe to inject into the popover body without producing broken markup.
pub fn build_excerpt_html(rendered_html: &str, max_chars: usize) -> String {
    let tokens = tokenize(rendered_html);
    let total_visible = total_visible_chars(&tokens);

    if total_visible == 0 || max_chars == 0 {
        return String::new();
    }

    let truncated = total_visible > max_chars;
    // Keep the total visible length (including the ellipsis) within budget.
    let budget = if truncated {
        max_chars.saturating_sub(1)
    } else {
        max_chars
    };

    render_excerpt(&tokens, budget, truncated)
}

#[derive(Debug, Clone)]
enum Token {
    Start {
        name: String,
        raw: String,
        self_closing: bool,
    },
    End {
        name: String,
    },
    Text(String),
}

#[derive(Debug, Default)]
struct SkipState {
    name: Option<String>,
    depth: usize,
}

impl SkipState {
    fn skipping(&self) -> bool {
        self.name.is_some()
    }

    fn on_start(&mut self, name: &str) {
        if let Some(skip_name) = &self.name {
            if skip_name == name {
                self.depth += 1;
            }
        } else if is_skipped_element(name) {
            self.name = Some(name.to_owned());
            self.depth = 1;
        }
    }

    fn on_end(&mut self, name: &str) {
        let Some(skip_name) = &self.name else {
            return;
        };

        if skip_name == name {
            self.depth = self.depth.saturating_sub(1);
            if self.depth == 0 {
                self.name = None;
            }
        }
    }
}

fn total_visible_chars(tokens: &[Token]) -> usize {
    let mut skip = SkipState::default();
    let mut total = 0;

    for token in tokens {
        match token {
            Token::Start { name, .. } => skip.on_start(name),
            Token::End { name } => skip.on_end(name),
            Token::Text(text) if !skip.skipping() => total += count_visible_units(text),
            Token::Text(_) => {}
        }
    }

    total
}

fn render_excerpt(tokens: &[Token], budget: usize, truncated: bool) -> String {
    let mut out = String::new();
    let mut stack: Vec<String> = Vec::new();
    let mut skip = SkipState::default();
    let mut remaining = budget;

    for token in tokens {
        match token {
            Token::Start {
                name,
                raw,
                self_closing,
            } => {
                let was_skipping = skip.skipping();
                skip.on_start(name);

                if was_skipping || skip.skipping() || remaining == 0 {
                    continue;
                }

                out.push_str(raw);
                if !self_closing && !is_void_element(name) {
                    stack.push(name.clone());
                }
            }
            Token::End { name } => {
                let was_skipping = skip.skipping();
                skip.on_end(name);

                if was_skipping || remaining == 0 {
                    continue;
                }

                if let Some(position) = stack.iter().rposition(|open| open == name) {
                    while stack.len() > position {
                        let open = stack.pop().expect("stack has an open tag");
                        out.push_str("</");
                        out.push_str(&open);
                        out.push('>');
                    }
                }
            }
            Token::Text(text) => {
                if skip.skipping() {
                    continue;
                }

                if remaining == 0 {
                    break;
                }

                let (piece, taken, more) = take_visible_units(text, remaining);
                out.push_str(&piece);
                remaining -= taken;

                if more || remaining == 0 {
                    break;
                }
            }
        }
    }

    if truncated {
        trim_trailing_whitespace(&mut out);
        out.push('…');
    }

    while let Some(name) = stack.pop() {
        out.push_str("</");
        out.push_str(&name);
        out.push('>');
    }

    out
}

fn tokenize(html: &str) -> Vec<Token> {
    let bytes = html.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'<' {
            let rest = &html[index..];

            if let Some(after_comment) = rest.strip_prefix("<!--") {
                if let Some(end) = after_comment.find("-->") {
                    index += 4 + end + 3;
                } else {
                    break;
                }
                continue;
            }

            if rest.starts_with("<!") || rest.starts_with("<?") {
                if let Some(end) = rest.find('>') {
                    index += end + 1;
                } else {
                    break;
                }
                continue;
            }

            if let Some(end) = find_tag_end(html, index) {
                let raw = &html[index..=end];
                let inner = html[index + 1..end].trim();

                if let Some(closing) = inner.strip_prefix('/') {
                    let name = parse_tag_name(closing);
                    if !name.is_empty() {
                        tokens.push(Token::End { name });
                    }
                } else {
                    let self_closing = inner.ends_with('/');
                    let name = parse_tag_name(inner.trim_end_matches('/'));
                    if !name.is_empty() {
                        tokens.push(Token::Start {
                            name,
                            raw: raw.to_owned(),
                            self_closing,
                        });
                    }
                }

                index = end + 1;
                continue;
            }

            // A stray `<` is not markup; treat the remainder as text.
            let text = collapse_whitespace(&html[index..]);
            if !text.is_empty() {
                tokens.push(Token::Text(text));
            }
            break;
        }

        let end = html[index..]
            .find('<')
            .map(|offset| index + offset)
            .unwrap_or(html.len());
        let text = collapse_whitespace(&html[index..end]);
        if !text.is_empty() {
            tokens.push(Token::Text(text));
        }
        index = end;
    }

    tokens
}

fn find_tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote = None;

    for (offset, byte) in html.as_bytes()[start + 1..].iter().enumerate() {
        let index = start + 1 + offset;

        match quote {
            Some(active) if *byte == active => quote = None,
            Some(_) => {}
            None if *byte == b'"' || *byte == b'\'' => quote = Some(*byte),
            None if *byte == b'>' => return Some(index),
            None => {}
        }
    }

    None
}

fn parse_tag_name(input: &str) -> String {
    input
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':'))
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut previous_was_space = false;

    for ch in input.chars() {
        if ch.is_whitespace() {
            if !previous_was_space {
                out.push(' ');
                previous_was_space = true;
            }
        } else {
            out.push(ch);
            previous_was_space = false;
        }
    }

    out
}

fn count_visible_units(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut count = 0;

    while index < bytes.len() {
        if bytes[index] == b'&'
            && let Some(length) = entity_length(&text[index..])
        {
            index += length;
            count += 1;
            continue;
        }

        let ch = text[index..]
            .chars()
            .next()
            .expect("index is on a char boundary");
        index += ch.len_utf8();
        count += 1;
    }

    count
}

fn take_visible_units(text: &str, limit: usize) -> (String, usize, bool) {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    let mut taken = 0;

    while index < bytes.len() && taken < limit {
        if bytes[index] == b'&'
            && let Some(length) = entity_length(&text[index..])
        {
            out.push_str(&text[index..index + length]);
            index += length;
            taken += 1;
            continue;
        }

        let ch = text[index..]
            .chars()
            .next()
            .expect("index is on a char boundary");
        out.push(ch);
        index += ch.len_utf8();
        taken += 1;
    }

    (out, taken, index < bytes.len())
}

fn entity_length(input: &str) -> Option<usize> {
    if !input.starts_with('&') {
        return None;
    }

    let semicolon = input.find(';')?;
    if !(2..=32).contains(&semicolon) {
        return None;
    }

    let body = &input[1..semicolon];

    if let Some(digits) = body.strip_prefix('#') {
        if digits.is_empty() {
            return None;
        }

        if digits.starts_with('x') || digits.starts_with('X') {
            let hex = &digits[1..];
            if !hex.is_empty() && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Some(semicolon + 1);
            }
            return None;
        }

        if digits.chars().all(|ch| ch.is_ascii_digit()) {
            return Some(semicolon + 1);
        }

        return None;
    }

    let mut chars = body.chars();
    if chars.next().is_some_and(|ch| ch.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric())
    {
        Some(semicolon + 1)
    } else {
        None
    }
}

fn trim_trailing_whitespace(input: &mut String) {
    while input.ends_with(char::is_whitespace) {
        input.pop();
    }
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_skipped_element(name: &str) -> bool {
    matches!(
        name,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "script" | "style" | "template" | "noscript"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extension() -> HoverPopover {
        HoverPopover::new()
    }

    fn asset_text(path: &str) -> String {
        let asset = extension().get_asset(path).expect("embedded asset");
        String::from_utf8(asset.bytes.into_owned()).expect("asset is valid UTF-8")
    }

    fn assert_balanced(html: &str) {
        let mut stack: Vec<String> = Vec::new();

        for token in tokenize(html) {
            match token {
                Token::Start {
                    name, self_closing, ..
                } => {
                    if !self_closing && !is_void_element(&name) {
                        stack.push(name);
                    }
                }
                Token::End { name } => {
                    let position = stack
                        .iter()
                        .rposition(|open| open == &name)
                        .unwrap_or_else(|| panic!("unexpected closing tag </{name}>"));

                    assert_eq!(
                        position,
                        stack.len() - 1,
                        "mis-nested </{name}> in {html:?}"
                    );
                    stack.pop();
                }
                Token::Text(_) => {}
            }
        }

        assert!(stack.is_empty(), "unclosed tags {stack:?} in {html:?}");
    }

    #[test]
    fn manifest_declares_popover_interface() {
        let manifest = extension().manifest();

        assert_eq!(manifest.id, "zcast-popover");
        assert_eq!(manifest.name, "Hover Popover");
        assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            manifest.description,
            "Hover preview popovers for [[wikilinks]]"
        );
        assert_eq!(manifest.provides.len(), 1);
        assert_eq!(manifest.provides[0].name, IFACE_POPOVER);
        assert_eq!(manifest.provides[0].version, "1.0.0");
        assert!(manifest.requires.is_empty());
        assert!(manifest.recommends.is_empty());
    }

    #[test]
    fn extension_slots_and_prefix_match_the_kernel_contract() {
        let popover = extension();

        assert_eq!(popover.asset_prefix(), "popover");
        assert_eq!(
            popover.head_slot(false).as_str(),
            r#"<link rel="stylesheet" href="/_zcast/ext/popover/popover.css">"#
        );
        assert_eq!(
            popover.head_slot(true).as_str(),
            r#"<link rel="stylesheet" href="/_zcast/ext/popover/popover.css">"#
        );
        assert_eq!(
            popover.body_end_slot(false).as_str(),
            r#"<script src="/_zcast/ext/popover/popover.js" defer></script>"#
        );
        assert_eq!(
            popover.body_end_slot(true).as_str(),
            r#"<script src="/_zcast/ext/popover/popover.js" defer></script>"#
        );
    }

    #[test]
    fn embedded_assets_are_served_with_correct_mime_types() {
        let popover = extension();

        for path in [
            "popover.css",
            "/popover.css",
            "_zcast/ext/popover/popover.css",
            "/_zcast/ext/popover/popover.css",
        ] {
            let asset = popover.get_asset(path).expect("CSS asset");
            assert_eq!(asset.content_type, CSS_MIME);
            assert!(!asset.bytes.is_empty());
        }

        for path in [
            "popover.js",
            "/popover.js",
            "_zcast/ext/popover/popover.js",
            "/_zcast/ext/popover/popover.js",
        ] {
            let asset = popover.get_asset(path).expect("JS asset");
            assert_eq!(asset.content_type, JS_MIME);
            assert!(!asset.bytes.is_empty());
        }

        assert!(popover.get_asset("missing.js").is_none());
        assert!(popover.get_asset("popover.css?cache=1").is_none());
    }

    #[test]
    fn short_html_is_preserved_exactly() {
        let html = "<p>Hello <strong>world</strong></p>";

        assert_eq!(build_excerpt_html(html, 200), html);
    }

    #[test]
    fn long_multi_paragraph_html_truncates_on_a_clean_boundary() {
        let html = concat!(
            "<p>First paragraph with several words for the excerpt.</p>",
            "<p>Second paragraph that should be cut somewhere in the middle.</p>",
            "<p>Third paragraph is only a fallback.</p>"
        );
        let excerpt = build_excerpt_html(html, 54);

        assert!(excerpt.starts_with("<p>First paragraph"));
        assert!(excerpt.contains('…'));
        assert!(excerpt.ends_with("</p>"));
        assert_balanced(&excerpt);
        assert!(total_visible_chars(&tokenize(&excerpt)) <= 54);
    }

    #[test]
    fn inline_formatting_is_closed_when_truncated() {
        let excerpt = build_excerpt_html(
            "<p>Hello <strong>brave <em>new</em> world</strong> and more text</p>",
            20,
        );

        assert!(excerpt.starts_with("<p>Hello <strong>"));
        assert!(excerpt.ends_with("…</strong></p>") || excerpt.ends_with("…</em></strong></p>"));
        assert_balanced(&excerpt);
        assert!(total_visible_chars(&tokenize(&excerpt)) <= 20);
    }

    #[test]
    fn html_entities_are_preserved_and_count_as_one_character() {
        let html = "<p>Tom &amp; Jerry &lt;3</p>";
        assert_eq!(build_excerpt_html(html, 200), html);

        let excerpt = build_excerpt_html("<p>&amp;&amp;&amp;&amp;</p>", 3);
        assert_eq!(excerpt, "<p>&amp;&amp;…</p>");
        assert_balanced(&excerpt);
        assert_eq!(total_visible_chars(&tokenize(&excerpt)), 3);
    }

    #[test]
    fn headings_are_removed_from_previews() {
        let excerpt = build_excerpt_html(
            "<h1>Page title</h1><p>Body <em>text</em></p><h2>Section</h2>",
            100,
        );

        assert_eq!(excerpt, "<p>Body <em>text</em></p>");
        assert!(!excerpt.contains("Page title"));
        assert!(!excerpt.contains("Section"));
    }

    #[test]
    fn empty_and_heading_only_html_return_empty_strings() {
        assert_eq!(build_excerpt_html("", 100), "");
        assert_eq!(build_excerpt_html("<h1>Only a title</h1>", 100), "");
        assert_eq!(build_excerpt_html("<p>Some text</p>", 0), "");
    }

    #[test]
    fn css_is_fully_driven_by_zcast_design_tokens() {
        let css = asset_text("popover.css");

        for token in [
            "--zcast-surface",
            "--zcast-border",
            "--zcast-radius",
            "--zcast-shadow-popover",
            "--zcast-z-popover",
            "--zcast-text",
            "--zcast-font-body",
            "--zcast-text-muted",
            "--zcast-accent",
            "--zcast-font-mono",
            "--zcast-surface-elevated",
        ] {
            assert!(css.contains(token), "missing token {token}");
        }

        assert!(css.contains("background: var(--zcast-surface-elevated, var(--zcast-surface))"));
        assert!(css.contains("border: 1px solid var(--zcast-border)"));
        assert!(css.contains("border-radius: var(--zcast-radius)"));
        assert!(css.contains("box-shadow: var(--zcast-shadow-popover)"));
        assert!(css.contains("z-index: var(--zcast-z-popover)"));
        assert!(css.contains("color: var(--zcast-text)"));
        assert!(css.contains("font-family: var(--zcast-font-body)"));
        assert!(css.contains("140ms"));
        assert!(css.contains("prefers-reduced-motion: reduce"));
        assert!(css.contains("@media (hover: none), (pointer: coarse)"));
        assert!(css.contains("display: none !important"));
        assert!(!css.contains('#'), "CSS must not hardcode hex colours");
    }

    #[test]
    fn javascript_contains_hover_touch_cache_and_invalidation_guards() {
        let js = asset_text("popover.js");

        for needle in [
            "pointerover",
            "pointerout",
            "focusin",
            "focusout",
            "a[data-zcast-note]",
            "data-zcast-unresolved",
            ".zcast-editor-surface",
            "[contenteditable=\"true\"]",
            ".zcast-popover",
            "pointerType === 'touch'",
            "matchMedia('(hover: none)')",
            "HOVER_DELAY_MS = 150",
            "LEAVE_DELAY_MS = 180",
            "CACHE_TTL_MS = 30 * 1000",
            "new Map()",
            "zcast:note-saved",
            "cache.clear()",
            "encodeURIComponent",
            "/_zcast/popover/",
            "preview=public",
            "Accept",
            "application/json",
            "credentials: 'same-origin'",
            "role",
            "aria-hidden",
            "Escape",
            "scroll",
            "getBoundingClientRect",
        ] {
            assert!(js.contains(needle), "missing JS behaviour: {needle}");
        }
    }
}
