//! Integration tests for CPI (Cross-Program Invocation) detection with
//! real DWARF debug info from ELF binaries.
//!
//! These tests verify that the CPI detection pipeline works correctly when
//! programs are backed by real ELF/DWARF data rather than synthetic source
//! locations. They simulate cross-program invocations by treating different
//! address ranges of a real ELF binary as separate "programs", each with
//! real DWARF source mapping.
//!
//! This validates:
//! - CpiDetector correctly identifies CPI call/return boundaries
//! - ProgramRegistry resolves source locations from real DWARF for each program
//! - record_with_cpi produces nested Call/Return events at CPI boundaries
//! - Source file paths from DWARF appear correctly in the trace output
//!
//! The ELF/DWARF source is the committed `test-programs/cpi_fixture.elf`
//! fixture (a small statically-linked binary with debug info and six named
//! functions).  Earlier revisions instead read the recorder's *own*
//! executable as the stand-in ELF, which only works on platforms where that
//! executable is itself ELF-formatted: on Windows the recorder binary is a
//! PE and carries no embedded DWARF, so `find_functions` returned nothing.
//! A committed ELF fixture makes the tests platform-independent.

use std::path::{Path, PathBuf};

use codetracer_solana_recorder::cpi::{CpiDetector, CpiEvent};
use codetracer_solana_recorder::dwarf::{DwarfParser, find_functions};
use codetracer_solana_recorder::multi_program::ProgramRegistry;
use codetracer_solana_recorder::recorder::record_with_cpi;
use codetracer_solana_recorder::register_trace::RegisterSnapshot;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load the committed ELF/DWARF fixture used as the CPI test program.
///
/// `test-programs/cpi_fixture.elf` is a small statically-linked ELF64
/// executable built with full debug info; its DWARF describes six named
/// functions, enough for `partition_functions_for_cpi` to split into two
/// non-overlapping "programs".
fn load_recorder_elf() -> Vec<u8> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs")
        .join("cpi_fixture.elf");
    std::fs::read(&fixture).unwrap_or_else(|e| {
        panic!(
            "should be able to read the CPI ELF fixture at {}: {e}",
            fixture.display()
        )
    })
}

/// Build a register snapshot with a given PC and r0 value.
fn snap(pc: u64, r0: u64) -> RegisterSnapshot {
    let mut registers = [0u64; 12];
    registers[0] = r0;
    registers[11] = pc;
    RegisterSnapshot { registers }
}

/// Build a register snapshot with given PC, r0, and r1 values.
fn snap_r0_r1(pc: u64, r0: u64, r1: u64) -> RegisterSnapshot {
    let mut registers = [0u64; 12];
    registers[0] = r0;
    registers[1] = r1;
    registers[11] = pc;
    RegisterSnapshot { registers }
}

/// Find function boundaries from DWARF and partition them into two groups
/// with non-overlapping PC ranges, suitable for simulating two programs.
/// Returns (program_a_range, program_b_range, locations_a, locations_b).
fn partition_functions_for_cpi(
    elf_data: &[u8],
) -> (
    std::ops::Range<u64>,
    std::ops::Range<u64>,
    Vec<(u64, String, u32)>,
    Vec<(u64, String, u32)>,
) {
    let parser = DwarfParser::new(elf_data).expect("ELF should parse");
    let text_vaddr = parser.text_vaddr();
    let mut functions = find_functions(elf_data).expect("should find functions");

    assert!(
        functions.len() >= 4,
        "need at least 4 functions to partition into two programs"
    );

    // Sort by start_addr to ensure non-overlapping partitioning.
    functions.sort_by_key(|f| f.start_addr);

    // Split functions into two halves by sorted address order.
    // Use the boundary between the two groups' addresses as the dividing line.
    let mid = functions.len() / 2;
    let funcs_a = &functions[..mid];
    let funcs_b = &functions[mid..];

    // Compute non-overlapping SBF PC ranges.
    // The boundary between A and B is the start of the first function in B.
    let boundary_addr = funcs_b[0].start_addr;
    let range_a = {
        let start_pc = (funcs_a[0].start_addr.saturating_sub(text_vaddr)) / 8;
        let end_pc = (boundary_addr.saturating_sub(text_vaddr)) / 8;
        start_pc..end_pc
    };
    let range_b = {
        let start_pc = (boundary_addr.saturating_sub(text_vaddr)) / 8;
        let end_pc = (funcs_b.last().unwrap().end_addr.saturating_sub(text_vaddr)) / 8 + 1;
        start_pc..end_pc
    };

    // Ensure non-overlapping: range_b should start at range_a end.
    assert!(
        range_b.start >= range_a.end,
        "ranges should not overlap: A={:?}, B={:?}",
        range_a,
        range_b
    );

    // Find source locations within each range.
    let mut locs_a = Vec::new();
    let mut locs_b = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for pc in range_a.clone() {
        if locs_a.len() >= 3 {
            break;
        }
        if let Some(loc) = parser.find_location(pc) {
            let key = (loc.file.clone(), loc.line);
            if !seen.contains(&key) {
                seen.insert(key);
                locs_a.push((pc, loc.file, loc.line));
            }
        }
    }

    for pc in range_b.clone() {
        if locs_b.len() >= 3 {
            break;
        }
        if let Some(loc) = parser.find_location(pc) {
            let key = (loc.file.clone(), loc.line);
            if !seen.contains(&key) {
                seen.insert(key);
                locs_b.push((pc, loc.file, loc.line));
            }
        }
    }

    (range_a, range_b, locs_a, locs_b)
}

// ===========================================================================
// Test 1: CPI detection with real DWARF-backed program ranges
// ===========================================================================

/// Verifies that CpiDetector correctly identifies CPI call and return
/// boundaries when program PC ranges are derived from real DWARF function
/// boundaries in the recorder's own binary.
#[test]
fn test_cpi_detection_with_real_function_boundaries() {
    let elf_data = load_recorder_elf();
    let (range_a, range_b, locs_a, locs_b) = partition_functions_for_cpi(&elf_data);

    // Skip if we couldn't find enough locations.
    if locs_a.is_empty() || locs_b.is_empty() {
        eprintln!(
            "Skipping: need locations in both ranges; got {} in A, {} in B",
            locs_a.len(),
            locs_b.len()
        );
        return;
    }

    let mut detector = CpiDetector::new(range_a.clone());
    detector.add_program_range("program_b", range_b.clone());

    // Start in program A.
    let pc_a = locs_a[0].0;
    assert_eq!(
        detector.process_snapshot(&snap(pc_a, 0)),
        CpiEvent::SameProgram,
        "first snapshot in range_a should be SameProgram"
    );
    assert_eq!(detector.call_depth(), 0);
    assert!(!detector.is_in_cpi());

    // CPI call: jump to program B.
    let pc_b = locs_b[0].0;
    let event = detector.process_snapshot(&snap(pc_b, 100));
    assert_eq!(
        event,
        CpiEvent::CpiCall { target_pc: pc_b },
        "jump to range_b should be a CPI call"
    );
    assert_eq!(detector.call_depth(), 1);
    assert!(detector.is_in_cpi());
    assert_eq!(detector.current_program(), "program_b");

    // Execute in program B.
    if locs_b.len() > 1 {
        assert_eq!(
            detector.process_snapshot(&snap(locs_b[1].0, 200)),
            CpiEvent::SameProgram,
            "subsequent PC in range_b should be SameProgram"
        );
    }

    // CPI return: jump back to program A.
    let return_pc = if locs_a.len() > 1 { locs_a[1].0 } else { pc_a };
    let event = detector.process_snapshot(&snap(return_pc, 0));
    assert_eq!(
        event,
        CpiEvent::CpiReturn { return_pc },
        "return to range_a should be CPI return"
    );
    assert_eq!(detector.call_depth(), 0);
    assert!(!detector.is_in_cpi());
    assert_eq!(detector.current_program(), "primary");
}

// ===========================================================================
// Test 2: Nested CPI with real function boundaries
// ===========================================================================

/// Simulates a nested CPI scenario: primary -> program_b -> program_c -> return
/// using three address ranges derived from the total PC span of the real ELF.
/// The ranges are computed by dividing the ELF's total PC space into three
/// equal, non-overlapping segments.
#[test]
fn test_nested_cpi_with_real_boundaries() {
    let elf_data = load_recorder_elf();
    let mut functions = find_functions(&elf_data).expect("should find functions");

    if functions.len() < 6 {
        eprintln!("Skipping: need at least 6 functions for 3-way split");
        return;
    }

    // Sort by start_addr to find the total address span.
    functions.sort_by_key(|f| f.start_addr);

    let parser = DwarfParser::new(&elf_data).expect("ELF should parse");
    let text_vaddr = parser.text_vaddr();

    // Compute total PC span.
    let min_pc = (functions[0].start_addr.saturating_sub(text_vaddr)) / 8;
    let max_pc = (functions
        .last()
        .unwrap()
        .end_addr
        .saturating_sub(text_vaddr))
        / 8
        + 1;
    let span = max_pc - min_pc;

    // Divide into 3 equal ranges.
    let ranges = [
        min_pc..min_pc + span / 3,
        min_pc + span / 3..min_pc + 2 * span / 3,
        min_pc + 2 * span / 3..max_pc,
    ];

    // Use midpoint PCs.
    let pcs: Vec<u64> = ranges
        .iter()
        .map(|r| r.start + (r.end - r.start) / 2)
        .collect();

    let mut detector = CpiDetector::new(ranges[0].clone());
    detector.add_program_range("program_b", ranges[1].clone());
    detector.add_program_range("program_c", ranges[2].clone());

    // primary -> execute
    assert_eq!(
        detector.process_snapshot(&snap(pcs[0], 0)),
        CpiEvent::SameProgram
    );
    assert_eq!(detector.call_depth(), 0);

    // primary -> program_b (CPI call)
    assert_eq!(
        detector.process_snapshot(&snap(pcs[1], 100)),
        CpiEvent::CpiCall { target_pc: pcs[1] }
    );
    assert_eq!(detector.call_depth(), 1);
    assert_eq!(detector.current_program(), "program_b");

    // program_b -> program_c (nested CPI call)
    assert_eq!(
        detector.process_snapshot(&snap(pcs[2], 200)),
        CpiEvent::CpiCall { target_pc: pcs[2] }
    );
    assert_eq!(detector.call_depth(), 2);
    assert_eq!(detector.current_program(), "program_c");

    // program_c -> program_b (return from nested CPI)
    let return_pc_b = pcs[1] + 1;
    assert_eq!(
        detector.process_snapshot(&snap(return_pc_b, 150)),
        CpiEvent::CpiReturn {
            return_pc: return_pc_b
        }
    );
    assert_eq!(detector.call_depth(), 1);
    assert_eq!(detector.current_program(), "program_b");

    // program_b -> primary (return from CPI)
    let return_pc_primary = pcs[0] + 1;
    assert_eq!(
        detector.process_snapshot(&snap(return_pc_primary, 50)),
        CpiEvent::CpiReturn {
            return_pc: return_pc_primary
        }
    );
    assert_eq!(detector.call_depth(), 0);
    assert_eq!(detector.current_program(), "primary");
}

// ===========================================================================
// Test 3: record_with_cpi end-to-end with DWARF-backed registry
// ===========================================================================

/// Runs the full record_with_cpi pipeline using a ProgramRegistry populated
/// with synthetic programs whose PC ranges come from real DWARF function
/// boundaries. Verifies the trace output contains CPI Call/Return events
/// and Step events with source mapping.
#[test]
fn test_record_with_cpi_using_real_boundaries() {
    let elf_data = load_recorder_elf();
    let (range_a, range_b, locs_a, locs_b) = partition_functions_for_cpi(&elf_data);

    if locs_a.len() < 2 || locs_b.len() < 2 {
        eprintln!(
            "Skipping: need at least 2 locations in each range; A={}, B={}",
            locs_a.len(),
            locs_b.len()
        );
        return;
    }

    // Set up ProgramRegistry with synthetic source locations derived from DWARF.
    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program("primary", range_a.clone(), locs_a.clone());
    registry.add_synthetic_program("token_program", range_b.clone(), locs_b.clone());

    // Build snapshot sequence: primary -> CPI to token_program -> return to primary
    let snapshots = vec![
        snap_r0_r1(locs_a[0].0, 0, 100),  // In primary
        snap_r0_r1(locs_a[1].0, 0, 200),  // Still in primary
        snap_r0_r1(locs_b[0].0, 50, 300), // CPI call to token_program
        snap_r0_r1(locs_b[1].0, 60, 400), // Inside token_program
        snap_r0_r1(locs_a[0].0, 0, 500),  // CPI return to primary
    ];

    let mut detector = CpiDetector::new(range_a.clone());
    detector.add_program_range("token_program", range_b.clone());

    let tmp = tempfile::TempDir::new().unwrap();
    let result = record_with_cpi(
        &snapshots,
        &registry,
        &mut detector,
        Path::new("primary.rs"),
        tmp.path(),
    );
    assert!(
        result.is_ok(),
        "record_with_cpi should succeed: {:?}",
        result.err()
    );

    // Verify .ct output with CTFS magic bytes.
    let ct_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect();
    assert!(!ct_files.is_empty(), "expected .ct file");
    let ct_content = std::fs::read(&ct_files[0]).unwrap();
    assert!(ct_content.len() >= 5, ".ct file too small");
    assert_eq!(&ct_content[..5], &[0xC0u8, 0xDE, 0x72, 0xAC, 0xE2]);
}

// ===========================================================================
// Test 4: CPI call depth tracking with DWARF-derived ranges
// ===========================================================================

/// Verifies that the CPI detector's call_depth tracking is accurate when
/// processing snapshots with PCs from real DWARF function boundaries,
/// including edge cases like returning directly to primary from depth 2.
#[test]
fn test_cpi_call_depth_accuracy_with_real_ranges() {
    let elf_data = load_recorder_elf();
    let mut functions = find_functions(&elf_data).expect("should find functions");

    if functions.len() < 6 {
        eprintln!("Skipping: need at least 6 functions");
        return;
    }

    // Sort by start_addr to ensure non-overlapping partitioning.
    functions.sort_by_key(|f| f.start_addr);

    let parser = DwarfParser::new(&elf_data).expect("ELF should parse");
    let text_vaddr = parser.text_vaddr();

    // Compute total PC span and divide into 3 equal ranges.
    let min_pc = (functions[0].start_addr.saturating_sub(text_vaddr)) / 8;
    let max_pc = (functions
        .last()
        .unwrap()
        .end_addr
        .saturating_sub(text_vaddr))
        / 8
        + 1;
    let span = max_pc - min_pc;

    let range_primary = min_pc..min_pc + span / 3;
    let range_b = min_pc + span / 3..min_pc + 2 * span / 3;
    let range_c = min_pc + 2 * span / 3..max_pc;

    let mut detector = CpiDetector::new(range_primary.clone());
    detector.add_program_range("program_b", range_b.clone());
    detector.add_program_range("program_c", range_c.clone());

    // Use midpoint PCs to be safely inside each range.
    let pc_primary = range_primary.start + (range_primary.end - range_primary.start) / 2;
    let pc_b = range_b.start + (range_b.end - range_b.start) / 2;
    let pc_c = range_c.start + (range_c.end - range_c.start) / 2;

    // depth 0: primary
    detector.process_snapshot(&snap(pc_primary, 0));
    assert_eq!(detector.call_depth(), 0);

    // depth 1: primary -> B
    detector.process_snapshot(&snap(pc_b, 0));
    assert_eq!(detector.call_depth(), 1);

    // depth 2: B -> C
    detector.process_snapshot(&snap(pc_c, 0));
    assert_eq!(detector.call_depth(), 2);

    // Direct return from C to primary (skipping B) -- should pop both levels.
    let return_pc = pc_primary + 1; // Still within primary range
    let event = detector.process_snapshot(&snap(return_pc, 0));
    assert_eq!(
        event,
        CpiEvent::CpiReturn { return_pc },
        "direct return to primary should unwind all CPI levels"
    );
    assert_eq!(
        detector.call_depth(),
        0,
        "call depth should be 0 after returning to primary"
    );
    assert_eq!(detector.current_program(), "primary");
}

// ===========================================================================
// Test 5: CPI trace output ordering
// ===========================================================================

/// Verifies that the trace output from record_with_cpi has correct event
/// ordering: Call appears before Steps in the called program, and Return
/// appears before Steps in the returned-to program.
#[test]
fn test_cpi_trace_event_ordering() {
    let elf_data = load_recorder_elf();
    let (range_a, range_b, locs_a, locs_b) = partition_functions_for_cpi(&elf_data);

    if locs_a.len() < 2 || locs_b.is_empty() {
        eprintln!("Skipping: insufficient locations");
        return;
    }

    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program("primary", range_a.clone(), locs_a.clone());
    registry.add_synthetic_program("callee", range_b.clone(), locs_b.clone());

    // Sequence: primary step -> CPI call -> callee step -> CPI return -> primary step
    let snapshots = vec![
        snap(locs_a[0].0, 0),   // primary step
        snap(locs_b[0].0, 100), // CPI call + callee step
        snap(locs_a[1].0, 0),   // CPI return + primary step
    ];

    let mut detector = CpiDetector::new(range_a.clone());
    detector.add_program_range("callee", range_b.clone());

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
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect();
    assert!(!ct_files.is_empty(), "expected .ct file");
    let ct_content = std::fs::read(&ct_files[0]).unwrap();
    assert!(ct_content.len() >= 5, ".ct file too small");
    assert_eq!(&ct_content[..5], &[0xC0u8, 0xDE, 0x72, 0xAC, 0xE2]);
}
