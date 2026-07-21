import assert from 'node:assert/strict'
import test from 'node:test'

import { findGaps, parseReviewable, toRepoRelative } from './review-args-completeness.mjs'

const REVIEWABLE = [
  'crates/aether-chassis-bloomery/tests/rest_api.rs',
  'crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs',
]

const complete = {
  files: ['crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs'],
  testFiles: ['crates/aether-chassis-bloomery/tests/rest_api.rs'],
  diffs: {
    'crates/aether-chassis-bloomery/tests/rest_api.rs': '@@ -0,0 +1,119 @@\n+// new test',
    'crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs': '@@ -0,0 +1,28 @@\n+// new test',
  },
}

// Tripwire (#3608): the completeness check is the primary lever that converts a
// silent partial review into a loud no-rollup failure. If findGaps ever stops
// firing on a dropped changed file, the exact #3600 defect reopens — the deep
// pass fabricates a "no test changes" finding (or, worse, APPROVEs) against a
// diff whose files the model never saw.
test('complete args (every changed file present with a non-empty diff) yields no gaps', () => {
  assert.deepEqual(findGaps(REVIEWABLE, complete), [])
})

// Tripwire (#3608): the #3600 shape — a changed file the acquisition dropped
// entirely, absent from files ∪ testFiles. It must be a gap, never silent.
test('a changed file absent from files/testFiles is a gap', () => {
  const args = {
    files: ['crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs'],
    testFiles: [],
    diffs: { 'crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs': '@@ +1 @@\n+x' },
  }
  const gaps = findGaps(REVIEWABLE, args)
  assert.deepEqual(gaps, [{ file: 'crates/aether-chassis-bloomery/tests/rest_api.rs', reason: 'absent-from-files' }])
})

// Tripwire (#3608): the subtler #3600 shape — a changed file present in the sets
// but whose hunk never reached args.diffs, which reads to the finders as
// "unchanged". A missing OR empty diff entry must both be gaps.
test('a changed file present but with a missing or empty diff is a gap', () => {
  const missing = {
    files: ['crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs'],
    testFiles: ['crates/aether-chassis-bloomery/tests/rest_api.rs'],
    diffs: { 'crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs': '@@ +1 @@\n+x' },
  }
  assert.deepEqual(findGaps(REVIEWABLE, missing), [
    { file: 'crates/aether-chassis-bloomery/tests/rest_api.rs', reason: 'missing-diff' },
  ])

  const empty = {
    ...complete,
    diffs: { ...complete.diffs, 'crates/aether-chassis-bloomery/tests/rest_api.rs': '   \n  ' },
  }
  assert.deepEqual(findGaps(REVIEWABLE, empty), [
    { file: 'crates/aether-chassis-bloomery/tests/rest_api.rs', reason: 'empty-diff' },
  ])
})

// Tripwire (#3608): the review session assembles args with ABSOLUTE paths
// (${GITHUB_WORKSPACE}/…) while the authoritative list is repo-relative. If the
// normalization ever breaks, every file reads as absent and the check goes from
// a real gate to a self-tripping no-op that fails every review.
test('absolute args paths normalize against GITHUB_WORKSPACE and match repo-relative reviewable paths', () => {
  const root = '/home/runner/work/aether/aether'
  const absolute = {
    files: [`${root}/crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs`],
    testFiles: [`${root}/crates/aether-chassis-bloomery/tests/rest_api.rs`],
    diffs: {
      [`${root}/crates/aether-chassis-bloomery/tests/rest_api.rs`]: '@@ +1 @@\n+a',
      [`${root}/crates/aether-chassis-bloomery/src/bloomery/approve/tests.rs`]: '@@ +1 @@\n+b',
    },
  }
  assert.deepEqual(findGaps(REVIEWABLE, absolute, root), [])
  assert.equal(toRepoRelative(`${root}/crates/x.rs`, root), 'crates/x.rs')
  assert.equal(toRepoRelative('crates/x.rs', root), 'crates/x.rs')
})

// Tripwire (#3608): the CLI accepts the authoritative set as either a JSON array
// or a newline list (the resolve step writes JSON; a hand run may pipe lines).
test('parseReviewable accepts a JSON array or a newline-separated list, dropping blanks', () => {
  assert.deepEqual(parseReviewable('["a.rs", "b.rs"]'), ['a.rs', 'b.rs'])
  assert.deepEqual(parseReviewable('a.rs\n\n  b.rs  \n'), ['a.rs', 'b.rs'])
})
