import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import assert from 'node:assert/strict';

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
    { cwd, env: { ...process.env, RUST_LOG: 'off' }, stdio: ['pipe', 'pipe', 'pipe'] });
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
    async command(command) {
      if (dead) throw dead;
      const id = ++serial;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`${name} command timed out: ${command.op}`));
        }, 30000);
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

export async function browserClient(browserType, url, directory, name) {
  // A separate browser process makes kill/restart a real process fault, rather
  // than relying on a tab close to release the browser's shared RTC resources.
  const server = await browserType.launchServer({ headless: true });
  try {
    const browser = await browserType.connect(server.wsEndpoint());
    const context = await browser.newContext();
    await context.tracing.start({ screenshots: true, snapshots: true, sources: true });
    const page = await context.newPage();
    const errors = [], consoleLog = [], history = [];
    page.on('pageerror', error => errors.push(String(error)));
    page.on('console', message => consoleLog.push({ type: message.type(), text: message.text() }));
    await page.goto(url);
    await page.evaluate(async () => {
      const module = await import('/node_modules/@tik-choco/mistlib/mistlib_wasm.js');
      await module.default();
      if (module.Delivery.Reliable !== 0 || module.MistEvent.Raw !== 0 ||
          module.get_version() !== JSON.parse(module.get_build_info()).version) {
        throw new Error('documented JS exports/version are inconsistent');
      }
      window.qa = { module, events: [] };
      module.register_event_callback((kind, sender, data, room) => {
        window.qa.events.push({ kind: ({ 0: 'raw', 5: 'connected', 6: 'disconnected', 7: 'aoi' })[kind] || String(kind),
          sender, room, data: Array.from(data) });
      });
    });
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
      async command(command) {
        assert.deepEqual(errors, [], `${name} browser errors`);
        return bounded(page.evaluate(async c => {
          const m = window.qa.module;
          switch (c.op) {
            case 'init': if (!m.init_with_config(c.node, JSON.stringify(c.config))) throw new Error('init rejected'); break;
            case 'join': await m.join_room_async(c.room); break;
            case 'leave': await m.leave_room_id_async(c.room); break;
            case 'position': m.update_position_in_room(c.room, ...c.position); break;
            case 'send': m.send_message_in_room(c.room, c.target, new Uint8Array(c.data), 0); break;
            case 'stats': return JSON.parse(m.get_stats());
            case 'store': return m.storage_add('qa.bin', new Uint8Array(c.data));
            case 'get': return Array.from(await m.storage_get(c.cid));
            default: throw new Error(`unknown command ${c.op}`);
          }
        }, command), 30000, `${name} command ${command.op}`);
      },
      async close() {
        try {
          const remaining = await bounded(page.evaluate(() => window.qa?.events || []), 1000, 'final events').catch(() => []);
          await writeFile(join(directory, `${name}.log`), JSON.stringify({ errors, consoleLog, events: [...history, ...remaining] }));
          await bounded(context.tracing.stop({ path: join(directory, `${name}-trace.zip`) }), TRACE_SAVE_DEADLINE_MS, 'save trace');
        } finally { await server.kill(); }
      },
    };
  } catch (error) { await server.kill(); throw error; }
}
