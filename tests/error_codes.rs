//! CLI-level scenarios for the stable machine-readable `code` that every failed
//! item, batch and top-level error carries (`research/CLI_CONTRACT.md`, "Error
//! codes"). Each test runs the real `platinum` binary against generated audio
//! in an isolated HOME and state directory and parses the JSON outcome.
//!
//! Codes that need a native tag bridge are `#[ignore]`d and read the bridge from
//! `PLATINUM_TEST_TAG_BRIDGE`. `every_error_code_has_a_documented_scenario`
//! keeps the list of scenarios in step with `ErrorCode::ALL`.

use platinum_engine::error_code::ErrorCode;
use platinum_engine::statistics::StatisticsStore;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Generated audio is 16-bit stereo at 44.1 kHz so native macOS and portable
/// (Linux) builds both accept it without sample-rate conversion.
const RATE: u32 = 44_100;

struct Sandbox { root: PathBuf, home: PathBuf, state: PathBuf }
impl Sandbox {
    fn new() -> Self {
        let root = loop {
            let candidate = std::env::temp_dir().join(format!("platinum-error-codes-{}-{}", std::process::id(), NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)));
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
    /// A sandbox whose settings name the real tag bridge for bridge-gated scenarios.
    fn with_bridge() -> Self {
        let sandbox = Self::new();
        let bridge = std::env::var("PLATINUM_TEST_TAG_BRIDGE").expect("provide public tag bridge path");
        sandbox.ok(&["config", "set", "libraries.tag_bridge", &json!(bridge).to_string()]);
        sandbox
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
}
impl Drop for Sandbox { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }

fn text(path: &Path) -> &str { path.to_str().unwrap() }

/// A 16-bit stereo PCM WAV: a 440 Hz sine, or digital silence.
fn generated_wav(seconds: u32, silent: bool) -> Vec<u8> {
    let frames = RATE * seconds;
    let data_len = frames * 4;
    let mut wav = Vec::with_capacity((44 + data_len) as usize);
    wav.extend_from_slice(b"RIFF"); wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt "); wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes()); wav.extend_from_slice(&(RATE * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes()); wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data"); wav.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let sample = if silent { 0 } else {
            ((frame as f32 * 2.0 * std::f32::consts::PI * 440.0 / RATE as f32).sin() * 6000.0) as i16
        };
        wav.extend_from_slice(&sample.to_le_bytes());
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// RIFF/WAVE with a well-formed `fmt ` chunk that declares zero channels, then
/// a data chunk. Marker inspection accepts the container; every decoder refuses it.
fn zero_channel_wav() -> Vec<u8> {
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF"); wav.extend_from_slice(&(36u32 + 400).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt "); wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); wav.extend_from_slice(&0u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes()); wav.extend_from_slice(&(RATE * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes()); wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data"); wav.extend_from_slice(&400u32.to_le_bytes());
    wav.extend_from_slice(&[0u8; 400]);
    wav
}

/// The item error object at `results[index]`, with its code and optional stage checked.
fn assert_item_error(outcome: &Value, index: usize, stage: Option<&str>, code: &str) {
    let result = &outcome["results"][index];
    let error = &result["error"];
    assert_eq!(error["code"], code, "results[{index}]: {outcome}");
    if let Some(stage) = stage { assert_eq!(error["stage"], stage, "results[{index}]: {outcome}"); }
    assert!(error["message"].as_str().is_some_and(|message| !message.is_empty()), "results[{index}] has no message: {outcome}");
    assert!(matches!(result["status"].as_str(), Some("failed" | "not_started")), "results[{index}]: {outcome}");
}

/// The result for one input path, whatever its position in `results`.
fn result_for<'a>(outcome: &'a Value, input: &Path) -> &'a Value {
    outcome["results"].as_array().unwrap().iter().find(|result| result["input"] == text(input))
        .unwrap_or_else(|| panic!("no result for {}: {outcome}", input.display()))
}

// --- Library-free scenarios -------------------------------------------------

#[test]
fn usage_is_reported_for_unknown_commands_and_conflicting_options() {
    let sandbox = Sandbox::new();
    let output = sandbox.dir("out");
    let conflict = ["process", "x", "--output-dir", text(&output), "--same-folder"];
    // The last case fails in the command itself rather than in argument parsing.
    let mismatched_backups = ["restore", "a.wav", "b.wav", "--backup", "/synthetic/backup.wav"];
    for args in [&["not-a-command"][..], &conflict[..], &mismatched_backups[..]] {
        let outcome = sandbox.fails(2, args);
        assert_eq!(outcome["status"], "error", "{args:?}: {outcome}");
        assert_eq!(outcome["error"]["kind"], "usage", "{args:?}: {outcome}");
        assert_eq!(outcome["error"]["code"], "usage", "{args:?}: {outcome}");
        assert!(outcome["error"]["message"].as_str().is_some_and(|message| !message.is_empty()), "{args:?}: {outcome}");
    }
}

#[test]
fn settings_error_is_reported_for_an_unknown_config_field() {
    let sandbox = Sandbox::new();
    let outcome = sandbox.fails(1, &["config", "set", "not.a.field", "1"]);
    assert_eq!(outcome["status"], "error", "{outcome}");
    assert_eq!(outcome["error"]["kind"], "settings", "{outcome}");
    assert_eq!(outcome["error"]["code"], "settings_error", "{outcome}");
}

#[test]
fn no_input_files_is_reported_for_a_directory_without_audio() {
    let sandbox = Sandbox::new();
    let empty = sandbox.dir("empty");
    let outcome = sandbox.fails(1, &["process", text(&empty)]);
    assert_eq!(outcome["status"], "failed", "{outcome}");
    assert_eq!(outcome["results"].as_array().unwrap().len(), 1, "{outcome}");
    assert_item_error(&outcome, 0, Some("discovery"), "no_input_files");
    assert_eq!(outcome["results"][0]["status"], "failed");

    // Files that are not admitted audio do not count as input either.
    let notes = sandbox.file("notes/readme.txt", b"not audio");
    let outcome = sandbox.fails(1, &["process", text(notes.parent().unwrap())]);
    assert_item_error(&outcome, 0, Some("discovery"), "no_input_files");
}

#[test]
fn invalid_input_path_is_reported_for_a_symlink_input() {
    let sandbox = Sandbox::new();
    let target = sandbox.file("target.wav", &generated_wav(1, false));
    let link = sandbox.root.join("link.wav");
    symlink(&target, &link).unwrap();
    let outcome = sandbox.fails(1, &["process", text(&link)]);
    assert_eq!(outcome["results"][0]["status"], "failed", "{outcome}");
    assert_item_error(&outcome, 0, None, "invalid_input_path");
}

#[test]
fn invalid_input_path_is_reported_for_a_symlink_that_inspect_pins() {
    let sandbox = Sandbox::new();
    let target = sandbox.file("target.wav", &generated_wav(1, false));
    let link = sandbox.root.join("link.wav");
    symlink(&target, &link).unwrap();
    let outcome = sandbox.fails(1, &["inspect", text(&link)]);
    assert_eq!(outcome["results"][0]["status"], "failed", "{outcome}");
    assert_item_error(&outcome, 0, Some("inspect source identity"), "invalid_input_path");
}

#[test]
fn invalid_input_path_is_reported_for_a_fifo_that_inspect_never_blocks_on() {
    let sandbox = Sandbox::new();
    let fifo = sandbox.root.join("pipe.wav");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `name` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "mkfifo");
    let outcome = sandbox.fails(1, &["inspect", text(&fifo)]);
    assert_eq!(outcome["results"][0]["status"], "failed", "{outcome}");
    assert_item_error(&outcome, 0, Some("inspect source identity"), "invalid_input_path");
}

#[test]
fn io_error_is_reported_for_a_path_that_does_not_exist() {
    let sandbox = Sandbox::new();
    let missing = sandbox.root.join("missing.wav");
    let outcome = sandbox.fails(1, &["process", text(&missing)]);
    assert_eq!(outcome["results"][0]["status"], "failed", "{outcome}");
    assert_item_error(&outcome, 0, None, "io_error");
}

#[test]
fn backup_missing_is_reported_for_a_supplied_backup_that_does_not_exist() {
    let sandbox = Sandbox::new();
    let wav = sandbox.file("processed.wav", &generated_wav(1, false));
    let gone = sandbox.root.join("gone/backup.wav");
    assert!(gone.is_absolute() && !gone.exists());
    let outcome = sandbox.fails(1, &["restore", text(&wav), "--backup", text(&gone)]);
    assert_eq!(outcome["status"], "error", "{outcome}");
    assert_eq!(outcome["error"]["code"], "backup_missing", "{outcome}");
    assert!(outcome["error"]["message"].as_str().is_some_and(|message| message.contains("supplied backup not found")), "{outcome}");
    assert!(wav.exists(), "nothing is consumed");
}

#[test]
fn library_not_configured_is_reported_without_a_tag_bridge() {
    let sandbox = Sandbox::new();
    let wav = sandbox.file("tone.wav", &generated_wav(1, false));
    for command in ["process", "inspect", "analyze"] {
        let outcome = sandbox.fails(1, &[command, text(&wav)]);
        assert_eq!(outcome["command"], command, "{outcome}");
        assert_eq!(outcome["results"][0]["status"], "failed", "{command}: {outcome}");
        assert_item_error(&outcome, 0, Some("tags"), "library_not_configured");
    }
}

#[test]
fn unsupported_input_format_is_reported_for_ogg_sources() {
    let sandbox = Sandbox::new();
    let mut ogg = b"OggS\0\x02".to_vec();
    ogg.extend_from_slice(&[0u8; 64]);
    let ogg = sandbox.file("a.ogg", &ogg);
    let outcome = sandbox.fails(1, &["process", text(&ogg)]);
    assert_item_error(&outcome, 0, Some("input"), "unsupported_input_format");
}

#[test]
fn protected_source_is_reported_for_m4p_sources() {
    let sandbox = Sandbox::new();
    let m4p = sandbox.file("a.m4p", b"synthetic protected source");
    let outcome = sandbox.fails(1, &["process", text(&m4p)]);
    assert_item_error(&outcome, 0, Some("input"), "protected_source");
}

#[test]
fn batch_preflight_failed_is_reported_when_overwrite_meets_an_undiscoverable_input() {
    let sandbox = Sandbox::new();
    let valid = sandbox.file("valid.wav", &generated_wav(1, false));
    let missing = sandbox.root.join("missing.wav");
    let output = sandbox.dir("out");
    let outcome = sandbox.fails(1, &["process", text(&valid), text(&missing), "--overwrite", "--format", "wav", "--output-dir", text(&output)]);
    assert_eq!(outcome["batch_error"]["code"], "batch_preflight_failed", "{outcome}");
    assert!(outcome["batch_error"]["message"].as_str().is_some_and(|message| !message.is_empty()), "{outcome}");
    // The undiscoverable input keeps its own cause.
    assert_eq!(result_for(&outcome, &missing)["error"]["code"], "io_error", "{outcome}");
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0, "nothing may be written");
}

#[test]
fn statistics_error_is_reported_for_a_corrupt_statistics_file() {
    let sandbox = Sandbox::new();
    // Create the state directory with its private permissions first.
    sandbox.ok(&["config", "show"]);
    let path = StatisticsStore::new(&sandbox.state).statistics_path();
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).unwrap();
    file.write_all(b"this is not a statistics document").unwrap();
    drop(file);
    let outcome = sandbox.fails(1, &["statistics", "show"]);
    assert_eq!(outcome["status"], "error", "{outcome}");
    assert_eq!(outcome["error"]["kind"], "statistics", "{outcome}");
    assert_eq!(outcome["error"]["code"], "statistics_error", "{outcome}");
    assert_eq!(fs::read(&path).unwrap(), b"this is not a statistics document", "malformed state is never overwritten");
}

/// Needs no real library: the configured "bridge" is a text file, which no
/// platform's loader or library policy accepts. Where a loader or policy
/// decides differs by platform, so the expected code does too.
#[test]
fn a_bridge_that_is_not_an_approved_library_is_refused_with_a_library_code() {
    let sandbox = Sandbox::new();
    let fake = sandbox.file("fake-bridge.so", b"this is not a shared library");
    sandbox.ok(&["config", "set", "libraries.tag_bridge", &json!(text(&fake)).to_string()]);
    let wav = sandbox.file("tone.wav", &generated_wav(1, false));
    // Linux checks the approved-library digest before loading; macOS reaches the loader.
    #[cfg(target_os = "linux")]
    const EXPECTED: &str = "library_unapproved";
    #[cfg(not(target_os = "linux"))]
    const EXPECTED: &str = "library_load_failed";
    let outcome = sandbox.fails(1, &["process", text(&wav)]);
    assert_item_error(&outcome, 0, Some("tags"), EXPECTED);
}

/// A configured library that cannot be loaded is `library_load_failed` however it fails: a path that does not exist
/// and a directory behave the same on every platform (a text file differs by platform, see above).
#[test]
fn a_missing_or_directory_bridge_path_is_library_load_failed() {
    let sandbox = Sandbox::new();
    let wav = sandbox.file("tone.wav", &generated_wav(1, false));
    for path in [sandbox.root.join("no/such/bridge.so"), sandbox.dir("bridge-directory")] {
        sandbox.ok(&["config", "set", "libraries.tag_bridge", &json!(text(&path)).to_string()]);
        for command in ["process", "inspect", "analyze"] {
            let outcome = sandbox.fails(1, &[command, text(&wav)]);
            assert_item_error(&outcome, 0, Some("tags"), "library_load_failed");
        }
    }
}

/// The same for the FLAC and LAME encoder libraries; the bridge loads, so the encoder load is the first failure.
#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn a_missing_or_directory_codec_library_path_is_library_load_failed() {
    let sandbox = Sandbox::with_bridge();
    let wav = sandbox.file("tone.wav", &generated_wav(1, false));
    let out = sandbox.dir("out");
    let pin = json!("0".repeat(64)).to_string();
    for (format, library, library_pin, stage) in [
        ("flac", "libraries.flac", "libraries.flac_sha256", "load FLAC encoder"),
        ("mp3", "libraries.lame", "libraries.lame_sha256", "load MP3 encoder"),
    ] {
        sandbox.ok(&["config", "set", library_pin, &pin]);
        for path in [sandbox.root.join("no/such/library.so"), sandbox.dir(&format!("{format}-library-directory"))] {
            sandbox.ok(&["config", "set", library, &json!(text(&path)).to_string()]);
            let outcome = sandbox.fails(1, &["process", text(&wav), "--format", format, "--output-dir", text(&out)]);
            assert_item_error(&outcome, 0, Some(stage), "library_load_failed");
        }
    }
    assert_eq!(fs::read_dir(&out).unwrap().count(), 0, "a failed library load writes nothing");
}

// --- Bridge-gated scenarios (--format wav only: no FLAC or LAME library) -----

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn marker_unverifiable_is_reported_for_a_wav_without_a_recognizable_container() {
    let sandbox = Sandbox::with_bridge();
    let junk = sandbox.file("x.wav", b"not audio");
    let output = sandbox.dir("out");
    let outcome = sandbox.fails(1, &["process", text(&junk), "--format", "wav", "--output-dir", text(&output)]);
    assert_item_error(&outcome, 0, Some("inspect tags"), "marker_unverifiable");
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn invalid_input_file_is_reported_for_a_wav_no_decoder_accepts() {
    let sandbox = Sandbox::with_bridge();
    let bad = sandbox.file("zero-channels.wav", &zero_channel_wav());
    let output = sandbox.dir("out");
    // Marker inspection accepts the RIFF container; the decoder then refuses it.
    for args in [
        vec!["inspect", text(&bad)],
        vec!["process", text(&bad), "--format", "wav", "--output-dir", text(&output)],
    ] {
        let outcome = sandbox.fails(1, &args);
        assert_item_error(&outcome, 0, Some("open input"), "invalid_input_file");
    }
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn already_processed_is_reported_when_an_output_is_processed_again() {
    let sandbox = Sandbox::with_bridge();
    let source = sandbox.file("song.wav", &generated_wav(2, false));
    let first = sandbox.dir("first");
    let second = sandbox.dir("second");
    let processed = sandbox.ok(&["process", text(&source), "--format", "wav", "--output-dir", text(&first)]);
    let output = processed["results"][0]["report"]["output_path"].as_str().unwrap();
    let outcome = sandbox.fails(1, &["process", output, "--format", "wav", "--output-dir", text(&second)]);
    assert_eq!(outcome["results"][0]["status"], "failed", "{outcome}");
    assert_item_error(&outcome, 0, None, "already_processed");
    assert_eq!(fs::read_dir(&second).unwrap().count(), 0);
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn output_exists_is_reported_when_an_output_would_be_replaced_without_overwrite() {
    let sandbox = Sandbox::with_bridge();
    let source = sandbox.file("song.wav", &generated_wav(2, false));
    let output = sandbox.dir("out");
    let args = ["process", text(&source), "--format", "wav", "--output-dir", text(&output)];
    let first = sandbox.ok(&args);
    let path = first["results"][0]["report"]["output_path"].as_str().unwrap();
    let published = fs::read(path).unwrap();
    let outcome = sandbox.fails(1, &args);
    assert_eq!(outcome["results"][0]["status"], "failed", "{outcome}");
    assert_item_error(&outcome, 0, None, "output_exists");
    assert_eq!(fs::read(path).unwrap(), published, "the existing output is untouched");
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn marker_ambiguous_is_reported_for_an_empty_wav_marker_unless_overridden() {
    let sandbox = Sandbox::with_bridge();
    let silent = sandbox.file("silent.wav", &generated_wav(2, true));
    let first = sandbox.dir("first");
    let second = sandbox.dir("second");
    let third = sandbox.dir("third");
    let fourth = sandbox.dir("fourth");
    let processed = sandbox.ok(&["process", text(&silent), "--format", "wav", "--output-dir", text(&first)]);
    assert_eq!(processed["results"][0]["report"]["processing_marker_status"], "nil_payload_empty_wav_chunk", "{processed}");
    let output = processed["results"][0]["report"]["output_path"].as_str().unwrap();
    // The same bytes without the `_PN` suffix that the engine gave the output.
    let renamed = sandbox.root.join("renamed.wav");
    fs::copy(output, &renamed).unwrap();

    for input in [output, text(&renamed)] {
        let outcome = sandbox.fails(1, &["process", input, "--format", "wav", "--output-dir", text(&second)]);
        assert_eq!(outcome["results"][0]["status"], "failed", "{outcome}");
        assert_item_error(&outcome, 0, Some("decode marker"), "marker_ambiguous");
    }
    assert_eq!(fs::read_dir(&second).unwrap().count(), 0);

    // The override lifts only this refusal: the `_PN` suffix still marks the engine's own
    // output as processed, while the renamed copy is reprocessed.
    let outcome = sandbox.fails(1, &["process", output, "--format", "wav", "--output-dir", text(&third), "--allow-ambiguous-marker"]);
    assert_item_error(&outcome, 0, None, "already_processed");
    assert_eq!(fs::read_dir(&third).unwrap().count(), 0);
    let overridden = sandbox.ok(&["process", text(&renamed), "--format", "wav", "--output-dir", text(&fourth), "--allow-ambiguous-marker"]);
    assert_eq!(overridden["results"][0]["status"], "ok", "{overridden}");
    assert_eq!(overridden["details"]["input_marker_overrides"][0]["kind"], "empty_wav_pltn", "{overridden}");
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn restore_not_applicable_is_reported_for_a_file_that_was_never_processed() {
    let sandbox = Sandbox::with_bridge();
    let wav = generated_wav(2, false);
    let plain = sandbox.file("plain.wav", &wav);
    let backup = sandbox.file("backup.wav", &wav);
    assert!(backup.is_absolute());
    let outcome = sandbox.fails(1, &["restore", text(&plain), "--backup", text(&backup)]);
    assert_eq!(outcome["results"][0]["status"], "failed", "{outcome}");
    assert_item_error(&outcome, 0, None, "restore_not_applicable");
    assert_eq!(fs::read(&plain).unwrap(), wav);
    assert_eq!(fs::read(&backup).unwrap(), wav, "the backup is only consumed by a verified restore");
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn output_conflict_is_reported_when_two_inputs_share_one_destination() {
    let sandbox = Sandbox::with_bridge();
    let wav = generated_wav(2, false);
    let left = sandbox.file("left/song.wav", &wav);
    let right = sandbox.file("right/song.wav", &wav);
    let output = sandbox.dir("out");
    let outcome = sandbox.fails(1, &["process", text(left.parent().unwrap()), text(right.parent().unwrap()),
        "--no-mirror", "--format", "wav", "--output-dir", text(&output)]);
    assert_eq!(outcome["batch_error"]["code"], "output_conflict", "{outcome}");
    let results = outcome["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{outcome}");
    for index in 0..results.len() {
        assert_eq!(results[index]["status"], "not_started", "{outcome}");
        assert_item_error(&outcome, index, None, "batch_preflight_failed");
    }
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0, "a preflight failure writes nothing");
    assert_eq!(fs::read(&left).unwrap(), wav);
    assert_eq!(fs::read(&right).unwrap(), wav);
}

// --- Coverage ledger ---------------------------------------------------------

/// Where every code in `ErrorCode::ALL` is exercised: the name of a test in
/// this file, or a pointer (`src/...`, `tests/...`) to the unit or integration
/// test that covers a code this file cannot provoke through the CLI.
const LEDGER: &[(&str, &str)] = &[
    // Input admission and capability.
    // What the portable decoders still refuse: HE-AAC in an MP4 and AAC in a WAVE container (`aac_streams_the_decoder_does_not_cover_are_refused_and_a_stub_is_an_invalid_file`,
    // `failed_items_have_no_report`). Plain AAC-LC input is decoded since v0.4.0 and no longer produces this code; a stub `.m4a` with no sample tables is `invalid_input_file`.
    ("unsupported_input_codec", "tests/linux_opt_in.rs"),
    ("unsupported_input_format", "unsupported_input_format_is_reported_for_ogg_sources"),
    // Emitted by no build: the AAC encoder is exact on Linux and AudioToolbox is used on macOS, so no codec path returns an encoding refusal any more.
    // The code stays in the contract; the mapping from `UnsupportedCodec { operation: "encoding" }` is the unit test in `src/error_code.rs`.
    ("unsupported_output_codec", "src/error_code.rs"),
    ("unsupported_output_format", "tests/linux_opt_in.rs"),
    ("unsupported_sample_rate_conversion", "tests/linux_opt_in.rs"),
    ("artwork_conversion_unavailable", "tests/linux_opt_in.rs"),
    ("protected_source", "protected_source_is_reported_for_m4p_sources"),
    ("invalid_input_file", "invalid_input_file_is_reported_for_a_wav_no_decoder_accepts"),
    ("invalid_input_path", "invalid_input_path_is_reported_for_a_symlink_input"),
    ("no_input_files", "no_input_files_is_reported_for_a_directory_without_audio"),
    // Opt-in formats.
    ("opus_not_enabled", "tests/opus_cli.rs"),
    ("unsupported_opus_variant", "tests/opus_cli.rs"),
    // Processed-marker safety.
    ("already_processed", "already_processed_is_reported_when_an_output_is_processed_again"),
    ("marker_ambiguous", "marker_ambiguous_is_reported_for_an_empty_wav_marker_unless_overridden"),
    ("marker_unverifiable", "marker_unverifiable_is_reported_for_a_wav_without_a_recognizable_container"),
    ("marker_invalid", "src/error_code.rs"),
    // Tags.
    ("tag_copy_failed", "src/error_code.rs"),
    // Native libraries: macOS loads (and fails to load) the file; Linux checks its digest first.
    ("library_not_configured", "library_not_configured_is_reported_without_a_tag_bridge"),
    ("library_unapproved", "a_bridge_that_is_not_an_approved_library_is_refused_with_a_library_code"),
    ("library_load_failed", "a_missing_or_directory_bridge_path_is_library_load_failed"),
    // Codec and processing runtime.
    ("decode_failed", "src/error_code.rs"),
    ("encode_failed", "src/error_code.rs"),
    ("processing_failed", "src/error_code.rs"),
    // Files and transactions.
    ("io_error", "io_error_is_reported_for_a_path_that_does_not_exist"),
    ("atomic_rename_unsupported", "src/main.rs (error_code_json_tests)"),
    ("output_exists", "output_exists_is_reported_when_an_output_would_be_replaced_without_overwrite"),
    ("output_conflict", "output_conflict_is_reported_when_two_inputs_share_one_destination"),
    ("source_changed", "src/main.rs (error_code_json_tests)"),
    ("backup_missing", "backup_missing_is_reported_for_a_supplied_backup_that_does_not_exist"),
    ("backup_invalid", "src/error_code.rs"),
    ("restore_not_applicable", "restore_not_applicable_is_reported_for_a_file_that_was_never_processed"),
    // Batch and command level.
    ("batch_preflight_failed", "batch_preflight_failed_is_reported_when_overwrite_meets_an_undiscoverable_input"),
    ("report_serialization_failed", "src/main.rs (error_code_json_tests)"),
    ("cancelled", "src/main.rs (error_code_json_tests)"),
    ("usage", "usage_is_reported_for_unknown_commands_and_conflicting_options"),
    ("settings_error", "settings_error_is_reported_for_an_unknown_config_field"),
    ("statistics_error", "statistics_error_is_reported_for_a_corrupt_statistics_file"),
    ("signal_handler_failed", "src/main.rs (error_code_json_tests)"),
    ("internal_error", "src/error_code.rs"),
];

#[test]
fn every_error_code_has_a_documented_scenario() {
    let mut ledger: Vec<&str> = LEDGER.iter().map(|(code, _)| *code).collect();
    ledger.sort_unstable();
    let duplicates: Vec<_> = ledger.windows(2).filter(|pair| pair[0] == pair[1]).map(|pair| pair[0]).collect();
    assert!(duplicates.is_empty(), "codes listed twice in LEDGER: {duplicates:?}");
    let mut defined: Vec<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
    defined.sort_unstable();
    assert_eq!(ledger, defined, "LEDGER and ErrorCode::ALL must name exactly the same codes");

    let source = include_str!("error_codes.rs");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (code, location) in LEDGER {
        if let Some(path) = location.split_whitespace().next().filter(|first| first.contains('/')) {
            assert!(root.join(path).is_file(), "{code}: pointer {path} does not exist");
        } else {
            let declaration = format!("fn {location}()");
            let at = source.find(&declaration).unwrap_or_else(|| panic!("{code}: no test named {location} in this file"));
            // The attribute block directly above the function must include #[test].
            let marked = source[..at].lines().rev()
                .take_while(|line| { let line = line.trim_start(); line.starts_with("#[") || line.starts_with("//") })
                .any(|line| line.trim() == "#[test]");
            assert!(marked, "{code}: {location} is not a #[test]");
        }
    }
}
