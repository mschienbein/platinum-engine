//! Generated end-to-end encoder fixtures. These tests require explicitly
//! supplied, trusted public codec libraries; no proprietary runtime is loaded.
#![cfg(target_os = "macos")]

use platinum_engine::engine::{inspect_file, prepare_file, process_file};
use platinum_engine::file_workflow::{self, AudioFormat, Cancellation, OutputMode};
use platinum_engine::settings::{Settings, TemplateRef};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture { dir: PathBuf, input: PathBuf }
impl Fixture {
    fn new() -> Self {
        let dir = loop {
            let p = std::env::temp_dir().join(format!("platinum-codec-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&p) { Ok(()) => break p, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create fixture: {e}") }
        };
        let frames = 96_000u32;
        let mut bytes = Vec::with_capacity(44 + frames as usize * 4);
        bytes.extend_from_slice(b"RIFF"); bytes.extend_from_slice(&(36 + frames * 4).to_le_bytes()); bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes()); bytes.extend_from_slice(&1u16.to_le_bytes()); bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes()); bytes.extend_from_slice(&192_000u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes()); bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data"); bytes.extend_from_slice(&(frames * 4).to_le_bytes());
        for i in 0..frames {
            let v = ((i as f64 * (std::f64::consts::TAU * 440.0 / 48_000.0)).sin() * 16_000.0) as i16;
            bytes.extend_from_slice(&v.to_le_bytes()); bytes.extend_from_slice(&(v / 2).to_le_bytes());
        }
        let input = dir.join("codec-source.wav"); fs::write(&input, bytes).unwrap();
        Self { dir, input }
    }
    fn settings(&self, format: AudioFormat) -> Settings {
        let mut s = Settings::defaults(&self.dir, 1);
        s.output.mode = OutputMode::SpecificFolder { directory: self.dir.clone(), mirror: false };
        s.output.format = format;
        s.libraries.tag_bridge = Some(PathBuf::from(std::env::var_os("PN_TAG_BRIDGE").expect("trusted ABI-5 tag bridge")));
        s
    }
    fn run(&self, s: &Settings, expected: &str) {
        let before = fs::read(&self.input).unwrap();
        let item = file_workflow::discover_inputs(&[self.input.clone()]).unwrap().remove(0);
        let mut template = s.template(TemplateRef::Index(0)).unwrap();
        template.clip.enabled = false; template.pitch.enabled = false; template.exciter_option = -1;
        template.expansion.enabled = false; template.compression.enabled = false;
        let prepared = prepare_file(&item, s).unwrap();
        let result = process_file(&prepared, &template, s, &Cancellation::new()).unwrap();
        assert_eq!(result.output_format, expected); assert!(result.output_frames > 0);
        assert_eq!(result.processing_marker_status, "present");
        assert!(result.applied_stages.iter().any(|s| s == "limiter"));
        let output = inspect_file(&result.output_path, s).unwrap();
        assert_eq!(output.input_format, expected); assert!(output.marker.unwrap().is_processed);
        assert_eq!(fs::read(&self.input).unwrap(), before);
    }
    fn run_silent(&self, settings: &Settings) {
        let mut bytes = fs::read(&self.input).unwrap();
        bytes[44..].fill(0);
        fs::write(&self.input, &bytes).unwrap();
        let item = file_workflow::discover_inputs(&[self.input.clone()]).unwrap().remove(0);
        let prepared = prepare_file(&item, settings).unwrap();
        let template = settings.template(TemplateRef::Index(0)).unwrap();
        let report = process_file(&prepared, &template, settings, &Cancellation::new()).unwrap();
        assert_eq!(report.processing_marker_status, "nil_payload_absent");
        assert_eq!(report.input_volume, f32::NEG_INFINITY);
        assert_eq!(serde_json::to_value(&report).unwrap()["input_volume"], "-Infinity");
        let output = inspect_file(&report.output_path, settings).unwrap();
        assert!(output.marker.is_none());
        assert_eq!(fs::read(&self.input).unwrap(), bytes);
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.dir); } }

#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_FLAC_LIB and PN_FLAC_SHA256"]
fn flac_encoder_and_decoder_preserve_actual_marker() {
    let f = Fixture::new(); let mut settings = f.settings(AudioFormat::Flac);
    settings.libraries.flac = Some(PathBuf::from(std::env::var_os("PN_FLAC_LIB").expect("trusted public FLAC 1.4.2")));
    settings.libraries.flac_sha256 = Some(std::env::var("PN_FLAC_SHA256").expect("FLAC SHA-256 pin"));
    f.run(&settings, "flac/flac");
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_LAME_LIB and PN_LAME_SHA256"]
fn mp3_encoder_and_pcm_converter_preserve_actual_marker() {
    let f = Fixture::new(); let mut settings = f.settings(AudioFormat::Mp3);
    settings.libraries.lame = Some(PathBuf::from(std::env::var_os("PN_LAME_LIB").expect("trusted public LAME 3.100")));
    settings.libraries.lame_sha256 = Some(std::env::var("PN_LAME_SHA256").expect("LAME SHA-256 pin"));
    f.run(&settings, "mp3/mp3");
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_FLAC_LIB and PN_FLAC_SHA256"]
fn silent_flac_reports_nil_payload_and_has_no_marker() {
    let f = Fixture::new(); let mut settings = f.settings(AudioFormat::Flac);
    settings.libraries.flac = Some(PathBuf::from(std::env::var_os("PN_FLAC_LIB").expect("trusted public FLAC 1.4.2")));
    settings.libraries.flac_sha256 = Some(std::env::var("PN_FLAC_SHA256").expect("FLAC SHA-256 pin"));
    f.run_silent(&settings);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_LAME_LIB and PN_LAME_SHA256"]
fn silent_mp3_reports_nil_payload_and_has_no_marker() {
    let f = Fixture::new(); let mut settings = f.settings(AudioFormat::Mp3);
    settings.libraries.lame = Some(PathBuf::from(std::env::var_os("PN_LAME_LIB").expect("trusted public LAME 3.100")));
    settings.libraries.lame_sha256 = Some(std::env::var("PN_LAME_SHA256").expect("LAME SHA-256 pin"));
    f.run_silent(&settings);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn silent_aac_reports_nil_payload_and_has_no_marker() {
    let f = Fixture::new();
    let settings = f.settings(AudioFormat::Aac);
    f.run_silent(&settings);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn silent_aiff_reports_nil_payload_and_has_no_marker() {
    let f = Fixture::new();
    let settings = f.settings(AudioFormat::Aiff);
    f.run_silent(&settings);
}
