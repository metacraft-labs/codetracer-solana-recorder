//! Source-level debug model of an SBF program, read from its DWARF.
//!
//! [`SourceDebugInfo`] answers the questions a source-level recorder asks
//! about an address in the program:
//!
//! * which source file, line and column it belongs to (the line table, with
//!   each file's directory resolved the way the compiler meant it -- see
//!   [`PathResolver`]);
//! * which function it is in, and which inlined calls enclose it
//!   (`DW_TAG_subprogram` / `DW_TAG_inlined_subroutine`);
//! * which named variables are in scope there, where each one lives
//!   (`DW_AT_location`, including location lists) and what its type is.
//!
//! Location expressions are evaluated against the VM's registers and memory
//! by [`SourceDebugInfo::read_variable`].
//!
//! # SBF frame base
//!
//! The SBF backend declares `DW_AT_frame_base` as `DW_OP_reg10`, but the
//! offsets of `DW_OP_fbreg` are measured from the *bottom* of the function's
//! stack frame: the backend's default frame-index reference is
//! `object_offset + stack_size`, while the code addresses the same object as
//! `r10 + object_offset`.  With fixed stack frames (`r10` does not move
//! inside a function) the object therefore lives at
//! `r10 + fbreg_offset - stack_size`, where `stack_size` is the function's
//! frame size rounded up to the backend's 64-byte stack alignment.
//! The frame size is not recorded in DWARF (`.debug_frame` carries no CFA
//! rules for SBF), so it is recovered from the function's own code: the
//! deepest `r10`-relative access it makes, together with the extent of every
//! `DW_OP_fbreg` variable DWARF places in the frame.  With dynamic stack
//! frames the prologue moves `r10` down by `stack_size` itself, and the
//! frame base is `r10` unadjusted.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use eyre::{Result, eyre};
use gimli::{AttributeValue, EndianSlice, Reader, RunTimeEndian};
use solana_sbpf::ebpf;

type R = EndianSlice<'static, RunTimeEndian>;

/// Stack alignment of the SBF LLVM backend; function frame sizes are
/// multiples of it.
const SBF_STACK_ALIGN: u64 = 64;

/// The frame-pointer register (`r10`), which is also DWARF register 10.
const FRAME_POINTER: u16 = 10;

/// A resolved source position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePosition {
    pub file: usize,
    pub line: u32,
    pub column: Option<u32>,
}

/// Index of a type in [`SourceDebugInfo::types`].
pub type TypeRef = usize;

/// The shape of a DWARF type, as far as the recorder decodes values.
#[derive(Debug, Clone)]
pub enum TypeDesc {
    Base {
        name: String,
        encoding: gimli::DwAte,
        size: u64,
    },
    Pointer {
        name: String,
        target: Option<TypeRef>,
    },
    Struct {
        name: String,
        size: u64,
        members: Vec<(String, u64, Option<TypeRef>)>,
    },
    Array {
        name: String,
        element: Option<TypeRef>,
        count: Option<u64>,
    },
    /// typedef / const / volatile: same representation as the target.
    Alias { target: Option<TypeRef> },
    /// Anything else (enums, unions, Rust enums with variant parts...):
    /// shown as its raw bytes.
    Opaque { name: String, size: Option<u64> },
}

/// Where a variable lives.
#[derive(Debug, Clone)]
enum VarLocation {
    Expr(gimli::Expression<R>),
    List(Vec<(Range<u64>, gimli::Expression<R>)>),
}

#[derive(Debug, Clone)]
pub struct Variable {
    pub name: String,
    pub is_parameter: bool,
    pub type_ref: Option<TypeRef>,
    location: Option<VarLocation>,
    encoding: gimli::Encoding,
}

/// A lexical scope inside a function: the function body itself, a lexical
/// block, or an inlined call.
#[derive(Debug, Clone)]
pub struct Scope {
    pub ranges: Vec<Range<u64>>,
    pub variables: Vec<Variable>,
    pub children: Vec<Scope>,
    /// Set when this scope is an inlined call (a frame of its own).
    pub inlined: Option<InlinedCall>,
}

#[derive(Debug, Clone)]
pub struct InlinedCall {
    pub name: String,
    pub decl_file: Option<usize>,
    pub decl_line: u32,
    pub return_type: Option<TypeRef>,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub decl_file: Option<usize>,
    pub decl_line: u32,
    pub return_type: Option<TypeRef>,
    pub body: Scope,
    frame_base: Option<gimli::Expression<R>>,
    encoding: gimli::Encoding,
    /// Frame size (bytes) of the function, see the module docs.
    pub stack_size: u64,
}

impl Function {
    pub fn contains(&self, addr: u64) -> bool {
        self.body.ranges.iter().any(|r| r.contains(&addr))
    }

    /// The scopes enclosing `addr`, outermost first (the body excluded).
    pub fn scope_chain(&self, addr: u64) -> Vec<&Scope> {
        let mut chain = Vec::new();
        let mut current = &self.body;
        while let Some(child) = current
            .children
            .iter()
            .find(|c| c.ranges.iter().any(|r| r.contains(&addr)))
        {
            chain.push(child);
            current = child;
        }
        chain
    }
}

/// One row range of the line table.
#[derive(Debug, Clone)]
struct LineRow {
    start: u64,
    end: u64,
    position: Option<SourcePosition>,
}

/// Machine state a location expression is evaluated against.
pub struct MachineState<'a> {
    pub registers: &'a [u64; 12],
    pub read_memory: &'a dyn Fn(u64, usize) -> Option<Vec<u8>>,
    /// True when the program bumps `r10` itself (dynamic stack frames).
    pub manual_stack_frames: bool,
}

pub struct SourceDebugInfo {
    pub text_vaddr: u64,
    pub files: Vec<PathBuf>,
    pub functions: Vec<Function>,
    pub types: Vec<TypeDesc>,
    rows: Vec<LineRow>,
}

impl SourceDebugInfo {
    /// Load the debug model of the ELF at `elf_path` (whose bytes are
    /// `elf_data`).  `text` is the program's `.text` section bytes, used to
    /// recover each function's frame size.
    pub fn load(elf_data: &[u8], elf_path: &Path) -> Result<Self> {
        use object::{Object, ObjectSection};

        let owned: &'static [u8] = Vec::leak(elf_data.to_vec());
        let obj = object::File::parse(owned).map_err(|e| eyre!("failed to parse ELF: {e}"))?;
        let endian = if obj.is_little_endian() {
            RunTimeEndian::Little
        } else {
            RunTimeEndian::Big
        };
        let text = obj
            .section_by_name(".text")
            .ok_or_else(|| eyre!("ELF has no .text section"))?;
        let text_vaddr = text.address();
        let text_bytes = text.data().map_err(|e| eyre!("read .text: {e}"))?;

        let dwarf = gimli::Dwarf::load(|id| -> std::result::Result<R, gimli::Error> {
            let data = obj
                .section_by_name(id.name())
                .and_then(|s| s.uncompressed_data().ok())
                .unwrap_or(std::borrow::Cow::Borrowed(&[]));
            Ok(EndianSlice::new(&*Vec::leak(data.into_owned()), endian))
        })
        .map_err(|e| eyre!("failed to load DWARF sections: {e}"))?;

        let resolver = PathResolver::new(elf_path);
        let mut builder = Builder {
            dwarf: &dwarf,
            resolver: &resolver,
            files: Vec::new(),
            file_ids: HashMap::new(),
            types: Vec::new(),
            type_ids: HashMap::new(),
            functions: Vec::new(),
            rows: Vec::new(),
        };

        let mut units = dwarf.units();
        while let Some(header) = units.next().map_err(|e| eyre!("DWARF units: {e}"))? {
            let unit = dwarf.unit(header).map_err(|e| eyre!("DWARF unit: {e}"))?;
            builder
                .unit(&unit)
                .map_err(|e| eyre!("DWARF unit contents: {e}"))?;
        }

        let Builder {
            files,
            mut types,
            mut functions,
            mut rows,
            type_ids,
            ..
        } = builder;
        // Types were referenced by DWARF offset while building; now that
        // every type has an index, rewrite the references.
        for t in &mut types {
            t.remap(&type_ids);
        }
        for f in &mut functions {
            f.return_type = f.return_type.and_then(|o| type_ids.get(&o).copied());
            f.body.remap(&type_ids);
        }
        rows.sort_by_key(|r| r.start);

        let info = SourceDebugInfo {
            text_vaddr,
            files,
            functions,
            types,
            rows,
        };
        let mut info = info;
        let sizes: Vec<u64> = info
            .functions
            .iter()
            .map(|f| info.infer_stack_size(f, text_bytes))
            .collect();
        for (f, size) in info.functions.iter_mut().zip(sizes) {
            f.stack_size = size;
        }
        Ok(info)
    }

    /// ELF address of the instruction in slot `pc`.
    pub fn address_of(&self, pc: u64) -> u64 {
        self.text_vaddr
            .wrapping_add(pc.wrapping_mul(ebpf::INSN_SIZE as u64))
    }

    /// The source position of the instruction at `addr`, if the line table
    /// attributes one.
    pub fn position(&self, addr: u64) -> Option<&SourcePosition> {
        let idx = self.rows.partition_point(|r| r.start <= addr);
        let row = self.rows[..idx].iter().rev().find(|r| r.start <= addr)?;
        if addr < row.end {
            row.position.as_ref()
        } else {
            None
        }
    }

    /// The (out-of-line) function containing `addr`.
    pub fn function_at(&self, addr: u64) -> Option<usize> {
        self.functions.iter().position(|f| f.contains(addr))
    }

    /// Frame size of `function`, recovered from its code and DWARF.
    fn infer_stack_size(&self, function: &Function, text: &[u8]) -> u64 {
        let mut depth: u64 = 0;
        for range in &function.body.ranges {
            let first = range.start.saturating_sub(self.text_vaddr) / ebpf::INSN_SIZE as u64;
            let last = range.end.saturating_sub(self.text_vaddr) / ebpf::INSN_SIZE as u64;
            // Registers currently holding a copy of r10.
            let mut fp_copies = [false; 11];
            fp_copies[FRAME_POINTER as usize] = true;
            for pc in first..last {
                let Some(insn) = crate::sbf_memory::decode(text, pc) else {
                    break;
                };
                let class = insn.opc & 0x07;
                let (dst, src) = (insn.dst as usize, insn.src as usize);
                let is_fp = |r: usize| r == FRAME_POINTER as usize;
                // Loads/stores addressed off r10.
                let mem_base = match class {
                    ebpf::BPF_LDX => Some(src),
                    ebpf::BPF_ST | ebpf::BPF_STX => Some(dst),
                    _ => None,
                };
                if let Some(base) = mem_base
                    && is_fp(base)
                    && insn.off < 0
                {
                    depth = depth.max(insn.off.unsigned_abs() as u64);
                }
                if dst < fp_copies.len() && class != ebpf::BPF_ST && class != ebpf::BPF_STX {
                    if insn.opc == ebpf::MOV64_REG {
                        fp_copies[dst] = src < fp_copies.len() && fp_copies[src];
                    } else if insn.opc == ebpf::ADD64_IMM && fp_copies[dst] {
                        if insn.imm < 0 {
                            depth = depth.max(insn.imm.unsigned_abs());
                        }
                        if !is_fp(dst) {
                            fp_copies[dst] = false;
                        }
                    } else if class != ebpf::BPF_JMP64 && class != ebpf::BPF_JMP32 && !is_fp(dst) {
                        fp_copies[dst] = false;
                    }
                }
            }
        }
        // Every fbreg variable must fit in the frame.
        let mut extent: u64 = 0;
        function.body.visit_variables(&mut |v| {
            if let Some(VarLocation::Expr(expr)) = &v.location
                && let Some(off) = fbreg_offset(expr)
                && off >= 0
            {
                let size = v
                    .type_ref
                    .and_then(|t| self.type_size(t))
                    .unwrap_or(1)
                    .max(1);
                extent = extent.max(off as u64 + size);
            }
        });
        depth.max(extent).div_ceil(SBF_STACK_ALIGN) * SBF_STACK_ALIGN
    }

    /// Size in bytes of a value of type `t`.
    pub fn type_size(&self, t: TypeRef) -> Option<u64> {
        self.type_size_depth(t, 0)
    }

    fn type_size_depth(&self, t: TypeRef, depth: u32) -> Option<u64> {
        if depth > 32 {
            return None;
        }
        match self.types.get(t)? {
            TypeDesc::Base { size, .. } => Some(*size),
            TypeDesc::Pointer { .. } => Some(8),
            TypeDesc::Struct { size, .. } => Some(*size),
            TypeDesc::Array { element, count, .. } => {
                Some(self.type_size_depth((*element)?, depth + 1)? * (*count)?)
            }
            TypeDesc::Alias { target } => self.type_size_depth((*target)?, depth + 1),
            TypeDesc::Opaque { size, .. } => *size,
        }
    }

    /// Display name of type `t`.
    pub fn type_name(&self, t: TypeRef) -> String {
        match self.types.get(t) {
            Some(TypeDesc::Base { name, .. })
            | Some(TypeDesc::Pointer { name, .. })
            | Some(TypeDesc::Struct { name, .. })
            | Some(TypeDesc::Array { name, .. })
            | Some(TypeDesc::Opaque { name, .. }) => name.clone(),
            Some(TypeDesc::Alias { target: Some(t) }) => self.type_name(*t),
            _ => "?".to_string(),
        }
    }

    /// Evaluate `var`'s location at `addr` against `state` and return the
    /// bytes of its value (`size` bytes, the size of its type), or `None`
    /// when the debug info gives it no location there or the location
    /// cannot be read.
    pub fn read_variable(
        &self,
        function: &Function,
        var: &Variable,
        addr: u64,
        state: &MachineState<'_>,
    ) -> Option<Vec<u8>> {
        let expr = match var.location.as_ref()? {
            VarLocation::Expr(e) => *e,
            VarLocation::List(entries) => entries
                .iter()
                .find(|(r, _)| r.contains(&addr))
                .map(|(_, e)| *e)?,
        };
        let size = var.type_ref.and_then(|t| self.type_size(t))? as usize;
        let frame_base = self.frame_base(function, state);
        let pieces = evaluate(expr, var.encoding, state, frame_base)?;
        let mut out = Vec::with_capacity(size);
        let single = pieces.len() == 1;
        for piece in pieces {
            let piece_size = match piece.size_in_bits {
                Some(bits) => (bits / 8) as usize,
                None if single => size,
                None => return None,
            };
            let bytes = match piece.location {
                gimli::Location::Register { register } => {
                    let v = *state.registers.get(register.0 as usize)?;
                    v.to_le_bytes()[..piece_size.min(8)].to_vec()
                }
                gimli::Location::Address { address } => (state.read_memory)(address, piece_size)?,
                gimli::Location::Value { value } => {
                    let v = value.to_u64(u64::MAX).ok()?;
                    v.to_le_bytes()[..piece_size.min(8)].to_vec()
                }
                gimli::Location::Bytes { value } => value.to_slice().ok()?.to_vec(),
                _ => return None,
            };
            if bytes.len() < piece_size {
                return None;
            }
            out.extend_from_slice(&bytes[..piece_size]);
        }
        if out.len() < size {
            return None;
        }
        out.truncate(size);
        Some(out)
    }

    /// The value `DW_OP_fbreg` offsets are relative to, in `function` at
    /// the state `state` (see the module docs for the SBF adjustment).
    fn frame_base(&self, function: &Function, state: &MachineState<'_>) -> Option<u64> {
        let expr = function.frame_base?;
        let pieces = evaluate(expr, function.encoding, state, None)?;
        let piece = pieces.first()?;
        match piece.location {
            gimli::Location::Register { register } => {
                let value = *state.registers.get(register.0 as usize)?;
                if register.0 == FRAME_POINTER && !state.manual_stack_frames {
                    Some(value.wrapping_sub(function.stack_size))
                } else {
                    Some(value)
                }
            }
            gimli::Location::Address { address } => Some(address),
            gimli::Location::Value { value } => value.to_u64(u64::MAX).ok(),
            _ => None,
        }
    }
}

/// `Some(offset)` when `expr` is exactly `DW_OP_fbreg offset`.
fn fbreg_offset(expr: &gimli::Expression<R>) -> Option<i64> {
    let mut reader = expr.0;
    let op = reader.read_u8().ok()?;
    if gimli::DwOp(op) != gimli::DW_OP_fbreg {
        return None;
    }
    let off = reader.read_sleb128().ok()?;
    reader.is_empty().then_some(off)
}

fn evaluate(
    expr: gimli::Expression<R>,
    encoding: gimli::Encoding,
    state: &MachineState<'_>,
    frame_base: Option<u64>,
) -> Option<Vec<gimli::Piece<R>>> {
    let mut eval = expr.evaluation(encoding);
    let mut result = eval.evaluate().ok()?;
    loop {
        result = match result {
            gimli::EvaluationResult::Complete => break,
            gimli::EvaluationResult::RequiresRegister { register, .. } => {
                let v = *state.registers.get(register.0 as usize)?;
                eval.resume_with_register(gimli::Value::Generic(v)).ok()?
            }
            gimli::EvaluationResult::RequiresFrameBase => {
                eval.resume_with_frame_base(frame_base?).ok()?
            }
            gimli::EvaluationResult::RequiresMemory { address, size, .. } => {
                let bytes = (state.read_memory)(address, size as usize)?;
                let mut word = [0u8; 8];
                word[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
                eval.resume_with_memory(gimli::Value::Generic(u64::from_le_bytes(word)))
                    .ok()?
            }
            gimli::EvaluationResult::RequiresRelocatedAddress(a) => {
                eval.resume_with_relocated_address(a).ok()?
            }
            _ => return None,
        };
    }
    Some(eval.result())
}

// ---------------------------------------------------------------------------
// Building the model
// ---------------------------------------------------------------------------

struct Builder<'a> {
    dwarf: &'a gimli::Dwarf<R>,
    resolver: &'a PathResolver,
    files: Vec<PathBuf>,
    file_ids: HashMap<PathBuf, usize>,
    types: Vec<TypeDesc>,
    /// DWARF `.debug_info` offset of a type DIE -> index in `types`.
    type_ids: HashMap<usize, TypeRef>,
    functions: Vec<Function>,
    rows: Vec<LineRow>,
}

impl Builder<'_> {
    fn unit(&mut self, unit: &gimli::Unit<R>) -> gimli::Result<()> {
        let comp_dir = unit
            .comp_dir
            .map(|d| PathBuf::from(d.to_string_lossy().into_owned()));
        // Line-table file index -> resolved path id.
        let mut file_map: HashMap<u64, usize> = HashMap::new();
        if let Some(program) = unit.line_program.clone() {
            let header = program.header().clone();
            let mut rows = program.rows();
            let mut current: Option<(u64, Option<SourcePosition>)> = None;
            while let Some((_, row)) = rows.next_row()? {
                let addr = row.address();
                if let Some((start, position)) = current.take()
                    && addr > start
                {
                    self.rows.push(LineRow {
                        start,
                        end: addr,
                        position,
                    });
                }
                if row.end_sequence() {
                    continue;
                }
                let file = self.file_id(
                    unit,
                    &header,
                    row.file_index(),
                    comp_dir.as_deref(),
                    &mut file_map,
                );
                let position = match (file, row.line()) {
                    (Some(file), Some(line)) => Some(SourcePosition {
                        file,
                        line: line.get() as u32,
                        column: match row.column() {
                            gimli::ColumnType::LeftEdge => None,
                            gimli::ColumnType::Column(c) => Some(c.get() as u32),
                        },
                    }),
                    _ => None,
                };
                current = Some((addr, position));
            }
        }

        let mut tree = unit.entries_tree(None)?;
        let root = tree.root()?;
        let header = unit.line_program.as_ref().map(|p| p.header().clone());
        let ctx = UnitCtx {
            unit,
            header: header.as_ref(),
            comp_dir: comp_dir.as_deref(),
        };
        self.walk(&ctx, root, &mut file_map)?;
        Ok(())
    }

    fn file_id(
        &mut self,
        unit: &gimli::Unit<R>,
        header: &gimli::LineProgramHeader<R>,
        index: u64,
        comp_dir: Option<&Path>,
        file_map: &mut HashMap<u64, usize>,
    ) -> Option<usize> {
        if let Some(id) = file_map.get(&index) {
            return Some(*id);
        }
        let entry = header.file(index)?;
        let name = self
            .dwarf
            .attr_string(unit, entry.path_name())
            .ok()?
            .to_string_lossy()
            .into_owned();
        let dir = entry
            .directory(header)
            .and_then(|d| self.dwarf.attr_string(unit, d).ok())
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        let joined = Path::new(&dir).join(&name);
        let resolved = self.resolver.resolve(&joined, comp_dir);
        let id = match self.file_ids.get(&resolved) {
            Some(id) => *id,
            None => {
                self.files.push(resolved.clone());
                self.file_ids.insert(resolved, self.files.len() - 1);
                self.files.len() - 1
            }
        };
        file_map.insert(index, id);
        Some(id)
    }

    fn walk(
        &mut self,
        ctx: &UnitCtx<'_>,
        node: gimli::EntriesTreeNode<'_, '_, '_, R>,
        file_map: &mut HashMap<u64, usize>,
    ) -> gimli::Result<()> {
        let entry = node.entry().clone();
        match entry.tag() {
            gimli::DW_TAG_subprogram => {
                let ranges = self.ranges(ctx.unit, &entry)?;
                if !ranges.is_empty() {
                    let name = self
                        .name(ctx, &entry)
                        .unwrap_or_else(|| "<anonymous>".into());
                    let (decl_file, decl_line) = self.decl(ctx, &entry, file_map);
                    let return_type = self.type_attr(ctx, &entry);
                    let frame_base = match entry.attr_value(gimli::DW_AT_frame_base)? {
                        Some(AttributeValue::Exprloc(e)) => Some(e),
                        _ => None,
                    };
                    let mut body = Scope {
                        ranges,
                        variables: Vec::new(),
                        children: Vec::new(),
                        inlined: None,
                    };
                    self.scope_children(ctx, node, &mut body, file_map)?;
                    self.functions.push(Function {
                        name,
                        decl_file,
                        decl_line,
                        return_type,
                        body,
                        frame_base,
                        encoding: ctx.unit.encoding(),
                        stack_size: 0,
                    });
                    return Ok(());
                }
                // Declarations: their nested types still matter.
                self.children(ctx, node, file_map)
            }
            tag if is_type_tag(tag) => {
                let offset = die_offset(ctx.unit, &entry);
                let desc = self.type_desc(ctx, &entry, node)?;
                self.types.push(desc);
                self.type_ids.insert(offset, self.types.len() - 1);
                Ok(())
            }
            _ => self.children(ctx, node, file_map),
        }
    }

    fn children(
        &mut self,
        ctx: &UnitCtx<'_>,
        node: gimli::EntriesTreeNode<'_, '_, '_, R>,
        file_map: &mut HashMap<u64, usize>,
    ) -> gimli::Result<()> {
        let mut children = node.children();
        while let Some(child) = children.next()? {
            self.walk(ctx, child, file_map)?;
        }
        Ok(())
    }

    /// Collect the variables and nested scopes of a function body, lexical
    /// block or inlined call into `scope`.
    fn scope_children(
        &mut self,
        ctx: &UnitCtx<'_>,
        node: gimli::EntriesTreeNode<'_, '_, '_, R>,
        scope: &mut Scope,
        file_map: &mut HashMap<u64, usize>,
    ) -> gimli::Result<()> {
        let mut children = node.children();
        while let Some(child) = children.next()? {
            let entry = child.entry().clone();
            match entry.tag() {
                gimli::DW_TAG_variable | gimli::DW_TAG_formal_parameter => {
                    let Some(name) = self.name(ctx, &entry) else {
                        continue;
                    };
                    let location = match entry.attr_value(gimli::DW_AT_location)? {
                        Some(AttributeValue::Exprloc(e)) => Some(VarLocation::Expr(e)),
                        Some(v) => match self.dwarf.attr_locations(ctx.unit, v)? {
                            Some(mut iter) => {
                                let mut entries = Vec::new();
                                while let Some(e) = iter.next()? {
                                    entries.push((e.range.begin..e.range.end, e.data));
                                }
                                Some(VarLocation::List(entries))
                            }
                            None => None,
                        },
                        None => None,
                    };
                    scope.variables.push(Variable {
                        name,
                        is_parameter: entry.tag() == gimli::DW_TAG_formal_parameter,
                        type_ref: self.type_attr(ctx, &entry),
                        location,
                        encoding: ctx.unit.encoding(),
                    });
                }
                gimli::DW_TAG_lexical_block | gimli::DW_TAG_inlined_subroutine => {
                    let ranges = self.ranges(ctx.unit, &entry)?;
                    let inlined = if entry.tag() == gimli::DW_TAG_inlined_subroutine {
                        let (decl_file, decl_line) = self.decl(ctx, &entry, file_map);
                        Some(InlinedCall {
                            name: self.name(ctx, &entry).unwrap_or_else(|| "<inlined>".into()),
                            decl_file,
                            decl_line,
                            return_type: self.type_attr(ctx, &entry),
                        })
                    } else {
                        None
                    };
                    let mut inner = Scope {
                        ranges,
                        variables: Vec::new(),
                        children: Vec::new(),
                        inlined,
                    };
                    self.scope_children(ctx, child, &mut inner, file_map)?;
                    if inner.ranges.is_empty() {
                        // A block without code of its own: its variables
                        // belong to the enclosing scope's ranges.
                        scope.variables.extend(inner.variables);
                        scope.children.extend(inner.children);
                    } else {
                        scope.children.push(inner);
                    }
                }
                tag if is_type_tag(tag) => {
                    let offset = die_offset(ctx.unit, &entry);
                    let desc = self.type_desc(ctx, &entry, child)?;
                    self.types.push(desc);
                    self.type_ids.insert(offset, self.types.len() - 1);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn ranges(
        &self,
        unit: &gimli::Unit<R>,
        entry: &gimli::DebuggingInformationEntry<'_, '_, R>,
    ) -> gimli::Result<Vec<Range<u64>>> {
        let mut out = Vec::new();
        let mut iter = self.dwarf.die_ranges(unit, entry)?;
        while let Some(r) = iter.next()? {
            if r.end > r.begin {
                out.push(r.begin..r.end);
            }
        }
        Ok(out)
    }

    /// `attr` of `entry`, or of the DIE its `DW_AT_abstract_origin` /
    /// `DW_AT_specification` names (which carries the source-level
    /// description of inlined and out-of-line instances).
    fn attr(
        &self,
        ctx: &UnitCtx<'_>,
        entry: &gimli::DebuggingInformationEntry<'_, '_, R>,
        attr: gimli::DwAt,
    ) -> Option<AttributeValue<R>> {
        if let Ok(Some(v)) = entry.attr_value(attr) {
            return Some(v);
        }
        let mut next = origin_ref(entry);
        for _ in 0..4 {
            let e = ctx.unit.entry(next?).ok()?;
            if let Ok(Some(v)) = e.attr_value(attr) {
                return Some(v);
            }
            next = origin_ref(&e);
        }
        None
    }

    fn name(
        &self,
        ctx: &UnitCtx<'_>,
        entry: &gimli::DebuggingInformationEntry<'_, '_, R>,
    ) -> Option<String> {
        let v = self
            .attr(ctx, entry, gimli::DW_AT_name)
            .or_else(|| self.attr(ctx, entry, gimli::DW_AT_linkage_name))?;
        let s = self.dwarf.attr_string(ctx.unit, v).ok()?;
        Some(s.to_string_lossy().into_owned())
    }

    fn decl(
        &mut self,
        ctx: &UnitCtx<'_>,
        entry: &gimli::DebuggingInformationEntry<'_, '_, R>,
        file_map: &mut HashMap<u64, usize>,
    ) -> (Option<usize>, u32) {
        let line = self
            .attr(ctx, entry, gimli::DW_AT_decl_line)
            .and_then(|v| v.udata_value())
            .unwrap_or(0) as u32;
        let file = match (self.attr(ctx, entry, gimli::DW_AT_decl_file), ctx.header) {
            (Some(AttributeValue::FileIndex(i)), Some(header)) => {
                self.file_id(ctx.unit, header, i, ctx.comp_dir, file_map)
            }
            (Some(v), Some(header)) => v
                .udata_value()
                .and_then(|i| self.file_id(ctx.unit, header, i, ctx.comp_dir, file_map)),
            _ => None,
        };
        (file, line)
    }

    /// `DW_AT_type` of `entry`, as a `.debug_info` offset (remapped to a
    /// [`TypeRef`] once all types are known).
    fn type_attr(
        &self,
        ctx: &UnitCtx<'_>,
        entry: &gimli::DebuggingInformationEntry<'_, '_, R>,
    ) -> Option<usize> {
        match self.attr(ctx, entry, gimli::DW_AT_type)? {
            AttributeValue::UnitRef(o) => Some(unit_ref_offset(ctx.unit, o)),
            AttributeValue::DebugInfoRef(o) => Some(o.0),
            _ => None,
        }
    }

    fn type_desc(
        &mut self,
        ctx: &UnitCtx<'_>,
        entry: &gimli::DebuggingInformationEntry<'_, '_, R>,
        node: gimli::EntriesTreeNode<'_, '_, '_, R>,
    ) -> gimli::Result<TypeDesc> {
        let name = self.name(ctx, entry).unwrap_or_default();
        let size = entry
            .attr_value(gimli::DW_AT_byte_size)?
            .and_then(|v| v.udata_value());
        let target = self.type_attr(ctx, entry);
        Ok(match entry.tag() {
            gimli::DW_TAG_base_type => TypeDesc::Base {
                name,
                encoding: match entry.attr_value(gimli::DW_AT_encoding)? {
                    Some(AttributeValue::Encoding(e)) => e,
                    _ => gimli::DW_ATE_unsigned,
                },
                size: size.unwrap_or(0),
            },
            gimli::DW_TAG_pointer_type | gimli::DW_TAG_reference_type => {
                TypeDesc::Pointer { name, target }
            }
            gimli::DW_TAG_typedef | gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type => {
                TypeDesc::Alias { target }
            }
            gimli::DW_TAG_array_type => {
                let mut count = None;
                let mut children = node.children();
                while let Some(child) = children.next()? {
                    let e = child.entry();
                    if e.tag() == gimli::DW_TAG_subrange_type {
                        count = e
                            .attr_value(gimli::DW_AT_count)?
                            .and_then(|v| v.udata_value())
                            .or_else(|| {
                                e.attr_value(gimli::DW_AT_upper_bound)
                                    .ok()
                                    .flatten()
                                    .and_then(|v| v.udata_value())
                                    .map(|u| u + 1)
                            });
                        break;
                    }
                }
                TypeDesc::Array {
                    name,
                    element: target,
                    count,
                }
            }
            gimli::DW_TAG_structure_type => {
                let mut members = Vec::new();
                let mut has_variants = false;
                let mut children = node.children();
                while let Some(child) = children.next()? {
                    let e = child.entry().clone();
                    match e.tag() {
                        gimli::DW_TAG_member => {
                            let offset = e
                                .attr_value(gimli::DW_AT_data_member_location)?
                                .and_then(|v| v.udata_value())
                                .unwrap_or(0);
                            members.push((
                                self.name(ctx, &e).unwrap_or_default(),
                                offset,
                                self.type_attr(ctx, &e),
                            ));
                        }
                        gimli::DW_TAG_variant_part => has_variants = true,
                        _ => {}
                    }
                }
                if has_variants {
                    TypeDesc::Opaque { name, size }
                } else {
                    TypeDesc::Struct {
                        name,
                        size: size.unwrap_or(0),
                        members,
                    }
                }
            }
            _ => TypeDesc::Opaque { name, size },
        })
    }
}

struct UnitCtx<'a> {
    unit: &'a gimli::Unit<R>,
    header: Option<&'a gimli::LineProgramHeader<R>>,
    comp_dir: Option<&'a Path>,
}

fn is_type_tag(tag: gimli::DwTag) -> bool {
    matches!(
        tag,
        gimli::DW_TAG_base_type
            | gimli::DW_TAG_pointer_type
            | gimli::DW_TAG_reference_type
            | gimli::DW_TAG_typedef
            | gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_array_type
            | gimli::DW_TAG_structure_type
            | gimli::DW_TAG_union_type
            | gimli::DW_TAG_enumeration_type
            | gimli::DW_TAG_subroutine_type
    )
}

fn die_offset(unit: &gimli::Unit<R>, entry: &gimli::DebuggingInformationEntry<'_, '_, R>) -> usize {
    unit_ref_offset(unit, entry.offset())
}

fn unit_ref_offset(unit: &gimli::Unit<R>, offset: gimli::UnitOffset) -> usize {
    offset
        .to_debug_info_offset(&unit.header)
        .map(|o| o.0)
        .unwrap_or(offset.0)
}

fn origin_ref(entry: &gimli::DebuggingInformationEntry<'_, '_, R>) -> Option<gimli::UnitOffset> {
    for attr in [gimli::DW_AT_abstract_origin, gimli::DW_AT_specification] {
        if let Ok(Some(AttributeValue::UnitRef(o))) = entry.attr_value(attr) {
            return Some(o);
        }
    }
    None
}

impl TypeDesc {
    fn remap(&mut self, ids: &HashMap<usize, TypeRef>) {
        let fix = |t: &mut Option<TypeRef>| *t = t.and_then(|o| ids.get(&o).copied());
        match self {
            TypeDesc::Pointer { target, .. } | TypeDesc::Alias { target } => fix(target),
            TypeDesc::Array { element, .. } => fix(element),
            TypeDesc::Struct { members, .. } => {
                for (_, _, t) in members {
                    fix(t);
                }
            }
            TypeDesc::Base { .. } | TypeDesc::Opaque { .. } => {}
        }
    }
}

impl Scope {
    fn remap(&mut self, ids: &HashMap<usize, TypeRef>) {
        for v in &mut self.variables {
            v.type_ref = v.type_ref.and_then(|o| ids.get(&o).copied());
        }
        if let Some(call) = &mut self.inlined {
            call.return_type = call.return_type.and_then(|o| ids.get(&o).copied());
        }
        for c in &mut self.children {
            c.remap(ids);
        }
    }

    fn visit_variables(&self, f: &mut dyn FnMut(&Variable)) {
        for v in &self.variables {
            f(v);
        }
        for c in &self.children {
            if c.inlined.is_none() {
                c.visit_variables(f);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Source path resolution
// ---------------------------------------------------------------------------

/// Turns the paths the line table names into paths of files on this machine.
///
/// A line-table path is `directory/name`; a relative directory is relative
/// to the unit's `DW_AT_comp_dir`.  `cargo-build-sbf` compiles with
/// `-Zremap-cwd-prefix=`, which removes `DW_AT_comp_dir` and leaves the
/// crate's own files relative to the directory the compiler ran in (the
/// crate root, e.g. `src/lib.rs`).  That directory is then recovered from
/// the build's own record of the files it compiled: cargo's dep-info file
/// next to the ELF (`<name>.d`) lists every source file by absolute path.
/// Failing that, the relative path is tried against the ELF's enclosing
/// Cargo crate directories.
pub struct PathResolver {
    dep_info_sources: Vec<PathBuf>,
    crate_dirs: Vec<PathBuf>,
}

impl PathResolver {
    pub fn new(elf_path: &Path) -> Self {
        let dep_info = elf_path.with_extension("d");
        let dep_info_sources = if dep_info.exists() {
            parse_dep_info(&dep_info)
        } else {
            Vec::new()
        };
        let crate_dirs = elf_path
            .ancestors()
            .skip(1)
            .filter(|a| a.join("Cargo.toml").is_file())
            .map(Path::to_path_buf)
            .collect();
        Self {
            dep_info_sources,
            crate_dirs,
        }
    }

    pub fn resolve(&self, path: &Path, comp_dir: Option<&Path>) -> PathBuf {
        if path.is_absolute() {
            return path.to_path_buf();
        }
        if let Some(dir) = comp_dir.filter(|d| !d.as_os_str().is_empty()) {
            return dir.join(path);
        }
        let mut matches = self
            .dep_info_sources
            .iter()
            .filter(|src| src.ends_with(path));
        if let (Some(found), None) = (matches.next(), matches.next()) {
            return found.clone();
        }
        for dir in &self.crate_dirs {
            let candidate = dir.join(path);
            if candidate.is_file() {
                return candidate;
            }
        }
        path.to_path_buf()
    }
}

/// Absolute source paths listed in a Makefile-style dep-info file.
fn parse_dep_info(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((_, deps)) = line.split_once(": ") else {
            continue;
        };
        // Paths are space separated, with spaces inside a path escaped.
        let mut current = String::new();
        let mut chars = deps.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => {
                    current.push(' ');
                    chars.next();
                }
                ' ' => {
                    if !current.is_empty() {
                        out.push(PathBuf::from(std::mem::take(&mut current)));
                    }
                }
                _ => current.push(c),
            }
        }
        if !current.is_empty() {
            out.push(PathBuf::from(current));
        }
    }
    out.retain(|p| p.is_absolute());
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dep_info_resolves_a_cwd_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crate").join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "").unwrap();
        let elf = dir.path().join("out").join("prog.so");
        std::fs::create_dir_all(elf.parent().unwrap()).unwrap();
        std::fs::write(
            elf.with_extension("d"),
            format!(
                "{}: /abs/other.rs {}\n",
                elf.display(),
                src.join("lib.rs").display()
            ),
        )
        .unwrap();
        let resolver = PathResolver::new(&elf);
        assert_eq!(
            resolver.resolve(Path::new("src/lib.rs"), None),
            src.join("lib.rs")
        );
        assert_eq!(
            resolver.resolve(Path::new("src/lib.rs"), Some(Path::new("/build"))),
            PathBuf::from("/build/src/lib.rs"),
            "a recorded compilation directory wins"
        );
        assert_eq!(
            resolver.resolve(Path::new("/x/y.rs"), None),
            PathBuf::from("/x/y.rs")
        );
    }
}
