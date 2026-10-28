//! Engine-owned cumulative gauges; this module never reads Platinum Notes state.
//!
//! An absent state file is an empty baseline, not the historical totals from
//! before this feature was installed. The first-recorded time is the local
//! system clock at the first successfully recorded report after that baseline
//! or an explicit reset; it is not necessarily the first audio processing time.
//! Audio and these gauges are separate publications and are not crash-atomic.

use crate::engine::ProcessReport;
use crate::file_workflow::Cancellation;
use crate::settings::{SecureSettingsDirectory, SettingsError, secure_file_metadata, secure_path_metadata, sync_directory, validate_private_acl, write_stage};
use serde::{Deserialize, Deserializer, Serialize};
use std::error::Error;
use std::ffi::{CString, c_char, c_int, c_uint};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "statistics.json";
const LOCK_FILE: &str = "statistics.lock";
const O_NOFOLLOW: c_int = libc::O_NOFOLLOW;
const O_NONBLOCK: c_int = libc::O_NONBLOCK;

// Option fields normally accept a missing key. This field must be present even
// when null, so a truncated/older state is not silently treated as a baseline.
fn required_timestamp<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(deserializer)
}

/// Signed 64-bit totals use the native wrapping additions. The timestamp is
/// milliseconds since the Unix epoch according to the recording host clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statistics {
    pub version: u32,
    pub processed_songs: i64,
    pub clipped_peaks: i64,
    pub decibel_adjustment: i64,
    #[serde(deserialize_with = "required_timestamp")]
    pub first_recorded_at_unix_ms: Option<u64>,
}

impl Default for Statistics {
    fn default() -> Self {
        Self { version: STATE_VERSION, processed_songs: 0, clipped_peaks: 0,
            decibel_adjustment: 0, first_recorded_at_unix_ms: None }
    }
}

#[derive(Debug)]
pub enum StatisticsError {
    Io { operation: &'static str, path: PathBuf, source: io::Error },
    /// The new file was renamed into place but its directory could not be synced.
    /// The update is visible but its durability after a crash is uncertain.
    AfterPublish { path: PathBuf, source: io::Error },
    Cancelled,
    Changed(PathBuf),
    Cleanup { path: PathBuf, cause: Box<StatisticsError>, source: io::Error },
    Storage(SettingsError),
    Corrupt(String),
    Clock(String),
    Serialize(serde_json::Error),
}

impl StatisticsError {
    /// An error after rename must not be retried as if no update occurred.
    pub fn publication_status(&self) -> &'static str {
        if matches!(self, Self::AfterPublish { .. }) { "published_durability_uncertain" } else { "not_published" }
    }
    /// Cleanup failure is not a clean cancellation, even when cancellation
    /// triggered the cleanup attempt.
    pub fn is_cancelled(&self) -> bool { matches!(self, Self::Cancelled) }
}

impl fmt::Display for StatisticsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, path, source } => write!(f, "{operation} {}: {source}", path.display()),
            Self::AfterPublish { path, source } => write!(f, "statistics were published at {}, but syncing the directory failed; durability is uncertain: {source}", path.display()),
            Self::Cancelled => write!(f, "statistics operation cancelled before publication"),
            Self::Changed(path) => write!(f, "statistics state or lock changed during the operation: {}", path.display()),
            Self::Cleanup { path, cause, source } => write!(f, "{cause}; additionally removing temporary {} failed: {source}", path.display()),
            Self::Storage(error) => write!(f, "statistics storage: {error}"),
            Self::Corrupt(message) => write!(f, "invalid statistics state: {message}"),
            Self::Clock(message) => write!(f, "cannot establish first-recorded time: {message}"),
            Self::Serialize(error) => write!(f, "serialize statistics: {error}"),
        }
    }
}
impl Error for StatisticsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::AfterPublish { source, .. } => Some(source),
            Self::Cleanup { cause, .. } => Some(cause.as_ref()),
            Self::Storage(error) => Some(error),
            Self::Serialize(error) => Some(error),
            Self::Corrupt(_) | Self::Clock(_) | Self::Cancelled | Self::Changed(_) => None,
        }
    }
}

#[cfg_attr(target_os = "macos", link(name = "System"))]
unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
    #[cfg(target_os = "macos")]
    fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    device: u64, inode: u64, size: u64,
    mtime: i64, mtime_nsec: i64, ctime: i64, ctime_nsec: i64,
    mode: u32, uid: u32,
}
impl FileStamp {
    fn new(metadata: &fs::Metadata) -> Self {
        Self { device: metadata.dev(), inode: metadata.ino(), size: metadata.size(),
            mtime: metadata.mtime(), mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(), ctime_nsec: metadata.ctime_nsec(), mode: metadata.mode(), uid: metadata.uid() }
    }
}

fn validate_state_path(path: &Path, expected: Option<FileStamp>) -> Result<(), StatisticsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && metadata.nlink() == 1 && expected.is_some() => {
            let metadata = secure_path_metadata(path, false).map_err(StatisticsError::Storage)?;
            if Some(FileStamp::new(&metadata)) == expected { Ok(()) } else { Err(StatisticsError::Changed(path.to_owned())) }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && expected.is_none() => Ok(()),
        Err(source) if source.kind() != io::ErrorKind::NotFound =>
            Err(StatisticsError::Io { operation: "inspect statistics identity", path: path.to_owned(), source }),
        _ => Err(StatisticsError::Changed(path.to_owned())),
    }
}

fn cleanup_stage(path: &Path, cause: StatisticsError) -> StatisticsError {
    match fs::remove_file(path) {
        Ok(()) => cause,
        Err(source) => StatisticsError::Cleanup { path: path.to_owned(), cause: Box::new(cause), source },
    }
}
fn cleanup_owned_stage(path: &Path, file: &File, cause: StatisticsError) -> StatisticsError {
    let identity = file.metadata().and_then(|held| {
        let named = fs::symlink_metadata(path)?;
        if !held.file_type().is_file() || !named.file_type().is_file()
            || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "statistics stage was replaced; retained"));
        }
        Ok(())
    });
    if let Err(source) = identity {
        return StatisticsError::Cleanup { path: path.to_owned(), cause: Box::new(cause), source };
    }
    cleanup_stage(path, cause)
}

fn publish_stage(stage: &Path, path: &Path, expected: Option<FileStamp>) -> Result<(), StatisticsError> {
    if expected.is_some() {
        return fs::rename(stage, path).map_err(|source| StatisticsError::Io {
            operation: "publish statistics", path: path.to_owned(), source,
        });
    }
    let encode = |path: &Path| CString::new(path.as_os_str().as_bytes()).map_err(|error| StatisticsError::Io {
        operation: "encode statistics path", path: path.to_owned(), source: io::Error::new(io::ErrorKind::InvalidInput, error),
    });
    let from = encode(stage)?;
    let to = encode(path)?;
    // SAFETY: both path strings are NUL-terminated and live for this call.
    // The no-replace flag prevents an existing state from being clobbered.
    #[cfg(target_os = "macos")]
    let status = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), 0x00000004) };
    #[cfg(target_os = "linux")]
    let status = unsafe { libc::renameat2(libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), libc::RENAME_NOREPLACE) };
    if status == 0 { return Ok(()); }
    let source = io::Error::last_os_error();
    if source.kind() == io::ErrorKind::AlreadyExists { Err(StatisticsError::Changed(path.to_owned())) }
    else { Err(StatisticsError::Io { operation: "publish statistics", path: path.to_owned(), source }) }
}

// Separate from the settings lock: this lock must not follow symlinks, be a
// special file, or be hard-linked to the state file whose inode we replace.
struct StatisticsLock { file: File, path: PathBuf, device: u64, inode: u64, directory: SecureSettingsDirectory, alias: PathBuf, state_pin: Mutex<Option<File>> }
impl StatisticsLock {
    fn acquire(path: &Path, cancelled: impl Fn() -> bool) -> Result<Self, StatisticsError> {
        let parent = path.parent().ok_or_else(|| StatisticsError::Corrupt("statistics lock has no parent".into()))?;
        let directory = SecureSettingsDirectory::open(parent).map_err(StatisticsError::Storage)?;
        let alias = directory.alias().map_err(StatisticsError::Storage)?;
        let pinned_path = alias.join(LOCK_FILE);
        let file = OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK).open(&pinned_path)
            .map_err(|source| StatisticsError::Io { operation: "open statistics lock", path: path.to_owned(), source })?;
        let initial = file.metadata().map_err(|source| StatisticsError::Io { operation: "inspect statistics lock", path: path.to_owned(), source })?;
        if !initial.file_type().is_file() || initial.nlink() != 1 {
            return Err(StatisticsError::Corrupt(format!("{} is not a singly linked regular lock file", path.display())));
        }
        let metadata = secure_file_metadata(&file, path, false).map_err(StatisticsError::Storage)?;
        validate_private_acl(&file, path).map_err(StatisticsError::Storage)?;
        let lock = Self { file, path: pinned_path, device: metadata.dev(), inode: metadata.ino(), directory, alias, state_pin: Mutex::new(None) };
        loop {
            lock.validate()?;
            // SAFETY: the descriptor remains owned by lock until this scope
            // ends. LOCK_NB makes cancellation observable while contended.
            if unsafe { flock(lock.file.as_raw_fd(), 2 | 4) } == 0 {
                lock.validate()?;
                return Ok(lock);
            }
            let error = io::Error::last_os_error();
            if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
                if cancelled() { return Err(StatisticsError::Cancelled); }
                if error.kind() == io::ErrorKind::WouldBlock { std::thread::sleep(Duration::from_millis(20)); }
                continue;
            }
            return Err(StatisticsError::Io { operation: "lock statistics", path: path.to_owned(), source: error });
        }
    }

    fn validate(&self) -> Result<(), StatisticsError> {
        self.directory.validate().map_err(StatisticsError::Storage)?;
        let descriptor = self.file.metadata().map_err(|source| StatisticsError::Io {
            operation: "inspect held statistics lock", path: self.path.clone(), source,
        })?;
        let path = match fs::symlink_metadata(&self.path) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(StatisticsError::Changed(self.path.clone())),
            Err(source) => return Err(StatisticsError::Io { operation: "inspect statistics lock identity", path: self.path.clone(), source }),
        };
        if !descriptor.file_type().is_file() || descriptor.nlink() != 1 || descriptor.dev() != self.device
            || descriptor.ino() != self.inode || !path.file_type().is_file() || path.nlink() != 1
            || path.dev() != self.device || path.ino() != self.inode {
            return Err(StatisticsError::Changed(self.path.clone()));
        }
        secure_file_metadata(&self.file, &self.path, false).map_err(StatisticsError::Storage)?;
        validate_private_acl(&self.file, &self.path).map_err(StatisticsError::Storage)?;
        secure_path_metadata(&self.path, false).map_err(StatisticsError::Storage)?;
        Ok(())
    }
}

/// A store is rooted at the Engine state directory, separate from settings and
/// from all vendor preferences. Cooperating processes share its permanent lock.
/// Initial publication never replaces an existing entry. For an existing state,
/// an uncooperative writer can still race the final identity check and rename.
#[derive(Debug, Clone)]
pub struct StatisticsStore { directory: PathBuf }

impl StatisticsStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self { Self { directory: directory.into() } }
    #[cfg(target_os = "linux")]
    pub fn in_home(home: &Path) -> Self {
        Self::new(crate::settings::xdg_directory("XDG_STATE_HOME", home.join(".local/state")).join("platinum-engine"))
    }
    pub fn statistics_path(&self) -> PathBuf { self.directory.join(STATE_FILE) }

    /// Missing state is an empty Engine baseline; malformed state is an error.
    pub fn show(&self, cancel: &Cancellation) -> Result<Statistics, StatisticsError> {
        let lock = self.lock(cancel)?;
        if cancel.is_cancelled() { return Err(StatisticsError::Cancelled); }
        let (state, _) = self.read_locked(&lock)?;
        if cancel.is_cancelled() { return Err(StatisticsError::Cancelled); }
        Ok(state)
    }

    /// Record a successfully published process report exactly once per call.
    /// The caller must not call this for errors or retry after an uncertain
    /// publication. The separate audio and state commits cannot be crash-atomic.
    pub fn record(&self, report: &ProcessReport, cancel: &Cancellation) -> Result<(), StatisticsError> {
        // Even if already cancelled, try once to record an already-published
        // report; do not wait for a contended lock after cancellation.
        let lock = self.lock(cancel)?;
        let (mut state, expected) = self.read_locked(&lock)?;
        if state.first_recorded_at_unix_ms.is_none() {
            let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)
                .map_err(|error| StatisticsError::Clock(error.to_string()))?;
            state.first_recorded_at_unix_ms = Some(u64::try_from(elapsed.as_millis())
                .map_err(|error| StatisticsError::Clock(error.to_string()))?);
        }
        // FABD operates in binary32, then FCVTAS rounds each contribution
        // independently, with ties away from zero. Rust's float-to-int cast
        // saturates positive overflow and maps NaN to zero as required here.
        let contribution = (report.input_volume - report.output_volume).abs().round() as i64;
        state.decibel_adjustment = state.decibel_adjustment.wrapping_add(contribution);
        state.clipped_peaks = state.clipped_peaks.wrapping_add(report.clip_count as i64);
        state.processed_songs = state.processed_songs.wrapping_add(1);
        self.publish_locked(&state, &lock, expected, None, sync_directory)
    }

    /// Reset is serialized with all records. Existing malformed data is never
    /// overwritten, even on an explicit reset.
    pub fn reset(&self, cancel: &Cancellation) -> Result<Statistics, StatisticsError> {
        let lock = self.lock(cancel)?;
        if cancel.is_cancelled() { return Err(StatisticsError::Cancelled); }
        let (_, expected) = self.read_locked(&lock)?;
        let state = Statistics::default();
        self.publish_locked(&state, &lock, expected, Some(cancel), sync_directory)?;
        Ok(state)
    }

    fn lock(&self, cancel: &Cancellation) -> Result<StatisticsLock, StatisticsError> {
        if self.directory.as_os_str().is_empty() { return Err(StatisticsError::Corrupt("state directory cannot be empty".into())); }
        StatisticsLock::acquire(&self.directory.join(LOCK_FILE), || cancel.is_cancelled())
    }


    fn read_locked(&self, lock: &StatisticsLock) -> Result<(Statistics, Option<FileStamp>), StatisticsError> {
        lock.validate()?;
        let path = lock.alias.join(STATE_FILE);
        let mut file = match OpenOptions::new().read(true).custom_flags(O_NOFOLLOW | O_NONBLOCK).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                validate_state_path(&path, None)?;
                lock.validate()?;
                *lock.state_pin.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Ok((Statistics::default(), None));
            }
            Err(source) => {
                if fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_file()) {
                    return Err(StatisticsError::Corrupt(format!("{} is not a regular file", path.display())));
                }
                return Err(StatisticsError::Io { operation: "open statistics", path, source });
            }
        };
        let initial = file.metadata().map_err(|source| StatisticsError::Io { operation: "inspect opened statistics", path: path.clone(), source })?;
        if !initial.file_type().is_file() || initial.nlink() != 1 {
            return Err(StatisticsError::Corrupt(format!("{} is not a singly linked regular file", path.display())));
        }
        let metadata = secure_file_metadata(&file, &path, false).map_err(StatisticsError::Storage)?;
        validate_private_acl(&file, &path).map_err(StatisticsError::Storage)?;
        let stamp = FileStamp::new(&metadata);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|source| StatisticsError::Io { operation: "read statistics", path: path.clone(), source })?;
        let after = secure_file_metadata(&file, &path, false).map_err(StatisticsError::Storage)?;
        validate_private_acl(&file, &path).map_err(StatisticsError::Storage)?;
        if !after.file_type().is_file() || after.nlink() != 1 || FileStamp::new(&after) != stamp { return Err(StatisticsError::Changed(path)); }
        validate_state_path(&path, Some(stamp))?;
        lock.validate()?;
        let state: Statistics = serde_json::from_slice(&bytes)
            .map_err(|error| StatisticsError::Corrupt(format!("{}: {error}", path.display())))?;
        if state.version != STATE_VERSION {
            return Err(StatisticsError::Corrupt(format!("unsupported version {}", state.version)));
        }
        if state.first_recorded_at_unix_ms.is_none()
            && (state.processed_songs != 0 || state.clipped_peaks != 0 || state.decibel_adjustment != 0) {
            return Err(StatisticsError::Corrupt("nonzero totals without a first-recorded time".into()));
        }
        *lock.state_pin.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
        Ok((state, Some(stamp)))
    }

    fn publish_locked(&self, state: &Statistics, lock: &StatisticsLock, expected: Option<FileStamp>,
                      cancel: Option<&Cancellation>, sync: impl FnOnce(&Path) -> io::Result<()>) -> Result<(), StatisticsError> {
        let mut bytes = serde_json::to_vec_pretty(state).map_err(StatisticsError::Serialize)?;
        bytes.push(b'\n');
        let (stage, stage_pin) = write_stage(&lock.alias, "statistics", &bytes).map_err(StatisticsError::Storage)?;
        let metadata = match secure_file_metadata(&stage_pin, &stage, false) {
            Ok(metadata) => metadata,
            Err(error) => return Err(cleanup_owned_stage(&stage, &stage_pin, StatisticsError::Storage(error))),
        };
        let stage_stamp = FileStamp::new(&metadata);
        let path = lock.alias.join(STATE_FILE);
        let publish = || {
            lock.validate()?;
            validate_state_path(&stage, Some(stage_stamp))?;
            validate_state_path(&path, expected)?;
            publish_stage(&stage, &path, expected)
        };
        // A reset checks cancellation and renames while holding the same
        // boundary as audio publication and cancel(). Do not hold that
        // boundary while waiting for the cross-process statistics lock.
        let result = if let Some(cancel) = cancel {
            let _guard = cancel.publication_guard();
            if cancel.is_cancelled() { Err(StatisticsError::Cancelled) } else { publish() }
        } else { publish() };
        if let Err(error) = result { return Err(cleanup_owned_stage(&stage, &stage_pin, error)); }
        sync(&lock.alias).map_err(|source| StatisticsError::AfterPublish { path: self.statistics_path(), source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            loop {
                let candidate = std::env::temp_dir().join(format!("platinum-statistics-internal-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
                match fs::create_dir(&candidate) {
                    Ok(()) => return Self(candidate),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create isolated directory: {error}"),
                }
            }
        }
        fn store(&self) -> StatisticsStore { StatisticsStore::new(&self.0) }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    #[test]
    fn restrictive_umask_stage_cleanup_uses_creation_descriptor() {
        use std::os::unix::process::CommandExt;
        const CHILD: &str = "PN_TEST_STATISTICS_UMASK_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let (stage, file) = write_stage(Path::new(&path), "statistics", b"discard").unwrap();
            assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
            assert!(matches!(cleanup_owned_stage(&stage, &file, StatisticsError::Cancelled), StatisticsError::Cancelled));
            assert!(!stage.exists());
            return;
        }
        let fixture = Fixture::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.arg("--exact").arg("statistics::tests::restrictive_umask_stage_cleanup_uses_creation_descriptor")
            .arg("--nocapture").env(CHILD, &fixture.0);
        unsafe { child.pre_exec(|| { libc::umask(0o400); Ok(()) }); }
        assert!(child.status().unwrap().success());
    }

    #[test]
    fn absent_state_collision_does_not_replace_the_new_file() {
        let fixture = Fixture::new();
        let (stage, _stage_pin) = write_stage(&fixture.0, "statistics", b"our staged data").unwrap();
        let destination = fixture.0.join(STATE_FILE);
        fs::write(&destination, b"independent writer").unwrap();
        let error = publish_stage(&stage, &destination, None).unwrap_err();
        assert!(matches!(&error, StatisticsError::Changed(path) if path == &destination));
        assert_eq!(fs::read(&destination).unwrap(), b"independent writer");
        assert_eq!(fs::read(&stage).unwrap(), b"our staged data");
        let error = cleanup_stage(&stage, error);
        assert!(matches!(error, StatisticsError::Changed(_)));
        assert!(!stage.exists());
        assert_eq!(fs::read(destination).unwrap(), b"independent writer");
    }

    #[test]
    fn failed_cleanup_after_cancellation_is_not_clean_cancellation() {
        let fixture = Fixture::new();
        let stage = fixture.0.join("stage-directory");
        fs::create_dir(&stage).unwrap();
        let error = cleanup_stage(&stage, StatisticsError::Cancelled);
        assert!(!error.is_cancelled());
        assert_eq!(error.publication_status(), "not_published");
        match error {
            StatisticsError::Cleanup { path, cause, .. } => {
                assert_eq!(path, stage);
                assert!(matches!(*cause, StatisticsError::Cancelled));
            }
            other => panic!("expected retained cleanup failure, got {other:?}"),
        }
        assert!(stage.exists());
    }

    #[test]
    fn directory_sync_failure_reports_visible_but_uncertain_publication() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let cancel = Cancellation::new();
        let state = Statistics { processed_songs: 1, clipped_peaks: 2, decibel_adjustment: 3,
            first_recorded_at_unix_ms: Some(4), ..Statistics::default() };
        let lock = store.lock(&cancel).unwrap();
        let (_, expected) = store.read_locked(&lock).unwrap();
        let error = store.publish_locked(&state, &lock, expected, None,
            |_| Err(io::Error::other("injected directory sync failure"))).unwrap_err();
        assert!(matches!(&error, StatisticsError::AfterPublish { .. }));
        assert_eq!(error.publication_status(), "published_durability_uncertain");
        drop(lock);
        assert_eq!(store.show(&cancel).unwrap(), state);
    }

    #[test]
    fn replaced_state_or_lock_is_not_published_over() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let cancel = Cancellation::new();
        store.reset(&cancel).unwrap();
        let original = fs::read(store.statistics_path()).unwrap();
        let lock = store.lock(&cancel).unwrap();
        let (_, expected) = store.read_locked(&lock).unwrap();
        let victim = fixture.0.join("victim");
        fs::write(&victim, b"unrelated").unwrap();
        fs::rename(store.statistics_path(), fixture.0.join("saved-state")).unwrap();
        std::os::unix::fs::symlink(&victim, store.statistics_path()).unwrap();
        let result = store.publish_locked(&Statistics::default(), &lock, expected, None, sync_directory);
        assert!(matches!(result, Err(StatisticsError::Changed(_))));
        assert_eq!(fs::read(&victim).unwrap(), b"unrelated");
        assert_eq!(fs::read(fixture.0.join("saved-state")).unwrap(), original);
        fs::remove_file(store.statistics_path()).unwrap();
        fs::rename(fixture.0.join("saved-state"), store.statistics_path()).unwrap();
        let (_, expected) = store.read_locked(&lock).unwrap();
        fs::rename(store.statistics_path(), fixture.0.join("saved-state")).unwrap();
        fs::write(store.statistics_path(), b"replacement state").unwrap();
        let result = store.publish_locked(&Statistics::default(), &lock, expected, None, sync_directory);
        assert!(matches!(result, Err(StatisticsError::Changed(_))));
        assert_eq!(fs::read(store.statistics_path()).unwrap(), b"replacement state");
        fs::remove_file(store.statistics_path()).unwrap();
        fs::rename(fixture.0.join("saved-state"), store.statistics_path()).unwrap();
        let (_, expected) = store.read_locked(&lock).unwrap();
        fs::remove_file(fixture.0.join(LOCK_FILE)).unwrap();
        fs::write(fixture.0.join(LOCK_FILE), b"replacement").unwrap();
        let result = store.publish_locked(&Statistics::default(), &lock, expected, None, sync_directory);
        assert!(matches!(result, Err(StatisticsError::Changed(_))));
        assert_eq!(fs::read(store.statistics_path()).unwrap(), original);
    }

    #[test]
    fn cancelled_reset_after_staging_does_not_publish() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let cancel = Cancellation::new();
        let state = Statistics { processed_songs: 1, clipped_peaks: 2, decibel_adjustment: 3,
            first_recorded_at_unix_ms: Some(4), ..Statistics::default() };
        fs::write(store.statistics_path(), serde_json::to_vec(&state).unwrap()).unwrap();
        let lock = store.lock(&cancel).unwrap();
        let (_, expected) = store.read_locked(&lock).unwrap();
        let before = fs::read(store.statistics_path()).unwrap();
        cancel.cancel();
        let error = store.publish_locked(&Statistics::default(), &lock, expected, Some(&cancel), sync_directory).unwrap_err();
        assert!(error.is_cancelled());
        assert_eq!(error.publication_status(), "not_published");
        assert_eq!(fs::read(store.statistics_path()).unwrap(), before);
    }

    #[test]
    fn reset_that_publishes_before_cancellation_reports_success() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let cancel = Cancellation::new();
        let state = Statistics { processed_songs: 1, first_recorded_at_unix_ms: Some(4), ..Statistics::default() };
        fs::write(store.statistics_path(), serde_json::to_vec(&state).unwrap()).unwrap();
        let lock = store.lock(&cancel).unwrap();
        let (_, expected) = store.read_locked(&lock).unwrap();
        // The sync callback runs only after the guarded rename succeeded.
        store.publish_locked(&Statistics::default(), &lock, expected, Some(&cancel), |path| {
            cancel.cancel();
            sync_directory(path)
        }).unwrap();
        drop(lock);
        assert!(cancel.is_cancelled());
        assert_eq!(store.show(&Cancellation::new()).unwrap(), Statistics::default());
    }

    #[test]
    fn contended_lock_checks_cancellation_without_waiting_for_release() {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;
        let fixture = Fixture::new();
        let store = fixture.store();
        let held = store.lock(&Cancellation::new()).unwrap();
        let path = fixture.0.join(LOCK_FILE);
        let cancelled = AtomicBool::new(false);
        let notified = AtomicBool::new(false);
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let result = StatisticsLock::acquire(&path, || {
                    if !notified.swap(true, Ordering::SeqCst) { waiting_tx.send(()).unwrap(); }
                    cancelled.load(Ordering::SeqCst)
                });
                result_tx.send(matches!(result, Err(StatisticsError::Cancelled))).unwrap();
            });
            let waiting = waiting_rx.recv_timeout(Duration::from_secs(3));
            cancelled.store(true, Ordering::SeqCst);
            let result = result_rx.recv_timeout(Duration::from_secs(3));
            drop(held);
            assert!(waiting.is_ok(), "worker never observed the contended lock");
            assert_eq!(result, Ok(true));
        });
    }

    #[test]
    fn replacing_lock_while_another_thread_waits_is_rejected() {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;
        let fixture = Fixture::new();
        let store = fixture.store();
        let held = store.lock(&Cancellation::new()).unwrap();
        let path = fixture.0.join(LOCK_FILE);
        let notified = AtomicBool::new(false);
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let result = StatisticsLock::acquire(&path, || {
                    if !notified.swap(true, Ordering::SeqCst) { waiting_tx.send(()).unwrap(); }
                    false
                });
                result_tx.send(matches!(result, Err(StatisticsError::Changed(_)))).unwrap();
            });
            let waiting = waiting_rx.recv_timeout(Duration::from_secs(3));
            fs::rename(&path, fixture.0.join("old-lock")).unwrap();
            fs::write(&path, b"replacement").unwrap();
            let result = result_rx.recv_timeout(Duration::from_secs(3));
            drop(held);
            assert!(waiting.is_ok(), "worker never observed the contended lock");
            assert_eq!(result, Ok(true));
            assert!(!store.statistics_path().exists());
        });
    }
}
