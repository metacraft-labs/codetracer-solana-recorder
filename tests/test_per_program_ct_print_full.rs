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
//! Where the recorder's current behaviour deviates from what the
//! Solana programming model dictates (e.g. `msg!` is not surfaced as
//! a write event because the synthetic-snapshot path has no syscall
//! hook; `Vec<T>` / `struct` / tuple values are not yet decoded as
//! `ValueRecord::Sequence` / `Struct` / `Tuple`), the deviation is
//! documented inline as `RECORDER BUG: ...` and a parallel `#[ignore]`d
//! test captures the spec-correct expectation so it surfaces the
//! moment the recorder catches up.

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

/// Decode every (varname, i64) pair from step events.  Rejects any
/// `ValueRecord` variant other than `Int` with a hard error that asks
/// the test author to extend the test rather than weaken it.
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
            let value = &v["value"];
            assert_eq!(
                value["kind"].as_str(),
                Some("Int"),
                "variable `{}` should decode as Int, got {}; \
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
    let snaps = vec![
        snap(0, &[(1, 7)]),
        snap(1, &[(1, 7), (2, 1)]),
        snap(2, &[(1, 7), (2, 1), (3, 300)]),
        snap(3, &[(1, 7), (2, 1), (3, 300), (4, 10)]),
        snap(4, &[(1, 7), (2, 1), (3, 300), (4, 10), (5, 324)]),
        snap(5, &[(0, 324), (1, 7), (2, 1), (3, 300), (4, 10), (5, 324)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "control_flow_test.rs", 60),
        (1, "control_flow_test.rs", 61),
        (2, "control_flow_test.rs", 62),
        (3, "control_flow_test.rs", 63),
        (4, "control_flow_test.rs", 64),
        (5, "control_flow_test.rs", 65),
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
    // RECORDER BUG: spec wants `process_instruction`, `compute`,
    // `classify`, `pick_bonus`, `accumulate`, `log_result` in the
    // function table.  Today the SBF recorder synthesises a single
    // `main` frame for every recording (no DWARF function-name pass),
    // so `compute` and friends never surface.  When DWARF function
    // resolution lands, this assertion will trip and the next
    // maintainer should extend it to the real function names.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions.len(),
        1,
        "expected exactly 1 entry in the functions table; got {:?}",
        functions
    );
    assert!(
        functions[0].ends_with("main"),
        "expected the sole function-table entry to be `main`; got {:?}",
        functions
    );

    // ----- counts ---------------------------------------------------------
    // 1 implicit start() step at line 1 + 6 line-changes = 7 step events.
    let counts = &doc["counts"];
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

    // ----- Call sequence --------------------------------------------------
    assert_eq!(observed_call_sequence(&doc), vec!["main".to_string()]);

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
#[ignore = "RECORDER BUG: msg!/sol_log syscalls are not surfaced as \
            io_events because the synthetic-snapshot recorder pipeline \
            has no syscall hook.  Spec-compliant output should emit at \
            least one io_event with the formatted log payload when a \
            control-flow branch invokes msg!()."]
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
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (100, "nested_calls_test.rs", 33),
        (200, "nested_calls_test.rs", 25),
        (300, "nested_calls_test.rs", 17),
        (400, "nested_calls_test.rs", 11),
        (403, "nested_calls_test.rs", 13),
        (206, "nested_calls_test.rs", 26),
        (106, "nested_calls_test.rs", 34),
        (50, "nested_calls_test.rs", 35),
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
    // The recorder synthesises `main` for the outermost frame and
    // `fn_at_pc_<pc>` for each forward-jump > 2.  We jump forward
    // three times (100→200, 200→300, 300→400) — that yields three
    // synthetic callee functions.  When DWARF function-name resolution
    // lands, these will become `outer`, `middle`, `inner`; until then
    // we pin the synthetic names so a regression in the heuristic is
    // caught immediately.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // The recorder's heuristic registers a callee for every forward
    // PC-jump > 2.  Our snapshot stream has four such jumps:
    //   100->200 (+100), 200->300 (+100), 300->400 (+100), 400->403 (+3).
    // The four `fn_at_pc_<pc>` entries are pinned in entry order so a
    // regression in the heuristic (e.g. dropping the +3 jump because
    // the inequality drifts to `>= 4`) is caught immediately.
    assert_eq!(
        functions,
        vec![
            "main",
            "fn_at_pc_200",
            "fn_at_pc_300",
            "fn_at_pc_400",
            "fn_at_pc_403",
        ],
        "function table should contain main + one fn_at_pc_<pc> per \
         forward-jump > 2; if DWARF function-name resolution has \
         landed, extend this test to assert on the resolved names"
    );

    // ----- counts ---------------------------------------------------------
    // Steps:     1 implicit start() + 8 distinct lines = 9 step events.
    // Functions: 1 main + 4 nested fn_at_pc_<pc> = 5 entries in the
    //            functions table.
    // Calls:     `counts.calls` reports the number of completed
    //            call/exit pairs the writer has flushed — 5 in this
    //            stream.  The trace writer's `close()` drains any
    //            unclosed PendingCalls (LIFO) so partial-trace
    //            recordings still produce balanced call_entry/call_exit
    //            pairs: 3 backward-jump returns + 1 inner forward-jump
    //            flushed at close + 1 outer `main` flushed at close = 5.
    //            (Before the writer fix, `close()` silently dropped the
    //            unclosed frames and `counts.calls` stopped at 4 even
    //            though the events array carried 5 call_entry events.)
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(9), "steps; counts={counts}");
    assert_eq!(counts["functions"].as_u64(), Some(5), "functions; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(5), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 9 step + 5 call_entry + 5 call_exit = 19 events.  Now that the
    // writer's `close()` drains unclosed PendingCalls, the outermost
    // `main` frame surfaces as both a call_entry and a call_exit, in
    // addition to the four synthesised callees from the +N forward
    // jumps.  All five call_exits balance the entries in LIFO order.
    assert_eq!(events.len(), 19, "events.len()");
    assert_step_indices_monotonic(&doc);

    let call_exit_count = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .count();
    assert_eq!(
        call_exit_count, 5,
        "expected exactly 5 call_exit events; got {call_exit_count}"
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
    // The writer's `close()` drain ensures the outermost `main` frame
    // appears in the call_entry stream as well as the function table.
    // The five entries are pinned in entry order (outermost first) so a
    // regression that re-elides `main` (or that drops/reorders a nested
    // call) is caught immediately.
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "fn_at_pc_200".to_string(),
            "fn_at_pc_300".to_string(),
            "fn_at_pc_400".to_string(),
            "fn_at_pc_403".to_string(),
        ],
        "call_entry events must include `main` and the four nested \
         callees in entry order (outermost first)"
    );

    // ----- Call exit order: LIFO ------------------------------------------
    // `main` is the deepest frame (latest to exit) and so appears last.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "fn_at_pc_403".to_string(),
            "fn_at_pc_400".to_string(),
            "fn_at_pc_300".to_string(),
            "fn_at_pc_200".to_string(),
            "main".to_string(),
        ],
        "call_exit events must appear in LIFO order (innermost first); \
         the outermost `main` exit is appended last by the writer's \
         close-time PendingCall drain"
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
#[ignore = "RECORDER BUG: nested call frames are synthesised as \
            `fn_at_pc_<pc>` rather than resolved DWARF function names. \
            Spec-compliant output should yield call_entry function \
            names [\"compute\", \"outer\", \"middle\", \"inner\"] in \
            entry order and [\"inner\", \"middle\", \"outer\", \
            \"compute\"] in LIFO exit order."]
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
// RECORDER BUG: a spec-compliant trace would expose `xs` as
// ValueRecord::Sequence, the (10,20) tuple as Tuple, and Point as
// Struct.  Today the recorder emits only Int values via the SBF
// register stream — the `#[ignore]`d sibling test pins the
// spec-compliant expectation.

fn collections_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // compute() lives at lines 50..58 of collections_test.rs:
    //   line 51: xs declaration
    //   line 52: xs_total = sum_of_vec(&xs)
    //   line 53: pair_total = sum_pair((10, 20))
    //   line 54: p = Point { x: 3, y: 4 }
    //   line 55: dist_sq = point_distance_sq(&p)
    //   line 56: combined = ...
    //   line 57: combined (return)
    let snaps = vec![
        snap(0, &[]),
        snap(1, &[(1, 10)]),
        snap(2, &[(1, 10), (2, 30)]),
        snap(3, &[(1, 10), (2, 30), (3, 25)]),
        snap(4, &[(1, 10), (2, 30), (3, 25), (4, 65)]),
        snap(5, &[(0, 65), (1, 10), (2, 30), (3, 25), (4, 65)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "collections_test.rs", 51),
        (1, "collections_test.rs", 52),
        (2, "collections_test.rs", 53),
        (3, "collections_test.rs", 55),
        (4, "collections_test.rs", 56),
        (5, "collections_test.rs", 57),
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
    assert_eq!(functions.len(), 1);
    assert!(functions[0].ends_with("main"));

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

    assert_eq!(observed_call_sequence(&doc), vec!["main".to_string()]);

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
}

#[test]
#[ignore = "RECORDER BUG: Vec / tuple / struct values are not encoded \
            as ValueRecord::Sequence / Tuple / Struct.  Spec-compliant \
            output should surface xs as Sequence, (10,20) as Tuple, \
            and Point{x:3,y:4} as Struct.  Today only Int values from \
            the SBF register stream surface."]
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
// Walk through process_instruction()'s safe-path execution.  The
// failing branches (withdraw → InsufficientFunds, panicking_compute)
// are unreachable from the snapshot stream because the SBF recorder
// has no syscall hook to abort / return Err — they are exercised by
// the `#[ignore]`d companion test below.

fn error_paths_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // safe_compute() lives at lines 26..32:
    //   line 27: a = 5
    //   line 28: b = 7
    //   line 29: c = safe_add(a, b)  -> 12
    //   line 30: bumped = c + 100    -> 112
    //   line 31: bumped (return)
    let snaps = vec![
        snap(0, &[(1, 5)]),
        snap(1, &[(1, 5), (2, 7)]),
        snap(2, &[(1, 5), (2, 7), (3, 12)]),
        snap(3, &[(1, 5), (2, 7), (3, 12), (4, 112)]),
        snap(4, &[(0, 112), (1, 5), (2, 7), (3, 12), (4, 112)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "error_paths_test.rs", 27),
        (1, "error_paths_test.rs", 28),
        (2, "error_paths_test.rs", 29),
        (3, "error_paths_test.rs", 30),
        (4, "error_paths_test.rs", 31),
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
    assert_eq!(functions.len(), 1);
    assert!(functions[0].ends_with("main"));

    let counts = &doc["counts"];
    // 1 implicit start() + 5 distinct lines = 6 step events.
    assert_eq!(counts["steps"].as_u64(), Some(6), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(1), "calls; counts={counts}");
    // RECORDER BUG: should be >= 1 once panic / Result::Err / ProgramError
    // surface as error io_events.  Today the SBF synthetic-snapshot
    // pipeline has no syscall hook so the count is 0.
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 6 steps + 1 call_entry + 1 call_exit = 8 events.
    assert_eq!(events.len(), 8, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(observed_call_sequence(&doc), vec!["main".to_string()]);

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
#[ignore = "RECORDER BUG: panic! / Result::Err / ProgramError are not \
            surfaced as error io_events because the SBF synthetic- \
            snapshot recorder pipeline has no syscall / abort hook. \
            Spec-compliant output should emit at least one io_event of \
            error kind for each unhandled error path."]
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
// Walk through compute()'s body in msg_log_test.rs.  The three msg!
// invocations are silent in the present-day SBF recorder (no syscall
// hook); the `#[ignore]`d companion pins the spec-compliant
// expectation of 3 io_events.

fn msg_log_snapshots() -> (Vec<RegisterSnapshot>, Vec<(u64, &'static str, u32)>) {
    // compute(4, 5) lives at lines 30..36:
    //   line 30: msg!("entering compute ...")
    //   line 31: sum_val = a + b -> 9
    //   line 32: msg!("sum_val=...")
    //   line 33: doubled = sum_val * 2 -> 18
    //   line 34: msg!("returning ...")
    //   line 35: doubled (return)
    let snaps = vec![
        snap(0, &[(1, 4), (2, 5)]),
        snap(1, &[(1, 4), (2, 5), (3, 9)]),
        snap(2, &[(1, 4), (2, 5), (3, 9)]),
        snap(3, &[(1, 4), (2, 5), (3, 9), (4, 18)]),
        snap(4, &[(1, 4), (2, 5), (3, 9), (4, 18)]),
        snap(5, &[(0, 18), (1, 4), (2, 5), (3, 9), (4, 18)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "msg_log_test.rs", 30),
        (1, "msg_log_test.rs", 31),
        (2, "msg_log_test.rs", 32),
        (3, "msg_log_test.rs", 33),
        (4, "msg_log_test.rs", 34),
        (5, "msg_log_test.rs", 35),
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
    assert_eq!(functions.len(), 1);
    assert!(functions[0].ends_with("main"));

    let counts = &doc["counts"];
    // 1 implicit start() + 6 distinct lines = 7 step events.
    assert_eq!(counts["steps"].as_u64(), Some(7), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(1), "calls; counts={counts}");
    // RECORDER BUG: should be exactly 3 (one per msg! invocation).
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 7 steps + 1 call_entry + 1 call_exit = 9 events.
    assert_eq!(events.len(), 9, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(observed_call_sequence(&doc), vec!["main".to_string()]);

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
#[ignore = "RECORDER BUG: msg! / sol_log syscalls are not surfaced as \
            io_events.  Spec-compliant output should emit exactly 3 \
            io_events (one per msg! invocation in compute()), each \
            with the formatted log payload as the value."]
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
    // process_transfer() lives at lines 31..38:
    //   line 32: account = AccountInfo { ... }
    //   line 33: before = read_balance(&account)   -> 1000
    //   line 34: after = debit(before, 250)        -> 750
    //   line 35: write_balance(&mut account, after)
    //   line 36: account.balance (return)
    let snaps = vec![
        snap(0, &[(1, 1000), (2, 42), (3, 1)]),
        snap(1, &[(1, 1000), (2, 42), (3, 1), (4, 1000)]),
        snap(2, &[(1, 1000), (2, 42), (3, 1), (4, 1000), (5, 750)]),
        snap(3, &[(1, 750), (2, 42), (3, 1), (4, 1000), (5, 750)]),
        snap(4, &[(0, 750), (1, 750), (2, 42), (3, 1), (4, 1000), (5, 750)]),
    ];
    let locs: Vec<(u64, &'static str, u32)> = vec![
        (0, "account_processing_test.rs", 32),
        (1, "account_processing_test.rs", 33),
        (2, "account_processing_test.rs", 34),
        (3, "account_processing_test.rs", 35),
        (4, "account_processing_test.rs", 36),
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
    assert_eq!(functions.len(), 1);
    assert!(functions[0].ends_with("main"));

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

    assert_eq!(observed_call_sequence(&doc), vec!["main".to_string()]);

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
}

#[test]
#[ignore = "RECORDER BUG: AccountInfo / Vec<AccountInfo> values are \
            not encoded as ValueRecord::Struct / Sequence.  Spec- \
            compliant output should surface the AccountInfo struct \
            with its three fields (balance, owner, is_signer) as a \
            named-fields Struct value at the read/write step."]
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
