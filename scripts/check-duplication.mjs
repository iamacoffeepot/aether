#!/usr/bin/env node
// The `Duplicate code` gate: jscpd over the paths named as arguments, wrapped
// in the checks that make a green result mean the scan actually happened.
//
// jscpd exits 0 whether it examined every file or none of them, so its exit
// code alone cannot carry the gate (#4856). A malformed `.jscpd.json` is one
// warning on stderr and a silent fall back to built-in defaults — no format
// list, no `minTokens`, and above all no threshold, so clone detection stops
// failing on duplication at exactly the moment its config broke. A scan path
// that does not exist analyzes 0 files and exits just as cleanly. Both are the
// gate passing *because* it failed, so each is checked here rather than
// inferred from a status that cannot express it.
//
// Run it the way the workflow and the `verify.dup` lane do:
//
//   node scripts/check-duplication.mjs crates

import { spawnSync } from 'node:child_process'
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'

// The pinned detector, named once so the workflow job and the `verify.dup`
// lane cannot drift onto different jscpd releases.
const JSCPD = 'jscpd@5.0.12'

// The config jscpd reads out of the working directory, and the single source
// of the gate's `minTokens` and duplicated-line threshold.
const CONFIG = '.jscpd.json'

// The report jscpd's `json` reporter writes into its output directory. Its
// `statistics.total.sources` is the count of files the run analyzed — the one
// number that separates "found no clones" from "looked at nothing".
const REPORT = 'jscpd-report.json'

/**
 * The parsed `.jscpd.json`, or a throw naming why no run against it can be
 * trusted.
 *
 * `JSON.parse` rather than the error-recovering reader jscpd itself uses:
 * strict is the direction a gate has to fail in. jscpd survives a broken
 * config by discarding it and scanning with defaults, so anything its reader
 * merely warns about has already cost the run its threshold.
 */
function loadConfig() {
  if (!existsSync(CONFIG)) {
    throw new Error(`${CONFIG} is missing — jscpd would scan with built-in defaults and no threshold at all`)
  }

  try {
    return JSON.parse(readFileSync(CONFIG, 'utf8'))
  } catch (error) {
    throw new Error(
      `${CONFIG} does not parse as JSON (${error.message}) — jscpd would warn, discard it, `
      + 'and scan with built-in defaults, threshold included',
    )
  }
}

/**
 * The reporters the run asks for: the ones the config already names, plus the
 * `json` one this wrapper reads its scanned-file count from. Derived from the
 * config rather than pinned here, so console output stays whatever
 * `.jscpd.json` chose.
 */
function reporters(config) {
  const configured = Array.isArray(config.reporters) ? config.reporters : []
  return [...new Set([...configured, 'json'])].join(',')
}

/**
 * How many files the finished run analyzed, from the report it wrote.
 *
 * A report that is absent or names no count is itself a failure: it leaves the
 * scan unwitnessed, which is the state this wrapper exists to refuse.
 */
function scannedFileCount(output) {
  const path = join(output, REPORT)
  if (!existsSync(path)) {
    throw new Error(`jscpd wrote no ${REPORT} to ${output}, so nothing witnesses that it scanned anything`)
  }

  const sources = JSON.parse(readFileSync(path, 'utf8'))?.statistics?.total?.sources
  if (typeof sources !== 'number') {
    throw new Error(`${REPORT} names no statistics.total.sources, so nothing states how many files were scanned`)
  }
  return sources
}

function main() {
  const paths = process.argv.slice(2)
  if (paths.length === 0) {
    throw new Error('usage: check-duplication.mjs <path> [path…]')
  }

  const config = loadConfig()

  // Both the roots the argument list names and any the config declares. A
  // positional argument overrides a config `path` array, so a stale entry
  // there scans nothing today and becomes the whole scan the day the argument
  // is dropped — a root that does not exist has no reading under which it is
  // fine.
  const declared = Array.isArray(config.path) ? config.path : []
  const absent = [...new Set([...paths, ...declared])].filter((path) => !existsSync(path))
  if (absent.length > 0) {
    throw new Error(`nothing to scan at ${absent.join(', ')} — jscpd reports zero clones over a path that is not there`)
  }

  const output = mkdtempSync(join(tmpdir(), 'aether-jscpd-'))
  try {
    const argv = ['--yes', JSCPD, '--reporters', reporters(config), '--output', output, ...paths]
    const run = spawnSync('npx', argv, { stdio: 'inherit' })
    if (run.error) {
      throw run.error
    }

    const scanned = scannedFileCount(output)
    if (scanned === 0) {
      throw new Error(
        `jscpd analyzed 0 files under ${paths.join(', ')} — a clean duplicate-code result over a scan `
        + 'that examined nothing is not a pass',
      )
    }

    process.stdout.write(`jscpd analyzed ${scanned} files under ${paths.join(', ')}\n`)
    // The detector's own verdict, once the run is known to have happened: 1 is
    // the duplicated-line threshold exceeded. A child that died on a signal has
    // no status, and an unwitnessed verdict fails closed.
    process.exitCode = run.status ?? 1
  } finally {
    rmSync(output, { recursive: true, force: true })
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    main()
  } catch (error) {
    console.error(`check-duplication: ${error.message}`)
    process.exitCode = 1
  }
}
