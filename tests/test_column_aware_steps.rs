//! Column-aware replay-navigation regression test for the Solana SBF
//! recorder.
//!
//! Mirrors `codetracer-js-recorder/tests/integration/column-aware.test.ts`:
//! when three Rust statements live on a single source line and the
//! recorder is fed columns alongside line numbers, every emitted step
//! must record its own distinct column on the canonical CTFS wire — not
//! collapse onto the previous statement's column.
//!
//! Acceptance:
//!
//! * `meta.dat` carries `FlagHasColumnAwareSteps` (bit 4), which
//!   `ct-print --full` surfaces as `metadata.flags.has_column_aware_steps`.
//! * Step events on the multi-statement line surface strictly distinct
//!   `column` fields.
//!
//! The recorder converts the 0-based column offsets the test seeds
//! (`0`, `11`, `22` — the byte starts of `let a`, `let b`, `let c` in
//! `let a = 1; let b = 2; let c = 3;`) to 1-based columns `1`, `12`, `23`
//! on the wire.  See the JS fixture for the matching invariant on the
//! JS recorder side.

use std::path::PathBuf;
use std::process::Command;

use codetracer_solana_recorder::recorder::record_from_snapshots_with_columns;
use codetracer_solana_recorder::register_trace::RegisterSnapshot;

/// Path to the `ct-print` binary shipped with `codetracer-trace-format-nim`.
fn ct_print_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join(format!("ct-print{}", std::env::consts::EXE_SUFFIX))
}

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

#[test]
fn test_multi_statement_line_surfaces_distinct_columns() {
    let test_name = "test_multi_statement_line_surfaces_distinct_columns";
    let Some(ct_print) = ct_print_or_skip(test_name) else {
        return;
    };

    // The fixture's first line is `let a = 1; let b = 2; let c = 3;`
    // (matches the JS recorder's P2 column-aware fixture).  Three
    // statements at 0-based byte offsets 0, 11, 22 — the recorder
    // converts to 1-based columns 1, 12, 23 on the CTFS wire.  We
    // dedicate one snapshot per landing column.
    //
    // Distinct PCs prevent the `HashMap<pc, (file, line, column)>` used
    // by `record_from_snapshots_into_writer` from collapsing rows.
    let snaps = vec![
        snap(0, &[(1, 1)]),
        snap(1, &[(1, 1), (2, 2)]),
        snap(2, &[(1, 1), (2, 2), (3, 3)]),
    ];
    let source_locs: Vec<(u64, &str, u32, Option<u32>)> = vec![
        (0, "column_aware_test.rs", 1, Some(1)),
        (1, "column_aware_test.rs", 1, Some(12)),
        (2, "column_aware_test.rs", 1, Some(23)),
    ];

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("column_aware_test.rs");
    record_from_snapshots_with_columns(&snaps, &source_locs, &source_path, &out_dir)
        .expect("record_from_snapshots_with_columns should succeed");

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

    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full should emit valid JSON");

    // ---- meta.dat bit 4 (`FlagHasColumnAwareSteps`) ---------------------
    let flag = doc["metadata"]["flags"]["has_column_aware_steps"].as_bool();
    assert_eq!(
        flag,
        Some(true),
        "expected metadata.flags.has_column_aware_steps == true; \
         got {} for the full document; recorder must call \
         `enable_column_aware_steps` before emitting any step",
        doc["metadata"]
    );

    // ---- Distinct landing columns on the multi-statement line -----------
    let events = doc["events"].as_array().expect("events array");
    let line1_cols: Vec<i64> = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .filter(|e| e["line"].as_i64() == Some(1))
        .filter_map(|e| e["column"].as_i64())
        .collect();
    let mut distinct = line1_cols.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct,
        vec![1, 12, 23],
        "expected the three statements on line 1 to surface columns \
         1, 12, 23; got per-event sequence {:?} (deduped {:?})",
        line1_cols,
        distinct
    );
    drop(tmp_dir);
}

#[test]
fn test_column_aware_flag_set_when_no_columns_provided() {
    // Even when every snapshot supplies `column == None` (line-only
    // DWARF), the recorder still opts the writer into column-aware
    // mode so `meta.dat` bit 4 is set unconditionally.  This is the
    // contract column-aware readers rely on — the absence of column
    // info per step is signalled by the *step events* lacking a
    // `column` field, not by a clear flag.
    let test_name = "test_column_aware_flag_set_when_no_columns_provided";
    let Some(ct_print) = ct_print_or_skip(test_name) else {
        return;
    };

    let snaps = vec![snap(0, &[(1, 1)])];
    let source_locs: Vec<(u64, &str, u32, Option<u32>)> =
        vec![(0, "column_aware_test.rs", 2, None)];

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();
    let source_path = test_programs_dir().join("column_aware_test.rs");
    record_from_snapshots_with_columns(&snaps, &source_locs, &source_path, &out_dir).unwrap();

    let ct_files: Vec<_> = std::fs::read_dir(&out_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect();
    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .unwrap();
    assert!(output.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        doc["metadata"]["flags"]["has_column_aware_steps"].as_bool(),
        Some(true),
        "the flag must be set even for traces that carry no per-step columns"
    );
}
