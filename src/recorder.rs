//! Core recording logic for Solana/SBF program execution.
//!
//! This module consumes register trace data from the SBF VM,
//! correlates it with DWARF source mapping, and produces CodeTracer
//! trace output.

use std::path::Path;

use codetracer_trace_types::{Line, TypeKind, ValueRecord, NONE_VALUE};
use codetracer_trace_writer::trace_writer::TraceWriter;
use codetracer_trace_writer::{TraceEventsFileFormat, create_trace_writer};
use eyre::{Context, Result, eyre};

use crate::dwarf::DwarfParser;
use crate::register_trace::{RegisterSnapshot, parse_regs_file};

/// Record a Solana program execution from pre-generated register trace
/// and DWARF-annotated ELF, producing CodeTracer trace output.
///
/// # Arguments
///
/// * `regs_data` - Raw `.regs` file content (binary register snapshots)
/// * `elf_data`  - Raw ELF file content (unstripped, with DWARF)
/// * `source_path` - Path to display in the trace for source locations
/// * `out_dir`   - Directory where trace files will be written
/// * `format`    - Output format (Binary or Json)
pub fn record_from_traces(
    regs_data: &[u8],
    elf_data: &[u8],
    source_path: &Path,
    out_dir: &Path,
    format: TraceEventsFileFormat,
) -> Result<()> {
    // 1. Parse register snapshots.
    let snapshots = parse_regs_file(regs_data)?;

    // 2. Parse DWARF from ELF.
    let dwarf = DwarfParser::new(elf_data)?;

    // 3. Build source locations from DWARF.
    let source_locations: Vec<(u64, String, u32)> = snapshots
        .iter()
        .filter_map(|snap| {
            let loc = dwarf.find_location(snap.pc())?;
            Some((snap.pc(), loc.file, loc.line))
        })
        .collect();

    // Convert to borrowed form for the shared implementation.
    let source_locs_ref: Vec<(u64, &str, u32)> = source_locations
        .iter()
        .map(|(pc, f, l)| (*pc, f.as_str(), *l))
        .collect();

    record_from_snapshots(&snapshots, &source_locs_ref, source_path, out_dir, format)
}

/// Record a Solana program execution from pre-parsed register snapshots
/// and source locations, producing CodeTracer trace output.
///
/// This is the lower-level entry point useful for testing with synthetic data
/// (bypassing ELF/DWARF parsing).
///
/// # Arguments
///
/// * `snapshots`        - Parsed register snapshots
/// * `source_locations` - Tuples of (pc, file, line) mapping PCs to source
/// * `source_path`      - Path to display in the trace
/// * `out_dir`          - Directory where trace files will be written
/// * `format`           - Output format
pub fn record_from_snapshots(
    snapshots: &[RegisterSnapshot],
    source_locations: &[(u64, &str, u32)],
    source_path: &Path,
    out_dir: &Path,
    format: TraceEventsFileFormat,
) -> Result<()> {
    // Build a PC -> (file, line) lookup.
    let pc_to_loc: std::collections::HashMap<u64, (&str, u32)> = source_locations
        .iter()
        .map(|(pc, file, line)| (*pc, (*file, *line)))
        .collect();

    // Create output directory.
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // Create the trace writer.
    let program_name = source_path.to_string_lossy();
    let mut writer = create_trace_writer(&program_name, &[], format);

    // Set up output files.
    let events_path = out_dir.join("trace.bin");
    let metadata_path = out_dir.join("trace_metadata.json");
    let paths_path = out_dir.join("trace_paths.json");

    TraceWriter::begin_writing_trace_events(&mut *writer, &events_path)
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_metadata(&mut *writer, &metadata_path)
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_paths(&mut *writer, &paths_path)
        .map_err(|e| eyre!("{e}"))?;

    // Start the trace.
    TraceWriter::start(&mut *writer, source_path, Line(1));

    // Register the u64 type (after start so "None" gets TypeId(0)).
    let u64_type_id = TraceWriter::ensure_type_id(&mut *writer, TypeKind::Int, "u64");

    // Register a function for the main program.
    let main_fn_id = TraceWriter::ensure_function_id(
        &mut *writer,
        "main",
        source_path,
        Line(1),
    );

    // Emit initial call.
    TraceWriter::register_call(&mut *writer, main_fn_id, vec![]);

    // Walk snapshots.
    let mut prev_line: Option<u32> = None;
    let mut prev_pc: Option<u64> = None;

    for snap in snapshots {
        let pc = snap.pc();

        // Look up source location for this PC.
        let (file_str, line) = match pc_to_loc.get(&pc) {
            Some(&(f, l)) => (f, l),
            None => continue, // No source mapping; skip.
        };

        // Detect function call/return from large PC jumps.
        if let Some(prev) = prev_pc {
            let diff = if pc > prev { pc - prev } else { prev - pc };
            // A forward jump of more than 2 instructions suggests a call;
            // a backward jump suggests a return. This is a heuristic.
            if diff > 2 && pc > prev {
                let callee_fn_id = TraceWriter::ensure_function_id(
                    &mut *writer,
                    &format!("fn_at_pc_{pc}"),
                    &Path::new(file_str),
                    Line(line as i64),
                );
                TraceWriter::register_call(&mut *writer, callee_fn_id, vec![]);
            } else if diff > 2 && pc < prev {
                TraceWriter::register_return(&mut *writer, NONE_VALUE);
            }
        }

        // Emit step when line changes.
        if prev_line != Some(line) {
            TraceWriter::register_step(
                &mut *writer,
                &Path::new(file_str),
                Line(line as i64),
            );
            prev_line = Some(line);
        }

        // Emit register values as variables (r0 through r10).
        for r in 0..=10 {
            let name = format!("r{r}");
            let value = ValueRecord::Int {
                i: snap.reg(r) as i64,
                type_id: u64_type_id,
            };
            TraceWriter::register_variable_with_full_value(&mut *writer, &name, value);
        }

        prev_pc = Some(pc);
    }

    // Emit return for the main function.
    TraceWriter::register_return(&mut *writer, NONE_VALUE);

    // Finish writing.
    TraceWriter::finish_writing_trace_events(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_metadata(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_paths(&mut *writer).map_err(|e| eyre!("{e}"))?;

    Ok(())
}
