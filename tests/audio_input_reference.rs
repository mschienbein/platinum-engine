#![cfg(target_os = "macos")]
//! Generated fixtures and an opt-in comparison to the original MIKAudio
//! reader. The oracle is a separate, digest-gated reference diagnostic; no
//! application library is used by AudioReader or the fixture encoder.

use platinum_engine::audio_input::{AudioInputError, AudioReader, InputLibraries};
use std::path::PathBuf;

#[test]
fn suffix_dispatch_requires_an_explicit_library_and_pin() {
    for path in ["missing.flac", "missing.FLAC", "anythingfLaC"] {
        assert!(matches!(AudioReader::open(path, None, &InputLibraries::default()), Err(AudioInputError::MissingFlacLibrary)));
        let libraries = InputLibraries { flac: Some(PathBuf::from("not-loaded.dylib")), flac_sha256: None };
        assert!(matches!(AudioReader::open(path, None, &libraries), Err(AudioInputError::MissingFlacDigest)));
        let libraries = InputLibraries { flac: Some(PathBuf::from("not-loaded.dylib")), flac_sha256: Some("not-a-hash".into()) };
        assert!(matches!(AudioReader::open(path, None, &libraries), Err(AudioInputError::InvalidInput(_))));
    }
}

mod oracle {
    use super::*;
    use libloading::Library;
    use serde_json::Value;
    use std::ffi::{c_char, c_int, c_void};
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::os::fd::IntoRawFd;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let root = std::env::current_dir().unwrap().join("local-artifacts/flac-implementation");
            for _ in 0..128 {
                let path = root.join(format!("input-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("create generated-fixture directory: {e}"),
                }
            }
            panic!("could not allocate test directory")
        }
        fn path(&self, name: &str) -> PathBuf { self.0.join(name) }
    }
    impl Drop for TestDir { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    #[link(name = "System")]
    unsafe extern "C" {
        fn fdopen(fd: c_int, mode: *const c_char) -> *mut c_void;
        fn close(fd: c_int) -> c_int;
    }
    type New = unsafe extern "C" fn() -> *mut c_void;
    type Delete = unsafe extern "C" fn(*mut c_void);
    type Set = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
    type InitFile = unsafe extern "C" fn(*mut c_void, *mut c_void, Option<unsafe extern "C" fn(*const c_void, u64, u64, u32, u32, *mut c_void)>, *mut c_void) -> c_int;
    type Process = unsafe extern "C" fn(*mut c_void, *const i32, u32) -> c_int;
    type Finish = unsafe extern "C" fn(*mut c_void) -> c_int;

    // The values are integer PCM independent of the engine's float quantizer.
    fn integer_sample(frame: usize, channel: usize, bits: u32) -> i32 {
        let magnitude = 1i64 << (bits - 1);
        let candidates = [-magnitude, magnitude - 1, 0, -1, 1, -(magnitude / 2), magnitude / 2,
            -(magnitude / 4), magnitude / 4, 7];
        candidates[(frame * 7 + channel * 3) % candidates.len()] as i32
    }
    fn write_pcm16_wav(path: &Path, rate: u32, channels: u16, frames: usize) {
        let bytes = u32::try_from(frames * channels as usize * 2).unwrap();
        let mut file = OpenOptions::new().write(true).create_new(true).open(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + bytes).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&channels.to_le_bytes()).unwrap();
        file.write_all(&rate.to_le_bytes()).unwrap();
        file.write_all(&(rate * u32::from(channels) * 2).to_le_bytes()).unwrap();
        file.write_all(&(channels * 2).to_le_bytes()).unwrap();
        file.write_all(&16u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&bytes.to_le_bytes()).unwrap();
        for frame in 0..frames {
            for channel in 0..channels as usize {
                file.write_all(&(integer_sample(frame, channel, 16) as i16).to_le_bytes()).unwrap();
            }
        }
    }
    fn encode_fixture(library: &Path, path: &Path, rate: u32, channels: u32, bits: u32, frames: usize) {
        let file: File = OpenOptions::new().read(true).write(true).create_new(true).open(path).unwrap();
        let samples: Vec<i32> = (0..frames).flat_map(|f| (0..channels).map(move |c| integer_sample(f, c as usize, bits))).collect();
        // SAFETY: library is the caller-selected public libFLAC, FILE* comes from
        // a new exclusive fixture file, and symbols have the 1.4.2 public ABI.
        unsafe {
            let lib = Library::new(library).unwrap();
            let new: New = *lib.get(b"FLAC__stream_encoder_new\0").unwrap();
            let delete: Delete = *lib.get(b"FLAC__stream_encoder_delete\0").unwrap();
            let set_channels: Set = *lib.get(b"FLAC__stream_encoder_set_channels\0").unwrap();
            let set_rate: Set = *lib.get(b"FLAC__stream_encoder_set_sample_rate\0").unwrap();
            let set_bits: Set = *lib.get(b"FLAC__stream_encoder_set_bits_per_sample\0").unwrap();
            let init: InitFile = *lib.get(b"FLAC__stream_encoder_init_FILE\0").unwrap();
            let process: Process = *lib.get(b"FLAC__stream_encoder_process_interleaved\0").unwrap();
            let finish: Finish = *lib.get(b"FLAC__stream_encoder_finish\0").unwrap();
            let encoder = new(); assert!(!encoder.is_null());
            assert_eq!(set_channels(encoder, channels), 1);
            assert_eq!(set_rate(encoder, rate), 1);
            assert_eq!(set_bits(encoder, bits), 1);
            let fd = file.into_raw_fd();
            let stream = fdopen(fd, b"w+b\0".as_ptr().cast());
            if stream.is_null() { close(fd); panic!("fdopen fixture failed"); }
            assert_eq!(init(encoder, stream, None, std::ptr::null_mut()), 0);
            if frames > 0 { assert_eq!(process(encoder, samples.as_ptr(), frames as u32), 1); }
            assert_eq!(finish(encoder), 1); // finish closes FILE*
            delete(encoder);
        }
    }
    fn env_path(name: &str) -> PathBuf { std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")).into() }
    fn run_original(oracle: &Path, image: &Path, source: &Path, output: &Path, block: usize, rate: Option<u32>, planar: bool) -> Value {
        let request_rate = rate.map_or_else(|| "native".to_owned(), |v| v.to_string());
        let mut command = Command::new(oracle);
        command.arg(image).arg(source).arg(output).arg(block.to_string()).arg(request_rate);
        if planar { command.arg("planar-stereo"); }
        let result = command.output().unwrap();
        assert!(result.status.success(), "original oracle failed: {}", String::from_utf8_lossy(&result.stderr));
        let stdout = String::from_utf8(result.stdout).unwrap();
        serde_json::from_str(stdout.lines().last().expect("missing oracle JSON")).expect("invalid oracle JSON")
    }
    fn as_bytes(samples: &[f32]) -> Vec<u8> { samples.iter().flat_map(|sample| sample.to_ne_bytes()).collect() }

    #[test]
    #[ignore = "requires PN_FLAC_LIBRARY, PN_FLAC_LIBRARY_SHA256, PN_FLAC_INPUT_ORACLE and PN_MIKAUDIO_REFERENCE"]
    fn generated_flac_matches_original_mikaudio_across_depths_channels_and_partitions() {
        let library = env_path("PN_FLAC_LIBRARY");
        let digest = std::env::var("PN_FLAC_LIBRARY_SHA256").expect("set PN_FLAC_LIBRARY_SHA256");
        let oracle = env_path("PN_FLAC_INPUT_ORACLE");
        let image = env_path("PN_MIKAUDIO_REFERENCE");
        let libraries = InputLibraries { flac: Some(library.clone()), flac_sha256: Some(digest) };
        let dir = TestDir::new();
        // Frame counts straddle libFLAC's normal 4096-frame block, include an
        // empty stream and short tails; rates exercise the original no-op setter.
        let cases = [
            (8, 1, 1, 48_000), (16, 1, 15, 22_050), (16, 2, 4095, 44_100),
            (16, 2, 4096, 48_000), (24, 2, 4097, 44_100), (24, 1, 0, 48_000),
            (24, 2, 8193, 48_000), (32, 1, 4099, 22_050), (32, 2, 4097, 44_100),
            (16, 3, 137, 48_000), (24, 6, 4097, 44_100),
            (12, 2, 129, 44_100), (20, 2, 129, 44_100),
        ];
        for (case, &(bits, channels, frames, source_rate)) in cases.iter().enumerate() {
            let source = dir.path(&format!("fixture-{case}.flac"));
            encode_fixture(&library, &source, source_rate, channels, bits, frames);
            for (partition, block) in [1usize, 257, 4096, 8193].into_iter().enumerate() {
                for rate in [None, Some(32_000u32)] {
                    let raw = dir.path(&format!("original-{case}-{partition}-{}.f32", rate.unwrap_or(0)));
                    let original = run_original(&oracle, &image, &source, &raw, block, rate, false);
                    let result = AudioReader::open(&source, rate.map(f64::from), &libraries);
                    let original_status = original["open_status"].as_i64().unwrap();
                    let mut reader = match (original_status, result) {
                        (0, Ok(reader)) => reader,
                        (0, Err(error)) => panic!("fixture {case}: original opened but Rust failed: {error}"),
                        (status, Err(AudioInputError::Converter { status: actual, .. })) => {
                            assert_eq!(i64::from(actual), status, "fixture {case} original converter status");
                            continue;
                        }
                        (status, Err(error)) => panic!("fixture {case}: original open status {status}, Rust failed differently: {error}"),
                        (status, Ok(_)) => panic!("fixture {case}: original open status {status}, Rust unexpectedly opened"),
                    };
                    let src = reader.source_format();
                    let client = reader.client_format();
                    assert_eq!(src.sample_rate, original["source_rate"].as_f64().unwrap());
                    for (actual, name) in [(src.format_id, "format_id"), (src.format_flags, "format_flags"),
                        (src.bytes_per_packet, "bytes_per_packet"), (src.frames_per_packet, "frames_per_packet"),
                        (src.bytes_per_frame, "bytes_per_frame"), (src.channels_per_frame, "channels"),
                        (src.bits_per_channel, "bits"), (src.reserved, "reserved")] {
                        assert_eq!(u64::from(actual), original[name].as_u64().unwrap(), "fixture {case}: {name}");
                    }
                    assert_eq!(reader.file_length_frames().unwrap(), original["source_frames"].as_i64().unwrap());
                    assert_eq!(client.sample_rate, f64::from(source_rate));
                    assert_eq!(client.channels_per_frame, channels);
                    assert_eq!(client.format_flags, 9);
                    assert!(matches!(reader.bitrate(), Err(AudioInputError::Unsupported(_))));
                    assert_eq!(reader.read(&mut []).unwrap(), 0);
                    if channels > 1 { assert!(matches!(reader.read(&mut [0.0]), Err(AudioInputError::InvalidInput(_)))); }
                    let mut values = Vec::new();
                    let mut buffer = vec![0.0f32; block * channels as usize];
                    let mut counts = 0usize;
                    loop {
                        buffer.fill(0.0);
                        let got = reader.read(&mut buffer).unwrap();
                        if got == 0 { break; }
                        values.extend_from_slice(&buffer[..got * channels as usize]);
                        counts += got;
                    }
                    assert_eq!(counts as u64, original["read_frames"].as_u64().unwrap());
                    assert_eq!(counts, frames);
                    let last_count = original["last_count"].as_u64().unwrap();
                    assert_eq!(original["last_bytes"].as_u64().unwrap(), last_count * u64::from(channels.min(2)) * 4);
                    let expected = fs::read(&raw).unwrap();
                    let actual = as_bytes(&values);
                    assert_eq!(actual, expected, "fixture {case}, block {block}, requested rate {rate:?}; first differing byte {:?}",
                        actual.iter().zip(&expected).position(|(a,b)| a != b));
                    // For byte-aligned sources the conventional signed scale is
                    // also independently checked; non-byte depths go through
                    // the actual native converter with floor(bits/8) input.
                    if channels <= 2 && bits % 8 == 0 {
                        for f in 0..frames {
                            for c in 0..channels as usize {
                                let expected = (f64::from(integer_sample(f, c, bits)) / ((1u64 << (bits - 1)) as f64)) as f32;
                                assert_eq!(values[f * channels as usize + c].to_bits(), expected.to_bits(), "fixture {case} frame {f} channel {c}");
                            }
                        }
                    }
                    reader.close().unwrap();
                }
            }
        }
    }
    #[test]
    #[ignore = "requires PN_FLAC_LIBRARY, PN_FLAC_LIBRARY_SHA256, PN_FLAC_INPUT_ORACLE and PN_MIKAUDIO_REFERENCE"]
    fn generated_flac_planar_stereo_matches_original_mono_stereo_and_multichannel() {
        let library = env_path("PN_FLAC_LIBRARY");
        let digest = std::env::var("PN_FLAC_LIBRARY_SHA256").expect("set PN_FLAC_LIBRARY_SHA256");
        let oracle = env_path("PN_FLAC_INPUT_ORACLE");
        let image = env_path("PN_MIKAUDIO_REFERENCE");
        let libraries = InputLibraries { flac: Some(library.clone()), flac_sha256: Some(digest) };
        let dir = TestDir::new();
        let cases = [
            (8, 1, 1, 48_000), (16, 1, 15, 22_050), (24, 1, 0, 48_000),
            (32, 1, 4099, 22_050), (16, 2, 4095, 44_100),
            (24, 2, 8193, 48_000), (32, 2, 4097, 44_100),
            (16, 3, 137, 48_000), (24, 6, 4097, 44_100),
            (12, 2, 129, 44_100), (20, 2, 129, 44_100),
        ];
        for (case, &(bits, channels, frames, source_rate)) in cases.iter().enumerate() {
            let source = dir.path(&format!("planar-fixture-{case}.flac"));
            encode_fixture(&library, &source, source_rate, channels, bits, frames);
            for (partition, block) in [1usize, 257, 4096, 8193].into_iter().enumerate() {
                for rate in [source_rate, 32_000u32] {
                    let raw = dir.path(&format!("planar-original-{case}-{partition}-{rate}.f32"));
                    let original = run_original(&oracle, &image, &source, &raw, block, Some(rate), true);
                    let original_status = original["open_status"].as_i64().unwrap();
                    let result = AudioReader::open(&source, Some(f64::from(rate)), &libraries);
                    let mut reader = match (original_status, result) {
                        (0, Ok(reader)) => reader,
                        (0, Err(error)) => panic!("planar fixture {case}: original opened but Rust failed: {error}"),
                        (status, Err(AudioInputError::Converter { status: actual, .. })) => {
                            assert_eq!(i64::from(actual), status, "planar fixture {case} original converter status");
                            continue;
                        }
                        (status, Err(error)) => panic!("planar fixture {case}: original open status {status}, Rust failed differently: {error}"),
                        (status, Ok(_)) => panic!("planar fixture {case}: original open status {status}, Rust unexpectedly opened"),
                    };
                    let source_format = reader.source_format();
                    let source_frames = reader.file_length_frames().unwrap();
                    reader.set_stereo_planar(f64::from(rate)).unwrap();
                    assert_eq!(reader.source_format(), source_format);
                    assert_eq!(reader.file_length_frames().unwrap(), source_frames);
                    assert_eq!(source_frames, original["source_frames"].as_i64().unwrap());
                    let client = reader.client_format();
                    assert_eq!(client.sample_rate, f64::from(source_rate), "FLAC setter must ignore requested rate");
                    assert_eq!(client.format_id, u32::from_be_bytes(*b"lpcm"));
                    assert_eq!(client.format_flags, 0x29);
                    assert_eq!(client.channels_per_frame, 2);
                    assert_eq!(client.bytes_per_frame, 4);
                    assert_eq!(client.bytes_per_packet, 4);
                    assert_eq!(client.bits_per_channel, 32);
                    assert_eq!(reader.read_stereo_planar(&mut [], &mut []).unwrap(), 0);
                    assert!(matches!(reader.read_stereo_planar(&mut [0.0], &mut []), Err(AudioInputError::InvalidInput(_))));
                    let mut left = vec![13.0f32; block];
                    let mut right = vec![-17.0f32; block];
                    let mut result = Vec::new();
                    let mut total = 0usize;
                    loop {
                        left.fill(13.0);
                        right.fill(-17.0);
                        let got = reader.read_stereo_planar(&mut left, &mut right).unwrap();
                        assert!(left[got..].iter().all(|&v| v == 13.0), "left short-read tail changed");
                        assert!(right[got..].iter().all(|&v| v == -17.0), "right short-read tail changed");
                        if got == 0 { break; }
                        if channels == 1 {
                            assert!(right[..got].iter().all(|v| v.to_bits() == 0), "mono's native second plane must be zero");
                        }
                        result.extend_from_slice(&left[..got]);
                        result.extend_from_slice(&right[..got]);
                        total += got;
                    }
                    assert!(original["tail_preserved"].as_bool().unwrap());
                    assert_eq!(original["requested_channels"].as_u64().unwrap(), 2);
                    assert_eq!(total as u64, original["read_frames"].as_u64().unwrap());
                    assert_eq!(total, frames);
                    let last_count = original["last_count"].as_u64().unwrap();
                    assert_eq!(original["last_bytes"].as_u64().unwrap(), last_count * 4);
                    assert_eq!(original["last_right_bytes"].as_u64().unwrap(), last_count * 4);
                    let expected = fs::read(&raw).unwrap();
                    let actual = as_bytes(&result);
                    assert_eq!(actual, expected, "planar fixture {case}, block {block}, rate {rate}; first differing byte {:?}",
                        actual.iter().zip(&expected).position(|(a,b)| a != b));
                    reader.close().unwrap();
                }
            }
        }
    }
    #[test]
    #[ignore = "requires PN_FLAC_INPUT_ORACLE and PN_MIKAUDIO_REFERENCE"]
    fn generated_apple_wav_planar_stereo_matches_original_with_rate_and_channel_conversion() {
        let oracle = env_path("PN_FLAC_INPUT_ORACLE");
        let image = env_path("PN_MIKAUDIO_REFERENCE");
        let dir = TestDir::new();
        let cases = [
            (1u16, 22_050u32, 44_100u32), (1, 44_100, 48_000),
            (2, 44_100, 32_000), (2, 48_000, 48_000),
        ];
        for (case, &(channels, source_rate, requested_rate)) in cases.iter().enumerate() {
            let source = dir.path(&format!("planar-apple-{case}.wav"));
            write_pcm16_wav(&source, source_rate, channels, 4097);
            for block in [1usize, 257, 4096] {
                let raw = dir.path(&format!("planar-apple-original-{case}-{block}.f32"));
                let original = run_original(&oracle, &image, &source, &raw, block, Some(requested_rate), true);
                assert_eq!(original["open_status"].as_i64().unwrap(), 0);
                let mut reader = AudioReader::open(&source, None, &InputLibraries::default()).unwrap();
                let source_format = reader.source_format();
                assert_eq!(source_format.sample_rate, f64::from(source_rate));
                assert_eq!(source_format.channels_per_frame, u32::from(channels));
                let length = reader.file_length_frames().unwrap();
                reader.set_stereo_planar(f64::from(requested_rate)).unwrap();
                assert_eq!(reader.source_format(), source_format);
                assert_eq!(reader.file_length_frames().unwrap(), length);
                assert_eq!(length, original["source_frames"].as_i64().unwrap());
                let client = reader.client_format();
                assert_eq!(client.sample_rate, f64::from(requested_rate));
                assert_eq!(client.format_flags, 0x29);
                assert_eq!(client.channels_per_frame, 2);
                let mut left = vec![13.0f32; block];
                let mut right = vec![-17.0f32; block];
                let mut result = Vec::new();
                let mut total = 0usize;
                loop {
                    left.fill(13.0);
                    right.fill(-17.0);
                    let got = reader.read_stereo_planar(&mut left, &mut right).unwrap();
                    if original["tail_preserved"].as_bool().unwrap() {
                        assert!(left[got..].iter().all(|&v| v == 13.0));
                        assert!(right[got..].iter().all(|&v| v == -17.0));
                    }
                    if got == 0 { break; }
                    result.extend_from_slice(&left[..got]);
                    result.extend_from_slice(&right[..got]);
                    total += got;
                }
                assert_eq!(total as u64, original["read_frames"].as_u64().unwrap());
                let actual = as_bytes(&result);
                let expected = fs::read(&raw).unwrap();
                assert_eq!(actual, expected, "Apple fixture {case}, block {block}; first differing byte {:?}",
                    actual.iter().zip(&expected).position(|(a,b)| a != b));
                reader.close().unwrap();
            }
        }
    }
}
