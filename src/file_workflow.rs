//! File discovery, destination planning, and transactional publication.
//!
//! The caller must inspect the source's PN marker and actual stream codec before
//! planning, and finish encoding *and* metadata transfer before publication.
//! A private staging directory is pinned by descriptor; path-only codecs use
//! an inode/descriptor alias rather than the directory's replaceable name.
//! No operation uses the proprietary application.
//!
//! Separate outputs refuse collisions unless explicitly authorized. Source
//! replacement retains a verified, directory-synced backup until successful
//! restoration. Reservations coordinate this module's own jobs. Deliberate
//! mutation by a process with the same privileges remains outside the model;
//! detected external mutation is reported.

use crate::metadata::{Marker, MarkerText};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::{CString, OsStr, OsString, c_char, c_int, c_uint, c_void};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Cooperative cancellation with a shared publication boundary.
///
/// A cancellation that acquires the boundary first prevents publication.
/// Once publication starts, cancellation waits for its commit or rollback.
#[derive(Debug, Default)]
pub struct Cancellation {
    cancelled: std::sync::atomic::AtomicBool,
    publication: Mutex<()>,
}

impl Cancellation {
    pub fn new() -> Self { Self::default() }
    pub fn cancel(&self) {
        let _guard = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool { self.cancelled.load(Ordering::Acquire) }
    /// Hold the same boundary used by cancellation and audio publication.
    /// Callers must check `is_cancelled` while holding it before publishing.
    pub(crate) fn publication_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.publication.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Output choices. `MatchInput` must be resolved using the detected stream
/// codec; a filename extension by itself is not a codec identification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat { Wav, Aiff, Alac, Flac, Mp3, Aac, MatchInput }

/// Actual source stream codec reported by the decoder. `Other` preserves an
/// unrecognized four-character format code for a precise match-input error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceCodec { Lpcm, Mp3, Aac, Alac, Flac, Other(u32) }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputMode {
    SpecificFolder { directory: PathBuf, mirror: bool },
    SameFolder,
    /// Explicitly replace the input and retain a backup in this existing folder.
    Replace { backup_directory: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSettings { pub mode: OutputMode, pub format: AudioFormat }

/// Path admission or an explicitly reported vendor-advertised Ogg type,
/// not a detected audio codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType { Mp3, M4a, Wav, Aiff, Flac, ProtectedM4p, ProtectedWlMp3, OggUnverified }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    /// Absolute, canonical file path. Discovery does not follow symbolic links.
    pub path: PathBuf,
    /// For a folder selection this begins with the selected folder's basename;
    /// for a selected file it consists of just the filename.
    pub relative_path: PathBuf,
    pub source_type: SourceType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingState {
    Unprocessed,
    Marked { backup: Option<PathBuf> },
    Suffixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreStatus { Inactive, Active, Protected }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication { pub destination: PathBuf, pub backup: Option<PathBuf>, pub replaced_existing_output: bool }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub destination: PathBuf, pub backup: PathBuf, pub backup_removed: bool,
    pub backup_removal_error: Option<BackupRemovalError>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRemovalError {
    pub operation: &'static str, pub message: String,
    pub retained_path: Option<PathBuf>, pub unexpected_path: Option<PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortReport { pub retained_backup: Option<PathBuf> }

#[derive(Debug)]
pub struct CleanupFailure { pub operation: &'static str, pub path: PathBuf, pub source: io::Error }

/// Transaction failures retain independent publication, backup, recovery,
/// rollback, and cleanup outcomes. `published` means the destination was
/// actually linked, even if a later verification or cleanup failed.
#[derive(Debug)]
pub enum WorkflowError {
    Io { operation: &'static str, path: PathBuf, source: io::Error },
    Move { from: PathBuf, to: PathBuf, source: io::Error },
    AtomicRenameUnsupported { operation: &'static str, path: PathBuf, source: io::Error },
    InvalidPath { path: PathBuf, reason: &'static str },
    UnsupportedSource(PathBuf),
    ProtectedSource(PathBuf),
    UnsupportedCodec(u32),
    AlreadyProcessed { path: PathBuf, state: ProcessingState },
    DestinationExists(PathBuf),
    DestinationChanged(PathBuf),
    DestinationConflict { destination: PathBuf, other_source: PathBuf },
    RestoreInputConflict { target: PathBuf, backup: PathBuf },
    ReservationExists(PathBuf),
    SourceChanged(PathBuf),
    BackupChanged(PathBuf),
    InvalidMarkerBackup,
    BackupNotRecorded(PathBuf),
    BackupNotFound(PathBuf),
    BackupMismatch { recorded: PathBuf, supplied: PathBuf },
    RestoreNotNeeded(PathBuf),
    NotProcessed(PathBuf),
    RestoreBusy(PathBuf),
    RestoreProtected(PathBuf),
    Cancelled,
    Transaction {
        cause: Box<WorkflowError>, backup: Option<PathBuf>, recovery: Option<PathBuf>,
        rollback: Option<Box<WorkflowError>>, cleanup: Vec<CleanupFailure>, published: bool,
    },
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, path, source } => write!(f, "{operation} {}: {source}", path.display()),
            Self::Move { from, to, source } => write!(f, "move without overwrite {} -> {}: {source}", from.display(), to.display()),
            Self::AtomicRenameUnsupported { operation, path, source } => write!(f, "{operation} is unsupported for {}: {source}", path.display()),
            Self::InvalidPath { path, reason } => write!(f, "invalid path {}: {reason}", path.display()),
            Self::UnsupportedSource(p) => write!(f, "unsupported input {}", p.display()),
            Self::ProtectedSource(p) => write!(f, "protected input {}", p.display()),
            Self::UnsupportedCodec(c) => write!(f, "cannot match source codec 0x{c:08x}"),
            Self::AlreadyProcessed { path, .. } => write!(f, "already processed: {}", path.display()),
            Self::DestinationExists(p) => write!(f, "destination already exists: {}", p.display()),
            Self::DestinationChanged(p) => write!(f, "destination changed after batch preflight: {}", p.display()),
            Self::DestinationConflict { destination, other_source } => write!(f, "destination {} conflicts with {}", destination.display(), other_source.display()),
            Self::RestoreInputConflict { target, backup } => write!(f, "restore target {} aliases backup {}", target.display(), backup.display()),
            Self::ReservationExists(p) => write!(f, "output reservation already exists: {}", p.display()),
            Self::SourceChanged(p) => write!(f, "source changed during operation: {}", p.display()),
            Self::BackupChanged(p) => write!(f, "backup is not the verified copy: {}", p.display()),
            Self::InvalidMarkerBackup => f.write_str("marker contains an invalid backup path"),
            Self::BackupNotRecorded(p) => write!(f, "no backup recorded for {}", p.display()),
            Self::BackupNotFound(p) => write!(f, "recorded backup not found: {}", p.display()),
            Self::BackupMismatch { recorded, supplied } => write!(f, "supplied backup {} differs from recorded {}", supplied.display(), recorded.display()),
            Self::RestoreNotNeeded(p) => write!(f, "restore not needed for suffix-only processed file {}", p.display()),
            Self::NotProcessed(p) => write!(f, "file is not marked processed: {}", p.display()),
            Self::RestoreBusy(p) => write!(f, "cannot restore active file {}", p.display()),
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::RestoreProtected(p) => write!(f, "cannot restore protected file {}", p.display()),
            Self::Transaction { cause, backup, recovery, rollback, cleanup, published } => {
                write!(f, "{cause}; published={published}")?;
                if let Some(p) = backup { write!(f, "; backup path: {}", p.display())?; }
                if let Some(p) = recovery { write!(f, "; retained recovery file: {}", p.display())?; }
                if let Some(e) = rollback { write!(f, "; rollback failed: {e}")?; }
                for e in cleanup { write!(f, "; {} {}: {}", e.operation, e.path.display(), e.source)?; }
                Ok(())
            }
        }
    }
}
impl WorkflowError {
    /// A cancellation remains a cancellation only if no publication, recovery,
    /// rollback, or cleanup failure needs to be reported as an error instead.
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::Transaction { cause, recovery: None, rollback: None, cleanup, published: false, .. }
                if cleanup.is_empty() => cause.is_cancelled(),
            _ => false,
        }
    }
}
impl Error for WorkflowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self { Self::Io { source, .. } | Self::Move { source, .. } | Self::AtomicRenameUnsupported { source, .. } => Some(source), Self::Transaction { cause, .. } => Some(cause), _ => None }
    }
}
fn io_error(operation: &'static str, path: &Path, source: io::Error) -> WorkflowError {
    WorkflowError::Io { operation, path: path.to_owned(), source }
}
fn invalid(path: &Path, reason: &'static str) -> WorkflowError {
    WorkflowError::InvalidPath { path: path.to_owned(), reason }
}

/// The reference's recovered `PNFile::canInitWithPath:` extension table does
/// not contain OGG, despite vendor advertising. We deliberately retain OGG as
/// an explicitly unsupported item instead of silently ignoring it as that
/// admission path would. Classification does not prove a file is decodable.
pub fn source_type(path: &Path) -> Option<SourceType> {
    let name = path.file_name()?.as_bytes();
    if name.len() >= 6 && name[name.len() - 6..].eq_ignore_ascii_case(b"wl.mp3") {
        return Some(SourceType::ProtectedWlMp3);
    }
    let ext = path.extension()?.as_bytes();
    if ext.eq_ignore_ascii_case(b"mp3") { Some(SourceType::Mp3) }
    else if ext.eq_ignore_ascii_case(b"m4a") { Some(SourceType::M4a) }
    else if ext.eq_ignore_ascii_case(b"wav") { Some(SourceType::Wav) }
    else if ext.eq_ignore_ascii_case(b"aif") || ext.eq_ignore_ascii_case(b"aiff") { Some(SourceType::Aiff) }
    else if ext.eq_ignore_ascii_case(b"flac") { Some(SourceType::Flac) }
    else if ext.eq_ignore_ascii_case(b"m4p") { Some(SourceType::ProtectedM4p) }
    else if ext.eq_ignore_ascii_case(b"ogg") { Some(SourceType::OggUnverified) }
    else { None }
}

/// Recursively discover regular admitted files and advertised-but-unsupported
/// OGG files, without following symbolic links. Duplicates are suppressed by
/// canonical absolute path, with the first selection retaining its relative
/// path. Results are ordered by absolute path.
pub fn discover_inputs(inputs: &[PathBuf]) -> Result<Vec<DiscoveredFile>, WorkflowError> {
    let mut found = BTreeMap::<PathBuf, DiscoveredFile>::new();
    for input in inputs {
        let meta = fs::symlink_metadata(input).map_err(|e| io_error("inspect input", input, e))?;
        if meta.file_type().is_symlink() { return Err(invalid(input, "symbolic-link inputs are not followed")); }
        let absolute = fs::canonicalize(input).map_err(|e| io_error("resolve input", input, e))?;
        if meta.is_file() {
            let name = absolute.file_name().ok_or_else(|| invalid(&absolute, "missing filename"))?;
            add_file(&mut found, &absolute, PathBuf::from(name));
        } else if meta.is_dir() {
            let name = absolute.file_name().ok_or_else(|| invalid(&absolute, "selection has no folder name"))?;
            walk_directory(&absolute, Path::new(name), &mut found)?;
        } else { return Err(invalid(input, "input is neither a regular file nor a directory")); }
    }
    Ok(found.into_values().collect())
}
fn add_file(found: &mut BTreeMap<PathBuf, DiscoveredFile>, path: &Path, relative: PathBuf) {
    if let Some(kind) = source_type(path) {
        found.entry(path.to_owned()).or_insert_with(|| DiscoveredFile { path: path.to_owned(), relative_path: relative, source_type: kind });
    }
}
fn walk_directory(dir: &Path, relative: &Path, found: &mut BTreeMap<PathBuf, DiscoveredFile>) -> Result<(), WorkflowError> {
    let entries = fs::read_dir(dir).map_err(|e| io_error("read input directory", dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_error("read input directory entry", dir, e))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| io_error("inspect input entry", &path, e))?;
        if meta.file_type().is_symlink() { continue; }
        let child_relative = relative.join(entry.file_name());
        if meta.is_dir() { walk_directory(&path, &child_relative, found)?; }
        else if meta.is_file() { add_file(found, &path, child_relative); }
    }
    Ok(())
}

/// `marker` must come from a successful source inspection: `None` means the
/// source was inspected and no PN marker was found, not that reading was skipped.
/// A processed marker wins over the exact, case-sensitive `_PN` stem suffix.
pub fn processed_state(path: &Path, marker: Option<&Marker>) -> Result<ProcessingState, WorkflowError> {
    if let Some(marker) = marker.filter(|m| m.is_processed) {
        let backup = match marker.backup_file_name.as_ref() {
            None => None,
            Some(MarkerText::Utf8(s)) if s.is_empty() => None,
            Some(MarkerText::Utf8(s)) => Some(PathBuf::from(s)),
            Some(MarkerText::Utf16(s)) if s.is_empty() => None,
            Some(MarkerText::Utf16(s)) => Some(PathBuf::from(String::from_utf16(s).map_err(|_| WorkflowError::InvalidMarkerBackup)?)),
        };
        return Ok(ProcessingState::Marked { backup });
    }
    Ok(suffix_state(path))
}

fn suffix_state(path: &Path) -> ProcessingState {
    if path.file_stem().is_some_and(|s| s.as_bytes().ends_with(b"_PN")) { ProcessingState::Suffixed }
    else { ProcessingState::Unprocessed }
}

fn existing_directory(path: &Path) -> Result<PathBuf, WorkflowError> {
    let meta = fs::metadata(path).map_err(|e| io_error("inspect directory", path, e))?;
    if !meta.is_dir() { return Err(invalid(path, "not a directory")); }
    fs::canonicalize(path).map_err(|e| io_error("resolve directory", path, e))
}
fn checked_relative(path: &Path) -> Result<(), WorkflowError> {
    if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(invalid(path, "relative path must contain only normal components"));
    }
    Ok(())
}
fn resolved_format(format: AudioFormat, codec: SourceCodec, path: &Path) -> Result<AudioFormat, WorkflowError> {
    if format != AudioFormat::MatchInput { return Ok(format); }
    Ok(match codec {
        SourceCodec::Mp3 => AudioFormat::Mp3, SourceCodec::Aac => AudioFormat::Aac,
        SourceCodec::Alac => AudioFormat::Alac, SourceCodec::Flac => AudioFormat::Flac,
        SourceCodec::Lpcm => match path.extension().map(OsStr::as_bytes) {
            Some(e) if e.eq_ignore_ascii_case(b"flac") => AudioFormat::Flac,
            Some(e) if e.eq_ignore_ascii_case(b"aif") || e.eq_ignore_ascii_case(b"aiff") => AudioFormat::Aiff,
            _ => AudioFormat::Wav,
        },
        SourceCodec::Other(code) => return Err(WorkflowError::UnsupportedCodec(code)),
    })
}
fn extension(format: AudioFormat, source: &Path, requested: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Wav => "wav", AudioFormat::Mp3 => "mp3", AudioFormat::Alac | AudioFormat::Aac => "m4a",
        AudioFormat::Flac => "flac",
        AudioFormat::Aiff if requested == AudioFormat::MatchInput && source.extension().is_some_and(|e| e.as_bytes().eq_ignore_ascii_case(b"aif")) => "aif",
        AudioFormat::Aiff => "aiff", AudioFormat::MatchInput => unreachable!("format is resolved before extension selection"),
    }
}

enum PlanMarker<'a> {
    Inspected(Option<&'a Marker>),
    AuthorizedAmbiguous,
}

/// An immutable plan. Use `validate_batch` before starting jobs, then `begin`
/// to reserve paths and create a private staging workspace.
#[derive(Debug, Clone)]
pub struct OutputPlan {
    source: PathBuf, destination: PathBuf, format: AudioFormat, requested_format: AudioFormat,
    directory_root: PathBuf, mirror_parent: PathBuf, backup_directory: Option<PathBuf>,
    overwrite: Option<Arc<OverwriteGuard>>,
}
impl OutputPlan {
    pub fn new(file: &DiscoveredFile, codec: SourceCodec, settings: &OutputSettings, marker: Option<&Marker>) -> Result<Self, WorkflowError> {
        Self::plan(file, codec, settings, PlanMarker::Inspected(marker))
    }

    // The caller must have inspected the marker and obtained an explicit
    // processing-only override. This is not evidence of marker absence.
    pub(crate) fn for_authorized_ambiguous_marker(file: &DiscoveredFile, codec: SourceCodec, settings: &OutputSettings) -> Result<Self, WorkflowError> {
        Self::plan(file, codec, settings, PlanMarker::AuthorizedAmbiguous)
    }

    fn plan(file: &DiscoveredFile, codec: SourceCodec, settings: &OutputSettings, marker: PlanMarker<'_>) -> Result<Self, WorkflowError> {
        checked_relative(&file.relative_path)?;
        let meta = fs::symlink_metadata(&file.path).map_err(|e| io_error("inspect source", &file.path, e))?;
        if !meta.file_type().is_file() { return Err(invalid(&file.path, "source must be a regular non-symlink file")); }
        let source = fs::canonicalize(&file.path).map_err(|e| io_error("resolve source", &file.path, e))?;
        if source.file_name() != file.relative_path.file_name() { return Err(invalid(&file.relative_path, "relative filename differs from source")); }
        let kind = source_type(&source).ok_or_else(|| WorkflowError::UnsupportedSource(source.clone()))?;
        if kind != file.source_type { return Err(invalid(&source, "source classification changed")); }
        if matches!(kind, SourceType::ProtectedM4p | SourceType::ProtectedWlMp3) { return Err(WorkflowError::ProtectedSource(source)); }
        if kind == SourceType::OggUnverified { return Err(WorkflowError::UnsupportedSource(source)); }
        let state = match marker {
            PlanMarker::Inspected(marker) => processed_state(&source, marker)?,
            PlanMarker::AuthorizedAmbiguous => suffix_state(&source),
        };
        if state != ProcessingState::Unprocessed { return Err(WorkflowError::AlreadyProcessed { path: source, state }); }
        let replace = matches!(&settings.mode, OutputMode::Replace { .. });
        let requested_format = if replace { AudioFormat::MatchInput } else { settings.format };
        let format = resolved_format(requested_format, codec, &source)?;
        let ext = extension(format, &source, requested_format);
        let source_parent = source.parent().ok_or_else(|| invalid(&source, "source has no parent"))?;
        let (directory_root, mirror_parent, backup_directory) = match &settings.mode {
            OutputMode::SpecificFolder { directory, mirror } => {
                let relative = if *mirror { file.relative_path.parent().unwrap_or(Path::new("")).to_owned() } else { PathBuf::new() };
                (existing_directory(directory)?, relative, None)
            }
            OutputMode::SameFolder => (source_parent.to_owned(), PathBuf::new(), None),
            OutputMode::Replace { backup_directory } => (source_parent.to_owned(), PathBuf::new(), Some(existing_directory(backup_directory)?)),
        };
        if let Some(directory) = &backup_directory {
            if directory.as_os_str().to_str().is_none() || source.file_name().and_then(OsStr::to_str).is_none() {
                return Err(invalid(&source, "backup path cannot be represented in the PN marker"));
            }
        }
        let destination = if replace && source.extension().is_some_and(|e| e.as_bytes().eq_ignore_ascii_case(ext.as_bytes())) {
            source.clone()
        } else if replace { source.with_extension(ext) } else {
            let mut name = source.file_stem().ok_or_else(|| invalid(&source, "missing source stem"))?.to_os_string();
            name.push("_PN."); name.push(ext);
            directory_root.join(&mirror_parent).join(name)
        };
        Ok(Self { source, destination, format, requested_format, directory_root, mirror_parent, backup_directory, overwrite: None })
    }
    pub fn source(&self) -> &Path { &self.source }
    pub fn destination(&self) -> &Path { &self.destination }
    pub fn format(&self) -> AudioFormat { self.format }
    pub fn requested_format(&self) -> AudioFormat { self.requested_format }
    pub fn backup_directory(&self) -> Option<&Path> { self.backup_directory.as_deref() }
    pub fn is_replacement(&self) -> bool { self.backup_directory.is_some() }

    /// Enable overwrite only for separate outputs, with every selected input
    /// and every planned destination protected throughout publication.
    pub fn enable_overwrite(&mut self, guard: Arc<OverwriteGuard>) -> Result<(), WorkflowError> {
        if self.is_replacement() { return Err(invalid(&self.source, "overwrite cannot be combined with source replacement")); }
        if !guard.sources.iter().any(|identity| identity.path == self.source) {
            return Err(invalid(&self.source, "overwrite guard does not contain the source"));
        }
        if !guard.destinations.iter().any(|entry| entry.path == self.destination) {
            return Err(invalid(&self.destination, "overwrite guard does not contain the destination"));
        }
        self.overwrite = Some(guard);
        Ok(())
    }

    /// Reserves the destination and, for replacement, the source. A complete,
    /// verified backup is created before returning, so its path can be written
    /// into the PN marker. An abandoned transaction must be explicitly aborted.
    pub fn begin(&self, expected: &FileIdentity) -> Result<StagedOutput, WorkflowError> {
        self.begin_impl(expected, #[cfg(test)] None)
    }
    fn begin_impl(&self, expected: &FileIdentity,
        #[cfg(test)] sync_override: Option<Box<dyn FnOnce(&Path) -> Result<(), WorkflowError>>>) -> Result<StagedOutput, WorkflowError> {
        expected.verify(&self.source)?;
        let source_stamp = expected.stamp;
        create_mirror(&self.directory_root, &self.mirror_parent)?;
        let same_destination = if self.overwrite.is_some() { false }
            else { check_destination(&self.source, &self.destination, self.is_replacement())? };
        let mut reservations = Vec::new();
        let first = if self.is_replacement() { &self.source } else { &self.destination };
        reservations.push(Reservation::acquire(first)?);
        if self.is_replacement() && !same_destination {
            match Reservation::acquire(&self.destination) {
                Ok(reservation) => reservations.push(reservation),
                Err(e) => return Err(transaction_error(e, None, None, None, release_reservations(&mut reservations), false)),
            }
        }
        if let Err(e) = check_stamp(&self.source, source_stamp) {
            return Err(transaction_error(e, None, None, None, release_reservations(&mut reservations), false));
        }
        let destination_stamp = match &self.overwrite {
            Some(guard) => match check_overwrite_destination(&self.destination, guard) {
                Ok(value) => value,
                Err(e) => return Err(transaction_error(e, None, None, None, release_reservations(&mut reservations), false)),
            },
            None => None,
        };
        let parent = self.destination.parent().ok_or_else(|| invalid(&self.destination, "destination has no parent"))?;
        let workspace = match Workspace::create(parent, extension(self.format, &self.source, self.requested_format)) {
            Ok(workspace) => workspace,
            Err(e) => return Err(transaction_error(e, None, None, None, release_reservations(&mut reservations), false)),
        };
        let mut transaction = StagedOutput { plan: self.clone(), workspace, reservations, source_stamp, destination_stamp, backup: None, backup_stamp: None,
            new_mp3_mode: None,
            #[cfg(test)] after_move: None, #[cfg(test)] before_commit: None, #[cfg(test)] after_publish: None,
            #[cfg(test)] after_overwrite_check: None, #[cfg(test)] after_overwrite_swap: None,
            #[cfg(test)] before_overwrite_rollback: None };
        if let Err(error) = transaction.workspace.validate_parent(&self.destination) {
            return Err(transaction.fail(error, None, None, false));
        }
        let parent_check = transaction.reservations.iter().try_for_each(|reservation| {
            if reservation.same_parent(&transaction.workspace.parent).map_err(|e| io_error("inspect reservation parent", &reservation.path, e))? { Ok(()) }
            else { Err(invalid(&reservation.path, "reservation and output workspace have different parents")) }
        });
        if let Err(error) = parent_check { return Err(transaction.fail(error, None, None, false)); }
        if self.format == AudioFormat::Mp3 {
            match transaction.workspace.default_file_mode() {
                Ok(mode) => transaction.new_mp3_mode = Some(mode),
                Err(error) => return Err(transaction.fail(error, None, None, false)),
            }
        }
        if let Some(directory) = &self.backup_directory {
            match create_backup(&self.source, &source_stamp, directory, #[cfg(test)] None) {
                Ok(backup) => {
                    transaction.backup = Some(backup.clone());
                    let backup_stamp = match stamp(&backup, "inspect created backup") {
                        Ok(value) => value, Err(e) => return Err(transaction.fail(e, None, None, false)),
                    };
                    transaction.backup_stamp = Some(backup_stamp);
                    if let Err(e) = verify_backup(&self.source, &backup, backup_stamp) {
                        return Err(transaction.fail(e, None, None, false));
                    }
                    #[cfg(test)]
                    let sync = match sync_override { Some(hook) => hook(&backup), None => sync_backup_directory(&backup) };
                    #[cfg(not(test))]
                    let sync = sync_backup_directory(&backup);
                    if let Err(e) = sync {
                        return Err(transaction.fail(e, None, None, false));
                    }
                }
                Err(e) => return Err(transaction.fail(e, None, None, false)),
            }
        }
        Ok(transaction)
    }
}

/// Detect destination conflicts, including selected sources whose own
/// preflight failed. `selected` must contain the complete discovered batch.
pub fn validate_batch<P: std::borrow::Borrow<OutputPlan>>(plans: &[P], selected: &[DiscoveredFile]) -> Result<(), WorkflowError> {
    validate_batch_with_policy(plans, selected, &destination_name_policy)
}
fn validate_batch_with_policy<P: std::borrow::Borrow<OutputPlan>>(
    plans: &[P], selected: &[DiscoveredFile], policy: &impl Fn(&Path) -> Result<NamePolicy, WorkflowError>,
) -> Result<(), WorkflowError> {
    let mut destinations = Vec::<(&Path, &Path, DestinationParent)>::new();
    let sources: BTreeSet<&Path> = plans.iter().map(|plan| {
        let plan: &OutputPlan = plan.borrow();
        plan.source.as_path()
    }).chain(selected.iter().map(|file| file.path.as_path())).collect();
    let sources = sources.into_iter().map(|source| Ok((source, destination_parent(source)?)))
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    for plan in plans {
        let plan: &OutputPlan = plan.borrow();
        let parent = destination_parent(&plan.destination)?;
        for (other_destination, other_source, other_parent) in &destinations {
            if same_destination_using(&plan.destination, &parent, other_destination, other_parent, policy)? {
                return Err(WorkflowError::DestinationConflict { destination: plan.destination.clone(), other_source: (*other_source).to_owned() });
            }
        }
        for (source, source_parent) in &sources {
            if plan.is_replacement() && plan.destination == plan.source && *source == plan.source.as_path() { continue; }
            if same_destination_using(&plan.destination, &parent, source, source_parent, policy)? {
                return Err(WorkflowError::DestinationConflict { destination: plan.destination.clone(), other_source: (*source).to_owned() });
            }
        }
        destinations.push((&plan.destination, &plan.source, parent));
        if let Some(guard) = &plan.overwrite { check_overwrite_destination(&plan.destination, guard)?; }
    }
    Ok(())
}

fn create_mirror(root: &Path, relative: &Path) -> Result<(), WorkflowError> {
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(name) = component else { return Err(invalid(relative, "invalid mirrored path")); };
        current.push(name);
        match fs::create_dir(&current) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(io_error("create mirrored directory", &current, e)),
        }
        let meta = fs::symlink_metadata(&current).map_err(|e| io_error("inspect mirrored directory", &current, e))?;
        if !meta.file_type().is_dir() { return Err(invalid(&current, "mirrored component is not a non-symlink directory")); }
    }
    Ok(())
}
fn check_destination(source: &Path, dest: &Path, replacement: bool) -> Result<bool, WorkflowError> {
    match fs::symlink_metadata(dest) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_error("inspect destination", dest, e)),
        Ok(meta) if replacement && meta.file_type().is_file() => {
            let real = fs::canonicalize(dest).map_err(|e| io_error("resolve destination", dest, e))?;
            if real == source { Ok(true) } else { Err(WorkflowError::DestinationExists(dest.to_owned())) }
        }
        Ok(_) => Err(WorkflowError::DestinationExists(dest.to_owned())),
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(allocator: *const c_void, bytes: *const u8, length: isize, encoding: u32, external: u8) -> *const c_void;
    fn CFStringCreateMutableCopy(allocator: *const c_void, max_length: isize, source: *const c_void) -> *mut c_void;
    fn CFStringNormalize(string: *mut c_void, form: isize);
    fn CFStringFold(string: *mut c_void, flags: usize, locale: *const c_void);
    fn CFStringCompare(left: *const c_void, right: *const c_void, flags: usize) -> isize;
    fn CFRelease(value: *const c_void);
}

#[cfg(target_os = "macos")]
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn CreateTextEncoding(base: u32, variant: u32, format: u32) -> u32;
    fn TECCreateConverter(converter: *mut *mut c_void, input: u32, output: u32) -> i32;
    fn TECConvertText(converter: *mut c_void, input: *const u8, length: usize, consumed: *mut usize,
        output: *mut u8, capacity: usize, written: *mut usize) -> i32;
    fn TECFlushText(converter: *mut c_void, output: *mut u8, capacity: usize, written: *mut usize) -> i32;
    fn TECDisposeConverter(converter: *mut c_void) -> i32;
    fn UCCompareTextNoLocale(options: u32, left: *const u16, left_len: usize, right: *const u16, right_len: usize,
        equivalent: *mut u8, order: *mut i32) -> i32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameNormalization { Apfs, HfsPlus, Bytes }
#[derive(Debug, Clone, Copy)]
struct NamePolicy { normalization: NameNormalization, case_sensitive: bool }

fn policy_for_fs_type(fs_type: &[u8], case_sensitive: bool) -> NamePolicy {
    let normalization = match fs_type {
        b"apfs" => NameNormalization::Apfs,
        b"hfs" | b"hfsx" => NameNormalization::HfsPlus,
        _ => NameNormalization::Bytes,
    };
    NamePolicy { normalization, case_sensitive }
}
#[cfg(any(target_os = "macos", test))]
fn pathconf_case_sensitive(result: libc::c_long) -> bool { result > 0 }

#[derive(Debug)]
struct DestinationParent { dev: u64, ino: u64, missing: Vec<OsString>, ancestor: PathBuf }

fn destination_parent(destination: &Path) -> Result<DestinationParent, WorkflowError> {
    let mut ancestor = destination.parent().ok_or_else(|| invalid(destination, "destination has no parent"))?;
    let mut missing = Vec::new();
    let metadata = loop {
        match fs::metadata(ancestor) {
            Ok(meta) if meta.is_dir() => break meta,
            Ok(_) => return Err(invalid(ancestor, "destination ancestor is not a directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name().ok_or_else(|| invalid(ancestor, "missing ancestor has no name"))?.to_owned());
                ancestor = ancestor.parent().ok_or_else(|| invalid(destination, "destination has no existing ancestor"))?;
            }
            Err(error) => return Err(io_error("inspect destination ancestor", ancestor, error)),
        }
    };
    missing.reverse();
    Ok(DestinationParent { dev: metadata.dev(), ino: metadata.ino(), missing, ancestor: ancestor.to_owned() })
}

#[cfg(target_os = "macos")]
fn destination_name_policy(ancestor: &Path) -> Result<NamePolicy, WorkflowError> {
    let path = CString::new(ancestor.as_os_str().as_bytes()).map_err(|_| invalid(ancestor, "path contains NUL"))?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    let mut fs_type = [0u8; 16];
    // An unknown or unsupported filesystem uses byte comparison.
    if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } == 0 {
        let info = unsafe { info.assume_init() };
        for (out, value) in fs_type.iter_mut().zip(info.f_fstypename) { *out = value as u8; }
    }
    let end = fs_type.iter().position(|&byte| byte == 0).unwrap_or(fs_type.len());
    // Negative means indeterminate or unsupported, not case-sensitive.
    let sensitivity = unsafe { libc::pathconf(path.as_ptr(), 11) };
    Ok(policy_for_fs_type(&fs_type[..end], pathconf_case_sensitive(sensitivity)))
}

#[cfg(target_os = "linux")]
fn linux_case_sensitive(flags: Option<libc::c_long>) -> bool {
    const FS_CASEFOLD_FL: libc::c_long = 0x4000_0000;
    flags.is_some_and(|flags| flags & FS_CASEFOLD_FL == 0)
}
#[cfg(target_os = "linux")]
fn destination_name_policy(ancestor: &Path) -> Result<NamePolicy, WorkflowError> {
    const FS_IOC_GETFLAGS: libc::c_ulong = 0x80086601;
    let case_sensitive = match File::open(ancestor) {
        Ok(directory) => {
            let mut flags: libc::c_long = 0;
            let status = unsafe { libc::ioctl(directory.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) };
            linux_case_sensitive((status == 0).then_some(flags))
        }
        Err(_) => linux_case_sensitive(None),
    };
    Ok(NamePolicy { normalization: NameNormalization::Bytes, case_sensitive })
}

#[cfg(test)]
fn same_destination(left: &Path, left_parent: &DestinationParent, right: &Path, right_parent: &DestinationParent) -> Result<bool, WorkflowError> {
    same_destination_using(left, left_parent, right, right_parent, &destination_name_policy)
}
fn same_destination_using(left: &Path, left_parent: &DestinationParent, right: &Path, right_parent: &DestinationParent,
    resolve: &impl Fn(&Path) -> Result<NamePolicy, WorkflowError>) -> Result<bool, WorkflowError> {
    if (left_parent.dev, left_parent.ino) != (right_parent.dev, right_parent.ino)
        || left_parent.missing.len() != right_parent.missing.len() { return Ok(false); }
    let left_name = left.file_name().ok_or_else(|| invalid(left, "destination has no filename"))?;
    let right_name = right.file_name().ok_or_else(|| invalid(right, "destination has no filename"))?;
    if left_name == right_name && left_parent.missing == right_parent.missing { return Ok(true); }
    let policy = resolve(&left_parent.ancestor)?;
    same_destination_with_policy(left_name, left_parent, right_name, right_parent, policy)
}

fn same_destination_with_policy(left_name: &OsStr, left_parent: &DestinationParent, right_name: &OsStr,
    right_parent: &DestinationParent, policy: NamePolicy) -> Result<bool, WorkflowError> {
    if (left_parent.dev, left_parent.ino) != (right_parent.dev, right_parent.ino)
        || left_parent.missing.len() != right_parent.missing.len() { return Ok(false); }
    for (a, b) in left_parent.missing.iter().map(OsString::as_os_str).chain(std::iter::once(left_name))
        .zip(right_parent.missing.iter().map(OsString::as_os_str).chain(std::iter::once(right_name))) {
        if !names_equal(a, b, policy)? { return Ok(false); }
    }
    Ok(true)
}

fn names_equal(left: &OsStr, right: &OsStr, policy: NamePolicy) -> Result<bool, WorkflowError> {
    if left == right { return Ok(true); }
    let a = left.as_bytes(); let b = right.as_bytes();
    if a.is_ascii() && b.is_ascii() { return Ok(!policy.case_sensitive && a.eq_ignore_ascii_case(b)); }
    let (Ok(a_text), Ok(b_text)) = (std::str::from_utf8(a), std::str::from_utf8(b)) else {
        return Ok(!policy.case_sensitive && a.eq_ignore_ascii_case(b));
    };
    match policy.normalization {
        NameNormalization::Bytes => {
            if policy.case_sensitive { return Ok(false); }
            #[cfg(target_os = "macos")]
            { let _ = (a_text, b_text); cf_names_equal(a, b, false, false) }
            #[cfg(target_os = "linux")]
            { Ok(a_text.chars().flat_map(char::to_lowercase).eq(b_text.chars().flat_map(char::to_lowercase))) }
        }
        #[cfg(target_os = "macos")]
        NameNormalization::Apfs => cf_names_equal(a, b, policy.case_sensitive, true),
        #[cfg(target_os = "macos")]
        NameNormalization::HfsPlus => hfs_names_equal(a, b, policy.case_sensitive),
        #[cfg(not(target_os = "macos"))]
        NameNormalization::Apfs | NameNormalization::HfsPlus => Err(invalid(Path::new(left), "macOS filename policy is unavailable")),
    }
}

#[cfg(target_os = "macos")]
fn cf_names_equal(left: &[u8], right: &[u8], case_sensitive: bool, normalize: bool) -> Result<bool, WorkflowError> {
    fn make_string(bytes: &[u8], case_sensitive: bool, normalize: bool) -> Result<*mut c_void, WorkflowError> {
        let length = isize::try_from(bytes.len()).map_err(|_| invalid(Path::new(""), "name is too long to compare"))?;
        let original = unsafe { CFStringCreateWithBytes(std::ptr::null(), bytes.as_ptr(), length, 0x08000100, 0) };
        if original.is_null() { return Err(invalid(Path::new(""), "cannot create Unicode name")); }
        let value = unsafe { CFStringCreateMutableCopy(std::ptr::null(), 0, original) };
        unsafe { CFRelease(original); }
        if value.is_null() { return Err(invalid(Path::new(""), "cannot allocate normalized name")); }
        unsafe {
            if normalize { CFStringNormalize(value, 0); }
            if !case_sensitive {
                CFStringFold(value, 1, std::ptr::null());
                if normalize { CFStringNormalize(value, 0); }
            }
        }
        Ok(value)
    }
    let a = make_string(left, case_sensitive, normalize)?;
    let b = match make_string(right, case_sensitive, normalize) {
        Ok(value) => value,
        Err(error) => { unsafe { CFRelease(a) }; return Err(error); }
    };
    let equal = unsafe { CFStringCompare(a, b, 0) == 0 };
    unsafe { CFRelease(a); CFRelease(b); }
    Ok(equal)
}

#[cfg(target_os = "macos")]
fn hfs_decompose(name: &[u8]) -> Result<Vec<u8>, WorkflowError> {
    // The HFS+ TEC mapping is fixed to Unicode 3.2 and preserves its excluded ranges.
    let capacity = name.len().checked_mul(8).and_then(|size| size.checked_add(32))
        .ok_or_else(|| invalid(Path::new(""), "HFS name is too long"))?;
    let input = unsafe { CreateTextEncoding(0x0106, 0, 2) };
    let output = unsafe { CreateTextEncoding(0x0106, 8, 2) };
    let mut converter = std::ptr::null_mut();
    let status = unsafe { TECCreateConverter(&mut converter, input, output) };
    if status != 0 || converter.is_null() {
        if !converter.is_null() { unsafe { TECDisposeConverter(converter) }; }
        return Err(invalid(Path::new(""), "cannot create HFS name converter"));
    }
    let mut result = vec![0u8; capacity]; let mut consumed = 0; let mut written = 0;
    let status = unsafe { TECConvertText(converter, name.as_ptr(), name.len(), &mut consumed, result.as_mut_ptr(), result.len(), &mut written) };
    let mut flushed = 0;
    let flush = if status == 0 && consumed == name.len() && written <= result.len() {
        unsafe { TECFlushText(converter, result.as_mut_ptr().add(written), result.len() - written, &mut flushed) }
    } else { -1 };
    let dispose = unsafe { TECDisposeConverter(converter) };
    if status != 0 || flush != 0 || dispose != 0 || consumed != name.len()
        || written > result.len() || flushed > result.len() - written {
        return Err(invalid(Path::new(""), "cannot normalize HFS filename"));
    }
    result.truncate(written + flushed);
    Ok(result)
}

#[cfg(target_os = "macos")]
fn hfs_names_equal(left: &[u8], right: &[u8], case_sensitive: bool) -> Result<bool, WorkflowError> {
    let left = hfs_decompose(left)?; let right = hfs_decompose(right)?;
    if case_sensitive { return Ok(left == right); }
    let left = std::str::from_utf8(&left).map_err(|_| invalid(Path::new(""), "invalid HFS conversion"))?;
    let right = std::str::from_utf8(&right).map_err(|_| invalid(Path::new(""), "invalid HFS conversion"))?;
    let left: Vec<u16> = left.encode_utf16().collect(); let right: Vec<u16> = right.encode_utf16().collect();
    let mut equivalent = 0u8; let mut order = 0i32;
    // kUCCollateTypeHFSExtended and kUCCollateCaseInsensitiveMask.
    let status = unsafe { UCCompareTextNoLocale((1 << 24) | 8, left.as_ptr(), left.len(), right.as_ptr(), right.len(), &mut equivalent, &mut order) };
    if status != 0 { return Err(invalid(Path::new(""), "cannot compare HFS filenames")); }
    Ok(equivalent != 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp { dev: u64, ino: u64, len: u64, mtime: i64, mtime_nsec: i64, ctime: i64, ctime_nsec: i64 }
fn metadata_stamp(meta: &fs::Metadata) -> Stamp {
    Stamp { dev: meta.dev(), ino: meta.ino(), len: meta.len(), mtime: meta.mtime(), mtime_nsec: meta.mtime_nsec(), ctime: meta.ctime(), ctime_nsec: meta.ctime_nsec() }
}
fn stamp(path: &Path, operation: &'static str) -> Result<Stamp, WorkflowError> {
    let meta = fs::symlink_metadata(path).map_err(|e| io_error(operation, path, e))?;
    if !meta.file_type().is_file() { return Err(invalid(path, "not a regular non-symlink file")); }
    Ok(metadata_stamp(&meta))
}
fn owner_metadata(file: &File, path: &Path) -> Result<fs::Metadata, WorkflowError> {
    let mut meta = file.metadata().map_err(|e| io_error("inspect private ownership", path, e))?;
    for _ in 0..2 {
        if meta.uid() == unsafe { libc::geteuid() } { return Ok(meta); }
        let next = file.metadata().map_err(|e| io_error("reinspect private ownership", path, e))?;
        if metadata_stamp(&next) != metadata_stamp(&meta) || next.mode() != meta.mode() || next.nlink() != meta.nlink() {
            return Err(WorkflowError::SourceChanged(path.to_owned()));
        }
        meta = next;
    }
    if meta.uid() == unsafe { libc::geteuid() } { Ok(meta) }
    else { Err(invalid(path, "private ownership could not be established")) }
}
fn access_metadata(file: &File, path: &Path) -> Result<fs::Metadata, WorkflowError> {
    let mut previous = file.metadata().map_err(|e| io_error("inspect access metadata", path, e))?;
    for _ in 0..3 {
        let next = file.metadata().map_err(|e| io_error("reinspect access metadata", path, e))?;
        if metadata_stamp(&next) != metadata_stamp(&previous) || next.mode() != previous.mode() || next.nlink() != previous.nlink() {
            return Err(WorkflowError::SourceChanged(path.to_owned()));
        }
        if (next.uid(), next.gid()) == (previous.uid(), previous.gid()) { return Ok(next); }
        previous = next;
    }
    Err(invalid(path, "filesystem could not consistently establish file ownership"))
}

/// Identity captured before reading source metadata or a restoration marker.
/// Verify it before committing decisions made from that inspection.
#[derive(Debug, Clone)]
pub struct FileIdentity { path: PathBuf, stamp: Stamp, file: Arc<File> }

impl FileIdentity {
    pub fn capture(path: &Path) -> Result<Self, WorkflowError> {
        let file = open_identity(path)?;
        let opened = file.metadata().map_err(|e| io_error("inspect pinned source", path, e))?;
        let stamp = metadata_stamp(&opened);
        if stamp != self::stamp(path, "inspect file before marker read")? {
            return Err(WorkflowError::SourceChanged(path.to_owned()));
        }
        let path = fs::canonicalize(path).map_err(|e| io_error("resolve marker inspection file", path, e))?;
        Ok(Self { path, stamp, file: Arc::new(file) })
    }

    pub fn verify(&self, path: &Path) -> Result<(), WorkflowError> {
        let current = fs::canonicalize(path).map_err(|e| io_error("resolve inspected file", path, e))?;
        if current != self.path { return Err(WorkflowError::SourceChanged(path.to_owned())); }
        let opened = self.file.metadata().map_err(|e| io_error("inspect pinned source", path, e))?;
        if metadata_stamp(&opened) != self.stamp { return Err(WorkflowError::SourceChanged(path.to_owned())); }
        check_stamp(path, self.stamp)
    }
}

/// A backup identity and digest frozen before any member of a restore batch runs.
#[derive(Debug, Clone)]
pub struct FrozenBackup { identity: FileIdentity, digest: [u8; 32] }
impl FrozenBackup {
    pub fn capture(path: &Path) -> Result<Self, WorkflowError> {
        let identity = FileIdentity::capture(path)?;
        let file = open_read(path)?;
        let opened = file.metadata().map_err(|e| io_error("inspect open backup", path, e))?;
        if metadata_stamp(&opened) != identity.stamp { return Err(WorkflowError::BackupChanged(path.to_owned())); }
        let digest = content_digest(&file, path)?;
        identity.verify(path).map_err(|_| WorkflowError::BackupChanged(path.to_owned()))?;
        Ok(Self { identity, digest })
    }

    fn verify(&self, path: &Path) -> Result<(), WorkflowError> {
        self.identity.verify(path).map_err(|_| WorkflowError::BackupChanged(path.to_owned()))?;
        let file = open_read(path)?;
        let opened = file.metadata().map_err(|e| io_error("inspect frozen backup", path, e))?;
        if metadata_stamp(&opened) != self.identity.stamp || content_digest(&file, path)? != self.digest {
            return Err(WorkflowError::BackupChanged(path.to_owned()));
        }
        self.identity.verify(path).map_err(|_| WorkflowError::BackupChanged(path.to_owned()))
    }

    fn verify_after_alias_removal(&self, path: &Path) -> Result<Stamp, WorkflowError> {
        let real = fs::canonicalize(path).map_err(|_| WorkflowError::BackupChanged(path.to_owned()))?;
        if real != self.identity.path { return Err(WorkflowError::BackupChanged(path.to_owned())); }
        let current = stamp(path, "inspect frozen backup after alias removal")?;
        if !same_moved_file(current, self.identity.stamp) { return Err(WorkflowError::BackupChanged(path.to_owned())); }
        let file = open_read(path)?;
        let opened = file.metadata().map_err(|e| io_error("inspect frozen backup", path, e))?;
        if metadata_stamp(&opened) != current || content_digest(&file, path)? != self.digest
            || stamp(path, "reinspect frozen backup")? != current {
            return Err(WorkflowError::BackupChanged(path.to_owned()));
        }
        Ok(current)
    }
}

/// One up-front snapshot for every target and backup in a CLI restore request.
pub struct RestoreBatchSnapshot {
    targets: Vec<FileIdentity>, backups: Vec<FrozenBackup>, groups: Vec<Vec<usize>>,
}
impl RestoreBatchSnapshot {
    pub fn capture(targets: &[PathBuf], backups: &[PathBuf]) -> Result<Self, WorkflowError> {
        Self::capture_impl(targets, backups, #[cfg(test)] None)
    }
    fn capture_impl(targets: &[PathBuf], backups: &[PathBuf],
        #[cfg(test)] mut after_capture: Option<Box<dyn FnMut(usize) + Send>>) -> Result<Self, WorkflowError> {
        if targets.len() != backups.len() { return Err(WorkflowError::InvalidMarkerBackup); }
        let mut target_ids = Vec::with_capacity(targets.len());
        let mut backup_ids: Vec<FrozenBackup> = Vec::with_capacity(backups.len());
        let mut by_path = BTreeMap::<PathBuf, usize>::new();
        let mut by_inode = BTreeMap::<(u64, u64), Vec<usize>>::new();
        for (index, (target, backup)) in targets.iter().zip(backups).enumerate() {
            if !backup.is_absolute() { return Err(WorkflowError::InvalidMarkerBackup); }
            target_ids.push(FileIdentity::capture(target)?);
            let frozen = FrozenBackup::capture(backup)?;
            if let Some(&previous) = by_path.get(&frozen.identity.path) {
                let prior = &backup_ids[previous];
                if frozen.identity.stamp != prior.identity.stamp || frozen.digest != prior.digest {
                    return Err(WorkflowError::BackupChanged(backup.to_owned()));
                }
            } else { by_path.insert(frozen.identity.path.clone(), index); }
            let stamp = frozen.identity.stamp;
            if let Some(indices) = by_inode.get(&(stamp.dev, stamp.ino)) {
                let prior = &backup_ids[indices[0]];
                if stamp != prior.identity.stamp || frozen.digest != prior.digest {
                    return Err(WorkflowError::BackupChanged(backup.to_owned()));
                }
            }
            by_inode.entry((stamp.dev, stamp.ino)).or_default().push(index);
            backup_ids.push(frozen);
            #[cfg(test)] if let Some(hook) = after_capture.as_mut() { hook(index); }
        }
        for target in &target_ids {
            for backup in &backup_ids {
                if target.path == backup.identity.path || same_identity(target.stamp, backup.identity.stamp) {
                    return Err(WorkflowError::RestoreInputConflict { target: target.path.clone(), backup: backup.identity.path.clone() });
                }
            }
        }
        for target in &target_ids { target.verify(&target.path)?; }
        for &index in by_path.values() {
            let backup = &backup_ids[index];
            backup.verify(&backup.identity.path)?;
        }
        Ok(Self { targets: target_ids, backups: backup_ids, groups: by_inode.into_values().collect() })
    }
    pub fn target(&self, index: usize) -> &FileIdentity { &self.targets[index] }
    pub fn backup(&self, index: usize) -> &FrozenBackup { &self.backups[index] }
    pub fn groups(&self) -> &[Vec<usize>] { &self.groups }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationSnapshot { Missing, Regular(Stamp), NonRegular }
#[derive(Debug)]
struct GuardedDestination { path: PathBuf, snapshot: DestinationSnapshot, _pin: Option<File> }

/// Immutable batch-wide protection and destination snapshots for overwrites.
#[derive(Debug)]
pub struct OverwriteGuard { sources: Vec<FileIdentity>, destinations: Vec<GuardedDestination> }
impl OverwriteGuard {
    pub fn new(inputs: &[DiscoveredFile], destinations: &[PathBuf]) -> Result<Self, WorkflowError> {
        let sources = inputs.iter().map(|input| FileIdentity::capture(&input.path)).collect::<Result<Vec<_>, _>>()?;
        let destinations = destinations.iter().map(|path| {
            let snapshot = destination_snapshot(path)?;
            let pin = if let DestinationSnapshot::Regular(expected) = snapshot {
                let file = open_identity(path)?;
                let meta = file.metadata().map_err(|e| io_error("inspect pinned overwrite destination", path, e))?;
                if metadata_stamp(&meta) != expected { return Err(WorkflowError::DestinationChanged(path.clone())); }
                Some(file)
            } else { None };
            Ok(GuardedDestination { path: path.clone(), snapshot, _pin: pin })
        }).collect::<Result<Vec<_>, WorkflowError>>()?;
        Ok(Self { sources, destinations })
    }
}

fn destination_snapshot(path: &Path) -> Result<DestinationSnapshot, WorkflowError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DestinationSnapshot::Missing),
        Err(error) => Err(io_error("inspect overwrite destination", path, error)),
        Ok(meta) if meta.file_type().is_file() => Ok(DestinationSnapshot::Regular(metadata_stamp(&meta))),
        Ok(_) => Ok(DestinationSnapshot::NonRegular),
    }
}

fn check_overwrite_destination(destination: &Path, guard: &OverwriteGuard) -> Result<Option<Stamp>, WorkflowError> {
    for source in &guard.sources {
        source.verify(&source.path)?;
        if destination == source.path.as_path() {
            return Err(WorkflowError::DestinationConflict { destination: destination.to_owned(), other_source: source.path.clone() });
        }
    }
    let snapshot = destination_snapshot(destination)?;
    let expected = guard.destinations.iter().find(|entry| entry.path == destination)
        .ok_or_else(|| invalid(destination, "overwrite guard does not contain the destination"))?.snapshot;
    if snapshot == DestinationSnapshot::NonRegular { return Err(WorkflowError::DestinationExists(destination.to_owned())); }
    if snapshot != expected { return Err(WorkflowError::DestinationChanged(destination.to_owned())); }
    let target = match snapshot {
        DestinationSnapshot::Missing => return Ok(None),
        DestinationSnapshot::Regular(target) => target,
        DestinationSnapshot::NonRegular => unreachable!(),
    };
    for source in &guard.sources {
        if same_identity(target, source.stamp) {
            return Err(WorkflowError::DestinationConflict { destination: destination.to_owned(), other_source: source.path.clone() });
        }
    }
    for entry in &guard.destinations {
        let other = &entry.path;
        if other == destination { continue; }
        match fs::metadata(other) {
            Ok(meta) if meta.dev() == target.dev && meta.ino() == target.ino => {
                return Err(WorkflowError::DestinationConflict { destination: destination.to_owned(), other_source: other.clone() });
            }
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(io_error("inspect other batch destination", other, error)),
        }
    }
    Ok(Some(target))
}
fn same_identity(a: Stamp, b: Stamp) -> bool { a.dev == b.dev && a.ino == b.ino }
fn same_moved_file(a: Stamp, b: Stamp) -> bool {
    same_identity(a, b) && a.len == b.len && a.mtime == b.mtime && a.mtime_nsec == b.mtime_nsec
}
fn check_stamp(path: &Path, expected: Stamp) -> Result<(), WorkflowError> {
    if stamp(path, "inspect source")? == expected { Ok(()) } else { Err(WorkflowError::SourceChanged(path.to_owned())) }
}
// Values and signatures in this section are from the macOS SDK's copyfile.h,
// sys/xattr.h, sys/acl.h, sys/attr.h, sys/fcntl.h and unistd.h.
const O_NOFOLLOW: c_int = libc::O_NOFOLLOW;
#[cfg(target_os = "macos")]
const COPYFILE_METADATA: u32 = (1 << 0) | (1 << 1) | (1 << 2);
#[cfg(target_os = "macos")]
const XATTR_SHOWCOMPRESSION: c_int = 0x0020;
#[cfg(target_os = "macos")]
const ACL_TYPE_EXTENDED: c_int = 0x00000100;
#[cfg(target_os = "macos")]
const ATTR_CMN_CRTIME: u32 = 0x00000200;
#[cfg(target_os = "macos")]
const ATTR_CMN_FLAGS: u32 = 0x00040000;

#[cfg(target_os = "macos")]
#[repr(C)]
struct AttrList {
    bitmapcount: u16, reserved: u16, commonattr: u32,
    volattr: u32, dirattr: u32, fileattr: u32, forkattr: u32,
}
#[cfg(target_os = "macos")]
impl AttrList {
    fn common(commonattr: u32) -> Self {
        Self { bitmapcount: 5, reserved: 0, commonattr, volattr: 0, dirattr: 0, fileattr: 0, forkattr: 0 }
    }
}

#[cfg(target_os = "macos")]
#[link(name = "System")]
unsafe extern "C" {
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    fn fcopyfile(from: c_int, to: c_int, state: *mut c_void, flags: u32) -> c_int;
    fn flistxattr(fd: c_int, names: *mut c_char, size: usize, options: c_int) -> isize;
    fn fgetxattr(fd: c_int, name: *const c_char, value: *mut c_void, size: usize, position: u32, options: c_int) -> isize;
    fn fsetxattr(fd: c_int, name: *const c_char, value: *const c_void, size: usize, position: u32, options: c_int) -> c_int;
    fn fremovexattr(fd: c_int, name: *const c_char, options: c_int) -> c_int;
    fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
    fn acl_size(acl: *mut c_void) -> isize;
    fn acl_copy_ext(buffer: *mut c_void, acl: *mut c_void, size: isize) -> isize;
    fn acl_free(object: *mut c_void) -> c_int;
    fn fgetattrlist(fd: c_int, attributes: *mut AttrList, buffer: *mut c_void, size: usize, options: c_uint) -> c_int;
    fn fsetattrlist(fd: c_int, attributes: *mut AttrList, buffer: *const c_void, size: usize, options: c_uint) -> c_int;
    #[cfg(not(feature = "portable"))]
    fn CC_SHA256_Init(context: *mut Sha256Context) -> c_int;
    #[cfg(not(feature = "portable"))]
    fn CC_SHA256_Update(context: *mut Sha256Context, data: *const c_void, length: u32) -> c_int;
    #[cfg(not(feature = "portable"))]
    fn CC_SHA256_Final(digest: *mut u8, context: *mut Sha256Context) -> c_int;
}
/// Darwin ACLs can grant access independently of the mode bits. A private
/// workspace/state object must not inherit an ACL we cannot prove safe.
#[cfg(target_os = "macos")]
pub(crate) fn require_no_extended_acl(file: &File, path: &Path) -> Result<(), WorkflowError> {
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(io_error("inspect private ACL", path, error)) };
    }
    unsafe { acl_free(acl); }
    Err(invalid(path, "extended ACL is not allowed on a private workspace or state file"))
}
#[cfg(target_os = "linux")]
pub(crate) fn require_no_extended_acl(_file: &File, _path: &Path) -> Result<(), WorkflowError> { Ok(()) }

fn open_read(path: &Path) -> Result<File, WorkflowError> {
    OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(path)
        .map_err(|e| io_error("open file without following symlinks", path, e))
}
fn open_identity(path: &Path) -> Result<File, WorkflowError> {
    let path_c = CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid(path, "path contains NUL"))?;
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH | O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "macos")]
    let flags = libc::O_EVTONLY | O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "linux")]
    let fd = unsafe { libc::open(path_c.as_ptr(), flags) };
    #[cfg(target_os = "macos")]
    let fd = {
        let fd = unsafe { libc::open(path_c.as_ptr(), flags) };
        if fd < 0 && io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied {
            // O_EVTONLY may still require read access. An existing output that
            // is writable but unreadable can be pinned without truncation.
            unsafe { libc::open(path_c.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | O_NOFOLLOW | libc::O_CLOEXEC) }
        } else { fd }
    };
    if fd < 0 { return Err(io_error("pin file identity", path, io::Error::last_os_error())); }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata().map_err(|e| io_error("inspect pinned file identity", path, e))?;
    if !meta.file_type().is_file() { return Err(invalid(path, "not a regular non-symlink file")); }
    Ok(file)
}

// getattrlist prefixes each returned, four-byte-packed attribute with a u32
// total size. A Darwin 64-bit timespec has two 64-bit fields.
#[cfg(target_os = "macos")]
fn creation_time(file: &File, path: &Path) -> Result<Option<[u8; 16]>, WorkflowError> {
    let mut attrs = AttrList::common(ATTR_CMN_CRTIME);
    let mut buffer = [0u8; 20];
    // SAFETY: the descriptor is live and the SDK-defined attribute buffer has
    // room for the length prefix and one 64-bit Darwin timespec.
    if unsafe { fgetattrlist(file.as_raw_fd(), &mut attrs, buffer.as_mut_ptr().cast(), buffer.len(), 0) } != 0 {
        return Err(io_error("read file creation time", path, io::Error::last_os_error()));
    }
    if u32::from_ne_bytes(buffer[..4].try_into().unwrap()) != buffer.len() as u32 {
        return Err(invalid(path, "unexpected creation-time attribute length"));
    }
    Ok(Some(buffer[4..].try_into().unwrap()))
}

#[cfg(target_os = "linux")]
fn creation_time(file: &File, path: &Path) -> Result<Option<[u8; 16]>, WorkflowError> {
    let mut value = std::mem::MaybeUninit::<libc::statx>::uninit();
    let status = unsafe { libc::statx(file.as_raw_fd(), c"".as_ptr(), libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        libc::STATX_BTIME, value.as_mut_ptr()) };
    if status != 0 {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)) { return Ok(None); }
        return Err(io_error("read file creation time", path, error));
    }
    let value = unsafe { value.assume_init() };
    if value.stx_mask & libc::STATX_BTIME == 0 { return Ok(None); }
    let mut time = [0u8; 16];
    time[..8].copy_from_slice(&value.stx_btime.tv_sec.to_ne_bytes());
    time[8..].copy_from_slice(&i64::from(value.stx_btime.tv_nsec).to_ne_bytes());
    Ok(Some(time))
}

#[cfg(target_os = "macos")]
fn copy_creation_time(source: &File, dest: &File, source_path: &Path, dest_path: &Path) -> Result<(), WorkflowError> {
    let time = creation_time(source, source_path)?.ok_or_else(|| invalid(source_path, "creation time is unavailable"))?;
    let mut attrs = AttrList::common(ATTR_CMN_CRTIME);
    // SAFETY: `time` contains exactly the timespec returned by fgetattrlist.
    if unsafe { fsetattrlist(dest.as_raw_fd(), &mut attrs, time.as_ptr().cast(), time.len(), 0) } != 0 {
        return Err(io_error("set copied file creation time", dest_path, io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn file_flags(file: &File, path: &Path) -> Result<Option<u32>, WorkflowError> {
    let mut attrs = AttrList::common(ATTR_CMN_FLAGS);
    let mut buffer = [0u32; 2];
    // SAFETY: the descriptor and fixed-size length/flags buffer are valid.
    if unsafe { fgetattrlist(file.as_raw_fd(), &mut attrs, buffer.as_mut_ptr().cast(), std::mem::size_of_val(&buffer), 0) } != 0 {
        return Err(io_error("read file flags", path, io::Error::last_os_error()));
    }
    if buffer[0] != std::mem::size_of_val(&buffer) as u32 { return Err(invalid(path, "unexpected file-flags attribute length")); }
    Ok(Some(buffer[1]))
}

#[cfg(target_os = "linux")]
fn file_flags(file: &File, path: &Path) -> Result<Option<u32>, WorkflowError> {
    const FS_IOC_GETFLAGS: libc::c_ulong = 0x80086601;
    let mut flags: libc::c_long = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) } == 0 { return Ok(Some(flags as u32)); }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::ENOTTY | libc::EINVAL | libc::EOPNOTSUPP | libc::ENOSYS)) { Ok(None) }
    else { Err(io_error("read file flags", path, error)) }
}

#[cfg(target_os = "macos")]
fn acl_bytes(file: &File, path: &Path) -> Result<Option<Vec<u8>>, WorkflowError> {
    // SAFETY: the descriptor is live; the returned ACL is released below.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // Darwin filesec_get_property reports ENOENT for an absent ACL.
        return if error.kind() == io::ErrorKind::NotFound { Ok(None) }
            else { Err(io_error("read file ACL", path, error)) };
    }
    // SAFETY: acl_get_fd_np returned a live ACL.
    let size = unsafe { acl_size(acl) };
    let result = if size < 0 {
        Err(io_error("size file ACL", path, io::Error::last_os_error()))
    } else {
        let mut bytes = vec![0u8; size as usize];
        // SAFETY: the buffer has the size requested by acl_size.
        let written = unsafe { acl_copy_ext(bytes.as_mut_ptr().cast(), acl, size) };
        if written != size {
            Err(io_error("serialize file ACL", path, if written < 0 { io::Error::last_os_error() }
                else { io::Error::new(io::ErrorKind::InvalidData, "short ACL serialization") }))
        } else { Ok(Some(bytes)) }
    };
    // SAFETY: the ACL was returned by acl_get_fd_np and has not been freed.
    unsafe { acl_free(acl); }
    result
}

#[cfg(target_os = "linux")]
fn acl_bytes(_file: &File, _path: &Path) -> Result<Option<Vec<u8>>, WorkflowError> {
    // Linux POSIX ACLs are represented and compared as system.posix_acl_* xattrs.
    Ok(None)
}

fn comparable_xattr(name: &[u8]) -> bool {
    // macOS assigns provenance to the creating/writing process; copyfile
    // cannot reproduce another process's value, even when fsetxattr succeeds.
    !cfg!(target_os = "macos") || name != b"com.apple.provenance"
}

fn list_xattrs(file: &File, buffer: *mut c_char, size: usize) -> isize {
    #[cfg(target_os = "macos")]
    { unsafe { flistxattr(file.as_raw_fd(), buffer, size, XATTR_SHOWCOMPRESSION) } }
    #[cfg(target_os = "linux")]
    { unsafe { libc::flistxattr(file.as_raw_fd(), buffer, size) } }
}
fn get_xattr(file: &File, name: &CString, buffer: *mut c_void, size: usize, offset: u32) -> isize {
    #[cfg(target_os = "macos")]
    { unsafe { fgetxattr(file.as_raw_fd(), name.as_ptr(), buffer, size, offset, XATTR_SHOWCOMPRESSION) } }
    #[cfg(target_os = "linux")]
    {
        if offset != 0 { return -1; }
        unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), buffer, size) }
    }
}
fn set_xattr(file: &File, path: &Path, name: &CString, value: &[u8], offset: u32) -> Result<(), WorkflowError> {
    #[cfg(target_os = "macos")]
    let status = unsafe { fsetxattr(file.as_raw_fd(), name.as_ptr(), value.as_ptr().cast(), value.len(), offset, 0) };
    #[cfg(target_os = "linux")]
    let status = {
        if offset != 0 { return Err(invalid(path, "Linux extended attributes do not support offsets")); }
        unsafe { libc::fsetxattr(file.as_raw_fd(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) }
    };
    if status == 0 { Ok(()) } else { Err(io_error("restore copied extended attribute", path, io::Error::last_os_error())) }
}
fn remove_xattr(file: &File, path: &Path, name: &CString) -> Result<(), WorkflowError> {
    #[cfg(target_os = "macos")]
    let status = unsafe { fremovexattr(file.as_raw_fd(), name.as_ptr(), 0) };
    #[cfg(target_os = "linux")]
    let status = unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) };
    if status == 0 { Ok(()) } else { Err(io_error("remove copied extended attribute", path, io::Error::last_os_error())) }
}

fn xattr_names(file: &File, path: &Path) -> Result<Vec<CString>, WorkflowError> {
    // SAFETY: a null buffer with zero length asks for the current list size.
    let size = list_xattrs(file, std::ptr::null_mut(), 0);
    if size < 0 { return Err(io_error("list extended attributes", path, io::Error::last_os_error())); }
    if size == 0 { return Ok(Vec::new()); }
    let mut buffer = vec![0u8; size as usize];
    // SAFETY: the buffer is writable for the advertised size.
    let count = list_xattrs(file, buffer.as_mut_ptr().cast(), buffer.len());
    if count < 0 { return Err(io_error("list extended attributes", path, io::Error::last_os_error())); }
    buffer.truncate(count as usize);
    if buffer.last() != Some(&0) { return Err(invalid(path, "invalid extended-attribute name list")); }
    let mut names = buffer[..buffer.len() - 1].split(|b| *b == 0)
        .filter(|name| comparable_xattr(name))
        .map(|name| CString::new(name).map_err(|_| invalid(path, "invalid extended-attribute name")))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

fn xattr_size(file: &File, path: &Path, name: &CString) -> Result<usize, WorkflowError> {
    // SAFETY: name is NUL-terminated; a null buffer requests the attribute size.
    let size = get_xattr(file, name, std::ptr::null_mut(), 0, 0);
    if size < 0 { Err(io_error("size extended attribute", path, io::Error::last_os_error())) } else { Ok(size as usize) }
}

fn read_xattr(file: &File, path: &Path, name: &CString, buffer: &mut [u8], offset: u32) -> Result<(), WorkflowError> {
    // SAFETY: name and the writable buffer remain live for the call.
    #[cfg(target_os = "linux")]
    if offset != 0 { return Err(invalid(path, "Linux extended attributes do not support offsets")); }
    let count = get_xattr(file, name, buffer.as_mut_ptr().cast(), buffer.len(), offset);
    if count < 0 { return Err(io_error("read extended attribute", path, io::Error::last_os_error())); }
    if count as usize != buffer.len() { return Err(invalid(path, "extended attribute changed during verification")); }
    Ok(())
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[repr(C)]
struct Sha256Context { count: [u32; 2], hash: [u32; 8], buffer: [u32; 16] }

struct WorkflowDigest {
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    context: Sha256Context,
    #[cfg(any(target_os = "linux", feature = "portable"))]
    context: crate::portable::digest::Sha256,
}
impl WorkflowDigest {
    fn new(path: &Path) -> Result<Self, WorkflowError> {
        #[cfg(all(target_os = "macos", not(feature = "portable")))]
        {
            let mut context = Sha256Context { count: [0; 2], hash: [0; 8], buffer: [0; 16] };
            if unsafe { CC_SHA256_Init(&mut context) } != 1 { return Err(invalid(path, "cannot initialize SHA-256")); }
            Ok(Self { context })
        }
        #[cfg(any(target_os = "linux", feature = "portable"))]
        { let _ = path; Ok(Self { context: crate::portable::digest::Sha256::new() }) }
    }
    fn update(&mut self, bytes: &[u8], path: &Path) -> Result<(), WorkflowError> {
        #[cfg(all(target_os = "macos", not(feature = "portable")))]
        {
            let length = u32::try_from(bytes.len()).map_err(|_| invalid(path, "SHA-256 input is too large"))?;
            if unsafe { CC_SHA256_Update(&mut self.context, bytes.as_ptr().cast(), length) } != 1 {
                return Err(invalid(path, "cannot update SHA-256"));
            }
        }
        #[cfg(any(target_os = "linux", feature = "portable"))]
        { let _ = path; self.context.update(bytes); }
        Ok(())
    }
    fn finish(mut self, path: &Path) -> Result<[u8; 32], WorkflowError> {
        #[cfg(all(target_os = "macos", not(feature = "portable")))]
        {
            let mut digest = [0u8; 32];
            if unsafe { CC_SHA256_Final(digest.as_mut_ptr(), &mut self.context) } != 1 {
                return Err(invalid(path, "cannot finalize SHA-256"));
            }
            Ok(digest)
        }
        #[cfg(any(target_os = "linux", feature = "portable"))]
        { let _ = path; Ok(self.context.finalize()) }
    }
}

fn content_digest(file: &File, path: &Path) -> Result<[u8; 32], WorkflowError> {
    let before = file.metadata().map_err(|e| io_error("inspect file for digest", path, e))?;
    let mut context = WorkflowDigest::new(path)?;
    let mut buffer = [0u8; 65536];
    let mut offset = 0u64;
    while offset < before.len() {
        let wanted = (before.len() - offset).min(buffer.len() as u64) as usize;
        let count = file.read_at(&mut buffer[..wanted], offset).map_err(|e| io_error("read file for digest", path, e))?;
        if count == 0 { return Err(invalid(path, "file shortened during digest")); }
        context.update(&buffer[..count], path)?;
        offset += count as u64;
    }
    let digest = context.finish(path)?;
    let after = file.metadata().map_err(|e| io_error("reinspect file after digest", path, e))?;
    if metadata_stamp(&before) != metadata_stamp(&after) { return Err(WorkflowError::SourceChanged(path.to_owned())); }
    Ok(digest)
}

#[derive(PartialEq, Eq)]
struct OverwriteMetadata {
    mode: u32, uid: u32, gid: u32, flags: Option<u32>, creation: Option<[u8; 16]>,
    acl: Option<Vec<u8>>, xattrs: Vec<(CString, usize, [u8; 32])>,
}

fn overwrite_metadata(file: &File, path: &Path) -> Result<OverwriteMetadata, WorkflowError> {
    let before = file.metadata().map_err(|e| io_error("inspect output metadata", path, e))?;
    let mut xattrs = Vec::new();
    for name in xattr_names(file, path)? {
        let size = xattr_size(file, path, &name)?;
        let mut context = WorkflowDigest::new(path)?;
        if name.as_bytes() == b"com.apple.ResourceFork" {
            if size > u32::MAX as usize { return Err(invalid(path, "resource fork is too large to verify")); }
            let mut buffer = [0u8; 65536];
            let mut offset = 0usize;
            while offset < size {
                let count = (size - offset).min(buffer.len());
                read_xattr(file, path, &name, &mut buffer[..count], offset as u32)?;
                context.update(&buffer[..count], path)?;
                offset += count;
            }
        } else if size != 0 {
            u32::try_from(size).map_err(|_| invalid(path, "extended attribute is too large to verify"))?;
            let mut buffer = vec![0u8; size];
            read_xattr(file, path, &name, &mut buffer, 0)?;
            context.update(&buffer, path)?;
        }
        let digest = context.finish(path)?;
        xattrs.push((name, size, digest));
    }
    let result = OverwriteMetadata { mode: before.mode(), uid: before.uid(), gid: before.gid(),
        flags: file_flags(file, path)?, creation: creation_time(file, path)?, acl: acl_bytes(file, path)?, xattrs };
    let after = file.metadata().map_err(|e| io_error("reinspect output metadata", path, e))?;
    if metadata_stamp(&before) != metadata_stamp(&after) { return Err(WorkflowError::DestinationChanged(path.to_owned())); }
    Ok(result)
}

fn xattr_values_equal(a: &File, b: &File, a_path: &Path, b_path: &Path, name: &CString) -> Result<bool, WorkflowError> {
    let size = xattr_size(a, a_path, name)?;
    if size != xattr_size(b, b_path, name)? { return Ok(false); }
    if size == 0 { return Ok(true); }
    if name.as_bytes() == b"com.apple.ResourceFork" {
        if size > u32::MAX as usize { return Err(invalid(a_path, "resource fork is too large to verify")); }
        let mut left = [0u8; 65536]; let mut right = [0u8; 65536];
        let mut offset = 0usize;
        while offset < size {
            let count = (size - offset).min(left.len());
            read_xattr(a, a_path, name, &mut left[..count], offset as u32)?;
            read_xattr(b, b_path, name, &mut right[..count], offset as u32)?;
            if left[..count] != right[..count] { return Ok(false); }
            offset += count;
        }
    } else {
        // Darwin only supports offset reads for resource forks.
        let mut left = vec![0u8; size]; let mut right = vec![0u8; size];
        read_xattr(a, a_path, name, &mut left, 0)?;
        read_xattr(b, b_path, name, &mut right, 0)?;
        if left != right { return Ok(false); }
    }
    Ok(true)
}

fn xattrs_equal(a: &File, b: &File, a_path: &Path, b_path: &Path) -> Result<bool, WorkflowError> {
    let names = xattr_names(a, a_path)?;
    if names != xattr_names(b, b_path)? { return Ok(false); }
    for name in names {
        if !xattr_values_equal(a, b, a_path, b_path, &name)? { return Ok(false); }
    }
    Ok(true)
}

fn reconcile_copied_xattrs(source: &File, dest: &File, source_path: &Path, dest_path: &Path, final_mode: Option<u32>) -> Result<(), WorkflowError> {
    let source_names = xattr_names(source, source_path)?;
    let dest_names = xattr_names(dest, dest_path)?;
    for name in &dest_names {
        if source_names.binary_search(name).is_err() {
            // The private destination is owned by this transaction.
            remove_xattr(dest, dest_path, name)?;
        }
    }
    for pass in 0..2 {
      if pass == 1 {
        if let Some(mode) = final_mode {
            if unsafe { libc::fchmod(dest.as_raw_fd(), mode as libc::mode_t) } != 0 {
                return Err(io_error("restore copied file mode", dest_path, io::Error::last_os_error()));
            }
        }
      }
      for name in &source_names {
        let deferred_acl = cfg!(target_os = "linux") && name.as_bytes().starts_with(b"system.posix_acl_");
        if deferred_acl != (pass == 1) { continue; }
        let present = dest_names.binary_search(name).is_ok();
        if present && xattr_values_equal(source, dest, source_path, dest_path, name)? { continue; }
        let size = xattr_size(source, source_path, name)?;
        if name.as_bytes() == b"com.apple.ResourceFork" {
            if size > u32::MAX as usize { return Err(invalid(source_path, "resource fork is too large to copy")); }
            if present { remove_xattr(dest, dest_path, name)?; }
            let mut buffer = [0u8; 65536]; let mut offset = 0usize;
            while offset < size {
                let count = (size - offset).min(buffer.len());
                read_xattr(source, source_path, name, &mut buffer[..count], offset as u32)?;
                set_xattr(dest, dest_path, name, &buffer[..count], offset as u32)?;
                offset += count;
            }
            if size == 0 { set_xattr(dest, dest_path, name, &[], 0)?; }
        } else {
            let mut value = vec![0u8; size];
            if size != 0 { read_xattr(source, source_path, name, &mut value, 0)?; }
            set_xattr(dest, dest_path, name, &value, 0)?;
        }
    }
    }
    Ok(())
}

fn metadata_equal(a: &File, b: &File, a_path: &Path, b_path: &Path, compare_access_time: bool) -> Result<bool, WorkflowError> {
    let left = a.metadata().map_err(|e| io_error("inspect file metadata", a_path, e))?;
    let right = b.metadata().map_err(|e| io_error("inspect file metadata", b_path, e))?;
    if left.mode() != right.mode() || left.uid() != right.uid() || left.gid() != right.gid()
        || left.mtime() != right.mtime() || left.mtime_nsec() != right.mtime_nsec()
        || (compare_access_time && (left.atime() != right.atime() || left.atime_nsec() != right.atime_nsec())) {
        return Ok(false);
    }
    #[cfg(target_os = "macos")]
    let platform_metadata_equal = creation_time(a, a_path)? == creation_time(b, b_path)?
        && file_flags(a, a_path)? == file_flags(b, b_path)?;
    // Linux cannot set statx birth time; filesystem flags are not part of the
    // POSIX copy contract. Both are still checked on the same inode for swaps.
    #[cfg(target_os = "linux")]
    let platform_metadata_equal = true;
    Ok(platform_metadata_equal && acl_bytes(a, a_path)? == acl_bytes(b, b_path)?
        && xattrs_equal(a, b, a_path, b_path)?)
}

fn files_equal(a: &Path, b: &Path) -> Result<bool, WorkflowError> {
    let mut a_file = open_read(a)?;
    let mut b_file = open_read(b)?;
    let a_len = a_file.metadata().map_err(|e| io_error("inspect verification file", a, e))?.len();
    let b_len = b_file.metadata().map_err(|e| io_error("inspect verification file", b, e))?.len();
    if a_len != b_len { return Ok(false); }
    let mut left = [0u8; 65536]; let mut right = [0u8; 65536];
    let mut remaining = a_len;
    while remaining != 0 {
        let count = remaining.min(left.len() as u64) as usize;
        a_file.read_exact(&mut left[..count]).map_err(|e| io_error("read verification file", a, e))?;
        b_file.read_exact(&mut right[..count]).map_err(|e| io_error("read verification file", b, e))?;
        if left[..count] != right[..count] { return Ok(false); }
        remaining -= count as u64;
    }
    Ok(true)
}
/// Check identity, non-aliasing, bytes and preservable metadata on both sides.
fn verify_backup(original: &Path, backup: &Path, expected: Stamp) -> Result<(), WorkflowError> {
    let original_before = stamp(original, "inspect original for backup verification")?;
    let backup_before = stamp(backup, "inspect backup for verification")?;
    if backup_before != expected || same_identity(original_before, backup_before) {
        return Err(WorkflowError::BackupChanged(backup.to_owned()));
    }
    if !files_equal(original, backup)? { return Err(WorkflowError::BackupChanged(backup.to_owned())); }
    let original_file = open_read(original)?;
    let backup_file = open_read(backup)?;
    // Verification reads may update access times independently; access times
    // are checked immediately after copying, before any verification read.
    if !metadata_equal(&original_file, &backup_file, original, backup, false)? {
        return Err(WorkflowError::BackupChanged(backup.to_owned()));
    }
    if stamp(original, "reinspect original after backup verification")? != original_before {
        return Err(WorkflowError::SourceChanged(original.to_owned()));
    }
    if stamp(backup, "reinspect backup after verification")? != expected {
        return Err(WorkflowError::BackupChanged(backup.to_owned()));
    }
    Ok(())
}
fn copy_new_verified(source: &Path, dest: &Path, expected: Stamp, workspace: Option<&Workspace>) -> Result<(), WorkflowError> {
    check_stamp(source, expected)?;
    let mut input = open_read(source)?;
    let opened = input.metadata().map_err(|e| io_error("inspect open copy source", source, e))?;
    if opened.dev() != expected.dev || opened.ino() != expected.ino { return Err(WorkflowError::SourceChanged(source.to_owned())); }
    let mut output = if let Some(workspace) = workspace {
        workspace.create_stage()?
    } else {
        OpenOptions::new().write(true).read(true).create_new(true).mode(0o600).open(dest)
            .map_err(|e| io_error("create private copy", dest, e))?
    };
    io::copy(&mut input, &mut output).map_err(|e| io_error("copy file", dest, e))?;
    check_stamp(source, expected)?;
    // Metadata copying can affect compressed files' backing streams; compare
    // the final data fork after it has completed.
    // Creation time is not copied by COPYFILE_STAT. Set it before metadata
    // copying, which can restore immutable flags and make later writes fail.
    #[cfg(target_os = "macos")]
    {
        copy_creation_time(&input, &output, source, dest)?;
        if unsafe { fcopyfile(input.as_raw_fd(), output.as_raw_fd(), std::ptr::null_mut(), COPYFILE_METADATA) } != 0 {
            return Err(io_error("copy file metadata", dest, io::Error::last_os_error()));
        }
        reconcile_copied_xattrs(&input, &output, source, dest, None)?;
    }
    #[cfg(target_os = "linux")]
    {
        let metadata = input.metadata().map_err(|e| io_error("inspect copy metadata", source, e))?;
        let created = output.metadata().map_err(|e| io_error("inspect copied file", dest, e))?;
        if (metadata.uid(), metadata.gid()) != (created.uid(), created.gid())
            && unsafe { libc::fchown(output.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
            return Err(io_error("restore copied ownership", dest, io::Error::last_os_error()));
        }
        reconcile_copied_xattrs(&input, &output, source, dest, Some(metadata.mode() & 0o7777))?;
        let times = [
            libc::timespec { tv_sec: metadata.atime() as libc::time_t, tv_nsec: metadata.atime_nsec() as libc::c_long },
            libc::timespec { tv_sec: metadata.mtime() as libc::time_t, tv_nsec: metadata.mtime_nsec() as libc::c_long },
        ];
        if unsafe { libc::futimens(output.as_raw_fd(), times.as_ptr()) } != 0 {
            return Err(io_error("restore copied timestamps", dest, io::Error::last_os_error()));
        }
    }
    output.sync_all().map_err(|e| io_error("sync copied file", dest, e))?;
    if !metadata_equal(&input, &output, source, dest, true)? {
        return Err(invalid(dest, "copied file metadata differs from source"));
    }
    if !files_equal(source, dest)? { return Err(WorkflowError::SourceChanged(source.to_owned())); }
    check_stamp(source, expected)
}

fn directory_fd_path(file: &File) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok();
    #[cfg(target_os = "macos")]
    let path = {
        let mut buffer = [0u8; libc::PATH_MAX as usize];
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } == 0 {
            let end = buffer.iter().position(|&byte| byte == 0).unwrap_or(buffer.len());
            Some(PathBuf::from(OsStr::from_bytes(&buffer[..end])))
        } else { None }
    };
    let held = file.metadata().ok()?;
    path.filter(|path| fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == (held.dev(), held.ino())))
}
static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);
fn remove_pinned_directory(parent: &File, name: &CString, identity: (u64, u64)) -> io::Result<()> {
    let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), named.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let named = unsafe { named.assume_init() };
    if named.st_mode & libc::S_IFMT != libc::S_IFDIR || (named.st_dev as u64, named.st_ino as u64) != identity {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "workspace directory was replaced; retained"));
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(crate) struct Workspace {
    directory: PathBuf,
    alias: PathBuf,
    staged: PathBuf,
    parent: File,
    name: CString,
    file: File,
    identity: (u64, u64),
    stage_file: Mutex<Option<File>>,
    #[cfg(test)]
    sync_error: Mutex<Option<io::Error>>,
}
impl Workspace {
    pub(crate) fn create(parent: &Path, ext: &str) -> Result<Self, WorkflowError> {
        Self::create_impl(parent, ext, #[cfg(test)] false, #[cfg(test)] None)
    }
    fn create_impl(parent: &Path, ext: &str, #[cfg(test)] fail_alias: bool,
        #[cfg(test)] mut after_create: Option<Box<dyn FnOnce(&Path)>>) -> Result<Self, WorkflowError> {
        let parent_fd = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent).map_err(|e| io_error("open workspace parent", parent, e))?;
        for _ in 0..128 {
            let id = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!(".pn-workflow-{}-{id:x}", std::process::id())).unwrap();
            let dir = parent.join(OsStr::from_bytes(name.as_bytes()));
            if unsafe { libc::mkdirat(parent_fd.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::AlreadyExists { continue; }
                return Err(io_error("create private workspace", &dir, e));
            }
            let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstatat(parent_fd.as_raw_fd(), name.as_ptr(), named.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                let cause = io_error("inspect created workspace", &dir, io::Error::last_os_error());
                let cleanup = CleanupFailure { operation: "remove unpinned workspace", path: dir,
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "workspace identity unavailable; directory retained") };
                return Err(transaction_error(cause, None, None, None, vec![cleanup], false));
            }
            let named = unsafe { named.assume_init() };
            let created_identity = (named.st_dev as u64, named.st_ino as u64);
            #[cfg(test)] if let Some(hook) = after_create.take() { hook(&dir); }
            let fd = unsafe { libc::openat(parent_fd.as_raw_fd(), name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | O_NOFOLLOW | libc::O_CLOEXEC) };
            if fd < 0 {
                let cause = io_error("pin private workspace", &dir, io::Error::last_os_error());
                let cleanup = CleanupFailure { operation: "remove unpinned workspace", path: dir,
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "workspace identity unavailable; directory retained") };
                return Err(transaction_error(cause, None, None, None, vec![cleanup], false));
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = match file.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    let cause = io_error("inspect created workspace", &dir, error);
                    let cleanup = CleanupFailure { operation: "remove unpinned workspace", path: dir,
                        source: io::Error::new(io::ErrorKind::PermissionDenied, "workspace identity unavailable; directory retained") };
                    return Err(transaction_error(cause, None, None, None, vec![cleanup], false));
                }
            };
            let identity = (metadata.dev(), metadata.ino());
            if named.st_mode & libc::S_IFMT != libc::S_IFDIR || identity != created_identity {
                let cause = invalid(&dir, "workspace changed before it could be pinned");
                let cleanup = CleanupFailure { operation: "remove unpinned workspace", path: dir,
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "created workspace was moved or replaced; location unavailable") };
                return Err(transaction_error(cause, None, None, None, vec![cleanup], false));
            }
            let prepare = (|| {
                let meta = owner_metadata(&file, &dir)?;
                if !meta.is_dir() || (meta.dev(), meta.ino()) != identity || meta.mode() & 0o777 != 0o700 {
                    return Err(invalid(&dir, "private workspace ownership or permissions changed"));
                }
                require_no_extended_acl(&file, &dir)?;
                #[cfg(target_os = "macos")]
                let alias = PathBuf::from(format!("/.vol/{}/{}", identity.0, identity.1));
                #[cfg(target_os = "linux")]
                let alias = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
                #[cfg(test)] if fail_alias { return Err(invalid(&dir, "injected workspace alias failure")); }
                let alias_meta = fs::metadata(&alias).map_err(|e| io_error("resolve pinned workspace", &dir, e))?;
                if (alias_meta.dev(), alias_meta.ino()) != identity {
                    return Err(invalid(&dir, "pinned workspace alias identifies another directory"));
                }
                Ok(alias)
            })();
            let alias = match prepare {
                Ok(alias) => alias,
                Err(cause) => {
                    let cleanup = match remove_pinned_directory(&parent_fd, &name, identity) {
                        Ok(()) => Vec::new(),
                        Err(source) => vec![CleanupFailure { operation: "remove failed workspace", path: dir, source }],
                    };
                    return Err(if cleanup.is_empty() { cause } else { transaction_error(cause, None, None, None, cleanup, false) });
                }
            };
            return Ok(Self { staged: alias.join(format!("output.{ext}")), directory: dir, alias,
                parent: parent_fd, name, file, identity, stage_file: Mutex::new(None),
                #[cfg(test)] sync_error: Mutex::new(None) });
        }
        Err(invalid(parent, "could not allocate a unique workspace"))
    }
    pub(crate) fn staging_path(&self) -> &Path { &self.staged }
    pub(crate) fn link_stage(&self, destination: &Path) -> Result<(), WorkflowError> {
        if destination.parent() != self.directory.parent() {
            return Err(invalid(destination, "destination is not in the pinned workspace parent"));
        }
        self.validate_parent(destination)?;
        self.stage_stamp("inspect private publication stage")?;
        link_no_replace(self, &self.staged, destination)
    }
    #[cfg(test)]
    pub(crate) fn directory_path(&self) -> &Path { &self.directory }
    pub(crate) fn mark_stage(&self) -> Result<(), WorkflowError> { self.remember_stage().map(|_| ()) }
    fn displaced(&self) -> PathBuf { self.alias.join("original") }
    fn argument(&self, path: &Path) -> Result<(c_int, CString), WorkflowError> {
        if path.parent() == Some(self.alias.as_path()) {
            let name = path.file_name().ok_or_else(|| invalid(path, "workspace entry has no name"))?;
            Ok((self.file.as_raw_fd(), CString::new(name.as_bytes()).map_err(|_| invalid(path, "path contains NUL"))?))
        } else if path.parent() == self.directory.parent() {
            let name = path.file_name().ok_or_else(|| invalid(path, "parent entry has no name"))?;
            Ok((self.parent.as_raw_fd(), CString::new(name.as_bytes()).map_err(|_| invalid(path, "path contains NUL"))?))
        } else {
            Ok((libc::AT_FDCWD, CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid(path, "path contains NUL"))?))
        }
    }
    fn open_entry(&self, path: &Path) -> Result<File, WorkflowError> {
        let (dir, name) = self.argument(path)?;
        let fd = unsafe { libc::openat(dir, name.as_ptr(), libc::O_RDONLY | O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 { return Err(io_error("open private file", path, io::Error::last_os_error())); }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().map_err(|e| io_error("inspect private file", path, e))?;
        if !meta.file_type().is_file() { return Err(invalid(path, "private entry is not a regular file")); }
        Ok(file)
    }
    fn observe_stage(&self) -> Result<(File, Stamp), WorkflowError> {
        let file = self.open_entry(&self.staged)?;
        let meta = owner_metadata(&file, &self.staged)?;
        let (dir, name) = self.argument(&self.staged)?;
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatat(dir, name.as_ptr(), named.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io_error("inspect named private stage", &self.staged, io::Error::last_os_error()));
        }
        let named = unsafe { named.assume_init() };
        if named.st_mode & libc::S_IFMT != libc::S_IFREG
            || (named.st_dev as u64, named.st_ino as u64) != (meta.dev(), meta.ino())
            || meta.uid() != unsafe { libc::geteuid() } || meta.nlink() != 1 {
            return Err(invalid(&self.staged, "private stage identity or ownership changed"));
        }
        Ok((file, metadata_stamp(&meta)))
    }
    fn remember_stage(&self) -> Result<Stamp, WorkflowError> {
        let (file, stamp) = self.observe_stage()?;
        let mut owned = self.stage_file.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pinned) = owned.as_ref() {
            let previous = pinned.metadata().map_err(|e| io_error("inspect pinned private stage", &self.staged, e))?;
            if (previous.dev(), previous.ino()) != (stamp.dev, stamp.ino) {
                return Err(invalid(&self.staged, "private stage was replaced; retained"));
            }
        } else {
            *owned = Some(file);
        }
        Ok(stamp)
    }
    fn accept_rewritten_stage(&self) -> Result<(), WorkflowError> {
        let (file, _) = self.observe_stage()?;
        let mut owned = self.stage_file.lock().unwrap_or_else(|e| e.into_inner());
        if owned.is_none() { return Err(invalid(&self.staged, "cannot adopt a rewrite of an unrecorded stage")); }
        // Keep the replacement descriptor live across every subsequent check.
        *owned = Some(file);
        Ok(())
    }
    pub(crate) fn sync_parent(&self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(error) = self.sync_error.lock().unwrap_or_else(|e| e.into_inner()).take() { return Err(error); }
        self.parent.sync_all()
    }
    fn set_stage_file(&self, file: File) {
        *self.stage_file.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
    }
    pub(crate) fn create_stage(&self) -> Result<File, WorkflowError> {
        let mut owned = self.stage_file.lock().unwrap_or_else(|e| e.into_inner());
        if owned.is_some() { return Err(invalid(&self.staged, "private stage was already recorded")); }
        let (dir, name) = self.argument(&self.staged)?;
        let fd = unsafe { libc::openat(dir, name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | O_NOFOLLOW | libc::O_CLOEXEC, 0o600) };
        if fd < 0 { return Err(io_error("create private stage", &self.staged, io::Error::last_os_error())); }
        // Store the exclusive descriptor before any fallible initialization.
        *owned = Some(unsafe { File::from_raw_fd(fd) });
        owned.as_ref().unwrap().try_clone().map_err(|e| io_error("duplicate private stage", &self.staged, e))
    }
    fn stage_stamp(&self, operation: &'static str) -> Result<Stamp, WorkflowError> {
        let owned = self.stage_file.lock().unwrap_or_else(|e| e.into_inner());
        let file = owned.as_ref().ok_or_else(|| invalid(&self.staged, "private stage is not pinned"))?;
        let meta = file.metadata().map_err(|e| io_error(operation, &self.staged, e))?;
        let (dir, name) = self.argument(&self.staged)?;
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatat(dir, name.as_ptr(), named.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io_error(operation, &self.staged, io::Error::last_os_error()));
        }
        let named = unsafe { named.assume_init() };
        if !meta.file_type().is_file() || named.st_mode & libc::S_IFMT != libc::S_IFREG
            || (meta.dev(), meta.ino()) != (named.st_dev as u64, named.st_ino as u64) {
            return Err(WorkflowError::SourceChanged(self.staged.clone()));
        }
        Ok(metadata_stamp(&meta))
    }
    fn check_stage_stamp(&self, expected: Stamp) -> Result<(), WorkflowError> {
        if self.stage_stamp("inspect pinned stage")? == expected { Ok(()) }
        else { Err(WorkflowError::SourceChanged(self.staged.clone())) }
    }
    fn default_file_mode(&self) -> Result<u32, WorkflowError> {
        let name = c".pn-mode-probe";
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | O_NOFOLLOW | libc::O_CLOEXEC, 0o666) };
        if fd < 0 { return Err(io_error("create mode probe", &self.directory, io::Error::last_os_error())); }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().map_err(|e| io_error("inspect mode probe", &self.directory, e))?;
        let probe = self.alias.join(".pn-mode-probe");
        self.unlink_owned(&probe, (meta.dev(), meta.ino())).map_err(|e| io_error("remove mode probe", &self.directory, e))?;
        Ok(meta.mode() & 0o7777)
    }
    fn set_stage_mode(&self, mode: u32) -> Result<(), WorkflowError> {
        let file = self.open_entry(&self.staged)?;
        let metadata = file.metadata().map_err(|e| io_error("inspect stage mode", &self.staged, e))?;
        let owned = self.stage_file.lock().unwrap_or_else(|e| e.into_inner());
        let expected = owned.as_ref().map(|file| file.metadata()).transpose()
            .map_err(|e| io_error("inspect pinned private stage", &self.staged, e))?;
        if expected.as_ref().map(|m| (m.dev(), m.ino())) != Some((metadata.dev(), metadata.ino())) {
            return Err(invalid(&self.staged, "private stage was replaced; retained"));
        }
        if metadata.mode() & 0o7777 != mode && unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(io_error("set final output mode", &self.staged, io::Error::last_os_error()));
        }
        if file.metadata().map_err(|e| io_error("reinspect stage mode", &self.staged, e))?.mode() & 0o7777 != mode {
            return Err(invalid(&self.staged, "could not preserve final output mode"));
        }
        Ok(())
    }
    fn set_overwrite_access(&self, source: &File, source_path: &Path, expected: Stamp) -> Result<(), WorkflowError> {
        let original = access_metadata(source, source_path)?;
        if metadata_stamp(&original) != expected { return Err(WorkflowError::DestinationChanged(source_path.to_owned())); }
        let stage = self.open_entry(&self.staged)?;
        let pinned = self.stage_stamp("inspect overwrite stage")?;
        let stage_meta = access_metadata(&stage, &self.staged)?;
        if !same_identity(pinned, metadata_stamp(&stage_meta)) { return Err(WorkflowError::SourceChanged(self.staged.clone())); }
        if (stage_meta.uid(), stage_meta.gid()) != (original.uid(), original.gid())
            && unsafe { libc::fchown(stage.as_raw_fd(), original.uid() as libc::uid_t, original.gid() as libc::gid_t) } != 0 {
            return Err(io_error("preserve overwritten output owner and group", &self.staged, io::Error::last_os_error()));
        }
        self.set_stage_mode(original.mode() & 0o7777)?;
        #[cfg(target_os = "macos")]
        {
            let wanted = acl_bytes(source, source_path)?;
            if wanted != acl_bytes(&stage, &self.staged)?
                && unsafe { fcopyfile(source.as_raw_fd(), stage.as_raw_fd(), std::ptr::null_mut(), 1) } != 0 {
                return Err(io_error("preserve overwritten output ACL", &self.staged, io::Error::last_os_error()));
            }
            if wanted != acl_bytes(&stage, &self.staged)? { return Err(invalid(&self.staged, "overwritten output ACL could not be preserved")); }
        }
        #[cfg(target_os = "linux")]
        {
            let name = CString::new("system.posix_acl_access").unwrap();
            let source_has = xattr_names(source, source_path)?.binary_search(&name).is_ok();
            let stage_has = xattr_names(&stage, &self.staged)?.binary_search(&name).is_ok();
            if source_has {
                let size = xattr_size(source, source_path, &name)?;
                let mut value = vec![0u8; size];
                if size != 0 { read_xattr(source, source_path, &name, &mut value, 0)?; }
                set_xattr(&stage, &self.staged, &name, &value, 0)?;
            } else if stage_has { remove_xattr(&stage, &self.staged, &name)?; }
            let actual_has = xattr_names(&stage, &self.staged)?.binary_search(&name).is_ok();
            if source_has != actual_has || (source_has && !xattr_values_equal(source, &stage, source_path, &self.staged, &name)?) {
                return Err(invalid(&self.staged, "overwritten output ACL could not be preserved"));
            }
        }
        let actual = access_metadata(&stage, &self.staged)?;
        let after = access_metadata(source, source_path)?;
        if metadata_stamp(&after) != expected || (after.uid(), after.gid(), after.mode()) != (original.uid(), original.gid(), original.mode()) {
            return Err(WorkflowError::DestinationChanged(source_path.to_owned()));
        }
        if (actual.uid(), actual.gid(), actual.mode() & 0o7777) != (original.uid(), original.gid(), original.mode() & 0o7777) {
            return Err(invalid(&self.staged, "overwritten output owner, group or mode could not be preserved"));
        }
        Ok(())
    }
    fn unlink_owned(&self, path: &Path, expected: (u64, u64)) -> io::Result<()> {
        let (dir, name) = self.argument(path).map_err(io::Error::other)?;
        let mut meta = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatat(dir, name.as_ptr(), meta.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let meta = unsafe { meta.assume_init() };
        if meta.st_mode & libc::S_IFMT != libc::S_IFREG || (meta.st_dev as u64, meta.st_ino as u64) != expected {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "workspace entry was replaced; retained"));
        }
        if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } != 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }
    fn recovery_path(&self, path: &Path) -> PathBuf {
        if path.parent() != Some(self.alias.as_path()) { return path.to_owned(); }
        directory_fd_path(&self.file).unwrap_or_else(|| self.directory.clone()).join(path.file_name().unwrap())
    }
    pub(crate) fn actual_destination(&self, destination: &Path) -> Option<PathBuf> {
        let name = destination.file_name()?;
        Some(directory_fd_path(&self.parent)?.join(name))
    }
    pub(crate) fn validate_parent(&self, destination: &Path) -> Result<(), WorkflowError> {
        let parent_path = self.directory.parent().ok_or_else(|| invalid(&self.directory, "workspace parent has no path"))?;
        let held = self.parent.metadata().map_err(|e| io_error("inspect pinned output parent", parent_path, e))?;
        let matches = fs::symlink_metadata(parent_path).is_ok_and(|named| named.file_type().is_dir()
            && (named.dev(), named.ino()) == (held.dev(), held.ino()));
        if matches { return Ok(()); }
        let actual = self.actual_destination(destination);
        Err(invalid(actual.as_deref().unwrap_or(destination),
            if actual.is_some() { "output parent changed; path shows the pinned parent's current location" }
            else { "output parent changed; current location is unavailable" }))
    }
    pub(crate) fn cleanup(&self, retain_displaced: bool) -> Vec<CleanupFailure> {
        let mut errors = Vec::new();
        let owned = self.stage_file.lock().unwrap_or_else(|e| e.into_inner());
        let cleanup = match owned.as_ref() {
            Some(file) => file.metadata().and_then(|meta| self.unlink_owned(&self.staged, (meta.dev(), meta.ino()))),
            None => match self.open_entry(&self.staged) {
                Err(WorkflowError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(()),
                _ => Err(io::Error::new(io::ErrorKind::PermissionDenied, "unproven private stage retained")),
            },
        };
        if let Err(source) = cleanup {
            if source.kind() != io::ErrorKind::NotFound {
                errors.push(CleanupFailure { operation: "remove private file", path: self.recovery_path(&self.staged), source });
            }
        }
        if !retain_displaced {
            if let Err(source) = remove_pinned_directory(&self.parent, &self.name, self.identity) {
                if source.kind() != io::ErrorKind::NotFound {
                    errors.push(CleanupFailure { operation: "remove private workspace",
                        path: directory_fd_path(&self.file).unwrap_or_else(|| self.directory.clone()), source });
                }
            }
        }
        errors
    }
}

struct Reservation { path: PathBuf, name: CString, quarantine: Workspace }
impl Reservation {
    fn acquire(target: &Path) -> Result<Self, WorkflowError> {
        let parent = target.parent().ok_or_else(|| invalid(target, "target has no parent"))?;
        let filename = target.file_name().ok_or_else(|| invalid(target, "target has no filename"))?;
        // Allocate cleanup quarantine before the reservation exists, so a later
        // workspace-creation failure never requires creating another directory.
        let quarantine = Workspace::create(parent, "reservation")?;
        let result = (|| {
            let mut lock = OsString::from("."); lock.push(filename); lock.push(".pn-reservation");
            let open_lock = |lock: &OsStr| -> io::Result<File> {
                let name = CString::new(lock.as_bytes()).map_err(io::Error::other)?;
                let fd = unsafe { libc::openat(quarantine.parent.as_raw_fd(), name.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | O_NOFOLLOW | libc::O_CLOEXEC, 0o600) };
                if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { File::from_raw_fd(fd) }) }
            };
            let file = match open_lock(&lock) {
                Ok(file) => file,
                Err(e) if e.raw_os_error() == Some(libc::ENAMETOOLONG) => {
                    let mut a = 0xcbf29ce484222325u64;
                    let mut b = 0x84222325cbf29ce4u64;
                    for byte in filename.as_bytes() {
                        let byte = u64::from(byte.to_ascii_lowercase());
                        a = (a ^ byte).wrapping_mul(0x100000001b3);
                        b = (b ^ byte).wrapping_mul(0x100000001b3);
                    }
                    lock = OsString::from(format!(".pn-reservation-{a:016x}{b:016x}"));
                    match open_lock(&lock) {
                        Ok(file) => file,
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(WorkflowError::ReservationExists(parent.join(&lock))),
                        Err(e) => return Err(io_error("reserve output", &parent.join(&lock), e)),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(WorkflowError::ReservationExists(parent.join(&lock))),
                Err(e) => return Err(io_error("reserve output", &parent.join(&lock), e)),
            };
            let path = parent.join(&lock);
            let name = CString::new(lock.as_bytes()).map_err(|_| invalid(&path, "path contains NUL"))?;
            Ok((path, name, file))
        })();
        match result {
            Ok((path, name, file)) => {
                quarantine.set_stage_file(file);
                Ok(Self { path, name, quarantine })
            }
            Err(cause) => {
                let cleanup = quarantine.cleanup(false);
                Err(if cleanup.is_empty() { cause } else { transaction_error(cause, None, None, None, cleanup, false) })
            }
        }
    }
    fn current_path(&self) -> PathBuf {
        directory_fd_path(&self.quarantine.parent).map(|parent| parent.join(OsStr::from_bytes(self.name.as_bytes())))
            .unwrap_or_else(|| self.path.clone())
    }
    fn release(&self) -> Result<(), io::Error> { self.release_impl(#[cfg(test)] None) }
    fn release_impl(&self, #[cfg(test)] before_move: Option<Box<dyn FnOnce()>>) -> Result<(), io::Error> {
        let work = &self.quarantine;
        let source = self.path.clone();
        #[cfg(test)] if let Some(hook) = before_move { hook(); }
        if let Err(error) = move_no_replace(&work, &source, &work.staged) {
            let cleanup = work.cleanup(false);
            return if cleanup.is_empty() { Err(io::Error::other(error)) }
                else { Err(io::Error::other(format!("{error}; reservation quarantine cleanup failed: {cleanup:?}"))) };
        }
        if let Err(error) = work.stage_stamp("verify quarantined reservation") {
            if let Err(rollback) = move_no_replace(&work, &work.staged, &source) {
                return Err(io::Error::other(format!("{error}; unexpected reservation retained at {}: {rollback}",
                    work.recovery_path(&work.staged).display())));
            }
            let cleanup = work.cleanup(false);
            return Err(io::Error::new(io::ErrorKind::PermissionDenied,
                format!("reservation was replaced and returned; cleanup: {cleanup:?}")));
        }
        let cleanup = work.cleanup(false);
        if cleanup.is_empty() { Ok(()) } else { Err(io::Error::other(format!("reservation quarantine cleanup failed: {cleanup:?}"))) }
    }
    fn same_parent(&self, other: &File) -> io::Result<bool> {
        let left = self.quarantine.parent.metadata()?;
        let right = other.metadata()?;
        Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
    }
}
fn release_reservations(reservations: &mut Vec<Reservation>) -> Vec<CleanupFailure> {
    let mut errors = Vec::new();
    for reservation in reservations.drain(..) {
        if let Err(source) = reservation.release() { errors.push(CleanupFailure { operation: "release reservation", path: reservation.current_path(), source }); }
    }
    errors
}
fn transaction_error(cause: WorkflowError, backup: Option<PathBuf>, recovery: Option<PathBuf>, rollback: Option<WorkflowError>, cleanup: Vec<CleanupFailure>, published: bool) -> WorkflowError {
    fn retained_backup(error: &WorkflowError) -> Option<&PathBuf> {
        match error {
            WorkflowError::Transaction { backup: Some(path), .. } => Some(path),
            WorkflowError::Transaction { cause, .. } => retained_backup(cause),
            _ => None,
        }
    }
    let backup = backup.or_else(|| retained_backup(&cause).cloned());
    WorkflowError::Transaction { cause: Box::new(cause), backup, recovery, rollback: rollback.map(Box::new), cleanup, published }
}

fn backup_candidate(source: &Path, directory: &Path, index: u64) -> Result<PathBuf, WorkflowError> {
    let filename = source.file_name().ok_or_else(|| invalid(source, "source has no filename"))?;
    if index == 0 { return Ok(directory.join(filename)); }
    let mut name = source.file_stem().ok_or_else(|| invalid(source, "source has no stem"))?.to_os_string();
    name.push(format!("_{index}"));
    if let Some(ext) = source.extension() { name.push("."); name.push(ext); }
    Ok(directory.join(name))
}
fn create_backup(source: &Path, expected: &Stamp, directory: &Path,
    #[cfg(test)] after_publish: Option<Box<dyn FnOnce(&Workspace)>>) -> Result<PathBuf, WorkflowError> {
    let work = Workspace::create(directory, "backup")?;
    let result = (|| {
        copy_new_verified(source, &work.staged, *expected, Some(&work))?;
        let mut index = 0u64;
        loop {
            let candidate = backup_candidate(source, directory, index)?;
            work.validate_parent(&candidate)?;
            let reservation = match Reservation::acquire(&candidate) {
                Ok(reservation) => reservation,
                Err(WorkflowError::ReservationExists(_)) => {
                    index = if index == 0 { 2 } else { index.checked_add(2).ok_or_else(|| invalid(directory, "backup suffix overflow"))? };
                    continue;
                }
                Err(error) => return Err(error),
            };
            match reservation.same_parent(&work.parent) {
                Ok(true) => (),
                result => {
                    let cause = match result {
                        Ok(false) => invalid(&candidate, "reservation and backup workspace have different parents"),
                        Err(error) => io_error("inspect backup reservation parent", &candidate, error),
                        Ok(true) => unreachable!(),
                    };
                    let mut reservations = vec![reservation];
                    return Err(transaction_error(cause, None, None, None, release_reservations(&mut reservations), false));
                }
            }
            let linked = link_at(&work, &work.staged, &candidate);
            let mut reservations = vec![reservation];
            let cleanup = release_reservations(&mut reservations);
            if !cleanup.is_empty() {
                let (cause, backup) = match linked {
                    Ok(()) => (invalid(&candidate, "backup published but reservation release failed"), Some(candidate)),
                    Err(e) => (io_error("publish backup without overwrite", &candidate, e), None),
                };
                return Err(transaction_error(cause, backup, None, None, cleanup, false));
            }
            match linked {
                Ok(()) => {
                    #[cfg(test)] if let Some(hook) = after_publish { hook(&work); }
                    if let Err(error) = work.validate_parent(&candidate) {
                        let actual = work.actual_destination(&candidate);
                        return Err(transaction_error(error, actual, None, None, Vec::new(), false));
                    }
                    return Ok(candidate);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    index = if index == 0 { 2 } else { index.checked_add(2).ok_or_else(|| invalid(directory, "backup suffix overflow"))? };
                }
                Err(e) => return Err(io_error("publish backup without overwrite", &candidate, e)),
            }
        }
    })();
    let cleanup = work.cleanup(false);
    match (result, cleanup.is_empty()) {
        (Ok(path), true) => Ok(path),
        (Ok(path), false) => Err(transaction_error(invalid(&path, "backup published but private cleanup failed"), Some(path), None, None, cleanup, false)),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(transaction_error(error, None, None, None, cleanup, false)),
    }
}
fn sync_backup_directory(backup: &Path) -> Result<(), WorkflowError> {
    let parent = backup.parent().ok_or_else(|| invalid(backup, "backup has no parent"))?;
    let directory = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent).map_err(|e| io_error("open backup directory for sync", parent, e))?;
    directory.sync_all().map_err(|e| io_error("sync verified backup directory", parent, e))
}

fn move_no_replace(workspace: &Workspace, from: &Path, to: &Path) -> Result<(), WorkflowError> {
    let (from_fd, from_c) = workspace.argument(from)?;
    let (to_fd, to_c) = workspace.argument(to)?;
    // SAFETY: descriptors and path strings remain live throughout the call.
    #[cfg(target_os = "macos")]
    let status = unsafe { libc::renameatx_np(from_fd, from_c.as_ptr(), to_fd, to_c.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let status = unsafe { libc::renameat2(from_fd, from_c.as_ptr(), to_fd, to_c.as_ptr(), libc::RENAME_NOREPLACE) };
    if status == 0 { return Ok(()); }
    let source = io::Error::last_os_error();
    #[cfg(target_os = "linux")]
    if matches!(source.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)) {
        return Err(WorkflowError::AtomicRenameUnsupported { operation: "rename without replacement", path: to.to_owned(), source });
    }
    Err(WorkflowError::Move { from: from.to_owned(), to: to.to_owned(), source })
}
fn swap_paths(workspace: &Workspace, first: &Path, second: &Path) -> Result<(), WorkflowError> {
    let (first_fd, first_c) = workspace.argument(first)?;
    let (second_fd, second_c) = workspace.argument(second)?;
    // Exchange remains atomic and preserves the displaced entry.
    #[cfg(target_os = "macos")]
    let status = unsafe { libc::renameatx_np(first_fd, first_c.as_ptr(), second_fd, second_c.as_ptr(), libc::RENAME_SWAP) };
    #[cfg(target_os = "linux")]
    let status = unsafe { libc::renameat2(first_fd, first_c.as_ptr(), second_fd, second_c.as_ptr(), libc::RENAME_EXCHANGE) };
    if status == 0 { return Ok(()); }
    let source = io::Error::last_os_error();
    #[cfg(target_os = "linux")]
    if matches!(source.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)) {
        return Err(WorkflowError::AtomicRenameUnsupported { operation: "atomic exchange", path: second.to_owned(), source });
    }
    Err(io_error("atomically swap output", second, source))
}
fn link_at(workspace: &Workspace, source: &Path, destination: &Path) -> io::Result<()> {
    let (source_fd, source_c) = workspace.argument(source).map_err(io::Error::other)?;
    let (dest_fd, dest_c) = workspace.argument(destination).map_err(io::Error::other)?;
    if source == workspace.staged {
        let owned = workspace.stage_file.lock().unwrap_or_else(|e| e.into_inner());
        let identity = owned.as_ref().map(|file| file.metadata()).transpose()?
            .map(|meta| (meta.dev(), meta.ino()));
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatat(source_fd, source_c.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG || identity != Some((stat.st_dev as u64, stat.st_ino as u64)) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "stage identity changed before publication"));
        }
    }
    // Flags 0 never dereferences a symlink in the final source component.
    if unsafe { libc::linkat(source_fd, source_c.as_ptr(), dest_fd, dest_c.as_ptr(), 0) } == 0 { Ok(()) }
    else { Err(io::Error::last_os_error()) }
}
fn link_no_replace(workspace: &Workspace, source: &Path, destination: &Path) -> Result<(), WorkflowError> {
    match link_at(workspace, source, destination) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(WorkflowError::DestinationExists(destination.to_owned())),
        Err(e) => Err(io_error("publish without overwrite", destination, e)),
    }
}

struct CommitFailure { cause: WorkflowError, rollback: Option<WorkflowError>, recovery: Option<PathBuf>, published: bool }
impl From<WorkflowError> for CommitFailure {
    fn from(cause: WorkflowError) -> Self { Self { cause, rollback: None, recovery: None, published: false } }
}
fn rollback_move(workspace: &Workspace, cause: WorkflowError, displaced: &Path, source: &Path) -> CommitFailure {
    match move_no_replace(workspace, displaced, source) {
        Ok(()) => CommitFailure { cause, rollback: None, recovery: None, published: false },
        Err(rollback) => CommitFailure { cause, rollback: Some(rollback), recovery: Some(displaced.to_owned()), published: false },
    }
}

fn commit_overwriting(workspace: &Workspace, destination: &Path, staged: &Path, expected: Stamp, staged_expected: Stamp,
    #[cfg(test)] after_check: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)] after_swap: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)] before_rollback: Option<Box<dyn FnOnce() + Send>>) -> Result<(), CommitFailure> {
    let pinned = open_read(destination).map_err(CommitFailure::from)?;
    let opened = pinned.metadata().map_err(|e| CommitFailure::from(io_error("inspect open overwrite destination", destination, e)))?;
    if metadata_stamp(&opened) != expected { return Err(WorkflowError::DestinationChanged(destination.to_owned()).into()); }
    let metadata = overwrite_metadata(&pinned, destination).map_err(CommitFailure::from)?;
    let digest = content_digest(&pinned, destination).map_err(CommitFailure::from)?;
    let opened = pinned.metadata().map_err(|e| CommitFailure::from(io_error("reinspect open overwrite destination", destination, e)))?;
    if metadata_stamp(&opened) != expected { return Err(WorkflowError::DestinationChanged(destination.to_owned()).into()); }
    let pinned_stage = workspace.open_entry(staged).map_err(CommitFailure::from)?;
    let stage_opened = pinned_stage.metadata().map_err(|e| CommitFailure::from(io_error("inspect open staging file", staged, e)))?;
    if metadata_stamp(&stage_opened) != staged_expected { return Err(WorkflowError::SourceChanged(staged.to_owned()).into()); }
    let staged_metadata = overwrite_metadata(&pinned_stage, staged).map_err(CommitFailure::from)?;
    let staged_digest = content_digest(&pinned_stage, staged).map_err(CommitFailure::from)?;
    let stage_opened = pinned_stage.metadata().map_err(|e| CommitFailure::from(io_error("reinspect open staging file", staged, e)))?;
    if metadata_stamp(&stage_opened) != staged_expected { return Err(WorkflowError::SourceChanged(staged.to_owned()).into()); }
    #[cfg(test)] if let Some(hook) = after_check { hook(); }
    swap_paths(workspace, staged, destination).map_err(CommitFailure::from)?;
    #[cfg(test)] if let Some(hook) = after_swap { hook(); }
    if let Err(cause) = workspace.validate_parent(destination) {
        return Err(CommitFailure { cause, rollback: None, recovery: Some(staged.to_owned()), published: true });
    }
    // Darwin updates ctime during rename. Compare the complete original stamp
    // before the swap, then bind the moved entry to that open inode and require
    // its post-swap path and descriptor stamps to agree exactly.
    let old_matches = match (stamp(staged, "inspect swapped-out output"), pinned.metadata()) {
        (Ok(old), Ok(opened)) => same_moved_file(old, expected) && old == metadata_stamp(&opened)
            && overwrite_metadata(&pinned, staged).is_ok_and(|current| current == metadata)
            && content_digest(&pinned, staged).is_ok_and(|current| current == digest)
            && stamp(staged, "reinspect swapped-out output").is_ok_and(|after| after == old),
        _ => false,
    };
    let published_matches = stamp(destination, "inspect swapped-in output")
        .is_ok_and(|new| same_moved_file(new, staged_expected));
    if old_matches && published_matches {
        if let Err(e) = workspace.sync_parent() {
            return Err(CommitFailure { cause: io_error("sync published output directory", destination, e),
                rollback: None, recovery: Some(staged.to_owned()), published: true });
        }
        workspace.set_stage_file(pinned);
        return Ok(());
    }
    let cause = WorkflowError::DestinationChanged(destination.to_owned());
    let rollback = if published_matches {
        #[cfg(test)] if let Some(hook) = before_rollback { hook(); }
        swap_paths(workspace, staged, destination)
    } else {
        Err(WorkflowError::DestinationChanged(destination.to_owned()))
    };
    match rollback {
        Ok(()) => {
            let staged_matches = match (stamp(staged, "inspect rollback staging entry"), pinned_stage.metadata()) {
                (Ok(current), Ok(opened)) => same_moved_file(current, staged_expected)
                    && current == metadata_stamp(&opened)
                    && overwrite_metadata(&pinned_stage, staged).is_ok_and(|actual| actual == staged_metadata)
                    && content_digest(&pinned_stage, staged).is_ok_and(|actual| actual == staged_digest)
                    && stamp(staged, "reinspect rollback staging entry").is_ok_and(|after| after == current),
                _ => false,
            };
            if staged_matches { Err(CommitFailure { cause, rollback: None, recovery: Some(staged.to_owned()), published: false }) }
            else {
                let recovery = fs::symlink_metadata(staged).ok().map(|_| staged.to_owned());
                Err(CommitFailure { cause, rollback: Some(invalid(staged, "rollback displaced an unexpected entry")),
                    recovery, published: true })
            }
        }
        Err(rollback) => Err(CommitFailure { cause, rollback: Some(rollback), recovery: Some(staged.to_owned()), published: true }),
    }
}

/// Commit a previously verified copy using only bounded identity checks.
/// Any failure before publication rolls the displaced file back.
fn commit_replacing(
    workspace: &Workspace, source: &Path, destination: &Path, staged: &Path, displaced: &Path, expected: Stamp, staged_expected: Stamp,
    original_backup: Option<(&Path, Stamp)>, staged_backup: Option<(&Path, Stamp)>, retain_displaced: bool,
    #[cfg(test)] after_move: Option<Box<dyn FnOnce(&Path) + Send>>,
) -> Result<(), CommitFailure> {
    if let Err(cause) = move_no_replace(workspace, source, displaced) { return Err(CommitFailure { cause, rollback: None, recovery: None, published: false }); }
    let moved_on_arrival = match stamp(displaced, "inspect newly displaced original") {
        Ok(moved) if same_moved_file(moved, expected) => moved,
        Ok(_) => return Err(rollback_move(workspace, WorkflowError::SourceChanged(source.to_owned()), displaced, source)),
        Err(cause) => return Err(rollback_move(workspace, cause, displaced, source)),
    };
    #[cfg(test)] if let Some(hook) = after_move { hook(source); }
    let verification = (|| {
        let moved = stamp(displaced, "inspect displaced original")?;
        if moved != moved_on_arrival { return Err(WorkflowError::SourceChanged(source.to_owned())); }
        if workspace.stage_stamp("inspect verified staging file")? != staged_expected {
            return Err(WorkflowError::SourceChanged(staged.to_owned()));
        }
        for (backup, expected) in original_backup.into_iter().chain(staged_backup) {
            if stamp(backup, "inspect verified backup")? != expected { return Err(WorkflowError::BackupChanged(backup.to_owned())); }
        }
        Ok(moved)
    })();
    let moved_before_link = match verification {
        Ok(moved) => moved,
        Err(cause) => return Err(rollback_move(workspace, cause, displaced, source)),
    };
    if let Err(cause) = link_no_replace(workspace, staged, destination) { return Err(rollback_move(workspace, cause, displaced, source)); }
    if let Err(cause) = workspace.validate_parent(destination) {
        return Err(CommitFailure { cause, rollback: None, recovery: Some(displaced.to_owned()), published: true });
    }
    let verification = (|| {
        for (backup, expected) in original_backup.into_iter().chain(staged_backup) {
            if stamp(backup, "reinspect verified backup")? != expected { return Err(WorkflowError::BackupChanged(backup.to_owned())); }
        }
        let moved = stamp(displaced, "reinspect displaced original")?;
        if moved != moved_before_link { return Err(WorkflowError::SourceChanged(source.to_owned())); }
        let stage = workspace.stage_stamp("reinspect published staging file")?;
        if !same_moved_file(stage, staged_expected) { return Err(WorkflowError::SourceChanged(staged.to_owned())); }
        Ok(())
    })();
    if let Err(cause) = verification { return Err(CommitFailure { cause, rollback: None, recovery: Some(displaced.to_owned()), published: true }); }
    if let Err(e) = workspace.sync_parent() {
        return Err(CommitFailure { cause: io_error("sync published output directory", destination, e),
            rollback: None, recovery: Some(displaced.to_owned()), published: true });
    }
    if !retain_displaced {
        if let Err(e) = workspace.unlink_owned(displaced, (expected.dev, expected.ino)) {
            return Err(CommitFailure { cause: io_error("remove displaced original after publication", displaced, e), rollback: None, recovery: Some(displaced.to_owned()), published: true });
        }
    }
    Ok(())
}

/// A private stage is not published merely because an encoder returned. The
/// caller must finish the encoder and all tag/metadata work before `finish`.
/// Drop deliberately does not delete files or conceal cleanup errors: call
/// `abort` when abandoning a transaction. Only files the caller created at the
/// staging path may be placed in the private workspace.
#[must_use]
pub struct StagedOutput {
    plan: OutputPlan, workspace: Workspace, reservations: Vec<Reservation>, source_stamp: Stamp,
    destination_stamp: Option<Stamp>, backup: Option<PathBuf>, backup_stamp: Option<Stamp>,
    new_mp3_mode: Option<u32>,
    #[cfg(test)] after_move: Option<Box<dyn FnOnce(&Path) + Send>>,
    #[cfg(test)] before_commit: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)] after_publish: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)] after_overwrite_check: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)] after_overwrite_swap: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)] before_overwrite_rollback: Option<Box<dyn FnOnce() + Send>>,
}
impl StagedOutput {
    pub fn staging_path(&self) -> &Path { &self.workspace.staged }
    pub(crate) fn create_stage(&self) -> Result<File, WorkflowError> { self.workspace.create_stage() }
    #[cfg(test)]
    pub(crate) fn workspace_directory(&self) -> &Path { &self.workspace.directory }
    /// Register the codec-owned inode before it is linked at the stage name.
    pub(crate) fn register_codec_stage(&self, file: &File) -> io::Result<()> {
        let mut owned = self.workspace.stage_file.lock().unwrap_or_else(|e| e.into_inner());
        if owned.is_some() { return Err(io::Error::new(io::ErrorKind::AlreadyExists, "stage already recorded")); }
        *owned = Some(file.try_clone()?);
        Ok(())
    }
    /// Record a successfully created stage before other fallible work. An
    /// unrecorded partial file is retained rather than unlinked on abort.
    pub fn remember_staging_file(&self) -> Result<(), WorkflowError> { self.workspace.remember_stage().map(|_| ()) }
    pub(crate) fn accept_tag_rewrite(&self) -> Result<(), WorkflowError> { self.workspace.accept_rewritten_stage() }
    pub fn backup_path(&self) -> Option<&Path> { self.backup.as_deref() }
    fn cleanup(&mut self, retain_displaced: bool) -> Vec<CleanupFailure> {
        let mut errors = self.workspace.cleanup(retain_displaced);
        errors.extend(release_reservations(&mut self.reservations));
        errors
    }
    fn fail(mut self, cause: WorkflowError, rollback: Option<WorkflowError>, recovery: Option<PathBuf>, published: bool) -> WorkflowError {
        let cleanup = if recovery.as_deref() == Some(self.workspace.staged.as_path()) {
            release_reservations(&mut self.reservations)
        } else { self.cleanup(recovery.is_some()) };
        let recovery = recovery.map(|path| self.workspace.recovery_path(&path));
        transaction_error(cause, self.backup.clone(), recovery, rollback, cleanup, published)
    }
    pub fn abort(mut self) -> Result<AbortReport, WorkflowError> {
        let cleanup = self.cleanup(false);
        if cleanup.is_empty() { Ok(AbortReport { retained_backup: self.backup }) }
        else {
            let cause = invalid(&self.workspace.directory, "transaction aborted but cleanup failed");
            Err(transaction_error(cause, self.backup, None, None, cleanup, false))
        }
    }
    pub fn finish(mut self, cancel: &Cancellation) -> Result<Publication, WorkflowError> {
        if let Err(e) = self.workspace.remember_stage() { return Err(self.fail(e, None, None, false)); }
        let _overwrite_pin = if let Some(expected) = self.destination_stamp {
            let file = match open_read(&self.plan.destination) {
                Ok(file) => file, Err(e) => return Err(self.fail(e, None, None, false)),
            };
            if let Err(e) = self.workspace.set_overwrite_access(&file, &self.plan.destination, expected) {
                return Err(self.fail(e, None, None, false));
            }
            Some(file)
        } else {
            if let Some(mode) = self.new_mp3_mode {
                if let Err(e) = self.workspace.set_stage_mode(mode) { return Err(self.fail(e, None, None, false)); }
            }
            None
        };
        let staged = match self.workspace.stage_stamp("inspect completed private stage") {
            Ok(s) => s, Err(e) => return Err(self.fail(e, None, None, false)),
        };
        let sync = self.workspace.open_entry(&self.workspace.staged).and_then(|file| file.sync_all()
            .map_err(|e| io_error("sync completed staging file", &self.workspace.staged, e)));
        if let Err(cause) = sync {
            return Err(self.fail(cause, None, None, false));
        }
        if let Err(e) = check_stamp(&self.plan.source, self.source_stamp) { return Err(self.fail(e, None, None, false)); }
        if let (Some(backup), Some(expected)) = (&self.backup, self.backup_stamp) {
            if let Err(e) = verify_backup(&self.plan.source, backup, expected) { return Err(self.fail(e, None, None, false)); }
        }
        match self.workspace.stage_stamp("reinspect completed staging file") {
            Ok(now) if now == staged => (),
            Ok(_) => {
                let cause = WorkflowError::SourceChanged(self.workspace.staged.clone());
                return Err(self.fail(cause, None, None, false));
            }
            Err(e) => return Err(self.fail(e, None, None, false)),
        }
        #[cfg(test)] if let Some(hook) = self.before_commit.take() { hook(); }
        let commit = {
            let _guard = cancel.publication.lock().unwrap_or_else(|e| e.into_inner());
            if cancel.is_cancelled() {
                Err(WorkflowError::Cancelled.into())
            } else if let Err(error) = self.workspace.validate_parent(&self.plan.destination) {
                Err(error.into())
            } else if let Err(error) = check_stamp(&self.plan.source, self.source_stamp) {
                Err(error.into())
            } else if let Err(error) = self.workspace.check_stage_stamp(staged) {
                Err(error.into())
            } else if self.plan.is_replacement() {
                commit_replacing(&self.workspace, &self.plan.source, &self.plan.destination, &self.workspace.staged,
                    &self.workspace.displaced(), self.source_stamp, staged, self.backup.as_deref().zip(self.backup_stamp), None, false,
                    #[cfg(test)] self.after_move.take())
            } else if let Some(guard) = &self.plan.overwrite {
                match check_overwrite_destination(&self.plan.destination, guard) {
                    Ok(current) if current == self.destination_stamp => {
                        if let Some(expected) = current {
                            commit_overwriting(&self.workspace, &self.plan.destination, &self.workspace.staged, expected, staged,
                                #[cfg(test)] self.after_overwrite_check.take(), #[cfg(test)] self.after_overwrite_swap.take(),
                                #[cfg(test)] self.before_overwrite_rollback.take())
                        } else {
                            link_no_replace(&self.workspace, &self.workspace.staged, &self.plan.destination).map_err(CommitFailure::from)
                        }
                    }
                    Ok(_) => Err(WorkflowError::DestinationChanged(self.plan.destination.clone()).into()),
                    Err(error) => Err(error.into()),
                }
            } else {
                link_no_replace(&self.workspace, &self.workspace.staged, &self.plan.destination).map_err(CommitFailure::from)
            }
        };
        if let Err(error) = commit { return Err(self.fail(error.cause, error.rollback, error.recovery, error.published)); }
        #[cfg(test)] if let Some(hook) = self.after_publish.take() { hook(); }
        if let Err(cause) = self.workspace.validate_parent(&self.plan.destination) {
            let recovery = self.workspace.staged.clone();
            return Err(self.fail(cause, None, Some(recovery), true));
        }
        if !self.plan.is_replacement() && self.destination_stamp.is_none() {
            if let Err(error) = self.workspace.sync_parent() {
                let cause = io_error("sync published output directory", &self.plan.destination, error);
                let recovery = self.workspace.staged.clone();
                return Err(self.fail(cause, None, Some(recovery), true));
            }
        }
        let publication = Publication { destination: self.plan.destination.clone(), backup: self.backup.clone(),
            replaced_existing_output: self.plan.overwrite.is_some() && self.destination_stamp.is_some() };
        let cleanup = self.cleanup(false);
        if cleanup.is_empty() { Ok(publication) }
        else {
            let cause = invalid(&publication.destination, "published but cleanup failed");
            Err(transaction_error(cause, publication.backup, None, None, cleanup, true))
        }
    }
}

fn backup_removal_error(operation: &'static str, error: impl fmt::Display, retained_path: Option<PathBuf>) -> BackupRemovalError {
    BackupRemovalError { operation, message: error.to_string(), retained_path, unexpected_path: None }
}

fn backup_path_error(operation: &'static str, error: impl fmt::Display, backup: &Path, expected: Stamp) -> BackupRemovalError {
    let verified = stamp(backup, "inspect retained backup").is_ok_and(|actual| actual == expected);
    let mut result = backup_removal_error(operation, error, verified.then(|| backup.to_owned()));
    if !verified && fs::symlink_metadata(backup).is_ok() { result.unexpected_path = Some(backup.to_owned()); }
    result
}

fn sync_restored(current: &Path, expected: Stamp) -> Result<(), BackupRemovalError> {
    let file = open_read(current).map_err(|e| backup_removal_error("sync restored file", e, None))?;
    let opened = file.metadata().map_err(|e| backup_removal_error("sync restored file", e, None))?;
    if !same_moved_file(metadata_stamp(&opened), expected) {
        return Err(backup_removal_error("sync restored file", WorkflowError::SourceChanged(current.to_owned()), None));
    }
    #[cfg(target_os = "macos")]
    // F_FULLFSYNC (51) asks the device to flush data, not merely the OS cache.
    if unsafe { fcntl(file.as_raw_fd(), 51) } != 0 {
        return Err(backup_removal_error("sync restored file", io::Error::last_os_error(), None));
    }
    #[cfg(target_os = "linux")]
    file.sync_all().map_err(|e| backup_removal_error("sync restored file", e, None))?;
    let current_stamp = stamp(current, "reinspect restored file before backup removal")
        .map_err(|e| backup_removal_error("sync restored file", e, None))?;
    if !same_moved_file(current_stamp, expected) {
        return Err(backup_removal_error("sync restored file", WorkflowError::SourceChanged(current.to_owned()), None));
    }
    let parent = current.parent().ok_or_else(|| backup_removal_error("sync restored directory", "restored file has no parent", None))?;
    let directory = File::open(parent).map_err(|e| backup_removal_error("sync restored directory", e, None))?;
    directory.sync_all().map_err(|e| backup_removal_error("sync restored directory", e, None))?;
    #[cfg(target_os = "macos")]
    if unsafe { fcntl(directory.as_raw_fd(), 51) } != 0 {
        let error = io::Error::last_os_error();
        // A directory may not support F_FULLFSYNC; the required fsync succeeded.
        if !matches!(error.raw_os_error(), Some(22 | 25 | 45)) {
            return Err(backup_removal_error("sync restored directory", error, None));
        }
    }
    Ok(())
}

fn quarantine_cleanup_error(workspace: &Workspace, mut error: BackupRemovalError) -> BackupRemovalError {
    for failure in workspace.cleanup(false) {
        error.message.push_str(&format!("; {} {}: {}", failure.operation, failure.path.display(), failure.source));
    }
    error
}

fn undo_quarantined_backup(workspace: &Workspace, backup: &Path, mut error: BackupRemovalError, entry_is_backup: bool) -> BackupRemovalError {
    match move_no_replace(workspace, &workspace.staged, backup) {
        Ok(()) => {
            if entry_is_backup { error.retained_path = Some(backup.to_owned()); }
            else { error.retained_path = None; error.unexpected_path = Some(backup.to_owned()); }
            quarantine_cleanup_error(workspace, error)
        }
        Err(rollback) => {
            error.message.push_str(&format!("; unable to return quarantined entry: {rollback}"));
            if entry_is_backup { error.retained_path = Some(workspace.recovery_path(&workspace.staged)); }
            else { error.retained_path = None; error.unexpected_path = Some(workspace.recovery_path(&workspace.staged)); }
            error
        }
    }
}

fn remove_verified_backup(restored: &Path, backup: &Path, expected: Stamp,
    #[cfg(test)] before_move: Option<Box<dyn FnOnce() + Send>>) -> (bool, Option<BackupRemovalError>) {
    let reservation = match Reservation::acquire(backup) {
        Ok(reservation) => reservation,
        Err(error) => return (false, Some(backup_path_error("reserve verified backup", error, backup, expected))),
    };
    let (removed, mut error) = remove_verified_backup_reserved(restored, backup, expected, #[cfg(test)] before_move);
    if let Err(failure) = reservation.release() {
        let message = format!("release backup reservation {}: {failure}", reservation.path.display());
        match &mut error {
            Some(existing) => existing.message.push_str(&format!("; {message}")),
            None => error = Some(backup_removal_error("release backup reservation", message, None)),
        }
    }
    (removed, error)
}

fn remove_verified_backup_reserved(restored: &Path, backup: &Path, expected: Stamp,
    #[cfg(test)] before_move: Option<Box<dyn FnOnce() + Send>>) -> (bool, Option<BackupRemovalError>) {
    let retained = || Some(backup.to_owned());
    let Some(parent) = backup.parent() else {
        return (false, Some(backup_path_error("remove verified backup", "backup has no parent", backup, expected)));
    };
    let workspace = match Workspace::create(parent, "backup-remove") {
        Ok(workspace) => workspace,
        Err(error) => return (false, Some(backup_path_error("remove verified backup", error, backup, expected))),
    };
    let unchanged = match stamp(backup, "reinspect verified backup before quarantine") {
        Ok(actual) if actual == expected => Ok(()),
        Ok(_) => Err(WorkflowError::BackupChanged(backup.to_owned())),
        Err(error) => Err(error),
    };
    if let Err(error) = unchanged {
        return (false, Some(quarantine_cleanup_error(&workspace,
            backup_path_error("reinspect verified backup", error, backup, expected))));
    }
    let pinned = match open_read(backup) {
        Ok(file) => file,
        Err(error) => return (false, Some(quarantine_cleanup_error(&workspace,
            backup_path_error("reinspect verified backup", error, backup, expected)))),
    };
    if !pinned.metadata().is_ok_and(|meta| metadata_stamp(&meta) == expected) {
        return (false, Some(quarantine_cleanup_error(&workspace,
            backup_path_error("reinspect verified backup", WorkflowError::BackupChanged(backup.to_owned()), backup, expected))));
    }
    #[cfg(test)] if let Some(hook) = before_move { hook(); }
    if let Err(error) = move_no_replace(&workspace, backup, &workspace.staged) {
        return (false, Some(quarantine_cleanup_error(&workspace,
            backup_path_error("quarantine verified backup", error, backup, expected))));
    }
    let moved = match (stamp(&workspace.staged, "inspect quarantined backup"), pinned.metadata()) {
        (Ok(actual), Ok(opened)) if same_moved_file(actual, expected) && actual == metadata_stamp(&opened) => actual,
        _ => {
            let error = backup_removal_error("verify quarantined backup", WorkflowError::BackupChanged(backup.to_owned()), None);
            return (false, Some(undo_quarantined_backup(&workspace, backup, error, false)));
        }
    };
    // Rename changes ctime; recheck bytes and preservable metadata against the
    // restored file before unlinking the pinned, quarantined inode.
    if let Err(cause) = verify_backup(restored, &workspace.staged, moved) {
        let error = backup_removal_error("verify quarantined backup", cause, None);
        return (false, Some(undo_quarantined_backup(&workspace, backup, error, false)));
    }
    if let Err(error) = workspace.unlink_owned(&workspace.staged, (moved.dev, moved.ino)) {
        let error = backup_removal_error("remove verified backup", error, retained());
        return (false, Some(undo_quarantined_backup(&workspace, backup, error, true)));
    }
    let cleanup = workspace.cleanup(false);
    if cleanup.is_empty() { (true, None) }
    else {
        let message = cleanup.iter().map(|e| format!("{} {}: {}", e.operation, e.path.display(), e.source)).collect::<Vec<_>>().join("; ");
        (true, Some(backup_removal_error("remove backup quarantine", message, None)))
    }
}

struct PinnedRestoreFile { file: File, stamp: Stamp, metadata: OverwriteMetadata, digest: [u8; 32] }
impl PinnedRestoreFile {
    fn capture(path: &Path, expected: Stamp) -> Result<Self, WorkflowError> {
        let file = open_read(path)?;
        let before = file.metadata().map_err(|e| io_error("inspect restore rollback file", path, e))?;
        if metadata_stamp(&before) != expected { return Err(WorkflowError::SourceChanged(path.to_owned())); }
        let metadata = overwrite_metadata(&file, path)?;
        let digest = content_digest(&file, path)?;
        let after = file.metadata().map_err(|e| io_error("reinspect restore rollback file", path, e))?;
        if metadata_stamp(&after) != expected { return Err(WorkflowError::SourceChanged(path.to_owned())); }
        Ok(Self { file, stamp: expected, metadata, digest })
    }

    fn matches(&self, path: &Path) -> bool {
        match (stamp(path, "inspect pinned restore file"), self.file.metadata()) {
            (Ok(current), Ok(opened)) => same_moved_file(current, self.stamp) && current == metadata_stamp(&opened)
                && overwrite_metadata(&self.file, path).is_ok_and(|metadata| metadata == self.metadata)
                && content_digest(&self.file, path).is_ok_and(|digest| digest == self.digest)
                && stamp(path, "reinspect pinned restore file").is_ok_and(|after| after == current),
            _ => false,
        }
    }
}

struct HeldRestore {
    current: PathBuf, workspace: Workspace, reservations: Vec<Reservation>,
    original: PinnedRestoreFile, restored: PinnedRestoreFile,
}
impl HeldRestore {
    fn retain_failure(mut self, cause: WorkflowError, rollback: Option<WorkflowError>) -> WorkflowError {
        let recovery = self.workspace.recovery_path(&self.workspace.displaced());
        let cleanup = release_reservations(&mut self.reservations);
        transaction_error(cause, None, Some(recovery), rollback, cleanup, true)
    }

    fn finish(mut self) -> Result<(), WorkflowError> {
        let displaced = self.workspace.displaced();
        if !self.original.matches(&displaced) {
            let cause = WorkflowError::SourceChanged(self.current.clone());
            return Err(self.retain_failure(cause, None));
        }
        if let Err(error) = self.workspace.unlink_owned(&displaced, (self.original.stamp.dev, self.original.stamp.ino)) {
            return Err(self.retain_failure(io_error("remove retained processed original", &displaced, error), None));
        }
        let mut cleanup = self.workspace.cleanup(false);
        cleanup.extend(release_reservations(&mut self.reservations));
        if cleanup.is_empty() { Ok(()) }
        else { Err(transaction_error(invalid(&self.current, "restored but cleanup failed"), None, None, None, cleanup, true)) }
    }

    fn rollback(mut self) -> Result<(), WorkflowError> {
        let displaced = self.workspace.displaced();
        if !self.original.matches(&displaced) || !self.restored.matches(&self.current) {
            let cause = invalid(&self.current, "restore rollback identity or contents changed");
            return Err(self.retain_failure(cause, None));
        }
        if let Err(error) = swap_paths(&self.workspace, &displaced, &self.current) {
            let cause = invalid(&self.current, "restore rollback swap failed");
            return Err(self.retain_failure(cause, Some(error)));
        }
        if !self.original.matches(&self.current) || !self.restored.matches(&displaced) {
            let cause = invalid(&self.current, "restore rollback displaced an unexpected entry");
            return Err(self.retain_failure(cause, None));
        }
        if let Err(error) = self.workspace.unlink_owned(&displaced, (self.restored.stamp.dev, self.restored.stamp.ino)) {
            return Err(self.retain_failure(io_error("remove rolled-back restore copy", &displaced, error), None));
        }
        let mut cleanup = self.workspace.cleanup(false);
        cleanup.extend(release_reservations(&mut self.reservations));
        if cleanup.is_empty() { Ok(()) }
        else { Err(transaction_error(invalid(&self.current, "restore rolled back but cleanup failed"), None, None, None, cleanup, false)) }
    }
}

/// Restore only the explicit backup named by a successfully inspected marker.
/// The current processed file remains recoverable until the verified backup
/// copy has been published. The backup is consumed only after the published
/// copy is verified; a removal failure is reported without undoing restoration.
/// The caller supplies the current scheduler/protection state and must hold that
/// state stable. `identity` must have been captured before reading `marker`.
pub fn restore(processed: &Path, backup: &Path, marker: Option<&Marker>, status: RestoreStatus,
               identity: &FileIdentity, cancel: &Cancellation) -> Result<RestoreReport, WorkflowError> {
    restore_impl(processed, backup, marker, status, identity, cancel, true, None, false, #[cfg(test)] None)
        .map(|(report, _)| report)
}

fn restore_impl(processed: &Path, backup: &Path, marker: Option<&Marker>, status: RestoreStatus,
                identity: &FileIdentity, cancel: &Cancellation, consume_backup: bool, frozen_backup: Option<&FrozenBackup>,
                preserve_original: bool,
                #[cfg(test)] sync_override: Option<Box<dyn FnOnce() -> Result<(), BackupRemovalError> + Send>>) -> Result<(RestoreReport, Option<HeldRestore>), WorkflowError> {
    match status {
        RestoreStatus::Active => return Err(WorkflowError::RestoreBusy(processed.to_owned())),
        RestoreStatus::Protected => return Err(WorkflowError::RestoreProtected(processed.to_owned())),
        RestoreStatus::Inactive => (),
    }
    let recorded = match processed_state(processed, marker)? {
        ProcessingState::Unprocessed => return Err(WorkflowError::NotProcessed(processed.to_owned())),
        ProcessingState::Suffixed => return Err(WorkflowError::RestoreNotNeeded(processed.to_owned())),
        ProcessingState::Marked { backup: None } => return Err(WorkflowError::BackupNotRecorded(processed.to_owned())),
        ProcessingState::Marked { backup: Some(path) } => path,
    };
    if !recorded.is_absolute() || !backup.is_absolute() { return Err(WorkflowError::InvalidMarkerBackup); }
    let recorded_meta = match fs::symlink_metadata(&recorded) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(WorkflowError::BackupNotFound(recorded)),
        Err(e) => return Err(io_error("inspect recorded backup", &recorded, e)),
    };
    if !recorded_meta.file_type().is_file() { return Err(invalid(&recorded, "recorded backup is not a regular non-symlink file")); }
    let backup_meta = fs::symlink_metadata(backup).map_err(|e| io_error("inspect supplied backup", backup, e))?;
    if !backup_meta.file_type().is_file() { return Err(invalid(backup, "supplied backup is not a regular non-symlink file")); }
    let recorded_real = fs::canonicalize(&recorded).map_err(|e| io_error("resolve recorded backup", &recorded, e))?;
    let backup_real = fs::canonicalize(backup).map_err(|e| io_error("resolve supplied backup", backup, e))?;
    if recorded_real != backup_real { return Err(WorkflowError::BackupMismatch { recorded, supplied: backup.to_owned() }); }
    let current_meta = fs::symlink_metadata(processed).map_err(|e| io_error("inspect processed file", processed, e))?;
    if !current_meta.file_type().is_file() { return Err(invalid(processed, "processed file is not a regular non-symlink file")); }
    let current = fs::canonicalize(processed).map_err(|e| io_error("resolve processed file", processed, e))?;
    let current_stamp = stamp(&current, "inspect processed file")?;
    let backup_stamp = stamp(&backup_real, "inspect recorded backup")?;
    if let Some(frozen) = frozen_backup {
        if frozen.identity.path != backup_real || frozen.identity.stamp != backup_stamp {
            return Err(WorkflowError::BackupChanged(backup_real));
        }
        frozen.verify(&backup_real)?;
    }
    if current == backup_real || same_identity(current_stamp, backup_stamp) { return Err(invalid(&backup_real, "backup aliases the processed file")); }
    let mut reservations = vec![Reservation::acquire(&current)?];
    let unchanged = if current != identity.path {
        Err(WorkflowError::SourceChanged(current.clone()))
    } else {
        check_stamp(&current, identity.stamp)
    };
    if let Err(error) = unchanged {
        let cleanup = release_reservations(&mut reservations);
        return if cleanup.is_empty() { Err(error) }
            else { Err(transaction_error(error, Some(backup_real), None, None, cleanup, false)) };
    }
    let parent = current.parent().ok_or_else(|| invalid(&current, "processed file has no parent"))?;
    let workspace = match Workspace::create(parent, "restore") {
        Ok(workspace) => workspace,
        Err(e) => return Err(transaction_error(e, Some(backup_real), None, None, release_reservations(&mut reservations), false)),
    };
    let mut completed_cleanup = None;
    let mut saved_pins = None;
    let attempt = (|| {
        copy_new_verified(&backup_real, &workspace.staged, backup_stamp, Some(&workspace))?;
        verify_backup(&workspace.staged, &backup_real, backup_stamp)?;
        let staged_stamp = workspace.stage_stamp("inspect verified restore copy")?;
        check_stamp(&current, current_stamp)?;
        check_stamp(&backup_real, backup_stamp)?;
        if preserve_original {
            saved_pins = Some((PinnedRestoreFile::capture(&current, current_stamp)?,
                PinnedRestoreFile::capture(&workspace.staged, staged_stamp)?));
        }
        let _guard = cancel.publication.lock().unwrap_or_else(|e| e.into_inner());
        if cancel.is_cancelled() { return Err(WorkflowError::Cancelled.into()); }
        check_stamp(&current, current_stamp)?;
        check_stamp(&backup_real, backup_stamp)?;
        if let Some(frozen) = frozen_backup { frozen.verify(&backup_real)?; }
        workspace.check_stage_stamp(staged_stamp)?;
        commit_replacing(&workspace, &current, &current, &workspace.staged, &workspace.displaced(), current_stamp, staged_stamp,
            None, Some((backup_real.as_path(), backup_stamp)), preserve_original, #[cfg(test)] None)?;
        verify_backup(&current, &backup_real, backup_stamp).map_err(|cause| CommitFailure {
            cause, rollback: None, recovery: preserve_original.then(|| workspace.displaced()), published: true,
        })?;
        if !preserve_original {
            let mut cleanup = workspace.cleanup(false); cleanup.extend(release_reservations(&mut reservations));
            if !cleanup.is_empty() {
                completed_cleanup = Some(cleanup);
                return Err(CommitFailure { cause: invalid(&current, "restored but cleanup failed"),
                    rollback: None, recovery: None, published: true });
            }
        }
        #[cfg(test)]
        let sync = match sync_override { Some(hook) => hook(), None => sync_restored(&current, staged_stamp) };
        #[cfg(not(test))]
        let sync = sync_restored(&current, staged_stamp);
        let result = match sync {
            Ok(()) if consume_backup => remove_verified_backup(&current, &backup_real, backup_stamp, #[cfg(test)] None),
            Ok(()) => (false, None),
            Err(error) => (false, Some(backup_path_error(error.operation, error.message, &backup_real, backup_stamp))),
        };
        Ok(result)
    })();
    match attempt {
        Ok((backup_removed, backup_removal_error)) => {
            let held = if preserve_original {
                let (original, restored) = saved_pins.take().expect("preserved restore has pinned files");
                Some(HeldRestore { current: current.clone(), workspace, reservations, original, restored })
            } else { None };
            Ok((RestoreReport { destination: current, backup: backup_real, backup_removed, backup_removal_error }, held))
        }
        Err(error) => {
            if preserve_original && error.published && saved_pins.is_some() {
                let (original, restored) = saved_pins.take().unwrap();
                let held = HeldRestore { current: current.clone(), workspace, reservations, original, restored };
                return match held.rollback() {
                    Ok(()) => Err(transaction_error(error.cause, Some(backup_real), None, error.rollback, Vec::new(), false)),
                    Err(rollback) => Err(rollback),
                };
            }
            let cleanup = completed_cleanup.unwrap_or_else(|| {
                let mut cleanup = workspace.cleanup(error.recovery.is_some());
                cleanup.extend(release_reservations(&mut reservations)); cleanup
            });
            let recovery = error.recovery.map(|path| workspace.recovery_path(&path));
            Err(transaction_error(error.cause, Some(backup_real), recovery, error.rollback, cleanup, error.published))
        }
    }
}

/// One marker-authorized restore in a group sharing the same backup inode.
pub struct RestoreRequest<'a> {
    pub processed: &'a Path, pub backup: &'a Path, pub marker: Option<&'a Marker>,
    pub status: RestoreStatus, pub identity: &'a FileIdentity, pub frozen_backup: &'a FrozenBackup,
}

fn retain_restore_group(results: &mut [Result<RestoreReport, WorkflowError>], expected: Option<Stamp>, reason: &str) {
    for result in results {
        if let Ok(report) = result {
            if report.backup_removal_error.is_none() {
                report.backup_removal_error = Some(match expected {
                    Some(stamp) => backup_path_error("retain shared backup", reason, &report.backup, stamp),
                    None => backup_removal_error("retain shared backup", reason, None),
                });
            }
        }
    }
}

fn finish_restore_holds(results: &mut [Result<RestoreReport, WorkflowError>], holds: &mut Vec<(usize, HeldRestore)>) {
    for (index, held) in holds.drain(..) {
        if let Err(error) = held.finish() { results[index] = Err(error); }
    }
}

fn rollback_restore_holds(results: &mut [Result<RestoreReport, WorkflowError>], holds: &mut Vec<(usize, HeldRestore)>,
    requests: &[RestoreRequest<'_>]) {
    while let Some((index, held)) = holds.pop() {
        results[index] = Err(match held.rollback() {
            Ok(()) => WorkflowError::BackupChanged(requests[index].backup.to_owned()),
            Err(error) => error,
        });
    }
}

fn merge_restore_cleanup(result: &mut Result<RestoreReport, WorkflowError>, cleanup: Vec<CleanupFailure>) {
    if cleanup.is_empty() { return; }
    if let Err(WorkflowError::Transaction { cleanup: existing, .. }) = result {
        existing.extend(cleanup);
        return;
    }
    let previous = std::mem::replace(result, Err(WorkflowError::Cancelled));
    *result = match previous {
        Err(cause) => Err(transaction_error(cause, None, None, None, cleanup, false)),
        Ok(report) => Err(transaction_error(invalid(&report.destination, "restore cleanup failed"), None, None, None, cleanup, true)),
    };
}

/// Restore every requested copy before consuming any shared backup. The caller
/// groups requests by backup inode and can withhold consumption if another
/// member could not be inspected. Backups are kept when any member fails.
pub fn restore_group(requests: &[RestoreRequest<'_>], allow_consumption: bool, cancel: &Cancellation)
    -> Vec<Result<RestoreReport, WorkflowError>> {
    restore_group_impl(requests, allow_consumption, cancel, #[cfg(test)] None, #[cfg(test)] None,
        #[cfg(test)] None, #[cfg(test)] None)
}

fn restore_group_impl(requests: &[RestoreRequest<'_>], allow_consumption: bool, cancel: &Cancellation,
    #[cfg(test)] mut before_member: Option<Box<dyn FnMut(usize) + Send>>,
    #[cfg(test)] before_consume: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)] after_reservations: Option<Box<dyn FnOnce(&[Reservation]) + Send>>,
    #[cfg(test)] before_removal: Option<Box<dyn FnOnce(&[Reservation]) + Send>>) -> Vec<Result<RestoreReport, WorkflowError>> {
    if requests.is_empty() { return Vec::new(); }
    let expected = requests[0].frozen_backup.identity.stamp;
    let expected_digest = requests[0].frozen_backup.digest;
    if requests.iter().any(|request| {
        !same_identity(request.frozen_backup.identity.stamp, expected)
            || request.frozen_backup.identity.stamp != expected
            || request.frozen_backup.digest != expected_digest
            || request.frozen_backup.verify(request.backup).is_err()
    }) {
        return requests.iter().map(|request| Err(WorkflowError::BackupChanged(request.backup.to_owned()))).collect();
    }
    let mut changed = false;
    let mut results = Vec::with_capacity(requests.len());
    let mut holds = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        #[cfg(test)] if let Some(hook) = before_member.as_mut() { hook(index); }
        let result = if cancel.is_cancelled() {
            Err(WorkflowError::Cancelled)
        } else if changed || request.frozen_backup.verify(request.backup).is_err() {
            changed = true;
            Err(WorkflowError::BackupChanged(request.backup.to_owned()))
        } else {
            match restore_impl(request.processed, request.backup, request.marker, request.status,
                request.identity, cancel, false, Some(request.frozen_backup), true, #[cfg(test)] None) {
                Ok((report, Some(held))) => { holds.push((index, held)); Ok(report) }
                Ok((_, None)) => unreachable!("group restore must retain its original"),
                Err(error) => Err(error),
            }
        };
        if result.is_err() && request.frozen_backup.verify(request.backup).is_err() { changed = true; }
        results.push(result);
    }
    if !changed && requests.iter().any(|request| request.frozen_backup.verify(request.backup).is_err()) {
        changed = true;
    }
    if changed {
        let _guard = cancel.publication.lock().unwrap_or_else(|e| e.into_inner());
        rollback_restore_holds(&mut results, &mut holds, requests);
        return results;
    }
    if !allow_consumption || cancel.is_cancelled()
        || results.iter().any(|result| result.as_ref().map_or(true, |report| report.backup_removal_error.is_some())) {
        finish_restore_holds(&mut results, &mut holds);
        retain_restore_group(&mut results, Some(expected), "another shared-backup restore failed, was cancelled, or could not be inspected");
        return results;
    }
    #[cfg(test)] if let Some(hook) = before_consume { hook(); }
    let _guard = cancel.publication.lock().unwrap_or_else(|e| e.into_inner());
    if cancel.is_cancelled() {
        finish_restore_holds(&mut results, &mut holds);
        retain_restore_group(&mut results, Some(expected), "batch cancelled before backup consumption");
        return results;
    }
    if requests.iter().any(|request| request.frozen_backup.verify(request.backup).is_err()) {
        rollback_restore_holds(&mut results, &mut holds, requests);
        return results;
    }
    let mut backups = BTreeMap::<PathBuf, (PathBuf, &FrozenBackup)>::new();
    for (result, request) in results.iter().zip(requests) {
        let report = result.as_ref().unwrap();
        backups.entry(report.backup.clone()).or_insert_with(|| (report.destination.clone(), request.frozen_backup));
    }
    let mut reservations = Vec::new();
    for backup in backups.keys() {
        match Reservation::acquire(backup) {
            Ok(reservation) => reservations.push(reservation),
            Err(error) => {
                let cleanup = release_reservations(&mut reservations);
                let reason = format!("cannot reserve shared backup: {error}; cleanup: {cleanup:?}");
                finish_restore_holds(&mut results, &mut holds);
                retain_restore_group(&mut results, Some(expected), &reason);
                return results;
            }
        }
    }
    #[cfg(test)] if let Some(hook) = after_reservations { hook(&reservations); }
    let verified = (|| -> Result<(), String> {
        for (result, request) in results.iter().zip(requests) {
            let report = result.as_ref().unwrap();
            request.frozen_backup.verify(&report.backup).map_err(|e| e.to_string())?;
            verify_backup(&report.destination, &report.backup, expected).map_err(|e| e.to_string())?;
            let restored_stamp = stamp(&report.destination, "inspect shared restored file").map_err(|e| e.to_string())?;
            sync_restored(&report.destination, restored_stamp).map_err(|error| format!("{}: {}", error.operation, error.message))?;
        }
        Ok(())
    })();
    if let Err(error) = verified {
        let cleanup = release_reservations(&mut reservations);
        if requests.iter().any(|request| request.frozen_backup.verify(request.backup).is_err()) {
            rollback_restore_holds(&mut results, &mut holds, requests);
            merge_restore_cleanup(&mut results[0], cleanup);
        } else {
            let reason = format!("shared backup retained: {error}; cleanup: {cleanup:?}");
            finish_restore_holds(&mut results, &mut holds);
            retain_restore_group(&mut results, Some(expected), &reason);
        }
        return results;
    }
    #[cfg(test)] if let Some(hook) = before_removal { hook(&reservations); }
    let mut dispositions = BTreeMap::new();
    let mut stopped = false;
    for (backup, (restored, frozen)) in &backups {
        let disposition = if stopped {
            let current = stamp(backup, "inspect retained shared backup").ok()
                .filter(|current| same_moved_file(*current, expected));
            (false, Some(match current {
                Some(current) => backup_path_error("retain shared backup", "another backup removal failed", backup, current),
                None => backup_removal_error("retain shared backup", "backup location is unknown", None),
            }))
        } else {
            match frozen.verify_after_alias_removal(backup) {
                Ok(current) => remove_verified_backup_reserved(restored, backup, current, #[cfg(test)] None),
                _ => (false, Some(backup_path_error("reinspect shared backup",
                    WorkflowError::BackupChanged(backup.clone()), backup, expected))),
            }
        };
        if dispositions.is_empty() && !disposition.0 && frozen.verify(backup).is_err() {
            let cleanup = release_reservations(&mut reservations);
            rollback_restore_holds(&mut results, &mut holds, requests);
            merge_restore_cleanup(&mut results[0], cleanup);
            return results;
        }
        if !disposition.0 || disposition.1.is_some() { stopped = true; }
        dispositions.insert(backup.clone(), disposition);
    }
    let cleanup = release_reservations(&mut reservations);
    for result in &mut results {
        let report = result.as_mut().unwrap();
        let (removed, error) = dispositions.get(&report.backup).unwrap();
        report.backup_removed = *removed; report.backup_removal_error = error.clone();
        for failure in &cleanup {
            let message = format!("{} {}: {}", failure.operation, failure.path.display(), failure.source);
            match &mut report.backup_removal_error {
                Some(error) => error.message.push_str(&format!("; {message}")),
                None => report.backup_removal_error = Some(backup_removal_error("release backup reservation", message, None)),
            }
        }
    }
    finish_restore_holds(&mut results, &mut holds);
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    // Test-only calls use the same declarations as sys/xattr.h, sys/acl.h,
    // membership.h and sys/stat.h in the macOS SDK.
    #[cfg(target_os = "macos")]
    #[link(name = "System")]
    unsafe extern "C" {
        fn fsetxattr(fd: c_int, name: *const c_char, value: *const c_void, size: usize, position: u32, options: c_int) -> c_int;
        fn fchflags(fd: c_int, flags: u32) -> c_int;
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_create_entry(acl: *mut *mut c_void, entry: *mut *mut c_void) -> c_int;
        fn acl_set_tag_type(entry: *mut c_void, tag: c_int) -> c_int;
        fn acl_set_qualifier(entry: *mut c_void, qualifier: *const c_void) -> c_int;
        fn acl_get_permset(entry: *mut c_void, permset: *mut *mut c_void) -> c_int;
        fn acl_add_perm(permset: *mut c_void, perm: c_int) -> c_int;
        fn acl_set_permset(entry: *mut c_void, permset: *mut c_void) -> c_int;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_int) -> c_int;
        fn mbr_uid_to_uuid(uid: u32, uuid: *mut u8) -> c_int;
    }

    #[cfg(target_os = "macos")]
    fn set_xattr(file: &File, name: &CString, value: &[u8]) {
        // SAFETY: name and value are live, and the descriptor is open.
        assert_eq!(unsafe { fsetxattr(file.as_raw_fd(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) }, 0);
    }

    #[cfg(target_os = "macos")]
    fn set_resource_fork(file: &File, name: &CString, bytes: &[u8]) {
        for (index, chunk) in bytes.chunks(65536).enumerate() {
            // SAFETY: resource-fork writes support offsets; the chunk is live.
            assert_eq!(unsafe { fsetxattr(file.as_raw_fd(), name.as_ptr(), chunk.as_ptr().cast(), chunk.len(), (index * 65536) as u32, 0) }, 0);
        }
    }

    #[cfg(target_os = "macos")]
    fn read_resource_fork(file: &File, path: &Path, name: &CString, bytes: &mut [u8]) {
        for (index, chunk) in bytes.chunks_mut(65536).enumerate() {
            read_xattr(file, path, name, chunk, (index * 65536) as u32).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    fn add_test_acl(file: &File) {
        let mut uuid = [0u8; 16];
        let uid = file.metadata().unwrap().uid();
        // SAFETY: uuid has the 16-byte storage required by mbr_uid_to_uuid.
        assert_eq!(unsafe { mbr_uid_to_uuid(uid, uuid.as_mut_ptr()) }, 0);
        // SAFETY: the SDK ACL calls initialize and populate working storage.
        unsafe {
            let mut acl = acl_init(1);
            assert!(!acl.is_null());
            let mut entry = std::ptr::null_mut();
            assert_eq!(acl_create_entry(&mut acl, &mut entry), 0);
            assert_eq!(acl_set_tag_type(entry, 1), 0); // ACL_EXTENDED_ALLOW
            assert_eq!(acl_set_qualifier(entry, uuid.as_ptr().cast()), 0);
            let mut perms = std::ptr::null_mut();
            assert_eq!(acl_get_permset(entry, &mut perms), 0);
            assert_eq!(acl_add_perm(perms, 1 << 7), 0); // ACL_READ_ATTRIBUTES
            assert_eq!(acl_set_permset(entry, perms), 0);
            assert_eq!(acl_set_fd_np(file.as_raw_fd(), acl, ACL_TYPE_EXTENDED), 0);
            assert_eq!(acl_free(acl), 0);
        }
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            for _ in 0..128 {
                let id = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!("pn-workflow-rollback-test-{}-{id}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("create isolated fixture: {e}"),
                }
            }
            panic!("could not allocate isolated rollback fixture")
        }
        fn setup(&self) -> (PathBuf, PathBuf, OutputPlan, StagedOutput) {
            let source = self.0.join("misnamed.wav");
            let backup_dir = self.0.join("backup");
            fs::create_dir(&backup_dir).unwrap();
            fs::write(&source, b"original").unwrap();
            let files = discover_inputs(&[source.clone()]).unwrap();
            let settings = OutputSettings { mode: OutputMode::Replace { backup_directory: backup_dir }, format: AudioFormat::Wav };
            // A real decoder could report AAC inside a .wav-named container;
            // the changed extension gives a separate final-path collision.
            let plan = OutputPlan::new(&files[0], SourceCodec::Aac, &settings, None).unwrap();
            let transaction = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
            let backup = transaction.backup_path().unwrap().to_owned();
            let mut stage = OpenOptions::new().write(true).create_new(true).open(transaction.staging_path()).unwrap();
            stage.write_all(b"encoded").unwrap();
            stage.sync_all().unwrap();
            fs::write(plan.destination(), b"late destination occupant").unwrap();
            (source, backup, plan, transaction)
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }

    #[test]
    fn failed_alias_resolution_cleans_workspace_and_existing_reservation() {
        let fixture = Fixture::new();
        let reservation = Reservation::acquire(&fixture.0.join("song.wav")).unwrap();
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
        let error = Workspace::create_impl(&fixture.0, "wav", true, None).err().unwrap();
        assert!(error.to_string().contains("injected workspace alias failure"));
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
        reservation.release().unwrap();
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[test]
    fn swapped_workspace_directory_never_changes_foreign_mode_or_acl() {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o770)).unwrap();
        let foreign = fixture.0.join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o750)).unwrap();
        let foreign_file = File::open(&foreign).unwrap();
        #[cfg(target_os = "macos")]
        add_test_acl(&foreign_file);
        #[cfg(target_os = "linux")]
        {
            let mut acl = 2u32.to_le_bytes().to_vec();
            for (tag, permissions, id) in [
                (0x01u16, 0x07u16, u32::MAX), (0x02, 0x04, unsafe { libc::geteuid() }.wrapping_add(1)),
                (0x04, 0x05, u32::MAX), (0x10, 0x05, u32::MAX), (0x20, 0x00, u32::MAX),
            ] {
                acl.extend_from_slice(&tag.to_le_bytes()); acl.extend_from_slice(&permissions.to_le_bytes());
                acl.extend_from_slice(&id.to_le_bytes());
            }
            let status = unsafe { libc::fsetxattr(foreign_file.as_raw_fd(), c"system.posix_acl_access".as_ptr(), acl.as_ptr().cast(), acl.len(), 0) };
            if status != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EOPNOTSUPP) || std::env::var_os("PN_REQUIRE_TEST_ACL").is_some() {
                    panic!("set synthetic foreign ACL: {error}");
                }
            }
        }
        let before_mode = foreign_file.metadata().unwrap().mode() & 0o7777;
        #[cfg(target_os = "macos")]
        let before_acl = acl_bytes(&foreign_file, &foreign).unwrap();
        #[cfg(target_os = "linux")]
        let before_acl = xattr_names(&foreign_file, &foreign).unwrap().into_iter()
            .filter(|name| name.as_bytes() == b"system.posix_acl_access")
            .map(|name| { let size = xattr_size(&foreign_file, &foreign, &name).unwrap(); let mut bytes = vec![0; size];
                read_xattr(&foreign_file, &foreign, &name, &mut bytes, 0).unwrap(); bytes }).collect::<Vec<_>>();
        let created_moved = fixture.0.join("created-workspace-moved");
        let captured = Arc::new(Mutex::new(None));
        let result_path = Arc::clone(&captured);
        let error = Workspace::create_impl(&fixture.0, "wav", false, Some(Box::new(move |workspace| {
            fs::rename(workspace, &created_moved).unwrap();
            fs::rename(&foreign, workspace).unwrap();
            *result_path.lock().unwrap() = Some(workspace.to_owned());
        }))).err().unwrap();
        assert!(error.to_string().contains("workspace changed before it could be pinned"));
        let current = captured.lock().unwrap().clone().unwrap();
        assert!(current.is_dir());
        assert_eq!(foreign_file.metadata().unwrap().mode() & 0o7777, before_mode);
        #[cfg(target_os = "macos")]
        assert_eq!(acl_bytes(&foreign_file, &current).unwrap(), before_acl);
        #[cfg(target_os = "linux")]
        {
            let after_acl = xattr_names(&foreign_file, &current).unwrap().into_iter()
                .filter(|name| name.as_bytes() == b"system.posix_acl_access")
                .map(|name| { let size = xattr_size(&foreign_file, &current, &name).unwrap(); let mut bytes = vec![0; size];
                    read_xattr(&foreign_file, &current, &name, &mut bytes, 0).unwrap(); bytes }).collect::<Vec<_>>();
            assert_eq!(after_acl, before_acl);
        }
    }

    #[test]
    fn restrictive_umask_refuses_without_stranding_a_reservation() {
        use std::os::unix::process::CommandExt;
        const CHILD: &str = "PN_TEST_WORKSPACE_UMASK_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = PathBuf::from(path);
            let error = Reservation::acquire(&path.join("song.wav")).err().unwrap();
            assert!(error.to_string().contains("workspace"));
            assert!(!path.join(".song.wav.pn-reservation").exists());
            return;
        }
        let fixture = Fixture::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.arg("--exact").arg("file_workflow::tests::restrictive_umask_refuses_without_stranding_a_reservation")
            .arg("--nocapture").env(CHILD, &fixture.0);
        unsafe { child.pre_exec(|| { libc::umask(0o400); Ok(()) }); }
        assert!(child.status().unwrap().success());
        for entry in fs::read_dir(&fixture.0).unwrap() {
            fs::set_permissions(entry.unwrap().path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inherited_macos_acl_is_refused_without_stranding_a_reservation() {
        let fixture = Fixture::new();
        let status = std::process::Command::new("/bin/chmod").args(["+a",
            "everyone allow list,add_file,search,add_subdirectory,delete_child,file_inherit,directory_inherit"])
            .arg(&fixture.0).status().unwrap();
        assert!(status.success());
        let error = Reservation::acquire(&fixture.0.join("song.wav")).err().unwrap();
        assert!(error.to_string().contains("extended ACL"));
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn new_mp3_preserves_destination_default_acl_and_mode() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav");
        fs::write(&source, b"source").unwrap();
        let parent = File::open(&fixture.0).unwrap();
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (0x01u16, 0x07u16, u32::MAX), (0x02, 0x06, unsafe { libc::geteuid() }.wrapping_add(1)),
            (0x04, 0x06, u32::MAX), (0x10, 0x06, u32::MAX), (0x20, 0x00, u32::MAX),
        ] {
            acl.extend_from_slice(&tag.to_le_bytes()); acl.extend_from_slice(&permissions.to_le_bytes());
            acl.extend_from_slice(&id.to_le_bytes());
        }
        let status = unsafe { libc::fsetxattr(parent.as_raw_fd(), c"system.posix_acl_default".as_ptr(), acl.as_ptr().cast(), acl.len(), 0) };
        if status != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EOPNOTSUPP) && std::env::var_os("PN_REQUIRE_TEST_ACL").is_none() { return; }
            panic!("set synthetic default ACL: {error}");
        }
        let reference = fixture.0.join("ordinary.mp3");
        let reference_file = OpenOptions::new().write(true).create_new(true).mode(0o666).open(&reference).unwrap();
        let get_acl = |file: &File| {
            let size = unsafe { libc::fgetxattr(file.as_raw_fd(), c"system.posix_acl_access".as_ptr(), std::ptr::null_mut(), 0) };
            assert!(size > 0);
            let mut bytes = vec![0u8; size as usize];
            assert_eq!(unsafe { libc::fgetxattr(file.as_raw_fd(), c"system.posix_acl_access".as_ptr(), bytes.as_mut_ptr().cast(), bytes.len()) }, size);
            bytes
        };
        let expected_acl = get_acl(&reference_file);
        let expected_mode = reference_file.metadata().unwrap().mode() & 0o7777;
        assert_eq!(expected_mode, 0o660);
        let files = discover_inputs(&[source]).unwrap();
        let plan = OutputPlan::new(&files[0], SourceCodec::Lpcm,
            &OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Mp3 }, None).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&files[0].path).unwrap()).unwrap();
        let mut stage = tx.create_stage().unwrap();
        stage.write_all(b"synthetic mp3").unwrap();
        let output = tx.finish(&Cancellation::new()).unwrap().destination;
        let output_file = File::open(&output).unwrap();
        assert_eq!(output_file.metadata().unwrap().mode() & 0o7777, expected_mode);
        assert_eq!(get_acl(&output_file), expected_acl);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn private_state_refuses_extended_acl_even_with_safe_mode_bits() {
        let fixture = Fixture::new();
        let state = fixture.0.join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let directory = File::open(&state).unwrap();
        add_test_acl(&directory);
        assert!(require_no_extended_acl(&directory, &state).is_err());
        let settings = crate::settings::SettingsStore::new(&state, &fixture.0, 1);
        assert!(settings.load().is_err());
        let statistics = crate::statistics::StatisticsStore::new(&state);
        assert!(statistics.show(&Cancellation::new()).is_err());
        assert!(!state.join("settings.lock").exists());
        assert!(!state.join("statistics.lock").exists());
    }

    #[test]
    fn workspace_symlink_swap_never_redirects_stage_or_cleanup() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let source = fixture.0.join("source.wav");
        let output = fixture.0.join("output"); let victim = fixture.0.join("victim");
        fs::create_dir(&output).unwrap(); fs::create_dir(&victim).unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o770)).unwrap();
        fs::write(&source, b"original").unwrap();
        fs::write(victim.join("output.wav"), b"victim").unwrap();
        let selected = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: output.clone(), mirror: false }, format: AudioFormat::Wav };
        let plan = OutputPlan::new(&selected[0], SourceCodec::Lpcm, &settings, None).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        let original_name = tx.workspace.directory.clone();
        let moved = output.join("moved-workspace");
        fs::rename(&original_name, &moved).unwrap();
        symlink(&victim, &original_name).unwrap();
        fs::write(tx.staging_path(), b"private candidate").unwrap();
        tx.remember_staging_file().unwrap();
        let error = tx.abort().unwrap_err();
        assert!(matches!(error, WorkflowError::Transaction { published: false, .. }));
        assert_eq!(fs::read(victim.join("output.wav")).unwrap(), b"victim");
        assert!(fs::symlink_metadata(&original_name).unwrap().file_type().is_symlink());
        assert!(!moved.join("output.wav").exists());
    }

    #[test]
    fn reservation_swap_is_quarantined_without_deleting_an_unowned_file() {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o770)).unwrap();
        let reservation = Reservation::acquire(&fixture.0.join("song.wav")).unwrap();
        let path = reservation.path.clone();
        let saved = fixture.0.join("saved-reservation");
        let changed = path.clone(); let saved_for_hook = saved.clone();
        let error = reservation.release_impl(Some(Box::new(move || {
            fs::rename(&changed, &saved_for_hook).unwrap();
            fs::write(&changed, b"unowned file").unwrap();
        }))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(path).unwrap(), b"unowned file");
        assert!(saved.exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
    }

    #[test]
    fn rewritten_stage_remains_pinned_across_fd_churn() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
        let selected = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
        let plan = OutputPlan::new(&selected[0], SourceCodec::Lpcm, &settings, None).unwrap();
        let iterations = std::env::var("PN_WORKFLOW_PIN_STRESS").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
        for _ in 0..iterations {
            let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
            fs::write(tx.staging_path(), b"before").unwrap();
            tx.remember_staging_file().unwrap();
            let rewrite = tx.workspace.alias.join("output.wav_");
            fs::write(&rewrite, b"tagged").unwrap();
            fs::rename(&rewrite, tx.staging_path()).unwrap();
            tx.accept_tag_rewrite().unwrap();
            let expected = tx.workspace.stage_stamp("capture rewritten stage").unwrap();
            for _ in 0..3 {
                let other = Workspace::create(&fixture.0, "caf").unwrap();
                assert!(other.cleanup(false).is_empty());
                let open = tx.workspace.open_entry(tx.staging_path()).unwrap();
                assert_eq!(metadata_stamp(&open.metadata().unwrap()), expected);
                drop(open);
            }
            assert_eq!(tx.workspace.stage_stamp("reinspect rewritten stage").unwrap(), expected);
            let publication = tx.finish(&Cancellation::new()).unwrap();
            assert_eq!(fs::read(&publication.destination).unwrap(), b"tagged");
            fs::remove_file(publication.destination).unwrap();
        }
    }

    #[test]
    fn renamed_output_parent_is_detected_before_and_after_publication() {
        for after_publish in [false, true] {
            let fixture = Fixture::new();
            let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
            let out = fixture.0.join("out"); fs::create_dir(&out).unwrap();
            let selected = discover_inputs(&[source.clone()]).unwrap();
            let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: out, mirror: false }, format: AudioFormat::Wav };
            let plan = OutputPlan::new(&selected[0], SourceCodec::Lpcm, &settings, None).unwrap();
            let mut tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
            fs::write(tx.staging_path(), b"encoded").unwrap();
            let parent = plan.destination().parent().unwrap().to_owned();
            let moved = parent.parent().unwrap().join("moved-output");
            let old = parent.clone(); let new = moved.clone();
            let hook = Box::new(move || {
                fs::rename(&old, &new).unwrap();
                fs::create_dir(&old).unwrap();
                fs::write(old.join(".song_PN.wav.pn-reservation"), b"unrelated").unwrap();
            });
            if after_publish { tx.after_publish = Some(hook); } else { tx.before_commit = Some(hook); }
            let error = tx.finish(&Cancellation::new()).unwrap_err();
            let moved_output = moved.join("song_PN.wav");
            match error {
                WorkflowError::Transaction { cause, recovery, published, .. } => {
                    assert_eq!(published, after_publish);
                    assert!(matches!(*cause, WorkflowError::InvalidPath { path, .. } if path == moved_output));
                    if after_publish {
                        assert_eq!(fs::read(&moved_output).unwrap(), b"encoded");
                        assert_eq!(fs::read(recovery.unwrap()).unwrap(), b"encoded");
                    } else { assert!(recovery.is_none()); assert!(!moved_output.exists()); }
                }
                e => panic!("{e}"),
            }
            assert_eq!(fs::read(parent.join(".song_PN.wav.pn-reservation")).unwrap(), b"unrelated");
            assert!(!moved.join(".song_PN.wav.pn-reservation").exists());
            assert_eq!(fs::read(source).unwrap(), b"source");
        }
    }

    #[test]
    fn unrecorded_and_replaced_stages_are_not_deleted() {
        let fixture = Fixture::new();
        for replace in [false, true] {
            let source = fixture.0.join(if replace { "second.wav" } else { "first.wav" });
            fs::write(&source, b"source").unwrap();
            let selected = discover_inputs(&[source.clone()]).unwrap();
            let plan = OutputPlan::new(&selected[0], SourceCodec::Lpcm,
                &OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav }, None).unwrap();
            let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
            let stage = tx.workspace.directory.join("output.wav");
            fs::write(tx.staging_path(), b"first").unwrap();
            if replace {
                tx.remember_staging_file().unwrap();
                fs::rename(tx.staging_path(), tx.workspace.alias.join("saved")).unwrap();
                fs::write(tx.staging_path(), b"foreign").unwrap();
            }
            assert!(tx.abort().is_err());
            assert_eq!(fs::read(stage).unwrap(), if replace { b"foreign".as_slice() } else { b"first".as_slice() });
        }
    }

    #[test]
    fn unknown_sensitivity_refuses_batch_collisions() {
        let fixture = Fixture::new();
        let a = fixture.0.join("a"); let b = fixture.0.join("b"); let out = fixture.0.join("out");
        fs::create_dir(&a).unwrap(); fs::create_dir(&b).unwrap(); fs::create_dir(&out).unwrap();
        let upper = a.join("Song.wav"); let lower = b.join("song.wav");
        fs::write(&upper, b"a").unwrap(); fs::write(&lower, b"b").unwrap();
        let selected = discover_inputs(&[upper, lower]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: out, mirror: false }, format: AudioFormat::Wav };
        let plans = selected.iter().map(|file| OutputPlan::new(file, SourceCodec::Lpcm, &settings, None).unwrap()).collect::<Vec<_>>();
        let unknown = |_: &Path| Ok(policy_for_fs_type(b"other", pathconf_case_sensitive(-1)));
        assert!(matches!(validate_batch_with_policy(&plans, &selected, &unknown), Err(WorkflowError::DestinationConflict { .. })));
        assert!(!pathconf_case_sensitive(0)); assert!(pathconf_case_sensitive(1));
        #[cfg(target_os = "linux")]
        {
            assert!(!linux_case_sensitive(None));
            assert!(linux_case_sensitive(Some(0)));
            assert!(!linux_case_sensitive(Some(0x4000_0000)));
            let unknown_linux = |_: &Path| Ok(NamePolicy { normalization: NameNormalization::Bytes,
                case_sensitive: linux_case_sensitive(None) });
            assert!(matches!(validate_batch_with_policy(&plans, &selected, &unknown_linux), Err(WorkflowError::DestinationConflict { .. })));
        }
    }

    #[test]
    fn mp3_final_mode_matches_create_and_overwrite_preserves_mode() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
        let files = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Mp3 };
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        let mode = tx.new_mp3_mode.unwrap();
        let probe = tx.workspace.alias.join("ordinary");
        OpenOptions::new().write(true).create_new(true).mode(0o666).open(&probe).unwrap();
        assert_eq!(fs::metadata(&probe).unwrap().mode() & 0o7777, mode);
        fs::remove_file(probe).unwrap();
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(tx.staging_path()).unwrap();
        assert_eq!(fs::metadata(tx.staging_path()).unwrap().mode() & 0o077, 0);
        let result = tx.finish(&Cancellation::new()).unwrap();
        assert_eq!(fs::metadata(&result.destination).unwrap().mode() & 0o7777, mode);
        fs::set_permissions(&result.destination, fs::Permissions::from_mode(0o640)).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(tx.staging_path()).unwrap();
        let result = tx.finish(&Cancellation::new()).unwrap();
        assert!(result.replaced_existing_output);
        assert_eq!(fs::metadata(result.destination).unwrap().mode() & 0o7777, 0o640);
    }

    #[test]
    fn overwrite_preserves_group_and_effective_acl() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); let destination = fixture.0.join("song_PN.wav");
        fs::write(&source, b"source").unwrap(); fs::write(&destination, b"old output").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
        let original = OpenOptions::new().read(true).write(true).open(&destination).unwrap();
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        assert!(count >= 0);
        let mut groups = vec![0 as libc::gid_t; count as usize];
        if count > 0 { assert_eq!(unsafe { libc::getgroups(count, groups.as_mut_ptr()) }, count); }
        if let Some(group) = groups.into_iter().find(|group| u64::from(*group) != u64::from(original.metadata().unwrap().gid())) {
            assert_eq!(unsafe { libc::fchown(original.as_raw_fd(), !0 as libc::uid_t, group) }, 0);
        }
        #[cfg(target_os = "macos")]
        add_test_acl(&original);
        #[cfg(target_os = "linux")]
        let acl_supported = {
            let name = CString::new("system.posix_acl_access").unwrap();
            let mut acl = 2u32.to_le_bytes().to_vec();
            for (tag, permissions, id) in [
                (0x01u16, 0x06u16, u32::MAX), (0x02, 0x04, unsafe { libc::geteuid() }.wrapping_add(1)),
                (0x04, 0x04, u32::MAX), (0x10, 0x04, u32::MAX), (0x20, 0x00, u32::MAX),
            ] {
                acl.extend_from_slice(&tag.to_le_bytes()); acl.extend_from_slice(&permissions.to_le_bytes());
                acl.extend_from_slice(&id.to_le_bytes());
            }
            let status = unsafe { libc::fsetxattr(original.as_raw_fd(), name.as_ptr(), acl.as_ptr().cast(), acl.len(), 0) };
            if status != 0 {
                let error = io::Error::last_os_error();
                assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP), "set ACL: {error}");
                false
            } else { true }
        };
        let before = original.metadata().unwrap();
        #[cfg(target_os = "macos")]
        let before_acl = acl_bytes(&original, &destination).unwrap();
        #[cfg(target_os = "linux")]
        let acl_name = CString::new("system.posix_acl_access").unwrap();
        let files = discover_inputs(&[source.clone()]).unwrap();
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm,
            &OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav }, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        fs::write(tx.staging_path(), b"new output").unwrap();
        tx.finish(&Cancellation::new()).unwrap();
        let result = open_read(&destination).unwrap();
        let after = result.metadata().unwrap();
        assert_eq!(after.gid(), before.gid());
        assert_eq!(after.mode() & 0o7777, before.mode() & 0o7777);
        #[cfg(target_os = "macos")]
        assert_eq!(acl_bytes(&result, &destination).unwrap(), before_acl);
        #[cfg(target_os = "linux")]
        if acl_supported { assert!(xattr_values_equal(&original, &result, &destination, &destination, &acl_name).unwrap()); }
        assert_eq!(fs::read(destination).unwrap(), b"new output");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a synthetic externally prepared non-member group fixture"]
    fn overwrite_refuses_when_existing_group_cannot_be_preserved() {
        let directory = PathBuf::from(std::env::var_os("PN_GROUP_DENIED_FIXTURE").unwrap());
        let source = directory.join("song.wav"); let destination = directory.join("song_PN.wav");
        let before = fs::metadata(&destination).unwrap();
        assert_eq!(before.uid(), unsafe { libc::geteuid() });
        assert_ne!(before.gid(), unsafe { libc::getegid() });
        let files = discover_inputs(&[source.clone()]).unwrap();
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm,
            &OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav }, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        fs::write(tx.staging_path(), b"new output").unwrap();
        let error = tx.finish(&Cancellation::new()).unwrap_err();
        assert!(error.to_string().contains("preserve overwritten output owner and group"), "{error}");
        assert_eq!(fs::read(destination).unwrap(), b"old output");
        assert_eq!(fs::read(source).unwrap(), b"source");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a synthetic externally prepared supplementary group fixture"]
    fn overwrite_preserves_an_authorized_other_group() {
        let directory = PathBuf::from(std::env::var_os("PN_GROUP_ALLOWED_FIXTURE").unwrap());
        let source = directory.join("song.wav"); let destination = directory.join("song_PN.wav");
        let before = fs::metadata(&destination).unwrap();
        assert_eq!(before.uid(), unsafe { libc::geteuid() });
        assert_ne!(before.gid(), unsafe { libc::getegid() });
        let files = discover_inputs(&[source.clone()]).unwrap();
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm,
            &OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav }, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        fs::write(tx.staging_path(), b"new output").unwrap();
        tx.finish(&Cancellation::new()).unwrap();
        assert_eq!(fs::metadata(&destination).unwrap().gid(), before.gid());
        assert_eq!(fs::read(destination).unwrap(), b"new output");
        assert_eq!(fs::read(source).unwrap(), b"source");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a synthetic root-owned workspace with another file owner"]
    fn overwrite_preserves_another_owner_when_permitted() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let directory = PathBuf::from(std::env::var_os("PN_OWNER_ALLOWED_FIXTURE").unwrap());
        let source = directory.join("song.wav"); let destination = directory.join("song_PN.wav");
        let before = fs::metadata(&destination).unwrap();
        assert_ne!(before.uid(), unsafe { libc::geteuid() });
        let files = discover_inputs(&[source.clone()]).unwrap();
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm,
            &OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav }, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        fs::write(tx.staging_path(), b"new output").unwrap();
        tx.finish(&Cancellation::new()).unwrap();
        let after = fs::metadata(&destination).unwrap();
        assert_eq!((after.uid(), after.gid(), after.mode() & 0o7777), (before.uid(), before.gid(), before.mode() & 0o7777));
        assert_eq!(fs::read(destination).unwrap(), b"new output");
    }

    #[test]
    fn new_output_sync_failures_report_publication_in_both_branches() {
        for overwrite in [false, true] {
            let fixture = Fixture::new();
            let source = fixture.0.join("song.wav");
            fs::write(&source, b"original").unwrap();
            let files = discover_inputs(&[source.clone()]).unwrap();
            let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm,
                &OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav }, None).unwrap();
            if overwrite {
                plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
            }
            let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
            fs::write(tx.staging_path(), b"processed").unwrap();
            *tx.workspace.sync_error.lock().unwrap() = Some(io::Error::other("injected output sync failure"));
            let error = tx.finish(&Cancellation::new()).unwrap_err();
            let recovery = match error {
                WorkflowError::Transaction { recovery: Some(path), published: true, .. } => path,
                error => panic!("overwrite={overwrite}: {error}"),
            };
            assert_eq!(fs::read(plan.destination()).unwrap(), b"processed");
            assert_eq!(fs::read(recovery).unwrap(), b"processed");
            assert_eq!(fs::read(source).unwrap(), b"original");
        }
    }

    #[test]
    fn sync_failures_keep_verified_backup_and_displaced_original() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); let backups = fixture.0.join("backups");
        fs::create_dir(&backups).unwrap(); fs::write(&source, b"original").unwrap();
        let selected = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::Replace { backup_directory: backups }, format: AudioFormat::Wav };
        let plan = OutputPlan::new(&selected[0], SourceCodec::Lpcm, &settings, None).unwrap();
        let identity = FileIdentity::capture(&source).unwrap();
        let error = plan.begin_impl(&identity, Some(Box::new(|backup| {
            Err(io_error("sync verified backup directory", backup, io::Error::other("injected")))
        }))).err().unwrap();
        let backup = match error { WorkflowError::Transaction { backup: Some(path), published: false, .. } => path, e => panic!("{e}") };
        assert_eq!(fs::read(&source).unwrap(), b"original"); assert_eq!(fs::read(backup).unwrap(), b"original");
        let tx = plan.begin(&identity).unwrap();
        let backup = tx.backup_path().unwrap().to_owned();
        fs::write(tx.staging_path(), b"processed").unwrap();
        *tx.workspace.sync_error.lock().unwrap() = Some(io::Error::other("injected output sync failure"));
        let error = tx.finish(&Cancellation::new()).unwrap_err();
        let recovery = match error { WorkflowError::Transaction { recovery: Some(path), published: true, .. } => path, e => panic!("{e}") };
        assert_eq!(fs::read(source).unwrap(), b"processed");
        assert_eq!(fs::read(backup).unwrap(), b"original");
        assert_eq!(fs::read(recovery).unwrap(), b"original");
    }

    #[test]
    fn overwrite_sync_failure_retains_the_displaced_output() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); let destination = fixture.0.join("song_PN.wav");
        fs::write(&source, b"source").unwrap(); fs::write(&destination, b"old").unwrap();
        let files = discover_inputs(&[source.clone()]).unwrap();
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm,
            &OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav }, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        let tx = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        fs::write(tx.staging_path(), b"new").unwrap();
        *tx.workspace.sync_error.lock().unwrap() = Some(io::Error::other("injected output sync failure"));
        let error = tx.finish(&Cancellation::new()).unwrap_err();
        let recovery = match error { WorkflowError::Transaction { recovery: Some(path), published: true, .. } => path, e => panic!("{e}") };
        assert_eq!(fs::read(destination).unwrap(), b"new");
        assert_eq!(fs::read(recovery).unwrap(), b"old");
        assert_eq!(fs::read(source).unwrap(), b"source");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn metadata_comparison_excludes_only_uncopyable_provenance() {
        let source_names = [b"com.apple.provenance".as_slice(), b"com.apple.macl", b"com.apple.quarantine", b"com.example.test"];
        let copy_names = [b"com.apple.macl".as_slice(), b"com.apple.quarantine", b"com.example.test"];
        fn filtered(names: &[&'static [u8]]) -> Vec<&'static [u8]> {
            names.iter().copied().filter(|name| comparable_xattr(name)).collect()
        }
        assert_eq!(filtered(&source_names), filtered(&copy_names));
        assert!(!comparable_xattr(b"com.apple.provenance"));
        assert!(comparable_xattr(b"com.apple.macl"));
        assert!(comparable_xattr(b"com.apple.quarantine"));
        assert!(comparable_xattr(b"com.apple.root.installed"));
        assert!(comparable_xattr(b"com.apple.provenance#extra"));

        let fixture = Fixture::new();
        let original_path = fixture.0.join("original");
        let copy_path = fixture.0.join("copy");
        fs::write(&original_path, b"same").unwrap(); fs::write(&copy_path, b"same").unwrap();
        let original = OpenOptions::new().read(true).write(true).open(&original_path).unwrap();
        let copy = OpenOptions::new().read(true).write(true).open(&copy_path).unwrap();
        let name = CString::new("com.example.test").unwrap();
        set_xattr(&original, &name, b"original"); set_xattr(&copy, &name, b"different");
        assert!(!xattrs_equal(&original, &copy, &original_path, &copy_path).unwrap());
        set_xattr(&copy, &name, b"original");
        assert!(xattrs_equal(&original, &copy, &original_path, &copy_path).unwrap());
        let before = overwrite_metadata(&original, &original_path).unwrap();
        set_xattr(&original, &name, b"modified");
        let after = overwrite_metadata(&original, &original_path).unwrap();
        assert!(before != after);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn copied_xattrs_restore_quarantine_and_other_differences_exactly() {
        let fixture = Fixture::new();
        let source_path = fixture.0.join("source");
        let copied_path = fixture.0.join("copied");
        let verified_path = fixture.0.join("verified");
        fs::write(&source_path, b"same data").unwrap(); fs::write(&copied_path, b"same data").unwrap();
        let source = OpenOptions::new().read(true).write(true).open(&source_path).unwrap();
        let copied = OpenOptions::new().read(true).write(true).open(&copied_path).unwrap();
        let quarantine = CString::new("com.apple.quarantine").unwrap();
        let custom = CString::new("com.example.metadata").unwrap();
        let extra = CString::new("com.example.extra").unwrap();
        set_xattr(&source, &quarantine, b"0081;12345678;platinum-unit;https://example.invalid/");
        set_xattr(&source, &custom, b"source value");
        assert_eq!(unsafe { fcopyfile(source.as_raw_fd(), copied.as_raw_fd(), std::ptr::null_mut(), COPYFILE_METADATA) }, 0);
        set_xattr(&copied, &quarantine, b"0083;87654321;other;https://example.invalid/");
        set_xattr(&copied, &custom, b"different");
        set_xattr(&copied, &extra, b"extra");
        assert!(!xattrs_equal(&source, &copied, &source_path, &copied_path).unwrap());
        reconcile_copied_xattrs(&source, &copied, &source_path, &copied_path, None).unwrap();
        assert!(xattrs_equal(&source, &copied, &source_path, &copied_path).unwrap());
        assert!(!xattr_names(&copied, &copied_path).unwrap().contains(&extra));
        let source_stamp = stamp(&source_path, "capture source").unwrap();
        copy_new_verified(&source_path, &verified_path, source_stamp, None).unwrap();
        let verified = open_read(&verified_path).unwrap();
        assert!(xattrs_equal(&source, &verified, &source_path, &verified_path).unwrap());
    }

    #[test]
    fn overwrite_swapped_entry_is_verified_and_races_are_rolled_back() {
        use std::os::unix::fs::symlink;
        for mode in 0..3 {
            let fixture = Fixture::new();
            let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
            let destination = fixture.0.join("song_PN.wav"); fs::write(&destination, b"old output").unwrap();
            let alternate = fixture.0.join("alternate"); fs::write(&alternate, b"racing file").unwrap();
            let alternate_inode = fs::metadata(&alternate).unwrap().ino();
            let files = discover_inputs(&[source.clone()]).unwrap();
            let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
            let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
            let guard = Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap());
            plan.enable_overwrite(guard).unwrap();
            validate_batch(&[&plan], &files).unwrap();
            let mut transaction = plan.begin(&FileIdentity::capture(plan.source()).unwrap()).unwrap();
            fs::write(transaction.staging_path(), b"candidate").unwrap();
            let target = plan.destination().to_owned(); let changed = target.clone();
            let source_for_hook = source.clone();
            transaction.after_overwrite_check = Some(Box::new(move || {
                fs::remove_file(&changed).unwrap();
                match mode {
                    0 => fs::rename(&alternate, &changed).unwrap(),
                    1 => symlink(&source_for_hook, &changed).unwrap(),
                    _ => fs::hard_link(&source_for_hook, &changed).unwrap(),
                }
            }));
            let error = transaction.finish(&Cancellation::new()).unwrap_err();
            assert!(matches!(error, WorkflowError::Transaction { cause, published: false, rollback: None, .. }
                if matches!(*cause, WorkflowError::DestinationChanged(_))));
            match mode {
                0 => {
                    assert_eq!(fs::read(&target).unwrap(), b"racing file");
                    assert_eq!(fs::metadata(&target).unwrap().ino(), alternate_inode);
                }
                1 => assert!(fs::symlink_metadata(&target).unwrap().file_type().is_symlink()),
                _ => assert_eq!(fs::metadata(&target).unwrap().ino(), fs::metadata(&source).unwrap().ino()),
            }
            assert_eq!(fs::read(&source).unwrap(), b"source");
        }
    }

    #[test]
    fn overwrite_rollback_retains_stage_for_late_open_descriptor_writes() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
        let destination = fixture.0.join("song_PN.wav"); fs::write(&destination, b"old output").unwrap();
        let alternate = fixture.0.join("alternate"); fs::write(&alternate, b"first writer").unwrap();
        let files = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        validate_batch(&[&plan], &files).unwrap();
        let mut transaction = plan.begin(&FileIdentity::capture(plan.source()).unwrap()).unwrap();
        fs::write(transaction.staging_path(), b"candidate").unwrap();
        let target = plan.destination().to_owned();
        let first = target.clone();
        transaction.after_overwrite_check = Some(Box::new(move || {
            fs::remove_file(&first).unwrap(); fs::rename(&alternate, &first).unwrap();
        }));
        let opened = Arc::new(Mutex::new(None::<File>)); let opened_for_hook = Arc::clone(&opened);
        let second = target.clone();
        transaction.before_overwrite_rollback = Some(Box::new(move || {
            *opened_for_hook.lock().unwrap() = Some(OpenOptions::new().write(true).open(&second).unwrap());
        }));
        let error = transaction.finish(&Cancellation::new()).unwrap_err();
        let recovery = match error {
            WorkflowError::Transaction { cause, rollback: None, recovery: Some(path), published: false, .. }
                if matches!(*cause, WorkflowError::DestinationChanged(_)) => path,
            other => panic!("unexpected error: {other}"),
        };
        assert_eq!(fs::read(&recovery).unwrap(), b"candidate");
        let mut writer = opened.lock().unwrap().take().unwrap();
        writer.write_all(b"late edit").unwrap(); writer.sync_all().unwrap();
        assert_eq!(fs::read(&recovery).unwrap(), b"late edit");
        assert_eq!(fs::read(&target).unwrap(), b"first writer");
        assert_eq!(fs::read(&source).unwrap(), b"source");
    }

    #[test]
    fn overwrite_retains_displaced_file_when_swap_rollback_is_unsafe() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
        let destination = fixture.0.join("song_PN.wav"); fs::write(&destination, b"old output").unwrap();
        let files = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        validate_batch(&[&plan], &files).unwrap();
        let mut transaction = plan.begin(&FileIdentity::capture(plan.source()).unwrap()).unwrap();
        fs::write(transaction.staging_path(), b"candidate").unwrap();
        let target = plan.destination().to_owned(); let changed = target.clone();
        transaction.after_overwrite_swap = Some(Box::new(move || {
            fs::remove_file(&changed).unwrap(); fs::write(&changed, b"third-party output").unwrap();
        }));
        let error = transaction.finish(&Cancellation::new()).unwrap_err();
        let recovery = match error {
            WorkflowError::Transaction { cause, rollback: Some(_), recovery: Some(path), published: true, .. }
                if matches!(*cause, WorkflowError::DestinationChanged(_)) => path,
            other => panic!("unexpected error: {other}"),
        };
        assert_eq!(fs::read(recovery).unwrap(), b"old output");
        assert_eq!(fs::read(target).unwrap(), b"third-party output");
        assert_eq!(fs::read(source).unwrap(), b"source");
    }

    #[test]
    fn overwrite_rollback_retains_an_unexpected_second_writer() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
        let destination = fixture.0.join("song_PN.wav"); fs::write(&destination, b"old output").unwrap();
        let alternate = fixture.0.join("alternate"); fs::write(&alternate, b"first writer").unwrap();
        let files = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        validate_batch(&[&plan], &files).unwrap();
        let mut transaction = plan.begin(&FileIdentity::capture(plan.source()).unwrap()).unwrap();
        fs::write(transaction.staging_path(), b"candidate").unwrap();
        let target = plan.destination().to_owned();
        let first = target.clone();
        transaction.after_overwrite_check = Some(Box::new(move || {
            fs::remove_file(&first).unwrap(); fs::rename(&alternate, &first).unwrap();
        }));
        let second = target.clone();
        transaction.before_overwrite_rollback = Some(Box::new(move || {
            fs::remove_file(&second).unwrap(); fs::write(&second, b"second writer").unwrap();
        }));
        let error = transaction.finish(&Cancellation::new()).unwrap_err();
        let recovery = match error {
            WorkflowError::Transaction { rollback: Some(_), recovery: Some(path), published: true, .. } => path,
            other => panic!("unexpected error: {other}"),
        };
        assert_eq!(fs::read(&recovery).unwrap(), b"second writer");
        assert_eq!(fs::read(&target).unwrap(), b"first writer");
        assert_eq!(fs::read(&source).unwrap(), b"source");
    }

    fn write_with_original_mtime(path: &Path, bytes: &[u8]) {
        let modified = fs::metadata(path).unwrap().modified().unwrap();
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
    }

    #[test]
    fn overwrite_detects_in_place_displaced_content_with_restored_mtime() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
        let destination = fixture.0.join("song_PN.wav"); fs::write(&destination, b"old output").unwrap();
        let files = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        validate_batch(&[&plan], &files).unwrap();
        let mut transaction = plan.begin(&FileIdentity::capture(plan.source()).unwrap()).unwrap();
        fs::write(transaction.staging_path(), b"candidate").unwrap();
        let target = plan.destination().to_owned(); let changed = target.clone();
        transaction.after_overwrite_check = Some(Box::new(move || write_with_original_mtime(&changed, b"new output")));
        let error = transaction.finish(&Cancellation::new()).unwrap_err();
        assert!(matches!(error, WorkflowError::Transaction { cause, published: false, rollback: None, .. }
            if matches!(*cause, WorkflowError::DestinationChanged(_))));
        assert_eq!(fs::read(target).unwrap(), b"new output");
        assert_eq!(fs::read(source).unwrap(), b"source");
    }

    #[test]
    fn overwrite_rollback_retains_in_place_content_changes_with_restored_mtime() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
        let destination = fixture.0.join("song_PN.wav"); fs::write(&destination, b"old output").unwrap();
        let alternate = fixture.0.join("alternate"); fs::write(&alternate, b"first writer").unwrap();
        let files = discover_inputs(&[source.clone()]).unwrap();
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
        let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
        plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
        validate_batch(&[&plan], &files).unwrap();
        let mut transaction = plan.begin(&FileIdentity::capture(plan.source()).unwrap()).unwrap();
        fs::write(transaction.staging_path(), b"candidate").unwrap();
        let target = plan.destination().to_owned(); let first = target.clone();
        transaction.after_overwrite_check = Some(Box::new(move || {
            fs::remove_file(&first).unwrap(); fs::rename(&alternate, &first).unwrap();
        }));
        let second = target.clone();
        transaction.before_overwrite_rollback = Some(Box::new(move || write_with_original_mtime(&second, b"tampered!")));
        let error = transaction.finish(&Cancellation::new()).unwrap_err();
        let recovery = match error {
            WorkflowError::Transaction { rollback: Some(_), recovery: Some(path), published: true, .. } => path,
            other => panic!("unexpected error: {other}"),
        };
        assert_eq!(fs::read(recovery).unwrap(), b"tampered!");
        assert_eq!(fs::read(target).unwrap(), b"first writer");
        assert_eq!(fs::read(source).unwrap(), b"source");
    }

    #[test]
    fn restore_snapshot_rejects_one_backup_path_changed_between_observations() {
        let fixture = Fixture::new();
        let first = fixture.0.join("first.wav"); fs::write(&first, b"first").unwrap();
        let second = fixture.0.join("second.wav"); fs::write(&second, b"second").unwrap();
        let backup = fixture.0.join("backup.wav"); fs::write(&backup, b"old backup").unwrap();
        let replacement = fixture.0.join("replacement"); fs::write(&replacement, b"new backup").unwrap();
        let changed = backup.clone();
        let result = RestoreBatchSnapshot::capture_impl(&[first.clone(), second.clone()], &[backup.clone(), backup.clone()],
            Some(Box::new(move |index| { if index == 0 { fs::rename(&replacement, &changed).unwrap(); } })));
        assert!(matches!(result, Err(WorkflowError::BackupChanged(_))));
        assert_eq!(fs::read(first).unwrap(), b"first"); assert_eq!(fs::read(second).unwrap(), b"second");
        assert_eq!(fs::read(backup).unwrap(), b"new backup");
    }

    #[test]
    fn restore_group_rolls_back_all_members_when_frozen_backup_changes_midflight() {
        use crate::metadata::ProcessingMarker;
        for before_final in [false, true] {
            let fixture = Fixture::new();
            let first = fixture.0.join("first.wav"); fs::write(&first, b"processed first").unwrap();
            let second = fixture.0.join("second.wav"); fs::write(&second, b"processed second").unwrap();
            let first_inode = fs::metadata(&first).unwrap().ino();
            let second_inode = fs::metadata(&second).unwrap().ino();
            let backup = fixture.0.join("backup.wav"); fs::write(&backup, b"original").unwrap();
            let marker = Marker::from_processing(ProcessingMarker {
                software_version: None, backup_file_name: Some(backup.to_str().unwrap()),
                applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
                expansion_applied: 0, compression_applied: 0, volume_analysis_version: 4,
                target_volume: -12.0, input_volume: -14.0, output_volume: -12.0,
                clip_repair_count: 0, exciter_option: 0, input_pitch: 0.0, output_pitch: 0.0,
                speed_adjustment: 1.0,
            });
            let snapshot = RestoreBatchSnapshot::capture(&[first.clone(), second.clone()], &[backup.clone(), backup.clone()]).unwrap();
            let requests = [
                RestoreRequest { processed: &first, backup: &backup, marker: Some(&marker), status: RestoreStatus::Inactive,
                    identity: snapshot.target(0), frozen_backup: snapshot.backup(0) },
                RestoreRequest { processed: &second, backup: &backup, marker: Some(&marker), status: RestoreStatus::Inactive,
                    identity: snapshot.target(1), frozen_backup: snapshot.backup(1) },
            ];
            let changed = backup.clone();
            let member_hook: Option<Box<dyn FnMut(usize) + Send>> = if before_final { None } else {
                Some(Box::new(move |index| { if index == 1 { write_with_original_mtime(&changed, b"tampered"); } }))
            };
            let changed = backup.clone();
            let final_hook: Option<Box<dyn FnOnce() + Send>> = if before_final {
                Some(Box::new(move || write_with_original_mtime(&changed, b"tampered")))
            } else { None };
            let results = restore_group_impl(&requests, true, &Cancellation::new(), member_hook, final_hook, None, None);
            assert!(results.into_iter().all(|result| matches!(result, Err(WorkflowError::BackupChanged(_)))));
            assert_eq!(fs::read(&first).unwrap(), b"processed first");
            assert_eq!(fs::read(&second).unwrap(), b"processed second");
            assert_eq!(fs::metadata(&first).unwrap().ino(), first_inode);
            assert_eq!(fs::metadata(&second).unwrap().ino(), second_inode);
            assert_eq!(fs::read(&backup).unwrap(), b"tampered");
        }
    }

    #[test]
    fn restore_rollback_preserves_recovery_when_backup_reservation_release_fails() {
        use crate::metadata::ProcessingMarker;
        for before_verification in [true, false] {
            let fixture = Fixture::new();
            let current = fixture.0.join("processed.wav"); fs::write(&current, b"processed").unwrap();
            let backup = fixture.0.join("backup.wav"); fs::write(&backup, b"original").unwrap();
            let marker = Marker::from_processing(ProcessingMarker {
                software_version: None, backup_file_name: Some(backup.to_str().unwrap()),
                applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
                expansion_applied: 0, compression_applied: 0, volume_analysis_version: 4,
                target_volume: -12.0, input_volume: -14.0, output_volume: -12.0,
                clip_repair_count: 0, exciter_option: 0, input_pitch: 0.0, output_pitch: 0.0,
                speed_adjustment: 1.0,
            });
            let snapshot = RestoreBatchSnapshot::capture(&[current.clone()], &[backup.clone()]).unwrap();
            let requests = [RestoreRequest { processed: &current, backup: &backup, marker: Some(&marker),
                status: RestoreStatus::Inactive, identity: snapshot.target(0), frozen_backup: snapshot.backup(0) }];
            let changed_backup = backup.clone(); let changed_current = current.clone();
            let hook: Box<dyn FnOnce(&[Reservation]) + Send> = Box::new(move |reservations| {
                write_with_original_mtime(&changed_backup, b"tampered");
                fs::write(&changed_current, b"intervening writer").unwrap();
                let old_reservation = File::open(&reservations[0].path).unwrap();
                fs::remove_file(&reservations[0].path).unwrap();
                fs::write(&reservations[0].path, b"replacement reservation").unwrap();
                drop(old_reservation);
            });
            let (after_reservations, before_removal) = if before_verification { (Some(hook), None) } else { (None, Some(hook)) };
            let mut results = restore_group_impl(&requests, true, &Cancellation::new(), None, None, after_reservations, before_removal);
            let error = results.remove(0).unwrap_err();
            let recovery = match error {
                WorkflowError::Transaction { recovery: Some(path), published: true, cleanup, .. } => {
                    assert!(cleanup.iter().any(|failure| failure.operation == "release reservation")); path
                }
                other => panic!("recovery or cleanup was lost: {other}"),
            };
            assert_eq!(fs::read(&recovery).unwrap(), b"processed");
            assert_eq!(fs::read(&current).unwrap(), b"intervening writer");
            assert_eq!(fs::read(&backup).unwrap(), b"tampered");
        }
    }

    #[test]
    fn frozen_backup_digest_rejects_same_stamp_with_changed_bytes() {
        let fixture = Fixture::new();
        let backup = fixture.0.join("backup.wav"); fs::write(&backup, b"original").unwrap();
        let digest = FrozenBackup::capture(&backup).unwrap().digest;
        write_with_original_mtime(&backup, b"tampered");
        let mut frozen = FrozenBackup::capture(&backup).unwrap();
        frozen.digest = digest;
        assert!(matches!(frozen.verify(&backup), Err(WorkflowError::BackupChanged(_))));
        assert_eq!(fs::read(backup).unwrap(), b"tampered");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn overwrite_rejects_metadata_changes_after_the_pin() {
        for change in 0..2 {
            let fixture = Fixture::new();
            let source = fixture.0.join("song.wav"); fs::write(&source, b"source").unwrap();
            let destination = fixture.0.join("song_PN.wav"); fs::write(&destination, b"old output").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
            let name = CString::new("com.example.overwrite-test").unwrap();
            let file = OpenOptions::new().read(true).write(true).open(&destination).unwrap();
            set_xattr(&file, &name, b"before");
            let files = discover_inputs(&[source.clone()]).unwrap();
            let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
            let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
            plan.enable_overwrite(Arc::new(OverwriteGuard::new(&files, &[plan.destination().to_owned()]).unwrap())).unwrap();
            validate_batch(&[&plan], &files).unwrap();
            let mut transaction = plan.begin(&FileIdentity::capture(plan.source()).unwrap()).unwrap();
            fs::write(transaction.staging_path(), b"candidate").unwrap();
            let target = plan.destination().to_owned(); let changed = target.clone();
            transaction.after_overwrite_check = Some(Box::new(move || {
                if change == 0 { fs::set_permissions(&changed, fs::Permissions::from_mode(0o600)).unwrap(); }
                else {
                    let file = OpenOptions::new().read(true).write(true).open(&changed).unwrap();
                    set_xattr(&file, &name, b"after!");
                }
            }));
            let error = transaction.finish(&Cancellation::new()).unwrap_err();
            assert!(matches!(error, WorkflowError::Transaction { cause, published: false, .. }
                if matches!(*cause, WorkflowError::DestinationChanged(_))));
            assert_eq!(fs::read(&target).unwrap(), b"old output");
            if change == 0 { assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600); }
            else {
                let file = open_read(&target).unwrap();
                let mut value = [0u8; 6];
                read_xattr(&file, &target, &CString::new("com.example.overwrite-test").unwrap(), &mut value, 0).unwrap();
                assert_eq!(&value, b"after!");
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn destination_comparison_respects_parent_identity_and_missing_components() {
        let parent = |dev, ino, missing: Vec<OsString>| DestinationParent {
            dev, ino, missing, ancestor: "/nonexistent-policy-probe".into(),
        };
        let left = parent(1, 11, vec![]);
        let other_mount = parent(2, 11, vec![]);
        // Different directory identities must not probe even an unsupported parent.
        assert!(!same_destination(Path::new("/mnt/A/Song_PN.wav"), &left,
            Path::new("/mnt/a/song_PN.wav"), &other_mount).unwrap());
        let same = parent(1, 11, vec![]);
        let insensitive = policy_for_fs_type(b"apfs", false);
        let sensitive = policy_for_fs_type(b"apfs", true);
        assert!(same_destination_with_policy(OsStr::new("Song_PN.wav"), &left,
            OsStr::new("song_PN.wav"), &same, insensitive).unwrap());
        let missing_a = parent(1, 11, vec!["Album".into()]);
        let missing_b = parent(1, 11, vec!["album".into()]);
        assert!(same_destination_with_policy(OsStr::new("Song.wav"), &missing_a,
            OsStr::new("song.wav"), &missing_b, insensitive).unwrap());
        assert!(same_destination_with_policy(OsStr::new("café_PN.wav"), &left,
            OsStr::new("cafe\u{301}_PN.wav"), &same, sensitive).unwrap());
        assert!(!same_destination_with_policy(OsStr::new("Café_PN.wav"), &left,
            OsStr::new("cafe\u{301}_PN.wav"), &same, sensitive).unwrap());
        let nfc_parent = parent(1, 11, vec!["café".into()]);
        let nfd_parent = parent(1, 11, vec!["cafe\u{301}".into()]);
        assert!(same_destination_with_policy(OsStr::new("song.wav"), &nfc_parent,
            OsStr::new("song.wav"), &nfd_parent, sensitive).unwrap());
    }

    #[test]
    fn quarantine_rolls_back_an_entry_swapped_after_backup_verification() {
        use std::os::unix::fs::symlink;
        for is_symlink in [false, true] {
            let fixture = Fixture::new();
            let backup = fixture.0.join("backup.wav"); fs::write(&backup, b"verified backup").unwrap();
            let expected = stamp(&backup, "capture backup").unwrap();
            let saved = fixture.0.join("original-held");
            let target = fixture.0.join("target"); fs::write(&target, b"target").unwrap();
            let raced_backup = backup.clone(); let held = saved.clone(); let link_target = target.clone();
            let (removed, error) = remove_verified_backup(&target, &backup, expected, Some(Box::new(move || {
                fs::rename(&raced_backup, &held).unwrap();
                if is_symlink { symlink(&link_target, &raced_backup).unwrap(); }
                else { fs::write(&raced_backup, b"racing file").unwrap(); }
            })));
            assert!(!removed);
            let error = error.unwrap();
            assert_eq!(error.operation, "verify quarantined backup");
            assert!(error.retained_path.is_none());
            assert_eq!(error.unexpected_path.as_deref(), Some(backup.as_path()));
            assert_eq!(fs::read(&saved).unwrap(), b"verified backup");
            assert_eq!(fs::read(&target).unwrap(), b"target");
            if is_symlink { assert!(fs::symlink_metadata(&backup).unwrap().file_type().is_symlink()); }
            else { assert_eq!(fs::read(&backup).unwrap(), b"racing file"); }
        }
    }

    #[test]
    fn restore_retains_backup_when_durable_flush_fails() {
        use crate::metadata::ProcessingMarker;
        let fixture = Fixture::new();
        let current = fixture.0.join("processed.wav"); fs::write(&current, b"processed").unwrap();
        let backup = fixture.0.join("backup.wav"); fs::write(&backup, b"original").unwrap();
        let marker = Marker::from_processing(ProcessingMarker {
            software_version: None, backup_file_name: Some(backup.to_str().unwrap()),
            applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
            expansion_applied: 0, compression_applied: 0, volume_analysis_version: 4,
            target_volume: -12.0, input_volume: -14.0, output_volume: -12.0,
            clip_repair_count: 0, exciter_option: 0, input_pitch: 0.0, output_pitch: 0.0,
            speed_adjustment: 1.0,
        });
        let identity = FileIdentity::capture(&current).unwrap();
        let (report, held) = restore_impl(&current, &backup, Some(&marker), RestoreStatus::Inactive, &identity, &Cancellation::new(), true, None, false,
            Some(Box::new(|| Err(backup_removal_error("sync restored file", "injected full sync failure", None))))).unwrap();
        assert!(held.is_none());
        assert!(!report.backup_removed);
        assert_eq!(report.backup_removal_error.unwrap().operation, "sync restored file");
        assert_eq!(fs::read(&current).unwrap(), b"original");
        assert_eq!(fs::read(&backup).unwrap(), b"original");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn name_comparison_uses_filesystem_policy_and_handles_invalid_bytes() {
        let apfs_sensitive = policy_for_fs_type(b"apfs", true);
        let apfs_insensitive = policy_for_fs_type(b"apfs", false);
        let hfs_sensitive = policy_for_fs_type(b"hfs", true);
        let hfs_insensitive = policy_for_fs_type(b"hfsx", false);
        let byte_sensitive = policy_for_fs_type(b"nfs", true);
        let byte_insensitive = policy_for_fs_type(b"smbfs", false);
        assert!(names_equal(OsStr::new("café"), OsStr::new("cafe\u{301}"), apfs_sensitive).unwrap());
        assert!(names_equal(OsStr::new("Café"), OsStr::new("cafe\u{301}"), apfs_insensitive).unwrap());
        assert!(names_equal(OsStr::new("café"), OsStr::new("cafe\u{301}"), hfs_sensitive).unwrap());
        assert!(names_equal(OsStr::new("Café"), OsStr::new("cafe\u{301}"), hfs_insensitive).unwrap());
        for (excluded, decomposed) in [("Å", "A\u{30a}"), ("\u{f900}", "\u{8c48}"), ("\u{2f800}", "\u{4e3d}")] {
            assert!(!names_equal(OsStr::new(excluded), OsStr::new(decomposed), hfs_sensitive).unwrap());
            assert!(!names_equal(OsStr::new(excluded), OsStr::new(decomposed), hfs_insensitive).unwrap());
        }
        assert!(!names_equal(OsStr::new("café"), OsStr::new("cafe\u{301}"), byte_sensitive).unwrap());
        assert!(!names_equal(OsStr::new("café"), OsStr::new("cafe\u{301}"), byte_insensitive).unwrap());
        assert!(names_equal(OsStr::new("Café"), OsStr::new("café"), byte_insensitive).unwrap());
        let invalid_a = OsStr::from_bytes(b"bad\xffA.wav");
        let invalid_b = OsStr::from_bytes(b"bad\xfeA.wav");
        assert!(!names_equal(invalid_a, invalid_b, apfs_sensitive).unwrap());
        assert!(!names_equal(invalid_a, invalid_b, byte_insensitive).unwrap());
        assert!(names_equal(invalid_a, OsStr::from_bytes(b"bad\xffa.wav"), byte_insensitive).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_byte_policy_casefolds_without_unicode_normalization() {
        let sensitive = NamePolicy { normalization: NameNormalization::Bytes, case_sensitive: true };
        let folded = NamePolicy { normalization: NameNormalization::Bytes, case_sensitive: false };
        assert!(!names_equal(OsStr::new("Song"), OsStr::new("song"), sensitive).unwrap());
        assert!(names_equal(OsStr::new("Café"), OsStr::new("café"), folded).unwrap());
        assert!(!names_equal(OsStr::new("café"), OsStr::new("cafe\u{301}"), folded).unwrap());
        assert!(!names_equal(OsStr::from_bytes(b"bad\xffA"), OsStr::from_bytes(b"bad\xfeA"), folded).unwrap());
        assert!(names_equal(OsStr::from_bytes(b"bad\xffA"), OsStr::from_bytes(b"bad\xffa"), folded).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_copy_preserves_posix_metadata_and_extended_attributes() {
        let fixture = Fixture::new();
        let source_path = fixture.0.join("source"); let dest_path = fixture.0.join("dest");
        fs::write(&source_path, b"original data").unwrap();
        fs::set_permissions(&source_path, fs::Permissions::from_mode(0o640)).unwrap();
        let source = OpenOptions::new().read(true).write(true).open(&source_path).unwrap();
        let name = CString::new("user.platinum-test").unwrap();
        super::set_xattr(&source, &source_path, &name, b"preserved", 0).unwrap();
        let acl_name = CString::new("system.posix_acl_access").unwrap();
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (0x01u16, 0x06u16, u32::MAX), (0x02, 0x04, unsafe { libc::geteuid() }.wrapping_add(1)),
            (0x04, 0x04, u32::MAX), (0x10, 0x04, u32::MAX), (0x20, 0x00, u32::MAX),
        ] {
            acl.extend_from_slice(&tag.to_le_bytes()); acl.extend_from_slice(&permissions.to_le_bytes());
            acl.extend_from_slice(&id.to_le_bytes());
        }
        let acl_status = unsafe { libc::fsetxattr(source.as_raw_fd(), acl_name.as_ptr(), acl.as_ptr().cast(), acl.len(), 0) };
        if acl_status != 0 {
            let error = io::Error::last_os_error();
            assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP), "set POSIX ACL: {error}");
        }
        let expected = stamp(&source_path, "capture source").unwrap();
        copy_new_verified(&source_path, &dest_path, expected, None).unwrap();
        let dest = open_read(&dest_path).unwrap();
        assert!(metadata_equal(&source, &dest, &source_path, &dest_path, false).unwrap());
        assert_eq!(fs::read(&dest_path).unwrap(), b"original data");
        let mut value = [0u8; 9]; read_xattr(&dest, &dest_path, &name, &mut value, 0).unwrap();
        assert_eq!(&value, b"preserved");
        if acl_status == 0 {
            assert_eq!(xattr_size(&dest, &dest_path, &acl_name).unwrap(), acl.len());
            let mut copied_acl = vec![0u8; acl.len()];
            read_xattr(&dest, &dest_path, &acl_name, &mut copied_acl, 0).unwrap();
            assert_eq!(copied_acl, acl);
        }
    }

    #[test]
    fn authorized_ambiguous_marker_still_rejects_suffix_and_destination_collision() {
        let fixture = Fixture::new();
        let suffixed = fixture.0.join("already_PN.wav");
        fs::write(&suffixed, b"source").unwrap();
        let selected = discover_inputs(&[suffixed.clone()]).unwrap().remove(0);
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
        assert!(matches!(OutputPlan::for_authorized_ambiguous_marker(&selected, SourceCodec::Lpcm, &settings),
            Err(WorkflowError::AlreadyProcessed { state: ProcessingState::Suffixed, .. })));

        let source = fixture.0.join("song.wav"); fs::write(&source, b"original").unwrap();
        let destination = fixture.0.join("song_PN.wav"); fs::write(&destination, b"existing").unwrap();
        let selected = discover_inputs(&[source.clone()]).unwrap().remove(0);
        let plan = OutputPlan::for_authorized_ambiguous_marker(&selected, SourceCodec::Lpcm, &settings).unwrap();
        assert!(matches!(plan.begin(&FileIdentity::capture(&source).unwrap()), Err(WorkflowError::DestinationExists(_))));
        assert_eq!(fs::read(source).unwrap(), b"original");
        assert_eq!(fs::read(destination).unwrap(), b"existing");
    }

    #[test]
    fn destination_collision_after_move_rolls_original_back() {
        let fixture = Fixture::new();
        let (source, backup, plan, transaction) = fixture.setup();
        let error = transaction.finish(&Cancellation::new()).unwrap_err();
        match error {
            WorkflowError::Transaction { cause, rollback, recovery, published, .. } => {
                assert!(matches!(*cause, WorkflowError::DestinationExists(_)));
                assert!(rollback.is_none()); assert!(recovery.is_none()); assert!(!published);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(fs::read(source).unwrap(), b"original");
        assert_eq!(fs::read(backup).unwrap(), b"original");
        assert_eq!(fs::read(plan.destination()).unwrap(), b"late destination occupant");
    }

    #[test]
    fn rollback_failure_retains_original_and_reports_the_recovery_path() {
        let fixture = Fixture::new();
        let (source, backup, plan, mut transaction) = fixture.setup();
        transaction.after_move = Some(Box::new(|path| {
            let mut file = OpenOptions::new().write(true).create_new(true).open(path).unwrap();
            file.write_all(b"late source occupant").unwrap();
        }));
        let error = transaction.finish(&Cancellation::new()).unwrap_err();
        let recovery = match error {
            WorkflowError::Transaction { cause, rollback: Some(_), recovery: Some(path), published: false, .. } => {
                assert!(matches!(*cause, WorkflowError::DestinationExists(_))); path
            }
            other => panic!("unexpected error: {other}"),
        };
        assert_eq!(fs::read(recovery).unwrap(), b"original");
        assert_eq!(fs::read(backup).unwrap(), b"original");
        assert_eq!(fs::read(source).unwrap(), b"late source occupant");
        assert_eq!(fs::read(plan.destination()).unwrap(), b"late destination occupant");
    }

    #[test]
    fn failed_restore_verification_rolls_the_processed_file_back() {
        let fixture = Fixture::new();
        let current = fixture.0.join("processed.wav");
        let backup = fixture.0.join("backup.wav");
        fs::write(&current, b"processed").unwrap();
        fs::write(&backup, b"original").unwrap();
        let current_stamp = stamp(&current, "inspect test current").unwrap();
        let backup_stamp = stamp(&backup, "inspect test backup").unwrap();
        let workspace = Workspace::create(&fixture.0, "restore").unwrap();
        copy_new_verified(&backup, &workspace.staged, backup_stamp, Some(&workspace)).unwrap();
        let staged_stamp = stamp(&workspace.staged, "inspect test restore copy").unwrap();
        let stage = workspace.staged.clone();
        let error = commit_replacing(&workspace, &current, &current, &workspace.staged, &workspace.displaced(),
            current_stamp, staged_stamp, None, Some((&backup, backup_stamp)), false,
            Some(Box::new(move |_| fs::remove_file(stage).unwrap()))).err().unwrap();
        assert!(error.rollback.is_none()); assert!(error.recovery.is_none()); assert!(!error.published);
        assert_eq!(fs::read(&current).unwrap(), b"processed");
        assert_eq!(fs::read(&backup).unwrap(), b"original");
        assert!(workspace.cleanup(false).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn backup_and_restore_preserve_resource_fork_xattrs_acl_and_stat() {
        use crate::metadata::ProcessingMarker;
        let fixture = Fixture::new();
        let source = fixture.0.join("metadata.wav");
        let backup_dir = fixture.0.join("backups");
        fs::create_dir(&backup_dir).unwrap();
        fs::write(&source, b"original data fork").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        let original = OpenOptions::new().read(true).write(true).open(&source).unwrap();
        let custom = CString::new("com.example.platinum-test").unwrap();
        let resource = CString::new("com.apple.ResourceFork").unwrap();
        let fork = vec![0x5a; 131_073];
        set_xattr(&original, &custom, b"extended attribute");
        set_resource_fork(&original, &resource, &fork);
        add_test_acl(&original);
        // SAFETY: UF_NODUMP is an owner-settable flag in sys/stat.h.
        assert_eq!(unsafe { fchflags(original.as_raw_fd(), 0x00000001) }, 0);
        let mut creation = [0u8; 16];
        creation[..8].copy_from_slice(&1_700_000_000i64.to_ne_bytes());
        creation[8..].copy_from_slice(&123_456_789i64.to_ne_bytes());
        let mut attrs = AttrList::common(ATTR_CMN_CRTIME);
        // SAFETY: the buffer is a Darwin 64-bit timespec for ATTR_CMN_CRTIME.
        assert_eq!(unsafe { fsetattrlist(original.as_raw_fd(), &mut attrs, creation.as_ptr().cast(), creation.len(), 0) }, 0);
        assert_eq!(creation_time(&original, &source).unwrap(), Some(creation));
        assert_eq!(file_flags(&original, &source).unwrap().unwrap() & 1, 1);
        assert!(acl_bytes(&original, &source).unwrap().is_some());

        let selected = discover_inputs(&[source.clone()]).unwrap().remove(0);
        let settings = OutputSettings { mode: OutputMode::Replace { backup_directory: backup_dir }, format: AudioFormat::Wav };
        let plan = OutputPlan::new(&selected, SourceCodec::Lpcm, &settings, None).unwrap();
        let transaction = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        let backup = transaction.backup_path().unwrap().to_owned();
        let backup_file = open_read(&backup).unwrap();
        assert!(metadata_equal(&original, &backup_file, &source, &backup, false).unwrap());
        let mut got_fork = vec![0u8; fork.len()];
        read_resource_fork(&backup_file, &backup, &resource, &mut got_fork);
        assert_eq!(got_fork, fork);
        let mut got_custom = vec![0u8; b"extended attribute".len()];
        read_xattr(&backup_file, &backup, &custom, &mut got_custom, 0).unwrap();
        assert_eq!(got_custom, b"extended attribute");
        fs::write(transaction.staging_path(), b"encoded data fork").unwrap();
        transaction.finish(&Cancellation::new()).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"encoded data fork");

        let marker = Marker::from_processing(ProcessingMarker {
            software_version: None, backup_file_name: Some(backup.to_str().unwrap()),
            applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
            expansion_applied: 0, compression_applied: 0, volume_analysis_version: 4,
            target_volume: -12.0, input_volume: -14.0, output_volume: -12.0,
            clip_repair_count: 0, exciter_option: 0, input_pitch: 0.0, output_pitch: 0.0,
            speed_adjustment: 1.0,
        });
        let identity = FileIdentity::capture(&source).unwrap();
        let report = restore(&source, &backup, Some(&marker), RestoreStatus::Inactive, &identity, &Cancellation::new()).unwrap();
        assert_eq!(fs::read(&report.destination).unwrap(), b"original data fork");
        assert!(report.backup_removed, "{report:?}"); assert!(report.backup_removal_error.is_none()); assert!(!report.backup.exists());
        let restored = open_read(&report.destination).unwrap();
        assert!(metadata_equal(&original, &restored, &source, &report.destination, false).unwrap());
        assert_eq!(acl_bytes(&original, &source).unwrap(), acl_bytes(&restored, &report.destination).unwrap());
        assert_eq!(creation_time(&restored, &report.destination).unwrap(), Some(creation));
        assert_eq!(file_flags(&restored, &report.destination).unwrap().unwrap() & 1, 1);
        read_resource_fork(&restored, &report.destination, &resource, &mut got_fork);
        assert_eq!(got_fork, fork);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn changed_backup_metadata_is_rejected_even_with_a_fresh_backup_stamp() {
        let fixture = Fixture::new();
        let source = fixture.0.join("original.wav");
        let backup_dir = fixture.0.join("backups");
        fs::create_dir(&backup_dir).unwrap();
        fs::write(&source, b"same data fork").unwrap();
        let name = CString::new("com.example.platinum-test").unwrap();
        let original = OpenOptions::new().read(true).write(true).open(&source).unwrap();
        set_xattr(&original, &name, b"original metadata");
        let selected = discover_inputs(&[source.clone()]).unwrap().remove(0);
        let settings = OutputSettings { mode: OutputMode::Replace { backup_directory: backup_dir }, format: AudioFormat::Wav };
        let plan = OutputPlan::new(&selected, SourceCodec::Lpcm, &settings, None).unwrap();
        let transaction = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        let backup = transaction.backup_path().unwrap().to_owned();
        let backup_file = OpenOptions::new().read(true).write(true).open(&backup).unwrap();
        set_xattr(&backup_file, &name, b"tampered metadata");
        let updated_stamp = stamp(&backup, "inspect tampered test backup").unwrap();
        assert!(matches!(verify_backup(&source, &backup, updated_stamp), Err(WorkflowError::BackupChanged(_))));
        fs::write(transaction.staging_path(), b"encoded").unwrap();
        let error = transaction.finish(&Cancellation::new()).unwrap_err();
        assert!(matches!(error, WorkflowError::Transaction { published: false, .. }));
        assert_eq!(fs::read(&source).unwrap(), b"same data fork");
    }

    #[test]
    fn nested_backup_cleanup_failure_reports_the_retained_backup() {
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav");
        let backup_dir = fixture.0.join("backups");
        fs::create_dir(&backup_dir).unwrap();
        fs::write(&source, b"original").unwrap();
        let selected = discover_inputs(&[source.clone()]).unwrap().remove(0);
        let settings = OutputSettings { mode: OutputMode::SameFolder, format: AudioFormat::Wav };
        let plan = OutputPlan::new(&selected, SourceCodec::Lpcm, &settings, None).unwrap();
        let identity = FileIdentity::capture(&source).unwrap();
        let transaction = plan.begin(&identity).unwrap();
        let inner = create_backup(&source, &identity.stamp, &backup_dir, Some(Box::new(|work| {
            fs::write(work.directory.join("cleanup-blocker"), b"obstruction").unwrap();
        }))).unwrap_err();
        let error = transaction.fail(inner, None, None, false);
        match error {
            WorkflowError::Transaction { backup: Some(backup), cause, published: false, .. } => {
                assert_eq!(backup, backup_dir.join("song.wav"));
                assert_eq!(fs::read(&backup).unwrap(), b"original");
                assert!(matches!(*cause, WorkflowError::Transaction { backup: Some(_), .. }));
            }
            other => panic!("missing retained backup in outer error: {other}"),
        }
        assert_eq!(fs::read(source).unwrap(), b"original");
    }

    #[test]
    fn cancellation_after_verification_but_before_commit_prevents_publication() {
        use std::sync::{Arc, mpsc};
        use std::thread;
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav");
        let backup_dir = fixture.0.join("backups");
        fs::create_dir(&backup_dir).unwrap();
        fs::write(&source, b"original").unwrap();
        let selected = discover_inputs(&[source.clone()]).unwrap().remove(0);
        let settings = OutputSettings { mode: OutputMode::Replace { backup_directory: backup_dir }, format: AudioFormat::Wav };
        let plan = OutputPlan::new(&selected, SourceCodec::Lpcm, &settings, None).unwrap();
        let mut transaction = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        let backup = transaction.backup_path().unwrap().to_owned();
        fs::write(transaction.staging_path(), b"encoded").unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        transaction.before_commit = Some(Box::new(move || {
            ready_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        }));
        let token = Arc::new(Cancellation::new());
        let finishing_token = token.clone();
        let finishing = thread::spawn(move || transaction.finish(&finishing_token));
        ready_rx.recv().unwrap();
        token.cancel();
        resume_tx.send(()).unwrap();
        let error = finishing.join().unwrap().unwrap_err();
        assert!(error.is_cancelled());
        assert!(matches!(&error, WorkflowError::Transaction { backup: Some(path), published: false, .. } if path == &backup));
        assert_eq!(fs::read(&source).unwrap(), b"original");
        assert_eq!(fs::read(&backup).unwrap(), b"original");
    }

    #[test]
    fn cancellation_waits_for_in_progress_publication() {
        use std::sync::{Arc, mpsc};
        use std::thread;
        let fixture = Fixture::new();
        let source = fixture.0.join("song.wav");
        let backup_dir = fixture.0.join("backups");
        fs::create_dir(&backup_dir).unwrap();
        fs::write(&source, b"original").unwrap();
        let selected = discover_inputs(&[source.clone()]).unwrap().remove(0);
        let settings = OutputSettings { mode: OutputMode::Replace { backup_directory: backup_dir }, format: AudioFormat::Wav };
        let plan = OutputPlan::new(&selected, SourceCodec::Lpcm, &settings, None).unwrap();
        let mut transaction = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
        fs::write(transaction.staging_path(), b"encoded").unwrap();
        let (moved_tx, moved_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        transaction.after_move = Some(Box::new(move |_| {
            moved_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        }));
        let token = Arc::new(Cancellation::new());
        let finishing_token = token.clone();
        let finishing = thread::spawn(move || transaction.finish(&finishing_token));
        moved_rx.recv().unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (done_tx, done_rx) = mpsc::channel();
        let cancelling_token = token.clone();
        let cancelling = thread::spawn(move || {
            started_tx.send(()).unwrap();
            cancelling_token.cancel();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(!token.is_cancelled());
        assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        resume_tx.send(()).unwrap();
        let publication = finishing.join().unwrap().unwrap();
        done_rx.recv().unwrap();
        cancelling.join().unwrap();
        assert!(token.is_cancelled());
        assert_eq!(fs::read(publication.destination).unwrap(), b"encoded");
        assert_eq!(fs::read(publication.backup.unwrap()).unwrap(), b"original");
    }
}
