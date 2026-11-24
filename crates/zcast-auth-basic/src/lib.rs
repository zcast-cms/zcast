//! Basic username/password admin authentication for zcast.
//!
//! Implements the optional `zcast:auth/provider@1.0` contract with a single
//! admin account protected by an Argon2id password hash. Everything is
//! persisted through the DB-agnostic [`ModuleStore`] (DEC-09) — this crate
//! contains **zero SQL** and therefore works unchanged on SQLite, MySQL or any
//! future `Storage` backend.
//!
//! Security properties:
//! - Passwords are only ever stored as Argon2id PHC strings (`$argon2id$...`).
//! - Session tokens are 32 random bytes (hex-encoded) returned to the browser
//!   once; the store only keeps `session:<sha256_hex(token)>`, so a database
//!   leak cannot be replayed as a live cookie.
//! - Sessions expire after [`DEFAULT_SESSION_TTL_SECONDS`] (7 days) through the
//!   store's `expires_at` support.
//!
//! On first boot the CLI can call [`BasicAuthProvider::ensure_admin_credentials`]
//! with `None` to generate a strong, human-readable password and print it once.

use std::sync::Arc;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use rand::Rng;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zcast_interfaces::{
    AuthError, AuthProvider, AuthSession, Html, IFACE_AUTH, IFACE_STORAGE, InterfaceReq,
    InterfaceSpec, ModuleManifest, ModuleStore, Storage, ThemeColorMode,
};

/// Module-store namespace owned by this provider.
const NAMESPACE: &str = "zcast-auth-basic";

/// Key holding the single admin credential document.
const ADMIN_USER_KEY: &str = "user:admin";

/// Prefix for hashed session records (`session:<sha256_hex(token)>`).
const SESSION_KEY_PREFIX: &str = "session:";

/// Session cookie name shared with the kernel's HTTP layer.
const SESSION_COOKIE_NAME: &str = "zcast_session";

/// Default session lifetime: seven days.
pub const DEFAULT_SESSION_TTL_SECONDS: u64 = 60 * 60 * 24 * 7;

/// Length (in bytes) of raw session tokens before hex encoding.
const SESSION_TOKEN_BYTES: usize = 32;

/// Alphabet for generated passwords: alphanumeric, minus easily confused
/// glyphs (`0`, `O`, `1`, `I`, `l`).
const PASSWORD_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

/// Number of dash-separated groups in a generated password.
const PASSWORD_GROUPS: usize = 4;
/// Characters per group in a generated password.
const PASSWORD_GROUP_LEN: usize = 4;

const THEME_CSS_HREF: &str = "/_zcast/theme/theme.css";
const LOGIN_PATH: &str = "/_zcast/login";

/// Same pre-paint bootstrap the first-party theme uses for `auto` mode.
const AUTO_THEME_SCRIPT: &str = r#"(function(){var r=document.documentElement,s=null;try{s=localStorage.getItem('zcast-color-mode');}catch(e){}if(s!=='dark'&&s!=='light'){s=(window.matchMedia&&window.matchMedia('(prefers-color-scheme: light)').matches)?'light':'dark';}r.setAttribute('data-theme',s);})();"#;

/// Inline CSS for the login card. Deliberately consumes the theme's
/// `--zcast-*` design tokens (with sane fallbacks) so the page inherits the
/// active palette, typography and radius without shipping a full stylesheet.
const LOGIN_CSS: &str = r#"
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body.zcast-auth{
  margin:0;
  min-height:100vh;
  display:grid;
  place-items:center;
  padding:clamp(1.5rem,5vw,4rem);
  background:var(--zcast-bg,#fffdfa);
  color:var(--zcast-text,#1d1d1d);
  font-family:var(--zcast-font-body,"Libre Franklin",system-ui,-apple-system,sans-serif);
  line-height:var(--zcast-leading-body,1.6);
  -webkit-font-smoothing:antialiased;
  text-rendering:optimizeLegibility;
}
.zcast-auth__card{
  width:min(100%,28rem);
  padding:clamp(1.75rem,4vw,2.75rem);
  background:var(--zcast-surface,#fff);
  border:1px solid var(--zcast-border,rgba(0,0,0,.12));
  border-radius:var(--zcast-radius,.5rem);
  box-shadow:var(--zcast-shadow-popover,0 8px 28px rgba(0,0,0,.12));
}
.zcast-auth__eyebrow{
  margin:0 0 .5rem;
  font-size:.72rem;
  letter-spacing:.16em;
  text-transform:uppercase;
  color:var(--zcast-text-muted,#6b6b6b);
}
.zcast-auth__title{
  margin:0;
  font-family:var(--zcast-font-heading,"Libre Franklin",Georgia,serif);
  font-size:clamp(1.5rem,4vw,2rem);
  line-height:1.15;
  letter-spacing:-.01em;
  color:var(--zcast-text-strong,#111);
}
.zcast-auth__lede{
  margin:.5rem 0 1.75rem;
  color:var(--zcast-text-muted,#6b6b6b);
  font-size:.95rem;
}
.zcast-auth__error{
  margin:0 0 1.25rem;
  padding:.75rem 1rem;
  border:1px solid var(--zcast-accent,rgba(0,0,0,.4));
  border-radius:calc(var(--zcast-radius,.5rem)/1.6);
  background:var(--zcast-accent-subtle,rgba(0,0,0,.05));
  color:var(--zcast-text-strong,#111);
  font-size:.9rem;
}
.zcast-auth__field{display:grid;gap:.4rem;margin-bottom:1.1rem}
.zcast-auth__label{
  font-size:.78rem;
  letter-spacing:.05em;
  text-transform:uppercase;
  color:var(--zcast-text-muted,#6b6b6b);
}
.zcast-auth__input{
  width:100%;
  padding:.7rem .85rem;
  font:inherit;
  color:inherit;
  background:var(--zcast-bg,#fffdfa);
  border:1px solid var(--zcast-border,rgba(0,0,0,.18));
  border-radius:calc(var(--zcast-radius,.5rem)/1.6);
}
.zcast-auth__input:focus-visible{
  outline:2px solid var(--zcast-accent,#1d1d1d);
  outline-offset:2px;
  border-color:var(--zcast-accent,#1d1d1d);
}
.zcast-auth__submit{
  width:100%;
  margin-top:.4rem;
  padding:.78rem 1rem;
  font:inherit;
  font-weight:600;
  color:var(--zcast-bg,#fff);
  background:var(--zcast-accent,#1d1d1d);
  border:0;
  border-radius:calc(var(--zcast-radius,.5rem)/1.6);
  cursor:pointer;
}
.zcast-auth__submit:hover{filter:brightness(1.1)}
.zcast-auth__back{
  display:inline-block;
  margin-top:1.5rem;
  color:var(--zcast-text-muted,#6b6b6b);
  font-size:.85rem;
  text-decoration:none;
}
.zcast-auth__back:hover{color:var(--zcast-accent,#111);text-decoration:underline}
@media (prefers-reduced-motion:no-preference){
  .zcast-auth__submit{transition:filter .15s ease}
}
"#;

static PROVIDES: &[InterfaceSpec] = &[InterfaceSpec {
    name: IFACE_AUTH,
    version: "1.0.0",
}];
static REQUIRES: &[InterfaceReq] = &[InterfaceReq {
    name: IFACE_STORAGE,
    version_req: "^1.0",
    optional: false,
}];
static RECOMMENDS: &[&str] = &[];

/// Stored admin credential document (`user:admin`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdminCredentials {
    username: String,
    /// Argon2id PHC string (`$argon2id$v=19$...`).
    password_hash: String,
}

/// Username/password authentication provider backed by [`ModuleStore`].
pub struct BasicAuthProvider {
    storage: Arc<dyn Storage>,
    session_ttl: Duration,
}

impl BasicAuthProvider {
    /// Create a provider using the default seven-day session lifetime.
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage,
            session_ttl: Duration::seconds(DEFAULT_SESSION_TTL_SECONDS as i64),
        }
    }

    /// Override the session lifetime (seconds). Mostly useful for tests and
    /// short-lived deployments.
    pub fn with_session_ttl_seconds(mut self, secs: u64) -> Self {
        self.session_ttl = Duration::seconds(secs.min(i64::MAX as u64) as i64);
        self
    }

    /// Ensure an admin credential exists, generating one when omitted.
    ///
    /// - `Some(password)` (non-empty) hashes and stores it, returning `Ok(None)`.
    /// - `None` (or an empty password) keeps any existing credential untouched.
    ///   When no credential exists yet a strong human-readable password is
    ///   generated, stored (hashed) and returned once so the CLI can print it.
    pub async fn ensure_admin_credentials(
        &self,
        username: &str,
        password: Option<&str>,
    ) -> Result<Option<String>, AuthError> {
        let store = self.store();

        let explicit = password.filter(|pw| !pw.is_empty());
        let mut generated = None;

        let plaintext = match explicit {
            Some(password) => password.to_owned(),
            None => {
                if store.get(ADMIN_USER_KEY).await.map_err(backend)?.is_some() {
                    // Keep the existing credentials; never silently rotate them.
                    return Ok(None);
                }

                let password = Self::generate_password();
                generated = Some(password.clone());
                password
            }
        };

        let credentials = AdminCredentials {
            username: username.trim().to_owned(),
            password_hash: hash_password(&plaintext)?,
        };
        let value = serde_json::to_value(&credentials).map_err(backend)?;
        store
            .put(ADMIN_USER_KEY, &value, None)
            .await
            .map_err(backend)?;

        Ok(generated)
    }

    /// Revoke every stored session, returning how many were deleted.
    ///
    /// The admin credential (`user:admin`) is never touched, so the account can
    /// be signed into again immediately. Useful after rotating a password via
    /// `zcast auth set-password`.
    pub async fn revoke_all_sessions(&self) -> Result<usize, AuthError> {
        let store = self.store();
        let sessions = store
            .list_prefix(SESSION_KEY_PREFIX, None)
            .await
            .map_err(backend)?;

        let mut revoked = 0usize;
        for (key, _value) in sessions {
            store.delete(&key).await.map_err(backend)?;
            revoked += 1;
        }

        Ok(revoked)
    }

    /// Generate a human-readable password: four dash-separated groups of four
    /// unambiguous alphanumeric characters (e.g. `k9M4-vP2x-Q7rL-8nTz`).
    pub fn generate_password() -> String {
        let mut rng = OsRng;
        let mut password = String::with_capacity(PASSWORD_GROUPS * (PASSWORD_GROUP_LEN + 1));

        for group in 0..PASSWORD_GROUPS {
            if group > 0 {
                password.push('-');
            }
            for _ in 0..PASSWORD_GROUP_LEN {
                let index = rng.gen_range(0..PASSWORD_ALPHABET.len());
                password.push(PASSWORD_ALPHABET[index] as char);
            }
        }

        password
    }

    /// Reject open-redirect payloads in the `next` query/form parameter.
    ///
    /// Only same-origin absolute paths are accepted: the value must start with
    /// `/`, must not start with `//` (protocol-relative), and must not contain
    /// CR/LF (header injection) or backslashes (which some browsers normalise
    /// to `/`). Anything else falls back to `/`.
    pub fn sanitize_next_path(next: &str) -> &str {
        let candidate = next.trim();

        if !candidate.starts_with('/')
            || candidate.starts_with("//")
            || candidate.contains('\r')
            || candidate.contains('\n')
            || candidate.contains('\\')
        {
            return "/";
        }

        candidate
    }

    fn store(&self) -> Box<dyn ModuleStore> {
        self.storage.module_store(NAMESPACE)
    }

    fn session_key(token: &str) -> String {
        format!("{SESSION_KEY_PREFIX}{}", hash_token(token))
    }
}

#[async_trait]
impl AuthProvider for BasicAuthProvider {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: "zcast-auth-basic",
            name: "Basic Auth",
            version: env!("CARGO_PKG_VERSION"),
            description: "Argon2id password login with ModuleStore-backed session cookies",
            provides: PROVIDES,
            requires: REQUIRES,
            recommends: RECOMMENDS,
        }
    }

    async fn validate_session(
        &self,
        session_token: &str,
    ) -> Result<Option<AuthSession>, AuthError> {
        if session_token.trim().is_empty() {
            return Ok(None);
        }

        let value = self
            .store()
            .get(&Self::session_key(session_token))
            .await
            .map_err(backend)?;

        match value {
            None => Ok(None),
            Some(value) => {
                let session: AuthSession = serde_json::from_value(value).map_err(backend)?;
                Ok(Some(session))
            }
        }
    }

    async fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<String>, AuthError> {
        let Some(value) = self.store().get(ADMIN_USER_KEY).await.map_err(backend)? else {
            return Ok(None);
        };
        let credentials: AdminCredentials = serde_json::from_value(value).map_err(backend)?;

        if credentials.username.trim() != username.trim() {
            return Ok(None);
        }

        let parsed = PasswordHash::new(&credentials.password_hash).map_err(backend)?;
        if Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_err()
        {
            return Ok(None);
        }

        let token = random_token();
        let session = AuthSession {
            username: credentials.username.clone(),
            is_admin: true,
        };
        let value = serde_json::to_value(&session).map_err(backend)?;
        let expires_at = Some(Utc::now() + self.session_ttl);

        self.store()
            .put(&Self::session_key(&token), &value, expires_at)
            .await
            .map_err(backend)?;

        Ok(Some(token))
    }

    async fn revoke_session(&self, session_token: &str) -> Result<(), AuthError> {
        if session_token.trim().is_empty() {
            return Ok(());
        }

        self.store()
            .delete(&Self::session_key(session_token))
            .await
            .map_err(backend)
    }

    fn session_cookie_name(&self) -> &'static str {
        SESSION_COOKIE_NAME
    }

    fn session_ttl_seconds(&self) -> u64 {
        u64::try_from(self.session_ttl.num_seconds()).unwrap_or(0)
    }

    fn header_action_slot(&self, is_admin: bool) -> Html {
        if is_admin {
            Html::from(LOG_OUT_SLOT)
        } else {
            Html::from(SIGN_IN_SLOT)
        }
    }

    fn render_login_page(
        &self,
        site_title: &str,
        color_mode: ThemeColorMode,
        error: Option<&str>,
        next: &str,
    ) -> Html {
        let safe_next = Self::sanitize_next_path(next);
        let page_title = format!("Sign in — {site_title}");
        let color_scheme = match color_mode {
            ThemeColorMode::Dark => "dark",
            ThemeColorMode::Light => "light",
            ThemeColorMode::Auto => "dark light",
        };

        let head = html! {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { (page_title) }
            meta name="color-scheme" content=(color_scheme);
            meta name="robots" content="noindex, nofollow";
            meta name="generator" content="zcast";
            link rel="stylesheet" href=(THEME_CSS_HREF);
            @if color_mode == ThemeColorMode::Auto {
                script { (PreEscaped(AUTO_THEME_SCRIPT)) }
            }
            style { (PreEscaped(LOGIN_CSS)) }
        };

        let body = html! {
            body class="zcast-auth" data-zcast-auth-page {
                main class="zcast-auth__main" {
                    section class="zcast-auth__card" aria-labelledby="zcast-auth-title" {
                        p class="zcast-auth__eyebrow" { "Admin" }
                        h1 id="zcast-auth-title" class="zcast-auth__title" { (site_title) }
                        p class="zcast-auth__lede" { "Sign in to continue." }
                        @if let Some(message) = error {
                            div class="zcast-auth__error" role="alert" { (message) }
                        }
                        form
                            class="zcast-auth__form"
                            method="post"
                            action=(LOGIN_PATH)
                            data-zcast-login-form
                        {
                            input type="hidden" name="next" value=(safe_next);
                            div class="zcast-auth__field" {
                                label class="zcast-auth__label" for="zcast-auth-username" {
                                    "Username"
                                }
                                input
                                    id="zcast-auth-username"
                                    class="zcast-auth__input"
                                    type="text"
                                    name="username"
                                    autocomplete="username"
                                    autocapitalize="none"
                                    spellcheck="false"
                                    required
                                    autofocus;
                            }
                            div class="zcast-auth__field" {
                                label class="zcast-auth__label" for="zcast-auth-password" {
                                    "Password"
                                }
                                input
                                    id="zcast-auth-password"
                                    class="zcast-auth__input"
                                    type="password"
                                    name="password"
                                    autocomplete="current-password"
                                    required;
                            }
                            button class="zcast-auth__submit" type="submit" {
                                "Sign in"
                            }
                        }
                        a class="zcast-auth__back" href="/" {
                            "Back to " (site_title)
                        }
                    }
                }
            }
        };

        let document: Markup = match color_mode {
            ThemeColorMode::Dark => html! {
                (DOCTYPE)
                html lang="en" data-theme="dark" data-color-mode="dark" {
                    (head)
                    (body)
                }
            },
            ThemeColorMode::Light => html! {
                (DOCTYPE)
                html lang="en" data-theme="light" data-color-mode="light" {
                    (head)
                    (body)
                }
            },
            ThemeColorMode::Auto => html! {
                (DOCTYPE)
                html lang="en" data-color-mode="auto" {
                    (head)
                    (body)
                }
            },
        };

        Html::from(document.into_string())
    }
}

const SIGN_IN_SLOT: &str =
    r#"<a href="/_zcast/login" class="zcast-header-btn" data-zcast-login>Sign in</a>"#;

const LOG_OUT_SLOT: &str = r#"<form method="post" action="/_zcast/logout" class="zcast-logout-form" style="display:inline"><button type="submit" class="zcast-header-btn" data-zcast-logout>Log out</button></form>"#;

/// Hash a password with Argon2id into a PHC string.
fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| AuthError::Backend(format!("failed to hash password: {err}")))
}

/// Generate a 32-byte session token, hex-encoded (64 chars).
fn random_token() -> String {
    let mut bytes = [0u8; SESSION_TOKEN_BYTES];
    let mut rng = OsRng;
    rng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// SHA-256 of a raw session token, hex-encoded (the only form ever stored).
fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn backend(err: impl std::fmt::Display) -> AuthError {
    AuthError::Backend(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcast_interfaces::Storage;
    use zcast_storage_sqlite::SqliteStorage;

    async fn provider() -> BasicAuthProvider {
        provider_with_ttl(DEFAULT_SESSION_TTL_SECONDS).await
    }

    async fn provider_with_ttl(ttl_secs: u64) -> BasicAuthProvider {
        let storage = SqliteStorage::connect("sqlite::memory:")
            .await
            .expect("connect");
        storage.initialize().await.expect("initialize");
        BasicAuthProvider::new(Arc::new(storage)).with_session_ttl_seconds(ttl_secs)
    }

    fn admin_value(raw: &serde_json::Value) -> AdminCredentials {
        serde_json::from_value(raw.clone()).expect("admin credential json")
    }

    #[tokio::test]
    async fn manifest_declares_auth_provider_and_storage_requirement() {
        let provider = provider().await;
        let manifest = provider.manifest();

        assert_eq!(manifest.id, "zcast-auth-basic");
        assert_eq!(manifest.name, "Basic Auth");
        assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest.provides.len(), 1);
        assert_eq!(manifest.provides[0].name, IFACE_AUTH);
        assert_eq!(manifest.provides[0].version, "1.0.0");

        assert_eq!(manifest.requires.len(), 1);
        assert_eq!(manifest.requires[0].name, IFACE_STORAGE);
        assert_eq!(manifest.requires[0].version_req, "^1.0");
        assert!(!manifest.requires[0].optional);
        assert!(manifest.recommends.is_empty());
    }

    #[tokio::test]
    async fn ensure_admin_credentials_hashes_explicit_password() {
        let provider = provider().await;

        let generated = provider
            .ensure_admin_credentials("admin", Some("correct horse battery staple"))
            .await
            .expect("ensure");
        assert!(generated.is_none());

        let raw = provider
            .store()
            .get(ADMIN_USER_KEY)
            .await
            .expect("get")
            .expect("stored");
        let credentials = admin_value(&raw);

        assert_eq!(credentials.username, "admin");
        assert!(credentials.password_hash.starts_with("$argon2id$"));
        assert!(!credentials.password_hash.contains("correct horse"));
        assert_ne!(credentials.password_hash, "correct horse battery staple");
    }

    #[tokio::test]
    async fn ensure_admin_credentials_generates_once_then_keeps_existing() {
        let provider = provider().await;

        let generated = provider
            .ensure_admin_credentials("admin", None)
            .await
            .expect("ensure")
            .expect("generated password");

        let groups: Vec<&str> = generated.split('-').collect();
        assert_eq!(groups.len(), 4, "four dash-separated groups: {generated}");
        for group in &groups {
            assert_eq!(group.len(), 4, "group length in {generated}");
            assert!(
                group.chars().all(|c| c.is_ascii_alphanumeric()),
                "group is alphanumeric in {generated}"
            );
        }

        // The generated password is usable immediately.
        assert!(
            provider
                .authenticate("admin", &generated)
                .await
                .expect("authenticate")
                .is_some()
        );

        // A second boot without a password must not rotate the credential.
        assert!(
            provider
                .ensure_admin_credentials("admin", None)
                .await
                .expect("second ensure")
                .is_none()
        );
        assert!(
            provider
                .authenticate("admin", &generated)
                .await
                .expect("authenticate")
                .is_some()
        );
    }

    #[tokio::test]
    async fn auto_generation_keeps_an_explicitly_set_password() {
        let provider = provider().await;

        provider
            .ensure_admin_credentials("admin", Some("s3cret-pw"))
            .await
            .expect("explicit");
        assert!(
            provider
                .ensure_admin_credentials("admin", None)
                .await
                .expect("implicit")
                .is_none()
        );

        assert!(
            provider
                .authenticate("admin", "s3cret-pw")
                .await
                .expect("authenticate")
                .is_some()
        );
    }

    #[tokio::test]
    async fn authenticate_validate_and_revoke_session_lifecycle() {
        let provider = provider().await;
        provider
            .ensure_admin_credentials("admin", Some("hunter2"))
            .await
            .expect("ensure");

        let token = provider
            .authenticate("  admin  ", "hunter2")
            .await
            .expect("authenticate")
            .expect("valid credentials");

        // Raw tokens are never stored in plaintext.
        assert_eq!(token.len(), SESSION_TOKEN_BYTES * 2);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            provider
                .store()
                .get(&format!("session:{token}"))
                .await
                .expect("raw token lookup")
                .is_none()
        );

        let session = provider
            .validate_session(&token)
            .await
            .expect("validate")
            .expect("active session");
        assert_eq!(session.username, "admin");
        assert!(session.is_admin);

        provider.revoke_session(&token).await.expect("revoke");
        assert!(
            provider
                .validate_session(&token)
                .await
                .expect("validate after revoke")
                .is_none()
        );
    }

    #[tokio::test]
    async fn revoke_all_sessions_revokes_every_session_and_keeps_admin_credentials() {
        let provider = provider().await;
        provider
            .ensure_admin_credentials("admin", Some("hunter2"))
            .await
            .expect("ensure");

        let first = provider
            .authenticate("admin", "hunter2")
            .await
            .expect("authenticate first")
            .expect("first token");
        let second = provider
            .authenticate("admin", "hunter2")
            .await
            .expect("authenticate second")
            .expect("second token");
        let third = provider
            .authenticate("admin", "hunter2")
            .await
            .expect("authenticate third")
            .expect("third token");

        assert_ne!(first, second);

        let revoked = provider.revoke_all_sessions().await.expect("revoke all");
        assert_eq!(revoked, 3, "three active sessions were revoked");

        for token in [&first, &second, &third] {
            assert!(
                provider
                    .validate_session(token)
                    .await
                    .expect("validate revoked")
                    .is_none()
            );
        }

        // The credential document survives, so the user can log in again.
        assert!(
            provider
                .store()
                .get(ADMIN_USER_KEY)
                .await
                .expect("admin lookup")
                .is_some()
        );
        assert!(
            provider
                .authenticate("admin", "hunter2")
                .await
                .expect("authenticate after revoke")
                .is_some()
        );

        // A second sweep is a no-op (only the new session above remains).
        assert_eq!(
            provider.revoke_all_sessions().await.expect("revoke again"),
            1
        );
    }

    #[tokio::test]
    async fn revoke_all_sessions_without_sessions_returns_zero() {
        let provider = provider().await;
        assert_eq!(
            provider.revoke_all_sessions().await.expect("revoke none"),
            0
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_wrong_password_and_username() {
        let provider = provider().await;
        provider
            .ensure_admin_credentials("admin", Some("hunter2"))
            .await
            .expect("ensure");

        assert!(
            provider
                .authenticate("admin", "wrong-password")
                .await
                .expect("wrong password")
                .is_none()
        );
        assert!(
            provider
                .authenticate("someone-else", "hunter2")
                .await
                .expect("wrong username")
                .is_none()
        );
    }

    #[tokio::test]
    async fn authenticate_without_credentials_returns_none() {
        let provider = provider().await;
        assert!(
            provider
                .authenticate("admin", "anything")
                .await
                .expect("authenticate")
                .is_none()
        );
    }

    #[tokio::test]
    async fn expired_sessions_are_invalid() {
        // A zero-second TTL stores an `expires_at` that is already in the past
        // by the time the session is read back.
        let provider = provider_with_ttl(0).await;
        provider
            .ensure_admin_credentials("admin", Some("hunter2"))
            .await
            .expect("ensure");

        let token = provider
            .authenticate("admin", "hunter2")
            .await
            .expect("authenticate")
            .expect("token");

        assert!(
            provider
                .validate_session(&token)
                .await
                .expect("validate")
                .is_none()
        );
    }

    #[tokio::test]
    async fn blank_tokens_are_ignored() {
        let provider = provider().await;
        assert!(
            provider
                .validate_session("")
                .await
                .expect("empty")
                .is_none()
        );
        assert!(
            provider
                .validate_session("   ")
                .await
                .expect("whitespace")
                .is_none()
        );
        provider.revoke_session("").await.expect("revoke empty");
    }

    #[tokio::test]
    async fn session_ttl_defaults_to_seven_days_and_can_be_overridden() {
        let provider = provider().await;
        assert_eq!(provider.session_ttl_seconds(), DEFAULT_SESSION_TTL_SECONDS);
        assert_eq!(provider.session_ttl_seconds(), 604_800);

        let provider = provider.with_session_ttl_seconds(1_800);
        assert_eq!(provider.session_ttl_seconds(), 1_800);
    }

    #[test]
    fn sanitize_next_path_blocks_open_redirects() {
        assert_eq!(
            BasicAuthProvider::sanitize_next_path("/my-note?stack=a,b"),
            "/my-note?stack=a,b"
        );
        assert_eq!(BasicAuthProvider::sanitize_next_path("/"), "/");
        assert_eq!(
            BasicAuthProvider::sanitize_next_path("  /padded  "),
            "/padded"
        );
        assert_eq!(BasicAuthProvider::sanitize_next_path("//evil.com"), "/");
        assert_eq!(
            BasicAuthProvider::sanitize_next_path("https://evil.com"),
            "/"
        );
        assert_eq!(BasicAuthProvider::sanitize_next_path("/\\evil.com"), "/");
        assert_eq!(
            BasicAuthProvider::sanitize_next_path("/\r\nSet-Cookie: x=y"),
            "/"
        );
        assert_eq!(BasicAuthProvider::sanitize_next_path("relative/path"), "/");
        assert_eq!(BasicAuthProvider::sanitize_next_path(""), "/");
    }

    #[test]
    fn generate_password_shape_and_entropy() {
        let first = BasicAuthProvider::generate_password();
        let second = BasicAuthProvider::generate_password();

        assert_eq!(first.len(), 19);
        assert_eq!(first.matches('-').count(), 3);
        assert!(first.split('-').all(|group| group.len() == 4));
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn render_login_page_includes_error_sanitized_next_and_theme() {
        let provider = provider().await;

        let html = provider.render_login_page(
            "David's Notes",
            ThemeColorMode::Dark,
            Some("Incorrect username or password."),
            "//evil.com",
        );
        let html = html.as_str();

        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("data-theme=\"dark\""));
        assert!(html.contains("data-color-mode=\"dark\""));
        assert!(html.contains("/_zcast/theme/theme.css"));
        assert!(html.contains("role=\"alert\""));
        assert!(html.contains("Incorrect username or password."));
        assert!(html.contains("name=\"next\" value=\"/\""));
        assert!(!html.contains("evil.com"));
        assert!(html.contains("autocomplete=\"username\""));
        assert!(html.contains("autocomplete=\"current-password\""));
        assert!(
            html.contains("Back to David&#39;s Notes") || html.contains("Back to David's Notes")
        );
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("action=\"/_zcast/login\""));
    }

    #[tokio::test]
    async fn render_login_page_auto_mode_bootstraps_theme_and_keeps_safe_next() {
        let provider = provider().await;

        let html =
            provider.render_login_page("Notes", ThemeColorMode::Auto, None, "/my-note?stack=a,b");
        let html = html.as_str();

        assert!(html.contains("data-color-mode=\"auto\""));
        assert!(!html.contains("data-theme="));
        assert!(html.contains("localStorage.getItem('zcast-color-mode')"));
        assert!(html.contains("name=\"next\" value=\"/my-note?stack=a,b\""));
        assert!(!html.contains("role=\"alert\""));
        assert!(html.contains("data-zcast-login-form"));
    }

    #[tokio::test]
    async fn header_action_slot_switches_between_sign_in_and_log_out() {
        let provider = provider().await;

        let guest = provider.header_action_slot(false).into_string();
        assert!(guest.contains("href=\"/_zcast/login\""));
        assert!(guest.contains("data-zcast-login"));
        assert!(guest.contains(">Sign in</a>"));
        assert!(!guest.contains("data-zcast-logout"));

        let admin = provider.header_action_slot(true).into_string();
        assert!(admin.contains("method=\"post\" action=\"/_zcast/logout\""));
        assert!(admin.contains("data-zcast-logout"));
        assert!(admin.contains(">Log out</button>"));
        assert!(!admin.contains("data-zcast-login"));
    }

    #[tokio::test]
    async fn cookie_name_and_login_slot_use_kernel_routes() {
        let provider = provider().await;
        assert_eq!(provider.session_cookie_name(), "zcast_session");
    }
}
