use std::process::ExitCode;
use std::sync::mpsc;

use anyhow::{Result, bail};

use garage::cli::Invocation;
use garage::source::LogSource;
use garage::{app, event, logging, source, terminal, tty};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // clap already knows how to print its own errors, including help
            // and the conventional exit codes.
            if let Some(e) = error.downcast_ref::<clap::Error>() {
                e.exit();
            }
            // The terminal is restored by then in every path that got as far as
            // entering it, so stderr is safe to use.
            eprintln!("garage: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let invocation = Invocation::from_env()?;

    let log_path = logging::init(&invocation.cli.log_file, invocation.cli.debug)?;
    if let Some(path) = &log_path {
        tracing::debug!(path = %path.display(), "diagnostics enabled");
    }

    // Config (keymap) parses before the terminal: a typo in config.toml is a
    // normal error message, not a broken TUI.
    let config = garage::config::Config::load(invocation.cli.config.as_deref())?;

    // Before `tty::acquire`, which makes stdin a terminal.
    let sources = choose_sources(&invocation)?;
    tracing::info!(count = sources.len(), "opening sources");

    // Sort the descriptors out before anything reads a key or a byte: this
    // moves a piped stdin out of fd 0 and puts the controlling terminal there.
    let access = tty::acquire()?;

    let (tx, rx) = mpsc::channel();

    // Terminal first: readers should not start filling a channel nobody is
    // draining, and a failure to grab the terminal should not leave threads
    // holding a pipe open.
    let mut guard = terminal::enter()?;
    event::spawn_input_thread(tx.clone())?;
    source::spawn_readers(&sources, access.data, tx.clone());

    let mut state = app::App::new(&sources, invocation.function, config.keys);
    let result = app::run(&mut guard, &mut state, rx);

    // Explicit, so the terminal is back before anything is printed. The guard's
    // Drop would do it too; this just makes the ordering visible.
    drop(guard);
    terminal::restore();
    // Wrapper mode: the session owns the child; quitting garage must not
    // leave a d8 running headless into a dead channel.
    source::kill_child();

    result
}

/// Decides where the trace comes from.
///
/// The three shapes from PLAN §4.1, in priority order: an explicit command,
/// explicit files, or — the primary invocation — a pipe on stdin.
fn choose_sources(invocation: &Invocation) -> Result<Vec<LogSource>> {
    // Wrapper mode (TODO 9.4): spawn the command, stream its output live.
    if let Some(command) = &invocation.command {
        let argv: Vec<String> = command
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        return Ok(vec![LogSource::Command(argv)]);
    }

    if !invocation.cli.files.is_empty() {
        return Ok(invocation
            .cli
            .files
            .iter()
            .cloned()
            .map(LogSource::File)
            .collect());
    }

    if tty::stdin_is_data() {
        return Ok(vec![LogSource::Stdin]);
    }

    bail!("no input: pass a trace file, or pipe one in (`d8 ... | garage`)")
}
