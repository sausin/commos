//! `commosd` — the CommOS single self-contained binary (CMOS-14-DEP-001/010).
//!
//! One process runs the control plane and (a loopback of) the media plane, with the
//! transactional outbox relaying events to an in-process Event Bus. Its only intended hard
//! dependency at scale is PostgreSQL (CMOS-14-DEP-020); with no `pbx.yaml` it boots on the
//! zero-dependency in-memory store so the artifact runs anywhere — a Raspberry Pi 4, a
//! server, a container — out of the box.
//!
//! Operable under systemd (CMOS-14-DEP-002): clean start/stop, graceful drain on SIGTERM,
//! and a defined exit-code contract (see [`exit`]).

mod api;
mod bus;
mod config;
mod control;
mod introspect;
mod media;
mod relay;
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

fn parse_args() -> Result<PathBuf, i32> {
    let mut path = PathBuf::from("pbx.yaml");
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

    // Select the system-of-record binding. With a configured (referenced) database we use
    // the durable PostgreSQL store (CMOS-14-DEP-020); with none we run the zero-dependency
    // in-process store so the single binary boots anywhere (CMOS-14-DEP-021). Callers below
    // are identical either way (CMOS-14-DEP-042).
    let store: Arc<dyn store::Store> = match &cfg.database_url {
        Some(secret) => {
            let dsn = match secret.resolve() {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("{e}");
                    return exit::CONFIG;
                }
            };
            match store::PgStore::connect(&dsn).await {
                Ok(pg) => {
                    tracing::info!("connected to PostgreSQL system of record");
                    Arc::new(pg)
                }
                Err(e) => {
                    tracing::error!("cannot reach the database: {e}");
                    return exit::UNAVAILABLE;
                }
            }
        }
        None => {
            tracing::warn!(
                "no database configured — running on the in-process store (single-binary, \
                 zero-dependency mode). State is not durable across restarts."
            );
            Arc::new(MemStore::new())
        }
    };

    let media = Arc::new(LoopbackMedia);
    let signal = RelaySignal::new();
    let routing = Routing::new(store.clone(), media, signal.clone());
    let messaging = control::messaging::MessagingService::new(store.clone(), signal.clone());

    let bind = cfg.listen.to_string();
    let app_state = AppState::new(store.clone(), routing, messaging, bus.clone(), recent);

    // --- shutdown plumbing -----------------------------------------------------------
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Outbox relay (CMOS-03-ARCH-030). Owns its own store handle and drains on shutdown.
    let relay_handle = tokio::spawn(relay::run(
        store.clone(),
        bus.clone(),
        signal.clone(),
        shutdown_rx.clone(),
    ));

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

    let serve = axum::serve(listener, router).with_graceful_shutdown(graceful);
    if let Err(e) = serve.await {
        tracing::error!("server error: {e}");
        return exit::UNAVAILABLE;
    }

    // Let the relay finish its final drain.
    let _ = relay_handle.await;
    tracing::info!("commosd stopped cleanly");
    exit::OK
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
