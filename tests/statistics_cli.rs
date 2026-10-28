use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};


static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture { root: PathBuf, home: PathBuf, state: PathBuf }
impl Fixture {
    fn new() -> Self {
        let root = loop {
            let path = std::env::temp_dir().join(format!("platinum-statistics-cli-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        };
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        let state = root.join("state");
        Self { root, home, state }
    }
    fn run(&self, args: &[&str]) -> (Output, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_platinum"))
            .env("HOME", &self.home).arg("--state-dir").arg(&self.state).arg("--json").args(args).output().unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!("invalid JSON outcome: {error}; stdout={:?}; stderr={:?}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
        });
        (output, value)
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (output, value) = self.run(args);
        assert!(output.status.success(), "{args:?}: {value}; stderr={}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(value["status"], "ok");
        value
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }

fn hold_statistics_lock(fixture: &Fixture) -> File {
    fixture.ok(&["statistics", "show"]);
    let file = OpenOptions::new().read(true).write(true).open(fixture.state.join("statistics.lock")).unwrap();
    // SAFETY: file owns a valid descriptor until the caller drops it.
    assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
    file
}

#[cfg(target_os = "macos")]
fn has_lock_descriptor(child: &Child, path: &Path) -> io::Result<bool> {
    let canonical = fs::canonicalize(path)?;
    let output = Command::new("/usr/sbin/lsof").arg("-a").arg("-p").arg(child.id().to_string())
        .arg("-Fn").output()?;
    Ok(String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| line.strip_prefix('n'))
        .any(|name| Path::new(name) == path || Path::new(name) == canonical.as_path()))
}

#[cfg(target_os = "linux")]
fn has_lock_descriptor(child: &Child, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let target = fs::metadata(path)?;
    for entry in fs::read_dir(format!("/proc/{}/fd", child.id()))? {
        match fs::metadata(entry?.path()) {
            Ok(meta) if meta.dev() == target.dev() && meta.ino() == target.ino() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

// Observe the real CLI's open lock descriptor before SIGINT. The handler is
// installed before execute() opens it, and the caller still holds the lock.
fn wait_for_lock_descriptor(child: &mut Child, path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() { panic!("CLI exited before lock contention: {status}"); }
        match has_lock_descriptor(child, path) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("inspect CLI lock descriptor: {error}"),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI did not open the statistics lock before the deadline");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn interrupt_while_contended(fixture: &Fixture, _held: &File, args: &[&str], timeout: Duration) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_platinum"))
        .env("HOME", &fixture.home).arg("--state-dir").arg(&fixture.state).arg("--json").args(args)
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    wait_for_lock_descriptor(&mut child, &fixture.state.join("statistics.lock"), timeout);
    // SAFETY: child is our live subprocess and SIGINT is the requested signal.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() { break; }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cancelled CLI did not exit while the statistics lock remained held");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130), "stderr={}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn show_reset_confirmation_and_config_reset_are_independent() {
    let fixture = Fixture::new();
    let empty = fixture.ok(&["statistics", "show"]);
    assert_eq!(empty["data"]["version"], 1);
    assert_eq!(empty["data"]["processed_songs"], 0);
    assert_eq!(empty["data"]["clipped_peaks"], 0);
    assert_eq!(empty["data"]["decibel_adjustment"], 0);
    assert!(empty["data"]["first_recorded_at_unix_ms"].is_null());
    assert!(empty["data"]["baseline"].is_string());
    assert!(empty["data"]["first_recorded_time"].is_string());
    assert!(empty["data"]["publication"].is_string());
    assert!(!fixture.state.join("statistics.json").exists());

    let seed = json!({"version":1, "processed_songs":7, "clipped_peaks":11,
        "decibel_adjustment":13, "first_recorded_at_unix_ms":123});
    fs::write(fixture.state.join("statistics.json"), serde_json::to_vec(&seed).unwrap()).unwrap();
    let (missing_confirm, error) = fixture.run(&["statistics", "reset"]);
    assert_eq!(missing_confirm.status.code(), Some(2));
    assert_eq!(error["error"]["kind"], "usage");
    assert_eq!(fixture.ok(&["statistics", "show"])["data"]["processed_songs"], 7);
    fixture.ok(&["config", "set", "output.format", "\"wav\""]);
    fixture.ok(&["config", "reset"]);
    assert_eq!(fixture.ok(&["statistics", "show"])["data"]["processed_songs"], 7);
    let settings_before = fs::read(fixture.state.join("settings.json")).unwrap();
    let reset = fixture.ok(&["statistics", "reset", "--confirm"]);
    assert_eq!(reset["data"]["processed_songs"], 0);
    assert!(reset["data"]["first_recorded_at_unix_ms"].is_null());
    assert_eq!(fs::read(fixture.state.join("settings.json")).unwrap(), settings_before);
    assert_eq!(fixture.ok(&["statistics", "show"])["data"]["clipped_peaks"], 0);
}

#[test]
fn corruption_is_reported_without_fallback_or_overwrite_and_is_independent_of_settings() {
    let fixture = Fixture::new();
    fixture.ok(&["statistics", "show"]);
    let path = fixture.state.join("statistics.json");
    fs::write(&path, b"{bad").unwrap();
    for args in [&["statistics", "show"][..], &["statistics", "reset", "--confirm"][..]] {
        let (output, value) = fixture.run(args);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(value["error"]["kind"], "statistics");
        assert_eq!(value["error"]["publication"], "not_published");
        assert_eq!(fs::read(&path).unwrap(), b"{bad");
    }
    fixture.ok(&["config", "show"]);
    fs::write(&path, serde_json::to_vec(&json!({"version":1, "processed_songs":0, "clipped_peaks":0,
        "decibel_adjustment":0, "first_recorded_at_unix_ms":null})).unwrap()).unwrap();
    fs::write(fixture.state.join("settings.json"), b"{bad settings").unwrap();
    assert_eq!(fixture.ok(&["statistics", "show"])["data"]["processed_songs"], 0);
}

#[test]
fn unsuccessful_process_and_read_only_commands_do_not_record() {
    let fixture = Fixture::new();
    let missing = fixture.root.join("missing.wav");
    let missing = missing.to_str().unwrap();
    let backup = fixture.root.join("missing-backup.wav");
    let backup = backup.to_str().unwrap();
    for args in [
        vec!["process", missing, "--same-folder"],
        vec!["analyze", missing],
        vec!["inspect", missing],
        vec!["restore", missing, "--backup", backup],
    ] {
        let (output, _) = fixture.run(&args);
        assert!(!output.status.success());
        assert_eq!(fixture.ok(&["statistics", "show"])["data"]["processed_songs"], 0);
    }
    assert!(!fixture.state.join("statistics.json").exists());
}

#[test]
fn default_directory_belongs_to_engine_not_vendor() {
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_platinum")).env("HOME", &fixture.home)
        .env_remove("XDG_CONFIG_HOME").env_remove("XDG_STATE_HOME")
        .args(["--json", "statistics", "show"]).output().unwrap();
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["data"]["processed_songs"], 0);
    #[cfg(target_os = "macos")]
    {
        assert!(fixture.home.join("Library/Application Support/Platinum Engine/statistics.lock").exists());
        assert!(!fixture.home.join("Library/Application Support/Mixedinkey").exists());
    }
    #[cfg(target_os = "linux")]
    assert!(fixture.home.join(".local/state/platinum-engine/statistics.lock").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn default_directories_honor_absolute_xdg_and_ignore_relative_paths() {
    let fixture = Fixture::new();
    let config = fixture.root.join("config-root");
    let state = fixture.root.join("state-root");
    let run = |config_path: &Path, state_path: &Path, args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_platinum"))
            .env("HOME", &fixture.home).env("XDG_CONFIG_HOME", config_path).env("XDG_STATE_HOME", state_path)
            .arg("--json").args(args).output().unwrap();
        assert!(output.status.success(), "stderr={}", String::from_utf8_lossy(&output.stderr));
    };
    run(&config, &state, &["statistics", "show"]);
    run(&config, &state, &["config", "set", "output.format", "\"wav\""]);
    assert!(state.join("platinum-engine/statistics.lock").exists());
    assert!(config.join("platinum-engine/settings.lock").exists());
    run(Path::new("relative-config"), Path::new("relative-state"), &["statistics", "show"]);
    run(Path::new("relative-config"), Path::new("relative-state"), &["config", "set", "output.format", "\"wav\""]);
    assert!(fixture.home.join(".local/state/platinum-engine/statistics.lock").exists());
    assert!(fixture.home.join(".config/platinum-engine/settings.lock").exists());
}

#[test]
fn sigint_while_locked_cancels_show_and_reset_without_changing_totals() {
    let fixture = Fixture::new();
    fixture.ok(&["statistics", "show"]);
    let path = fixture.state.join("statistics.json");
    let before = serde_json::to_vec(&json!({"version":1, "processed_songs":7, "clipped_peaks":11,
        "decibel_adjustment":13, "first_recorded_at_unix_ms":123})).unwrap();
    fs::write(&path, &before).unwrap();
    let held = hold_statistics_lock(&fixture);
    for args in [&["statistics", "show"][..], &["statistics", "reset", "--confirm"][..]] {
        let value = interrupt_while_contended(&fixture, &held, args, Duration::from_secs(20));
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["error"]["kind"], "cancelled");
        assert_eq!(value["error"]["publication"], "not_published");
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    drop(held);
    assert_eq!(fixture.ok(&["statistics", "show"])["data"]["processed_songs"], 7);
}

fn write_synthetic_wav(path: &Path) {
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
    fs::write(path, wav).unwrap();
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn published_audio_survives_accounting_failure_and_restore_never_changes_totals() {
    let fixture = Fixture::new();
    let bridge = std::env::var("PLATINUM_TEST_TAG_BRIDGE").expect("provide public tag bridge path");
    fixture.ok(&["config", "set", "libraries.tag_bridge", &json!(bridge).to_string()]);
    let first = fixture.root.join("first.wav");
    let second = fixture.root.join("second.wav");
    let replacement = fixture.root.join("replacement.wav");
    let output = fixture.root.join("output");
    let backup = fixture.root.join("backup");
    for path in [&first, &second, &replacement] { write_synthetic_wav(path); }
    fs::create_dir(&output).unwrap();
    fs::create_dir(&backup).unwrap();
    let first = first.to_str().unwrap();
    let second = second.to_str().unwrap();
    let replacement = replacement.to_str().unwrap();
    let output = output.to_str().unwrap();
    let backup = backup.to_str().unwrap();
    fixture.ok(&["analyze", first, "--jobs", "1"]);
    fixture.ok(&["inspect", first]);
    assert_eq!(fixture.ok(&["statistics", "show"])["data"]["processed_songs"], 0);

    let state_path = fixture.state.join("statistics.json");
    fs::write(&state_path, b"{bad").unwrap();
    let (failed, value) = fixture.run(&["process", first, "--format", "wav", "--output-dir", output, "--jobs", "1"]);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(value["status"], "partial_failure");
    assert_eq!(value["results"][0]["status"], "ok");
    assert_eq!(value["results"][0]["statistics"]["status"], "failed");
    assert_eq!(value["results"][0]["statistics"]["publication"], "not_published");
    let published = value["results"][0]["report"]["output_path"].as_str().unwrap();
    let inspected = fixture.ok(&["inspect", published]);
    let inspected_report = &inspected["results"][0]["report"];
    assert_eq!(inspected_report["processed"], true);
    assert_eq!(inspected_report["input_format"], "wav/pcm");
    assert!(inspected_report["frames"].as_u64().unwrap() > 0);
    assert_eq!(inspected_report["frames"], value["results"][0]["report"]["encoded_length_frames"]);
    assert_eq!(fs::read(&state_path).unwrap(), b"{bad");
    fs::remove_file(&state_path).unwrap();

    let processed = fixture.ok(&["process", second, "--format", "wav", "--output-dir", output, "--jobs", "1"]);
    assert_eq!(processed["results"][0]["statistics"]["status"], "recorded");
    let report = &processed["results"][0]["report"];
    let expected_delta = ((report["input_volume"].as_f64().unwrap() as f32 - report["output_volume"].as_f64().unwrap() as f32).abs().round()) as i64;
    let totals = fixture.ok(&["statistics", "show"]);
    assert_eq!(totals["data"]["processed_songs"], 1);
    assert_eq!(totals["data"]["clipped_peaks"], report["clip_count"]);
    assert_eq!(totals["data"]["decibel_adjustment"], expected_delta);
    let (collision, _) = fixture.run(&["process", second, "--format", "wav", "--output-dir", output, "--jobs", "1"]);
    assert_eq!(collision.status.code(), Some(1));
    assert_eq!(fixture.ok(&["statistics", "show"])["data"]["processed_songs"], 1);

    let replaced = fixture.ok(&["process", replacement, "--replace-originals", "--backup-dir", backup, "--jobs", "1"]);
    let backup_path = replaced["results"][0]["report"]["backup_path"].as_str().unwrap();
    let before_restore = fixture.ok(&["statistics", "show"])["data"].clone();
    assert_eq!(before_restore["processed_songs"], 2);
    fixture.ok(&["restore", replacement, "--backup", backup_path]);
    assert_eq!(fixture.ok(&["statistics", "show"])["data"], before_restore);
}

#[test]
#[ignore = "requires PLATINUM_TEST_TAG_BRIDGE pointing to the public tag bridge"]
fn sigint_after_audio_publication_keeps_report_and_leaves_gauges_unchanged() {
    let fixture = Fixture::new();
    let bridge = std::env::var("PLATINUM_TEST_TAG_BRIDGE").expect("provide public tag bridge path");
    fixture.ok(&["config", "set", "libraries.tag_bridge", &json!(bridge).to_string()]);
    fixture.ok(&["statistics", "show"]);
    let path = fixture.state.join("statistics.json");
    let before = serde_json::to_vec(&json!({"version":1, "processed_songs":3, "clipped_peaks":2,
        "decibel_adjustment":1, "first_recorded_at_unix_ms":123})).unwrap();
    fs::write(&path, &before).unwrap();
    let source = fixture.root.join("signal.wav");
    let output = fixture.root.join("output");
    write_synthetic_wav(&source);
    fs::create_dir(&output).unwrap();
    let held = hold_statistics_lock(&fixture);
    let value = interrupt_while_contended(&fixture, &held,
        &["process", source.to_str().unwrap(), "--format", "wav", "--output-dir", output.to_str().unwrap(), "--jobs", "1"],
        Duration::from_secs(180));
    assert_eq!(value["status"], "cancelled");
    assert_eq!(value["results"][0]["status"], "ok");
    assert_eq!(value["results"][0]["statistics"]["status"], "not_recorded");
    assert_eq!(value["results"][0]["statistics"]["reason"], "cancelled");
    assert_eq!(value["results"][0]["statistics"]["publication"], "not_published");
    assert_eq!(value["results"][0]["statistics"]["audio_published"], true);
    let published = value["results"][0]["report"]["output_path"].as_str().unwrap();
    let inspected = fixture.ok(&["inspect", published]);
    assert_eq!(inspected["results"][0]["report"]["processed"], true);
    assert_eq!(inspected["results"][0]["report"]["input_format"], "wav/pcm");
    assert_eq!(inspected["results"][0]["report"]["frames"], value["results"][0]["report"]["encoded_length_frames"]);
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(held);
    assert_eq!(fixture.ok(&["statistics", "show"])["data"]["processed_songs"], 3);
}
