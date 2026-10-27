//! MP3 parser vectors and explicit opt-in native-comparison probes.
//! The ignored probe requires an explicitly pinned LAME 3.100 and the pinned
//! PN LAME oracle. They may be the same file to isolate recorder behavior;
//! neither library is obtained or built by this test.
#![cfg(target_os = "macos")]

use platinum_engine::mp3::{inspect_mp3_header_bytes, output_sample_rate, Mp3Header, SourceHeader};

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= u16::from(b);
        for _ in 0..8 { crc = if crc & 1 != 0 { (crc >> 1) ^ 0xa001 } else { crc >> 1 }; }
    }
    crc
}

// Four synthetic MPEG-1 Layer-III/128kbps/44100Hz frames; frame 1 has the
// MIKAudio-specific fixed-offset Xing and LAME fields. These are derived
// vectors, not a recording made by the application.
fn header_fixture(with_tag: bool, with_id3: bool) -> Vec<u8> {
    let prefix = if with_id3 { 15 } else { 0 };
    let mut data = vec![0u8; prefix + 417 * 4];
    if with_id3 { data[..15].copy_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x05abcde"); }
    for frame in 0..4 {
        let mode = if frame == 0 { 0u32 } else { 1u32 };
        let word = 0xfffb_9000u32 | (mode << 6);
        let pos = prefix + frame * 417;
        data[pos..pos + 4].copy_from_slice(&word.to_be_bytes());
    }
    let xing = prefix + 36;
    data[xing..xing + 4].copy_from_slice(b"Xing");
    let tag = xing + 120;
    if with_tag {
        data[tag..tag + 9].copy_from_slice(b"LAME3.100");
        let packed = (576u32 << 12) | 1000;
        data[tag + 21] = (packed >> 16) as u8;
        data[tag + 22] = (packed >> 8) as u8;
        data[tag + 23] = packed as u8;
        let crc = crc16(&data[prefix..tag + 34]);
        data[tag + 34..tag + 36].copy_from_slice(&crc.to_be_bytes());
    }
    data
}

#[test]
fn native_source_header_fields_and_id3_offset() {
    for id3 in [false, true] {
        let data = header_fixture(true, id3);
        let info = inspect_mp3_header_bytes(&data).unwrap();
        assert!(info.valid_layer3 && info.xing && !info.info);
        assert_eq!(info.lame_identifier, Some(*b"LAME3.100"));
        assert_eq!((info.encoder_delay, info.encoder_padding, info.start_skip_samples), (576, 1000, 1105));
        assert!(info.stereo_mode_change);
        assert_eq!(info.stereo_mode, 1);
        assert!(info.correct_crc);
        assert!(SourceHeader::Mp3(info).write_vbr_tag());
    }
}

#[test]
fn native_header_fallback_and_output_rate_policy() {
    let data = header_fixture(false, false);
    let info = inspect_mp3_header_bytes(&data).unwrap();
    assert!(info.xing && info.lame_identifier.is_none());
    assert_eq!((info.encoder_delay, info.encoder_padding, info.start_skip_samples), (576, 0, 1105));
    assert!(!info.correct_crc);
    assert!(!SourceHeader::Mp3(Mp3Header::default()).write_vbr_tag());
    assert!(SourceHeader::Other(Mp3Header::default()).write_vbr_tag());
    for rate in [22_050u32, 32_000, 44_100, 48_000] { assert_eq!(output_sample_rate(rate as f64), rate); }
    assert_eq!(output_sample_rate(96_000.0), 44_100);
}

mod oracle {
    use super::*;
    use libloading::Library;
    use platinum_engine::apple_codec::Pcm16StereoConverter;
    use platinum_engine::mp3::{file_sha256, LameLibrary, Mp3Config, Mp3Encoder, Mp3Mode};
    use std::ffi::{c_char, c_int, c_void, CStr};
    use std::fs::{self, OpenOptions};
    use std::os::fd::IntoRawFd;
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::time::{SystemTime, UNIX_EPOCH};

    const REF_UNIVERSAL: &str = "00ac736edb02e3487d2df4cf572a825309054e7a08c9f08e4a147ab83b83f155";
    const REF_ARM64: &str = "2f7321fbaa80d3486f95215511f25b7994da1bb54725af2edb37b1d520a8d5f9";
    type Init = unsafe extern "C" fn() -> *mut c_void;
    type Set = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    type One = unsafe extern "C" fn(*mut c_void) -> c_int;
    type Encode = unsafe extern "C" fn(*mut c_void, *mut i16, c_int, *mut u8, c_int) -> c_int;
    type Flush = unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int;
    type Tags = unsafe extern "C" fn(*mut c_void, *mut c_void);
    type Version = unsafe extern "C" fn() -> *const c_char;
    unsafe extern "C" {
        fn fdopen(fd: c_int, mode: *const c_char) -> *mut c_void;
        fn close(fd: c_int) -> c_int;
        fn fwrite(p: *const c_void, size: usize, count: usize, stream: *mut c_void) -> usize;
        fn fflush(stream: *mut c_void) -> c_int;
        fn fclose(stream: *mut c_void) -> c_int;
    }
    unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> T {
        // This module alone may load the original, strictly as a pinned oracle.
        unsafe { *lib.get::<T>(name).unwrap() }
    }
    struct NativeLame {
        _library: Library,
        init: Init, channels: Set, rate: Set, mode: Set, brate: Set, quality: Set,
        vbr: Set, mean: Set, vbr_q: Set, write_tag: Set, init_params: One,
        encode: Encode, flush: Flush, tags: Tags, close: One,
    }
    impl NativeLame {
        fn load(path: &Path) -> Self {
            let sha = file_sha256(path).unwrap();
            assert!(sha == REF_UNIVERSAL || sha == REF_ARM64, "not the pinned PN LAME oracle: {sha}");
            // SAFETY: this opt-in probe explicitly requires the pinned reference oracle.
            let library = unsafe { Library::new(path).unwrap() };
            unsafe {
                let version: Version = sym(&library, b"get_lame_version\0");
                assert_eq!(CStr::from_ptr(version()).to_bytes(), b"3.100");
                Self {
                    init: sym(&library, b"lame_init\0"), channels: sym(&library, b"lame_set_num_channels\0"),
                    rate: sym(&library, b"lame_set_in_samplerate\0"), mode: sym(&library, b"lame_set_mode\0"),
                    brate: sym(&library, b"lame_set_brate\0"), quality: sym(&library, b"lame_set_quality\0"),
                    vbr: sym(&library, b"lame_set_VBR\0"), mean: sym(&library, b"lame_set_VBR_mean_bitrate_kbps\0"),
                    vbr_q: sym(&library, b"lame_set_VBR_q\0"), write_tag: sym(&library, b"lame_set_bWriteVbrTag\0"),
                    init_params: sym(&library, b"lame_init_params\0"), encode: sym(&library, b"lame_encode_buffer_interleaved\0"),
                    flush: sym(&library, b"lame_encode_flush\0"), tags: sym(&library, b"lame_mp3_tags_fid\0"),
                    close: sym(&library, b"lame_close\0"), _library: library,
                }
            }
        }
    }
    struct NativeStream<'a> { api: &'a NativeLame, flags: NonNull<c_void>, file: NonNull<c_void> }
    impl<'a> NativeStream<'a> {
        fn new(api: &'a NativeLame, path: &Path, channels: u8, rate: u32, mode: Mp3Mode, tag: bool) -> Self {
            Self::try_new(api, path, channels, rate, mode, tag).expect("native LAME init_params")
        }
        fn try_new(api: &'a NativeLame, path: &Path, channels: u8, rate: u32, mode: Mp3Mode, tag: bool) -> Result<Self, i32> {
            unsafe {
                let flags = NonNull::new((api.init)()).unwrap();
                let f = flags.as_ptr();
                assert!((api.channels)(f, i32::from(channels)) >= 0);
                assert!((api.rate)(f, rate as i32) >= 0);
                assert!((api.mode)(f, if channels == 1 { 3 } else { 0 }) >= 0);
                match mode {
                    Mp3Mode::Cbr320 => { assert!((api.brate)(f, 320) >= 0); assert!((api.quality)(f, 4) >= 0); }
                    Mp3Mode::Vbr192 => { assert!((api.vbr)(f, 3) >= 0); assert!((api.mean)(f, 192) >= 0); assert!((api.vbr_q)(f, 5) >= 0); }
                }
                assert!((api.write_tag)(f, i32::from(tag)) >= 0);
                let status = (api.init_params)(f);
                if status < 0 {
                    (api.close)(f);
                    return Err(status);
                }
                let file = OpenOptions::new().write(true).read(true).create_new(true).open(path).unwrap();
                let fd = file.into_raw_fd();
                let stream = fdopen(fd, c"r+".as_ptr());
                if stream.is_null() { close(fd); panic!("fdopen: {}", std::io::Error::last_os_error()); }
                Ok(Self { api, flags, file: NonNull::new(stream).unwrap() })
            }
        }
        fn write(&mut self, samples: &mut [i16]) {
            let frames = samples.len() / 2;
            let mut output = vec![0u8; frames * 5 / 2 + 7200];
            unsafe {
                let n = (self.api.encode)(self.flags.as_ptr(), samples.as_mut_ptr(), frames as i32, output.as_mut_ptr(), output.len() as i32);
                assert!(n >= 0);
                assert_eq!(fwrite(output.as_ptr().cast(), 1, n as usize, self.file.as_ptr()), n as usize);
            }
        }
        fn finish(self) {
            unsafe {
                let mut output = [0u8; 10_000];
                let n = (self.api.flush)(self.flags.as_ptr(), output.as_mut_ptr(), output.len() as i32);
                assert!(n >= 0);
                assert_eq!(fwrite(output.as_ptr().cast(), 1, n as usize, self.file.as_ptr()), n as usize);
                assert_eq!(fflush(self.file.as_ptr()), 0);
                (self.api.tags)(self.flags.as_ptr(), self.file.as_ptr());
                assert_eq!((self.api.close)(self.flags.as_ptr()), 0);
                assert_eq!(fclose(self.file.as_ptr()), 0);
            }
        }
    }
    fn probe_dir() -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("local-artifacts/mp3-implementation");
        fs::create_dir_all(&root).unwrap();
        let name = format!("probe-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
        let path = root.join(name);
        fs::create_dir(&path).unwrap();
        path
    }
    fn required(name: &str) -> String { std::env::var(name).unwrap_or_else(|_| panic!("set {name} for the ignored native probe")) }

    #[cfg(target_arch = "aarch64")]
    unsafe fn call_native_update(function: unsafe extern "C" fn(), path: &CStr, source: &Mp3Header, scratch: u8) {
        use std::arch::asm;
        let mut result = [0u64; 5];
        result[0] = u64::from(scratch) << 24;
        let mut id = [0u8; 10];
        if let Some(identifier) = source.lame_identifier { id[..9].copy_from_slice(&identifier); }
        let remove = u32::from(source.lame_identifier.is_none());
        // SAFETY: this is the macOS ARM64 ABI used by the pinned updater:
        // x0..x7 are explicit arguments, x8 points to initialized result storage,
        // and the final two bools occupy bytes 0 and 1 of the aligned stack.
        unsafe {
            asm!(
                "sub sp, sp, #16",
                "strb {xing:w}, [sp]",
                "strb {crc:w}, [sp, #1]",
                "blr {target}",
                "add sp, sp, #16",
                target = in(reg) function as *const (),
                xing = in(reg) u32::from(source.xing),
                crc = in(reg) u32::from(source.correct_crc),
                in("x0") path.as_ptr(),
                in("x1") remove,
                in("x2") 1u32 - remove,
                in("x3") id.as_ptr(),
                in("x4") source.encoder_delay,
                in("x5") -1i32,
                in("x6") u32::from(source.stereo_mode_change),
                in("x7") 1u32,
                in("x8") result.as_mut_ptr(),
                clobber_abi("C"),
            );
        }
    }

    /// Run with `cargo test --test mp3_reference -- --ignored --nocapture` and
    /// PN_MP3_LAME_PATH, PN_MP3_LAME_SHA256, PN_MP3_REFERENCE_LAME_PATH set.
    /// Artifacts stay under local-artifacts/mp3-implementation. Comparison is
    /// of complete recorder bytes before PN metadata/tag copying, not full-app
    /// output. A failure identifies the paths and both SHA-256 values.
    #[test]
    #[ignore = "requires explicitly pinned LAME 3.100 and a pinned native oracle"]
    fn converted_stereo_and_mono_against_bundled_lame_oracle() {
        let candidate = LameLibrary::load(required("PN_MP3_LAME_PATH"), &required("PN_MP3_LAME_SHA256")).unwrap();
        let native_path = required("PN_MP3_REFERENCE_LAME_PATH");
        let native_hash = file_sha256(&native_path).unwrap();
        let pinned_reference = LameLibrary::load(&native_path, &native_hash).unwrap();
        assert_eq!(pinned_reference.identity().sha256, native_hash);
        drop(pinned_reference);
        assert!(matches!(LameLibrary::load(&native_path, &"0".repeat(64)),
            Err(platinum_engine::mp3::Mp3Error::DigestMismatch { .. })));
        let native = NativeLame::load(Path::new(&native_path));
        let dir = probe_dir();
        println!("candidate: {:?}; probe artifacts: {}", candidate.identity(), dir.display());
        let occupied = dir.join("occupied.mp3");
        fs::write(&occupied, b"existing file").unwrap();
        let config = Mp3Config { sample_rate: 44_100, channels: 2, mode: Mp3Mode::Cbr320,
            source: SourceHeader::Other(Mp3Header::default()), native_update_result_byte_3: None };
        assert!(matches!(&Mp3Encoder::create_new(&candidate, &occupied, config),
            Err(platinum_engine::mp3::Mp3Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists));
        assert_eq!(fs::read(&occupied).unwrap(), b"existing file");
        for channels in [1u8, 2] {
            for mode in [Mp3Mode::Cbr320, Mp3Mode::Vbr192] {
                for tag in [false, true] {
                    for rate in [22_050u32, 32_000, 44_100, 48_000] {
                        let scenarios: [(&str, &[usize]); 4] = [
                            ("empty", &[]), ("single", &[1]), ("block", &[512]),
                            ("irregular", &[512, 512, 512, 512, 17, 31, 257, 512, 511, 730]),
                        ];
                        for (scenario, blocks) in scenarios {
                        let label = format!("ch{channels}-{mode:?}-tag{tag}-{rate}-{scenario}");
                        let ours = dir.join(format!("{label}-candidate.mp3"));
                        let reference = dir.join(format!("{label}-native.mp3"));
                        let source = if tag { SourceHeader::Other(Mp3Header::default()) } else { SourceHeader::Mp3(Mp3Header::default()) };
                        let config = Mp3Config { sample_rate: rate, channels, mode, source, native_update_result_byte_3: None };
                        let candidate_result = Mp3Encoder::create_new(&candidate, &ours, config);
                        let native_result = NativeStream::try_new(&native, &reference, channels, rate, mode, tag);
                        let (mut encoder, mut original) = match (candidate_result, native_result) {
                            (Ok(encoder), Ok(original)) => (encoder, original),
                            (Err(platinum_engine::mp3::Mp3Error::Lame { operation: "init_params", code }), Err(native_code)) => {
                                assert_eq!(code, native_code, "{label}: init error differs");
                                println!("{label}: both libraries rejected init_params with {code}");
                                continue;
                            }
                            (Err(e), Err(code)) => panic!("{label}: candidate {e}, native init {code}"),
                            (Err(e), Ok(_)) => panic!("{label}: candidate {e}, native initialized"),
                            (Ok(_), Err(code)) => panic!("{label}: candidate initialized, native init {code}"),
                        };
                        let mut converter = Pcm16StereoConverter::new(rate as f64, u32::from(channels)).unwrap();
                        let mut offset = 0usize;
                        for &frames in blocks {
                            let mut input = Vec::with_capacity(frames * channels as usize);
                            for frame in offset..offset + frames {
                                for ch in 0..channels as usize {
                                    input.push((((frame * 17 + ch * 137) % 2029) as i32 - 1014) as f32 / 1024.0);
                                }
                            }
                            let mut pcm = vec![0i16; 2 * frames];
                            assert_eq!(converter.convert(&input, &mut pcm).unwrap(), frames);
                            let mut original_pcm = pcm.clone();
                            encoder.write_converted_stereo(&mut pcm).unwrap();
                            original.write(&mut original_pcm);
                            offset += frames;
                        }
                        let report = encoder.finish().unwrap();
                        original.finish();
                        converter.close().unwrap();
                        let candidate_hash = file_sha256(&ours).unwrap();
                        let native_hash = file_sha256(&reference).unwrap();
                        println!("{label}: {report:?} candidate={candidate_hash} reference={native_hash}");
                        assert_eq!(fs::read(&ours).unwrap(), fs::read(&reference).unwrap(),
                            "whole-recorder mismatch: {} vs {}", ours.display(), reference.display());
                        }
                    }
                }
            }
        }
    }
    /// In addition to the three LAME variables above, requires
    /// PN_MP3_REFERENCE_MIKAUDIO_PATH pointing to the pinned framework binary.
    /// It calls the reference only in this opt-in test, after recorder finish.
    #[cfg(target_arch = "aarch64")]
    #[test]
    #[ignore = "requires pinned native LAME and MIKAudio comparison oracles"]
    fn xing_source_patch_against_mikaudio_oracle() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;


        let candidate = LameLibrary::load(required("PN_MP3_LAME_PATH"), &required("PN_MP3_LAME_SHA256")).unwrap();
        let native = NativeLame::load(Path::new(&required("PN_MP3_REFERENCE_LAME_PATH")));
        let audio_path = PathBuf::from(required("PN_MP3_REFERENCE_MIKAUDIO_PATH"));
        let sha = file_sha256(&audio_path).unwrap();
        assert!(sha == "f793e89ef220486f6aea96b33654145fed4ce232f2a69edf2f5b54e5f40c2a24" ||
            sha == "05b607da99a54400095075f8384982b4b56e03c6d0ecb1f8e42851e629adffa5",
            "not the pinned MIKAudio oracle: {sha}");
        // SAFETY: this test deliberately loads a digest-pinned reference as a comparison oracle.
        let audio = unsafe { Library::new(&audio_path).unwrap() };
        let update: unsafe extern "C" fn() = unsafe { sym(&audio, b"MIKLameInfoHeaderUpdate\0") };
        let source = inspect_mp3_header_bytes(&header_fixture(true, false)).unwrap();
        let dir = probe_dir();
        let ours = dir.join("xing-candidate.mp3");
        let reference = dir.join("xing-native.mp3");
        let config = Mp3Config { sample_rate: 44_100, channels: 2, mode: Mp3Mode::Cbr320,
            source: SourceHeader::Mp3(source.clone()), native_update_result_byte_3: None };
        let mut encoder = Mp3Encoder::create_new(&candidate, &ours, config).unwrap();
        let mut original = NativeStream::new(&native, &reference, 2, 44_100, Mp3Mode::Cbr320, true);
        let mut converter = Pcm16StereoConverter::new(44_100.0, 2).unwrap();
        for block in 0..8usize {
            let samples: Vec<f32> = (0..1024).map(|i| (((block * 1024 + i) * 37 % 2003) as i32 - 1001) as f32 / 1024.0).collect();
            let mut pcm = vec![0i16; 1024];
            assert_eq!(converter.convert(&samples, &mut pcm).unwrap(), 512);
            let mut native_pcm = pcm.clone();
            encoder.write_converted_stereo(&mut pcm).unwrap();
            original.write(&mut native_pcm);
        }
        let report = encoder.finish().unwrap();
        original.finish();
        converter.close().unwrap();
        let path = CString::new(reference.as_os_str().as_bytes()).unwrap();
        // SAFETY: digest-pinned updater, initialized indirect result, and live path.
        unsafe { call_native_update(update, &path, &source, 0); }
        println!("source patch {report:?}: candidate={} reference={}",
            file_sha256(&ours).unwrap(), file_sha256(&reference).unwrap());
        assert!(report.source_header_patch_applied);
        assert_eq!(fs::read(&ours).unwrap(), fs::read(&reference).unwrap(),
            "whole-recorder mismatch: {} vs {}", ours.display(), reference.display());
    }
    /// The native updater uses an indirect 40-byte result in x8. Control its
    /// byte +3 explicitly to reproduce both machine-code paths; this is an
    /// oracle diagnostic, not a suggested CLI option or a parity assumption.
    #[cfg(target_arch = "aarch64")]
    #[test]
    #[ignore = "requires pinned native LAME and MIKAudio comparison oracles"]
    fn source_without_lame_controlled_native_scratch_probe() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let candidate = LameLibrary::load(required("PN_MP3_LAME_PATH"), &required("PN_MP3_LAME_SHA256")).unwrap();
        let native = NativeLame::load(Path::new(&required("PN_MP3_REFERENCE_LAME_PATH")));
        let audio_path = PathBuf::from(required("PN_MP3_REFERENCE_MIKAUDIO_PATH"));
        let sha = file_sha256(&audio_path).unwrap();
        assert!(sha == "f793e89ef220486f6aea96b33654145fed4ce232f2a69edf2f5b54e5f40c2a24" ||
            sha == "05b607da99a54400095075f8384982b4b56e03c6d0ecb1f8e42851e629adffa5");
        let audio = unsafe { Library::new(&audio_path).unwrap() };
        let update: unsafe extern "C" fn() = unsafe { sym(&audio, b"MIKLameInfoHeaderUpdate\0") };
        let source = inspect_mp3_header_bytes(&header_fixture(false, false)).unwrap();
        assert!(source.xing && source.lame_identifier.is_none());
        let dir = probe_dir();
        let mut native_paths = Vec::new();
        for scratch in [0u8, 1] {
            let ours = dir.join(format!("scratch-{scratch}-candidate.mp3"));
            let reference = dir.join(format!("scratch-{scratch}-native.mp3"));
            let config = Mp3Config { sample_rate: 44_100, channels: 2, mode: Mp3Mode::Cbr320,
                source: SourceHeader::Mp3(source.clone()), native_update_result_byte_3: Some(scratch) };
            let mut encoder = Mp3Encoder::create_new(&candidate, &ours, config).unwrap();
            let mut original = NativeStream::new(&native, &reference, 2, 44_100, Mp3Mode::Cbr320, true);
            let mut converter = Pcm16StereoConverter::new(44_100.0, 2).unwrap();
            for block in 0..8usize {
                let input: Vec<f32> = (0..1024).map(|i| (((block * 1024 + i) * 37 % 2003) as i32 - 1001) as f32 / 1024.0).collect();
                let mut pcm = vec![0i16; 1024];
                assert_eq!(converter.convert(&input, &mut pcm).unwrap(), 512);
                let mut reference_pcm = pcm.clone();
                encoder.write_converted_stereo(&mut pcm).unwrap();
                original.write(&mut reference_pcm);
            }
            let report = encoder.finish().unwrap();
            original.finish();
            converter.close().unwrap();
            let path = CString::new(reference.as_os_str().as_bytes()).unwrap();
            // SAFETY: digest-pinned updater, initialized indirect result, and live path.
            unsafe { call_native_update(update, &path, &source, scratch); }
            assert!(report.source_header_patch_applied);
            println!("scratch={scratch} candidate={} native={}", file_sha256(&ours).unwrap(), file_sha256(&reference).unwrap());
            assert_eq!(fs::read(&ours).unwrap(), fs::read(&reference).unwrap(),
                "scratch={scratch}: {} vs {}", ours.display(), reference.display());
            native_paths.push(reference);
        }
        assert_ne!(fs::read(&native_paths[0]).unwrap(), fs::read(&native_paths[1]).unwrap(),
            "the two controlled native stack states did not change bytes; inspect the outputs");
    }
    /// Independently compare our source inspection with MIKAudio's returned
    /// 40-byte record on synthetic Xing/Info, ID3 and LAME-presence cases.
    #[test]
    #[ignore = "requires the pinned native MIKAudio comparison oracle"]
    fn source_header_scan_against_mikaudio_oracle() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        #[repr(C, align(8))]
        struct NativeHeaderResult { bytes: [u8; 36] }
        type Load = unsafe extern "C" fn(*const c_char) -> NativeHeaderResult;
        let audio_path = PathBuf::from(required("PN_MP3_REFERENCE_MIKAUDIO_PATH"));
        let sha = file_sha256(&audio_path).unwrap();
        assert!(sha == "f793e89ef220486f6aea96b33654145fed4ce232f2a69edf2f5b54e5f40c2a24" ||
            sha == "05b607da99a54400095075f8384982b4b56e03c6d0ecb1f8e42851e629adffa5");
        let audio = unsafe { Library::new(&audio_path).unwrap() };
        let load: Load = unsafe { sym(&audio, b"MIKLameInfoHeaderLoad\0") };
        let dir = probe_dir();
        for id3 in [false, true] {
            for lame in [false, true] {
                for info in [false, true] {
                    let mut fixture = header_fixture(lame, id3);
                    let xing = if id3 { 51 } else { 36 };
                    if info { fixture[xing..xing + 4].copy_from_slice(b"Info"); }
                    if lame {
                        let tag = xing + 120;
                        let frame = if id3 { 15 } else { 0 };
                        let crc = crc16(&fixture[frame..tag + 34]);
                        fixture[tag + 34..tag + 36].copy_from_slice(&crc.to_be_bytes());
                    }
                    let path = dir.join(format!("source-id3{id3}-lame{lame}-info{info}.mp3"));
                    fs::write(&path, &fixture).unwrap();
                    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
                    // SAFETY: the loader initializes all 36 data bytes; trailing
                    // alignment padding is not represented as Rust data.
                    let native = unsafe { load(name.as_ptr()) };
                    let bytes = &native.bytes;
                    let u32_at = |i: usize| u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap());
                    let actual = inspect_mp3_header_bytes(&fixture).unwrap();
                    assert_eq!(actual.valid_layer3, bytes[0] != 0);
                    assert_eq!(actual.xing, bytes[1] != 0);
                    assert_eq!(actual.info, bytes[2] != 0);
                    assert_eq!(actual.lame_identifier.is_some(), bytes[3] != 0);
                    assert_eq!(actual.encoder_delay, u32_at(4));
                    assert_eq!(actual.encoder_padding, u32_at(8));
                    assert_eq!(actual.start_skip_samples, u32_at(12));
                    assert_eq!(actual.stereo_mode_change, bytes[16] != 0);
                    assert_eq!(actual.stereo_mode, u32_at(20));
                    assert_eq!(actual.correct_crc, bytes[24] != 0);
                    if let Some(id) = actual.lame_identifier { assert_eq!(id.as_slice(), &bytes[25..34]); }
                }
            }
        }
    }
}
