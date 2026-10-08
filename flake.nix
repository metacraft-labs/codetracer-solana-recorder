{
  description = "CodeTracer Solana Recorder";

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
          SOLANA_CI_PYTHON = "${pkgs.python3}/bin/python3";
          SOLANA_CI_GIT = "${pkgs.git}/bin/git";
          SOLANA_CI_DIRENV = "${pkgs.direnv}/bin/direnv";
          inputsFrom = [ mcl-blockchain.devShells.${system}.solana ];
          packages = [
            pkgs.direnv
            pkgs.zstd # required by libcodetracer_trace_writer (Nim FFI)
            # Declare the toolchain explicitly so CI's dev shell
            # mirrors local dev exactly.  Cached mcl-blockchain
            # devShells from Attic sometimes drop nim/nimble from
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

          # `cargo <subcommand>` looks for `cargo-<subcommand>` in
          # `$CARGO_HOME/bin` BEFORE it searches PATH. On any machine with
          # rustup — including the self-hosted macOS runner — that directory
          # holds rustup's proxies, so `cargo fmt` and `cargo clippy` run
          # rustup's `cargo-fmt` / `cargo-clippy` instead of the shell's, and
          # fail with "'cargo-fmt' is not installed for the toolchain".
          #
          # The shell therefore gets its own CARGO_HOME with an empty `bin/`,
          # so subcommand lookup falls through to PATH. `registry/` and `git/`
          # are symlinks to the real CARGO_HOME, and so are its config and
          # credentials when present: the download cache is shared, and only
          # the proxy directory is left behind.
          shellHook = ''
            _solana_real_cargo_home="''${CARGO_HOME:-$HOME/.cargo}"
            _solana_cargo_home="''${XDG_CACHE_HOME:-$HOME/.cache}/codetracer-solana-recorder/cargo-home"
            if [ "$_solana_real_cargo_home" != "$_solana_cargo_home" ]; then
              mkdir -p "$_solana_cargo_home" \
                "$_solana_real_cargo_home/registry" "$_solana_real_cargo_home/git"
              # Re-pointed on every entry, so a changed CARGO_HOME is followed
              # rather than left sharing the previous one's cache. Only a link
              # is ever replaced; a real file placed here is left alone.
              for _solana_entry in registry git config.toml credentials.toml; do
                if [ -e "$_solana_real_cargo_home/$_solana_entry" ] &&
                  { [ -L "$_solana_cargo_home/$_solana_entry" ] ||
                    [ ! -e "$_solana_cargo_home/$_solana_entry" ]; }; then
                  ln -sfn "$_solana_real_cargo_home/$_solana_entry" "$_solana_cargo_home/$_solana_entry"
                fi
              done
              export CARGO_HOME="$_solana_cargo_home"
            fi
            unset _solana_real_cargo_home _solana_cargo_home _solana_entry
          '';
        };
      }
    );
}
