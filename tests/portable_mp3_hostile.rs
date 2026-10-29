//! Hostile-input tests for the portable MP3 reader: whatever the bytes, opening and reading must neither
//! panic, hang, nor deliver an impossible number of frames, every refusal and read failure must be one of the
//! `OSStatus` values Apple produces, and the outcome must be deterministic.
//!
//! Seeded mutation of the golden fixtures (`tests/fixtures/mp3-exact`): truncation, byte and bit damage,
//! insertions and deletions, repeated and swapped ranges, spliced streams (rate, layer, version and channel
//! changes), forged headers, leading ID3 chains (including a version byte of 0xff, which hangs Apple's parser)
//! and pure noise. 20 000 cases run in memory against `Mp3Reader`; a subset runs through the public `Reader` on
//! files and must agree with the in-memory outcome.

#[path = "support/mp3_exact.rs"]
mod mp3_exact;

use mp3_exact::{BLOCK, Fixtures, Scratch};
use platinum_engine::portable::audio::mp3::reader::Mp3Reader;
use platinum_engine::portable::audio::{CodecError, Reader};
use std::time::{Duration, Instant};

const DTA: i32 = 0x6474_613f;
const WHT: i32 = 0x7768_743f;
const EOF: i32 = -39;
const FMT: i32 = 0x666d_743f;
const BADA: i32 = 0x6261_6461;

const ITERATIONS: usize = 20_000;
const FILE_ITERATIONS: usize = 1_500;
/// The longest any single case may take; a hang would never come back at all.
const CASE_LIMIT: Duration = Duration::from_secs(20);

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize }
    fn byte(&mut self) -> u8 { (self.next() >> 24) as u8 }
}

fn syncs(data: &[u8]) -> Vec<usize> {
    (0..data.len().saturating_sub(3)).filter(|&i| data[i] == 0xff && data[i + 1] & 0xe0 == 0xe0).collect()
}

fn mutate(rng: &mut Rng, original: &[u8], other: &[u8], case: usize) -> Vec<u8> {
    let mut data = original.to_vec();
    if data.is_empty() { data = vec![rng.byte()]; }
    match case % 14 {
        0 => data.truncate(rng.below(data.len())),
        1 => for _ in 0..1 + rng.below(30) { let i = rng.below(data.len()); data[i] = rng.byte(); },
        2 => { let at = rng.below(data.len()); let junk: Vec<u8> = (0..rng.below(1000)).map(|_| rng.byte()).collect(); data.splice(at..at, junk); }
        3 => { let at = rng.below(data.len()); let end = (at + rng.below(1500)).min(data.len()); data.drain(at..end); }
        4 => data = (0..rng.below(4000)).map(|_| rng.byte()).collect(),
        5 => for _ in 0..1 + rng.below(6) { let i = rng.below(data.len().min(900)); data[i] ^= 1 << rng.below(8); },
        6 => { let at = rng.below(data.len()); let end = (at + rng.below(1200)).min(data.len()); let copy = data[at..end].to_vec(); data.splice(at..at, copy); }
        7 => { let at = rng.below(data.len()); let end = (at + rng.below(600)).min(data.len()); let fill = if rng.below(2) == 0 { 0xff } else { 0x00 }; data[at..end].fill(fill); }
        8 => {
            data = (0..rng.below(4000) + 400).map(|_| rng.byte()).collect();
            for _ in 0..rng.below(20) { let i = rng.below(data.len().saturating_sub(4)); data[i] = 0xff; data[i + 1] = 0xfb; data[i + 2] = 0x90; }
        }
        9 => { // splice a range of another stream in (rate, layer, version or channel change mid-stream)
            let from = rng.below(other.len().max(1)); let to = (from + rng.below(3000)).min(other.len());
            let at = rng.below(data.len()); data.splice(at..at, other[from..to].iter().copied());
        }
        10 => { // leading ID3v2 header(s): random version/flags/size, chained, sometimes 0xff version bytes
            let mut prefix = Vec::new();
            for _ in 0..1 + rng.below(3) {
                let size = [0usize, 1, 10, 100, rng.below(5000)][rng.below(5)];
                prefix.extend_from_slice(b"ID3");
                prefix.extend_from_slice(&[[2, 3, 4, 0xff, rng.byte()][rng.below(5)], [0, 0, 0xff, rng.byte()][rng.below(4)], [0, 0x10, 0x40, 0x80, rng.byte()][rng.below(5)]]);
                prefix.extend_from_slice(&[(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
                prefix.extend((0..size.min(600)).map(|_| rng.byte()));
            }
            data.splice(0..0, prefix);
        }
        11 => { // swap two ranges
            let a = rng.below(data.len()); let b = rng.below(data.len());
            let len = rng.below(800).min(data.len() - a.max(b));
            for k in 0..len { data.swap(a + k, b + k); }
        }
        12 => { // damage frame headers only
            let positions = syncs(&data);
            for _ in 0..1 + rng.below(4) {
                if positions.is_empty() { break; }
                let at = positions[rng.below(positions.len())];
                data[at + 1 + rng.below(3)] ^= 1 << rng.below(8);
            }
        }
        _ => { // forge the first header: any version, layer, bitrate, rate, mode
            let positions = syncs(&data);
            let at = positions.first().copied().unwrap_or(0);
            if at + 4 <= data.len() {
                data[at] = 0xff;
                data[at + 1] = 0xe0 | (rng.byte() & 0x1f);
                data[at + 2] = rng.byte();
                data[at + 3] = rng.byte();
            }
        }
    }
    if data.len() > 4_096 { data.truncate(4_096); }
    data
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Refused(i32),
    Decoded { frames: u64, checksum: u64, error: Option<i32> },
}

fn fold(checksum: u64, sample: u32) -> u64 { checksum.wrapping_mul(0x100_0000_01b3) ^ u64::from(sample) }

/// Open and drain `bytes` through the exact reader in memory.
fn run_memory(tag: &str, bytes: &[u8]) -> Outcome {
    let start = Instant::now();
    let outcome = match Mp3Reader::open(bytes.to_vec()) {
        Err(error) => {
            let status = error.status();
            assert!([DTA, WHT, EOF, FMT].contains(&status), "{tag}: open refused with an unexpected status {status:#x}");
            Outcome::Refused(status)
        }
        Ok(mut reader) => {
            let channels = reader.channels();
            let mut buffer = vec![0i16; BLOCK * channels];
            // 24 bytes is the smallest possible frame, each holds at most 1152 samples, plus the decoder-delay tail.
            let limit = (bytes.len() / 24 + 3) as u64 * 1152;
            let (mut frames, mut checksum, mut error) = (0u64, 0u64, None);
            loop {
                match reader.read(&mut buffer, BLOCK) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &sample in &buffer[..n * channels] { checksum = fold(checksum, u32::from(sample as u16)); }
                        frames += n as u64;
                        assert!(frames <= limit, "{tag}: {frames} frames from {} bytes", bytes.len());
                    }
                    Err(e) => { error = Some(e.status()); break; }
                }
            }
            if let Some(status) = error { assert!([DTA, BADA].contains(&status), "{tag}: read failed with an unexpected status {status:#x}"); }
            Outcome::Decoded { frames, checksum, error }
        }
    };
    assert!(start.elapsed() < CASE_LIMIT, "{tag}: took {:?} for {} bytes", start.elapsed(), bytes.len());
    outcome
}

/// The same through the public façade on a file, in planar form as the engine reads.
fn run_file(scratch: &Scratch, tag: &str, bytes: &[u8]) -> Outcome {
    let path = scratch.write("case.mp3", bytes);
    let mut reader = match Reader::open(&path, None) {
        Err(CodecError::Native { operation: "ExtAudioFileOpenURL" | "ExtAudioFileSetProperty(client format)", status }) => return Outcome::Refused(status),
        // A file named .mp3 goes to the MPEG reader whatever its bytes are (AudioFile commits to the MPEG types for that
        // extension): ADTS-, MP4- or WAVE-looking damage is refused by the MPEG reader like any other noise.
        Err(other) => panic!("{tag}: unexpected open error {other:?}"),
        Ok(reader) => reader,
    };
    let rate = reader.source_format().sample_rate;
    reader.set_stereo_planar(rate).unwrap();
    let mono = reader.source_format().channels_per_frame == 1;
    let (mut left, mut right) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
    let (mut frames, mut checksum, mut error) = (0u64, 0u64, None);
    loop {
        match reader.read_stereo_planar(&mut left, &mut right) {
            Ok(0) => break,
            Ok(n) => {
                for i in 0..n {
                    assert!(left[i].is_finite() && right[i].is_finite() && left[i].abs() <= 1.0 && right[i].abs() <= 1.0, "{tag}: out-of-range sample");
                    // The same int16 values the in-memory run folds, taken from the float plane.
                    checksum = fold(checksum, u32::from((left[i] * 32768.0) as i32 as i16 as u16));
                    if !mono { checksum = fold(checksum, u32::from((right[i] * 32768.0) as i32 as i16 as u16)); }
                }
                frames += n as u64;
            }
            Err(CodecError::Native { status, .. }) => { error = Some(status); break; }
            Err(other) => panic!("{tag}: unexpected read error {other:?}"),
        }
    }
    Outcome::Decoded { frames, checksum, error }
}

#[test]
fn twenty_thousand_mutated_streams_never_panic_hang_or_misreport() {
    let fixtures = Fixtures::load();
    let sources: Vec<&[u8]> = fixtures.files.iter().filter(|f| (300..=3_500).contains(&f.length)).map(|f| fixtures.bytes(f)).collect();
    assert!(sources.len() >= 60, "{} small fixtures to mutate", sources.len());
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut refused, mut decoded, mut failed, mut nonempty) = (0usize, 0usize, 0usize, 0usize);
    let started = Instant::now();
    let scratch = Scratch::new("hostile");
    for case in 0..ITERATIONS {
        let original = sources[rng.below(sources.len())];
        let other = sources[rng.below(sources.len())];
        let data = mutate(&mut rng, original, other, case);
        let tag = format!("case {case}");
        let first = run_memory(&tag, &data);
        match &first {
            Outcome::Refused(_) => refused += 1,
            Outcome::Decoded { frames, error, .. } => { decoded += 1; failed += usize::from(error.is_some()); nonempty += usize::from(*frames > 0); }
        }
        if case % 16 == 0 { assert_eq!(first, run_memory(&tag, &data), "{tag}: decoding is not deterministic"); }
        if case % (ITERATIONS / FILE_ITERATIONS) == 0 {
            let through_file = run_file(&scratch, &tag, &data);
            // The façade reads planar stereo and folds both planes (mono once), the memory run folds source samples: same
            // checksum for mono and stereo alike, and the same frames and statuses.
            assert_eq!(through_file, first, "{tag}: the public Reader disagrees with the in-memory reader");
        }
    }
    println!("{ITERATIONS} cases in {:.1?}: {refused} refused at open, {decoded} decoded ({nonempty} with audio, {failed} ending in a read failure)", started.elapsed());
    assert!(refused > 1000 && decoded > 1000 && failed > 100 && nonempty > 1000, "refused {refused}, decoded {decoded}, failed {failed}, with audio {nonempty}");
}

#[test]
fn degenerate_inputs_are_refused_cleanly() {
    let scratch = Scratch::new("degenerate");
    let empty: &[u8] = &[];
    for (name, bytes) in [("empty", empty), ("one", &[0xff][..]), ("sync-only", &[0xff, 0xfb]), ("zeros", &[0u8; 5000]), ("ones", &[0xffu8; 5000])] {
        let outcome = run_memory(name, bytes);
        assert!(matches!(outcome, Outcome::Refused(_)), "{name}: {outcome:?}");
        assert_eq!(run_file(&scratch, name, bytes), outcome, "{name}");
    }
    // A claimed gigantic ID3v2 tag with nothing behind it.
    let mut id3 = b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f".to_vec();
    id3.extend([0u8; 64]);
    assert!(matches!(run_memory("id3-huge", &id3), Outcome::Refused(_)));
    // Apple hangs on a chained ID3 header whose version byte is 0xff; this reader refuses it.
    let mut chain = b"ID3\x04\x00\x00\x00\x00\x00\x14".to_vec(); // a 20-byte tag, then a second header
    chain.extend([0u8; 20]);
    chain.extend_from_slice(b"ID3\xff\x00\x00\x00\x00\x00\x00");
    chain.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
    chain.resize(1000, 0);
    assert!(matches!(run_memory("id3-chain-ff", &chain), Outcome::Refused(DTA)));
    // A Xing/Info header claiming 4 billion frames in front of real frames delivers only what exists.
    let fixtures = Fixtures::load();
    let mut checked = 0;
    for fixture in fixtures.files.iter().filter(|f| f.has("family:tags_xing") && f.opens()) {
        let mut file = fixtures.bytes(fixture).to_vec();
        let Some(tag) = file.windows(4).position(|w| w == b"Info" || w == b"Xing") else { continue };
        if tag + 12 > file.len() { continue; }
        file[tag + 8..tag + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        let frames_in_file = syncs(&file).len() as u64;
        match run_memory("xing-huge", &file) {
            Outcome::Decoded { frames, .. } => assert!(frames <= (frames_in_file + 2) * 1152, "{}: {frames} frames from about {frames_in_file} sync words", fixture.id),
            Outcome::Refused(_) => {}
        }
        checked += 1;
    }
    assert!(checked >= 5, "{checked} Xing/Info fixtures");
}
