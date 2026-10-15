//! Client logging: daily rotated files in `<data dir>/logs`, plus stderr when
//! a terminal is attached. Tokens, audio and transcript text are never logged.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

pub fn init(logs_dir: &Path, file_name: &str) -> Option<WorkerGuard> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let console = fmt::layer().with_writer(std::io::stderr).with_filter(filter());
    if std::fs::create_dir_all(logs_dir).is_err() {
        let _ = tracing_subscriber::registry().with(console).try_init();
        return None;
    }
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(file_name)
        .filename_suffix("log")
        .max_log_files(14)
        .build(logs_dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file = fmt::layer().with_ansi(false).with_writer(writer).with_filter(filter());
    let _ = tracing_subscriber::registry().with(console).with(file).try_init();
    Some(guard)
}
