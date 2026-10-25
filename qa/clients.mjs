import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import assert from 'node:assert/strict';
import { createWriteStream } from 'node:fs';

// Traces grow with soak length; killing the browser mid-save truncates them and crashes Playwright.
const TRACE_SAVE_DEADLINE_MS = 60000;

export async function bounded(promise, timeout, description) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`deadline exceeded: ${description}`)), timeout);
    })]);
  } finally { clearTimeout(timer); }
}

export async function until(read, predicate, timeout, description) {
  const end = performance.now() + timeout;
  while (performance.now() < end) {
    const value = await read();
    if (predicate(value)) return value;
    await delay(50);
  }
  throw new Error(`deadline exceeded: ${description}`);
}

export async function nativeClient(binary, directory, name) {
  const cwd = await mkdtemp(join(tmpdir(), 'mistlib-native-qa-'));
  const child = spawn(process.env.QA_PYTHON || (process.platform === 'win32' ? 'python' : 'python3'),
    [fileURLToPath(new URL('./native.py', import.meta.url)), binary],
    // The native library keeps blocks under the OS temp dir, so point it at this
    // process's own directory; a shared block store would make cross-peer gets local.
    { cwd, env: { ...process.env, RUST_LOG: process.env.QA_RUST_LOG || 'off', TMPDIR: cwd, TMP: cwd, TEMP: cwd },
      stdio: ['pipe', 'pipe', 'pipe'] });
  const events = [], output = [], pending = new Map();
  let serial = 0, buildInfo, dead;
  child.stderr.on('data', bytes => output.push(bytes.toString()));
  child.on('error', error => { dead = error; });
  child.on('exit', (code, signal) => {
    dead = new Error(`${name} exited: code=${code}, signal=${signal}`);
    for (const { reject } of pending.values()) reject(dead);
    pending.clear();
  });
  createInterface({ input: child.stdout }).on('line', line => {
    output.push(line + '\n');
    let value;
    try { value = JSON.parse(line); } catch { return; }
    if (value.ready) buildInfo = value.ready;
    if (value.event) events.push(value.event);
    const request = pending.get(value.id);
    if (request) {
      pending.delete(value.id);
      if (value.error) request.reject(new Error(value.error));
      else request.resolve(value.value);
    }
  });
  const client = {
    name,
    async info() {
      return until(() => { if (dead) throw dead; return buildInfo; }, Boolean, 15000, `${name} startup`);
    },
    async command(command, timeout = 30000) {
      if (dead) throw dead;
      const id = ++serial;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`${name} command timed out: ${command.op}`));
        }, timeout);
        pending.set(id, {
          resolve: value => { clearTimeout(timer); resolve(value); },
          reject: error => { clearTimeout(timer); reject(error); },
        });
        child.stdin.write(JSON.stringify({ ...command, id }) + '\n');
      });
    },
    async events() { if (dead) throw dead; return events; },
    async drain() { if (dead) throw dead; return events.splice(0); },
    async close() {
      if (child.pid && child.exitCode === null && !child.signalCode) {
        const exited = new Promise(resolve => child.once('exit', resolve));
        child.kill('SIGKILL');
        await bounded(exited, 5000, `${name} process termination`);
      }
      await writeFile(join(directory, `${name}.log`), output.join(''));
      await rm(cwd, { recursive: true, force: true });
    },
  };
  return client;
}

export async function browserClient(browserType, url, directory, name, { trace = true } = {}) {
  // A separate browser process makes kill/restart a real process fault, rather
  // than relying on a tab close to release the browser's shared RTC resources.
  const chromium = browserType.name?.() === 'chromium';
  // "1" logs both endpoints; an exact client name logs only that endpoint.
  const sctpLogging = chromium && ['1', name].includes(process.env.QA_CHROME_SCTP_LOG);
  const customArgs = chromium ? JSON.parse(process.env.QA_CHROMIUM_ARGS || '[]') : [];
  assert(Array.isArray(customArgs) && customArgs.every(arg => typeof arg === 'string'), 'QA_CHROMIUM_ARGS must be a JSON string array');
  const extraArgs = [];
  if (sctpLogging) {
    extraArgs.push('--enable-logging=stderr', '--v=0', '--vmodule=*dcsctp*=2,*sctp*=2,*/dcsctp/*=2');
  }
  // Explicit diagnostic args can override the default logging verbosity.
  extraArgs.push(...customArgs);
  const server = await browserType.launchServer({ headless: true, args: extraArgs });
  const chromeLog = sctpLogging
    ? createWriteStream(join(directory, `${name}-sctp.log`)) : null;
  const recordStderr = bytes => chromeLog.write(bytes);
  if (chromeLog) server.process().stderr.on('data', recordStderr);
  const stopServer = async () => {
    await server.kill();
    if (chromeLog) {
      server.process().stderr.off('data', recordStderr);
      await new Promise(resolve => chromeLog.end(resolve));
    }
  };
  try {
    const browser = await browserType.connect(server.wsEndpoint());
    const context = await browser.newContext();
    if (trace) await context.tracing.start({ screenshots: true, snapshots: true, sources: true });
    const page = await context.newPage();
    const errors = [], consoleLog = [], history = [];
    page.on('pageerror', error => errors.push(String(error)));
    page.on('console', message => consoleLog.push({ type: message.type(), text: message.text() }));
    await page.goto(url);
    await page.evaluate(async diagnostics => {
      // Keep PC discovery and RTC stats in the page, alongside the wasm client.
      const pcs = new Set();
      const channels = new Map();
      const observeChannel = channel => {
        if (!diagnostics || channels.has(channel)) return;
        const counters = { receivedBytes: 0, receivedMessages: 0 };
        channels.set(channel, counters);
        channel.addEventListener('message', ({ data }) => {
          counters.receivedBytes += typeof data === 'string' ? new TextEncoder().encode(data).length
            : (data.byteLength ?? data.size ?? 0);
          counters.receivedMessages++;
        });
      };
      const PeerConnection = window.RTCPeerConnection;
      window.RTCPeerConnection = class extends PeerConnection {
        constructor(...args) {
          super(...args); pcs.add(this);
          this.addEventListener('datachannel', ({ channel }) => observeChannel(channel));
        }
        createDataChannel(...args) {
          const channel = super.createDataChannel(...args);
          observeChannel(channel);
          return channel;
        }
      };
      const module = await import('/node_modules/@tik-choco/mistlib/mistlib_wasm.js');
      await module.default();
      if (module.Delivery.Reliable !== 0 || module.MistEvent.Raw !== 0 ||
          module.get_version() !== JSON.parse(module.get_build_info()).version) {
        throw new Error('documented JS exports/version are inconsistent');
      }
      window.qa = { module, events: [] };
      // Opt-in benchmark probes: callbacks and timers stay live during async get.
      const control = window.qa.control = {
        config: null, pending: new Map(), samples: [], timeouts: 0, censored: 0, serial: 0,
        send(text) {
          const c = this.config;
          module.send_message_in_room(c.room, c.target, new TextEncoder().encode(text), 0);
        },
        receive(room, sender, data) {
          const c = this.config;
          if (!c || room !== c.room || sender !== c.target) return false;
          const prefix = `mistlib-storage-control:${c.token}:`;
          const text = new TextDecoder().decode(data);
          if (!text.startsWith(prefix)) return false;
          const message = text.slice(prefix.length);
          if (message.startsWith('ping:')) this.send(prefix + 'pong:' + message.slice(5));
          else if (message.startsWith('pong:')) {
            const id = message.slice(5), started = this.pending.get(id);
            if (started !== undefined) {
              this.samples.push(performance.now() - started);
              this.pending.delete(id);
            }
          }
          return true;
        },
        start() {
          if (!this.config) throw new Error('control probe is not configured');
          this.pending.clear(); this.samples = []; this.timeouts = this.censored = 0;
          const ping = () => {
            const now = performance.now();
            for (const [id, started] of this.pending) {
              if (now - started >= 10000) { this.pending.delete(id); this.timeouts++; }
            }
            const id = String(++this.serial);
            this.pending.set(id, now);
            this.send(`mistlib-storage-control:${this.config.token}:ping:${id}`);
          };
          ping(); this.timer = setInterval(ping, 1000);
        },
        stop() {
          if (this.timer) {
            clearInterval(this.timer); this.timer = null;
            this.censored = this.pending.size; this.timeouts += this.censored; this.pending.clear();
          }
          const samples = [...this.samples].sort((a, b) => a - b);
          const percentile = p => samples.length ? samples[Math.ceil(p * samples.length) - 1] : null;
          return { controlSamples: samples.length, controlTimeouts: this.timeouts,
            controlCensored: this.censored, controlRttP50Ms: percentile(0.5), controlRttP95Ms: percentile(0.95) };
        },
      };
      const telemetry = window.qa.telemetry = {
        series: [], timer: null, pending: null,
        async sample() {
          const t = Date.now();
          try {
            const stats = JSON.parse(module.get_stats());
            const rtc = [];
            this.series.push({ t, ...stats, rtc, ...(diagnostics ? { channels: [...channels].map(([c, counters]) =>
              ({ label: c.label, id: c.id, ordered: c.ordered, maxRetransmits: c.maxRetransmits,
                readyState: c.readyState, bufferedAmount: c.bufferedAmount, ...counters })) } : {}) });
            for (const pc of pcs) {
              if (pc.connectionState === 'closed') { pcs.delete(pc); continue; }
              try {
                let timer;
                const report = await Promise.race([pc.getStats(), new Promise((_, reject) => {
                  timer = setTimeout(() => reject(new Error('RTC stats timeout')), 500);
                })]).finally(() => clearTimeout(timer));
                // Retain selected pairs (transport reference is authoritative),
                // with nominated/succeeded as a fallback for browser variants.
                const selected = new Set([...report.values()].filter(s => s.type === 'transport')
                  .map(s => s.selectedCandidatePairId).filter(Boolean));
                for (const stat of report.values()) {
                  if (stat.type === 'transport' || (stat.type === 'candidate-pair' &&
                      (selected.has(stat.id) || (!selected.size && stat.nominated && stat.state === 'succeeded')))) {
                    const value = { id: stat.id, type: stat.type };
                    for (const key of ['selectedCandidatePairId', 'currentRoundTripTime', 'bytesSent',
                      'bytesReceived', 'availableOutgoingBitrate']) {
                      if (stat[key] !== undefined) value[key] = stat[key];
                    }
                    rtc.push(value);
                  }
                }
              } catch (error) { rtc.push({ error: String(error) }); }
            }
          } catch (error) { this.series.push({ t, error: String(error) }); }
        },
        async start() {
          this.series = [];
          await this.sample();
          this.timer = setInterval(() => {
            if (!this.pending) this.pending = this.sample().finally(() => { this.pending = null; });
          }, 1000);
        },
        async stop() {
          clearInterval(this.timer); this.timer = null;
          // Capture final counters before waiting for an in-flight RTC query.
          const final = this.sample();
          await this.pending;
          await final;
          return this.series;
        },
      };
      module.register_event_callback((kind, sender, data, room) => {
        if (kind === 0 && control.receive(room, sender, data)) return;
        window.qa.events.push({ kind: ({ 0: 'raw', 5: 'connected', 6: 'disconnected', 7: 'aoi' })[kind] || String(kind),
          sender, room, data: Array.from(data) });
      });
    }, process.env.QA_RTC_DIAGNOSTICS === '1' || Boolean(process.env.QA_CHROME_SCTP_LOG));
    return {
      name, browserVersion: browser.version(),
      async info() { return bounded(page.evaluate(() => JSON.parse(window.qa.module.get_build_info())), 5000, `${name} build identity`); },
      async events() {
        assert.deepEqual(errors, [], `${name} browser errors`);
        return bounded(page.evaluate(() => window.qa.events), 5000, `${name} event inspection`);
      },
      async drain() {
        assert.deepEqual(errors, [], `${name} browser errors`);
        const events = await bounded(page.evaluate(() => window.qa.events.splice(0)), 5000, `${name} event drain`);
        history.push(...events);
        return events;
      },
      async command(command, timeout = 30000) {
        assert.deepEqual(errors, [], `${name} browser errors`);
        return bounded(page.evaluate(async c => {
          const m = window.qa.module;
          const digest = async data => ({
            sha256: Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', data)),
              byte => byte.toString(16).padStart(2, '0')).join(''),
            size: data.length,
          });
          switch (c.op) {
            case 'init':
              if (c.wasmLogLevel && !m.set_log_level(c.wasmLogLevel)) throw new Error('WASM log level rejected (set before first init)');
              if (!m.init_with_config(c.node, JSON.stringify(c.config))) throw new Error('init rejected');
              break;
            case 'join': await m.join_room_async(c.room); break;
            case 'leave': await m.leave_room_async(c.room); break;
            case 'leave-all': m.leave_all_rooms(); break;
            case 'position': m.update_position_in_room(c.room, ...c.position); break;
            case 'send': m.send_message_in_room(c.room, c.target, new Uint8Array(c.data), 0); break;
            case 'stats': return JSON.parse(m.get_stats());
            case 'telemetry-start': await window.qa.telemetry.start(); break;
            case 'telemetry-stop': return window.qa.telemetry.stop();
            case 'store': return m.storage_add('qa.bin', new Uint8Array(c.data));
            case 'get': return Array.from(await m.storage_get(c.cid));
            case 'store-generated': {
              const data = new Uint8Array(c.size);
              // xorshift32: the high byte avoids a short repeating block pattern.
              let state = c.seed >>> 0;
              for (let i = 0; i < data.length; i++) {
                state ^= state << 13; state ^= state >>> 17; state ^= state << 5;
                data[i] = state >>> 24;
              }
              const cid = await m.storage_add('qa.bin', data);
              return { cid, ...await digest(data) };
            }
            case 'get-digest': return digest(await m.storage_get(c.cid));
            case 'control-config': window.qa.control.config = c; break;
            case 'control-stop': return window.qa.control.stop();
            case 'get-bench': {
              const started = performance.now();
              window.qa.control.start();
              try {
                const data = await m.storage_get(c.cid), fetched = performance.now();
                const result = await digest(data);
                if (result.size !== c.expected.size || result.sha256 !== c.expected.sha256) {
                  throw new Error(`size/SHA-256 mismatch: ${JSON.stringify(result)}`);
                }
                const finished = performance.now();
                return { ...result, getMs: fetched - started, verifyMs: finished - fetched,
                  completionMs: finished - started, ...window.qa.control.stop() };
              } finally { window.qa.control.stop(); }
            }
            default: throw new Error(`unknown command ${c.op}`);
          }
        }, command.op === 'init' && process.env.QA_WASM_LOG
          ? { ...command, wasmLogLevel: process.env.QA_WASM_LOG } : command), timeout, `${name} command ${command.op}`);
      },
      async close() {
        try {
          const remaining = await bounded(page.evaluate(() => window.qa?.events || []), 1000, 'final events').catch(() => []);
          await writeFile(join(directory, `${name}.log`), JSON.stringify({ errors, consoleLog, events: [...history, ...remaining] }));
          if (trace) await bounded(context.tracing.stop({ path: join(directory, `${name}-trace.zip`) }), TRACE_SAVE_DEADLINE_MS, 'save trace');
        } finally { await stopServer(); }
      },
    };
  } catch (error) { await stopServer(); throw error; }
}
