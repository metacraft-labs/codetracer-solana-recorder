//! Tests for the RPC client parsing and replay pipeline.
//!
//! These tests use static JSON fixtures rather than a live RPC endpoint,
//! exercising the parsing and extraction logic in isolation.

use codetracer_solana_recorder::rpc_client::{
    parse_account_response, parse_transaction_response, rpc_error_response, rpc_response_from_value,
};
use codetracer_solana_recorder::replay::{extract_all_accounts, extract_program_ids};

// ---------------------------------------------------------------------------
// JSON fixtures
// ---------------------------------------------------------------------------

/// Realistic `getTransaction` result (trimmed to the fields we use).
fn sample_get_transaction_result() -> serde_json::Value {
    serde_json::json!({
        "slot": 123_456_789,
        "transaction": {
            "message": {
                "accountKeys": [
                    "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
                    "11111111111111111111111111111111",
                    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                    "SysvarRent111111111111111111111111111111111"
                ],
                "instructions": [
                    {
                        "programIdIndex": 2,
                        "accounts": [0, 1, 3],
                        "data": "3Bxs4ThwQbE4vyj5"
                    },
                    {
                        "programIdIndex": 1,
                        "accounts": [0],
                        "data": "2ZpSih36go9D"
                    }
                ]
            },
            "signatures": [
                "5VERv8NMhJhskKBcUEjGji4hYP3vR8p4FXGs6iCdMHc6KrLCjg4eV5nzEhXfuj6N1LhgC4Au3EY5BmqJkx4F2z8E"
            ]
        },
        "meta": {
            "err": null,
            "fee": 5000,
            "preBalances": [100000000, 0, 1000000, 100000],
            "postBalances": [99995000, 0, 1000000, 100000]
        }
    })
}

/// Transaction result where the transaction failed on-chain.
fn sample_failed_transaction_result() -> serde_json::Value {
    serde_json::json!({
        "slot": 999_999,
        "transaction": {
            "message": {
                "accountKeys": [
                    "Sender111111111111111111111111111111111111",
                    "Program111111111111111111111111111111111111"
                ],
                "instructions": [
                    {
                        "programIdIndex": 1,
                        "accounts": [0],
                        "data": "AQAAAA=="
                    }
                ]
            },
            "signatures": ["failedSig"]
        },
        "meta": {
            "err": {
                "InstructionError": [0, "InvalidAccountData"]
            }
        }
    })
}

/// Realistic `getAccountInfo` result.
fn sample_get_account_info_result() -> serde_json::Value {
    // "SGVsbG8gU29sYW5hIQ==" is base64 for "Hello Solana!"
    serde_json::json!({
        "context": { "slot": 123_456_789 },
        "value": {
            "lamports": 1_000_000_000,
            "owner": "BPFLoaderUpgradeab1e11111111111111111111111",
            "executable": true,
            "rentEpoch": 361,
            "data": ["SGVsbG8gU29sYW5hIQ==", "base64"]
        }
    })
}

/// `getAccountInfo` result for a non-existent account.
fn sample_null_account_result() -> serde_json::Value {
    serde_json::json!({
        "context": { "slot": 100 },
        "value": null
    })
}

/// Transaction with versioned (object-style) account keys.
fn sample_versioned_transaction_result() -> serde_json::Value {
    serde_json::json!({
        "slot": 200_000_000,
        "transaction": {
            "message": {
                "accountKeys": [
                    { "pubkey": "Abc111111111111111111111111111111111111111", "signer": true, "writable": true },
                    { "pubkey": "Def222222222222222222222222222222222222222", "signer": false, "writable": false }
                ],
                "instructions": [
                    {
                        "programIdIndex": 1,
                        "accounts": [0],
                        "data": "test_data"
                    }
                ]
            },
            "signatures": ["versionedSig"]
        },
        "meta": { "err": null }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify that a realistic `getTransaction` response is correctly parsed.
#[test]
fn test_rpc_client_parses_transaction() {
    let resp = rpc_response_from_value(sample_get_transaction_result());
    let tx = parse_transaction_response("test_sig", &resp).unwrap();

    assert_eq!(tx.signature, "test_sig");
    assert_eq!(tx.slot, 123_456_789);
    assert_eq!(tx.account_keys.len(), 4);
    assert_eq!(
        tx.account_keys[0],
        "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin"
    );
    assert_eq!(
        tx.account_keys[2],
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
    );
    assert_eq!(tx.instructions.len(), 2);

    // First instruction: program_id_index=2 (Token program), accounts=[0,1,3].
    let ix0 = &tx.instructions[0];
    assert_eq!(ix0.program_id_index, 2);
    assert_eq!(ix0.account_indices, vec![0, 1, 3]);
    assert_eq!(ix0.data, "3Bxs4ThwQbE4vyj5");

    // Second instruction: program_id_index=1 (System program), accounts=[0].
    let ix1 = &tx.instructions[1];
    assert_eq!(ix1.program_id_index, 1);
    assert_eq!(ix1.account_indices, vec![0]);
    assert_eq!(ix1.data, "2ZpSih36go9D");

    // No error.
    assert!(tx.err.is_none());
}

/// Verify that a failed transaction's error is captured.
#[test]
fn test_rpc_client_parses_failed_transaction() {
    let resp = rpc_response_from_value(sample_failed_transaction_result());
    let tx = parse_transaction_response("failed_sig", &resp).unwrap();

    assert_eq!(tx.slot, 999_999);
    assert!(tx.err.is_some(), "failed transaction should have err set");

    let err = tx.err.unwrap();
    assert!(
        err.get("InstructionError").is_some(),
        "error should be InstructionError"
    );
}

/// Verify that a realistic `getAccountInfo` response is correctly parsed.
#[test]
fn test_rpc_client_parses_account() {
    let resp = rpc_response_from_value(sample_get_account_info_result());
    let acct = parse_account_response("test_pubkey", &resp).unwrap();

    assert_eq!(acct.pubkey, "test_pubkey");
    assert_eq!(acct.lamports, 1_000_000_000);
    assert_eq!(acct.owner, "BPFLoaderUpgradeab1e11111111111111111111111");
    assert!(acct.executable);
    assert_eq!(acct.data, b"Hello Solana!");
}

/// Verify that a null account (not found) returns an error.
#[test]
fn test_rpc_client_handles_null_account() {
    let resp = rpc_response_from_value(sample_null_account_result());
    let result = parse_account_response("missing_pubkey", &resp);
    assert!(result.is_err(), "null account value should be an error");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("not found"),
        "error should mention 'not found', got: {err_msg}"
    );
}

/// Verify that instruction data and program IDs are correctly extracted.
#[test]
fn test_replay_extracts_instruction_data() {
    let resp = rpc_response_from_value(sample_get_transaction_result());
    let tx = parse_transaction_response("test_sig", &resp).unwrap();

    // The first instruction's program is at index 2 = Token program.
    let program_id = &tx.account_keys[tx.instructions[0].program_id_index as usize];
    assert_eq!(program_id, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

    // The second instruction's program is at index 1 = System program.
    let program_id2 = &tx.account_keys[tx.instructions[1].program_id_index as usize];
    assert_eq!(program_id2, "11111111111111111111111111111111");

    // Verify instruction data is preserved.
    assert_eq!(tx.instructions[0].data, "3Bxs4ThwQbE4vyj5");
    assert_eq!(tx.instructions[1].data, "2ZpSih36go9D");
}

/// Verify that all account keys are extracted from the transaction.
#[test]
fn test_replay_identifies_accounts() {
    let resp = rpc_response_from_value(sample_get_transaction_result());
    let tx = parse_transaction_response("test_sig", &resp).unwrap();

    let accounts = extract_all_accounts(&tx);
    assert_eq!(accounts.len(), 4);
    assert!(accounts.contains(&"9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin".to_string()));
    assert!(accounts.contains(&"11111111111111111111111111111111".to_string()));
    assert!(accounts.contains(&"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string()));
    assert!(accounts.contains(&"SysvarRent111111111111111111111111111111111".to_string()));
}

/// Verify that program IDs are correctly extracted and deduplicated.
#[test]
fn test_replay_extracts_program_ids() {
    let resp = rpc_response_from_value(sample_get_transaction_result());
    let tx = parse_transaction_response("test_sig", &resp).unwrap();

    let program_ids = extract_program_ids(&tx);
    // Two instructions reference two different programs.
    assert_eq!(program_ids.len(), 2);
    assert!(program_ids.contains(&"11111111111111111111111111111111".to_string()));
    assert!(program_ids.contains(&"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string()));
}

/// Verify graceful error handling on RPC error responses.
#[test]
fn test_replay_handles_rpc_error() {
    let resp = rpc_error_response(-32600, "Invalid request");
    let result = parse_transaction_response("bad_sig", &resp);
    assert!(
        result.is_err(),
        "RPC error response should produce an error"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("null"),
        "error should indicate null result, got: {err_msg}"
    );
}

/// Verify that versioned transactions with object-style account keys parse correctly.
#[test]
fn test_rpc_client_parses_versioned_transaction() {
    let resp = rpc_response_from_value(sample_versioned_transaction_result());
    let tx = parse_transaction_response("versioned_sig", &resp).unwrap();

    assert_eq!(tx.slot, 200_000_000);
    assert_eq!(tx.account_keys.len(), 2);
    assert_eq!(
        tx.account_keys[0],
        "Abc111111111111111111111111111111111111111"
    );
    assert_eq!(
        tx.account_keys[1],
        "Def222222222222222222222222222222222222222"
    );
    assert_eq!(tx.instructions.len(), 1);
    assert_eq!(tx.instructions[0].program_id_index, 1);
}

/// Verify that a transaction with zero instructions still parses.
#[test]
fn test_rpc_client_parses_empty_instructions() {
    let result = serde_json::json!({
        "slot": 42,
        "transaction": {
            "message": {
                "accountKeys": ["SomeKey111111111111111111111111111111111111"],
                "instructions": []
            },
            "signatures": ["emptySig"]
        },
        "meta": { "err": null }
    });
    let resp = rpc_response_from_value(result);
    let tx = parse_transaction_response("empty_sig", &resp).unwrap();

    assert_eq!(tx.instructions.len(), 0);
    assert_eq!(tx.account_keys.len(), 1);

    let program_ids = extract_program_ids(&tx);
    assert!(program_ids.is_empty());
}

/// Verify account data decoding from base64.
#[test]
fn test_rpc_client_decodes_account_data() {
    // Encode some known bytes.
    let raw = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03];
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);

    let result = serde_json::json!({
        "context": { "slot": 50 },
        "value": {
            "lamports": 500,
            "owner": "OwnerProg111111111111111111111111111111111",
            "executable": false,
            "rentEpoch": 0,
            "data": [encoded, "base64"]
        }
    });

    let resp = rpc_response_from_value(result);
    let acct = parse_account_response("data_test", &resp).unwrap();

    assert_eq!(acct.data, raw);
    assert!(!acct.executable);
    assert_eq!(acct.lamports, 500);
}
