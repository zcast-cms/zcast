//! Pure-Rust full-text and graph-boosted search provider for zcast.
//!
//! Implements [`SearchProvider`] (`zcast:search/provider@1.0.0`) and
//! [`NoteLifecycleHook`] on top of a concurrent in-memory index. There are no
//! native dependencies, so the crate builds cleanly for static `musl` targets.
//!
//! Privacy is enforced inside the provider (DEC-02): private notes are removed
//! before scoring, limiting, and graph boosting, so they can never leak through
//! a public query. Incoming-link counts used for boosting only consider visible
//! referrers on public reads.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use async_trait::async_trait;
use zcast_interfaces::{
    IFACE_SEARCH, InterfaceSpec, MatchKind, ModuleManifest, Note, NoteEvent, NoteId,
    NoteLifecycleHook, ReadContext, SearchCapabilities, SearchError, SearchHit, SearchProvider,
    SearchQuery,
};

/// Maximum number of characters in a generated snippet (including ellipses).
const SNIPPET_CHARS: usize = 140;

const W_TITLE_EXACT: f32 = 1_000.0;
const W_TITLE_WHOLE_PREFIX: f32 = 350.0;
const W_ALIAS_WHOLE: f32 = 40.0;
const W_SLUG_WHOLE: f32 = 30.0;
const W_TITLE_TOKEN: f32 = 90.0;
const W_TITLE_TOKEN_PREFIX: f32 = 45.0;
const W_ALIAS_TOKEN: f32 = 42.0;
const W_ALIAS_TOKEN_PREFIX: f32 = 21.0;
const W_SLUG_TOKEN: f32 = 34.0;
const W_SLUG_TOKEN_PREFIX: f32 = 17.0;
const W_NEIGHBOR: f32 = 55.0;
const W_IN_DEGREE: f32 = 6.0;

const PROVIDES: &[InterfaceSpec] = &[InterfaceSpec {
    name: IFACE_SEARCH,
    version: "1.0.0",
}];

/// A single indexed note plus all derived token/link structures.
#[derive(Debug, Clone)]
struct IndexedNote {
    id: NoteId,
    slug: String,
    title: String,
    private: bool,
    updated_nanos: i64,
    normalized_title: String,
    normalized_slug: String,
    /// Normalized alias titles and slugs (lowercase, separator-folded).
    aliases: Vec<String>,
    /// Clean plain-text body used for snippets and tokenization.
    body: String,
    title_tokens: Vec<String>,
    alias_tokens: Vec<String>,
    slug_tokens: Vec<String>,
    body_tokens: HashMap<String, u32>,
    body_len: usize,
    /// Normalized outgoing wikilink targets (case-insensitive).
    outgoing_targets: HashSet<String>,
}

impl IndexedNote {
    fn from_note(note: &Note) -> Self {
        let body = clean_markdown(&note.markdown_body);
        let body_token_list = tokenize(&body);
        let mut body_tokens: HashMap<String, u32> = HashMap::new();
        for token in &body_token_list {
            *body_tokens.entry(token.clone()).or_insert(0) += 1;
        }

        let mut title_tokens = tokenize(&note.title);
        title_tokens.sort();
        title_tokens.dedup();

        let mut slug_tokens = tokenize(&note.slug);
        slug_tokens.sort();
        slug_tokens.dedup();

        let mut aliases = Vec::with_capacity(note.aliases.len() * 2);
        let mut alias_tokens = Vec::with_capacity(note.aliases.len() * 4);
        for alias in &note.aliases {
            let title_key = normalize_key(&alias.title);
            if !title_key.is_empty() {
                aliases.push(title_key);
            }
            let slug_key = normalize_key(&alias.slug);
            if !slug_key.is_empty() {
                aliases.push(slug_key);
            }
            alias_tokens.extend(tokenize(&alias.title));
            alias_tokens.extend(tokenize(&alias.slug));
        }
        aliases.sort();
        aliases.dedup();
        alias_tokens.sort();
        alias_tokens.dedup();

        Self {
            id: note.id.clone(),
            slug: note.slug.clone(),
            title: note.title.clone(),
            private: note.private,
            updated_nanos: note
                .updated_at
                .timestamp_nanos_opt()
                .unwrap_or_else(|| note.updated_at.timestamp_millis() * 1_000_000),
            normalized_title: normalize_key(&note.title),
            normalized_slug: normalize_key(&note.slug),
            aliases,
            body,
            title_tokens,
            alias_tokens,
            slug_tokens,
            body_tokens,
            body_len: body_token_list.len(),
            outgoing_targets: extract_outgoing_targets(&note.markdown_body),
        }
    }

    fn link_keys(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.normalized_title.as_str())
            .chain(std::iter::once(self.normalized_slug.as_str()))
            .chain(self.aliases.iter().map(String::as_str))
            .filter(|key| !key.is_empty())
    }
}

/// In-memory index and link graph protected by a single read/write lock.
#[derive(Debug, Default)]
struct IndexState {
    docs: HashMap<NoteId, IndexedNote>,
    /// Resolved directed edges: source note -> linked note ids.
    outgoing: HashMap<NoteId, HashSet<NoteId>>,
    /// Reverse edges: target note -> referring note ids.
    incoming: HashMap<NoteId, HashSet<NoteId>>,
}

impl IndexState {
    fn insert(&mut self, note: &Note) {
        self.docs
            .insert(note.id.clone(), IndexedNote::from_note(note));
        self.rebuild_graph();
    }

    fn remove(&mut self, id: &NoteId) {
        if self.docs.remove(id).is_some() {
            self.rebuild_graph();
        }
    }

    fn replace_all(&mut self, notes: &[Note]) {
        self.docs.clear();
        self.outgoing.clear();
        self.incoming.clear();
        for note in notes {
            self.docs
                .insert(note.id.clone(), IndexedNote::from_note(note));
        }
        self.rebuild_graph();
    }

    /// Resolve outgoing wikilink targets against all indexed titles, slugs,
    /// and aliases, then derive reverse edges for in-degree boosting.
    fn rebuild_graph(&mut self) {
        let mut key_index: HashMap<String, HashSet<NoteId>> = HashMap::new();
        for (id, doc) in &self.docs {
            for key in doc.link_keys() {
                key_index
                    .entry(key.to_owned())
                    .or_default()
                    .insert(id.clone());
            }
        }

        let mut outgoing: HashMap<NoteId, HashSet<NoteId>> = HashMap::new();
        let mut incoming: HashMap<NoteId, HashSet<NoteId>> = HashMap::new();

        for (id, doc) in &self.docs {
            let mut targets = HashSet::new();
            for target in &doc.outgoing_targets {
                if let Some(linked) = key_index.get(target) {
                    for target_id in linked {
                        if target_id != id {
                            targets.insert(target_id.clone());
                        }
                    }
                }
            }
            if targets.is_empty() {
                continue;
            }
            for target_id in &targets {
                incoming
                    .entry(target_id.clone())
                    .or_default()
                    .insert(id.clone());
            }
            outgoing.insert(id.clone(), targets);
        }

        self.outgoing = outgoing;
        self.incoming = incoming;
    }
}

/// Pure-Rust instant full-text and graph-boosted search provider.
#[derive(Debug, Default)]
pub struct FtsSearchProvider {
    state: RwLock<IndexState>,
}

impl FtsSearchProvider {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(IndexState::default()),
        }
    }

    fn read_state(&self) -> Result<std::sync::RwLockReadGuard<'_, IndexState>, SearchError> {
        self.state
            .read()
            .map_err(|_| SearchError::Index("search index lock poisoned".to_owned()))
    }

    fn write_state(&self) -> Result<std::sync::RwLockWriteGuard<'_, IndexState>, SearchError> {
        self.state
            .write()
            .map_err(|_| SearchError::Index("search index lock poisoned".to_owned()))
    }
}

#[async_trait]
impl SearchProvider for FtsSearchProvider {
    fn manifest(&self) -> ModuleManifest {
        ModuleManifest {
            id: "zcast-search-fts",
            name: "Full-Text Search",
            version: env!("CARGO_PKG_VERSION"),
            description: "Pure-Rust instant full-text and graph-boosted search provider",
            provides: PROVIDES,
            requires: &[],
            recommends: &[],
        }
    }

    fn capabilities(&self) -> SearchCapabilities {
        SearchCapabilities {
            keyword: true,
            topical_semantic: false,
            graph_boosted: true,
            instant_as_you_type: true,
        }
    }

    async fn index_note(&self, note: &Note) -> Result<(), SearchError> {
        let mut state = self.write_state()?;
        state.insert(note);
        Ok(())
    }

    async fn remove_note(&self, id: &NoteId) -> Result<(), SearchError> {
        let mut state = self.write_state()?;
        state.remove(id);
        Ok(())
    }

    async fn reindex_all(&self, notes: &[Note]) -> Result<(), SearchError> {
        let mut state = self.write_state()?;
        state.replace_all(notes);
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

        let state = self.read_state()?;

        // DEC-02 privacy gate: filter before scoring and before applying limit.
        let visible: Vec<&IndexedNote> = state
            .docs
            .values()
            .filter(|doc| ctx.include_private || !doc.private)
            .collect();

        if visible.is_empty() {
            return Ok(Vec::new());
        }

        let visible_ids: HashSet<&str> = visible.iter().map(|doc| doc.id.as_str()).collect();

        if query.text.trim().is_empty() {
            return Ok(empty_query_hits(
                &state,
                &visible,
                &visible_ids,
                query.limit,
            ));
        }

        let tokens = tokenize(&query.text);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let normalized_query = normalize_key(&query.text);

        // Document frequencies and average body length for the BM25-like body
        // score, computed over visible notes only.
        let mut doc_freq: HashMap<&str, u32> = HashMap::new();
        for doc in &visible {
            for token in doc.body_tokens.keys() {
                *doc_freq.entry(token.as_str()).or_insert(0) += 1;
            }
        }
        let doc_count = visible.len() as f32;
        let avg_body_len = visible.iter().map(|doc| doc.body_len).sum::<usize>().max(1) as f32
            / doc_count.max(1.0);

        let context_neighbors = context_neighbors(&state, query, ctx, &visible_ids);

        let mut all_matches: Vec<(&IndexedNote, f32)> = Vec::new();
        let mut partial_matches: Vec<(&IndexedNote, f32)> = Vec::new();

        for doc in &visible {
            let mut score = 0.0_f32;

            if !normalized_query.is_empty() {
                if doc.normalized_title == normalized_query {
                    score += W_TITLE_EXACT;
                } else if doc.normalized_title.starts_with(&normalized_query) {
                    score += W_TITLE_WHOLE_PREFIX;
                }
                if doc.normalized_slug == normalized_query {
                    score += W_SLUG_WHOLE;
                }
                if doc.aliases.iter().any(|alias| alias == &normalized_query) {
                    score += W_ALIAS_WHOLE;
                }
            }

            let mut matched = 0_usize;
            for token in &tokens {
                let title_score = token_field_score(
                    &doc.title_tokens,
                    token,
                    W_TITLE_TOKEN,
                    W_TITLE_TOKEN_PREFIX,
                );
                let alias_score = token_field_score(
                    &doc.alias_tokens,
                    token,
                    W_ALIAS_TOKEN,
                    W_ALIAS_TOKEN_PREFIX,
                );
                let slug_score =
                    token_field_score(&doc.slug_tokens, token, W_SLUG_TOKEN, W_SLUG_TOKEN_PREFIX);
                let body_score = body_token_score(doc, token, &doc_freq, doc_count, avg_body_len);

                let token_score = title_score + alias_score + slug_score + body_score;
                if token_score > 0.0 {
                    score += token_score;
                    matched += 1;
                }
            }

            if matched == 0 {
                continue;
            }

            let indegree = visible_in_degree(&state, &visible_ids, &doc.id);
            if indegree > 0 {
                score += W_IN_DEGREE * (indegree as f32 + 1.0).ln();
            }
            if context_neighbors.contains(doc.id.as_str()) {
                score += W_NEIGHBOR;
            }

            if matched == tokens.len() {
                all_matches.push((doc, score));
            } else {
                // OR fallback: fewer matched terms rank lower.
                let scaled = score * (matched as f32 / tokens.len() as f32);
                partial_matches.push((doc, scaled));
            }
        }

        let mut candidates = if all_matches.is_empty() {
            partial_matches
        } else {
            all_matches
        };

        candidates.sort_by(|(left, left_score), (right, right_score)| {
            right_score
                .total_cmp(left_score)
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        candidates.truncate(query.limit);

        Ok(candidates
            .into_iter()
            .map(|(doc, score)| SearchHit {
                note_id: doc.id.clone(),
                slug: doc.slug.clone(),
                title: doc.title.clone(),
                snippet: doc_snippet(doc, &tokens),
                score,
                match_kind: MatchKind::Keyword,
            })
            .collect())
    }
}

#[async_trait]
impl NoteLifecycleHook for FtsSearchProvider {
    fn id(&self) -> &'static str {
        "zcast-search-fts"
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

fn empty_query_hits(
    state: &IndexState,
    visible: &[&IndexedNote],
    visible_ids: &HashSet<&str>,
    limit: usize,
) -> Vec<SearchHit> {
    let mut ranked: Vec<(&IndexedNote, usize)> = visible
        .iter()
        .map(|doc| (*doc, visible_in_degree(state, visible_ids, &doc.id)))
        .collect();

    ranked.sort_by(|(left, left_indegree), (right, right_indegree)| {
        right
            .updated_nanos
            .cmp(&left.updated_nanos)
            .then_with(|| right_indegree.cmp(left_indegree))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });
    ranked.truncate(limit);

    ranked
        .into_iter()
        .map(|(doc, indegree)| SearchHit {
            note_id: doc.id.clone(),
            slug: doc.slug.clone(),
            title: doc.title.clone(),
            snippet: doc_snippet(doc, &[]),
            score: 1.0 + (indegree as f32).ln_1p() * 0.01,
            match_kind: MatchKind::Keyword,
        })
        .collect()
}

fn visible_in_degree(state: &IndexState, visible_ids: &HashSet<&str>, id: &NoteId) -> usize {
    state
        .incoming
        .get(id)
        .map(|referrers| {
            referrers
                .iter()
                .filter(|referrer| visible_ids.contains(referrer.as_str()))
                .count()
        })
        .unwrap_or(0)
}

/// Direct neighbours (incoming and outgoing) of the active note that are also
/// visible under this read context. A hidden context note grants no boost.
fn context_neighbors<'a>(
    state: &'a IndexState,
    query: &SearchQuery,
    ctx: ReadContext,
    visible_ids: &HashSet<&'a str>,
) -> HashSet<&'a str> {
    let Some(context_id) = &query.context_note_id else {
        return HashSet::new();
    };
    let Some(context_doc) = state.docs.get(context_id) else {
        return HashSet::new();
    };
    if context_doc.private && !ctx.include_private {
        return HashSet::new();
    }

    let mut neighbors = HashSet::new();
    if let Some(outgoing) = state.outgoing.get(context_id) {
        for neighbor in outgoing {
            if visible_ids.contains(neighbor.as_str()) {
                neighbors.insert(neighbor.as_str());
            }
        }
    }
    if let Some(incoming) = state.incoming.get(context_id) {
        for neighbor in incoming {
            if visible_ids.contains(neighbor.as_str()) {
                neighbors.insert(neighbor.as_str());
            }
        }
    }
    neighbors
}

fn token_field_score(tokens: &[String], query: &str, exact: f32, prefix: f32) -> f32 {
    let mut best = 0.0_f32;
    for token in tokens {
        if token == query {
            return exact;
        }
        if token.starts_with(query) && prefix > best {
            best = prefix;
        }
    }
    best
}

/// BM25-like body score. Exact token matches outrank prefix-only matches, so
/// instant as-you-type queries still prefer complete words.
fn body_token_score(
    doc: &IndexedNote,
    query: &str,
    doc_freq: &HashMap<&str, u32>,
    doc_count: f32,
    avg_body_len: f32,
) -> f32 {
    let mut best = 0.0_f32;
    if let Some(freq) = doc.body_tokens.get(query) {
        best = bm25(
            *freq,
            query,
            doc_freq,
            doc_count,
            doc.body_len,
            avg_body_len,
        );
    }
    for (token, freq) in &doc.body_tokens {
        if token == query || !token.starts_with(query) {
            continue;
        }
        let score = 0.6
            * bm25(
                *freq,
                token,
                doc_freq,
                doc_count,
                doc.body_len,
                avg_body_len,
            );
        if score > best {
            best = score;
        }
    }
    best
}

fn bm25(
    term_freq: u32,
    token: &str,
    doc_freq: &HashMap<&str, u32>,
    doc_count: f32,
    body_len: usize,
    avg_body_len: f32,
) -> f32 {
    let frequency = doc_freq.get(token).copied().unwrap_or(0) as f32;
    let idf = (1.0 + (doc_count - frequency + 0.5) / (frequency + 0.5)).ln();
    let k1 = 1.2_f32;
    let b = 0.75_f32;
    let length_norm = 1.0 - b + b * (body_len as f32 / avg_body_len.max(1.0));
    let tf = term_freq as f32;
    idf * (tf * (k1 + 1.0)) / (tf + k1 * length_norm)
}

fn doc_snippet(doc: &IndexedNote, tokens: &[String]) -> String {
    if doc.body.is_empty() {
        return doc.title.clone();
    }
    make_snippet(&doc.body, tokens)
}

/// Build a plain-text snippet centred on the earliest matching query token.
/// Returns an opening excerpt when the query only matched the title.
fn make_snippet(body: &str, tokens: &[String]) -> String {
    let chars: Vec<char> = body.chars().collect();
    let char_count = chars.len();
    if char_count == 0 {
        return String::new();
    }
    if char_count <= SNIPPET_CHARS {
        return chars.into_iter().collect::<String>().trim().to_owned();
    }

    let match_span = earliest_match_span(body, tokens);
    let (match_start, match_len) = match_span.unwrap_or((0, 0));

    // Reserve room for both possible ellipses so the result never exceeds 140
    // characters even when the match sits in the middle of a long body.
    let window = SNIPPET_CHARS.saturating_sub(2).max(1);
    let before = window.saturating_sub(match_len.min(window)) / 2;
    let mut start = match_start.saturating_sub(before);
    let mut end = (start + window).min(char_count);
    if end.saturating_sub(start) < window {
        start = end.saturating_sub(window);
        end = (start + window).min(char_count);
    }

    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(chars[start..end].iter());
    if end < char_count {
        snippet.push('…');
    }
    snippet
}

/// Locate the earliest body token that exactly matches or prefixes a query
/// token, returning its character start and character length.
fn earliest_match_span(body: &str, tokens: &[String]) -> Option<(usize, usize)> {
    if tokens.is_empty() {
        return None;
    }

    let mut token = String::new();
    let mut token_start = 0_usize;
    let mut token_len = 0_usize;

    for (index, ch) in body.chars().enumerate() {
        if ch.is_alphanumeric() {
            if token.is_empty() {
                token_start = index;
            }
            token.extend(ch.to_lowercase());
            token_len += 1;
        } else if !token.is_empty() {
            if token_matches_any(&token, tokens) {
                return Some((token_start, token_len));
            }
            token.clear();
            token_len = 0;
        }
    }

    if !token.is_empty() && token_matches_any(&token, tokens) {
        return Some((token_start, token_len));
    }
    None
}

fn token_matches_any(token: &str, queries: &[String]) -> bool {
    queries
        .iter()
        .any(|query| token == query || token.starts_with(query))
}

/// Fold text into lowercase alphanumeric words separated by single spaces.
/// Titles, slugs, aliases, and wikilink targets all use this same normal form,
/// which makes `[[My Note]]` resolve against slug `my-note`.
fn normalize_key(input: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;

    for ch in input.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

fn tokenize(input: &str) -> Vec<String> {
    normalize_key(input)
        .split(' ')
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Extract case-insensitive, normalized wikilink targets from Markdown,
/// ignoring fenced and inline code.
fn extract_outgoing_targets(markdown: &str) -> HashSet<String> {
    let mut targets = HashSet::new();
    let mut in_fence = false;

    for line in markdown.lines() {
        let trimmed_start = line.trim_start();
        if trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        for inner in scan_wikilinks(line) {
            let target = wikilink_target(inner);
            let key = normalize_key(target);
            if !key.is_empty() {
                targets.insert(key);
            }
        }
    }
    targets
}

/// Return the raw inner text of every `[[...]]` on a single line, skipping
/// inline code spans.
fn scan_wikilinks(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut links = Vec::new();
    let mut index = 0_usize;
    let mut code_run = 0_usize;

    while index < bytes.len() {
        if bytes[index] == b'`' {
            let run = count_run(bytes, index, b'`');
            if code_run == 0 {
                code_run = run;
            } else if code_run == run {
                code_run = 0;
            }
            index += run;
            continue;
        }

        if code_run == 0
            && bytes[index] == b'['
            && bytes.get(index + 1) == Some(&b'[')
            && let Some(offset) = line[index + 2..].find("]]")
        {
            let end = index + 2 + offset;
            links.push(&line[index + 2..end]);
            index = end + 2;
            continue;
        }

        index += 1;
    }
    links
}

fn wikilink_target(inner: &str) -> &str {
    inner
        .split('|')
        .next()
        .unwrap_or(inner)
        .split('#')
        .next()
        .unwrap_or("")
        .trim()
}

fn wikilink_display(inner: &str) -> &str {
    if let Some((_, display)) = inner.split_once('|') {
        let display = display.trim();
        if !display.is_empty() {
            return display;
        }
    }
    let target = wikilink_target(inner);
    if target.is_empty() {
        inner.trim()
    } else {
        target
    }
}

/// Convert Markdown into clean plain text for tokenization and snippets.
fn clean_markdown(markdown: &str) -> String {
    let mut pieces: Vec<String> = Vec::new();
    let mut in_fence = false;

    for raw_line in markdown.lines() {
        let trimmed_start = raw_line.trim_start();
        if trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            let cleaned = collapse_whitespace(trimmed_start);
            if !cleaned.is_empty() {
                pieces.push(cleaned);
            }
            continue;
        }

        let mut line = trimmed_start.trim_end();
        while let Some(rest) = line.strip_prefix('>') {
            line = rest.trim_start();
        }

        let hashes = line.chars().take_while(|ch| *ch == '#').count();
        if (1..=6).contains(&hashes) {
            let rest = &line[hashes..];
            if rest.is_empty() || rest.starts_with(' ') {
                line = rest.trim_start();
            }
        }

        if is_horizontal_rule(line) || is_table_separator(line) {
            continue;
        }

        line = strip_list_marker(line);
        line = strip_task_marker(line);

        let cleaned = collapse_whitespace(&strip_inline(line));
        if !cleaned.is_empty() {
            pieces.push(cleaned);
        }
    }

    pieces.join(" ")
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let count = trimmed
        .chars()
        .filter(|ch| matches!(ch, '-' | '*' | '_'))
        .count();
    count >= 3
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '-' | '*' | '_' | ' '))
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|')
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
        && trimmed.contains('-')
}

fn strip_list_marker(line: &str) -> &str {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return rest.trim_start();
        }
    }

    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    line
}

fn strip_task_marker(line: &str) -> &str {
    for marker in ["[ ] ", "[x] ", "[X] "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    line
}

/// Strip inline Markdown constructs while preserving their human-readable
/// text: wikilinks, images, links, code spans, emphasis markers, and HTML tags.
fn strip_inline(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0_usize;

    while index < bytes.len() {
        let ch = input[index..].chars().next().unwrap_or('\0');

        match ch {
            '`' => {
                let run = count_run(bytes, index, b'`');
                if let Some(close) = find_run(bytes, index + run, b'`', run) {
                    out.push_str(&input[index + run..close]);
                    index = close + run;
                } else {
                    index += run;
                }
            }
            '!' if bytes.get(index + 1) == Some(&b'[') => {
                if let Some((label, end)) = parse_bracket_link(input, index + 1) {
                    out.push_str(&strip_inline(label));
                    index = end;
                } else {
                    index += ch.len_utf8();
                }
            }
            '[' => {
                if bytes.get(index + 1) == Some(&b'[')
                    && let Some(offset) = input[index + 2..].find("]]")
                {
                    let end = index + 2 + offset;
                    let inner = &input[index + 2..end];
                    out.push_str(&strip_inline(wikilink_display(inner)));
                    index = end + 2;
                    continue;
                }

                if let Some((label, end)) = parse_bracket_link(input, index) {
                    out.push_str(&strip_inline(label));
                    index = end;
                } else {
                    out.push(' ');
                    index += ch.len_utf8();
                }
            }
            '<' => {
                if let Some(offset) = input[index + 1..].find('>') {
                    index = index + 1 + offset + 1;
                    out.push(' ');
                } else {
                    out.push('<');
                    index += ch.len_utf8();
                }
            }
            '\\' => {
                index += ch.len_utf8();
            }
            '*' | '_' | '~' | '|' => {
                out.push(' ');
                index += ch.len_utf8();
            }
            _ => {
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }

    out
}

/// Parse `[label](target)` or `[label]` starting at `start` (the `[` byte).
/// Returns the label and the byte index just past the construct.
fn parse_bracket_link(input: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = input.as_bytes();
    let close_offset = input[start + 1..].find(']')?;
    let close = start + 1 + close_offset;
    let label = &input[start + 1..close];
    let after_label = close + 1;

    if bytes.get(after_label) == Some(&b'(')
        && let Some(paren_offset) = input[after_label + 1..].find(')')
    {
        return Some((label, after_label + 1 + paren_offset + 1));
    }

    Some((label, after_label))
}

fn count_run(bytes: &[u8], start: usize, needle: u8) -> usize {
    bytes[start..]
        .iter()
        .take_while(|byte| **byte == needle)
        .count()
}

fn find_run(bytes: &[u8], start: usize, needle: u8, len: usize) -> Option<usize> {
    let mut index = start;
    while index < bytes.len() {
        if bytes[index] == needle {
            let run = count_run(bytes, index, needle);
            if run == len {
                return Some(index);
            }
            index += run;
        } else {
            index += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use zcast_interfaces::NoteAlias;

    fn provider() -> FtsSearchProvider {
        FtsSearchProvider::new()
    }

    async fn index(provider: &FtsSearchProvider, notes: &[Note]) {
        provider.reindex_all(notes).await.expect("reindex");
    }

    async fn search(
        provider: &FtsSearchProvider,
        text: &str,
        limit: usize,
        context_note_id: Option<NoteId>,
        ctx: ReadContext,
    ) -> Vec<SearchHit> {
        provider
            .search(
                &SearchQuery {
                    text: text.to_owned(),
                    limit,
                    context_note_id,
                },
                ctx,
            )
            .await
            .expect("search")
    }

    fn titles(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.title.as_str()).collect()
    }

    #[test]
    fn manifest_and_capabilities_match_contract() {
        let provider = provider();
        assert_eq!(provider.manifest(), provider.manifest());
        let manifest = provider.manifest();
        assert_eq!(manifest.id, "zcast-search-fts");
        assert_eq!(manifest.name, "Full-Text Search");
        assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            manifest.description,
            "Pure-Rust instant full-text and graph-boosted search provider"
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
        assert!(!capabilities.topical_semantic);
        assert!(capabilities.graph_boosted);
        assert!(capabilities.instant_as_you_type);
    }

    #[tokio::test]
    async fn matches_exact_tokens_and_instant_prefixes() {
        let provider = provider();
        let title = Note::new("Rustacean Handbook", "Notes about ownership.", false);
        let body = Note::new("Other Topic", "A concatenation example.", false);
        index(&provider, &[title.clone(), body.clone()]).await;

        let hits = search(&provider, "rust", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Rustacean Handbook"]);

        let hits = search(&provider, "rusta", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Rustacean Handbook"]);

        let hits = search(&provider, "concat", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Other Topic"]);

        let hits = search(&provider, "missing", 10, None, ReadContext::PUBLIC).await;
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn title_matches_outrank_body_matches() {
        let provider = provider();
        let exact = Note::new("Rust", "Unrelated wording.", false);
        let prefix = Note::new("Rust Handbook", "More unrelated wording.", false);
        let body = Note::new("Other", "rust rust rust rust rust", false);
        index(&provider, &[body.clone(), prefix.clone(), exact.clone()]).await;

        let hits = search(&provider, "rust", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(
            titles(&hits),
            vec!["Rust", "Rust Handbook", "Other"],
            "exact title > title prefix > body"
        );
    }

    #[tokio::test]
    async fn alias_titles_and_slugs_are_searchable() {
        let provider = provider();
        let mut note = Note::new("Primary Name", "A note that was renamed.", false);
        note.aliases.push(NoteAlias {
            slug: "legacy-name".to_owned(),
            title: "Legacy Name".to_owned(),
            created_at: note.created_at,
        });
        index(&provider, &[note]).await;

        let hits = search(&provider, "legacy", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Primary Name"]);

        let hits = search(&provider, "legacy-name", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Primary Name"]);
    }

    #[tokio::test]
    async fn title_token_matches_outrank_alias_matches() {
        let provider = provider();
        let title_token = Note::new("The Legacy", "unrelated body", false);
        let mut alias_match = Note::new("Other", "unrelated body", false);
        alias_match.aliases.push(NoteAlias {
            slug: "legacy".to_owned(),
            title: "Legacy".to_owned(),
            created_at: alias_match.created_at,
        });
        index(&provider, &[alias_match, title_token]).await;

        let hits = search(&provider, "legacy", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["The Legacy", "Other"]);
    }

    #[tokio::test]
    async fn graph_neighbors_and_indegree_boost_ranking() {
        let provider = provider();
        let hub = Note::new("Hub", "See [[Neighbor]] for details.", false);
        let neighbor = Note::new("Neighbor", "needle alpha", false);
        let unrelated = Note::new("Unrelated", "needle alpha", false);
        index(
            &provider,
            &[hub.clone(), neighbor.clone(), unrelated.clone()],
        )
        .await;

        let hits = search(
            &provider,
            "needle",
            10,
            Some(hub.id.clone()),
            ReadContext::PUBLIC,
        )
        .await;
        assert_eq!(hits[0].note_id, neighbor.id);
        assert_eq!(hits[1].note_id, unrelated.id);
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn public_queries_never_expose_private_notes() {
        let provider = provider();
        let public = Note::new("Public Needle", "A public body.", false);
        let private = Note::new("Secret Needle", "classifiedmaterial", true);
        let hub = Note::new(
            "Hub",
            "Links [[Public Needle]] and [[Secret Needle]].",
            false,
        );
        index(&provider, &[public.clone(), private.clone(), hub.clone()]).await;

        let public_hits = search(&provider, "secret", 10, None, ReadContext::PUBLIC).await;
        assert!(
            !public_hits
                .iter()
                .any(|hit| hit.note_id == private.id || hit.title == private.title),
            "private note leaked into public search: {:?}",
            titles(&public_hits)
        );

        let public_only = search(
            &provider,
            "classifiedmaterial",
            10,
            None,
            ReadContext::PUBLIC,
        )
        .await;
        assert!(public_only.is_empty());

        let admin_only = search(
            &provider,
            "classifiedmaterial",
            10,
            None,
            ReadContext::ADMIN,
        )
        .await;
        assert_eq!(admin_only.len(), 1);
        assert_eq!(admin_only[0].note_id, private.id);
    }

    #[tokio::test]
    async fn public_graph_boost_ignores_private_referrers() {
        let provider = provider();
        let target_a = Note::new("Zulu", "zebra", false);
        let target_b = Note::new("Alpha", "zebra", false);
        let public_hub = Note::new("Public One", "[[Zulu]]", false);
        let other_public_hub = Note::new("Public Two", "[[Alpha]]", false);
        let private_hub = Note::new("Private One", "[[Zulu]]", true);
        index(
            &provider,
            &[
                target_a.clone(),
                target_b.clone(),
                public_hub.clone(),
                other_public_hub.clone(),
                private_hub.clone(),
            ],
        )
        .await;

        // Both targets have one visible referrer, so the title tie-break wins.
        let public_hits = search(&provider, "zebra", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&public_hits), vec!["Alpha", "Zulu"]);

        // Admin sees the extra private referrer, which lifts Zulu above Alpha.
        let admin_hits = search(&provider, "zebra", 10, None, ReadContext::ADMIN).await;
        assert_eq!(titles(&admin_hits), vec!["Zulu", "Alpha"]);
    }

    #[tokio::test]
    async fn index_remove_and_reindex_all_keep_index_consistent() {
        let provider = provider();
        let alpha = Note::new("Alpha", "alpha body", false);
        let beta = Note::new("Beta", "beta body", false);

        provider
            .index_note(&alpha)
            .await
            .expect("index single note");
        let hits = search(&provider, "alpha", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Alpha"]);

        provider.remove_note(&alpha.id).await.expect("remove note");
        let hits = search(&provider, "alpha", 10, None, ReadContext::PUBLIC).await;
        assert!(hits.is_empty());

        index(&provider, &[alpha.clone(), beta.clone()]).await;
        let hits = search(&provider, "beta", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Beta"]);

        index(&provider, std::slice::from_ref(&alpha)).await;
        let hits = search(&provider, "beta", 10, None, ReadContext::PUBLIC).await;
        assert!(
            hits.is_empty(),
            "reindex_all should replace the whole index"
        );
    }

    #[tokio::test]
    async fn lifecycle_hook_indexes_renames_and_deletes() {
        let provider = provider();
        let mut note = Note::new("Lifecycle Old", "old body text", false);
        provider.on_event(&NoteEvent::Saved(note.clone())).await;

        let hits = search(&provider, "old", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Lifecycle Old"]);

        let old_title = note.title.clone();
        let old_slug = note.slug.clone();
        note.title = "Lifecycle New".to_owned();
        note.slug = "lifecycle-new".to_owned();
        note.markdown_body = "fresh body text".to_owned();
        provider
            .on_event(&NoteEvent::Renamed {
                old_title,
                old_slug,
                note: note.clone(),
            })
            .await;

        let hits = search(&provider, "fresh", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Lifecycle New"]);
        let hits = search(&provider, "old", 10, None, ReadContext::PUBLIC).await;
        assert!(hits.is_empty(), "old body should have been replaced");

        provider
            .on_event(&NoteEvent::Deleted(note.id.clone()))
            .await;
        let hits = search(&provider, "fresh", 10, None, ReadContext::PUBLIC).await;
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn empty_query_returns_recent_notes_and_respects_limit() {
        let provider = provider();
        let older = Note::new("Older", "old body", false);
        std::thread::sleep(Duration::from_millis(2));
        let newer = Note::new("Newer", "new body", false);
        index(&provider, &[older.clone(), newer.clone()]).await;

        let hits = search(&provider, "", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].note_id, newer.id);
        assert_eq!(hits[0].match_kind, MatchKind::Keyword);
        assert!(!hits[0].snippet.is_empty());

        let hits = search(&provider, "   ", 1, None, ReadContext::PUBLIC).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].note_id, newer.id);

        let hits = search(&provider, "", 0, None, ReadContext::PUBLIC).await;
        assert!(hits.is_empty());

        let hits = search(&provider, "new", 0, None, ReadContext::PUBLIC).await;
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn multi_token_queries_use_and_then_or_fallback() {
        let provider = provider();
        let both = Note::new("Alpha Beta", "shared body", false);
        let only_alpha = Note::new("Alpha", "gamma body", false);
        index(&provider, &[both.clone(), only_alpha.clone()]).await;

        let hits = search(&provider, "alpha beta", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Alpha Beta"]);

        let hits = search(&provider, "alpha missing", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(hits.len(), 2, "OR fallback should return partial matches");
        assert!(hits.iter().all(|hit| hit.title.contains("Alpha")));
    }

    #[tokio::test]
    async fn snippets_are_centred_on_body_match_and_plain_text() {
        let provider = provider();
        let long_body = format!("{}needle{}", "filler ".repeat(40), " trailer".repeat(40));
        let note = Note::new("Snippet Test", long_body, false);
        index(&provider, &[note]).await;

        let hits = search(&provider, "needle", 10, None, ReadContext::PUBLIC).await;
        let snippet = &hits[0].snippet;
        assert!(snippet.contains("needle"));
        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert!(
            snippet.chars().count() <= SNIPPET_CHARS,
            "snippet too long: {} chars",
            snippet.chars().count()
        );
    }

    #[tokio::test]
    async fn title_only_match_uses_opening_body_snippet() {
        let provider = provider();
        let note = Note::new(
            "Unique Heading",
            "Plain opening body text with **markdown** formatting.",
            false,
        );
        index(&provider, &[note]).await;

        let hits = search(&provider, "unique", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].snippet,
            "Plain opening body text with markdown formatting."
        );
    }

    #[tokio::test]
    async fn markdown_is_cleaned_for_tokenization_and_snippets() {
        let provider = provider();
        let note = Note::new(
            "Markdown Note",
            "## **Bold** and [[Linked Page|the page]]\n\n- `code` item",
            false,
        );
        index(&provider, &[note]).await;

        let hits = search(&provider, "bold", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.contains("**"));
        assert!(!hits[0].snippet.contains("[["));
        assert!(hits[0].snippet.contains("Bold"));

        let hits = search(&provider, "page", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn equal_scores_break_ties_by_title_then_note_id() {
        let provider = provider();
        let beta = Note::new("Beta Note", "common keyword", false);
        let alpha = Note::new("Alpha Note", "common keyword", false);
        index(&provider, &[beta, alpha]).await;

        let hits = search(&provider, "common", 10, None, ReadContext::PUBLIC).await;
        assert_eq!(titles(&hits), vec!["Alpha Note", "Beta Note"]);
    }

    #[tokio::test]
    async fn outgoing_links_update_when_note_is_reindexed() {
        let provider = provider();
        let hub = Note::new("Hub", "[[First]]", false);
        let first = Note::new("First", "needle", false);
        let second = Note::new("Second", "needle", false);
        index(&provider, &[hub.clone(), first.clone(), second.clone()]).await;

        let hits = search(
            &provider,
            "needle",
            10,
            Some(hub.id.clone()),
            ReadContext::PUBLIC,
        )
        .await;
        assert_eq!(hits[0].note_id, first.id);

        let mut updated_hub = hub.clone();
        updated_hub.markdown_body = "[[Second]]".to_owned();
        provider
            .index_note(&updated_hub)
            .await
            .expect("reindex hub");

        let hits = search(
            &provider,
            "needle",
            10,
            Some(updated_hub.id.clone()),
            ReadContext::PUBLIC,
        )
        .await;
        assert_eq!(hits[0].note_id, second.id);
    }
}
