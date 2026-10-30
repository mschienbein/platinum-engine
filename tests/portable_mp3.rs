//! The portable MP3 reader against what AudioToolbox delivered, replayed from the golden fixtures.
//!
//! `tests/fixtures/mp3-exact/` holds generated MP3 inputs and, for each, the macOS arm64 reference recorded
//! through the engine's own `apple_codec::Reader` (see its README): whether `ExtAudioFileOpenURL` accepted the
//! file and with which `OSStatus` it refused, the source description, the file-length and bitrate properties, and
//! the SHA-256 of the float32 stream of 512-frame reads (planar stereo, and interleaved at the source channel
//! count), including the frames delivered before a read failed and that failure's `OSStatus`. `readsizes.json` adds
//! the same recordings at 100-, 513- and 4096-frame reads for the files whose result depends on the read size (every
//! read failure, every stream change) and a sample of clean streams. Everything is compared bit for bit through the
//! public `Reader`; no Apple component is needed at run time, so the same test runs unchanged on macOS
//! (`--features portable`), Linux arm64 and Linux x86_64.

#[path = "support/mp3_exact.rs"]
mod mp3_exact;

use mp3_exact::{BLOCK, Failure, Fixture, Fixtures, ReadSizes, Scratch, decode_interleaved, decode_interleaved_block, decode_planar, decode_planar_block, failure, hex, open};
use platinum_engine::portable::audio::{CodecError, Reader};
use platinum_engine::portable::digest::sha256;
use serde_json::json;
use std::fs;

const DTA: i32 = 0x6474_613f; // 'dta?'
const WHT: i32 = 0x7768_743f; // 'wht?'
const EOF: i32 = -39;
const FMT: i32 = 0x666d_743f; // 'fmt?'
const BADA: i32 = 0x6261_6461; // 'bada'
const OPEN: &str = "ExtAudioFileOpenURL";
const CLIENT: &str = "ExtAudioFileSetProperty(client format)";

/// Write the fixture to a scratch file and open it as the engine does.
fn open_fixture(scratch: &Scratch, fixtures: &Fixtures, index: usize, fixture: &Fixture) -> Result<Reader, CodecError> {
    open(&scratch.write(&format!("{index:04}.mp3"), fixtures.bytes(fixture)))
}

#[test]
fn the_fixture_pack_is_intact_and_covers_what_it_claims() {
    let fixtures = Fixtures::load();
    let manifest = &fixtures.manifest;
    assert_eq!(hex(&sha256(&fixtures.pack)), manifest["pack"]["sha256"].as_str().unwrap(), "pack.bin does not match the manifest");
    assert_eq!(fixtures.pack.len() as u64, manifest["pack"]["bytes"].as_u64().unwrap());
    let mut end = 0usize;
    for fixture in &fixtures.files {
        assert_eq!(fixture.offset, end, "{}: the pack is not contiguous", fixture.id);
        end += fixture.length;
        assert!(fixture.length <= 40_000, "{}: fixtures are small", fixture.id);
        assert_eq!(hex(&sha256(fixtures.bytes(fixture))), fixture.sha256, "{}: input bytes", fixture.id);
    }
    assert_eq!(end, fixtures.pack.len());
    assert!(fixtures.files.windows(2).all(|pair| pair[0].id < pair[1].id), "manifest is sorted by id");
    assert!(fixtures.pack.len() <= 5_000_000, "the fixture pack stays small");

    assert!(fixtures.files.len() >= 280, "{} fixtures", fixtures.files.len());
    let refused: Vec<i32> = fixtures.files.iter().filter_map(|f| f.open_error.as_ref().map(|e| e.status)).collect();
    for status in [DTA, WHT, EOF, FMT] { assert!(refused.contains(&status), "no fixture refused with {status:#x}"); }
    let failed: Vec<i32> = fixtures.files.iter().filter_map(|f| f.planar512.as_ref()?.error.as_ref().map(|e| e.status)).collect();
    for status in [DTA, BADA] { assert!(failed.contains(&status), "no fixture whose read fails with {status:#x}"); }
    let mut rates: Vec<u32> = fixtures.files.iter().filter_map(|f| f.info.as_ref().map(|i| i.sample_rate)).collect();
    rates.sort_unstable();
    rates.dedup();
    assert_eq!(rates, [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000]);
    let formats: Vec<u32> = { let mut f: Vec<u32> = fixtures.files.iter().filter_map(|f| f.info.as_ref().map(|i| i.format_id)).collect(); f.sort_unstable(); f.dedup(); f };
    assert_eq!(formats, [0x2e6d_7031, 0x2e6d_7032, 0x2e6d_7033], "MP1, MP2 and MP3 all open");
    assert!(fixtures.files.iter().any(|f| f.info.as_ref().is_some_and(|i| i.channels == 1)));
    assert!(fixtures.files.iter().any(|f| f.info.as_ref().is_some_and(|i| i.channels == 2)));
}

#[test]
fn every_fixture_is_bit_exact_with_the_apple_reference() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("golden");
    let mut mismatches = Vec::new();
    let (mut opened, mut refused, mut failed_reads, mut frames) = (0usize, 0usize, 0usize, 0u64);
    for (index, fixture) in fixtures.files.iter().enumerate() {
        let path = scratch.write(&format!("{index:04}.mp3"), fixtures.bytes(fixture));
        let mut bad = |what: &str, expected: String, actual: String| mismatches.push(format!("{}: {what}: expected {expected}, got {actual}", fixture.id));
        let reader = match open(&path) {
            Ok(reader) => {
                if let Some(expected) = &fixture.open_error { bad("open", format!("{expected:?}"), "opened".into()); continue; }
                reader
            }
            Err(error) => {
                let actual = failure(error);
                if fixture.open_error.as_ref() != Some(&actual) { bad("open", format!("{:?}", fixture.open_error), format!("{actual:?}")); }
                refused += 1;
                continue;
            }
        };
        opened += 1;
        let info = fixture.info.as_ref().unwrap();
        let source = reader.source_format();
        let got = (source.sample_rate, source.format_id, source.format_flags, source.bytes_per_packet, source.frames_per_packet,
            source.bytes_per_frame, source.channels_per_frame, source.bits_per_channel);
        let want = (f64::from(info.sample_rate), info.format_id, 0, 0, info.frames_per_packet, 0, info.channels, 0);
        if got != want { bad("source format", format!("{want:?}"), format!("{got:?}")); }
        let length = reader.file_length_frames().unwrap();
        if length != info.length_frames { bad("file length", info.length_frames.to_string(), length.to_string()); }
        let bitrate = reader.bitrate().unwrap();
        if bitrate != info.bitrate { bad("bitrate", info.bitrate.to_string(), bitrate.to_string()); }
        let client = reader.client_format();
        if (client.sample_rate, client.channels_per_frame, client.format_flags, client.bits_per_channel)
            != (f64::from(info.sample_rate), info.channels, 9, 32) {
            bad("client format", format!("{} Hz {} ch float", info.sample_rate, info.channels), format!("{client:?}"));
        }
        reader.close().unwrap();

        let mut planar_reader = open(&path).unwrap();
        let planar = decode_planar(&mut planar_reader);
        if Some(&planar) != fixture.planar512.as_ref() { bad("planar 512 stream", format!("{:?}", fixture.planar512), format!("{planar:?}")); }
        let mut inter_reader = open(&path).unwrap();
        let inter = decode_interleaved(&mut inter_reader);
        if Some(&inter) != fixture.inter512.as_ref() { bad("interleaved 512 stream", format!("{:?}", fixture.inter512), format!("{inter:?}")); }
        failed_reads += usize::from(planar.error.is_some());
        frames += planar.frames;
    }
    println!("{} fixtures: {opened} opened, {refused} refused at open, {failed_reads} with a read failure, {frames} frames compared", fixtures.files.len());
    assert!(mismatches.is_empty(), "{} mismatches against the Apple reference:\n{}", mismatches.len(), mismatches.iter().take(25).cloned().collect::<Vec<_>>().join("\n"));
    assert!(opened > 150 && refused > 15 && failed_reads > 10 && frames > 1_000_000, "the fixtures must exercise open, refusal and read failure");
}

/// Apple's result depends on the read size when a read fails (the frames the failed read had gathered are lost, and the
/// packet groups the converter asks for shift): 254 of the 7,100 + 2,300 + 6,440 corpus files differ between 512 and 4096 frames.
/// The recordings below are `apple_probe dump ... planar|inter BLOCK` of the engine's own macOS reader, and the reader
/// reproduces all of them: delivered frames, the failing call and its `OSStatus`, and every sample.
#[test]
fn read_sizes_other_than_512_match_the_apple_recordings() {
    let fixtures = Fixtures::load();
    let sizes = ReadSizes::load();
    assert_eq!(sizes.document["pack_sha256"], fixtures.manifest["pack"]["sha256"], "recorded against another pack");
    assert_eq!(sizes.document["block_sizes"], json!([100, 513, 4096]));
    assert_eq!(sizes.document["apple"]["probe_sha256"], fixtures.manifest["apple"]["probe_sha256"]);
    let scratch = Scratch::new("readsizes");
    let (mut records, mut differing, mut with_error, mut mismatches) = (0usize, 0usize, 0usize, Vec::new());
    for file in &sizes.files {
        let (index, fixture) = fixtures.files.iter().enumerate().find(|(_, f)| f.id == file.id).unwrap_or_else(|| panic!("no fixture {}", file.id));
        assert_eq!(file.sha256, fixture.sha256, "{}", file.id);
        let at_512 = (fixture.planar512.as_ref().unwrap(), fixture.inter512.as_ref().unwrap());
        let path = scratch.write(&format!("{index:04}.mp3"), fixtures.bytes(fixture));
        for (shape, rows, reference) in [("planar", &file.planar, at_512.0), ("interleaved", &file.inter, at_512.1)] {
            assert_eq!(rows.len(), 3, "{}: {shape} sizes", file.id);
            for (block, expected) in rows {
                let mut reader = open(&path).unwrap();
                let actual = if shape == "planar" { decode_planar_block(&mut reader, *block) } else { decode_interleaved_block(&mut reader, *block) };
                records += 1;
                differing += usize::from(expected != reference);
                with_error += usize::from(expected.error.is_some());
                if &actual != expected {
                    mismatches.push(format!("{} {shape} {block}: expected {expected:?}, got {actual:?}", file.id));
                }
            }
        }
    }
    println!("{} files, {records} recordings ({differing} differ from the 512-frame result, {with_error} end in a read failure)", sizes.files.len());
    assert!(mismatches.is_empty(), "{} mismatches against the Apple recordings:\n{}", mismatches.len(), mismatches.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
    assert!(sizes.files.len() >= 50 && records >= 300, "{} files, {records} recordings", sizes.files.len());
    assert!(differing >= 60, "only {differing} recordings differ from the 512-frame ones: the test would not detect a size-independent decoder");
}

#[test]
fn files_apple_refuses_are_refused_with_apples_status_whatever_the_client_rate() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("refused");
    let mut checked = 0;
    for (index, fixture) in fixtures.files.iter().enumerate().filter(|(_, f)| !f.opens()) {
        let expected = fixture.open_error.as_ref().unwrap();
        assert!(expected.operation == OPEN || expected.operation == CLIENT, "{}: {expected:?}", fixture.id);
        let path = scratch.write(&format!("{index:04}.mp3"), fixtures.bytes(fixture));
        for rate in [None, Some(44_100.0), Some(48_000.0), Some(8_000.0)] {
            let actual = Reader::open(&path, rate).map(|_| ()).map_err(failure);
            assert_eq!(actual, Err(expected.clone()), "{} at {rate:?}", fixture.id);
        }
        checked += 1;
    }
    assert!(checked >= 20, "{checked} refused fixtures");
}

#[test]
fn short_and_non_mp3_files_are_refused_as_an_mp3_open_not_as_an_io_failure() {
    let scratch = Scratch::new("short");
    let empty = scratch.write("empty.mp3", b"");
    assert_eq!(open(&empty).map(|_| ()).map_err(failure), Err(Failure { operation: OPEN.into(), status: WHT }));
    for (name, bytes) in [("one", &b"\xff"[..]), ("sync", b"\xff\xfb"), ("text", b"not audio at all, just text"), ("zeros", &[0u8; 64])] {
        let path = scratch.write(&format!("{name}.mp3"), bytes);
        let error = open(&path).map(|_| ()).map_err(failure).expect_err(name);
        assert_eq!(error.operation, OPEN, "{name}");
        assert!([DTA, EOF, WHT].contains(&error.status), "{name}: {error:?}");
    }
    // The file name decides which reader looks. An ADTS stream named .mp3 is MPEG data with no frame header in it (Apple: 'dta?');
    // under any other name AudioFile recognises ADTS, which the AAC reader opens: this one announces a frame of 13 bytes and holds 12, so
    // it has no frame at all ('wht?').
    let adts_bytes = [0xff, 0xf9, 0x50, 0xa0, 0x01, 0xa0, 0x00, 0x21, 0x20, 0x03, 0x40, 0x68];
    let adts = scratch.write("adts.mp3", &adts_bytes);
    assert_eq!(open(&adts).map(|_| ()).map_err(failure), Err(Failure { operation: OPEN.into(), status: DTA }));
    let adts = scratch.write("adts.bin", &adts_bytes);
    assert_eq!(open(&adts).map(|_| ()).map_err(failure), Err(Failure { operation: OPEN.into(), status: WHT }));
    let mut wav = b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0".to_vec();
    wav.extend_from_slice(&[0x55, 0, 2, 0, 0x44, 0xac, 0, 0, 0x10, 0xb1, 2, 0, 1, 0, 0, 0]); // tag 0x55: MPEG layer III in WAVE
    wav.extend_from_slice(b"data\0\0\0\0");
    let in_wave = scratch.write("in-wave.wav", &wav);
    match open(&in_wave) {
        Err(CodecError::UnsupportedCodec { codec: "MP3", operation: "decoding", reason }) => assert!(reason.contains("WAVE"), "{reason}"),
        other => panic!("MP3 in a WAVE container must stay refused: {:?}", other.map(|_| ())),
    }
    let flac_named = scratch.write("song.flac", b"fLaC");
    assert!(matches!(open(&flac_named), Err(CodecError::UnsupportedFormat(_))), "the FLAC path suffix keeps its refusal");
}

#[test]
fn a_client_rate_that_differs_from_the_file_is_refused_never_resampled() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("rates");
    let (index, fixture) = fixtures.files.iter().enumerate()
        .find(|(_, f)| f.info.as_ref().is_some_and(|i| i.sample_rate == 44_100 && i.channels == 2 && i.format_id == 0x2e6d_7033) && f.planar512.as_ref().is_some_and(|s| s.error.is_none()))
        .expect("a 44.1 kHz stereo fixture");
    let path = scratch.write("a.mp3", fixtures.bytes(fixture));
    let _ = index;
    for rate in [48_000.0, 22_050.0, 96_000.0] {
        match Reader::open(&path, Some(rate)) {
            Err(CodecError::UnsupportedSampleRateConversion { source_rate, client_rate }) => assert_eq!((source_rate, client_rate), (44_100.0, rate)),
            other => panic!("rate {rate}: expected a refusal, got {:?}", other.map(|_| ())),
        }
    }
    for rate in [f64::NAN, 0.0, -44_100.0, f64::INFINITY] {
        assert!(matches!(Reader::open(&path, Some(rate)), Err(CodecError::InvalidInput(_))), "{rate}");
    }
    let mut reader = Reader::open(&path, Some(44_100.0)).unwrap();
    assert!(matches!(reader.set_stereo_planar(48_000.0), Err(CodecError::UnsupportedSampleRateConversion { source_rate: 44_100.0, client_rate: 48_000.0 })));
    assert_eq!(reader.client_format().format_flags, 9, "a refused setter leaves the client format alone");
    reader.set_stereo_planar(44_100.0).unwrap();
    let client = reader.client_format();
    assert_eq!((client.format_flags, client.channels_per_frame, client.bytes_per_frame), (0x29, 2, 4));
}

#[test]
fn a_failed_read_poisons_the_reader_and_loses_its_frames() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("poison");
    let mut cases = 0;
    for (index, fixture) in fixtures.files.iter().enumerate() {
        let (Some(planar), Some(inter)) = (&fixture.planar512, &fixture.inter512) else { continue };
        for (is_planar, expected) in [(true, planar), (false, inter)] {
            let Some(error) = &expected.error else { continue };
            let mut reader = open_fixture(&scratch, fixtures, index, fixture).unwrap();
            let actual = if is_planar { decode_planar(&mut reader) } else { decode_interleaved(&mut reader) };
            assert_eq!(actual.error.as_ref(), Some(error), "{}", fixture.id);
            assert_eq!(actual.frames % BLOCK as u64, 0, "{}: the frames of the failed read are not delivered", fixture.id);
            let (mut left, mut right) = ([7.0f32; 4], [7.0f32; 4]);
            for _ in 0..2 {
                assert!(matches!(reader.read_stereo_planar(&mut left, &mut right), Err(CodecError::Poisoned)), "{}", fixture.id);
                assert!(matches!(reader.read(&mut [0.0; 4]), Err(CodecError::Poisoned)), "{}", fixture.id);
                assert!(matches!(reader.read(&mut []), Err(CodecError::Poisoned)), "{}", fixture.id);
                assert!(matches!(reader.set_stereo_planar(reader.source_format().sample_rate), Err(CodecError::Poisoned)), "{}", fixture.id);
            }
            assert_eq!((left, right), ([7.0; 4], [7.0; 4]), "a poisoned read writes nothing");
            cases += 1;
        }
    }
    assert!(cases >= 20, "{cases} failing reads");
}

#[test]
fn reads_of_any_size_deliver_the_same_stream_when_nothing_fails() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("chunks");
    let mut checked = 0;
    for (index, fixture) in fixtures.files.iter().enumerate() {
        let (Some(planar), Some(inter)) = (&fixture.planar512, &fixture.inter512) else { continue };
        if planar.error.is_some() || inter.error.is_some() || planar.frames == 0 { continue; }
        if checked >= 40 { break; }
        let channels = fixture.info.as_ref().unwrap().channels as usize;
        for chunk in [1usize, 7, 576, 1152, 1153, 4096, 100_000] {
            let mut reader = open_fixture(&scratch, fixtures, index, fixture).unwrap();
            let rate = reader.source_format().sample_rate;
            reader.set_stereo_planar(rate).unwrap();
            let (mut left, mut right) = (vec![0f32; chunk], vec![0f32; chunk]);
            let mut hash = platinum_engine::portable::digest::Sha256::new();
            loop {
                let n = reader.read_stereo_planar(&mut left, &mut right).unwrap();
                if n == 0 { break; }
                let mut bytes = Vec::with_capacity(n * 8);
                for i in 0..n { bytes.extend_from_slice(&left[i].to_le_bytes()); bytes.extend_from_slice(&right[i].to_le_bytes()); }
                hash.update(&bytes);
            }
            assert_eq!(hex(&hash.finalize()), planar.sha256, "{}: planar reads of {chunk} frames", fixture.id);

            let mut reader = open_fixture(&scratch, fixtures, index, fixture).unwrap();
            let mut buffer = vec![0f32; chunk * channels];
            let mut hash = platinum_engine::portable::digest::Sha256::new();
            loop {
                let n = reader.read(&mut buffer).unwrap();
                if n == 0 { break; }
                let mut bytes = Vec::with_capacity(n * channels * 4);
                for sample in &buffer[..n * channels] { bytes.extend_from_slice(&sample.to_le_bytes()); }
                hash.update(&bytes);
            }
            assert_eq!(hex(&hash.finalize()), inter.sha256, "{}: interleaved reads of {chunk} frames", fixture.id);
        }
        checked += 1;
    }
    assert_eq!(checked, 40);
}

#[test]
fn misuse_is_reported_like_the_other_readers_do() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("misuse");
    let (index, fixture) = fixtures.files.iter().enumerate()
        .find(|(_, f)| f.info.as_ref().is_some_and(|i| i.channels == 2) && f.planar512.as_ref().is_some_and(|s| s.frames > 2048 && s.error.is_none()))
        .expect("a stereo fixture");
    let mut reader = open_fixture(&scratch, fixtures, index, fixture).unwrap();
    assert_eq!(reader.read(&mut []).unwrap(), 0, "an empty buffer reads nothing");
    assert!(matches!(reader.read(&mut [0.0; 3]), Err(CodecError::InvalidInput(_))), "interleaved buffers hold whole frames");
    assert!(matches!(reader.read_stereo_planar(&mut [0.0; 4], &mut [0.0; 4]), Err(CodecError::InvalidInput(_))), "planar reads need the planar client format");
    reader.set_stereo_planar(reader.source_format().sample_rate).unwrap();
    assert!(matches!(reader.read(&mut [0.0; 4]), Err(CodecError::InvalidInput(_))), "interleaved reads are refused once planar");
    assert!(matches!(reader.read_stereo_planar(&mut [0.0; 4], &mut [0.0; 5]), Err(CodecError::InvalidInput(_))), "planes of different length");
    assert_eq!(reader.read_stereo_planar(&mut [], &mut []).unwrap(), 0);
    let (mut left, mut right) = ([9.0f32; 600], [9.0f32; 600]);
    let n = reader.read_stereo_planar(&mut left[..10], &mut right[..10]).unwrap();
    assert_eq!(n, 10);
    assert!(left[10..].iter().chain(&right[10..]).all(|v| *v == 9.0), "the tail beyond the returned frames is untouched");
    reader.close().unwrap();
}

#[test]
fn an_input_larger_than_2_gib_is_refused_before_it_is_read() {
    let scratch = Scratch::new("huge");
    let path = scratch.0.join("huge.mp3");
    let file = fs::File::create(&path).unwrap();
    file.set_len((1 << 31) + 1).unwrap(); // sparse: nothing is written
    match Reader::open(&path, None) {
        Err(CodecError::UnsupportedFormat(message)) => assert!(message.contains("2 GiB"), "{message}"),
        other => panic!("expected the size refusal, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn decode_ignores_the_ambient_flush_to_zero_mode() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("fz");
    let mut checked = 0;
    for (index, fixture) in fixtures.files.iter().enumerate()
        .filter(|(_, f)| f.features.iter().any(|t| t.starts_with("block:short") || t == "family:loud_overs" || t == "family:p_loud" || t == "family:p_overflow")).take(8) {
        let Some(expected) = &fixture.planar512 else { continue };
        let path = scratch.write(&format!("{index}.mp3"), fixtures.bytes(fixture));
        let flushed = platinum_engine::fp_mode::with_flush_to_zero(|| decode_planar(&mut open(&path).unwrap()));
        assert_eq!(&flushed, expected, "{}: flush-to-zero changes the decode", fixture.id);
        checked += 1;
    }
    assert!(checked >= 4, "{checked} fixtures with loud or short-block content");
}

/// Decode speed on the fixtures, for the integration notes: `cargo test --release --test portable_mp3 -- --ignored --nocapture`.
#[test]
#[ignore = "timing report, run with --release"]
fn decode_speed_report() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("speed");
    let mut paths = Vec::new();
    let mut seconds_of_audio = 0.0f64;
    for (index, fixture) in fixtures.files.iter().enumerate() {
        let Some(planar) = &fixture.planar512 else { continue };
        if planar.error.is_some() { continue; }
        paths.push(scratch.write(&format!("{index}.mp3"), fixtures.bytes(fixture)));
        seconds_of_audio += planar.frames as f64 / f64::from(fixture.info.as_ref().unwrap().sample_rate);
    }
    // Open (which reads the file and scans its packets), decode every planar 512-frame block and fold the samples; no hashing.
    let start = std::time::Instant::now();
    let mut fold = 0u32;
    for _ in 0..5 {
        for path in &paths {
            let mut reader = open(path).unwrap();
            let rate = reader.source_format().sample_rate;
            reader.set_stereo_planar(rate).unwrap();
            let (mut left, mut right) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
            while reader.read_stereo_planar(&mut left, &mut right).unwrap() == BLOCK {
                fold ^= left[0].to_bits() ^ right[BLOCK - 1].to_bits();
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!("{} files, {:.1} s of audio x5 in {:.3} s: {:.0}x real time on one core (fold {fold:08x})", paths.len(), seconds_of_audio, elapsed, seconds_of_audio * 5.0 / elapsed);
}

// ---- which reader opens a file: AudioFile's type selection, recorded on macOS ------------------------------------------------

#[path = "support/admission.rs"]
mod admission;

/// `describe_open`'s grammar is the recording's: `ok/<rate>/<format id>/<channels>/<frames per packet>/<length>/<bitrate>` or
/// `err/<open|client>/<OSStatus>` (the failing call), anything else is `other/<message>`.
fn describe_open(result: &Result<Reader, CodecError>) -> String {
    match result {
        Ok(reader) => {
            let s = reader.source_format();
            format!("ok/{}/{}/{}/{}/{}/{}", s.sample_rate as u64, s.format_id, s.channels_per_frame, s.frames_per_packet,
                reader.file_length_frames().unwrap(), reader.bitrate().unwrap())
        }
        Err(CodecError::Native { operation, status }) => {
            let call = match *operation { OPEN => "open", CLIENT => "client", other => other };
            format!("err/{call}/{status}")
        }
        Err(other) => format!("other/{other}"),
    }
}

#[test]
fn file_type_admission_follows_audiofile_for_every_recorded_name_and_content() {
    let fixtures = Fixtures::load();
    let document = admission::load("admission.json", "platinum-mp3-admission-v1");
    let scratch = Scratch::new("admission");
    let contents = document["contents"].as_object().unwrap();
    let cases = document["cases"].as_array().unwrap();
    assert!(cases.len() >= 450, "{} recorded cases", cases.len());
    let mut bytes_of = std::collections::HashMap::new();
    let (mut mismatches, mut kinds) = (Vec::new(), std::collections::BTreeMap::<String, usize>::new());
    for case in cases {
        let (content, extension, apple, kind) = (case[0].as_str().unwrap(), case[1].as_str().unwrap(), case[2].as_str().unwrap(), case[3].as_str().unwrap());
        let bytes = bytes_of.entry(content).or_insert_with(|| admission::build(&contents[content], &document, fixtures));
        let name = if extension.is_empty() { "case".to_owned() } else { format!("case.{extension}") };
        let result = Reader::open(scratch.write(&name, bytes), None);
        // What AudioToolbox answered for the file, without the outcome of the second open the recording also made.
        let apple_open: String = apple.splitn(8, '/').take(7).collect::<Vec<_>>().join("/");
        let ok = match kind {
            "same" => describe_open(&result) == apple_open,
            // recorded when AAC was refused; the AAC reader answers as AudioToolbox does
            "aac" => describe_open(&result) == apple_open,
            "flac" => matches!(&result, Err(CodecError::UnsupportedCodec { codec: "FLAC", operation: "decoding", .. })),
            "ogg" => matches!(&result, Err(CodecError::UnsupportedFormat(m)) if m.contains("Ogg")),
            "wave-behind-tag" => matches!(&result, Err(CodecError::UnsupportedFormat(m)) if m.contains("behind a leading ID3 tag")),
            // The malformed WAVE is refused with `InvalidInput`, which the engine reports like AudioToolbox's `'dta?'`; a malformed MP4 or CAF gets the
            // status itself from the container model of the AAC reader.
            "invalid-input" => describe_open(&result) == apple_open || (matches!(&result, Err(CodecError::InvalidInput(_))) && apple == "err/open/1685348671"),
            other => panic!("unknown kind {other}"),
        };
        *kinds.entry(kind.to_owned()).or_default() += 1;
        if !ok { mismatches.push(format!("{content}.{extension}: Apple {apple}; expected {kind}; got {}", describe_open(&result))); }
    }
    assert!(mismatches.is_empty(), "{} of {} cases differ:\n{}", mismatches.len(), cases.len(), mismatches.join("\n"));
    assert!(kinds["same"] >= 400, "{kinds:?}");
}

#[test]
fn an_mpeg_extension_commits_to_the_mpeg_reader_whatever_the_content() {
    let scratch = Scratch::new("hint");
    let wav = {
        let document = admission::load("admission.json", "platinum-mp3-admission-v1");
        admission::build(&json!({"wav": [44100, 2, 16, 300]}), &document, Fixtures::load())
    };
    for name in ["a.mp3", "a.MP3", "a.Mp3", "a.mp2", "a.mp1", "a.mpa", "a.mpeg"] {
        // Not MPEG: no frame header anywhere, the MPEG reader's own refusal and nothing else looks at the data.
        match Reader::open(scratch.write(name, &wav), None) {
            Err(CodecError::Native { operation: OPEN, status: DTA }) => {}
            other => panic!("{name}: {:?}", other.err()),
        }
    }
    for name in ["a.wav", "a.aif", "a.aiff", "a.m4a", "a.bin", "a"] {
        assert!(Reader::open(scratch.write(name, &wav), None).is_ok(), "{name}: a WAVE is still a WAVE under any other name");
    }
    // The same stream is an MPEG stream under every name its first frame header lets AudioFile sniff.
    let fixtures = Fixtures::load();
    let stream = fixtures.bytes(fixtures.get("corpus/quality_q/q2_abr128"));
    let expected = describe_open(&Reader::open(scratch.write("s.mp3", stream), None));
    assert!(expected.starts_with("ok/44100/"), "{expected}");
    for name in ["s.wav", "s.aif", "s.aiff", "s.m4a", "s.bin", "s"] {
        assert_eq!(describe_open(&Reader::open(scratch.write(name, stream), None)), expected, "{name}");
    }
}

/// Every read of `block` frames that returned fewer, as the recording describes it: the sentinel-filled buffer is searched
/// for what the reader wrote behind the returned frames. `(frames returned in total, [(returned, written behind, hash)], error)`.
fn tail_probe(path: &std::path::Path, block: usize, planar: bool) -> (u64, Vec<(usize, usize, String)>, Option<String>) {
    use admission::{FNV_START, SENTINEL, fold};
    let rate = Reader::open(path, None).unwrap().source_format().sample_rate;
    let mut reader = Reader::open(path, Some(rate)).unwrap();
    let channels = reader.source_format().channels_per_frame as usize;
    if planar { reader.set_stereo_planar(rate).unwrap(); }
    let (mut left, mut right, mut inter) = (vec![SENTINEL; block], vec![SENTINEL; block], vec![SENTINEL; block * channels]);
    let (mut total, mut shorts, mut error) = (0u64, Vec::new(), None);
    for _ in 0..100_000 {
        left.fill(SENTINEL);
        right.fill(SENTINEL);
        inter.fill(SENTINEL);
        let read = if planar { reader.read_stereo_planar(&mut left, &mut right) } else { reader.read(&mut inter) };
        let n = match read { Ok(n) => n, Err(e) => { error = Some(e.to_string()); break; } };
        total += n as u64;
        if n < block {
            let (mut written, mut hash) = (0usize, FNV_START);
            if planar {
                for i in n..block { if left[i] != SENTINEL || right[i] != SENTINEL { written = i + 1 - n; } }
                for i in n..n + written { hash = fold(hash, left[i].to_bits()); hash = fold(hash, right[i].to_bits()); }
            } else {
                for i in n * channels..block * channels { if inter[i] != SENTINEL { written = (i + channels) / channels - n; } }
                for v in &inter[n * channels..(n + written) * channels] { hash = fold(hash, v.to_bits()); }
            }
            shorts.push((n, written, format!("{hash:016x}")));
            if n == 0 { break; }
        }
    }
    (total, shorts, error)
}

#[test]
fn the_decoders_padding_behind_the_valid_frames_lands_in_the_callers_buffer_like_on_macos() {
    let fixtures = Fixtures::load();
    let document = admission::load("tails.json", "platinum-mp3-tails-v1");
    let scratch = Scratch::new("tails");
    let records = document["records"].as_array().unwrap();
    let (mut mismatches, mut with_tail, mut zero_reads_with_tail) = (Vec::new(), 0usize, 0usize);
    for record in records {
        let (id, cut, block, shape) = (record[0].as_str().unwrap(), record[1].as_u64(), record[2].as_u64().unwrap() as usize, record[3].as_str().unwrap());
        let mut bytes = fixtures.bytes(fixtures.get(id)).to_vec();
        if let Some(cut) = cut { bytes.truncate(cut as usize); }
        let (total, shorts, error) = tail_probe(&scratch.write("tail.mp3", &bytes), block, shape == "p");
        let expected: Vec<(usize, usize, String)> = record[5].as_array().unwrap().iter()
            .map(|s| (s[0].as_u64().unwrap() as usize, s[1].as_u64().unwrap() as usize, s[2].as_str().unwrap().to_owned())).collect();
        if total != record[4].as_u64().unwrap() || shorts != expected || error.as_deref() != record[6].as_str() {
            mismatches.push(format!("{id} prefix {cut:?} block {block} {shape}: expected {} frames {expected:?}, got {total} {shorts:?} {error:?}", record[4]));
        }
        with_tail += usize::from(expected.iter().any(|s| s.1 > 0));
        zero_reads_with_tail += usize::from(expected.iter().any(|s| s.0 == 0 && s.1 > 0));
    }
    assert!(mismatches.is_empty(), "{} of {} records differ:\n{}", mismatches.len(), records.len(), mismatches.join("\n"));
    assert!(with_tail >= 100 && zero_reads_with_tail >= 50, "{with_tail} records with a written tail, {zero_reads_with_tail} behind a read that returned 0");
}
