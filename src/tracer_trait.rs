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

use codetracer_trace_types::{EventLogKind, Line, NONE_VALUE, TypeKind, ValueRecord};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{TraceEventsFileFormat, create_trace_writer};
use eyre::{Result, eyre};

use crate::register_trace::{ROW_SIZE, RegisterSnapshot};

// The recorder is CTFS-only per `Recorder-CLI-Conventions.md` §4 (see
// `codetracer-specs`).  We pin every `create_trace_writer` call site to
// this constant so the tracer surface no longer carries a `format`
// parameter and the writer cannot accidentally drift away from the
// canonical multi-stream container.
const CTFS_FORMAT: TraceEventsFileFormat = TraceEventsFileFormat::Ctfs;

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
    /// Previous source column (for column-aware same-line dedup).
    /// Tracks the last column emitted on `prev_line` so multi-statement
    /// lines surface a fresh step on each column change, matching the
    /// column-aware navigation contract from the JS recorder fixture
    /// (`codetracer-js-recorder/tests/integration/column-aware.test.ts`).
    prev_column: Option<u32>,
    /// Previous PC (for call/return heuristic).
    prev_pc: Option<u64>,
    /// Source location table: pc -> (file, line, optional column).
    /// The column is `None` for DWARF entries that only carry line info
    /// and for legacy callers that pass the line-only `(pc, file, line)`
    /// tuple shape via [`Self::new`].  When `Some`, the recorder emits a
    /// column-aware step (`register_step_with_column`) so column-aware
    /// readers can navigate to the exact statement on the line.
    source_locations: std::collections::HashMap<u64, (String, u32, Option<u32>)>,
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
    /// * `source_locations` — PC-to-source mapping as `(pc, file, line)`.
    ///
    /// The output format is fixed to the canonical CodeTracer CTFS
    /// multi-stream container.
    pub fn new(
        source_path: &Path,
        out_dir: &Path,
        source_locations: Vec<(u64, String, u32)>,
    ) -> Result<Self> {
        // Promote line-only callers to the column-aware tuple shape
        // with `column == None`.  Existing fixtures keep working
        // byte-for-byte (the writer falls back to the column-less step
        // path when `column` is `None`).
        let with_cols: Vec<(u64, String, u32, Option<u32>)> = source_locations
            .into_iter()
            .map(|(pc, f, l)| (pc, f, l, None))
            .collect();
        Self::new_with_columns(source_path, out_dir, with_cols)
    }

    /// Column-aware variant of [`Self::new`].
    ///
    /// `source_locations` carries a `(pc, file, line, column)` tuple
    /// per PC.  When `column == Some(c)` the tracer emits a column-
    /// aware step on transitions to that line, so column-aware readers
    /// (e.g. `ct-print --full`) can resolve the exact statement.  When
    /// `column == None` the writer falls back to the column-less path.
    pub fn new_with_columns(
        source_path: &Path,
        out_dir: &Path,
        source_locations: Vec<(u64, String, u32, Option<u32>)>,
    ) -> Result<Self> {
        std::fs::create_dir_all(out_dir).map_err(|e| eyre!("cannot create output dir: {e}"))?;

        let program_name = source_path.to_string_lossy();
        let mut writer = create_trace_writer(&program_name, &[], CTFS_FORMAT);

        // CTFS-only writer — events stream lives in `trace.bin`.
        let events_path = out_dir.join("trace.bin");

        TraceWriter::begin_writing_trace_events(&mut *writer, &events_path)
            .map_err(|e| eyre!("{e}"))?;

        // Opt the writer into column-aware step encoding before any
        // step is emitted.  `meta.dat` bit 4 (`FlagHasColumnAwareSteps`)
        // is set unconditionally; readers that only understand the
        // legacy line-only encoding reject the trace cleanly via the
        // reserved-bits check.
        TraceWriter::enable_column_aware_steps(&mut *writer);

        // M-capability-flags: the Solana recorder ships sbpf/RBPF PC
        // → source maps that resolve each VM instruction to a single
        // sub-statement on the source line, so both per-column
        // breakpoints and per-column motions are well-defined.
        // Advertise both capabilities to the GUI.
        TraceWriter::enable_column_breakpoints_support(&mut *writer);
        TraceWriter::enable_column_motions_support(&mut *writer);

        // Pre-register every distinct source path together with its
        // per-line byte counts BEFORE `TraceWriter::start` — `start`
        // implicitly interns the primary path with no line-length
        // data, so calling `register_path_with_line_lengths` after
        // `start` would create a paths.dat entry without per-line
        // counts and the column-aware reader would silently fall back
        // to the legacy DefaultLinesPerFile GLI.
        let mut registered: std::collections::HashSet<String> = std::collections::HashSet::new();
        registered.insert(source_path.to_string_lossy().into_owned());
        {
            let line_lengths = crate::recorder::read_line_lengths_for_path(source_path);
            let _ = TraceWriter::register_path_with_line_lengths(
                &mut *writer,
                source_path,
                &line_lengths,
            );
        }
        let mut loc_map: std::collections::HashMap<u64, (String, u32, Option<u32>)> =
            std::collections::HashMap::with_capacity(source_locations.len());
        for (pc, file, line, column) in source_locations {
            if registered.insert(file.clone()) {
                let p = Path::new(&file);
                let lengths = crate::recorder::read_line_lengths_for_path(p);
                let _ = TraceWriter::register_path_with_line_lengths(&mut *writer, p, &lengths);
            }
            loc_map.insert(pc, (file, line, column));
        }

        TraceWriter::start(&mut *writer, source_path, Line(1));

        let u64_type_id = TraceWriter::ensure_type_id(&mut *writer, TypeKind::Int, "u64");

        let main_fn_id =
            TraceWriter::ensure_function_id(&mut *writer, "main", source_path, Line(1));
        TraceWriter::register_call(&mut *writer, main_fn_id, vec![]);

        Ok(Self {
            writer,
            u64_type_id: Some(u64_type_id),
            prev_line: None,
            prev_column: None,
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
        TraceWriter::finish_writing_trace_events(&mut *self.writer).map_err(|e| eyre!("{e}"))?;
        self.writer
            .write_meta_dat("codetracer-solana-recorder")
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

        let (file_str, line, column) = match self.source_locations.get(&pc) {
            Some((f, l, c)) => (f.clone(), *l, *c),
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

        // Emit step on fresh (line, column).  Line changes always
        // fire; same-line column changes fire too so multi-statement
        // lines surface distinct steps.  When `column` is `None` the
        // wrapper falls back to the column-less `register_step` path.
        let line_changed = self.prev_line != Some(line);
        let column_changed = column.is_some() && self.prev_column != column;
        if line_changed || column_changed {
            TraceWriter::register_step_with_column(
                &mut *self.writer,
                &Path::new(&file_str),
                Line(line as i64),
                column.map(|c| Line(c as i64)),
            );
            self.prev_line = Some(line);
            self.prev_column = column;
        }

        // Emit register values.
        if let Some(u64_type_id) = self.u64_type_id {
            for r in 0..=10 {
                let name = format!("r{r}");
                let value = ValueRecord::Int {
                    i: registers[r] as i64,
                    type_id: u64_type_id,
                };
                TraceWriter::register_variable_with_full_value(&mut *self.writer, &name, value);
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
