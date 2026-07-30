# `garage` — Implementation Roadmap & Task List

Task breakdown for `garage` (see [PLAN.md](PLAN.md)). Resequenced from v1:
**de-risking comes first** (the parsers are the project risk, so fixtures and a
parse spike precede any TUI work), and the **MVP is cut at Phase 5** instead of
spanning every feature in the plan.

---

## Phase 0: De-Risking & Fixture Corpus  ✅ done

- [x] **0.1. Collect fixture corpus (checked into repo)** — 69 fixtures,
  23 flag/workload combinations × 3 builds, ~9 MB. See
  [fixtures/README.md](fixtures/README.md).
  - Real `d8` outputs for every flag in PLAN §4 that exists (see 0.2).
  - Three builds: `arm64-v15.2.0` (tip of tree), `arm64-v14.9.0` (version axis),
    `x64-v14.9.0` (architecture axis) — deliberately separating the two axes.
    *Deviation from the original plan:* the second version is an older local
    build rather than a stable branch, because it was available and gives the
    same de-risking value. It paid off immediately (see 0.3).
  - `concurrent.mixed.log` has concurrent recompilation on; everything else uses
    `--no-concurrent-recompilation --predictable`.
  - Annotation-attachment fixtures use `--trace-maglev-inlining`, not
    `--trace-maglev-truncation` — see 0.3.
  - Multi-hundred-MB trace: [`tools/gen-large-trace.sh`](tools/gen-large-trace.sh),
    git-ignored.
- [x] **0.2. Verify flag inventory** — two entries in PLAN §4 were wrong:
  `--trace-ic` does not exist (IC transitions go to `v8.log` via `--log-ic`) and
  `--trace-prototypes` is `--trace-prototype-users`. PLAN.md corrected; details
  in [docs/printer-parser-contract.md](docs/printer-parser-contract.md) §2.
- [x] **0.3. Throwaway parse spike** — [`spike/`](spike/). Findings in
  [docs/spike-findings.md](docs/spike-findings.md); the ones that change the
  design: output is ANSI-colored even when piped; every Maglev phase banner was
  renamed between 14.9 and 15.2 (so the marker table needs a version axis, and
  the compilation anchor must be the `Compiling … with <Tier>` line);
  node-line syntax varies per phase; deopt-frame lines are structure, not
  annotations; `--trace-maglev-truncation` emits nothing.
- [x] **0.4. Printer↔parser contract note** —
  [docs/printer-parser-contract.md](docs/printer-parser-contract.md), plus
  [`tools/gen-fixtures.sh`](tools/gen-fixtures.sh) which regenerates the corpus
  from any local d8 build and records command line, sha256 and measured
  reproducibility per file.
- [x] **0.5. Correlation-key spec** —
  [docs/correlation-keys.md](docs/correlation-keys.md). `opt id` and the Code
  address are printed on the deopt side only, so the key is
  `(SFI address, tier, ordinal)` + `bytecode offset`, with explicit confidence
  levels and a no-guessing rule.

### Carried into Phase 2 from Phase 0

- [x] **0.6. Version-keyed marker TOML** — done in 2.2
  ([`assets/markers.toml`](assets/markers.toml)).
- [x] **0.7. Golden-test harness** — done in 2.7, with one deliberate deviation
  from "key off the `reproducible` flag"; see the Phase 2 notes.

---

## Phase 1: Project Skeleton & Terminal Infrastructure  ✅ done

- [x] **1.1. Crate setup** — binary **plus library** (`src/lib.rs`): the Phase 2
  parsers and the golden suite (2.7) need the parsing half without a terminal.
  MVP deps as planned, plus `tracing`/`tracing-subscriber` (1.2) and `libc`
  (1.5). Release profile: fat LTO, one codegen unit, and line tables *kept* —
  "a malformed trace must never panic" is a promise we will occasionally break,
  and a usable backtrace in the bug report is worth a few hundred KB. The
  Phase 0 spike is `exclude`d from the workspace so it stays dependency-free.
- [x] **1.2. File-based internal logging** — `tracing` → `garage.log`, installed
  only when asked for (`--debug`, or `GARAGE_LOG=<filter>`), so an ordinary run
  does not drop a log file into whatever directory the user was in.
  `--log-file` overrides the path.
- [x] **1.3. CLI parsing** — `garage [FILE...]`, `garage -- <cmd>...`,
  `--config`, `--debug`, `--function <regex>`, `--log-file`. argv is split at
  the first `--` *before* clap sees it; clap would otherwise merge the wrapped
  command into the file list and `garage -- d8 x.js` would be
  indistinguishable from `garage d8 x.js`. `--function` is compiled at parse
  time, so a bad regex fails on the command line rather than after the
  alternate screen is up. Wrapper mode parses and is rejected with a pointer
  to 9.4.
- [x] **1.4. Input abstraction** — `LogSource::{File, Stdin}`, one detached
  reader thread each, all feeding one `mpsc` channel; no `tokio`. Files are
  mmapped (no copy, no read loop); stdin streams in 64 KiB chunks. `LogBuffer`
  keeps bytes **verbatim**, escapes and all — the raw view needs d8's own
  colours and the parser needs offsets that still match the file — and carries
  an incrementally built line index. Threads are detached on purpose: joining
  one blocked on a pipe that never closes is exactly the hang to avoid on quit.
- [x] **1.5. `/dev/tty` keyboard input** — works, but not the way the plan
  assumed. See below.
- [x] **1.6. Terminal lifecycle** — raw mode, alt screen, event-driven redraw
  (blocking `recv`, batched drain, no tick), restore on clean exit, on error,
  and from the panic hook. The hook restores *before* delegating to the default
  one, otherwise the panic message is printed into the alt screen and vanishes
  with it. Mouse capture is deliberately **off**: it would take over the
  terminal's own selection and break copy-paste.

### What 1.5 actually took

crossterm does fall back to `/dev/tty` when stdin is a pipe — but on macOS that
descriptor **cannot be registered with `kqueue`** (`EINVAL`), so its event
source fails to initialise and every keystroke is silently dropped. Measured in
tmux:

| descriptor | `mio` register |
| :-- | :-- |
| fd 0, a pty slave | OK |
| `open("/dev/tty")` | `EINVAL` ← crossterm's fallback |
| `open("/dev/ttys003")`, the *same* terminal | OK |

`/dev/tty` is a clone device, `ttyname()` on such a descriptor answers
`/dev/tty` (so the real path cannot be recovered from it), and `dup2`-ing it
onto fd 0 does not help either — the rejection follows the open file
description, not the descriptor number. The fix in [`src/tty.rs`](src/tty.rs):
`ttyname(1)`/`ttyname(2)` still name the real device even when stdin is a pipe,
so open *that*, and `dup2` it onto fd 0 after moving the trace pipe out of the
way. crossterm's own `isatty(0)` check then takes the path that works, with no
patching of crossterm.

Two consequences worth keeping:

- **Startup verifies the keyboard** with a zero-length `poll`. crossterm builds
  its event source once, caches it, and surfaces failure only as an error from
  a later `poll` — so without the check an unusable terminal yields a UI that
  paints once and then ignores every key.
- **No `Terminal::clear()` on startup.** It snapshots the cursor position first,
  which crossterm implements as an `ESC[6n` round-trip whose retry loop has
  neither backoff nor an attempt limit. A dead event source *or* a redirected
  stdout (the query goes into the file; nothing answers) turns it into a 100%
  CPU hang. Entering the alternate screen already gives a blank screen.

Verified in tmux: file and piped sources, scrolling, resize, redirected stdout
(0 bytes reached the redirect), missing file (non-fatal, `failed` badge, other
sources still usable), binary input (`/bin/ls`, no panic), empty stream, the
terminal destroyed underneath the process, and `q` while an endless producer is
still writing. Panic restore checked with a temporary panic: message lands on
the real screen, shell still usable, exit 101, and the panic is in the log.

**Not verified: over SSH, and on Linux.** The fix should be a no-op on Linux —
`/dev/tty` plus `epoll` works there — but that is reasoning, not a measurement.
Folded into 5.4.

Measured on the way: indexing a **489 MiB** trace takes **212 ms** (6.85 M
lines, ~2.4 GB/s), so the naive newline scan is far inside the PLAN §12 budget
and `memchr` is not needed yet. The line index costs 8 bytes per line — 55 MB
for that file, ~11% of its size — which is the first thing to attack in 5.1 if
memory becomes the binding constraint.

---

## Phase 2: Data Model, Indexer & Maglev Parser (only)  ✅ done

Notes on what the fixtures taught beyond the spike, and the deviations:

- **Deopt frames print asymmetrically**: eager frames come *before* the node
  they belong to, lazy/throw frames *after* it. The spike said "attach to the
  preceding node" (§6); that is only true for half of them. The parser patches
  eager-frame attachment forward when the node line arrives.
- **Inlining-trace lines are not confined to the preamble.** They print
  *between* the `Bytecode array` / `Inlining …` banners too, so the decision
  scanner runs over every unmatched line, not just pre-phase ones. An
  invariant test locks in that `[ML:<id>]`-prefixed and prefix-free runs yield
  identical decisions (2.8).
- **`▼`/`▲` are gutter glyphs** the spike's box-drawing list missed, and
  `with gap moves:` is a phi-edge sub-header that classifies as an annotation
  (visible in goldens as the constant per-phase annotation counts).
- **Golden coverage is wider than planned** (0.7 said "key off the
  `reproducible` flag"): the summary is a canonicalized projection — no
  timings, no host pointers, hex/decimal masking in raw labels — so all 27
  fixtures per build except `concurrent.*` are goldens, including the
  timing-volatile `mvp.*`/`trace-opt+deopt.*`/`trace-osr.*` ones. That is what
  gives the 2.5 event parser golden coverage at all; spike §10 ("run through
  the canonicalizer rather than trusting the flag") is the justification.
  83 goldens total; `UPDATE_GOLDENS=1 cargo test --test golden` regenerates.
- **Open-path cost measured** (release, M-series): 489.5 MiB / 6.85 M lines =
  214 ms line index + 328 ms section index ≈ **550 ms**, ~4× inside the §12
  budget, with 7 000 compilations found. ANSI-stripping allocations on
  escape-bearing lines did not matter at this scale.
- The event-line binder records `(sfi, tier, ordinal)` per compilation and the
  OSR flag/offset with explicit [`Confidence`], per docs/correlation-keys.md —
  the deopt→graph *jump* stays Phase 6.

- [x] **2.1. Core types**: `CompilationKey` (**SFI address** — the correlation
  key, and the only stable identity when the function name is empty — function
  name, script, line, tier, compilation index, **OSR offset: Option**), `Tier`
  enum (incl. distinguishing OSR), `FunctionCompilation`, `Phase`, `BasicBlock`,
  `IRNode`, `TimelineEvent`, `RawSection`.
  - `IRNode` must separate **identity** (`nN`, stable across phases — this is
    what makes the phase diff work without Turbolizer JSON) from **per-phase
    rendering** (`N/M:` ids, `v0/n9:(x)` input decoration, registers, live
    ranges, `→ (x)` vs `, N uses`). Phase 0.3 §5.
  - Deopt frames (`↱ eager @2 (5 live vars)`, `│` depth = inlining depth) are a
    node-attached structural type, *not* annotations. Phase 0.3 §6.
  - `DeoptFrame` is its own **interned** type with identity, referenced by nodes
    — not a per-node string. Under `--print-maglev-deopt-verbose` frames carry a
    `(addr:0x…)` host pointer; frames are heavily shared (215 rendered lines →
    39 distinct frames), so interning on it collapses the display and recovers
    sharing structure nothing else exposes. Phase 0.3 §12.
  - The frame payload has **three** syntaxes, flag- and phase-dependent:
    `(N live vars)` / `{reg:node:loc, …}` / `{reg:node}` for `↳ throw`; the
    `loc` field varies by phase exactly as node lines do (empty → `(x)` →
    `[stack:-6|t]`), and may be *empty*, not absent. Parse `↳ throw` as its own
    arm — its `bN` catch-block id is the only place the exception edge appears.
    Phase 0.3 §12.
  - `RawSection` keeps both the original byte range (ANSI preserved, for the raw
    view) and a stripped view (for matching). d8 colorizes piped output.
- [x] **2.2. Section indexer**: single streaming pass that finds section
  boundaries (table-driven markers, embedded TOML) and records byte ranges —
  **without parsing bodies**. This is what makes 1 GB files open instantly.
  - Anchor compilations on `Compiling 0x… <JSFunction …> with <Tier>`, not on
    the `Begin compiling method` banner, which does not exist before V8 15.2.
  - Marker table is **version-keyed** (0.6). Not arch-keyed: architecture only
    affects register names inside node lines.
  - `----- Bytecode array -----` and `----- Inlining 0x… with bytecode -----`
    share the phase-banner grammar but are not phases; match them specially.
- [x] **2.3. Lazy per-compilation parsing** on first view; parsed results cached.
- [x] **2.4. Maglev graph parser**: blocks (`b0:`, loop headers), node defs
  (`n10 = Int32Add n8, n9`), inputs/users, register allocation output, bytecode
  offsets, type feedback.
- [x] **2.5. Deopt/opt line parser** (`--trace-opt`, `--trace-deopt`) into
  `TimelineEvent`s (timeline *UI* comes later; the data is cheap to keep now).
- [x] **2.6. Fallback raw sections** for anything unmatched, with `[unparsed]`
  status; malformed input must never panic (fuzz test, see 5.4).
- [x] **2.7. Golden-file tests** over the Phase 0 corpus for everything above.
  - Key off `manifest.json`'s `"reproducible"` flag (0.7). Note the
    `+deoptverbose` fixtures are `false` **only** because of the `(addr:0x…)`
    host pointer — masking it makes them exact, so they are still usable as
    goldens through the canonicalizer. Phase 0.3 §12.
- [x] **2.8. Annotation attachment rule (minimal)** (PLAN §6.1): unmatched lines
  inside an open compilation attach to the enclosing phase/transition as
  positioned annotations — never dropped, never exiled to an orphan raw
  section. Must handle "inside a compilation, no phase open yet": inlining
  traces print before the first phase banner. Golden-test against
  `maglev-graphs+inlining.inlining.log` (no ids) **and**
  `maglev-graphs+inlining-ids.inlining.log` (with `[ML:<id>]` prefixes) — the
  prefix is absent whenever `--no-trace-with-compilation-id` is used, so it
  cannot be load-bearing. (Channels, prefix maps, and node-ID linking: icebox.)

---

## Phase 3: TUI Layout & Navigation  ✅ done

- [x] **3.1. Layout grid**: telemetry bar / sidebar + viewport / status line.
- [x] **3.2. Telemetry bar**: compilations per tier, deopt count, OSR count,
  source status, detected V8 version (from the marker-table votes).
- [x] **3.3. Sidebar**: chronological ⇄ grouped-by-function toggle, OSR and
  `[unparsed]` badges, selection highlight, scrollbar. Grouped mode keys on the
  SFI address (names can be empty/duplicated) and collapses groups by default —
  that is the mode that scales to thousands of compilations. Raw sections are
  interleaved chronologically and never hidden.
- [x] **3.4. Viewport**: vertical scroll with a cursor line (Phase 4 needs the
  cursor anyway), horizontal scroll, `PgUp`/`PgDn`/`Home`/`End`, wrap toggle,
  line numbers.
- [x] **3.5. Keybindings** per PLAN §8, remappable via `[keys]` in
  `~/.config/garage/config.toml` (or `--config`). Remapping an action *frees*
  its default chords; a chord bound to two actions is a startup error. Config
  parses before the alt screen, so a typo is an error message, not a broken
  TUI.
- [x] **3.6. Help modal** (`?`) generated from the live keymap, so remaps show
  up in it automatically.

Model and deviations worth recording:

- **Selection is the view**: moving the sidebar selection immediately points
  the viewport at that section; `Enter` only expands/collapses (compilations →
  phases, function groups → compilations). There is no "open" step.
- **Keys beyond the PLAN §8 fixed map** (all visible in `?`): `c` grouping
  toggle, `w` wrap, `<`/`>` horizontal scroll, `F` follow, and `Tab` = next
  source *for now* — §8 gives Tab to the timeline, which is Phase 6; the
  binding moves there when the timeline exists.
- Two live-stream UX traps found in tmux and fixed: sidebar navigation now
  breaks follow (otherwise the next chunk yanks the selection back to the
  newest section), and any key closes the help modal — `q` used to quit the
  whole app from inside it.

---

## Phase 4: Syntax Highlighting & Node Interactivity  ✅ done

- [x] **4.1. IR tokenizer** → styled spans. Not a second tokenizer: the Phase 2
  parse already records opcode/id/input/target spans per line, and rendering
  paints those (a per-byte class array, layered base → def-use → search, then
  run-length encoded). Opcode classes match on name *shape* (`Check*`/`*Deopt*`
  guards, `Jump/Branch/Return` control, `*Constant*`, `φ*`, `Call*`) because
  the vocabulary is open. 256-colour palette with named-ANSI fallback via
  TERM/COLORTERM. *Deviation:* the planned register-vocabulary arch detection
  dissolved — span-based styling never enumerates register names, so there is
  nothing arch-dependent left to detect.
- [x] **4.2. Basic block folding** (`Space` anywhere in the block), state
  persisted per `(source, compilation, phase, block)`, `[+] b1 — 14 lines
  hidden` markers. This is why the cursor became a *display row*: folded views
  are not contiguous line ranges. Fold state survives navigating away.
- [x] **4.3. Cursor node tracking**: the status line shows
  `n11 Int32AddWithOverflow · 2 in · 1 uses` for the node defined under the
  cursor.
- [x] **4.4. Def-use / use-def highlighting** from the parsed def/use maps:
  the cursor node's definition (green), its inputs' definitions (blue), and
  every reference to it (yellow) — including references inside verbose deopt
  frames and phi moves.
- [x] **4.5. Node jumps**: `i` cycles through input definitions, `u` cycles
  consumers — anchored to the node the cycle *started* from, so `u u` visits
  the second consumer instead of the first consumer's consumers.
  `Ctrl+O`/`Ctrl+I` history. A jump into a folded block unfolds it.
- [x] **4.6. Search**: `/` prompt with incremental highlight while typing,
  `n`/`N` with wrap-around; works in parsed and raw sections (matching runs on
  the stripped text either way).
- [x] **4.7. Sidebar quick filter** (`f`): regex over function names and raw
  labels; empty input clears. Filters compilations, groups, and raw rows.
- [x] **4.8. Annotation rendering (minimal)**: dimmed italic; runs collapse to
  `[+] N trace lines (t to show)`; `t` toggles inline. Deopt frames are never
  folded as annotations — they are structure (spike-findings.md §6).

---

## Phase 5: MVP Hardening & Ship  🏁

- [x] **5.1. Perf pass**, measured on a 489.5 MiB / 6.85 M-line trace
  (release build, M-series):
  - open: 214 ms line index + 328 ms section index ≈ **550 ms** (budget: 2 s);
  - cursor latency: a **slow-frame detector** now logs any draw ≥ 16 ms; a
    navigation session over the big trace (jump-to-end, repeated half-pages,
    expansion, phase views, lazy parses) recorded **zero** slow frames;
  - memory: max RSS 593 MB ≈ the mmapped file + the 55 MB line index + ~50 MB
    of everything else — no full-file copies. The 8-bytes/line index remains
    the first target if memory ever binds.
  - Grouped-sidebar row building was the one accidental quadratic found
    (per-SFI membership scans); now linear.
- [x] **5.2. Clipboard** — `y` (cursor line) / `Y` (visible section). Inside
  tmux/SSH: OSC 52, with the tmux passthrough wrapper, since that is the only
  route to the *local* clipboard; locally: `arboard`, falling back to OSC 52.
  Payloads over 72 KB are refused with a pointer to export — terminals cap
  OSC 52 and truncate silently.
- [x] **5.3. Export** — `E` prompts for a filename and writes the current view
  (folds rendered as their markers, exactly as on screen) as a fenced Markdown
  block with a provenance title. *Deviation:* bound to keys (`y`/`Y`/`E`)
  rather than `:copy`/`:export` — the `:` command palette is Phase 6.1, and
  holding the clipboard hostage to it helped nobody.
- [x] **5.4. Robustness** (the part reachable from this machine):
  - always-on bounded fuzz in the test suite: 200 seeded rounds of byte flips,
    truncations, boundary splices, and slice duplications over real fixture
    data, plus pure-noise and truncation tests — indexer must produce a valid
    partition and the parser must not panic, every round;
  - terminal matrix: 80×24 with `TERM=xterm` (16-colour), 40×10, 200×50,
    resize mid-run, tmux throughout.
  - [ ] **Still owed: Linux and SSH** for the fd-0 terminal handling from 1.5
    — including the case where neither stdout nor stderr is a terminal and
    `/dev/tty` is the only remaining route. Needs hardware this session does
    not have; the reasoning says Linux is a no-op (`/dev/tty` + epoll works),
    but that is reasoning, not measurement.
- [x] **5.5. README** — install, sample invocations, keymap, clean-trace flag
  guidance, the `V8_ENABLE_MAGLEV_GRAPH_PRINTER` / `v8_enable_disassembler`
  build gates, and the flag-implication trap.
- [ ] **5.6. Dogfood milestone**: author uses `garage` instead of `less` for one
  week of real Maglev work; fix what hurts. **MVP done = you don't go back.**
  (A user milestone by definition — everything mechanical above is done.)

---

## MVP review pass  ✅ done

Five independent reviews, one per phase commit, each instructed to verify
findings against HEAD before reporting. 20 findings survived their
verification (0 critical / 6 major / 14 minor); all are resolved in the
review-fixes commit. The majors, for the record:

- **tmux clipboard silently dead on tmux ≥ 3.3**: OSC 52 was always wrapped in
  the DCS passthrough, which `allow-passthrough` (default *off* since 3.3)
  discards. Now: plain OSC 52 (handled by tmux's default `set-clipboard
  external`), passthrough only when tmux confirms it is enabled.
- **`Y` on a huge raw section** materialised ~1 GB of Strings to earn a
  guaranteed 72 KB refusal; the size is now checked in O(1) from the line
  index first.
- **Partition invariant hole**: an event line closing a compilation with
  pending rule/`Begin` lines orphaned those lines from every section
  (regression test added).
- **Follow in grouped mode** pinned to the last row, which grouped ordering
  makes a stale section; grouping now breaks follow, and the newest-section
  pin only runs in chronological mode.
- **`SCHEDULE_ONLY` leak**: every GapMove line shares the sentinel id, so the
  cursor on one highlighted them all and the status line read `n4294967295`;
  the sentinel now means "no node" everywhere it is consumed.
- **`i` could never reach the second input** — the cycle re-anchored on the
  node it had just jumped to; it now stays anchored like `u`, and jump history
  stores buffer lines instead of display rows so Ctrl+O survives unfolds.

Also fixed from the minors: help modal now closes on genuinely any key;
sidebar selection re-locates by identity when grouped rows shift mid-stream;
`n<digits>` inside `<…>` descriptors / quoted strings is no longer a node ref
(minified-JS names like `n1` corrupted def/use maps); duplicate chords within
one action no longer a fatal "conflict"; unreadable default config errors
instead of silently vanishing; jumps into folded blocks no longer flip the
global annotation toggle; phase selection works on oversized compilations;
search clears the `u` cycle and stopped allocating per line; `--help` no
longer claims `/dev/tty`; the `line_starts` comment states the real streaming
invariant; the dead `unparsed_lines` field is gone (48 goldens regenerated);
the golden mask covers `[pid:isolate]` prefixes.

---

## Phase 6 (post-MVP): Command Palette & Timeline  ✅ done

- [x] **6.1. `:` palette** with completion and message line. Tab completes to
  the longest common prefix; the status line doubles as the message line and
  shows live candidates (or the unique match's description) while typing. The
  command table is one data structure so completion, dispatch and the help
  modal cannot drift.
- [x] **6.2. Semantic filters**: `:deopts`, `:checks` (with count), `:spill`,
  `:phi`, `:megamorphic`, `:function <name>`, plus `:copy`/`:export`/`:clear`/
  `:timeline`. *Deviation:* `:checks` is a **lens** (filter + count), not
  highlight-only as PLAN §7.2 sketched — guards are already red by default,
  so the added value is the count and the narrowed view. A lens keeps the
  banner/block-header skeleton plus matching rows; `:checks`/`:phi` match on
  parsed opcode shape (through the same predicates the styling uses, so the
  count and the colours cannot disagree), `:spill`/`:megamorphic` on line
  text, which also catches feedback preambles. Jumps whose target a lens
  hides clear the lens (same rule as folded blocks).
- [x] **6.3. Timeline view** (`Tab`): ordinal event list, severity colors,
  per-mode selection memory. Source switching moved from `Tab` to `]` as
  PLAN §8 always intended. Selecting an event shows it in context (cursor on
  the event line); jump history records the sidebar *mode*, so Ctrl+O from a
  deopt jump lands back on the event row.
- [x] **6.4. Deopt→graph jump** on `Enter` (PLAN §5.2 "selecting an event
  jumps"; `g` stays top-of-view). Follows docs/correlation-keys.md rule 3:
  most recent `(sfi, tier)` instance before the event; marking/compile events
  may bind forward to their dump; anything unresolvable reports why instead
  of guessing. Offset landing prefers the earliest graph phase's interleaved
  bytecode line, then a deopt frame at that offset, then the bytecode-array
  dump. *Not done:* "expected vs actual map" — the corpus traces do not print
  map addresses on both sides, so there is nothing honest to show yet.
- [x] **6.5. Deopt frame unwinding panel** (`--trace-deopt-verbose`): the
  panel *is* the event view — selecting a deopt shows `[bailout begin]`
  through `[bailout end]`, which under the verbose flag is the full
  reading/translating frame dump, with shape-based styling for the block
  (headers bold, input rows green, translation rows dim). Zero new pane
  machinery, and `y`/`Y`/`E`/search work on it for free.

---

### Phase 6 review pass  ✅ resolved

An independent review of the Phase 6 commit (instructed to verify every
finding by executing probes against the code and the fixture corpus)
surfaced 11 findings (0 critical / 3 major / 8 minor); all fixed. The
majors: the 6.5 deopt panel collapsed to one line on a *live* stream
(the open tail raw section is not in the index until the next boundary,
so the bailout-end search had no room — now falls back to
`lines_indexed()`); marking/compile-start events bound *backwards* to a
stale instance instead of forwards to the dump they trigger (binding
direction is now per event kind: deopts backward-only, marking/start
forward-first, done backward-first); and a history round-trip through a
deopt jump clobbered the parked selection, so the next Tab interpreted
an event index against the compilation list (history now parks the
selection it leaves and clamps). Minors: parked selections (other mode,
inactive split pane) now re-locate by identity on stream growth; the
telemetry source hint reads the live keymap instead of a hard-coded
"Tab"; event jumps always report where they landed; `--function`-hidden
targets say "hidden", not "absent"; `:clear` keeps the same event
selected as the timeline widens; timeline toggling clamps before the
event-cursor sync; palette completion truncates at char boundaries; and
the tests that were too weak to catch any of this were rewritten to
pin the fixed behavior.

## Phase 7 (post-MVP): Splits & Diff Engine  ✅ done

- [x] **7.1. Pane manager**: `v`/`s` splits (same key closes, other key flips
  orientation), `Ctrl+W` focus moves, per-pane selection driven by the
  sidebar. Implementation note: the active pane lives in the App's flat
  view fields and the inactive pane is a parked snapshot (the same swap
  pattern as the timeline selection), so every existing keybinding operates
  on "the current view" unchanged. *Deviation:* synced scroll exists only in
  diff mode, where it is structural (both panes walk one aligned row list);
  free-scrolling panes scroll independently.
- [x] **7.2. Canonicalizer** (`src/diff.rs`): hex addresses → `0x·`,
  `(addr:0x…)` host pointers, `[ML:n]` prefixes stripped, timing decimals
  masked; canonical node renumbering by definition order feeds the
  cross-compilation structural keys.
- [x] **7.3. Node-identity diff**: nodes keyed by `nN` id within one
  compilation (what `IRNode::id` preserves across phases), by structural
  hash (opcode + canonically renumbered inputs) across compilations;
  `similar`/Myers aligns the key sequences, and non-node rows are keyed by
  canonicalized text — the line-diff fallback and the aligner are one
  mechanism. Never a raw Myers over IR text: schedule ids, registers, live
  ranges and use counts are invisible to the diff by construction (verified
  against the corpus: graph building vs dead-nodes-sweeping shows exactly
  the dead node removed, zero false "changed" rows).
- [x] **7.4. Phase diff mode** (`d`): row-aligned two-column view with
  gutters and 256-colour row tints (`+` added, `−` deleted, `~` changed,
  `→` replaced, `≈` moved), synced by construction, summary counts in the
  status line plus a per-row story ("n9 inputs rerouted via Identity:
  [n5] → [n12]"). The states the diff distinguishes, per the design goal:
  added, deleted, opcode-changed, input-changed, input-*rerouted* (both
  sides' inputs resolve equal through the Identity maps), replaced-by-
  Identity (`nA: Identity [nB]`, chains resolved with a cycle guard), and
  moved (same id realigned elsewhere — not deleted+added). `d` without a
  split picks the pair: previous graph phase vs this one on a phase row,
  first vs last on a compilation. Search walks the aligned rows (either
  side matches); `y`/`Y`/`E` are diff-aware; folds and node jumps are
  disabled inside the diff with a pointer out. No `Identity` nodes exist in
  the corpus workloads, so that path is pinned by unit tests against the
  documented printer format.

---

### Phase 7 review pass  ✅ resolved

An independent review (probe-tested against HEAD and the corpus) surfaced
13 findings (1 critical / 4 major / 8 minor); all resolved. The critical:
matched node pairs compared only opcode + input ids, so **every change
carried in the operand payload read `Same`** — `Int32Constant(0)` →
`(7)`, `CheckMaps` against a different map, a retargeted branch, a flipped
truncation verdict. Node shapes now carry branch targets and a
component-wise operand payload (attached parameter group with input ids
masked, `<…>` heap operands, meaningful detached brackets, truncation
verdict, dead marker), each component compared only when both sides print
it — so register-allocation output dropping the clauses still diffs
clean, which a corpus-shaped test pins. Structurally-matched pairs
(cross-compilation) get the same payload judgement (`SmiConstant(3)` vs
`(7)` now reports). The majors: moved-and-changed nodes no longer swallow
the change (`Moved { changed }`); split panes' title row is accounted for
in mouse routing and paging; jump history refuses inside the diff (it
re-paired the panes and misread its row coordinates); the canonicalizer
trims indentation/gutter so the schedule column's re-indent no longer
fabricates delete+insert pairs. Minors: canonical renumbering now runs as
a first pass so loop-phi back edges match cross-compilation; the rerouted
test's cross-side identity resolution is documented as same-compilation-
only (it is sound there and unreachable elsewhere); `F` in diff uses the
aligned row count; timeline toggling re-syncs the parked pane and drops
the diff; `Ctrl+W` keeps the shared horizontal scroll; the help modal went
two-column instead of silently truncating; closing a split zeroes the
stale mouse rect; `d` on a collapsed function group expands the group too.

## Phase 8 (post-MVP): More Text Parsers  ✅ done (8.4 deliberately skipped)

Method note, per the exhaustiveness goal set for this phase: every format
was inventoried line-shape by line-shape across all three fixture builds
*before* writing a parser, and the goldens are the evidence — after the
regeneration, the three new formats parse with **zero** annotation
(unrecognised-line) counts and zero unknown phases in every build.

- [x] **8.1. Turboshaft / TurboFan text parser** (`--trace-turbo-graph`).
  Index: `Begin compiling method X using TurboFan` is the anchor (there is no
  `Compiling 0x…` line); identity is patched from the first SFI object in the
  body whose name matches the Begin line's, ordinals assigned then. Banner
  names are recognised by *shape* (`Graph after V8.TF*`, `V8.TFTurboshaft*`,
  `schedule`, `Instruction sequence *`) — they are version-stable in the
  corpus and carry no version vote, unlike Maglev's. Parse: four body
  grammars in `src/parse/turbofan.rs` — sea-of-nodes (`#17:Op[params](#2:…)`),
  schedule (`--- BLOCK B1 id1 ---` + bare-number inputs + `-> B1` targets),
  Turboshaft (`BLOCK/MERGE/LOOP` headers, `Load */Store *` memory forms,
  block targets in the bracket block), and instruction sequences (`vN`
  virtual registers with def/use tracking, gap rows as schedule-only). All
  of it lands in the same `IRNode`/`LineInfo` model, so styling, def-use
  highlighting, folding (Turboshaft/schedule blocks), lenses, `i`/`u` jumps
  and the **phase diff** work on TurboFan dumps unchanged — including
  `Enter` on a `TURBOFAN_JS` deopt event now that TF sections carry real
  SFIs. `--print-turbolev-frontend` re-audited under the same standard: the
  15.2 pipeline names were already in the marker table, 14.9 phase bodies
  are Maglev-shaped, and the goldens confirm full classification.
- [x] **8.2. Disassembly parser** (`--print-opt-code`). Index: `--- Raw
  source ---` anchors a code section (source text = always-visible
  preamble), `--- Optimized code ---` / `Instructions` / `Inlined functions`
  / `Deoptimization Input Data` / `Safepoints` / `RelocInfo` open
  [`PhaseKind::Listing`] phases, `--- End code ---` closes; identity comes
  from the `name = ` / `kind = ` header lines (scanned only inside the
  Optimized-code phase so source text cannot spoof it; the SFI is genuinely
  not printed, so these sections stay uncorrelated — documented, not
  guessed). Parse: `LineInfo::Disasm` rows with spans for the mnemonic, the
  branch target (`(addr 0x…)` on arm64, `<+0x…>` on x64) and the `;;` reloc
  comment — styled as-emitted, no decoding promised beyond what V8 prints
  (PLAN §7.6). Lookalike rows (safepoints, reloc, constant pools, SFI
  context) are shape-rejected, with tests.
- [x] **8.3. Bytecode parser** (`--print-bytecode`). `[generated bytecode
  for function: name (0x… <SharedFunctionInfo …>)]` anchors with full
  identity (tier Ignition, real SFI, ordinals); the bytecode rows reuse the
  existing bytecode-array line parser; `Constant pool` / `Handler Table` /
  `Source Position Table` open Listing phases — gated on the Ignition tier,
  because the same header lines appear inside Turbolev bytecode dumps as
  plain content (regression-tested).
- [ ] **8.4. (Optional) Turbolizer JSON import** (`--trace-turbo`) —
  deliberately skipped; it should be its own later phase. Nothing above
  depends on it.
- [x] **8.5. Bytecode-array & feedback-vector parser** (dogfood request).
  The `----- Bytecode array -----` dump (and Ignition `--print-bytecode`
  listings, and `Inlining …` callee dumps — same grammar) now parses
  exhaustively instead of offset-only: rows carry mnemonic span, jump-target
  operands (the `(0x… @ N)` suffix and switch `{ 0: @44, … }` tables),
  `FBV[N]` feedback-slot refs, and `[N:…]` constant-pool refs (bare `[N]` is
  an immediate, not a pool index; `EmbeddedFeedback[N]` is not a vector
  slot). Regions delimited by V8's own headers: `Constant pool (size` →
  entry rows (`N: value`), `0x…: [FeedbackVector]` → ` - slot #N Kind STATE`
  headers classified by IC state (mono / poly / mega / uninitialized /
  other-lattice). Navigation mirrors the graph def-use chain: `i` on a row
  cycles its refs (target offset, then slots, then pool entries — the
  latter two found cross-phase for Ignition dumps), `u` on a slot / pool
  entry / jump target cycles the rows referencing it, all through the same
  anchored-cycle + history machinery (`Anchor::{Node,Offset,Slot,Pool}`).
  Styling: dump rows get dim addr/hex columns, offset def-token, mnemonic
  by opcode shape, refs in node/constant/block colours; slot states colour
  by severity (mega red, poly yellow, mono green); a cursor-linked overlay
  lights refs ⇄ definitions both directions. Interleaved graph-context rows
  share the parser (jump/`i` works there too) but stay uniformly dim.
  Goldens print per-dump row/ref/slot counts as falsifiable coverage
  evidence; fixtures already carried the formats (poly = mega/poly slots,
  truncation = jumps + BinaryOp lattice, print-bytecode = wide mnemonics,
  `CallRuntime [Name]`, `EmbeddedFeedback`).
- [x] **8.6. Block navigation** (dogfood request). `[`/`]` walk the listing
  block by block — unconditional, no history, `j`/`k` at block granularity;
  folded blocks land on their fold marker (`}` took over source switching).
  Enter in the viewport follows the cursor node's control-flow refs — a
  `Jump`'s successor, a branch's left then right target, a switch's cases —
  cycling block headers through the same anchored-cycle + history machinery
  as `i`/`u` (`Anchor::Block`/`Anchor::Line` for headers and schedule-only
  rows). `u` on a block header cycles its *predecessors*: every node whose
  targets include it — the "who jumps here" question at loop headers and
  merges. Sidebar/timeline Enter behaviour unchanged.
- [x] **8.7. Sidebar toggle** (dogfood request). `b` hides/shows the
  sidebar; hidden, its columns go to the viewport and a two-column strip
  stays as the clue it exists: an accent `▸` at mid-height in front of a
  dim rule (a top-corner glyph proved too subtle), clickable.
  `h`, a click on the strip, or Tab (the timeline lives there) bring it
  back; focus leaves a hidden pane; strip scroll events are ignored so a
  hidden sidebar's selection cannot drift invisibly.
- [x] **8.8. End markers close only their own section** (dogfood report).
  `Finished compiling method X using <Tier>` now closes the open section
  only when the *name* matches too (tier already checked; Turbolev's
  misleading `using TurboFan` trailers stay the exception). With
  concurrent recompilation A's trailer lands inside B's body — closing B
  there glued A's end marker onto B and orphaned B's own trailer as a raw
  section. A mismatched trailer stays what it physically is: interleaved
  content of whoever is open. Also: the closing rule and Begin/Finished
  trailers inside a phase's range classify as Control now, not
  annotations — every Maglev section ended in dim-italic noise that `t`'s
  annotation folds swept up (goldens: last-phase annotation counts drop
  by exactly the rule+trailer pair, boundaries unchanged).

---

### Phase 8 review pass  ✅ resolved

An independent review (probe-tested against HEAD and all three builds)
surfaced 15 findings (2 critical / 5 major / 8 minor); all resolved. The
criticals: `[x3|R|w32] = Arm64Asr32 …` bracket-destination rows — ~14 %
of post-RA instruction rows — lost their opcode to a bogus `insn`
placeholder (the opcode now always comes from the right of ` = `, and
`phi:` rows parse as phis); and TurboFan identity bound through the
*name* of the first matching SFI object anywhere in the body, letting a
same-named closure constant steal the identity — the scan now accepts
only SFI objects on `FrameState` lines, is budget-bounded (an
unidentifiable section costs O(5000 lines), not O(section)), and a
section with no such line stays honestly unidentified. The majors:
grouped mode no longer merges every sfi-less section into one bogus
"function" (they list individually); the goldens now print
`N disasm, M annotations` for listing phases and unmatched Instructions
rows classify as annotations, making the coverage claim falsifiable;
`--- Optimized code ---` anchors by itself (the Raw-source block is
optional in V8's output); unidentified sections carry 1-based
provisional ordinals instead of duplicate `#0` keys; and the
uppercase-word instruction fallback no longer swallows stray program
output as fake IR (corroborating structure required). Minors: the
`FUNC` regex no longer crosses `>` boundaries; disasm branch-target
search stops at the `;;` comment; `:phi`/`:spill` lenses know
Turboshaft's `Phi`/`Goto` and TF's `gap (… [stack:…])` spelling; diff
keys for schedule-only instruction rows drop the leading instruction
index (before/after-RA diffs no longer cascade); `Finished compiling`
only closes a section of its own tier and clears the code-section
state; duplicate `Optimized code` headers cannot bump ordinals or
rename (one-shot latches); and tests now pin every one of these shapes
plus `LOOP` headers and paren-less schedule rows.

### Dogfood fixes (first real-trace reports)  ✅ resolved

Four author reports from real traces, all fixed and verified end-to-end
in tmux:

- **Copy never reached the system clipboard.** `remote()` treated a local
  tmux session as OSC-52-only; Terminal.app does not interpret OSC 52 at
  all, so copies vanished. tmux alone is no longer "remote" — locally the
  OS pasteboard (arboard) is authoritative, OSC 52 stays the SSH route
  and the local fallback. Verified: `y` inside local tmux lands in
  `pbpaste`.
- **`Begin compiling … using TurboFan` ahead of a Turbolev dump** split a
  bogus three-line Turbofan stub off the real section. The tier word on
  that line is misleading; only the following banner reveals the
  pipeline. The indexer now converts the still-empty TurboFan section in
  place when `Bytecode before MaglevGraphBuilding` arrives, and lets the
  `using TurboFan` trailer close a Turbolev section.
- **Turboshaft phases inside Turbolev sections** (the pipeline prints
  Maglev IR early and Turboshaft IR after lowering) were parsed with the
  Maglev grammar — `MERGE B… <- B…` headers and `#N` refs were invisible,
  so folding, def-use, and `i`/`u` were dead there. Graph phases in
  non-TurboFan sections now sniff a bounded body prefix for
  `BLOCK/MERGE/LOOP B<n>` headers and route to the Turboshaft grammar,
  regardless of what the banner is named.

## Phase 9 (post-MVP): Source Alignment, Dual-Run & Live Mode

- [ ] **9.1. Source resolution**: load `.js` when the script path resolves;
  graceful degradation otherwise (PLAN §5.1).
- [ ] **9.2. Alignment pane** (`S`): JS ⇄ bytecode ⇄ IR cross-highlighting via
  bytecode offsets, both directions.
- [ ] **9.3. Dual-run mode** (`garage a.log b.log`): dual stores, telemetry
  comparison header, per-function drill-down using the Phase 7 diff engine.
- [ ] **9.4. Wrapper/live mode** (`garage -- d8 ...`): spawn child, capture
  stdout+stderr with explicit merge policy, incremental sidebar updates, signal
  handling (`SIGINT`/`SIGTERM`) and child cleanup. Introduce `tokio` here only
  if plain threads prove insufficient.
- [ ] **9.5. Inlining tree** (`I`) from trace output.

---

## Phase 10: Quality, Packaging & Ongoing

- [ ] **10.1. CI**: golden tests, fuzz targets, clippy, release build check.
- [ ] **10.2. Format-drift canary**: scheduled job parsing fresh tip-of-tree
  fixtures; failures open issues with the offending output attached.
- [ ] **10.3. Terminal compatibility matrix** re-run per release (resize, small
  viewports, color depths, tmux, SSH).
- [ ] **10.4. Packaging**: static release binary, versioned releases, README
  kept current with a keymap cheatsheet.

---

## Icebox — agreed ideas, deliberately not scheduled

Kept out of the numbered phases on purpose; nothing above depends on them, and
the annotation attachment rule (2.8) keeps them cheap to pick up later.

- Annotation channels: TOML prefix map (`truncation`, `inlining`, …),
  `:trace <channel>` filter, node-ID linking in annotation lines (PLAN §6.1).
- Node biography panel (`e`) — "explain this node" (PLAN §7.11).
- Loop lens (`:loops`) with per-loop stats (PLAN §7.12).
- Deopt flip-flop detector (PLAN §7.13).
- Watch mode (`--watch`) with position restore (PLAN §7.14).
- Non-interactive subcommands `stats`/`grep`/`diff` for CI (PLAN §7.15).
