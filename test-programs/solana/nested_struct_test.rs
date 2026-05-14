//! Solana program exercising nested struct literals with mixed field
//! types — string literals, `Vec<T>` literals, nested struct literals,
//! and `Pubkey::default()` placeholders.  This is the parse_struct_literal
//! coverage gap M11 fixes: previously the recorder's source-driven
//! synthesiser only decoded int/bool fields and dropped the rest, leaving
//! complex `Outer { inner, items, owner }` literals as empty `Struct`
//! placeholders.
//!
//! After the recorder fix, the strict test pins:
//! * `inner` decodes recursively as a `Struct` carrying its own typed
//!   fields (`count: Int`, `label: String`),
//! * `items` decodes as a `Sequence` of `Int` elements,
//! * `owner` decodes as a `String` carrying the canonical
//!   `Pubkey::default()` base58 placeholder ("11111111111111111111111111111111").

#![allow(dead_code)]

/// Stand-in for `solana_msg::msg!` — the source-driven synthesiser
/// pattern-matches the `msg!(` token and emits the matching event.
macro_rules! msg {
    ($($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

/// Stand-in for `solana_program::pubkey::Pubkey`.
#[derive(Debug, Clone, Copy)]
pub struct Pubkey([u8; 32]);

impl Pubkey {
    pub const fn default() -> Self {
        Pubkey([0u8; 32])
    }
    pub const fn new_from_array(a: [u8; 32]) -> Self {
        Pubkey(a)
    }
}

/// Inner struct with mixed-type fields.  Both fields exercise the
/// extended `parse_struct_literal`: `count` is the legacy int path,
/// `label` is a string literal that previously got dropped.
#[derive(Debug)]
pub struct Inner {
    pub count: u64,
    pub label: &'static str,
}

/// Outer struct with a nested `Inner`, a `Vec<u64>` element list, and
/// a `Pubkey` field.  The recorder's extended `parse_struct_literal`
/// recursively decodes each.
#[derive(Debug)]
pub struct Outer {
    pub inner: Inner,
    pub items: Vec<u64>,
    pub owner: Pubkey,
}

/// Top-level entry the trace assertions hang off of.  The single
/// let-binding at line 67 is the literal-construction site the strict
/// test pins.
pub fn process_instruction() -> u64 {
    let outer = Outer {
        inner: Inner { count: 7, label: "hi" },
        items: vec![1, 2, 3],
        owner: Pubkey::default(),
    };
    msg!("{:?}", outer);
    outer.inner.count
}
