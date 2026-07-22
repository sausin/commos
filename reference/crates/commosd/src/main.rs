//! `commosd` — the CommOS single self-contained binary (CMOS-14-DEP-001/010).
//!
//! One process runs the control plane and (a loopback of) the media plane, with the
//! transactional outbox relaying events to an in-process Event Bus. With no `pbx.yaml` it
//! boots on the **embedded SQLite** store — durable with zero external dependency
//! (ADR-0012) — so the artifact runs anywhere out of the box; PostgreSQL is the opt-in
//! multi-node / HA backend (CMOS-14-DEP-020).
//!
//! Operable under systemd (CMOS-14-DEP-002): clean start/stop, graceful drain on SIGTERM,
//! and a defined exit-code contract (see [`exit`]).

mod api;
mod bus;
mod config;
mod control;
mod introspect;
mod media;
mod metrics;
mod net;
mod objectstore;
mod relay;
mod sip;
mod state;
mod store;
mod telemetry;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;

use crate::bus::EventBus;
use crate::config::{Config, ConfigError};
use crate::control::routing::Routing;
use crate::introspect::RecentEvents;
use crate::media::LoopbackMedia;
use crate::relay::RelaySignal;
use crate::state::AppState;
use crate::store::MemStore;

/// Tenant the SIP ingress attributes registrations to, until SIP-domain→tenant mapping
/// lands (Volume 9). Matches the dev bearer token used elsewhere.
const SIP_DEFAULT_TENANT: &str = "01920000-0000-7000-8000-000000000001";

/// Exit-code contract (CMOS-14-DEP-002), following the BSD `sysexits.h` convention so
/// systemd and operators can distinguish failure classes.
mod exit {
    pub const OK: i32 = 0;
    /// EX_USAGE — bad invocation.
    pub const USAGE: i32 = 64;
    /// EX_CONFIG — configuration error.
    pub const CONFIG: i32 = 78;
    /// EX_UNAVAILABLE — a required service (e.g. the listener) could not start.
    pub const UNAVAILABLE: i32 = 69;
}

fn main() {
    // Parse args before the async runtime so `--help`/config errors exit cleanly.
    let config_path = match parse_args() {
        Ok(p) => p,
        Err(code) => std::process::exit(code),
    };

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            // Logging may not be up yet; write plainly to stderr.
            eprintln!("commosd: {e}");
            // Every config failure class maps to the same exit-code contract slot.
            let code = match e {
                ConfigError::InlineSecret(_)
                | ConfigError::Parse(_)
                | ConfigError::Io { .. }
                | ConfigError::UnresolvedSecret(_) => exit::CONFIG,
            };
            std::process::exit(code);
        }
    };

    telemetry::init(&cfg.log);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let code = runtime.block_on(run(cfg));
    std::process::exit(code);
}

/// Resolve the config path to use when `--config` is not given.
///
/// Historically the binary looked only at `./pbx.yaml`, but the installer writes the config to
/// the data dir (`/var/lib/commos/pbx.yaml` as root). So a bare `commosd` on an installed box
/// missed its own config and silently booted on defaults. Search the conventional locations, in
/// order, and use the first that exists:
///
/// 1. `$COMMOS_CONFIG` — explicit override (wins if set, even when the file is missing, so a
///    typo surfaces as a clear "cannot read config" error rather than a silent defaults boot),
/// 2. `./pbx.yaml` — the working-directory config (dev / `--data-dir ./data`),
/// 3. `/etc/commos/pbx.yaml` — the FHS config location,
/// 4. `/var/lib/commos/pbx.yaml` — where the installer writes it by default.
///
/// Falls back to `./pbx.yaml` (which `Config::load` reports as "booting with defaults") when none
/// exists, preserving the zero-config boot.
fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("COMMOS_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    const CANDIDATES: [&str; 3] =
        ["pbx.yaml", "/etc/commos/pbx.yaml", "/var/lib/commos/pbx.yaml"];
    for candidate in CANDIDATES {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("pbx.yaml")
}

fn parse_args() -> Result<PathBuf, i32> {
    let mut path = default_config_path();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                path = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        eprintln!("commosd: --config requires a path");
                        exit::USAGE
                    })?;
            }
            "-h" | "--help" => {
                println!(
                    "commosd {} — CommOS single-binary\n\n\
                     USAGE:\n  commosd [--config <pbx.yaml>]\n\n\
                     ENV:\n  RUST_LOG   override log level\n",
                    env!("CARGO_PKG_VERSION")
                );
                return Err(exit::OK);
            }
            "--version" => {
                println!("commosd {}", env!("CARGO_PKG_VERSION"));
                return Err(exit::OK);
            }
            other => {
                eprintln!("commosd: unknown argument '{other}'");
                return Err(exit::USAGE);
            }
        }
    }
    Ok(path)
}

async fn run(cfg: Config) -> i32 {
    // --- wire the subsystems ---------------------------------------------------------
    let recent = RecentEvents::new();
    let bus = EventBus::new(recent.clone());

    // Select the system-of-record binding. Callers below are identical whichever binding
    // is chosen (CMOS-14-DEP-042).
    let store: Arc<dyn store::Store> = match select_store(&cfg).await {
        Ok(s) => s,
        Err(code) => return code,
    };

    let (fact_tx, mut fact_rx) = tokio::sync::mpsc::unbounded_channel::<media::MediaFact>();
    let media = Arc::new(LoopbackMedia::new(fact_tx));
    let signal = RelaySignal::new();
    let policy = control::policy::PolicyLimits {
        allow_international: cfg.allow_international,
        max_concurrent_calls: cfg.max_concurrent_calls,
    };
    if !policy.allow_international {
        tracing::info!("origination policy: international calling BLOCKED (set allow_international to permit)");
    }
    let routing = Routing::new(
        store.clone(),
        media,
        signal.clone(),
        policy,
        cfg.default_country_code.clone(),
    );
    let messaging = control::messaging::MessagingService::new(store.clone(), signal.clone());
    let realtime = control::realtime::RealtimeService::new(store.clone(), signal.clone());
    let queues = control::queue::QueueService::new(store.clone(), signal.clone());
    // Routing programs (Volume 2/7): versioned CallFlows with publish/rollback, and IVR nodes.
    let call_flows = control::callflow::CallFlowService::new(store.clone(), signal.clone());
    let ivrs = control::ivr::IvrService::new(store.clone(), signal.clone());
    // PSTN / SIP trunking (Volume 7): carriers, gateways, trunks (outbound), inbound DIDs.
    let trunking = control::trunking::TrunkingService::new(store.clone(), signal.clone());
    let provisioning = control::provisioning::Provisioning::new(store.clone(), signal.clone());
    let webhooks = control::webhooks::WebhookService::new(store.clone(), signal.clone());
    // Object storage: local filesystem by default; S3-compatible when configured + built with
    // the `s3` feature. Recordings, voicemail, exports, and diagnostics all sit on it.
    let blob_store = match select_object_store(&cfg) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let objects = control::objects::ObjectService::new(blob_store, store.clone(), signal.clone());
    // Call recording (Volume 7): captured RTP audio is stored as-is on the object store and
    // indexed by a Recording. Off by default; enabled per-deployment via `record_calls`.
    let recordings =
        control::recordings::RecordingService::new(objects.clone(), store.clone(), signal.clone());
    // Voicemail (Volume 7): a caller's audio captured when a callee does not answer, stored
    // as-is on the object store and indexed by a Voicemail. Reuses the recording capture path.
    let voicemails =
        control::voicemail::VoicemailService::new(objects.clone(), store.clone(), signal.clone());
    let metrics = metrics::Metrics::new();
    let agents = control::agents::AgentRegistry::new(store.clone(), signal.clone());
    let registrations = control::registrations::RegistrationRegistry::new();

    // Bearer-auth config: resolve the JWT secret (if any); dev tokens on by default.
    //
    // Secure-by-default with zero operator burden: when no `jwt_secret` is configured we
    // auto-generate a strong one and persist it under the data dir, so real signed-JWT auth is
    // always available for remote callers out of the box. The unsigned `tenant:<uuid>` dev
    // token still works, but only from a trusted (loopback/LAN) peer — see `api::peer`.
    let jwt_secret = match &cfg.jwt_secret {
        Some(secret) => match secret.resolve() {
            Ok(s) => Some(s.into_bytes()),
            Err(e) => {
                tracing::error!("{e}");
                return exit::CONFIG;
            }
        },
        None => match ensure_jwt_secret(&cfg.data_dir) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!("cannot establish a JWT signing secret: {e}");
                return exit::CONFIG;
            }
        },
    };
    let auth = api::auth::AuthConfig { jwt_secret, dev_tokens: cfg.dev_tokens };
    if auth.jwt_secret.is_some() {
        tracing::info!(dev_tokens = cfg.dev_tokens, "bearer auth: HS256 JWT verification enabled");
    }

    // Admin auth: resolve the admin password (if referenced). When set, the privileged setup
    // routes require an admin session; when unset, admin auth stays in dev mode (any tenant
    // bearer acts as admin) so zero-config local setup keeps working (CMOS-14-DEP-083).
    let admin_password = match &cfg.admin_password {
        Some(secret) => match secret.resolve() {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::error!("{e}");
                return exit::CONFIG;
            }
        },
        None => None,
    };
    let admin = api::admin::AdminAuth::new(admin_password);
    if admin.is_dev_mode() {
        tracing::warn!("admin auth: DEV MODE (no admin_password set) — any tenant bearer acts as admin");
    } else {
        tracing::info!("admin auth: enabled — privileged setup requires POST /admin/login");
    }

    // SIP signalling ingress (Volume 7): a real softphone can REGISTER, and an INVITE creates
    // an inbound Call, reports ring/answer as media facts, sets up an RTP echo path, and is
    // answered with SDP. The ingress maps to a single tenant for now (Volume 9).
    if cfg.sip_listen.is_some() || cfg.sips_listen.is_some() {
        let default_tenant = commos_core::common::Uuid::parse(SIP_DEFAULT_TENANT)
            .expect("valid default SIP tenant");
        let server = std::sync::Arc::new(sip::SipServer::new(
            registrations.clone(),
            routing.clone(),
            cfg.media_ip,
            default_tenant,
            store.clone(),
            cfg.require_sip_auth,
            cfg.sip_realm.clone(),
            cfg.record_calls,
            recordings.clone(),
            cfg.voicemail_enabled,
            cfg.no_answer_rings,
            voicemails.clone(),
            ivrs.clone(),
            objects.clone(),
            cfg.default_country_code.clone(),
            cfg.srtp,
            cfg.trunk_srtp,
        ));
        if cfg.require_sip_auth {
            tracing::info!(realm = %cfg.sip_realm, "SIP digest auth: REQUIRED");
        } else {
            tracing::warn!("SIP digest auth: DISABLED (REGISTER accepted unauthenticated) — enable require_sip_auth before exposing SIP");
        }
        if cfg.record_calls {
            tracing::info!("call recording: ENABLED (caller audio stored as-is on hangup)");
        }
        if cfg.voicemail_enabled {
            tracing::info!(
                no_answer_rings = cfg.no_answer_rings,
                "voicemail: ENABLED (record-on-no-answer for internal extensions; MWI via SIP NOTIFY)"
            );
        } else {
            tracing::info!("voicemail: DISABLED (no-answer falls back to the echo path)");
        }
        if cfg.srtp {
            tracing::info!("SRTP: ENABLED (encrypt echo/voicemail media when the caller offers RTP/SAVP + SDES)");
        } else {
            tracing::info!("SRTP: DISABLED (media answered in the clear even when offered)");
        }

        // UDP ingress.
        if let Some(sip_addr) = cfg.sip_listen {
            // The #1 misconfiguration that silently breaks audio: SIP is reachable on the LAN but
            // the SDP advertises a loopback RTP address, so calls connect and no one can hear
            // anything. Warn loudly with the address to use (detected LAN IP if we can find one).
            if cfg.media_ip.is_loopback() && !sip_addr.ip().is_loopback() {
                let suggestion = control::onboarding::primary_host_ip()
                    .filter(|ip| !ip.is_loopback())
                    .map(|ip| format!(" — set `media_ip: \"{ip}\"` in pbx.yaml"))
                    .unwrap_or_else(|| " — set `media_ip` to this host's LAN IP in pbx.yaml".to_string());
                tracing::warn!(
                    media_ip = %cfg.media_ip, sip = %sip_addr,
                    "media_ip is loopback but SIP listens on {sip_addr}: real phones will register \
                     and connect, but NO AUDIO will flow{suggestion} (or run scripts/install.sh)."
                );
            }
            let udp = server.clone();
            tokio::spawn(async move {
                if let Err(e) = udp.run(sip_addr).await {
                    tracing::error!("SIP ingress stopped: {e}");
                }
            });
            tracing::info!(addr = %sip_addr, "SIP signalling ingress listening (UDP)");
        }

        // TLS ingress (SIPS) — opt-in, and only in a `--features tls` build. Encrypting the
        // signalling channel protects the SDES SRTP keys and call metadata in transit.
        #[cfg(feature = "tls")]
        if let Some(sips_addr) = cfg.sips_listen {
            match load_sip_tls(&cfg) {
                Ok(tls_config) => {
                    let tls_server = server.clone();
                    tokio::spawn(async move {
                        if let Err(e) = sip::tls::run_tls(tls_server, sips_addr, tls_config).await {
                            tracing::error!("SIP/TLS ingress stopped: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("SIP/TLS ingress could not start: {e}");
                    return exit::CONFIG;
                }
            }
        }
        #[cfg(not(feature = "tls"))]
        if cfg.sips_listen.is_some() {
            tracing::warn!("sips_listen is set but this binary was built without `--features tls`; the SIPS ingress is DISABLED");
        }
    }

    // Media-fact loop: apply media→control facts (ring/answer/…) to Call state and emit their
    // events (CMOS-03-ARCH-003). In the single binary this is an in-process channel; in the
    // split-media topology it is the media node's fact stream — same control-plane logic.
    {
        let routing = routing.clone();
        tokio::spawn(async move {
            while let Some(fact) = fact_rx.recv().await {
                if let Err(e) = routing.apply_fact(fact).await {
                    tracing::warn!("failed to apply media fact: {e}");
                }
            }
        });
    }

    let bind = cfg.listen.to_string();
    let app_state = AppState::new(
        store.clone(),
        routing,
        messaging,
        realtime,
        queues,
        call_flows,
        ivrs,
        trunking,
        provisioning,
        webhooks,
        objects,
        recordings,
        voicemails,
        metrics.clone(),
        agents,
        registrations,
        auth,
        admin,
        cfg.media_ip,
        cfg.sip_listen.map(|a| a.port()).unwrap_or(5060),
        cfg.ntp_server.clone(),
        cfg.timezone.clone(),
        describe_storage(&cfg),
        bus.clone(),
        recent,
    );

    // --- shutdown plumbing -----------------------------------------------------------
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Outbox relay (CMOS-03-ARCH-030). Owns its own store handle and drains on shutdown.
    let relay_handle = tokio::spawn(relay::run(
        store.clone(),
        bus.clone(),
        signal.clone(),
        shutdown_rx.clone(),
    ));

    // Webhook dispatcher (Volume 5 §EVT-014): deliver relayed events to registered endpoints.
    tokio::spawn(control::webhooks::run(
        store.clone(),
        signal.clone(),
        bus.clone(),
        shutdown_rx.clone(),
    ));

    // Metrics collector: fold the relayed event stream into counters. Subscribing to the bus
    // keeps the control plane free of metrics plumbing (Volume 15 §OBS-010).
    {
        let metrics = metrics.clone();
        let mut rx = bus.subscribe();
        let mut shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
                    recv = rx.recv() => match recv {
                        Ok(ev) => {
                            if let Some(t) = ev.get("type").and_then(|v| v.as_str()) {
                                metrics.on_event(t);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
    }

    // --- bind & serve ----------------------------------------------------------------
    let listener = match TcpListener::bind(cfg.listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %cfg.listen, "cannot bind listener: {e}");
            return exit::UNAVAILABLE;
        }
    };

    let router = api::router(app_state.clone());

    // Warm-up done: mark ready so the load balancer admits this node (CMOS-14-DEP-033).
    app_state.set_ready(true);
    tracing::info!(
        addr = %bind,
        version = env!("CARGO_PKG_VERSION"),
        arch = std::env::consts::ARCH,
        "commosd ready"
    );

    let state_for_shutdown = app_state.clone();
    let graceful = async move {
        wait_for_signal().await;
        // Drain: report not-ready first so the LB stops sending new work, then let
        // in-flight requests finish (CMOS-14-DEP-051).
        tracing::info!("shutdown signal received — draining");
        state_for_shutdown.set_ready(false);
        let _ = shutdown_tx.send(true);
    };

    // Serve with peer connection info so extractors can classify each request by its source
    // address (loopback/private = trusted LAN, public = must authenticate). See `api::peer`.
    let serve = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(graceful);
    if let Err(e) = serve.await {
        tracing::error!("server error: {e}");
        return exit::UNAVAILABLE;
    }

    // Let the relay finish its final drain.
    let _ = relay_handle.await;
    tracing::info!("commosd stopped cleanly");
    exit::OK
}

/// Build the rustls config for the SIPS listener from the configured certificate path and key
/// reference, so a missing or unreadable/invalid cert/key surfaces as a boot-time error rather
/// than a silent failure inside the accept loop.
#[cfg(feature = "tls")]
fn load_sip_tls(cfg: &Config) -> Result<Arc<tokio_rustls::rustls::ServerConfig>, String> {
    let cert_path = cfg
        .sip_tls_cert
        .as_ref()
        .ok_or("sips_listen is set but sip_tls_cert is not configured")?;
    let key_ref = cfg
        .sip_tls_key
        .as_ref()
        .ok_or("sips_listen is set but sip_tls_key is not configured")?;
    let cert_pem =
        std::fs::read(cert_path).map_err(|e| format!("reading sip_tls_cert '{cert_path}': {e}"))?;
    let key_pem = key_ref.resolve().map_err(|e| e.to_string())?.into_bytes();
    let config = sip::tls::load_server_config(&cert_pem, &key_pem).map_err(|e| e.to_string())?;
    Ok(Arc::new(config))
}

/// Establish an HS256 JWT signing secret when the operator has not configured one, so signed
/// bearer auth works for remote callers with zero setup. The secret is generated once from the
/// OS CSPRNG and persisted at `{data_dir}/secrets/jwt.key` (0600 on unix) so tokens survive
/// restarts; subsequent boots reuse it. A configured `jwt_secret` reference always takes
/// precedence over this and this path is not reached.
fn ensure_jwt_secret(data_dir: &str) -> Result<Vec<u8>, String> {
    let dir = format!("{}/secrets", data_dir.trim_end_matches('/'));
    let path = format!("{dir}/jwt.key");

    // Reuse an existing secret if we already generated one.
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() >= 32 {
            tracing::info!(path = %path, "bearer auth: using auto-generated JWT secret");
            return Ok(bytes);
        }
        // A too-short/truncated file is unusable; fall through and regenerate.
        tracing::warn!(path = %path, "existing JWT secret is too short; regenerating");
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {dir}: {e}"))?;

    // 32 bytes (256 bits) from the OS CSPRNG.
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret).map_err(|e| format!("CSPRNG: {e}"))?;

    // Write restrictively: the secret must not be world-readable.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("writing {path}: {e}"))?;
        f.write_all(&secret).map_err(|e| format!("writing {path}: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, secret).map_err(|e| format!("writing {path}: {e}"))?;
    }

    tracing::warn!(
        path = %path,
        "bearer auth: generated a new JWT signing secret (persisted). Mint tenant JWTs with it \
         for remote API access; the tenant:<uuid> dev token works only from the local network."
    );
    Ok(secret.to_vec())
}

/// Choose the system-of-record binding from config, connecting/opening it.
///
/// Default (no `database_url`): the **embedded SQLite** store — durable with zero external
/// dependency (ADR-0012), the right fit for a single box / Raspberry Pi. `postgres://…`
/// selects PostgreSQL (multi-node / HA); `memory://` selects the ephemeral in-process store.
/// A human-readable, secret-free description of where the system of record lives — shown to the
/// operator at the end of onboarding so they know exactly where their configuration was saved.
/// Mirrors [`select_store`]'s branching. Never echoes a resolved DSN (it may carry credentials):
/// PostgreSQL is named without its connection string; SQLite paths are safe and are made absolute.
fn describe_storage(cfg: &Config) -> String {
    let abs = |p: &str| {
        std::fs::canonicalize(p)
            .ok()
            .map(|x| x.display().to_string())
            .unwrap_or_else(|| p.to_string())
    };
    match &cfg.database_url {
        None => format!("embedded SQLite database at {}", abs(&cfg.default_sqlite_path())),
        Some(secret) => match secret.resolve() {
            Ok(d) if d.starts_with("postgres://") || d.starts_with("postgresql://") => {
                "an external PostgreSQL database (configured via database_url)".to_string()
            }
            Ok(d) if d == "memory://" || d == ":memory:" => {
                "an in-process memory store — NOT durable, lost on restart".to_string()
            }
            Ok(d) => {
                let path = d.strip_prefix("sqlite:").unwrap_or(&d);
                format!("SQLite database at {}", abs(path))
            }
            // Can't resolve the reference here; don't guess or leak it.
            Err(_) => "the database referenced by database_url".to_string(),
        },
    }
}

async fn select_store(cfg: &Config) -> Result<Arc<dyn store::Store>, i32> {
    let dsn = match &cfg.database_url {
        Some(secret) => match secret.resolve() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("{e}");
                return Err(exit::CONFIG);
            }
        },
        None => {
            let path = cfg.default_sqlite_path();
            tracing::info!(path = %path, "system of record: embedded SQLite (durable, zero external dependency)");
            return store::SqliteStore::connect(&path)
                .await
                .map(|s| Arc::new(s) as Arc<dyn store::Store>)
                .map_err(|e| {
                    tracing::error!("cannot open SQLite database: {e}");
                    exit::UNAVAILABLE
                });
        }
    };

    if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        store::PgStore::connect(&dsn)
            .await
            .map(|s| {
                tracing::info!("system of record: PostgreSQL");
                Arc::new(s) as Arc<dyn store::Store>
            })
            .map_err(|e| {
                tracing::error!("cannot reach PostgreSQL: {e}");
                exit::UNAVAILABLE
            })
    } else if dsn == "memory://" || dsn == ":memory:" {
        tracing::warn!("system of record: in-process store (ephemeral — not durable across restarts)");
        Ok(Arc::new(MemStore::new()))
    } else {
        // Anything else is a SQLite path/DSN (`sqlite:foo.db` or a bare path).
        let path = dsn.strip_prefix("sqlite:").unwrap_or(&dsn).to_string();
        store::SqliteStore::connect(&path)
            .await
            .map(|s| {
                tracing::info!(path = %path, "system of record: SQLite");
                Arc::new(s) as Arc<dyn store::Store>
            })
            .map_err(|e| {
                tracing::error!("cannot open SQLite database: {e}");
                exit::UNAVAILABLE
            })
    }
}

/// Choose the object-storage backend from config. Default (no `object_storage`): the local
/// filesystem under `{data_dir}/objects` — durable with zero external dependency. `s3://<bucket>`
/// selects S3-compatible storage, which requires a build with `--features s3` and credentials in
/// the environment (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`).
fn select_object_store(cfg: &Config) -> Result<Arc<dyn objectstore::ObjectStore>, i32> {
    match &cfg.object_storage {
        Some(url) if url.starts_with("s3://") => {
            #[cfg(feature = "s3")]
            {
                let bucket = url.trim_start_matches("s3://").trim_end_matches('/');
                if bucket.is_empty() {
                    tracing::error!("object_storage 's3://' requires a bucket name (e.g. s3://my-bucket)");
                    return Err(exit::CONFIG);
                }
                let ak = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default();
                let sk = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
                if ak.is_empty() || sk.is_empty() {
                    tracing::error!(
                        "S3 object storage needs AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY in the environment"
                    );
                    return Err(exit::CONFIG);
                }
                match objectstore::S3ObjectStore::new(
                    bucket,
                    cfg.s3_region.clone(),
                    cfg.s3_endpoint.clone(),
                    ak,
                    sk,
                    cfg.s3_path_style,
                ) {
                    Ok(s) => {
                        tracing::info!(bucket, endpoint = ?cfg.s3_endpoint, "object storage: S3-compatible");
                        Ok(Arc::new(s))
                    }
                    Err(e) => {
                        tracing::error!("cannot initialise S3 object storage: {e}");
                        Err(exit::UNAVAILABLE)
                    }
                }
            }
            #[cfg(not(feature = "s3"))]
            {
                let _ = url;
                tracing::error!(
                    "object_storage is set to 's3://' but this binary was built without the `s3` feature — rebuild with `cargo build --features s3`"
                );
                Err(exit::CONFIG)
            }
        }
        _ => {
            let root = format!("{}/objects", cfg.data_dir.trim_end_matches('/'));
            tracing::info!(path = %root, "object storage: local filesystem");
            Ok(Arc::new(objectstore::LocalObjectStore::new(root)))
        }
    }
}

/// Resolve on SIGINT (Ctrl-C) or SIGTERM (systemd stop).
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
