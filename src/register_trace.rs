//! Register trace parsing for SBF VM execution.
//!
//! This module parses the register-level trace output from the
//! Solana SBF VM, providing per-instruction register state snapshots
//! for the recorder pipeline.
//!
//! The binary `.regs` format stores one row per instruction:
//! each row is 96 bytes = 12 × u64 (little-endian).
//! Register layout: [r0, r1, r2, r3, r4, r5, r6, r7, r8, r9, r10, r11]
//! where r11 = program counter, r0 = return value, r10 = frame pointer.

use eyre::{Result, ensure};

/// Number of registers per snapshot (r0–r11).
pub const NUM_REGISTERS: usize = 12;

/// Size of a single register snapshot in bytes (12 × 8).
pub const ROW_SIZE: usize = NUM_REGISTERS * 8;

/// A snapshot of all SBF register values at a single instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSnapshot {
    /// Raw register values r0–r11.
    pub registers: [u64; NUM_REGISTERS],
}

impl RegisterSnapshot {
    /// Program counter (r11).
    pub fn pc(&self) -> u64 {
        self.registers[11]
    }

    /// Return-value register (r0).
    pub fn r0(&self) -> u64 {
        self.registers[0]
    }

    /// Frame pointer / stack pointer (r10).
    pub fn frame_pointer(&self) -> u64 {
        self.registers[10]
    }

    /// Access a register by index (0–11).
    pub fn reg(&self, index: usize) -> u64 {
        self.registers[index]
    }
}

/// Parse a `.regs` binary blob into a vector of [`RegisterSnapshot`]s.
///
/// # Errors
///
/// Returns an error if the data length is not a multiple of [`ROW_SIZE`] (96).
pub fn parse_regs_file(data: &[u8]) -> Result<Vec<RegisterSnapshot>> {
    ensure!(
        data.len() % ROW_SIZE == 0,
        "regs data length {} is not a multiple of {ROW_SIZE}",
        data.len()
    );

    let count = data.len() / ROW_SIZE;
    let mut snapshots = Vec::with_capacity(count);

    for i in 0..count {
        let base = i * ROW_SIZE;
        let mut registers = [0u64; NUM_REGISTERS];
        for r in 0..NUM_REGISTERS {
            let offset = base + r * 8;
            registers[r] = u64::from_le_bytes(
                data[offset..offset + 8]
                    .try_into()
                    .expect("slice is exactly 8 bytes"),
            );
        }
        snapshots.push(RegisterSnapshot { registers });
    }

    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty() {
        let snaps = parse_regs_file(&[]).unwrap();
        assert!(snaps.is_empty());
    }

    #[test]
    fn parse_single_row() {
        let mut data = vec![0u8; ROW_SIZE];
        // Set r0 = 42, r11 (PC) = 7
        data[0..8].copy_from_slice(&42u64.to_le_bytes());
        data[88..96].copy_from_slice(&7u64.to_le_bytes());

        let snaps = parse_regs_file(&data).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].r0(), 42);
        assert_eq!(snaps[0].pc(), 7);
    }

    #[test]
    fn parse_bad_length() {
        let data = vec![0u8; 50]; // not a multiple of 96
        assert!(parse_regs_file(&data).is_err());
    }
}
