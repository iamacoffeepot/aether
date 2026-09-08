# member-independence spike (round 2)

Issue #5354. Round 1 lives at `REPORT-round-1.md`, carried forward from branch
`spike/symbolic-diff` (`ffc14e9f8`). This round implements the three things that
comment named as prerequisites and rewrites the measurement so it cannot report
a number the oracle never produced.

Not a workspace member: `main` never carries a `spikes/` tree, and this crate is
built from inside its own directory.

## The question

> A candidate's diff as items (added, removed, changed signature, changed body,
> renamed) plus the sites that reference each, computed once and read by every
> stage — then the pairwise intersection that decides whether two members of a
> wave are independent.

Two halves. The first is a value: `SymbolDiff`, one candidate against its base.
The second is a predicate over two of them: can these land in one wave without
breaking each other. The predicate is what has a consumer.

### The consumer already exists on `main`, deliberately toothless

`aether_bloomery::surface_intersection` (`crates/aether-bloomery/src/values/approval.rs:264`)
intersects two members' declared surfaces at the seal door, and
`Fact::SurfaceOverlap` journals what it found. `crates/aether-bloomery/src/reduce/event.rs`
states why it is only ever a warning:

> Coarse globs over-predict — two members declaring the same crate glob
> routinely fold clean — so an overlap that blocked would refuse far more seals
> than it saved.

That is the gap a symbolic diff is for, stated in the repository's own words.
The declared surface is a crate glob by standing rule (surfaces are crate globs,
never files), so two members touching one crate always intersect, and the
intersection can never say which of those overlaps is real. Nothing on `main`
distinguishes "same crate, disjoint symbols" from "same crate, one renames what
the other reads". Grepped `main` at `eb667275d`: no `SymbolDiff`, no symbolic
diff, no item-level intersection anywhere in `crates/` or `docs/`. The question
is open, and its consumer is waiting with a warning it cannot act on.

## Method

Same shape as round 1 — `syn` extraction per touched file at both revisions,
items keyed by a stable module path, rename detection by normalized body hash,
then a pairwise conflict set — with three changes.

### 1. References resolve through the `use` graph (`src/resolve.rs`)

Round 1 asked `git grep -w -F <last path segment>` and kept a hit if the file
sat in the same cargo package or mentioned the crate anywhere. On
`xtask::bloom::Endpoint::resolve` that collected every `resolve` in the
workspace: a `done.resolve` inside a derive crate's diagnostic string, a method
on an audio type. One pair reached 7,187 sentences at 108 s.

The question is now put to the reading file's import surface. In order:

1. A `use` that binds the name settles it both ways — a file importing `resolve`
   from another crate is reading *that* `resolve`, not ours.
2. No import, same crate as the target: keep it (an item is reachable through
   `crate::` / `super::` / module scope), unless the file defines its own item
   under that name and not the changed one.
3. No import, other crate: keep it only for `use target_crate::*` or a written
   `target_crate::` path.

Plus a prose filter: a comment line, or a line whose every whole-word occurrence
of the identifier sits inside a string literal, is not a reader.

The bias is deliberate and one-directional — every "cannot tell" answers *yes*,
so the residual error is extra conflicts, never a missed break.

### 2. `pub use` is a re-export edge, not a signature (`src/diff.rs`)

`ChangeKind::Reexport` is a third class. When either revision's item at a path is
a `use`, the change is a re-export: the definition moved, the name did not, and
a reader writing the same path still compiles. Round 1 called
`crates/aether-chassis-bloomery/.../containment.rs` replacing a local
`pub fn path_in_surface` with `pub use aether_bloomery::path_in_surface` a
`SignatureChanged`, and the pair a `Conflict(Breaks)`, over a call site that
never stopped compiling.

`Reexport` is excluded from Breaks and still gets a sentence, because the
residual risk is real: a re-export pointing at an item whose signature differs
is not followed across the crate boundary here.

### 3. The oracle replays both patches onto one compiling base (`src/eval.rs`)

Round 1's candidates were a linear chain, so `git merge-tree A B` was always a
fast-forward to the descendant and the compile verdict described *that commit*,
not the pair. Every tree inherited two pre-existing errors, so `CompileOk` never
occurred once in 21 pairs — and the report still printed precision 0.000, which
reads as "the tool is wrong" when it means "nothing was measured".

Both halves are fixed:

- Each candidate's parent-relative patch is replayed onto a shared base with
  `git merge-tree --write-tree --merge-base=<parent> <base> <candidate>`, A
  first then B, the intermediate tree wrapped by `git commit-tree`. Pinning the
  merge base is what makes an ancestor pair a real three-way merge.
- The base is named on the command line (`--base`), repaired by named patches
  (`--prepare <rev>`, repeatable), and **proved to compile before any pair runs**
  over exactly the union of packages the candidates touch. If it does not
  compile, every pair reads `OracleInvalid` and the metrics section says *no
  measurement*, naming the base error and the excluded count.
- Rows the oracle never decided are excluded from precision and recall rather
  than counted as negative, and an empty denominator prints `n/a`, not `0.000`.

The exit code of `merge-tree` is now the conflict authority; round 1 treated any
trailing output line as a conflict, which a pinned merge base makes wrong
(rename-detection notices print after a clean tree).

## Result

**Not measured.** Nothing in this round was executed. The measurement needs a
compute host: a `cargo check` per distinct pair tree over up to six packages,
times 21 pairs, plus the base check. The agent that wrote this round had no
build budget, so what is here is the instrument, not the reading.

To take the reading, from a checkout of this branch:

```
cd spikes/member-independence
cargo test                       # the unit tests below
cargo run --release -- eval --base <compiling base> --prepare <repair rev> ...
```

`eval` writes `REPORT-round-2.md` next to this file — pair table, precision over
decided rows only, false positives and negatives with their sentences, per
candidate and per pair cost, and the rung-3 test set against the crate suite.
Round 1's base `c51ee808b` is still red for the two errors its report names
(`detail.rs:387` usize/u16 and `policy.rs:25` missing `Display`, the latter
fixed by `86fc2f519`); pass those as `--prepare` or pick a base that is green,
or the run will correctly refuse to report a number.

### What was verified, and what was not

Verified:

- `cargo fmt` runs clean over the crate, so every file **parses**. That is a
  syntax check and nothing more — it says nothing about types, borrows, or
  behavior.
- The three changes are read-through-complete against round 1's source, which
  compiled and ran (its report carries real timings and real compiler output).
- Six unit tests were added over the new pure logic — the resolver's four
  decision paths, the string-literal filter and its escape handling, and the
  re-export classification. Each names the round-1 failure it catches. They are
  the cheapest thing to run first and they need no repository history.

Not verified:

- **Nothing was compiled.** `cargo build`, `cargo test`, `cargo clippy`, and
  `cargo fmt` were all unavailable to this agent. Expect to fix compile errors
  before the first run.
- No CI ran. `.github/workflows/ci.yml` triggers on pull requests and on pushes
  to `main` and `bloomery/daily/**` only, so a push to a `spike/**` branch runs
  nothing, and a spike does not open a pull request against `main`.
- No number in the "Method" section above is a round-2 measurement. Every figure
  quoted (7,187 sentences, 108 s, 48× the crate suite) is round 1's, from
  `REPORT-round-1.md`.

## Recommendation for `main`

**Keep it a spike. Do not put a symbolic diff on `main` yet, and do not open an
ADR for one, until round 2's measurement clears one specific bar.**

The bar: at least one pair where the crate-glob intersection reports an overlap
and the item-level intersection plus the compiler agree there is none — the
over-prediction `event.rs` names — and no pair where the item-level intersection
says independent and the compiler disagrees. That is the only evidence that
would let `Fact::SurfaceOverlap` become something a machine acts on instead of
advice an operator reads. Round 1 could not test it: its oracle never once said
yes.

Three things follow from what *is* known, and they hold regardless of how the
measurement lands:

1. **Cost rules out the per-seal path until references are cheap.** Round 1 cost
   30–110 s per pair, grep-bound. A wave of ten members is 45 pairs. The `use`
   graph should cut it hard — the noise it removes *is* the cost — but the
   measurement has to show that before this goes anywhere near the seal door.
2. **Rung-3 test selection is not the first consumer.** Round 1's selected test
   set was about 48× the crate's own suite. Selecting more tests than running
   everything is not a selection; that consumer stays parked until the reference
   resolution is measured.
3. **Independence stays advisory wherever it lands.** The standing rules do not
   move: surfaces are crate globs, waves compose on DAG edges only, and surface
   overlap derives no readiness edge. A symbolic diff sharpens the *warning*
   attached to a seal — it does not become a gate that refuses one, for exactly
   the reason `reduce_surface_overlap` gives today.

If the bar is cleared, the smallest useful landing is not the whole value: it is
the pairwise predicate alone, computed at the seal door where the declared
surfaces already are, feeding a sharper `Fact::SurfaceOverlap` intersection. The
`SymbolDiff` value, the repair scope, the review brief, and the predictor
features are all downstream of that one number being trustworthy.

## Layout

| file | what |
| --- | --- |
| `src/extract.rs` | `syn` walk to stable-path items with signature/body hashes |
| `src/diff.rs` | item classification between two revisions, including `Reexport` |
| `src/resolve.rs` | **new** — does a hit resolve to *this* item, per the file's `use` graph |
| `src/refs.rs` | grep hits classified as defining / referencing, filtered by the resolver |
| `src/independent.rs` | the pairwise conflict set and its verdict |
| `src/eval.rs` | shared-base replay oracle, base compile gate, metrics, report writer |
| `src/git.rs`, `src/tokens.rs` | git plumbing; normalized token hashing |

The interface stays language-parametric as the issue asks: `syn` is one producer
of items, and file granularity remains the degraded fallback for anything it
cannot parse — `refs.rs` already falls back to unparsed-file handling per hit.
