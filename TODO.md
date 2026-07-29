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

- [ ] **0.6. Version-keyed marker TOML**: phase names come from
  `src/maglev/maglev-phase.h` `PhaseName()`, which is a generated-from-source
  table, not an observed one. Needs a version axis (14.9 and 15.2 share no phase
  names). Folds into 2.2.
- [ ] **0.7. Golden-test harness** over the corpus, keyed off `manifest.json`'s
  `reproducible` flag. Folds into 2.7.

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

## Phase 2: Data Model, Indexer & Maglev Parser (only)

- [ ] **2.1. Core types**: `CompilationKey` (**SFI address** — the correlation
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
- [ ] **2.2. Section indexer**: single streaming pass that finds section
  boundaries (table-driven markers, embedded TOML) and records byte ranges —
  **without parsing bodies**. This is what makes 1 GB files open instantly.
  - Anchor compilations on `Compiling 0x… <JSFunction …> with <Tier>`, not on
    the `Begin compiling method` banner, which does not exist before V8 15.2.
  - Marker table is **version-keyed** (0.6). Not arch-keyed: architecture only
    affects register names inside node lines.
  - `----- Bytecode array -----` and `----- Inlining 0x… with bytecode -----`
    share the phase-banner grammar but are not phases; match them specially.
- [ ] **2.3. Lazy per-compilation parsing** on first view; parsed results cached.
- [ ] **2.4. Maglev graph parser**: blocks (`b0:`, loop headers), node defs
  (`n10 = Int32Add n8, n9`), inputs/users, register allocation output, bytecode
  offsets, type feedback.
- [ ] **2.5. Deopt/opt line parser** (`--trace-opt`, `--trace-deopt`) into
  `TimelineEvent`s (timeline *UI* comes later; the data is cheap to keep now).
- [ ] **2.6. Fallback raw sections** for anything unmatched, with `[unparsed]`
  status; malformed input must never panic (fuzz test, see 5.4).
- [ ] **2.7. Golden-file tests** over the Phase 0 corpus for everything above.
  - Key off `manifest.json`'s `"reproducible"` flag (0.7). Note the
    `+deoptverbose` fixtures are `false` **only** because of the `(addr:0x…)`
    host pointer — masking it makes them exact, so they are still usable as
    goldens through the canonicalizer. Phase 0.3 §12.
- [ ] **2.8. Annotation attachment rule (minimal)** (PLAN §6.1): unmatched lines
  inside an open compilation attach to the enclosing phase/transition as
  positioned annotations — never dropped, never exiled to an orphan raw
  section. Must handle "inside a compilation, no phase open yet": inlining
  traces print before the first phase banner. Golden-test against
  `maglev-graphs+inlining.inlining.log` (no ids) **and**
  `maglev-graphs+inlining-ids.inlining.log` (with `[ML:<id>]` prefixes) — the
  prefix is absent whenever `--no-trace-with-compilation-id` is used, so it
  cannot be load-bearing. (Channels, prefix maps, and node-ID linking: icebox.)

---

## Phase 3: TUI Layout & Navigation

- [ ] **3.1. Layout grid**: telemetry bar / sidebar + viewport / status line.
- [ ] **3.2. Telemetry bar**: compilations per tier, deopt count, source status.
- [ ] **3.3. Sidebar**: chronological ⇄ grouped-by-function toggle, OSR and
  `[unparsed]` badges, selection highlight, scrollbar.
- [ ] **3.4. Viewport**: vertical/horizontal scroll, `PgUp`/`PgDn`/`Home`/`End`,
  line wrapping toggle.
- [ ] **3.5. Keybindings** per PLAN §8 (**`?` = help; `h`/`l` = pane focus — the
  v1 double-binding of `h` is resolved**); bindings remappable via config TOML.
- [ ] **3.6. Help modal** (`?`) with keymap + command reference.

---

## Phase 4: Syntax Highlighting & Node Interactivity

- [ ] **4.1. IR tokenizer** → styled spans: opcodes, block labels, registers,
  node IDs, types/maps; 16/256/TrueColor palettes with detection + fallback.
  Register vocabulary is the one arch-dependent axis (`x0`/`rax`); detect it
  from the trace, not the host. Strip d8's own SGR escapes first — it colorizes
  even when piped — and re-emit `garage`'s own styling.
- [ ] **4.2. Basic block folding** (`Space`), per-block persisted state,
  `[+] b1 (loop) — 14 hidden` summaries.
- [ ] **4.3. Cursor node tracking**: resolve node ID under cursor.
- [ ] **4.4. Def-use / use-def highlighting**: definition vs inputs vs consumers
  in distinct styles, computed from the parsed graph (not regex-on-view).
- [ ] **4.5. Node jumps**: `i` to input, `u` cycles consumers, `Ctrl+O`/`Ctrl+I`
  history stack.
- [ ] **4.6. Search**: `/` regex prompt, incremental highlight, `n`/`N`; works in
  parsed *and* raw sections.
- [ ] **4.7. Sidebar quick filter** (`f`) by function/script/tier.
- [ ] **4.8. Annotation rendering (minimal)**: dimmed, folded by default
  (`[+] 12 trace lines`); `t` toggles inline visibility.

---

## Phase 5: MVP Hardening & Ship  🏁

- [ ] **5.1. Perf pass** on the large fixture: open 500 MB < 2 s, cursor latency
  < 16 ms, memory bounded (string interning, no full-file String copies).
- [ ] **5.2. Clipboard**: `:copy` via **OSC 52** (SSH/tmux) with `arboard` local
  fallback.
- [ ] **5.3. Export**: `:export <file>` writes current view/selection as
  Markdown/plain text (for bugs and Gerrit comments).
- [ ] **5.4. Robustness**: fuzz parsers with garbage/truncated input; terminal
  matrix test (80×24, tmux, 16-color, SSH). Must include **Linux and SSH** for
  the fd-0 terminal handling from 1.5, which so far is only measured on macOS
  under tmux — including the case where neither stdout nor stderr is a terminal
  and `/dev/tty` is the only remaining route.
- [ ] **5.5. README**: install, sample `d8` invocations, keymap, recommended
  flags for clean traces (`--no-concurrent-recompilation --predictable`, plus
  `--no-trace-with-compilation-id` when diffing runs). Note that the Maglev
  graph flags need a build with `V8_ENABLE_MAGLEV_GRAPH_PRINTER`.
- [ ] **5.6. Dogfood milestone**: author uses `garage` instead of `less` for one
  week of real Maglev work; fix what hurts. **MVP done = you don't go back.**

---

## Phase 6 (post-MVP): Command Palette & Timeline

- [ ] **6.1. `:` palette** with completion and message line.
- [ ] **6.2. Semantic filters**: `:deopts`, `:checks` (with count), `:spill`,
  `:phi`, `:megamorphic`, `:function <name>`.
- [ ] **6.3. Timeline view** (`Tab`): ordinal event list, severity colors.
- [ ] **6.4. Deopt→graph jump** (`g`) using the Phase 0.5 correlation spec;
  show expected vs actual map when the trace provides it.
- [ ] **6.5. Deopt frame unwinding panel** (`--trace-deopt-verbose`).

---

## Phase 7 (post-MVP): Splits & Diff Engine

- [ ] **7.1. Pane manager**: `v`/`s` splits, `Ctrl+W` focus moves, synced scroll
  option.
- [ ] **7.2. Canonicalizer**: strip/normalize addresses, timestamps, compilation
  ids; canonical node renumbering. (Prerequisite for useful diffs — see PLAN
  §7.4; **do not ship raw Myers line diff of IR**.)
- [ ] **7.3. Node-identity diff**: match by preserved IDs where available, else
  structural hash (opcode + canonical inputs); `similar` line-diff fallback for
  unmatched regions.
- [ ] **7.4. Phase diff mode** (`d`): insert/delete/modify styling, synced
  scrolling, diff summary counts.

---

## Phase 8 (post-MVP): More Text Parsers

- [ ] **8.1. Turboshaft / Turbolev text parsers** (`--print-turbolev-frontend`,
  `--trace-turbo-graph`).
- [ ] **8.2. Disassembly parser** (`--print-opt-code`): jump targets, registers,
  reloc comments as-emitted (no promised decoding beyond what V8 prints).
- [ ] **8.3. Bytecode parser** (`--print-bytecode`).
- [ ] **8.4. (Optional) Turbolizer JSON import** (`--trace-turbo`) behind the
  same parser trait — supplemental source, handy for cross-phase node identity
  in diffs; nothing depends on it.

---

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
