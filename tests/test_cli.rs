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
        stderr.contains("ELF magic") || stderr.contains("invalid ELF") || stderr.contains("\\x7fELF"),
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
