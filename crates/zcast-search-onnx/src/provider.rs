//! `zcast:search/provider@1.0.0` implementation: dense-vector semantic search
//! fused with lexical scoring and wikilink graph boosts.
//!
//! Privacy is the first gate on every query path (DEC-02): notes marked
//! `private` are removed from the candidate set *before* lexical scoring,
//! semantic scoring, graph boosting, or limiting. Private referrers never
//! contribute graph boosts to a public query either.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use async_trait::async_trait;
use zcast_interfaces::{
    IFACE_SEARCH, InterfaceSpec, MatchKind, ModuleManifest, Note, NoteEvent, NoteId,
    NoteLifecycleHook, ReadContext, SearchCapabilities, SearchError, SearchHit, SearchProvider,
    SearchQuery, slugify,
};

use crate::embedder::{HashSemanticEmbedder, TextEmbedder, cosine};
use crate::text::{
    clean_body, extract_wikilink_targets, make_snippet, normalize_name, query_stems,
    stem_frequencies, term_frequencies, tokenize,
};

const MANIFEST: ModuleManifest = ModuleManifest {
    id: "zcast-search-onnx",
    name: "Semantic Hybrid Search (ONNX)",
    version: env!("CARGO_PKG_VERSION"),
    description: "Local dense-vector semantic + lexical + graph-boosted hybrid search provider",
    provides: &[InterfaceSpec {
        name: IFACE_SEARCH,
        version: "1.0.0",
    }],
    requires: &[],
    recommends: &[],
};

/// BM25 term-frequency saturation parameter.
const BM25_K1: f32 = 1.2;
/// BM25 document-length normalization parameter.
const BM25_B: f32 = 0.75;

// Lexical field weights. Exact literal token overlap is what earns the
// `Hybrid` label, so it is deliberately the strongest signal.
const TITLE_FULL_EXACT: f32 = 1.5;
const TITLE_FULL_PREFIX: f32 = 0.8;
const ALIAS_FULL_EXACT: f32 = 1.0;
const TITLE_TOKEN_EXACT: f32 = 0.6;
const TITLE_TOKEN_PREFIX: f32 = 0.3;
const TITLE_TOKEN_STEM: f32 = 0.42;
const ALIAS_TOKEN_EXACT: f32 = 0.4;
const ALIAS_TOKEN_PREFIX: f32 = 0.18;
const ALIAS_TOKEN_STEM: f32 = 0.28;
const SLUG_FULL_EXACT: f32 = 0.5;
const SLUG_TOKEN_EXACT: f32 = 0.35;
const SLUG_TOKEN_PREFIX: f32 = 0.16;
const SLUG_TOKEN_STEM: f32 = 0.25;
const BODY_EXACT_WEIGHT: f32 = 0.4;
const BODY_STEM_WEIGHT: f32 = 0.25;
const BODY_PREFIX_WEIGHT: f32 = 0.15;

// Semantic weighting: title similarity counts for more than whole-document
// similarity because bodies are long and drift off-topic.
const TITLE_SEMANTIC_WEIGHT: f32 = 0.65;
const DOCUMENT_SEMANTIC_WEIGHT: f32 = 0.35;
/// Minimum blended cosine for a result to be considered topical, and the
/// bar a candidate with zero lexical evidence must clear to be returned at all.
const TOPICAL_SEMANTIC_MIN: f32 = 0.25;

// Graph boosts.
const IN_DEGREE_WEIGHT: f32 = 0.12;
const NEIGHBOR_BOOST: f32 = 0.5;

// Final blend before per-query normalization.
const LEXICAL_WEIGHT: f32 = 1.5;
const SEMANTIC_WEIGHT: f32 = 1.0;

/// Owned, scored view of one index entry.
#[derive(Debug, Clone)]
struct IndexedNote {
    /// Original note retained so embeddings survive an embedder swap.
    note: Note,
    private: bool,
    updated_at_millis: i64,
    /// Cleaned plain-text body.
    body: String,
    title_norm: String,
    alias_norms: Vec<String>,
    slug_norm: String,
    title_tokens: Vec<String>,
    title_stems: HashSet<String>,
    alias_tokens: Vec<String>,
    alias_stems: HashSet<String>,
    slug_tokens: Vec<String>,
    slug_stems: HashSet<String>,
    body_terms: HashMap<String, u32>,
    body_stem_terms: HashMap<String, u32>,
    body_len: usize,
    title_vector: Vec<f32>,
    embedding: Vec<f32>,
    link_targets: Vec<String>,
}

impl IndexedNote {
    fn from_note(note: &Note, embedder: &dyn TextEmbedder) -> Self {
        let body = clean_body(&note.markdown_body);
        let body_tokens = tokenize(&body);
        let body_terms = term_frequencies(&body_tokens);
        let body_stem_terms = stem_frequencies(&body_tokens);
        let body_len: usize = body_terms.values().map(|count| *count as usize).sum();

        let alias_norms: Vec<String> = note
            .aliases
            .iter()
            .flat_map(|alias| [normalize_name(&alias.title), normalize_name(&alias.slug)])
            .filter(|alias| !alias.is_empty())
            .collect();

        let mut alias_tokens = Vec::new();
        for alias in &alias_norms {
            alias_tokens.extend(tokenize(alias));
        }
        let alias_stems = stem_set(&alias_tokens);

        // Alias titles are folded into the semantic title so renamed notes stay
        // discoverable by their historical names.
        let title_text = if alias_norms.is_empty() {
            note.title.clone()
        } else {
            format!("{} {}", note.title, alias_norms.join(" "))
        };
        let title_vector = embedder.embed(&title_text);
        let body_vector = embedder.embed(&body);
        let embedding = blend_vectors(&title_vector, &body_vector);

        Self {
            note: note.clone(),
            private: note.private,
            updated_at_millis: note.updated_at.timestamp_millis(),
            body,
            title_norm: normalize_name(&note.title),
            alias_norms,
            slug_norm: normalize_name(&note.slug),
            title_tokens: tokenize(&note.title),
            title_stems: stem_set(&tokenize(&note.title)),
            alias_tokens,
            alias_stems,
            slug_tokens: tokenize(&note.slug),
            slug_stems: stem_set(&tokenize(&note.slug)),
            body_terms,
            body_stem_terms,
            body_len,
            title_vector,
            embedding,
            link_targets: extract_wikilink_targets(&note.markdown_body),
        }
    }

    /// Names by which a `[[wikilink]]` can resolve to this note.
    fn name_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        push_name_keys(&mut keys, &self.note.title);
        push_name_keys(&mut keys, &self.note.slug);
        for alias in &self.note.aliases {
            push_name_keys(&mut keys, &alias.title);
            push_name_keys(&mut keys, &alias.slug);
        }
        keys
    }
}

/// Weighted blend of the title and body vectors, L2-normalized.
fn blend_vectors(title: &[f32], body: &[f32]) -> Vec<f32> {
    if title.len() != body.len() {
        return title.to_vec();
    }
    if body.iter().all(|value| *value == 0.0) {
        return title.to_vec();
    }
    if title.iter().all(|value| *value == 0.0) {
        return body.to_vec();
    }
    let mut blended: Vec<f32> = title
        .iter()
        .zip(body)
        .map(|(t, b)| TITLE_SEMANTIC_WEIGHT * t + DOCUMENT_SEMANTIC_WEIGHT * b)
        .collect();
    crate::embedder::normalize_l2(&mut blended);
    blended
}

fn stem_set(tokens: &[String]) -> HashSet<String> {
    stem_frequencies(tokens).into_keys().collect()
}

fn push_name_keys(keys: &mut Vec<String>, raw: &str) {
    let normalized = normalize_name(raw);
    if normalized.is_empty() {
        return;
    }
    keys.push(normalized.clone());
    let slug = slugify(&normalized);
    if slug != normalized {
        keys.push(slug);
    }
}

/// All mutable index state, guarded by a single `RwLock`.
#[derive(Debug, Default)]
struct IndexState {
    docs: HashMap<NoteId, IndexedNote>,
    /// Normalized title/slug/alias -> note id.
    names: HashMap<String, NoteId>,
    /// Adjacency from a citing note to the notes it links to.
    outgoing: HashMap<NoteId, HashSet<NoteId>>,
    /// Adjacency from a linked note back to its referrers.
    incoming: HashMap<NoteId, HashSet<NoteId>>,
}

impl IndexState {
    /// Rebuild name resolution and both link-graph directions from the stored
    /// docs so updates, removals, and out-of-order indexing stay consistent.
    fn rebuild_graph(&mut self) {
        self.names.clear();
        self.outgoing.clear();
        self.incoming.clear();

        for doc in self.docs.values() {
            for key in doc.name_keys() {
                self.names.insert(key, doc.id().clone());
            }
        }

        let mut edges: Vec<(NoteId, NoteId)> = Vec::new();
        for doc in self.docs.values() {
            for target in &doc.link_targets {
                let resolved = self
                    .names
                    .get(target)
                    .or_else(|| self.names.get(&slugify(target)));
                if let Some(target_id) = resolved
                    && target_id != doc.id()
                {
                    edges.push((doc.id().clone(), target_id.clone()));
                }
            }
        }

        for (from, to) in edges {
            self.outgoing
                .entry(from.clone())
                .or_default()
                .insert(to.clone());
            self.incoming.entry(to).or_default().insert(from);
        }
    }

    fn is_visible_to(&self, id: &NoteId, ctx: ReadContext) -> bool {
        self.docs
            .get(id)
            .map(|doc| ctx.include_private || !doc.private)
            .unwrap_or(false)
    }

    /// Number of incoming links from notes visible in `ctx`.
    fn visible_in_degree(&self, id: &NoteId, ctx: ReadContext) -> usize {
        self.incoming
            .get(id)
            .map(|referrers| {
                referrers
                    .iter()
                    .filter(|referrer| self.is_visible_to(referrer, ctx))
                    .count()
            })
            .unwrap_or(0)
    }

    /// True when `a` and `b` are directly connected in either direction.
    fn are_neighbors(&self, a: &NoteId, b: &NoteId) -> bool {
        self.outgoing
            .get(a)
            .is_some_and(|neighbours| neighbours.contains(b))
            || self
                .incoming
                .get(a)
                .is_some_and(|neighbours| neighbours.contains(b))
    }
}

impl IndexedNote {
    fn id(&self) -> &NoteId {
        &self.note.id
    }
}

/// Hybrid dense-vector + lexical + graph-boosted search provider.
///
/// The default embedder is the pure-Rust [`HashSemanticEmbedder`]; supply an
/// ONNX-runtime-backed [`TextEmbedder`] via
/// [`OnnxSearchProvider::with_embedder`] for real transformer embeddings.
pub struct OnnxSearchProvider {
    embedder: Arc<dyn TextEmbedder>,
    model_path: Option<PathBuf>,
    state: RwLock<IndexState>,
}

impl fmt::Debug for OnnxSearchProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnnxSearchProvider")
            .field("embedder_dimensions", &self.embedder.dimensions())
            .field("model_path", &self.model_path)
            .field("indexed_notes", &self.indexed_note_count())
            .finish()
    }
}

impl Default for OnnxSearchProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OnnxSearchProvider {
    /// Provider using the built-in 256-dimensional pure-Rust embedder.
    pub fn new() -> Self {
        Self {
            embedder: Arc::new(HashSemanticEmbedder::new()),
            model_path: None,
            state: RwLock::new(IndexState::default()),
        }
    }

    /// Swap in a custom embedder (e.g. an `ort` ONNX session). Notes already in
    /// the index are re-embedded from their stored source text.
    pub fn with_embedder(mut self, embedder: Arc<dyn TextEmbedder>) -> Self {
        let notes: Vec<Note> = {
            let mut state = self.write_state();
            let notes = state.docs.values().map(|doc| doc.note.clone()).collect();
            *state = IndexState::default();
            notes
        };
        self.embedder = embedder;
        for note in &notes {
            self.insert_note(note);
        }
        self
    }

    /// Record where the ONNX model would live (metadata only; no download).
    pub fn with_model_path(mut self, model_path: impl Into<PathBuf>) -> Self {
        self.model_path = Some(model_path.into());
        self
    }

    /// Path to the configured ONNX model, if any.
    pub fn model_path(&self) -> Option<&Path> {
        self.model_path.as_deref()
    }

    /// Width of vectors produced by the active embedder.
    pub fn embedder_dimensions(&self) -> usize {
        self.embedder.dimensions()
    }

    /// Number of notes currently held in the index (including private notes).
    pub fn indexed_note_count(&self) -> usize {
        self.read_state().docs.len()
    }

    fn read_state(&self) -> RwLockReadGuard<'_, IndexState> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_state(&self) -> RwLockWriteGuard<'_, IndexState> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Embed and store one note, then refresh the link graph.
    fn insert_note(&self, note: &Note) {
        let doc = IndexedNote::from_note(note, self.embedder.as_ref());
        let mut state = self.write_state();
        state.docs.insert(doc.id().clone(), doc);
        state.rebuild_graph();
    }

    /// Empty-query results: most recently updated visible notes first.
    fn empty_query_hits(
        &self,
        state: &IndexState,
        visible: &[&IndexedNote],
        limit: usize,
        ctx: ReadContext,
    ) -> Vec<SearchHit> {
        let mut docs: Vec<&IndexedNote> = visible.to_vec();
        docs.sort_by(|a, b| {
            b.updated_at_millis
                .cmp(&a.updated_at_millis)
                .then_with(|| {
                    state
                        .visible_in_degree(&b.note.id, ctx)
                        .cmp(&state.visible_in_degree(&a.note.id, ctx))
                })
                .then_with(|| a.note.title.cmp(&b.note.title))
                .then_with(|| a.note.id.as_str().cmp(b.note.id.as_str()))
        });
        docs.truncate(limit);

        let denominator = docs.len().max(1) as f32;
        docs.into_iter()
            .enumerate()
            .map(|(rank, doc)| SearchHit {
                note_id: doc.note.id.clone(),
                slug: doc.note.slug.clone(),
                title: doc.note.title.clone(),
                snippet: make_snippet(&doc.body, &[], &[]),
                score: (1.0 - rank as f32 / (denominator + 1.0)).max(0.01),
                match_kind: MatchKind::Keyword,
            })
            .collect()
    }
}

#[async_trait]
impl SearchProvider for OnnxSearchProvider {
    fn manifest(&self) -> ModuleManifest {
        MANIFEST
    }

    fn capabilities(&self) -> SearchCapabilities {
        SearchCapabilities {
            keyword: true,
            topical_semantic: true,
            graph_boosted: true,
            instant_as_you_type: true,
        }
    }

    async fn index_note(&self, note: &Note) -> Result<(), SearchError> {
        self.insert_note(note);
        Ok(())
    }

    async fn remove_note(&self, id: &NoteId) -> Result<(), SearchError> {
        let mut state = self.write_state();
        state.docs.remove(id);
        state.rebuild_graph();
        Ok(())
    }

    async fn reindex_all(&self, notes: &[Note]) -> Result<(), SearchError> {
        {
            let mut state = self.write_state();
            *state = IndexState::default();
        }
        for note in notes {
            self.insert_note(note);
        }
        Ok(())
    }

    async fn search(
        &self,
        query: &SearchQuery,
        ctx: ReadContext,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if query.limit == 0 {
            return Ok(Vec::new());
        }

        let state = self.read_state();

        // DEC-02 privacy gate: remove private notes BEFORE any scoring,
        // graph boosting, or limiting happens.
        let visible: Vec<&IndexedNote> = state
            .docs
            .values()
            .filter(|doc| ctx.include_private || !doc.private)
            .collect();

        if visible.is_empty() {
            return Ok(Vec::new());
        }

        let query_text = query.text.trim();
        if query_text.is_empty() {
            return Ok(self.empty_query_hits(&state, &visible, query.limit, ctx));
        }

        let tokens = tokenize(query_text);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let stems = query_stems(&tokens);
        let full_query = normalize_name(query_text);
        let query_vector = self.embedder.embed(query_text);

        let total_docs = visible.len() as f32;
        let average_len = {
            let total: usize = visible.iter().map(|doc| doc.body_len).sum();
            (total as f32 / total_docs).max(1.0)
        };

        // Document frequency / IDF over visible documents only, so private
        // notes never influence public ranking.
        let idf: HashMap<String, f32> = tokens
            .iter()
            .map(|token| {
                let df = visible
                    .iter()
                    .filter(|doc| doc.body_terms.contains_key(token))
                    .count() as f32;
                let value = (1.0 + (total_docs - df + 0.5) / (df + 0.5)).ln();
                (token.clone(), value)
            })
            .collect();

        // A context note may only boost results when it is visible to this
        // reader.
        let context_id = query
            .context_note_id
            .as_ref()
            .filter(|id| state.is_visible_to(id, ctx));

        let mut scored: Vec<(&IndexedNote, ScoreParts)> = visible
            .iter()
            .copied()
            .filter_map(|doc| {
                let (lexical_raw, exact_lexical) =
                    lexical_score(doc, &tokens, &stems, &full_query, &idf, average_len);
                let semantic = semantic_score(query_vector.as_slice(), doc);
                let lexical = 1.0 - (-lexical_raw).exp();

                if lexical_raw <= 0.0 && semantic < TOPICAL_SEMANTIC_MIN {
                    return None;
                }

                let mut graph =
                    IN_DEGREE_WEIGHT * (1.0 + state.visible_in_degree(doc.id(), ctx) as f32).ln();
                if let Some(context) = context_id
                    && state.are_neighbors(context, doc.id())
                {
                    graph += NEIGHBOR_BOOST;
                }

                let combined = LEXICAL_WEIGHT * lexical + SEMANTIC_WEIGHT * semantic + graph;
                Some((
                    doc,
                    ScoreParts {
                        exact_lexical,
                        semantic,
                        combined,
                    },
                ))
            })
            .collect();

        scored.sort_by(|(doc_a, part_a), (doc_b, part_b)| {
            part_b
                .combined
                .partial_cmp(&part_a.combined)
                .unwrap_or(Ordering::Equal)
                .then_with(|| doc_a.note.title.cmp(&doc_b.note.title))
                .then_with(|| doc_a.note.id.as_str().cmp(doc_b.note.id.as_str()))
        });

        scored.truncate(query.limit);

        let max_score = scored
            .iter()
            .map(|(_, parts)| parts.combined)
            .fold(0.0_f32, f32::max);

        Ok(scored
            .into_iter()
            .map(|(doc, parts)| {
                let match_kind = if parts.semantic >= TOPICAL_SEMANTIC_MIN {
                    if parts.exact_lexical {
                        MatchKind::Hybrid
                    } else {
                        MatchKind::Topical
                    }
                } else {
                    MatchKind::Keyword
                };

                SearchHit {
                    note_id: doc.note.id.clone(),
                    slug: doc.note.slug.clone(),
                    title: doc.note.title.clone(),
                    snippet: make_snippet(&doc.body, &tokens, &stems),
                    score: if max_score > 0.0 {
                        (parts.combined / max_score).clamp(0.0, 1.0)
                    } else {
                        0.0
                    },
                    match_kind,
                }
            })
            .collect())
    }
}

#[async_trait]
impl NoteLifecycleHook for OnnxSearchProvider {
    fn id(&self) -> &'static str {
        "zcast-search-onnx"
    }

    async fn on_event(&self, event: &NoteEvent) {
        match event {
            NoteEvent::Saved(note) | NoteEvent::Renamed { note, .. } => {
                let _ = self.index_note(note).await;
            }
            NoteEvent::Deleted(id) => {
                let _ = self.remove_note(id).await;
            }
        }
    }
}

/// Weighted lexical and semantic components for one candidate.
#[derive(Debug, Clone, Copy)]
struct ScoreParts {
    exact_lexical: bool,
    semantic: f32,
    combined: f32,
}

/// Blended title/document cosine similarity in `0.0..=1.0`.
fn semantic_score(query_vector: &[f32], doc: &IndexedNote) -> f32 {
    let title = cosine(query_vector, &doc.title_vector).max(0.0);
    let document = cosine(query_vector, &doc.embedding).max(0.0);
    (TITLE_SEMANTIC_WEIGHT * title + DOCUMENT_SEMANTIC_WEIGHT * document).clamp(0.0, 1.0)
}

/// Lexical match outcome for a single field.
#[derive(Debug, Clone, Copy)]
struct FieldMatch {
    score: f32,
    /// Literal query-token equality (not prefix/stem) was found.
    exact: bool,
}

impl FieldMatch {
    const NONE: Self = Self {
        score: 0.0,
        exact: false,
    };
}

#[derive(Debug, Clone, Copy)]
struct FieldWeights {
    exact: f32,
    prefix: f32,
    stem: f32,
}

/// Sum the lexical evidence for one query, plus whether any literal token
/// overlap occurred.
fn lexical_score(
    doc: &IndexedNote,
    tokens: &[String],
    stems: &[String],
    full_query: &str,
    idf: &HashMap<String, f32>,
    average_len: f32,
) -> (f32, bool) {
    let mut score = 0.0_f32;
    let mut exact = false;

    if !full_query.is_empty() {
        if doc.title_norm == full_query {
            score += TITLE_FULL_EXACT;
            exact = true;
        } else if doc.title_norm.starts_with(full_query) {
            score += TITLE_FULL_PREFIX;
        }
        if doc.alias_norms.iter().any(|alias| alias == full_query) {
            score += ALIAS_FULL_EXACT;
            exact = true;
        }
        if doc.slug_norm == full_query {
            score += SLUG_FULL_EXACT;
            exact = true;
        }
    }

    for token in tokens {
        let title = field_match(
            &doc.title_tokens,
            &doc.title_stems,
            token,
            stems,
            FieldWeights {
                exact: TITLE_TOKEN_EXACT,
                prefix: TITLE_TOKEN_PREFIX,
                stem: TITLE_TOKEN_STEM,
            },
        );
        let alias = field_match(
            &doc.alias_tokens,
            &doc.alias_stems,
            token,
            stems,
            FieldWeights {
                exact: ALIAS_TOKEN_EXACT,
                prefix: ALIAS_TOKEN_PREFIX,
                stem: ALIAS_TOKEN_STEM,
            },
        );
        let slug = field_match(
            &doc.slug_tokens,
            &doc.slug_stems,
            token,
            stems,
            FieldWeights {
                exact: SLUG_TOKEN_EXACT,
                prefix: SLUG_TOKEN_PREFIX,
                stem: SLUG_TOKEN_STEM,
            },
        );
        let token_idf = idf.get(token).copied().unwrap_or(0.0);
        let body = body_match(doc, token, stems, token_idf, average_len);

        score += title.score + alias.score + slug.score + body.score;
        exact |= title.exact || alias.exact || slug.exact || body.exact;
    }

    (score, exact)
}

fn field_match(
    tokens: &[String],
    stems: &HashSet<String>,
    query_token: &str,
    query_stems: &[String],
    weights: FieldWeights,
) -> FieldMatch {
    if tokens.iter().any(|token| token == query_token) {
        return FieldMatch {
            score: weights.exact,
            exact: true,
        };
    }
    if tokens.iter().any(|token| token.starts_with(query_token)) {
        return FieldMatch {
            score: weights.prefix,
            exact: false,
        };
    }
    if query_stems
        .iter()
        .any(|stem| stem.as_str() != query_token && stems.contains(stem.as_str()))
    {
        return FieldMatch {
            score: weights.stem,
            exact: false,
        };
    }
    FieldMatch::NONE
}

/// BM25-flavoured body match with exact, stem, and prefix contributions.
fn body_match(
    doc: &IndexedNote,
    query_token: &str,
    query_stems: &[String],
    idf: f32,
    average_len: f32,
) -> FieldMatch {
    let exact_tf = doc.body_terms.get(query_token).copied().unwrap_or(0) as f32;
    let stem_tf: f32 = query_stems
        .iter()
        .filter(|stem| stem.as_str() != query_token)
        .map(|stem| doc.body_stem_terms.get(stem).copied().unwrap_or(0) as f32)
        .sum();
    let prefix_tf: f32 = doc
        .body_terms
        .iter()
        .filter(|(token, _)| token.as_str() != query_token && token.starts_with(query_token))
        .map(|(_, count)| *count as f32)
        .sum();

    if exact_tf == 0.0 && stem_tf == 0.0 && prefix_tf == 0.0 {
        return FieldMatch::NONE;
    }

    let length_norm = 1.0 - BM25_B + BM25_B * (doc.body_len as f32 / average_len.max(1.0));
    let saturate = |tf: f32| (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * length_norm).max(1e-6);

    let mut score = 0.0_f32;
    if exact_tf > 0.0 {
        score += BODY_EXACT_WEIGHT * idf * saturate(exact_tf);
    }
    if stem_tf > 0.0 {
        score += BODY_STEM_WEIGHT * idf * saturate(stem_tf);
    }
    if prefix_tf > 0.0 {
        score += BODY_PREFIX_WEIGHT * idf * saturate(prefix_tf);
    }

    FieldMatch {
        score,
        exact: exact_tf > 0.0,
    }
}
