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
//! # Usage
//!
//! ```text
//! # Full pipeline (register trace + DWARF):
//! codetracer-solana-recorder record --regs trace.regs --elf program.so \
//!     -o <output-dir> [-f <format>]
//!
//! # Legacy placeholder mode (positional ELF only):
//! codetracer-solana-recorder record <ELF_FILE> \
//!     -o <output-dir> [-f <format>]
//!
//! # Replay a confirmed on-chain transaction:
//! codetracer-solana-recorder replay --signature <SIG> \
//!     [--rpc-url <URL>] [--program-dir <PATH>] [-o <out-dir>] [-f <format>]
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use codetracer_trace_writer::TraceEventsFileFormat;
use eyre::{Context, Result};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// CodeTracer Solana recorder -- record Solana/SBF program execution traces.
#[derive(Debug, Parser)]
#[command(
    name = "codetracer-solana-recorder",
    version,
    about = "Record Solana program execution traces for CodeTracer"
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

#[derive(Debug, Clone, ValueEnum)]
enum OutputFormat {
    Binary,
    Json,
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
    /// The directory will be created if it does not exist.
    #[arg(short = 'o', long = "out-dir", default_value = "./ct-traces/")]
    out_dir: PathBuf,

    /// Output format for the trace data.
    #[arg(short = 'f', long = "format", default_value = "binary")]
    format: OutputFormat,
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
    #[arg(short = 'o', long = "out-dir", default_value = "./ct-traces/")]
    out_dir: PathBuf,

    /// Output format for the trace data.
    #[arg(short = 'f', long = "format", default_value = "binary")]
    format: OutputFormat,
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

    eprintln!("ELF file: {}", elf_path.display());

    // Determine trace format.
    let format = match args.format {
        OutputFormat::Binary => TraceEventsFileFormat::Binary,
        OutputFormat::Json => TraceEventsFileFormat::Json,
    };

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
            &args.out_dir,
            format,
        )?;

        eprintln!("Trace written to {}", args.out_dir.display());
        return Ok(());
    }

    // 3. Legacy placeholder mode: no --regs provided.
    eprintln!("Recording not yet implemented");

    // Create output directory.
    let out_dir = &args.out_dir;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // Write placeholder trace_metadata.json.
    let metadata = serde_json::json!({
        "recorder": "codetracer-solana-recorder",
        "version": env!("CARGO_PKG_VERSION"),
        "format": format!("{:?}", args.format).to_lowercase(),
        "status": "placeholder"
    });
    std::fs::write(
        out_dir.join("trace_metadata.json"),
        serde_json::to_string_pretty(&metadata)?,
    )
    .context("failed to write trace_metadata.json")?;

    // Write placeholder trace_paths.json.
    let paths = serde_json::json!({
        "elf_file": elf_path.to_string_lossy(),
        "trace_dir": out_dir.to_string_lossy()
    });
    std::fs::write(
        out_dir.join("trace_paths.json"),
        serde_json::to_string_pretty(&paths)?,
    )
    .context("failed to write trace_paths.json")?;

    eprintln!("Placeholder trace written to {}", out_dir.display());
    eprintln!("  trace_metadata.json");
    eprintln!("  trace_paths.json");

    Ok(())
}

// ---------------------------------------------------------------------------
// `replay` implementation
// ---------------------------------------------------------------------------

/// Execute the `replay` subcommand.
fn replay(args: ReplayArgs) -> Result<()> {
    let format = match args.format {
        OutputFormat::Binary => TraceEventsFileFormat::Binary,
        OutputFormat::Json => TraceEventsFileFormat::Json,
    };

    codetracer_solana_recorder::replay::replay_transaction(
        &args.rpc_url,
        &args.signature,
        &args.out_dir,
        format,
        args.program_dir.as_deref(),
    )
}
