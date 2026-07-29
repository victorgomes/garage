//! File-based diagnostics (TODO 1.2).
//!
//! Nothing may write to stdout or stderr while the alternate screen is up, so
//! `tracing` goes to a file and only to a file. The subscriber is installed
//! only when asked for — `--debug`, or `GARAGE_LOG=<filter>` — so a normal run
//! does not drop a `garage.log` into whatever directory the user happened to be
//! in.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Writes every event to one long-lived file handle.
///
/// `&File` implements `Write`, so the handle can be shared without a mutex;
/// each event is a single `write` on a file opened for append, which is enough
/// ordering for a diagnostics log.
struct FileWriter(Arc<File>);

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = &'a File;

    fn make_writer(&'a self) -> Self::Writer {
        &self.0
    }
}

/// Installs the file subscriber if diagnostics were requested.
///
/// Returns the log path when one was opened, so the caller can tell the user
/// where to look.
pub fn init(path: &Path, debug: bool) -> Result<Option<PathBuf>> {
    let filter = match (debug, std::env::var("GARAGE_LOG")) {
        (_, Ok(spec)) if !spec.is_empty() => EnvFilter::new(spec),
        (true, _) => EnvFilter::new("garage=debug"),
        (false, _) => return Ok(None),
    };

    let file = File::options()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open log file {}", path.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(FileWriter(Arc::new(file)))
        .with_ansi(false)
        .with_target(true)
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "garage started");
    Ok(Some(path.to_path_buf()))
}

/// Logs a panic payload, for the panic hook.
///
/// Called after the terminal has been restored, so this is belt-and-braces:
/// the message also reaches stderr. The point is that a user who ran with
/// `--debug` has it in the file to attach to a bug report.
pub fn record_panic(info: &std::panic::PanicHookInfo<'_>) {
    let message = payload(info);
    match info.location() {
        Some(loc) => tracing::error!(location = %loc, "panic: {message}"),
        None => tracing::error!("panic: {message}"),
    }
}

fn payload(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}
