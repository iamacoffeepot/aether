import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  isActionable,
  markerVerdict,
  readRollup,
  renderComment,
  renderNoRollupComment,
} from './post-dogfood-rollup.mjs'

test('an unset or unreadable rollup path selects no-rollup mode', async () => {
  assert.equal(await readRollup(''), null)
  const directory = await mkdtemp(join(tmpdir(), 'aether-no-rollup-'))
  try {
    assert.equal(await readRollup(join(directory, 'missing.json')), null)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test('no-rollup mode records an actionable failed attempt', () => {
  const comment = renderNoRollupComment({
    attempt: 2,
    runUrl: 'https://github.com/iamacoffeepot/aether/actions/runs/12345',
  })

  assert.match(
    comment,
    /<!-- aether-dogfood attempts=2 verdict=failed -->/,
  )
  assert.match(
    comment,
    /\[Open the Actions run\]\(https:\/\/github\.com\/iamacoffeepot\/aether\/actions\/runs\/12345\)/,
  )
  assert.doesNotMatch(comment, /evidence viewer|frame\.png/)
})

test('a clean rollup remains green and non-actionable', () => {
  const rollup = {
    succeeded: true,
    buildGreen: true,
    summary: 'The consumer completed the task.',
    artifact: { verdict: 'correct', rationale: 'The frame matches the expected artifact.' },
    friction: { blocker: [], 'missing-primitive': [], papercut: [], 'doc-gap': [] },
    softHolds: [],
  }

  assert.equal(isActionable(rollup), false)
  assert.equal(markerVerdict(rollup), 'green')
  assert.match(
    renderComment(rollup, false, { attempt: 3, runRef: '3088/12345-3' }),
    /<!-- aether-dogfood attempts=3 verdict=green -->/,
  )
})

// The trial that motivated task + provenance capture (run 29155032950): the judge
// ruled `wrong` against a rubric describing a three-frame comparison it could never
// have made from one capture, and the posted comment showed neither the task nor the
// rubric — so the verdict could not be checked by the human reading it. A comment
// that carries a verdict must carry the question that produced it.
const GROUNDED_ROLLUP = {
  succeeded: true,
  buildGreen: null,
  summary: 'Drove the proposal lifecycle over MCP.',
  artifact: { verdict: 'wrong', rationale: 'No terrain surface in the frame.' },
  friction: { blocker: [], 'missing-primitive': [], papercut: [], 'doc-gap': [] },
  softHolds: [],
  task: {
    medium: 'drive',
    prompt: 'Load `aether.kit.world` and stage a cross-chunk proposal.',
    surfaceUnderTest: '`aether.kit.world.{propose,commit_proposal}`',
    expectedArtifact: 'a preview frame that visibly differs from the committed baseline',
  },
  provenance: {
    driver: { model: 'claude-sonnet-5' },
    phases: {
      author: { model: 'sonnet', effort: 'medium', agentType: null, prompt: null },
      attempt: { model: 'opus', effort: 'high', agentType: 'general-purpose', prompt: 'You are a FRESH consumer.' },
      judge: { model: 'opus', effort: 'high', agentType: 'general-purpose', prompt: 'You are a vision judge.' },
    },
  },
}

test('a rollup carrying a task renders the prompt and the rubric the judge graded against', () => {
  const comment = renderComment(GROUNDED_ROLLUP, true, { attempt: 1, runRef: '3088/12345-1' })

  assert.match(comment, /### The task/)
  assert.match(comment, /\*\*Medium:\*\* drive/)
  assert.match(comment, /> Load `aether\.kit\.world` and stage a cross-chunk proposal\./)
  // The pass bar must be visible next to the verdict it produced, and labelled as
  // something the consumer was never shown — that asymmetry is the bug it exposes.
  assert.match(comment, /\*\*Expected artifact\*\*.*the consumer is not shown it/)
  assert.match(comment, /> a preview frame that visibly differs from the committed baseline/)
})

test('provenance reports every phase model and effort, and flags the phase that did not run', () => {
  const comment = renderComment(GROUNDED_ROLLUP, false, { attempt: 1, runRef: '3088/12345-1' })

  assert.match(comment, /### Provenance/)
  assert.match(comment, /\| driver \(harness session\) \| claude-sonnet-5 \|/)
  assert.match(comment, /\| attempt \| opus \| high \|/)
  assert.match(comment, /\| judge \| opus \| high \|/)
  // Author is skipped whenever CI supplies the task, which is every unattended run.
  // Reporting its model without saying it never ran would misdescribe the trial.
  assert.match(comment, /\| author _\(did not run\)_ \| sonnet \| medium \|/)
  assert.match(comment, /<details><summary>The <code>attempt<\/code> prompt, as sent<\/summary>/)
  assert.doesNotMatch(comment, /<summary>The <code>author<\/code> prompt/)
})

test('a fenced prompt cannot break out of the collapsible that contains it', () => {
  const rollup = {
    ...GROUNDED_ROLLUP,
    provenance: {
      driver: { model: 'claude-sonnet-5' },
      phases: {
        attempt: {
          model: 'opus',
          effort: 'high',
          agentType: 'general-purpose',
          prompt: 'Run this:\n```bash\necho hi\n```\nthen stop.',
        },
      },
    },
  }

  const comment = renderComment(rollup, false, { attempt: 1, runRef: '3088/12345-1' })
  const details = comment.slice(comment.indexOf('<details>'))
  // Exactly the one opening fence and the one closing fence this renderer emits.
  assert.equal(details.match(/```/g).length, 2)
  assert.match(details, /'''bash/)
})

test('a rollup predating task and provenance capture still renders', () => {
  const legacy = {
    succeeded: true,
    buildGreen: null,
    summary: 'An older trial.',
    artifact: null,
    friction: { blocker: [], 'missing-primitive': [], papercut: [], 'doc-gap': [] },
    softHolds: [],
  }

  const comment = renderComment(legacy, false, { attempt: 1, runRef: '3088/12345-1' })
  assert.doesNotMatch(comment, /### The task|### Provenance/)
  assert.match(comment, /## Dogfood trial/)
})
