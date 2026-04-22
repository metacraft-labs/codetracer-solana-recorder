//! Integration tests for the `SbpfTracer` trait and its implementations.

use std::path::Path;

use codetracer_solana_recorder::register_trace::{RegisterSnapshot, parse_regs_file, ROW_SIZE};
use codetracer_solana_recorder::tracer_trait::{
    CodeTracerTracer, NoOpTracer, RegisterTraceTracer, SbpfTracer, replay_snapshots,
};
use codetracer_trace_writer_nim::TraceEventsFileFormat;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal synthetic `.regs` binary blob (3 instructions).
fn small_synthetic_regs() -> Vec<u8> {
    let mut data = Vec::new();
    for step in 0..3u64 {
        let mut regs = [0u64; 12];
        regs[11] = step; // PC
        match step {
            0 => regs[1] = 10,
            1 => {
                regs[1] = 10;
                regs[2] = 20;
            }
            2 => {
                regs[0] = 30;
                regs[1] = 10;
                regs[2] = 20;
            }
            _ => {}
        }
        for r in &regs {
            data.extend_from_slice(&r.to_le_bytes());
        }
    }
    data
}

fn small_snapshots() -> Vec<RegisterSnapshot> {
    parse_regs_file(&small_synthetic_regs()).unwrap()
}

fn small_source_locations() -> Vec<(u64, String, u32)> {
    vec![
        (0, "test.rs".to_string(), 1),
        (1, "test.rs".to_string(), 2),
        (2, "test.rs".to_string(), 3),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// NoOpTracer is a zero-sized type.
#[test]
fn test_noop_tracer_zero_size() {
    assert_eq!(std::mem::size_of::<NoOpTracer>(), 0);
}

/// All NoOpTracer methods are callable without panic.
#[test]
fn test_noop_tracer_callable() {
    let mut tracer = NoOpTracer;
    let regs = [0u64; 12];
    let program_id = [0u8; 32];

    tracer.on_start(0);
    tracer.on_step(42, 0xABCD, &regs);
    tracer.on_syscall("sol_log", &regs);
    tracer.on_memory_read(0x1000, 8, &[1, 2, 3, 4, 5, 6, 7, 8]);
    tracer.on_memory_write(0x2000, 4, &[0xDE, 0xAD, 0xBE, 0xEF]);
    tracer.on_cpi_call(&program_id, &[]);
    tracer.on_cpi_return(&program_id, 99);
    tracer.on_exit(0);
}

/// CodeTracerTracer records step events into trace files.
#[test]
fn test_codetracer_tracer_records_steps() {
    let tmp = tempfile::TempDir::new().unwrap();
    let snapshots = small_snapshots();
    let source_locs = small_source_locations();

    let mut tracer = CodeTracerTracer::new(
        Path::new("test.rs"),
        tmp.path(),
        TraceEventsFileFormat::Json,
        source_locs,
    )
    .unwrap();

    // Feed steps.
    for snap in &snapshots {
        tracer.on_step(snap.pc(), 0, &snap.registers);
    }
    tracer.finish().unwrap();

    // Verify trace files exist.
    assert!(tmp.path().join("trace.json").exists());
    assert!(tmp.path().join("trace_metadata.json").exists());
    assert!(tmp.path().join("trace_paths.json").exists());

    // Verify content has Step events.
    let content = std::fs::read_to_string(tmp.path().join("trace.json")).unwrap();
    let step_count = content.matches("\"Step\"").count();
    assert!(
        step_count >= 3,
        "expected at least 3 Step events, got {step_count}"
    );

    // Verify register values appear.
    assert!(content.contains("r1"), "should contain r1 variable");
    assert!(content.contains("10"), "should contain value 10");
}

/// CodeTracerTracer records syscall events.
#[test]
fn test_codetracer_tracer_records_syscalls() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source_locs = small_source_locations();

    let mut tracer = CodeTracerTracer::new(
        Path::new("test.rs"),
        tmp.path(),
        TraceEventsFileFormat::Json,
        source_locs,
    )
    .unwrap();

    let regs = [0u64; 12];
    tracer.on_syscall("sol_log", &regs);
    tracer.on_syscall("sol_invoke_signed", &regs);
    tracer.finish().unwrap();

    let syscalls = tracer.recorded_syscalls();
    assert_eq!(syscalls.len(), 2);
    assert_eq!(syscalls[0], "sol_log");
    assert_eq!(syscalls[1], "sol_invoke_signed");
}

/// RegisterTraceTracer produces correct binary .regs output.
#[test]
fn test_register_trace_tracer_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    let regs_path = tmp.path().join("output.regs");

    let mut tracer = RegisterTraceTracer::new(&regs_path).unwrap();

    // Write two snapshots.
    let regs1: [u64; 12] = [100, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0];
    let regs2: [u64; 12] = [200, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 1];

    tracer.on_step(0, 0, &regs1);
    tracer.on_step(1, 0, &regs2);
    let rows = tracer.finish().unwrap();
    assert_eq!(rows, 2);

    // Read back and verify binary format.
    let data = std::fs::read(&regs_path).unwrap();
    assert_eq!(data.len(), 2 * ROW_SIZE);

    let snapshots = parse_regs_file(&data).unwrap();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].registers, regs1);
    assert_eq!(snapshots[1].registers, regs2);
}

/// SbpfTracer is trait-object safe: we can use Box<dyn SbpfTracer>.
#[test]
fn test_tracer_trait_polymorphism() {
    let regs = [0u64; 12];
    let program_id = [0u8; 32];

    // Use NoOpTracer as a trait object.
    let mut tracer: Box<dyn SbpfTracer> = Box::new(NoOpTracer);
    tracer.on_start(0);
    tracer.on_step(0, 0, &regs);
    tracer.on_syscall("test", &regs);
    tracer.on_memory_read(0, 1, &[0]);
    tracer.on_memory_write(0, 1, &[0]);
    tracer.on_cpi_call(&program_id, &[]);
    tracer.on_cpi_return(&program_id, 0);
    tracer.on_exit(0);

    // Use RegisterTraceTracer as a trait object.
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("poly.regs");
    let mut reg_tracer: Box<dyn SbpfTracer> = Box::new(RegisterTraceTracer::new(&path).unwrap());
    reg_tracer.on_step(0, 0, &regs);
    reg_tracer.on_exit(0);

    // Verify the file was created.
    assert!(path.exists());
}

/// replay_snapshots feeds snapshots through a tracer correctly.
#[test]
fn test_replay_snapshots_through_noop() {
    let snapshots = small_snapshots();
    let mut tracer = NoOpTracer;
    // Should not panic.
    replay_snapshots(&mut tracer, &snapshots);
}

/// replay_snapshots with RegisterTraceTracer produces correct output.
#[test]
fn test_replay_snapshots_through_register_tracer() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("replay.regs");
    let snapshots = small_snapshots();

    let mut tracer = RegisterTraceTracer::new(&path).unwrap();
    replay_snapshots(&mut tracer, &snapshots);
    let rows = tracer.finish().unwrap();
    assert_eq!(rows, 3);

    let data = std::fs::read(&path).unwrap();
    let parsed = parse_regs_file(&data).unwrap();
    assert_eq!(parsed.len(), 3);
    // Verify round-trip: register values match.
    for (original, parsed) in snapshots.iter().zip(parsed.iter()) {
        assert_eq!(original.registers, parsed.registers);
    }
}
