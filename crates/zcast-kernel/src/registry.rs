use serde::Serialize;
use std::sync::Arc;
use zcast_interfaces::{
    HookStage, Html, HtmlRenderHook, ModuleManifest, Note, NoteEvent, NoteLifecycleHook,
    ReadContext,
};

#[derive(Debug, Clone, Serialize)]
pub struct SlotBinding {
    pub interface: &'static str,
    pub module_id: &'static str,
    pub module_version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistrySnapshot {
    pub modules: Vec<ModuleManifest>,
    pub bindings: Vec<SlotBinding>,
}

struct RegisteredHtmlHook {
    order: usize,
    hook: Arc<dyn HtmlRenderHook>,
}

struct RegisteredLifecycleHook {
    order: usize,
    hook: Arc<dyn NoteLifecycleHook>,
}

/// Tracks installed modules, active interface bindings, and deterministic staged hooks (DEC-16).
#[derive(Default)]
pub struct ModuleRegistry {
    modules: Vec<ModuleManifest>,
    bindings: Vec<SlotBinding>,
    html_hooks: Vec<RegisteredHtmlHook>,
    lifecycle_hooks: Vec<RegisteredLifecycleHook>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_module(&mut self, manifest: ModuleManifest) {
        if !self.modules.iter().any(|m| m.id == manifest.id) {
            self.modules.push(manifest);
        }
    }

    pub fn bind_slot(&mut self, interface: &'static str, manifest: &ModuleManifest) {
        self.bindings.retain(|b| b.interface != interface);
        self.bindings.push(SlotBinding {
            interface,
            module_id: manifest.id,
            module_version: manifest.version,
        });
    }

    pub fn add_html_hook(&mut self, hook: Arc<dyn HtmlRenderHook>) {
        let order = self.html_hooks.len();
        self.html_hooks.push(RegisteredHtmlHook { order, hook });
        self.html_hooks
            .sort_by_key(|h| (h.hook.stage() as u8, h.order));
    }

    pub fn add_lifecycle_hook(&mut self, hook: Arc<dyn NoteLifecycleHook>) {
        let order = self.lifecycle_hooks.len();
        self.lifecycle_hooks
            .push(RegisteredLifecycleHook { order, hook });
        self.lifecycle_hooks
            .sort_by_key(|h| (h.hook.stage() as u8, h.order));
    }

    pub fn run_html_hooks(&self, note: &Note, mut html: Html, ctx: ReadContext) -> Html {
        for entry in &self.html_hooks {
            html = entry.hook.transform(note, html, ctx);
        }
        html
    }

    pub async fn emit_event(&self, event: &NoteEvent) {
        for entry in &self.lifecycle_hooks {
            entry.hook.on_event(event).await;
        }
    }

    pub fn has_binding(&self, interface: &str) -> bool {
        self.bindings.iter().any(|b| b.interface == interface)
    }

    /// Verify that every registered module's required (non-optional) interfaces are bound.
    pub fn validate_dependencies(&self) -> Result<(), (&'static str, &'static str)> {
        for module in &self.modules {
            for req in module.requires {
                if !req.optional && !self.has_binding(req.name) {
                    return Err((module.id, req.name));
                }
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> RegistrySnapshot {
        RegistrySnapshot {
            modules: self.modules.clone(),
            bindings: self.bindings.clone(),
        }
    }

    pub fn stage_order_for_test(&self) -> Vec<(&'static str, HookStage)> {
        self.html_hooks
            .iter()
            .map(|h| (h.hook.id(), h.hook.stage()))
            .collect()
    }
}
