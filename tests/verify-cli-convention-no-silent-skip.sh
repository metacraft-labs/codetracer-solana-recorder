#!/usr/bin/env bash
# Verify that the codetracer-solana-recorder CLI complies with
# `Recorder-CLI-Conventions.md` (no silent skip — every assertion
# either passes or fails loudly):
#
#   * `--format` is absent from `--help` (CTFS-only — convention §4)
#   * `CODETRACER_FORMAT` is absent from `--help` (convention §5)
#   * `--out-dir` and `--version` are present in `--help` (§3)
#   * `--help` mentions `ct print` (the canonical conversion tool, §4)
#   * `CODETRACER_SOLANA_RECORDER_OUT_DIR` /
#     `CODETRACER_SOLANA_RECORDER_DISABLED` are referenced in source so
#     the env-var fallback (§5) cannot regress silently.
#
# Wire-up: see `Justfile` (`just lint` and `just test` both run this
# script).
#
# Exit codes:
#   0  all assertions held
#   1  at least one assertion failed (the failing line is printed to
#      stderr and the script exits at the first failure for clarity)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Build the binary if it isn't already built (cargo build is a no-op
# when nothing has changed).  We use --quiet so the output of this
# script stays focused on verification results.
( cd "${REPO_ROOT}" && cargo build --locked --quiet )

recorder() {
  ( cd "${REPO_ROOT}" && cargo run --locked --quiet -- "$@" )
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

assert_absent() {
  # assert_absent <needle> <haystack-description> <haystack>
  local needle="$1"
  local desc="$2"
  local haystack="$3"
  if grep -qF -- "${needle}" <<< "${haystack}"; then
    echo "FAIL: ${desc} must NOT contain '${needle}'" >&2
    echo "----- ${desc} -----" >&2
    echo "${haystack}" >&2
    echo "-------------------" >&2
    exit 1
  fi
  echo "ok: '${needle}' absent from ${desc}"
}

assert_present() {
  # assert_present <needle> <haystack-description> <haystack>
  local needle="$1"
  local desc="$2"
  local haystack="$3"
  if ! grep -qF -- "${needle}" <<< "${haystack}"; then
    echo "FAIL: ${desc} must contain '${needle}'" >&2
    echo "----- ${desc} -----" >&2
    echo "${haystack}" >&2
    echo "-------------------" >&2
    exit 1
  fi
  echo "ok: '${needle}' present in ${desc}"
}

# ---------------------------------------------------------------------------
# Top-level --help
# ---------------------------------------------------------------------------

TOP_HELP="$(recorder --help)"

assert_absent "--format" "top-level --help" "${TOP_HELP}"
assert_absent "CODETRACER_FORMAT" "top-level --help" "${TOP_HELP}"
assert_present "--help" "top-level --help" "${TOP_HELP}"
assert_present "--version" "top-level --help" "${TOP_HELP}"
assert_present "ct print" "top-level --help" "${TOP_HELP}"

# ---------------------------------------------------------------------------
# `record` subcommand --help
# ---------------------------------------------------------------------------

RECORD_HELP="$(recorder record --help)"

assert_absent "--format" "record --help" "${RECORD_HELP}"
assert_absent "CODETRACER_FORMAT" "record --help" "${RECORD_HELP}"
assert_present "--out-dir" "record --help" "${RECORD_HELP}"

# ---------------------------------------------------------------------------
# `replay` subcommand --help
# ---------------------------------------------------------------------------

REPLAY_HELP="$(recorder replay --help)"

assert_absent "--format" "replay --help" "${REPLAY_HELP}"
assert_absent "CODETRACER_FORMAT" "replay --help" "${REPLAY_HELP}"
assert_present "--out-dir" "replay --help" "${REPLAY_HELP}"

# ---------------------------------------------------------------------------
# --version output
# ---------------------------------------------------------------------------

VERSION_OUT="$(recorder --version)"
assert_present "codetracer-solana-recorder" "--version output" "${VERSION_OUT}"

# ---------------------------------------------------------------------------
# Source-level reference for the env-var fallback
# ---------------------------------------------------------------------------

# The recorder must reference CODETRACER_SOLANA_RECORDER_OUT_DIR in
# source (otherwise the env-var fallback either doesn't exist or has
# been silently removed).  We grep recursively under src/.
if ! grep -rqF "CODETRACER_SOLANA_RECORDER_OUT_DIR" "${REPO_ROOT}/src"; then
  echo "FAIL: CODETRACER_SOLANA_RECORDER_OUT_DIR must be referenced in src/" >&2
  exit 1
fi
echo "ok: CODETRACER_SOLANA_RECORDER_OUT_DIR referenced in src/"

if ! grep -rqF "CODETRACER_SOLANA_RECORDER_DISABLED" "${REPO_ROOT}/src"; then
  echo "FAIL: CODETRACER_SOLANA_RECORDER_DISABLED must be referenced in src/" >&2
  exit 1
fi
echo "ok: CODETRACER_SOLANA_RECORDER_DISABLED referenced in src/"

echo "verify-cli-convention-no-silent-skip: all checks passed"
