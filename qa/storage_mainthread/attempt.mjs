import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { join, resolve, sep } from 'node:path';
import { cpus, platform, release, arch } from 'node:os';
import { randomUUID, createHash } from 'node:crypto';
import { startRelay } from '../relay.mjs';
import { bounded, until } from '../clients.mjs';
import { analyze } from './metrics.mjs';

const spec = JSON.parse(process.argv[2]);
await mkdir(spec.directory, { recursive: true });
const row = { schema: 1, ...spec, success: false, error: null, environment: { platform: platform(), release: release(), arch: arch(), cpu: cpus()[0]?.model, logicalCpus: cpus().length, node: process.version, samplingIntervalUs: 500, renderBudgetMs: 8 }, peers: [] };
const browsers = [], servers = [], peers = [];
let host, relay;
const save = (name, value) => writeFile(join(spec.directory, name), JSON.stringify(value));
try {
  relay = await startRelay();
  host = createServer(async (req, res) => {
    try {
      if (spec.blockWorkers) res.setHeader('Content-Security-Policy', "worker-src 'none'");
      const match = req.url.match(/^\/([01])\/(mistlib_wasm(?:_bg\.wasm|\.js)|snippets\/.*\.js)$/);
      if (match) {
        const root = resolve(spec.packages[Number(match[1])]);
        const file = resolve(root, decodeURIComponent(match[2]));
        if (!file.startsWith(root + sep)) throw new Error('invalid fixture path');
        res.setHeader('Content-Type', match[2].endsWith('.wasm') ? 'application/wasm' : 'text/javascript');
        res.end(await readFile(file));
      } else { res.setHeader('Content-Type', 'text/html'); res.end('<!doctype html><title>Storage main thread measurement</title>'); }
    } catch { res.statusCode = 404; res.end(); }
  });
  await new Promise(resolve => host.listen(0, '127.0.0.1', resolve));
  const url = `http://127.0.0.1:${host.address().port}`;
  const nonce = randomUUID(), room = `sm-${nonce}`;
  const ids = [`a-${nonce}`, `z-${nonce}`];
  const config = { iceServers: [], aoiRange: 100, signaling: { mode: 'nostr', nostr: {
    relays: [relay.url], inviteSalt: 'storage-mainthread', inviteCode: nonce,
  } } };
  for (let i = 0; i < 2; i++) {
    const server = await chromium.launchServer({ headless: true, args: ['--disable-background-networking'] });
    servers.push(server);
    await save('browser-pids.json', servers.map(s => s.process().pid));
    const browser = await chromium.connect(server.wsEndpoint());
    browsers.push(browser);
    const context = await browser.newContext();
    await context.route('**/*', route => route.request().url().startsWith(url + '/') ? route.continue() : route.abort());
    const page = await context.newPage();
    const logs = [];
    page.on('pageerror', e => logs.push({ error: String(e) }));
    // Do not retain SDP/candidate addresses in research artifacts.
    page.on('console', m => { if (m.type() === 'error') logs.push({ error: m.text().replace(/(?:\d{1,3}\.){3}\d{1,3}/g, '<address>') }); });
    await page.goto(url);
    await page.evaluate(async ({ i, config, node }) => {
      const m = await import(`/${i}/mistlib_wasm.js`); await m.default();
      m.set_log_level?.('error');
      window.sm = { m, events: [], phases: {}, frames: [], longtasks: [] };
      m.register_event_callback((kind, sender, data, room) => {
        if (kind === m.MistEvent.PeerConnected) sm.events.push({ kind: 'connected', sender });
        if (kind === m.MistEvent.AoiNodes) sm.events.push({ kind: 'aoi', nodes: JSON.parse(new TextDecoder().decode(data)) });
      });
      if (!m.init_with_config(node, JSON.stringify(config))) throw new Error('init rejected');
      sm.mark = (phase, edge) => {
        const t = performance.now();
        (sm.phases[phase] ??= {})[edge] = t;
        performance.mark(`sm:${phase}:${edge}`);
      };
      new PerformanceObserver(list => {
        sm.longtasks.push(...list.getEntries().map(t => ({ startTime: t.startTime, duration: t.duration })));
      }).observe({ type: 'longtask', buffered: true });
    }, { i, config, node: ids[i] });
    const cdp = await context.newCDPSession(page);
    peers.push({ page, cdp, logs });
    row.peers.push({ version: await page.evaluate(() => sm.m.get_version()), browser: browser.version(), jsSha256: createHash('sha256').update(await readFile(join(spec.packages[i], 'mistlib_wasm.js'))).digest('hex'), wasmSha256: createHash('sha256').update(await readFile(join(spec.packages[i], 'mistlib_wasm_bg.wasm'))).digest('hex') });
  }
  await Promise.all(peers.map(p => p.page.evaluate(room => sm.m.join_room_async(room), room)));
  await Promise.all(peers.map((p, i) => until(() => p.page.evaluate(() => sm.events),
    events => events.some(e => e.kind === 'connected' && e.sender === ids[1 - i]), 30000, 'peer connected')));
  await Promise.all(peers.map((p, i) => p.page.evaluate(({ room, i }) => sm.m.update_position_in_room(room, i, 0, 0), { room, i })));
  await Promise.all(peers.map((p, i) => until(() => p.page.evaluate(() => sm.events),
    events => events.some(e => e.kind === 'aoi' && e.nodes.some(n => n.id === ids[1 - i])), 30000, 'overlay visible')));
  // Generate and hash outside profiling/throttling; integrity verification also
  // runs after measurement. Each context has a fresh OPFS and cache.
  row.expected = await peers[0].page.evaluate(async size => {
    sm.bytes = new Uint8Array(size);
    for (let i = 0; i < size; i += 65536) crypto.getRandomValues(sm.bytes.subarray(i, i + 65536));
    return Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', sm.bytes))).map(b => b.toString(16).padStart(2, '0')).join('');
  }, spec.bytes);
  for (const p of peers) {
    await p.cdp.send('Emulation.setCPUThrottlingRate', { rate: spec.rate });
    await p.cdp.send('Profiler.enable');
    await p.cdp.send('Profiler.setSamplingInterval', { interval: 500 });
    await p.cdp.send('Profiler.start');
    let categories = 'toplevel,devtools.timeline,blink.user_timing';
    if (spec.traceWasm) {
      const available = await p.cdp.send('Tracing.getCategories');
      row.wasmTraceCategories = [...new Set([
        ...available.categories.filter(c => /wasm|v8/i.test(c)),
        'v8', 'v8.wasm', 'disabled-by-default-v8.wasm',
        'disabled-by-default-v8.wasm.detailed', 'disabled-by-default-v8.compile',
      ])];
      categories += ',' + row.wasmTraceCategories.join(',');
    }
    await p.cdp.send('Tracing.start', { categories, transferMode: 'ReturnAsStream' });
    p.recording = true;
    await p.page.evaluate(() => {
      let last;
      function render() {
        const now = performance.now();
        if (last !== undefined) sm.frames.push({ t: now, dt: now - last });
        last = now;
        while (performance.now() - now < 8) { /* simulated rendering budget in wall milliseconds */ }
        sm.raf = requestAnimationFrame(render);
      }
      sm.raf = requestAnimationFrame(render);
    });
  }
  const mark = (phase, edge) => Promise.all(peers.map(p => p.page.evaluate(({ phase, edge }) => sm.mark(phase, edge), { phase, edge })));
  await mark('idle', 'start');
  await new Promise(r => setTimeout(r, 1500));
  await mark('idle', 'end');
  await mark('add', 'start');
  const stored = await peers[0].page.evaluate(async mode => {
    const start = performance.now();
    const api = { pinned: 'storage_add_pinned', 'add-pin': 'storage_add',
      'add-owned': 'storage_add_pinned_owned', 'add-owned-pin': 'storage_add_owned' }[mode];
    const promise = sm.m[api]('avatar.bin', sm.bytes);
    if (mode.startsWith('add-owned') && sm.bytes.buffer.byteLength !== 0) throw new Error('owned API did not detach input synchronously');
    const cid = await promise;
    if (mode === 'add-pin' || mode === 'add-owned-pin') await sm.m.storage_pin(cid);
    return { cid, ms: performance.now() - start };
  }, spec.mode);
  row.addMs = stored.ms; row.cid = stored.cid;
  await mark('add', 'end');
  await mark('transfer', 'start');
  row.transferMs = await bounded(peers[1].page.evaluate(async cid => {
    const start = performance.now(); sm.received = await sm.m.storage_get(cid);
    return performance.now() - start;
  }, stored.cid), spec.transferTimeoutMs, 'storage_get');
  await mark('transfer', 'end');
  // Allow the observer to deliver the final long-task entry.
  await new Promise(r => setTimeout(r, 100));
  row.success = true;
} catch (error) {
  row.error = String(error.stack || error);
  // Preserve censored phases on failure; never summarize them as successes.
  await Promise.allSettled(peers.map(p => bounded(p.page.evaluate(() => {
    for (const [phase, window] of Object.entries(sm.phases)) {
      if (window.end === undefined) { window.censored = true; sm.mark(phase, 'end'); }
    }
  }), 2000, 'failure marks')));
}
finally {
  for (const [i, p] of peers.entries()) {
    try {
      if (p.recording) {
        const { profile } = await p.cdp.send('Profiler.stop');
        const complete = new Promise(resolve => p.cdp.once('Tracing.tracingComplete', resolve));
        await p.cdp.send('Tracing.end');
        const { stream } = await bounded(complete, 10000, 'trace completion');
        let raw = '';
        while (true) {
          const chunk = await p.cdp.send('IO.read', { handle: stream });
          raw += chunk.base64Encoded ? Buffer.from(chunk.data, 'base64').toString() : chunk.data;
          if (chunk.eof) break;
        }
        await p.cdp.send('IO.close', { handle: stream });
        const trace = JSON.parse(raw);
        const page = await p.page.evaluate(() => {
          cancelAnimationFrame(sm.raf);
          return { phases: sm.phases, frames: sm.frames, longtasks: sm.longtasks };
        });
        await save(`${i}.cpuprofile`, profile); await save(`${i}.trace.json`, trace); await save(`${i}.page.json`, page);
        row.peers[i].phases = {};
        for (const phase of Object.keys(page.phases)) {
          if (page.phases[phase].end) row.peers[i].phases[phase] = analyze(profile, trace, page, phase);
        }
      }
      await save(`${i}.errors.json`, p.logs);
    } catch (error) { row.success = false; row.error = [row.error, `analysis: ${error.stack || error}`].filter(Boolean).join('\n'); }
  }
  if (row.success) {
    const received = await peers[1].page.evaluate(async () => ({ size: sm.received.length,
      digest: Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', sm.received))).map(b => b.toString(16).padStart(2, '0')).join('') }));
    row.verifiedBytes = received.size;
    if (received.digest !== row.expected || received.size !== spec.bytes) { row.success = false; row.error = 'size/SHA-256 mismatch'; }
  }
  await Promise.allSettled(servers.map(s => s.kill()));
  if (host) await new Promise(r => host.close(r));
  if (relay) await relay.close();
  await save('result.json', row);
  console.log(JSON.stringify(row));
}
