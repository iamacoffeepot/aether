#!/usr/bin/env python3
"""Build prompts_v2/ — the finder prompt with five targeted edits.

Each edit answers a failure observed in the five medium trials. Everything else
is byte-identical to the production prompt, so a score difference is
attributable to these words and nothing else.
"""
import pathlib, sys
import os

RUN = pathlib.Path(os.environ.get("MUSE_RUN_DIR", pathlib.Path(__file__).resolve().parent.parent))
SRC, DST = RUN / "prompts/v1", RUN / "prompts/v2"
DST.mkdir(exist_ok=True)

# A. The shape list read as a closed whitelist. Muse: "not a MISSING BOUNDS CAP
#    shape under this lens" — withheld a defect it had already diagnosed.
A_OLD = 'Named bug-shapes (NOT "find any bug" — flag only these, each with a concrete misbehaving input/path).'
A_NEW = ('Named bug-shapes (NOT "find any bug" — stay inside this lens, each finding with a concrete misbehaving '
         'input/path). These shapes are the lens\'s focus, not an exhaustive checklist: if the change makes the code '
         'behave contrary to its own contract and the defect is nearest to one of these shapes, report it under that '
         'shape. Never withhold a behavioral defect you can name because it is an imperfect fit for a shape\'s wording.')

# B. MISSING BOUNDS CAP is worded around iteration/recursion/growth, so an
#    allocation sized from the wrong quantity (s5) and an uncapped read of an
#    untrusted-size file (r1) both fell outside it on a literal reading.
B_OLD = ('- MISSING BOUNDS CAP: recursion or geometrically-/user-derived iteration without the CLAUDE.md-mandated '
         'depth/budget cap that returns an error rather than overflowing; unbounded growth; integer overflow on '
         'user-derived arithmetic.')
B_NEW = ('- MISSING BOUNDS CAP: recursion or geometrically-/user-derived iteration without the CLAUDE.md-mandated '
         'depth/budget cap that returns an error rather than overflowing; unbounded growth; integer overflow on '
         'user-derived arithmetic. Also: a bound computed from the WRONG quantity (sized against the whole input '
         'rather than what remains, a limit applied after the allocation instead of before), and an untrusted-size '
         'input read or allocated whole with no maximum. "Bounded by something, eventually" is not a cap — ask what '
         'the largest allocation this code can be made to perform is, and against which quantity.')

# C. The lintCandidates chute took a contract-level judgment (s5) that no linter
#    could reach. Narrow it to what is decidable without knowing the contract.
C_OLD = 'Put mechanically-decidable observations in lintCandidates, not findings.'
C_NEW = ('Put mechanically-decidable observations in lintCandidates, not findings — meaning only what a linter could '
         'decide with no knowledge of what this code is supposed to do. If you had to reason about the code\'s own '
         'contract to conclude it is wrong, it is a finding, even when a lint fires nearby. Routing a defect you have '
         'already diagnosed to lintCandidates is a miss, not caution.')

# D. "Be precise and conservative" suppressed the medium-confidence band: on the
#    items it drops, Muse never once rated a finding severity:high, and 100% of
#    its never-dropped hits were confidence:high. Separate rigor from severity.
D_OLD = ('Be precise and conservative: a confident story is not a finding; flag only when you can name the concrete '
         'better form AND why it wins.')
D_NEW = ('Be precise: a confident story is not a finding; flag only when you can name the concrete better form AND why '
         'it wins. Conservative means not inventing a misbehaving path you cannot name — it does NOT mean withholding '
         'a defect you can name because its blast radius is small, its severity low, or your confidence merely medium. '
         'Report at the confidence you actually hold; a medium-confidence finding with a named path is wanted.')

# E. The aimed-at-the-silent-misses edit. r1 needs the sibling path that enforces
#    the cap; r2 needs the caller that supplies a zero-fuel store; s8 needs the
#    spec fixing 127 as the maximum. The old text restricts where you REPORT and
#    was plausibly read as restricting where you LOOK.
E_OLD = ('Report ONLY sites located in the file(s) above: if you notice an issue in a different file they reference, '
         'do not report it (that file gets its own finder).')
E_NEW = ('Report ONLY sites located in the file(s) above: if you notice an issue in a different file they reference, '
         'do not report it (that file gets its own finder). That restricts where you REPORT, never where you LOOK — '
         'read whatever you need to judge THIS change: the callers that establish its preconditions, a sibling doing '
         'the same job under a guard this code lacks, the type whose method it calls, the ADR or doc comment that '
         'fixes a constant. Much correct-looking code is wrong only against something defined elsewhere. Before you '
         'conclude a file is clean, state to yourself what the changed lines depend on being true, and go check it.')

EDITS = [("A", A_OLD, A_NEW), ("B", B_OLD, B_NEW), ("C", C_OLD, C_NEW), ("D", D_OLD, D_NEW), ("E", E_OLD, E_NEW)]

applied = {k: 0 for k, _, _ in EDITS}
files = sorted(p for p in SRC.glob("*.txt") if ".built." not in p.name)
for src in files:
    text = src.read_text()
    for key, old, new in EDITS:
        if old in text:
            text = text.replace(old, new)
            applied[key] += 1
    (DST / src.name).write_text(text)

print(f"{len(files)} prompts -> {DST}")
for key, _, _ in EDITS:
    n = applied[key]
    note = "" if n else "   <-- NEVER MATCHED"
    print(f"  edit {key}: applied to {n}/{len(files)}{note}")
if applied["A"] != applied["B"]:
    print(f"  (A/B are correctness-lens only — {applied['B']} correctness, "
          f"{len(files) - applied['B']} test-integrity; expected)")
sys.exit(0 if all(applied.values()) else 1)
