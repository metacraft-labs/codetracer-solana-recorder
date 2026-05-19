//! Tests for CPI (Cross-Program Invocation) detection and multi-program support.

use std::path::Path;

use codetracer_solana_recorder::cpi::{CpiDetector, CpiEvent};
use codetracer_solana_recorder::multi_program::ProgramRegistry;
use codetracer_solana_recorder::recorder::record_with_cpi;
use codetracer_solana_recorder::register_trace::RegisterSnapshot;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a register snapshot with a given PC and zeroed registers.
fn snap_with_pc(pc: u64) -> RegisterSnapshot {
    let mut registers = [0u64; 12];
    registers[11] = pc;
    RegisterSnapshot { registers }
}

/// Build a register snapshot with a given PC and r0 value.
fn snap_with_pc_and_r0(pc: u64, r0: u64) -> RegisterSnapshot {
    let mut registers = [0u64; 12];
    registers[0] = r0;
    registers[11] = pc;
    RegisterSnapshot { registers }
}

// ---------------------------------------------------------------------------
// CPI detector tests
// ---------------------------------------------------------------------------

/// PC stays within the primary program range -- no CPI events.
#[test]
fn test_cpi_detector_same_program() {
    let mut detector = CpiDetector::new(0..100);

    assert_eq!(
        detector.process_snapshot(&snap_with_pc(0)),
        CpiEvent::SameProgram
    );
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(1)),
        CpiEvent::SameProgram
    );
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(50)),
        CpiEvent::SameProgram
    );
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(99)),
        CpiEvent::SameProgram
    );

    assert_eq!(detector.call_depth(), 0);
    assert!(!detector.is_in_cpi());
}

/// PC jumps outside the primary range -- CPI call detected.
#[test]
fn test_cpi_detector_call() {
    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("token_program", 10000..10100);

    // Start in primary program.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(5)),
        CpiEvent::SameProgram
    );
    assert_eq!(detector.call_depth(), 0);

    // Jump to token program.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(10005)),
        CpiEvent::CpiCall { target_pc: 10005 }
    );
    assert_eq!(detector.call_depth(), 1);
    assert!(detector.is_in_cpi());
    assert_eq!(detector.current_program(), "token_program");
}

/// PC returns to primary range after a CPI -- CPI return detected.
#[test]
fn test_cpi_detector_return() {
    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("token_program", 10000..10100);

    // Enter CPI.
    detector.process_snapshot(&snap_with_pc(5));
    detector.process_snapshot(&snap_with_pc(10005));
    assert!(detector.is_in_cpi());

    // Execute inside CPI.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(10010)),
        CpiEvent::SameProgram
    );

    // Return to primary.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(6)),
        CpiEvent::CpiReturn { return_pc: 6 }
    );
    assert_eq!(detector.call_depth(), 0);
    assert!(!detector.is_in_cpi());
    assert_eq!(detector.current_program(), "primary");
}

/// Nested CPI: primary -> program A -> program B -> return B -> return A.
#[test]
fn test_cpi_nested_calls() {
    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("program_a", 1000..1100);
    detector.add_program_range("program_b", 2000..2100);

    // Primary program.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(10)),
        CpiEvent::SameProgram
    );
    assert_eq!(detector.call_depth(), 0);

    // CPI into program A.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(1010)),
        CpiEvent::CpiCall { target_pc: 1010 }
    );
    assert_eq!(detector.call_depth(), 1);
    assert_eq!(detector.current_program(), "program_a");

    // Execute in program A.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(1015)),
        CpiEvent::SameProgram
    );

    // CPI from program A into program B.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(2005)),
        CpiEvent::CpiCall { target_pc: 2005 }
    );
    assert_eq!(detector.call_depth(), 2);
    assert_eq!(detector.current_program(), "program_b");

    // Execute in program B.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(2010)),
        CpiEvent::SameProgram
    );

    // Return from program B to program A.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(1020)),
        CpiEvent::CpiReturn { return_pc: 1020 }
    );
    assert_eq!(detector.call_depth(), 1);
    assert_eq!(detector.current_program(), "program_a");

    // Return from program A to primary.
    assert_eq!(
        detector.process_snapshot(&snap_with_pc(15)),
        CpiEvent::CpiReturn { return_pc: 15 }
    );
    assert_eq!(detector.call_depth(), 0);
    assert!(!detector.is_in_cpi());
}

// ---------------------------------------------------------------------------
// Program registry tests
// ---------------------------------------------------------------------------

/// Register multiple programs and verify correct lookup by PC.
#[test]
fn test_program_registry_lookup() {
    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program(
        "primary",
        0..100,
        vec![
            (0, "primary.rs".to_string(), 1),
            (5, "primary.rs".to_string(), 10),
        ],
    );
    registry.add_synthetic_program(
        "token_program",
        10000..10100,
        vec![
            (10000, "token.rs".to_string(), 1),
            (10005, "token.rs".to_string(), 15),
        ],
    );
    registry.add_synthetic_program(
        "system_program",
        20000..20100,
        vec![(20000, "system.rs".to_string(), 1)],
    );

    // Primary program lookups.
    assert_eq!(registry.program_name(0), Some("primary"));
    assert_eq!(registry.program_name(5), Some("primary"));
    assert_eq!(
        registry.find_location(5),
        Some(("primary.rs".to_string(), 10))
    );

    // Token program lookups.
    assert_eq!(registry.program_name(10005), Some("token_program"));
    assert_eq!(
        registry.find_location(10005),
        Some(("token.rs".to_string(), 15))
    );

    // System program lookups.
    assert_eq!(registry.program_name(20000), Some("system_program"));

    // Unknown PC.
    assert_eq!(registry.program_name(50000), None);
    assert_eq!(registry.find_location(50000), None);

    // Range lookups.
    assert_eq!(registry.program_range("primary"), Some(0..100));
    assert_eq!(registry.program_range("nonexistent"), None);
}

// ---------------------------------------------------------------------------
// End-to-end CPI trace output test
// ---------------------------------------------------------------------------

/// Create synthetic register data with CPI boundaries and verify that the
/// trace output contains nested Call/Return events.
#[test]
fn test_cpi_trace_output() {
    // Simulate: primary(pc=0,1,2) -> CPI to token(pc=10000,10001) -> return primary(pc=3,4)
    let snapshots = vec![
        snap_with_pc_and_r0(0, 0),
        snap_with_pc_and_r0(1, 0),
        snap_with_pc_and_r0(2, 0),
        snap_with_pc_and_r0(10000, 100), // CPI call
        snap_with_pc_and_r0(10001, 200), // Inside CPI
        snap_with_pc_and_r0(3, 0),       // CPI return
        snap_with_pc_and_r0(4, 42),      // Back in primary
    ];

    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program(
        "primary",
        0..100,
        vec![
            (0, "primary.rs".to_string(), 5),
            (1, "primary.rs".to_string(), 6),
            (2, "primary.rs".to_string(), 7),
            (3, "primary.rs".to_string(), 8),
            (4, "primary.rs".to_string(), 9),
        ],
    );
    registry.add_synthetic_program(
        "token_program",
        10000..10100,
        vec![
            (10000, "token.rs".to_string(), 10),
            (10001, "token.rs".to_string(), 11),
        ],
    );

    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("token_program", 10000..10100);

    let tmp = tempfile::TempDir::new().unwrap();
    record_with_cpi(
        &snapshots,
        &registry,
        &mut detector,
        Path::new("primary.rs"),
        tmp.path(),
    )
    .unwrap();

    // Verify .ct output with CTFS magic bytes.
    let ct_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ct"))
        .collect();
    assert!(!ct_files.is_empty(), "expected .ct file in CPI output");
    let ct_content = std::fs::read(&ct_files[0]).unwrap();
    assert!(ct_content.len() >= 5, ".ct file too small");
    assert_eq!(&ct_content[..5], &[0xC0u8, 0xDE, 0x72, 0xAC, 0xE2]);
}
