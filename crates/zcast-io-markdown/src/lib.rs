//! Markdown directory and zip archive exporter/importer for zcast.
//!
//! Notes are serialised as Markdown with a YAML-style frontmatter block. The
//! parser deliberately accepts plain Markdown and Obsidian-style notes with no
//! frontmatter, inferring a title from the first `# Heading` or the filename.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use zcast_interfaces::{
    ExportSummary, IFACE_IO, IFACE_STORAGE, ImportSummary, InterfaceReq, InterfaceSpec, IoError,
    ModuleManifest, Note, NoteAlias, NoteExporter, NoteId, NoteImporter, NoteListFilter, NoteSort,
    ReadContext, Storage, StorageError, slugify,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const MANIFEST: ModuleManifest = ModuleManifest {
    id: "zcast-io-markdown",
    name: "Markdown Export & Import",
    version: env!("CARGO_PKG_VERSION"),
    description: "Export and import zcast notes as Markdown directories or zip archives preserving [[wikilinks]]",
    provides: &[InterfaceSpec {
        name: IFACE_IO,
        version: "1.0.0",
    }],
    requires: &[InterfaceReq {
        name: IFACE_STORAGE,
        version_req: "^1.0",
        optional: false,
    }],
    recommends: &[],
};

const FRONTMATTER_DELIMITER: &str = "---";
const MACOSX_PREFIX: &str = "__MACOSX/";

/// Markdown directory/zip exporter and importer.
#[derive(Debug, Clone, Copy, Default)]
pub struct MarkdownIo;

impl MarkdownIo {
    pub fn new() -> Self {
        Self
    }

    /// Manifest describing this module's provided and required interfaces.
    pub fn manifest(&self) -> ModuleManifest {
        MANIFEST
    }
}

#[async_trait]
impl NoteExporter for MarkdownIo {
    fn manifest(&self) -> ModuleManifest {
        MANIFEST
    }

    async fn export_to_dir(
        &self,
        storage: &dyn Storage,
        dir: &Path,
        ctx: ReadContext,
    ) -> Result<ExportSummary, IoError> {
        fs::create_dir_all(dir)?;

        let (notes, skipped_private) = visible_notes(storage, ctx).await?;
        let mut used = HashMap::new();

        for note in &notes {
            let filename = unique_filename(&safe_base_name(note), &mut used);
            fs::write(dir.join(filename), format_note_markdown(note))?;
        }

        Ok(ExportSummary {
            exported: notes.len(),
            skipped_private,
        })
    }

    async fn export_to_zip_bytes(
        &self,
        storage: &dyn Storage,
        ctx: ReadContext,
    ) -> Result<(Vec<u8>, ExportSummary), IoError> {
        let (notes, skipped_private) = visible_notes(storage, ctx).await?;

        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut used = HashMap::new();

        for note in &notes {
            let filename = unique_filename(&safe_base_name(note), &mut used);
            writer
                .start_file(filename, options)
                .map_err(|error| IoError::Format(error.to_string()))?;
            writer.write_all(format_note_markdown(note).as_bytes())?;
        }

        let cursor = writer
            .finish()
            .map_err(|error| IoError::Format(error.to_string()))?;

        Ok((
            cursor.into_inner(),
            ExportSummary {
                exported: notes.len(),
                skipped_private,
            },
        ))
    }
}

#[async_trait]
impl NoteImporter for MarkdownIo {
    fn manifest(&self) -> ModuleManifest {
        MANIFEST
    }

    async fn import_from_dir(
        &self,
        storage: &dyn Storage,
        dir: &Path,
    ) -> Result<ImportSummary, IoError> {
        let mut summary = ImportSummary::default();

        for path in collect_markdown_files(dir)? {
            let raw = fs::read_to_string(&path)?;
            let filename = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();

            record_import(storage, &filename, &raw, &mut summary).await?;
        }

        Ok(summary)
    }

    async fn import_from_zip_bytes(
        &self,
        storage: &dyn Storage,
        bytes: &[u8],
    ) -> Result<ImportSummary, IoError> {
        let mut archive = ZipArchive::new(Cursor::new(bytes))
            .map_err(|error| IoError::Format(error.to_string()))?;
        let mut summary = ImportSummary::default();

        let mut entries: Vec<(String, usize)> = (0..archive.len())
            .filter_map(|index| {
                let entry = archive.by_index(index).ok()?;
                let name = entry.name().to_owned();
                if entry.is_dir() || !safe_zip_entry_name(&name) {
                    return None;
                }
                Some((name, index))
            })
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));

        for (name, index) in entries {
            let mut entry = archive
                .by_index(index)
                .map_err(|error| IoError::Format(error.to_string()))?;
            let mut raw = String::new();
            entry.read_to_string(&mut raw)?;

            record_import(storage, &name, &raw, &mut summary).await?;
        }

        Ok(summary)
    }
}

/// Serialise a note as frontmatter plus canonical Markdown, preserving wikilinks.
pub fn format_note_markdown(note: &Note) -> String {
    let mut output = String::new();
    output.push_str(FRONTMATTER_DELIMITER);
    output.push('\n');
    output.push_str("id: ");
    output.push_str(note.id.as_str());
    output.push('\n');
    output.push_str("title: ");
    output.push_str(&quote_string(&note.title));
    output.push('\n');
    output.push_str("slug: ");
    output.push_str(&plain_or_quoted(&note.slug));
    output.push('\n');
    output.push_str("private: ");
    output.push_str(if note.private { "true" } else { "false" });
    output.push('\n');

    if !note.aliases.is_empty() {
        output.push_str("aliases:\n");
        for alias in &note.aliases {
            output.push_str("  - ");
            output.push_str(&plain_or_quoted(&format!("{}|{}", alias.slug, alias.title)));
            output.push('\n');
        }
    }

    output.push_str("created_at: ");
    output.push_str(&format_timestamp(note.created_at));
    output.push('\n');
    output.push_str("updated_at: ");
    output.push_str(&format_timestamp(note.updated_at));
    output.push('\n');
    output.push_str(FRONTMATTER_DELIMITER);
    output.push_str("\n\n");
    output.push_str(&note.markdown_body);
    output
}

/// Parse a Markdown file, with or without frontmatter, into a [`Note`].
pub fn parse_note_markdown(filename: &str, raw: &str) -> Result<Note, IoError> {
    if filename.trim().is_empty() {
        return Err(IoError::Format("filename must not be empty".to_owned()));
    }

    let now = Utc::now();
    let (frontmatter, body) = match split_frontmatter(raw) {
        Some((frontmatter, body)) => (Some(frontmatter), clean_body(body)),
        None => (None, clean_body(raw)),
    };

    let mut id = None;
    let mut title = None;
    let mut slug = None;
    let mut private = false;
    let mut aliases = Vec::new();
    let mut created_at = None;
    let mut updated_at = None;

    if let Some(frontmatter) = frontmatter {
        parse_frontmatter(
            frontmatter,
            &mut id,
            &mut title,
            &mut slug,
            &mut private,
            &mut aliases,
            &mut created_at,
            &mut updated_at,
        );
    }

    let stem = filename_stem(filename);
    let title = title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| infer_title(&body, &stem));
    let slug = slug
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| slugify(&title));
    let created_at = created_at.unwrap_or(now);
    let updated_at = updated_at.unwrap_or(created_at);

    let aliases = aliases
        .into_iter()
        .map(|(alias_slug, alias_title)| NoteAlias {
            slug: if alias_slug.trim().is_empty() {
                slugify(&alias_title)
            } else {
                alias_slug
            },
            title: alias_title,
            created_at,
        })
        .collect();

    Ok(Note {
        id: id.unwrap_or_else(NoteId::new),
        slug,
        title,
        markdown_body: body,
        private,
        aliases,
        ext: Default::default(),
        created_at,
        updated_at,
    })
}

async fn visible_notes(
    storage: &dyn Storage,
    ctx: ReadContext,
) -> Result<(Vec<Note>, usize), IoError> {
    let filter = || NoteListFilter {
        sort: NoteSort::TitleAsc,
        ..Default::default()
    };

    let notes = storage.list_notes(filter(), ctx).await?;
    let skipped_private = if ctx.include_private {
        0
    } else {
        storage
            .list_notes(filter(), ReadContext::ADMIN)
            .await?
            .iter()
            .filter(|note| note.private)
            .count()
    };

    Ok((notes, skipped_private))
}

fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let mut lines = raw.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end_matches(['\r', '\n']) != FRONTMATTER_DELIMITER {
        return None;
    }

    let mut offset = first.len();
    for line in lines {
        let content = line.trim_end_matches(['\r', '\n']);
        if content == FRONTMATTER_DELIMITER {
            let frontmatter = &raw[first.len()..offset];
            let body = &raw[offset + line.len()..];
            return Some((frontmatter, body));
        }
        offset += line.len();
    }

    None
}

fn clean_body(body: &str) -> String {
    let body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    body.trim_end_matches(['\r', '\n']).to_owned()
}

#[allow(clippy::too_many_arguments)]
fn parse_frontmatter(
    frontmatter: &str,
    id: &mut Option<NoteId>,
    title: &mut Option<String>,
    slug: &mut Option<String>,
    private: &mut bool,
    aliases: &mut Vec<(String, String)>,
    created_at: &mut Option<DateTime<Utc>>,
    updated_at: &mut Option<DateTime<Utc>>,
) {
    let mut in_aliases = false;

    for line in frontmatter.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        if in_aliases {
            if let Some(item) = line.trim_start().strip_prefix('-') {
                aliases.push(parse_alias(item.trim()));
                continue;
            }
            if !line.starts_with(char::is_whitespace) {
                in_aliases = false;
            } else {
                continue;
            }
        }

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match key {
            "id" => {
                let parsed = unquote(value);
                if !parsed.is_empty() {
                    *id = Some(NoteId::from(parsed));
                }
            }
            "title" => *title = Some(unquote(value)),
            "slug" => *slug = Some(unquote(value)),
            "private" => *private = parse_bool(value),
            "aliases" => {
                let value = unquote(value);
                if value.starts_with('[') && value.ends_with(']') {
                    for item in value[1..value.len() - 1].split(',') {
                        if !item.trim().is_empty() {
                            aliases.push(parse_alias(item.trim()));
                        }
                    }
                } else {
                    in_aliases = true;
                }
            }
            "created_at" => *created_at = parse_timestamp(value),
            "updated_at" => *updated_at = parse_timestamp(value),
            _ => {}
        }
    }
}

fn parse_alias(value: &str) -> (String, String) {
    match unquote(value).split_once('|') {
        Some((slug, title)) => (slug.trim().to_owned(), title.trim().to_owned()),
        None => {
            let title = unquote(value).trim().to_owned();
            (slugify(&title), title)
        }
    }
}

fn parse_bool(value: &str) -> bool {
    matches!(
        unquote(value).trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(unquote(value).trim())
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn quote_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{}\"", value.escape_default()))
}

fn plain_or_quoted(value: &str) -> String {
    if is_plain_safe(value) {
        value.to_owned()
    } else {
        quote_string(value)
    }
}

fn is_plain_safe(value: &str) -> bool {
    if value.is_empty() || value.trim() != value {
        return false;
    }
    if value.chars().next().is_some_and(|ch| {
        matches!(
            ch,
            '-' | '?'
                | ':'
                | ','
                | '['
                | ']'
                | '{'
                | '}'
                | '#'
                | '&'
                | '*'
                | '!'
                | '|'
                | '>'
                | '\''
                | '"'
                | '%'
                | '@'
                | '`'
        )
    }) {
        return false;
    }
    !value
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, ':' | '#' | '"' | '\\'))
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        if let Ok(decoded) = serde_json::from_str::<String>(value) {
            return decoded;
        }
        return value[1..value.len() - 1].to_owned();
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("''", "'");
    }
    value.to_owned()
}

fn infer_title(body: &str, stem: &str) -> String {
    for line in body.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix("# ") {
            let heading = heading.trim().trim_end_matches('#').trim();
            if !heading.is_empty() {
                return heading.to_owned();
            }
        }
    }

    humanise_stem(stem)
}

fn humanise_stem(stem: &str) -> String {
    let mut words = Vec::new();
    for word in stem.split(|ch: char| ch == '-' || ch == '_' || ch == '.' || ch.is_whitespace()) {
        if word.is_empty() {
            continue;
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            words.push(format!("{}{}", first.to_uppercase(), chars.as_str()));
        }
    }

    if words.is_empty() {
        "Untitled".to_owned()
    } else {
        words.join(" ")
    }
}

fn filename_stem(filename: &str) -> String {
    let name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let stem = if name.len() > 3 && name[name.len() - 3..].eq_ignore_ascii_case(".md") {
        &name[..name.len() - 3]
    } else {
        name
    };
    stem.trim().to_owned()
}

fn safe_base_name(note: &Note) -> String {
    let source = if note.slug.trim().is_empty() {
        note.title.as_str()
    } else {
        note.slug.as_str()
    };

    let mut base = String::with_capacity(source.len());
    for ch in source.trim().chars() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            base.push(ch);
        } else if ch.is_whitespace() {
            if !base.is_empty() && !base.ends_with('-') {
                base.push('-');
            }
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }

    let base = base.trim_matches(['-', '_']);
    if base.is_empty() {
        note.id.as_str().to_owned()
    } else {
        base.to_owned()
    }
}

fn unique_filename(base: &str, used: &mut HashMap<String, usize>) -> String {
    let key = base.to_ascii_lowercase();
    match used.entry(key) {
        Entry::Vacant(entry) => {
            entry.insert(1);
            format!("{base}.md")
        }
        Entry::Occupied(mut entry) => {
            let count = entry.get_mut();
            *count += 1;
            format!("{base}-{count}.md")
        }
    }
}

fn is_markdown_name(name: &str) -> bool {
    name.len() > 3 && name[name.len() - 3..].eq_ignore_ascii_case(".md")
}

fn collect_markdown_files(dir: &Path) -> Result<Vec<PathBuf>, IoError> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }

            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && is_markdown_name(&name) {
                files.push(path);
            }
        }
    }

    files.sort();
    Ok(files)
}

fn safe_zip_entry_name(name: &str) -> bool {
    let normalized = name.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.ends_with('/')
        || normalized.starts_with(MACOSX_PREFIX)
        || normalized.contains(&format!("/{MACOSX_PREFIX}"))
    {
        return false;
    }

    let mut basename = "";
    for part in normalized.split('/') {
        if part.is_empty() {
            continue;
        }
        if part == "." || part.contains("..") || part.starts_with('.') {
            return false;
        }
        basename = part;
    }

    is_markdown_name(basename)
}

async fn record_import(
    storage: &dyn Storage,
    filename: &str,
    raw: &str,
    summary: &mut ImportSummary,
) -> Result<(), IoError> {
    let note = match parse_note_markdown(filename, raw) {
        Ok(note) => note,
        Err(_) => {
            summary.skipped += 1;
            return Ok(());
        }
    };

    match upsert_note(storage, note).await? {
        UpsertOutcome::Created => summary.created += 1,
        UpsertOutcome::Updated => summary.updated += 1,
        UpsertOutcome::Skipped => summary.skipped += 1,
    }

    Ok(())
}

enum UpsertOutcome {
    Created,
    Updated,
    Skipped,
}

async fn upsert_note(storage: &dyn Storage, mut note: Note) -> Result<UpsertOutcome, IoError> {
    let by_id = storage.get_note_by_id(&note.id, ReadContext::ADMIN).await?;
    let by_slug = storage
        .get_note_by_slug_or_alias(&note.slug, ReadContext::ADMIN)
        .await?;
    let by_title = storage
        .get_note_by_title(&note.title, ReadContext::ADMIN)
        .await?;

    let mut matches = Vec::new();
    for candidate in [by_id, by_slug.map(|lookup| lookup.note), by_title]
        .into_iter()
        .flatten()
    {
        if !matches
            .iter()
            .any(|existing: &Note| existing.id == candidate.id)
        {
            matches.push(candidate);
        }
    }

    if matches.len() > 1 {
        return Ok(UpsertOutcome::Skipped);
    }

    let outcome = if let Some(existing) = matches.into_iter().next() {
        note.id = existing.id;
        note.created_at = existing.created_at;
        note.ext = existing.ext;
        if note.aliases.is_empty() {
            note.aliases = existing.aliases;
        }
        UpsertOutcome::Updated
    } else {
        UpsertOutcome::Created
    };

    match storage.save_note(&note).await {
        Ok(()) => Ok(outcome),
        Err(StorageError::SlugConflict(_)) | Err(StorageError::TitleConflict(_)) => {
            Ok(UpsertOutcome::Skipped)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests;
