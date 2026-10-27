//! Streaming Platinum Notes controller integration. The implementations behind
//! these calls are native reconstructions, not the proprietary application.
//! Complete encoded-file identity is a separate, not-yet-established gate.

use crate::apple_codec::{
    self, AudioBuffer, AudioBufferList, StereoBufferList, StreamDescription,
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
use crate::metadata::{Marker, ProcessingMarker};
use crate::multipass::{CrossoverFrequencyScale, ParameterWrite, ShippedPreset};
use crate::multipass_graph::{GraphContext, MultipassGraph};
use crate::phase::PhaseCorrection;
use crate::pitch::{PitchMode, PitchProcessor};
use crate::pitch_detect::PitchDetector;
use crate::settings::Settings;
use crate::tags::{self, TagBridge};
use crate::templates::Template;
use serde::Serialize;
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const BLOCK: usize = controller::RENDER_BLOCK_FRAMES as usize;
const VERSION: &str = "10.5.0.1256";

/// An error is never a successful or partially successful processing report.
/// `partial_publication` means the destination was published before an error;
/// `retained_backup` identifies a backup that must not be discarded.
#[derive(Debug, Serialize)]
pub struct EngineError {
    pub stage: &'static str,
    pub message: String,
    pub cancelled: bool,
    pub retained_backup: Option<PathBuf>,
    pub partial_publication: bool,
}
impl EngineError {
    fn new(stage: &'static str, error: impl fmt::Display) -> Self {
        Self { stage, message: error.to_string(), cancelled: false, retained_backup: None, partial_publication: false }
    }
    fn cancelled(stage: &'static str) -> Self {
        Self { stage, message: "processing cancelled".into(), cancelled: true, retained_backup: None, partial_publication: false }
    }
    fn workflow(stage: &'static str, error: WorkflowError) -> Self {
        let (backup, published) = match &error {
            WorkflowError::Transaction { backup, published, .. } => (backup.clone(), *published),
            _ => (None, false),
        };
        let cancelled = error.is_cancelled();
        Self { stage, message: error.to_string(), cancelled, retained_backup: backup, partial_publication: published }
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
    pub processed: bool,
    pub processing_marker: Option<serde_json::Value>,
    pub dynamics: Option<ReportedDynamics>,
    /// First-pass analysis does not run the repair/dynamics nodes.
    pub unavailable: Vec<String>,
    pub waveform: Waveform,
    /// Present when the current file's marker names a readable original.
    pub original_waveform: Option<Waveform>,
    pub original_waveform_unavailable: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct ReportedDynamics {
    pub source: &'static str,
    pub expansion_applied: bool,
    pub compression_applied: bool,
}

/// Per-second extrema of decoded graph-output samples, excluding padded EOF frames.
#[derive(Debug, Serialize)]
pub struct Waveform {
    pub path: PathBuf,
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
    pub minimum: Option<f32>,
    pub maximum: Option<f32>,
    pub non_finite_samples: u64,
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
    pub tags: tags::TagInspection,
    pub processed: bool,
    pub processing_state: &'static str,
    pub backup_path: Option<PathBuf>,
    pub backup_exists: Option<bool>,
    pub backup_error: Option<String>,
    pub marker_json: Option<serde_json::Value>,
    #[serde(skip)]
    pub marker: Option<Marker>,
    #[serde(skip)]
    pub source_codec: SourceCodec,
}

fn input_libraries(settings: &Settings) -> InputLibraries {
    InputLibraries { flac: settings.libraries.flac.clone(), flac_sha256: settings.libraries.flac_sha256.clone() }
}
fn bridge(settings: &Settings) -> Result<TagBridge, EngineError> {
    let path = settings.libraries.tag_bridge.as_ref().ok_or_else(|| EngineError::new("tags", "a trusted TagLib bridge path is required"))?;
    // Settings selects the native executable. As with the codec dylibs the
    // caller is responsible for its provenance and executable initializers.
    unsafe { TagBridge::load(path) }.map_err(|e| EngineError::new("tags", e))
}
fn source_tag_format(path: &Path) -> Result<tags::Format, EngineError> {
    match file_workflow::source_type(path) {
        Some(SourceType::Mp3) => Ok(tags::Format::Mp3),
        Some(SourceType::M4a) => Ok(tags::Format::Mp4),
        Some(SourceType::Wav) => Ok(tags::Format::Wav),
        Some(SourceType::Aiff) => Ok(tags::Format::Aiff),
        Some(SourceType::Flac) => Ok(tags::Format::Flac),
        Some(SourceType::ProtectedM4p | SourceType::ProtectedWlMp3) => Err(EngineError::new("input", "protected source is not admitted")),
        _ => Err(EngineError::new("input", format!("unsupported source {}", path.display()))),
    }
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
fn read_marker(bridge: &TagBridge, path: &Path, format: tags::Format) -> Result<Option<Marker>, EngineError> {
    bridge.read_processing_marker(path, format).map_err(|e| EngineError::new("read marker", e))?
        .map(|payload| Marker::from_encoded_bytes(&payload).map_err(|e| EngineError::new("decode marker", e))).transpose()
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

/// Inspect using the actual decoder, container tag reader and decoded marker.
/// Absence and a malformed present marker are distinct outcomes.
pub fn inspect_file(path: &Path, settings: &Settings) -> Result<InspectionReport, EngineError> {
    require_reportable_path(path, "inspect input")?;
    let identity = file_workflow::FileIdentity::capture(path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let family = source_tag_format(path)?;
    let bridge = bridge(settings)?;
    let tag_inspection = bridge.inspect(path, family).map_err(|e| EngineError::new("inspect tags", e))?;
    let marker = read_marker(&bridge, path, family)?;
    let state = file_workflow::processed_state(path, marker.as_ref()).map_err(|e| EngineError::workflow("inspect marker", e))?;
    let (processing_state, backup_path) = match &state {
        ProcessingState::Unprocessed => ("unprocessed", None),
        ProcessingState::Suffixed => ("suffixed", None),
        ProcessingState::Marked { backup } => ("marked", backup.clone()),
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
    let frames = reader.file_length_frames().map_err(|e| EngineError::new("inspect length", e))?;
    if frames < 0 { return Err(EngineError::new("inspect length", "decoder returned a negative frame count")); }
    let (bitrate, bitrate_error) = match reader.bitrate() { Ok(b) => (Some(b), None), Err(e) => (None, Some(e.to_string())) };
    reader.close().map_err(|e| EngineError::new("close input", e))?;
    identity.verify(path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let marker_json = marker_json(marker.as_ref())?;
    Ok(InspectionReport { path: path.to_owned(), input_format: format_label(path, codec), sample_rate: source.sample_rate,
        channels: source.channels_per_frame, bit_depth: source.bits_per_channel, frames: frames as u64,
        bitrate, bitrate_error, processed: state != ProcessingState::Unprocessed, processing_state,
        backup_path, backup_exists, backup_error, marker_json, tags: tag_inspection, marker, source_codec: codec })
}

/// A plan and the source identity and codec on which it was based.
/// Keep this intact through batch validation and worker dispatch.
#[derive(Debug)]
pub struct PreparedProcess {
    plan: OutputPlan,
    identity: FileIdentity,
    source_codec: SourceCodec,
    input_format: String,
}
impl PreparedProcess {
    pub fn output_plan(&self) -> &OutputPlan { &self.plan }
}

/// Inspect and plan a source for later batch validation and processing.
pub fn prepare_file(input: &DiscoveredFile, settings: &Settings) -> Result<PreparedProcess, EngineError> {
    let identity = FileIdentity::capture(&input.path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let inspection = inspect_file(&input.path, settings)?;
    identity.verify(&input.path).map_err(|e| EngineError::workflow("inspect source identity", e))?;
    let plan = OutputPlan::new(input, inspection.source_codec, &settings.output, inspection.marker.as_ref())
        .map_err(|e| EngineError::workflow("plan output", e))?;
    identity.verify(&input.path).map_err(|e| EngineError::workflow("plan source identity", e))?;
    require_reportable_path(plan.source(), "plan output")?;
    require_reportable_path(plan.destination(), "plan output")?;
    if let Some(path) = plan.backup_directory() { require_reportable_path(path, "plan backup")?; }
    Ok(PreparedProcess { plan, identity, source_codec: inspection.source_codec, input_format: inspection.input_format })
}

struct Measured {
    values: AnalysisValues,
    frames: i64,
    processing_rate: f64,
    source: StreamDescription,
    codec: SourceCodec,
    waveform: Option<Waveform>,
}

struct WaveformBuilder {
    path: PathBuf,
    rate: f64,
    channels: usize,
    layout: &'static str,
    frames_per_bin: u32,
    total: u64,
    count: u32,
    min: Option<f32>,
    max: Option<f32>,
    non_finite: u64,
    bins: Vec<WaveformBin>,
}
impl WaveformBuilder {
    fn new(path: &Path, rate: f64, channels: usize) -> Self {
        Self { path: path.to_owned(), rate, channels, layout: "interleaved_graph_output", frames_per_bin: (rate as u32).max(1),
            total: 0, count: 0, min: None, max: None, non_finite: 0, bins: Vec::new() }
    }
    fn feed(&mut self, samples: &[f32]) {
        for frame in samples.chunks_exact(self.channels) {
            for &sample in frame {
                if sample.is_finite() {
                    self.min = Some(self.min.map_or(sample, |v| v.min(sample)));
                    self.max = Some(self.max.map_or(sample, |v| v.max(sample)));
                } else { self.non_finite += 1; }
            }
            self.count += 1;
            self.total += 1;
            if self.count == self.frames_per_bin { self.flush(); }
        }
    }
    fn flush(&mut self) {
        if self.count == 0 { return; }
        self.bins.push(WaveformBin { start_frame: self.total - u64::from(self.count), frames: self.count,
            minimum: self.min.take(), maximum: self.max.take(), non_finite_samples: self.non_finite });
        self.count = 0;
        self.non_finite = 0;
    }
    fn finish(mut self) -> Waveform {
        self.flush();
        Waveform { path: self.path, sample_rate: self.rate, channels: self.channels as u32, layout: self.layout, frames: self.total, bins: self.bins }
    }
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
    let rate = controller::processing_sample_rate(source.sample_rate, codec == SourceCodec::Mp3);
    original.close().map_err(|e| EngineError::new("close analysis input", e))?;
    let mut reader = AudioReader::open(path, Some(rate), &libraries).map_err(|e| EngineError::new("open analysis input", e))?;
    reader.set_stereo_planar(rate).map_err(|e| EngineError::new("configure analysis input", e))?;
    let channels = source.channels_per_frame as usize;
    if channels == 0 { return Err(EngineError::new("analysis", "decoder has no channels")); }
    let mut meter = LoudnessMeter::new(rate, channels).map_err(|e| EngineError::new("analysis meter", e))?;
    let mut detector = PitchDetector::new(rate as f32, channels).map_err(|e| EngineError::new("analysis pitch detector", e))?;
    let mut accounting = FrameAccounting::new(RenderPass::Analysis, None);
    let mut samples = vec![0.0; BLOCK.checked_mul(channels).ok_or_else(|| EngineError::new("analysis", "channel count overflows"))?];
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut converter = ChannelConverter::new(rate, channels)?;
    let mut waveform = capture_waveform.then(|| WaveformBuilder::new(path, rate, channels));
    loop {
        left.fill(0.0); right.fill(0.0);
        let read = reader.read_stereo_planar(&mut left, &mut right).map_err(|e| EngineError::new("read analysis input", e))?;
        converter.convert(&left, &right, &mut samples)?;
        if let Some(waveform) = &mut waveform { waveform.feed(&samples[..read * channels]); }
        let source_read = accounting.source_read(BLOCK as u32, read as u32).map_err(|e| EngineError::new("analysis accounting", format!("{e:?}")))?;
        meter.process_interleaved(&samples).map_err(|e| EngineError::new("analysis meter", e))?;
        detector.process_interleaved(&samples).map_err(|e| EngineError::new("analysis pitch detector", e))?;
        let flags = RENDER_POST | if source_read.signal_silence { RENDER_SILENCE } else { 0 };
        if accounting.finish_render(BLOCK as u32, flags, cancel.is_cancelled()) { break; }
    }
    reader.close().map_err(|e| EngineError::new("close analysis input", e))?;
    converter.dispose()?;
    check_cancel(cancel, "analysis")?;
    Ok(Measured { values: AnalysisValues::from_measurements(meter.integrated_loudness(), detector.tuning_hz()),
        frames: accounting.actual_input_frames, processing_rate: rate, source, codec,
        waveform: waveform.map(WaveformBuilder::finish) })
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
    let (original_waveform, original_waveform_unavailable) = match state {
        ProcessingState::Marked { backup: Some(path) } => match require_reportable_path(&path, "analyze original")
            .and_then(|()| {
                let identity = file_workflow::FileIdentity::capture(&path).map_err(|e| EngineError::workflow("inspect original identity", e))?;
                let original = analyze_original(&path, settings, cancel)?;
                identity.verify(&path).map_err(|e| EngineError::workflow("analyze original identity", e))?;
                Ok(original)
            }) {
                Ok(original) => (original.waveform, None),
                Err(error) if error.cancelled => return Err(error),
                Err(error) => (None, Some(error.to_string())),
            },
        ProcessingState::Marked { backup: None } => (None, Some("processing marker does not record a backup path".to_owned())),
        ProcessingState::Suffixed => (None, Some("suffix-only processed state does not identify an original".to_owned())),
        ProcessingState::Unprocessed => (None, None),
    };
    let mut unavailable = if marked.is_some() { vec!["new clip and dynamics measurements require the healing pass".to_owned()] }
        else { vec!["clip-repair count and dynamics adjustments require the healing pass".to_owned()] };
    if measured.codec == SourceCodec::Flac && measured.source.channels_per_frame > 2 {
        unavailable.push("native FLAC planar reader supplies at most the first two source channels; graph waveform reflects its channel conversion".to_owned());
    }
    identity.verify(path).map_err(|e| EngineError::workflow("analyze source identity", e))?;
    Ok(AnalysisReport { path: path.to_owned(), input_format: inspection.input_format, sample_rate: measured.processing_rate,
        channels: measured.source.channels_per_frame, frames: measured.frames as u64, volume: measured.values.input_level_db,
        pitch_hz: measured.values.input_tuning_hz, clip_count, processed: inspection.processed,
        processing_marker: inspection.marker_json, dynamics,
        unavailable,
        waveform: measured.waveform.take().expect("requested waveform"), original_waveform, original_waveform_unavailable })
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
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
struct TempCaf { directory: PathBuf, path: PathBuf }
impl TempCaf {
    fn new() -> Result<Self, EngineError> {
        for _ in 0..128 {
            let name = format!("platinum-engine-{}-{}", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed));
            let directory = std::env::temp_dir().join(name);
            match fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => return Ok(Self { path: directory.join("healed.caf"), directory }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(EngineError::new("create intermediate directory", e)),
            }
        }
        Err(EngineError::new("create intermediate directory", "could not reserve a unique private directory"))
    }
    fn cleanup(&self) -> Result<(), EngineError> {
        match fs::remove_file(&self.path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(EngineError::new("remove intermediate", format!("{}: {e}", self.path.display()))),
        }
        fs::remove_dir(&self.directory).map_err(|e| EngineError::new("remove intermediate directory", format!("{}: {e}", self.directory.display())))
    }
}
impl Drop for TempCaf {
    fn drop(&mut self) { let _ = fs::remove_file(&self.path); let _ = fs::remove_dir(&self.directory); }
}

struct CafWriter { handle: Option<std::ptr::NonNull<std::ffi::c_void>>, channels: usize }
impl CafWriter {
    fn new(path: &Path, rate: f64, channels: usize) -> Result<Self, EngineError> {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let url = unsafe { CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 0) };
        if url.is_null() { return Err(EngineError::new("create intermediate", "CFURL creation failed")); }
        let frame_bytes = u32::try_from(channels.checked_mul(4).ok_or_else(|| EngineError::new("create intermediate", "channel count overflows"))?)
            .map_err(|e| EngineError::new("create intermediate", e))?;
        let format = StreamDescription { sample_rate: rate, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 9,
            bytes_per_packet: frame_bytes, frames_per_packet: 1, bytes_per_frame: frame_bytes,
            channels_per_frame: channels as u32, bits_per_channel: 32, reserved: 0 };
        let mut raw = std::ptr::null_mut();
        let status = unsafe { ExtAudioFileCreateWithURL(url, u32::from_be_bytes(*b"caff"), &format, std::ptr::null(), 0, &mut raw) };
        unsafe { CFRelease(url); }
        let handle = std::ptr::NonNull::new(raw);
        if status != 0 || handle.is_none() {
            if let Some(handle) = handle { unsafe { ExtAudioFileDispose(handle.as_ptr()); } }
            return Err(EngineError::new("create intermediate", format!("ExtAudioFileCreateWithURL: OSStatus {status}")));
        }
        let mut writer = Self { handle, channels };
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
}
impl Drop for CafWriter { fn drop(&mut self) { let _ = self.dispose(); } }

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
        return Err(EngineError::new("healing channel layout", format!("unsupported channel count {channels}")));
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
    let mut writer = CafWriter::new(&temp.path, rate, channels)?;
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut interleaved = vec![0.0f32; BLOCK * channels];
    let mut channel_converter = ChannelConverter::new(rate, channels)?;
    let mut stages = Vec::new();
    if plan.clip.is_some() { stages.push("clip_repair".to_owned()); }
    if plan.pitch.is_some() { stages.push("pitch".to_owned()); }
    if plan.warmth.is_some() { stages.push("warmth".to_owned()); }
    if plan.expansion.is_some() { stages.push("expansion".to_owned()); }
    if plan.compression.is_some() { stages.push("compression".to_owned()); }
    if plan.gain.is_some() { stages.push("gain".to_owned()); }
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
    writer.finish()?;
    Ok(HealingResult { analyzer, frames: frames as u64, clip_count, tempo_factor: tempo, stages })
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
            let path = settings.libraries.flac.as_ref().ok_or_else(|| EngineError::new("load FLAC encoder", "trusted libFLAC path is required"))?;
            let expected = settings.libraries.flac_sha256.as_ref().ok_or_else(|| EngineError::new("load FLAC encoder", "libFLAC SHA-256 pin is required"))?;
            let before = crate::mp3::file_sha256(path).map_err(|e| EngineError::new("hash FLAC encoder", e))?;
            if !before.eq_ignore_ascii_case(expected) { return Err(EngineError::new("load FLAC encoder", "libFLAC SHA-256 pin mismatch")); }
            let library = crate::flac::FlacLibrary::load(path).map_err(|e| EngineError::new("load FLAC encoder", e))?;
            let loaded: String = library.identity().file_sha256.iter().map(|b| format!("{b:02x}")).collect();
            if !loaded.eq_ignore_ascii_case(expected) { return Err(EngineError::new("load FLAC encoder", "libFLAC changed during loading")); }
            result.flac = Some(library);
        }
        if format == AudioFormat::Mp3 {
            let path = settings.libraries.lame.as_ref().ok_or_else(|| EngineError::new("load MP3 encoder", "trusted LAME path is required"))?;
            let digest = settings.libraries.lame_sha256.as_deref().ok_or_else(|| EngineError::new("load MP3 encoder", "LAME SHA-256 pin is required"))?;
            result.lame = Some(crate::mp3::LameLibrary::load(path, digest).map_err(|e| EngineError::new("load MP3 encoder", e))?);
        }
        Ok(result)
    }
}
enum Encoder<'a> {
    Apple(apple_codec::Writer),
    Flac(crate::flac::FlacEncoder<'a>),
    Mp3 { encoder: crate::mp3::Mp3Encoder<'a>, converter: apple_codec::Pcm16StereoConverter, scratch: [i16; BLOCK * 2] },
}
impl<'a> Encoder<'a> {
    fn new(path: &Path, format: AudioFormat, rate: f64, channels: u32, depth: u32,
           source_header: crate::mp3::SourceHeader, settings: &Settings, libraries: &'a EncodingLibraries) -> Result<Self, EngineError> {
        match format {
            AudioFormat::Mp3 => {
                let converter = apple_codec::Pcm16StereoConverter::new(rate, channels).map_err(|e| EngineError::new("create MP3 PCM converter", e))?;
                let config = crate::mp3::Mp3Config { sample_rate: rate as u32, channels: channels as u8,
                    mode: settings.mp3_mode.into(), source: source_header, native_update_result_byte_3: None };
                let encoder = crate::mp3::Mp3Encoder::create_new(libraries.lame.as_ref().expect("loaded LAME"), path, config)
                    .map_err(|e| EngineError::new("create MP3 encoder", e))?;
                Ok(Self::Mp3 { encoder, converter, scratch: [0; BLOCK * 2] })
            }
            AudioFormat::Flac => {
                let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(path)
                    .map_err(|e| EngineError::new("create FLAC staging file", e))?;
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
                let writer = apple_codec::Writer::create(path, apple_codec::WriterConfig { format, sample_rate: rate, channels, bit_depth: depth })
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
    fn finish(self) -> Result<(), EngineError> {
        match self {
            Self::Apple(w) => w.finish().map_err(|e| EngineError::new("finish Apple output", e)),
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

fn stereo_planar_format(rate: f64) -> StreamDescription {
    StreamDescription { sample_rate: rate, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 0x29,
        bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4,
        channels_per_frame: 2, bits_per_channel: 32, reserved: 0 }
}
fn interleaved_format(rate: f64, channels: usize) -> StreamDescription {
    StreamDescription { sample_rate: rate, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 9,
        bytes_per_packet: (channels * 4) as u32, frames_per_packet: 1, bytes_per_frame: (channels * 4) as u32,
        channels_per_frame: channels as u32, bits_per_channel: 32, reserved: 0 }
}

/// GenericOutput's planar-stereo input to interleaved source-channel output.
/// System AudioConverter owns channel mapping; no manual downmix is inferred.
struct ChannelConverter { handle: Option<std::ptr::NonNull<std::ffi::c_void>>, channels: usize }
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
impl Drop for ChannelConverter { fn drop(&mut self) { let _ = self.dispose(); } }


struct EncodedInfo { format: String, stream: StreamDescription, frames: u64 }
fn inspect_encoded(path: &Path, format: AudioFormat, settings: &Settings) -> Result<EncodedInfo, EngineError> {
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
fn final_pass(temp: &TempCaf, staging: &Path, measured: &Measured, healed: &HealingResult, template: &Template,
              format: AudioFormat, source_header: crate::mp3::SourceHeader, settings: &Settings,
              libraries: &EncodingLibraries, cancel: &Cancellation) -> Result<FinalResult, EngineError> {
    let processing_rate = measured.processing_rate;
    let channels = measured.source.channels_per_frame as usize;
    let output_code = match format { AudioFormat::Mp3 => 1, AudioFormat::Aac => 6, _ => 0 };
    let output_rate = controller::final_sample_rate(processing_rate, output_code);
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
    let mut encoder = Encoder::new(staging, format, output_rate, channels as u32, bit_depth, source_header, settings, libraries)?;
    // The original attaches the meter to the output callback and retains its
    // processing-rate configuration even when the output rate differs.
    let mut meter = LoudnessMeter::new(processing_rate, channels).map_err(|e| EngineError::new("create output meter", e))?;
    let mut rendered = vec![0.0f32; BLOCK * channels];
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
    encoder.finish()?;
    Ok(FinalResult { frames: frames as u64, output_volume: meter.integrated_loudness() as f32 })
}


fn abort_with(mut error: EngineError, staged: file_workflow::StagedOutput) -> EngineError {
    let backup = staged.backup_path().map(Path::to_owned);
    match staged.abort() {
        Ok(report) => error.retained_backup = report.retained_backup.or(backup),
        Err(cleanup) => {
            error.cancelled = false;
            error.message.push_str(&format!("; abort failed: {cleanup}"));
            if let WorkflowError::Transaction { backup: b, published, .. } = &cleanup {
                error.retained_backup = b.clone().or(backup);
                error.partial_publication |= *published;
            } else { error.retained_backup = backup; }
        }
    }
    error
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
    let run = (|| -> Result<(Measured, HealingResult, FinalResult, EncodedInfo, &'static str), EngineError> {
        let measured = analyze(input, settings, cancel, false)?;
        identity.verify(input).map_err(|e| EngineError::workflow("verify source after analysis", e))?;
        if measured.codec != prepared.source_codec {
            return Err(EngineError::new("analysis input", "source codec changed since inspection"));
        }
        let plan_healing = HealingPlan::from_analysis(template, measured.values);
        let healed = healing_pass(input, &temp, &measured, &plan_healing, plan.is_replacement(), settings, cancel)?;
        identity.verify(input).map_err(|e| EngineError::workflow("verify source after healing", e))?;
        let header = crate::mp3::inspect_mp3_header(input).map_err(|e| EngineError::new("inspect source MP3 header", e))?;
        let source_header = if measured.codec == SourceCodec::Mp3 { crate::mp3::SourceHeader::Mp3(header) }
            else { crate::mp3::SourceHeader::Other(header) };
        let final_result = final_pass(&temp, staged.staging_path(), &measured, &healed, template, plan.format(), source_header, settings, &libraries, cancel)?;
        identity.verify(input).map_err(|e| EngineError::workflow("verify source after encoding", e))?;
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
        let report = bridge.transfer(tag_source, staged.staging_path(), source_tag_format(input)?,
            output_tag_format(plan.format()), choice, &marker).map_err(|e| EngineError::new("copy source tags", e))?;
        if !report.is_success() {
            return Err(EngineError::new("copy source tags", format!("copy error: {:?}; marker error: {:?}", report.copy_error, report.marker_error)));
        }
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
        Ok((measured, healed, final_result, encoded, marker_status))
    })();
    let cleanup = temp.cleanup();
    let (measured, healed, output, encoded, marker_status) = match (run, cleanup) {
        (Ok(result), Ok(())) => result,
        (Err(mut error), Err(cleanup)) => {
            error.cancelled = false;
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
        input_format: prepared.input_format.clone(), output_format: encoded.format,
        input_sample_rate: measured.source.sample_rate, processing_sample_rate: measured.processing_rate, sample_rate: encoded.stream.sample_rate,
        channels: measured.source.channels_per_frame, output_channels: encoded.stream.channels_per_frame,
        output_bit_depth: encoded.stream.bits_per_channel, encoded_length_frames: encoded.frames,
        input_frames: measured.frames as u64, healed_frames: healed.frames,
        output_frames: output.frames, input_volume: measured.values.input_level_db, healed_volume: healed.analyzer.integrated_loudness() as f32,
        output_volume: output.output_volume, input_pitch_hz: measured.values.input_tuning_hz, output_pitch_hz: output_pitch,
        tempo_factor: healed.tempo_factor, clip_count: healed.clip_count, applied_stages: stages, processing_marker_status: marker_status })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn waveform_bins_exclude_padding_and_preserve_nonfinite_counts() {
        let mut waveform = WaveformBuilder::new(Path::new("fixture.wav"), 2.0, 2);
        waveform.feed(&[-1.0, 0.5, 0.25, f32::NAN, 0.1, f32::INFINITY]);
        let result = waveform.finish();
        assert_eq!(result.frames, 3);
        assert_eq!(result.bins.len(), 2);
        assert_eq!(result.bins[0].start_frame, 0);
        assert_eq!(result.bins[0].frames, 2);
        assert_eq!(result.bins[0].minimum, Some(-1.0));
        assert_eq!(result.bins[0].maximum, Some(0.5));
        assert_eq!(result.bins[0].non_finite_samples, 1);
        assert_eq!(result.bins[1].start_frame, 2);
        assert_eq!(result.bins[1].frames, 1);
        assert_eq!(result.bins[1].non_finite_samples, 1);
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
