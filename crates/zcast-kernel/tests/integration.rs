use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;
use zcast_editor_inline::InlineEditor;
use zcast_interfaces::{Note, ReadContext, ThemeColorMode};
use zcast_kernel::{KernelConfig, ZcastKernelBuilder};
use zcast_links_wikilink::WikilinkParser;
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
    assert!(!before.rendered_html.contains(r#"data-zcast-note="my-first-note""#));

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
    assert!(after.rendered_html.contains(r#"data-zcast-note="my-first-note""#));
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
    assert_eq!(res.renamed_from_slug.as_deref(), Some("how-sliding-panes-work"));
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
        edges.iter().all(|link| link["target_title"] != "Taken Title"),
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
