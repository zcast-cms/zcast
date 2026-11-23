use crate::cache::RenderCache;
use crate::config::KernelConfig;
use crate::error::KernelError;
use crate::registry::{ModuleRegistry, RegistrySnapshot};
use chrono::Utc;
use std::sync::Arc;
use zcast_interfaces::{
    AuthProvider, CardRenderContext, CardView, EditorProvider, HtmlRenderHook, IFACE_AUTH,
    IFACE_EDITOR, IFACE_LINKS, IFACE_STORAGE, IFACE_THEME, LinkParser, Note, NoteAlias, NoteEvent,
    NoteId, NoteListFilter, NoteLifecycleHook, ReadContext, Storage, StorageError, ThemeProvider,
    UiSlots, slugify,
};

#[derive(Debug, Clone)]
pub struct SaveNoteResult {
    pub note: Note,
    pub rendered_html: String,
    pub card_html: String,
    pub renamed_from_slug: Option<String>,
}

pub struct ZcastKernel {
    pub config: KernelConfig,
    pub storage: Arc<dyn Storage>,
    pub links: Arc<dyn LinkParser>,
    pub theme: Arc<dyn ThemeProvider>,
    pub editor: Option<Arc<dyn EditorProvider>>,
    pub auth: Option<Arc<dyn AuthProvider>>,
    pub render_cache: RenderCache,
    pub registry: ModuleRegistry,
}

pub struct ZcastKernelBuilder {
    config: KernelConfig,
    storage: Arc<dyn Storage>,
    links: Arc<dyn LinkParser>,
    theme: Arc<dyn ThemeProvider>,
    editor: Option<Arc<dyn EditorProvider>>,
    auth: Option<Arc<dyn AuthProvider>>,
    registry: ModuleRegistry,
}

impl ZcastKernelBuilder {
    pub fn new(
        config: KernelConfig,
        storage: Arc<dyn Storage>,
        links: Arc<dyn LinkParser>,
        theme: Arc<dyn ThemeProvider>,
    ) -> Self {
        let mut registry = ModuleRegistry::new();
        let s_m = storage.manifest();
        registry.bind_slot(IFACE_STORAGE, &s_m);
        registry.register_module(s_m);

        let l_m = links.manifest();
        registry.bind_slot(IFACE_LINKS, &l_m);
        registry.register_module(l_m);

        let t_m = theme.manifest();
        registry.bind_slot(IFACE_THEME, &t_m);
        registry.register_module(t_m);

        Self {
            config,
            storage,
            links,
            theme,
            editor: None,
            auth: None,
            registry,
        }
    }

    pub fn with_editor(mut self, editor: Arc<dyn EditorProvider>) -> Self {
        let m = editor.manifest();
        self.registry.bind_slot(IFACE_EDITOR, &m);
        self.registry.register_module(m);
        self.editor = Some(editor);
        self
    }

    pub fn with_auth(mut self, auth: Arc<dyn AuthProvider>) -> Self {
        let m = auth.manifest();
        self.registry.bind_slot(IFACE_AUTH, &m);
        self.registry.register_module(m);
        self.auth = Some(auth);
        self
    }

    pub fn with_html_hook(mut self, hook: Arc<dyn HtmlRenderHook>) -> Self {
        self.registry.add_html_hook(hook);
        self
    }

    pub fn with_lifecycle_hook(mut self, hook: Arc<dyn NoteLifecycleHook>) -> Self {
        self.registry.add_lifecycle_hook(hook);
        self
    }

    pub async fn build(self) -> Result<Arc<ZcastKernel>, KernelError> {
        self.storage.initialize().await?;

        let kernel = Arc::new(ZcastKernel {
            config: self.config,
            storage: self.storage,
            links: self.links,
            theme: self.theme,
            editor: self.editor,
            auth: self.auth,
            render_cache: RenderCache::new(),
            registry: self.registry,
        });

        kernel.seed_starter_content().await?;
        Ok(kernel)
    }
}

impl ZcastKernel {
    pub fn registry_snapshot(&self) -> RegistrySnapshot {
        self.registry.snapshot()
    }

    pub fn has_auth(&self) -> bool {
        self.auth.is_some()
    }

    pub fn custom_css_enabled(&self) -> bool {
        self.config
            .custom_css_path
            .as_ref()
            .is_some_and(|p| p.exists())
    }

    /// Seed starter cards when booting against an empty database so `/` works immediately.
    pub async fn seed_starter_content(&self) -> Result<(), KernelError> {
        let existing = self
            .storage
            .list_notes(
                NoteListFilter {
                    limit: Some(1),
                    ..Default::default()
                },
                ReadContext::ADMIN,
            )
            .await?;
        if !existing.is_empty() {
            return Ok(());
        }

        let mut index = Note::new(
            "Index",
            "Welcome to **Zcast** — your modular Zettelkasten.\n\n\
             Click [[How Sliding Panes Work]] to open a card to the right and trace your trail across the screen.\n\n\
             Try clicking **Edit** in the card toolbar to edit this note in place, select text for the floating formatting toolbar, or click [[My First Note]] to spin up a brand new card from an unresolved link.",
            false,
        );
        index.slug = self.config.pinned_home_slug.clone();

        let panes = Note::new(
            "How Sliding Panes Work",
            "Every time you click a `[[wikilink]]`, the target card opens to the right and the canvas scrolls smoothly.\n\n\
             - Use the **breadcrumbs** at the top to jump back up your rabbit hole.\n\
             - Check **Linked from** below to see backlinks pointing here (like [[Index]]).\n\
             - Links to private or not-yet-written notes (such as [[My First Note]]) appear as clickable draft triggers when editing, and as clean plain text for public readers.",
            false,
        );

        let secret = Note::new(
            "Private Scratchpad",
            "This note is marked **private**. In public mode (`?preview=public` or unauthenticated visitors), it never leaks into pages, backlinks, or `[[links]]`.",
            true,
        );

        for note in [&index, &panes, &secret] {
            self.storage.save_note(note).await?;
            self.links.sync_note_links(note, self.storage.as_ref()).await?;
        }
        self.render_cache.invalidate_all();
        Ok(())
    }

    pub async fn build_card_view(
        &self,
        note: &Note,
        ctx: ReadContext,
    ) -> Result<CardView, KernelError> {
        let rendered_html = if let Some(cached) = self.render_cache.get(&note.id, ctx) {
            cached
        } else {
            let raw_html = self
                .links
                .render_markdown_with_links(&note.markdown_body, self.storage.as_ref(), ctx)
                .await?;
            let hooked = self.registry.run_html_hooks(note, raw_html, ctx).into_string();
            self.render_cache.insert(note.id.clone(), ctx, hooked.clone());
            hooked
        };

        let backlinks = self
            .links
            .get_backlinks(&note.id, self.storage.as_ref(), ctx)
            .await?;

        Ok(CardView {
            id: note.id.clone(),
            slug: note.slug.clone(),
            title: note.title.clone(),
            markdown_body: note.markdown_body.clone(),
            rendered_html,
            private: note.private,
            backlinks,
            updated_at: note.updated_at,
        })
    }

    pub fn render_card_html(&self, card: &CardView, is_admin: bool) -> String {
        let card_toolbar_slot = if is_admin {
            self.editor
                .as_ref()
                .map(|e| e.card_toolbar_slot(card).into_string())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let ctx = CardRenderContext {
            is_admin,
            card_toolbar_slot,
            card_footer_slot: String::new(),
        };
        self.theme.render_card(card, &ctx).into_string()
    }

    pub fn page_ui_slots(&self, is_admin: bool) -> UiSlots {
        let mut slots = UiSlots::default();
        if is_admin {
            if let Some(editor) = &self.editor {
                slots.head.push_str(editor.head_slot().as_str());
                slots.body_end.push_str(editor.body_end_slot().as_str());
            }
            slots.header_actions.push_str(
                r#"<button type="button" class="zcast-header-btn" data-zcast-new-note>+ New Note</button>"#,
            );
        }
        slots
    }

    pub async fn create_note(
        &self,
        title: String,
        markdown_body: String,
        private: bool,
    ) -> Result<SaveNoteResult, KernelError> {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return Err(KernelError::BadRequest("note title cannot be empty".into()));
        }
        let slug = slugify(trimmed);
        if slug == "_zcast" {
            return Err(KernelError::BadRequest("slug '_zcast' is reserved".into()));
        }

        let mut note = Note::new(trimmed, markdown_body, private);
        note.slug = slug;
        self.storage.save_note(&note).await?;
        self.links
            .sync_note_links(&note, self.storage.as_ref())
            .await?;
        self.render_cache.invalidate_all();
        self.registry.emit_event(&NoteEvent::Saved(note.clone())).await;

        let card = self.build_card_view(&note, ReadContext::ADMIN).await?;
        let card_html = self.render_card_html(&card, true);
        Ok(SaveNoteResult {
            note,
            rendered_html: card.rendered_html,
            card_html,
            renamed_from_slug: None,
        })
    }

    pub async fn update_note(
        &self,
        id: &NoteId,
        title: String,
        markdown_body: String,
        private: bool,
    ) -> Result<SaveNoteResult, KernelError> {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return Err(KernelError::BadRequest("note title cannot be empty".into()));
        }

        let mut existing = self
            .storage
            .get_note_by_id(id, ReadContext::ADMIN)
            .await?
            .ok_or_else(|| KernelError::NotFound(id.to_string()))?;

        let old_title = existing.title.clone();
        let old_slug = existing.slug.clone();
        let renamed = old_title != trimmed;

        existing.title = trimmed.to_owned();
        existing.markdown_body = markdown_body;
        existing.private = private;
        existing.updated_at = Utc::now();

        let renamed_from_slug = if renamed {
            let new_slug = slugify(trimmed);
            if new_slug == "_zcast" {
                return Err(KernelError::BadRequest("slug '_zcast' is reserved".into()));
            }
            existing.slug = new_slug;
            let alias = NoteAlias {
                slug: old_slug.clone(),
                title: old_title.clone(),
                created_at: Utc::now(),
            };
            existing.aliases.push(alias.clone());

            let rewritten = self
                .links
                .rewrite_links_for_rename(&old_title, &existing.title, self.storage.as_ref())
                .await?;
            // Exclude self from rewritten referrers if self-linked; handle self-link in `existing`
            let rewritten_others: Vec<Note> = rewritten
                .into_iter()
                .filter(|n| n.id != existing.id)
                .collect();

            self.storage
                .rename_note_atomic(&existing, &alias, &rewritten_others)
                .await?;

            for r in &rewritten_others {
                self.links.sync_note_links(r, self.storage.as_ref()).await?;
            }
            self.links
                .sync_note_links(&existing, self.storage.as_ref())
                .await?;
            self.render_cache.invalidate_all();

            self.registry
                .emit_event(&NoteEvent::Renamed {
                    old_title,
                    old_slug: old_slug.clone(),
                    note: existing.clone(),
                })
                .await;

            Some(old_slug)
        } else {
            self.storage.save_note(&existing).await?;
            self.links
                .sync_note_links(&existing, self.storage.as_ref())
                .await?;
            self.render_cache.invalidate_all();
            self.registry
                .emit_event(&NoteEvent::Saved(existing.clone()))
                .await;
            None
        };

        let card = self.build_card_view(&existing, ReadContext::ADMIN).await?;
        let card_html = self.render_card_html(&card, true);
        Ok(SaveNoteResult {
            note: existing,
            rendered_html: card.rendered_html,
            card_html,
            renamed_from_slug,
        })
    }

    pub async fn delete_note(&self, id: &NoteId) -> Result<(), KernelError> {
        self.links
            .remove_note_links(id, self.storage.as_ref())
            .await?;
        self.storage.delete_note(id).await.map_err(|e| match e {
            StorageError::NotFound(s) => KernelError::NotFound(s),
            other => KernelError::Storage(other),
        })?;
        self.render_cache.invalidate_all();
        self.registry
            .emit_event(&NoteEvent::Deleted(id.clone()))
            .await;
        Ok(())
    }
}
