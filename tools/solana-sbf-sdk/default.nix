{ system ? builtins.currentSystem }:
let
  projectRoot = ../..;
  source = builtins.path {
    path = projectRoot;
    name = "solana-owning-flake-source";
    filter = path: type:
      path == toString projectRoot ||
      (type == "regular" && builtins.elem (builtins.baseNameOf path) [ "flake.nix" "flake.lock" ]);
  };
  owner = builtins.getFlake (builtins.unsafeDiscardStringContext ("path:" + toString source));
in
assert builtins.elem system [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
owner.inputs.mcl-blockchain.packages.${system}.cargo-build-sbf
