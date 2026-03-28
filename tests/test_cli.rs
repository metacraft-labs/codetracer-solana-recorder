use std::process::Command;

fn cargo_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_codetracer-solana-recorder"))
}

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
    let output = cargo_bin().arg("--version").output().expect("failed to run");
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
fn record_creates_output_files() {
    let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
    let elf_file = tmp.path().join("dummy_program.so");
    std::fs::write(&elf_file, b"dummy ELF data").expect("failed to write dummy ELF");

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
        output.status.success(),
        "record should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        out_dir.join("trace_metadata.json").exists(),
        "trace_metadata.json should be created"
    );
    assert!(
        out_dir.join("trace_paths.json").exists(),
        "trace_paths.json should be created"
    );
}
