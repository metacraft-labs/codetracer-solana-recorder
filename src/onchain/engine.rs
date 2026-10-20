//! Executes a replay bundle through the real Solana runtime.
//!
//! The runtime is LiteSVM over Agave's own program runtime and SBF VM, with
//! register tracing compiled into every loaded executable. Nothing here
//! emulates the VM: the instruction stream a trace is written from is the one
//! the VM reports it executed, per invocation, including cross-program
//! invocations.
//!
//! What this module adds on top of the runtime is bookkeeping the trace and
//! the divergence check need:
//!
//! * the whole instruction trace of each transaction (builtin invocations too,
//!   which execute no SBF and so carry no register trace), each entry with its
//!   stack height, so a cross-program invocation can be placed where its
//!   caller made it;
//! * a listing per executed program, keyed by the executable's text, so an
//!   upgraded program gets a new listing rather than the old one's lines;
//! * the account states before and after each transaction, which is what the
//!   chain's recorded balances are compared against and what the next
//!   transaction executes on.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use agave_feature_set::FeatureSet;
use eyre::{Result, eyre};
use litesvm::{InvocationInspectCallback, LiteSVM};
use solana_account::{Account, ReadableAccount};
use solana_address::Address;
use solana_clock::Clock;
use solana_program_runtime::invoke_context::{Executable, InvokeContext};
use solana_transaction::sanitized::SanitizedTransaction;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_context::IndexOfAccount;
use solana_transaction_error::TransactionError;

use super::bundle::{AccountEntry, BundleTransaction, ReplayBundle};
use super::listing::Listing;

/// One entry of a transaction's instruction trace.
#[derive(Debug, Clone)]
pub struct InstructionRecord {
    pub index_in_trace: usize,
    /// 1 for a top-level instruction, 2 for a CPI it made, and so on.
    pub stack_height: usize,
    /// The invoking entry's `index_in_trace`, for a CPI. Agave places every
    /// top-level instruction at the head of the trace and appends CPIs in the
    /// order they were made, so nesting cannot be read off the order; the
    /// runtime's own caller link is what places a CPI under its caller.
    pub caller: Option<usize>,
    pub program_id: String,
    /// The VM's register trace (`[r0..r10, pc]` before each instruction), or
    /// `None` for a builtin program, which runs no SBF.
    pub registers: Option<Vec<[u64; 12]>>,
    /// Key into [`Replay::listings`] for the executable that produced
    /// `registers`.
    pub listing: Option<u64>,
}

/// What one transaction did in the replay.
pub struct TxExecution {
    pub signature: String,
    pub slot: u64,
    /// The runtime's error, or `None` when the transaction succeeded.
    pub error: Option<TransactionError>,
    pub logs: Vec<String>,
    pub compute_units_consumed: u64,
    pub fee: u64,
    /// `(program_id, data)` when the transaction set return data.
    pub return_data: Option<(String, Vec<u8>)>,
    /// Inner instructions per top-level instruction: `(program_id_index,
    /// accounts, data, stack_height)`.
    pub inner_instructions: Vec<Vec<(u8, Vec<u8>, Vec<u8>, u8)>>,
    pub instructions: Vec<InstructionRecord>,
    /// The transaction's account keys in balance order: static keys, then
    /// lookup-table addresses loaded writable, then loaded readonly.
    pub account_keys: Vec<String>,
    pub pre_accounts: Vec<Option<Account>>,
    pub post_accounts: Vec<Option<Account>>,
}

/// Why a transaction was not executed at all.
#[derive(Debug, Clone)]
pub struct NotExecuted {
    pub signature: String,
    pub reason: String,
}

pub enum TxOutcome {
    Executed(Box<TxExecution>),
    NotExecuted(NotExecuted),
}

#[derive(Default)]
struct Capture {
    instructions: Vec<InstructionRecord>,
    listings: HashMap<u64, Arc<Listing>>,
}

struct CaptureCallback(Arc<Mutex<Capture>>);

fn text_key(executable: &Executable) -> u64 {
    // FNV-1a over the relocated text: a new deployment is a new listing.
    let (_, text) = executable.get_text_bytes();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl InvocationInspectCallback for CaptureCallback {
    fn before_invocation(
        &self,
        _svm: &LiteSVM,
        _tx: &SanitizedTransaction,
        _program_indices: &[IndexOfAccount],
        _invoke_context: &mut InvokeContext,
        _register_tracing_enabled: bool,
    ) {
    }

    fn after_invocation(
        &self,
        _svm: &LiteSVM,
        _tx: &SanitizedTransaction,
        _program_indices: &[IndexOfAccount],
        invoke_context: &InvokeContext,
        _register_tracing_enabled: bool,
    ) {
        let tc = &invoke_context.transaction_context;
        let mut records = Vec::new();
        for index in 0..tc.get_instruction_trace_length() {
            let Ok(ictx) = tc.get_instruction_context_at_index_in_trace(index) else {
                continue;
            };
            let program_id = ictx
                .get_program_key()
                .map(|k| k.to_string())
                .unwrap_or_default();
            let height = ictx.get_stack_height();
            records.push(InstructionRecord {
                index_in_trace: index,
                stack_height: height,
                caller: (height > 1).then(|| ictx.get_index_of_caller()),
                program_id,
                registers: None,
                listing: None,
            });
        }
        let traced = std::cell::RefCell::new(Vec::new());
        let new_listings = std::cell::RefCell::new(Vec::new());
        let known: HashSet<u64> = self.0.lock().unwrap().listings.keys().copied().collect();
        invoke_context.iterate_vm_traces(&|ictx, executable, trace| {
            let key = text_key(executable);
            if !known.contains(&key) {
                new_listings
                    .borrow_mut()
                    .push((key, Arc::new(Listing::from_executable(executable))));
            }
            traced
                .borrow_mut()
                .push((ictx.get_index_in_trace(), key, trace.to_vec()));
        });
        for (index, key, regs) in traced.into_inner() {
            if let Some(r) = records.iter_mut().find(|r| r.index_in_trace == index) {
                r.registers = Some(regs);
                r.listing = Some(key);
            }
        }
        let mut cap = self.0.lock().unwrap();
        for (key, listing) in new_listings.into_inner() {
            cap.listings.entry(key).or_insert(listing);
        }
        cap.instructions = records;
    }
}

pub struct Replay {
    svm: LiteSVM,
    capture: Arc<Mutex<Capture>>,
    /// Every account whose state this replay holds: supplied by the bundle,
    /// or written by a transaction it executed. A transaction naming an
    /// account outside this set is not executed — an account the replay does
    /// not hold would be read as nonexistent, which is a prestate guess.
    held: HashSet<Address>,
    base_clock: Clock,
}

fn parse_address(s: &str) -> Result<Address> {
    Address::from_str(s).map_err(|e| eyre!("{s:?} is not an address: {e}"))
}

fn to_account(entry: &AccountEntry) -> Result<Account> {
    Ok(Account {
        lamports: entry.lamports,
        data: entry.data_bytes()?,
        owner: parse_address(&entry.owner)?,
        executable: entry.executable,
        rent_epoch: entry.rent_epoch,
    })
}

/// Programs the runtime provides itself, and sysvars it constructs. Neither
/// is carried state, so neither needs to be in a bundle.
fn runtime_provided(key: &Address, svm: &LiteSVM) -> bool {
    let s = key.to_string();
    s.starts_with("Sysvar")
        || svm
            .get_account(key)
            .is_some_and(|a| a.executable && a.owner == solana_sdk_ids::native_loader::id())
}

impl Replay {
    pub fn new(bundle: &ReplayBundle) -> Result<Self> {
        let mut feature_set = FeatureSet::default();
        for f in &bundle.features {
            if let Some(slot) = f.activated_at {
                feature_set.activate(&parse_address(&f.id)?, slot);
            }
        }
        let capture = Arc::new(Mutex::new(Capture::default()));
        let mut svm = LiteSVM::new_debuggable(true)
            .with_feature_set(feature_set)
            .with_builtins()
            .with_precompiles()
            .with_sigverify(false)
            .with_blockhash_check(false);
        svm.set_invocation_inspect_callback(CaptureCallback(capture.clone()));

        for entry in &bundle.sysvars {
            svm.set_account(parse_address(&entry.pubkey)?, to_account(entry)?)
                .map_err(|e| eyre!("sysvar {}: {e:?}", entry.pubkey))?;
        }
        let base_clock: Clock = svm.get_sysvar();

        let mut held = HashSet::new();
        // Data accounts first, executables last: an upgradeable program is
        // loaded from its programdata account at the moment the program
        // account is set, so the programdata must already be there.
        let (programs, data): (Vec<_>, Vec<_>) = bundle.accounts.iter().partition(|a| a.executable);
        for entry in data.into_iter().chain(programs) {
            let key = parse_address(&entry.pubkey)?;
            svm.set_account(key, to_account(entry)?)
                .map_err(|e| eyre!("account {}: {e:?}", entry.pubkey))?;
            held.insert(key);
        }
        Ok(Self {
            svm,
            capture,
            held,
            base_clock,
        })
    }

    pub fn listing(&self, key: u64) -> Option<Arc<Listing>> {
        self.capture.lock().unwrap().listings.get(&key).cloned()
    }

    /// The account's state as the replay holds it now.
    pub fn account(&self, key: &str) -> Option<Account> {
        parse_address(key)
            .ok()
            .and_then(|k| self.svm.get_account(&k))
    }

    pub fn held_keys(&self) -> Vec<String> {
        let mut v: Vec<String> = self.held.iter().map(|k| k.to_string()).collect();
        v.sort();
        v
    }

    pub fn execute(&mut self, tx: &BundleTransaction) -> Result<TxOutcome> {
        let wire = tx.wire_bytes()?;
        let vtx: VersionedTransaction = wincode::deserialize(&wire).map_err(|e| {
            eyre!(
                "transaction {}: wire bytes do not decode: {e}",
                tx.signature
            )
        })?;

        // Balance order: static keys, then loaded writable, then loaded readonly.
        // The lookup tables are resolved from the replay's own state, and the
        // chain's record says what they resolved to on chain; the two are
        // compared by the oracle rather than trusted here.
        let mut keys: Vec<Address> = vtx.message.static_account_keys().to_vec();
        if let Some(meta) = &tx.meta {
            for part in ["writable", "readonly"] {
                if let Some(list) = meta["loadedAddresses"][part].as_array() {
                    for k in list {
                        keys.push(parse_address(k.as_str().unwrap_or_default())?);
                    }
                }
            }
        }

        let missing: Vec<String> = keys
            .iter()
            .filter(|k| !self.held.contains(k) && !runtime_provided(k, &self.svm))
            .map(|k| k.to_string())
            .collect();
        if !missing.is_empty() {
            return Ok(TxOutcome::NotExecuted(NotExecuted {
                signature: tx.signature.clone(),
                reason: format!(
                    "prestate-unavailable: the replay holds no state for {} account(s): {}",
                    missing.len(),
                    missing.join(", ")
                ),
            }));
        }

        let mut clock = self.base_clock.clone();
        clock.slot = tx.slot;
        if let Some(t) = tx.block_time {
            clock.unix_timestamp = t;
        }
        self.svm.set_sysvar(&clock);

        let pre_accounts: Vec<Option<Account>> =
            keys.iter().map(|k| self.svm.get_account(k)).collect();

        self.capture.lock().unwrap().instructions.clear();
        let result = self.svm.send_transaction(vtx);
        let (error, meta) = match result {
            Ok(meta) => (None, meta),
            Err(failed) => (Some(failed.err), failed.meta),
        };

        // A failed transaction is not committed by LiteSVM, but on chain it
        // still pays its fee: the fee payer (key 0) is debited, nothing else
        // changes. Apply exactly that, so the next transaction sees the chain's
        // state rather than the simulator's.
        if error.is_some() && meta.fee > 0 {
            if let Some(mut payer) = self.svm.get_account(&keys[0]) {
                payer.lamports = payer.lamports.saturating_sub(meta.fee);
                self.svm
                    .set_account(keys[0], payer)
                    .map_err(|e| eyre!("fee payer: {e:?}"))?;
            }
        }

        let post_accounts: Vec<Option<Account>> =
            keys.iter().map(|k| self.svm.get_account(k)).collect();
        for k in &keys {
            if self.svm.get_account(k).is_some() {
                self.held.insert(*k);
            }
        }

        let instructions = std::mem::take(&mut self.capture.lock().unwrap().instructions);
        let return_data = if meta.return_data.data.is_empty() {
            None
        } else {
            Some((
                meta.return_data.program_id.to_string(),
                meta.return_data.data.clone(),
            ))
        };
        let inner_instructions = meta
            .inner_instructions
            .iter()
            .map(|list| {
                list.iter()
                    .map(|ii| {
                        (
                            ii.instruction.program_id_index,
                            ii.instruction.accounts.clone(),
                            ii.instruction.data.clone(),
                            ii.stack_height,
                        )
                    })
                    .collect()
            })
            .collect();

        Ok(TxOutcome::Executed(Box::new(TxExecution {
            signature: tx.signature.clone(),
            slot: tx.slot,
            error,
            logs: meta.logs,
            compute_units_consumed: meta.compute_units_consumed,
            fee: meta.fee,
            return_data,
            inner_instructions,
            instructions,
            account_keys: keys.iter().map(|k| k.to_string()).collect(),
            pre_accounts,
            post_accounts,
        })))
    }

    pub fn account_entry(&self, key: &str, read_at_slot: Option<u64>) -> Option<AccountEntry> {
        use base64::Engine as _;
        self.account(key).map(|a| AccountEntry {
            pubkey: key.to_string(),
            lamports: a.lamports(),
            owner: a.owner().to_string(),
            executable: a.executable(),
            rent_epoch: a.rent_epoch(),
            data: base64::engine::general_purpose::STANDARD.encode(a.data()),
            read_at_slot,
        })
    }
}
