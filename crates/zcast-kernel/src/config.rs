use crate::error::KernelError;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use zcast_interfaces::ThemeColorMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KernelConfig {
    pub site_title: String,
    pub bind_addr: String,
    pub database_url: String,
    pub color_mode: ThemeColorMode,
    pub pinned_home_slug: String,
    pub custom_css_path: Option<PathBuf>,
    pub allow_unauthenticated_public: bool,
    /// Optional auth provider (`none` or `basic`).
    pub auth: Option<String>,
    /// Admin username for `basic` auth (default: `"admin"`).
    pub admin_user: String,
    /// Optional admin password for `basic` auth (set via `ZCAST_ADMIN_PASSWORD` or CLI).
    pub admin_password: Option<String>,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            site_title: "Zcast".to_owned(),
            bind_addr: "127.0.0.1:3000".to_owned(),
            database_url: "sqlite://zcast.db?mode=rwc".to_owned(),
            color_mode: ThemeColorMode::Auto,
            pinned_home_slug: "index".to_owned(),
            custom_css_path: None,
            allow_unauthenticated_public: false,
            auth: None,
            admin_user: "admin".to_owned(),
            admin_password: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct TomlRoot {
    #[serde(default)]
    zcast: Option<KernelConfig>,
}

impl KernelConfig {
    /// Load from an optional `zcast.toml` file, then apply `ZCAST_*` environment overrides (DEC-15).
    pub fn load(path: Option<&Path>) -> Result<Self, KernelError> {
        let mut cfg = if let Some(p) = path {
            if p.exists() {
                let raw = std::fs::read_to_string(p)?;
                let parsed: TomlRoot =
                    toml::from_str(&raw).map_err(|e| KernelError::Config(e.to_string()))?;
                parsed.zcast.unwrap_or_default()
            } else {
                Self::default()
            }
        } else {
            Self::default()
        };

        if let Ok(v) = std::env::var("ZCAST_SITE_TITLE")
            && !v.trim().is_empty()
        {
            cfg.site_title = v;
        }
        if let Ok(v) = std::env::var("ZCAST_BIND_ADDR")
            && !v.trim().is_empty()
        {
            cfg.bind_addr = v;
        }
        if let Ok(v) = std::env::var("ZCAST_DATABASE_URL")
            && !v.trim().is_empty()
        {
            cfg.database_url = v;
        }
        if let Ok(v) = std::env::var("ZCAST_THEME_MODE")
            && !v.trim().is_empty()
        {
            cfg.color_mode = ThemeColorMode::parse(&v);
        }
        if let Ok(v) = std::env::var("ZCAST_CUSTOM_CSS")
            && !v.trim().is_empty()
        {
            cfg.custom_css_path = Some(PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("ZCAST_AUTH")
            && !v.trim().is_empty()
        {
            cfg.auth = Some(v.trim().to_ascii_lowercase());
        }
        if let Ok(v) = std::env::var("ZCAST_ADMIN_USER")
            && !v.trim().is_empty()
        {
            cfg.admin_user = v.trim().to_owned();
        }
        if let Ok(v) = std::env::var("ZCAST_ADMIN_PASSWORD")
            && !v.is_empty()
        {
            cfg.admin_password = Some(v);
        }

        Ok(cfg)
    }

    /// Enforce DEC-10 safety guard: refuse binding to non-loopback interfaces when no `auth` module is bound.
    pub fn validate_bind_safety(&self, has_auth_module: bool) -> Result<SocketAddr, KernelError> {
        let addr: SocketAddr = self
            .bind_addr
            .parse()
            .map_err(|e| KernelError::Config(format!("invalid bind_addr '{}': {e}", self.bind_addr)))?;

        if !has_auth_module && !addr.ip().is_loopback() && !self.allow_unauthenticated_public {
            return Err(KernelError::UnsafeUnauthenticatedBind(
                self.bind_addr.clone(),
            ));
        }
        Ok(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_public_bind_without_auth() {
        let cfg = KernelConfig {
            bind_addr: "0.0.0.0:3000".into(),
            allow_unauthenticated_public: false,
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate_bind_safety(false),
            Err(KernelError::UnsafeUnauthenticatedBind(_))
        ));
        assert!(cfg.validate_bind_safety(true).is_ok());
    }

    #[test]
    fn allows_loopback_bind_without_auth() {
        let cfg = KernelConfig {
            bind_addr: "127.0.0.1:3000".into(),
            ..Default::default()
        };
        assert!(cfg.validate_bind_safety(false).is_ok());
    }
}
