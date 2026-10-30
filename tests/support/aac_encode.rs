//! Shared by `tests/portable_aac_encode.rs`: the golden fixture manifest, a scratch directory, and the replay helpers that
//! drive the public portable `Writer` the way the engine does.
//!
//! `tests/fixtures/aac-encode/` holds, per generated input, what the engine's macOS `apple_codec::Writer` (AudioToolbox
//! `ExtAudioFileWrite`, 512-frame writes, stack pinned) produced: the SHA-256 and length of the complete `.m4a` after the
//! MP4 clock is normalised to [`FIXED_CLOCK`], plus access-unit diagnostics, and `refusals.json` records what
//! `Writer::create` answered for a grid of rates and channel counts. Nothing here needs Apple at run time. See
//! `tests/fixtures/aac-encode/README.md` for provenance and how the fixtures are rebuilt.
#![allow(dead_code)]

#[path = "aac_encode_input.rs"]
pub mod input;

pub use input::{BLOCK, FIXED_CLOCK, Source, Spec, hex};

use platinum_engine::portable::audio::{CodecError, CodecWarning, OutputFormat, Writer, WriterConfig, set_mp4_header_times};
use platinum_engine::portable::digest::sha256;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn fixture_dir() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aac-encode") }

pub struct Item {
    pub id: String,
    pub group: String,
    pub spec: Spec,
    pub input_sha256: String,
    pub m4a_sha256: String,
    pub m4a_length: usize,
    /// Top-level box types of the Apple file in file order, e.g. `ftyp free mdat moov`.
    pub layout: String,
    pub au_count: usize,
    pub first_au_sha256: String,
    pub au_stream_sha256: String,
    /// Empty for the long items.
    pub au_sizes: Vec<u32>,
    pub au_sha8: Vec<String>,
    /// The raw Apple file as kept in the pack, for the smaller items.
    pub reference: Option<Reference>,
    /// Write sizes (frames; 0 = one write) Apple was verified to be independent of, for the items that carry them.
    pub write_sizes: Vec<usize>,
    /// Apple's file hash when the whole encode runs inside the engine's flush-to-zero scope.
    pub fz_sha256: Option<String>,
}
impl Item {
    pub fn seconds(&self) -> f64 { self.spec.frames as f64 / f64::from(self.spec.rate) }
}

pub struct Fixtures {
    pub manifest: Value,
    pub pack: Vec<u8>,
    pub items: Vec<Item>,
}
impl Fixtures {
    pub fn load() -> &'static Fixtures {
        static FIXTURES: LazyLock<Fixtures> = LazyLock::new(Fixtures::build);
        &FIXTURES
    }
    fn build() -> Fixtures {
        let dir = fixture_dir();
        let manifest: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json")).expect("read manifest.json")).expect("parse manifest.json");
        assert_eq!(manifest["schema"], "platinum-aac-encode-fixtures-v1");
        let pack = fs::read(dir.join(manifest["pack"]["file"].as_str().unwrap())).expect("read the reference pack");
        let text = |v: &Value| v.as_str().unwrap().to_owned();
        let items = manifest["items"].as_array().unwrap().iter().map(|v| Item {
            id: text(&v["id"]),
            group: text(&v["group"]),
            spec: Spec::from_json(v).expect("item spec"),
            input_sha256: text(&v["input_sha256"]),
            m4a_sha256: text(&v["m4a_sha256"]),
            m4a_length: v["m4a_length"].as_u64().unwrap() as usize,
            layout: text(&v["layout"]),
            au_count: v["au_count"].as_u64().unwrap() as usize,
            first_au_sha256: text(&v["first_au_sha256"]),
            au_stream_sha256: text(&v["au_stream_sha256"]),
            au_sizes: v["au_sizes"].as_array().map(|a| a.iter().map(|n| n.as_u64().unwrap() as u32).collect()).unwrap_or_default(),
            au_sha8: v["au_sha8"].as_str().map(|s| s.split(' ').filter(|p| !p.is_empty()).map(str::to_owned).collect()).unwrap_or_default(),
            reference: v["ref"].as_object().map(|r| Reference {
                offset: r["offset"].as_u64().unwrap() as usize,
                length: r["length"].as_u64().unwrap() as usize,
                zeros: r["zeros"].as_array().unwrap().iter().map(|z| (z[0].as_u64().unwrap() as usize, z[1].as_u64().unwrap() as usize)).collect(),
            }),
            write_sizes: v["write_sizes"].as_array().map(|a| a.iter().map(|n| n.as_u64().unwrap() as usize).collect()).unwrap_or_default(),
            fz_sha256: v["fz_sha256"].as_str().map(str::to_owned),
        }).collect();
        Fixtures { manifest, pack, items }
    }
    pub fn group(&self, name: &str) -> Vec<&Item> { self.items.iter().filter(|i| i.group == name).collect() }
    pub fn get(&self, id: &str) -> &Item { self.items.iter().find(|i| i.id == id).unwrap_or_else(|| panic!("no item {id}")) }
    /// The Apple file of a kept item: the pack stores it without its long runs of zero bytes (the `free` reservation).
    pub fn reference(&self, item: &Item) -> Option<Vec<u8>> {
        let kept = item.reference.as_ref()?;
        let stored = &self.pack[kept.offset..kept.offset + kept.length];
        let mut file = Vec::new();
        let mut used = 0;
        for &(position, length) in &kept.zeros {
            let take = position - file.len();
            file.extend_from_slice(&stored[used..used + take]);
            used += take;
            file.resize(file.len() + length, 0);
        }
        file.extend_from_slice(&stored[used..]);
        Some(file)
    }
}

/// `refusals.json`: Writer::create outcomes, bit depths and misuse.
pub fn refusals() -> &'static Value {
    static DOCUMENT: LazyLock<Value> = LazyLock::new(|| {
        let document: Value = serde_json::from_slice(&fs::read(fixture_dir().join("refusals.json")).expect("read refusals.json")).expect("parse refusals.json");
        assert_eq!(document["schema"], "platinum-aac-encode-refusals-v1");
        document
    });
    &DOCUMENT
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A private temporary directory, removed on drop.
pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("platinum-aac-encode-{}-{tag}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).expect("create scratch directory");
        Self(dir)
    }
    /// A fresh empty subdirectory.
    pub fn dir(&self) -> PathBuf {
        let dir = self.0.join(format!("d{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).expect("create scratch subdirectory");
        dir
    }
}
impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

pub fn config(rate: f64, channels: u32, depth: u32) -> WriterConfig { WriterConfig { format: OutputFormat::Aac, sample_rate: rate, channels, bit_depth: depth } }
pub fn config_of(spec: &Spec) -> WriterConfig { config(f64::from(spec.rate), spec.channels as u32, 0) }

/// The classification both the recorder and the replay compare; the same shape as `encode_probe`'s `describe`.
pub fn describe(error: &CodecError) -> Value {
    match error {
        CodecError::InvalidInput(message) => json!({"kind": "InvalidInput", "message": message}),
        CodecError::Native { operation, status } => json!({"kind": "Native", "operation": operation, "status": status}),
        CodecError::Io { operation, source, .. } => json!({"kind": "Io", "operation": operation, "io_kind": format!("{:?}", source.kind())}),
        CodecError::Cleanup { primary, .. } => json!({"kind": "Cleanup", "primary": describe(primary)}),
        CodecError::Poisoned => json!({"kind": "Poisoned"}),
        other => json!({"kind": "Other", "text": other.to_string()}),
    }
}
pub fn describe_warnings(warnings: &[CodecWarning]) -> Value {
    Value::Array(warnings.iter().map(|w| match w {
        CodecWarning::AacConverterUnavailable { status } => json!({"kind": "AacConverterUnavailable", "status": status}),
        CodecWarning::AacBitRateNotSet { status } => json!({"kind": "AacBitRateNotSet", "status": status}),
    }).collect())
}
pub fn result_of<T>(result: &Result<T, CodecError>) -> Value {
    match result { Ok(_) => json!({"result": "ok"}), Err(e) => json!({"result": "err", "error": describe(e)}) }
}
pub fn listing(dir: &Path) -> Value {
    let mut names: Vec<String> = fs::read_dir(dir).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    json!(names)
}

/// Feed `spec`'s stream, or the pre-rendered `data`, to `writer` in `block`-frame writes (0 = one write).
pub fn feed(writer: &mut Writer, spec: &Spec, block: usize, data: Option<&[f32]>) -> Result<(), CodecError> {
    if let Some(data) = data {
        if block == 0 { return writer.write(data); }
        for chunk in data.chunks(block * spec.channels) { writer.write(chunk)?; }
        return Ok(());
    }
    let mut source = Source::new(spec).expect("spec");
    let mut buffer = Vec::new();
    let step = if block == 0 { spec.frames.max(1) as usize } else { block };
    while source.next_block(step, &mut buffer) > 0 { writer.write(&buffer)?; }
    Ok(())
}

/// The engine's sequence for one file: create, write, finish, then the clock normalised like the reference. Returns the
/// bytes of the published file; a failure says which step it was.
pub fn encode(dir: &Path, name: &str, spec: &Spec, block: usize, data: Option<&[f32]>) -> Result<Vec<u8>, String> {
    let path = dir.join(name);
    let mut writer = Writer::create(&path, config_of(spec)).map_err(|e| format!("create: {e:?}"))?;
    if !writer.warnings().is_empty() { return Err(format!("create returned warnings {:?}", writer.warnings())); }
    feed(&mut writer, spec, block, data).map_err(|e| format!("write: {e:?}"))?;
    writer.finish().map_err(|e| format!("finish: {e:?}"))?;
    set_mp4_header_times(&path, FIXED_CLOCK).map_err(|e| format!("clock: {e}"))?;
    let bytes = fs::read(&path).map_err(|e| format!("read back: {e}"))?;
    let _ = fs::remove_file(&path);
    Ok(bytes)
}

/// `None` when `bytes` is Apple's file for `item`; otherwise what differs, as precisely as the fixtures allow.
pub fn difference(fixtures: &Fixtures, item: &Item, bytes: &[u8], expected_sha256: &str) -> Option<String> {
    let sha = hex(&sha256(bytes));
    if sha == expected_sha256 && (expected_sha256 != item.m4a_sha256 || bytes.len() == item.m4a_length) { return None; }
    let mut notes = vec![format!("length {} (expected {}), sha256 {sha} (expected {expected_sha256})", bytes.len(), item.m4a_length)];
    if let Some(reference) = fixtures.reference(item) {
        match bytes.iter().zip(&reference).position(|(a, b)| a != b) {
            Some(at) => notes.push(format!("first differing byte {at} ({} in the portable file, {} in Apple's)", input::locate(bytes, at), input::locate(&reference, at))),
            None => notes.push(format!("identical for the first {} bytes", bytes.len().min(reference.len()))),
        }
    }
    notes.push(format!("boxes: {} (expected {})", input::layout(bytes).iter().map(|b| b.0.clone()).collect::<Vec<_>>().join(" "), item.layout));
    match input::summarize(bytes) {
        Err(e) => notes.push(format!("not an AAC file the reader understands: {e}")),
        Ok(got) => {
            notes.push(format!("{} access units (expected {})", got.count, item.au_count));
            notes.push(format!("first AU {} (expected {}); AU stream {} (expected {})", got.first_sha256 == item.first_au_sha256, item.first_au_sha256, got.stream_sha256 == item.au_stream_sha256, item.au_stream_sha256));
            if !item.au_sha8.is_empty() {
                match got.sha8.iter().zip(&item.au_sha8).position(|(a, b)| a != b) {
                    Some(i) => notes.push(format!("first differing access unit: {i} (size {} vs expected {})", got.sizes[i], item.au_sizes.get(i).copied().unwrap_or(0))),
                    None if got.count != item.au_count => notes.push("every access unit up to the shorter count is identical".into()),
                    None => notes.push("every access unit is identical: only the container differs".into()),
                }
            }
        }
    }
    Some(notes.join("; "))
}

/// Replay one item at one write size; `Err` carries what differs from Apple (or why the portable writer failed).
pub fn replay(fixtures: &Fixtures, scratch: &Scratch, item: &Item, block: usize) -> Result<(), String> {
    let dir = scratch.dir();
    let bytes = encode(&dir, &format!("{}.m4a", item.id), &item.spec, block, None)?;
    match difference(fixtures, item, &bytes, &item.m4a_sha256) { None => Ok(()), Some(d) => Err(d) }
}

// ---- the refusal grid and its mirror of the recorder ---------------------------------------------------------------------------------

pub const WRITE_FRAMES: u64 = 1100;

/// 1100 frames of seeded white noise for 1..=8 channels (anything else: a constant), as the recorder wrote them.
pub fn probe_data(channels: u32) -> Vec<f32> {
    if channels == 0 || channels > 8 { return vec![0.25; WRITE_FRAMES as usize * channels.max(1) as usize]; }
    let spec = Spec { kind: "white".into(), rate: 48000, channels: channels as usize, frames: WRITE_FRAMES, seed: 0x5eed_0001, params: [400, 0, 0, 0] };
    input::render(&spec).expect("probe spec")
}

fn finish_record(path: &Path, finished: &Result<(), CodecError>, slot: &mut Value) {
    if finished.is_ok() {
        set_mp4_header_times(path, FIXED_CLOCK).expect("normalise the clock");
        let bytes = fs::read(path).expect("read the finished file");
        slot["sha256"] = json!(hex(&sha256(&bytes)));
        slot["length"] = json!(bytes.len());
        fs::remove_file(path).expect("remove the finished file");
    }
}

/// What `Writer::create` does for `(rate, channels, depth)` and what follows; the portable twin of the recorder's `cell`,
/// producing the same JSON.
pub fn run_cell(dir: &Path, rate: f64, channels: u32, depth: u32) -> Value {
    let target = dir.join("out.m4a");
    let cfg = config(rate, channels, depth);
    let data = probe_data(channels);
    let per = channels.max(1) as usize;
    let mut record = json!({});
    match Writer::create(&target, cfg) {
        Err(e) => {
            record["create"] = result_of::<()>(&Err(e));
            record["leftover"] = listing(dir);
        }
        Ok(writer) => {
            let has_warnings = !writer.warnings().is_empty();
            record["create"] = json!({"result": "ok", "warnings": describe_warnings(writer.warnings())});
            record["abort"] = result_of(&writer.abort());
            record["leftover_after_abort"] = listing(dir);
            drop(Writer::create(&target, cfg));
            record["leftover_after_drop"] = listing(dir);
            let empty = Writer::create(&target, cfg).and_then(|w| w.finish());
            record["empty_finish"] = result_of(&empty);
            finish_record(&target, &empty, &mut record["empty_finish"]);
            record["leftover_after_empty_finish"] = listing(dir);
            let mut steps = json!({});
            match Writer::create(&target, cfg) {
                Err(e) => steps["create"] = result_of::<()>(&Err(e)),
                Ok(mut w) => {
                    let mut failure = None;
                    for chunk in data.chunks(BLOCK * per) {
                        if let Err(e) = w.write(chunk) { failure = Some(describe(&e)); break; }
                    }
                    steps["write"] = match &failure { None => json!({"result": "ok"}), Some(e) => json!({"result": "err", "error": e}) };
                    if failure.is_some() { steps["write_again"] = result_of(&w.write(&data[..per])); }
                    let finished = w.finish();
                    steps["finish"] = result_of(&finished);
                    finish_record(&target, &finished, &mut steps["finish"]);
                }
            }
            steps["leftover"] = listing(dir);
            if let Ok(mut w) = Writer::create(&target, cfg) {
                let _ = w.write(&data[..per * 600]);
                steps["abort_after_write"] = result_of(&w.abort());
                steps["leftover_after_abort_after_write"] = listing(dir);
            }
            record["post"] = steps;
            record["has_warnings"] = json!(has_warnings);
        }
    }
    record
}

pub fn rate_of(cell: &Value) -> f64 {
    let bits = cell["rate_bits"].as_str().unwrap();
    f64::from_bits(u64::from_str_radix(bits.trim_start_matches("0x"), 16).unwrap())
}

/// The keys of a recorded cell that are the portable writer's to reproduce, with the recorded values. A step that never ran on either side (no
/// `writes` after a failed `create`) is not a difference, but a record must not be vacuous: the first key (`create`: the step every record has) must be
/// recorded, and at least one key must have been compared.
pub fn mismatches(label: &str, expected: &Value, actual: &Value, keys: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let mut compared = 0;
    if keys.first().is_some_and(|k| expected.get(*k).is_none()) {
        found.push(format!("{label}: {}: the recording has no such key", keys[0]));
    }
    for k in keys {
        let (want, got) = (expected.get(*k), actual.get(*k));
        if want.is_none() && got.is_none() {
            continue;
        }
        compared += 1;
        if want != got {
            found.push(format!("{label}: {k}: expected {}, got {}", want.unwrap_or(&Value::Null), got.unwrap_or(&Value::Null)));
        }
    }
    if compared == 0 {
        found.push(format!("{label}: none of {keys:?} is recorded or replayed, nothing was compared"));
    }
    found
}

/// Where a kept reference lives in the pack: `length` stored bytes at `offset`, and the runs of zero bytes (`(position in
/// the file, length)`) that were left out.
pub struct Reference { pub offset: usize, pub length: usize, pub zeros: Vec<(usize, usize)> }

// ---- bit depths, destination paths and write misuse: the portable twins of the recorder's `misc` ---------------------------------

/// A finished file of `blocks`, as (create, writes, finish, sha256, length, leftover).
pub fn finished_probe(dir: &Path, cfg: WriterConfig, blocks: &[&[f32]]) -> Value {
    let target = dir.join("out.m4a");
    let mut record = json!({});
    match Writer::create(&target, cfg) {
        Err(e) => record["create"] = result_of::<()>(&Err(e)),
        Ok(mut w) => {
            record["create"] = json!({"result": "ok", "warnings": describe_warnings(w.warnings())});
            record["writes"] = Value::Array(blocks.iter().map(|b| result_of(&w.write(b))).collect());
            let finished = w.finish();
            record["finish"] = result_of(&finished);
            if finished.is_ok() {
                set_mp4_header_times(&target, FIXED_CLOCK).expect("normalise the clock");
                let bytes = fs::read(&target).expect("read the finished file");
                record["sha256"] = json!(hex(&sha256(&bytes)));
                record["length"] = json!(bytes.len());
            }
        }
    }
    record["leftover"] = listing(dir);
    let _ = fs::remove_file(&target);
    record
}

/// Which of several failing checks wins, with a pre-existing target or not.
pub fn combined_case(dir: &Path, rate: f64, channels: u32, depth: u32, existing: bool) -> Value {
    let target = dir.join("out.m4a");
    if existing { fs::write(&target, b"existing").expect("write the existing target"); }
    let r = Writer::create(&target, config(rate, channels, depth));
    let mut record = json!({"create": result_of(&r), "leftover": listing(dir)});
    if existing { record["existing_unchanged"] = json!(fs::read(&target).map(|b| b == b"existing").unwrap_or(false)); }
    drop(r);
    record["leftover_after_drop"] = listing(dir);
    record
}

/// Unusable destination paths.
pub fn path_case(dir: &Path, case: &str) -> Value {
    let cfg = config(44100.0, 2, 0);
    match case {
        "missing_parent" => json!({"create": result_of(&Writer::create(dir.join("no-such-dir").join("out.m4a"), cfg)), "leftover": listing(dir)}),
        "empty" => json!({"create": result_of(&Writer::create("", cfg))}),
        "nul" => json!({"create": result_of(&Writer::create("a\0b.m4a", cfg))}),
        "root" => json!({"create": result_of(&Writer::create("/", cfg))}),
        "directory" => {
            let as_dir = dir.join("is-a-dir");
            fs::create_dir(&as_dir).expect("create the directory");
            json!({"create": result_of(&Writer::create(&as_dir, cfg)), "leftover": listing(dir)})
        }
        other => panic!("unknown path case {other}"),
    }
}

/// Write misuse on a healthy 44.1 kHz stereo writer.
pub fn misuse_case(dir: &Path, case: &str, data: &[f32]) -> Value {
    let empty: &[f32] = &[];
    let ragged = &data[..3];
    let blocks: Vec<&[f32]> = match case {
        "reference" => vec![data],
        "empty_write" => vec![empty, data, empty],
        "ragged_then_valid" => vec![ragged, data],
        "ragged_only" => vec![ragged],
        "two_writes" => vec![&data[..1000], &data[1000..]],
        "no_write" => vec![],
        other => panic!("unknown misuse case {other}"),
    };
    finished_probe(dir, config(44100.0, 2, 0), &blocks)
}

// ---- refusal-grid and flush-to-zero helpers shared by the test targets ------------------------------------------------------------

pub fn integer_rate(rate: f64) -> bool { rate.is_finite() && rate.fract() == 0.0 }

/// The "not reproduced" list: rates AudioToolbox serves (a converter is created and a file is written) but whose access units follow the exact
/// rate, not the nominal AAC rate the file declares. Each is given with its nominal rate. The portable writer refuses them at `create` with
/// `CodecError::UnsupportedFormat`; Apple's recorded files for them stay in `refusals.json` as the evidence that the refusal is justified.
pub const NOT_REPRODUCED: [(&str, &str); 4] = [("44100.5", "44100"), ("44099.99999", "44100"), ("44100.00001", "44100"), ("47999.999", "48000")];

/// A cell Apple created without a warning at a rate that is not an integer: one of [`NOT_REPRODUCED`] on 1..=8 channels.
pub fn not_reproduced(cell: &Value) -> bool { created(cell) && !warned(cell) && !integer_rate(rate_of(cell)) }

/// The recorded cell for a rate label and channel count.
pub fn cell_for(label: &str, channels: u64) -> &'static Value {
    cells().iter().find(|c| c["rate_label"] == label && c["channels"] == channels).unwrap_or_else(|| panic!("no cell {label} Hz x {channels}"))
}

pub fn cells() -> &'static Vec<Value> { refusals()["cells"].as_array().unwrap() }

pub fn warned(cell: &Value) -> bool { cell["create"]["warnings"].as_array().is_some_and(|w| !w.is_empty()) }
pub fn created(cell: &Value) -> bool { cell["create"]["result"] == "ok" }

/// Replay the selected cells and compare the `keys(cell)` of each with the recording; returns the number of cells and every
/// mismatch.
pub fn run_cells(select: impl Fn(&Value) -> bool, keys: impl Fn(&Value) -> Vec<&'static str>) -> (usize, Vec<String>) {
    let scratch = Scratch::new("cells");
    let (mut count, mut bad) = (0, Vec::new());
    for cell in cells().iter().filter(|c| select(c)) {
        let (rate, channels) = (rate_of(cell), cell["channels"].as_u64().unwrap() as u32);
        let dir = scratch.dir();
        let actual = run_cell(&dir, rate, channels, 0);
        let _ = fs::remove_dir_all(&dir);
        let label = format!("{} Hz x {channels} ch", cell["rate_label"].as_str().unwrap());
        bad.extend(mismatches(&label, cell, &actual, &keys(cell)));
        count += 1;
    }
    (count, bad)
}

/// Replay the selected flush-to-zero items inside the engine's flush-to-zero scope and compare with Apple's file for that mode.
pub fn flush_to_zero_replays(select: impl Fn(&Item) -> bool) -> (usize, Vec<String>) {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("fz");
    let mut bad = Vec::new();
    let mut checked = 0;
    for item in fixtures.items.iter().filter(|i| i.fz_sha256.is_some() && select(i)) {
        // the input is rendered outside the scope so that only the writer runs flushed
        let data = input::render(&item.spec).unwrap();
        let dir = scratch.dir();
        let result = platinum_engine::fp_mode::with_flush_to_zero(|| encode(&dir, "fz.m4a", &item.spec, BLOCK, Some(&data)));
        checked += 1;
        match result {
            Err(e) => bad.push(format!("{}: {e}", item.id)),
            Ok(bytes) => if let Some(d) = difference(fixtures, item, &bytes, item.fz_sha256.as_ref().unwrap()) { bad.push(format!("{}: {d}", item.id)); },
        }
    }
    (checked, bad)
}
