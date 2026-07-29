# `garage` - MVP Implementation Roadmap & Task List

This document breaks down the development of `garage` (Interactive TUI Tool for V8 Traces and Logs) into phased, highly granular milestones for the Minimum Viable Product (MVP) based on [`PLAN.md`](file:///usr/local/google/home/victorgomes/garage/PLAN.md).

---

## Phase 1: Project Setup, CLI & Core Architecture

- [ ] **1.1. Cargo Workspace & Dependency Configuration**
  - Initialize Rust binary crate `garage`.
  - Add core dependencies to `Cargo.toml`: `ratatui`, `crossterm`, `clap`, `regex`, `tokio`, `anyhow`, `thiserror`, `bitflags`, `unicode-width`, `arboard` (clipboard), `tempfile`.
  - Configure build profiles (`opt-level = 3`, LTO enabled for release builds).

- [ ] **1.2. Internal Logging & Diagnostic System**
  - Implement a hidden file-based logger (e.g. `garage.log` via `tracing` or `log`) so standard stdout/stderr logging does not interfere with the terminal TUI screen buffer.

- [ ] **1.3. CLI Argument Parsing (`clap`)**
  - Parse positional arguments: `garage [FILE_1] [FILE_2]`.
  - Support subcommands or raw command wrapper mode: `garage -- <COMMAND> [ARGS...]`.
  - Add CLI flags for configuration overrides (`--config`, `--debug`).

- [ ] **1.4. Multi-Source Log Input Abstraction**
  - Define `LogSource` enum/trait: `File(PathBuf)`, `Stdin`, `Process(Child)`.
  - Implement async stream reader reading line-by-line using `tokio::io::BufReader`.
  - Implement graceful signal handling (`SIGINT`, `SIGTERM`) for spawned child processes.

- [ ] **1.5. Terminal Raw Mode & Event Loop Skeleton**
  - Setup `crossterm` raw mode, alt screen, mouse capture.
  - Implement central event loop processing key events, terminal resize events, and tick events (16ms / 60fps refresh rate limit).
  - Implement clean terminal restoration on exit or panic hook.

---

## Phase 2: Log Parsing Engine & Data Model

- [ ] **2.1. Core Domain Data Structures**
  - Define `CompilationKey`: `(FunctionName, ScriptUrl, LineNumber, Tier, CompilationIndex)`.
  - Define `Tier` enum: `Ignition`, `Sparkplug`, `Maglev`, `Turboshaft`, `TurboFan`, `Unknown`.
  - Define `FunctionCompilation`: ID, `CompilationKey`, timestamp/order, list of `Phase`s, list of `DeoptEvent`s.
  - Define `Phase`: name, index, raw lines, list of parsed `BasicBlock`s, list of `IRNode`s.
  - Define `IRNode`: ID (`n55`, `v12`), opcode name, inputs (`Vec<NodeId>`), outputs/users (`Vec<NodeId>`), block ID, registers assigned (`Vec<String>`), type annotations, bytecode offset, JS line.
  - Define `BasicBlock`: ID (`b0`), label, loop header flag, instructions range.
  - Define `TimelineEvent`: ID, step index, event type (`DeoptEager`, `DeoptSoft`, `ICUpdate`, `OptEvent`, `GCEvent`), message, timestamp, target compilation & phase reference.

- [ ] **2.2. Dynamic Regex & Grammar Rule Engine**
  - Create `parser/config.rs` loading embedded markers with fallback definitions.
  - Implement regex matchers for section boundaries (e.g., `--- <Pass Name> ---`, `== Phase: <Name> ==`).
  - Implement fallback handling for unrecognized log structures so no log data is dropped.

- [ ] **2.3. Maglev Graph Parser**
  - Implement parser for `--print-maglev-graphs` and `--print-maglev-graph`.
  - Parse basic block boundaries (`b0:`, `b1 (Loop Header)`).
  - Parse node definitions, opcode names, inputs (`n10 = Int32Add n8, n9`), and type feedback annotations.
  - Parse register allocation outputs and bytecode offsets.

- [ ] **2.4. Turboshaft Graph Parser**
  - Implement parser for `--print-turbolev-frontend` and `--trace-turbo-graph`.
  - Parse block identifiers, operations, inputs/uses, and representation hints.

- [ ] **2.5. CodeGen & Disassembly Parser**
  - Implement parser for `--print-opt-code` and `--print-code`.
  - Parse assembly instructions, jump targets (`jmp .Lentry_1`), registers (`rax`, `x0`), relocation hints, and memory offsets.

- [ ] **2.6. Deopt & Lifecycle Event Parser**
  - Implement parser for `--trace-opt`, `--trace-deopt`, `--trace-deopt-verbose`.
  - Extract deopt reasons, function targets, frame unwinding stack registers, and interpreter bytecode offsets.
  - Parse `--trace-ic` and `--trace-gc` lines into `TimelineEvent` items.

- [ ] **2.7. Log Indexing & Streaming Pipeline**
  - Connect parser stream to state container (`TraceStore`).
  - Thread-safe shared state (`Arc<RwLock<TraceStore>>`) populated incrementally in background tasks.

---

## Phase 3: TUI Layout Framework & Basic Navigation

- [ ] **3.1. Main Window Layout Grid**
  - Create main layout using `ratatui::layout::Layout`:
    - Top: Engine Telemetry Bar (1-2 lines).
    - Middle Split: Left Sidebar (Compilation Tree / List) vs Right Viewport (Graph / Phase content).
    - Bottom: Status Bar & Command Palette (1 line).

- [ ] **3.2. Engine Telemetry Bar Component**
  - Render summary stats: total compilations count per tier (Maglev, Turboshaft, etc.), total deopts count, total GC events.
  - Display log source metadata (filename, PID, stream status).

- [ ] **3.3. Sidebar (Compilation & Phase Tree View)**
  - Render chronologically sorted `Function * Tier` compilation instances.
  - Expandable tree nodes for function compilations -> sub-items for compilation phases.
  - Visual selection indicators (highlight bar, scrollbar).

- [ ] **3.4. Main Viewport & Scroll Manager**
  - Implement line-wrapping, horizontal scrolling, and vertical viewport clipping.
  - Support fast scrolling (`PgUp`, `PgDn`, `Home`, `End`).

- [ ] **3.5. Navigation Keybindings Controller**
  - Implement Vim-style bindings: `j`/`k` (up/down), `h`/`l` (pane switch focus), `Enter` (select phase/function).
  - Implement `Tab` key to toggle between Compilation Tree view and Global Timeline view.

- [ ] **3.6. Help Modal (`?` / `h`)**
  - Create interactive popup modal displaying keymaps, shortcut cheat-sheet, and command palette descriptions.
  - Press `Esc` or `q` to dismiss modal.

---

## Phase 4: IR Syntax Highlighting & Node Interactivity Engine

- [ ] **4.1. ANSI & IR Syntax Tokenizer**
  - Tokenize IR phase lines into styled spans (`ratatui::text::Span`).
  - Color map rules:
    - Opcodes (`Int32Add`, `Phi`, `Branch`): Bold Cyan/Magenta.
    - Block Labels (`b0`, `b1`): Yellow.
    - Registers (`rax`, `rdi`, `x0`): Green.
    - Node IDs (`n55`, `v12`): Light Blue.
    - Type annotations & maps: Dark Gray / Blue.

- [ ] **4.2. Basic Block Folding (`Space`)**
  - Track expand/fold toggle state per `BasicBlock` ID.
  - Collapse folded basic blocks to header summary line `[+] b1 (Loop Header) - 14 instructions hidden`.

- [ ] **4.3. Cursor & Node Selection Tracker**
  - Detect cursor line position over IR nodes (`n55` / `v12`).
  - Extract focused node ID under cursor.

- [ ] **4.4. Def-Use & Use-Def Visual Highlighting**
  - On node selection, compute:
    - **Node Definition**: Highlight line defining the node.
    - **Inputs (Def-Use)**: Highlight all predecessor lines generating node inputs.
    - **Consumers (Use-Def)**: Highlight all successor lines consuming node value.
  - Apply distinct background highlight colors for definition vs inputs vs users.

- [ ] **4.5. Node Jump Navigation & History Stack**
  - Implement `i` key action: jump cursor to selected node's input definition line.
  - Implement `u` key action: cycle cursor through consuming node lines.
  - Maintain navigation history stack: `Ctrl+O` (back) and `Ctrl+I` (forward).

---

## Phase 5: Search, Filtering & Command Palette

- [ ] **5.1. In-View Regex Search (`/`)**
  - Press `/` to activate search prompt in status bar.
  - Real-time search query matching against current viewport lines.
  - Highlight all search hits in current view.
  - `n`: Jump to next match; `N`: Jump to previous match.

- [ ] **5.2. Sidebar Quick Filter (`f`)**
  - Press `f` to filter compilation tree items by function name, script URL, or tier.
  - Esc clears filter string.

- [ ] **5.3. Vim-Style Command Palette Component (`:`)**
  - Activate input prompt on `:` key.
  - Support auto-complete suggestions for available commands.
  - Display user messages / error status in command line buffer.

- [ ] **5.4. Core Built-In Commands Implementation**
  - `:deopts` — Jump directly to the next deoptimization event / phase in trace.
  - `:phi` — Toggle filter mode hiding arithmetic nodes to show only backbone control flow & Phi nodes.
  - `:check` — Highlight type guards (`CheckMaps`, `CheckSmi`, `CheckBounds`).
  - `:spill` — Highlight register spills (`GapMove`, `Spill`, `Reload`).
  - `:megamorphic` — Highlight slow-path IC stub calls.
  - `:copy` — Copy selected node line or phase selection to system clipboard using `arboard`.

---

## Phase 6: Multi-Pane & Side-by-Side Phase Diffing

- [ ] **6.1. Multi-Pane Workspace Manager**
  - Support active pane focus management and layout tree (Left, Right, Top, Bottom).
  - Implement `v` key: toggle vertical split view.
  - Implement `s` key: toggle horizontal split view.
  - Implement `Ctrl+W` / arrow keys for focus navigation between split panes.

- [ ] **6.2. Side-by-Side Phase Diff Engine (`d`)**
  - Implement line alignment diff algorithm (using Myers diff or `similar` crate) for compiler phases.
  - Press `d` to enter diff mode between Phase N and Phase N+1 (or left vs right pane).
  - Highlight diffs:
    - **Green**: Newly inserted nodes/instructions.
    - **Red**: Removed/folded nodes.
    - **Yellow**: Modified register assignments or annotations.
  - Synchronize vertical scrolling across diffed panes.

---

## Phase 7: JS Source & Bytecode Alignment Pane

- [ ] **7.1. JS Source & Ignition Bytecode Loader**
  - Add file reader for original `.js` source files or embed extracted JS code from trace.
  - Parse Ignition Bytecode stream output (`--print-code` / `--print-bytecode`).

- [ ] **7.2. Alignment Pane Layout (`S`)**
  - Press `S` to toggle a side-by-side JS source / Bytecode alignment pane.
  - Render JS source code on left sub-pane, Bytecode on middle sub-pane, IR on right pane.

- [ ] **7.3. Cross-Highlighting Engine**
  - Map IR node bytecode offsets (e.g. `@ offset 14`) to Bytecode instructions (`Ldar a0`) and JS source line numbers (`return a + b;`).
  - Selecting an IR node automatically highlights corresponding Bytecode line and JS line.
  - Selecting a JS source line highlights all associated IR nodes in the graph pane.

---

## Phase 8: Deopt Frame Unwinding & Global Timeline Interactivity

- [ ] **8.1. Global Timeline View (`Tab`)**
  - Render full sequential log of execution events (Deopts, IC updates, Opt events, GC pauses).
  - Assign relative ordinal steps (`#001`, `#002`, ...).
  - Color-code events by severity (Red for Deopts, Yellow for IC transitions, Green for Optimizations).

- [ ] **8.2. Deopt Frame Unwinding UI Component**
  - Render simulated interpreter stack frame popup / side pane when viewing a deopt event.
  - Format unboxed registers (`r0`, `r1`), stack slot contents, and HeapObject map pointer references.

- [ ] **8.3. Deopt-to-Graph Jump (`g`)**
  - When focused on a Deopt item in Timeline, pressing `g` jumps directly to the corresponding `Function * Tier` phase at the exact bytecode offset instruction in the CodeGen/IR viewport.

---

## Phase 9: Dual-Run Trace Comparison & Live Process Mode

- [ ] **9.1. Dual-Run Trace Loader (`garage baseline.log patched.log`)**
  - Accept two trace file paths from CLI arguments.
  - Load both trace models into dual root viewports.
  - Render dual compilation telemetry comparison header.

- [ ] **9.2. Dual-Run Side-by-Side Diffing**
  - Compare total compilation count, deopt count, and individual function phase outputs between run 1 and run 2.

- [ ] **9.3. Live Process Wrapper & Stream Monitoring (`garage -- d8 ...`)**
  - Spawn child `d8` process and capture standard stdout/stderr streams asynchronously.
  - Continuously update sidebar compilation entries and telemetry header while `d8` executes.
  - Non-blocking TUI rendering during active streaming.

---

## Phase 10: Testing, Quality Assurance & Polish

- [ ] **10.1. Unit Test Suite for Parsers**
  - Add test fixtures containing sample outputs for `--print-maglev-graphs`, `--print-turbolev-frontend`, `--trace-deopt`, `--trace-ic`.
  - Write unit tests verifying correct node, basic block, and timeline parsing.

- [ ] **10.2. Performance Benchmarking & Memory Optimization**
  - Test loading large trace files (100MB - 1GB).
  - Optimize memory usage (string interning for opcodes/registers, lazy line parsing where appropriate).

- [ ] **10.3. Integration & Terminal Compatibility Testing**
  - Test terminal resize behavior, small terminal viewports (80x24), and high-DPI viewports.
  - Test color palette fallback on 16-color, 256-color, and TrueColor terminals.

- [ ] **10.4. Packaging & Documentation**
  - Verify static binary build (`cargo build --release`).
  - Create README with usage instructions, keyboard cheatsheet, and sample `d8` invocation commands.
