//! Solana program exercising the three canonical pre-execution
//! validation checks every native handler runs:
//!
//! * `if !payer.is_signer { return Err(ProgramError::MissingRequiredSignature); }`
//! * `if account.owner != program_id { return Err(ProgramError::IllegalOwner); }`
//! * `if account.lamports() < 1000 { return Err(ProgramError::InsufficientFunds); }`
//!
//! Each check lives in its own helper function so the strict test can
//! drive a snapshot stream through all three call sites and pin each
//! error path independently.  After the recorder's
//! `synthesise_return_value` extension, every early-return surfaces as
//! a `register_return(Err(ProgramError::Variant))` carrying a typed
//! `ValueRecord::Variant` (not the legacy `NONE_VALUE` placeholder),
//! and each `return Err(..)` line emits an `EventLogKind::Error`
//! io_event whose payload contains the variant name.

#![allow(dead_code)]

/// Stand-in for `solana_program::program_error::ProgramError`.
#[derive(Debug)]
pub enum ProgramError {
    MissingRequiredSignature,
    IllegalOwner,
    InsufficientFunds,
}

/// Stand-in for `solana_program::account_info::AccountInfo`.  The
/// recorder's source-driven model only inspects field NAMES (not
/// runtime values), so this trimmed-down shape is sufficient for the
/// pin.
pub struct AccountInfo {
    pub is_signer: bool,
    pub owner: u64,
    lamports_value: u64,
}

impl AccountInfo {
    pub fn lamports(&self) -> u64 {
        self.lamports_value
    }
}

/// Signer check — returns `Err(ProgramError::MissingRequiredSignature)`
/// when `payer.is_signer` is false.  The strict test drives a snapshot
/// stream through this fn so the synthesiser emits the matching
/// io_event AND the typed Variant return value.
pub fn check_signer(payer: &AccountInfo) -> Result<(), ProgramError> {
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    Ok(())
}

/// Owner check — returns `Err(ProgramError::IllegalOwner)` when the
/// account doesn't belong to this program.
pub fn check_owner(account: &AccountInfo, program_id: u64) -> Result<(), ProgramError> {
    if account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(())
}

/// Lamports check — returns `Err(ProgramError::InsufficientFunds)` when
/// the account balance is below the minimum.
pub fn check_lamports(account: &AccountInfo) -> Result<(), ProgramError> {
    if account.lamports() < 1000 {
        return Err(ProgramError::InsufficientFunds);
    }
    Ok(())
}

/// Top-level test driver — calls each validation helper in turn so the
/// snapshot stream can cross three forward fn-boundaries (each
/// followed by a backward fn-boundary on the early-return) and pin all
/// three error io_events / Variant returns in one trace.
pub fn process_instruction(payer: &AccountInfo, account: &AccountInfo, program_id: u64) -> u64 {
    let _ = check_signer(payer);
    let _ = check_owner(account, program_id);
    let _ = check_lamports(account);
    0
}
