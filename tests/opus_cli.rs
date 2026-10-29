//! CLI-level behaviour of the opt-in Opus format (`formats.opus`).
//!
//! The first group needs no native library: with the setting off every build
//! refuses Opus exactly as it always has. The second group (`#[ignore]`d) needs
//! a trusted tag bridge and the fixed-point libopus 1.6.1 build:
//! `PLATINUM_TEST_TAG_BRIDGE`, `PLATINUM_TEST_OPUS_LIB`, `PLATINUM_TEST_OPUS_SHA256`
//! (run with `cargo test --test opus_cli -- --ignored`).

use platinum_engine::opus::{self, CommentHeader};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Sandbox { root: PathBuf, home: PathBuf, state: PathBuf }
impl Sandbox {
    fn new() -> Self {
        let root = loop {
            let candidate = std::env::temp_dir().join(format!("platinum-opus-cli-{}-{}", std::process::id(), NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create isolated test directory: {e}"),
            }
        };
        let root = fs::canonicalize(root).unwrap();
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        let state = root.join("state");
        Self { root, home, state }
    }
    fn run(&self, args: &[&str]) -> (Output, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_platinum"))
            .env("HOME", &self.home).arg("--state-dir").arg(&self.state).arg("--json").args(args).output().unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!("invalid JSON for {args:?}: {e}; stdout={:?}; stderr={:?}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
        });
        (output, value)
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (output, value) = self.run(args);
        assert!(output.status.success(), "{args:?}: {value}");
        value
    }
    fn dir(&self, name: &str) -> PathBuf { let p = self.root.join(name); fs::create_dir_all(&p).unwrap(); p }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.root.join(name);
        if let Some(parent) = p.parent() { fs::create_dir_all(parent).unwrap(); }
        fs::write(&p, bytes).unwrap();
        p
    }
    fn set(&self, field: &str, value: Value) { self.ok(&["config", "set", field, &value.to_string()]); }
}
impl Drop for Sandbox { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }
fn text(path: &Path) -> &str { path.to_str().unwrap() }

fn item_code(outcome: &Value) -> String {
    outcome["results"][0]["error"]["code"].as_str().unwrap_or_else(|| panic!("no item error code: {outcome}")).to_owned()
}

/// A syntactically valid Ogg Opus file made without libopus (fake audio packets).
fn fake_opus(packets: usize) -> Vec<u8> {
    use opus::ogg::PageWriter;
    let mut out = Vec::new();
    let mut w = PageWriter::new(&mut out, 0x1357_9bdf);
    w.write_header_packet(&opus::OpusHead { version: 1, channels: 2, pre_skip: 312, input_sample_rate: 48_000, output_gain: 0 }.encode()).unwrap();
    w.write_header_packet(&CommentHeader { vendor: b"t".to_vec(), comments: Vec::new(), trailing: Vec::new() }.encode().unwrap()).unwrap();
    for i in 0..packets { w.push_audio(&[0xfc, i as u8, 1, 2, 3], 960, opus::PAGE_SAMPLES).unwrap(); }
    w.finish(packets as u64 * 960).unwrap();
    out
}

fn wav(rate: u32, seconds: u32, channels: u16) -> Vec<u8> {
    let frames = rate * seconds;
    let data_len = frames * 2 * u32::from(channels);
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF"); b.extend_from_slice(&(36 + data_len).to_le_bytes()); b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes()); b.extend_from_slice(&1u16.to_le_bytes()); b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes()); b.extend_from_slice(&(rate * 2 * u32::from(channels)).to_le_bytes());
    b.extend_from_slice(&(2 * channels).to_le_bytes()); b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data"); b.extend_from_slice(&data_len.to_le_bytes());
    for n in 0..frames {
        let v = ((f64::from(n) * 0.0576).sin() * 9000.0) as i16;
        for _ in 0..channels { b.extend_from_slice(&v.to_le_bytes()); }
    }
    b
}

// --- no library needed: the opt-in is off ---------------------------------------

#[test]
fn opus_is_refused_exactly_as_before_while_the_setting_is_off() {
    let s = Sandbox::new();
    let input = s.file("in/song.opus", &fake_opus(3));
    let out = s.dir("out");
    // Discovery ignores .opus: nothing admitted.
    let (output, outcome) = s.run(&["process", text(input.parent().unwrap()), "--format", "wav", "--output-dir", text(&out), "--jobs", "1"]);
    assert_eq!(output.status.code(), Some(1), "{outcome}");
    assert_eq!(outcome["results"][0]["error"]["code"], "no_input_files", "{outcome}");
    // Naming the file does not admit it either.
    let (_, outcome) = s.run(&["process", text(&input), "--format", "wav", "--output-dir", text(&out), "--jobs", "1"]);
    assert_eq!(item_code(&outcome), "no_input_files");
    // Inspect names the source type as unsupported, like any unadmitted extension.
    let (_, outcome) = s.run(&["inspect", text(&input)]);
    assert_eq!(item_code(&outcome), "unsupported_input_format", "{outcome}");
    // Opus output is refused with its own code before anything runs.
    let wavfile = s.file("in/a.wav", &wav(48_000, 1, 2));
    let (output, outcome) = s.run(&["process", text(&wavfile), "--format", "opus", "--output-dir", text(&out), "--jobs", "1"]);
    assert_eq!(output.status.code(), Some(2), "{outcome}");
    assert_eq!(outcome["error"]["code"], "opus_not_enabled", "{outcome}");
    assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
}

#[test]
fn the_setting_is_a_validated_config_field_that_defaults_to_off_and_stays_out_of_old_state_files() {
    let s = Sandbox::new();
    assert_eq!(s.ok(&["config", "get", "formats.opus"])["data"]["value"], json!(false));
    let shown = s.ok(&["config", "show"]);
    assert_eq!(shown["data"]["formats"]["opus"], json!(false), "{shown}");
    assert_eq!(shown["data"]["formats_effect"]["opus"], json!(false), "{shown}");
    // Opus output cannot be selected while the format is off.
    let (output, outcome) = s.run(&["config", "set", "output.format", "\"opus\""]);
    assert_eq!(output.status.code(), Some(1), "{outcome}");
    assert_eq!(outcome["error"]["code"], "settings_error", "{outcome}");
    s.set("formats.opus", json!(true));
    s.set("output.format", json!("opus"));
    // ... and the format cannot be switched off underneath it.
    let (_, outcome) = s.run(&["config", "set", "formats.opus", "false"]);
    assert_eq!(outcome["error"]["code"], "settings_error", "{outcome}");
    s.set("output.format", json!("mp3"));
    s.ok(&["config", "reset", "formats.opus"]);
    assert_eq!(s.ok(&["config", "get", "formats.opus"])["data"]["value"], json!(false));
    let stored = fs::read_to_string(s.state.join("settings.json")).unwrap();
    assert!(!stored.contains("\"formats\"") && !stored.contains("\"opus\"") && !stored.contains("opus_sha256"),
        "a default state file must not mention the opt-in: {stored}");
    // Library paths are validated like the other pins.
    let (_, outcome) = s.run(&["config", "set", "libraries.opus_sha256", "\"xyz\""]);
    assert_eq!(outcome["error"]["code"], "settings_error", "{outcome}");
}

#[test]
fn ogg_files_stay_unsupported_with_the_opt_in_on() {
    let s = Sandbox::new();
    s.set("formats.opus", json!(true));
    let input = s.file("in/a.ogg", &fake_opus(3));
    let (_, outcome) = s.run(&["inspect", text(&input)]);
    assert_eq!(item_code(&outcome), "unsupported_input_format", "{outcome}");
}

// --- libraries needed -------------------------------------------------------------

fn libraries() -> (String, String, String) {
    (std::env::var("PLATINUM_TEST_TAG_BRIDGE").expect("PLATINUM_TEST_TAG_BRIDGE"),
     std::env::var("PLATINUM_TEST_OPUS_LIB").expect("PLATINUM_TEST_OPUS_LIB"),
     std::env::var("PLATINUM_TEST_OPUS_SHA256").expect("PLATINUM_TEST_OPUS_SHA256"))
}
fn enabled() -> Sandbox {
    let s = Sandbox::new();
    let (bridge, lib, digest) = libraries();
    s.set("libraries.tag_bridge", json!(bridge));
    s.set("libraries.opus", json!(lib));
    s.set("libraries.opus_sha256", json!(digest));
    s.set("formats.opus", json!(true));
    s
}
fn encode(path: &Path, channels: usize, frames: usize) {
    let (_, lib, digest) = libraries();
    let library = opus::OpusLibrary::load(lib, &digest).unwrap();
    let file = fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap();
    let mut writer = opus::OpusWriter::create(&library, file, channels).unwrap();
    let pcm: Vec<f32> = (0..frames * channels).map(|i| (((i / channels) as f32) * 0.0576).sin() * 0.3).collect();
    writer.write(&pcm).unwrap();
    writer.finish().unwrap().sync_all().unwrap();
    opus::repack_with_comments(path, &CommentHeader { vendor: b"libopus 1.6.1-fixed".to_vec(),
        comments: vec![b"TITLE=Round trip".to_vec(), b"R128_TRACK_GAIN=-573".to_vec(), b"ARTIST=A".to_vec()], trailing: Vec::new() }).unwrap();
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PLATINUM_TEST_OPUS_LIB and PLATINUM_TEST_OPUS_SHA256"]
fn opus_round_trip_is_deterministic_marked_and_reported_as_having_no_reference() {
    let s = enabled();
    let input = s.dir("in").join("song.opus");
    encode(&input, 2, 96_000);
    let mut hashes = Vec::new();
    for run in 0..2 {
        let out = s.dir(&format!("out{run}"));
        let outcome = s.ok(&["process", text(&input), "--format", "match-input", "--output-dir", text(&out), "--jobs", "1"]);
        let report = &outcome["results"][0]["report"];
        assert_eq!(outcome["results"][0]["status"], "ok", "{outcome}");
        assert_eq!(report["output_format"], "opus/opus");
        assert_eq!(report["processing_sample_rate"], json!(48_000.0));
        assert_eq!(report["processing_marker_status"], "present");
        assert_eq!(report["encoded_length_frames"], report["input_frames"]);
        assert!(report["no_platinum_notes_reference"].as_str().unwrap().contains("no Platinum Notes reference"));
        assert_eq!(report["opus_output"]["dropped_tags"], json!(["R128_TRACK_GAIN"]));
        assert_eq!(report["opus_output"]["target_bitrate_bps"], json!(256_000));
        hashes.push(fs::read(out.join("song_PN.opus")).unwrap());
    }
    assert_eq!(hashes[0], hashes[1], "same input and settings must give the same bytes");
    // The output carries the marker, so it is refused as already processed.
    let again = s.dir("again");
    let (_, outcome) = s.run(&["process", text(&s.root.join("out0/song_PN.opus")), "--format", "match-input", "--output-dir", text(&again), "--jobs", "1"]);
    assert_eq!(item_code(&outcome), "already_processed", "{outcome}");
    // Source comments are copied, the loudness tag is not, the marker is added.
    let header = opus::tags::read_comment_header(&s.root.join("out0/song_PN.opus")).unwrap();
    let names: Vec<String> = header.comments.iter().map(|c| String::from_utf8_lossy(&c[..c.iter().position(|b| *b == b'=').unwrap()]).into_owned()).collect();
    assert_eq!(names, ["TITLE", "ARTIST", "PLATINUMNOTES"]);
    // Inspect and analyze say the same thing.
    let inspected = s.ok(&["inspect", text(&input)]);
    assert_eq!(inspected["results"][0]["report"]["input_format"], "opus/opus");
    assert!(inspected["results"][0]["report"]["no_platinum_notes_reference"].is_string());
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PLATINUM_TEST_OPUS_LIB and PLATINUM_TEST_OPUS_SHA256"]
fn unsupported_and_damaged_opus_streams_get_typed_codes() {
    let s = enabled();
    let out = s.dir("out");
    let good = s.dir("in").join("good.opus");
    encode(&good, 2, 24_000);
    let bytes = fs::read(&good).unwrap();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("chained", [bytes.clone(), bytes.clone()].concat(), "unsupported_opus_variant"),
        ("truncated", bytes[..bytes.len() / 2].to_vec(), "invalid_input_file"),
        // Not Ogg at all: the container cannot be established, so absence of a marker cannot be either.
        ("junk", b"not an ogg stream at all".to_vec(), "marker_unverifiable"),
        ("empty", Vec::new(), "marker_unverifiable"),
    ];
    for (name, data, code) in cases {
        let path = s.file(&format!("bad/{name}.opus"), &data);
        let (_, outcome) = s.run(&["process", text(&path), "--format", "match-input", "--output-dir", text(&out), "--jobs", "1"]);
        assert_eq!(item_code(&outcome), code, "{name}: {outcome}");
    }
    assert_eq!(fs::read_dir(&out).unwrap().count(), 0, "failures publish nothing");
    // Mapping family 1 (surround) and an Ogg stream of another codec: both are first pages only,
    // because the OpusHead check refuses them before anything else is read.
    let single = |body: Vec<u8>| opus::ogg::Page { flags: opus::ogg::FLAG_BOS, granule: 0, serial: 1, sequence: 0,
        lacing: opus::ogg::lacing_for(body.len()).collect(), body, offset: 0 };
    let mut head = opus::OpusHead { version: 1, channels: 2, pre_skip: 312, input_sample_rate: 48_000, output_gain: 0 }.encode();
    head[18] = 1;
    let first = single(head);
    let vorbis = single(b"\x01vorbis\0\0\0\0".to_vec());
    for (name, page, code) in [("family1", first, "unsupported_opus_variant"), ("vorbis", vorbis, "unsupported_input_codec")] {
        let path = s.file(&format!("bad/{name}.opus"), &page.encode());
        let (_, outcome) = s.run(&["process", text(&path), "--format", "match-input", "--output-dir", text(&out), "--jobs", "1"]);
        assert_eq!(item_code(&outcome), code, "{name}: {outcome}");
    }
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PLATINUM_TEST_OPUS_LIB and PLATINUM_TEST_OPUS_SHA256"]
fn opus_output_is_refused_without_conversion_for_other_rates_and_layouts() {
    let s = enabled();
    let out = s.dir("out");
    for (name, rate, channels, code) in [("r44", 44_100u32, 2u16, "unsupported_sample_rate_conversion"), ("c6", 48_000, 6, "unsupported_opus_variant")] {
        let path = s.file(&format!("in/{name}.wav"), &wav(rate, 1, channels));
        let (_, outcome) = s.run(&["process", text(&path), "--format", "opus", "--output-dir", text(&out), "--jobs", "1"]);
        assert_eq!(item_code(&outcome), code, "{name}: {outcome}");
    }
    // A 48 kHz stereo WAV converts, and an unprocessed Opus file converts back to PCM
    // (a processed one carries the marker and is refused as already processed).
    let path = s.file("in/ok.wav", &wav(48_000, 2, 2));
    s.ok(&["process", text(&path), "--format", "opus", "--output-dir", text(&out), "--jobs", "1"]);
    assert!(out.join("ok_PN.opus").is_file());
    let source = s.dir("in2").join("src.opus");
    encode(&source, 2, 48_000);
    let back = s.dir("back");
    let outcome = s.ok(&["process", text(&source), "--format", "wav", "--output-dir", text(&back), "--jobs", "1"]);
    assert_eq!(outcome["results"][0]["status"], "ok", "{outcome}");
    assert!(back.join("src_PN.wav").is_file());
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE"]
fn a_missing_opus_library_is_reported_as_not_configured() {
    let s = Sandbox::new();
    s.set("libraries.tag_bridge", json!(std::env::var("PLATINUM_TEST_TAG_BRIDGE").unwrap()));
    s.set("formats.opus", json!(true));
    let input = s.file("in/a.opus", &fake_opus(3));
    let out = s.dir("out");
    let (_, outcome) = s.run(&["process", text(&input), "--format", "match-input", "--output-dir", text(&out), "--jobs", "1"]);
    assert_eq!(item_code(&outcome), "library_not_configured", "{outcome}");
}
