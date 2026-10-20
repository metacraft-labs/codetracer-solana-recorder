//! The real SBF VM path, under test, against pinned mainnet transactions.
//!
//! Each fixture under `tests/fixtures/onchain/` is a replay bundle built by
//! `codetracer-solana-recorder replay --signature <sig>` on 2026-10-20: the
//! transaction's wire bytes and the cluster's recorded metadata, the program
//! code, sysvars and feature accounts, and every account the transaction
//! names in its CURRENT state at capture time. The tests need no network and
//! no toolchain beyond cargo, so none of them can skip.
//!
//! * `p-token-transfer` — slot 459241795, a token transfer through SPL Token
//!   (p-token), 76 SBF instructions, no CPI.
//! * `ata-create-cpi` — slot 459244245, two Associated Token Account
//!   instructions whose first makes four CPIs (three into SPL Token, one into
//!   the System program, a builtin), followed by three SPL Token instructions.
//!
//! Current state is a later POST-state for every account those transactions
//! wrote, so the bundles as pinned are the stale-prestate case the divergence
//! check exists to catch. The control for each is the same bundle with the
//! pre-facts the chain recorded (lamports from `preBalances`, token amounts
//! from `preTokenBalances`) written over the stale values. For these two
//! transactions those are the only fields that changed since, and the replay
//! then agrees with every fact the chain recorded, including compute units
//! and the full log. The patch is a test construction, not a prestate route:
//! nothing outside this file applies it.

use std::path::PathBuf;

use base64::Engine as _;
use codetracer_solana_recorder::onchain::bundle::{ReplayBundle, read_bundle};
use codetracer_solana_recorder::onchain::engine::{Replay, TxExecution, TxOutcome};
use codetracer_solana_recorder::onchain::oracle::{self, Cause, Status, Verdict};

fn fixture(name: &str) -> ReplayBundle {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/onchain")
        .join(format!("{name}.bundle.json"));
    read_bundle(&path).expect("fixture bundle reads")
}

fn run(bundle: &ReplayBundle) -> (TxExecution, Verdict) {
    let mut replay = Replay::new(bundle).expect("replay builds");
    let tx = &bundle.transactions[0];
    match replay.execute(tx).expect("replay executes") {
        TxOutcome::Executed(exec) => {
            let verdict = oracle::check(&exec, tx.meta.as_ref());
            (*exec, verdict)
        }
        TxOutcome::NotExecuted(n) => panic!("not executed: {}", n.reason),
    }
}

/// The bundle with the chain's recorded pre-facts written over its stale
/// account state (see the module comment).
fn with_recorded_prefacts(bundle: &ReplayBundle, keys: &[String]) -> ReplayBundle {
    let mut patched = bundle.clone();
    let meta = bundle.transactions[0].meta.clone().unwrap();
    let pre: Vec<u64> = meta["preBalances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    for (key, lamports) in keys.iter().zip(&pre) {
        let entry = patched
            .accounts
            .iter_mut()
            .find(|a| &a.pubkey == key)
            .expect("every key is in the bundle");
        entry.lamports = *lamports;
    }
    for tb in meta["preTokenBalances"].as_array().unwrap() {
        let key = &keys[tb["accountIndex"].as_u64().unwrap() as usize];
        let amount: u64 = tb["uiTokenAmount"]["amount"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let entry = patched
            .accounts
            .iter_mut()
            .find(|a| &a.pubkey == key)
            .unwrap();
        let mut data = entry.data_bytes().unwrap();
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        entry.data = base64::engine::general_purpose::STANDARD.encode(&data);
    }
    patched
}

fn failed(v: &Verdict) -> Vec<&'static str> {
    v.facts
        .iter()
        .filter(|f| !f.matched)
        .map(|f| f.name)
        .collect()
}

const EXECUTION_FACTS: &[&str] = &[
    "err",
    "computeUnitsConsumed",
    "fee",
    "logMessages",
    "innerInstructions",
    "returnData",
];

#[test]
fn test_a_stale_prestate_replay_is_caught_as_divergent() {
    for name in ["p-token-transfer", "ata-create-cpi"] {
        let bundle = fixture(name);
        let (exec, stale) = run(&bundle);
        assert_eq!(stale.status, Status::Divergent, "{name}: {:?}", stale.facts);
        assert_eq!(stale.cause, Some(Cause::Prestate), "{name}");
        assert!(failed(&stale).contains(&"preBalances"), "{name}");

        // Control: the same transaction on the chain's own prestate matches,
        // so the check detects exactly the stale state and not something else.
        let (_, good) = run(&with_recorded_prefacts(&bundle, &exec.account_keys));
        assert_eq!(good.status, Status::Match, "{name}: {:?}", good.facts);
        assert!(good.cause.is_none());
    }
}

#[test]
fn test_the_real_vm_reproduces_every_recorded_execution_fact() {
    for name in ["p-token-transfer", "ata-create-cpi"] {
        let bundle = fixture(name);
        let (exec, _) = run(&bundle);
        let (exec, verdict) = run(&with_recorded_prefacts(&bundle, &exec.account_keys));
        for fact in EXECUTION_FACTS {
            let f = verdict
                .facts
                .iter()
                .find(|f| f.name == *fact)
                .unwrap_or_else(|| panic!("{name}: {fact} was not compared"));
            assert!(f.matched, "{name}: {fact}: {:?}", f.detail);
        }
        // The register trace is the VM's own: every SBF invocation has one,
        // and every builtin has none.
        for r in &exec.instructions {
            let builtin = r.program_id == "11111111111111111111111111111111"
                || r.program_id.starts_with("ComputeBudget");
            assert_eq!(r.registers.is_none(), builtin, "{name}: {}", r.program_id);
        }
    }
}

#[test]
fn test_the_vm_path_test_fails_when_the_vm_path_breaks() {
    // The defect: the instruction at the SPL Token executable's entrypoint is
    // overwritten with `exit` (0x95). The program then returns at its first
    // step on the real VM, so a suite that runs the real path must see the
    // execution differ.
    let bundle = fixture("p-token-transfer");
    let (exec, _) = run(&bundle);
    let good = with_recorded_prefacts(&bundle, &exec.account_keys);
    let (_, control) = run(&good);
    assert_eq!(control.status, Status::Match, "{:?}", control.facts);

    let mut broken = good.clone();
    let programdata = broken
        .accounts
        .iter_mut()
        .find(|a| a.owner == "BPFLoaderUpgradeab1e11111111111111111111111" && !a.executable)
        .expect("the SPL Token programdata account");
    let mut data = programdata.data_bytes().unwrap();
    let elf = &data[45..];
    let entry = entrypoint_offset(elf).expect("the ELF entrypoint lies in .text");
    data[45 + entry..45 + entry + 8].copy_from_slice(&[0x95, 0, 0, 0, 0, 0, 0, 0]);
    programdata.data = base64::engine::general_purpose::STANDARD.encode(&data);

    let (_, verdict) = run(&broken);
    assert_eq!(verdict.status, Status::Divergent, "{:?}", verdict.facts);
    assert_eq!(verdict.cause, Some(Cause::Execution));
}

#[test]
fn test_a_cpi_target_appears_in_the_trace() {
    let bundle = fixture("ata-create-cpi");
    let (exec, _) = run(&bundle);
    let ata = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
    let token = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    let first = &exec.instructions[0];
    assert_eq!(first.program_id, ata);
    let children: Vec<_> = exec
        .instructions
        .iter()
        .filter(|r| r.caller == Some(first.index_in_trace))
        .collect();
    assert_eq!(
        children
            .iter()
            .map(|r| r.program_id.as_str())
            .collect::<Vec<_>>(),
        [token, "11111111111111111111111111111111", token, token],
    );
    // The callee's steps are the callee's own register trace, not the caller's.
    assert!(
        children[0]
            .registers
            .as_ref()
            .is_some_and(|r| !r.is_empty())
    );

    // Control: the transfer makes no CPI, and every invocation is top-level.
    let (plain, _) = run(&fixture("p-token-transfer"));
    assert!(plain.instructions.iter().all(|r| r.caller.is_none()));
}

#[test]
fn test_a_check_with_nothing_to_compare_fails() {
    let mut bundle = fixture("p-token-transfer");
    let (exec, _) = run(&bundle);
    let verdict = oracle::check(&exec, None);
    assert_eq!(verdict.status, Status::Unchecked);

    // Control: with the facts present the same execution gets a verdict.
    bundle.transactions[0].meta = fixture("p-token-transfer").transactions[0].meta.clone();
    let (_, v) = run(&bundle);
    assert_ne!(v.status, Status::Unchecked);
}

#[test]
fn test_an_account_the_replay_does_not_hold_refuses_execution() {
    let mut bundle = fixture("p-token-transfer");
    // Drop the sender's token account: the replay must refuse, not read it as
    // nonexistent.
    let (exec, _) = run(&bundle);
    let victim = exec.account_keys[1].clone();
    bundle.accounts.retain(|a| a.pubkey != victim);
    let mut replay = Replay::new(&bundle).unwrap();
    match replay.execute(&bundle.transactions[0]).unwrap() {
        TxOutcome::NotExecuted(n) => {
            assert!(n.reason.starts_with("prestate-unavailable"), "{}", n.reason);
            assert!(n.reason.contains(&victim));
        }
        TxOutcome::Executed(_) => panic!("executed with an account it does not hold"),
    }
}

#[test]
fn test_the_trace_follows_the_vm_and_its_cpis() {
    let bundle = fixture("ata-create-cpi");
    let (exec, _) = run(&bundle);
    let dir = tempfile::tempdir().unwrap();
    let mut replay = Replay::new(&bundle).unwrap();
    let TxOutcome::Executed(exec2) = replay.execute(&bundle.transactions[0]).unwrap() else {
        panic!()
    };
    let listings = exec2
        .instructions
        .iter()
        .filter_map(|r| r.listing)
        .filter_map(|k| replay.listing(k).map(|l| (k, l)))
        .collect();
    let written = codetracer_solana_recorder::onchain::ctfs::write_transaction_trace(
        &exec2,
        &listings,
        dir.path(),
        &codetracer_solana_recorder::onchain::ctfs::TraceOptions { registers: false },
    )
    .unwrap();
    // One step per SBF instruction the VM executed, one per builtin
    // invocation, and one for the transaction frame.
    let sbf: u64 = exec
        .instructions
        .iter()
        .filter_map(|r| r.registers.as_ref().map(|v| v.len() as u64))
        .sum();
    let builtins = exec
        .instructions
        .iter()
        .filter(|r| r.registers.is_none())
        .count() as u64;
    assert_eq!(written.steps, sbf + builtins + 1);
    let tree = std::fs::read_to_string(dir.path().join("transaction.txt")).unwrap();
    assert!(
        tree.contains("\n  [6] 11111111111111111111111111111111 (builtin)"),
        "{tree}"
    );
}

/// File offset of the instruction at an ELF64 image's entrypoint.
fn entrypoint_offset(elf: &[u8]) -> Option<usize> {
    let u16_at = |o: usize| u16::from_le_bytes(elf[o..o + 2].try_into().unwrap()) as usize;
    let u64_at = |o: usize| u64::from_le_bytes(elf[o..o + 8].try_into().unwrap()) as usize;
    let u32_at = |o: usize| u32::from_le_bytes(elf[o..o + 4].try_into().unwrap()) as usize;
    let shoff = u64_at(0x28);
    let shentsize = u16_at(0x3a);
    let shnum = u16_at(0x3c);
    let shstrndx = u16_at(0x3e);
    let strtab = u64_at(shoff + shstrndx * shentsize + 0x18);
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        let name = strtab + u32_at(sh);
        if elf[name..].starts_with(b".text\0") {
            let (addr, offset) = (u64_at(sh + 0x10), u64_at(sh + 0x18));
            return u64_at(0x18).checked_sub(addr).map(|d| offset + d);
        }
    }
    None
}

#[test]
fn test_a_refused_transaction_stops_the_replay_holding_what_it_writes() {
    use codetracer_solana_recorder::onchain::{Summary, ctfs::TraceOptions, replay_stream};
    let bundle = fixture("p-token-transfer");
    let (exec, _) = run(&bundle);
    let good = with_recorded_prefacts(&bundle, &exec.account_keys);
    let sender = exec.account_keys[1].clone();
    let tx = serde_json::to_value(&good.transactions[0]).unwrap();
    let mut tx_op = tx.clone();
    tx_op["op"] = "tx".into();

    let stream = |accounts: Vec<&codetracer_solana_recorder::onchain::bundle::AccountEntry>,
                  ops: &[serde_json::Value]|
     -> (Vec<serde_json::Value>, Summary) {
        let mut input = String::new();
        input +=
            &serde_json::json!({"op": "init", "sysvars": good.sysvars, "features": good.features})
                .to_string();
        input += "\n";
        input += &serde_json::json!({"op": "accounts", "accounts": accounts}).to_string();
        input += "\n";
        for op in ops {
            input += &op.to_string();
            input += "\n";
        }
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let s = replay_stream(
            input.as_bytes(),
            &mut out,
            dir.path(),
            &TraceOptions { registers: false },
        )
        .unwrap();
        let lines = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (lines, s)
    };

    // Without the sender's token account the transaction is refused, and the
    // accounts it writes stop being held: supplying the missing one does not
    // make the transaction runnable, because its other writable accounts are
    // now unknown too.
    let without: Vec<_> = good
        .accounts
        .iter()
        .filter(|a| a.pubkey != sender)
        .collect();
    let only_sender: Vec<_> = good
        .accounts
        .iter()
        .filter(|a| a.pubkey == sender)
        .collect();
    let (lines, s) = stream(
        without,
        &[
            tx_op.clone(),
            serde_json::json!({"op": "accounts", "accounts": only_sender}),
            tx_op.clone(),
        ],
    );
    assert_eq!(lines[2]["executed"], false, "{}", lines[2]);
    assert!(lines[2]["evicted"].as_u64().unwrap() > 0, "{}", lines[2]);
    assert_eq!(lines[4]["executed"], false, "{}", lines[4]);
    assert_eq!(s.matched, 0);

    // Control: with every account held from the start, the same stream matches.
    let (lines, s) = stream(good.accounts.iter().collect(), &[tx_op]);
    assert_eq!(lines[2]["verdict"]["status"], "match", "{}", lines[2]);
    assert_eq!(s.matched, 1);
}
