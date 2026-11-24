//! Dense-vector semantic + lexical + graph-boosted search for zcast.
//!
//! This crate implements the `zcast:search/provider@1.0.0` contract
//! (`zcast-search-onnx`) and the [`NoteLifecycleHook`] used by the kernel to
//! keep the index fresh. It is built around two seams:
//!
//! * [`TextEmbedder`] — pluggable dense embeddings. The default
//!   [`HashSemanticEmbedder`] is pure Rust and needs no model download, so
//!   static `musl` builds and tests work out of the box. An `ort`-backed
//!   embedder can be injected with
//!   [`OnnxSearchProvider::with_embedder`].
//! * [`OnnxSearchProvider`] — the provider itself. It fuses lexical field
//!   matching, dense cosine similarity (title similarity weighted higher), and
//!   wikilink graph boosts into one normalized hybrid score per hit.
//!
//! Privacy comes first (DEC-02): private notes are removed *before* lexical
//! scoring, semantic scoring, graph boosting, or limiting, and private
//! referrers never contribute graph boosts to a public query.
//!
//! ```no_run
//! use std::sync::Arc;
//! use zcast_interfaces::{ReadContext, SearchProvider, SearchQuery};
//! use zcast_search_onnx::{HashSemanticEmbedder, OnnxSearchProvider};
//!
//! # async fn example() {
//! let provider = OnnxSearchProvider::new()
//!     .with_embedder(Arc::new(HashSemanticEmbedder::new()));
//! let hits = provider
//!     .search(
//!         &SearchQuery {
//!             text: "architectural".to_owned(),
//!             limit: 10,
//!             context_note_id: None,
//!         },
//!         ReadContext::PUBLIC,
//!     )
//!     .await
//!     .unwrap();
//! # let _ = hits;
//! # }
//! ```

mod embedder;
mod provider;
mod text;

#[cfg(test)]
mod tests;

pub use embedder::{DEFAULT_DIMENSIONS, HashSemanticEmbedder, TextEmbedder, normalize_l2};
pub use provider::OnnxSearchProvider;
