//! On-chain replay through the real Solana runtime, with a mandatory
//! divergence check.
//!
//! `replay-bundle` reads a [`bundle::ReplayBundle`], executes its
//! transactions in order through LiteSVM (Agave's program runtime and SBF VM,
//! register tracing on), compares each against the chain's recorded metadata,
//! and writes:
//!
//! * `verdicts.jsonl` — one line per transaction: executed or not (and why
//!   not), the verdict and its facts, and the trace written, if any;
//! * `traces/<signature>/` — a CodeTracer trace for each transaction the
//!   bundle asked to trace (see [`ctfs`]);
//! * `held-state.json` — every account the replay holds after the last
//!   transaction, so a capture can carry state into its next bundle.
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

use std::io::Write as _;
use std::path::Path;

use eyre::{Context, Result};
use serde_json::json;

use bundle::{ReplayBundle, read_bundle};
use engine::{Replay, TxOutcome};

pub const HELD_STATE_FORMAT: &str = "codetracer-solana/held-state@1";

pub struct Summary {
    pub executed: usize,
    pub not_executed: usize,
    pub matched: usize,
    pub divergent: usize,
    pub unchecked: usize,
    pub traced: usize,
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
    let mut summary = Summary {
        executed: 0,
        not_executed: 0,
        matched: 0,
        divergent: 0,
        unchecked: 0,
        traced: 0,
    };
    let mut last_slot = None;

    for tx in &bundle.transactions {
        last_slot = Some(tx.slot);
        let line = match replay.execute(tx)? {
            TxOutcome::NotExecuted(n) => {
                summary.not_executed += 1;
                json!({
                    "signature": n.signature,
                    "slot": tx.slot,
                    "indexInBlock": tx.index_in_block,
                    "executed": false,
                    "reason": n.reason,
                })
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
                    serde_json::Value::Null
                };
                let programs: Vec<serde_json::Value> = exec
                    .instructions
                    .iter()
                    .map(|r| {
                        json!({
                            "indexInTrace": r.index_in_trace,
                            "stackHeight": r.stack_height,
                            "programId": r.program_id,
                            "sbfInstructions": r.registers.as_ref().map(|v| v.len()),
                        })
                    })
                    .collect();
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
                })
            }
        };
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
                .unwrap_or_else(|| bundle::AccountEntry {
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
