// CPU samples and trace timestamps use Chromium's monotonic microsecond clock.
export const percentile = (values, p) => {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted.length ? sorted[Math.max(0, Math.ceil(sorted.length * p) - 1)] : null;
};
const mistFrame = f => /wasm|mistlib_wasm/.test(f.url || '') || /wasm-function\[/.test(f.functionName || '');
export function analyze(profile, trace, page, phase) {
  const events = trace.traceEvents;
  const start = events.find(e => e.name === `sm:${phase}:start`);
  const end = events.find(e => e.name === `sm:${phase}:end`);
  if (!start || !end) throw new Error(`missing phase marks: ${phase}`);
  const lo = start.ts, hi = end.ts;
  const nodes = new Map(profile.nodes.map(n => [n.id, n]));
  const parents = new Map(profile.nodes.flatMap(n => (n.children || []).map(c => [c, n.id])));
  const stacks = new Map();
  for (const node of profile.nodes) {
    const stack = []; let n = node;
    while (n) { stack.push(n.callFrame); n = nodes.get(parents.get(n.id)); }
    stacks.set(node.id, stack);
  }
  // Chromium emits the post-task microtask checkpoint as a sibling of RunTask.
  // It still blocks rendering in the same turn; include it in the preceding
  // task. Inner checkpoints and duplicate scheduler aliases must not count twice.
  const tasks = events.filter(e => e.pid === start.pid && e.tid === start.tid &&
    e.ph === 'X' && /^(ThreadControllerImpl::RunTask|RunTask|BlinkScheduler_PerformMicrotaskCheckpoint)$/.test(e.name))
    .sort((a, b) => a.ts - b.ts || b.dur - a.dur)
    .reduce((outer, e) => {
      const previous = outer.at(-1);
      if (previous && e.name === 'BlinkScheduler_PerformMicrotaskCheckpoint') {
        previous.dur = Math.max(previous.dur, e.ts + e.dur - previous.ts);
      } else if (!previous || e.ts >= previous.ts + previous.dur) outer.push({ ...e });
      return outer;
    }, [])
    .filter(e => e.ts < hi && e.ts + e.dur > lo)
    .map(e => ({ start: Math.max(e.ts, lo), end: Math.min(e.ts + e.dur, hi), wall: e.dur / 1000, cpu: 0 }));
  if (!tasks.length) throw new Error('trace has no renderer task boundaries');
  const lt = tasks.filter(t => t.wall >= 50);
  const hot = new Map(), longHot = new Map(), stackHot = new Map();
  let clock = profile.startTime, total = 0, sampled = 0, mistInLongTasks = 0, cursor = 0;
  for (let i = 0; i < profile.samples.length; i++) {
    const next = clock + profile.timeDeltas[i];
    const a = Math.max(clock, lo), b = Math.min(next, hi), weight = Math.max(0, b - a) / 1000;
    const stack = stacks.get(profile.samples[i]);
    if (weight) {
      sampled += weight;
      if (stack.some(mistFrame)) {
        total += weight;
        const name = stack[0].functionName || stack[0].url || '(anonymous)';
        hot.set(name, (hot.get(name) || 0) + weight);
        const key = stack.slice(0, 8).map(f => f.functionName || f.url || '(anonymous)').join(' ← ');
        stackHot.set(key, (stackHot.get(key) || 0) + weight);
        while (cursor < tasks.length && tasks[cursor].end <= a) cursor++;
        for (let j = cursor; j < tasks.length && tasks[j].start < b; j++) {
          tasks[j].cpu += Math.max(0, Math.min(b, tasks[j].end) - Math.max(a, tasks[j].start)) / 1000;
        }
        if (lt.some(t => t.start < b && t.end > a)) {
          mistInLongTasks += weight;
          longHot.set(name, (longHot.get(name) || 0) + weight);
        }
      }
    }
    clock = next;
  }
  const window = page.phases[phase];
  const frames = page.frames.filter(f => f.t >= window.start && f.t <= window.end).map(f => f.dt);
  const longs = page.longtasks.filter(t => t.startTime < window.end && t.startTime + t.duration > window.start);
  const top = map => [...map].sort((a, b) => b[1] - a[1]).slice(0, 15).map(([name, ms]) => ({ name, ms }));
  return {
    durationMs: (hi - lo) / 1000,
    frameMs: { p50: percentile(frames, .5), p95: percentile(frames, .95), p99: percentile(frames, .99), max: Math.max(0, ...frames), over33: frames.filter(x => x > 33).length, over50: frames.filter(x => x > 50).length, count: frames.length },
    longtasks: { count: longs.length, maxMs: Math.max(0, ...longs.map(t => t.duration)) },
    // A sampled estimate of CPU contribution within one real task, not a hard
    // upper bound. The containing task's full wall duration is reported too.
    mistTaskCpuMaxMs: Math.max(0, ...tasks.map(t => t.cpu)),
    mistContainingTaskWallMaxMs: Math.max(0, ...tasks.filter(t => t.cpu > 0).map(t => t.wall)),
    mistTasksOver16: tasks.filter(t => t.cpu > 16).length,
    mistTaskTop: [...tasks].sort((a, b) => b.cpu - a.cpu).slice(0, 5)
      .map(t => ({ startUs: t.start, endUs: t.end, cpuMs: t.cpu, wallMs: t.wall })),
    mistMs: total, mistPercent: total / ((hi - lo) / 1000) * 100,
    sampledMs: sampled, mistInLongTasksMs: mistInLongTasks,
    hotSelf: top(hot), longtaskHotSelf: top(longHot), hotStacks: top(stackHot),
  };
}
