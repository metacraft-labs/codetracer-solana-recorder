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

# --- 3. Solana SBF toolchain -------------------------------------------------
# The recorder's tests and the cross-repo prepare-solana-fixture.sh script
# both invoke ``cargo build-sbf`` to produce the SBF ELF objects whose DWARF
# the recorder consumes.  ``cargo-build-sbf`` is installed by the official
# Solana platform-tools release; on Nix it comes from the ``mcl-blockchain``
# flake's ``cargo-build-sbf`` package, but on Windows DIY there's no Nix to
# do that for us.
#
# Provisioning order:
#   1. Pre-set ``CARGO_BUILD_SBF`` / ``SOLANA_PLATFORM_TOOLS_DIR`` (user
#      override).
#   2. Canonical Solana CLI install path.  ``solana-install`` drops
#      ``cargo-build-sbf`` under
#      ``%LOCALAPPDATA%\solana\install\active_release\bin`` (mirror of the
#      POSIX ``~/.local/share/solana/install/active_release/bin``).
#   3. Opt-out: ``WINDOWS_DIY_SKIP_SOLANA_SBF=1`` -- same shape as the other
#      ``WINDOWS_DIY_SKIP_*`` knobs above so satellite repos can opt out.
#
# When nothing resolves, emit a single WARNING and continue -- the rest of
# the env still loads.  Tests that DO need ``cargo build-sbf`` will then
# fail with a clear "not found" error from the recorder itself.
if (-not $env:WINDOWS_DIY_SKIP_SOLANA_SBF) {
    $sbfBin = $null
    if ($env:CARGO_BUILD_SBF -and (Test-Path $env:CARGO_BUILD_SBF)) {
        $sbfBin = $env:CARGO_BUILD_SBF
    }
    elseif ($env:SOLANA_PLATFORM_TOOLS_DIR -and (Test-Path (Join-Path $env:SOLANA_PLATFORM_TOOLS_DIR "cargo-build-sbf.exe"))) {
        $sbfBin = Join-Path $env:SOLANA_PLATFORM_TOOLS_DIR "cargo-build-sbf.exe"
    }
    else {
        $candidates = @(
            (Join-Path $env:LOCALAPPDATA "solana\install\active_release\bin\cargo-build-sbf.exe"),
            (Join-Path $env:USERPROFILE  ".local\share\solana\install\active_release\bin\cargo-build-sbf.exe")
        )
        foreach ($candidate in $candidates) {
            if (Test-Path $candidate) { $sbfBin = $candidate; break }
        }
    }

    if ($sbfBin) {
        $sbfDir = Split-Path -Parent $sbfBin
        if ($env:Path -notlike "*$sbfDir*") {
            $env:Path = "$sbfDir;$($env:Path)"
        }
        $env:CARGO_BUILD_SBF = $sbfBin
        Write-Host "Solana SBF toolchain: $sbfBin"
    }
    else {
        Write-Warning "Solana SBF toolchain not found. Install via the official Solana installer (see https://docs.anza.xyz/cli/install) or set CARGO_BUILD_SBF / SOLANA_PLATFORM_TOOLS_DIR. Tests that build SBF programs will fail until this is provisioned."
    }
}

Write-Host "codetracer-solana-recorder dev environment ready."