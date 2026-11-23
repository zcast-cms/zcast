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
