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
            let candidate = std::env::temp_dir().join(format!("platinum-cli-test-{}-{}", std::process::id(), NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated test directory: {error}"),
            }
        };
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        let state = root.join("state");
        Self { root, home, state }
    }
    fn run_in(&self, state: &Path, args: &[&str]) -> (Output, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_platinum"))
            .env("HOME", &self.home).arg("--state-dir").arg(state).arg("--json").args(args)
            .output().unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!("invalid JSON outcome: {error}; stdout={:?}; stderr={:?}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
        });
        (output, value)
    }
    fn run(&self, args: &[&str]) -> (Output, Value) { self.run_in(&self.state, args) }
    fn ok(&self, args: &[&str]) -> Value {
        let (output, value) = self.run(args);
        assert!(output.status.success(), "{args:?}: {value}; stderr={}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(value["status"], "ok");
        value
    }
}
impl Drop for Sandbox { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }

#[test]
fn help_version_and_machine_usage_error() {
    let help = Command::new(env!("CARGO_BIN_EXE_platinum")).arg("--help").output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    for command in ["process", "analyze", "inspect", "restore", "templates", "config", "provenance"] {
        assert!(text.contains(command), "help missing {command}");
    }
    let sandbox = Sandbox::new();
    let version = sandbox.ok(&["version"]);
    assert_eq!(version["data"]["version"], env!("CARGO_PKG_VERSION"));
    let (output, error) = sandbox.run(&["not-a-command"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(error["error"]["kind"], "usage");
}

#[test]
fn config_is_persistent_isolated_and_run_overrides_are_ephemeral() {
    let sandbox = Sandbox::new();
    let original = sandbox.ok(&["config", "get", "output.format"]);
    assert_eq!(original["data"]["value"], "mp3");
    sandbox.ok(&["config", "set", "output.format", "\"wav\""]);
    sandbox.ok(&["config", "set", "processor_max", "1"]);
    let missing = sandbox.root.join("missing.wav");
    let (output, value) = sandbox.run(&["process", missing.to_str().unwrap(), "--format", "flac"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["status"], "failed");
    assert_eq!(value["details"]["effective_config"]["output"]["format"], "flac");
    assert_eq!(sandbox.ok(&["config", "get", "output.format"])["data"]["value"], "wav");
    let (output, analyzed) = sandbox.run(&["analyze", missing.to_str().unwrap(), "--jobs", "999"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(analyzed["details"]["jobs"], 1);
    assert_eq!(analyzed["details"]["requested_jobs"], 999);
    let other = sandbox.root.join("other-state");
    let (output, isolated) = sandbox.run_in(&other, &["config", "get", "output.format"]);
    assert!(output.status.success());
    assert_eq!(isolated["data"]["value"], "mp3");
    sandbox.ok(&["config", "reset", "output.format"]);
    assert_eq!(sandbox.ok(&["config", "get", "output.format"])["data"]["value"], "mp3");
}

#[test]
fn complete_template_workflow_exposes_effective_and_serialized_values() {
    let sandbox = Sandbox::new();
    let list = sandbox.ok(&["templates", "list"]);
    let builtin_count = list["data"].as_array().unwrap().len();
    assert!(builtin_count >= 1);
    let clone = sandbox.ok(&["templates", "clone", "--index", "0"]);
    assert_eq!(clone["data"]["index"], builtin_count);
    let index = builtin_count.to_string();
    sandbox.ok(&["templates", "edit", "--index", &index, "clip.percentage", "12.5"]);
    sandbox.ok(&["templates", "edit", "--index", &index, "expansion.bands[1].threshold", "-12.5"]);
    sandbox.ok(&["templates", "edit", "--index", &index, "pitch.enabled", "true"]);
    sandbox.ok(&["templates", "rename", "--index", &index, "Isolated custom"]);
    sandbox.ok(&["templates", "select", "--index", &index]);
    let shown = sandbox.ok(&["templates", "show", "--index", &index]);
    assert_eq!(shown["data"]["effective"]["name"], "Isolated custom");
    assert_eq!(shown["data"]["effective"]["clip"]["percentage"], 12.5);
    assert_eq!(shown["data"]["effective"]["expansion"]["bands"][1]["threshold"], -12.5);
    assert_eq!(shown["data"]["serialized"]["clip"]["percentage"], "0x41480000");
    assert_eq!(sandbox.ok(&["templates", "list"])["data"][builtin_count]["selected"], true);
    let (output, error) = sandbox.run(&["templates", "rename", "--index", "0", "Forbidden"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(error["status"], "error");
    sandbox.ok(&["config", "reset"]);
    assert_eq!(sandbox.ok(&["templates", "list"])["data"].as_array().unwrap().len(), builtin_count + 1);
    sandbox.ok(&["templates", "select", "--index", &index]);
    sandbox.ok(&["templates", "remove", "--index", &index]);
    let after = sandbox.ok(&["templates", "list"]);
    assert_eq!(after["data"].as_array().unwrap().len(), builtin_count);
    assert_eq!(after["data"][0]["selected"], true);
}

#[test]
fn template_export_import_reports_losses_and_never_clobbers() {
    let sandbox = Sandbox::new();
    let destination = sandbox.root.join("custom.pntemplate");
    let path = destination.to_str().unwrap();
    let exported = sandbox.ok(&["templates", "export", "--index", "0", path]);
    assert!(exported["data"]["report"]["copy_losses"].is_array());
    assert!(exported["data"]["report"]["serialization_losses"].is_array());
    let original = fs::read(&destination).unwrap();
    let (output, error) = sandbox.run(&["templates", "export", "--index", "0", path]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(error["status"], "error");
    assert_eq!(fs::read(&destination).unwrap(), original);
    let imported = sandbox.ok(&["templates", "import", path]);
    assert!(imported["data"]["indices"].as_array().unwrap().len() >= 1);
    assert!(imported["data"]["ignored_elements"].is_array());
    assert!(imported["data"]["reserialization_losses"].is_array());
}

#[test]
fn replacement_and_restore_require_explicit_unambiguous_pairs() {
    let sandbox = Sandbox::new();
    let backup = sandbox.root.join("backups");
    fs::create_dir(&backup).unwrap();
    let mode = json!({"replace":{"backup_directory":backup}}).to_string();
    sandbox.ok(&["config", "set", "output.mode", &mode]);
    let source = sandbox.root.join("missing.wav");
    let source = source.to_str().unwrap();
    let backup = backup.to_str().unwrap();
    let (output, error) = sandbox.run(&["process", source]);
    assert_eq!(output.status.code(), Some(2));
    assert!(error["error"]["message"].as_str().unwrap().contains("--replace-originals"));
    let (output, _) = sandbox.run(&["process", source, "--replace-originals"]);
    assert_eq!(output.status.code(), Some(2));
    let (output, error) = sandbox.run(&["process", source, "--replace-originals", "--backup-dir", backup, "--format", "wav"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(error["error"]["message"].as_str().unwrap().contains("match-input"));
    let (output, _) = sandbox.run(&["restore", "first.wav", "second.wav", "--backup", backup]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(sandbox.ok(&["config", "get", "output.format"])["data"]["value"], "match_input");
}

#[test]
fn explicit_missing_output_directory_is_not_created() {
    let sandbox = Sandbox::new();
    let source = sandbox.root.join("not-audio.wav");
    let output = sandbox.root.join("uncreated-output");
    fs::write(&source, b"not an audio file").unwrap();
    let (result, outcome) = sandbox.run(&["process", source.to_str().unwrap(), "--output-dir", output.to_str().unwrap()]);
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(outcome["status"], "failed");
    assert!(!output.exists());
    assert_eq!(fs::read(source).unwrap(), b"not an audio file");
}

#[cfg(unix)]
#[test]
fn non_unicode_input_is_a_truthful_machine_failure() {
    use std::os::unix::ffi::OsStringExt;
    let sandbox = Sandbox::new();
    let path = sandbox.root.join(std::ffi::OsString::from_vec(b"\xff.wav".to_vec()));
    let output = Command::new(env!("CARGO_BIN_EXE_platinum"))
        .env("HOME", &sandbox.home).arg("--state-dir").arg(&sandbox.state).arg("--json")
        .arg("analyze").arg(&path).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "failed");
    assert!(value["results"][0]["input"]["unix_bytes_hex"].as_str().unwrap().ends_with("ff2e776176"));
}

// The public tag bridge is a caller-supplied native dependency, not embedded in
// the crate. This test runs explicitly with a bridge built from this repository.
#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn synthetic_audio_analysis_processing_and_restore() {
    let sandbox = Sandbox::new();
    let bridge = std::env::var("PLATINUM_TEST_TAG_BRIDGE").expect("provide public tag bridge path");
    let bridge_json = json!(bridge).to_string();
    sandbox.ok(&["config", "set", "libraries.tag_bridge", &bridge_json]);

    const RATE: u32 = 48_000;
    const FRAMES: u32 = RATE * 3;
    let data_len = FRAMES * 4;
    let mut wav = Vec::with_capacity((44 + data_len) as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..FRAMES {
        let sample = ((frame as f32 * 2.0 * std::f32::consts::PI * 440.0 / RATE as f32).sin() * 6000.0) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    let source = sandbox.root.join("synthetic.wav");
    let replacement = sandbox.root.join("replacement.wav");
    let outputs = sandbox.root.join("outputs");
    let backups = sandbox.root.join("backups");
    fs::write(&source, &wav).unwrap();
    fs::write(&replacement, &wav).unwrap();
    fs::create_dir(&outputs).unwrap();
    fs::create_dir(&backups).unwrap();
    let source = source.to_str().unwrap();
    let replacement = replacement.to_str().unwrap();
    let outputs = outputs.to_str().unwrap();
    let backups = backups.to_str().unwrap();

    let analysis = sandbox.ok(&["analyze", source, "--jobs", "1"]);
    assert_eq!(analysis["results"][0]["report"]["frames"], FRAMES);
    assert_eq!(analysis["results"][0]["report"]["waveform"]["frames"], FRAMES);
    assert!(!analysis["results"][0]["report"]["waveform"]["bins"].as_array().unwrap().is_empty());
    assert!(analysis["results"][0]["report"]["clip_count"].is_null());
    let processed = sandbox.ok(&["process", source, "--format", "wav", "--output-dir", outputs, "--jobs", "1"]);
    let output_path = processed["results"][0]["report"]["output_path"].as_str().unwrap();
    let output_bytes = fs::read(output_path).unwrap();
    assert!(!output_bytes.is_empty());
    let inspection = sandbox.ok(&["inspect", output_path]);
    assert_eq!(inspection["results"][0]["report"]["processed"], true);
    assert!(inspection["results"][0]["report"]["tags"].is_object());
    let (collision, outcome) = sandbox.run(&["process", source, "--format", "wav", "--output-dir", outputs, "--jobs", "1"]);
    assert_eq!(collision.status.code(), Some(1));
    assert_eq!(outcome["results"][0]["status"], "failed");
    assert_eq!(fs::read(output_path).unwrap(), output_bytes);

    let replaced = sandbox.ok(&["process", replacement, "--replace-originals", "--backup-dir", backups, "--jobs", "1"]);
    let backup = replaced["results"][0]["report"]["backup_path"].as_str().unwrap();
    assert_eq!(fs::read(backup).unwrap(), wav);
    let comparison = sandbox.ok(&["analyze", replacement, "--jobs", "1"]);
    assert_eq!(comparison["results"][0]["report"]["processed"], true);
    assert_eq!(comparison["results"][0]["report"]["original_waveform"]["frames"], FRAMES);
    let restored = sandbox.ok(&["restore", replacement, "--backup", backup]);
    assert_eq!(restored["results"][0]["status"], "ok");
    assert_eq!(fs::read(replacement).unwrap(), wav);
    assert_eq!(fs::read(backup).unwrap(), wav);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn short_and_silent_wav_report_the_nil_marker_and_nonfinite_loudness() {
    let sandbox = Sandbox::new();
    let bridge = std::env::var("PLATINUM_TEST_TAG_BRIDGE").expect("provide public tag bridge path");
    sandbox.ok(&["config", "set", "libraries.tag_bridge", &json!(bridge).to_string()]);
    for (name, frames, silent) in [("short.wav", 513u32, false), ("silent.wav", 48_000u32, true)] {
        let data_len = frames * 4;
        let mut wav = Vec::with_capacity((44 + data_len) as usize);
        wav.extend_from_slice(b"RIFF"); wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt "); wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes()); wav.extend_from_slice(&192_000u32.to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes()); wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data"); wav.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let sample = if silent { 0 } else {
                ((frame as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 6000.0) as i16
            };
            wav.extend_from_slice(&sample.to_le_bytes()); wav.extend_from_slice(&sample.to_le_bytes());
        }
        let source = sandbox.root.join(name);
        fs::write(&source, &wav).unwrap();
        let result = sandbox.ok(&["process", source.to_str().unwrap(), "--format", "wav", "--same-folder", "--jobs", "1"]);
        let report = &result["results"][0]["report"];
        assert_eq!(report["processing_marker_status"], "nil_payload_empty_wav_chunk");
        assert_eq!(report["input_volume"], "-Infinity");
        let output = fs::read(report["output_path"].as_str().unwrap()).unwrap();
        assert!(output.windows(8).any(|bytes| bytes == b"PLTN\0\0\0\0"));
        assert_eq!(fs::read(source).unwrap(), wav);
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE, PN_FLAC_LIB and PN_FLAC_SHA256"]
fn restore_marked_flac_without_a_configured_decoder() {
    use platinum_engine::flac::{FlacEncoder, FlacLibrary};
    let sandbox = Sandbox::new();
    let bridge = std::env::var("PLATINUM_TEST_TAG_BRIDGE").expect("provide public tag bridge path");
    let flac = std::env::var("PN_FLAC_LIB").expect("provide trusted FLAC 1.4.2");
    let sha256 = std::env::var("PN_FLAC_SHA256").expect("provide FLAC SHA-256 pin");
    sandbox.ok(&["config", "set", "libraries.tag_bridge", &json!(bridge).to_string()]);
    sandbox.ok(&["config", "set", "libraries.flac", &json!(&flac).to_string()]);
    sandbox.ok(&["config", "set", "libraries.flac_sha256", &json!(sha256).to_string()]);
    let source = sandbox.root.join("source.flac");
    let backups = sandbox.root.join("backups");
    fs::create_dir(&backups).unwrap();
    let library = FlacLibrary::load(&flac).unwrap();
    let file = fs::OpenOptions::new().write(true).create_new(true).open(&source).unwrap();
    let mut encoder = FlacEncoder::new(&library, file, 48_000.0, 2, 16).unwrap();
    let samples: Vec<f32> = (0..96_000).flat_map(|frame| {
        let v = (frame as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.4;
        [v, v * 0.5]
    }).collect();
    encoder.process_interleaved(&samples).unwrap();
    drop(encoder.finish().unwrap());
    let original = fs::read(&source).unwrap();
    let replaced = sandbox.ok(&["process", source.to_str().unwrap(), "--replace-originals",
        "--backup-dir", backups.to_str().unwrap(), "--jobs", "1"]);
    assert_eq!(replaced["results"][0]["report"]["processing_marker_status"], "present");
    let backup = replaced["results"][0]["report"]["backup_path"].as_str().unwrap();
    assert_eq!(fs::read(backup).unwrap(), original);
    sandbox.ok(&["config", "set", "libraries.flac", "null"]);
    sandbox.ok(&["config", "set", "libraries.flac_sha256", "null"]);
    let (inspection, _) = sandbox.run(&["inspect", source.to_str().unwrap()]);
    assert_eq!(inspection.status.code(), Some(1), "full inspection must still require the decoder");
    let restored = sandbox.ok(&["restore", source.to_str().unwrap(), "--backup", backup]);
    assert_eq!(restored["results"][0]["status"], "ok");
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(fs::read(backup).unwrap(), original);
}
