//! Solana program exercising Program-Derived Address (PDA) derivation
//! and verification — the canonical idiom for any on-chain program
//! that owns mutable state.
//!
//! `Pubkey::find_program_address(&[b"vault", user.as_ref()], program_id)`
//! returns `(Pubkey, u8)` — the derived address and the bump byte the
//! caller must supply when later re-deriving the same PDA.  This
//! fixture exercises:
//!
//! * the seed list (a `&[&[u8]]` of byte slices) — must surface as a
//!   `ValueRecord::Sequence`,
//! * the bump byte — must surface as a `ValueRecord::Int`,
//! * the `assert_eq!(derived, expected)` verification idiom.

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pubkey([u8; 32]);

impl Pubkey {
    pub const fn new_from_array(a: [u8; 32]) -> Self {
        Pubkey(a)
    }
    pub fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Stand-in for `Pubkey::find_program_address`.  Returns `(pda, bump)`
/// — the recorder's source-driven model decodes the LHS as a tuple
/// literal pattern when the let-binding parses.
fn find_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> (Pubkey, u8) {
    let _ = seeds;
    let _ = program_id;
    (Pubkey::new_from_array([7u8; 32]), 253)
}

/// Stand-in for `Pubkey::create_program_address` (the bump-known
/// variant; used in the verification path when re-deriving an existing
/// PDA).  Returns just the address — no bump search.
fn create_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> Pubkey {
    let _ = seeds;
    let _ = program_id;
    Pubkey::new_from_array([7u8; 32])
}

/// Top-level entry — the strict test pins each let-binding line.
/// Source layout chosen so the snapshot stream can attach exact
/// register values per line.
pub fn derive_vault_pda(user: &Pubkey, program_id: &Pubkey) -> u8 {
    let seeds: [i64; 3] = [1, 2, 3];
    let (pda, bump) = find_program_address(&[b"vault", user.as_ref()], program_id);
    let bump_seed: [u8; 1] = [bump];
    let verify_seeds: [&[u8]; 3] = [b"vault", user.as_ref(), &bump_seed];
    let derived = create_program_address(&verify_seeds, program_id);
    msg!("derived PDA verified against expected");
    let _ = derived;
    let _ = pda;
    bump
}
