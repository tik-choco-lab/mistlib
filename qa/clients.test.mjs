import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { browserClient } from './clients.mjs';

test('browser generates and hashes five distinct blocks locally, matching native PRNG', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'qa-client-unit-'));
  const commands = [], payloads = new Map();
  const page = {
    on() {}, async goto() {},
    async evaluate(fn, command) {
      // Skip browser bootstrap. Execute the actual command callback with fake
      // storage, without launching a browser or loading any release package.
      if (!command) return [];
      commands.push(command);
      return fn(command);
    },
  };
  const context = { tracing: { async start() {}, async stop() {} }, async newPage() { return page; } };
  const browser = { version() { return 'unit'; }, async newContext() { return context; } };
  const browserType = {
    async launchServer() { return { wsEndpoint() { return 'unit'; }, async kill() {} }; },
    async connect() { return browser; },
  };
  const logLevels = [], configs = [];
  const previousLog = process.env.QA_WASM_LOG;
  globalThis.window = { qa: { module: {
    set_log_level(level) { logLevels.push(level); return level === 'debug'; },
    init_with_config(_node, config) { configs.push(JSON.parse(config)); return true; },
    async storage_add(_name, data) {
      const cid = createHash('sha256').update(data).digest('hex');
      payloads.set(cid, data);
      return cid;
    },
    async storage_get(cid) { return payloads.get(cid); },
  } } };
  let client;
  try {
    client = await browserClient(browserType, 'unit', directory, 'unit');
    delete process.env.QA_WASM_LOG;
    await client.command({ op: 'init', node: 'unit', config: { signalingUrl: 'unit' } });
    assert.deepEqual(logLevels, [], 'default preserves the library log level');
    process.env.QA_WASM_LOG = 'debug';
    await client.command({ op: 'init', node: 'unit', config: { signalingUrl: 'unit' } });
    assert.deepEqual(logLevels, ['debug']);
    assert.deepEqual(configs, [{ signalingUrl: 'unit' }, { signalingUrl: 'unit' }]);
    process.env.QA_WASM_LOG = 'invalid';
    await assert.rejects(client.command({ op: 'init', node: 'unit', config: {} }), /log level rejected/);
    delete process.env.QA_WASM_LOG;
    // The same fixed vectors are checked in test_native.py.
    for (const [seed, sha256] of [
      [0x19a50001, 'bdf5642785b605326f142f265452b55f96a60027b461bf4a8e1632fe1bea34eb'],
      [0x19a50002, 'd670e3563ed51ed082188c390d1beb0c98a06e6df1f9c4da19e229d6a4bd6f70'],
    ]) {
      const size = 4 * 1024 * 1024 + 1;
      const stored = await client.command({ op: 'store-generated', seed, size });
      const expected = { sha256, size };
      assert.deepEqual({ sha256: stored.sha256, size: stored.size }, expected);
      assert.deepEqual(await client.command({ op: 'get-digest', cid: stored.cid }), expected);
      const data = payloads.get(stored.cid), blocks = new Set();
      for (let offset = 0; offset < size; offset += 1024 * 1024) {
        blocks.add(createHash('sha256').update(data.subarray(offset, offset + 1024 * 1024)).digest('hex'));
      }
      assert.equal(blocks.size, 5, 'each block requires distinct content');
    }
    assert.equal(payloads.size, 2, 'directions cannot satisfy each other from cache');
    assert(commands.every(command => JSON.stringify(command).length < 200), 'only metadata crosses command channel');
  } finally {
    if (client) await client.close();
    delete globalThis.window;
    if (previousLog === undefined) delete process.env.QA_WASM_LOG;
    else process.env.QA_WASM_LOG = previousLog;
    await rm(directory, { recursive: true, force: true });
  }
});
