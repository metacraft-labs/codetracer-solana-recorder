# Solana recorder examples

A small collection of self-contained Solana programs you can record
and replay end-to-end with CodeTracer. Each fixture is intentionally
tiny so the resulting trace is easy to step through in the GUI.

## Prerequisites

* `ct` on your `PATH`. `ct` is the CodeTracer CLI; it dispatches to
  the language-specific recorder (here, `codetracer-solana-recorder`)
  and opens the resulting trace bundle in the CodeTracer GUI.
* The Solana recorder built locally. From the repository root:

  ```bash
  nix develop
  cargo build
  ```

  `nix develop` provides `cargo-build-sbf` and the SBF toolchain, and
  arranges the resulting recorder binary on the same `PATH` that `ct`
  searches.

## Programs in `solana/`

| File | What it exercises |
|---|---|
| `column_aware_test.rs` | Three `let` statements on a single line — drives the column-aware step-over walkthrough below. |
| `nested_calls_test.rs` | A four-deep call chain (`compute` -> `outer` -> `middle` -> `inner`) for inspecting the call-stack panel. |
| `control_flow_test.rs` | `if` / `else if` / `match` / `while` / early return, all in one fixture. |
| `solana_flow_test.rs` | A broader language-construct tour: structs, enums, loops, bitwise ops, iterators, recursion — useful for stress-testing locals rendering. |

All four fixtures are promoted from `test-programs/solana/` and are
exercised by the recorder's integration tests, so they are guaranteed
to record cleanly.

## Two-step workflow: record, then replay

Record a trace into a `.ct` bundle:

```bash
ct record examples/solana/column_aware_test.rs
```

`ct record` invokes the Solana recorder, which compiles the program
with `cargo-build-sbf`, executes it through the SBF VM with register
tracing, resolves DWARF debug info, and writes a canonical CTFS
multi-stream bundle. The bundle path is printed on stdout — typically
`./ct-traces/<program>-<timestamp>/`.

Open the recorded bundle in the GUI:

```bash
ct replay -t ./ct-traces/column_aware_test-<timestamp>
```

You can replay the same bundle as many times as you like; it is a
self-contained artefact and does not need to be re-recorded.

## One-step workflow: record + open

For a quick poke-around, do both in one shot:

```bash
ct run examples/solana/column_aware_test.rs
```

`ct run` records into a temporary bundle and immediately hands it to
`ct replay`. This is the fastest path from source to GUI.

## Walkthrough: column-aware step-over

`column_aware_test.rs` is the smallest fixture in this directory and
the one that best showcases the recorder's column-aware stepping.
The full source is two lines:

```rust
let a = 1; let b = 2; let c = 3;
fn compute() -> i64 { let xs = (a, b, c); xs.0 + xs.1 + xs.2 }
```

Record and open it:

```bash
ct run examples/solana/column_aware_test.rs
```

The recorder forwards `(line, column)` per step on the canonical
CTFS wire (see `FlagHasColumnAwareSteps` and
`register_step_with_column` in `src/recorder.rs`). When several
statements share a source line, **step-over** stops once per
column-distinct statement.

What to look for in the GUI:

1. The first line packs three `let` bindings. Step-over stops three
   times on this line — once at column 1 (`let a`), once at column 12
   (`let b`), and once at column 23 (`let c`). A line-only recorder
   would collapse all three into a single step.
2. After each stop, the locals pane shows the newly-bound variable
   (`a = 1`, then `a, b = 1, 2`, then `a, b, c = 1, 2, 3`) — so you
   can verify each statement's effect before moving on.
3. Inside `compute`, the tuple destructure-and-sum `xs.0 + xs.1 +
   xs.2` likewise emits per-column steps for each field projection,
   which makes "next" feel precise on dense Rust expressions.

This per-column precision is what the GUI's "next" button rides on:
on packed lines it advances one statement at a time, on plain
single-statement lines it behaves exactly like a line-based stepper.

## Cross-function call chains

`nested_calls_test.rs` walks four frames deep:

```bash
ct run examples/solana/nested_calls_test.rs
```

In the GUI the call-stack panel shows `compute -> outer -> middle ->
inner`. Step **Out** of `inner` and you land back on the assignment
in `middle` with the return value (`3`) attached to the call exit.
Returns are strictly LIFO and call-entry order is sequential, which
makes this a good fixture for sanity-checking the call/return wiring.

## Where to go next

* See [`../README.md`](../README.md) for the recorder's CLI flags,
  env vars, and architecture overview.
* See `test-programs/solana/` for a broader catalogue of fixtures
  covering Anchor, CPI, PDAs, SPL token transfers, sysvars, and more
  — all recordable with the same `ct record` / `ct run` commands.
