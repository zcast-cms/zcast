//! Scanner-safe magic-link authentication provider for zcast.
//!
//! Implements the optional `zcast:auth/provider@1.0` contract with a single
//! admin email address. Everything is persisted through the DB-agnostic
//! [`ModuleStore`] (DEC-09) — this crate contains **zero SQL** and therefore
//! works unchanged on SQLite, MySQL or any future `Storage` backend.
//!
//! Security properties:
//! - Magic-link tokens are 32 random bytes (hex-encoded) and are only ever
//!   stored as `magic:<sha256_hex(token)>`, so a database leak cannot be
//!   replayed as a live link.
//! - `GET /_zcast/auth/magic?token=...` only *peeks* at a token. Consuming it
//!   requires a `POST`, protecting links from email security scanners that
//!   pre-fetch URLs (Guardrail #3).
//! - Consuming a token deletes it before returning, making replay impossible.
//! - Sessions are stored as `session:<sha256_hex(token)>` and expire after
//!   [`DEFAULT_SESSION_TTL_SECONDS`] (7 days).
//! - Unknown emails always produce `Ok(None)` without minting a token, so the
//!   login page never leaks whether an address is the configured admin.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zcast_interfaces::{
    AuthError, AuthProvider, AuthSession, Html, IFACE_AUTH, IFACE_STORAGE, InterfaceReq,
    InterfaceSpec, ModuleManifest, ModuleStore, Storage, ThemeColorMode,
};

/// Module-store namespace owned by this provider.
const NAMESPACE: &str = "zcast-auth-magic-link";

/// Prefix for hashed magic-link records (`magic:<sha256_hex(token)>`).
const MAGIC_KEY_PREFIX: &str = "magic:";

/// Prefix for hashed session records (`session:<sha256_hex(token)>`).
const SESSION_KEY_PREFIX: &str = "session:";

/// Session cookie name shared with the kernel's HTTP layer.
const SESSION_COOKIE_NAME: &str = "zcast_session";

/// Magic-link token lifetime: fifteen minutes.
pub const DEFAULT_TOKEN_TTL_SECONDS: u64 = 60 * 15;

/// Default session lifetime: seven days.
pub const DEFAULT_SESSION_TTL_SECONDS: u64 = 60 * 60 * 24 * 7;

/// Length (in bytes) of raw tokens before hex encoding.
const TOKEN_BYTES: usize = 32;

const THEME_CSS_HREF: &str = "/_zcast/theme/theme.css";
const LOGIN_PATH: &str = "/_zcast/login";
const MAGIC_PATH: &str = "/_zcast/auth/magic";

/// Same pre-paint bootstrap the first-party theme uses for `auto` mode.
const AUTO_THEME_SCRIPT: &str = r#"(function(){var r=document.documentElement,s=null;try{s=localStorage.getItem('zcast-color-mode');}catch(e){}if(s!=='dark'&&s!=='light'){s=(window.matchMedia&&window.matchMedia('(prefers-color-scheme: light)').matches)?'light':'dark';}r.setAttribute('data-theme',s);})();"#;

/// Inline CSS for the login and confirmation cards. Deliberately consumes the
/// theme's `--zcast-*` design tokens (with sane fallbacks) so the pages inherit
/// the active palette, typography and radius without shipping a full
/// stylesheet.
const AUTH_CSS: &str = r#"
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
.zcast-auth__notice{
  margin:0 0 1.25rem;
  padding:.75rem 1rem;
  border:1px solid var(--zcast-border,rgba(0,0,0,.18));
  border-radius:calc(var(--zcast-radius,.5rem)/1.6);
  background:var(--zcast-surface-muted,rgba(0,0,0,.04));
  color:var(--zcast-text-strong,#111);
  font-size:.9rem;
}
.zcast-auth__notice--error{
  border-color:var(--zcast-accent,rgba(0,0,0,.4));
  background:var(--zcast-accent-subtle,rgba(0,0,0,.05));
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

/// Stored magic-link document (`magic:<sha256_hex(token)>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MagicToken {
    email: String,
}

/// Scanner-safe magic-link authentication provider backed by [`ModuleStore`].
pub struct MagicLinkAuthProvider {
    storage: Arc<dyn Storage>,
    admin_email: String,
    token_ttl: Duration,
    session_ttl: Duration,
}

impl MagicLinkAuthProvider {
    /// Create a provider using the default 15-minute token and 7-day session
    /// lifetimes. The admin email is trimmed and lower-cased once here.
    pub fn new(storage: Arc<dyn Storage>, admin_email: impl Into<String>) -> Self {
        Self {
            storage,
            admin_email: admin_email.into().trim().to_ascii_lowercase(),
            token_ttl: Duration::seconds(DEFAULT_TOKEN_TTL_SECONDS as i64),
            session_ttl: Duration::seconds(DEFAULT_SESSION_TTL_SECONDS as i64),
        }
    }

    /// Override the magic-link token lifetime (seconds).
    pub fn with_token_ttl_seconds(mut self, secs: u64) -> Self {
        self.token_ttl = duration_from_secs(secs);
        self
    }

    /// Override the session lifetime (seconds).
    pub fn with_session_ttl_seconds(mut self, secs: u64) -> Self {
        self.session_ttl = duration_from_secs(secs);
        self
    }

    /// The configured admin email, normalised to lower-case.
    pub fn admin_email(&self) -> &str {
        &self.admin_email
    }

    /// Mint a one-time login link for SSH recovery (`zcast auth login-link`).
    ///
    /// The raw token is returned only inside the URL; the store keeps just its
    /// SHA-256 hash with a short `token_ttl`.
    pub async fn create_cli_login_link(&self, base_url: &str) -> Result<String, AuthError> {
        let token = random_token();
        let value = serde_json::to_value(MagicToken {
            email: self.admin_email.clone(),
        })
        .map_err(backend)?;
        let expires_at = Some(Utc::now() + self.token_ttl);

        self.store()
            .put(&Self::magic_key(&token), &value, expires_at)
            .await
            .map_err(backend)?;

        let base = base_url.trim().trim_end_matches('/');
        Ok(format!("{base}{MAGIC_PATH}?token={token}"))
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

    fn magic_key(token: &str) -> String {
        format!("{MAGIC_KEY_PREFIX}{}", hash_token(token))
    }

    fn session_key(token: &str) -> String {
        format!("{SESSION_KEY_PREFIX}{}", hash_token(token))
    }
}

#[async_trait]
impl AuthProvider for MagicLinkAuthProvider {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: "zcast-auth-magic-link",
            name: "Magic Link Auth",
            version: env!("CARGO_PKG_VERSION"),
            description: "Scanner-safe one-time email login with ModuleStore-backed sessions",
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

    fn supports_magic_link(&self) -> bool {
        true
    }

    async fn request_magic_link(
        &self,
        email: &str,
        base_url: &str,
    ) -> Result<Option<String>, AuthError> {
        // Never leak whether an address is the configured admin: unknown
        // emails (and an unconfigured provider) simply return `Ok(None)`.
        if self.admin_email.is_empty() || email.trim().to_ascii_lowercase() != self.admin_email {
            return Ok(None);
        }

        let url = self.create_cli_login_link(base_url).await?;
        tracing::info!(
            email = %self.admin_email,
            url = %url,
            "magic login link minted"
        );
        Ok(Some(url))
    }

    async fn peek_magic_token(&self, token: &str) -> Result<bool, AuthError> {
        if token.trim().is_empty() {
            return Ok(false);
        }

        // Deliberately no delete: scanners pre-fetch GET URLs and must never
        // be able to burn a real recipient's link.
        let value = self
            .store()
            .get(&Self::magic_key(token))
            .await
            .map_err(backend)?;

        Ok(value.is_some())
    }

    async fn consume_magic_token(&self, token: &str) -> Result<Option<String>, AuthError> {
        if token.trim().is_empty() {
            return Ok(None);
        }

        let store = self.store();
        let key = Self::magic_key(token);

        let Some(value) = store.get(&key).await.map_err(backend)? else {
            return Ok(None);
        };
        if value.is_null() {
            return Ok(None);
        }
        let magic: MagicToken = serde_json::from_value(value).map_err(backend)?;

        // Delete before minting so replay of the same link is impossible, even
        // if session creation fails below.
        store.delete(&key).await.map_err(backend)?;

        let session_token = random_token();
        let session = AuthSession {
            username: magic.email,
            is_admin: true,
        };
        let session_value = serde_json::to_value(&session).map_err(backend)?;
        let expires_at = Some(Utc::now() + self.session_ttl);

        store
            .put(
                &Self::session_key(&session_token),
                &session_value,
                expires_at,
            )
            .await
            .map_err(backend)?;

        Ok(Some(session_token))
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
        notice_or_error: Option<&str>,
        next: &str,
    ) -> Html {
        let safe_next = Self::sanitize_next_path(next);
        let head = auth_head(&format!("Sign in — {site_title}"), color_mode);

        let body = html! {
            body class="zcast-auth" data-zcast-auth-page {
                main class="zcast-auth__main" {
                    section class="zcast-auth__card" aria-labelledby="zcast-auth-title" {
                        p class="zcast-auth__eyebrow" { "Admin access" }
                        h1 id="zcast-auth-title" class="zcast-auth__title" { (site_title) }
                        p class="zcast-auth__lede" {
                            "Enter your admin email and we'll send you a one-time sign-in link."
                        }
                        @if let Some(message) = notice_or_error {
                            div class="zcast-auth__notice" role="alert" { (message) }
                        }
                        form
                            class="zcast-auth__form"
                            method="post"
                            action=(LOGIN_PATH)
                            data-zcast-magic-request-form
                        {
                            input type="hidden" name="next" value=(safe_next);
                            div class="zcast-auth__field" {
                                label class="zcast-auth__label" for="zcast-auth-email" {
                                    "Email"
                                }
                                input
                                    id="zcast-auth-email"
                                    class="zcast-auth__input"
                                    type="email"
                                    name="email"
                                    autocomplete="email"
                                    autocapitalize="none"
                                    spellcheck="false"
                                    required
                                    autofocus;
                            }
                            button class="zcast-auth__submit" type="submit" {
                                "Send magic link"
                            }
                        }
                        a class="zcast-auth__back" href="/" {
                            "Back to " (site_title)
                        }
                    }
                }
            }
        };

        render_document(color_mode, head, body)
    }

    fn render_magic_confirm_page(
        &self,
        site_title: &str,
        color_mode: ThemeColorMode,
        token: &str,
        valid: bool,
        next: &str,
    ) -> Html {
        if valid {
            let safe_next = Self::sanitize_next_path(next);
            let page_title = format!("Confirm sign in — {site_title}");
            let head = auth_head(&page_title, color_mode);
            let action_label = format!("Sign in to {site_title}");

            let body = html! {
                body class="zcast-auth" data-zcast-auth-page {
                    main class="zcast-auth__main" {
                        section class="zcast-auth__card" aria-labelledby="zcast-auth-title" {
                            p class="zcast-auth__eyebrow" { "Confirm sign-in" }
                            h1 id="zcast-auth-title" class="zcast-auth__title" {
                                "One last step"
                            }
                            p class="zcast-auth__lede" {
                                "For your security, confirm that you want to sign in. "
                                "This link can only be used once."
                            }
                            form
                                class="zcast-auth__form"
                                method="post"
                                action=(MAGIC_PATH)
                                data-zcast-magic-confirm-form
                            {
                                input type="hidden" name="token" value=(token);
                                input type="hidden" name="next" value=(safe_next);
                                button
                                    class="zcast-auth__submit"
                                    type="submit"
                                    data-zcast-magic-confirm
                                {
                                    (action_label)
                                }
                            }
                            a class="zcast-auth__back" href=(LOGIN_PATH) {
                                "Use a different email"
                            }
                        }
                    }
                }
            };

            render_document(color_mode, head, body)
        } else {
            let page_title = format!("Link expired — {site_title}");
            let head = auth_head(&page_title, color_mode);

            let body = html! {
                body class="zcast-auth" data-zcast-auth-page {
                    main class="zcast-auth__main" {
                        section class="zcast-auth__card" aria-labelledby="zcast-auth-title" {
                            p class="zcast-auth__eyebrow" { "Link expired" }
                            h1 id="zcast-auth-title" class="zcast-auth__title" {
                                "This link is no longer valid"
                            }
                            p class="zcast-auth__lede" {
                                "Magic links expire after 15 minutes and can only be used once."
                            }
                            div
                                class="zcast-auth__notice zcast-auth__notice--error"
                                role="alert"
                            {
                                "That sign-in link has expired or has already been used."
                            }
                            a class="zcast-auth__back" href=(LOGIN_PATH) {
                                "Request a new sign-in link"
                            }
                        }
                    }
                }
            };

            render_document(color_mode, head, body)
        }
    }
}

const SIGN_IN_SLOT: &str =
    r#"<a href="/_zcast/login" class="zcast-header-btn" data-zcast-login>Sign in</a>"#;

const LOG_OUT_SLOT: &str = r#"<form method="post" action="/_zcast/logout" class="zcast-logout-form" style="display:inline"><button type="submit" class="zcast-header-btn" data-zcast-logout>Log out</button></form>"#;

fn auth_head(page_title: &str, color_mode: ThemeColorMode) -> Markup {
    let color_scheme = match color_mode {
        ThemeColorMode::Dark => "dark",
        ThemeColorMode::Light => "light",
        ThemeColorMode::Auto => "dark light",
    };

    html! {
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
        style { (PreEscaped(AUTH_CSS)) }
    }
}

fn render_document(color_mode: ThemeColorMode, head: Markup, body: Markup) -> Html {
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

/// Clamp a seconds value into a [`chrono::Duration`] without overflow.
fn duration_from_secs(secs: u64) -> Duration {
    Duration::seconds(secs.min(i64::MAX as u64) as i64)
}

/// Generate a 32-byte token, hex-encoded (64 chars).
fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    let mut rng = OsRng;
    rng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// SHA-256 of a raw token, hex-encoded (the only form ever stored).
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

    async fn provider() -> MagicLinkAuthProvider {
        provider_with_ttls(DEFAULT_TOKEN_TTL_SECONDS, DEFAULT_SESSION_TTL_SECONDS).await
    }

    async fn provider_with_ttls(token_ttl: u64, session_ttl: u64) -> MagicLinkAuthProvider {
        let storage = SqliteStorage::connect("sqlite::memory:")
            .await
            .expect("connect");
        storage.initialize().await.expect("initialize");
        MagicLinkAuthProvider::new(Arc::new(storage), "  Admin@Example.COM ")
            .with_token_ttl_seconds(token_ttl)
            .with_session_ttl_seconds(session_ttl)
    }

    fn token_from_url(url: &str) -> &str {
        url.split_once("token=")
            .map(|(_, token)| token)
            .expect("token query parameter")
    }

    #[tokio::test]
    async fn manifest_declares_auth_provider_and_storage_requirement() {
        let provider = provider().await;
        let manifest = provider.manifest();

        assert_eq!(manifest.id, "zcast-auth-magic-link");
        assert_eq!(manifest.name, "Magic Link Auth");
        assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest.provides.len(), 1);
        assert_eq!(manifest.provides[0].name, IFACE_AUTH);
        assert_eq!(manifest.provides[0].version, "1.0.0");

        assert_eq!(manifest.requires.len(), 1);
        assert_eq!(manifest.requires[0].name, IFACE_STORAGE);
        assert_eq!(manifest.requires[0].version_req, "^1.0");
        assert!(!manifest.requires[0].optional);
        assert!(manifest.recommends.is_empty());

        assert!(provider.supports_magic_link());
        assert_eq!(provider.admin_email(), "admin@example.com");
        assert_eq!(provider.session_ttl_seconds(), DEFAULT_SESSION_TTL_SECONDS);
        assert_eq!(provider.session_ttl_seconds(), 604_800);
    }

    #[tokio::test]
    async fn request_magic_link_matches_case_insensitively_and_ignores_unknown() {
        let provider = provider().await;

        let url = provider
            .request_magic_link("  ADMIN@example.com  ", "https://notes.example.com/")
            .await
            .expect("request")
            .expect("matching admin email");
        assert!(url.starts_with("https://notes.example.com/_zcast/auth/magic?token="));
        let token = token_from_url(&url);
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            provider
                .peek_magic_token(token)
                .await
                .expect("peek minted token")
        );

        // Unknown email: no link and, crucially, no token minted in the store.
        assert!(
            provider
                .request_magic_link("someone@else.com", "https://notes.example.com")
                .await
                .expect("unknown email")
                .is_none()
        );
        let magic_entries = provider
            .store()
            .list_prefix(MAGIC_KEY_PREFIX, None)
            .await
            .expect("list magic");
        assert_eq!(magic_entries.len(), 1, "only the matching request minted");
    }

    #[tokio::test]
    async fn unconfigured_admin_email_never_mints_a_link() {
        let storage = SqliteStorage::connect("sqlite::memory:")
            .await
            .expect("connect");
        storage.initialize().await.expect("initialize");
        let provider = MagicLinkAuthProvider::new(Arc::new(storage), "  ");

        assert!(provider.admin_email().is_empty());
        assert!(
            provider
                .request_magic_link("admin@example.com", "https://notes.example.com")
                .await
                .expect("request")
                .is_none()
        );
        assert!(
            provider
                .store()
                .list_prefix(MAGIC_KEY_PREFIX, None)
                .await
                .expect("list")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn peek_magic_token_never_consumes() {
        let provider = provider().await;
        let url = provider
            .request_magic_link("admin@example.com", "https://notes.example.com")
            .await
            .expect("request")
            .expect("link");
        let token = token_from_url(&url);

        for _ in 0..3 {
            assert!(provider.peek_magic_token(token).await.expect("peek"));
        }

        let session_token = provider
            .consume_magic_token(token)
            .await
            .expect("consume")
            .expect("token still valid after peeks");
        assert!(
            provider
                .validate_session(&session_token)
                .await
                .expect("validate")
                .is_some()
        );
    }

    #[tokio::test]
    async fn consume_magic_token_works_once_and_creates_admin_session() {
        let provider = provider().await;
        let url = provider
            .request_magic_link("admin@example.com", "https://notes.example.com")
            .await
            .expect("request")
            .expect("link");
        let token = token_from_url(&url);

        let session_token = provider
            .consume_magic_token(token)
            .await
            .expect("consume")
            .expect("fresh session token");
        assert_eq!(session_token.len(), TOKEN_BYTES * 2);
        assert!(session_token.chars().all(|c| c.is_ascii_hexdigit()));

        let session = provider
            .validate_session(&session_token)
            .await
            .expect("validate")
            .expect("active session");
        assert_eq!(session.username, "admin@example.com");
        assert!(session.is_admin);

        // Raw tokens and consumed magic tokens are never stored in plaintext.
        assert!(
            provider
                .store()
                .get(&format!("session:{session_token}"))
                .await
                .expect("raw session lookup")
                .is_none()
        );
        assert!(
            provider
                .store()
                .get(&format!("magic:{token}"))
                .await
                .expect("raw magic lookup")
                .is_none()
        );

        // Replay must fail, and the original magic token must be gone.
        assert!(
            provider
                .consume_magic_token(token)
                .await
                .expect("second consume")
                .is_none()
        );
        assert!(
            !provider
                .peek_magic_token(token)
                .await
                .expect("peek after consume")
        );

        provider
            .revoke_session(&session_token)
            .await
            .expect("revoke");
        assert!(
            provider
                .validate_session(&session_token)
                .await
                .expect("validate after revoke")
                .is_none()
        );
    }

    #[tokio::test]
    async fn expired_magic_tokens_are_invalid() {
        let provider = provider_with_ttls(0, DEFAULT_SESSION_TTL_SECONDS).await;
        let url = provider
            .request_magic_link("admin@example.com", "https://notes.example.com")
            .await
            .expect("request")
            .expect("link");
        let token = token_from_url(&url);

        assert!(
            !provider
                .peek_magic_token(token)
                .await
                .expect("peek expired")
        );
        assert!(
            provider
                .consume_magic_token(token)
                .await
                .expect("consume expired")
                .is_none()
        );
    }

    #[tokio::test]
    async fn cli_login_link_token_can_be_consumed_into_a_working_session() {
        let provider = provider().await;
        let url = provider
            .create_cli_login_link(" https://notes.example.com/ ")
            .await
            .expect("create link");
        assert!(url.starts_with("https://notes.example.com/_zcast/auth/magic?token="));

        let token = token_from_url(&url);
        let session_token = provider
            .consume_magic_token(token)
            .await
            .expect("consume")
            .expect("session token");

        let session = provider
            .validate_session(&session_token)
            .await
            .expect("validate")
            .expect("active session");
        assert_eq!(session.username, "admin@example.com");
        assert!(session.is_admin);
    }

    #[test]
    fn sanitize_next_path_blocks_open_redirects() {
        assert_eq!(
            MagicLinkAuthProvider::sanitize_next_path("/my-note?stack=a,b"),
            "/my-note?stack=a,b"
        );
        assert_eq!(MagicLinkAuthProvider::sanitize_next_path("/"), "/");
        assert_eq!(
            MagicLinkAuthProvider::sanitize_next_path("  /padded  "),
            "/padded"
        );
        assert_eq!(MagicLinkAuthProvider::sanitize_next_path("//evil.com"), "/");
        assert_eq!(
            MagicLinkAuthProvider::sanitize_next_path("https://evil.com"),
            "/"
        );
        assert_eq!(
            MagicLinkAuthProvider::sanitize_next_path("/\\evil.com"),
            "/"
        );
        assert_eq!(
            MagicLinkAuthProvider::sanitize_next_path("/\r\nSet-Cookie: x=y"),
            "/"
        );
        assert_eq!(
            MagicLinkAuthProvider::sanitize_next_path("relative/path"),
            "/"
        );
        assert_eq!(MagicLinkAuthProvider::sanitize_next_path(""), "/");
    }

    #[tokio::test]
    async fn render_login_page_includes_notice_sanitized_next_and_theme() {
        let provider = provider().await;

        let html = provider.render_login_page(
            "David's Notes",
            ThemeColorMode::Dark,
            Some("Check your inbox for a sign-in link."),
            "//evil.com",
        );
        let html = html.as_str();

        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("data-theme=\"dark\""));
        assert!(html.contains("data-color-mode=\"dark\""));
        assert!(html.contains("/_zcast/theme/theme.css"));
        assert!(html.contains("role=\"alert\""));
        assert!(html.contains("Check your inbox for a sign-in link."));
        assert!(html.contains("name=\"next\" value=\"/\""));
        assert!(!html.contains("evil.com"));
        assert!(html.contains("type=\"email\""));
        assert!(html.contains("name=\"email\""));
        assert!(html.contains("autocomplete=\"email\""));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("action=\"/_zcast/login\""));
        assert!(html.contains("Send magic link"));
        assert!(html.contains("data-zcast-magic-request-form"));
    }

    #[tokio::test]
    async fn render_login_page_auto_mode_bootstraps_theme() {
        let provider = provider().await;

        let html =
            provider.render_login_page("Notes", ThemeColorMode::Auto, None, "/my-note?stack=a,b");
        let html = html.as_str();

        assert!(html.contains("data-color-mode=\"auto\""));
        assert!(!html.contains("data-theme="));
        assert!(html.contains("localStorage.getItem('zcast-color-mode')"));
        assert!(html.contains("name=\"next\" value=\"/my-note?stack=a,b\""));
        assert!(!html.contains("role=\"alert\""));
    }

    #[tokio::test]
    async fn render_magic_confirm_page_is_scanner_safe_and_posts_to_consume() {
        let provider = provider().await;

        let html = provider.render_magic_confirm_page(
            "David's Notes",
            ThemeColorMode::Light,
            "abc123token",
            true,
            "//evil.com",
        );
        let html = html.as_str();

        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("data-theme=\"light\""));
        assert!(html.contains("data-color-mode=\"light\""));
        assert!(html.contains("/_zcast/theme/theme.css"));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("action=\"/_zcast/auth/magic\""));
        assert!(html.contains("name=\"token\" value=\"abc123token\""));
        assert!(html.contains("name=\"next\" value=\"/\""));
        assert!(!html.contains("evil.com"));
        assert!(html.contains("data-zcast-magic-confirm"));
        assert!(
            html.contains("Sign in to David&#39;s Notes")
                || html.contains("Sign in to David's Notes")
        );
        assert!(!html.contains("no longer valid"));
    }

    #[tokio::test]
    async fn render_magic_confirm_page_expired_offers_a_fresh_login() {
        let provider = provider().await;

        let html = provider.render_magic_confirm_page(
            "Notes",
            ThemeColorMode::Auto,
            "abc123token",
            false,
            "/",
        );
        let html = html.as_str();

        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("data-color-mode=\"auto\""));
        assert!(html.contains("This link is no longer valid"));
        assert!(html.contains("has expired or has already been used"));
        assert!(html.contains("href=\"/_zcast/login\""));
        assert!(html.contains("Request a new sign-in link"));
        assert!(!html.contains("data-zcast-magic-confirm"));
        assert!(!html.contains("name=\"token\""));
    }

    #[tokio::test]
    async fn header_action_slot_switches_between_sign_in_and_log_out() {
        let provider = provider().await;

        let guest = provider.header_action_slot(false).into_string();
        assert_eq!(guest, SIGN_IN_SLOT);
        assert!(guest.contains("href=\"/_zcast/login\""));

        let admin = provider.header_action_slot(true).into_string();
        assert_eq!(admin, LOG_OUT_SLOT);
        assert!(admin.contains("method=\"post\" action=\"/_zcast/logout\""));
        assert!(admin.contains("data-zcast-logout"));
    }
}
