//! Solana program exercising the account-read / account-write pattern
//! that dominates real on-chain code.  An `AccountInfo`-shaped struct
//! holds a balance, owner key, and is_signer flag.  The instruction
//! handler reads the balance, applies a transfer of a constant amount,
//! and writes the new balance back.
//!
//! The recorder must eventually surface:
//! * the account struct as a `ValueRecord::Struct`,
//! * the read of `balance` as a step variable with the pre-transfer
//!   value,
//! * the write of `balance` as a step variable with the post-transfer
//!   value (so a stepping debugger can show before/after state).
//!
//! Today the SBF recorder runs synthetic register snapshots, so the
//! Rust struct here is informational; the test below pins the
//! present-day Int-only register stream and a parallel `#[ignore]`d
//! companion captures the spec-compliant Struct expectation.
//!
//! Canonical execution:
//! * Initial balance = 1000, transfer 250.
//! * `process_transfer` returns the new balance = 750.

#![allow(dead_code)]

#[derive(Debug)]
pub struct AccountInfo {
    pub balance: u64,
    pub owner: u64,
    pub is_signer: bool,
}

fn read_balance(account: &AccountInfo) -> u64 {
    account.balance
}

fn write_balance(account: &mut AccountInfo, new_balance: u64) {
    account.balance = new_balance;
}

fn debit(balance: u64, amount: u64) -> u64 {
    balance - amount
}

pub fn process_transfer() -> u64 {
    let mut account = AccountInfo {
        balance: 1000,
        owner: 42,
        is_signer: true,
    };
    let before = read_balance(&account);
    let after = debit(before, 250);
    write_balance(&mut account, after);
    account.balance
}
