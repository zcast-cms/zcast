//! SQLite implementation of the `zcast:storage/repository@1.0` contract.
//!
//! Provides durable `Note` persistence (with historical slug aliases) and the
//! DB-agnostic [`ModuleStore`] used by every other zcast module. Reads always
//! honour [`ReadContext::include_private`] so private notes cannot leak to the
//! public rendering path.

use std::collections::HashMap;
use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow,
};
use sqlx::{Row, Sqlite, Transaction};
use zcast_interfaces::{
    IFACE_STORAGE, InterfaceSpec, ModuleManifest, ModuleStore, Note, NoteAlias, NoteId,
    NoteListFilter, NoteLookup, NoteSort, ReadContext, Storage, StorageError,
};

const MANIFEST: ModuleManifest = ModuleManifest {
    id: "zcast-storage-sqlite",
    name: "SQLite Storage",
    version: "0.1.0",
    description: "SQLite (WAL) persistence for notes, aliases, and module key-value data",
    provides: &[InterfaceSpec {
        name: IFACE_STORAGE,
        version: "1.0.0",
    }],
    requires: &[],
    recommends: &[],
};

const NOTE_COLUMNS: &str = "id, slug, title, markdown_body, private, ext, created_at, updated_at";

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS notes (
        id            TEXT PRIMARY KEY NOT NULL,
        slug          TEXT NOT NULL UNIQUE,
        title         TEXT NOT NULL,
        markdown_body TEXT NOT NULL,
        private       INTEGER NOT NULL DEFAULT 0,
        ext           TEXT NOT NULL DEFAULT '{}',
        created_at    TEXT NOT NULL,
        updated_at    TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_notes_slug ON notes (slug)",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_title_lower ON notes (LOWER(title))",
    "CREATE TABLE IF NOT EXISTS note_aliases (
        slug       TEXT PRIMARY KEY NOT NULL,
        note_id    TEXT NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
        title      TEXT NOT NULL,
        created_at TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_note_aliases_slug ON note_aliases (slug)",
    "CREATE INDEX IF NOT EXISTS idx_note_aliases_note_id ON note_aliases (note_id)",
    "CREATE TABLE IF NOT EXISTS module_kv (
        namespace  TEXT NOT NULL,
        key        TEXT NOT NULL,
        value      TEXT NOT NULL,
        expires_at TEXT,
        PRIMARY KEY (namespace, key)
    )",
    "CREATE INDEX IF NOT EXISTS idx_module_kv_ns_key ON module_kv (namespace, key)",
];

/// SQLite-backed [`Storage`] provider.
#[derive(Debug, Clone)]
pub struct SqliteStorage {
    pool: SqlitePool,
}

impl SqliteStorage {
    /// Connect to `database_url` (e.g. `sqlite://zcast.db` or `:memory:`),
    /// creating the file if needed and enabling WAL on disk-backed databases.
    pub async fn connect(database_url: &str) -> Result<Self, StorageError> {
        let in_memory = is_in_memory_url(database_url);

        let mut options = SqliteConnectOptions::from_str(database_url).map_err(|e| {
            StorageError::Backend(format!("invalid sqlite database url `{database_url}`: {e}"))
        })?;
        options = options.create_if_missing(true).foreign_keys(true);
        if !in_memory {
            options = options.journal_mode(SqliteJournalMode::Wal);
        }

        // A single connection keeps `:memory:` databases coherent.
        let max_connections = if in_memory { 1 } else { 5 };
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .map_err(backend)?;

        Ok(Self { pool })
    }

    /// The underlying connection pool (useful for migrations and diagnostics).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn load_aliases(
        &self,
        note_ids: &[String],
    ) -> Result<HashMap<String, Vec<NoteAlias>>, StorageError> {
        let mut map: HashMap<String, Vec<NoteAlias>> = HashMap::new();
        if note_ids.is_empty() {
            return Ok(map);
        }

        let placeholders = vec!["?"; note_ids.len()].join(", ");
        let sql = format!(
            "SELECT note_id, slug, title, created_at FROM note_aliases \
             WHERE note_id IN ({placeholders}) ORDER BY created_at ASC, slug ASC"
        );
        let mut query = sqlx::query(&sql);
        for id in note_ids {
            query = query.bind(id);
        }

        let rows = query.fetch_all(&self.pool).await.map_err(backend)?;
        for row in rows {
            let note_id: String = row.try_get("note_id").map_err(backend)?;
            map.entry(note_id).or_default().push(NoteAlias {
                slug: row.try_get("slug").map_err(backend)?,
                title: row.try_get("title").map_err(backend)?,
                created_at: row.try_get("created_at").map_err(backend)?,
            });
        }

        Ok(map)
    }

    async fn hydrate(&self, rows: Vec<SqliteRow>) -> Result<Vec<Note>, StorageError> {
        let mut ids = Vec::with_capacity(rows.len());
        for row in &rows {
            ids.push(row.try_get::<String, _>("id").map_err(backend)?);
        }
        let aliases = self.load_aliases(&ids).await?;

        let mut notes = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id").map_err(backend)?;
            let note_aliases = aliases.get(&id).cloned().unwrap_or_default();
            notes.push(row_to_note(&row, note_aliases)?);
        }
        Ok(notes)
    }
}

#[async_trait]
impl Storage for SqliteStorage {
    fn manifest(&self) -> ModuleManifest {
        MANIFEST
    }

    async fn initialize(&self) -> Result<(), StorageError> {
        for statement in SCHEMA {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .map_err(backend)?;
        }
        Ok(())
    }

    async fn get_note_by_id(
        &self,
        id: &NoteId,
        ctx: ReadContext,
    ) -> Result<Option<Note>, StorageError> {
        let sql = format!(
            "SELECT {NOTE_COLUMNS} FROM notes WHERE id = ? AND (? = 1 OR private = 0) LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(id.as_str())
            .bind(ctx.include_private)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;

        match row {
            Some(row) => Ok(self.hydrate(vec![row]).await?.pop()),
            None => Ok(None),
        }
    }

    async fn get_note_by_slug_or_alias(
        &self,
        slug: &str,
        ctx: ReadContext,
    ) -> Result<Option<NoteLookup>, StorageError> {
        let slug = slug.trim();

        let current_sql = format!(
            "SELECT {NOTE_COLUMNS} FROM notes \
             WHERE slug = ? AND (? = 1 OR private = 0) LIMIT 1"
        );
        if let Some(row) = sqlx::query(&current_sql)
            .bind(slug)
            .bind(ctx.include_private)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?
        {
            let note = self
                .hydrate(vec![row])
                .await?
                .pop()
                .ok_or_else(|| StorageError::NotFound(slug.to_owned()))?;
            return Ok(Some(NoteLookup {
                note,
                matched_alias: false,
            }));
        }

        let alias_sql = format!(
            "SELECT {} FROM notes n \
             JOIN note_aliases a ON a.note_id = n.id \
             WHERE a.slug = ? AND (? = 1 OR n.private = 0) LIMIT 1",
            NOTE_COLUMNS
                .split(", ")
                .map(|column| format!("n.{column}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some(row) = sqlx::query(&alias_sql)
            .bind(slug)
            .bind(ctx.include_private)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?
        {
            let note = self
                .hydrate(vec![row])
                .await?
                .pop()
                .ok_or_else(|| StorageError::NotFound(slug.to_owned()))?;
            return Ok(Some(NoteLookup {
                note,
                matched_alias: true,
            }));
        }

        Ok(None)
    }

    async fn get_note_by_title(
        &self,
        title: &str,
        ctx: ReadContext,
    ) -> Result<Option<Note>, StorageError> {
        let sql = format!(
            "SELECT {NOTE_COLUMNS} FROM notes \
             WHERE LOWER(title) = LOWER(?) AND (? = 1 OR private = 0) LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(title.trim())
            .bind(ctx.include_private)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;

        match row {
            Some(row) => Ok(self.hydrate(vec![row]).await?.pop()),
            None => Ok(None),
        }
    }

    async fn list_notes(
        &self,
        filter: NoteListFilter,
        ctx: ReadContext,
    ) -> Result<Vec<Note>, StorageError> {
        let order = match filter.sort {
            NoteSort::UpdatedDesc => "updated_at DESC, id ASC",
            NoteSort::CreatedDesc => "created_at DESC, id ASC",
            NoteSort::TitleAsc => "title COLLATE NOCASE ASC, id ASC",
        };

        let sql = format!(
            "SELECT {NOTE_COLUMNS} FROM notes WHERE (? = 1 OR private = 0) \
             ORDER BY {order} LIMIT ? OFFSET ?"
        );
        let limit = filter.limit.map(|v| v as i64).unwrap_or(-1);
        let offset = filter.offset.map(|v| v as i64).unwrap_or(0);

        let rows = sqlx::query(&sql)
            .bind(ctx.include_private)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;

        self.hydrate(rows).await
    }

    async fn save_note(&self, note: &Note) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        save_note_in(&mut tx, note).await?;
        tx.commit().await.map_err(backend)
    }

    async fn rename_note_atomic(
        &self,
        renamed_note: &Note,
        new_alias: &NoteAlias,
        rewritten_referrers: &[Note],
    ) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;

        save_note_in(&mut tx, renamed_note).await?;

        sqlx::query(
            "INSERT INTO note_aliases (slug, note_id, title, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(slug) DO UPDATE SET \
             note_id = excluded.note_id, title = excluded.title, created_at = excluded.created_at",
        )
        .bind(&new_alias.slug)
        .bind(renamed_note.id.as_str())
        .bind(&new_alias.title)
        .bind(new_alias.created_at)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;

        for referrer in rewritten_referrers {
            save_note_in(&mut tx, referrer).await?;
        }

        tx.commit().await.map_err(backend)
    }

    async fn delete_note(&self, id: &NoteId) -> Result<(), StorageError> {
        let result = sqlx::query("DELETE FROM notes WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(backend)?;

        if result.rows_affected() == 0 {
            return Err(StorageError::NotFound(id.to_string()));
        }
        Ok(())
    }

    fn module_store(&self, namespace: &str) -> Box<dyn ModuleStore> {
        Box::new(SqliteModuleStore {
            pool: self.pool.clone(),
            namespace: namespace.to_owned(),
        })
    }
}

async fn save_note_in(tx: &mut Transaction<'_, Sqlite>, note: &Note) -> Result<(), StorageError> {
    let existing_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM notes WHERE slug = ? LIMIT 1")
            .bind(&note.slug)
            .fetch_optional(&mut **tx)
            .await
            .map_err(backend)?;
    if let Some(existing_id) = existing_id
        && existing_id != note.id.as_str()
    {
        return Err(StorageError::SlugConflict(note.slug.clone()));
    }

    let existing_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM notes WHERE LOWER(title) = LOWER(?) LIMIT 1")
            .bind(&note.title)
            .fetch_optional(&mut **tx)
            .await
            .map_err(backend)?;
    if let Some(existing_id) = existing_id
        && existing_id != note.id.as_str()
    {
        return Err(StorageError::TitleConflict(note.title.clone()));
    }

    let ext = serde_json::to_string(&note.ext)
        .map_err(|e| StorageError::Backend(format!("invalid ext metadata: {e}")))?;

    sqlx::query(
        "INSERT INTO notes \
         (id, slug, title, markdown_body, private, ext, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
         slug = excluded.slug, title = excluded.title, \
         markdown_body = excluded.markdown_body, private = excluded.private, \
         ext = excluded.ext, updated_at = excluded.updated_at",
    )
    .bind(note.id.as_str())
    .bind(&note.slug)
    .bind(&note.title)
    .bind(&note.markdown_body)
    .bind(note.private)
    .bind(ext)
    .bind(note.created_at)
    .bind(note.updated_at)
    .execute(&mut **tx)
    .await
    .map_err(|e| map_write_error(e, note))?;

    sqlx::query("DELETE FROM note_aliases WHERE note_id = ?")
        .bind(note.id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(backend)?;

    for alias in &note.aliases {
        sqlx::query(
            "INSERT INTO note_aliases (slug, note_id, title, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(slug) DO UPDATE SET \
             note_id = excluded.note_id, title = excluded.title, created_at = excluded.created_at",
        )
        .bind(&alias.slug)
        .bind(note.id.as_str())
        .bind(&alias.title)
        .bind(alias.created_at)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    }

    Ok(())
}

/// SQLite `ModuleStore` namespace, backed by the shared `module_kv` table.
#[derive(Debug, Clone)]
struct SqliteModuleStore {
    pool: SqlitePool,
    namespace: String,
}

#[async_trait]
impl ModuleStore for SqliteModuleStore {
    async fn get(&self, key: &str) -> Result<Option<serde_json::Value>, StorageError> {
        let row = sqlx::query(
            "SELECT value FROM module_kv \
             WHERE namespace = ? AND key = ? \
             AND (expires_at IS NULL OR expires_at > ?) LIMIT 1",
        )
        .bind(&self.namespace)
        .bind(key)
        .bind(Utc::now())
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;

        match row {
            None => Ok(None),
            Some(row) => {
                let raw: String = row.try_get("value").map_err(backend)?;
                serde_json::from_str(&raw).map(Some).map_err(|e| {
                    StorageError::Backend(format!("invalid JSON for module key `{}`: {e}", key))
                })
            }
        }
    }

    async fn put(
        &self,
        key: &str,
        value: &serde_json::Value,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), StorageError> {
        let value = serde_json::to_string(value)
            .map_err(|e| StorageError::Backend(format!("invalid module value: {e}")))?;

        sqlx::query(
            "INSERT INTO module_kv (namespace, key, value, expires_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(namespace, key) DO UPDATE SET \
             value = excluded.value, expires_at = excluded.expires_at",
        )
        .bind(&self.namespace)
        .bind(key)
        .bind(value)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM module_kv WHERE namespace = ? AND key = ?")
            .bind(&self.namespace)
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn list_prefix(
        &self,
        prefix: &str,
        limit: Option<usize>,
    ) -> Result<Vec<(String, serde_json::Value)>, StorageError> {
        let pattern = format!("{}%", escape_like(prefix));
        let limit = limit.map(|v| v as i64).unwrap_or(-1);

        let rows = sqlx::query(
            "SELECT key, value FROM module_kv \
             WHERE namespace = ? AND key LIKE ? ESCAPE '\\' \
             AND (expires_at IS NULL OR expires_at > ?) \
             ORDER BY key ASC LIMIT ?",
        )
        .bind(&self.namespace)
        .bind(pattern)
        .bind(Utc::now())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let key: String = row.try_get("key").map_err(backend)?;
            let raw: String = row.try_get("value").map_err(backend)?;
            let value = serde_json::from_str(&raw).map_err(|e| {
                StorageError::Backend(format!("invalid JSON for module key `{key}`: {e}"))
            })?;
            entries.push((key, value));
        }

        Ok(entries)
    }

    async fn purge_expired(&self) -> Result<u64, StorageError> {
        let result = sqlx::query(
            "DELETE FROM module_kv \
             WHERE namespace = ? AND expires_at IS NOT NULL AND expires_at <= ?",
        )
        .bind(&self.namespace)
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map_err(backend)?;

        Ok(result.rows_affected())
    }
}

fn row_to_note(row: &SqliteRow, aliases: Vec<NoteAlias>) -> Result<Note, StorageError> {
    let ext_raw: String = row.try_get("ext").map_err(backend)?;
    let ext = serde_json::from_str(&ext_raw)
        .map_err(|e| StorageError::Backend(format!("invalid ext JSON on note row: {e}")))?;

    Ok(Note {
        id: NoteId(row.try_get::<String, _>("id").map_err(backend)?),
        slug: row.try_get("slug").map_err(backend)?,
        title: row.try_get("title").map_err(backend)?,
        markdown_body: row.try_get("markdown_body").map_err(backend)?,
        private: row.try_get("private").map_err(backend)?,
        aliases,
        ext,
        created_at: row.try_get("created_at").map_err(backend)?,
        updated_at: row.try_get("updated_at").map_err(backend)?,
    })
}

fn map_write_error(error: sqlx::Error, note: &Note) -> StorageError {
    if let sqlx::Error::Database(db_error) = &error {
        let message = db_error.message();
        if message.contains("notes.slug") {
            return StorageError::SlugConflict(note.slug.clone());
        }
        if message.contains("notes.title") || message.contains("idx_notes_title_lower") {
            return StorageError::TitleConflict(note.title.clone());
        }
    }
    backend(error)
}

fn is_in_memory_url(url: &str) -> bool {
    url.contains(":memory:") || url.contains("mode=memory")
}

fn escape_like(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn backend(error: impl std::fmt::Display) -> StorageError {
    StorageError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use serde_json::json;
    use zcast_interfaces::slugify;

    async fn memory_storage() -> SqliteStorage {
        let storage = SqliteStorage::connect(":memory:").await.expect("connect");
        storage.initialize().await.expect("initialize");
        storage
    }

    fn note(title: &str, body: &str, private: bool) -> Note {
        Note::new(title, body, private)
    }

    fn at(offset_minutes: i64) -> DateTime<Utc> {
        Utc::now() + TimeDelta::minutes(offset_minutes)
    }

    #[tokio::test]
    async fn manifest_declares_storage_contract() {
        let storage = memory_storage().await;
        let manifest = storage.manifest();
        assert_eq!(manifest.id, "zcast-storage-sqlite");
        assert_eq!(manifest.provides.len(), 1);
        assert_eq!(manifest.provides[0].name, IFACE_STORAGE);
        assert_eq!(manifest.provides[0].version, "1.0.0");
    }

    #[tokio::test]
    async fn initialize_is_idempotent() {
        let storage = memory_storage().await;
        storage.initialize().await.expect("second initialize");
    }

    #[tokio::test]
    async fn save_and_get_roundtrip_preserves_fields() {
        let storage = memory_storage().await;
        let mut note = note("Hello World", "# Body\n\nText", false);
        note.ext.insert("zcast-tags".into(), json!(["rust", "cms"]));
        note.aliases.push(NoteAlias {
            slug: "old-hello".into(),
            title: "Old Hello".into(),
            created_at: at(-10),
        });

        storage.save_note(&note).await.expect("save");

        let loaded = storage
            .get_note_by_id(&note.id, ReadContext::PUBLIC)
            .await
            .expect("get")
            .expect("present");

        assert_eq!(loaded.id, note.id);
        assert_eq!(loaded.slug, "hello-world");
        assert_eq!(loaded.title, "Hello World");
        assert_eq!(loaded.markdown_body, "# Body\n\nText");
        assert!(!loaded.private);
        assert_eq!(loaded.ext, note.ext);
        assert_eq!(loaded.aliases.len(), 1);
        assert_eq!(loaded.aliases[0].slug, "old-hello");
    }

    #[tokio::test]
    async fn save_updates_existing_note_by_id() {
        let storage = memory_storage().await;
        let mut note = note("Draft", "one", false);
        storage.save_note(&note).await.expect("save");

        note.title = "Final".into();
        note.slug = slugify("Final");
        note.markdown_body = "two".into();
        note.updated_at = at(5);
        storage.save_note(&note).await.expect("update");

        let listed = storage
            .list_notes(NoteListFilter::default(), ReadContext::ADMIN)
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "Final");
        assert_eq!(listed[0].markdown_body, "two");
    }

    #[tokio::test]
    async fn duplicate_slug_is_rejected() {
        let storage = memory_storage().await;
        let first = note("Same Slug", "", false);
        storage.save_note(&first).await.expect("first");

        let mut second = note("Different Title", "", false);
        second.slug = first.slug.clone();

        match storage.save_note(&second).await {
            Err(StorageError::SlugConflict(slug)) => assert_eq!(slug, first.slug),
            other => panic!("expected slug conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn duplicate_title_is_case_insensitively_rejected() {
        let storage = memory_storage().await;
        storage
            .save_note(&note("Shared Title", "", false))
            .await
            .expect("first");

        let mut second = note("SHARED TITLE", "", false);
        second.slug = "shared-title-2".into();

        match storage.save_note(&second).await {
            Err(StorageError::TitleConflict(title)) => assert_eq!(title, "SHARED TITLE"),
            other => panic!("expected title conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn slug_and_alias_lookup_reports_match_kind() {
        let storage = memory_storage().await;
        let mut note = note("Current Title", "", false);
        note.aliases.push(NoteAlias {
            slug: "previous-title".into(),
            title: "Previous Title".into(),
            created_at: at(-30),
        });
        storage.save_note(&note).await.expect("save");

        let by_slug = storage
            .get_note_by_slug_or_alias("current-title", ReadContext::PUBLIC)
            .await
            .expect("lookup")
            .expect("present");
        assert!(!by_slug.matched_alias);
        assert_eq!(by_slug.note.id, note.id);

        let by_alias = storage
            .get_note_by_slug_or_alias("previous-title", ReadContext::PUBLIC)
            .await
            .expect("lookup")
            .expect("present");
        assert!(by_alias.matched_alias);
        assert_eq!(by_alias.note.id, note.id);

        assert!(
            storage
                .get_note_by_slug_or_alias("missing", ReadContext::PUBLIC)
                .await
                .expect("lookup")
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_by_title_is_case_insensitive() {
        let storage = memory_storage().await;
        let note = note("Case Sensitive Title", "", false);
        storage.save_note(&note).await.expect("save");

        let found = storage
            .get_note_by_title("case sensitive title", ReadContext::PUBLIC)
            .await
            .expect("lookup")
            .expect("present");
        assert_eq!(found.id, note.id);
    }

    #[tokio::test]
    async fn private_notes_never_leak_to_public_context() {
        let storage = memory_storage().await;
        let mut private = note("Secret Plans", "", true);
        private.aliases.push(NoteAlias {
            slug: "secret-old".into(),
            title: "Secret Old".into(),
            created_at: at(-5),
        });
        storage.save_note(&private).await.expect("save");
        let public = note("Public Note", "", false);
        storage.save_note(&public).await.expect("save");

        assert!(
            storage
                .get_note_by_id(&private.id, ReadContext::PUBLIC)
                .await
                .expect("get")
                .is_none()
        );
        assert!(
            storage
                .get_note_by_slug_or_alias(&private.slug, ReadContext::PUBLIC)
                .await
                .expect("slug lookup")
                .is_none()
        );
        assert!(
            storage
                .get_note_by_slug_or_alias("secret-old", ReadContext::PUBLIC)
                .await
                .expect("alias lookup")
                .is_none()
        );
        assert!(
            storage
                .get_note_by_title("Secret Plans", ReadContext::PUBLIC)
                .await
                .expect("title lookup")
                .is_none()
        );

        let visible = storage
            .list_notes(NoteListFilter::default(), ReadContext::PUBLIC)
            .await
            .expect("list");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, public.id);

        // Admin context still sees everything.
        assert!(
            storage
                .get_note_by_id(&private.id, ReadContext::ADMIN)
                .await
                .expect("get")
                .is_some()
        );
        let all = storage
            .list_notes(NoteListFilter::default(), ReadContext::ADMIN)
            .await
            .expect("list");
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn list_notes_supports_sort_limit_and_offset() {
        let storage = memory_storage().await;

        let mut apple = note("apple", "", false);
        apple.created_at = at(-30);
        apple.updated_at = at(-3);
        let mut banana = note("Banana", "", false);
        banana.created_at = at(-20);
        banana.updated_at = at(-2);
        let mut cherry = note("Cherry", "", false);
        cherry.created_at = at(-10);
        cherry.updated_at = at(-1);

        for note in [&apple, &banana, &cherry] {
            storage.save_note(note).await.expect("save");
        }

        let by_title = storage
            .list_notes(
                NoteListFilter {
                    sort: NoteSort::TitleAsc,
                    ..Default::default()
                },
                ReadContext::PUBLIC,
            )
            .await
            .expect("list");
        let titles: Vec<_> = by_title.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["apple", "Banana", "Cherry"]);

        let by_created = storage
            .list_notes(
                NoteListFilter {
                    sort: NoteSort::CreatedDesc,
                    ..Default::default()
                },
                ReadContext::PUBLIC,
            )
            .await
            .expect("list");
        let titles: Vec<_> = by_created.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["Cherry", "Banana", "apple"]);

        let by_updated = storage
            .list_notes(
                NoteListFilter {
                    sort: NoteSort::UpdatedDesc,
                    limit: Some(2),
                    offset: Some(1),
                },
                ReadContext::PUBLIC,
            )
            .await
            .expect("list");
        let titles: Vec<_> = by_updated.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["Banana", "apple"]);
    }

    #[tokio::test]
    async fn rename_note_atomic_updates_alias_and_referrers() {
        let storage = memory_storage().await;

        let alpha = note("Alpha", "alpha body", false);
        storage.save_note(&alpha).await.expect("save alpha");
        let beta = note("Beta", "see [[Alpha]]", false);
        storage.save_note(&beta).await.expect("save beta");

        let alias = NoteAlias {
            slug: alpha.slug.clone(),
            title: alpha.title.clone(),
            created_at: Utc::now(),
        };

        let mut renamed = alpha.clone();
        renamed.title = "Alpha Renamed".into();
        renamed.slug = slugify("Alpha Renamed");
        renamed.updated_at = at(1);
        renamed.aliases.push(alias.clone());

        let mut referrer = beta.clone();
        referrer.markdown_body = "see [[Alpha Renamed]]".into();
        referrer.updated_at = at(1);

        storage
            .rename_note_atomic(&renamed, &alias, std::slice::from_ref(&referrer))
            .await
            .expect("rename");

        let loaded = storage
            .get_note_by_id(&alpha.id, ReadContext::ADMIN)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(loaded.title, "Alpha Renamed");
        assert_eq!(loaded.slug, "alpha-renamed");

        let old_slug = storage
            .get_note_by_slug_or_alias("alpha", ReadContext::PUBLIC)
            .await
            .expect("lookup")
            .expect("alias present");
        assert!(old_slug.matched_alias);
        assert_eq!(old_slug.note.id, alpha.id);

        let updated_referrer = storage
            .get_note_by_id(&beta.id, ReadContext::ADMIN)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(updated_referrer.markdown_body, "see [[Alpha Renamed]]");
    }

    #[tokio::test]
    async fn rename_note_atomic_rolls_back_on_conflict() {
        let storage = memory_storage().await;

        let alpha = note("Alpha", "", false);
        storage.save_note(&alpha).await.expect("save alpha");
        let taken = note("Taken", "", false);
        storage.save_note(&taken).await.expect("save taken");

        let alias = NoteAlias {
            slug: alpha.slug.clone(),
            title: alpha.title.clone(),
            created_at: Utc::now(),
        };
        let mut renamed = alpha.clone();
        renamed.title = "Renamed".into();
        renamed.slug = slugify("Renamed");

        let mut referrer = Note::new("Other", "", false);
        referrer.slug = taken.slug.clone();

        let result = storage
            .rename_note_atomic(&renamed, &alias, std::slice::from_ref(&referrer))
            .await;
        assert!(result.is_err(), "conflicting referrer should fail");

        // The rename itself must have been rolled back.
        let loaded = storage
            .get_note_by_id(&alpha.id, ReadContext::ADMIN)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(loaded.title, "Alpha");
        assert!(
            storage
                .get_note_by_slug_or_alias("alpha", ReadContext::PUBLIC)
                .await
                .expect("lookup")
                .expect("current slug")
                .note
                .title
                .eq("Alpha")
        );
    }

    #[tokio::test]
    async fn delete_note_removes_aliases() {
        let storage = memory_storage().await;
        let mut note = note("Deletable", "", false);
        note.aliases.push(NoteAlias {
            slug: "deletable-old".into(),
            title: "Deletable Old".into(),
            created_at: at(-1),
        });
        storage.save_note(&note).await.expect("save");

        storage.delete_note(&note.id).await.expect("delete");

        assert!(
            storage
                .get_note_by_id(&note.id, ReadContext::ADMIN)
                .await
                .expect("get")
                .is_none()
        );
        assert!(
            storage
                .get_note_by_slug_or_alias("deletable-old", ReadContext::ADMIN)
                .await
                .expect("lookup")
                .is_none()
        );
        assert!(matches!(
            storage.delete_note(&note.id).await,
            Err(StorageError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn module_store_roundtrip_and_delete() {
        let storage = memory_storage().await;
        let store = storage.module_store("test-module");

        assert!(store.get("missing").await.expect("get").is_none());

        store
            .put("doc", &json!({"a": 1, "b": [true, null]}), None)
            .await
            .expect("put");
        assert_eq!(
            store.get("doc").await.expect("get"),
            Some(json!({"a": 1, "b": [true, null]}))
        );

        store
            .put("doc", &json!("replaced"), None)
            .await
            .expect("overwrite");
        assert_eq!(
            store.get("doc").await.expect("get"),
            Some(json!("replaced"))
        );

        store.delete("doc").await.expect("delete");
        assert!(store.get("doc").await.expect("get").is_none());
    }

    #[tokio::test]
    async fn module_store_ignores_expired_keys() {
        let storage = memory_storage().await;
        let store = storage.module_store("ttl");

        store
            .put("live", &json!(1), Some(Utc::now() + TimeDelta::hours(1)))
            .await
            .expect("put live");
        store
            .put("dead", &json!(2), Some(Utc::now() - TimeDelta::minutes(1)))
            .await
            .expect("put dead");
        store
            .put("forever", &json!(3), None)
            .await
            .expect("put forever");

        assert!(store.get("dead").await.expect("get").is_none());
        assert_eq!(store.get("live").await.expect("get"), Some(json!(1)));

        let entries = store.list_prefix("", None).await.expect("list");
        let keys: Vec<_> = entries.into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["forever", "live"]);

        let purged = store.purge_expired().await.expect("purge");
        assert_eq!(purged, 1);
        assert_eq!(store.list_prefix("", None).await.expect("list").len(), 2);
    }

    #[tokio::test]
    async fn module_store_list_prefix_is_ordered_and_limited() {
        let storage = memory_storage().await;
        let store = storage.module_store("prefix");

        for (key, value) in [
            ("out:03", 3),
            ("out:01", 1),
            ("out:02", 2),
            ("in:01", 10),
            ("out_extra", 4),
        ] {
            store.put(key, &json!(value), None).await.expect("put");
        }

        let entries = store.list_prefix("out:", None).await.expect("list");
        let keys: Vec<_> = entries.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["out:01", "out:02", "out:03"]);

        let limited = store.list_prefix("out:", Some(2)).await.expect("list");
        let keys: Vec<_> = limited.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["out:01", "out:02"]);

        // Wildcards in the prefix are treated literally.
        let escaped = store.list_prefix("out_", None).await.expect("list");
        assert_eq!(escaped.len(), 1);
        assert_eq!(escaped[0].0, "out_extra");

        let none = store.list_prefix("nope", None).await.expect("list");
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn module_store_namespaces_are_isolated() {
        let storage = memory_storage().await;
        let first = storage.module_store("first");
        let second = storage.module_store("second");

        first.put("shared", &json!("one"), None).await.expect("put");
        second
            .put("shared", &json!("two"), None)
            .await
            .expect("put");

        assert_eq!(first.get("shared").await.expect("get"), Some(json!("one")));
        assert_eq!(second.get("shared").await.expect("get"), Some(json!("two")));
    }
}
