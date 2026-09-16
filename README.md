## codetracer-solana-recorder

A recorder of Solana/SBF program executions that produces [CodeTracer](https://github.com/metacraft-labs/CodeTracer) traces.

> [!WARNING]
> Currently it is in a very early phase: we're welcoming contribution and discussion!

### Overview

codetracer-solana-recorder loads compiled Solana program ELF files (`.so` produced by `cargo-build-sbf`), executes them through the SBF VM with register tracing enabled, and resolves DWARF debug information to recover source mapping. It emits structured trace files compatible with CodeTracer.

### Building

```bash
cargo build
```

### Usage

Record a trace from a compiled Solana program:

```bash
codetracer-solana-recorder record <program.so> --out-dir <dir>
# Produces a CTFS multi-stream `.ct` bundle plus trace_metadata.json /
# trace_paths.json in <dir>.
```

Record from a pre-generated register trace (`.regs`) plus a DWARF-annotated ELF:

```bash
codetracer-solana-recorder record --regs trace.regs --elf program.so \
    --out-dir <dir>
```

Replay a confirmed on-chain transaction via Solana JSON-RPC:

```bash
codetracer-solana-recorder replay --signature <SIG> \
    [--rpc-url <URL>] [--program-dir <PATH>] --out-dir <dir>
```

The recorder always writes traces in the canonical CodeTracer CTFS
multi-stream format (see
`Recorder-CLI-Conventions.md`
§4). To convert a recorded `.ct` bundle to JSON or text for
inspection, use `ct print` (shipped with
[`codetracer-trace-format-nim`](https://github.com/metacraft-labs/codetracer-trace-format-nim)).

However, you probably want to use it in combination with CodeTracer, which would be released soon.

### Examples

See [`examples/`](examples/) for self-contained Solana programs you
can record and replay end-to-end with the `ct` CLI, including a
column-aware step-over walkthrough.

### Architecture

The recorder is organized into the following modules:

* `recorder.rs` — top-level recording orchestration and trace file output (CTFS-only)
* `executor.rs` — SBF VM execution with register tracing
* `dwarf.rs` — DWARF debug info parsing for source mapping
* `register_trace.rs` — `.regs` binary register snapshot parsing
* `cpi.rs` — CPI (Cross-Program Invocation) detection
* `multi_program.rs` — multi-program registry for CPI-aware tracing
* `account_decoder.rs` — Anchor IDL / Borsh account data decoding
* `tracer_trait.rs` — pluggable `SbpfTracer` trait + `CodeTracerTracer` impl
* `replay.rs` — on-chain transaction replay via Solana JSON-RPC
* `rpc_client.rs` — Solana JSON-RPC client

### Testing

```bash
cargo test
```

The repo also ships a `Justfile`:

```bash
just build       # cargo build --locked
just test        # cargo test --locked + verify-cli-convention
just lint        # cargo fmt --check + clippy + verify-cli-convention
```

### Environment variables

* `CODETRACER_SOLANA_RECORDER_OUT_DIR` — fallback for `--out-dir` when the flag is omitted (convention: `Recorder-CLI-Conventions.md` §5)
* `CODETRACER_SOLANA_RECORDER_DISABLED` — set to `1` or `true` to skip recording entirely (the recorder still validates inputs and exits cleanly)

### Contributing

We'd be very happy if the community finds this useful, and if anyone wants to:

* Use and test the Solana support or CodeTracer.
* Provide feedback and discuss alternative implementation ideas: in the issue tracker, or in our [discord](https://discord.gg/qSDCAFMP).
* Contribute code to enhance the Solana support of CodeTracer.
* Provide [sponsorship](https://opencollective.com/codetracer), so we can hire dedicated full-time maintainers for this project.

### Legal info

LICENSE: Apache-2.0

Copyright (c) 2025 Metacraft Labs Ltd
