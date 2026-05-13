//! Solana program exercising `msg!` format-string interpolation — the
//! canonical debug-logging idiom in on-chain code.
//!
//! Real Solana programs use:
//!   msg!("balance: {}", balance);
//!   msg!("from {} to {}", src, dst);
//!   msg!("{:?}", account_data);
//!
//! Today the recorder's `extract_macro_string_arg` only captures the
//! literal format string — `msg!("balance: {}", balance)` produces an
//! io_event with text `balance: {}` (no substitution).  The strict
//! test below pins the present-day shape; an `#[ignore]`d sibling
//! captures the spec-correct expectation (substituted text) until the
//! format-arg interpolation path lands.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

fn compute(balance: i64, delta: i64) -> i64 {
    msg!("balance: {}", balance);
    let new_balance = balance + delta;
    msg!("from {} to {}", balance, new_balance);
    let doubled = new_balance * 2;
    msg!("doubled = {}", doubled);
    doubled
}

pub fn process_instruction() -> i64 {
    compute(100, 50)
}
