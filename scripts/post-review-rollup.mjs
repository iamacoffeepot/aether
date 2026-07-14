#!/usr/bin/env node
// Post a five-pillar review rollup onto its PR: inline annotations for
// findings that land on a changed diff line, everything else folded into
// the verdict review body, plus a marker-anchored summary comment.
//
// The posture is load-bearing: the verdict review is authored by the
// reviewer App `iamabarista` (BARISTA_TOKEN) as a NATIVE `APPROVE` /
// `REQUEST_CHANGES` review — barista authors no PR, so GitHub does not 422
// its verdict the way it would kettle's self-authored one (ADR-0148). Every
// run submits exactly one verdict: the review runs on an explicit request,
// a request means "vouch for this PR", so the run owes `REQUEST_CHANGES`
// when the rollup is actionable and `APPROVE` when it is clean — no third
// outcome.
//
// Branch protection's native required review is the merge gate: barista's
// standing verdict blocks the merge until it is APPROVE, or the owner
// approves / dismisses it natively. Everything except the single
// verdict-submission call stays on GITHUB_TOKEN; only that POST rides barista.
//
// Inputs (env):
//   GITHUB_TOKEN        least-privilege token (pull-requests + issues write)
//   BARISTA_TOKEN       iamabarista installation token — authors the verdict
//   GITHUB_REPOSITORY   owner/repo
//   PR_NUMBER           the PR to annotate
//   HEAD_SHA            the reviewed PR head SHA
//   ROLLUP_PATH         review-rollup.json (the workflow's returned rollup)
//   FILES_PATH          pr-files.json (gh api pulls/{n}/files --paginate)
//
// No external deps — Node built-ins + global fetch only.

import { readFile } from 'node:fs/promises'
import { pathToFileURL } from 'node:url'

const MARKER = '<!-- aether-review -->'
const FP_RE = /aether-review-fp:([^\s>]+)/g
const API = 'https://api.github.com'

// Barista submits the verdict natively; branch protection's required review is
// what gates the merge on it.
const POSTURE_FOOTER =
  '_Barista submits this as a native `APPROVE` / `REQUEST_CHANGES` review; branch protection\'s required review gates the merge on the standing verdict._'

function requireEnv(name) {
  const v = process.env[name]
  if (!v) throw new Error(`post-review-rollup: ${name} is required`)
  return v
}

function environment() {
  return {
    token: requireEnv('GITHUB_TOKEN'),
    baristaToken: requireEnv('BARISTA_TOKEN'),
    repo: requireEnv('GITHUB_REPOSITORY'),
    pr: Number(requireEnv('PR_NUMBER')),
    headSha: requireEnv('HEAD_SHA'),
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

async function graphql(token, query, variables) {
  const res = await fetch(`${API}/graphql`, {
    method: 'POST',
    headers: {
      authorization: `Bearer ${token}`,
      accept: 'application/vnd.github+json',
      'user-agent': 'aether-review-poster',
      'content-type': 'application/json',
    },
    body: JSON.stringify({ query, variables }),
  })
  const text = await res.text()
  return { ok: res.ok, data: text ? safeJson(text) : null }
}

// The issue a PR closes, via the authoritative closingIssuesReferences link
// (not a body regex — a re-worded body never loses it), mirroring the resolver
// dogfood.yml / reconciler.yml use. null when the PR closes no issue.
async function closingIssue(token, repo, pr) {
  const [owner, name] = repo.split('/')
  const { ok, data } = await graphql(token,
    `query($owner:String!,$name:String!,$pr:Int!){
       repository(owner:$owner,name:$name){
         pullRequest(number:$pr){ closingIssuesReferences(first:1){ nodes{ number } } }
       }
     }`,
    { owner, name, pr })
  if (!ok) return null
  return data?.data?.repository?.pullRequest?.closingIssuesReferences?.nodes?.[0]?.number ?? null
}

// Interim ask-and-park for a confirm-pass restart signal (issue #3390). Until the
// native bounce outcome lands (#3391), a delta that needs restart-level rework
// stays inside ADR-0148's two-outcome model: barista's standing REQUEST_CHANGES
// holds the merge, and the owner is parked on the closing issue to decide. Resolve
// the linked issue, apply `agent:awaiting-answer`, and post the park comment. A
// missing linked issue or a failed call degrades to the standing REQUEST_CHANGES
// alone — the restart is still surfaced in the verdict body, so the park is a best
// effort on top, never a gate.
async function parkForRestart(env, rationale) {
  const issue = await closingIssue(env.token, env.repo, env.pr)
  if (!issue) {
    console.warn('confirm restart: PR closes no issue — cannot park; standing REQUEST_CHANGES holds the merge.')
    return
  }
  await api(env.token, 'POST', `repos/${env.repo}/issues/${issue}/labels`, { labels: ['agent:awaiting-answer'] })
  const why = rationale ? rationale : 'the delta since the deep review diverges enough that confirming individual findings is meaningless'
  const body = [
    `**Parked on #${issue} — need a decision.**`,
    '',
    `The confirm re-review of PR #${env.pr} raised a **restart signal**: ${why}. A confirm pass never re-reviews from scratch (that is the non-converging ratchet it exists to stop) and the deep pass runs at most once per PR, so a restart-level rework is a bounce decision, not another review round. Barista's standing \`REQUEST_CHANGES\` holds the merge in the meantime.`,
    '',
    'Options:',
    `1. Bounce the issue and re-scope the rework — the honest path when the delta is a redesign.`,
    `2. Force a fresh deep review (\`@barista full review\` on the PR) — if you judge the delta a genuine new baseline worth one more full pass.`,
    '',
    'Reply with an option number or free-form; your reply re-dispatches this job.',
  ].join('\n')
  await api(env.token, 'POST', `repos/${env.repo}/issues/${issue}/comments`, { body })
  console.log(`confirm restart: parked owner on #${issue} (agent:awaiting-answer)`)
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

// Prior barista correctness-pillar fingerprint paths, parsed from the full
// review/comment history the caller already fetched for the `posted` dedup
// pass. Reuses normalizeFingerprint outright — a legacy absolute-path marker
// (predating the repo-relative convention) yields the same repo-relative key
// a fresh emission carries, so a mixed-form history still matches.
export function disclosedPaths(comments) {
  const disclosed = new Set()
  for (const c of comments) {
    for (const m of String(c.body || '').matchAll(FP_RE)) {
      const [path, , pillar] = normalizeFingerprint(m[1]).split('|')
      if (pillar === 'correctness') disclosed.add(path)
    }
  }
  return disclosed
}

function findingBody(f) {
  const rec = f.recommendation ? `${f.recommendation}: ` : ''
  const sev = f.severity ? ` _(${f.severity})_` : ''
  const suggestion = f.suggested_form || f.description || ''
  const cat = f.category ? `/${f.category}` : ''
  return `**${f.pillar}${cat}**${sev} — ${f.symbol || ''} ${rec}${suggestion}`.trim()
}

// Shared by confirmed findings, spec findings, and soft-holds: resolve `file`
// to the full changed-file path once, up front, so every downstream anchor /
// fingerprint computation reads a common `path` field.
function withResolvedPath(f, changed) {
  return { ...f, path: resolvePath(f.file, changed) }
}

// One folded-finding line pair: the finding text + anchor, then its dedup
// marker. Shared by the ordinary folded section and the soft-hold section so
// the two render identically.
function renderFoldedFinding(f, fp) {
  const anchor = `${f.path || f.file || ''}${f.line != null ? `:${f.line}` : ''}`
  return [`- ${findingBody(f)} \`${anchor}\``, `  <!-- aether-review-fp:${fp} -->`]
}

// The one scope-leakage filter shared by the `isActionable` gate and the
// rendered soft-hold banner count, so they can't drift apart: a `disclosed`
// path — one a prior barista correctness finding already named on this same
// PR — routes a matching scope-leakage soft-hold to advisory. review.js's
// `specSoftHolds` mirror base-names `file` (`base(f.file)`), so the match
// tolerates a `disclosed` entry that is only a path suffix of it.
export function visibleSoftHolds(rollup, disclosed = new Set()) {
  const softHolds = Array.isArray(rollup.softHolds) ? rollup.softHolds : []
  return softHolds.filter((f) => {
    if (f.category !== 'scope-leakage' || !f.file) return true
    return !(disclosed.has(f.file) || [...disclosed].some((d) => d.endsWith(`/${f.file}`)))
  })
}

// The rollup is actionable when it carries anything a reviewer must clear: a
// confirmed finding, a soft-hold, a high-severity spec-fidelity finding, or —
// on a confirm pass (issue #3390) — a raised restart signal (the delta needs
// restart-level rework). This is the predicate `verdictEvent` reads to choose
// REQUEST_CHANGES over APPROVE, so it is the single definition of what "clean"
// means. A confirm pass re-asserts its still-open prior findings into
// `confirmed`, so an unaddressed-finding round is already actionable through
// the confirmed arm; the restart arm covers the case where the restart is the
// only signal (every prior finding addressed but the delta warrants a redo).
// `disclosed` — prior barista correctness-fingerprint paths on this same PR —
// routes a matching scope-leakage entry to advisory in the spec arm below.
export function isActionable(rollup, disclosed = new Set()) {
  const confirmed = Array.isArray(rollup.confirmed) ? rollup.confirmed : []
  const softHolds = visibleSoftHolds(rollup, disclosed)
  const specFindings = rollup.spec && Array.isArray(rollup.spec.findings) ? rollup.spec.findings : []
  const gatingSpecFindings = specFindings.filter(
    (f) => !(f.category === 'scope-leakage' && disclosed.has(f.file)),
  )
  const restart = !!(rollup.restart && rollup.restart.signaled)
  return confirmed.length > 0 || softHolds.length > 0 || gatingSpecFindings.some((f) => f.severity === 'high') || restart
}

// Select barista's verdict event. Every request yields a verdict — the
// request means "vouch for this PR", so a clean rollup APPROVEs and an
// actionable one REQUEST_CHANGES; there is no third outcome.
export function verdictEvent(rollup, disclosed = new Set()) {
  return isActionable(rollup, disclosed) ? 'REQUEST_CHANGES' : 'APPROVE'
}

export function buildReviewBody(folded, softHoldFolded, softHoldCount, owedEvent, followUps = []) {
  const lines = ['## Five-pillar review', '']
  if (owedEvent === 'APPROVE' && !folded.length && !softHoldCount) {
    lines.push('No confirmed findings — the change is clean under all five pillars.', '')
  }
  if (folded.length) {
    lines.push('Findings not anchored to a changed line:', '')
    for (const { f, fp } of folded) lines.push(...renderFoldedFinding(f, fp))
    lines.push('')
  }
  if (softHoldCount) {
    lines.push(`**${softHoldCount} soft-hold** finding(s) — clear before un-draft.`, '')
    for (const { f, fp } of softHoldFolded) lines.push(...renderFoldedFinding(f, fp))
    lines.push('')
  }
  // Advisory pre-existing findings (issue #3250): reachable on `main`, so a discovery to file as a
  // separate issue — NEVER an in-PR demand, and kept out of the confirmed / soft-hold counts so they
  // never gate. Rendered without a dedup marker: they carry no verdict teeth.
  if (followUps.length) {
    lines.push(`**${followUps.length} pre-existing finding(s)** — reachable on \`main\`; file as a separate follow-up issue, not fixed in this PR.`, '')
    for (const f of followUps) {
      const anchor = `${f.path || f.file || ''}${f.line != null ? `:${f.line}` : ''}`
      lines.push(`- ${findingBody(f)}${anchor ? ` \`${anchor}\`` : ''}`)
    }
    lines.push('')
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
  const followUps = Array.isArray(rollup.followUps) ? rollup.followUps : []
  const specFindings = rollup.spec && Array.isArray(rollup.spec.findings) ? rollup.spec.findings : []

  const filesData = JSON.parse(await readFile(env.filesPath, 'utf8'))
  const changed = new Set(filesData.map((f) => f.filename))
  const hunks = new Map(filesData.map((f) => [f.filename, commentableLines(f.patch)]))

  // Gather fingerprints already posted, from every surface that could carry
  // one, so a dispatched re-run never double-annotates. The same history
  // yields the disclosed-paths set: a prior barista correctness finding on
  // this same PR routes a matching scope-leakage finding to advisory.
  const posted = new Set()
  const [reviewComments, reviews, issueComments] = await Promise.all([
    apiList(env.token, `repos/${env.repo}/pulls/${env.pr}/comments`),
    apiList(env.token, `repos/${env.repo}/pulls/${env.pr}/reviews`),
    apiList(env.token, `repos/${env.repo}/issues/${env.pr}/comments`),
  ])
  const disclosed = disclosedPaths([...reviewComments, ...reviews, ...issueComments])
  const softHolds = visibleSoftHolds(rollup, disclosed)
  for (const c of [...reviewComments, ...reviews, ...issueComments]) {
    for (const m of String(c.body || '').matchAll(FP_RE)) posted.add(normalizeFingerprint(m[1]))
  }

  // Partition findings into inline (anchored on a changed line) and body
  // (everything else, including spec-fidelity findings, which carry no
  // line). Each finding is normalized to a common shape first.
  const normalized = [
    ...confirmed.map((f) => withResolvedPath(f, changed)),
    ...specFindings.map((f) => withResolvedPath({
      ...f, pillar: 'spec-fidelity', line: undefined, recommendation: undefined,
      suggested_form: f.description,
    }, changed)),
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

  // Every soft-hold duplicates a row already present in `confirmed` or
  // `spec.findings` (review.js's `specSoftHolds` decoupling), so dedup against
  // the fingerprints normalized/spec just produced — a soft-hold that overlaps
  // renders once, via its confirmed/spec row, not twice. `posted` catches the
  // re-run case: a soft-hold with no confirmed/spec twin that was already
  // annotated on an earlier pass. Survivors partition inline/folded exactly
  // like the confirmed/spec path; `softHoldsForSummary` (dedup against
  // confirmed/spec only, not `posted`) feeds the always-regenerated summary
  // comment so it never goes missing a soft-hold-only entry.
  const confirmedSpecFingerprints = new Set(allFingerprints)
  const softHoldsNormalized = softHolds.map((f) => withResolvedPath(f, changed))
  const softHoldFolded = []
  const softHoldsForSummary = []
  for (const f of softHoldsNormalized) {
    const anchor = f.path && f.line != null && hunks.get(f.path)?.has(Number(f.line))
    const fp = fingerprint(anchor ? f.path : f.path || f.file, anchor ? f.line : undefined, f.pillar)
    const duplicate = confirmedSpecFingerprints.has(fp)
    if (!duplicate) softHoldsForSummary.push(f)
    if (duplicate || posted.has(fp)) continue
    allFingerprints.push(fp)
    if (anchor) {
      inline.push({
        path: f.path,
        line: Number(f.line),
        side: 'RIGHT',
        body: `${findingBody(f)}\n<!-- aether-review-fp:${fp} -->`,
      })
    } else {
      softHoldFolded.push({ f, fp })
    }
  }

  // The native barista verdict: every run submits one — REQUEST_CHANGES when
  // the rollup is actionable, APPROVE when it is clean — with fresh inline
  // annotations attached when there are any and the folded findings in the
  // body. Unconditional by design: the review runs on an explicit request,
  // so the request is what owes the verdict, and a fresh submission after a
  // fix push is exactly what supersedes the standing REQUEST_CHANGES that
  // `dismiss_stale_reviews` left behind.
  const owedEvent = verdictEvent(rollup, disclosed)
  const followUpsNormalized = followUps.map((f) => withResolvedPath(f, changed))
  const body = buildReviewBody(folded, softHoldFolded, softHolds.length, owedEvent, followUpsNormalized)
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

  // Confirm-pass restart signal (issue #3390) — the interim ask-and-park stand-in
  // for the native bounce (#3391). The standing REQUEST_CHANGES above already
  // holds the merge; this parks the owner on the linked issue to decide the redo.
  if (rollup.reviewPass === 'confirm' && rollup.restart && rollup.restart.signaled) {
    await parkForRestart(env, rollup.restart.rationale)
  }

  // Upsert the marker-anchored summary comment — the human-readable rollup.
  // Regenerated in full each run, and it carries every fingerprint so
  // re-runs dedup against it.
  const summary = renderSummary(rollup, normalized, softHoldsForSummary, followUpsNormalized, allFingerprints)
  const existing = issueComments.find((c) => String(c.body || '').includes(MARKER))
  if (existing) {
    await api(env.token, 'PATCH', `repos/${env.repo}/issues/comments/${existing.id}`, { body: summary })
    console.log('updated summary comment')
  } else {
    await api(env.token, 'POST', `repos/${env.repo}/issues/${env.pr}/comments`, { body: summary })
    console.log('posted summary comment')
  }
}

function renderSummary(rollup, normalized, softHoldsForSummary, followUps, fingerprints) {
  const lines = [MARKER, '## Five-pillar review — summary', '']
  lines.push(
    `Confirmed: **${(rollup.confirmed || []).length}** · ` +
      `Soft-holds: **${(rollup.softHolds || []).length}** · ` +
      `Spec: **${normalized.filter((f) => f.pillar === 'spec-fidelity').length}** · ` +
      `Follow-ups: ${(rollup.followUps || []).length} · ` +
      `Lint candidates: ${(rollup.lintCandidates || []).length} · ` +
      `Spared: ${(rollup.spared || []).length} · ` +
      `Uncertain: ${(rollup.uncertain || []).length}`,
    '',
  )

  const grouped = new Map()
  for (const f of [...normalized, ...softHoldsForSummary]) {
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

  // Advisory pre-existing findings (issue #3250) — reachable on `main`, so file as separate
  // follow-up issues, never fixed in this PR. Kept out of the confirmed / soft-hold sections above.
  if (followUps.length) {
    lines.push('### Pre-existing (advisory — file as follow-up, not fixed here)', '')
    for (const f of followUps) {
      const loc = f.line != null ? `:${f.line}` : ''
      const anchor = `${f.path || f.file || ''}${loc}`
      lines.push(`- ${findingBody(f)}${anchor ? ` \`${anchor}\`` : ''}`)
    }
    lines.push('')
  }

  lines.push(POSTURE_FOOTER)
  // Hidden fingerprints so re-runs dedup against this comment.
  for (const fp of fingerprints) lines.push(`<!-- aether-review-fp:${fp} -->`)
  return lines.join('\n')
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((err) => {
    console.error(err)
    process.exit(1)
  })
}
