//! Solana program exercising control-flow primitives the trace must
//! preserve: if/else branches, while loops, for loops, match arms,
//! early returns, and a `msg!` log inside a non-trivial branch.
//!
//! This file is kept as plain Rust (no `solana_program` import) so it
//! compiles without `cargo-build-sbf`; the recorder's tests synthesise
//! register snapshots whose source-line layout matches the lines below
//! and pin the resulting `ct-print --full` shape with strict counts.
//! The arithmetic chosen here is deterministic so the assertions can
//! cite exact register values for every step.
//!
//! Canonical execution (matching `tests/test_tracer.rs::
//! test_control_flow_test_via_ct_print_full`):
//!
//! * `classify(7)` returns `1` (positive branch).
//! * `pick_bonus(1)` returns `300` (match arm `1`).
//! * `accumulate(0, 5)` returns `0+1+2+3+4 = 10` via the `for` loop.
//! * `compute()` returns `(7 * 2) + 300 + 10 = 324`.

#![allow(dead_code)]

/// Classify a signed input as -1 / 0 / 1.  Exercises if / else if / else.
fn classify(raw: i64) -> i64 {
    if raw > 0 {
        1
    } else if raw < 0 {
        -1
    } else {
        0
    }
}

/// Pick a bonus based on the sign produced by `classify`.  Exercises
/// `match` with multiple literal arms plus a default.
fn pick_bonus(sign: i64) -> i64 {
    match sign {
        1 => 300,
        -1 => 100,
        0 => 0,
        _ => -1,
    }
}

/// Sum `start..(start+count)` using a `for` loop.  Exercises iteration
/// with a deterministic loop trip-count so the recorder must surface
/// every loop-body step.
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
    let acc = accumulate(0, 5);
    let combined = raw * 2 + bonus + acc;
    combined
}

/// Solana-style logging hook.  In the real program this would call
/// `solana_msg::msg!`; for trace fixtures the recorder synthesises the
/// equivalent RecordEvent.  Keeping the call here documents the source
/// line the test pins.
fn log_result(value: i64) {
    let _ = value;
    // RECORDER BUG: msg! should surface as a write event in the trace
    // (`io_events`), but the synthetic register-snapshot pipeline has
    // no syscall hook — the test below documents the gap.
}

pub fn process_instruction() -> i64 {
    let result = compute();
    log_result(result);
    result
}
