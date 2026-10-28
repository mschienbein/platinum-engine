#!/usr/bin/env node
// Synthetic fixtures and separately attested original / independent CLI captures.
// Never launches, attaches to, modifies, or automates the reference application.
import { createHash } from 'node:crypto';
import { constants } from 'node:fs';
import { copyFile, chmod, link, lstat, mkdir, open, readFile, readdir, realpath, unlink, writeFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { release } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HELP = `Generated-audio complete-file parity workflow (Node.js; no app automation)

  node tools/parity-matrix.mjs init --case NEW_DIR --rate HZ --channels 1|2 \\
    --bits 16|24|32 --frames N --profile complex|tone|silence \\
    --hz-milli N --input-format wav|aiff|flac|alac|aac|mp3-cbr|mp3-vbr|mp3-no-xing|ogg \\
    [--ffmpeg /path/to/ffmpeg]
  node tools/parity-matrix.mjs preflight --case DIR
  node tools/parity-matrix.mjs capture-original --case DIR --label ID \\
    --file BASENAME_IN_OUTPUT --observation OBSERVATION.json
  node tools/parity-matrix.mjs park --case DIR --label ID
  node tools/parity-matrix.mjs run --case DIR --label ID --executable PATH \\
    --template-id ID --format wav|aiff|alac|flac|mp3|aac|match-input \\
    --mp3-mode cbr320|vbr192 [--jobs N] [--controlled-mp4-time-1904 SECONDS]
  node tools/parity-matrix.mjs compare --case DIR --a ID --b ID \\
    [--rust ID --review REVIEW.json] --report ID
  node tools/parity-matrix.mjs compare-candidates --case DIR --a ID --b ID \\
    [--review REVIEW.json] --report ID

The case directory MUST NOT already exist. Its parent must exist. init creates
only generated PCM; other formats use the explicitly supplied FFmpeg executable.
The CLI uses only DIR/state; configure its trusted public dependencies and any
custom template there. The reference app is operated independently by a human:
use exactly the generated input, the same empty DIR/output path and non-replace
settings for both exports. Record the actual output basename; do not guess it.
The candidate run uses one worker by default; --jobs records an explicit limit.
Capture preserves a read-only copy. park moves only the captured, unchanged
file from this tool-created output directory to an exclusive retained directory;
run park only after the export has completed and all writers have stopped.

Original observation JSON (schematic: replace the inline TEMPLATE below):
  {"completed":true,"generated_input_used":true,"replacement_disabled":true,
   "reference":{"build":"version","sha256":"64 hex digits","architecture":"arm64"},
   "environment":{"macos_build":"build","locale":"en-US","timezone":"UTC"},
   "settings":{"template_id":"native ID","template_state":"effective state evidence",
      "output_choice":"WAV","mp3_mode":"mode or not-applicable","other":"other settings","jobs":1},
   "effective_settings":{"observed":true,"template":TEMPLATE,
      "output":{"format":"wav","mode":{"specific_folder":{"directory":"/absolute/case/output","mirror":false}}},
      "replace_originals":false,"mp3_mode":"not-applicable","evidence":"reference settings observations"},
   "timing":{"started_utc":"2026-01-01T00:00:01Z","finished_utc":"2026-01-01T00:00:02Z"},
   "process_context":"observed process and state",
   "process_instance":{"observed":true,"pid":1234,"started_utc":"2026-01-01T00:00:00Z","evidence":"observed process identity"},
   "concurrency":{"observed":true,"actual":1,"evidence":RUNTIME_EVIDENCE},
   "render_event":{"observed":true,"id":"unique render ID","evidence":"separate export observation"},
   "notes":"capture details"}
TEMPLATE is the independently observed complete effective processing state, not
a copy of candidate stdout: unique_id, name, is_builtin, is_default, numeric
analysis_volume and exciter_option; clip {enabled,low,high,percentage,headroom};
pitch {enabled,threshold_cents,option}; expansion {enabled,low,high,target_volume,
headroom,bands,crossovers_hz}; compression {enabled,low,high,target_volume,bands,
crossovers_hz}; and output {limiter_threshold,margin,character,limiter_release}.
Each bands array has four objects containing numeric threshold, gain, ratio,
attack and release; each crossovers_hz has three numbers. An optional description
does not affect processing comparison. name must be present and may be null.
template.unique_id must equal the native
settings.template_id. Paths are absolute and the directory is the real case output.
output_choice must unambiguously name the same format as effective_settings.output.
Float fields compare by binary32 identity, including signed zero; integer fields
compare exactly. This does not normalize any captured file bytes.
For MP3 output (including match_input from MP3), record the actual cbr320/vbr192 mode.

Capture accepts legacy/unknown values and stores the validated observation JSON
verbatim, including signed zero. A/B
strictly requires known matching settings, process context and identified process
instance, plus individually evidenced render events with distinct IDs and source
file stats. The first captured export must precede the second render. Source stats
and operator descriptions are evidence, not independent proof of rendering.
Each render needs ordered UTC bounds in YYYY-MM-DDTHH:mm:ss[.fraction]Z form;
the A/B render times need not match. Actual concurrency is the observed peak
simultaneous processing jobs, not a configured maximum or an OS thread count.
RUNTIME_EVIDENCE is {"kind":"runtime-observation","coverage":"entire-render",
 "source":{"kind":"runtime-trace","mode":"continuous","reference":"independent source ID or path"},
 "intervals":[{"started_utc":"2026-01-01T00:00:01Z",
               "finished_utc":"2026-01-01T00:00:02Z","active_jobs":1}],
 "detail":"observed runtime peak"}. Intervals are ordered, overlapping or
adjacent evidence windows whose union covers the render; active_jobs is the peak
within each window. Their maximum must equal actual, and a maximum-bearing window
must lie inside the render. A configured limit is not a runtime observation.
Alternatively source.kind may be "single-queued-item-invariant", with separately
evidenced invariant {observed:true,queued_items:1,max_jobs_per_item:1,
no_other_jobs:true,source:{kind,reference},evidence,limit_source:{kind,reference},
limit_evidence} and running {observed:true,active_jobs:1,started_utc,finished_utc,
source:{kind,reference},evidence}. The invariant establishes <=1 for the whole
render; the running observation inside the render establishes >=1.
Invariant queue/no-other-jobs and running sources must be runtime-trace or
operator-observation; limit_source may also be implementation-proof. Full-render
and queue/no-other-job sources require mode:"continuous", not a single screenshot.
These are operator attestations, not authenticated scheduler proof.

For compare --rust, separately review captured CLI and reference evidence:
  {"same_effective_settings":true,"settings_evidence":"specific observed facts",
   "candidate":{"pid":1235,"process_context":"observed child process and state",
      "locale":"en-US","timezone":"UTC","environment_evidence":"observed facts",
      "loaded_libraries_evidence":"observed loaded identities, or why none are relevant",
      "concurrency":{"observed":true,"actual":1,"evidence":RUNTIME_EVIDENCE}},
   "target_stages_observed":true,
   "stage_evidence":{"mode":"observed","stages":[{"name":"candidate stage ID",
      "original_a":{"state":"active","observed":true,"independent":true,"source":{"kind":"processing-marker","reference":"A marker"},"evidence":"independent fact"},
      "original_b":{"state":"active","observed":true,"independent":true,"source":{"kind":"processing-marker","reference":"B marker"},"evidence":"independent fact"},
      "candidate":{"state":"active","observed":true,"independent":true,"source":{"kind":"runtime-trace","reference":"candidate trace"},"evidence":"independent fact"}}]}}
Each stage state is active or inactive; states must agree and at least one target
must be active. Candidate states must agree with its bound applied_stages. Each
observation attests evidence independent of template selection and CLI stage claims.
Stage source kinds are runtime-trace, processing-marker or operator-observation;
inspecting only CLI stdout or template settings is not independent stage evidence.
References to the candidate stdout or template configuration, including detected
local copies, are not independent. Every successful CLI report must include its
limiter, and must include warmth when exciter_option is nonnegative.
For no relevant stage activation, target_stages_observed:false requires
stage_evidence {"mode":"not-applicable","reason":"case-specific reason",
 "original_a":{"state":"not-applicable","observed":true,"independent":true,"source":{"kind":"operator-observation","reference":"A evidence"},"evidence":"fact"},
 "original_b":{"state":"not-applicable","observed":true,"independent":true,"source":{"kind":"operator-observation","reference":"B evidence"},"evidence":"fact"},
 "candidate":{"state":"not-applicable","observed":true,"independent":true,"source":{"kind":"runtime-trace","reference":"candidate evidence"},"evidence":"fact"}}.
The candidate may report only its unconditional limiter; any other applied stage
contradicts this review. A template, boolean, byte match or configured worker
limit alone is not stage or concurrency evidence.

If native and imported candidate template IDs differ, review must also include
"template_identity":{"observed":true,"kind":"native-export-import",
 "source_id":"native ID","export_id":"new exported ID","imported_id":"exported ID",
 "export_evidence":"source-to-export observation","import_evidence":"import observation"}.
Even with this mapping, the typed processing values must match exactly.
Candidate runs record their PID, timing and stdout digest; older receipts lacking
these, distinct render source stats, or concrete settings remain incomplete and
must not be rewritten. compare-candidates accepts --review with candidate_a and
candidate_b contexts; each process is a separate run. Incomplete comparisons
still report observed bytes and the independent A/B-only status. Captures and
reviews are operator attestations, not authenticated GUI/process observations.
Reports and all captures are exclusive, preserved, and never normalized. A single
AA match demonstrates only observed repetition, not general determinism. Exit 0
means the requested comparison met its strict gate; 1 means a byte mismatch with
complete comparable evidence; 2 means invalid/incomplete evidence or an operation
error. No case proves global parity.
`;

function fail(message) { throw new Error(message); }
function required(o, key) { const v = o[key]; if (v === undefined || v === '') fail(`missing --${key}`); return v; }
function intArg(o, key, min, max) {
  const s = required(o, key);
  if (!/^[0-9]+$/.test(s)) fail(`--${key} must be an integer`);
  const n = Number(s);
  if (!Number.isSafeInteger(n) || n < min || n > max) fail(`--${key} must be ${min}..${max}`);
  return n;
}
function options(args, allowed) {
  const result = {};
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i]?.startsWith('--') ? args[i].slice(2) : '';
    if (!allowed.includes(key) || result[key] !== undefined || args[i + 1] === undefined || args[i + 1].startsWith('--')) fail(`invalid or duplicate option: ${args[i]}`);
    result[key] = args[i + 1];
  }
  return result;
}
function id(value) { if (!/^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$/.test(value)) fail('label/report must be a simple 1..64 character identifier'); return value; }
function basename(value) { if (value === '.' || value === '..' || path.basename(value) !== value || value.includes('\0')) fail('file must be a basename in the case output directory'); return value; }
function nonempty(value) { return typeof value === 'string' && value.trim().length > 0; }
// Free-form evidence can only attest what the operator supplied. Treat material
// uncertainty conservatively; this is not a semantic or authenticity check.
const UNCERTAIN = /\b(?:unknown|unverified|unconfirmed|unobserved|uncertain|unspecified|undetermined|unavailable|unresolved|unsure|unreported|unrecorded|unidentified|unattested|unproven|incomplete|pending|tbd|possibly|maybe|assumed|assumption|guessed|(?:not|never)\s+(?:(?:yet|independently|fully|actually|be)\s+)*(?:verified|confirmed|observed|known|recorded|captured|identified|established|attested|available|measured|determined|checked)|cannot\s+(?:be\s+)?(?:verified|confirmed|observed|known|measured|determined))\b/i;
function notApplicable(value) { return typeof value === 'string' && /^not[-\s]+applicable[.! ]*$/i.test(value.trim()); }
function knownIdentity(value) { return nonempty(value) && !notApplicable(value) && !/^(?:unknown|unverified|unobserved|unconfirmed|n\/?a|null|nil|none|[-?]+)[.! ]*$/i.test(value.trim()); }
function knownText(value) { return knownIdentity(value) && !UNCERTAIN.test(value) && !/^(?:no|missing)\s+(?:data|evidence|details|information)[.! ]*$/i.test(value.trim()); }
function evidenceText(value, literals = []) {
  if (!knownIdentity(value)) return false;
  let text = value;
  for (const literal of literals) {
    if (knownIdentity(literal)) text = text.split(literal).join('[identity]');
    else if (nonempty(literal)) {
      const escaped = literal.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      text = text.replace(new RegExp(`\\b${escaped}(?=\\s+(?:template|preset)\\b)`, 'gi'), '[identity]')
        .replace(new RegExp(`(\\b(?:template|preset)\\s+)${escaped}\\b`, 'gi'), '$1[identity]');
    }
  }
  text = text.replace(/(?:\/[^\s,;]+)+/g, '[path]');
  if (!knownText(text) || /^(?:yes|no|true|false)[.! ]*$/i.test(text.trim())) return false;
  if (/\b(?:evidence|proof)\b[^.;\n]{0,160}?\b(?:is|was|remains|still)\s+(?:missing|absent|unavailable|lacking)\b/i.test(text)) return false;
  const negative = /\b(?:no|without|lacks?|lacking|missing)\s+(?:(?:independent|direct|supporting)\s+)?evidence\b(?:\s+(?:of|for|that)\s+)?/gi;
  for (const match of text.matchAll(negative)) {
    if (!/^(?:an?\s+)?(?:errors?|failures?|faults?|problems?|defects?|exceptions?)\b/i.test(text.slice(match.index + match[0].length))) return false;
  }
  return true;
}
function containsUncertainty(value, allowNull = false) {
  if (value === null || value === undefined) return !allowNull;
  if (typeof value === 'string') return !knownText(value);
  if (Array.isArray(value)) return value.some(item => containsUncertainty(item, allowNull));
  if (typeof value === 'object') return Object.entries(value).some(([key, item]) => !(key === 'mp3_mode' && notApplicable(item)) && !(key === 'selected_template' && item === null) && containsUncertainty(item, allowNull || key === 'libraries'));
  return false;
}
function utcInstant(value) {
  if (typeof value !== 'string') return null;
  const match = /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.(\d+))?Z$/.exec(value);
  if (!match) return null;
  const second = `${match[1]}.000Z`, ms = Date.parse(second);
  return Number.isFinite(ms) && new Date(ms).toISOString() === second ? { ms, fraction: (match[2] ?? '').replace(/0+$/, '') } : null;
}
function compareInstants(a, b) {
  if (a.ms !== b.ms) return a.ms < b.ms ? -1 : 1;
  const length = Math.max(a.fraction.length, b.fraction.length);
  const left = a.fraction.padEnd(length, '0'), right = b.fraction.padEnd(length, '0');
  return left === right ? 0 : left < right ? -1 : 1;
}
function timingBounds(start, finish, label, flags) {
  const first = utcInstant(start), last = utcInstant(finish);
  if (first === null || last === null || compareInstants(first, last) > 0) {
    flags.push(`${label} timing unknown, invalid or out of order`);
    return null;
  }
  return [first, last];
}
function canonicalLocale(value) {
  if (!knownIdentity(value)) return null;
  try { return Intl.getCanonicalLocales(value)[0] ?? null; } catch { return null; }
}
function validLocale(value) { return canonicalLocale(value) !== null; }
function validTimezone(value) {
  if (!knownText(value)) return false;
  try { new Intl.DateTimeFormat('en-US', { timeZone: value }); return true; } catch { return false; }
}
function positiveInt(value) { return Number.isSafeInteger(value) && value > 0; }
function validSourceName(value) { return nonempty(value) && value !== '.' && value !== '..' && path.basename(value) === value && !value.includes('\0'); }
function validIdentity(value) {
  return value && Number.isSafeInteger(value.bytes) && value.bytes >= 0 && typeof value.sha256 === 'string' && /^[a-f0-9]{64}$/i.test(value.sha256);
}
function evidenceSource(value, allowed) {
  return objectValue(value) && allowed.includes(value.kind) && knownIdentity(value.reference);
}
function continuousSource(source, detail) {
  if (source?.mode !== 'continuous' || /\.(?:png|jpe?g|heic|gif|bmp|webp)$/i.test(source.reference)) return false;
  return !/\b(?:(?:single|one|only|sole)\s+(?:(?:ui|screen|still|runtime)\s+)?(?:screenshot|snapshot|frame|image|sample)|(?:screenshot|snapshot|still\s+image)\s+only)\b/i.test(detail);
}
function concurrencyEvidence(value, actual, bounds, literals) {
  if (!bounds || !objectValue(value) || value.kind !== 'runtime-observation' || value.coverage !== 'entire-render' ||
      !evidenceSource(value.source, ['runtime-trace', 'operator-observation', 'single-queued-item-invariant']) || !evidenceText(value.detail, literals) ||
      !Array.isArray(value.intervals) || !value.intervals.length) return false;
  if (value.source.kind !== 'single-queued-item-invariant' && !continuousSource(value.source, value.detail)) return false;
  const intervals = []; let first = null, last = null, priorStart = null, peak = 0;
  for (const interval of value.intervals) {
    const start = utcInstant(interval?.started_utc), finish = utcInstant(interval?.finished_utc);
    if (!start || !finish || compareInstants(start, finish) > 0 || !Number.isSafeInteger(interval?.active_jobs) || interval.active_jobs < 0 ||
        (last && (compareInstants(start, last) > 0 || compareInstants(start, priorStart) < 0))) return false;
    if (!first) first = start;
    if (!last || compareInstants(finish, last) > 0) last = finish;
    priorStart = start; peak = Math.max(peak, interval.active_jobs);
    intervals.push({ start, finish, active: interval.active_jobs });
  }
  if (compareInstants(first, bounds[0]) > 0 || compareInstants(last, bounds[1]) < 0 || peak !== actual) return false;
  for (const inner of intervals) for (const outer of intervals) {
    if (compareInstants(outer.start, inner.start) <= 0 && compareInstants(outer.finish, inner.finish) >= 0 && inner.active > outer.active) return false;
  }
  if (value.source.kind === 'single-queued-item-invariant') {
    const invariant = value.invariant, running = value.running;
    const start = utcInstant(running?.started_utc), finish = utcInstant(running?.finished_utc);
    if (actual !== 1 || invariant?.observed !== true || invariant.queued_items !== 1 || invariant.max_jobs_per_item !== 1 || invariant.no_other_jobs !== true ||
        !evidenceSource(invariant.source, ['runtime-trace', 'operator-observation']) || !evidenceText(invariant.evidence, literals) || !continuousSource(invariant.source, invariant.evidence) ||
        !evidenceSource(invariant.limit_source, ['runtime-trace', 'operator-observation', 'implementation-proof']) || !evidenceText(invariant.limit_evidence, literals) ||
        running?.observed !== true || running.active_jobs !== 1 || !evidenceSource(running.source, ['runtime-trace', 'operator-observation']) ||
        !evidenceText(running.evidence, literals) || !start || !finish || compareInstants(start, finish) > 0 ||
        compareInstants(start, bounds[0]) < 0 || compareInstants(finish, bounds[1]) > 0) return false;
    if (intervals.some(interval => interval.active === 0 && compareInstants(interval.start, finish) <= 0 && compareInstants(start, interval.finish) <= 0)) return false;
  } else if (!intervals.some(interval => interval.active === actual && compareInstants(interval.start, bounds[0]) >= 0 && compareInstants(interval.finish, bounds[1]) <= 0)) return false;
  return true;
}
function checkConcurrency(value, label, flags, literals, bounds) {
  if (value?.observed !== true || !positiveInt(value?.actual) || !concurrencyEvidence(value?.evidence, value?.actual, bounds, literals)) {
    flags.push(`${label} actual concurrency or its runtime evidence unknown or unverified`);
    return null;
  }
  return value.actual;
}
async function comparisonJson(p, label, flags) {
  try { return await jsonFile(p); } catch { flags.push(`${label} absent or invalid JSON`); return null; }
}
function stable(value) {
  if (typeof value === 'number' && Object.is(value, -0)) return '-0';
  if (Array.isArray(value)) return `[${value.map(stable).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map(k => `${JSON.stringify(k)}:${stable(value[k])}`).join(',')}}`;
  return JSON.stringify(value);
}
async function jsonFile(p) { return JSON.parse(await readFile(p, 'utf8')); }
async function writeJson(p, value) { await writeFile(p, JSON.stringify(value, null, 2) + '\n', { flag: 'wx', mode: 0o600 }); }
async function regular(p) {
  const s = await lstat(p);
  if (!s.isFile() || s.isSymbolicLink()) fail(`expected regular, non-symlink file: ${p}`);
  return s;
}
function sameStat(a, b) { return a.dev === b.dev && a.ino === b.ino && a.size === b.size && a.mtimeMs === b.mtimeMs && a.ctimeMs === b.ctimeMs; }
async function capturedStdout(root, receipt, label, flags) {
  const file = path.join(root, 'captures', receipt.label, 'stdout.json');
  try {
    const before = await regular(file), bytes = await readFile(file), after = await regular(file);
    const identity = { bytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') };
    if (!sameStat(before, after) || receipt.stdout?.artifact !== 'stdout.json' || stable(identity) !== stable(receipt.stdout?.identity)) flags.push(`${label} stdout is unbound or changed`);
    return JSON.parse(bytes.toString('utf8'));
  } catch {
    flags.push(`${label} stdout absent or invalid JSON`);
    return null;
  }
}
async function digest(p) {
  const before = await regular(p);
  const f = await open(p, 'r');
  const hash = createHash('sha256');
  let bytes = 0;
  try {
    const buffer = Buffer.alloc(65536);
    for (;;) {
      const r = await f.read(buffer, 0, buffer.length, null);
      if (!r.bytesRead) break;
      hash.update(buffer.subarray(0, r.bytesRead)); bytes += r.bytesRead;
    }
  } finally { await f.close(); }
  const after = await regular(p);
  if (!sameStat(before, after) || bytes !== before.size) fail(`file changed while being read: ${p}`);
  return { bytes, sha256: hash.digest('hex') };
}
async function inputDigest(root, manifest) {
  const actual = await digest(path.join(root, manifest.input.path));
  if (stable(actual) !== stable(manifest.input.identity)) fail('generated input changed after initialization');
  return actual;
}
async function caseInfo(o) {
  const root = await realpath(required(o, 'case'));
  const manifest = await jsonFile(path.join(root, 'manifest.json'));
  if (manifest.schema !== 'parity-matrix-case-v1' || !['input/fixture.wav', 'input/fixture.aiff', 'input/fixture.flac', 'input/fixture.m4a', 'input/fixture.mp3', 'input/fixture.ogg'].includes(manifest.input?.path)) fail('unrecognized case manifest');
  for (const d of ['input', 'output', 'captures', 'parked', 'reports', 'state']) {
    const s = await lstat(path.join(root, d));
    if (!s.isDirectory() || s.isSymbolicLink()) fail(`case subdirectory is missing or a symlink: ${d}`);
  }
  return { root, manifest };
}
async function writeAll(f, b) {
  let n = 0;
  while (n < b.length) { const r = await f.write(b, n, b.length - n, null); if (!r.bytesWritten) fail('short file write'); n += r.bytesWritten; }
}
// Integer-only, periodic sine approximation (Bhaskara), with a 32-bit phase accumulator.
function sine(phase) {
  const half = 32768;
  const x = Math.floor(phase / 65536) % half;
  const p = x * (half - x);
  const v = Math.floor((16 * p * 32767) / (5 * half * half - 4 * p));
  return phase >= 2147483648 ? -v : v;
}
async function makeWav(p, rate, channels, bits, frames, profile, hzMilli) {
  const block = channels * (bits / 8), dataSize = frames * block;
  const h = Buffer.alloc(44);
  h.write('RIFF', 0); h.writeUInt32LE(36 + dataSize, 4); h.write('WAVEfmt ', 8);
  h.writeUInt32LE(16, 16); h.writeUInt16LE(1, 20); h.writeUInt16LE(channels, 22);
  h.writeUInt32LE(rate, 24); h.writeUInt32LE(rate * block, 28); h.writeUInt16LE(block, 32);
  h.writeUInt16LE(bits, 34); h.write('data', 36); h.writeUInt32LE(dataSize, 40);
  const f = await open(p, 'wx', 0o600);
  const step = Math.round(hzMilli * 4294967296 / (1000 * rate));
  let phase = 0, noise = 0x9e3779b9, clipped = 0;
  try {
    await writeAll(f, h);
    for (let start = 0; start < frames; start += 4096) {
      const count = Math.min(4096, frames - start), b = Buffer.alloc(count * block);
      for (let j = 0; j < count; j++) {
        const frame = start + j;
        noise ^= noise << 13; noise ^= noise >>> 17; noise ^= noise << 5; noise >>>= 0;
        for (let c = 0; c < channels; c++) {
          const ph = (phase + c * 211812352) % 4294967296;
          let v = 0;
          if (profile === 'tone') v = Math.trunc(sine(ph) * 0.7);
          else if (profile === 'complex') {
            const section = frame / frames;
            const gain = section < 0.03125 || section >= 0.96875 ? 0 : section < 0.25 ? 350 : section < 0.5 ? 1500 : section < 0.75 ? 100 + Math.floor(700 * (section - 0.5) / 0.25) : (frame % Math.max(1, Math.floor(rate / 4)) < rate / 100 ? 1300 : 180);
            const harmonic = sine((ph * 3) % 4294967296);
            const jitter = ((noise >>> 20) - 2048) * (c === 0 ? 1 : -1);
            v = Math.trunc((sine(ph) + Math.trunc(harmonic * 17 / 100) + jitter) * gain / 1000);
          }
          if (v < -32768 || v > 32767) clipped++;
          v = Math.max(-32768, Math.min(32767, v));
          const at = j * block + c * bits / 8;
          if (bits === 16) b.writeInt16LE(v, at);
          else {
            const shift = bits - 16;
            // Exercise the lower PCM bits too; keep true silence silent.
            const extra = v === 0 ? 0 : ((noise ^ (c ? 0x9e3779b9 : 0)) & ((1 << shift) - 1));
            const sample = v * 2 ** shift + extra;
            if (bits === 24) b.writeIntLE(sample, at, 3);
            else b.writeInt32LE(sample, at);
          }
        }
        phase = (phase + step) % 4294967296;
      }
      await writeAll(f, b);
    }
  } finally { await f.close(); }
  return { clipped_channel_samples: clipped, phase_step: step };
}
const ENCODINGS = {
  aiff: { extension: 'aiff', codec: bits => [`-c:a`, `pcm_s${bits}be`, '-f', 'aiff'] },
  flac: { extension: 'flac', codec: () => ['-c:a', 'flac', '-compression_level', '5', '-f', 'flac'] },
  alac: { extension: 'm4a', codec: () => ['-c:a', 'alac', '-f', 'ipod'] },
  aac: { extension: 'm4a', codec: () => ['-c:a', 'aac', '-b:a', '256k', '-f', 'ipod'] },
  'mp3-cbr': { extension: 'mp3', codec: () => ['-c:a', 'libmp3lame', '-b:a', '320k', '-f', 'mp3'] },
  'mp3-vbr': { extension: 'mp3', codec: () => ['-c:a', 'libmp3lame', '-q:a', '5', '-f', 'mp3'] },
  'mp3-no-xing': { extension: 'mp3', codec: () => ['-c:a', 'libmp3lame', '-b:a', '320k', '-write_xing', '0', '-f', 'mp3'] },
  ogg: { extension: 'ogg', codec: () => ['-c:a', 'vorbis', '-strict', 'experimental', '-q:a', '4', '-f', 'ogg'] },
};
async function init(o) {
  const target = path.resolve(required(o, 'case'));
  const rate = intArg(o, 'rate', 8000, 192000), channels = intArg(o, 'channels', 1, 2);
  const bits = intArg(o, 'bits', 16, 32), frames = intArg(o, 'frames', 1, rate * 60);
  if (![16, 24, 32].includes(bits)) fail('--bits must be 16, 24, or 32');
  const profile = required(o, 'profile'), hzMilli = intArg(o, 'hz-milli', 1, 2000000), format = required(o, 'input-format');
  if (!['complex', 'tone', 'silence'].includes(profile)) fail('invalid --profile');
  if (hzMilli >= rate * 500) fail('frequency must be below Nyquist');
  if (format !== 'wav' && !ENCODINGS[format]) fail('invalid --input-format');
  if (format === 'wav' && o.ffmpeg !== undefined) fail('--ffmpeg is only used for encoded inputs');
  if (format !== 'wav') required(o, 'ffmpeg');
  await mkdir(target, { mode: 0o700 });
  for (const d of ['input', 'output', 'captures', 'parked', 'reports', 'state']) await mkdir(path.join(target, d), { mode: 0o700 });
  const root = await realpath(target), base = path.join(root, 'input/fixture.wav');
  const signal = await makeWav(base, rate, channels, bits, frames, profile, hzMilli);
  let input = 'input/fixture.wav', encoder = null;
  if (format !== 'wav') {
    const executable = await realpath(o.ffmpeg), encoding = ENCODINGS[format];
    await regular(executable);
    const version = spawnSync(executable, ['-version'], { encoding: 'utf8', maxBuffer: 1024 * 1024 });
    if (version.error || version.status !== 0) fail(`FFmpeg version failed: ${version.error?.message || version.stderr}`);
    input = `input/fixture.${encoding.extension}`;
    const args = ['-nostdin', '-hide_banner', '-loglevel', 'error', '-n', '-i', base, '-map_metadata', '-1', ...encoding.codec(bits), path.join(root, input)];
    const result = spawnSync(executable, args, { encoding: 'utf8', maxBuffer: 1024 * 1024 });
    encoder = { executable, identity: await digest(executable), version: version.stdout.split('\n')[0], args, status: result.status, error: result.error?.message ?? null };
    await writeFile(path.join(root, 'input/encoder.stderr'), result.stderr ?? '', { flag: 'wx', mode: 0o600 });
    if (result.error || result.status !== 0) { await writeJson(path.join(root, 'input/encoder-failure.json'), encoder); fail('encoding failed; case and diagnostics retained'); }
  }
  const manifest = {
    schema: 'parity-matrix-case-v1', generated_audio_only: true,
    generator: { path: 'tools/parity-matrix.mjs', identity: await digest(fileURLToPath(import.meta.url)), rate, channels, bits, frames, profile, hz_milli: hzMilli, ...signal },
    base_wav: { path: 'input/fixture.wav', identity: await digest(base) },
    input: { path: input, format, identity: await digest(path.join(root, input)), encoder },
    note: 'Input encoder parameters specify fixture creation only, not reference-app output parameters. Pins, not regeneration assumptions, identify the input.'
  };
  await writeJson(path.join(root, 'manifest.json'), manifest);
  console.log(JSON.stringify({ case: root, input: path.join(root, input), input_identity: manifest.input.identity, output_directory: path.join(root, 'output'), state_directory: path.join(root, 'state') }, null, 2));
}
async function preflight(o) {
  const { root, manifest } = await caseInfo(o);
  const identity = await inputDigest(root, manifest), entries = await readdir(path.join(root, 'output'));
  console.log(JSON.stringify({ input: path.join(root, manifest.input.path), identity, output_directory: path.join(root, 'output'), output_empty: entries.length === 0, output_entries: entries }, null, 2));
  if (entries.length) fail('output directory is not empty; preserve and park its prior capture before processing');
}
function validateObservation(obs) {
  if (obs?.completed !== true || obs.generated_input_used !== true || obs.replacement_disabled !== true) fail('observation must attest completed generated-input, non-replacement processing');
  if (!nonempty(obs.reference?.build) || !/^[a-fA-F0-9]{64}$/.test(obs.reference?.sha256 ?? '') || !nonempty(obs.reference?.architecture)) fail('observation requires reference build, SHA-256 and architecture');
  for (const k of ['macos_build', 'locale', 'timezone']) if (!nonempty(obs.environment?.[k])) fail(`observation requires environment.${k}`);
  for (const k of ['template_id', 'template_state', 'output_choice', 'mp3_mode', 'other']) if (!nonempty(obs.settings?.[k])) fail(`observation requires settings.${k}`);
  for (const k of ['started_utc', 'finished_utc']) if (!nonempty(obs.timing?.[k])) fail(`observation requires timing.${k} (or explicitly unknown)`);
  const jobs = obs.settings?.jobs;
  if (!(jobs === null || (Number.isSafeInteger(jobs) && jobs >= 1)) || !nonempty(obs.process_context) || !nonempty(obs.notes)) fail('observation requires jobs (positive integer or null), process_context and notes');
}
async function snapshot(root, manifest, folder, sourceName) {
  const source = path.join(root, 'output', basename(sourceName)), dest = path.join(folder, 'output.bin');
  const before = await regular(source);
  for (const p of [manifest.input.path, manifest.base_wav.path]) {
    const s = await regular(path.join(root, p));
    if (before.dev === s.dev && before.ino === s.ino) fail('output is the generated input, not a processed export');
  }
  const sourceIdentity = await digest(source);
  await copyFile(source, dest, constants.COPYFILE_EXCL);
  const copyIdentity = await digest(dest), after = await regular(source);
  if (!sameStat(before, after) || stable(sourceIdentity) !== stable(copyIdentity)) fail('export changed while captured; copy retained without a valid receipt');
  await chmod(dest, 0o400);
  return { source_name: sourceName, identity: copyIdentity, source_stat: { device: before.dev, inode: before.ino, bytes: before.size, mtime_ms: before.mtimeMs, ctime_ms: before.ctimeMs }, artifact: 'output.bin' };
}
async function captureOriginal(o) {
  const { root, manifest } = await caseInfo(o), label = id(required(o, 'label'));
  const observationText = await readFile(required(o, 'observation'), 'utf8'), obs = JSON.parse(observationText); validateObservation(obs);
  const before = await inputDigest(root, manifest), folder = path.join(root, 'captures', label);
  await mkdir(folder, { mode: 0o700 });
  const output = await snapshot(root, manifest, folder, required(o, 'file'));
  const after = await inputDigest(root, manifest);
  // Preserve the exact validated observation JSON: JSON.stringify would collapse
  // negative zero, which can be significant in effective float settings.
  const header = JSON.stringify({ schema: 'parity-matrix-capture-v1', label, role: 'original', source_authority: 'operator-attested-application-export', recorded_at_utc: new Date().toISOString(), input: before, input_after: after, output }, null, 2);
  await writeFile(path.join(folder, 'receipt.json'), `${header.slice(0, -2)},\n  \"observation\": ${observationText}\n}\n`, { flag: 'wx', mode: 0o600 });
  console.log(JSON.stringify({ label, output, original_source_retained: true, next: `park --case ${root} --label ${label}` }, null, 2));
}
async function loadReceipt(root, label) {
  id(label);
  const folder = path.join(root, 'captures', label);
  const receipt = await jsonFile(path.join(folder, 'receipt.json'));
  if (receipt.schema !== 'parity-matrix-capture-v1' || receipt.label !== label || receipt.output?.artifact !== 'output.bin') fail(`invalid or unsuccessful capture: ${label}`);
  const actual = await digest(path.join(folder, 'output.bin'));
  if (stable(actual) !== stable(receipt.output.identity)) fail(`captured bytes changed: ${label}`);
  return receipt;
}
async function park(o) {
  const { root, manifest } = await caseInfo(o), label = id(required(o, 'label'));
  const receipt = await loadReceipt(root, label);
  await inputDigest(root, manifest);
  const source = path.join(root, 'output', basename(receipt.output.source_name)), before = await regular(source);
  if (stable(await digest(source)) !== stable(receipt.output.identity)) fail('output no longer equals the preserved capture; refusing to move it');
  const folder = path.join(root, 'parked', label);
  await mkdir(folder, { mode: 0o700 });
  const destination = path.join(folder, basename(receipt.output.source_name));
  // The destination link is create-only. This private directory has no concurrent
  // writers; preserve both names if identity or content changes during parking.
  if (!sameStat(before, await regular(source))) fail('output changed before parking');
  await link(source, destination);
  const linked = await regular(destination), current = await regular(source);
  if (linked.dev !== before.dev || linked.ino !== before.ino || current.dev !== before.dev || current.ino !== before.ino || current.size !== before.size || current.mtimeMs !== before.mtimeMs || stable(await digest(destination)) !== stable(receipt.output.identity)) fail('output changed during parking; both names retained');
  const checked = await regular(source);
  if (!sameStat(current, checked)) fail('output changed before removal of the old name; both names retained');
  await unlink(source);
  console.log(JSON.stringify({ label, retained_at: destination, output_directory_empty: (await readdir(path.join(root, 'output'))).length === 0 }, null, 2));
}
function runtimeEnvironment() {
  const version = process.platform === 'darwin' ? spawnSync('/usr/bin/sw_vers', ['-buildVersion'], { encoding: 'utf8' }) : null;
  const international = Intl.DateTimeFormat().resolvedOptions();
  return {
    platform: process.platform, architecture: process.arch, kernel_release: release(),
    macos_build: version?.status === 0 ? version.stdout.trim() : null,
    node_version: process.version, locale: international.locale, timezone: international.timeZone,
    environment: Object.fromEntries(['LANG', 'LC_ALL', 'LC_CTYPE', 'TZ'].map(key => [key, process.env[key] ?? null]))
  };
}
async function libraryObservations(value) {
  const configured = value.details?.effective_config?.libraries;
  if (!configured || typeof configured !== 'object') return { error: 'CLI did not report effective libraries' };
  const observed = {};
  for (const key of ['tag_bridge', 'flac', 'lame']) {
    const supplied = configured[key];
    if (supplied === null || supplied === undefined) { observed[key] = null; continue; }
    try {
      const resolved = await realpath(supplied);
      observed[key] = { path: supplied, resolved, identity: await digest(resolved), observation: 'after candidate process; not proof of loaded-image identity' };
    } catch (e) { observed[key] = { path: supplied, error: e.message }; }
  }
  return observed;
}
async function run(o) {
  const { root, manifest } = await caseInfo(o), label = id(required(o, 'label'));
  const executable = await realpath(required(o, 'executable')), template = required(o, 'template-id'), format = required(o, 'format'), mp3 = required(o, 'mp3-mode');
  const jobs = o.jobs === undefined ? 1 : intArg(o, 'jobs', 1, 65535);
  if (!['wav', 'aiff', 'alac', 'flac', 'mp3', 'aac', 'match-input'].includes(format) || !['cbr320', 'vbr192'].includes(mp3)) fail('invalid output format or MP3 mode');
  await regular(executable);
  if ((await readdir(path.join(root, 'output'))).length) fail('output directory is not empty');
  const input = await inputDigest(root, manifest), binary = await digest(executable), folder = path.join(root, 'captures', label);
  await mkdir(folder, { mode: 0o700 });
  const args = ['--state-dir', path.join(root, 'state'), '--json', 'process', path.join(root, manifest.input.path), '--template-id', template, '--format', format, '--output-dir', path.join(root, 'output'), '--no-mirror', '--jobs', String(jobs), '--mp3-mode', mp3];
  // Diagnostic clock control for AAC/ALAC: the native export's recorded MP4 header second.
  if (o['controlled-mp4-time-1904'] !== undefined) args.push('--controlled-mp4-time-1904', String(intArg(o, 'controlled-mp4-time-1904', 0, 0xffffffff)));
  const startedAt = new Date().toISOString();
  const result = spawnSync(executable, args, { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
  const finishedAt = new Date().toISOString();
  const stdout = result.stdout ?? '', stdoutIdentity = { bytes: Buffer.byteLength(stdout), sha256: createHash('sha256').update(stdout).digest('hex') };
  await writeFile(path.join(folder, 'stdout.json'), stdout, { flag: 'wx', mode: 0o600 });
  await writeFile(path.join(folder, 'stderr.txt'), result.stderr ?? '', { flag: 'wx', mode: 0o600 });
  const receipt = { schema: 'parity-matrix-capture-v1', label, role: 'rust', source_authority: 'operator-selected-independent-cli', process_id: result.pid ?? null, started_at_utc: startedAt, finished_at_utc: finishedAt, runtime: runtimeEnvironment(), input, executable: { path: executable, identity: binary }, args, exit_status: result.status, signal: result.signal, error: result.error?.message ?? null, stdout: { artifact: 'stdout.json', identity: stdoutIdentity }, output: null };
  try {
    if (stable(await digest(path.join(folder, 'stdout.json'))) !== stable(stdoutIdentity)) fail('candidate stdout changed while captured');
    receipt.input_after = await inputDigest(root, manifest);
    receipt.executable_after = await digest(executable);
    if (stable(receipt.executable_after) !== stable(binary)) fail('candidate executable changed while running');
    if (result.error || result.status !== 0) fail('candidate command failed; stdout, stderr and any output retained');
    const value = JSON.parse(result.stdout);
    receipt.libraries_after = await libraryObservations(value);
    if (value.status !== 'ok' || !Array.isArray(value.results) || value.results.length !== 1 || value.results[0].status !== 'ok') fail('candidate did not report one successful input');
    const actualOutput = value.results[0].report?.output_path;
    if (typeof actualOutput !== 'string' || path.dirname(path.resolve(actualOutput)) !== path.join(root, 'output')) fail('candidate did not report an output directly in the planned directory');
    receipt.output = await snapshot(root, manifest, folder, path.basename(actualOutput));
  } catch (e) { receipt.capture_error = e.message; }
  await writeJson(path.join(folder, 'receipt.json'), receipt);
  console.log(JSON.stringify({ label, exit_status: result.status, output: receipt.output, capture_error: receipt.capture_error ?? null, stdout: path.join(folder, 'stdout.json'), stderr: path.join(folder, 'stderr.txt') }, null, 2));
  if (!receipt.output) fail('candidate capture unsuccessful; evidence retained');
}
async function compareBytes(left, right) {
  const ls = await regular(left), rs = await regular(right), a = await open(left, 'r'), b = await open(right, 'r');
  const ah = createHash('sha256'), bh = createHash('sha256'), ab = Buffer.alloc(65536), bb = Buffer.alloc(65536);
  let as = 0, bs = 0, first = null;
  async function block(f, buf) {
    let n = 0;
    while (n < buf.length) { const r = await f.read(buf, n, buf.length - n, null); if (!r.bytesRead) break; n += r.bytesRead; }
    return n;
  }
  try {
    for (;;) {
      const [an, bn] = await Promise.all([block(a, ab), block(b, bb)]);
      if (!an && !bn) break;
      ah.update(ab.subarray(0, an)); bh.update(bb.subarray(0, bn));
      if (first === null) {
        const common = Math.min(an, bn);
        for (let i = 0; i < common; i++) if (ab[i] !== bb[i]) { first = { offset: as + i, left_byte: ab[i], right_byte: bb[i] }; break; }
        if (first === null && an !== bn) first = { offset: Math.min(as + an, bs + bn), left_byte: common < an ? ab[common] : null, right_byte: common < bn ? bb[common] : null };
      }
      as += an; bs += bn;
    }
  } finally { await Promise.all([a.close(), b.close()]); }
  if (!sameStat(ls, await regular(left)) || !sameStat(rs, await regular(right)) || as !== ls.size || bs !== rs.size) fail('capture changed during comparison');
  const leftIdentity = { bytes: as, sha256: ah.digest('hex') }, rightIdentity = { bytes: bs, sha256: bh.digest('hex') };
  return { left: leftIdentity, right: rightIdentity, identical: stable(leftIdentity) === stable(rightIdentity), first_difference: first };
}
function bindComparison(comparison, left, right, label, flags) {
  if (stable(comparison.left) !== stable(left.output?.identity) || stable(comparison.right) !== stable(right.output?.identity)) flags.push(`${label} comparison bytes differ from capture receipts`);
}
function validSourceStat(stat, output) {
  return stat && Number.isSafeInteger(stat.device) && stat.device >= 0 && Number.isSafeInteger(stat.inode) && stat.inode >= 0 &&
    Number.isSafeInteger(stat.bytes) && stat.bytes === output?.identity?.bytes && Number.isFinite(stat.mtime_ms) && Number.isFinite(stat.ctime_ms);
}
function sameUnchangedSource(left, right) {
  const a = left.output?.source_stat, b = right.output?.source_stat;
  return validSourceStat(a, left.output) && validSourceStat(b, right.output) && a.device === b.device && a.inode === b.inode && a.bytes === b.bytes && a.mtime_ms === b.mtime_ms;
}
function checkOriginal(receipt, label, root, manifest, flags) {
  const obs = receipt.observation ?? {}, prefix = `original ${label}`;
  const literals = [obs.effective_settings?.template?.name, obs.settings?.template_id, root];
  if (receipt.source_authority !== 'operator-attested-application-export' || obs.completed !== true || obs.generated_input_used !== true || obs.replacement_disabled !== true) flags.push(`${prefix} capture attestations missing`);
  for (const [group, fields] of [['reference', ['build', 'architecture']], ['environment', ['macos_build', 'locale', 'timezone']], ['settings', ['template_id', 'template_state', 'output_choice', 'mp3_mode', 'other']]]) {
    for (const field of fields) {
      const value = obs[group]?.[field];
      const valid = group === 'settings' && field === 'template_id' ? knownIdentity(value) : group === 'settings' && field === 'template_state' && nonempty(value) && value === obs.effective_settings?.template?.name ? true : group === 'settings' && field === 'mp3_mode' && notApplicable(value) ? true : evidenceText(value, literals);
      if (!valid) flags.push(`${prefix} ${group}.${field} unknown or unverified`);
    }
    const extra = Object.fromEntries(Object.entries(obs[group] ?? {}).filter(([key]) => !fields.includes(key) && !(group === 'reference' && key === 'sha256') && !(group === 'settings' && key === 'jobs')));
    if (containsUncertainty(extra)) flags.push(`${prefix} ${group} contains unknown or unverified evidence`);
  }
  if (!/^[a-f0-9]{64}$/i.test(obs.reference?.sha256 ?? '')) flags.push(`${prefix} reference SHA-256 unknown or invalid`);
  if (!validLocale(obs.environment?.locale)) flags.push(`${prefix} locale invalid`);
  if (!validTimezone(obs.environment?.timezone)) flags.push(`${prefix} timezone invalid`);
  if (!positiveInt(obs.settings?.jobs)) flags.push(`${prefix} configured jobs unknown or invalid`);
  if (!evidenceText(obs.process_context, literals)) flags.push(`${prefix} process context unknown or unverified`);
  if (!evidenceText(obs.notes, literals)) flags.push(`${prefix} notes unknown or unverified`);
  const bounds = timingBounds(obs.timing?.started_utc, obs.timing?.finished_utc, prefix, flags), recorded = utcInstant(receipt.recorded_at_utc);
  if (!recorded || (bounds && compareInstants(recorded, bounds[1]) < 0)) flags.push(`${prefix} capture time missing or before render finished`);
  const processStart = utcInstant(obs.process_instance?.started_utc);
  if (obs.process_instance?.observed !== true || !positiveInt(obs.process_instance?.pid) || processStart === null || !evidenceText(obs.process_instance?.evidence, literals) || (bounds && processStart && compareInstants(processStart, bounds[0]) > 0)) flags.push(`${prefix} process instance unknown, unverified or inconsistent with timing`);
  const concurrency = checkConcurrency(obs.concurrency, prefix, flags, literals, bounds);
  if (concurrency !== null && positiveInt(obs.settings?.jobs) && concurrency > obs.settings.jobs) flags.push(`${prefix} actual concurrency exceeds configured jobs`);
  const settings = obs.effective_settings;
  if (settings?.observed !== true || !evidenceText(settings?.evidence, literals) || !validEffectiveTemplate(settings?.template) || settings.template?.unique_id !== obs.settings?.template_id || !validOutput(settings?.output, root) || settings?.replace_originals !== false || !(notApplicable(settings?.mp3_mode) || ['cbr320', 'vbr192'].includes(settings?.mp3_mode))) flags.push(`${prefix} concrete effective settings unknown or unverified`);
  if (outputChoiceFormat(obs.settings?.output_choice) !== settings?.output?.format) flags.push(`${prefix} output choice is ambiguous or contradicts effective format`);
  const needsMp3 = outputUsesMp3(settings?.output?.format, manifest);
  if (needsMp3 === null || (needsMp3 && !['cbr320', 'vbr192'].includes(settings?.mp3_mode))) flags.push(`${prefix} relevant MP3 mode or source format unknown`);
  if (needsMp3 && obs.settings?.mp3_mode !== settings?.mp3_mode) flags.push(`${prefix} MP3 mode attestations differ`);
  if (!validSourceStat(receipt.output?.source_stat, receipt.output)) flags.push(`${prefix} source output stat missing or invalid`);
  if (obs.render_event?.observed !== true || !knownIdentity(obs.render_event?.id) || !evidenceText(obs.render_event?.evidence)) flags.push(`${prefix} distinct render event not attested`);
  return { bounds, recorded, processStart, concurrency };
}
function objectValue(value) { return value !== null && typeof value === 'object' && !Array.isArray(value); }
function onlyKnownKeys(value, keys) { return objectValue(value) && Object.keys(value).every(key => keys.includes(key)); }
function numericFields(value, fields) { return objectValue(value) && fields.every(key => typeof value[key] === 'number' && Number.isFinite(value[key]) && Number.isFinite(Math.fround(value[key]))); }
function integerFields(value, fields) { return objectValue(value) && fields.every(key => Number.isSafeInteger(value[key])); }
function validEffectiveTemplate(value) {
  const dynamics = (item, fields) => onlyKnownKeys(item, ['enabled', ...fields, 'bands', 'crossovers_hz']) && typeof item.enabled === 'boolean' && numericFields(item, fields) &&
    Array.isArray(item.bands) && item.bands.length === 4 && item.bands.every(band => onlyKnownKeys(band, ['threshold', 'gain', 'ratio', 'attack', 'release']) && numericFields(band, ['threshold', 'gain', 'ratio', 'attack', 'release'])) &&
    Array.isArray(item.crossovers_hz) && item.crossovers_hz.length === 3 && item.crossovers_hz.every(number => typeof number === 'number' && Number.isFinite(number) && Number.isFinite(Math.fround(number)));
  return onlyKnownKeys(value, ['unique_id', 'name', 'description', 'is_builtin', 'is_default', 'analysis_volume', 'exciter_option', 'clip', 'pitch', 'expansion', 'compression', 'output']) && knownIdentity(value.unique_id) && Object.hasOwn(value, 'name') && (value.name === null || typeof value.name === 'string') && typeof value.is_builtin === 'boolean' && typeof value.is_default === 'boolean' &&
    numericFields(value, ['analysis_volume']) && integerFields(value, ['exciter_option']) &&
    onlyKnownKeys(value.clip, ['enabled', 'low', 'high', 'percentage', 'headroom']) && typeof value.clip?.enabled === 'boolean' && numericFields(value.clip, ['low', 'high', 'percentage', 'headroom']) &&
    onlyKnownKeys(value.pitch, ['enabled', 'threshold_cents', 'option']) && typeof value.pitch?.enabled === 'boolean' && integerFields(value.pitch, ['threshold_cents', 'option']) &&
    dynamics(value.expansion, ['low', 'high', 'target_volume', 'headroom']) && dynamics(value.compression, ['low', 'high', 'target_volume']) &&
    onlyKnownKeys(value.output, ['limiter_threshold', 'margin', 'character', 'limiter_release']) && numericFields(value.output, ['limiter_threshold', 'margin', 'limiter_release']) && integerFields(value.output, ['character']);
}
function processingTemplate(template) {
  if (!validEffectiveTemplate(template)) return null;
  const floats = (value, keys) => Object.fromEntries(keys.map(key => [key, Math.fround(value[key])]));
  const dynamics = (value, extra) => ({ enabled: value.enabled, ...floats(value, ['low', 'high', 'target_volume', ...extra]), bands: value.bands.map(band => floats(band, ['threshold', 'gain', 'ratio', 'attack', 'release'])), crossovers_hz: value.crossovers_hz.map(Math.fround) });
  return { analysis_volume: Math.fround(template.analysis_volume), exciter_option: template.exciter_option,
    clip: { enabled: template.clip.enabled, ...floats(template.clip, ['low', 'high', 'percentage', 'headroom']) },
    pitch: { enabled: template.pitch.enabled, threshold_cents: template.pitch.threshold_cents, option: template.pitch.option },
    expansion: dynamics(template.expansion, ['headroom']), compression: dynamics(template.compression, []),
    output: { ...floats(template.output, ['limiter_threshold', 'margin', 'limiter_release']), character: template.output.character } };
}
function comparableTemplate(template) {
  const processing = processingTemplate(template);
  return processing === null ? null : { unique_id: template.unique_id, name: template.name, description: template.description, is_builtin: template.is_builtin, is_default: template.is_default, processing };
}
function validOutput(output, root) {
  const mode = output?.mode, folder = mode?.specific_folder;
  return onlyKnownKeys(output, ['format', 'mode']) && ['wav', 'aiff', 'alac', 'flac', 'mp3', 'aac', 'match_input'].includes(output.format) &&
    objectValue(mode) && Object.keys(mode).length === 1 && objectValue(folder) && stable(Object.keys(folder).sort()) === stable(['directory', 'mirror']) &&
    folder.directory === path.join(root, 'output') && folder.mirror === false;
}
function outputChoiceFormat(value) {
  if (!nonempty(value)) return null;
  const formats = [
    ['match_input', /\b(?:match[\s_-]*input|same\s+as\s+(?:the\s+)?input)\b/i],
    ['wav', /\b(?:wav|wave)\b/i], ['aiff', /\baiff?\b/i], ['alac', /\balac\b/i],
    ['flac', /\bflac\b/i], ['mp3', /\bmp3\b/i], ['aac', /\baac\b/i]
  ];
  const matches = formats.filter(([, pattern]) => pattern.test(value));
  return matches.length === 1 ? matches[0][0] : null;
}
function outputUsesMp3(format, manifest) {
  if (format === 'mp3') return true;
  if (format !== 'match_input') return false;
  const source = manifest.input.format;
  if (!['wav', 'aiff', 'alac', 'flac', 'aac', 'mp3-cbr', 'mp3-vbr', 'mp3-no-xing', 'ogg'].includes(source)) return null;
  return source.startsWith('mp3-');
}
function validEffectiveConfig(value, template, receipt, root, manifest) {
  const args = receipt.args, selection = value?.effective_selected_template, selected = selection?.builtin ?? selection?.custom;
  const clock = Array.isArray(args) && args.length === 18 && args[16] === '--controlled-mp4-time-1904' && /^(0|[1-9]\d*)$/.test(args[17] ?? '') ? args.slice(16) : [];
  if (!Array.isArray(args) || args.length !== 16 + clock.length || !objectValue(selection) || Object.keys(selection).length !== 1 || !objectValue(selected) || !knownIdentity(selected.id)) return false;
  const kind = Object.keys(selection)[0];
  if (kind === 'builtin') {
    if (!onlyKnownKeys(selected, ['id'])) return false;
  } else if (kind === 'custom') {
    if (!onlyKnownKeys(selected, ['index', 'id']) || !Number.isSafeInteger(selected.index) || selected.index < 0) return false;
  } else return false;
  const format = args[8], mp3 = args[15], job = args[13];
  if (!['wav', 'aiff', 'alac', 'flac', 'mp3', 'aac', 'match-input'].includes(format) || !['cbr320', 'vbr192'].includes(mp3) || typeof job !== 'string' || !/^[1-9]\d*$/.test(job)) return false;
  const expected = ['--state-dir', path.join(root, 'state'), '--json', 'process', path.join(root, manifest.input.path), '--template-id', selected.id, '--format', format, '--output-dir', path.join(root, 'output'), '--no-mirror', '--jobs', job, '--mp3-mode', mp3, ...clock];
  // Candidate runs never request --overwrite; current CLIs report it explicitly as false.
  if (Object.hasOwn(value ?? {}, 'overwrite') && value.overwrite !== false) return false;
  return onlyKnownKeys(value, ['output', 'processor_max', 'mp3_mode', 'libraries', 'selected_template', 'effective_selected_template', 'overwrite']) && Object.hasOwn(value, 'selected_template') && (value.selected_template === null || stable(value.selected_template) === stable(selection)) && positiveInt(value.processor_max) && value.mp3_mode === mp3 && validOutput(value.output, root) &&
    value.output.format === (format === 'match-input' ? 'match_input' : format) && selected.id === template?.unique_id && stable(args) === stable(expected);
}
function checkCandidate(root, manifest, receipt, value, context, label, flags) {
  const prefix = `candidate ${label}`;
  const bounds = timingBounds(receipt.started_at_utc, receipt.finished_at_utc, prefix, flags);
  if (receipt.source_authority !== 'operator-selected-independent-cli' || receipt.exit_status !== 0 || receipt.signal !== null || receipt.error !== null || receipt.capture_error) flags.push(`${prefix} successful process receipt missing`);
  if (!validSourceStat(receipt.output?.source_stat, receipt.output)) flags.push(`${prefix} source output stat missing or invalid`);
  const literals = [value?.details?.effective_template?.name, receipt.executable?.path, root];
  if (!positiveInt(receipt.process_id) || context?.pid !== receipt.process_id || !evidenceText(context?.process_context, literals)) flags.push(`${prefix} process instance or context unknown or unverified`);
  if (!validIdentity(receipt.executable?.identity) || stable(receipt.executable?.identity) !== stable(receipt.executable_after) || !Array.isArray(receipt.args) || !receipt.args.length) flags.push(`${prefix} executable or command unknown or changed`);
  const runtime = receipt.runtime ?? {};
  for (const field of ['platform', 'architecture', 'kernel_release', 'node_version']) if (!knownText(runtime[field])) flags.push(`${prefix} runtime.${field} unknown or unverified`);
  if (runtime.platform === 'darwin' && !knownText(runtime.macos_build)) flags.push(`${prefix} macOS build unknown or unverified`);
  if (!validLocale(runtime.locale) || !validTimezone(runtime.timezone)) flags.push(`${prefix} runtime locale or timezone unknown or invalid`);
  if (!runtime.environment || typeof runtime.environment !== 'object' || Array.isArray(runtime.environment) || !['LANG', 'LC_ALL', 'LC_CTYPE', 'TZ'].every(key => Object.hasOwn(runtime.environment, key)) || containsUncertainty(runtime.environment, true)) flags.push(`${prefix} inherited environment unknown or unverified`);
  if (!validLocale(context?.locale) || !validTimezone(context?.timezone) || !evidenceText(context?.environment_evidence, literals)) flags.push(`${prefix} effective locale/timezone or evidence unknown or unverified`);
  else if (canonicalLocale(context.locale) !== canonicalLocale(runtime.locale) || context.timezone !== runtime.timezone) flags.push(`${prefix} effective locale/timezone differs from recorded runtime`);
  if (value?.command !== 'process' || value.status !== 'ok' || !Array.isArray(value.results) || value.results.length !== 1 || value.results[0]?.status !== 'ok') flags.push(`${prefix} successful CLI observation missing`);
  const result = value?.results?.[0], expectedInput = path.join(root, manifest.input.path), expectedOutput = path.join(root, 'output', validSourceName(receipt.output?.source_name) ? receipt.output.source_name : '');
  if (result?.input !== expectedInput || result?.report?.input_path !== expectedInput || result?.report?.output_path !== expectedOutput) flags.push(`${prefix} reported input or output path differs from the capture`);
  const applied = result?.report?.applied_stages, template = value?.details?.effective_template;
  if (!Array.isArray(applied) || !applied.every(knownIdentity) || new Set(applied).size !== applied.length) flags.push(`${prefix} applied-stage observation missing or invalid`);
  else {
    if (!applied.includes('limiter')) flags.push(`${prefix} successful report omits the mandatory limiter`);
    if (Number.isSafeInteger(template?.exciter_option) && template.exciter_option >= 0 && !applied.includes('warmth')) flags.push(`${prefix} successful report omits required warmth`);
    for (const name of applied) if (stageEnabled(template, name) === false) flags.push(`${prefix} applied stage ${name} contradicts its effective template`);
  }
  if (!validEffectiveTemplate(value?.details?.effective_template)) flags.push(`${prefix} effective template is incomplete or invalid`);
  if (!validEffectiveConfig(value?.details?.effective_config, value?.details?.effective_template, receipt, root, manifest)) flags.push(`${prefix} effective configuration is incomplete or invalid`);
  const jobs = value?.details?.jobs, requested = value?.details?.requested_jobs;
  const jobIndex = Array.isArray(receipt.args) ? receipt.args.indexOf('--jobs') : -1;
  const arg = jobIndex < 0 ? null : receipt.args[jobIndex + 1];
  if (!positiveInt(jobs) || !positiveInt(requested) || jobs > requested || jobs > value?.details?.effective_config?.processor_max || typeof arg !== 'string' || !/^[1-9]\d*$/.test(arg) || Number(arg) !== requested) flags.push(`${prefix} effective or requested jobs unknown or inconsistent`);
  const concurrency = checkConcurrency(context?.concurrency, prefix, flags, literals, bounds);
  if (concurrency !== null && positiveInt(jobs) && concurrency > jobs) flags.push(`${prefix} actual concurrency exceeds effective jobs`);
  const libs = value?.details?.effective_config?.libraries, after = receipt.libraries_after;
  if (!onlyKnownKeys(libs, ['tag_bridge', 'flac', 'flac_sha256', 'lame', 'lame_sha256']) || !['tag_bridge', 'flac', 'flac_sha256', 'lame', 'lame_sha256'].every(key => Object.hasOwn(libs, key)) || !after || typeof after !== 'object' || after.error) flags.push(`${prefix} libraries unknown`);
  else {
    for (const key of ['tag_bridge', 'flac', 'lame']) {
      const supplied = libs[key], observed = after[key];
      if (key !== 'tag_bridge') {
        const pin = libs[`${key}_sha256`];
        if (pin !== null && (typeof pin !== 'string' || !/^[a-f0-9]{64}$/i.test(pin) || supplied === null || !validIdentity(observed?.identity) || pin.toLowerCase() !== observed.identity.sha256.toLowerCase())) flags.push(`${prefix} ${key} library pin unknown or inconsistent`);
      }
      if (supplied === null && observed === null) continue;
      if (!knownIdentity(supplied) || !observed || observed.path !== supplied || !knownIdentity(observed.resolved) || !validIdentity(observed.identity) || observed.error) flags.push(`${prefix} ${key} library unknown or unverified`);
    }
    if (!evidenceText(context?.loaded_libraries_evidence, literals)) flags.push(`${prefix} loaded library identity not independently attested`);
  }
  return { jobs, concurrency, bounds };
}
function stageEnabled(template, name) {
  if (name === 'clip_repair') return template?.clip?.enabled;
  if (name === 'pitch') return template?.pitch?.enabled;
  if (name === 'expansion') return template?.expansion?.enabled;
  if (name === 'compression') return template?.compression?.enabled;
  if (name === 'warmth' && typeof template?.exciter_option === 'number') return template.exciter_option >= 0;
  return undefined;
}
function independentStageReference(reference, root, label) {
  if (!knownIdentity(reference)) return false;
  let name = reference.trim();
  if (name.startsWith('file:')) { try { name = fileURLToPath(name); } catch { return false; } }
  const resolved = path.resolve(root, name), state = path.join(root, 'state');
  if (resolved === state || resolved.startsWith(`${state}${path.sep}`)) return false;
  if (!name.includes(path.sep) && /\b(?:stdout(?:\.json)?|applied_stages)\b/i.test(name)) return false;
  if (!name.includes(path.sep) && /\b(?:template|settings|configuration)\b/i.test(name) && !/\b(?:trace|marker|log|probe)\b/i.test(name)) return false;
  const compact = path.basename(name).toLowerCase().replace(/[^a-z0-9]/g, '');
  const labelKey = label.toLowerCase().replace(/[^a-z0-9]/g, '');
  const prohibited = new Set(['stdout', 'stdoutjson', 'clistdout', 'candidatestdout', 'candidatestdoutjson', `${labelKey}stdout`, `${labelKey}stdoutjson`, `candidate${labelKey}stdout`, `candidate${labelKey}stdoutjson`, 'template', 'templatestate', 'templatesettings', 'templatejson', 'templatexml', 'templatesxml', 'effectivetemplate', 'effectivetemplatejson', 'candidateeffectivetemplate', 'config', 'configjson', 'configuration', 'effectiveconfig', 'effectiveconfigjson', 'candidateeffectiveconfig', 'settings', 'settingsjson']);
  const configurationLabel = /^(?:candidate|original)?(?:effective|selected)?(?:template|config|settings)(?:json|xml|id|uuid|state|values|settings|libraries)?$/i.test(compact);
  return !prohibited.has(compact) && !configurationLabel && !/\.pntemplate$/i.test(name);
}
function stageObservation(value, states, literals, root, receipt) {
  if (value?.observed !== true || value.independent !== true || !states.includes(value.state) ||
      !evidenceSource(value.source, ['runtime-trace', 'processing-marker', 'operator-observation']) || !independentStageReference(value.source.reference, root, receipt.label) || !evidenceText(value.evidence, literals)) return false;
  const configuredOnly = /\b(?:template|settings?|configuration|configured|stdout|applied_stages|cli)\b/i.test(value.evidence);
  const independentSource = /\b(?:trace|tracing|marker|measurement|measured|probe|instrumentation|processing\s+log)\b/i.test(value.evidence);
  return !configuredOnly || independentSource;
}
function checkStageReview(review, candidate, originalA, originalB, root, receipt, flags) {
  const evidence = review?.stage_evidence, applied = candidate?.results?.[0]?.report?.applied_stages;
  const validApplied = Array.isArray(applied) && applied.every(knownIdentity) && new Set(applied).size === applied.length;
  if (!validApplied) flags.push('candidate applied-stage observation missing or invalid');
  const subjects = ['original_a', 'original_b', 'candidate'];
  const templates = { original_a: originalA.observation?.effective_settings?.template, original_b: originalB.observation?.effective_settings?.template, candidate: candidate?.details?.effective_template };
  // Candidate-wide contradictions are checked for both full and repeat comparisons.
  if (review?.target_stages_observed === true && evidence?.mode === 'observed') {
    const stages = evidence.stages;
    if (!Array.isArray(stages) || !stages.length) { flags.push('target stage observations absent'); return; }
    const names = new Set(); let active = false;
    for (const stage of stages) {
      const valid = knownIdentity(stage?.name) && !names.has(stage.name) && subjects.every(key => stageObservation(stage?.[key], ['active', 'inactive'], [stage.name, candidate?.details?.effective_template?.name], root, receipt));
      if (!valid) { flags.push('target stage evidence unknown, duplicate or incomplete for a capture'); continue; }
      names.add(stage.name);
      const state = stage.original_a.state;
      if (stage.original_b.state !== state || stage.candidate.state !== state) flags.push(`target stage ${stage.name} states differ`);
      for (const subject of subjects) if (stage[subject].state === 'active' && stageEnabled(templates[subject], stage.name) === false) flags.push(`${subject} active stage ${stage.name} contradicts its effective template`);
      if (state === 'active') active = true;
      if (validApplied && applied.includes(stage.name) !== (stage.candidate.state === 'active')) flags.push(`target stage ${stage.name} contradicts candidate applied stages`);
    }
    if (!active) flags.push('no target stage observed active');
  } else if (review?.target_stages_observed === false && evidence?.mode === 'not-applicable') {
    if (!evidenceText(evidence.reason, [candidate?.details?.effective_template?.name]) || !subjects.every(key => stageObservation(evidence?.[key], ['not-applicable'], [candidate?.details?.effective_template?.name], root, receipt))) flags.push('not-applicable stage evidence incomplete for a capture');
    if (validApplied && applied.some(name => name !== 'limiter')) flags.push('not-applicable stage review contradicts candidate applied stages');
  } else flags.push('target stage activation unconfirmed or review absent');
}
async function checkStageSourceBindings(root, receipt, review, flags) {
  const evidence = review?.stage_evidence, subjects = ['original_a', 'original_b', 'candidate'];
  const observations = evidence?.mode === 'observed' && Array.isArray(evidence.stages)
    ? evidence.stages.flatMap(stage => subjects.map(key => stage?.[key]))
    : subjects.map(key => evidence?.[key]);
  const references = [...new Set(observations.map(observation => observation?.source?.reference).filter(nonempty))]
    .filter(reference => path.isAbsolute(reference) || reference.includes('/') || reference.startsWith('file:'));
  if (!references.length) return;
  const settingsFile = path.join(root, 'state', 'settings.json');
  let settingsStat = null, settingsIdentity = null;
  try { settingsStat = await regular(settingsFile); } catch { /* State file is optional. */ }
  for (const reference of references) {
    try {
      const file = reference.startsWith('file:') ? fileURLToPath(reference) : path.resolve(root, reference);
      const resolved = await realpath(file), state = path.join(root, 'state');
      if (resolved === state || resolved.startsWith(`${state}${path.sep}`)) {
        flags.push('stage evidence source is bound to candidate template settings'); continue;
      }
      const stat = await regular(resolved), stdout = receipt.stdout?.identity;
      const matchesStdoutSize = validIdentity(stdout) && stat.size === stdout.bytes;
      const matchesSettingsSize = settingsStat && stat.size === settingsStat.size;
      if (matchesStdoutSize || matchesSettingsSize) {
        const identity = await digest(resolved);
        if (matchesSettingsSize && !settingsIdentity) settingsIdentity = await digest(settingsFile);
        if ((matchesStdoutSize && stable(identity) === stable(stdout)) || (matchesSettingsSize && stable(identity) === stable(settingsIdentity))) flags.push('stage evidence source is bound to candidate stdout or template settings');
      }
    } catch (error) {
      if (error.code !== 'ENOENT' && error.code !== 'ENOTDIR') flags.push('stage evidence source binding could not be checked');
    }
  }
}
function comparableReference(value) {
  return { ...value, sha256: typeof value?.sha256 === 'string' ? value.sha256.toLowerCase() : value?.sha256 };
}
function comparableEnvironment(value) { return { ...value, locale: canonicalLocale(value?.locale) }; }
function checkTemplateMapping(originalId, candidateId, review, flags) {
  const mapping = review?.template_identity;
  if (!knownIdentity(originalId) || !knownIdentity(candidateId)) { flags.push('template identity unknown'); return; }
  if (originalId === candidateId && mapping === undefined) return;
  if (mapping?.kind !== 'native-export-import' || mapping.observed !== true || mapping.source_id !== originalId ||
      !knownIdentity(mapping.export_id) || mapping.export_id === originalId || mapping.imported_id !== mapping.export_id ||
      mapping.imported_id !== candidateId || !evidenceText(mapping.export_evidence) || !evidenceText(mapping.import_evidence)) flags.push('native/export/import template identity mapping missing or unverified');
}
function compareOriginalSettings(a, b, flags) {
  const left = a.observation?.effective_settings, right = b.observation?.effective_settings;
  if (stable(processingTemplate(left?.template)) !== stable(processingTemplate(right?.template)) ||
      left?.template?.unique_id !== right?.template?.unique_id || left?.template?.name !== right?.template?.name ||
      stable(left?.output) !== stable(right?.output) || left?.replace_originals !== right?.replace_originals || left?.mp3_mode !== right?.mp3_mode) flags.push('original concrete effective settings differ');
}
function compareCandidateSettings(original, candidate, manifest, flags) {
  const reference = original.observation?.effective_settings, actual = candidate?.details;
  if (stable(processingTemplate(reference?.template)) !== stable(processingTemplate(actual?.effective_template)) ||
      reference?.template?.name !== actual?.effective_template?.name) flags.push('candidate effective processing settings differ from original');
  if (stable(reference?.output) !== stable(actual?.effective_config?.output) || reference?.replace_originals !== false) flags.push('candidate effective output settings differ from original');
  const needsMp3 = outputUsesMp3(reference?.output?.format, manifest);
  if (needsMp3 === null) flags.push('match-input source format unknown');
  else if (needsMp3 && reference?.mp3_mode !== actual?.effective_config?.mp3_mode) flags.push('candidate effective MP3 mode differs from original');
}
async function compareCandidates(o) {
  const { root, manifest } = await caseInfo(o), aLabel = id(required(o, 'a')), bLabel = id(required(o, 'b')), reportName = id(required(o, 'report'));
  if (aLabel === bLabel) fail('candidate A and B must be distinct captures');
  const a = await loadReceipt(root, aLabel), b = await loadReceipt(root, bLabel);
  if (a.role !== 'rust' || b.role !== 'rust') fail('both captures must be independent CLI runs');
  const current = await inputDigest(root, manifest), flags = [];
  if ([a, b].some(x => stable(x.input) !== stable(current) || stable(x.input_after) !== stable(current))) flags.push('input identity differs');
  if (!validSourceName(a.output.source_name) || !validSourceName(b.output.source_name) || a.output.source_name !== b.output.source_name) flags.push('output path unknown or differs');
  if (stable(a.executable?.identity) !== stable(b.executable?.identity)) flags.push('candidate executable differs');
  if (stable(a.args) !== stable(b.args)) flags.push('candidate arguments differ');
  if (stable(comparableEnvironment(a.runtime)) !== stable(comparableEnvironment(b.runtime))) flags.push('candidate runtime environment differs');
  if (stable(a.libraries_after) !== stable(b.libraries_after)) flags.push('candidate libraries differ');
  const [av, bv] = await Promise.all([capturedStdout(root, a, 'candidate A', flags), capturedStdout(root, b, 'candidate B', flags)]);
  const review = o.review === undefined ? null : await comparisonJson(o.review, 'candidate review', flags);
  if (!review) flags.push('candidate context review absent');
  const ac = checkCandidate(root, manifest, a, av, review?.candidate_a, 'A', flags), bc = checkCandidate(root, manifest, b, bv, review?.candidate_b, 'B', flags);
  for (const key of ['effective_config', 'jobs']) if (stable(av?.details?.[key]) !== stable(bv?.details?.[key])) flags.push(`candidate ${key} differs`);
  if (stable(comparableTemplate(av?.details?.effective_template)) !== stable(comparableTemplate(bv?.details?.effective_template))) flags.push('candidate effective_template differs');
  if (ac.concurrency !== bc.concurrency) flags.push('candidate actual concurrency differs');
  if (ac.bounds && bc.bounds && compareInstants(ac.bounds[1], bc.bounds[0]) > 0 && compareInstants(bc.bounds[1], ac.bounds[0]) > 0) flags.push('candidate process timing overlaps');
  if (sameUnchangedSource(a, b)) flags.push('candidate source output is the same unchanged file');
  if (canonicalLocale(review?.candidate_a?.locale) !== canonicalLocale(review?.candidate_b?.locale) || review?.candidate_a?.timezone !== review?.candidate_b?.timezone) flags.push('candidate effective locale or timezone differs');
  const bytes = await compareBytes(path.join(root, 'captures', aLabel, 'output.bin'), path.join(root, 'captures', bLabel, 'output.bin'));
  bindComparison(bytes, a, b, 'candidate A/B', flags);
  const status = flags.length ? 'invalid-or-incomplete-case' : bytes.identical ? 'candidate-repeated-byte-match' : 'candidate-repeated-byte-mismatch';
  const report = { schema: 'parity-matrix-candidate-comparison-v1', criterion: 'complete-file-sha256-and-length', status, flags, candidate_a: aLabel, candidate_b: bLabel, comparison: bytes, timing: { a: [a.started_at_utc, a.finished_at_utc], b: [b.started_at_utc, b.finished_at_utc] }, review, metadata_normalized: false, reference_app_observed: false, global_equivalence_claimed: false, attestation_limit: 'Context reviews are operator attestations, not independently authenticated process observations.' };
  const dest = path.join(root, 'reports', `${reportName}.json`);
  await writeJson(dest, report);
  console.log(JSON.stringify({ report: dest, ...report }, null, 2));
  process.exitCode = flags.length ? 2 : bytes.identical ? 0 : 1;
}
async function compare(o) {
  const { root, manifest } = await caseInfo(o), aLabel = id(required(o, 'a')), bLabel = id(required(o, 'b')), reportName = id(required(o, 'report'));
  if (aLabel === bLabel) fail('A and B must be distinct captures');
  const a = await loadReceipt(root, aLabel), b = await loadReceipt(root, bLabel);
  if (a.role !== 'original' || b.role !== 'original') fail('A and B must be independently attested original exports');
  const flags = [], ac = checkOriginal(a, 'A', root, manifest, flags), bc = checkOriginal(b, 'B', root, manifest, flags);
  if (stable(comparableReference(a.observation?.reference)) !== stable(comparableReference(b.observation?.reference))) flags.push('original reference differs');
  if (stable(comparableEnvironment(a.observation?.environment)) !== stable(comparableEnvironment(b.observation?.environment))) flags.push('original environment differs');
  for (const key of ['settings', 'process_context']) if (stable(a.observation?.[key]) !== stable(b.observation?.[key])) flags.push(`original ${key} differs`);
  compareOriginalSettings(a, b, flags);
  if (a.observation?.process_instance?.pid !== b.observation?.process_instance?.pid || (ac.processStart && bc.processStart && compareInstants(ac.processStart, bc.processStart) !== 0)) flags.push('original process instance differs');
  if (ac.concurrency !== bc.concurrency) flags.push('original actual concurrency differs');
  if (a.observation?.render_event?.id === b.observation?.render_event?.id) flags.push('original render events are not distinct');
  if (sameUnchangedSource(a, b)) flags.push('original source output is the same unchanged file');
  if (ac.bounds && bc.bounds && ac.recorded && bc.recorded) {
    const aBefore = compareInstants(ac.bounds[1], bc.bounds[0]) <= 0 && compareInstants(ac.recorded, bc.bounds[0]) <= 0;
    const bBefore = compareInstants(bc.bounds[1], ac.bounds[0]) <= 0 && compareInstants(bc.recorded, ac.bounds[0]) <= 0;
    if (!aBefore && !bBefore) flags.push('original renders and captures are not distinct ordered events');
  }
  if (!validSourceName(a.output.source_name) || !validSourceName(b.output.source_name) || a.output.source_name !== b.output.source_name) flags.push('original output path unknown or differs');
  const current = await inputDigest(root, manifest);
  if ([a, b].some(x => stable(x.input) !== stable(current) || stable(x.input_after) !== stable(current))) flags.push('original input identity differs');
  const file = label => path.join(root, 'captures', label, 'output.bin');
  const aa = await compareBytes(file(aLabel), file(bLabel));
  bindComparison(aa, a, b, 'original A/B', flags);
  const originalFlags = [...flags], originalStatus = originalFlags.length ? 'invalid-or-incomplete-case' : aa.identical ? 'original-A-B-match-only' : 'observed-original-A-B-mismatch';
  const timing = { original_a: a.observation?.timing ?? null, original_b: b.observation?.timing ?? null, candidate: null };
  let ar = null, br = null, review = null;
  if (o.rust !== undefined) {
    const rLabel = id(o.rust), r = await loadReceipt(root, rLabel);
    if (r.role !== 'rust') fail('candidate must be a CLI run capture');
    if (!validSourceName(r.output.source_name) || r.output.source_name !== a.output.source_name) flags.push('candidate output path unknown or differs');
    if (sameUnchangedSource(r, a) || sameUnchangedSource(r, b)) flags.push('candidate source output is the same unchanged original file');
    if (stable(r.input) !== stable(current) || stable(r.input_after) !== stable(current)) flags.push('candidate input identity differs');
    const candidate = await capturedStdout(root, r, 'candidate', flags);
    review = o.review === undefined ? null : await comparisonJson(o.review, 'candidate review', flags);
    if (review?.same_effective_settings !== true || !evidenceText(review?.settings_evidence, [a.observation?.effective_settings?.template?.name, candidate?.details?.effective_template?.name])) flags.push('matching effective settings not independently attested');
    const rc = checkCandidate(root, manifest, r, candidate, review?.candidate, 'run', flags);
    if (rc.jobs !== a.observation?.settings?.jobs) flags.push('candidate effective jobs differs from original');
    if (candidate?.details?.effective_config?.processor_max !== a.observation?.settings?.jobs) flags.push('candidate configured processor maximum differs from original');
    checkTemplateMapping(a.observation?.settings?.template_id, candidate?.details?.effective_template?.unique_id, review, flags);
    compareCandidateSettings(a, candidate, manifest, flags);
    if (rc.concurrency !== ac.concurrency || rc.concurrency !== bc.concurrency) flags.push('candidate actual concurrency differs from original');
    if (r.runtime?.platform !== 'darwin' || r.runtime?.architecture !== a.observation?.reference?.architecture || r.runtime?.macos_build !== a.observation?.environment?.macos_build || canonicalLocale(review?.candidate?.locale) !== canonicalLocale(a.observation?.environment?.locale) || review?.candidate?.timezone !== a.observation?.environment?.timezone) flags.push('candidate runtime differs from original environment or architecture');
    checkStageReview(review, candidate, a, b, root, r, flags);
    await checkStageSourceBindings(root, r, review, flags);
    timing.candidate = [r.started_at_utc, r.finished_at_utc];
    ar = await compareBytes(file(aLabel), file(rLabel)); br = await compareBytes(file(bLabel), file(rLabel));
    bindComparison(ar, a, r, 'original A/candidate', flags); bindComparison(br, b, r, 'original B/candidate', flags);
  } else if (o.review !== undefined) fail('--review requires --rust');
  let status, exit;
  if (flags.length) { status = 'invalid-or-incomplete-case'; exit = 2; }
  else if (!aa.identical) { status = 'observed-original-A-B-mismatch'; exit = 1; }
  else if (ar && (!ar.identical || !br.identical)) { status = 'candidate-byte-mismatch'; exit = 1; }
  else if (ar) { status = 'reviewed-complete-file-match-for-this-case'; exit = 0; }
  else { status = 'original-A-B-match-only'; exit = 0; }
  const report = { schema: 'parity-matrix-comparison-v1', criterion: 'complete-file-sha256-and-length', status, flags, original_a: aLabel, original_b: bLabel, candidate: o.rust ?? null, original_a_vs_b: aa, original_a_vs_candidate: ar, original_b_vs_candidate: br, timing, review, metadata_normalized: false, audio_decoded_for_acceptance: false, global_equivalence_claimed: false, reference_repeatability_scope: 'Only these attested runs; A/B equality does not prove general determinism.', stage_activation: 'Stage observations or explicit inapplicability are independently attested in the review, not inferred from a template or byte match.', attestation_limit: 'Captures and reviews are operator attestations, not independently authenticated GUI or process observations.' };
  report.original_comparison_status = originalStatus;
  report.original_flags = originalFlags;
  const dest = path.join(root, 'reports', `${reportName}.json`);
  await writeJson(dest, report);
  console.log(JSON.stringify({ report: dest, ...report }, null, 2));
  process.exitCode = exit;
}
try {
  const [cmd, ...args] = process.argv.slice(2);
  if (!cmd || cmd === '--help' || cmd === 'help') console.log(HELP);
  else if (cmd === 'init') await init(options(args, ['case', 'rate', 'channels', 'bits', 'frames', 'profile', 'hz-milli', 'input-format', 'ffmpeg']));
  else if (cmd === 'preflight') await preflight(options(args, ['case']));
  else if (cmd === 'capture-original') await captureOriginal(options(args, ['case', 'label', 'file', 'observation']));
  else if (cmd === 'park') await park(options(args, ['case', 'label']));
  else if (cmd === 'run') await run(options(args, ['case', 'label', 'executable', 'template-id', 'format', 'mp3-mode', 'jobs', 'controlled-mp4-time-1904']));
  else if (cmd === 'compare-candidates') await compareCandidates(options(args, ['case', 'a', 'b', 'review', 'report']));
  else if (cmd === 'compare') await compare(options(args, ['case', 'a', 'b', 'rust', 'review', 'report']));
  else fail(`unknown command: ${cmd}`);
} catch (e) { console.error(`parity-matrix: ${e.message}`); process.exitCode = 2; }
