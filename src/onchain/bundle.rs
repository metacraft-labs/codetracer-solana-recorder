//! The replay bundle: everything one on-chain replay needs, in one file.
//!
//! A bundle is written by whatever captured the chain (BlockTracer's Solana
//! producer, or the `replay` subcommand for a single signature) and read here
//! with no network access. Keeping the replay offline is what makes a
//! divergence fixture self-contained: the same bundle replays to the same
//! verdict on any machine, and CI can pin it.
//!
//! The bundle says nothing about how its account states were obtained beyond
//! the slot each was read at. Whether those states ARE the transaction's
//! prestate is exactly what the divergence check measures; the bundle does
//! not get to assert it.

use std::path::Path;

use base64::Engine as _;
use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The `format` member a bundle must carry, verbatim.
pub const BUNDLE_FORMAT: &str = "codetracer-solana/replay-bundle@1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayBundle {
    pub format: String,
    /// `mainnet-beta`, `devnet`, ... Informational; the replay does not branch on it.
    pub cluster: String,
    /// Sysvar accounts as read from the chain. `Clock` is overridden per
    /// transaction from the transaction's own slot and block time.
    #[serde(default)]
    pub sysvars: Vec<AccountEntry>,
    /// Feature-gate accounts as read from the chain. The runtime's active
    /// feature set is built from these and from nothing else, so a replay
    /// runs the features the cluster had rather than a library's guess.
    pub features: Vec<FeatureEntry>,
    /// Every account the transactions may touch, in the state the capture holds.
    pub accounts: Vec<AccountEntry>,
    /// Executed in order, each against the state the previous one left.
    pub transactions: Vec<BundleTransaction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureEntry {
    pub id: String,
    /// The slot the feature activated at; `None` for a feature account that
    /// exists but is pending, which leaves the feature inactive.
    pub activated_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountEntry {
    pub pubkey: String,
    pub lamports: u64,
    pub owner: String,
    pub executable: bool,
    #[serde(default)]
    pub rent_epoch: u64,
    /// Base64 of the account data.
    pub data: String,
    /// The slot the state was read at (`context.slot` of the RPC answer), or
    /// the slot of the replay that produced it. Carried for the reader, not
    /// used by the replay.
    #[serde(default)]
    pub read_at_slot: Option<u64>,
}

impl AccountEntry {
    pub fn data_bytes(&self) -> Result<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .with_context(|| format!("account {}: data is not base64", self.pubkey))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleTransaction {
    pub signature: String,
    pub slot: u64,
    #[serde(default)]
    pub index_in_block: Option<u32>,
    #[serde(default)]
    pub block_time: Option<i64>,
    /// Base64 of the transaction's wire bytes (`getTransaction` with
    /// `encoding: "base64"`), so the replay executes the exact message the
    /// chain did, including v0 lookup-table and v1 forms.
    pub wire: String,
    /// The chain's own record of the execution: `meta` from `getTransaction`
    /// with `encoding: "json"`, verbatim. `None` makes every verdict `unchecked`.
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
    /// Write a trace for this transaction. Untraced transactions still run,
    /// because later transactions read the state they leave.
    #[serde(default)]
    pub trace: bool,
}

impl BundleTransaction {
    pub fn wire_bytes(&self) -> Result<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.wire)
            .with_context(|| format!("transaction {}: wire is not base64", self.signature))
    }
}

pub fn read_bundle(path: &Path) -> Result<ReplayBundle> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read bundle {}", path.display()))?;
    let bundle: ReplayBundle = serde_json::from_str(&text)
        .with_context(|| format!("bundle {} is not a replay bundle", path.display()))?;
    if bundle.format != BUNDLE_FORMAT {
        bail!(
            "bundle {} has format {:?}; this recorder reads {:?} only",
            path.display(),
            bundle.format,
            BUNDLE_FORMAT
        );
    }
    Ok(bundle)
}
