//! Solana program exercising the canonical top-of-`lib.rs` macro
//! invocations every native Solana program ships with:
//!
//!   * `solana_program::declare_id!("11111111111111111111111111111112")`
//!   * `entrypoint!(process_instruction)`
//!
//! Neither macro performs any logging — they synthesise a constant
//! `id()` accessor and the SBF entry-point shim.  The recorder's
//! source-driven `synthesise_step_events` recogniser MUST stay silent
//! on these (no spurious `SolanaMsg`/`SolanaPanic`/`SolanaError`
//! io_events): the only legal io_events for this fixture are the ones
//! emitted by an explicit `msg!(...)` call inside `process_instruction`.
//!
//! Today the matcher anchors on the literal substrings `"msg!("` and
//! `"panic!("` (plus `Err(`-anchored variants), so a bare line
//! containing `declare_id!("..")` or `entrypoint!(process_instruction)`
//! cannot false-match into a SolanaMsg / SolanaPanic event.  This
//! fixture provides a regression test for that property: any future
//! widening of the matcher to "any `name!(` invocation" must keep
//! these lines silent.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for `solana_program::declare_id!`.  Synthesises a
/// `id()` accessor that returns the embedded base58 string.  The
/// real macro returns a `Pubkey` constant; we don't need the typed
/// value for the recorder's source-pattern test.
macro_rules! declare_id {
    ($id:literal) => {
        pub const fn id() -> &'static str {
            $id
        }
    };
}

/// Stand-in for `solana_program::entrypoint!`.  The real macro
/// expands into the SBF entry-point glue that calls the named
/// handler — we don't need that wiring here.
macro_rules! entrypoint {
    ($handler:ident) => {
        // No-op placeholder — the recorder's source-pattern matcher
        // must not synthesise an io_event from this line.
        const _: fn() = || {
            let _ = stringify!($handler);
        };
    };
}

declare_id!("11111111111111111111111111111112");

entrypoint!(process_instruction);

/// The handler `entrypoint!` references.  The strict test drives a
/// snapshot stream that visits the handler's body.  The single
/// `msg!(...)` line is the ONLY io_event the trace must contain;
/// the `declare_id!` and `entrypoint!` lines above stay silent.
pub fn process_instruction() -> u64 {
    let value: u64 = 7;
    msg!("processing instruction with value={}", value);
    value
}
