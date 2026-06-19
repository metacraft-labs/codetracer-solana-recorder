//! Solana program exercising control-flow primitives the trace must
//! preserve: if/else branches, while loops, for loops, match arms,
//! early returns, and a `msg!` log inside a non-trivial branch.
//!
//! This file is kept as plain Rust (no `solana_program` import) so it
//! compiles without `cargo-build-sbf`; the recorder's tests synthesise
//! register snapshots whose source-line layout matches the lines below.
//! The arithmetic chosen here is deterministic so the assertions can
//! cite exact register values for every step.
//!
//! Canonical execution:
//! * `classify(7)` returns `1` (positive branch).
//! * `pick_bonus(1)` returns `300` (match arm `1`).
//! * `accumulate(0, 5)` returns `0+1+2+3+4 = 10` via the `for` loop.
//! * `compute()` returns `(7 * 2) + 300 + 10 = 324`.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!` so the file compiles without the
/// real Solana SDK.  In the synthetic-snapshot recorder pipeline this
/// expands to the host-side `format!` (no syscall); the recorder's
/// source-driven synthesiser pattern-matches the `msg!(` token at
/// recording time and emits the matching `RecordEvent` directly.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Classify a signed input as -1 / 0 / 1.
fn classify(raw: i64) -> i64 {
    if raw > 0 {
        1
    } else if raw < 0 {
        -1
    } else {
        0
    }
}

/// Pick a bonus based on the sign produced by `classify`.
fn pick_bonus(sign: i64) -> i64 {
    match sign {
        1 => 300,
        -1 => 100,
        0 => 0,
        _ => -1,
    }
}

/// Sum `start..(start+count)` using a `while` loop.
fn accumulate(start: i64, count: i64) -> i64 {
    let mut total: i64 = 0;
    let mut i: i64 = 0;
    while i < count {
        total += start + i;
        i += 1;
    }
    total
}

/// Top-level entry the trace assertions hang off of.  Each let-binding
/// occupies its own source line so the recorder's per-line step events
/// are pin-pointable.
fn compute() -> i64 {
    let raw: i64 = 7;
    let sign = classify(raw);
    let bonus = pick_bonus(sign);
    let acc = { msg!("accumulating start=0 count=5"); accumulate(0, 5) };
    let combined = raw * 2 + bonus + acc;
    combined
}

/// Solana-style logging hook (kept reachable for future fixtures).
fn log_result(value: i64) {
    let _ = value;
}

pub fn process_instruction() -> i64 {
    let result = compute();
    log_result(result);
    result
}
