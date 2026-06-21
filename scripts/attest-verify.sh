#!/usr/bin/env bash
# Attestation verifier: confirm a PR's checks were attested by the PR author,
# who must be a write-collaborator. Run by .github/workflows/attest-verify.yml
# on `pull_request_target`, so this logic always comes from the trusted base
# branch — never the PR head — and it only reads the attestation ref + the
# author's public keys (it never checks out or runs PR code).
#
# Trust model: a collaborator vouches for their own checks by signing the
# attestations with a key already on their GitHub account. The verifier proves
# the signer is that collaborator; it does not re-run the checks. The main
# canary (real CI on push) is the backstop.
#
# Required inputs (environment):
#   REPO       — owner/name
#   PR_AUTHOR  — the PR author's GitHub login
#   HEAD_SHA   — the PR head commit sha
#   GH_TOKEN   — token for `gh api` (read-only is enough)
#
# Prerequisites on PATH: witness, sshpk-conv, gh, openssl, python3, curl.

set -euo pipefail

: "${REPO:?}"; : "${PR_AUTHOR:?}"; : "${HEAD_SHA:?}"

# Canonical checks. The expected-command fragment binds each attestation to the
# real check, so an attestation that ran `true` under the name "clippy" is
# rejected. Keep in lockstep with scripts/attest.sh / .github/workflows/ci.yml.
REQUIRED_STEPS=(fmt clippy doc dist test qodana)
expected_cmd() {
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

fail() { echo "::error::attest-verify: $*"; exit 1; }

# 1. The author must be a write-collaborator on this repo.
perm="$(gh api "repos/$REPO/collaborators/$PR_AUTHOR/permission" -q .permission 2>/dev/null)" \
    || fail "cannot read collaborator permission for $PR_AUTHOR"
case "$perm" in
    admin|write|maintain) ;;
    *) fail "$PR_AUTHOR is not a write-collaborator (permission=$perm)" ;;
esac

# 2. Fetch the attestation side ref the producer published for this commit.
git fetch --quiet origin "+refs/attestations/$HEAD_SHA:refs/attestations/incoming" 2>/dev/null \
    || fail "no attestations published at refs/attestations/$HEAD_SHA"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# 3. Resolve the author's registered keys into witness functionaries. The keyid
#    is the sha256 of the PEM public key (witness's scheme); the same key must
#    have signed the attestations.
keys_json=""; funcs_json=""; nkeys=0
while read -r ktype kval _; do
    [[ "$ktype" == ssh-* ]] || continue
    printf '%s %s\n' "$ktype" "$kval" > "$work/k$nkeys.pub"
    sshpk-conv -t pem -f "$work/k$nkeys.pub" > "$work/k$nkeys.pem" 2>/dev/null || continue
    kid="$(openssl dgst -sha256 < "$work/k$nkeys.pem" | awk '{print $NF}')"
    kb64="$(base64 < "$work/k$nkeys.pem" | tr -d '\n')"
    keys_json+="\"$kid\":{\"keyid\":\"$kid\",\"key\":\"$kb64\"},"
    funcs_json+="{\"type\":\"publickey\",\"publickeyid\":\"$kid\"},"
    nkeys=$((nkeys + 1))
done < <(curl -fsSL "https://github.com/$PR_AUTHOR.keys")
(( nkeys > 0 )) || fail "$PR_AUTHOR has no registered SSH keys to verify against"
keys_json="${keys_json%,}"; funcs_json="${funcs_json%,}"

# Each attestation binds to the commit through a product subject whose digest is
# sha256(head_sha) — the producer writes the sha into that file. Reconstruct the
# digest to pass as the verified subject.
subject_digest="$(printf '%s' "$HEAD_SHA" | openssl dgst -sha256 | awk '{print $NF}')"

# 4. Verify each required step: present, signed by one of the author's keys,
#    bound to this commit, and recording the canonical command.
for step in "${REQUIRED_STEPS[@]}"; do
    att="$work/$step.att.json"
    git show "refs/attestations/incoming:$step.json" > "$att" 2>/dev/null \
        || fail "missing attestation for required step '$step'"

    cat > "$work/policy.json" <<EOF
{ "expires":"2099-01-01T00:00:00Z",
  "publickeys": { $keys_json },
  "steps": { "$step": { "name":"$step", "functionaries":[ $funcs_json ],
    "attestations":[
      {"type":"https://witness.dev/attestations/material/v0.1","regopolicies":[]},
      {"type":"https://witness.dev/attestations/command-run/v0.1","regopolicies":[]},
      {"type":"https://witness.dev/attestations/product/v0.1","regopolicies":[]}]}}}
EOF
    # The policy is signed with an ephemeral key (integrity within this run); its
    # trust is its content — the functionaries are the author's GitHub keys.
    openssl genpkey -algorithm ed25519 -out "$work/eph.pem" 2>/dev/null
    openssl pkey -in "$work/eph.pem" -pubout -out "$work/eph.pub" 2>/dev/null
    witness sign -f "$work/policy.json" -k "$work/eph.pem" -o "$work/policy.signed.json" \
        -t https://witness.testifysec.com/policy/v0.1 2>/dev/null

    witness verify -p "$work/policy.signed.json" -k "$work/eph.pub" -a "$att" --subjects "$subject_digest" \
        >/dev/null 2>&1 \
        || fail "step '$step' failed verification (not signed by $PR_AUTHOR, tampered, or wrong commit)"

    cmd="$(python3 - "$att" <<'PY'
import base64, json, sys
d = json.load(open(sys.argv[1]))
j = json.loads(base64.b64decode(d["payload"]))
cr = next(a for a in j["predicate"]["attestations"] if "command-run" in a["type"])
print(" ".join(cr["attestation"].get("cmd", [])))
PY
)"
    [[ "$cmd" == *"$(expected_cmd "$step")"* ]] \
        || fail "step '$step' attested the wrong command: $cmd"
    echo "  ✓ $step — signed by $PR_AUTHOR, bound to $HEAD_SHA, command verified"
done

echo "attest-verify: all ${#REQUIRED_STEPS[@]} checks verified for $HEAD_SHA by collaborator $PR_AUTHOR"
