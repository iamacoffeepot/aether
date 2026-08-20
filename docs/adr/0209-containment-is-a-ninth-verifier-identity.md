# ADR-0209: Containment is a ninth verifier identity

- **Status:** Proposed
- **Date:** 2026-08-20

## Context

Declared-surface containment is a real gate with no identity of its own. `out_of_surface` (`crates/aether-chassis-bloomery/src/bloomery/verify/containment.rs:23`) computes the sorted, deduplicated list of paths a candidate changed that no glob in its member's sealed surface covers; the local backend calls it at `crates/aether-chassis-bloomery/src/bloomery/executor/local/backend.rs:1431` and overlays the result onto the member's Verify verdict at `backend.rs:1635`. When the mechanical umbrella found nothing of its own, `apply_containment` fails the verdict and stamps `VerifyFailureSet::one(VerifyFailure::Test)` (`containment.rs:90`). Its doc comment states the reason without disguising it:

> A ninth identity would need a wider mask; this reuses the "candidate is wrong" class and leaves the paths as the named failure.

Two things follow, and neither is small.

A member refused for reaching outside its authorized surface is byte-identical in the journal to a member whose tests failed. `Fact::VerifyFailed` (`crates/aether-bloomery/src/reduce/event.rs:282`) carries `bloom`, `workpiece`, `evidence`, and `failed_verifiers`, and the set it carries reads `verify.test` for both causes. The two invite opposite remedies — a test failure is a repair, a containment refusal is a question about the scope — and the reducer, the calibration ledger (`crates/aether-bloomery/src/calibration.rs:276`), and every outward projection see one class.

The paths are computed and then discarded. `containment_findings` (`containment.rs:98`) renders them into prose, and prose is not a journaled field. The only text record is `review_findings`, written `INSERT OR REPLACE` per `(bloom, workpiece)` at `crates/aether-chassis-bloomery/src/store/runtime.rs:1301` and cleared the moment the member passes (`crates/aether-chassis-bloomery/src/bloomery/intake/admit.rs:715`). A member refused and later repaired leaves nothing at all — which is the common case, since the loop exists to repair.

The cost is measurement. #5297's survey establishes seven members observably refused by containment against a denominator of 187 verify failures, with the true figure anywhere in `[7, 52]` and no stored artifact able to narrow it. ADR-0208 builds a producer for declared surfaces and needs that rate to calibrate against; a number with a sevenfold spread, further contaminated by the sibling-surface defect that accounts for 13 of 25 observed offending paths, is not a calibration target.

### Why a ninth identity is its own decision

ADR-0181 appended `verify.suppress` as the eighth identity and recorded the boundary it landed on. Its Consequences section states:

> The failure vocabulary is full. A ninth identity is a wire break against the two-hex-digit mask token and requires its own decision, not an appended variant. A tripwire pins the full set's mask at `ff`, so the ninth is caught at that boundary instead of truncating into the token.

That is not rhetoric. `VerifyFailureSet` is a newtype over `u8` (`crates/aether-bloomery/src/values/verify.rs:160`); `VerifyFailure::bit` returns `u8` and is `1 << (self as u8)` (`verify.rs:77`); `ALL` is `[Self; 8]` (`verify.rs:43`) and `VERIFY_FAILURE_NAMES` is `[&str; 8]` (`verify.rs:143`). A ninth variant forces, exactly:

- the integer widening — `bit()` shifting by 8 overflows the byte, and with overflow checks on (the profile `cargo test` and CI use) it panics rather than wrapping;
- the token width — `to_mask` renders `{:02x}` (`verify.rs:204`);
- the decoder's width check — `from_mask` refuses any token whose `len() != 2` (`verify.rs:217`);
- the length-based mask/subject disambiguation in the attempt-artifact name, where a two-character field in the mask position is read as a mask and anything else as a 64-hex subject (`crates/aether-chassis-bloomery/src/bloomery/intake/claims.rs:78`);
- the saturation tripwire pinning the full set at `"ff"` (`verify.rs:346`), whose own comment says the panic is the signal and that a release-profile run would not catch the ninth;
- ADR-0178's forgiveness bound `N + B`, where `N` is the vocabulary size — carried in code as a comment reading `V1 has N = 8 identities` (`crates/aether-bloomery/src/reduce/verify.rs:91`).

### What is not broken

The durable form of a failure set is not the mask. `Serialize for VerifyFailureSet` (`verify.rs:230`) emits a length-prefixed sequence of canonical identity strings, and that is what `Fact::VerifyFailed`, `StageProgress::seen_verify_failures`, and `Wedge::repeated_verifiers` encode. The mask exists only as the token in an attempt artifact's name, composed at `claims.rs:61` and decoded at intake. Widening the in-memory representation therefore changes no stored byte, and appending an identity to the end of the canonical order leaves every existing name array in canonical order and decoding to the same set — the same argument ADR-0181 made for the eighth, unchanged in force for the ninth.

One thing genuinely would break if handled carelessly. `VerifyGateSet::lane()` builds its verifier set as `VerifyFailure::ALL.into_iter().collect()` (`crates/aether-bloomery/src/values/proof.rs:78`) and is content-addressed; its digest is half of `VerifiedTree` (`proof.rs:100`), which keys every stored verify proof memo, looked up at `crates/aether-bloomery/src/reduce/snapshot.rs:1236`. A ninth identity flowing into `ALL` silently re-keys the entire verification ledger and re-proves everything already proven. The type's own doc calls a re-prove "the ordinary consequence of keying on identity" (`proof.rs:69`), which is right when the gates actually changed; here the gates would not have changed at all. The existing tripwire at `proof.rs:162` compares the axes against each other rather than against a fixed expectation, so it would not catch this.

### Why the paths cannot ride on the existing fact

#5297 asks for the violating paths as a typed list on `Fact::VerifyFailed`. The wire refuses it.

Struct variants encode as a bare four-byte little-endian variant index followed by their fields in declaration order, with no field count and no names (`crates/aether-data/src/wire/ser.rs:202`). Decoding reads exactly `fields.len()` values, taken from the type's compiled field list rather than from the bytes (`crates/aether-data/src/wire/de.rs:301`). A read past the end of the buffer is `Error::UnexpectedEof` (`de.rs:34`), and `from_bytes` requires every byte consumed, returning `Error::TrailingBytes` otherwise (`crates/aether-data/src/wire/mod.rs:106`). Appending a field to `Fact::VerifyFailed` therefore makes every historical `VerifyFailed` row fail to decode, in the strict direction: the decoder asks for a fifth field that the bytes do not have.

Boot replay decodes exactly that way and aborts on exactly that failure. `on_replay_result` folds each journal record through `from_bytes` (`crates/aether-chassis-bloomery/src/control/runtime.rs:572`) and calls `ctx.fatal_abort` when a record does not decode (`runtime.rs:574`) — deliberately, because ADR-0063 prefers a fail-fast to coming up on a torn snapshot. Widening the fact would stop the coordinator from starting against its own journal, which is the journal this decision exists to make useful.

## Decision

### `verify.containment` becomes the ninth identity

Append `VerifyFailure::Containment`, spelled `verify.containment`, to the end of the canonical order after `Suppress`. Appending is what preserves every deployed bit assignment: bits 0–7 keep their identities, so a mask emitted by a pre-change binary decodes to the same set under the post-change table, and no historical name array gains or loses a member.

The identity is coordinator-side. `verify_check_members()` (`xtask/src/transform/verify/mod.rs:429`) fans `verify.check` out to seven mechanical members and does not name containment; containment is applied by the local backend over the umbrella's result, against surface globs sealed at admission that the lane never sees. It is a verifier identity because the accounting is keyed on identity — `seen_verify_failures`, `repeated_verifiers`, the wedge projection, the per-lane calibration table — not because a lane member produces it.

### The set widens to `u16`; the token is decoded at both widths and emitted at the width its producer can reach

`VerifyFailureSet` becomes a newtype over `u16` and `bit()` widens with it. `to_mask` renders four lowercase hex digits. `from_mask` accepts **either** two digits, zero-extended, or four — a legacy `"ff"` names bits 0–7 and zero-extends to exactly the same eight identities it named before, so the widened decoder is exact rather than merely tolerant. `claims.rs`'s length disambiguation accepts both widths; a subject is 64 hex, so neither is ambiguous against it.

The Actions workflow's `printf '%02x'` (`.github/workflows/transform.yml:110`) stays at two digits. The lane cannot produce bit 8 — its `verifier_bit` table is built from the umbrella's members and would reject `verify.containment` outright — so a two-digit token remains a complete and correct rendering of everything that path can emit, and widening it would only open a window in which a not-yet-restarted coordinator refuses a four-digit token it has no need to read. The header comment at `transform.yml:18` asserting an exact two-digit mask changes, because after this decision it describes the emitter rather than the decoder and would otherwise misdescribe both.

The local path emits and decodes inside one binary, so its widened rendering has no mixed-version exposure in the forward direction. The one asymmetric direction is a rollback: an attempt artifact named by the new binary carries a four-digit token that the old binary's `from_mask` refuses, `claim_for` yields no upload, and the artifact is skipped. That is fail-closed, confined to a rollback window, and cleared by running the new binary.

The saturation tripwire moves with the width and keeps doing its job: it pins the full vocabulary's mask at `01ff` and gains a round trip asserting that a legacy two-digit token decodes to the same set as its zero-extended four-digit form.

### The lane's gate set does not gain the identity

`VerifyGateSet::lane()` is built from an explicit list of the umbrella identities the compiled lane actually runs, and `Containment` is not among them. A tripwire pins that the compiled gate set contains no `Containment`, naming the memo key it protects.

This is the one place where the general rule — a changed vocabulary is a changed identity — gives the wrong answer, because the gate set describes what the lane runs and the lane does not run containment. Letting `ALL` widen the gate set would invalidate every stored proof to record a gate that never executed under it.

### The paths ride an appended fact

Add `Fact::ContainmentRefused { bloom, workpiece, evidence, failed_verifiers, violating_paths }`, appended past `FoldRefused` so every prior variant keeps its wire discriminant — the same move ADR-0178 made for `VerifyFailed` itself, and for the same reason. Historical records decode unchanged because nothing about their bytes or their field counts moves.

The reducer routes it to the same handler as `Fact::VerifyFailed` with the same arguments. The paths are journal payload the reducer never reads, so the outcome, the effects, the roll accounting, and the pinned decision-stream digest are all unchanged; what changes is that the bytes exist. It reuses `AdmissionKey::VerifyFailed`, exactly as `Fact::VerifyHostFault` already does (`crates/aether-chassis-bloomery/src/bloomery/intake/admit.rs:497`), so `AdmissionKey::ALL: [Self; 8]` (`crates/aether-chassis-bloomery/src/bloomery/intake/admission_key.rs:53`) is untouched and one dispatch is still answered exactly once.

Three matches over `Fact` are exhaustive by intent and gain an arm: the reducer's dispatch (`crates/aether-bloomery/src/reduce/mod.rs:114`), `event_bloom` (`crates/aether-chassis-bloomery/src/control/runtime.rs:1259`, whose doc states the exhaustiveness is deliberate), and `fact_blooms` (`crates/aether-chassis-bloomery/src/api/runtime/reads/journal.rs:98`). Every other reader is a tolerant `if let` or `filter_map`.

Carrying the paths from the gate to intake needs no new trust channel. `EvidenceRef` (`crates/aether-bloomery/src/port/executor.rs:104`) already carries `candidate`, `findings`, and `cost` as host-recorded state explicitly outside the artifact-name contract; the path list joins them and is copied onto `UploadedEvidence` the same way `findings` is (`claims.rs:119`). The local backend already holds the exact `Vec<String>` at the `apply_containment` call site.

`review_findings` keeps its current behaviour. It is a live projection of the current attempt and should stay one; the fix is that a durable record now exists elsewhere.

### Forgiveness arithmetic

ADR-0178's bound is `N + B`. `N` moves from 8 to 9 and nothing else moves: the sealed `Verify` retry budget `B` is unchanged, the wedge trigger is unchanged, and the worst-case roll-free ceiling per member rises by exactly one round. That ceiling is reachable only by a member that introduces a brand-new identity every round and never repeats one, which is not the shape a repairing member has.

The change that bites is the one this decision exists for. A member that goes outside its surface is now told which identity it failed, forgiven the first time, charged a roll for repeating it, and wedged with `repeated_verifiers = {verify.containment}` visible if it keeps doing so — the correct reading, since a member repeatedly reaching outside its declared surface is a scope problem worth surfacing rather than a candidate defect to keep repairing.

## Consequences

The containment refusal rate becomes answerable by query. Today it is bounded only to a range: seven members observable, a true figure somewhere in `[7, 52]` of 187 verify failures, and no archive able to narrow it, because the identity is shared with `verify.test` and the paths were never written down. Afterwards, "how many members were refused, over what denominator, against which paths" is a decode over journaled events. That measurement gap currently blocks calibrating the scoping producer ADR-0208 decides, and the sibling-surface confound — a member blamed for its neighbour's files — becomes separable by comparing journaled paths against journaled surfaces instead of by hand.

The failure vocabulary is no longer saturated, and the headroom is real: a `u16` set leaves seven unassigned bits, so a tenth identity is an append rather than another wire decision. The tripwire moves rather than being retired, so the next saturation is still caught at the boundary.

The mask token now has two accepted widths on the wire, permanently. That is a decoder that must stay bivalent as long as any two-digit emitter exists, and the Actions workflow is deliberately kept as one. The cost is one branch in `from_mask` and one in the name disambiguation, both pinned by tests; the benefit is that neither deploy direction has a window where a valid artifact is refused, except the rollback case stated above.

`VerifyFailure`, its `ALL` array, `VERIFY_FAILURE_NAMES`, the set's width, the mask codec, the containment overlay, the evidence reference, the appended fact, the three exhaustive matches, and the calibration fold are one coordinated pre-1.0 change. The durable journal encoding is not part of it — no stored byte changes meaning, and the journal-compatibility policy is not engaged.

`VerifyGateSet::lane()` stops being derivable from the vocabulary. That is a small ongoing obligation: a future identity that the lane *does* run must be added to the explicit list, and the tripwire that protects the memo key does not tell you when you forgot to add one. The alternative obligation — remembering to exclude — is the one that silently invalidates a ledger, so this is the safer direction to owe.

## Alternatives considered

- **Keep reusing `Test` and recover the intent from the findings prose.** The prose is not journaled at all; the only copy lives in a mutable projection that is deleted when the member passes. Even if it were durable, recovering typed accounting from diagnostics text is exactly what ADR-0178 refused, and the accounting is the whole point: an identity is what makes a refusal forgivable, chargeable, and reportable.

- **Write the violating paths to a side table outside the journal.** Cheap and wire-safe, and it reproduces the defect it is meant to fix. The journal is the append-only record the coordinator replays; anything beside it is a projection with its own lifetime, and the measurement this decision exists to enable is precisely a query over what survives.

- **Add `violating_paths` to `Fact::VerifyFailed`, as #5297's step 2 asks.** The wire is positional and untagged with no field count, and `from_bytes` rejects both short and trailing input, so every already-journaled `VerifyFailed` row stops decoding and boot replay fatal-aborts. The coordinator would not start against the history this decision exists to preserve.

- **Skip the ninth identity and let the new fact's discriminant carry the meaning.** Wire-safe and half-right: the fact would distinguish the cause, but the repair accounting is keyed on `VerifyFailureSet`, so a containment refusal would still be charged as `verify.test` or as nothing. The two causes would remain one class everywhere a roll is spent.

- **Let `VerifyGateSet::lane()` pick the ninth identity up from `ALL`.** The smallest diff and the most expensive one: it re-keys every stored verify proof memo and re-proves the whole ledger, to record a gate the lane does not run.

- **Widen the Actions `printf` to `%04x` in the same change.** It buys nothing, because the lane cannot set bit 8, and it opens a window in which a not-yet-restarted coordinator refuses a token it never needed to read.

- **Amend ADR-0181 in place.** It avoids a new number at the cost of erasing the reasoning a future author needs — ADR-0181's stated decision is that the vocabulary saturates, and that finding is what makes this one necessary rather than optional. Both records stand, cross-linked.
