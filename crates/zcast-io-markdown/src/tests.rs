use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{TimeZone, Utc};
use zcast_interfaces::{
    IFACE_IO, IFACE_STORAGE, Note, NoteAlias, NoteId, NoteImporter, ReadContext, Storage, slugify,
};
use zcast_storage_sqlite::SqliteStorage;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::{MarkdownIo, format_note_markdown, parse_note_markdown};
use zcast_interfaces::NoteExporter;

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zcast-io-markdown-{label}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn memory_storage() -> SqliteStorage {
    let storage = SqliteStorage::connect(":memory:").await.expect("connect");
    storage.initialize().await.expect("initialize");
    storage
}

fn at(seconds: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 21, 0, seconds)
        .single()
        .expect("valid timestamp")
}

fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for (name, content) in entries {
        writer.start_file(*name, options).expect("start zip entry");
        writer
            .write_all(content.as_bytes())
            .expect("write zip entry");
    }

    writer.finish().expect("finish zip").into_inner()
}

fn zip_names(bytes: &[u8]) -> Vec<String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("open zip");
    (0..archive.len())
        .map(|index| {
            archive
                .by_index(index)
                .expect("zip entry")
                .name()
                .to_owned()
        })
        .collect()
}

#[test]
fn manifest_declares_io_contract() {
    let manifest = MarkdownIo::new().manifest();

    assert_eq!(manifest.id, "zcast-io-markdown");
    assert_eq!(manifest.name, "Markdown Export & Import");
    assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        manifest.description,
        "Export and import zcast notes as Markdown directories or zip archives preserving [[wikilinks]]"
    );
    assert_eq!(
        manifest.provides,
        &[zcast_interfaces::InterfaceSpec {
            name: IFACE_IO,
            version: "1.0.0",
        }]
    );
    assert_eq!(
        manifest.requires,
        &[zcast_interfaces::InterfaceReq {
            name: IFACE_STORAGE,
            version_req: "^1.0",
            optional: false,
        }]
    );
    assert!(manifest.recommends.is_empty());
}

#[test]
fn frontmatter_roundtrips_with_quotes_colons_aliases_and_wikilinks() {
    let created = at(0);
    let updated = at(5);
    let title = "He said: \"Hello\" \\ path — ünïcode 🚀";
    let body = "Links to [[Other Note]] and [[Third|display]].\n\n## Section\n\nContent.";

    let mut note = Note::new(title, body, true);
    note.id = NoteId::from("01J00000000000000000000000");
    note.slug = "he-said-hello".to_owned();
    note.aliases = vec![
        NoteAlias {
            slug: "old-slug".to_owned(),
            title: "Old Title".to_owned(),
            created_at: created,
        },
        NoteAlias {
            slug: "quoted:alias".to_owned(),
            title: "A \"Quoted\" Alias".to_owned(),
            created_at: created,
        },
    ];
    note.created_at = created;
    note.updated_at = updated;

    let formatted = format_note_markdown(&note);
    assert!(formatted.starts_with("---\n"));
    assert!(formatted.contains("title: \"He said: \\\"Hello\\\""));
    assert!(formatted.contains("aliases:\n  - old-slug|Old Title\n"));
    assert!(formatted.contains("[[Other Note]]"));

    let parsed = parse_note_markdown("ignored-file.md", &formatted).expect("parse frontmatter");

    assert_eq!(parsed.id, note.id);
    assert_eq!(parsed.title, note.title);
    assert_eq!(parsed.slug, note.slug);
    assert!(parsed.private);
    assert_eq!(parsed.markdown_body, note.markdown_body);
    assert_eq!(parsed.created_at, note.created_at);
    assert_eq!(parsed.updated_at, note.updated_at);
    assert_eq!(parsed.aliases.len(), 2);
    assert_eq!(parsed.aliases[0].slug, "old-slug");
    assert_eq!(parsed.aliases[0].title, "Old Title");
    assert_eq!(parsed.aliases[1].title, "A \"Quoted\" Alias");
}

#[test]
fn plain_markdown_infers_title_from_heading_or_filename() {
    let with_heading = parse_note_markdown("my-cool-note.md", "# My Heading\n\nHello [[World]]\n")
        .expect("parse heading note");

    assert_eq!(with_heading.title, "My Heading");
    assert_eq!(with_heading.slug, "my-heading");
    assert!(!with_heading.private);
    assert!(with_heading.markdown_body.contains("# My Heading"));
    assert!(with_heading.markdown_body.contains("[[World]]"));

    let without_heading =
        parse_note_markdown("my_cool-note.md", "Just some text.\n").expect("parse plain note");

    assert_eq!(without_heading.title, "My Cool Note");
    assert_eq!(without_heading.slug, slugify("My Cool Note"));
    assert_eq!(without_heading.markdown_body, "Just some text.");
    assert!(!without_heading.id.as_str().is_empty());

    assert!(parse_note_markdown("   ", "Body").is_err());
}

#[tokio::test]
async fn directory_export_and_import_roundtrip() {
    let io = MarkdownIo::new();
    let source = memory_storage().await;

    let alpha = Note::new(
        "Alpha",
        "Alpha links to [[Beta]] and [[Gamma|display]].",
        false,
    );
    let beta = Note::new("Beta", "Beta body.", false);
    source.save_note(&alpha).await.expect("save alpha");
    source.save_note(&beta).await.expect("save beta");

    let dir = TempDir::new("dir-roundtrip");
    let exported = io
        .export_to_dir(&source, dir.path(), ReadContext::ADMIN)
        .await
        .expect("export");

    assert_eq!(exported.exported, 2);
    assert_eq!(exported.skipped_private, 0);
    assert!(dir.path().join("alpha.md").is_file());
    assert!(dir.path().join("beta.md").is_file());

    let target = memory_storage().await;
    let imported = io
        .import_from_dir(&target, dir.path())
        .await
        .expect("import");

    assert_eq!(imported.created, 2);
    assert_eq!(imported.updated, 0);
    assert_eq!(imported.skipped, 0);

    let imported_alpha = target
        .get_note_by_id(&alpha.id, ReadContext::ADMIN)
        .await
        .expect("lookup")
        .expect("alpha exists");
    assert_eq!(imported_alpha.markdown_body, alpha.markdown_body);
    assert_eq!(imported_alpha.title, alpha.title);
    assert!(imported_alpha.markdown_body.contains("[[Beta]]"));

    let reimported = io
        .import_from_dir(&target, dir.path())
        .await
        .expect("reimport");

    assert_eq!(reimported.created, 0);
    assert_eq!(reimported.updated, 2);
    assert_eq!(reimported.skipped, 0);

    let notes = target
        .list_notes(Default::default(), ReadContext::ADMIN)
        .await
        .expect("list");
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().any(|note| note.id == alpha.id));
}

#[tokio::test]
async fn zip_export_and_import_roundtrip() {
    let io = MarkdownIo::new();
    let source = memory_storage().await;

    let alpha = Note::new("Alpha", "Alpha links to [[Beta]].", false);
    let beta = Note::new("Beta", "Beta body.", false);
    source.save_note(&alpha).await.expect("save alpha");
    source.save_note(&beta).await.expect("save beta");

    let (bytes, exported) = io
        .export_to_zip_bytes(&source, ReadContext::ADMIN)
        .await
        .expect("export zip");

    assert_eq!(exported.exported, 2);
    assert_eq!(exported.skipped_private, 0);

    let names = zip_names(&bytes);
    assert_eq!(names, vec!["alpha.md", "beta.md"]);

    let target = memory_storage().await;
    let imported = io
        .import_from_zip_bytes(&target, &bytes)
        .await
        .expect("import zip");

    assert_eq!(imported.created, 2);
    assert_eq!(imported.updated, 0);
    assert_eq!(imported.skipped, 0);

    let imported_alpha = target
        .get_note_by_id(&alpha.id, ReadContext::ADMIN)
        .await
        .expect("lookup")
        .expect("alpha exists");
    assert_eq!(imported_alpha.markdown_body, alpha.markdown_body);
    assert!(imported_alpha.markdown_body.contains("[[Beta]]"));

    let reimported = io
        .import_from_zip_bytes(&target, &bytes)
        .await
        .expect("reimport zip");

    assert_eq!(reimported.created, 0);
    assert_eq!(reimported.updated, 2);
    assert_eq!(reimported.skipped, 0);
}

#[tokio::test]
async fn public_export_excludes_private_notes_and_admin_includes_them() {
    let io = MarkdownIo::new();
    let storage = memory_storage().await;

    let public = Note::new("Public Note", "Visible to everyone.", false);
    let private = Note::new("Private Note", "Admin only.", true);
    storage.save_note(&public).await.expect("save public");
    storage.save_note(&private).await.expect("save private");

    let public_dir = TempDir::new("public-export");
    let public_summary = io
        .export_to_dir(&storage, public_dir.path(), ReadContext::PUBLIC)
        .await
        .expect("public export");

    assert_eq!(public_summary.exported, 1);
    assert_eq!(public_summary.skipped_private, 1);
    assert!(public_dir.path().join("public-note.md").is_file());
    assert!(!public_dir.path().join("private-note.md").exists());

    let admin_dir = TempDir::new("admin-export");
    let admin_summary = io
        .export_to_dir(&storage, admin_dir.path(), ReadContext::ADMIN)
        .await
        .expect("admin export");

    assert_eq!(admin_summary.exported, 2);
    assert_eq!(admin_summary.skipped_private, 0);
    assert!(admin_dir.path().join("private-note.md").is_file());

    let (_, zip_summary) = io
        .export_to_zip_bytes(&storage, ReadContext::PUBLIC)
        .await
        .expect("public zip export");
    assert_eq!(zip_summary.exported, 1);
    assert_eq!(zip_summary.skipped_private, 1);
}

#[tokio::test]
async fn zip_import_skips_traversal_hidden_and_non_markdown_entries() {
    let io = MarkdownIo::new();
    let storage = memory_storage().await;

    let bytes = zip_bytes(&[
        ("good.md", "# Good\n\nSafe body [[Link]]."),
        ("notes/nested.md", "# Nested\n\nNested body."),
        ("../evil.md", "# Evil\n\nShould not import."),
        ("/absolute.md", "# Absolute\n\nShould not import."),
        ("notes/../escape.md", "# Escape\n\nShould not import."),
        ("__MACOSX/._good.md", "# Junk\n\nShould not import."),
        (".hidden.md", "# Hidden\n\nShould not import."),
        (".obsidian/config.md", "# Config\n\nShould not import."),
        ("notes/readme.txt", "not markdown"),
        ("folder/", ""),
    ]);

    let imported = io
        .import_from_zip_bytes(&storage, &bytes)
        .await
        .expect("import zip");

    assert_eq!(imported.created, 2);
    assert_eq!(imported.updated, 0);
    assert_eq!(imported.skipped, 0);

    let notes = storage
        .list_notes(Default::default(), ReadContext::ADMIN)
        .await
        .expect("list notes");
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().any(|note| note.title == "Good"));
    assert!(notes.iter().any(|note| note.title == "Nested"));

    let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("open zip");
    let mut entry = archive.by_name("good.md").expect("good entry");
    let mut body = String::new();
    entry.read_to_string(&mut body).expect("read good entry");
    assert!(body.contains("Safe body [[Link]]."));
}
