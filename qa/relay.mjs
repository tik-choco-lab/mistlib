// Loopback-only Nostr fixture. Faults affect signaling; this is not a UDP emulator.
import { WebSocketServer, WebSocket } from 'ws';

export async function startRelay() {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0, maxPayload: 1024 * 1024 });
  await new Promise((resolve, reject) => {
    server.once('listening', resolve);
    server.once('error', reject);
  });
  const subscribers = new Map();
  const retained = new Map();
  const faults = { offline: false, dropNextEvent: false, duplicate: false, delayMs: 0 };
  const counts = { events: 0, delivered: 0, dropped: 0, disconnected: 0 };
  const matches = (event, filters) => filters.some(filter =>
    (!filter.kinds || filter.kinds.includes(event.kind)) &&
    (!filter.authors || filter.authors.includes(event.pubkey)) &&
    (!filter.since || event.created_at >= filter.since) &&
    Object.entries(filter).filter(([key]) => key.startsWith('#')).every(([key, values]) =>
      event.tags.some(tag => tag[0] === key.slice(1) && values.includes(tag[1]))));
  const send = (socket, value) => {
    if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(value));
  };
  const deliver = (socket, subscription, event) => {
    setTimeout(() => {
      if (faults.offline) return;
      send(socket, ['EVENT', subscription, event]);
      counts.delivered++;
      if (faults.duplicate) send(socket, ['EVENT', subscription, event]);
    }, faults.delayMs);
  };
  server.on('connection', socket => {
    if (faults.offline) return socket.terminate();
    subscribers.set(socket, new Map());
    socket.on('close', () => subscribers.delete(socket));
    socket.on('error', () => {});
    socket.on('message', bytes => {
      let message;
      try { message = JSON.parse(bytes.toString()); } catch { return socket.close(1007); }
      if (!Array.isArray(message)) return socket.close(1007);
      if (message[0] === 'REQ') {
        const [, id, ...filters] = message;
        subscribers.get(socket)?.set(id, filters);
        for (const event of retained.values()) {
          if (matches(event, filters)) deliver(socket, id, event);
        }
        send(socket, ['EOSE', id]);
      } else if (message[0] === 'CLOSE') {
        subscribers.get(socket)?.delete(message[1]);
      } else if (message[0] === 'EVENT') {
        const event = message[1];
        counts.events++;
        send(socket, ['OK', event.id, true, '']);
        if (faults.dropNextEvent) {
          faults.dropNextEvent = false;
          counts.dropped++;
          return;
        }
        // Bound retained discovery events; messages use ephemeral Nostr kinds.
        if (event.kind < 20000 || event.kind >= 30000) {
          retained.set(event.id, event);
          if (retained.size > 256) retained.delete(retained.keys().next().value);
        }
        for (const [peer, subscriptions] of subscribers) {
          for (const [id, filters] of subscriptions) {
            if (matches(event, filters)) deliver(peer, id, event);
          }
        }
      }
    });
  });
  return {
    url: `ws://127.0.0.1:${server.address().port}`,
    faults, counts,
    outage() {
      faults.offline = true;
      for (const socket of server.clients) { counts.disconnected++; socket.terminate(); }
    },
    async close() {
      for (const socket of server.clients) socket.terminate();
      await new Promise(resolve => server.close(resolve));
    },
  };
}
