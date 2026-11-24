//! `xcaddy`-style module management for zcast (DEC-05 / DEC-20).
//!
//! Modules are plain Rust crates compiled into a thin generated shim crate. The
//! lock file (`zcast-modules.toml`) records the pinned core version, the active
//! interface bindings, and every extra crate that must be compiled in. Builds
//! happen in a warm `target/` directory and the resulting binary is swapped in
//! atomically, keeping the previous binary as `zcast.prev` for rollback.
#![allow(dead_code)] // Wired into `main.rs` by the coordinator.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use zcast_kernel::RegistrySnapshot;

/// Canonical lock file name, persisted alongside `zcast.toml`.
pub const MODULE_LOCK_FILE: &str = "zcast-modules.toml";

const ZCAST_REPO: &str = "https://github.com/zcast-cms/zcast.git";
const ZCAST_INTERFACES_REPO: &str = "https://github.com/zcast-cms/zcast-interfaces.git";
const ZCAST_THEME_REPO: &str = "https://github.com/zcast-cms/zcast-theme-davidhewitson.git";

/// Core crates that every custom build compiles in.
const CORE_CRATES: &[&str] = &[
    "zcast-cli",
    "zcast-interfaces",
    "zcast-kernel",
    "zcast-storage-sqlite",
    "zcast-links-wikilink",
    "zcast-editor-inline",
    "zcast-auth-basic",
    "zcast-auth-magic-link",
    "zcast-popover",
    "zcast-search-fts",
    "zcast-cmd-k",
    "zcast-io-markdown",
    "zcast-theme-davidhewitson",
];

/// Modules that a build cannot function without. They can only be swapped for
/// an equivalent provider via `zcast module bind`, never switched off.
pub const MANDATORY_MODULES: &[&str] = &[
    "zcast-storage-sqlite",
    "zcast-storage-mysql",
    "zcast-links-wikilink",
    "zcast-theme-davidhewitson",
];

/// Bundled modules that ship with zcast and can be deactivated at runtime.
pub const OPTIONAL_BUNDLED_MODULES: &[&str] = &[
    "zcast-editor-inline",
    "zcast-popover",
    "zcast-cmd-k",
    "zcast-io-markdown",
    "zcast-search-fts",
    "zcast-search-onnx",
    "zcast-auth-basic",
    "zcast-auth-magic-link",
];

/// Search provider used when no `zcast:search/provider` binding is recorded.
pub const DEFAULT_SEARCH_MODULE: &str = "zcast-search-fts";

/// True when `id` is a bundled module that must stay compiled in.
pub fn is_mandatory_module(id: &str) -> bool {
    let id = id.trim();
    MANDATORY_MODULES.contains(&id)
}

/// True when `id` is a bundled module that can be deactivated/removed.
pub fn is_optional_bundled_module(id: &str) -> bool {
    let id = id.trim();
    OPTIONAL_BUNDLED_MODULES.contains(&id)
}

/// True when `id` is any bundled (mandatory or optional) module.
pub fn is_bundled_module(id: &str) -> bool {
    is_mandatory_module(id) || is_optional_bundled_module(id)
}

/// Normalise a search provider choice (`fts`, `onnx`, module ID) to a module ID.
pub fn normalize_search_module_id(id: &str) -> &str {
    match id.trim() {
        "fts" | "zcast-search-fts" => "zcast-search-fts",
        "onnx" | "semantic" | "zcast-search-onnx" => "zcast-search-onnx",
        other => other,
    }
}

/// Normalise an auth provider choice (`basic`, `magic-link`, module ID) to a module ID.
pub fn normalize_auth_module_id(id: &str) -> Option<&'static str> {
    match id.trim() {
        "basic" | "zcast-auth-basic" => Some("zcast-auth-basic"),
        "magic-link" | "zcast-auth-magic-link" => Some("zcast-auth-magic-link"),
        _ => None,
    }
}

/// Persisted module selection (`zcast-modules.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleLockConfig {
    pub core_version: String,
    /// Active interface binding: interface name -> module crate/ID.
    pub bindings: BTreeMap<String, String>,
    /// Additional crates compiled into the custom binary.
    pub extra_crates: Vec<ModuleCrateSpec>,
    /// Bundled modules switched off at runtime (kept compiled, never registered).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled: Vec<String>,
}

/// A single extra crate compiled into a custom zcast binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleCrateSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl ModuleLockConfig {
    /// True when the bundled module `id` has been deactivated.
    pub fn is_disabled(&self, id: &str) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        self.disabled.iter().any(|disabled| disabled.trim() == id)
    }

    /// The search module that will actually be registered at runtime, if any.
    pub fn active_search_module(&self) -> Option<&str> {
        let bound = self
            .bindings
            .get(zcast_interfaces::IFACE_SEARCH)
            .map(String::as_str)
            .map(str::trim)
            .filter(|bound| !bound.is_empty());

        let candidate = match bound {
            Some("none") | Some("off") => return None,
            Some(other) => normalize_search_module_id(other),
            None => DEFAULT_SEARCH_MODULE,
        };

        if self.is_disabled(candidate) {
            None
        } else {
            Some(candidate)
        }
    }

    /// Deactivate a bundled module (or an already-installed extra crate).
    pub fn deactivate_module(&mut self, id: &str) -> Result<String, String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("module id cannot be empty".to_owned());
        }
        if is_mandatory_module(id) {
            return Err(format!(
                "cannot deactivate mandatory module '{id}' (swap it instead via 'zcast module bind <interface> <replacement>')"
            ));
        }
        let is_extra = self.extra_crates.iter().any(|spec| spec.name == id);
        if !is_optional_bundled_module(id) && !is_extra {
            return Err(format!(
                "unknown module '{id}' (run 'zcast module list' to see installed modules)"
            ));
        }

        if !self.is_disabled(id) {
            // Dependency guard: never strand a module that requires a search provider.
            let mut probe = self.clone();
            probe.disabled.push(id.to_owned());
            if self.active_search_module().is_some()
                && probe.active_search_module().is_none()
                && !self.is_disabled("zcast-cmd-k")
            {
                return Err(format!(
                    "cannot deactivate '{id}': active module 'zcast-cmd-k' requires 'zcast:search/provider@^1.0' (deactivate 'zcast-cmd-k' first, or switch search via 'zcast module activate zcast-search-onnx')"
                ));
            }
        }

        if self.is_disabled(id) {
            return Ok(format!("Module '{id}' is already deactivated."));
        }

        self.disabled.push(id.to_owned());
        self.disabled.sort();
        self.disabled.dedup();

        Ok(format!(
            "Deactivated module '{id}'. Restart zcast to apply, or re-enable anytime with 'zcast module activate {id}'."
        ))
    }

    /// Re-activate a previously deactivated bundled module (or extra crate).
    pub fn activate_module(&mut self, id: &str) -> Result<String, String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("module id cannot be empty".to_owned());
        }
        let is_extra = self.extra_crates.iter().any(|spec| spec.name == id);
        if !is_bundled_module(id) && !is_extra {
            return Err(format!(
                "module '{id}' is not installed (install it first with 'zcast module install <id>')"
            ));
        }
        if id == "zcast-cmd-k" && self.active_search_module().is_none() {
            return Err(
                "cannot activate 'zcast-cmd-k': requires 'zcast:search/provider@^1.0' (activate 'zcast-search-fts' or 'zcast-search-onnx' first)"
                    .to_owned(),
            );
        }

        let was_disabled = self.is_disabled(id);
        if was_disabled {
            self.disabled.retain(|disabled| disabled.trim() != id);
        }

        // Smart slot binding: activating a provider fills its interface slot.
        match id {
            "zcast-search-fts" | "zcast-search-onnx" => {
                self.bindings
                    .insert(zcast_interfaces::IFACE_SEARCH.to_owned(), id.to_owned());
            }
            "zcast-auth-basic" | "zcast-auth-magic-link" => {
                self.bindings
                    .insert(zcast_interfaces::IFACE_AUTH.to_owned(), id.to_owned());
            }
            _ => {}
        }

        if was_disabled {
            Ok(format!("Activated module '{id}'. Restart zcast to apply."))
        } else {
            Ok(format!("Module '{id}' is already active."))
        }
    }

    /// Load a lock file from disk.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        toml::from_str(&raw).map_err(|e| format!("failed to parse {}: {e}", path.display()))
    }

    /// Persist the lock file as pretty TOML.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let raw = toml::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize module lock: {e}"))?;
        fs::write(path, raw).map_err(|e| format!("failed to write {}: {e}", path.display()))
    }
}

/// Parse a module spec from the CLI.
///
/// Supported forms:
/// - `zcast-foo`
/// - `zcast-foo@0.4.0`
/// - `zcast-foo=git+https://example.com/zcast-foo.git`
/// - `zcast-foo=path:../zcast-foo`
pub fn parse_module_spec(raw: &str) -> Result<ModuleCrateSpec, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("module spec cannot be empty".to_owned());
    }

    let (name, source) = match raw.split_once('=') {
        Some((name, source)) => (name.trim(), Some(source.trim())),
        None => (raw, None),
    };

    let (name, version) = match name.split_once('@') {
        Some((name, version)) => {
            let name = name.trim();
            let version = version.trim();
            if version.is_empty() {
                return Err(format!("module spec '{raw}' has an empty version"));
            }
            (name, Some(version.to_owned()))
        }
        None => (name, None),
    };

    if name.is_empty() {
        return Err(format!("module spec '{raw}' is missing a crate name"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(format!(
            "module spec '{raw}' has an invalid crate name '{name}'"
        ));
    }

    let mut spec = ModuleCrateSpec {
        name: name.to_owned(),
        version,
        git: None,
        path: None,
    };

    if let Some(source) = source {
        if source.is_empty() {
            return Err(format!("module spec '{raw}' has an empty source"));
        }
        if let Some(path) = source.strip_prefix("path:") {
            let path = path.trim();
            if path.is_empty() {
                return Err(format!("module spec '{raw}' has an empty path"));
            }
            spec.path = Some(path.to_owned());
            // A path dependency cannot also pin a registry version.
            spec.version = None;
        } else if let Some(url) = source
            .strip_prefix("git+")
            .or_else(|| source.strip_prefix("git:"))
        {
            let url = url.trim();
            if url.is_empty() {
                return Err(format!("module spec '{raw}' has an empty git URL"));
            }
            spec.git = Some(url.to_owned());
        } else if source.starts_with("http://")
            || source.starts_with("https://")
            || source.starts_with("ssh://")
            || source.starts_with("git@")
        {
            spec.git = Some(source.to_owned());
        } else {
            spec.version = Some(source.to_owned());
        }
    }

    Ok(spec)
}

fn branch_for(count: usize, index: usize) -> &'static str {
    if index + 1 == count {
        "└──"
    } else {
        "├──"
    }
}

fn child_prefix(count: usize, index: usize) -> &'static str {
    if index + 1 == count { "    " } else { "│   " }
}

fn is_bound(snapshot: &RegistrySnapshot, interface: &str) -> bool {
    snapshot.bindings.iter().any(|b| b.interface == interface)
}

/// Render a readable dependency graph for `zcast module graph`.
pub fn render_module_graph_ascii(snapshot: &RegistrySnapshot) -> String {
    let mut out = String::new();
    out.push_str("zcast module graph\n");
    out.push_str("==================\n\n");

    out.push_str(&format!(
        "Active interface bindings ({})\n",
        snapshot.bindings.len()
    ));
    if snapshot.bindings.is_empty() {
        out.push_str("    (none)\n");
    } else {
        let count = snapshot.bindings.len();
        for (i, binding) in snapshot.bindings.iter().enumerate() {
            out.push_str(&format!(
                "{} {} -> {} ({})\n",
                branch_for(count, i),
                binding.interface,
                binding.module_id,
                binding.module_version
            ));
        }
    }
    out.push('\n');

    out.push_str(&format!("Compiled modules ({})\n", snapshot.modules.len()));
    if snapshot.modules.is_empty() {
        out.push_str("    (none)\n");
        return out;
    }

    let module_count = snapshot.modules.len();
    for (i, module) in snapshot.modules.iter().enumerate() {
        out.push_str(&format!(
            "{} {} v{}  {}\n",
            branch_for(module_count, i),
            module.id,
            module.version,
            module.name
        ));
        let prefix = child_prefix(module_count, i);

        if module.provides.is_empty() {
            out.push_str(&format!("{prefix}    provides: (none)\n"));
        } else {
            out.push_str(&format!("{prefix}    provides:\n"));
            for spec in module.provides {
                out.push_str(&format!(
                    "{prefix}        ✓ {}@{}\n",
                    spec.name, spec.version
                ));
            }
        }

        if module.requires.is_empty() {
            out.push_str(&format!("{prefix}    requires: (none)\n"));
        } else {
            out.push_str(&format!("{prefix}    requires:\n"));
            for req in module.requires {
                let satisfied = is_bound(snapshot, req.name);
                let mark = if satisfied { '✓' } else { '✗' };
                let hint = if satisfied {
                    ""
                } else if req.optional {
                    "  (optional — not bound)"
                } else {
                    "  (MISSING — install a provider)"
                };
                out.push_str(&format!(
                    "{prefix}        {mark} {}@{}{hint}\n",
                    req.name, req.version_req
                ));
            }
        }

        if !module.recommends.is_empty() {
            out.push_str(&format!("{prefix}    recommends:\n"));
            for suggestion in module.recommends {
                out.push_str(&format!(
                    "{prefix}        → {suggestion}  (optional upgrade: `zcast module install {suggestion}`)\n"
                ));
            }
        }
    }

    out
}

fn normalized_version(version: &str) -> &str {
    let version = version.trim();
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
}

fn escape_toml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn core_dependency(name: &str, version: &str) -> String {
    let repo = match name {
        "zcast-interfaces" => ZCAST_INTERFACES_REPO,
        "zcast-theme-davidhewitson" => ZCAST_THEME_REPO,
        _ => ZCAST_REPO,
    };
    format!("{name} = {{ git = \"{repo}\", tag = \"v{version}\" }}")
}

fn render_extra_dependency(spec: &ModuleCrateSpec) -> String {
    let mut fields: Vec<String> = Vec::new();
    if let Some(version) = spec.version.as_deref().filter(|v| !v.trim().is_empty()) {
        fields.push(format!("version = \"{}\"", escape_toml(version.trim())));
    }
    if let Some(git) = spec.git.as_deref().filter(|g| !g.trim().is_empty()) {
        fields.push(format!("git = \"{}\"", escape_toml(git.trim())));
    }
    if let Some(path) = spec.path.as_deref().filter(|p| !p.trim().is_empty()) {
        fields.push(format!("path = \"{}\"", escape_toml(path.trim())));
    }
    if fields.is_empty() {
        format!("{} = \"*\"", spec.name)
    } else {
        format!("{} = {{ {} }}", spec.name, fields.join(", "))
    }
}

/// Generate the shim crate (`Cargo.toml` + `src/main.rs`) for a custom build.
pub fn generate_shim_crate(build_dir: &Path, cfg: &ModuleLockConfig) -> Result<(), std::io::Error> {
    fs::create_dir_all(build_dir.join("src"))?;

    let version = normalized_version(&cfg.core_version);
    let version = if version.is_empty() { "0.0.0" } else { version };

    let mut manifest = String::new();
    manifest.push_str("# Generated by `zcast module` — do not edit by hand.\n");
    manifest.push_str("# Re-run `zcast module install <spec>` to regenerate this shim crate.\n");
    manifest.push_str("#\n");
    manifest.push_str("# Active interface bindings:\n");
    if cfg.bindings.is_empty() {
        manifest.push_str("#   (none)\n");
    } else {
        for (interface, module) in &cfg.bindings {
            manifest.push_str(&format!("#   {interface} -> {module}\n"));
        }
    }
    manifest.push('\n');
    manifest.push_str("[package]\n");
    manifest.push_str("name = \"zcast-custom\"\n");
    manifest.push_str(&format!("version = \"{version}\"\n"));
    manifest.push_str("edition = \"2024\"\n");
    manifest.push_str("license = \"MIT\"\n\n");
    manifest.push_str("[[bin]]\n");
    manifest.push_str("name = \"zcast\"\n");
    manifest.push_str("path = \"src/main.rs\"\n\n");
    manifest.push_str("[dependencies]\n");
    for name in CORE_CRATES {
        if cfg.is_disabled(name) {
            manifest.push_str(&format!("# {name} = deactivated via zcast-modules.toml\n"));
            continue;
        }
        manifest.push_str(&core_dependency(name, version));
        manifest.push('\n');
    }
    if !cfg.extra_crates.is_empty() {
        manifest.push('\n');
        manifest.push_str("# Additional modules selected via `zcast module install`.\n");
        for spec in &cfg.extra_crates {
            manifest.push_str(&render_extra_dependency(spec));
            manifest.push('\n');
        }
    }
    manifest.push_str(
        "\n[profile.release]\n\
         opt-level = 3\n\
         lto = \"thin\"\n\
         codegen-units = 1\n\
         strip = \"symbols\"\n\
         panic = \"abort\"\n",
    );

    fs::write(build_dir.join("Cargo.toml"), manifest)?;

    let mut main_rs = String::new();
    main_rs.push_str("//! Generated by `zcast module` — do not edit by hand.\n");
    main_rs.push_str("//!\n");
    main_rs.push_str("//! This shim only exists to pull the selected module crates into one\n");
    main_rs.push_str("//! native binary. All runtime behaviour lives in `zcast-kernel`, and the\n");
    main_rs.push_str("//! stock CLI entrypoint is provided by `zcast-cli`.\n");
    main_rs.push_str("#![allow(dead_code)]\n\n");
    main_rs.push_str("/// Interface bindings compiled into this binary.\n");
    main_rs.push_str("pub const BINDINGS: &[(&str, &str)] = &[\n");
    for (interface, module) in &cfg.bindings {
        main_rs.push_str(&format!(
            "    (\"{}\", \"{}\"),\n",
            escape_toml(interface),
            escape_toml(module)
        ));
    }
    main_rs.push_str("];\n\n");
    main_rs.push_str("fn main() -> Result<(), Box<dyn std::error::Error>> {\n");
    main_rs.push_str("    zcast_cli::main_entry()\n");
    main_rs.push_str("}\n");

    fs::write(build_dir.join("src").join("main.rs"), main_rs)?;
    Ok(())
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

fn ensure_parent_dir(path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

/// Atomically replace `current_bin` with `new_bin`, keeping a `.prev` backup.
pub fn atomic_swap_binary(new_bin: &Path, current_bin: &Path) -> Result<PathBuf, std::io::Error> {
    ensure_parent_dir(current_bin)?;

    let prev_bin = sibling_with_suffix(current_bin, ".prev");
    let staged_bin = sibling_with_suffix(current_bin, ".new");

    if current_bin.exists() {
        fs::copy(current_bin, &prev_bin)?;
    }

    // Stage next to the destination so the final rename is same-filesystem.
    fs::copy(new_bin, &staged_bin)?;
    make_executable(&staged_bin)?;
    fs::rename(&staged_bin, current_bin)?;

    Ok(prev_bin)
}

/// Roll back to the previous binary by atomically renaming it over the current one.
pub fn rollback_binary(prev_bin: &Path, current_bin: &Path) -> Result<(), std::io::Error> {
    ensure_parent_dir(current_bin)?;
    fs::rename(prev_bin, current_bin)
}

/// Where `cargo build --release` places the shim binary.
pub fn expected_binary_path(build_dir: &Path) -> PathBuf {
    let name = if cfg!(windows) { "zcast.exe" } else { "zcast" };
    build_dir.join("target").join("release").join(name)
}

/// Generate the shim crate and compile a custom binary (or preview the path on `dry_run`).
pub fn build_custom_binary(
    build_dir: &Path,
    cfg: &ModuleLockConfig,
    jobs: Option<usize>,
    dry_run: bool,
) -> Result<PathBuf, String> {
    if jobs == Some(0) {
        return Err("CARGO_BUILD_JOBS must be at least 1".to_owned());
    }

    generate_shim_crate(build_dir, cfg).map_err(|e| {
        format!(
            "failed to generate shim crate in {}: {e}",
            build_dir.display()
        )
    })?;

    let binary_path = expected_binary_path(build_dir);
    if dry_run {
        return Ok(binary_path);
    }

    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--release")
        .arg("--bin")
        .arg("zcast")
        .current_dir(build_dir);
    if let Some(jobs) = jobs {
        command.env("CARGO_BUILD_JOBS", jobs.to_string());
    }

    let output = command.output().map_err(|e| {
        format!(
            "failed to run cargo in {} (is the Rust toolchain installed?): {e}",
            build_dir.display()
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "cargo build failed (exit {}):\n{tail}",
            output.status
        ));
    }

    if !binary_path.exists() {
        return Err(format!(
            "cargo reported success but {} is missing",
            binary_path.display()
        ));
    }

    Ok(binary_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use zcast_interfaces::{InterfaceReq, InterfaceSpec, ModuleManifest};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zcast-cli-modules-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_plain_name() {
        let spec = parse_module_spec("zcast-foo").expect("parse");
        assert_eq!(spec.name, "zcast-foo");
        assert_eq!(spec.version, None);
        assert_eq!(spec.git, None);
        assert_eq!(spec.path, None);
    }

    #[test]
    fn parses_pinned_version() {
        let spec = parse_module_spec("zcast-foo@0.4.0").expect("parse");
        assert_eq!(spec.name, "zcast-foo");
        assert_eq!(spec.version.as_deref(), Some("0.4.0"));
        assert_eq!(spec.git, None);
    }

    #[test]
    fn parses_git_source() {
        let spec =
            parse_module_spec("zcast-foo=git+https://example.com/zcast-foo.git").expect("parse");
        assert_eq!(spec.name, "zcast-foo");
        assert_eq!(
            spec.git.as_deref(),
            Some("https://example.com/zcast-foo.git")
        );
        assert_eq!(spec.path, None);
    }

    #[test]
    fn parses_path_source() {
        let spec = parse_module_spec("zcast-foo=path:../foo").expect("parse");
        assert_eq!(spec.name, "zcast-foo");
        assert_eq!(spec.path.as_deref(), Some("../foo"));
        assert_eq!(spec.git, None);
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(parse_module_spec("").is_err());
        assert!(parse_module_spec("zcast foo").is_err());
        assert!(parse_module_spec("zcast-foo@").is_err());
        assert!(parse_module_spec("zcast-foo=").is_err());
        assert!(parse_module_spec("zcast-foo=path:").is_err());
    }

    static STORAGE_PROVIDES: &[InterfaceSpec] = &[InterfaceSpec {
        name: "zcast:storage/repository",
        version: "1.0.0",
    }];
    static STORAGE_REQUIRES: &[InterfaceReq] = &[];
    static STORAGE_RECOMMENDS: &[&str] = &["zcast-storage-mysql"];

    static CMD_K_PROVIDES: &[InterfaceSpec] = &[InterfaceSpec {
        name: "zcast:ui/command-bar",
        version: "1.0.0",
    }];
    static CMD_K_REQUIRES: &[InterfaceReq] = &[
        InterfaceReq {
            name: "zcast:search/provider",
            version_req: "^1.0",
            optional: false,
        },
        InterfaceReq {
            name: "zcast:ui/editor",
            version_req: "^1.0",
            optional: true,
        },
    ];
    static CMD_K_RECOMMENDS: &[&str] = &["zcast-search-onnx"];

    fn storage_manifest() -> ModuleManifest {
        ModuleManifest {
            id: "zcast-storage-sqlite",
            name: "SQLite Storage",
            version: "0.4.0",
            description: "SQLite storage provider",
            provides: STORAGE_PROVIDES,
            requires: STORAGE_REQUIRES,
            recommends: STORAGE_RECOMMENDS,
        }
    }

    fn cmd_k_manifest() -> ModuleManifest {
        ModuleManifest {
            id: "zcast-cmd-k",
            name: "Command Bar",
            version: "0.4.0",
            description: "Spotlight command bar",
            provides: CMD_K_PROVIDES,
            requires: CMD_K_REQUIRES,
            recommends: CMD_K_RECOMMENDS,
        }
    }

    #[test]
    fn renders_graph_with_satisfied_and_missing_requirements() {
        use zcast_kernel::SlotBinding;

        let snapshot = RegistrySnapshot {
            modules: vec![storage_manifest(), cmd_k_manifest()],
            bindings: vec![SlotBinding {
                interface: "zcast:storage/repository",
                module_id: "zcast-storage-sqlite",
                module_version: "0.4.0",
            }],
        };

        let graph = render_module_graph_ascii(&snapshot);
        assert!(graph.contains("zcast module graph"));
        assert!(graph.contains("Active interface bindings (1)"));
        assert!(graph.contains("zcast:storage/repository -> zcast-storage-sqlite (0.4.0)"));
        assert!(graph.contains("Compiled modules (2)"));
        assert!(graph.contains("zcast-storage-sqlite v0.4.0"));
        assert!(graph.contains("provides:"));
        assert!(graph.contains("✓ zcast:storage/repository@1.0.0"));
        assert!(!graph.contains("✓ zcast:search/provider@^1.0"));
        assert!(graph.contains("✗ zcast:search/provider@^1.0"));
        assert!(graph.contains("✗ zcast:ui/editor@^1.0"));
        assert!(graph.contains("(optional — not bound)"));
        assert!(graph.contains("→ zcast-search-onnx"));
        assert!(graph.contains("zcast module install zcast-search-onnx"));
    }

    #[test]
    fn renders_empty_graph_cleanly() {
        let snapshot = RegistrySnapshot {
            modules: vec![],
            bindings: vec![],
        };
        let graph = render_module_graph_ascii(&snapshot);
        assert!(graph.contains("(none)"));
        assert!(!graph.contains('✗'));
    }

    #[test]
    fn generates_shim_crate_files() {
        let dir = temp_dir("shim");
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "zcast:storage/repository".to_owned(),
            "zcast-storage-sqlite".to_owned(),
        );
        let cfg = ModuleLockConfig {
            core_version: "v0.4.0".to_owned(),
            bindings,
            disabled: Vec::new(),
            extra_crates: vec![
                ModuleCrateSpec {
                    name: "zcast-extra-git".to_owned(),
                    version: None,
                    git: Some("https://example.com/zcast-extra-git.git".to_owned()),
                    path: None,
                },
                ModuleCrateSpec {
                    name: "zcast-extra-path".to_owned(),
                    version: Some("0.1.0".to_owned()),
                    git: None,
                    path: Some("../zcast-extra-path".to_owned()),
                },
            ],
        };

        generate_shim_crate(&dir, &cfg).expect("generate shim");

        let manifest = fs::read_to_string(dir.join("Cargo.toml")).expect("read manifest");
        assert!(manifest.contains("name = \"zcast-custom\""));
        assert!(manifest.contains("version = \"0.4.0\""));
        assert!(manifest.contains("[[bin]]"));
        assert!(manifest.contains("name = \"zcast\""));
        assert!(manifest.contains("zcast-cli = { git ="));
        assert!(manifest.contains("zcast-kernel = { git ="));
        assert!(manifest.contains("zcast-interfaces = { git ="));
        assert!(
            manifest.contains(
                "zcast-extra-git = { git = \"https://example.com/zcast-extra-git.git\" }"
            )
        );
        assert!(manifest.contains(
            "zcast-extra-path = { version = \"0.1.0\", path = \"../zcast-extra-path\" }"
        ));
        assert!(manifest.contains("[profile.release]"));
        assert!(manifest.contains("#   zcast:storage/repository -> zcast-storage-sqlite"));

        let main_rs = fs::read_to_string(dir.join("src/main.rs")).expect("read main");
        assert!(main_rs.contains("fn main()"));
        assert!(main_rs.contains("zcast_cli::main_entry()"));
        assert!(main_rs.contains("\"zcast:storage/repository\", \"zcast-storage-sqlite\""));

        // Every generated manifest field must be valid TOML.
        let parsed: toml::Value = toml::from_str(&manifest).expect("manifest parses as TOML");
        assert!(parsed.get("dependencies").is_some());

        cleanup(&dir);
    }

    #[test]
    fn swaps_binary_atomically_and_rolls_back() {
        let dir = temp_dir("swap");
        let current = dir.join("zcast");
        let new = dir.join("zcast-new");
        fs::write(&current, b"old-binary").expect("write current");
        fs::write(&new, b"new-binary").expect("write new");

        let prev = atomic_swap_binary(&new, &current).expect("swap");
        assert_eq!(prev, dir.join("zcast.prev"));
        assert_eq!(fs::read(&current).expect("read current"), b"new-binary");
        assert_eq!(fs::read(&prev).expect("read prev"), b"old-binary");
        assert!(
            !dir.join("zcast.new").exists(),
            "staged file must be renamed"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&current)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        rollback_binary(&prev, &current).expect("rollback");
        assert_eq!(
            fs::read(&current).expect("read after rollback"),
            b"old-binary"
        );
        assert!(!prev.exists(), "rollback consumes the backup");

        cleanup(&dir);
    }

    #[test]
    fn swap_without_existing_binary_leaves_no_backup() {
        let dir = temp_dir("swap-fresh");
        let current = dir.join("bin").join("zcast");
        let new = dir.join("zcast-new");
        fs::write(&new, b"fresh-binary").expect("write new");

        let prev = atomic_swap_binary(&new, &current).expect("swap");
        assert_eq!(fs::read(&current).expect("read current"), b"fresh-binary");
        assert!(!prev.exists());

        cleanup(&dir);
    }

    #[test]
    fn dry_run_build_returns_expected_path_without_cargo() {
        let dir = temp_dir("dry-run");
        let cfg = ModuleLockConfig {
            core_version: "0.4.0".to_owned(),
            bindings: BTreeMap::new(),
            extra_crates: vec![],
            disabled: Vec::new(),
        };

        let path = build_custom_binary(&dir, &cfg, Some(2), true).expect("dry run");
        assert_eq!(path, expected_binary_path(&dir));
        assert!(path.ends_with("target/release/zcast"));
        assert!(dir.join("Cargo.toml").exists());
        assert!(dir.join("src/main.rs").exists());

        assert!(build_custom_binary(&dir, &cfg, Some(0), true).is_err());

        cleanup(&dir);
    }

    fn base_lock() -> ModuleLockConfig {
        ModuleLockConfig {
            core_version: "0.4.0".to_owned(),
            bindings: BTreeMap::new(),
            extra_crates: vec![],
            disabled: Vec::new(),
        }
    }

    #[test]
    fn deactivating_optional_module_marks_it_disabled() {
        let mut cfg = base_lock();
        let msg = cfg.deactivate_module("zcast-popover").expect("deactivate");
        assert!(msg.contains("zcast-popover"), "{msg}");
        assert!(cfg.is_disabled("zcast-popover"));
        assert_eq!(cfg.disabled, vec!["zcast-popover".to_owned()]);
    }

    #[test]
    fn reactivating_optional_module_clears_disabled() {
        let mut cfg = base_lock();
        cfg.deactivate_module("zcast-popover").expect("deactivate");
        let msg = cfg.activate_module("zcast-popover").expect("activate");
        assert!(msg.contains("zcast-popover"), "{msg}");
        assert!(!cfg.is_disabled("zcast-popover"));
        assert!(cfg.disabled.is_empty());
    }

    #[test]
    fn mandatory_modules_cannot_be_deactivated() {
        for id in [
            "zcast-storage-sqlite",
            "zcast-storage-mysql",
            "zcast-links-wikilink",
            "zcast-theme-davidhewitson",
        ] {
            let mut cfg = base_lock();
            let err = cfg.deactivate_module(id).expect_err("mandatory guard");
            assert!(
                err.contains("cannot deactivate mandatory module") && err.contains(id),
                "unexpected error: {err}"
            );
            assert!(err.contains("zcast module bind"), "{err}");
            assert!(!cfg.is_disabled(id));
        }
    }

    #[test]
    fn unknown_modules_are_rejected() {
        let mut cfg = base_lock();
        let err = cfg
            .deactivate_module("zcast-nope")
            .expect_err("unknown deactivate");
        assert!(err.contains("unknown module 'zcast-nope'"), "{err}");

        let err = cfg
            .activate_module("zcast-nope")
            .expect_err("unknown activate");
        assert!(err.contains("is not installed"), "{err}");
    }

    #[test]
    fn search_provider_cannot_be_deactivated_while_cmd_k_is_active() {
        let mut cfg = base_lock();
        let err = cfg
            .deactivate_module("zcast-search-fts")
            .expect_err("dependency guard");
        assert!(err.contains("zcast:search/provider@^1.0"), "{err}");
        assert!(err.contains("zcast-cmd-k"), "{err}");
        assert!(!cfg.is_disabled("zcast-search-fts"));

        // Deactivating the command bar first unblocks the search provider.
        cfg.deactivate_module("zcast-cmd-k").expect("cmd-k");
        cfg.deactivate_module("zcast-search-fts")
            .expect("search-fts after cmd-k");
        assert!(cfg.is_disabled("zcast-cmd-k"));
        assert!(cfg.is_disabled("zcast-search-fts"));
        assert_eq!(cfg.active_search_module(), None);

        // With no active search provider the command bar cannot come back.
        let err = cfg
            .activate_module("zcast-cmd-k")
            .expect_err("cmd-k needs search");
        assert!(err.contains("cannot activate 'zcast-cmd-k'"), "{err}");
        assert!(err.contains("zcast:search/provider@^1.0"), "{err}");
    }

    #[test]
    fn activating_onnx_binds_search_and_unblocks_fts_deactivation() {
        let mut cfg = base_lock();
        assert_eq!(cfg.active_search_module(), Some("zcast-search-fts"));

        cfg.activate_module("zcast-search-onnx")
            .expect("activate onnx");
        assert_eq!(
            cfg.bindings
                .get(zcast_interfaces::IFACE_SEARCH)
                .map(String::as_str),
            Some("zcast-search-onnx")
        );
        assert_eq!(cfg.active_search_module(), Some("zcast-search-onnx"));

        // cmd-k still registers because onnx keeps the slot fulfilled.
        cfg.deactivate_module("zcast-search-fts")
            .expect("fts with onnx active");
        assert!(cfg.is_disabled("zcast-search-fts"));
        assert_eq!(cfg.active_search_module(), Some("zcast-search-onnx"));
    }

    #[test]
    fn active_search_module_respects_bindings_and_disabled() {
        let mut cfg = base_lock();
        assert_eq!(cfg.active_search_module(), Some("zcast-search-fts"));

        cfg.bindings
            .insert(zcast_interfaces::IFACE_SEARCH.to_owned(), "none".to_owned());
        assert_eq!(cfg.active_search_module(), None);

        cfg.bindings.insert(
            zcast_interfaces::IFACE_SEARCH.to_owned(),
            "zcast-search-fts".to_owned(),
        );
        cfg.deactivate_module("zcast-cmd-k").expect("cmd-k");
        cfg.deactivate_module("zcast-search-fts").expect("fts");
        assert_eq!(cfg.active_search_module(), None);

        cfg.activate_module("zcast-search-onnx").expect("onnx");
        assert_eq!(cfg.active_search_module(), Some("zcast-search-onnx"));
    }

    #[test]
    fn switching_auth_provider_binds_auth_slot() {
        let mut cfg = base_lock();
        cfg.deactivate_module("zcast-auth-basic")
            .expect("basic off");
        assert!(cfg.is_disabled("zcast-auth-basic"));

        cfg.activate_module("zcast-auth-magic-link")
            .expect("magic-link on");
        assert!(!cfg.is_disabled("zcast-auth-magic-link"));
        assert_eq!(
            cfg.bindings
                .get(zcast_interfaces::IFACE_AUTH)
                .map(String::as_str),
            Some("zcast-auth-magic-link")
        );
    }

    #[test]
    fn deactivated_modules_are_tracked_once_and_sorted() {
        let mut cfg = base_lock();
        cfg.deactivate_module("zcast-popover").expect("popover");
        cfg.deactivate_module("zcast-io-markdown").expect("io");
        let again = cfg.deactivate_module("zcast-popover").expect("repeat");
        assert!(again.contains("already deactivated"), "{again}");
        assert_eq!(
            cfg.disabled,
            vec!["zcast-io-markdown".to_owned(), "zcast-popover".to_owned()]
        );
    }

    #[test]
    fn extra_crates_can_be_deactivated_and_reactivated() {
        let mut cfg = base_lock();
        cfg.extra_crates.push(ModuleCrateSpec {
            name: "zcast-extra-widget".to_owned(),
            version: Some("0.1.0".to_owned()),
            git: None,
            path: None,
        });

        cfg.deactivate_module("zcast-extra-widget")
            .expect("deactivate extra");
        assert!(cfg.is_disabled("zcast-extra-widget"));

        cfg.activate_module("zcast-extra-widget")
            .expect("reactivate extra");
        assert!(!cfg.is_disabled("zcast-extra-widget"));
    }

    #[test]
    fn shim_crate_omits_disabled_bundled_modules() {
        let dir = temp_dir("shim-disabled");
        let mut cfg = base_lock();
        cfg.deactivate_module("zcast-popover").expect("popover");
        cfg.deactivate_module("zcast-cmd-k").expect("cmd-k");

        generate_shim_crate(&dir, &cfg).expect("generate shim");

        let manifest = fs::read_to_string(dir.join("Cargo.toml")).expect("read manifest");
        assert!(manifest.contains("zcast-storage-sqlite = { git ="));
        assert!(!manifest.contains("zcast-popover = { git ="));
        assert!(!manifest.contains("zcast-cmd-k = { git ="));
        assert!(manifest.contains("# zcast-popover = deactivated"));
        toml::from_str::<toml::Value>(&manifest).expect("manifest parses as TOML");

        cleanup(&dir);
    }

    #[test]
    fn lock_config_roundtrips_disabled_modules() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(MODULE_LOCK_FILE);

        let mut cfg = base_lock();
        // Nothing disabled: the key is omitted entirely.
        cfg.save(&path).expect("save");
        let raw = fs::read_to_string(&path).expect("read");
        assert!(!raw.contains("disabled"), "{raw}");
        let loaded = ModuleLockConfig::load(&path).expect("load");
        assert!(loaded.disabled.is_empty());

        cfg.deactivate_module("zcast-popover").expect("popover");
        cfg.deactivate_module("zcast-cmd-k").expect("cmd-k");
        cfg.save(&path).expect("save disabled");

        let loaded = ModuleLockConfig::load(&path).expect("load disabled");
        assert_eq!(loaded.disabled, cfg.disabled);
        assert!(loaded.is_disabled("zcast-popover"));
        assert!(loaded.is_disabled("zcast-cmd-k"));
        assert!(!loaded.is_disabled("zcast-search-fts"));

        cleanup(&dir);
    }

    #[test]
    fn classifies_bundled_modules() {
        assert!(is_mandatory_module("zcast-storage-sqlite"));
        assert!(is_mandatory_module("zcast-storage-mysql"));
        assert!(!is_mandatory_module("zcast-popover"));
        assert!(is_optional_bundled_module("zcast-popover"));
        assert!(!is_optional_bundled_module("zcast-storage-sqlite"));
        assert!(is_bundled_module("zcast-storage-sqlite"));
        assert!(is_bundled_module("zcast-search-fts"));
        assert!(!is_bundled_module("zcast-extra-widget"));
    }
}
