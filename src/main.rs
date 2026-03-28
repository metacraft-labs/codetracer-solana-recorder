//! CLI entry point for the CodeTracer Solana recorder.
//!
//! Supports the `record` subcommand which takes a compiled Solana program
//! ELF file (.so), runs it through the SBF VM with register tracing enabled,
//! and writes the CodeTracer trace output files.
//!
//! # Usage
//!
//! ```text
//! codetracer-solana-recorder record <ELF_FILE> \
//!     -o <output-dir> \
//!     [-f <format>]
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
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
    elf_file: PathBuf,

    /// Directory where the trace files will be written.
    ///
    /// The directory will be created if it does not exist.
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
    // 1. Validate the ELF file exists.
    let elf_path = args
        .elf_file
        .canonicalize()
        .with_context(|| format!("ELF file not found: {}", args.elf_file.display()))?;

    eprintln!("ELF file: {}", elf_path.display());

    // 2. Recording not yet implemented.
    eprintln!("Recording not yet implemented");

    // 3. Create output directory.
    let out_dir = &args.out_dir;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // 4. Write placeholder trace_metadata.json.
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

    // 5. Write placeholder trace_paths.json.
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
