import assert from 'node:assert/strict'
import test from 'node:test'

import { withinTrailingWindow, deterministicSample, parseClosingIssue, isCodeBearing } from './quality-eval-select.mjs'
import { parseVerdict, aggregateRates } from './quality-eval-judge.mjs'

const DAY = 86_400_000
const NOW = 1_700_000_000_000 // fixed "now" so the window math is deterministic

test('withinTrailingWindow keeps merged PRs inside the window and drops the rest', () => {
  const prs = [
    { number: 1, merged_at: new Date(NOW - 1 * DAY).toISOString() }, // in
    { number: 2, merged_at: new Date(NOW - 6 * DAY).toISOString() }, // in
    { number: 3, merged_at: new Date(NOW - 8 * DAY).toISOString() }, // out (too old)
    { number: 4, merged_at: null }, // out (closed, never merged)
    { number: 5, merged_at: new Date(NOW + 1 * DAY).toISOString() }, // out (future)
  ]
  const kept = withinTrailingWindow(prs, NOW, 7).map((p) => p.number)
  assert.deepEqual(kept, [1, 2])
})

test('deterministicSample is stable across input order and caps at n', () => {
  const items = [{ id: 'a' }, { id: 'b' }, { id: 'c' }, { id: 'd' }, { id: 'e' }]
  const key = (x) => x.id
  const first = deterministicSample(items, 3, key)
  const shuffled = deterministicSample([...items].reverse(), 3, key)
  assert.equal(first.length, 3)
  // Same candidate set -> same sample regardless of input order (re-runnable).
  assert.deepEqual(first.map(key), shuffled.map(key))
  // n >= length returns every item.
  assert.equal(deterministicSample(items, 99, key).length, items.length)
})

test('parseClosingIssue reads the linked issue, or null', () => {
  assert.equal(parseClosingIssue('Closes #1234.\n\n## Summary'), 1234)
  assert.equal(parseClosingIssue('fixes #7'), 7)
  assert.equal(parseClosingIssue('Resolves #42 in passing'), 42)
  assert.equal(parseClosingIssue('no reference here'), null)
})

test('isCodeBearing requires a crate source file', () => {
  assert.equal(isCodeBearing(['crates/aether-data/src/lib.rs']), true)
  assert.equal(isCodeBearing(['docs/guide/x.md', '.github/workflows/ci.yml']), false)
  assert.equal(isCodeBearing([]), false)
})

test('parseVerdict extracts verdict + defect_class, ignoring class on correct', () => {
  assert.deepEqual(parseVerdict('```quality-verdict\nverdict: correct\ndefect_class: none\n```'), {
    verdict: 'correct',
    defect_class: null,
  })
  assert.deepEqual(parseVerdict('verdict: defect\ndefect_class: boundary'), {
    verdict: 'defect',
    defect_class: 'boundary',
  })
  // A defect verdict whose class reads "none" carries no class.
  assert.deepEqual(parseVerdict('verdict: defect\ndefect_class: -'), { verdict: 'defect', defect_class: null })
  // Unparseable -> unknown, never scored as correct.
  assert.deepEqual(parseVerdict('the model rambled'), { verdict: 'unknown', defect_class: null })
})

// Tripwire: the rate aggregation is the harness's whole output — a regression
// alarm reads these numbers to decide whether routing degraded correctness. The
// defect_rate is computed over SCORED (correct+defect) samples only; an
// `unknown` verdict is counted in `total`/`unknown` but must never move the
// rate. If the grouping key, the scored denominator, or the overall roll-up
// ever drifts, these fixed rates change and this test catches it.
test('aggregateRates groups by size/model and rates over scored samples only', () => {
  const verdicts = [
    { issue: 1, verdict: 'correct', size_label: 'size:l', model_label: 'model:opus' },
    { issue: 2, verdict: 'defect', size_label: 'size:l', model_label: 'model:opus' },
    { issue: 3, verdict: 'correct', size_label: 'size:m', model_label: 'model:sonnet' },
    { issue: 4, verdict: 'unknown', size_label: 'size:m', model_label: 'model:sonnet' },
  ]
  const { rows, overall } = aggregateRates(verdicts)

  const lOpus = rows.find((r) => r.size_label === 'size:l' && r.model_label === 'model:opus')
  assert.deepEqual(
    { total: lOpus.total, correct: lOpus.correct, defect: lOpus.defect, defect_rate: lOpus.defect_rate },
    { total: 2, correct: 1, defect: 1, defect_rate: 0.5 },
  )

  const mSonnet = rows.find((r) => r.size_label === 'size:m' && r.model_label === 'model:sonnet')
  // One correct + one unknown: scored denominator is 1, so defect_rate is 0 — the
  // unknown neither scores as correct nor inflates the defect rate.
  assert.deepEqual(
    { total: mSonnet.total, correct: mSonnet.correct, unknown: mSonnet.unknown, defect_rate: mSonnet.defect_rate },
    { total: 2, correct: 1, unknown: 1, defect_rate: 0 },
  )

  assert.equal(overall.total, 4)
  assert.equal(overall.correct, 2)
  assert.equal(overall.defect, 1)
  assert.equal(overall.defect_rate, 1 / 3) // 1 defect over 3 scored (correct+defect)
})

test('aggregateRates reports a null rate when no sample is scorable', () => {
  const { overall } = aggregateRates([{ issue: 9, verdict: 'unknown', size_label: 'size:s', model_label: 'model:opus' }])
  assert.equal(overall.defect_rate, null)
})
