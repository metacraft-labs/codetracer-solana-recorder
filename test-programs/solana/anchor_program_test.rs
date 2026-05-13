//! Solana program exercising the Anchor framework's macro-generated
//! dispatch surface — the dominant modern Solana dev framework.
//!
//! Real Anchor programs use:
//! * `#[program] mod my_program { pub fn initialize(ctx, ..) -> Result<...> }`
//!   to declare instruction handlers.
//! * `#[derive(Accounts)] pub struct Initialize<'info> { .. }` to declare
//!   the account context the handler unpacks.
//! * `#[account] pub struct MyState { .. }` to declare on-chain account
//!   types.
//!
//! At compile time Anchor expands these into a discriminator-prefixed
//! enum and a `__handler` shim that routes each instruction to the
//! user's handler.  The recorder's source-driven model parses the
//! original `pub fn initialize(..)` declaration (not the macro-expanded
//! shim) so the call trace surfaces the user-written handler name.
//!
//! Today the recorder's `SourceModel` only sees the raw source text;
//! it doesn't expand procedural macros.  The `#[program]`,
//! `#[derive(Accounts)]`, and `#[account]` attributes are therefore
//! pure documentation from the recorder's perspective — the `fn`
//! declarations inside the `#[program] mod my_program` block still
//! parse normally and surface as call frames.

#![allow(dead_code)]
#![allow(unused_attributes)]

/// Stand-in for `solana_msg::msg!`.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for Anchor's procedural attribute macros — they expand to
/// nothing at host-compile time so the file compiles without the real
/// Anchor crate, but the source layout the recorder sees is identical.
macro_rules! program {
    ($($x:tt)*) => { $($x)* };
}

#[derive(Debug)]
pub struct Pubkey([u8; 32]);

/// `#[account]`-style on-chain account state.
#[derive(Debug)]
pub struct MyState {
    pub authority: u64,
    pub counter: u64,
    pub is_initialized: bool,
}

/// `#[derive(Accounts)]`-style account context.  Anchor unpacks the
/// instruction's account list into this struct at the start of the
/// handler — the recorder's source-driven model sees the struct
/// literal at the call site and surfaces it as a typed `Struct`.
#[derive(Debug)]
pub struct Initialize {
    pub authority: u64,
    pub bump: u64,
    pub rent_exempt: bool,
}

/// Anchor handler arguments — the `Context<T>` wrapper bundles the
/// program id, accounts, and remaining-accounts vector.  In the
/// expanded form Anchor passes a borrowed `Context`; here we model the
/// shape with a flat struct so the recorder's `parse_struct_literal`
/// can decode it.
#[derive(Debug)]
pub struct Context {
    pub program_id: u64,
    pub bump: u64,
    pub signer: u64,
}

/// `#[program] mod my_program { pub fn initialize(..) -> Result<..> }`.
/// The expanded form wraps this in a `__handler` shim — but the
/// recorder sees the source-level `pub fn initialize` declaration and
/// surfaces it (not the shim) as the call-trace entry.
pub fn initialize(ctx: Context, seed: i64) -> i64 {
    msg!("initialize handler entered");
    let authority = ctx.program_id as i64;
    let bumped = authority + seed;
    bumped
}

/// A second handler so the call trace has more than one resolved
/// function-table entry — exercises the multi-handler shape every
/// real Anchor program has.
pub fn update_counter(ctx: Context, delta: i64) -> i64 {
    let prev = ctx.signer as i64;
    let next = prev + delta;
    next
}

/// Top-level entry the strict test drives a snapshot stream against.
/// Constructs a `Context` struct literal (so the typed `Struct` decode
/// can be pinned) and calls the `initialize` handler.
pub fn process_instruction() -> i64 {
    let ctx = Context {
        program_id: 1,
        bump: 254,
        signer: 7,
    };
    let result = initialize(ctx, 5);
    result
}
