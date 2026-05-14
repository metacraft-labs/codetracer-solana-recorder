//! Solana program exercising iterator-adapter / closure idioms — the
//! `accounts.iter().filter(|a| a.is_signer).count()` and
//! `accounts.iter().map(|a| a.lamports()).sum()` shapes that every
//! native Solana handler uses for bulk per-account computation, plus
//! the canonical `for (i, account) in accounts.iter().enumerate()`
//! loop with per-iteration `msg!` logging.
//!
//! To keep the strict pin achievable without reaching into the SBF
//! VM, the iterator chains live in dedicated helper functions
//! (`count_signers` / `sum_lamports`) and the per-iteration logger is
//! its own `log_account` helper.  The recorder's existing call-frame
//! resolution handles each helper as a balanced Call/Return; the
//! source-driven `msg!` substituter resolves the helper's parameter
//! names against the snapshot register stream so each iteration's
//! io_event carries the substituted runtime values.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for `solana_program::account_info::AccountInfo`.
pub struct AccountInfo {
    pub is_signer: bool,
    pub lamports_value: u64,
}

impl AccountInfo {
    pub fn lamports(&self) -> u64 {
        self.lamports_value
    }
}

/// Iterator-adapter chain: `accounts.iter().filter(|a| a.is_signer).count()`.
/// The closure body `|a| a.is_signer` lives at the same source line as
/// the chain — the recorder's snapshot stream visits this line so the
/// step event surfaces the closure-bearing line.
pub fn count_signers(accounts: &[AccountInfo]) -> usize {
    accounts.iter().filter(|a| a.is_signer).count()
}

/// Iterator-adapter chain: `accounts.iter().map(|a| a.lamports()).sum()`.
/// Same structure as `count_signers` — the closure body
/// `|a| a.lamports()` shares a source line with the chain.
pub fn sum_lamports(accounts: &[AccountInfo]) -> u64 {
    accounts.iter().map(|a| a.lamports()).sum()
}

/// Per-iteration logger called from the `for ... enumerate()` loop in
/// `process_instruction`.  The two parameters (`i`, `account_lamports`)
/// are loaded into r1/r2 by the SBF calling convention so the
/// recorder's `substitute_format` resolves the `{}` placeholders to
/// the runtime register values for each iteration.
pub fn log_account(i: u64, account_lamports: u64) {
    msg!("account {}: {} lamports", i, account_lamports);
}

/// Top-level test driver — exercises the two iterator-adapter chains
/// and the `for ... enumerate()` loop with per-iteration logging.
pub fn process_instruction(accounts: &[AccountInfo]) -> u64 {
    let signer_count = count_signers(accounts);
    let total_lamports = sum_lamports(accounts);
    for (i, account) in accounts.iter().enumerate() {
        log_account(i as u64, account.lamports());
    }
    signer_count as u64 + total_lamports
}
