// Run wasm-bindgen's real browser suite using the same pinned browser as E2E.
// No separate WebDriver/browser version pairing or silent live-test early returns.
import { spawn, spawnSync } from 'node:child_process';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { until } from './clients.mjs';

await mkdir('qa/results', { recursive: true });
const build = spawnSync('cargo', ['test', '-p', 'mistlib-wasm', '--target', 'wasm32-unknown-unknown',
  '--lib', '--no-run', '--locked', '--message-format=json'], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
await writeFile('qa/results/wasm-build.log', build.stdout + build.stderr);
assert.equal(build.status, 0, `WASM test build failed: ${build.stderr}`);
const artifacts = build.stdout.split('\n').filter(Boolean).map(line => JSON.parse(line))
  .filter(value => value.reason === 'compiler-artifact' && value.profile.test && value.executable?.endsWith('.wasm'));
assert.equal(artifacts.length, 1, 'expected one WASM lib test executable');
const runner = process.env.WASM_BINDGEN_TEST_RUNNER || 'wasm-bindgen-test-runner';
const lock = await readFile('Cargo.lock', 'utf8');
const version = lock.match(/name = "wasm-bindgen"\nversion = "([^"]+)"/)[1];
const installed = spawnSync(runner, ['--version'], { encoding: 'utf8' });
assert.equal(installed.status, 0, 'wasm-bindgen-test-runner must be installed');
assert(installed.stdout.trim().endsWith(version), 'runner must match Cargo.lock');
const browser = await chromium.launch({ headless: true });
const results = [];
try {
  for (const [filter, minimum] of [['transport::webrtc', 7], ['app::tests', 12], ['signaling::nostr::tests', 22]]) {
    const child = spawn(runner, [artifacts[0].executable, filter, '--skip', 'nostr_live_'], {
      env: { ...process.env, NO_HEADLESS: '1', WASM_BINDGEN_TEST_ADDRESS: '127.0.0.1:0' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let log = '', failed;
    child.stdout.on('data', data => { log += data; });
    child.stderr.on('data', data => { log += data; });
    child.on('error', error => { failed = error; });
    child.on('exit', code => { failed = new Error(`runner exited before completion: ${code}`); });
    const context = await browser.newContext();
    const group = { filter, minimum, passed: false };
    results.push(group);
    try {
      const url = await until(() => {
        if (failed) throw failed;
        return log.match(/http:\/\/(?:127\.0\.0\.1|localhost):\d+\/?/)?.[0];
      }, Boolean, 30000, `WASM runner startup: ${filter}`);
      const page = await context.newPage();
      await page.goto(url);
      const body = await until(() => page.textContent('body'), text => text?.includes('test result:'),
        120000, `WASM browser completion: ${filter}`);
      log += '\n' + body;
      const summary = body.match(/test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored/);
      assert(summary && summary[1] === 'ok' && Number(summary[2]) >= minimum &&
        Number(summary[3]) === 0 && Number(summary[4]) === 0, `failed, skipped, or empty WASM group: ${body}`);
      group.executed = Number(summary[2]); group.passed = true;
    } finally {
      await context.close(); child.kill('SIGKILL');
      await writeFile(`qa/results/wasm-${filter.split('::')[0]}.log`, log);
    }
  }
} finally {
  await browser.close();
  await writeFile('qa/results/wasm-regressions.json', JSON.stringify({ schema: 1, groups: results }, null, 2));
}
