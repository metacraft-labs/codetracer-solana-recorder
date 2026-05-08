//! On-chain transaction replay pipeline.
//!
//! Fetches a confirmed Solana transaction via JSON-RPC, reconstructs its
//! execution context, and feeds the result into the existing CodeTracer
//! recording pipeline.
//!
//! ## Current limitations
//!
//! * **Account state**: The pipeline fetches *current* account state rather
//!   than the historical state at the transaction's slot.  This means
//!   replays against mainnet programs that have been upgraded since the
//!   transaction was confirmed will use the newer program binary.
//!
//! * **Register traces**: Real SBF register-level traces require
//!   integration with [Mollusk](https://github.com/buffalojoec/mollusk)
//!   or a patched SBF VM.  Until that integration is complete, this
//!   module constructs a synthetic single-step register trace so the
//!   existing recorder pipeline can produce output.  The resulting trace
//!   will contain minimal step/variable information.
//!
//! * **Inner instructions / CPI**: Cross-program invocations are not yet
//!   replayed individually.  Only top-level instructions are processed.

use std::path::{Path, PathBuf};

use eyre::{Context, Result, bail, eyre};

use crate::recorder::record_from_snapshots;
use crate::register_trace::RegisterSnapshot;
use crate::rpc_client::{self, TransactionData};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Replay a confirmed transaction and produce a CodeTracer trace.
///
/// # Arguments
///
/// * `rpc_url`     - Solana JSON-RPC endpoint URL.
/// * `signature`   - Base-58 transaction signature.
/// * `out_dir`     - Directory where the trace files will be written.
/// * `program_dir` - Optional directory to search for debug `.so` files.
///                   Defaults to `target/deploy/` relative to CWD.
///
/// The output format is fixed to the canonical CodeTracer CTFS multi-stream
/// container.
///
/// # Errors
///
/// Returns an error if the transaction cannot be fetched, the program binary
/// cannot be located, or the recording pipeline fails.
pub fn replay_transaction(
    rpc_url: &str,
    signature: &str,
    out_dir: &Path,
    program_dir: Option<&Path>,
) -> Result<()> {
    eprintln!("Fetching transaction {signature} from {rpc_url} ...");

    // 1. Fetch the transaction.
    let tx = rpc_client::fetch_transaction(rpc_url, signature)?;

    if let Some(ref err) = tx.err {
        eprintln!("Warning: transaction failed on-chain with error: {err}");
    }

    eprintln!(
        "Transaction at slot {}: {} account(s), {} instruction(s)",
        tx.slot,
        tx.account_keys.len(),
        tx.instructions.len(),
    );

    // 2. Identify the program(s) involved.
    let program_ids = extract_program_ids(&tx);
    if program_ids.is_empty() {
        bail!("transaction has no instructions; nothing to replay");
    }

    eprintln!("Program IDs: {}", program_ids.join(", "));

    // 3. Fetch current account state for all referenced accounts.
    //    NOTE: This uses *current* state, not historical state at the tx slot.
    eprintln!(
        "Fetching account state for {} account(s) (current state, not historical) ...",
        tx.account_keys.len()
    );

    for key in &tx.account_keys {
        match rpc_client::fetch_account(rpc_url, key) {
            Ok(acct) => {
                eprintln!(
                    "  {}: {} lamports, owner={}, executable={}",
                    key, acct.lamports, acct.owner, acct.executable
                );
            }
            Err(e) => {
                eprintln!("  {}: failed to fetch ({e:#})", key);
            }
        }
    }

    // 4. Locate the program .so file with DWARF debug info.
    let search_dir = program_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/deploy"));

    let program_so = find_program_so(&search_dir, &program_ids)?;
    eprintln!("Using program binary: {}", program_so.display());

    // 5. Construct a synthetic register trace.
    //
    // TODO(M5+): Replace with real SBF VM execution via Mollusk.
    //
    // For now we create a single-step trace so the recorder pipeline can
    // produce a valid (though minimal) trace output.  The register values
    // are zeroed except for the PC.
    let snapshots = build_synthetic_trace(&tx);
    let source_locations = build_synthetic_source_locations(&tx);

    let source_locs_ref: Vec<(u64, &str, u32)> = source_locations
        .iter()
        .map(|(pc, f, l)| (*pc, f.as_str(), *l))
        .collect();

    // 6. Record through the existing pipeline.
    record_from_snapshots(
        &snapshots,
        &source_locs_ref,
        &program_so,
        out_dir,
    )?;

    eprintln!("Trace written to {}", out_dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Search `dir` for a `.so` file that could be the program binary.
///
/// Looks for any `.so` file in the directory.  If multiple exist, the first
/// match alphabetically is used.
fn find_program_so(dir: &Path, _program_ids: &[String]) -> Result<PathBuf> {
    if !dir.exists() {
        bail!(
            "program directory {} does not exist; \
             build your program with `cargo-build-sbf` or pass --program-dir",
            dir.display()
        );
    }

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read directory {}", dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("so") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    candidates.sort();

    candidates.into_iter().next().ok_or_else(|| {
        eyre!(
            "no .so files found in {}; \
             build your program with `cargo-build-sbf` or pass --program-dir",
            dir.display()
        )
    })
}

/// Build a synthetic register trace from a transaction.
///
/// Creates one snapshot per instruction with PC set to the instruction index.
/// All other registers are zeroed.  This is a placeholder until real SBF VM
/// replay (via Mollusk) is integrated.
fn build_synthetic_trace(tx: &TransactionData) -> Vec<RegisterSnapshot> {
    if tx.instructions.is_empty() {
        return vec![];
    }

    tx.instructions
        .iter()
        .enumerate()
        .map(|(i, _ix)| {
            let mut regs = [0u64; 12];
            regs[11] = i as u64; // PC = instruction index
            RegisterSnapshot { registers: regs }
        })
        .collect()
}

/// Build synthetic source locations matching the synthetic trace.
///
/// Each instruction gets a placeholder file and line.
fn build_synthetic_source_locations(tx: &TransactionData) -> Vec<(u64, String, u32)> {
    tx.instructions
        .iter()
        .enumerate()
        .map(|(i, _ix)| {
            let program_idx = _ix.program_id_index as usize;
            let program_id = tx
                .account_keys
                .get(program_idx)
                .map(|s| s.as_str())
                .unwrap_or("unknown_program");
            (
                i as u64,
                format!("{program_id}.rs"),
                (i + 1) as u32,
            )
        })
        .collect()
}
