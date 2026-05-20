//! Integration tests for the Solana recorder pipeline using synthetic data.
//!
//! These tests bypass DWARF/ELF parsing entirely, using synthetic register
//! traces and source locations to exercise the recording pipeline.

use std::path::{Path, PathBuf};
use std::process::Command;

use codetracer_solana_recorder::recorder::record_from_snapshots;
use codetracer_solana_recorder::register_trace::{parse_regs_file, RegisterSnapshot, ROW_SIZE};
use codetracer_trace_types::{
    CallRecord, FullValueRecord, FunctionId, FunctionRecord, Line, PathId, ReturnRecord,
    StepRecord, TraceLowLevelEvent, TypeId, ValueRecord, VariableId,
};

// ---------------------------------------------------------------------------
// CTFS reader: shells out to `ct-print --full --strip-paths` and translates
// the JSON document into the legacy `TraceLowLevelEvent` stream the
// downstream assertions consume.  Only the variants the tests in this file
// (and `test_comprehensive.rs`) check are emitted — `Int`-typed `Value`
// events, `Step`, `Call`, `Return`, `Function`, `Path`, `VariableName`.
// Compound `ValueRecord` variants (`Sequence` / `Tuple` / `Struct` / ...)
// are intentionally skipped because nothing in these tests asserts on them.
fn parse_events_from_ct(ct_path: &Path) -> Vec<TraceLowLevelEvent> {
    // `EXE_SUFFIX` is "" on Unix and ".exe" on Windows -- the Nim build
    // emits `ct-print.exe` there, so an extensionless path would fail the
    // `.exists()` check even though `Command::new` would still resolve it.
    let ct_print = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join(format!("ct-print{}", std::env::consts::EXE_SUFFIX));
    assert!(
        ct_print.exists(),
        "ct-print binary not found at {}; tests require the \
         codetracer-trace-format-nim sibling repo",
        ct_print.display()
    );
    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(ct_path)
        .output()
        .expect("failed to invoke ct-print");
    assert!(
        output.status.success(),
        "ct-print --full failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full must emit valid JSON");

    let mut events: Vec<TraceLowLevelEvent> = Vec::new();

    for p in doc["paths"].as_array().expect("paths array").iter() {
        events.push(TraceLowLevelEvent::Path(PathBuf::from(
            p.as_str().expect("path is string"),
        )));
    }
    for v in doc["varnames"].as_array().expect("varnames array").iter() {
        events.push(TraceLowLevelEvent::VariableName(
            v.as_str().expect("varname is string").to_string(),
        ));
    }
    for f in doc["functions"].as_array().expect("functions array").iter() {
        events.push(TraceLowLevelEvent::Function(FunctionRecord {
            path_id: PathId(0),
            line: Line(1),
            name: f.as_str().expect("function is string").to_string(),
        }));
    }
    for ev in doc["events"].as_array().expect("events array").iter() {
        match ev["kind"].as_str().expect("kind str") {
            "step" => {
                let path_id = ev["path_id"].as_u64().expect("path_id u64") as usize;
                let line = ev["line"].as_i64().expect("line i64");
                events.push(TraceLowLevelEvent::Step(StepRecord {
                    path_id: PathId(path_id),
                    line: Line(line),
                }));
                if let Some(vars) = ev["vars"].as_array() {
                    for v in vars {
                        let value = &v["value"];
                        if value["kind"].as_str() == Some("Int") {
                            let i = value["i"].as_i64().expect("Int.i must be i64");
                            let type_id = value["type_id"].as_u64().unwrap_or(0) as usize;
                            let varname_id =
                                v["varname_id"].as_u64().expect("varname_id u64") as usize;
                            events.push(TraceLowLevelEvent::Value(FullValueRecord {
                                variable_id: VariableId(varname_id),
                                value: ValueRecord::Int {
                                    i,
                                    type_id: TypeId(type_id),
                                },
                            }));
                        }
                    }
                }
            }
            "call_entry" => {
                let function_id = ev["function_id"].as_u64().expect("function_id u64") as usize;
                events.push(TraceLowLevelEvent::Call(CallRecord {
                    function_id: FunctionId(function_id),
                    args: vec![],
                }));
            }
            "call_exit" => {
                events.push(TraceLowLevelEvent::Return(ReturnRecord {
                    return_value: ValueRecord::None { type_id: TypeId(0) },
                }));
            }
            _ => {} // `io` and any future kinds are not asserted on by these tests.
        }
    }

    events
}

/// Read the single `.ct` file in `dir` and parse its events via ct-print.
fn read_ct_events(dir: &Path) -> Vec<TraceLowLevelEvent> {
    let ct_files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ct"))
        .collect();
    assert_eq!(ct_files.len(), 1, "expected exactly one .ct file");
    parse_events_from_ct(&ct_files[0])
}

// ---------------------------------------------------------------------------
// Synthetic test data helpers
// ---------------------------------------------------------------------------

/// Build a synthetic `.regs` binary blob simulating a 7-instruction program.
fn create_synthetic_regs() -> Vec<u8> {
    let mut data = Vec::new();
    for step in 0..7u64 {
        let mut regs = [0u64; 12];
        regs[11] = step; // PC
        match step {
            0 => {
                regs[1] = 10;
            }
            1 => {
                regs[1] = 10;
                regs[2] = 32;
            }
            2 => {
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
            }
            3 => {
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
                regs[4] = 84;
            }
            4 => {
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
                regs[4] = 84;
                regs[5] = 94;
            }
            5 => {
                regs[0] = 94;
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
                regs[4] = 84;
                regs[5] = 94;
            }
            6 => {
                regs[0] = 94;
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
                regs[4] = 84;
                regs[5] = 94;
            }
            _ => {}
        }
        for r in &regs {
            data.extend_from_slice(&r.to_le_bytes());
        }
    }
    data
}

/// Build synthetic source locations for the 7-step program.
fn create_synthetic_source_locations() -> Vec<(u64, &'static str, u32)> {
    vec![
        (0, "test.rs", 5),  // let a = 10
        (1, "test.rs", 6),  // let b = 32
        (2, "test.rs", 7),  // let sum = a + b
        (3, "test.rs", 8),  // let doubled = sum * 2
        (4, "test.rs", 9),  // let final_val = doubled + a
        (5, "test.rs", 10), // log(final_val)
        (6, "test.rs", 11), // return
    ]
}

/// Parse synthetic regs data into snapshots (convenience).
fn synthetic_snapshots() -> Vec<RegisterSnapshot> {
    parse_regs_file(&create_synthetic_regs()).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1: Parse a synthetic `.regs` file and verify register values.
#[test]
fn test_sbpf_register_trace_parsing() {
    let data = create_synthetic_regs();
    assert_eq!(data.len(), 7 * ROW_SIZE);

    let snapshots = parse_regs_file(&data).unwrap();
    assert_eq!(snapshots.len(), 7);

    // Step 0: PC=0, r1=10
    assert_eq!(snapshots[0].pc(), 0);
    assert_eq!(snapshots[0].reg(1), 10);
    assert_eq!(snapshots[0].r0(), 0);

    // Step 2: PC=2, r3=42 (sum)
    assert_eq!(snapshots[2].pc(), 2);
    assert_eq!(snapshots[2].reg(3), 42);

    // Step 5: PC=5, r0=94 (return value set)
    assert_eq!(snapshots[5].pc(), 5);
    assert_eq!(snapshots[5].r0(), 94);

    // Step 6: PC=6, same r0
    assert_eq!(snapshots[6].pc(), 6);
    assert_eq!(snapshots[6].r0(), 94);
}

/// Test 2: Verify Step events map to correct source lines.
#[test]
fn test_sbpf_source_mapping() {
    let snapshots = synthetic_snapshots();
    let source_locs = create_synthetic_source_locations();

    // Each snapshot has a unique PC (0-6) and each PC maps to a unique line (5-11).
    for (snap, &(expected_pc, expected_file, expected_line)) in
        snapshots.iter().zip(source_locs.iter())
    {
        assert_eq!(snap.pc(), expected_pc);
        assert_eq!(expected_file, "test.rs");
        // Lines go from 5 to 11.
        assert!(expected_line >= 5 && expected_line <= 11);
    }

    // Verify the mapping is sequential.
    for i in 0..source_locs.len() {
        assert_eq!(source_locs[i].2, 5 + i as u32);
    }

    // Run through the recorder to ensure it does not panic.
    let tmp = tempfile::TempDir::new().unwrap();
    record_from_snapshots(&snapshots, &source_locs, Path::new("test.rs"), tmp.path()).unwrap();
}

/// Test 3: Verify register values appear as variables in the trace output.
#[test]
fn test_sbpf_variable_extraction() {
    let snapshots = synthetic_snapshots();
    let source_locs = create_synthetic_source_locations();
    let tmp = tempfile::TempDir::new().unwrap();

    record_from_snapshots(&snapshots, &source_locs, Path::new("test.rs"), tmp.path()).unwrap();

    let events = read_ct_events(tmp.path());

    // Variable-name table: the recorder always emits the full r0..r10 set on
    // every snapshot (see `record_from_snapshots_into_writer`), so the
    // interning order is fixed and total.
    let var_names: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::VariableName(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        var_names,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10"],
    );

    // Integer values across every Value event, deduplicated and sorted —
    // this is the strict spec of "what register values appear in the trace"
    // for the canonical 7-snapshot fixture.
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, 10, 32, 42, 84, 94]);
}

/// Test 4: Verify 3-file output (trace.json, trace_metadata.json, trace_paths.json).
#[test]
fn test_solana_trace_3file_output() {
    let snapshots = synthetic_snapshots();
    let source_locs = create_synthetic_source_locations();
    let tmp = tempfile::TempDir::new().unwrap();

    record_from_snapshots(&snapshots, &source_locs, Path::new("test.rs"), tmp.path()).unwrap();

    // Verify .ct output with CTFS magic bytes.
    let ct_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ct"))
        .collect();
    assert!(!ct_files.is_empty(), "expected .ct file");
    let ct_content = std::fs::read(&ct_files[0]).unwrap();
    assert!(ct_content.len() >= 5);
    assert_eq!(&ct_content[..5], &[0xC0u8, 0xDE, 0x72, 0xAC, 0xE2]);
}

/// Test 5: Verify step count and Call/Return events using parsed trace data.
#[test]
fn test_sbpf_step_events() {
    let snapshots = synthetic_snapshots();
    let source_locs = create_synthetic_source_locations();
    let tmp = tempfile::TempDir::new().unwrap();

    record_from_snapshots(&snapshots, &source_locs, Path::new("test.rs"), tmp.path()).unwrap();

    let events = read_ct_events(tmp.path());

    let step_count = events
        .iter()
        .filter(|e| matches!(e, TraceLowLevelEvent::Step(_)))
        .count();
    let call_count = events
        .iter()
        .filter(|e| matches!(e, TraceLowLevelEvent::Call(_)))
        .count();
    let return_count = events
        .iter()
        .filter(|e| matches!(e, TraceLowLevelEvent::Return(_)))
        .count();

    // 1 implicit start step at line 1 + 7 line-changes (5..=11) = 8 step events.
    assert_eq!(step_count, 8);
    // 1 outer "main" frame -> 1 call_entry, 1 call_exit.
    assert_eq!(call_count, 1);
    assert_eq!(return_count, 1);

    // Path table is exactly the single fixture file.
    let paths: Vec<&Path> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Path(p) => Some(p.as_path()),
            _ => None,
        })
        .collect();
    assert_eq!(paths, vec![Path::new("test.rs")]);

    // Step lines in event order: implicit start at 1, then snapshots' 5..11.
    let step_lines: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Step(s) => Some(s.line.0),
            _ => None,
        })
        .collect();
    assert_eq!(step_lines, vec![1i64, 5, 6, 7, 8, 9, 10, 11]);
}
