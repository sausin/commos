//! Structured logging (Volume 15). Log output integrates with the platform's structured
//! logging and is systemd-friendly (CMOS-14-DEP-002).

use crate::config::{LogConfig, LogFormat};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialise the global tracing subscriber from config. `RUST_LOG` overrides the level.
pub fn init(cfg: &LogConfig) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.level.clone()));

    let registry = tracing_subscriber::registry().with(filter);
    match cfg.format {
        LogFormat::Json => registry.with(fmt::layer().json().flatten_event(true)).init(),
        LogFormat::Text => registry.with(fmt::layer()).init(),
    }
}
