#[cfg(all(target_os = "macos", not(feature = "portable")))]
mod native {

use platinum_engine::apple_codec::{OutputFormat, Reader, Writer, WriterConfig};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

fn artifact_dir(name: &str) -> PathBuf {
    let parent = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").expect("set PN_AUDIO_ARTIFACTS to a new audio-01 directory"));
    let name = match std::env::var("PN_AUDIO_RUN") { Ok(suffix) => format!("{name}-{suffix}"), Err(_) => name.to_owned() };
    let dir = parent.join(name);
    fs::create_dir(&dir).expect("probe directory must not already exist");
    dir
}

fn write_new(path: &Path, bytes: &[u8]) {
    let mut f = OpenOptions::new().write(true).create_new(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
}

fn next_random(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

#[test]
#[ignore = "records the macOS AudioToolbox PCM quantizer; requires PN_AUDIO_ARTIFACTS"]
fn capture_native_pcm_quantization() {
    let dir = artifact_dir("quantization");
    let mut samples = vec![
        0.0, -0.0, 1.0, -1.0, 2.0, -2.0, f32::INFINITY, f32::NEG_INFINITY,
        f32::NAN, f32::MIN_POSITIVE, -f32::MIN_POSITIVE, f32::from_bits(1), f32::from_bits(0x8000_0001),
    ];
    for bits in [16, 24, 32] {
        let scale = 2.0f64.powi(bits - 1);
        for q in [-32_768, -1001, -1, 0, 1, 1001, 32_767] {
            for fraction in [-0.5001, -0.5, -0.4999, -0.0001, 0.0, 0.0001, 0.4999, 0.5, 0.5001] {
                samples.push(((q as f64 + fraction) / scale) as f32);
            }
        }
    }
    let seed = 0x9e37_79b9;
    let mut state = seed;
    for _ in 0..65_536 {
        samples.push(f32::from_bits(next_random(&mut state)));
    }
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_bits().to_le_bytes()).collect();
    write_new(&dir.join("source.f32le"), &bytes);
    let mut rows = Vec::new();
    for (name, format) in [("wav", OutputFormat::Wav), ("aiff", OutputFormat::Aiff)] {
        for bits in [16, 24, 32] {
            let path = dir.join(format!("{name}-{bits}.{name}"));
            let config = WriterConfig { format, sample_rate: 44_100.0, channels: 1, bit_depth: bits };
            let mut writer = Writer::create(&path, config).unwrap();
            for block in samples.chunks(512) { writer.write(block).unwrap(); }
            writer.finish().unwrap();
            rows.push(json!({"path": path, "bits": bits, "frames": samples.len()}));
        }
    }
    let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh", "seed": seed,
        "sample_count": samples.len(), "pattern":"explicit IEEE classes, quantizer boundaries, then xorshift32 bit patterns",
        "outputs": rows});
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
}

fn wav_float(rate: u32, samples: &[f32]) -> Vec<u8> {
    let size = (samples.len() * 4) as u32;
    let mut out = Vec::with_capacity(44 + size as usize);
    out.extend_from_slice(b"RIFF"); out.extend_from_slice(&(36 + size).to_le_bytes()); out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 4).to_le_bytes()); out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes()); out.extend_from_slice(b"data");
    out.extend_from_slice(&size.to_le_bytes());
    for value in samples { out.extend_from_slice(&value.to_bits().to_le_bytes()); }
    out
}

fn capture_decode(source: &Path, output: &Path, rate: f64) -> usize {
    let mut reader = Reader::open(source, Some(rate)).unwrap();
    let mut buffer = vec![0.0; 512 * reader.client_format().channels_per_frame as usize];
    let mut result = Vec::new();
    loop {
        let count = reader.read(&mut buffer).unwrap();
        if count == 0 { break; }
        for v in &buffer[..count] { result.extend_from_slice(&v.to_bits().to_le_bytes()); }
    }
    reader.close().unwrap();
    write_new(output, &result);
    result.len() / 4
}

#[test]
#[ignore = "records macOS AudioToolbox sample-rate-conversion impulse responses; requires PN_AUDIO_ARTIFACTS"]
fn capture_native_src_impulses() {
    let dir = artifact_dir("src-impulses");
    let frames = 4096;
    let mut state = 0x6d2b_79f5;
    let random: Vec<f32> = (0..frames).map(|_| ((next_random(&mut state) >> 8) as f32 / 16_777_216.0) - 0.5).collect();
    let mut cases: Vec<(&str, Vec<f32>)> = Vec::new();
    for (name, impulses) in [
        ("zero", &[][..]), ("a", &[(1024usize, 1.0f32)][..]),
        ("b", &[(1031, 1.0)][..]), ("both", &[(1024, 1.0), (1031, 1.0)][..]),
        ("half", &[(1024, 0.5)][..]), ("shift", &[(1025, 1.0)][..]),
    ] {
        let mut s = vec![0.0; frames];
        for &(at, value) in impulses { s[at] = value; }
        cases.push((name, s));
    }
    cases.push(("random", random));
    let mut rows = Vec::new();
    for source_rate in [11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000] {
        for (name, signal) in &cases {
            let source = dir.join(format!("{source_rate}-{name}.wav"));
            let decoded = dir.join(format!("{source_rate}-{name}.f32le"));
            write_new(&source, &wav_float(source_rate, signal));
            let count = capture_decode(&source, &decoded, 44_100.0);
            rows.push(json!({"source_rate":source_rate,"destination_rate":44_100,"signal":name,
                "source":source,"output":decoded,"source_frames":frames,"output_frames":count}));
            if *name == "random" {
                let repeat = dir.join(format!("{source_rate}-random-repeat.f32le"));
                let repeat_count = capture_decode(&source, &repeat, 44_100.0);
                rows.push(json!({"source_rate":source_rate,"destination_rate":44_100,"signal":"random-repeat",
                    "source":source,"output":repeat,"source_frames":frames,"output_frames":repeat_count}));
            }
        }
    }
    let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh", "input":"mono RIFF IEEE-float32, 4096 frames",
        "random_seed":"0x6d2b79f5", "read_block_frames":512, "cases":rows});
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
}

#[test]
#[ignore = "records same-rate native PCM stream descriptions and stereo-planar reads"]
fn capture_native_pcm_planar() {
    let dir = artifact_dir("native-input-planar");
    let source_dir = dir.parent().unwrap().join("native-input");
    let mut rows = Vec::new();
    for format in ["wav", "aiff"] {
        for bits in [8, 16, 24, 32] {
            for channels in [1, 2, 6] {
                let frames = if channels == 1 && bits == 16 { 65_536 } else { 1027 };
                let name = format!("{format}-{bits}-{channels}-{frames}");
                let source = source_dir.join(format!("{name}.{format}"));
                let mut reader = Reader::open(&source, None).unwrap();
                let asbd = reader.source_format();
                let length = reader.file_length_frames().unwrap();
                let bitrate = reader.bitrate().ok();
                reader.set_stereo_planar(asbd.sample_rate).unwrap();
                let mut left = [13.0f32; 257];
                let mut right = [-17.0f32; 257];
                let mut bytes = Vec::new();
                let mut count = 0usize;
                loop {
                    let n = reader.read_stereo_planar(&mut left, &mut right).unwrap();
                    if n == 0 { break; }
                    for i in 0..n {
                        bytes.extend_from_slice(&left[i].to_bits().to_le_bytes());
                        bytes.extend_from_slice(&right[i].to_bits().to_le_bytes());
                    }
                    count += n;
                }
                reader.close().unwrap();
                let output = dir.join(format!("{name}.stereo.f32le"));
                write_new(&output, &bytes);
                rows.push(json!({"input":source,"output":output,"frames":count,"native_length":length,"bitrate":bitrate,
                    "asbd":{"sample_rate":asbd.sample_rate,"format_id":asbd.format_id,"format_flags":asbd.format_flags,
                    "bytes_per_packet":asbd.bytes_per_packet,"frames_per_packet":asbd.frames_per_packet,
                    "bytes_per_frame":asbd.bytes_per_frame,"channels_per_frame":asbd.channels_per_frame,
                    "bits_per_channel":asbd.bits_per_channel}}));
            }
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh", "block_frames":257, "cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "records effective AudioConverter properties without changing them"]
fn capture_native_converter_properties() {
    use platinum_engine::apple_codec::StreamDescription;
    use std::ffi::c_void;
    use std::ptr;

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn ExtAudioFileCreateWithURL(url: *const c_void, kind: u32, format: *const StreamDescription,
            layout: *const c_void, flags: u32, result: *mut *mut c_void) -> i32;
        fn ExtAudioFileOpenURL(url: *const c_void, result: *mut *mut c_void) -> i32;
        fn ExtAudioFileGetProperty(file: *mut c_void, property: u32, size: *mut u32, value: *mut c_void) -> i32;
        fn ExtAudioFileSetProperty(file: *mut c_void, property: u32, size: u32, value: *const c_void) -> i32;
        fn ExtAudioFileDispose(file: *mut c_void) -> i32;
        fn AudioConverterGetProperty(converter: *mut c_void, property: u32, size: *mut u32, value: *mut c_void) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(allocator: *const c_void, bytes: *const u8, length: isize, directory: u8) -> *const c_void;
        fn CFRelease(object: *const c_void);
    }
    fn fourcc(s: &[u8; 4]) -> u32 { u32::from_be_bytes(*s) }
    fn float_asbd(rate: f64, channels: u32) -> StreamDescription {
        StreamDescription { sample_rate: rate, format_id: fourcc(b"lpcm"), format_flags: 9,
            bytes_per_packet: channels * 4, frames_per_packet: 1, bytes_per_frame: channels * 4,
            channels_per_frame: channels, bits_per_channel: 32, reserved: 0 }
    }
    fn url(path: &Path) -> *const c_void {
        let bytes = path.as_os_str().as_encoded_bytes();
        let result = unsafe { CFURLCreateFromFileSystemRepresentation(ptr::null(), bytes.as_ptr(), bytes.len() as isize, 0) };
        assert!(!result.is_null());
        result
    }
    fn inspect(file: *mut c_void) -> serde_json::Value {
        let mut converter = ptr::null_mut::<c_void>();
        let mut size = std::mem::size_of::<*mut c_void>() as u32;
        let status = unsafe { ExtAudioFileGetProperty(file, fourcc(b"acnv"), &mut size, (&mut converter as *mut *mut c_void).cast()) };
        if status != 0 || converter.is_null() {
            return json!({"get_converter_status":status,"available":false});
        }
        let mut values = serde_json::Map::new();
        for name in [b"dith", b"srcq", b"srca", b"prmm"] {
            let mut value = 0u32;
            let mut size = 4u32;
            let status = unsafe { AudioConverterGetProperty(converter, fourcc(name), &mut size, (&mut value as *mut u32).cast()) };
            values.insert(String::from_utf8_lossy(name).into_owned(), json!({"status":status,"value":value,"size":size}));
        }
        json!({"get_converter_status":status,"available":true,"properties":values})
    }

    let dir = artifact_dir("converter-properties");
    let mut rows = Vec::new();
    for (name, kind, id, flags) in [("wav", b"WAVE", b"lpcm", 12u32),
        ("aiff", b"AIFF", b"lpcm", 14), ("alac", b"m4af", b"alac", 1)] {
        for bits in [16, 24, 32] {
            for channels in [1, 2, 6] {
                let path = dir.join(format!("{name}-{bits}-{channels}.{name}"));
                let client = float_asbd(44_100.0, channels);
                let mut dst = StreamDescription { sample_rate: 44_100.0, format_id: fourcc(id),
                    format_flags: if name == "alac" { if bits == 32 { 4 } else if bits == 24 { 3 } else { 1 } } else { flags },
                    channels_per_frame: channels, ..Default::default() };
                if name != "alac" {
                    dst.bits_per_channel = bits;
                    dst.bytes_per_packet = channels * bits / 8;
                    dst.bytes_per_frame = dst.bytes_per_packet;
                    dst.frames_per_packet = 1;
                }
                let url = url(&path);
                let mut file = ptr::null_mut();
                let create_status = unsafe { ExtAudioFileCreateWithURL(url, fourcc(kind), &dst, ptr::null(), 0, &mut file) };
                unsafe { CFRelease(url); }
                assert_eq!(create_status, 0);
                let set_status = unsafe { ExtAudioFileSetProperty(file, fourcc(b"cfmt"), std::mem::size_of_val(&client) as u32, (&client as *const StreamDescription).cast()) };
                assert_eq!(set_status, 0);
                let observed = inspect(file);
                assert_eq!(unsafe { ExtAudioFileDispose(file) }, 0);
                rows.push(json!({"direction":"encode","format":name,"bits":bits,"channels":channels,"observed":observed}));
            }
        }
    }
    for client_rate in [44_100.0, 48_000.0] {
        let source = dir.parent().unwrap().join("native-input/wav-16-2-1027.wav");
        let url = url(&source);
        let mut file = ptr::null_mut();
        let status = unsafe { ExtAudioFileOpenURL(url, &mut file) };
        unsafe { CFRelease(url); }
        assert_eq!(status, 0);
        let client = float_asbd(client_rate, 2);
        assert_eq!(unsafe { ExtAudioFileSetProperty(file, fourcc(b"cfmt"), std::mem::size_of_val(&client) as u32, (&client as *const StreamDescription).cast()) }, 0);
        let observed = inspect(file);
        assert_eq!(unsafe { ExtAudioFileDispose(file) }, 0);
        rows.push(json!({"direction":"decode","format":"wav","source_rate":44_100,"client_rate":client_rate,"observed":observed}));
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh", "inspection_note":"GetProperty is an intervention; no property is set.",
        "cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "records native conversion at quantization boundaries"]
fn capture_native_pcm_boundaries() {
    let dir = artifact_dir("quantizer-boundaries");
    let mut rows = Vec::new();
    for bits in [16, 24] {
        let scale = (1u32 << (bits - 1)) as f64;
        let mut samples = Vec::new();
        for i in -32_768i32..32_768 {
            let q = if bits == 16 { i } else { i * 256 };
            let center = ((q as f64 + 0.5) / scale) as f32;
            samples.extend([center.next_down(), center, center.next_up()]);
        }
        if bits == 24 {
            for q in -4096i32..4096 {
                let center = ((q as f64 + 0.5) / scale) as f32;
                samples.extend([center.next_down(), center, center.next_up()]);
            }
        }
        let source = dir.join(format!("{bits}-samples.f32le"));
        let bytes: Vec<_> = samples.iter().flat_map(|s| s.to_bits().to_le_bytes()).collect();
        write_new(&source, &bytes);
        for (name, format) in [("wav", OutputFormat::Wav), ("aiff", OutputFormat::Aiff)] {
            let output = dir.join(format!("{name}-{bits}.{name}"));
            let config = WriterConfig { format, sample_rate: 44_100.0, channels: 1, bit_depth: bits };
            let mut writer = Writer::create(&output, config).unwrap();
            for block in samples.chunks(512) { writer.write(block).unwrap(); }
            writer.finish().unwrap();
            rows.push(json!({"format":name,"bits":bits,"source":source,"output":output,"samples":samples.len()}));
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh", "pattern":"half-integer quantizer boundaries and adjacent float32 values",
        "cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "compares portable PCM readers against AudioToolbox and records raw-bit results"]
fn portable_pcm_reader_parity() {
    use platinum_engine::portable::audio as portable;
    let dir = artifact_dir("reader-parity");
    let root = dir.parent().unwrap();
    let mut sources = Vec::new();
    for folder in ["native-input", "native-float-input", "native-input-depths", "native-pcm", "native-pcm-exhaustive", "native-aiff-lengths"] {
        for entry in fs::read_dir(root.join(folder)).unwrap() {
            let path = entry.unwrap().path();
            if matches!(path.extension().and_then(|x| x.to_str()), Some("wav" | "aiff")) { sources.push(path); }
        }
    }
    for entry in fs::read_dir(root.join("writer-parity-long-01")).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with("-native.wav") || n.ends_with("-native.aiff")) {
            sources.push(path);
        }
    }
    sources.sort();
    let mut rows = Vec::new();
    let mut failures = 0usize;
    let mut compared = 0u64;
    for path in sources {
        for planar in [false, true] {
            for block in [1usize, 257, 4096] {
                let mut native = Reader::open(&path, None).unwrap();
                let mut ours = match portable::Reader::open(&path, None) {
                    Ok(r) => r,
                    Err(e) => {
                        failures += 1;
                        rows.push(json!({"path":path,"planar":planar,"block":block,"open_error":e.to_string()}));
                        continue;
                    }
                };
                let n = native.source_format();
                let p = ours.source_format();
                let metadata_equal = n.sample_rate == p.sample_rate && n.format_id == p.format_id &&
                    n.format_flags == p.format_flags && n.bytes_per_packet == p.bytes_per_packet &&
                    n.frames_per_packet == p.frames_per_packet && n.bytes_per_frame == p.bytes_per_frame &&
                    n.channels_per_frame == p.channels_per_frame && n.bits_per_channel == p.bits_per_channel &&
                    native.file_length_frames().unwrap() == ours.file_length_frames().unwrap() &&
                    native.bitrate().ok() == ours.bitrate().ok();
                if !metadata_equal { failures += 1; }
                if planar {
                    native.set_stereo_planar(n.sample_rate).unwrap();
                    ours.set_stereo_planar(p.sample_rate).unwrap();
                }
                let channels = if planar { 2 } else { n.channels_per_frame as usize };
                let mut nb = vec![13.0f32; block * channels];
                let mut pb = vec![13.0f32; block * channels];
                let mut nr = vec![-17.0f32; block];
                let mut pr = vec![-17.0f32; block];
                let mut frame_offset = 0usize;
                let mut mismatch_count = 0u64;
                let mut examples = Vec::new();
                loop {
                    let (nn, pn) = if planar {
                        (native.read_stereo_planar(&mut nb[..block], &mut nr).unwrap(),
                         ours.read_stereo_planar(&mut pb[..block], &mut pr).unwrap())
                    } else { (native.read(&mut nb).unwrap(), ours.read(&mut pb).unwrap()) };
                    if nn != pn {
                        failures += 1;
                        examples.push(json!({"offset":frame_offset,"native_frames":nn,"portable_frames":pn}));
                        break;
                    }
                    if nn == 0 { break; }
                    let lanes = if planar { 1 } else { channels };
                    for i in 0..nn * lanes {
                        compared += 1;
                        if nb[i].to_bits() != pb[i].to_bits() {
                            mismatch_count += 1;
                            if examples.len() < 8 { examples.push(json!({"offset":frame_offset,"index":i,
                                "native_bits":format!("{:08x}",nb[i].to_bits()),"portable_bits":format!("{:08x}",pb[i].to_bits())})); }
                        }
                    }
                    if planar {
                        for i in 0..nn {
                            compared += 1;
                            if nr[i].to_bits() != pr[i].to_bits() {
                                mismatch_count += 1;
                                if examples.len() < 8 { examples.push(json!({"offset":frame_offset,"right_index":i,
                                    "native_bits":format!("{:08x}",nr[i].to_bits()),"portable_bits":format!("{:08x}",pr[i].to_bits())})); }
                            }
                        }
                    }
                    frame_offset += nn;
                }
                if mismatch_count != 0 { failures += 1; }
                native.close().unwrap();
                ours.close().unwrap();
                rows.push(json!({"path":path,"planar":planar,"block":block,"frames":frame_offset,
                    "metadata_equal":metadata_equal,"native_asbd":format!("{n:?}"),"portable_asbd":format!("{p:?}"),
                    "mismatch_count":mismatch_count,"examples":examples}));
            }
        }
    }
    let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","compared_sample_bits":compared,"failures":failures,"cases":rows});
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
    assert_eq!(failures, 0, "PCM parity failures; see {}", dir.join("receipt.json").display());
}

#[test]
#[ignore = "compares full portable PCM files to AudioToolbox and records every mismatch"]
fn portable_pcm_writer_parity() {
    use platinum_engine::portable::audio as portable;
    let dir = artifact_dir("writer-parity");
    let mut rows = Vec::new();
    let mut failures = 0usize;
    let mut bytes_compared = 0u64;
    let mut counter = 0usize;
    for (name, native_format, portable_format) in [
        ("wav", OutputFormat::Wav, portable::OutputFormat::Wav),
        ("aiff", OutputFormat::Aiff, portable::OutputFormat::Aiff),
    ] {
        for bits in [16, 24, 32] {
            for channels in [1u32, 2, 6] {
                for rate in [22_050u32, 32_000, 44_100, 48_000, 88_200, 96_000] {
                    for frames in [0usize, 1, 511, 512, 513, 8193, 65_537] {
                        let label = format!("{name}-{bits}-{channels}-{rate}-{frames}");
                        let native_path = dir.join(format!("{label}-native.{name}"));
                        let portable_path = dir.join(format!("{label}-portable.{name}"));
                        let config = WriterConfig { format: native_format, sample_rate: rate as f64, channels, bit_depth: bits };
                        let portable_config = portable::WriterConfig { format: portable_format, sample_rate: rate as f64, channels, bit_depth: bits };
                        let mut state = 0xcafe_77a1u32;
                        let edge = [0.0, -0.0, 1.0, -1.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN,
                            f32::from_bits(1), f32::from_bits(0x8000_0001), -0.5 / 32_768.0,
                            -0.5 / 8_388_608.0, 0.5 / 32_768.0];
                        let samples: Vec<f32> = (0..frames * channels as usize).map(|i| {
                            if i < edge.len() { edge[i] }
                            else if i % 2 == 0 { f32::from_bits(next_random(&mut state)) }
                            else { ((next_random(&mut state) >> 8) as f32 / 8_388_608.0) - 1.0 }
                        }).collect();
                        let mut native = Writer::create(&native_path, config).unwrap();
                        let mut ours = portable::Writer::create(&portable_path, portable_config).unwrap();
                        for block in samples.chunks(512 * channels as usize) {
                            native.write(block).unwrap();
                            ours.write(block).unwrap();
                        }
                        native.finish().unwrap();
                        ours.finish().unwrap();
                        let n = fs::read(&native_path).unwrap();
                        let p = fs::read(&portable_path).unwrap();
                        bytes_compared += n.len().min(p.len()) as u64;
                        let count = n.iter().zip(&p).filter(|(a, b)| a != b).count() + n.len().abs_diff(p.len());
                        if count != 0 { failures += 1; }
                        let first = n.iter().zip(&p).position(|(a, b)| a != b);
                        rows.push(json!({"case":label,"frames":frames,"native_path":native_path,"portable_path":portable_path,
                            "native_len":n.len(),"portable_len":p.len(),"differing_bytes":count,"first_differing_byte":first,
                            "native_sha256":platinum_engine::mp3::file_sha256(&native_path).unwrap(),
                            "portable_sha256":platinum_engine::mp3::file_sha256(&portable_path).unwrap()}));
                        counter += 1;
                    }
                }
            }
        }
    }
    for bits in [16u32, 24] {
        let source = dir.parent().unwrap().join(format!("quantizer-boundaries/{bits}-samples.f32le"));
        let raw = fs::read(source).unwrap();
        let samples: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_bits(u32::from_le_bytes(b.try_into().unwrap()))).collect();
        for (name, format) in [("wav", portable::OutputFormat::Wav), ("aiff", portable::OutputFormat::Aiff)] {
            let native_path = dir.parent().unwrap().join(format!("quantizer-boundaries/{name}-{bits}.{name}"));
            let output = dir.join(format!("boundaries-{name}-{bits}.{name}"));
            let mut writer = portable::Writer::create(&output, portable::WriterConfig {
                format, sample_rate: 44_100.0, channels: 1, bit_depth: bits
            }).unwrap();
            for block in samples.chunks(512) { writer.write(block).unwrap(); }
            writer.finish().unwrap();
            let n = fs::read(&native_path).unwrap(); let p = fs::read(&output).unwrap();
            bytes_compared += n.len().min(p.len()) as u64;
            let count = n.iter().zip(&p).filter(|(a, b)| a != b).count() + n.len().abs_diff(p.len());
            if count != 0 { failures += 1; }
            rows.push(json!({"case":format!("boundary-{name}-{bits}"),"samples":samples.len(),"native_path":native_path,
                "portable_path":output,"differing_bytes":count,"first_differing_byte":n.iter().zip(&p).position(|(a,b)| a!=b),
                "native_sha256":platinum_engine::mp3::file_sha256(&native_path).unwrap(),
                "portable_sha256":platinum_engine::mp3::file_sha256(&output).unwrap()}));
            counter += 1;
        }
    }
    let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","seed":"0xcafe77a1",
        "matrix_cases":counter,"bytes_compared":bytes_compared,"failures":failures,"cases":rows});
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
    assert_eq!(failures, 0, "PCM writer parity failures; see {}", dir.join("receipt.json").display());
}

#[test]
#[ignore = "compares PCM output at fractional and extra channel configurations"]
fn portable_pcm_writer_extended_parity() {
    use platinum_engine::portable::audio as portable;
    let dir = artifact_dir("pcm-writer-extended");
    let mut rows = Vec::new(); let mut failures = 0usize; let mut compared = 0u64;
    for (name, native_format, portable_format) in [
        ("wav", OutputFormat::Wav, portable::OutputFormat::Wav),
        ("aiff", OutputFormat::Aiff, portable::OutputFormat::Aiff),
    ] {
        for bits in [0, 16, 24, 32] {
            for channels in 1u32..=8 {
                for rate in [8_000.0, 11_025.0, 22_050.25, 32_000.5, 44_100.4, 44_100.5,
                    48_000.75, 57_343.99, 57_344.01, 88_200.5, 95_999.9, 96_001.0, 192_000.0] {
                    for frames in [1usize, 513] {
                        let label = format!("{name}-{bits}-{channels}-{rate}-{frames}");
                        let native_path = dir.join(format!("{label}-native.{name}"));
                        let portable_path = dir.join(format!("{label}-portable.{name}"));
                        let native_config = WriterConfig { format: native_format, sample_rate: rate, channels, bit_depth: bits };
                        let portable_config = portable::WriterConfig { format: portable_format, sample_rate: rate, channels, bit_depth: bits };
                        let native = Writer::create(&native_path, native_config);
                        let ours = portable::Writer::create(&portable_path, portable_config);
                        let (mut native, mut ours) = match (native, ours) {
                            (Ok(native), Ok(ours)) => (native, ours),
                            (native, ours) => {
                                if native.is_ok() != ours.is_ok() { failures += 1; }
                                rows.push(json!({"case":label,"native_create":native.err().map(|e| e.to_string()),
                                    "portable_create":ours.err().map(|e| e.to_string())}));
                                continue;
                            }
                        };
                        let mut state = 0x6bd1_0e9fu32;
                        let input: Vec<f32> = (0..frames * channels as usize).map(|i| {
                            if i % 2 == 0 { f32::from_bits(next_random(&mut state)) }
                            else { ((next_random(&mut state) >> 8) as f32 / 8_388_608.0) - 1.0 }
                        }).collect();
                        for block in input.chunks(512 * channels as usize) {
                            native.write(block).unwrap(); ours.write(block).unwrap();
                        }
                        native.finish().unwrap(); ours.finish().unwrap();
                        let expected = fs::read(&native_path).unwrap(); let actual = fs::read(&portable_path).unwrap();
                        compared += expected.len().min(actual.len()) as u64;
                        let differing = expected.iter().zip(&actual).filter(|(a,b)| a != b).count() + expected.len().abs_diff(actual.len());
                        if differing != 0 { failures += 1; }
                        rows.push(json!({"case":label,"differing_bytes":differing,
                            "first_differing_byte":expected.iter().zip(&actual).position(|(a,b)| a != b),
                            "native_sha256":platinum_engine::mp3::file_sha256(&native_path).unwrap(),
                            "portable_sha256":platinum_engine::mp3::file_sha256(&portable_path).unwrap()}));
                    }
                }
            }
        }
    }
    let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","seed":"0x6bd10e9f",
        "compared_bytes":compared,"failures":failures,"cases":rows});
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
    assert_eq!(failures, 0, "extended PCM output mismatch; see {}", dir.display());
}

#[test]
#[ignore = "captures same-rate native PCM output at all requested rates for Linux replay"]
fn capture_native_pcm_rate_corpus() {
    let dir = artifact_dir("native-pcm-rates");
    let source = dir.parent().unwrap().join("writer-parity-long-01");
    let mut entries: Vec<_> = fs::read_dir(source).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    let mut rows = Vec::new();
    for path in entries {
        let name = path.file_name().unwrap().to_str().unwrap();
        if !name.ends_with("-native.wav") && !name.ends_with("-native.aiff") { continue; }
        let target = dir.join(name);
        write_new(&target, &fs::read(&path).unwrap());
        let mut reader = Reader::open(&path, None).unwrap();
        let description = reader.source_format();
        let mut buffer = vec![0.0f32; 512 * description.channels_per_frame as usize];
        let mut output = Vec::new(); let mut frames = 0usize;
        loop {
            let count = reader.read(&mut buffer).unwrap();
            if count == 0 { break; }
            frames += count;
            for sample in &buffer[..count * description.channels_per_frame as usize] {
                output.extend_from_slice(&sample.to_bits().to_le_bytes());
            }
        }
        let expected_frames = reader.file_length_frames().unwrap();
        reader.close().unwrap();
        assert_eq!(frames as i64, expected_frames);
        let reference = target.with_extension("f32le");
        write_new(&reference, &output);
        rows.push(json!({"path":target,"native_source":path,"source_asbd":format!("{description:?}"),
            "frames":frames,"reference":reference,"reference_sha256":platinum_engine::mp3::file_sha256(&reference).unwrap()}));
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh", "read_block":512, "cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "compares a 600-second ALAC stream with the movie atom after media data"]
fn portable_alac_tail_movie_parity() {
    use platinum_engine::portable::audio as portable;
    use std::io::{Read, Seek, SeekFrom};
    use std::time::{SystemTime, UNIX_EPOCH};
    fn copy_new(source: &Path, target: &Path) {
        let mut input = fs::File::open(source).unwrap();
        let mut output = OpenOptions::new().write(true).create_new(true).open(target).unwrap();
        std::io::copy(&mut input, &mut output).unwrap();
    }
    fn movie_clock(path: &Path) -> (u32, u64) {
        let mut file = fs::File::open(path).unwrap();
        let length = file.metadata().unwrap().len(); let mut offset = 0u64;
        while offset < length {
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut header = [0u8; 8]; file.read_exact(&mut header).unwrap();
            let size = u64::from(u32::from_be_bytes(header[..4].try_into().unwrap()));
            assert!(size >= 8 && size <= length - offset);
            if &header[4..] == b"moov" {
                file.read_exact(&mut header).unwrap(); assert_eq!(&header[4..], b"mvhd");
                let mut times = [0u8; 8]; file.read_exact(&mut times).unwrap();
                return (u32::from_be_bytes(times[4..8].try_into().unwrap()), offset);
            }
            offset += size;
        }
        panic!("missing movie atom");
    }
    fn now_1904() -> u32 {
        (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 2_082_844_800) as u32
    }
    let dir = artifact_dir("alac-tail-movie-parity");
    let reference = dir.parent().unwrap().parent().unwrap().join("alac-01/native-600s-01.m4a");
    let output = dir.join("portable.m4a");
    let mut writer = portable::Writer::create(&output, portable::WriterConfig {
        format: portable::OutputFormat::Alac, sample_rate: 44_100.0, channels: 1, bit_depth: 16
    }).unwrap();
    let frames = 26_460_000usize;
    let mut input = vec![0.0f32; 512];
    for start in (0..frames).step_by(512) {
        let count = (frames - start).min(512);
        for (i, sample) in input[..count].iter_mut().enumerate() {
            *sample = ((((start + i) % 101 * 17) % 101) as i32 - 50) as f32 / 128.0;
        }
        writer.write(&input[..count]).unwrap();
    }
    let before = now_1904(); writer.finish().unwrap(); let after = now_1904();
    let (clock, movie_offset) = movie_clock(&output);
    let ordinary_reference = dir.join("ordinary-native.m4a"); copy_new(&reference, &ordinary_reference);
    let native_fields = platinum_engine::apple_codec::set_mp4_header_times(&ordinary_reference, clock).unwrap();
    let ordinary_expected = fs::read(&ordinary_reference).unwrap(); let ordinary_actual = fs::read(&output).unwrap();
    let ordinary_diff = ordinary_expected.iter().zip(&ordinary_actual).filter(|(a,b)| a != b).count() +
        ordinary_expected.len().abs_diff(ordinary_actual.len());
    let ordinary_first = ordinary_expected.iter().zip(&ordinary_actual).position(|(a,b)| a != b);
    let controlled = dir.join("controlled-portable.m4a"); copy_new(&output, &controlled);
    let controlled_native_fields = platinum_engine::apple_codec::set_mp4_header_times(&ordinary_reference, 0x1234_5678).unwrap();
    let portable_fields = portable::set_mp4_header_times(&controlled, 0x1234_5678).unwrap();
    let controlled_expected = fs::read(&ordinary_reference).unwrap(); let controlled_actual = fs::read(&controlled).unwrap();
    let controlled_diff = controlled_expected.iter().zip(&controlled_actual).filter(|(a,b)| a != b).count() +
        controlled_expected.len().abs_diff(controlled_actual.len());
    let controlled_first = controlled_expected.iter().zip(&controlled_actual).position(|(a,b)| a != b);
    let mut native = Reader::open(&reference, None).unwrap();
    let mut ours = portable::Reader::open(&reference, None).unwrap();
    let mut a = vec![0.0f32; 4096]; let mut b = vec![0.0f32; 4096];
    let mut read_frames = 0usize; let mut read_mismatches = 0usize; let mut first_read = None;
    loop {
        let na = native.read(&mut a).unwrap(); let nb = ours.read(&mut b).unwrap();
        assert_eq!(na, nb); if na == 0 { break; }
        for i in 0..na {
            if a[i].to_bits() != b[i].to_bits() {
                read_mismatches += 1;
                if first_read.is_none() { first_read = Some(json!({"frame":read_frames+i,
                    "native_bits":format!("{:08x}",a[i].to_bits()),"portable_bits":format!("{:08x}",b[i].to_bits())})); }
            }
        }
        read_frames += na;
    }
    native.close().unwrap(); ours.close().unwrap();
    let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","frames":frames,
        "native":reference,"native_sha256":platinum_engine::mp3::file_sha256(&reference).unwrap(),
        "ordinary_portable":output,"ordinary_clock":clock,"clock_before":before,"clock_after":after,
        "movie_offset":movie_offset,"file_bytes":ordinary_actual.len(),"native_patch_fields":native_fields,
        "ordinary_differing_bytes":ordinary_diff,"ordinary_first_difference":ordinary_first,
        "controlled_native_fields":controlled_native_fields,"controlled_portable_fields":portable_fields,
        "controlled_differing_bytes":controlled_diff,"controlled_first_difference":controlled_first,
        "read_frames":read_frames,"read_mismatches":read_mismatches,"first_read_mismatch":first_read});
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
    assert!(movie_offset > 20_480 && (before..=after).contains(&clock));
    assert_eq!((ordinary_diff, controlled_diff, read_mismatches), (0,0,0));
    assert_eq!(read_frames, frames);
    assert_eq!((native_fields, controlled_native_fields, portable_fields), (6,6,6));
}

#[test]
#[ignore = "records fail-closed codec and sample-rate conversion behavior"]
fn portable_codec_refusals() {
    use platinum_engine::portable::audio::{CodecError, OutputFormat as PFormat, Reader as PReader, Writer as PWriter, WriterConfig as PConfig};
    let dir = artifact_dir("refusals");
    let root = dir.parent().unwrap();
    let pcm = root.join("native-input/wav-16-2-1027.wav");
    let result = PReader::open(&pcm, Some(48_000.0));
    assert!(matches!(result, Err(CodecError::UnsupportedSampleRateConversion { source_rate: 44_100.0, client_rate: 48_000.0 })));
    let mut reader = PReader::open(&pcm, None).unwrap();
    assert!(matches!(reader.set_stereo_planar(48_000.0),
        Err(CodecError::UnsupportedSampleRateConversion { source_rate: 44_100.0, client_rate: 48_000.0 })));
    assert_eq!(reader.client_format().channels_per_frame, 2);
    assert_eq!(reader.client_format().format_flags, 9);
    let mp3 = dir.join("mp3-header");
    write_new(&mp3, b"ID3\0\0\0\0\0\0\0\0\0");
    // An ID3-only file is an MP3 open: refused by the exact reader with AudioFile's status, not by a codec gate.
    assert!(matches!(PReader::open(&mp3, None), Err(CodecError::Native { operation: "ExtAudioFileOpenURL", .. })));
    let aac = dir.join("aac-header");
    // A header AudioFile sniffs as ADTS (frame length 13, sampling index 4); a name without an extension leaves the choice to the content.
    write_new(&aac, &[0xff, 0xf1, 0x50, 0x80, 0x01, 0xa0, 0, 0, 0, 0, 0, 0]);
    assert!(matches!(PReader::open(&aac, None), Err(CodecError::UnsupportedCodec { codec: "AAC", operation: "decoding", .. })));
    let aac_m4a = root.parent().unwrap().parent().unwrap().join("apple-codecs/aac-writer-warning-matrix-01/aac-44100-2.m4a");
    assert!(matches!(PReader::open(&aac_m4a, None), Err(CodecError::UnsupportedCodec { codec: "AAC/MP4A", operation: "decoding", .. })));
    let alac = root.join("native-alac/alac-16-2-44100-513.m4a");
    let alac_reader = PReader::open(&alac, None).unwrap();
    assert_eq!(alac_reader.source_format().format_id, u32::from_be_bytes(*b"alac"));
    assert!(matches!(PReader::open(&alac, Some(48_000.0)),
        Err(CodecError::UnsupportedSampleRateConversion { source_rate: 44_100.0, client_rate: 48_000.0 })));
    let config = PConfig { format: PFormat::Aac, sample_rate: 44_100.0, channels: 2, bit_depth: 16 };
    assert!(matches!(PWriter::create(dir.join("no-aac.m4a"), config),
        Err(CodecError::UnsupportedCodec { codec: "AAC", operation: "encoding", .. })));
    let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","cases":[
        "same-rate PCM accepted","44.1->48 kHz open refused","44.1->48 kHz planar setter refused without changing client format",
        "MP3 ID3 refused","AAC ADTS refused","AAC M4A refused","ALAC same-rate accepted and SRC refused","AAC writer refused"
    ]});
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
}

#[test]
#[ignore = "records AudioToolbox ASBDs for non-byte-aligned PCM inputs"]
fn capture_native_pcm_depth_descriptions() {
    let dir = artifact_dir("native-depth-descriptions");
    let source_dir = dir.parent().unwrap().join("native-input-depths");
    let mut rows = Vec::new();
    for format in ["wav", "aiff"] {
        for bits in 1..=32 {
            let path = source_dir.join(format!("{format}-{bits}.{format}"));
            let reader = Reader::open(&path, None).unwrap();
            let asbd = reader.source_format();
            rows.push(json!({"format":format,"declared_bits":bits,"source":format!("{asbd:?}"),
                "format_flags":asbd.format_flags,"bytes_per_frame":asbd.bytes_per_frame,
                "bits_per_channel":asbd.bits_per_channel,"length":reader.file_length_frames().unwrap(),
                "bitrate":reader.bitrate().ok()}));
            reader.close().unwrap();
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh", "cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "captures native interleaved-float CAF intermediate files"]
fn capture_native_caf_intermediate() {
    use platinum_engine::apple_codec::StreamDescription;
    use std::ffi::c_void;
    use std::ptr;

    #[repr(C)]
    struct Buffer { channels: u32, bytes: u32, data: *mut c_void }
    #[repr(C)]
    struct BufferList { count: u32, buffer: Buffer }
    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn ExtAudioFileCreateWithURL(url: *const c_void, kind: u32, format: *const StreamDescription,
            layout: *const c_void, flags: u32, result: *mut *mut c_void) -> i32;
        fn ExtAudioFileSetProperty(file: *mut c_void, property: u32, size: u32, value: *const c_void) -> i32;
        fn ExtAudioFileWrite(file: *mut c_void, frames: u32, data: *const BufferList) -> i32;
        fn ExtAudioFileDispose(file: *mut c_void) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(allocator: *const c_void, bytes: *const u8, length: isize, directory: u8) -> *const c_void;
        fn CFRelease(object: *const c_void);
    }
    let dir = artifact_dir("native-caf");
    let mut rows = Vec::new();
    for rate in [44_100u32, 48_000, 96_000] {
        for channels in [1u32, 2, 6] {
            for frames in [0usize, 1, 511, 512, 513, 8193] {
                let label = format!("{rate}-{channels}-{frames}");
                let path = dir.join(format!("{label}.caf"));
                let source = dir.join(format!("{label}.f32le"));
                let mut state = 0x63c7_a459u32;
                let edges = [0.0, -0.0, 1.0, -1.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN,
                    f32::from_bits(1), f32::from_bits(0x8000_0001)];
                let samples: Vec<f32> = (0..frames * channels as usize).map(|i|
                    if i < edges.len() { edges[i] } else { f32::from_bits(next_random(&mut state)) }).collect();
                let raw: Vec<u8> = samples.iter().flat_map(|f| f.to_bits().to_le_bytes()).collect();
                write_new(&source, &raw);
                let format = StreamDescription { sample_rate: rate as f64, format_id: u32::from_be_bytes(*b"lpcm"),
                    format_flags: 9, bytes_per_packet: channels * 4, frames_per_packet: 1,
                    bytes_per_frame: channels * 4, channels_per_frame: channels, bits_per_channel: 32, reserved: 0 };
                let bytes = path.as_os_str().as_encoded_bytes();
                let url = unsafe { CFURLCreateFromFileSystemRepresentation(ptr::null(), bytes.as_ptr(), bytes.len() as isize, 0) };
                assert!(!url.is_null());
                let mut file = ptr::null_mut();
                let status = unsafe { ExtAudioFileCreateWithURL(url, u32::from_be_bytes(*b"caff"), &format, ptr::null(), 0, &mut file) };
                unsafe { CFRelease(url); }
                assert_eq!(status, 0);
                assert_eq!(unsafe { ExtAudioFileSetProperty(file, u32::from_be_bytes(*b"cfmt"),
                    std::mem::size_of_val(&format) as u32, (&format as *const StreamDescription).cast()) }, 0);
                for block in samples.chunks(512 * channels as usize) {
                    let list = BufferList { count: 1, buffer: Buffer { channels, bytes: (block.len() * 4) as u32,
                        data: block.as_ptr().cast_mut().cast() } };
                    assert_eq!(unsafe { ExtAudioFileWrite(file, (block.len() / channels as usize) as u32, &list) }, 0);
                }
                assert_eq!(unsafe { ExtAudioFileDispose(file) }, 0);
                rows.push(json!({"path":path,"source":source,"rate":rate,"channels":channels,"frames":frames,
                    "sha256":platinum_engine::mp3::file_sha256(&path).unwrap()}));
            }
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh", "seed":"0x63c7a459", "write_block_frames":512, "cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "compares native and portable CAF sample bits, frame counts, and read partitions"]
fn portable_caf_intermediate_parity() {
    use platinum_engine::portable::audio as portable;
    let dir = artifact_dir("caf-parity");
    let native_dir = dir.parent().unwrap().join("native-caf");
    let mut rows = Vec::new();
    let mut failures = 0usize;
    let mut bits_compared = 0u64;
    for rate in [44_100u32, 48_000, 96_000] {
        for channels in [1u32, 2, 6] {
            for frames in [0usize, 1, 511, 512, 513, 8193] {
                let label = format!("{rate}-{channels}-{frames}");
                let native_path = native_dir.join(format!("{label}.caf"));
                let portable_path = dir.join(format!("{label}.caf"));
                let raw = fs::read(native_dir.join(format!("{label}.f32le"))).unwrap();
                let samples: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_bits(u32::from_le_bytes(b.try_into().unwrap()))).collect();
                let mut writer = portable::CafWriter::new(&portable_path, rate as f64, channels as usize).unwrap();
                for block in samples.chunks(512 * channels as usize) { writer.write(block).unwrap(); }
                writer.finish().unwrap();
                for path in [&native_path, &portable_path] {
                    for planar in [false, true] {
                        for block in [1usize, 257, 512, 4096] {
                            let mut reference = Reader::open(path, None).unwrap();
                            let mut ours = portable::Reader::open(path, None).unwrap();
                            let desc = reference.source_format();
                            let ours_desc = ours.source_format();
                            let metadata_equal = desc.sample_rate == ours_desc.sample_rate &&
                                desc.format_id == ours_desc.format_id && desc.format_flags == ours_desc.format_flags &&
                                desc.bytes_per_packet == ours_desc.bytes_per_packet &&
                                desc.frames_per_packet == ours_desc.frames_per_packet && desc.bytes_per_frame == ours_desc.bytes_per_frame &&
                                desc.channels_per_frame == ours_desc.channels_per_frame && desc.bits_per_channel == ours_desc.bits_per_channel &&
                                reference.file_length_frames().unwrap() == ours.file_length_frames().unwrap();
                            if !metadata_equal { failures += 1; }
                            if planar {
                                reference.set_stereo_planar(rate as f64).unwrap();
                                ours.set_stereo_planar(rate as f64).unwrap();
                            }
                            let lanes = if planar { 1 } else { channels as usize };
                            let mut a = vec![13.0f32; block * lanes]; let mut b = a.clone();
                            let mut ar = vec![-17.0f32; block]; let mut br = ar.clone();
                            let mut total = 0usize; let mut mismatch = 0u64; let mut examples = Vec::new();
                            loop {
                                let (na, nb) = if planar {
                                    (reference.read_stereo_planar(&mut a, &mut ar).unwrap(), ours.read_stereo_planar(&mut b, &mut br).unwrap())
                                } else { (reference.read(&mut a).unwrap(), ours.read(&mut b).unwrap()) };
                                if na != nb {
                                    failures += 1;
                                    examples.push(json!({"at_frame":total,"native_frames":na,"portable_frames":nb}));
                                    break;
                                }
                                if na == 0 { break; }
                                for i in 0..na * lanes {
                                    bits_compared += 1;
                                    if a[i].to_bits() != b[i].to_bits() {
                                        mismatch += 1;
                                        if examples.len() < 8 { examples.push(json!({"frame":total,"index":i,
                                            "native_bits":format!("{:08x}",a[i].to_bits()),"portable_bits":format!("{:08x}",b[i].to_bits())})); }
                                    }
                                }
                                if planar {
                                    for i in 0..na {
                                        bits_compared += 1;
                                        if ar[i].to_bits() != br[i].to_bits() {
                                            mismatch += 1;
                                            if examples.len() < 8 { examples.push(json!({"frame":total,"right_index":i,
                                                "native_bits":format!("{:08x}",ar[i].to_bits()),"portable_bits":format!("{:08x}",br[i].to_bits())})); }
                                        }
                                    }
                                }
                                total += na;
                            }
                            if mismatch != 0 || total != frames { failures += 1; }
                            reference.close().unwrap(); ours.close().unwrap();
                            rows.push(json!({"case":label,"path":path,"planar":planar,"block":block,"frames":total,
                                "metadata_equal":metadata_equal,"native_asbd":format!("{desc:?}"),
                                "portable_asbd":format!("{ours_desc:?}"),"mismatched_samples":mismatch,"examples":examples}));
                        }
                    }
                }
            }
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","sample_bits_compared":bits_compared,"failures":failures,"cases":rows
    })).unwrap().as_bytes());
    assert_eq!(failures, 0, "CAF intermediate parity failures; see {}", dir.join("receipt.json").display());
}

#[test]
#[ignore = "captures AudioConverter channel mappings and MP3 PCM conversion"]
fn capture_native_channel_conversions() {
    use platinum_engine::apple_codec::{Pcm16StereoConverter, StreamDescription};
    use std::ffi::c_void;
    use std::ptr;
    #[repr(C)]
    struct Buffer { channels: u32, bytes: u32, data: *mut c_void }
    #[repr(C)]
    struct TwoBuffers { count: u32, buffers: [Buffer; 2] }
    #[repr(C)]
    struct OneBuffer { count: u32, buffers: [Buffer; 1] }
    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioConverterNew(input: *const StreamDescription, output: *const StreamDescription, converter: *mut *mut c_void) -> i32;
        fn AudioConverterConvertComplexBuffer(converter: *mut c_void, frames: u32, input: *const TwoBuffers, output: *mut OneBuffer) -> i32;
        fn AudioConverterGetProperty(converter: *mut c_void, property: u32, size: *mut u32, value: *mut c_void) -> i32;
        fn AudioConverterDispose(converter: *mut c_void) -> i32;
    }
    let dir = artifact_dir("native-channel-conversion");
    let frames = 4099usize;
    let mut state = 0x11cf_63a9;
    let edge = [0.0f32, -0.0, 1.0, -1.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN,
        f32::from_bits(1), f32::from_bits(0x8000_0001), -0.5 / 32_768.0, 0.5 / 32_768.0];
    let mut planar = Vec::new();
    for i in 0..frames * 2 {
        planar.push(if i < edge.len() { edge[i] } else if i % 2 == 0 {
            f32::from_bits(next_random(&mut state))
        } else { ((next_random(&mut state) >> 8) as f32 / 8_388_608.0) - 1.0 });
    }
    let source: Vec<u8> = planar.iter().flat_map(|x| x.to_bits().to_le_bytes()).collect();
    write_new(&dir.join("source.f32le"), &source);
    let mut rows = Vec::new();
    for channels in [1u32, 2, 6] {
        for block in [1usize, 257, 512, 4096] {
            let input = StreamDescription { sample_rate: 44_100.0, format_id: u32::from_be_bytes(*b"lpcm"),
                format_flags: 0x29, bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4,
                channels_per_frame: 2, bits_per_channel: 32, reserved: 0 };
            let output = StreamDescription { sample_rate: 44_100.0, format_id: u32::from_be_bytes(*b"lpcm"),
                format_flags: 9, bytes_per_packet: channels * 4, frames_per_packet: 1, bytes_per_frame: channels * 4,
                channels_per_frame: channels, bits_per_channel: 32, reserved: 0 };
            let mut converter = ptr::null_mut();
            assert_eq!(unsafe { AudioConverterNew(&input, &output, &mut converter) }, 0);
            let mut dither = u32::MAX; let mut size = 4u32;
            let dither_status = unsafe { AudioConverterGetProperty(converter, u32::from_be_bytes(*b"dith"), &mut size, (&mut dither as *mut u32).cast()) };
            let mut result = Vec::new();
            for offset in (0..frames).step_by(block) {
                let n = (frames - offset).min(block);
                let list = TwoBuffers { count: 2, buffers: [
                    Buffer { channels: 1, bytes: (n * 4) as u32, data: planar[offset..].as_ptr().cast_mut().cast() },
                    Buffer { channels: 1, bytes: (n * 4) as u32, data: planar[frames + offset..].as_ptr().cast_mut().cast() },
                ] };
                let mut out = vec![13.0f32; n * channels as usize];
                let mut dst = OneBuffer { count: 1, buffers: [Buffer { channels, bytes: (out.len() * 4) as u32,
                    data: out.as_mut_ptr().cast() }] };
                assert_eq!(unsafe { AudioConverterConvertComplexBuffer(converter, n as u32, &list, &mut dst) }, 0);
                for sample in out { result.extend_from_slice(&sample.to_bits().to_le_bytes()); }
            }
            assert_eq!(unsafe { AudioConverterDispose(converter) }, 0);
            let path = dir.join(format!("planar-to-{channels}-{block}.f32le"));
            write_new(&path, &result);
            rows.push(json!({"operation":"planar_stereo_to_interleaved","channels":channels,"block":block,"output":path,
                "dither_status":dither_status,"dither_value":dither}));
        }
    }
    for channels in [1u32, 2, 6] {
        let mut state = 0x29e8_aad1;
        let input: Vec<f32> = (0..frames * channels as usize).map(|i|
            if i < edge.len() { edge[i] } else if i % 2 == 0 { f32::from_bits(next_random(&mut state)) }
            else { ((next_random(&mut state) >> 8) as f32 / 8_388_608.0) - 1.0 }).collect();
        let raw: Vec<u8> = input.iter().flat_map(|x| x.to_bits().to_le_bytes()).collect();
        write_new(&dir.join(format!("mp3-input-{channels}.f32le")), &raw);
        for block in [1usize, 257, 512, 4096] {
            let mut converter = Pcm16StereoConverter::new(44_100.0, channels).unwrap();
            let mut result = Vec::new();
            for part in input.chunks(block * channels as usize) {
                let mut out = vec![13i16; (part.len() / channels as usize) * 2];
                assert_eq!(converter.convert(part, &mut out).unwrap(), part.len() / channels as usize);
                for value in out { result.extend_from_slice(&value.to_le_bytes()); }
            }
            converter.close().unwrap();
            let path = dir.join(format!("mp3-output-{channels}-{block}.s16le"));
            write_new(&path, &result);
            rows.push(json!({"operation":"interleaved_to_mp3_pcm","channels":channels,"block":block,"output":path}));
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","frames":frames,"seeds":["0x11cf63a9","0x29e8aad1"],"cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "records the MP3 PCM converter's effective dither property"]
fn capture_native_mp3_pcm_dither() {
    use platinum_engine::apple_codec::StreamDescription;
    use std::ffi::c_void;
    use std::ptr;
    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioConverterNew(input: *const StreamDescription, output: *const StreamDescription, converter: *mut *mut c_void) -> i32;
        fn AudioConverterGetProperty(converter: *mut c_void, property: u32, size: *mut u32, value: *mut c_void) -> i32;
        fn AudioConverterDispose(converter: *mut c_void) -> i32;
    }
    let dir = artifact_dir("mp3-pcm-dither");
    let mut rows = Vec::new();
    for channels in [1u32, 2, 6] {
        let input = StreamDescription { sample_rate: 44_100.0, format_id: u32::from_be_bytes(*b"lpcm"),
            format_flags: 9, bytes_per_packet: channels * 4, frames_per_packet: 1, bytes_per_frame: channels * 4,
            channels_per_frame: channels, bits_per_channel: 32, reserved: 0 };
        let output = StreamDescription { sample_rate: 44_100.0, format_id: u32::from_be_bytes(*b"lpcm"),
            format_flags: 12, bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4,
            channels_per_frame: 2, bits_per_channel: 16, reserved: 0 };
        let mut converter = ptr::null_mut();
        assert_eq!(unsafe { AudioConverterNew(&input, &output, &mut converter) }, 0);
        let mut value = u32::MAX; let mut size = 4u32;
        let status = unsafe { AudioConverterGetProperty(converter, u32::from_be_bytes(*b"dith"), &mut size, (&mut value as *mut u32).cast()) };
        assert_eq!(unsafe { AudioConverterDispose(converter) }, 0);
        rows.push(json!({"channels":channels,"status":status,"value":value,"size":size}));
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "records native channel-map coverage for engine channel counts"]
fn capture_native_channel_count_map() {
    use platinum_engine::apple_codec::{Pcm16StereoConverter, StreamDescription};
    use std::ffi::c_void;
    use std::ptr;
    #[repr(C)]
    struct Buffer { channels: u32, bytes: u32, data: *mut c_void }
    #[repr(C)]
    struct TwoBuffers { count: u32, buffers: [Buffer; 2] }
    #[repr(C)]
    struct OneBuffer { count: u32, buffers: [Buffer; 1] }
    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioConverterNew(input: *const StreamDescription, output: *const StreamDescription, converter: *mut *mut c_void) -> i32;
        fn AudioConverterConvertComplexBuffer(converter: *mut c_void, frames: u32, input: *const TwoBuffers, output: *mut OneBuffer) -> i32;
        fn AudioConverterDispose(converter: *mut c_void) -> i32;
    }
    let dir = artifact_dir("channel-counts");
    let input = StreamDescription { sample_rate: 44_100.0, format_id: u32::from_be_bytes(*b"lpcm"),
        format_flags: 0x29, bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4,
        channels_per_frame: 2, bits_per_channel: 32, reserved: 0 };
    let left = [0.25f32; 4]; let right = [-0.5f32; 4];
    let list = TwoBuffers { count: 2, buffers: [
        Buffer { channels: 1, bytes: 16, data: left.as_ptr().cast_mut().cast() },
        Buffer { channels: 1, bytes: 16, data: right.as_ptr().cast_mut().cast() },
    ] };
    let mut rows = Vec::new();
    for channels in 1u32..=64 {
        let output = StreamDescription { sample_rate: 44_100.0, format_id: u32::from_be_bytes(*b"lpcm"),
            format_flags: 9, bytes_per_packet: channels * 4, frames_per_packet: 1, bytes_per_frame: channels * 4,
            channels_per_frame: channels, bits_per_channel: 32, reserved: 0 };
        let mut converter = ptr::null_mut();
        let create = unsafe { AudioConverterNew(&input, &output, &mut converter) };
        let mut out = vec![13.0f32; channels as usize * 4];
        let mut status = None;
        if create == 0 && !converter.is_null() {
            let mut dst = OneBuffer { count: 1, buffers: [Buffer { channels, bytes: (out.len() * 4) as u32, data: out.as_mut_ptr().cast() }] };
            status = Some(unsafe { AudioConverterConvertComplexBuffer(converter, 4, &list, &mut dst) });
            assert_eq!(unsafe { AudioConverterDispose(converter) }, 0);
        }
        let mp3 = match Pcm16StereoConverter::new(44_100.0, channels) {
            Ok(mut converter) => {
                let mut samples = vec![0.25f32; channels as usize * 4];
                if channels > 1 { for frame in samples.chunks_exact_mut(channels as usize) { frame[1] = -0.5; } }
                let mut converted = [0i16; 8];
                let result = converter.convert(&samples, &mut converted);
                converter.close().unwrap();
                json!({"result":result.map_err(|e| e.to_string()),"output":converted})
            }
            Err(error) => json!({"error":error.to_string()}),
        };
        rows.push(json!({"channels":channels,"create":create,"convert":status,"first_frame":&out[..channels as usize],"mp3":mp3}));
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "compares portable channel converters against recorded AudioConverter output"]
fn portable_channel_converter_parity() {
    use platinum_engine::portable::audio::{ChannelConverter, Pcm16StereoConverter};
    let dir = artifact_dir("channel-parity");
    let native = dir.parent().unwrap().join("native-channel-conversion-02");
    let load = |path: &Path| -> Vec<f32> {
        fs::read(path).unwrap().chunks_exact(4).map(|b| f32::from_bits(u32::from_le_bytes(b.try_into().unwrap()))).collect()
    };
    let planar = load(&native.join("source.f32le"));
    let frames = planar.len() / 2;
    let (left, right) = planar.split_at(frames);
    let mut rows = Vec::new();
    let mut failures = 0usize;
    let mut samples_compared = 0usize;
    for channels in [1usize, 2, 6] {
        for block in [1usize, 257, 512, 4096] {
            let mut converter = ChannelConverter::new(44_100.0, channels).unwrap();
            let mut actual = Vec::new();
            for offset in (0..frames).step_by(block) {
                let count = (frames - offset).min(block);
                let mut out = vec![13.0f32; count * channels];
                converter.convert(&left[offset..offset + count], &right[offset..offset + count], &mut out).unwrap();
                for value in out { actual.extend_from_slice(&value.to_bits().to_le_bytes()); }
            }
            converter.dispose().unwrap();
            let reference = fs::read(native.join(format!("planar-to-{channels}-{block}.f32le"))).unwrap();
            let mismatches = reference.iter().zip(&actual).filter(|(a,b)| a != b).count() + reference.len().abs_diff(actual.len());
            if mismatches != 0 { failures += 1; }
            samples_compared += reference.len() / 4;
            rows.push(json!({"operation":"planar_stereo_to_interleaved","channels":channels,"block":block,
                "differing_bytes":mismatches,"first_differing_byte":reference.iter().zip(&actual).position(|(a,b)| a!=b)}));

            let source = load(&native.join(format!("mp3-input-{channels}.f32le")));
            let mut converter = Pcm16StereoConverter::new(44_100.0, channels as u32).unwrap();
            let mut actual = Vec::new();
            for part in source.chunks(block * channels) {
                let mut out = vec![13i16; part.len() / channels * 2];
                assert_eq!(converter.convert(part, &mut out).unwrap(), part.len() / channels);
                for value in out { actual.extend_from_slice(&value.to_le_bytes()); }
            }
            converter.close().unwrap();
            let reference = fs::read(native.join(format!("mp3-output-{channels}-{block}.s16le"))).unwrap();
            let mismatches = reference.iter().zip(&actual).filter(|(a,b)| a != b).count() + reference.len().abs_diff(actual.len());
            if mismatches != 0 { failures += 1; }
            samples_compared += reference.len() / 2;
            rows.push(json!({"operation":"interleaved_to_mp3_pcm","channels":channels,"block":block,
                "differing_bytes":mismatches,"first_differing_byte":reference.iter().zip(&actual).position(|(a,b)| a!=b)}));
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","samples_compared":samples_compared,"failures":failures,"cases":rows
    })).unwrap().as_bytes());
    assert_eq!(failures, 0, "channel conversion mismatch; see {}", dir.join("receipt.json").display());
}

#[test]
#[ignore = "records AudioToolbox AIFF-C stream descriptions"]
fn capture_native_aifc_descriptions() {
    let dir = artifact_dir("native-aifc-descriptions");
    let source = dir.parent().unwrap().join("native-aifc/receipt.json");
    let source: serde_json::Value = serde_json::from_slice(&fs::read(source).unwrap()).unwrap();
    let mut rows = Vec::new();
    for case in source["cases"].as_array().unwrap() {
        if case["exit"].as_i64() != Some(0) { continue; }
        let path = PathBuf::from(case["args"][3].as_str().unwrap());
        let reader = Reader::open(&path, None).unwrap();
        let asbd = reader.source_format();
        rows.push(json!({"codec":case["codec"],"source":path,"asbd":format!("{asbd:?}"),"sample_rate":asbd.sample_rate,
            "format_id":asbd.format_id,"format_flags":asbd.format_flags,"bytes_per_packet":asbd.bytes_per_packet,
            "frames_per_packet":asbd.frames_per_packet,"bytes_per_frame":asbd.bytes_per_frame,
            "channels_per_frame":asbd.channels_per_frame,"bits_per_channel":asbd.bits_per_channel,
            "length":reader.file_length_frames().unwrap(),"bitrate":reader.bitrate().ok()}));
        reader.close().unwrap();
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "compares AIFF-C PCM, G.711 and IMA4 with native AudioToolbox"]
fn portable_aifc_and_g711_parity() {
    use platinum_engine::portable::audio as portable;
    let dir = artifact_dir("aifc-g711-parity");
    let root = dir.parent().unwrap();
    let mut sources = Vec::new();
    for folder in ["native-aifc", "native-g711", "native-g711-lengths", "native-ima4", "native-ima4-indices", "native-ima4-partial",
        "native-wav-extensible", "native-wav-adpcm", "native-wav-adpcm-matrix", "native-wav-adpcm-partial",
        "native-ima-wav-alignment", "native-ms-adpcm-extreme", "native-wav-gsm", "native-wav-gsm-partial"] {
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(root.join(folder).join("receipt.json")).unwrap()).unwrap();
        for case in receipt["cases"].as_array().unwrap() {
            if case["exit"].as_i64() == Some(0) { sources.push(PathBuf::from(case["args"][3].as_str().unwrap())); }
        }
    }
    let mut rows = Vec::new(); let mut failures = 0usize; let mut compared = 0u64;
    for path in sources {
        for planar in [false, true] {
            for block in [1usize, 257, 512, 4096] {
                let mut native = Reader::open(&path, None).unwrap();
                let mut ours = match portable::Reader::open(&path, None) {
                    Ok(reader) => reader,
                    Err(error) => {
                        failures += 1;
                        rows.push(json!({"path":path,"planar":planar,"block":block,"open_error":error.to_string()}));
                        continue;
                    }
                };
                let a = native.source_format(); let b = ours.source_format();
                let metadata_equal = a.sample_rate == b.sample_rate && a.format_id == b.format_id && a.format_flags == b.format_flags &&
                    a.bytes_per_packet == b.bytes_per_packet && a.frames_per_packet == b.frames_per_packet &&
                    a.bytes_per_frame == b.bytes_per_frame && a.channels_per_frame == b.channels_per_frame &&
                    a.bits_per_channel == b.bits_per_channel && native.file_length_frames().unwrap() == ours.file_length_frames().unwrap() &&
                    native.bitrate().ok() == ours.bitrate().ok();
                if !metadata_equal { failures += 1; }
                if planar {
                    native.set_stereo_planar(a.sample_rate).unwrap();
                    ours.set_stereo_planar(b.sample_rate).unwrap();
                }
                let channels = if planar { 1 } else { a.channels_per_frame as usize };
                let mut nb = vec![13.0f32; block * channels]; let mut pb = nb.clone();
                let mut nr = vec![-17.0f32; block]; let mut pr = nr.clone();
                let mut frames = 0usize; let mut mismatch = 0u64; let mut examples = Vec::new();
                loop {
                    let (n,p) = if planar {
                        (native.read_stereo_planar(&mut nb, &mut nr).unwrap(), ours.read_stereo_planar(&mut pb, &mut pr).unwrap())
                    } else { (native.read(&mut nb).unwrap(), ours.read(&mut pb).unwrap()) };
                    if n != p {
                        failures += 1;
                        examples.push(json!({"at_frame":frames,"native_frames":n,"portable_frames":p}));
                        break;
                    }
                    if n == 0 { break; }
                    for i in 0..n * channels {
                        compared += 1;
                        if nb[i].to_bits() != pb[i].to_bits() {
                            mismatch += 1;
                            if examples.len() < 8 { examples.push(json!({"at_frame":frames,"index":i,
                                "native_bits":format!("{:08x}",nb[i].to_bits()),"portable_bits":format!("{:08x}",pb[i].to_bits())})); }
                        }
                    }
                    if planar {
                        for i in 0..n {
                            compared += 1;
                            if nr[i].to_bits() != pr[i].to_bits() {
                                mismatch += 1;
                                if examples.len() < 8 { examples.push(json!({"at_frame":frames,"right_index":i,
                                    "native_bits":format!("{:08x}",nr[i].to_bits()),"portable_bits":format!("{:08x}",pr[i].to_bits())})); }
                            }
                        }
                    }
                    frames += n;
                }
                if mismatch != 0 { failures += 1; }
                native.close().unwrap(); ours.close().unwrap();
                rows.push(json!({"path":path,"planar":planar,"block":block,"frames":frames,"metadata_equal":metadata_equal,
                    "native_asbd":format!("{a:?}"),"portable_asbd":format!("{b:?}"),"mismatched_samples":mismatch,"examples":examples}));
            }
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","compared_sample_bits":compared,"failures":failures,"cases":rows
    })).unwrap().as_bytes());
    assert_eq!(failures, 0, "AIFF-C/G.711 parity mismatch; see {}", dir.join("receipt.json").display());
}

#[test]
#[ignore = "records WAVE extensible ASBDs and stereo-planar outputs"]
fn capture_native_wav_extensible() {
    let dir = artifact_dir("native-wav-extensible-planar");
    let root = dir.parent().unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&fs::read(root.join("native-wav-extensible/receipt.json")).unwrap()).unwrap();
    let mut rows = Vec::new();
    for case in receipt["cases"].as_array().unwrap() {
        if case["exit"].as_i64() != Some(0) { continue; }
        let path = PathBuf::from(case["args"][3].as_str().unwrap());
        let mut reader = Reader::open(&path, None).unwrap();
        let format = reader.source_format(); let bitrate = reader.bitrate().ok(); let length = reader.file_length_frames().unwrap();
        reader.set_stereo_planar(format.sample_rate).unwrap();
        let mut left = [13.0f32; 257]; let mut right = [-17.0f32; 257]; let mut output = Vec::new();
        loop {
            let n = reader.read_stereo_planar(&mut left, &mut right).unwrap();
            if n == 0 { break; }
            for i in 0..n { output.extend_from_slice(&left[i].to_bits().to_le_bytes()); output.extend_from_slice(&right[i].to_bits().to_le_bytes()); }
        }
        reader.close().unwrap();
        let name = path.file_stem().unwrap().to_str().unwrap();
        let result = dir.join(format!("{name}.stereo.f32le"));
        write_new(&result, &output);
        rows.push(json!({"source":path,"output":result,"asbd":format!("{format:?}"),"format_flags":format.format_flags,
            "bits_per_channel":format.bits_per_channel,"bytes_per_frame":format.bytes_per_frame,
            "bitrate":bitrate,"length":length}));
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "records IMA4 file-length property for partial final packets"]
fn capture_native_ima4_lengths() {
    let dir = artifact_dir("native-ima4-lengths");
    let input: serde_json::Value = serde_json::from_slice(&fs::read(dir.parent().unwrap().join("native-ima4-partial/receipt.json")).unwrap()).unwrap();
    let mut rows = Vec::new();
    for case in input["cases"].as_array().unwrap() {
        let path = PathBuf::from(case["args"][3].as_str().unwrap());
        let reader = Reader::open(&path, None).unwrap();
        rows.push(json!({"source":path,"declared_frames":case["declared_frames"],
            "file_length_frames":reader.file_length_frames().unwrap(),"asbd":format!("{:?}",reader.source_format())}));
        reader.close().unwrap();
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","cases":rows
    })).unwrap().as_bytes());
}

#[test]
#[ignore = "records AudioToolbox WAVE ADPCM stream descriptions"]
fn capture_native_wav_adpcm_descriptions() {
    let dir = artifact_dir("native-wav-adpcm-descriptions");
    let root = dir.parent().unwrap();
    let mut rows = Vec::new();
    for folder in ["native-wav-adpcm", "native-wav-adpcm-matrix", "native-wav-adpcm-partial"] {
        let input: serde_json::Value = serde_json::from_slice(&fs::read(root.join(folder).join("receipt.json")).unwrap()).unwrap();
        for case in input["cases"].as_array().unwrap() {
            if case["exit"].as_i64() != Some(0) { continue; }
            let path = PathBuf::from(case["args"][3].as_str().unwrap());
            let reader = Reader::open(&path, None).unwrap();
            rows.push(json!({"source":path,"mode":case["mode"],"asbd":format!("{:?}",reader.source_format()),
                "length":reader.file_length_frames().unwrap(),"bitrate":reader.bitrate().ok()}));
            reader.close().unwrap();
        }
    }
    write_new(&dir.join("receipt.json"), serde_json::to_string_pretty(&json!({
        "model":"Anthropic claude-opus-5-5 xhigh","cases":rows
    })).unwrap().as_bytes());
}
}

#[test]
fn portable_channel_and_mp3_pcm_basics() {
    use platinum_engine::portable::audio::{ChannelConverter, CodecError, OutputFormat, Pcm16StereoConverter, Writer, WriterConfig};
    let mut channels = ChannelConverter::new(48_000.0, 6).unwrap();
    let left = [-0.0f32, 0.25]; let right = [1.0f32, -0.5];
    let mut interleaved = [7.0f32; 12];
    channels.convert(&left, &right, &mut interleaved).unwrap();
    assert_eq!(interleaved[0].to_bits(), (-0.0f32).to_bits());
    assert_eq!(interleaved[1..6], [1.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(interleaved[6..], [0.25, -0.5, 0.0, 0.0, 0.0, 0.0]);
    assert!(matches!(channels.convert(&left, &right[..1], &mut interleaved), Err(CodecError::InvalidInput(_))));
    channels.dispose().unwrap();
    let mut pcm = Pcm16StereoConverter::new(44_100.0, 1).unwrap();
    let mut encoded = [7i16; 10];
    let half = 0.5f32 / 32_768.0;
    assert_eq!(pcm.convert(&[0.0, half, -half, 1.0, -1.0], &mut encoded).unwrap(), 5);
    assert_eq!(encoded, [0, 0, 1, 1, -1, -1, 32_767, 32_767, -32_768, -32_768]);
    assert!(matches!(pcm.convert(&[0.0], &mut []), Err(CodecError::InvalidInput(_))));
    pcm.close().unwrap();
    let config = WriterConfig { format: OutputFormat::Aac, sample_rate: 44_100.0, channels: 2, bit_depth: 16 };
    assert!(matches!(Writer::create("unused-aac.m4a", config),
        Err(CodecError::UnsupportedCodec { codec: "AAC", operation: "encoding", .. })));
    for sample_rate in [22_049.0, 96_000.5, 96_001.0, f64::NAN] {
        let config = WriterConfig { format: OutputFormat::Alac, sample_rate, channels: 2, bit_depth: 16 };
        assert!(matches!(Writer::create("unused-alac.m4a", config), Err(CodecError::UnsupportedFormat(_))));
    }
    let config = WriterConfig { format: OutputFormat::Alac, sample_rate: 44_100.0, channels: 9, bit_depth: 16 };
    assert!(matches!(Writer::create("unused-alac.m4a", config), Err(CodecError::UnsupportedFormat(_))));
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "compares fractional-rate native PCM headers"]
fn portable_fractional_rate_parity() {
    use platinum_engine::portable::audio::{OutputFormat, Writer, WriterConfig};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("fractional-rate-parity");
    fs::create_dir(&dir).unwrap();
    let mut rows = Vec::new(); let mut failures = 0usize;
    for (name, format) in [("wav", OutputFormat::Wav), ("aiff", OutputFormat::Aiff)] {
        for label in ["22050.25", "44100.4", "44100.5", "44100.9", "47999.99", "96000.5"] {
            let rate: f64 = label.parse().unwrap();
            let source = root.join(format!("native-fractional-rate/{name}-{label}.{name}"));
            let output = dir.join(format!("{name}-{label}.{name}"));
            let mut writer = Writer::create(&output, WriterConfig { format, sample_rate: rate, channels: 1, bit_depth: 16 }).unwrap();
            writer.write(&[-50.0 / 128.0]).unwrap(); writer.finish().unwrap();
            let native = fs::read(&source).unwrap(); let portable = fs::read(&output).unwrap();
            let differing = native.iter().zip(&portable).filter(|(a,b)| a!=b).count() + native.len().abs_diff(portable.len());
            if differing != 0 { failures += 1; }
            rows.push(serde_json::json!({"format":name,"sample_rate":rate,"native":source,"portable":output,
                "differing_bytes":differing,"first_differing_byte":native.iter().zip(&portable).position(|(a,b)|a!=b)}));
        }
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","failures":failures,"cases":rows});
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    file.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap();
    file.sync_all().unwrap();
    assert_eq!(failures, 0, "fractional PCM file mismatch");
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "characterizes AudioConverter SRC across process and read partitions"]
fn native_src_partition_stability() {
    use platinum_engine::apple_codec::Reader;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("src-partitions");
    fs::create_dir(&dir).unwrap();
    let mut rows = Vec::new();
    for rate in [11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000] {
        let source = root.join(format!("src-impulses/{rate}-random.wav"));
        let expected = fs::read(root.join(format!("src-impulses/{rate}-random.f32le"))).unwrap();
        for block in [1usize, 257, 512, 4096] {
            let mut reader = Reader::open(&source, Some(44_100.0)).unwrap();
            let mut buffer = vec![13.0f32; block]; let mut bytes = Vec::new();
            loop {
                let n = reader.read(&mut buffer).unwrap();
                if n == 0 { break; }
                for value in &buffer[..n] { bytes.extend_from_slice(&value.to_bits().to_le_bytes()); }
            }
            reader.close().unwrap();
            let output = dir.join(format!("{rate}-{block}.f32le"));
            let mut file = OpenOptions::new().write(true).create_new(true).open(&output).unwrap();
            file.write_all(&bytes).unwrap(); file.sync_all().unwrap();
            let differing = bytes.iter().zip(&expected).filter(|(a,b)|a!=b).count() + bytes.len().abs_diff(expected.len());
            rows.push(serde_json::json!({"source_rate":rate,"target_rate":44100,"read_block_frames":block,
                "frames":bytes.len()/4,"output":output,"differing_bytes":differing,
                "first_differing_byte":bytes.iter().zip(&expected).position(|(a,b)|a!=b),
                "sha256":platinum_engine::mp3::file_sha256(&output).unwrap()}));
        }
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh",
        "baseline":"src-impulses/<rate>-random.f32le from probe-run-01, a separate process","cases":rows});
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    file.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); file.sync_all().unwrap();
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "compares PCM writer output across write block partitions"]
fn portable_pcm_writer_partitioning() {
    use platinum_engine::apple_codec::{OutputFormat as NativeFormat, Writer as NativeWriter, WriterConfig as NativeConfig};
    use platinum_engine::portable::audio::{OutputFormat, Writer, WriterConfig};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("writer-partitions"); fs::create_dir(&dir).unwrap();
    let mut rows = Vec::new(); let mut failures = 0usize;
    for (name, native_format, format) in [("wav", NativeFormat::Wav, OutputFormat::Wav), ("aiff", NativeFormat::Aiff, OutputFormat::Aiff)] {
        for bits in [16, 24, 32] {
            for channels in [1u32, 2, 6] {
                for rate in [44_100u32, 96_000] {
                    for block in [1usize, 7, 257, 512, 4096] {
                        let label = format!("{name}-{bits}-{channels}-{rate}-{block}");
                        let reference = dir.join(format!("{label}-native.{name}"));
                        let portable = dir.join(format!("{label}-portable.{name}"));
                        let mut state = 0x71f0_4a29u32;
                        let signal: Vec<f32> = (0..2049 * channels as usize).map(|i| {
                            state ^= state << 13; state ^= state >> 17; state ^= state << 5;
                            if i % 2 == 0 { f32::from_bits(state) } else { ((state >> 8) as f32 / 8_388_608.0) - 1.0 }
                        }).collect();
                        let mut a = NativeWriter::create(&reference, NativeConfig { format: native_format, sample_rate: rate as f64, channels, bit_depth: bits }).unwrap();
                        let mut b = Writer::create(&portable, WriterConfig { format, sample_rate: rate as f64, channels, bit_depth: bits }).unwrap();
                        for part in signal.chunks(block * channels as usize) { a.write(part).unwrap(); b.write(part).unwrap(); }
                        a.finish().unwrap(); b.finish().unwrap();
                        let expected = fs::read(&reference).unwrap(); let actual = fs::read(&portable).unwrap();
                        let differing = expected.iter().zip(&actual).filter(|(a,b)|a!=b).count() + expected.len().abs_diff(actual.len());
                        if differing != 0 { failures += 1; }
                        rows.push(serde_json::json!({"case":label,"frames":2049,"differing_bytes":differing,
                            "first_differing_byte":expected.iter().zip(&actual).position(|(a,b)|a!=b),
                            "native_sha256":platinum_engine::mp3::file_sha256(&reference).unwrap(),
                            "portable_sha256":platinum_engine::mp3::file_sha256(&portable).unwrap()}));
                    }
                }
            }
        }
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","seed":"0x71f04a29","failures":failures,"cases":rows});
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    file.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); file.sync_all().unwrap();
    assert_eq!(failures, 0, "PCM writer partition mismatch");
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "records AIFF file-length property when COMM disagrees with data length"]
fn native_aiff_declared_length_properties() {
    use platinum_engine::apple_codec::Reader;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("aiff-length-properties"); fs::create_dir(&dir).unwrap();
    let input: serde_json::Value = serde_json::from_slice(&fs::read(root.join("native-aiff-lengths/receipt.json")).unwrap()).unwrap();
    let mut rows = Vec::new();
    for case in input["cases"].as_array().unwrap() {
        let path = PathBuf::from(case["args"][3].as_str().unwrap());
        let reader = Reader::open(&path, None).unwrap();
        rows.push(serde_json::json!({"path":path,"declared_frames":case["declared_frames"],
            "source_frames":case["source_frames"],"property_frames":reader.file_length_frames().unwrap()}));
        reader.close().unwrap();
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","cases":rows});
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    file.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); file.sync_all().unwrap();
}

#[test]
#[ignore = "checks no-replace publication and workspace ownership in an exclusive evidence directory"]
fn portable_writer_publication_safety() {
    use platinum_engine::portable::audio::{OutputFormat, Writer, WriterConfig};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let name = std::env::var("PN_AUDIO_RUN").map_or_else(|_| "writer-safety".to_owned(), |s| format!("writer-safety-{s}"));
    let dir = root.join(name); fs::create_dir(&dir).unwrap();
    let config = WriterConfig { format: OutputFormat::Wav, sample_rate: 44_100.0, channels: 2, bit_depth: 16 };
    let create = |path: &std::path::Path, bytes: &[u8]| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(path).unwrap();
        f.write_all(bytes).unwrap(); f.sync_all().unwrap();
    };
    let existing = dir.join("existing.wav"); create(&existing, b"existing");
    assert!(Writer::create(&existing, config).is_err()); assert_eq!(fs::read(&existing).unwrap(), b"existing");
    let late = dir.join("late.wav");
    let mut writer = Writer::create(&late, config).unwrap(); writer.write(&[0.25, -0.25]).unwrap();
    create(&late, b"late collision");
    assert!(writer.finish().is_err()); assert_eq!(fs::read(&late).unwrap(), b"late collision");
    let aborted = dir.join("aborted.wav");
    let mut writer = Writer::create(&aborted, config).unwrap(); writer.write(&[0.25, -0.25]).unwrap();
    writer.abort().unwrap(); assert!(!aborted.exists());
    let target = dir.join("replacement.wav");
    let mut writer = Writer::create(&target, config).unwrap(); writer.write(&[0.25, -0.25]).unwrap();
    let workspace = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).find(|p| p.is_dir()).unwrap();
    let staged = workspace.join("replacement.wav"); let original = workspace.join("original-owned-file");
    fs::rename(&staged, &original).unwrap(); create(&staged, b"replacement");
    assert!(writer.finish().is_err()); assert!(!target.exists());
    assert_eq!(fs::read(&staged).unwrap(), b"replacement"); assert!(original.exists());
    let alac_config = WriterConfig { format: OutputFormat::Alac, ..config };
    let alac_late = dir.join("late.m4a");
    let mut writer = Writer::create(&alac_late, alac_config).unwrap(); writer.write(&[0.25, -0.25]).unwrap();
    create(&alac_late, b"late ALAC collision");
    assert!(writer.finish().is_err()); assert_eq!(fs::read(&alac_late).unwrap(), b"late ALAC collision");
    let alac_target = dir.join("aux-target.m4a");
    let mut writer = Writer::create(&alac_target, alac_config).unwrap(); writer.write(&[0.25, -0.25]).unwrap();
    let alac_workspace = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path())
        .find(|p| p.is_dir() && p.join("aux-target.m4a").exists()).unwrap();
    let spool = alac_workspace.join(".pn-alac-packets");
    let original_spool = alac_workspace.join("original-packet-spool");
    fs::rename(&spool, &original_spool).unwrap(); create(&spool, b"replacement spool");
    assert!(writer.finish().is_err()); assert!(!alac_target.exists());
    assert_eq!(fs::read(&spool).unwrap(), b"replacement spool"); assert!(original_spool.exists());
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh",
        "cases":["existing destination retained","late PCM and ALAC collisions retained","abort left no target",
                 "replaced PCM workspace and ALAC spool files not removed or published"],
        "retained_workspace":workspace,"retained_alac_workspace":alac_workspace});
    create(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "records native ALAC stream descriptions and bitrates"]
fn native_alac_stream_descriptions() {
    use platinum_engine::apple_codec::Reader;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("native-alac-descriptions"); fs::create_dir(&dir).unwrap();
    let input: serde_json::Value = serde_json::from_slice(&fs::read(root.join("native-alac-receipt.json")).unwrap()).unwrap();
    let mut rows = Vec::new();
    for case in input["cases"].as_array().unwrap() {
        if case["exit"].as_i64() != Some(0) { continue; }
        let path = PathBuf::from(case["args"][7].as_str().unwrap());
        let reader = Reader::open(&path, None).unwrap();
        let s = reader.source_format();
        rows.push(serde_json::json!({"path":path,"sample_rate":s.sample_rate,"format_id":s.format_id,"format_flags":s.format_flags,
            "bytes_per_packet":s.bytes_per_packet,"frames_per_packet":s.frames_per_packet,"bytes_per_frame":s.bytes_per_frame,
            "channels_per_frame":s.channels_per_frame,"bits_per_channel":s.bits_per_channel,
            "frames":reader.file_length_frames().unwrap(),"bitrate":reader.bitrate().ok()}));
        reader.close().unwrap();
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","cases":rows});
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    file.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); file.sync_all().unwrap();
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "compares high-level ALAC Reader to AudioToolbox"]
fn portable_alac_reader_parity() {
    use platinum_engine::apple_codec::{CodecError as NativeError, Reader as NativeReader};
    use platinum_engine::portable::audio::{CodecError, Reader};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let name = std::env::var("PN_AUDIO_RUN").map_or_else(|_| "alac-reader-parity".to_owned(), |s| format!("alac-reader-parity-{s}"));
    let dir = root.join(name); fs::create_dir(&dir).unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&fs::read(root.join("native-alac-receipt.json")).unwrap()).unwrap();
    let mut rows = Vec::new(); let mut failures = 0usize; let mut compared = 0u64;
    let mut paths: Vec<PathBuf> = receipt["cases"].as_array().unwrap().iter()
        .filter(|fixture| fixture["exit"].as_i64() == Some(0))
        .map(|fixture| PathBuf::from(fixture["args"][7].as_str().unwrap())).collect();
    let layouts: serde_json::Value = serde_json::from_slice(&fs::read(root.parent().unwrap().join("alac-01/layout-02/receipt.json")).unwrap()).unwrap();
    paths.extend(layouts["rows"].as_array().unwrap().iter().filter(|row| row["error"].is_null())
        .map(|row| PathBuf::from(row["path"].as_str().unwrap())));
    let mono_stereo: serde_json::Value = serde_json::from_slice(&fs::read(root.join("native-alac-mono-stereo-layout/receipt.json")).unwrap()).unwrap();
    paths.extend(mono_stereo["cases"].as_array().unwrap().iter().filter(|case| case["exit"].as_i64() == Some(0))
        .map(|case| PathBuf::from(case["args"][3].as_str().unwrap())));
    let extra_layouts: serde_json::Value = serde_json::from_slice(&fs::read(root.parent().unwrap().join("alac-01/layout-extra-01/receipt.json")).unwrap()).unwrap();
    let extra_paths: Vec<PathBuf> = extra_layouts["rows"].as_array().unwrap().iter()
        .filter_map(|row| row["path"].as_str()).map(PathBuf::from).collect();
    paths.extend(extra_paths.iter().cloned());
    let retained: serde_json::Value = serde_json::from_slice(&fs::read(root.parent().unwrap().join("alac-01/decode-retained-05/receipt.json")).unwrap()).unwrap();
    paths.extend(retained["rows"].as_array().unwrap().iter().filter_map(|row| row["path"].as_str())
        .filter(|path| path.contains("/hash-parity/")).map(PathBuf::from));
    if std::env::var_os("PN_AUDIO_EXTRA_CHANNELS").is_some() { paths.retain(|path| extra_paths.contains(path)); }
    for path in paths {
        let blocks: &[usize] = if path.to_string_lossy().contains("/hash-parity/") { &[512, 4096] } else { &[1, 257, 512, 4096] };
        for planar in [false, true] { for &block in blocks {
            let mut native = NativeReader::open(&path, None).unwrap();
            let mut portable = match Reader::open(&path, None) {
                Ok(reader) => reader,
                Err(error) => { failures += 1; rows.push(serde_json::json!({"path":path,"planar":planar,"block":block,"open_error":error.to_string()})); continue; }
            };
            let n = native.source_format(); let p = portable.source_format();
            let length = native.file_length_frames().unwrap();
            let metadata_equal = n.sample_rate == p.sample_rate && n.format_id == p.format_id && n.format_flags == p.format_flags &&
                n.bytes_per_packet == p.bytes_per_packet && n.frames_per_packet == p.frames_per_packet &&
                n.bytes_per_frame == p.bytes_per_frame && n.channels_per_frame == p.channels_per_frame &&
                n.bits_per_channel == p.bits_per_channel && length == portable.file_length_frames().unwrap() &&
                native.bitrate().ok() == portable.bitrate().ok();
            if !metadata_equal { failures += 1; }
            if planar { native.set_stereo_planar(n.sample_rate).unwrap(); portable.set_stereo_planar(p.sample_rate).unwrap(); }
            if length == 0 {
                if planar {
                    assert_eq!(native.read_stereo_planar(&mut [], &mut []).unwrap(), 0);
                    assert_eq!(portable.read_stereo_planar(&mut [], &mut []).unwrap(), 0);
                } else {
                    assert_eq!(native.read(&mut []).unwrap(), 0);
                    assert_eq!(portable.read(&mut []).unwrap(), 0);
                }
            }
            let lanes = if planar { 1 } else { n.channels_per_frame as usize };
            let mut na = vec![13.0f32; block * lanes]; let mut pa = na.clone();
            let mut nb = vec![-17.0f32; block]; let mut pb = nb.clone();
            let mut frames = 0usize; let mut mismatch = 0u64; let mut examples = Vec::new();
            let mut expected_empty_failure = false;
            loop {
                let (nr, pr) = if planar {
                    (native.read_stereo_planar(&mut na, &mut nb), portable.read_stereo_planar(&mut pa, &mut pb))
                } else { (native.read(&mut na), portable.read(&mut pa)) };
                let (count, actual) = match (nr, pr) {
                    (Ok(count), Ok(actual)) => (count, actual),
                    (Err(NativeError::Native { status: -66_567, .. }), Err(CodecError::Native { status: -66_567, .. })) if length == 0 => {
                        expected_empty_failure = true; break;
                    }
                    (a,b) => { failures += 1; examples.push(serde_json::json!({"native":format!("{a:?}"),"portable":format!("{b:?}")})); break; }
                };
                if count != actual { failures += 1; examples.push(serde_json::json!({"native_frames":count,"portable_frames":actual})); break; }
                if count == 0 { break; }
                for i in 0..count * lanes {
                    compared += 1;
                    if na[i].to_bits() != pa[i].to_bits() {
                        mismatch += 1;
                        if examples.len() < 8 { examples.push(serde_json::json!({"at_frame":frames,"index":i,
                            "native_bits":format!("{:08x}",na[i].to_bits()),"portable_bits":format!("{:08x}",pa[i].to_bits())})); }
                    }
                }
                if planar { for i in 0..count {
                    compared += 1;
                    if nb[i].to_bits() != pb[i].to_bits() {
                        mismatch += 1;
                        if examples.len() < 8 { examples.push(serde_json::json!({"at_frame":frames,"right_index":i,
                            "native_bits":format!("{:08x}",nb[i].to_bits()),"portable_bits":format!("{:08x}",pb[i].to_bits())})); }
                    }
                } }
                frames += count;
            }
            if mismatch != 0 { failures += 1; }
            native.close().unwrap(); portable.close().unwrap();
            rows.push(serde_json::json!({"path":path,"planar":planar,"block":block,"frames":frames,
                "metadata_equal":metadata_equal,"native_asbd":format!("{n:?}"),"portable_asbd":format!("{p:?}"),
                "empty_failure_matched":expected_empty_failure,"mismatched_samples":mismatch,"examples":examples}));
        } }
    }
    let result = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","compared_sample_bits":compared,"failures":failures,"cases":rows});
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    file.write_all(serde_json::to_string_pretty(&result).unwrap().as_bytes()).unwrap(); file.sync_all().unwrap();
    assert_eq!(failures, 0, "ALAC high-level reader mismatch");
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "compares high-level ALAC writer in ordinary and controlled clock modes"]
fn portable_alac_writer_parity() {
    use platinum_engine::apple_codec;
    use platinum_engine::portable::audio::{self, OutputFormat, Writer, WriterConfig};
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn copy_new(source: &Path, destination: &Path) {
        let mut input = fs::File::open(source).unwrap();
        let mut output = OpenOptions::new().write(true).create_new(true).open(destination).unwrap();
        std::io::copy(&mut input, &mut output).unwrap(); output.sync_all().unwrap();
    }
    fn clock(path: &Path) -> u32 {
        let mut file = fs::File::open(path).unwrap(); let mut header = [0u8; 52]; file.read_exact(&mut header).unwrap();
        assert_eq!(&header[40..44], b"mvhd");
        u32::from_be_bytes(header[48..52].try_into().unwrap())
    }
    fn now_1904() -> u32 { (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 2_082_844_800) as u32 }
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let oracle = root.parent().unwrap().parent().unwrap().join("apple-codecs/codec-oracle");
    let name = std::env::var("PN_AUDIO_RUN").map_or_else(|_| "alac-writer-parity".to_owned(), |s| format!("alac-writer-parity-{s}"));
    let dir = root.join(name); fs::create_dir(&dir).unwrap();
    let mut rows = Vec::new(); let mut failures = 0usize; let mut compared = 0u64;
    let channels_to_test: &[u32] = if std::env::var_os("PN_AUDIO_EXTRA_CHANNELS").is_some() { &[3,4,5,7,8] } else { &[1,2,6] };
    let arbitrary_rates = std::env::var_os("PN_AUDIO_ARBITRARY_RATES").is_some();
    let fractional_rates = std::env::var_os("PN_AUDIO_FRACTIONAL_RATES").is_some();
    let rates: &[f64] = if fractional_rates { &[22_050.25, 44_100.4, 44_100.5, 44_100.9, 57_343.99, 57_344.01, 95_999.9] }
        else if arbitrary_rates { &[24_576.0,32_768.0,40_960.0,49_152.0,57_343.0,57_344.0,65_536.0,81_920.0,95_999.0] }
        else { &[22_050.0,32_000.0,44_100.0,48_000.0,88_200.0,96_000.0] };
    let lengths: &[usize] = if arbitrary_rates || fractional_rates { &[1,513,65_537] } else { &[0,1,511,512,513,4096,8192,8193] };
    for bits in [16u32,24,32] { for &channels in channels_to_test { for &rate in rates {
        for &frames in lengths {
            let label = format!("{bits}-{channels}-{rate}-{frames}");
            let native = dir.join(format!("{label}-native.m4a")); let portable = dir.join(format!("{label}-portable.m4a"));
            let reference = Command::new(&oracle).args(["encode", "alac", &rate.to_string(), &channels.to_string(),
                &bits.to_string(), &frames.to_string()]).arg(&native).output().unwrap();
            assert!(reference.status.success(), "native oracle {label}: {}", String::from_utf8_lossy(&reference.stderr));
            let mut p = Writer::create(&portable, WriterConfig { format: OutputFormat::Alac, sample_rate: rate, channels, bit_depth: bits }).unwrap();
            for start in (0..frames).step_by(512) {
                let count = (frames - start).min(512);
                let samples: Vec<f32> = (start..start+count).flat_map(|frame| (0..channels).map(move |channel|
                    ((((frame % 101) * 17 + (channel as usize % 101) * 29) % 101) as i32 - 50) as f32 / 128.0)).collect();
                p.write(&samples).unwrap();
            }
            let before = now_1904();
            if let Err(error) = p.finish() {
                failures += 1; rows.push(serde_json::json!({"case":label,"portable_error":error.to_string()})); continue;
            }
            let after = now_1904();
            let actual_clock = clock(&portable);
            let clock_valid = (before..=after).contains(&actual_clock);
            let ordinary_native = dir.join(format!("{label}-ordinary-native.m4a"));
            copy_new(&native, &ordinary_native);
            let n_count = apple_codec::set_mp4_header_times(&ordinary_native, actual_clock).unwrap();
            let ordinary_ref = fs::read(&ordinary_native).unwrap(); let ordinary_actual = fs::read(&portable).unwrap();
            let ordinary_diff = ordinary_ref.iter().zip(&ordinary_actual).filter(|(a,b)|a!=b).count() + ordinary_ref.len().abs_diff(ordinary_actual.len());
            compared += ordinary_ref.len().min(ordinary_actual.len()) as u64;
            let controlled_native = dir.join(format!("{label}-controlled-native.m4a"));
            let controlled_portable = dir.join(format!("{label}-controlled-portable.m4a"));
            copy_new(&native, &controlled_native); copy_new(&portable, &controlled_portable);
            let expected_count = apple_codec::set_mp4_header_times(&controlled_native, 0x1234_5678).unwrap();
            let actual_count = audio::set_mp4_header_times(&controlled_portable, 0x1234_5678).unwrap();
            let controlled_ref = fs::read(&controlled_native).unwrap(); let controlled_actual = fs::read(&controlled_portable).unwrap();
            let controlled_diff = controlled_ref.iter().zip(&controlled_actual).filter(|(a,b)|a!=b).count() + controlled_ref.len().abs_diff(controlled_actual.len());
            compared += controlled_ref.len().min(controlled_actual.len()) as u64;
            if !clock_valid || n_count != actual_count || expected_count != actual_count || ordinary_diff != 0 || controlled_diff != 0 { failures += 1; }
            rows.push(serde_json::json!({"case":label,"frames":frames,"clock_1904":actual_clock,"clock_valid":clock_valid,
                "ordinary_differing_bytes":ordinary_diff,"ordinary_first_difference":ordinary_ref.iter().zip(&ordinary_actual).position(|(a,b)|a!=b),
                "controlled_differing_bytes":controlled_diff,"controlled_first_difference":controlled_ref.iter().zip(&controlled_actual).position(|(a,b)|a!=b),
                "native_patch_fields":expected_count,"portable_patch_fields":actual_count,
                "native_sha256":platinum_engine::mp3::file_sha256(&controlled_native).unwrap(),
                "portable_sha256":platinum_engine::mp3::file_sha256(&controlled_portable).unwrap()}));
        }
    } } }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","controlled_clock":"0x12345678",
        "oracle":oracle,"oracle_sha256":platinum_engine::mp3::file_sha256(&oracle).unwrap(),
        "compared_bytes":compared,"failures":failures,"cases":rows});
    let mut output = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    output.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); output.sync_all().unwrap();
    assert_eq!(failures, 0, "ALAC high-level writer parity mismatch");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "records AudioConverter FLAC planar signed-integer to float conversion"]
fn native_flac_integer_conversion() {
    use platinum_engine::apple_codec::StreamDescription;
    use std::ffi::c_void;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::ptr;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Buffer { channels: u32, bytes: u32, data: *mut c_void }
    #[repr(C)]
    struct BufferList { count: u32, buffers: [Buffer; 8] }
    struct Input { pointers: [*const u8; 8], channels: usize, frames: u32, width: u32, sent: bool }
    unsafe extern "C" fn feed(_converter: *mut c_void, packets: *mut u32, list: *mut BufferList,
        _descriptions: *mut *mut c_void, context: *mut c_void) -> i32 {
        let input = unsafe { &mut *context.cast::<Input>() };
        if input.sent { unsafe { *packets = 0; } return u32::from_be_bytes(*b"more") as i32; }
        let count = unsafe { ptr::addr_of!((*list).count).read() } as usize;
        if count > input.channels { unsafe { *packets = 0; } return -1; }
        let buffers = unsafe { ptr::addr_of_mut!((*list).buffers).cast::<Buffer>() };
        for c in 0..count {
            let buffer = unsafe { buffers.add(c) };
            unsafe {
                ptr::addr_of_mut!((*buffer).bytes).write(input.frames * input.width);
                ptr::addr_of_mut!((*buffer).data).write(input.pointers[c].cast_mut().cast());
            }
        }
        unsafe { *packets = input.frames; }
        input.sent = true;
        0
    }
    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioConverterNew(input: *const StreamDescription, output: *const StreamDescription, converter: *mut *mut c_void) -> i32;
        fn AudioConverterFillComplexBuffer(converter: *mut c_void,
            callback: Option<unsafe extern "C" fn(*mut c_void, *mut u32, *mut BufferList, *mut *mut c_void, *mut c_void) -> i32>,
            context: *mut c_void, packets: *mut u32, output: *mut BufferList, descriptions: *mut c_void) -> i32;
        fn AudioConverterDispose(converter: *mut c_void) -> i32;
    }
    fn create(path: &std::path::Path, bytes: &[u8]) {
        let mut f = OpenOptions::new().write(true).create_new(true).open(path).unwrap();
        f.write_all(bytes).unwrap(); f.sync_all().unwrap();
    }
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let exhaustive = std::env::var_os("PN_AUDIO_FLAC_EXHAUSTIVE").is_some();
    let dir = root.join(if exhaustive { "flac-integer-exhaustive" } else { "flac-integer-conversion" }); fs::create_dir(&dir).unwrap();
    let mut rows = Vec::new();
    for bits in 8u32..=32 {
        if exhaustive && !matches!(bits, 8 | 16) { continue; }
        for channels in [1u32,2,6] {
            if exhaustive && channels != 1 { continue; }
            let width = bits / 8;
            let source = StreamDescription { sample_rate: 44_100.0, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 0x24,
                bytes_per_packet: width, frames_per_packet: 1, bytes_per_frame: width, channels_per_frame: channels, bits_per_channel: bits, reserved: 0 };
            let target = StreamDescription { sample_rate: 44_100.0, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 0x29,
                bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4, channels_per_frame: channels, bits_per_channel: 32, reserved: 0 };
            let mut converter = ptr::null_mut();
            let status = unsafe { AudioConverterNew(&source, &target, &mut converter) };
            let mut record = serde_json::json!({"bits":bits,"channels":channels,"new_status":status});
            if status == 0 && !converter.is_null() {
                let n = if exhaustive { 1usize << bits } else { 1027usize }; let mut seed = 0x54ac_23d1u32;
                let mut packed = Vec::new(); let mut integers = Vec::new();
                for _ in 0..channels {
                    let mut plane = Vec::new();
                    for i in 0..n {
                        seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
                        let value = if exhaustive { (((i as u32) << (32 - bits)) as i32) >> (32 - bits) }
                            else if i < 8 { [0i32,1,-1,i32::MIN,i32::MAX,127,-128,32767][i] }
                            else { ((seed << (32 - bits)) as i32) >> (32 - bits) };
                        integers.extend_from_slice(&value.to_le_bytes());
                        plane.extend_from_slice(&value.to_le_bytes()[..width as usize]);
                    }
                    packed.push(plane);
                }
                let mut input = Input { pointers: [ptr::null();8], channels: channels as usize, frames: n as u32, width, sent: false };
                let mut output: Vec<Vec<f32>> = (0..channels).map(|_| vec![13.0; n]).collect();
                let blank = Buffer { channels: 0, bytes: 0, data: ptr::null_mut() };
                let mut list = BufferList { count: channels, buffers: [blank;8] };
                for c in 0..channels as usize {
                    input.pointers[c] = packed[c].as_ptr();
                    list.buffers[c] = Buffer { channels: 1, bytes: (n*4) as u32, data: output[c].as_mut_ptr().cast() };
                }
                let mut frames = n as u32;
                let fill = unsafe { AudioConverterFillComplexBuffer(converter, Some(feed), (&mut input as *mut Input).cast(), &mut frames, &mut list, ptr::null_mut()) };
                let close = unsafe { AudioConverterDispose(converter) };
                let input_path = dir.join(format!("{bits}-{channels}.i32le")); create(&input_path, &integers);
                let output_path = dir.join(format!("{bits}-{channels}.f32le"));
                let values: Vec<u8> = output.iter().flat_map(|plane| plane[..frames as usize].iter().flat_map(|x| x.to_bits().to_le_bytes())).collect();
                create(&output_path, &values);
                record["fill_status"] = serde_json::json!(fill); record["frames"] = serde_json::json!(frames);
                record["close_status"] = serde_json::json!(close); record["input"] = serde_json::json!(input_path);
                record["output"] = serde_json::json!(output_path);
            } else if !converter.is_null() { let _ = unsafe { AudioConverterDispose(converter) }; }
            rows.push(record);
        }
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","seed":"0x54ac23d1",
        "pattern":if exhaustive { "every possible signed sample word" } else { "seeded samples plus boundary values" },"cases":rows});
    create(&dir.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap().as_bytes());
}

#[test]
#[ignore = "compares the FLAC integer conversion primitive with captured AudioConverter results"]
fn portable_flac_integer_parity() {
    use platinum_engine::portable::audio::{convert_flac_pcm, CodecError};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let exhaustive = std::env::var_os("PN_AUDIO_FLAC_EXHAUSTIVE").is_some();
    let dir = root.join(if exhaustive { "flac-integer-exhaustive-parity" } else { "flac-integer-parity" }); fs::create_dir(&dir).unwrap();
    let source = if exhaustive { "flac-integer-exhaustive/receipt.json" } else { "flac-integer-conversion/receipt.json" };
    let input: serde_json::Value = serde_json::from_slice(&fs::read(root.join(source)).unwrap()).unwrap();
    let mut rows = Vec::new(); let mut failures = 0usize; let mut compared = 0usize;
    for case in input["cases"].as_array().unwrap() {
        let bits = case["bits"].as_u64().unwrap() as u32;
        if case["new_status"].as_i64() != Some(0) {
            if !matches!(convert_flac_pcm(&[], bits, &mut []), Err(CodecError::UnsupportedFormat(_))) { failures += 1; }
            rows.push(serde_json::json!({"bits":bits,"channels":case["channels"],"native_refused":true}));
            continue;
        }
        let raw = fs::read(case["input"].as_str().unwrap()).unwrap();
        let expected = fs::read(case["output"].as_str().unwrap()).unwrap();
        let samples: Vec<i32> = raw.chunks_exact(4).map(|b| i32::from_le_bytes(b.try_into().unwrap())).collect();
        let mut actual = vec![0f32; samples.len()];
        convert_flac_pcm(&samples, bits, &mut actual).unwrap();
        let mut mismatches = 0usize; let mut examples = Vec::new();
        for (i, (value, b)) in actual.iter().zip(expected.chunks_exact(4)).enumerate() {
            compared += 1;
            let expected = u32::from_le_bytes(b.try_into().unwrap());
            if expected != value.to_bits() {
                mismatches += 1;
                if examples.len() < 8 { examples.push(serde_json::json!({"index":i,"expected":format!("{expected:08x}"),"actual":format!("{:08x}",value.to_bits())})); }
            }
        }
        if expected.len() != actual.len() * 4 || mismatches != 0 { failures += 1; }
        rows.push(serde_json::json!({"bits":bits,"channels":case["channels"],"samples":actual.len(),"mismatches":mismatches,"examples":examples}));
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","compared_sample_bits":compared,"failures":failures,"cases":rows});
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    file.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); file.sync_all().unwrap();
    assert_eq!(failures, 0, "FLAC PCM conversion mismatch");
}

#[test]
#[ignore = "checks that oversized ALAC packets fail closed without publishing"]
fn portable_alac_native_loss_refusal() {
    use platinum_engine::portable::audio::{alac::EncodeError, CodecError, OutputFormat, Writer, WriterConfig};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("alac-native-loss-refusal"); fs::create_dir(&dir).unwrap();
    let source = root.parent().unwrap().join("alac-01/native-signals-01/inputs/noise-6-4096.f32le");
    let bytes = fs::read(&source).unwrap();
    let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_bits(u32::from_le_bytes(b.try_into().unwrap()))).collect();
    let mut rows = Vec::new();
    for bits in [24u32,32] {
        let target = dir.join(format!("noise-{bits}.m4a"));
        let mut writer = Writer::create(&target, WriterConfig { format: OutputFormat::Alac, sample_rate: 44_100.0, channels: 6, bit_depth: bits }).unwrap();
        let mut error = None;
        for chunk in samples.chunks(512 * 6) {
            if let Err(e) = writer.write(chunk) { error = Some(e); break; }
        }
        assert!(matches!(&error, Some(CodecError::AlacEncode(EncodeError::NativePacketLimit { .. }))));
        let finish = writer.finish();
        assert!(matches!(&finish, Err(CodecError::Poisoned)));
        assert!(!target.exists());
        rows.push(serde_json::json!({"bits":bits,"error":format!("{error:?}"),"finish":format!("{finish:?}"),"target_exists":target.exists()}));
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","source":source,"cases":rows,
        "native_loss_evidence":"../alac-01/portable-results-linux-arm64-02.tsv"});
    let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); f.sync_all().unwrap();
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "inventories AudioFile advertised readable format IDs"]
fn native_audiofile_format_inventory() {
    use std::ffi::c_void;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioFileGetGlobalInfoSize(property: u32, spec_size: u32, spec: *const c_void, output_size: *mut u32) -> i32;
        fn AudioFileGetGlobalInfo(property: u32, spec_size: u32, spec: *const c_void, output_size: *mut u32, output: *mut c_void) -> i32;
    }
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("audiofile-format-inventory"); fs::create_dir(&dir).unwrap();
    let mut rows = Vec::new();
    for kind in [*b"WAVE", *b"AIFF", *b"AIFC", *b"m4af", *b"caff"] {
        let spec = u32::from_be_bytes(kind); let mut size = 0u32;
        let size_status = unsafe { AudioFileGetGlobalInfoSize(u32::from_be_bytes(*b"fmid"), 4, (&spec as *const u32).cast(), &mut size) };
        let mut values = vec![0u32; size as usize / 4]; let mut get_status = None;
        if size_status == 0 && size % 4 == 0 {
            get_status = Some(unsafe { AudioFileGetGlobalInfo(u32::from_be_bytes(*b"fmid"), 4,
                (&spec as *const u32).cast(), &mut size, values.as_mut_ptr().cast()) });
        }
        rows.push(serde_json::json!({"file_type":String::from_utf8_lossy(&kind),"size_status":size_status,
            "get_status":get_status,"format_ids":values.iter().map(|v| format!("{v:08x}")).collect::<Vec<_>>(),
            "fourcc":values.iter().map(|v| String::from_utf8_lossy(&v.to_be_bytes()).into_owned()).collect::<Vec<_>>()}));
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh",
        "scope":"AudioFile advertised IDs only; does not prove a decoder or particular file is accepted","cases":rows});
    let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); f.sync_all().unwrap();
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "compares long ALAC streams and native-loss refusals through the high-level writer"]
fn portable_alac_long_writer_parity() {
    use platinum_engine::apple_codec;
    use platinum_engine::portable::audio::{self, alac::EncodeError, CodecError, OutputFormat, Writer, WriterConfig};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    fn copy_new(source: &Path, target: &Path) {
        let mut input = fs::File::open(source).unwrap();
        let mut output = OpenOptions::new().write(true).create_new(true).open(target).unwrap();
        std::io::copy(&mut input, &mut output).unwrap(); output.sync_all().unwrap();
    }
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("alac-long-writer-parity"); fs::create_dir(&dir).unwrap();
    let peer = root.parent().unwrap().join("alac-01");
    let tsv = fs::read_to_string(peer.join("portable-results-linux-arm64-02.tsv")).unwrap();
    let mut rows = Vec::new(); let mut failures = 0usize; let mut compared = 0u64; let mut refused = 0usize;
    for line in tsv.lines().skip(1) {
        let f: Vec<_> = line.split('\t').collect();
        if f.len() < 10 || f[2] != "65537" || !matches!(f[0], "silence" | "tone" | "noise") ||
            !matches!(f[3], "16" | "24" | "32") || !matches!(f[4], "22050" | "44100" | "96000") { continue; }
        let channels: u32 = f[1].parse().unwrap(); let bits: u32 = f[3].parse().unwrap(); let rate: u32 = f[4].parse().unwrap();
        let label = format!("{}-{channels}-65537-{bits}-{rate}", f[0]);
        let source = peer.join(format!("native-signals-01/inputs/{}-{channels}-65537.f32le", f[0]));
        let reference = peer.join(format!("native-signals-01/outputs/{label}.m4a"));
        let native = dir.join(format!("{label}-native.m4a")); let portable = dir.join(format!("{label}-portable.m4a"));
        let raw = fs::read(&source).unwrap();
        let samples: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_bits(u32::from_le_bytes(b.try_into().unwrap()))).collect();
        let mut writer = Writer::create(&portable, WriterConfig { format: OutputFormat::Alac, sample_rate: rate as f64, channels, bit_depth: bits }).unwrap();
        let mut write_error = None;
        for part in samples.chunks(512 * channels as usize) {
            if let Err(error) = writer.write(part) { write_error = Some(error); break; }
        }
        let finish = writer.finish();
        if f[9] == "REFUSED_NATIVE_LOSS" {
            let safe = matches!(&write_error, Some(CodecError::AlacEncode(EncodeError::NativePacketLimit { .. }))) &&
                matches!(&finish, Err(CodecError::Poisoned)) && !portable.exists();
            if safe { refused += 1; } else { failures += 1; }
            rows.push(serde_json::json!({"case":label,"outcome":"native_loss","safe_refusal":safe,
                "write_error":format!("{write_error:?}"),"finish":format!("{finish:?}")}));
            continue;
        }
        if write_error.is_some() || finish.is_err() {
            failures += 1;
            rows.push(serde_json::json!({"case":label,"write_error":format!("{write_error:?}"),"finish":format!("{finish:?}")}));
            continue;
        }
        copy_new(&reference, &native);
        apple_codec::set_mp4_header_times(&native, 0x1234_5678).unwrap();
        audio::set_mp4_header_times(&portable, 0x1234_5678).unwrap();
        let n = fs::read(&native).unwrap(); let p = fs::read(&portable).unwrap();
        let differing = n.iter().zip(&p).filter(|(a,b)|a!=b).count() + n.len().abs_diff(p.len());
        compared += n.len().min(p.len()) as u64;
        if differing != 0 { failures += 1; }
        rows.push(serde_json::json!({"case":label,"outcome":"match","differing_bytes":differing,
            "first_differing_byte":n.iter().zip(&p).position(|(a,b)|a!=b),
            "native_sha256":platinum_engine::mp3::file_sha256(&native).unwrap(),
            "portable_sha256":platinum_engine::mp3::file_sha256(&portable).unwrap()}));
    }
    let result = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","controlled_clock":"0x12345678",
        "compared_bytes":compared,"safe_refusals":refused,"failures":failures,"cases":rows});
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    file.write_all(serde_json::to_string_pretty(&result).unwrap().as_bytes()).unwrap(); file.sync_all().unwrap();
    assert_eq!(failures, 0, "long ALAC high-level parity mismatch");
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "compares high-level ALAC output across write partitions"]
fn portable_alac_writer_partitions() {
    use platinum_engine::apple_codec::{self, OutputFormat as NativeFormat, Writer as NativeWriter, WriterConfig as NativeConfig};
    use platinum_engine::portable::audio::{self, OutputFormat, Writer, WriterConfig};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    fn copy_new(source: &Path, destination: &Path) {
        let mut input = fs::File::open(source).unwrap();
        let mut output = OpenOptions::new().write(true).create_new(true).open(destination).unwrap();
        std::io::copy(&mut input, &mut output).unwrap(); output.sync_all().unwrap();
    }
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("alac-writer-partitions"); fs::create_dir(&dir).unwrap();
    let mut rows = Vec::new(); let mut failures = 0usize; let mut compared = 0u64;
    for bits in [16u32,24,32] { for channels in [1u32,2,6] { for frames in [4097usize,8193] {
        let input: Vec<f32> = (0..frames).flat_map(|frame| (0..channels).map(move |channel|
            ((((frame % 101) * 17 + (channel as usize % 101) * 29) % 101) as i32 - 50) as f32 / 128.0)).collect();
        for block in [1usize,257,512,4096,5000] {
            let label = format!("{bits}-{channels}-{frames}-{block}");
            let native = dir.join(format!("{label}-native.m4a")); let portable = dir.join(format!("{label}-portable.m4a"));
            let mut n = NativeWriter::create(&native, NativeConfig { format: NativeFormat::Alac, sample_rate: 44_100.0, channels, bit_depth: bits }).unwrap();
            let mut p = Writer::create(&portable, WriterConfig { format: OutputFormat::Alac, sample_rate: 44_100.0, channels, bit_depth: bits }).unwrap();
            for part in input.chunks(block * channels as usize) { n.write(part).unwrap(); p.write(part).unwrap(); }
            n.finish().unwrap(); p.finish().unwrap();
            let native_normalized = dir.join(format!("{label}-native-controlled.m4a"));
            let portable_normalized = dir.join(format!("{label}-portable-controlled.m4a"));
            copy_new(&native, &native_normalized); copy_new(&portable, &portable_normalized);
            apple_codec::set_mp4_header_times(&native_normalized, 0x1234_5678).unwrap();
            audio::set_mp4_header_times(&portable_normalized, 0x1234_5678).unwrap();
            let a = fs::read(&native_normalized).unwrap(); let b = fs::read(&portable_normalized).unwrap();
            let differing = a.iter().zip(&b).filter(|(a,b)|a!=b).count() + a.len().abs_diff(b.len());
            compared += a.len().min(b.len()) as u64;
            if differing != 0 { failures += 1; }
            rows.push(serde_json::json!({"case":label,"differing_bytes":differing,
                "first_difference":a.iter().zip(&b).position(|(a,b)|a!=b),
                "native_sha256":platinum_engine::mp3::file_sha256(&native_normalized).unwrap(),
                "portable_sha256":platinum_engine::mp3::file_sha256(&portable_normalized).unwrap()}));
        }
    } } }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","compared_bytes":compared,"failures":failures,"cases":rows});
    let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); f.sync_all().unwrap();
    assert_eq!(failures, 0, "ALAC partition mismatch");
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "records native WAVE GSM stream descriptions"]
fn native_wav_gsm_descriptions() {
    use platinum_engine::apple_codec::Reader;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("native-wav-gsm-descriptions"); fs::create_dir(&dir).unwrap();
    let mut rows = Vec::new();
    for folder in ["native-wav-gsm", "native-wav-gsm-partial"] {
        let input: serde_json::Value = serde_json::from_slice(&fs::read(root.join(folder).join("receipt.json")).unwrap()).unwrap();
        for case in input["cases"].as_array().unwrap() {
            if case["exit"].as_i64() != Some(0) { continue; }
            let path = PathBuf::from(case["args"][3].as_str().unwrap());
            let reader = Reader::open(&path, None).unwrap();
            rows.push(serde_json::json!({"source":path,"asbd":format!("{:?}",reader.source_format()),
                "length":reader.file_length_frames().unwrap(),"bitrate":reader.bitrate().ok()}));
            reader.close().unwrap();
        }
    }
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","cases":rows});
    let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); f.sync_all().unwrap();
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "records source properties of fractional-rate ALAC output"]
fn native_alac_fractional_reader_properties() {
    use platinum_engine::apple_codec::Reader;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("PN_AUDIO_ARTIFACTS").unwrap());
    let dir = root.join("alac-fractional-reader-properties"); fs::create_dir(&dir).unwrap();
    let source_dir = root.parent().unwrap().join("alac-01/fractional-long-01");
    let mut rows = Vec::new();
    for entry in fs::read_dir(source_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|x| x.to_str()) != Some("m4a") { continue; }
        let reader = Reader::open(&path, None).unwrap(); let source = reader.source_format();
        rows.push(serde_json::json!({"path":path,"sample_rate":source.sample_rate,"flags":source.format_flags,
            "frames":reader.file_length_frames().unwrap(),"bitrate":reader.bitrate().ok(),
            "sha256":platinum_engine::mp3::file_sha256(&path).unwrap()}));
        reader.close().unwrap();
    }
    rows.sort_by_key(|r| r["path"].as_str().unwrap().to_owned());
    let receipt = serde_json::json!({"model":"Anthropic claude-opus-5-5 xhigh","cases":rows});
    let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap(); f.sync_all().unwrap();
}
