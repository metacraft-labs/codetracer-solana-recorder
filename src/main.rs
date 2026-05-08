//! CLI entry point for the CodeTracer Solana recorder.
//!
//! Supports the `record` subcommand which takes a compiled Solana program
//! ELF file (.so), runs it through the SBF VM with register tracing enabled,
//! and writes the CodeTracer trace output files.
//!
//! Also supports the `replay` subcommand which fetches a confirmed on-chain
//! transaction via Solana JSON-RPC and replays it through the recording
//! pipeline.
//!
//! The recorder always writes traces in the canonical CodeTracer multi-stream
//! CTFS format (see `Recorder-CLI-Conventions.md` §4 in `codetracer-specs`).
//! No `--format` flag is exposed: human-readable conversion is handled
//! out-of-band by `ct print` (shipped with `codetracer-trace-format-nim`).
//!
//! # Usage
//!
//! ```text
//! # Full pipeline (register trace + DWARF):
//! codetracer-solana-recorder record --regs trace.regs --elf program.so \
//!     -o <output-dir>
//!
//! # Legacy placeholder mode (positional ELF only):
//! codetracer-solana-recorder record <ELF_FILE> \
//!     -o <output-dir>
//!
//! # Replay a confirmed on-chain transaction:
//! codetracer-solana-recorder replay --signature <SIG> \
//!     [--rpc-url <URL>] [--program-dir <PATH>] [-o <out-dir>]
//! ```
//!
//! # Environment variables
//!
//! * `CODETRACER_SOLANA_RECORDER_OUT_DIR` — fallback for `--out-dir` when the
//!   flag is not given. The CLI flag always wins.
//! * `CODETRACER_SOLANA_RECORDER_DISABLED` — set to `1` or `true` to skip
//!   recording entirely. The recorder still validates its inputs (where
//!   applicable) and propagates a clean exit code.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::{Context, Result};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Environment variable used as a fallback for `--out-dir` when the CLI
/// flag is omitted.  Convention: see `Recorder-CLI-Conventions.md` §5.
const ENV_OUT_DIR: &str = "CODETRACER_SOLANA_RECORDER_OUT_DIR";

/// Environment variable that, when set to `1`/`true`, disables tracing
/// entirely — the recorder runs as a transparent pass-through.
const ENV_DISABLED: &str = "CODETRACER_SOLANA_RECORDER_DISABLED";

/// Default output directory used when neither `--out-dir` nor
/// `CODETRACER_SOLANA_RECORDER_OUT_DIR` is set.
const DEFAULT_OUT_DIR: &str = "./ct-traces/";

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// CodeTracer Solana recorder -- record Solana/SBF program execution traces.
///
/// Traces are always written in the canonical CTFS multi-stream format.
/// To convert a recorded `.ct` bundle to JSON / text for inspection, use
/// `ct print` from `codetracer-trace-format-nim`.
#[derive(Debug, Parser)]
#[command(
    name = "codetracer-solana-recorder",
    version,
    about = "Record Solana program execution traces for CodeTracer (CTFS-only). \
             Use `ct print` from codetracer-trace-format-nim for human-readable conversion.",
    long_about = "Record Solana program execution traces for CodeTracer.\n\
                  \n\
                  Output is always written in the canonical CodeTracer CTFS\n\
                  multi-stream format. Use `ct print` (shipped with the\n\
                  codetracer-trace-format-nim sibling) to convert a recorded\n\
                  `.ct` bundle to JSON or other human-readable forms.\n\
                  \n\
                  Environment variables:\n\
                    CODETRACER_SOLANA_RECORDER_OUT_DIR    fallback for --out-dir\n\
                    CODETRACER_SOLANA_RECORDER_DISABLED   set to 1/true to skip recording"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Record execution of a compiled Solana program (ELF/SBF).
    ///
    /// Takes the path to a `.so` ELF file produced by `cargo-build-sbf`,
    /// runs it through the SBF VM with register tracing, and writes
    /// CodeTracer trace files to the output directory.
    Record(RecordArgs),

    /// Replay a confirmed on-chain transaction.
    ///
    /// Fetches the transaction via Solana JSON-RPC, reconstructs its
    /// execution context, and records a CodeTracer trace.
    ///
    /// NOTE: Uses *current* account state, not historical state at the
    /// transaction's slot.
    Replay(ReplayArgs),

    /// Print version information.
    Version,
}

#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// Path to the compiled Solana program ELF file (.so).
    /// Used as legacy positional argument; also used as the ELF for DWARF
    /// if --elf is not provided.
    elf_file: PathBuf,

    /// Path to the pre-generated register trace file (.regs binary).
    /// When provided together with --elf, uses the full recording pipeline.
    #[arg(long = "regs")]
    regs_file: Option<PathBuf>,

    /// Path to the unstripped ELF file for DWARF source mapping.
    /// Defaults to the positional ELF_FILE if not provided.
    #[arg(long = "elf")]
    elf_override: Option<PathBuf>,

    /// Directory where the trace files will be written.
    ///
    /// The directory will be created if it does not exist.  Falls back to
    /// the `CODETRACER_SOLANA_RECORDER_OUT_DIR` environment variable when
    /// the flag is omitted.
    #[arg(short = 'o', long = "out-dir")]
    out_dir: Option<PathBuf>,

    /// Path to an Anchor IDL JSON file for account data decoding.
    #[arg(long = "idl")]
    idl: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct ReplayArgs {
    /// Transaction signature to replay (base-58).
    #[arg(long = "signature")]
    signature: String,

    /// Solana JSON-RPC endpoint URL.
    #[arg(long = "rpc-url", default_value = "http://localhost:8899")]
    rpc_url: String,

    /// Directory containing compiled `.so` files with DWARF debug info.
    ///
    /// The recorder will search this directory for program binaries.
    /// Defaults to `target/deploy/`.
    #[arg(long = "program-dir")]
    program_dir: Option<PathBuf>,

    /// Directory where the trace files will be written.
    ///
    /// Falls back to the `CODETRACER_SOLANA_RECORDER_OUT_DIR` environment
    /// variable when the flag is omitted.
    #[arg(short = 'o', long = "out-dir")]
    out_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the effective output directory:
///   1. `--out-dir` if given on the CLI.
///   2. `CODETRACER_SOLANA_RECORDER_OUT_DIR` env var.
///   3. `DEFAULT_OUT_DIR` ("./ct-traces/").
fn resolve_out_dir(cli_out_dir: Option<PathBuf>) -> PathBuf {
    if let Some(path) = cli_out_dir {
        return path;
    }
    if let Some(value) = std::env::var_os(ENV_OUT_DIR)
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }
    PathBuf::from(DEFAULT_OUT_DIR)
}

/// Whether the recorder is disabled via env var.  When true, the CLI
/// must execute its target operation in pass-through mode without
/// emitting any trace artefacts.
fn recording_disabled() -> bool {
    match std::env::var(ENV_DISABLED) {
        Ok(value) => {
            let v = value.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Record(args) => record(args),
        Commands::Replay(args) => replay(args),
        Commands::Version => {
            println!(
                "codetracer-solana-recorder {}",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// `record` implementation
// ---------------------------------------------------------------------------

/// Execute the `record` subcommand.
fn record(args: RecordArgs) -> Result<()> {
    // 1. Validate the positional ELF file exists.
    let elf_path = args
        .elf_file
        .canonicalize()
        .with_context(|| format!("ELF file not found: {}", args.elf_file.display()))?;

    // 2. Validate the file has a valid ELF magic number.
    {
        let header = std::fs::read(&elf_path)
            .with_context(|| format!("failed to read ELF file: {}", elf_path.display()))?;
        if header.len() < 4 || &header[..4] != b"\x7fELF" {
            eyre::bail!(
                "invalid ELF file: {} — file does not start with the ELF magic number (\\x7fELF)",
                elf_path.display()
            );
        }
    }

    eprintln!("ELF file: {}", elf_path.display());

    if recording_disabled() {
        // Pass-through: the Solana recorder runs the SBF VM itself — so
        // disabling recording simply means "don't emit any trace artefacts".
        eprintln!("{ENV_DISABLED} is set; skipping trace recording (no output written).");
        return Ok(());
    }

    let out_dir = resolve_out_dir(args.out_dir);

    // 2. If --regs is provided, use the full pipeline.
    if let Some(regs_path) = &args.regs_file {
        let regs_path = regs_path
            .canonicalize()
            .with_context(|| format!("regs file not found: {}", regs_path.display()))?;

        let elf_for_dwarf = match &args.elf_override {
            Some(p) => p
                .canonicalize()
                .with_context(|| format!("ELF override not found: {}", p.display()))?,
            None => elf_path.clone(),
        };

        eprintln!("Register trace: {}", regs_path.display());
        eprintln!("ELF for DWARF: {}", elf_for_dwarf.display());

        let regs_data = std::fs::read(&regs_path)
            .with_context(|| format!("failed to read regs file: {}", regs_path.display()))?;
        let elf_data = std::fs::read(&elf_for_dwarf)
            .with_context(|| format!("failed to read ELF file: {}", elf_for_dwarf.display()))?;

        codetracer_solana_recorder::recorder::record_from_traces(
            &regs_data,
            &elf_data,
            &elf_path,
            &out_dir,
        )?;

        eprintln!("Trace written to {}", out_dir.display());
        return Ok(());
    }

    // 3. Execute the ELF through the SBF VM with register tracing.
    //    This is the primary recording mode for standalone programs.
    eprintln!("Executing ELF with register tracing...");

    let elf_data = std::fs::read(&elf_path)
        .with_context(|| format!("failed to read ELF file: {}", elf_path.display()))?;

    let regs_data = codetracer_solana_recorder::executor::execute_with_tracing(
        &elf_data,
        1_000_000, // 1M compute units
    )
    .with_context(|| "SBF VM execution failed")?;

    eprintln!(
        "Execution complete: {} register snapshots",
        regs_data.len() / 96
    );

    // Use the existing record_from_traces pipeline.
    codetracer_solana_recorder::recorder::record_from_traces(
        &regs_data,
        &elf_data,
        &elf_path,
        &out_dir,
    )?;

    eprintln!("Trace written to {}", out_dir.display());

    Ok(())
}

// ---------------------------------------------------------------------------
// `replay` implementation
// ---------------------------------------------------------------------------

/// Execute the `replay` subcommand.
fn replay(args: ReplayArgs) -> Result<()> {
    if recording_disabled() {
        eprintln!("{ENV_DISABLED} is set; skipping replay recording (no output written).");
        return Ok(());
    }

    let out_dir = resolve_out_dir(args.out_dir);

    codetracer_solana_recorder::replay::replay_transaction(
        &args.rpc_url,
        &args.signature,
        &out_dir,
        args.program_dir.as_deref(),
    )
}
