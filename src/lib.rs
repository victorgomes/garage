//! `garage` — a terminal UI for V8 trace output.
//!
//! Split into a library and a thin binary so the parsers can be tested without
//! a terminal. The golden-file suite over `fixtures/` (TODO 2.7) is the reason
//! this matters: those tests need the parsing half and none of the UI half.

pub mod ansi;
pub mod app;
pub mod cli;
pub mod event;
pub mod logging;
pub mod source;
pub mod terminal;
pub mod tty;
pub mod ui;
