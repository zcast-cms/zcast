//! Default `[[wikilink]]` implementation of the `zcast:links/parser@1.0` contract.
//!
//! Parses wikilinks (ignoring fenced and inline code), renders Markdown to
//! sanitised GFM HTML with links resolved against [`Storage`], maintains the
//! outgoing/incoming link graph in the `zcast-links-wikilink` module store, and
//! rewrites references when a note is renamed.

use std::collections::HashSet;

use async_trait::async_trait;
use chrono::Utc;
use pulldown_cmark::{Options, Parser, html};
use serde::{Deserialize, Serialize};
use zcast_interfaces::{
    BacklinkRef, Html, IFACE_LINKS, IFACE_STORAGE, InterfaceReq, InterfaceSpec, LinkError,
    LinkParser, ModuleManifest, Note, NoteId, NoteListFilter, ParsedLink, ReadContext,
    ResolvedTarget, Storage, slugify,
};

/// Module-store namespace holding the link graph edges.
const NAMESPACE: &str = "zcast-links-wikilink";
/// Key prefix for a note's outgoing edges (`out:<note_id>`).
const OUT_PREFIX: &str = "out:";

const MANIFEST: ModuleManifest = ModuleManifest {
    id: "zcast-links-wikilink",
    name: "Wikilink Parser",
    version: "0.1.0",
    description: "[[wikilink]] parsing, GFM rendering, backlink graph, and rename rewriting",
    provides: &[InterfaceSpec {
        name: IFACE_LINKS,
        version: "1.0.0",
    }],
    requires: &[InterfaceReq {
        name: IFACE_STORAGE,
        version_req: "^1.0",
        optional: false,
    }],
    recommends: &[],
};

/// Default wikilink [`LinkParser`] provider.
#[derive(Debug, Default, Clone, Copy)]
pub struct WikilinkParser;

impl WikilinkParser {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LinkParser for WikilinkParser {
    fn manifest(&self) -> ModuleManifest {
        MANIFEST
    }

    fn extract_links(&self, markdown: &str) -> Vec<ParsedLink> {
        scan(markdown)
            .into_iter()
            .filter_map(|segment| match segment {
                Segment::Link {
                    raw,
                    target,
                    display,
                    ..
                } => Some(ParsedLink {
                    raw: raw.to_owned(),
                    target_title: target.to_owned(),
                    display_text: display.to_owned(),
                }),
                Segment::Text(_) => None,
            })
            .collect()
    }

    async fn render_markdown_with_links(
        &self,
        markdown: &str,
        storage: &dyn Storage,
        ctx: ReadContext,
    ) -> Result<Html, LinkError> {
        let segments = scan(markdown);

        // Resolve each distinct target once.
        let mut resolutions: Vec<(String, Option<ResolvedTarget>)> = Vec::new();
        for segment in &segments {
            if let Segment::Link { target, .. } = segment
                && !resolutions.iter().any(|(cached, _)| cached == target)
            {
                let resolved = resolve_target(target, storage, ctx).await?;
                resolutions.push(((*target).to_owned(), resolved));
            }
        }

        let mut replaced = String::with_capacity(markdown.len());
        for segment in &segments {
            match segment {
                Segment::Text(text) => replaced.push_str(text),
                Segment::Link {
                    target, display, ..
                } => {
                    let resolved = resolutions
                        .iter()
                        .find(|(cached, _)| cached == target)
                        .and_then(|(_, resolved)| resolved.as_ref());

                    match resolved {
                        Some(target_note) => {
                            replaced.push_str(&render_resolved(target_note, display));
                        }
                        None if ctx.include_private => {
                            replaced.push_str(&render_unresolved_admin(target, display));
                        }
                        None => replaced.push_str(&render_unresolved_public(display)),
                    }
                }
            }
        }

        let rendered = render_gfm(&replaced);
        Ok(Html(sanitise(&rendered)))
    }

    async fn sync_note_links(&self, note: &Note, storage: &dyn Storage) -> Result<(), LinkError> {
        let store = storage.module_store(NAMESPACE);
        let key = out_key(&note.id);

        let links: Vec<StoredLink> = self
            .extract_links(&note.markdown_body)
            .into_iter()
            .map(StoredLink::from)
            .collect();

        if links.is_empty() {
            store.delete(&key).await?;
            return Ok(());
        }

        let value = serde_json::to_value(&links)
            .map_err(|e| LinkError::Parse(format!("unable to persist link edges: {e}")))?;
        store.put(&key, &value, None).await?;
        Ok(())
    }

    async fn remove_note_links(
        &self,
        note_id: &NoteId,
        storage: &dyn Storage,
    ) -> Result<(), LinkError> {
        storage
            .module_store(NAMESPACE)
            .delete(&out_key(note_id))
            .await?;
        Ok(())
    }

    async fn get_backlinks(
        &self,
        note_id: &NoteId,
        storage: &dyn Storage,
        ctx: ReadContext,
    ) -> Result<Vec<BacklinkRef>, LinkError> {
        let Some(target) = storage.get_note_by_id(note_id, ctx).await? else {
            return Ok(Vec::new());
        };

        // Match links by title, slug, or either of those for a historical alias so
        // that `[[how-sliding-panes-work]]` resolves just like `[[How Sliding Panes Work]]`.
        let mut target_names: Vec<String> = vec![
            target.title.to_lowercase(),
            target.slug.to_lowercase(),
        ];
        target_names.extend(
            target
                .aliases
                .iter()
                .flat_map(|a| [a.title.to_lowercase(), a.slug.to_lowercase()]),
        );

        let store = storage.module_store(NAMESPACE);
        let entries = store.list_prefix(OUT_PREFIX, None).await?;

        let mut backlinks = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for (key, value) in entries {
            let Some(referring_id) = key.strip_prefix(OUT_PREFIX) else {
                continue;
            };
            if referring_id == note_id.as_str() || !seen.insert(referring_id.to_owned()) {
                continue;
            }

            let Ok(links) = serde_json::from_value::<Vec<StoredLink>>(value) else {
                continue;
            };
            let cites_target = links.iter().any(|link| {
                target_names
                    .iter()
                    .any(|name| link.target_title.trim().to_lowercase() == *name)
            });
            if !cites_target {
                continue;
            }

            // Visibility is enforced by the storage layer (`ctx`), so private
            // referring notes are silently omitted for public readers.
            if let Some(referrer) = storage
                .get_note_by_id(&NoteId::from(referring_id), ctx)
                .await?
            {
                backlinks.push(BacklinkRef {
                    note_id: referrer.id,
                    slug: referrer.slug,
                    title: referrer.title,
                });
            }
        }

        backlinks.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.note_id.cmp(&b.note_id))
        });

        Ok(backlinks)
    }

    async fn rewrite_links_for_rename(
        &self,
        old_title: &str,
        new_title: &str,
        storage: &dyn Storage,
    ) -> Result<Vec<Note>, LinkError> {
        let old_title = old_title.trim();
        let new_title = new_title.trim();
        let old_lower = old_title.to_lowercase();
        // Also rewrite slug-style links (`[[old-slug]]`) to the new title.
        let old_slug_lower = slugify(old_title);

        let notes = storage
            .list_notes(NoteListFilter::default(), ReadContext::ADMIN)
            .await?;

        let mut modified = Vec::new();

        for mut note in notes {
            let segments = scan(&note.markdown_body);
            let mut changed = false;
            let mut body = String::with_capacity(note.markdown_body.len());

            for segment in &segments {
                match segment {
                    Segment::Text(text) => body.push_str(text),
                    Segment::Link {
                        raw,
                        target,
                        display,
                        has_display,
                    } => {
                        let target_lower = target.trim().to_lowercase();
                        if target_lower == old_lower || target_lower == old_slug_lower {
                            changed = true;
                            if *has_display {
                                body.push_str("[[");
                                body.push_str(new_title);
                                body.push('|');
                                body.push_str(display);
                                body.push_str("]]");
                            } else {
                                body.push_str("[[");
                                body.push_str(new_title);
                                body.push_str("]]");
                            }
                        } else {
                            body.push_str(raw);
                        }
                    }
                }
            }

            if changed {
                note.markdown_body = body;
                note.updated_at = Utc::now();
                // Pure computation: the caller persists these notes atomically
                // (`Storage::rename_note_atomic`) and only then re-syncs edges.
                // Writing to `module_kv` here would leave stale graph edges if
                // the rename later fails on a slug/title conflict.
                modified.push(note);
            }
        }

        Ok(modified)
    }
}

/// Persisted edge shape for a single outgoing wikilink.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredLink {
    target_title: String,
    display_text: String,
    raw: String,
}

impl From<ParsedLink> for StoredLink {
    fn from(link: ParsedLink) -> Self {
        Self {
            target_title: link.target_title,
            display_text: link.display_text,
            raw: link.raw,
        }
    }
}

fn out_key(note_id: &NoteId) -> String {
    format!("{OUT_PREFIX}{}", note_id.as_str())
}

async fn resolve_target(
    target: &str,
    storage: &dyn Storage,
    ctx: ReadContext,
) -> Result<Option<ResolvedTarget>, LinkError> {
    if let Some(note) = storage.get_note_by_title(target, ctx).await? {
        return Ok(Some(ResolvedTarget {
            note_id: note.id,
            slug: note.slug,
            title: note.title,
        }));
    }

    if let Some(lookup) = storage.get_note_by_slug_or_alias(target, ctx).await? {
        return Ok(Some(ResolvedTarget {
            note_id: lookup.note.id,
            slug: lookup.note.slug,
            title: lookup.note.title,
        }));
    }

    Ok(None)
}

fn render_resolved(target: &ResolvedTarget, display: &str) -> String {
    let href = format!("/{}", target.slug);
    format!(
        "<a href=\"{}\" data-zcast-note=\"{}\">{}</a>",
        escape_html(&href),
        escape_html(&target.slug),
        escape_html(display)
    )
}

fn render_unresolved_admin(target_title: &str, display: &str) -> String {
    format!(
        "<a href=\"#\" class=\"zcast-unresolved\" data-zcast-unresolved=\"{}\">{}</a>",
        escape_html(target_title),
        escape_html(display)
    )
}

fn render_unresolved_public(display: &str) -> String {
    format!(
        "<span class=\"zcast-unresolved-public\">{}</span>",
        escape_html(display)
    )
}

fn render_gfm(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(
        Options::ENABLE_GFM
            | Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS,
    );

    let parser = Parser::new_ext(markdown, options);
    let mut output = String::new();
    html::push_html(&mut output, parser);
    output
}

fn sanitise(rendered: &str) -> String {
    ammonia::Builder::new()
        .add_tags(["u"])
        .add_generic_attributes(["class", "data-zcast-note", "data-zcast-unresolved"])
        .link_rel(None)
        .url_relative(ammonia::UrlRelative::PassThrough)
        .clean(rendered)
        .to_string()
}

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// A run of Markdown, either verbatim text or a parsed wikilink.
#[derive(Debug)]
enum Segment<'a> {
    Text(&'a str),
    Link {
        raw: &'a str,
        target: &'a str,
        display: &'a str,
        has_display: bool,
    },
}

/// Split Markdown into literal text and wikilinks, skipping fenced code blocks
/// and inline code spans.
fn scan(markdown: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut fence: Option<(u8, usize)> = None;

    for line in markdown.split_inclusive('\n') {
        if let Some((marker, length)) = fence {
            segments.push(Segment::Text(line));
            if is_closing_fence(line, marker, length) {
                fence = None;
            }
            continue;
        }

        if let Some((marker, length)) = opening_fence(line) {
            fence = Some((marker, length));
            segments.push(Segment::Text(line));
            continue;
        }

        scan_line(line, &mut segments);
    }

    segments
}

fn opening_fence(line: &str) -> Option<(u8, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }

    let marker = *trimmed.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }

    let length = trimmed.bytes().take_while(|b| *b == marker).count();
    if length < 3 {
        return None;
    }

    // A backtick fence's info string may not contain backticks.
    if marker == b'`' && trimmed[length..].contains('`') {
        return None;
    }

    Some((marker, length))
}

fn is_closing_fence(line: &str, marker: u8, min_length: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }

    let length = trimmed.bytes().take_while(|b| *b == marker).count();
    length >= min_length && trimmed[length..].trim().is_empty()
}

fn scan_line<'a>(line: &'a str, out: &mut Vec<Segment<'a>>) {
    let bytes = line.as_bytes();
    let length = bytes.len();
    let mut cursor = 0;
    let mut text_start = 0;

    while cursor < length {
        if bytes[cursor] == b'`' {
            let run = bytes[cursor..].iter().take_while(|b| **b == b'`').count();
            let mut probe = cursor + run;
            let mut closed = false;

            while probe < length {
                if bytes[probe] == b'`' {
                    let close_run = bytes[probe..].iter().take_while(|b| **b == b'`').count();
                    if close_run == run {
                        cursor = probe + close_run;
                        closed = true;
                        break;
                    }
                    probe += close_run;
                } else {
                    probe += next_char_len(&line[probe..]);
                }
            }

            if !closed {
                cursor += run;
            }
            continue;
        }

        if bytes[cursor] == b'[' && cursor + 1 < length && bytes[cursor + 1] == b'[' {
            if let Some(inner_len) = find_closer(&line[cursor + 2..])
                && let Some((target, display, has_display)) =
                    parse_inner(&line[cursor + 2..cursor + 2 + inner_len])
            {
                let end = cursor + 2 + inner_len + 2;
                if cursor > text_start {
                    out.push(Segment::Text(&line[text_start..cursor]));
                }
                out.push(Segment::Link {
                    raw: &line[cursor..end],
                    target,
                    display,
                    has_display,
                });
                cursor = end;
                text_start = end;
                continue;
            }

            cursor += 2;
            continue;
        }

        cursor += next_char_len(&line[cursor..]);
    }

    if text_start < length {
        out.push(Segment::Text(&line[text_start..]));
    }
}

fn find_closer(input: &str) -> Option<usize> {
    input.find("]]")
}

fn parse_inner(inner: &str) -> Option<(&str, &str, bool)> {
    let inner = inner.trim();
    if inner.is_empty() || inner.contains("[[") {
        return None;
    }

    match inner.split_once('|') {
        Some((target, display)) => {
            let target = target.trim();
            if target.is_empty() {
                return None;
            }
            let display = display.trim();
            Some((
                target,
                if display.is_empty() { target } else { display },
                true,
            ))
        }
        None => Some((inner, inner, false)),
    }
}

fn next_char_len(input: &str) -> usize {
    input.chars().next().map(char::len_utf8).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcast_interfaces::{NoteAlias, Storage};
    use zcast_storage_sqlite::SqliteStorage;

    async fn storage() -> SqliteStorage {
        let storage = SqliteStorage::connect(":memory:").await.expect("connect");
        storage.initialize().await.expect("initialize");
        storage
    }

    async fn save(storage: &SqliteStorage, title: &str, body: &str) -> Note {
        let note = Note::new(title, body, false);
        storage.save_note(&note).await.expect("save");
        note
    }

    async fn save_private(storage: &SqliteStorage, title: &str, body: &str) -> Note {
        let note = Note::new(title, body, true);
        storage.save_note(&note).await.expect("save");
        note
    }

    async fn render(
        parser: &WikilinkParser,
        markdown: &str,
        storage: &SqliteStorage,
        ctx: ReadContext,
    ) -> String {
        parser
            .render_markdown_with_links(markdown, storage, ctx)
            .await
            .expect("render")
            .into_string()
    }

    #[test]
    fn manifest_declares_links_contract() {
        let manifest = WikilinkParser::new().manifest();
        assert_eq!(manifest.id, "zcast-links-wikilink");
        assert_eq!(manifest.provides.len(), 1);
        assert_eq!(manifest.provides[0].name, IFACE_LINKS);
        assert_eq!(manifest.provides[0].version, "1.0.0");
        assert_eq!(manifest.requires.len(), 1);
        assert_eq!(manifest.requires[0].name, IFACE_STORAGE);
        assert_eq!(manifest.requires[0].version_req, "^1.0");
        assert!(!manifest.requires[0].optional);
    }

    #[test]
    fn extracts_wikilinks_with_and_without_display() {
        let parser = WikilinkParser::new();
        let links = parser.extract_links("Intro [[Alpha]] and [[Beta|the beta]].");
        assert_eq!(links.len(), 2);
        assert_eq!(
            links[0],
            ParsedLink {
                raw: "[[Alpha]]".into(),
                target_title: "Alpha".into(),
                display_text: "Alpha".into(),
            }
        );
        assert_eq!(
            links[1],
            ParsedLink {
                raw: "[[Beta|the beta]]".into(),
                target_title: "Beta".into(),
                display_text: "the beta".into(),
            }
        );
    }

    #[test]
    fn extraction_ignores_fenced_and_inline_code() {
        let parser = WikilinkParser::new();
        let markdown = "\
[[Top]]

```
[[InFence]]
[[AlsoInFence]]
```

~~~text
[[TildeFence]]
~~~

Inline `[[InCode]]` and ``code with [[Tick]]`` then [[Real]].";

        let links = parser.extract_links(markdown);
        let targets: Vec<_> = links.iter().map(|l| l.target_title.as_str()).collect();
        assert_eq!(targets, ["Top", "Real"]);
    }

    #[test]
    fn extraction_ignores_malformed_links() {
        let parser = WikilinkParser::new();
        let links = parser.extract_links("[[|empty]] [[   ]] [[ok]] [[unclosed");
        let targets: Vec<_> = links.iter().map(|l| l.target_title.as_str()).collect();
        assert_eq!(targets, ["ok"]);
    }

    #[tokio::test]
    async fn renders_resolved_link_by_title_and_slug() {
        let storage = storage().await;
        save(&storage, "Target Note", "body").await;

        let parser = WikilinkParser::new();
        let html = render(
            &parser,
            "See [[Target Note]].",
            &storage,
            ReadContext::PUBLIC,
        )
        .await;
        assert!(html.contains("href=\"/target-note\""), "{html}");
        assert!(html.contains("data-zcast-note=\"target-note\""), "{html}");
        assert!(html.contains(">Target Note</a>"), "{html}");

        let by_slug = render(
            &parser,
            "See [[target-note|the note]].",
            &storage,
            ReadContext::PUBLIC,
        )
        .await;
        assert!(
            by_slug.contains("data-zcast-note=\"target-note\""),
            "{by_slug}"
        );
        assert!(by_slug.contains(">the note</a>"), "{by_slug}");
    }

    #[tokio::test]
    async fn renders_private_target_as_plain_text_publicly() {
        let storage = storage().await;
        save_private(&storage, "Hidden Plans", "secret").await;

        let parser = WikilinkParser::new();
        let html = render(
            &parser,
            "Read [[Hidden Plans]] now.",
            &storage,
            ReadContext::PUBLIC,
        )
        .await;

        assert!(html.contains("zcast-unresolved-public"), "{html}");
        assert!(!html.contains("hidden-plans"), "slug leaked: {html}");
        assert!(!html.contains("data-zcast-note"), "note id leaked: {html}");
        assert!(
            !html.contains("<a "),
            "private link must not be clickable: {html}"
        );
        assert!(html.contains("Hidden Plans"), "{html}");
    }

    #[tokio::test]
    async fn renders_missing_target_per_context() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        let public = render(&parser, "[[Missing Note]]", &storage, ReadContext::PUBLIC).await;
        assert!(
            public.contains("class=\"zcast-unresolved-public\""),
            "{public}"
        );
        assert!(!public.contains("data-zcast-unresolved"), "{public}");

        let admin = render(&parser, "[[Missing Note]]", &storage, ReadContext::ADMIN).await;
        assert!(admin.contains("class=\"zcast-unresolved\""), "{admin}");
        assert!(
            admin.contains("data-zcast-unresolved=\"Missing Note\""),
            "{admin}"
        );
        assert!(admin.contains("href=\"#\""), "{admin}");
    }

    #[tokio::test]
    async fn rendering_keeps_markdown_and_sanitises_html() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        let html = render(
            &parser,
            "# Title\n\nSome **bold** and `<code>`.\n\n[[Missing|A <b> & \"B\"]]\n\n<script>alert(1)</script>",
            &storage,
            ReadContext::PUBLIC,
        )
        .await;

        assert!(html.contains("<h1>Title</h1>"), "{html}");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("&lt;b&gt; &amp;"), "{html}");
        assert!(!html.contains("<script"), "{html}");
        assert!(
            !html.contains("<b>"),
            "raw inline HTML must be escaped: {html}"
        );
    }

    #[tokio::test]
    async fn rendering_leaves_code_untouched() {
        let storage = storage().await;
        save(&storage, "Target Note", "").await;
        let parser = WikilinkParser::new();

        let html = render(
            &parser,
            "Inline `[[Target Note]]` and\n\n```\n[[Target Note]]\n```\n",
            &storage,
            ReadContext::PUBLIC,
        )
        .await;

        assert!(html.contains("<code>[[Target Note]]</code>"), "{html}");
        assert!(html.contains("[[Target Note]]\n"), "{html}");
        assert!(!html.contains("data-zcast-note"), "{html}");
    }

    #[tokio::test]
    async fn sync_and_backlinks_respect_visibility() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        let alpha = save(&storage, "Alpha", "root").await;
        let beta = save(&storage, "Beta", "links to [[Alpha]]").await;
        let gamma = save_private(&storage, "Gamma", "secret [[alpha]] link").await;
        let delta = save(&storage, "Delta", "no links here").await;

        for note in [&alpha, &beta, &gamma, &delta] {
            parser.sync_note_links(note, &storage).await.expect("sync");
        }

        let public = parser
            .get_backlinks(&alpha.id, &storage, ReadContext::PUBLIC)
            .await
            .expect("backlinks");
        assert_eq!(public.len(), 1, "{public:?}");
        assert_eq!(public[0].note_id, beta.id);
        assert_eq!(public[0].slug, "beta");

        let admin = parser
            .get_backlinks(&alpha.id, &storage, ReadContext::ADMIN)
            .await
            .expect("backlinks");
        let titles: Vec<_> = admin.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(titles, ["Beta", "Gamma"]);

        // An unknown target has no backlinks.
        assert!(
            parser
                .get_backlinks(&NoteId::from("nope"), &storage, ReadContext::ADMIN)
                .await
                .expect("backlinks")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn backlinks_match_aliases_and_are_sorted() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        let mut alpha = save(&storage, "Alpha", "").await;
        alpha.aliases.push(NoteAlias {
            slug: "old-alpha".into(),
            title: "Old Alpha".into(),
            created_at: Utc::now(),
        });
        storage.save_note(&alpha).await.expect("save alias");

        let zeta = save(&storage, "Zeta", "[[Old Alpha]]").await;
        let beta = save(&storage, "Beta", "[[Alpha]]").await;
        for note in [&zeta, &beta] {
            parser.sync_note_links(note, &storage).await.expect("sync");
        }

        let backlinks = parser
            .get_backlinks(&alpha.id, &storage, ReadContext::ADMIN)
            .await
            .expect("backlinks");
        let titles: Vec<_> = backlinks.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(titles, ["Beta", "Zeta"]);
    }

    #[tokio::test]
    async fn backlinks_match_slug_and_alias_slug_links() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        let mut panes = save(&storage, "How Sliding Panes Work", "root").await;
        panes.aliases.push(NoteAlias {
            slug: "old-panes".into(),
            title: "Old Panes".into(),
            created_at: Utc::now(),
        });
        storage.save_note(&panes).await.expect("save alias");

        let by_slug = save(&storage, "By Slug", "[[how-sliding-panes-work]]").await;
        let by_alias_slug = save(&storage, "By Alias Slug", "[[old-panes|panes]]").await;
        let unrelated = save(&storage, "Unrelated", "[[elsewhere]]").await;
        for note in [&by_slug, &by_alias_slug, &unrelated] {
            parser.sync_note_links(note, &storage).await.expect("sync");
        }

        let backlinks = parser
            .get_backlinks(&panes.id, &storage, ReadContext::ADMIN)
            .await
            .expect("backlinks");
        let ids: Vec<_> = backlinks.iter().map(|b| b.note_id.clone()).collect();
        assert_eq!(ids, [by_alias_slug.id, by_slug.id]);
    }

    #[tokio::test]
    async fn remove_note_links_clears_outgoing_edges() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        let alpha = save(&storage, "Alpha", "").await;
        let beta = save(&storage, "Beta", "[[Alpha]]").await;
        parser.sync_note_links(&beta, &storage).await.expect("sync");

        assert_eq!(
            parser
                .get_backlinks(&alpha.id, &storage, ReadContext::ADMIN)
                .await
                .expect("backlinks")
                .len(),
            1
        );

        parser
            .remove_note_links(&beta.id, &storage)
            .await
            .expect("remove");
        assert!(
            parser
                .get_backlinks(&alpha.id, &storage, ReadContext::ADMIN)
                .await
                .expect("backlinks")
                .is_empty()
        );

        // A note with no links keeps no edge record.
        let empty = save(&storage, "Empty", "plain text").await;
        parser
            .sync_note_links(&empty, &storage)
            .await
            .expect("sync");
        let store = storage.module_store(NAMESPACE);
        assert!(store.get(&out_key(&empty.id)).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn rewrite_links_for_rename_skips_code_and_preserves_display() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        let alpha = save(&storage, "Alpha", "root").await;
        let body = "\
[[Alpha]] and [[Alpha|see alpha]].
Also [[alpha]] once more.

`[[Alpha]]` stays.

```
[[Alpha]]
```
";
        let beta = save(&storage, "Beta", body).await;
        let unrelated = save(&storage, "Gamma", "[[Delta]]").await;

        let modified = parser
            .rewrite_links_for_rename("Alpha", "Alpha Prime", &storage)
            .await
            .expect("rewrite");

        assert_eq!(modified.len(), 1);
        let updated = &modified[0];
        assert_eq!(updated.id, beta.id);
        assert_eq!(
            updated.markdown_body,
            "\
[[Alpha Prime]] and [[Alpha Prime|see alpha]].
Also [[Alpha Prime]] once more.

`[[Alpha]]` stays.

```
[[Alpha]]
```
"
        );
        assert!(updated.updated_at >= beta.updated_at);

        // Rewriting is pure: the stored note and `module_kv` edges are untouched
        // until the caller commits via `rename_note_atomic` and re-syncs.
        let stored = storage
            .get_note_by_id(&beta.id, ReadContext::ADMIN)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(stored.markdown_body, body);

        // Unrelated notes and the renamed note itself are not returned.
        assert!(
            modified
                .iter()
                .all(|n| n.id != unrelated.id && n.id != alpha.id)
        );

        // Pre-existing edges still point at the old title because nothing was synced.
        let edges = storage
            .module_store(NAMESPACE)
            .get(&out_key(&beta.id))
            .await
            .expect("get");
        assert!(edges.is_none(), "rewrite must not write edges: {edges:?}");
    }

    #[tokio::test]
    async fn rewrite_links_for_rename_leaves_existing_edges_untouched() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        save(&storage, "Alpha", "root").await;
        let beta = save(&storage, "Beta", "[[Alpha]]").await;
        parser.sync_note_links(&beta, &storage).await.expect("sync");

        let modified = parser
            .rewrite_links_for_rename("Alpha", "Alpha Prime", &storage)
            .await
            .expect("rewrite");
        assert_eq!(modified.len(), 1);

        let store = storage.module_store(NAMESPACE);
        let edges: Vec<StoredLink> = serde_json::from_value(
            store
                .get(&out_key(&beta.id))
                .await
                .expect("get")
                .expect("edges"),
        )
        .expect("edges json");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_title, "Alpha");
    }

    #[tokio::test]
    async fn rewrite_links_for_rename_matches_slug_links() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        save(&storage, "Old Title", "root").await;
        let beta = save(
            &storage,
            "Beta",
            "[[old-title]] and [[OLD-TITLE|slug display]] and [[Old Title]]",
        )
        .await;

        let modified = parser
            .rewrite_links_for_rename("Old Title", "New Title", &storage)
            .await
            .expect("rewrite");
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].id, beta.id);
        assert_eq!(
            modified[0].markdown_body,
            "[[New Title]] and [[New Title|slug display]] and [[New Title]]"
        );
    }

    #[tokio::test]
    async fn rewrite_links_is_case_insensitive_and_ignores_other_targets() {
        let storage = storage().await;
        let parser = WikilinkParser::new();

        save(&storage, "Alpha", "").await;
        let beta = save(&storage, "Beta", "[[ALPHA|upper]] and [[Alphabet]]").await;

        let modified = parser
            .rewrite_links_for_rename("alpha", "Renamed", &storage)
            .await
            .expect("rewrite");
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].id, beta.id);
        assert_eq!(
            modified[0].markdown_body,
            "[[Renamed|upper]] and [[Alphabet]]"
        );
    }
}
