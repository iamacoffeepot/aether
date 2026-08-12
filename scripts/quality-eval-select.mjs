#!/usr/bin/env node
// Sample selector for the weekly offline quality eval (#3380). Enumerate merged
// PRs from a trailing window that closed a code-bearing issue, resolve each PR's
// closing squash commit and its PARENT SHA, and — at selection time — capture
// the closing issue's normalized Plan routing and body. Deterministically sample
// N and emit one JSON record per line:
//
//   {issue, pr, squash_sha, parent_sha, model, size, issue_body}
//
// `parent_sha` is where the blind runner clones (the pre-merge trunk tip);
// `squash_sha` is the landed "ground truth" the judge compares against and the
// contamination assert forbids in the scratch clone. Routing and `issue_body`
// are captured here because no later stage produces them — the runner needs the
// body as its task input, and the judge groups verdict rates by size and model.
//
// Inputs (env):
//   GITHUB_TOKEN | GH_TOKEN     GitHub API token (contents+issues read)
//   GITHUB_REPOSITORY           owner/repo (default iamacoffeepot/aether)
//   GITHUB_SERVER_URL           API-host origin (default https://github.com)
//   QUALITY_EVAL_SAMPLE_SIZE    how many to sample (default 5)
//   QUALITY_EVAL_WINDOW_DAYS    trailing merge window in days (default 7)
//
// No external deps — Node built-ins + global fetch only. The windowing and
// sampling logic is exported pure for scripts/quality-eval.test.mjs.

import { createHash } from 'node:crypto'
import { pathToFileURL } from 'node:url'

const API = 'https://api.github.com'
const DAY_MILLIS = 86_400_000

// Merged-within-window filter, pure so the test can pin `nowMillis`. A PR counts
// when it has a `merged_at` at or after (nowMillis - windowDays*day) and at or
// before nowMillis — an un-merged (closed-only) PR carries a null `merged_at` and
// is excluded.
export function withinTrailingWindow(prs, nowMillis, windowDays) {
  const start = nowMillis - windowDays * DAY_MILLIS
  return prs.filter((pr) => {
    if (!pr.merged_at) return false
    const merged = Date.parse(pr.merged_at)
    return Number.isFinite(merged) && merged >= start && merged <= nowMillis
  })
}

// Deterministic sample of `n` items: order by a stable hash of each item's key
// and take the first `n`. Deterministic so a re-run over the same candidate set
// reproduces the same sample (re-runnable from a fixed SHA list), and spread
// rather than recency-biased because the hash order is independent of merge
// time. Returns a copy; when n >= length every item is returned (hash-ordered).
export function deterministicSample(items, n, keyFn) {
  const size = Math.max(0, Math.floor(n))
  return [...items]
    .map((item) => ({ item, key: createHash('sha256').update(String(keyFn(item))).digest('hex') }))
    .sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0))
    .slice(0, size)
    .map((w) => w.item)
}

// The closing issue a PR body declares. The PR template writes `Closes #<n>.`;
// GitHub also links `fixes`/`resolves`. Returns the first referenced number, or
// null when the body links no closing issue (skip — nothing to re-implement).
export function parseClosingIssue(body) {
  const m = String(body || '').match(/\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s+#(\d+)/i)
  return m ? Number(m[1]) : null
}

// A squash commit is code-bearing when it touched at least one crate source
// file — a pure docs / workflow / script PR is not a meaningful re-implement
// target for a correctness eval.
export function isCodeBearing(paths) {
  return (paths || []).some((p) => /^crates\/.+\.rs$/.test(p))
}

// Parse the canonical routing triplet from the managed Plan section. Routing is
// eligible only when exactly one section exists and its final three non-empty
// lines are the sole size/model/reason fields in that section. No label or
// default fallback is permitted: malformed or misplaced routing returns null.
export function parsePlanRouting(body) {
  const lines = String(body || '')
    .replace(/\r\n?/g, '\n')
    .split('\n')
  const headings = lines.flatMap((line, index) => (line === '## Implementation plan' ? [index] : []))
  if (headings.length !== 1) return null

  const start = headings[0] + 1
  const nextHeading = lines.findIndex((line, index) => index >= start && /^##\s+/.test(line))
  const section = lines.slice(start, nextHeading === -1 ? lines.length : nextHeading).filter((line) => line.trim() !== '')

  const sizeFields = section.filter((line) => line.trimStart().startsWith('**Size:**'))
  const modelFields = section.filter((line) => line.trimStart().startsWith('**Implementation model:**'))
  const reasonFields = section.filter((line) => line.trimStart().startsWith('**Routing reason:**'))
  if (sizeFields.length !== 1 || modelFields.length !== 1 || reasonFields.length !== 1 || section.length < 3) return null
  if (section.at(-3) !== sizeFields[0] || section.at(-2) !== modelFields[0] || section.at(-1) !== reasonFields[0]) return null

  const size = sizeFields[0].match(/^\*\*Size:\*\* (s|m|l)$/)
  const model = modelFields[0].match(/^\*\*Implementation model:\*\* (haiku|sonnet|opus)$/)
  const reason = reasonFields[0].match(/^\*\*Routing reason:\*\* (\S.*)$/)
  return size && model && reason ? { size: size[1], model: model[1] } : null
}

function requireEnv(name) {
  const v = process.env[name]
  if (!v) throw new Error(`quality-eval-select: ${name} is required`)
  return v
}

async function api(token, path) {
  const url = path.startsWith('http') ? path : `${API}/${path}`
  const res = await fetch(url, {
    headers: {
      authorization: `Bearer ${token}`,
      accept: 'application/vnd.github+json',
      'x-github-api-version': '2022-11-28',
      'user-agent': 'aether-quality-eval-select',
    },
  })
  if (!res.ok) {
    const err = new Error(`GET ${url} -> ${res.status} ${await res.text()}`)
    err.status = res.status
    throw err
  }
  return { data: await res.json(), link: res.headers.get('link') }
}

function nextLink(link) {
  if (!link) return null
  for (const part of link.split(',')) {
    const m = part.match(/<([^>]+)>;\s*rel="next"/)
    if (m) return m[1]
  }
  return null
}

// A set-but-garbage env var must throw, not flow NaN into the window arithmetic.
function intEnv(name, fallback) {
  const raw = process.env[name]
  if (raw === undefined || raw === '') return fallback
  const n = Number(raw)
  if (!Number.isInteger(n) || n < 1) throw new Error(name + ' must be a positive integer, got: ' + raw)
  return n
}

async function main() {
  const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN || requireEnv('GITHUB_TOKEN')
  const repo = process.env.GITHUB_REPOSITORY || 'iamacoffeepot/aether'
  const sampleSize = intEnv('QUALITY_EVAL_SAMPLE_SIZE', 5)
  const windowDays = intEnv('QUALITY_EVAL_WINDOW_DAYS', 7)
  const nowMillis = Date.now()
  const windowStart = nowMillis - windowDays * DAY_MILLIS

  // Walk closed PRs newest-updated-first, stopping once a whole page predates
  // the window. Cap the walk so a quiet repo can't spin the API indefinitely.
  const merged = []
  let path = `repos/${repo}/pulls?state=closed&base=main&sort=updated&direction=desc&per_page=100`
  for (let page = 0; page < 8 && path; page++) {
    const { data, link } = await api(token, path)
    if (!Array.isArray(data) || data.length === 0) break
    merged.push(...withinTrailingWindow(data, nowMillis, windowDays))
    const oldestUpdated = data.map((pr) => Date.parse(pr.updated_at)).filter(Number.isFinite).sort((a, b) => a - b)[0]
    if (oldestUpdated !== undefined && oldestUpdated < windowStart) break
    path = nextLink(link)
  }

  const candidates = []
  const seenIssues = new Set()
  for (const pr of merged) {
    if (!pr.merge_commit_sha) continue
    const issueNumber = parseClosingIssue(pr.body)
    if (!issueNumber || seenIssues.has(issueNumber)) continue

    let commit
    try {
      commit = (await api(token, `repos/${repo}/commits/${pr.merge_commit_sha}`)).data
    } catch (err) {
      if (err.status === 404) {
        console.error(`skip PR #${pr.number}: squash commit gone — ${err.message}`)
        continue
      }
      throw err // systemic (auth/rate-limit/network) — abort rather than skip-spam an empty sample
    }
    const parentSha = commit.parents && commit.parents[0] && commit.parents[0].sha
    const paths = (commit.files || []).map((f) => f.filename)
    if (!parentSha || !isCodeBearing(paths)) continue

    let issue
    try {
      issue = (await api(token, `repos/${repo}/issues/${issueNumber}`)).data
    } catch (err) {
      if (err.status === 404) {
        console.error(`skip #${issueNumber}: closing issue gone — ${err.message}`)
        continue
      }
      throw err // systemic — abort rather than skip-spam an empty sample
    }
    if (issue.pull_request) continue // the reference resolved to a PR, not an issue

    const routing = parsePlanRouting(issue.body)
    if (!routing) {
      console.error(`skip #${issueNumber}: missing or invalid final Plan routing triplet`)
      continue
    }
    seenIssues.add(issueNumber)
    candidates.push({
      issue: issueNumber,
      pr: pr.number,
      squash_sha: pr.merge_commit_sha,
      parent_sha: parentSha,
      model: routing.model,
      size: routing.size,
      issue_body: issue.body || '',
    })
  }

  const sample = deterministicSample(candidates, sampleSize, (c) => c.squash_sha)
  console.error(`quality-eval-select: ${candidates.length} code-bearing candidate(s) in the trailing ${windowDays}d, sampling ${sample.length}`)
  for (const record of sample) process.stdout.write(`${JSON.stringify(record)}\n`)
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((err) => {
    console.error(err)
    process.exit(1)
  })
}
