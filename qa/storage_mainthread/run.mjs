import { spawn, execFileSync } from 'node:child_process';
import { mkdir, appendFile, readFile, writeFile } from 'node:fs/promises';
import { resolve, join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
const { values: args } = parseArgs({ options: {
  package: { type: 'string', multiple: true }, rates: { type: 'string', default: '1,4' },
  'package-mode': { type: 'string', multiple: true },
  repeats: { type: 'string', default: '4' }, out: { type: 'string' },
  bytes: { type: 'string', default: String(20 * 1024 * 1024) },
  timeout: { type: 'string', default: '180' }, mode: { type: 'string', default: 'pinned' },
  mixed: { type: 'boolean', default: false },
  'block-workers': { type: 'boolean', default: false },
  'trace-wasm': { type: 'boolean', default: false },
} });
if (!args.package?.length) throw new Error('use --package label=/absolute/package/directory (repeat for alternating comparisons)');
const modes = ['pinned', 'add-pin', 'add-owned', 'add-owned-pin'];
if (!modes.includes(args.mode)) throw new Error(`mode must be one of ${modes}`);
const packages = args.package.map(arg => {
  const split = arg.indexOf('=');
  if (split <= 0) throw new Error('package must be label=path');
  return { label: arg.slice(0, split), path: resolve(arg.slice(split + 1)) };
});
const packageModes = new Map((args['package-mode'] || []).map(value => {
  const [label, mode] = value.split('=');
  if (!packages.some(p => p.label === label) || !modes.includes(mode)) throw new Error('invalid package-mode label=mode');
  return [label, mode];
}));
if (args.mixed && packages.length !== 2) throw new Error('mixed requires exactly two packages');
const out = resolve(args.out || `qa/storage_mainthread/results/${Date.now()}.jsonl`);
await mkdir(dirname(out), { recursive: true });
const rev = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
const rates = args.rates.split(',').map(Number), repeats = Number(args.repeats);
for (const rate of rates) if (![1, 4].includes(rate)) throw new Error('rate must be 1 or 4');
if (!Number.isInteger(repeats) || repeats < 1 || !Number.isFinite(Number(args.timeout)) || Number(args.timeout) <= 0 || !Number.isInteger(Number(args.bytes)) || Number(args.bytes) <= 0) throw new Error('invalid run limits');
const cells = args.mixed ? [
  { label: `${packages[0].label}-to-${packages[1].label}`, packages: packages.map(p => p.path), mode: packageModes.get(packages[0].label) || args.mode },
  { label: `${packages[1].label}-to-${packages[0].label}`, packages: packages.map(p => p.path).reverse(), mode: packageModes.get(packages[1].label) || args.mode },
] : packages.map(p => ({ label: p.label, packages: [p.path, p.path], mode: packageModes.get(p.label) || args.mode }));
for (const rate of rates) for (let repetition = 0; repetition < repeats; repetition++) {
  // AB / BA alternation also balances the first-run order.
  for (const cell of (repetition % 2 ? [...cells].reverse() : cells)) {
    const id = `${Date.now()}-${cell.label}-${rate}x-${repetition}`;
    const directory = out + '.artifacts/' + id;
    await mkdir(directory, { recursive: true });
    const spec = { ...cell, id, directory, rate, repetition, rev, bytes: Number(args.bytes),
      blockWorkers: args['block-workers'],
      traceWasm: args['trace-wasm'],
      transferTimeoutMs: Number(args.timeout) * 1000, timestamp: new Date().toISOString() };
    console.error(`start ${id}`);
    const child = spawn(process.execPath, [fileURLToPath(new URL('./attempt.mjs', import.meta.url)), JSON.stringify(spec)], { detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '', timedOut = false;
    child.stdout.on('data', data => { stdout += data; });
    child.stderr.on('data', data => { stderr += data; });
    const timer = setTimeout(() => { timedOut = true; try { process.kill(-child.pid, 'SIGKILL'); } catch {} }, (Number(args.timeout) + 90) * 1000);
    const code = await new Promise((resolve, reject) => { child.once('exit', resolve); child.once('error', reject); });
    clearTimeout(timer);
    if (timedOut || code !== 0) {
      try {
        const pids = JSON.parse(await readFile(join(directory, 'browser-pids.json'), 'utf8'));
        for (const pid of pids) { try { process.kill(-pid, 'SIGKILL'); } catch { try { process.kill(pid, 'SIGKILL'); } catch {} } }
      } catch { /* Browser launch may not have completed. */ }
    }
    let row;
    try { row = JSON.parse(await readFile(join(directory, 'result.json'), 'utf8')); }
    catch { row = { schema: 1, ...spec, success: false, error: timedOut ? 'attempt deadline (process group killed)' : `attempt exited ${code}`, peers: [] }; }
    await writeFile(join(directory, 'runner.log'), stderr + '\n' + stdout);
    await appendFile(out, JSON.stringify(row) + '\n');
    console.error(`${row.success ? 'ok' : 'FAILED'} ${id}: add=${row.addMs?.toFixed(1)} ms transfer=${row.transferMs?.toFixed(1)} ms ${row.error || ''}`);
    // A failed attempt still leaves its JSONL row; do not silently retry it.
    if (!row.success) process.exitCode = 1;
  }
}
console.error(`results: ${out}`);
