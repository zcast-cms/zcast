use crate::error::KernelError;
use crate::kernel::{AppSettings, ZcastKernel};
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html as AxumHtml, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use zcast_interfaces::{
    Asset, BreadcrumbItem, CardView, NoteId, NoteListFilter, NoteSort, PageContext, ReadContext,
    SearchCapabilities, SearchHit, SearchQuery, StorageError, ThemeColorMode,
};

/// Neutral confirmation shown for every magic-link request so the endpoint
/// never leaks whether an address is the configured administrator.
const MAGIC_LINK_NOTICE: &str = "If that email matches the site administrator, a one-time \
sign-in link has been sent (check your inbox or server logs).";

pub fn router(kernel: Arc<ZcastKernel>) -> Router {
    Router::new()
        .route("/_zcast/health", get(health))
        .route("/_zcast/modules", get(list_modules))
        .route("/_zcast/theme/{*path}", get(theme_asset))
        .route("/_zcast/editor/{*path}", get(editor_asset))
        .route("/_zcast/ext/{prefix}/{*path}", get(extension_asset))
        .route("/_zcast/custom.css", get(custom_css))
        .route("/_zcast/login", get(login_page).post(login_submit))
        .route(
            "/_zcast/auth/magic",
            get(magic_confirm_page).post(magic_confirm_submit),
        )
        .route("/_zcast/logout", post(logout_submit))
        .route("/_zcast/admin", get(admin_page))
        .route("/_zcast/admin/settings", post(admin_update_settings))
        .route("/_zcast/api/export.zip", get(export_zip_api))
        .route("/_zcast/popover/{slug}", get(get_popover))
        .route("/_zcast/search", get(search_notes))
        .route("/_zcast/cards/{slug}", get(get_card_fragment))
        .route(
            "/_zcast/api/notes",
            get(list_notes_api).post(create_note_api),
        )
        .route(
            "/_zcast/api/notes/{id}",
            get(get_note_api)
                .put(update_note_api)
                .delete(delete_note_api),
        )
        .route("/", get(home_page))
        .route("/{slug}", get(note_page))
        .layer(TraceLayer::new_for_http())
        .with_state(kernel)
}

#[derive(Debug, Default, Deserialize)]
struct PageQuery {
    stack: Option<String>,
    preview: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LoginForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    next: String,
}

#[derive(Debug, Default, Deserialize)]
struct MagicQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    next: String,
}

#[derive(Debug, Default, Deserialize)]
struct MagicForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    next: String,
}

#[derive(Debug, Default, Deserialize)]
struct AdminQuery {
    #[serde(default)]
    saved: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AdminSettingsForm {
    #[serde(default)]
    site_title: String,
    #[serde(default)]
    pinned_home_slug: String,
    #[serde(default)]
    color_mode: String,
}

#[derive(Debug, Default, Deserialize)]
struct SearchParams {
    q: Option<String>,
    limit: Option<usize>,
    context: Option<String>,
    preview: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct NotesQuery {
    q: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NotePayload {
    title: String,
    markdown_body: String,
    #[serde(default)]
    private: bool,
}

#[derive(Debug, Serialize)]
struct CardFragmentResponse {
    card: CardView,
    html: String,
    canonical_slug: String,
}

#[derive(Debug, Serialize)]
struct PopoverResponse {
    slug: String,
    canonical_slug: String,
    title: String,
    excerpt_html: String,
    backlink_count: usize,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
struct SearchResponse {
    query: String,
    capabilities: SearchCapabilities,
    hits: Vec<SearchHit>,
}

#[derive(Debug, Serialize)]
struct NoteSummary {
    id: NoteId,
    slug: String,
    title: String,
    private: bool,
}

#[derive(Debug, Serialize)]
struct SaveResponse {
    note: zcast_interfaces::Note,
    rendered_html: String,
    card_html: String,
    renamed_from_slug: Option<String>,
}

fn extract_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let trimmed = part.trim();
        if let Some((k, v)) = trimmed.split_once('=')
            && k.trim() == name
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

async fn resolve_is_admin(
    kernel: &ZcastKernel,
    headers: &HeaderMap,
    preview: Option<&str>,
) -> bool {
    if preview == Some("public") {
        return false;
    }
    match &kernel.auth {
        None => true, // Local no-auth mode (protected by loopback bind check)
        Some(auth) => {
            let Some(token) = extract_cookie(headers, auth.session_cookie_name()) else {
                return false;
            };
            matches!(auth.validate_session(&token).await, Ok(Some(s)) if s.is_admin)
        }
    }
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn list_modules(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return Err(ApiError(KernelError::Forbidden));
    }
    Ok(Json(kernel.registry_snapshot()))
}

fn asset_response(asset: Option<Asset>) -> Response {
    match asset {
        Some(a) => {
            let mut resp = Response::new(axum::body::Body::from(a.bytes.into_owned()));
            *resp.status_mut() = StatusCode::OK;
            let headers = resp.headers_mut();
            if let Ok(ct) = HeaderValue::from_str(a.content_type) {
                headers.insert(header::CONTENT_TYPE, ct);
            }
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=300"),
            );
            resp
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn theme_asset(State(kernel): State<Arc<ZcastKernel>>, Path(path): Path<String>) -> Response {
    asset_response(kernel.theme.get_asset(&path))
}

async fn editor_asset(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(path): Path<String>,
) -> Response {
    let asset = kernel.editor.as_ref().and_then(|e| e.get_asset(&path));
    asset_response(asset)
}

async fn extension_asset(
    State(kernel): State<Arc<ZcastKernel>>,
    Path((prefix, path)): Path<(String, String)>,
) -> Response {
    asset_response(kernel.get_extension_asset(&prefix, &path))
}

fn sanitize_redirect_path(next: &str) -> &str {
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

fn request_is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|proto| {
            proto
                .split(',')
                .next()
                .is_some_and(|first| first.trim().eq_ignore_ascii_case("https"))
        })
}

async fn login_page(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let Some(auth) = &kernel.auth else {
        return Redirect::to("/").into_response();
    };
    let next = sanitize_redirect_path(query.next.as_deref().unwrap_or("/"));
    if resolve_is_admin(&kernel, &headers, None).await {
        return Redirect::to(next).into_response();
    }
    let settings = kernel.app_settings();
    let html = auth.render_login_page(&settings.site_title, settings.color_mode, None, next);
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        AxumHtml(html.into_string()),
    )
        .into_response()
}

async fn login_submit(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let Some(auth) = &kernel.auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let next = sanitize_redirect_path(&form.next);

    if auth.supports_magic_link() {
        // Always answer with the same neutral notice: whether the address
        // matched the administrator must never be observable.
        let base_url = request_base_url(&kernel, &headers);
        if let Err(error) = auth.request_magic_link(&form.email, &base_url).await {
            tracing::warn!(%error, "could not mint a magic login link");
        }
        let settings = kernel.app_settings();
        let html = auth.render_login_page(
            &settings.site_title,
            settings.color_mode,
            Some(MAGIC_LINK_NOTICE),
            next,
        );
        return (
            StatusCode::OK,
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            AxumHtml(html.into_string()),
        )
            .into_response();
    }

    match auth.authenticate(&form.username, &form.password).await {
        Ok(Some(token)) => {
            let cookie = session_cookie(auth.as_ref(), &token, &headers);
            let mut resp = Redirect::to(next).into_response();
            if let Ok(val) = HeaderValue::from_str(&cookie) {
                resp.headers_mut().insert(header::SET_COOKIE, val);
            }
            resp
        }
        Ok(None) => {
            let settings = kernel.app_settings();
            let html = auth.render_login_page(
                &settings.site_title,
                settings.color_mode,
                Some("Invalid username or password."),
                next,
            );
            (
                StatusCode::UNAUTHORIZED,
                [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
                AxumHtml(html.into_string()),
            )
                .into_response()
        }
        Err(e) => {
            let settings = kernel.app_settings();
            let html = auth.render_login_page(
                &settings.site_title,
                settings.color_mode,
                Some(&format!("Sign-in failed: {e}")),
                next,
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
                AxumHtml(html.into_string()),
            )
                .into_response()
        }
    }
}

/// Build the `Set-Cookie` value for a freshly minted session token.
fn session_cookie(
    auth: &dyn zcast_interfaces::AuthProvider,
    token: &str,
    headers: &HeaderMap,
) -> String {
    let secure = if request_is_https(headers) {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
        auth.session_cookie_name(),
        token,
        auth.session_ttl_seconds(),
        secure
    )
}

/// Resolve the canonical base URL for absolute links.
fn request_base_url(kernel: &ZcastKernel, headers: &HeaderMap) -> String {
    if let Some(configured) = &kernel.config.public_url {
        let trimmed = configured.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    let scheme = if request_is_https(headers) {
        "https"
    } else {
        "http"
    };
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

/// `GET /_zcast/auth/magic` — scanner-safe confirmation page.
/// Deliberately only *peeks* at the token so email security scanners that
/// pre-fetch `GET` URLs can never burn a real recipient's one-time link.
async fn magic_confirm_page(
    State(kernel): State<Arc<ZcastKernel>>,
    Query(query): Query<MagicQuery>,
) -> Response {
    let Some(auth) = &kernel.auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !auth.supports_magic_link() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let next = sanitize_redirect_path(&query.next);
    let valid = auth.peek_magic_token(&query.token).await.unwrap_or(false);
    let settings = kernel.app_settings();
    let html = auth.render_magic_confirm_page(
        &settings.site_title,
        settings.color_mode,
        &query.token,
        valid,
        next,
    );
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        AxumHtml(html.into_string()),
    )
        .into_response()
}

/// `POST /_zcast/auth/magic` — consume the one-time token and start a session.
async fn magic_confirm_submit(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Form(form): Form<MagicForm>,
) -> Response {
    let Some(auth) = &kernel.auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !auth.supports_magic_link() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let next = sanitize_redirect_path(&form.next);
    match auth.consume_magic_token(&form.token).await {
        Ok(Some(token)) => {
            let cookie = session_cookie(auth.as_ref(), &token, &headers);
            let mut resp = Redirect::to(next).into_response();
            if let Ok(val) = HeaderValue::from_str(&cookie) {
                resp.headers_mut().insert(header::SET_COOKIE, val);
            }
            resp
        }
        Ok(None) => expired_magic_link(&kernel, auth.as_ref(), &form.token, next),
        Err(error) => {
            tracing::warn!(%error, "magic login link could not be consumed");
            expired_magic_link(&kernel, auth.as_ref(), &form.token, next)
        }
    }
}

fn expired_magic_link(
    kernel: &ZcastKernel,
    auth: &dyn zcast_interfaces::AuthProvider,
    token: &str,
    next: &str,
) -> Response {
    let settings = kernel.app_settings();
    let html = auth.render_magic_confirm_page(
        &settings.site_title,
        settings.color_mode,
        token,
        false,
        next,
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        AxumHtml(html.into_string()),
    )
        .into_response()
}

/// `GET /_zcast/admin` — live settings + module dependency graph dashboard.
async fn admin_page(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return admin_denied(&kernel);
    }

    let notice = match query.saved.as_deref() {
        Some("1") => Some("Settings saved."),
        _ => None,
    };
    let error = query.error.as_deref();
    let html = crate::admin::render_admin_page(&kernel, notice, error);
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        AxumHtml(html),
    )
        .into_response()
}

/// Admin pages redirect to the login form when auth is active, else forbid.
fn admin_denied(kernel: &ZcastKernel) -> Response {
    if kernel.has_auth() {
        Redirect::to("/_zcast/login?next=/_zcast/admin").into_response()
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

/// `POST /_zcast/admin/settings` — persist live settings and redirect back.
async fn admin_update_settings(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Form(form): Form<AdminSettingsForm>,
) -> Response {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return StatusCode::FORBIDDEN.into_response();
    }

    let next = AppSettings {
        site_title: form.site_title.trim().to_owned(),
        pinned_home_slug: form.pinned_home_slug.trim().to_owned(),
        color_mode: ThemeColorMode::parse(&form.color_mode),
    };

    match kernel.update_app_settings(next).await {
        Ok(_) => Redirect::to("/_zcast/admin?saved=1").into_response(),
        Err(error) => {
            let html = crate::admin::render_admin_page(&kernel, None, Some(&error.to_string()));
            (
                StatusCode::BAD_REQUEST,
                [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
                AxumHtml(html),
            )
                .into_response()
        }
    }
}

/// `GET /_zcast/api/export.zip` — authenticated Markdown backup archive.
async fn export_zip_api(State(kernel): State<Arc<ZcastKernel>>, headers: HeaderMap) -> Response {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(exporter) = &kernel.exporter else {
        return StatusCode::NOT_FOUND.into_response();
    };

    match exporter
        .export_to_zip_bytes(kernel.storage.as_ref(), ReadContext::ADMIN)
        .await
    {
        Ok((bytes, summary)) => {
            tracing::info!(
                exported = summary.exported,
                skipped_private = summary.skipped_private,
                "markdown backup exported"
            );
            let mut resp = Response::new(axum::body::Body::from(bytes));
            *resp.status_mut() = StatusCode::OK;
            let response_headers = resp.headers_mut();
            response_headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            );
            response_headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"zcast-notes.zip\""),
            );
            response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(error) => {
            tracing::error!(%error, "markdown backup export failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn logout_submit(State(kernel): State<Arc<ZcastKernel>>, headers: HeaderMap) -> Response {
    let Some(auth) = &kernel.auth else {
        return Redirect::to("/").into_response();
    };
    if let Some(token) = extract_cookie(&headers, auth.session_cookie_name()) {
        let _ = auth.revoke_session(&token).await;
    }
    let secure = if request_is_https(&headers) {
        "; Secure"
    } else {
        ""
    };
    let clear = format!(
        "{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{}",
        auth.session_cookie_name(),
        secure
    );
    let mut resp = Redirect::to("/").into_response();
    if let Ok(val) = HeaderValue::from_str(&clear) {
        resp.headers_mut().insert(header::SET_COOKIE, val);
    }
    resp
}

async fn get_popover(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let is_admin = resolve_is_admin(&kernel, &headers, query.preview.as_deref()).await;
    let ctx = ReadContext {
        include_private: is_admin,
    };

    let found = kernel
        .storage
        .get_note_by_slug_or_alias(&slug, ctx)
        .await
        .map_err(KernelError::from)?
        .ok_or(ApiError(KernelError::NotFound(slug)))?;

    let card = kernel.build_card_view(&found.note, ctx).await?;
    let excerpt_html = build_popover_excerpt_html(&card.rendered_html, 280);

    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(PopoverResponse {
            slug: card.slug.clone(),
            canonical_slug: card.slug,
            title: card.title,
            excerpt_html,
            backlink_count: card.backlinks.len(),
            updated_at: card.updated_at,
        }),
    ))
}

async fn search_notes(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(search) = &kernel.search else {
        return Err(ApiError(KernelError::NotFound(
            "no search provider is bound".into(),
        )));
    };

    let is_admin = resolve_is_admin(&kernel, &headers, params.preview.as_deref()).await;
    let ctx = ReadContext {
        include_private: is_admin,
    };

    let text = params.q.unwrap_or_default();
    let limit = params.limit.unwrap_or(12).min(50);
    let context_note_id = params
        .context
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(NoteId::from);

    let query = SearchQuery {
        text: text.clone(),
        limit,
        context_note_id,
    };
    let hits = search
        .search(&query, ctx)
        .await
        .map_err(KernelError::from)?;

    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(SearchResponse {
            query: text,
            capabilities: search.capabilities(),
            hits,
        }),
    ))
}

/// Produce a clean, tag-balanced HTML excerpt from already-sanitized card HTML.
fn build_popover_excerpt_html(rendered_html: &str, max_chars: usize) -> String {
    if rendered_html.is_empty() || max_chars == 0 {
        return String::new();
    }

    let mut out = String::with_capacity(rendered_html.len().min(max_chars + 64));
    let mut stack: Vec<String> = Vec::new();
    let mut skip_heading: Option<String> = None;
    let mut visible = 0usize;
    let mut truncated = false;
    let bytes = rendered_html.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            let Some(end_rel) = rendered_html[i..].find('>') else {
                break;
            };
            let end = i + end_rel;
            let raw = &rendered_html[i..=end];
            let inner = rendered_html[i + 1..end].trim();

            if let Some(closing) = inner.strip_prefix('/') {
                let name = closing
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if skip_heading.as_deref() == Some(name.as_str()) {
                    skip_heading = None;
                } else if skip_heading.is_none()
                    && let Some(pos) = stack.iter().rposition(|open| open == &name)
                {
                    while stack.len() > pos {
                        let open = stack.pop().unwrap();
                        out.push_str("</");
                        out.push_str(&open);
                        out.push('>');
                    }
                }
            } else if !inner.starts_with('!') && !inner.starts_with('?') {
                let self_closing = inner.ends_with('/');
                let name = inner
                    .trim_end_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(name.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                    skip_heading = Some(name);
                } else if skip_heading.is_none() && !name.is_empty() {
                    out.push_str(raw);
                    let is_void = matches!(
                        name.as_str(),
                        "br" | "hr"
                            | "img"
                            | "input"
                            | "meta"
                            | "link"
                            | "area"
                            | "base"
                            | "col"
                            | "embed"
                            | "param"
                            | "source"
                            | "track"
                            | "wbr"
                    );
                    if !self_closing && !is_void {
                        stack.push(name);
                    }
                }
            }

            i = end + 1;
            continue;
        }

        if skip_heading.is_some() {
            i += rendered_html[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }

        if visible >= max_chars {
            truncated = true;
            break;
        }

        // Preserve HTML entities (`&amp;`, `&#39;`, etc.) as single visible characters.
        if bytes[i] == b'&'
            && let Some(semi_rel) = rendered_html[i..].find(';')
            && semi_rel > 1
            && semi_rel <= 10
            && rendered_html[i + 1..i + semi_rel]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '#')
        {
            out.push_str(&rendered_html[i..=i + semi_rel]);
            visible += 1;
            i += semi_rel + 1;
            continue;
        }

        let ch = rendered_html[i..].chars().next().unwrap();
        out.push(ch);
        visible += 1;
        i += ch.len_utf8();
    }

    if truncated {
        let trimmed_len = out.trim_end().len();
        out.truncate(trimmed_len);
        out.push('…');
    }

    while let Some(open) = stack.pop() {
        out.push_str("</");
        out.push_str(&open);
        out.push('>');
    }

    out
}

async fn custom_css(State(kernel): State<Arc<ZcastKernel>>) -> Response {
    if let Some(path) = &kernel.config.custom_css_path
        && let Ok(content) = tokio::fs::read(path).await
    {
        return ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], content).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn home_page(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    let slug = kernel.app_settings().pinned_home_slug;
    render_note_stack(kernel, slug, headers, query, true).await
}

async fn note_page(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    render_note_stack(kernel, slug, headers, query, false).await
}

async fn render_note_stack(
    kernel: Arc<ZcastKernel>,
    root_slug: String,
    headers: HeaderMap,
    query: PageQuery,
    is_home: bool,
) -> Response {
    let is_admin = resolve_is_admin(&kernel, &headers, query.preview.as_deref()).await;
    let ctx = ReadContext {
        include_private: is_admin,
    };
    let settings = kernel.app_settings();

    let lookup = match kernel
        .storage
        .get_note_by_slug_or_alias(&root_slug, ctx)
        .await
    {
        Ok(Some(found)) => found,
        Ok(None) => {
            let html = kernel.theme.render_error(
                404,
                "Note not found",
                &format!("No note exists at '/{root_slug}'."),
                &settings.site_title,
                settings.color_mode,
            );
            return (StatusCode::NOT_FOUND, AxumHtml(html.into_string())).into_response();
        }
        Err(e) => {
            let html = kernel.theme.render_error(
                500,
                "Server error",
                &e.to_string(),
                &settings.site_title,
                settings.color_mode,
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                AxumHtml(html.into_string()),
            )
                .into_response();
        }
    };

    // DEC-02: Permanent redirect from historical alias slug to canonical slug.
    if lookup.matched_alias && !is_home {
        let mut target = format!("/{}", lookup.note.slug);
        if let Some(stack) = &query.stack
            && !stack.is_empty()
        {
            target.push_str("?stack=");
            target.push_str(stack);
        }
        return Redirect::permanent(&target).into_response();
    }

    let mut cards = Vec::new();
    let mut breadcrumbs = Vec::new();
    let mut seen_ids = Vec::new();

    match kernel.build_card_view(&lookup.note, ctx).await {
        Ok(card) => {
            seen_ids.push(card.id.clone());
            breadcrumbs.push(BreadcrumbItem {
                slug: card.slug.clone(),
                title: card.title.clone(),
            });
            cards.push(card);
        }
        Err(e) => {
            let html = kernel.theme.render_error(
                500,
                "Render error",
                &e.to_string(),
                &settings.site_title,
                settings.color_mode,
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                AxumHtml(html.into_string()),
            )
                .into_response();
        }
    }

    if let Some(stack_raw) = &query.stack {
        for raw_slug in stack_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Ok(Some(extra)) = kernel
                .storage
                .get_note_by_slug_or_alias(raw_slug, ctx)
                .await
            {
                if seen_ids.contains(&extra.note.id) {
                    continue;
                }
                if let Ok(card) = kernel.build_card_view(&extra.note, ctx).await {
                    seen_ids.push(card.id.clone());
                    breadcrumbs.push(BreadcrumbItem {
                        slug: card.slug.clone(),
                        title: card.title.clone(),
                    });
                    cards.push(card);
                }
            }
        }
    }

    let mut slots = kernel.page_ui_slots(is_admin);
    if is_admin && let (Some(editor), Some(first)) = (&kernel.editor, cards.first()) {
        slots.card_toolbar = editor.card_toolbar_slot(first).into_string();
    }

    let page_ctx = PageContext {
        site_title: settings.site_title,
        color_mode: settings.color_mode,
        is_admin,
        breadcrumbs,
        cards,
        slots,
        custom_css_enabled: kernel.custom_css_enabled(),
    };

    AxumHtml(kernel.theme.render_page(&page_ctx).into_string()).into_response()
}

async fn get_card_fragment(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let is_admin = resolve_is_admin(&kernel, &headers, query.preview.as_deref()).await;
    let ctx = ReadContext {
        include_private: is_admin,
    };

    let found = kernel
        .storage
        .get_note_by_slug_or_alias(&slug, ctx)
        .await
        .map_err(KernelError::from)?
        .ok_or(ApiError(KernelError::NotFound(slug)))?;

    let card = kernel.build_card_view(&found.note, ctx).await?;
    let html = kernel.render_card_html(&card, is_admin);
    let canonical_slug = card.slug.clone();

    // Card fragments are hot-swapped by the inline editor after every save, so
    // browsers must never serve them from cache: a cached fragment would show
    // stale backlinks or rendered body HTML until a hard refresh.
    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(CardFragmentResponse {
            card,
            html,
            canonical_slug,
        }),
    ))
}

async fn list_notes_api(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Query(query): Query<NotesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return Err(ApiError(KernelError::Forbidden));
    }

    let notes = kernel
        .storage
        .list_notes(
            NoteListFilter {
                sort: NoteSort::TitleAsc,
                ..Default::default()
            },
            ReadContext::ADMIN,
        )
        .await
        .map_err(KernelError::from)?;

    let q_lower = query
        .q
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();

    let results: Vec<NoteSummary> = notes
        .into_iter()
        .filter(|n| {
            q_lower.is_empty()
                || n.title.to_ascii_lowercase().contains(&q_lower)
                || n.slug.contains(&q_lower)
        })
        .take(25)
        .map(|n| NoteSummary {
            id: n.id,
            slug: n.slug,
            title: n.title,
            private: n.private,
        })
        .collect();

    Ok(Json(results))
}

async fn get_note_api(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return Err(ApiError(KernelError::Forbidden));
    }

    let note = kernel
        .storage
        .get_note_by_id(&NoteId(id.clone()), ReadContext::ADMIN)
        .await
        .map_err(KernelError::from)?
        .ok_or(ApiError(KernelError::NotFound(id)))?;

    Ok(Json(note))
}

async fn create_note_api(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Json(payload): Json<NotePayload>,
) -> Result<impl IntoResponse, ApiError> {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return Err(ApiError(KernelError::Forbidden));
    }

    let res = kernel
        .create_note(payload.title, payload.markdown_body, payload.private)
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(SaveResponse {
            note: res.note,
            rendered_html: res.rendered_html,
            card_html: res.card_html,
            renamed_from_slug: res.renamed_from_slug,
        }),
    ))
}

async fn update_note_api(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<NotePayload>,
) -> Result<impl IntoResponse, ApiError> {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return Err(ApiError(KernelError::Forbidden));
    }

    let res = kernel
        .update_note(
            &NoteId(id),
            payload.title,
            payload.markdown_body,
            payload.private,
        )
        .await?;

    Ok(Json(SaveResponse {
        note: res.note,
        rendered_html: res.rendered_html,
        card_html: res.card_html,
        renamed_from_slug: res.renamed_from_slug,
    }))
}

async fn delete_note_api(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if !resolve_is_admin(&kernel, &headers, None).await {
        return Err(ApiError(KernelError::Forbidden));
    }

    kernel.delete_note(&NoteId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

struct ApiError(KernelError);

impl From<KernelError> for ApiError {
    fn from(value: KernelError) -> Self {
        Self(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            KernelError::NotFound(_) => StatusCode::NOT_FOUND,
            KernelError::BadRequest(_) => StatusCode::BAD_REQUEST,
            KernelError::Forbidden => StatusCode::FORBIDDEN,
            KernelError::Storage(
                StorageError::SlugConflict(_) | StorageError::TitleConflict(_),
            ) => StatusCode::CONFLICT,
            KernelError::Storage(StorageError::NotFound(_)) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(serde_json::json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}
