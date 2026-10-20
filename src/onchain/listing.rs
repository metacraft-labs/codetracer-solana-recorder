//! A disassembly listing of a loaded SBF executable, and the facts about each
//! program counter the trace writer needs.
//!
//! Deployed Solana programs are almost never shipped with DWARF, so the
//! position a step can honestly point at is the instruction itself. The
//! listing is that position space: one line per instruction, a label line at
//! every function the executable's own function registry names, and a map
//! from program counter to listing line. Nothing here is inferred from source
//! text; every name comes from the executable or the loader.

use std::collections::{BTreeMap, HashMap};

use solana_program_runtime::invoke_context::Executable;
use solana_program_runtime::solana_sbpf::{disassembler, ebpf, static_analysis::CfgNode};

/// What an instruction does to control flow, as far as the trace writer cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    Plain,
    /// `call imm` / `callx`: a call into this program's own code when the next
    /// executed instruction is not the following one, a syscall otherwise.
    Call {
        syscall: Option<String>,
    },
    /// `exit`: returns to the caller frame, or ends the invocation.
    Exit,
}

pub struct Listing {
    /// The listing text, one entry per line (1-based line N is `lines[N-1]`).
    pub lines: Vec<String>,
    /// Program counter (in instruction slots) -> 1-based listing line.
    pub pc_line: Vec<u32>,
    /// Program counter -> control-flow class.
    pub flow: HashMap<usize, Flow>,
    /// Function entry program counters, with the registry's name for each.
    pub functions: BTreeMap<usize, String>,
    /// The instruction set revision the executable was loaded as (`v0`..`v3`).
    pub sbpf_version: String,
}

impl Listing {
    pub fn from_executable(executable: &Executable) -> Self {
        let (_vaddr, text) = executable.get_text_bytes();
        let registry = executable.get_function_registry();
        let loader = executable.get_loader();
        let version = executable.get_sbpf_version();
        let no_cfg: BTreeMap<usize, CfgNode> = BTreeMap::new();

        let mut functions = BTreeMap::new();
        for (_key, (name, pc)) in registry.iter() {
            functions.insert(pc, String::from_utf8_lossy(name).to_string());
        }

        let slots = text.len() / ebpf::INSN_SIZE;
        let mut lines = Vec::with_capacity(slots + functions.len() + 1);
        let mut pc_line = vec![0u32; slots + 1];
        let mut flow = HashMap::new();

        let mut ptr = 0usize;
        while ptr < slots {
            let mut insn = ebpf::get_insn_unchecked(text, ptr);
            let width = if insn.opc == ebpf::LD_DW_IMM && ptr + 1 < slots {
                ebpf::augment_lddw_unchecked(text, &mut insn);
                2
            } else {
                1
            };
            if let Some(name) = functions.get(&ptr) {
                lines.push(format!("{name}:"));
            }
            let mut text_line = disassembler::disassemble_instruction(
                &insn, ptr, &no_cfg, registry, loader, version,
            );
            if text_line.contains("[invalid]") {
                // Jumps name their target through CFG labels, which this
                // listing does not build. Print the target program counter.
                let target = ptr as isize + insn.off as isize + 1;
                text_line = text_line.replace("[invalid]", &format!("pc {target}"));
            }
            lines.push(format!("{ptr:>7}  {text_line}"));
            let line_no = lines.len() as u32;
            for slot in ptr..ptr + width {
                pc_line[slot] = line_no;
            }
            match insn.opc {
                ebpf::CALL_IMM => {
                    let syscall = loader
                        .get_function_registry()
                        .lookup_by_key(insn.imm as u32)
                        .map(|(name, _)| String::from_utf8_lossy(name).to_string());
                    flow.insert(ptr, Flow::Call { syscall });
                }
                ebpf::CALL_REG => {
                    flow.insert(ptr, Flow::Call { syscall: None });
                }
                ebpf::EXIT => {
                    flow.insert(ptr, Flow::Exit);
                }
                _ => {}
            }
            ptr += width;
        }

        Self {
            lines,
            pc_line,
            flow,
            functions,
            sbpf_version: format!("{version:?}"),
        }
    }

    pub fn line_of(&self, pc: usize) -> u32 {
        self.pc_line
            .get(pc)
            .copied()
            .filter(|l| *l > 0)
            .unwrap_or(1)
    }

    pub fn flow_of(&self, pc: usize) -> &Flow {
        self.flow.get(&pc).unwrap_or(&Flow::Plain)
    }

    /// The registry's name for the function starting at `pc`, or a name made
    /// from the program counter when the registry has none.
    pub fn function_name(&self, pc: usize) -> String {
        self.functions
            .get(&pc)
            .cloned()
            .unwrap_or_else(|| format!("function_{pc}"))
    }

    pub fn text(&self) -> String {
        let mut s = self.lines.join("\n");
        s.push('\n');
        s
    }
}
