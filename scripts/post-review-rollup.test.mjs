import assert from 'node:assert/strict'
import test from 'node:test'

import {
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
