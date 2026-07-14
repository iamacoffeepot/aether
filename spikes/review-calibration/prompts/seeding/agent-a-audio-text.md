# Seeding agent A — audio/text bugs (s1, s3, s8)

Dispatched as a `general-purpose` background agent. Token usage: 108,034 subagent tokens, 28 tool uses, 414.2 s wall.

## Prompt (verbatim, paths sanitized to `<WORKTREE>`/`<SCRATCH>`)

You are constructing a benchmark dataset of seeded bugs for calibrating a code-review agent. Work in the git worktree at `<WORKTREE>` (run all commands from there; do NOT cd elsewhere, do NOT touch any file except the three assigned below, do NOT commit, do NOT run cargo fmt).

Scratch dir: SCRATCH=`<SCRATCH>/dataset`

You will seed exactly one bug into each of three files, disguised inside a plausible PR-style diff. For EACH item:
1. FIRST copy the pristine file: mkdir -p $SCRATCH/\<item\> && cp \<file\> $SCRATCH/\<item\>/base.rs
2. Edit the live tree file to inject the specified bug, PLUS 10–30 lines of BENIGN cover changes in the same file (safe, behavior-preserving: doc-comment improvements, renaming a local, extracting a well-named local, reordering independent statements). The final diff should read like a normal small refactor/feature PR where exactly ONE change is the bug. CRITICAL: benign changes must be provably behavior-preserving — when in doubt, use doc comments and local renames only. No comment may hint at the bug. Match the file's existing style and idiom; keep lines ≤ 120 chars; do not add section-divider comments.
3. Verify: cargo check -p aether-capabilities must PASS with no NEW warnings on your files.
4. Write the diff: git diff -- \<file\> > $SCRATCH/\<item\>/diff.patch (must be non-empty).

ITEMS:

item s1_swallowed_error (level L1, shape swallowed-error):
file crates/aether-capabilities/src/audio/runtime/handlers.rs
Inject a blatant swallowed error: a fallible operation on a real runtime path whose Result is discarded (`let _ = …;` or `.ok();`) where the surrounding code otherwise propagates or logs errors — e.g. a reply send or a load result that currently is handled, changed to be discarded. It must be on a genuinely fallible runtime path, clearly wrong to a careful reviewer, and compile clean.

item s3_off_by_one (level L2, shape missing-bounds/off-by-one):
file crates/aether-capabilities/src/text/runtime/atlas.rs
Inject an off-by-one in the shelf-packing/fit arithmetic: e.g. a fit check that uses <= where < is required (or drops a +1/padding term) so a glyph placed at the boundary can overflow or overlap a row by one pixel. Moderate subtlety: visible to a reviewer who traces the arithmetic, not to a skimmer.

item s8_pan_law (level L4, shape invariant-violation, very subtle):
file crates/aether-capabilities/src/audio/runtime/voice.rs
Find the constant-power pan law (bipolar pan i8, ADR-0127: 0 center, -128 hard left, 127 hard right, constant-power = sin/cos quarter-turn). Inject a VERY subtle breakage that compiles and behaves plausibly at center but violates the constant-power invariant or the mapping at extremes — e.g. both channels computed with cos, the normalization denominator off (127.0 vs 128.0 asymmetry inverted), or the quarter-turn angle doubled. It should survive a casual read; only someone who knows the pan-law math should catch it.

RETURN exactly this JSON (no prose around it):
{"items":[{"item":"s1_swallowed_error","file":"\<abs path\>","fn":"\<enclosing fn name\>","line":\<line in the EDITED file where the bug is\>,"shape":"swallowed-error","level":"L1","bug":"\<one precise sentence: what is wrong and the misbehaving path\>","cover":"\<one sentence: what the benign cover changes were\>"}, …same for s3_off_by_one and s8_pan_law]}
If any item cannot be seeded as specified (site doesn't exist), pick the closest same-shape site in the SAME file and say so in "bug".

## Returned answer key (verbatim)

```json
{"items":[
 {"item":"s1_swallowed_error","file":"<WORKTREE>/crates/aether-capabilities/src/audio/runtime/handlers.rs","fn":"handle_schedule","line":137,"shape":"swallowed-error","level":"L1","bug":"handle_schedule now discards the event-queue push result (`let _ = sender.push(ev);`) instead of returning ScheduleResult::Err on a full queue, so a dropped schedule batch is silently reported as Ok { accepted }.","cover":"Renamed the local `s` to `sender` across five handlers for consistency and added one-line doc comments to handle_note_on and handle_note_off."},
 {"item":"s3_off_by_one","file":"<WORKTREE>/crates/aether-capabilities/src/text/runtime/atlas.rs","fn":"pack","line":211,"shape":"missing-bounds/off-by-one","level":"L2","bug":"The shelf-height accumulator drops the padding term (`.max(height)` instead of `.max(padded_height)`), so the next shelf starts flush against the tallest glyph's bottom row — the one-pixel GLYPH_PADDING gutter between rows is lost and bilinear sampling bleeds neighboring glyphs' coverage across shelf boundaries.","cover":"Extracted `expected_len` for the coverage-length check, renamed the local `size` to `atlas_size` in get_or_insert, and renamed `out` to `bytes` in rect_rgba."},
 {"item":"s8_pan_law","file":"<WORKTREE>/crates/aether-capabilities/src/audio/runtime/voice.rs","fn":"pan_law","line":589,"shape":"invariant-violation","level":"L4","bug":"The pan normalization divides by 128.0 instead of 127.0 (inverting the ADR-0127 asymmetry), so pan=127 maps to 0.992 rather than 1.0 — hard right retains a ~-44 dB left-channel residue instead of fully panning, the clamp becomes dead code, and only pan=-128 still reaches a true extreme.","cover":"Renamed the velocity-normalization local `v` to `vel_norm` in OscVoice::new and PartialBankVoice::new, renamed `total` to `amp_total` in the partial normalization, and reworded the pan_law doc comment."}
]}
```
