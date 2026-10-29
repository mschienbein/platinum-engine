//! Portable (open-source, opt-in) MP3 decoder: accuracy against recorded AudioToolbox output,
//! pinned deterministic output, the opt-in gate and the refusals.
//!
//! The fixtures in `tests/fixtures/mp3` are generated audio (see
//! `tools/portable/mp3dec/make_fixtures.py`); the `.apple.s16` files are what AudioToolbox
//! delivered for each of them on macOS arm64.

use platinum_engine::portable::audio::mp3::{Mp3Error, Mp3Stream};
use platinum_engine::portable::audio::{CodecError, Reader, ReaderOptions};
use platinum_engine::portable::digest::Sha256;
use std::fs::File;
use std::path::{Path, PathBuf};

const OPT_IN: ReaderOptions = ReaderOptions { non_identical_mp3: true };

fn dir() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mp3") }

struct Fixture { name: String, rate: u32, channels: usize, length_property: i64, delivered: usize }

fn fixtures() -> Vec<Fixture> {
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(dir().join("manifest.json")).unwrap()).unwrap();
    manifest.as_array().unwrap().iter().map(|e| Fixture {
        name: e["name"].as_str().unwrap().to_owned(),
        rate: e["rate"].as_u64().unwrap() as u32,
        channels: e["apple_channels"].as_u64().unwrap() as usize,
        length_property: e["apple_length_property"].as_i64().unwrap(),
        delivered: e["apple_frames_delivered"].as_u64().unwrap() as usize,
    }).collect()
}

fn mp3_path(name: &str) -> PathBuf { dir().join(format!("{name}.mp3")) }

/// Decode a whole fixture through the stream API with `chunk`-frame reads.
fn decode(name: &str, chunk: usize) -> (Vec<f32>, Vec<f32>) { decode_path(&mp3_path(name), chunk) }

fn decode_path(path: &Path, chunk: usize) -> (Vec<f32>, Vec<f32>) {
    drain(&mut Mp3Stream::open(File::open(path).unwrap()).unwrap(), chunk)
}

fn drain(stream: &mut Mp3Stream, chunk: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let (mut l, mut r) = (vec![0f32; chunk], vec![0f32; chunk]);
    loop {
        let n = stream.read_planar(&mut l, &mut r).unwrap();
        if n == 0 { break; }
        left.extend_from_slice(&l[..n]);
        right.extend_from_slice(&r[..n]);
    }
    (left, right)
}

fn read_json(path: &Path) -> serde_json::Value { serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap() }

fn read_s16(path: &Path) -> Vec<i16> {
    std::fs::read(path).unwrap().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

/// (identical samples, largest difference in 16-bit LSB, samples compared) against AudioToolbox's
/// interleaved 16-bit capture.
fn accuracy(name: &str, left: &[f32], right: &[f32], channels: usize, apple: &[i16]) -> (usize, i32, usize) {
    assert_eq!(apple.len(), left.len() * channels, "{name}: capture size");
    let (mut identical, mut max_lsb, mut total) = (0usize, 0i32, 0usize);
    for (frame, pair) in left.iter().zip(right).enumerate() {
        for (c, value) in [*pair.0, *pair.1].into_iter().take(channels).enumerate() {
            let mine = (f64::from(value) * 32768.0).round() as i32;
            assert_eq!(f64::from(value) * 32768.0, f64::from(mine), "{name}: output is not on the 16-bit grid");
            let diff = (mine - i32::from(apple[frame * channels + c])).abs();
            identical += usize::from(diff == 0);
            max_lsb = max_lsb.max(diff);
            total += 1;
        }
    }
    (identical, max_lsb, total)
}

fn digest(left: &[f32], right: &[f32]) -> String {
    let mut hash = Sha256::new();
    let mut bytes = Vec::with_capacity(left.len() * 8);
    for (l, r) in left.iter().zip(right) {
        bytes.extend_from_slice(&l.to_bits().to_le_bytes());
        bytes.extend_from_slice(&r.to_bits().to_le_bytes());
    }
    hash.update(&bytes);
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of the decoded planar float bits (left, right per frame), identical on macOS
/// `--features portable`, Linux arm64 and Linux x86_64 (see local-artifacts/portable-parity/mp3dec-01).
const PINNED: [(&str, &str); 16] = [
    ("ffmpeg_cbr128_44100", "a5238e12ac16976d317b1743778c0c7abb18d23870e8a1525585b5a4c819ba14"),
    ("lame_abr96_32000_js", "0ad476bad52a5fa05f7239dd430f0762de4d0238524cacad7d3b8b7d16375800"),
    ("lame_cbr128_44100_crc", "3cce68e374a84e182a7222dfddc9ee8fd774d9ea19521a8ed01352ea97d5c486"),
    ("lame_cbr128_44100_js", "c98bf5f7fec9dc7f03fba3085525872c09dd6659c8fb86ab5bacb3275489cbd4"),
    ("lame_cbr128_44100_mono", "72731c587176f325b45a8a9d941ae4c560bf4ad6b4207ee62219ba22c5ef0e95"),
    ("lame_cbr192_48000_stereo", "17ac499d285168997c8f173f3a29364c1d456b1d13cfd7cf5ccdbf418f9b61e3"),
    ("lame_cbr32_11025_mono", "728c0c8cbde03ef7cc11cb7b3dac4ce698ab4ecaaef0669955b62de4af4e18c0"),
    ("lame_loud_cbr128", "86465707a28f69782bdbed667e52b5be09de83838a3e59b6c13a6f58e8c8ec9b"),
    ("lame_vbrV2_22050_js", "d57c7665a163dff5dc6bdb7263046aa332c4511559afa25824eb20ed772e5ab4"),
    ("tagless_cbr128_44100", "b8735fb44f5a2807f5dd81d240e66d11db6c416216ecc89a9bd114fbf4783e8c"),
    // One per VBRI layout: every variant of a layout decodes to the same samples.
    ("vbri/payload_vbrV2_44100", "ddfabf44c4da918f505b313df48f63fed567ff78344a23383f9cdba9e388c7b4"),
    ("vbri/vbri_v1m_normal", "3176259de8c4ac8644d92655c7f0b67a485c692779cb810968be1e909d70ba97"),
    ("vbri/vbri_v1s_normal", "2f3396ad8fac128df43de19ec8d3cefb40d3d237174641bb64ab17d5d137b27c"),
    ("vbri/vbri_v1vbr_normal", "86427f969ba12da2d403aa0e808fcc4a96c0edc34b9ea0b39f2a7945bb37fefd"),
    ("vbri/vbri_v25m_normal", "351f13618b6bccd9b0fcf8bf800db3f5d252af941dd1e575b81f4dabee14101f"),
    ("vbri/vbri_v2svbr_normal", "c552bbd5b45b91b026906b47348ad4ac4cc3ab3add2f02fc8dc0e41f9fd7529b"),
];

#[test]
fn length_alignment_and_samples_match_recorded_audiotoolbox_output() {
    for fixture in fixtures() {
        let stream = Mp3Stream::open(File::open(mp3_path(&fixture.name)).unwrap()).unwrap();
        let info = stream.info();
        assert_eq!((info.sample_rate, info.channels as usize, info.length_frames), (fixture.rate, fixture.channels, fixture.length_property), "{}", fixture.name);
        let (left, right) = decode(&fixture.name, 4096);
        assert_eq!(left.len(), fixture.delivered, "{}: decoded frame count", fixture.name);
        let apple: Vec<i16> = std::fs::read(dir().join(format!("{}.apple.s16", fixture.name))).unwrap()
            .chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(apple.len(), fixture.delivered * fixture.channels, "{}: reference size", fixture.name);
        let (mut identical, mut max_lsb, mut total) = (0usize, 0i32, 0usize);
        for (frame, pair) in left.iter().zip(&right).enumerate() {
            for (c, value) in [*pair.0, *pair.1].into_iter().take(fixture.channels).enumerate() {
                let mine = (f64::from(value) * 32768.0).round() as i32;
                assert_eq!(f64::from(value) * 32768.0, f64::from(mine), "{}: output is not on the 16-bit grid", fixture.name);
                let diff = (mine - i32::from(apple[frame * fixture.channels + c])).abs();
                identical += usize::from(diff == 0);
                max_lsb = max_lsb.max(diff);
                total += 1;
            }
        }
        assert!(max_lsb <= 2, "{}: max difference {max_lsb} LSB", fixture.name);
        assert!(identical * 100 >= total * 80, "{}: only {identical}/{total} samples identical", fixture.name);
        if fixture.channels == 1 { assert_eq!(left, right, "{}: mono must be duplicated into the second plane", fixture.name); }
    }
}

#[test]
fn output_is_pinned_and_independent_of_the_read_size() {
    for (name, expected) in PINNED {
        let (left, right) = decode(name, 4096);
        assert_eq!(digest(&left, &right), expected, "{name}");
    }
    for name in ["lame_cbr128_44100_js", "lame_vbrV2_22050_js", "lame_cbr32_11025_mono"] {
        let reference = decode(name, 4096);
        for chunk in [1, 7, 576, 1152, 1153, 100_000] {
            assert!(decode(name, chunk) == reference, "{name}: read size {chunk} changes the output");
        }
    }
}

#[test]
fn reader_refuses_mp3_without_the_opt_in_and_decodes_with_it() {
    let path = mp3_path("lame_cbr128_44100_js");
    match Reader::open(&path, None) {
        Err(CodecError::UnsupportedCodec { codec: "MP3", .. }) => {}
        other => panic!("default open must refuse MP3, got {:?}", other.map(|_| ())),
    }
    let mut reader = Reader::open_with(&path, None, OPT_IN).unwrap();
    assert!(reader.non_identical());
    assert_eq!(reader.source_format().sample_rate, 44100.0);
    assert_eq!(reader.source_format().channels_per_frame, 2);
    assert_eq!(reader.source_format().frames_per_packet, 1152);
    assert_eq!(reader.file_length_frames().unwrap(), 13230);
    assert_eq!(reader.bitrate().unwrap(), 128_000);
    reader.set_stereo_planar(44100.0).unwrap();
    let (expected_left, expected_right) = decode("lame_cbr128_44100_js", 4096);
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let (mut l, mut r) = (vec![0f32; 1000], vec![0f32; 1000]);
    loop {
        let n = reader.read_stereo_planar(&mut l, &mut r).unwrap();
        if n == 0 { break; }
        left.extend_from_slice(&l[..n]);
        right.extend_from_slice(&r[..n]);
    }
    assert!(left == expected_left && right == expected_right);
}

#[test]
fn reader_interleaved_read_matches_the_stream_for_stereo_and_mono() {
    for (name, channels) in [("lame_cbr128_44100_js", 2usize), ("lame_cbr128_44100_mono", 1)] {
        let (left, right) = decode(name, 4096);
        let mut reader = Reader::open_with(mp3_path(name), None, OPT_IN).unwrap();
        let mut all = Vec::new();
        let mut buffer = vec![0f32; 512 * channels];
        loop {
            let n = reader.read(&mut buffer).unwrap();
            if n == 0 { break; }
            all.extend_from_slice(&buffer[..n * channels]);
        }
        assert_eq!(all.len(), left.len() * channels, "{name}");
        for (frame, chunk) in all.chunks_exact(channels).enumerate() {
            assert_eq!(chunk[0], left[frame], "{name} frame {frame}");
            if channels == 2 { assert_eq!(chunk[1], right[frame], "{name} frame {frame}"); }
        }
    }
}

#[test]
fn sample_rate_conversion_is_refused_not_approximated() {
    let path = mp3_path("lame_cbr128_44100_js");
    for rate in [48000.0, 22050.0, 96000.0] {
        match Reader::open_with(&path, Some(rate), OPT_IN) {
            Err(CodecError::UnsupportedSampleRateConversion { source_rate, client_rate }) => assert_eq!((source_rate, client_rate), (44100.0, rate)),
            other => panic!("rate {rate}: expected a refusal, got {:?}", other.map(|_| ())),
        }
    }
    assert!(Reader::open_with(&path, Some(44100.0), OPT_IN).is_ok());
}

fn write_temp(tag: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("platinum-mp3-{}-{tag}.mp3", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn unmatched_variants_are_refused_with_typed_errors() {
    let good = std::fs::read(mp3_path("tagless_cbr128_44100")).unwrap();
    let frame = 417usize;
    // Layer II header (MPEG-1, layer II, 128 kbit/s, 44.1 kHz): 144 * 128000 / 44100 = 417 bytes.
    let mut layer2 = vec![0xff, 0xfd, 0x80, 0x00];
    layer2.resize(417, 0);
    let mut data = layer2.repeat(4);
    let path = write_temp("layer2", &data);
    assert!(matches!(Mp3Stream::open(File::open(&path).unwrap()), Err(Mp3Error::Unsupported(_))), "layer II");
    // Free-format header (bitrate index 0).
    data = vec![0xff, 0xfb, 0x00, 0x00];
    data.resize(417, 0);
    let path = write_temp("free", &data.repeat(4));
    assert!(matches!(Mp3Stream::open(File::open(&path).unwrap()), Err(Mp3Error::Unsupported("free-format MPEG audio"))), "free format");
    // A file that is only a truncated first frame.
    let path = write_temp("truncated", &good[..frame / 2]);
    assert!(Mp3Stream::open(File::open(&path).unwrap()).is_err(), "half a frame");
    // Not an MP3 at all.
    let path = write_temp("text", b"this is not audio data, just text".repeat(40).as_slice());
    assert!(matches!(Mp3Stream::open(File::open(&path).unwrap()), Err(Mp3Error::NotMp3)));
    // Through the Reader the same inputs surface as typed CodecError values.
    let path = write_temp("layer2b", &layer2.repeat(4));
    assert!(matches!(Reader::open_with(&path, None, OPT_IN), Err(CodecError::UnsupportedCodec { codec: "MP3", .. })));
    for tag in ["layer2", "free", "truncated", "text", "layer2b"] {
        let _ = std::fs::remove_file(std::env::temp_dir().join(format!("platinum-mp3-{}-{tag}.mp3", std::process::id())));
    }
}

#[test]
fn mixed_blocks_and_mpeg2_intensity_stereo_are_refused() {
    // One MPEG-1 stereo frame whose side information marks channel 0 of granule 0 as a mixed block.
    let mut frame = vec![0u8; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
    // side info starts at byte 4: 9 bits main_data_begin + 3 private + 8 scfsi bits = 20 bits, then
    // granule 0 channel 0: 12+9+8+4 bits, window_switching_flag, block_type (2), mixed_block_flag.
    let set_bit = |frame: &mut Vec<u8>, bit: usize| frame[4 + bit / 8] |= 0x80 >> (bit % 8);
    let base = 20 + 12 + 9 + 8 + 4;
    set_bit(&mut frame, base);          // window_switching_flag
    set_bit(&mut frame, base + 1);      // block_type = 2 (binary 10)
    set_bit(&mut frame, base + 3);      // mixed_block_flag
    let path = write_temp("mixed", &frame.repeat(4));
    assert!(matches!(Mp3Stream::open(File::open(&path).unwrap()), Err(Mp3Error::Unsupported("mixed long/short blocks"))));
    // MPEG-2 (LSF) joint stereo with the intensity bit set.
    let mut lsf = vec![0u8; 313];
    lsf[..4].copy_from_slice(&[0xff, 0xf3, 0xa0, 0x50]); // MPEG-2, 96 kbit/s, 22.05 kHz, joint stereo, mode_extension 1
    let path2 = write_temp("lsfis", &lsf.repeat(4));
    let lsf_result = Mp3Stream::open(File::open(&path2).unwrap());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&path2);
    assert!(matches!(lsf_result, Err(Mp3Error::Unsupported("MPEG-2 intensity stereo"))), "{:?}", lsf_result.map(|_| ()));
}

#[test]
fn decode_ignores_the_ambient_flush_to_zero_mode() {
    for name in ["lame_cbr128_44100_js", "lame_loud_cbr128", "lame_cbr32_11025_mono"] {
        let normal = decode(name, 4096);
        let flushed = platinum_engine::fp_mode::with_flush_to_zero(|| decode(name, 4096));
        assert!(normal == flushed, "{name}: flush-to-zero changes the decode");
    }
}

#[test]
fn vbri_fixtures_match_recorded_audiotoolbox_properties_and_samples() {
    let vbri = dir().join("vbri");
    let manifest = read_json(&vbri.join("manifest.json"));
    let entries = manifest.as_array().unwrap();
    assert!(entries.len() >= 18, "VBRI fixture set shrank");
    for e in entries {
        let name = e["name"].as_str().unwrap();
        let path = vbri.join(format!("{name}.mp3"));
        let channels = e["channels"].as_u64().unwrap() as usize;
        let stream = Mp3Stream::open(File::open(&path).unwrap()).unwrap_or_else(|err| panic!("{name}: {err:?}"));
        let info = stream.info();
        assert_eq!(info.length_frames, e["apple_length_property"].as_i64().unwrap(), "{name}: length property");
        assert_eq!(i64::from(info.bitrate), e["apple_bitrate"].as_i64().unwrap(), "{name}: bitrate property");
        assert_eq!(info.channels as usize, channels, "{name}: channels");
        let (left, right) = decode_path(&path, 4096);
        let delivered = e["apple_frames_delivered"].as_u64().unwrap() as usize;
        assert_eq!(left.len(), delivered, "{name}: frames delivered");
        let apple = read_s16(&vbri.join(e["samples_file"].as_str().unwrap()));
        let (identical, max_lsb, total) = accuracy(name, &left, &right, channels, &apple);
        assert!(max_lsb <= 2, "{name}: max difference {max_lsb} LSB");
        assert!(identical * 100 >= total * 80, "{name}: only {identical}/{total} samples identical");
    }
}

#[test]
fn truncated_final_frame_is_accepted_at_every_byte_offset_like_audiotoolbox() {
    let truncation = dir().join("truncation");
    let manifest = read_json(&truncation.join("manifest.json"));
    let temp = std::env::temp_dir().join(format!("platinum-mp3-truncated-{}.mp3", std::process::id()));
    for e in manifest.as_array().unwrap() {
        let name = e["name"].as_str().unwrap();
        let size = e["last_frame_bytes"].as_u64().unwrap() as usize;
        let channels = e["channels"].as_u64().unwrap() as usize;
        let data = std::fs::read(mp3_path(name)).unwrap();
        let start = data.len() - size;
        let apple = read_s16(&truncation.join(e["samples_file"].as_str().unwrap()));
        for cut in 0..size {
            std::fs::write(&temp, &data[..start + cut]).unwrap();
            let mut stream = Mp3Stream::open(File::open(&temp).unwrap()).unwrap_or_else(|err| panic!("{name} cut at {cut}: {err:?}"));
            let info = stream.info();
            assert_eq!(info.length_frames, e["apple_length_property"].as_i64().unwrap(), "{name} cut at {cut}: length property");
            assert_eq!(i64::from(info.bitrate), e["apple_bitrate"].as_i64().unwrap(), "{name} cut at {cut}: bitrate property");
            assert_eq!(info.channels as usize, channels);
            // Full decodes are slow in debug builds: all the header-cut offsets, then a sample of the rest.
            if cut < 6 || cut % 37 == 0 || cut + 1 == size {
                let (left, right) = drain(&mut stream, 4096);
                assert_eq!(left.len(), e["apple_frames_delivered"].as_u64().unwrap() as usize, "{name} cut at {cut}: frames delivered");
                let (identical, max_lsb, total) = accuracy(name, &left, &right, channels, &apple);
                assert!(max_lsb <= 2 && identical * 100 >= total * 80, "{name} cut at {cut}: {identical}/{total} identical, max {max_lsb} LSB");
            }
        }
    }
    let _ = std::fs::remove_file(&temp);
}

#[test]
fn inputs_that_are_not_mp3_keep_their_classification_under_the_opt_in() {
    let adts = [vec![0xff, 0xf1, 0x50, 0x80, 0x1c, 0x1f, 0xfc], vec![0u8; 600]].concat();
    let mut flac = b"fLaC".to_vec();
    for _ in 0..1000 { flac.extend([0xff, 0xf8, 0xc9, 0x18, 0x00, 0x01, 0x02]); } // FLAC frame syncs
    // Pseudo-random bytes with many valid MPEG headers scattered through them (and a few at the start).
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || { state ^= state << 13; state ^= state >> 7; state ^= state << 17; (state >> 24) as u8 };
    let mut noise: Vec<u8> = (0..9000).map(|_| next()).collect();
    for at in (0..8000).step_by(97) { noise[at..at + 4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]); }
    let mut noise_from_sync = noise.clone();
    noise_from_sync[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
    let mut id3_noise = b"ID3\x03\x00\x00\x00\x00\x00\x10".to_vec();
    id3_noise.extend(&noise);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()), ("text", b"This is plain text, not audio. ".repeat(40)), ("zeros", vec![0u8; 5000]),
        ("adts aac", adts), ("ogg", [b"OggS".to_vec(), vec![0u8; 400]].concat()), ("flac", flac),
        ("truncated wave", b"RIFF\x24\x00\x00\x00WAVE".to_vec()), ("sync bytes after text", [b"abcdef".to_vec(), vec![0xff; 100]].concat()),
        ("noise with frame headers", noise), ("noise starting with a frame header", noise_from_sync), ("id3 then noise", id3_noise),
        ("late lookalikes", [vec![b'x'; 40], vec![0xff, 0xe3, 0x90, 0x00, 0xff, 0xfb]].concat()),
    ];
    for (name, bytes) in cases {
        let path = write_temp("classify", &bytes);
        let plain = Reader::open(&path, None).err().map(|e| format!("{e:?}"));
        let opted_in = Reader::open_with(&path, None, OPT_IN).err().map(|e| format!("{e:?}"));
        let _ = std::fs::remove_file(&path);
        assert!(plain.is_some(), "{name}: expected the default reader to refuse");
        assert_eq!(plain, opted_in, "{name}: the opt-in changed the error");
    }
}

#[test]
fn junk_prefixed_mp3_decodes_with_the_opt_in_and_is_refused_without() {
    let good = std::fs::read(mp3_path("lame_cbr128_44100_js")).unwrap();
    // 3000 bytes that contain no sync byte at all.
    let junk: Vec<u8> = (0..3000u32).map(|i| ((i * 37 + 11) % 251) as u8).collect();
    let prefixed = write_temp("junk-prefixed", &[junk.clone(), good.clone()].concat());
    assert!(matches!(Reader::open(&prefixed, None), Err(CodecError::UnsupportedFormat(_))), "default classification");
    let reference = decode("lame_cbr128_44100_js", 4096);
    let mut stream = Mp3Stream::open(File::open(&prefixed).unwrap()).unwrap();
    assert_eq!(drain(&mut stream, 4096), reference);
    let reader = Reader::open_with(&prefixed, None, OPT_IN).unwrap();
    assert_eq!(reader.file_length_frames().unwrap(), 13230);
    let _ = std::fs::remove_file(&prefixed);
    // A header-like pattern inside the junk is ambiguous: refused with a typed error, never guessed.
    let mut ambiguous = junk[..100].to_vec();
    ambiguous.extend([0xff, 0xfb, 0x90, 0x00]);
    ambiguous.extend(&junk[100..]);
    ambiguous.extend(&good);
    let path = write_temp("junk-ambiguous", &ambiguous);
    match Reader::open_with(&path, None, OPT_IN) {
        Err(CodecError::UnsupportedCodec { codec: "MP3", reason: "ambiguous data before the first MP3 frame", .. }) => {}
        other => panic!("expected the ambiguity refusal, got {:?}", other.map(|_| ())),
    }
    let _ = std::fs::remove_file(&path);
}
