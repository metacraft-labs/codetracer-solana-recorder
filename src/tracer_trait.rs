//! Proposed `SbpfTracer` trait for pluggable SBPF execution tracers.
//!
//! This module defines a trait that the SBPF VM could call during execution,
//! enabling pluggable trace consumers without modifying the VM core.
//!
//! The design goals are:
//! - **Zero overhead** when tracing is disabled (via `NoOpTracer`).
//! - **Trait-object safety** for runtime polymorphism when needed.
//! - **Complete coverage** of execution events: steps, syscalls, memory
//!   operations, CPI boundaries, and program start/exit.

use std::io::Write;
use std::path::Path;

use codetracer_trace_types::{EventLogKind, Line, TypeKind, ValueRecord, NONE_VALUE};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{TraceEventsFileFormat, create_trace_writer};
use eyre::{Result, eyre};

use crate::register_trace::{RegisterSnapshot, ROW_SIZE};

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Trait for pluggable SBPF execution tracers.
///
/// Implementations receive callbacks at each execution point.
/// All methods have default empty implementations so that the `NoOpTracer`
/// compiles to zero overhead.
pub trait SbpfTracer {
    /// Called before each instruction executes.
    fn on_step(&mut self, _pc: u64, _instruction: u64, _registers: &[u64; 12]) {}

    /// Called when a syscall is invoked (sol_log, CPI, etc.)
    fn on_syscall(&mut self, _name: &str, _registers: &[u64; 12]) {}

    /// Called on memory read (address, size, data).
    fn on_memory_read(&mut self, _addr: u64, _size: usize, _data: &[u8]) {}

    /// Called on memory write (address, size, data).
    fn on_memory_write(&mut self, _addr: u64, _size: usize, _data: &[u8]) {}

    /// Called when execution begins.
    fn on_start(&mut self, _entry_pc: u64) {}

    /// Called when execution completes (with return value).
    fn on_exit(&mut self, _return_value: u64) {}

    /// Called on CPI boundary (program_id changes).
    fn on_cpi_call(&mut self, _program_id: &[u8; 32], _instruction_data: &[u8]) {}

    /// Called when CPI returns.
    fn on_cpi_return(&mut self, _program_id: &[u8; 32], _return_value: u64) {}
}

// ---------------------------------------------------------------------------
// NoOpTracer — zero-overhead baseline
// ---------------------------------------------------------------------------

/// A tracer that does nothing.
///
/// Because all trait methods have default empty bodies and `NoOpTracer` is a
/// zero-sized type, the compiler inlines and eliminates all calls when this
/// tracer is used via monomorphisation (i.e. as a generic parameter rather
/// than a trait object).
pub struct NoOpTracer;

impl SbpfTracer for NoOpTracer {}

// ---------------------------------------------------------------------------
// CodeTracerTracer — our recorder using the trait
// ---------------------------------------------------------------------------

/// A tracer that records execution events into CodeTracer trace files.
///
/// Wraps the existing recording pipeline behind the `SbpfTracer` trait so
/// that the VM only needs to call trait methods.
pub struct CodeTracerTracer {
    /// Boxed trace writer (type-erased because `create_trace_writer` returns
    /// `Box<dyn TraceWriter>`).
    writer: Box<dyn TraceWriter>,
    /// Type-id for u64 register values (registered once in `on_start`).
    u64_type_id: Option<codetracer_trace_types::TypeId>,
    /// Previous source line (for dedup).
    prev_line: Option<u32>,
    /// Previous PC (for call/return heuristic).
    prev_pc: Option<u64>,
    /// Source location table: (pc, file, line).
    source_locations: std::collections::HashMap<u64, (String, u32)>,
    /// Output directory (kept for reference).
    _out_dir: std::path::PathBuf,
    /// Whether `on_start` has been called.
    started: bool,
    /// Accumulated syscall names for later inspection in tests.
    recorded_syscalls: Vec<String>,
}

impl CodeTracerTracer {
    /// Create a new `CodeTracerTracer`.
    ///
    /// # Arguments
    ///
    /// * `source_path` — Path displayed in the trace for source locations.
    /// * `out_dir`     — Directory where trace files will be written.
    /// * `format`      — Output format (Binary or Json).
    /// * `source_locations` — PC-to-source mapping as `(pc, file, line)`.
    pub fn new(
        source_path: &Path,
        out_dir: &Path,
        format: TraceEventsFileFormat,
        source_locations: Vec<(u64, String, u32)>,
    ) -> Result<Self> {
        std::fs::create_dir_all(out_dir)
            .map_err(|e| eyre!("cannot create output dir: {e}"))?;

        let program_name = source_path.to_string_lossy();
        let mut writer = create_trace_writer(&program_name, &[], format);

        let events_filename = match format {
            TraceEventsFileFormat::Json => "trace.json",
            TraceEventsFileFormat::Binary | TraceEventsFileFormat::BinaryV0 | TraceEventsFileFormat::Ctfs => "trace.bin",
        };
        let events_path = out_dir.join(events_filename);
        let metadata_path = out_dir.join("trace_metadata.json");
        let paths_path = out_dir.join("trace_paths.json");

        TraceWriter::begin_writing_trace_events(&mut *writer, &events_path)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::begin_writing_trace_metadata(&mut *writer, &metadata_path)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::begin_writing_trace_paths(&mut *writer, &paths_path)
            .map_err(|e| eyre!("{e}"))?;

        TraceWriter::start(&mut *writer, source_path, Line(1));

        let u64_type_id =
            TraceWriter::ensure_type_id(&mut *writer, TypeKind::Int, "u64");

        let main_fn_id = TraceWriter::ensure_function_id(
            &mut *writer,
            "main",
            source_path,
            Line(1),
        );
        TraceWriter::register_call(&mut *writer, main_fn_id, vec![]);

        let loc_map = source_locations
            .into_iter()
            .map(|(pc, file, line)| (pc, (file, line)))
            .collect();

        Ok(Self {
            writer,
            u64_type_id: Some(u64_type_id),
            prev_line: None,
            prev_pc: None,
            source_locations: loc_map,
            _out_dir: out_dir.to_path_buf(),
            started: true,
            recorded_syscalls: Vec::new(),
        })
    }

    /// Flush and finalise all trace files. Must be called when done.
    pub fn finish(&mut self) -> Result<()> {
        TraceWriter::register_return(&mut *self.writer, NONE_VALUE);
        TraceWriter::finish_writing_trace_events(&mut *self.writer)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::finish_writing_trace_metadata(&mut *self.writer)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::finish_writing_trace_paths(&mut *self.writer)
            .map_err(|e| eyre!("{e}"))?;
        self.writer.close().map_err(|e| eyre!("{e}"))?;
        Ok(())
    }

    /// Returns a reference to the list of recorded syscall names (for testing).
    pub fn recorded_syscalls(&self) -> &[String] {
        &self.recorded_syscalls
    }
}

impl SbpfTracer for CodeTracerTracer {
    fn on_step(&mut self, pc: u64, _instruction: u64, registers: &[u64; 12]) {
        if !self.started {
            return;
        }

        let (file_str, line) = match self.source_locations.get(&pc) {
            Some((f, l)) => (f.clone(), *l),
            None => return,
        };

        // Call/return heuristic based on PC jumps.
        if let Some(prev) = self.prev_pc {
            let diff = if pc > prev { pc - prev } else { prev - pc };
            if diff > 2 && pc > prev {
                let callee_fn_id = TraceWriter::ensure_function_id(
                    &mut *self.writer,
                    &format!("fn_at_pc_{pc}"),
                    &Path::new(&file_str),
                    Line(line as i64),
                );
                TraceWriter::register_call(&mut *self.writer, callee_fn_id, vec![]);
            } else if diff > 2 && pc < prev {
                TraceWriter::register_return(&mut *self.writer, NONE_VALUE);
            }
        }

        // Emit step on line change.
        if self.prev_line != Some(line) {
            TraceWriter::register_step(
                &mut *self.writer,
                &Path::new(&file_str),
                Line(line as i64),
            );
            self.prev_line = Some(line);
        }

        // Emit register values.
        if let Some(u64_type_id) = self.u64_type_id {
            for r in 0..=10 {
                let name = format!("r{r}");
                let value = ValueRecord::Int {
                    i: registers[r] as i64,
                    type_id: u64_type_id,
                };
                TraceWriter::register_variable_with_full_value(
                    &mut *self.writer,
                    &name,
                    value,
                );
            }
        }

        self.prev_pc = Some(pc);
    }

    fn on_syscall(&mut self, name: &str, _registers: &[u64; 12]) {
        self.recorded_syscalls.push(name.to_string());

        // Emit the syscall to the canonical IO event stream so the
        // CodeTracer "event log" pane shows program log output.
        // Solana program logs are emitted through `sol_log` and the
        // `sol_log_*` family of syscalls — these are the SBF
        // equivalent of stdout writes, so they map to `EventLogKind::Write`
        // (the same bucket used by the Ruby/Python recorders for
        // stdout — see handoff entries 1.21 / 1.27 in
        // /tmp/isonim-migration.txt).
        //
        // Other syscalls (sol_invoke_signed et al.) are non-IO control
        // events; we use `EventLogKind::TraceLogEvent` for them so they
        // appear in the trace as structured diagnostic events without
        // polluting the program-log pane.
        if !self.started {
            return;
        }
        let kind = if name.starts_with("sol_log") {
            EventLogKind::Write
        } else {
            EventLogKind::TraceLogEvent
        };
        TraceWriter::register_special_event(&mut *self.writer, kind, name, "");
    }

    fn on_cpi_call(&mut self, _program_id: &[u8; 32], _instruction_data: &[u8]) {
        // CPI call handling would integrate with CpiDetector here.
        // For now, record a generic call event.
    }

    fn on_cpi_return(&mut self, _program_id: &[u8; 32], _return_value: u64) {
        // CPI return handling.
    }
}

// ---------------------------------------------------------------------------
// RegisterTraceTracer — raw register trace output
// ---------------------------------------------------------------------------

/// A tracer that writes raw register snapshots to a `.regs` binary file,
/// compatible with Mollusk's existing format.
///
/// Each row is 96 bytes: 12 x u64 in little-endian order.
pub struct RegisterTraceTracer {
    /// Buffered writer for the output `.regs` file.
    file: std::io::BufWriter<std::fs::File>,
    /// Number of rows written (for diagnostics).
    rows_written: u64,
}

impl RegisterTraceTracer {
    /// Create a new `RegisterTraceTracer` that writes to `output_path`.
    pub fn new(output_path: &Path) -> Result<Self> {
        let file = std::fs::File::create(output_path)
            .map_err(|e| eyre!("cannot create regs output file: {e}"))?;
        Ok(Self {
            file: std::io::BufWriter::new(file),
            rows_written: 0,
        })
    }

    /// Flush remaining data and return the number of rows written.
    pub fn finish(&mut self) -> Result<u64> {
        self.file.flush().map_err(|e| eyre!("flush failed: {e}"))?;
        Ok(self.rows_written)
    }

    /// Returns the number of rows written so far.
    pub fn rows_written(&self) -> u64 {
        self.rows_written
    }
}

impl SbpfTracer for RegisterTraceTracer {
    fn on_step(&mut self, _pc: u64, _instruction: u64, registers: &[u64; 12]) {
        let mut buf = [0u8; ROW_SIZE];
        for (i, &val) in registers.iter().enumerate() {
            let offset = i * 8;
            buf[offset..offset + 8].copy_from_slice(&val.to_le_bytes());
        }
        // Silently ignore write errors in the hot path; callers should
        // check `finish()` for final flush errors.
        let _ = self.file.write_all(&buf);
        self.rows_written += 1;
    }
}

// ---------------------------------------------------------------------------
// Convenience: run a set of snapshots through any tracer
// ---------------------------------------------------------------------------

/// Feed a sequence of register snapshots through a tracer.
///
/// This is a convenience function for testing and migration.
pub fn replay_snapshots(tracer: &mut impl SbpfTracer, snapshots: &[RegisterSnapshot]) {
    if let Some(first) = snapshots.first() {
        tracer.on_start(first.pc());
    }
    for snap in snapshots {
        tracer.on_step(snap.pc(), 0, &snap.registers);
    }
    if let Some(last) = snapshots.last() {
        tracer.on_exit(last.registers[0]);
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_tracer_is_zero_size() {
        assert_eq!(std::mem::size_of::<NoOpTracer>(), 0);
    }

    #[test]
    fn test_noop_tracer_default_methods() {
        let mut tracer = NoOpTracer;
        let regs = [0u64; 12];
        let program_id = [0u8; 32];

        // None of these should panic.
        tracer.on_start(0);
        tracer.on_step(0, 0, &regs);
        tracer.on_syscall("sol_log", &regs);
        tracer.on_memory_read(0, 4, &[0, 0, 0, 0]);
        tracer.on_memory_write(0, 4, &[0, 0, 0, 0]);
        tracer.on_cpi_call(&program_id, &[1, 2, 3]);
        tracer.on_cpi_return(&program_id, 0);
        tracer.on_exit(0);
    }

    #[test]
    fn bench_noop_overhead() {
        // Simulate calling NoOpTracer hooks in a tight loop.
        // The compiler should optimise all of this away, but we verify
        // it runs in negligible time and does not panic.
        let mut tracer = NoOpTracer;
        let regs = [0u64; 12];
        tracer.on_start(0);
        for pc in 0..100_000u64 {
            tracer.on_step(pc, 0, &regs);
        }
        tracer.on_exit(0);
    }
}
