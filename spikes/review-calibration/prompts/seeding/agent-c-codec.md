# Seeding agent C — codec bugs + clean control (s2, s5, c3)

Dispatched as a `general-purpose` background agent. Token usage: 110,051 subagent tokens, 18 tool uses, 381.8 s wall.

## Prompt (verbatim, paths sanitized)

You are constructing a benchmark dataset of seeded bugs for calibrating a code-review agent. Work in the git worktree at `<WORKTREE>` (run all commands from there; do NOT cd elsewhere, do NOT touch any file except the three assigned below, do NOT commit, do NOT run cargo fmt).

Scratch dir: SCRATCH=`<SCRATCH>/dataset`

For EACH item:
1. FIRST copy the pristine file: mkdir -p $SCRATCH/\<item\> && cp \<file\> $SCRATCH/\<item\>/base.rs
2. Edit the live tree file per the spec, PLUS 10–30 lines of BENIGN cover changes in the same file (safe, behavior-preserving: doc-comment improvements, renaming a local, extracting a well-named local). The diff should read like a normal small refactor PR where exactly ONE change is the bug (zero for the clean item). Benign changes must be provably behavior-preserving. No comment may hint at any bug. Match existing style; lines ≤ 120; no section-divider comments.
3. Verify: cargo check -p aether-codec must PASS with no NEW warnings. The crate's existing tests must still pass for touched code paths where the bug does not reach them — do NOT worry if the seeded bug itself would fail a test IF a test directly pins the buggy behavior; in that case choose a site the existing tests do not pin (run cargo test -p aether-codec to confirm the suite still passes WITH your seeded bugs; if a test fails, move the bug to an unpinned site).
4. Write the diff: git diff -- \<file\> > $SCRATCH/\<item\>/diff.patch (must be non-empty).

ITEMS:

item s2_unwrap (level L1, shape swallowed-error/unwrap-on-fallible):
file crates/aether-codec/src/frame.rs
Inject a blatant unwrap()/expect() on a runtime-fallible path (e.g. a length conversion, a decode step, a read that can legitimately fail on malformed/oversized input) where the surrounding code otherwise returns errors. Clearly wrong to a careful reviewer.

item s5_bounds_order (level L3, shape missing-bounds-cap/ordering):
file crates/aether-codec/src/decode.rs
Inject a bounds/validation ordering bug: a length or size check that currently guards an allocation/consume moved (or weakened) so untrusted input reaches the allocation/consume before the check — e.g. Vec::with_capacity(len) or a slice take driven by a wire-supplied length before the length is validated against the remaining input. Subtle: the check still exists somewhere, just too late or against the wrong quantity.

item c3_clean (CLEAN control — zero bugs):
file crates/aether-codec/src/encode.rs
ONLY benign, provably behavior-preserving changes (15–35 changed lines): doc comments, local renames, extracting a named local. NO defect at all.

RETURN exactly this JSON (no prose around it):
{"items":[{"item":"s2_unwrap","file":"\<abs path\>","fn":"\<enclosing fn\>","line":\<line in EDITED file\>,"shape":"swallowed-error","level":"L1","bug":"\<one precise sentence\>","cover":"\<one sentence\>"},{"item":"s5_bounds_order",…,"shape":"missing-bounds-cap","level":"L3",…},{"item":"c3_clean","file":"\<abs path\>","fn":"","line":0,"shape":"none","level":"CLEAN","bug":"none","cover":"\<one sentence\>"}]}
If a specified site doesn't exist, pick the closest same-shape site in the SAME file and say so in "bug".

## Returned answer key (verbatim)

```json
{"items":[
 {"item":"s2_unwrap","file":"<WORKTREE>/crates/aether-codec/src/frame.rs","fn":"read_frame","line":168,"shape":"swallowed-error","level":"L1","bug":"The 4-byte length-prefix read is `r.read_exact(&mut prefix_buf).unwrap()` instead of `?`, so a truncated prefix or closed connection panics inside a fn that otherwise returns FrameError on every I/O path.","cover":"Renamed read_frame's locals (len_buf/len/buf to prefix_buf/body_len/body_buf) and reworded the read_frame and pop_frame doc comments."},
 {"item":"s5_bounds_order","file":"<WORKTREE>/crates/aether-codec/src/decode.rs","fn":"decode_wire_value","line":334,"shape":"missing-bounds-cap","level":"L3","bug":"The Vec arm pre-allocation clamp is `len.min(cur.bytes.len())` (total input length) instead of `len.min(cur.remaining())` (bytes still unconsumed), so a wire-supplied length at a nested/late cursor position drives Vec::with_capacity against the whole buffer rather than what actually remains — the comment still says 'against the bytes that remain'.","cover":"Renamed struct_alignment's a/f locals to max_align/field and added doc comments to alignment_of_schema, Cursor::remaining, and Cursor::take."},
 {"item":"c3_clean","file":"<WORKTREE>/crates/aether-codec/src/encode.rs","fn":"","line":0,"shape":"none","level":"CLEAN","bug":"none","cover":"Renamed locals (value/a to field_value/field_align in encode_struct_fields, a to align in the Scalar arm, a/f to max_align/field in alignment_of_schema) and reworded two doc comments — all behavior-preserving."}
]}
```

## Known downstream effect (accepted)

The seeded `read_frame` unwrap makes four socket-flavored tests in `aether-capabilities` fail deterministically (`rpc::wire::client::tests::peer_close_surfaces_eof_bye_on_inbound`, `engine::proxy::tests::proxy_reports_died_when_connection_closes`, and two `game::player` tests) — they exercise `read_frame` on EOF paths two crates downstream, which the codec-scoped seeder could not see. Accepted for the study: finder agents never run tests, and the refuter calibration ran read-only prompts. It is also a faithful property of a blatant L1 bug: CI would catch it in production.
