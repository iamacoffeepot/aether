# Verify-pipeline wall clock: what was wrong, what changed, how to prove it on eve

Branch `perf/verify-pipeline`, based on `night24-perf-base` (`8b398f9ed`). Four
commits, one per item. Everything below that is called *measured* was measured
on this laptop; everything called *projected* is arithmetic over the fleet-host
numbers the operator supplied, and the eve session is what settles it.

The acceptance bar for that session: **a warm `verify.check` on the 32-core
fleet host completes in <= 3 minutes wall, with the test gate <= 2 minutes.**
Free GitHub runners do the same work in ~15 minutes wall (Format 0.4, Clippy
2.6, Rustdoc 2.7, dup/deps/lock ~1, Tests 14.8 across 3 shards). Eve's warm
per-gate numbers already beat the runners badly — clippy 43s against 2.6 min,
docs 26s against 2.7 min — which is why the cold-slot item below carries nearly
all the value: the lanes were not warm.

## Item 1 — why a seeded slot was cold

**Root cause, confirmed by measurement: the lane's *checkout path* moved, and
both layers of the cache are bound to it — cargo by mtime, sccache by path.**

`#5425` moved a dispatch's checkout from the lane slot's own directory
(`<base>/slot-<index>`) to its member's session directory
(`<base>/sessions/<slug>/tree`), because a harness binds a conversation
permanently to the directory it was born in. The lane's cargo target directory
stayed per slot (`<base>/slot-<index>-target`). Every member therefore built at
a path no earlier lane had ever built at, against a target directory that had
never seen that path.

Both halves of that are fatal, and they are separable.
`scripts/lane-warmth-probe.sh` separates them on a three-crate synthetic
workspace (leaf -> mid -> app, plus one registry dependency). Run on this
laptop, cargo 1.97.1 / sccache 0.17.0:

| probe | result | reading |
|---|---|---|
| cold build at path A | 4 crates compiled | baseline |
| identical tree at path B, **mtimes preserved**, same target dir | **0 crates compiled** | the path alone invalidates nothing |
| identical tree at path C, **mtimes restamped**, same target dir | **3 crates compiled** (all workspace members) | mtime is cargo's whole freshness test |
| sccache: wiped target, same source path, same target path | **4 hits / 0 misses** | the cache works |
| sccache: wiped target, **different source path**, same target path | **1 hit / 3 misses** | only the registry dep hit; every workspace crate missed |
| sccache: wiped target, same source path, **different target path** | **0 hits / 4 misses** | the target path is in the key too |
| sccache with `--remap-path-prefix` mapping each source path to one name | **0 hits / 4 misses** | remapping does not recover cross-path hits |

Mechanisms behind those readings:

- **cargo is relocatable but mtime-driven.** A workspace member's unit hash is
  computed from its package id hashed *relative to the workspace root*, and its
  dep-info is stored with workspace-relative paths, which is why moving the tree
  changed nothing and the artifact filenames were byte-identical. Freshness for
  a path dependency is then decided by comparing each source file's mtime
  against the artifact built from it. A fresh `git worktree add` — which is what
  creating a session directory does — writes every file with the current time,
  so the whole workspace is dirty however warm the target directory is.
- **sccache keys on the paths in the rustc invocation.** Same bytes, different
  directory, and every workspace crate missed; the one hit was `cfg-if`, whose
  source lives in the registry cache at a path that never moves. This is why the
  fleet host reported ~35% hits (112/203) while recompiling everything it owns:
  the dependency tree hit, the workspace missed. `--remap-path-prefix` was
  tested and does not fix it, so there is no flag-level escape — and adding one
  would have broken the gates' CI-argv parity anyway.

Together: a per-session checkout path forces a full workspace recompile *and*
cannot serve any of it from cache. That is the observed near-scratch lane.

**Two mechanisms ruled out, so eve does not re-litigate them:** `RUSTC_WRAPPER`
is exported uniformly by `sccache::export` for every gate, and
`CARGO_INCREMENTAL=0` is in `CI_BUILD_ENV` for all of them — the incremental /
sccache conflict is already correctly resolved. Feature sets differ between
gates (clippy default, docs and test `--all-features`) but that is per-unit
hashing, not invalidation, and it is identical under either checkout scheme.

**The fix** (`3daac5249`): a lane that carries no conversation resolves no
session and builds in its slot's own checkout again. Verify materializes its
tree from the order's checkout object, so it is reproducible at any path; the
model lanes (`construct.implement`, `review.critic`, `scope.fill`) keep their
session trees, which is what `#5425` is actually for. In the slot checkout the
reset is `git checkout --detach --force` over a tree that already holds a
neighbouring subject, so only the files that differ between the two members are
rewritten — a handful — and the paths are the ones that slot has always built
at, so sccache serves the rest.

There is a soundness half to this, not only a speed half. Cargo decides a source
file is unchanged by finding it *older* than the artifact built from it, so a
target directory lent to a tree whose files predate its artifacts will reuse the
artifact instead of the file. Slot target x session tree was exactly that
arrangement, and slot affinity is only a preference — a member bumped to another
slot could be judged against another member's compiled code. Pairing the slot's
checkout with the slot's target closes it for the gate that judges candidates.

**Residual risk, deliberately not fixed here:** model lanes still build their
session trees against the slot's shared target directory, so the stale-artifact
window above still exists for the *model's own* `cargo check` runs. The gate is
now sound and the model's incidental builds are not. Closing it properly means a
target directory per session, which multiplies the largest directory on the host
by the member count rather than by the lane ceiling — a janitor-budget question,
and its own issue.

**Expected saving.** A member's first verify lane went from a from-scratch
workspace build (the 12-24 min observation) to a slot-warm one. Projected warm
lane after this item alone, using eve's own warm gate numbers: clippy 43s +
docs 26s + prepare 95s + test 42s + the small gates ~= **3.5-4 min**, from
12-24 min.

## Item 2 — independent gates run concurrently

**Measured constraint that shapes the design:** two cargo invocations against
one `CARGO_TARGET_DIR` do not overlap. The second prints `Blocking waiting for
file lock on artifact directory` and waits — reproduced locally with a 6s
`build.rs` sleep; `cargo doc` started 1s into a `cargo build` finished 1s after
it, not 5s after. So overlapping clippy with docs, or the wasm prepare with
either, buys nothing while they share a target directory, and giving each its
own directory would make all three rebuild the shared dependency tree they
currently amortize. The lanes therefore split on *who writes artifacts*:

- **compiling lane** (serial): `verify.clippy`, `verify.docs`, `verify.test`
  (with its `xtask dist` prepare).
- **read-only lane** (runs beside it): `verify.suppress`, `verify.fmt`,
  `verify.dup`, `verify.deps`, `verify.lock`. None of them spawns rustc;
  `verify.lock` is `cargo metadata --locked`, which builds nothing.

Every gate still spawns with its own pipes, so attribution never depended on
interleaving. Receipts, the evidence `log` listing and the umbrella exit code
are reassembled in CI-parity order rather than completion order, so the
"first failing member" is still the first *listed* failing member. Per-gate
`duration_millis` and `prepare_millis` remain wall clock around that gate's own
work; the gate receipts now sum to more than the umbrella total, and the field's
doc says so — the difference is reclaimed overlap, not overhead. `PeakMemory`
moved to an atomic high-water mark since two threads report into it.

A tripwire test asserts every member that compiles is in the build lane, keyed
on its own argv (`cargo` + a subcommand that is not `fmt`/`metadata`) or the
presence of a prepare — so a new compiling member cannot be silently scheduled
against a lock another gate holds.

**Expected saving:** the read-only gates' wall clock disappears into the
compiles. On CI's numbers dup+deps+lock are ~1 min and format 0.4 min; on eve
scope+suppress+fmt measured ~3s and dup is the long pole. Projected **20-60s**
off a warm lane, more when jscpd is slow.

## Item 3 — the wasm prepare skips when nothing it reads changed

`cargo xtask dist` cross-builds each component package in its own cargo
invocation (batching `-p` is forbidden: feature unification makes wasm-lld
reject duplicate `init` / `receive_p32` symbols) plus the behaviour variants and
the chassis bins. Warm, nearly all of that is cargo being told there is nothing
to do — once per package, each resolving the workspace again.

The build is now keyed on what it reads: a sha256 over every workspace member's
source tree, the workspace manifests (`Cargo.toml`, `Cargo.lock`,
`rust-toolchain.toml`), the profile, the `--no-bins` selector, and the
compiler's version line. The key is stamped at
`<target>/wasm32-unknown-unknown/<profile>/.aether-dist-key`, so it dies with
the artifacts it describes; a run whose key matches *and* whose every expected
artifact still exists skips the builds. `dist/` itself is always reassembled, so
a `git clean` that took it cannot leave a lane without a manifest.

The key is content-based, never mtime-based — deliberately, since the case worth
skipping is exactly the fresh-checkout case cargo cannot skip. Everything
uncertain resolves toward building: unreadable file, tree deeper than the walk's
cap (32, iterative over an explicit stack), missing artifact, unwritable stamp.
The stamp is cleared before the first build and written only after the last, so
an interrupted run leaves nothing to trust.

The chassis binaries are in the key's scope too, not just the component crates:
`dist/bin/` is what `FleetHarness` resolves `aether-headless` through, so a key
over component sources alone would have handed the test gate a stale substrate.
The whole workspace is therefore the input set — an over-approximation that
fails toward rebuilding.

**Measured on this laptop:** 82s to build, 20s to rebuild after a one-line
change to `aether-kit-commons`, **8s to decide it need not build** (of which
~6s is `cargo run -p xtask` and `cargo metadata`; the digest itself is under a
second over ~100 crates).

**Expected saving:** eve's warm prepare is 95s; a skip is ~5s there. Worth
**~90s** on any lane whose tree matches the previous dispatch in that slot —
which, with item 1 keeping members on slot checkouts, is the repair-lap case and
the base-replay case rather than a rarity.

## Item 4 — the per-binary nextest slicing: load-bearing, kept

**The premise in the brief does not hold, and the finding is worth recording.**
`verify.test` is **one** nextest invocation:
`cargo nextest run --all-features --profile ci --no-fail-fast` plus `-p` flags
for the candidate closure. It is not sliced. The `-E 'binary_id(...)'`
invocations are the **post-failure triage** path (FIX-4b), they filter per
*test* rather than per binary, and `binary_id(...)` is there only to disambiguate
a test name that exists in several binaries.

That slicing is load-bearing and stays:

- **Step 1** replays one failing test against its own persisted counterexample
  under `PROPTEST_CASES=1`. Batched, a proptest's `minimal failing input:` line
  would be scanned out of a shared log and mislabel every sibling's flake-ledger
  entry, and a solo replay's idle box is part of what "same input" means.
- **Step 2** runs the same test at the work order's base to separate an
  inherited failure from a new one.

What *was* wrong is what each of those replays cost. Step 2 opened a fresh
detached worktree per replay, gave it a `CARGO_TARGET_DIR` inside itself, ran
its own `cargo xtask dist`, and then removed the worktree — deleting that target
directory. Four triaged failures meant four from-scratch workspace builds of one
commit. The checkout is now the runner's: opened once, prepared once, reused,
closed when the member finishes. This composes with item 3 — the base tree's
prepare is itself keyed.

**Expected saving:** zero on a green lane; on a red one with *N* triaged
failures it removes *N-1* cold workspace builds, which on eve is minutes each.
This is the difference between a failing lane costing 20 minutes and 4.

## Projected end state

| stage | now (measured, eve) | after items 1-4 (projected) |
|---|---|---|
| whole warm `verify.check` | 12-24 min | **2.5-3 min** |
| clippy | 43s | 43s (unchanged, already 3.6x the runner) |
| docs | 26s | 26s (unchanged, already 6x the runner) |
| wasm prepare | 95s warm, worse cold | ~5s on a key hit, 95s on a miss |
| test compile + run | compile + 42s | 42s + whatever the closure changed |
| dup / deps / lock / fmt / suppress | serial, additive | 0 added wall clock (overlapped) |
| a member's *first* lane | full workspace build | slot-warm |

The <= 3 min bar is reachable if and only if item 1 behaves as measured on the
synthetic workspace. If eve shows slot checkouts still recompiling broadly, the
next suspect is the reset itself — `git checkout --detach --force` between two
members whose subjects differ widely rewrites more files than expected — and the
measurement for that is in the validation plan below.

## Validating on eve — commands, in order

Run from a checkout of `perf/verify-pipeline` on the fleet host.

**0. The mechanism probe (2 minutes, no lane needed).** Confirms the four
readings above on Linux with the fleet's own cargo/sccache:

```bash
scripts/lane-warmth-probe.sh /mnt/dev/tmp/warmth-probe
```

Expect: `hypothesis 1 ... CONFIRMED`, the same-path sccache rebuild all hits,
the different-source-path rebuild hitting only the registry dependency, and the
different-target-path rebuild missing everything. Any other shape means the
root-cause analysis above does not transfer and item 1 needs re-deriving before
it is trusted.

**1. Unit gates.** All four were run on this laptop against the final tree:

| gate | exit | note |
|---|---|---|
| `cargo fmt -- --check` | 0 | |
| `cargo clippy -p xtask -p aether-chassis-bloomery --all-targets -- -D warnings` | 0 | |
| `cargo test -p xtask` | 0 | 407 passed |
| `cargo test -p aether-chassis-bloomery` | 101 | 825 lib + 1 scenario passed; `a_bloom_with_all_scripted_verdicts_lands` failed |

That one scenario fails **identically at `night24-perf-base` with every change
in this branch reverted** — `expected a document reply, got Err { error: "the
boot journal replay has not finished; the projection is not yet the one the
journal describes" }` (`crates/aether-harness-bloomery/src/support/wire.rs:90`).
It is a pre-existing failure on this macOS laptop, not a regression from this
work. Re-run it first on eve; if it is green there, it is environmental and this
branch is clean.

```bash
cargo fmt -- --check
cargo clippy -p xtask -p aether-chassis-bloomery --all-targets -- -D warnings
cargo test -p xtask
cargo test -p aether-chassis-bloomery
```

**2. The dist key, end to end.**

```bash
cd /mnt/dev/workspace/aether
time cargo xtask dist                      # cold or warm, builds
time cargo xtask dist                      # expect: "sources unchanged ...", seconds
echo '// probe' >> crates/aether-kit-commons/src/lib.rs
time cargo xtask dist                      # expect: rebuilds the kit crates
git checkout -- crates/aether-kit-commons/src/lib.rs
```

**3. A real lane, cold then warm.** The acceptance measurement. Pick a member
and run its verify twice in the same slot, reading the gate receipts:

```bash
cd /mnt/dev/bloomery/worktrees/slot-0        # a slot checkout, post-fix
CARGO_TARGET_DIR=/mnt/dev/bloomery/worktrees/slot-0-target \
  cargo xtask transform verify.check --out /tmp/verify-1
jq '{total: .duration_millis, gates: [.gates[] | {command, duration_millis, prepare_millis}], sccache}' \
  /tmp/verify-1/evidence.json
```

Then, without touching the tree, run it again into `/tmp/verify-2` and compare.
Expect on the second run: `prepare_millis` in the single-digit seconds, sccache
`hits` far exceeding `misses`, and `duration_millis` under the sum of the gate
receipts (the overlap from item 2). The bar is `duration_millis < 180000`.

**4. Two members through one slot** — the case item 1 is actually about:

```bash
# dispatch member A's verify, then member B's, into the same slot;
# read both evidence.json files
jq -r '"\(.command) total=\(.duration_millis)ms sccache=\(.sccache)"' /tmp/verify-a/evidence.json /tmp/verify-b/evidence.json
```

Expect B's total to be close to A's, not several times it. A B that is multiples
of A means the slot reset is rewriting far more than the two members' diffs, and
the next step is `git diff --name-only` between their subjects.

**5. Concurrency sanity.** In any run's logs, `verify.dup.log` /
`verify.deps.log` should show no `Blocking waiting for file lock` line — if one
appears, a read-only member is touching the target directory and the lane
partition needs revisiting.

## Risks

- **Item 1 narrows a recent decision.** `#5425`'s acceptance test asserted that
  *every* lane of a member stands in its session tree; it now asserts that of
  the model-driven lanes, with a sibling test pinning the mechanical lanes to
  their slot. If a verify lane turns out to depend on session-tree state the
  order does not carry, this is where it will show — as a verify judging a tree
  that differs from what the construct produced. The order carries a checkout
  digest and the capture is a commit, so it should not, but it is the one
  assumption worth watching in the first bloom after this lands.
- **The containment test changed shape.**
  `containment_reads_the_finishing_lanes_own_tree` guarded a finishing lane's
  tree being reset by the sibling dispatched behind it. That pairing (verify +
  its member's construct) no longer shares a directory; two mechanical lanes
  taking the same slot still do, and the test now exercises that. The hazard is
  the same one; the pair that produces it changed.
- **Item 2's exit-code ordering.** The umbrella's exit code is the first
  *listed* failing gate's, which under concurrency is no longer the first to
  fail in time. This is deliberate and matches the previous behaviour exactly,
  but a reader comparing a lane's wall-clock log order against the receipt order
  will see them differ.
- **Item 3 over-approximates its inputs.** The key covers every workspace
  member's tree, so any change anywhere rebuilds all the component wasm. That is
  the safe direction and it means the skip fires less often than a
  narrowly-keyed one would. Narrowing it to each component's dependency closure
  is a later refinement, and it is only safe once the chassis binaries are keyed
  separately from the wasm.
- **Item 4 leaves the base build cold once per member run.** The first base
  replay still pays a full build at the base commit in a fresh worktree, because
  its target directory cannot be shared with the candidate's (divergent source,
  one target directory — the phantom-error arrangement `CLAUDE.md` warns about).
  Persisting a base target directory keyed by commit would remove even that, at
  the cost of a directory the janitor would have to bound.
