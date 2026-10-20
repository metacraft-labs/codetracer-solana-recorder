//! Writes one replayed transaction as a CodeTracer trace.
//!
//! Positions are T1: a step points at the executed instruction's line in a
//! disassembly listing of the program's own executable, never at a source
//! line guessed from text. Two files carry the positions:
//!
//! * `transaction.txt` — the transaction's whole instruction trace, one line
//!   per invocation, indented by stack height. A builtin program (which runs
//!   no SBF) and the transaction frame itself step here.
//! * `listings/<program>-<text key>.sbf` — one per executable that ran.
//!
//! Frames are exact rather than inferred. An internal call is a `call`/`callx`
//! after which the VM's next instruction is not the following one; its frame
//! ends when the VM reaches the return address that call pushed. A
//! cross-program invocation is the `sol_invoke_signed_*` syscall, and its
//! frame is the next invocation the runtime records as made by this one — the same
//! entry the runtime recorded, so the callee's steps are attributed to the
//! callee. Values are the VM's registers, named `r0`..`r10`: machine state,
//! labelled as what it is, never presented as recovered variables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use codetracer_trace_types::{EventLogKind, Line, NONE_VALUE, TypeKind, ValueRecord};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{NimTraceWriter, TraceEventsFileFormat};
use eyre::{Result, eyre};

use super::engine::{InstructionRecord, TxExecution};
use super::listing::{Flow, Listing};

pub struct TraceOptions {
    /// Record `r0`..`r10` at every step.
    pub registers: bool,
}

pub struct WrittenTrace {
    pub dir: PathBuf,
    pub steps: u64,
    pub calls: u64,
    pub listings: Vec<PathBuf>,
}

struct Ctx<'a> {
    writer: &'a mut dyn TraceWriter,
    u64_type: codetracer_trace_types::TypeId,
    tx_path: PathBuf,
    listing_paths: HashMap<u64, PathBuf>,
    listings: HashMap<u64, Arc<Listing>>,
    records: &'a [InstructionRecord],
    /// `transaction.txt` line of each instruction-trace entry.
    record_line: Vec<u32>,
    opts: &'a TraceOptions,
    steps: u64,
    calls: u64,
}

pub fn write_transaction_trace(
    exec: &TxExecution,
    listings: &HashMap<u64, Arc<Listing>>,
    out_dir: &Path,
    opts: &TraceOptions,
) -> Result<WrittenTrace> {
    std::fs::create_dir_all(out_dir.join("listings"))
        .map_err(|e| eyre!("cannot create {}: {e}", out_dir.display()))?;
    let out_dir = std::fs::canonicalize(out_dir)?;

    // transaction.txt: header, then one line per instruction-trace entry.
    let mut tx_lines = vec![
        format!("transaction {}", exec.signature),
        format!("slot {}", exec.slot),
    ];
    // Listed as a tree: each invocation followed by the ones it made.
    let mut record_line = vec![0u32; exec.instructions.len()];
    let mut order: Vec<usize> = exec
        .instructions
        .iter()
        .enumerate()
        .filter(|(_, r)| r.caller.is_none())
        .map(|(i, _)| i)
        .rev()
        .collect();
    while let Some(i) = order.pop() {
        let r = &exec.instructions[i];
        let indent = "  ".repeat(r.stack_height.saturating_sub(1));
        let kind = if r.registers.is_some() {
            "sbf"
        } else {
            "builtin"
        };
        tx_lines.push(format!(
            "{indent}[{}] {} ({kind})",
            r.index_in_trace, r.program_id
        ));
        record_line[i] = tx_lines.len() as u32;
        order.extend(
            children_of(&exec.instructions, r.index_in_trace)
                .into_iter()
                .rev(),
        );
    }
    // Paths are recorded RELATIVE to the trace directory: an absolute path
    // would put the directory a trace happened to be written in into the
    // container's bytes, and the same execution rebuilt elsewhere would differ.
    let tx_path = PathBuf::from("transaction.txt");
    std::fs::write(out_dir.join(&tx_path), tx_lines.join("\n") + "\n")?;

    let mut listing_paths = HashMap::new();
    let mut used = HashMap::new();
    for r in &exec.instructions {
        if let Some(key) = r.listing {
            if let std::collections::hash_map::Entry::Vacant(slot) = listing_paths.entry(key) {
                let listing = listings
                    .get(&key)
                    .ok_or_else(|| eyre!("no listing for executable {key:016x}"))?;
                let path =
                    PathBuf::from("listings").join(format!("{}-{key:016x}.sbf", r.program_id));
                std::fs::write(out_dir.join(&path), listing.text())?;
                slot.insert(path);
                used.insert(key, listing.clone());
            }
        }
    }

    // The recording's identity is DERIVED, not minted, so the same execution
    // always writes the same container bytes; a trace rebuilt from its bundle
    // in a sandbox is then byte-identical to the one the capture wrote.
    let mut writer = Box::new(NimTraceWriter::new(
        &exec.signature,
        &[],
        TraceEventsFileFormat::Ctfs,
    ));
    writer
        .set_recording_id(&recording_id(exec.block_time, &exec.signature))
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_events(&mut *writer, &out_dir.join("trace.bin"))
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::start(&mut *writer, &tx_path, Line(1));
    let u64_type = TraceWriter::ensure_type_id(&mut *writer, TypeKind::Int, "u64");

    let mut ctx = Ctx {
        writer: &mut *writer,
        u64_type,
        tx_path: tx_path.clone(),
        listing_paths: listing_paths.clone(),
        listings: used,
        records: &exec.instructions,
        record_line,
        opts,
        steps: 0,
        calls: 0,
    };

    let root = TraceWriter::ensure_function_id(
        ctx.writer,
        &format!("transaction {}", exec.signature),
        &tx_path,
        Line(1),
    );
    TraceWriter::register_call(ctx.writer, root, vec![]);
    TraceWriter::register_step(ctx.writer, &tx_path, Line(1));
    ctx.steps += 1;

    for i in 0..exec.instructions.len() {
        if exec.instructions[i].caller.is_none() {
            ctx.invocation(i)?;
        }
    }

    TraceWriter::register_return(ctx.writer, NONE_VALUE);
    let (steps, calls) = (ctx.steps, ctx.calls);
    TraceWriter::finish_writing_trace_events(&mut *writer).map_err(|e| eyre!("{e}"))?;
    writer
        .write_meta_dat("codetracer-solana-recorder")
        .map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;

    Ok(WrittenTrace {
        dir: out_dir.clone(),
        steps,
        calls,
        listings: listing_paths
            .into_values()
            .map(|p| out_dir.join(p))
            .collect(),
    })
}

/// A UUIDv7 for the recording, derived from the transaction: the block time
/// as its millisecond timestamp, and SHA-256 of the signature for the 74 bits
/// a minted UUIDv7 would fill at random.
pub fn recording_id(block_time: Option<i64>, signature: &str) -> String {
    use sha2::{Digest, Sha256};
    let ms = (block_time.unwrap_or(0).max(0) as u64).saturating_mul(1000) & 0xffff_ffff_ffff;
    let h = Sha256::digest(signature.as_bytes());
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6..].copy_from_slice(&h[..10]);
    b[6] = 0x70 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let x: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &x[0..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..32]
    )
}

/// Positions (in `records`) of the invocations `caller` made, in order.
fn children_of(records: &[InstructionRecord], caller: usize) -> Vec<usize> {
    records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.caller == Some(caller))
        .map(|(i, _)| i)
        .collect()
}

impl Ctx<'_> {
    /// Writes the invocation at `index` (a frame, its steps, and every
    /// invocation it made).
    fn invocation(&mut self, index: usize) -> Result<()> {
        let records = self.records;
        let record = &records[index];
        let height = record.stack_height;
        // Direct children, in the order the runtime invoked them.
        let mut children: std::collections::VecDeque<usize> =
            children_of(records, record.index_in_trace).into();

        let line = self.record_line[index];
        let label = if height == 1 {
            format!(
                "instruction {} → {}",
                record.index_in_trace, record.program_id
            )
        } else {
            format!("cpi → {}", record.program_id)
        };
        let fid =
            TraceWriter::ensure_function_id(self.writer, &label, &self.tx_path, Line(line as i64));
        TraceWriter::register_call(self.writer, fid, vec![]);
        self.calls += 1;

        match (&record.registers, record.listing) {
            (Some(regs), Some(key)) => {
                let listing = self
                    .listings
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| eyre!("no listing for {key:016x}"))?;
                let path = self.listing_paths[&key].clone();
                self.steps_of(regs, &listing, &path, &mut children)?;
            }
            _ => {
                TraceWriter::register_step(self.writer, &self.tx_path, Line(line as i64));
                self.steps += 1;
                // A builtin can itself invoke programs (rarely); keep their
                // frames rather than dropping them.
                while let Some(c) = children.pop_front() {
                    self.invocation(c)?;
                }
            }
        }
        // Children the caller's register trace did not reach (an aborted
        // invocation) are still written, so no executed program goes missing.
        while let Some(c) = children.pop_front() {
            self.invocation(c)?;
        }
        TraceWriter::register_return(self.writer, NONE_VALUE);
        Ok(())
    }

    fn steps_of(
        &mut self,
        regs: &[[u64; 12]],
        listing: &Listing,
        path: &Path,
        children: &mut std::collections::VecDeque<usize>,
    ) -> Result<()> {
        // Return addresses of the internal calls currently open.
        let mut returns: Vec<usize> = Vec::new();
        for (j, entry) in regs.iter().enumerate() {
            let pc = entry[11] as usize;
            TraceWriter::register_step(self.writer, path, Line(listing.line_of(pc) as i64));
            self.steps += 1;
            if self.opts.registers {
                for (r, value) in entry.iter().take(11).enumerate() {
                    TraceWriter::register_variable_with_full_value(
                        self.writer,
                        &format!("r{r}"),
                        ValueRecord::Int {
                            i: *value as i64,
                            type_id: self.u64_type,
                        },
                    );
                }
            }
            let next_pc = regs.get(j + 1).map(|e| e[11] as usize);
            match listing.flow_of(pc) {
                Flow::Call { syscall } => {
                    if next_pc.is_some_and(|n| n != pc + 1) {
                        let target = next_pc.unwrap();
                        let fid = TraceWriter::ensure_function_id(
                            self.writer,
                            &listing.function_name(target),
                            path,
                            Line(listing.line_of(target) as i64),
                        );
                        TraceWriter::register_call(self.writer, fid, vec![]);
                        self.calls += 1;
                        returns.push(pc + 1);
                    } else {
                        let name = syscall.clone().unwrap_or_else(|| "syscall".into());
                        TraceWriter::register_special_event(
                            self.writer,
                            EventLogKind::TraceLogEvent,
                            "syscall",
                            &name,
                        );
                        if name.starts_with("sol_invoke_signed") {
                            if let Some(c) = children.pop_front() {
                                self.invocation(c)?;
                            }
                        }
                    }
                }
                Flow::Exit => {
                    if let (Some(&ret), Some(n)) = (returns.last(), next_pc) {
                        if n == ret {
                            returns.pop();
                            TraceWriter::register_return(
                                self.writer,
                                ValueRecord::Int {
                                    i: entry[0] as i64,
                                    type_id: self.u64_type,
                                },
                            );
                        }
                    }
                }
                Flow::Plain => {}
            }
        }
        // An invocation that aborted inside an internal call leaves its
        // frames open; close them so the instruction frame closes cleanly.
        for _ in returns {
            TraceWriter::register_return(self.writer, NONE_VALUE);
        }
        Ok(())
    }
}
