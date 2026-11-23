# `zcast-interfaces` — Contract Sketches (`Storage` & `SearchProvider`)

> **Status:** Design Sketch (Illustrative Rust traits — not compiled implementation code)
> **Purpose:** Concrete preview of how `zcast-interfaces` models versioned module contracts, core `Note` operations, DB-agnostic module persistence (`ModuleStore`), visibility enforcement (`ReadContext`), and swappable `SearchProvider` implementations.

---

## 1. Shared Primitives & Module Metadata

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Immutable, time-sortable note identifier (ULID).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NoteId(pub String);

/// Historical slug/title preserved so DEC-02 renames redirect cleanly without 404s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteAlias {
    pub slug: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

/// The sacred minimal Core `Note` Primitive (DEC-06).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: NoteId,
    pub slug: String,
    pub title: String,
    pub markdown_body: String,
    pub private: bool,
    pub aliases: Vec<NoteAlias>,
    /// Namespaced per-module metadata (e.g., ext["zcast-tags"], ext["zcast-pinned"]).
    pub ext: BTreeMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Passed on every read/query path so private notes can NEVER leak to public visitors.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadContext {
    pub include_private: bool,
}

impl ReadContext {
    pub const PUBLIC: Self = Self { include_private: false };
    pub const ADMIN: Self = Self { include_private: true };
}

/// Metadata every module crate exports so `zcast-kernel` can build the
/// dependency graph, validate SemVer contracts, and surface recommendations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleManifest {
    pub id: &'static str,                    // e.g. "zcast-cmd-k"
    pub version: &'static str,               // SemVer, e.g. "0.1.0"
    pub provides: &'static [InterfaceSpec],  // e.g. [("zcast:ui/command-bar", "1.0.0")]
    pub requires: &'static [InterfaceReq],   // e.g. [("zcast:search/provider", "^1.0")]
    pub recommends: &'static [&'static str], // e.g. ["zcast-search-onnx"]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceSpec {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceReq {
    pub name: &'static str,
    pub version_req: &'static str,
    pub optional: bool,
}
```

---

## 2. `Storage` Contract (`zcast:storage/repository@1.0`)

Designed so **both** `zcast-storage-sqlite` and `zcast-storage-mysql` implement the exact same trait—and so every other module (`zcast-auth-magic-link`, `zcast-links-wikilink`, `zcast-revisions`, etc.) persists structured data via `ModuleStore` without ever writing SQLite or MySQL SQL.

```rust
use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("note not found: {0}")]
    NotFound(String),
    #[error("slug already exists: {0}")]
    SlugConflict(String),
    #[error("storage backend error: {0}")]
    Backend(String),
}

#[derive(Debug, Clone)]
pub struct NoteLookup {
    pub note: Note,
    /// True if matched via an old `NoteAlias` rather than current slug
    /// (signals the HTTP layer to issue a 301 redirect to `note.slug`).
    pub matched_alias: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NoteListFilter {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub sort: NoteSort,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum NoteSort {
    #[default]
    UpdatedDesc,
    CreatedDesc,
    TitleAsc,
}

/// Required singleton contract (`zcast:storage/repository@1.0`).
#[async_trait]
pub trait Storage: Send + Sync {
    /// Run backend-specific migrations / schema initialization on startup.
    async fn initialize(&self) -> Result<(), StorageError>;

    /// Fetch a note by immutable ID (returns NotFound if private and ctx is PUBLIC).
    async fn get_note_by_id(
        &self,
        id: &NoteId,
        ctx: ReadContext,
    ) -> Result<Option<Note>, StorageError>;

    /// Fetch by current slug OR historical alias (enables automatic rename redirects).
    async fn get_note_by_slug_or_alias(
        &self,
        slug: &str,
        ctx: ReadContext,
    ) -> Result<Option<NoteLookup>, StorageError>;

    /// Fetch by exact title (case-insensitive) for [[Card Title]] resolution.
    async fn get_note_by_title(
        &self,
        title: &str,
        ctx: ReadContext,
    ) -> Result<Option<Note>, StorageError>;

    /// List notes (for index rebuilding, exports, admin lists).
    async fn list_notes(
        &self,
        filter: NoteListFilter,
        ctx: ReadContext,
    ) -> Result<Vec<Note>, StorageError>;

    /// Insert or update a note.
    async fn save_note(&self, note: &Note) -> Result<(), StorageError>;

    /// Atomically rename a note, record its previous slug/title as an alias,
    /// and persist any rewritten citing notes in a single transaction.
    async fn rename_note_atomic(
        &self,
        renamed_note: &Note,
        new_alias: &NoteAlias,
        rewritten_referrers: &[Note],
    ) -> Result<(), StorageError>;

    /// Permanently delete a note by ID.
    async fn delete_note(&self, id: &NoteId) -> Result<(), StorageError>;

    /// Access the DB-agnostic namespaced key-value/document store for modules.
    fn module_store(&self, namespace: &str) -> Box<dyn ModuleStore>;
}

/// DB-agnostic persistence exposed to every module (DEC-09).
/// - `zcast-auth-magic-link` stores hashed one-time tokens (with TTL) and sessions here.
/// - `zcast-links-wikilink` stores outgoing/incoming link edges here.
/// - `zcast-revisions` stores timestamped note snapshots here.
/// - Kernel stores active module slot bindings and pinned homepage ID here.
#[async_trait]
pub trait ModuleStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<serde_json::Value>, StorageError>;

    async fn put(
        &self,
        key: &str,
        value: &serde_json::Value,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), StorageError>;

    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// List entries whose keys start with `prefix` (e.g., `"backlinks:01J..."` or `"rev:01J..."`).
    async fn list_prefix(
        &self,
        prefix: &str,
        limit: Option<usize>,
    ) -> Result<Vec<(String, serde_json::Value)>, StorageError>;

    /// Purge expired keys (useful for session/magic-link cleanup).
    async fn purge_expired(&self) -> Result<u64, StorageError>;
}
```

### Design Highlights in `Storage`
- **`ReadContext` is mandatory on all reads:** Even a newly written module calling `storage.list_notes(...)` cannot accidentally forget to filter out `private = true` notes when serving a public request.
- **`rename_note_atomic`:** Guarantees that renaming a card, saving the old slug redirect alias, and updating `[[Old Title]]` → `[[New Title]]` across citing notes succeed or fail together in one transaction.
- **`ModuleStore` with `expires_at` + `list_prefix`:** Gives auth modules TTL-expiring tokens/sessions, gives `zcast-links-wikilink` fast prefix queries for `backlinks:<note_id>:...`, and works identically on SQLite and MySQL.

---

## 3. `SearchProvider` Contract (`zcast:search/provider@1.0`)

Designed so `zcast-cmd-k` (or any search UI) can run against **any** search provider (`zcast-search-fts`, `zcast-search-onnx`, or a third-party external API provider) while knowing what capabilities the bound provider supports.

```rust
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("search index error: {0}")]
    Index(String),
    #[error("search query failed: {0}")]
    Query(String),
}

/// Lets consumers like `zcast-cmd-k` adapt their UI or show admin hints
/// (e.g. "Install zcast-search-onnx for semantic topical matches").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchCapabilities {
    pub keyword: bool,          // Exact / prefix / fuzzy text matching
    pub topical_semantic: bool, // Embeddings or topic-vector similarity
    pub graph_boosted: bool,    // Uses link-graph neighborhood signals
    pub instant_as_you_type: bool,
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub text: String,
    pub limit: usize,
    /// Optional currently active note slug so providers can boost graph-adjacent cards.
    pub context_note_id: Option<NoteId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchKind {
    Keyword,
    Topical,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub note_id: NoteId,
    pub slug: String,
    pub title: String,
    /// Short highlighted snippet or topical excerpt for the Cmd+K dropdown.
    pub snippet: String,
    /// Normalized relevance score (0.0 ..= 1.0).
    pub score: f32,
    pub match_kind: MatchKind,
}

/// Optional singleton contract (`zcast:search/provider@1.0`).
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Advertise what kinds of matching this provider supports.
    fn capabilities(&self) -> SearchCapabilities;

    /// Index or update a single note (called via kernel `after_note_save` / `after_note_rename`).
    /// Providers MUST store `note.private` in their index so public queries filter it out.
    async fn index_note(&self, note: &Note) -> Result<(), SearchError>;

    /// Remove a deleted note from the index (called via `after_note_delete`).
    async fn remove_note(&self, id: &NoteId) -> Result<(), SearchError>;

    /// Full index rebuild (e.g., after swapping search providers via `zcast module bind`).
    async fn reindex_all(&self, notes: &[Note]) -> Result<(), SearchError>;

    /// Execute a search query, strictly respecting `ctx.include_private`.
    async fn search(
        &self,
        query: &SearchQuery,
        ctx: ReadContext,
    ) -> Result<Vec<SearchHit>, SearchError>;
}
```

### How Swapping Search Providers Works in Practice
1. You start with `zcast-search-fts` (fast pure-Rust Tantivy FTS + link-graph boosting). `capabilities().topical_semantic` is `false`.
2. In the Admin Module Graph, `zcast-cmd-k` displays: *"Bound to `zcast-search-fts` (`search@1.0`). Recommends `zcast-search-onnx` for semantic topical search."*
3. Later, you run `zcast module install zcast-search-onnx && zcast module bind search zcast-search-onnx`.
4. On startup, the kernel notices the bound `search` provider changed, calls `reindex_all` once from `Storage`, and `Cmd+K` immediately returns both `Keyword` and `Topical` (`Hybrid`) hits—without changing a single line in `zcast-cmd-k`!
