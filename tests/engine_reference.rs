//! Isolated generated fixtures for the streaming engine. These do not establish
//! byte identity against the original application. Native comparisons belong
//! to the parent-controlled reference capture and complete-file SHA-256 gate.
#![cfg(target_os = "macos")]

use platinum_engine::engine::{analyze_file, inspect_file, prepare_file, process_file};
use platinum_engine::file_workflow::{self, AudioFormat, Cancellation, OutputMode};
use platinum_engine::settings::{Settings, TemplateRef};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        loop {
            let p = std::env::temp_dir().join(format!("platinum-engine-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&p) {
                Ok(()) => return Self(p),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("cannot create fixture: {e}"),
            }
        }
    }
    fn input(&self, name: &str, frames: u32, channels: u16, malformed_marker: bool) -> PathBuf {
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF"); data.extend_from_slice(&0u32.to_le_bytes()); data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes()); data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&channels.to_le_bytes()); data.extend_from_slice(&48_000u32.to_le_bytes());
        data.extend_from_slice(&(48_000u32 * u32::from(channels) * 2).to_le_bytes());
        data.extend_from_slice(&(channels * 2).to_le_bytes()); data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data"); data.extend_from_slice(&(frames * u32::from(channels) * 2).to_le_bytes());
        for i in 0..frames {
            let sample = ((i as f64 * (std::f64::consts::TAU * 437.0 / 48_000.0)).sin() * 20_000.0) as i16;
            for channel in 0..channels { data.extend_from_slice(&(if channel == 0 { sample } else { sample / 2 }).to_le_bytes()); }
        }
        if malformed_marker { data.extend_from_slice(b"PLTN\x04\x00\x00\x00!!!!"); }
        let length = u32::try_from(data.len() - 8).unwrap(); data[4..8].copy_from_slice(&length.to_le_bytes());
        let path = self.0.join(name); fs::write(&path, data).unwrap(); path
    }
    fn settings(&self) -> Settings {
        let mut s = Settings::defaults(&self.0, 1);
        s.output.mode = OutputMode::SpecificFolder { directory: self.0.clone(), mirror: false };
        s.output.format = AudioFormat::Wav;
        s.libraries.tag_bridge = Some(PathBuf::from(std::env::var_os("PN_TAG_BRIDGE").expect("set PN_TAG_BRIDGE to trusted ABI-5 public TagLib bridge")));
        s
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
fn discovered(path: &Path) -> file_workflow::DiscoveredFile {
    file_workflow::discover_inputs(&[path.to_owned()]).unwrap().remove(0)
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn inspection_and_analysis_use_real_source_and_distinguish_malformed_markers() {
    let f = Fixture::new(); let s = f.settings();
    let input = f.input("source.wav", 48_513, 2, false);
    let bytes = fs::read(&input).unwrap();
    let info = inspect_file(&input, &s).unwrap();
    assert_eq!(info.input_format, "wav/pcm"); assert_eq!(info.channels, 2);
    assert_eq!(info.frames, 48_513); assert_eq!(info.sample_rate, 48_000.0);
    assert!(!info.processed && info.marker.is_none());
    let result = analyze_file(&input, &s, &Cancellation::new()).unwrap();
    assert_eq!(result.frames, 48_513); assert_eq!(result.waveform.frames, result.frames);
    assert_eq!(result.waveform.bins.len(), 2);
    assert_eq!(result.waveform.bins[0].frames, 48_000);
    assert_eq!(result.waveform.bins[1].frames, 513);
    assert!(result.volume.is_finite()); assert!(result.pitch_hz.is_finite());
    assert_eq!(result.clip_count, None); assert_eq!(fs::read(&input).unwrap(), bytes);
    let invalid = f.input("invalid.wav", 2_048, 2, true);
    let error = inspect_file(&invalid, &s).unwrap_err();
    assert_eq!(error.stage, "decode marker");
    assert!(prepare_file(&discovered(&invalid), &s).is_err());
    assert_eq!(fs::read(&input).unwrap(), bytes);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn stereo_process_exercises_all_stages_and_publishes_marked_wav_without_clobber() {
    let f = Fixture::new(); let s = f.settings();
    let input = f.input("stage-test.wav", 96_513, 2, false);
    let original = fs::read(&input).unwrap();
    let mut t = s.template(TemplateRef::Index(0)).unwrap();
    t.clip.enabled = true; t.clip.low = -200.0; t.clip.high = 100.0;
    t.pitch.enabled = true; t.pitch.threshold_cents = 0; t.pitch.option = 1;
    t.exciter_option = 0;
    t.expansion.enabled = true; t.expansion.low = -200.0; t.expansion.high = 100.0;
    t.compression.enabled = true; t.compression.low = -200.0; t.compression.high = 100.0;
    let prepared = prepare_file(&discovered(&input), &s).unwrap();
    let result = process_file(&prepared, &t, &s, &Cancellation::new()).unwrap();
    for name in ["clip_repair", "pitch", "warmth", "expansion", "compression", "limiter"] {
        assert!(result.applied_stages.iter().any(|v| v == name), "missing {name}");
    }
    assert_eq!(result.input_frames, 96_513); assert!(result.healed_frames > 0 && result.output_frames > 0);
    assert_eq!(result.processing_marker_status, "present");
    assert_eq!(result.input_format, "wav/pcm"); assert_eq!(result.output_format, "wav/pcm");
    assert_eq!(fs::read(&input).unwrap(), original);
    let output = inspect_file(&result.output_path, &s).unwrap();
    assert!(output.processed && output.marker.as_ref().unwrap().is_processed);
    let marked = output.marker.unwrap(); assert!(marked.applied_clip_repair && marked.applied_pitch_correction);
    assert!(marked.applied_warmth && marked.applied_expansion && marked.applied_compression);
    let produced = fs::read(&result.output_path).unwrap();
    let prepared = prepare_file(&discovered(&input), &s).unwrap();
    assert!(process_file(&prepared, &t, &s, &Cancellation::new()).is_err());
    assert_eq!(fs::read(&result.output_path).unwrap(), produced);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn replacement_retains_verified_backup_and_cancellation_does_not_publish() {
    let f = Fixture::new(); let mut s = f.settings();
    let input = f.input("replacement.wav", 96_000, 1, false);
    let original = fs::read(&input).unwrap();
    let t = s.template(TemplateRef::Index(0)).unwrap();
    s.output.mode = OutputMode::Replace { backup_directory: f.0.clone() };
    let prepared = prepare_file(&discovered(&input), &s).unwrap();
    let cancel = Cancellation::new(); cancel.cancel();
    let cancelled = process_file(&prepared, &t, &s, &cancel).unwrap_err();
    assert!(cancelled.cancelled); assert_eq!(fs::read(&input).unwrap(), original);
    let report = process_file(&prepared, &t, &s, &Cancellation::new()).unwrap();
    assert_eq!(fs::read(report.backup_path.as_ref().unwrap()).unwrap(), original);
    let inspected = inspect_file(&report.output_path, &s).unwrap();
    assert_eq!(inspected.marker.as_ref().unwrap().backup_file_name.as_ref().unwrap(),
        &platinum_engine::metadata::MarkerText::from(report.backup_path.unwrap().to_str().unwrap()));
    let analysis = analyze_file(&report.output_path, &s, &Cancellation::new()).unwrap();
    assert!(analysis.original_waveform.is_some());
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn changed_source_after_preflight_is_rejected_before_transaction() {
    let f = Fixture::new(); let mut s = f.settings();
    s.output.mode = OutputMode::Replace { backup_directory: f.0.clone() };
    let input = f.input("changed.wav", 48_000, 1, false);
    let prepared = prepare_file(&discovered(&input), &s).unwrap();
    file_workflow::validate_batch(&[prepared.output_plan()]).unwrap();
    let destination = prepared.output_plan().destination().to_owned();
    f.input("changed.wav", 48_001, 1, false);
    let changed = fs::read(&input).unwrap();
    let mut before: Vec<_> = fs::read_dir(&f.0).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    before.sort();
    let t = s.template(TemplateRef::Index(0)).unwrap();
    let error = process_file(&prepared, &t, &s, &Cancellation::new()).unwrap_err();
    assert_eq!(error.stage, "plan source identity");
    assert!(!error.cancelled && error.retained_backup.is_none() && !error.partial_publication);
    assert_eq!(destination, fs::canonicalize(&input).unwrap());
    assert_eq!(fs::read(&input).unwrap(), changed);
    let mut after: Vec<_> = fs::read_dir(&f.0).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    after.sort();
    assert_eq!(after, before, "a stale plan created a backup, workspace, or reservation");
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn short_and_silent_wav_publish_native_nil_marker_without_fake_measurements() {
    use platinum_engine::tags::{Format, TagBridge};
    let f = Fixture::new(); let s = f.settings();
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    for (name, frames, silent) in [("short.wav", 513, false), ("silent.wav", 48_000, true)] {
        let input = f.input(name, frames, 2, false);
        if silent {
            let mut bytes = fs::read(&input).unwrap();
            bytes[44..].fill(0);
            fs::write(&input, bytes).unwrap();
        }
        let before = fs::read(&input).unwrap();
        let prepared = prepare_file(&discovered(&input), &s).unwrap();
        let t = s.template(TemplateRef::Index(0)).unwrap();
        let report = process_file(&prepared, &t, &s, &Cancellation::new()).unwrap();
        assert_eq!(report.processing_marker_status, "nil_payload_empty_wav_chunk");
        assert_eq!(report.input_volume, f32::NEG_INFINITY);
        assert_eq!(serde_json::to_value(&report).unwrap()["input_volume"], "-Infinity");
        assert_eq!(bridge.read_processing_marker(&report.output_path, Format::Wav).unwrap(), Some(Vec::new()));
        assert_eq!(inspect_file(&report.output_path, &s).unwrap_err().stage, "decode marker");
        assert_eq!(fs::read(&input).unwrap(), before);
    }
}
