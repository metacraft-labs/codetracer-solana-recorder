//! Source-level recording of an executed SBF program, driven by its DWARF.
//!
//! Given an [`ExecutionTrace`] (registers before every executed instruction,
//! plus what is needed to reconstruct guest memory) and the program's debug
//! information, this writes a trace of what the *source* program did:
//!
//! * a step whenever execution reaches a new source position of the line
//!   table, attributed to the real source file;
//! * a call when an internal `call` transfers control to a function, and a
//!   return (with the function's typed return value) at its `exit`; inlined
//!   calls described by `DW_TAG_inlined_subroutine` are frames too;
//! * at every step, the named variables DWARF places in scope there, with
//!   the values their location expressions evaluate to against the VM's
//!   registers and memory, decoded through their DWARF types.
//!
//! Nothing is inferred from source text.  A variable DWARF does not describe,
//! or gives no location at an address, is not recorded there.

use std::collections::HashMap;
use std::path::Path;

use codetracer_trace_types::{
    FieldTypeRecord, Line, NONE_VALUE, TypeId, TypeKind, TypeRecord, TypeSpecificInfo, ValueRecord,
};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{TraceEventsFileFormat, create_trace_writer};
use eyre::{Context, Result, eyre};
use solana_sbpf::ebpf;

use crate::executor::ExecutionTrace;
use crate::recorder::read_line_lengths_for_path;
use crate::register_trace::parse_regs_file;
use crate::sbf_memory::{InstructionEffect, ShadowMemory, decode};
use crate::source_debug::{Function, MachineState, Scope, SourceDebugInfo, TypeDesc, TypeRef};

/// Nesting limit when decoding aggregate values (and pointer chains).
const MAX_VALUE_DEPTH: u32 = 6;
/// Longest array rendered element by element.
const MAX_ARRAY_ELEMENTS: u64 = 64;

/// Whether `debug` describes the code `exec` ran, i.e. whether a
/// source-level recording is possible.
pub fn describes_execution(debug: &SourceDebugInfo, exec: &ExecutionTrace) -> bool {
    let Ok(snapshots) = parse_regs_file(&exec.regs) else {
        return false;
    };
    snapshots.first().is_some_and(|s| {
        let addr = debug.address_of(s.pc());
        debug.function_at(addr).is_some()
    })
}

/// A frame on the recorded call stack.
struct ActiveFrame {
    /// Index into `debug.functions`.
    function: usize,
    /// Inlined calls currently active inside this frame, outermost first,
    /// identified by the address of their scope (stable for the run).
    inlined: Vec<*const Scope>,
}

/// Record `exec` as a source-level trace into `out_dir`.
pub fn record_execution(
    exec: &ExecutionTrace,
    debug: &SourceDebugInfo,
    elf_path: &Path,
    out_dir: &Path,
) -> Result<()> {
    let snapshots = parse_regs_file(&exec.regs)?;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // The program is identified by the source of the first instruction
    // with a position; the ELF itself when there is none.
    let first_path = snapshots
        .iter()
        .find_map(|s| debug.position(debug.address_of(s.pc())))
        .map(|p| debug.files[p.file].clone())
        .unwrap_or_else(|| elf_path.to_path_buf());

    let program_name = first_path.to_string_lossy().into_owned();
    let mut writer = create_trace_writer(&program_name, &[], TraceEventsFileFormat::Ctfs);
    let writer: &mut dyn TraceWriter = &mut *writer;
    TraceWriter::begin_writing_trace_events(writer, &out_dir.join("trace.bin"))
        .map_err(|e| eyre!("{e}"))?;

    writer.enable_column_aware_steps();
    writer.enable_column_breakpoints_support();
    writer.enable_column_motions_support();
    let mut line_lengths = Vec::with_capacity(debug.files.len());
    for file in &debug.files {
        let lengths = read_line_lengths_for_path(file);
        let _ = writer.register_path_with_line_lengths(file, &lengths);
        line_lengths.push(lengths);
    }
    TraceWriter::start(writer, &first_path, Line(1));

    let mut rec = SourceRecorder {
        debug,
        writer,
        types: HashMap::new(),
        function_ids: HashMap::new(),
        manual_stack_frames: exec.manual_stack_frames,
        line_lengths,
    };
    rec.run(exec, &snapshots);

    let writer = rec.writer;
    TraceWriter::finish_writing_trace_events(writer).map_err(|e| eyre!("{e}"))?;
    writer
        .write_meta_dat("codetracer-solana-recorder")
        .map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;
    Ok(())
}

struct SourceRecorder<'a> {
    debug: &'a SourceDebugInfo,
    writer: &'a mut dyn TraceWriter,
    types: HashMap<TypeKey, TypeId>,
    function_ids: HashMap<(String, Option<usize>, u32), codetracer_trace_types::FunctionId>,
    manual_stack_frames: bool,
    /// Per file in `debug.files`: the byte length of each source line (empty
    /// when the file is not readable here).
    line_lengths: Vec<Vec<u32>>,
}

#[derive(Hash, PartialEq, Eq)]
enum TypeKey {
    Dwarf(TypeRef),
    Named(u8, String),
}

impl SourceRecorder<'_> {
    fn run(
        &mut self,
        exec: &ExecutionTrace,
        snapshots: &[crate::register_trace::RegisterSnapshot],
    ) {
        let debug = self.debug;
        let mut memory =
            ShadowMemory::new(exec.initial_memory.clone(), exec.moved_memory_instructions);
        let mut syscall_writes = exec.syscall_writes.iter();
        let mut stack: Vec<ActiveFrame> = Vec::new();
        let mut last_position: Option<(usize, u32, Option<u32>)> = None;

        for (i, snap) in snapshots.iter().enumerate() {
            let pc = snap.pc();
            let addr = debug.address_of(pc);
            let regs = &snap.registers;

            // Frame transitions caused by the previous instruction.
            if i == 0 {
                if let Some(f) = debug.function_at(addr) {
                    self.call(f);
                    stack.push(ActiveFrame {
                        function: f,
                        inlined: Vec::new(),
                    });
                }
            } else {
                let prev = &snapshots[i - 1];
                let continued = pc == prev.pc() + 1;
                match decode(&exec.text, prev.pc()).map(|insn| insn.opc) {
                    Some(ebpf::CALL_IMM) | Some(ebpf::CALL_REG) if !continued => {
                        if let Some(f) = debug.function_at(addr) {
                            self.call(f);
                            stack.push(ActiveFrame {
                                function: f,
                                inlined: Vec::new(),
                            });
                            last_position = None;
                        }
                    }
                    Some(ebpf::EXIT) => {
                        if let Some(frame) = stack.pop() {
                            self.unwind_inlined(&frame);
                            let value = self.return_value(
                                debug.functions[frame.function].return_type,
                                prev.registers[0],
                                &memory,
                            );
                            TraceWriter::register_return(self.writer, value);
                            last_position = None;
                        }
                    }
                    _ => {}
                }
            }

            // Inlined calls entered or left within the current frame.
            if let Some(frame) = stack.last_mut() {
                let function = &debug.functions[frame.function];
                let chain: Vec<&Scope> = function
                    .scope_chain(addr)
                    .into_iter()
                    .filter(|s| s.inlined.is_some())
                    .collect();
                let common = frame
                    .inlined
                    .iter()
                    .zip(&chain)
                    .take_while(|(a, b)| std::ptr::eq(**a, **b as *const Scope))
                    .count();
                if common < frame.inlined.len() || common < chain.len() {
                    for _ in common..frame.inlined.len() {
                        TraceWriter::register_return(self.writer, NONE_VALUE);
                    }
                    frame.inlined.truncate(common);
                    for scope in &chain[common..] {
                        let call = scope.inlined.as_ref().expect("filtered to inlined scopes");
                        let id = self.function_id(&call.name, call.decl_file, call.decl_line);
                        TraceWriter::register_call(self.writer, id, vec![]);
                        frame.inlined.push(*scope as *const Scope);
                    }
                    last_position = None;
                }
            }

            // A step at every new source position.
            if let Some(position) = debug.position(addr) {
                let key = (position.file, position.line, position.column);
                if last_position != Some(key) {
                    last_position = Some(key);
                    let path = &debug.files[position.file];
                    // A column is only encodable inside the text of its line:
                    // the writer addresses (line, column) as one offset into
                    // the file, so a column past the line's end would read
                    // back as a later line.  Such columns (and every column
                    // of a file not readable here) are recorded line-only.
                    let column = position.column.filter(|&c| {
                        self.line_lengths[position.file]
                            .get(position.line as usize - 1)
                            .is_some_and(|&len| c >= 1 && c <= len)
                    });
                    TraceWriter::register_step_with_column(
                        self.writer,
                        path,
                        Line(position.line as i64),
                        column.map(|c| Line(c as i64)),
                    );
                    if let Some(frame) = stack.last() {
                        self.record_locals(&debug.functions[frame.function], addr, regs, &memory);
                    }
                }
            }

            // Bring memory up to date with this instruction's effect, so it
            // is current at the next snapshot.
            if memory.apply_instruction(&exec.text, pc, regs) == InstructionEffect::MaybeSyscall {
                let next_continues = snapshots.get(i + 1).is_none_or(|n| n.pc() == pc + 1);
                if next_continues && let Some(writes) = syscall_writes.next() {
                    for (at, bytes) in writes {
                        memory.write(*at, bytes);
                    }
                }
            }
        }

        // Frames still open when execution stopped: the outermost `exit`
        // ends the program (its return value is the program's result);
        // anything else was cut short by an error.
        let last = snapshots.last();
        let ended_with_exit = last
            .and_then(|s| decode(&exec.text, s.pc()))
            .is_some_and(|insn| insn.opc == ebpf::EXIT);
        while let Some(frame) = stack.pop() {
            self.unwind_inlined(&frame);
            let value = match last {
                Some(s) if ended_with_exit && stack.is_empty() => self.return_value(
                    debug.functions[frame.function].return_type,
                    s.registers[0],
                    &memory,
                ),
                _ => NONE_VALUE,
            };
            TraceWriter::register_return(self.writer, value);
        }
    }

    /// Close the inlined-call frames open inside `frame`.
    fn unwind_inlined(&mut self, frame: &ActiveFrame) {
        for _ in 0..frame.inlined.len() {
            TraceWriter::register_return(self.writer, NONE_VALUE);
        }
    }

    fn call(&mut self, function: usize) {
        let f = &self.debug.functions[function];
        let id = self.function_id(&f.name, f.decl_file, f.decl_line);
        TraceWriter::register_call(self.writer, id, vec![]);
    }

    fn function_id(
        &mut self,
        name: &str,
        decl_file: Option<usize>,
        decl_line: u32,
    ) -> codetracer_trace_types::FunctionId {
        let key = (name.to_string(), decl_file, decl_line);
        if let Some(id) = self.function_ids.get(&key) {
            return *id;
        }
        let path = decl_file
            .map(|f| self.debug.files[f].clone())
            .unwrap_or_default();
        let id = TraceWriter::ensure_function_id(self.writer, name, &path, Line(decl_line as i64));
        self.function_ids.insert(key, id);
        id
    }

    /// The typed return value of a function returning `ty`, held in `r0`.
    fn return_value(&mut self, ty: Option<TypeRef>, r0: u64, memory: &ShadowMemory) -> ValueRecord {
        let Some(ty) = ty else {
            return NONE_VALUE;
        };
        match self.debug.type_size(ty) {
            Some(size) if size <= 8 => {
                let bytes = r0.to_le_bytes()[..size as usize].to_vec();
                self.decode_value(ty, &bytes, memory, 0)
            }
            // Larger values are returned through memory the caller
            // provides; DWARF does not say where.
            _ => NONE_VALUE,
        }
    }

    /// Record every variable in scope at `addr` in `function`'s innermost
    /// frame (the function body or the innermost inlined call).
    fn record_locals(
        &mut self,
        function: &Function,
        addr: u64,
        regs: &[u64; 12],
        memory: &ShadowMemory,
    ) {
        let chain = function.scope_chain(addr);
        let frame_start = chain
            .iter()
            .rposition(|s| s.inlined.is_some())
            .map(|p| p + 1);
        let mut scopes: Vec<&Scope> = Vec::new();
        match frame_start {
            Some(start) => scopes.extend(&chain[start - 1..]),
            None => {
                scopes.push(&function.body);
                scopes.extend(&chain);
            }
        }
        let read = |at: u64, len: usize| memory.read(at, len).map(<[u8]>::to_vec);
        let state = MachineState {
            registers: regs,
            read_memory: &read,
            manual_stack_frames: self.manual_stack_frames,
        };
        for scope in scopes {
            for var in &scope.variables {
                let Some(ty) = var.type_ref else { continue };
                let Some(bytes) = self.debug.read_variable(function, var, addr, &state) else {
                    continue;
                };
                let value = self.decode_value(ty, &bytes, memory, 0);
                TraceWriter::register_variable_with_full_value(self.writer, &var.name, value);
            }
        }
    }

    fn named_type(&mut self, kind: TypeKind, name: &str) -> TypeId {
        let key = TypeKey::Named(kind as u8, name.to_string());
        if let Some(id) = self.types.get(&key) {
            return *id;
        }
        let id = TraceWriter::ensure_type_id(self.writer, kind, name);
        self.types.insert(key, id);
        id
    }

    fn dwarf_type(&mut self, ty: TypeRef, kind: TypeKind) -> TypeId {
        if let Some(id) = self.types.get(&TypeKey::Dwarf(ty)) {
            return *id;
        }
        let name = self.debug.type_name(ty);
        let id = match self.debug.types.get(ty) {
            Some(TypeDesc::Struct { members, .. }) => {
                let fields = members
                    .iter()
                    .map(|(field, _, t)| FieldTypeRecord {
                        name: field.clone(),
                        type_id: match t {
                            Some(t) => {
                                let k = self.kind_of(*t);
                                self.dwarf_type(*t, k)
                            }
                            None => self.named_type(TypeKind::Raw, "?"),
                        },
                    })
                    .collect();
                TraceWriter::ensure_raw_type_id(
                    self.writer,
                    TypeRecord {
                        kind: TypeKind::Struct,
                        lang_type: name,
                        specific_info: TypeSpecificInfo::Struct { fields },
                    },
                )
            }
            _ => TraceWriter::ensure_type_id(self.writer, kind, &name),
        };
        self.types.insert(TypeKey::Dwarf(ty), id);
        id
    }

    fn kind_of(&self, ty: TypeRef) -> TypeKind {
        match self.debug.types.get(ty) {
            Some(TypeDesc::Base { encoding, .. }) => match *encoding {
                gimli::DW_ATE_boolean => TypeKind::Bool,
                gimli::DW_ATE_float => TypeKind::Float,
                gimli::DW_ATE_UTF | gimli::DW_ATE_unsigned_char | gimli::DW_ATE_signed_char => {
                    TypeKind::Char
                }
                _ => TypeKind::Int,
            },
            Some(TypeDesc::Pointer { .. }) => TypeKind::Pointer,
            Some(TypeDesc::Struct { .. }) => TypeKind::Struct,
            Some(TypeDesc::Array { .. }) => TypeKind::Array,
            Some(TypeDesc::Alias { target: Some(t) }) => self.kind_of(*t),
            _ => TypeKind::Raw,
        }
    }

    /// Decode the bytes of a value of DWARF type `ty`.
    fn decode_value(
        &mut self,
        ty: TypeRef,
        bytes: &[u8],
        memory: &ShadowMemory,
        depth: u32,
    ) -> ValueRecord {
        let kind = self.kind_of(ty);
        let desc = self.debug.types.get(ty).cloned();
        match desc {
            Some(TypeDesc::Alias { target: Some(t) }) => self.decode_value(t, bytes, memory, depth),
            Some(TypeDesc::Base { encoding, size, .. }) => {
                let type_id = self.dwarf_type(ty, kind);
                if size == 0 {
                    // A zero-sized value such as `()` has no bits to show.
                    ValueRecord::None { type_id }
                } else {
                    decode_base(encoding, size, bytes, type_id)
                }
            }
            Some(TypeDesc::Pointer { target, .. }) => {
                let type_id = self.dwarf_type(ty, kind);
                let address = le_u64(bytes);
                let pointee = target.filter(|_| depth < MAX_VALUE_DEPTH).and_then(|t| {
                    let size = self.debug.type_size(t)?;
                    let data = memory.read(address, size as usize)?.to_vec();
                    Some(self.decode_value(t, &data, memory, depth + 1))
                });
                match pointee {
                    Some(dereferenced) => ValueRecord::Reference {
                        dereferenced: Box::new(dereferenced),
                        address,
                        mutable: true,
                        type_id,
                    },
                    None => ValueRecord::Raw {
                        r: format!("{address:#x}"),
                        type_id,
                    },
                }
            }
            Some(TypeDesc::Struct { members, .. }) if depth < MAX_VALUE_DEPTH => {
                let type_id = self.dwarf_type(ty, kind);
                let field_values = members
                    .iter()
                    .map(|(_, offset, t)| {
                        let size = t.and_then(|t| self.debug.type_size(t)).unwrap_or(0) as usize;
                        let start = *offset as usize;
                        match (t, bytes.get(start..start + size)) {
                            (Some(t), Some(field)) => {
                                self.decode_value(*t, field, memory, depth + 1)
                            }
                            _ => NONE_VALUE,
                        }
                    })
                    .collect();
                ValueRecord::Struct {
                    field_values,
                    type_id,
                }
            }
            Some(TypeDesc::Array {
                element: Some(element),
                count: Some(count),
                ..
            }) if depth < MAX_VALUE_DEPTH && count <= MAX_ARRAY_ELEMENTS => {
                let type_id = self.dwarf_type(ty, kind);
                let size = self.debug.type_size(element).unwrap_or(0) as usize;
                let elements = (0..count as usize)
                    .map(|i| match bytes.get(i * size..(i + 1) * size) {
                        Some(e) => self.decode_value(element, e, memory, depth + 1),
                        None => NONE_VALUE,
                    })
                    .collect();
                ValueRecord::Sequence {
                    elements,
                    is_slice: false,
                    type_id,
                }
            }
            _ => {
                let type_id = self.dwarf_type(ty, TypeKind::Raw);
                ValueRecord::Raw {
                    r: bytes.iter().map(|b| format!("{b:02x}")).collect(),
                    type_id,
                }
            }
        }
    }
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    let n = bytes.len().min(8);
    word[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(word)
}

fn decode_base(encoding: gimli::DwAte, size: u64, bytes: &[u8], type_id: TypeId) -> ValueRecord {
    match encoding {
        gimli::DW_ATE_boolean => ValueRecord::Bool {
            b: bytes.iter().any(|b| *b != 0),
            type_id,
        },
        gimli::DW_ATE_float if size == 4 => ValueRecord::Float {
            f: f32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")) as f64,
            type_id,
        },
        gimli::DW_ATE_float if size == 8 => ValueRecord::Float {
            f: f64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")),
            type_id,
        },
        gimli::DW_ATE_UTF | gimli::DW_ATE_unsigned_char | gimli::DW_ATE_signed_char
            if size <= 4 =>
        {
            match char::from_u32(le_u64(bytes) as u32) {
                Some(c) => ValueRecord::Char { c, type_id },
                None => ValueRecord::Int {
                    i: le_u64(bytes) as i64,
                    type_id,
                },
            }
        }
        gimli::DW_ATE_signed if size <= 8 => {
            let shift = 64 - 8 * size.max(1) as u32;
            ValueRecord::Int {
                i: ((le_u64(bytes) << shift) as i64) >> shift,
                type_id,
            }
        }
        _ if size <= 8 => {
            let v = le_u64(bytes);
            match i64::try_from(v) {
                Ok(i) => ValueRecord::Int { i, type_id },
                Err(_) => ValueRecord::BigInt {
                    b: v.to_be_bytes().to_vec(),
                    negative: false,
                    type_id,
                },
            }
        }
        // 128-bit integers.
        gimli::DW_ATE_signed => {
            let v = i128::from_le_bytes(pad16(bytes));
            match i64::try_from(v) {
                Ok(i) => ValueRecord::Int { i, type_id },
                Err(_) => ValueRecord::BigInt {
                    b: v.unsigned_abs().to_be_bytes().to_vec(),
                    negative: v < 0,
                    type_id,
                },
            }
        }
        _ => {
            let v = u128::from_le_bytes(pad16(bytes));
            match i64::try_from(v) {
                Ok(i) => ValueRecord::Int { i, type_id },
                Err(_) => ValueRecord::BigInt {
                    b: v.to_be_bytes().to_vec(),
                    negative: false,
                    type_id,
                },
            }
        }
    }
}

fn pad16(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let n = bytes.len().min(16);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}
