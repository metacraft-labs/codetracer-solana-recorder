//! Solana program exercising the canonical SPL Token CPI shape:
//!
//!   `let ix = spl_token::instruction::transfer(
//!        &token_program_id, &source, &destination, &authority, &[], amount);`
//!   `invoke(&ix, &accounts)?;`
//!
//! In real Solana the `transfer` builder lives in the `spl_token` crate
//! (a separate ELF) and the CPI dispatch crosses the SPL Token program-
//! id boundary.  The fixture's stand-ins compile without those crates
//! while preserving the source-level shape (function names, let-binding
//! structure, types) the recorder's source-driven model + CPI registry
//! pattern-match against.
//!
//! The strict pin runs through `record_with_cpi` with a registry that
//! places the SPL Token program at a sibling PC range so the CPI Call
//! event surfaces with the `target_program = "spl_token"` arg the
//! existing CPI-tagging machinery emits.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for `solana_program::pubkey::Pubkey`.
#[derive(Debug, Clone, Copy)]
pub struct Pubkey([u8; 32]);

impl Pubkey {
    pub const fn new_from_array(a: [u8; 32]) -> Self {
        Pubkey(a)
    }
    pub fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Stand-in for `solana_program::instruction::Instruction`.
#[derive(Debug)]
pub struct Instruction {
    pub program_id: Pubkey,
    pub accounts: Vec<u8>, // simplified
    pub data: Vec<u8>,
}

/// The SPL Token program's well-known program id.  In real Solana this
/// is `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`; the value is
/// irrelevant to the recorder's source-pattern matching, only the
/// construction site matters.
const SPL_TOKEN_PROGRAM_ID: Pubkey = Pubkey::new_from_array([6u8; 32]);

/// SPL Token `transfer` instruction builder.  In the real SDK this
/// lives in `spl_token::instruction::transfer`.  The discriminator byte
/// 3 is the canonical SPL Token Transfer instruction tag; the
/// trailing 8 bytes are the little-endian `amount` (one byte populated
/// here for the synthetic-snapshot pipeline).
fn transfer(
    token_program_id: &Pubkey,
    source: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    signer_pubkeys: &[&Pubkey],
    amount: u64,
) -> Instruction {
    let _ = source;
    let _ = destination;
    let _ = authority;
    let _ = signer_pubkeys;
    Instruction {
        program_id: *token_program_id,
        accounts: vec![0u8, 1u8, 2u8],
        // SPL Token Transfer discriminator = 3; followed by the
        // 8-byte little-endian amount (only the low byte populated).
        data: vec![3, (amount & 0xff) as u8, 0, 0, 0, 0, 0, 0, 0],
    }
}

/// Stand-in for `solana_program::program::invoke`.  Returns a
/// `Result<(), ()>` placeholder so the strict pin can drive it
/// through `record_with_cpi`'s CpiEvent::CpiCall path.
fn invoke(ix: &Instruction, accounts: &[u8]) -> Result<(), ()> {
    let _ = ix;
    let _ = accounts;
    Ok(())
}

/// Top-level entry point.  Each let-binding lives on its own source
/// line so the per-line step events are pin-pointable.
pub fn process_instruction(
    source: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
) -> Result<(), ()> {
    let amount: u64 = 1_000;
    let ix = transfer(
        &SPL_TOKEN_PROGRAM_ID,
        source,
        destination,
        authority,
        &[],
        amount,
    );
    invoke(&ix, &[0u8, 1u8, 2u8])?;
    Ok(())
}
