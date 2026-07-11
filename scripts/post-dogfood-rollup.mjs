#!/usr/bin/env node
// Post a dogfood rollup onto its feature issue as one living, marker-
// anchored comment, and set or clear the advisory `dogfood:unresolved`
// label (issue 2967). Mirrors scripts/post-review-rollup.mjs — same env
// contract, fetch wrapper, marker-anchored upsert, and label set/clear.
//
// The posture matches the review poster: the comment and label are
// advisory. The teeth are the /land gate that reads `dogfood:unresolved`
// (a separate skill-text change), not a CI failure here. A failed API
// call logs and continues where the review poster does.
//
// The marker line carries the retry state the dogfood action's resolve
// step reads back on the next green event (issue 2968): it is extended
// from the bare `<!-- aether-dogfood -->` to
// `<!-- aether-dogfood attempts=<N> verdict=<green|failed> -->`, where
// `attempts` is this comment's attempt number (the ATTEMPT env, defaulting
// to 1) and `verdict` is `green` when the trial is clean and `failed` when
// the actionability predicate fires or the attempt did not succeed. A
// `green` verdict is terminal (no more auto-runs); `attempts >= the cap`
// is terminal-failed. The upsert lookup still matches on the bare
// `<!-- aether-dogfood` prefix, so an old bare-marker comment migrates to
// the extended form in place on the first post that carries the state.
//
// Inputs (env):
//   GITHUB_TOKEN        least-privilege token (issues write)
//   GITHUB_REPOSITORY   owner/repo
//   ISSUE               the feature issue to comment on (required)
//   PR                  the PR to carry the label (optional; defaults to ISSUE)
//   RUN_REF             <issue>/<run-id> — the evidence-branch path segment
//   RUN_URL             Actions URL for a no-rollup attempt's transcript
//   ROLLUP_PATH         rollup.json (the workflow's returned rollup); unset or
//                       unreadable records a no-rollup attempt
//   ATTEMPT             this trial's attempt number, baked into the marker
//                       (optional; defaults to 1)
//   HAS_FRAME           "1"/"true" when the staged run dir held a judged-frame.png
//                       (falls back to rollup.artifact != null when unset)
//
// No external deps — Node built-ins + global fetch only.

import { readFile } from 'node:fs/promises'
import { pathToFileURL } from 'node:url'

// The bare prefix is the upsert anchor — it matches both the old
// `<!-- aether-dogfood -->` comment and the extended
// `<!-- aether-dogfood attempts=… verdict=… -->` one, so a format change
// migrates in place rather than stacking a second comment.
const MARKER_PREFIX = '<!-- aether-dogfood'
const LABEL = 'dogfood:unresolved'
const API = 'https://api.github.com'
const RAW_BASE = 'https://raw.githubusercontent.com/iamacoffeepot/aether/dogfood/evidence/evidence'
const VIEWER_BASE = 'https://iamacoffeepot.github.io/aether/evidence/'

function requireEnv(name) {
  const v = process.env[name]
  if (!v) throw new Error(`post-dogfood-rollup: ${name} is required`)
  return v
}

function environment() {
  const issue = Number(requireEnv('ISSUE'))
  return {
    token: requireEnv('GITHUB_TOKEN'),
    repo: requireEnv('GITHUB_REPOSITORY'),
    issue,
    pr: process.env.PR ? Number(process.env.PR) : issue,
    runRef: process.env.RUN_REF || '',
    runUrl: process.env.RUN_URL || '',
    rollupPath: process.env.ROLLUP_PATH || '',
    attempt: process.env.ATTEMPT ? Number(process.env.ATTEMPT) : 1,
  }
}

async function api(token, method, path, body) {
  const res = await fetch(`${API}/${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      accept: 'application/vnd.github+json',
      'x-github-api-version': '2022-11-28',
      'content-type': 'application/json',
      'user-agent': 'aether-dogfood-poster',
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  const text = await res.text()
  const data = text ? safeJson(text) : null
  return { ok: res.ok, status: res.status, data, text }
}

function safeJson(t) {
  try { return JSON.parse(t) } catch { return null }
}

// Paginate a GitHub list endpoint via the Link header.
async function apiList(token, path) {
  const out = []
  let url = `${API}/${path}${path.includes('?') ? '&' : '?'}per_page=100`
  while (url) {
    const res = await fetch(url, {
      headers: {
        authorization: `Bearer ${token}`,
        accept: 'application/vnd.github+json',
        'x-github-api-version': '2022-11-28',
        'user-agent': 'aether-dogfood-poster',
      },
    })
    if (!res.ok) throw new Error(`GET ${url} -> ${res.status} ${await res.text()}`)
    const page = await res.json()
    if (Array.isArray(page)) out.push(...page)
    url = nextLink(res.headers.get('link'))
  }
  return out
}

function nextLink(link) {
  if (!link) return null
  for (const part of link.split(',')) {
    const m = part.match(/<([^>]+)>;\s*rel="next"/)
    if (m) return m[1]
  }
  return null
}

function verdictLines(rollup) {
  const lines = []
  const succeeded = rollup.succeeded === true
  lines.push(`- **Attempt:** ${succeeded ? 'succeeded' : 'did not succeed'}`)
  if (rollup.buildGreen !== null && rollup.buildGreen !== undefined) {
    lines.push(`- **Build:** ${rollup.buildGreen ? 'green' : 'red'}`)
  }
  if (rollup.artifact) {
    lines.push(`- **Artifact:** ${rollup.artifact.verdict} — ${rollup.artifact.rationale}`)
  }
  if (rollup.summary) {
    lines.push('', rollup.summary)
  }
  return lines
}

// A verdict is only readable against the question that produced it. `wrong` on a
// frame says nothing until you can see the task the consumer was handed and the
// artifact the judge was told to expect — those two disagreeing is itself the
// most common way a trial goes bad. So they lead, ahead of the friction table.
function taskLines(task) {
  if (!task) return []
  const lines = ['', '### The task', '']
  lines.push(`- **Medium:** ${cell(task.medium)}`)
  lines.push(`- **Surface under test:** ${cell(task.surfaceUnderTest)}`)
  if (task.prompt) lines.push('', '**Handed verbatim to the consumer:**', '', blockquote(task.prompt))
  lines.push(
    '',
    '**Expected artifact** — the judge grades against this; the consumer is not shown it:',
    '',
    task.expectedArtifact ? blockquote(task.expectedArtifact) : '> _(none — nothing renders, so no artifact was judged)_',
  )
  return lines
}

// Prompts are long and only wanted when a verdict is being contested, so they
// collapse. The clip is the GitHub comment budget, not the evidence budget — the
// rollup on the evidence branch keeps the fuller text, and the viewer shows it.
const COMMENT_PROMPT_MAX = 4000

function provenanceLines(provenance) {
  if (!provenance) return []
  const phases = provenance.phases || {}
  const lines = ['', '### Provenance', '', '| phase | model | effort |', '| --- | --- | --- |']
  const driver = (provenance.driver && provenance.driver.model) || null
  lines.push(`| driver (harness session) | ${cell(driver)} | ${cell(null)} |`)
  for (const name of ['author', 'attempt', 'judge']) {
    const p = phases[name]
    if (!p) continue
    const ran = p.prompt ? '' : ' _(did not run)_'
    lines.push(`| ${name}${ran} | ${cell(p.model)} | ${cell(p.effort)} |`)
  }
  for (const name of ['author', 'attempt', 'judge']) {
    const p = phases[name]
    if (!p || !p.prompt) continue
    const text = p.prompt.length > COMMENT_PROMPT_MAX
      ? `${p.prompt.slice(0, COMMENT_PROMPT_MAX)}\n\n[…clipped — full prompt in the evidence viewer]`
      : p.prompt
    lines.push(
      '',
      `<details><summary>The <code>${name}</code> prompt, as sent</summary>`,
      '',
      '```',
      // A fence inside the prompt would otherwise close this one early.
      text.replace(/```/g, "'''"),
      '```',
      '',
      '</details>',
    )
  }
  return lines
}

function blockquote(text) {
  return String(text).split(/\r?\n/).map((l) => `> ${l}`).join('\n')
}

const CATEGORIES = ['blocker', 'missing-primitive', 'papercut', 'doc-gap']

function frictionLines(friction) {
  const lines = []
  let any = false
  for (const category of CATEGORIES) {
    const items = Array.isArray(friction && friction[category]) ? friction[category] : []
    if (!items.length) continue
    any = true
    lines.push('', `#### ${category}`, '', '| severity | where | what | suggested |', '| --- | --- | --- | --- |')
    for (const f of items) {
      lines.push(`| ${cell(f.severity)} | ${cell(f.where)} | ${cell(f.what)} | ${cell(f.suggested)} |`)
    }
  }
  if (!any) lines.push('', '_No friction logged — the surface was friction-free._')
  return lines
}

function tokenTableLines(tokensPerTool) {
  if (!Array.isArray(tokensPerTool)) return []

  const lines = ['', '### MCP tool tokens']
  if (!tokensPerTool.length) {
    lines.push('', '_No MCP tool traffic recorded._')
    return lines
  }

  const rows = [...tokensPerTool].sort((left, right) => {
    const tokenDifference = Number(right.tokensIn || 0) + Number(right.tokensOut || 0)
      - Number(left.tokensIn || 0) - Number(left.tokensOut || 0)
    return tokenDifference || String(left.tool || '').localeCompare(String(right.tool || ''))
  })
  lines.push('', '| tool | calls | tokens in | tokens out |', '| --- | ---: | ---: | ---: |')
  for (const row of rows) {
    lines.push(`| ${cell(row.tool)} | ${cell(row.calls)} | ${cell(row.tokensIn)} | ${cell(row.tokensOut)} |`)
  }
  return lines
}

// Escape pipes and collapse newlines so a finding never breaks the table.
function cell(v) {
  return String(v ?? '').replace(/\|/g, '\\|').replace(/\r?\n/g, ' ').trim() || '—'
}

function softHoldLines(softHolds) {
  if (!Array.isArray(softHolds) || !softHolds.length) return []
  const lines = ['', '### Soft-holds', '', '_Advisory: a soft-hold flags something a reviewer should clear; it never gates the land._', '']
  for (const h of softHolds) {
    const where = h.where ? ` (${h.where})` : ''
    lines.push(`- **${h.kind}**${where} — ${h.detail || ''}`)
  }
  return lines
}

// The verdict baked into the marker mirrors the label decision: a trial
// with anything actionable (a failed attempt, a wrong artifact, a
// soft-hold, a blocker / missing-primitive finding) is `failed`, else
// `green`. `green` is terminal for the auto-retry loop.
export function markerVerdict(rollup) {
  return isActionable(rollup) || rollup.succeeded === false ? 'failed' : 'green'
}

export function markerLine(rollup, attempt = 1, verdict = markerVerdict(rollup)) {
  return `${MARKER_PREFIX} attempts=${attempt} verdict=${verdict} -->`
}

export function renderComment(rollup, hasFrame, { attempt = 1, runRef = '' } = {}) {
  const lines = [markerLine(rollup, attempt), '## Dogfood trial', '']
  lines.push(...verdictLines(rollup))
  lines.push(...taskLines(rollup.task))
  lines.push('', '### Friction')
  lines.push(...frictionLines(rollup.friction))
  lines.push(...softHoldLines(rollup.softHolds))
  lines.push(...tokenTableLines(rollup.tokensPerTool))
  if (hasFrame) {
    // The frame the JUDGE graded — not a later re-capture. The two used to differ.
    lines.push('', '### The frame the judge graded', '', `![the frame the judge graded](${RAW_BASE}/${runRef}/judged-frame.png)`)
  }
  lines.push(...provenanceLines(rollup.provenance))
  lines.push('', `[Full run in the evidence viewer](${VIEWER_BASE}?run=${encodeURIComponent(runRef)})`)
  return lines.join('\n')
}

export function renderNoRollupComment({ attempt = 1, runUrl } = {}) {
  return [
    markerLine(null, attempt, 'failed'),
    '## Dogfood trial',
    '',
    '- **Attempt:** did not succeed',
    '',
    `The trial ended without its required rollup. [Open the Actions run](${runUrl}) to inspect the \`dogfood-session-transcript\` artifact and runner log.`,
  ].join('\n')
}

export function isActionable(rollup) {
  if (rollup.succeeded === false) return true
  if (rollup.artifact && rollup.artifact.verdict === 'wrong') return true
  // A rubric the judge could not settle is not a pass. It raises no soft-hold — an
  // unjudgeable artifact is not a claim that the feature is broken — but it must not
  // read as green either, because nothing was actually verified. Actionable routes it
  // to a re-trial and, at the attempt cap, to a human.
  if (rollup.artifact && rollup.artifact.verdict === 'insufficient-evidence') return true
  if (Array.isArray(rollup.softHolds) && rollup.softHolds.length) return true
  const friction = rollup.friction || {}
  for (const category of ['blocker', 'missing-primitive']) {
    if (Array.isArray(friction[category]) && friction[category].length) return true
  }
  return false
}

export async function readRollup(path) {
  if (!path) return null
  try {
    return JSON.parse(await readFile(path, 'utf8'))
  } catch (error) {
    if (error instanceof SyntaxError) throw error
    return null
  }
}

// Create the label if it does not exist yet. An already-exists 422 is a
// clean success — the label is present, which is all we need.
async function ensureLabel({ token, repo }) {
  const res = await api(token, 'POST', `repos/${repo}/labels`, {
    name: LABEL,
    color: 'd93f0b',
    description: 'A dogfood trial surfaced something actionable',
  })
  if (res.ok || res.status === 422) return
  console.error(`ensure label failed: ${res.status} ${res.text}`)
}

async function main() {
  const env = environment()
  const raw = await readRollup(env.rollupPath)
  const noRollup = raw === null
  // Accept either the bare rollup or the workflow's full { rollup, task }.
  const rollup = raw && raw.rollup ? raw.rollup : raw
  const hasFrame = !noRollup && (process.env.HAS_FRAME
    ? /^(1|true)$/i.test(process.env.HAS_FRAME)
    : rollup.artifact != null)
  const body = noRollup
    ? renderNoRollupComment({ ...env, runUrl: requireEnv('RUN_URL') })
    : renderComment(rollup, hasFrame, { ...env, runRef: requireEnv('RUN_REF') })

  // Upsert the marker-anchored living comment — edit if present, create
  // otherwise. The evidence branch keeps per-run history, so the comment
  // shows latest-state.
  const issueComments = await apiList(env.token, `repos/${env.repo}/issues/${env.issue}/comments`)
  const existing = issueComments.find((c) => String(c.body || '').includes(MARKER_PREFIX))
  if (existing) {
    const res = await api(env.token, 'PATCH', `repos/${env.repo}/issues/comments/${existing.id}`, { body })
    if (!res.ok) console.error(`update comment failed: ${res.status} ${res.text}`)
    else console.log('updated dogfood comment')
  } else {
    const res = await api(env.token, 'POST', `repos/${env.repo}/issues/${env.issue}/comments`, { body })
    if (!res.ok) console.error(`post comment failed: ${res.status} ${res.text}`)
    else console.log('posted dogfood comment')
  }

  // Advisory label on the PR when passed (that is where /land reads),
  // else on the issue. Set when actionable, cleared clean.
  const target = env.pr
  if (noRollup || isActionable(rollup)) {
    await ensureLabel(env)
    const res = await api(env.token, 'POST', `repos/${env.repo}/issues/${target}/labels`, { labels: [LABEL] })
    if (!res.ok) console.error(`add label failed: ${res.status} ${res.text}`)
    else console.log(`set ${LABEL} on #${target}`)
  } else {
    const res = await api(env.token, 'DELETE', `repos/${env.repo}/issues/${target}/labels/${encodeURIComponent(LABEL)}`)
    // 404 = the label was not present; a clean no-op.
    if (res.ok || res.status === 404) console.log(`cleared ${LABEL} on #${target}`)
    else console.error(`remove label failed: ${res.status} ${res.text}`)
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((err) => {
    console.error(err)
    process.exit(1)
  })
}
