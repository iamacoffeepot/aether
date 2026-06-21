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

# Canonical check set: shared with the producer (scripts/attest.sh) via
# scripts/checks.sh, so the command each attestation records and the fragment
# this verifier matches against cannot drift. The fragment binds each attestation
# to the real check, so one that ran `true` under the name "clippy" is rejected.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/checks.sh
source "$SCRIPT_DIR/checks.sh"
read -ra REQUIRED_STEPS <<< "$CANONICAL_STEPS"

fail() { echo "::error::attest-verify: $*"; exit 1; }

# 1. The author must be a write-collaborator. Only someone with push access can
#    publish the attestation ref this gate reads, so a present ref is already an
#    authenticated opt-in by a collaborator; this check is the matching guard on
#    the PR author. A non-collaborator (typically a fork PR) cannot opt in, so
#    the gate passes through and the real CI jobs gate their PR instead.
perm="$(gh api "repos/$REPO/collaborators/$PR_AUTHOR/permission" -q .permission 2>/dev/null)" \
    || fail "cannot read collaborator permission for $PR_AUTHOR"
case "$perm" in
    admin|write|maintain) ;;
    *) echo "attest-verify: $PR_AUTHOR is not a write-collaborator (permission=$perm); deferring to real CI."; exit 0 ;;
esac

# 2. Attestation is opt-in. The author opts into the attested path by publishing
#    refs/attestations/<sha>; with no such ref they did not opt in, so the gate
#    passes through and real CI gates the PR. That ref's presence is also what
#    makes the heavy CI jobs skip (see ci.yml's `changes` job), so validating it
#    here is the matching gate for the path the author chose.
git fetch --quiet origin "+refs/attestations/$HEAD_SHA:refs/attestations/incoming" 2>/dev/null \
    || { echo "attest-verify: no attestation published for $HEAD_SHA; author did not opt into the attested path, deferring to real CI."; exit 0; }

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

# 4. Verify each required step: present, signed by one of the author's keys,
#    proven by witness's git attestor to have run on a clean checkout of this
#    exact commit, and recording the canonical command. The commit sha is the git
#    attestor's `commithash` subject, so it is passed directly to `--subjects`.
for step in "${REQUIRED_STEPS[@]}"; do
    att="$work/$step.att.json"
    git show "refs/attestations/incoming:$step.json" > "$att" 2>/dev/null \
        || fail "missing attestation for required step '$step'"

    cat > "$work/policy.json" <<EOF
{ "expires":"2099-01-01T00:00:00Z",
  "publickeys": { $keys_json },
  "steps": { "$step": { "name":"$step", "functionaries":[ $funcs_json ],
    "attestations":[
      {"type":"https://witness.dev/attestations/git/v0.1","regopolicies":[]},
      {"type":"https://witness.dev/attestations/material/v0.1","regopolicies":[]},
      {"type":"https://witness.dev/attestations/command-run/v0.1","regopolicies":[]}]}}}
EOF
    # The policy is signed with an ephemeral key (integrity within this run); its
    # trust is its content — the functionaries are the author's GitHub keys.
    openssl genpkey -algorithm ed25519 -out "$work/eph.pem" 2>/dev/null
    openssl pkey -in "$work/eph.pem" -pubout -out "$work/eph.pub" 2>/dev/null
    witness sign -f "$work/policy.json" -k "$work/eph.pem" -o "$work/policy.signed.json" \
        -t https://witness.testifysec.com/policy/v0.1 2>/dev/null

    # Signature + policy + commit binding: --subjects requires the attestation's
    # git `commithash` subject to equal the PR head.
    witness verify -p "$work/policy.signed.json" -k "$work/eph.pub" -a "$att" --subjects "$HEAD_SHA" \
        >/dev/null 2>&1 \
        || fail "step '$step' failed verification (not signed by $PR_AUTHOR, tampered, or wrong commit)"

    # Read the git attestor's commit + tree-status count and the recorded command
    # from one parse. A non-empty status means modified or untracked files were
    # present when the check ran, so the result does not reflect the committed
    # code — reject it.
    read -r v_commit v_dirty v_cmd < <(python3 - "$att" <<'PY'
import base64, json, sys
d = json.load(open(sys.argv[1]))
j = json.loads(base64.b64decode(d["payload"]))
atts = j["predicate"]["attestations"]
git = next(a for a in atts if "/git/v0.1" in a["type"])["attestation"]
cr  = next(a for a in atts if "command-run" in a["type"])["attestation"]
status = git.get("status") or {}
print(git.get("commithash", ""), len(status), " ".join(cr.get("cmd", [])))
PY
)
    [[ "$v_commit" == "$HEAD_SHA" ]] \
        || fail "step '$step' attests commit $v_commit, not the PR head $HEAD_SHA"
    [[ "$v_dirty" == "0" ]] \
        || fail "step '$step' ran on a dirty tree ($v_dirty modified/untracked files); result does not reflect the committed code"
    [[ "$v_cmd" == *"$(canonical_cmd "$step")"* ]] \
        || fail "step '$step' attested the wrong command: $v_cmd"
    echo "  ✓ $step — signed by $PR_AUTHOR, ran on a clean checkout of $HEAD_SHA, command verified"
done

echo "attest-verify: all ${#REQUIRED_STEPS[@]} checks verified for $HEAD_SHA by collaborator $PR_AUTHOR"
