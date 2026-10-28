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

// Unlike the separate-process oracle above, this diagnostic alternates the
// production reader and a direct system reader in one process, using the same
// output allocation. It intentionally retains exact mismatches as failures.
mod aac_context {
    use super::*;
    use std::ffi::c_void;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::ptr;

    #[repr(C)]
    struct Buffer { channels: u32, bytes: u32, data: *mut c_void }
    #[repr(C)]
    struct BufferList { count: u32, buffers: [Buffer; 1] }

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        #[link_name = "ExtAudioFileOpenURL"]
        fn open_url(url: *const c_void, output: *mut *mut c_void) -> i32;
        #[link_name = "ExtAudioFileGetProperty"]
        fn get_property(file: *mut c_void, property: u32, size: *mut u32, output: *mut c_void) -> i32;
        #[link_name = "ExtAudioFileSetProperty"]
        fn set_property(file: *mut c_void, property: u32, size: u32, data: *const c_void) -> i32;
        #[link_name = "ExtAudioFileRead"]
        fn read_frames(file: *mut c_void, frames: *mut u32, list: *mut BufferList) -> i32;
        #[link_name = "ExtAudioFileDispose"]
        fn dispose(file: *mut c_void) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        #[link_name = "CFURLCreateFromFileSystemRepresentation"]
        fn create_url(allocator: *const c_void, bytes: *const u8, length: isize, directory: u8) -> *const c_void;
        #[link_name = "CFRelease"]
        fn release(object: *const c_void);
    }

    const fn fourcc(bytes: [u8; 4]) -> u32 { u32::from_be_bytes(bytes) }
    fn status(operation: &str, result: i32) { assert_eq!(result, 0, "{operation}: OSStatus {result}"); }

    #[cfg(target_arch = "aarch64")]
    fn current_fpcr() -> u64 {
        let value: u64;
        unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) value, options(nostack, preserves_flags)); }
        value
    }
    #[cfg(not(target_arch = "aarch64"))]
    fn current_fpcr() -> u64 { 0 }

    struct Direct {
        file: *mut c_void,
        source: StreamDescription,
        client: StreamDescription,
    }
    impl Direct {
        fn open(path: &Path) -> Self {
            let path_bytes = path.as_os_str().as_bytes();
            let url = unsafe {
                create_url(ptr::null(), path_bytes.as_ptr(), path_bytes.len().try_into().unwrap(), 0)
            };
            assert!(!url.is_null());
            let mut file = ptr::null_mut();
            let result = unsafe { open_url(url, &mut file) };
            unsafe { release(url) };
            if result != 0 {
                if !file.is_null() { unsafe { dispose(file) }; }
                status("direct open", result);
            }
            assert!(!file.is_null());
            let mut direct = Self { file, source: StreamDescription::default(), client: StreamDescription::default() };
            let mut size = std::mem::size_of::<StreamDescription>() as u32;
            status("direct source format", unsafe {
                get_property(direct.file, fourcc(*b"ffmt"), &mut size, (&mut direct.source as *mut StreamDescription).cast())
            });
            assert_eq!(size as usize, std::mem::size_of::<StreamDescription>());
            let source = direct.source;
            let bytes = source.channels_per_frame.checked_mul(4).unwrap();
            direct.client = StreamDescription {
                sample_rate: source.sample_rate, format_id: fourcc(*b"lpcm"), format_flags: 9,
                bytes_per_packet: bytes, frames_per_packet: 1, bytes_per_frame: bytes,
                channels_per_frame: source.channels_per_frame, bits_per_channel: 32, reserved: 0,
            };
            status("direct client format", unsafe {
                set_property(direct.file, fourcc(*b"cfmt"), std::mem::size_of::<StreamDescription>() as u32,
                    (&direct.client as *const StreamDescription).cast())
            });
            direct
        }
        fn length(&self) -> i64 {
            let mut length = 0i64;
            let mut size = std::mem::size_of::<i64>() as u32;
            status("direct length", unsafe {
                get_property(self.file, fourcc(*b"#frm"), &mut size, (&mut length as *mut i64).cast())
            });
            assert_eq!(size as usize, std::mem::size_of::<i64>());
            length
        }
        fn read(&mut self, block: &mut [f32]) -> (usize, Option<u32>) {
            let channels = self.client.channels_per_frame as usize;
            assert_eq!(block.len() % channels, 0);
            let mut frames: u32 = (block.len() / channels).try_into().unwrap();
            let capacity: u32 = (block.len() * std::mem::size_of::<f32>()).try_into().unwrap();
            let data = block.as_mut_ptr().cast();
            let mut list = BufferList { count: 1, buffers: [Buffer { channels: channels as u32, bytes: capacity, data }] };
            status("direct read", unsafe { read_frames(self.file, &mut frames, &mut list) });
            assert!(frames as usize <= block.len() / channels && list.count == 1);
            assert!(list.buffers[0].bytes <= capacity && list.buffers[0].data == data);
            (frames as usize, Some(list.buffers[0].bytes))
        }
        fn close(&mut self) {
            let file = std::mem::replace(&mut self.file, ptr::null_mut());
            if !file.is_null() { status("direct dispose", unsafe { dispose(file) }); }
        }
    }
    impl Drop for Direct {
        fn drop(&mut self) { if !self.file.is_null() { unsafe { dispose(self.file) }; } }
    }

    enum Input { Rust(Reader), Native(Direct) }
    impl Input {
        fn open(arm: char, path: &Path) -> Self {
            match arm {
                'R' => Self::Rust(Reader::open(path, None).unwrap()),
                'N' => Self::Native(Direct::open(path)),
                _ => unreachable!(),
            }
        }
        fn formats(&self) -> (StreamDescription, StreamDescription) {
            match self {
                Self::Rust(r) => (r.source_format(), r.client_format()),
                Self::Native(r) => (r.source, r.client),
            }
        }
        fn length(&self) -> i64 {
            match self {
                Self::Rust(r) => r.file_length_frames().unwrap(),
                Self::Native(r) => r.length(),
            }
        }
        fn read(&mut self, block: &mut [f32]) -> (usize, Option<u32>) {
            match self {
                Self::Rust(r) => (r.read(block).unwrap(), None),
                Self::Native(r) => r.read(block),
            }
        }
        fn close(self) {
            match self {
                Self::Rust(r) => r.close().unwrap(),
                Self::Native(mut r) => r.close(),
            }
        }
    }
    struct Capture {
        arm: char, source: StreamDescription, client: StreamDescription, length: i64,
        open_fpcr: (u64, u64), reads: Vec<(usize, Option<u32>, u64, u64)>, samples: Vec<f32>,
    }
    fn capture(arm: char, path: &Path, block: &mut [f32]) -> Capture {
        let before = current_fpcr();
        let mut input = Input::open(arm, path);
        let after = current_fpcr();
        let (source, client) = input.formats();
        assert_eq!(source.format_id, fourcc(*b"aac "), "fixture must be generated mono AAC");
        assert_eq!(source.channels_per_frame, 1, "fixture must be generated mono AAC");
        let length = input.length();
        let capacity: usize = length.try_into().expect("negative or oversized file frame count");
        let mut samples = Vec::with_capacity(capacity);
        let mut reads = Vec::new();
        loop {
            block.fill(0.0);
            let before = current_fpcr();
            let (frames, bytes) = input.read(block);
            let after = current_fpcr();
            reads.push((frames, bytes, before, after));
            if frames == 0 { break; }
            samples.extend_from_slice(&block[..frames]);
        }
        input.close();
        Capture { arm, source, client, length, open_fpcr: (before, after), reads, samples }
    }

    #[test]
    #[ignore = "diagnostic requiring PN_AAC_DIAG_SOURCE; retains any exact mismatches"]
    fn aac_interleaved_same_process_rust_and_native_contexts() {
        let root = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("local-artifacts/apple-codecs")).unwrap();
        let path = fs::canonicalize(std::env::var_os("PN_AAC_DIAG_SOURCE").expect("set PN_AAC_DIAG_SOURCE")).unwrap();
        assert!(path.starts_with(&root) && path.is_file(), "fixture must be generated under local-artifacts/apple-codecs");
        let sequence = std::env::var("PN_AAC_DIAG_SEQUENCE").unwrap_or_else(|_| "RRNNNRRN".to_owned());
        assert!(!sequence.is_empty() && sequence.len() <= 32 && sequence.contains('R') && sequence.contains('N')
            && sequence.bytes().all(|b| b == b'R' || b == b'N'), "sequence must contain both R and N, maximum 32");
        let frames: usize = std::env::var("PN_AAC_DIAG_BLOCK").unwrap_or_else(|_| "512".to_owned()).parse().unwrap();
        assert!((1..=65_535).contains(&frames));
        let output = std::env::var_os("PN_AAC_DIAG_OUTPUT").map(|value| {
            let path = PathBuf::from(value);
            let path = if path.is_absolute() { path } else { std::env::current_dir().unwrap().join(path) };
            assert_eq!(fs::canonicalize(path.parent().unwrap()).unwrap(), root, "output must be a new child of the fixture root");
            fs::DirBuilder::new().mode(0o700).create(&path).expect("create exclusive output directory");
            path
        });
        assert_eq!(std::mem::size_of::<StreamDescription>(), 40);
        assert_eq!(std::mem::size_of::<Buffer>(), 16);
        assert_eq!(std::mem::offset_of!(Buffer, data), 8);
        assert_eq!(std::mem::offset_of!(BufferList, buffers), 8);
        assert_eq!(std::mem::size_of::<BufferList>(), 24);
        println!("fixture_sha256={} sequence={sequence} block_frames={frames}",
            platinum_engine::mp3::file_sha256(&path).unwrap());
        let mut block = vec![0.0f32; frames];
        println!("shared_output_mod64={}", block.as_ptr() as usize % 64);
        let mut captures = Vec::new();
        for (index, arm) in sequence.chars().enumerate() {
            let result = capture(arm, &path, &mut block);
            let hash = output.as_ref().map(|dir| {
                let target = dir.join(format!("{index}-{arm}.f32"));
                let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&target).unwrap();
                file.write_all(&as_bytes_f32(&result.samples)).unwrap();
                drop(file);
                platinum_engine::mp3::file_sha256(target).unwrap()
            });
            println!("trial={index} arm={arm} sha256={hash:?} frames={} source={:?} client={:?} source_length={} open_fpcr={:?} reads={:?}",
                result.samples.len(), result.source, result.client, result.length, result.open_fpcr, result.reads);
            captures.push(result);
        }
        let baseline = &captures[0];
        let mut all_equal = true;
        for (index, result) in captures.iter().enumerate().skip(1) {
            let first = baseline.samples.iter().zip(&result.samples).position(|(a, b)| a.to_bits() != b.to_bits());
            let differing_words = baseline.samples.iter().zip(&result.samples).filter(|(a, b)| a.to_bits() != b.to_bits()).count();
            let same_schedule = baseline.reads.iter().map(|read| read.0).eq(result.reads.iter().map(|read| read.0));
            let equal = baseline.source == result.source && baseline.client == result.client && baseline.length == result.length
                && same_schedule && baseline.samples.len() == result.samples.len() && differing_words == 0;
            println!("comparison=0/{index} arms={}/{} equal={equal} same_schedule={same_schedule} first_word={first:?} differing_overlap_words={differing_words}",
                baseline.arm, result.arm);
            all_equal &= equal;
        }
        assert!(all_equal, "same-process AAC captures differ; retain exact raw outputs and inspect the trial log");
    }
}
