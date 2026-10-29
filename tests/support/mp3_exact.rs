//! Shared by `portable_mp3.rs` and `portable_mp3_hostile.rs`: the golden MP3 fixture pack, a scratch
//! directory, and the two reference decode loops.
//!
//! `tests/fixtures/mp3-exact/` holds generated MP3 inputs (`pack.bin`) and, per input, what the engine's
//! macOS `apple_codec::Reader` (AudioToolbox `ExtAudioFileRead`) delivered for it (`manifest.json`): open
//! outcome, source properties, and the SHA-256 of the float32 stream of 512-frame reads in the two shapes the
//! engine uses (planar stereo, and interleaved at the source channel count). Nothing here needs Apple at run
//! time. See `tests/fixtures/mp3-exact/README.md` for provenance and how the pack is rebuilt.
#![allow(dead_code)]

use platinum_engine::portable::audio::{CodecError, Reader};
use platinum_engine::portable::digest::Sha256;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The block size of every engine read, and the contract of the port.
pub const BLOCK: usize = 512;

pub fn fixture_dir() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mp3-exact") }

pub fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure { pub operation: String, pub status: i32 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info { pub sample_rate: u32, pub format_id: u32, pub frames_per_packet: u32, pub channels: u32, pub length_frames: i64, pub bitrate: u32 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream { pub frames: u64, pub error: Option<Failure>, pub sha256: String }

pub struct Fixture {
    pub id: String,
    pub family: String,
    pub offset: usize,
    pub length: usize,
    pub sha256: String,
    pub features: Vec<String>,
    /// `Some` when Apple refused the file at open.
    pub open_error: Option<Failure>,
    pub info: Option<Info>,
    pub planar512: Option<Stream>,
    pub inter512: Option<Stream>,
}
impl Fixture {
    pub fn opens(&self) -> bool { self.open_error.is_none() }
    pub fn has(&self, feature: &str) -> bool { self.features.iter().any(|f| f == feature) }
}

pub struct Fixtures {
    pub manifest: Value,
    pub pack: Vec<u8>,
    pub files: Vec<Fixture>,
}
impl Fixtures {
    pub fn load() -> &'static Fixtures {
        static FIXTURES: LazyLock<Fixtures> = LazyLock::new(Fixtures::build);
        &FIXTURES
    }
    fn build() -> Fixtures {
        let dir = fixture_dir();
        let manifest: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json")).expect("read manifest.json")).expect("parse manifest.json");
        assert_eq!(manifest["schema"], "platinum-mp3-exact-fixtures-v1");
        let pack = fs::read(dir.join(manifest["pack"]["file"].as_str().unwrap())).expect("read pack");
        let failure = |v: &Value| -> Option<Failure> {
            if v.is_null() { None } else { Some(Failure { operation: v["operation"].as_str().unwrap().to_owned(), status: v["status"].as_i64().unwrap() as i32 }) }
        };
        let stream = |v: &Value| -> Option<Stream> {
            if v.is_null() { None } else { Some(Stream { frames: v["frames"].as_u64().unwrap(), error: failure(&v["error"]), sha256: v["sha256"].as_str().unwrap().to_owned() }) }
        };
        let files = manifest["files"].as_array().unwrap().iter().map(|f| {
            let ok = f["open"]["ok"].as_bool().unwrap();
            Fixture {
                id: f["id"].as_str().unwrap().to_owned(),
                family: f["family"].as_str().unwrap().to_owned(),
                offset: f["offset"].as_u64().unwrap() as usize,
                length: f["length"].as_u64().unwrap() as usize,
                sha256: f["sha256"].as_str().unwrap().to_owned(),
                features: f["features"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_owned()).collect(),
                open_error: if ok { None } else { failure(&f["open"]) },
                info: ok.then(|| {
                    let i = &f["info"];
                    let n = |k: &str| i[k].as_u64().unwrap() as u32;
                    Info { sample_rate: n("sample_rate"), format_id: n("format_id"), frames_per_packet: n("frames_per_packet"),
                        channels: n("channels"), length_frames: i["length_frames"].as_i64().unwrap(), bitrate: n("bitrate") }
                }),
                planar512: if ok { stream(&f["planar512"]) } else { None },
                inter512: if ok { stream(&f["inter512"]) } else { None },
            }
        }).collect();
        Fixtures { manifest, pack, files }
    }
    pub fn bytes(&self, fixture: &Fixture) -> &[u8] { &self.pack[fixture.offset..fixture.offset + fixture.length] }
    pub fn get(&self, id: &str) -> &Fixture { self.files.iter().find(|f| f.id == id).unwrap_or_else(|| panic!("no fixture {id}")) }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A private temporary directory, removed on drop.
pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("platinum-mp3-exact-{}-{tag}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).expect("create scratch directory");
        Self(dir)
    }
    pub fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).expect("write scratch file");
        path
    }
}
impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

/// The native failure a `CodecError` stands for; every MP3 open or read failure is one.
pub fn failure(error: CodecError) -> Failure {
    match error {
        CodecError::Native { operation, status } => Failure { operation: operation.to_owned(), status },
        other => panic!("expected a reproduced native failure, got {other:?}"),
    }
}

/// What Apple's harness (`apple_probe dump ... planar BLOCK`) records: reads of `block` frames until a read returns 0 or
/// fails (a short read is not the end: the next read must return 0); the frames of a failed read are not delivered.
/// The reader is left behind for further probing.
pub fn decode_planar_block(reader: &mut Reader, block: usize) -> Stream {
    let rate = reader.source_format().sample_rate;
    reader.set_stereo_planar(rate).expect("same-rate planar client format");
    let mono = reader.source_format().channels_per_frame == 1;
    let (mut left, mut right) = (vec![0f32; block], vec![0f32; block]);
    let mut hash = Sha256::new();
    let (mut frames, mut error) = (0u64, None);
    let mut bytes = Vec::with_capacity(block * 8);
    loop {
        left.fill(0.0);
        right.fill(0.0);
        match reader.read_stereo_planar(&mut left, &mut right) {
            Ok(0) => break,
            Ok(n) => {
                bytes.clear();
                for i in 0..n {
                    if mono { assert_eq!(left[i].to_bits(), right[i].to_bits(), "mono must be duplicated into both planes"); }
                    bytes.extend_from_slice(&left[i].to_le_bytes());
                    bytes.extend_from_slice(&right[i].to_le_bytes());
                }
                hash.update(&bytes);
                frames += n as u64;
            }
            Err(e) => { error = Some(failure(e)); break; }
        }
    }
    Stream { frames, error, sha256: hex(&hash.finalize()) }
}

/// The same for `apple_probe dump ... inter BLOCK`: interleaved at the source channel count.
pub fn decode_interleaved_block(reader: &mut Reader, block: usize) -> Stream {
    let channels = reader.source_format().channels_per_frame as usize;
    let mut buffer = vec![0f32; block * channels];
    let mut hash = Sha256::new();
    let (mut frames, mut error) = (0u64, None);
    let mut bytes = Vec::with_capacity(block * channels * 4);
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                bytes.clear();
                for sample in &buffer[..n * channels] { bytes.extend_from_slice(&sample.to_le_bytes()); }
                hash.update(&bytes);
                frames += n as u64;
            }
            Err(e) => { error = Some(failure(e)); break; }
        }
    }
    Stream { frames, error, sha256: hex(&hash.finalize()) }
}

pub fn decode_planar(reader: &mut Reader) -> Stream { decode_planar_block(reader, BLOCK) }
pub fn decode_interleaved(reader: &mut Reader) -> Stream { decode_interleaved_block(reader, BLOCK) }

pub fn open(path: &Path) -> Result<Reader, CodecError> { Reader::open(path, None) }

/// Apple's results at read sizes other than 512 (`readsizes.json`, recorded by `tools/portable/mp3/record_readsizes.py`).
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
        assert_eq!(document["schema"], "platinum-mp3-exact-readsizes-v1");
        let stream = |v: &Value| -> Stream {
            let error = if v["error"].is_null() { None }
                else { Some(Failure { operation: v["error"]["operation"].as_str().unwrap().to_owned(), status: v["error"]["status"].as_i64().unwrap() as i32 }) };
            Stream { frames: v["frames"].as_u64().unwrap(), error, sha256: v["sha256"].as_str().unwrap().to_owned() }
        };
        let rows = |v: &Value| -> Vec<(usize, Stream)> {
            v.as_object().unwrap().iter().map(|(block, r)| (block.parse().unwrap(), stream(r))).collect()
        };
        let files = document["files"].as_array().unwrap().iter().map(|f| ReadSizeFile {
            id: f["id"].as_str().unwrap().to_owned(), sha256: f["sha256"].as_str().unwrap().to_owned(),
            planar: rows(&f["planar"]), inter: rows(&f["inter"]),
        }).collect();
        ReadSizes { document, files }
    }
}
