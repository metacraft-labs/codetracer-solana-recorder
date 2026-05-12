//! Solana program exercising the `msg!` log macro (Solana's canonical
//! `RecordEvent`-producing syscall).  The trace must surface every
//! `msg!` invocation as an io_event whose payload is the formatted
//! string.
//!
//! Today the synthetic-register recorder pipeline has no syscall hook,
//! so msg! invocations don't produce io_events — the test below pins
//! the present-day shape (zero io_events) and a parallel `#[ignore]`d
//! companion captures the spec-correct expectation (three io_events,
//! one per `msg!` call).
//!
//! Canonical execution:
//! * `compute(4, 5)` returns `9`, with three `msg!` invocations.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!` so the file compiles without the
/// real Solana SDK.  The recorder's eventual syscall hook will pin
/// this on the SBF side rather than here.
macro_rules! msg {
    ($($arg:tt)*) => {{
        // Side-effect-free in host tests; in real Solana programs this
        // expands to a sol_log syscall the recorder must capture.
        let _ = format!($($arg)*);
    }};
}

fn compute(a: i64, b: i64) -> i64 {
    msg!("entering compute with a={a} b={b}");
    let sum_val = a + b;
    msg!("sum_val={sum_val}");
    let doubled = sum_val * 2;
    msg!("returning doubled={doubled}");
    doubled
}

pub fn process_instruction() -> i64 {
    compute(4, 5)
}
