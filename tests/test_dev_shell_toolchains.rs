//! The dev shell carries two Rust toolchains, and each must keep its own
//! flags.
//!
//! * The host toolchain (Rust 1.97 from fenix) links x86_64-linux executables
//!   with its bundled `rust-lld` unless told `-C linker-features=-lld`.
//!   `rust-lld` bypasses the Nix ld-wrapper, so the binaries get neither the
//!   Nix dynamic loader nor a RUNPATH for the store libraries they need.
//! * The Solana SBF toolchain that `cargo build-sbf` drives is an older rustc
//!   (platform-tools), which rejects `-C linker-features` as an unknown codegen
//!   option. It compiles host-side code too: build scripts and proc macros, and
//!   the target probe `cargo metadata` runs before anything is built.
//!
//! The flag therefore has to travel with the host toolchain and stay out of
//! the environment both toolchains share. These tests run the two compilers
//! the way a developer in `nix develop` does, with the ambient environment:
//! nothing is stubbed, the compilers and the SBF build are the real ones.
//!
//! Prerequisite: the repo's dev shell (`nix develop`), which puts the host
//! toolchain and `cargo-build-sbf` on `PATH`. When `cargo-build-sbf` is
//! missing the SBF test FAILS naming the prerequisite.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn find_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// A scratch crate with a build script, so that a build compiles and links
/// host code as well as the crate itself.
fn scratch_crate(dir: &Path, name: &str, crate_type: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let (target_section, source) = match crate_type {
        "bin" => ("[[bin]]", "src/main.rs"),
        _ => ("[lib]\ncrate-type = [\"cdylib\"]", "src/lib.rs"),
    };
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             build = \"build.rs\"\n\n{target_section}\nname = \"{name}\"\npath = \"{source}\"\n\n\
             [dependencies]\n\n[workspace]\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
    let body = match crate_type {
        "bin" => "fn main() {}\n",
        _ => "#[no_mangle]\npub extern \"C\" fn entrypoint(_input: *mut u8) -> u64 { 0 }\n",
    };
    std::fs::write(dir.join(source), body).unwrap();
}

/// `cmd` with the developer's shell environment, minus what `cargo test`
/// itself adds for its children. `RUSTFLAGS` is removed because cargo lets it
/// override every target-specific rustflags setting; leaving one set by the
/// caller would hide exactly the flags these tests are about.
fn ambient(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `cargo build-sbf` in the dev shell, as
/// codetracer-vscode-extension's `prepare-solana-fixture.sh` runs it.
#[test]
fn cargo_build_sbf_succeeds_in_the_dev_shell_environment() {
    let cargo_build_sbf = find_on_path("cargo-build-sbf").unwrap_or_else(|| {
        panic!(
            "missing prerequisite: `cargo-build-sbf` is not on PATH. Run this test inside \
             the repo's dev shell (`nix develop`), which provides it."
        )
    });
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let crate_dir = scratch.path().join("sbf_program");
    scratch_crate(&crate_dir, "ct_dev_shell_sbf_probe", "cdylib");

    let build = ambient(&mut Command::new(&cargo_build_sbf))
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        .current_dir(&crate_dir)
        .output()
        .expect("run cargo-build-sbf");
    assert!(
        build.status.success(),
        "cargo-build-sbf failed in the dev shell environment:\n{}",
        describe(&build)
    );
    let elf = crate_dir
        .join("target")
        .join("sbpf-solana-solana")
        .join("release")
        .join("ct_dev_shell_sbf_probe.so");
    assert!(elf.is_file(), "no SBF ELF at {}", elf.display());
}

/// The host toolchain still links through the C compiler driver's own
/// linker, not its bundled `rust-lld`, when cargo drives it from the dev
/// shell. `--print=link-args` reports the linker command rustc ran;
/// `rust-lld` shows up there as `-fuse-ld=lld` with a `gcc-ld` search path.
#[test]
fn host_cargo_build_does_not_link_with_bundled_rust_lld() {
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let crate_dir = scratch.path().join("host_program");
    scratch_crate(&crate_dir, "ct_dev_shell_host_probe", "bin");

    let build = ambient(&mut Command::new(
        std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
    ))
    .args(["rustc", "--quiet", "--bin", "ct_dev_shell_host_probe"])
    .arg("--manifest-path")
    .arg(crate_dir.join("Cargo.toml"))
    .args(["--", "--print=link-args"])
    .current_dir(&crate_dir)
    .output()
    .expect("run cargo rustc");
    assert!(
        build.status.success(),
        "host build failed:\n{}",
        describe(&build)
    );
    let link_args = String::from_utf8_lossy(&build.stdout);
    assert!(
        link_args.contains("ct_dev_shell_host_probe"),
        "rustc printed no link line for the probe binary:\n{}",
        describe(&build)
    );
    assert!(
        !link_args.contains("-fuse-ld=lld") && !link_args.contains("gcc-ld"),
        "the host toolchain linked with its bundled rust-lld, which bypasses the Nix \
         ld-wrapper:\n{link_args}"
    );

    // The executable has to start: under rust-lld it would name the FHS
    // loader, which a NixOS host does not have.
    let probe = crate_dir
        .join("target")
        .join("debug")
        .join("ct_dev_shell_host_probe");
    let run = Command::new(&probe).output().expect("run the probe binary");
    assert!(
        run.status.success(),
        "probe binary did not run:\n{}",
        describe(&run)
    );
}
