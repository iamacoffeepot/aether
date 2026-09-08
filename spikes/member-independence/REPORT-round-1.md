# symbolic-diff spike — round 1

The round-1 record, carried forward unchanged from branch `spike/symbolic-diff`
(`ffc14e9f8`) except for this header and the scratch-path scrub below. Round 2
lives in `README.md` alongside it; the numbers here are what round 2 sets out
to fix.

CLI: `symdiff diff|refs|independent|eval`. Not a workspace member.

Independence: parent-relative diffs (`<sha>^..<sha>`), then the conflict rules. Oracle: `git merge-tree --write-tree <sha1> <sha2>` (two-arg; merge-base is the ancestor). Compile: a scratch git worktree with its own `CARGO_TARGET_DIR`, `cargo check -p` union of packages each side touched.

Candidates are a **linear chain** off `c51ee808b`: `655016948` → `4b2a0f30a` → `c6d99b489` → `0d8630e68` → `6db14c9cc` → `0e7344ed7` → `a29409a29`. Two-arg merge-tree of any pair is a fast-forward to the later tree (`GitClean` in all 21).

## Pair table

CompileFail is the later tree, not a merge of two patches. Two bugs, both already on that tree:

- E0308 `crates/aether-bloomery-console/src/screen/detail.rs:387` — `RowKey::Other(9 + index)` (`usize` vs `u16`). Present on every candidate tree. Surfaces when the check set includes `aether-bloomery-console` (later tree `4b2a0f30a` / `c6d99b489`).
- E0277 `crates/aether-chassis-bloomery/src/bloomery/approve/policy.rs:25` — `PolicyError` has no `Display`. Present on every candidate; later-fixed by `86fc2f519` (`impl fmt::Display for PolicyError` at that file:33). Surfaces when the check set includes `aether-chassis-bloomery` (later trees `0d8630e68` and after).

| pair | subjects | symdiff | git | compile | ms |
| --- | --- | --- | --- | --- | --- |
| `655016948` × `4b2a0f30a` | surface request × amend | Conflict(Breaks) | GitClean | CompileFail E0308 `detail.rs:387` 32045ms | 36315 |
| `655016948` × `c6d99b489` | surface request × seal | Conflict(Breaks) | GitClean | CompileFail E0308 `detail.rs:387` 6852ms | 82662 |
| `655016948` × `0d8630e68` | surface request × withdraw | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` 13725ms | 108813 |
| `655016948` × `a29409a29` | surface request × re-export split | Suspect(TestPin) | GitClean | CompileFail E0277 `policy.rs:25` 9462ms | 38613 |
| `655016948` × `6db14c9cc` | surface request × xtask triage | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` 9314ms | 49161 |
| `655016948` × `0e7344ed7` | surface request × symbols refs | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` 9395ms | 35207 |
| `4b2a0f30a` × `c6d99b489` | amend × seal | Conflict(Breaks) | GitClean | CompileFail E0308 `detail.rs:387` (cached) | 71732 |
| `4b2a0f30a` × `0d8630e68` | amend × withdraw | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 84575 |
| `4b2a0f30a` × `a29409a29` | amend × re-export split | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 11337 |
| `4b2a0f30a` × `6db14c9cc` | amend × xtask triage | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 25921 |
| `4b2a0f30a` × `0e7344ed7` | amend × symbols refs | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 13441 |
| `c6d99b489` × `0d8630e68` | seal × withdraw | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 76453 |
| `c6d99b489` × `a29409a29` | seal × re-export split | Independent | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 893 |
| `c6d99b489` × `6db14c9cc` | seal × xtask triage | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 15441 |
| `c6d99b489` × `0e7344ed7` | seal × symbols refs | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 1275 |
| `0d8630e68` × `a29409a29` | withdraw × re-export split | Independent | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 1658 |
| `0d8630e68` × `6db14c9cc` | withdraw × xtask triage | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 16195 |
| `0d8630e68` × `0e7344ed7` | withdraw × symbols refs | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 2011 |
| `a29409a29` × `6db14c9cc` | re-export split × xtask triage | Independent | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 1659 |
| `a29409a29` × `0e7344ed7` | re-export split × symbols refs | Independent | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 41 |
| `6db14c9cc` × `0e7344ed7` | xtask triage × symbols refs | Conflict(Breaks) | GitClean | CompileFail E0277 `policy.rs:25` (cached) | 2042 |

`a29409a29` extracts **0** item changes: brace-list `pub use` vs one name per statement hashes equal per imported path.

## Precision / recall of `Independent` vs compile oracle

Positive class: predicted `Independent`. Oracle positive: `GitClean` **and** `CompileOk`.

| | oracle + | oracle − |
| --- | --- | --- |
| pred Independent | tp=0 | fp=4 |
| pred not Independent | fn=0 | tn=17 |

precision = 0/4 = **0.000**. recall = 0/0 = **0.000** (no CompileOk tree).

The compile oracle never goes green because every later tree already fails `cargo check` for the two errors above. Those errors are **not produced by combining the two patches**; they are in the fast-forwarded later commit. `86fc2f519` adds `Display` for `PolicyError`; the `detail.rs:387` `usize`/`u16` mismatch is still on `a29409a29`.

### False positives (pred Independent, oracle not CompileOk+GitClean)

All four are `a29409a29` (0 symbolic changes) against a patch that does not share a renamed/removed/sig-changed ident the other side still reads:

- `c6d99b489` × `a29409a29` — 0 sentences. Seal vs re-export formatting.
- `0d8630e68` × `a29409a29` — 0 sentences. Withdraw vs re-export formatting.
- `a29409a29` × `6db14c9cc` — 0 sentences. Re-export formatting vs xtask triage (`xtask/src/transform/verify/{mod,nextest,triage}.rs`).
- `a29409a29` × `0e7344ed7` — 0 sentences. Re-export formatting vs symbols search.

Cause: **compile oracle pollution**, not a symbolic miss. Git agrees (`GitClean`). The re-export commit is the one case the extractor gets right: `a29409a29` rewrote `crates/aether-bloomery/src/lib.rs` packed `pub use module::{A,B}` lists into one statement per name; per-name import paths did not change.

If the oracle is **GitClean only**, these four are true Independents (precision 4/4 vs git; the other 17 are git-FNs).

### False negatives (pred not Independent, oracle CompileOk+GitClean)

None against the compile oracle (it never compiled).

Against git (all 21 GitClean), every `Conflict*` / `Suspect*` is a false negative. Causes, with file:line:

**1. Bare-ident grep (dominant).** `refs` is `git grep -n -w -F <last path segment>`. `xtask::bloom::Endpoint::resolve` (`xtask/src/bloom/mod.rs:47` at `4b2a0f30a`) is `SignatureChanged`; word `resolve` then hits `crates/aether-actor-derive/src/handler_parse.rs:439` (`done.resolve*` in a diagnostic string) and `crates/aether-audio/src/runtime/tests/instrument.rs:73`. Same for `xtask::bloom::ProjectionArgs::input` vs `crates/aether-actor-derive/src/lib.rs:279` and `crates/aether-audio/src/runtime/reverb.rs:53`. 981–7187 sentences on the large pairs are this class.

**2. Re-export counted as signature change.** `6db14c9cc` × `0e7344ed7`: `0e7344ed7` replaced the local `pub fn path_in_surface` at `crates/aether-chassis-bloomery/src/bloomery/verify/containment.rs:37` (`6db14c9cc`) with `pub use aether_bloomery::path_in_surface` at `containment.rs:23` (`0e7344ed7`). Call site `batch.rs:357` (`super::path_in_surface(&member.declared_surface, path)`) still compiles. Tool: `Conflict(Breaks)`.

**3. Enum variant add → BodyChanged + TestPin.** Same pair: `xtask/src/symbols/table.rs` `SymbolKind` gains `TraitMethod` (`6db14c9cc` lines 12–17, four variants; `0e7344ed7` lines 12–26, five). Tests at `xtask/src/symbols/diff.rs:51` (`SymbolKind::Fn`) and `extract.rs:439` still compile. Tool: TestPin on `xtask::symbols::table::SymbolKind`.

**4. Ancestry.** Parent tree still has the old item, so `reader_already_has` does not skip; descendant merge-tree is a fast-forward that already applied the change. Example: `path_in_surface` above (`6db14c9cc` is parent of `0e7344ed7`).

**5. TestPin on a type the other crate's tests mention.** `655016948` × `a29409a29`: `Suspect(TestPin)` — `aether_bloomery::port::projection::MemberView` body-changed in the surface-request patch; `a29409a29` tests still name it at `crates/aether-bloomery-console/src/screen/board.rs:391`, `partition.rs:162`, `quiet.rs:69`. Git-clean fast-forward; tests pin the *later* body.

**6. Trait impl / method identity.** Impl paths stringify via `quote` (`impl Trait for Type`). Inherent `Type::method` vs `impl Trait for Type::method` are distinct; a moved method looks like Removed+Added rather than BodyChanged.

**7. Macros / derives.** `#[derive(…)]` impls are invisible. Not observed as a pair-level miss here; it would under-count, not over-count.

**8. Overlapping files git would conflict if they were siblings, that fast-forward hides.** `4b2a0f30a` ∩ `c6d99b489`: `crates/aether-bloomery/src/values/approval.rs`, `xtask/src/bloom/amend/mod.rs`, `xtask/src/bloom/mod.rs`, `xtask/src/bloom/plan.rs`. Two-arg merge-tree is still GitClean because `c6d99b489` is the child.

## Cost per candidate (`diff` vs parent)

| sha | subject | changes | packages | ms |
| --- | --- | --- | --- | --- |
| `655016948` | typed surface request | 222 | aether-bloomery, aether-bloomery-console, aether-bloomery-github, aether-chassis-bloomery, aether-harness-bloomery, xtask | 1800 |
| `4b2a0f30a` | amendment verb | 198 | aether-bloomery, aether-chassis-bloomery, xtask | 487 |
| `c6d99b489` | seal refuses unnamed file entry | 58 | aether-bloomery, aether-chassis-bloomery, xtask | 294 |
| `0d8630e68` | withdraw member | 202 | aether-bloomery, aether-bloomery-console, aether-chassis-bloomery, aether-harness-bloomery, xtask | 1558 |
| `a29409a29` | one crate-root re-export per statement | 0 | — | 27 |
| `6db14c9cc` | triage each failing test | 84 | xtask | 209 |
| `0e7344ed7` | rev-pinned reference search | 63 | aether-bloomery, aether-chassis-bloomery, xtask | 192 |

## Cost per pair (`independent --parents`)

Wall time is grep-bound (one `git grep` per distinct ident on Removed/Renamed/SignatureChanged/BodyChanged).

| pair | ms | sentences |
| --- | --- | --- |
| `655016948` × `4b2a0f30a` | 36315 | 981 |
| `655016948` × `c6d99b489` | 82662 | 1544 |
| `655016948` × `0d8630e68` | 108813 | 7187 |
| `655016948` × `a29409a29` | 38613 | 2378 |
| `655016948` × `6db14c9cc` | 49161 | 2699 |
| `655016948` × `0e7344ed7` | 35207 | 2389 |
| `4b2a0f30a` × `c6d99b489` | 71732 | 1869 |
| `4b2a0f30a` × `0d8630e68` | 84575 | 5136 |
| `4b2a0f30a` × `a29409a29` | 11337 | 331 |
| `4b2a0f30a` × `6db14c9cc` | 25921 | 652 |
| `4b2a0f30a` × `0e7344ed7` | 13441 | 342 |
| `c6d99b489` × `0d8630e68` | 76453 | 4808 |
| `c6d99b489` × `a29409a29` | 893 | 0 |
| `c6d99b489` × `6db14c9cc` | 15441 | 321 |
| `c6d99b489` × `0e7344ed7` | 1275 | 11 |
| `0d8630e68` × `a29409a29` | 1658 | 0 |
| `0d8630e68` × `6db14c9cc` | 16195 | 321 |
| `0d8630e68` × `0e7344ed7` | 2011 | 11 |
| `a29409a29` × `6db14c9cc` | 1659 | 0 |
| `a29409a29` × `0e7344ed7` | 41 | 0 |
| `6db14c9cc` × `0e7344ed7` | 2042 | 11 |

## Rung-3 test set vs crate `#[test]` count

`refs` TEST hits for changed idents (len≥4) vs `#[test]` functions parsed in that crate at the revision. Hits are bare-ident, so they over-count.

### `655016948` typed surface request — crate `aether-bloomery`

- changed idents grepped: 168
- idents with ≥1 TEST ref: 73
- TEST reference hits: **20050**
- `#[test]` fns in `aether-bloomery` at that rev: **415**
- real pins: `SurfaceRequest` defining `crates/aether-bloomery/src/values/surface.rs:38`; TEST refs at `crates/aether-bloomery/src/reduce/surface_request.rs:107`, `:129` (`request`), `crates/aether-bloomery/src/values/surface.rs:170` (`a_glob_bearing_path_is_dropped_rather_than_widening_the_appeal`)
- junk pins: `evidence` → `crates/aether-bloomery-console/src/dto.rs:833`; `StageVerdict` → `crates/aether-bloomery/src/inward.rs:170`

Ratio TEST-hits / crate tests ≈ 48×. The 73 idents are the rung-3 set; 415 is the full crate test count.

### `0d8630e68` withdraw — crate `aether-bloomery`

- changed idents grepped: 134
- idents with ≥1 TEST ref: 66
- TEST reference hits: **13339**
- `#[test]` fns in `aether-bloomery` at that rev: **439**
- real pins: `WithdrawError` / `Withdrawal` / `WithdrawalCause` at `crates/aether-bloomery/src/reduce/withdraw.rs:202–204`
- junk pins: `as_str` → `crates/aether-actor/src/log.rs:553` `tail_contains_matches_message_subset_case_sensitively`; `bloom` → `crates/aether-bloomery-console/src/cursor.rs:79`

Ratio ≈ 30×.

## Three changes next

1. Resolve references (file `use` graph + same-crate last-ident), not `git grep -w -F`. That is the 981–7187-sentence failure (`resolve`, `input`, `MemberView`).
2. Oracle: cherry-pick both parent-relative patches onto a shared tree (or `git merge-tree --merge-base=<parent-of-earlier>` is still wrong for a chain). Two-arg merge-tree on ancestor pairs cannot disagree with “later commit compiles”.
3. Treat `pub use x;` as a re-export edge, not a signature of `x`. `path_in_surface` at `containment.rs:23` (`0e7344ed7`) vs `containment.rs:37` (`6db14c9cc`) would then be Rename/Reexport, and `a29409a29` stays 0 changes for the right reason.
