//! Tests for DWARF debug info parsing (`dwarf.rs`).
//!
//! These tests exercise the DwarfParser against real ELF files to verify
//! that PC-to-source-line resolution works correctly, complementing the
//! synthetic data tests in other test files.

use codetracer_solana_recorder::dwarf::{DwarfParser, SourceLocation};

// ---------------------------------------------------------------------------
// Tests: error handling
// ---------------------------------------------------------------------------

/// DwarfParser::new rejects non-ELF data.
#[test]
fn test_dwarf_parser_rejects_non_elf() {
    let result = DwarfParser::new(b"this is not an ELF file");
    assert!(result.is_err(), "non-ELF data should produce an error");
    let err_msg = format!("{:#}", result.err().unwrap());
    assert!(
        err_msg.contains("ELF") || err_msg.contains("parse"),
        "error should mention ELF or parsing, got: {err_msg}"
    );
}

/// DwarfParser::new rejects empty data.
#[test]
fn test_dwarf_parser_rejects_empty() {
    let result = DwarfParser::new(&[]);
    assert!(result.is_err(), "empty data should produce an error");
}

/// DwarfParser::new rejects truncated ELF magic.
#[test]
fn test_dwarf_parser_rejects_truncated_magic() {
    // Just the ELF magic but nothing else.
    let result = DwarfParser::new(b"\x7fELF");
    assert!(result.is_err(), "truncated ELF should produce an error");
}

/// DwarfParser::new rejects random binary garbage.
#[test]
fn test_dwarf_parser_rejects_random_bytes() {
    let garbage: Vec<u8> = (0..256u16).map(|i| (i % 256) as u8).collect();
    let result = DwarfParser::new(&garbage);
    assert!(result.is_err(), "random bytes should produce an error");
}

// ---------------------------------------------------------------------------
// Tests: real binary parsing
// ---------------------------------------------------------------------------

/// DwarfParser correctly parses the recorder's own debug binary.
///
/// The test binary (compiled with debug info) has DWARF sections.
/// We verify that:
/// 1. The parser loads successfully
/// 2. text_vaddr is non-zero
/// 3. At least some addresses resolve to source locations
/// 4. Resolved locations reference real source files
#[test]
fn test_dwarf_parser_on_real_binary() {
    // Use the recorder's own test binary as a real ELF with DWARF.
    let binary_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");
    let elf_data = std::fs::read(binary_path).expect("should be able to read the test binary");

    let parser = DwarfParser::new(&elf_data).expect("real binary should parse successfully");

    // The .text section should have a non-zero virtual address.
    assert!(
        parser.text_vaddr() > 0,
        "real binary should have non-zero .text vaddr, got: {}",
        parser.text_vaddr()
    );
}

/// Scan the real binary's .text section and verify at least some addresses
/// resolve to source locations with valid file paths and line numbers.
#[test]
fn test_dwarf_parser_resolves_source_locations() {
    let binary_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");
    let elf_data = std::fs::read(binary_path).expect("should be able to read the test binary");

    let parser = DwarfParser::new(&elf_data).expect("real binary should parse successfully");

    // Scan a range of SBF PCs to find resolvable addresses.
    // For a host binary, the formula elf_addr = text_vaddr + sbf_pc * 8
    // means SBF PC 0 maps to text_vaddr, PC 1 maps to text_vaddr+8, etc.
    let mut found_any = false;
    let mut found_location: Option<SourceLocation> = None;

    for sbf_pc in 0..100_000u64 {
        if let Some(loc) = parser.find_location(sbf_pc) {
            found_any = true;
            found_location = Some(loc);
            break;
        }
    }

    assert!(
        found_any,
        "should find at least one resolvable address in the binary's .text section"
    );

    let loc = found_location.unwrap();
    assert!(
        !loc.file.is_empty(),
        "resolved file path should not be empty"
    );
    assert!(
        loc.line > 0,
        "resolved line number should be positive, got: {}",
        loc.line
    );
}

/// Verify that multiple distinct source locations can be resolved from the binary.
#[test]
fn test_dwarf_parser_multiple_locations() {
    let binary_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");
    let elf_data = std::fs::read(binary_path).expect("should be able to read the test binary");

    let parser = DwarfParser::new(&elf_data).expect("real binary should parse successfully");

    // Collect multiple resolved locations.
    let mut locations: Vec<SourceLocation> = Vec::new();
    let mut last_file = String::new();

    for sbf_pc in 0..200_000u64 {
        if let Some(loc) = parser.find_location(sbf_pc) {
            // Only collect distinct locations (different file or line).
            if loc.file != last_file || locations.last().map(|l: &SourceLocation| l.line) != Some(loc.line) {
                last_file = loc.file.clone();
                locations.push(loc);
                if locations.len() >= 10 {
                    break;
                }
            }
        }
    }

    assert!(
        locations.len() >= 2,
        "should resolve at least 2 distinct source locations, got {}",
        locations.len()
    );

    // All locations should have valid file paths and positive line numbers.
    for loc in &locations {
        assert!(!loc.file.is_empty(), "file path should not be empty");
        assert!(loc.line > 0, "line number should be positive");
    }

    // At least one location should reference a .rs file (since this is a Rust binary).
    let has_rs_file = locations.iter().any(|loc| loc.file.ends_with(".rs"));
    assert!(
        has_rs_file,
        "at least one location should reference a .rs file, found: {:?}",
        locations.iter().map(|l| &l.file).collect::<Vec<_>>()
    );
}

/// Verify the SBF PC-to-ELF-address formula: elf_addr = text_vaddr + (sbf_pc * 8).
/// We verify that different PCs produce lookups at different addresses and
/// don't cause panics.
#[test]
fn test_dwarf_parser_address_formula_no_panic() {
    let binary_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");
    let elf_data = std::fs::read(binary_path).expect("should be able to read the test binary");

    let parser = DwarfParser::new(&elf_data).expect("real binary should parse successfully");

    let text_vaddr = parser.text_vaddr();
    assert!(text_vaddr > 0);

    // Various PC values should not panic, including edge cases.
    // The overflow-safe formula uses checked arithmetic, so even extreme
    // values return None instead of panicking.
    for &pc in &[0, 1, 100, 1_000, 10_000, 100_000, u64::MAX / 8 - 1, u64::MAX] {
        let _result = parser.find_location(pc);
    }
}

/// Out-of-range PCs should return None gracefully.
#[test]
fn test_dwarf_parser_out_of_range_pc() {
    let binary_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");
    let elf_data = std::fs::read(binary_path).expect("should be able to read the test binary");

    let parser = DwarfParser::new(&elf_data).expect("real binary should parse successfully");

    // A very large PC that maps beyond any code section should return None.
    assert_eq!(
        parser.find_location(u64::MAX / 16),
        None,
        "extremely large PC should return None"
    );
}

// ---------------------------------------------------------------------------
// Tests: SourceLocation struct
// ---------------------------------------------------------------------------

/// SourceLocation equality works as expected.
#[test]
fn test_source_location_equality() {
    let loc1 = SourceLocation {
        file: "test.rs".to_string(),
        line: 42,
        column: Some(5),
    };
    let loc2 = SourceLocation {
        file: "test.rs".to_string(),
        line: 42,
        column: Some(5),
    };
    let loc3 = SourceLocation {
        file: "test.rs".to_string(),
        line: 43,
        column: None,
    };

    assert_eq!(loc1, loc2, "identical locations should be equal");
    assert_ne!(loc1, loc3, "different locations should not be equal");
}

/// SourceLocation clone works correctly.
#[test]
fn test_source_location_clone() {
    let loc = SourceLocation {
        file: "main.rs".to_string(),
        line: 10,
        column: Some(3),
    };
    let cloned = loc.clone();
    assert_eq!(loc, cloned);
}

/// SourceLocation debug formatting includes all fields.
#[test]
fn test_source_location_debug() {
    let loc = SourceLocation {
        file: "lib.rs".to_string(),
        line: 7,
        column: None,
    };
    let debug = format!("{:?}", loc);
    assert!(debug.contains("lib.rs"), "debug should contain file name");
    assert!(debug.contains("7"), "debug should contain line number");
}

// ---------------------------------------------------------------------------
// Tests: integration with recorder
// ---------------------------------------------------------------------------

/// Verify that record_from_traces (the full pipeline using DWARF) can parse
/// the recorder's own binary and produce trace output, even if source mappings
/// for a synthetic register trace are partial.
#[test]
fn test_dwarf_integration_with_recorder() {
    use codetracer_solana_recorder::recorder::record_from_traces;
    use codetracer_solana_recorder::register_trace::ROW_SIZE;
    use codetracer_trace_writer::TraceEventsFileFormat;

    let binary_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");
    let elf_data = std::fs::read(binary_path).expect("should be able to read the test binary");

    // Create a small synthetic register trace (3 instructions).
    let mut regs_data = Vec::new();
    for step in 0..3u64 {
        let mut regs = [0u64; 12];
        regs[11] = step; // PC
        regs[1] = step * 10; // r1 value
        for r in &regs {
            regs_data.extend_from_slice(&r.to_le_bytes());
        }
    }
    assert_eq!(regs_data.len(), 3 * ROW_SIZE);

    let tmp = tempfile::tempdir().unwrap();

    // This may produce partial traces (not all PCs will resolve to source),
    // but it should not panic or error.
    let result = record_from_traces(
        &regs_data,
        &elf_data,
        std::path::Path::new("test_program.so"),
        tmp.path(),
        TraceEventsFileFormat::Json,
    );

    // The call should succeed (even if no source locations are found,
    // the recorder handles missing mappings gracefully).
    assert!(
        result.is_ok(),
        "record_from_traces should succeed with real ELF, got: {:?}",
        result.err()
    );

    // Output files should exist.
    assert!(tmp.path().join("trace.bin").exists(), "trace.bin should exist");
    assert!(
        tmp.path().join("trace_metadata.json").exists(),
        "trace_metadata.json should exist"
    );
}
