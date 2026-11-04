import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const source = await readFile(new URL('../../mistlib-wasm/src/storage/owned.js', import.meta.url), 'utf8');
const { takeStorageInput } = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

test('transfer preserves a view and detaches every alias, including an empty view', () => {
  for (const length of [0, 3]) {
    const bytes = Uint8Array.from([0, 1, 2, 3, 4]);
    const view = bytes.subarray(1, 1 + length);
    const owned = takeStorageInput(view);
    assert.deepEqual([...owned], [1, 2, 3].slice(0, length));
    assert.equal(owned.byteOffset, 1);
    assert.equal(bytes.buffer.byteLength, 0);
    assert.equal(view.length, 0);
    assert.throws(() => takeStorageInput(view));
  }
});

test('shared and WebAssembly memory inputs reject without detaching', () => {
  const memory = new WebAssembly.Memory({ initial: 1 });
  for (const buffer of [new SharedArrayBuffer(16), memory.buffer]) {
    assert.throws(() => takeStorageInput(new Uint8Array(buffer)));
    assert.ok(buffer.byteLength > 0);
  }
});

test('missing transfer support rejects without a copying fallback', () => {
  const original = globalThis.structuredClone;
  const bytes = new Uint8Array(16);
  try {
    globalThis.structuredClone = undefined;
    assert.throws(() => takeStorageInput(bytes));
    assert.equal(bytes.byteLength, 16);
  } finally {
    globalThis.structuredClone = original;
  }
});
