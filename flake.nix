{
  description = "CodeTracer Solana Recorder";

  nixConfig = {
    extra-substituters = [
      "https://cache.metacraft-labs.com/metacraft-public"
    ];
    extra-trusted-public-keys = [
      "metacraft-public:UtS6PK+p0uZaJK3i/jD2DQOjTpddhQUQmNQDQih5N4Q="
    ];
  };

  inputs = {
    mcl-blockchain.url = "github:metacraft-labs/nix-blockchain-development";
    nixpkgs.follows = "mcl-blockchain/nixpkgs";
    flake-utils.follows = "mcl-blockchain/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      mcl-blockchain,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          inputsFrom = [ mcl-blockchain.devShells.${system}.solana ];
          packages = [
            pkgs.zstd # required by libcodetracer_trace_writer (Nim FFI)
            # Declare the toolchain explicitly so CI's dev shell
            # mirrors local dev exactly.  Cached mcl-blockchain
            # devShells on cachix sometimes drop nim/nimble from
            # PATH on resolution; declaring them here keeps the
            # contract visible in flake.nix.
            pkgs.nim
            pkgs.nimble
            pkgs.just
            pkgs.capnproto
            pkgs.rustc
            pkgs.cargo
            pkgs.rustfmt
            pkgs.clippy
            pkgs.pkg-config
            # Solana SBF toolchain.  The mcl-blockchain ``solana``
            # devShell exposes ``cargo-build-sbf`` via its own
            # ``packages = [...]`` list; ``inputsFrom`` only propagates
            # ``nativeBuildInputs`` / ``buildInputs`` from the source
            # shell -- it does NOT propagate ``packages``.  Declaring
            # the SBF tool explicitly here keeps it on PATH for both
            # ``cargo test`` (recorder unit tests build SBF programs)
            # and the cross-repo ``prepare-solana-fixture.sh`` step
            # in codetracer-vscode-extension (which shells into this
            # devShell via ``nix develop -c sh``).
            mcl-blockchain.packages.${system}.cargo-build-sbf
          ];
        };
      }
    );
}
