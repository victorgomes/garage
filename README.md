# garage

A terminal UI for V8 engineers to view, navigate, and search `d8` trace
output — Maglev graphs first — without dumping megabytes into scrollback or
switching to browser tools. The tool `--print-maglev-graphs | less` should
have been.

```
d8 --print-maglev-graphs --trace-deopt bench.js | garage
garage trace.log
```

Opens a 490 MB trace in ~0.5 s. Sections are indexed on load; compilations
parse lazily on first view; nothing is ever dropped — anything the parser does
not recognise stays visible as raw text with an `[unparsed]` badge.

## Install

```sh
cargo build --release        # rustc 1.85+, single static binary
cp target/release/garage ~/bin/
```

## Getting a useful trace

```sh
# The MVP invocation:
d8 --print-maglev-graphs --trace-deopt bench.js | garage

# Cleaner, diffable traces (recommended for graph work):
d8 --no-concurrent-recompilation --predictable \
   --print-maglev-graphs bench.js | garage

# Richer deopt frames (register → node → location):
d8 --print-maglev-graphs --print-maglev-deopt-verbose bench.js | garage

# Tiering + OSR story alongside the graphs:
d8 --print-maglev-graphs --trace-opt --trace-deopt --trace-osr app.js | garage
```

Notes that save an afternoon:

- `--print-maglev-graphs` and friends exist only in builds with
  `V8_ENABLE_MAGLEV_GRAPH_PRINTER` (on by default in `debug`; check your GN
  args otherwise). Without it they silently print nothing.
- `--print-opt-code` additionally needs `v8_enable_disassembler = true`.
- `--trace-deopt-verbose` *implies* `--print-maglev-deopt-verbose` and changes
  the graph output as a side effect — flag implications are real.
- Use `--no-trace-with-compilation-id` when you want two runs to be
  comparable; it strips the volatile `[ML:<id>]` prefixes.
- With concurrent recompilation on (the default), output from several threads
  interleaves; garage degrades the un-demultiplexable parts to raw sections.
  `--no-concurrent-recompilation --predictable` avoids that entirely.

## Keys

| Key | Action |
| :-- | :-- |
| `?` | help (generated from the live keymap) |
| `j`/`k`, `↑`/`↓` | move |
| `h`/`l`, `←`/`→` | focus sidebar ⇄ viewport |
| `Enter` | expand/collapse compilation, function group |
| `Ctrl+D`/`Ctrl+U`, `PgDn`/`PgUp`, `g`/`G` | paging, top/bottom |
| `c` | sidebar: chronological ⇄ grouped by function |
| `f` | filter sidebar by regex |
| `/`, `n`, `N` | regex search (incremental), next/previous match |
| `Space` | fold/unfold the basic block under the cursor |
| `i` | jump to input definitions (cycles) |
| `u` | cycle consumers of the node the cycle started from |
| `Ctrl+O` / `Ctrl+I` | jump history back / forward |
| `t` | show/fold interleaved trace annotations |
| `y` / `Y` | copy cursor line / whole section (OSC 52 over SSH/tmux) |
| `E` | export the current view as Markdown |
| `w` | wrap long lines · `<`/`>` scroll horizontally |
| `F` | follow the stream end (on for piped input) |
| `Tab` | next source (multi-file runs) |
| `q`, `Esc` | quit / back |

Every binding is remappable in `~/.config/garage/config.toml` (or
`--config <path>`):

```toml
[keys]
quit = ["x"]              # frees q
half-page-down = "Ctrl+f"
```

A key bound to two actions is a startup error, and a config typo is a normal
error message — the alternate screen only opens once the config is valid.

## What the UI shows

- **Telemetry bar** — compilations per tier, OSR count, deopt count, detected
  V8 version (inferred from the phase-banner vocabulary).
- **Sidebar** — compilations (`name · tier · #ordinal`, OSR and `[unparsed]`
  badges) interleaved with raw sections, chronological or grouped by function
  (grouped mode keys on the SFI address, so an empty function name is fine).
- **Viewport** — the trace itself, styled from the parse: guards red, control
  flow blue, constants magenta, phis yellow, interleaved source bytecode dim.
  The cursor node's definition, inputs, and consumers highlight in distinct
  colours; deopt frames show which registers hold which nodes.

## CLI

```
garage [FILE...]              files (mmapped; several files = Tab to switch)
d8 ... | garage               piped stdin; keyboard input still works
garage --function '^process'  index-time prefilter on function names
garage --debug                write diagnostics to garage.log (--log-file)
```

## Development

Real `d8` output is checked in under `fixtures/` (three builds: two V8
versions × two architectures) and every parser change runs against it:

```sh
cargo test                          # unit + golden + fuzz tests
UPDATE_GOLDENS=1 cargo test --test golden   # after an intentional change
git diff tests/golden/              # review exactly what changed
```

`docs/` has the design records: `spike-findings.md` (measured V8 output
behaviour the parsers encode), `printer-parser-contract.md` (which V8 source
prints what), `correlation-keys.md` (how deopt events map to compilations).
`PLAN.md` and `TODO.md` carry the design and the phase history.

Verified on macOS (Terminal.app/tmux, 80×24 up to full screen, 16 and 256
colour). Linux and SSH are expected to work but not yet measured — the fd-0
terminal handling in `src/tty.rs` documents the one platform-specific piece.
