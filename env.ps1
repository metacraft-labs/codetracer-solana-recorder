# codetracer-solana-recorder Windows dev environment (PowerShell)
# Usage: . .\env.ps1
#
# The recorder builds and tests with a plain `cargo build` / `cargo test`.
# Its only non-standard requirements on Windows are:
#
#   1. The shared CodeTracer toolchain (Rust, Nim + nimble, just, Cap'n Proto,
#      MSVC).  These are provisioned by the main `codetracer` repo's env.ps1,
#      which this script dot-sources.  The Nim toolchain is needed because the
#      `codetracer_trace_writer_nim` crate's build script compiles a Nim
#      static library, so `nim`/`nimble` must be on PATH.
#
#   2. An explicit MSVC linker for the `x86_64-pc-windows-msvc` target.  The
#      `just test` recipe runs `tests/verify-cli-convention-no-silent-skip.sh`
#      via bash, and that script invokes `cargo build`.  Git Bash ships a
#      coreutils `link.exe` (the hard-link tool) in its `usr/bin`, and a bash
#      login shell re-orders PATH so `usr/bin` precedes the MSVC toolchain.
#      Cargo would then resolve `link.exe` to coreutils `link` and the link
#      fails with `link: missing operand`.  Pointing
#      `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER` at MSVC's absolute
#      `link.exe` bypasses PATH resolution entirely, so every build -- whether
#      launched from PowerShell or from a bash `just` recipe -- links with the
#      correct linker.

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

# --- 1. Shared CodeTracer toolchain -----------------------------------------
# The blockchain recorders do not need FPC, LLVM, nargo or dotnet; skip those
# bootstrap steps so activation is fast.
$env:WINDOWS_DIY_SKIP_FPC = "1"
$env:WINDOWS_DIY_SKIP_LLVM = "1"
$env:WINDOWS_DIY_SKIP_NARGO = "1"
$env:WINDOWS_DIY_SKIP_DOTNET = "1"

$codetracerEnv = Join-Path (Split-Path -Parent $scriptDir) "codetracer\env.ps1"
if (-not (Test-Path $codetracerEnv)) {
    throw "Could not find the shared CodeTracer env.ps1 at $codetracerEnv -- the ``codetracer`` repo must be checked out as a sibling of this repo."
}
. $codetracerEnv

# --- 2. Explicit MSVC linker (immune to Git Bash PATH reordering) -----------
if ($env:WINDOWS_DIY_CL_EXE -and (Test-Path $env:WINDOWS_DIY_CL_EXE)) {
    $msvcBin = Split-Path -Parent $env:WINDOWS_DIY_CL_EXE
    $msvcLink = Join-Path $msvcBin "link.exe"
    if (Test-Path $msvcLink) {
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $msvcLink
    }
    if ($env:Path -notlike "$msvcBin;*") {
        $env:Path = "$msvcBin;$($env:Path)"
    }
}

Write-Host "codetracer-solana-recorder dev environment ready."