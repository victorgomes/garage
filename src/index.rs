//! The streaming section indexer (TODO 2.2) and timeline-event parser (2.5).
//!
//! One pass over the line stream, finding section boundaries **without parsing
//! bodies** — this is what makes a 1 GB file open instantly (PLAN §3.3). The
//! pass is incremental: `ingest` consumes whatever complete lines have arrived
//! and picks up where it left off on the next chunk.
//!
//! Anchoring rules (spike-findings.md §2):
//!
//! - A compilation is anchored on `Compiling 0x… <JSFunction …> with <Tier>`,
//!   which exists in every corpus version. `Begin compiling method` does not
//!   exist before V8 15.2 and is only a preamble/display-name supplement.
//! - Phase banners share one grammar (`----- <name> -----`) with two
//!   non-phases matched specially: `Bytecode array` and `Inlining … with
//!   bytecode` (§9).
//! - The `-----------…` rule lines that 15.2 prints around the begin/finished
//!   banners are buffered as *pending* until the next classified line decides
//!   whether they belong to the closing compilation or the opening one.
//!
//! Everything that is not inside a compilation lands in a [`RawSection`] —
//! the sections partition the file, no line is ever dropped (PLAN principle
//! 1). Single-line lifecycle events (`[marking …]`, `[bailout …]`, `[OSR …]`)
//! are additionally recorded as [`TimelineEvent`]s wherever they appear; an
//! event line also *closes* an open compilation, because deopts happen at run
//! time — with concurrent recompilation this can split an interleaved dump,
//! which degrades the tail to raw per the demultiplexing strategy (PLAN §4.1).

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::ansi;
use crate::markers::{MarkerTable, VersionVotes};
use crate::model::{
    Addr, CompilationKey, CompilationSection, Confidence, EventKind, OsrInfo, PhaseKind,
    PhaseSection, RawSection, Tier, TimelineEvent,
};
use crate::source::LogBuffer;

/// Compiled once; the indexer runs per line and must stay cheap.
struct Res {
    compiling: Regex,
    begin: Regex,
    finished: Regex,
    banner: Regex,
    rule: Regex,
    inlining_banner: Regex,
    ev_marking: Regex,
    ev_compile_start: Regex,
    ev_compile_done: Regex,
    ev_deopt_begin: Regex,
    ev_deopt_end: Regex,
    ev_osr: Regex,
}

fn res() -> &'static Res {
    static RES: OnceLock<Res> = OnceLock::new();
    RES.get_or_init(|| {
        // `<JSFunction name (sfi = 0x…)>` where the name is empty for the
        // toplevel script function (spike-findings.md §11): the ` ?… ?`
        // dance accepts both `JSFunction read (sfi` and `JSFunction (sfi`.
        const FUNC: &str = r"<JSFunction ?(.*?) ?\(sfi = (0x[0-9a-f]+)\)>";
        let re = |s: &str| Regex::new(s).expect("static regex");
        Res {
            compiling: re(&format!(
                r"^Compiling (0x[0-9a-f]+) {FUNC} with (\S+)\s*$"
            )),
            begin: re(r"^Begin compiling method (.*?) using (\S+)\s*$"),
            finished: re(r"^Finished compiling method (.*?) using (\S+)\s*$"),
            banner: re(r"^----- (.+?) -----\s?$"),
            rule: re(r"^-{20,}\s*$"),
            inlining_banner: re(r"^Inlining (0x[0-9a-f]+) <SharedFunctionInfo ?(.*?)> with bytecode$"),
            ev_marking: re(&format!(
                r"^\[marking (?:0x[0-9a-f]+ )?{FUNC} for optimization to ([A-Z_]+),.*reason: (.*?)\]$"
            )),
            ev_compile_start: re(&format!(
                r"^\[compiling method (?:0x[0-9a-f]+ )?{FUNC} \(target ([A-Z_]+)\)( OSR)?, mode: .*\]$"
            )),
            ev_compile_done: re(&format!(
                r"^\[completed compiling (?:0x[0-9a-f]+ )?{FUNC} \(target ([A-Z_]+)\)( OSR)? - took .*\]$"
            )),
            ev_deopt_begin: re(&format!(
                r"^\[bailout \(kind: ([^,]+), reason: (.*?)\): begin\. deoptimizing (?:0x[0-9a-f]+ )?{FUNC}, 0x[0-9a-f]+ <Code ([A-Z_]+)>, opt id (\d+), bytecode offset (\d+),.*\]$"
            )),
            ev_deopt_end: re(r"^\[bailout end\. code_invalidation: (\w+).*\]$"),
            ev_osr: re(r"^\[OSR - ([^.]*)\. function: ([^,\]]*)(?:, osr offset: (\d+))?.*\]$"),
        }
    })
}

/// Rule/`Begin compiling` lines seen but not yet attributed: whether they open
/// the next compilation or close the current one depends on the line after
/// them.
#[derive(Debug)]
struct Pending {
    start: usize,
}

#[derive(Debug, Clone, Copy)]
enum State {
    /// Top level. `raw_since` is the start of an open raw section, if any.
    Idle { raw_since: Option<usize> },
    /// Inside compilation `self.compilations[comp]`.
    InCompilation { comp: usize },
}

/// The index of one source: sections that partition the lines, plus the
/// timeline events found along the way.
pub struct TraceIndex {
    pub compilations: Vec<CompilationSection>,
    pub raw: Vec<RawSection>,
    pub events: Vec<TimelineEvent>,
    /// Best guess at the V8 version, from marker-table votes.
    pub detected_version: Option<String>,

    state: State,
    pending: Option<Pending>,
    raw_label: Option<String>,
    next_line: usize,
    ordinals: HashMap<(Addr, Tier), u32>,
    votes: VersionVotes,
    /// Last `[compiling method …]` event: `(sfi, tier, osr)`. Binds the OSR
    /// flag to the next matching graph dump (correlation-keys.md §3 rule 1).
    last_compile_start: Option<(Addr, Tier, bool)>,
    /// Last `[OSR - compilation started]` offset per function *name* — a weak
    /// key, so anything derived from it is marked [`Confidence::Heuristic`].
    osr_offset_by_name: HashMap<String, u32>,
    function_filter: Option<Regex>,
}

impl TraceIndex {
    pub fn new(function_filter: Option<Regex>) -> Self {
        Self {
            compilations: Vec::new(),
            raw: Vec::new(),
            events: Vec::new(),
            detected_version: None,
            state: State::Idle { raw_since: None },
            pending: None,
            raw_label: None,
            next_line: 0,
            ordinals: HashMap::new(),
            votes: VersionVotes::default(),
            last_compile_start: None,
            osr_offset_by_name: HashMap::new(),
            function_filter,
        }
    }

    /// Consumes complete lines that arrived since the last call. While the
    /// source is still streaming, the final line may be a partial one whose
    /// bytes are not all here yet, so it is held back until `finished`.
    pub fn ingest(&mut self, buffer: &LogBuffer, finished: bool) {
        let started = std::time::Instant::now();
        let from = self.next_line;
        let mut end = buffer.line_count();
        if !finished {
            end = end.saturating_sub(1);
        }
        while self.next_line < end {
            let i = self.next_line;
            let line = buffer.line(i).unwrap_or(b"");
            self.step(i, line);
            self.next_line += 1;
        }
        if finished {
            self.close_all(buffer.line_count());
            self.detected_version = self.votes.detected().map(str::to_string);
        }
        // Alongside the line-index timing in LogBuffer, this is the other
        // O(file) step on the open path (PLAN §12: 500 MB < 2 s).
        if self.next_line - from > 100_000 {
            tracing::debug!(
                lines = self.next_line - from,
                compilations = self.compilations.len(),
                events = self.events.len(),
                micros = started.elapsed().as_micros(),
                "sections indexed"
            );
        }
    }

    /// The number of lines fully indexed so far.
    pub fn lines_indexed(&self) -> usize {
        self.next_line
    }

    fn step(&mut self, i: usize, line: &[u8]) {
        // Fast path: only a handful of first bytes can start a boundary line
        // (`-----`, `Begin`, `Compiling`, `Finished`, `[event]`), possibly
        // behind an ANSI escape. Everything else — the vast majority of graph
        // body lines — extends the current section with no further work.
        match line.first() {
            Some(b'-' | b'B' | b'C' | b'F' | b'[' | 0x1b) => {
                let stripped = ansi::strip(line);
                match std::str::from_utf8(&stripped) {
                    Ok(text) => self.classify(i, text),
                    Err(_) => self.content(i, line),
                }
            }
            _ => self.content(i, line),
        }
    }

    fn classify(&mut self, i: usize, text: &str) {
        let r = res();

        // Cheap prefix guards before any regex runs.
        if text.starts_with("-----") {
            if let Some(c) = r.banner.captures(text) {
                self.on_banner(i, &c[1]);
                return;
            }
            if r.rule.is_match(text) {
                self.on_rule(i);
                return;
            }
        } else if text.starts_with("Compiling ") {
            if let Some(c) = r.compiling.captures(text) {
                let (addr, name, sfi, tier) = (&c[1], &c[2], &c[3], &c[4]);
                self.on_compiling(i, addr, name, sfi, tier);
                return;
            }
        } else if text.starts_with("Begin compiling method ") {
            if r.begin.is_match(text) {
                self.on_begin(i);
                return;
            }
        } else if text.starts_with("Finished compiling method ") {
            if r.finished.is_match(text) {
                self.on_finished(i, text);
                return;
            }
        } else if text.starts_with('[') {
            if let Some(kind) = parse_event(text) {
                self.on_event(i, kind, text);
                return;
            }
        }

        self.content(i, text.as_bytes());
    }

    /// A non-boundary line: extends whatever is open. Compilation extents are
    /// kept live so that a still-streaming compilation is viewable (and its
    /// parse cache invalidates) before it closes.
    fn content(&mut self, i: usize, line: &[u8]) {
        self.flush_pending_into_current();
        match &mut self.state {
            State::InCompilation { comp } => {
                let section = &mut self.compilations[*comp];
                section.lines.end = i + 1;
                if let Some(open) = section.phases.last_mut() {
                    open.lines.end = i + 1;
                } else {
                    section.preamble.end = i + 1;
                }
            }
            State::Idle { raw_since } => {
                if raw_since.is_none() {
                    *raw_since = Some(i);
                    self.raw_label = None;
                }
                if self.raw_label.is_none() && !line.is_empty() {
                    self.raw_label = Some(label_from(line));
                }
            }
        }
    }

    fn on_rule(&mut self, i: usize) {
        // Ambiguous until the next line: 15.2 prints a rule both before
        // `Finished compiling` (belongs to the closing compilation) and before
        // `Begin compiling` (belongs to the opening one).
        if self.pending.is_none() {
            self.pending = Some(Pending { start: i });
        }
    }

    fn on_begin(&mut self, i: usize) {
        if self.pending.is_none() {
            self.pending = Some(Pending { start: i });
        }
    }

    fn on_compiling(&mut self, i: usize, addr: &str, name: &str, sfi: &str, tier: &str) {
        let cut = self.pending.take().map(|p| p.start).unwrap_or(i);
        self.close_all(cut);

        let sfi = match Addr::parse(sfi) {
            Some(a) => a,
            None => {
                // Cannot happen with the regex above, but never panic on input.
                tracing::error!(line = i, "unparseable sfi address");
                Addr(0)
            }
        };
        let tier = Tier::parse(tier);
        let ordinal = {
            let n = self.ordinals.entry((sfi, tier.clone())).or_insert(0);
            *n += 1;
            *n
        };

        let osr = match &self.last_compile_start {
            Some((event_sfi, event_tier, true)) if *event_sfi == sfi && *event_tier == tier => {
                Some(OsrInfo {
                    offset: self.osr_offset_by_name.get(name).copied(),
                    confidence: if self.osr_offset_by_name.contains_key(name) {
                        Confidence::Heuristic
                    } else {
                        Confidence::Exact
                    },
                })
            }
            _ => None,
        };

        let filtered_out = match &self.function_filter {
            Some(re) => {
                let display = if name.is_empty() { "<toplevel>" } else { name };
                !re.is_match(display)
            }
            None => false,
        };

        self.compilations.push(CompilationSection {
            key: CompilationKey { sfi, tier, ordinal },
            name: name.to_string(),
            function_addr: Addr::parse(addr),
            lines: cut..i + 1,
            osr,
            phases: Vec::new(),
            preamble: i + 1..i + 1,
            filtered_out,
        });
        self.state = State::InCompilation {
            comp: self.compilations.len() - 1,
        };
    }

    fn on_finished(&mut self, i: usize, text: &str) {
        match self.state {
            State::InCompilation { comp } => {
                // The pending rule (if any) belongs to this compilation, and so
                // does the Finished line itself.
                self.pending = None;
                self.close_compilation(comp, i + 1);
                self.state = State::Idle { raw_since: None };
            }
            State::Idle { .. } => {
                // A Finished with no open compilation: raw content.
                self.content(i, text.as_bytes());
            }
        }
    }

    fn on_banner(&mut self, i: usize, name: &str) {
        let State::InCompilation { comp } = self.state else {
            // Banner grammar outside a compilation (TurboFan graphs, Phase 8
            // material): raw fallback, never dropped.
            self.content(i, name.as_bytes());
            return;
        };
        // A pending rule inside a compilation followed by a banner is just
        // body content; forget it.
        self.pending = None;

        let kind = if name == "Bytecode array" {
            PhaseKind::Bytecode
        } else if let Some(c) = res().inlining_banner.captures(name) {
            PhaseKind::Inlining {
                sfi: Addr::parse(&c[1]),
                callee: c[2].to_string(),
            }
        } else {
            let known = match MarkerTable::get().phase_versions(name) {
                Some(versions) => {
                    self.votes.record(versions);
                    true
                }
                None => false,
            };
            PhaseKind::Graph { known }
        };

        let section = &mut self.compilations[comp];
        section.lines.end = i + 1;
        if let Some(open) = section.phases.last_mut() {
            open.lines.end = i;
        } else {
            section.preamble.end = i;
        }
        section.phases.push(PhaseSection {
            name: name.to_string(),
            kind,
            lines: i..i + 1,
        });
    }

    fn on_event(&mut self, i: usize, kind: EventKind, text: &str) {
        match &kind {
            EventKind::CompileStart {
                sfi: Some(sfi),
                target,
                osr,
                ..
            } => {
                self.last_compile_start = Some((*sfi, target.clone(), *osr));
            }
            EventKind::Osr {
                what,
                name,
                osr_offset: Some(offset),
            } if what.starts_with("compilation started") => {
                self.osr_offset_by_name.insert(name.clone(), *offset);
            }
            _ => {}
        }
        self.events.push(TimelineEvent { line: i, kind });

        // Lifecycle events happen at run time, outside compile dumps; one
        // arriving inside a dump means interleaving, and the tail degrades to
        // raw (module doc). The event line itself is raw content.
        if matches!(self.state, State::InCompilation { .. }) {
            let cut = self.pending.take().map(|p| p.start).unwrap_or(i);
            self.close_all(cut);
        }
        self.content(i, text.as_bytes());
    }

    /// A pending rule/begin that turned out to precede ordinary content
    /// belongs to whatever is open (or opens a raw section).
    fn flush_pending_into_current(&mut self) {
        let Some(p) = self.pending.take() else { return };
        if let State::Idle { raw_since } = &mut self.state {
            if raw_since.is_none() {
                *raw_since = Some(p.start);
                self.raw_label = None;
            }
        }
    }

    fn close_compilation(&mut self, comp: usize, end: usize) {
        let section = &mut self.compilations[comp];
        section.lines.end = end;
        if let Some(open) = section.phases.last_mut() {
            open.lines.end = end;
        } else {
            section.preamble.end = end;
        }
    }

    /// Closes whatever is open so that the sections partition `0..end`.
    fn close_all(&mut self, end: usize) {
        // A rule/begin line still pending at a boundary (e.g. EOF right after
        // it) must not fall out of the partition.
        self.flush_pending_into_current();
        match std::mem::replace(&mut self.state, State::Idle { raw_since: None }) {
            State::InCompilation { comp } => self.close_compilation(comp, end),
            State::Idle {
                raw_since: Some(start),
            } if start < end => {
                self.raw.push(RawSection {
                    lines: start..end,
                    label: self.raw_label.take().unwrap_or_default(),
                });
            }
            State::Idle { .. } => {}
        }
    }
}

fn label_from(line: &[u8]) -> String {
    let mut label = ansi::to_display_string(line);
    if label.len() > 96 {
        let cut = (0..=96)
            .rev()
            .find(|&i| label.is_char_boundary(i))
            .unwrap_or(0);
        label.truncate(cut);
        label.push('…');
    }
    label
}

/// Parses one `[…]` lifecycle line into an event, or `None` if it is not one
/// (programs can print bracketed lines of their own; never guess).
fn parse_event(text: &str) -> Option<EventKind> {
    let r = res();

    if text.starts_with("[marking ") {
        let c = r.ev_marking.captures(text)?;
        return Some(EventKind::Marking {
            name: c[1].to_string(),
            sfi: Addr::parse(&c[2]),
            target: Tier::parse(&c[3]),
            reason: c[4].to_string(),
        });
    }
    if text.starts_with("[compiling method ") {
        let c = r.ev_compile_start.captures(text)?;
        return Some(EventKind::CompileStart {
            name: c[1].to_string(),
            sfi: Addr::parse(&c[2]),
            target: Tier::parse(&c[3]),
            osr: c.get(4).is_some(),
        });
    }
    if text.starts_with("[completed compiling ") {
        let c = r.ev_compile_done.captures(text)?;
        return Some(EventKind::CompileDone {
            name: c[1].to_string(),
            sfi: Addr::parse(&c[2]),
            target: Tier::parse(&c[3]),
            osr: c.get(4).is_some(),
        });
    }
    if text.starts_with("[bailout (") {
        let c = r.ev_deopt_begin.captures(text)?;
        return Some(EventKind::DeoptBegin {
            kind: c[1].to_string(),
            reason: c[2].to_string(),
            name: c[3].to_string(),
            sfi: Addr::parse(&c[4]),
            tier: Tier::parse(&c[5]),
            opt_id: c[6].parse().ok(),
            bytecode_offset: c[7].parse().ok(),
        });
    }
    if text.starts_with("[bailout end") {
        let c = r.ev_deopt_end.captures(text)?;
        return Some(EventKind::DeoptEnd {
            invalidated: &c[1] == "invalidated",
        });
    }
    if text.starts_with("[OSR - ") {
        let c = r.ev_osr.captures(text)?;
        return Some(EventKind::Osr {
            what: c[1].to_string(),
            name: c[2].to_string(),
            osr_offset: c.get(3).and_then(|m| m.as_str().parse().ok()),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(text: &str) -> TraceIndex {
        let mut buffer = LogBuffer::new();
        buffer.append(text.as_bytes());
        buffer.finish();
        let mut idx = TraceIndex::new(None);
        idx.ingest(&buffer, true);
        idx
    }

    /// Sections must partition the line range: no gaps, no overlaps.
    fn assert_partition(idx: &TraceIndex, line_count: usize) {
        let mut ranges: Vec<(usize, usize)> = idx
            .compilations
            .iter()
            .map(|c| (c.lines.start, c.lines.end))
            .chain(idx.raw.iter().map(|r| (r.lines.start, r.lines.end)))
            .collect();
        ranges.sort();
        let mut at = 0;
        for (start, end) in ranges {
            assert_eq!(start, at, "gap or overlap at line {at}");
            assert!(end >= start);
            at = end;
        }
        assert_eq!(at, line_count, "sections do not cover the file");
    }

    const V152: &str = "\
V8 is running with developer-only features enabled.
---------------------------------------------------
Begin compiling method add using Maglev
Compiling 0x09b80101e371 <JSFunction add (sfi = 0x9b80101e2cd)> with Maglev
----- Bytecode array -----
Parameter count 3
----- Maglev graph building -----
Graph
   9: CheckedSmiUntag [n2], 1 uses
----- Register allocation -----
    9/9: CheckedSmiUntag [v5/n2:[x0|R|t]] \u{2192} [x0|R|w32]
---------------------------------------------------
Finished compiling method add using Maglev
";

    #[test]
    fn v152_shape_indexes_one_compilation() {
        let idx = index(V152);
        assert_eq!(idx.compilations.len(), 1);
        let c = &idx.compilations[0];
        assert_eq!(c.name, "add");
        assert_eq!(c.key.tier, Tier::Maglev);
        assert_eq!(c.key.ordinal, 1);
        // Starts at the rule line, ends after the Finished line.
        assert_eq!(c.lines, 1..13);
        assert_eq!(c.phases.len(), 3);
        assert_eq!(c.phases[0].kind, PhaseKind::Bytecode);
        assert_eq!(c.phases[1].kind, PhaseKind::Graph { known: true });
        // The trailing rule + Finished lines live in the last phase's range.
        assert_eq!(c.phases[2].lines, 9..13);
        assert_partition(&idx, 13);
        assert_eq!(idx.detected_version.as_deref(), Some("15.2"));
    }

    #[test]
    fn v149_shape_without_begin_finished() {
        let idx = index(
            "\
Compiling 0x1 <JSFunction a (sfi = 0x10)> with Maglev
----- After graph building -----
   1: Foo
Compiling 0x2 <JSFunction b (sfi = 0x20)> with Maglev
----- After graph building -----
   1: Bar
",
        );
        assert_eq!(idx.compilations.len(), 2);
        assert_eq!(idx.compilations[0].lines, 0..3);
        assert_eq!(idx.compilations[1].lines, 3..6);
        assert_partition(&idx, 6);
        assert_eq!(idx.detected_version.as_deref(), Some("14.9"));
    }

    #[test]
    fn toplevel_empty_name_parses() {
        let idx = index("Compiling 0x1 <JSFunction (sfi = 0x10)> with Maglev\n");
        assert_eq!(idx.compilations.len(), 1);
        assert_eq!(idx.compilations[0].name, "");
        assert_eq!(idx.compilations[0].display_name(), "<toplevel>");
    }

    #[test]
    fn same_key_gets_ordinals() {
        let idx = index(
            "\
Compiling 0x1 <JSFunction f (sfi = 0x10)> with Maglev
Compiling 0x1 <JSFunction f (sfi = 0x10)> with Maglev
",
        );
        assert_eq!(idx.compilations[0].key.ordinal, 1);
        assert_eq!(idx.compilations[1].key.ordinal, 2);
    }

    #[test]
    fn events_are_parsed_and_close_compilations() {
        let idx = index(
            "\
[marking 0x09b8 <JSFunction f (sfi = 0x10)> for optimization to MAGLEV, ConcurrencyMode::kConcurrent, reason: hot and stable]
Compiling 0x1 <JSFunction f (sfi = 0x10)> with Maglev
----- Maglev graph building -----
   1: Foo
[bailout (kind: deopt-eager, reason: not a Smi): begin. deoptimizing 0x09b8 <JSFunction f (sfi = 0x10)>, 0x031a <Code MAGLEV>, opt id 1, bytecode offset 2, deopt exit 0, FP to SP delta 32, caller SP 0x0001, pc 0x0002]
trailing program output
",
        );
        assert_eq!(idx.events.len(), 2);
        assert!(matches!(
            idx.events[0].kind,
            EventKind::Marking { ref name, .. } if name == "f"
        ));
        let EventKind::DeoptBegin {
            ref reason,
            bytecode_offset,
            ..
        } = idx.events[1].kind
        else {
            panic!("expected deopt event");
        };
        assert_eq!(reason, "not a Smi");
        assert_eq!(bytecode_offset, Some(2));
        // The bailout closed the compilation; the event line + trailing output
        // are raw.
        assert_eq!(idx.compilations[0].lines, 1..4);
        assert_partition(&idx, 6);
    }

    #[test]
    fn osr_flag_binds_by_sfi_and_tier() {
        let idx = index(
            "\
[OSR - compilation started. function: hot, osr offset: 28, mode: ConcurrencyMode::kSynchronous]
[compiling method 0x1 <JSFunction hot (sfi = 0x10)> (target MAGLEV) OSR, mode: ConcurrencyMode::kSynchronous]
Compiling 0x1 <JSFunction hot (sfi = 0x10)> with Maglev
----- Maglev graph building -----
",
        );
        let osr = idx.compilations[0].osr.as_ref().expect("osr flagged");
        assert_eq!(osr.offset, Some(28));
        assert_eq!(osr.confidence, Confidence::Heuristic);
    }

    #[test]
    fn non_event_bracket_lines_are_content() {
        let idx = index("[object Object]\n[1, 2, 3]\n");
        assert!(idx.events.is_empty());
        assert_eq!(idx.raw.len(), 1);
        assert_eq!(idx.raw[0].label, "[object Object]");
    }

    #[test]
    fn function_filter_marks_but_keeps_sections() {
        let mut buffer = LogBuffer::new();
        buffer.append(
            b"Compiling 0x1 <JSFunction keepme (sfi = 0x10)> with Maglev
Compiling 0x2 <JSFunction dropme (sfi = 0x20)> with Maglev
",
        );
        buffer.finish();
        let mut idx = TraceIndex::new(Some(Regex::new("^keep").unwrap()));
        idx.ingest(&buffer, true);
        assert!(!idx.compilations[0].filtered_out);
        assert!(idx.compilations[1].filtered_out);
        assert_partition(&idx, 2);
    }

    #[test]
    fn streaming_holds_back_the_partial_line() {
        let mut buffer = LogBuffer::new();
        let mut idx = TraceIndex::new(None);
        buffer.append(b"Compiling 0x1 <JSFunction f (sfi = 0x10)> with Mag");
        idx.ingest(&buffer, false);
        assert_eq!(idx.compilations.len(), 0);
        buffer.append(b"lev\n----- Maglev graph building -----\n");
        idx.ingest(&buffer, false);
        assert_eq!(idx.compilations.len(), 1);
        buffer.finish();
        idx.ingest(&buffer, true);
        assert_eq!(idx.compilations[0].phases.len(), 1);
    }

    #[test]
    fn ansi_wrapped_boundaries_still_match() {
        let idx =
            index("\u{1b}[0;32mCompiling 0x1 <JSFunction f (sfi = 0x10)> with Maglev\u{1b}[0m\n");
        assert_eq!(idx.compilations.len(), 1);
    }

    #[test]
    fn garbage_never_panics_and_partitions() {
        let noise = "-----\n---- x ----\nCompiling gibberish\n[bailout mangled\n\u{fffd}\u{1b}[9\n";
        let idx = index(noise);
        assert_partition(&idx, 5);
    }
}
