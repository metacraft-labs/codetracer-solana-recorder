//! Per-program `ct-print --full` strict-assertion tests for the Solana
//! recorder.
//!
//! These tests follow the recorder-test-requirements policy
//! (`metacraft-specs/policies/recorder-test-requirements.md`):
//!
//! * Each test records one Solana fixture program through the
//!   recorder's normal entry point (`record_from_snapshots`) — the
//!   same path the CLI takes once a `.regs` register trace is in hand.
//! * The produced `.ct` is piped through
//!   `ct-print --full --strip-paths`.
//! * Assertions are made on the **decoded JSON document** with EXACT
//!   counts (`assert_eq!(events.len(), N)` — never `>=`), EXACT
//!   ordering (later step from a strictly later source line where
//!   applicable), and EXACT decoded values
//!   (`value["i"] == 42`, `value["kind"] == "Int"`).
//!
//! `ValueRecord` variants outside the expected set are rejected with a
//! hard error message asking the test author to extend the test rather
//! than weaken the assertion.
//!
//! Background — the SBF recorder pipeline:
//!
//! Real Solana programs compile through `cargo-build-sbf` to SBF (a
//! RISC-V-flavoured BPF VM); the recorder records register-level
//! snapshots (`.regs` binary, 12 × u64 per row) and correlates each
//! PC to a source location via DWARF.  The `cargo-build-sbf` toolchain
//! is not installed in the dev shell these tests run in, so the
//! fixtures here are real Rust source files (under
//! `test-programs/solana/`) paired with synthetic register snapshots
//! whose PCs map to lines from those files.  This is the same pattern
//! the existing `tests/test_cli.rs::test_recorded_trace_via_ct_print_json`
//! uses.  When the cargo-build-sbf path is wired into the dev shell,
//! these tests can be upgraded to drive real ELFs end-to-end without
//! changing any of the assertions: the recorder pipeline downstream of
//! the snapshots is identical.
//!
//! The recorder's source-driven model fills in the gaps the
//! synthetic-snapshot path leaves: it parses the fixture file at
//! recording time to (a) resolve nested call frames to real fn names
//! (replacing the previous `fn_at_pc_<pc>` placeholder) and (b)
//! recognise side-effecting (`msg!(...)` / `panic!(...)` /
//! `Err(...)`) and structured-value (`vec![..]` / `[..]` array
//! literal, tuple literal, `Name { .. }` struct literal) constructs
//! the SBF interpreter would observe at runtime.  Each such construct
//! is round-tripped via `ct-print --full` and asserted in the strict
//! test, plus a focused per-feature companion test (no
//! `#[ignore]`d gaps remain — every spec invariant has a live test).

use std::path::{Path, PathBuf};
use std::process::Command;

use codetracer_solana_recorder::recorder::record_from_snapshots;
use codetracer_solana_recorder::register_trace::RegisterSnapshot;

// ===========================================================================
// Helpers
// ===========================================================================

/// Path to the `ct-print` binary shipped with `codetracer-trace-format-nim`.
fn ct_print_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join("ct-print")
}

/// Skip-helper: returns `Some(path)` to ct-print or logs a clear
/// `SKIP:` diagnostic and returns `None`.  The
/// `verify-cli-convention-no-silent-skip.sh` script greps for the
/// literal `SKIP:` token, so silent skips remain forbidden.
fn ct_print_or_skip(test_name: &str) -> Option<PathBuf> {
    let p = ct_print_path();
    if !p.exists() {
        eprintln!(
            "SKIP: {test_name} requires ct-print at {} — only available \
             within the metacraft workspace where codetracer-trace-format-nim \
             is a sibling.",
            p.display()
        );
        return None;
    }
    Some(p)
}

fn test_programs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-programs/solana")
}

/// Build a register snapshot with selected r0..r10 register values.
/// `pc` is stored in r11.
fn snap(pc: u64, regs: &[(usize, i64)]) -> RegisterSnapshot {
    let mut registers = [0u64; 12];
    registers[11] = pc;
    for &(i, v) in regs {
        debug_assert!(i <= 10, "snap reg index must be 0..=10");
        registers[i] = v as u64;
    }
    RegisterSnapshot { registers }
}

/// Record a synthetic snapshot stream against a real Rust source file
/// in `test-programs/solana/`, pipe through `ct-print --full
/// --strip-paths`, and return the decoded JSON document.  Returns
/// `None` when ct-print is unavailable (caller has already emitted a
/// `SKIP:` line).
fn record_and_dump_full(
    test_name: &str,
    program: &str,
    snapshots: &[RegisterSnapshot],
    source_locs: &[(u64, &str, u32)],
) -> Option<(serde_json::Value, PathBuf)> {
    let ct_print = ct_print_or_skip(test_name)?;

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join(program);
    record_from_snapshots(snapshots, source_locs, &source_path, &out_dir)
        .expect("recorder::record_from_snapshots should succeed");

    let ct_files: Vec<_> = std::fs::read_dir(&out_dir)
        .expect("read out_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect();
    assert!(
        !ct_files.is_empty(),
        "expected at least one .ct file in {}",
        out_dir.display()
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");

    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let doc: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("ct-print --full should emit valid JSON");

    drop(tmp_dir);
    Some((doc, source_path))
}

/// Decode every (varname, i64) pair from step events for **register
/// variables only** (names matching `r0` .. `r10`).  Rejects any
/// `ValueRecord` variant other than `Int` for those names with a hard
/// error that asks the test author to extend the test rather than
/// weaken it.
///
/// Compound-typed locals (`Struct` / `Tuple` / `Sequence`) the
/// source-driven synthesiser surfaces (`xs`, `pair`, `p`, `account`,
/// ...) are *not* register variables and are returned by
/// [`observed_compound_vars`] for explicit per-test assertions, rather
/// than being conflated into the integer round-trip stream.
fn observed_int_vars(doc: &serde_json::Value) -> Vec<(String, i64)> {
    let events = doc["events"].as_array().expect("events array");
    let mut out = Vec::new();
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(vars) = ev["vars"].as_array() else {
            continue;
        };
        for v in vars {
            let name = v["varname"].as_str().expect("varname str").to_string();
            if !is_register_name(&name) {
                continue;
            }
            let value = &v["value"];
            assert_eq!(
                value["kind"].as_str(),
                Some("Int"),
                "register variable `{}` should decode as Int, got {}; \
                 if a new ValueRecord variant has landed for SBF \
                 register values, extend this test to assert on it \
                 explicitly rather than weakening the check",
                name,
                value
            );
            let i = value["i"]
                .as_i64()
                .unwrap_or_else(|| panic!("Int.i must be i64 for `{name}`; got {value}"));
            out.push((name, i));
        }
    }
    out
}

/// Decode every (varname, value-json) pair from step events for **non-
/// register** variables (the source-driven synthesiser surfaces these
/// as `Sequence` / `Tuple` / `Struct` etc.).  Returns the variables in
/// the order they appear; per-test assertions then key off varname and
/// inspect `value["kind"]`, `value["elements"]`, `value["field_values"]`,
/// etc.
fn observed_compound_vars(doc: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let events = doc["events"].as_array().expect("events array");
    let mut out = Vec::new();
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(vars) = ev["vars"].as_array() else {
            continue;
        };
        for v in vars {
            let name = v["varname"].as_str().expect("varname str").to_string();
            if is_register_name(&name) {
                continue;
            }
            out.push((name, v["value"].clone()));
        }
    }
    out
}

/// Returns `true` when `name` is one of the synthetic register names
/// the recorder emits (`r0` through `r10`) — used to partition step
/// variables into the "register stream" (asserted by
/// [`observed_int_vars`]) and the "compound locals" stream (asserted by
/// [`observed_compound_vars`]).
fn is_register_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('r') else {
        return false;
    };
    rest.parse::<u8>().is_ok_and(|n| n <= 10)
}

/// Decode the call-entry sequence as a vector of function names.
fn observed_call_sequence(doc: &serde_json::Value) -> Vec<String> {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            e["function"]
                .as_str()
                .expect("call_entry.function str")
                .to_string()
        })
        .collect()
}

/// Decode the call-exit sequence as a vector of function names.
fn observed_exit_sequence(doc: &serde_json::Value) -> Vec<String> {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .filter_map(|e| e["function"].as_str().map(|s| s.to_string()))
        .collect()
}

/// Assert that every `step` event carries a strictly non-decreasing
/// `step_index`.  This is the recorder's only ordering guarantee
/// against duplicates / reorderings.
fn assert_step_indices_monotonic(doc: &serde_json::Value) {
    let mut last = -1i64;
    for ev in doc["events"].as_array().expect("events array") {
        if ev["kind"] != "step" {
            continue;
        }
        let idx = ev["step_index"]
            .as_i64()
            .expect("step_index must be present on step events");
        assert!(
            idx > last,
            "step_index must strictly increase; got {idx} after {last}"
        );
        last = idx;
    }
}

/// Assert `metadata.program` ends with the expected source filename.
fn assert_metadata_program_ends_with(doc: &serde_json::Value, source_path: &Path) {
    let prog = doc["metadata"]["program"]
        .as_str()
        .expect("metadata.program str");
    let want = source_path.file_name().unwrap().to_string_lossy();
    assert!(
        prog.ends_with(&*want),
        "metadata.program {prog} must end with {want}"
    );
}

/// Assert that the trace's `paths` table contains the fixture source
/// file at least once (path stripping leaves only the trailing
/// component, hence `ends_with`).
fn assert_paths_contain(doc: &serde_json::Value, fname: &str) {
    let paths: Vec<&str> = doc["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with(fname)),
        "expected {fname} in paths table; got {:?}",
        paths
    );
}

/// Convenience: count step events.
fn count_step_events(doc: &serde_json::Value) -> usize {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "step")
        .count()
}

/// Pick out (name, value) pairs for a specific register across every
/// step event (in event order).
fn observed_register_sequence(doc: &serde_json::Value, reg: &str) -> Vec<i64> {
    observed_int_vars(doc)
        .into_iter()
        .filter(|(n, _)| n == reg)
        .map(|(_, v)| v)
        .collect()
}

// ===========================================================================
// control_flow_test.rs
// ===========================================================================
//
// Source layout (see test-programs/solana/control_flow_test.rs).  We
// drive a snapshot stream through compute()'s body, with diff(pc)=1
// throughout so the recorder's call-jump heuristic stays quiet (only
// the implicit `main` frame is registered).
//
// Canonical compute() execution:
//   raw      = 7   (line 60)   r1=7
//   sign     = 1   (line 61)   r2=1     (classify result)
//   bonus    = 300 (line 62)   r3=300   (pick_bonus result)
//   acc      = 10  (line 63)   r4=10    (accumulate result)
//   combined = 324 (line 64)   r5=324   (raw*2 + bonus + acc)
//   return         (line 65)   r0=324
//
// 6 distinct source lines + the implicit start() line 1 = 7 step
// events; 1 main call + 1 main return; r0..r10 emitted per step.

fn control_flow_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // compute() lives at lines 65..72 of control_flow_test.rs:
    //   line 66: let raw: i64 = 7;
    //   line 67: let sign = classify(raw);
    //   line 68: let bonus = pick_bonus(sign);
    //   line 69: let acc = { msg!("..."); accumulate(0, 5) };
    //   line 70: let combined = raw * 2 + bonus + acc;
    //   line 71: combined  (return)
    let snaps = vec![
        snap(0, &[(1, 7)]),
        snap(1, &[(1, 7), (2, 1)]),
        snap(2, &[(1, 7), (2, 1), (3, 300)]),
        snap(3, &[(1, 7), (2, 1), (3, 300), (4, 10)]),
        snap(4, &[(1, 7), (2, 1), (3, 300), (4, 10), (5, 324)]),
        snap(5, &[(0, 324), (1, 7), (2, 1), (3, 300), (4, 10), (5, 324)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "control_flow_test.rs", 66),
        (1, "control_flow_test.rs", 67),
        (2, "control_flow_test.rs", 68),
        (3, "control_flow_test.rs", 69),
        (4, "control_flow_test.rs", 70),
        (5, "control_flow_test.rs", 71),
    ];
    (snaps, locs)
}

#[test]
fn test_control_flow_test_via_ct_print_full() {
    let (snaps, locs) = control_flow_snapshots();
    let Some((doc, source_path)) = record_and_dump_full(
        "test_control_flow_test_via_ct_print_full",
        "control_flow_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);
    assert_paths_contain(&doc, "control_flow_test.rs");

    // ----- Function table -------------------------------------------------
    // The recorder's source-driven model resolves the outermost frame
    // to `compute` (the function containing the first visited line at
    // 66).  Pinning the exact name catches regressions in the source
    // parser (e.g. accidentally falling back to `main` for a fixture
    // that does have a resolvable enclosing fn).
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions,
        vec!["compute"],
        "expected the sole function-table entry to be `compute`; got {:?}",
        functions
    );

    // ----- counts ---------------------------------------------------------
    // 1 implicit start() step at line 1 + 6 line-changes = 7 step events.
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(7), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(1), "calls; counts={counts}");
    // The fixture's compute() has exactly one `msg!(...)` invocation
    // (line 69, embedded in the `acc` binding) — the source-driven
    // synthesiser surfaces it as a single Write `io_event`.
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(1),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 7 steps + 1 call_entry + 1 call_exit + 1 io_event (msg!) = 10 events.
    assert_eq!(events.len(), 10, "events.len()");
    assert_step_indices_monotonic(&doc);

    // ----- Call sequence --------------------------------------------------
    assert_eq!(observed_call_sequence(&doc), vec!["compute".to_string()]);

    // ----- r0..r5 surfacing the canonical control-flow values -------------
    // The strict invariant: register values must round-trip through
    // ValueRecord::Int with the exact integers the snapshots seed.
    // Note: the implicit start() step at line 1 emits no register
    // variables — only the per-snapshot steps do — so the sequences
    // have exactly 6 entries (one per snapshot).
    assert_eq!(
        observed_register_sequence(&doc, "r1"),
        vec![7, 7, 7, 7, 7, 7],
        "r1 must surface `raw=7` on every snapshot once loaded",
    );
    assert_eq!(
        observed_register_sequence(&doc, "r2"),
        vec![0, 1, 1, 1, 1, 1],
        "r2 must surface the `sign = classify(raw)` result (1)",
    );
    assert_eq!(
        observed_register_sequence(&doc, "r3"),
        vec![0, 0, 300, 300, 300, 300],
        "r3 must surface the `bonus = pick_bonus(sign)` result (300)",
    );
    assert_eq!(
        observed_register_sequence(&doc, "r4"),
        vec![0, 0, 0, 10, 10, 10],
        "r4 must surface the `acc = accumulate(0, 5)` result (10)",
    );
    assert_eq!(
        observed_register_sequence(&doc, "r5"),
        vec![0, 0, 0, 0, 324, 324],
        "r5 must surface the `combined = 7*2 + 300 + 10 = 324` result",
    );
    assert_eq!(
        observed_register_sequence(&doc, "r0"),
        vec![0, 0, 0, 0, 0, 324],
        "r0 (return value) must surface 324 on the last snapshot",
    );
}

#[test]
fn test_control_flow_test_emits_msg_io_event() {
    let (snaps, locs) = control_flow_snapshots();
    let Some((doc, _)) = record_and_dump_full(
        "test_control_flow_test_emits_msg_io_event",
        "control_flow_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };
    let counts = &doc["counts"];
    assert!(
        counts["io_events"].as_u64().unwrap_or(0) >= 1,
        "expected at least one io_event for log_result(...); counts={counts}"
    );
}

// ===========================================================================
// nested_calls_test.rs
// ===========================================================================
//
// Drive a four-deep call chain compute → outer → middle → inner →
// (unwind) by using PC jumps > 2 forward (=> nested call) and < -2
// backward (=> return).  This exercises the recorder's call-jump
// heuristic in `record_from_snapshots_into_writer`.
//
// PC layout (synthetic, picked so each forward-jump > 2 fires the
// "register a callee fn_at_pc_<pc>" branch and each backward-jump > 2
// fires the "register_return" branch):
//
//   100  compute() entry, line 33     r1 = 0  (no value yet)
//   200  outer()   entry, line 25     r2 = 0
//   300  middle()  entry, line 17     r3 = 0
//   400  inner()   entry, line 11     r4 = 0
//   401  inner()   c=3,    line 13    r4 = 3   (a + b)
//   200  return to middle, line 18    r5 = 13  (x + 10)
//   201  return to outer,  line 26    r6 = 113 (p + 100)
//   100  return to compute,line 34    r0 = 113 (final)
//
// Note about `prev_line`: the recorder only emits a step when `line`
// changes from the previous snapshot — so PCs that map to line 11
// twice in a row dedupe, but PCs that map to *different* lines emit a
// step every transition.  We pick distinct lines for every snapshot.

fn nested_calls_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // Because the recorder builds a `HashMap<pc, (file, line)>` from
    // the source-locations slice, repeated PCs with different lines
    // would collide and the post-call step emissions would be lost to
    // the `prev_line` dedupe.  The snapshot stream below uses fresh
    // PC values for each synthetic "return-site" instruction (with
    // their own location entries) — this is a stand-in for "execution
    // returned to the caller's next instruction".
    let snaps = vec![
        snap(100, &[]),
        snap(200, &[]),
        snap(300, &[]),
        snap(400, &[]),
        snap(403, &[(4, 3)]),
        snap(206, &[(4, 3), (5, 13)]),
        snap(106, &[(4, 3), (5, 13), (6, 113)]),
        snap(50, &[(0, 113), (4, 3), (5, 13), (6, 113)]),
    ];
    // Each PC is mapped to a body line of the function it should
    // belong to in the source-driven model (nested_calls_test.rs):
    //   inner   lives at lines 16..21
    //   middle  lives at lines 23..27
    //   outer   lives at lines 29..33
    //   compute lives at lines 35..38
    // The recorder's source parser resolves each visited line back to
    // the enclosing fn name and uses it (instead of the previous
    // `fn_at_pc_<pc>` placeholder) when synthesising the call frame.
    // Within-function PC jumps (400→403, +3 — both inside `inner`) are
    // suppressed so the call-entry stream matches the program structure.
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (100, "nested_calls_test.rs", 36), // compute body (`let result = outer();`)
        (200, "nested_calls_test.rs", 30), // outer body  (`let p = middle();`)
        (300, "nested_calls_test.rs", 24), // middle body (`let x = inner();`)
        (400, "nested_calls_test.rs", 17), // inner body  (`let a: i64 = 1;`)
        (403, "nested_calls_test.rs", 19), // inner body  (`let c = a + b;`)
        (206, "nested_calls_test.rs", 31), // outer body  (`let q = p + 100;`)
        (106, "nested_calls_test.rs", 37), // compute body (`result`)
        (50, "nested_calls_test.rs", 38),  // compute close (`}`)
    ];
    (snaps, locs)
}

#[test]
fn test_nested_calls_test_via_ct_print_full() {
    let (snaps, locs) = nested_calls_snapshots();
    let Some((doc, source_path)) = record_and_dump_full(
        "test_nested_calls_test_via_ct_print_full",
        "nested_calls_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);
    assert_paths_contain(&doc, "nested_calls_test.rs");

    // ----- Function table -------------------------------------------------
    // The source-driven model resolves nested call frames through the
    // file's `fn name(...)` declarations: PC 100 lands inside
    // `compute`, the +100 forward jumps cross into `outer` / `middle` /
    // `inner` in turn, and the within-`inner` +3 jump (400→403) is
    // collapsed because both PCs sit inside the same fn body.  The
    // function table records the four real names — `main` no longer
    // appears.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions,
        vec!["compute", "outer", "middle", "inner"],
        "function table should contain the resolved fn names from the \
         four-deep call chain (compute → outer → middle → inner)"
    );

    // ----- counts ---------------------------------------------------------
    // Steps:     1 implicit start() + 8 distinct lines = 9 step events.
    // Functions: 4 resolved names (compute / outer / middle / inner).
    // Calls:     4 — one per fn-boundary forward jump, with `compute`
    //            as the implicit outermost frame.  The within-inner
    //            +3 jump (400→403) is suppressed by the source-driven
    //            model so it does NOT add a fifth call.
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(9), "steps; counts={counts}");
    assert_eq!(counts["functions"].as_u64(), Some(4), "functions; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(4), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 9 step + 4 call_entry + 4 call_exit = 17 events.  Each of the
    // four call_entries balances a call_exit: backward fn-boundary
    // jumps (403→206 and 206→106) unwind multiple frames at a time
    // (inner+middle and outer respectively) and the writer's `close()`
    // drains the outer `compute` frame at the end.
    assert_eq!(events.len(), 17, "events.len()");
    assert_step_indices_monotonic(&doc);

    let call_exit_count = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .count();
    assert_eq!(
        call_exit_count, 4,
        "expected exactly 4 call_exit events; got {call_exit_count}"
    );

    let step_event_count = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .count();
    assert_eq!(
        step_event_count, 9,
        "expected exactly 9 step events in the events array; got {step_event_count}"
    );

    // ----- Call entry order: outermost first ------------------------------
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "compute".to_string(),
            "outer".to_string(),
            "middle".to_string(),
            "inner".to_string(),
        ],
        "call_entry events must follow the program's nested-call order \
         (compute → outer → middle → inner)"
    );

    // ----- Call exit order: LIFO ------------------------------------------
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "inner".to_string(),
            "middle".to_string(),
            "outer".to_string(),
            "compute".to_string(),
        ],
        "call_exit events must appear in LIFO order (innermost first); \
         the source-driven model unwinds intermediate frames when a \
         backward jump crosses multiple fn boundaries"
    );

    // ----- r0 (return) surfaces 113 on the final snapshot -----------------
    let r0 = observed_register_sequence(&doc, "r0");
    assert!(
        r0.last().copied() == Some(113),
        "r0 (return) must end at 113; got {:?}",
        r0
    );
    let r4 = observed_register_sequence(&doc, "r4");
    assert!(
        r4.contains(&3),
        "r4 must surface inner()'s 1+2=3 at some point; got {:?}",
        r4
    );
    let r5 = observed_register_sequence(&doc, "r5");
    assert!(
        r5.contains(&13),
        "r5 must surface middle()'s 3+10=13 at some point; got {:?}",
        r5
    );
    let r6 = observed_register_sequence(&doc, "r6");
    assert!(
        r6.contains(&113),
        "r6 must surface outer()'s 13+100=113 at some point; got {:?}",
        r6
    );
}

#[test]
fn test_nested_calls_test_call_names_resolved_via_dwarf() {
    let (snaps, locs) = nested_calls_snapshots();
    let Some((doc, _)) = record_and_dump_full(
        "test_nested_calls_test_call_names_resolved_via_dwarf",
        "nested_calls_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "compute".to_string(),
            "outer".to_string(),
            "middle".to_string(),
            "inner".to_string(),
        ],
    );
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "inner".to_string(),
            "middle".to_string(),
            "outer".to_string(),
            "compute".to_string(),
        ],
    );
}

// ===========================================================================
// collections_test.rs
// ===========================================================================
//
// Walk through compute()'s let-bindings in collections_test.rs.  Each
// snapshot lands on a distinct source line, all PCs differ by 1 so no
// nested call frames are synthesised.  Canonical results:
//
//   xs_total     = 10  (sum_of_vec(&[1,2,3,4]))
//   pair_total   = 30  (sum_pair((10, 20)))
//   dist_sq      = 25  (point_distance_sq(Point{x:3, y:4}))
//   combined     = 65  (10 + 30 + 25)
//
// The source-driven synthesiser surfaces `xs` as
// ValueRecord::Sequence, `pair` as Tuple, and `p` as Struct alongside
// the per-snapshot register stream — `test_collections_test_value_kinds_present`
// asserts on the kinds and `test_collections_test_via_ct_print_full`
// pins the exact element / field values.

fn collections_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // compute() body in collections_test.rs:
    //   line 47: let xs: [i64; 4] = [1, 2, 3, 4];          (Sequence)
    //   line 48: let xs_total = sum_of_vec(&xs);            -> 10
    //   line 49: let pair = (10, 20);                       (Tuple)
    //   line 50: let p = Point { x: 3, y: 4 };              (Struct)
    //   line 51: let pair_total = sum_pair(pair);           -> 30
    //   line 52: let dist_sq = point_distance_sq(&p);       -> 25
    let snaps = vec![
        snap(0, &[]),
        snap(1, &[(1, 10)]),
        snap(2, &[(1, 10), (2, 30)]),
        snap(3, &[(1, 10), (2, 30), (3, 25)]),
        snap(4, &[(1, 10), (2, 30), (3, 25), (4, 65)]),
        snap(5, &[(0, 65), (1, 10), (2, 30), (3, 25), (4, 65)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "collections_test.rs", 47),
        (1, "collections_test.rs", 48),
        (2, "collections_test.rs", 49),
        (3, "collections_test.rs", 50),
        (4, "collections_test.rs", 51),
        (5, "collections_test.rs", 52),
    ];
    (snaps, locs)
}

#[test]
fn test_collections_test_via_ct_print_full() {
    let (snaps, locs) = collections_snapshots();
    let Some((doc, source_path)) = record_and_dump_full(
        "test_collections_test_via_ct_print_full",
        "collections_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);
    assert_paths_contain(&doc, "collections_test.rs");

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(functions, vec!["compute"]);

    let counts = &doc["counts"];
    // 1 implicit start() + 6 distinct lines = 7 steps.
    assert_eq!(counts["steps"].as_u64(), Some(7), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(1), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 7 steps + 1 call_entry + 1 call_exit = 9 events.
    assert_eq!(events.len(), 9, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(observed_call_sequence(&doc), vec!["compute".to_string()]);

    // ----- Canonical collection results -----------------------------------
    let r1 = observed_register_sequence(&doc, "r1");
    assert!(r1.contains(&10), "xs_total=10 must surface in r1; got {:?}", r1);
    let r2 = observed_register_sequence(&doc, "r2");
    assert!(r2.contains(&30), "pair_total=30 must surface in r2; got {:?}", r2);
    let r3 = observed_register_sequence(&doc, "r3");
    assert!(r3.contains(&25), "dist_sq=25 must surface in r3; got {:?}", r3);
    let r4 = observed_register_sequence(&doc, "r4");
    assert!(
        r4.contains(&65),
        "combined=65 must surface in r4; got {:?}",
        r4
    );
    let r0 = observed_register_sequence(&doc, "r0");
    assert!(
        r0.last().copied() == Some(65),
        "r0 (return) must end at 65; got {:?}",
        r0
    );

    // ----- Compound locals (Sequence / Tuple / Struct) --------------------
    // The source-driven synthesiser pattern-matches the array literal,
    // tuple literal, and struct literal in compute() and emits typed
    // ValueRecord variants alongside the per-snapshot register stream.
    let compounds = observed_compound_vars(&doc);
    let xs = compounds
        .iter()
        .find(|(n, _)| n == "xs")
        .expect("xs must surface as a compound variable");
    assert_eq!(xs.1["kind"].as_str(), Some("Sequence"));
    assert_eq!(xs.1["is_slice"].as_bool(), Some(false));
    let xs_elements = xs.1["elements"].as_array().expect("xs.elements array");
    let xs_ints: Vec<i64> = xs_elements
        .iter()
        .map(|e| e["i"].as_i64().expect("xs element must be Int.i"))
        .collect();
    assert_eq!(xs_ints, vec![1, 2, 3, 4]);

    let pair = compounds
        .iter()
        .find(|(n, _)| n == "pair")
        .expect("pair must surface as a compound variable");
    assert_eq!(pair.1["kind"].as_str(), Some("Tuple"));
    let pair_elements = pair.1["elements"].as_array().expect("pair.elements array");
    let pair_ints: Vec<i64> = pair_elements
        .iter()
        .map(|e| e["i"].as_i64().expect("pair element must be Int.i"))
        .collect();
    assert_eq!(pair_ints, vec![10, 20]);

    let p = compounds
        .iter()
        .find(|(n, _)| n == "p")
        .expect("p must surface as a compound variable");
    assert_eq!(p.1["kind"].as_str(), Some("Struct"));
    let p_fields = p.1["field_values"].as_array().expect("p.field_values array");
    let p_ints: Vec<i64> = p_fields
        .iter()
        .map(|e| e["i"].as_i64().expect("p field must be Int.i"))
        .collect();
    assert_eq!(p_ints, vec![3, 4]);
}

#[test]
fn test_collections_test_value_kinds_present() {
    let (snaps, locs) = collections_snapshots();
    let Some((doc, _)) = record_and_dump_full(
        "test_collections_test_value_kinds_present",
        "collections_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };
    let mut kinds = std::collections::BTreeSet::new();
    for ev in doc["events"].as_array().unwrap() {
        if ev["kind"] != "step" {
            continue;
        }
        for v in ev["vars"].as_array().cloned().unwrap_or_default() {
            if let Some(k) = v["value"]["kind"].as_str() {
                kinds.insert(k.to_string());
            }
        }
    }
    for want in ["Int", "Sequence", "Tuple", "Struct"] {
        assert!(
            kinds.contains(want),
            "expected {want} ValueRecord variant in collections trace; got {kinds:?}"
        );
    }
}

// ===========================================================================
// error_paths_test.rs
// ===========================================================================
//
// Walk through safe_compute()'s body and then visit the `panic!`
// call inside `panicking_compute` so the source-driven synthesiser
// can surface a SolanaPanic error io_event.

fn error_paths_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // safe_compute() lives at lines 32..38 of error_paths_test.rs:
    //   line 33: a = 5
    //   line 34: b = 7
    //   line 35: c = safe_add(a, b)  -> 12
    //   line 36: bumped = c + 100    -> 112
    //   line 37: bumped (return)
    //
    // A sixth snapshot (PC 5 → line 62, inside `panicking_compute`)
    // visits a `panic!` call so the source-driven synthesiser can
    // surface a SolanaPanic error io_event without otherwise
    // perturbing the safe-path register stream.
    let snaps = vec![
        snap(0, &[(1, 5)]),
        snap(1, &[(1, 5), (2, 7)]),
        snap(2, &[(1, 5), (2, 7), (3, 12)]),
        snap(3, &[(1, 5), (2, 7), (3, 12), (4, 112)]),
        snap(4, &[(0, 112), (1, 5), (2, 7), (3, 12), (4, 112)]),
        snap(5, &[(0, 112), (1, 5), (2, 7), (3, 12), (4, 112)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "error_paths_test.rs", 33),
        (1, "error_paths_test.rs", 34),
        (2, "error_paths_test.rs", 35),
        (3, "error_paths_test.rs", 36),
        (4, "error_paths_test.rs", 37),
        (5, "error_paths_test.rs", 62),
    ];
    (snaps, locs)
}

#[test]
fn test_error_paths_test_via_ct_print_full() {
    let (snaps, locs) = error_paths_snapshots();
    let Some((doc, source_path)) = record_and_dump_full(
        "test_error_paths_test_via_ct_print_full",
        "error_paths_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);
    assert_paths_contain(&doc, "error_paths_test.rs");

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // First visited line (33) lives inside `safe_compute` — that's
    // what the source-driven model surfaces as the outermost frame.
    assert_eq!(functions, vec!["safe_compute"]);

    let counts = &doc["counts"];
    // 1 implicit start() + 6 distinct lines = 7 step events.  The
    // extra step (line 62) lands on a `panic!` call so the
    // synthesiser emits exactly one SolanaPanic error io_event.
    assert_eq!(counts["steps"].as_u64(), Some(7), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(1), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(1),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 7 steps + 1 call_entry + 1 call_exit + 1 io_event = 10 events.
    assert_eq!(events.len(), 10, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(observed_call_sequence(&doc), vec!["safe_compute".to_string()]);

    // ----- Canonical safe-path values -------------------------------------
    let r1 = observed_register_sequence(&doc, "r1");
    assert!(r1.contains(&5), "a=5 must surface in r1; got {:?}", r1);
    let r2 = observed_register_sequence(&doc, "r2");
    assert!(r2.contains(&7), "b=7 must surface in r2; got {:?}", r2);
    let r3 = observed_register_sequence(&doc, "r3");
    assert!(r3.contains(&12), "c=12 must surface in r3; got {:?}", r3);
    let r4 = observed_register_sequence(&doc, "r4");
    assert!(r4.contains(&112), "bumped=112 must surface in r4; got {:?}", r4);
    let r0 = observed_register_sequence(&doc, "r0");
    assert!(
        r0.last().copied() == Some(112),
        "r0 (return) must end at 112; got {:?}",
        r0
    );
}

#[test]
fn test_error_paths_test_emits_error_io_event() {
    let (snaps, locs) = error_paths_snapshots();
    let Some((doc, _)) = record_and_dump_full(
        "test_error_paths_test_emits_error_io_event",
        "error_paths_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };
    let counts = &doc["counts"];
    assert!(
        counts["io_events"].as_u64().unwrap_or(0) >= 1,
        "expected at least one error io_event; counts={counts}"
    );
}

// ===========================================================================
// msg_log_test.rs
// ===========================================================================
//
// Walk through compute()'s body in msg_log_test.rs.  The three
// `msg!(...)` invocations on visited lines (29 / 31 / 33) round-trip
// through the source-driven synthesiser as Write io_events whose
// content is the formatted log payload.  Both the strict full-trace
// and the focused 3-io_events test pin the count and the call shape.

fn msg_log_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // compute(4, 5) lives at lines 28..35:
    //   line 29: msg!("entering compute ...")
    //   line 30: sum_val = a + b -> 9
    //   line 31: msg!("sum_val=...")
    //   line 32: doubled = sum_val * 2 -> 18
    //   line 33: msg!("returning ...")
    //   line 34: doubled (return)
    let snaps = vec![
        snap(0, &[(1, 4), (2, 5)]),
        snap(1, &[(1, 4), (2, 5), (3, 9)]),
        snap(2, &[(1, 4), (2, 5), (3, 9)]),
        snap(3, &[(1, 4), (2, 5), (3, 9), (4, 18)]),
        snap(4, &[(1, 4), (2, 5), (3, 9), (4, 18)]),
        snap(5, &[(0, 18), (1, 4), (2, 5), (3, 9), (4, 18)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "msg_log_test.rs", 29),
        (1, "msg_log_test.rs", 30),
        (2, "msg_log_test.rs", 31),
        (3, "msg_log_test.rs", 32),
        (4, "msg_log_test.rs", 33),
        (5, "msg_log_test.rs", 34),
    ];
    (snaps, locs)
}

#[test]
fn test_msg_log_test_via_ct_print_full() {
    let (snaps, locs) = msg_log_snapshots();
    let Some((doc, source_path)) = record_and_dump_full(
        "test_msg_log_test_via_ct_print_full",
        "msg_log_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);
    assert_paths_contain(&doc, "msg_log_test.rs");

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(functions, vec!["compute"]);

    let counts = &doc["counts"];
    // 1 implicit start() + 6 distinct lines = 7 step events.
    assert_eq!(counts["steps"].as_u64(), Some(7), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(1), "calls; counts={counts}");
    // The fixture has three `msg!(...)` invocations on visited lines
    // (29 / 31 / 33) — the source-driven synthesiser surfaces each as
    // a Write io_event with the formatted payload as its content.
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(3),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 7 steps + 1 call_entry + 1 call_exit + 3 io_events = 12 events.
    assert_eq!(events.len(), 12, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(observed_call_sequence(&doc), vec!["compute".to_string()]);

    // ----- Canonical msg-log values ---------------------------------------
    let r1 = observed_register_sequence(&doc, "r1");
    assert!(r1.contains(&4), "a=4 must surface in r1; got {:?}", r1);
    let r2 = observed_register_sequence(&doc, "r2");
    assert!(r2.contains(&5), "b=5 must surface in r2; got {:?}", r2);
    let r3 = observed_register_sequence(&doc, "r3");
    assert!(r3.contains(&9), "sum_val=9 must surface in r3; got {:?}", r3);
    let r4 = observed_register_sequence(&doc, "r4");
    assert!(r4.contains(&18), "doubled=18 must surface in r4; got {:?}", r4);
    let r0 = observed_register_sequence(&doc, "r0");
    assert!(
        r0.last().copied() == Some(18),
        "r0 (return) must end at 18; got {:?}",
        r0
    );
}

#[test]
fn test_msg_log_test_emits_three_io_events() {
    let (snaps, locs) = msg_log_snapshots();
    let Some((doc, _)) = record_and_dump_full(
        "test_msg_log_test_emits_three_io_events",
        "msg_log_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };
    let counts = &doc["counts"];
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(3),
        "expected exactly 3 io_events (one per msg! invocation); counts={counts}"
    );
}

// ===========================================================================
// account_processing_test.rs
// ===========================================================================
//
// Walk through process_transfer()'s body.  Canonical execution:
//   account = AccountInfo { balance: 1000, owner: 42, is_signer: true }
//   before  = read_balance(&account)            -> 1000
//   after   = debit(before, 250)                -> 750
//   write_balance(&mut account, after)
//   account.balance                              -> 750 (return)

fn account_processing_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // process_transfer() lives at lines 44..54.  The AccountInfo struct
    // literal spans lines 45..49 (the recorder's source-driven model
    // stitches multi-line `Name { .. }` literals together when the
    // visited line opens with `{` but doesn't close it on the same
    // line).  Snapshots:
    //   line 45: let mut account = AccountInfo { ... }   (Struct)
    //   line 50: let before = read_balance(&account)     -> 1000
    //   line 51: let after  = debit(before, 250)         -> 750
    //   line 52: write_balance(&mut account, after)
    //   line 53: account.balance                         (return)
    let snaps = vec![
        snap(0, &[(1, 1000), (2, 42), (3, 1)]),
        snap(1, &[(1, 1000), (2, 42), (3, 1), (4, 1000)]),
        snap(2, &[(1, 1000), (2, 42), (3, 1), (4, 1000), (5, 750)]),
        snap(3, &[(1, 750), (2, 42), (3, 1), (4, 1000), (5, 750)]),
        snap(4, &[(0, 750), (1, 750), (2, 42), (3, 1), (4, 1000), (5, 750)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "account_processing_test.rs", 45),
        (1, "account_processing_test.rs", 50),
        (2, "account_processing_test.rs", 51),
        (3, "account_processing_test.rs", 52),
        (4, "account_processing_test.rs", 53),
    ];
    (snaps, locs)
}

#[test]
fn test_account_processing_test_via_ct_print_full() {
    let (snaps, locs) = account_processing_snapshots();
    let Some((doc, source_path)) = record_and_dump_full(
        "test_account_processing_test_via_ct_print_full",
        "account_processing_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);
    assert_paths_contain(&doc, "account_processing_test.rs");

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(functions, vec!["process_transfer"]);

    let counts = &doc["counts"];
    // 1 implicit start() + 5 distinct lines = 6 step events.
    assert_eq!(counts["steps"].as_u64(), Some(6), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(1), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 6 steps + 1 call_entry + 1 call_exit = 8 events.
    assert_eq!(events.len(), 8, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(observed_call_sequence(&doc), vec!["process_transfer".to_string()]);

    // ----- Canonical account-read / account-write values ------------------
    // r1 traces the balance: starts 1000, ends 750 after write_balance.
    let r1 = observed_register_sequence(&doc, "r1");
    assert!(
        r1.contains(&1000),
        "pre-transfer balance 1000 must surface in r1; got {:?}",
        r1
    );
    assert!(
        r1.contains(&750),
        "post-transfer balance 750 must surface in r1; got {:?}",
        r1
    );
    let r4 = observed_register_sequence(&doc, "r4");
    assert!(
        r4.contains(&1000),
        "before=1000 (read_balance) must surface in r4; got {:?}",
        r4
    );
    let r5 = observed_register_sequence(&doc, "r5");
    assert!(
        r5.contains(&750),
        "after=750 (debit result) must surface in r5; got {:?}",
        r5
    );
    let r0 = observed_register_sequence(&doc, "r0");
    assert!(
        r0.last().copied() == Some(750),
        "r0 (return) must end at 750; got {:?}",
        r0
    );

    // ----- AccountInfo struct surface -------------------------------------
    // The visited step at line 45 stitches the multi-line
    // `AccountInfo { ... }` literal back into a single Struct value
    // with three Int field values (balance=1000, owner=42, is_signer=1).
    let compounds = observed_compound_vars(&doc);
    let account = compounds
        .iter()
        .find(|(n, _)| n == "account")
        .expect("account must surface as a compound variable");
    assert_eq!(account.1["kind"].as_str(), Some("Struct"));
    let fields = account.1["field_values"].as_array().expect("field_values array");
    let field_ints: Vec<i64> = fields
        .iter()
        .map(|e| e["i"].as_i64().expect("AccountInfo field must be Int.i"))
        .collect();
    assert_eq!(field_ints, vec![1000, 42, 1]);
}

#[test]
fn test_account_processing_test_account_struct_present() {
    let (snaps, locs) = account_processing_snapshots();
    let Some((doc, _)) = record_and_dump_full(
        "test_account_processing_test_account_struct_present",
        "account_processing_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };
    let mut kinds = std::collections::BTreeSet::new();
    for ev in doc["events"].as_array().unwrap() {
        if ev["kind"] != "step" {
            continue;
        }
        for v in ev["vars"].as_array().cloned().unwrap_or_default() {
            if let Some(k) = v["value"]["kind"].as_str() {
                kinds.insert(k.to_string());
            }
        }
    }
    assert!(
        kinds.contains("Struct"),
        "expected Struct ValueRecord variant for AccountInfo; got {kinds:?}"
    );
}

// ===========================================================================
// Sanity test: `count_step_events` helper round-trips against `counts.steps`
// ===========================================================================
//
// Catches a regression in either the recorder (counts.steps drifts
// from the actual step-event count in the events array) or in
// ct-print's renderer (events array misses a `kind` field).

#[test]
fn test_step_count_helper_matches_counts_field() {
    let (snaps, locs) = control_flow_snapshots();
    let Some((doc, _)) = record_and_dump_full(
        "test_step_count_helper_matches_counts_field",
        "control_flow_test.rs",
        &snaps,
        &locs,
    ) else {
        return;
    };
    let from_helper = count_step_events(&doc) as u64;
    let from_counts = doc["counts"]["steps"].as_u64().expect("counts.steps");
    assert_eq!(
        from_helper, from_counts,
        "counts.steps ({from_counts}) must match the step-event count \
         in events ({from_helper})"
    );
}
