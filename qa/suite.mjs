// The same public-API contract is run against every pair of consumers.
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { readFile, writeFile, mkdir, mkdtemp, rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { join, resolve, sep, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';
import { nativeClient, browserClient, until } from './clients.mjs';
import { startRelay } from './relay.mjs';

const harnessErrors = [];
process.on('unhandledRejection', error => {
  harnessErrors.push(String(error?.stack || error));
  process.exitCode = 1;
});

const { values: args } = parseArgs({ options: { ...Object.fromEntries(
  ['native', 'package', 'browser', 'target', 'out', 'commit', 'version', 'soak-seconds'].map(key => [key, { type: 'string' }])),
  'allow-dirty': { type: 'boolean', default: false } } });
for (const key of ['native', 'target', 'out', 'commit', 'version']) assert(args[key], `--${key} is required`);
const policy = JSON.parse(await readFile(new URL('./policy.json', import.meta.url)));
const soakSeconds = args['soak-seconds'] ? Number(args['soak-seconds']) : policy.soakSeconds;
assert(Number.isFinite(soakSeconds) && soakSeconds >= policy.soakSeconds, 'soak duration cannot shorten policy');
if (args.browser) {
  assert(policy.browsers.includes(args.browser), 'unsupported browser');
  assert(args.package, '--package is required with --browser');
} else assert(policy.nativeTargets.includes(args.target), 'unsupported native target');
const output = resolve(args.out);
await rm(output, { force: true });
await mkdir(dirname(output), { recursive: true });
const logs = output.replace(/\.json$/, '') + '-logs';
await mkdir(logs, { recursive: true });
const digest = async file => createHash('sha256').update(await readFile(file)).digest('hex');
const report = {
  schema: 1, id: args.browser ? `browser-${args.browser}` : `native-${args.target}`,
  commit: args.commit, version: args.version, policySha256: await digest(new URL('./policy.json', import.meta.url)),
  nativeSha256: await digest(args.native), packageSha256: args.package ? await digest(args.package) : null,
  platform: process.platform, architecture: process.arch, nodeVersion: process.version, startedAt: new Date().toISOString(),
  runId: process.env.GITHUB_RUN_ID || null, runAttempt: Number(process.env.GITHUB_RUN_ATTEMPT || 0),
  buildInfo: [], cases: [], passed: false, allowDirty: args['allow-dirty'],
};
let browser, server, app;
const resources = new Set();
const nonce = randomUUID();
const netemScript = fileURLToPath(new URL('./netem.sh', import.meta.url));
const shapeWan = process.platform === 'linux' && process.env.MISTLIB_QA_NETEM === '1';
const deepWanQueue = process.env.MISTLIB_QA_WAN_QUEUE === 'deep';
let netemActive = false;
// 90% of payload serialization time at the shaped rate, tolerating netem burst.
const wanFloorMs = (bytes, rateKbit) => 0.9 * bytes * 8 / rateKbit;
function disableNetem() {
  if (!netemActive) return;
  execFileSync('sudo', ['-n', netemScript, 'off'], { stdio: 'inherit' });
  netemActive = false;
}
// Exit handlers must be synchronous; also cover interruption during a transfer.
process.on('exit', () => {
  try { disableNetem(); }
  catch (error) { console.error('netem cleanup failed:', error); process.exitCode = 1; }
});
process.once('SIGINT', () => process.exit(130));
process.once('SIGTERM', () => process.exit(143));

async function hostPackage() {
  app = await mkdtemp(join(tmpdir(), 'mistlib-web-qa-'));
  await writeFile(join(app, 'package.json'), '{"private":true}');
  execFileSync('npm', ['install', '--ignore-scripts', '--offline', '--no-audit', '--no-fund',
    '--cache', join(app, '.npm'), '--prefix', app, resolve(args.package)], { stdio: 'pipe' });
  server = createServer(async (req, res) => {
    try {
      if (req.url === '/') { res.setHeader('Content-Type', 'text/html'); return res.end('<!doctype html><title>MistLib QA</title>'); }
      const path = resolve(app, '.' + new URL(req.url, 'http://localhost').pathname);
      if (!path.startsWith(app + sep)) { res.writeHead(403); return res.end(); }
      const data = await readFile(path);
      res.setHeader('Content-Type', path.endsWith('.wasm') ? 'application/wasm' : 'text/javascript');
      res.end(data);
    } catch { res.writeHead(404); res.end(); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const playwright = await import('playwright');
  browser = playwright[args.browser];
  return `http://127.0.0.1:${server.address().port}`;
}

async function runPair(kinds, url) {
  const pair = kinds.join('-');
  const relay = await startRelay();
  resources.add(relay);
  const config = { iceServers: [], aoiRange: 100, signaling: { mode: 'nostr', nostr: {
    relays: [relay.url], inviteSalt: 'release-qa', inviteCode: nonce,
  } } };
  const room = `qa-${nonce}-${pair}`, otherRoom = `${room}-other`;
  const ids = [`a-${nonce}`, `z-${nonce}`];
  const clients = [], ledgers = new Map();
  let generation = 0;
  const make = async (kind, index) => {
    const name = `${pair}-${index}-${generation++}`;
    const client = kind === 'native' ? await nativeClient(resolve(args.native), logs, name)
      : await browserClient(browser, url, logs, name);
    resources.add(client);
    if (client.browserVersion) report.browserVersion = client.browserVersion;
    const info = await client.info();
    report.buildInfo.push(info);
    assert.equal(info.version, args.version, `${name} loaded version`);
    assert.equal(info.commit, args.commit, `${name} loaded commit`);
    assert.equal(info.profile, 'release', `${name} must consume a release build`);
    if (!args['allow-dirty']) assert.equal(info.dirty, false, `${name} candidate must be clean`);
    ledgers.set(client, []);
    await client.command({ op: 'init', node: ids[index], config });
    return client;
  };
  const checkpoint = async () => Promise.all(clients.map(client => client.events().then(events => events.length)));
  const connect = async (joinedRoom, marks) => {
    const started = performance.now();
    await Promise.all(clients.map((client, index) => until(() => client.events(),
      events => events.slice(marks[index]).some(event => event.kind === 'connected' &&
        event.sender === ids[1 - index] && event.room === joinedRoom),
      policy.connectionDeadlineMs, `${pair} connected in ${joinedRoom}`)));
    return performance.now() - started;
  };
  const validateLedger = async client => {
    const actual = (await client.drain()).filter(event => event.kind === 'raw' &&
      Buffer.from(event.data).toString().startsWith(`qa:${nonce}:`));
    assert.deepEqual(actual, ledgers.get(client), `${client.name}: exact data/order/sender/room, no late duplicates`);
    // Completed batches are retired. A late duplicate in a later batch is now
    // unexpected, without repeatedly copying the entire soak's history.
    ledgers.set(client, []);
  };
  const deliver = async (joinedRoom, label, count = policy.messagesPerDirection) => {
    const expected = [[], []], started = performance.now();
    for (let index = 0; index < count; index++) {
      for (let sender = 0; sender < 2; sender++) {
        const data = Array.from(Buffer.concat([Buffer.from(`qa:${nonce}:${label}:${sender}:${index}:`),
          Buffer.from([0, 128, 255, index % 256])]));
        const event = { kind: 'raw', sender: ids[sender], room: joinedRoom, data };
        ledgers.get(clients[1 - sender]).push(event);
        expected[1 - sender].push(event);
        await clients[sender].command({ op: 'send', room: joinedRoom, target: ids[1 - sender], data });
      }
    }
    await Promise.all(clients.map((client, index) => until(() => client.events(), events =>
      expected[index].every(want => events.some(event => event.kind === 'raw' &&
        JSON.stringify(event.data) === JSON.stringify(want.data))),
      policy.deliveryDeadlineMs, `${pair} ${label} received every message`)));
    await Promise.all(clients.map(validateLedger));
    return { sent: count * 2, received: count * 2, deliveryMs: performance.now() - started };
  };
  const phase = async (name, action) => {
    const result = { id: `${pair}/${name}`, passed: false, metrics: {} };
    report.cases.push(result);
    const started = performance.now();
    try { result.metrics = await action(); result.passed = true; }
    catch (error) { result.error = String(error.stack || error); throw error; }
    finally {
      result.durationMs = performance.now() - started;
      console.log(`${result.id}: ${result.passed ? 'PASS' : 'FAIL'} (${Math.round(result.durationMs)} ms)`);
    }
  };
  try {
    for (let index = 0; index < 2; index++) clients[index] = await make(kinds[index], index);
    await phase('delivery', async () => {
      const marks = await checkpoint();
      await Promise.all(clients.map(client => client.command({ op: 'join', room })));
      const connectionMs = await connect(room, marks);
      await Promise.all(clients.map((client, i) => client.command({ op: 'position', room, position: [i, 0, 0] })));
      return { connectionMs, ...await deliver(room, 'initial') };
    });
    await phase('aoi', async () => {
      let convergenceMs = 0, samples = 0;
      for (const x of [1, 250, 1]) {
        const marks = await checkpoint(), started = performance.now();
        await clients[1].command({ op: 'position', room, position: [x, 0, 0] });
        await Promise.all(clients.map((client, index) => until(() => client.events(), events => {
          const event = events.slice(marks[index]).filter(e => e.kind === 'aoi' && e.room === room).at(-1);
          if (!event) return false;
          const nodes = JSON.parse(Buffer.from(event.data).toString());
          const expected = x < 100 ? [ids[1 - index]] : [];
          return JSON.stringify(nodes.map(node => node.id).sort()) === JSON.stringify(expected);
        }, policy.connectionDeadlineMs, `${pair} AOI matches commanded positions at x=${x}`)));
        convergenceMs = Math.max(convergenceMs, performance.now() - started);
        samples += 2;
      }
      return { samples, missing: 0, false: 0, convergenceMs };
    });
    await phase('multi-room', async () => {
      const marks = await checkpoint();
      await Promise.all(clients.map(client => client.command({ op: 'join', room: otherRoom })));
      const connectionMs = await connect(otherRoom, marks);
      const first = await deliver(room, 'room-a'), second = await deliver(otherRoom, 'room-b');
      await delay(policy.negativeObservationMs);
      await Promise.all(clients.map(validateLedger));
      return { connectionMs, sent: first.sent + second.sent, received: first.received + second.received,
        deliveryMs: Math.max(first.deliveryMs, second.deliveryMs), roomLeaks: 0,
        observationMs: policy.negativeObservationMs };
    });
    await phase('leave-rejoin', async () => {
      let total = 0, maxConnectionMs = 0, maxDeliveryMs = 0;
      for (let cycle = 0; cycle < policy.rejoinCycles; cycle++) {
        const marks = await checkpoint();
        await clients[1].command({ op: 'leave', room }); // per-room leave; preserves the other room
        await until(() => clients[0].events(), events => events.slice(marks[0]).some(event =>
          event.kind === 'disconnected' && event.sender === ids[1] && event.room === room),
        policy.connectionDeadlineMs, 'leave observed by the other peer');
        const retained = await deliver(otherRoom, `retained-${cycle}`);
        const rejoinMarks = await checkpoint();
        await clients[1].command({ op: 'join', room });
        maxConnectionMs = Math.max(maxConnectionMs, await connect(room, rejoinMarks));
        const rejoined = await deliver(room, `rejoin-${cycle}`);
        total += retained.sent + rejoined.sent;
        maxDeliveryMs = Math.max(maxDeliveryMs, retained.deliveryMs, rejoined.deliveryMs);
      }
      return { sent: total, received: total, cycles: policy.rejoinCycles,
        connectionMs: maxConnectionMs, deliveryMs: maxDeliveryMs };
    });
    await phase('relay-outage', async () => {
      relay.outage();
      assert(relay.counts.disconnected >= 2, 'fault must disconnect actual signaling sockets');
      await delay(policy.negativeObservationMs);
      const measurement = await deliver(room, 'relay-down');
      relay.faults.offline = false;
      return { ...measurement, disconnectedSockets: relay.counts.disconnected };
    });
    await phase('peer-restart', async () => {
      const marks = await checkpoint();
      await validateLedger(clients[1]);
      await clients[1].close(); resources.delete(clients[1]);
      await until(() => clients[0].events(), events => events.slice(marks[0]).some(event =>
        event.kind === 'disconnected' && event.sender === ids[1] && event.room === room),
      policy.connectionDeadlineMs, 'crashed peer detected');
      const rejoinMarks = [(await clients[0].events()).length, 0];
      clients[1] = await make(kinds[1], 1);
      await clients[1].command({ op: 'join', room });
      const connectionMs = await connect(room, rejoinMarks);
      return { connectionMs, restarts: 1, ...await deliver(room, 'restart') };
    });
    await phase('storage', async () => {
      for (let sender = 0; sender < 2; sender++) {
        const data = Array.from(Buffer.concat([Buffer.from(`storage-${nonce}-${sender}`), Buffer.from([0, 128, 255])]));
        const cid = await clients[sender].command({ op: 'store', data });
        assert.equal(typeof cid, 'string'); assert(cid.length > 0);
        assert.deepEqual(await clients[1 - sender].command({ op: 'get', cid }), data, 'cross-peer content hash and bytes');
      }
      return { transfers: 2, corruptions: 0 };
    });
    await phase('storage-wan', async () => {
      const { bytes, delayMs, rateKbit, queueMs, deadlineMs } = policy.wanStorage;
      // One shared band carries both directions. netem's limit includes delay-stage
      // packets: reserve 2*delayMs propagation plus queueMs buffering at rateKbit.
      // ceil((2*delayMs + queueMs)/1000 * rateKbit*1000/8 / 1200), assuming
      // MTU-ish 1200 B packets; this is a packet budget, so overflow loss is allowed.
      const limitPackets = Math.ceil((2 * delayMs + queueMs) / 1000 * rateKbit * 1000 / 8 / 1200);
      // Five blocks cross get()'s four-way boundary. At 1 mbit even one 1 MiB
      // block needs ~8.4 s, exceeding the old resolver's fixed 5 s deadline.
      const transferMs = [], cids = new Set();
      try {
        if (shapeWan) {
          netemActive = true; // Cleanup also covers a partially applied setup.
          execFileSync('sudo', ['-n', netemScript, 'on', String(delayMs), String(rateKbit),
            ...(deepWanQueue ? [] : [String(limitPackets)])], { stdio: 'inherit' });
        }
        for (let sender = 0; sender < 2; sender++) {
          const stored = await clients[sender].command({ op: 'store-generated',
            seed: 0x19a50001 + sender, size: bytes }, deadlineMs);
          assert.equal(typeof stored.cid, 'string'); assert(stored.cid.length > 0);
          assert(!cids.has(stored.cid), 'each direction must fetch fresh content');
          cids.add(stored.cid);
          assert.equal(stored.size, bytes);
          assert.match(stored.sha256, /^[0-9a-f]{64}$/);
          const started = performance.now();
          const fetched = await clients[1 - sender].command({ op: 'get-digest', cid: stored.cid }, deadlineMs);
          const elapsed = performance.now() - started;
          assert(elapsed <= deadlineMs, 'WAN storage per-direction deadline');
          // Faster than the shaped link's serialization time means the bytes never
          // crossed it (e.g. a shared local block store); that is not a WAN pass.
          if (shapeWan) assert(elapsed >= wanFloorMs(bytes, rateKbit),
            `WAN transfer ${Math.round(elapsed)} ms is below the shaped-link floor`);
          assert.deepEqual(fetched, { sha256: stored.sha256, size: stored.size }, 'WAN cross-peer size and digest');
          transferMs.push(elapsed);
        }
        // Record only the shaping actually applied; unshaped runs report null.
        return { transfers: 2, corruptions: 0, bytes, shaped: shapeWan,
          delayMs: shapeWan ? delayMs : null, rateKbit: shapeWan ? rateKbit : null,
          queueMs: shapeWan && !deepWanQueue ? queueMs : null,
          limitPackets: shapeWan && !deepWanQueue ? limitPackets : null,
          queueProfile: shapeWan ? (deepWanQueue ? 'deep' : 'bounded') : null, transferMs };
      } finally { disableNetem(); }
    });
    await phase('soak', async () => {
      const started = performance.now(), samples = [];
      let sent = 0, deliveryMs = 0, batch = 0;
      do {
        const measurement = await deliver(room, `soak-${batch++}`);
        sent += measurement.sent; deliveryMs = Math.max(deliveryMs, measurement.deliveryMs);
        samples.push(await Promise.all(clients.map(client => client.command({ op: 'stats' }))));
        await delay(250);
      } while (performance.now() - started < soakSeconds * 1000);
      await delay(policy.negativeObservationMs);
      await Promise.all(clients.map(validateLedger));
      return { sent, received: sent, deliveryMs, observedMs: performance.now() - started,
        samples: samples.length, stats: [samples[0], samples.at(-1)] };
    });
  } finally {
    for (const client of clients) {
      if (client && resources.delete(client)) await client.close();
    }
    resources.delete(relay); await relay.close();
  }
}

try {
  const url = args.browser ? await hostPackage() : null;
  // The lower NodeId initiates: both mixed orders exercise distinct native /
  // browser negotiation and crash-recovery paths without duplicating scenarios.
  for (const kinds of args.browser ? [['browser', 'browser'], ['browser', 'native'], ['native', 'browser']] : [['native', 'native']]) {
    await runPair(kinds, url);
  }
  report.passed = harnessErrors.length === 0;
} catch (error) {
  report.error = String(error.stack || error);
  process.exitCode = 1;
} finally {
  try { disableNetem(); }
  catch (error) { harnessErrors.push(`netem cleanup failed: ${error.stack || error}`); process.exitCode = 1; }
  for (const resource of resources) await resource.close().catch(() => {});
  if (server) await new Promise(resolve => server.close(resolve));
  if (app) await rm(app, { recursive: true, force: true });
  if (harnessErrors.length) {
    report.passed = false;
    report.error = [report.error, ...harnessErrors].filter(Boolean).join('\n');
  }
  report.finishedAt = new Date().toISOString();
  await writeFile(output, JSON.stringify(report, null, 2) + '\n');
  console.log(JSON.stringify({ id: report.id, passed: report.passed, cases: report.cases.length, error: report.error }));
}
