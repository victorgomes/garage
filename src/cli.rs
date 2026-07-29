//! Command-line parsing (TODO 1.3).
//!
//! Three invocation shapes:
//!
//! ```text
//! garage trace.log [more.log ...]     # files
//! d8 ... | garage                     # stdin is the data pipe
//! garage -- d8 --print-maglev-graphs x.js   # wrapper mode (TODO 9.4)
//! ```
//!
//! The third one is why argv is split by hand before clap sees it. clap treats
//! `--` as "everything after this is positional" and would merge the wrapped
//! command into `files`, making `garage -- d8 x.js` indistinguishable from
//! `garage d8 x.js`. So the split happens first, and clap only ever parses the
//! left-hand side.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use regex::Regex;

#[derive(Parser, Debug)]
#[command(
    name = "garage",
    version,
    about = "Terminal UI for V8 trace output",
    long_about = "Terminal UI for viewing, navigating, searching and diffing d8 \
                  trace output.\n\n\
                  Reads trace files, or stdin when d8 output is piped in. \
                  Keyboard input comes from the controlling terminal in that \
                  case, so `d8 ... | garage` is fully interactive."
)]
pub struct Cli {
    /// Trace files to open. Omit to read the trace from stdin.
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,

    /// Only keep compilations whose function name matches this regex.
    ///
    /// A parse-time prefilter: matching happens while indexing, so it also
    /// bounds memory on very large traces (PLAN J6).
    #[arg(long, value_name = "REGEX")]
    pub function: Option<String>,

    /// Config file to load instead of ~/.config/garage/config.toml.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Write debug diagnostics to the log file.
    #[arg(long)]
    pub debug: bool,

    /// Where diagnostics go when they are enabled.
    #[arg(long, value_name = "PATH", default_value = "garage.log")]
    pub log_file: PathBuf,
}

/// A parsed command line, with the `--` tail separated out and the regex
/// compiled — both failures we would rather hit before touching the terminal.
#[derive(Debug)]
pub struct Invocation {
    pub cli: Cli,
    /// The command after `--`, if any. Non-empty when present.
    pub command: Option<Vec<OsString>>,
    pub function: Option<Regex>,
}

impl Invocation {
    pub fn from_env() -> Result<Self> {
        Self::from_argv(std::env::args_os())
    }

    pub fn from_argv<I, T>(argv: I) -> Result<Self>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
        let (head, command) = split_at_double_dash(argv);

        let cli = Cli::try_parse_from(head)?;

        if let Some(cmd) = &command {
            if cmd.is_empty() {
                bail!(
                    "expected a command after `--`, e.g. `garage -- d8 --print-maglev-graphs x.js`"
                );
            }
            if !cli.files.is_empty() {
                bail!("cannot read files and wrap a command at the same time");
            }
        }

        // Compiled here so a bad pattern fails on the command line rather than
        // after the alternate screen is already up.
        let function = cli
            .function
            .as_deref()
            .map(Regex::new)
            .transpose()
            .context("--function is not a valid regex")?;

        Ok(Self {
            cli,
            command,
            function,
        })
    }
}

/// Splits argv at the first bare `--`.
///
/// Returns everything up to it (for clap) and everything after it (the wrapped
/// command). `Some(vec![])` distinguishes a trailing `--` with nothing after it
/// from no `--` at all, so the caller can report it.
fn split_at_double_dash(argv: Vec<OsString>) -> (Vec<OsString>, Option<Vec<OsString>>) {
    match argv.iter().position(|a| a == "--") {
        Some(i) => {
            let mut head = argv;
            let tail = head.split_off(i);
            (head, Some(tail[1..].to_vec()))
        }
        None => (argv, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation> {
        Invocation::from_argv(args.iter().map(OsString::from))
    }

    #[test]
    fn files_are_positional() {
        let inv = parse(&["garage", "a.log", "b.log"]).unwrap();
        assert_eq!(inv.cli.files.len(), 2);
        assert!(inv.command.is_none());
    }

    #[test]
    fn no_arguments_means_stdin() {
        let inv = parse(&["garage"]).unwrap();
        assert!(inv.cli.files.is_empty());
        assert!(inv.command.is_none());
    }

    #[test]
    fn double_dash_separates_the_wrapped_command() {
        let inv = parse(&[
            "garage",
            "--debug",
            "--",
            "d8",
            "--print-maglev-graphs",
            "x.js",
        ])
        .unwrap();
        assert!(inv.cli.debug);
        assert!(inv.cli.files.is_empty());
        assert_eq!(
            inv.command.unwrap(),
            vec!["d8", "--print-maglev-graphs", "x.js"]
        );
    }

    /// The whole reason for the manual split: without it clap would swallow the
    /// wrapped command's own flags as garage's.
    #[test]
    fn wrapped_command_keeps_its_own_flags() {
        let inv = parse(&["garage", "--", "d8", "--debug", "--function", "foo"]).unwrap();
        assert!(!inv.cli.debug);
        assert!(inv.cli.function.is_none());
        assert_eq!(inv.command.unwrap().len(), 4);
    }

    #[test]
    fn trailing_double_dash_is_an_error() {
        assert!(parse(&["garage", "--"]).is_err());
    }

    #[test]
    fn files_and_wrapped_command_are_exclusive() {
        assert!(parse(&["garage", "a.log", "--", "d8", "x.js"]).is_err());
    }

    #[test]
    fn bad_function_regex_fails_at_parse_time() {
        assert!(parse(&["garage", "--function", "foo("]).is_err());
        assert!(parse(&["garage", "--function", "^process"]).is_ok());
    }
}
