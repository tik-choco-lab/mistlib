import test from 'node:test';
import assert from 'node:assert/strict';
import { mergeSeries, deriveMetrics } from './storage_metrics.mjs';

const point = (t, acceptedBytes, duplicateBytes = 0) => ({ t, rx: { storage: { acceptedBytes, duplicateBytes } }, tx: {} });
test('metrics include start, stalls, tail and subtract cumulative baselines', () => {
  const series = [point(0, 100, 10), point(1000, 100, 10), point(2000, 300, 20),
    point(3000, 500, 30), point(4000, 700, 30), point(5000, 900, 30), point(6000, 1100, 30), point(10000, 1100, 30)];
  assert.deepEqual(deriveMetrics(series, 2000), { partialBytes: 1000, maxNoProgressMs: 4000,
    duplicateBytes: 20, steadyGoodputBps: 1600, metricsComplete: true });
  assert.equal(deriveMetrics([point(0, 0), point(5000, 0)], 10).maxNoProgressMs, 5000);
  assert.equal(deriveMetrics([point(0, 0), point(5000, 0)], 10).steadyGoodputBps, null);
});
test('partial attempts get a middle 60% rate and missing/reset samples stay incomplete', () => {
  const series = [point(0, 0), point(1000, 200), point(2000, 400)];
  assert.equal(deriveMetrics(series, 10000).steadyGoodputBps, 1600);
  assert.equal(deriveMetrics(series, 10000).partialBytes, 400);
  for (const broken of [[], [point(0, 0)], [point(0, 100), point(1000, 50)],
    [point(0, 0), { t: 1000, rx: {} }]]) assert.equal(deriveMetrics(broken, 1000).metricsComplete, false);
});
test('endpoint samples align by timestamp and retain explicit baseline and end', () => {
  const rx = [{ t: 90, storage: { acceptedBytes: 5 } }, { t: 1100, storage: { acceptedBytes: 10 } },
    { t: 1600, storage: { acceptedBytes: 15 } }];
  const tx = [{ t: 80, nodes: [] }, { t: 1110, nodes: [{ id: 'peer' }] }, { t: 1610, nodes: [] }];
  const series = mergeSeries(rx, tx, 100, 1500);
  assert.deepEqual(series.map(p => p.t), [0, 1000, 1400]);
  assert.equal(series[1].tx.nodes[0].id, 'peer');
  assert.equal(series[2].rx.storage.acceptedBytes, 15);
  assert.equal('t' in series[0].rx, false);
});
