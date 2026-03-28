# Test Programs

This directory contains Solana program source code used for testing the
codetracer-solana-recorder.

## Building

These programs require `cargo-build-sbf` (part of the Solana SDK toolchain)
to compile into SBF ELF files (.so). They cannot be compiled as regular Rust
libraries because they depend on the Solana program runtime crates
(`solana-program`, etc.) and target the SBF architecture.

The build environment will be set up in Milestone M1 via a Nix flake that
provides the Solana SDK and `cargo-build-sbf`.

## Usage

Once compiled, the resulting `.so` files can be passed to the recorder:

```sh
codetracer-solana-recorder record path/to/program.so -o ./ct-traces/
```
