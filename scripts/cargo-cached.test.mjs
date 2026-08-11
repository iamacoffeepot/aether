import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const WRAPPER = join(dirname(fileURLToPath(import.meta.url)), 'cargo-cached.sh');

function command(command, args, cwd, env = {}) {
    return execFileSync(command, args, { cwd, env: { ...process.env, ...env }, encoding: 'utf8' });
}

function writeExecutable(path, contents) {
    writeFileSync(path, contents);
    chmodSync(path, 0o755);
}

function setupWorktrees() {
    const root = mkdtempSync(join(tmpdir(), 'cargo-cached-'));
    const bin = join(root, 'bin');
    const first = join(root, 'first');
    const second = join(root, 'second');
    const log = join(root, 'cargo-log.jsonl');
    mkdirSync(bin);
    command('git', ['init', '--quiet', first], root);
    command('git', ['config', 'user.email', 'test@example.invalid'], first);
    command('git', ['config', 'user.name', 'cargo-cached test'], first);
    writeFileSync(join(first, 'README.md'), 'fixture\n');
    command('git', ['add', 'README.md'], first);
    command('git', ['commit', '--quiet', '-m', 'fixture'], first);
    command('git', ['worktree', 'add', '--quiet', second], first);
    writeExecutable(join(bin, 'sccache'), '#!/usr/bin/env sh\nexit 0\n');
    writeExecutable(join(bin, 'cargo'), `#!/usr/bin/env node
const fs = require('node:fs');
fs.appendFileSync(process.env.FAKE_CARGO_LOG, JSON.stringify({
  args: process.argv.slice(2),
  target: process.env.CARGO_TARGET_DIR,
  wrapper: process.env.RUSTC_WRAPPER,
  incremental: process.env.CARGO_INCREMENTAL,
}) + '\\n');
process.exit(Number(process.env.FAKE_CARGO_EXIT || 0));
`);
    return { root, bin, first, second, log };
}

function wrapperEnv(fixture, extra = {}) {
    return {
        ...extra,
        PATH: `${fixture.bin}:${process.env.PATH}`,
        FAKE_CARGO_LOG: fixture.log,
    };
}

function records(log) {
    return readFileSync(log, 'utf8').trim().split('\n').map((line) => JSON.parse(line));
}

test('direct wrapper invocation preserves arguments, isolates worktrees, overrides ambient values, and propagates cargo exits', (t) => {
    const fixture = setupWorktrees();
    t.after(() => rmSync(fixture.root, { recursive: true, force: true }));

    command(WRAPPER, ['check', '-p', 'example', '--features', 'fast mode'], fixture.first, wrapperEnv(fixture, {
        CARGO_TARGET_DIR: '/ambient/target',
        RUSTC_WRAPPER: '/ambient/wrapper',
        CARGO_INCREMENTAL: '1',
    }));
    command(WRAPPER, ['test'], fixture.second, wrapperEnv(fixture));

    const [first, second] = records(fixture.log);
    assert.deepEqual(first.args, ['check', '-p', 'example', '--features', 'fast mode']);
    assert.equal(first.target, `${realpathSync(fixture.first)}/target`);
    assert.equal(second.target, `${realpathSync(fixture.second)}/target`);
    assert.notEqual(first.target, second.target);
    assert.equal(first.wrapper, join(fixture.bin, 'sccache'));
    assert.ok(first.wrapper.startsWith('/'));
    assert.equal(first.incremental, '0');

    const failed = spawnSync(WRAPPER, ['check'], {
        cwd: fixture.first,
        env: wrapperEnv(fixture, { FAKE_CARGO_EXIT: '17' }),
        encoding: 'utf8',
    });
    assert.equal(failed.status, 17);
});

test('direct wrapper invocation reports its Git and sccache prerequisites', (t) => {
    const outside = mkdtempSync(join(tmpdir(), 'cargo-cached-outside-'));
    t.after(() => rmSync(outside, { recursive: true, force: true }));
    const outsideGit = spawnSync(WRAPPER, ['check'], { cwd: outside, encoding: 'utf8' });
    assert.equal(outsideGit.status, 2);
    assert.match(outsideGit.stderr, /scripts\/cargo-cached\.sh must run inside a Git worktree/);

    const fixture = setupWorktrees();
    t.after(() => rmSync(fixture.root, { recursive: true, force: true }));
    const noSccache = spawnSync(WRAPPER, ['check'], {
        cwd: fixture.first,
        env: { ...process.env, PATH: '/usr/bin:/bin' },
        encoding: 'utf8',
    });
    assert.equal(noSccache.status, 2);
    assert.match(noSccache.stderr, /scripts\/cargo-cached\.sh requires sccache on PATH/);
});
