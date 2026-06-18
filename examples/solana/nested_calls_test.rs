//! Solana program exercising a four-deep call chain
//! `compute → outer → middle → inner` so the recorder's call/return
//! ordering can be asserted strictly (LIFO returns, sequential
//! call_entry order).  The arithmetic is deterministic:
//!
//! * `inner()` returns `1 + 2 = 3`.
//! * `middle()` calls `inner()` and returns `3 + 10 = 13`.
//! * `outer()` calls `middle()` and returns `13 + 100 = 113`.
//! * `compute()` calls `outer()` and returns `113`.
//!
//! Mirrors the Cardano `nested_calls_test.ak` fixture so cross-recorder
//! regressions in nested-call ordering are immediately visible.

#![allow(dead_code)]

fn inner() -> i64 {
    let a: i64 = 1;
    let b: i64 = 2;
    let c = a + b;
    c
}

fn middle() -> i64 {
    let x = inner();
    let y = x + 10;
    y
}

fn outer() -> i64 {
    let p = middle();
    let q = p + 100;
    q
}

pub fn compute() -> i64 {
    let result = outer();
    result
}
