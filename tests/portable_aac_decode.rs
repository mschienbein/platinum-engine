//! The portable AAC reader against what AudioToolbox delivered, replayed from the golden fixtures.
//!
//! `tests/fixtures/aac-decode/` holds generated AAC inputs (MP4/M4A, ADTS, LOAS, CAF, mutated and misnamed ones, plus HE-AAC,
//! LD/ELD and the other codecs Apple decodes that the port does not) and, for each, the macOS arm64 reference recorded through
//! the engine's own `apple_codec::Reader` (see its README): whether `ExtAudioFileOpenURL` accepted the file under the name it
//! has here and with which `OSStatus` it refused, the source description, the file-length and bitrate properties, and per
//! shape (planar stereo, interleaved at the source channel count) the frames delivered, the read failure that ended the stream,
//! the SHA-256 of the float32 stream, the read lengths, and what the reads wrote behind the frames they returned. Every input
//! carries `expect`, what the portable `Reader` must do with it: `exact` (reproduce Apple's recording bit for bit), `refuse_codec`
//! (`CodecError::UnsupportedCodec`, for what Apple decodes and the port does not), `refuse_src` (`UnsupportedSampleRateConversion`),
//! `apple_error` (Apple's own status).
//! `readsizes.json` adds the recordings at 100-, 513- and 4096-frame reads. Everything is compared through the public `Reader`;
//! no Apple component is needed at run time, so the same test runs unchanged on macOS (`--features portable`), Linux arm64 and
//! Linux x86_64.

#[path = "support/aac_decode.rs"]
mod aac_decode;

use aac_decode::{BLOCK, CLIENT, Expect, Fixture, Fixtures, OPEN, ReadSizes, Scratch, allocation_counters, decoded, decode_interleaved, decode_planar, decode_planar_block, failure, hex,
    info_diffs, open, open_fixture, reset_allocation_counters};
use platinum_engine::portable::audio::{CodecError, Reader};
use platinum_engine::portable::digest::{Sha256, sha256};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const DTA: i32 = 0x6474_613f; // 'dta?'
const WHT: i32 = 0x7768_743f; // 'wht?'
const TYP: i32 = 0x7479_703f; // 'typ?'
const FMT: i32 = 0x666d_743f; // 'fmt?'
const BADA: i32 = 0x6261_6461; // 'bada'
const PARAM: i32 = -50;
/// SHA-256 of `manifest.json` and `readsizes.json` as committed (see the README).
const MANIFEST_SHA256: &str = "76edb70c1597fac78d5f10fb1fe42342f5b7e2e8da915d51b8e5487613ca64c9";
const READSIZES_SHA256: &str = "fd14eb79de6afa61330ca9fbdfcb09d01ec97dabf93d4a890677e4c7f16c7531";

/// `UnsupportedCodec { codec: "AAC", operation: "decoding", .. }` (the MP4 path may name the codec `AAC/MP4A`).
fn is_codec_refusal(error: &CodecError) -> bool {
    matches!(error, CodecError::UnsupportedCodec { codec, operation: "decoding", .. } if *codec == "AAC" || codec.starts_with("AAC/"))
}

fn is_src_refusal(error: &CodecError) -> bool { matches!(error, CodecError::UnsupportedSampleRateConversion { .. }) }

fn opens_ok(fixture: &Fixture) -> bool { fixture.opens() && matches!(fixture.expect, Expect::Exact) }

/// Files whose recordings the portable reader must reproduce bit for bit.
fn must_match(fixture: &Fixture) -> bool { opens_ok(fixture) }

#[test]
fn the_fixture_pack_is_intact_and_covers_what_it_claims() {
    let fixtures = Fixtures::load();
    let manifest = &fixtures.manifest;
    let parts = manifest["pack"]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), fixtures.parts.len());
    let mut whole = Vec::new();
    for (meta, data) in parts.iter().zip(&fixtures.parts) {
        assert_eq!(hex(&sha256(data)), meta["sha256"].as_str().unwrap(), "{} does not match the manifest", meta["file"]);
        assert_eq!(data.len() as u64, meta["bytes"].as_u64().unwrap());
        assert!(data.len() <= 1_500_000, "{}: pack parts stay below 1.5 MB", meta["file"]);
        whole.extend_from_slice(data);
    }
    assert_eq!(hex(&sha256(&whole)), manifest["pack"]["sha256"].as_str().unwrap(), "the pack does not match the manifest");
    assert_eq!(whole.len() as u64, manifest["pack"]["bytes"].as_u64().unwrap());
    assert!(whole.len() <= 5_500_000, "the fixture pack stays small");
    assert_eq!(parts.len(), 11);
    // the two documents the replay reads are pinned as well (the manifest pins every input, `readsizes.json` the pack it was recorded against)
    for (file, pinned) in [("manifest.json", MANIFEST_SHA256), ("readsizes.json", READSIZES_SHA256)] {
        let bytes = std::fs::read(aac_decode::fixture_dir().join(file)).unwrap();
        assert_eq!(hex(&sha256(&bytes)), pinned, "{file} changed: regenerate the pins (and the README) with the pack");
    }

    // every input once, contiguous inside its part; inputs with equal bytes (the same stream under another name) share their place
    let mut spans = BTreeSet::new();
    for fixture in &fixtures.files {
        assert!(fixture.length <= 60_000, "{}: fixtures are small", fixture.id);
        assert_eq!(hex(&sha256(fixtures.bytes(fixture))), fixture.sha256, "{}: input bytes", fixture.id);
        spans.insert((fixture.part, fixture.offset, fixture.length));
    }
    let mut end = vec![0usize; fixtures.parts.len()];
    for (part, offset, length) in spans {
        assert_eq!(offset, end[part], "part {part} is not contiguous");
        end[part] = offset + length;
    }
    assert!(end.iter().zip(&fixtures.parts).all(|(e, p)| *e == p.len()), "every pack byte belongs to an input");
    assert!(fixtures.files.windows(2).all(|pair| pair[0].id < pair[1].id), "manifest is sorted by unique id");
    assert!(fixtures.files.iter().all(|f| !f.name.contains('/') && !f.name.is_empty()), "replay names are plain file names");

    assert_eq!(fixtures.files.len(), 838, "fixtures");
    assert_eq!(fixtures.files.iter().map(|f| &f.sha256).collect::<BTreeSet<_>>().len(), 790, "distinct inputs");
    let seconds: f64 = fixtures.files.iter().filter(|f| f.expect == Expect::Exact).map(|f| f.seconds()).sum();
    assert!(seconds <= 130.0, "{seconds:.1} s of audio to decode: the test target must stay fast in debug builds");
    let count = |expect: Expect| fixtures.files.iter().filter(|f| f.expect == expect).count();
    assert_eq!([Expect::Exact, Expect::RefuseCodec, Expect::RefuseSrc, Expect::AppleError].map(count), [510, 74, 0, 254], "inputs per expectation class: exact, refuse_codec, refuse_src, apple_error");

    // what Apple answered: refusals of several kinds, read failures of several kinds
    let refused: BTreeSet<i32> = fixtures.files.iter().filter_map(|f| f.open_error.as_ref().map(|e| e.status)).collect();
    for status in [DTA, WHT, TYP, FMT] { assert!(refused.contains(&status), "no fixture refused with {status:#x}"); }
    let failed: BTreeSet<i32> = fixtures.files.iter().filter_map(|f| f.planar512.as_ref()?.error.as_ref().map(|e| e.status)).collect();
    for status in [BADA, PARAM] { assert!(failed.contains(&status), "no fixture whose read fails with {status:#x}"); }
    assert!(fixtures.files.iter().any(|f| f.planar512.as_ref().is_some_and(|s| s.error.as_ref().is_some_and(|e| e.operation == CLIENT))),
        "no fixture whose planar client format Apple refuses");

    // rates (every AAC sampling-frequency index 0..=12 occurs in the inputs), channel counts, formats
    let mut indices = BTreeSet::new();
    for fixture in &fixtures.files {
        for feature in &fixture.features { if let Some(n) = feature.strip_prefix("asc:sfi:") { indices.insert(n.parse::<u32>().unwrap()); } }
    }
    for index in 0..=12 { assert!(indices.contains(&index), "no input whose container header names sampling-frequency index {index}"); }
    let rates: BTreeSet<u32> = fixtures.files.iter().filter_map(|f| f.info.as_ref().map(|i| i.source.sample_rate as u32)).collect();
    for rate in [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000] { assert!(rates.contains(&rate), "no opened fixture at {rate} Hz"); }
    let channels: BTreeSet<u32> = fixtures.files.iter().filter_map(|f| f.info.as_ref().map(|i| i.source.channels)).collect();
    assert_eq!(channels.into_iter().collect::<Vec<_>>(), (1..=8).collect::<Vec<_>>(), "channel counts of the opened fixtures");
    let formats: BTreeSet<u32> = fixtures.files.iter().filter_map(|f| f.info.as_ref().map(|i| i.source.format_id)).collect();
    for tag in [b"aac ", b"aach", b"aacp", b"aacl", b"aace"] { assert!(formats.contains(&u32::from_be_bytes(*tag)), "no fixture Apple opens as {}", String::from_utf8_lossy(tag)); }

    // the features the task lists, each with several inputs
    let with = |feature: &str| fixtures.files.iter().filter(|f| f.has(feature)).count();
    for (feature, minimum) in [
        ("syntax:pns", 6), ("syntax:intensity", 6), ("syntax:ms-mask1", 4), ("syntax:ms-mask2", 4), ("syntax:tns", 20), ("syntax:short", 20), ("syntax:start-stop", 20),
        ("syntax:pulse", 3), ("syntax:kbd", 8), ("syntax:fil", 8), ("syntax:dse", 3), ("syntax:lfe", 3), ("syntax:multi-element", 8), ("syntax:asc-pce", 4), ("syntax:pce", 4),
        ("container:mp4", 150), ("container:adts", 20), ("container:loas", 6), ("container:caf", 15),
        ("mp4:moov-first", 20), ("mp4:moov-last", 8), ("mp4:free", 4), ("mp4:wide", 1), ("mp4:tracks=2", 2), ("mp4:no-esds", 3), ("truncated", 10),
        ("trim:itunsmpb", 8), ("trim:elst", 4), ("trim:roll-group", 2),
        ("adts:crc", 1), ("adts:multi-block", 1), ("adts:mpeg2-id", 1), ("adts:profile:0", 1), ("adts:profile:3", 1),
        ("bad-au:consecutive:1", 2), ("bad-au:consecutive:2", 2), ("bad-au:consecutive:3", 2), ("bad-au:consecutive:4", 2), ("au:oversize", 2), ("zero-packets", 1),
        ("misnamed", 20), ("id3:leading", 6), ("rate:le22050", 12), ("read:err:bada", 8), ("he:implicit-signalling", 2),
        ("rule:brand", 24), ("rule:ftyp", 23), ("rule:atoms", 10), ("rule:trun", 19), ("rule:cafhe", 10), ("rule:asc", 41), ("rule:damage", 71), ("rule:loas", 22), ("rule:pcemix", 18), ("rule:sgpd", 30), ("rule:smpb", 37), ("rule:cafpakt", 9), ("rule:short", 21), ("rule:udta", 13), ("rule:ldsc", 12), ("rule:sizes", 44),
    ] {
        assert!(with(feature) >= minimum, "{feature}: {} fixtures, expected at least {minimum}", with(feature));
    }
    // names the replay uses: AudioFile's type selection looks at the extension
    for extension in ["m4a", "aac", "mp4", "caf", "loas", "mp3", "wav", "bin", "none"] {
        assert!(fixtures.files.iter().any(|f| f.has(&format!("name:{extension}"))), "no fixture named *.{extension}");
    }
    // Apple's raw handle (not the engine's poisoning wrapper) both continues and keeps failing after a failed read
    let continues = fixtures.files.iter().filter(|f| f.after_failure.as_ref().is_some_and(|a| a.later.iter().any(|(s, n)| *s == 0 && *n > 0))).count();
    let stays = fixtures.files.iter().filter(|f| f.after_failure.as_ref().is_some_and(|a| a.later.iter().all(|(s, _)| *s != 0))).count();
    assert!(continues >= 5 && stays >= 5, "raw reads after a failure: {continues} continue, {stays} keep failing");

    // the iTunSMPB gate: the same bytes under 24 `ftyp` brand sets, the tag counting for some and not for others
    let lengths: BTreeSet<i64> = fixtures.files.iter().filter(|f| f.has("rule:brand")).filter_map(|f| f.info.as_ref().map(|i| match i.length { aac_decode::Prop::Value(v) => v, aac_decode::Prop::Error(_) => -1 })).collect();
    assert_eq!(lengths.into_iter().collect::<Vec<_>>(), [8820, 9152], "file lengths of the brand fixtures: the tag's 8820 frames and the default priming's 9152");
    let honoured = fixtures.files.iter().filter(|f| f.has("rule:brand") && f.info.as_ref().is_some_and(|i| i.length == aac_decode::Prop::Value(8820))).count();
    assert_eq!((honoured, fixtures.files.iter().filter(|f| f.has("rule:brand")).count() - honoured), (10, 14), "brand sets with and without the tag");
}

#[test]
fn every_fixture_is_bit_exact_with_the_apple_reference() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("golden");
    let mut mismatches = Vec::new();
    let (mut exact, mut refused_as_expected, mut failed_reads, mut frames) = (0usize, 0usize, 0usize, 0u64);
    for (index, fixture) in fixtures.files.iter().enumerate() {
        let path = scratch.write(index, &fixture.name, fixtures.bytes(fixture));
        let mut bad = |what: &str, expected: String, actual: String| mismatches.push(format!("{} ({}): {what}: expected {expected}, got {actual}", fixture.id, fixture.name));
        let result = open(&path);
        match (fixture.expect, &result) {
            (Expect::Exact, Ok(reader)) => {
                let info = fixture.info.as_ref().unwrap();
                for diff in info_diffs(reader, info) { bad("properties", String::new(), diff); }
            }
            (Expect::Exact, Err(error)) => {
                bad("open", "an open reader".into(), format!("{error:?}"));
                continue;
            }
            (Expect::AppleError, Err(error)) => {
                let actual = failure(error);
                if fixture.open_error.as_ref() != Some(&actual) { bad("open", format!("{:?}", fixture.open_error), format!("{error:?}")); }
                refused_as_expected += 1;
                continue;
            }
            (Expect::AppleError, Ok(_)) => { bad("open", format!("{:?}", fixture.open_error), "opened".into()); continue; }
            (Expect::RefuseCodec, Err(error)) if is_codec_refusal(error) => { refused_as_expected += 1; continue; }
            (Expect::RefuseSrc, Err(error)) if is_src_refusal(error) => { refused_as_expected += 1; continue; }
            (Expect::RefuseCodec | Expect::RefuseSrc, other) => {
                bad("open", format!("a typed refusal ({:?}: {})", fixture.expect, fixture.reason), format!("{:?}", other.as_ref().err()));
                continue;
            }
        }
        exact += 1;
        let both = decoded(fixtures, index, BLOCK, None);
        let (planar, inter) = match &*both {
            Ok(pair) => pair,
            Err(message) => { bad("decode", "a decodable file".into(), message.clone()); continue; }
        };
        for (label, expected, actual) in [("planar 512", fixture.planar512.as_ref().unwrap(), planar), ("interleaved 512", fixture.inter512.as_ref().unwrap(), inter)] {
            if let Some(diff) = expected.core_diff(actual) { bad(label, String::new(), diff); }
        }
        failed_reads += usize::from(planar.error.is_some());
        frames += planar.frames;
    }
    println!("{} fixtures: {exact} decoded, {refused_as_expected} refused as recorded, {failed_reads} with a read failure, {frames} frames compared", fixtures.files.len());
    assert!(mismatches.is_empty(), "{} mismatches against the Apple reference:\n{}", mismatches.len(), mismatches.iter().take(25).cloned().collect::<Vec<_>>().join("\n"));
    assert_eq!((exact, refused_as_expected, failed_reads, frames), (510, 328, 105, 4_176_985), "inputs decoded, refused as recorded, decoded to a read failure, frames compared");
}

/// Apple's converter hands the decoder's padding to the caller behind the frames a short read returns, and a read that returns 0
/// writes it too: the engine reads whole blocks. The recordings pre-fill the buffers with a sentinel and hash what was written.
#[test]
fn the_decoders_padding_behind_the_valid_frames_lands_in_the_callers_buffer_like_on_macos() {
    let fixtures = Fixtures::load();
    let (mut mismatches, mut with_tail, mut zero_reads_with_tail) = (Vec::new(), 0usize, 0usize);
    for (index, fixture) in fixtures.files.iter().enumerate().filter(|(_, f)| must_match(f)) {
        let both = decoded(fixtures, index, BLOCK, None);
        let Ok((planar, inter)) = &*both else { continue };
        for (label, expected, actual) in [("planar", fixture.planar512.as_ref().unwrap(), planar), ("interleaved", fixture.inter512.as_ref().unwrap(), inter)] {
            if let Some(diff) = expected.tails_diff(actual) { mismatches.push(format!("{} ({label}): {diff}", fixture.id)); }
            with_tail += usize::from(expected.tails.iter().any(|t| t.1 > 0));
            zero_reads_with_tail += usize::from(expected.tails.iter().any(|t| t.0 == 0 && t.1 > 0));
        }
    }
    assert!(mismatches.is_empty(), "{} recordings differ:\n{}", mismatches.len(), mismatches.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
    assert!(with_tail >= 200 && zero_reads_with_tail >= 100, "{with_tail} recordings with a written tail, {zero_reads_with_tail} behind a read that returned 0");
}

/// Apple's result depends on the read size when a read fails (the frames the failed read had gathered are lost, the packet
/// groups the converter asks for shift) and in how much padding lands behind a short read. The recordings are
/// `decode_probe record --block N` of the engine's own macOS reader.
#[test]
fn read_sizes_other_than_512_match_the_apple_recordings() {
    let fixtures = Fixtures::load();
    let sizes = ReadSizes::load();
    assert_eq!(sizes.document["pack_sha256"], fixtures.manifest["pack"]["sha256"], "recorded against another pack");
    assert_eq!(sizes.document["block_sizes"], json!([100, 513, 4096]));
    assert_eq!(sizes.document["apple"]["probe_sha256"], fixtures.manifest["apple"]["probe_sha256"]);
    let (mut records, mut differing, mut with_error, mut mismatches, mut tail_mismatches) = (0usize, 0usize, 0usize, Vec::new(), Vec::new());
    for file in &sizes.files {
        let index = fixtures.index_of(&file.id);
        let fixture = &fixtures.files[index];
        assert_eq!(file.sha256, fixture.sha256, "{}", file.id);
        let at_512 = (fixture.planar512.as_ref().unwrap(), fixture.inter512.as_ref().unwrap());
        for (shape, rows, reference) in [("planar", &file.planar, at_512.0), ("interleaved", &file.inter, at_512.1)] {
            assert_eq!(rows.len(), 3, "{}: {shape} sizes", file.id);
            for (block, expected) in rows {
                let both = decoded(fixtures, index, *block, None);
                let Ok(pair) = &*both else {
                    mismatches.push(format!("{} {shape} {block}: {:?}", file.id, both));
                    continue
                };
                let actual = if shape == "planar" { &pair.0 } else { &pair.1 };
                records += 1;
                differing += usize::from(expected != reference);
                with_error += usize::from(expected.error.is_some());
                if let Some(diff) = expected.core_diff(actual) { mismatches.push(format!("{} {shape} {block}: {diff}", file.id)); }
                if let Some(diff) = expected.tails_diff(actual) { tail_mismatches.push(format!("{} {shape} {block}: {diff}", file.id)); }
            }
        }
    }
    println!("{} files, {records} recordings ({differing} differ from the 512-frame result, {with_error} end in a read failure)", sizes.files.len());
    assert!(mismatches.is_empty(), "{} mismatches against the Apple recordings:\n{}", mismatches.len(), mismatches.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
    assert!(tail_mismatches.is_empty(), "{} recordings with another padding behind the valid frames:\n{}", tail_mismatches.len(), tail_mismatches.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
    assert_eq!((sizes.files.len(), records, differing, with_error), (182, 1092, 653, 582), "files, recordings, recordings that differ from the 512-frame one, recordings that end in a read failure");
}

#[test]
fn files_the_port_does_not_decode_are_refused_with_the_documented_error_class() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("refused");
    let (mut codec, mut native) = (0usize, 0usize);
    for (index, fixture) in fixtures.files.iter().enumerate() {
        match fixture.expect {
            Expect::RefuseCodec => {
                let path = scratch.write(index, &fixture.name, fixtures.bytes(fixture));
                // the client rate is irrelevant to the refusal: the codec (or, behind it, the rate conversion) is what is not reproduced
                for rate in [None, Some(44_100.0), Some(48_000.0), Some(8_000.0)] {
                    match Reader::open(&path, rate) {
                        Err(e) if is_codec_refusal(&e) || is_src_refusal(&e) => {}
                        other => panic!("{} at {rate:?}: expected a typed refusal, got {:?}", fixture.id, other.map(|_| ())),
                    }
                }
                match open(&path) {
                    Err(e) => assert!(is_codec_refusal(&e), "{}: {e:?}", fixture.id),
                    Ok(_) => panic!("{}: Apple decodes it as {:?}; the port must refuse it", fixture.id, fixture.info.as_ref().map(|i| i.source.format_id)),
                }
                codec += 1;
            }
            Expect::AppleError => {
                let expected = fixture.open_error.as_ref().unwrap();
                assert!(expected.operation == OPEN || expected.operation == CLIENT || expected.operation.starts_with("other:"), "{}: {expected:?}", fixture.id);
                let path = scratch.write(index, &fixture.name, fixtures.bytes(fixture));
                let actual = open(&path).map(|_| ()).map_err(|e| failure(&e));
                assert_eq!(actual, Err(expected.clone()), "{} ({})", fixture.id, fixture.name);
                native += 1;
            }
            _ => {}
        }
    }
    assert!(codec >= 8 && native >= 60, "{codec} codec refusals, {native} native refusals");
}

/// A client rate that differs from the file's is Apple's sample-rate conversion, which the port does not reproduce: the
/// recordings of Apple's answers at other client rates (`rate_variants`) say what each case must be.
#[test]
fn a_client_rate_that_differs_from_the_file_is_refused_never_resampled() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("rates");
    let (mut src, mut native, mut codec, mut variants) = (0usize, 0usize, 0usize, 0usize);
    for (index, fixture) in fixtures.files.iter().enumerate() {
        if fixture.variants.is_empty() { continue; }
        let path = scratch.write(index, &fixture.name, fixtures.bytes(fixture));
        for variant in &fixture.variants {
            variants += 1;
            let result = Reader::open(&path, Some(variant.client_rate));
            let what = format!("{} at {} Hz", fixture.id, variant.client_rate);
            match variant.expect {
                Expect::RefuseSrc => match result {
                    Err(CodecError::UnsupportedSampleRateConversion { source_rate, client_rate }) => {
                        assert_eq!((source_rate, client_rate), (variant.info.as_ref().unwrap().source.sample_rate, variant.client_rate), "{what}");
                        src += 1;
                    }
                    other => panic!("{what}: expected the refusal of the rate conversion, got {:?}", other.map(|_| ())),
                },
                Expect::AppleError => match result {
                    Err(e) => { assert_eq!(Some(failure(&e)), variant.open_error, "{what}: {e:?}"); native += 1; }
                    Ok(_) => panic!("{what}: Apple refuses ({:?}); the port opened it", variant.open_error),
                },
                Expect::RefuseCodec => match result {
                    Err(e) if is_codec_refusal(&e) || is_src_refusal(&e) => codec += 1,
                    other => panic!("{what}: expected a typed refusal, got {:?}", other.map(|_| ())),
                },
                Expect::Exact => {
                    let mut reader = result.unwrap_or_else(|e| panic!("{what}: {e:?}"));
                    let planar = decode_planar_block(&mut reader, variant.client_rate, BLOCK);
                    assert_eq!(planar.core_diff(variant.planar512.as_ref().unwrap()), None, "{what}");
                }
            }
        }
    }
    println!("{variants} client-rate variants: {src} sample-rate refusals, {native} native refusals, {codec} codec refusals");
    assert_eq!((variants, src, native, codec), (170, 95, 60, 15), "client-rate variants: sample-rate refusals, native refusals, codec refusals");

    // the same through an open reader: the planar client format at another rate is refused and leaves the client format alone
    let (index, fixture) = fixtures.files.iter().enumerate()
        .find(|(_, f)| must_match(f) && f.info.as_ref().is_some_and(|i| i.source.sample_rate == 44_100.0 && i.source.channels == 2) && f.planar512.as_ref().is_some_and(|s| s.error.is_none() && s.frames > 0))
        .expect("a 44.1 kHz stereo fixture");
    let path = scratch.write(index, &fixture.name, fixtures.bytes(fixture));
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

/// The engine's reader poisons itself at the first failed read (`failed`), unlike the AudioToolbox handle underneath it, which
/// carries on (`after_failure` in the manifest records what the raw handle does: it keeps failing for some files and delivers
/// the frames of the next good packets for others). The portable reader must follow the engine's wrapper: every later call
/// returns `CodecError::Poisoned` and writes nothing. A refused planar client format is not a read failure: the reader stays usable.
#[test]
fn a_failed_read_poisons_the_reader_and_loses_its_frames() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("poison");
    let (mut cases, mut refused_setters) = (0, 0);
    for (index, fixture) in fixtures.files.iter().enumerate().filter(|(_, f)| must_match(f)) {
        let (Some(planar), Some(inter)) = (&fixture.planar512, &fixture.inter512) else { continue };
        for (is_planar, expected) in [(true, planar), (false, inter)] {
            let Some(error) = &expected.error else { continue };
            let mut reader = open_fixture(&scratch, fixtures, index, fixture).unwrap();
            if error.operation == CLIENT {
                assert!(is_planar, "{}", fixture.id);
                // Apple refuses the planar client format; nothing was read, nothing is poisoned, and the interleaved format still works
                let rate = reader.source_format().sample_rate;
                assert_eq!(reader.set_stereo_planar(rate).map_err(|e| failure(&e)), Err(error.clone()), "{}", fixture.id);
                assert!(!matches!(reader.read_stereo_planar(&mut [0.0; 4], &mut [0.0; 4]), Err(CodecError::Poisoned)), "{}", fixture.id);
                if let Some(after) = &fixture.inter_after_planar_refusal {
                    let mut fresh = open_fixture(&scratch, fixtures, index, fixture).unwrap();
                    assert!(fresh.set_stereo_planar(rate).is_err());
                    assert_eq!(after.core_diff(&decode_interleaved(&mut fresh)), None, "{}: interleaved read after a refused planar format", fixture.id);
                }
                refused_setters += 1;
                continue;
            }
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
    assert!(cases >= 30 && refused_setters >= 1, "{cases} failing reads, {refused_setters} refused planar formats");
}

#[test]
fn reads_of_any_size_deliver_the_same_stream_when_nothing_fails() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("chunks");
    let mut checked = 0;
    for (index, fixture) in fixtures.files.iter().enumerate().filter(|(_, f)| must_match(f)) {
        let (Some(planar), Some(inter)) = (&fixture.planar512, &fixture.inter512) else { continue };
        if planar.error.is_some() || inter.error.is_some() || planar.frames == 0 || fixture.seconds() > 0.6 { continue; }
        if checked >= 16 { break; }
        let channels = fixture.channels();
        for chunk in [7usize, 576, 1000, 1024, 1025, 4096, 100_000] {
            let mut reader = open_fixture(&scratch, fixtures, index, fixture).unwrap();
            let rate = reader.source_format().sample_rate;
            reader.set_stereo_planar(rate).unwrap();
            let (mut left, mut right) = (vec![0f32; chunk], vec![0f32; chunk]);
            let mut hash = Sha256::new();
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
            let mut hash = Sha256::new();
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
    assert!(checked >= 12, "{checked} clean fixtures");
}

#[test]
fn misuse_is_reported_like_the_other_readers_do() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("misuse");
    let (index, fixture) = fixtures.files.iter().enumerate()
        .find(|(_, f)| must_match(f) && f.channels() == 2 && f.planar512.as_ref().is_some_and(|s| s.frames > 2048 && s.error.is_none()))
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
fn decode_ignores_the_ambient_flush_to_zero_mode() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("fz");
    let mut checked = 0;
    // TNS decay tails and loud content are where the decoder's float arithmetic produces subnormals and overflows
    for (index, fixture) in fixtures.files.iter().enumerate()
        .filter(|(_, f)| must_match(f) && f.seconds() < 1.5 && (f.has("syntax:tns") || f.has("syntax:pns") || f.name.contains("loud"))).take(10) {
        let expected = fixture.planar512.as_ref().unwrap();
        let path = scratch.write(index, &fixture.name, fixtures.bytes(fixture));
        let flushed = platinum_engine::fp_mode::with_flush_to_zero(|| decode_planar(&mut open(&path).unwrap()));
        assert_eq!(expected.core_diff(&flushed), None, "{}: flush-to-zero changes the decode", fixture.id);
        checked += 1;
    }
    assert!(checked >= 6, "{checked} fixtures with TNS, PNS or loud content");
}

/// The same input under every name AudioFile might read it by: the file name decides which reader looks at the bytes (an MPEG
/// audio extension commits to the MPEG reader, whatever the content), so the recorded result differs between the renames.
#[test]
fn the_file_name_selects_the_reader_for_the_same_bytes() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("names");
    let mut by_bytes: BTreeMap<&str, Vec<(usize, &Fixture)>> = BTreeMap::new();
    for (index, fixture) in fixtures.files.iter().enumerate() { by_bytes.entry(fixture.sha256.as_str()).or_default().push((index, fixture)); }
    let (mut groups, mut differing_groups) = (0usize, 0usize);
    for group in by_bytes.values().filter(|g| g.len() >= 4) {
        groups += 1;
        let mut outcomes = BTreeSet::new();
        for (index, fixture) in group {
            let result = open(&scratch.write(*index, &fixture.name, fixtures.bytes(fixture)));
            outcomes.insert(format!("{:?}", fixture.open_error.as_ref().map(|e| (e.operation.clone(), e.status))));
            match (&fixture.open_error, &result) {
                (Some(expected), Err(e)) if fixture.expect == Expect::AppleError => assert_eq!(&failure(e), expected, "{} ({})", fixture.id, fixture.name),
                (None, _) => {}
                (Some(_), other) => panic!("{} ({}): {:?}", fixture.id, fixture.name, other.as_ref().err()),
            }
        }
        differing_groups += usize::from(outcomes.len() > 1);
    }
    assert!(groups >= 4, "{groups} groups of renamed inputs");
    assert!(differing_groups >= 2, "{differing_groups} groups where the name changes Apple's answer");
}

/// Decode speed on the fixtures, for the integration notes: `cargo test --release --test portable_aac_decode -- --ignored --nocapture`.
#[test]
#[ignore = "timing report, run with --release"]
fn decode_speed_report() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("speed");
    let mut paths = Vec::new();
    let mut seconds_of_audio = 0.0f64;
    for (index, fixture) in fixtures.files.iter().enumerate().filter(|(_, f)| must_match(f)) {
        let Some(planar) = &fixture.planar512 else { continue };
        if planar.error.is_some() { continue; }
        paths.push(scratch.write(index, &fixture.name, fixtures.bytes(fixture)));
        seconds_of_audio += fixture.seconds();
    }
    // Open, decode every planar 512-frame block and fold the samples; no hashing.
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

// ---- long inputs: generated here, never stored ------------------------------------------------------------------------------

fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn full_box(kind: &[u8; 4], flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut body = flags.to_be_bytes().to_vec();
    body.extend_from_slice(payload);
    boxed(kind, &body)
}

fn be32s(values: &[u32]) -> Vec<u8> { values.iter().flat_map(|v| v.to_be_bytes()).collect() }

/// One AAC-LC access unit of silence: a channel-pair element with `max_sfb = 0` and two channel streams without spectral data,
/// ended by `ID_END` (6 bytes). Apple decodes it to exact zeros (of either sign).
fn silent_au() -> Vec<u8> {
    let mut bits: Vec<bool> = Vec::new();
    let mut put = |value: u32, count: u32| { for i in (0..count).rev() { bits.push(value >> i & 1 == 1); } };
    put(1, 3); // ID_CPE
    put(0, 4); // element_instance_tag
    put(1, 1); // common_window
    put(0, 1); put(0, 2); put(0, 1); put(0, 6); put(0, 1); // ics_info: reserved, ONLY_LONG, sine window, max_sfb 0, no predictor data
    put(0, 2); // ms_mask_present
    for _ in 0..2 { put(100, 8); put(0, 1); put(0, 1); put(0, 1); } // global_gain; no pulse, TNS or gain control
    put(7, 3); // ID_END
    while bits.len() % 8 != 0 { bits.push(false); }
    bits.chunks(8).map(|b| b.iter().fold(0u8, |acc, bit| acc << 1 | u8::from(*bit))).collect()
}

/// ADTS (MPEG-4, AAC-LC, 44.1 kHz, stereo, no CRC) of `frames` silent access units.
fn silent_adts(frames: usize) -> Vec<u8> {
    let au = silent_au();
    let length = au.len() + 7;
    let mut out = Vec::with_capacity(frames * length);
    for _ in 0..frames {
        out.extend_from_slice(&[0xff, 0xf1, 0x50, 0x80 | ((length >> 11) & 3) as u8, (length >> 3) as u8, (((length & 7) << 5) | 0x1f) as u8, 0xfc]);
        out.extend_from_slice(&au);
    }
    out
}

/// M4A of `n` silent access units in one chunk with a constant sample size, `moov` behind `mdat` or in front of it, and no
/// iTunSMPB or edit list.
fn silent_mp4(n: u32, moov_last: bool) -> Vec<u8> {
    const RATE: u32 = 44_100;
    let au = silent_au();
    let ftyp = boxed(b"ftyp", &[&b"M4A "[..], &[0; 4], b"M4A mp42isom"].concat());
    let mdat = boxed(b"mdat", &au.repeat(n as usize));
    let matrix = be32s(&[0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000]);
    let moov = |chunk_offset: u32| {
        let mvhd = full_box(b"mvhd", 0, &[be32s(&[0, 0, RATE, n * 1024, 0x10000]), vec![1, 0], vec![0; 10], matrix.clone(), vec![0; 24], be32s(&[2])].concat());
        let tkhd = full_box(b"tkhd", 3, &[be32s(&[0, 0, 1, 0, n * 1024]), vec![0; 8], vec![0, 0, 0, 0, 1, 0, 0, 0], matrix.clone(), vec![0; 8]].concat());
        let mdhd = full_box(b"mdhd", 0, &[be32s(&[0, 0, RATE, n * 1024]), vec![0x55, 0xc4, 0, 0]].concat());
        let hdlr = full_box(b"hdlr", 0, &[vec![0; 4], b"soun".to_vec(), vec![0; 12], b"SoundHandler\0".to_vec()].concat());
        let smhd = full_box(b"smhd", 0, &[0; 4]);
        let dinf = boxed(b"dinf", &full_box(b"dref", 0, &[be32s(&[1]), full_box(b"url ", 1, &[])].concat()));
        let asc = [0x12u8, 0x10];
        let dsi = [&[5u8, asc.len() as u8][..], &asc].concat();
        let dcd = [&[4u8, (13 + dsi.len()) as u8, 0x40, 0x15][..], &[0; 3], &be32s(&[0, 0]), &dsi].concat();
        let sl = [6u8, 1, 2];
        let esd = [&[3u8, (3 + dcd.len() + sl.len()) as u8, 0, 0, 0][..], &dcd, &sl].concat();
        let esds = full_box(b"esds", 0, &esd);
        let mp4a = boxed(b"mp4a", &[vec![0; 6], vec![0, 1], vec![0; 8], vec![0, 2, 0, 16, 0, 0, 0, 0], (RATE << 16).to_be_bytes().to_vec(), esds].concat());
        let stsd = full_box(b"stsd", 0, &[be32s(&[1]), mp4a].concat());
        let stts = full_box(b"stts", 0, &be32s(&[1, n, 1024]));
        let stsc = full_box(b"stsc", 0, &be32s(&[1, 1, n, 1]));
        let stsz = full_box(b"stsz", 0, &be32s(&[au.len() as u32, n]));
        let stco = full_box(b"stco", 0, &be32s(&[1, chunk_offset]));
        let stbl = boxed(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
        let minf = boxed(b"minf", &[smhd, dinf, stbl].concat());
        let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
        boxed(b"moov", &[mvhd, boxed(b"trak", &[tkhd, mdia].concat())].concat())
    };
    if moov_last {
        [ftyp.clone(), mdat, moov(ftyp.len() as u32 + 8)].concat()
    } else {
        let size = moov(0).len() as u32;
        [ftyp.clone(), moov(ftyp.len() as u32 + size + 8), mdat].concat()
    }
}

// What Apple's `ExtAudioFile` (the engine's reader on macOS) delivered for these inputs (`decode_probe record`, class G, 512-frame
// reads): the planar stream of 13,000 silent access units is 13,000 x 1,024 frames less the 2,112 frames of encoder delay that
// Apple removes from an MP4 without iTunSMPB or edit list; an ADTS stream gets no priming trim.
const ADTS_SHA256: &str = "9840ebb54a9cf881d161c1f0dc904f7fdc1c2b2a4a2a3f4499f0861328505507";
const MP4_LAST_SHA256: &str = "330fae753fe14f48ba5b8e46ce295382cc66386c5517f75ff42b276a4a51b446";
const MP4_FIRST_SHA256: &str = "089426d6cfbaa2115ecb7361365bb0a2f6790341ecbb77c2c54ca4570e8feb6d";
const ADTS_STREAM_SHA256: &str = "06e6726bde98fc0fa5d00bae55cf52b38d6d30105331d6f43a02794f3c8dc3cc";
const MP4_STREAM_SHA256: &str = "f18f6f5d1368c9b1a610e10466d5312ec4df7f7caaac13de998fece27cc01683";
const ADTS_FRAMES: u64 = 20_000 * 1024;
const MP4_FRAMES: u64 = 13_000 * 1024 - 2112;


#[test]
fn the_generated_long_inputs_are_the_ones_apple_decoded() {
    assert_eq!(hex(&sha256(&silent_adts(20_000))), ADTS_SHA256);
    assert_eq!(hex(&sha256(&silent_mp4(13_000, true))), MP4_LAST_SHA256);
    assert_eq!(hex(&sha256(&silent_mp4(13_000, false))), MP4_FIRST_SHA256);
}

fn decode_long(name: &str, bytes: &[u8], frames: u64, stream_sha256: &str, bitrate: u32) {
    let scratch = Scratch::new("long");
    let path = scratch.write(0, name, bytes);
    reset_allocation_counters();
    let reader = open(&path).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    assert_eq!(reader.file_length_frames().unwrap(), frames as i64, "{name}: file length");
    assert_eq!(reader.bitrate().unwrap(), bitrate, "{name}: bitrate");
    let source = reader.source_format();
    assert_eq!((source.sample_rate, source.channels_per_frame, source.frames_per_packet), (44_100.0, 2, 1024));
    let mut reader = reader;
    let rate = source.sample_rate;
    let stream = decode_planar_block(&mut reader, rate, BLOCK);
    assert_eq!(stream.error, None, "{name}");
    assert_eq!((stream.frames, stream.reads, stream.sha256.as_str()), (frames, frames.div_ceil(BLOCK as u64), stream_sha256), "{name}");
    assert_eq!(stream.short, u64::from(frames % BLOCK as u64 != 0), "{name}: only the last read is short");
    // the decoded stream (164 MB) is never held: the file and its tables are all the reader keeps
    let (largest, live) = allocation_counters();
    assert!(live <= 4 * bytes.len() + (16 << 20), "{name}: {live} bytes live at once for a file of {} bytes: the decoded stream must not be held", bytes.len());
    assert!(largest <= 2 * bytes.len() + (8 << 20), "{name}: one allocation of {largest} bytes for a file of {} bytes", bytes.len());
}

/// Long streams decode in bounded memory (the decoded stream here is 164 MB; nothing may keep it): ADTS, 464 s of silent frames.
#[test]
#[ignore = "long: run by the release battery"]
fn a_464_second_adts_stream_decodes_to_its_end_like_apple() {
    decode_long("long.aac", &silent_adts(20_000), ADTS_FRAMES, ADTS_STREAM_SHA256, 4478);
}

/// An MP4 of more than 301 s: the layout the Apple encoder switches to for long files (`moov` behind `mdat`), and the plain one.
#[test]
#[ignore = "long: run by the release battery"]
fn a_302_second_mp4_decodes_to_its_end_like_apple_with_moov_at_the_end_or_in_front() {
    for (name, moov_last) in [("long-moov-last.m4a", true), ("long-moov-first.m4a", false)] {
        decode_long(name, &silent_mp4(13_000, moov_last), MP4_FRAMES, MP4_STREAM_SHA256, 2067);
    }
}
