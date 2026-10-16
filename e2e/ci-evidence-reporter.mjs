// AF-768: append evidence during execution so a killed runner leaves named operands.
import fs from 'node:fs';
import path from 'node:path';

export default class EvidenceReporter {
  constructor(options = {}) {
    this.dir = options.outputDir || process.env.AMUX_E2E_EVIDENCE_DIR;
    if (!this.dir) throw new Error('Evidence reporter needs an output directory');
  }
  write(name, value) {
    fs.writeFileSync(path.join(this.dir, name), JSON.stringify(value, null, 2) + '\n');
  }
  event(value) {
    fs.appendFileSync(path.join(this.dir, 'events.ndjson'), JSON.stringify(value) + '\n');
  }
  onBegin(config, suite) {
    fs.mkdirSync(this.dir, { recursive: true });
    // Never allow a previous successful final record to bless an interrupted rerun.
    fs.rmSync(path.join(this.dir, 'final.json'), { force: true });
    fs.writeFileSync(path.join(this.dir, 'events.ndjson'), '');
    const tests = suite.allTests().map(test => ({
      id: test.id, title: test.titlePath(), project: test.parent.project()?.name,
    }));
    // A planned shard (scripts/e2e-plan.mjs) is not a Playwright --shard, so
    // config.shard is null there; the plan's identity is what the verdict checks.
    const planned = /^(\d+)\/(\d+)$/.exec(process.env.AMUX_E2E_SHARD || '');
    const shard = planned ? { current: Number(planned[1]), total: Number(planned[2]) } : config.shard;
    this.write('manifest.json', { schema: 1, shard, tests });
    console.log(JSON.stringify({ event: 'e2e_evidence_started', measured: true,
      n_considered: tests.length, shard, directory: this.dir }));
  }
  onTestBegin(test, result) {
    this.event({ event: 'started', id: test.id, retry: result.retry });
  }
  onTestEnd(test, result) {
    this.event({ event: 'finished', id: test.id, retry: result.retry,
      status: result.status, expected: test.expectedStatus, outcome: test.outcome(),
      duration_ms: result.duration, errors: (result.errors || []).map(error => error.message || String(error)) });
  }
  onEnd(result) {
    this.write('final.json', { status: result.status, duration_ms: result.duration });
  }
}
