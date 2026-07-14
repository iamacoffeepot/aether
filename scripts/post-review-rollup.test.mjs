import assert from 'node:assert/strict'
import test from 'node:test'

import {
  bounceSignal,
  buildReviewBody,
  disclosedPaths,
  fingerprint,
  isActionable,
  normalizeFingerprint,
  verdictEvent,
  visibleSoftHolds,
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
test('a rollup with only followUps is not actionable and APPROVEs', () => {
  assert.equal(isActionable(FOLLOWUPS_ONLY), false)
  assert.equal(verdictEvent(FOLLOWUPS_ONLY), 'APPROVE')
})

// Tripwire: verdictEvent is a total two-outcome function — every request
// yields a verdict, REQUEST_CHANGES when actionable and APPROVE when clean.
// If a third outcome (a null / no-submission case) ever crept back in, a
// findings fix-push would dismiss the stale verdict, the re-requested review
// would submit nothing, and the PR would wedge at REVIEW_REQUIRED — the #3208
// deadlock this model retires.
test('verdictEvent always yields a verdict: REQUEST_CHANGES when actionable, APPROVE when clean', () => {
  for (const rollup of [CONFIRMED, SOFT_HOLD, HIGH_SPEC]) {
    assert.equal(verdictEvent(rollup), 'REQUEST_CHANGES')
  }
  assert.equal(verdictEvent(CLEAN), 'APPROVE')
  assert.equal(verdictEvent(LOW_SPEC), 'APPROVE')
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

// Tripwire for #3255: a scope-leakage finding whose file a prior barista
// correctness fingerprint already named on this same PR is evidence-based
// disclosure, not a prose self-waiver — it must drop from both gate arms
// (isActionable's high-severity spec check, verdictEvent) AND from the
// rendered soft-hold banner count (visibleSoftHolds), which mirrors
// review.js's specSoftHolds basename-only `file`.
test('a disclosed scope-leakage finding drops from isActionable, verdictEvent, and visibleSoftHolds', () => {
  const disclosed = new Set(['scripts/other.mjs'])
  const rollup = {
    confirmed: [],
    softHolds: [],
    spec: {
      findings: [
        { severity: 'high', category: 'scope-leakage', file: 'scripts/other.mjs', description: 'touched unasked file' },
      ],
    },
  }
  assert.equal(isActionable(rollup, disclosed), false)
  assert.notEqual(verdictEvent(rollup, disclosed), 'REQUEST_CHANGES')

  // review.js's specSoftHolds mirror base-names `file`, so the softHolds-shaped
  // entry carries only the basename while `disclosed` holds the full repo-relative
  // path — visibleSoftHolds must still match it (the suffix tolerance).
  const softHoldRollup = {
    softHolds: [{ pillar: 'spec-fidelity', category: 'scope-leakage', file: 'other.mjs', severity: 'high' }],
  }
  assert.equal(visibleSoftHolds(softHoldRollup, disclosed).length, 0)
})

// Tripwire for #3255 / the owner's #3250 bounce, requirement 3: this must be
// the ABSOLUTE-form case, not two repo-relative paths — a test with only
// repo-relative paths would pass today even if disclosedPaths never
// normalized, which is exactly the silent no-op (green tests, green CI,
// catch-22 still wedging PRs) the bounce called out.
test('disclosedPaths normalizes an absolute-form correctness fingerprint, and it still drops a repo-relative scope-leakage finding', () => {
  const prior = process.env.GITHUB_WORKSPACE
  process.env.GITHUB_WORKSPACE = '/home/runner/_work/aether/aether'
  try {
    const comments = [
      { body: 'aether-review-fp:/home/runner/_work/aether/aether/scripts/other.mjs|12|correctness' },
    ]
    const disclosed = disclosedPaths(comments)
    assert.deepEqual([...disclosed], ['scripts/other.mjs'])

    const rollup = {
      confirmed: [],
      softHolds: [],
      spec: {
        findings: [
          { severity: 'high', category: 'scope-leakage', file: 'scripts/other.mjs', description: 'touched unasked file' },
        ],
      },
    }
    assert.equal(isActionable(rollup, disclosed), false)
  } finally {
    if (prior === undefined) delete process.env.GITHUB_WORKSPACE
    else process.env.GITHUB_WORKSPACE = prior
  }
})

// Tripwire: the carve-out is evidence-gated, not blanket — a scope-leakage
// finding with no matching entry in `disclosed` still gates, both against an
// unrelated-path disclosed set and against the default empty set (so every
// pre-existing isActionable/verdictEvent call site keeps working unchanged).
test('a scope-leakage finding with no matching disclosed entry still gates', () => {
  const rollup = {
    confirmed: [],
    softHolds: [],
    spec: {
      findings: [
        { severity: 'high', category: 'scope-leakage', file: 'scripts/other.mjs', description: 'touched unasked file' },
      ],
    },
  }
  assert.equal(isActionable(rollup, new Set(['scripts/unrelated.mjs'])), true)
  assert.equal(isActionable(rollup), true)
  assert.equal(verdictEvent(rollup), 'REQUEST_CHANGES')
})

// Confirm pass (issue #3390): the three terminal outcomes of a re-review, all inside
// ADR-0148's two native verdicts. review.js's runConfirmPass re-asserts still-open prior
// findings into `confirmed` (unchanged, so their fingerprints match across rounds) and rides
// the restart flag on `rollup.restart`. These pin the verdict the poster derives from each.

// All prior findings addressed, no restart -> a clean confirm APPROVEs: the convergent
// end of the address->confirm loop, not another deep re-roll.
const CONFIRM_CLEAN = {
  reviewPass: 'confirm', restart: { signaled: false, rationale: 'ordinary fix round' },
  confirmed: [], softHolds: [], spec: null, followUps: [],
}
test('a clean confirm pass (all addressed, no restart) is not actionable and APPROVEs', () => {
  assert.equal(isActionable(CONFIRM_CLEAN), false)
  assert.equal(verdictEvent(CONFIRM_CLEAN), 'APPROVE')
})

// A still-open prior finding re-asserts the standing REQUEST_CHANGES through the confirmed
// arm — no new finding, no flip, the same fingerprints the deep pass posted.
const CONFIRM_UNADDRESSED = {
  reviewPass: 'confirm', restart: { signaled: false, rationale: '' },
  confirmed: [{ pillar: 'correctness', file: 'a.rs', line: 3, source: 'confirm', gate: 'advisory' }],
  softHolds: [], spec: null, followUps: [],
}
test('a confirm pass with a still-open prior finding re-asserts REQUEST_CHANGES', () => {
  assert.equal(isActionable(CONFIRM_UNADDRESSED), true)
  assert.equal(verdictEvent(CONFIRM_UNADDRESSED), 'REQUEST_CHANGES')
})

// Tripwire: the restart signal is actionable ON ITS OWN — every prior finding addressed
// (empty confirmed) but the delta warrants a full redo. Without the restart arm in
// isActionable this would APPROVE, silently landing a rework the reviewer flagged as
// restart-level; the poster's ask-and-park hangs off this same signal.
const CONFIRM_RESTART = {
  reviewPass: 'confirm', restart: { signaled: true, rationale: 'delta is a redesign' },
  confirmed: [], softHolds: [], spec: null, followUps: [],
}
test('a confirm-pass restart signal is actionable and REQUEST_CHANGES even with no open findings', () => {
  assert.equal(isActionable(CONFIRM_RESTART), true)
  assert.equal(verdictEvent(CONFIRM_RESTART), 'REQUEST_CHANGES')
})

// Reviewer bounce (issue #3391): the third TERMINAL outcome, carried out-of-band as rollup.bounce =
// { to, reason } (review.js normalizes both the deep pass's fundamental-problem conclusion and the confirm
// pass's restart escalation into it). bounceSignal is the pure reader the poster's bounce path and the
// reconciler edge both key on.
const DEEP_BOUNCE = {
  confirmed: [], softHolds: [], spec: { findings: [] },
  bounce: { to: 'design', reason: 'the change is architecturally wrong at the root' },
}
const SCOPE_BOUNCE = {
  confirmed: [], softHolds: [], spec: { findings: [] },
  bounce: { to: 'plan', reason: 'the issue was scoped incorrectly' },
}

test('bounceSignal returns {to, reason} when a bounce is present, null when absent', () => {
  assert.deepEqual(bounceSignal(DEEP_BOUNCE), { to: 'design', reason: 'the change is architecturally wrong at the root' })
  assert.deepEqual(bounceSignal(SCOPE_BOUNCE), { to: 'plan', reason: 'the issue was scoped incorrectly' })
  assert.equal(bounceSignal(CLEAN), null)
  assert.equal(bounceSignal(CONFIRM_RESTART), null) // restart alone is not a bounce field
  assert.equal(bounceSignal({ bounce: { reason: 'no phase' } }), null) // a bounce with no `to` is not a signal
})

// A bounce keeps the PR merge-blocked (issue #3391): barista still owes REQUEST_CHANGES, even with no
// confirmed finding, soft-hold, or spec finding to gate on — the bounce arm in isActionable carries it.
test('a bounce with no other finding is still actionable and REQUEST_CHANGES', () => {
  assert.equal(isActionable(DEEP_BOUNCE), true)
  assert.equal(verdictEvent(DEEP_BOUNCE), 'REQUEST_CHANGES')
})

// Tripwire: verdictEvent stays the TOTAL two-outcome function ADR-0148 pins even when a bounce is present —
// bounce is a third terminal outcome carried out-of-band (the PR labels), NOT a third verdict value. If
// verdictEvent ever returned a bounce-shaped third value, ADR-0148's native merge gate (which reads only
// APPROVE / REQUEST_CHANGES) would stop recognizing the verdict and the PR would wedge.
test('verdictEvent never yields a third value even when a bounce is present', () => {
  assert.equal(verdictEvent(DEEP_BOUNCE), 'REQUEST_CHANGES')
  assert.equal(verdictEvent(SCOPE_BOUNCE), 'REQUEST_CHANGES')
})
