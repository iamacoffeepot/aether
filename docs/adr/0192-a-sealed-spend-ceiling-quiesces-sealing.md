# ADR-0192: A sealed spend ceiling quiesces sealing

- **Status:** Proposed
- **Date:** 2026-08-14

## Context

Nothing in the pipeline reacts to how much it has spent. Every cost mechanism that exists measures: `StudyCost` carries the token columns per attempt, `PriceTable` prices them from rates an operator sealed, `study_report::grade` folds the resolved records into per-bloom actuals, and ADR-0184 projects those actuals into a capability ledger. All of it reports after the fact. The one mechanism that ever claimed to bound spend — ADR-0149's sealed `Budget`, with its `token_ceiling` and `wall_clock_secs` — was removed by ADR-0177 precisely because nothing read those fields, nothing metered the quantity they named, and a bloom-wide ceiling projected onto concurrently executing workers meant a different thing to every reader.

Two things changed since. The fleet now runs more than one billing relationship: the grok harness arm is filed, per-stage model routing is sealed configuration, and the rates for both vendors sit in one `PriceTable` keyed by model id. A day's spend is therefore a single number accruing across two accounts with no mechanism anywhere that reads it. And ADR-0185's train removed the landing barrier between blooms, so the seal door — which used to fire once per landing cycle under the one-active-bloom rule (`active_unlanded_bloom`) — now fires as fast as successors can be projected. Unbounded seal rate over two vendors is the operating state.

The pieces a governor needs are already in place and already honest about their own limits.

- **The priced figure is computed, not reported.** The study broker prices each dispatch at intake (`price_of` in the chassis crate) against the `PriceTable` its bloom sealed, through `PriceTable::price_dispatch`, and writes the result into `StudyCost::cost_micro_usd` on the durable `StudyRecord` artifact. No harness's self-reported dollar figure enters. A mechanical lane with no model, a table that prices no such model, and a pre-migration or unresolvable table each price to zero and log the gap rather than failing the record.
- **The per-call usage that selects a long-context band lives only at intake.** `UploadedStudyRecord::calls` is not persisted; the band it selected is already folded into the priced column. Anything that re-prices from the artifact alone would silently drop back to the sub-band rate.
- **Configuration the reducer must read arrives as an argument.** ADR-0174 settled that: `reduce` takes `ResolvedConfigs` because reaching a store is a property of where the code runs, not of what the reducer can decode. `validate_line` and `sealed_config` already resolve sealed values at the seal door and refuse rather than default when content will not produce.
- **A recorded decision is what replay folds.** ADR-0190 made boot replay `apply` the journaled `Decisions` rather than re-decide the event, so a door whose answer depends on a measurement taken at admission time stays stable across reducer versions and across a coordinator restart.
- **The day is already a boundary with a drain.** ADR-0186 cuts `bloomery/daily/<date>` from main, points the coordinator's mainline at it through the boot-resolved `AETHER_BLOOMERY_MAINLINE_REF`, and makes the roll a quiesce point: stop sealing, drain the train, sync back, cut, repoint, resume. An undrained bloom drains on its branch and the roll waits for it.

What is missing is a reader and an actuator.

## Decision

A spend governor sits at the bloom's seal door. It compares the window's measured spend against a ceiling the sealing draft's configuration registry names, and when the ceiling is crossed it quiesces sealing: blooms already in flight run to completion, and no new bloom seals until the window rolls or a newly sealed ceiling raises the bar.

### The ceiling is a registry entry

`SpendCeiling` is a configuration kind under `aether.bloomery.spend_ceiling`, sealed and resolved through the ADR-0174 registry exactly as `PriceTable` and `StageCatalog` are, with two axes:

```rust
pub struct SpendCeiling {
    pub window_micro_usd: Option<u64>,
    pub bloom_micro_usd: Option<u64>,
}
```

Micro-USD for the reason `StudyCost::cost_micro_usd` and every `PriceRates` column are: a float is not `Eq` and so not a stable content address, and this value is sealed. An absent axis is uncapped, and an absent entry is uncapped on both — the same posture `PriceTable::default` takes when it prices nothing, and for the same reason. A compiled-in dollar figure would be the workspace stating what the owner's fleet may spend, which is not a number this repository knows.

Resolution is **bloom-wide only**, the departure ADR-0174 already took for `aether.bloomery.approval_policy` and for the same reason: a per-member entry would let one member choose the ceiling that admits its own bloom. A member-scoped entry refuses the seal rather than resolving or being quietly ignored. The door reads it through the existing `sealed_config::<SpendCeiling>` path, so an address whose content is missing, misfiled, or undecodable is the existing `SealError::UnproducibleConfig` and needs no new refusal.

That a draft seals the ceiling governing its own admission is deliberate and is the raise path the design wants: an operator who decides the fleet may spend more authors a new ceiling through `POST /configs` and seals it, and the receipt then attests exactly which ceiling applied to which bloom. The governor bounds a pipeline that would otherwise run all night; it does not defend the ledger against the operator, and it should not pretend to.

### Spend is summed, never re-priced

The window's spend is the sum of `StudyCost::cost_micro_usd` over the resolved `StudyRecord` artifacts named by the `EvidenceKind::StudyRecord` entries in each bloom's evidence log — the same fold, over the same evidence, through the same injected `Fn(&Digest) -> Option<StudyRecord>` resolver `study_report::grade` already uses. It lives beside `grade` in `aether-bloomery` as a pure read, and it re-derives nothing: the priced column on the artifact is the figure the sealed table produced at intake, band selection included, so summing it keeps one accounting path where re-pricing from the artifact would open a second one that disagrees with the first about every long-context dispatch.

The reducer cannot perform that read — it holds no store handle and the evidence log holds digests, not columns — so the measurement arrives as an argument beside `ResolvedConfigs`:

```rust
pub fn reduce(snapshot: &Snapshot, event: &Event, configs: &ResolvedConfigs, spend: &SpendWindow) -> Decisions;
```

`SpendWindow` carries the window's label, its total in micro-USD, the per-bloom totals inside it, and the counts of dispatches with no admitted study record and of records whose model the sealed table priced at nothing. The two counts ride along rather than being folded into the total, because a ceiling that never trips against a fleet nobody has authored rates for should be legible as an unpriced fleet rather than as a cheap one — the `price` versus `None` distinction the table draws, carried one level up. The control core fills the argument the way it fills `configs` today, and a caller with nothing to measure passes the default, which is an empty window that never quiesces.

### The window is the operating day, and the reducer holds no clock

The spend window is ADR-0186's day branch. Its identity is host-side because ADR-0186 already made the mainline ref boot-resolved configuration rather than a sealed value, and because `reduce` is pure and has no clock — a wall-clock date comparison inside it would be a second source of truth about what day it is, disagreeing with the branch on exactly the runs that matter. The host names the window and measures it; the reducer compares two numbers.

The boundary needs no special handling. The roll is already a quiesce point that waits for the train to drain, so a bloom cannot straddle two windows, and a study record arriving after the roll for a lap that ran before it belongs to the bloom it grades and therefore to that bloom's window. When the day rolls, the new window measures zero and the door reopens with no further act — a quiesced pipeline resumes by the calendar, which is the behavior the ceiling is for.

### Where the check sits, and what it answers

The governor is the last gate in `reduce_seal`, after the membership, configuration, line, conflict, landed-set, and active-bloom checks and immediately before the claim effects. Order carries meaning: every check above it names something wrong with the draft, and this one names something true about the fleet. A draft refused here is a correct draft that will seal unchanged once the window rolls, and running the governor first would hand an operator a spending message about a draft whose real problem was an unapproved member.

`reduce_supersede` is **not** gated. A supersession is how a wedged bloom escapes, how the train resyncs on a missed projection, and how mainline catches up — and ADR-0186's roll waits for the drain, so a governor that blocked supersession could stop the very drain that rolls the window and resets the thing it was measuring. The leak is real and worth naming: a successor may admit net-new members and dispatch them past the ceiling. It is bounded by supersession being an act against one identified in-flight bloom rather than a loop that runs by itself.

Crossing produces an outcome and a recorded effect, not a bare refusal. Two wire-frozen enums under `crates/aether-bloomery/src/reduce/` gain one tail-appended variant each:

- `Outcome::SealQuiesced(SpendQuiesce)`, so the REST edge and the admitting caller are told which axis closed the door and by how much.
- `Decision::RecordSpendQuiesce { quiesce: Option<SpendQuiesce> }`, folding a `Snapshot`-level marker that `view_of` renders onto `ViewDocument`. `Some` records the crossing; `None` clears it on the first seal that passes the governor again. One variant with an `Option` rather than a raised-and-cleared pair, because the clearing edge carries no operator, no reason, and nothing else worth journaling — the `Decision::RecordReviewPark` shape rather than the `Decision::RecordOperatorHold` / `Decision::RecordOperatorRelease` shape.

`SpendQuiesce` names its axis in its own shape rather than through a flag beside an optional bloom:

```rust
pub enum SpendQuiesce {
    Window { window: String, spent_micro_usd: u64, ceiling_micro_usd: u64 },
    Bloom { window: String, bloom: BloomId, spent_micro_usd: u64, ceiling_micro_usd: u64 },
}
```

The window label rides in the journaled value so a reader can tell which day's ceiling closed the door without joining back to host configuration, the legibility argument ADR-0174 accepted a string key for. The per-bloom axis is evaluated over every bloom in the window and names the first that reached its ceiling: the over-budget bloom keeps running, and what stops is the seal that would put a second bloom beside it.

A quiesced seal is a non-admission that carries an effect, which is a shape ADR-0190 does not otherwise produce — a refused row folds nothing. That is the intent of recording it: the pipeline did not refuse a request, it closed a door, and a door that closes without a journal row is a coordinator that looks idle for no reason anyone can reconstruct.

### Wire discipline

`Decision` and `Outcome` gain variants at the **tail**, leaving every prior variant's wire discriminant unchanged, the rule every appended variant in both enums already states in its own doc comment. `SealError` gains nothing.

`SpendQuiesce` becomes reachable from `Decision`, so it joins the journal's frozen decisions graph. The implementing change therefore repins `GOLDEN_DECISIONS` in `crates/aether-bloomery/tests/golden_decisions/main.rs` **in the same diff**, extends `representative()` with the new effect family, and carries the repin note the previous four repins carry — which byte inside the old length moved (the effect count) and how many bytes appended at the tail. The `completeness` sibling derives the required position set from `Decision::SCHEMA` and will name `SpendQuiesce` as missing until the representative reaches both of its payload-carrying variants, so the coverage half is enforced rather than eyeballed. `DECISIONS_SCHEMA` stays `aether.bloomery.decisions.v1`: appending a variant leaves every previously written row decodable, which is the condition ADR-0187 sets for keeping an identity.

## Consequences

- The fleet acquires its first mechanism that reads its own spend and acts on it. The read is the ledger's own figure, so a number that appears in a quiesce is the same number `/view` and the capability ledger show.
- The governor is a floor on real spend, so it can be late but never early. A dispatch with no admitted study record counts zero, a model the sealed table does not price counts zero, and a lap still running counts zero until its record lands. It cannot quiesce a fleet that has not actually spent, and it will let a fleet overshoot by whatever the ledger has not yet seen. The overshoot is bounded by the in-flight tail plus the accounting gap, and the two counts on `SpendWindow` make the gap a rendered number rather than a suspicion.
- Closing that gap is the ledger's work, not the governor's. A dispatch that never produces a study record is a defect in the study lane, and repairing it there improves the governor for free; teaching the governor to estimate a missing record would be the second accounting path this decision exists to avoid.
- The seal door gains a refusal an operator can act on without editing anything: wait for the roll, or seal a higher ceiling. Both are visible in the journal, and the second is attested in the receipt of every bloom that runs under it.
- A supersession can spend past a quiesced ceiling. That is the deliberate cost of keeping the drain unblocked, and it stays visible because the successor's own spend accrues into the same window.
- The reduce signature changes, so every caller threads the new argument. Callers that do not gate spend pass the default window.
- The day-roll ceremony gains a reason to be coordinator-owned sooner: while the repoint is an operator command with a restart, the window's identity is a thing a human sets, and a forgotten repoint leaves yesterday's spend governing today.
- This does not reinstate anything ADR-0177 removed. `Budget`, `token_ceiling`, `wall_clock_secs`, and `retry_cap` stay deleted; `ExecutionLimits` remains the per-dispatch bound and the sealed `StageCatalog` remains the whole retry authority. The difference is not one of degree: 0177 removed per-bloom promises sealed into the spec's identity that no code read and no meter could have served. This is a registry entry that re-digests no spec, with one named reader at one named door, over settled facts that are already priced and already journaled, actuating a door rather than a running worker.

## Alternatives considered

- **An environment-variable ceiling** (`AETHER_BLOOMERY_SPEND_CEILING` through the ADR-0090 config path). Rejected: what the fleet may spend is an owner statement that belongs in the receipt, and an env var is invisible to the journal — a reader looking at a quiesced day could not reconstruct what the ceiling was, and changing the variable would silently restate what past runs had been allowed to spend. The env path is right for host tuning and wrong for an attested policy, which is the line `clippy.toml` already draws around naked env reads.
- **Kill in-flight lanes at the ceiling instead of quiescing seals.** Rejected on three counts. It discards paid work at exactly the moment cost matters, which spends more than it saves. A killed lane returns no evidence, so the accounting under-counts precisely the spend that triggered the kill and the ledger learns nothing from the incident. And ADR-0186's roll waits for the drain, so killing to save money leaves the day unable to roll cleanly — the actuator would fight the window it is measured in. Quiescing seals costs at most the in-flight tail and leaves every artifact intact.
- **Per-vendor ceilings rather than one fleet-wide number.** Rejected for now, though it is the composable direction. The trigger is one total across two billing relationships, and a per-vendor split cannot answer it. It is also not expressible against the values that exist: `PriceTable::rows` is keyed by model id, with no vendor axis, so per-vendor accounting would need an attested model-to-vendor mapping before it could mean anything — a second sealed value to keep correct, added before anything needs it. `SpendCeiling` can gain per-vendor axes once that mapping is itself attested and sealed.
- **Fold spend into the snapshot by carrying `StudyCost` on the admitting fact.** Rejected: it is a `Fact` wire break to duplicate columns the study artifact already holds, and ADR-0184 settled the shape of that argument for the resolved profile — a second copy of a measured fact can only agree with the first or be a bug. Keeping the columns on the artifact also keeps the priced figure and its band selection in one place.
- **Re-price from the journal at governor time rather than summing the priced column.** Rejected: `UploadedStudyRecord::calls` is not persisted, so a re-price cannot select a long-context band and would bill every banded dispatch at the sub-band rate — a second accounting path that disagrees with the first in the direction of under-counting, on exactly the vendor the ceiling was filed for.
- **Refuse the seal as an ordinary `SealError`.** Rejected: every other `SealError` names something wrong with the draft, and a spend refusal names something true about the fleet. Folding them together would send an operator to re-author a draft that was never wrong, and would leave `/view` unable to distinguish a pipeline that is stopped from one that is at its ceiling.
- **Gate supersession as well as sealing.** Rejected: a supersession is the escape from a wedge and the train's resync, and the day roll waits for the drain, so blocking it can prevent the window from ever rolling. The narrower rule — gate the door that admits new work, leave the door that finishes started work open — accepts a bounded leak to avoid a deadlock against the mechanism that resets the ceiling.
