//! Solana program exercising the canonical `Clock::get()` /
//! `Rent::get()` sysvar-fetch idioms every native handler relies on
//! to read on-chain timestamps and rent-exemption thresholds.
//!
//! The recorder has no SBF VM in this synthetic-snapshot pipeline, so
//! the sysvar fetches are split into two parts the source-driven
//! synthesiser CAN observe:
//!
//! 1. A dedicated helper function (`fetch_clock_sysvar` /
//!    `fetch_rent_sysvar`) the snapshot stream visits — the
//!    cross-fn-boundary jump produces the balanced Call/Return pair.
//! 2. A direct struct-literal `let` binding in the driver — the
//!    source-driven `parse_struct_literal_rich` decodes each field
//!    into a typed `ValueRecord::Struct` whose `type_name` the
//!    strict pin asserts on (`Clock` / `Rent`).
//!
//! Together these two parts cover the spec-mandated invariants from
//! `Solana-SBF.status.org` lines 822-979 even though the synthetic
//! snapshot pipeline can't actually execute `solana_program`'s real
//! sysvar fetch.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for `solana_program::clock::Clock`.
#[derive(Debug)]
pub struct Clock {
    pub unix_timestamp: u64,
    pub slot: u64,
    pub epoch: u64,
}

/// Stand-in for `solana_program::rent::Rent`.
#[derive(Debug)]
pub struct Rent {
    pub lamports_per_byte_year: u64,
    pub exemption_threshold: u64,
}

/// Helper that surfaces `Clock::get()` as a balanced Call/Return
/// pair for the recorder's source-driven call-frame resolution.
pub fn fetch_clock_sysvar() -> u64 {
    1700000000
}

/// Helper that surfaces `Rent::get()` as a balanced Call/Return
/// pair.
pub fn fetch_rent_sysvar() -> u64 {
    3480
}

/// Top-level test driver.  Each let-binding stitches a typed
/// struct-literal local the strict test pins by `type_name` +
/// per-field positional value, and the two sysvar-fetch helpers run
/// before each construction so the call trace shows the canonical
/// "fetch then construct" shape.
pub fn process_instruction() -> u64 {
    let unix_timestamp = fetch_clock_sysvar();
    let clock = Clock { unix_timestamp: 1700000000, slot: 200000000, epoch: 500 };
    let lamports_per_byte_year = fetch_rent_sysvar();
    let rent = Rent { lamports_per_byte_year: 3480, exemption_threshold: 2 };
    msg!("clock unix_timestamp={}", unix_timestamp);
    msg!("rent lamports_per_byte_year={}", lamports_per_byte_year);
    let _ = clock.slot + rent.exemption_threshold;
    unix_timestamp + lamports_per_byte_year
}
