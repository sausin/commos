//! Config-as-code — the declarative `pbx.yaml` (Volume 14 §10, CMOS-14-DEP-080..084).
//!
//! The file captures *intent* and is the desired state, not an imperative script. It is
//! Git-reviewable (deterministic, diff-friendly) and MUST NOT embed secrets
//! (CMOS-14-DEP-083): secrets are *referenced*, resolved from an external manager.

use std::net::SocketAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Root of `pbx.yaml`. Only the fields the current slice reconciles are modelled; the
/// full people/phones/numbers/flows intent is layered on as those subsystems land.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Config-as-code identity, so a file is self-describing and versionable.
    #[serde(default = "default_api_version")]
    pub api_version: String,

    /// Address the API Gateway binds. Default suits a single-binary box (a Raspberry Pi
    /// on the LAN, a server, a container with host networking — CMOS-14-DEP-003).
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,

    /// PostgreSQL DSN — the system of record (CMOS-14-DEP-020). A **reference**, never an
    /// inline credential (CMOS-14-DEP-083). `None` means run with the in-process store so
    /// the single binary boots with zero external dependencies (CMOS-14-DEP-021).
    #[serde(default)]
    pub database_url: Option<SecretRef>,

    #[serde(default)]
    pub log: LogConfig,
}

/// A reference to a secret held in an external manager (Vault / KMS / 1Password, Volume 9).
/// The value is a URI, never the secret itself — import rejects an inline secret.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    /// e.g. `vault://commos/db#dsn`, `env://DATABASE_URL`, `file:///run/secrets/db`.
    pub ref_uri: String,
}

impl SecretRef {
    /// Resolve the reference to its secret value from an external source. The reference
    /// lives in Git; the secret never does (CMOS-14-DEP-083). This slice supports the two
    /// simplest backends — an environment variable and a mounted file (e.g. a Kubernetes/
    /// systemd secret); Vault/KMS bindings slot in behind the same scheme dispatch.
    pub fn resolve(&self) -> Result<String, ConfigError> {
        let uri = &self.ref_uri;
        if let Some(var) = uri.strip_prefix("env://") {
            std::env::var(var).map_err(|_| {
                ConfigError::UnresolvedSecret(format!("environment variable {var} is not set"))
            })
        } else if let Some(path) = uri.strip_prefix("file://") {
            std::fs::read_to_string(path)
                .map(|s| s.trim().to_string())
                .map_err(|e| ConfigError::UnresolvedSecret(format!("{uri}: {e}")))
        } else {
            Err(ConfigError::UnresolvedSecret(format!(
                "unsupported secret scheme in '{uri}' (expected env:// or file://)"
            )))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[serde(default = "default_log_format")]
    pub format: LogFormat,
    #[serde(default = "default_log_level")]
    pub level: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            format: default_log_format(),
            level: default_log_level(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api_version: default_api_version(),
            listen: default_listen(),
            database_url: None,
            log: LogConfig::default(),
        }
    }
}

fn default_api_version() -> String {
    "commos.dev/v0.4".to_string()
}
fn default_listen() -> SocketAddr {
    "0.0.0.0:8080".parse().expect("valid default addr")
}
fn default_log_format() -> LogFormat {
    LogFormat::Json
}
fn default_log_level() -> String {
    "info".to_string()
}

/// Error loading or validating config. Distinct from IO so `main` can map to the
/// systemd exit-code contract (CMOS-14-DEP-002).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config: {0}")]
    Parse(String),
    #[error("config rejected — secrets must be referenced, never inline (CMOS-14-DEP-083): {0}")]
    InlineSecret(String),
    #[error("could not resolve a referenced secret: {0}")]
    UnresolvedSecret(String),
}

impl Config {
    /// Load and validate `pbx.yaml`. A missing file yields defaults (single-binary,
    /// zero-dependency boot), which keeps the primary artifact runnable out of the box.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        if !path.exists() {
            tracing::warn!(path = %path.display(), "no pbx.yaml found; booting with defaults");
            return Ok(Config::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_yaml(&raw)
    }

    /// Parse + validate from a YAML string (also the unit-test entry point).
    pub fn from_yaml(raw: &str) -> Result<Config, ConfigError> {
        reject_inline_secrets(raw)?;
        let cfg: Config =
            serde_yaml::from_str(raw).map_err(|e| ConfigError::Parse(e.to_string()))?;
        Ok(cfg)
    }
}

/// Enforce CMOS-14-DEP-083: a `database_url`/secret expressed as a bare string that
/// looks like a live DSN (embeds credentials) is rejected; only a `ref_uri` reference is
/// allowed. This is a coarse guard — the full policy lives in Volume 9 — but it holds the
/// invariant that a committed `pbx.yaml` never carries a secret.
fn reject_inline_secrets(raw: &str) -> Result<(), ConfigError> {
    for (n, line) in raw.lines().enumerate() {
        let l = line.trim();
        // A DSN with an embedded password, e.g. `database_url: postgres://u:p@host/db`.
        if l.starts_with("database_url:") && l.contains("://") && l.contains('@') {
            return Err(ConfigError::InlineSecret(format!(
                "line {}: inline DSN with credentials",
                n + 1
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yaml_is_defaults() {
        let cfg = Config::from_yaml("{}").unwrap();
        assert_eq!(cfg.listen.port(), 8080);
        assert!(cfg.database_url.is_none());
        assert_eq!(cfg.log.format, LogFormat::Json);
    }

    #[test]
    fn secret_reference_is_accepted() {
        let cfg = Config::from_yaml(
            "listen: \"127.0.0.1:9090\"\ndatabase_url:\n  ref_uri: \"vault://commos/db#dsn\"\n",
        )
        .unwrap();
        assert_eq!(cfg.listen.port(), 9090);
        assert_eq!(cfg.database_url.unwrap().ref_uri, "vault://commos/db#dsn");
    }

    #[test]
    fn inline_dsn_is_rejected() {
        let err = Config::from_yaml("database_url: postgres://user:pw@db:5432/commos\n")
            .unwrap_err();
        assert!(matches!(err, ConfigError::InlineSecret(_)));
    }
}
