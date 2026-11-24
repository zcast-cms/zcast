use crate::cache::RenderCache;
use crate::config::KernelConfig;
use crate::error::KernelError;
use crate::registry::{ModuleRegistry, RegistrySnapshot};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use zcast_interfaces::{
    AuthProvider, CardRenderContext, CardView, EditorProvider, HtmlRenderHook, IFACE_AUTH,
    IFACE_EDITOR, IFACE_IO, IFACE_LINKS, IFACE_SEARCH, IFACE_STORAGE, IFACE_THEME, LinkParser,
    Note, NoteAlias, NoteEvent, NoteExporter, NoteId, NoteImporter, NoteLifecycleHook,
    NoteListFilter, ReadContext, SearchProvider, Storage, StorageError, ThemeColorMode,
    ThemeProvider, UiExtension, UiSlots, slugify,
};

/// Namespace + key used to persist the live site settings in the `ModuleStore` (DEC-15).
const APP_SETTINGS_NAMESPACE: &str = "zcast-kernel";
const APP_SETTINGS_KEY: &str = "app:settings";

/// Content/app settings edited live from the Admin UI (DEC-15).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub site_title: String,
    pub pinned_home_slug: String,
    pub color_mode: ThemeColorMode,
}

/// Partial on-disk representation so older or hand-edited values can merge over
/// `KernelConfig` boot defaults field by field.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StoredAppSettings {
    site_title: Option<String>,
    pinned_home_slug: Option<String>,
    color_mode: Option<ThemeColorMode>,
}

impl AppSettings {
    fn from_config(config: &KernelConfig) -> Self {
        Self {
            site_title: config.site_title.clone(),
            pinned_home_slug: config.pinned_home_slug.clone(),
            color_mode: config.color_mode,
        }
    }

    fn merge_stored(defaults: Self, stored: StoredAppSettings) -> Self {
        let site_title = stored
            .site_title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(defaults.site_title);
        let pinned_home_slug = stored
            .pinned_home_slug
            .filter(|value| {
                let slug = value.trim();
                !slug.is_empty() && slug != "_zcast"
            })
            .unwrap_or(defaults.pinned_home_slug);

        Self {
            site_title,
            pinned_home_slug,
            color_mode: stored.color_mode.unwrap_or(defaults.color_mode),
        }
    }
}

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
    pub search: Option<Arc<dyn SearchProvider>>,
    pub exporter: Option<Arc<dyn NoteExporter>>,
    pub importer: Option<Arc<dyn NoteImporter>>,
    pub ui_extensions: Vec<Arc<dyn UiExtension>>,
    pub render_cache: RenderCache,
    pub registry: ModuleRegistry,
    app_settings: RwLock<AppSettings>,
}

pub struct ZcastKernelBuilder {
    config: KernelConfig,
    storage: Arc<dyn Storage>,
    links: Arc<dyn LinkParser>,
    theme: Arc<dyn ThemeProvider>,
    editor: Option<Arc<dyn EditorProvider>>,
    auth: Option<Arc<dyn AuthProvider>>,
    search: Option<Arc<dyn SearchProvider>>,
    exporter: Option<Arc<dyn NoteExporter>>,
    importer: Option<Arc<dyn NoteImporter>>,
    ui_extensions: Vec<Arc<dyn UiExtension>>,
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
            search: None,
            exporter: None,
            importer: None,
            ui_extensions: Vec::new(),
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

    pub fn with_search(mut self, search: Arc<dyn SearchProvider>) -> Self {
        let m = search.manifest();
        self.registry.bind_slot(IFACE_SEARCH, &m);
        self.registry.register_module(m);
        self.search = Some(search);
        self
    }

    pub fn with_exporter(mut self, exporter: Arc<dyn NoteExporter>) -> Self {
        let m = exporter.manifest();
        self.registry.bind_slot(IFACE_IO, &m);
        self.registry.register_module(m);
        self.exporter = Some(exporter);
        self
    }

    pub fn with_importer(mut self, importer: Arc<dyn NoteImporter>) -> Self {
        let m = importer.manifest();
        self.registry.bind_slot(IFACE_IO, &m);
        self.registry.register_module(m);
        self.importer = Some(importer);
        self
    }

    pub fn with_ui_extension(mut self, ext: Arc<dyn UiExtension>) -> Self {
        let m = ext.manifest();
        for provided in m.provides {
            self.registry.bind_slot(provided.name, &m);
        }
        self.registry.register_module(m);
        self.ui_extensions.push(ext);
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
        if let Err((module, interface)) = self.registry.validate_dependencies() {
            return Err(KernelError::MissingDependency { module, interface });
        }

        self.storage.initialize().await?;

        let defaults = AppSettings::from_config(&self.config);
        let app_settings = match self
            .storage
            .module_store(APP_SETTINGS_NAMESPACE)
            .get(APP_SETTINGS_KEY)
            .await?
        {
            Some(value) => match serde_json::from_value::<StoredAppSettings>(value) {
                Ok(stored) => AppSettings::merge_stored(defaults, stored),
                Err(_) => defaults,
            },
            None => defaults,
        };

        let kernel = Arc::new(ZcastKernel {
            config: self.config,
            storage: self.storage,
            links: self.links,
            theme: self.theme,
            editor: self.editor,
            auth: self.auth,
            search: self.search,
            exporter: self.exporter,
            importer: self.importer,
            ui_extensions: self.ui_extensions,
            render_cache: RenderCache::new(),
            registry: self.registry,
            app_settings: RwLock::new(app_settings),
        });

        kernel.seed_starter_content().await?;

        if let Some(search) = &kernel.search {
            let notes = kernel
                .storage
                .list_notes(NoteListFilter::default(), ReadContext::ADMIN)
                .await?;
            search.reindex_all(&notes).await?;
        }

        Ok(kernel)
    }
}

impl ZcastKernel {
    pub fn registry_snapshot(&self) -> RegistrySnapshot {
        self.registry.snapshot()
    }

    /// Snapshot of the live content/app settings (DEC-15).
    pub fn app_settings(&self) -> AppSettings {
        self.app_settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Validate, persist, and activate new live site settings.
    /// Takes effect immediately: the render cache is invalidated so every page
    /// picks up the new title, pinned home slug, and colour mode.
    pub async fn update_app_settings(&self, next: AppSettings) -> Result<AppSettings, KernelError> {
        let site_title = next.site_title.trim();
        if site_title.is_empty() {
            return Err(KernelError::BadRequest("site title cannot be empty".into()));
        }

        let pinned_home_slug = next.pinned_home_slug.trim();
        if pinned_home_slug.is_empty() {
            return Err(KernelError::BadRequest(
                "pinned home slug cannot be empty".into(),
            ));
        }
        if pinned_home_slug == "_zcast" {
            return Err(KernelError::BadRequest("slug '_zcast' is reserved".into()));
        }

        let next = AppSettings {
            site_title: site_title.to_owned(),
            pinned_home_slug: pinned_home_slug.to_owned(),
            color_mode: next.color_mode,
        };
        let value =
            serde_json::to_value(&next).map_err(|error| KernelError::Config(error.to_string()))?;

        self.storage
            .module_store(APP_SETTINGS_NAMESPACE)
            .put(APP_SETTINGS_KEY, &value, None)
            .await?;

        {
            let mut guard = self
                .app_settings
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = next.clone();
        }
        self.render_cache.invalidate_all();

        Ok(next)
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
        index.slug = self.app_settings().pinned_home_slug;

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
            self.links
                .sync_note_links(note, self.storage.as_ref())
                .await?;
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
            let hooked = self
                .registry
                .run_html_hooks(note, raw_html, ctx)
                .into_string();
            self.render_cache
                .insert(note.id.clone(), ctx, hooked.clone());
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

    pub fn get_extension_asset(&self, prefix: &str, path: &str) -> Option<zcast_interfaces::Asset> {
        self.ui_extensions
            .iter()
            .find(|ext| ext.asset_prefix() == prefix)
            .and_then(|ext| ext.get_asset(path))
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

        let mut card_footer_slot = String::new();
        for ext in &self.ui_extensions {
            card_footer_slot.push_str(ext.card_footer_slot(card, is_admin).as_str());
        }

        let ctx = CardRenderContext {
            is_admin,
            card_toolbar_slot,
            card_footer_slot,
        };
        self.theme.render_card(card, &ctx).into_string()
    }

    pub fn page_ui_slots(&self, is_admin: bool) -> UiSlots {
        let mut slots = UiSlots::default();
        for ext in &self.ui_extensions {
            slots.head.push_str(ext.head_slot(is_admin).as_str());
            slots
                .header_actions
                .push_str(ext.header_actions_slot(is_admin).as_str());
            slots
                .body_end
                .push_str(ext.body_end_slot(is_admin).as_str());
        }
        if is_admin {
            if let Some(editor) = &self.editor {
                slots.head.push_str(editor.head_slot().as_str());
                slots.body_end.push_str(editor.body_end_slot().as_str());
            }
            slots.header_actions.push_str(
                r#"<button type="button" class="zcast-header-btn" data-zcast-new-note>+ New Note</button>"#,
            );
            slots.header_actions.push_str(
                r#"<a href="/_zcast/admin" class="zcast-header-btn" data-zcast-admin-link>Admin</a>"#,
            );
        }
        if let Some(auth) = &self.auth {
            slots
                .header_actions
                .push_str(auth.header_action_slot(is_admin).as_str());
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
        if let Some(search) = &self.search {
            search.index_note(&note).await?;
        }
        self.registry
            .emit_event(&NoteEvent::Saved(note.clone()))
            .await;

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

            // A renamed note may link to itself (`[[Old Title]]` in its own body).
            // Apply that rewrite to `existing` before the atomic rename so the
            // self-link is persisted, but keep it out of `rewritten_others` so
            // the note is not written twice in the same transaction.
            let mut rewritten_others: Vec<Note> = Vec::with_capacity(rewritten.len());
            for note in rewritten {
                if note.id == existing.id {
                    existing.markdown_body = note.markdown_body;
                } else {
                    rewritten_others.push(note);
                }
            }

            self.storage
                .rename_note_atomic(&existing, &alias, &rewritten_others)
                .await?;

            // Only re-sync the graph after the rename committed: rewriting the
            // edges earlier would leave them stale if the rename conflicts.
            for r in &rewritten_others {
                self.links.sync_note_links(r, self.storage.as_ref()).await?;
            }
            self.links
                .sync_note_links(&existing, self.storage.as_ref())
                .await?;
            self.render_cache.invalidate_all();

            if let Some(search) = &self.search {
                for r in &rewritten_others {
                    search.index_note(r).await?;
                }
                search.index_note(&existing).await?;
            }

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
            if let Some(search) = &self.search {
                search.index_note(&existing).await?;
            }
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
        if let Some(search) = &self.search {
            search.remove_note(id).await?;
        }
        self.registry
            .emit_event(&NoteEvent::Deleted(id.clone()))
            .await;
        Ok(())
    }
}
