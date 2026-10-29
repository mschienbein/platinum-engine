//! Streaming Platinum Notes controller integration. The implementations behind
//! these calls are native reconstructions, not the proprietary application.
//! Complete encoded-file identity is established only for a pinned reference case.

use crate::apple_codec::{self, StreamDescription};
#[cfg(all(target_os = "macos", not(feature = "portable")))]
use crate::apple_codec::{
    AudioBuffer, AudioBufferList, StereoBufferList,
    AudioConverterConvertComplexBuffer, AudioConverterDispose, AudioConverterNew,
    CFRelease, CFURLCreateFromFileSystemRepresentation, ExtAudioFileCreateWithURL,
    ExtAudioFileDispose, ExtAudioFileSetProperty, ExtAudioFileWrite,
};
use crate::audio_input::{AudioReader, InputLibraries};
use crate::clip::ClipRepair;
use crate::controller::{self, AnalysisValues, FinalPlan, FrameAccounting, HealingPlan, RenderPass, RecorderAccounting, RENDER_POST, RENDER_SILENCE};
use crate::file_workflow::{self, AudioFormat, Cancellation, DiscoveredFile, FileIdentity, OutputPlan, ProcessingState, SourceCodec, SourceType, WorkflowError};
use crate::fp_mode::with_flush_to_zero;
use crate::limiter::{LimiterContext, LimiterDsp, LimiterModel, LimiterValue};
use crate::loudness::{GainAnalyzer, LoudnessMeter};
use crate::metadata::{self, Marker, ProcessingMarker};
use crate::multipass::{CrossoverFrequencyScale, ParameterWrite, ShippedPreset};
use crate::multipass_graph::{GraphContext, MultipassGraph};
use crate::phase::PhaseCorrection;
use crate::pitch::{PitchMode, PitchProcessor};
use crate::pitch_detect::PitchDetector;
use crate::error_code::{self, ErrorCode};
use crate::settings::Settings;
use crate::tags::{self, TagBridge};
use crate::templates::Template;
use serde::Serialize;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const BLOCK: usize = controller::RENDER_BLOCK_FRAMES as usize;
const VERSION: &str = "10.5.0.1256";

/// An error is never a successful or partially successful processing report.
/// `partial_publication` means the destination was published before an error;
/// `retained_backup` and `recovery_path` identify files that must not be discarded.
/// `code` is the stable machine-readable reason (see `error_code`); `stage` and
/// `message` are diagnostic text.
#[derive(Debug, Serialize)]
pub struct EngineError {
    pub stage: &'static str,
    pub code: ErrorCode,
    pub message: String,
    pub cancelled: bool,
    pub retained_backup: Option<PathBuf>,
    pub recovery_path: Option<PathBuf>,
    pub partial_publication: bool,
}
impl EngineError {
    /// The code comes from the error's type when its variant has one meaning,
    /// otherwise from the stage label.
    fn new(stage: &'static str, error: impl fmt::Display + 'static) -> Self {
        let code = error_code::classify(stage, &error);
        Self { stage, code, message: error.to_string(), cancelled: false, retained_backup: None, recovery_path: None, partial_publication: false }
    }
    /// A failure whose code is fixed by the call site rather than its stage.
    fn coded(stage: &'static str, code: ErrorCode, error: impl fmt::Display) -> Self {
        Self { stage, code, message: error.to_string(), cancelled: false, retained_backup: None, recovery_path: None, partial_publication: false }
    }
    /// A native library that could not be loaded. A missing path, a directory, a dynamic-loader failure and an I/O
    /// error while resolving or hashing the file are all `library_load_failed`; only a more specific typed answer
    /// (an unapproved or mismatching digest) keeps its own code.
    fn library_load(stage: &'static str, error: impl fmt::Display + 'static) -> Self {
        let mut error = Self::new(stage, error);
        if error.code == ErrorCode::IoError { error.code = ErrorCode::LibraryLoadFailed; }
        error
    }
    fn cancelled(stage: &'static str) -> Self {
        Self { stage, code: ErrorCode::Cancelled, message: "processing cancelled".into(), cancelled: true, retained_backup: None, recovery_path: None, partial_publication: false }
    }
    fn workflow(stage: &'static str, error: WorkflowError) -> Self {
        let (backup, recovery, published) = match &error {
            WorkflowError::Transaction { backup, recovery, published, .. } => (backup.clone(), recovery.clone(), *published),
            _ => (None, None, false),
        };
        let cancelled = error.is_cancelled();
        Self { stage, code: error.code(), message: error.to_string(), cancelled, retained_backup: backup, recovery_path: recovery, partial_publication: published }
    }
    /// A cancellation that a failed cleanup, abort or rollback turned into a failure is no
    /// longer a clean cancel: `cancelled` and `code` must agree, so it reports `io_error`.
    fn no_longer_a_clean_cancel(&mut self) {
        self.cancelled = false;
        if self.code == ErrorCode::Cancelled { self.code = ErrorCode::IoError; }
    }
}
impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}: {}", self.stage, self.message) }
}
impl Error for EngineError {}

// JSON cannot represent infinities; preserve their meaning instead of silently
// converting a measured silent track to null.
fn float_json<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    if v.is_nan() { s.serialize_str("NaN") }
    else if *v == f32::INFINITY { s.serialize_str("Infinity") }
    else if *v == f32::NEG_INFINITY { s.serialize_str("-Infinity") }
    else { s.serialize_f32(*v) }
}

#[derive(Debug, Serialize)]
pub struct ProcessReport {
    pub input_path: PathBuf,
    pub input_sample_rate: f64,
    pub processing_sample_rate: f64,
    pub output_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    /// Whether a distinct pre-existing output was atomically replaced.
    pub replaced_existing_output: bool,
    pub input_format: String,
    pub output_format: String,
    pub sample_rate: f64,
    pub channels: u32,
    pub output_channels: u32,
    pub output_bit_depth: u32,
    /// File length reported by the encoded container, not the recorder count.
    pub encoded_length_frames: u64,
    pub input_frames: u64,
    pub healed_frames: u64,
    pub output_frames: u64,
    #[serde(serialize_with = "float_json")]
    pub input_volume: f32,
    #[serde(serialize_with = "float_json")]
    pub healed_volume: f32,
    #[serde(serialize_with = "float_json")]
    pub output_volume: f32,
    #[serde(serialize_with = "float_json")]
    pub input_pitch_hz: f32,
    #[serde(serialize_with = "float_json")]
    pub output_pitch_hz: f32,
    pub tempo_factor: f64,
    pub clip_count: u64,
    pub applied_stages: Vec<String>,
    /// `present`, `nil_payload_absent`, or `nil_payload_empty_wav_chunk`.
    pub processing_marker_status: &'static str,
    /// An explicitly authorized ambiguous input marker, if one was used.
    pub input_marker_override: Option<&'static str>,
    /// A source tag container could not be inspected or copied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_tags_unavailable: Option<String>,
    /// Diagnostic only: the supplied second (since 1904) written to every MP4
    /// creation/modification header field instead of the AudioToolbox clock.
    pub controlled_mp4_time_1904: Option<u32>,
    /// Paths used for this item that are not proven byte-identical to the macOS
    /// reference: `mp3_decode` (open-source MP3 decoder) and `artwork_copy`
    /// (embedded cover art copied unchanged). Always present; empty when every
    /// path used is an identical one (always the case on macOS default builds).
    pub non_identical_paths: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct AnalysisReport {
    pub path: PathBuf,
    pub input_format: String,
    pub sample_rate: f64,
    pub channels: u32,
    pub frames: u64,
    #[serde(serialize_with = "float_json")]
    pub volume: f32,
    #[serde(serialize_with = "float_json")]
    pub pitch_hz: f32,
    /// Previous repair count if the input has a processed marker, not a new repair measurement.
    pub clip_count: Option<u64>,
    /// None means the source tag container could not be inspected.
    pub processed: Option<bool>,
    pub processing_marker: Option<serde_json::Value>,
    pub dynamics: Option<ReportedDynamics>,
    /// First-pass analysis does not run the repair/dynamics nodes.
    pub unavailable: Vec<String>,
    pub waveform: Option<Waveform>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waveform_unavailable: Option<String>,
    /// Present when the current file's marker names a readable original.
    pub original_waveform: Option<Waveform>,
    pub original_waveform_unavailable: Option<String>,
    /// Non-identical paths whose output feeds these measurements (`mp3_decode`);
    /// always present, empty when none was used.
    pub non_identical_paths: Vec<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct ReportedDynamics {
    pub source: &'static str,
    pub expansion_applied: bool,
    pub compression_applied: bool,
}

/// Per-read channel mean-absolute amplitudes used by the native waveform view.
/// Apple inputs request 44.1 kHz; the native FLAC route retains its source rate.
#[derive(Debug, Serialize)]
pub struct Waveform {
    pub path: PathBuf,
    pub requested_sample_rate: f64,
    pub sample_rate: f64,
    pub channels: u32,
    pub layout: &'static str,
    pub frames: u64,
    pub bins: Vec<WaveformBin>,
}
#[derive(Debug, Serialize)]
pub struct WaveformBin {
    pub start_frame: u64,
    pub frames: u32,
    #[serde(serialize_with = "float_json")]
    pub left_mean_absolute: f32,
    #[serde(serialize_with = "float_json")]
    pub right_mean_absolute: f32,
}

#[derive(Debug, Serialize)]
pub struct InspectionReport {
    pub path: PathBuf,
    pub input_format: String,
    pub sample_rate: f64,
    pub channels: u32,
    pub bit_depth: u32,
    /// Advertised container length, not the result of a complete decode.
    pub frames: u64,
    pub bitrate: Option<u32>,
    pub bitrate_error: Option<String>,
    pub tags: Option<tags::TagInspection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_unavailable: Option<String>,
    /// Successful inspection establishes marker or suffix state.
    pub processed: Option<bool>,
    pub processing_state: &'static str,
    pub backup_path: Option<PathBuf>,
    pub backup_exists: Option<bool>,
    pub backup_error: Option<String>,
    pub marker_json: Option<serde_json::Value>,
    /// Non-identical paths that produced these decoder-derived properties
    /// (`mp3_decode`); always present, empty when none was used.
    pub non_identical_paths: Vec<&'static str>,
    #[serde(skip)]
    pub marker: Option<Marker>,
    #[serde(skip)]
    pub source_codec: SourceCodec,
}

/// Names listed in `non_identical_paths`.
pub const NON_IDENTICAL_MP3_DECODE: &str = "mp3_decode";
pub const NON_IDENTICAL_ARTWORK_COPY: &str = "artwork_copy";

fn non_identical_paths(mp3_decode: bool, artwork_copy: bool) -> Vec<&'static str> {
    let mut paths = Vec::new();
    if mp3_decode { paths.push(NON_IDENTICAL_MP3_DECODE); }
    if artwork_copy { paths.push(NON_IDENTICAL_ARTWORK_COPY); }
    paths
}

fn input_libraries(settings: &Settings) -> InputLibraries {
    InputLibraries { flac: settings.libraries.flac.clone(), flac_sha256: settings.libraries.flac_sha256.clone(),
        allow_non_identical: settings.non_identical_enabled() }
}
fn bridge(settings: &Settings) -> Result<TagBridge, EngineError> {
    let path = settings.libraries.tag_bridge.as_ref().ok_or_else(|| EngineError::coded("tags", ErrorCode::LibraryNotConfigured, "a trusted TagLib bridge path is required"))?;
    // Settings selects the native executable. As with the codec dylibs the
    // caller is responsible for its provenance and executable initializers.
    unsafe { TagBridge::load(path) }.map_err(|e| EngineError::library_load("tags", e))
}
fn source_tag_format(path: &Path) -> Result<tags::Format, EngineError> {
    match file_workflow::source_type(path) {
        Some(SourceType::Mp3) => Ok(tags::Format::Mp3),
        Some(SourceType::M4a) => Ok(tags::Format::Mp4),
        Some(SourceType::Wav) => Ok(tags::Format::Wav),
        Some(SourceType::Aiff) => Ok(tags::Format::Aiff),
        Some(SourceType::Flac) => Ok(tags::Format::Flac),
        Some(SourceType::ProtectedM4p | SourceType::ProtectedWlMp3) => Err(EngineError::coded("input", ErrorCode::ProtectedSource, "protected source is not admitted")),
        _ => Err(EngineError::new("input", format!("unsupported source {}", path.display()))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SniffedContainer {
    Writable(tags::Format),
    Ogg,
    Unknown,
}

// Sniff independently of the extension-selected tag parser: that parser can
// succeed on another family if its payload happens to contain an MPEG frame.
fn sniff_container(path: &Path, extension: tags::Format) -> Result<(SniffedContainer, usize, bool), EngineError> {
    let map_io = |e| EngineError::new("sniff source container", e);
    let mut input = fs::File::open(path).map_err(map_io)?;
    let length = input.metadata().map_err(map_io)?.len();
    let mut position = 0u64;
    let mut id3_count = 0usize;
    let mut additional_id3_nonempty = false;
    loop {
        input.seek(SeekFrom::Start(position)).map_err(map_io)?;
        let mut prefix = [0u8; 12];
        let mut len = 0usize;
        let want = usize::try_from((length - position).min(prefix.len() as u64)).unwrap();
        while len < want {
            match input.read(&mut prefix[len..want]) {
                Ok(0) => return Err(map_io(io::Error::new(io::ErrorKind::UnexpectedEof, "source shortened while sniffing"))),
                Ok(n) => len += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(map_io(e)),
            }
        }
        if !prefix[..len].starts_with(b"ID3") {
            let detected = sniff_container_prefix(&prefix[..len]);
            if matches!(detected, SniffedContainer::Unknown | SniffedContainer::Writable(tags::Format::Mp3))
                && sniff_mp4_atoms(&mut input, position, length)? {
                return Ok((SniffedContainer::Writable(tags::Format::Mp4), id3_count, additional_id3_nonempty));
            }
            if detected == SniffedContainer::Unknown && (extension == tags::Format::Mp3 || id3_count != 0)
                && sniff_mpeg_frames(&mut input, position, length)? {
                return Ok((SniffedContainer::Writable(tags::Format::Mp3), id3_count, additional_id3_nonempty));
            }
            return Ok((detected, id3_count, additional_id3_nonempty));
        }
        if len < 10 || id3_count == 1_000_000 { return Ok((SniffedContainer::Unknown, id3_count, additional_id3_nonempty)); }
        let valid_flags = match prefix[3] {
            2 => prefix[5] & 0x3f == 0,
            3 => prefix[5] & 0x1f == 0,
            4 => prefix[5] & 0x0f == 0,
            _ => false,
        };
        if !valid_flags || prefix[4] == 0xff || prefix[6..10].iter().any(|b| b & 0x80 != 0) {
            return Ok((SniffedContainer::Unknown, id3_count, additional_id3_nonempty));
        }
        let size = prefix[6..10].iter().fold(0u64, |n, b| (n << 7) | u64::from(*b));
        if id3_count != 0 && size != 0 { additional_id3_nonempty = true; }
        let footer = prefix[3] == 4 && prefix[5] & 0x10 != 0;
        let Some(end) = position.checked_add(10 + size + if footer { 10 } else { 0 }).filter(|end| *end <= length) else {
            return Ok((SniffedContainer::Unknown, id3_count, additional_id3_nonempty));
        };
        if footer {
            input.seek(SeekFrom::Start(end - 10)).map_err(map_io)?;
            let mut footer_bytes = [0u8; 10];
            input.read_exact(&mut footer_bytes).map_err(map_io)?;
            if &footer_bytes[..3] != b"3DI" || footer_bytes[3..] != prefix[3..10] {
                return Ok((SniffedContainer::Unknown, id3_count, additional_id3_nonempty));
            }
        }
        id3_count += 1;
        position = end;
    }
}

fn sniff_container_prefix(prefix: &[u8]) -> SniffedContainer {
    use SniffedContainer::{Ogg, Unknown, Writable};
    if prefix.len() >= 12 && &prefix[..4] == b"RIFF" && &prefix[8..12] == b"WAVE" {
        Writable(tags::Format::Wav)
    } else if prefix.len() >= 12 && &prefix[..4] == b"FORM" && matches!(&prefix[8..12], b"AIFF" | b"AIFC") {
        Writable(tags::Format::Aiff)
    } else if prefix.starts_with(b"fLaC") {
        Writable(tags::Format::Flac)
    } else if prefix.len() >= 2 && prefix[0] == 0xff && prefix[1] & 0xe0 == 0xe0 {
        Writable(tags::Format::Mp3)
    } else if prefix.starts_with(b"OggS") {
        Ogg
    } else {
        Unknown
    }
}

// Inspect top-level atom headers without loading atom payloads. MP4 need not
// start with ftyp: free/wide/skip or other valid atoms may precede moov/mdat.
fn sniff_mp4_atoms(input: &mut fs::File, mut position: u64, length: u64) -> Result<bool, EngineError> {
    let map_io = |e| EngineError::new("sniff source container", e);
    let mut movie = false;
    let mut media = false;
    for _ in 0..1_000_000 {
        if position == length { return Ok(movie && media); }
        if length - position < 8 { return Ok(false); }
        input.seek(SeekFrom::Start(position)).map_err(map_io)?;
        let mut header = [0u8; 16];
        input.read_exact(&mut header[..8]).map_err(map_io)?;
        let short = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, header_size) = match short {
            0 => (length - position, 8),
            1 if length - position >= 16 => {
                input.read_exact(&mut header[8..]).map_err(map_io)?;
                (u64::from_be_bytes(header[8..].try_into().unwrap()), 16)
            }
            1 => return Ok(false),
            n => (u64::from(n), 8),
        };
        if size < header_size || size > length - position { return Ok(false); }
        if &header[4..8] == b"moov" {
            let payload_size = size - header_size;
            if payload_size < 8 { return Ok(false); }
            input.seek(SeekFrom::Start(position + header_size)).map_err(map_io)?;
            let mut child = [0u8; 16];
            input.read_exact(&mut child[..8]).map_err(map_io)?;
            let (child_size, child_header) = match u32::from_be_bytes(child[..4].try_into().unwrap()) {
                0 => (payload_size, 8),
                1 if payload_size >= 16 => {
                    input.read_exact(&mut child[8..]).map_err(map_io)?;
                    (u64::from_be_bytes(child[8..].try_into().unwrap()), 16)
                }
                1 => return Ok(false),
                n => (u64::from(n), 8),
            };
            if child_size < child_header || child_size > payload_size { return Ok(false); }
            movie = true;
        }
        if &header[4..8] == b"mdat" { media = true; }
        position += size;
    }
    Ok(false)
}

// Only use this weaker signature for an MP3 extension or after a validated
// ID3 prefix. Three complete consecutive frames must agree on version, layer
// and sample rate; bitrate and padding may vary.
fn sniff_mpeg_frames(input: &mut fs::File, start: u64, length: u64) -> Result<bool, EngineError> {
    const WINDOW: usize = 64 * 1024;
    let map_io = |e| EngineError::new("sniff source container", e);
    let count = usize::try_from((length - start).min(WINDOW as u64)).unwrap();
    let mut bytes = [0u8; WINDOW];
    input.seek(SeekFrom::Start(start)).map_err(map_io)?;
    input.read_exact(&mut bytes[..count]).map_err(map_io)?;
    for candidate in 0..count.saturating_sub(3) {
        if bytes[candidate] != 0xff || bytes[candidate + 1] & 0xe0 != 0xe0 { continue; }
        let mut position = candidate;
        let mut signature = None;
        let mut frames = 0;
        while frames < 3 {
            let Some(header) = bytes.get(position..position + 4).filter(|_| position + 4 <= count) else { break; };
            let word = u32::from_be_bytes(header.try_into().unwrap());
            if word >> 21 != 0x7ff || (word >> 19) & 3 == 1 || word & 3 == 2 { break; }
            let current = word & 0x001e_0c00;
            if signature.is_some_and(|expected| expected != current) { break; }
            let size = crate::mp3::frame_size(word);
            if size < 4 || size > count - position { break; }
            signature = Some(current);
            position += size;
            frames += 1;
        }
        if frames == 3 { return Ok(true); }
    }
    Ok(false)
}

// The bridge can reject an otherwise decodable truncated RIFF/FORM. Only admit
// it when a bounded walk of the physically present chunks proves that none
// can be a PN marker. A truncated final audio payload is safe to clamp; a
// truncated header/metadata chunk or trailing garbage is not.
fn verify_unavailable_riff_marker_absent(path: &Path, family: tags::Format, reason: &str) -> Result<(), EngineError> {
    let refuse = |detail: &str| EngineError::new("inspect tags",
        tags::TagError::SourceContainerUnavailable(format!("{reason}; cannot establish marker absence: {detail}")));
    let io_error = |source| EngineError::new("inspect tags", tags::TagError::Io { operation: "scan source marker", source });
    let mut input = fs::File::open(path).map_err(io_error)?;
    let physical_end = input.metadata().map_err(io_error)?.len();
    let mut header = [0u8; 12];
    input.read_exact(&mut header).map_err(io_error)?;
    if sniff_container_prefix(&header) != SniffedContainer::Writable(family) {
        return Err(refuse("container changed during inspection"));
    }
    let wav = family == tags::Format::Wav;
    let declared_size = if wav { u32::from_le_bytes(header[4..8].try_into().unwrap()) }
        else { u32::from_be_bytes(header[4..8].try_into().unwrap()) };
    let declared_end = u64::from(declared_size) + 8;
    if declared_end < 12 || declared_end < physical_end {
        return Err(refuse("invalid container length or trailing bytes"));
    }
    let end = declared_end.min(physical_end);
    let mut position = 12u64;
    let mut chunks = 0usize;
    let mut format_seen = false;
    let mut audio_seen = false;
    while position < end {
        if chunks == 1_000_000 { return Err(refuse("chunk walk limit exceeded")); }
        chunks += 1;
        if end - position < 8 { return Err(refuse("truncated chunk header")); }
        input.seek(SeekFrom::Start(position)).map_err(io_error)?;
        let mut chunk = [0u8; 8];
        input.read_exact(&mut chunk).map_err(io_error)?;
        let id = &chunk[..4];
        if !id.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
            return Err(refuse("invalid chunk identifier"));
        }
        if matches!(id, b"PLTN" | b"ID3 " | b"id3 ") {
            return Err(refuse("a possible PLTN or ID3 processing marker is present"));
        }
        let size = if wav { u32::from_le_bytes(chunk[4..8].try_into().unwrap()) }
            else { u32::from_be_bytes(chunk[4..8].try_into().unwrap()) } as u64;
        let body = position + 8;
        let available = end - body;
        if (wav && id == b"fmt ") || (!wav && id == b"COMM") {
            let minimum = if wav { 16 } else { 18 };
            if size < minimum || available < minimum { return Err(refuse("truncated format chunk")); }
            format_seen = true;
        }
        let audio = (wav && id == b"data") || (!wav && id == b"SSND");
        if audio {
            if !wav && (size < 8 || available < 8) { return Err(refuse("truncated sound header")); }
            audio_seen = true;
        }
        if size > available {
            if !audio { return Err(refuse("truncated non-audio chunk")); }
            position = end;
            break;
        }
        position = body + size;
        if size & 1 != 0 {
            if position == end { return Err(refuse("missing chunk padding")); }
            position += 1;
        }
    }
    if position != end || !format_seen || !audio_seen {
        return Err(refuse("incomplete audio chunk walk"));
    }
    Ok(())
}
fn source_codec(path: &Path, stream: StreamDescription) -> SourceCodec {
    if matches!(file_workflow::source_type(path), Some(SourceType::Flac)) { return SourceCodec::Flac; }
    match stream.format_id {
        n if n == u32::from_be_bytes(*b"lpcm") => SourceCodec::Lpcm,
        n if n == u32::from_be_bytes(*b".mp3") => SourceCodec::Mp3,
        n if n == u32::from_be_bytes(*b"aac ") => SourceCodec::Aac,
        n if n == u32::from_be_bytes(*b"alac") => SourceCodec::Alac,
        n if n == u32::from_be_bytes(*b"flac") => SourceCodec::Flac,
        n => SourceCodec::Other(n),
    }
}
fn format_label(path: &Path, codec: SourceCodec) -> String {
    let container = match file_workflow::source_type(path) {
        Some(SourceType::Mp3) => "mp3", Some(SourceType::M4a) => "m4a", Some(SourceType::Wav) => "wav",
        Some(SourceType::Aiff) => "aiff", Some(SourceType::Flac) => "flac", _ => "unknown",
    };
    let codec = match codec {
        SourceCodec::Lpcm => "pcm".to_owned(), SourceCodec::Mp3 => "mp3".to_owned(), SourceCodec::Aac => "aac".to_owned(),
        SourceCodec::Alac => "alac".to_owned(), SourceCodec::Flac => "flac".to_owned(), SourceCodec::Other(n) => format!("0x{n:08x}"),
    };
    format!("{container}/{codec}")
}

// TagLib's AIFF parser selects the first ID3 chunk even if later chunks exist.
// Walk the physical chunks, as TagLib does, before trusting that selection.
fn refuse_duplicate_aiff_id3(path: &Path) -> Result<(), EngineError> {
    let map_io = |e| EngineError::new("read marker", e);
    let invalid = |reason| EngineError::new("read marker", reason);
    let mut input = fs::File::open(path).map_err(map_io)?;
    let length = input.metadata().map_err(map_io)?.len();
    let mut header = [0u8; 12];
    input.read_exact(&mut header).map_err(map_io)?;
    if sniff_container_prefix(&header) != SniffedContainer::Writable(tags::Format::Aiff) {
        return Err(invalid("AIFF container changed during marker inspection"));
    }
    let declared_end = u64::from(u32::from_be_bytes(header[4..8].try_into().unwrap())) + 8;
    if declared_end < 12 || declared_end > length {
        return Err(invalid("invalid or truncated AIFF FORM length"));
    }
    let mut position = 12u64;
    let mut chunks = 0usize;
    let mut id3_count = 0u8;
    while position < length {
        if chunks == 1_000_000 { return Err(invalid("AIFF chunk walk limit exceeded")); }
        chunks += 1;
        if length - position < 8 { return Err(invalid("truncated AIFF chunk header")); }
        input.seek(SeekFrom::Start(position)).map_err(map_io)?;
        let mut chunk = [0u8; 8];
        input.read_exact(&mut chunk).map_err(map_io)?;
        if matches!(&chunk[..4], b"ID3 " | b"id3 ") {
            id3_count += 1;
            if id3_count > 1 {
                return Err(EngineError::new("decode marker", "multiple AIFF ID3 chunks are refused; a processed marker may be hidden"));
            }
        }
        let size = u64::from(u32::from_be_bytes(chunk[4..].try_into().unwrap()));
        let body = position + 8;
        if size > length - body { return Err(invalid("truncated AIFF chunk")); }
        position = body + size;
        // TagLib advances over an odd-byte pad only when it is zero.
        if position & 1 != 0 && position < length {
            input.seek(SeekFrom::Start(position)).map_err(map_io)?;
            let mut pad = [0u8; 1];
            input.read_exact(&mut pad).map_err(map_io)?;
            if pad[0] == 0 { position += 1; }
        }
    }
    Ok(())
}

fn read_marker_payload(bridge: &TagBridge, path: &Path, format: tags::Format) -> Result<Option<Vec<u8>>, EngineError> {
    if format == tags::Format::Aiff { refuse_duplicate_aiff_id3(path)?; }
    bridge.read_processing_marker(path, format).map_err(|e| EngineError::new("read marker", e))
}
fn read_marker(bridge: &TagBridge, path: &Path, format: tags::Format) -> Result<Option<Marker>, EngineError> {
    read_marker_payload(bridge, path, format)?
        .map(|payload| Marker::from_encoded_bytes(&payload).map_err(|e| EngineError::new("decode marker", e))).transpose()
}

enum InspectedMarker {
    Known(Option<Marker>),
    AuthorizedAmbiguous(&'static str),
}

fn read_process_marker(bridge: &TagBridge, path: &Path, format: tags::Format, allow: bool) -> Result<InspectedMarker, EngineError> {
    let payload = read_marker_payload(bridge, path, format)?;
    let layout = if format == tags::Format::Wav {
        let mut input = fs::File::open(path).map_err(|e| EngineError::new("read marker", e))?;
        Some(metadata::wav_marker_layout(&mut input).map_err(|e| EngineError::new("read marker", e))?)
    } else {
        None
    };
    admit_process_marker(payload.as_deref(), format, allow, layout)
}

fn admit_process_marker(
    payload: Option<&[u8]>, format: tags::Format, allow: bool, layout: Option<metadata::WavMarkerLayout>,
) -> Result<InspectedMarker, EngineError> {
    if format == tags::Format::Wav {
        let layout = layout.ok_or_else(|| EngineError::new("read marker", "missing WAV marker layout"))?;
        if layout.marker_count > 1 {
            return Err(EngineError::new("decode marker", "multiple WAV PLTN chunks are refused for processing; a processed marker may be hidden"));
        }
        if layout.trailing_marker_risk {
            return Err(EngineError::new("decode marker", "possible PLTN byte sequence or partial identifier overlapping or after the declared RIFF end; processing refused"));
        }
        if (layout.marker_count == 0) != payload.is_none() {
            return Err(EngineError::new("read marker", "WAV marker layout changed during inspection"));
        }
    }
    let marker = decode_process_marker(payload, format, allow)?;
    if matches!(&marker, InspectedMarker::AuthorizedAmbiguous(_)) {
        let layout = layout.ok_or_else(|| EngineError::new("read marker", "missing WAV marker layout"))?;
        if layout.marker_count != 1 {
            return Err(EngineError::new("decode marker", "ambiguous-marker override requires exactly one WAV PLTN chunk"));
        }
        if layout.trailing_bytes {
            return Err(EngineError::new("decode marker", "ambiguous-marker override refuses bytes after the declared RIFF end"));
        }
    }
    Ok(marker)
}

fn decode_process_marker(payload: Option<&[u8]>, format: tags::Format, allow: bool) -> Result<InspectedMarker, EngineError> {
    let Some(payload) = payload else { return Ok(InspectedMarker::Known(None)); };
    let parsed = if format == tags::Format::Wav {
        metadata::marker_for_wav_processing(payload)
    } else {
        Marker::from_encoded_bytes(payload).map_err(|error| (error, None))
    };
    match parsed {
        Ok(marker) => Ok(InspectedMarker::Known(Some(marker))),
        Err((error, ambiguous)) => {
            match ambiguous {
                Some(kind) if allow => Ok(InspectedMarker::AuthorizedAmbiguous(kind)),
                Some(kind) => Err(EngineError::coded("decode marker", ErrorCode::MarkerAmbiguous, format!(
                    "{error}; ambiguous WAV marker ({kind}) requires --allow-ambiguous-marker for processing, subject to other safeguards; this may reprocess an already processed file"
                ))),
                None => Err(EngineError::new("decode marker", error)),
            }
        }
    }
}

/// Read only the marker and retain the identity captured before the read.
/// Restore does not need an audio decoder or a full metadata inspection.
pub fn read_restore_marker(path: &Path, settings: &Settings) -> Result<(FileIdentity, Option<Marker>), EngineError> {
    require_reportable_path(path, "inspect input")?;
    let identity = FileIdentity::capture(path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let family = source_tag_format(path)?;
    let marker = read_marker(&bridge(settings)?, path, family)?;
    identity.verify(path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    Ok((identity, marker))
}
fn require_reportable_path(path: &Path, stage: &'static str) -> Result<(), EngineError> {
    if path.to_str().is_none() {
        return Err(EngineError::new(stage, format!("path cannot be represented in JSON: {}", path.display())));
    }
    Ok(())
}
fn marker_json(marker: Option<&Marker>) -> Result<Option<serde_json::Value>, EngineError> {
    let Some(marker) = marker else { return Ok(None) };
    let bytes = marker.json_bytes().map_err(|e| EngineError::new("serialize marker", e))?
        .ok_or_else(|| EngineError::new("serialize marker", "Foundation rejected the existing marker"))?;
    Ok(Some(serde_json::from_slice(&bytes).map_err(|e| EngineError::new("serialize marker", e))?))
}

enum InspectionOutcome {
    Known(InspectionReport),
    AuthorizedAmbiguous { kind: &'static str, source_codec: SourceCodec, input_format: String, tags_unavailable: Option<String> },
}

// `process_override` is None for strict inspection and Some for process
// preflight. An authorized ambiguity never becomes a public inspection report.
fn inspect_file_with_policy(path: &Path, settings: &Settings, process_override: Option<bool>) -> Result<InspectionOutcome, EngineError> {
    require_reportable_path(path, "inspect input")?;
    let identity = file_workflow::FileIdentity::capture(path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let family = source_tag_format(path)?;
    let bridge = bridge(settings)?;
    let (detected, id3_count, additional_id3_nonempty) = sniff_container(path, family)?;
    if detected == SniffedContainer::Unknown {
        return Err(EngineError::new("inspect tags",
            tags::TagError::SourceContainerUnavailable("cannot establish marker absence for an unknown container".into())));
    }
    if additional_id3_nonempty {
        return Err(EngineError::new("decode marker", "additional nonempty leading ID3 tags are refused; a processed marker may be hidden"));
    }
    if id3_count != 0 && detected == SniffedContainer::Ogg {
        return Err(EngineError::new("decode marker", "an ID3 prefix on an Ogg source may hide a processed marker"));
    }
    let (tag_inspection, tags_unavailable, marker_family) = match bridge.inspect(path, family) {
        Ok(tags) => {
            let marker_family = match detected {
                SniffedContainer::Writable(actual) => actual,
                SniffedContainer::Ogg | SniffedContainer::Unknown => family,
            };
            (Some(tags), None, Some(marker_family))
        }
        Err(tags::TagError::SourceContainerUnavailable(reason)) => match detected {
            SniffedContainer::Writable(actual) if actual == family && matches!(actual, tags::Format::Wav | tags::Format::Aiff) => {
                verify_unavailable_riff_marker_absent(path, actual, &reason)?;
                (None, Some(reason), None)
            }
            SniffedContainer::Writable(actual) if actual == family =>
                return Err(EngineError::new("inspect tags", tags::TagError::SourceContainerUnavailable(reason))),
            SniffedContainer::Writable(actual) => (None, Some(reason), Some(actual)),
            SniffedContainer::Ogg => (None, Some(reason), None),
            SniffedContainer::Unknown => return Err(EngineError::new("inspect tags",
                tags::TagError::SourceContainerUnavailable(format!("{reason}; cannot establish marker absence for an unknown container")))),
        },
        Err(error) => return Err(EngineError::new("inspect tags", error)),
    };
    let marker = match marker_family {
        Some(format) => match process_override {
            Some(allow) => read_process_marker(&bridge, path, format, allow)?,
            None => InspectedMarker::Known(read_marker(&bridge, path, format)?),
        },
        None => InspectedMarker::Known(None),
    };
    let state = match &marker {
        InspectedMarker::Known(marker) => Some(file_workflow::processed_state(path, marker.as_ref()).map_err(|e| EngineError::workflow("inspect marker", e))?),
        InspectedMarker::AuthorizedAmbiguous(_) => None,
    };
    let (processing_state, backup_path) = match &state {
        Some(ProcessingState::Unprocessed) => (Some("unprocessed"), None),
        Some(ProcessingState::Suffixed) => (Some("suffixed"), None),
        Some(ProcessingState::Marked { backup }) => (Some("marked"), backup.clone()),
        None => (None, None),
    };
    let (backup_exists, backup_error) = match &backup_path {
        None => (None, None),
        Some(path) => match fs::symlink_metadata(path) {
            Ok(_) => (Some(true), None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Some(false), None),
            Err(e) => (None, Some(format!("{}: {e}", path.display()))),
        },
    };
    let reader = AudioReader::open(path, None, &input_libraries(settings)).map_err(|e| EngineError::new("open input", e))?;
    let source = reader.source_format();
    let codec = source_codec(path, source);
    let non_identical = reader.non_identical();
    let frames = reader.file_length_frames().map_err(|e| EngineError::new("inspect length", e))?;
    if frames < 0 { return Err(EngineError::new("inspect length", "decoder returned a negative frame count")); }
    let (bitrate, bitrate_error) = match reader.bitrate() { Ok(b) => (Some(b), None), Err(e) => (None, Some(e.to_string())) };
    reader.close().map_err(|e| EngineError::new("close input", e))?;
    identity.verify(path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let input_format = format_label(path, codec);
    match marker {
        InspectedMarker::Known(marker) => {
            let marker_json = marker_json(marker.as_ref())?;
            let state = state.expect("known marker has a processing state");
            let processed = Some(state != ProcessingState::Unprocessed);
            Ok(InspectionOutcome::Known(InspectionReport { path: path.to_owned(), input_format, sample_rate: source.sample_rate,
                channels: source.channels_per_frame, bit_depth: source.bits_per_channel, frames: frames as u64,
                bitrate, bitrate_error, processed, non_identical_paths: non_identical_paths(non_identical, false),
                processing_state: processing_state.expect("known marker has a processing state"),
                backup_path, backup_exists, backup_error, marker_json, tags: tag_inspection, tags_unavailable, marker, source_codec: codec }))
        }
        InspectedMarker::AuthorizedAmbiguous(kind) => Ok(InspectionOutcome::AuthorizedAmbiguous {
            kind, source_codec: codec, input_format, tags_unavailable,
        }),
    }
}

/// Inspect using the actual decoder, container tag reader and decoded marker.
/// Absence and a malformed present marker are distinct outcomes.
pub fn inspect_file(path: &Path, settings: &Settings) -> Result<InspectionReport, EngineError> {
    match inspect_file_with_policy(path, settings, None)? {
        InspectionOutcome::Known(report) => Ok(report),
        InspectionOutcome::AuthorizedAmbiguous { .. } => Err(EngineError::coded("inspect marker", ErrorCode::MarkerAmbiguous, "ambiguous marker cannot be inspected")),
    }
}

/// A plan and the source identity and codec on which it was based.
/// Keep this intact through batch validation and worker dispatch.
#[derive(Debug)]
pub struct PreparedProcess {
    plan: OutputPlan,
    identity: FileIdentity,
    source_codec: SourceCodec,
    input_format: String,
    input_marker_override: Option<&'static str>,
    source_tags_unavailable: Option<String>,
    controlled_mp4_time_1904: Option<u32>,
}
impl PreparedProcess {
    pub fn output_plan(&self) -> &OutputPlan { &self.plan }
    pub fn input_marker_override(&self) -> Option<&'static str> { self.input_marker_override }
    /// Authorize overwrite using a shared guard for the complete selected batch.
    pub fn enable_overwrite(&mut self, guard: std::sync::Arc<file_workflow::OverwriteGuard>) -> Result<(), EngineError> {
        self.plan.enable_overwrite(guard).map_err(|e| EngineError::workflow("plan overwrite", e))
    }
    /// Diagnostic clock control: AAC/ALAC outputs store `seconds_since_1904`
    /// in all MP4 header time fields. Other formats are unaffected.
    #[doc(hidden)]
    pub fn with_controlled_mp4_time(mut self, seconds_since_1904: u32) -> Self {
        self.controlled_mp4_time_1904 = Some(seconds_since_1904);
        self
    }
}

/// Inspect and plan a source for later batch validation and processing.
/// An explicit override accepts an ambiguous WAV marker and may reprocess a
/// previously processed file; valid processed markers are still refused.
pub fn prepare_file(input: &DiscoveredFile, settings: &Settings, allow_ambiguous_marker: bool) -> Result<PreparedProcess, EngineError> {
    let identity = FileIdentity::capture(&input.path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let inspection = inspect_file_with_policy(&input.path, settings, Some(allow_ambiguous_marker))?;
    identity.verify(&input.path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let (plan, source_codec, input_format, input_marker_override, source_tags_unavailable) = match inspection {
        InspectionOutcome::Known(inspection) => {
            let plan = OutputPlan::new(input, inspection.source_codec, &settings.output, inspection.marker.as_ref())
                .map_err(|e| EngineError::workflow("plan output", e))?;
            (plan, inspection.source_codec, inspection.input_format, None, inspection.tags_unavailable)
        }
        InspectionOutcome::AuthorizedAmbiguous { kind, source_codec, input_format, tags_unavailable } => {
            let plan = OutputPlan::for_authorized_ambiguous_marker(input, source_codec, &settings.output)
                .map_err(|e| EngineError::workflow("plan output", e))?;
            (plan, source_codec, input_format, Some(kind), tags_unavailable)
        }
    };
    identity.verify(&input.path).map_err(|e| EngineError::workflow("plan source identity", e))?;
    require_reportable_path(plan.source(), "plan output")?;
    require_reportable_path(plan.destination(), "plan output")?;
    if let Some(path) = plan.backup_directory() { require_reportable_path(path, "plan backup")?; }
    Ok(PreparedProcess { plan, identity, source_codec, input_format, input_marker_override, source_tags_unavailable, controlled_mp4_time_1904: None })
}

struct Measured {
    values: AnalysisValues,
    frames: i64,
    processing_rate: f64,
    source: StreamDescription,
    codec: SourceCodec,
    /// The open-source MP3 decoder produced the samples.
    non_identical: bool,
    waveform: Option<Waveform>,
    waveform_unavailable: Option<String>,
}

struct WaveformBuilder {
    path: PathBuf,
    rate: f64,
    channels: usize,
    total: u64,
    bins: Vec<WaveformBin>,
}
impl WaveformBuilder {
    fn new(path: &Path, rate: f64, channels: usize) -> Self {
        Self { path: path.to_owned(), rate, channels, total: 0, bins: Vec::new() }
    }
    fn feed(&mut self, samples: &[f32]) -> Result<(), EngineError> {
        let frames = samples.len() / self.channels;
        if frames == 0 { return Ok(()); }
        if self.bins.len() == self.bins.capacity() {
            self.bins.try_reserve(1).map_err(|error| EngineError::new("allocate waveform", error))?;
        }
        let left_channel = self.channels.saturating_sub(2);
        let right_channel = self.channels - 1;
        let mut left = 0.0_f32;
        let mut right = 0.0_f32;
        for frame in samples.chunks_exact(self.channels) {
            left += frame[left_channel].abs();
            right += frame[right_channel].abs();
        }
        self.bins.push(WaveformBin {
            start_frame: self.total, frames: frames as u32,
            left_mean_absolute: left / frames as f32,
            right_mean_absolute: right / frames as f32,
        });
        self.total += frames as u64;
        Ok(())
    }
    fn finish(self) -> Waveform {
        Waveform {
            path: self.path, requested_sample_rate: 44_100.0, sample_rate: self.rate,
            channels: self.channels as u32, layout: "per_read_last_two_channels_mean_absolute",
            frames: self.total, bins: self.bins,
        }
    }
}
fn analyze_waveform(path: &Path, settings: &Settings, cancel: &Cancellation) -> Result<Waveform, EngineError> {
    check_cancel(cancel, "waveform")?;
    let mut reader = AudioReader::open(path, Some(44_100.0), &input_libraries(settings))
        .map_err(|error| EngineError::new("open waveform input", error))?;
    let source = reader.source_format();
    let client = reader.client_format();
    let channels = client.channels_per_frame as usize;
    if channels == 0 {
        return Err(EngineError::coded("waveform", ErrorCode::InvalidInputFile, "decoder has no channels"));
    }
    if source_codec(path, source) == SourceCodec::Flac && channels > 2 {
        return Err(EngineError::coded("waveform", ErrorCode::UnsupportedInputFormat, "native multichannel FLAC waveform reads an unwritten buffer suffix; no deterministic sample values are established"));
    }
    let mut waveform = WaveformBuilder::new(path, client.sample_rate, channels);
    let mut samples = vec![0.0; BLOCK.checked_mul(channels)
        .ok_or_else(|| EngineError::new("waveform", "channel count overflows"))?];
    loop {
        check_cancel(cancel, "waveform")?;
        let read = reader.read(&mut samples).map_err(|error| EngineError::new("read waveform input", error))?;
        if read == 0 { break; }
        waveform.feed(&samples[..read * channels])?;
    }
    reader.close().map_err(|error| EngineError::new("close waveform input", error))?;
    check_cancel(cancel, "waveform")?;
    Ok(waveform.finish())
}
fn check_cancel(cancel: &Cancellation, stage: &'static str) -> Result<(), EngineError> {
    if cancel.is_cancelled() { Err(EngineError::cancelled(stage)) } else { Ok(()) }
}
fn analyze(path: &Path, settings: &Settings, cancel: &Cancellation, capture_waveform: bool) -> Result<Measured, EngineError> {
    check_cancel(cancel, "analysis")?;
    let libraries = input_libraries(settings);
    let original = AudioReader::open(path, None, &libraries).map_err(|e| EngineError::new("open analysis input", e))?;
    let source = original.source_format();
    let codec = source_codec(path, source);
    let non_identical = original.non_identical();
    let rate = controller::processing_sample_rate(source.sample_rate, codec == SourceCodec::Mp3);
    original.close().map_err(|e| EngineError::new("close analysis input", e))?;
    #[cfg(any(target_os = "linux", feature = "portable"))]
    if codec != SourceCodec::Mp3 && source.sample_rate <= 22_050.0 {
        return Err(EngineError::new("analysis sample-rate conversion",
            apple_codec::CodecError::UnsupportedSampleRateConversion { source_rate: source.sample_rate, client_rate: rate }));
    }
    let mut reader = AudioReader::open(path, Some(rate), &libraries).map_err(|e| EngineError::new("open analysis input", e))?;
    reader.set_stereo_planar(rate).map_err(|e| EngineError::new("configure analysis input", e))?;
    let channels = source.channels_per_frame as usize;
    if channels == 0 { return Err(EngineError::coded("analysis", ErrorCode::InvalidInputFile, "decoder has no channels")); }
    let mut meter = LoudnessMeter::new(rate, channels).map_err(|e| EngineError::new("analysis meter", e))?;
    let mut detector = PitchDetector::new(rate as f32, channels).map_err(|e| EngineError::new("analysis pitch detector", e))?;
    let mut accounting = FrameAccounting::new(RenderPass::Analysis, None);
    let mut samples = vec![0.0; BLOCK.checked_mul(channels).ok_or_else(|| EngineError::new("analysis", "channel count overflows"))?];
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut converter = ChannelConverter::new(rate, channels)?;
    loop {
        left.fill(0.0); right.fill(0.0);
        let read = reader.read_stereo_planar(&mut left, &mut right).map_err(|e| EngineError::new("read analysis input", e))?;
        converter.convert(&left, &right, &mut samples)?;
        let source_read = accounting.source_read(BLOCK as u32, read as u32).map_err(|e| EngineError::new("analysis accounting", format!("{e:?}")))?;
        meter.process_interleaved(&samples).map_err(|e| EngineError::new("analysis meter", e))?;
        detector.process_interleaved(&samples).map_err(|e| EngineError::new("analysis pitch detector", e))?;
        let flags = RENDER_POST | if source_read.signal_silence { RENDER_SILENCE } else { 0 };
        if accounting.finish_render(BLOCK as u32, flags, cancel.is_cancelled()) { break; }
    }
    reader.close().map_err(|e| EngineError::new("close analysis input", e))?;
    converter.dispose()?;
    check_cancel(cancel, "analysis")?;
    let waveform_unavailable = if capture_waveform && cfg!(any(target_os = "linux", feature = "portable"))
        && source.sample_rate != 44_100.0 {
        Some("portable waveform conversion is unavailable for non-44.1 kHz input".to_owned())
    } else { None };
    let waveform = if capture_waveform && waveform_unavailable.is_none() {
        Some(analyze_waveform(path, settings, cancel)?)
    } else { None };
    Ok(Measured { values: AnalysisValues::from_measurements(meter.integrated_loudness(), detector.tuning_hz()),
        frames: accounting.actual_input_frames, processing_rate: rate, source, codec, non_identical, waveform, waveform_unavailable })
}

pub fn analyze_file(path: &Path, settings: &Settings, cancel: &Cancellation) -> Result<AnalysisReport, EngineError> {
    analyze_file_with(path, settings, cancel, |path, settings, cancel| analyze(path, settings, cancel, true))
}

fn analyze_file_with(
    path: &Path, settings: &Settings, cancel: &Cancellation,
    analyze_original: impl FnOnce(&Path, &Settings, &Cancellation) -> Result<Measured, EngineError>,
) -> Result<AnalysisReport, EngineError> {
    let identity = file_workflow::FileIdentity::capture(path).map_err(|e| EngineError::workflow("analyze source identity", e))?;
    let inspection = inspect_file(path, settings)?;
    let state = file_workflow::processed_state(path, inspection.marker.as_ref()).map_err(|e| EngineError::workflow("analyze marker", e))?;
    let marked = inspection.marker.as_ref().filter(|m| m.is_processed);
    let clip_count = marked.and_then(|m| u64::try_from(m.clip_repair_count).ok());
    let dynamics = marked.map(|m| ReportedDynamics { source: "processing_marker",
        expansion_applied: m.applied_expansion, compression_applied: m.applied_compression });
    let mut measured = analyze(path, settings, cancel, true)?;
    identity.verify(path).map_err(|e| EngineError::workflow("analyze source identity", e))?;
    let (original_waveform, original_waveform_unavailable, original_non_identical) = match state {
        ProcessingState::Marked { backup: Some(path) } => match require_reportable_path(&path, "analyze original")
            .and_then(|()| {
                let identity = file_workflow::FileIdentity::capture(&path).map_err(|e| EngineError::workflow("inspect original identity", e))?;
                let original = analyze_original(&path, settings, cancel)?;
                identity.verify(&path).map_err(|e| EngineError::workflow("analyze original identity", e))?;
                Ok(original)
            }) {
                // Only the backup's waveform reaches the report: an unavailable waveform means its decode produced nothing.
                Ok(original) => { let produced = original.non_identical && original.waveform.is_some();
                                  (original.waveform, original.waveform_unavailable, produced) }
                Err(error) if error.cancelled => return Err(error),
                Err(error) => (None, Some(error.to_string()), false),
            },
        ProcessingState::Marked { backup: None } => (None, Some("processing marker does not record a backup path".to_owned()), false),
        ProcessingState::Suffixed => (None, Some("suffix-only processed state does not identify an original".to_owned()), false),
        ProcessingState::Unprocessed => (None, None, false),
    };
    let unavailable = if marked.is_some() { vec!["new clip and dynamics measurements require the healing pass".to_owned()] }
        else { vec!["clip-repair count and dynamics adjustments require the healing pass".to_owned()] };
    identity.verify(path).map_err(|e| EngineError::workflow("analyze source identity", e))?;
    Ok(AnalysisReport { path: path.to_owned(), input_format: inspection.input_format, sample_rate: measured.processing_rate,
        channels: measured.source.channels_per_frame, frames: measured.frames as u64, volume: measured.values.input_level_db,
        pitch_hz: measured.values.input_tuning_hz, clip_count, processed: inspection.processed,
        processing_marker: inspection.marker_json, dynamics,
        unavailable,
        waveform: measured.waveform.take(), waveform_unavailable: measured.waveform_unavailable.take(),
        original_waveform, original_waveform_unavailable,
        non_identical_paths: non_identical_paths(measured.non_identical || original_non_identical, false) })
}

/// One wrapper: model writes precede the first callback. The native 50 ms
/// warmup traverses both the hosted processor and wrapper phase correction.
struct MultipassUnit {
    graph: MultipassGraph,
    phase: PhaseCorrection,
}
impl MultipassUnit {
    fn new(rate: f64, channels: usize, preset: ShippedPreset, flags: [bool; 5], writes: &[ParameterWrite]) -> Result<Self, EngineError> {
        let mut graph = MultipassGraph::new(rate as f32, preset).map_err(|e| EngineError::new("create multipass", e))?;
        let scale = CrossoverFrequencyScale::new();
        let mut parameters = preset.snapshot().crossover_hz.map(|hz| {
            let editor_value = scale.from_normalized(scale.to_normalized(hz));
            scale.to_normalized(editor_value)
        });
        for write in writes {
            graph.set_parameter(write.id, write.value).map_err(|e| EngineError::new("configure multipass", e))?;
            if write.id < 4 { parameters[write.id as usize] = write.value; }
        }
        let phase = PhaseCorrection::new(rate as f32, flags, parameters, channels);
        let mut unit = Self { graph, phase };
        let mut remaining = (rate * 0.05) as usize;
        let mut left = [0.0; BLOCK];
        let mut right = [0.0; BLOCK];
        while remaining != 0 {
            let n = remaining.min(BLOCK);
            left[..n].fill(0.0);
            right[..n].fill(0.0);
            unit.host_callback(&mut left[..n], &mut right[..n])?;
            // The original warmup passes one scratch pointer to every channel.
            // Phase channel 1 therefore consumes channel 0's output in order.
            unit.phase.process_channel(0, &mut left[..n]);
            unit.phase.process_channel(1, &mut left[..n]);
            remaining -= n;
        }
        Ok(unit)
    }
    fn host_callback(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<(), EngineError> {
        let frames = left.len();
        if frames == 0 { return Ok(()); }
        let chunks = frames.div_ceil(64);
        with_flush_to_zero(|| {
            for k in 0..chunks {
                let start = k * frames / chunks;
                let end = (k + 1) * frames / chunks;
                let threshold = f32::from_bits(0x3580_0000);
                let silent = left[start..end].iter().chain(&right[start..end])
                    .all(|&x| x > -threshold && x < threshold);
                let mut context = GraphContext { silent, bypass: false, segment_remaining: frames - start };
                self.graph.process_chunk(&mut context, &mut left[start..end], &mut right[start..end])
                    .map_err(|e| EngineError::new("render multipass", e))?;
            }
            Ok(())
        })
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<(), EngineError> {
        self.host_callback(left, right)?;
        self.phase.process_channel(0, left);
        self.phase.process_channel(1, right);
        Ok(())
    }
    fn latency_seconds(&self, rate: f64) -> f64 {
        (self.graph.latency_frames() as f64 + self.phase.latency_frames() as f64) / rate
    }
}

struct FinalLimiter {
    model: LimiterModel,
    dsp: LimiterDsp,
}
impl FinalLimiter {
    fn new(rate: f64, plan: &FinalPlan) -> Result<Self, EngineError> {
        let mut model = LimiterModel::new();
        let dsp = LimiterDsp::new(rate as f32, &mut model).map_err(|e| EngineError::new("create limiter", format!("{e:?}")))?;
        model.parameters.dry_latency_frames = dsp.latency_frames() as i32;
        let writes: Vec<LimiterValue> = plan.writes.iter().map(|w| model.decode_editor_parameter(w.id, w.value)
            .ok_or_else(|| EngineError::new("configure limiter", format!("unknown parameter {}", w.id)))).collect::<Result<_, _>>()?;
        let mut result = Self { model, dsp };
        let mut remaining = (rate * 0.05) as usize;
        let mut left = [0.0; BLOCK];
        let mut right = [0.0; BLOCK];
        let mut first = true;
        while remaining != 0 {
            let n = remaining.min(BLOCK);
            left[..n].fill(0.0); right[..n].fill(0.0);
            result.process_host(&mut left[..n], &mut right[..n], if first { &writes } else { &[] })?;
            first = false;
            remaining -= n;
        }
        if first {
            with_flush_to_zero(|| for value in writes { result.model.apply_editor_value(value) });
        }
        Ok(result)
    }
    fn process_host(&mut self, left: &mut [f32], right: &mut [f32], writes: &[LimiterValue]) -> Result<(), EngineError> {
        let frames = left.len();
        if frames == 0 { return Ok(()); }
        let chunks = frames.div_ceil(64);
        with_flush_to_zero(|| {
            for &value in writes { self.model.apply_editor_value(value); }
            for k in 0..chunks {
                let start = k * frames / chunks;
                let end = (k + 1) * frames / chunks;
                let threshold = f32::from_bits(0x3580_0000);
                let silent = left[start..end].iter().chain(&right[start..end])
                    .all(|&x| x > -threshold && x < threshold);
                let mut context = LimiterContext { silent, bypass: false };
                self.dsp.process_chunk(&mut self.model, &mut context, &mut left[start..end], &mut right[start..end])
                    .map_err(|e| EngineError::new("render limiter", format!("{e:?}")))?;
            }
            Ok(())
        })
    }
}

struct SourcePipeline {
    reader: AudioReader,
    clip: Vec<ClipRepair>,
    scratch: [f32; BLOCK],
    accounting: FrameAccounting,
}
impl SourcePipeline {
    fn new(reader: AudioReader, rate: f64, plan: &HealingPlan, recorder: RecorderAccounting) -> Result<Self, EngineError> {
        let mut clip = Vec::new();
        if let Some(parameters) = plan.clip {
            for _ in 0..2 {
                clip.push(ClipRepair::new(rate, parameters).map_err(|e| EngineError::new("create clip repair", e))?);
            }
        }
        Ok(Self { reader, clip, scratch: [0.0; BLOCK],
            accounting: FrameAccounting::new(RenderPass::Healing, Some(recorder)) })
    }
    fn pull(&mut self, planes: &mut [&mut [f32]]) -> Result<bool, EngineError> {
        let n = planes.first().map_or(0, |p| p.len());
        if n == 0 || n > BLOCK || planes.len() != 2 || planes.iter().any(|p| p.len() != n) {
            return Err(EngineError::new("source pull", "invalid stereo planar request"));
        }
        for plane in planes.iter_mut() { plane.fill(0.0); }
        let (left, right) = planes.split_at_mut(1);
        let read = self.reader.read_stereo_planar(left[0], right[0]).map_err(|e| EngineError::new("read healing input", e))?;
        let result = self.accounting.source_read(n as u32, read as u32).map_err(|e| EngineError::new("healing accounting", format!("{e:?}")))?;
        for (repair, plane) in self.clip.iter_mut().zip(planes.iter_mut()) {
            repair.process(plane, &mut self.scratch[..n]).map_err(|e| EngineError::new("clip repair", e))?;
            plane.copy_from_slice(&self.scratch[..n]);
        }
        Ok(result.signal_silence)
    }
    fn clip_count(&self) -> u64 {
        self.clip.iter().map(|c| c.event_times().len() as u64).sum()
    }
}

// The intermediate is an owned, mode-0700 temporary directory containing a
// float32 CAF. AudioToolbox performs the same file and client format handling
// as the observed PNCAFRecorder, without storing a track in memory.
struct TempCaf {
    workspace: file_workflow::Workspace,
    path: PathBuf,
    #[cfg(test)]
    directory: PathBuf,
}
impl TempCaf {
    fn new() -> Result<Self, EngineError> {
        let workspace = file_workflow::Workspace::create(&std::env::temp_dir(), "caf")
            .map_err(|e| EngineError::workflow("create intermediate directory", e))?;
        Ok(Self { path: workspace.staging_path().to_owned(),
            #[cfg(test)] directory: workspace.directory_path().to_owned(),
            workspace })
    }
    fn cleanup(&self) -> Result<(), EngineError> {
        let failures = self.workspace.cleanup(false);
        if failures.is_empty() { Ok(()) } else {
            Err(EngineError::new("remove intermediate", failures.iter()
                .map(|e| format!("{} {}: {}", e.operation, e.path.display(), e.source)).collect::<Vec<_>>().join("; ")))
        }
    }
}
impl Drop for TempCaf { fn drop(&mut self) { let _ = self.workspace.cleanup(false); } }

#[cfg(all(target_os = "macos", not(feature = "portable")))]
struct CafWriter { handle: Option<std::ptr::NonNull<std::ffi::c_void>>, channels: usize }
#[cfg(all(target_os = "macos", not(feature = "portable")))]
impl CafWriter {
    fn new(temp: &TempCaf, rate: f64, channels: usize) -> Result<Self, EngineError> {
        let _file = temp.workspace.create_stage().map_err(|e| EngineError::workflow("create intermediate", e))?;
        let path = &temp.path;
        let frame_bytes = u32::try_from(channels.checked_mul(4).ok_or_else(|| EngineError::new("create intermediate", "channel count overflows"))?)
            .map_err(|e| EngineError::new("create intermediate", e))?;
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let url = unsafe { CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 0) };
        if url.is_null() { return Err(EngineError::new("create intermediate", "CFURL creation failed")); }
        let format = StreamDescription { sample_rate: rate, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 9,
            bytes_per_packet: frame_bytes, frames_per_packet: 1, bytes_per_frame: frame_bytes,
            channels_per_frame: channels as u32, bits_per_channel: 32, reserved: 0 };
        let mut raw = std::ptr::null_mut();
        // Erase only the exclusively created file already pinned by TempCaf.
        let status = unsafe { ExtAudioFileCreateWithURL(url, u32::from_be_bytes(*b"caff"), &format, std::ptr::null(), 1, &mut raw) };
        unsafe { CFRelease(url); }
        let handle = std::ptr::NonNull::new(raw);
        if status != 0 || handle.is_none() {
            if let Some(handle) = handle { unsafe { ExtAudioFileDispose(handle.as_ptr()); } }
            return Err(EngineError::new("create intermediate", format!("ExtAudioFileCreateWithURL: OSStatus {status}")));
        }
        let mut writer = Self { handle, channels };
        if let Err(error) = temp.workspace.mark_stage() {
            let close = writer.dispose();
            return Err(EngineError::new("pin intermediate", format!("{error}; close: {close:?}")));
        }
        let status = unsafe { ExtAudioFileSetProperty(raw, u32::from_be_bytes(*b"cfmt"), std::mem::size_of::<StreamDescription>() as u32, (&format as *const StreamDescription).cast()) };
        if status != 0 {
            let close = writer.dispose();
            return Err(EngineError::new("configure intermediate", format!("OSStatus {status}; close: {close:?}")));
        }
        Ok(writer)
    }
    fn write(&mut self, samples: &[f32]) -> Result<(), EngineError> {
        if samples.len() % self.channels != 0 { return Err(EngineError::new("write intermediate", "incomplete frame")); }
        let frames = u32::try_from(samples.len() / self.channels).map_err(|e| EngineError::new("write intermediate", e))?;
        if frames == 0 { return Ok(()); }
        let bytes = u32::try_from(samples.len() * 4).map_err(|e| EngineError::new("write intermediate", e))?;
        let list = AudioBufferList { number_buffers: 1, buffers: [
            AudioBuffer { number_channels: self.channels as u32, data_byte_size: bytes, data: samples.as_ptr().cast_mut().cast() },
        ] };
        let status = unsafe { ExtAudioFileWrite(self.handle.expect("live intermediate writer").as_ptr(), frames, &list) };
        if status == 0 { Ok(()) } else { Err(EngineError::new("write intermediate", format!("OSStatus {status}"))) }
    }
    fn dispose(&mut self) -> Result<(), EngineError> {
        if let Some(handle) = self.handle.take() {
            let status = unsafe { ExtAudioFileDispose(handle.as_ptr()) };
            if status != 0 { return Err(EngineError::new("close intermediate", format!("OSStatus {status}"))); }
        }
        Ok(())
    }
    fn finish(mut self) -> Result<(), EngineError> { self.dispose() }
    fn abort(mut self) -> Result<(), EngineError> { self.dispose() }
}
#[cfg(all(target_os = "macos", not(feature = "portable")))]
impl Drop for CafWriter { fn drop(&mut self) { let _ = self.dispose(); } }

#[cfg(any(target_os = "linux", feature = "portable"))]
struct CafWriter(apple_codec::CafWriter);
#[cfg(any(target_os = "linux", feature = "portable"))]
impl CafWriter {
    fn new(temp: &TempCaf, rate: f64, channels: usize) -> Result<Self, EngineError> {
        let file = temp.workspace.create_stage().map_err(|e| EngineError::workflow("create intermediate", e))?;
        apple_codec::CafWriter::from_file(&temp.path, file, rate, channels).map(Self).map_err(|e| EngineError::new("create intermediate", e))
    }
    fn write(&mut self, samples: &[f32]) -> Result<(), EngineError> {
        self.0.write(samples).map_err(|e| EngineError::new("write intermediate", e))
    }
    fn finish(self) -> Result<(), EngineError> {
        self.0.finish().map_err(|e| EngineError::new("close intermediate", e))
    }
    fn abort(self) -> Result<(), EngineError> { drop(self); Ok(()) }
}

struct HealingResult {
    analyzer: GainAnalyzer,
    frames: u64,
    clip_count: u64,
    tempo_factor: f64,
    stages: Vec<String>,
}

fn create_healing_units(rate: f64, channels: usize, plan: &HealingPlan) -> Result<Vec<MultipassUnit>, EngineError> {
    let mut units = Vec::new();
    if let Some(p) = plan.warmth {
        let writes: Vec<_> = p.writes.into_iter().flatten().collect();
        units.push(MultipassUnit::new(rate, channels, p.preset, p.wrapper_flags, &writes)?);
    }
    for p in [plan.expansion, plan.compression].into_iter().flatten() {
        units.push(MultipassUnit::new(rate, channels, p.preset, p.wrapper_flags, &p.writes)?);
    }
    if let Some(p) = plan.gain {
        // A freshly constructed Gain wrapper has zero band flags. Its
        // controller does not write any of the phase-enable properties.
        units.push(MultipassUnit::new(rate, channels, p.preset, [false; 5], &p.writes)?);
    }
    Ok(units)
}
fn healing_pass(path: &Path, temp: &TempCaf, measured: &Measured, plan: &HealingPlan,
                replace: bool, settings: &Settings, cancel: &Cancellation) -> Result<HealingResult, EngineError> {
    let rate = measured.processing_rate;
    let channels = measured.source.channels_per_frame as usize;
    if !(1..=64).contains(&channels) {
        return Err(EngineError::coded("healing channel layout", ErrorCode::UnsupportedInputFormat, format!("unsupported channel count {channels}")));
    }
    // The effect graph negotiates stereo planar on every effect even when the
    // final GenericOutput and source container are mono or multichannel.
    let mut units = create_healing_units(rate, 2, plan)?;
    let mut latencies = Vec::new();
    if plan.clip.is_some() { latencies.push(Some(2560.0 / rate)); }
    if plan.pitch.is_some() { latencies.push(Some(0.0)); }
    latencies.extend(units.iter().map(|unit| Some(unit.latency_seconds(rate))));
    let latency = controller::graph_latency_frames(Some(rate), &latencies);
    let mut reader = AudioReader::open(path, Some(rate), &input_libraries(settings)).map_err(|e| EngineError::new("open healing input", e))?;
    reader.set_stereo_planar(rate).map_err(|e| EngineError::new("configure healing input", e))?;
    let mut source = SourcePipeline::new(reader, rate, plan, RecorderAccounting::for_healing(latency))?;
    let mut pitch = None;
    let mut tempo = 1.0;
    if let Some(p) = plan.pitch {
        let mode = if replace { PitchMode::PreserveTempo }
        else if p.requested_mode == 0.0 { PitchMode::PreserveTempo }
        else if p.requested_mode == 1.0 { PitchMode::Turntable }
        else { return Err(EngineError::new("configure pitch", format!("unsupported native pitch mode {}", p.requested_mode))); };
        let unit = PitchProcessor::new(rate as f32, 2, BLOCK, mode, p.requested_ratio)
            .map_err(|e| EngineError::new("create pitch", e))?;
        tempo = p.resolve(unit.ratio(), replace).tempo_factor;
        pitch = Some(unit);
    }
    let mut analyzer = GainAnalyzer::new(rate, channels, plan.analyzer_release_seconds, true)
        .map_err(|e| EngineError::new("create healing meter", e))?;
    let mut channel_converter = ChannelConverter::new(rate, channels)?;
    let mut writer = CafWriter::new(temp, rate, channels)?;
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut interleaved = vec![0.0f32; BLOCK * channels];
    let mut stages = Vec::new();
    if plan.clip.is_some() { stages.push("clip_repair".to_owned()); }
    if plan.pitch.is_some() { stages.push("pitch".to_owned()); }
    if plan.warmth.is_some() { stages.push("warmth".to_owned()); }
    if plan.expansion.is_some() { stages.push("expansion".to_owned()); }
    if plan.compression.is_some() { stages.push("compression".to_owned()); }
    if plan.gain.is_some() { stages.push("gain".to_owned()); }
    let rendered = (|| -> Result<(u64, u64), EngineError> {
        loop {
            check_cancel(cancel, "healing")?;
            let source_silent = if let Some(unit) = &mut pitch {
                let mut pull_error = None;
                let mut pull = |_: f64, planes: &mut [&mut [f32]]| {
                    let count = planes[0].len();
                    match source.pull(planes) {
                        Ok(_) => Ok(count),
                        Err(e) => { pull_error = Some(e); Err(-1) }
                    }
                };
                let result = unit.render(&mut [&mut left, &mut right], &mut pull);
                if let Some(e) = pull_error { return Err(e); }
                result.map_err(|e| EngineError::new("render pitch", e))?
            } else {
                source.pull(&mut [&mut left, &mut right])?
            };
            for unit in &mut units { unit.process(&mut left, &mut right)?; }
            channel_converter.convert(&left, &right, &mut interleaved)?;
            analyzer.process_interleaved(&interleaved).map_err(|e| EngineError::new("healing meter", e))?;
            // With clip as the last effect, the first EOS-marked pull still has
            // a full clip latency pending: the native clip-only graph reports
            // flags=0 for that terminal render. Pitch supplies its own decision;
            // each Kilohearts wrapper likewise returned flags=0 in EOF captures.
            let silent = units.is_empty() && (pitch.is_some() || plan.clip.is_none()) && source_silent;
            let flags = RENDER_POST | if silent { RENDER_SILENCE } else { 0 };
            let recorder = source.accounting.recorder.as_mut().expect("healing recorder");
            if let Some(span) = recorder.record_render(flags, true, BLOCK as u32) {
                let start = usize::try_from(span.offset).map_err(|e| EngineError::new("healing recorder", e))?;
                let end = start.checked_add(span.frames as usize).ok_or_else(|| EngineError::new("healing recorder", "span overflows"))?;
                if end > BLOCK { return Err(EngineError::new("healing recorder", "native record span exceeds render buffer")); }
                writer.write(&interleaved[start * channels..end * channels])?;
            }
            if source.accounting.finish_render(BLOCK as u32, flags, cancel.is_cancelled()) { break; }
        }
        check_cancel(cancel, "healing")?;
        let frames = source.accounting.recorder.as_ref().expect("healing recorder").current_frames();
        if frames < 0 { return Err(EngineError::new("healing recorder", "negative recorded frame count")); }
        let clip_count = source.clip_count();
        source.reader.close().map_err(|e| EngineError::new("close healing input", e))?;
        channel_converter.dispose()?;
        Ok((frames as u64, clip_count))
    })();
    let (frames, clip_count) = match rendered {
        Ok(result) => result,
        Err(error) => return Err(merge_writer_abort(error, writer.abort())),
    };
    writer.finish()?;
    Ok(HealingResult { analyzer, frames, clip_count, tempo_factor: tempo, stages })
}

fn output_tag_format(format: AudioFormat) -> tags::Format {
    match format {
        AudioFormat::Wav => tags::Format::Wav, AudioFormat::Aiff => tags::Format::Aiff,
        AudioFormat::Alac | AudioFormat::Aac => tags::Format::Mp4,
        AudioFormat::Flac => tags::Format::Flac, AudioFormat::Mp3 => tags::Format::Mp3,
        AudioFormat::MatchInput => unreachable!("output plan resolves match input"),
    }
}

struct EncodingLibraries {
    flac: Option<crate::flac::FlacLibrary>,
    lame: Option<crate::mp3::LameLibrary>,
}
impl EncodingLibraries {
    fn load(format: AudioFormat, settings: &Settings) -> Result<Self, EngineError> {
        let mut result = Self { flac: None, lame: None };
        if format == AudioFormat::Flac {
            let path = settings.libraries.flac.as_ref().ok_or_else(|| EngineError::coded("load FLAC encoder", ErrorCode::LibraryNotConfigured, "trusted libFLAC path is required"))?;
            let expected = settings.libraries.flac_sha256.as_ref().ok_or_else(|| EngineError::coded("load FLAC encoder", ErrorCode::LibraryNotConfigured, "libFLAC SHA-256 pin is required"))?;
            result.flac = Some(crate::flac::FlacLibrary::load(path, expected).map_err(|e| EngineError::library_load("load FLAC encoder", e))?);
        }
        if format == AudioFormat::Mp3 {
            let path = settings.libraries.lame.as_ref().ok_or_else(|| EngineError::coded("load MP3 encoder", ErrorCode::LibraryNotConfigured, "trusted LAME path is required"))?;
            let digest = settings.libraries.lame_sha256.as_deref().ok_or_else(|| EngineError::coded("load MP3 encoder", ErrorCode::LibraryNotConfigured, "LAME SHA-256 pin is required"))?;
            result.lame = Some(crate::mp3::LameLibrary::load(path, digest).map_err(|e| EngineError::library_load("load MP3 encoder", e))?);
        }
        Ok(result)
    }
}
enum Encoder<'a> {
    Apple(apple_codec::Writer),
    Flac(crate::flac::FlacEncoder<'a>),
    Mp3 { encoder: crate::mp3::Mp3Encoder<'a>, converter: apple_codec::Pcm16StereoConverter, scratch: [i16; BLOCK * 2] },
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
fn apple_finish_error(error: apple_codec::CodecError) -> EngineError {
    match error {
        apple_codec::CodecError::Cancelled => EngineError::cancelled("finish Apple output"),
        error => EngineError::new("finish Apple output", error),
    }
}
impl<'a> Encoder<'a> {
    fn new(staged: &file_workflow::StagedOutput, format: AudioFormat, rate: f64, channels: u32, depth: u32,
           source_header: crate::mp3::SourceHeader, settings: &Settings, libraries: &'a EncodingLibraries) -> Result<Self, EngineError> {
        match format {
            AudioFormat::Mp3 => {
                let source_channels = u8::try_from(channels).map_err(|_| EngineError::new("create MP3 encoder", "MP3 source channel count exceeds 64"))?;
                let converter = apple_codec::Pcm16StereoConverter::new(rate, channels).map_err(|e| EngineError::new("create MP3 PCM converter", e))?;
                let config = crate::mp3::Mp3Config { sample_rate: rate as u32, channels: source_channels,
                    mode: settings.mp3_mode.into(), source: source_header, native_update_result_byte_3: None };
                let file = staged.create_stage().map_err(|e| EngineError::workflow("create MP3 staging file", e))?;
                let encoder = crate::mp3::Mp3Encoder::from_file(libraries.lame.as_ref().expect("loaded LAME"), file, config)
                    .map_err(|e| EngineError::new("create MP3 encoder", e))?;
                Ok(Self::Mp3 { encoder, converter, scratch: [0; BLOCK * 2] })
            }
            AudioFormat::Flac => {
                let file = staged.create_stage().map_err(|e| EngineError::workflow("create FLAC staging file", e))?;
                let encoder = crate::flac::FlacEncoder::new(libraries.flac.as_ref().expect("loaded FLAC"), file, rate, channels, depth)
                    .map_err(|e| EngineError::new("create FLAC encoder", e))?;
                Ok(Self::Flac(encoder))
            }
            format => {
                let format = match format {
                    AudioFormat::Wav => apple_codec::OutputFormat::Wav, AudioFormat::Aiff => apple_codec::OutputFormat::Aiff,
                    AudioFormat::Alac => apple_codec::OutputFormat::Alac, AudioFormat::Aac => apple_codec::OutputFormat::Aac,
                    _ => unreachable!("output plan resolves match input"),
                };
                let writer = apple_codec::Writer::create(staged.staging_path(), apple_codec::WriterConfig { format, sample_rate: rate, channels, bit_depth: depth })
                    .map_err(|e| EngineError::new("create Apple encoder", e))?;
                if !writer.warnings().is_empty() {
                    let warnings = format!("{:?}", writer.warnings());
                    let cleanup = writer.abort();
                    return Err(EngineError::new("configure Apple encoder", format!("{warnings}; cleanup: {cleanup:?}")));
                }
                Ok(Self::Apple(writer))
            }
        }
    }
    fn abort(self) -> Result<(), EngineError> {
        match self {
            Self::Apple(writer) => writer.abort().map_err(|e| EngineError::new("abort Apple output", e)),
            Self::Flac(writer) => { drop(writer); Ok(()) }
            Self::Mp3 { encoder, converter, .. } => {
                drop(encoder);
                converter.close().map_err(|e| EngineError::new("abort MP3 PCM converter", e))
            }
        }
    }
    fn write(&mut self, samples: &[f32]) -> Result<(), EngineError> {
        match self {
            Self::Apple(w) => w.write(samples).map_err(|e| EngineError::new("encode Apple output", e)),
            Self::Flac(w) => w.process_interleaved(samples).map_err(|e| EngineError::new("encode FLAC output", e)),
            Self::Mp3 { encoder, converter, scratch } => {
                let frames = converter.convert(samples, scratch).map_err(|e| EngineError::new("convert MP3 PCM", e))?;
                encoder.write_converted_stereo(&mut scratch[..frames * 2]).map_err(|e| EngineError::new("encode MP3 output", e))
            }
        }
    }
    fn finish(self, cancel: &Cancellation, staged: &file_workflow::StagedOutput) -> Result<(), EngineError> {
        match self {
            Self::Apple(w) => {
                #[cfg(all(target_os = "macos", not(feature = "portable")))]
                {
                    w.finish_cancellable_registered(|| cancel.is_cancelled(), |file| staged.register_codec_stage(file)).map_err(apple_finish_error)
                }
                #[cfg(any(target_os = "linux", feature = "portable"))]
                {
                    let _ = cancel;
                    w.finish_registered(|file| staged.register_codec_stage(file)).map_err(|e| EngineError::new("finish Apple output", e))
                }
            }
            Self::Flac(w) => { let file = w.finish().map_err(|e| EngineError::new("finish FLAC output", e))?;
                file.sync_all().map_err(|e| EngineError::new("sync FLAC output", e)) },
            Self::Mp3 { encoder, converter, .. } => {
                converter.close().map_err(|e| EngineError::new("close MP3 PCM converter", e))?;
                encoder.finish().map_err(|e| EngineError::new("finish MP3 output", e))?;
                Ok(())
            }
        }
    }
}

/// Final limiter source. The graph's rate-only update resets GenericOutput to
/// the processing rate even when the recorder uses a different MP3 rate.
/// Observed for a 96 kHz graph with an initially requested 44.1 kHz output.
struct FinalSource {
    reader: AudioReader,
    accounting: FrameAccounting,
    limiter: FinalLimiter,
    left: [f32; BLOCK],
    right: [f32; BLOCK],
}
impl FinalSource {
    fn pull(&mut self, n: usize) -> Result<(), EngineError> {
        if n == 0 || n > BLOCK { return Err(EngineError::new("final source", "invalid source pull size")); }
        self.left[..n].fill(0.0);
        self.right[..n].fill(0.0);
        let count = self.reader.read_stereo_planar(&mut self.left[..n], &mut self.right[..n])
            .map_err(|e| EngineError::new("read intermediate", e))?;
        self.accounting.source_read(n as u32, count as u32).map_err(|e| EngineError::new("final accounting", format!("{e:?}")))?;
        self.limiter.process_host(&mut self.left[..n], &mut self.right[..n], &[])?;
        Ok(())
    }
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
fn stereo_planar_format(rate: f64) -> StreamDescription {
    StreamDescription { sample_rate: rate, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 0x29,
        bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4,
        channels_per_frame: 2, bits_per_channel: 32, reserved: 0 }
}
#[cfg(all(target_os = "macos", not(feature = "portable")))]
fn interleaved_format(rate: f64, channels: usize) -> StreamDescription {
    StreamDescription { sample_rate: rate, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 9,
        bytes_per_packet: (channels * 4) as u32, frames_per_packet: 1, bytes_per_frame: (channels * 4) as u32,
        channels_per_frame: channels as u32, bits_per_channel: 32, reserved: 0 }
}

/// GenericOutput's planar-stereo input to interleaved source-channel output.
/// System AudioConverter owns channel mapping; no manual downmix is inferred.
#[cfg(all(target_os = "macos", not(feature = "portable")))]
struct ChannelConverter { handle: Option<std::ptr::NonNull<std::ffi::c_void>>, channels: usize }
#[cfg(all(target_os = "macos", not(feature = "portable")))]
impl ChannelConverter {
    fn new(rate: f64, channels: usize) -> Result<Self, EngineError> {
        if channels == 2 { return Ok(Self { handle: None, channels }); }
        let mut ptr = std::ptr::null_mut();
        let status = unsafe { AudioConverterNew(&stereo_planar_format(rate), &interleaved_format(rate, channels), &mut ptr) };
        let handle = std::ptr::NonNull::new(ptr);
        if status != 0 || handle.is_none() {
            if let Some(p) = handle { unsafe { AudioConverterDispose(p.as_ptr()); } }
            return Err(EngineError::new("create channel converter", format!("OSStatus {status}")));
        }
        Ok(Self { handle, channels })
    }
    fn convert(&mut self, left: &[f32], right: &[f32], output: &mut [f32]) -> Result<(), EngineError> {
        if left.len() != right.len() || output.len() != left.len() * self.channels || left.len() > BLOCK {
            return Err(EngineError::new("convert channels", "invalid buffer shape"));
        }
        if self.channels == 2 {
            for (i, frame) in output.chunks_exact_mut(2).enumerate() { frame[0] = left[i]; frame[1] = right[i]; }
            return Ok(());
        }
        let bytes = (left.len() * 4) as u32;
        let input = StereoBufferList { number_buffers: 2, buffers: [
            AudioBuffer { number_channels: 1, data_byte_size: bytes, data: left.as_ptr().cast_mut().cast() },
            AudioBuffer { number_channels: 1, data_byte_size: bytes, data: right.as_ptr().cast_mut().cast() },
        ] };
        let mut result = AudioBufferList { number_buffers: 1, buffers: [
            AudioBuffer { number_channels: self.channels as u32,
                data_byte_size: (output.len() * 4) as u32, data: output.as_mut_ptr().cast() },
        ] };
        let status = unsafe { AudioConverterConvertComplexBuffer(self.handle.expect("live channel converter").as_ptr(),
            left.len() as u32, (&input as *const StereoBufferList).cast(), &mut result) };
        if status != 0 || result.buffers[0].data_byte_size as usize != output.len() * 4 {
            return Err(EngineError::new("convert channels", format!("OSStatus {status}, {} output bytes", result.buffers[0].data_byte_size)));
        }
        Ok(())
    }
    fn dispose(&mut self) -> Result<(), EngineError> {
        if let Some(handle) = self.handle.take() {
            let status = unsafe { AudioConverterDispose(handle.as_ptr()) };
            if status != 0 { return Err(EngineError::new("close channel converter", format!("OSStatus {status}"))); }
        }
        Ok(())
    }
}
#[cfg(all(target_os = "macos", not(feature = "portable")))]
impl Drop for ChannelConverter { fn drop(&mut self) { let _ = self.dispose(); } }

#[cfg(any(target_os = "linux", feature = "portable"))]
struct ChannelConverter(apple_codec::ChannelConverter);
#[cfg(any(target_os = "linux", feature = "portable"))]
impl ChannelConverter {
    fn new(rate: f64, channels: usize) -> Result<Self, EngineError> {
        apple_codec::ChannelConverter::new(rate, channels).map(Self).map_err(|e| EngineError::new("create channel converter", e))
    }
    fn convert(&mut self, left: &[f32], right: &[f32], output: &mut [f32]) -> Result<(), EngineError> {
        self.0.convert(left, right, output).map_err(|e| EngineError::new("convert channels", e))
    }
    fn dispose(&mut self) -> Result<(), EngineError> {
        self.0.dispose().map_err(|e| EngineError::new("close channel converter", e))
    }
}


struct EncodedInfo { format: String, stream: StreamDescription, frames: u64 }
fn inspect_encoded(path: &Path, format: AudioFormat, settings: &Settings) -> Result<EncodedInfo, EngineError> {
    #[cfg(any(target_os = "linux", feature = "portable"))]
    if format == AudioFormat::Mp3 {
        let info = crate::mp3::inspect_encoded_output(path).map_err(|e| EngineError::new("inspect encoded output", e))?;
        let stream = StreamDescription {
            sample_rate: f64::from(info.sample_rate),
            format_id: u32::from_be_bytes(*b".mp3"),
            frames_per_packet: info.frames_per_packet,
            channels_per_frame: info.channels,
            ..Default::default()
        };
        return Ok(EncodedInfo { format: format_label(path, SourceCodec::Mp3), stream, frames: info.frames });
    }
    let reader = AudioReader::open(path, None, &input_libraries(settings)).map_err(|e| EngineError::new("inspect encoded output", e))?;
    let stream = reader.source_format();
    let codec = source_codec(path, stream);
    let expected = match format {
        AudioFormat::Wav | AudioFormat::Aiff => SourceCodec::Lpcm,
        AudioFormat::Alac => SourceCodec::Alac, AudioFormat::Aac => SourceCodec::Aac,
        AudioFormat::Flac => SourceCodec::Flac, AudioFormat::Mp3 => SourceCodec::Mp3,
        AudioFormat::MatchInput => return Err(EngineError::new("inspect encoded output", "unresolved output format")),
    };
    if codec != expected {
        return Err(EngineError::new("inspect encoded output", format!("expected {expected:?}, decoder reported {codec:?}")));
    }
    let frames = reader.file_length_frames().map_err(|e| EngineError::new("inspect encoded length", e))?;
    if frames < 0 { return Err(EngineError::new("inspect encoded length", "decoder returned a negative frame count")); }
    reader.close().map_err(|e| EngineError::new("close encoded inspection", e))?;
    Ok(EncodedInfo { format: format_label(path, codec), stream, frames: frames as u64 })
}
struct FinalResult { frames: u64, output_volume: f32 }
fn final_pass(temp: &TempCaf, staged: &file_workflow::StagedOutput, measured: &Measured, healed: &HealingResult, template: &Template,
              format: AudioFormat, source_header: crate::mp3::SourceHeader, settings: &Settings,
              libraries: &EncodingLibraries, cancel: &Cancellation) -> Result<FinalResult, EngineError> {
    let processing_rate = measured.processing_rate;
    let channels = measured.source.channels_per_frame as usize;
    let output_code = match format { AudioFormat::Mp3 => 1, AudioFormat::Aac => 6, _ => 0 };
    let output_rate = controller::final_sample_rate(processing_rate, output_code);
    #[cfg(any(target_os = "linux", feature = "portable"))]
    if output_rate != processing_rate {
        return Err(EngineError::new("output sample-rate conversion",
            apple_codec::CodecError::UnsupportedSampleRateConversion { source_rate: processing_rate, client_rate: output_rate }));
    }
    let plan = FinalPlan::from_healed(template, &healed.analyzer);
    let limiter = FinalLimiter::new(processing_rate, &plan)?;
    let latency = controller::graph_latency_frames(Some(processing_rate), &[Some(limiter.dsp.latency_frames() as f64 / processing_rate)]);
    let header = match &source_header { crate::mp3::SourceHeader::Other(h) | crate::mp3::SourceHeader::Mp3(h) => h };
    let is_mp3 = measured.codec == SourceCodec::Mp3;
    // analyzeVolumeAndPitch initializes decodeUsingVbrHeader to true. With an
    // MP3 source, no recognized LAME tag or no VBR header sets delay to 576;
    // otherwise the delay is zero (0x100016e48..0x100016ffc).
    let decoder_delay = if is_mp3 && (!(header.xing || header.info) || header.lame_identifier.is_none()) { 576 } else { 0 };
    let mp3 = (format == AudioFormat::Mp3).then_some(controller::Mp3CountInputs {
        input_is_mp3: is_mp3, input_sample_rate: measured.source.sample_rate, output_sample_rate: output_rate,
        input_has_xing_header: header.xing, input_has_info_header: header.info, decode_using_vbr_header: true,
        input_lame_info_latency: header.encoder_delay as i32, input_decoder_delay: decoder_delay,
    });
    let frame_plan = controller::final_frame_plan(measured.frames, healed.tempo_factor, latency, mp3);
    if frame_plan.recorder_shift_frames > 0 {
        return Err(EngineError::new("final recorder", "native positive shift never advances and cannot finish safely"));
    }
    let mut reader = AudioReader::open(&temp.path, Some(processing_rate), &input_libraries(settings))
        .map_err(|e| EngineError::new("open intermediate", e))?;
    reader.set_stereo_planar(processing_rate).map_err(|e| EngineError::new("configure intermediate input", e))?;
    let mut source = FinalSource { reader, accounting: FrameAccounting::new(RenderPass::Final, Some(RecorderAccounting::for_final(&frame_plan))),
        limiter, left: [0.0; BLOCK], right: [0.0; BLOCK] };
    let mut channel_converter = ChannelConverter::new(processing_rate, channels)?;
    let bit_depth = apple_codec::recorder_bit_depth(&measured.source);
    // The original attaches the meter to the output callback and retains its
    // processing-rate configuration even when the output rate differs.
    let mut meter = LoudnessMeter::new(processing_rate, channels).map_err(|e| EngineError::new("create output meter", e))?;
    let mut encoder = Encoder::new(staged, format, output_rate, channels as u32, bit_depth, source_header, settings, libraries)?;
    let mut rendered = vec![0.0f32; BLOCK * channels];
    let rendered_frames = (|| -> Result<u64, EngineError> {
        loop {
            check_cancel(cancel, "final render")?;
            source.pull(BLOCK)?;
            channel_converter.convert(&source.left, &source.right, &mut rendered)?;
            meter.process_interleaved(&rendered).map_err(|e| EngineError::new("output meter", e))?;
            // The final node is the same Kilohearts wrapper family as the
            // reference captures; recorder POST remains set.
            let flags = RENDER_POST;
            let recorder = source.accounting.recorder.as_mut().expect("final recorder");
            if let Some(span) = recorder.record_render(flags, true, BLOCK as u32) {
                let start = usize::try_from(span.offset).map_err(|e| EngineError::new("final recorder", e))?;
                let end = start.checked_add(span.frames as usize).ok_or_else(|| EngineError::new("final recorder", "span overflows"))?;
                if end > BLOCK { return Err(EngineError::new("final recorder", "native record span exceeds render buffer")); }
                encoder.write(&rendered[start * channels..end * channels])?;
            }
            if source.accounting.finish_render(BLOCK as u32, flags, cancel.is_cancelled()) { break; }
        }
        check_cancel(cancel, "final render")?;
        let frames = source.accounting.recorder.as_ref().expect("final recorder").current_frames();
        if frames < 0 { return Err(EngineError::new("final recorder", "negative output frame count")); }
        channel_converter.dispose()?;
        source.reader.close().map_err(|e| EngineError::new("close intermediate input", e))?;
        Ok(frames as u64)
    })();
    let frames = match rendered_frames {
        Ok(frames) => frames,
        Err(error) => return Err(merge_writer_abort(error, encoder.abort())),
    };
    encoder.finish(cancel, staged)?;
    Ok(FinalResult { frames, output_volume: meter.integrated_loudness() as f32 })
}


fn abort_with(mut error: EngineError, staged: file_workflow::StagedOutput) -> EngineError {
    let backup = staged.backup_path().map(Path::to_owned);
    match staged.abort() {
        Ok(report) => error.retained_backup = report.retained_backup.or(backup),
        Err(cleanup) => {
            error.no_longer_a_clean_cancel();
            error.message.push_str(&format!("; abort failed: {cleanup}"));
            if let WorkflowError::Transaction { backup: b, recovery, published, .. } = &cleanup {
                error.retained_backup = b.clone().or(backup);
                error.recovery_path = recovery.clone().or(error.recovery_path);
                error.partial_publication |= *published;
            } else { error.retained_backup = backup; }
        }
    }
    error
}

fn merge_writer_abort(mut primary: EngineError, cleanup: Result<(), EngineError>) -> EngineError {
    if let Err(error) = cleanup {
        primary.no_longer_a_clean_cancel();
        primary.message.push_str(&format!("; writer abort failed: {error}"));
    }
    primary
}

/// Analyze, heal, remeasure, limit/encode, copy source metadata and publish.
/// No full-track PCM is held in RAM; the intermediate is a private float CAF.
pub fn process_file(prepared: &PreparedProcess, template: &Template, settings: &Settings,
                    cancel: &Cancellation) -> Result<ProcessReport, EngineError> {
    check_cancel(cancel, "planning")?;
    let plan = &prepared.plan;
    let identity = &prepared.identity;
    let input = plan.source();
    identity.verify(input).map_err(|e| EngineError::workflow("plan source identity", e))?;
    let libraries = EncodingLibraries::load(plan.format(), settings)?;
    let staged = plan.begin(identity).map_err(|e| EngineError::workflow("begin output", e))?;
    if let Err(error) = identity.verify(input) {
        return Err(abort_with(EngineError::workflow("reserve source identity", error), staged));
    }
    if let Some(path) = staged.backup_path() {
        if let Err(e) = require_reportable_path(path, "backup") { return Err(abort_with(e, staged)); }
    }
    let temp = match TempCaf::new() { Ok(t) => t, Err(e) => return Err(abort_with(e, staged)) };
    let controlled_mp4_time = match plan.format() {
        AudioFormat::Aac | AudioFormat::Alac => prepared.controlled_mp4_time_1904,
        _ => None,
    };
    let run = (|| -> Result<(Measured, HealingResult, FinalResult, EncodedInfo, &'static str, bool), EngineError> {
        let measured = analyze(input, settings, cancel, false)?;
        identity.verify(input).map_err(|e| EngineError::workflow("verify source after analysis", e))?;
        if measured.codec != prepared.source_codec {
            return Err(EngineError::coded("analysis input", ErrorCode::SourceChanged, "source codec changed since inspection"));
        }
        let plan_healing = HealingPlan::from_analysis(template, measured.values);
        let healed = healing_pass(input, &temp, &measured, &plan_healing, plan.is_replacement(), settings, cancel)?;
        identity.verify(input).map_err(|e| EngineError::workflow("verify source after healing", e))?;
        let header = crate::mp3::inspect_mp3_header(input).map_err(|e| EngineError::new("inspect source MP3 header", e))?;
        let source_header = if measured.codec == SourceCodec::Mp3 { crate::mp3::SourceHeader::Mp3(header) }
            else { crate::mp3::SourceHeader::Other(header) };
        let final_result = final_pass(&temp, &staged, &measured, &healed, template, plan.format(), source_header, settings, &libraries, cancel)?;
        staged.remember_staging_file().map_err(|e| EngineError::workflow("pin output stage", e))?;
        identity.verify(input).map_err(|e| EngineError::workflow("verify source after encoding", e))?;
        if let Some(seconds) = controlled_mp4_time {
            apple_codec::set_mp4_header_times(staged.staging_path(), seconds)
                .map_err(|e| EngineError::new("set controlled MP4 time", e))?;
        }
        check_cancel(cancel, "copy tags")?;
        let output_pitch = if plan_healing.pitch.is_some() { 440.0 } else { measured.values.input_tuning_hz };
        let backup = staged.backup_path().and_then(Path::to_str);
        let marker = Marker::from_processing(ProcessingMarker {
            software_version: Some(VERSION), backup_file_name: backup,
            applied_clip_repair: plan_healing.clip.is_some(), applied_pitch_correction: plan_healing.pitch.is_some(),
            applied_warmth: plan_healing.warmth.is_some(), expansion_applied: i32::from(plan_healing.expansion.is_some()),
            compression_applied: i32::from(plan_healing.compression.is_some()), volume_analysis_version: 4,
            target_volume: plan_healing.target_level_db, input_volume: measured.values.input_level_db,
            output_volume: final_result.output_volume, clip_repair_count: healed.clip_count as i32,
            exciter_option: template.exciter_option, input_pitch: measured.values.input_tuning_hz,
            output_pitch, speed_adjustment: healed.tempo_factor,
        });
        let has_payload = marker.encoded_bytes().map_err(|e| EngineError::new("serialize processing marker", e))?.is_some();
        let choice = if plan.requested_format() == AudioFormat::MatchInput { tags::OutputChoice::MatchInput } else { tags::OutputChoice::Explicit };
        let bridge = bridge(settings)?;
        let tag_source = staged.backup_path().unwrap_or(input);
        staged.remember_staging_file().map_err(|e| EngineError::workflow("verify output stage", e))?;
        // Embedded artwork is copied unchanged only under the explicit opt-in; otherwise the reference policy applies.
        let options = tags::TransferOptions { copy_artwork: settings.non_identical_enabled() };
        let report = bridge.transfer_with_options(tag_source, staged.staging_path(), source_tag_format(input)?,
            output_tag_format(plan.format()), choice, &marker, options).map_err(|e| EngineError::new("copy source tags", e))?;
        let artwork_copied = report.artwork_copied;
        if !report.is_success() {
            return Err(EngineError::new("copy source tags", format!("copy error: {:?}; marker error: {:?}", report.copy_error, report.marker_error)));
        }
        staged.accept_tag_rewrite().map_err(|e| EngineError::workflow("pin tagged output", e))?;
        let raw = bridge.read_processing_marker(staged.staging_path(), output_tag_format(plan.format()))
            .map_err(|e| EngineError::new("verify processing marker", e))?;
        let marker_status = if has_payload {
            let written = raw.as_deref().map(Marker::from_encoded_bytes).transpose()
                .map_err(|e| EngineError::new("verify processing marker", e))?;
            if written.as_ref() != Some(&marker) {
                return Err(EngineError::new("verify processing marker", "staged marker is absent or differs from the processing result"));
            }
            "present"
        } else if plan.format() == AudioFormat::Wav && raw.as_deref() == Some(&[][..]) {
            // The native WAV nil setter emits a zero-length PLTN chunk.
            "nil_payload_empty_wav_chunk"
        } else if plan.format() != AudioFormat::Wav && raw.is_none() {
            "nil_payload_absent"
        } else {
            return Err(EngineError::new("verify processing marker", "nil payload did not produce the expected container marker state"));
        };
        let encoded = inspect_encoded(staged.staging_path(), plan.format(), settings)?;
        check_cancel(cancel, "publication")?;
        Ok((measured, healed, final_result, encoded, marker_status, artwork_copied))
    })();
    let cleanup = temp.cleanup();
    let (measured, healed, output, encoded, marker_status, artwork_copied) = match (run, cleanup) {
        (Ok(result), Ok(())) => result,
        (Err(mut error), Err(cleanup)) => {
            error.no_longer_a_clean_cancel();
            error.message.push_str(&format!("; intermediate cleanup failed: {cleanup}"));
            return Err(abort_with(error, staged));
        }
        (Err(error), _) | (Ok(_), Err(error)) => return Err(abort_with(error, staged)),
    };
    if let Err(error) = check_cancel(cancel, "publication") { return Err(abort_with(error, staged)); }
    let publication = staged.finish(cancel).map_err(|e| EngineError::workflow("publish output", e))?;
    let output_pitch = if healed.stages.iter().any(|stage| stage == "pitch") { 440.0 } else { measured.values.input_tuning_hz };
    let mut stages = healed.stages;
    stages.push("limiter".to_owned());
    Ok(ProcessReport { input_path: input.to_owned(), output_path: publication.destination, backup_path: publication.backup,
        replaced_existing_output: publication.replaced_existing_output, input_format: prepared.input_format.clone(), output_format: encoded.format,
        input_sample_rate: measured.source.sample_rate, processing_sample_rate: measured.processing_rate, sample_rate: encoded.stream.sample_rate,
        channels: measured.source.channels_per_frame, output_channels: encoded.stream.channels_per_frame,
        output_bit_depth: encoded.stream.bits_per_channel, encoded_length_frames: encoded.frames,
        input_frames: measured.frames as u64, healed_frames: healed.frames,
        output_frames: output.frames, input_volume: measured.values.input_level_db, healed_volume: healed.analyzer.integrated_loudness() as f32,
        output_volume: output.output_volume, input_pitch_hz: measured.values.input_tuning_hz, output_pitch_hz: output_pitch,
        tempo_factor: healed.tempo_factor, clip_count: healed.clip_count, applied_stages: stages, processing_marker_status: marker_status,
        input_marker_override: prepared.input_marker_override, source_tags_unavailable: prepared.source_tags_unavailable.clone(),
        controlled_mp4_time_1904: controlled_mp4_time,
        non_identical_paths: non_identical_paths(measured.non_identical, artwork_copied) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader};

    #[test]
    fn partial_caf_stage_is_owned_before_initialization_write_and_cancel_errors() {
        for scenario in 0..3 {
            let temp = TempCaf::new().unwrap();
            let directory = temp.directory.clone();
            if scenario == 0 {
                assert!(CafWriter::new(&temp, 48_000.0, usize::MAX).is_err());
            } else {
                let mut writer = CafWriter::new(&temp, 48_000.0, 2).unwrap();
                if scenario == 1 {
                    assert!(writer.write(&[0.25]).is_err());
                } else {
                    writer.write(&[0.0; 1024]).unwrap();
                    let token = Cancellation::new(); token.cancel();
                    assert!(check_cancel(&token, "analysis").unwrap_err().cancelled);
                }
                writer.abort().unwrap();
            }
            temp.cleanup().unwrap();
            assert!(!directory.exists());
        }
    }

    #[test]
    fn writer_abort_failure_preserves_primary_error_and_breaks_clean_cancellation() {
        let cancelled = EngineError::cancelled("healing");
        let clean = merge_writer_abort(cancelled, Ok(()));
        assert!(clean.cancelled);
        let primary = EngineError::cancelled("final render");
        let combined = merge_writer_abort(primary, Err(EngineError::new("close intermediate", "OSStatus -1")));
        assert!(!combined.cancelled);
        assert_eq!(combined.code, ErrorCode::IoError, "a failed abort is not a clean cancellation");
        assert_eq!(combined.stage, "final render");
        assert!(combined.message.contains("writer abort failed: close intermediate: OSStatus -1"));
    }

    #[test]
    fn a_cleared_cancellation_never_keeps_the_cancelled_code() {
        let mut cancelled = EngineError::cancelled("healing");
        assert_eq!(cancelled.code, ErrorCode::Cancelled);
        cancelled.no_longer_a_clean_cancel();
        assert!(!cancelled.cancelled);
        assert_eq!(cancelled.code, ErrorCode::IoError);
        let mut other = EngineError::coded("analysis", ErrorCode::ProcessingFailed, "x");
        other.no_longer_a_clean_cancel();
        assert_eq!(other.code, ErrorCode::ProcessingFailed);
    }

    #[test]
    fn libraries_that_cannot_be_loaded_report_library_load_failed() {
        let root = TempCaf::new().unwrap();
        let mut settings = Settings::defaults(&root.directory, 1);
        let digest = "0".repeat(64);
        let missing = root.directory.join("no/such/library");
        let directory = root.directory.join("a-directory");
        fs::create_dir(&directory).unwrap();
        for path in [&missing, &directory] {
            settings.libraries.tag_bridge = Some(path.clone());
            settings.libraries.flac = Some(path.clone());
            settings.libraries.flac_sha256 = Some(digest.clone());
            settings.libraries.lame = Some(path.clone());
            settings.libraries.lame_sha256 = Some(digest.clone());
            let error = bridge(&settings).err().expect("a missing path or a directory is not a bridge");
            assert_eq!((error.stage, error.code), ("tags", ErrorCode::LibraryLoadFailed), "{}", error.message);
            for (format, stage) in [(AudioFormat::Flac, "load FLAC encoder"), (AudioFormat::Mp3, "load MP3 encoder")] {
                let error = EncodingLibraries::load(format, &settings).err().expect("a missing path or a directory is not a library");
                assert_eq!((error.stage, error.code), (stage, ErrorCode::LibraryLoadFailed), "{}", error.message);
            }
        }
        // An I/O error stays `io_error` elsewhere, and a typed identity answer keeps its own code.
        let io = || crate::mp3::Mp3Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(EngineError::new("encode MP3 output", io()).code, ErrorCode::IoError);
        assert_eq!(EngineError::library_load("load MP3 encoder", io()).code, ErrorCode::LibraryLoadFailed);
        let mismatch = crate::mp3::Mp3Error::DigestMismatch { expected: "a".into(), actual: "b".into() };
        assert_eq!(EngineError::library_load("load MP3 encoder", mismatch).code, ErrorCode::LibraryUnapproved);
    }

    #[test]
    fn apple_writer_abort_removes_unpublished_output() {
        let temp = TempCaf::new().unwrap();
        let destination = temp.directory.join("output.wav");
        let config = apple_codec::WriterConfig {
            format: apple_codec::OutputFormat::Wav, sample_rate: 44_100.0, channels: 2, bit_depth: 16,
        };
        let mut writer = apple_codec::Writer::create(&destination, config).unwrap();
        writer.write(&[0.0; 32]).unwrap();
        Encoder::Apple(writer).abort().unwrap();
        assert!(!destination.exists());
        temp.cleanup().unwrap();
    }

    #[test]
    #[ignore = "requires pinned public PN_LAME_LIB/PN_LAME_SHA256 and PN_FLAC_LIB/PN_FLAC_SHA256"]
    fn partial_encoders_cleanup_after_errors_and_cancellation() {
        let root = TempCaf::new().unwrap();
        let source = root.directory.join("source.wav");
        fs::write(&source, b"synthetic source").unwrap();
        let files = file_workflow::discover_inputs(&[source.clone()]).unwrap();
        let mut settings = Settings::defaults(&root.directory, 1);
        settings.libraries.lame = Some(PathBuf::from(std::env::var_os("PN_LAME_LIB").unwrap()));
        settings.libraries.lame_sha256 = Some(std::env::var("PN_LAME_SHA256").unwrap());
        settings.libraries.flac = Some(PathBuf::from(std::env::var_os("PN_FLAC_LIB").unwrap()));
        settings.libraries.flac_sha256 = Some(std::env::var("PN_FLAC_SHA256").unwrap());
        let source_header = crate::mp3::SourceHeader::Other(crate::mp3::Mp3Header::default());
        for format in [AudioFormat::Mp3, AudioFormat::Flac, AudioFormat::Wav, AudioFormat::Aiff, AudioFormat::Alac] {
            for cancelled in [false, true] {
                let output = file_workflow::OutputSettings { mode: file_workflow::OutputMode::SameFolder, format };
                let plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &output, None).unwrap();
                let staged = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
                let directory = staged.workspace_directory().to_owned();
                let libraries = EncodingLibraries::load(format, &settings).unwrap();
                let mut encoder = Encoder::new(&staged, format, 48_000.0, 2, 16, source_header.clone(), &settings, &libraries).unwrap();
                let error = if cancelled {
                    encoder.write(&[0.0; 1024]).unwrap();
                    let token = Cancellation::new(); token.cancel();
                    check_cancel(&token, "final render").unwrap_err()
                } else {
                    encoder.write(&[0.25]).unwrap_err()
                };
                drop(encoder);
                let error = abort_with(error, staged);
                assert_eq!(error.cancelled, cancelled, "format={format:?}: {error}");
                assert!(!error.message.contains("cleanup failed"), "format={format:?}: {error}");
                assert!(!directory.exists(), "format={format:?} left {}", directory.display());
                assert!(!plan.destination().exists());
            }
        }
        for format in [AudioFormat::Mp3, AudioFormat::Flac] {
            let output = file_workflow::OutputSettings { mode: file_workflow::OutputMode::SameFolder, format };
            let plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &output, None).unwrap();
            let staged = plan.begin(&FileIdentity::capture(&source).unwrap()).unwrap();
            let directory = staged.workspace_directory().to_owned();
            let libraries = EncodingLibraries::load(format, &settings).unwrap();
            let file = staged.create_stage().unwrap();
            if format == AudioFormat::Mp3 {
                let invalid = crate::mp3::Mp3Config { sample_rate: 1, channels: 2, mode: crate::mp3::Mp3Mode::Cbr320,
                    source: source_header.clone(), native_update_result_byte_3: None };
                assert!(crate::mp3::Mp3Encoder::from_file(libraries.lame.as_ref().unwrap(), file, invalid).is_err());
            } else {
                assert!(crate::flac::FlacEncoder::new(libraries.flac.as_ref().unwrap(), file, 48_000.0, 0, 16).is_err());
            }
            staged.abort().unwrap();
            assert!(!directory.exists(), "initialization failure left {}", directory.display());
        }
        fs::remove_file(source).unwrap();
        root.cleanup().unwrap();
    }

    #[test]
    fn source_container_magic_classifies_all_policy_families() {
        use SniffedContainer::{Ogg, Unknown, Writable};
        for (bytes, expected) in [
            (b"RIFF\xff\xff\xff\xffWAVE".as_slice(), Writable(tags::Format::Wav)),
            (b"FORM\xff\xff\xff\xffAIFF".as_slice(), Writable(tags::Format::Aiff)),
            (b"FORM\xff\xff\xff\xffAIFC".as_slice(), Writable(tags::Format::Aiff)),
            (b"fLaC".as_slice(), Writable(tags::Format::Flac)),
            (b"ID3".as_slice(), Unknown),
            (b"\xff\xfb".as_slice(), Writable(tags::Format::Mp3)),
            (b"\0\0\0\x18ftyp".as_slice(), Unknown),
            (b"OggS".as_slice(), Ogg),
            (b"RIFF".as_slice(), Unknown),
            (b"FORM\0\0\0\x04WAVE".as_slice(), Unknown),
            (b"Ogg".as_slice(), Unknown),
            (b"\xff\x1b".as_slice(), Unknown),
            (b"".as_slice(), Unknown),
        ] {
            assert_eq!(sniff_container_prefix(bytes), expected, "{bytes:?}");
        }
    }

    #[test]
    fn mp4_sniff_walks_leading_atoms_and_rejects_incomplete_headers() {
        let fixture = TempCaf::new().unwrap();
        let path = fixture.directory.join("atom-walk");
        for first in [b"ftyp", b"moov", b"mdat"] {
            let mut bytes = if first == b"moov" { b"\0\0\0\x10moov\0\0\0\x08trak".to_vec() }
                else { let mut bytes = 8u32.to_be_bytes().to_vec(); bytes.extend_from_slice(first); bytes };
            if first != b"moov" { bytes.extend_from_slice(b"\0\0\0\x10moov\0\0\0\x08trak"); }
            if first != b"mdat" { bytes.extend_from_slice(b"\0\0\0\x08mdat"); }
            fs::write(&path, bytes).unwrap();
            assert_eq!(sniff_container(&path, tags::Format::Wav).unwrap(), (SniffedContainer::Writable(tags::Format::Mp4), 0, false));
        }
        let mut leading = b"\0\0\0\x08free\0\0\0\x01wide\0\0\0\0\0\0\0\x10\0\0\0\x08skip".to_vec();
        leading.extend_from_slice(b"\0\0\0\x08uuid\0\0\0\x10moov\0\0\0\x08trak\0\0\0\x08mdat");
        fs::write(&path, &leading).unwrap();
        assert_eq!(sniff_container(&path, tags::Format::Wav).unwrap(), (SniffedContainer::Writable(tags::Format::Mp4), 0, false));
        for bytes in [
            b"\0\0\0\0free\0\0\0\x08moov".as_slice(),
            b"\0\0\0\x04free\0\0\0\x08moov".as_slice(),
            b"\0\0\0\x40free\0\0\0\x08moov".as_slice(),
            b"\0\0\0\x01wide\0\0\0\0".as_slice(),
            b"\0\0\0\x08ftyp\0\0\0\x08moov\0\0\0\x08mdat".as_slice(),
            b"\0\0\0\x10moov\0\0\0\x20trak\0\0\0\x08mdat".as_slice(),
            b"unknown garbage".as_slice(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(sniff_container(&path, tags::Format::Wav).unwrap(), (SniffedContainer::Unknown, 0, false), "{bytes:?}");
        }
        fs::remove_file(path).unwrap();
        fixture.cleanup().unwrap();
    }

    #[test]
    fn id3_prefix_sniff_validates_sizes_flags_footer_and_following_family() {
        let fixture = TempCaf::new().unwrap();
        let path = fixture.directory.join("id3-prefix");
        let empty = b"ID3\x04\0\0\0\0\0\0";
        let footer_header = b"ID3\x04\0\x10\0\0\0\0";
        let footer = b"3DI\x04\0\x10\0\0\0\0";
        for (after, expected) in [
            (b"fLaC".as_slice(), SniffedContainer::Writable(tags::Format::Flac)),
            (b"\xff\xfb\x90\x64".as_slice(), SniffedContainer::Writable(tags::Format::Mp3)),
            (b"RIFF\0\0\0\0WAVE".as_slice(), SniffedContainer::Writable(tags::Format::Wav)),
            (b"FORM\0\0\0\0AIFC".as_slice(), SniffedContainer::Writable(tags::Format::Aiff)),
            (b"\0\0\0\x08free\0\0\0\x10moov\0\0\0\x08trak\0\0\0\x08mdat".as_slice(), SniffedContainer::Writable(tags::Format::Mp4)),
            (b"OggS".as_slice(), SniffedContainer::Ogg),
            (b"other".as_slice(), SniffedContainer::Unknown),
        ] {
            let mut bytes = empty.to_vec(); bytes.extend_from_slice(footer_header); bytes.extend_from_slice(footer); bytes.extend_from_slice(after);
            fs::write(&path, bytes).unwrap();
            assert_eq!(sniff_container(&path, tags::Format::Wav).unwrap(), (expected, 2, false), "{after:?}");
        }
        for bytes in [
            b"ID3".as_slice(),
            b"ID3\x04\0\0\x80\0\0\0fLaC".as_slice(),
            b"ID3\x05\0\0\0\0\0\0fLaC".as_slice(),
            b"ID3\x03\0\x10\0\0\0\0fLaC".as_slice(),
            b"ID3\x04\0\0\0\0\0\x7ffLaC".as_slice(),
            b"ID3\x04\0\x10\0\0\0\0fLaC".as_slice(),
            b"ID3\x04\0\x10\0\0\0\0bad-footerfLaC".as_slice(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(sniff_container(&path, tags::Format::Wav).unwrap().0, SniffedContainer::Unknown, "{bytes:?}");
        }
        fs::remove_file(path).unwrap();
        fixture.cleanup().unwrap();
    }

    #[test]
    fn mpeg_sniff_requires_three_bounded_consistent_frames_and_respects_magic() {
        let fixture = TempCaf::new().unwrap();
        let path = fixture.directory.join("mpeg-scan");
        let mut frames = Vec::new();
        for _ in 0..3 { frames.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]); frames.extend_from_slice(&[0; 413]); }
        for prefix in [vec![0; 23], b"junk-before-audio".to_vec()] {
            let mut bytes = prefix.clone(); bytes.extend_from_slice(&frames);
            fs::write(&path, &bytes).unwrap();
            assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, SniffedContainer::Writable(tags::Format::Mp3));
            assert_eq!(sniff_container(&path, tags::Format::Wav).unwrap().0, SniffedContainer::Unknown);
            let mut id3 = b"ID3\x04\0\0\0\0\0\0".to_vec(); id3.extend_from_slice(&bytes);
            fs::write(&path, id3).unwrap();
            assert_eq!(sniff_container(&path, tags::Format::Wav).unwrap().0, SniffedContainer::Writable(tags::Format::Mp3));
        }
        let mut two = b"junk".to_vec(); two.extend_from_slice(&frames[..834]);
        fs::write(&path, two).unwrap();
        assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, SniffedContainer::Unknown);
        let mut inconsistent = b"junk".to_vec(); inconsistent.extend_from_slice(&frames); inconsistent[4 + 417 + 2] = 0x94;
        fs::write(&path, inconsistent).unwrap();
        assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, SniffedContainer::Unknown);
        let mut reserved = b"junk".to_vec(); reserved.extend_from_slice(&frames); reserved[4 + 417 + 1] = 0xeb;
        fs::write(&path, reserved).unwrap();
        assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, SniffedContainer::Unknown);
        let mut reserved_emphasis = b"junk".to_vec(); reserved_emphasis.extend_from_slice(&frames); reserved_emphasis[4 + 417 + 3] = 0x66;
        fs::write(&path, reserved_emphasis).unwrap();
        assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, SniffedContainer::Unknown);
        let mut beyond = vec![0; 64 * 1024]; beyond.extend_from_slice(&frames);
        fs::write(&path, beyond).unwrap();
        assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, SniffedContainer::Unknown);
        for (prefix, expected) in [
            (b"RIFF\0\0\0\0WAVE".as_slice(), SniffedContainer::Writable(tags::Format::Wav)),
            (b"FORM\0\0\0\0AIFF".as_slice(), SniffedContainer::Writable(tags::Format::Aiff)),
            (b"fLaC".as_slice(), SniffedContainer::Writable(tags::Format::Flac)),
            (b"OggS".as_slice(), SniffedContainer::Ogg),
        ] {
            let mut bytes = prefix.to_vec(); bytes.extend_from_slice(&frames);
            fs::write(&path, bytes).unwrap();
            assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, expected);
        }
        let mut mp4 = b"\0\0\0\x10ftypM4A \0\0\0\0\0\0\0\x10moov\0\0\0\x08trak".to_vec();
        mp4.extend_from_slice(&u32::try_from(frames.len() + 8).unwrap().to_be_bytes());
        mp4.extend_from_slice(b"mdat"); mp4.extend_from_slice(&frames);
        fs::write(&path, mp4).unwrap();
        assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, SniffedContainer::Writable(tags::Format::Mp4));
        for name in [b"ftyp", b"moov", b"mdat"] {
            let mut collision = frames.clone(); collision[4..8].copy_from_slice(name);
            fs::write(&path, collision).unwrap();
            assert_eq!(sniff_container(&path, tags::Format::Mp3).unwrap().0, SniffedContainer::Writable(tags::Format::Mp3));
        }
        fs::remove_file(path).unwrap();
        fixture.cleanup().unwrap();
    }

    #[test]
    fn unavailable_riff_chunk_walk_clamps_audio_and_refuses_marker_or_garbage() {
        fn chunk(bytes: &mut Vec<u8>, id: &[u8; 4], size: u32, payload: &[u8], wav: bool) {
            bytes.extend_from_slice(id);
            bytes.extend_from_slice(&if wav { size.to_le_bytes() } else { size.to_be_bytes() });
            bytes.extend_from_slice(payload);
        }
        fn prefix(wav: bool) -> Vec<u8> {
            let mut bytes = if wav { b"RIFF\xff\xff\xff\xffWAVE".to_vec() } else { b"FORM\xff\xff\xff\xffAIFF".to_vec() };
            if wav { chunk(&mut bytes, b"fmt ", 16, &[0; 16], true); }
            else { chunk(&mut bytes, b"COMM", 18, &[0; 18], false); }
            bytes
        }
        let fixture = TempCaf::new().unwrap();
        let path = fixture.directory.join("chunk-walk");
        for (wav, format) in [(true, tags::Format::Wav), (false, tags::Format::Aiff)] {
            for marker in [b"PLTN", b"ID3 ", b"id3 "] {
                let mut bytes = prefix(wav);
                chunk(&mut bytes, marker, 0, &[], wav);
                if wav { chunk(&mut bytes, b"data", 4, &[0; 4], true); }
                else { chunk(&mut bytes, b"SSND", 8, &[0; 8], false); }
                fs::write(&path, bytes).unwrap();
                assert!(verify_unavailable_riff_marker_absent(&path, format, "fixture").unwrap_err().message.contains("possible PLTN or ID3"));
            }
            let mut bytes = prefix(wav);
            if wav { chunk(&mut bytes, b"data", 100, b"PLTN", true); }
            else { chunk(&mut bytes, b"SSND", 100, b"\0\0\0\0\0\0\0\0PLTN", false); }
            fs::write(&path, &bytes).unwrap();
            verify_unavailable_riff_marker_absent(&path, format, "fixture").unwrap();

            let mut partial_header = prefix(wav);
            partial_header.extend_from_slice(b"PLT");
            fs::write(&path, partial_header).unwrap();
            assert!(verify_unavailable_riff_marker_absent(&path, format, "fixture").is_err());
            let mut truncated_metadata = prefix(wav);
            chunk(&mut truncated_metadata, b"JUNK", 100, b"XX", wav);
            fs::write(&path, truncated_metadata).unwrap();
            assert!(verify_unavailable_riff_marker_absent(&path, format, "fixture").is_err());
            let mut garbage = prefix(wav);
            garbage.extend_from_slice(&[0xff; 8]);
            fs::write(&path, garbage).unwrap();
            assert!(verify_unavailable_riff_marker_absent(&path, format, "fixture").is_err());
        }
        fs::remove_file(path).unwrap();
        fixture.cleanup().unwrap();
    }


    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    #[test]
    fn apple_finish_only_marks_unwrapped_cancellation_as_cancelled() {
        let cancelled = apple_finish_error(apple_codec::CodecError::Cancelled);
        assert!(cancelled.cancelled);
        let cleanup = apple_finish_error(apple_codec::CodecError::Cleanup {
            primary: Box::new(apple_codec::CodecError::Cancelled),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "cleanup denied"),
        });
        assert!(!cleanup.cancelled);
        assert!(cleanup.message.contains("cleanup denied"));
    }

    #[test]
    fn marker_report_retains_exact_decoded_pitch_number() {
        let encoded = metadata::encode_marker_base64(br#"{"inputPitch":437.76852416992188}"#).unwrap();
        let marker = Marker::from_encoded_bytes(&encoded).unwrap();
        assert_eq!(marker.input_pitch.to_bits(), 0x407b_5c4b_e000_0000);
        let report = marker_json(Some(&marker)).unwrap().unwrap();
        assert_eq!(report["inputPitch"].as_f64().unwrap().to_bits(), 0x407b_5c4b_e000_0000);
    }

    #[test]
    fn ambiguous_marker_override_is_explicit_and_wav_only() {
        let incomplete = metadata::encode_marker_base64(br#"{"isProcessed":"#).unwrap();
        let processed = metadata::encode_marker_base64(br#"{"isProcessed":true}"#).unwrap();
        for (payload, kind) in [(b"".as_slice(), "empty_wav_pltn"), (incomplete.as_slice(), "incomplete_json")] {
            let error = decode_process_marker(Some(payload), tags::Format::Wav, false).err().unwrap();
            assert!(error.message.contains("--allow-ambiguous-marker"));
            assert!(error.message.contains("may reprocess"));
            assert!(matches!(decode_process_marker(Some(payload), tags::Format::Wav, true),
                Ok(InspectedMarker::AuthorizedAmbiguous(actual)) if actual == kind));
            for format in [tags::Format::Mp3, tags::Format::Mp4, tags::Format::Aiff, tags::Format::Flac] {
                assert!(decode_process_marker(Some(payload), format, true).is_err());
            }
        }
        assert!(matches!(decode_process_marker(Some(&processed), tags::Format::Wav, true),
            Ok(InspectedMarker::Known(Some(marker))) if marker.is_processed));
        assert!(matches!(decode_process_marker(None, tags::Format::Wav, true), Ok(InspectedMarker::Known(None))));
        assert!(decode_process_marker(Some(b"!!!!"), tags::Format::Wav, true).is_err());
        let malformed = metadata::encode_marker_base64(br#"{"isProcessed":truX}"#).unwrap();
        assert!(decode_process_marker(Some(&malformed), tags::Format::Wav, true).is_err());
        let invalid_field = metadata::encode_marker_base64(br#"{"isProcessed":true,"targetVolume":[]}"#).unwrap();
        assert!(decode_process_marker(Some(&invalid_field), tags::Format::Wav, true).is_err());
    }

    #[test]
    fn wav_processing_refuses_duplicate_markers_and_ambiguous_trailing_bytes() {
        fn wave(payloads: &[&[u8]]) -> Vec<u8> {
            let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
            for &payload in payloads { bytes.extend(metadata::wav_pltn_chunk(Some(payload)).unwrap()); }
            let length = (bytes.len() - 8) as u32;
            bytes[4..8].copy_from_slice(&length.to_le_bytes());
            bytes
        }
        fn layout(bytes: Vec<u8>) -> metadata::WavMarkerLayout {
            metadata::wav_marker_layout(&mut std::io::Cursor::new(bytes)).unwrap()
        }
        let processed = metadata::encode_marker_base64(br#"{"isProcessed":true}"#).unwrap();
        let unprocessed = metadata::encode_marker_base64(br#"{"isProcessed":false}"#).unwrap();
        let duplicate = layout(wave(&[&processed, &unprocessed]));
        for allow in [false, true] {
            let error = admit_process_marker(Some(&unprocessed), tags::Format::Wav, allow, Some(duplicate)).err().unwrap();
            assert!(error.message.contains("multiple WAV PLTN"));
        }
        let mut trailing = wave(&[b""]);
        trailing.extend(metadata::wav_pltn_chunk(Some(&processed)).unwrap());
        let trailing_layout = layout(trailing);
        let error = admit_process_marker(Some(b""), tags::Format::Wav, true, Some(trailing_layout)).err().unwrap();
        assert_eq!(error.stage, "decode marker");

        let mut absent_trailer = wave(&[]);
        absent_trailer.extend(metadata::wav_pltn_chunk(Some(&processed)).unwrap());
        let absent_trailer = layout(absent_trailer);
        let mut unprocessed_trailer = wave(&[&unprocessed]);
        unprocessed_trailer.extend(metadata::wav_pltn_chunk(Some(&processed)).unwrap());
        let unprocessed_trailer = layout(unprocessed_trailer);
        let mut benign_absent = wave(&[]);
        benign_absent.extend_from_slice(b"helloP");
        let benign_absent = layout(benign_absent);
        let mut benign_unprocessed = wave(&[&unprocessed]);
        benign_unprocessed.extend_from_slice(b"helloP");
        let benign_unprocessed = layout(benign_unprocessed);
        let mut benign_ambiguous = wave(&[b""]);
        benign_ambiguous.extend_from_slice(b"helloP");
        let benign_ambiguous = layout(benign_ambiguous);
        let mut split_buffer = wave(&[]);
        split_buffer.extend(std::iter::repeat_n(b'x', 8188));
        split_buffer.extend_from_slice(b"PLTN");
        let split_buffer = layout(split_buffer);
        for allow in [false, true] {
            for (payload, source_layout) in [
                (None, absent_trailer),
                (Some(unprocessed.as_slice()), unprocessed_trailer),
                (None, split_buffer),
            ] {
                let error = admit_process_marker(payload, tags::Format::Wav, allow, Some(source_layout)).err().unwrap();
                assert_eq!(error.stage, "decode marker");
            }
            assert!(matches!(admit_process_marker(None, tags::Format::Wav, allow, Some(benign_absent)),
                Ok(InspectedMarker::Known(None))));
            assert!(matches!(admit_process_marker(Some(&unprocessed), tags::Format::Wav, allow, Some(benign_unprocessed)),
                Ok(InspectedMarker::Known(Some(marker))) if !marker.is_processed));
        }
        let error = admit_process_marker(Some(b""), tags::Format::Wav, true, Some(benign_ambiguous)).err().unwrap();
        assert_eq!(error.stage, "decode marker");

        let fixture = TempCaf::new().unwrap();
        let source = fixture.directory.join("single-processed.wav");
        fs::write(&source, wave(&[&processed])).unwrap();
        let selected = file_workflow::discover_inputs(&[source.clone()]).unwrap().remove(0);
        let settings = file_workflow::OutputSettings { mode: file_workflow::OutputMode::SameFolder, format: AudioFormat::Wav };

        for allow in [false, true] {
            assert!(matches!(admit_process_marker(None, tags::Format::Wav, allow, Some(layout(wave(&[])))),
                Ok(InspectedMarker::Known(None))));
            assert!(matches!(admit_process_marker(Some(&unprocessed), tags::Format::Wav, allow,
                Some(layout(wave(&[&unprocessed])))), Ok(InspectedMarker::Known(Some(marker))) if !marker.is_processed));
            let marker = match admit_process_marker(Some(&processed), tags::Format::Wav, allow,
                Some(layout(wave(&[&processed])))).unwrap_or_else(|error| panic!("{error}")) {
                InspectedMarker::Known(Some(marker)) => marker,
                _ => panic!("valid processed marker was not preserved"),
            };
            assert!(marker.is_processed);
            assert!(matches!(OutputPlan::new(&selected, SourceCodec::Lpcm, &settings, Some(&marker)),
                Err(WorkflowError::AlreadyProcessed { .. })));
        }
        fs::remove_file(source).unwrap();
        fixture.cleanup().unwrap();
    }

    #[test]
    fn waveform_preserves_read_boundaries_and_channel_mean_absolute_values() {
        let mut waveform = WaveformBuilder::new(Path::new("fixture.wav"), 44_100.0, 2);
        let mut samples = [0.0_f32; 1024];
        samples[0] = -1.0;
        samples[2] = 0.5;
        samples[1] = -0.25;
        samples[3] = 1.0;
        waveform.feed(&samples).unwrap();
        waveform.feed(&[0.25, -0.75]).unwrap();
        waveform.feed(&[]).unwrap();
        let result = waveform.finish();
        assert_eq!(result.frames, 513);
        assert_eq!(result.bins.len(), 2);
        assert_eq!(result.bins[0].frames, 512);
        assert_eq!(result.bins[0].left_mean_absolute.to_bits(), 0x3b40_0000);
        assert_eq!(result.bins[0].right_mean_absolute.to_bits(), 0x3b20_0000);
        assert_eq!(result.bins[1].start_frame, 512);
        assert_eq!(result.bins[1].frames, 1);
        assert_eq!(result.bins[1].left_mean_absolute.to_bits(), 0x3e80_0000);
        assert_eq!(result.bins[1].right_mean_absolute.to_bits(), 0x3f40_0000);
    }

    #[test]
    fn waveform_duplicates_mono_and_selects_last_two_multichannel_values() {
        let mut mono = WaveformBuilder::new(Path::new("mono.wav"), 44_100.0, 1);
        mono.feed(&[-0.25, 0.75]).unwrap();
        let mono = mono.finish();
        assert_eq!(mono.bins[0].left_mean_absolute.to_bits(), 0x3f00_0000);
        assert_eq!(mono.bins[0].right_mean_absolute.to_bits(), 0x3f00_0000);
        let mut surround = WaveformBuilder::new(Path::new("multi.wav"), 44_100.0, 3);
        surround.feed(&[1.0, -0.25, 0.5, 1.0, 0.5, -0.375]).unwrap();
        let surround = surround.finish();
        assert_eq!(surround.bins[0].left_mean_absolute.to_bits(), 0x3ec0_0000);
        assert_eq!(surround.bins[0].right_mean_absolute.to_bits(), 0x3ee0_0000);
    }

    #[test]
    fn waveform_does_not_hide_nonfinite_channel_values() {
        let mut waveform = WaveformBuilder::new(Path::new("nonfinite.wav"), 44_100.0, 2);
        waveform.feed(&[f32::NAN, f32::NEG_INFINITY]).unwrap();
        let result = serde_json::to_value(waveform.finish()).unwrap();
        assert_eq!(result["bins"][0]["left_mean_absolute"], "NaN");
        assert_eq!(result["bins"][0]["right_mean_absolute"], "Infinity");
    }

    #[test]
    #[ignore = "requires separately captured MIKWaveformAnalyzer JSONL in PN_WAVEFORM_NATIVE_DIR"]
    fn waveform_reducer_matches_native_component_words() {
        let root = PathBuf::from(std::env::var_os("PN_WAVEFORM_NATIVE_DIR").expect("native waveform capture directory"));
        for name in ["stereo-512-plus-1", "mono", "three-channel"] {
            let text = fs::read_to_string(root.join(format!("{name}.stdout"))).unwrap();
            let records: Vec<serde_json::Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            let identity = records.iter().find(|record| record["record"] == "identity").unwrap();
            assert_eq!(identity["universal_sha256"], "67eafcae58496a56c026804e675a2e234aa91a06502319dde8f8a568f9a34a23");
            assert_eq!(identity["signature_check_status"], 0);
            let first = records.iter().find(|record| record["record"] == "input").unwrap();
            let channels = first["channels"].as_u64().unwrap() as usize;
            let word = |value: &serde_json::Value| {
                u32::from_str_radix(value.as_str().unwrap().strip_prefix("0x").unwrap(), 16).unwrap()
            };
            let mut waveform = WaveformBuilder::new(Path::new(name), 44_100.0, channels);
            let mut calls = 0;
            for record in &records {
                if record["record"] == "input" {
                    assert_eq!(record["channels"].as_u64().unwrap() as usize, channels);
                    let samples: Vec<f32> = record["sample_words"].as_array().unwrap()
                        .iter().map(|value| f32::from_bits(word(value))).collect();
                    assert_eq!(samples.len(), record["frames"].as_u64().unwrap() as usize * channels);
                    waveform.feed(&samples).unwrap();
                    calls += 1;
                } else if record["record"] == "analyze" {
                    assert_eq!(record["status"], 0);
                } else if record["record"] == "finish" {
                    assert_eq!(record["status"], 1);
                }
            }
            let native = records.iter().find(|record| record["record"] == "spectrum").unwrap();
            let result = waveform.finish();
            assert_eq!(result.bins.len(), calls);
            assert_eq!(result.bins.len(), native["count"].as_u64().unwrap() as usize);
            for (field, left) in [("left_words", true), ("right_words", false)] {
                let expected = native[field].as_array().unwrap();
                assert_eq!(expected.len(), result.bins.len());
                for (index, (bin, expected)) in result.bins.iter().zip(expected).enumerate() {
                    let actual = if left { bin.left_mean_absolute } else { bin.right_mean_absolute };
                    assert_eq!(actual.to_bits(), word(expected), "{name} {field} bucket {index}");
                }
            }
        }
    }

    #[test]
    fn cancellation_and_nonfinite_measurements_remain_explicit() {
        let cancel = Cancellation::new();
        cancel.cancel();
        let error = check_cancel(&cancel, "analysis").unwrap_err();
        assert!(error.cancelled);
        assert_eq!(error.stage, "analysis");
        #[derive(Serialize)]
        struct MeasuredFloat(#[serde(serialize_with = "float_json")] f32);
        assert_eq!(serde_json::to_value(MeasuredFloat(f32::NEG_INFINITY)).unwrap(), "-Infinity");
        assert_eq!(serde_json::to_value(MeasuredFloat(f32::NAN)).unwrap(), "NaN");
    }

    #[test]
    fn cancelled_publication_preserves_backup_and_cleanup_failures() {
        let backup = PathBuf::from("/fixture/retained-backup.wav");
        let cancelled = WorkflowError::Transaction {
            cause: Box::new(WorkflowError::Cancelled), backup: Some(backup.clone()), recovery: None,
            rollback: None, cleanup: Vec::new(), published: false,
        };
        let error = EngineError::workflow("publish output", cancelled);
        assert!(error.cancelled);
        assert_eq!(error.retained_backup.as_ref(), Some(&backup));
        assert!(!error.partial_publication);

        let failure = WorkflowError::Transaction {
            cause: Box::new(WorkflowError::Cancelled), backup: Some(backup.clone()), recovery: None,
            rollback: None, cleanup: vec![file_workflow::CleanupFailure {
                operation: "remove staging file", path: PathBuf::from("/fixture/staging"),
                source: io::Error::new(io::ErrorKind::PermissionDenied, "fixture cleanup failure"),
            }], published: false,
        };
        let error = EngineError::workflow("publish output", failure);
        assert!(!error.cancelled);
        assert_eq!(error.retained_backup.as_ref(), Some(&backup));
        assert!(error.message.contains("cleanup"));

        let recovery = PathBuf::from("/fixture/retained-stage.wav");
        let failure = WorkflowError::Transaction {
            cause: Box::new(WorkflowError::DestinationChanged("/fixture/output.wav".into())),
            backup: None, recovery: Some(recovery.clone()), rollback: None, cleanup: Vec::new(), published: false,
        };
        let error = EngineError::workflow("publish output", failure);
        assert_eq!(error.recovery_path.as_ref(), Some(&recovery));
        assert!(!error.partial_publication);
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
    fn primary_changed_during_original_analysis_invalidates_combined_report() {
        use std::io::Write;
        let fixture = TempCaf::new().unwrap();
        let backup = fixture.directory.join("original.wav");
        let primary = fixture.directory.join("processed.wav");
        let frames = 96_000u32;
        let mut wav = Vec::with_capacity((44 + frames * 2) as usize);
        wav.extend_from_slice(b"RIFF"); wav.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt "); wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes()); wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes()); wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data"); wav.extend_from_slice(&(frames * 2).to_le_bytes());
        for frame in 0..frames {
            let sample = ((frame as f64 * std::f64::consts::TAU * 440.0 / 48_000.0).sin() * 16_000.0) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(&backup, &wav).unwrap();
        fs::write(&primary, &wav).unwrap();
        let mut settings = Settings::defaults(&fixture.directory, 1);
        settings.libraries.tag_bridge = Some(PathBuf::from(std::env::var_os("PN_TAG_BRIDGE").expect("trusted ABI-5 tag bridge")));
        let marker = Marker::from_processing(ProcessingMarker {
            software_version: Some(VERSION), backup_file_name: Some(backup.to_str().unwrap()),
            applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
            expansion_applied: 0, compression_applied: 0, volume_analysis_version: 4,
            target_volume: -8.0, input_volume: -12.0, output_volume: -8.0,
            clip_repair_count: 0, exciter_option: -1, input_pitch: 440.0,
            output_pitch: 440.0, speed_adjustment: 1.0,
        });
        let bridge = bridge(&settings).unwrap();
        let transfer = bridge.transfer(&backup, &primary, tags::Format::Wav, tags::Format::Wav,
            tags::OutputChoice::Explicit, &marker).unwrap();
        assert!(transfer.is_success());
        let mut analyzed_backup = false;
        let error = analyze_file_with(&primary, &settings, &Cancellation::new(), |path, settings, cancel| {
            let result = analyze(path, settings, cancel, true)?;
            analyzed_backup = true;
            OpenOptions::new().append(true).open(&primary).unwrap().write_all(b"changed").unwrap();
            Ok(result)
        }).unwrap_err();
        assert!(analyzed_backup);
        assert_eq!(error.stage, "analyze source identity");
        assert!(!error.cancelled);
        assert_eq!(fs::read(&backup).unwrap(), wav);
        fs::remove_dir_all(&fixture.directory).unwrap();
    }

    /// This reads an already-generated native capture, never loads the
    /// original binaries. It checks the full mono decoder → stereo graph →
    /// system mono-converter path, including warmup and callback subdivision.
    #[test]
    #[ignore = "requires PN_ENGINE_GRAPH_GAIN_MONO native JSONL capture"]
    fn mono_gain_graph_matches_generated_reference_first_block() {
        let path = std::env::var_os("PN_ENGINE_GRAPH_GAIN_MONO").expect("set native graph capture path");
        let mut identity = false;
        let mut reference = None;
        for line in BufReader::new(fs::File::open(path).unwrap()).lines() {
            let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            if value["record"] == "identity" {
                assert_eq!(value["kind"], "multipass");
                assert_eq!(value["source_channels"], 1);
                assert_eq!(value["source_frames"], 513);
                assert_eq!(value["preset_sha256"], "4eb841415b3cf6f1970fc7bd7e2fcd3b621f0f7ad495e684e91afe58b1a705db");
                identity = true;
            }
            if value["record"] == "render" && value["offset"] == 0 {
                let samples = value["samples"][0].as_array().unwrap().iter().map(|word| {
                    u32::from_str_radix(word.as_str().unwrap(), 16).unwrap()
                }).collect::<Vec<_>>();
                reference = Some(samples);
            }
        }
        assert!(identity);
        let reference = reference.expect("missing first native render");
        assert_eq!(reference.len(), BLOCK);
        let temp = TempCaf::new().unwrap();
        let path = temp.directory.join("source.wav");
        let frames = 513u32;
        let mut bytes = Vec::with_capacity(44 + frames as usize * 2);
        bytes.extend_from_slice(b"RIFF"); bytes.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt "); bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes()); bytes.extend_from_slice(&96_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes()); bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data"); bytes.extend_from_slice(&(frames * 2).to_le_bytes());
        for i in 0..frames {
            let value = (((i * 17) % 101) as i32 - 50) as i16 * 256;
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        fs::write(&path, bytes).unwrap();
        let mut reader = AudioReader::open(&path, None, &InputLibraries::default()).unwrap();
        reader.set_stereo_planar(48_000.0).unwrap();
        let mut left = [0.0; BLOCK]; let mut right = [0.0; BLOCK];
        assert_eq!(reader.read_stereo_planar(&mut left, &mut right).unwrap(), BLOCK);
        let mut graph = MultipassUnit::new(48_000.0, 2, ShippedPreset::Gain, [false; 5], &[]).unwrap();
        graph.process(&mut left, &mut right).unwrap();
        let mut output = [0.0; BLOCK];
        let mut converter = ChannelConverter::new(48_000.0, 1).unwrap();
        converter.convert(&left, &right, &mut output).unwrap();
        for (i, (&sample, &expected)) in output.iter().zip(&reference).enumerate() {
            assert_eq!(sample.to_bits(), expected, "mono graph frame {i}");
        }
        converter.dispose().unwrap();
        reader.close().unwrap();
        fs::remove_file(path).unwrap();
        temp.cleanup().unwrap();
    }
}
