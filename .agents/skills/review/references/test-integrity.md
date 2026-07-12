# Test-Integrity Lens

Ask what logic owned by this crate the test exercises and what plausible owned-code regression it catches. Read `docs/guide/testing.md` before judging a disputed case.

## Junk categories

- `mirror`: restates a declaration, literal name, schema shape, or attribute.
- `derive-only-roundtrip`: checks symmetric generated encode/decode behavior without owned transformation logic.
- `not-owned`: primarily tests standard-library, dependency, derive, registry, or shared codec behavior owned elsewhere.
- `mock-theater`: verifies mock wiring instead of production behavior.
- `no-assertion`, `echo`, or `vacuous`: cannot fail on a meaningful owned regression.
- `bulk-duplicate`: repeats coverage already provided by a stronger focused test.
- `coverage-chasing`: exists to execute lines without pinning behavior.

## Bar

Keep a test when it exercises owned behavior another crate's tests do not cover, or pins a computed value such as a hash, golden byte sequence, or derived numeric id. A flat value copied from the declaration it tests is a mirror, not a tripwire. Recommend `remove` or a concrete `rewrite`; identify the owned logic the replacement must exercise.
