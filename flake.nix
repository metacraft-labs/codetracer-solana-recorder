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
          ];
        };
      }
    );
}
