# Solana Recorder CTFS Audit — 2026-05-02

This audit checks `codetracer-solana-recorder` against the canonical
CodeTracer multi-stream CTFS schema and the section 5.6 audit checklist
maintained in `/tmp/isonim-migration.txt`.  Prior audits set the canonical
patterns: Ruby (1.21, 1.22), Python (1.27), JavaScript (1.38), EVM (1.39)
and PHP (1.41).

The Solana recorder consumes register-trace data from the SBF VM
(`solana-sbpf`), correlates it with DWARF source mapping, and writes
CodeTracer trace files.  It uses the **Rust-native NimTraceWriter**
(`codetracer_trace_writer_nim` crate, not the C FFI) — so it has access
to every canonical entry point (`register_call`, `arg`,
`register_special_event`, `register_thread_*`).  It does **not** suffer
from the C-FFI gaps documented in section 5.6 from the PHP audit.

## Summary

| # | Check | Status (pre-fix) | Status (post-fix) | Notes |
|---|---|---|---|---|
| a | `register_call` for each call | OK | OK | Three call sites: top-level `main`, internal-call heuristic on PC jumps, CPI-target.  No `add_event(Call(..))` calls anywhere. |
| b | Call args via `register_call_arg` / `arg` | **GAP** | **PARTIAL** | Pre-fix: every `register_call` site passes `vec![]` and there are zero `arg()` calls.  Post-fix: CPI call sites now stage `target_program` and `target_pc` via `arg()` before `register_call`.  Top-level `main` and internal-call heuristic sites still pass `vec![]` — main has no callsite-visible args, and the internal-call heuristic recovers function entries from PC deltas (no symbolic SBF stack analysis is done yet, the Solana equivalent of EVM's open StackTracker gap). |
| c | Write/WriteOther for stdout/stderr via `register_special_event` | **GAP** | **OK (post-fix)** | Pre-fix: `CodeTracerTracer::on_syscall` accumulated names into `Vec<String>` for testing only; no event was emitted to the trace.  Post-fix: `sol_log` family routes to `EventLogKind::Write` (the canonical stdout bucket — same convention used by Ruby/Python recorders for stdout, see 1.21 / 1.27).  Other syscalls (`sol_invoke_signed`, account creation, etc.) route to `EventLogKind::TraceLogEvent` so they appear as structured diagnostic events without polluting the program-log pane.  Note: the SBF VM in this recorder does not yet *invoke* `on_syscall` (the wiring is at the trait API only — see "open path" below). |
| d | Thread events (ThreadStart / Exit / Switch) | OK | OK | Solana SBF programs are single-threaded by construction; the recorder correctly emits no thread events. |
| e | Step records for line navigation | OK | OK | All three recording paths (`record_from_snapshots_into_writer`, `record_with_cpi`, `CodeTracerTracer::on_step`) call `register_step(path, line)` on every source-line change. |
| f | Canonical CTFS schema match | **GAP** | **OK (post-fix)** | Pre-fix: CLI `--format` defaulted to `binary` (legacy CBOR+Zstd) and the `OutputFormat` enum did not even expose `Ctfs` (`Binary` / `Json` only).  Post-fix: CLI exposes a `Ctfs` variant, defaults to it, and the `OutputFormat → TraceEventsFileFormat` mapping passes it through.  This is the same legacy-format issue the EVM audit (1.39) caught and the most important file-format fix per section 5.6 cross-cutting findings. |
| g | Obsolete `#[no_mangle]` stubs | OK | OK | None present — `grep -r '#\[no_mangle\]' src/` returns nothing.  This recorder predates the JS-recorder pattern that introduced the FFI stubs that conflicted with upstream Nim exports. |
| C-FFI vs native | OK | OK | Uses the Rust-native `codetracer_trace_writer_nim` crate (sibling-path dep) — every canonical API is reachable.  No FFI-extension blockers. |

## Concrete fixes applied

### 1. CLI now exposes and defaults to `Ctfs`

`src/main.rs` `OutputFormat` enum now has three variants (`Ctfs`,
`Binary`, `Json`) instead of two (`Binary`, `Json`).  Both `RecordArgs`
and `ReplayArgs` `--format` flags default to `ctfs`.  The
`OutputFormat → TraceEventsFileFormat` conversion passes the new variant
through.

Pre-fix:

```rust
#[derive(Debug, Clone, ValueEnum)]
enum OutputFormat {
    Binary,
    Json,
}
// ...
#[arg(short = 'f', long = "format", default_value = "binary")]
format: OutputFormat,
```

Post-fix: `Ctfs` is the first variant, the default, and is documented as
the canonical multi-stream container consumed by `NimTraceReaderHandle`
and the db-backend `CTFSTraceReader`.

The `events_filename = match format { ... }` matches in `recorder.rs`
and `tracer_trait.rs` were already future-proofed (they had a
`Binary | BinaryV0 | Ctfs` arm yielding `"trace.bin"`), so no further
file-naming work was needed at the recorder level.

### 2. CPI call-arg staging

`src/recorder.rs`'s `record_with_cpi` now stages `target_program` (string)
and `target_pc` (u64) as call args via `TraceWriter::arg(...)` before
emitting the CPI `register_call`.  This mirrors the Ruby (1.22) and JS
(1.38) call-arg staging fix pattern.  A `String` type id is registered
once per CPI call (the writer dedups on subsequent calls).

```rust
let pc_type_id =
    TraceWriter::ensure_type_id(&mut *writer, TypeKind::Int, "u64");
let str_type_id =
    TraceWriter::ensure_type_id(&mut *writer, TypeKind::String, "string");
let _ = TraceWriter::arg(&mut *writer, "target_program",
    ValueRecord::String { text: program_name_str.to_string(),
                          type_id: str_type_id });
let _ = TraceWriter::arg(&mut *writer, "target_pc",
    ValueRecord::Int { i: target_pc as i64, type_id: pc_type_id });
TraceWriter::register_call(&mut *writer, cpi_fn_id, vec![]);
```

This makes the calltrace pane display the CPI target program identifier
and entry PC for every cross-program invocation.  See "Open: internal
call args" below for the remaining call-arg gap.

### 3. Syscall events routed through `register_special_event`

`src/tracer_trait.rs`'s `CodeTracerTracer::on_syscall` previously only
accumulated syscall names into `recorded_syscalls: Vec<String>` (an
in-memory test-only buffer).  Post-fix it also emits a special event so
the syscall appears in the CodeTracer "event log" pane:

```rust
let kind = if name.starts_with("sol_log") {
    EventLogKind::Write          // stdout-equivalent
} else {
    EventLogKind::TraceLogEvent  // structured trace-only event
};
TraceWriter::register_special_event(&mut *self.writer, kind, name, "");
```

Solana program logs are emitted through `sol_log` and the `sol_log_*`
syscall family — these are the SBF equivalent of stdout writes, so the
canonical mapping is `EventLogKind::Write` (the same bucket used by the
Ruby/Python recorders, per handoff entries 1.21 and 1.27).  Other
syscalls (`sol_invoke_signed`, `sol_create_program_address`, etc.) are
non-IO control events and route to `EventLogKind::TraceLogEvent` so they
appear as structured trace-only events without showing up in the
terminal-output pane.

The `recorded_syscalls` buffer is preserved for backward compatibility
with the existing test suite that asserts on syscall names.

## Tests run

`cargo test --release` (after fixes):

- `lib` unit tests: 6/6 passing.
- `test_account_decoder`: 17/17 passing.
- `test_cli`: 6/6 passing.
- `test_comprehensive` (the largest suite): 48/48 passing.
- `test_cpi`: 6/6 passing.
- `test_cpi_execution`: 2/5 passing (3 pre-existing failures —
  `partition_functions_for_cpi` requires a built test program with ≥4
  functions that is not present in the dev tree; **unrelated to this
  audit, present at baseline**).
- `test_dwarf`: passing.
- `test_execution_trace`: passing.
- `test_replay`: passing.
- `test_tracer`: passing.
- `test_tracer_trait`: passing.

No regressions introduced.  The 3 pre-existing CPI-execution failures
existed before the audit and are environmental (missing built fixture),
not a recorder defect.

## Open gaps / follow-ups

### Call args for the internal-call heuristic (`b`)

The internal-call heuristic in `record_from_snapshots_into_writer`,
`record_with_cpi` (same-program branch), and `CodeTracerTracer::on_step`
detects call/return from raw PC deltas (`diff > 2`) and emits
`register_call(callee_fn_id, vec![])` with no args.  Recovering
parameter values would require:

1. Parsing the SBF function prologue at the callee PC to determine the
   ABI argument register / stack layout, OR
2. A pluggable hook from the SBF VM that supplies parameter metadata
   (the `tracer_trait::SbpfTracer::on_step` hook gets all 12 registers
   already; what's missing is symbolic metadata that says "r1 is param
   `lamports`, r2 is param `seeds`").

This mirrors the EVM recorder's open `Call.args` gap (1.39).  Concrete
fix shape would be: at each detected call boundary, walk the DWARF
`DW_TAG_formal_parameter` entries for the callee `DW_TAG_subprogram`,
read the `DW_AT_location` descriptions to derive register / stack-offset
locations, and stage each `(name, value)` via
`TraceWriter::arg(...)` from the live registers/memory at the call PC.
Solana SBF uses the standard SystemV-like ABI: r1..r5 for the first 5
arguments, additional args via stack frame.

Filed as follow-up; not blocking the audit.

### `on_syscall` is not yet invoked by the recorder's executor (`c`)

The `CodeTracerTracer::on_syscall` event path is now wired correctly,
but the recorder's `executor::execute_with_tracing` does NOT yet plumb
the SBF VM's syscall callbacks into the `SbpfTracer` trait — the
executor uses the raw `EbpfVm::execute_program` API and post-processes
register snapshots, so syscalls are never observed.  The trait API
exists (`SbpfTracer::on_syscall`) and is exercised by unit tests
(`test_codetracer_tracer_records_syscalls`), but the live recording
path bypasses it.

To complete the syscall→event-log pipeline end-to-end, the executor
would need to either:

1. Switch from `execute_program` to a manually-driven step loop that
   inspects each instruction for syscall dispatches (`call imm` with
   SBPF syscall numbers) and invokes `tracer.on_syscall(name, regs)`
   between steps, OR
2. Register a custom syscall handler with `BuiltinProgram` that wraps
   each builtin to forward the call to the tracer before delegating.

This is a larger refactor of `executor.rs` and is tracked separately —
the audit-level fix establishes the canonical event mapping so the
plumbing work has a clean target.

### Replay path uses synthetic register trace

`replay::replay_transaction` constructs a single-step synthetic register
trace per instruction (PC = instruction index, all other registers
zeroed) because real on-chain SBF replay requires Mollusk / a patched
SBF VM (TODO marked in the source).  The recorder pipeline produces a
valid (minimal) trace from this stub, but no real register data /
syscalls are observed.  Not a CTFS-format issue — this is a Solana
upstream replay infrastructure gap.

### `CpiContext.program_name` defaults to `"primary"`

`CpiDetector::new` initialises the bottom-of-stack context with the
hard-coded program name `"primary"` instead of the actual Pubkey of
the entry program.  The CPI call-arg `target_program` value is therefore
the literal string `"primary"` for the outer-most program.  Plumbing the
real Pubkey through is a small follow-up; not a CTFS-format issue.

### Multi-stream IO event collapse (cross-cutting infrastructure issue)

As documented in section 5.6 of `/tmp/isonim-migration.txt`
("New issues uncovered"), the Nim multi-stream IO event stream's
`toIOEventKind` collapse drops most of the 13 `EventLogKind` variants
into 4 buckets (`stdout`, `stderr`, `fileOp`, `error`).  In that
collapsed view, `TraceLogEvent` (used for non-`sol_log` Solana syscalls
above) lands in the `stderr` bucket, alongside `EvmEvent` from the EVM
recorder.  This is acceptable for the audit (`sol_log` lands cleanly in
the `stdout` bucket), but a follow-up to preserve the original
`EventLogKind` byte through the multi-stream format would let the
frontend distinguish Solana control events from terminal stderr.

This is an infrastructure change in
`codetracer-trace-format-nim/src/codetracer_trace_writer_ffi.nim` — out
of scope for any single recorder audit.

---

## Convention compliance follow-up — 2026-05-08

Mirrors the cairo / cardano / circom / flow / fuel / leo / miden / move /
polkavm follow-ups: the recorder is now CTFS-only at the CLI surface, with
the canonical `CODETRACER_<NAME>_RECORDER_OUT_DIR` /
`CODETRACER_<NAME>_RECORDER_DISABLED` env-var contract from
`Recorder-CLI-Conventions.md` §4 / §5.

### CLI changes

* `--format` / `-f` removed from both subcommands (`record`, `replay`).
  Clap now rejects the flag at every level — exercised by
  `tests/test_cli.rs::test_format_flag_rejected_by_clap`.
* `OutputFormat` enum (and its `OutputFormat → TraceEventsFileFormat`
  mapping) deleted from `src/main.rs`.
* `RecordArgs.out_dir` / `ReplayArgs.out_dir` changed from `PathBuf`
  (with `default_value = "./ct-traces/"`) to `Option<PathBuf>`.  A new
  `resolve_out_dir` helper resolves `--out-dir` →
  `CODETRACER_SOLANA_RECORDER_OUT_DIR` → `./ct-traces/` in priority
  order.
* New `recording_disabled()` helper reads
  `CODETRACER_SOLANA_RECORDER_DISABLED` (`1` / `true`); each subcommand
  short-circuits with a "skipping trace recording" note when it is set.
* `--help` text now points users at `ct print` from
  `codetracer-trace-format-nim` for human-readable conversion.

### Library / tracer changes

* `src/recorder.rs::record_from_traces(regs_data, elf_data, source_path,
  out_dir)` and `record_from_snapshots(snapshots, source_locations,
  source_path, out_dir)` no longer take a `format` parameter; the writer
  is pinned to `TraceEventsFileFormat::Ctfs` via the new module-level
  `CTFS_FORMAT` constant.
* `src/recorder.rs::record_with_cpi(snapshots, registry, cpi_detector,
  source_path, out_dir)` no longer takes a `format` parameter; same pin
  via `CTFS_FORMAT`.  The `events_filename` match-on-`format` collapsed
  to the unconditional `trace.bin`.
* `src/replay.rs::replay_transaction(rpc_url, signature, out_dir,
  program_dir)` no longer takes a `format` parameter; the route through
  `recorder::record_from_snapshots` is CTFS-only.
* `src/tracer_trait.rs::CodeTracerTracer::new(source_path, out_dir,
  source_locations)` no longer takes a `format` parameter; same pin via
  the module-level `CTFS_FORMAT` constant.

### Tests

* `tests/test_cli.rs` extended with the six standard convention tests:
  - `test_recorded_trace_via_ct_print_json` — records a synthetic
    7-instruction register trace through `record_from_snapshots`, pipes
    the produced `.ct` file through `ct-print --json` from
    `codetracer-trace-format-nim`, and asserts on **structural anchors**
    (the fixture path `solana_fixture.rs` and at least one of the SBF
    register names `r0..r5`).  Integer values are not asserted because
    the recorder's variable payload (`ValueRecord::Int { i, type_id }`
    over a `u64` register-type id) doesn't round-trip through
    `ct print --json` today (same pre-existing limitation as cardano /
    circom / flow / fuel / leo / miden / move / polkavm).
  - `test_env_out_dir_used_when_flag_omitted` — sets
    `CODETRACER_SOLANA_RECORDER_OUT_DIR=<tmp>` without `--out-dir` and
    asserts the env-supplied dir receives the `.ct` bundle.  Drives the
    `--regs <path>` branch (synthetic register trace + recorder's own
    binary as the ELF for DWARF) so the test does not depend on a built
    SBF program.
  - `test_env_disabled_skips_recording` — sets
    `CODETRACER_SOLANA_RECORDER_DISABLED=1` and asserts the recorder
    exits 0 with no trace artefacts written.
  - `test_format_flag_rejected_by_clap` — asserts clap rejects
    `--format json` at both subcommand levels (`record` / `replay`).
  - `test_no_format_flag_in_help` — asserts `--help` (top-level + each
    subcommand) does not advertise `--format` or `CODETRACER_FORMAT`.
  - `test_help_mentions_ct_print` — asserts top-level `--help` mentions
    `ct print` so users discover the canonical conversion tool.
* `tests/test_comprehensive.rs::test_trace_output_binary_format`
  **deleted**.  It asserted on the OLD `--format binary` contract,
  which is incompatible with the post-2026-05-08 contract (`--format`
  must not exist; CTFS is the only format).  The structural CTFS
  magic-byte assertion that test produced is already covered by
  `test_trace_output_valid_json` (renamed conceptually but kept under
  its original name) and by every other recording test that asserts on
  the `.ct` magic.
* All call-sites in `tests/test_tracer.rs`, `tests/test_cpi.rs`,
  `tests/test_cpi_execution.rs`, `tests/test_execution_trace.rs`,
  `tests/test_dwarf.rs`, `tests/test_tracer_trait.rs`,
  `tests/test_comprehensive.rs`, and `tests/test_account_decoder.rs`
  updated to call the new `format`-less recorder/tracer signatures.
  Direct `create_trace_writer(...)` usages in the
  `decoded_fields_to_struct_record` and `decoded_to_value_record` tests
  switched from `TraceEventsFileFormat::Json` to
  `TraceEventsFileFormat::Ctfs` to align with the CTFS-only philosophy.

### New artefacts

* `Justfile` — standard `build` / `test` / `lint` /
  `verify-cli-convention` / `format` recipes.  `lint` and `test` both
  run `tests/verify-cli-convention-no-silent-skip.sh`.
* `tests/verify-cli-convention-no-silent-skip.sh` — shell-side
  verification that `--format` is absent from `--help` at all three
  levels (top + record + replay), `--out-dir` / `--version` / `ct print`
  are present where the convention requires them, and the two env vars
  are referenced in `src/`.  Wired into `just lint` and `just test`.
* `README.md` — top-level usage / architecture / env-var documentation
  matching the rest of the recorder fleet.

### Verification

```
export LIBRARY_PATH=/nix/store/<…>-zstd-<…>/lib   # local libzstd workaround
cd /home/zahary/metacraft/codetracer-solana-recorder
cargo build --locked              # clean
cargo test --locked               # 142 active passing across 12 test binaries
                                  # (lib 6 + 17 account_decoder + 12 cli +
                                  #  47 comprehensive + 6 cpi + 5 cpi_execution +
                                  #  21 dwarf + 4 execution_trace + 11 replay +
                                  #  5 tracer + 8 tracer_trait)
bash tests/verify-cli-convention-no-silent-skip.sh   # 14 ok lines, 0 fails
```

`tests/test_cli.rs::test_recorded_trace_via_ct_print_json` runs end-to-end
(does not skip) inside the metacraft workspace where
`../codetracer-trace-format-nim/ct-print` exists.

### Recorder-CLI-Conventions.md

The Implementation Status table now lists Solana as `✓ Compliant
(CTFS-only)` with the standard env-var notes.
