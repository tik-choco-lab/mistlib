// Research-only runner. Does not consume or produce release gate evidence.
import assert from 'node:assert/strict';
import { randomUUID, createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFile, mkdir, appendFile, access, writeFile } from 'node:fs/promises';
import { openSync, closeSync, unlinkSync, writeFileSync, statSync } from 'node:fs';
import { createServer } from 'node:http';
import { resolve, join, dirname, sep } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { nativeClient, browserClient, until } from './clients.mjs';
import { mergeSeries, deriveMetrics } from './storage_metrics.mjs';

export const ROOT = fileURLToPath(new URL('../', import.meta.url));
export const profiles = JSON.parse(await readFile(new URL('./storage_profiles.json', import.meta.url)));
export const pairs = ['chromium-chromium', 'firefox-firefox', 'chromium-firefox', 'native-browser', 'browser-native'];

export function deepMerge(base, override) {
  const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
  assert(object(override), 'config override must be a JSON object');
  const merged = structuredClone(base);
  for (const [key, value] of Object.entries(override)) {
    assert(!['__proto__', 'constructor', 'prototype'].includes(key), 'unsafe config key');
    merged[key] = object(value) ? deepMerge(object(merged[key]) ? merged[key] : {}, value) : structuredClone(value);
  }
  return merged;
}

export function parseSize(value) {
  const match = /^(\d+(?:\.\d+)?)\s*(B|KiB|MiB|GiB|KB|MB|GB)?$/i.exec(value);
  assert(match, `invalid size: ${value}`);
  const units = { b: 1, kib: 1024, mib: 1024 ** 2, gib: 1024 ** 3, kb: 1000, mb: 1e6, gb: 1e9 };
  const size = Number(match[1]) * units[(match[2] || 'b').toLowerCase()];
  assert(Number.isSafeInteger(size) && size > 0, 'size must be a positive integer number of bytes');
  return size;
}

export function deadlineMs(size, profile) {
  return Math.max(90000, profile.rateKbit ? 3 * size * 8 / profile.rateKbit + 30000 : 90000);
}

function selection(value, choices) {
  const selected = value === 'all' ? choices : value.split(',');
  assert(selected.length && selected.every(item => choices.includes(item)), `choose from: ${choices.join(',')}`);
  return [...new Set(selected)];
}

export function cells(args) {
  const result = [];
  const repeat = Number(args.repeat || 1);
  assert(Number.isInteger(repeat) && repeat > 0, 'repeat must be a positive integer');
  assert(['chromium', 'firefox'].includes(args.browser || 'chromium'), 'unsupported mixed browser');
  const seed = Number(args.seed ?? 0x19a50001);
  assert(Number.isInteger(seed) && seed > 0 && seed <= 0xffffffff, 'seed must be a nonzero uint32');
  assert(['forward', 'reverse', 'both'].includes(args.direction || 'both'), 'invalid direction');
  const repetition = Number(args.repetition || 1);
  assert(Number.isInteger(repetition) && repetition > 0, 'repetition must be positive');
  for (let r = 0; r < repeat; r++) {
    let index = 0;
    for (const pair of selection(args.pair || 'all', pairs)) {
      const ends = pair.split('-').map(kind => kind === 'browser' ? (args.browser || 'chromium') : kind);
      const orders = pair.includes('native') ? ['forward'] : ['forward', 'reverse'];
      for (const order of orders.filter(order => !args.direction || args.direction === 'both' || args.direction === order)) {
        const kinds = order === 'forward' ? ends : [...ends].reverse();
        for (const profile of selection(args.profile || 'clean', Object.keys(profiles))) {
          for (const sizeText of (args.size || '4MiB').split(',')) {
            result.push({ pair, direction: kinds.join('->'), order, kinds, profile, size: parseSize(sizeText),
              seed: ((seed + r * 100003 + index++) >>> 0) || 1, repetition: repetition + r });
          }
        }
      }
    }
  }
  assert(result.length, 'no matching directed transfers');
  return result;
}

function run(command, args, options = {}) {
  return execFileSync(command, args, { cwd: ROOT, stdio: ['ignore', 2, 2], ...options });
}

export async function prepare(args) {
  try { await access(join(ROOT, 'qa/node_modules/playwright/package.json')); }
  catch { run('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund'], { cwd: join(ROOT, 'qa') }); }
  if (!args['no-build'] && !args.artifacts) {
    const env = { ...process.env, MISTLIB_BUILD_COMMIT: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim() };
    delete env.MISTLIB_BUILD_DIRTY;
    run('cargo', ['build', '--release', '--locked', '-p', 'mistlib-native'], { env });
    run('wasm-pack', ['build', 'mistlib-wasm', '--target', 'web', '--release', '--', '--locked'], { env });
  }
  const root = resolve(args.artifacts || ROOT);
  const library = process.platform === 'win32' ? 'mistlib.dll' : process.platform === 'darwin' ? 'libmistlib.dylib' : 'libmistlib.so';
  async function find(candidates) {
    for (const candidate of candidates) {
      try { await access(candidate); return candidate; } catch { /* next conventional layout */ }
    }
    throw new Error(`missing built artifact; expected one of: ${candidates.join(', ')}`);
  }
  return {
    native: await find([join(root, 'target/release', library), join(root, library)]),
    pkg: await find([join(root, 'mistlib-wasm/pkg'), join(root, 'pkg')]),
  };
}

// The benchmark serves the already-built wasm-pack directory directly. The release
// suite deliberately retains its stricter npm-candidate installation/hosting path.
async function host(pkg) {
  const prefix = '/node_modules/@tik-choco/mistlib/';
  const server = createServer(async (req, res) => {
    try {
      const pathname = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
      if (pathname === '/') { res.setHeader('Content-Type', 'text/html'); return res.end('<!doctype html><title>Storage research</title>'); }
      if (!pathname.startsWith(prefix)) { res.writeHead(404); return res.end(); }
      const path = resolve(pkg, pathname.slice(prefix.length));
      if (!path.startsWith(pkg + sep)) { res.writeHead(403); return res.end(); }
      const bytes = await readFile(path);
      res.setHeader('Content-Type', path.endsWith('.wasm') ? 'application/wasm' : 'text/javascript');
      res.end(bytes);
    } catch { res.writeHead(404); res.end(); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return { url: `http://127.0.0.1:${server.address().port}`, close: () => new Promise(resolve => {
    server.close(resolve); server.closeAllConnections();
  }) };
}

const netem = fileURLToPath(new URL('./netem.sh', import.meta.url));
// lo and its qdisc belong to this network namespace, including in testbed slots.
const netns = process.platform === 'linux' ? statSync('/proc/self/ns/net').ino : 'host';
const lockPath = join(tmpdir(), `mistlib-storage-netem-${netns}.lock`);
let lockFd, shaped = false;
function cleanupShaping() {
  // Keep the lock if tc cleanup fails, so another run cannot inherit dirty shaping.
  if (shaped) { run('sudo', ['-n', netem, 'off']); shaped = false; }
  if (lockFd !== undefined) { closeSync(lockFd); unlinkSync(lockPath); lockFd = undefined; }
}
function shape(profile) {
  if (!profile.tcArgs.length) return;
  assert(process.platform === 'linux', 'shaping requires Linux tc and sudo');
  try { lockFd = openSync(lockPath, 'wx', 0o600); }
  catch { throw new Error(`shaped benchmark already running (lock ${lockPath}); refusing concurrent shaping`); }
  writeFileSync(lockFd, `${process.pid}\n`);
  // netem.sh uses add (never replace), and only removes its own root on error.
  run('sudo', ['-n', netem, 'on', ...profile.tcArgs]);
  shaped = true;
}

export async function main(argv = process.argv.slice(2)) {
  const { values: args } = parseArgs({ args: argv, options: {
    ...Object.fromEntries(['pair', 'profile', 'size', 'repeat', 'browser', 'out', 'timeout', 'config-override',
      'label', 'artifacts', 'seed', 'repetition', 'direction', 'max-seconds'].map(key => [key, { type: 'string' }])),
    ...Object.fromEntries(['no-build', 'dry-run', 'prepare-only', 'plan', 'help'].map(key => [key, { type: 'boolean' }])),
  } });
  if (args.help) {
    console.log(`Usage: just bench storage [--pair ${pairs.join('|')}|all]
  [--profile ${Object.keys(profiles).join('|')}|all] [--size 4MiB,32MiB] [--repeat N]
  [--browser chromium|firefox] [--no-build | --artifacts DIR] [--out FILE.jsonl]
  [--timeout auto|SECONDS] [--config-override '{"experimental":{}}'] [--label TEXT]
  [--seed UINT32] [--max-seconds N] [--dry-run]
Browser pairs run both directions with fresh stores; mixed pairs run the named direction.
Defaults: all pairs, clean, 4MiB, one repetition. --out appends attempts.`);
    return;
  }
  const plan = cells(args);
  const override = JSON.parse(args['config-override'] || '{}');
  deepMerge({}, override); // Validate before building or attempting a transfer.
  const timeout = args.timeout && args.timeout !== 'auto' ? Number(args.timeout) * 1000 : null;
  assert(timeout === null || (Number.isFinite(timeout) && timeout > 0), 'timeout is auto or seconds');
  const maxMs = args['max-seconds'] === undefined ? null : Number(args['max-seconds']) * 1000;
  assert(maxMs === null || (Number.isFinite(maxMs) && maxMs > 0), 'max-seconds must be positive seconds');
  if (args.plan) { console.log(JSON.stringify(plan)); return; }
  if (args['dry-run']) {
    for (const cell of plan) {
      const p = profiles[cell.profile];
      console.log(JSON.stringify({ ...cell, timeoutMs: timeout ?? deadlineMs(cell.size, p),
        commands: p.tcArgs.length ? [
          `sudo -n ${netem} on ${p.tcArgs.join(' ')}`,
          'tc qdisc add dev lo root handle 1: prio bands 3 priomap ' + Array(16).fill('0').join(' '),
          `tc qdisc add dev lo parent 1:3 handle 30: netem delay ${p.delayMs}ms rate ${p.rateKbit}kbit limit ${p.tcArgs[2]} ${p.tcArgs.slice(3).join(' ')}`.trim(),
          'tc filter add dev lo parent 1: protocol ip pref 1 u32 match ip protocol 17 0xff flowid 1:3',
          'tc filter add dev lo parent 1: protocol ipv6 pref 2 u32 match ip6 protocol 17 0xff flowid 1:3',
          `sudo -n ${netem} off`, 'tc qdisc del dev lo root',
        ] : [] }));
    }
    return;
  }
  const artifacts = await prepare(args);
  if (args['prepare-only']) return;
  const { startRelay } = await import('./relay.mjs');
  const playwright = await import('playwright');
  const identity = {
    rev: execFileSync('git', ['rev-parse', '--short', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim(),
    dirty: Boolean(execFileSync('git', ['status', '--porcelain'], { cwd: ROOT, encoding: 'utf8' }).trim()),
  };
  const hash = async path => createHash('sha256').update(await readFile(path)).digest('hex');
  const artifactHashes = { native: await hash(artifacts.native), wasm: await hash(join(artifacts.pkg, 'mistlib_wasm_bg.wasm')),
    js: await hash(join(artifacts.pkg, 'mistlib_wasm.js')) };
  const out = resolve(args.out || join(ROOT, 'qa/results', `storage-${Date.now()}-${process.pid}.jsonl`));
  await mkdir(dirname(out), { recursive: true });
  console.error(`Storage research rows: ${out}`);
  const abort = new AbortController();
  const interrupt = signal => abort.abort(new Error(`interrupted: ${signal}`));
  const sigint = () => interrupt('SIGINT'), sigterm = () => interrupt('SIGTERM');
  process.on('SIGINT', sigint); process.on('SIGTERM', sigterm);
  process.on('exit', cleanupShaping);
  let hosting;
  try {
    hosting = await host(artifacts.pkg);
    for (const cell of plan) {
      if (abort.signal.aborted) break;
      const p = profiles[cell.profile], nonce = randomUUID();
      const directory = join(out + '-logs', `${cell.repetition}-${cell.pair}-${cell.order}-${cell.profile}-${cell.size}-${nonce}`);
      await mkdir(directory, { recursive: true });
      const row = { schema: 1, id: nonce, ts: new Date().toISOString(), ...identity, label: args.label || '', configOverride: override,
        ...cell, browser: cell.pair.includes('native') ? (args.browser || 'chromium') : null,
        tcArgs: p.tcArgs, artifactHashes, buildInfo: [], browserVersions: [],
        timeoutMs: timeout ?? deadlineMs(cell.size, p), success: false, error: null,
        completionMs: null, getMs: null, verifyMs: null, verifiedBytes: null, goodputBps: 0,
        controlRttP50Ms: null, controlRttP95Ms: null, controlSamples: null, controlTimeouts: null, controlCensored: null,
        maxNoProgressMs: null, duplicateBytes: null, steadyGoodputBps: null, partialBytes: null,
        maxSeconds: maxMs === null ? null : maxMs / 1000, seriesPath: join(dirname(out), `${nonce}.series.json`), metricsComplete: false, artifacts: [] };
      const clients = [];
      let series = [];
      let relay, onAbort, capTimer, transferStarted, transferEnded, telemetryStarted = false, getStarted = false;
      const checkAbort = () => abort.signal.throwIfAborted();
      try {
        const attempt = async () => {
          checkAbort();
          shape(p);
          relay = await startRelay();
          checkAbort();
          const room = `storage-${nonce}`, ids = [`a-${nonce}`, `z-${nonce}`];
          const config = deepMerge({ iceServers: [], aoiRange: 100, signaling: { mode: 'nostr', nostr: {
            relays: [relay.url], inviteSalt: 'storage-research', inviteCode: nonce,
          } } }, override);
          for (let i = 0; i < 2; i++) {
            const name = `${i}-${cell.kinds[i]}`;
            const client = cell.kinds[i] === 'native' ? await nativeClient(artifacts.native, directory, name)
              : await browserClient(playwright[cell.kinds[i]], hosting.url, directory, name, { trace: false });
            clients.push(client);
            checkAbort();
            row.artifacts.push(join(directory, `${name}.log`));
            row.buildInfo.push(await client.info());
            row.browserVersions.push(client.browserVersion || null);
            await client.command({ op: 'init', node: ids[i], config });
            checkAbort();
          }
          await Promise.all(clients.map(c => c.command({ op: 'join', room })));
          checkAbort();
          await Promise.all(clients.map((c, i) => until(() => { checkAbort(); return c.events(); }, events => events.some(e =>
            e.kind === 'connected' && e.sender === ids[1 - i] && e.room === room), 30000, 'storage peers connected')));
          await Promise.all(clients.map((c, i) => c.command({ op: 'position', room, position: [i, 0, 0] })));
          // Transport callbacks precede the overlay's first topology tick.
          // Wait for actual peer visibility, otherwise a fast store can race
          // route installation and the resolver rejects WANT as unreachable.
          await Promise.all(clients.map((c, i) => until(() => { checkAbort(); return c.events(); }, events =>
            events.some(e => e.kind === 'aoi' && e.room === room &&
              JSON.parse(Buffer.from(e.data).toString()).some(node => node.id === ids[1 - i])),
          30000, 'storage overlay peers visible')));
          await Promise.all(clients.map((c, i) => c.command({ op: 'control-config', room, target: ids[1 - i], token: nonce })));
          checkAbort();
          const stored = await clients[0].command({ op: 'store-generated', size: cell.size, seed: cell.seed }, Math.max(30000, row.timeoutMs));
          checkAbort();
          assert.equal(stored.size, cell.size);
          const expected = { size: stored.size, sha256: stored.sha256 };
          const interrupted = new Promise((_, reject) => {
            onAbort = () => reject(abort.signal.reason);
            abort.signal.addEventListener('abort', onAbort, { once: true });
          });
          await Promise.all(clients.map(c => c.command({ op: 'telemetry-start' }, 5000)));
          telemetryStarted = true;
          getStarted = true;
          transferStarted = Date.now();
          const capped = new Promise((_, reject) => {
            if (maxMs !== null) capTimer = setTimeout(() => reject(new Error('max-seconds')), maxMs);
          });
          const result = await Promise.race([
            clients[1].command({ op: 'get-bench', cid: stored.cid, expected }, row.timeoutMs), interrupted, capped,
          ]);
          assert.equal(result.size, expected.size); assert.equal(result.sha256, expected.sha256);
          for (const key of ['completionMs', 'getMs', 'verifyMs', 'controlRttP50Ms', 'controlRttP95Ms',
            'controlSamples', 'controlTimeouts', 'controlCensored']) row[key] = result[key] ?? null;
          row.verifiedBytes = result.size;
          row.goodputBps = result.size * 8 / (result.completionMs / 1000);
          row.success = true;
        };
        // Allocations finish and register before checking the signal; only get
        // is raced, after all resources are registered for teardown.
        try { await attempt(); } finally {
          transferEnded = Date.now();
          clearTimeout(capTimer);
          if (telemetryStarted) {
            const samples = await Promise.allSettled(clients.map(c => c.command({ op: 'telemetry-stop' }, 5000)));
            const tx = samples[0].status === 'fulfilled' ? samples[0].value : [];
            const rx = samples[1].status === 'fulfilled' ? samples[1].value : [];
            series = mergeSeries(rx, tx, transferStarted, transferEnded);
            Object.assign(row, deriveMetrics(series, cell.size));
            if (!tx.length || tx.some(p => !p.storage)) row.metricsComplete = false;
          }
        }
      } catch (error) {
        row.error = error.message === 'max-seconds' ? 'max-seconds' : String(error.stack || error);
        process.exitCode = abort.signal.aborted ? 130 : 1;
        if (/size\/SHA-256 mismatch/.test(row.error)) row.verifiedBytes = 0;
        if (getStarted && clients[1]) {
          try { Object.assign(row, await clients[1].command({ op: 'control-stop' }, 1000)); } catch { /* unavailable */ }
        }
      } finally {
        if (onAbort) abort.signal.removeEventListener('abort', onAbort);
        for (const client of clients.reverse()) {
          try { await client.close(); } catch (error) { row.error = [row.error, `cleanup: ${error}`].filter(Boolean).join('\n'); row.success = false; process.exitCode = 1; }
        }
        if (relay) await relay.close();
        try { cleanupShaping(); }
        catch (error) { row.error = [row.error, `shaping cleanup: ${error}`].filter(Boolean).join('\n'); row.success = false; process.exitCode = 1; }
        if (!row.success) row.goodputBps = 0;
        await writeFile(row.seriesPath, JSON.stringify({ id: row.id, series }) + '\n');
        const line = JSON.stringify(row) + '\n';
        await appendFile(out, line);
        process.stdout.write(line);
      }
    }
  } finally {
    if (hosting) await hosting.close();
    cleanupShaping();
    process.removeListener('SIGINT', sigint); process.removeListener('SIGTERM', sigterm);
    process.removeListener('exit', cleanupShaping);
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
}
