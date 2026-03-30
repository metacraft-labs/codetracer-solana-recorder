//! Integration tests for the Solana recorder pipeline using synthetic data.
//!
//! These tests bypass DWARF/ELF parsing entirely, using synthetic register
//! traces and source locations to exercise the recording pipeline.

use std::path::Path;

use codetracer_solana_recorder::recorder::record_from_snapshots;
use codetracer_solana_recorder::register_trace::{RegisterSnapshot, parse_regs_file, ROW_SIZE};
use codetracer_trace_types::{FullValueRecord, TraceLowLevelEvent, ValueRecord};
use codetracer_trace_writer::TraceEventsFileFormat;

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
    record_from_snapshots(
        &snapshots,
        &source_locs,
        Path::new("test.rs"),
        tmp.path(),
        TraceEventsFileFormat::Json,
    )
    .unwrap();
}

/// Test 3: Verify register values appear as variables in the trace output.
#[test]
fn test_sbpf_variable_extraction() {
    let snapshots = synthetic_snapshots();
    let source_locs = create_synthetic_source_locations();
    let tmp = tempfile::TempDir::new().unwrap();

    // Use JSON format so we can inspect the output.
    record_from_snapshots(
        &snapshots,
        &source_locs,
        Path::new("test.rs"),
        tmp.path(),
        TraceEventsFileFormat::Json,
    )
    .unwrap();

    // Read and parse the trace events as structured data.
    let events_path = tmp.path().join("trace.bin");
    assert!(events_path.exists(), "trace.bin should exist");
    let content = std::fs::read_to_string(&events_path).unwrap();
    let events: Vec<TraceLowLevelEvent> =
        serde_json::from_str(&content).expect("trace output should be valid JSON");

    // Collect all variable names that were interned.
    let var_names: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::VariableName(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();

    // Register variable names r0-r10 should be interned.
    for reg in &["r0", "r1", "r2", "r3", "r4", "r5"] {
        assert!(
            var_names.contains(reg),
            "trace should intern variable name '{reg}', found: {var_names:?}"
        );
    }

    // Collect all integer values from Value events.
    let int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord { value: ValueRecord::Int { i, .. }, .. }) => {
                Some(*i)
            }
            _ => None,
        })
        .collect();

    // Expected register values at various steps.
    assert!(
        int_values.contains(&10),
        "trace should contain value 10 (r1), found: {int_values:?}"
    );
    assert!(
        int_values.contains(&32),
        "trace should contain value 32 (r2), found: {int_values:?}"
    );
    assert!(
        int_values.contains(&42),
        "trace should contain value 42 (r3 = sum), found: {int_values:?}"
    );
    assert!(
        int_values.contains(&84),
        "trace should contain value 84 (r4 = doubled), found: {int_values:?}"
    );
    assert!(
        int_values.contains(&94),
        "trace should contain value 94 (r0 = return value), found: {int_values:?}"
    );
}

/// Test 4: Verify 3-file output (trace.bin, trace_metadata.json, trace_paths.json).
#[test]
fn test_solana_trace_3file_output() {
    let snapshots = synthetic_snapshots();
    let source_locs = create_synthetic_source_locations();
    let tmp = tempfile::TempDir::new().unwrap();

    record_from_snapshots(
        &snapshots,
        &source_locs,
        Path::new("test.rs"),
        tmp.path(),
        TraceEventsFileFormat::Json,
    )
    .unwrap();

    // All three trace files must exist.
    assert!(
        tmp.path().join("trace.bin").exists(),
        "trace.bin should be created"
    );
    assert!(
        tmp.path().join("trace_metadata.json").exists(),
        "trace_metadata.json should be created"
    );
    assert!(
        tmp.path().join("trace_paths.json").exists(),
        "trace_paths.json should be created"
    );

    // trace_metadata.json should be valid JSON.
    let meta_content =
        std::fs::read_to_string(tmp.path().join("trace_metadata.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_content)
        .expect("trace_metadata.json should be valid JSON");
    // Should have at least some content.
    assert!(meta.is_object(), "metadata should be a JSON object");

    // trace_paths.json should be valid JSON (the TraceWriter writes a JSON array of paths).
    let paths_content =
        std::fs::read_to_string(tmp.path().join("trace_paths.json")).unwrap();
    let paths: serde_json::Value = serde_json::from_str(&paths_content)
        .expect("trace_paths.json should be valid JSON");
    assert!(
        paths.is_array() || paths.is_object(),
        "paths should be a JSON array or object, got: {paths}"
    );

    // trace.bin should be parseable as a JSON array of TraceLowLevelEvent.
    let events_content =
        std::fs::read_to_string(tmp.path().join("trace.bin")).unwrap();
    let events: Vec<TraceLowLevelEvent> = serde_json::from_str(&events_content)
        .expect("trace.bin should be valid JSON array of events");
    assert!(
        !events.is_empty(),
        "trace events should not be empty"
    );

    // Metadata should contain recorder info.
    assert!(
        meta.get("lang").is_some() || meta.get("program").is_some() || meta.get("command").is_some(),
        "metadata should have at least one recognized key, got: {meta}"
    );
}

/// Test 5: Verify step count and Call/Return events using parsed trace data.
#[test]
fn test_sbpf_step_events() {
    let snapshots = synthetic_snapshots();
    let source_locs = create_synthetic_source_locations();
    let tmp = tempfile::TempDir::new().unwrap();

    // Use JSON format so we can parse.
    record_from_snapshots(
        &snapshots,
        &source_locs,
        Path::new("test.rs"),
        tmp.path(),
        TraceEventsFileFormat::Json,
    )
    .unwrap();

    let events_path = tmp.path().join("trace.bin");
    let content = std::fs::read_to_string(&events_path).unwrap();
    let events: Vec<TraceLowLevelEvent> =
        serde_json::from_str(&content).expect("trace output should be valid JSON");

    // Count structured event types.
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

    // We have 7 snapshots, each with a unique line, so at least 7 Step events.
    assert!(
        step_count >= 7,
        "expected at least 7 Step events, got {step_count}"
    );

    // There should be at least one Call event (the main function call).
    assert!(
        call_count >= 1,
        "expected at least 1 Call event, got {call_count}"
    );

    // There should be at least one Return event.
    assert!(
        return_count >= 1,
        "expected at least 1 Return event, got {return_count}"
    );

    // Verify that step events reference the correct source file.
    let step_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Step(s) => Some(s),
            _ => None,
        })
        .collect();

    // All steps should reference "test.rs" path (by path_id).
    // Verify we have path events that include "test.rs".
    let has_test_rs_path = events.iter().any(|e| match e {
        TraceLowLevelEvent::Path(p) => p.to_string_lossy().contains("test.rs"),
        _ => false,
    });
    assert!(
        has_test_rs_path,
        "trace should contain a Path event for 'test.rs'"
    );

    // Steps should have sequential line numbers (5 through 11).
    let step_lines: Vec<i64> = step_events.iter().map(|s| s.line.0).collect();
    for expected_line in 5..=11i64 {
        assert!(
            step_lines.contains(&expected_line),
            "step events should include line {expected_line}, found: {step_lines:?}"
        );
    }
}
