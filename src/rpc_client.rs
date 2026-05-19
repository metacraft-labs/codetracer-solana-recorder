//! Minimal Solana JSON-RPC client for fetching transaction and account data.
//!
//! Uses raw `reqwest` + `serde_json` to avoid pulling in the heavyweight
//! `solana-client` crate and its deep dependency tree.  Only the fields
//! needed by the replay pipeline are modelled.

use eyre::{Context, Result, bail, eyre};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed representation of a Solana transaction returned by `getTransaction`.
#[derive(Debug, Clone)]
pub struct TransactionData {
    /// The transaction signature.
    pub signature: String,
    /// Slot in which the transaction was confirmed.
    pub slot: u64,
    /// Ordered list of account keys referenced by the transaction.
    pub account_keys: Vec<String>,
    /// Instructions contained in the transaction message.
    pub instructions: Vec<InstructionData>,
    /// Whether the transaction succeeded (`Ok`) or failed.
    pub err: Option<serde_json::Value>,
}

/// A single instruction extracted from a transaction.
#[derive(Debug, Clone)]
pub struct InstructionData {
    /// Index into `TransactionData::account_keys` identifying the program.
    pub program_id_index: u8,
    /// Indices into `TransactionData::account_keys` for the accounts
    /// referenced by this instruction.
    pub account_indices: Vec<u8>,
    /// Raw instruction data (decoded from base58 by the RPC).
    pub data: String,
}

/// Account information returned by `getAccountInfo`.
#[derive(Debug, Clone)]
pub struct AccountData {
    /// The account public key (echoed back for convenience).
    pub pubkey: String,
    /// Lamports held by the account.
    pub lamports: u64,
    /// Owner program of the account.
    pub owner: String,
    /// Whether the account is executable (i.e. a program).
    pub executable: bool,
    /// Raw account data bytes (decoded from base64).
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// JSON-RPC envelope types (private)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: serde_json::Value,
}

/// Parsed JSON-RPC response envelope.
///
/// Made public so that test helpers (`rpc_response_from_value`,
/// `rpc_error_response`) can return it to callers in other crates/modules.
#[derive(Deserialize)]
pub struct RpcResponse {
    result: Option<serde_json::Value>,
    error: Option<RpcError>,
}

/// JSON-RPC error payload.
#[derive(Deserialize, Debug)]
pub struct RpcError {
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fetch and parse a confirmed transaction from a Solana RPC endpoint.
///
/// Calls `getTransaction` with `jsonParsed` encoding so that account keys
/// are returned as base-58 strings.
///
/// # Errors
///
/// Returns an error if the HTTP request fails, the RPC returns an error,
/// or the response cannot be parsed into the expected shape.
pub fn fetch_transaction(rpc_url: &str, signature: &str) -> Result<TransactionData> {
    let body = RpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "getTransaction",
        params: serde_json::json!([
            signature,
            {
                "encoding": "jsonParsed",
                "maxSupportedTransactionVersion": 0
            }
        ]),
    };

    let resp = send_rpc_request(rpc_url, &body)
        .with_context(|| format!("getTransaction RPC call failed for signature {signature}"))?;

    parse_transaction_response(signature, &resp)
}

/// Fetch account information from a Solana RPC endpoint.
///
/// Calls `getAccountInfo` with `base64` encoding so the raw account data
/// can be decoded.
///
/// # Errors
///
/// Returns an error if the HTTP request fails, the RPC returns an error,
/// or the response cannot be parsed.
pub fn fetch_account(rpc_url: &str, pubkey: &str) -> Result<AccountData> {
    let body = RpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "getAccountInfo",
        params: serde_json::json!([
            pubkey,
            { "encoding": "base64" }
        ]),
    };

    let resp = send_rpc_request(rpc_url, &body)
        .with_context(|| format!("getAccountInfo RPC call failed for pubkey {pubkey}"))?;

    parse_account_response(pubkey, &resp)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Send a JSON-RPC request and return the parsed `RpcResponse`.
fn send_rpc_request(rpc_url: &str, body: &RpcRequest<'_>) -> Result<RpcResponse> {
    let client = reqwest::blocking::Client::new();
    let http_resp = client
        .post(rpc_url)
        .json(body)
        .send()
        .with_context(|| format!("HTTP POST to {rpc_url} failed"))?;

    let status = http_resp.status();
    if !status.is_success() {
        let text = http_resp.text().unwrap_or_default();
        bail!("RPC endpoint returned HTTP {status}: {text}");
    }

    let rpc_resp: RpcResponse = http_resp
        .json()
        .context("failed to deserialize JSON-RPC response")?;

    if let Some(err) = &rpc_resp.error {
        bail!("RPC error (code {}): {}", err.code, err.message);
    }

    Ok(rpc_resp)
}

/// Parse a `getTransaction` JSON-RPC result into [`TransactionData`].
pub fn parse_transaction_response(signature: &str, resp: &RpcResponse) -> Result<TransactionData> {
    let result = resp
        .result
        .as_ref()
        .ok_or_else(|| eyre!("getTransaction returned null for signature {signature}"))?;

    let slot = result.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);

    // Extract account keys from the transaction message.
    let message = result
        .pointer("/transaction/message")
        .ok_or_else(|| eyre!("transaction response missing /transaction/message"))?;

    let account_keys: Vec<String> = message
        .get("accountKeys")
        .and_then(|v| v.as_array())
        .ok_or_else(|| eyre!("transaction message missing accountKeys array"))?
        .iter()
        .map(|v| {
            // accountKeys can be strings (legacy) or objects with a "pubkey" field (versioned).
            if let Some(s) = v.as_str() {
                s.to_string()
            } else if let Some(s) = v.get("pubkey").and_then(|p| p.as_str()) {
                s.to_string()
            } else {
                String::new()
            }
        })
        .collect();

    // Extract instructions.
    let raw_instructions = message
        .get("instructions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| eyre!("transaction message missing instructions array"))?;

    let instructions: Vec<InstructionData> = raw_instructions
        .iter()
        .filter_map(|ix| {
            let program_id_index = ix.get("programIdIndex")?.as_u64()? as u8;
            let account_indices: Vec<u8> = ix
                .get("accounts")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default();
            let data = ix
                .get("data")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            Some(InstructionData {
                program_id_index,
                account_indices,
                data,
            })
        })
        .collect();

    // Extract error status.
    let err = result
        .pointer("/meta/err")
        .and_then(|v| if v.is_null() { None } else { Some(v.clone()) });

    Ok(TransactionData {
        signature: signature.to_string(),
        slot,
        account_keys,
        instructions,
        err,
    })
}

/// Parse a `getAccountInfo` JSON-RPC result into [`AccountData`].
pub fn parse_account_response(pubkey: &str, resp: &RpcResponse) -> Result<AccountData> {
    let result = resp
        .result
        .as_ref()
        .ok_or_else(|| eyre!("getAccountInfo returned null for pubkey {pubkey}"))?;

    let value = result
        .get("value")
        .ok_or_else(|| eyre!("getAccountInfo result missing 'value' for pubkey {pubkey}"))?;

    if value.is_null() {
        bail!("account {pubkey} not found (value is null)");
    }

    let lamports = value.get("lamports").and_then(|v| v.as_u64()).unwrap_or(0);

    let owner = value
        .get("owner")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let executable = value
        .get("executable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Account data is returned as ["<base64>", "base64"].
    let data_bytes = value
        .get("data")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .map(|b64| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap_or_default()
        })
        .unwrap_or_default();

    Ok(AccountData {
        pubkey: pubkey.to_string(),
        lamports,
        owner,
        executable,
        data: data_bytes,
    })
}

// ---------------------------------------------------------------------------
// Helpers for constructing RpcResponse from raw JSON (used by tests)
// ---------------------------------------------------------------------------

/// Construct an `RpcResponse` from a raw JSON value (for testing).
pub fn rpc_response_from_value(result: serde_json::Value) -> RpcResponse {
    RpcResponse {
        result: Some(result),
        error: None,
    }
}

/// Construct an error `RpcResponse` (for testing).
pub fn rpc_error_response(code: i64, message: &str) -> RpcResponse {
    RpcResponse {
        result: None,
        error: Some(RpcError {
            code,
            message: message.to_string(),
        }),
    }
}
