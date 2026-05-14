//! Solana program exercising the canonical account-data borrow +
//! instruction-data slice-indexing idioms every native handler uses
//! to parse on-chain bytes:
//!
//!   * `let data = account.try_borrow_data()?;` — borrow the
//!     `RefCell`-wrapped account data as `&[u8]`.
//!   * `let count = from_le_bytes_u64(&data);` — wrap a
//!     `u64::from_le_bytes(data[0..8].try_into().unwrap())` conversion
//!     into a helper so the recorder's existing call-frame resolution
//!     surfaces the conversion as a balanced Call/Return pair.
//!   * `let prefix = &instruction_data[..32];` and
//!     `let rest = &instruction_data[32..];` — the canonical
//!     "discriminator prefix + remainder" instruction-data split every
//!     native dispatcher uses.
//!
//! The strict pin asserts that:
//!
//!   * `data`, `prefix`, and `rest` surface as `ValueRecord::Sequence`
//!     with `is_slice == true` (NOT opaque `Raw`/`String` pointers).
//!   * The `from_le_bytes_u64` helper produces a Call/Return pair, and
//!     the parsed integer surfaces in the r0 register at the helper's
//!     return line.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for `solana_program::account_info::AccountInfo` carrying
/// a borrowable byte buffer.
pub struct AccountInfo {
    pub data: Vec<u8>,
}

impl AccountInfo {
    /// Stand-in for `try_borrow_data` returning `Result<&[u8], Err>`.
    /// In real Solana this returns a `Ref<&mut [u8]>`; the synthetic
    /// shape here is sufficient for the recorder's source-pattern
    /// matcher (which keys off the `.try_borrow_data()` substring).
    pub fn try_borrow_data(&self) -> Result<&[u8], ()> {
        Ok(&self.data)
    }
}

/// Helper that wraps `u64::from_le_bytes(data[0..8].try_into().unwrap())`
/// so the conversion surfaces in the trace as a balanced Call/Return
/// pair through the recorder's source-driven call-frame resolution.
pub fn from_le_bytes_u64(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[0..8].try_into().unwrap())
}

/// Top-level test driver.  Each let-binding lives on its own source
/// line so the per-line step events are pin-pointable.
pub fn process_instruction(account: &AccountInfo, instruction_data: &[u8]) -> u64 {
    let data = account.try_borrow_data().unwrap();
    let count = from_le_bytes_u64(&data);
    let prefix = &instruction_data[..32];
    let rest = &instruction_data[32..];
    count + prefix.len() as u64 + rest.len() as u64
}
