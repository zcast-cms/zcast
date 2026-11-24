//! Fearless `zcast update`: hot SQLite backups, SHA-256 verification, SemVer
//! gating for the release channel, and rollback-friendly update planning.
//!
//! The update flow is:
//! 1. `backup_sqlite_db` takes a consistent `VACUUM INTO` snapshot (WAL safe).
//! 2. The release archive + `SHA256SUMS` are downloaded and verified.
//! 3. The new binary is swapped in atomically (see `modules::atomic_swap_binary`).
//! 4. `/_zcast/health` is checked; on failure the previous binary is restored.
#![allow(dead_code)] // Wired into `main.rs` by the coordinator.

use chrono::Utc;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Default GitHub repository used for release downloads.
pub const DEFAULT_RELEASE_REPO: &str = "zcast-cms/zcast";

/// Release asset listing checksums for every platform archive.
pub const CHECKSUMS_ASSET: &str = "SHA256SUMS";

/// Compile-time target triple used to pick the release asset (DEC-13).
pub fn current_target_triple() -> &'static str {
    const TRIPLE: &str = if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "musl"
    )) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "unknown-unknown-unknown"
    };
    TRIPLE
}

fn sql_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

/// Poll `http://<bind_addr>/_zcast/health` until it answers or attempts run out.
///
/// Each probe is `curl -fsS --max-time 2`, so a hung server cannot stall the
/// updater for long. `delay_ms` is the wait between failed attempts.
pub fn check_health_endpoint(bind_addr: &str, attempts: usize, delay_ms: u64) -> bool {
    let url = format!("http://{}/_zcast/health", bind_addr.trim());

    for attempt in 0..attempts {
        if attempt > 0 && delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        let healthy = Command::new("curl")
            .args(["-fsS", "--max-time", "2", &url])
            .output()
            .is_ok_and(|out| out.status.success());
        if healthy {
            return true;
        }
    }

    false
}

/// True when `service` is active according to systemd (always false off Linux).
pub fn is_systemd_service_active(service: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        Command::new("systemctl")
            .args(["is-active", "--quiet", service])
            .output()
            .is_ok_and(|out| out.status.success())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = service;
        false
    }
}

/// Restart a systemd service, surfacing its stderr on failure.
pub fn restart_systemd_service(service: &str) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["restart", service])
        .output()
        .map_err(|e| format!("failed to run systemctl restart {service}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!("systemctl restart {service} failed"))
    } else {
        Err(format!("systemctl restart {service} failed: {stderr}"))
    }
}

/// Take a consistent hot snapshot of the SQLite database via `VACUUM INTO`.
///
/// SQLite guarantees the resulting file is a transactionally consistent copy,
/// including any data still sitting in the WAL, so no downtime is required.
pub async fn backup_sqlite_db(db_path: &Path, backup_dir: &Path) -> Result<PathBuf, String> {
    if !db_path.exists() {
        return Err(format!(
            "database not found at {} — nothing to back up",
            db_path.display()
        ));
    }

    std::fs::create_dir_all(backup_dir).map_err(|e| {
        format!(
            "failed to create backup directory {}: {e}",
            backup_dir.display()
        )
    })?;

    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let mut backup_path = backup_dir.join(format!("zcast-{stamp}.db"));
    let mut attempt = 1;
    while backup_path.exists() {
        backup_path = backup_dir.join(format!("zcast-{stamp}-{attempt}.db"));
        attempt += 1;
    }

    // A read-write connection is used so SQLite can recover/read the WAL.
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false);
    let pool = SqlitePool::connect_with(options)
        .await
        .map_err(|e| format!("failed to open SQLite database {}: {e}", db_path.display()))?;

    let vacuum = format!("VACUUM INTO '{}'", sql_quote(&backup_path));
    let result = sqlx::query(&vacuum).execute(&pool).await;
    pool.close().await;

    result.map_err(|e| {
        format!(
            "failed to snapshot {} into {}: {e}",
            db_path.display(),
            backup_path.display()
        )
    })?;

    if !backup_path.exists() {
        return Err(format!(
            "SQLite reported success but no backup exists at {}",
            backup_path.display()
        ));
    }

    Ok(backup_path)
}

/// Verify downloaded bytes against a lowercase/uppercase SHA-256 hex digest.
pub fn verify_sha256_bytes(bytes: &[u8], expected_hex: &str) -> bool {
    let expected = expected_hex.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    let actual = hex::encode(Sha256::digest(bytes));
    // Constant-ish comparison without early exit on content.
    let mut diff = 0u8;
    for (a, b) in actual.bytes().zip(expected.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn normalize_checksum_name(name: &str) -> &str {
    let name = name.trim();
    let name = name.strip_prefix('*').unwrap_or(name);
    let name = name.strip_prefix("./").unwrap_or(name);
    name.trim()
}

/// Parse a GNU/BSD `SHA256SUMS` listing for `filename`.
///
/// Accepts `<hash>  file`, `<hash> *file`, and `SHA256 (file) = <hash>`.
pub fn parse_sha256sums(sums: &str, filename: &str) -> Option<String> {
    let wanted = normalize_checksum_name(filename);
    if wanted.is_empty() {
        return None;
    }

    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // BSD style: `SHA256 (zcast-0.4.1.tar.gz) = <hash>`
        if let Some(rest) = line.strip_prefix("SHA256 (")
            && let Some((name, hash)) = rest.split_once(") =")
        {
            let hash = hash.trim().to_ascii_lowercase();
            if normalize_checksum_name(name) == wanted && is_sha256_hex(&hash) {
                return Some(hash);
            }
            continue;
        }

        // GNU style: `<hash>  file` or `<hash> *file`
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let hash = hash.to_ascii_lowercase();
        if !is_sha256_hex(&hash) {
            continue;
        }
        if normalize_checksum_name(name) == wanted {
            return Some(hash);
        }
    }

    None
}

/// Strip a leading `v`/`V` from a release tag (`v0.4.0` -> `0.4.0`).
pub fn normalize_version_tag(tag: &str) -> &str {
    let tag = tag.trim();
    tag.strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
}

fn parse_version(version: &str) -> (u64, u64, u64, Option<String>) {
    let version = normalize_version_tag(version);
    let version = version.split('+').next().unwrap_or(version);
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_owned())),
        Some((core, _)) => (core, None),
        None => (version, None),
    };

    let mut parts = core
        .split('.')
        .map(|part| part.trim().parse::<u64>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    (major, minor, patch, pre)
}

fn compare_prerelease(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        // A release outranks any prerelease of the same core version.
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => {
            let mut left_parts = left.split('.');
            let mut right_parts = right.split('.');
            loop {
                match (left_parts.next(), right_parts.next()) {
                    (None, None) => return Ordering::Equal,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(a), Some(b)) => {
                        let ordering = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(a), Ok(b)) => a.cmp(&b),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if ordering != Ordering::Equal {
                            return ordering;
                        }
                    }
                }
            }
        }
    }
}

fn compare_versions(current: &str, candidate: &str) -> Ordering {
    let (current_major, current_minor, current_patch, current_pre) = parse_version(current);
    let (candidate_major, candidate_minor, candidate_patch, candidate_pre) =
        parse_version(candidate);

    match (candidate_major, candidate_minor, candidate_patch).cmp(&(
        current_major,
        current_minor,
        current_patch,
    )) {
        Ordering::Equal => compare_prerelease(candidate_pre.as_deref(), current_pre.as_deref()),
        other => other,
    }
}

/// True when `candidate` is a strictly newer SemVer release than `current`.
///
/// Numeric comparison is per-component, so `0.10.0 > 0.4.0` and `0.4.1 > 0.4.0`.
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    compare_versions(current, candidate) == Ordering::Greater
}

/// Everything the updater needs to fetch and verify a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePlan {
    pub current_version: String,
    pub target_version: String,
    pub target_triple: String,
    pub download_url: String,
    pub checksums_url: String,
    /// Custom-module builds are rebuilt locally from the warm `target/` cache.
    pub is_custom_module_build: bool,
}

/// Build the download plan for a release (bundled or custom-module build).
pub fn build_update_plan(
    repo: &str,
    current_version: &str,
    target_version: &str,
    has_custom_modules: bool,
) -> UpdatePlan {
    let repo = repo.trim().trim_matches('/');
    let current = normalize_version_tag(current_version).to_owned();
    let target = normalize_version_tag(target_version).to_owned();
    let triple = current_target_triple().to_owned();
    let base = format!("https://github.com/{repo}/releases/download/v{target}");
    let asset = format!("zcast-{triple}.tar.gz");

    UpdatePlan {
        current_version: current,
        target_version: target,
        target_triple: triple,
        download_url: format!("{base}/{asset}"),
        checksums_url: format!("{base}/{CHECKSUMS_ASSET}"),
        is_custom_module_build: has_custom_modules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteJournalMode;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zcast-cli-update-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn backup_creates_consistent_snapshot_with_rows() {
        let dir = temp_dir("backup");
        let db_path = dir.join("zcast.db");

        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("open db");

        sqlx::query("CREATE TABLE notes (id TEXT PRIMARY KEY, title TEXT NOT NULL)")
            .execute(&pool)
            .await
            .expect("create table");
        sqlx::query("INSERT INTO notes (id, title) VALUES ('01', 'Index')")
            .execute(&pool)
            .await
            .expect("insert index");
        sqlx::query("INSERT INTO notes (id, title) VALUES ('02', 'Second')")
            .execute(&pool)
            .await
            .expect("insert second");
        pool.close().await;

        let backup_dir = dir.join("backups");
        let backup = backup_sqlite_db(&db_path, &backup_dir)
            .await
            .expect("backup succeeds");

        assert!(backup.exists(), "backup file exists");
        assert!(backup.starts_with(&backup_dir));
        let file_name = backup
            .file_name()
            .expect("filename")
            .to_string_lossy()
            .to_string();
        assert!(file_name.starts_with("zcast-"), "name: {file_name}");
        assert!(file_name.ends_with(".db"), "name: {file_name}");

        let backup_options = SqliteConnectOptions::new().filename(&backup);
        let backup_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(backup_options)
            .await
            .expect("open backup");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes")
            .fetch_one(&backup_pool)
            .await
            .expect("count rows");
        assert_eq!(count, 2);
        let title: String = sqlx::query_scalar("SELECT title FROM notes WHERE id = '01'")
            .fetch_one(&backup_pool)
            .await
            .expect("read title");
        assert_eq!(title, "Index");
        backup_pool.close().await;

        cleanup(&dir);
    }

    #[tokio::test]
    async fn backup_errors_for_missing_database() {
        let dir = temp_dir("backup-missing");
        let err = backup_sqlite_db(&dir.join("nope.db"), &dir.join("backups"))
            .await
            .expect_err("missing db must error");
        assert!(err.contains("database not found"));
        cleanup(&dir);
    }

    #[test]
    fn verifies_sha256_digests() {
        // SHA-256("abc")
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256_bytes(b"abc", expected));
        assert!(verify_sha256_bytes(b"abc", &expected.to_uppercase()));
        assert!(verify_sha256_bytes(b"abc", &format!("  {expected}  ")));
        assert!(!verify_sha256_bytes(b"abd", expected));
        assert!(!verify_sha256_bytes(b"abc", "deadbeef"));
        assert!(!verify_sha256_bytes(b"abc", "zz"));
    }

    #[test]
    fn parses_gnu_and_bsd_checksum_listings() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let archive = "zcast-0.4.1-x86_64-unknown-linux-musl.tar.gz";
        let sums = format!("{hash}  README.md\n{hash} *{archive}\n{hash}  other.tar.gz\n");

        assert_eq!(parse_sha256sums(&sums, archive).as_deref(), Some(hash));
        assert_eq!(
            parse_sha256sums(&sums, &format!("./{archive}")).as_deref(),
            Some(hash)
        );
        assert_eq!(
            parse_sha256sums(&sums, "other.tar.gz").as_deref(),
            Some(hash)
        );
        assert_eq!(parse_sha256sums(&sums, "missing.tar.gz"), None);
        assert_eq!(parse_sha256sums("", archive), None);
        assert_eq!(parse_sha256sums("# comment only\n", archive), None);
    }

    #[test]
    fn parses_bsd_checksum_lines() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let sums = format!("SHA256 (zcast-0.4.1.tar.gz) = {hash}\n");
        assert_eq!(
            parse_sha256sums(&sums, "zcast-0.4.1.tar.gz").as_deref(),
            Some(hash)
        );
    }

    #[test]
    fn normalizes_version_tags() {
        assert_eq!(normalize_version_tag("v0.4.0"), "0.4.0");
        assert_eq!(normalize_version_tag("V0.4.0"), "0.4.0");
        assert_eq!(normalize_version_tag(" 0.4.0 "), "0.4.0");
        assert_eq!(normalize_version_tag("0.4.0"), "0.4.0");
    }

    #[test]
    fn compares_semver_components_numerically() {
        assert!(is_newer_version("0.4.0", "0.4.1"));
        assert!(is_newer_version("0.4.0", "v0.4.1"));
        assert!(is_newer_version("0.4.0", "0.10.0"));
        assert!(is_newer_version("0.4.0", "1.0.0"));
        assert!(is_newer_version("0.4.9", "0.5.0"));

        assert!(!is_newer_version("0.4.1", "0.4.0"));
        assert!(!is_newer_version("0.4.0", "0.4.0"));
        assert!(!is_newer_version("0.10.0", "0.4.0"));

        assert!(is_newer_version("1.0.0-rc.1", "1.0.0"));
        assert!(!is_newer_version("1.0.0", "1.0.0-rc.1"));
        assert!(is_newer_version("1.0.0-rc.1", "1.0.0-rc.2"));
    }

    #[test]
    fn builds_update_plan_for_bundled_build() {
        let plan = build_update_plan("zcast-cms/zcast", "v0.4.0", "v0.4.1", false);
        assert_eq!(plan.current_version, "0.4.0");
        assert_eq!(plan.target_version, "0.4.1");
        assert_eq!(plan.target_triple, current_target_triple());
        assert_eq!(
            plan.download_url,
            format!(
                "https://github.com/zcast-cms/zcast/releases/download/v0.4.1/zcast-{}.tar.gz",
                current_target_triple()
            )
        );
        assert_eq!(
            plan.checksums_url,
            "https://github.com/zcast-cms/zcast/releases/download/v0.4.1/SHA256SUMS"
        );
        assert!(!plan.is_custom_module_build);
    }

    #[test]
    fn builds_update_plan_for_custom_module_build() {
        let plan = build_update_plan("/zcast-cms/zcast/", "0.4.0", "0.5.0", true);
        assert!(plan.is_custom_module_build);
        assert!(plan.download_url.contains("/v0.5.0/"));
        assert!(!plan.download_url.contains("//releases"));
    }

    #[test]
    fn health_check_with_no_attempts_is_false() {
        assert!(!check_health_endpoint("127.0.0.1:1", 0, 0));
    }

    #[test]
    fn health_check_against_closed_port_is_false() {
        // Nothing listens on port 1; curl fails fast with a connection error.
        assert!(!check_health_endpoint("127.0.0.1:1", 1, 0));
    }

    #[test]
    fn target_triple_is_known() {
        let triple = current_target_triple();
        assert!(!triple.is_empty());
        assert_ne!(triple, "unknown-unknown-unknown", "unsupported build host");
    }
}
