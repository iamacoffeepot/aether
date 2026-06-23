# Testing

A test earns its place when you can name the bug it would catch and that bug is
plausible — a real branch, boundary, invariant, or wire contract a future edit
could break without also breaking the test. That is the whole bar. Before you
write a test, finish this sentence: *"this fails if someone ___."* If the only
honest ending is "edits the test," the test is junk and CI is poorer for it.

The sharpest form of that question is **what logic owned by this crate does the test
exercise?** Junk tests routinely pass the first question with a plausible story — "it
pins the wire name," "it round-trips the kind," "it proves the kind is registered" —
while exercising no logic this crate wrote. They run a `#[derive]` macro, the shared
codec, or the inventory registration, all owned and tested once elsewhere, and confirm
the value came back. If the only honest answer is "none of ours — it restates a
declaration or re-runs machinery another crate already tests," the test is junk no
matter how much ceremony surrounds it. Field-by-field assertions, a non-trivial value
under test, and a confident doc comment are not evidence of load; they are the
camouflage junk hides behind.

Junk tests are not free. Each one spends compile time, run time, and reviewer
attention, and every false sense of coverage it adds is a place a real regression
can slip through unnoticed. A small suite of load-bearing tests beats a large suite
padded with tests that pass no matter what the code does.

## What does not clear the bar

These shapes recur. None of them can fail for a reason you care about, so none of
them belong in the suite:

- **Mirror tests** restate the source as an assertion. `assert_eq!(Foo::default().x, 0)`
  sitting next to `x: 0` in the `Default` impl breaks only when someone edits both
  halves together, which is to say never on its own. The common disguise is the
  derived constant: `assert_eq!(NoteOn::NAME, "aether.audio.note_on")` reads like it
  guards the wire name, but `NAME` *is* the `#[kind(name = "…")]` literal — the
  assertion's expected value is the same string retyped, with no independent source of
  truth. A rename edits the attribute, and the test sitting beside it is updated in the
  same motion. Every real consumer routes on `NoteOn::NAME` or its hash, so they track
  the rename for free; the literal in the test is the one copy nobody downstream uses.
- **Round-tripping a derive-only type.** `decode(encode(x)) == x` over a type whose
  `Serialize` / `Deserialize` / `Schema` are all `#[derive]`d tests nothing this crate
  wrote. The roundtrip is **symmetric**: encode and decode are generated from the same
  definition, so any change to one changes the other in lockstep and the test still
  passes. It can fail only if the two *disagree* — which for a derived type means the
  derive macro is broken, and that is tested where the macro lives. Building an
  elaborate value and asserting each field survives does not change this; it confirms
  the shared codec is an identity function over your struct, which is the codec's
  invariant, tested once in `aether-data`. A roundtrip earns its place only when the
  type has hand-written ser/de, or an invariant the roundtrip actually exercises (a
  clamp, a normalization, a rejected input) — not when it is plain derives over plain
  fields.
- **Testing code you do not own.** The unit under test must be logic we wrote. A
  test that exercises a dependency's behavior catches a bug only the dependency's
  authors can fix, and we keep it green by never upgrading. This covers the standard
  library and the compiler (pushing three items and asserting `len() == 3`, checking
  that `#[derive(Clone)]` clones), serde and every other third-party crate (does
  `wgpu` clear the surface, does `tokio` schedule the task, does `fontdue` rasterize),
  and any generated code whose generator already has its own tests. The codec is the
  trap here: it is ours, but it is owned *once*, in `aether-data`. Testing it means
  testing it there — re-running it from a consumer crate on a consumer's struct tests
  the consumer's `#[derive]`s and the shared codec, neither of which is that crate's
  logic. When a test fails, the fix should land in the crate the test lives in — if it
  would land in someone else's, the test was never yours to write.
- **Re-testing shared engine machinery from a consumer.** Some logic we own is owned
  *once* and already tested where it lives: config resolution through
  `#[derive(Config)]` (argv > env > default, whether an env var is set, how the string
  parses), mail routing, settlement, id and lineage hashing, and everything the `Kind`
  / `Schema` derives emit. A capability test that asserts its own knob picks up
  `AETHER_FOO`, or that a missing variable falls back to the default, is testing the
  `Config` derive rather than the capability. The same trap wears other masks:
  - *Derive-emitted registration.* Asserting a kind appears in `descriptors::all()`
    guards nothing — `#[derive(Kind)]` emits the `inventory::submit!`, so the entry is
    present by the fact the type derives `Kind` and the crate is linked. There is no
    manual registration to forget; a missing entry means the derive is broken (tested
    elsewhere) or the type was deleted (a compile error, not a test failure).
  - *Schema-shape assertions.* `assert!(matches!(Role::SCHEMA, SchemaType::Enum))`
    restates the `enum` keyword through the derive. The derive maps `enum` → `Enum`
    and `struct` → `Struct` mechanically; the assertion adds nothing the declaration
    does not already say.

  Test what the capability *does* with these — the value it computes, the mail it sends,
  the input it rejects — and let the machinery's own suite cover that the derive, the
  codec, and the registry work.
- **Mock theater.** A test that stands up so many fakes it only ever exercises the
  fakes, asserting that a mock returns what the test told it to return. It verifies
  the setup, not the system.
- **No real assertion.** Calling the function and never checking the result;
  asserting only that it "didn't panic"; or checking the output against a value the
  test recomputes the same way the code does. An assertion needs a known-good oracle,
  not a second copy of the implementation.
- **Vacuous bodies.** `assert!(true)`, an empty test, a loop that runs zero times, an
  early `return` ahead of the assertion, or a guard that skips on every machine that
  will ever run it.
- **Bulk duplication.** Ten near-identical cases driving one branch with different
  literals. One table-driven case carries the same signal; the other nine are noise.
- **Coverage chasing.** A test written to turn a line green rather than because the
  behavior matters — a trivial getter, a `Display` impl with no logic, an
  exhaustiveness arm that can never be reached.

## The tripwire that looks like junk

A deliberately boring test can be load-bearing. Pinning the `MailboxId` lineage hash,
a wire format's byte layout, or a `KindId`'s numeric value reads like a mirror test —
a flat assertion against a fixed value. The difference is what sits on each side. A
tripwire pins a **computed** value — a hash, a serialized byte layout, a derived id —
against an independent constant, so it fails when the *logic that produces the value*
drifts even though the declaration that named it did not. That is a real contract:
downstream code depends on the computed value, the value can change invisibly, and the
test makes the change loud.

This is exactly what a derived-constant mirror is not. `assert_eq!(NoteOn::NAME,
"aether.audio.note_on")` has the declaration on one side and a copy of it on the other
— nothing is computed, nothing can drift on its own. If you want to guard a kind's wire
identity, pin the thing consumers actually route on and that *is* computed from the
name: `assert_eq!(NoteOn::ID, KindId(0x…))`. That fails if the hashing changes or the
name changes, both of which move the id without touching any line a reader would notice.
Pinning the name string against its own literal guards none of that.

Mark a genuine tripwire so the next reader (and the next sweep) can tell it from junk.
A one-line comment naming the invariant and why it is pinned is enough:

```rust
// Tripwire: this byte layout is the wire contract with the hub. A change here
// breaks every connected engine — if this assertion fails, update the protocol
// version, do not just re-bless the bytes.
assert_eq!(frame.as_bytes(), EXPECTED_WIRE_BYTES);
```

The comment is necessary but not sufficient: a comment over a value that cannot drift
on its own is a mirror with a story told over the top, and the sweep treats it as junk.
The contract is real only when the pinned value is computed.

## Where the test goes

Once a test clears the bar, the harness follows from what it checks. Engine-internal
and visual correctness goes to **TestBench** (`aether_substrate_bundle::test_bench`)
with a concrete assertion (`captured`, `reply`, `count_observed`); behavior over the
wire — recipient-name resolution, fleet lifecycle, the RPC boundary — goes to
**FleetBench** (`crates/aether-substrate-bundle/tests/fleetbench/`). FleetBench is
headless, so any rendered-output assertion has to be TestBench, and any
externally-addressable-over-the-wire assertion has to be FleetBench.
A test that drives neither harness and exercises none of our own pure logic is the
case to look at hardest, because there may be no engine behavior under it at all. Our
pure logic — the codec, `aether-math`, schema encode/decode, id and lineage hashing —
is load-bearing to test *in the crate that owns it* (`aether-data`, `aether-math`).
Re-running it from a consumer crate, on a consumer's derived type, is the junk case
above, not a second copy worth keeping.
