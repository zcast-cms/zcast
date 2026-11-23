use crate::error::KernelError;
use crate::kernel::ZcastKernel;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html as AxumHtml, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use zcast_interfaces::{
    Asset, BreadcrumbItem, CardView, NoteId, NoteListFilter, NoteSort, PageContext, ReadContext,
    StorageError,
};

pub fn router(kernel: Arc<ZcastKernel>) -> Router {
    Router::new()
        .route("/_zcast/health", get(health))
        .route("/_zcast/modules", get(list_modules))
        .route("/_zcast/theme/{*path}", get(theme_asset))
        .route("/_zcast/editor/{*path}", get(editor_asset))
        .route("/_zcast/custom.css", get(custom_css))
        .route("/_zcast/cards/{slug}", get(get_card_fragment))
        .route("/_zcast/api/notes", get(list_notes_api).post(create_note_api))
        .route(
            "/_zcast/api/notes/{id}",
            get(get_note_api).put(update_note_api).delete(delete_note_api),
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

async fn theme_asset(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(path): Path<String>,
) -> Response {
    asset_response(kernel.theme.get_asset(&path))
}

async fn editor_asset(
    State(kernel): State<Arc<ZcastKernel>>,
    Path(path): Path<String>,
) -> Response {
    let asset = kernel.editor.as_ref().and_then(|e| e.get_asset(&path));
    asset_response(asset)
}

async fn custom_css(State(kernel): State<Arc<ZcastKernel>>) -> Response {
    if let Some(path) = &kernel.config.custom_css_path
        && let Ok(content) = tokio::fs::read(path).await
    {
        return (
            [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
            content,
        )
            .into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn home_page(
    State(kernel): State<Arc<ZcastKernel>>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    let slug = kernel.config.pinned_home_slug.clone();
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
                &kernel.config.site_title,
                kernel.config.color_mode,
            );
            return (StatusCode::NOT_FOUND, AxumHtml(html.into_string())).into_response();
        }
        Err(e) => {
            let html = kernel.theme.render_error(
                500,
                "Server error",
                &e.to_string(),
                &kernel.config.site_title,
                kernel.config.color_mode,
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, AxumHtml(html.into_string()))
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
                &kernel.config.site_title,
                kernel.config.color_mode,
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, AxumHtml(html.into_string()))
                .into_response();
        }
    }

    if let Some(stack_raw) = &query.stack {
        for raw_slug in stack_raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
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
    if is_admin
        && let (Some(editor), Some(first)) = (&kernel.editor, cards.first())
    {
        slots.card_toolbar = editor.card_toolbar_slot(first).into_string();
    }

    let page_ctx = PageContext {
        site_title: kernel.config.site_title.clone(),
        color_mode: kernel.config.color_mode,
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
            KernelError::Storage(StorageError::SlugConflict(_) | StorageError::TitleConflict(_)) => {
                StatusCode::CONFLICT
            }
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
