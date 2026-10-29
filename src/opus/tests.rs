//! Tests of the Ogg Opus layer. Scan/validation tests build synthetic streams
//! with fake audio packets (valid table of contents, arbitrary payload) and need
//! no libopus. Decode/encode tests need a trusted fixed-point libopus and are
//! ignored unless `PN_OPUS_LIB` and `PN_OPUS_SHA256` are set (`cargo test --
//! --ignored`).

use super::head::{CommentHeader, OpusHead};
use super::ogg::{FLAG_CONTINUED, FLAG_EOS, NO_GRANULE, Page, PageWriter, lacing_for};
use super::reader::{StreamInfo, scan};
use super::*;

fn fake_packet(len: usize) -> Vec<u8> {
    let mut p = vec![0xfc]; // CELT fullband 20 ms, code 0
    p.extend((1..len).map(|i| (i * 31 + 7) as u8));
    p
}

fn head(channels: u8, pre_skip: u16) -> Vec<u8> {
    OpusHead { version: 1, channels, pre_skip, input_sample_rate: 48_000, output_gain: 0 }.encode()
}
fn tags() -> Vec<u8> {
    CommentHeader { vendor: b"test".to_vec(), comments: vec![b"TITLE=t".to_vec()], trailing: Vec::new() }.encode().unwrap()
}

struct Built { bytes: Vec<u8> }
/// `packets` are written one per page unless `per_page` says otherwise.
fn build(head_packet: &[u8], tags_packet: &[u8], packets: &[Vec<u8>], per_page: usize, initial: u64, end_trim: u64, _pre_skip: u64) -> Built {
    let mut out = Vec::new();
    let mut writer = PageWriter::new(&mut out, 0x1357_9bdf);
    writer.write_header_packet(head_packet).unwrap();
    writer.write_header_packet(tags_packet).unwrap();
    for p in packets {
        writer.push_audio(p, 960, if per_page == 1 { 1 } else { 960 * per_page as u64 }).unwrap();
    }
    // The writer counts granules from zero; `initial` shifts them afterwards.
    writer.finish(packets.len() as u64 * 960 - end_trim).unwrap();
    let mut bytes = out;
    if initial != 0 { bytes = shift_granules(&bytes, initial); }
    Built { bytes }
}

/// Add `delta` to every audio page's granule (pages after the comment header) and refresh CRCs.
fn shift_granules(bytes: &[u8], delta: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut reader = super::ogg::PageReader::new(bytes);
    let mut page = Page::default();
    let mut index = 0;
    while reader.read_page(&mut page).unwrap() {
        if index >= 2 && page.granule != NO_GRANULE { page.granule += delta; }
        out.extend_from_slice(&page.encode());
        index += 1;
    }
    out
}

fn scan_bytes(bytes: &[u8]) -> Result<StreamInfo, OpusError> { scan(bytes, false).map(|(info, _)| info) }

fn stream(n: usize) -> Vec<u8> { build(&head(2, 312), &tags(), &vec![fake_packet(200); n], 1, 0, 0, 312).bytes }

#[test]
fn scan_reports_exact_length_pre_skip_and_trim() {
    let packets: Vec<Vec<u8>> = (0..10).map(|i| fake_packet(100 + i)).collect();
    // 10 packets = 9600 samples; keep all but 200 at the end; pre-skip 312.
    let b = build(&head(2, 312), &tags(), &packets, 3, 0, 200, 312);
    let info = scan_bytes(&b.bytes).unwrap();
    assert_eq!(info.decoded_samples, 9600);
    assert_eq!(info.end_trim, 200);
    assert_eq!(info.frames, 9600 - 200 - 312);
    assert_eq!(info.audio_packets, 10);
    assert_eq!(info.head.channels, 2);
    assert_eq!(info.comment_count, 1);
    assert_eq!(info.audio_bytes, (0..10).map(|i| 100 + i as u64).sum::<u64>());
}

#[test]
fn scan_accepts_a_cropped_start_with_an_initial_granule() {
    let packets = vec![fake_packet(50); 6];
    let b = build(&head(1, 312), &tags(), &packets, 2, 100_000, 0, 312);
    let info = scan_bytes(&b.bytes).unwrap();
    assert_eq!(info.initial_granule, 100_000);
    assert_eq!(info.frames, 6 * 960 - 312);
}

#[test]
fn scan_handles_a_final_page_that_is_also_the_first_audio_page() {
    let b = build(&head(2, 100), &tags(), &[fake_packet(40), fake_packet(40)], 100, 0, 700, 100);
    let info = scan_bytes(&b.bytes).unwrap();
    assert_eq!(info.frames, 1920 - 700 - 100);
    // Granule smaller than pre-skip is invalid.
    let bad = build(&head(2, 1500), &tags(), &[fake_packet(40), fake_packet(40)], 100, 0, 700, 1500);
    assert!(matches!(scan_bytes(&bad.bytes), Err(OpusError::Invalid(_))));
}

#[test]
fn scan_rejects_structural_problems_with_typed_errors() {
    let good = stream(8);
    assert!(scan_bytes(&good).is_ok());
    // Truncation anywhere is an error, never a short success.
    for cut in (0..good.len()).step_by(37) {
        assert!(scan_bytes(&good[..cut]).is_err(), "cut at {cut}");
    }
    // Any single flipped bit is caught by a page checksum or structural check.
    for position in (0..good.len()).step_by(53) {
        let mut damaged = good.clone();
        damaged[position] ^= 0x10;
        assert!(scan_bytes(&damaged).is_err(), "bit flip at {position}");
    }
    // Chained: a second logical stream appended.
    let mut chained = good.clone();
    chained.extend_from_slice(&good);
    assert!(matches!(scan_bytes(&chained), Err(OpusError::Unsupported(_))));
    // Trailing garbage.
    let mut garbage = good.clone();
    garbage.extend_from_slice(b"junk");
    assert!(matches!(scan_bytes(&garbage), Err(OpusError::Invalid(_))));
    // Empty and non-Ogg input.
    assert!(matches!(scan_bytes(&[]), Err(OpusError::Invalid(_))));
    assert!(matches!(scan_bytes(b"RIFF....WAVEfmt "), Err(OpusError::Invalid(_))));
}

#[test]
fn scan_rejects_unsupported_stream_variants() {
    let packets = vec![fake_packet(60); 4];
    let mut h = head(2, 312);
    h[18] = 1; // mapping family 1
    assert!(matches!(scan_bytes(&build(&h, &tags(), &packets, 1, 0, 0, 312).bytes), Err(OpusError::Unsupported(_))));
    assert!(matches!(scan_bytes(&build(&head(3, 312), &tags(), &packets, 1, 0, 0, 312).bytes), Err(OpusError::Unsupported(_))));
    assert!(matches!(scan_bytes(&build(&head(0, 312), &tags(), &packets, 1, 0, 0, 312).bytes), Err(OpusError::Invalid(_))));
    let mut future = head(2, 312);
    future[8] = 16;
    assert!(matches!(scan_bytes(&build(&future, &tags(), &packets, 1, 0, 0, 312).bytes), Err(OpusError::Unsupported(_))));
    // Vorbis or any non-Opus first packet.
    assert!(scan_bytes(&build(b"\x01vorbis....", &tags(), &packets, 1, 0, 0, 0).bytes).is_err());
}

#[test]
fn scan_rejects_inconsistent_granules_sequences_and_packets() {
    // Missing end-of-stream page.
    let good = stream(6);
    let mut reader = super::ogg::PageReader::new(&good[..]);
    let mut pages = Vec::new();
    let mut page = Page::default();
    while reader.read_page(&mut page).unwrap() { pages.push(page.clone()); }
    let join = |ps: &[Page]| ps.iter().flat_map(|p| p.encode()).collect::<Vec<u8>>();
    assert!(matches!(scan_bytes(&join(&pages[..pages.len() - 1])), Err(OpusError::Invalid(_))));
    // A page lost from the middle (sequence gap).
    let mut gap = pages.clone();
    gap.remove(4);
    assert!(matches!(scan_bytes(&join(&gap)), Err(OpusError::Invalid(_))));
    // Granule that disagrees with the packet durations.
    let mut skewed = pages.clone();
    skewed[4].granule += 1;
    assert!(matches!(scan_bytes(&join(&skewed)), Err(OpusError::Invalid(_))));
    // A second serial number.
    let mut serial = pages.clone();
    serial[4].serial ^= 1;
    assert!(matches!(scan_bytes(&join(&serial)), Err(OpusError::Unsupported(_))));
    // Beginning-of-stream flag mid-stream.
    let mut bos = pages.clone();
    bos[4].flags |= super::ogg::FLAG_BOS;
    assert!(matches!(scan_bytes(&join(&bos)), Err(OpusError::Unsupported(_))));
    // A page with completed packets but granule -1.
    let mut unset = pages.clone();
    unset[3].granule = NO_GRANULE;
    assert!(matches!(scan_bytes(&join(&unset)), Err(OpusError::Invalid(_))));
    // First audio page continued.
    let mut continued = pages.clone();
    continued[2].flags |= FLAG_CONTINUED;
    assert!(matches!(scan_bytes(&join(&continued)), Err(OpusError::Invalid(_))));
    // Data after EOS flag set on an earlier page.
    let mut early = pages.clone();
    early[3].flags |= FLAG_EOS;
    assert!(scan_bytes(&join(&early)).is_err());
    // An end-of-stream page that completes its packets and then starts one that never ends: damaged, not shorter.
    let mut unfinished = pages.clone();
    let last = unfinished.last_mut().unwrap();
    last.lacing.push(255);
    last.body.extend(std::iter::repeat_n(0xfc, 255));
    assert!(matches!(scan_bytes(&join(&unfinished)), Err(OpusError::Invalid(m)) if m.contains("unfinished packet")), "{:?}", scan_bytes(&join(&unfinished)));
    // Zero-length packet, malformed TOC, oversized packet.
    for packet in [Vec::new(), vec![0xff], vec![0xff, 0], vec![0xfc; 61_441]] {
        let b = build(&head(2, 312), &tags(), &[fake_packet(10), packet, fake_packet(10)], 1, 0, 0, 312);
        assert!(scan_bytes(&b.bytes).is_err());
    }
    // RFC 7845 s4: the OpusHead page has granule position 0 ...
    let mut head_granule = pages.clone();
    head_granule[0].granule = 960;
    assert!(matches!(scan_bytes(&join(&head_granule)), Err(OpusError::Invalid(_))));
    // ... and a header page entirely spanned by the comment packet has granule position -1.
    let big = CommentHeader { vendor: b"v".to_vec(), comments: vec![vec![b'x'; 300_000]], trailing: Vec::new() }.encode().unwrap();
    let spanning = build(&head(2, 312), &big, &vec![fake_packet(30); 3], 1, 0, 0, 312).bytes;
    assert!(scan_bytes(&spanning).is_ok());
    let mut reader = super::ogg::PageReader::new(&spanning[..]);
    let mut spanned = Vec::new();
    while reader.read_page(&mut page).unwrap() { spanned.push(page.clone()); }
    assert_eq!(spanned[1].granule, NO_GRANULE, "the writer marks a spanned comment page with -1");
    spanned[1].granule = 0;
    assert!(matches!(scan_bytes(&join(&spanned)), Err(OpusError::Invalid(_))));
    // Pre-skip larger than the stream.
    assert!(matches!(scan_bytes(&build(&head(2, 5000), &tags(), &vec![fake_packet(30); 3], 1, 0, 0, 5000).bytes), Err(OpusError::Invalid(_))));
}

#[test]
fn comment_header_may_span_many_pages_and_is_bounded() {
    let big = CommentHeader { vendor: b"v".to_vec(), comments: vec![vec![b'x'; 300_000]], trailing: Vec::new() }.encode().unwrap();
    let b = build(&head(2, 312), &big, &vec![fake_packet(30); 3], 1, 0, 0, 312);
    let (info, kept) = scan(&b.bytes[..], true).unwrap();
    assert_eq!(kept.unwrap(), big);
    assert_eq!(info.comment_header_bytes as usize, big.len());
    // A comment header that claims huge fields is refused without allocating them.
    let mut lie = b"OpusTags".to_vec();
    lie.extend_from_slice(&u32::MAX.to_le_bytes());
    lie.extend_from_slice(&[0u8; 12]);
    assert!(scan_bytes(&build(&head(2, 312), &lie, &vec![fake_packet(30); 3], 1, 0, 0, 312).bytes).is_err());
    // Padding that is a continuation with the wrong flag is refused.
    let mut pages = Vec::new();
    let mut reader = super::ogg::PageReader::new(&b.bytes[..]);
    let mut page = Page::default();
    while reader.read_page(&mut page).unwrap() { pages.push(page.clone()); }
    pages[2].flags &= !FLAG_CONTINUED;
    assert!(scan_bytes(&pages.iter().flat_map(|p| p.encode()).collect::<Vec<u8>>()).is_err());
}

#[test]
fn writer_layout_is_a_pure_function_of_the_packets() {
    // Page layout is independent of how the packets were pushed.
    let packets: Vec<Vec<u8>> = (0..130).map(|i| fake_packet(300 + (i % 7) * 50)).collect();
    let make = || {
        let mut out = Vec::new();
        let mut w = PageWriter::new(&mut out, 7);
        w.write_header_packet(&head(2, 312)).unwrap();
        w.write_header_packet(&tags()).unwrap();
        for p in &packets { w.push_audio(p, 960, PAGE_SAMPLES).unwrap(); }
        w.finish(130 * 960 - 5).unwrap();
        out
    };
    let a = make();
    assert_eq!(a, make());
    let mut reader = super::ogg::PageReader::new(&a[..]);
    let mut page = Page::default();
    let mut count = 0;
    let mut last_flags = 0;
    while reader.read_page(&mut page).unwrap() {
        if count >= 2 && page.flags & FLAG_EOS == 0 {
            assert!(page.lacing.len() <= 255);
        }
        last_flags = page.flags;
        count += 1;
    }
    assert_ne!(last_flags & FLAG_EOS, 0);
    assert_eq!(scan_bytes(&a).unwrap().frames, 130 * 960 - 5 - 312);
    let _ = lacing_for;
}

#[test]
fn repack_replaces_tags_and_is_deterministic() {
    let dir = std::env::temp_dir().join(format!("pn-opus-repack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.opus");
    let packets: Vec<Vec<u8>> = (0..75).map(|i| fake_packet(120 + (i % 5))).collect();
    std::fs::write(&path, build(&head(2, 312), &tags(), &packets, 40, 0, 17, 312).bytes).unwrap();
    let new = CommentHeader { vendor: b"libopus 1.6.1-fixed".to_vec(),
        comments: vec![b"TITLE=\xc3\xa9".to_vec(), b"PLATINUMNOTES=abc".to_vec(), vec![b'x'; 200_000]], trailing: vec![1, 2, 3] };
    repack_with_comments(&path, &new).unwrap();
    let first = std::fs::read(&path).unwrap();
    let (info, kept) = scan(&first[..], true).unwrap();
    assert_eq!(info.frames, 75 * 960 - 17 - 312);
    assert_eq!(CommentHeader::parse(&kept.unwrap()).unwrap(), new);
    // The audio packets survive byte for byte.
    let mut reader = super::ogg::PageReader::new(&first[..]);
    let mut page = Page::default();
    let mut audio = 0usize;
    let mut index = 0;
    while reader.read_page(&mut page).unwrap() {
        if page.sequence as u64 >= info.audio_sequence as u64 { audio += page.body.len(); }
        index += 1;
    }
    assert!(index > 4);
    assert_eq!(audio as u64, info.audio_bytes);
    // Repacking an already repacked file with the same tags changes nothing.
    repack_with_comments(&path, &new).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), first);
    // The serial depends on the content.
    let path2 = dir.join("b.opus");
    std::fs::write(&path2, build(&head(2, 312), &tags(), &packets[..74], 40, 0, 17, 312).bytes).unwrap();
    repack_with_comments(&path2, &new).unwrap();
    assert_ne!(scan(&std::fs::read(&path2).unwrap()[..], false).unwrap().0.serial, info.serial);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- tests that need a real libopus ---------------------------------------

use library_tests::load_for_tests;

fn tone(frames: usize, channels: usize, amplitude: f32) -> Vec<f32> {
    // Deterministic, libm-free: a pair of triangle waves.
    let mut out = Vec::with_capacity(frames * channels);
    for n in 0..frames {
        let a = ((n * 37) % 480) as f32 / 240.0 - 1.0;
        let a = if a < 0.0 { -a } else { a } * 2.0 - 1.0;
        let b = ((n * 91) % 320) as f32 / 160.0 - 1.0;
        let b = if b < 0.0 { -b } else { b } * 2.0 - 1.0;
        for c in 0..channels { out.push(amplitude * if c == 0 { 0.6 * a + 0.4 * b } else { 0.4 * a - 0.6 * b }); }
    }
    out
}

fn encode_to(path: &std::path::Path, lib: &OpusLibrary, channels: usize, pcm: &[f32], chunk: usize) {
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap();
    let mut writer = OpusWriter::create(lib, file, channels).unwrap();
    for part in pcm.chunks(chunk * channels) { writer.write(part).unwrap(); }
    writer.finish().unwrap().sync_all().unwrap();
}

#[test]
#[ignore = "requires PN_OPUS_LIB/PN_OPUS_SHA256 (fixed-point libopus 1.6.1)"]
fn print_known_answer_digests() {
    let path = std::env::var_os("PN_OPUS_LIB").unwrap();
    let digest = std::env::var("PN_OPUS_SHA256").unwrap();
    let lib = OpusLibrary::load_unchecked(path, &digest).unwrap();
    let (encoded, decoded, lookahead) = known_answer_digests(&lib).unwrap();
    println!("KAT encoded {encoded}\nKAT decoded {decoded}\nKAT lookahead {lookahead}");
}

#[test]
#[ignore = "requires PN_OPUS_LIB/PN_OPUS_SHA256 (fixed-point libopus 1.6.1)"]
fn encode_decode_round_trip_is_exact_in_length_and_deterministic() {
    let lib = load_for_tests();
    let dir = std::env::temp_dir().join(format!("pn-opus-rt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for channels in [1usize, 2] {
        for frames in [0usize, 1, 311, 312, 959, 960, 961, 4800, 48_000 + 123, 150_000] {
            let pcm = tone(frames, channels, 0.5);
            let a = dir.join(format!("a-{channels}-{frames}.opus"));
            let b = dir.join(format!("b-{channels}-{frames}.opus"));
            encode_to(&a, &lib, channels, &pcm, 512);
            encode_to(&b, &lib, channels, &pcm, 777);
            assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap(), "chunking changed the bytes ({channels}ch {frames})");
            let tags = CommentHeader { vendor: lib.identity().version.clone().into_bytes(), comments: vec![b"TITLE=x".to_vec()], trailing: Vec::new() };
            repack_with_comments(&a, &tags).unwrap();
            let mut reader = OpusReader::open(&lib, &a, Some(48_000.0)).unwrap();
            assert_eq!(reader.info().frames, frames as u64, "{channels}ch {frames}");
            assert_eq!(reader.info().head.pre_skip, 312);
            let mut out = vec![0f32; (frames + 10) * channels];
            let mut got = 0;
            loop {
                let n = reader.read(&mut out[got * channels..]).unwrap();
                if n == 0 { break; }
                got += n;
            }
            assert_eq!(got, frames);
            if frames >= 4800 {
                // The codec is lossy but a smooth signal must come back close to the input.
                let (mut err, mut sig) = (0f64, 0f64);
                for i in 0..frames * channels {
                    let d = f64::from(out[i] - pcm[i]);
                    err += d * d;
                    sig += f64::from(pcm[i]) * f64::from(pcm[i]);
                }
                let snr = 10.0 * (sig / if err > 1e-30 { err } else { 1e-30 }).log10();
                assert!(snr > 20.0, "SNR {snr} dB ({channels}ch {frames})");
            }
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[ignore = "requires PN_OPUS_LIB/PN_OPUS_SHA256 (fixed-point libopus 1.6.1)"]
fn planar_reads_duplicate_mono_and_refuse_other_rates() {
    let lib = load_for_tests();
    let dir = std::env::temp_dir().join(format!("pn-opus-pl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("m.opus");
    encode_to(&path, &lib, 1, &tone(9600, 1, 0.4), 1000);
    assert!(matches!(OpusReader::open(&lib, &path, Some(44_100.0)), Err(OpusError::SampleRate { .. })));
    let mut reader = OpusReader::open(&lib, &path, None).unwrap();
    let (mut l, mut r) = (vec![0f32; 4096], vec![0f32; 4096]);
    let n = reader.read_stereo_planar(&mut l, &mut r).unwrap();
    assert_eq!(n, 4096);
    assert_eq!(l, r);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Deterministic xorshift for the mutation tests (no external randomness).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
}

#[test]
fn random_damage_never_panics_and_truncation_is_never_a_success() {
    let good = stream(14);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..6000 {
        let mut data = good.clone();
        match round % 5 {
            0 => { for _ in 0..1 + rng.below(4) { let i = rng.below(data.len()); data[i] ^= 1 << rng.below(8); } }
            1 => { let cut = rng.below(data.len()); data.truncate(cut); assert!(scan_bytes(&data).is_err(), "truncated at {cut}"); continue; }
            2 => { let at = rng.below(data.len()); let n = rng.below(64); let junk: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect(); data.splice(at..at, junk); }
            3 => { let at = rng.below(data.len()); let n = rng.below(data.len() - at); data.drain(at..at + n); }
            _ => { for b in data.iter_mut().skip(rng.below(40)).step_by(1 + rng.below(30)) { *b = rng.next() as u8; } }
        }
        let _ = scan_bytes(&data); // any typed error or success is fine; a panic is not
    }
}

#[test]
fn damaged_pages_with_valid_checksums_never_panic_the_scanner() {
    // Structure fields are randomized but every checksum is recomputed, so the
    // scanner's own consistency checks (not the CRC) have to catch the damage.
    let good = stream(10);
    let mut reader = super::ogg::PageReader::new(&good[..]);
    let mut pages = Vec::new();
    let mut page = Page::default();
    while reader.read_page(&mut page).unwrap() { pages.push(page.clone()); }
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..4000 {
        let mut copy = pages.clone();
        for _ in 0..1 + rng.below(3) {
            let i = rng.below(copy.len());
            match rng.below(6) {
                0 => copy[i].granule = rng.next(),
                1 => copy[i].flags = (rng.next() & 7) as u8,
                2 => copy[i].sequence = rng.next() as u32,
                3 => copy[i].serial = rng.next() as u32,
                4 => { let n = rng.below(copy[i].body.len().max(1)); copy[i].body.truncate(n); let l = copy[i].lacing.len(); if l > 0 { copy[i].lacing[rng.below(l)] = rng.next() as u8; } }
                _ => { if copy.len() > 2 { copy.remove(i); } }
            }
        }
        // Page bodies must match their lacing sums to be encodable; fix the body length instead of the table.
        for p in &mut copy {
            let want: usize = p.lacing.iter().map(|&l| usize::from(l)).sum();
            p.body.resize(want, 0x5a);
        }
        let bytes: Vec<u8> = copy.iter().flat_map(|p| p.encode()).collect();
        let _ = scan_bytes(&bytes);
    }
}

#[test]
#[ignore = "requires PN_OPUS_LIB/PN_OPUS_SHA256 (fixed-point libopus 1.6.1)"]
fn corrupt_audio_packets_decode_or_fail_without_panicking() {
    let lib = load_for_tests();
    let dir = std::env::temp_dir().join(format!("pn-opus-fz-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v.opus");
    encode_to(&path, &lib, 2, &tone(48_000, 2, 0.5), 960);
    let good = std::fs::read(&path).unwrap();
    let mut reader = super::ogg::PageReader::new(&good[..]);
    let mut pages = Vec::new();
    let mut page = Page::default();
    while reader.read_page(&mut page).unwrap() { pages.push(page.clone()); }
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let bad = dir.join("bad.opus");
    for round in 0..300 {
        let mut copy = pages.clone();
        for p in copy.iter_mut().skip(2) {
            for _ in 0..1 + rng.below(6) {
                if p.body.is_empty() { break; }
                let i = rng.below(p.body.len());
                p.body[i] = rng.next() as u8;
            }
        }
        std::fs::write(&bad, copy.iter().flat_map(|p| p.encode()).collect::<Vec<u8>>()).unwrap();
        if let Ok(mut reader) = OpusReader::open(&lib, &bad, None) {
            let mut out = vec![0f32; 4096 * 2];
            let mut total = 0u64;
            loop {
                match reader.read(&mut out) {
                    Ok(0) => break,
                    Ok(n) => { total += n as u64; assert!(out[..n * 2].iter().all(|v| v.is_finite()), "round {round}"); }
                    Err(_) => break, // typed failure; the reader is then poisoned
                }
            }
            assert!(total <= reader.info().frames);
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
