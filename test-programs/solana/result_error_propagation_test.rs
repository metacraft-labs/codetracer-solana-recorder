//! Solana program exercising `Result<T, ProgramError>`-style error
//! propagation via the `?` operator — the canonical "fall through on
//! error" idiom every Solana handler uses to short-circuit on first
//! failure.
//!
//! After the recorder's `synthesise_return_value` extension and its
//! `?`-propagation slot, a `let v = helper(0)?;` line that observes a
//! callee returning `Err(..)` re-emits the SAME typed Variant from
//! the caller's `register_return` (mirrors Rust's `?` semantics: the
//! operator forwards the inner `Err`, it does NOT chain a new
//! wrapper).  The `msg!(..)` after `?` is NEVER emitted because
//! control left the function at the `?`.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for `solana_program::program_error::ProgramError`.
#[derive(Debug)]
pub enum ProgramError {
    Custom(u32),
}

/// Helper that returns `Err(ProgramError::Custom(42))` for the
/// canonical "input was zero" error path.  Each branch sits on its
/// own source line so the recorder's snapshot stream can drive the
/// `Err(..)` line independently of the `Ok(..)` line.
pub fn helper(x: u64) -> Result<u64, ProgramError> {
    if x == 0 {
        Err(ProgramError::Custom(42))
    } else {
        Ok(x * 2)
    }
}

/// Top-level test driver.  The strict test drives a snapshot stream
/// that visits `let v = helper(0)?;` and the `Err(..)` line inside
/// `helper`, then returns out of `process_instruction` via the
/// `?` operator's implicit early-return.
pub fn process_instruction() -> Result<u64, ProgramError> {
    let v = helper(0)?;
    msg!("got {}", v);
    Ok(v)
}
