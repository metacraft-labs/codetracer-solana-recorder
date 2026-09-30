//! Guest-memory reconstruction for a recorded SBF execution.
//!
//! The VM's register trace records the registers before every executed
//! instruction, but not memory.  Memory is nevertheless fully determined by
//! the trace: the program starts from a known image (read-only data, a
//! zeroed stack and heap, the serialized input), and afterwards memory only
//! changes through
//!
//! * the program's own store instructions -- whose target address and
//!   stored value follow from the instruction and the registers it ran
//!   with, and
//! * syscalls, whose writes the recorder's syscall handlers journal
//!   ([`crate::syscalls::SyscallState::mem_writes`]).
//!
//! [`ShadowMemory`] replays both, one instruction at a time, so the memory
//! contents *before* instruction `i` are available exactly when the
//! registers before instruction `i` are.

use solana_sbpf::ebpf;

/// A replayable copy of the program's memory regions.
pub struct ShadowMemory {
    regions: Vec<(u64, Vec<u8>)>,
    moved_memory_instructions: bool,
}

/// What executing one instruction did to memory, as far as the shadow can
/// tell from the instruction itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionEffect {
    /// No memory write (arithmetic, loads, jumps, internal calls, exit).
    None,
    /// A store instruction; already applied.
    Store,
    /// A `call imm` that may have been a syscall.  Whether it was one is
    /// decided by the caller from where execution continued.
    MaybeSyscall,
}

impl ShadowMemory {
    pub fn new(initial: Vec<(u64, Vec<u8>)>, moved_memory_instructions: bool) -> Self {
        Self {
            regions: initial,
            moved_memory_instructions,
        }
    }

    /// Read `len` bytes at `addr`, if they lie inside one mapped region.
    pub fn read(&self, addr: u64, len: usize) -> Option<&[u8]> {
        self.regions.iter().find_map(|(start, bytes)| {
            let off = addr.checked_sub(*start)? as usize;
            bytes.get(off..off.checked_add(len)?)
        })
    }

    /// Write `data` at `addr`.  Writes outside the mapped regions are
    /// dropped: the VM would have faulted on them, so no later instruction
    /// observes them.
    pub fn write(&mut self, addr: u64, data: &[u8]) {
        for (start, bytes) in &mut self.regions {
            let Some(off) = addr.checked_sub(*start) else {
                continue;
            };
            let off = off as usize;
            if let Some(end) = off.checked_add(data.len())
                && end <= bytes.len()
            {
                bytes[off..end].copy_from_slice(data);
                return;
            }
        }
    }

    /// Apply the effect of the instruction at `pc` (an index into `text`)
    /// executed with registers `regs`.
    pub fn apply_instruction(
        &mut self,
        text: &[u8],
        pc: u64,
        regs: &[u64; 12],
    ) -> InstructionEffect {
        let Some(insn) = decode(text, pc) else {
            return InstructionEffect::None;
        };
        if insn.opc == ebpf::CALL_IMM {
            return InstructionEffect::MaybeSyscall;
        }
        let Some((size, from_register)) = self.store_kind(insn.opc) else {
            return InstructionEffect::None;
        };
        let addr = regs[insn.dst as usize].wrapping_add(insn.off as i64 as u64);
        let value = if from_register {
            regs[insn.src as usize]
        } else {
            insn.imm as u64
        };
        self.write(addr, &value.to_le_bytes()[..size]);
        InstructionEffect::Store
    }

    /// Store width in bytes, and whether the stored value comes from a
    /// register (as opposed to the immediate), for a store opcode.
    fn store_kind(&self, opc: u8) -> Option<(usize, bool)> {
        if self.moved_memory_instructions {
            match opc {
                ebpf::ST_1B_IMM => Some((1, false)),
                ebpf::ST_2B_IMM => Some((2, false)),
                ebpf::ST_4B_IMM => Some((4, false)),
                ebpf::ST_8B_IMM => Some((8, false)),
                ebpf::ST_1B_REG => Some((1, true)),
                ebpf::ST_2B_REG => Some((2, true)),
                ebpf::ST_4B_REG => Some((4, true)),
                ebpf::ST_8B_REG => Some((8, true)),
                _ => None,
            }
        } else {
            match opc {
                ebpf::ST_B_IMM => Some((1, false)),
                ebpf::ST_H_IMM => Some((2, false)),
                ebpf::ST_W_IMM => Some((4, false)),
                ebpf::ST_DW_IMM => Some((8, false)),
                ebpf::ST_B_REG => Some((1, true)),
                ebpf::ST_H_REG => Some((2, true)),
                ebpf::ST_W_REG => Some((4, true)),
                ebpf::ST_DW_REG => Some((8, true)),
                _ => None,
            }
        }
    }
}

/// Decode the instruction in slot `pc` of `text`.
pub fn decode(text: &[u8], pc: u64) -> Option<ebpf::Insn> {
    let start = (pc as usize).checked_mul(ebpf::INSN_SIZE)?;
    let end = start.checked_add(ebpf::INSN_SIZE)?;
    if end > text.len() {
        return None;
    }
    Some(ebpf::get_insn_unchecked(text, pc as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insn(opc: u8, dst: u8, src: u8, off: i16, imm: i32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0] = opc;
        b[1] = (src << 4) | dst;
        b[2..4].copy_from_slice(&off.to_le_bytes());
        b[4..8].copy_from_slice(&imm.to_le_bytes());
        b
    }

    #[test]
    fn replays_immediate_and_register_stores() {
        let stack = 0x2_0000_0000u64;
        let mut mem = ShadowMemory::new(vec![(stack, vec![0; 64])], false);
        let mut text = Vec::new();
        text.extend_from_slice(&insn(ebpf::ST_DW_IMM, 10, 0, -16, 10));
        text.extend_from_slice(&insn(ebpf::ST_W_REG, 10, 3, -4, 0));
        let mut regs = [0u64; 12];
        regs[10] = stack + 64;
        regs[3] = 0xdead_beef_1234_5678;
        assert_eq!(
            mem.apply_instruction(&text, 0, &regs),
            InstructionEffect::Store
        );
        assert_eq!(
            mem.apply_instruction(&text, 1, &regs),
            InstructionEffect::Store
        );
        assert_eq!(mem.read(stack + 48, 8).unwrap(), &10u64.to_le_bytes());
        assert_eq!(
            mem.read(stack + 60, 4).unwrap(),
            &0x1234_5678u32.to_le_bytes()
        );
    }

    #[test]
    fn reads_outside_any_region_are_refused() {
        let mem = ShadowMemory::new(vec![(0x1000, vec![0; 16])], false);
        assert!(mem.read(0x100c, 8).is_none());
        assert!(mem.read(0x0fff, 1).is_none());
        assert!(mem.read(0x1008, 8).is_some());
    }
}
