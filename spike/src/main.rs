//! Throwaway parse spike (TODO 0.3).
//!
//! Splits a d8 trace into compilations -> phases -> blocks and prints the tree,
//! then reports every line it could *not* classify, grouped by normalized shape.
//! That second report is the actual deliverable: it is how we find out what the
//! real grammar is before committing to a parser design.
//!
//!   cargo run -- ../fixtures/arm64-v15.2.0/maglev-graphs.mixed.log
//!   cargo run -- --shapes ../fixtures/*/*.log

use std::collections::BTreeMap;
use std::fs;

#[derive(Debug)]
struct Phase {
    name: String,
    line: usize,
    blocks: usize,
    nodes: usize,
    annotations: usize,
}

#[derive(Debug)]
struct Compilation {
    name: String,
    tier: String,
    line: usize,
    phases: Vec<Phase>,
    /// Lines seen inside the compilation but before any phase banner.
    preamble_annotations: usize,
}

/// d8 colorizes graph output even when stdout is a pipe, so stripping SGR
/// sequences is step zero for any line-shape decision.
fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b && i + 1 < b.len() && b[i + 1] == b'[' {
            i += 2;
            while i < b.len() && !b[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1; // final byte
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// "  12: Float64Add [n9, n11], 2 uses" -> Some(12)
/// Also matches the post-AnyUseMarking form "  11: Int32Add [v0/n9:(x)] -> (x)"
/// and the post-scheduling form "  11/6: InitialValue(a0) -> v-1, live range: ..."
/// where the id becomes <schedule-position>/<node-id>.
fn node_id(line: &str) -> Option<u32> {
    let t = line.trim_start();
    let (num, rest) = t.split_at(t.find(|c: char| !c.is_ascii_digit())?);
    if num.is_empty() {
        return None;
    }
    // Bytecode lines look like "   0 : 0b 04   Ldar a1" -- space before the
    // colon -- so requiring "N: " (no space) separates IR nodes from bytecode.
    if rest.starts_with(": ") {
        return num.parse().ok();
    }
    let second = rest.strip_prefix('/')?;
    let (num2, rest2) = second.split_at(second.find(|c: char| !c.is_ascii_digit())?);
    (!num2.is_empty() && rest2.starts_with(": ")).then(|| num2.parse().ok())?
}

/// "Compiling 0x09b8.. <JSFunction add (sfi = 0x9b8..)> with Maglev"
///  -> Some(("add", "Maglev")).
///
/// This line, unlike the "Begin compiling method" banner, exists in both V8
/// 14.9 and 15.2 -- see docs/spike-findings.md. It is the compilation anchor.
fn compiling_with(line: &str) -> Option<(String, String)> {
    let rest = line.trim_end().strip_prefix("Compiling ")?;
    let idx = rest.rfind(" with ")?;
    let (subject, tier) = (&rest[..idx], &rest[idx + 6..]);
    let name = subject
        .find("<JSFunction ")
        .map(|i| {
            let after = &subject[i + 12..];
            after
                .find(" (sfi")
                .map(|j| after[..j].to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    Some((name, tier.to_string()))
}

fn block_label(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t.strip_prefix("Block ")?;
    let label = rest.split_whitespace().next()?;
    label.starts_with('b').then_some(label)
}

/// "----- Phi untagging -----" -> Some("Phi untagging").
/// TurboFan banners carry a trailing space ("----- Graph after V8.TFTyper ----- ")
/// which is exactly the kind of thing a naive `strip_suffix` would miss.
fn phase_banner(line: &str) -> Option<String> {
    let t = line.trim_end();
    let inner = t.strip_prefix("----- ")?.strip_suffix(" -----")?;
    (!inner.is_empty()).then(|| inner.to_string())
}

/// "Begin compiling method foo using Maglev" -> Some(("foo", "Maglev")).
/// The toplevel script function has an empty name, so this yields ("", "Maglev")
/// -- the parser must not treat an empty name as a failure.
fn begin_compiling(line: &str) -> Option<(String, String)> {
    let rest = line.trim_end().strip_prefix("Begin compiling method ")?;
    let idx = rest.rfind(" using ")?;
    Some((rest[..idx].to_string(), rest[idx + 7..].to_string()))
}

fn is_finished_compiling(line: &str) -> bool {
    line.starts_with("Finished compiling method ")
}

/// Free-form pass-trace lines, e.g. "[ML:38564] INLINE small ...".
fn trace_channel(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let end = rest.find(']')?;
    let tag = &rest[..end];
    let name = tag.split(':').next()?;
    (!name.is_empty()
        && name.len() <= 4
        && name.chars().all(|c| c.is_ascii_uppercase()))
    .then_some(name)
}

/// Timeline events: "[marking ...]", "[bailout ...]", "[OSR - ...]", ...
fn timeline_event(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let word = rest.split(|c: char| !c.is_ascii_alphabetic()).next()?;
    (!word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_uppercase()))
        .then_some(word)
}

/// Collapse a line to a shape so unclassified lines can be counted by kind.
fn shape(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    let mut last_class = ' ';
    while let Some(c) = chars.next() {
        let class = if c == '0' && chars.peek() == Some(&'x') {
            chars.next();
            while chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                chars.next();
            }
            'H'
        } else if c.is_ascii_digit() {
            while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                chars.next();
            }
            '#'
        } else if c.is_whitespace() {
            ' '
        } else {
            c
        };
        if class == ' ' && last_class == ' ' {
            continue;
        }
        out.push(class);
        last_class = class;
    }
    out.trim_end().chars().take(90).collect()
}

struct Stats {
    compilations: Vec<Compilation>,
    timeline: BTreeMap<String, usize>,
    channels: BTreeMap<String, usize>,
    /// Lines outside any compilation: the Tier-B raw fallback.
    orphan: usize,
    unclassified: BTreeMap<String, usize>,
}

fn parse(text: &str) -> Stats {
    let mut st = Stats {
        compilations: Vec::new(),
        timeline: BTreeMap::new(),
        channels: BTreeMap::new(),
        orphan: 0,
        unclassified: BTreeMap::new(),
    };
    let mut open = false;

    for (i, raw) in text.lines().enumerate() {
        let line = strip_ansi(raw);
        let line = line.as_str();
        let lineno = i + 1;

        if let Some((name, tier)) = compiling_with(line) {
            st.compilations.push(Compilation {
                name,
                tier,
                line: lineno,
                phases: Vec::new(),
                preamble_annotations: 0,
            });
            open = true;
            continue;
        }
        // Present from V8 15.2 only; carries the display name for the toplevel
        // script function, which the anchor line renders as "<JSFunction (sfi=..)>".
        if begin_compiling(line).is_some() || is_finished_compiling(line) {
            continue;
        }
        if let Some(name) = phase_banner(line) {
            if let Some(c) = st.compilations.last_mut() {
                c.phases.push(Phase {
                    name,
                    line: lineno,
                    blocks: 0,
                    nodes: 0,
                    annotations: 0,
                });
                continue;
            }
        }
        if let Some(ch) = trace_channel(line) {
            *st.channels.entry(ch.to_string()).or_default() += 1;
            match st.compilations.last_mut().filter(|_| open) {
                Some(c) => match c.phases.last_mut() {
                    Some(p) => p.annotations += 1,
                    None => c.preamble_annotations += 1,
                },
                None => st.orphan += 1,
            }
            continue;
        }
        if line.starts_with('[') {
            if let Some(ev) = timeline_event(line) {
                *st.timeline.entry(ev.to_string()).or_default() += 1;
                continue;
            }
        }

        let inner = st.compilations.last_mut().filter(|_| open);
        let Some(c) = inner else {
            if !line.trim().is_empty() {
                st.orphan += 1;
            }
            continue;
        };
        let Some(p) = c.phases.last_mut() else {
            if !line.trim().is_empty() {
                c.preamble_annotations += 1;
            }
            continue;
        };
        if block_label(line).is_some() {
            p.blocks += 1;
        } else if node_id(line).is_some() {
            p.nodes += 1;
        } else if !line.trim().is_empty() {
            p.annotations += 1;
            *st.unclassified.entry(shape(line)).or_default() += 1;
        }
    }
    st
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let shapes_only = args.iter().any(|a| a == "--shapes");
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if files.is_empty() {
        eprintln!("usage: spike [--shapes] <trace.log>...");
        std::process::exit(2);
    }

    let mut all_shapes: BTreeMap<String, usize> = BTreeMap::new();

    for path in &files {
        let text = match fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                eprintln!("{path}: {e}");
                continue;
            }
        };
        let st = parse(&text);
        for (k, v) in &st.unclassified {
            *all_shapes.entry(k.clone()).or_default() += v;
        }
        if shapes_only {
            continue;
        }

        println!("=== {path}");
        println!(
            "  {} compilations, {} orphan lines, timeline {:?}, channels {:?}",
            st.compilations.len(),
            st.orphan,
            st.timeline,
            st.channels
        );
        for (n, c) in st.compilations.iter().enumerate() {
            let name = if c.name.is_empty() { "<toplevel>" } else { &c.name };
            println!(
                "  [{}] {} [{}] @L{}{}",
                n + 1,
                name,
                c.tier,
                c.line,
                if c.preamble_annotations > 0 {
                    format!("  (+{} preamble annotations)", c.preamble_annotations)
                } else {
                    String::new()
                }
            );
            for p in &c.phases {
                println!(
                    "        {:<44} L{:<6} {:>3} blocks {:>5} nodes{}",
                    p.name,
                    p.line,
                    p.blocks,
                    p.nodes,
                    if p.annotations > 0 {
                        format!("  {:>4} unstructured", p.annotations)
                    } else {
                        String::new()
                    }
                );
            }
        }
        println!();
    }

    println!("=== unclassified line shapes (top 30 of {})", all_shapes.len());
    let mut v: Vec<_> = all_shapes.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (sh, n) in v.into_iter().take(30) {
        println!("  {n:>7}  {sh}");
    }
}
