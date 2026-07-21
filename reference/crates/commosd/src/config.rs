//! Config-as-code — the declarative `pbx.yaml` (Volume 14 §10, CMOS-14-DEP-080..084).
//!
//! The file captures *intent* and is the desired state, not an imperative script. It is
//! Git-reviewable (deterministic, diff-friendly) and MUST NOT embed secrets
//! (CMOS-14-DEP-083): secrets are *referenced*, resolved from an external manager.

use std::net::{IpAddr, SocketAddr};
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

    /// UDP address the SIP signalling ingress binds (Volume 7). Default `0.0.0.0:5060`
    /// (the IANA SIP port); `null` disables the SIP plane.
    #[serde(default = "default_sip_listen")]
    pub sip_listen: Option<SocketAddr>,

    /// Require SIP digest authentication on REGISTER (Volume 9). Default `false` so a LAN test
    /// bed works with zero setup; set `true` to demand per-device credentials (generated during
    /// provisioning) — do this before exposing SIP beyond a trusted network.
    #[serde(default)]
    pub require_sip_auth: bool,

    /// SIP digest realm presented in the auth challenge. Default `commos`.
    #[serde(default = "default_sip_realm")]
    pub sip_realm: String,

    /// Record calls (Volume 7). When `true`, the caller's audio is captured as-is (no
    /// transcoding) and stored as an `audio/basic` recording on hangup. Default `false`.
    #[serde(default)]
    pub record_calls: bool,

    /// IP address advertised to callers in SDP for RTP media. Default `127.0.0.1` (loopback
    /// echo test); set to the server's LAN/public address for real phones.
    #[serde(default = "default_media_ip")]
    pub media_ip: IpAddr,

    /// HS256 JWT signing secret — a **reference**, never inline (CMOS-14-DEP-083). When set,
    /// `/v1` bearer tokens are verified as JWTs (Volume 9). When unset (default), only the
    /// `tenant:<uuid>` dev token is accepted (see `dev_tokens`).
    #[serde(default)]
    pub jwt_secret: Option<SecretRef>,

    /// Accept the `tenant:<uuidv7>` development bearer token. Default `true` so local dev and
    /// the dashboard work with zero setup; set `false` in production once `jwt_secret` is set.
    #[serde(default = "default_true")]
    pub dev_tokens: bool,

    /// Database DSN — the system of record. A **reference**, never an inline credential
    /// (CMOS-14-DEP-083). `None` (the default) means use the embedded SQLite store at
    /// `{data_dir}/commos.db` — durable with zero external dependency (CMOS-14-DEP-021,
    /// ADR-0012). Set it for PostgreSQL (`postgres://…`) in a multi-node / HA deployment,
    /// or to `memory://` for an ephemeral in-process store (tests).
    #[serde(default)]
    pub database_url: Option<SecretRef>,

    /// Directory for the embedded SQLite database (and other local state). Default `.`.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,

    /// Object-storage backend. `None` (default) stores blobs on the local filesystem under
    /// `{data_dir}/objects`. Set to `s3://<bucket>` to use S3-compatible storage (requires a
    /// build with `--features s3`); credentials come from the environment
    /// (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`), never from this file (CMOS-14-DEP-083).
    #[serde(default)]
    pub object_storage: Option<String>,
    /// S3 endpoint for S3-compatible services (MinIO/R2/B2/Wasabi/Ceph). Omit for AWS S3.
    #[serde(default)]
    pub s3_endpoint: Option<String>,
    /// S3 region. Default `us-east-1` (many S3-compatible servers ignore it).
    #[serde(default = "default_s3_region")]
    pub s3_region: String,
    /// Use path-style addressing (`endpoint/bucket/key`). Default `true` — the safe default for
    /// most S3-compatible servers; set `false` for AWS virtual-hosted style.
    #[serde(default = "default_true")]
    pub s3_path_style: bool,

    /// Home country code (digits, no `+`) used to classify a dialled number as national vs
    /// international for the origination policy. Default `"1"`.
    #[serde(default = "default_country_code")]
    pub default_country_code: String,

    /// Allow international calls (Volume 9 toll-fraud guardrail). Default `false` — outbound
    /// international is **blocked** until an operator opts in, the safe default for a PBX.
    #[serde(default)]
    pub allow_international: bool,

    /// Cap on concurrent in-progress calls per tenant (velocity guardrail). `None` (default)
    /// means no cap.
    #[serde(default)]
    pub max_concurrent_calls: Option<u32>,

    /// Admin password — a **reference**, never inline (CMOS-14-DEP-083). When set, the
    /// privileged setup routes (onboarding apply, config import) require an admin session
    /// obtained via `POST /admin/login`. When unset (the default), admin auth is in dev mode:
    /// any valid tenant/dev bearer acts as admin, keeping zero-config local setup working.
    #[serde(default)]
    pub admin_password: Option<SecretRef>,

    #[serde(default)]
    pub log: LogConfig,
}

impl Config {
    /// Path to the embedded SQLite database used when no `database_url` is configured.
    pub fn default_sqlite_path(&self) -> String {
        format!("{}/commos.db", self.data_dir.trim_end_matches('/'))
    }
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
            sip_listen: default_sip_listen(),
            require_sip_auth: false,
            sip_realm: default_sip_realm(),
            record_calls: false,
            media_ip: default_media_ip(),
            jwt_secret: None,
            dev_tokens: default_true(),
            database_url: None,
            data_dir: default_data_dir(),
            object_storage: None,
            s3_endpoint: None,
            s3_region: default_s3_region(),
            s3_path_style: true,
            default_country_code: default_country_code(),
            allow_international: false,
            max_concurrent_calls: None,
            admin_password: None,
            log: LogConfig::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_media_ip() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}

fn default_data_dir() -> String {
    ".".to_string()
}

fn default_country_code() -> String {
    "1".to_string()
}

fn default_s3_region() -> String {
    "us-east-1".to_string()
}

fn default_sip_listen() -> Option<SocketAddr> {
    Some("0.0.0.0:5060".parse().expect("valid default SIP addr"))
}

fn default_sip_realm() -> String {
    "commos".to_string()
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
