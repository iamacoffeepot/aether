import assert from 'node:assert/strict'
import test from 'node:test'

import {
  buildReviewBody,
  fingerprint,
  isActionable,
  normalizeFingerprint,
  shouldSubmitVerdict,
  verdictEvent,
} from './post-review-rollup.mjs'

const CONFIRMED = { confirmed: [{ pillar: 'correctness', file: 'a.rs', line: 3 }], softHolds: [], spec: { findings: [] } }
const SOFT_HOLD = { confirmed: [], softHolds: [{ pillar: 'economy' }], spec: { findings: [] } }
const HIGH_SPEC = { confirmed: [], softHolds: [], spec: { findings: [{ severity: 'high', description: 'unasked change' }] } }
const LOW_SPEC = { confirmed: [], softHolds: [], spec: { findings: [{ severity: 'low', description: 'nit' }] } }
const CLEAN = { confirmed: [], softHolds: [], spec: { findings: [] } }
const FOLLOWUPS_ONLY = {
  confirmed: [],
  softHolds: [],
  spec: { findings: [] },
  followUps: [{ pillar: 'correctness', file: 'a.rs', line: 3, source: 'pre-existing', severity: 'high' }],
}

// Tripwire: the actionable predicate is what both the native verdict and the
// mirror label read; if these three actionable shapes ever stop counting, a
// blocking verdict silently downgrades to APPROVE. A low-severity spec finding
// is NOT actionable — only high-severity spec findings gate.
test('isActionable fires on a confirmed finding, a soft-hold, or a high-severity spec finding', () => {
  assert.equal(isActionable(CONFIRMED), true)
  assert.equal(isActionable(SOFT_HOLD), true)
  assert.equal(isActionable(HIGH_SPEC), true)
  assert.equal(isActionable(LOW_SPEC), false)
  assert.equal(isActionable(CLEAN), false)
})

// Tripwire (#3250): a provenance-routed pre-existing finding lands in `followUps`, which
// isActionable / verdictEvent must IGNORE — the whole point of the channel is that a pre-existing
// bug in code the diff merely touched never gates the PR that revealed it. If followUps ever started
// counting toward actionable, the catch-22 this fixes reopens: the correctness pillar demands an
// in-PR fix and the spec pillar then punishes it as scope leakage, wedging the PR un-approvable.
test('a rollup with only followUps is not actionable and APPROVEs on full', () => {
  assert.equal(isActionable(FOLLOWUPS_ONLY), false)
  assert.equal(verdictEvent(FOLLOWUPS_ONLY, 'full'), 'APPROVE')
  assert.equal(verdictEvent(FOLLOWUPS_ONLY, 'incremental'), null)
})

// Tripwire: an actionable rollup owes REQUEST_CHANGES in BOTH review modes —
// an actionable delta is still actionable, so the mode never softens it.
test('an actionable rollup selects REQUEST_CHANGES in both modes', () => {
  for (const rollup of [CONFIRMED, SOFT_HOLD, HIGH_SPEC]) {
    assert.equal(verdictEvent(rollup, 'full'), 'REQUEST_CHANGES')
    assert.equal(verdictEvent(rollup, 'incremental'), 'REQUEST_CHANGES')
  }
})

// Tripwire: a clean pass APPROVEs only on a FULL review; a clean INCREMENTAL
// delta owes NO verdict, so barista never supersedes its own standing
// REQUEST_CHANGES off a delta that re-checked only the newly-changed lines.
// This is the exact refutation the judge raised and the owner endorsed.
test('a clean rollup APPROVEs on full but submits no verdict on incremental', () => {
  assert.equal(verdictEvent(CLEAN, 'full'), 'APPROVE')
  assert.equal(verdictEvent(CLEAN, 'incremental'), null)
})

// Tripwire: the dedup guard keeps the review timeline clean under the
// per-green-push re-trigger. It must skip a same-event same-SHA re-run with no
// new content, fire on a clean<->actionable transition, fire when barista has
// no standing verdict, and — the case the judge flagged — never submit for a
// clean incremental pass (owedEvent null), leaving a standing REQUEST_CHANGES
// in place.
test('shouldSubmitVerdict dedups a standing verdict but fires on a transition', () => {
  // Same event already standing, nothing new → skip.
  assert.equal(
    shouldSubmitVerdict({ owedEvent: 'REQUEST_CHANGES', latestBarista: 'REQUEST_CHANGES', hasNewContent: false }),
    false,
  )
  // Same event standing, but fresh annotations this run → submit.
  assert.equal(
    shouldSubmitVerdict({ owedEvent: 'REQUEST_CHANGES', latestBarista: 'REQUEST_CHANGES', hasNewContent: true }),
    true,
  )
  // actionable → clean-full transition (standing REQUEST_CHANGES, now owe APPROVE) → submit.
  assert.equal(
    shouldSubmitVerdict({ owedEvent: 'APPROVE', latestBarista: 'REQUEST_CHANGES', hasNewContent: false }),
    true,
  )
  // No standing verdict on this SHA yet → submit.
  assert.equal(
    shouldSubmitVerdict({ owedEvent: 'APPROVE', latestBarista: null, hasNewContent: false }),
    true,
  )
  // Clean incremental (no verdict owed) → never submit, whatever is standing.
  assert.equal(
    shouldSubmitVerdict({ owedEvent: null, latestBarista: 'REQUEST_CHANGES', hasNewContent: true }),
    false,
  )
  assert.equal(
    shouldSubmitVerdict({ owedEvent: null, latestBarista: null, hasNewContent: false }),
    false,
  )
})

const SOFT_HOLD_FINDING = {
  pillar: 'spec-fidelity',
  category: 'scope-leakage',
  file: 'scripts/post-review-rollup.mjs',
  line: 233,
  symbol: 'buildReviewBody',
  severity: 'high',
  suggested_form: 'render the soft-hold body instead of a bare count',
}

// Fix for #3249: a rollup whose only finding is a soft-hold must not collapse
// to a bare count — the reader needs the same suggested_form + anchor a
// folded finding gets, since soft-holds are the high-severity, land-gating
// class.
test('buildReviewBody renders a soft-hold finding, not just a count', () => {
  const body = buildReviewBody([], [{ f: SOFT_HOLD_FINDING, fp: 'fp-soft-hold' }], 1, 'REQUEST_CHANGES')
  assert.match(body, /\*\*1 soft-hold\*\* finding\(s\) — clear before un-draft\./)
  assert.match(body, /render the soft-hold body instead of a bare count/)
  assert.match(body, /`scripts\/post-review-rollup\.mjs:233`/)
})

// A soft-hold that duplicates a confirmed/spec finding is the caller's
// (main()'s) fingerprint-dedup responsibility — it excludes the duplicate
// from `softHoldFolded` before calling buildReviewBody. This asserts the
// render side of that contract: given the deduped input, the shared finding
// text appears exactly once, never doubled between the folded and soft-hold
// sections.
test('buildReviewBody renders a soft-hold deduped against a confirmed finding exactly once', () => {
  const shared = { pillar: 'correctness', file: 'a.rs', line: 3, suggested_form: 'fix the off-by-one' }
  const body = buildReviewBody([{ f: shared, fp: 'a.rs|3|correctness' }], [], 1, 'REQUEST_CHANGES')
  const occurrences = body.split('fix the off-by-one').length - 1
  assert.equal(occurrences, 1)
})

test('buildReviewBody renders no soft-hold section for a clean rollup', () => {
  const body = buildReviewBody([], [], 0, 'APPROVE')
  assert.doesNotMatch(body, /soft-hold/)
  assert.match(body, /No confirmed findings — the change is clean under all five pillars\./)
})

// Tripwire: fingerprint's path half must canonicalize to repo-relative
// regardless of whether it's fed an absolute CI-runner path or an
// already-relative one — otherwise the same file fingerprints two ways and
// cross-run dedup silently breaks (the bug this issue fixes). The parse-side
// normalizer must map a legacy absolute marker onto that same canonical key
// so already-posted findings keep deduping after the change.
test('fingerprint canonicalizes both path forms to one key, and normalizeFingerprint matches legacy markers', () => {
  const prior = process.env.GITHUB_WORKSPACE
  process.env.GITHUB_WORKSPACE = '/home/runner/_work/aether/aether'
  try {
    const absolute = fingerprint('/home/runner/_work/aether/aether/crates/x.rs', 3, 'correctness')
    const relative = fingerprint('crates/x.rs', 3, 'correctness')
    assert.equal(absolute, relative)

    const legacyMarker = '/home/runner/_work/aether/aether/crates/x.rs|3|correctness'
    assert.equal(normalizeFingerprint(legacyMarker), relative)
  } finally {
    if (prior === undefined) delete process.env.GITHUB_WORKSPACE
    else process.env.GITHUB_WORKSPACE = prior
  }
})
