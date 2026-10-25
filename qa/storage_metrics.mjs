// Counter-based metrics use admission bytes, including the small manifest block.
// Endpoints retain a baseline, 1 Hz samples, and an explicit final sample.
export function mergeSeries(rx, tx, started, ended) {
  const duration = Math.max(0, ended - started);
  const snapshot = sample => {
    const { t, ...stats } = sample;
    return stats;
  };
  const series = [];
  for (let i = 0; i < rx.length; i++) {
    const sample = rx[i];
    const t = i === 0 ? 0 : i === rx.length - 1 ? duration : Math.max(0, Math.min(duration, sample.t - started));
    const other = i === 0 ? tx[0] : i === rx.length - 1 ? tx.at(-1)
      : tx.reduce((best, item) => !best || Math.abs(item.t - sample.t) < Math.abs(best.t - sample.t) ? item : best, null);
    const point = { t, rx: snapshot(sample), tx: other ? snapshot(other) : {} };
    if (series.at(-1)?.t === t) series[series.length - 1] = point;
    else series.push(point);
  }
  return series;
}

export function deriveMetrics(series, size) {
  const missing = { partialBytes: null, maxNoProgressMs: null, duplicateBytes: null,
    steadyGoodputBps: null, metricsComplete: false };
  if (series.length < 2) return missing;
  const valid = value => Number.isFinite(value) && value >= 0;
  if (!series.every(p => valid(p.t) && valid(p.rx?.storage?.acceptedBytes) && valid(p.rx?.storage?.duplicateBytes))) return missing;
  const baseline = series[0].rx.storage;
  const samples = series.map(p => ({ t: p.t, bytes: p.rx.storage.acceptedBytes - baseline.acceptedBytes }));
  if (samples.some((p, i) => p.bytes < 0 || (i && (p.bytes < samples[i - 1].bytes || p.t < samples[i - 1].t)))) return missing;
  const total = samples.at(-1).bytes;
  const duplicateBytes = series.at(-1).rx.storage.duplicateBytes - baseline.duplicateBytes;
  if (duplicateBytes < 0) return missing;
  let lastProgress = 0, maxNoProgressMs = 0;
  for (let i = 1; i < samples.length; i++) {
    if (samples[i].bytes > samples[i - 1].bytes) {
      maxNoProgressMs = Math.max(maxNoProgressMs, samples[i].t - lastProgress);
      lastProgress = samples[i].t;
    }
  }
  maxNoProgressMs = Math.max(maxNoProgressMs, samples.at(-1).t - lastProgress);
  // Linear interpolation at the middle 60% of observed unique bytes. On a
  // capped attempt this measures the partial transfer; no completion required.
  const crossing = target => {
    for (let i = 1; i < samples.length; i++) {
      const a = samples[i - 1], b = samples[i];
      if (b.bytes >= target && b.bytes > a.bytes) return a.t + (b.t - a.t) * (target - a.bytes) / (b.bytes - a.bytes);
    }
    return null;
  };
  const span = total > 0 ? crossing(total * 0.8) - crossing(total * 0.2) : 0;
  return { partialBytes: Math.min(size, total), maxNoProgressMs, duplicateBytes,
    steadyGoodputBps: span > 0 ? total * 0.6 * 8 * 1000 / span : null, metricsComplete: true };
}
