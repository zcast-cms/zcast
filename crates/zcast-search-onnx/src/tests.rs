//! End-to-end behaviour of the hybrid semantic search provider.

use std::sync::Arc;

use zcast_interfaces::{
    IFACE_SEARCH, InterfaceSpec, MatchKind, Note, NoteAlias, NoteEvent, NoteId, NoteLifecycleHook,
    ReadContext, SearchHit, SearchProvider, SearchQuery, slugify,
};

use crate::{HashSemanticEmbedder, OnnxSearchProvider, TextEmbedder, normalize_l2};

fn provider() -> OnnxSearchProvider {
    OnnxSearchProvider::new()
}

fn test_note(title: &str, body: &str) -> Note {
    Note::new(title, body, false)
}

fn private_note(title: &str, body: &str) -> Note {
    Note::new(title, body, true)
}

fn query(text: &str, limit: usize) -> SearchQuery {
    SearchQuery {
        text: text.to_owned(),
        limit,
        context_note_id: None,
    }
}

fn contextual_query(text: &str, limit: usize, context: &NoteId) -> SearchQuery {
    SearchQuery {
        text: text.to_owned(),
        limit,
        context_note_id: Some(context.clone()),
    }
}

async fn search(
    provider: &OnnxSearchProvider,
    text: &str,
    limit: usize,
    ctx: ReadContext,
) -> Vec<SearchHit> {
    provider.search(&query(text, limit), ctx).await.unwrap()
}

async fn index_all(provider: &OnnxSearchProvider, notes: &[&Note]) {
    for note in notes {
        provider.index_note(note).await.unwrap();
    }
}

#[tokio::test]
async fn manifest_and_capabilities_are_declared() {
    let provider = provider();
    let manifest = provider.manifest();

    assert_eq!(manifest.id, "zcast-search-onnx");
    assert_eq!(manifest.name, "Semantic Hybrid Search (ONNX)");
    assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        manifest.description,
        "Local dense-vector semantic + lexical + graph-boosted hybrid search provider"
    );
    assert_eq!(
        manifest.provides,
        &[InterfaceSpec {
            name: IFACE_SEARCH,
            version: "1.0.0",
        }]
    );
    assert!(manifest.requires.is_empty());
    assert!(manifest.recommends.is_empty());

    let capabilities = provider.capabilities();
    assert!(capabilities.keyword);
    assert!(capabilities.topical_semantic);
    assert!(capabilities.graph_boosted);
    assert!(capabilities.instant_as_you_type);
}

#[tokio::test]
async fn default_embedder_is_pure_rust_and_metadata_is_optional() {
    let provider = provider();
    assert_eq!(provider.embedder_dimensions(), 256);
    assert!(provider.model_path().is_none());

    let with_model = OnnxSearchProvider::new()
        .with_model_path("models/all-MiniLM-L6-v2.onnx")
        .with_embedder(Arc::new(HashSemanticEmbedder::with_dimensions(384)));
    assert_eq!(
        with_model.model_path(),
        Some(std::path::Path::new("models/all-MiniLM-L6-v2.onnx"))
    );
    assert_eq!(with_model.embedder_dimensions(), 384);
}

#[tokio::test]
async fn topical_semantic_matching_finds_morphological_variants() {
    let provider = provider();
    let architecture = test_note(
        "Architecture Blueprint",
        "Structural design documents for buildings and public spaces.",
    );
    let sliding = test_note(
        "Sliding Panes",
        "A pane layout where linked notes open side by side.",
    );
    let writer = test_note(
        "Writer's Notebook",
        "Notes on composition, drafting, and prose rhythm.",
    );
    let unrelated = test_note(
        "Sourdough Baking",
        "Flour, water, salt, and a lively starter.",
    );

    index_all(&provider, &[&architecture, &sliding, &writer, &unrelated]).await;

    let architectural = search(&provider, "architectural", 10, ReadContext::PUBLIC).await;
    assert_eq!(architectural[0].note_id, architecture.id);
    assert!(matches!(
        architectural[0].match_kind,
        MatchKind::Topical | MatchKind::Hybrid
    ));

    let slides = search(&provider, "slides", 10, ReadContext::PUBLIC).await;
    assert_eq!(slides[0].note_id, sliding.id);
    assert!(matches!(
        slides[0].match_kind,
        MatchKind::Topical | MatchKind::Hybrid
    ));

    let writing = search(&provider, "writing", 10, ReadContext::PUBLIC).await;
    assert_eq!(writing[0].note_id, writer.id);
    assert!(matches!(
        writing[0].match_kind,
        MatchKind::Topical | MatchKind::Hybrid
    ));

    // Semantic matching must not turn into "return everything".
    assert!(!architectural.iter().any(|hit| hit.note_id == unrelated.id));
    assert!(!slides.iter().any(|hit| hit.note_id == unrelated.id));
}

#[tokio::test]
async fn hybrid_match_ranks_combined_evidence_above_weaker_matches() {
    let provider = provider();
    let strong = test_note(
        "Rust Ownership",
        "Rust ownership and borrowing rules keep memory safe without a garbage collector.",
    );
    let weak = test_note(
        "Rust Tooling",
        "cargo clippy rustfmt and editor plugins for everyday work.",
    );
    let unrelated = test_note(
        "Sourdough Baking",
        "Flour, water, salt, and a lively starter.",
    );

    index_all(&provider, &[&strong, &weak, &unrelated]).await;

    let hits = search(&provider, "rust ownership", 10, ReadContext::PUBLIC).await;
    assert!(hits.len() >= 2, "expected hybrid + weaker match");
    assert_eq!(hits[0].note_id, strong.id);
    assert_eq!(hits[0].match_kind, MatchKind::Hybrid);
    assert!((hits[0].score - 1.0).abs() < 1e-6);

    let strong_hit = &hits[0];
    let weak_hit = hits
        .iter()
        .find(|hit| hit.note_id == weak.id)
        .expect("weaker keyword match should still rank");
    assert!(strong_hit.score > weak_hit.score);
    assert!(!hits.iter().any(|hit| hit.note_id == unrelated.id));
}

#[tokio::test]
async fn graph_context_boost_elevates_direct_neighbors() {
    let provider = provider();
    let context = test_note("Context Note", "See [[Alpha Notes]] for the related idea.");
    let alpha = test_note("Alpha Notes", "shared topic words about layouts and panes.");
    let beta = test_note("Beta Notes", "shared topic words about layouts and panes.");

    index_all(&provider, &[&context, &alpha, &beta]).await;

    let plain = search(&provider, "shared topic words", 10, ReadContext::PUBLIC).await;
    let boosted = provider
        .search(
            &contextual_query("shared topic words", 10, &context.id),
            ReadContext::PUBLIC,
        )
        .await
        .unwrap();

    assert_eq!(plain.len(), 2);
    assert_eq!(boosted.len(), 2);

    let plain_alpha = plain
        .iter()
        .find(|hit| hit.note_id == alpha.id)
        .expect("alpha should match");
    let boosted_alpha = boosted
        .iter()
        .find(|hit| hit.note_id == alpha.id)
        .expect("alpha should match");
    let boosted_beta = boosted
        .iter()
        .find(|hit| hit.note_id == beta.id)
        .expect("beta should match");

    assert!(boosted_alpha.score > boosted_beta.score);
    assert_eq!(boosted[0].note_id, alpha.id);
    assert!(
        boosted_alpha.score >= plain_alpha.score,
        "context boost must never lower a score"
    );
}

#[tokio::test]
async fn graph_boost_prefers_direct_neighbors_over_hubs_of_equal_text() {
    let provider = provider();
    let hub = test_note("Hub Note", "A frequently referenced page.");

    // Two notes with byte-identical bodies; only the link graph differs.
    let target = test_note("Target Page", "equal shared wording.");
    let decoy = test_note("Decoy Page", "equal shared wording.");

    index_all(&provider, &[&hub, &target, &decoy]).await;

    let mut contextual = test_note("Reader Context", "Links to [[Target Page]] only.");
    contextual.id = NoteId::from("reader-context");
    provider.index_note(&contextual).await.unwrap();

    let hits = provider
        .search(
            &contextual_query("equal shared wording", 10, &contextual.id),
            ReadContext::PUBLIC,
        )
        .await
        .unwrap();

    let target_hit = hits
        .iter()
        .find(|hit| hit.note_id == target.id)
        .expect("target should match");
    let decoy_hit = hits
        .iter()
        .find(|hit| hit.note_id == decoy.id)
        .expect("decoy should match");
    assert!(
        target_hit.score > decoy_hit.score,
        "target {} should outrank decoy {}",
        target_hit.score,
        decoy_hit.score
    );
}

#[tokio::test]
async fn strict_public_and_admin_isolation() {
    let provider = provider();
    let public = test_note("Public Plan", "Visible planning notes about architecture.");
    let private = private_note(
        "Architecture Blueprint Secret",
        "Hidden planning notes about architecture.",
    );

    index_all(&provider, &[&public, &private]).await;

    let public_hits = search(&provider, "planning architecture", 10, ReadContext::PUBLIC).await;
    assert_eq!(public_hits.len(), 1);
    assert_eq!(public_hits[0].note_id, public.id);

    // Purely semantic queries must not leak private notes either.
    let public_semantic = search(&provider, "architectural", 10, ReadContext::PUBLIC).await;
    assert!(!public_semantic.iter().any(|hit| hit.note_id == private.id));

    let admin_hits = search(&provider, "planning architecture", 10, ReadContext::ADMIN).await;
    assert_eq!(admin_hits.len(), 2);
    assert!(admin_hits.iter().any(|hit| hit.note_id == private.id));

    // Empty queries must not leak private notes.
    let public_recent = search(&provider, "   ", 10, ReadContext::PUBLIC).await;
    assert_eq!(public_recent.len(), 1);
    assert_eq!(public_recent[0].note_id, public.id);

    let admin_recent = search(&provider, "", 10, ReadContext::ADMIN).await;
    assert_eq!(admin_recent.len(), 2);
    assert_eq!(admin_recent[0].note_id, private.id);
}

#[tokio::test]
async fn private_referrers_never_boost_public_results() {
    let provider = provider();
    // Both candidates share a title and body so their lexical and semantic
    // scores are identical; only the link graph can separate them. They are
    // addressed by distinct slugs to keep the names map unambiguous.
    let mut referrers = Vec::new();
    for index in 0..4 {
        referrers.push(test_note(
            &format!("Public Referrer {index}"),
            "Links to [[target-page]].",
        ));
    }
    let mut target = test_note("Shared Page", "shared wording for the graph test.");
    target.slug = "target-page".to_owned();
    let mut decoy = test_note("Shared Page", "shared wording for the graph test.");
    decoy.slug = "decoy-page".to_owned();
    let private_referrer = private_note("Secret Referrer", "Links to [[decoy-page]].");

    for referrer in &referrers {
        provider.index_note(referrer).await.unwrap();
    }
    index_all(&provider, &[&target, &decoy, &private_referrer]).await;

    let hits = search(
        &provider,
        "shared wording graph test",
        10,
        ReadContext::PUBLIC,
    )
    .await;
    let target_hit = hits
        .iter()
        .find(|hit| hit.note_id == target.id)
        .expect("target should match");
    let decoy_hit = hits
        .iter()
        .find(|hit| hit.note_id == decoy.id)
        .expect("decoy should match");

    assert!(
        target_hit.score > decoy_hit.score,
        "only visible in-degree should count: target {} vs decoy {}",
        target_hit.score,
        decoy_hit.score
    );
}

/// Deterministic synonym embedder used to prove the `TextEmbedder` seam.
#[derive(Debug, Default)]
struct SynonymEmbedder;

impl TextEmbedder for SynonymEmbedder {
    fn dimensions(&self) -> usize {
        4
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; self.dimensions()];
        for word in text
            .to_lowercase()
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            let slot = match word {
                "car" | "cars" | "automobile" | "vehicle" | "motor" => 0,
                "rust" | "cargo" | "compiler" | "borrow" => 1,
                "cake" | "baking" | "recipe" | "flour" => 2,
                "garden" | "soil" | "compost" => 3,
                _ => continue,
            };
            vector[slot] += 1.0;
        }
        normalize_l2(&mut vector);
        vector
    }
}

#[tokio::test]
async fn custom_text_embedder_can_be_injected_and_reembeds_existing_notes() {
    let automobile = test_note(
        "Automobile Maintenance",
        "Servicing a vehicle regularly keeps the motor happy.",
    );
    let compiler = test_note("Compiler Notes", "cargo rust borrow checker");
    let baking = test_note("Sourdough Baking", "flour water salt starter");

    let provider = OnnxSearchProvider::new();
    index_all(&provider, &[&automobile, &compiler, &baking]).await;

    // Swapping the embedder must re-embed everything already indexed.
    let provider = provider.with_embedder(Arc::new(SynonymEmbedder));
    assert_eq!(provider.embedder_dimensions(), 4);
    assert_eq!(provider.indexed_note_count(), 3);

    let hits = search(&provider, "car", 10, ReadContext::PUBLIC).await;
    assert_eq!(hits[0].note_id, automobile.id);
    assert_eq!(hits[0].match_kind, MatchKind::Topical);
    assert!((hits[0].score - 1.0).abs() < 1e-6);

    // "cargo" shares a prefix with the query, so it is a keyword-only hit.
    let compiler_hit = hits.iter().find(|hit| hit.note_id == compiler.id);
    if let Some(compiler_hit) = compiler_hit {
        assert_eq!(compiler_hit.match_kind, MatchKind::Keyword);
    }
    assert!(!hits.iter().any(|hit| hit.note_id == baking.id));
}

#[tokio::test]
async fn empty_query_returns_recent_visible_notes_up_to_limit() {
    let provider = provider();
    let mut older = test_note("Older Note", "older body");
    older.updated_at -= std::time::Duration::from_secs(3600);

    let mut newer = test_note("Newer Note", "newer body");
    newer.updated_at += std::time::Duration::from_secs(3600);

    let mut private_newest = private_note("Private Newest", "private body");
    private_newest.updated_at += std::time::Duration::from_secs(7200);

    index_all(&provider, &[&older, &newer, &private_newest]).await;

    let recent = search(&provider, "   ", 2, ReadContext::PUBLIC).await;
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].note_id, newer.id);
    assert_eq!(recent[1].note_id, older.id);
    assert!(
        recent
            .iter()
            .all(|hit| hit.match_kind == MatchKind::Keyword)
    );
    assert!(!recent.iter().any(|hit| hit.note_id == private_newest.id));

    assert!(
        search(&provider, "", 0, ReadContext::PUBLIC)
            .await
            .is_empty()
    );
    assert!(
        search(&provider, "older", 0, ReadContext::PUBLIC)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn aliases_and_slugs_are_searchable() {
    let provider = provider();
    let mut renamed = test_note("Current Name", "The body remains unchanged.");
    renamed.aliases.push(NoteAlias {
        slug: "old-name".to_owned(),
        title: "Old Name".to_owned(),
        created_at: renamed.created_at,
    });
    provider.index_note(&renamed).await.unwrap();

    let hits = search(&provider, "old name", 10, ReadContext::PUBLIC).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].note_id, renamed.id);
    assert_eq!(hits[0].title, "Current Name");
}

#[tokio::test]
async fn snippets_are_clean_plain_text_centered_on_the_match() {
    let provider = provider();
    let body = format!(
        "{} **needle** in the haystack {}",
        "alpha ".repeat(60),
        "omega ".repeat(60)
    );
    let note = test_note("Snippet Check", &body);
    provider.index_note(&note).await.unwrap();

    let hits = search(&provider, "needle", 10, ReadContext::PUBLIC).await;
    assert_eq!(hits.len(), 1);
    let snippet = &hits[0].snippet;

    assert!(snippet.contains("needle"));
    assert!(!snippet.contains("**"));
    assert!(snippet.starts_with('…'));
    assert!(snippet.ends_with('…'));
    assert!(snippet.chars().count() <= 140);
    assert!(!snippet.starts_with("alpha alpha alpha"));
}

#[tokio::test]
async fn index_update_remove_reindex_and_lifecycle_hooks() {
    let provider = provider();
    let mut note = test_note("Version One", "The first body.");
    provider.index_note(&note).await.unwrap();
    assert_eq!(
        search(&provider, "version one", 10, ReadContext::PUBLIC)
            .await
            .len(),
        1
    );

    note.title = "Version Two".to_owned();
    note.slug = slugify(&note.title);
    note.markdown_body = "The second body.".to_owned();
    provider.index_note(&note).await.unwrap();

    let updated = search(&provider, "version two", 10, ReadContext::PUBLIC).await;
    assert_eq!(updated[0].title, "Version Two");
    assert!(
        search(&provider, "first", 10, ReadContext::PUBLIC)
            .await
            .is_empty()
    );
    assert_eq!(provider.indexed_note_count(), 1);

    provider.remove_note(&note.id).await.unwrap();
    assert!(
        search(&provider, "version", 10, ReadContext::PUBLIC)
            .await
            .is_empty()
    );
    assert_eq!(provider.indexed_note_count(), 0);

    let alpha = test_note("Alpha Note", "alpha body");
    let beta = test_note("Beta Note", "beta body");
    provider
        .index_note(&test_note("Gamma Note", "gamma body"))
        .await
        .unwrap();
    provider
        .reindex_all(&[alpha.clone(), beta.clone()])
        .await
        .unwrap();

    assert!(
        search(&provider, "gamma", 10, ReadContext::PUBLIC)
            .await
            .is_empty()
    );
    assert_eq!(provider.indexed_note_count(), 2);
    assert_eq!(
        search(&provider, "alpha", 10, ReadContext::PUBLIC).await[0].note_id,
        alpha.id
    );
    assert_eq!(
        search(&provider, "beta", 10, ReadContext::PUBLIC).await[0].note_id,
        beta.id
    );

    assert_eq!(NoteLifecycleHook::id(&provider), "zcast-search-onnx");

    // Lifecycle hook: Saved indexes, Renamed re-indexes, Deleted removes.
    let hooked = test_note("Hooked Note", "hooked body");
    provider.on_event(&NoteEvent::Saved(hooked.clone())).await;
    assert_eq!(
        search(&provider, "hooked", 10, ReadContext::PUBLIC)
            .await
            .len(),
        1
    );

    let mut renamed = hooked.clone();
    renamed.title = "Renamed Note".to_owned();
    renamed.slug = slugify(&renamed.title);
    renamed.markdown_body = "renamed body".to_owned();
    provider
        .on_event(&NoteEvent::Renamed {
            old_title: hooked.title.clone(),
            old_slug: hooked.slug.clone(),
            note: renamed.clone(),
        })
        .await;

    let renamed_hits = search(&provider, "renamed", 10, ReadContext::PUBLIC).await;
    assert_eq!(renamed_hits[0].note_id, renamed.id);
    assert!(
        search(&provider, "hooked", 10, ReadContext::PUBLIC)
            .await
            .is_empty()
    );

    provider
        .on_event(&NoteEvent::Deleted(renamed.id.clone()))
        .await;
    assert!(
        search(&provider, "renamed", 10, ReadContext::PUBLIC)
            .await
            .is_empty()
    );
    assert_eq!(provider.indexed_note_count(), 2);
}
