## Exact owning SBF compiler view; provisioned bytes follow owning flake sources.
## Windows SBF SDK provisioning remains a required unresolved capability.
import blake3
import repro_project_dsl

const sourceBytes = staticRead("tools/solana-sbf-sdk/default.nix") & "\0" &
  staticRead("flake.nix") & "\0" & staticRead("flake.lock")
let sourceIdentity = blake3.toHex(blake3.digest(sourceBytes))

package `solana-sbf-sdk`:
  provisioning:
    nixPackage "cargo-build-sbf", executablePath = "bin/cargo-build-sbf",
      expressionFile = "tools/solana-sbf-sdk/default.nix",
      lockIdentity = "owning-solana-flake-sbf:" & sourceIdentity
