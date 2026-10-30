//! Shared by `portable_aac_decode.rs` and `portable_aac_hostile.rs`: the golden AAC decoder fixture pack, a scratch
//! directory, and the reference decode loops.
//!
//! `tests/fixtures/aac-decode/` holds generated input files (`pack-N.bin`) and, per input, what the engine's macOS
//! `apple_codec::Reader` (AudioToolbox `ExtAudioFileOpenURL` / `ExtAudioFileRead`) did with it (`manifest.json`): the open
//! outcome, the source description, the file-length and bitrate properties, and per read shape (planar stereo, and
//! interleaved at the source channel count) the frames delivered, the failure that ended the stream, the SHA-256 of the
//! float32 stream, the read lengths and what the reads wrote behind the frames they returned. `expect` says what the
//! portable reader must do with it. Nothing here needs Apple at run time. See `tests/fixtures/aac-decode/README.md`.
#![allow(dead_code)]

use platinum_engine::portable::audio::{CodecError, Reader};
use platinum_engine::portable::digest::Sha256;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

// ---- allocation accounting --------------------------------------------------------------------------------------------------------
// Every test binary that includes this module counts the allocations of each thread: what a decode may hold is asserted in bytes, not in the
// resident set of a process that other tests run in.

thread_local! {
    static LIVE: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static PEAK: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static LARGEST: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct Counting;

fn track(delta: isize, size: usize) {
    let live = LIVE.with(|live| { live.set(live.get() + delta); live.get() });
    PEAK.with(|peak| if live > peak.get() { peak.set(live) });
    LARGEST.with(|largest| if size > largest.get() { largest.set(size) });
}

// SAFETY: every method forwards to `System` with the arguments it was given; the accounting touches thread-local `Cell`s of `Copy` types only
// (const-initialised, no destructor, so no allocation and no re-entry).
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 { track(layout.size() as isize, layout.size()); unsafe { std::alloc::System.alloc(layout) } }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 { track(layout.size() as isize, layout.size()); unsafe { std::alloc::System.alloc_zeroed(layout) } }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) { track(-(layout.size() as isize), 0); unsafe { std::alloc::System.dealloc(ptr, layout) } }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        track(new_size as isize - layout.size() as isize, new_size);
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Starts counting the calling thread's allocations from zero.
pub fn reset_allocation_counters() {
    LIVE.with(|live| live.set(0));
    PEAK.with(|peak| peak.set(0));
    LARGEST.with(|largest| largest.set(0));
}

/// (largest single allocation, most bytes live at once) of the calling thread since the reset.
pub fn allocation_counters() -> (usize, usize) {
    (LARGEST.with(std::cell::Cell::get), PEAK.with(std::cell::Cell::get).max(0) as usize)
}

/// The block size of every engine read, and the contract of the port.
pub const BLOCK: usize = 512;
/// What the reference harness pre-fills every read buffer with, to see what a read writes behind the frames it returns.
pub const SENTINEL: f32 = 12345.5;
pub const AAC_LC: u32 = u32::from_be_bytes(*b"aac ");
const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;
fn fold(hash: u64, bits: u32) -> u64 { (hash ^ u64::from(bits)).wrapping_mul(0x100_0000_01b3) }

pub const OPEN: &str = "ExtAudioFileOpenURL";
pub const CLIENT: &str = "ExtAudioFileSetProperty(client format)";
pub const READ: &str = "ExtAudioFileRead";
pub const READ_PLANAR: &str = "ExtAudioFileRead(planar stereo)";

pub fn fixture_dir() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aac-decode") }

pub fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }

pub fn sha_hex(bytes: &[u8]) -> String { hex(&platinum_engine::portable::digest::sha256(bytes)) }

/// A native failure (`operation` and signed `OSStatus`), or what the engine's own wrapper reports instead of one:
/// `operation` is then `other:<variant>` and the status 0. Equality ignores `detail` (the error's text).
#[derive(Clone, Debug)]
pub struct Failure { pub operation: String, pub status: i32, pub detail: String }
impl PartialEq for Failure { fn eq(&self, other: &Self) -> bool { self.operation == other.operation && self.status == other.status } }
impl Eq for Failure {}

pub fn failure(error: &CodecError) -> Failure {
    let other = |name: &str| Failure { operation: format!("other:{name}"), status: 0, detail: error.to_string() };
    match error {
        CodecError::Native { operation, status } => Failure { operation: (*operation).to_owned(), status: *status, detail: String::new() },
        CodecError::InvalidInput(_) => other("InvalidInput"),
        CodecError::UnsupportedFormat(_) => other("UnsupportedFormat"),
        CodecError::UnsupportedCodec { .. } => other("UnsupportedCodec"),
        CodecError::UnsupportedSampleRateConversion { .. } => other("UnsupportedSampleRateConversion"),
        CodecError::Poisoned => other("Poisoned"),
        _ => other("Io-or-container"),
    }
}

/// How the engine's own wrapper words a refusal that is not a native status: `{"other": "<message>"}` in the manifest.
fn recorded_other(message: &str) -> Failure {
    let name = if message.starts_with("invalid audio codec input") { "InvalidInput" } else { "Unexpected" };
    Failure { operation: format!("other:{name}"), status: 0, detail: message.to_owned() }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Asbd {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels: u32,
    pub bits: u32,
    pub reserved: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prop { Value(i64), Error(Failure) }

#[derive(Clone, Debug)]
pub struct Info { pub source: Asbd, pub client: Asbd, pub length: Prop, pub bitrate: Prop }

/// `(frames returned by a short read, frames the read wrote behind them, FNV-1a of those frames' float bits)`.
pub type Tail = (usize, usize, String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub frames: u64,
    pub reads: u64,
    pub short: u64,
    pub lens_sha256: String,
    pub error: Option<Failure>,
    pub sha256: String,
    pub tails: Vec<Tail>,
}
impl Stream {
    /// Everything but the tails: the part of a recording a decoder must reproduce for the engine's samples to be right.
    pub fn core_diff(&self, actual: &Stream) -> Option<String> {
        let mut parts = Vec::new();
        if self.frames != actual.frames { parts.push(format!("frames {} vs {}", self.frames, actual.frames)); }
        if self.error != actual.error { parts.push(format!("error {:?} vs {:?}", self.error, actual.error)); }
        if self.sha256 != actual.sha256 { parts.push(format!("samples {} vs {}", &self.sha256[..12], &actual.sha256[..12])); }
        if (self.reads, self.short, &self.lens_sha256) != (actual.reads, actual.short, &actual.lens_sha256) {
            parts.push(format!("read lengths ({} reads, {} short) vs ({} reads, {} short)", self.reads, self.short, actual.reads, actual.short));
        }
        (!parts.is_empty()).then(|| format!("expected != actual: {}", parts.join("; ")))
    }
    pub fn tails_diff(&self, actual: &Stream) -> Option<String> {
        (self.tails != actual.tails).then(|| format!("tails expected {:?}, got {:?}", self.tails, actual.tails))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect { Exact, RefuseCodec, RefuseSrc, AppleError }
impl Expect {
    fn parse(text: &str) -> Expect {
        match text {
            "exact" => Expect::Exact,
            "refuse_codec" => Expect::RefuseCodec,
            "refuse_src" => Expect::RefuseSrc,
            "apple_error" => Expect::AppleError,
            other => panic!("unknown expectation {other}"),
        }
    }
}

/// Apple's own behaviour (the raw AudioToolbox handle, not the engine's poisoning wrapper) for further reads after the first
/// failed one: `(status, frames)` of each of the next reads.
#[derive(Clone, Debug)]
pub struct AfterFailure { pub first_status: i32, pub later: Vec<(i32, u32)> }

/// The same file opened with a client sample rate other than its own.
pub struct Variant {
    pub client_rate: f64,
    pub expect: Expect,
    pub open_error: Option<Failure>,
    pub info: Option<Info>,
    pub planar512: Option<Stream>,
    pub inter512: Option<Stream>,
}

pub struct Fixture {
    pub id: String,
    pub name: String,
    pub family: String,
    pub part: usize,
    pub offset: usize,
    pub length: usize,
    pub sha256: String,
    pub features: Vec<String>,
    pub expect: Expect,
    pub reason: String,
    /// `Some` when the reference refused the file at open.
    pub open_error: Option<Failure>,
    pub info: Option<Info>,
    pub planar512: Option<Stream>,
    pub inter512: Option<Stream>,
    /// For a file whose planar client format Apple refuses: what an interleaved read of that same reader delivers.
    pub inter_after_planar_refusal: Option<Stream>,
    pub after_failure: Option<AfterFailure>,
    pub variants: Vec<Variant>,
}
impl Fixture {
    pub fn opens(&self) -> bool { self.open_error.is_none() }
    pub fn has(&self, feature: &str) -> bool { self.features.iter().any(|f| f == feature) }
    pub fn channels(&self) -> usize { self.info.as_ref().map_or(0, |i| i.source.channels as usize) }
    pub fn sample_rate(&self) -> f64 { self.info.as_ref().map_or(0.0, |i| i.source.sample_rate) }
    /// Seconds of audio the reference delivered (the planar stream).
    pub fn seconds(&self) -> f64 {
        match (&self.planar512, &self.info) { (Some(s), Some(i)) if i.source.sample_rate > 0.0 => s.frames as f64 / i.source.sample_rate, _ => 0.0 }
    }
}

pub struct Fixtures {
    pub manifest: Value,
    pub parts: Vec<Vec<u8>>,
    pub files: Vec<Fixture>,
}

fn failure_json(v: &Value) -> Option<Failure> {
    if v.is_null() { return None; }
    if let Some(message) = v.get("other").and_then(Value::as_str) { return Some(recorded_other(message)); }
    Some(Failure { operation: v["operation"].as_str().unwrap().to_owned(), status: v["status"].as_i64().unwrap() as i32, detail: String::new() })
}

fn asbd_json(v: &Value) -> Asbd {
    let n = |k: &str| v[k].as_u64().unwrap() as u32;
    Asbd { sample_rate: v["sample_rate"].as_f64().unwrap(), format_id: n("format_id"), format_flags: n("format_flags"), bytes_per_packet: n("bytes_per_packet"),
        frames_per_packet: n("frames_per_packet"), bytes_per_frame: n("bytes_per_frame"), channels: n("channels"), bits: n("bits"), reserved: n("reserved") }
}

fn prop_json(v: &Value) -> Prop {
    match v.get("value") { Some(value) => Prop::Value(value.as_i64().unwrap()), None => Prop::Error(failure_json(&v["error"]).unwrap()) }
}

fn info_json(v: &Value) -> Info {
    Info { source: asbd_json(&v["source"]), client: asbd_json(&v["client"]), length: prop_json(&v["length"]), bitrate: prop_json(&v["bitrate"]) }
}

pub fn stream_json(v: &Value) -> Stream {
    Stream {
        frames: v["frames"].as_u64().unwrap(),
        reads: v["reads"].as_u64().unwrap(),
        short: v["short"].as_u64().unwrap(),
        lens_sha256: v["lens_sha256"].as_str().unwrap().to_owned(),
        error: failure_json(&v["error"]),
        sha256: v["sha256"].as_str().unwrap().to_owned(),
        tails: v["tails"].as_array().unwrap().iter()
            .map(|t| (t[0].as_u64().unwrap() as usize, t[1].as_u64().unwrap() as usize, t[2].as_str().unwrap().to_owned())).collect(),
    }
}

impl Fixtures {
    pub fn load() -> &'static Fixtures {
        static FIXTURES: LazyLock<Fixtures> = LazyLock::new(Fixtures::build);
        &FIXTURES
    }
    fn build() -> Fixtures {
        let dir = fixture_dir();
        let manifest: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json")).expect("read manifest.json")).expect("parse manifest.json");
        assert_eq!(manifest["schema"], "platinum-aac-decode-fixtures-v1");
        let parts = manifest["pack"]["parts"].as_array().unwrap().iter()
            .map(|p| fs::read(dir.join(p["file"].as_str().unwrap())).expect("read pack part")).collect();
        let files = manifest["files"].as_array().unwrap().iter().map(|f| {
            let ok = f["open"]["ok"].as_bool().unwrap();
            let open_error = if ok { None } else { failure_json(&f["open"]) };
            let variants = f.get("rate_variants").and_then(Value::as_array).map_or_else(Vec::new, |vs| vs.iter().map(|v| {
                let ok = v["open"]["ok"].as_bool().unwrap();
                Variant {
                    client_rate: v["client_rate"].as_f64().unwrap(),
                    expect: Expect::parse(v["expect"].as_str().unwrap()),
                    open_error: if ok { None } else { failure_json(&v["open"]) },
                    info: ok.then(|| info_json(&v["info"])),
                    planar512: ok.then(|| stream_json(&v["planar512"])),
                    inter512: ok.then(|| stream_json(&v["inter512"])),
                }
            }).collect());
            Fixture {
                id: f["id"].as_str().unwrap().to_owned(),
                name: f["name"].as_str().unwrap().to_owned(),
                family: f["family"].as_str().unwrap().to_owned(),
                part: f["part"].as_u64().unwrap() as usize,
                offset: f["offset"].as_u64().unwrap() as usize,
                length: f["length"].as_u64().unwrap() as usize,
                sha256: f["sha256"].as_str().unwrap().to_owned(),
                features: f["features"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_owned()).collect(),
                expect: Expect::parse(f["expect"].as_str().unwrap()),
                reason: f["expect_reason"].as_str().unwrap().to_owned(),
                open_error,
                info: ok.then(|| info_json(&f["info"])),
                planar512: ok.then(|| stream_json(&f["planar512"])),
                inter512: ok.then(|| stream_json(&f["inter512"])),
                inter_after_planar_refusal: f.get("inter_after_planar_refusal").map(stream_json),
                after_failure: f.get("after_failure").filter(|a| !a.is_null()).map(|a| AfterFailure {
                    first_status: a["first_status"].as_i64().unwrap() as i32,
                    later: a["later"].as_array().unwrap().iter().map(|r| (r[0].as_i64().unwrap() as i32, r[1].as_u64().unwrap() as u32)).collect(),
                }),
                variants,
            }
        }).collect();
        Fixtures { manifest, parts, files }
    }
    pub fn bytes(&self, fixture: &Fixture) -> &[u8] { &self.parts[fixture.part][fixture.offset..fixture.offset + fixture.length] }
    pub fn get(&self, id: &str) -> &Fixture { self.files.iter().find(|f| f.id == id).unwrap_or_else(|| panic!("no fixture {id}")) }
    pub fn index_of(&self, id: &str) -> usize { self.files.iter().position(|f| f.id == id).unwrap_or_else(|| panic!("no fixture {id}")) }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A private temporary directory, removed on drop. Every file goes into a directory of its own (the replay name is part of
/// the input: AudioFile looks at the extension), so equal names never collide.
pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("platinum-aac-decode-{}-{tag}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).expect("create scratch directory");
        Self(dir)
    }
    pub fn write(&self, slot: usize, name: &str, bytes: &[u8]) -> PathBuf {
        let dir = self.0.join(format!("{slot:05}"));
        fs::create_dir_all(&dir).expect("create scratch slot");
        let path = dir.join(name);
        fs::write(&path, bytes).expect("write scratch file");
        path
    }
}
impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

pub fn open(path: &Path) -> Result<Reader, CodecError> { Reader::open(path, None) }

/// Write the fixture under its replay name and open it as the engine does.
pub fn open_fixture(scratch: &Scratch, fixtures: &Fixtures, index: usize, fixture: &Fixture) -> Result<Reader, CodecError> {
    open(&scratch.write(index, &fixture.name, fixtures.bytes(fixture)))
}

pub fn describe(result: &Result<Reader, CodecError>) -> String {
    match result { Ok(_) => "opened".to_owned(), Err(e) => format!("{e} [{e:?}]") }
}

fn tail_planar(left: &[f32], right: &[f32], n: usize) -> Tail {
    let (mut written, mut hash) = (0usize, FNV_START);
    for i in n..left.len() { if left[i].to_bits() != SENTINEL.to_bits() || right[i].to_bits() != SENTINEL.to_bits() { written = i + 1 - n; } }
    for i in n..n + written { hash = fold(hash, left[i].to_bits()); hash = fold(hash, right[i].to_bits()); }
    (n, written, format!("{hash:016x}"))
}

fn tail_inter(buffer: &[f32], channels: usize, n: usize) -> Tail {
    let (mut written, mut hash) = (0usize, FNV_START);
    for i in n * channels..buffer.len() { if buffer[i].to_bits() != SENTINEL.to_bits() { written = (i + channels) / channels - n; } }
    for v in &buffer[n * channels..(n + written) * channels] { hash = fold(hash, v.to_bits()); }
    (n, written, format!("{hash:016x}"))
}

fn empty_stream(error: Option<Failure>) -> Stream {
    let empty = sha_hex(&[]);
    Stream { frames: 0, reads: 0, short: 0, lens_sha256: empty.clone(), error, sha256: empty, tails: Vec::new() }
}

/// What the reference harness (`decode_probe record`, planar shape) records: the client format set to planar stereo at
/// `client_rate`, then reads of `block` frames into sentinel-filled planes until a read returns 0 or fails (a short read is not
/// the end: the next read must return 0); the frames of a failed read are not delivered. The reader is left behind for probing.
pub fn decode_planar_block(reader: &mut Reader, client_rate: f64, block: usize) -> Stream {
    if let Err(e) = reader.set_stereo_planar(client_rate) { return empty_stream(Some(failure(&e))); }
    let mono = reader.source_format().channels_per_frame == 1;
    let (mut left, mut right) = (vec![SENTINEL; block], vec![SENTINEL; block]);
    let (mut all, mut lens) = (Sha256::new(), Sha256::new());
    let mut stream = empty_stream(None);
    let mut bytes = Vec::with_capacity(block * 8);
    loop {
        left.fill(SENTINEL);
        right.fill(SENTINEL);
        match reader.read_stereo_planar(&mut left, &mut right) {
            Ok(n) => {
                assert!(n <= block, "a read of {block} frames returned {n}");
                if n < block { stream.tails.push(tail_planar(&left, &right, n)); }
                if n == 0 { break; }
                bytes.clear();
                for i in 0..n {
                    if mono { assert_eq!(left[i].to_bits(), right[i].to_bits(), "mono must be duplicated into both planes"); }
                    bytes.extend_from_slice(&left[i].to_le_bytes());
                    bytes.extend_from_slice(&right[i].to_le_bytes());
                }
                all.update(&bytes);
                lens.update(&(n as u32).to_le_bytes());
                stream.reads += 1;
                stream.short += u64::from(n < block);
                stream.frames += n as u64;
            }
            Err(e) => { stream.error = Some(failure(&e)); break; }
        }
    }
    stream.sha256 = hex(&all.finalize());
    stream.lens_sha256 = hex(&lens.finalize());
    stream
}

/// The same for `decode_probe record`'s interleaved shape: at the source channel count.
pub fn decode_interleaved_block(reader: &mut Reader, block: usize) -> Stream {
    let channels = reader.source_format().channels_per_frame as usize;
    let mut buffer = vec![SENTINEL; block * channels];
    let (mut all, mut lens) = (Sha256::new(), Sha256::new());
    let mut stream = empty_stream(None);
    let mut bytes = Vec::with_capacity(block * channels * 4);
    loop {
        buffer.fill(SENTINEL);
        match reader.read(&mut buffer) {
            Ok(n) => {
                assert!(n <= block, "a read of {block} frames returned {n}");
                if n < block { stream.tails.push(tail_inter(&buffer, channels, n)); }
                if n == 0 { break; }
                bytes.clear();
                for sample in &buffer[..n * channels] { bytes.extend_from_slice(&sample.to_le_bytes()); }
                all.update(&bytes);
                lens.update(&(n as u32).to_le_bytes());
                stream.reads += 1;
                stream.short += u64::from(n < block);
                stream.frames += n as u64;
            }
            Err(e) => { stream.error = Some(failure(&e)); break; }
        }
    }
    stream.sha256 = hex(&all.finalize());
    stream.lens_sha256 = hex(&lens.finalize());
    stream
}

pub fn decode_planar(reader: &mut Reader) -> Stream {
    let rate = reader.source_format().sample_rate;
    decode_planar_block(reader, rate, BLOCK)
}
pub fn decode_interleaved(reader: &mut Reader) -> Stream { decode_interleaved_block(reader, BLOCK) }

/// What differs between a reader's description of the file and the recorded one (empty when nothing does).
pub fn info_diffs(reader: &Reader, info: &Info) -> Vec<String> {
    let mut out = Vec::new();
    if reader.source_format().sample_rate != info.source.sample_rate
        || (reader.source_format().format_id, reader.source_format().format_flags, reader.source_format().bytes_per_packet, reader.source_format().frames_per_packet,
            reader.source_format().bytes_per_frame, reader.source_format().channels_per_frame, reader.source_format().bits_per_channel, reader.source_format().reserved)
            != (info.source.format_id, info.source.format_flags, info.source.bytes_per_packet, info.source.frames_per_packet, info.source.bytes_per_frame,
                info.source.channels, info.source.bits, info.source.reserved) {
        out.push(format!("source format: expected {:?}, got {:?}", info.source, reader.source_format()));
    }
    let c = reader.client_format();
    if c.sample_rate != info.client.sample_rate
        || (c.format_id, c.format_flags, c.bytes_per_packet, c.frames_per_packet, c.bytes_per_frame, c.channels_per_frame, c.bits_per_channel)
            != (info.client.format_id, info.client.format_flags, info.client.bytes_per_packet, info.client.frames_per_packet, info.client.bytes_per_frame, info.client.channels, info.client.bits) {
        out.push(format!("client format: expected {:?}, got {c:?}", info.client));
    }
    let length = match reader.file_length_frames() { Ok(v) => Prop::Value(v), Err(e) => Prop::Error(failure(&e)) };
    if length != info.length { out.push(format!("file length: expected {:?}, got {length:?}", info.length)); }
    let bitrate = match reader.bitrate() { Ok(v) => Prop::Value(i64::from(v)), Err(e) => Prop::Error(failure(&e)) };
    if bitrate != info.bitrate { out.push(format!("bitrate: expected {:?}, got {bitrate:?}", info.bitrate)); }
    out
}

/// Apple's results at read sizes other than 512 (`readsizes.json`, recorded by `make_decode_fixtures.py readsizes`).
pub struct ReadSizeFile {
    pub id: String,
    pub sha256: String,
    /// `(block frames, result)` for planar stereo reads.
    pub planar: Vec<(usize, Stream)>,
    /// The same for interleaved reads at the source channel count.
    pub inter: Vec<(usize, Stream)>,
}
pub struct ReadSizes { pub document: Value, pub files: Vec<ReadSizeFile> }
impl ReadSizes {
    pub fn load() -> &'static ReadSizes {
        static SIZES: LazyLock<ReadSizes> = LazyLock::new(ReadSizes::build);
        &SIZES
    }
    fn build() -> ReadSizes {
        let document: Value = serde_json::from_slice(&fs::read(fixture_dir().join("readsizes.json")).expect("read readsizes.json")).expect("parse readsizes.json");
        assert_eq!(document["schema"], "platinum-aac-decode-readsizes-v1");
        let rows = |v: &Value| -> Vec<(usize, Stream)> { v.as_object().unwrap().iter().map(|(block, r)| (block.parse().unwrap(), stream_json(r))).collect() };
        let files = document["files"].as_array().unwrap().iter().map(|f| ReadSizeFile {
            id: f["id"].as_str().unwrap().to_owned(), sha256: f["sha256"].as_str().unwrap().to_owned(), planar: rows(&f["planar"]), inter: rows(&f["inter"]),
        }).collect();
        ReadSizes { document, files }
    }
}

type Decoded = Arc<Result<(Stream, Stream), String>>;
static DECODED: LazyLock<Mutex<HashMap<(usize, usize, u64), Arc<OnceLock<Decoded>>>>> = LazyLock::new(Default::default);

/// The planar and the interleaved stream of a fixture at `block`-frame reads and `client_rate` (`None`: the file's own),
/// decoded once per process whichever test asks first (several tests compare different parts of the same recording).
pub fn decoded(fixtures: &Fixtures, index: usize, block: usize, client_rate: Option<f64>) -> Decoded {
    let key = (index, block, client_rate.map_or(0, f64::to_bits));
    let cell = DECODED.lock().unwrap().entry(key).or_default().clone();
    cell.get_or_init(|| {
        let fixture = &fixtures.files[index];
        let scratch = Scratch::new("decoded");
        let path = scratch.write(index, &fixture.name, fixtures.bytes(fixture));
        let open = || Reader::open(&path, client_rate).map_err(|e| format!("open: {}", describe(&Err(e))));
        let result = (|| {
            let mut planar_reader = open()?;
            let rate = client_rate.unwrap_or(planar_reader.source_format().sample_rate);
            let planar = decode_planar_block(&mut planar_reader, rate, block);
            let mut inter_reader = open()?;
            let inter = decode_interleaved_block(&mut inter_reader, block);
            Ok((planar, inter))
        })();
        Arc::new(result)
    }).clone()
}
