# `garage` — Implementation Roadmap & Task List

Task breakdown for `garage` (see [PLAN.md](PLAN.md)). Resequenced from v1:
**de-risking comes first** (the parsers are the project risk, so fixtures and a
parse spike precede any TUI work), and the **MVP is cut at Phase 5** instead of
spanning every feature in the plan.

---

## Phase 0: De-Risking & Fixture Corpus  ← do this before writing the tool

- [ ] **0.1. Collect fixture corpus (checked into repo)**
  - Real `d8` outputs for: `--print-maglev-graphs`, `--print-turbolev-frontend`,
    `--trace-turbo-graph`, `--print-opt-code`, `--print-bytecode`, `--trace-opt`,
    `--trace-deopt[-verbose]`, `--trace-osr`, `--trace-ic`, `--trace-gc`.
  - From tip-of-tree **and** one stable branch; on **x64 and arm64**.
  - Include one trace with concurrent recompilation enabled (interleaved output)
    and one with `--no-concurrent-recompilation --predictable` (clean).
  - Include a graph dump interleaved with pass tracing (e.g.
    `--print-maglev-graphs --trace-maglev-truncation`) to pin down the
    annotation-attachment rules (PLAN §6.1).
  - Include at least one multi-hundred-MB trace for perf testing (not checked in;
    scripted generation).
- [ ] **0.2. Verify flag inventory** against current `flag-definitions.h`; correct
  PLAN.md if any listed flag is stale or misnamed.
- [ ] **0.3. Throwaway parse spike**: a ~200-line Rust program that splits one
  Maglev fixture into compilations/phases and prints the tree. Goal: validate
  section-marker assumptions and discover interleaving/format surprises *now*.
- [ ] **0.4. Printer↔parser contract note** (½ page in `docs/`): list the V8
  source files that emit each parsed format (Maglev graph printer, deopt traces,
  …) and add a script that regenerates fixtures from a local `d8` build — so
  when a printer changes, updating `garage` is a mechanical step done in the
  same breath.
- [ ] **0.5. Correlation-key spec**: document exactly how `--trace-deopt` lines
  map to compilation instances (code address / optimization id / function+offset)
  for current V8. Blocks the deopt→graph jump later.

---

## Phase 1: Project Skeleton & Terminal Infrastructure

- [ ] **1.1. Crate setup**: binary crate, MVP deps only (`ratatui`, `crossterm`,
  `clap`, `regex`, `memmap2`, `anyhow`, `thiserror`); release profile with LTO.
- [ ] **1.2. File-based internal logging** (`tracing` → `garage.log`) so
  diagnostics never touch the TUI screen buffer.
- [ ] **1.3. CLI parsing** (`clap`): `garage [FILE...]`, `garage -- <cmd>...`,
  `--config`, `--debug`, `--function <name>` (parse-time prefilter).
- [ ] **1.4. Input abstraction**: `LogSource` = `File` (mmap), `Stdin`, later
  `Process`. Reader thread + channel into app state (no `tokio` unless wrapper
  mode later justifies it).
- [ ] **1.5. `/dev/tty` keyboard input** when stdin is the data pipe
  (`d8 | garage`). Day-one requirement; test in tmux and over SSH.
- [ ] **1.6. Terminal lifecycle**: raw mode, alt screen, **event-driven** redraw
  (input / data / resize — no fixed 60 fps tick), clean restore on exit and in
  the panic hook.

---

## Phase 2: Data Model, Indexer & Maglev Parser (only)

- [ ] **2.1. Core types**: `CompilationKey` (function, script, line, tier,
  compilation index, **OSR offset: Option**), `Tier` enum (incl. distinguishing
  OSR), `FunctionCompilation`, `Phase`, `BasicBlock`, `IRNode` (id, opcode,
  inputs, users, block, registers, type annotations, bytecode offset),
  `TimelineEvent`, `RawSection`.
- [ ] **2.2. Section indexer**: single streaming pass that finds section
  boundaries (table-driven markers, embedded TOML) and records byte ranges —
  **without parsing bodies**. This is what makes 1 GB files open instantly.
- [ ] **2.3. Lazy per-compilation parsing** on first view; parsed results cached.
- [ ] **2.4. Maglev graph parser**: blocks (`b0:`, loop headers), node defs
  (`n10 = Int32Add n8, n9`), inputs/users, register allocation output, bytecode
  offsets, type feedback.
- [ ] **2.5. Deopt/opt line parser** (`--trace-opt`, `--trace-deopt`) into
  `TimelineEvent`s (timeline *UI* comes later; the data is cheap to keep now).
- [ ] **2.6. Fallback raw sections** for anything unmatched, with `[unparsed]`
  status; malformed input must never panic (fuzz test, see 5.4).
- [ ] **2.7. Golden-file tests** over the Phase 0 corpus for everything above.
- [ ] **2.8. Annotation attachment rule (minimal)** (PLAN §6.1): unmatched lines
  inside an open compilation attach to the enclosing phase/transition as
  positioned annotations — never dropped, never exiled to an orphan raw
  section. Golden-test against the interleaved `--trace-maglev-truncation`
  fixture. (Channels, prefix maps, and node-ID linking: icebox.)

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
  matrix test (80×24, tmux, 16-color, SSH).
- [ ] **5.5. README**: install, sample `d8` invocations, keymap, recommended
  flags for clean traces (`--no-concurrent-recompilation --predictable`).
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
