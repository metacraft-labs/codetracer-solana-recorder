//! A program built with its dependencies (`solana-program`, as every real
//! Solana program is): every recorded step is in a file that exists, the
//! recording starts in the `entrypoint!` code of `solana-program-entrypoint`
//! (its own `src/lib.rs`, by absolute path), the program's handler runs in
//! its own source, and what it logs with `msg!` is recorded as its output.
//!
//! The repo's `test-programs` crate is compiled with `cargo-build-sbf` into a
//! scratch target directory and its unstripped ELF recorded through the
//! recorder CLI. Two things in such a build broke source mapping:
//! `cargo-build-sbf` removes `DW_AT_comp_dir`, so a dependency's files are
//! named only `src/lib.rs`; and `--gc-sections` leaves the DWARF of every
//! discarded function at address 0, where it overlaps the real code.
//!
//! Prerequisite: `cargo-build-sbf` on `PATH` (the dev shell provides it).

use std::path::{Path, PathBuf};
use std::process::Command;

use codetracer_trace_types::TraceLowLevelEvent;

fn find_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

#[test]
fn a_program_with_dependencies_steps_through_real_files() {
    let cargo_build_sbf = find_on_path("cargo-build-sbf").unwrap_or_else(|| {
        panic!("missing prerequisite: `cargo-build-sbf` is not on PATH; run inside `nix develop`")
    });
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-programs");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let target_dir = scratch.path().join("target");
    let build = Command::new(&cargo_build_sbf)
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .expect("run cargo-build-sbf");
    assert!(
        build.status.success(),
        "cargo-build-sbf failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    let elf = target_dir.join("sbpf-solana-solana").join("release").join("test_programs.so");
    assert!(elf.is_file(), "no unstripped ELF at {}", elf.display());

    let out_dir = scratch.path().join("trace");
    let record = Command::new(env!("CARGO_BIN_EXE_codetracer-solana-recorder"))
        .arg("record")
        .arg(&elf)
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .expect("run the recorder");
    assert!(
        record.status.success(),
        "recorder failed:\nstderr: {}",
        String::from_utf8_lossy(&record.stderr)
    );

    let ct = std::fs::read_dir(&out_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "ct"))
        .expect("a .ct container");
    let events = codetracer_trace_reader::ctfs_reader::read_trace_from_ctfs(&ct).expect("decode");
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut step_paths: Vec<PathBuf> = Vec::new();
    let mut output: Vec<String> = Vec::new();
    for event in events {
        match event {
            TraceLowLevelEvent::Path(p) => paths.push(p),
            TraceLowLevelEvent::Step(s) => step_paths.push(paths[s.path_id.0].clone()),
            TraceLowLevelEvent::Event(e) => output.push(e.content),
            _ => {}
        }
    }
    // The program's `msg!("result: {}", final_result)` is its output.
    assert!(
        output.iter().any(|o| o.contains("result: 94")),
        "the program's msg! output is recorded as an event: {output:?}"
    );
    assert!(!step_paths.is_empty(), "no steps recorded");

    // Absolute as well as present: a relative `src/lib.rs` would be found
    // relative to whatever directory the replay runs in (this repo has one).
    let missing: Vec<&PathBuf> = step_paths
        .iter()
        .filter(|p| !p.is_absolute() || !p.is_file())
        .collect();
    assert!(missing.is_empty(), "steps in files that do not exist: {:?}", dedup(&missing));

    let first = &step_paths[0];
    assert!(
        first.ends_with("src/lib.rs")
            && first
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with("solana-program-entrypoint")),
        "the recording starts in solana-program-entrypoint's src/lib.rs, not {}",
        first.display()
    );

    let program = crate_dir.join("src").join("solana_flow_test.rs").canonicalize().unwrap();
    assert!(
        step_paths.iter().any(|p| p.canonicalize().ok().as_deref() == Some(program.as_path())),
        "the handler's steps are in {}",
        program.display()
    );
}

fn dedup<'a>(paths: &[&'a PathBuf]) -> Vec<&'a Path> {
    let mut out: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    out.sort();
    out.dedup();
    out
}
