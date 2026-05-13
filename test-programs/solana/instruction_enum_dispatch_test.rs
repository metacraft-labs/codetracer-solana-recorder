//! Solana program exercising the canonical *instruction-enum dispatch*
//! shape that virtually every native (non-Anchor) on-chain program uses:
//!
//! 1. The program defines a `MyInstruction` enum whose variants carry
//!    typed payloads (`Init { lamports }`, `Update { delta }`, `Close`).
//! 2. `process_instruction` peels the discriminator byte off
//!    `instruction_data[0]` and constructs the matching variant.
//! 3. A `match` over the constructed variant dispatches to a per-variant
//!    handler function.
//!
//! Today the SBF recorder runs synthetic register snapshots (no real
//! SBF VM).  The strict test drives the `Init { lamports: 500 }` path
//! via a let-binding (`let init = MyInstruction::Init { lamports: 500 };`)
//! so the recorder's source-driven synthesiser can pattern-match the
//! `Type::Variant { .. }` literal on the visited line and emit a
//! `ValueRecord::Variant { discriminator, contents }` whose `contents`
//! is a `Struct` carrying the field values.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!` — the recorder's source-driven
/// synthesiser pattern-matches the `msg!(` token and emits the matching
/// `RecordEvent` directly.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Canonical native-program instruction enum.  Each variant carries its
/// typed payload — the dispatcher decodes the discriminator byte from
/// `instruction_data` and constructs the matching variant.
#[derive(Debug)]
pub enum MyInstruction {
    Init { lamports: u64 },
    Update { delta: i64 },
    Close,
}

/// Per-variant handlers — every real native program splits each
/// instruction-enum arm into its own function so the call trace shows
/// "what was dispatched".
fn handle_init(lamports: u64) -> i64 {
    let acc = lamports as i64;
    acc + 1
}

fn handle_update(delta: i64) -> i64 {
    delta + 100
}

fn handle_close() -> i64 {
    0
}

/// Top-level entry the trace assertions hang off of.  Each let-binding
/// occupies its own source line so the recorder's per-line step events
/// are pin-pointable.  The `init` binding is the variant-construction
/// site the strict test pins as `ValueRecord::Variant`.
pub fn process_instruction(instruction_data: &[u8]) -> i64 {
    let discriminator = instruction_data[0];
    let init = MyInstruction::Init { lamports: 500 };
    let result = handle_init(500);
    let _ = discriminator;
    let _ = init;
    result
}
