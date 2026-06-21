# Canonical CI check set — the single definition the attestation producer
# (scripts/attest.sh) runs and the verifier (scripts/attest-verify.sh) matches
# against, so the command each attestation records and the fragment the verifier
# requires cannot drift.
#
# Sourced, not executed. Bash 3.2 compatible (no associative arrays).
#
# scripts/preflight.sh and .github/workflows/ci.yml run the same checks through
# structurally different drivers (a local runner; CI jobs and the qodana
# action), so they cannot source this file directly — keep their commands in
# step with the ones below. ci.yml is also the push-to-main canary, so a drift
# there surfaces as a red canary.

# Ordered canonical step names.
CANONICAL_STEPS="fmt clippy doc dist test qodana"

# The identifying command for a step. The producer runs exactly this, adding
# per-run wrappers (RUSTDOCFLAGS for doc, AETHER_REQUIRE_RUNTIME for test,
# qodana's --diff-start), and the verifier requires the attestation's recorded
# command to contain it — so an attestation that ran `true` under the name
# "clippy" is rejected.
canonical_cmd() {
    case "$1" in
        fmt)    echo "cargo fmt --all -- --check" ;;
        clippy) echo "cargo clippy --workspace --all-targets -- -D warnings" ;;
        doc)    echo "cargo doc --workspace --no-deps" ;;
        dist)   echo "cargo xtask dist --no-bins" ;;
        test)   echo "cargo nextest run --workspace --all-features --profile ci" ;;
        qodana) echo "qodana scan" ;;
        *)      return 1 ;;
    esac
}
