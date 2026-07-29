# `garage` - Interactive TUI Tool for V8 Traces and Logs

## 1. Executive Summary & Vision

`garage` is a high-performance Terminal User Interface (TUI) designed for V8 engineers and JavaScript engine researchers to view, search, diff, and debug `d8` execution traces and compiler graphs (Maglev, Turboshaft, TurboFan, Ignition, Deopts, ICs, GC).

Instead of dumping megabytes of text into stdout or relying on heavy browser-based tools like Turbolizer for quick CLI iterations, `garage` provides a keyboard-driven, syntax-aware, multi-pane TUI tool directly in the shell.

---

## 2. Supported Traces & Auto-Detection

`garage` automatically detects and categorizes logs output by standard and developer `d8` flags, including:

- **Compiler Graphs:**
  - `--print-maglev-graphs`, `--print-maglev-graph`
  - `--print-turbolev-frontend`
  - `--trace-turbo-graph`
  - `--print-opt-code`, `--print-code`
- **Tiering & Lifecycle Events:**
  - `--trace-opt`, `--trace-deopt`, `--trace-deopt-verbose`
  - `--trace-ic`, `--trace-prototypes`
  - `--trace-gc`, `--trace-alloc`
- **Fallback / Raw Developer Flags:**
  - Standard support for any developer flag defined in V8's `flag-definitions.h` via generic ANSI-preserving log streams.

---

## 3. Data Model & Navigation Hierarchy

### 3.1. Primary Hierarchy: `Function * Tier` -> `Phase` -> `View`

Rather than nesting compilations inside function subtrees, `garage` flattens the top-level list into distinct **`Function * Tier` compilation instances**, sorted chronologically:

```
[1] foo() @ script.js:12  [Ignition Bytecode]
[2] foo() @ script.js:12  [Maglev Compilation #1]
     ├── Phase 1: After graph building
     ├── Phase 2: After SSA optimization
     ├── Phase 3: After truncation
     ├── Phase 4: After Phi untagging
     ├── Phase 5: After register allocation
     └── Phase 6: Code Generation (Disassembly)
[3] bar() @ script.js:45  [Maglev Compilation #1]
[4] foo() @ script.js:12  [Maglev Deopt → Eager]
     └── Lifecycle & Deopt Reason
[5] foo() @ script.js:12  [Turboshaft Compilation #2]
     ├── Phase 1: BuildGraphPhase
     ├── Phase 2: MachineOptimizationPhase
     └── Phase 3: InstructionSelectionPhase
```

### 3.2. Timeline View (`[T] Chronological Event Log`)
For events without compilation graphs (e.g. `--trace-opt`, `--trace-deopt`, IC updates), or to inspect the sequence of execution across time:
- A global **Timeline View** lists all events sequentially with timestamp / relative ordinal steps (`#001`, `#002`, ...).
- Selecting a Timeline event jumps directly to the corresponding `Function * Tier` phase or deopt report.

---

## 4. Key TUI Features & User Interactions

### 4.1. Graph View & Semantic Node Highlighting
When viewing a compiler graph (e.g., Maglev IR or Turboshaft IR):
- **Hover / Node Selection (e.g., `n55` / `v12`):** Selecting or placing the cursor on a node automatically highlights:
  - **Node Definition:** The line where the node is produced.
  - **Inputs / Predecessors (Def-Use):** Highlight all nodes referenced as inputs to `n55`.
  - **Consumers / Successors (Use-Def):** Highlight all downstream nodes that consume `n55`.
- **Node Jumping / Breadcrumbs:**
  - Press `i` to jump to the input source node.
  - Press `u` to cycle through downstream user nodes.
  - Press `Ctrl+O` / `Ctrl+I` to jump back/forward in node history.
- **Default Syntax Highlighting:**
  - ANSI colors for opcodes (`Int32Add`, `GapMove`, `Phi`, `Branch`), register assignments (`rax`, `rdi`), type feedback annotations, and block labels (`b0`, `b1`).

### 4.2. Split-Screen & Structural Diff Mode
- **Split Screen (`v` / `s`):** Split the terminal vertically or horizontally to view two compiler phases side-by-side (e.g., *Phase 1: Graph Building* vs *Phase 4: Register Allocation*).
- **Phase Diff (`d`):** Compare Phase N with Phase N+1:
  - Highlight newly inserted instructions/nodes in green.
  - Highlight removed/folded nodes in red.
  - Highlight modified annotations/registers in yellow.

### 4.4. JS Source & Bytecode Alignment (Sparkplug & Maglev)
- **Side-by-Side Source View (`S`):** Open a split pane displaying the original JavaScript source file or Ignition Bytecode stream.
- **Cross-Highlighting:** Selecting a Maglev node (e.g., `n42 @ offset 18`) highlights the exact **Bytecode instruction** (`Ldar a0`) and the corresponding **JavaScript line** (`return a + b;`).

### 4.5. Graph Topology & Basic Block Controls
- **Basic Block Folding (`Space`):** Expand or collapse basic block containers (e.g. `b0`, `b1 (Loop)`) to collapse noisy blocks and focus strictly on loop headers or function exits.
- **Node Type Filtering:**
  - Type `:phi` to hide standard arithmetic nodes and inspect only the control-flow & Phi node backbone.
  - Type `:check` to highlight and count all map/type guards (`CheckMaps`, `CheckSmi`, `CheckBounds`).
  - Type `:spill` to view register allocation spills and reloads.

### 4.6. Summary Telemetry Header & Command Palette
- **Engine Telemetry Bar:** Displayed at the top of the terminal viewport:
  ```
  ┌─ garage v8 trace ──────────────────────────────────────────────────────────┐
  │ Compilations: 14 (Maglev: 10 | Turboshaft: 4) │ Deopts: 2 │ GC Events: 3  │
  └────────────────────────────────────────────────────────────────────────────┘
  ```
- **Vim-style Command Palette (`:`):**
  - `:deopts` — Jump directly to the next deoptimization.
  - `:spills` — Highlight all register allocation spills.
  - `:checks` — Highlight all type guard checks.
  - `:copy` — Copy current phase snippet to system clipboard.

### 4.7. Dual-Run Diffing Mode (`garage baseline.log patched.log`)
- Compare the output of **two separate `d8` trace files** (e.g., before and after a compiler optimization CL):
  - **Compilation Count Diff:** Compare total compilations and deopts side-by-side.
  - **Phase Output Diff:** Compare *Phase N* in `baseline.log` vs `patched.log` to verify if nodes were eliminated or transformed as expected.

### 4.8. Interactive Disassembly & Register Lifetime View (`A`)
- **Assembly Inspection:** When viewing disassembly (`--print-opt-code` or CodeGen phases), hover over branch/jump instructions (`jmp .Lentry_1`, `b.eq 0x...`) to highlight target code labels.
- **Register Lifetime Overlay:** Hovering over a register (e.g. `rax` on x64 or `x0` on ARM64) highlights all instructions in the phase where that register is read, written, or modified.
- **Relocation Pointer Decoding:** Inline decoding of tagged memory addresses (e.g. `[rax+0x17]` decoded into `[Field: Map]`).

### 4.9. Inlining Hierarchy & Decision Tree (`I`)
- **Visual Inlining Tree:** Press `I` to open a hierarchical tree view of all inlined function calls:
  ```
  main() @ script.js:5
   ├── inlined helper() @ script.js:42 (Cost: 12, Inlined)
   │    └── inlined clamp() @ utils.js:100 (Cost: 4, Inlined)
   └── rejected heavyWorker() @ script.js:88 (Reason: target too large)
  ```
- **Inlined Subgraph Filtering:** Selecting an inlined callee filters the graph view to show only the basic blocks belonging to that specific inlined function.

### 4.10. Feedback Vector Heatmap & Polymorphism Alerts
- **Color-Coded IC Stability:** Annotate Inline Cache operations in Bytecode/Maglev views:
  - 🟢 **Green (Monomorphic):** Fast single-map path.
  - 🟡 **Yellow (Polymorphic):** Multi-map dispatch.
  - 🔴 **Red (Megamorphic / Generic):** Slow path stub call / dictionary lookup.
- **Query Filter:** `:megamorphic` filter to highlight all slow-path IC calls across the trace.

### 4.11. Deopt Frame Unwinding Panel
- **Reconstructed Stack Frame:** When inspecting an eager or soft deoptimization (`--trace-deopt-verbose`), `garage` renders the simulated Interpreter stack frame at the exact moment of deopt:
  ```
  ┌─ Deopt Frame #0: processArray() @ offset 14 ─────────┐
  │ Register r0: 42 (Unboxed Int32)                      │
  │ Register r1: HeapObject (Map 0x3f... HeapNumber)    │
  │ Stack Slot 0: Undefined                              │
  └──────────────────────────────────────────────────────┘
  ```

---

## 5. Keyboard & Interaction Map

| Key | Action |
| :--- | :--- |
| `?` / `h` | **Open Help Modal** (Displays interactive shortcut & command popup) |
| `j` / `k` or `↑` / `↓` | Navigate list / scroll viewport |
| `h` / `l` or `←` / `→` | Switch focus between Sidebar (List) and Main Viewport |
| `Enter` | Select Function/Phase |
| `Tab` | Switch between Phase List view and Global Timeline view |
| `v` | Toggle Vertical Split Screen |
| `d` | Toggle Side-by-Side Phase Diff Mode |
| `S` | Toggle JavaScript / Bytecode Alignment Pane |
| `A` | Toggle Disassembly & Register Lifetime View |
| `I` | Toggle Inlining Tree & Decision Panel |
| `Space` | Fold / Unfold Basic Block |
| `i` / `u` | Jump to Node Inputs / Consumers |
| `Ctrl+O` / `Ctrl+I` | Back / Forward in node navigation history |
| `/` | Regex search in current view |
| `:` | Open Command Palette (`:deopts`, `:phi`, `:check`, `:spill`, `:megamorphic`, `:copy`) |
| `n` / `N` | Next / Previous search match |
| `f` | Quick filter sidebar entries |
| `q` / `Esc` | Exit view / Close modal overlay |

---

## 6. Implementation Strategy & Technology Stack

### 6.1. Why Rust + `ratatui`?
- **Performance & Memory Efficiency:** V8 compilation logs can easily grow to hundreds of megabytes. Rust's zero-cost abstractions and stream parsing allow `garage` to index logs at gigabytes-per-second with minimal memory footprint.
- **Single Static Binary Distribution:** Compiles to a single zero-dependency static binary (`garage`) that developers can drop into `PATH` or depot tools.
- **Ecosystem Standard:** `ratatui` (with `crossterm`) is the standard framework for modern, high-performance terminal applications (e.g. `yazi`, `zellij`, `gitui`).

### 6.2. Phase Marker & Parser Maintenance Strategy
To ensure `garage` remains resilient when V8 engineers add or rename compiler phases:
1. **Structural Heuristics + Specific Regex:** `garage` uses hierarchical regexes (e.g., matching `--- <Pass Name> ---` or `== Phase: <Name> ==`). Even if a phase name changes, the visual banner format in V8 output is preserved.
2. **Dynamic Grammar Configuration:** Phase markers and section delimiters are stored in an embedded configuration file (TOML format), allowing new flags and formats to be added without code refactoring.
3. **Graceful Fallthrough View:** Unrecognized logs or phase titles are never lost; they are seamlessly placed into generic searchable sections without breaking the navigation tree.
4. **CI Integration Tests:** Run `garage` against sample `d8` outputs in V8 CI to detect breaking parser changes early.

---

## 7. User Journeys

### Journey 1: Investigating Maglev Graph Optimization Passes
> **Goal:** Developer runs `d8 --print-maglev-graphs bench.js` and wants to see how phi untagging transformed a loop.

1. **Invocation:** Developer runs `d8 --print-maglev-graphs bench.js | garage`.
2. **Initial View:** `garage` opens with the left sidebar showing all `Function * Tier` compilations.
3. **Selection:** Developer chooses `compute() [Maglev Comp #1]`.
4. **Phase Selection:** Developer selects `Phase 4: After Phi untagging`.
5. **Split View:** Developer presses `v` and selects `Phase 1: After graph building` on the left pane.
6. **Diffing:** Developer presses `d` to highlight diffs; instantly sees which `Phi` nodes were untagged to `Int32` representations.
7. **Node Trace:** Developer moves cursor over node `n32` (an untagged Phi). The TUI highlights all inputs to `n32` across loop header blocks.

---

### Journey 2: Debugging a Sudden Performance Drop (Deopt Analysis)
> **Goal:** A function `processArray` experienced an eager deoptimization. Developer wants to trace from Deopt back to Maglev IR.

1. **Invocation:** Developer runs `d8 --trace-deopt --trace-opt --print-maglev-graphs app.js | garage`.
2. **Timeline View:** Developer presses `Tab` to open the Global Timeline View.
3. **Locating Deopt:** Developer sees a red `[DEOPT EAGER]` event for `processArray() @ app.js:88` at timestamp `42.1ms`.
4. **Inspecting Deopt:** Developer presses `Enter` on the Deopt event. `garage` displays the deopt reason: `wrong map / eager deopt at bytecode offset 14`.
5. **Jumping to IR:** Developer presses `g` ("Go to Graph"). `garage` jumps directly to `processArray() [Maglev Comp #1]` -> `Code Generation` at the exact instruction corresponding to bytecode offset 14.

---

### Journey 3: Streamed Live Iteration
> **Goal:** Developer is tweaking a JS benchmark and wants live trace feedback as `d8` executes.

1. **Invocation:** Developer runs `garage -- d8 --print-maglev-graphs --trace-deopt test.js`.
2. **Streaming:** As `d8` executes, `garage` populates the sidebar live with `Function * Tier` entries as compilations finish.
3. **Live Filter:** Developer types `:deopt` to quickly verify if any deoptimizations occurred during the run.

---

### Journey 4: Dual-Run Optimization Verification (CL Comparison)
> **Goal:** Developer wrote a V8 patch to eliminate redundant `CheckMaps` in Maglev and wants to compare baseline vs patched output.

1. **Invocation:** Developer runs `garage baseline.trace patched.trace`.
2. **Side-by-Side Comparison:** `garage` loads both trace files in a split view.
3. **Command Filtering:** Developer types `:checks` on both sides.
4. **Verification:** `garage` highlights that `CheckMaps` count dropped from 14 to 3 in `patched.trace`.

---

## 8. Future Roadmap

### 8.1. Near-Term Future (Soon)
- **IR Node Lineage & Ancestry Tree (`L`):**
  - Turboshaft compilation graphs undergo 10–20 transformation passes (e.g. *BuildGraphPhase -> TypeInferencePhase -> MachineOptimizationPhase -> MachineLoweringPhase*).
  - Pressing `L` on any node in Phase N renders a complete **Origin Lineage Tree** tracing node creation backward:
    ```
    Phase 15 (MachineLowering):     n105 [Int32Add]
     └── Phase 8 (TypeInference):   n88  [TruncateFloat64ToInt32]
          └── Phase 1 (BuildGraph): n12  [LoadField]
               └── Bytecode Offset 18: Ldar a0 @ bench.js:24
    ```
  - Allows single-keypress navigation backward and forward through a node's historical transformations across phases.
- **Deopt Guard Node 1-Click Jump (`g`):**
  - When inspecting a Deopt event in the Timeline View (`Tab`), pressing `g` jumps directly to the exact failing IR Guard Node (`CheckMaps`, `DeoptimizeIf`) in Maglev or Turboshaft.
  - Automatically displays the expected vs received Map ID mismatch (`Map 0x123...` vs `Map 0x567...`) and cross-highlights the JS source line that mutated the object shape.
- **Representation Transition Visualizer (`T`):**
  - Visualizes number representation unboxing and boxing (`Tagged -> Int32`, `Float64 -> Tagged`) across compiler passes.
  - Automatically highlights **Boxing Ping-Pong** performance traps where a value is repeatedly boxed onto the heap and unboxed inside a loop block.
- **Register Pressure Heatmap Bar (`R`):**
  - Renders a color-coded Register Pressure Heatmap (level 1–10) next to basic blocks in Maglev disassembly/regalloc output.
  - High-activity blocks are highlighted in red/orange, allowing developers to filter for `Spill` and `Reload` gap moves instantly.
- **Inline Cache (IC) State Machine Visualizer:**
  - Interactive progression timeline for property access IC slots:
    ```
    [Property: .x @ script.js:14]
    Uninitialized ──► Monomorphic (Map A) ──► Polymorphic ({Map A, Map B}) ──► Megamorphic
        (0.1ms)             (1.4ms)                    (5.2ms)                   (12.0ms)
    ```
- **Turboshaft Reducer Delta Inspector:**
  - Highlights which specific Turboshaft Reducer (e.g. `CopyEliminationReducer`, `ValueNumberingReducer`, `MemoryLoweringReducer`) inserted, modified, or deleted a node within a phase.

### 8.2. Medium-Term Future
- **Scriptable Filters & Custom Checks (`garage --check`):**
  - Allows developers to pass custom JS/Lua rule scripts (e.g. `garage app.trace --check "find_nodes('CheckMaps').inside_loop()"`) to automatically audit trace files for performance anti-patterns.
- **Terminal ASCII Graph Minimap & Search Radar Bar:**
  - High-density Braille/ASCII minimap strip rendered on the right border of the terminal viewport for 1,000+ line graph dumps.
  - Displays color-coded indicators for loop headers (blue), deopts (red), and active search match hits (yellow).
- **Bookmarks, Sticky Notes & Markdown Export (`m` / `c`):**
  - Bookmark nodes (`ma`), jump back (`'a`), add inline sticky notes (`c`), and export annotated sessions to formatted Markdown (`:export report.md`) for Buganizer tickets (`b/...`) or Gerrit CL comments.
- **`v8.log` (`--log-**`) Support:**
  - Ingest binary/csv profiler tick logs and event streams (`--log-all`, `--log-code`, `--prof`) to visualize tick samples and IC state changes alongside graph dumps.
- **Turbolizer `.json` File Import:**
  - Parse and render JSON graph files exported by `--trace-turbo` directly in terminal mode.

### 8.3. Far-Term Future
- **Escape Analysis & Virtual Object Panel (`E`):**
  - Displays escape analysis status for all heap allocations in a function:
    - 🟢 **Scalar Replaced:** Allocations eliminated; fields kept in registers/stack.
    - 🟡 **Materialized on Deopt:** Stays virtual unless an eager deopt occurs.
    - 🔴 **Escaped:** Object allocated on heap (escaped scope or un-inlined call).
- **Isolate & Multi-Thread Selector (`W`):**
  - Un-tangles interleaved multi-threaded logs (Web Workers, main thread, background concurrent compiler threads) by filtering trace items by Isolate pointer (`0x5555...`) or Thread ID.
- **Node Motion & LICM Hoisting Tracer (`M`):**
  - Select a node in Phase N and trace where it originated, highlighting nodes moved across basic block boundaries during Loop Invariant Code Motion (e.g., *Node n18 moved from b2 [Loop Body] to b1 [Pre-Header] in Phase 3*).
- **Graph Projection Modes (`P`):**
  - Toggle graph viewport density:
    - **Raw IR:** Full V8 output.
    - **Simplified IR:** Hides constant loads (`Int32Constant`) and parameter definitions.
    - **Dataflow-Only:** Hides control nodes (`Goto`, `Branch`, `Merge`) to focus on calculation flow.
- **Interactive `d8` Flag Switcher & Live Re-runner (`r`):**
  - When running in wrapper mode (`garage -- d8 script.js`), press `r` to open an in-TUI flag prompt, edit flags (e.g. `--no-maglev`), and re-run `d8` live without quitting to bash.
