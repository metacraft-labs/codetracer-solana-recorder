//! Helpers over a fetched [`TransactionData`].
//!
//! This module used to hold a `replay_transaction` that answered `replay
//! --signature` with a SYNTHETIC trace: one zeroed register snapshot per
//! top-level instruction, against current account state, with placeholder
//! source lines. That is a complete-looking trace of an execution that never
//! happened — the failure mode Chain-Support-Matrix.md §3.5 says is the
//! default on Solana — so it is gone. `replay --signature` now runs the real
//! runtime through [`crate::onchain`] and states the divergence verdict.

use crate::rpc_client::TransactionData;

/// Extract the unique set of program IDs from a transaction's instructions.
pub fn extract_program_ids(tx: &TransactionData) -> Vec<String> {
    let mut ids: Vec<String> = tx
        .instructions
        .iter()
        .filter_map(|ix| tx.account_keys.get(ix.program_id_index as usize).cloned())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Extract all unique account keys referenced by the transaction.
pub fn extract_all_accounts(tx: &TransactionData) -> Vec<String> {
    tx.account_keys.clone()
}
