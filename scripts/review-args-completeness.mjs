#!/usr/bin/env node
// Deterministic completeness self-check for direct-drive five-pillar review's
// caller-side diff acquisition (#3608).
//
// The review session assembles the workflow arg contract ({files, testFiles,
// diffs}) itself, from a `git diff` the model drives. On PR #3600 that
// acquisition silently dropped two changed test files before the spec agent
// saw them — a changed file whose hunk never reaches `args.diffs` shows to the
// finders as a bare filename with no diff, which reads as "unchanged" — and the
// deep pass then fabricated a high-severity "no test changes" under-delivery
// finding against a diff whose test files were present throughout. The mirror
// failure is silent: a critic blind to a changed file can also APPROVE a PR
// while never reviewing code that needed it.
//
// This check closes the silent-drop hole by comparing the AUTHORITATIVE changed
// reviewable-file set — computed by the caller from the PR's file list — against
// the args the session assembled. Every authoritative file must appear in
// `files ∪ testFiles`
// AND carry a non-empty `diffs` entry. Any gap is a loud failure: the session
// must NOT invoke the local review engine and must NOT write a rollup, leaving
// the failure in the transcript rather than producing a silent partial review.
//
// Usage (CLI):
//   node scripts/review-args-completeness.mjs <reviewable-list> <args-json>
//     <reviewable-list>  path to the authoritative reviewable set — a JSON array
//                        of paths, or a newline-separated list.
//     <args-json>        path to the assembled workflow args, a JSON object with
//                        at least {files, testFiles?, diffs}.
//   GITHUB_WORKSPACE (env, optional) — repo root; args paths are normalized
//                        relative to it so an absolute `files` entry matches a
//                        repo-relative authoritative path.
// Exit 0 + a one-line summary on a complete set; exit 1 + a machine-readable
// gap list (JSON, to stderr) on any gap.

import { readFile } from 'node:fs/promises'
import { pathToFileURL } from 'node:url'

// Strip an optional repo-root prefix so an absolute args path
// (`${GITHUB_WORKSPACE}/crates/foo.rs`) compares equal to the repo-relative
// authoritative path (`crates/foo.rs`). A path that does not start with the
// prefix is returned unchanged (already relative, or under a different root).
export function toRepoRelative(path, repoRoot = '') {
  if (!repoRoot) return path
  const prefix = repoRoot.endsWith('/') ? repoRoot : `${repoRoot}/`
  return path.startsWith(prefix) ? path.slice(prefix.length) : path
}

// Report every authoritative reviewable file that the assembled args fail to
// cover completely. A file is complete iff it appears in `files ∪ testFiles`
// AND its `diffs` entry is a non-empty string. Returns an array of
// { file, reason } gaps; an empty array means the args are complete.
export function findGaps(reviewable, args = {}, repoRoot = '') {
  const files = Array.isArray(args.files) ? args.files : []
  const testFiles = Array.isArray(args.testFiles) ? args.testFiles : []
  const diffs = args.diffs && typeof args.diffs === 'object' ? args.diffs : {}

  const covered = new Set([...files, ...testFiles].map((p) => toRepoRelative(p, repoRoot)))
  const diffByRel = new Map(Object.entries(diffs).map(([k, v]) => [toRepoRelative(k, repoRoot), v]))

  const gaps = []
  for (const raw of reviewable) {
    const file = toRepoRelative(raw, repoRoot)
    if (!covered.has(file)) {
      gaps.push({ file, reason: 'absent-from-files' })
      continue
    }
    const diff = diffByRel.get(file)
    if (diff === undefined || diff === null) {
      gaps.push({ file, reason: 'missing-diff' })
      continue
    }
    if (typeof diff !== 'string' || diff.trim() === '') {
      gaps.push({ file, reason: 'empty-diff' })
    }
  }
  return gaps
}

// Parse the authoritative reviewable set from a file's contents: a JSON array
// of paths, or a newline-separated list. Blank lines are dropped.
export function parseReviewable(text) {
  const trimmed = text.trim()
  if (trimmed.startsWith('[')) {
    const parsed = JSON.parse(trimmed)
    if (!Array.isArray(parsed)) throw new Error('reviewable list JSON must be an array of paths')
    return parsed.filter((p) => typeof p === 'string' && p.length > 0)
  }
  return trimmed.split('\n').map((l) => l.trim()).filter((l) => l.length > 0)
}

async function main() {
  const [reviewablePath, argsPath] = process.argv.slice(2)
  if (!reviewablePath || !argsPath) {
    console.error('usage: review-args-completeness.mjs <reviewable-list> <args-json>')
    process.exit(2)
  }
  const reviewable = parseReviewable(await readFile(reviewablePath, 'utf8'))
  const args = JSON.parse(await readFile(argsPath, 'utf8'))
  const repoRoot = process.env.GITHUB_WORKSPACE || ''

  const gaps = findGaps(reviewable, args, repoRoot)
  if (gaps.length) {
    console.error(`review args INCOMPLETE — ${gaps.length} changed reviewable file(s) dropped before the model:`)
    console.error(JSON.stringify(gaps, null, 2))
    console.error('Do NOT invoke the review engine and do NOT write a rollup — leave this failure in the transcript.')
    process.exit(1)
  }
  console.log(`review args complete: ${reviewable.length} reviewable file(s) all present with non-empty diffs.`)
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((err) => {
    console.error(err)
    process.exit(1)
  })
}
