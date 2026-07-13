#!/usr/bin/env node
// Post a five-pillar review rollup onto its PR: inline annotations for
// findings that land on a changed diff line, everything else folded into
// the verdict review body, plus a marker-anchored summary comment and the
// `review:unresolved` label.
//
// The posture is load-bearing: the verdict review is now authored by the
// reviewer App `iamabarista` (BARISTA_TOKEN) as a NATIVE `APPROVE` /
// `REQUEST_CHANGES` review — barista authors no PR, so GitHub does not 422
// its verdict the way it would kettle's self-authored one (ADR-0148). The
// event is selected by review mode: `REQUEST_CHANGES` when the rollup is
// actionable (either mode), `APPROVE` only on a clean FULL pass, and NO
// submission on a clean INCREMENTAL pass — an incremental delta says nothing
// about an earlier full-review finding, so an `APPROVE` there would wrongly
// supersede barista's own standing `REQUEST_CHANGES`.
//
// This rung ships the native verdict SIDE-BY-SIDE with the still-authoritative
// mirror chain: the `review:unresolved` label (written here on GITHUB_TOKEN)
// mirrors into the required `Review gate` commit status, which is what branch
// protection gates on until the flip retires it. Everything except the single
// verdict-submission call stays on GITHUB_TOKEN; only that POST rides barista.
//
// Inputs (env):
//   GITHUB_TOKEN        least-privilege token (pull-requests + issues write)
//   BARISTA_TOKEN       iamabarista installation token — authors the verdict
//   BARISTA_LOGIN       barista's bot login (default iamabarista[bot]); used to
//                       find barista's standing verdict for the dedup guard
//   GITHUB_REPOSITORY   owner/repo
//   PR_NUMBER           the PR to annotate
//   HEAD_SHA            the reviewed PR head SHA
//   REVIEW_MODE         full or incremental
//   ROLLUP_PATH         review-rollup.json (the workflow's returned rollup)
//   FILES_PATH          pr-files.json (gh api pulls/{n}/files --paginate)
//
// No external deps — Node built-ins + global fetch only.

import { readFile } from 'node:fs/promises'
import { pathToFileURL } from 'node:url'

const MARKER = '<!-- aether-review -->'
const FP_RE = /aether-review-fp:([^\s>]+)/g
const LABEL = 'review:unresolved'
const API = 'https://api.github.com'

// The verdict is native now, but the label → `Review gate` status mirror stays
// authoritative until #3233 retires it, so the footer names both.
const POSTURE_FOOTER =
  '_Barista submits this as a native `APPROVE` / `REQUEST_CHANGES` verdict; until the branch-protection flip lands, the required `Review gate` status mirrored from the `review:unresolved` label remains the authoritative merge gate._'

function requireEnv(name) {
  const v = process.env[name]
  if (!v) throw new Error(`post-review-rollup: ${name} is required`)
  return v
}

function environment() {
  return {
    token: requireEnv('GITHUB_TOKEN'),
    baristaToken: requireEnv('BARISTA_TOKEN'),
    baristaLogin: process.env.BARISTA_LOGIN || 'iamabarista[bot]',
    repo: requireEnv('GITHUB_REPOSITORY'),
    pr: Number(requireEnv('PR_NUMBER')),
    headSha: requireEnv('HEAD_SHA'),
    reviewMode: requireEnv('REVIEW_MODE'),
    rollupPath: process.env.ROLLUP_PATH || 'review-rollup.json',
    filesPath: process.env.FILES_PATH || 'pr-files.json',
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
async function apiList(token, path) {
  const out = []
  let url = `${API}/${path}${path.includes('?') ? '&' : '?'}per_page=100`
  while (url) {
    const res = await fetch(url, {
      headers: {
        authorization: `Bearer ${token}`,
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

// Canonicalize a path to repo-relative by stripping a leading checkout-root
// ($GITHUB_WORKSPACE) prefix. No-op when the env is unset or the prefix
// doesn't match, and idempotent on input that's already relative — so the
// same helper normalizes both a fresh emission and a legacy absolute marker
// parsed out of history.
export function repoRelative(path) {
  const root = process.env.GITHUB_WORKSPACE
  if (!root || !path) return path
  const prefix = `${root.replace(/\/+$/, '')}/`
  return path.startsWith(prefix) ? path.slice(prefix.length) : path
}

export function fingerprint(path, line, pillar) {
  return `${repoRelative(path) || '?'}|${line ?? '-'}|${pillar || '?'}`
}

// Normalize the path segment of a fingerprint parsed out of prior comment
// history, so a legacy absolute marker still matches a newly-emitted
// canonical one when building the `posted` dedup set.
export function normalizeFingerprint(fp) {
  const [path, ...rest] = fp.split('|')
  return [repoRelative(path), ...rest].join('|')
}

function findingBody(f) {
  const rec = f.recommendation ? `${f.recommendation}: ` : ''
  const sev = f.severity ? ` _(${f.severity})_` : ''
  const suggestion = f.suggested_form || f.description || ''
  const cat = f.category ? `/${f.category}` : ''
  return `**${f.pillar}${cat}**${sev} — ${f.symbol || ''} ${rec}${suggestion}`.trim()
}

// The rollup is actionable when it carries anything a reviewer must clear: a
// confirmed finding, a soft-hold, or a high-severity spec-fidelity finding.
// This is the same predicate the `review:unresolved` label decision uses, so
// the native verdict and the mirror label never disagree on what "clean" means.
export function isActionable(rollup) {
  const confirmed = Array.isArray(rollup.confirmed) ? rollup.confirmed : []
  const softHolds = Array.isArray(rollup.softHolds) ? rollup.softHolds : []
  const specFindings = rollup.spec && Array.isArray(rollup.spec.findings) ? rollup.spec.findings : []
  return confirmed.length > 0 || softHolds.length > 0 || specFindings.some((f) => f.severity === 'high')
}

// Select barista's verdict event, review-mode-aware:
//   actionable (either mode)  -> REQUEST_CHANGES
//   clean, full pass          -> APPROVE
//   clean, incremental pass   -> null (no submission — leave standing verdict)
// The incremental-clean null mirrors the label branch that leaves
// `review:unresolved` untouched after a clean incremental review: a delta that
// re-checks only the newly-changed lines cannot vouch for the whole PR, so an
// `APPROVE` there would supersede an earlier full-review `REQUEST_CHANGES`.
export function verdictEvent(rollup, reviewMode) {
  if (isActionable(rollup)) return 'REQUEST_CHANGES'
  return reviewMode === 'incremental' ? null : 'APPROVE'
}

// Whether to submit the owed verdict, given barista's standing verdict on this
// head SHA (`latestBarista`, in event vocabulary, or null) and whether there is
// fresh inline/body content this run. Submit only when a verdict is owed AND
// one of: there is new content, barista's standing verdict differs from the one
// now owed (a clean<->actionable transition), or barista has no standing verdict
// on this SHA yet. A null `owedEvent` (clean incremental) never submits.
export function shouldSubmitVerdict({ owedEvent, latestBarista, hasNewContent }) {
  if (!owedEvent) return false
  if (hasNewContent) return true
  if (!latestBarista) return true
  return latestBarista !== owedEvent
}

// GitHub stores a submitted review's verdict as APPROVED / CHANGES_REQUESTED;
// the submit event is APPROVE / REQUEST_CHANGES. Normalize a stored state back
// to the event vocabulary so the dedup guard compares like with like. Anything
// that is not a verdict (COMMENTED / DISMISSED / PENDING) maps to null.
function reviewStateToEvent(state) {
  if (state === 'APPROVED') return 'APPROVE'
  if (state === 'CHANGES_REQUESTED') return 'REQUEST_CHANGES'
  return null
}

// Barista's most-recent verdict (APPROVE / REQUEST_CHANGES) on this head SHA,
// in event vocabulary, or null when barista has none. Reviews arrive in
// submitted order, so the last matching verdict is the standing one.
function standingBaristaVerdict(reviews, baristaLogin, headSha) {
  const events = reviews
    .filter((r) => r.user && r.user.login === baristaLogin && r.commit_id === headSha)
    .map((r) => reviewStateToEvent(r.state))
    .filter(Boolean)
  return events.length ? events[events.length - 1] : null
}

function buildReviewBody(folded, softHolds, owedEvent) {
  const lines = ['## Five-pillar review', '']
  if (owedEvent === 'APPROVE' && !folded.length && !softHolds.length) {
    lines.push('No confirmed findings — the change is clean under all five pillars.', '')
  }
  if (folded.length) {
    lines.push('Findings not anchored to a changed line:', '')
    for (const { f, fp } of folded) {
      lines.push(`- ${findingBody(f)} \`${f.path || f.file || ''}${f.line != null ? `:${f.line}` : ''}\``)
      lines.push(`  <!-- aether-review-fp:${fp} -->`)
    }
    lines.push('')
  }
  if (softHolds.length) {
    lines.push(`**${softHolds.length} soft-hold** finding(s) — clear before un-draft.`, '')
  }
  lines.push(POSTURE_FOOTER)
  return lines.join('\n')
}

async function main() {
  const env = environment()
  const rawRollup = JSON.parse(await readFile(env.rollupPath, 'utf8'))
  // Accept either the bare rollup or the workflow's full { rollup, files }.
  const rollup = rawRollup && rawRollup.rollup ? rawRollup.rollup : rawRollup
  const confirmed = Array.isArray(rollup.confirmed) ? rollup.confirmed : []
  const softHolds = Array.isArray(rollup.softHolds) ? rollup.softHolds : []
  const specFindings = rollup.spec && Array.isArray(rollup.spec.findings) ? rollup.spec.findings : []

  const filesData = JSON.parse(await readFile(env.filesPath, 'utf8'))
  const changed = new Set(filesData.map((f) => f.filename))
  const hunks = new Map(filesData.map((f) => [f.filename, commentableLines(f.patch)]))

  // Gather fingerprints already posted, from every surface that could carry
  // one, so a dispatched re-run never double-annotates.
  const posted = new Set()
  const [reviewComments, reviews, issueComments] = await Promise.all([
    apiList(env.token, `repos/${env.repo}/pulls/${env.pr}/comments`),
    apiList(env.token, `repos/${env.repo}/pulls/${env.pr}/reviews`),
    apiList(env.token, `repos/${env.repo}/issues/${env.pr}/comments`),
  ])
  for (const c of [...reviewComments, ...reviews, ...issueComments]) {
    for (const m of String(c.body || '').matchAll(FP_RE)) posted.add(normalizeFingerprint(m[1]))
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

  // The native barista verdict: event selected by review mode, inline
  // annotations attached when there are fresh ones, the folded findings in the
  // body. Decoupled from the inline/folded guard — a verdict may be owed (an
  // APPROVE on a clean pass, or a standing REQUEST_CHANGES to (re)assert) with
  // no fresh annotations — but the duplicate-verdict guard keeps the timeline
  // clean: a same-event same-SHA re-run with no new content is skipped, and a
  // clean incremental pass submits nothing so it never supersedes a standing
  // REQUEST_CHANGES.
  const owedEvent = verdictEvent(rollup, env.reviewMode)
  const hasNewContent = inline.length > 0 || folded.length > 0
  const latestBarista = standingBaristaVerdict(reviews, env.baristaLogin, env.headSha)
  if (shouldSubmitVerdict({ owedEvent, latestBarista, hasNewContent })) {
    const body = buildReviewBody(folded, softHolds, owedEvent)
    const review = { event: owedEvent, body, commit_id: env.headSha }
    if (inline.length) review.comments = inline
    let res = await api(env.baristaToken, 'POST', `repos/${env.repo}/pulls/${env.pr}/reviews`, review)
    if (!res.ok && review.comments) {
      // A rejected inline position (e.g. an outdated hunk) 422s the whole
      // review. Fold the inline findings into the body and retry — with the
      // SAME verdict event, so a bad hunk position never silently downgrades a
      // blocking REQUEST_CHANGES to an advisory comment.
      console.warn(`inline review rejected (${res.status}) — retrying folded into the body`)
      const extra = inline.map((c) => `- ${c.body.split('\n')[0]} \`${c.path}:${c.line}\``)
      res = await api(env.baristaToken, 'POST', `repos/${env.repo}/pulls/${env.pr}/reviews`, {
        event: owedEvent,
        commit_id: env.headSha,
        body: `${body}\n\n${extra.join('\n')}`,
      })
    }
    if (!res.ok) console.error(`review POST failed: ${res.status} ${res.text}`)
    else console.log(`posted ${owedEvent} verdict: ${inline.length} inline, ${folded.length} folded`)
  } else if (owedEvent) {
    console.log(`barista ${owedEvent} already standing on ${env.headSha} — no re-submission`)
  } else {
    console.log('clean incremental review — leaving barista\'s standing verdict untouched')
  }

  // Upsert the marker-anchored summary comment — the human-readable rollup
  // and the reviewed head SHA. Regenerated in full each run, and it carries
  // every fingerprint so re-runs dedup against it.
  const summary = renderSummary(rollup, normalized, allFingerprints, env.headSha)
  const existing = issueComments.find((c) => String(c.body || '').includes(MARKER))
  if (existing) {
    await api(env.token, 'PATCH', `repos/${env.repo}/issues/comments/${existing.id}`, { body: summary })
    console.log('updated summary comment')
  } else {
    await api(env.token, 'POST', `repos/${env.repo}/issues/${env.pr}/comments`, { body: summary })
    console.log('posted summary comment')
  }

  // Advisory label: set when there is something actionable (a confirmed
  // finding, a soft-hold, or a high-severity spec finding), cleared clean.
  if (isActionable(rollup)) {
    const res = await api(env.token, 'POST', `repos/${env.repo}/issues/${env.pr}/labels`, { labels: [LABEL] })
    if (!res.ok) console.error(`add label failed: ${res.status} ${res.text}`)
    else console.log(`set ${LABEL}`)
  } else if (env.reviewMode !== 'incremental') {
    const res = await api(env.token, 'DELETE', `repos/${env.repo}/issues/${env.pr}/labels/${encodeURIComponent(LABEL)}`)
    // 404 = the label was not present; a clean no-op.
    if (res.ok || res.status === 404) console.log(`cleared ${LABEL}`)
    else console.error(`remove label failed: ${res.status} ${res.text}`)
  } else {
    console.log(`left ${LABEL} unchanged after clean incremental review`)
  }
}

function renderSummary(rollup, normalized, fingerprints, headSha) {
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

  lines.push(POSTURE_FOOTER)
  // Hidden reviewed state and fingerprints for incremental routing + dedup.
  lines.push(`<!-- aether-reviewed-sha:${headSha} -->`)
  for (const fp of fingerprints) lines.push(`<!-- aether-review-fp:${fp} -->`)
  return lines.join('\n')
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((err) => {
    console.error(err)
    process.exit(1)
  })
}
