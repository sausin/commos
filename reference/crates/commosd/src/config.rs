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

    /// Take a voicemail when an internal callee does not answer (Volume 7). When `true`
    /// (the default — voicemail is a core PBX feature), a call to a registered extension that
    /// rings unanswered, or to a known-but-offline extension, is answered by the platform,
    /// the caller's audio is captured as-is, stored as a voicemail on hangup, and a
    /// message-waiting indication (MWI) is pushed to the phone. External/PSTN destinations
    /// (no mailbox) still fall through to the echo path. Set `false` to restore the plain
    /// no-answer→echo behaviour.
    #[serde(default = "default_true")]
    pub voicemail_enabled: bool,

    /// How many times a called extension rings before an unanswered call is diverted to
    /// voicemail (or the echo fallback when voicemail is off). Default `5` (~30 s — a standard
    /// PBX ring). One "ring" is the ~6 s ring cadence, so the effective no-answer timeout is
    /// `no_answer_rings × 6 s`. Raise it if callers are being sent to voicemail before people
    /// can reach the phone; lower it for a snappier fallback.
    #[serde(default = "default_no_answer_rings")]
    pub no_answer_rings: u32,

    /// IP address advertised to callers in SDP for RTP media. Default `127.0.0.1` (loopback
    /// echo test); set to the server's LAN/public address for real phones.
    #[serde(default = "default_media_ip")]
    pub media_ip: IpAddr,

    /// NTP time server written into provisioned phone configs, so handsets on an isolated
    /// network (no Internet) sync their clock from an internal source. `None` (the default)
    /// points phones at the CommOS host itself (`media_ip`) — run an NTP service there (e.g.
    /// chrony with `allow`). Set it to a dedicated internal NTP appliance's address/hostname
    /// to use that instead. Only affects what phones are told at provisioning time.
    #[serde(default)]
    pub ntp_server: Option<String>,

    /// Timezone written into provisioned phone configs so handsets show the correct *local*
    /// time (NTP only supplies UTC). A POSIX TZ string, e.g. `PST8PDT`, `GMT-5`, `CET-1CEST`.
    /// `None` (the default) emits no timezone directive — phones keep their own/UTC setting.
    /// On an isolated network this is usually the real fix for a wrong clock display.
    #[serde(default)]
    pub timezone: Option<String>,

    /// Admin (web-UI) password written into provisioned phone configs — a **reference**, never
    /// inline (CMOS-14-DEP-083). When set, phones are locked down so a guest cannot open the
    /// handset's web UI with the factory `admin`/`admin` and change SIP, network, or dial
    /// settings ("funny business"); it is re-asserted on every re-provision, so a checkout
    /// re-provision restores the locked state. Applied to Yealink (`static.security.user_password
    /// = admin:<pw>`) and Grandstream (`P2`); the generic fallback has no portable key so it is
    /// skipped there. `None` (the default) leaves the phone's existing web password untouched.
    #[serde(default)]
    pub phone_admin_password: Option<SecretRef>,

    /// Encrypt the RTP media path with SRTP (RFC 3711) when a caller offers it — the secure
    /// `RTP/SAVP` profile keyed by an SDES `a=crypto` line (RFC 4568, `AES_CM_128_HMAC_SHA1_80`).
    /// Default `true`: worth doing even on a trusted LAN, since it stops a passive sniffer from
    /// capturing call audio. SRTP is only ever *offered* by the phone, so this default never
    /// breaks a plain-RTP caller — a plain `RTP/AVP` INVITE is still answered in the clear. It
    /// applies to the endpoint media paths CommOS terminates (echo test and voicemail) and, per
    /// leg, across the two-leg bridge/trunk relay. Because SDES carries the key in the SDP, pair
    /// this with SIP-over-TLS below to protect the key in transit.
    #[serde(default = "default_true")]
    pub srtp: bool,

    /// Attempt SRTP toward an **outbound carrier trunk** as well (default `false`). SDP cannot
    /// downgrade the profile in an answer, so offering `RTP/SAVP` to a carrier that only speaks
    /// plain RTP makes it reject the call. Carrier SRTP support is inconsistent, so by default the
    /// carrier (trunk) leg is left **plaintext** — the caller's access leg is still encrypted when
    /// they offered SRTP, and the outbound call always connects. Set `true` only when the carrier
    /// is known to support `AES_CM_128_HMAC_SHA1_80` SDES.
    #[serde(default)]
    pub trunk_srtp: bool,

    /// TLS address the SIP signalling ingress binds for **SIPS** (SIP over TLS, RFC 3261). `null`
    /// (the default) disables it — TLS is opt-in and requires a build with `--features tls`. The
    /// IANA SIPS port is `5061`. Encrypting the signalling channel protects the SDES SRTP keys
    /// (and every header — who calls whom) from a passive network observer.
    #[serde(default)]
    pub sips_listen: Option<SocketAddr>,

    /// PEM certificate chain served on the SIPS listener. A public certificate, so a plain path
    /// (not a secret reference). Required when `sips_listen` is set.
    #[serde(default)]
    pub sip_tls_cert: Option<String>,

    /// PEM private key for the SIPS certificate — a **reference**, never inline
    /// (CMOS-14-DEP-083); e.g. `{ ref_uri: "file:///etc/commos/tls/sip-key.pem" }`. Required when
    /// `sips_listen` is set.
    #[serde(default)]
    pub sip_tls_key: Option<SecretRef>,

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

    /// Directory holding audio prompt files (voicemail greeting, retrieval menu, etc.) as raw
    /// G.711 μ-law `.ulaw` files, organised by language (`<sounds_dir>/en/<name>.ulaw`). `None`
    /// (the default) resolves to `{data_dir}/sounds` — where the installer downloads the public
    /// FreePBX sound pack. A missing file falls back to a synthesized tone, so the system still
    /// works with no sounds installed; set this only to point at a shared/custom prompt library.
    #[serde(default)]
    pub sounds_dir: Option<String>,

    /// Path to a plain-text file whose contents become the **display name** shown on a phone when
    /// CommOS places the call (the identity the called handset renders — otherwise the bare
    /// "commos"). One non-empty line → that text on every call; multiple lines → one picked per
    /// call (varied by call id), so you can rotate through friendly/rotating messages. `None` (the
    /// default) resolves to `{data_dir}/display_name.txt`; if that file is absent or empty, phones
    /// see the default "commos". Re-read per call, so edits apply without a restart.
    #[serde(default)]
    pub display_name_file: Option<String>,

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

    /// Directory holding audio prompt files. Explicit `sounds_dir` wins; otherwise it is
    /// `{data_dir}/sounds` — the same `data_dir`-relative convention as the DB, object store,
    /// and JWT secret, so an installed binary always resolves it against the state it owns
    /// (never the current working directory).
    pub fn sounds_dir(&self) -> String {
        match &self.sounds_dir {
            Some(dir) if !dir.trim().is_empty() => dir.trim_end_matches('/').to_string(),
            _ => format!("{}/sounds", self.data_dir.trim_end_matches('/')),
        }
    }

    /// Path to the phone display-name file. Explicit `display_name_file` wins; otherwise it is
    /// `{data_dir}/display_name.txt` — the same `data_dir`-relative convention as everything else.
    pub fn display_name_file(&self) -> String {
        match &self.display_name_file {
            Some(p) if !p.trim().is_empty() => p.clone(),
            _ => format!("{}/display_name.txt", self.data_dir.trim_end_matches('/')),
        }
    }

    /// Directory of optional operator-supplied provisioning overlays, `{data_dir}/provision`
    /// (same `data_dir`-relative convention as everything else). A `<vendor>.cfg` file there
    /// (e.g. `grandstream.cfg`, `yealink.cfg`) holds vendor-native `KEY = VALUE` lines that are
    /// appended to that vendor's generated phone config — the import hook for hardware-specific
    /// settings CommOS does not model (LCD backlight, screensaver, …). Absent → nothing appended.
    pub fn provision_dir(&self) -> String {
        format!("{}/provision", self.data_dir.trim_end_matches('/'))
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
            voicemail_enabled: true,
            no_answer_rings: default_no_answer_rings(),
            media_ip: default_media_ip(),
            ntp_server: None,
            timezone: None,
            phone_admin_password: None,
            srtp: true,
            trunk_srtp: false,
            sips_listen: None,
            sip_tls_cert: None,
            sip_tls_key: None,
            jwt_secret: None,
            dev_tokens: default_true(),
            database_url: None,
            data_dir: default_data_dir(),
            sounds_dir: None,
            display_name_file: None,
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

fn default_no_answer_rings() -> u32 {
    5
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
