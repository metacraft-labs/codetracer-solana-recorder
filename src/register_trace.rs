//! Register trace parsing for SBF VM execution.
//!
//! This module will parse the register-level trace output from the
//! Solana SBF VM, providing per-instruction register state snapshots
//! for the recorder pipeline.

/// A snapshot of SBF register state at a single instruction.
#[derive(Debug, Clone)]
pub struct RegisterSnapshot {
    /// Program counter (instruction address).
    pub pc: u64,
    /// General-purpose register values (r0-r10).
    pub registers: [u64; 11],
}
