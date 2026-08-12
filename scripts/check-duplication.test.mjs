import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

const GATE = fileURLToPath(new URL('./check-duplication.mjs', import.meta.url))

const WELL_FORMED = JSON.stringify({ format: ['rust'], minTokens: 150, threshold: 0.5, reporters: ['consoleFull'] })

/** A throwaway tree with the given `.jscpd.json` text and a `crates/` to scan. */
function tree({ config, scanPath = 'crates' } = {}) {
  const root = mkdtempSync(join(tmpdir(), 'aether-check-duplication-'))
  if (config !== undefined) {
    writeFileSync(join(root, '.jscpd.json'), config)
  }
  mkdirSync(join(root, scanPath), { recursive: true })
  return root
}

/** The gate run against `root`, with `npx` deliberately unreachable so a run
 * that gets past the preflight fails on the spawn rather than on the network. */
function gate(root, ...paths) {
  const empty = mkdtempSync(join(tmpdir(), 'aether-empty-path-'))
  const run = spawnSync(process.execPath, [GATE, ...paths], { cwd: root, encoding: 'utf8', env: { PATH: empty } })
  return { status: run.status, stderr: run.stderr }
}

test('a .jscpd.json that does not parse fails the gate', () => {
  // Tripwire: jscpd answers a malformed config with one stderr warning, a
  // silent fall back to built-in defaults — threshold included — and exit 0.
  // The gate must be strictly stricter than the detector's own recovering
  // reader, so swapping this check for a lenient parse, or dropping it,
  // restores a green gate over a scan whose threshold no longer exists.
  const broken = gate(tree({ config: `${WELL_FORMED.slice(0, -1)},}` }), 'crates')

  assert.notEqual(broken.status, 0)
  assert.match(broken.stderr, /does not parse as JSON/)
})

test('a missing .jscpd.json fails the gate', () => {
  // Same silent-defaults fall back, reached by deleting the file rather than
  // by breaking its syntax: jscpd needs no config to run and reports no
  // threshold breach when it has none.
  const bare = gate(tree(), 'crates')

  assert.notEqual(bare.status, 0)
  assert.match(bare.stderr, /is missing/)
})

test('a scan path that does not exist fails the gate', () => {
  // Tripwire: jscpd analyzes 0 files under a path that is not there and exits
  // 0, which reads identically to a clean tree. A renamed or mistyped scan
  // root must turn the gate red rather than retire it.
  const absent = gate(tree({ config: WELL_FORMED }), 'crates', 'no-such-directory')

  assert.notEqual(absent.status, 0)
  assert.match(absent.stderr, /nothing to scan at no-such-directory/)
})

test('a config path entry that does not exist fails the gate', () => {
  // Tripwire: an argument overrides the config's own `path` array, so jscpd
  // scans the right tree and reports nothing wrong while the config names a
  // root that is not there. That entry is one dropped argument away from being
  // the entire scan, and it analyzes 0 files at exit 0 when it becomes one.
  const stale = gate(tree({ config: JSON.stringify({ ...JSON.parse(WELL_FORMED), path: ['no-such-directory'] }) }), 'crates')

  assert.notEqual(stale.status, 0)
  assert.match(stale.stderr, /nothing to scan at no-such-directory/)
})

test('a well-formed config over a real path reaches the detector', () => {
  // The positive control the three refusals above need: without it a gate that
  // rejected every tree would satisfy all of them while checking nothing. The
  // run gets past the preflight and dies on the unreachable `npx`, which is a
  // statement about this harness rather than about the tree.
  const reached = gate(tree({ config: WELL_FORMED }), 'crates')

  assert.match(reached.stderr, /ENOENT/)
  assert.doesNotMatch(reached.stderr, /does not parse|is missing|nothing to scan/)
})
