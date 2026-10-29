use platinum_engine::engine::ProcessReport;
use platinum_engine::file_workflow::Cancellation;
use platinum_engine::statistics::{Statistics, StatisticsError, StatisticsStore};
use serde_json::json;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};


static NEXT: AtomicU64 = AtomicU64::new(0);
const CHILD_DIR: &str = "PLATINUM_STATISTICS_TEST_CHILD_DIR";
const CHILD_SPECIAL: &str = "PLATINUM_STATISTICS_TEST_SPECIAL_DIR";
const UPDATES: usize = 12;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!("platinum-statistics-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        }
    }
    fn store(&self) -> StatisticsStore { StatisticsStore::new(&self.0) }
}
impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }

fn report(input_volume: f32, output_volume: f32, clip_count: u64) -> ProcessReport {
    ProcessReport {
        input_path: "input.wav".into(), input_sample_rate: 48000.0, processing_sample_rate: 48000.0,
        output_path: "output.wav".into(), backup_path: None, replaced_existing_output: false, input_format: "WAV".into(), output_format: "WAV".into(),
        sample_rate: 48000.0, channels: 2, output_channels: 2, output_bit_depth: 16,
        encoded_length_frames: 1, input_frames: 1, healed_frames: 1, output_frames: 1,
        input_volume, healed_volume: input_volume, output_volume, input_pitch_hz: 440.0, output_pitch_hz: 440.0,
        tempo_factor: 1.0, clip_count, applied_stages: Vec::new(),
        processing_marker_status: "present", input_marker_override: None, source_tags_unavailable: None, controlled_mp4_time_1904: None,
        non_identical_paths: Vec::new(),
    }
}

#[test]
fn restrictive_umask_record_can_be_read_and_updated_again() {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::process::CommandExt;
    const CHILD: &str = "PN_TEST_RECORD_UMASK_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        let store = StatisticsStore::new(PathBuf::from(path));
        store.record(&report(0.0, 0.6, 4), &Cancellation::new()).unwrap();
        assert_eq!(fs::metadata(store.statistics_path()).unwrap().permissions().mode() & 0o777, 0o600);
        let first = store.show(&Cancellation::new()).unwrap();
        assert_eq!((first.processed_songs, first.clipped_peaks, first.decibel_adjustment), (1, 4, 1));
        store.record(&report(0.0, 1.5, 3), &Cancellation::new()).unwrap();
        let second = store.show(&Cancellation::new()).unwrap();
        assert_eq!((second.processed_songs, second.clipped_peaks, second.decibel_adjustment), (2, 7, 3));
        return;
    }
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(fixture.0.join("statistics.lock")).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.arg("--exact").arg("restrictive_umask_record_can_be_read_and_updated_again")
        .arg("--nocapture").env(CHILD, &fixture.0);
    unsafe { child.pre_exec(|| { libc::umask(0o400); Ok(()) }); }
    assert!(child.status().unwrap().success());
}

#[test]
fn empty_pre_upgrade_baseline_and_explicit_reset_sequence() {
    let fixture = Fixture::new();
    let settings = fixture.0.join("settings.json");
    fs::write(&settings, b"unrelated settings bytes").unwrap();
    let store = fixture.store();
    assert_eq!(store.show(&Cancellation::new()).unwrap(), Statistics::default());
    assert!(!store.statistics_path().exists());

    store.record(&report(0.0, 0.6, 4), &Cancellation::new()).unwrap();
    let first = store.show(&Cancellation::new()).unwrap();
    assert_eq!((first.processed_songs, first.clipped_peaks, first.decibel_adjustment), (1, 4, 1));
    assert!(first.first_recorded_at_unix_ms.is_some());
    store.record(&report(0.6, 0.0, 5), &Cancellation::new()).unwrap();
    let second = store.show(&Cancellation::new()).unwrap();
    assert_eq!((second.processed_songs, second.clipped_peaks, second.decibel_adjustment), (2, 9, 2));
    assert_eq!(second.first_recorded_at_unix_ms, first.first_recorded_at_unix_ms);

    assert_eq!(store.reset(&Cancellation::new()).unwrap(), Statistics::default());
    assert_eq!(store.show(&Cancellation::new()).unwrap(), Statistics::default());
    store.record(&report(0.0, -1.5, 3), &Cancellation::new()).unwrap();
    let after_reset = store.show(&Cancellation::new()).unwrap();
    assert_eq!((after_reset.processed_songs, after_reset.clipped_peaks, after_reset.decibel_adjustment), (1, 3, 2));
    assert!(after_reset.first_recorded_at_unix_ms.is_some());
    assert_eq!(fs::read(settings).unwrap(), b"unrelated settings bytes");
}

#[test]
fn per_file_binary32_absolute_delta_uses_nearest_ties_away() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let cases = [
        (0.0, 0.49, 0), (0.0, 0.5, 1), (0.0, -0.5, 1),
        (0.0, 1.5, 2), (0.0, -1.5, 2), (0.0, 2.5, 3), (0.0, -2.5, 3),
        (-3.7824109, -11.453844, 8),
        (8_388_608.0, -0.5, 8_388_608), // binary32 subtraction rounds before integer conversion
    ];
    for (input, output, expected) in cases {
        store.reset(&Cancellation::new()).unwrap();
        store.record(&report(input, output, 0), &Cancellation::new()).unwrap();
        assert_eq!(store.show(&Cancellation::new()).unwrap().decibel_adjustment, expected, "input={input:?} output={output:?}");
    }
}

#[test]
fn nonfinite_saturation_and_all_totals_wrap_like_signed_native_addition() {
    let fixture = Fixture::new();
    let store = fixture.store();
    for (input, output, expected) in [
        (f32::NAN, 0.0, 0), (f32::INFINITY, f32::INFINITY, 0),
        (f32::NEG_INFINITY, f32::NEG_INFINITY, 0), (f32::INFINITY, 0.0, i64::MAX),
        (f32::NEG_INFINITY, 0.0, i64::MAX), (f32::MAX, 0.0, i64::MAX),
        (f32::MAX, -f32::MAX, i64::MAX),
    ] {
        store.reset(&Cancellation::new()).unwrap();
        store.record(&report(input, output, 0), &Cancellation::new()).unwrap();
        assert_eq!(store.show(&Cancellation::new()).unwrap().decibel_adjustment, expected);
    }
    let seed = json!({"version":1, "processed_songs":i64::MAX, "clipped_peaks":i64::MAX,
        "decibel_adjustment":i64::MAX, "first_recorded_at_unix_ms":123});
    fs::write(store.statistics_path(), serde_json::to_vec(&seed).unwrap()).unwrap();
    store.record(&report(0.0, 0.5, 2), &Cancellation::new()).unwrap();
    let wrapped = store.show(&Cancellation::new()).unwrap();
    assert_eq!((wrapped.processed_songs, wrapped.clipped_peaks, wrapped.decibel_adjustment), (i64::MIN, i64::MIN + 1, i64::MIN));
    assert_eq!(wrapped.first_recorded_at_unix_ms, Some(123));
    store.reset(&Cancellation::new()).unwrap();
    store.record(&report(0.0, 0.0, u64::MAX), &Cancellation::new()).unwrap();
    assert_eq!(store.show(&Cancellation::new()).unwrap().clipped_peaks, -1);
}

#[test]
fn malformed_unsupported_missing_fields_and_nonregular_states_are_never_overwritten() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let path = store.statistics_path();
    let invalid = [
        b"{broken".to_vec(),
        serde_json::to_vec(&json!({"version":2, "processed_songs":0, "clipped_peaks":0, "decibel_adjustment":0, "first_recorded_at_unix_ms":null})).unwrap(),
        serde_json::to_vec(&json!({"version":1, "processed_songs":0, "clipped_peaks":0, "decibel_adjustment":0})).unwrap(),
        serde_json::to_vec(&json!({"version":1, "processed_songs":1, "clipped_peaks":0, "decibel_adjustment":0, "first_recorded_at_unix_ms":null})).unwrap(),
        serde_json::to_vec(&json!({"version":1, "processed_songs":0, "clipped_peaks":0, "decibel_adjustment":0, "first_recorded_at_unix_ms":null, "extra":1})).unwrap(),
    ];
    for bytes in invalid {
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(store.show(&Cancellation::new()), Err(StatisticsError::Corrupt(_))));
        assert!(matches!(store.record(&report(0.0, 1.0, 1), &Cancellation::new()), Err(StatisticsError::Corrupt(_))));
        assert!(matches!(store.reset(&Cancellation::new()), Err(StatisticsError::Corrupt(_))));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    fs::remove_file(&path).unwrap();
    let target = fixture.0.join("unrelated");
    fs::write(&target, b"untouched").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(matches!(store.show(&Cancellation::new()), Err(StatisticsError::Corrupt(_))));
    assert!(matches!(store.reset(&Cancellation::new()), Err(StatisticsError::Corrupt(_))));
    assert_eq!(fs::read(target).unwrap(), b"untouched");
}

#[test]
fn state_and_lock_symlinks_and_hardlink_aliases_are_never_followed() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let state = store.statistics_path();
    let lock = fixture.0.join("statistics.lock");
    let cancel = Cancellation::new();
    std::os::unix::fs::symlink(&state, &lock).unwrap();
    assert!(store.show(&cancel).is_err());
    assert!(!state.exists());
    fs::remove_file(&lock).unwrap();

    let bytes = serde_json::to_vec(&Statistics::default()).unwrap();
    fs::write(&state, &bytes).unwrap();
    fs::hard_link(&state, &lock).unwrap();
    assert!(store.show(&cancel).is_err());
    assert!(store.record(&report(0.0, 1.0, 1), &cancel).is_err());
    assert!(store.reset(&cancel).is_err());
    assert_eq!(fs::read(&state).unwrap(), bytes);
    fs::remove_file(&lock).unwrap();

    let victim = fixture.0.join("unrelated");
    fs::remove_file(&state).unwrap();
    fs::write(&victim, &bytes).unwrap();
    fs::hard_link(&victim, &state).unwrap();
    assert!(matches!(store.show(&cancel), Err(StatisticsError::Corrupt(_))));
    assert_eq!(fs::read(victim).unwrap(), bytes);
}

#[test]
fn unsafe_statistics_directory_state_and_lock_are_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let store = fixture.store();
    let cancel = Cancellation::new();
    store.reset(&cancel).unwrap();
    let state = store.statistics_path();
    let lock = fixture.0.join("statistics.lock");
    let before = fs::read(&state).unwrap();
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(store.show(&cancel).is_err()); assert!(store.reset(&cancel).is_err());
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o620)).unwrap();
    assert!(store.show(&cancel).is_err()); assert!(store.reset(&cancel).is_err());
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o602)).unwrap();
    assert!(store.show(&cancel).is_err());
    assert_eq!(fs::read(&state).unwrap(), before);
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();

    let target = fixture.0.join("real"); fs::create_dir(&target).unwrap();
    let alias = fixture.0.join("alias"); symlink(&target, &alias).unwrap();
    for suffix in ["", "/", "/."] {
        assert!(StatisticsStore::new(PathBuf::from(format!("{}{suffix}", alias.display()))).show(&cancel).is_err());
    }
    assert!(!target.join("statistics.lock").exists());
}

#[test]
fn fifo_state_and_lock_are_rejected_without_blocking() {
    if let Some(directory) = std::env::var_os(CHILD_SPECIAL) {
        let store = StatisticsStore::new(PathBuf::from(directory));
        assert!(store.show(&Cancellation::new()).is_err());
        return;
    }
    use std::os::unix::ffi::OsStrExt;
    for name in ["statistics.json", "statistics.lock"] {
        let fixture = Fixture::new();
        let path = fixture.0.join(name);
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: c_path is a valid NUL-terminated path in an isolated fixture.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact").arg("fifo_state_and_lock_are_rejected_without_blocking")
            .env(CHILD_SPECIAL, &fixture.0).stdout(Stdio::null()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() { break Some(status); }
            if Instant::now() >= deadline { child.kill().unwrap(); child.wait().unwrap(); break None; }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.is_some_and(|status| status.success()), "FIFO operation blocked or failed");
    }
}

#[test]
fn concurrent_processes_and_threads_do_not_lose_updates() {
    if let Some(directory) = std::env::var_os(CHILD_DIR) {
        let store = StatisticsStore::new(PathBuf::from(directory));
        for _ in 0..UPDATES { store.record(&report(0.0, 0.6, 3), &Cancellation::new()).unwrap(); }
        return;
    }
    let fixture = Fixture::new();
    let store = fixture.store();
    let mut children: Vec<_> = (0..4).map(|_| Command::new(std::env::current_exe().unwrap())
        .arg("--exact").arg("concurrent_processes_and_threads_do_not_lose_updates").env(CHILD_DIR, &fixture.0)
        .stdout(Stdio::null()).spawn().unwrap()).collect();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let store = store.clone();
            scope.spawn(move || {
                for _ in 0..UPDATES { store.record(&report(0.0, 0.6, 3), &Cancellation::new()).unwrap(); }
            });
        }
    });
    for child in &mut children { assert!(child.wait().unwrap().success()); }
    let state = store.show(&Cancellation::new()).unwrap();
    let total = (8 * UPDATES) as i64;
    assert_eq!((state.processed_songs, state.clipped_peaks, state.decibel_adjustment), (total, total * 3, total));
    assert!(state.first_recorded_at_unix_ms.is_some());
}
