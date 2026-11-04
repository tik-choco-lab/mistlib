import test from 'node:test';
import assert from 'node:assert/strict';
import { analyze } from './metrics.mjs';
const frame = (id, functionName, url, children = []) => ({ id, callFrame: { functionName, url }, children });
const profile = { startTime: 1000000, nodes: [frame(1, '(root)', '', [2, 3]), frame(2, 'render', 'harness.js'), frame(3, 'invoke', 'mistlib_wasm.js', [4]), frame(4, 'copy', '')], samples: [2, 4, 4, 2], timeDeltas: [8000, 7000, 5000, 8000] };
const event = (name, ts, dur) => ({ name, ts, dur, ph: dur ? 'X' : 'I', pid: 1, tid: 2 });
const trace = { traceEvents: [event('sm:transfer:start', 1000000), event('ThreadControllerImpl::RunTask', 1000000, 28000), event('sm:transfer:end', 1028000)] };
const page = { phases: { transfer: { start: 0, end: 28 } }, frames: [{ t: 20, dt: 20 }], longtasks: [] };
test('attribute native children of glue, excluding rendering in the same task', () => {
  const m = analyze(profile, trace, page, 'transfer');
  assert.equal(m.mistMs, 12);
  assert.equal(m.mistTaskCpuMaxMs, 12);
  assert.equal(m.mistContainingTaskWallMaxMs, 28);
  assert.equal(m.mistTasksOver16, 0);
  assert.equal(m.longtasks.count, 0);
  assert.equal(m.frameMs.p95, 20);
});
test('sample contributions are split at real task boundaries', () => {
  const split = { traceEvents: [trace.traceEvents[0], event('RunTask', 1000000, 16000), event('RunTask', 1016000, 12000), trace.traceEvents[2]] };
  assert.equal(analyze(profile, split, page, 'transfer').mistTaskCpuMaxMs, 8);
});
test('missing task instrumentation is an error, not a passing zero', () => {
  assert.throws(() => analyze(profile, { traceEvents: trace.traceEvents.filter(e => e.ph !== 'X') }, page, 'transfer'), /no renderer task/);
});

test('nested task aliases do not double count CPU time', () => {
  const nested = { traceEvents: [...trace.traceEvents, event('RunTask', 1001000, 10000), event('RunTask', 1012000, 10000)] };
  const m = analyze(profile, nested, page, 'transfer');
  assert.equal(m.mistTaskCpuMaxMs, 12);
  assert.equal(m.mistContainingTaskWallMaxMs, 28);
});

test('post-task microtasks belong to the preceding rendering-blocking turn', () => {
  const checkpoint = { traceEvents: [trace.traceEvents[0], event('RunTask', 1000000, 8000),
    event('BlinkScheduler_PerformMicrotaskCheckpoint', 1008000, 12000),
    event('RunTask', 1020000, 8000), trace.traceEvents[2]] };
  const m = analyze(profile, checkpoint, page, 'transfer');
  assert.equal(m.mistTaskCpuMaxMs, 12);
  assert.equal(m.mistContainingTaskWallMaxMs, 20);
});
