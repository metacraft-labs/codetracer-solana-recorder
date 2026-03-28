{
  description = "CodeTracer Solana Recorder development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust build dependencies
            rustc
            cargo
            capnproto
            pkg-config
            openssl
          ];

          shellHook = ''
            echo "CodeTracer Solana Recorder dev shell"
          '';
        };
      }
    );
}
