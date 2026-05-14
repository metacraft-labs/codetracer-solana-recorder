//! Solana program exercising the binary-log + compute-units sysvar
//! syscalls every Solana program reaches for when it needs structured
//! event emission or BPF VM compute metering:
//!
//!   * `sol_log_data(&[b"event", &payload])` — Solana's binary log
//!     syscall, distinct from `msg!`.  The runtime tags the trace with
//!     a separate event kind so off-chain indexers can demux structured
//!     events from human-readable `msg!` output.
//!   * `sol_log_compute_units()` — emits the SBF VM's remaining-units
//!     counter for budget visibility.  In real Solana this is a
//!     zero-arg function; in this synthetic-snapshot fixture we expose
//!     the value via a single integer literal arg the recorder parses
//!     out (the SBF VM's actual counter isn't observable from the
//!     synthetic-register pipeline).
//!
//! The strict pin asserts that:
//!
//!   * `sol_log_data!(...)` surfaces with a different `io_kind` than
//!     `msg!(...)` (TraceLogEvent → ioStderr vs Write → ioStdout).
//!   * `sol_log_compute_units!(N)` surfaces as a metadata-style event
//!     whose payload contains the parsed `compute_units_remaining=<N>`
//!     integer.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for `solana_program::log::sol_log_data`.  The real
/// function takes `&[&[u8]]`; the macro form here just discards the
/// args at compile time so the file builds without the SDK.
macro_rules! sol_log_data {
    ($args:expr) => {{
        let _ = $args;
    }};
}

/// Stand-in for `solana_program::log::sol_log_compute_units`.  Real
/// Solana takes no args and reads the BPF VM's remaining-units
/// counter; here the fixture passes the canonical value via a single
/// integer literal so the recorder's source-pattern matcher can
/// surface a metadata event with the parsed integer.
macro_rules! sol_log_compute_units {
    ($remaining:literal) => {{
        let _ = $remaining;
    }};
}

/// Top-level test driver.  Each line lives on its own source line so
/// the per-line step events are pin-pointable, and the strict test
/// drives a snapshot stream that visits every line in declaration
/// order.
pub fn process_instruction() -> u64 {
    let payload: u64 = 42;
    msg!("about to log binary event");
    sol_log_data!(&[b"event", &payload]);
    sol_log_compute_units!(199500);
    payload
}
