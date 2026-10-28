import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { linkSync, mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const tool = fileURLToPath(new URL('./parity-matrix.mjs', import.meta.url));
const identity = bytes => ({ bytes: Buffer.byteLength(bytes), sha256: createHash('sha256').update(bytes).digest('hex') });
const putJson = (file, value) => writeFileSync(file, JSON.stringify(value));
const clone = value => structuredClone(value);
function runtimeConcurrency(started, finished, reference) {
  return { observed: true, actual: 1, evidence: { kind: 'runtime-observation', coverage: 'entire-render',
    source: { kind: 'runtime-trace', mode: 'continuous', reference }, intervals: [{ started_utc: started, finished_utc: finished, active_jobs: 1 }],
    detail: 'observed maximum simultaneous processing jobs: 1' } };
}
function invariantConcurrency(started, finished, reference) {
  const value = runtimeConcurrency(started, finished, reference);
  value.evidence.source = { kind: 'single-queued-item-invariant', reference };
  value.evidence.invariant = { observed: true, queued_items: 1, max_jobs_per_item: 1, no_other_jobs: true,
    source: { kind: 'runtime-trace', mode: 'continuous', reference: `${reference}-queue` }, evidence: 'one queued item and absence of other jobs were observed',
    limit_source: { kind: 'implementation-proof', reference: `${reference}-bound` }, limit_evidence: 'one processing job per input item was established' };
  value.evidence.running = { observed: true, active_jobs: 1, started_utc: started, finished_utc: finished,
    source: { kind: 'runtime-trace', reference: `${reference}-running` }, evidence: 'one processing job was observed running' };
  value.evidence.detail = 'single-job invariant and actual running observation';
  return value;
}

// Synthetic receipts exercise comparison only; they are not reference-app evidence.
function makeCase(t) {
  const base = realpathSync(mkdtempSync(path.join(tmpdir(), 'parity-matrix-test-')));
  t.after(() => rmSync(base, { recursive: true, force: true }));
  const root = path.join(base, 'case'); mkdirSync(root);
  for (const dir of ['input', 'output', 'captures', 'parked', 'reports', 'state']) mkdirSync(path.join(root, dir));
  const input = 'generated fixture', output = 'identical rendered bytes';
  const inputIdentity = identity(input), binaryIdentity = identity('candidate executable');
  writeFileSync(path.join(root, 'input/fixture.wav'), input);
  putJson(path.join(root, 'manifest.json'), { schema: 'parity-matrix-case-v1', input: { path: 'input/fixture.wav', format: 'wav', identity: inputIdentity }, base_wav: { path: 'input/fixture.wav' } });
  const observation = {
    completed: true, generated_input_used: true, replacement_disabled: true,
    reference: { build: '10.5.0.1256', sha256: 'a'.repeat(64), architecture: 'arm64' },
    environment: { macos_build: '26B123', locale: 'en-US', timezone: 'UTC' },
    settings: { template_id: 'official-id', template_state: 'captured effective template', output_choice: 'WAV', mp3_mode: 'not-applicable', other: 'replacement and mirror off', jobs: 1 },
    timing: { started_utc: '2026-01-01T00:00:01Z', finished_utc: '2026-01-01T00:00:02Z' },
    process_context: 'PID 1234 with an empty queue',
    process_instance: { observed: true, pid: 1234, started_utc: '2026-01-01T00:00:00Z', evidence: 'process inspector recorded PID and launch time' },
    concurrency: runtimeConcurrency('2026-01-01T00:00:01Z', '2026-01-01T00:00:02Z', 'original-a-trace'),
    notes: 'the generated fixture completed'
  };
  const a = { schema: 'parity-matrix-capture-v1', label: 'a', role: 'original', source_authority: 'operator-attested-application-export', recorded_at_utc: '2026-01-01T00:00:03Z', input: inputIdentity, input_after: inputIdentity, output: { source_name: 'fixture_PN.wav', identity: identity(output), source_stat: { device: 1, inode: 100, bytes: Buffer.byteLength(output), mtime_ms: 1000, ctime_ms: 1000 }, artifact: 'output.bin' }, observation: clone(observation) };
  a.observation.render_event = { observed: true, id: 'render-a', evidence: 'the first separately initiated export completed' };
  const b = clone(a); b.label = 'b'; b.recorded_at_utc = '2026-01-01T00:00:06Z'; b.output.source_stat.inode = 101; b.observation.render_event = { observed: true, id: 'render-b', evidence: 'a new separately initiated export completed' }; b.observation.timing = { started_utc: '2026-01-01T00:00:04.100Z', finished_utc: '2026-01-01T00:00:05.200Z' };
  b.observation.process_instance.evidence = 'second process inspector recorded the same PID and launch time';
  b.observation.concurrency = runtimeConcurrency(b.observation.timing.started_utc, b.observation.timing.finished_utc, 'original-b-trace');
  const runtime = { platform: 'darwin', architecture: 'arm64', kernel_release: '27.2.0', macos_build: '26B123', node_version: 'v26.12.1', locale: 'en-US', timezone: 'UTC', environment: { LANG: 'en_US.UTF-8', LC_ALL: null, LC_CTYPE: null, TZ: 'UTC' } };
  const band = { threshold: -20, gain: 0, ratio: 1, attack: 1, release: 1 };
  const dynamics = { enabled: false, low: -12, high: 0, target_volume: -13, bands: Array.from({ length: 4 }, () => ({ ...band })), crossovers_hz: [95, 550, 5500] };
  const template = { unique_id: 'official-id', name: 'official', description: '', is_builtin: true, is_default: true, analysis_volume: -36, exciter_option: 0,
    clip: { enabled: true, low: -13, high: 0, percentage: 99, headroom: 1.5 }, pitch: { enabled: false, threshold_cents: 10, option: 0 },
    expansion: { ...clone(dynamics), headroom: 4.5 }, compression: clone(dynamics),
    output: { limiter_threshold: -11.5, margin: -0.6, character: 4, limiter_release: 3 } };
  const config = { libraries: { tag_bridge: null, flac: null, flac_sha256: null, lame: null, lame_sha256: null }, output: { format: 'wav', mode: { specific_folder: { directory: path.join(root, 'output'), mirror: false } } }, processor_max: 1, mp3_mode: 'cbr320', selected_template: null, effective_selected_template: { builtin: { id: 'official-id' } } };
  a.observation.effective_settings = { observed: true, template: clone(template), output: clone(config.output), replace_originals: false, mp3_mode: 'not-applicable', evidence: 'retained reference settings inspection records every processing value' };
  b.observation.effective_settings = clone(a.observation.effective_settings); b.observation.effective_settings.evidence = 'second reference settings inspection records every processing value';
  // The CLI prints an expanded binary64 representation of the same binary32.
  template.output.margin = -0.6000000238418579;
  const stdout = { command: 'process', status: 'ok', details: { jobs: 1, requested_jobs: 1, effective_config: config, effective_template: template }, results: [{ input: path.join(root, 'input/fixture.wav'), status: 'ok', report: { input_path: path.join(root, 'input/fixture.wav'), output_path: path.join(root, 'output/fixture_PN.wav'), applied_stages: ['clip_repair', 'warmth', 'limiter'] } }] };
  const r = { schema: 'parity-matrix-capture-v1', label: 'r', role: 'rust', source_authority: 'operator-selected-independent-cli', process_id: 1235, started_at_utc: '2026-01-01T00:00:10Z', finished_at_utc: '2026-01-01T00:00:11Z', runtime, input: inputIdentity, input_after: inputIdentity, executable: { path: '/test/platinum', identity: binaryIdentity }, executable_after: binaryIdentity, args: ['--state-dir', path.join(root, 'state'), '--json', 'process', path.join(root, 'input/fixture.wav'), '--template-id', 'official-id', '--format', 'wav', '--output-dir', path.join(root, 'output'), '--no-mirror', '--jobs', '1', '--mp3-mode', 'cbr320'], exit_status: 0, signal: null, error: null, output: clone(a.output), libraries_after: { tag_bridge: null, flac: null, lame: null } };
  r.output.source_stat.inode = 102;
  const s = clone(r); s.label = 's'; s.process_id = 1236; s.output.source_stat.inode = 103; s.started_at_utc = '2026-01-01T00:00:12Z'; s.finished_at_utc = '2026-01-01T00:00:13Z';
  const candidateContext = { pid: 1235, process_context: 'process inspector recorded the CLI child and loaded state', locale: 'en-US', timezone: 'UTC', environment_evidence: 'child locale and timezone observed in the process', loaded_libraries_evidence: 'process inventory confirmed no external codec libraries', concurrency: runtimeConcurrency(r.started_at_utc, r.finished_at_utc, 'candidate-r-trace') };
  const review = { same_effective_settings: true, settings_evidence: 'compared captured effective template and output settings', candidate: candidateContext, target_stages_observed: true, stage_evidence: { mode: 'observed', stages: [{ name: 'clip_repair', original_a: { state: 'active', observed: true, independent: true, source: { kind: 'processing-marker', reference: 'original-a-marker' }, evidence: 'original A marker inspection records clip repair' }, original_b: { state: 'active', observed: true, independent: true, source: { kind: 'processing-marker', reference: 'original-b-marker' }, evidence: 'original B marker inspection records clip repair' }, candidate: { state: 'active', observed: true, independent: true, source: { kind: 'runtime-trace', reference: 'candidate-r-stage-trace' }, evidence: 'independent candidate processing trace records clip repair' } }] } };
  const candidateReview = { candidate_a: clone(candidateContext), candidate_b: { ...clone(candidateContext), pid: 1236, concurrency: runtimeConcurrency(s.started_at_utc, s.finished_at_utc, 'candidate-s-trace') } };
  const receipts = { a, b, r, s }, outputs = { a: output, b: output, r: output, s: output }, values = { r: clone(stdout), s: clone(stdout) };
  for (const label of Object.keys(receipts)) mkdirSync(path.join(root, 'captures', label));
  function save(label) {
    const folder = path.join(root, 'captures', label);
    receipts[label].output.identity = identity(outputs[label]);
    if (receipts[label].output.source_stat) receipts[label].output.source_stat.bytes = receipts[label].output.identity.bytes;
    writeFileSync(path.join(folder, 'output.bin'), outputs[label]);
    if (values[label]) {
      const text = JSON.stringify(values[label]); writeFileSync(path.join(folder, 'stdout.json'), text);
      receipts[label].stdout = { artifact: 'stdout.json', identity: identity(text) };
    }
    putJson(path.join(folder, 'receipt.json'), receipts[label]);
  }
  for (const label of Object.keys(receipts)) save(label);
  let serial = 0;
  function compare(kind = 'original', selectedReview = undefined) {
    const report = `report-${++serial}`, args = ['--case', root, '--a', kind === 'candidates' ? 'r' : 'a', '--b', kind === 'candidates' ? 's' : 'b', '--report', report];
    if (kind === 'candidate') args.push('--rust', 'r');
    if (selectedReview !== undefined) { const p = path.join(base, `review-${serial}.json`); putJson(p, selectedReview); args.push('--review', p); }
    const result = spawnSync(process.execPath, [tool, kind === 'candidates' ? 'compare-candidates' : 'compare', ...args], { encoding: 'utf8' });
    const reportPath = path.join(root, 'reports', `${report}.json`);
    return { result, report: JSON.parse(readFileSync(reportPath, 'utf8')) };
  }
  return { base, root, receipts, outputs, values, review, candidateReview, save, compare };
}

test('valid original A/B use separately ordered times and preserve receipts and bytes', t => {
  const x = makeCase(t);
  x.receipts.a.observation.timing = { started_utc: '2026-01-01T00:00:01.123456789Z', finished_utc: '2026-01-01T00:00:01.123456790Z' };
  x.receipts.a.observation.concurrency = runtimeConcurrency(x.receipts.a.observation.timing.started_utc, x.receipts.a.observation.timing.finished_utc, 'original-a-precise-trace');
  x.receipts.b.observation.process_instance.started_utc = '2026-01-01T00:00:00.000000Z';
  x.save('a'); x.save('b');
  const before = readFileSync(path.join(x.root, 'captures/a/receipt.json'));
  const { result, report } = x.compare();
  assert.equal(result.status, 0, result.stderr);
  assert.equal(report.status, 'original-A-B-match-only');
  assert.equal(report.original_a_vs_b.identical, true);
  assert.notDeepEqual(report.timing.original_a, report.timing.original_b);
  assert.equal(report.metadata_normalized, false);
  assert.deepEqual(readFileSync(path.join(x.root, 'captures/a/receipt.json')), before);
  assert.equal(readFileSync(path.join(x.root, 'captures/a/output.bin'), 'utf8'), x.outputs.a);
});

test('unknown and unverified material never becomes a match just because both captures agree', async t => {
  const cases = [
    ['build', o => { o.reference.build = 'build unknown'; }],
    ['architecture', o => { o.reference.architecture = 'arm64 (unverified)'; }],
    ['locale', o => { o.environment.locale = 'unknown'; }],
    ['timezone', o => { o.environment.timezone = 'timezone not independently verified'; }],
    ['settings', o => { o.settings.template_state = 'Official selected; hidden settings unverified'; }],
    ['no reference settings evidence', o => { o.effective_settings.evidence = 'no evidence of matching effective settings'; }],
    ['legacy settings prose only', o => { delete o.effective_settings; }],
    ['missing typed parameter', o => { delete o.effective_settings.template.clip.low; }],
    ['placeholder settings', o => { o.settings.template_state = 'N/A'; }],
    ['output choice contradicts typed format', o => { o.settings.output_choice = 'MP3'; }],
    ['output choice ambiguous', o => { o.settings.output_choice = 'WAV or MP3'; }],
    ['template inapplicable placeholder', o => { o.settings.template_id = 'not-applicable'; }],
    ['nullable settings', o => { o.settings.hidden_state = null; }],
    ['nested settings', o => { o.settings.other = 'processor limit set; actual concurrency unknown'; }],
    ['jobs', o => { o.settings.jobs = null; }],
    ['process context', o => { o.process_context = 'process identity unknown'; }],
    ['process instance', o => { delete o.process_instance; }],
    ['process not observed', o => { o.process_instance.observed = false; }],
    ['never observed process', o => { o.process_instance.evidence = 'process identity never observed'; }],
    ['process evidence', o => { o.process_instance.evidence = 'not yet observed'; }],
    ['actual concurrency', o => { delete o.concurrency; }],
    ['concurrency not observed', o => { o.concurrency.observed = false; }],
    ['concurrency evidence', o => { o.concurrency.evidence = 'configured jobs only; actual concurrency unknown'; }],
    ['configured limit only', o => { o.concurrency.evidence = '1 processing job configured'; }],
    ['maximum concurrent jobs only', o => { o.concurrency.evidence = 'maximum concurrent jobs configured: 1'; }],
    ['maximum without configured word', o => { o.concurrency.evidence = 'maximum concurrent jobs: 1'; }],
    ['configured active maximum', o => { o.concurrency.evidence = 'maximum active jobs configured: 1'; }],
    ['bare worker count', o => { o.concurrency.evidence = '1 worker'; }],
    ['configured-only maximum wording', o => { o.concurrency.evidence = 'maximum concurrently running jobs: 1'; }],
    ['configured maximum mislabeled as runtime', o => { o.concurrency.evidence.source.kind = 'configured-limit'; o.concurrency.evidence.detail = 'maximum concurrently running jobs: 1'; }],
    ['continuous provenance absent', o => { delete o.concurrency.evidence.source.mode; }],
    ['single screenshot cannot cover render', o => { o.concurrency.evidence.source = { kind: 'operator-observation', mode: 'continuous', reference: '/tmp/worker-state.png' }; o.concurrency.evidence.detail = 'a single screenshot showed one running job'; }],
    ['missing interval evidence', o => { o.concurrency.evidence.intervals = []; }],
    ['interval coverage gap', o => { o.concurrency.evidence.intervals = [{ started_utc: o.timing.started_utc, finished_utc: o.timing.started_utc, active_jobs: 1 }, { started_utc: o.timing.finished_utc, finished_utc: o.timing.finished_utc, active_jobs: 1 }]; }],
    ['peak observed only outside render', o => { o.concurrency.evidence.intervals = [{ started_utc: '2025-12-31T23:59:59Z', finished_utc: '2026-01-02T00:00:00Z', active_jobs: 1 }]; }],
    ['peak count differs', o => { o.concurrency.evidence.intervals[0].active_jobs = 0; }],
    ['notes', o => { o.notes = 'effective settings could not be confirmed'; }]
  ];
  for (const [name, mutate] of cases) await t.test(name, t => {
    const x = makeCase(t); for (const label of ['a', 'b']) { mutate(x.receipts[label].observation); x.save(label); }
    const { result, report } = x.compare();
    assert.equal(result.status, 2, result.stderr); assert.equal(report.status, 'invalid-or-incomplete-case');
    assert.equal(report.original_a_vs_b.identical, true); assert.ok(report.flags.length);
  });
});

test('each original timing and identified process instance must be valid and ordered', async t => {
  const cases = [
    ['unknown', o => { o.timing.started_utc = 'unknown'; }],
    ['invalid calendar date', o => { o.timing.started_utc = '2026-02-30T00:00:01Z'; }],
    ['missing timezone', o => { o.timing.started_utc = '2026-01-01T00:00:01'; }],
    ['reversed', o => { o.timing.finished_utc = '2026-01-01T00:00:00Z'; }],
    ['sub-millisecond reversed', o => { o.timing.started_utc = '2026-01-01T00:00:01.000000002Z'; o.timing.finished_utc = '2026-01-01T00:00:01.000000001Z'; }],
    ['launch after render', o => { o.process_instance.started_utc = '2026-01-01T00:00:03Z'; }],
    ['different PID', o => { o.process_instance.pid = 9999; }],
    ['PID reuse with different launch', o => { o.process_instance.started_utc = '2025-12-31T23:59:59Z'; }],
    ['different context', o => { o.process_context = 'PID 1234 with prior jobs in the queue'; }]
  ];
  for (const [name, mutate] of cases) await t.test(name, t => {
    const x = makeCase(t); mutate(x.receipts.a.observation); x.save('a');
    const { result, report } = x.compare();
    assert.equal(result.status, 2, result.stderr); assert.equal(report.original_a_vs_b.identical, true); assert.ok(report.flags.length);
  });
});

test('a single-item invariant needs a separate in-render running observation', t => {
  const x = makeCase(t);
  for (const label of ['a', 'b']) {
    const obs = x.receipts[label].observation;
    obs.concurrency = invariantConcurrency(obs.timing.started_utc, obs.timing.finished_utc, `reference-${label}`); x.save(label);
  }
  x.review.candidate.concurrency = invariantConcurrency(x.receipts.r.started_at_utc, x.receipts.r.finished_at_utc, 'candidate-r');
  let run = x.compare('candidate', x.review); assert.equal(run.result.status, 0, run.result.stderr);
  const runningSnapshot = clone(x.review); runningSnapshot.candidate.concurrency.evidence.running.source = { kind: 'operator-observation', reference: '/tmp/running.png' };
  runningSnapshot.candidate.concurrency.evidence.running.evidence = 'one screenshot showed one running job';
  run = x.compare('candidate', runningSnapshot); assert.equal(run.result.status, 0, run.result.stderr);
  const noRunning = clone(x.review); delete noRunning.candidate.concurrency.evidence.running;
  run = x.compare('candidate', noRunning); assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_candidate.identical, true);
  const zeroRunning = clone(x.review); zeroRunning.candidate.concurrency.evidence.running.active_jobs = 0;
  run = x.compare('candidate', zeroRunning); assert.equal(run.result.status, 2, run.result.stderr);
  const otherJobs = clone(x.review); otherJobs.candidate.concurrency.evidence.invariant.no_other_jobs = false;
  run = x.compare('candidate', otherJobs); assert.equal(run.result.status, 2, run.result.stderr);
  const staticQueue = clone(x.review); staticQueue.candidate.concurrency.evidence.invariant.source.kind = 'implementation-proof';
  run = x.compare('candidate', staticQueue); assert.equal(run.result.status, 2, run.result.stderr);
  const snapshotQueue = clone(x.review); snapshotQueue.candidate.concurrency.evidence.invariant.source.reference = '/tmp/queue.png';
  run = x.compare('candidate', snapshotQueue); assert.equal(run.result.status, 2, run.result.stderr);
  const missingBound = clone(x.review); delete missingBound.candidate.concurrency.evidence.invariant.limit_source;
  run = x.compare('candidate', missingBound); assert.equal(run.result.status, 2, run.result.stderr);
  const late = clone(x.review); late.candidate.concurrency.evidence.running.started_utc = '2026-01-01T00:00:12Z';
  run = x.compare('candidate', late); assert.equal(run.result.status, 2, run.result.stderr);
});

test('full comparison requires separate per-capture stage evidence, but preserves A/B-only result', async t => {
  const cases = [
    ['unconfirmed', r => { r.target_stages_observed = false; r.stage_evidence = 'not observed'; }],
    ['boolean alone', r => { r.stage_evidence = 'yes'; }],
    ['legacy prose', r => { r.stage_evidence = 'clip repair happened'; }],
    ['missing original B', r => { delete r.stage_evidence.stages[0].original_b; }],
    ['not independently observed', r => { r.stage_evidence.stages[0].candidate.independent = false; }],
    ['CLI report alone', r => { r.stage_evidence.stages[0].candidate.evidence = 'candidate CLI stdout applied_stages'; }],
    ['inspected stdout is not independent', r => { r.stage_evidence.stages[0].candidate.evidence = 'inspected candidate CLI stdout'; }],
    ['inspected template is not independent', r => { r.stage_evidence.stages[0].original_a.evidence = 'inspected template settings'; }],
    ['stdout source is not independent', r => { r.stage_evidence.stages[0].candidate.source.kind = 'cli-stdout'; }],
    ['unverified candidate', r => { r.stage_evidence.stages[0].candidate.evidence = 'candidate stage not independently verified'; }],
    ['no stage evidence', r => { r.stage_evidence.stages[0].original_a.evidence = 'no evidence of clip repair'; }],
    ['negative evidence after subject', r => { r.stage_evidence.stages[0].original_a.evidence = 'evidence of clip repair is missing'; }],
    ['contradictory stage state', r => { r.stage_evidence.stages[0].candidate.state = 'inactive'; }],
    ['no stages', r => { r.stage_evidence.stages = []; }],
    ['settings unverified', r => { r.settings_evidence = 'settings selected; hidden settings unknown'; }],
    ['matching settings evidence missing', r => { r.settings_evidence = 'evidence of matching effective settings is missing'; }],
  ];
  for (const [name, mutate] of cases) await t.test(name, t => {
    const x = makeCase(t), review = clone(x.review); mutate(review);
    const { result, report } = x.compare('candidate', review);
    assert.equal(result.status, 2, result.stderr); assert.equal(report.status, 'invalid-or-incomplete-case');
    assert.equal(report.original_comparison_status, 'original-A-B-match-only');
    assert.equal(report.original_a_vs_b.identical, true); assert.equal(report.original_a_vs_candidate.identical, true);
  });
});

test('stage evidence cannot reference candidate stdout or template configuration', async t => {
  const direct = ['candidate stdout', 'the CLI stdout of candidate', 'stdout.json', 'effective_template', 'state/settings.json', 'exported.pntemplate'];
  for (const reference of direct) await t.test(reference, t => {
    const x = makeCase(t); x.review.stage_evidence.stages[0].candidate.source.reference = reference;
    const run = x.compare('candidate', x.review);
    assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_candidate.identical, true);
  });
  await t.test('hard-linked and copied stdout aliases', t => {
    const x = makeCase(t), stdout = path.join(x.root, 'captures/r/stdout.json');
    for (const [name, copy] of [['hardlink', false], ['copy', true]]) {
      const alias = path.join(x.base, `${name}-stage-evidence.json`);
      if (copy) writeFileSync(alias, readFileSync(stdout)); else linkSync(stdout, alias);
      x.review.stage_evidence.stages[0].candidate.source.reference = alias;
      const run = x.compare('candidate', x.review);
      assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_candidate.identical, true);
      assert.ok(run.report.flags.some(flag => flag.includes('bound to candidate stdout or template')));
    }
  });
});

test('complete observed and evidenced not-applicable stage reviews can pass', t => {
  const x = makeCase(t);
  let run = x.compare('candidate', x.review);
  assert.equal(run.result.status, 0, run.result.stderr); assert.equal(run.report.status, 'reviewed-complete-file-match-for-this-case');
  const withLimit = clone(x.review); withLimit.candidate.concurrency.evidence.detail = 'observed maximum simultaneous processing jobs: 1';
  run = x.compare('candidate', withLimit); assert.equal(run.result.status, 0, run.result.stderr);
  const namedTrace = clone(x.review); namedTrace.candidate.concurrency.evidence.source.reference = '/tmp/maximum-jobs.trace';
  run = x.compare('candidate', namedTrace); assert.equal(run.result.status, 0, run.result.stderr);
  const review = clone(x.review); review.target_stages_observed = false;
  review.stage_evidence = { mode: 'not-applicable', reason: 'case has no relevant target stage', original_a: { state: 'not-applicable', observed: true, independent: true, source: { kind: 'operator-observation', reference: 'original-a-no-target' }, evidence: 'A processing log shows no target stage' }, original_b: { state: 'not-applicable', observed: true, independent: true, source: { kind: 'operator-observation', reference: 'original-b-no-target' }, evidence: 'B processing log shows no target stage' }, candidate: { state: 'not-applicable', observed: true, independent: true, source: { kind: 'runtime-trace', reference: 'candidate-no-target' }, evidence: 'candidate processing log shows no target stage' } };
  run = x.compare('candidate', review);
  assert.equal(run.result.status, 2, run.result.stderr); assert.ok(run.report.flags.some(flag => flag.includes('contradicts candidate applied stages')));
  x.values.r.results[0].report.applied_stages = ['limiter']; x.values.r.details.effective_template.clip.enabled = false; x.values.r.details.effective_template.exciter_option = -1;
  for (const label of ['a', 'b']) { x.receipts[label].observation.effective_settings.template.clip.enabled = false; x.receipts[label].observation.effective_settings.template.exciter_option = -1; x.save(label); }
  x.save('r'); run = x.compare('candidate', review);
  assert.equal(run.result.status, 0, run.result.stderr); assert.equal(run.report.status, 'reviewed-complete-file-match-for-this-case');
  delete review.stage_evidence.original_b; run = x.compare('candidate', review);
  assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_candidate.identical, true);
});

test('candidate process, timing, effective environment and real concurrency need evidence', async t => {
  const cases = [
    ['missing child PID', x => { delete x.receipts.r.process_id; }],
    ['different child PID', x => { x.review.candidate.pid = 1236; }],
    ['unknown process context', x => { x.review.candidate.process_context = 'unknown'; }],
    ['unknown timing', x => { x.receipts.r.finished_at_utc = 'unknown'; }],
    ['reversed timing', x => { x.receipts.r.finished_at_utc = '2026-01-01T00:00:09Z'; }],
    ['unverified effective locale', x => { x.review.candidate.environment_evidence = 'locale not observed'; }],
    ['different timezone', x => { x.review.candidate.timezone = 'America/Chicago'; }],
    ['missing actual concurrency', x => { delete x.review.candidate.concurrency; }],
    ['configured jobs is not an observation', x => { x.review.candidate.concurrency.evidence = 'actual concurrency unknown; requested jobs 1'; }],
    ['only configured jobs', x => { x.review.candidate.concurrency.evidence = '1 processing job configured'; }],
    ['unknown effective template', x => { x.values.r.details.effective_template.name = 'official settings unverified'; }],
    ['nullable effective template', x => { x.values.r.details.effective_template.clip = null; }],
    ['missing template parameter', x => { delete x.values.r.details.effective_template.pitch.threshold_cents; }],
    ['wrong effective template identity', x => { x.values.r.details.effective_config.effective_selected_template.builtin.id = 'different'; }],
    ['missing library pin field', x => { delete x.values.r.details.effective_config.libraries.flac_sha256; }],
    ['pin without a configured library', x => { x.values.r.details.effective_config.libraries.flac_sha256 = 'a'.repeat(64); }],
    ['empty specific-folder mode', x => { x.values.r.details.effective_config.output.mode.specific_folder = {}; }],
    ['wrong output directory', x => { x.values.r.details.effective_config.output.mode.specific_folder.directory = '/another/output'; }],
    ['mirror enabled', x => { x.values.r.details.effective_config.output.mode.specific_folder.mirror = true; }],
    ['output replacement requested', x => { x.values.r.details.effective_config.overwrite = true; }],
    ['different configured format', x => { x.values.r.details.effective_config.output.format = 'mp3'; }],
    ['command mirror flag differs', x => { x.receipts.r.args[11] = '--mirror'; }],
    ['command input differs', x => { x.receipts.r.args[4] = '/another/input.wav'; }],
    ['reported input differs', x => { x.values.r.results[0].input = '/another/input.wav'; }],
    ['reported processing input differs', x => { x.values.r.results[0].report.input_path = '/another/input.wav'; }],
    ['reported output differs', x => { x.values.r.results[0].report.output_path = '/another/output.wav'; }],
    ['limiter missing from successful report', x => { x.values.r.results[0].report.applied_stages = ['clip_repair', 'warmth']; }],
    ['required warmth missing from successful report', x => { x.values.r.results[0].report.applied_stages = ['clip_repair', 'limiter']; }],
    ['warmth reported with disabled exciter', x => { x.values.r.details.effective_template.exciter_option = -1; for (const label of ['a', 'b']) { x.receipts[label].observation.effective_settings.template.exciter_option = -1; x.save(label); } }],
    ['candidate stage contradicts review', x => { x.values.r.results[0].report.applied_stages = []; }],
    ['applied stage is disabled in typed settings', x => { x.values.r.details.effective_template.clip.enabled = false; for (const label of ['a', 'b']) { x.receipts[label].observation.effective_settings.template.clip.enabled = false; x.save(label); } }],
    ['reference template identity differs', x => { for (const label of ['a', 'b']) { x.receipts[label].observation.settings.template_id = 'different'; x.receipts[label].observation.effective_settings.template.unique_id = 'different'; x.save(label); } }],
    ['processor maximum differs despite same actual concurrency', x => { x.values.r.details.effective_config.processor_max = 2; }],
    ['inconsistent effective jobs', x => { x.values.r.details.jobs = 2; }]
  ];
  for (const [name, mutate] of cases) await t.test(name, t => {
    const x = makeCase(t); mutate(x); x.save('r');
    const { result, report } = x.compare('candidate', x.review);
    assert.equal(result.status, 2, result.stderr); assert.equal(report.original_comparison_status, 'original-A-B-match-only');
    assert.equal(report.original_a_vs_candidate.identical, true); assert.ok(report.flags.length);
  });
});

test('typed reference values are independently compared to candidate processing values', t => {
  const x = makeCase(t);
  for (const label of ['a', 'b']) { x.receipts[label].observation.effective_settings.template.output.margin = -0.4; x.save(label); }
  const run = x.compare('candidate', x.review);
  assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_comparison_status, 'original-A-B-match-only');
  assert.ok(run.report.flags.some(flag => flag.includes('processing settings differ')));
});

test('effective floats compare as binary32 while integer settings remain exact', t => {
  const x = makeCase(t);
  let run = x.compare('candidate', x.review); assert.equal(run.result.status, 0, run.result.stderr);
  x.values.s.details.effective_template.output.margin = -0.6; x.save('s');
  run = x.compare('candidates', x.candidateReview); assert.equal(run.result.status, 0, run.result.stderr);
  x.values.r.details.effective_template.output.margin = -0.6000000834465027; x.save('r');
  run = x.compare('candidate', x.review); assert.equal(run.result.status, 2, run.result.stderr);
  assert.ok(run.report.flags.some(flag => flag.includes('processing settings differ')));
  x.values.r.details.effective_template.output.margin = -0.6;
  for (const label of ['a', 'b']) { x.receipts[label].observation.effective_settings.template.pitch.threshold_cents = 16777216; x.save(label); }
  x.values.r.details.effective_template.pitch.threshold_cents = 16777217; x.save('r');
  run = x.compare('candidate', x.review); assert.equal(run.result.status, 2, run.result.stderr);
  assert.ok(run.report.flags.some(flag => flag.includes('processing settings differ')));
});

test('typed processing comparison distinguishes negative zero from positive zero', t => {
  const x = makeCase(t);
  for (const label of ['a', 'b']) {
    x.receipts[label].observation.effective_settings.template.output.margin = 0; x.save(label);
    const file = path.join(x.root, 'captures', label, 'receipt.json'), text = readFileSync(file, 'utf8');
    assert.ok(text.includes('\"margin\":0,')); writeFileSync(file, text.replace('\"margin\":0,', '\"margin\":-0,'));
  }
  x.values.r.details.effective_template.output.margin = 0; x.save('r');
  const run = x.compare('candidate', x.review);
  assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_comparison_status, 'original-A-B-match-only');
  assert.ok(run.report.flags.some(flag => flag.includes('processing settings differ')));
});

test('capture-original preserves negative zero through receipt and strict comparison', t => {
  const x = makeCase(t), now = Date.now(), processStarted = new Date(now - 10000).toISOString();
  const command = (...args) => spawnSync(process.execPath, [tool, ...args], { encoding: 'utf8' });
  const observations = ['a', 'b'].map(label => clone(x.receipts[label].observation));
  for (const obs of observations) { obs.process_instance.started_utc = processStarted; obs.effective_settings.template.output.margin = 0; }
  observations[0].timing = { started_utc: new Date(now - 6000).toISOString(), finished_utc: new Date(now - 5000).toISOString() };
  observations[0].concurrency = runtimeConcurrency(observations[0].timing.started_utc, observations[0].timing.finished_utc, 'signed-a-trace');
  function saveObservation(index) {
    const file = path.join(x.base, `signed-${index}.json`), text = JSON.stringify(observations[index]);
    assert.ok(text.includes('\"margin\":0,')); writeFileSync(file, text.replace('\"margin\":0,', '\"margin\":-0,')); return file;
  }
  writeFileSync(path.join(x.root, 'output/fixture_PN.wav'), x.outputs.a);
  let result = command('capture-original', '--case', x.root, '--label', 'signed-a', '--file', 'fixture_PN.wav', '--observation', saveObservation(0));
  assert.equal(result.status, 0, result.stderr);
  const signedA = JSON.parse(readFileSync(path.join(x.root, 'captures/signed-a/receipt.json'), 'utf8'));
  assert.ok(Object.is(signedA.observation.effective_settings.template.output.margin, -0));
  result = command('park', '--case', x.root, '--label', 'signed-a'); assert.equal(result.status, 0, result.stderr);
  const startB = new Date(Date.now()).toISOString();
  writeFileSync(path.join(x.root, 'output/fixture_PN.wav'), x.outputs.b);
  observations[1].timing = { started_utc: startB, finished_utc: new Date(Date.now()).toISOString() };
  observations[1].concurrency = runtimeConcurrency(observations[1].timing.started_utc, observations[1].timing.finished_utc, 'signed-b-trace');
  result = command('capture-original', '--case', x.root, '--label', 'signed-b', '--file', 'fixture_PN.wav', '--observation', saveObservation(1));
  assert.equal(result.status, 0, result.stderr);
  const signedB = JSON.parse(readFileSync(path.join(x.root, 'captures/signed-b/receipt.json'), 'utf8'));
  assert.ok(Object.is(signedB.observation.effective_settings.template.output.margin, -0));
  x.values.r.details.effective_template.output.margin = 0; x.save('r');
  const reviewFile = path.join(x.base, 'signed-review.json'); putJson(reviewFile, x.review);
  result = command('compare', '--case', x.root, '--a', 'signed-a', '--b', 'signed-b', '--rust', 'r', '--review', reviewFile, '--report', 'signed-zero');
  const report = JSON.parse(readFileSync(path.join(x.root, 'reports/signed-zero.json'), 'utf8'));
  assert.equal(result.status, 2, result.stderr); assert.equal(report.original_comparison_status, 'original-A-B-match-only');
  assert.ok(report.flags.some(flag => flag.includes('processing settings differ')));
});

test('relevant MP3 modes must be known and match the candidate', t => {
  const x = makeCase(t);
  for (const label of ['a', 'b']) {
    const obs = x.receipts[label].observation;
    obs.settings.output_choice = 'MP3'; obs.settings.mp3_mode = 'cbr320'; obs.effective_settings.mp3_mode = 'cbr320'; obs.effective_settings.output.format = 'mp3';
    x.receipts[label].output.source_name = 'fixture_PN.mp3'; x.save(label);
  }
  x.receipts.r.output.source_name = 'fixture_PN.mp3'; x.receipts.r.args[8] = 'mp3'; x.receipts.r.args[15] = 'vbr192';
  x.values.r.details.effective_config.output.format = 'mp3'; x.values.r.details.effective_config.mp3_mode = 'vbr192';
  x.values.r.results[0].report.output_path = path.join(x.root, 'output/fixture_PN.mp3'); x.save('r');
  const run = x.compare('candidate', x.review);
  assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_comparison_status, 'original-A-B-match-only');
  assert.ok(run.report.flags.some(flag => flag.includes('MP3 mode differs')));
});

test('evidenced native export/import can map different IDs, but cannot hide processing differences', t => {
  const x = makeCase(t);
  for (const label of ['a', 'b']) {
    x.receipts[label].observation.settings.template_id = 'native-id';
    x.receipts[label].observation.effective_settings.template.unique_id = 'native-id';
    x.receipts[label].observation.effective_settings.template.is_builtin = false;
    x.receipts[label].observation.effective_settings.template.is_default = false; x.save(label);
  }
  x.values.r.details.effective_template.unique_id = 'export-id';
  x.values.r.details.effective_template.is_builtin = false; x.values.r.details.effective_template.is_default = false;
  x.values.r.details.effective_config.effective_selected_template = { custom: { index: 0, id: 'export-id' } };
  x.receipts.r.args[6] = 'export-id'; x.save('r');
  let run = x.compare('candidate', x.review); assert.equal(run.result.status, 2, run.result.stderr);
  x.review.template_identity = { observed: true, kind: 'native-export-import', source_id: 'native-id', export_id: 'export-id', imported_id: 'export-id', export_evidence: 'native source and exported copy IDs observed in retained export', import_evidence: 'candidate imported ID observed in the CLI template catalog' };
  run = x.compare('candidate', x.review); assert.equal(run.result.status, 0, run.result.stderr);
  x.review.template_identity.export_evidence = 'no evidence of native export identity';
  run = x.compare('candidate', x.review); assert.equal(run.result.status, 2, run.result.stderr);
  x.review.template_identity.export_evidence = 'native source and exported copy IDs observed in retained export';
  x.values.r.details.effective_template.pitch.threshold_cents++; x.save('r');
  run = x.compare('candidate', x.review); assert.equal(run.result.status, 2, run.result.stderr); assert.ok(run.report.flags.some(flag => flag.includes('processing settings differ')));
});

test('literal names and library paths containing Unknown are data, not uncertainty', t => {
  const x = makeCase(t), name = 'Unknown Pleasures', library = '/test/Unknown Pleasures/bridge.dylib';
  for (const label of ['a', 'b']) {
    x.receipts[label].observation.settings.template_state = name;
    x.receipts[label].observation.effective_settings.template.name = name; x.save(label);
  }
  x.values.r.details.effective_template.name = name;
  x.values.r.details.effective_config.libraries.tag_bridge = library;
  x.receipts.r.libraries_after.tag_bridge = { path: library, resolved: library, identity: identity('bridge') };
  x.review.settings_evidence = 'compared Unknown Pleasures effective settings and reference values';
  x.review.candidate.loaded_libraries_evidence = `process image inventory identified ${library}`;
  x.save('r'); const run = x.compare('candidate', x.review);
  assert.equal(run.result.status, 0, run.result.stderr); assert.equal(run.report.status, 'reviewed-complete-file-match-for-this-case');
});

test('a literal Unknown name and an explicitly null name are valid identified templates', async t => {
  for (const name of ['Unknown', null]) await t.test(String(name), t => {
    const x = makeCase(t);
    for (const label of ['a', 'b']) {
      const obs = x.receipts[label].observation;
      obs.effective_settings.template.name = name;
      obs.settings.template_state = name === null ? 'template name explicitly absent in native state' : name;
      x.save(label);
    }
    x.values.r.details.effective_template.name = name;
    x.review.settings_evidence = name === null ? 'inspected the native template with explicitly absent name and its processing values' : 'inspected the Unknown template and its processing values';
    x.save('r');
    let run = x.compare('candidate', x.review); assert.equal(run.result.status, 0, run.result.stderr);
    delete x.values.r.details.effective_template.name; x.save('r');
    run = x.compare('candidate', x.review); assert.equal(run.result.status, 2, run.result.stderr);
  });
});

test('locale spellings and reference hash case compare canonically, without weakening evidence', t => {
  const x = makeCase(t);
  x.receipts.a.observation.environment.locale = 'en-us'; x.receipts.b.observation.environment.locale = 'en-US';
  x.receipts.b.observation.reference.sha256 = x.receipts.b.observation.reference.sha256.toUpperCase();
  x.receipts.r.runtime.locale = 'EN-us'; x.review.candidate.locale = 'en-US';
  x.candidateReview.candidate_a.locale = 'en-us'; x.candidateReview.candidate_b.locale = 'EN-us';
  x.save('a'); x.save('b'); x.save('r');
  let run = x.compare('candidate', x.review); assert.equal(run.result.status, 0, run.result.stderr);
  run = x.compare('candidates', x.candidateReview); assert.equal(run.result.status, 0, run.result.stderr);
});

test('negative evidence about settings is rejected but no evidence of an error is allowed', t => {
  const x = makeCase(t);
  x.review.settings_evidence = 'inspection has no evidence of matching effective settings';
  let run = x.compare('candidate', x.review); assert.equal(run.result.status, 2, run.result.stderr);
  x.review.settings_evidence = 'settings inspections agree; no evidence of an error';
  run = x.compare('candidate', x.review); assert.equal(run.result.status, 0, run.result.stderr);
});

test('candidate stdout is receipt-bound while observed output bytes remain reportable', async t => {
  await t.test('missing legacy stdout identity', t => {
    const x = makeCase(t); delete x.receipts.r.stdout; putJson(path.join(x.root, 'captures/r/receipt.json'), x.receipts.r);
    const run = x.compare('candidate', x.review);
    assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_candidate.identical, true);
    assert.ok(run.report.flags.some(flag => flag.includes('stdout is unbound')));
  });
  await t.test('changed stdout', t => {
    const x = makeCase(t); x.values.r.details.effective_template.clip.low = -12;
    putJson(path.join(x.root, 'captures/r/stdout.json'), x.values.r);
    const run = x.compare('candidate', x.review);
    assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_candidate.identical, true);
    assert.ok(run.report.flags.some(flag => flag.includes('stdout is unbound')));
  });
});

test('matching bytes require distinct timed original render events and different source stats', t => {
  const x = makeCase(t);
  x.receipts.b.output.source_stat = clone(x.receipts.a.output.source_stat); x.receipts.b.output.source_stat.ctime_ms++; x.save('b');
  let run = x.compare(); assert.equal(run.result.status, 2, run.result.stderr);
  assert.ok(run.report.flags.some(flag => flag.includes('same unchanged file'))); assert.equal(run.report.original_a_vs_b.identical, true);
  x.receipts.b.output.source_stat.inode++; x.receipts.b.observation.render_event.id = 'render-a'; x.save('b');
  run = x.compare(); assert.equal(run.result.status, 2, run.result.stderr); assert.ok(run.report.flags.some(flag => flag.includes('render events are not distinct')));
  x.receipts.b.observation.render_event.id = 'render-b'; x.receipts.b.observation.timing.started_utc = '2026-01-01T00:00:01Z'; x.save('b');
  run = x.compare(); assert.equal(run.result.status, 2, run.result.stderr); assert.ok(run.report.flags.some(flag => flag.includes('not distinct ordered events')));
  delete x.receipts.b.output.source_stat; delete x.receipts.b.observation.render_event; x.save('b');
  run = x.compare(); assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_b.identical, true);
  assert.ok(run.report.flags.some(flag => flag.includes('source output stat missing')));
});

test('candidate source cannot reuse either original unchanged file', async t => {
  for (const label of ['a', 'b']) await t.test(label, t => {
    const x = makeCase(t); x.receipts.r.output.source_stat = clone(x.receipts[label].output.source_stat);
    x.receipts.r.output.source_stat.ctime_ms++; x.save('r');
    const run = x.compare('candidate', x.review);
    assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_candidate.identical, true);
    assert.ok(run.report.flags.some(flag => flag.includes('same unchanged original file')));
  });
});

test('the same unchanged export captured twice remains incomplete', t => {
  const x = makeCase(t); writeFileSync(path.join(x.root, 'output/fixture_PN.wav'), 'same original export');
  for (const [label, original] of [['first', 'a'], ['second', 'b']]) {
    const obs = path.join(x.base, `${label}.json`); putJson(obs, x.receipts[original].observation);
    const result = spawnSync(process.execPath, [tool, 'capture-original', '--case', x.root, '--label', label, '--file', 'fixture_PN.wav', '--observation', obs], { encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr);
  }
  const result = spawnSync(process.execPath, [tool, 'compare', '--case', x.root, '--a', 'first', '--b', 'second', '--report', 'same-source'], { encoding: 'utf8' });
  const report = JSON.parse(readFileSync(path.join(x.root, 'reports/same-source.json'), 'utf8'));
  assert.equal(result.status, 2, result.stderr); assert.equal(report.original_a_vs_b.identical, true);
  assert.ok(report.flags.some(flag => flag.includes('same unchanged file')));
});

test('comparison rebinds its actual hashes to the captured receipt after a replacement race', t => {
  const x = makeCase(t), instrumented = path.join(x.base, 'instrumented-matrix.mjs');
  const source = readFileSync(tool, 'utf8'), anchor = '  const aa = await compareBytes(file(aLabel), file(bLabel));';
  assert.equal(source.split(anchor).length, 2);
  writeFileSync(instrumented, source.replace(anchor, `  await writeFile(file(aLabel), process.env.MATRIX_REPLACEMENT);\n${anchor}`));
  const env = { ...process.env, MATRIX_REPLACEMENT: 'identical rendered byteS' };
  const result = spawnSync(process.execPath, [instrumented, 'compare', '--case', x.root, '--a', 'a', '--b', 'b', '--report', 'race'], { encoding: 'utf8', env });
  const report = JSON.parse(readFileSync(path.join(x.root, 'reports/race.json'), 'utf8'));
  assert.equal(result.status, 2, result.stderr); assert.equal(report.original_a_vs_b.identical, false);
  assert.ok(report.flags.some(flag => flag.includes('comparison bytes differ from capture receipts')));
});

test('configured libraries need actual loaded-identity evidence, not a post-run hash alone', t => {
  const x = makeCase(t); x.values.r.details.effective_config.libraries.tag_bridge = '/test/bridge.dylib';
  x.receipts.r.libraries_after.tag_bridge = { path: '/test/bridge.dylib', resolved: '/test/bridge.dylib', identity: identity('bridge'), observation: 'after candidate process; not proof of loaded-image identity' }; x.save('r');
  delete x.review.candidate.loaded_libraries_evidence;
  let run = x.compare('candidate', x.review);
  assert.equal(run.result.status, 2, run.result.stderr); assert.ok(run.report.flags.some(flag => flag.includes('loaded library')));
  x.review.candidate.loaded_libraries_evidence = 'process image inventory recorded the bridge identity';
  run = x.compare('candidate', x.review); assert.equal(run.result.status, 0, run.result.stderr);
});

test('candidate-only repetition needs separate process context and concurrency, not matching unknowns', t => {
  const x = makeCase(t);
  let run = x.compare('candidates'); assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.comparison.identical, true);
  run = x.compare('candidates', x.candidateReview); assert.equal(run.result.status, 0, run.result.stderr); assert.equal(run.report.status, 'candidate-repeated-byte-match');
  const review = clone(x.candidateReview); review.candidate_b.concurrency = { actual: 1, evidence: 'worker count unverified' };
  run = x.compare('candidates', review); assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.comparison.identical, true);
  x.receipts.s.started_at_utc = '2026-01-01T00:00:10.500Z'; x.save('s');
  run = x.compare('candidates', x.candidateReview); assert.equal(run.result.status, 2, run.result.stderr); assert.ok(run.report.flags.some(flag => flag.includes('timing overlaps')));
  x.receipts.s.started_at_utc = '2026-01-01T00:00:12Z'; x.receipts.s.output.source_stat = clone(x.receipts.r.output.source_stat); x.save('s');
  run = x.compare('candidates', x.candidateReview); assert.equal(run.result.status, 2, run.result.stderr); assert.ok(run.report.flags.some(flag => flag.includes('same unchanged file')));
  x.receipts.s.output.source_stat.inode = 103;
  x.receipts.s.started_at_utc = 'unknown'; x.save('s');
  run = x.compare('candidates', x.candidateReview); assert.equal(run.result.status, 2, run.result.stderr); assert.ok(run.report.flags.some(flag => flag.includes('timing')));
});

test('complete comparable byte mismatches exit 1 and report the exact difference', t => {
  const x = makeCase(t); x.outputs.r = 'identical rendered byteS'; x.save('r');
  let run = x.compare('candidate', x.review);
  assert.equal(run.result.status, 1, run.result.stderr); assert.equal(run.report.status, 'candidate-byte-mismatch');
  assert.equal(run.report.original_a_vs_candidate.first_difference.offset, 23);
  const incomplete = clone(x.review); incomplete.target_stages_observed = false;
  run = x.compare('candidate', incomplete); assert.equal(run.result.status, 2, run.result.stderr); assert.equal(run.report.original_a_vs_candidate.identical, false);
  run = x.compare('candidates', x.candidateReview); assert.equal(run.result.status, 1, run.result.stderr); assert.equal(run.report.status, 'candidate-repeated-byte-mismatch');
  x.outputs.b = 'identical rendered byteS'; x.save('b');
  run = x.compare(); assert.equal(run.result.status, 1, run.result.stderr); assert.equal(run.report.status, 'observed-original-A-B-mismatch');
});

test('capture still preserves unknown legacy observations without requiring new strict fields', t => {
  const x = makeCase(t), observation = clone(x.receipts.a.observation);
  observation.timing = { started_utc: 'unknown', finished_utc: 'unknown' }; observation.process_context = 'unknown';
  delete observation.process_instance; delete observation.concurrency;
  const obsPath = path.join(x.base, 'legacy-observation.json'); putJson(obsPath, observation);
  writeFileSync(path.join(x.root, 'output/fixture_PN.wav'), 'legacy export bytes');
  const result = spawnSync(process.execPath, [tool, 'capture-original', '--case', x.root, '--label', 'legacy', '--file', 'fixture_PN.wav', '--observation', obsPath], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  const receipt = JSON.parse(readFileSync(path.join(x.root, 'captures/legacy/receipt.json'), 'utf8'));
  assert.deepEqual(receipt.observation, observation);
  assert.equal(readFileSync(path.join(x.root, 'captures/legacy/output.bin'), 'utf8'), 'legacy export bytes');
});

test('new candidate captures retain the spawned child PID and process bounds', t => {
  const x = makeCase(t), executable = path.join(x.base, 'synthetic-cli.cjs');
  const source = `#!/usr/bin/env node
const { writeFileSync } = require('node:fs');
const path = require('node:path');
const args = process.argv.slice(2);
const input = args[args.indexOf('process') + 1];
const output = path.join(args[args.indexOf('--output-dir') + 1], 'fixture_PN.wav');
writeFileSync(output, 'synthetic candidate output');
process.stdout.write(JSON.stringify({ status: 'ok', details: { effective_config: { libraries: { tag_bridge: null, flac: null, lame: null } } }, results: [{ input, status: 'ok', report: { input_path: input, output_path: output } }] }));
`;
  writeFileSync(executable, source, { mode: 0o755 });
  const result = spawnSync(process.execPath, [tool, 'run', '--case', x.root, '--label', 'child', '--executable', executable, '--template-id', 'official-id', '--format', 'wav', '--mp3-mode', 'cbr320', '--jobs', '1'], { encoding: 'utf8', env: { ...process.env, PATH: `${path.dirname(process.execPath)}${path.delimiter}${process.env.PATH ?? ''}` } });
  assert.equal(result.status, 0, result.stderr);
  const receipt = JSON.parse(readFileSync(path.join(x.root, 'captures/child/receipt.json'), 'utf8'));
  assert.ok(Number.isSafeInteger(receipt.process_id) && receipt.process_id > 0);
  assert.ok(Date.parse(receipt.started_at_utc) <= Date.parse(receipt.finished_at_utc));
  assert.deepEqual(receipt.stdout.identity, identity(readFileSync(path.join(x.root, 'captures/child/stdout.json'))));
  assert.ok(Number.isSafeInteger(receipt.output.source_stat.inode));
  assert.equal(readFileSync(path.join(x.root, 'captures/child/output.bin'), 'utf8'), 'synthetic candidate output');
});
