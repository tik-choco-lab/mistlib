import test from 'node:test';
import assert from 'node:assert/strict';
import { cells, deepMerge, parseSize, deadlineMs, profiles } from './storage_bench.mjs';

test('matrix has eight fresh directed attempts per quick repetition and deterministic seeds', () => {
  const plan = cells({ repeat: '2' });
  assert.equal(plan.length, 16);
  assert.deepEqual(plan, cells({ repeat: '2' }));
  assert.equal(new Set(plan.map(c => c.seed)).size, 16);
  assert.deepEqual(plan.filter(c => c.pair === 'chromium-firefox').map(c => c.direction),
    ['chromium->firefox', 'firefox->chromium', 'chromium->firefox', 'firefox->chromium']);
  assert.equal(cells({ pair: 'browser-native', browser: 'firefox' })[0].direction, 'firefox->native');
  assert.equal(cells({ pair: 'native-browser', browser: 'firefox' })[0].direction, 'native->firefox');
  assert.equal(cells({ pair: 'chromium-chromium', direction: 'reverse', seed: '123', repetition: '2' })[0].seed, 123);
});

test('sizes, selection, seeds and serialization deadlines are validated', () => {
  assert.equal(parseSize('32MiB'), 33554432);
  assert.equal(parseSize('1.5KiB'), 1536);
  for (const invalid of ['0', '-1', 'a', '1.1B']) assert.throws(() => parseSize(invalid));
  for (const invalid of [{ pair: 'no' }, { repeat: '0' }, { seed: '0' }, { profile: 'no' }, { direction: 'up' }]) {
    assert.throws(() => cells(invalid));
  }
  assert.equal(deadlineMs(parseSize('4MiB'), profiles.clean), 90000);
  assert.equal(deadlineMs(parseSize('32MiB'), profiles.wan150), 835306.368);
});

test('nested overrides reach the complete config without mutating the baseline', () => {
  const base = { signaling: { mode: 'nostr', nostr: { relays: ['local'], inviteCode: 'unique' } } };
  const config = deepMerge(base, { signaling: { nostr: { relays: ['override'] } }, experimental: { enabled: true } });
  assert.deepEqual(config.signaling.nostr, { relays: ['override'], inviteCode: 'unique' });
  assert.equal(config.experimental.enabled, true);
  assert.equal(base.signaling.nostr.relays[0], 'local');
  for (const override of [null, [], JSON.parse('{"__proto__":{"bad":true}}')]) {
    assert.throws(() => deepMerge(base, override));
  }
});
