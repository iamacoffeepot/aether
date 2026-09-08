# ADR-0215: The checkout declares its lanes

- **Status:** Accepted
- **Date:** 2026-09-09

## Context

The lane vocabulary exists as five independently compiled copies, and nothing machine-checks that the tree the coordinator checked out can run what the coordinator dispatches.

1. The coordinator's identities and their bit assignments — `VerifyFailure`, `VerifyFailure::ALL`, `from_name`, and `VERIFY_FAILURE_NAMES` (`crates/aether-bloomery/src/values/verify.rs:25`).
2. The coordinator's belief about what each verify position runs — `VerifyGateSet::fold` / `member` / `base`, each a hand-written list (`crates/aether-bloomery/src/values/proof.rs:88`).
3. The lane's actual fan-out — `verify_check_members()` and `Position::runs` (`xtask/src/transform/verify/mod.rs:900`, `:879`).
4. The Actions path's bit table — the jq `def verifier_bit:` block in `.github/workflows/transform.yml:79`, kept in step with copy 1 by a tripwire that renders it from `VerifyFailure::ALL` (`xtask/src/transform/verify/workflow.rs:345`).
5. The lane command spellings. Three of them are shared — `xtask` re-exports `CONSTRUCT_IMPLEMENT_COMMAND` and `REVIEW_CRITIC_COMMAND` and imports `SCOPE_FILL_COMMAND` (`xtask/src/transform/construct.rs:20`, `review.rs:25`, `mod.rs:45`) — and three are not: `VERIFY_CHECK`, `VERIFY_MEMBER`, and `VERIFY_BASE` are re-declared as string literals at `xtask/src/transform/verify/mod.rs:806`–`:823` beside the `VERIFY_*_COMMAND` constants `aether-bloomery` already exports (`crates/aether-bloomery/src/values/stage.rs:49`–`:67`).

Three of those copies have already drifted, all in the same direction — an identity was appended and a copy was not updated:

- `VERIFY_FAILURE_NAMES` is `[&str; 9]` (`verify.rs:191`) against an `ALL` of ten. `verify.lock` (#5309) is missing from the unknown-variant hint a decoder emits.
- ADR-0178's forgiveness bound is stated in code as a comment — `V1 has N = 9 identities` (`crates/aether-bloomery/src/reduce/verify.rs:135`) — while `ALL.len()` is 10. The bound the reducer actually applies is `record.stage_catalog.retry_budget_of(Verify)` plus however many identities happen to be compiled; the `N` half of `N + B` is prose, and the prose is stale.
- ADR-0209 kept the Actions workflow's `printf '%02x'` (`transform.yml:112`) on the reasoning that the lane cannot set bit 8, because `verify.containment` is coordinator-side and never an umbrella member. That reasoning was correct then and #5309 invalidated it: `verify.lock` is bit 9 and *is* an umbrella member (`verify/mod.rs:909`). A `verify.lock` finding on the Actions path renders `printf '%02x' 512` as the three-character token `200`; `VerifyFailureSet::from_mask` accepts only two or four hex digits (`verify.rs:277`), so `claim_for` takes its subject branch, `Digest::from_hex("200")` fails, and the attempt buys no upload (`crates/aether-chassis-bloomery/src/bloomery/intake/claims.rs:78`). It is fail-closed and silent. That defect is independent of this decision and is filed on its own terms; it is cited here because it is what a drifting fifth copy looks like in production.

Nothing in the repository can detect any of this. The gate that would — "is what I am about to dispatch something this tree can run" — has no value to compare against, because there is no single statement of what the tree runs.

Three couplings hold the shape in place, in ascending order of depth.

**The spawn line.** `ProcessTransformRunner` spawns `LaneProgram`, which resolves from `CoordinatorConfig::local_lane_program` and defaults to the compiled `DEFAULT_LANE_PROGRAM = "cargo xtask transform"` (`crates/aether-chassis-bloomery/src/bloomery/executor/local/lane_program.rs:31`, `config.rs:213`). #4727 made it a resolvable derive-`Config` field so a test can point it at a stand-in, which is host configuration; it is not a declaration read from the tree being run, and the runner still appends its own argv (`process_runner.rs:380`).

**The stage vocabulary.** ADR-0174 made the `StageCatalog` sealable, and `validate` (`stage.rs:394`) admits a catalog structurally rather than by equality with the compiled line — that is the whole point of sealing one. But the `process` a binding may name is checked against `is_known_process` (`stage.rs:135`), a hardcoded match over this repository's strings. It grew another compiled entry for ADR-0208's `scope.fill`. So a bloom may seal a *different* line and may not seal a line naming a lane this repository does not compile.

**`VerifyFailureSet`.** ADR-0178 introduced a closed enum of verifier identities in the reducer's value vocabulary, deliberately: an unbounded identity space would let novelty extend the repair loop. ADR-0181 appended the eighth and recorded that the vocabulary was full. ADR-0209 appended the ninth and needed a whole ADR to do it, because a verifier identity touches the mask width, the artifact-name token, the length-based mask/subject disambiguation, the saturation tripwire, ADR-0178's `N + B` bound, and — the expensive one — `VerifyGateSet`, whose digest is half of `VerifiedTree` (`proof.rs:191`), the key of every stored verify-proof memo. ADR-0209's list of what an identity touches is the checklist any repo-declared vocabulary has to answer.

The enumerate-when-enumerable rule cuts the other way here. A verifier set is a property of the repository being verified, not of the coordinator verifying it, so the coordinator's copy of it is a skew edge — and the drift above is what a skew edge produces when nothing checks it.

Two mechanisms make this decidable now that were not available when ADR-0178 was written. ADR-0190 makes replay fold recorded decisions rather than re-decide, so a change to what the seal door admits governs future admissions and cannot re-judge history — `Decision::RecordStageCatalog` (`crates/aether-bloomery/src/reduce/decision.rs:472`) is the existing instance of exactly the pattern this record needs: a value resolved once at admission, journaled, and read by the fold instead of re-derived. And ADR-0199/ADR-0205 make the base a tree the coordinator alone advances, so a file in it is a signed, verified, coordinator-authored input rather than ambient text on a host.

## Decision

The repository declares its own lanes in a manifest at its root; the coordinator reads that manifest out of the sealed base's tree, seals it, and attests it. Vocabulary comes from the repository. Semantics, confinement, and the line's shape stay with the coordinator.

### The manifest

`pipeline.toml` at the repository root — the sibling of `approval-policy.toml`, which is the existing precedent for a checked-in file the coordinator reads. TOML, because a human edits it in a pull request. It carries an integer `version`; the coordinator refuses a version it does not implement, naming the version it read and the range it supports.

```toml
version = 1

[entrypoint]
program = "cargo"
args = ["xtask", "transform"]

[lanes]
model = ["construct.implement", "review.critic", "scope.fill"]
mechanical = ["verify.member", "verify.check", "verify.base"]

[verifiers]
identities = [
  "verify.preflight", "verify.fmt", "verify.clippy", "verify.docs", "verify.test",
  "verify.dup", "verify.deps", "verify.suppress", "verify.containment", "verify.lock",
]

[verifiers.runs]
"verify.member" = ["verify.preflight", "verify.fmt", "verify.clippy", "verify.test", "verify.dup", "verify.deps", "verify.suppress", "verify.lock"]
"verify.check"  = ["verify.preflight", "verify.fmt", "verify.clippy", "verify.docs", "verify.test", "verify.dup", "verify.deps", "verify.suppress", "verify.lock"]
"verify.base"   = ["verify.preflight", "verify.fmt", "verify.clippy", "verify.docs", "verify.test", "verify.dup", "verify.deps", "verify.suppress", "verify.lock"]

[evidence]
envelope = 1
```

`identities` is the vocabulary; `[verifiers.runs]` is what each verify position's fan-out actually runs. The two differ by exactly `verify.containment`, which is a legal identity that no lane runs — ADR-0209's coordinator-side gate, stated as data instead of as an obligation to remember. That discharges the standing obligation ADR-0209 accepted in its Consequences ("a future identity that the lane *does* run must be added to the explicit list, and the tripwire does not tell you when you forgot"): the omission is now expressible only by editing the manifest, where both halves sit side by side.

The declared vocabulary is bounded at 16 identities. That keeps `VerifyFailureSet` a `u16`, keeps the four-hex-digit artifact token ADR-0209 landed, and keeps `N + B` a small number. A repository wanting a seventeenth identity is a further decision, exactly as the ninth was.

### The manifest is a sealed configuration derived from the base, not authored by an operator

The manifest reaches the reducer as a `ConfigRegistry` entry under `Kind::NAME` `aether.bloomery.pipeline_manifest` (ADR-0174), resolved bloom-wide. The host derives it: at draft formation it reads `pipeline.toml` out of the sealed base's tree through the source port, decodes it, encodes the decoded value to canonical wire bytes, stores them content-addressed, and puts the digest in the draft's registry. Everything downstream then works unchanged — `sealed_config::<PipelineManifest>` in the seal door, ADR-0174's "a sealed key the host cannot resolve is a loud failure", and the spec digest covering exactly which manifest the bloom ran under.

Two properties follow from deriving rather than authoring.

`POST /configs` **refuses this kind.** The reducer is `no_std` and cannot fetch, so it cannot check a manifest value against a tree; the only place that check can live is the host that reads both. An operator-authored entry would let a draft attest a vocabulary the base does not carry, which is the divergence ADR-0174 exists to remove. The host derives the entry and refuses a draft that names a different digest for it.

**The sealed bytes are the value, not the file text.** The digest is over the canonical wire encoding of the decoded `PipelineManifest`, so reformatting `pipeline.toml`, reordering its tables, or changing a comment re-seals nothing. Only a change in meaning moves the digest — which matters, because this digest is about to appear in every receipt.

The resolved manifest is journaled as `Decision::RecordPipelineManifest { bloom, manifest }`, appended past the current last `Decision` variant so every prior discriminant is unchanged, and carried on `BloomRecord` beside `stage_catalog`. Under ADR-0190 the fold reads the record; replay never re-derives it from a tree, and a bloom sealed before this decision keeps the compiled fallback at record construction, exactly as `RecordStageCatalog` already provides for.

### A checkout with no manifest is refused, with the base named

A base whose tree carries no `pipeline.toml`, or one whose file does not parse, or one declaring an unimplemented `version`, refuses the seal: `SealError::UnusablePipelineManifest`, naming the base digest, the expected path, and the parse or version failure. It does not fall back to the compiled line. A fallback would re-create the drift this record exists to close, and it would do it silently at exactly the moment the tree and the coordinator disagree most.

The refusal has a bootstrap order, and it is the same wrinkle ADR-0205 recorded for itself: the manifest cannot arrive by the mechanism that requires it. So the file lands first, as ordinary content on the day's base, and the refusal is armed only once every base the coordinator can seal against carries one. The follow-on slices below are ordered on that constraint; it is a sequencing obligation, not a permanent tolerance.

### The seal-time cross-check is a pure function over two values

`StageCatalog::validate` (`stage.rs:394`) stays what it is — a pure, `no_std`, structural check — and gains a sibling `validate_against(&self, manifest: &PipelineManifest) -> Result<(), CatalogError>`. It is equally pure: the manifest arrives as a decoded value because the host already resolved it, which is ADR-0174's rule that whoever resolves an entry must already hold its content. `validate_line` (`crates/aether-bloomery/src/reduce/seal.rs:322`) calls both and keeps returning the resolved catalog, so the door and the dispatch still read one value.

`is_known_process` splits along the line the current match already straddles. Its accepted strings are two different things today: typed lane commands the executor routes (`construct.implement`, `review.critic`, `scope.fill`) and host positions naming coordinator code (`sketch`, `aether.bloomery.api`, `aether.bloomery.approve_gate`, `transform.verify`, `review`, `integrate`, `aggregate-verify`, `aggregate-review`, `source.cas_land`, `retrospect`). The host positions stay compiled — they name the coordinator's own code, not the repository's. The lane commands come from the manifest. A catalog naming a lane command the manifest does not declare is refused at the seal door, where the operator is still holding the catalog they wrote, rather than surfacing as a member that wedges with no attempt ever made.

`is_model_lane` (`stage.rs:95`) becomes a lookup in the manifest's `[lanes]` split rather than a compiled disjunction. It decides which dispatches carry a credential and a resolved model, so it must answer from the value the bloom sealed — a mechanical lane cannot acquire model-lane treatment through host configuration, which is the property that disjunction was written to guarantee, preserved.

### Verifier identity becomes a declared id, and the wire does not move

`VerifyFailure` stops being a closed enum and becomes a validated identity — a bounded string with the `verify.` prefix, a charset, and a length cap. Boundedness, which is what ADR-0178 actually required, moves from "compiled into the coordinator" to "declared in the sealed manifest, at most sixteen": a lane still cannot invent an identity to extend its own repair loop, because an identity outside the sealed vocabulary is refused at intake.

**The persisted shape is unchanged, and that is a checkable claim.** `VerifyFailure`'s `SCHEMA` is `SchemaType::String` with `LABEL` `aether.bloomery.verify_failure`, and the set's is `SchemaType::Vec` of it with `LABEL` `aether.bloomery.verify_failure_set` (`verify.rs:128`–`:138`); the durable form of `Fact::VerifyFailed`, `StageProgress::seen_verify_failures`, and `Wedge::repeated_verifiers` is already the ordered sequence of identity strings, never the mask. Preserving both schema and label byte-identically means no journal or config schema digest moves, so **no `PersistedUpcast` is owed** and `schema-digests.txt` gains no line for this change. ADR-0187's ledger is the tripwire that proves it: an implementation that moves the shape fails the last-line check rather than shipping an unnoticed re-key. If a future manifest change *does* move a persisted shape, ADR-0187's amendment governs — the pin is a literal copied from recorded stamps, never a digest recomputed from live code.

What does change is where an unknown identity is refused. `from_name` against a compiled table becomes membership in the sealed vocabulary, and `WireDecode` has no manifest in hand, so the decoder admits any syntactically valid identity and the vocabulary check moves to intake — the trust boundary ADR-0178 already names for it. Decode is then tolerant where it was strict, and intake is strict where it always was; a malformed row still fails at decode, and a well-formed row naming an undeclared identity fails at admission with the bloom, and so the manifest, in hand.

Canonical order stays the manifest's declaration order, and the set stays a mask over the interned positions of that order. Two consequences worth stating rather than discovering. Sorting is manifest-relative, so a reader with the bytes and no manifest can check dedup but not order; that is the same relativity the mask already had against a binary, moved somewhere it is recorded. And lexicographic order was the tempting alternative — self-describing, no manifest needed — but the current canonical order is not lexicographic, so adopting it would make every historical row fail the decoder's order check and abort replay at boot.

`to_mask` / `from_mask`, the four-hex token, the two-width decode, and the length-based mask/subject disambiguation in the attempt-artifact name are unchanged. The jq `verifier_bit` table keeps being rendered by `workflow.rs` and its tripwire keeps comparing the rendering to the checked-in block; it renders from the manifest instead of from `VerifyFailure::ALL`.

`N` becomes the sealed manifest's `identities.len()` for that bloom. ADR-0178's bound is unchanged in form and finally computed rather than commented: `N + B`, with `N` read off the record the bloom sealed and `B` its sealed `Verify` retry budget. The stale `N = 9` comment at `reduce/verify.rs:135` is deleted rather than corrected — a number that has to be maintained by hand next to the value it describes is the defect.

### `VerifyGateSet` takes its verifiers from the manifest, and the migration is byte-neutral

`VerifyGateSet::fold` / `member` / `base` stop hand-listing identities and read `[verifiers.runs]` for their command. `command` still comes from the position, and `image` and `network` stay compiled (`VERIFY_LANE_IMAGE`, `VERIFY_LANE_NETWORK`).

**The manifest declares vocabulary; the coordinator declares confinement.** A repository naming its own execution image or network posture would let a tree grant itself egress on the coordinator's host, which is precisely the thing ADR-0194's confinement exists to deny. Those two axes of the gate-set identity are the coordinator's, and a manifest carrying a field for either is refused rather than ignored.

Because a `VerifyGateSet` serializes its verifiers as the same ordered sequence of the same strings, a manifest that faithfully transcribes this repository's current vocabulary and per-position lists produces byte-identical gate-set bytes, an identical digest, and therefore an intact `VerifiedTree` memo table. Nothing re-proves. That is not a hope; it is a tripwire — the migration pins `VerifyGateSet::fold().digest()`, `member()`, and `base()` to the digest literals the current binary produces, and a transcription error fails there instead of silently re-keying the verification ledger. It is the same failure ADR-0209 refused to walk into by letting `ALL` widen the gate set, caught by a mechanism instead of by care.

### The calibration ledger keys its columns by identity

ADR-0184's per-verifier failure-mix columns are fixed-width arrays indexed by enum discriminant today — `const IDENTITIES: usize = VerifyFailure::ALL.len()` and `[u64; IDENTITIES]` at `crates/aether-bloomery/src/calibration.rs:94`, `:240`, `:458`, indexed as `identity as usize`. Both properties go: the width is per-manifest, and the position is not stable across manifests. Columns key on the identity string. A cell's column set is the union of the identities observed over the folded window, and the rendered ledger distinguishes *not declared by this bloom's manifest* from *declared and never failed* — ADR-0184's own honesty rule, the one that makes an empty Codex column a rendered fact rather than tribal knowledge, applied to a vocabulary that now varies between rows.

### The lane entrypoint comes from the manifest

`[entrypoint]` supplies the program and leading arguments the dispatch spawns; the coordinator appends the work-order argv as it does today (`process_runner.rs:380`). `DEFAULT_LANE_PROGRAM` is deleted, and `CoordinatorConfig::local_lane_program` demotes from "the production default with a test override" to "a host override only": non-empty overrides the manifest (which is what #4727 built it for), and empty means the sealed manifest rather than a compiled string.

The entrypoint is an argv word list, never a shell string, spawned with the checkout as its working directory — the shape `LaneProgram` already has (`lane_program.rs:63`, whitespace split into `Command::new` plus args, no shell). The manifest is trusted exactly as far as the tree it came from, which is the tree whose `xtask` the coordinator already compiles and executes; under ADR-0205 that tree advances only through a bloom the coordinator itself verified and landed.

### What stays fixed

The manifest declares vocabulary. It never declares semantics.

- **The evidence envelope shape.** `status` (`pass` / `fail` / `environment`, ADR-0176), `nonce`, subject binding, `failed_verifiers`, and the six channel keys `ChannelKind::key` names (`xtask/src/transform/mod.rs:182`). `[evidence].envelope` is a version the coordinator refuses when it does not implement it, not a description it adapts to.
- **Settlement semantics.** Admission, the nonce/subject/digest checks, the three-valued lane verdict, and `MemberOutcome`'s mapping of a fault to `verify.preflight` rather than to a member's own identity.
- **The stage-cursor model.** `StageId` stays a closed compiled vocabulary, and so do `MEMBER_LINE`, `next_member_stage`, completion gates, and retry budgets. This is a deliberate departure from #4884's phrasing that "stages and verifier identities become opaque ids": a stage is the coordinator's line semantics — the reducer decides re-dispatch, repair re-entry, and wedge on it — while the *lane command a stage dispatches* is the repository's. The manifest declares which lane commands exist; `StageCatalog` maps stages onto them; the cross-check joins the two.
- **Confinement.** Execution image and network posture, per the gate-set section above.

## Consequences

- The "can this tree run what I am about to dispatch" question becomes answerable, at the seal door, from one sealed value — and every drift listed in the Context becomes a refusal instead of a silence. The five copies collapse to one declaration plus renderers of it.
- Every receipt gains the vocabulary the bloom ran under. A forensic read of a wedge with `repeated_verifiers = {verify.lock}` can name which identities were even available to that bloom, which today requires knowing which binary was deployed.
- ADR-0209's standing obligation is discharged, and the class of change it represents gets cheaper: appending a verifier identity that the lane runs becomes an edit to `pipeline.toml` plus its lane implementation, reviewed in one diff, with the seal-time cross-check and the gate-set tripwire as the enforcement. It does not stop being a real change — the gate set's digest moves, which re-keys the memo table and re-proves, correctly, because the gates genuinely changed.
- The verification ledger survives the migration intact, or the migration fails loudly. There is no third outcome, which is the property ADR-0209 had to reason its way to by hand.
- `N` stops being a comment. The forgiveness bound is computed from the record.
- Blooms sealed before this decision keep working under ADR-0190: their recorded decisions fold unchanged, and their records fall back to the compiled line. No journal row changes meaning and no upcast is owed — conditional on preserving `VerifyFailure`'s schema and label exactly, which `schema-digests.txt` enforces.
- The coordinator gains a hard dependency on a file in the tree it is dispatching against. A base that loses the file cannot be sealed against, which is a real new way to break the day and is intended: the alternative is dispatching a lane vocabulary nothing agreed to.
- A second repository becomes describable rather than achievable. This record moves the boundary; it ships no second-language lane implementation, and this repository stays the only manifest author until a second one exists. Two things a second repository will still find compiled and will need their own decisions: the execution image per lane command, and `StageId` itself.
- Cost, accepted: one more sealed configuration kind, one more journal decision variant, one file read per draft formation, and a bootstrap ordering that has to be respected once.

## Alternatives considered

- **Leave the vocabulary compiled and add a cross-check between the existing copies.** The cheapest fix, and it is what `workflow.rs`'s tripwire already does for one pair. Rejected: it can only compare copies inside one build, so it cannot see the lane implementations in the tree the coordinator checked out — which is the direction the drift actually runs — and it leaves five copies to keep in step, one more with every identity.
- **Put the vocabulary in the sealed `StageCatalog` instead of in a manifest.** The catalog is already sealable, so this looks free. Rejected: the catalog is an operator's authored choice about *how the line runs* — budgets, profiles, limits — while the vocabulary is a fact about *what the tree can do*. Fusing them lets an operator seal a catalog claiming lanes the tree does not implement, which is the current failure mode with an extra step, and it re-digests every sealed catalog for a value that is not the operator's to choose.
- **Let an operator author the manifest entry through `POST /configs`.** Uniform with every other registry kind. Rejected: the reducer cannot check a manifest against a tree, so nothing would ever verify that the sealed vocabulary is the one the checkout carries — the attested-but-untrue divergence ADR-0174 was written to remove.
- **Fall back to the compiled line when a checkout has no manifest.** Keeps every existing base sealable and makes the change purely additive. Rejected: the fallback is silent at the exact moment the tree and the coordinator disagree, and a fallback that works is a fallback nobody removes. The bootstrap ordering costs one sequencing constraint and buys a decision that means something.
- **Pass the manifest to `reduce` as a new argument rather than through the config registry.** Slightly less machinery. Rejected: `ResolvedConfigs` is already the channel for "content the host fetched so the reducer can read it", and a second parallel channel would be attested by nothing — the spec digest would no longer cover which manifest the bloom ran under.
- **Make `VerifyFailureSet` an unordered set of strings and drop the mask entirely.** Simplest possible declared-id representation. Rejected: it re-keys `VerifyGateSet`, breaks the artifact-name token that ADR-0178 and ADR-0209 built, and changes the canonical encoding of a persisted shape — a byte-neutral migration is available and this is not it.
- **Adopt lexicographic canonical order so a set is self-describing without a manifest.** Genuinely attractive for forensics. Rejected on decode: the current order is declaration order and is not lexicographic, so historical rows would fail the order check in `visit_seq` and boot replay would fatal-abort on the journal this record exists to keep readable.
- **Version the manifest with ADR-0187's schema-digest machinery instead of an integer.** Consistent with how persisted bytes are versioned. Rejected: the file is an *input*, not persisted bytes — the value it decodes to is schema-stamped like everything else once sealed. An integer the reader refuses is legible to the human editing the file, which a digest is not.
- **Let the manifest declare the execution image and network posture too.** The obvious completion, and a second repository will want it. Rejected here: a tree that names its own confinement grants itself egress on the coordinator's host. If a second repository needs its own image, that is host configuration keyed by lane command, and its own decision.
- **Wait for a second repository to exist.** The honest counter-argument: nothing is broken *for this repository* that a careful reviewer could not catch. It does not survive the Context — three copies have already drifted, two of them since ADR-0209 reasoned explicitly about the boundary they crossed, and the third silently drops evidence on the Actions path.

## Follow-on implementation issues

Sliced to land in order. 1–3 are inert additions; 4 arms the first refusal; 5–8 remove the compiled copies one at a time; 9 arms the last refusal, and can only land once every sealable base carries the file. Each coordinator-behaviour slice ships with a `ScenarioHarness` scenario.

1. **`feat(bloomery): the pipeline manifest kind and the repository's own manifest`** — the `PipelineManifest` kind, its TOML reader and version refusal, and the checked-in root `pipeline.toml` transcribing today's vocabulary exactly. Nothing reads it yet. Surface: `crates/aether-bloomery/**`, `pipeline.toml`, `Cargo.lock`.
2. **`feat(bloomery): resolve the pipeline manifest from the sealed base`** — host-side read of `pipeline.toml` out of the base tree at draft formation, store, registry entry, and the `POST /configs` refusal for the kind. Surface: `crates/aether-chassis-bloomery/**`, `crates/aether-bloomery/**`.
3. **`feat(bloomery): record the resolved pipeline manifest on the bloom`** — `Decision::RecordPipelineManifest`, `BloomRecord` carrying it beside `stage_catalog`, compiled fallback for records predating it. Surface: `crates/aether-bloomery/**`, `crates/aether-chassis-bloomery/**`.
4. **`feat(bloomery): cross-check the stage catalog against the manifest at seal`** — `validate_against`, the `is_known_process` split into compiled host positions and declared lane commands, `is_model_lane` from `[lanes]`, the `SealError` variant. Surface: `crates/aether-bloomery/**`, `crates/aether-harness-bloomery/**`.
5. **`refactor(bloomery): verifier identity becomes a declared id`** — `VerifyFailure` to a validated identity, interning against the sealed vocabulary, the decode/intake strictness move, `N` computed from the record, and the schema/label byte-neutrality tripwire. Surface: `crates/aether-bloomery/**`, `crates/aether-chassis-bloomery/**`, `crates/aether-bloomery-github/**`.
6. **`refactor(bloomery): the verify gate sets read the manifest`** — `VerifyGateSet::{fold,member,base}` from `[verifiers.runs]`, image and network still compiled, and the pinned gate-set digest literals proving the memo table survives. Surface: `crates/aether-bloomery/**`.
7. **`refactor(bloomery): the calibration ledger keys its verifier columns by identity`** — ADR-0184's fixed arrays to identity-keyed columns, and the not-declared / never-failed distinction in the rendered ledger. Surface: `crates/aether-bloomery/**`.
8. **`refactor(xtask): the verify lane reads its vocabulary from the manifest`** — the re-declared `VERIFY_*` literals, `verify_check_members`, `Position::runs`, and the jq table renderer all sourced from the manifest. Surface: `xtask/**`, `.github/workflows/transform.yml`.
9. **`feat(bloomery): the lane entrypoint comes from the manifest, and a manifestless base is refused`** — `[entrypoint]` into `LaneProgram`, `DEFAULT_LANE_PROGRAM` deleted, `local_lane_program` demoted to an override, and the seal refusal armed. Surface: `crates/aether-chassis-bloomery/**`, `crates/aether-bloomery/**`, `crates/aether-harness-bloomery/**`.
