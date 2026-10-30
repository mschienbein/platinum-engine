//! Whole-file SHA-256 pins of the portable ALAC writer (`Writer` with `OutputFormat::Alac`).
//!
//! Every digest is that of the complete `.m4a` the v0.3.0 writer produced for a generated input, with the MP4 clock
//! fixed to [`CLOCK`] (the creation and modification fields of `mvhd`, `tkhd` and `mdhd` are the only bytes that depend
//! on it). The writer, its packet encoder and the MP4 helpers it shares with the AAC writer must keep producing exactly
//! these files, so any change to a digest here is a change to the bytes users get and needs a reason of its own.
//!
//! The inputs are generated in this file from integer arithmetic and IEEE operations only: an xorshift64* generator and
//! an integer triangle wave, converted to `f32` exactly and scaled by powers of two, with at most one correctly rounded
//! addition per sample. They are identical on every host; no audio files and no libm are involved. The writer takes
//! interleaved `f32` frames, so the signal is shaped for each recorder depth: a half-scale tone with noise a few
//! hundred quantizer steps wide, quiet passages a few steps above the quantizer step, digital silence, white noise
//! (full scale where the writer can store the uncompressed packet) and clipped passages.

use platinum_engine::portable::audio::{OutputFormat, Writer, WriterConfig, set_mp4_header_times};
use platinum_engine::portable::digest::sha256;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Seconds since 1904 written to the MP4 header times of every file.
const CLOCK: u32 = 3_800_000_001;
/// Frames of one segment of the generated signal: half a packet.
const SEGMENT: usize = 2048;

// ---- generated input ------------------------------------------------------------------------------------------------

/// xorshift64* seeded through one splitmix64 round.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Self((z ^ (z >> 31)) | 1)
    }
    fn step(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `[-2^23, 2^23)`, exactly representable as `f32`.
    fn signed24(&mut self) -> i32 { (self.step() >> 40) as i32 - (1 << 23) }
}

/// `2^exp` as an `f32`, built from its bits so that no library function is involved.
fn pow2(exp: i32) -> f32 {
    assert!((-126..=127).contains(&exp));
    f32::from_bits(((127 + exp) as u32) << 23)
}

/// Integer triangle wave of a 32-bit phase, in `[-2^23, 2^23)`.
fn triangle(phase: u32) -> i32 {
    let folded = if phase >> 31 == 0 { phase } else { !phase };
    (folded >> 7) as i32 - (1 << 23)
}

/// Per-channel phase increments: channels 0 and 1 share a pitch (a correlated pair), the others differ.
const STEPS: [u32; 8] = [0x00A3_D70B, 0x00A3_D70B, 0x0121_4F8D, 0x01D6_B2C9, 0x0067_E4A1, 0x02F1_0C35, 0x0039_B8D3, 0x0188_2E57];

#[derive(Clone, Copy)]
enum Kind {
    /// Triangle wave of half scale with noise a few hundred quantizer steps wide.
    Tone,
    /// Triangle wave of a hundred quantizer steps with noise of a few.
    Quiet,
    /// Digital silence.
    Silence,
    /// White noise with a peak of 2^12 quantizer steps, which compresses.
    Hiss,
    /// Full-scale white noise, which cannot be compressed: the encoder stores such a packet uncompressed.
    Noise,
    /// Triangle wave of peak 2.0, clamped by the quantizer.
    Clipped,
}

/// The signal is a cycle of 16 segments of [`SEGMENT`] frames (two segments are one packet). The seed's low four bits
/// select the segment it starts at, so across the cases every kind appears alone in a packet, next to others in one,
/// and across packet boundaries.
const SCHEDULE: [Kind; 16] = {
    use Kind::*;
    [Tone, Tone, Noise, Noise, Quiet, Hiss, Clipped, Tone, Silence, Silence, Tone, Quiet, Noise, Hiss, Hiss, Clipped]
};

/// Frames of generated signal in a file: two cycles of the schedule. No short case is that long; the long ones go on as
/// digital silence, which the encoder takes much faster than audio, to keep a debug build quick.
const ACTIVE: usize = 2 * 16 * SEGMENT;

/// `frames` interleaved frames for a recorder of `bits` (16, 24 or 32) significant bits: the first [`ACTIVE`] frames of
/// the schedule, then silence. The signal depends only on the arguments, never on how it is later split into writes.
fn generate(seed: u64, bits: i32, channels: usize, frames: usize) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut phase = [0u32; 8];
    let lsb = 1 - bits; // the quantizer step is 2^lsb
    // A full-scale noise packet exceeds the output capacity AudioToolbox allows (the writer refuses it) once a frame
    // takes 16 bytes or more; such frames get hiss instead.
    let full_scale_fits = channels * (bits as usize / 8) < 16;
    let mut out = Vec::with_capacity(frames * channels);
    // peak 0.5, noise of about 256 steps; peak 128 steps, noise of 8; peak 2^12 steps; full scale; peak 2.0
    let (tone, tone_noise) = (pow2(-24), pow2(lsb + 8 - 23));
    let (quiet, quiet_noise) = (pow2(lsb + 7 - 23), pow2(lsb + 3 - 23));
    let (hiss, noise, clipped) = (pow2(lsb + 12 - 23), pow2(-23), pow2(-22));
    for frame in 0..frames.min(ACTIVE) {
        let kind = match SCHEDULE[(frame / SEGMENT + seed as usize) % SCHEDULE.len()] {
            Kind::Noise if !full_scale_fits => Kind::Hiss,
            kind => kind,
        };
        for (c, ph) in phase.iter_mut().enumerate().take(channels) {
            *ph = ph.wrapping_add(STEPS[c]);
            let wave = triangle(*ph) as f32;
            out.push(match kind {
                Kind::Tone => wave * tone + rng.signed24() as f32 * tone_noise,
                Kind::Quiet => wave * quiet + rng.signed24() as f32 * quiet_noise,
                Kind::Silence => 0.0,
                Kind::Hiss => rng.signed24() as f32 * hiss,
                Kind::Noise => rng.signed24() as f32 * noise,
                Kind::Clipped => wave * clipped,
            });
        }
    }
    // a file longer than that only has to be long: the rest is digital silence
    out.resize(frames * channels, 0.0);
    out
}

// ---- the writer under test ------------------------------------------------------------------------------------------

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A private directory that is removed again, with whatever the writer left in it.
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("platinum-alac-digests-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).expect("create scratch directory");
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

/// Top-level atoms of a file with no audio: the movie header only.
const EMPTY: &[&str] = &["ftyp", "moov"];
/// Top-level atoms of a file whose `moov` fits in front of the media data, with a `free` atom filling the reservation.
const MOOV_FIRST: &[&str] = &["ftyp", "moov", "free", "mdat"];
/// A `moov` that fills the reservation exactly, so that no `free` atom is needed.
const MOOV_FILLS_RESERVATION: &[&str] = &["ftyp", "moov", "mdat"];
/// A `moov` too large for the reservation: it follows the media data.
const MOOV_LAST: &[&str] = &["ftyp", "free", "mdat", "moov"];

struct Case {
    name: &'static str,
    rate: f64,
    channels: u32,
    /// The recorder depth passed to the writer; 0 is its default.
    depth: u32,
    frames: usize,
    /// Seeds the noise and, modulo 16, the segment of the signal's schedule that the file starts at.
    seed: u64,
    /// The top-level atoms of the file, in order.
    atoms: &'static [&'static str],
    /// SHA-256 of the whole file written by the v0.3.0 writer, in lower-case hexadecimal.
    sha256: &'static str,
}

impl Case {
    fn signal(&self) -> Vec<f32> {
        let bits = if self.depth == 0 { 16 } else { self.depth as i32 };
        generate(self.seed, bits, self.channels as usize, self.frames)
    }

    /// The finished file in `block`-frame writes with the clock fixed.
    fn file(&self, signal: &[f32], block: usize) -> Vec<u8> {
        let scratch = Scratch::new();
        let path = scratch.0.join("out.m4a");
        let config = WriterConfig { format: OutputFormat::Alac, sample_rate: self.rate, channels: self.channels, bit_depth: self.depth };
        let mut writer = Writer::create(&path, config).unwrap_or_else(|e| panic!("{}: create: {e}", self.name));
        assert!(writer.warnings().is_empty(), "{}", self.name);
        for chunk in signal.chunks(block * self.channels as usize) {
            writer.write(chunk).unwrap_or_else(|e| panic!("{}: write: {e}", self.name));
        }
        writer.finish().unwrap_or_else(|e| panic!("{}: finish: {e}", self.name));
        set_mp4_header_times(&path, CLOCK).unwrap_or_else(|e| panic!("{}: clock: {e}", self.name));
        fs::read(&path).unwrap()
    }

    /// What differs from the pinned file when written in `block`-frame writes, if anything.
    fn difference(&self, signal: &[f32], block: usize) -> Option<String> {
        let file = self.file(signal, block);
        let sha = hex(&sha256(&file));
        if sha == self.sha256 && atoms(&file) == self.atoms { return None; }
        Some(format!("{} in writes of {block} frames: {} bytes, atoms {:?}, sha256 {sha} (pinned {}, atoms {:?})",
            self.name, file.len(), atoms(&file), self.sha256, self.atoms))
    }
}

fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }

/// The top-level atoms of an MP4 file, in order.
fn atoms(file: &[u8]) -> Vec<String> {
    let mut kinds = Vec::new();
    let mut at = 0;
    while at < file.len() {
        kinds.push(String::from_utf8_lossy(&file[at + 4..at + 8]).into_owned());
        let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        assert!(size >= 8, "atom of {size} bytes at offset {at}");
        at += size;
    }
    assert_eq!(at, file.len());
    kinds
}

// ---- pinned digests -------------------------------------------------------------------------------------------------

/// Rates 22,050 to 96,000 Hz (one of them fractional), 1 to 8 channels, recorder depths 0, 16, 24 and 32, and frame
/// counts around the 4,096-frame packet.
const CASES: &[Case] = &[
    Case { name: "empty_22050_mono_depth0", rate: 22_050.0, channels: 1, depth: 0, frames: 0, seed: 1, atoms: EMPTY, sha256: "e5e4385ab57a670d8f09cca2163c404e1bb415df2750b6af130425c31591ab5c" },
    Case { name: "one_frame_44100_stereo_depth16", rate: 44_100.0, channels: 2, depth: 16, frames: 1, seed: 16, atoms: MOOV_FIRST, sha256: "6c6252ad506a19f9934b9fdebed736ee0bf155a5836669c061ab8eda55ef6ab8" },
    Case { name: "one_frame_96000_8ch_depth32", rate: 96_000.0, channels: 8, depth: 32, frames: 1, seed: 32, atoms: MOOV_FIRST, sha256: "b87ef03924e330375e9014b8e6bbf8ece38d864c91e6260fbc37d10811b9731b" },
    Case { name: "short_packet_48000_6ch_depth24", rate: 48_000.0, channels: 6, depth: 24, frames: 4095, seed: 6, atoms: MOOV_FIRST, sha256: "1cf16bbf75a1129cc5d950e15373c36868525fc79da6a03b93e2e26d73fe562f" },
    Case { name: "full_packet_96000_8ch_depth32", rate: 96_000.0, channels: 8, depth: 32, frames: 4096, seed: 36, atoms: MOOV_FIRST, sha256: "663bbc268a35410a070ff4f15c86f13207a7b034f1567a74662f9eef9eea3e69" },
    Case { name: "full_packet_plus_one_44100_3ch_depth0", rate: 44_100.0, channels: 3, depth: 0, frames: 4097, seed: 2, atoms: MOOV_FIRST, sha256: "baeb819b97204dcfbd20dd1043d13cbe1750055c7e1c7ded55d99261fd20be1e" },
    Case { name: "short_packet_22050_stereo_depth24", rate: 22_050.0, channels: 2, depth: 24, frames: 4095, seed: 18, atoms: MOOV_FIRST, sha256: "69bfef0c6ed996615153e4f10b03cd55ae69b52d8c386d7ff5b2c73421aae2df" },
    Case { name: "full_packet_48000_mono_depth16", rate: 48_000.0, channels: 1, depth: 16, frames: 4096, seed: 48, atoms: MOOV_FIRST, sha256: "85b2d4d5ee71d905572867e62f466a64262e056296c6c20a25a8e2465b6c83e8" },
    Case { name: "full_packet_plus_one_96000_mono_depth24", rate: 96_000.0, channels: 1, depth: 24, frames: 4097, seed: 14, atoms: MOOV_FIRST, sha256: "661903f25fc7063417198ffab833b6386cb4dfbcd9cc9c9d12f3b7e884b93922" },
    Case { name: "multi_packet_44100_stereo_depth16", rate: 44_100.0, channels: 2, depth: 16, frames: 20_000, seed: 3, atoms: MOOV_FIRST, sha256: "12202b63831e8a12ffde6c8d2ca07073da60e6103458f33d39bac2af5c22b98d" },
    Case { name: "multi_packet_48000_4ch_depth24", rate: 48_000.0, channels: 4, depth: 24, frames: 12_289, seed: 64, atoms: MOOV_FIRST, sha256: "a953ee1a96a30aad89ef587ff3db1a6ce9af1298730e30d669d31c72c548c515" },
    Case { name: "multi_packet_44100_5ch_depth32", rate: 44_100.0, channels: 5, depth: 32, frames: 9_000, seed: 8, atoms: MOOV_FIRST, sha256: "07f6bdd8b102efecda94e669915d7fa665391ea05a8ce2ce6c1024bce5d71468" },
    Case { name: "multi_packet_96000_7ch_depth16", rate: 96_000.0, channels: 7, depth: 16, frames: 8_193, seed: 34, atoms: MOOV_FIRST, sha256: "f541bba7a545e1c9489b39abb64cfbf2fd9a3485aab8b6a762367fbfb7431f37" },
    Case { name: "multi_packet_22050_3ch_depth32", rate: 22_050.0, channels: 3, depth: 32, frames: 16_383, seed: 30, atoms: MOOV_FIRST, sha256: "2a0fe8e027adcae042b9a6717cece391d08cbd9803ae6dae845d537ebf7ae140" },
    Case { name: "fractional_rate_44100_5_mono_depth0", rate: 44_100.5, channels: 1, depth: 0, frames: 5_000, seed: 24, atoms: MOOV_FIRST, sha256: "5e81cc68402d10cd1e6415d387c3e9f034ff229f43924a3726f296a940a18a6f" },
];

/// Mono at 22,050 Hz is the cheapest way past the sample-table reservation the writer makes before the media data (301
/// seconds' worth of packets plus the fixed boxes, rounded up to a 4 KiB boundary). Where the `moov` stands against it:
/// the last file with room for only the header of a `free` atom, the first that fills it exactly (no `free` atom), and
/// the first that no longer fits and follows `mdat`.
const LONG: &[Case] = &[
    Case { name: "long_mono_22050_moov_leaves_free_atom_header", rate: 22_050.0, channels: 1, depth: 0, frames: 8_785_920, seed: 9, atoms: MOOV_FIRST, sha256: "fa406ae1a7e3dad6099aba1be88f81dc597fb4415f7a17712be7c6db7450b1fc" },
    Case { name: "long_mono_22050_moov_fills_reservation", rate: 22_050.0, channels: 1, depth: 0, frames: 8_785_921, seed: 11, atoms: MOOV_FILLS_RESERVATION, sha256: "9e26de4e80ef8d8d3ffc9b1ea71ef7a81f5d4a5cd93c0580f2c0c9872fdd78eb" },
    Case { name: "long_mono_22050_moov_last", rate: 22_050.0, channels: 1, depth: 0, frames: 8_790_016, seed: 5, atoms: MOOV_LAST, sha256: "bed536a710834086b8e5eb1b5a3ef99964c733167cff4a0c732ddc43048a90b6" },
];

/// Every difference from the pinned files when `case` is written in writes of each of `blocks` frames.
fn differences(case: &Case, blocks: &[usize]) -> Vec<String> {
    let signal = case.signal();
    blocks.iter().filter_map(|&block| case.difference(&signal, block)).collect()
}

#[test]
fn whole_file_digests_are_those_of_v0_3_0() {
    let wrong: Vec<String> = CASES.iter().flat_map(|case| differences(case, &[case.frames.max(1)])).collect();
    assert!(wrong.is_empty(), "ALAC output changed:\n{}", wrong.join("\n"));
}

#[test]
fn digests_do_not_depend_on_how_the_input_is_cut_into_writes() {
    let mut wrong = Vec::new();
    for name in ["multi_packet_44100_stereo_depth16", "multi_packet_48000_4ch_depth24", "multi_packet_22050_3ch_depth32"] {
        let case = CASES.iter().find(|case| case.name == name).unwrap();
        wrong.extend(differences(case, &[1, 7, 512, 4095, 4096, 4097, 5000]));
    }
    assert!(wrong.is_empty(), "ALAC output depends on the write size:\n{}", wrong.join("\n"));
}

#[test]
fn long_files_around_the_movie_atom_reservation_are_unchanged() {
    let wrong: Vec<String> = std::thread::scope(|scope| {
        let runs: Vec<_> = LONG.iter().map(|case| scope.spawn(move || differences(case, &[case.frames]))).collect();
        runs.into_iter().flat_map(|run| run.join().unwrap()).collect()
    });
    assert!(wrong.is_empty(), "ALAC output changed:\n{}", wrong.join("\n"));
}
