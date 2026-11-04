import { readFile } from 'node:fs/promises';
import { percentile } from './metrics.mjs';
const rows = (await Promise.all(process.argv.slice(2).map(p => readFile(p, 'utf8')))).flatMap(s => s.trim().split('\n').filter(Boolean).map(s => JSON.parse(s)));
const groups = new Map();
for (const row of rows) {
  const key = `${row.label} / ${row.rate}x / ${row.mode}`;
  if (!groups.has(key)) groups.set(key, []);
  groups.get(key).push(row);
}
console.log('| Build / CPU / API | OK / all | storage_add ms | storage_get ms |');
console.log('|---|---:|---:|---:|');
for (const [key, group] of groups) {
  const good = group.filter(r => r.success);
  const med = key => percentile(good.map(r => r[key]), .5)?.toFixed(1) ?? '—';
  console.log(`| ${key} | ${good.length}/${group.length} | ${med('addMs')} | ${med('transferMs')} |`);
}
console.log('');
console.log('| Build / CPU / API | OK / all | Side / phase | Duration ms | Frame p50/p95/p99/max ms | >33/>50 | Long tasks count/max ms | Mist task CPU max ms | Containing task max ms | Mist ms / % |');
console.log('|---|---:|---|---:|---|---|---|---:|---:|---|');
const fmt = x => x == null ? '—' : x.toFixed(1);
for (const [key, group] of groups) {
  const good = group.filter(r => r.success);
  for (const phase of ['idle', 'add', 'transfer']) for (const [i, side] of ['sender', 'receiver'].entries()) {
    const data = good.map(r => r.peers[i]?.phases?.[phase]).filter(Boolean);
    const median = fn => fmt(percentile(data.map(fn), .5));
    const max = fn => data.length ? fmt(Math.max(...data.map(fn))) : '—';
    console.log(`| ${key} | ${good.length}/${group.length} | ${side} ${phase} | ${median(x => x.durationMs)} | ${['p50','p95','p99'].map(p => median(x => x.frameMs[p])).join('/')} / ${max(x => x.frameMs.max)} | ${median(x => x.frameMs.over33)}/${median(x => x.frameMs.over50)} | ${median(x => x.longtasks.count)}/${max(x => x.longtasks.maxMs)} | ${max(x => x.mistTaskCpuMaxMs)} | ${max(x => x.mistContainingTaskWallMaxMs)} | ${median(x => x.mistMs)} / ${median(x => x.mistPercent)} |`);
  }
}
for (const row of rows.filter(r => !r.success)) console.log(`\nFAILED ${row.id}: ${row.error}`);
console.log('\nDurations, percentiles, counts and CPU totals: median across successful runs (lower median for even n). Maxima: worst run. Raw samples, failures and profiles remain in JSONL/artifacts. CPU attribution is sampled, not a strict upper bound.');
