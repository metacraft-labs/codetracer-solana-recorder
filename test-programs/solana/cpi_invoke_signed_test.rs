//! Solana program exercising a Cross-Program Invocation via
//! `invoke_signed`: a PDA-signed CPI that calls the System Program to
//! create an account.  Mirrors the canonical native-program flow at
//! https://docs.solana.com/developing/runtime-facilities/programs#system-program
//! and the `solana_program::program::invoke_signed` signature.
//!
//! The fixture below is a host-compilable stand-in: the System Program
//! types and the CPI helpers are minimal local copies, so this file
//! compiles without `solana_program` while preserving the source-level
//! shape (function names, let-binding structure, types) the recorder's
//! source-driven model pattern-matches against.
//!
//! M10 pinned that the recorder's CPI path (`record_with_cpi` in
//! `src/recorder.rs`) does NOT use the SourceModel — only the default
//! `record_from_snapshots_into_writer` path does.  This fixture
//! therefore drives `record_from_snapshots` (CPI as a regular nested
//! call) and the strict test pins the source-model-resolved call chain.
//! A sibling `#[ignore]`d test documents the CPI-path gap.
//!
//! Canonical execution:
//! * `create_account(payer, new_account, lamports=1_000_000, space=64)`
//!   constructs the System Program `CreateAccount` instruction
//!   (discriminator 0) with the PDA signer seeds.
//! * `invoke_signed(&ix, &accounts, &signer_seeds)` performs the CPI.
//! * `process_instruction` returns the post-CPI lamports = 1_000_000.

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

/// The System Program's well-known program id (all zeros in the real
/// chain — the value is irrelevant to the recorder's source-pattern
/// matching, only the construction site matters).
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);

/// System Program `CreateAccount` instruction builder.  In the real
/// SDK this lives in `solana_program::system_instruction::create_account`.
fn create_account(
    from: &Pubkey,
    to: &Pubkey,
    lamports: u64,
    space: u64,
) -> Instruction {
    let _ = from;
    let _ = to;
    let _ = space;
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![0u8, 1u8],
        // CreateAccount discriminator = 0; followed by the lamports +
        // space arguments.  The synthesiser doesn't decode the bytes —
        // it just sees the call site.
        data: vec![0u8, (lamports & 0xff) as u8],
    }
}

/// PDA-signed CPI dispatch — stand-in for
/// `solana_program::program::invoke_signed`.  Returns the CPI's
/// post-execution lamports so the strict test can assert on r0.
fn invoke_signed(
    ix: &Instruction,
    accounts: &[u8],
    signer_seeds: &[&[&[u8]]],
) -> u64 {
    let _ = ix;
    let _ = accounts;
    let _ = signer_seeds;
    1_000_000
}

/// Find the program-derived address for a `vault` PDA.  Stand-in for
/// `Pubkey::find_program_address`.  Returns `(pda, bump)`.
fn find_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> (Pubkey, u8) {
    let _ = seeds;
    let _ = program_id;
    (Pubkey::new_from_array([1u8; 32]), 254)
}

/// Top-level entry point.  The strict test drives a snapshot stream
/// through this function's body.  Each let-binding lives on its own
/// source line so the per-line step events are pin-pointable.
pub fn process_instruction(payer: &Pubkey, new_account: &Pubkey) -> u64 {
    let program_id = SYSTEM_PROGRAM_ID;
    let seeds: [&[u8]; 2] = [b"vault", payer.as_ref()];
    let (pda, bump) = find_program_address(&seeds, &program_id);
    let bump_seed: [u8; 1] = [bump];
    let signer_seeds: [&[&[u8]]; 1] = [&[b"vault", payer.as_ref(), &bump_seed]];
    let ix = create_account(payer, &pda, 1_000_000, 64);
    msg!("invoking system_program::create_account via PDA signer");
    let result = invoke_signed(&ix, &[0u8, 1u8, 2u8], &signer_seeds);
    let _ = new_account;
    result
}
