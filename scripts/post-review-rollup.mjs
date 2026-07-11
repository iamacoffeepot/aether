#!/usr/bin/env node
// Post a five-pillar review rollup onto its PR: inline annotations for
// findings that land on a changed diff line, everything else folded into
// one COMMENT review body, plus a marker-anchored summary comment and the
// advisory `review:unresolved` label.
//
// The posture is load-bearing: the review event is ALWAYS `COMMENT`, never
// `REQUEST_CHANGES` — a bot changes-requested review would hard-gate the
// merge, and the five-pillar verdicts are agent judgment. The teeth are the
// label: it mirrors into the required `Review gate` commit status on a
// Rust-touching PR (blocking merge there), plus the implement loop's
// obligation to address the comment.
//
// Inputs (env):
//   GITHUB_TOKEN        least-privilege token (pull-requests + issues write)
//   GITHUB_REPOSITORY   owner/repo
//   PR_NUMBER           the PR to annotate
//   HEAD_SHA            the reviewed PR head SHA
//   REVIEW_MODE         full or incremental
//   ROLLUP_PATH         review-rollup.json (the workflow's returned rollup)
//   FILES_PATH          pr-files.json (gh api pulls/{n}/files --paginate)
//
// No external deps — Node built-ins + global fetch only.

import { readFile } from 'node:fs/promises'

const TOKEN = requireEnv('GITHUB_TOKEN')
const REPO = requireEnv('GITHUB_REPOSITORY')
const PR = Number(requireEnv('PR_NUMBER'))
const HEAD_SHA = requireEnv('HEAD_SHA')
const REVIEW_MODE = requireEnv('REVIEW_MODE')
const ROLLUP_PATH = process.env.ROLLUP_PATH || 'review-rollup.json'
const FILES_PATH = process.env.FILES_PATH || 'pr-files.json'

const MARKER = '<!-- aether-review -->'
const FP_RE = /aether-review-fp:([^\s>]+)/g
const LABEL = 'review:unresolved'
const API = 'https://api.github.com'

function requireEnv(name) {
  const v = process.env[name]
  if (!v) throw new Error(`post-review-rollup: ${name} is required`)
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
      'user-agent': 'aether-review-poster',
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
        'user-agent': 'aether-review-poster',
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

// The set of new-side (RIGHT) line numbers a unified-diff patch makes
// commentable — added and context lines. Deleted lines don't exist on the
// new side, so they can't anchor an inline comment. Anchoring only to lines
// in this set is what keeps the review POST from 422-ing on a bad position.
function commentableLines(patch) {
  const lines = new Set()
  if (!patch) return lines
  let newLine = 0
  for (const row of patch.split('\n')) {
    const hunk = row.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/)
    if (hunk) { newLine = Number(hunk[1]); continue }
    if (row.startsWith('+')) { lines.add(newLine); newLine++ }
    else if (row.startsWith('-')) { /* old side only */ }
    else if (row.startsWith('\\')) { /* "\ No newline at end of file" */ }
    else { lines.add(newLine); newLine++ } // context line
  }
  return lines
}

// The rollup now stores each confirmed finding's `file` as the full changed-file
// path (review.js only bases at render-time label sites). The changed.has()
// fast-path below resolves it directly; the basename-match arms are defensive
// back-compat for a rollup produced before this normalization, degrading to
// null when a bare basename is ambiguous, in which case the finding drops to
// the folded review body rather than being dropped outright.
function resolvePath(fileField, changed) {
  if (!fileField) return null
  if (changed.has(fileField)) return fileField
  const matches = [...changed].filter(
    (p) => p === fileField || p.endsWith(`/${fileField}`) || p.split('/').pop() === fileField,
  )
  return matches.length === 1 ? matches[0] : null
}

function fingerprint(path, line, pillar) {
  return `${path || '?'}|${line ?? '-'}|${pillar || '?'}`
}

function findingBody(f) {
  const rec = f.recommendation ? `${f.recommendation}: ` : ''
  const sev = f.severity ? ` _(${f.severity})_` : ''
  const suggestion = f.suggested_form || f.description || ''
  const cat = f.category ? `/${f.category}` : ''
  return `**${f.pillar}${cat}**${sev} — ${f.symbol || ''} ${rec}${suggestion}`.trim()
}

async function main() {
  const rawRollup = JSON.parse(await readFile(ROLLUP_PATH, 'utf8'))
  // Accept either the bare rollup or the workflow's full { rollup, files }.
  const rollup = rawRollup && rawRollup.rollup ? rawRollup.rollup : rawRollup
  const confirmed = Array.isArray(rollup.confirmed) ? rollup.confirmed : []
  const softHolds = Array.isArray(rollup.softHolds) ? rollup.softHolds : []
  const specFindings = rollup.spec && Array.isArray(rollup.spec.findings) ? rollup.spec.findings : []

  const filesData = JSON.parse(await readFile(FILES_PATH, 'utf8'))
  const changed = new Set(filesData.map((f) => f.filename))
  const hunks = new Map(filesData.map((f) => [f.filename, commentableLines(f.patch)]))

  // Gather fingerprints already posted, from every surface that could carry
  // one, so a dispatched re-run never double-annotates.
  const posted = new Set()
  const [reviewComments, reviews, issueComments] = await Promise.all([
    apiList(`repos/${REPO}/pulls/${PR}/comments`),
    apiList(`repos/${REPO}/pulls/${PR}/reviews`),
    apiList(`repos/${REPO}/issues/${PR}/comments`),
  ])
  for (const c of [...reviewComments, ...reviews, ...issueComments]) {
    for (const m of String(c.body || '').matchAll(FP_RE)) posted.add(m[1])
  }

  // Partition findings into inline (anchored on a changed line) and body
  // (everything else, including spec-fidelity findings, which carry no
  // line). Each finding is normalized to a common shape first.
  const normalized = [
    ...confirmed.map((f) => ({ ...f, path: resolvePath(f.file, changed) })),
    ...specFindings.map((f) => ({
      ...f, pillar: 'spec-fidelity', line: undefined, recommendation: undefined,
      suggested_form: f.description, path: resolvePath(f.file, changed),
    })),
  ]

  const inline = []
  const folded = []
  const allFingerprints = []
  for (const f of normalized) {
    const anchor = f.path && f.line != null && hunks.get(f.path)?.has(Number(f.line))
    const fp = fingerprint(anchor ? f.path : f.path || f.file, anchor ? f.line : undefined, f.pillar)
    allFingerprints.push(fp)
    if (posted.has(fp)) continue
    if (anchor) {
      inline.push({
        path: f.path,
        line: Number(f.line),
        side: 'RIGHT',
        body: `${findingBody(f)}\n<!-- aether-review-fp:${fp} -->`,
      })
    } else {
      folded.push({ f, fp })
    }
  }

  // The COMMENT review: inline annotations + the folded findings in its
  // body. Only posted when there is genuinely new content — a re-run that
  // annotates nothing new stays silent.
  if (inline.length || folded.length) {
    const commit = await api('GET', `repos/${REPO}/pulls/${PR}`)
    const commitId = commit.ok ? commit.data.head.sha : undefined
    const bodyLines = ['## Five-pillar review', '']
    if (folded.length) {
      bodyLines.push('Findings not anchored to a changed line:', '')
      for (const { f, fp } of folded) {
        bodyLines.push(`- ${findingBody(f)} \`${f.path || f.file || ''}${f.line != null ? `:${f.line}` : ''}\``)
        bodyLines.push(`  <!-- aether-review-fp:${fp} -->`)
      }
      bodyLines.push('')
    }
    if (softHolds.length) {
      bodyLines.push(`**${softHolds.length} soft-hold** finding(s) — clear before un-draft.`, '')
    }
    bodyLines.push('_Findings are advisory (this is a COMMENT review); on a Rust-touching PR an unresolved verdict blocks merge via the required `Review gate` status._')

    const review = { event: 'COMMENT', body: bodyLines.join('\n') }
    if (inline.length && commitId) { review.commit_id = commitId; review.comments = inline }
    let res = await api('POST', `repos/${REPO}/pulls/${PR}/reviews`, review)
    if (!res.ok && review.comments) {
      // A rejected inline position (e.g. an outdated hunk) 422s the whole
      // review. Fold the inline findings into the body and retry so the
      // findings still land rather than being dropped.
      console.warn(`inline review rejected (${res.status}) — retrying folded into the body`)
      const extra = inline.map((c) => `- ${c.body.split('\n')[0]} \`${c.path}:${c.line}\``)
      res = await api('POST', `repos/${REPO}/pulls/${PR}/reviews`, {
        event: 'COMMENT',
        body: `${review.body}\n\n${extra.join('\n')}`,
      })
    }
    if (!res.ok) console.error(`review POST failed: ${res.status} ${res.text}`)
    else console.log(`posted review: ${inline.length} inline, ${folded.length} folded`)
  } else {
    console.log('no new findings to annotate')
  }

  // Upsert the marker-anchored summary comment — the human-readable rollup
  // and the reviewed head SHA. Regenerated in full each run, and it carries
  // every fingerprint so re-runs dedup against it.
  const summary = renderSummary(rollup, normalized, allFingerprints)
  const existing = issueComments.find((c) => String(c.body || '').includes(MARKER))
  if (existing) {
    await api('PATCH', `repos/${REPO}/issues/comments/${existing.id}`, { body: summary })
    console.log('updated summary comment')
  } else {
    await api('POST', `repos/${REPO}/issues/${PR}/comments`, { body: summary })
    console.log('posted summary comment')
  }

  // Advisory label: set when there is something actionable (a confirmed
  // finding, a soft-hold, or a high-severity spec finding), cleared clean.
  const actionable =
    confirmed.length > 0 || softHolds.length > 0 || specFindings.some((f) => f.severity === 'high')
  if (actionable) {
    const res = await api('POST', `repos/${REPO}/issues/${PR}/labels`, { labels: [LABEL] })
    if (!res.ok) console.error(`add label failed: ${res.status} ${res.text}`)
    else console.log(`set ${LABEL}`)
  } else if (REVIEW_MODE !== 'incremental') {
    const res = await api('DELETE', `repos/${REPO}/issues/${PR}/labels/${encodeURIComponent(LABEL)}`)
    // 404 = the label was not present; a clean no-op.
    if (res.ok || res.status === 404) console.log(`cleared ${LABEL}`)
    else console.error(`remove label failed: ${res.status} ${res.text}`)
  } else {
    console.log(`left ${LABEL} unchanged after clean incremental review`)
  }
}

function renderSummary(rollup, normalized, fingerprints) {
  const t = rollup.totals || {}
  const lines = [MARKER, '## Five-pillar review — summary', '']
  lines.push(
    `Confirmed: **${(rollup.confirmed || []).length}** · ` +
      `Soft-holds: **${(rollup.softHolds || []).length}** · ` +
      `Spec: **${normalized.filter((f) => f.pillar === 'spec-fidelity').length}** · ` +
      `Lint candidates: ${(rollup.lintCandidates || []).length} · ` +
      `Spared: ${(rollup.spared || []).length} · ` +
      `Uncertain: ${(rollup.uncertain || []).length}`,
    '',
  )

  const grouped = new Map()
  for (const f of normalized) {
    const key = f.path || f.file || '(unknown)'
    if (!grouped.has(key)) grouped.set(key, [])
    grouped.get(key).push(f)
  }
  if (grouped.size) {
    lines.push('### Findings', '')
    for (const [file, fs] of grouped) {
      lines.push(`**\`${file}\`**`)
      for (const f of fs) {
        const loc = f.line != null ? `:${f.line}` : ''
        lines.push(`- ${findingBody(f)}${loc ? ` \`${loc}\`` : ''}`)
      }
      lines.push('')
    }
  } else {
    lines.push('_No confirmed findings — the change is clean under all five pillars._', '')
  }

  lines.push('_Findings are advisory (this is a COMMENT review); on a Rust-touching PR an unresolved verdict blocks merge via the required `Review gate` status._')
  // Hidden reviewed state and fingerprints for incremental routing + dedup.
  lines.push(`<!-- aether-reviewed-sha:${HEAD_SHA} -->`)
  for (const fp of fingerprints) lines.push(`<!-- aether-review-fp:${fp} -->`)
  return lines.join('\n')
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})
