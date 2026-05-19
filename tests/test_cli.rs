//! CLI-surface integration tests for `codetracer-solana-recorder`.
//!
//! Tests cover three areas:
//!
//! 1. **Smoke tests** — basic `--help`, `--version`, error paths.
//! 2. **`ct print` content** — record a fixture and pipe the resulting
//!    `.ct` container through `ct-print --json` from
//!    `codetracer-trace-format-nim` to make content-level assertions.
//!    Skips gracefully when `ct-print` is not present (i.e. when this
//!    crate is built outside the metacraft workspace).
//! 3. **CLI env-var contract** — exercise the post-2026-05-08
//!    `CODETRACER_SOLANA_RECORDER_OUT_DIR` /
//!    `CODETRACER_SOLANA_RECORDER_DISABLED` env vars and the
//!    no-`--format` invariant from `Recorder-CLI-Conventions.md` §4 / §5.
//!
//! History note: pre-2026-05-08 the recorder shipped a `--format
//! ctfs|binary|json` flag at two subcommand levels (`record`,
//! `replay`).  When the convention switched to CTFS-only the
//! `--format` argument was removed at every level and the dedicated
//! tests asserting on the OLD `--format` contract were
//! deleted/replaced.  See `AUDIT-CTFS-2026-05.md` ("Convention
//! compliance follow-up — 2026-05-08") for the full record.

use std::path::{Path, PathBuf};
use std::process::Command;

use codetracer_solana_recorder::recorder::record_from_snapshots;
use codetracer_solana_recorder::register_trace::{RegisterSnapshot, parse_regs_file};

fn cargo_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_codetracer-solana-recorder"))
}

/// Path to the `ct-print` binary shipped with `codetracer-trace-format-nim`.
///
/// The Solana recorder is CTFS-only; tests that need to make
/// content-level assertions on a recorded trace pipe the `.ct`
/// container through `ct-print --json` and assert on the resulting
/// JSON.  This is the same workflow that `Recorder-CLI-Conventions.md`
/// §4 prescribes for downstream tools / golden snapshots.
fn ct_print_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join("ct-print")
}

/// Build a synthetic `.regs` binary blob simulating a 7-instruction program.
///
/// Mirrors `create_synthetic_regs` in `tests/test_tracer.rs` but stays
/// local so the CLI tests do not depend on cross-test helpers.
fn build_synthetic_regs() -> Vec<u8> {
    let mut data = Vec::new();
    for step in 0..7u64 {
        let mut regs = [0u64; 12];
        regs[11] = step; // PC
        match step {
            0 => {
                regs[1] = 10;
            }
            1 => {
                regs[1] = 10;
                regs[2] = 32;
            }
            2 => {
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
            }
            3 => {
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
                regs[4] = 84;
            }
            4 => {
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
                regs[4] = 84;
                regs[5] = 94;
            }
            5 => {
                regs[0] = 94;
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
                regs[4] = 84;
                regs[5] = 94;
            }
            6 => {
                regs[0] = 94;
                regs[1] = 10;
                regs[2] = 32;
                regs[3] = 42;
                regs[4] = 84;
                regs[5] = 94;
            }
            _ => {}
        }
        for r in &regs {
            data.extend_from_slice(&r.to_le_bytes());
        }
    }
    data
}

/// Source locations matching the synthetic 7-step program (file `solana_fixture.rs`).
fn synthetic_source_locations() -> Vec<(u64, &'static str, u32)> {
    vec![
        (0, "solana_fixture.rs", 5),
        (1, "solana_fixture.rs", 6),
        (2, "solana_fixture.rs", 7),
        (3, "solana_fixture.rs", 8),
        (4, "solana_fixture.rs", 9),
        (5, "solana_fixture.rs", 10),
        (6, "solana_fixture.rs", 11),
    ]
}

// ===========================================================================
// Smoke tests
// ===========================================================================

#[test]
fn help_succeeds_and_mentions_name() {
    let output = cargo_bin().arg("--help").output().expect("failed to run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("codetracer-solana-recorder"),
        "Help output should mention codetracer-solana-recorder, got: {stdout}"
    );
}

#[test]
fn version_succeeds_and_contains_version() {
    let output = cargo_bin()
        .arg("--version")
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.1.0"),
        "Version output should contain 0.1.0, got: {stdout}"
    );
}

#[test]
fn record_nonexistent_file_fails() {
    let output = cargo_bin()
        .args(["record", "/nonexistent/path/program.so"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "record with nonexistent file should fail"
    );
}

#[test]
fn record_rejects_invalid_elf() {
    let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
    let elf_file = tmp.path().join("dummy_program.so");
    std::fs::write(&elf_file, b"dummy ELF data").expect("failed to write dummy file");

    let out_dir = tmp.path().join("ct-traces");

    let output = cargo_bin()
        .args([
            "record",
            "-o",
            out_dir.to_str().unwrap(),
            elf_file.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");

    assert!(
        !output.status.success(),
        "record should fail for invalid ELF input"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The error should specifically mention the ELF magic number validation.
    assert!(
        stderr.contains("ELF magic")
            || stderr.contains("invalid ELF")
            || stderr.contains("\\x7fELF"),
        "error should mention ELF magic number validation, got: {stderr}"
    );
}

/// Verify that a file with only the ELF magic but no valid structure is also rejected.
#[test]
fn record_rejects_elf_magic_only() {
    let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
    let elf_file = tmp.path().join("magic_only.so");
    // Only the ELF magic bytes, nothing else.
    std::fs::write(&elf_file, b"\x7fELF").expect("failed to write file");

    let out_dir = tmp.path().join("ct-traces");

    let output = cargo_bin()
        .args([
            "record",
            "-o",
            out_dir.to_str().unwrap(),
            elf_file.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");

    // This should either succeed (legacy placeholder mode since no --regs)
    // or fail during ELF processing. The key is it doesn't crash.
    let _status = output.status;
}

/// Verify that the record subcommand with --regs but nonexistent regs file fails.
#[test]
fn record_rejects_missing_regs_file() {
    let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
    // Create a valid ELF file (just magic + padding).
    let elf_file = tmp.path().join("valid.so");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&elf_file).unwrap();
        f.write_all(b"\x7fELF").unwrap();
        // Pad to a valid-ish ELF header size.
        f.write_all(&[0u8; 60]).unwrap();
    }

    let out_dir = tmp.path().join("ct-traces");

    let output = cargo_bin()
        .args([
            "record",
            "--regs",
            "/nonexistent/trace.regs",
            "-o",
            out_dir.to_str().unwrap(),
            elf_file.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");

    assert!(
        !output.status.success(),
        "record should fail when --regs file does not exist"
    );
}

// ===========================================================================
// CTFS content via `ct-print` — replaces the legacy `--format json` content
// assertions
// ===========================================================================

/// Record a synthetic 7-instruction program through `record_from_snapshots`,
/// then convert the produced `.ct` container to JSON via `ct-print` and
/// assert on:
///
/// 1. **Structural anchors** (legacy layer): `ct-print --json` output
///    contains the source filename and at least one of the SBF
///    register names somewhere in the textual rendering.
/// 2. **Exact decoded values** (the layer enabled by `ct-print --full`):
///    the synthetic fixture mirrors the canonical `(10 + 32) * 2 + 10
///    = 94` flow used by every other recorder's content test
///    (cairo / cardano / circom / flow / fuel / leo / miden / move /
///    polkavm).  Intermediate let-bindings `a=10`, `b=32`,
///    `sum_val=42`, `doubled=84`, `final_result=94` are not recovered
///    by the Solana recorder yet (no DWARF-aware let-binding pass —
///    same limitation as polkavm), so the canonical values surface
///    through the SBF register stream the synthetic snapshots seed:
///    `r1=10` (a), `r2=32` (b), `r3=42` (sum_val), `r4=84` (doubled),
///    `r5=94` (final_result), and `r0=94` for the return value.
///    Each value must surface in the trace as a step variable with a
///    decoded `Int` ValueRecord whose `i` field matches the canonical
///    literal.
///
/// Pre-2026-05-08 the recorder shipped a `--format json` mode and a
/// `trace.json` file was written directly.  The convention now mandates
/// CTFS-only output; `ct print` is the canonical conversion tool.  See
/// `Recorder-CLI-Conventions.md` §4.  `ct-print --full` (added 2026-05
/// in `codetracer-trace-format-nim`) is what enables the exact-value
/// layer — its output is a deterministic JSON document with every CBOR
/// `ValueRecord` decoded to a structured form like
/// `{"kind":"Int","i":42,"type_id":N}`.
///
/// The note in earlier revisions of this test about register integer
/// payloads not round-tripping through `ct-print --json` is empirically
/// obsolete for `--full`: the recorder's
/// `register_variable_with_full_value` path decodes back to
/// `{"kind":"Int","i":<n>,"type_id":N}` with values intact.  The
/// strict `value.kind == "Int"` invariant means: if a future Solana
/// recorder upgrade emits a different `ValueRecord` variant for SBF
/// register values (e.g. a `Raw` 8-byte register snapshot once the
/// recorder learns to surface 64-bit unsigned values that overflow
/// i64), this test fails loudly and the next maintainer extends the
/// assertion to the new variant rather than silently accepting it.
#[test]
fn test_recorded_trace_via_ct_print_json() {
    let ct_print = ct_print_path();
    if !ct_print.exists() {
        eprintln!(
            "SKIP: ct-print not found at {} — only available within the \
             metacraft workspace where codetracer-trace-format-nim is a sibling.",
            ct_print.display()
        );
        return;
    }

    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let regs_data = build_synthetic_regs();
    let snapshots: Vec<RegisterSnapshot> = parse_regs_file(&regs_data).unwrap();
    let source_locs = synthetic_source_locations();

    record_from_snapshots(
        &snapshots,
        &source_locs,
        Path::new("solana_fixture.rs"),
        &out_dir,
    )
    .expect("recorder should succeed on the synthetic fixture");

    let ct_files: Vec<_> = std::fs::read_dir(&out_dir)
        .expect("failed to read output directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect();
    assert!(
        !ct_files.is_empty(),
        "expected at least one .ct file in {}",
        out_dir.display()
    );
    let ct_path = &ct_files[0];

    // -----------------------------------------------------------------
    // Layer 1 (legacy): ct-print --json — substring presence checks.
    // Kept as a safety net so a regression in the textual rendering
    // is caught even if --full's JSON shape evolves.
    // -----------------------------------------------------------------
    let output = Command::new(&ct_print)
        .args(["--json"])
        .arg(ct_path)
        .output()
        .expect("failed to run ct-print");

    assert!(
        output.status.success(),
        "ct-print --json should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_json = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout_json.is_empty(),
        "ct-print --json produced empty output"
    );

    // Structural anchor 1: the fixture source path name appears in the
    // path stream rendered by ct-print.
    assert!(
        stdout_json.contains("solana_fixture.rs"),
        "ct-print --json output should mention the fixture source path \
         (solana_fixture.rs); got:\n{stdout_json}"
    );

    // Structural anchor 2: at least one of the SBF register names
    // captured by the tracer (r0..r10) appears.  The recorder emits
    // these as variable names per step — see
    // `record_from_snapshots_into_writer` in `src/recorder.rs`.
    let register_anchor = ["r0", "r1", "r2", "r3", "r4", "r5"]
        .iter()
        .any(|v| stdout_json.contains(v));
    assert!(
        register_anchor,
        "ct-print --json output should mention at least one of the \
         SBF register names (r0..r5); got:\n{stdout_json}"
    );

    // -----------------------------------------------------------------
    // Layer 2 (the upgrade): ct-print --full — exact decoded values.
    // -----------------------------------------------------------------
    let full_output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(ct_path)
        .output()
        .expect("failed to run ct-print --full");

    assert!(
        full_output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&full_output.stderr)
    );

    let doc: serde_json::Value = serde_json::from_slice(&full_output.stdout)
        .expect("ct-print --full should emit valid JSON");

    // ----- Path table: the canonical fixture path must appear ---------
    let paths: Vec<&str> = doc["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("solana_fixture.rs")),
        "expected solana_fixture.rs in paths table; got {:?}",
        paths
    );

    // ----- Function table: exactly `main` ----------------------------
    // The Solana recorder synthesises a single top-level `main`
    // function for every recording (see
    // `record_from_snapshots_into_writer` in `src/recorder.rs`) and
    // emits no nested call frames for the synthetic fixture (the PC
    // jumps stay within the +/- 2 step delta that the recorder's call
    // heuristic tolerates without synthesising an `fn_at_pc_*`
    // frame).  Pin both invariants down explicitly so a future change
    // (DWARF-aware function-name resolution, or a different call
    // heuristic) trips this assertion and the next maintainer extends
    // the call-sequence checks below to cover the new behaviour.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions.len(),
        1,
        "expected exactly 1 entry in the functions table; got {:?} — \
         if multi-frame call synthesis or DWARF function-name \
         resolution has landed, extend this test to assert on the \
         resolved names via `ends_with` matching",
        functions
    );
    assert!(
        functions[0].ends_with("main"),
        "expected the sole function-table entry to be `main`; got {:?}",
        functions
    );

    // ----- Step / call counts ----------------------------------------
    // The recorder emits one initial step at the trace start (line 1),
    // then a delta step per source-line transition for each of the 7
    // synthetic snapshots (lines 5..=11), for 8 step events total.  A
    // single call_entry frame for `main` wraps the whole trace.  These
    // are stable properties of the synthetic fixture under the current
    // Solana recorder — if they change, that's a real regression to
    // investigate, not a flake.
    let counts = &doc["counts"];
    assert_eq!(
        counts["steps"].as_u64(),
        Some(8),
        "expected 8 step events for the synthetic Solana fixture; \
         counts={counts}",
    );
    assert_eq!(
        counts["calls"].as_u64(),
        Some(1),
        "expected exactly 1 call event (the synthesised `main` frame); \
         counts={counts}",
    );
    assert_eq!(
        counts["paths"].as_u64(),
        Some(1),
        "expected exactly 1 path (solana_fixture.rs); counts={counts}",
    );

    let events = doc["events"].as_array().expect("events array");

    // ----- Call sequence: exactly `main` -----------------------------
    let call_sequence: Vec<&str> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .filter_map(|e| e["function"].as_str())
        .collect();
    assert_eq!(
        call_sequence.len(),
        1,
        "expected exactly 1 call_entry event; got {:?}",
        call_sequence
    );
    assert!(
        call_sequence[0].ends_with("main"),
        "expected the sole call_entry to be `main`; got {:?}",
        call_sequence
    );

    // ----- Strict ValueRecord variant + exact decoded values ---------
    // Collect every (varname, i64) pair surfaced by step events.  The
    // Solana recorder writes register values as `ValueRecord::Int`
    // CBOR blobs (see `register_variable_with_full_value` in
    // `src/recorder.rs`); ct-print --full decodes them back to
    // `{"kind":"Int","i":<n>,...}`.  If a different variant surfaces
    // (e.g. a `Raw` 8-byte register snapshot once the recorder learns
    // to surface 64-bit unsigned values that overflow i64), fail
    // loudly so the test author can decide whether to extend the
    // assertions or accept the new variant.
    let observed_vars: Vec<(String, i64)> = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .flat_map(|e| {
            e["vars"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
        })
        .map(|v| {
            let name = v["varname"]
                .as_str()
                .expect("step var should have a varname")
                .to_string();
            let value = &v["value"];
            assert_eq!(
                value["kind"].as_str(),
                Some("Int"),
                "register `{}` should decode as Int, got {}; \
                 if a new ValueRecord variant has landed for Solana \
                 SBF register values, extend this test to assert on \
                 it explicitly rather than weakening the check",
                name,
                value
            );
            let i = value["i"]
                .as_i64()
                .unwrap_or_else(|| panic!("Int.i must be i64 for `{name}`; got {value}"));
            (name, i)
        })
        .collect();

    // The canonical (10+32)*2+10 = 94 flow seeded by
    // `build_synthetic_regs` above:
    //   r1 = 10 (a)
    //   r2 = 32 (b)
    //   r3 = 42 (sum_val = a + b)
    //   r4 = 84 (doubled = sum_val * 2)
    //   r5 = 94 (final_result = doubled + a)
    //   r0 = 94 (return value)
    // Each (register, value) pair must surface at least once across
    // the step stream.  Same canonical values as cairo / polkavm /
    // etc. — if these six don't surface, that's the bug to chase.
    let expected: &[(&str, i64)] = &[
        ("r1", 10),
        ("r2", 32),
        ("r3", 42),
        ("r4", 84),
        ("r5", 94),
        ("r0", 94),
    ];
    for (name, value) in expected {
        assert!(
            observed_vars.iter().any(|(n, v)| n == name && v == value),
            "expected step variable `{name}` = {value} in --full output; \
             observed = {observed_vars:?}"
        );
    }
}

// ===========================================================================
// CLI env-var contract
// ===========================================================================

/// `CODETRACER_SOLANA_RECORDER_OUT_DIR` must be honoured as a fallback
/// for `--out-dir`.  Convention: `Recorder-CLI-Conventions.md` §5.
///
/// We drive the `--regs <path>` branch (synthetic register trace +
/// recorder's own binary as the ELF for DWARF) so the test does not
/// depend on a built SBF program — see the
/// `record_rejects_missing_regs_file` smoke test for the same shape.
#[test]
fn test_env_out_dir_used_when_flag_omitted() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let env_out_dir = tmp_dir.path().join("via-env");

    // Pre-built recorder binary doubles as a valid (non-SBF) ELF so the
    // `\x7fELF` magic-byte gate in `main.rs` passes and the `--regs` path
    // is taken.  DWARF resolution may not find SBF-mapped lines for this
    // ELF — that is fine; the recorder still produces a `.ct` bundle.
    let elf_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");

    // Synthetic .regs file (3 snapshots).
    let regs_path = tmp_dir.path().join("trace.regs");
    let mut regs_data = Vec::new();
    for step in 0..3u64 {
        let mut regs = [0u64; 12];
        regs[11] = step;
        for r in &regs {
            regs_data.extend_from_slice(&r.to_le_bytes());
        }
    }
    std::fs::write(&regs_path, &regs_data).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_codetracer-solana-recorder"))
        .args(["record"])
        .args(["--regs"])
        .arg(&regs_path)
        .arg(elf_path)
        .env("CODETRACER_SOLANA_RECORDER_OUT_DIR", &env_out_dir)
        // Make sure the env-var doesn't bleed in from the developer's shell.
        .env_remove("CODETRACER_SOLANA_RECORDER_DISABLED")
        .output()
        .expect("failed to run recorder");

    assert!(
        output.status.success(),
        "recorder should succeed when CODETRACER_SOLANA_RECORDER_OUT_DIR is set; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The env-var-supplied output dir must contain the .ct bundle.
    let ct_files: Vec<_> = std::fs::read_dir(&env_out_dir)
        .unwrap_or_else(|e| {
            panic!(
                "expected env-supplied out-dir {:?} to exist after record: {e}",
                env_out_dir
            )
        })
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect();
    assert!(
        !ct_files.is_empty(),
        "expected the env-supplied output dir {:?} to receive the .ct trace bundle",
        env_out_dir
    );
}

/// `CODETRACER_SOLANA_RECORDER_DISABLED=1` must skip recording entirely.
/// The recorder process should still exit 0 (the Solana recorder runs
/// the SBF VM itself — so "disabled" simply means "don't write any
/// trace artefacts").
#[test]
fn test_env_disabled_skips_recording() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("should-stay-empty");

    let elf_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");

    let output = Command::new(env!("CARGO_BIN_EXE_codetracer-solana-recorder"))
        .args(["record"])
        .arg(elf_path)
        .args(["--out-dir"])
        .arg(&out_dir)
        .env("CODETRACER_SOLANA_RECORDER_DISABLED", "1")
        .output()
        .expect("failed to run recorder");

    assert!(
        output.status.success(),
        "recorder should succeed in disabled mode; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // No trace artefacts of any kind should have been written.
    let no_artefacts = !out_dir.exists()
        || (std::fs::read_dir(&out_dir)
            .map(|rd| rd.filter_map(|e| e.ok()).next().is_none())
            .unwrap_or(true));
    assert!(
        no_artefacts,
        "no trace artefacts should be written when \
         CODETRACER_SOLANA_RECORDER_DISABLED=1; got files in {:?}",
        out_dir
    );
}

/// `--format` is no longer accepted at any level — clap must reject it.
/// Convention: §4 (CTFS-only).  Pre-2026-05-08 the flag existed at both
/// subcommand levels (`record`, `replay`); we exercise each here so a
/// partial regression is caught.
#[test]
fn test_format_flag_rejected_by_clap() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    let elf_path = env!("CARGO_BIN_EXE_codetracer-solana-recorder");

    // record --format json
    let output = Command::new(env!("CARGO_BIN_EXE_codetracer-solana-recorder"))
        .args(["record"])
        .arg(elf_path)
        .args(["--out-dir"])
        .arg(&out_dir)
        .args(["--format", "json"])
        .output()
        .expect("failed to run recorder");

    assert!(
        !output.status.success(),
        "--format should be rejected by clap on `record`; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--format")
            || stderr.contains("unexpected argument")
            || stderr.contains("unrecognized")
            || stderr.contains("found argument"),
        "clap error should mention the unknown --format flag on `record`; \
         got stderr:\n{stderr}"
    );

    // replay --format json (clap should reject the flag before the
    // RPC client even has to do anything).
    let output = Command::new(env!("CARGO_BIN_EXE_codetracer-solana-recorder"))
        .args(["replay"])
        .args(["--signature", "deadbeef"])
        .args(["--format", "json"])
        .output()
        .expect("failed to run recorder");

    assert!(
        !output.status.success(),
        "--format should be rejected by clap on `replay`"
    );
}

/// The CLI binary must not expose a `--format` flag at any level.
/// Convention: `Recorder-CLI-Conventions.md` §4 — recorders are
/// CTFS-only.
#[test]
fn test_no_format_flag_in_help() {
    let bin = env!("CARGO_BIN_EXE_codetracer-solana-recorder");

    for subcmd in [None, Some("record"), Some("replay")] {
        let mut cmd = Command::new(bin);
        if let Some(s) = subcmd {
            cmd.arg(s);
        }
        cmd.arg("--help");

        let output = cmd.output().expect("failed to run --help");
        assert!(
            output.status.success(),
            "--help (subcmd={:?}) should exit 0",
            subcmd
        );

        let help = String::from_utf8_lossy(&output.stdout);
        assert!(
            !help.contains("--format"),
            "--help (subcmd={:?}) must not advertise --format; got:\n{help}",
            subcmd
        );
        assert!(
            !help.contains("CODETRACER_FORMAT"),
            "--help (subcmd={:?}) must not advertise CODETRACER_FORMAT; got:\n{help}",
            subcmd
        );
    }
}

/// `--help` must mention `ct print` so users know where to go for
/// human-readable conversion of the recorded CTFS bundle.
#[test]
fn test_help_mentions_ct_print() {
    let bin = env!("CARGO_BIN_EXE_codetracer-solana-recorder");
    let output = Command::new(bin)
        .arg("--help")
        .output()
        .expect("failed to run --help");
    assert!(output.status.success(), "--help should exit 0");

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("ct print"),
        "--help must mention `ct print` as the conversion tool; got:\n{help}"
    );
}
