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
fn version_and_machine_usage_error() {
    let sandbox = Sandbox::new();
    let version = sandbox.ok(&["version"]);
    assert_eq!(version["data"]["version"], env!("CARGO_PKG_VERSION"));
    let (output, error) = sandbox.run(&["not-a-command"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(error["error"]["kind"], "usage");
}

#[test]
fn ambiguous_marker_override_is_rejected_outside_process() {
    let sandbox = Sandbox::new();
    let input = "input.wav";
    for args in [
        vec!["inspect", input, "--allow-ambiguous-marker"],
        vec!["analyze", input, "--allow-ambiguous-marker"],
        vec!["restore", input, "--backup", "/synthetic/backup.wav", "--allow-ambiguous-marker"],
    ] {
        let (output, error) = sandbox.run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(error["error"]["kind"], "usage");
    }
}

#[test]
fn overwrite_is_explicit_ephemeral_and_conflicts_with_source_replacement() {
    let sandbox = Sandbox::new();
    let source = sandbox.root.join("missing.wav");
    let backup = sandbox.root.join("backups");
    fs::create_dir(&backup).unwrap();
    let source = source.to_str().unwrap(); let backup = backup.to_str().unwrap();
    let (output, normal) = sandbox.run(&["process", source, "--format", "wav"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(normal["details"]["overwrite"], false);
    assert_eq!(normal["details"]["effective_config"]["overwrite"], false);
    let (output, opted_in) = sandbox.run(&["process", source, "--format", "wav", "--overwrite"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(opted_in["details"]["overwrite"], true);
    assert_eq!(opted_in["details"]["effective_config"]["overwrite"], true);
    assert!(sandbox.ok(&["config", "show"])["data"].get("overwrite").is_none());
    let (output, error) = sandbox.run(&["process", source, "--overwrite", "--replace-originals", "--backup-dir", backup]);
    assert_eq!(output.status.code(), Some(2)); assert_eq!(error["error"]["kind"], "usage");
    let help = Command::new(env!("CARGO_BIN_EXE_platinum")).args(["process", "--help"]).output().unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--overwrite"));
}

#[test]
fn restore_rejects_cross_pair_target_backup_alias_before_any_write() {
    for hardlink_alias in [false, true] {
        let sandbox = Sandbox::new();
        let first = sandbox.root.join("first.wav"); fs::write(&first, b"first").unwrap();
        let second = sandbox.root.join("second.wav"); fs::write(&second, b"second").unwrap();
        let backup = sandbox.root.join("backup.wav"); fs::write(&backup, b"backup").unwrap();
        let alias = if hardlink_alias {
            let alias = sandbox.root.join("second-alias.wav"); fs::hard_link(&second, &alias).unwrap(); alias
        } else { second.clone() };
        let (output, result) = sandbox.run(&["restore", first.to_str().unwrap(), second.to_str().unwrap(),
            "--backup", alias.to_str().unwrap(), "--backup", backup.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(result["error"]["kind"], "usage");
        assert_eq!(fs::read(first).unwrap(), b"first");
        assert_eq!(fs::read(second).unwrap(), b"second");
        assert_eq!(fs::read(backup).unwrap(), b"backup");
    }
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
    let waveform = &analysis["results"][0]["report"]["waveform"];
    assert_eq!(waveform["sample_rate"], 44_100.0);
    assert_eq!(waveform["requested_sample_rate"], 44_100.0);
    assert_eq!(waveform["frames"], 132_300);
    let bins = waveform["bins"].as_array().unwrap();
    assert_eq!(bins[0]["frames"], 512);
    assert_eq!(bins.last().unwrap()["frames"], 204);
    assert_eq!(bins.iter().map(|bin| bin["frames"].as_u64().unwrap()).sum::<u64>(), 132_300);
    assert!(analysis["results"][0]["report"]["clip_count"].is_null());
    let processed = sandbox.ok(&["process", source, "--format", "wav", "--output-dir", outputs, "--jobs", "1"]);
    assert_eq!(processed["details"]["overwrite"], false);
    assert_eq!(processed["results"][0]["report"]["replaced_existing_output"], false);
    let output_path = processed["results"][0]["report"]["output_path"].as_str().unwrap();
    let output_bytes = fs::read(output_path).unwrap();
    let old_inode = fs::metadata(output_path).unwrap().ino();
    let inspection = sandbox.ok(&["inspect", output_path]);
    assert_eq!(inspection["results"][0]["report"]["processed"], true);
    assert_eq!(inspection["results"][0]["report"]["input_format"], "wav/pcm");
    assert_eq!(inspection["results"][0]["report"]["frames"], FRAMES);
    let (collision, outcome) = sandbox.run(&["process", source, "--format", "wav", "--output-dir", outputs, "--jobs", "1"]);
    assert_eq!(collision.status.code(), Some(1));
    assert_eq!(outcome["results"][0]["status"], "failed");
    assert_eq!(fs::read(output_path).unwrap(), output_bytes);

    let overwritten = sandbox.ok(&["process", source, "--format", "wav", "--output-dir", outputs, "--jobs", "1", "--overwrite"]);
    assert_eq!(overwritten["details"]["overwrite"], true);
    assert_eq!(overwritten["details"]["effective_config"]["overwrite"], true);
    assert_eq!(overwritten["results"][0]["report"]["replaced_existing_output"], true);
    assert_ne!(fs::metadata(output_path).unwrap().ino(), old_inode);
    assert_eq!(fs::read(source).unwrap(), wav);
    let new_output = fs::read(output_path).unwrap();
    let (collision, outcome) = sandbox.run(&["process", source, output_path, "--format", "wav", "--output-dir", outputs, "--overwrite"]);
    assert_eq!(collision.status.code(), Some(1));
    assert!(outcome["batch_error"].is_object());
    assert_eq!(fs::read(output_path).unwrap(), new_output);
    assert_eq!(fs::read(source).unwrap(), wav);

    let replaced = sandbox.ok(&["process", replacement, "--replace-originals", "--backup-dir", backups, "--jobs", "1"]);
    let backup = replaced["results"][0]["report"]["backup_path"].as_str().unwrap();
    assert_eq!(fs::read(backup).unwrap(), wav);
    let comparison = sandbox.ok(&["analyze", replacement, "--jobs", "1"]);
    assert_eq!(comparison["results"][0]["report"]["processed"], true);
    assert_eq!(comparison["results"][0]["report"]["original_waveform"]["frames"], 132_300);
    assert_eq!(comparison["results"][0]["report"]["original_waveform"]["sample_rate"], 44_100.0);
    let copy = sandbox.root.join("shared-copy.wav");
    fs::copy(replacement, &copy).unwrap();
    let restored = sandbox.ok(&["restore", replacement, copy.to_str().unwrap(), "--backup", backup, "--backup", backup]);
    assert_eq!(restored["results"][0]["status"], "ok");
    assert_eq!(restored["results"][0]["report"]["backup_removed"], true);
    assert!(restored["results"][0]["report"]["backup_removal_error"].is_null());
    assert_eq!(restored["results"][1]["status"], "ok");
    assert_eq!(restored["results"][1]["report"]["backup_removed"], true);
    assert_eq!(fs::read(&copy).unwrap(), wav);
    assert_eq!(restored["details"]["backup_policy"], "remove_after_verified_restore");
    assert_eq!(fs::read(replacement).unwrap(), wav);
    assert!(!Path::new(backup).exists());

    let replaced = sandbox.ok(&["process", replacement, "--replace-originals", "--backup-dir", backups, "--jobs", "1"]);
    let backup = replaced["results"][0]["report"]["backup_path"].as_str().unwrap();
    fs::set_permissions(backups, fs::Permissions::from_mode(0o500)).unwrap();
    let (output, restored) = sandbox.run(&["restore", replacement, "--backup", backup]);
    fs::set_permissions(backups, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(output.status.success(), "{restored}");
    assert_eq!(restored["results"][0]["status"], "ok");
    assert_eq!(restored["results"][0]["report"]["backup_removed"], false);
    assert!(restored["results"][0]["report"]["backup_removal_error"]["operation"].is_string());
    assert_eq!(restored["results"][0]["report"]["backup_removal_error"]["path"], backup);
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
    assert_eq!(restored["results"][0]["report"]["backup_removed"], true);
    assert_eq!(fs::read(&source).unwrap(), original);
    assert!(!Path::new(backup).exists());
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn cli_reports_ambiguous_wav_override_and_keeps_strict_guards() {
    use platinum_engine::metadata::{encode_marker_base64, wav_pltn_chunk};

    let sandbox = Sandbox::new();
    let bridge = std::env::var("PLATINUM_TEST_TAG_BRIDGE").expect("provide public tag bridge path");
    sandbox.ok(&["config", "set", "libraries.tag_bridge", &json!(bridge).to_string()]);
    let output = sandbox.root.join("override-output"); fs::create_dir(&output).unwrap();
    let output = output.to_str().unwrap();
    let frames = 48_513u32;
    let data_len = frames * 4;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF"); wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt "); wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&48_000u32.to_le_bytes()); wav.extend_from_slice(&192_000u32.to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes()); wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data"); wav.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let sample = ((frame as f64 * std::f64::consts::TAU * 437.0 / 48_000.0).sin() * 16_000.0) as i16;
        wav.extend_from_slice(&sample.to_le_bytes()); wav.extend_from_slice(&(sample / 2).to_le_bytes());
    }
    let incomplete = encode_marker_base64(br#"{"isProcessed":"#).unwrap();
    for (name, payload, kind) in [
        ("empty.wav", b"".as_slice(), "empty_wav_pltn"),
        ("incomplete.wav", incomplete.as_slice(), "incomplete_json"),
    ] {
        let path = sandbox.root.join(name);
        let mut bytes = wav.clone();
        bytes.extend_from_slice(&wav_pltn_chunk(Some(payload)).unwrap());
        let length = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        let input = path.to_str().unwrap();
        let (denied, value) = sandbox.run(&["process", input, "--format", "wav", "--output-dir", output, "--jobs", "1"]);
        assert_eq!(denied.status.code(), Some(1));
        assert!(value["results"][0]["error"]["message"].as_str().unwrap().contains("--allow-ambiguous-marker"));
        let strict_backup = sandbox.root.join("strict-backup.wav");
        fs::write(&strict_backup, b"unrelated").unwrap();
        for args in [vec!["inspect", input], vec!["restore", input, "--backup", strict_backup.to_str().unwrap()]] {
            let (strict, value) = sandbox.run(&args);
            assert_eq!(strict.status.code(), Some(1));
            assert_eq!(value["results"][0]["error"]["stage"], "decode marker");
        }
        let args = ["process", input, "--format", "wav", "--output-dir", output, "--jobs", "1", "--allow-ambiguous-marker"];
        let result = sandbox.ok(&args);
        assert_eq!(result["details"]["input_marker_overrides"][0]["kind"], kind);
        assert_eq!(result["results"][0]["report"]["input_marker_override"], kind);
        let produced_path = result["results"][0]["report"]["output_path"].as_str().unwrap();
        let produced = fs::read(produced_path).unwrap();
        let (collision, result) = sandbox.run(&args);
        assert_eq!(collision.status.code(), Some(1));
        assert_eq!(result["details"]["input_marker_overrides"][0]["kind"], kind);
        assert_eq!(fs::read(produced_path).unwrap(), produced);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    let path = sandbox.root.join("marked.wav");
    let mut bytes = wav.clone();
    let marker = encode_marker_base64(br#"{"isProcessed":true}"#).unwrap();
    bytes.extend_from_slice(&wav_pltn_chunk(Some(&marker)).unwrap());
    let length = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&length.to_le_bytes()); fs::write(&path, &bytes).unwrap();
    let (denied, result) = sandbox.run(&["process", path.to_str().unwrap(), "--format", "wav", "--output-dir", output, "--allow-ambiguous-marker"]);
    assert_eq!(denied.status.code(), Some(1));
    assert!(result["results"][0]["error"]["message"].as_str().unwrap().contains("already processed"));
    assert_eq!(result["details"]["input_marker_overrides"], json!([]));
    assert_eq!(fs::read(&path).unwrap(), bytes);

    let duplicate = sandbox.root.join("duplicate.wav");
    let unprocessed = encode_marker_base64(br#"{"isProcessed":false}"#).unwrap();
    let mut bytes = wav.clone();
    bytes.extend_from_slice(&wav_pltn_chunk(Some(&marker)).unwrap());
    bytes.extend_from_slice(&wav_pltn_chunk(Some(&unprocessed)).unwrap());
    let length = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&length.to_le_bytes()); fs::write(&duplicate, &bytes).unwrap();
    for allow in [false, true] {
        let mut args = vec!["process", duplicate.to_str().unwrap(), "--format", "wav", "--output-dir", output];
        if allow { args.push("--allow-ambiguous-marker"); }
        let (denied, result) = sandbox.run(&args);
        assert_eq!(denied.status.code(), Some(1));
        assert!(result["results"][0]["error"]["message"].as_str().unwrap().contains("multiple WAV PLTN"));
    }
    assert_eq!(fs::read(duplicate).unwrap(), bytes);

    for (name, present) in [("absent-trailer.wav", false), ("unprocessed-trailer.wav", true)] {
        let source = sandbox.root.join(name);
        let mut bytes = wav.clone();
        if present {
            bytes.extend_from_slice(&wav_pltn_chunk(Some(&unprocessed)).unwrap());
            let length = u32::try_from(bytes.len() - 8).unwrap();
            bytes[4..8].copy_from_slice(&length.to_le_bytes());
        }
        bytes.extend_from_slice(&wav_pltn_chunk(Some(&marker)).unwrap());
        fs::write(&source, &bytes).unwrap();
        for allow in [false, true] {
            let mut args = vec!["process", source.to_str().unwrap(), "--format", "wav", "--output-dir", output];
            if allow { args.push("--allow-ambiguous-marker"); }
            let (denied, result) = sandbox.run(&args);
            assert_eq!(denied.status.code(), Some(1));
            assert_eq!(result["results"][0]["error"]["stage"], "decode marker");
            assert_eq!(result["results"][0]["error"]["partial_publication"], false);
        }
        assert_eq!(fs::read(source).unwrap(), bytes);
    }

    let trailing = sandbox.root.join("trailing.wav");
    let mut bytes = wav;
    bytes.extend_from_slice(&wav_pltn_chunk(Some(b"")).unwrap());
    let length = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&wav_pltn_chunk(Some(&marker)).unwrap());
    fs::write(&trailing, &bytes).unwrap();
    let (denied, result) = sandbox.run(&["process", trailing.to_str().unwrap(), "--format", "wav", "--output-dir", output, "--allow-ambiguous-marker"]);
    assert_eq!(denied.status.code(), Some(1));
    assert_eq!(result["results"][0]["error"]["stage"], "decode marker");
    assert_eq!(result["results"][0]["error"]["partial_publication"], false);
    assert_eq!(fs::read(trailing).unwrap(), bytes);
}
