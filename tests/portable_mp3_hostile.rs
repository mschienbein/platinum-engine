//! Hostile-input tests for the opt-in portable MP3 decoder: whatever the bytes, opening and reading
//! must neither panic, hang, nor produce non-finite or out-of-range samples, and the outcome must
//! be deterministic.

use platinum_engine::portable::audio::{Reader, ReaderOptions};
use std::path::PathBuf;

const OPT_IN: ReaderOptions = ReaderOptions { non_identical_mp3: true };

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

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mp3").join(format!("{name}.mp3"))).unwrap()
}

/// Open and drain `bytes`; returns Err(kind) when refused at open or during reading, else the
/// number of frames and a checksum of the sample bits.
fn run(tag: &str, bytes: &[u8]) -> Result<(usize, u64), String> {
    let path = std::env::temp_dir().join(format!("platinum-mp3-hostile-{}-{tag}.mp3", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    let outcome = (|| {
        let mut reader = Reader::open_with(&path, None, OPT_IN).map_err(|e| format!("open: {e}"))?;
        reader.set_stereo_planar(reader.source_format().sample_rate).map_err(|e| format!("planar: {e}"))?;
        let (mut left, mut right) = (vec![0f32; 1024], vec![0f32; 1024]);
        let (mut frames, mut checksum) = (0usize, 0u64);
        // 24 bytes is the smallest possible frame and each frame holds at most 1152 samples.
        let limit = bytes.len() / 24 * 1152 + 1152;
        loop {
            let n = reader.read_stereo_planar(&mut left, &mut right).map_err(|e| format!("read: {e}"))?;
            if n == 0 { return Ok((frames, checksum)); }
            for (l, r) in left[..n].iter().zip(&right[..n]) {
                assert!(l.is_finite() && r.is_finite() && l.abs() <= 1.0 && r.abs() <= 1.0, "{tag}: out-of-range sample {l} {r}");
                checksum = checksum.wrapping_mul(0x100000001b3) ^ u64::from(l.to_bits()) ^ (u64::from(r.to_bits()) << 17);
            }
            frames += n;
            assert!(frames <= limit, "{tag}: produced {frames} frames from {} bytes", bytes.len());
        }
    })();
    let _ = std::fs::remove_file(&path);
    outcome
}

fn mutate(rng: &mut Rng, original: &[u8], case: usize) -> Vec<u8> {
    let mut data = original.to_vec();
    match case % 9 {
        0 => data.truncate(rng.below(data.len())),
        1 => for _ in 0..1 + rng.below(30) { let i = rng.below(data.len()); data[i] = rng.byte(); },
        2 => { let at = rng.below(data.len()); let junk: Vec<u8> = (0..rng.below(2000)).map(|_| rng.byte()).collect(); data.splice(at..at, junk); }
        3 => { let at = rng.below(data.len()); let end = (at + rng.below(1500)).min(data.len()); data.drain(at..end); }
        4 => data = (0..rng.below(8000)).map(|_| rng.byte()).collect(),
        5 => { // flip single bits only inside the first frames' headers and side information
            for _ in 0..1 + rng.below(6) { let i = rng.below(data.len().min(900)); data[i] ^= 1 << rng.below(8); }
        }
        6 => { // duplicate a range
            let at = rng.below(data.len()); let end = (at + rng.below(1200)).min(data.len());
            let copy = data[at..end].to_vec(); data.splice(at..at, copy);
        }
        7 => { // overwrite a run with 0xFF or 0x00
            let at = rng.below(data.len()); let end = (at + rng.below(600)).min(data.len());
            let fill = if rng.below(2) == 0 { 0xff } else { 0x00 };
            for b in &mut data[at..end] { *b = fill; }
        }
        _ => { // sync words scattered through random data
            data = (0..rng.below(4000) + 400).map(|_| rng.byte()).collect();
            for _ in 0..rng.below(20) { let i = rng.below(data.len().saturating_sub(4)); data[i] = 0xff; data[i + 1] = 0xfb; data[i + 2] = 0x90; }
        }
    }
    data
}

#[test]
fn mutated_and_random_files_never_panic_and_are_deterministic() {
    let sources = [fixture("lame_cbr128_44100_js"), fixture("lame_vbrV2_22050_js"), fixture("lame_cbr32_11025_mono"), fixture("tagless_cbr128_44100")];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut accepted, mut refused) = (0usize, 0usize);
    for case in 0..252 {
        let data = mutate(&mut rng, &sources[case % sources.len()], case / sources.len());
        let tag = format!("c{case}");
        let first = run(&tag, &data);
        assert_eq!(first, run(&tag, &data), "case {case}: decoding is not deterministic");
        if first.is_ok() { accepted += 1 } else { refused += 1 }
    }
    assert!(accepted > 0 && refused > 0, "accepted {accepted}, refused {refused}");
}

#[test]
fn degenerate_inputs_are_refused_cleanly() {
    let empty: &[u8] = &[];
    assert!(run("empty", empty).is_err());
    assert!(run("one", &[0xff]).is_err());
    assert!(run("sync-only", &[0xff, 0xfb]).is_err());
    assert!(run("zeros", &[0u8; 5000]).is_err());
    assert!(run("ones", &[0xffu8; 5000]).is_err());
    // A claimed gigantic ID3v2 tag with nothing behind it.
    let mut id3 = b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f".to_vec();
    id3.extend([0u8; 64]);
    assert!(run("id3-huge", &id3).is_err());
    // A Xing header claiming 4 billion frames in front of three real frames.
    let mut file = fixture("lame_cbr128_44100_js");
    let tag = file.windows(4).position(|w| w == b"Info" || w == b"Xing").expect("fixture has an info frame");
    file[tag + 8..tag + 12].copy_from_slice(&u32::MAX.to_be_bytes());
    file.truncate(417 * 4);
    let result = run("xing-huge", &file);
    assert!(result.is_err() || result.as_ref().is_ok_and(|(frames, _)| *frames <= 4 * 1152), "{result:?}");
}
