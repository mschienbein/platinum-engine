//! End-to-end CLI tests of the v0.2.0 opt-in `parity.allow_non_identical` and of
//! the Linux-only failure codes (`unsupported_input_codec`,
//! `unsupported_output_codec`, `unsupported_sample_rate_conversion`,
//! `artwork_conversion_unavailable`; see `tests/error_codes.rs`, whose coverage
//! ledger points here). Every test runs the real `platinum` binary against
//! generated audio and generated images in an isolated HOME and state
//! directory, and parses the JSON outcome.
//!
//! Every test is `#[ignore]`d: it needs the libraries of the image under test,
//! passed as `PLATINUM_TEST_TAG_BRIDGE`, `PN_FLAC_LIB`/`PN_FLAC_SHA256` and
//! `PN_LAME_LIB`/`PN_LAME_SHA256`. Run with `cargo test --test linux_opt_in --
//! --ignored` on Linux, or with `--features portable` on another host.
//!
//! # PN_LINUX_OPT_IN_NOTES
//!
//! Which scenario depends on which piece of v0.2.0 work (a failure of a scenario
//! points at that owner first). "Decoder" is the portable MP3 decoder behind
//! `Reader::open_with(.., ReaderOptions { non_identical_mp3 })`; "art bridge" is
//! the portable tag bridge exporting the ABI-5 artwork extension
//! (`pn_tag_bridge_features`, `pn_tag_copy_{direct,generic}_ex`).
//!
//! | Scenario | Depends on |
//! |---|---|
//! | A `mp3_is_refused_without_the_opt_in_with_the_v0_1_0_message` | engine/settings wiring only (the refusal exists without the decoder); the fixture MP3 comes from the LAME encoder |
//! | B `mp3_with_opt_in` | decoder, engine `non_identical_paths` reporting, LAME |
//! | C `identical_paths_stay_identical_with_the_opt_in_on` | engine/settings wiring, libFLAC |
//! | D1 `unsupported_sample_rate_conversion_for_a_22_05_khz_wav` | engine analysis pre-check (the portable reader refuses sample-rate conversion) |
//! | D2 `unsupported_sample_rate_conversion_for_a_24_khz_mp3` | decoder: opens at the native rate, refuses a 44.1 kHz client rate |
//! | D3 `unsupported_output_format_for_alac_beyond_96_khz` | portable ALAC writer limits (22,050–96,000 Hz, 1–8 channels); output-side code override in `error_code::classify` |
//! | E `corrupt_mp3_with_the_opt_in_is_an_invalid_input_file` | decoder: header scan at open, `InvalidInput` at the first read of damaged frame data |
//! | E2 `a_truncated_mp3_is_refused_by_the_container_check_before_any_decoder_runs` | art bridge / tag bridge refusing "invalid or truncated MPEG audio" (status 3), engine container sniffing |
//! | F `aac_is_always_refused` | portable codec layer keeping AAC (decode and encode) unsupported in v0.2.0; tag bridge parsing the hand-built `.m4a` |
//! | G1, G2 `artwork_flac_png_to_*` | art bridge, engine `TransferOptions`, libFLAC (and LAME for G2) |
//! | G3 `artwork_flac_webp_to_alac_is_refused_but_png_is_copied` | art bridge (MP4 `covr` rules), libFLAC |
//! | G4 `artwork_mp3_apic_is_moved_unchanged_and_reports_only_mp3_decode` | decoder, the bridge's ID3 direct copy, LAME |
//! | G5 `sources_without_artwork_report_no_artwork_copy` | engine reporting, art bridge accepting pictureless sources |
//! | G6 `wav_tag_chunks_are_kept_with_the_opt_in_and_dropped_without` | `src/wav_preserve.rs` (Rust marker rewrite), engine `TransferOptions` and `non_identical_paths`; works with any bridge |
//! | G7 `cross_format_copies_into_a_wav_keep_the_chunk_taglib_wrote` | the same rewrite for generic copies into a WAV (tempo and key are always written) |
//! | H `failed_items_have_no_report` | engine/CLI JSON assembly |
#![cfg(any(target_os = "linux", feature = "portable"))]

use platinum_engine::mp3::file_sha256;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Generated audio is 16-bit stereo at 44.1 kHz: no sample-rate conversion on any host.
const RATE: u32 = 44_100;
/// Generated audio is short: processing is the cost of every scenario.
const SECONDS: u32 = 1;
/// The message every release before the MP3 opt-in gave; it stays the default.
const MP3_REFUSAL: &str = "MP3 decoding is unavailable: Apple's decoder output is not reproducible";

struct Sandbox { root: PathBuf, home: PathBuf, state: PathBuf }
impl Sandbox {
    fn new() -> Self {
        let root = loop {
            let candidate = std::env::temp_dir().join(format!("platinum-linux-opt-in-{}-{}", std::process::id(), NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated test directory: {error}"),
            }
        };
        // Reported input paths are canonical; resolve symlinked temp roots up front.
        let root = fs::canonicalize(root).unwrap();
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        let state = root.join("state");
        Self { root, home, state }
    }
    /// A sandbox whose settings name the image's tag bridge, FLAC and LAME, with
    /// the opt-in at its default (off).
    fn configured() -> Self {
        let sandbox = Self::new();
        for (field, variable) in [
            ("libraries.tag_bridge", "PLATINUM_TEST_TAG_BRIDGE"),
            ("libraries.flac", "PN_FLAC_LIB"), ("libraries.flac_sha256", "PN_FLAC_SHA256"),
            ("libraries.lame", "PN_LAME_LIB"), ("libraries.lame_sha256", "PN_LAME_SHA256"),
        ] {
            let value = std::env::var(variable).unwrap_or_else(|_| panic!("set {variable} (see the #[ignore] reason)"));
            sandbox.ok(&["config", "set", field, &json!(value).to_string()]);
        }
        sandbox
    }
    /// Persistently set `parity.allow_non_identical`, as a user would.
    fn set_opt_in(&self, on: bool) {
        let outcome = self.ok(&["config", "set", "parity.allow_non_identical", if on { "true" } else { "false" }]);
        assert_eq!(outcome["data"]["value"], on, "{outcome}");
    }
    fn run(&self, args: &[&str]) -> (Output, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_platinum"))
            .env("HOME", &self.home).arg("--state-dir").arg(&self.state).arg("--json").args(args)
            .output().unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!("invalid JSON outcome for {args:?}: {error}; stdout={:?}; stderr={:?}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
        });
        (output, value)
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (output, value) = self.run(args);
        assert!(output.status.success(), "{args:?}: {value}; stderr={}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(value["status"], "ok", "{args:?}: {value}");
        value
    }
    /// A run that must exit with `exit`; returns the parsed outcome.
    fn fails(&self, exit: i32, args: &[&str]) -> Value {
        let (output, value) = self.run(args);
        assert_eq!(output.status.code(), Some(exit), "{args:?}: {value}; stderr={}", String::from_utf8_lossy(&output.stderr));
        value
    }
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() { fs::create_dir_all(parent).unwrap(); }
        fs::write(&path, bytes).unwrap();
        path
    }
    /// The engine's own output for a generated WAV: runs the CLI, returns the published file.
    fn engine_output(&self, name: &str, format: &str) -> PathBuf {
        let wav = self.file(&format!("fixtures/{name}.wav"), &generated_wav(RATE, SECONDS, false));
        let out = self.dir(&format!("fixtures/{name}-{format}"));
        let outcome = self.ok(&["process", text(&wav), "--format", format, "--output-dir", text(&out)]);
        PathBuf::from(report(&outcome, 0)["output_path"].as_str().unwrap())
    }
    /// A plain MP3 with no ID3 tag and no processed marker: LAME output with its tags stripped.
    fn plain_mp3(&self, name: &str) -> PathBuf {
        let produced = fs::read(self.engine_output(name, "mp3")).unwrap();
        self.file(&format!("inputs/{name}.mp3"), &strip_id3(&produced))
    }
    /// A FLAC with only STREAMINFO (so no marker and no tags) and, optionally, one picture.
    fn flac(&self, name: &str, picture: Option<&FlacPicture>) -> PathBuf {
        let produced = fs::read(self.engine_output(name, "flac")).unwrap();
        self.file(&format!("inputs/{name}.flac"), &flac_rewritten(&produced, picture))
    }
}
impl Drop for Sandbox { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }

fn text(path: &Path) -> &str { path.to_str().unwrap() }

// --- JSON outcome helpers -----------------------------------------------------

fn report(outcome: &Value, index: usize) -> &Value {
    let report = &outcome["results"][index]["report"];
    assert!(report.is_object(), "results[{index}] has no report: {outcome}");
    report
}

/// `report.non_identical_paths`: always present, empty when every path used is identical.
fn paths(report: &Value) -> Vec<&str> {
    report["non_identical_paths"].as_array().unwrap_or_else(|| panic!("non_identical_paths is always present: {report}"))
        .iter().map(|path| path.as_str().unwrap()).collect()
}

/// A failed item: status, code (and the stage where it is stable), a message, and no report.
fn assert_failed_item(outcome: &Value, index: usize, stage: Option<&str>, code: &str) {
    let result = &outcome["results"][index];
    assert_eq!(result["status"], "failed", "results[{index}]: {outcome}");
    let error = &result["error"];
    assert_eq!(error["code"], code, "results[{index}]: {outcome}");
    if let Some(stage) = stage { assert_eq!(error["stage"], stage, "results[{index}]: {outcome}"); }
    assert!(error["message"].as_str().is_some_and(|message| !message.is_empty()), "results[{index}] has no message: {outcome}");
    assert!(result.get("report").is_none(), "a failed item has no report: {outcome}");
    assert!(result.get("statistics").is_none(), "a failed item has no statistics: {outcome}");
}

/// The result for one input path, whatever its position in `results`.
fn result_for<'a>(outcome: &'a Value, input: &Path) -> &'a Value {
    outcome["results"].as_array().unwrap().iter().find(|result| result["input"] == text(input))
        .unwrap_or_else(|| panic!("no result for {}: {outcome}", input.display()))
}

fn assert_no_files(directory: &Path) {
    let names: Vec<_> = fs::read_dir(directory).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert!(names.is_empty(), "nothing may be published or left behind in {}: {names:?}", directory.display());
}

// --- Generated audio ------------------------------------------------------------

/// A 16-bit stereo PCM WAV: a 440 Hz sine, or digital silence.
fn generated_wav(rate: u32, seconds: u32, silent: bool) -> Vec<u8> {
    let frames = rate * seconds;
    let data_len = frames * 4;
    let mut wav = Vec::with_capacity((44 + data_len) as usize);
    wav.extend_from_slice(b"RIFF"); wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt "); wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes()); wav.extend_from_slice(&(rate * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes()); wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data"); wav.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let sample = if silent { 0 } else {
            ((frame as f32 * 2.0 * std::f32::consts::PI * 440.0 / rate as f32).sin() * 6000.0) as i16
        };
        wav.extend_from_slice(&sample.to_le_bytes());
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

fn synchsafe(bytes: &[u8]) -> usize { bytes.iter().fold(0, |value, byte| (value << 7) | usize::from(byte & 0x7f)) }

/// Total size of the ID3v2 tags (header, body, footer) that lead `data`.
fn id3v2_prefix_len(data: &[u8]) -> usize {
    let mut length = 0;
    while data.len() - length >= 10 && &data[length..length + 3] == b"ID3" {
        let footer = data[length + 3] == 4 && data[length + 5] & 0x10 != 0;
        length += 10 + synchsafe(&data[length + 6..length + 10]) + if footer { 10 } else { 0 };
    }
    length
}

/// Remove every leading ID3v2 tag and a trailing ID3v1 tag, leaving bare MPEG
/// frames: no tag, so no processed marker.
fn strip_id3(mp3: &[u8]) -> Vec<u8> {
    let mut audio = &mp3[id3v2_prefix_len(mp3)..];
    if audio.len() >= 128 && &audio[audio.len() - 128..audio.len() - 125] == b"TAG" { audio = &audio[..audio.len() - 128]; }
    assert!(audio.len() > 4 && audio[0] == 0xff && audio[1] & 0xe0 == 0xe0, "stripped MP3 must start with an MPEG sync, starts {:02x?}", &audio[..4.min(audio.len())]);
    audio.to_vec()
}

/// `count` MPEG frames of `frame_len` bytes: `header`, then a body that `fill` writes.
fn mpeg_frames(header: [u8; 4], frame_len: usize, count: usize, mut fill: impl FnMut(usize, &mut [u8])) -> Vec<u8> {
    let mut frames = Vec::with_capacity(frame_len * count);
    for index in 0..count {
        let start = frames.len();
        frames.extend_from_slice(&header);
        frames.resize(start + frame_len, 0);
        fill(index, &mut frames[start + 4..]);
    }
    frames
}

/// MPEG-2 Layer III, 24 kHz, 64 kbit/s, stereo: 24 frames of zero side information
/// and payload, which decode as silence. 72 * 64000 / 24000 = 192 bytes per frame.
fn mpeg2_24khz_silence() -> Vec<u8> {
    mpeg_frames([0xff, 0xf3, 0x84, 0x00], 192, 24, |_, _| {})
}

/// Eight complete MPEG-1 Layer III 44.1 kHz 128 kbit/s frames (417 bytes each) whose
/// first granule claims 4095 bits of part 2/3 data while the frame holds only 3048:
/// the header scan at open succeeds and the first read finds damaged frame data.
fn damaged_mp3() -> Vec<u8> {
    mpeg_frames([0xff, 0xfb, 0x90, 0x00], 417, 8, |_, body| body[..4].copy_from_slice(&[0x00, 0x00, 0x0f, 0xff]))
}

/// A valid MPEG-1 Layer III 44.1 kHz 128 kbit/s header, then only 100 body bytes:
/// the stream holds no complete (417-byte) frame.
fn truncated_mp3() -> Vec<u8> {
    mpeg_frames([0xff, 0xfb, 0x90, 0x00], 104, 1, |_, _| {})
}

/// A minimal MP4 (ftyp, moov > trak > mdia > minf > stbl > stsd with an `mp4a`
/// entry, mdat) of the kind the sniffer and the portable reader recognise.
fn aac_m4a() -> Vec<u8> {
    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut atom = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        atom.extend_from_slice(kind);
        atom.extend_from_slice(body);
        atom
    }
    let mut entry = vec![0u8; 6];
    entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    entry.extend_from_slice(&[0u8; 8]); // version, revision, vendor
    entry.extend_from_slice(&2u16.to_be_bytes()); // channels
    entry.extend_from_slice(&16u16.to_be_bytes()); // sample size
    entry.extend_from_slice(&[0u8; 4]); // compression id, packet size
    entry.extend_from_slice(&(RATE << 16).to_be_bytes()); // 16.16 sample rate
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1]; // version/flags, one entry
    stsd.extend(atom(b"mp4a", &entry));
    let stbl = atom(b"stbl", &atom(b"stsd", &stsd));
    let minf = atom(b"minf", &stbl);
    let mut mdhd = vec![0u8; 12]; // version/flags, creation, modification
    mdhd.extend_from_slice(&RATE.to_be_bytes()); // time scale
    mdhd.extend_from_slice(&(RATE * 2).to_be_bytes()); // duration
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und", quality
    let mut hdlr = vec![0u8; 8]; // version/flags, pre-defined
    hdlr.extend_from_slice(b"soun");
    hdlr.extend_from_slice(&[0u8; 13]); // reserved, empty name
    let mut mdia = atom(b"mdhd", &mdhd);
    mdia.extend(atom(b"hdlr", &hdlr));
    mdia.extend(minf);
    let moov = atom(b"moov", &atom(b"trak", &atom(b"mdia", &mdia)));
    let mut ftyp = b"M4A ".to_vec();
    ftyp.extend_from_slice(&[0u8; 4]);
    ftyp.extend_from_slice(b"M4A mp42isom");
    let mut m4a = atom(b"ftyp", &ftyp);
    m4a.extend(moov);
    m4a.extend(atom(b"mdat", &[0u8; 64]));
    m4a
}

// --- Generated images: framing-valid and, where practical, decodable -------------

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 { crc = if crc & 1 != 0 { 0xedb8_8320 ^ (crc >> 1) } else { crc >> 1 }; }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes { a = (a + u32::from(byte)) % 65_521; b = (b + a) % 65_521; }
    (b << 16) | a
}

/// A valid 8-bit RGB PNG with correct chunk CRCs and a stored (uncompressed) zlib stream.
fn generated_png(width: u32, height: u32) -> Vec<u8> {
    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut chunk = (body.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(body);
        chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
        chunk
    }
    let mut raw = Vec::new();
    for y in 0..height {
        raw.push(0); // filter type: none
        for x in 0..width { raw.extend_from_slice(&[(x * 255 / width) as u8, (y * 255 / height) as u8, ((x ^ y) * 8) as u8]); }
    }
    let length = u16::try_from(raw.len()).expect("one stored deflate block");
    let mut zlib = vec![0x78, 0x01, 0x01];
    zlib.extend_from_slice(&length.to_le_bytes());
    zlib.extend_from_slice(&(!length).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut header = width.to_be_bytes().to_vec();
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, deflate, no filter method, no interlace
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(chunk(b"IHDR", &header));
    png.extend(chunk(b"IDAT", &zlib));
    png.extend(chunk(b"IEND", &[]));
    png
}

/// A valid baseline 8x8 grayscale JPEG (one DC and one end-of-block code) behind a
/// 2000-byte comment, so a truncated or altered copy cannot match by accident.
fn generated_jpeg() -> Vec<u8> {
    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut segment = vec![0xff, marker];
        segment.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(body);
        segment
    }
    let mut jpeg = vec![0xff, 0xd8];
    jpeg.extend(segment(0xdb, &[&[0u8][..], &[1u8; 64][..]].concat())); // quantization table
    jpeg.extend(segment(0xfe, &(0..2000u32).map(|i| (i * 31 % 251) as u8).collect::<Vec<_>>()));
    jpeg.extend(segment(0xc0, &[8, 0, 8, 0, 8, 1, 1, 0x11, 0])); // 8-bit, 8x8, one component
    for class_and_id in [0x00, 0x10] { // DC table 0, AC table 0: one code of length 1 for symbol 0
        let mut table = vec![class_and_id, 1];
        table.extend_from_slice(&[0u8; 15]);
        table.push(0);
        jpeg.extend(segment(0xc4, &table));
    }
    jpeg.extend(segment(0xda, &[1, 1, 0x00, 0, 63, 0]));
    jpeg.extend_from_slice(&[0x3f, 0xff, 0xd9]); // DC "0", EOB "0", padding; EOI
    jpeg
}

/// A WebP container whose single VP8L chunk has a valid header: framing-valid, so
/// the bridge accepts it as an image, but an MP4 `covr` atom cannot store it.
fn generated_webp() -> Vec<u8> {
    let mut chunk = vec![0x2f];
    chunk.extend_from_slice(&(15u32 | (15 << 14)).to_le_bytes()); // 16 x 16, no alpha, version 0
    chunk.extend_from_slice(&[0u8; 15]);
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&(4 + 8 + chunk.len() as u32).to_le_bytes());
    webp.extend_from_slice(b"WEBPVP8L");
    webp.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
    webp.extend_from_slice(&chunk);
    webp
}

// --- FLAC pictures ------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct FlacPicture { kind: u32, mime: String, description: String, width: u32, height: u32, depth: u32, colors: u32, data: Vec<u8> }
impl FlacPicture {
    fn front_cover(mime: &str, data: Vec<u8>, width: u32, height: u32, depth: u32) -> Self {
        Self { kind: 3, mime: mime.to_owned(), description: "Front cover".to_owned(), width, height, depth, colors: 0, data }
    }
    fn png() -> Self { Self::front_cover("image/png", generated_png(32, 32), 32, 32, 24) }
    fn webp() -> Self { Self::front_cover("image/webp", generated_webp(), 16, 16, 24) }
    fn block(&self) -> Vec<u8> {
        let mut body = self.kind.to_be_bytes().to_vec();
        body.extend_from_slice(&(self.mime.len() as u32).to_be_bytes());
        body.extend_from_slice(self.mime.as_bytes());
        body.extend_from_slice(&(self.description.len() as u32).to_be_bytes());
        body.extend_from_slice(self.description.as_bytes());
        for value in [self.width, self.height, self.depth, self.colors, self.data.len() as u32] { body.extend_from_slice(&value.to_be_bytes()); }
        body.extend_from_slice(&self.data);
        body
    }
    fn parse(body: &[u8]) -> Self {
        let word = |at: usize| u32::from_be_bytes(body[at..at + 4].try_into().unwrap());
        let mime_len = word(4) as usize;
        let mime = String::from_utf8(body[8..8 + mime_len].to_vec()).unwrap();
        let description_len = word(8 + mime_len) as usize;
        let description_at = 12 + mime_len;
        let description = String::from_utf8(body[description_at..description_at + description_len].to_vec()).unwrap();
        let geometry = description_at + description_len;
        let data_len = word(geometry + 16) as usize;
        let data = body[geometry + 20..].to_vec();
        assert_eq!(data.len(), data_len, "FLAC picture block length mismatch");
        Self { kind: word(0), mime, description, width: word(geometry), height: word(geometry + 4), depth: word(geometry + 8), colors: word(geometry + 12), data }
    }
}

/// `(block type, body)` of every metadata block, and the offset of the first audio frame.
fn flac_blocks(data: &[u8]) -> (Vec<(u8, &[u8])>, usize) {
    let start = id3v2_prefix_len(data);
    assert_eq!(&data[start..start + 4], b"fLaC", "not a FLAC stream");
    let mut position = start + 4;
    let mut blocks = Vec::new();
    loop {
        let header = data[position];
        let size = u32::from_be_bytes([0, data[position + 1], data[position + 2], data[position + 3]]) as usize;
        blocks.push((header & 0x7f, &data[position + 4..position + 4 + size]));
        position += 4 + size;
        if header & 0x80 != 0 { return (blocks, position); }
    }
}

fn flac_pictures(data: &[u8]) -> Vec<FlacPicture> {
    flac_blocks(data).0.into_iter().filter(|(kind, _)| *kind == 6).map(|(_, body)| FlacPicture::parse(body)).collect()
}

/// The same audio frames behind STREAMINFO alone (every tag, and with it the
/// processed marker, is dropped) and, when given, one PICTURE block as the last block.
fn flac_rewritten(flac: &[u8], picture: Option<&FlacPicture>) -> Vec<u8> {
    let (blocks, frames) = flac_blocks(flac);
    assert_eq!(blocks[0].0, 0, "first metadata block must be STREAMINFO");
    let mut out = b"fLaC".to_vec();
    let mut push = |kind: u8, last: bool, body: &[u8]| {
        assert!(body.len() < 1 << 24, "FLAC metadata block too large");
        out.push(kind | if last { 0x80 } else { 0 });
        out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(body);
    };
    push(0, picture.is_none(), blocks[0].1);
    if let Some(picture) = picture { push(6, true, &picture.block()); }
    out.extend_from_slice(&flac[frames..]);
    out
}

// --- ID3v2 pictures -----------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Apic { mime: String, kind: u8, description: String, data: Vec<u8> }

/// An ID3v2.3 tag holding one APIC frame (Latin-1 text, no flags).
fn id3v23_apic_tag(mime: &str, kind: u8, description: &str, data: &[u8]) -> Vec<u8> {
    let mut body = vec![0];
    body.extend_from_slice(mime.as_bytes());
    body.push(0);
    body.push(kind);
    body.extend_from_slice(description.as_bytes());
    body.push(0);
    body.extend_from_slice(data);
    let mut frame = b"APIC".to_vec();
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend(body);
    let size = frame.len();
    assert!(size < 1 << 28);
    let mut tag = vec![b'I', b'D', b'3', 3, 0, 0];
    tag.extend_from_slice(&[(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
    tag.extend(frame);
    tag
}

/// Every APIC frame of the leading ID3v2.3/2.4 tag of `data`, in order.
fn id3_pictures(data: &[u8]) -> Vec<Apic> {
    assert_eq!(&data[..3], b"ID3", "output has no leading ID3v2 tag");
    let (version, flags) = (data[3], data[5]);
    assert!(matches!(version, 3 | 4), "ID3v2.{version} is not handled by this test");
    assert_eq!(flags & 0xc0, 0, "unsynchronised or extended-header tags are not handled by this test");
    let end = 10 + synchsafe(&data[6..10]);
    let mut position = 10;
    let mut pictures = Vec::new();
    while position + 10 <= end && data[position] != 0 {
        let size_bytes: [u8; 4] = data[position + 4..position + 8].try_into().unwrap();
        let size = if version == 4 { synchsafe(&size_bytes) } else { u32::from_be_bytes(size_bytes) as usize };
        let frame_flags = &data[position + 8..position + 10];
        let body = &data[position + 10..position + 10 + size];
        let is_apic = &data[position..position + 4] == b"APIC";
        position += 10 + size;
        if !is_apic { continue; }
        assert_eq!(frame_flags, [0, 0], "APIC frame flags");
        let encoding = body[0];
        let mime_end = 1 + body[1..].iter().position(|byte| *byte == 0).expect("MIME terminator");
        let kind = body[mime_end + 1];
        let rest = &body[mime_end + 2..];
        let wide = matches!(encoding, 1 | 2);
        let terminator = if wide {
            (0..rest.len() / 2).map(|i| i * 2).find(|&i| rest[i] == 0 && rest[i + 1] == 0).expect("description terminator")
        } else {
            rest.iter().position(|byte| *byte == 0).expect("description terminator")
        };
        let description = if wide {
            let big_endian = encoding == 2 || rest.starts_with(&[0xfe, 0xff]);
            let units: Vec<u16> = rest[..terminator].chunks(2)
                .map(|pair| if big_endian { u16::from_be_bytes([pair[0], pair[1]]) } else { u16::from_le_bytes([pair[0], pair[1]]) }).collect();
            String::from_utf16_lossy(&units).trim_start_matches('\u{feff}').to_owned()
        } else {
            String::from_utf8_lossy(&rest[..terminator]).into_owned()
        };
        pictures.push(Apic { mime: String::from_utf8(body[1..mime_end].to_vec()).unwrap(), kind, description,
            data: rest[terminator + if wide { 2 } else { 1 }..].to_vec() });
    }
    pictures
}

// --- A: the v0.1.0 refusal stays the default --------------------------------------------

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn mp3_is_refused_without_the_opt_in_with_the_v0_1_0_message() {
    let sandbox = Sandbox::configured();
    let mp3 = sandbox.plain_mp3("tone");
    let before = fs::read(&mp3).unwrap();
    // First the setting at its default, then explicitly stored as false.
    for (round, explicit) in [None, Some(false)].into_iter().enumerate() {
        if let Some(on) = explicit { sandbox.set_opt_in(on); }
        let out = sandbox.dir(&format!("out-{round}"));
        for args in [
            vec!["process", text(&mp3), "--format", "match-input", "--output-dir", text(&out)],
            vec!["inspect", text(&mp3)],
            vec!["analyze", text(&mp3)],
        ] {
            let outcome = sandbox.fails(1, &args);
            assert_failed_item(&outcome, 0, None, "unsupported_input_codec");
            let message = outcome["results"][0]["error"]["message"].as_str().unwrap();
            assert!(message.contains(MP3_REFUSAL), "{args:?}: message {message:?} must contain {MP3_REFUSAL:?}");
        }
        assert_no_files(&out);
        assert_eq!(fs::read(&mp3).unwrap(), before, "the input is never modified");
    }
}

// --- B: the opt-in decodes MP3 and says so ----------------------------------------------

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn mp3_with_opt_in() {
    let sandbox = Sandbox::configured();
    let input = sandbox.plain_mp3("tone");
    let before = fs::read(&input).unwrap();
    sandbox.set_opt_in(true);

    let out = sandbox.dir("out");
    let processed = sandbox.ok(&["process", text(&input), "--format", "match-input", "--output-dir", text(&out)]);
    assert_eq!(paths(report(&processed, 0)), ["mp3_decode"], "{processed}");
    let output = report(&processed, 0)["output_path"].as_str().unwrap();
    assert_eq!(Path::new(output).extension().and_then(|e| e.to_str()), Some("mp3"), "{processed}");
    assert!(Path::new(output).starts_with(&out), "output {output} must be inside {}", out.display());

    let inspected = sandbox.ok(&["inspect", output]);
    let inspection = report(&inspected, 0);
    assert_eq!(inspection["processed"], true, "{inspected}");
    assert_eq!(paths(inspection), ["mp3_decode"], "{inspected}");
    assert_eq!(inspection["sample_rate"], 44_100.0, "{inspected}");
    assert_eq!(inspection["channels"], 2, "{inspected}");

    let analyzed = sandbox.ok(&["analyze", text(&input)]);
    assert_eq!(paths(report(&analyzed, 0)), ["mp3_decode"], "{analyzed}");
    assert!(report(&analyzed, 0)["volume"].as_f64().is_some_and(f64::is_finite), "{analyzed}");

    let wav_out = sandbox.dir("wav-out");
    let as_wav = sandbox.ok(&["process", text(&input), "--format", "wav", "--output-dir", text(&wav_out)]);
    // The generic copy into a WAV writes an `ID3 ` chunk (tempo and key) that the default policy drops; the opt-in keeps it.
    assert_eq!(paths(report(&as_wav, 0)), ["mp3_decode", "wav_tag_preservation"], "{as_wav}");
    assert_eq!(Path::new(report(&as_wav, 0)["output_path"].as_str().unwrap()).extension().and_then(|e| e.to_str()), Some("wav"));

    assert_eq!(fs::read(&input).unwrap(), before, "the input is never modified");
}

// --- C: identical paths stay identical ------------------------------------------------------

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn identical_paths_stay_identical_with_the_opt_in_on() {
    let sandbox = Sandbox::configured();
    let wav = sandbox.file("song.wav", &generated_wav(RATE, SECONDS, false));
    for format in ["wav", "flac"] {
        let mut digests = Vec::new();
        for (round, on) in [false, true].into_iter().enumerate() {
            sandbox.set_opt_in(on);
            let out = sandbox.dir(&format!("{format}-{round}"));
            let processed = sandbox.ok(&["process", text(&wav), "--format", format, "--output-dir", text(&out)]);
            assert_eq!(paths(report(&processed, 0)), Vec::<&str>::new(), "{format}, opt-in {on}: {processed}");
            let output = report(&processed, 0)["output_path"].as_str().unwrap();
            let inspected = sandbox.ok(&["inspect", output]);
            assert_eq!(paths(report(&inspected, 0)), Vec::<&str>::new(), "{format}, opt-in {on}: {inspected}");
            digests.push(file_sha256(output).unwrap());
        }
        assert_eq!(digests[0], digests[1], "{format}: the opt-in must not change an output that uses only identical paths");
    }
}

// --- D: sample-rate conversion is not reproduced ----------------------------------------------

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn unsupported_sample_rate_conversion_for_a_22_05_khz_wav() {
    let sandbox = Sandbox::configured();
    let wav = sandbox.file("low-rate.wav", &generated_wav(22_050, SECONDS, false));
    for (round, on) in [false, true].into_iter().enumerate() {
        sandbox.set_opt_in(on);
        let out = sandbox.dir(&format!("out-{round}"));
        // Inspection reads the file as it is; processing and analysis need 44.1 kHz.
        let inspected = sandbox.ok(&["inspect", text(&wav)]);
        assert_eq!(report(&inspected, 0)["sample_rate"], 22_050.0, "opt-in {on}: {inspected}");
        for args in [
            vec!["process", text(&wav), "--format", "wav", "--output-dir", text(&out)],
            vec!["analyze", text(&wav)],
        ] {
            let outcome = sandbox.fails(1, &args);
            assert_failed_item(&outcome, 0, None, "unsupported_sample_rate_conversion");
        }
        assert_no_files(&out);
    }
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn unsupported_sample_rate_conversion_for_a_24_khz_mp3() {
    let sandbox = Sandbox::configured();
    sandbox.set_opt_in(true);
    let mp3 = sandbox.file("mpeg2-24khz.mp3", &mpeg2_24khz_silence());
    // Opening at the file's own rate works; processing opens at 44.1 kHz and the decoder does not resample.
    let inspected = sandbox.ok(&["inspect", text(&mp3)]);
    assert_eq!(report(&inspected, 0)["sample_rate"], 24_000.0, "{inspected}");
    assert_eq!(paths(report(&inspected, 0)), ["mp3_decode"], "{inspected}");
    let out = sandbox.dir("out");
    for args in [
        vec!["process", text(&mp3), "--format", "wav", "--output-dir", text(&out)],
        vec!["analyze", text(&mp3)],
    ] {
        let outcome = sandbox.fails(1, &args);
        assert_failed_item(&outcome, 0, None, "unsupported_sample_rate_conversion");
    }
    assert_no_files(&out);
}

// --- D3: the output side refuses what the input side accepted ------------------------------------

/// A 192 kHz WAV is a fine input (it inspects and would process to WAV), but the portable ALAC writer only
/// encodes 22,050–96,000 Hz and 1–8 channels. The refusal concerns the requested output, so its code is the
/// output-side `unsupported_output_format`, not `unsupported_input_format`; the same code covers more than eight
/// channels (one check in the writer; no generated fixture is needed for that half).
#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn unsupported_output_format_for_alac_beyond_96_khz() {
    let sandbox = Sandbox::configured();
    let wav = sandbox.file("high-rate.wav", &generated_wav(192_000, SECONDS, false));
    for (round, on) in [false, true].into_iter().enumerate() {
        sandbox.set_opt_in(on);
        let inspected = sandbox.ok(&["inspect", text(&wav)]);
        assert_eq!(report(&inspected, 0)["sample_rate"], 192_000.0, "opt-in {on}: {inspected}");
        let out = sandbox.dir(&format!("out-{round}"));
        let outcome = sandbox.fails(1, &["process", text(&wav), "--format", "alac", "--output-dir", text(&out)]);
        assert_failed_item(&outcome, 0, None, "unsupported_output_format");
        assert_no_files(&out);
    }
}

// --- E: a damaged MP3 is an invalid input -------------------------------------------------------

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn corrupt_mp3_with_the_opt_in_is_an_invalid_input_file() {
    let sandbox = Sandbox::configured();
    sandbox.set_opt_in(true);
    let mp3 = sandbox.file("damaged.mp3", &damaged_mp3());
    let before = fs::read(&mp3).unwrap();
    // The decoder scans only frame headers when it opens a file, so inspection succeeds ...
    let inspected = sandbox.ok(&["inspect", text(&mp3)]);
    assert_eq!(report(&inspected, 0)["sample_rate"], 44_100.0, "{inspected}");
    assert_eq!(report(&inspected, 0)["channels"], 2, "{inspected}");
    assert_eq!(paths(report(&inspected, 0)), ["mp3_decode"], "{inspected}");
    // ... and the damage surfaces at the first read, which processing and analysis make.
    let out = sandbox.dir("out");
    for args in [
        vec!["process", text(&mp3), "--format", "wav", "--output-dir", text(&out)],
        vec!["analyze", text(&mp3)],
    ] {
        let outcome = sandbox.fails(1, &args);
        assert_failed_item(&outcome, 0, Some("read analysis input"), "invalid_input_file");
        let message = outcome["results"][0]["error"]["message"].as_str().unwrap();
        assert!(message.contains("MP3"), "{args:?}: {message}");
    }
    assert_no_files(&out);
    assert_eq!(fs::read(&mp3).unwrap(), before);
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn a_truncated_mp3_is_refused_by_the_container_check_before_any_decoder_runs() {
    let sandbox = Sandbox::configured();
    let mp3 = sandbox.file("truncated.mp3", &truncated_mp3());
    for (round, on) in [false, true].into_iter().enumerate() {
        sandbox.set_opt_in(on);
        let out = sandbox.dir(&format!("out-{round}"));
        for args in [
            vec!["process", text(&mp3), "--format", "wav", "--output-dir", text(&out)],
            vec!["inspect", text(&mp3)],
            vec!["analyze", text(&mp3)],
        ] {
            // Source tags are read before the audio, and the tag bridge cannot establish
            // that a stream with no complete frame carries no processed marker.
            let outcome = sandbox.fails(1, &args);
            assert_failed_item(&outcome, 0, Some("inspect tags"), "marker_unverifiable");
        }
        assert_no_files(&out);
    }
}

// --- F: AAC stays refused in both directions ------------------------------------------------------

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn aac_is_always_refused() {
    let sandbox = Sandbox::configured();
    let m4a = sandbox.file("tone.m4a", &aac_m4a());
    let wav = sandbox.file("tone.wav", &generated_wav(RATE, SECONDS, false));
    let before = fs::read(&m4a).unwrap();
    for (round, on) in [false, true].into_iter().enumerate() {
        sandbox.set_opt_in(on);
        let decode_out = sandbox.dir(&format!("decode-out-{round}"));
        for args in [
            vec!["process", text(&m4a), "--format", "wav", "--output-dir", text(&decode_out)],
            vec!["inspect", text(&m4a)],
            vec!["analyze", text(&m4a)],
        ] {
            let outcome = sandbox.fails(1, &args);
            assert_failed_item(&outcome, 0, None, "unsupported_input_codec");
        }
        assert_no_files(&decode_out);

        let encode_out = sandbox.dir(&format!("encode-out-{round}"));
        let outcome = sandbox.fails(1, &["process", text(&wav), "--format", "aac", "--output-dir", text(&encode_out)]);
        assert_failed_item(&outcome, 0, None, "unsupported_output_codec");
        assert_no_files(&encode_out);
    }
    assert_eq!(fs::read(&m4a).unwrap(), before);
}

// --- G: embedded artwork --------------------------------------------------------------------------

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn artwork_flac_png_to_flac_is_dropped_off_and_copied_on() {
    let sandbox = Sandbox::configured();
    let picture = FlacPicture::png();
    let source = sandbox.flac("cover", Some(&picture));
    let before = fs::read(&source).unwrap();
    assert_eq!(flac_pictures(&before), [FlacPicture::png()], "fixture must carry exactly the source picture");
    for (round, on) in [false, true].into_iter().enumerate() {
        sandbox.set_opt_in(on);
        let out = sandbox.dir(&format!("out-{round}"));
        let processed = sandbox.ok(&["process", text(&source), "--format", "flac", "--output-dir", text(&out)]);
        let pictures = flac_pictures(&fs::read(report(&processed, 0)["output_path"].as_str().unwrap()).unwrap());
        if on {
            assert_eq!(paths(report(&processed, 0)), ["artwork_copy"], "{processed}");
            // One block, the source image byte for byte, with its type, description and geometry.
            assert_eq!(pictures, [FlacPicture::png()], "{processed}");
        } else {
            // The reference writes no PICTURE block for a FLAC source.
            assert_eq!(paths(report(&processed, 0)), Vec::<&str>::new(), "{processed}");
            assert!(pictures.is_empty(), "opt-in off must not write a PICTURE block, found {}", pictures.len());
        }
    }
    assert_eq!(fs::read(&source).unwrap(), before, "the input is never modified");
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn artwork_flac_png_to_mp3_is_refused_off_and_copied_on() {
    let sandbox = Sandbox::configured();
    let picture = FlacPicture::png();
    let source = sandbox.flac("cover", Some(&picture));
    let before = fs::read(&source).unwrap();

    // Off: the reference would transcode the picture to PNG; this platform cannot.
    let refused_out = sandbox.dir("refused-out");
    let outcome = sandbox.fails(1, &["process", text(&source), "--format", "mp3", "--output-dir", text(&refused_out)]);
    assert_failed_item(&outcome, 0, None, "artwork_conversion_unavailable");
    assert_no_files(&refused_out);

    // On: the encoded bytes are carried into one ID3v2 APIC frame.
    sandbox.set_opt_in(true);
    let out = sandbox.dir("out");
    let processed = sandbox.ok(&["process", text(&source), "--format", "mp3", "--output-dir", text(&out)]);
    assert_eq!(paths(report(&processed, 0)), ["artwork_copy"], "{processed}");
    let pictures = id3_pictures(&fs::read(report(&processed, 0)["output_path"].as_str().unwrap()).unwrap());
    let expected = Apic { mime: "image/png".to_owned(), kind: 3, description: "Front cover".to_owned(), data: picture.data.clone() };
    assert_eq!(pictures, [expected], "{processed}");
    assert_eq!(fs::read(&source).unwrap(), before, "the input is never modified");
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn artwork_flac_webp_to_alac_is_refused_but_png_is_copied() {
    let sandbox = Sandbox::configured();
    let webp = sandbox.flac("cover-webp", Some(&FlacPicture::webp()));
    let png = sandbox.flac("cover-png", Some(&FlacPicture::png()));
    let before = fs::read(&webp).unwrap();
    // Off: no transcoding is available, so any picture the reference would convert is refused.
    // On: an MP4 `covr` atom cannot hold WebP and the copy never transcodes, so WebP is refused too.
    for (round, on) in [false, true].into_iter().enumerate() {
        sandbox.set_opt_in(on);
        let out = sandbox.dir(&format!("webp-out-{round}"));
        let outcome = sandbox.fails(1, &["process", text(&webp), "--format", "alac", "--output-dir", text(&out)]);
        assert_failed_item(&outcome, 0, None, "artwork_conversion_unavailable");
        assert_no_files(&out);
    }
    // The same copy stores a PNG in `covr` unchanged.
    let out = sandbox.dir("png-out");
    let processed = sandbox.ok(&["process", text(&png), "--format", "alac", "--output-dir", text(&out)]);
    assert_eq!(paths(report(&processed, 0)), ["artwork_copy"], "{processed}");
    assert_eq!(fs::read(&webp).unwrap(), before, "the input is never modified");
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn artwork_mp3_apic_is_moved_unchanged_and_reports_only_mp3_decode() {
    let sandbox = Sandbox::configured();
    sandbox.set_opt_in(true);
    let jpeg = generated_jpeg();
    let mut with_cover = id3v23_apic_tag("image/jpeg", 3, "Front cover", &jpeg);
    with_cover.extend(fs::read(sandbox.plain_mp3("tone")).unwrap()); // already tag-free MPEG frames
    let source = sandbox.file("inputs/cover.mp3", &with_cover);
    let out = sandbox.dir("out");
    // ID3 to ID3 is a direct copy that already moves the frame untouched, so only the decoder is non-identical.
    let processed = sandbox.ok(&["process", text(&source), "--format", "match-input", "--output-dir", text(&out)]);
    assert_eq!(paths(report(&processed, 0)), ["mp3_decode"], "{processed}");
    let output = report(&processed, 0)["output_path"].as_str().unwrap();
    assert_eq!(Path::new(output).extension().and_then(|e| e.to_str()), Some("mp3"), "{processed}");
    let pictures = id3_pictures(&fs::read(output).unwrap());
    assert_eq!(pictures.len(), 1, "{processed}");
    assert_eq!(pictures[0].mime, "image/jpeg");
    assert_eq!(pictures[0].data, jpeg, "the APIC image must be byte-identical to the source's");
    assert_eq!(fs::read(&source).unwrap(), with_cover, "the input is never modified");
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn sources_without_artwork_report_no_artwork_copy() {
    let sandbox = Sandbox::configured();
    sandbox.set_opt_in(true);
    let wav = sandbox.file("song.wav", &generated_wav(RATE, SECONDS, false));
    let flac = sandbox.flac("plain", None);
    assert!(flac_pictures(&fs::read(&flac).unwrap()).is_empty());
    for (round, (source, format)) in [(&wav, "wav"), (&wav, "flac"), (&flac, "mp3")].into_iter().enumerate() {
        let out = sandbox.dir(&format!("out-{round}"));
        let processed = sandbox.ok(&["process", text(source), "--format", format, "--output-dir", text(&out)]);
        assert_eq!(paths(report(&processed, 0)), Vec::<&str>::new(), "{} -> {format}: {processed}", source.display());
    }
}

// --- G6: a WAV source's tag chunks survive WAV output only with the opt-in -----------------------------

fn riff_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut chunk = id.to_vec();
    chunk.extend_from_slice(&(body.len() as u32).to_le_bytes());
    chunk.extend_from_slice(body);
    if body.len() % 2 == 1 { chunk.push(0); }
    chunk
}

/// `generated_wav` with extra chunks after `data` and a consistent RIFF size.
fn wav_with_chunks(extra: &[Vec<u8>]) -> Vec<u8> {
    let mut wav = generated_wav(RATE, SECONDS, false);
    for chunk in extra { wav.extend_from_slice(chunk); }
    let size = (wav.len() - 8) as u32;
    wav[4..8].copy_from_slice(&size.to_le_bytes());
    wav
}

/// Top-level chunks of a WAV, in file order, checking that the RIFF size matches the file.
fn wav_chunks(data: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    assert_eq!((&data[..4], &data[8..12]), (&b"RIFF"[..], &b"WAVE"[..]));
    assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize + 8, data.len(), "RIFF size must match the file");
    let (mut position, mut chunks) = (12, Vec::new());
    while position + 8 <= data.len() {
        let length = u32::from_le_bytes(data[position + 4..position + 8].try_into().unwrap()) as usize;
        assert!(position + 8 + length <= data.len());
        chunks.push((data[position..position + 4].try_into().unwrap(), data[position + 8..position + 8 + length].to_vec()));
        position += 8 + length + (length & 1);
    }
    assert_eq!(position, data.len());
    chunks
}

fn id3v23_frame(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut frame = id.to_vec();
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(body);
    frame
}

/// An ID3v2.3 tag with a title, an artist and one PNG picture.
fn id3v23_tag_with_picture(png: &[u8]) -> Vec<u8> {
    let text = |value: &str| { let mut body = vec![0]; body.extend_from_slice(value.as_bytes()); body };
    let mut apic = vec![0];
    apic.extend_from_slice(b"image/png\0");
    apic.push(3);
    apic.extend_from_slice(b"front\0");
    apic.extend_from_slice(png);
    let frames = [id3v23_frame(b"TIT2", &text("Wav title")), id3v23_frame(b"TPE1", &text("Wav artist")), id3v23_frame(b"APIC", &apic)].concat();
    let size = frames.len();
    let mut tag = vec![b'I', b'D', b'3', 3, 0, 0, (size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f];
    tag.extend(frames);
    tag
}

/// The reference marker writer drops the chunk TagLib saves last (see CLI_CONTRACT.md, WAV tag chunks); with
/// `parity.allow_non_identical` the engine keeps every chunk TagLib wrote and says so in `non_identical_paths`.
/// Nothing else about the output changes, and the default policy is exactly what it was.
#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn wav_tag_chunks_are_kept_with_the_opt_in_and_dropped_without() {
    let sandbox = Sandbox::configured();
    let png = generated_png(8, 8);
    let id3 = id3v23_tag_with_picture(&png);
    let info = [b"INFO".to_vec(), riff_chunk(b"INAM", b"Info title\0"), riff_chunk(b"IART", b"Info artist\0")].concat();
    // (label, chunks after data, chunk ids the default policy keeps of them, ids the opt-in keeps, the opt-in changes the output)
    let cases: [(&str, Vec<Vec<u8>>, &[&str], &[&str], bool); 5] = [
        ("ID3 chunk only", vec![riff_chunk(b"ID3 ", &id3)], &[], &["ID3 "], true),
        ("lowercase id3 chunk only", vec![riff_chunk(b"id3 ", &id3)], &[], &["ID3 "], true),
        ("INFO LIST only", vec![riff_chunk(b"LIST", &info)], &[], &["LIST"], true),
        ("ID3 chunk and INFO LIST", vec![riff_chunk(b"ID3 ", &id3), riff_chunk(b"LIST", &info)], &["ID3 "], &["ID3 ", "LIST"], true),
        ("no tag chunks", Vec::new(), &[], &[], false),
    ];
    let plain = {
        let wav = sandbox.file("plain.wav", &wav_with_chunks(&[]));
        let out = sandbox.dir("plain-out");
        let processed = sandbox.ok(&["process", text(&wav), "--format", "wav", "--output-dir", text(&out)]);
        wav_chunks(&fs::read(report(&processed, 0)["output_path"].as_str().unwrap()).unwrap())
    };
    let plain_ids: Vec<String> = plain.iter().map(|(id, _)| String::from_utf8_lossy(id).into_owned()).collect();
    assert_eq!(plain_ids.last().map(String::as_str), Some("PLTN"), "the marker is the last chunk: {plain_ids:?}");
    // The chunk ids of an output: the plain layout with the kept tag chunks before the marker.
    let layout = |kept: &[&str]| -> Vec<String> {
        let mut ids = plain_ids[..plain_ids.len() - 1].to_vec();
        ids.extend(kept.iter().map(|id| id.to_string()));
        ids.push("PLTN".to_owned());
        ids
    };
    for (case, (label, extra, default_kept, opt_in_kept, preserved)) in cases.into_iter().enumerate() {
        let wav = sandbox.file(&format!("case-{case}.wav"), &wav_with_chunks(&extra));
        for format in ["wav", "match-input"] {
            let mut outputs = Vec::new();
            for (round, on) in [false, true].into_iter().enumerate() {
                sandbox.set_opt_in(on);
                let out = sandbox.dir(&format!("out-{case}-{format}-{round}"));
                let processed = sandbox.ok(&["process", text(&wav), "--format", format, "--output-dir", text(&out)]);
                let chunks = wav_chunks(&fs::read(report(&processed, 0)["output_path"].as_str().unwrap()).unwrap());
                let ids: Vec<String> = chunks.iter().map(|(id, _)| String::from_utf8_lossy(id).to_ascii_uppercase()).collect();
                let expected = layout(if on { opt_in_kept } else { default_kept });
                assert_eq!(ids, expected.iter().map(|id| id.to_ascii_uppercase()).collect::<Vec<_>>(), "{label}, {format}, opt-in {on}: {processed}");
                let expected_paths: &[&str] = if on && preserved { &["wav_tag_preservation"] } else { &[] };
                assert_eq!(paths(report(&processed, 0)), expected_paths, "{label}, {format}, opt-in {on}: {processed}");
                if on {
                    if let Some((_, body)) = chunks.iter().find(|(id, _)| id.eq_ignore_ascii_case(b"ID3 ")) {
                        let pictures = id3_pictures(body);
                        assert_eq!(pictures.len(), 1, "{label}, {format}: {processed}");
                        assert_eq!(pictures[0].data, png, "{label}, {format}: the picture survives byte for byte");
                        assert!(body.windows(9).any(|window| window == b"Wav title") && body.windows(10).any(|window| window == b"Wav artist"), "{label}, {format}");
                    }
                    if let Some((_, body)) = chunks.iter().find(|(id, _)| id == b"LIST") {
                        assert!(body.starts_with(b"INFO") && body.windows(10).any(|window| window == b"Info title"), "{label}, {format}");
                    }
                }
                outputs.push(chunks);
            }
            // Only chunks are added: everything the default policy writes is byte-identical under the opt-in.
            for (id, body) in &outputs[0] {
                let kept = outputs[1].iter().find(|(other, _)| other == id).unwrap_or_else(|| panic!("{label}, {format}: {id:?} missing under the opt-in"));
                assert_eq!(&kept.1, body, "{label}, {format}: {id:?}");
            }
        }
    }
}

/// A cross-format copy into a WAV always writes tempo and key through TagLib, so even a source without tags gains an
/// `ID3 ` chunk (an empty key and a zero tempo) under the opt-in, and the item says so.
#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn cross_format_copies_into_a_wav_keep_the_chunk_taglib_wrote() {
    let sandbox = Sandbox::configured();
    let flac = sandbox.flac("plain", None);
    let mut outputs = Vec::new();
    for (round, on) in [false, true].into_iter().enumerate() {
        sandbox.set_opt_in(on);
        let out = sandbox.dir(&format!("out-{round}"));
        let processed = sandbox.ok(&["process", text(&flac), "--format", "wav", "--output-dir", text(&out)]);
        let expected: &[&str] = if on { &["wav_tag_preservation"] } else { &[] };
        assert_eq!(paths(report(&processed, 0)), expected, "opt-in {on}: {processed}");
        outputs.push(wav_chunks(&fs::read(report(&processed, 0)["output_path"].as_str().unwrap()).unwrap()));
    }
    for (id, body) in &outputs[0] {
        let kept = outputs[1].iter().find(|(other, _)| other == id).unwrap_or_else(|| panic!("{id:?} missing under the opt-in"));
        assert_eq!(&kept.1, body, "{id:?}");
    }
    let added: Vec<&[u8; 4]> = outputs[1].iter().map(|(id, _)| id).filter(|id| !outputs[0].iter().any(|(other, _)| other == *id)).collect();
    assert_eq!(added, [b"ID3 "], "the opt-in only adds the chunk TagLib wrote");
}

// --- H: failed items carry a code and no report -------------------------------------------------------

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB/PN_FLAC_SHA256, PN_LAME_LIB/PN_LAME_SHA256 (the image's libraries)"]
fn failed_items_have_no_report() {
    let sandbox = Sandbox::configured();
    let m4a = sandbox.file("tone.m4a", &aac_m4a());
    let low = sandbox.file("low-rate.wav", &generated_wav(22_050, SECONDS, false));
    let good = sandbox.file("good.wav", &generated_wav(RATE, SECONDS, false));
    let out = sandbox.dir("out");
    for (input, code) in [(&m4a, "unsupported_input_codec"), (&low, "unsupported_sample_rate_conversion")] {
        for command in ["process", "analyze"] {
            let args = if command == "process" { vec![command, text(input), "--format", "wav", "--output-dir", text(&out)] } else { vec![command, text(input)] };
            let outcome = sandbox.fails(1, &args);
            assert_eq!(outcome["status"], "failed", "{outcome}");
            assert_failed_item(&outcome, 0, None, code);
        }
    }
    assert_failed_item(&sandbox.fails(1, &["inspect", text(&m4a)]), 0, None, "unsupported_input_codec");

    // In one batch each item has either a report or an error, never both.
    let batch = sandbox.fails(1, &["process", text(&good), text(&m4a), "--format", "wav", "--output-dir", text(&out)]);
    assert_eq!(batch["status"], "partial_failure", "{batch}");
    let succeeded = result_for(&batch, &good);
    assert_eq!(succeeded["status"], "ok", "{batch}");
    assert!(succeeded["report"].is_object() && succeeded.get("error").is_none(), "{batch}");
    let failed = result_for(&batch, &m4a);
    assert_eq!(failed["status"], "failed", "{batch}");
    assert_eq!(failed["error"]["code"], "unsupported_input_codec", "{batch}");
    assert!(failed.get("report").is_none(), "{batch}");
}
