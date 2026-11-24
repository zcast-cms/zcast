use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use std::sync::Arc;
use tower::ServiceExt;
use zcast_auth_basic::BasicAuthProvider;
use zcast_auth_magic_link::MagicLinkAuthProvider;
use zcast_cmd_k::CommandBar;
use zcast_editor_inline::InlineEditor;
use zcast_interfaces::{
    AuthProvider, IFACE_SEARCH, Note, NoteImporter, NoteListFilter, ReadContext, Storage,
    ThemeColorMode,
};
use zcast_io_markdown::MarkdownIo;
use zcast_kernel::{KernelConfig, KernelError, ZcastKernelBuilder};
use zcast_links_wikilink::WikilinkParser;
use zcast_popover::HoverPopover;
use zcast_search_fts::FtsSearchProvider;
use zcast_search_onnx::OnnxSearchProvider;
use zcast_storage_sqlite::SqliteStorage;
use zcast_theme_davidhewitson::DavidHewitsonTheme;

async fn test_kernel(color_mode: ThemeColorMode) -> Arc<zcast_kernel::ZcastKernel> {
    let storage = Arc::new(SqliteStorage::connect("sqlite::memory:").await.unwrap());
    let links = Arc::new(WikilinkParser::new());
    let theme = Arc::new(DavidHewitsonTheme::new());
    let editor = Arc::new(InlineEditor::new());

    let config = KernelConfig {
        site_title: "David's Notes".into(),
        color_mode,
        ..Default::default()
    };

    ZcastKernelBuilder::new(config, storage, links, theme)
        .with_editor(editor)
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn home_page_renders_seeded_index_with_dark_lock_and_slots() {
    let kernel = test_kernel(ThemeColorMode::Dark).await;
    let app = zcast_kernel::http::router(kernel);

    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    assert!(html.contains(r#"data-theme="dark""#));
    assert!(html.contains("David's Notes"));
    assert!(html.contains("How Sliding Panes Work"));
    assert!(html.contains(r#"data-zcast-note="how-sliding-panes-work""#));
    // Admin local mode exposes unresolved link trigger + editor bundle
    assert!(html.contains(r#"data-zcast-unresolved="My First Note""#));
    assert!(html.contains("/_zcast/editor/editor.bundle.js"));
}

#[tokio::test]
async fn public_preview_hides_private_notes_and_unresolved_links() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;
    let app = zcast_kernel::http::router(kernel.clone());

    // Public preview of Index must NOT render clickable unresolved link or editor script
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/?preview=public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!html.contains("data-zcast-unresolved"));
    assert!(!html.contains("/_zcast/editor/editor.bundle.js"));
    assert!(html.contains("My First Note"));

    // Private Scratchpad must return 404 in public preview
    let priv_resp = app
        .oneshot(
            Request::builder()
                .uri("/private-scratchpad?preview=public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(priv_resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn creating_target_note_invalidates_cache_and_resolves_link() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;

    // Warm public render cache for Index before "My First Note" exists
    let index = kernel
        .storage
        .get_note_by_slug_or_alias("index", ReadContext::PUBLIC)
        .await
        .unwrap()
        .unwrap()
        .note;
    let before = kernel
        .build_card_view(&index, ReadContext::PUBLIC)
        .await
        .unwrap();
    assert!(
        !before
            .rendered_html
            .contains(r#"data-zcast-note="my-first-note""#)
    );

    // Create "My First Note"
    kernel
        .create_note(
            "My First Note".into(),
            "Now I exist and link back to [[Index]].".into(),
            false,
        )
        .await
        .unwrap();

    // Cache should have invalidated and Index now renders a real link
    let after = kernel
        .build_card_view(&index, ReadContext::PUBLIC)
        .await
        .unwrap();
    assert!(
        after
            .rendered_html
            .contains(r#"data-zcast-note="my-first-note""#)
    );
}

#[tokio::test]
async fn renaming_note_rewrites_citing_markdown_and_redirects_old_slug() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;

    let panes = kernel
        .storage
        .get_note_by_slug_or_alias("how-sliding-panes-work", ReadContext::ADMIN)
        .await
        .unwrap()
        .unwrap()
        .note;

    let res = kernel
        .update_note(
            &panes.id,
            "Sliding Panes Guide".into(),
            panes.markdown_body.clone(),
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        res.renamed_from_slug.as_deref(),
        Some("how-sliding-panes-work")
    );
    assert_eq!(res.note.slug, "sliding-panes-guide");

    // Index's markdown should now contain [[Sliding Panes Guide]]
    let index = kernel
        .storage
        .get_note_by_slug_or_alias("index", ReadContext::ADMIN)
        .await
        .unwrap()
        .unwrap()
        .note;
    assert!(index.markdown_body.contains("[[Sliding Panes Guide]]"));

    // Requesting old slug via HTTP should 301 redirect to new slug
    let app = zcast_kernel::http::router(kernel);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/how-sliding-panes-work?stack=index")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "/sliding-panes-guide?stack=index"
    );
}

#[tokio::test]
async fn card_fragment_and_assets_are_served() {
    let kernel = test_kernel(ThemeColorMode::Dark).await;
    let app = zcast_kernel::http::router(kernel);

    let card_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/cards/how-sliding-panes-work")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(card_resp.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(card_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(json["canonical_slug"], "how-sliding-panes-work");
    assert!(json["html"].as_str().unwrap().contains("data-zcast-card"));

    let css_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/theme/theme.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(css_resp.status(), StatusCode::OK);

    let ed_resp = app
        .oneshot(
            Request::builder()
                .uri("/_zcast/editor/editor.bundle.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ed_resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_create_and_list_notes_for_autocomplete() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;
    let app = zcast_kernel::http::router(kernel);

    let create_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/_zcast/api/notes")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "title": "Theatre Dramaturgy",
                        "markdown_body": "Notes on <u>structure</u> and [[Index]].",
                        "private": false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);

    let list_resp = app
        .oneshot(
            Request::builder()
                .uri("/_zcast/api/notes?q=dramaturgy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let arr: Vec<serde_json::Value> = serde_json::from_slice(
        &axum::body::to_bytes(list_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "Theatre Dramaturgy");

    let _ = Note::new("unused", "", false);
}

async fn link_edges(
    kernel: &zcast_kernel::ZcastKernel,
    note_id: &zcast_interfaces::NoteId,
) -> Vec<serde_json::Value> {
    let store = kernel.storage.module_store("zcast-links-wikilink");
    let value = store
        .get(&format!("out:{}", note_id.as_str()))
        .await
        .expect("read edges");
    value
        .map(|v| serde_json::from_value(v).expect("edges json"))
        .unwrap_or_default()
}

#[tokio::test]
async fn slug_based_wikilinks_appear_in_backlinks_and_rewrite_on_rename() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;

    let panes = kernel
        .storage
        .get_note_by_slug_or_alias("how-sliding-panes-work", ReadContext::ADMIN)
        .await
        .unwrap()
        .unwrap()
        .note;

    let referrer = kernel
        .create_note(
            "Slug Referrer".into(),
            "This note links via [[how-sliding-panes-work]].".into(),
            false,
        )
        .await
        .unwrap()
        .note;

    let backlinks = kernel
        .links
        .get_backlinks(&panes.id, kernel.storage.as_ref(), ReadContext::ADMIN)
        .await
        .unwrap();
    assert!(
        backlinks.iter().any(|b| b.note_id == referrer.id),
        "slug link must appear in backlinks: {backlinks:?}"
    );

    kernel
        .update_note(
            &panes.id,
            "Sliding Panes Guide".into(),
            panes.markdown_body.clone(),
            false,
        )
        .await
        .unwrap();

    let referrer = kernel
        .storage
        .get_note_by_id(&referrer.id, ReadContext::ADMIN)
        .await
        .unwrap()
        .expect("referrer present");
    assert_eq!(
        referrer.markdown_body,
        "This note links via [[Sliding Panes Guide]]."
    );

    let backlinks = kernel
        .links
        .get_backlinks(&panes.id, kernel.storage.as_ref(), ReadContext::ADMIN)
        .await
        .unwrap();
    assert!(
        backlinks.iter().any(|b| b.note_id == referrer.id),
        "rewritten link must still resolve: {backlinks:?}"
    );
}

#[tokio::test]
async fn failed_rename_leaves_link_edges_untouched() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;

    let alpha = kernel
        .create_note("Alpha".into(), "root".into(), false)
        .await
        .unwrap()
        .note;
    let beta = kernel
        .create_note("Beta".into(), "See [[Alpha]] for details.".into(), false)
        .await
        .unwrap()
        .note;
    kernel
        .create_note("Taken Title".into(), "occupied".into(), false)
        .await
        .unwrap();

    // Renaming Alpha onto an existing title must fail. Before the atomic fix this
    // still synced Beta's edges to "Taken Title", losing the real backlink.
    let result = kernel
        .update_note(&alpha.id, "Taken Title".into(), "root".into(), false)
        .await;
    assert!(result.is_err(), "conflicting rename must fail");

    let stored = kernel
        .storage
        .get_note_by_id(&alpha.id, ReadContext::ADMIN)
        .await
        .unwrap()
        .expect("alpha present");
    assert_eq!(stored.title, "Alpha");

    let edges = link_edges(&kernel, &beta.id).await;
    assert!(
        edges.iter().any(|link| link["target_title"] == "Alpha"),
        "edges must still point at the old title: {edges:?}"
    );
    assert!(
        edges
            .iter()
            .all(|link| link["target_title"] != "Taken Title"),
        "failed rename must not rewrite edges: {edges:?}"
    );

    let backlinks = kernel
        .links
        .get_backlinks(&alpha.id, kernel.storage.as_ref(), ReadContext::ADMIN)
        .await
        .unwrap();
    assert_eq!(backlinks.len(), 1, "{backlinks:?}");
    assert_eq!(backlinks[0].note_id, beta.id);
}

#[tokio::test]
async fn renaming_rewrites_self_links_in_the_renamed_note() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;

    let old = kernel
        .create_note(
            "Old Title".into(),
            "I refer to myself as [[Old Title]] often.".into(),
            false,
        )
        .await
        .unwrap()
        .note;

    let res = kernel
        .update_note(
            &old.id,
            "New Title".into(),
            "I refer to myself as [[Old Title]] often.".into(),
            false,
        )
        .await
        .unwrap();

    assert_eq!(res.renamed_from_slug.as_deref(), Some("old-title"));
    assert_eq!(res.note.title, "New Title");
    assert_eq!(
        res.note.markdown_body,
        "I refer to myself as [[New Title]] often."
    );

    let stored = kernel
        .storage
        .get_note_by_id(&old.id, ReadContext::ADMIN)
        .await
        .unwrap()
        .expect("note present");
    assert_eq!(
        stored.markdown_body,
        "I refer to myself as [[New Title]] often."
    );

    let edges = link_edges(&kernel, &old.id).await;
    assert!(
        edges.iter().any(|link| link["target_title"] == "New Title"),
        "self edge must point at the new title: {edges:?}"
    );
    assert!(
        edges.iter().all(|link| link["target_title"] != "Old Title"),
        "stale self edge must be gone: {edges:?}"
    );
}

/* ------------------------------------------------------------------------- */
/* Real-time card fragments (issue #11 / v0.1.2 polish #4)                   */
/* ------------------------------------------------------------------------- */

fn json_request(method: &str, uri: &str, payload: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = app.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

async fn card_fragment(app: &axum::Router, slug: &str) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/_zcast/cards/{slug}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "fragment for {slug}");
    serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}

fn backlink_slugs(fragment: &serde_json::Value) -> Vec<String> {
    fragment["card"]["backlinks"]
        .as_array()
        .expect("card.backlinks array")
        .iter()
        .filter_map(|b| b["slug"].as_str().map(str::to_owned))
        .collect()
}

fn has_backlink(fragment: &serde_json::Value, slug: &str) -> bool {
    backlink_slugs(fragment).iter().any(|s| s == slug)
}

#[tokio::test]
async fn card_fragment_is_served_with_no_store_cache_control() {
    let kernel = test_kernel(ThemeColorMode::Dark).await;
    let app = zcast_kernel::http::router(kernel);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/_zcast/cards/how-sliding-panes-work")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "card fragments must never be cacheable"
    );
}

#[tokio::test]
async fn adding_and_removing_wikilink_updates_card_fragment_backlinks_immediately() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;
    let app = zcast_kernel::http::router(kernel);

    const TARGET: &str = "how-sliding-panes-work";
    const REFERRER: &str = "realtime-referrer";

    // POST creates a note that already cites the target: the target fragment
    // must show it as a backlink straight away (no re-render/restart).
    let (status, created) = send(
        &app,
        json_request(
            "POST",
            "/_zcast/api/notes",
            serde_json::json!({
                "title": "Realtime Referrer",
                "markdown_body": "This note cites [[How Sliding Panes Work]].",
                "private": false
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let referrer_id = created["note"]["id"].as_str().unwrap().to_owned();

    let after_create = card_fragment(&app, TARGET).await;
    assert!(
        has_backlink(&after_create, REFERRER),
        "POST with [[link]] must appear in card.backlinks: {after_create}"
    );
    let html = after_create["html"].as_str().unwrap();
    assert!(html.contains("Linked from"), "{html}");
    assert!(
        html.contains(r#"data-zcast-note="realtime-referrer""#),
        "POST with [[link]] must render a Linked from entry: {html}"
    );

    // PUT removes the link: the backlink must disappear immediately.
    let (status, _) = send(
        &app,
        json_request(
            "PUT",
            &format!("/_zcast/api/notes/{referrer_id}"),
            serde_json::json!({
                "title": "Realtime Referrer",
                "markdown_body": "The link has been removed.",
                "private": false
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let after_remove = card_fragment(&app, TARGET).await;
    assert!(
        !has_backlink(&after_remove, REFERRER),
        "removed [[link]] must vanish from card.backlinks: {after_remove}"
    );
    let html = after_remove["html"].as_str().unwrap();
    assert!(
        !html.contains(r#"data-zcast-note="realtime-referrer""#),
        "removed [[link]] must vanish from rendered Linked from HTML: {html}"
    );

    // PUT adds the link back: it must reappear immediately.
    let (status, _) = send(
        &app,
        json_request(
            "PUT",
            &format!("/_zcast/api/notes/{referrer_id}"),
            serde_json::json!({
                "title": "Realtime Referrer",
                "markdown_body": "Now it cites [[How Sliding Panes Work]] again.",
                "private": false
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let after_readd = card_fragment(&app, TARGET).await;
    assert!(
        has_backlink(&after_readd, REFERRER),
        "re-added [[link]] must reappear in card.backlinks: {after_readd}"
    );
    let html = after_readd["html"].as_str().unwrap();
    assert!(
        html.contains(r#"data-zcast-note="realtime-referrer""#),
        "re-added [[link]] must reappear in rendered Linked from HTML: {html}"
    );
}

#[tokio::test]
async fn renaming_note_updates_citing_card_fragment_immediately() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;

    let panes = kernel
        .storage
        .get_note_by_slug_or_alias("how-sliding-panes-work", ReadContext::ADMIN)
        .await
        .unwrap()
        .unwrap()
        .note;
    let app = zcast_kernel::http::router(kernel);

    let (status, saved) = send(
        &app,
        json_request(
            "PUT",
            &format!("/_zcast/api/notes/{}", panes.id.as_str()),
            serde_json::json!({
                "title": "Panes Deep Dive",
                "markdown_body": panes.markdown_body,
                "private": false
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["renamed_from_slug"], "how-sliding-panes-work");
    assert_eq!(saved["note"]["slug"], "panes-deep-dive");

    // Index cites the renamed note; its fragment must be rewritten right away.
    let citing = card_fragment(&app, "index").await;
    let citing_html = citing["html"].as_str().unwrap();
    let citing_rendered = citing["card"]["rendered_html"].as_str().unwrap();
    for html in [citing_html, citing_rendered] {
        assert!(
            html.contains("Panes Deep Dive"),
            "citing card must show the new title text: {html}"
        );
        assert!(
            html.contains(r#"data-zcast-note="panes-deep-dive""#),
            "citing card must point at the new slug: {html}"
        );
        assert!(
            !html.contains("How Sliding Panes Work"),
            "citing card must not keep the old title: {html}"
        );
    }

    // The renamed note still records the citing note in its backlinks.
    let renamed = card_fragment(&app, "panes-deep-dive").await;
    assert_eq!(renamed["canonical_slug"], "panes-deep-dive");
    assert!(
        has_backlink(&renamed, "index"),
        "renamed card must still list its citing note: {renamed}"
    );
    let renamed_html = renamed["html"].as_str().unwrap();
    assert!(renamed_html.contains("Linked from"), "{renamed_html}");
    assert!(
        renamed_html.contains(r#"data-zcast-note="index""#),
        "{renamed_html}"
    );

    // Historical aliases keep resolving, but report the canonical slug so
    // editors can hot-swap the correct pane.
    let alias = card_fragment(&app, "how-sliding-panes-work").await;
    assert_eq!(alias["canonical_slug"], "panes-deep-dive");
}

#[tokio::test]
async fn deleting_citing_note_removes_it_from_backlinks_immediately() {
    let kernel = test_kernel(ThemeColorMode::Auto).await;
    let app = zcast_kernel::http::router(kernel);

    const TARGET: &str = "how-sliding-panes-work";
    const REFERRER: &str = "doomed-referrer";

    let (status, created) = send(
        &app,
        json_request(
            "POST",
            "/_zcast/api/notes",
            serde_json::json!({
                "title": "Doomed Referrer",
                "markdown_body": "This note will be deleted but cites [[How Sliding Panes Work]].",
                "private": false
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let referrer_id = created["note"]["id"].as_str().unwrap().to_owned();

    assert!(
        has_backlink(&card_fragment(&app, TARGET).await, REFERRER),
        "referrer must be visible before deletion"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/_zcast/api/notes/{referrer_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let after_delete = card_fragment(&app, TARGET).await;
    assert!(
        !has_backlink(&after_delete, REFERRER),
        "deleted note must vanish from card.backlinks: {after_delete}"
    );
    let html = after_delete["html"].as_str().unwrap();
    assert!(
        !html.contains(r#"data-zcast-note="doomed-referrer""#),
        "deleted note must vanish from rendered Linked from HTML: {html}"
    );
}

/* ========================================================================== */
/* v0.2 Module Integration Tests (Auth Basic, Popover, Search FTS, Cmd+K)     */
/* ========================================================================== */

async fn full_v02_kernel(
    with_auth: bool,
) -> (
    Arc<zcast_kernel::ZcastKernel>,
    Option<Arc<BasicAuthProvider>>,
) {
    let storage = Arc::new(SqliteStorage::connect("sqlite::memory:").await.unwrap());
    let links = Arc::new(WikilinkParser::new());
    let theme = Arc::new(DavidHewitsonTheme::new());
    let editor = Arc::new(InlineEditor::new());
    let search = Arc::new(FtsSearchProvider::new());
    let popover = Arc::new(HoverPopover::new());
    let cmd_k = Arc::new(CommandBar::new());

    let config = KernelConfig {
        site_title: "David's Notes".into(),
        color_mode: ThemeColorMode::Dark,
        ..Default::default()
    };

    let mut builder = ZcastKernelBuilder::new(config, storage.clone(), links, theme)
        .with_editor(editor)
        .with_search(search)
        .with_ui_extension(popover)
        .with_ui_extension(cmd_k);

    let auth = if with_auth {
        let provider = Arc::new(BasicAuthProvider::new(storage));
        builder = builder.with_auth(provider.clone());
        Some(provider)
    } else {
        None
    };

    let kernel = builder.build().await.unwrap();
    if let Some(provider) = &auth {
        provider
            .ensure_admin_credentials("admin", Some("s3cr3t-pass"))
            .await
            .unwrap();
    }
    (kernel, auth)
}

#[tokio::test]
async fn cmd_k_requires_search_provider_at_kernel_build() {
    let storage = Arc::new(SqliteStorage::connect("sqlite::memory:").await.unwrap());
    let links = Arc::new(WikilinkParser::new());
    let theme = Arc::new(DavidHewitsonTheme::new());
    let cmd_k = Arc::new(CommandBar::new());

    let err = match ZcastKernelBuilder::new(KernelConfig::default(), storage, links, theme)
        .with_ui_extension(cmd_k)
        .build()
        .await
    {
        Ok(_) => panic!("building zcast-cmd-k without a SearchProvider must fail"),
        Err(err) => err,
    };

    assert!(
        matches!(
            err,
            KernelError::MissingDependency {
                module: "zcast-cmd-k",
                interface,
            } if interface == IFACE_SEARCH
        ),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn popover_extension_assets_slots_and_privacy_gate_work_end_to_end() {
    let (kernel, _) = full_v02_kernel(false).await;
    let app = zcast_kernel::http::router(kernel);

    // Home page embeds popover CSS + JS for both public preview and admin.
    let home_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/?preview=public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(home_resp.status(), StatusCode::OK);
    let home_html = String::from_utf8(
        axum::body::to_bytes(home_resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(home_html.contains("/_zcast/ext/popover/popover.css"));
    assert!(home_html.contains("/_zcast/ext/popover/popover.js"));

    // Extension assets are served with proper status and MIME types.
    for (uri, expected_ct) in [
        ("/_zcast/ext/popover/popover.css", "text/css"),
        ("/_zcast/ext/popover/popover.js", "application/javascript"),
    ] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "asset {uri}");
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .contains(expected_ct)
        );
    }

    // Popover endpoint returns balanced excerpt + backlink count + no-store.
    let pop_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/popover/how-sliding-panes-work?preview=public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pop_resp.status(), StatusCode::OK);
    assert_eq!(
        pop_resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let pop_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(pop_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(pop_json["canonical_slug"], "how-sliding-panes-work");
    assert_eq!(pop_json["title"], "How Sliding Panes Work");
    assert!(!pop_json["excerpt_html"].as_str().unwrap().is_empty());
    assert!(pop_json["backlink_count"].as_u64().unwrap() >= 1);

    // Private note popover MUST return 404 in public preview, 200 in admin mode.
    let priv_pub = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/popover/private-scratchpad?preview=public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(priv_pub.status(), StatusCode::NOT_FOUND);

    let priv_admin = app
        .oneshot(
            Request::builder()
                .uri("/_zcast/popover/private-scratchpad")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(priv_admin.status(), StatusCode::OK);
}

#[tokio::test]
async fn search_fts_and_cmd_k_sync_live_and_respect_privacy() {
    let (kernel, _) = full_v02_kernel(false).await;
    let app = zcast_kernel::http::router(kernel);

    // Home page includes Cmd+K trigger, CSS, and JS.
    let page_resp = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let page_html = String::from_utf8(
        axum::body::to_bytes(page_resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(page_html.contains("data-zcast-cmdk-open"));
    assert!(page_html.contains("/_zcast/ext/cmd-k/cmd-k.css"));
    assert!(page_html.contains("/_zcast/ext/cmd-k/cmd-k.js"));

    // Extension assets served.
    for uri in ["/_zcast/ext/cmd-k/cmd-k.css", "/_zcast/ext/cmd-k/cmd-k.js"] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Instant prefix query finds seeded card with no-store header.
    let search_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/search?q=slid&preview=public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(search_resp.status(), StatusCode::OK);
    assert_eq!(
        search_resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let data: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(search_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(data["hits"][0]["slug"], "how-sliding-panes-work");

    // Private note never appears in public search, appears in admin search.
    let (_, priv_pub) = send(
        &app,
        Request::builder()
            .uri("/_zcast/search?q=scratchpad&preview=public")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(priv_pub["hits"].as_array().unwrap().len(), 0);

    let (_, priv_admin) = send(
        &app,
        Request::builder()
            .uri("/_zcast/search?q=scratchpad")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(priv_admin["hits"][0]["slug"], "private-scratchpad");

    // Creating, renaming, and deleting a note updates search live.
    let (status, created) = send(
        &app,
        json_request(
            "POST",
            "/_zcast/api/notes",
            serde_json::json!({
                "title": "Kestrel Observation",
                "markdown_body": "Hovering calmly over the ridge.",
                "private": false
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let note_id = created["note"]["id"].as_str().unwrap().to_owned();

    let (_, found) = send(
        &app,
        Request::builder()
            .uri("/_zcast/search?q=kestrel&preview=public")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(found["hits"][0]["slug"], "kestrel-observation");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/_zcast/api/notes/{note_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (_, after_del) = send(
        &app,
        Request::builder()
            .uri("/_zcast/search?q=kestrel&preview=public")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(after_del["hits"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn basic_auth_protects_admin_routes_and_handles_login_logout_flow() {
    let (kernel, _) = full_v02_kernel(true).await;
    let app = zcast_kernel::http::router(kernel);

    // 1. Unauthenticated visitor sees Sign in, no editor, and cannot write via API.
    let home = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(home.status(), StatusCode::OK);
    let html = String::from_utf8(
        axum::body::to_bytes(home.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("data-zcast-login"));
    assert!(!html.contains("/_zcast/editor/editor.bundle.js"));
    assert!(!html.contains("data-zcast-new-note"));

    let (write_status, _) = send(
        &app,
        json_request(
            "POST",
            "/_zcast/api/notes",
            serde_json::json!({
                "title": "Blocked Note",
                "markdown_body": "nope",
                "private": false
            }),
        ),
    )
    .await;
    assert_eq!(write_status, StatusCode::FORBIDDEN);

    // 2. GET /_zcast/login renders login page.
    let login_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/login?next=/how-sliding-panes-work")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login_get.status(), StatusCode::OK);

    // 3. POST /_zcast/login with wrong password returns 401 and no cookie.
    let bad_login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/_zcast/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("username=admin&password=wrong&next=/"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_login.status(), StatusCode::UNAUTHORIZED);
    assert!(bad_login.headers().get(header::SET_COOKIE).is_none());

    // 4. POST /_zcast/login with valid credentials sets HttpOnly cookie and redirects.
    let ok_login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/_zcast/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "username=admin&password=s3cr3t-pass&next=/how-sliding-panes-work",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok_login.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        ok_login.headers().get(header::LOCATION).unwrap(),
        "/how-sliding-panes-work"
    );
    let set_cookie = ok_login
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(set_cookie.starts_with("zcast_session="));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Lax"));
    let cookie_pair = set_cookie.split(';').next().unwrap().to_owned();

    // 5. Authenticated cookie unlocks editor, Log out button, private notes & API writes.
    let auth_home = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::COOKIE, &cookie_pair)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let auth_html = String::from_utf8(
        axum::body::to_bytes(auth_home.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(auth_html.contains("data-zcast-logout"));
    assert!(auth_html.contains("/_zcast/editor/editor.bundle.js"));
    assert!(auth_html.contains("data-zcast-new-note"));

    let priv_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/private-scratchpad")
                .header(header::COOKIE, &cookie_pair)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(priv_ok.status(), StatusCode::OK);

    // 6. POST /_zcast/logout revokes session; replaying old cookie fails immediately.
    let logout = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/_zcast/logout")
                .header(header::COOKIE, &cookie_pair)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    assert!(
        logout
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );

    let after_logout = app
        .oneshot(
            Request::builder()
                .uri("/private-scratchpad")
                .header(header::COOKIE, &cookie_pair)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(after_logout.status(), StatusCode::NOT_FOUND);
}

/* ========================================================================== */
/* v0.3 + v0.4: Magic-link auth, live admin settings, module graph, backups    */
/* ========================================================================== */

async fn response_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn form_request(method: &str, uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

async fn magic_link_kernel() -> (
    Arc<zcast_kernel::ZcastKernel>,
    Arc<MagicLinkAuthProvider>,
    Arc<SqliteStorage>,
) {
    let storage = Arc::new(SqliteStorage::connect("sqlite::memory:").await.unwrap());
    let links = Arc::new(WikilinkParser::new());
    let theme = Arc::new(DavidHewitsonTheme::new());
    let provider = Arc::new(MagicLinkAuthProvider::new(
        storage.clone(),
        "admin@example.com",
    ));
    let config = KernelConfig {
        site_title: "David's Notes".into(),
        color_mode: ThemeColorMode::Dark,
        public_url: Some("https://notes.example.com".into()),
        ..Default::default()
    };

    let kernel = ZcastKernelBuilder::new(config, storage.clone(), links, theme)
        .with_auth(provider.clone())
        .build()
        .await
        .unwrap();

    (kernel, provider, storage)
}

#[tokio::test]
async fn magic_link_login_is_scanner_safe_one_time_and_unlocks_admin() {
    let (kernel, provider, storage) = magic_link_kernel().await;
    let app = zcast_kernel::http::router(kernel);

    // 1. Requesting a link always returns the same neutral notice, with no cookie.
    let login = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/_zcast/login",
            "email=admin%40example.com&next=%2F_zcast%2Fadmin".to_owned(),
        ))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    assert_eq!(
        login.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert!(login.headers().get(header::SET_COOKIE).is_none());
    let login_html = response_text(login).await;
    assert!(
        login_html.contains("one-time sign-in link"),
        "neutral notice missing: {login_html}"
    );

    // The POST minted a hashed token in the provider's ModuleStore.
    let minted = storage
        .module_store("zcast-auth-magic-link")
        .list_prefix("magic:", None)
        .await
        .unwrap();
    assert_eq!(minted.len(), 1, "login POST must mint exactly one token");

    // 2. Mint a known link through the same provider path so the token can be replayed.
    let link = provider
        .create_cli_login_link("https://notes.example.com")
        .await
        .unwrap();
    let token = link
        .split("token=")
        .nth(1)
        .expect("token in magic link")
        .to_owned();

    // 3. GET twice: peeking must never consume the token.
    for _ in 0..2 {
        let confirm = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/_zcast/auth/magic?token={token}&next=/_zcast/admin"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(confirm.status(), StatusCode::OK);
        assert_eq!(
            confirm.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let confirm_html = response_text(confirm).await;
        assert!(
            confirm_html.contains("data-zcast-magic-confirm"),
            "valid link must render the confirm form: {confirm_html}"
        );
    }
    assert!(
        provider.peek_magic_token(&token).await.unwrap(),
        "GET requests must not consume the token"
    );

    // 4. POST consumes the token and starts an admin session.
    let consume = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/_zcast/auth/magic",
            format!("token={token}&next=/_zcast/admin"),
        ))
        .await
        .unwrap();
    assert_eq!(consume.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        consume.headers().get(header::LOCATION).unwrap(),
        "/_zcast/admin"
    );
    let set_cookie = consume
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(set_cookie.starts_with("zcast_session="));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Lax"));
    assert!(set_cookie.contains("Path=/"));
    let cookie_pair = set_cookie.split(';').next().unwrap().to_owned();

    // 5. The session cookie unlocks the admin dashboard.
    let admin = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/admin")
                .header(header::COOKIE, &cookie_pair)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin.status(), StatusCode::OK);
    assert!(response_text(admin).await.contains("data-zcast-admin-page"));

    // 6. Replaying the same one-time token is rejected.
    let replay = app
        .oneshot(form_request(
            "POST",
            "/_zcast/auth/magic",
            format!("token={token}&next=/_zcast/admin"),
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
}

async fn admin_io_kernel() -> (Arc<zcast_kernel::ZcastKernel>, Arc<SqliteStorage>) {
    let storage = Arc::new(SqliteStorage::connect("sqlite::memory:").await.unwrap());
    let links = Arc::new(WikilinkParser::new());
    let theme = Arc::new(DavidHewitsonTheme::new());
    let auth = Arc::new(BasicAuthProvider::new(storage.clone()));
    let config = KernelConfig {
        site_title: "David's Notes".into(),
        color_mode: ThemeColorMode::Dark,
        ..Default::default()
    };

    let kernel = ZcastKernelBuilder::new(config, storage.clone(), links, theme)
        .with_auth(auth.clone())
        .with_exporter(Arc::new(MarkdownIo::new()))
        .with_importer(Arc::new(MarkdownIo::new()))
        .build()
        .await
        .unwrap();

    auth.ensure_admin_credentials("admin", Some("s3cr3t-pass"))
        .await
        .unwrap();

    (kernel, storage)
}

async fn admin_cookie(app: &axum::Router) -> String {
    let login = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/_zcast/login",
            "username=admin&password=s3cr3t-pass&next=/_zcast/admin".to_owned(),
        ))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    login
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn admin_zip_export_is_gated_and_round_trips_through_markdown_io() {
    let (kernel, _storage) = admin_io_kernel().await;
    let app = zcast_kernel::http::router(kernel);

    // Unauthenticated visitors cannot download a backup.
    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/api/export.zip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::FORBIDDEN);

    let cookie = admin_cookie(&app).await;
    let export = app
        .oneshot(
            Request::builder()
                .uri("/_zcast/api/export.zip")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(export.status(), StatusCode::OK);
    assert_eq!(
        export.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/zip"
    );
    assert_eq!(
        export.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let disposition = export
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(disposition.contains("zcast-notes.zip"), "{disposition}");

    let bytes = axum::body::to_bytes(export.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..2], b"PK", "export must be a zip archive");

    // The archive must round-trip through the Markdown importer.
    let fresh = Arc::new(SqliteStorage::connect("sqlite::memory:").await.unwrap());
    fresh.initialize().await.unwrap();
    let summary = MarkdownIo::new()
        .import_from_zip_bytes(fresh.as_ref(), &bytes)
        .await
        .unwrap();
    assert!(summary.created >= 3, "expected seeded notes: {summary:?}");

    let restored = fresh
        .list_notes(NoteListFilter::default(), ReadContext::ADMIN)
        .await
        .unwrap();
    assert!(restored.iter().any(|note| note.title == "Index"));
    assert!(
        restored.iter().any(|note| note.private),
        "admin backup must include private notes"
    );
}

#[tokio::test]
async fn admin_dashboard_redirects_guests_and_applies_live_settings() {
    let (kernel, _storage) = admin_io_kernel().await;
    let app = zcast_kernel::http::router(kernel);

    // 1. Guests are redirected to the login page (auth is active).
    let guest = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(guest.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        guest.headers().get(header::LOCATION).unwrap(),
        "/_zcast/login?next=/_zcast/admin"
    );

    let cookie = admin_cookie(&app).await;

    // 2. Admins get the module graph and live settings form.
    let dashboard = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/admin")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dashboard.status(), StatusCode::OK);
    assert_eq!(
        dashboard.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let dashboard_html = response_text(dashboard).await;
    assert!(dashboard_html.contains("data-zcast-module-graph"));
    assert!(dashboard_html.contains("data-zcast-save-settings"));
    assert!(dashboard_html.contains("data-zcast-dependency-svg"));
    assert!(dashboard_html.contains("data-zcast-export-zip"));
    assert!(dashboard_html.contains("zcast-storage-sqlite"));
    assert!(dashboard_html.contains("zcast:storage/repository"));
    assert!(dashboard_html.contains("zcast:io/markdown"));

    // 3. Saving settings persists live (no restart).
    let save = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/_zcast/admin/settings")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "site_title=Architectural+Notes&pinned_home_slug=index&color_mode=light",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(save.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        save.headers().get(header::LOCATION).unwrap(),
        "/_zcast/admin?saved=1"
    );

    let saved = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_zcast/admin?saved=1")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response_text(saved).await.contains("Settings saved."));

    // 4. `GET /` reflects the new title and colour mode immediately.
    let home = app
        .oneshot(
            Request::builder()
                .uri("/?preview=public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(home.status(), StatusCode::OK);
    let home_html = response_text(home).await;
    assert!(home_html.contains("Architectural Notes"), "{home_html}");
    assert!(!home_html.contains("David's Notes"), "{home_html}");
    assert!(home_html.contains(r#"data-theme="light""#), "{home_html}");
}

#[tokio::test]
async fn unauthenticated_admin_settings_write_is_forbidden() {
    let (kernel, _storage) = admin_io_kernel().await;
    let app = zcast_kernel::http::router(kernel);

    let resp = app
        .oneshot(form_request(
            "POST",
            "/_zcast/admin/settings",
            "site_title=Hijacked&pinned_home_slug=index&color_mode=dark".to_owned(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn onnx_search_provider_reports_semantic_capabilities_and_matches_morphology() {
    let storage = Arc::new(SqliteStorage::connect("sqlite::memory:").await.unwrap());
    let links = Arc::new(WikilinkParser::new());
    let theme = Arc::new(DavidHewitsonTheme::new());
    let kernel = ZcastKernelBuilder::new(KernelConfig::default(), storage, links, theme)
        .with_search(Arc::new(OnnxSearchProvider::new()))
        .build()
        .await
        .unwrap();

    kernel
        .create_note(
            "Architecture Blueprint".into(),
            "Structural design documents for buildings and public spaces.".into(),
            false,
        )
        .await
        .unwrap();

    let app = zcast_kernel::http::router(kernel);
    let (status, data) = send(
        &app,
        Request::builder()
            .uri("/_zcast/search?q=architectural&preview=public")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(data["capabilities"]["topical_semantic"].as_bool().unwrap());
    assert!(data["capabilities"]["keyword"].as_bool().unwrap());

    let hits = data["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "semantic query must find a match: {data}");
    assert_eq!(hits[0]["slug"], "architecture-blueprint", "{data}");
}
