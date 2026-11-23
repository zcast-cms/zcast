use dashmap::DashMap;
use zcast_interfaces::{NoteId, ReadContext};

/// In-memory HTML render cache keyed by `(note_id, include_private)` (DEC-18).
/// Cleared on note writes, renames, or deletions so `[[wikilink]]` resolution never goes stale.
#[derive(Debug, Default)]
pub struct RenderCache {
    entries: DashMap<(NoteId, bool), String>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, note_id: &NoteId, ctx: ReadContext) -> Option<String> {
        self.entries
            .get(&(note_id.clone(), ctx.include_private))
            .map(|v| v.value().clone())
    }

    pub fn insert(&self, note_id: NoteId, ctx: ReadContext, rendered_html: String) {
        self.entries
            .insert((note_id, ctx.include_private), rendered_html);
    }

    pub fn invalidate_all(&self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
