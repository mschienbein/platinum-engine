#![cfg(target_os = "macos")]
//! Codec-only probes. The ignored oracle test compares complete files against an
//! independent AudioToolbox call sequence, not against Platinum Notes tags/DSP.

use platinum_engine::apple_codec::{
    CodecError, OutputFormat, Pcm16StereoConverter, Reader, StreamDescription, Writer,
    WriterConfig, recorder_bit_depth,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        for _ in 0..128 {
            let path = std::env::temp_dir().join(format!("pn-apple-codec-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create test directory: {e}"),
            }
        }
        panic!("could not reserve a test directory")
    }
    fn path(&self, name: &str) -> PathBuf { self.0.join(name) }
}
impl Drop for TestDir { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

fn config(format: OutputFormat, channels: u32, bit_depth: u32) -> WriterConfig {
    WriterConfig { format, sample_rate: 44_100.0, channels, bit_depth }
}
fn sample(frame: usize, channel: usize) -> f32 {
    ((((frame % 101) * 17 + (channel % 101) * 29) % 101) as i32 - 50) as f32 / 128.0
}
fn samples(start: usize, frames: usize, channels: usize) -> Vec<f32> {
    (start..start + frames).flat_map(|frame| (0..channels).map(move |channel| sample(frame, channel))).collect()
}
fn write_fixture(path: &Path, config: WriterConfig, frames: usize) {
    let mut writer = Writer::create(path, config).unwrap();
    for start in (0..frames).step_by(512) {
        writer.write(&samples(start, (frames - start).min(512), config.channels as usize)).unwrap();
    }
    writer.finish().unwrap();
}
fn read_all(reader: &mut Reader, frames_per_block: usize) -> Vec<f32> {
    let channels = reader.client_format().channels_per_frame as usize;
    let mut buffer = vec![0.0; frames_per_block * channels];
    let mut result = Vec::new();
    loop {
        let frames = reader.read(&mut buffer).unwrap();
        if frames == 0 { break; }
        result.extend_from_slice(&buffer[..frames * channels]);
    }
    result
}
fn as_bytes_f32(samples: &[f32]) -> Vec<u8> { samples.iter().flat_map(|v| v.to_ne_bytes()).collect() }
fn as_bytes_i16(samples: &[i16]) -> Vec<u8> { samples.iter().flat_map(|v| v.to_ne_bytes()).collect() }

#[test]
fn controller_bit_depth_bucketing_and_validation() {
    for format_id in [u32::from_be_bytes(*b"lpcm"), u32::from_be_bytes(*b"alac"), u32::from_be_bytes(*b"flac")] {
        for (bits, expected) in [(0, 16), (16, 16), (17, 24), (24, 24), (25, 32), (32, 32)] {
            let source = StreamDescription { format_id, bits_per_channel: bits, ..Default::default() };
            assert_eq!(recorder_bit_depth(&source), expected);
        }
    }
    assert_eq!(recorder_bit_depth(&StreamDescription { format_id: u32::from_be_bytes(*b"aac "), bits_per_channel: 24, ..Default::default() }), 0);
    let dir = TestDir::new();
    assert!(matches!(Writer::create(dir.path("bad.wav"), config(OutputFormat::Wav, 0, 16)), Err(CodecError::InvalidInput(_))));
    assert!(matches!(Writer::create(dir.path("bad.wav"), config(OutputFormat::Wav, 2, 20)), Err(CodecError::InvalidInput(_))));
    assert!(matches!(Writer::create(dir.path("bad.wav"), WriterConfig { sample_rate: f64::NAN, ..config(OutputFormat::Wav, 2, 16) }), Err(CodecError::InvalidInput(_))));
    assert!(matches!(Reader::open(dir.path("missing.FLAC"), None), Err(CodecError::Unsupported(_))));
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn output_never_clobbers_existing_or_late_collision() {
    let dir = TestDir::new();
    let existing = dir.path("existing.wav");
    fs::write(&existing, b"original").unwrap();
    assert!(Writer::create(&existing, config(OutputFormat::Wav, 2, 16)).is_err());
    assert_eq!(fs::read(&existing).unwrap(), b"original");
    let symlink = dir.path("symlink.wav");
    std::os::unix::fs::symlink(&existing, &symlink).unwrap();
    assert!(Writer::create(&symlink, config(OutputFormat::Wav, 2, 16)).is_err());
    assert_eq!(fs::read(&existing).unwrap(), b"original");
    let late = dir.path("late.wav");
    let mut writer = Writer::create(&late, config(OutputFormat::Wav, 2, 16)).unwrap();
    writer.write(&samples(0, 3, 2)).unwrap();
    fs::write(&late, b"late collision").unwrap();
    assert!(writer.finish().is_err());
    assert_eq!(fs::read(&late).unwrap(), b"late collision");
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 3);
}

#[test]
fn abort_and_drop_remove_unpublished_workspace() {
    let dir = TestDir::new();
    let target = dir.path("aborted.wav");
    let mut writer = Writer::create(&target, config(OutputFormat::Wav, 2, 16)).unwrap();
    writer.write(&samples(0, 512, 2)).unwrap();
    writer.abort().unwrap();
    assert!(!target.exists());
    {
        let mut writer = Writer::create(&target, config(OutputFormat::Wav, 2, 16)).unwrap();
        writer.write(&samples(0, 8, 2)).unwrap();
    }
    assert!(!target.exists());
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn replaced_workspace_file_is_not_published_or_removed() {
    let dir = TestDir::new();
    let target = dir.path("target.wav");
    let mut writer = Writer::create(&target, config(OutputFormat::Wav, 2, 16)).unwrap();
    writer.write(&samples(0, 8, 2)).unwrap();
    let workspace = fs::read_dir(&dir.0).unwrap().next().unwrap().unwrap().path();
    let staged = workspace.join("target.wav");
    fs::rename(&staged, workspace.join("original-encoder-file")).unwrap();
    fs::write(&staged, b"replacement").unwrap();
    assert!(writer.finish().is_err());
    assert!(!target.exists());
    assert_eq!(fs::read(&staged).unwrap(), b"replacement");
}

#[test]
fn wav_roundtrip_and_invalid_buffer_do_not_advance_stream() {
    let dir = TestDir::new();
    let path = dir.path("audio.wav");
    let mut writer = Writer::create(&path, config(OutputFormat::Wav, 2, 16)).unwrap();
    assert!(matches!(writer.write(&[1.0]), Err(CodecError::InvalidInput(_))));
    let expected = samples(0, 1041, 2);
    for block in expected.chunks(512 * 2) { writer.write(block).unwrap(); }
    writer.finish().unwrap();
    let mut reader = Reader::open(&path, None).unwrap();
    assert_eq!(reader.source_format().format_id, u32::from_be_bytes(*b"lpcm"));
    assert_eq!(reader.client_format().format_flags, 9);
    assert_eq!(reader.client_format().channels_per_frame, 2);
    assert!(matches!(reader.read(&mut [0.0]), Err(CodecError::InvalidInput(_))));
    let actual = read_all(&mut reader, 257);
    assert_eq!(actual.len(), expected.len());
    for (a, b) in actual.iter().zip(&expected) { assert!((a - b).abs() <= 2.0 / 32768.0); }
    reader.close().unwrap();
}

#[test]
fn pcm_converter_checks_whole_frames_and_capacity() {
    let mut converter = Pcm16StereoConverter::new(44_100.0, 2).unwrap();
    assert!(matches!(converter.convert(&[0.0], &mut [0; 2]), Err(CodecError::InvalidInput(_))));
    assert!(matches!(converter.convert(&[0.0, 0.0], &mut [0; 1]), Err(CodecError::InvalidInput(_))));
    let mut output = [7; 4];
    assert_eq!(converter.convert(&[0.0, 0.0, 0.0, 0.0], &mut output).unwrap(), 2);
    assert_eq!(output, [0; 4]);
    converter.close().unwrap();
}

fn run_oracle(oracle: &Path, args: &[&str]) {
    let result = Command::new(oracle).args(args).output().unwrap();
    assert!(result.status.success(), "oracle {:?}: {}", args, String::from_utf8_lossy(&result.stderr));
}
fn assert_bytes_equal(actual: &[u8], reference: &[u8], label: &str) {
    if actual != reference {
        let first = actual.iter().zip(reference).position(|(a, b)| a != b);
        panic!("{label}: complete bytes differ (lengths {} vs {}, first differing overlap byte {:?})", actual.len(), reference.len(), first);
    }
}

#[test]
#[ignore = "requires PN_APPLE_CODEC_ORACLE pointing to the separately compiled native probe; compares codec-only files"]
fn native_reference_full_file_and_decode_comparison() {
    let oracle = PathBuf::from(std::env::var_os("PN_APPLE_CODEC_ORACLE").expect("set PN_APPLE_CODEC_ORACLE"));
    let dir = TestDir::new();
    let rust_dir = dir.path("rust"); let oracle_dir = dir.path("oracle");
    fs::create_dir(&rust_dir).unwrap(); fs::create_dir(&oracle_dir).unwrap();
    let cases = [
        ("wav", "wav", OutputFormat::Wav, 16), ("wav", "wav", OutputFormat::Wav, 24), ("wav", "wav", OutputFormat::Wav, 32),
        ("aiff", "aiff", OutputFormat::Aiff, 16), ("aiff", "aiff", OutputFormat::Aiff, 24), ("aiff", "aiff", OutputFormat::Aiff, 32),
        ("alac", "m4a", OutputFormat::Alac, 16), ("alac", "m4a", OutputFormat::Alac, 24), ("alac", "m4a", OutputFormat::Alac, 32),
        ("aac", "m4a", OutputFormat::Aac, 0),
    ];
    for channels in [1u32, 2] {
        for (name, extension, format, bits) in cases {
            let basename = format!("{name}-{bits}-{channels}.{extension}");
            let rust = rust_dir.join(&basename); let native = oracle_dir.join(&basename);
            write_fixture(&rust, config(format, channels, bits), 4099);
            run_oracle(&oracle, &["encode", name, "44100", &channels.to_string(), &bits.to_string(), "4099", native.to_str().unwrap()]);
            assert_bytes_equal(&fs::read(&rust).unwrap(), &fs::read(&native).unwrap(), &basename);
            let raw = oracle_dir.join(format!("{basename}.f32"));
            run_oracle(&oracle, &["decode", "44100", rust.to_str().unwrap(), raw.to_str().unwrap()]);
            let mut reader = Reader::open(&rust, None).unwrap();
            let decoded = read_all(&mut reader, 512); reader.close().unwrap();
            assert_bytes_equal(&as_bytes_f32(&decoded), &fs::read(raw).unwrap(), &format!("decoded {basename}"));
        }
        let raw = oracle_dir.join(format!("mp3-convert-{channels}.s16"));
        run_oracle(&oracle, &["pcm16", "44100", &channels.to_string(), "4099", raw.to_str().unwrap()]);
        let mut converter = Pcm16StereoConverter::new(44_100.0, channels).unwrap();
        let mut converted = Vec::new();
        for start in (0..4099).step_by(512) {
            let frames = (4099 - start).min(512);
            let mut block = vec![0i16; frames * 2];
            assert_eq!(converter.convert(&samples(start, frames, channels as usize), &mut block).unwrap(), frames);
            converted.extend(block);
        }
        converter.close().unwrap();
        assert_bytes_equal(&as_bytes_i16(&converted), &fs::read(raw).unwrap(), "MP3 PCM conversion");
    }
    let source = rust_dir.join("wav-16-2.wav");
    for rate in [32_000u32, 48_000] {
        let raw = oracle_dir.join(format!("resampled-{rate}.f32"));
        run_oracle(&oracle, &["decode", &rate.to_string(), source.to_str().unwrap(), raw.to_str().unwrap()]);
        let mut reader = Reader::open(&source, Some(rate as f64)).unwrap();
        let decoded = read_all(&mut reader, 512); reader.close().unwrap();
        assert_bytes_equal(&as_bytes_f32(&decoded), &fs::read(raw).unwrap(), &format!("resampled {rate}"));
    }
}

#[test]
#[ignore = "requires PN_APPLE_CODEC_GOLDEN_DIR with pinned codec-only reference files and matching runtime context"]
fn pinned_codec_reference_full_file_comparison() {
    let golden = PathBuf::from(std::env::var_os("PN_APPLE_CODEC_GOLDEN_DIR").expect("set PN_APPLE_CODEC_GOLDEN_DIR"));
    let dir = TestDir::new();
    for channels in [1u32, 2] {
        for (name, extension, format, bits) in [
            ("wav", "wav", OutputFormat::Wav, 16), ("wav", "wav", OutputFormat::Wav, 24), ("wav", "wav", OutputFormat::Wav, 32),
            ("aiff", "aiff", OutputFormat::Aiff, 16), ("aiff", "aiff", OutputFormat::Aiff, 24), ("aiff", "aiff", OutputFormat::Aiff, 32),
            ("alac", "m4a", OutputFormat::Alac, 16), ("alac", "m4a", OutputFormat::Alac, 24), ("alac", "m4a", OutputFormat::Alac, 32),
            ("aac", "m4a", OutputFormat::Aac, 0),
        ] {
            let basename = format!("{name}-{bits}-{channels}.{extension}");
            let output = dir.path(&basename); write_fixture(&output, config(format, channels, bits), 4099);
            let reference = fs::read(golden.join(&basename)).expect("missing pinned codec-only reference");
            assert_bytes_equal(&fs::read(&output).unwrap(), &reference, &basename);
        }
    }
}
