# ADR-0216: The Line Ends In A Reader That Files What It Will Not Fix

- **Status:** Accepted
- **Date:** 2026-09-09

## Context

The line's closed stage vocabulary ends at `Study` (`stage_vocabulary!` in `crates/aether-bloomery/src/ids.rs`). Every stage before it runs. `Study` is declared and unrun:

```rust
StageId::Study => (&["bloom.receipt"], &["bloom.study"], "retrospect", "study-recorded", 1, 3_600),
```

That binding is in `StageCatalog::binding_of` (`crates/aether-bloomery/src/values/stage.rs`). It consumes `Land`'s product and produces an artifact tag nothing writes. `dispatched_command(StageId::Study)` returns `None`, so no executor routes it. `StageCatalog::profile_of` calibrates it `(Harness::Claude, OPUS_MODEL, ReasoningEffort::High)` and the comment beside that row states the calibration is inert, "Study is not dispatched as a worker lane". The reactor's `timeout_verdict` (`crates/aether-chassis-bloomery/src/bloomery/reactor/executor/runtime/mod.rs`) groups `Study` with the bloom-level tail "the coordinator performs itself", so no order can carry it and none can expire. `ModelOverride::validate`'s test helper resolves no command for it, so a sealed per-member override key naming it is refused. `priority_of` bands it `Start` (`crates/aether-chassis-bloomery/src/bloomery/executor/local/priority.rs`). The slot is fully described and entirely empty.

`crates/aether-bloomery/src/study_report.rs` is not this and must not be mistaken for it. `grade` is a pure read that folds a bloom's admitted `EvidenceKind::StudyRecord` cost artifacts — tokens, worker seconds, and retries read off the dispatch ledger — against its sealed `Forecast` (ADR-0151 / ADR-0180). Those records are uploaded by `construct.implement` attempts and admitted by `admit_study`. The word is shared; the subject is not. Forecast grading measures what a bloom *cost*. Nothing measures what it *left behind*.

What reads a landed batch today is a person. On 2026-09-06 that pass was run by hand over the estate and produced roughly fifty `Bloomery review F00NN` issues (#5557–#5610), filed under "we'll have agents pick them up later". That is the demand, and it also fixes the artifact's shape: the pass was worth running because its output was work orders somebody could pick up. A finding that lands anywhere else is a finding nobody reads.

The estate has already decided the disposition and left it without a mechanism. ADR-0191 §4 says a composition-review finding genuinely about member code rather than the weave "is recorded and filed as new work for a future bloom — fix-forward". Nothing files it. The only sender of `CreateCommission` is `ApiCapabilityState::create_commission`, behind `authorize` against the configured control token (`crates/aether-chassis-bloomery/src/api/runtime/commissions/mod.rs`), and a commission becomes sealable work only through a signed Approve statement: `signed_approval` mints it (`crates/aether-bloomery/src/values/statement.rs`), `classify_approval` refuses `Provenance::StageReceipt` with `CommissionError::WrongProvenance` (`crates/aether-chassis-bloomery/src/store/commission/mod.rs`), and the seal door requires each member's `EvidenceKind::Approval` to validate that member's own subject (`admit_members` in `crates/aether-bloomery/src/reduce/seal.rs`). There is no lane-to-work-order path anywhere between them.

ADR-0214 fixed where a model process gets its instructions. The bundle is `ModelProcessInstructions` (`crates/aether-bloomery/src/values/process_instructions.rs`): seventeen fields of complete static instruction text, `#[serde(deny_unknown_fields)]`, content-addressed through `ConfigKind::address` and sealed bloom-wide in the ADR-0174 `ConfigRegistry`. Its fields cover construct, review, scope, and the framing they share; there is no reader slot. `is_model_lane` recognizes exactly `construct.implement`, `review.critic`, and `scope.fill`, and every executor backend asks it which dispatches get the model wrapper and the credential. A reader lane is therefore not a configuration entry — it is instruction fields, a lane command, and a calibrated seat, all of them code, all of them digest-changing.

ADR-0214 also states its own remaining gap: it "closes one part of #5589. Production provenance admission and derivation records still need implementation." #5589 is open — production model dispatch never invokes the prompt-manifest provenance validator, which has only test callers.

The move this ADR makes has a direct precedent one stage-vocabulary entry away. ADR-0208 took `Scope` — declared, unrun, opus-calibrated, its non-dispatch asserted at the implementation site and never recorded — and made it a dispatched lane, changing only who fills fields before a freeze and moving no approval. This is the same move at the other end of the line, under the same constraint.

## Decision

The line ends in a dispatched reader. It reads what a bloom landed and files what it will not fix, as work orders that acquire no authority from having been written by a machine.

### 1. The subject is a bloom, and `StageId::Study` hosts it

A landed batch is a bloom. `Study`'s existing binding already consumes `bloom.receipt` — `Land`'s product — at the bloom-level tail position keyed by `DispatchKey::Bloom`. Wiring it needs `dispatched_command` to return a command and a `Transformation` constructor beside the `for_aggregate_review` / `for_aggregate_verify` / `for_base_verify` siblings. The persisted stage vocabulary, `StageId::ALL`, the catalog's shape, receipt shapes, and journal decode are all unchanged. Every stored bloom keeps decoding exactly as it does today.

A day is not a subject the vocabulary has. ADR-0186's per-day cut is a ref name resolved as boot configuration, deliberately not a sealed value — "a sealed base already pins each bloom to the exact commit it builds on, so sealing the ref name would only freeze the roll". Nothing keys evidence, dispatch, receipts, or the ledger by day. A day-subject reader needs a new persisted identity, its own dispatch key, its own idempotence, and its own place in the journal and the doctor's invariants: precisely the new persisted stage identity this decision exists to avoid. It would also have to run at the roll, which ADR-0186 already names the busiest moment of the day and ADR-0205 already treats as a quiesce point.

The cost of choosing the bloom is real and is accepted: a per-bloom reader cannot see a pattern that only shows across a day's several blooms. The answer is that a later day-level view is a fold over *filed findings*, which this decision makes durable and addressable, rather than a second reading of the source.

### 2. A reader is a fourth model lane, sealed exactly like the other three

**Instructions.** `ModelProcessInstructions` gains two fields: `retrospect`, the reader's process instructions, and `retrospect_finding_contract`, how one finding is emitted as a work order — the analogue of the existing `scope_emission`. Both are complete static text under ADR-0214's rule. The bloom id, the receipt digest, and the landed range are prompt-manifest context slots, never interpolated into instruction text.

**The upcast.** The struct is content-addressed and `deny_unknown_fields`, and the wire encoding is positional and untagged, so appending fields changes both `ConfigKind::address` for a given value and the schema digest a stored bundle is stamped with. `ModelProcessInstructions` has no `PersistedKind` entry today (`crates/aether-bloomery/src/persisted/mod.rs` registers `DECISIONS`, `EVENT`, `APPROVAL_POLICY`, `MODEL_OVERRIDE`, `PRICE_TABLE`, `SPEND_CEILING`, `STAGE_CATALOG`), and `decode_config` passes an empty upcast array for kinds the registry does not name — so today a shape change would surface as `ConfigResolveError::NoUpcast` against every bundle already sealed. Registering it with the pre-reader shape as its first `PersistedUpcast` is part of the same change. The pinned digest is a literal copied from the stamp recorded beside sealed bytes, never a value recomputed from a type at build time: a pin that recomputes re-pins itself the moment the shape moves, which is the failure it exists to catch.

**The lane.** A new command `retrospect.read`, admitted by `is_model_lane` so the executor hands it the model wrapper and the credential and the dispatch is judged as a model call everywhere that asks. `profile_of`'s existing `StageId::Study => (Harness::Claude, OPUS_MODEL, ReasoningEffort::High)` row becomes live rather than inert; the comment asserting the opposite moves in the same change, because a code comment that contradicts a ratified ADR is worse than no comment.

**Authorization.** Two new fields make a new bundle, and a new bundle is a different sealed configuration that a host operator authorizes afresh (ADR-0214 §"Model-process instructions are explicit configuration"). Knowing its digest, uploading it, or naming it in a request authorizes nothing. A bloom sealed against the pre-reader bundle never adopts the new one mid-flight, and no pin is invented for a bloom already landed (ADR-0214 §Migration) — such a bloom simply does not run `Study`.

**Sequencing behind #5589.** ADR-0214 requires the host, before invoking a model, to resolve the exact pinned bundle, assemble and validate the prompt manifest through `assemble_manifest`, and persist that manifest as attempt evidence. Production dispatch does not call the validator yet; that is #5589, and ADR-0214 names it as the unfinished half. A fourth lane shipped before that gate is enforced is one more unvalidated model call, and this is the lane least able to afford it: its whole input is a landed diff — text authored by construct lanes — and its whole output is a proposal that work be done. Prompt injection from source and context is explicitly *not* closed by ADR-0214. The reader is therefore authored now and **not enabled until #5589's gate is enforced on the production dispatch path**.

### 3. A filed finding is an unapproved commission carrying its own derivation

The reader gets exactly one write. For each finding it will not fix, it files an open commission whose intent carries a `Statement` with `Provenance::StageReceipt`, the receipt naming the `Study` stage, the exact `AgentProfile` digest that ran, the inputs it consumed (the bloom receipt and the landed range), and the outputs it produced (the filed intents), with that receipt statement's digest as the filing's derivation parent.

What that provenance buys is the record and nothing else, and that is the point. `Statement::verify_authority` returns `false` for a `StageReceipt` at every door. `Statement::is_instruction_capable` is false for it. `classify_approval` refuses it with `WrongProvenance`, so it cannot be presented as an approval. `first_approval` renders it unsigned in the commission view. The seal door admits only a member whose `EvidenceKind::Approval` validates its own subject. A filed finding is visible, addressable, greppable, and ordinary — and it becomes work only when the owner signs an Approve statement over a frozen scope revision, exactly as a hand-filed commission does. ADR-0214's closing rule holds verbatim: task text does not acquire authority by parenting the instruction bundle, and it does not acquire authority by being emitted from an authorized process either.

Two guards keep that true in the implementation rather than only in the prose:

- **The reader does not hold the control token.** The bearer token gates the surface that can approve work; a lane holding it could file and approve in one breath. Filings enter the way every other lane result enters — as a coordinator-applied fact the reducer folds — and `POST /commissions` stays the human path.
- **A filing carries no scope revision.** It is intent text plus its derivation. Nothing about it is seal-eligible: `admit_loaded` refuses a seal naming any revision that is not the commission's current tip, and a filing has no tip at all.

The alternative door — recording findings only as `EvidenceKind` rows, appending a variant beside `ReviewAdvisory` — is cheaper and safer-looking, and it is refused. An evidence row lives in the evidence log of a bloom that is by then closed, addressed by a digest whose bytes must be resolved out of the artifact store to be read at all. That is the buried artifact this issue exists to replace. The 2026-09-06 hand pass produced issues, not annotations, because a finding nobody can pick up is not a finding; `ReviewAdvisory` is the existing instance of the same shape, and its findings are exactly the ones nobody reads.

### 4. Spend is the owner's call

Not decided here, and this ADR is ratifiable without it: §1–§3 are shape decisions, and enablement is a bundle the owner authorizes.

The stated consequence is that the seat as calibrated is `claude-opus-5` at high effort reading a whole landed batch, once per bloom, standing — new recurring cost that scales with batch size rather than with member count. It also lands on the forecast axes: `study_report::grade` sums a bloom's actual tokens and worker seconds from the study records belonging to that bloom and grades them against the sealed `Forecast`, so a standing reader shifts every bloom's actuals, and whether the reader's own attempt uploads a study record — grading itself into the bloom it just read — is part of the same call. Either every forecast budgets the read, or every bloom grades over on two axes.

The shapes available: standing per bloom (as specified); operator-triggered per bloom, a demand read with no standing cost, at the price of the practice staying something someone must remember; a cheaper seat, since construct and review already run muse, at the price of the judgment the task actually needs; or one read per day at the roll, which §1 refuses on subject grounds. No recommendation is made.

## Consequences

- `StageId::Study` stops being a declared-and-inert slot. The comments in `stage.rs` and the reactor that assert it is never dispatched become false and must move with the code that makes them false.
- The persisted stage vocabulary and `StageId::ALL` are unchanged, so nothing already journaled decodes differently. Changing `profile_of` or a binding re-digests the catalog, which is the ordinary recalibration cost the catalog already documents, not a migration.
- `ModelProcessInstructions` gains its first upcast and, with it, the schema discipline the journal kinds already carry — a debt that was going to come due at the next instruction change regardless of this decision.
- ADR-0191 §4's "filed as new work for a future bloom" acquires the mechanism it has been missing since it was written.
- The bloom's tail grows a stage, so bloom-completion moves later by one read. The reader's binding carries a retry budget of 1 and produces no candidate, so a read that fails must resolve the bloom with the study missing rather than wedge it: findings are a product, never a gate. A read that cannot judge its subject at all reports `EvidenceKind::ExecutorFault` like any other lane.
- The estate gains a machine-authored intake stream whose volume is bounded by nothing but the model — the hand pass produced about fifty issues from one day. Triage of that stream is the owner's, and an unread pile is the failure mode to watch. Nothing here claims the filings are good; the claim is that they are visible and inert.
- Nothing is claimed about model obedience. A reader is as injectable as what it reads, which is why enablement sequences behind #5589 rather than shipping beside it.

## Alternatives considered

- **A new persisted stage identity for the retrospective.** Rejected: `Study` already binds `bloom.receipt` → `bloom.study` at exactly this position with the process named `retrospect`. A new variant extends the closed vocabulary, changes `StageId::ALL`, and re-digests every catalog, for nothing the existing slot does not already give.
- **A day as the subject.** Rejected: no day identity exists in the value vocabulary; it would need a persisted subject, a dispatch key, and a place in the ledger, and would have to run at the roll ADR-0186 already loads.
- **Findings recorded only as `EvidenceKind` rows.** Rejected in §3: it reproduces the buried artifact the issue exists to replace, and `ReviewAdvisory` is the standing evidence that the shape does not get read.
- **The reader files approved work.** `CommissionApprovalTier::Auto` exists for `ObservationAttestation`, so the door is technically reachable. Rejected: it is a lane minting its own authorization, which ADR-0214 §"Process policy does not authorize arbitrary task text" and ADR-0182's door separation both forbid.
- **Extend `AggregateReview` instead of adding a reader.** Rejected: it runs pre-land against the weave, and ADR-0191 §3 scopes it to intent preservation with member diffs explicitly not re-read. It is the wrong subject at the wrong time, and its non-blocking findings already go where nobody reads them.
- **Take the reader's instructions from the candidate's repository documentation.** Rejected outright by ADR-0214 §"The candidate cannot replace the process" — and doubly so here, since this lane's subject *is* the candidate, so it would be reading its own instructions out of the thing it is judging.
- **Keep the pass manual.** Rejected: it is the status quo. It happened once, produced about fifty issues, and has not happened since.

## Follow-on implementation issues

Sliced but not filed. Surfaces are crate globs.

1. **`feat(bloomery): add the retrospect instruction fields and pin the prior bundle shape`** — append `retrospect` and `retrospect_finding_contract` to `ModelProcessInstructions`, extend `validate`, and register the kind in the persisted registry with the recorded pre-reader digest as its first upcast. Surface: `crates/aether-bloomery/**`.
2. **`feat(bloomery): dispatch StageId::Study as the retrospect.read lane`** — `dispatched_command`, `is_model_lane`, the `Transformation` constructor for the bloom-level Study slot, and the comments that assert Study never dispatches. Surface: `crates/aether-bloomery/**`.
3. **`feat(chassis-bloomery): run the Study lane after Land and admit its result`** — reactor progression from `bloom.receipt` into a Study dispatch, order and timeout classification, and resolve-without-wedge when the read fails or faults. Ships with an end-to-end `ScenarioHarness` scenario. Surface: `crates/aether-chassis-bloomery/**`.
4. **`feat(chassis-bloomery): file reader findings as unapproved derived commissions`** — the coordinator-applied filing path writing open commissions whose intent carries `Provenance::StageReceipt` derivation, with no scope revision, no approval, and no control token. Surface: `crates/aether-chassis-bloomery/**`, `crates/aether-bloomery/**`.
5. **`feat(bloomery-console): surface a bloom's filed findings on its receipt`** — the console view for the new intake stream, so the pile is visible where the bloom is read. Surface: `crates/aether-bloomery-console/**`.
6. **`chore(bloomery): authorize and seal the reader instruction bundle`** — the operator-side import and authorization of the extended bundle, gated on #5589's provenance gate being enforced on the production dispatch path. Surface: `crates/aether-chassis-bloomery/**`.
