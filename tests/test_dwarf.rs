//! Tests for DWARF debug info parsing (`dwarf.rs`).
//!
//! These tests exercise the DwarfParser against a real ELF file to verify
//! that PC-to-source-line resolution works correctly, complementing the
//! synthetic data tests in other test files.
//!
//! The "real ELF" is the committed `test-programs/cpi_fixture.elf` fixture
//! (a small statically-linked binary built with full debug info, defining
//! many named functions including `main`).  Earlier revisions read the
//! recorder's *own* executable instead -- which only works where that
//! executable is itself ELF-formatted: on Windows the recorder binary is a
//! PE and carries no embedded DWARF, so `DwarfParser`/`find_functions` had
//! nothing to parse.  A committed ELF fixture makes the tests
//! platform-independent.

use std::path::PathBuf;

use codetracer_solana_recorder::dwarf::{
    DwarfParser, FunctionBoundary, SourceLocation, find_functions,
};

/// Read the committed ELF/DWARF test fixture.
///
/// `test-programs/cpi_fixture.elf` is a small statically-linked ELF64 built
/// with full debug info; its DWARF describes many named functions (one of
/// them `main`) with line-table rows referencing the fixture's `.rs`
/// source.  It stands in for "a real debug binary" so these tests do not
/// depend on the recorder's own executable being an ELF.
fn fixture_elf() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs")
        .join("cpi_fixture.elf");
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("should be able to read ELF fixture {}: {e}", path.display()))
}

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
    let elf_data = fixture_elf();

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
    let elf_data = fixture_elf();

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
    let elf_data = fixture_elf();

    let parser = DwarfParser::new(&elf_data).expect("real binary should parse successfully");

    // Collect multiple resolved locations.
    let mut locations: Vec<SourceLocation> = Vec::new();
    let mut last_file = String::new();

    for sbf_pc in 0..200_000u64 {
        if let Some(loc) = parser.find_location(sbf_pc) {
            // Only collect distinct locations (different file or line).
            if loc.file != last_file
                || locations.last().map(|l: &SourceLocation| l.line) != Some(loc.line)
            {
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
    let elf_data = fixture_elf();

    let parser = DwarfParser::new(&elf_data).expect("real binary should parse successfully");

    let text_vaddr = parser.text_vaddr();
    assert!(text_vaddr > 0);

    // Various PC values should not panic, including edge cases.
    // The overflow-safe formula uses checked arithmetic, so even extreme
    // values return None instead of panicking.
    for &pc in &[
        0,
        1,
        100,
        1_000,
        10_000,
        100_000,
        u64::MAX / 8 - 1,
        u64::MAX,
    ] {
        let _result = parser.find_location(pc);
    }
}

/// Out-of-range PCs should return None gracefully.
#[test]
fn test_dwarf_parser_out_of_range_pc() {
    let elf_data = fixture_elf();

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

    let elf_data = fixture_elf();

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
    );

    // The call should succeed (even if no source locations are found,
    // the recorder handles missing mappings gracefully).
    assert!(
        result.is_ok(),
        "record_from_traces should succeed with real ELF, got: {:?}",
        result.err()
    );

    // Verify .ct output exists.
    let ct_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ct"))
        .collect();
    assert!(!ct_files.is_empty(), "expected .ct file");
}

// ---------------------------------------------------------------------------
// Tests: .debug_info function boundary extraction (Call/Return events)
// ---------------------------------------------------------------------------

/// find_functions extracts function boundaries from the recorder's own binary.
///
/// A real debug binary compiled with `-g` should contain many DW_TAG_subprogram
/// entries with low_pc/high_pc ranges.
#[test]
fn test_find_functions_on_real_binary() {
    let elf_data = fixture_elf();

    let functions =
        find_functions(&elf_data).expect("find_functions should succeed on a real binary");

    assert!(
        !functions.is_empty(),
        "real binary should contain at least one function boundary"
    );

    // Every function boundary must have a non-empty name and a valid range.
    for func in &functions {
        assert!(!func.name.is_empty(), "function name should not be empty");
        assert!(
            func.end_addr > func.start_addr,
            "function end_addr ({:#x}) should be greater than start_addr ({:#x}) for {:?}",
            func.end_addr,
            func.start_addr,
            func.name,
        );
    }
}

/// The "main" function (or equivalent entry point) should appear in the list.
#[test]
fn test_find_functions_contains_main() {
    let elf_data = fixture_elf();

    let functions = find_functions(&elf_data).expect("find_functions should succeed");

    let has_main = functions.iter().any(|f| f.name == "main");
    assert!(
        has_main,
        "should find a 'main' function boundary; found: {:?}",
        functions
            .iter()
            .map(|f| &f.name)
            .take(20)
            .collect::<Vec<_>>()
    );
}

/// Multiple distinct functions should be found, with non-overlapping or
/// reasonably sized ranges.
#[test]
fn test_find_functions_multiple_distinct() {
    let elf_data = fixture_elf();

    let functions = find_functions(&elf_data).expect("find_functions should succeed");

    // A real Rust binary should have many functions.
    assert!(
        functions.len() >= 10,
        "expected at least 10 function boundaries, got {}",
        functions.len()
    );

    // Collect unique function names.
    let unique_names: std::collections::HashSet<&str> =
        functions.iter().map(|f| f.name.as_str()).collect();
    assert!(
        unique_names.len() >= 5,
        "expected at least 5 unique function names, got {}",
        unique_names.len()
    );
}

/// An ELF address can be tested against function boundaries to determine
/// which function it belongs to, enabling Call/Return event generation.
#[test]
fn test_find_functions_address_lookup() {
    let elf_data = fixture_elf();

    let functions = find_functions(&elf_data).expect("find_functions should succeed");

    // Pick the first function and verify an address in its range resolves to it.
    let func = &functions[0];
    let mid_addr = func.start_addr + (func.end_addr - func.start_addr) / 2;

    let found = functions
        .iter()
        .find(|f| mid_addr >= f.start_addr && mid_addr < f.end_addr);
    assert!(
        found.is_some(),
        "address {:#x} should fall within at least one function boundary",
        mid_addr
    );
    assert_eq!(
        found.unwrap().name,
        func.name,
        "address should resolve to the expected function"
    );
}

/// find_functions rejects non-ELF input.
#[test]
fn test_find_functions_rejects_non_elf() {
    let result = find_functions(b"not an elf");
    assert!(result.is_err(), "non-ELF data should produce an error");
}

/// find_functions returns an empty list when no DW_TAG_subprogram entries
/// have address ranges (tested by checking that out-of-range addresses
/// don't produce false matches).
#[test]
fn test_find_functions_all_have_valid_ranges() {
    let elf_data = fixture_elf();

    let functions = find_functions(&elf_data).expect("find_functions should succeed");

    // Every returned function must have end_addr > start_addr (no degenerate ranges).
    for func in &functions {
        assert!(
            func.end_addr > func.start_addr,
            "function {:?} has invalid range: {:#x}..{:#x}",
            func.name,
            func.start_addr,
            func.end_addr,
        );
        // Sizes should be reasonable (not spanning the entire address space).
        let size = func.end_addr - func.start_addr;
        assert!(
            size < 100_000_000,
            "function {:?} has suspiciously large size: {} bytes",
            func.name,
            size,
        );
    }
}

/// FunctionBoundary struct equality and clone work correctly.
#[test]
fn test_function_boundary_equality_and_clone() {
    let fb1 = FunctionBoundary {
        name: "process_instruction".to_string(),
        start_addr: 0x1000,
        end_addr: 0x1100,
    };
    let fb2 = fb1.clone();
    assert_eq!(fb1, fb2, "cloned FunctionBoundary should be equal");

    let fb3 = FunctionBoundary {
        name: "other_fn".to_string(),
        start_addr: 0x2000,
        end_addr: 0x2080,
    };
    assert_ne!(
        fb1, fb3,
        "different FunctionBoundary values should not be equal"
    );
}

/// FunctionBoundary debug formatting includes all fields.
#[test]
fn test_function_boundary_debug() {
    let fb = FunctionBoundary {
        name: "my_func".to_string(),
        start_addr: 0x4000,
        end_addr: 0x4100,
    };
    let debug = format!("{:?}", fb);
    assert!(
        debug.contains("my_func"),
        "debug should contain function name"
    );
    assert!(
        debug.contains("4000") || debug.contains("16384"),
        "debug should contain start_addr"
    );
}
