// Recompute metrics from preserved traces without rerunning the experiment.
import { readFile, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { analyze } from './metrics.mjs';
const [input, output] = process.argv.slice(2);
if (!input || !output || resolve(input) === resolve(output)) throw new Error('use input.jsonl output.jsonl (distinct paths)');
const rows = (await readFile(input, 'utf8')).trim().split('\n').filter(Boolean).map(JSON.parse);
for (const row of rows) {
  for (const [i, peer] of row.peers.entries()) {
    if (!peer.phases) continue;
    const [profile, trace, page] = await Promise.all(['cpuprofile', 'trace.json', 'page.json']
      .map(suffix => readFile(join(row.directory, `${i}.${suffix}`), 'utf8').then(JSON.parse)));
    for (const phase of Object.keys(peer.phases)) peer.phases[phase] = analyze(profile, trace, page, phase);
  }
  row.analysisVersion = 2;
}
await writeFile(output, rows.map(JSON.stringify).join('\n') + '\n');
