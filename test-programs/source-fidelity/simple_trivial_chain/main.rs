// simple_trivial_chain -- a Solana (sBPF) program recorded end-to-end.
//
// `compute` binds a=10; b=a; c=b and returns c.  The recorder's
// source-fidelity tests compile this file with cargo-build-sbf (inside a
// scratch cdylib crate that `include!`s it, exactly as CodeTracer's own
// test harness does), record the real program, and check the trace against
// the lines, locals and frames of this file.
//
// Line numbers matter: the tests name lines 13-22 below.  Keep this header
// exactly twelve lines long.
//
//
fn compute() -> u64 {
    let a: u64 = 10;
    let b: u64 = a;
    let c: u64 = b;
    c
}

fn main() {
    let _ = compute();
}
