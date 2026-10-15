//! Logging to stdout and daily rotated files.
//!
//! Tokens are never logged in full (see `auth::redact`), and neither audio nor
//! transcript text is ever written to the log.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::config::ServerConfig;

/// Sets up logging. Keep the guard alive until shutdown so buffered lines are flushed.
pub fn init(config: &ServerConfig) -> anyhow::Result<WorkerGuard> {
    let dir = config.log_dir();
    std::fs::create_dir_all(&dir)?;
    let appender = tracing_appender::rolling::daily(&dir, "server.log");
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{},tower_http=warn", config.logging.level)));

    let registry = tracing_subscriber::registry().with(filter);
    if config.logging.format == "json" {
        registry
            .with(fmt::layer().json().with_writer(std::io::stdout))
            .with(fmt::layer().json().with_writer(file_writer))
            .try_init()?;
    } else {
        registry
            .with(fmt::layer().with_writer(std::io::stdout))
            .with(fmt::layer().with_ansi(false).with_writer(file_writer))
            .try_init()?;
    }
    Ok(guard)
}

/// Minimal logging for admin commands.
pub fn init_cli() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .with_writer(std::io::stderr)
        .try_init();
}
