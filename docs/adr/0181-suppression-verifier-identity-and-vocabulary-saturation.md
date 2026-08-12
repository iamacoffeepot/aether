# ADR-0181: Suppression verifier identity and vocabulary saturation

- **Status:** Accepted
- **Date:** 2026-08-12

## Context

ADR-0178 fixed a closed vocabulary of seven verifier-failure identities — `verify.preflight`, `verify.fmt`, `verify.clippy`, `verify.docs`, `verify.test`, `verify.dup`, `verify.deps` — and required another decision before that vocabulary grows, because its size sets the maximum number of roll-free failing `Verify` rounds a member may take.

An eighth umbrella member landed without one. `verify_check_members()` in `xtask/src/transform/verify/mod.rs` now fans `verify.check` out to `verify.suppress` plus the six mechanical checks, mirroring CI's required `New suppressions` job. `VerifyFailure` still carries seven identities and `VerifyFailure::from_name` returns `None` for `verify.suppress`, so `failed_verifiers()` silently drops it from the projected set. The counts agreeing at seven on both sides is what hid the mismatch; the two sevens are different sets.

A run in which `verify.suppress` is the only failing member therefore emits `status: "fail"` with `failed_verifiers` absent — the exact shape ADR-0178 declares invalid at the trust boundary — and neither transport admits it. The Actions wrapper's `jq` guard raises `failing evidence must carry failed_verifiers`, the artifact-name step exits non-zero, and no attempt artifact is uploaded. The local backend reads the absent field as the valid empty set, produces a `VerificationFailed` verdict carrying `VerifyFailureSet::EMPTY`, and intake's `verifier_failure_refusal` refuses it as `InvalidVerifierFailures` without consuming the order. Either way the member's dispatch is never answered and stalls to its ADR-0177 deadline. The member cannot consume a repair roll, cannot be forgiven, and cannot be reported.

The gap matters more than its size suggests. A `Refine` re-entry is under direct pressure to make a verifier green, and `#[allow(…)]` is the cheapest way to do it — the suppression scanner exists precisely because a suppression makes the tool it targets pass by construction. It is the quiet failure the other six cannot see, and it is the one identity the loop currently cannot account for.

Two facts about the transport bound the decision. The durable form of a failure set is an array of canonical identity strings — that is what `Fact::VerifyFailed`, `StageProgress::seen_verify_failures`, and `Wedge::repeated_verifiers` encode. The compact seven-bit mask exists only as the two-lowercase-hex-digit token in an attempt artifact's name, decoded at intake and never stored. `VerifyFailureSet` is a `u8` whose `KNOWN_MASK` is `0x7f`, so exactly one bit remains.

## Decision

### `verify.suppress` becomes the eighth identity

Add `VerifyFailure::Suppress`, spelled `verify.suppress`, **appended to the end** of the canonical order:

1. `verify.preflight`
2. `verify.fmt`
3. `verify.clippy`
4. `verify.docs`
5. `verify.test`
6. `verify.dup`
7. `verify.deps`
8. `verify.suppress`

The alternative — dropping `verify.suppress` from `verify_check_members()` so the umbrella returns to six reportable members — is rejected. It would silence the lane's only defense against a candidate making another verifier green by construction, in the one execution context where nobody reads the diff before it integrates.

The canonical vocabulary order is **append-only and independent of the umbrella's run order**. It already is: `verify.preflight` is a synthetic pre-member identity that no run-order list contains, and `verify_check_members()` runs `verify.suppress` first for CI-job parity. Appending is what keeps every deployed bit assignment intact; the run order stays free to change for parity reasons without touching the wire.

### The vocabulary saturates the byte

At eight identities every bit of the mask byte is assigned, so `KNOWN_MASK` becomes `0xff` and the `value & !KNOWN_MASK == 0` guard it existed for reduces to `value & 0 == 0`. The constant and the guard therefore go away together rather than being kept as an always-true test — `clippy::bad_bit_mask` denies the masked comparison outright, and silencing it to preserve a check that can no longer fail would be a suppression in the very change that makes suppressions accountable. Three consequences follow and are accepted deliberately.

`VerifyFailureSet` still fits one `u8`, and `to_mask` still renders exactly two lowercase hex digits, so the attempt-artifact name grammar `attempt.<verdict>.<mask>.<subject>.<detail>.<nonce>` is **unchanged in shape**. The two-character mask field stays unambiguous against a 64-hex subject, so `NameEvidenceClaims::claim_for`'s length-based disambiguation is untouched.

`from_mask`'s unknown-high-bit refusal disappears: every two-lowercase-hex-digit token now names a valid set, and what remains is the length and lowercase-hex charset check. On the GitHub path the reference set and the claimed set are both derived from that same token, so their agreement check was already tautological there; after this change the only semantic validation of a mask on that path is the workflow's own `jq` table, which must therefore gain `verify.suppress` in the same change. The evidence body's content digest continues to bind the bytes, and the local path continues to cross-check the body-derived set against the name-derived one.

A **ninth** identity does not fit. It requires widening the set to `u16` and the token to four hex digits, which is a coordinated change to the artifact-name grammar and both backends' decoders. That is a materially larger decision than this one, and this ADR states the cost so a future author does not discover it mid-implementation.

### Forgiveness arithmetic

ADR-0178's bound is `N + B`: at most `N` failing member-`Verify` verdicts can carry an empty repeated set, because each must introduce at least one identity absent from `S`, and at most `B` later verdicts spend the sealed `Verify` retry budget before the member wedges.

`N` moves from 7 to 8. Nothing else moves. Specifically:

- `B`, the sealed `Verify` retry budget, is unchanged.
- The wedge trigger is unchanged: a verdict whose `R = F ∩ S` is non-empty spends exactly one roll, and a roll reaching `B` wedges.
- The **worst-case roll-free ceiling** per member rises by exactly one round, from seven to eight.

That ceiling is only reachable by a member that introduces a brand-new identity in every successive round and never repeats one. It is not one extra forgiven round for every member. The umbrella runs every member on every dispatch, so a member failing *k* checks in its first round seeds `S` with all *k* at once and any repeat in the next round spends a roll immediately; the realistic path is one forgiven round followed by charged ones, and the eighth identity does not lengthen it.

The change that actually bites is the one this ADR exists for: a `verify.suppress` failure becomes accountable. First occurrence for a member is forgiven like any other identity, a repeat spends a roll, and a repeat at budget wedges with `repeated_verifiers = {verify.suppress}` visible in the stored `Wedge`, the reducer outcome, and the outward view.

### Wire compatibility

Already-journaled evidence carrying the seven-identity vocabulary decodes unchanged, and no stored byte changes meaning.

The durable encoding is the name array. Appending an identity preserves the relative order of the existing seven, so a historical array is still in canonical order under the widened `Ord` and still decodes to the same set. No historical value contains `verify.suppress`, so no historical value gains a member. `Fact::VerifyFailed`, `StageProgress`, and `Wedge` need no migration, and the journal-compatibility policy is not engaged.

The mask is transient. Appending leaves bits 0–6 assigned exactly as before, so a mask emitted by the pre-change workflow decodes identically under the post-change table, and a mask emitted by the pre-change local backend does too. The only asymmetric direction is a new mask with bit 7 set reaching a decoder that still refuses it — `from_mask` returns `None`, `claim_for` yields no upload, and the artifact is skipped. That is fail-closed, confined to a mixed-version window, and cleared by restarting the coordinator on the new binary. Landing the widened decoder before or with the widened `jq` table keeps even that window shut.

## Consequences

- The lane's suppression gate becomes a first-class, repairable, reportable failure instead of a lane-killing one. A `Refine` re-entry that suppresses a lint is now told so, charged for repeating it, and wedged visibly if it keeps doing so.
- `VerifyFailure`, its `ALL` array, `VERIFY_FAILURE_NAMES`, the retired `KNOWN_MASK`, and the Actions `verifier_bit` table are one coordinated pre-1.0 change; the durable journal encoding is not part of it.
- The failure vocabulary is full. A ninth identity is a wire break against the two-hex-digit mask token and requires its own decision, not an appended variant. A tripwire pins the full set's mask at `ff`, so the ninth is caught at that boundary instead of truncating into the token.
- The invariant whose absence caused this — every id in `verify_check_members()` resolves through `VerifyFailure::from_name` — becomes an enforced tripwire rather than a coincidence of two lists having the same length.
- The mask token retains no semantic validation of its own once every bit is known. Integrity on the Actions path rests on the workflow's canonical-order and duplicate checks plus the evidence digest, which is where ADR-0178 already placed the trust boundary.
- The umbrella's run order and the vocabulary's canonical order are now explicitly separate concerns. Future CI-parity reordering of `verify_check_members()` carries no wire consequence.

## Alternatives considered

- **Remove `verify.suppress` from `verify_check_members()`** — restores a consistent seven-identity vocabulary at the cost of the lane no longer running the one check that catches a candidate making another verifier green by construction. The autonomous loop is exactly the context that check exists for.
- **Insert `Suppress` at the front to match the umbrella's run order** — reassigns bits 1–6, so every in-flight artifact name emitted by the other side of the deploy decodes to the wrong set. It buys a cosmetic alignment the vocabulary never had, since `verify.preflight` is not a run-order member either.
- **Map `verify.suppress` onto `Preflight`** — `Preflight` means the host could not run the check. A suppression finding is a candidate-owned defect; conflating them would charge a real defect against the identity reserved for environment faults and make both unreadable in a wedge projection.
- **Relax the trust-boundary invariant so a failing `Verify` may carry an empty set** — the smallest diff and the worst one. An untyped failure can never be forgiven or charged, which reinstates the one-roll-per-umbrella-run collapse ADR-0178 removed, and it makes the invalid shape indistinguishable from a genuine encoder bug.
- **Widen the set to `u16` and the token to four hex digits now** — pays a coordinated artifact-name change for headroom nothing needs today. The eighth identity fits the existing grammar exactly; the cost of a ninth is recorded above rather than pre-paid.
