//! The divergence check: a replay compared against the chain's own record.
//!
//! The oracle is the transaction metadata the cluster recorded — result,
//! compute units, logs, balances before and after, token balances, the inner
//! instruction tree and return data. These are consensus facts, so no
//! recorder defect can make the chain's side of the comparison agree with a
//! wrong replay.
//!
//! Two rules carry the module:
//!
//! * **Nothing to compare is not agreement.** A verdict of `match` needs at
//!   least one fact compared and none disagreeing; a replay with no
//!   comparable facts is `unchecked`, which the caller must treat as
//!   unpublishable.
//! * **A divergence names its cause.** The balances BEFORE the transaction are
//!   on the chain's record too, so a replay whose starting lamports disagree
//!   with them ran on the wrong prestate, and that is a different finding from
//!   a replay that started right and executed differently. Only the second can
//!   be a defect in the recorder or the runtime.

use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;

use super::engine::TxExecution;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Match,
    Divergent,
    Unchecked,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// The replay's starting state disagrees with the chain's recorded
    /// pre-balances: the trace executed on the wrong prestate.
    Prestate,
    /// The starting state agrees with every pre-fact the chain recorded, and
    /// the execution still differs: the runtime, the recorder, or account
    /// data the pre-facts do not cover.
    Execution,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Fact {
    pub name: &'static str,
    pub matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub status: Status,
    pub oracle: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    pub facts: Vec<Fact>,
    /// Facts the chain's record did not carry, so were not compared.
    pub not_compared: Vec<&'static str>,
}

pub const ORACLE: &str = "chain-transaction-metadata";

fn fact(name: &'static str, matched: bool, detail: Option<String>) -> Fact {
    Fact {
        name,
        matched,
        detail,
    }
}

fn u64s(v: &Value) -> Option<Vec<u64>> {
    v.as_array()?.iter().map(|x| x.as_u64()).collect()
}

/// SPL Token and Token-2022 account amount: bytes 64..72 of the account data.
fn token_amount(data: &[u8]) -> Option<u64> {
    data.get(64..72)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

pub fn check(exec: &TxExecution, meta: Option<&Value>) -> Verdict {
    let mut facts = Vec::new();
    let mut not_compared = Vec::new();
    let Some(meta) = meta.filter(|m| m.is_object()) else {
        return Verdict {
            status: Status::Unchecked,
            oracle: ORACLE,
            cause: None,
            facts,
            not_compared: vec!["meta"],
        };
    };

    // ── pre-facts: is the replay starting from the chain's state? ──────────
    let mut prestate_ok = true;
    match u64s(&meta["preBalances"]) {
        Some(chain) => {
            let ours: Vec<u64> = exec
                .pre_accounts
                .iter()
                .map(|a| a.as_ref().map_or(0, |a| a.lamports))
                .collect();
            let ok = chain == ours;
            prestate_ok &= ok;
            facts.push(fact(
                "preBalances",
                ok,
                (!ok).then(|| first_difference(&exec.account_keys, &chain, &ours)),
            ));
        }
        None => not_compared.push("preBalances"),
    }
    match meta["preTokenBalances"].as_array() {
        Some(list) => {
            let (ok, detail) = token_balances(list, &exec.pre_accounts);
            prestate_ok &= ok;
            facts.push(fact("preTokenBalances", ok, detail));
        }
        None => not_compared.push("preTokenBalances"),
    }

    // ── post-facts: did it execute as the chain did? ───────────────────────
    let mut execution_ok = true;
    let chain_err = &meta["err"];
    let ours_err = exec
        .error
        .as_ref()
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    let ok = *chain_err == ours_err;
    execution_ok &= ok;
    facts.push(fact(
        "err",
        ok,
        (!ok).then(|| format!("chain {chain_err}, replay {ours_err}")),
    ));

    match meta["computeUnitsConsumed"].as_u64() {
        Some(cu) => {
            let ok = cu == exec.compute_units_consumed;
            execution_ok &= ok;
            facts.push(fact(
                "computeUnitsConsumed",
                ok,
                (!ok).then(|| format!("chain {cu}, replay {}", exec.compute_units_consumed)),
            ));
        }
        None => not_compared.push("computeUnitsConsumed"),
    }

    match meta["fee"].as_u64() {
        Some(fee) => {
            let ok = fee == exec.fee;
            execution_ok &= ok;
            facts.push(fact(
                "fee",
                ok,
                (!ok).then(|| format!("chain {fee}, replay {}", exec.fee)),
            ));
        }
        None => not_compared.push("fee"),
    }

    match meta["logMessages"].as_array() {
        Some(list) => {
            let chain: Vec<&str> = list.iter().filter_map(|l| l.as_str()).collect();
            // A log the cluster truncated is compared up to the truncation.
            let truncated = chain.iter().position(|l| *l == "Log truncated");
            let n = truncated.unwrap_or(chain.len());
            let ours: Vec<&str> = exec.logs.iter().map(|s| s.as_str()).collect();
            let ok = if truncated.is_some() {
                ours.len() >= n && chain[..n] == ours[..n]
            } else {
                chain == ours
            };
            execution_ok &= ok;
            facts.push(fact(
                "logMessages",
                ok,
                (!ok).then(|| {
                    let i = chain
                        .iter()
                        .zip(ours.iter())
                        .position(|(a, b)| a != b)
                        .unwrap_or(chain.len().min(ours.len()));
                    format!(
                        "first difference at line {i}: chain {:?}, replay {:?}",
                        chain.get(i),
                        ours.get(i)
                    )
                }),
            ));
        }
        None => not_compared.push("logMessages"),
    }

    match u64s(&meta["postBalances"]) {
        Some(chain) => {
            let ours: Vec<u64> = exec
                .post_accounts
                .iter()
                .map(|a| a.as_ref().map_or(0, |a| a.lamports))
                .collect();
            let ok = chain == ours;
            execution_ok &= ok;
            facts.push(fact(
                "postBalances",
                ok,
                (!ok).then(|| first_difference(&exec.account_keys, &chain, &ours)),
            ));
        }
        None => not_compared.push("postBalances"),
    }

    match meta["postTokenBalances"].as_array() {
        Some(list) => {
            let (ok, detail) = token_balances(list, &exec.post_accounts);
            execution_ok &= ok;
            facts.push(fact("postTokenBalances", ok, detail));
        }
        None => not_compared.push("postTokenBalances"),
    }

    match meta["innerInstructions"].as_array() {
        Some(list) => {
            let (ok, detail) = inner_instructions(list, exec);
            execution_ok &= ok;
            facts.push(fact("innerInstructions", ok, detail));
        }
        None => not_compared.push("innerInstructions"),
    }

    // Return data: the cluster omits the member when none was set.
    let chain_rd = &meta["returnData"];
    if chain_rd.is_null() {
        let ok = exec.return_data.is_none();
        execution_ok &= ok;
        facts.push(fact(
            "returnData",
            ok,
            (!ok).then(|| "chain recorded none, replay set some".to_string()),
        ));
    } else {
        let chain_pid = chain_rd["programId"].as_str().unwrap_or_default();
        let chain_data = chain_rd["data"][0]
            .as_str()
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .unwrap_or_default();
        let ok = exec
            .return_data
            .as_ref()
            .is_some_and(|(pid, d)| pid == chain_pid && *d == chain_data);
        execution_ok &= ok;
        facts.push(fact(
            "returnData",
            ok,
            (!ok).then(|| "return data differs".to_string()),
        ));
    }

    let compared = !facts.is_empty();
    let status = if !compared {
        Status::Unchecked
    } else if prestate_ok && execution_ok {
        Status::Match
    } else {
        Status::Divergent
    };
    let cause = match status {
        Status::Divergent if !prestate_ok => Some(Cause::Prestate),
        Status::Divergent => Some(Cause::Execution),
        _ => None,
    };
    Verdict {
        status,
        oracle: ORACLE,
        cause,
        facts,
        not_compared,
    }
}

fn first_difference(keys: &[String], chain: &[u64], ours: &[u64]) -> String {
    if chain.len() != ours.len() {
        return format!(
            "chain lists {} balances, replay {}",
            chain.len(),
            ours.len()
        );
    }
    let i = chain
        .iter()
        .zip(ours)
        .position(|(a, b)| a != b)
        .unwrap_or(0);
    format!(
        "account {} ({}): chain {}, replay {}",
        i,
        keys.get(i).map(String::as_str).unwrap_or("?"),
        chain[i],
        ours[i]
    )
}

fn token_balances(
    list: &[Value],
    accounts: &[Option<solana_account::Account>],
) -> (bool, Option<String>) {
    for entry in list {
        let Some(index) = entry["accountIndex"].as_u64() else {
            return (false, Some("token balance without accountIndex".into()));
        };
        let chain: Option<u64> = entry["uiTokenAmount"]["amount"]
            .as_str()
            .and_then(|s| s.parse().ok());
        let ours = accounts
            .get(index as usize)
            .and_then(|a| a.as_ref())
            .and_then(|a| token_amount(&a.data));
        if chain != ours {
            return (
                false,
                Some(format!(
                    "account {index}: chain amount {chain:?}, replay {ours:?}"
                )),
            );
        }
    }
    (true, None)
}

fn inner_instructions(list: &[Value], exec: &TxExecution) -> (bool, Option<String>) {
    // The chain lists only top-level instructions that made inner ones,
    // each tagged with its index; the replay lists every top-level slot.
    let mut chain_by_index: Vec<(usize, &Vec<Value>)> = Vec::new();
    for group in list {
        let (Some(index), Some(ixs)) = (group["index"].as_u64(), group["instructions"].as_array())
        else {
            return (false, Some("malformed innerInstructions group".into()));
        };
        chain_by_index.push((index as usize, ixs));
    }
    let ours: Vec<(usize, &Vec<(u8, Vec<u8>, Vec<u8>, u8)>)> = exec
        .inner_instructions
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.is_empty())
        .collect();
    if chain_by_index.len() != ours.len() {
        return (
            false,
            Some(format!(
                "chain has inner instructions under {} top-level instruction(s), replay {}",
                chain_by_index.len(),
                ours.len()
            )),
        );
    }
    for ((ci, cixs), (oi, oixs)) in chain_by_index.iter().zip(ours.iter()) {
        if ci != oi || cixs.len() != oixs.len() {
            return (
                false,
                Some(format!(
                    "top-level {ci}: chain {} inner, replay {} under {oi}",
                    cixs.len(),
                    oixs.len()
                )),
            );
        }
        for (k, (c, o)) in cixs.iter().zip(oixs.iter()).enumerate() {
            let pid = c["programIdIndex"].as_u64().map(|x| x as u8);
            let accts: Option<Vec<u8>> = c["accounts"].as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_u64().map(|x| x as u8))
                    .collect()
            });
            let data = c["data"]
                .as_str()
                .and_then(|s| bs58::decode(s).into_vec().ok());
            let height = c["stackHeight"].as_u64().map(|x| x as u8);
            let same = pid == Some(o.0)
                && accts.as_deref() == Some(o.1.as_slice())
                && data.as_deref() == Some(o.2.as_slice())
                && height.is_none_or(|h| h == o.3);
            if !same {
                return (
                    false,
                    Some(format!("top-level {ci}, inner {k}: instruction differs")),
                );
            }
        }
    }
    (true, None)
}
