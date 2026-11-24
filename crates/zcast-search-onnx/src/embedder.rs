//! Pluggable dense text embeddings.
//!
//! [`TextEmbedder`] is the seam that keeps this search provider free of any
//! hard dependency on a C++ ONNX runtime: the default
//! [`HashSemanticEmbedder`] is pure Rust (so static `musl` builds and unit
//! tests work with zero model downloads), and an `ort`-backed embedder can be
//! dropped in through [`crate::OnnxSearchProvider::with_embedder`].

use std::collections::HashMap;

use crate::text::tokenize;

/// Default embedding width used by [`HashSemanticEmbedder`].
pub const DEFAULT_DIMENSIONS: usize = 256;

/// Total order not required: only dense cosine similarity is used.
///
/// Implementations MUST return `dimensions()` values and MUST L2-normalize the
/// result so `dot(a, b) == cosine(a, b)`. An all-zero vector is returned for
/// text with no usable features (it has no direction to normalise).
pub trait TextEmbedder: Send + Sync {
    /// Width of every vector returned by [`TextEmbedder::embed`].
    fn dimensions(&self) -> usize;

    /// Embed `text` into an L2-normalized dense vector.
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Scale `vector` to unit length in place. Zero-length or non-finite vectors
/// are left untouched.
pub fn normalize_l2(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm.is_finite() && norm > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

/// Dot product of two L2-normalized vectors (i.e. cosine similarity).
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Feature weights. Stems get slightly more weight than raw unigrams so
/// inflectional variants converge after hashing.
const UNIGRAM_WEIGHT: f32 = 1.0;
const STEM_WEIGHT: f32 = 1.15;
const BIGRAM_WEIGHT: f32 = 0.55;
const TRIGRAM_WEIGHT: f32 = 0.3;
const QUADGRAM_WEIGHT: f32 = 0.35;

/// Deterministic pure-Rust semantic embedder.
///
/// Text is decomposed into word unigrams, lightweight English morphological
/// stems, word bigrams, and character 3-/4-grams. Every feature is projected
/// into a fixed-width dense vector with signed FNV-1a feature hashing, then
/// L2-normalized. Shared vocabulary, shared stems, and shared sub-word
/// fragments all raise cosine similarity, which is what makes
/// `"architectural"` land on `"Architecture Blueprint"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashSemanticEmbedder {
    dimensions: usize,
}

impl HashSemanticEmbedder {
    /// 256-dimensional embedder (the default).
    pub fn new() -> Self {
        Self::default()
    }

    /// Embedder with a custom width. Zero falls back to
    /// [`DEFAULT_DIMENSIONS`].
    pub fn with_dimensions(dimensions: usize) -> Self {
        Self {
            dimensions: if dimensions == 0 {
                DEFAULT_DIMENSIONS
            } else {
                dimensions
            },
        }
    }
}

impl Default for HashSemanticEmbedder {
    fn default() -> Self {
        Self {
            dimensions: DEFAULT_DIMENSIONS,
        }
    }
}

impl TextEmbedder for HashSemanticEmbedder {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let dimensions = self.dimensions;
        let words = tokenize(text);
        if words.is_empty() {
            return vec![0.0; dimensions];
        }

        // Count features first so repeated words saturate instead of
        // dominating the vector.
        let mut counts: HashMap<String, f32> = HashMap::new();

        for (position, word) in words.iter().enumerate() {
            add_feature(&mut counts, format!("w:{word}"), UNIGRAM_WEIGHT);

            for stem in stem_variants(word) {
                add_feature(&mut counts, format!("s:{stem}"), STEM_WEIGHT);
            }

            if let Some(next) = words.get(position + 1) {
                add_feature(&mut counts, format!("b:{word}_{next}"), BIGRAM_WEIGHT);
            }

            for gram in char_ngrams(word, 3) {
                add_feature(&mut counts, format!("3:{gram}"), TRIGRAM_WEIGHT);
            }
            for gram in char_ngrams(word, 4) {
                add_feature(&mut counts, format!("4:{gram}"), QUADGRAM_WEIGHT);
            }
        }

        let mut vector = vec![0.0_f32; dimensions];
        for (feature, count) in counts {
            let index_hash = fnv1a64(feature.as_bytes(), 0);
            let sign_hash = fnv1a64(feature.as_bytes(), 0x9e37_79b9_7f4a_7c15);
            let index = (index_hash as usize) % dimensions;
            let sign = if sign_hash & 1 == 0 { 1.0 } else { -1.0 };
            // sqrt damping keeps a word repeated ten times from swamping the
            // rest of the document.
            vector[index] += sign * count.sqrt();
        }

        normalize_l2(&mut vector);
        vector
    }
}

/// Accumulate one feature weight.
fn add_feature(counts: &mut HashMap<String, f32>, feature: String, weight: f32) {
    *counts.entry(feature).or_insert(0.0) += weight;
}

/// Deterministic 64-bit FNV-1a with an optional salt, followed by a splitmix64
/// finalizer. FNV-1a alone has weak low-bit avalanche, which clusters the
/// `hash % dimensions` bucket; the finalizer spreads every bit so the feature
/// projection is close to uniform.
fn fnv1a64(bytes: &[u8], salt: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ salt;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;
    hash
}

/// Boundary-padded character n-grams (`^arch`, `ch$`, ...).
fn char_ngrams(word: &str, n: usize) -> Vec<String> {
    let mut padded: Vec<char> = Vec::with_capacity(word.chars().count() + 2);
    padded.push('^');
    padded.extend(word.chars());
    padded.push('$');
    if padded.len() < n {
        return Vec::new();
    }
    padded
        .windows(n)
        .map(|window| window.iter().collect())
        .collect()
}

/// Lightweight English stemming: strip the common inflectional and
/// derivational endings listed in [`INFLECTIONS`] / [`DERIVATIONS`], keeping
/// every plausible stem so variants converge regardless of which rule they hit.
///
/// The returned list always includes the word itself when it is long enough to
/// be interesting.
pub(crate) fn stem_variants(word: &str) -> Vec<String> {
    let mut variants = Vec::new();
    if word.chars().count() < 4 {
        return variants;
    }
    push_unique(&mut variants, word.to_owned());

    // Inflectional endings. `sliding` -> {slid, slide}; `writer` -> {writ,
    // write}; `running` -> {runn, runne, run}.
    for suffix in ["ings", "ing", "ers", "er", "ed"] {
        let Some(stem) = word.strip_suffix(suffix) else {
            continue;
        };
        let stem_chars: Vec<char> = stem.chars().collect();
        if stem_chars.len() < 3 {
            break;
        }
        push_unique(&mut variants, stem.to_owned());
        push_unique(&mut variants, format!("{stem}e"));
        if stem_chars.len() >= 4 {
            let last = stem_chars[stem_chars.len() - 1];
            if last == stem_chars[stem_chars.len() - 2] {
                let dedoubled: String = stem_chars[..stem_chars.len() - 1].iter().collect();
                push_unique(&mut variants, dedoubled);
            }
        }
        break;
    }

    // Derivational endings: `architecture` -> `architect`,
    // `architectural` -> `architect`, `happiness` -> `happi`, ...
    for suffix in [
        "ational", "ations", "ation", "tion", "sion", "ment", "ness", "ance", "ence", "ural",
        "ure", "al", "ly",
    ] {
        let Some(stem) = word.strip_suffix(suffix) else {
            continue;
        };
        if stem.chars().count() >= 3 {
            push_unique(&mut variants, stem.to_owned());
        }
        break;
    }

    // Plurals / third person singular.
    if let Some(stem) = word.strip_suffix("ies") {
        if stem.chars().count() >= 3 {
            push_unique(&mut variants, format!("{stem}y"));
        }
    } else if let Some(stem) = word.strip_suffix("es") {
        if stem.chars().count() >= 3 {
            push_unique(&mut variants, stem.to_owned());
            push_unique(&mut variants, format!("{stem}e"));
            // Dropping only the `s` keeps `slides` -> `slide`.
            if let Some(singular) = word.strip_suffix('s') {
                push_unique(&mut variants, singular.to_owned());
            }
        }
    } else if let Some(stem) = word.strip_suffix('s')
        && !word.ends_with("ss")
        && stem.chars().count() >= 3
    {
        push_unique(&mut variants, stem.to_owned());
    }

    variants
}

fn push_unique(variants: &mut Vec<String>, value: String) {
    if !variants.contains(&value) {
        variants.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        cosine(a, b)
    }

    #[test]
    fn default_embedder_is_256_dimensional_and_normalized() {
        let embedder = HashSemanticEmbedder::new();
        assert_eq!(embedder.dimensions(), DEFAULT_DIMENSIONS);

        let vector = embedder.embed("sliding panes and architecture");
        assert_eq!(vector.len(), DEFAULT_DIMENSIONS);
        let norm: f32 = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "expected unit norm, got {norm}");
    }

    #[test]
    fn empty_text_yields_zero_vector() {
        let embedder = HashSemanticEmbedder::new();
        let vector = embedder.embed("   ...   ");
        assert_eq!(vector.len(), DEFAULT_DIMENSIONS);
        assert!(vector.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn stems_cover_the_documented_variants() {
        let sliding = stem_variants("sliding");
        assert!(sliding.contains(&"slid".to_owned()));
        assert!(sliding.contains(&"slide".to_owned()));

        let slides = stem_variants("slides");
        assert!(slides.contains(&"slide".to_owned()));

        let writing = stem_variants("writing");
        let writer = stem_variants("writer");
        assert!(writing.iter().any(|stem| writer.contains(stem)));

        let architecture = stem_variants("architecture");
        let architectural = stem_variants("architectural");
        assert!(architecture.contains(&"architect".to_owned()));
        assert!(architectural.contains(&"architect".to_owned()));
    }

    #[test]
    fn morphological_variants_are_more_similar_than_unrelated_text() {
        let embedder = HashSemanticEmbedder::new();
        let query = embedder.embed("architectural");
        let related = embedder.embed("Architecture Blueprint");
        let unrelated = embedder.embed("Sourdough starter feeding schedule");

        let related_score = dot(&query, &related);
        let unrelated_score = dot(&query, &unrelated);

        assert!(related_score > 0.3, "related cosine was {related_score}");
        assert!(
            related_score > unrelated_score + 0.15,
            "related {related_score} vs unrelated {unrelated_score}"
        );
    }

    #[test]
    fn shared_topical_vocabulary_raises_similarity() {
        let embedder = HashSemanticEmbedder::new();
        let a = embedder.embed("Rust ownership and borrowing rules");
        let b = embedder.embed("Borrowing rules in Rust");
        let c = embedder.embed("Tomato compost and soil");
        assert!(dot(&a, &b) > dot(&a, &c) + 0.2);
    }

    #[test]
    fn custom_dimensions_are_honoured() {
        let embedder = HashSemanticEmbedder::with_dimensions(64);
        assert_eq!(embedder.dimensions(), 64);
        assert_eq!(embedder.embed("hello world").len(), 64);
        assert_eq!(
            HashSemanticEmbedder::with_dimensions(0).dimensions(),
            DEFAULT_DIMENSIONS
        );
    }
}
