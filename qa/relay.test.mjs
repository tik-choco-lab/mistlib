import test from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { WebSocket } from 'ws';
import { startRelay } from './relay.mjs';
import { until, bounded } from './clients.mjs';

test('a hung consumer API fails within its deadline; real errors stay failures', async () => {
  await assert.rejects(bounded(new Promise(() => {}), 20, 'hung join'), /deadline exceeded: hung join/);
  const failure = new Error('actual consumer failure');
  await assert.rejects(bounded(Promise.reject(failure), 1000, 'join'), error => error === failure);
  assert.equal(await bounded(Promise.resolve('done'), 1000, 'join'), 'done');
});

test('relay fixture routes rooms, injects a real drop/duplicate, and closes live sockets', async () => {
  const relay = await startRelay();
  const a = new WebSocket(relay.url), b = new WebSocket(relay.url);
  try {
    await Promise.all([once(a, 'open'), once(b, 'open')]);
    const messages = [];
    b.on('message', bytes => messages.push(JSON.parse(bytes.toString())));
    b.send(JSON.stringify(['REQ', 'room-a', { kinds: [25050], '#d': ['a'] }]));
    await until(() => messages, values => values.some(v => v[0] === 'EOSE'), 1000, 'subscription ready');
    const publish = (id, room = 'a') => a.send(JSON.stringify(['EVENT', { id, kind: 25050, pubkey: 'qa', tags: [['d', room]] }]));
    publish('wrong-room', 'b');
    await until(() => relay.counts.events, n => n === 1, 1000, 'wrong-room event processed');
    relay.faults.dropNextEvent = true;
    publish('dropped');
    await until(() => relay.counts.dropped, n => n === 1, 1000, 'injected drop');
    relay.faults.duplicate = true;
    publish('twice');
    await until(() => messages, values => values.filter(v => v[0] === 'EVENT').length === 2, 1000, 'duplicate delivery');
    assert.deepEqual(messages.filter(v => v[0] === 'EVENT').map(v => v[2].id), ['twice', 'twice']);
    const closed = Promise.all([once(a, 'close'), once(b, 'close')]);
    relay.outage();
    await closed;
    assert.equal(relay.counts.disconnected, 2);
  } finally { a.terminate(); b.terminate(); await relay.close(); }
});
