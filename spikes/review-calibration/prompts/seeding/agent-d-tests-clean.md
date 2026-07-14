# Seeding agent D — junk tests + clean controls (t1, t2, c4, c2)

Dispatched as a `general-purpose` background agent. Token usage: 108,740 subagent tokens, 39 tool uses, 419.1 s wall.

## Prompt (verbatim, paths sanitized)

You are constructing a benchmark dataset for calibrating a code-review agent's TEST-INTEGRITY lens (this repo's testing policy: docs/guide/testing.md — read it first, plus the test-integrity section of CLAUDE.md). Work in the git worktree at `<WORKTREE>` (run all commands from there; do NOT touch any file except the four assigned below, do NOT commit, do NOT run cargo fmt).

Scratch dir: SCRATCH=`<SCRATCH>/dataset`

For EACH item:
1. FIRST copy the pristine file: mkdir -p $SCRATCH/\<item\> && cp \<file\> $SCRATCH/\<item\>/base.rs
2. Edit the live tree file per the spec. For test items, ALSO make 5–15 lines of benign cover changes in the same file (doc tweaks, local renames in other tests) so the diff isn't a single lone test. No comment may hint that a test is deliberately junk — write each junk test with full confident ceremony (doc comment, careful asserts), as a well-meaning contributor would.
3. Verify: the touched crate compiles (cargo check -p \<crate\>) AND every test you added PASSES (cargo test -p \<crate\> \<its name\>).
4. Write the diff: git diff -- \<file\> > $SCRATCH/\<item\>/diff.patch (must be non-empty).

ITEMS — choose 3 DIFFERENT existing test-bearing files (each item gets its own file, one file per item; pick files under crates/aether-capabilities/src/\*\*/tests\*.rs or crates/aether-kinds or crates/aether-data with existing #[test] fns where the needed types are in scope):

item t1_mirror (junk shape: derived-constant mirror):
Add a junk MIRROR test: e.g. assert_eq!(SomeKind::NAME, "its.literal.name") or a SchemaType-shape restatement (matches!(X::SCHEMA, SchemaType::Struct…)) — a test that restates a declaration with no independent truth. Give it a confident doc comment.

item t2_roundtrip (junk shape: derive-only roundtrip):
In a DIFFERENT file: add a junk derive-only roundtrip test — decode(encode(x)) == x over a plain #[derive(Kind, Schema, Serialize, Deserialize)] type with field-by-field asserts, exercising no crate-owned logic (the derive/codec machinery another crate owns and tests). Full ceremony.

item c4_tripwire (CLEAN control — a LEGIT test that must NOT be flagged):
In a THIRD file: add a genuine COMPUTED-value tripwire test: pin a value that is computed by owned logic — e.g. a derived KindId numeric value (KindId is derived by hashing the kind name — pinning the number catches hash-fn/name drift), a serialized byte layout, or equivalent computed constant available in that file's scope. It must carry a `// Tripwire: <invariant>` comment naming what drifts, and it must PASS (run it to get the actual value, then pin it).

item c2_clean (CLEAN code control — zero defects):
file crates/aether-capabilities/src/component/runtime/mod.rs
ONLY benign, provably behavior-preserving changes (15–35 changed lines): doc comments, local renames, extracting a named local. NO defect.

RETURN exactly this JSON (no prose around it):
{"items":[{"item":"t1_mirror","file":"\<abs path\>","fn":"\<test fn name\>","line":\<line in EDITED file\>,"shape":"mirror","level":"L1","bug":"\<one sentence: why it is junk\>","cover":"\<one sentence\>"},{"item":"t2_roundtrip",…,"shape":"derive-only-roundtrip","level":"L2",…},{"item":"c4_tripwire","file":"\<abs path\>","fn":"\<test fn\>","line":\<line\>,"shape":"none","level":"CLEAN","bug":"none — legit computed tripwire","cover":"\<one sentence\>"},{"item":"c2_clean","file":"\<abs path\>","fn":"","line":0,"shape":"none","level":"CLEAN","bug":"none","cover":"\<one sentence\>"}]}

## Returned answer key (verbatim)

```json
{"items":[
 {"item":"t1_mirror","file":"<WORKTREE>/crates/aether-kinds/src/transforms.rs","fn":"mat4_apply_kind_name_is_stable","line":69,"shape":"mirror","level":"L1","bug":"Asserts Mat4Apply::NAME against the same literal the #[kind(name)] attribute declares — no independent truth, so a rename edits both sides in one motion and the test can never fail on its own.","cover":"Renamed the existing test's local out→applied, reworded two comment lines, and tweaked the transform's doc comment."},
 {"item":"t2_roundtrip","file":"<WORKTREE>/crates/aether-kinds/src/text_metrics.rs","fn":"font_metrics_survives_a_wire_roundtrip","line":134,"shape":"derive-only-roundtrip","level":"L2","bug":"decode(encode(x)) over FontMetrics, whose Serialize/Deserialize/Schema are all plain derives — the symmetric roundtrip exercises only aether-data's codec (owned and tested there), no aether-kinds logic, so it passes regardless of what this crate does.","cover":"Renamed count→glyph_count in the caret monotonicity test and reworded two test comments."},
 {"item":"c4_tripwire","file":"<WORKTREE>/crates/aether-data/src/hash.rs","fn":"mailbox_id_derivation_is_pinned","line":343,"shape":"none","level":"CLEAN","bug":"none — legit computed tripwire: pins with_tag(Mailbox, fnv1a_64(MAILBOX_DOMAIN++name)) = 0x1BB7_BDAF_DA11_73C0, computed by this crate's own hash/tag logic, so it fails on hash-fn, domain-prefix, or tag-layout drift; carries a // Tripwire: comment.","cover":"Renamed big→oversized in the scope-path length test and reworded one comment."},
 {"item":"c2_clean","file":"<WORKTREE>/crates/aether-capabilities/src/component/runtime/mod.rs","fn":"","line":0,"shape":"none","level":"CLEAN","bug":"none","cover":"21 changed lines: renamed locals names→trampoline_names and d→descriptor, extracted a named capability_registry local, and reworded three doc-comment lines — all provably behavior-preserving; crate checks clean."}
]}
```
