//! Solana program exercising the three Solana-flavoured error surfaces
//! the recorder must eventually capture:
//!
//! 1. `panic!` — program-terminating, no handler.
//! 2. `Result::Err` returned from a helper and propagated via `?`.
//! 3. `ProgramError` (a Solana-specific Result::Err variant).
//!
//! Today the SBF recorder runs synthetic register snapshots and has no
//! syscall hook, so panics / aborts are not surfaced as dedicated
//! `RecordEvent` (write kind = error).  The test below pins the
//! present-day "safe path" output and a parallel `#[ignore]`d
//! companion captures the spec-correct expectation that the failing
//! path emits at least one error io_event.

#![allow(dead_code)]

/// Solana-style `ProgramError` enum (mirrors a subset of the real
/// `solana_program_error::ProgramError`).
#[derive(Debug)]
pub enum ProgramError {
    InsufficientFunds,
    InvalidAccountData,
    Custom(u32),
}

/// Safe arithmetic — never panics, never returns Err.  Used by the
/// "safe path" execution covered by the strict-count test.
fn safe_add(a: i64, b: i64) -> i64 {
    a + b
}

fn safe_compute() -> i64 {
    let a: i64 = 5;
    let b: i64 = 7;
    let c = safe_add(a, b);
    let bumped = c + 100;
    bumped
}

/// Withdraw helper that returns `ProgramError::InsufficientFunds` when
/// the balance is below the requested amount.  Demonstrates a
/// program-error-typed `Result`.
fn withdraw(balance: u64, amount: u64) -> Result<u64, ProgramError> {
    if balance < amount {
        return Err(ProgramError::InsufficientFunds);
    }
    Ok(balance - amount)
}

/// Failing path that propagates a `ProgramError` via `?`.
fn failing_compute() -> Result<u64, ProgramError> {
    let new_balance = withdraw(50, 200)?;
    Ok(new_balance + 1)
}

/// Hard panic path.  In a real Solana program this would abort the
/// transaction; the recorder should emit an error RecordEvent before
/// the abort.
fn panicking_compute() -> i64 {
    let v: i64 = 0;
    if v == 0 {
        panic!("divide by zero in panicking_compute");
    }
    100 / v
}

/// The default entry point the strict-count test exercises is the safe
/// path; the failing helpers are kept reachable so static analysis and
/// future syscall-aware fixtures can call them.
pub fn process_instruction() -> i64 {
    let safe_val = safe_compute();
    let _ = withdraw(50, 25); // Ok branch
    let _ = failing_compute(); // Err branch (suppressed)
    safe_val
}
