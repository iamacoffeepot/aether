# Seeding agent B — trace/render/EWMA bugs + clean control (s4, s6, s7, c1)

Dispatched as a `general-purpose` background agent. Token usage: 115,915 subagent tokens, 29 tool uses, 452.5 s wall.

## Prompt (verbatim, paths sanitized)

You are constructing a benchmark dataset of seeded bugs for calibrating a code-review agent. Work in the git worktree at `<WORKTREE>` (run all commands from there; do NOT cd elsewhere, do NOT touch any file except the four assigned below, do NOT commit, do NOT run cargo fmt).

Scratch dir: SCRATCH=`<SCRATCH>/dataset`

For EACH item:
1. FIRST copy the pristine file: mkdir -p $SCRATCH/\<item\> && cp \<file\> $SCRATCH/\<item\>/base.rs
2. Edit the live tree file per the spec, PLUS 10–30 lines of BENIGN cover changes in the same file (safe, behavior-preserving: doc-comment improvements, renaming a local, extracting a well-named local, reordering independent statements). The diff should read like a normal small refactor PR where exactly ONE change is the bug (zero for the clean item). CRITICAL: benign changes must be provably behavior-preserving — when in doubt use doc comments and local renames only. No comment may hint at any bug. Match existing style; lines ≤ 120; no section-divider comments.
3. Verify: cargo check -p aether-capabilities must PASS with no NEW warnings.
4. Write the diff: git diff -- \<file\> > $SCRATCH/\<item\>/diff.patch (must be non-empty).

ITEMS:

item s4_silent_noop (level L2, shape silent-incompleteness):
file crates/aether-capabilities/src/trace/walk.rs
Inject a branch/match arm that silently no-ops where logic is required: e.g. a node class or edge case in the trace-tree walk that gets skipped (continue / empty arm / early return) so those nodes silently vanish from output, where the code previously handled them. Moderate subtlety.

item s6_leak (level L3, shape resource-leak):
file crates/aether-capabilities/src/render/runtime/texture.rs
Inject a resource leak on an early-return or error path: an entry registered/inserted (texture slot, id allocation, map insert) that is not rolled back / released when a subsequent step in the same function fails or early-returns. Subtle: the happy path stays correct.

item s7_ewma (level L4, shape invariant-violation, very subtle):
file crates/aether-capabilities/src/lifecycle/runtime/settlement.rs
Find the per-handler execution-cost EWMA (mean_nanos / mad_nanos update). Inject a very subtle statistical bug that compiles and looks right: e.g. the deviation (MAD) updated against the NEW mean instead of the pre-update mean (or vice versa contrary to the current correct code), or the EWMA alpha applied to the wrong term so the estimate lags/overshoots. Must survive a casual read.

item c1_clean (CLEAN control — zero bugs):
file crates/aether-capabilities/src/http/client/runtime.rs
Make ONLY benign, provably behavior-preserving changes (15–35 changed lines): doc comments, local renames, extracting a named local, tidying. The diff must look like the same kind of PR as the others but contain NO defect at all. Be extremely careful nothing changes behavior.

RETURN exactly this JSON (no prose around it):
{"items":[{"item":"s4_silent_noop","file":"\<abs path\>","fn":"\<enclosing fn\>","line":\<line in EDITED file\>,"shape":"silent-incompleteness","level":"L2","bug":"\<one precise sentence\>","cover":"\<one sentence\>"},{"item":"s6_leak",…,"shape":"resource-leak","level":"L3",…},{"item":"s7_ewma",…,"shape":"invariant-violation","level":"L4",…},{"item":"c1_clean","file":"\<abs path\>","fn":"","line":0,"shape":"none","level":"CLEAN","bug":"none","cover":"\<one sentence\>"}]}
If a specified site doesn't exist, pick the closest same-shape site in the SAME file and say so in "bug".

## Returned answer key (verbatim)

```json
{"items":[
 {"item":"s4_silent_noop","file":"<WORKTREE>/crates/aether-capabilities/src/trace/walk.rs","fn":"TreeWalk::absorb","line":90,"shape":"silent-incompleteness","level":"L2","bug":"The let-chain in absorb was restructured into a nested if whose already-visited-recipient guard now says `continue` before `collected.push(entry)`, so a Sent whose recipient ring was already visited (any reply/ping-pong hop back to an earlier actor) is silently dropped and that node vanishes from the stitched tree; all in-file tests still pass since none sends to a previously-visited mailbox.","cover":"Renamed local `mbx` to `mailbox` in next_mailbox, extracted the `unfinished` count local in stitch_with, and polished three doc comments."},
 {"item":"s6_leak","file":"<WORKTREE>/crates/aether-capabilities/src/render/runtime/texture.rs","fn":"StagedTexture::apply_subrect","line":41,"shape":"resource-leak","level":"L3","bug":"No map-insert/id-allocation error path exists in this file (the registry insert lives in the parent handler), so the closest same-shape site was used: `self.dirty = true` was hoisted above the bounds/length validation and is not rolled back on the two early `return false` paths, so a rejected update leaves a stale dirty flag that forces a spurious full GPU re-upload at next record (happy path unchanged; note two in-file unit tests assert !dirty after failure, so cargo test — though not the required cargo check — would flag it).","cover":"Renamed local `in_bounds` to `rect_in_bounds` and rewrapped/polished the apply_subrect, TextureRegistry, and expected_pixel_bytes doc comments."},
 {"item":"s7_ewma","file":"<WORKTREE>/crates/aether-capabilities/src/lifecycle/runtime/settlement.rs","fn":"LifecycleCapabilityState::record_settlement_latency","line":101,"shape":"invariant-violation","level":"L4","bug":"The file has no mean_nanos/mad_nanos table (that is the actor-cost surface elsewhere), so the same-shape settlement-latency EWMA was used: the decreasing-sample branch now computes `sample + α·(prev − sample)` instead of `prev − α·(prev − sample)`, anchoring at the new sample so a single fast settle collapses the average by weight 1−α instead of α (contradicting the adjacent `next = prev ± α·|sample − prev|` comment and biasing the EWMA and its warn context low); the in-file test only feeds monotonically increasing samples, so it still passes.","cover":"Renamed closure locals prev/sample to prev_nanos/sample_nanos and `ewma` to `smoothed`, renamed the cooldown closure binding to `armed`, and touched three doc comments."},
 {"item":"c1_clean","file":"<WORKTREE>/crates/aether-capabilities/src/http/client/runtime.rs","fn":"","line":0,"shape":"none","level":"CLEAN","bug":"none","cover":"Extracted the compile-time DEFAULT_USER_AGENT const from the inline concat!, renamed locals r/h/parsed to resp/header/parsed_url, and improved four doc comments — all provably behavior-preserving."}
]}
```

## Post-hoc harness adjustment (s6)

The seeder noted two in-file unit tests pinned `!dirty` after a failed `apply_subrect` — a PR carrying this bug would be CI-red and never reach review, and the tests are an in-file giveaway. The harness therefore weakened those two asserts in the tree (removing the `texture.dirty = false; … assert!(!texture.dirty)` fragments from `staged_texture_apply_subrect_bounds` and `staged_texture_apply_subrect_uses_r8_stride`) and regenerated `s6_leak/diff.patch` to contain only the four production-code hunks. The committed dataset tree state (see README §History) carries the weakened tests; the presented diff does not show them.
