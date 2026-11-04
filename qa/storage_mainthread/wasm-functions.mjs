// Inspect names and function bodies without rebuilding the public package.
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
export function functions(bytes) {
  let offset = 8;
  const leb = () => { let v = 0, shift = 0, b; do { b = bytes[offset++]; v += (b & 127) * 2 ** shift; shift += 7; } while (b & 128); return v; };
  const str = () => { const len = leb(), start = offset; offset += len; return bytes.subarray(start, offset).toString(); };
  const limits = () => { const flags = leb(); leb(); if (flags & 1) leb(); };
  let imports = 0;
  const bodies = [], names = new Map();
  while (offset < bytes.length) {
    const id = bytes[offset++], size = leb(), end = offset + size;
    if (id === 2) {
      const count = leb();
      for (let i = 0; i < count; i++) {
        str(); str(); const kind = bytes[offset++];
        if (kind === 0) { imports++; leb(); }
        else if (kind === 1) { offset++; limits(); }
        else if (kind === 2) limits();
        else if (kind === 3) offset += 2;
        else if (kind === 4) { leb(); leb(); }
        else throw new Error(`unknown import kind ${kind}`);
      }
    } else if (id === 10) {
      const count = leb();
      for (let i = 0; i < count; i++) {
        const length = leb(), start = offset;
        const body = bytes.subarray(start, start + length);
        bodies.push({ index: imports + i, offset: start, length, sha256: createHash('sha256').update(body).digest('hex'), body });
        offset += length;
      }
    } else if (id === 0 && str() === 'name') {
      while (offset < end) {
        const sub = bytes[offset++], length = leb(), next = offset + length;
        if (sub === 1) {
          const count = leb(); for (let i = 0; i < count; i++) { const idx = leb(); names.set(idx, str()); }
        }
        offset = next;
      }
    }
    offset = end;
  }
  return bodies.map(b => ({ ...b, name: names.get(b.index) || null }));
}
if (process.argv[1]?.endsWith('wasm-functions.mjs')) {
  const [oldPath, namedPath] = process.argv.slice(2);
  const old = functions(await readFile(oldPath));
  const named = namedPath ? functions(await readFile(namedPath)) : [];
  for (const index of [175, 700]) {
    const fn = old.find(f => f.index === index);
    const { body, ...info } = fn;
    console.log(JSON.stringify({ ...info, exactBodyMatches: named.filter(f => f.sha256 === fn.sha256).map(({ body, ...f }) => f) }));
  }
}
