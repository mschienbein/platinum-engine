//! Stable machine-readable failure codes for CLI results.
//!
//! Every failed item, batch error and top-level error in the JSON outcome
//! carries one `code` next to its human-readable `message`. The strings below
//! are a public contract (see `research/CLI_CONTRACT.md`): they are never
//! renamed or reused; new causes add new codes. The message and `stage` stay
//! diagnostic text and can change.
//!
//! A code comes from the typed error when its variant has one unambiguous
//! meaning (for example an Apple-only codec refusal) and otherwise from the
//! stage that failed. The stage table is exhaustive over the stage labels in
//! `engine.rs` (checked by a test), so no engine failure is left without a
//! specific code; `internal_error` is the final fallback for unknown labels.

use crate::file_workflow::WorkflowError;
use serde::{Serialize, Serializer};
use std::any::Any;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // Input admission and capability.
    UnsupportedInputCodec,
    UnsupportedInputFormat,
    UnsupportedOutputCodec,
    UnsupportedOutputFormat,
    UnsupportedSampleRateConversion,
    ArtworkConversionUnavailable,
    ProtectedSource,
    InvalidInputFile,
    InvalidInputPath,
    NoInputFiles,
    // Opt-in formats (settings `formats.*`).
    OpusNotEnabled,
    UnsupportedOpusVariant,
    // Processed-marker safety.
    AlreadyProcessed,
    MarkerAmbiguous,
    MarkerUnverifiable,
    MarkerInvalid,
    // Tags.
    TagCopyFailed,
    // Native libraries.
    LibraryNotConfigured,
    LibraryUnapproved,
    LibraryLoadFailed,
    // Codec and processing runtime.
    DecodeFailed,
    EncodeFailed,
    ProcessingFailed,
    // Files and transactions.
    IoError,
    AtomicRenameUnsupported,
    OutputExists,
    OutputConflict,
    SourceChanged,
    BackupMissing,
    BackupInvalid,
    RestoreNotApplicable,
    // Batch and command level.
    BatchPreflightFailed,
    ReportSerializationFailed,
    Cancelled,
    Usage,
    SettingsError,
    StatisticsError,
    SignalHandlerFailed,
    InternalError,
}

impl ErrorCode {
    /// Every code, in documentation order.
    pub const ALL: &'static [ErrorCode] = &[
        Self::UnsupportedInputCodec, Self::UnsupportedInputFormat, Self::UnsupportedOutputCodec, Self::UnsupportedOutputFormat,
        Self::UnsupportedSampleRateConversion, Self::ArtworkConversionUnavailable, Self::ProtectedSource,
        Self::InvalidInputFile, Self::InvalidInputPath, Self::NoInputFiles,
        Self::OpusNotEnabled, Self::UnsupportedOpusVariant,
        Self::AlreadyProcessed, Self::MarkerAmbiguous, Self::MarkerUnverifiable, Self::MarkerInvalid,
        Self::TagCopyFailed,
        Self::LibraryNotConfigured, Self::LibraryUnapproved, Self::LibraryLoadFailed,
        Self::DecodeFailed, Self::EncodeFailed, Self::ProcessingFailed,
        Self::IoError, Self::AtomicRenameUnsupported, Self::OutputExists, Self::OutputConflict,
        Self::SourceChanged, Self::BackupMissing, Self::BackupInvalid, Self::RestoreNotApplicable,
        Self::BatchPreflightFailed, Self::ReportSerializationFailed, Self::Cancelled, Self::Usage,
        Self::SettingsError, Self::StatisticsError, Self::SignalHandlerFailed, Self::InternalError,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedInputCodec => "unsupported_input_codec",
            Self::UnsupportedInputFormat => "unsupported_input_format",
            Self::UnsupportedOutputCodec => "unsupported_output_codec",
            Self::UnsupportedOutputFormat => "unsupported_output_format",
            Self::UnsupportedSampleRateConversion => "unsupported_sample_rate_conversion",
            Self::ArtworkConversionUnavailable => "artwork_conversion_unavailable",
            Self::ProtectedSource => "protected_source",
            Self::InvalidInputFile => "invalid_input_file",
            Self::InvalidInputPath => "invalid_input_path",
            Self::NoInputFiles => "no_input_files",
            Self::OpusNotEnabled => "opus_not_enabled",
            Self::UnsupportedOpusVariant => "unsupported_opus_variant",
            Self::AlreadyProcessed => "already_processed",
            Self::MarkerAmbiguous => "marker_ambiguous",
            Self::MarkerUnverifiable => "marker_unverifiable",
            Self::MarkerInvalid => "marker_invalid",
            Self::TagCopyFailed => "tag_copy_failed",
            Self::LibraryNotConfigured => "library_not_configured",
            Self::LibraryUnapproved => "library_unapproved",
            Self::LibraryLoadFailed => "library_load_failed",
            Self::DecodeFailed => "decode_failed",
            Self::EncodeFailed => "encode_failed",
            Self::ProcessingFailed => "processing_failed",
            Self::IoError => "io_error",
            Self::AtomicRenameUnsupported => "atomic_rename_unsupported",
            Self::OutputExists => "output_exists",
            Self::OutputConflict => "output_conflict",
            Self::SourceChanged => "source_changed",
            Self::BackupMissing => "backup_missing",
            Self::BackupInvalid => "backup_invalid",
            Self::RestoreNotApplicable => "restore_not_applicable",
            Self::BatchPreflightFailed => "batch_preflight_failed",
            Self::ReportSerializationFailed => "report_serialization_failed",
            Self::Cancelled => "cancelled",
            Self::Usage => "usage",
            Self::SettingsError => "settings_error",
            Self::StatisticsError => "statistics_error",
            Self::SignalHandlerFailed => "signal_handler_failed",
            Self::InternalError => "internal_error",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
}
impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> { serializer.serialize_str(self.as_str()) }
}

/// The code for an engine stage label when the error type itself does not
/// determine one. `None` means the label is unknown to this table.
pub fn stage_code(stage: &str) -> Option<ErrorCode> {
    use ErrorCode::*;
    Some(match stage {
        // Source selection, sniffing and marker policy.
        "input" => UnsupportedInputFormat,
        "inspect input" | "plan output" | "plan backup" | "backup" | "analyze original" => InvalidInputPath,
        "sniff source container" | "inspect source identity" | "analyze source identity" | "inspect original identity"
        | "analyze original identity" => IoError,
        "plan source identity" | "reserve source identity" | "verify source after analysis" | "verify source after healing"
        | "verify source after encoding" => SourceChanged,
        "inspect tags" => InvalidInputFile,
        "read marker" | "decode marker" | "inspect marker" | "analyze marker" => MarkerUnverifiable,
        "serialize marker" | "serialize processing marker" => InternalError,
        "verify processing marker" => MarkerInvalid,
        "tags" => LibraryLoadFailed,
        "copy source tags" => TagCopyFailed,
        // Decoding and analysis.
        "open input" | "open analysis input" | "open waveform input" | "open healing input" | "inspect length"
        | "inspect source MP3 header" | "analysis input" => InvalidInputFile,
        "close input" | "close analysis input" | "close waveform input" | "close healing input"
        | "read analysis input" | "read waveform input" | "read healing input" | "read intermediate"
        | "close intermediate input" => DecodeFailed,
        "configure analysis input" | "configure healing input" | "configure intermediate input" => ProcessingFailed,
        "analysis sample-rate conversion" | "output sample-rate conversion" => UnsupportedSampleRateConversion,
        "analysis" | "waveform" | "analysis meter" | "analysis pitch detector" | "analysis accounting" | "allocate waveform"
        | "create multipass" | "configure multipass" | "render multipass" | "create limiter" | "configure limiter"
        | "render limiter" | "create clip repair" | "clip repair" | "source pull" | "healing accounting"
        | "healing channel layout" | "configure pitch" | "create pitch" | "render pitch" | "create healing meter"
        | "healing meter" | "healing recorder" | "final source" | "final accounting" | "final recorder"
        | "create output meter" | "output meter" | "create channel converter" | "convert channels"
        | "close channel converter" => ProcessingFailed,
        // Temporary workspace.
        "create intermediate directory" | "create intermediate" | "pin intermediate" | "configure intermediate"
        | "write intermediate" | "close intermediate" | "remove intermediate" | "open intermediate" => IoError,
        // Encoding and publication.
        "load FLAC encoder" | "load MP3 encoder" | "load Opus encoder" => LibraryLoadFailed,
        "create MP3 encoder" | "create MP3 PCM converter" | "create FLAC encoder" | "create Apple encoder" | "create Opus encoder"
        | "encode Opus output" | "finish Opus output"
        | "configure Apple encoder" | "convert MP3 PCM" | "encode MP3 output" | "encode FLAC output"
        | "encode Apple output" | "finish Apple output" | "finish FLAC output" | "finish MP3 output"
        | "abort Apple output" | "abort MP3 PCM converter" | "close MP3 PCM converter" | "set controlled MP4 time"
        | "inspect encoded output" | "inspect encoded length" | "close encoded inspection" => EncodeFailed,
        "sync FLAC output" | "sync Opus output" | "create MP3 staging file" | "create FLAC staging file" | "create Opus staging file" | "begin output" | "pin output stage"
        | "verify output stage" | "pin tagged output" | "publish output" => IoError,
        "plan overwrite" => OutputConflict,
        // Reporting (emitted by the CLI layer).
        "report" => ReportSerializationFailed,
        _ => return None,
    })
}

/// Classify one engine failure: a typed error with an unambiguous meaning wins,
/// then the stage table, then `internal_error`.
pub fn classify(stage: &str, error: &dyn Any) -> ErrorCode {
    let code = typed_code(error).or_else(|| stage_code(stage)).unwrap_or(ErrorCode::InternalError);
    // A format refusal raised while creating or running the encoder is about the requested output, not the input:
    // the typed codec errors carry no side, so the stage (every encoder stage maps to `encode_failed`) supplies it.
    if code == ErrorCode::UnsupportedInputFormat && stage_code(stage) == Some(ErrorCode::EncodeFailed) {
        return ErrorCode::UnsupportedOutputFormat;
    }
    code
}

fn typed_code(error: &dyn Any) -> Option<ErrorCode> {
    if let Some(e) = error.downcast_ref::<crate::apple_codec::CodecError>() { return codec_code(e); }
    if let Some(e) = error.downcast_ref::<crate::audio_input::AudioInputError>() { return input_code(e); }
    if let Some(e) = error.downcast_ref::<crate::tags::TagError>() { return tag_code(e); }
    if let Some(e) = error.downcast_ref::<crate::flac::FlacError>() { return flac_code(e); }
    if let Some(e) = error.downcast_ref::<crate::mp3::Mp3Error>() { return mp3_code(e); }
    if let Some(e) = error.downcast_ref::<crate::opus::OpusError>() { return opus_code(e); }
    if let Some(e) = error.downcast_ref::<crate::metadata::MarkerError>() { return marker_code(e); }
    if error.downcast_ref::<std::io::Error>().is_some() { return Some(ErrorCode::IoError); }
    #[cfg(target_os = "linux")]
    if let Some(e) = error.downcast_ref::<crate::linux_libraries::LibraryPolicyError>() { return Some(policy_code(e)); }
    None
}

#[cfg(any(target_os = "linux", feature = "portable"))]
fn codec_code(error: &crate::apple_codec::CodecError) -> Option<ErrorCode> {
    use crate::apple_codec::CodecError as E;
    match error {
        // Produced by no build since v0.4.0 (AAC output is exact, AudioToolbox serves macOS); the mapping stays so that a codec path that reports an encoding
        // refusal gets the reserved code, which `portable_codec_refusals_have_distinct_codes` pins.
        E::UnsupportedCodec { operation: "encoding", .. } => Some(ErrorCode::UnsupportedOutputCodec),
        E::UnsupportedCodec { .. } => Some(ErrorCode::UnsupportedInputCodec),
        E::UnsupportedFormat(_) => Some(ErrorCode::UnsupportedInputFormat),
        E::UnsupportedSampleRateConversion { .. } => Some(ErrorCode::UnsupportedSampleRateConversion),
        E::Io { .. } | E::PublishedButCleanupFailed { .. } => Some(ErrorCode::IoError),
        // A failed cleanup after a cancel is a failure, never a clean cancellation.
        E::Cleanup { primary, .. } => match codec_code(primary) { Some(ErrorCode::Cancelled) => Some(ErrorCode::IoError), other => other },
        E::InvalidInput(_) | E::Mp4Read(_) | E::AlacDecode(_) => Some(ErrorCode::InvalidInputFile),
        E::AacEncode(_) | E::AacContainer(_) => Some(ErrorCode::EncodeFailed),
        // Variants without one meaning (including future ones) take the stage code.
        _ => None,
    }
}
#[cfg(not(any(target_os = "linux", feature = "portable")))]
fn codec_code(error: &crate::apple_codec::CodecError) -> Option<ErrorCode> {
    use crate::apple_codec::CodecError as E;
    match error {
        E::Unsupported(_) => Some(ErrorCode::UnsupportedInputFormat),
        E::Io { .. } | E::PublishedButCleanupFailed { .. } => Some(ErrorCode::IoError),
        // A failed cleanup after a cancel is a failure, never a clean cancellation.
        E::Cleanup { primary, .. } => match codec_code(primary) { Some(ErrorCode::Cancelled) => Some(ErrorCode::IoError), other => other },
        E::InvalidInput(_) => Some(ErrorCode::InvalidInputFile),
        E::Cancelled => Some(ErrorCode::Cancelled),
        E::AlacFrameMismatch { .. } => Some(ErrorCode::EncodeFailed),
        E::Native { .. } | E::Poisoned | E::UnexpectedNativeResult(_) => None,
    }
}

fn input_code(error: &crate::audio_input::AudioInputError) -> Option<ErrorCode> {
    use crate::audio_input::AudioInputError as E;
    match error {
        E::Apple(e) => codec_code(e),
        E::FlacLibrary(e) => flac_code(e),
        E::MissingFlacLibrary | E::MissingFlacDigest => Some(ErrorCode::LibraryNotConfigured),
        E::DigestMismatch { .. } => Some(ErrorCode::LibraryUnapproved),
        E::Library(_) => Some(ErrorCode::LibraryLoadFailed),
        E::InvalidInput(_) => Some(ErrorCode::InvalidInputFile),
        E::Unsupported(_) => Some(ErrorCode::UnsupportedInputFormat),
        E::Opus(e) => opus_code(e),
        E::MissingOpusLibrary | E::MissingOpusDigest => Some(ErrorCode::LibraryNotConfigured),
        E::Allocation | E::Decoder { .. } | E::DecoderData(_) | E::Converter { .. } | E::UnexpectedNativeResult(_) | E::Poisoned => None,
    }
}

fn flac_code(error: &crate::flac::FlacError) -> Option<ErrorCode> {
    use crate::flac::FlacError as E;
    match error {
        // Resolving the configured library path is part of loading the library.
        E::Io { operation: "resolve libFLAC path", .. } => Some(ErrorCode::LibraryLoadFailed),
        E::Io { .. } => Some(ErrorCode::IoError),
        E::Library(_) | E::IncompatibleLibrary { .. } | E::UnexpectedDefault { .. } => Some(ErrorCode::LibraryLoadFailed),
        #[cfg(target_os = "linux")]
        E::LibraryPolicy(e) => Some(policy_code(e)),
        E::InvalidDigest | E::DigestMismatch { .. } => Some(ErrorCode::LibraryUnapproved),
        E::InvalidStagingFile(_) | E::Allocation | E::IncompleteFrame { .. } | E::TooManyFrames(_) | E::Encoder { .. } | E::EncoderFailed => None,
    }
}

fn mp3_code(error: &crate::mp3::Mp3Error) -> Option<ErrorCode> {
    use crate::mp3::Mp3Error as E;
    match error {
        E::Io(_) => Some(ErrorCode::IoError),
        E::Load(_) | E::UnsupportedVersion(_) => Some(ErrorCode::LibraryLoadFailed),
        #[cfg(target_os = "linux")]
        E::LibraryPolicy(e) => Some(policy_code(e)),
        E::InvalidDigest | E::DigestMismatch { .. } => Some(ErrorCode::LibraryUnapproved),
        E::InvalidConfiguration(_) | E::InvalidBuffer(_) | E::Lame { .. } | E::StreamFailed | E::TagRewriteFailed => None,
    }
}

/// Opus input and output: the type of the failure decides, so a stage that can
/// also fail for other reasons still reports the specific cause.
fn opus_code(error: &crate::opus::OpusError) -> Option<ErrorCode> {
    use crate::opus::OpusError as E;
    match error {
        // Resolving the configured libopus path is part of loading it (the contract: every load stage).
        E::Io { operation: "resolve library path", .. } => Some(ErrorCode::LibraryLoadFailed),
        E::Io { .. } => Some(ErrorCode::IoError),
        E::Library(_) | E::IncompatibleLibrary { .. } => Some(ErrorCode::LibraryLoadFailed),
        #[cfg(target_os = "linux")]
        E::LibraryPolicy(e) => Some(policy_code(e)),
        E::InvalidDigest | E::DigestMismatch { .. } | E::SelfTest(_) => Some(ErrorCode::LibraryUnapproved),
        E::NotOpus(_) => Some(ErrorCode::UnsupportedInputCodec),
        E::Invalid(_) | E::Limit(_) => Some(ErrorCode::InvalidInputFile),
        E::Unsupported(_) => Some(ErrorCode::UnsupportedOpusVariant),
        E::SampleRate { .. } => Some(ErrorCode::UnsupportedSampleRateConversion),
        E::Decode { .. } => Some(ErrorCode::DecodeFailed),
        E::Encode { .. } => Some(ErrorCode::EncodeFailed),
        E::Poisoned => None,
    }
}

#[cfg(target_os = "linux")]
fn policy_code(error: &crate::linux_libraries::LibraryPolicyError) -> ErrorCode {
    use crate::linux_libraries::LibraryPolicyError as E;
    match error { E::Unapproved { .. } => ErrorCode::LibraryUnapproved, E::Identity(_) => ErrorCode::LibraryLoadFailed }
}

fn tag_code(error: &crate::tags::TagError) -> Option<ErrorCode> {
    use crate::tags::TagError as E;
    match error {
        E::Io { operation: "resolve bridge", .. } => Some(ErrorCode::LibraryLoadFailed),
        E::Io { .. } => Some(ErrorCode::IoError),
        #[cfg(target_os = "linux")]
        E::LibraryPolicy(e) => Some(policy_code(e)),
        E::ArtworkTranscodeUnavailable(_) => Some(ErrorCode::ArtworkConversionUnavailable),
        E::SourceContainerUnavailable(_) => Some(ErrorCode::MarkerUnverifiable),
        E::WrongBridgeAbi(_) => Some(ErrorCode::LibraryLoadFailed),
        E::PathContainsNul => Some(ErrorCode::InvalidInputPath),
        E::SourceIsDestination => Some(ErrorCode::OutputConflict),
        E::Marker(e) => marker_code(e),
        E::Opus(e) => opus_code(e),
        E::MarkerCallbackPanicked | E::MarkerReadCallbackPanicked | E::InspectionCallbackPanicked => Some(ErrorCode::InternalError),
        E::Library(_) => None,
    }
}

fn marker_code(error: &crate::metadata::MarkerError) -> Option<ErrorCode> {
    use crate::metadata::MarkerError as E;
    match error {
        E::Io { .. } => Some(ErrorCode::IoError),
        E::InvalidWave(_) => Some(ErrorCode::InvalidInputFile),
        E::InvalidMarker(_) | E::InvalidField(_) | E::UnobservedShortBase64 { .. } | E::SizeOverflow => Some(ErrorCode::MarkerInvalid),
        E::FoundationUnavailable(_) | E::FoundationObjectNil(_) | E::FoundationDataInvalid => Some(ErrorCode::InternalError),
    }
}

impl WorkflowError {
    /// The stable CLI code for this failure; a transaction reports its cause.
    pub fn code(&self) -> ErrorCode {
        use ErrorCode as C;
        match self {
            Self::Io { .. } | Self::Move { .. } => C::IoError,
            Self::AtomicRenameUnsupported { .. } => C::AtomicRenameUnsupported,
            Self::InvalidPath { .. } => C::InvalidInputPath,
            Self::UnsupportedSource(_) => C::UnsupportedInputFormat,
            Self::ProtectedSource(_) | Self::RestoreProtected(_) => C::ProtectedSource,
            Self::UnsupportedCodec(_) => C::UnsupportedInputCodec,
            Self::AlreadyProcessed { .. } => C::AlreadyProcessed,
            Self::DestinationExists(_) | Self::ReservationExists(_) => C::OutputExists,
            Self::DestinationChanged(_) | Self::DestinationConflict { .. } => C::OutputConflict,
            Self::RestoreInputConflict { .. } => C::Usage,
            Self::SourceChanged(_) => C::SourceChanged,
            Self::BackupChanged(_) | Self::InvalidMarkerBackup | Self::BackupMismatch { .. } => C::BackupInvalid,
            Self::BackupNotRecorded(_) | Self::BackupNotFound(_) | Self::SuppliedBackupNotFound(_) => C::BackupMissing,
            Self::RestoreNotNeeded(_) | Self::NotProcessed(_) | Self::RestoreBusy(_) => C::RestoreNotApplicable,
            Self::Cancelled => C::Cancelled,
            // A cancellation that left a recovery file, rollback or cleanup failure, or a publication behind
            // is a failure that needs attention (`is_cancelled` is false for it), not a clean cancel.
            Self::Transaction { cause, .. } => match cause.code() {
                C::Cancelled if !self.is_cancelled() => C::IoError,
                code => code,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_libopus_path_is_a_library_load_failure_and_other_opus_io_stays_io_error() {
        use crate::opus::{OpusError, io_error};
        let missing = || std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(opus_code(&io_error("resolve library path", missing())), Some(ErrorCode::LibraryLoadFailed));
        assert_eq!(opus_code(&io_error("read input", missing())), Some(ErrorCode::IoError));
        assert_eq!(opus_code(&OpusError::Library("x".into())), Some(ErrorCode::LibraryLoadFailed));
    }

    #[test]
    fn codes_are_unique_snake_case_and_complete() {
        let mut seen = std::collections::BTreeSet::new();
        for code in ErrorCode::ALL {
            let text = code.as_str();
            assert!(!text.is_empty() && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "{text}");
            assert!(seen.insert(text), "duplicate code {text}");
            assert_eq!(serde_json::to_value(code).unwrap(), serde_json::Value::String(text.to_owned()));
        }
        // Adding a variant without listing it here fails the match in `as_str`;
        // this guards the list itself.
        assert_eq!(seen.len(), 39);
    }

    #[test]
    fn every_engine_stage_label_has_a_stage_code() {
        let source = include_str!("engine.rs");
        let mut labels = std::collections::BTreeSet::new();
        for marker in ["EngineError::new(\"", "EngineError::library_load(\"", "EngineError::workflow(\"", "EngineError::coded(\"", "require_reportable_path(path, \"",
            "require_reportable_path(plan.source(), \"", "require_reportable_path(plan.destination(), \"",
            "require_reportable_path(&path, \""] {
            for part in source.split(marker).skip(1) {
                if let Some(end) = part.find('"') { labels.insert(part[..end].to_owned()); }
            }
        }
        assert!(labels.len() > 60, "label scan found only {}", labels.len());
        let unmapped: Vec<_> = labels.iter().filter(|label| stage_code(label).is_none()).collect();
        assert!(unmapped.is_empty(), "stage labels without a code: {unmapped:?}");
    }

    #[test]
    fn typed_errors_override_stage_defaults() {
        let io = std::io::Error::other("boom");
        assert_eq!(classify("open input", &io), ErrorCode::IoError);
        assert_eq!(classify("open input", &String::from("x")), ErrorCode::InvalidInputFile);
        assert_eq!(classify("no such stage", &String::from("x")), ErrorCode::InternalError);
    }

    #[test]
    fn workflow_codes_follow_the_cause_of_a_transaction() {
        let transaction = WorkflowError::Transaction {
            cause: Box::new(WorkflowError::DestinationExists("/x".into())), backup: None, recovery: None,
            rollback: None, cleanup: Vec::new(), published: false,
        };
        assert_eq!(transaction.code(), ErrorCode::OutputExists);
        assert_eq!(WorkflowError::Cancelled.code(), ErrorCode::Cancelled);
    }

    #[test]
    fn only_a_clean_cancellation_reports_the_cancelled_code() {
        use crate::file_workflow::CleanupFailure;
        let path = || std::path::PathBuf::from("/x");
        let transaction = |recovery: Option<std::path::PathBuf>, rollback: Option<Box<WorkflowError>>, cleanup: Vec<CleanupFailure>, published: bool| {
            WorkflowError::Transaction { cause: Box::new(WorkflowError::Cancelled), backup: None, recovery, rollback, cleanup, published }
        };
        let clean = transaction(None, None, Vec::new(), false);
        assert!(clean.is_cancelled());
        assert_eq!(clean.code(), ErrorCode::Cancelled);
        let cases = [
            transaction(Some(path()), None, Vec::new(), false),
            transaction(None, Some(Box::new(WorkflowError::SourceChanged(path()))), Vec::new(), false),
            transaction(None, None, vec![CleanupFailure { operation: "remove", path: path(), source: std::io::Error::other("x") }], false),
            transaction(None, None, Vec::new(), true),
        ];
        for error in cases {
            assert!(!error.is_cancelled(), "{error}");
            assert_eq!(error.code(), ErrorCode::IoError, "{error}");
        }
        // Nested transactions follow the same rule at every level.
        let nested = WorkflowError::Transaction {
            cause: Box::new(transaction(Some(path()), None, Vec::new(), false)), backup: None, recovery: None,
            rollback: None, cleanup: Vec::new(), published: false,
        };
        assert_eq!(nested.code(), ErrorCode::IoError);
    }

    #[cfg(not(any(target_os = "linux", feature = "portable")))]
    #[test]
    fn an_apple_writer_cleanup_failure_after_a_cancel_is_not_a_cancellation() {
        use crate::apple_codec::CodecError as E;
        let cancelled = E::Cleanup { primary: Box::new(E::Cancelled), source: std::io::Error::other("remove failed") };
        assert_eq!(classify("finish Apple output", &cancelled), ErrorCode::IoError);
        assert_eq!(classify("finish Apple output", &E::Cancelled), ErrorCode::Cancelled);
    }

    #[cfg(any(target_os = "linux", feature = "portable"))]
    #[test]
    fn a_portable_codec_cleanup_failure_keeps_its_primary_meaning() {
        use crate::apple_codec::CodecError as E;
        let wrapped = E::Cleanup { primary: Box::new(E::InvalidInput("x")), source: std::io::Error::other("remove failed") };
        assert_eq!(classify("open input", &wrapped), ErrorCode::InvalidInputFile);
    }

    #[test]
    fn resolving_a_library_path_is_part_of_loading_it() {
        use crate::{flac::FlacError, tags::TagError};
        let missing = || std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(classify("load FLAC encoder", &FlacError::Io { operation: "resolve libFLAC path", source: missing() }), ErrorCode::LibraryLoadFailed);
        assert_eq!(classify("sync FLAC output", &FlacError::Io { operation: "flush FLAC staging file", source: missing() }), ErrorCode::IoError);
        assert_eq!(classify("tags", &TagError::Io { operation: "resolve bridge", source: missing() }), ErrorCode::LibraryLoadFailed);
        assert_eq!(classify("copy source tags", &TagError::Io { operation: "open WAV for marker", source: missing() }), ErrorCode::IoError);
    }

    /// Every format refusal needs a side: an encoder stage that refuses the audio's format is about the output.
    #[test]
    fn a_format_refusal_in_an_encoder_stage_is_about_the_output() {
        #[cfg(any(target_os = "linux", feature = "portable"))]
        let refusal = crate::apple_codec::CodecError::UnsupportedFormat("ALAC output channel count or sample rate is not verified");
        #[cfg(not(any(target_os = "linux", feature = "portable")))]
        let refusal = crate::apple_codec::CodecError::Unsupported("synthetic format refusal");
        for stage in ["create Apple encoder", "configure Apple encoder", "encode Apple output", "create FLAC encoder", "create MP3 encoder"] {
            assert_eq!(classify(stage, &refusal), ErrorCode::UnsupportedOutputFormat, "{stage}");
        }
        for stage in ["open input", "open analysis input", "open healing input", "inspect length"] {
            assert_eq!(classify(stage, &refusal), ErrorCode::UnsupportedInputFormat, "{stage}");
        }
    }

    #[test]
    fn every_workflow_variant_maps_to_its_documented_code() {
        use crate::file_workflow::ProcessingState;
        use ErrorCode as C;
        use WorkflowError as W;
        let path = || std::path::PathBuf::from("/x");
        let io = || std::io::Error::other("x");
        let cases: Vec<(WorkflowError, ErrorCode)> = vec![
            (W::Io { operation: "op", path: path(), source: io() }, C::IoError),
            (W::Move { from: path(), to: path(), source: io() }, C::IoError),
            (W::AtomicRenameUnsupported { operation: "op", path: path(), source: io() }, C::AtomicRenameUnsupported),
            (W::InvalidPath { path: path(), reason: "r" }, C::InvalidInputPath),
            (W::UnsupportedSource(path()), C::UnsupportedInputFormat),
            (W::ProtectedSource(path()), C::ProtectedSource),
            (W::UnsupportedCodec(1), C::UnsupportedInputCodec),
            (W::AlreadyProcessed { path: path(), state: ProcessingState::Suffixed }, C::AlreadyProcessed),
            (W::DestinationExists(path()), C::OutputExists),
            (W::DestinationChanged(path()), C::OutputConflict),
            (W::DestinationConflict { destination: path(), other_source: path() }, C::OutputConflict),
            (W::RestoreInputConflict { target: path(), backup: path() }, C::Usage),
            (W::ReservationExists(path()), C::OutputExists),
            (W::SourceChanged(path()), C::SourceChanged),
            (W::BackupChanged(path()), C::BackupInvalid),
            (W::InvalidMarkerBackup, C::BackupInvalid),
            (W::BackupNotRecorded(path()), C::BackupMissing),
            (W::BackupNotFound(path()), C::BackupMissing),
            (W::SuppliedBackupNotFound(path()), C::BackupMissing),
            (W::BackupMismatch { recorded: path(), supplied: path() }, C::BackupInvalid),
            (W::RestoreNotNeeded(path()), C::RestoreNotApplicable),
            (W::NotProcessed(path()), C::RestoreNotApplicable),
            (W::RestoreBusy(path()), C::RestoreNotApplicable),
            (W::RestoreProtected(path()), C::ProtectedSource),
            (W::Cancelled, C::Cancelled),
        ];
        for (error, code) in cases { assert_eq!(error.code(), code, "{error}"); }
    }

    #[test]
    fn tag_marker_flac_and_mp3_errors_carry_their_meaning() {
        use crate::tags::TagError as T;
        assert_eq!(classify("copy source tags", &T::ArtworkTranscodeUnavailable("art".into())), ErrorCode::ArtworkConversionUnavailable);
        assert_eq!(classify("inspect tags", &T::SourceContainerUnavailable("x".into())), ErrorCode::MarkerUnverifiable);
        assert_eq!(classify("tags", &T::WrongBridgeAbi(4)), ErrorCode::LibraryLoadFailed);
        assert_eq!(classify("tags", &T::PathContainsNul), ErrorCode::InvalidInputPath);
        assert_eq!(classify("copy source tags", &T::SourceIsDestination), ErrorCode::OutputConflict);
        // A runtime bridge failure has no meaning of its own: the stage decides.
        assert_eq!(classify("copy source tags", &T::Library("x".into())), ErrorCode::TagCopyFailed);
        assert_eq!(classify("tags", &T::Library("x".into())), ErrorCode::LibraryLoadFailed);
        assert_eq!(classify("inspect tags", &T::Library("x".into())), ErrorCode::InvalidInputFile);

        use crate::metadata::MarkerError as M;
        assert_eq!(classify("decode marker", &M::InvalidMarker("x")), ErrorCode::MarkerInvalid);
        assert_eq!(classify("read marker", &M::InvalidWave("x")), ErrorCode::InvalidInputFile);
        assert_eq!(classify("read marker", &M::FoundationDataInvalid), ErrorCode::InternalError);

        use crate::flac::FlacError as F;
        assert_eq!(classify("load FLAC encoder", &F::InvalidDigest), ErrorCode::LibraryUnapproved);
        assert_eq!(classify("load FLAC encoder", &F::Library("x".into())), ErrorCode::LibraryLoadFailed);
        assert_eq!(classify("encode FLAC output", &F::EncoderFailed), ErrorCode::EncodeFailed);

        use crate::mp3::Mp3Error as P;
        assert_eq!(classify("load MP3 encoder", &P::DigestMismatch { expected: "a".into(), actual: "b".into() }), ErrorCode::LibraryUnapproved);
        assert_eq!(classify("load MP3 encoder", &P::UnsupportedVersion("x".into())), ErrorCode::LibraryLoadFailed);
        assert_eq!(classify("encode MP3 output", &P::StreamFailed), ErrorCode::EncodeFailed);

        use crate::audio_input::AudioInputError as A;
        assert_eq!(classify("open input", &A::MissingFlacLibrary), ErrorCode::LibraryNotConfigured);
        assert_eq!(classify("open input", &A::MissingFlacDigest), ErrorCode::LibraryNotConfigured);
        assert_eq!(classify("open input", &A::InvalidInput("x")), ErrorCode::InvalidInputFile);
        assert_eq!(classify("open input", &A::Unsupported("x")), ErrorCode::UnsupportedInputFormat);
        assert_eq!(classify("read analysis input", &A::DecoderData(1)), ErrorCode::DecodeFailed);
    }

    #[cfg(any(target_os = "linux", feature = "portable"))]
    #[test]
    fn portable_codec_refusals_have_distinct_codes() {
        use crate::apple_codec::CodecError as E;
        let decode = E::UnsupportedCodec { codec: "AAC", operation: "decoding", reason: "r" };
        let mp3 = E::UnsupportedCodec { codec: "MP3", operation: "decoding", reason: "r" };
        let encode = E::UnsupportedCodec { codec: "AAC", operation: "encoding", reason: "r" };
        assert_eq!(classify("open input", &decode), ErrorCode::UnsupportedInputCodec);
        assert_eq!(classify("open input", &mp3), ErrorCode::UnsupportedInputCodec);
        assert_eq!(classify("create Apple encoder", &encode), ErrorCode::UnsupportedOutputCodec);
        assert_eq!(classify("open input", &E::UnsupportedFormat("x")), ErrorCode::UnsupportedInputFormat);
        assert_eq!(classify("output sample-rate conversion", &E::UnsupportedSampleRateConversion { source_rate: 22_050.0, client_rate: 44_100.0 }),
            ErrorCode::UnsupportedSampleRateConversion);
        assert_eq!(classify("open input", &E::InvalidInput("x")), ErrorCode::InvalidInputFile);
        assert_eq!(classify("encode Apple output", &E::Native { operation: "op", status: -1 }), ErrorCode::EncodeFailed);
        let wrapped = crate::audio_input::AudioInputError::Apple(E::UnsupportedCodec { codec: "MP3", operation: "decoding", reason: "r" });
        assert_eq!(classify("open analysis input", &wrapped), ErrorCode::UnsupportedInputCodec);
    }

    #[cfg(any(target_os = "linux", feature = "portable"))]
    #[test]
    fn portable_aac_encoder_and_container_failures_are_encode_failures_at_every_stage() {
        use crate::apple_codec::CodecError as E;
        use crate::portable::aac::{encoder::EncodeError, mp4::Mp4Error};
        let encode = E::AacEncode(EncodeError::PartialFrame);
        let container = E::AacContainer(Mp4Error::FileTooLarge);
        // a stage that names the output says encode_failed anyway; the typed variant carries the meaning, not the stage
        for stage in ["encode Apple output", "finish Apple output", "create Apple encoder"] {
            assert_eq!(classify(stage, &encode), ErrorCode::EncodeFailed, "{stage}");
            assert_eq!(classify(stage, &container), ErrorCode::EncodeFailed, "{stage}");
        }
        // wrapped by a failed cleanup, the primary failure keeps its code
        let wrapped = E::Cleanup { primary: Box::new(E::AacContainer(Mp4Error::TooManyAccessUnits(1 << 23))), source: std::io::Error::other("remove failed") };
        assert_eq!(classify("finish Apple output", &wrapped), ErrorCode::EncodeFailed);
        // a fractional rate is a format refusal about the output, not the input
        let fractional = E::UnsupportedFormat("fractional AAC sample rates are not reproduced");
        assert_eq!(classify("create Apple encoder", &fractional), ErrorCode::UnsupportedOutputFormat);
        // the native statuses of an AAC format AudioToolbox cannot create are encode failures too
        let native = E::Native { operation: "ExtAudioFileSetProperty(client format)", status: 0x666d_743f };
        assert_eq!(classify("create Apple encoder", &native), ErrorCode::EncodeFailed);
    }

    #[cfg(not(any(target_os = "linux", feature = "portable")))]
    #[test]
    fn apple_codec_errors_map_by_variant_or_stage() {
        use crate::apple_codec::CodecError as E;
        assert_eq!(classify("open input", &E::Unsupported("x")), ErrorCode::UnsupportedInputFormat);
        assert_eq!(classify("open input", &E::InvalidInput("x")), ErrorCode::InvalidInputFile);
        assert_eq!(classify("finish Apple output", &E::AlacFrameMismatch { submitted: 2, decoded: 1 }), ErrorCode::EncodeFailed);
        assert_eq!(classify("encode Apple output", &E::Native { operation: "op", status: -1 }), ErrorCode::EncodeFailed);
        assert_eq!(classify("open input", &E::Native { operation: "op", status: -1 }), ErrorCode::InvalidInputFile);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn library_policy_codes_distinguish_unapproved_from_unloadable() {
        use crate::linux_libraries::{LibraryKind, LibraryPolicyError as P};
        let unapproved = P::Unapproved { library: LibraryKind::Flac, actual: [0; 32], expected: "x" };
        assert_eq!(classify("load FLAC encoder", &unapproved), ErrorCode::LibraryUnapproved);
        assert_eq!(classify("tags", &P::Identity("x".into())), ErrorCode::LibraryLoadFailed);
    }

    #[test]
    fn contract_table_lists_exactly_the_defined_codes() {
        let contract = include_str!("../research/CLI_CONTRACT.md");
        let section = contract.split("\n## Error codes").nth(1).expect("contract has an Error codes section");
        let section = section.split("\n## ").next().unwrap();
        let documented: std::collections::BTreeSet<&str> = section.lines()
            .filter_map(|line| line.strip_prefix("| `")).filter_map(|row| row.split('`').next()).collect();
        let defined: std::collections::BTreeSet<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
        assert_eq!(documented, defined);
    }
}
