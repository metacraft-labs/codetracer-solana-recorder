//! Builds a replay bundle for one transaction from a JSON-RPC endpoint, using
//! the accounts' CURRENT state.
//!
//! Solana's RPC has no way to read an account at a past slot, so this is the
//! only bundle an RPC alone can build, and for almost every transaction it is
//! the wrong prestate: by the time the read happens the transaction has run
//! and its writable accounts hold its POST-state. The bundle is built anyway,
//! honestly labelled, because the divergence check is what says so — the
//! replay of such a bundle is expected to come back `divergent` with cause
//! `prestate`, and one that comes back `match` has been checked rather than
//! assumed.

use base64::Engine as _;
use eyre::{Result, bail, eyre};
use serde_json::{Value, json};

use super::bundle::{AccountEntry, BUNDLE_FORMAT, BundleTransaction, FeatureEntry, ReplayBundle};

/// The sysvar accounts a bundle carries.
pub const SYSVARS: &[&str] = &[
    "SysvarC1ock11111111111111111111111111111111",
    "SysvarRent111111111111111111111111111111111",
    "SysvarEpochSchedu1e111111111111111111111111",
    "SysvarS1otHashes111111111111111111111111111",
    "SysvarStakeHistory1111111111111111111111111",
    "SysvarEpochRewards1111111111111111111111111",
    "SysvarLastRestartS1ot1111111111111111111111",
];
const UPGRADEABLE_LOADER: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

struct Rpc {
    url: String,
    client: reqwest::blocking::Client,
}

impl Rpc {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        for attempt in 0..6 {
            let res = self
                .client
                .post(&self.url)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                .send();
            let res = match res {
                Ok(r) => r,
                Err(e) if attempt < 5 => {
                    eprintln!("{method}: {e}; retrying");
                    std::thread::sleep(std::time::Duration::from_millis(800 * (attempt + 1)));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if res.status().as_u16() == 429 {
                std::thread::sleep(std::time::Duration::from_millis(1500 * (attempt + 1)));
                continue;
            }
            let body: Value = res.json()?;
            if let Some(err) = body.get("error") {
                bail!("{method}: {err}");
            }
            return Ok(body["result"].clone());
        }
        bail!("{method}: rate limited")
    }

    /// `getMultipleAccounts`, 100 at a time. Returns entries for accounts that
    /// exist and the slot every batch was read at (the latest of them).
    fn accounts(&self, keys: &[String]) -> Result<(Vec<AccountEntry>, Vec<String>, u64)> {
        let mut found = Vec::new();
        let mut absent = Vec::new();
        let mut slot = 0;
        // 100 keys per call is the protocol's limit; some providers allow only
        // 10 (publicnode answers -32602 above that), so a refused batch is
        // re-asked in tens.
        let batches: Vec<&[String]> = keys.chunks(100).collect();
        let mut queue: std::collections::VecDeque<&[String]> = batches.into();
        while let Some(chunk) = queue.pop_front() {
            let r = match self.call(
                "getMultipleAccounts",
                json!([chunk, {"encoding": "base64", "commitment": "finalized"}]),
            ) {
                Ok(r) => r,
                Err(e) if chunk.len() > 10 && e.to_string().contains("-32602") => {
                    for small in chunk.chunks(10).rev() {
                        queue.push_front(small);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            let read_at = r["context"]["slot"].as_u64();
            slot = slot.max(read_at.unwrap_or(0));
            for (key, v) in chunk
                .iter()
                .zip(r["value"].as_array().into_iter().flatten())
            {
                if v.is_null() {
                    absent.push(key.clone());
                    continue;
                }
                found.push(AccountEntry {
                    pubkey: key.clone(),
                    lamports: v["lamports"].as_u64().unwrap_or(0),
                    owner: v["owner"].as_str().unwrap_or_default().to_string(),
                    executable: v["executable"].as_bool().unwrap_or(false),
                    rent_epoch: v["rentEpoch"].as_u64().unwrap_or(0),
                    data: v["data"][0].as_str().unwrap_or_default().to_string(),
                    read_at_slot: read_at,
                });
            }
        }
        Ok((found, absent, slot))
    }
}

/// Fetches `signature` and everything its replay needs, at current state.
pub fn bundle_for_signature(rpc_url: &str, signature: &str) -> Result<ReplayBundle> {
    let rpc = Rpc {
        url: rpc_url.to_string(),
        client: reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?,
    };
    let opts = |enc: &str| json!({"encoding": enc, "maxSupportedTransactionVersion": 1, "commitment": "finalized"});
    let tx_json = rpc.call("getTransaction", json!([signature, opts("json")]))?;
    if tx_json.is_null() {
        bail!("{signature}: the endpoint does not have this transaction");
    }
    let tx_b64 = rpc.call("getTransaction", json!([signature, opts("base64")]))?;
    let wire = tx_b64["transaction"][0]
        .as_str()
        .ok_or_else(|| eyre!("{signature}: no base64 transaction in the answer"))?
        .to_string();
    let slot = tx_json["slot"].as_u64().unwrap_or(0);
    let message = &tx_json["transaction"]["message"];

    let mut keys: Vec<String> = Vec::new();
    let push = |k: &str, keys: &mut Vec<String>| {
        if !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
    };
    for k in message["accountKeys"].as_array().into_iter().flatten() {
        push(k.as_str().unwrap_or_default(), &mut keys);
    }
    for part in ["writable", "readonly"] {
        for k in tx_json["meta"]["loadedAddresses"][part]
            .as_array()
            .into_iter()
            .flatten()
        {
            push(k.as_str().unwrap_or_default(), &mut keys);
        }
    }
    for l in message["addressTableLookups"]
        .as_array()
        .into_iter()
        .flatten()
    {
        push(l["accountKey"].as_str().unwrap_or_default(), &mut keys);
    }
    keys.retain(|k| !SYSVARS.contains(&k.as_str()));

    let (mut accounts, absent, _) = rpc.accounts(&keys)?;
    // Every upgradeable program's code lives in its programdata account.
    let mut programdata = Vec::new();
    for a in &accounts {
        if a.executable && a.owner == UPGRADEABLE_LOADER {
            let data = a.data_bytes()?;
            if data.len() >= 36 && data[0..4] == [2, 0, 0, 0] {
                programdata.push(bs58::encode(&data[4..36]).into_string());
            }
        }
    }
    programdata.retain(|k| !keys.contains(k));
    let (pd, _, _) = rpc.accounts(&programdata)?;
    accounts.extend(pd);
    // A key that does not exist now is stated as absent (zero lamports), so the
    // replay holds it rather than refusing it; that is still a current-state
    // claim, and the check judges it like the others.
    for k in absent {
        accounts.push(AccountEntry {
            pubkey: k,
            lamports: 0,
            owner: "11111111111111111111111111111111".into(),
            executable: false,
            rent_epoch: 0,
            data: String::new(),
            read_at_slot: None,
        });
    }

    let sysvar_keys: Vec<String> = SYSVARS.iter().map(|s| s.to_string()).collect();
    let (sysvars, _, _) = rpc.accounts(&sysvar_keys)?;

    let feature_ids: Vec<String> = agave_feature_set::FEATURE_NAMES
        .keys()
        .map(|k| k.to_string())
        .collect();
    let (feature_accounts, _, _) = rpc.accounts(&feature_ids)?;
    let mut features = Vec::new();
    for f in feature_accounts {
        let data = base64::engine::general_purpose::STANDARD.decode(&f.data)?;
        // `Feature { activated_at: Option<u64> }`, bincode: tag byte then u64 LE.
        let activated_at = (data.first() == Some(&1) && data.len() >= 9)
            .then(|| u64::from_le_bytes(data[1..9].try_into().unwrap()));
        features.push(FeatureEntry {
            id: f.pubkey,
            activated_at,
        });
    }

    Ok(ReplayBundle {
        format: BUNDLE_FORMAT.to_string(),
        cluster: "mainnet-beta".to_string(),
        sysvars,
        features,
        accounts,
        transactions: vec![BundleTransaction {
            signature: signature.to_string(),
            slot,
            index_in_block: None,
            block_time: tx_json["blockTime"].as_i64(),
            wire,
            meta: Some(tx_json["meta"].clone()),
            trace: true,
        }],
    })
}
