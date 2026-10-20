//! On-chain replay through the real Solana runtime, with a mandatory
//! divergence check.
//!
//! Two drivers share one per-transaction step ([`run_transaction`]):
//!
//! * `replay-bundle` reads a [`bundle::ReplayBundle`] and writes
//!   `verdicts.jsonl`, `traces/<signature>/` for each transaction the bundle
//!   marks `trace`, and `held-state.json` (every account the replay holds
//!   after the last transaction).
//! * `replay-stream` keeps the replay alive and reads operations as JSON
//!   lines — accounts to hold, transactions to run, keys to evict — writing a
//!   result line for each. It is what a live capture drives, because the
//!   state it holds (tens of thousands of accounts, some of them megabytes of
//!   program code) cannot be passed around as a file per block.
//!
//! The step executes the transaction through LiteSVM (Agave's program runtime
//! and SBF VM, register tracing on), checks it against the chain's recorded
//! metadata, traces it if asked, and — unless the verdict is `match` —
//! EVICTS every account the transaction may have written. A transaction that
//! was refused or diverged has an unknown post-state, so the replay must stop
//! claiming to hold what it wrote; otherwise the next transaction would run
//! on a guess, and the guess would be invisible.
//!
//! A transaction is traced whatever its verdict, and the verdict travels with
//! it. Deciding that a `divergent` or `unchecked` trace is not published is
//! the consumer's job; hiding the verdict is never this module's.

pub mod bundle;
pub mod ctfs;
pub mod engine;
pub mod fetch;
pub mod listing;
pub mod oracle;

use std::io::{BufRead, Write};
use std::path::Path;

use eyre::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use bundle::{AccountEntry, BundleTransaction, FeatureEntry, ReplayBundle, read_bundle};
use engine::{Replay, TxOutcome, writable_keys};

pub const HELD_STATE_FORMAT: &str = "codetracer-solana/held-state@1";

#[derive(Default)]
pub struct Summary {
    pub executed: usize,
    pub not_executed: usize,
    pub matched: usize,
    pub divergent: usize,
    pub unchecked: usize,
    pub traced: usize,
}

/// Executes, checks, traces (if asked) and evicts (unless `match`) one
/// transaction, returning its result line.
pub fn run_transaction(
    replay: &mut Replay,
    tx: &BundleTransaction,
    out_dir: &Path,
    opts: &ctfs::TraceOptions,
    summary: &mut Summary,
) -> Result<Value> {
    let outcome = replay.execute(tx)?;
    let refused = matches!(outcome, TxOutcome::NotExecuted(_));
    let (mut line, matched) = match outcome {
        TxOutcome::NotExecuted(n) => {
            summary.not_executed += 1;
            (
                json!({
                    "signature": n.signature,
                    "slot": tx.slot,
                    "indexInBlock": tx.index_in_block,
                    "executed": false,
                    "reason": n.reason,
                }),
                false,
            )
        }
        TxOutcome::Executed(exec) => {
            summary.executed += 1;
            let verdict = oracle::check(&exec, tx.meta.as_ref());
            match verdict.status {
                oracle::Status::Match => summary.matched += 1,
                oracle::Status::Divergent => summary.divergent += 1,
                oracle::Status::Unchecked => summary.unchecked += 1,
            }
            let trace = if tx.trace {
                let listings = exec
                    .instructions
                    .iter()
                    .filter_map(|r| r.listing)
                    .filter_map(|k| replay.listing(k).map(|l| (k, l)))
                    .collect();
                let written = ctfs::write_transaction_trace(
                    &exec,
                    &listings,
                    &out_dir.join("traces").join(&exec.signature),
                    opts,
                )?;
                summary.traced += 1;
                json!({
                    "dir": written.dir,
                    "steps": written.steps,
                    "calls": written.calls,
                })
            } else {
                Value::Null
            };
            let programs: Vec<Value> = exec
                .instructions
                .iter()
                .map(|r| {
                    json!({
                        "indexInTrace": r.index_in_trace,
                        "stackHeight": r.stack_height,
                        "caller": r.caller,
                        "programId": r.program_id,
                        "sbfInstructions": r.registers.as_ref().map(|v| v.len()),
                    })
                })
                .collect();
            let matched = verdict.status == oracle::Status::Match;
            (
                json!({
                    "signature": exec.signature,
                    "slot": exec.slot,
                    "indexInBlock": tx.index_in_block,
                    "executed": true,
                    "verdict": verdict,
                    "computeUnitsConsumed": exec.compute_units_consumed,
                    "fee": exec.fee,
                    "invocations": programs,
                    "trace": trace,
                }),
                matched,
            )
        }
    };
    let held_before: Vec<String> = writable_keys(tx)?
        .into_iter()
        .filter(|k| replay.holds(k))
        .collect();
    if refused {
        // The replay never touched this transaction's accounts, so those whose
        // post-state the chain's record fully states are carried to it.
        let (carried, evicted) = replay.settle_unreplayed(tx)?;
        line["carried"] = json!(carried);
        line["evicted"] = json!(evicted);
    } else if !matched {
        // The replay executed it and wrote its own (wrong) post-state: nothing
        // it wrote can be carried.
        let evicted = replay.evict(&writable_keys(tx)?);
        line["evicted"] = json!(evicted);
    }
    if refused || !matched {
        // Which keys stopped being held, so a capture knows what to read again.
        let gone: Vec<&String> = held_before.iter().filter(|k| !replay.holds(k)).collect();
        line["evictedKeys"] = json!(gone);
    }
    Ok(line)
}

pub fn replay_bundle_file(
    bundle_path: &Path,
    out_dir: &Path,
    opts: &ctfs::TraceOptions,
) -> Result<Summary> {
    let bundle = read_bundle(bundle_path)?;
    replay_bundle(&bundle, out_dir, opts)
}

pub fn replay_bundle(
    bundle: &ReplayBundle,
    out_dir: &Path,
    opts: &ctfs::TraceOptions,
) -> Result<Summary> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create {}", out_dir.display()))?;
    let mut replay = Replay::new(bundle)?;
    let mut verdicts = std::fs::File::create(out_dir.join("verdicts.jsonl"))?;
    let mut summary = Summary::default();
    let mut last_slot = None;

    for tx in &bundle.transactions {
        last_slot = Some(tx.slot);
        let line = run_transaction(&mut replay, tx, out_dir, opts, &mut summary)?;
        writeln!(verdicts, "{line}")?;
    }

    // An account the replay holds as nonexistent (never created, or closed by
    // a transaction) is stated with zero lamports rather than dropped, so the
    // next bundle holds it too instead of refusing it.
    let accounts: Vec<_> = replay
        .held_keys()
        .iter()
        .map(|k| {
            replay
                .account_entry(k, last_slot)
                .unwrap_or_else(|| AccountEntry {
                    pubkey: k.clone(),
                    lamports: 0,
                    owner: "11111111111111111111111111111111".into(),
                    executable: false,
                    rent_epoch: 0,
                    data: String::new(),
                    read_at_slot: last_slot,
                })
        })
        .collect();
    let held = json!({
        "format": HELD_STATE_FORMAT,
        "afterSlot": last_slot,
        "accounts": accounts,
    });
    std::fs::write(out_dir.join("held-state.json"), serde_json::to_vec(&held)?)?;
    Ok(summary)
}

/// One operation of the `replay-stream` protocol.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum StreamOp {
    /// Must come first: the runtime is built from the cluster's sysvars and
    /// feature accounts, which cannot change under a running replay.
    Init {
        #[serde(default)]
        sysvars: Vec<AccountEntry>,
        features: Vec<FeatureEntry>,
    },
    /// Start holding these accounts (state read at `readAtSlot`). The caller
    /// must only send an account read at a slot BEFORE the next transaction
    /// it sends, and must send every later transaction that touches it.
    Accounts {
        accounts: Vec<AccountEntry>,
    },
    Tx(BundleTransaction),
    Evict {
        keys: Vec<String>,
    },
    /// Answer with the number of accounts held.
    Held,
}

/// Runs the `replay-stream` protocol: one JSON operation per input line, one
/// JSON result line per operation, flushed as it is written.
pub fn replay_stream(
    input: impl BufRead,
    mut output: impl Write,
    out_dir: &Path,
    opts: &ctfs::TraceOptions,
) -> Result<Summary> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create {}", out_dir.display()))?;
    let mut replay: Option<Replay> = None;
    let mut summary = Summary::default();
    for (n, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let op: StreamOp = serde_json::from_str(&line)
            .with_context(|| format!("input line {}: not a replay-stream operation", n + 1))?;
        let answer = match op {
            StreamOp::Init { sysvars, features } => {
                if replay.is_some() {
                    bail!("input line {}: `init` given twice", n + 1);
                }
                replay = Some(Replay::new(&ReplayBundle {
                    format: bundle::BUNDLE_FORMAT.into(),
                    cluster: String::new(),
                    sysvars,
                    features,
                    accounts: Vec::new(),
                    transactions: Vec::new(),
                })?);
                json!({"op": "init", "ok": true})
            }
            other => {
                let Some(r) = replay.as_mut() else {
                    bail!("input line {}: the first operation must be `init`", n + 1);
                };
                match other {
                    StreamOp::Init { .. } => unreachable!(),
                    StreamOp::Accounts { accounts } => {
                        r.add_accounts(&accounts)?;
                        json!({"op": "accounts", "added": accounts.len(), "held": r.held_count()})
                    }
                    StreamOp::Tx(tx) => {
                        let mut v = run_transaction(r, &tx, out_dir, opts, &mut summary)?;
                        v["op"] = json!("tx");
                        v
                    }
                    StreamOp::Evict { keys } => {
                        json!({"op": "evict", "evicted": r.evict(&keys), "held": r.held_count()})
                    }
                    StreamOp::Held => json!({"op": "held", "held": r.held_count()}),
                }
            }
        };
        writeln!(output, "{answer}")?;
        output.flush()?;
    }
    Ok(summary)
}
