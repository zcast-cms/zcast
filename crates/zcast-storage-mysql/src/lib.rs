//! MySQL implementation of the `zcast:storage/repository@1.0` contract.
//!
//! Behaviourally identical to `zcast-storage-sqlite`: durable [`Note`]
//! persistence (with historical slug aliases) plus the DB-agnostic
//! [`ModuleStore`] used by every other zcast module. Reads always honour
//! [`ReadContext::include_private`] so private notes can never leak into the
//! public rendering path.
//!
//! All timestamps are stored as fixed-width RFC3339 UTC strings
//! (`YYYY-MM-DDTHH:MM:SS.ffffffZ`). This avoids MySQL's `DATETIME`
//! timezone/precision pitfalls while keeping lexicographic ordering identical
//! to chronological ordering, which is what the expiry comparisons rely on.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{MySql, Row, Transaction};
use zcast_interfaces::{
    IFACE_STORAGE, InterfaceSpec, ModuleManifest, ModuleStore, Note, NoteAlias, NoteId,
    NoteListFilter, NoteLookup, NoteSort, ReadContext, Storage, StorageError,
};

const MANIFEST: ModuleManifest = ModuleManifest {
    id: "zcast-storage-mysql",
    name: "MySQL Storage",
    version: env!("CARGO_PKG_VERSION"),
    description: "MySQL persistence for notes, aliases, and DB-agnostic module key-value data",
    provides: &[InterfaceSpec {
        name: IFACE_STORAGE,
        version: "1.0.0",
    }],
    requires: &[],
    recommends: &[],
};

const MAX_CONNECTIONS: u32 = 10;

const NOTE_COLUMNS: &str = "id, slug, title, markdown_body, private, ext, created_at, updated_at";

/// MySQL-flavoured DDL. `key`/`value` are reserved words in MySQL, hence
/// `kv_key`/`kv_value` in `module_kv`. `title_lower` is a real column so the
/// case-insensitive uniqueness rule can be enforced by a unique index instead
/// of a functional index.
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS notes (
        id            VARCHAR(64)  NOT NULL,
        slug          VARCHAR(255) NOT NULL UNIQUE,
        title         VARCHAR(255) NOT NULL,
        title_lower   VARCHAR(255) NOT NULL UNIQUE,
        markdown_body LONGTEXT     NOT NULL,
        private       TINYINT(1)   NOT NULL DEFAULT 0,
        ext           LONGTEXT     NOT NULL,
        created_at    VARCHAR(64)  NOT NULL,
        updated_at    VARCHAR(64)  NOT NULL,
        PRIMARY KEY (id),
        INDEX idx_notes_created_at (created_at),
        INDEX idx_notes_updated_at (updated_at),
        INDEX idx_notes_private (private)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
    "CREATE TABLE IF NOT EXISTS note_aliases (
        slug       VARCHAR(255) NOT NULL,
        note_id    VARCHAR(64)  NOT NULL,
        title      VARCHAR(255) NOT NULL,
        created_at VARCHAR(64)  NOT NULL,
        PRIMARY KEY (slug),
        INDEX idx_note_aliases_note_id (note_id),
        CONSTRAINT fk_note_aliases_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
    "CREATE TABLE IF NOT EXISTS module_kv (
        namespace  VARCHAR(128) NOT NULL,
        kv_key     VARCHAR(255) NOT NULL,
        kv_value   LONGTEXT     NOT NULL,
        expires_at VARCHAR(64)  NULL,
        PRIMARY KEY (namespace, kv_key),
        INDEX idx_module_kv_expires_at (expires_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
];

/// MySQL-backed [`Storage`] provider.
#[derive(Debug, Clone)]
pub struct MysqlStorage {
    pool: MySqlPool,
}

impl MysqlStorage {
    /// Connect to `database_url` (e.g. `mysql://user:pass@host:3306/zcast`).
    pub async fn connect(database_url: &str) -> Result<Self, StorageError> {
        let pool = MySqlPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect(database_url)
            .await
            .map_err(backend)?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool (useful for shared pools and diagnostics).
    pub fn from_pool(pool: MySqlPool) -> Self {
        Self { pool }
    }

    /// The underlying connection pool (useful for migrations and diagnostics).
    pub fn pool(&self) -> &MySqlPool {
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
                created_at: parse_ts(
                    row.try_get::<String, _>("created_at")
                        .map_err(backend)?
                        .as_str(),
                )?,
            });
        }

        Ok(map)
    }

    async fn hydrate(&self, rows: Vec<MySqlRow>) -> Result<Vec<Note>, StorageError> {
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
impl Storage for MysqlStorage {
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
            .bind(include_private_flag(ctx))
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
            .bind(include_private_flag(ctx))
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
            qualified_note_columns("n")
        );
        if let Some(row) = sqlx::query(&alias_sql)
            .bind(slug)
            .bind(include_private_flag(ctx))
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
             WHERE title_lower = LOWER(?) AND (? = 1 OR private = 0) LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(title.trim())
            .bind(include_private_flag(ctx))
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
        // `utf8mb4_unicode_ci` already sorts case-insensitively, matching the
        // `COLLATE NOCASE` behaviour of the SQLite provider.
        let order = match filter.sort {
            NoteSort::UpdatedDesc => "updated_at DESC, id ASC",
            NoteSort::CreatedDesc => "created_at DESC, id ASC",
            NoteSort::TitleAsc => "title ASC, id ASC",
        };

        // MySQL does not accept negative `LIMIT`/`OFFSET`, and the values are
        // plain integers derived from `usize`, so inlining them is safe.
        let limit = filter.limit.map(|v| v as u64).unwrap_or(u64::MAX);
        let offset = filter.offset.unwrap_or(0) as u64;

        let sql = format!(
            "SELECT {NOTE_COLUMNS} FROM notes WHERE (? = 1 OR private = 0) \
             ORDER BY {order} LIMIT {limit} OFFSET {offset}"
        );

        let rows = sqlx::query(&sql)
            .bind(include_private_flag(ctx))
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
             ON DUPLICATE KEY UPDATE \
             note_id = VALUES(note_id), title = VALUES(title), created_at = VALUES(created_at)",
        )
        .bind(&new_alias.slug)
        .bind(renamed_note.id.as_str())
        .bind(&new_alias.title)
        .bind(format_ts(new_alias.created_at))
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
        Box::new(MysqlModuleStore {
            pool: self.pool.clone(),
            namespace: namespace.to_owned(),
        })
    }
}

async fn save_note_in(tx: &mut Transaction<'_, MySql>, note: &Note) -> Result<(), StorageError> {
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
        sqlx::query_scalar("SELECT id FROM notes WHERE title_lower = LOWER(?) LIMIT 1")
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
         (id, slug, title, title_lower, markdown_body, private, ext, created_at, updated_at) \
         VALUES (?, ?, ?, LOWER(?), ?, ?, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE \
         slug = VALUES(slug), title = VALUES(title), \
         title_lower = LOWER(VALUES(title)), \
         markdown_body = VALUES(markdown_body), private = VALUES(private), \
         ext = VALUES(ext), updated_at = VALUES(updated_at)",
    )
    .bind(note.id.as_str())
    .bind(&note.slug)
    .bind(&note.title)
    .bind(&note.title)
    .bind(&note.markdown_body)
    .bind(private_flag(note.private))
    .bind(ext)
    .bind(format_ts(note.created_at))
    .bind(format_ts(note.updated_at))
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
             ON DUPLICATE KEY UPDATE \
             note_id = VALUES(note_id), title = VALUES(title), created_at = VALUES(created_at)",
        )
        .bind(&alias.slug)
        .bind(note.id.as_str())
        .bind(&alias.title)
        .bind(format_ts(alias.created_at))
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    }

    Ok(())
}

/// MySQL `ModuleStore` namespace, backed by the shared `module_kv` table.
#[derive(Debug, Clone)]
struct MysqlModuleStore {
    pool: MySqlPool,
    namespace: String,
}

#[async_trait]
impl ModuleStore for MysqlModuleStore {
    async fn get(&self, key: &str) -> Result<Option<serde_json::Value>, StorageError> {
        let row = sqlx::query(
            "SELECT kv_value FROM module_kv \
             WHERE namespace = ? AND kv_key = ? \
             AND (expires_at IS NULL OR expires_at > ?) LIMIT 1",
        )
        .bind(&self.namespace)
        .bind(key)
        .bind(now_ts())
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;

        match row {
            None => Ok(None),
            Some(row) => {
                let raw: String = row.try_get("kv_value").map_err(backend)?;
                serde_json::from_str(&raw).map(Some).map_err(|e| {
                    StorageError::Backend(format!("invalid JSON for module key `{key}`: {e}"))
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
            "INSERT INTO module_kv (namespace, kv_key, kv_value, expires_at) VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE \
             kv_value = VALUES(kv_value), expires_at = VALUES(expires_at)",
        )
        .bind(&self.namespace)
        .bind(key)
        .bind(value)
        .bind(expires_at.map(format_ts))
        .execute(&self.pool)
        .await
        .map_err(backend)?;

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM module_kv WHERE namespace = ? AND kv_key = ?")
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
        let limit = limit.map(|v| v as u64).unwrap_or(u64::MAX);

        // `ESCAPE '\\'` (a single backslash) keeps `%`/`_` in the prefix literal.
        let rows = sqlx::query(&format!(
            "SELECT kv_key, kv_value FROM module_kv \
             WHERE namespace = ? AND kv_key LIKE ? ESCAPE '\\\\' \
             AND (expires_at IS NULL OR expires_at > ?) \
             ORDER BY kv_key ASC LIMIT {limit}"
        ))
        .bind(&self.namespace)
        .bind(pattern)
        .bind(now_ts())
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let key: String = row.try_get("kv_key").map_err(backend)?;
            let raw: String = row.try_get("kv_value").map_err(backend)?;
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
        .bind(now_ts())
        .execute(&self.pool)
        .await
        .map_err(backend)?;

        Ok(result.rows_affected())
    }
}

fn row_to_note(row: &MySqlRow, aliases: Vec<NoteAlias>) -> Result<Note, StorageError> {
    let ext_raw: String = row.try_get("ext").map_err(backend)?;
    let ext = serde_json::from_str(&ext_raw)
        .map_err(|e| StorageError::Backend(format!("invalid ext JSON on note row: {e}")))?;

    let private: i8 = row.try_get("private").map_err(backend)?;

    Ok(Note {
        id: NoteId(row.try_get::<String, _>("id").map_err(backend)?),
        slug: row.try_get("slug").map_err(backend)?,
        title: row.try_get("title").map_err(backend)?,
        markdown_body: row.try_get("markdown_body").map_err(backend)?,
        private: private != 0,
        aliases,
        ext,
        created_at: parse_ts(
            row.try_get::<String, _>("created_at")
                .map_err(backend)?
                .as_str(),
        )?,
        updated_at: parse_ts(
            row.try_get::<String, _>("updated_at")
                .map_err(backend)?
                .as_str(),
        )?,
    })
}

/// Translate a write failure into the contract's conflict variants.
fn map_write_error(error: sqlx::Error, note: &Note) -> StorageError {
    if let sqlx::Error::Database(db_error) = &error
        && let Some(conflict) =
            classify_duplicate_key(db_error.code().as_deref(), db_error.message(), note)
    {
        return conflict;
    }
    backend(error)
}

/// Classify a driver error as a slug/title conflict.
///
/// MySQL reports all unique-key violations with SQLSTATE `23000`; the offending
/// index is only visible in the human-readable message, so that is what we key
/// off. Anything else stays a [`StorageError::Backend`] error.
fn classify_duplicate_key(code: Option<&str>, message: &str, note: &Note) -> Option<StorageError> {
    let lower = message.to_ascii_lowercase();
    let is_duplicate = code == Some("23000") || lower.contains("duplicate entry");
    if !is_duplicate {
        return None;
    }

    if lower.contains("slug") {
        Some(StorageError::SlugConflict(note.slug.clone()))
    } else if lower.contains("title_lower") || lower.contains("title") {
        Some(StorageError::TitleConflict(note.title.clone()))
    } else {
        // e.g. a foreign-key failure: a genuine backend problem.
        None
    }
}

fn qualified_note_columns(prefix: &str) -> String {
    NOTE_COLUMNS
        .split(", ")
        .map(|column| format!("{prefix}.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render a UTC timestamp as fixed-width RFC3339 so string ordering matches
/// chronological ordering (required for `expires_at` range comparisons).
fn format_ts(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn now_ts() -> String {
    format_ts(Utc::now())
}

fn parse_ts(raw: &str) -> Result<DateTime<Utc>, StorageError> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(raw) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    // Tolerate values written by hand or by a plain `DATETIME` column.
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, format) {
            return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
        }
    }
    Err(StorageError::Backend(format!(
        "invalid timestamp in storage: `{raw}`"
    )))
}

fn include_private_flag(ctx: ReadContext) -> i64 {
    if ctx.include_private { 1 } else { 0 }
}

fn private_flag(private: bool) -> i8 {
    if private { 1 } else { 0 }
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

    /// A pool that is never connected to: enough for manifest/schema checks.
    fn offline_storage() -> MysqlStorage {
        let pool = MySqlPoolOptions::new()
            .connect_lazy("mysql://zcast:zcast@127.0.0.1:3306/zcast")
            .expect("lazy pool");
        MysqlStorage::from_pool(pool)
    }

    fn note(title: &str) -> Note {
        Note::new(title, "body", false)
    }

    fn ts(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    #[tokio::test]
    async fn manifest_declares_storage_contract() {
        let storage = offline_storage();
        let manifest = storage.manifest();
        assert_eq!(manifest.id, "zcast-storage-mysql");
        assert_eq!(manifest.name, "MySQL Storage");
        assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            manifest.description,
            "MySQL persistence for notes, aliases, and DB-agnostic module key-value data"
        );
        assert_eq!(manifest.provides.len(), 1);
        assert_eq!(manifest.provides[0].name, IFACE_STORAGE);
        assert_eq!(manifest.provides[0].version, "1.0.0");
        assert!(manifest.requires.is_empty());
        assert!(manifest.recommends.is_empty());
    }

    #[tokio::test]
    async fn from_pool_exposes_pool() {
        let pool = MySqlPoolOptions::new()
            .connect_lazy("mysql://zcast:zcast@127.0.0.1:3306/zcast")
            .expect("lazy pool");
        let storage = MysqlStorage::from_pool(pool.clone());
        assert_eq!(storage.pool().size(), pool.size());
        assert_eq!(
            storage.pool().options().get_max_connections(),
            MAX_CONNECTIONS
        );
        assert_eq!(storage.pool().options().get_max_connections(), 10);
    }

    #[test]
    fn schema_uses_mysql_ddl() {
        let notes = SCHEMA[0];
        let aliases = SCHEMA[1];
        let kv = SCHEMA[2];

        for statement in SCHEMA {
            assert!(statement.contains("ENGINE=InnoDB"), "{statement}");
            assert!(statement.contains("utf8mb4"), "{statement}");
            assert!(
                statement.contains("COLLATE=utf8mb4_unicode_ci"),
                "{statement}"
            );
        }

        assert!(notes.contains("CREATE TABLE IF NOT EXISTS notes"));
        assert!(notes.contains("id            VARCHAR(64)  NOT NULL"));
        assert!(notes.contains("slug          VARCHAR(255) NOT NULL UNIQUE"));
        assert!(notes.contains("title_lower   VARCHAR(255) NOT NULL UNIQUE"));
        assert!(notes.contains("markdown_body LONGTEXT     NOT NULL"));
        assert!(notes.contains("private       TINYINT(1)"));
        assert!(notes.contains("created_at    VARCHAR(64)  NOT NULL"));
        assert!(notes.contains("updated_at    VARCHAR(64)  NOT NULL"));
        assert!(notes.contains("PRIMARY KEY (id)"));

        assert!(aliases.contains("CREATE TABLE IF NOT EXISTS note_aliases"));
        assert!(aliases.contains("PRIMARY KEY (slug)"));
        assert!(aliases.contains("INDEX idx_note_aliases_note_id (note_id)"));
        assert!(aliases.contains(
            "CONSTRAINT fk_note_aliases_note FOREIGN KEY (note_id) REFERENCES notes(id) \
             ON DELETE CASCADE"
        ));

        assert!(kv.contains("CREATE TABLE IF NOT EXISTS module_kv"));
        assert!(kv.contains("kv_key     VARCHAR(255) NOT NULL"));
        assert!(kv.contains("kv_value   LONGTEXT     NOT NULL"));
        assert!(kv.contains("expires_at VARCHAR(64)  NULL"));
        assert!(kv.contains("PRIMARY KEY (namespace, kv_key)"));

        // MySQL reserves `key`/`value`, so they must not appear as bare columns.
        assert!(!kv.contains(" key "), "{kv}");
        assert!(!kv.contains(" value "), "{kv}");
    }

    #[test]
    fn escape_like_escapes_wildcards_and_backslash() {
        assert_eq!(escape_like("plain/key"), "plain/key");
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
        assert_eq!(escape_like("%_\\"), "\\%\\_\\\\");
        assert_eq!(escape_like(""), "");
    }

    #[test]
    fn rfc3339_timestamps_roundtrip() {
        for (raw, canonical) in [
            ("2024-01-01T00:00:00Z", "2024-01-01T00:00:00.000000Z"),
            ("2024-05-01T12:34:56.123456Z", "2024-05-01T12:34:56.123456Z"),
            ("1999-12-31T23:59:59+00:00", "1999-12-31T23:59:59.000000Z"),
        ] {
            let original = ts(raw);
            let formatted = format_ts(original);
            assert_eq!(
                formatted, canonical,
                "format must be fixed-width RFC3339 UTC"
            );
            assert_eq!(formatted.len(), 27);
            assert_eq!(parse_ts(&formatted).expect("parse"), original);
        }

        // Sub-microsecond precision is truncated, not lost to timezone drift.
        let precise = ts("2024-05-01T12:34:56.123456789Z");
        assert_eq!(
            parse_ts(&format_ts(precise)).expect("parse"),
            ts("2024-05-01T12:34:56.123456Z")
        );

        // Legacy values are still readable.
        assert_eq!(
            parse_ts("2024-05-01 12:34:56").expect("parse"),
            ts("2024-05-01T12:34:56Z")
        );
        assert!(parse_ts("not a timestamp").is_err());
    }

    #[test]
    fn rfc3339_lexicographic_order_matches_chronology() {
        let mut chronological = [
            ts("2024-01-01T00:00:00Z"),
            ts("2024-01-01T00:00:00.000001Z"),
            ts("2024-01-01T00:00:01Z"),
            ts("2024-01-01T00:01:00Z"),
            ts("2024-01-02T00:00:00Z"),
            ts("2024-02-01T00:00:00Z"),
            ts("2025-01-01T00:00:00Z"),
        ];
        chronological.sort();

        let formatted: Vec<String> = chronological.iter().copied().map(format_ts).collect();
        let mut lexicographic = formatted.clone();
        lexicographic.sort();
        assert_eq!(formatted, lexicographic);

        // The expiry predicate relies on `expires_at > now` being a plain string
        // comparison, so a past value must sort before `now` and vice versa.
        let now = format_ts(ts("2024-06-01T00:00:00Z"));
        assert!(format_ts(ts("2024-05-31T23:59:59.999999Z")) < now);
        assert!(format_ts(ts("2024-06-01T00:00:00.000001Z")) > now);
        assert!(format_ts(ts("2023-06-01T00:00:00Z")) <= now);
    }

    #[test]
    fn expiry_comparison_uses_formatted_strings() {
        let now = ts("2024-06-01T00:00:00Z");
        let live = now + TimeDelta::hours(1);
        let dead = now - TimeDelta::minutes(1);

        assert!(format_ts(live) > format_ts(now));
        assert!(format_ts(dead) <= format_ts(now));
    }

    #[test]
    fn duplicate_key_errors_map_to_conflicts() {
        let note = note("Shared Title");

        let slug = classify_duplicate_key(
            Some("23000"),
            "Duplicate entry 'shared-title' for key 'notes.slug'",
            &note,
        );
        assert!(matches!(slug, Some(StorageError::SlugConflict(slug)) if slug == "shared-title"));

        let title = classify_duplicate_key(
            Some("23000"),
            "Duplicate entry 'Shared Title' for key 'notes.idx_notes_title_lower'",
            &note,
        );
        assert!(matches!(title, Some(StorageError::TitleConflict(t)) if t == "Shared Title"));

        // Message-only detection also works (drivers that omit SQLSTATE).
        let message_only = classify_duplicate_key(
            None,
            "Duplicate entry 'x' for key 'notes.title_lower'",
            &note,
        );
        assert!(matches!(message_only, Some(StorageError::TitleConflict(_))));

        // Foreign-key failures and unrelated errors are backend problems.
        assert!(
            classify_duplicate_key(
                Some("23000"),
                "Cannot add or update a child row: a foreign key constraint fails",
                &note,
            )
            .is_none()
        );
        assert!(
            classify_duplicate_key(Some("42S02"), "Table 'zcast.notes' doesn't exist", &note)
                .is_none()
        );

        let backend_error =
            map_write_error(sqlx::Error::Protocol("lost connection".to_owned()), &note);
        assert!(
            matches!(backend_error, StorageError::Backend(message) if message.contains("lost connection"))
        );
    }

    #[test]
    fn flags_follow_read_context() {
        assert_eq!(include_private_flag(ReadContext::PUBLIC), 0);
        assert_eq!(include_private_flag(ReadContext::ADMIN), 1);
        assert_eq!(private_flag(true), 1);
        assert_eq!(private_flag(false), 0);
    }

    #[test]
    fn qualified_columns_prefix_every_field() {
        assert_eq!(
            qualified_note_columns("n"),
            "n.id, n.slug, n.title, n.markdown_body, n.private, n.ext, n.created_at, n.updated_at"
        );
    }

    /// Live roundtrip. Runs only when `ZCAST_TEST_MYSQL_URL` is set, so the
    /// default `cargo test` never needs a MySQL server.
    #[tokio::test]
    async fn live_mysql_roundtrip() {
        let Ok(url) = std::env::var("ZCAST_TEST_MYSQL_URL") else {
            eprintln!("ZCAST_TEST_MYSQL_URL not set; skipping live MySQL roundtrip");
            return;
        };

        let storage = MysqlStorage::connect(&url).await.expect("connect");
        storage.initialize().await.expect("initialize");
        // DDL must be idempotent.
        storage.initialize().await.expect("re-initialize");

        let run = NoteId::new().to_string();
        let mut note = Note::new(
            format!("Live Mysql Roundtrip {run}"),
            "# Body\n\nText",
            false,
        );
        note.ext.insert("zcast-tags".into(), json!(["mysql"]));
        note.aliases.push(NoteAlias {
            slug: format!("live-old-{run}"),
            title: "Live Old".into(),
            created_at: Utc::now() - TimeDelta::minutes(5),
        });

        storage.save_note(&note).await.expect("save");

        let loaded = storage
            .get_note_by_id(&note.id, ReadContext::PUBLIC)
            .await
            .expect("get id")
            .expect("present");
        assert_eq!(loaded.id, note.id);
        assert_eq!(loaded.slug, note.slug);
        assert_eq!(loaded.title, note.title);
        assert_eq!(loaded.markdown_body, note.markdown_body);
        assert_eq!(loaded.ext, note.ext);
        assert_eq!(loaded.aliases.len(), 1);
        assert_eq!(loaded.aliases[0].slug, note.aliases[0].slug);
        assert_eq!(
            loaded
                .created_at
                .to_rfc3339_opts(SecondsFormat::Micros, true),
            format_ts(note.created_at)
        );

        let by_slug = storage
            .get_note_by_slug_or_alias(&note.slug, ReadContext::PUBLIC)
            .await
            .expect("slug lookup")
            .expect("present");
        assert!(!by_slug.matched_alias);
        assert_eq!(by_slug.note.id, note.id);

        let by_alias = storage
            .get_note_by_slug_or_alias(&format!("live-old-{run}"), ReadContext::PUBLIC)
            .await
            .expect("alias lookup")
            .expect("present");
        assert!(by_alias.matched_alias);
        assert_eq!(by_alias.note.id, note.id);

        let by_title = storage
            .get_note_by_title(&note.title.to_uppercase(), ReadContext::PUBLIC)
            .await
            .expect("title lookup")
            .expect("present");
        assert_eq!(by_title.id, note.id);

        // Update by id.
        let mut updated = note.clone();
        updated.markdown_body = "# Body\n\nUpdated".into();
        updated.updated_at = Utc::now();
        storage.save_note(&updated).await.expect("update");
        let reloaded = storage
            .get_note_by_id(&note.id, ReadContext::ADMIN)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(reloaded.markdown_body, "# Body\n\nUpdated");

        // Conflicts.
        let mut slug_clash = Note::new(format!("Different Title {run}"), "", false);
        slug_clash.slug = note.slug.clone();
        assert!(matches!(
            storage.save_note(&slug_clash).await,
            Err(StorageError::SlugConflict(_))
        ));

        let mut title_clash = Note::new(note.title.to_uppercase(), "", false);
        title_clash.slug = format!("live-title-clash-{run}");
        assert!(matches!(
            storage.save_note(&title_clash).await,
            Err(StorageError::TitleConflict(_))
        ));

        // Private notes stay hidden from public reads.
        let private = Note::new(format!("Live Private {run}"), "secret", true);
        storage.save_note(&private).await.expect("save private");
        assert!(
            storage
                .get_note_by_id(&private.id, ReadContext::PUBLIC)
                .await
                .expect("get")
                .is_none()
        );
        assert!(
            storage
                .get_note_by_id(&private.id, ReadContext::ADMIN)
                .await
                .expect("get")
                .is_some()
        );
        let public_ids: Vec<String> = storage
            .list_notes(NoteListFilter::default(), ReadContext::PUBLIC)
            .await
            .expect("list")
            .into_iter()
            .map(|n| n.id.to_string())
            .collect();
        assert!(!public_ids.contains(&private.id.to_string()));

        // Sorting: `UpdatedDesc` must order our notes by `updated_at`, newest first.
        let listed = storage
            .list_notes(NoteListFilter::default(), ReadContext::ADMIN)
            .await
            .expect("list");
        let our_ids: Vec<String> = listed
            .iter()
            .filter(|n| n.id == note.id || n.id == private.id)
            .map(|n| n.id.to_string())
            .collect();
        assert_eq!(our_ids.len(), 2);
        let mut expected_order = vec![
            (note.id.to_string(), reloaded.updated_at),
            (private.id.to_string(), private.updated_at),
        ];
        expected_order.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        assert_eq!(
            our_ids,
            expected_order
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        );

        // `TitleAsc` sorts case-insensitively and `limit`/`offset` page the result.
        let first_page = storage
            .list_notes(
                NoteListFilter {
                    sort: NoteSort::TitleAsc,
                    limit: Some(1),
                    offset: Some(0),
                },
                ReadContext::ADMIN,
            )
            .await
            .expect("list");
        let second_page = storage
            .list_notes(
                NoteListFilter {
                    sort: NoteSort::TitleAsc,
                    limit: Some(1),
                    offset: Some(1),
                },
                ReadContext::ADMIN,
            )
            .await
            .expect("list");
        assert_eq!(first_page.len(), 1);
        assert_eq!(second_page.len(), 1);
        assert_ne!(first_page[0].id, second_page[0].id);
        assert!(first_page[0].title.to_lowercase() <= second_page[0].title.to_lowercase());

        // Atomic rename + alias redirect + referrer rewrite.
        let referrer = Note::new(
            format!("Live Referrer {run}"),
            format!("see [[{}]]", note.title),
            false,
        );
        storage.save_note(&referrer).await.expect("save referrer");

        let alias = NoteAlias {
            slug: note.slug.clone(),
            title: note.title.clone(),
            created_at: Utc::now(),
        };
        let mut renamed = note.clone();
        renamed.title = format!("Live Renamed {run}");
        renamed.slug = slugify(&renamed.title);
        renamed.updated_at = Utc::now();
        renamed.aliases.push(alias.clone());

        let mut rewritten = referrer.clone();
        rewritten.markdown_body = format!("see [[{}]]", renamed.title);
        rewritten.updated_at = Utc::now();

        storage
            .rename_note_atomic(&renamed, &alias, std::slice::from_ref(&rewritten))
            .await
            .expect("rename");

        let old_slug = storage
            .get_note_by_slug_or_alias(&alias.slug, ReadContext::PUBLIC)
            .await
            .expect("alias lookup")
            .expect("alias present");
        assert!(old_slug.matched_alias);
        assert_eq!(old_slug.note.id, note.id);
        assert_eq!(old_slug.note.title, renamed.title);

        let updated_referrer = storage
            .get_note_by_id(&referrer.id, ReadContext::ADMIN)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(updated_referrer.markdown_body, rewritten.markdown_body);

        // Rollback on conflict.
        let taken = Note::new(format!("Live Taken {run}"), "", false);
        storage.save_note(&taken).await.expect("save taken");
        let mut conflicting = renamed.clone();
        conflicting.title = format!("Live Rollback {run}");
        conflicting.slug = slugify(&conflicting.title);
        let mut bad_referrer = Note::new(format!("Live Bad Referrer {run}"), "", false);
        bad_referrer.slug = taken.slug.clone();
        assert!(
            storage
                .rename_note_atomic(
                    &conflicting,
                    &NoteAlias {
                        slug: renamed.slug.clone(),
                        title: renamed.title.clone(),
                        created_at: Utc::now(),
                    },
                    std::slice::from_ref(&bad_referrer),
                )
                .await
                .is_err()
        );
        let rolled_back = storage
            .get_note_by_id(&note.id, ReadContext::ADMIN)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(rolled_back.title, renamed.title);

        // Module store.
        let store = storage.module_store(&format!("live-test-{run}"));
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

        for (key, value) in [
            ("out:03", 3),
            ("out:01", 1),
            ("out:02", 2),
            ("in:01", 10),
            ("out_extra", 4),
        ] {
            store.put(key, &json!(value), None).await.expect("put");
        }
        let listed_keys: Vec<String> = store
            .list_prefix("out:", None)
            .await
            .expect("list")
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(listed_keys, ["out:01", "out:02", "out:03"]);

        let limited_keys: Vec<String> = store
            .list_prefix("out:", Some(2))
            .await
            .expect("list")
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(limited_keys, ["out:01", "out:02"]);

        // Escaped wildcards: `out_` must not match `out:`.
        let escaped_keys: Vec<String> = store
            .list_prefix("out_", None)
            .await
            .expect("list")
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(escaped_keys, ["out_extra"]);

        store
            .put("live", &json!(1), Some(Utc::now() + TimeDelta::hours(1)))
            .await
            .expect("put live");
        store
            .put("dead", &json!(2), Some(Utc::now() - TimeDelta::minutes(1)))
            .await
            .expect("put dead");
        assert!(store.get("dead").await.expect("get").is_none());
        assert_eq!(store.get("live").await.expect("get"), Some(json!(1)));
        assert_eq!(store.purge_expired().await.expect("purge"), 1);

        store.delete("doc").await.expect("delete");
        assert!(store.get("doc").await.expect("get").is_none());

        // Namespace isolation.
        let other = storage.module_store(&format!("live-other-{run}"));
        other.put("shared", &json!("two"), None).await.expect("put");
        store.put("shared", &json!("one"), None).await.expect("put");
        assert_eq!(store.get("shared").await.expect("get"), Some(json!("one")));
        assert_eq!(other.get("shared").await.expect("get"), Some(json!("two")));

        // Cleanup.
        for id in [&note.id, &private.id, &referrer.id, &taken.id] {
            storage.delete_note(id).await.expect("delete note");
        }
        assert!(matches!(
            storage.delete_note(&note.id).await,
            Err(StorageError::NotFound(_))
        ));
        assert!(
            storage
                .get_note_by_slug_or_alias(&alias.slug, ReadContext::ADMIN)
                .await
                .expect("alias lookup")
                .is_none(),
            "aliases must cascade on delete"
        );
        let _ = store.purge_expired().await;
        for key in [
            "live",
            "shared",
            "out:01",
            "out:02",
            "out:03",
            "out_extra",
            "in:01",
        ] {
            store.delete(key).await.expect("cleanup key");
        }
        other.delete("shared").await.expect("cleanup key");
    }
}
