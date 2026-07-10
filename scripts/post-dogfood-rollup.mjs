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
// Inputs (env):
//   GITHUB_TOKEN        least-privilege token (issues write)
//   GITHUB_REPOSITORY   owner/repo
//   ISSUE               the feature issue to comment on (required)
//   PR                  the PR to carry the label (optional; defaults to ISSUE)
//   RUN_REF             <issue>/<run-id> — the evidence-branch path segment
//   ROLLUP_PATH         rollup.json (the workflow's returned rollup)
//   HAS_FRAME           "1"/"true" when the staged run dir held a frame.png
//                       (falls back to rollup.artifact != null when unset)
//
// No external deps — Node built-ins + global fetch only.

import { readFile } from 'node:fs/promises'

const TOKEN = requireEnv('GITHUB_TOKEN')
const REPO = requireEnv('GITHUB_REPOSITORY')
const ISSUE = Number(requireEnv('ISSUE'))
const PR = process.env.PR ? Number(process.env.PR) : ISSUE
const RUN_REF = requireEnv('RUN_REF')
const ROLLUP_PATH = process.env.ROLLUP_PATH || 'rollup.json'

const MARKER = '<!-- aether-dogfood -->'
const LABEL = 'dogfood:unresolved'
const API = 'https://api.github.com'
const RAW_BASE = 'https://raw.githubusercontent.com/iamacoffeepot/aether/dogfood/evidence/evidence'
const VIEWER_BASE = 'https://iamacoffeepot.github.io/aether/evidence/'

function requireEnv(name) {
  const v = process.env[name]
  if (!v) throw new Error(`post-dogfood-rollup: ${name} is required`)
  return v
}

async function api(method, path, body) {
  const res = await fetch(`${API}/${path}`, {
    method,
    headers: {
      authorization: `Bearer ${TOKEN}`,
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
async function apiList(path) {
  const out = []
  let url = `${API}/${path}${path.includes('?') ? '&' : '?'}per_page=100`
  while (url) {
    const res = await fetch(url, {
      headers: {
        authorization: `Bearer ${TOKEN}`,
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

function renderComment(rollup, hasFrame) {
  const lines = [MARKER, '## Dogfood trial', '']
  lines.push(...verdictLines(rollup))
  lines.push('', '### Friction')
  lines.push(...frictionLines(rollup.friction))
  lines.push(...softHoldLines(rollup.softHolds))
  if (hasFrame) {
    lines.push('', '### Judged frame', '', `![judged frame](${RAW_BASE}/${RUN_REF}/frame.png)`)
  }
  lines.push('', `[Full run in the evidence viewer](${VIEWER_BASE}?run=${encodeURIComponent(RUN_REF)})`)
  return lines.join('\n')
}

function isActionable(rollup) {
  if (rollup.succeeded === false) return true
  if (rollup.artifact && rollup.artifact.verdict === 'wrong') return true
  if (Array.isArray(rollup.softHolds) && rollup.softHolds.length) return true
  const friction = rollup.friction || {}
  for (const category of ['blocker', 'missing-primitive']) {
    if (Array.isArray(friction[category]) && friction[category].length) return true
  }
  return false
}

// Create the label if it does not exist yet. An already-exists 422 is a
// clean success — the label is present, which is all we need.
async function ensureLabel() {
  const res = await api('POST', `repos/${REPO}/labels`, {
    name: LABEL,
    color: 'd93f0b',
    description: 'A dogfood trial surfaced something actionable',
  })
  if (res.ok || res.status === 422) return
  console.error(`ensure label failed: ${res.status} ${res.text}`)
}

async function main() {
  const raw = JSON.parse(await readFile(ROLLUP_PATH, 'utf8'))
  // Accept either the bare rollup or the workflow's full { rollup, task }.
  const rollup = raw && raw.rollup ? raw.rollup : raw

  const hasFrame = process.env.HAS_FRAME
    ? /^(1|true)$/i.test(process.env.HAS_FRAME)
    : rollup.artifact != null

  const body = renderComment(rollup, hasFrame)

  // Upsert the marker-anchored living comment — edit if present, create
  // otherwise. The evidence branch keeps per-run history, so the comment
  // shows latest-state.
  const issueComments = await apiList(`repos/${REPO}/issues/${ISSUE}/comments`)
  const existing = issueComments.find((c) => String(c.body || '').includes(MARKER))
  if (existing) {
    const res = await api('PATCH', `repos/${REPO}/issues/comments/${existing.id}`, { body })
    if (!res.ok) console.error(`update comment failed: ${res.status} ${res.text}`)
    else console.log('updated dogfood comment')
  } else {
    const res = await api('POST', `repos/${REPO}/issues/${ISSUE}/comments`, { body })
    if (!res.ok) console.error(`post comment failed: ${res.status} ${res.text}`)
    else console.log('posted dogfood comment')
  }

  // Advisory label on the PR when passed (that is where /land reads),
  // else on the issue. Set when actionable, cleared clean.
  const target = PR
  if (isActionable(rollup)) {
    await ensureLabel()
    const res = await api('POST', `repos/${REPO}/issues/${target}/labels`, { labels: [LABEL] })
    if (!res.ok) console.error(`add label failed: ${res.status} ${res.text}`)
    else console.log(`set ${LABEL} on #${target}`)
  } else {
    const res = await api('DELETE', `repos/${REPO}/issues/${target}/labels/${encodeURIComponent(LABEL)}`)
    // 404 = the label was not present; a clean no-op.
    if (res.ok || res.status === 404) console.log(`cleared ${LABEL} on #${target}`)
    else console.error(`remove label failed: ${res.status} ${res.text}`)
  }
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})
