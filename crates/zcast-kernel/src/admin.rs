//! Admin dashboard: live site settings, module dependency graph, and backups.
//!
//! Rendered only for authenticated administrators by the HTTP layer
//! (`GET /_zcast/admin`). The page consumes the theme's `--zcast-*` design
//! tokens so it inherits the active palette, typography, and radius without
//! shipping a second stylesheet, and it reads every live setting from
//! [`ZcastKernel::app_settings`] so edits take effect immediately.

use std::fmt::Write as _;

use maud::{DOCTYPE, Markup, PreEscaped, html};
use zcast_interfaces::ThemeColorMode;

use crate::kernel::ZcastKernel;
use crate::registry::RegistrySnapshot;

/// Render the full admin dashboard document.
pub fn render_admin_page(
    kernel: &ZcastKernel,
    notice: Option<&str>,
    error: Option<&str>,
) -> String {
    let settings = kernel.app_settings();
    let snapshot = kernel.registry_snapshot();
    let color_mode = settings.color_mode;
    let has_auth = kernel.has_auth();
    let diagram = dependency_diagram(&snapshot);
    let search_manifest = kernel.search.as_ref().map(|search| search.manifest());
    let capabilities = kernel.search.as_ref().map(|search| search.capabilities());
    let has_search = kernel.search.is_some();
    let has_exporter = kernel.exporter.is_some();
    let has_importer = kernel.importer.is_some();

    let document = html! {
        (DOCTYPE)
        html lang="en" data-theme=(color_mode.as_str()) data-color-mode=(color_mode.as_str()) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Admin — " (settings.site_title) }
                meta name="robots" content="noindex, nofollow";
                link rel="stylesheet" href="/_zcast/theme/theme.css";
                @if color_mode == ThemeColorMode::Auto {
                    script { (PreEscaped(AUTO_THEME_SCRIPT)) }
                }
                style { (PreEscaped(ADMIN_CSS)) }
            }
            body class="zcast-admin" data-zcast-admin-page {
                header class="zcast-admin-header" {
                    div class="zcast-admin-header__inner" {
                        a class="zcast-admin-header__brand" href="/" {
                            (settings.site_title)
                        }
                        nav class="zcast-admin-header__nav" aria-label="Admin navigation" {
                            a class="zcast-header-btn" href="/" data-zcast-back-to-site {
                                "Back to site"
                            }
                            @if has_auth {
                                form method="post" action="/_zcast/logout" class="zcast-admin-header__logout" {
                                    button
                                        type="submit"
                                        class="zcast-header-btn"
                                        data-zcast-logout
                                    {
                                        "Log out"
                                    }
                                }
                            }
                        }
                    }
                }
                main class="zcast-admin-main" {
                    div class="zcast-admin-main__inner" {
                        div class="zcast-admin-heading" {
                            p class="zcast-admin-eyebrow" { "Site administration" }
                            h1 class="zcast-admin-title" { "Admin" }
                            p class="zcast-admin-lede" {
                                "Live settings, compiled modules, and portable Markdown backups."
                            }
                        }

                        @if let Some(message) = notice {
                            div
                                class="zcast-admin-flash zcast-admin-flash--success"
                                role="status"
                                data-zcast-flash="success"
                            {
                                (message)
                            }
                        }
                        @if let Some(message) = error {
                            div
                                class="zcast-admin-flash zcast-admin-flash--error"
                                role="alert"
                                data-zcast-flash="error"
                            {
                                (message)
                            }
                        }

                        section
                            class="zcast-admin-card"
                            aria-labelledby="zcast-admin-settings-title"
                            data-zcast-settings-card
                        {
                            h2 id="zcast-admin-settings-title" { "Live Site Settings" }
                            p class="zcast-admin-hint" {
                                "Saved to the module store and applied immediately — no restart required."
                            }
                            form
                                class="zcast-admin-form"
                                method="post"
                                action="/_zcast/admin/settings"
                                data-zcast-settings-form
                            {
                                div class="zcast-admin-field" {
                                    label for="zcast-admin-site-title" { "Site title" }
                                    input
                                        id="zcast-admin-site-title"
                                        type="text"
                                        name="site_title"
                                        value=(settings.site_title)
                                        autocomplete="off"
                                        required;
                                }
                                div class="zcast-admin-field" {
                                    label for="zcast-admin-home-slug" { "Pinned home slug" }
                                    input
                                        id="zcast-admin-home-slug"
                                        type="text"
                                        name="pinned_home_slug"
                                        value=(settings.pinned_home_slug)
                                        autocomplete="off"
                                        required;
                                    span class="zcast-admin-field__hint" {
                                        "Slug served at / — for example “index”. “_zcast” is reserved."
                                    }
                                }
                                div class="zcast-admin-field" {
                                    label for="zcast-admin-color-mode" { "Colour mode" }
                                    select id="zcast-admin-color-mode" name="color_mode" {
                                        option value="auto" selected[color_mode == ThemeColorMode::Auto] {
                                            "Auto (follow system)"
                                        }
                                        option value="dark" selected[color_mode == ThemeColorMode::Dark] {
                                            "Dark only"
                                        }
                                        option value="light" selected[color_mode == ThemeColorMode::Light] {
                                            "Light only"
                                        }
                                    }
                                }
                                div class="zcast-admin-form__actions" {
                                    button
                                        type="submit"
                                        class="zcast-header-btn"
                                        data-zcast-save-settings
                                    {
                                        "Save settings"
                                    }
                                }
                            }
                        }

                        section
                            class="zcast-admin-card"
                            aria-labelledby="zcast-admin-graph-title"
                            data-zcast-module-graph
                        {
                            h2 id="zcast-admin-graph-title" {
                                "Module Dependency Graph & Slot Bindings"
                            }
                            p class="zcast-admin-hint" {
                                "Every active interface binding, every compiled module, and how they connect."
                            }

                            h3 class="zcast-admin-subtitle" { "Active interface bindings" }
                            @if snapshot.bindings.is_empty() {
                                p class="zcast-admin-empty" { "No interfaces are currently bound." }
                            } @else {
                                ul class="zcast-admin-bindings" {
                                    @for binding in &snapshot.bindings {
                                        li class="zcast-admin-binding" {
                                            code class="zcast-admin-code" { (binding.interface) }
                                            span class="zcast-admin-arrow" { "→" }
                                            strong { (binding.module_id) }
                                            span class="zcast-admin-badge" {
                                                "v" (binding.module_version)
                                            }
                                        }
                                    }
                                }
                            }

                            h3 class="zcast-admin-subtitle" { "Compiled modules" }
                            @if snapshot.modules.is_empty() {
                                p class="zcast-admin-empty" { "No modules are compiled in." }
                            } @else {
                                ul class="zcast-admin-modules" {
                                    @for module in &snapshot.modules {
                                        li
                                            class="zcast-admin-module"
                                            data-zcast-module=(module.id)
                                        {
                                            div class="zcast-admin-module__head" {
                                                code class="zcast-admin-code" { (module.id) }
                                                span class="zcast-admin-badge" {
                                                    "v" (module.version)
                                                }
                                            }
                                            p class="zcast-admin-module__description" {
                                                (module.description)
                                            }

                                            @if !module.provides.is_empty() {
                                                div class="zcast-admin-module__row" {
                                                    span class="zcast-admin-module__label" {
                                                        "provides"
                                                    }
                                                    div class="zcast-admin-badges" {
                                                        @for spec in module.provides {
                                                            span
                                                                class="zcast-admin-badge zcast-admin-badge--provides"
                                                                title=(spec.version)
                                                            {
                                                                (spec.name)
                                                            }
                                                        }
                                                    }
                                                }
                                            }

                                            @if !module.requires.is_empty() {
                                                div class="zcast-admin-module__row" {
                                                    span class="zcast-admin-module__label" {
                                                        "requires"
                                                    }
                                                    div class="zcast-admin-badges" {
                                                        @for req in module.requires {
                                                            span
                                                                class="zcast-admin-badge zcast-admin-badge--requires"
                                                                title=(req.version_req)
                                                            {
                                                                (req.name)
                                                                @if req.optional {
                                                                    " (optional)"
                                                                }
                                                                @if snapshot
                                                                    .bindings
                                                                    .iter()
                                                                    .any(|binding| binding.interface == req.name)
                                                                {
                                                                    span class="zcast-admin-status zcast-admin-status--ok" {
                                                                        "satisfied"
                                                                    }
                                                                } @else {
                                                                    span class="zcast-admin-status zcast-admin-status--unbound" {
                                                                        "unbound"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }

                                            @if !module.recommends.is_empty() {
                                                div class="zcast-admin-module__row" {
                                                    span class="zcast-admin-module__label" {
                                                        "recommends"
                                                    }
                                                    div class="zcast-admin-badges" {
                                                        @for recommendation in module.recommends {
                                                            span
                                                                class="zcast-admin-badge zcast-admin-badge--recommends"
                                                                data-zcast-recommendation=(recommendation)
                                                            {
                                                                (recommendation)
                                                                @if !snapshot
                                                                    .modules
                                                                    .iter()
                                                                    .any(|compiled| compiled.id == *recommendation)
                                                                {
                                                                    span class="zcast-admin-status zcast-admin-status--hint" {
                                                                        "not installed"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                @if module.recommends.contains(&"zcast-search-onnx") {
                                                    p class="zcast-admin-hint zcast-admin-hint--recommend" {
                                                        "Hint: install "
                                                        code { "zcast-search-onnx" }
                                                        " for local semantic and graph-boosted search."
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            h3 class="zcast-admin-subtitle" { "Dependency diagram" }
                            div class="zcast-admin-graph" {
                                (PreEscaped(diagram))
                            }
                            div class="zcast-admin-legend" {
                                span class="zcast-admin-legend__item" {
                                    span class="zcast-admin-legend__swatch zcast-admin-legend__swatch--provides" {}
                                    "provides"
                                }
                                span class="zcast-admin-legend__item" {
                                    span class="zcast-admin-legend__swatch zcast-admin-legend__swatch--requires" {}
                                    "requires"
                                }
                                span class="zcast-admin-legend__item" {
                                    span class="zcast-admin-legend__swatch zcast-admin-legend__swatch--optional" {}
                                    "optional requirement"
                                }
                            }

                            h3 class="zcast-admin-subtitle" { "Search capabilities" }
                            @if has_search {
                                @if let Some(manifest) = &search_manifest {
                                    p class="zcast-admin-hint" {
                                        "Active provider: "
                                        code { (manifest.id) }
                                        " v" (manifest.version)
                                    }
                                }
                                @if let Some(caps) = &capabilities {
                                    ul class="zcast-admin-capabilities" data-zcast-search-capabilities {
                                        (capability_item("keyword", caps.keyword))
                                        (capability_item("topical_semantic", caps.topical_semantic))
                                        (capability_item("graph_boosted", caps.graph_boosted))
                                        (capability_item(
                                            "instant_as_you_type",
                                            caps.instant_as_you_type,
                                        ))
                                    }
                                }
                            } @else {
                                p class="zcast-admin-empty" {
                                    "No search provider is bound. Keyword, semantic, and instant search are unavailable."
                                }
                            }
                        }

                        @if has_exporter {
                            section
                                class="zcast-admin-card"
                                aria-labelledby="zcast-admin-backup-title"
                                data-zcast-backup-card
                            {
                                h2 id="zcast-admin-backup-title" { "Backup & Export" }
                                p class="zcast-admin-hint" {
                                    "Every note as a Markdown file with YAML frontmatter, preserving "
                                    code { "[[wikilinks]]" }
                                    " and packaged as a single zip archive."
                                }
                                a
                                    href="/_zcast/api/export.zip"
                                    class="zcast-header-btn"
                                    data-zcast-export-zip
                                {
                                    "Download Markdown Backup (.zip)"
                                }
                                @if has_importer {
                                    p class="zcast-admin-hint" {
                                        "A Markdown importer is also bound and available for restoring archives."
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    document.into_string()
}

fn capability_item(name: &str, enabled: bool) -> Markup {
    html! {
        li class="zcast-admin-capability" {
            @if enabled {
                span class="zcast-admin-status zcast-admin-status--ok" { "on" }
            } @else {
                span class="zcast-admin-status zcast-admin-status--off" { "off" }
            }
            code { (name) }
        }
    }
}

/// Same pre-paint bootstrap the first-party theme uses for `auto` mode.
const AUTO_THEME_SCRIPT: &str = r#"(function(){var r=document.documentElement,s=null;try{s=localStorage.getItem('zcast-color-mode');}catch(e){}if(s!=='dark'&&s!=='light'){s=(window.matchMedia&&window.matchMedia('(prefers-color-scheme: light)').matches)?'light':'dark';}r.setAttribute('data-theme',s);})();"#;

const ADMIN_CSS: &str = r#"
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body.zcast-admin{
  margin:0;
  min-height:100vh;
  background:var(--zcast-bg,#fffdfa);
  color:var(--zcast-text,#1d1d1d);
  font-family:var(--zcast-font-body,"Libre Franklin",system-ui,-apple-system,sans-serif);
  font-size:var(--zcast-text-base,1.0625rem);
  line-height:var(--zcast-leading-body,1.7);
  -webkit-font-smoothing:antialiased;
}
a{color:var(--zcast-accent,#111)}
.zcast-admin-header{
  border-bottom:1px solid var(--zcast-border,#e5e5e3);
  background:var(--zcast-surface,#f7f7f7);
}
.zcast-admin-header__inner{
  max-width:1040px;
  margin:0 auto;
  padding:0.85rem clamp(1rem,4vw,2.5rem);
  display:flex;
  align-items:center;
  justify-content:space-between;
  gap:1rem;
}
.zcast-admin-header__brand{
  font-family:var(--zcast-font-heading,inherit);
  font-weight:700;
  color:var(--zcast-text-strong,var(--zcast-text,#111));
  text-decoration:none;
  letter-spacing:-0.01em;
}
.zcast-admin-header__nav{display:flex;align-items:center;gap:0.6rem}
.zcast-admin-header__logout{margin:0}
.zcast-admin-main{padding:clamp(1.5rem,4vw,3rem) clamp(1rem,4vw,2.5rem) 4rem}
.zcast-admin-main__inner{max-width:1040px;margin:0 auto;display:grid;gap:1.35rem}
.zcast-admin-heading{display:grid;gap:0.25rem}
.zcast-admin-eyebrow{
  margin:0;
  text-transform:uppercase;
  letter-spacing:0.12em;
  font-size:0.72rem;
  color:var(--zcast-text-muted,rgba(0,0,0,0.55));
}
.zcast-admin-title{margin:0;font-size:clamp(1.7rem,4vw,2.4rem);line-height:1.15}
.zcast-admin-lede{margin:0;color:var(--zcast-text-muted,rgba(0,0,0,0.6))}
.zcast-admin-card{
  border:1px solid var(--zcast-border,#e5e5e3);
  border-radius:var(--zcast-radius,6px);
  background:var(--zcast-surface,#f7f7f7);
  padding:clamp(1.1rem,3vw,1.75rem);
  display:grid;
  gap:0.85rem;
}
.zcast-admin-card h2{margin:0;font-size:1.2rem;line-height:1.3}
.zcast-admin-subtitle{
  margin:0.4rem 0 0;
  font-size:0.95rem;
  text-transform:uppercase;
  letter-spacing:0.08em;
  color:var(--zcast-text-muted,rgba(0,0,0,0.55));
}
.zcast-admin-hint{margin:0;color:var(--zcast-text-muted,rgba(0,0,0,0.6));font-size:0.92rem}
.zcast-admin-hint--recommend{margin-top:0.15rem}
.zcast-admin-empty{margin:0;color:var(--zcast-text-muted,rgba(0,0,0,0.6));font-style:italic}
.zcast-admin-form{display:grid;gap:1rem;max-width:34rem}
.zcast-admin-field{display:grid;gap:0.35rem}
.zcast-admin-field label{font-weight:600;font-size:0.92rem}
.zcast-admin-field__hint{font-size:0.82rem;color:var(--zcast-text-muted,rgba(0,0,0,0.55))}
.zcast-admin-field input,.zcast-admin-field select{
  width:100%;
  padding:0.55rem 0.65rem;
  border:1px solid var(--zcast-border,rgba(0,0,0,0.2));
  border-radius:calc(var(--zcast-radius,6px) - 1px);
  background:var(--zcast-bg,#fff);
  color:inherit;
  font:inherit;
}
.zcast-admin-field input:focus,.zcast-admin-field select:focus{
  outline:2px solid var(--zcast-accent,#111);
  outline-offset:1px;
}
.zcast-admin-form__actions{display:flex;gap:0.5rem}
.zcast-admin-bindings,.zcast-admin-capabilities{
  list-style:none;
  margin:0;
  padding:0;
  display:grid;
  gap:0.45rem;
}
.zcast-admin-binding{
  display:flex;
  flex-wrap:wrap;
  align-items:center;
  gap:0.5rem;
  padding:0.45rem 0.65rem;
  border:1px solid var(--zcast-border,#e5e5e3);
  border-radius:calc(var(--zcast-radius,6px) - 1px);
  background:var(--zcast-surface-elevated,var(--zcast-bg,#fff));
}
.zcast-admin-arrow{color:var(--zcast-text-muted,rgba(0,0,0,0.5))}
.zcast-admin-code{
  font-family:var(--zcast-font-mono,ui-monospace,monospace);
  font-size:0.82rem;
  background:var(--zcast-code-bg,rgba(0,0,0,0.08));
  padding:0.1rem 0.35rem;
  border-radius:4px;
}
.zcast-admin-badge{
  display:inline-flex;
  align-items:center;
  gap:0.35rem;
  padding:0.1rem 0.45rem;
  border:1px solid var(--zcast-border,rgba(0,0,0,0.18));
  border-radius:999px;
  font-size:0.78rem;
  line-height:1.5;
  white-space:nowrap;
}
.zcast-admin-badge--provides{border-color:var(--zcast-accent,#111)}
.zcast-admin-badge--recommends{border-style:dashed}
.zcast-admin-modules{list-style:none;margin:0;padding:0;display:grid;gap:0.65rem}
.zcast-admin-module{
  border:1px solid var(--zcast-border,#e5e5e3);
  border-radius:calc(var(--zcast-radius,6px) - 1px);
  background:var(--zcast-surface-elevated,var(--zcast-bg,#fff));
  padding:0.75rem 0.85rem;
  display:grid;
  gap:0.45rem;
}
.zcast-admin-module__head{display:flex;align-items:center;gap:0.5rem;flex-wrap:wrap}
.zcast-admin-module__description{margin:0;font-size:0.92rem}
.zcast-admin-module__row{display:flex;flex-wrap:wrap;gap:0.4rem 0.6rem;align-items:center}
.zcast-admin-module__label{
  min-width:5.5rem;
  font-size:0.76rem;
  text-transform:uppercase;
  letter-spacing:0.08em;
  color:var(--zcast-text-muted,rgba(0,0,0,0.55));
}
.zcast-admin-badges{display:flex;flex-wrap:wrap;gap:0.35rem}
.zcast-admin-status{font-size:0.7rem;text-transform:uppercase;letter-spacing:0.06em}
.zcast-admin-status--ok{color:var(--zcast-accent,#1a7f37)}
.zcast-admin-status--unbound{color:#b3261e}
.zcast-admin-status--off{color:var(--zcast-text-muted,rgba(0,0,0,0.5))}
.zcast-admin-status--hint{color:var(--zcast-text-muted,rgba(0,0,0,0.55))}
.zcast-admin-graph{
  overflow-x:auto;
  border:1px solid var(--zcast-border,#e5e5e3);
  border-radius:calc(var(--zcast-radius,6px) - 1px);
  background:var(--zcast-bg,#fff);
  padding:0.5rem;
}
.zcast-admin-graph__svg{display:block;width:100%;min-width:720px;height:auto}
.zcast-admin-graph__node rect{
  fill:var(--zcast-surface-elevated,var(--zcast-surface,#f7f7f7));
  stroke:var(--zcast-border,rgba(0,0,0,0.2));
}
.zcast-admin-graph__node text{
  fill:var(--zcast-text,#111);
  font-family:var(--zcast-font-mono,ui-monospace,monospace);
  font-size:11px;
}
.zcast-admin-graph__node--interface rect{fill:var(--zcast-surface,var(--zcast-bg,#f7f7f7))}
.zcast-admin-graph__edge{fill:none;stroke-width:1.6}
.zcast-admin-graph__edge--provides{stroke:var(--zcast-accent,#111)}
.zcast-admin-graph__edge--requires{stroke:var(--zcast-text-muted,rgba(0,0,0,0.45));stroke-dasharray:5 4}
.zcast-admin-graph__edge--optional{stroke:var(--zcast-text-muted,rgba(0,0,0,0.35));stroke-dasharray:2 4}
.zcast-admin-legend{display:flex;flex-wrap:wrap;gap:0.9rem;font-size:0.8rem;color:var(--zcast-text-muted,rgba(0,0,0,0.6))}
.zcast-admin-legend__item{display:inline-flex;align-items:center;gap:0.35rem}
.zcast-admin-legend__swatch{width:1.4rem;height:0;border-top:2px solid var(--zcast-accent,#111)}
.zcast-admin-legend__swatch--requires{border-top-style:dashed;border-top-color:var(--zcast-text-muted,rgba(0,0,0,0.45))}
.zcast-admin-legend__swatch--optional{border-top-style:dotted;border-top-color:var(--zcast-text-muted,rgba(0,0,0,0.35))}
.zcast-admin-capability{display:flex;align-items:center;gap:0.5rem}
.zcast-admin-flash{
  border-radius:var(--zcast-radius,6px);
  border:1px solid var(--zcast-border,#e5e5e3);
  padding:0.65rem 0.85rem;
  font-size:0.94rem;
}
.zcast-admin-flash--success{border-color:var(--zcast-accent,#1a7f37);background:var(--zcast-accent-subtle,rgba(0,0,0,0.06))}
.zcast-admin-flash--error{border-color:#b3261e;background:rgba(179,38,30,0.08);color:#8c1d18}
.zcast-header-btn{
  display:inline-flex;
  align-items:center;
  gap:0.35rem;
  padding:0.42rem 0.8rem;
  border:1px solid var(--zcast-border,rgba(0,0,0,0.2));
  border-radius:var(--zcast-radius,6px);
  background:var(--zcast-surface-elevated,var(--zcast-bg,#fff));
  color:var(--zcast-text,#111);
  font:inherit;
  font-size:0.9rem;
  font-weight:600;
  text-decoration:none;
  cursor:pointer;
}
.zcast-header-btn:hover{border-color:var(--zcast-accent,#111)}
.zcast-header-btn:focus-visible{outline:2px solid var(--zcast-accent,#111);outline-offset:2px}
@media (max-width:640px){
  .zcast-admin-header__inner{flex-direction:column;align-items:flex-start}
  .zcast-admin-module__label{min-width:auto}
}
"#;

/// Render a compact inline SVG mapping each compiled module to its
/// provided/required interfaces.
fn dependency_diagram(snapshot: &RegistrySnapshot) -> String {
    const BOX_W: i32 = 260;
    const BOX_H: i32 = 30;
    const GAP: i32 = 14;
    const LEFT_X: i32 = 16;
    const RIGHT_X: i32 = 588;
    const WIDTH: i32 = 864;
    const TOP: i32 = 26;

    let mut interfaces: Vec<&'static str> = Vec::new();
    for module in &snapshot.modules {
        for spec in module.provides {
            if !interfaces.contains(&spec.name) {
                interfaces.push(spec.name);
            }
        }
        for req in module.requires {
            if !interfaces.contains(&req.name) {
                interfaces.push(req.name);
            }
        }
    }

    let rows = snapshot.modules.len().max(interfaces.len()).max(1);
    let height = TOP + rows as i32 * (BOX_H + GAP) + 10;
    let row_y = |index: usize| TOP + index as i32 * (BOX_H + GAP);
    let interface_row = |name: &str| interfaces.iter().position(|candidate| *candidate == name);

    let mut svg = String::with_capacity(2048 + snapshot.modules.len() * 160);
    let _ = write!(
        svg,
        "<svg class=\"zcast-admin-graph__svg\" viewBox=\"0 0 {WIDTH} {height}\" \
         role=\"img\" aria-labelledby=\"zcast-admin-diagram-title\" \
         data-zcast-dependency-svg>\
         <title id=\"zcast-admin-diagram-title\">Module to interface dependency diagram</title>"
    );

    // Edges first so node boxes paint cleanly over the connectors.
    for (module_index, module) in snapshot.modules.iter().enumerate() {
        let y1 = row_y(module_index) + BOX_H / 2;
        for spec in module.provides {
            if let Some(interface_index) = interface_row(spec.name) {
                let y2 = row_y(interface_index) + BOX_H / 2;
                let _ = write!(
                    svg,
                    "<path class=\"zcast-admin-graph__edge zcast-admin-graph__edge--provides\" \
                     d=\"M {} {} C {} {} {} {} {} {}\"/>",
                    LEFT_X + BOX_W,
                    y1,
                    LEFT_X + BOX_W + 70,
                    y1,
                    RIGHT_X - 70,
                    y2,
                    RIGHT_X,
                    y2
                );
            }
        }
        for req in module.requires {
            if let Some(interface_index) = interface_row(req.name) {
                let y2 = row_y(interface_index) + BOX_H / 2;
                let class = if req.optional {
                    "zcast-admin-graph__edge zcast-admin-graph__edge--optional"
                } else {
                    "zcast-admin-graph__edge zcast-admin-graph__edge--requires"
                };
                let _ = write!(
                    svg,
                    "<path class=\"{class}\" d=\"M {} {} C {} {} {} {} {} {}\"/>",
                    LEFT_X + BOX_W,
                    y1,
                    LEFT_X + BOX_W + 70,
                    y1,
                    RIGHT_X - 70,
                    y2,
                    RIGHT_X,
                    y2
                );
            }
        }
    }

    for (index, module) in snapshot.modules.iter().enumerate() {
        let _ = write!(
            svg,
            "<g class=\"zcast-admin-graph__node zcast-admin-graph__node--module\">\
             <rect x=\"{LEFT_X}\" y=\"{}\" width=\"{BOX_W}\" height=\"{BOX_H}\" rx=\"6\"/>\
             <text x=\"{}\" y=\"{}\">{}</text></g>",
            row_y(index),
            LEFT_X + 12,
            row_y(index) + BOX_H / 2 + 4,
            escape_xml(module.id)
        );
    }

    for (index, interface) in interfaces.iter().enumerate() {
        let _ = write!(
            svg,
            "<g class=\"zcast-admin-graph__node zcast-admin-graph__node--interface\">\
             <rect x=\"{RIGHT_X}\" y=\"{}\" width=\"{BOX_W}\" height=\"{BOX_H}\" rx=\"6\"/>\
             <text x=\"{}\" y=\"{}\">{}</text></g>",
            row_y(index),
            RIGHT_X + 12,
            row_y(index) + BOX_H / 2 + 4,
            escape_xml(interface)
        );
    }

    svg.push_str("</svg>");
    svg
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
