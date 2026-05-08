//! Core recording logic for Solana/SBF program execution.
//!
//! This module consumes register trace data from the SBF VM,
//! correlates it with DWARF source mapping, and produces CodeTracer
//! trace output.
//!
//! The output format is fixed to CTFS — see
//! `Recorder-CLI-Conventions.md` §4 in `codetracer-specs`.  Use
//! `ct print` (from `codetracer-trace-format-nim`) for human-readable
//! conversion of the produced bundle.

use std::path::Path;

use codetracer_trace_types::{Line, NONE_VALUE, TypeKind, ValueRecord};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{TraceEventsFileFormat, create_trace_writer};
use eyre::{Context, Result, eyre};

use crate::cpi::{CpiDetector, CpiEvent};
use crate::dwarf::DwarfParser;
use crate::multi_program::ProgramRegistry;
use crate::register_trace::{RegisterSnapshot, parse_regs_file};

// The recorder is CTFS-only per `Recorder-CLI-Conventions.md` §4 (see
// `codetracer-specs`).  We pin every `create_trace_writer` call site to
// this constant so the recorder surface no longer carries a `format`
// parameter and the writer cannot accidentally drift away from the
// canonical multi-stream container.
const CTFS_FORMAT: TraceEventsFileFormat = TraceEventsFileFormat::Ctfs;

/// Record a Solana program execution from pre-generated register trace
/// and DWARF-annotated ELF, producing CodeTracer trace output.
///
/// # Arguments
///
/// * `regs_data` - Raw `.regs` file content (binary register snapshots)
/// * `elf_data`  - Raw ELF file content (unstripped, with DWARF)
/// * `source_path` - Path to display in the trace for source locations
/// * `out_dir`   - Directory where trace files will be written
///
/// The output format is fixed to the canonical CodeTracer CTFS multi-stream
/// container.
pub fn record_from_traces(
    regs_data: &[u8],
    elf_data: &[u8],
    source_path: &Path,
    out_dir: &Path,
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

    record_from_snapshots(&snapshots, &source_locs_ref, source_path, out_dir)
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
///
/// The output format is fixed to the canonical CodeTracer CTFS multi-stream
/// container.
pub fn record_from_snapshots(
    snapshots: &[RegisterSnapshot],
    source_locations: &[(u64, &str, u32)],
    source_path: &Path,
    out_dir: &Path,
) -> Result<()> {
    // Create output directory.
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // Create the trace writer (CTFS only).
    let program_name = source_path.to_string_lossy();
    let mut writer = create_trace_writer(&program_name, &[], CTFS_FORMAT);

    // Set up output files.  CTFS-only writer — events stream lives in
    // `trace.bin`.
    let events_path = out_dir.join("trace.bin");
    let metadata_path = out_dir.join("trace_metadata.json");
    let paths_path = out_dir.join("trace_paths.json");

    TraceWriter::begin_writing_trace_events(&mut *writer, &events_path)
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_metadata(&mut *writer, &metadata_path)
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_paths(&mut *writer, &paths_path)
        .map_err(|e| eyre!("{e}"))?;

    record_from_snapshots_into_writer(snapshots, source_locations, source_path, &mut *writer)?;

    // Finish writing.
    TraceWriter::finish_writing_trace_events(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_metadata(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_paths(&mut *writer).map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;

    Ok(())
}

/// Core recording logic that writes into any TraceWriter.
/// Useful for tests with NonStreamingTraceWriter.
pub fn record_from_snapshots_into_writer(
    snapshots: &[RegisterSnapshot],
    source_locations: &[(u64, &str, u32)],
    source_path: &Path,
    writer: &mut dyn TraceWriter,
) -> Result<()> {
    // Build a PC -> (file, line) lookup.
    let pc_to_loc: std::collections::HashMap<u64, (&str, u32)> = source_locations
        .iter()
        .map(|(pc, file, line)| (*pc, (*file, *line)))
        .collect();

    // Start the trace.
    TraceWriter::start(writer, source_path, Line(1));

    // Register the u64 type (after start so "None" gets TypeId(0)).
    let u64_type_id = TraceWriter::ensure_type_id(writer, TypeKind::Int, "u64");

    // Register a function for the main program.
    let main_fn_id = TraceWriter::ensure_function_id(
        writer,
        "main",
        source_path,
        Line(1),
    );

    // Emit initial call.
    TraceWriter::register_call(writer, main_fn_id, vec![]);

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
            if diff > 2 && pc > prev {
                let callee_fn_id = TraceWriter::ensure_function_id(
                    writer,
                    &format!("fn_at_pc_{pc}"),
                    &Path::new(file_str),
                    Line(line as i64),
                );
                TraceWriter::register_call(writer, callee_fn_id, vec![]);
            } else if diff > 2 && pc < prev {
                TraceWriter::register_return(writer, NONE_VALUE);
            }
        }

        // Emit step when line changes.
        if prev_line != Some(line) {
            TraceWriter::register_step(
                writer,
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
            TraceWriter::register_variable_with_full_value(writer, &name, value);
        }

        prev_pc = Some(pc);
    }

    // Emit return for the main function.
    TraceWriter::register_return(writer, NONE_VALUE);

    Ok(())
}

/// Record a Solana program execution with CPI (Cross-Program Invocation)
/// awareness, producing CodeTracer trace output with nested Call/Return
/// events at CPI boundaries.
///
/// # Arguments
///
/// * `snapshots`         - Parsed register snapshots (may span multiple programs)
/// * `registry`          - Program registry mapping PCs to programs and source info
/// * `cpi_detector`      - CPI detector initialised with the primary program's range
/// * `source_path`       - Path to display in the trace for the primary program
/// * `out_dir`           - Directory where trace files will be written
///
/// The output format is fixed to the canonical CodeTracer CTFS multi-stream
/// container.
pub fn record_with_cpi(
    snapshots: &[RegisterSnapshot],
    registry: &ProgramRegistry,
    cpi_detector: &mut CpiDetector,
    source_path: &Path,
    out_dir: &Path,
) -> Result<()> {
    // Create output directory.
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // Create the trace writer (CTFS only).
    let program_name = source_path.to_string_lossy();
    let mut writer = create_trace_writer(&program_name, &[], CTFS_FORMAT);

    // Set up output files.  CTFS-only writer — events stream lives in
    // `trace.bin`.
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

    // Register the u64 type.
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

    // Walk snapshots with CPI detection.
    let mut prev_line: Option<u32> = None;
    let mut prev_pc: Option<u64> = None;

    for snap in snapshots {
        let pc = snap.pc();

        // Check for CPI boundaries.
        let cpi_event = cpi_detector.process_snapshot(snap);
        match cpi_event {
            CpiEvent::CpiCall { target_pc } => {
                let program_name_str = registry
                    .program_name(target_pc)
                    .unwrap_or("unknown_program");
                let (file_str, line) = registry
                    .find_location(target_pc)
                    .unwrap_or_else(|| (format!("{program_name_str}.sbf"), 0));
                let cpi_fn_id = TraceWriter::ensure_function_id(
                    &mut *writer,
                    program_name_str,
                    &Path::new(&file_str),
                    Line(line as i64),
                );
                // Stage CPI-target metadata as call args so the calltrace
                // pane displays which program was invoked and at what PC.
                // Mirrors the `register_call_arg` / `arg` pattern from the
                // Ruby (1.22) and JS (1.38) recorders — see section 5.6 of
                // /tmp/isonim-migration.txt.
                let pc_type_id =
                    TraceWriter::ensure_type_id(&mut *writer, TypeKind::Int, "u64");
                let str_type_id =
                    TraceWriter::ensure_type_id(&mut *writer, TypeKind::String, "string");
                let _ = TraceWriter::arg(
                    &mut *writer,
                    "target_program",
                    ValueRecord::String {
                        text: program_name_str.to_string(),
                        type_id: str_type_id,
                    },
                );
                let _ = TraceWriter::arg(
                    &mut *writer,
                    "target_pc",
                    ValueRecord::Int {
                        i: target_pc as i64,
                        type_id: pc_type_id,
                    },
                );
                TraceWriter::register_call(&mut *writer, cpi_fn_id, vec![]);
                // Reset line tracking for the new program context.
                prev_line = None;
            }
            CpiEvent::CpiReturn { return_pc: _ } => {
                TraceWriter::register_return(&mut *writer, NONE_VALUE);
                // Reset line tracking for the returned-to context.
                prev_line = None;
            }
            CpiEvent::SameProgram => {
                // Within the same program, use the existing call/return heuristic.
                if let Some(prev) = prev_pc {
                    let diff = if pc > prev { pc - prev } else { prev - pc };
                    if diff > 2 {
                        let current_program = cpi_detector.current_program();
                        let (file_str, line) = registry
                            .find_location(pc)
                            .unwrap_or_else(|| (format!("{current_program}.sbf"), 0));
                        if pc > prev {
                            let callee_fn_id = TraceWriter::ensure_function_id(
                                &mut *writer,
                                &format!("fn_at_pc_{pc}"),
                                &Path::new(&file_str),
                                Line(line as i64),
                            );
                            TraceWriter::register_call(&mut *writer, callee_fn_id, vec![]);
                        } else {
                            TraceWriter::register_return(&mut *writer, NONE_VALUE);
                        }
                    }
                }
            }
        }

        // Look up source location for this PC.
        let (file_str, line) = match registry.find_location(pc) {
            Some((f, l)) => (f, l),
            None => {
                // No source mapping; emit opcode-based line number.
                let program_name_str = cpi_detector.current_program();
                (format!("{program_name_str}.sbf"), pc as u32)
            }
        };

        // Emit step when line changes.
        if prev_line != Some(line) {
            TraceWriter::register_step(
                &mut *writer,
                &Path::new(&file_str),
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

    // Emit returns for any remaining CPI contexts (handles traces that
    // end while still inside nested CPIs).
    for _ in 0..cpi_detector.call_depth() {
        TraceWriter::register_return(&mut *writer, NONE_VALUE);
    }

    // Emit return for the main function.
    TraceWriter::register_return(&mut *writer, NONE_VALUE);

    // Finish writing.
    TraceWriter::finish_writing_trace_events(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_metadata(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_paths(&mut *writer).map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;

    Ok(())
}
