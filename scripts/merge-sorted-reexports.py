#!/usr/bin/env python3
"""Git merge driver for sorted one-name-per-line re-export lists.

Two members each appending a re-export to a crate root's `pub use` block is a
conflict with one obviously-correct answer, and repairing it through a reconcile
lane spends a model dispatch on a merge whose result is determined. This driver
resolves exactly that shape and refuses everything else.

Invoked by git as `merge-sorted-reexports.py %O %A %B`, where %O is the merge
base, %A is our side (and the file the result must be written to) and %B is
theirs. Exit 0 means resolved; any nonzero exit leaves the conflict for the
reconcile lane, which is the safe direction: a resolver that guesses produces a
silently wrong merge that no stage is looking at, which is strictly worse than a
conflict that stops and asks.

The refusal rules, in the order they are checked:

* Either side deleting or modifying a base line. Only whole-line insertions are
  in scope; anything else is a real edit needing judgement.
* A block both sides inserted at the same base position holding any line that is
  not exactly `pub use <path>;`. This is what keeps the driver safe when it is
  misapplied: `.gitattributes` lives in the tree and a candidate can edit it, so
  the driver must be harmless pointed at a path it was never meant for. An
  attribute line, a doc comment, a `pub mod`, or a braced list all refuse here.
* A resolved run that is not sorted. The union has to be byte-identical to what
  `cargo fmt` would leave, or a resolved conflict becomes a failed verify —
  `reorder_imports` is on by default, so an unsorted block is a fmt finding.
"""

import difflib
import re
import sys

# One re-export, one name, no alias and no braces — the form the crate roots
# hold. A braced list is deliberately out: its interior is what conflicts, and
# unioning two brace bodies is not a line-oriented insertion.
REEXPORT = re.compile(r"^pub use [A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)*;$")


def read(path):
    with open(path, "r", encoding="utf-8") as handle:
        return handle.read().splitlines(keepends=True)


def insertions(base, side):
    """Every whole-line insertion of `side` over `base`, keyed by base position.

    Returns None when `side` deletes or modifies anything, which is the driver's
    first refusal.
    """
    found = {}
    for tag, base_start, base_end, side_start, side_end in difflib.SequenceMatcher(
        None, base, side, autojunk=False
    ).get_opcodes():
        if tag == "equal":
            continue
        if tag != "insert":
            return None
        found.setdefault(base_start, []).extend(side[side_start:side_end])
    return found


def sorted_union(ours, theirs):
    """The sorted union of two insertion blocks, or None when out of scope."""
    combined = sorted(set(ours) | set(theirs))
    if not all(REEXPORT.match(line.rstrip("\n")) for line in combined):
        return None
    return combined


def merged(base, ours, theirs):
    """The resolved file, or None when the driver declines to resolve it."""
    left = insertions(base, ours)
    right = insertions(base, theirs)
    if left is None or right is None:
        return None

    result = []
    for position in range(len(base) + 1):
        mine = left.get(position, [])
        yours = right.get(position, [])
        if mine and yours:
            block = sorted_union(mine, yours)
            if block is None:
                return None
            result.extend(block)
        else:
            result.extend(mine or yours)
        if position < len(base):
            result.append(base[position])
    return result


def runs_are_sorted(lines):
    """Whether every maximal run of re-export lines is in ascending byte order.

    Checked over the whole result rather than only the blocks this driver
    touched: a union placed into a run the base had already left unsorted is
    still a `cargo fmt --check` failure, and the driver must not hand back a
    file it cannot claim fmt would accept.
    """
    run = []
    for line in lines + ["\n"]:
        if REEXPORT.match(line.rstrip("\n")):
            run.append(line)
            continue
        if run != sorted(run):
            return False
        run = []
    return True


def main(argv):
    if len(argv) < 4:
        print("usage: merge-sorted-reexports.py %O %A %B", file=sys.stderr)
        return 2

    base_path, ours_path, theirs_path = argv[1], argv[2], argv[3]
    base, ours, theirs = read(base_path), read(ours_path), read(theirs_path)

    result = merged(base, ours, theirs)
    if result is None or not runs_are_sorted(result):
        return 1

    with open(ours_path, "w", encoding="utf-8") as handle:
        handle.write("".join(result))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
