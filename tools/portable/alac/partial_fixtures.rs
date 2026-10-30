//! Deterministic PCM for the ALAC partial-packet and oversized-packet cases. Shared by
//! `tests/portable_alac_packets.rs` and `tools/portable/alac/partial_probe.rs`
//! (which records the AudioToolbox output the test pins). Integer arithmetic for the
//! packet cases and exactly rounded single `f32` operations for the float cases, so the
//! samples are identical on every platform.

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// Uniform noise in `[-amp, amp]`.
    Noise,
    /// Per-channel triangle waves of different periods plus faint noise.
    Wave,
    /// A triangle wave in the first channel, the others near copies of it.
    Correlated,
    /// Mostly digital silence with occasional values (zero runs).
    Sparse,
}

#[derive(Clone, Copy, Debug)]
pub struct Case {
    pub name: &'static str,
    pub channels: u8,
    pub depth: u8,
    pub frames: usize,
    pub kind: Kind,
    pub seed: u32,
    /// Peak magnitude in right-justified samples of at most 24 bits.
    pub amp: i32,
}

fn next(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn uniform(state: &mut u32, amp: i32) -> i32 {
    if amp == 0 { return 0; }
    (next(state) % (2 * amp as u32 + 1)) as i32 - amp
}

fn triangle(position: usize, period: usize, amp: i32) -> i32 {
    let half = period / 2;
    let phase = position % period;
    let ramp = if phase < half { phase } else { period - phase } as i64;
    ((ramp * 2 - half as i64) * i64::from(amp) / half as i64) as i32
}

/// Interleaved right-justified PCM of `case.frames` frames. Depth 32 carries 24-bit
/// values in its upper bits, as the float path the recorder feeds AudioToolbox can.
pub fn pcm(case: &Case) -> Vec<i32> {
    let channels = usize::from(case.channels);
    let effective = u32::from(case.depth).min(24);
    let limit = (1i32 << (effective - 1)) - 1;
    let amp = case.amp.min(limit);
    let mut state = case.seed | 1;
    let mut out = Vec::with_capacity(case.frames * channels);
    for frame in 0..case.frames {
        let first = triangle(frame, 97, amp);
        for channel in 0..channels {
            let value = match case.kind {
                Kind::Noise => uniform(&mut state, amp),
                Kind::Wave => triangle(frame + 31 * channel, 61 + 29 * channel, amp) + uniform(&mut state, amp / 64),
                Kind::Correlated => if channel == 0 { first } else { first - first / 8 * channel as i32 + uniform(&mut state, amp / 128) },
                Kind::Sparse => if next(&mut state) % 11 == 0 { uniform(&mut state, amp) } else { 0 },
            };
            let value = value.clamp(-limit - 1, limit);
            out.push(if case.depth == 32 { value << 8 } else { value });
        }
    }
    out
}

/// Packets of these cases are recorded from AudioToolbox. The first 34 end in a partial packet
/// of 14 to 268 frames that the v0.3.0 encoder wrote differently (another predictor order or
/// compression where AudioToolbox escapes, or the reverse); the next 7 are partial packets of 2 to
/// 699 frames it already reproduced (mono, long, or no differing decision); the last 4 are an exact
/// multiple of 4096 frames and packets of 1 to 8 frames.
pub const CASES: &[Case] = &[
    Case { name: "sparse-2ch-16b-88", channels: 2, depth: 16, frames: 88, kind: Kind::Sparse, seed: 3012081834, amp: 14 },
    Case { name: "correlated-2ch-20b-4144", channels: 2, depth: 20, frames: 4144, kind: Kind::Correlated, seed: 3054512982, amp: 185743 },
    Case { name: "wave-2ch-32b-18", channels: 2, depth: 32, frames: 18, kind: Kind::Wave, seed: 3378491079, amp: 4362 },
    Case { name: "sparse-3ch-20b-32", channels: 3, depth: 20, frames: 32, kind: Kind::Sparse, seed: 2523619579, amp: 4056 },
    Case { name: "noise-3ch-20b-81", channels: 3, depth: 20, frames: 81, kind: Kind::Noise, seed: 1478763951, amp: 9 },
    Case { name: "correlated-3ch-20b-4126", channels: 3, depth: 20, frames: 4126, kind: Kind::Correlated, seed: 132210396, amp: 4696 },
    Case { name: "noise-3ch-24b-4211", channels: 3, depth: 24, frames: 4211, kind: Kind::Noise, seed: 1714387292, amp: 5 },
    Case { name: "sparse-3ch-32b-135", channels: 3, depth: 32, frames: 135, kind: Kind::Sparse, seed: 1410717003, amp: 1654738 },
    Case { name: "noise-4ch-20b-72", channels: 4, depth: 20, frames: 72, kind: Kind::Noise, seed: 519610446, amp: 11 },
    Case { name: "noise-4ch-24b-27", channels: 4, depth: 24, frames: 27, kind: Kind::Noise, seed: 3469679182, amp: 2497 },
    Case { name: "sparse-5ch-20b-14", channels: 5, depth: 20, frames: 14, kind: Kind::Sparse, seed: 676936392, amp: 12699 },
    Case { name: "sparse-5ch-24b-8436", channels: 5, depth: 24, frames: 8436, kind: Kind::Sparse, seed: 1372191209, amp: 472791 },
    Case { name: "sparse-5ch-32b-61", channels: 5, depth: 32, frames: 61, kind: Kind::Sparse, seed: 1085984116, amp: 241679 },
    Case { name: "noise-6ch-16b-56", channels: 6, depth: 16, frames: 56, kind: Kind::Noise, seed: 3479698675, amp: 85 },
    Case { name: "sparse-6ch-20b-111", channels: 6, depth: 20, frames: 111, kind: Kind::Sparse, seed: 2469637717, amp: 18817 },
    Case { name: "noise-6ch-20b-155", channels: 6, depth: 20, frames: 155, kind: Kind::Noise, seed: 1012622001, amp: 7108 },
    Case { name: "correlated-6ch-20b-180", channels: 6, depth: 20, frames: 180, kind: Kind::Correlated, seed: 252683763, amp: 64623 },
    Case { name: "correlated-6ch-24b-4136", channels: 6, depth: 24, frames: 4136, kind: Kind::Correlated, seed: 1493360576, amp: 1631808 },
    Case { name: "correlated-6ch-24b-8460", channels: 6, depth: 24, frames: 8460, kind: Kind::Correlated, seed: 3853042992, amp: 334 },
    Case { name: "sparse-6ch-32b-111", channels: 6, depth: 32, frames: 111, kind: Kind::Sparse, seed: 3969696413, amp: 21018 },
    Case { name: "sparse-7ch-16b-209", channels: 7, depth: 16, frames: 209, kind: Kind::Sparse, seed: 789571232, amp: 158 },
    Case { name: "wave-7ch-20b-4161", channels: 7, depth: 20, frames: 4161, kind: Kind::Wave, seed: 3687026446, amp: 549 },
    Case { name: "wave-7ch-24b-4172", channels: 7, depth: 24, frames: 4172, kind: Kind::Wave, seed: 2553679619, amp: 1728284 },
    Case { name: "wave-7ch-32b-15", channels: 7, depth: 32, frames: 15, kind: Kind::Wave, seed: 3145988100, amp: 5682 },
    Case { name: "wave-7ch-32b-4266", channels: 7, depth: 32, frames: 4266, kind: Kind::Wave, seed: 3643360910, amp: 858882 },
    Case { name: "sparse-8ch-16b-218", channels: 8, depth: 16, frames: 218, kind: Kind::Sparse, seed: 151750901, amp: 210 },
    Case { name: "noise-8ch-16b-246", channels: 8, depth: 16, frames: 246, kind: Kind::Noise, seed: 1236272772, amp: 446 },
    Case { name: "sparse-8ch-16b-4126", channels: 8, depth: 16, frames: 4126, kind: Kind::Sparse, seed: 4106022001, amp: 208 },
    Case { name: "wave-8ch-16b-4187", channels: 8, depth: 16, frames: 4187, kind: Kind::Wave, seed: 3636422006, amp: 276 },
    Case { name: "correlated-8ch-16b-4330", channels: 8, depth: 16, frames: 4330, kind: Kind::Correlated, seed: 3718334856, amp: 12312 },
    Case { name: "sparse-8ch-20b-83", channels: 8, depth: 20, frames: 83, kind: Kind::Sparse, seed: 3557729870, amp: 420976 },
    Case { name: "noise-8ch-24b-18", channels: 8, depth: 24, frames: 18, kind: Kind::Noise, seed: 2154770779, amp: 4708 },
    Case { name: "noise-8ch-24b-8363", channels: 8, depth: 24, frames: 8363, kind: Kind::Noise, seed: 1979538413, amp: 5 },
    Case { name: "sparse-8ch-32b-4144", channels: 8, depth: 32, frames: 4144, kind: Kind::Sparse, seed: 4054183059, amp: 2191628 },
    // Controls the previous encoder already reproduced.
    Case { name: "correlated-1ch-24b-8237", channels: 1, depth: 24, frames: 8237, kind: Kind::Correlated, seed: 3060740427, amp: 399704 },
    Case { name: "sparse-1ch-16b-4197", channels: 1, depth: 16, frames: 4197, kind: Kind::Sparse, seed: 425841214, amp: 3077 },
    Case { name: "correlated-2ch-20b-699", channels: 2, depth: 20, frames: 699, kind: Kind::Correlated, seed: 1468809237, amp: 8809 },
    Case { name: "noise-6ch-16b-4775", channels: 6, depth: 16, frames: 4775, kind: Kind::Noise, seed: 3685714540, amp: 67 },
    Case { name: "correlated-8ch-32b-4165", channels: 8, depth: 32, frames: 4165, kind: Kind::Correlated, seed: 1801190703, amp: 1 },
    Case { name: "wave-3ch-20b-8337", channels: 3, depth: 20, frames: 8337, kind: Kind::Wave, seed: 1569302113, amp: 351831 },
    Case { name: "correlated-2ch-32b-8194", channels: 2, depth: 32, frames: 8194, kind: Kind::Correlated, seed: 569493361, amp: 22232 },
    Case { name: "wave-3ch-24b-8192", channels: 3, depth: 24, frames: 8192, kind: Kind::Wave, seed: 77, amp: 300000 },
    Case { name: "noise-2ch-16b-1", channels: 2, depth: 16, frames: 1, kind: Kind::Noise, seed: 5, amp: 20000 },
    Case { name: "noise-5ch-16b-7", channels: 5, depth: 16, frames: 7, kind: Kind::Noise, seed: 9, amp: 20000 },
    Case { name: "wave-2ch-16b-8", channels: 2, depth: 16, frames: 8, kind: Kind::Wave, seed: 11, amp: 20000 },
];

/// The 5-channel, 16-bit series shaped like the v0.3.0 repro: one full packet and a final
/// partial packet of 1 to 40 frames.
pub const SERIES_SIZES: std::ops::RangeInclusive<usize> = 1..=40;

pub fn series_case(size: usize) -> Case {
    Case { name: "series", channels: 5, depth: 16, frames: 4096 + size, kind: Kind::Sparse, seed: 99, amp: 64 }
}

/// Length-framed concatenation, so that equal digests mean equal packet boundaries too.
pub fn framed(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for packet in packets {
        out.extend_from_slice(&(packet.len() as u32).to_be_bytes());
        out.extend_from_slice(packet);
    }
    out
}

/// Interleaved float noise for the oversized-packet cases: uniform over the full range of a
/// 32-bit integer, scaled by `amp` (a level just above where the first encode attempt of a
/// 4096-frame packet exceeds AudioToolbox's output buffer), with `specials` samples replaced by
/// NaN, +Inf and -Inf in turn. The writer takes whatever the float path delivers.
#[derive(Clone, Copy, Debug)]
pub struct Overflow {
    pub name: &'static str,
    pub channels: u8,
    pub depth: u8,
    pub rate: u32,
    pub frames: usize,
    pub seed: u32,
    pub amp: f32,
    pub specials: u32,
}

pub fn overflow_floats(case: &Overflow) -> Vec<f32> {
    let mut state = case.seed | 1;
    let mut samples: Vec<f32> = (0..case.frames * usize::from(case.channels))
        .map(|_| ((next(&mut state) as i32) as f32 * (1.0 / 2147483648.0)) * case.amp).collect();
    for k in 0..case.specials {
        let index = next(&mut state) as usize % samples.len();
        samples[index] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY][(k % 3) as usize];
    }
    samples
}

/// The outcomes of AudioToolbox's writer for these, recorded on macOS with 512-frame write calls: a
/// file, or a refusal (the file decodes to fewer frames than were written, or has no packets).
pub const OVERFLOW: &[Overflow] = &[
    Overflow { name: "ovf-4ch-32b-4096-f1", channels: 4, depth: 32, rate: 44100, frames: 4096, seed: 1000, amp: 0.32958335, specials: 0 },
    Overflow { name: "ovf-4ch-32b-4109-f2nan", channels: 4, depth: 32, rate: 44100, frames: 4109, seed: 1002, amp: 0.51375, specials: 7 },
    Overflow { name: "ovf-4ch-32b-4096-limit", channels: 4, depth: 32, rate: 44100, frames: 4096, seed: 1000, amp: 0.49958333, specials: 0 },
    Overflow { name: "ovf-4ch-32b-5120-loss", channels: 4, depth: 32, rate: 44100, frames: 5120, seed: 1002, amp: 0.5194167, specials: 7 },
    Overflow { name: "ovf-4ch-32b-8205-multi", channels: 4, depth: 32, rate: 44100, frames: 8205, seed: 1003, amp: 0.49675, specials: 0 },
    Overflow { name: "ovf-4ch-32b-8205-ctl", channels: 4, depth: 32, rate: 44100, frames: 8205, seed: 1002, amp: 0.28141665, specials: 7 },
    Overflow { name: "ovf-5ch-32b-4096-f1", channels: 5, depth: 32, rate: 48000, frames: 4096, seed: 1000, amp: 0.34941667, specials: 0 },
    Overflow { name: "ovf-5ch-32b-4109-f2nan", channels: 5, depth: 32, rate: 48000, frames: 4109, seed: 1002, amp: 0.5165833, specials: 7 },
    Overflow { name: "ovf-5ch-32b-4096-limit", channels: 5, depth: 32, rate: 48000, frames: 4096, seed: 1000, amp: 0.48258334, specials: 0 },
    Overflow { name: "ovf-5ch-32b-5120-loss", channels: 5, depth: 32, rate: 48000, frames: 5120, seed: 1002, amp: 0.53641665, specials: 7 },
    Overflow { name: "ovf-5ch-32b-12301-multi", channels: 5, depth: 32, rate: 48000, frames: 12301, seed: 1004, amp: 0.5024167, specials: 0 },
    Overflow { name: "ovf-5ch-32b-8205-ctl", channels: 5, depth: 32, rate: 48000, frames: 8205, seed: 1002, amp: 0.28141665, specials: 7 },
    Overflow { name: "ovf-6ch-24b-4096-f1", channels: 6, depth: 24, rate: 96000, frames: 4096, seed: 1000, amp: 0.37491667, specials: 0 },
    Overflow { name: "ovf-6ch-24b-4109-f2nan", channels: 6, depth: 24, rate: 96000, frames: 4109, seed: 1002, amp: 0.48825002, specials: 7 },
    Overflow { name: "ovf-6ch-24b-4096-limit", channels: 6, depth: 24, rate: 96000, frames: 4096, seed: 1000, amp: 0.5194167, specials: 0 },
    Overflow { name: "ovf-6ch-24b-5120-loss", channels: 6, depth: 24, rate: 96000, frames: 5120, seed: 1002, amp: 0.5109167, specials: 7 },
    Overflow { name: "ovf-6ch-24b-8205-multi", channels: 6, depth: 24, rate: 96000, frames: 8205, seed: 1000, amp: 0.5024167, specials: 0 },
    Overflow { name: "ovf-6ch-24b-8205-ctl", channels: 6, depth: 24, rate: 96000, frames: 8205, seed: 1002, amp: 0.28141665, specials: 7 },
    Overflow { name: "ovf-8ch-16b-4096-f1", channels: 8, depth: 16, rate: 22050, frames: 4096, seed: 1000, amp: 0.36358333, specials: 0 },
    Overflow { name: "ovf-8ch-16b-4109-f2nan", channels: 8, depth: 16, rate: 22050, frames: 4109, seed: 1002, amp: 0.49108332, specials: 7 },
    Overflow { name: "ovf-8ch-16b-4096-limit", channels: 8, depth: 16, rate: 22050, frames: 4096, seed: 1000, amp: 0.5194167, specials: 0 },
    Overflow { name: "ovf-8ch-16b-5120-loss", channels: 8, depth: 16, rate: 22050, frames: 5120, seed: 1002, amp: 0.5165833, specials: 7 },
    Overflow { name: "ovf-8ch-16b-8192-multi", channels: 8, depth: 16, rate: 22050, frames: 8192, seed: 1003, amp: 0.50808334, specials: 0 },
    Overflow { name: "ovf-8ch-16b-8205-ctl", channels: 8, depth: 16, rate: 22050, frames: 8205, seed: 1002, amp: 0.28141665, specials: 7 },
    Overflow { name: "ovf-7ch-24b-4096-f1", channels: 7, depth: 24, rate: 88200, frames: 4096, seed: 1000, amp: 0.35508335, specials: 0 },
    Overflow { name: "ovf-7ch-24b-4109-f2nan", channels: 7, depth: 24, rate: 88200, frames: 4109, seed: 1002, amp: 0.47974998, specials: 7 },
    Overflow { name: "ovf-7ch-24b-4096-limit", channels: 7, depth: 24, rate: 88200, frames: 4096, seed: 1000, amp: 0.49675, specials: 0 },
    Overflow { name: "ovf-7ch-24b-5120-loss", channels: 7, depth: 24, rate: 88200, frames: 5120, seed: 1002, amp: 0.51375, specials: 7 },
    Overflow { name: "ovf-7ch-24b-8192-multi", channels: 7, depth: 24, rate: 88200, frames: 8192, seed: 1003, amp: 0.49675, specials: 0 },
    Overflow { name: "ovf-7ch-24b-8205-ctl", channels: 7, depth: 24, rate: 88200, frames: 8205, seed: 1002, amp: 0.28141665, specials: 7 },
    Overflow { name: "ovf-6ch-32b-4096-f1", channels: 6, depth: 32, rate: 44100, frames: 4096, seed: 1000, amp: 0.37491667, specials: 0 },
    Overflow { name: "ovf-6ch-32b-4109-f2nan", channels: 6, depth: 32, rate: 44100, frames: 4109, seed: 1002, amp: 0.48825002, specials: 7 },
    Overflow { name: "ovf-6ch-32b-4096-limit", channels: 6, depth: 32, rate: 44100, frames: 4096, seed: 1000, amp: 0.5194167, specials: 0 },
    Overflow { name: "ovf-6ch-32b-5120-loss", channels: 6, depth: 32, rate: 44100, frames: 5120, seed: 1002, amp: 0.5109167, specials: 7 },
    Overflow { name: "ovf-6ch-32b-8205-multi", channels: 6, depth: 32, rate: 44100, frames: 8205, seed: 1000, amp: 0.5024167, specials: 0 },
    Overflow { name: "ovf-6ch-32b-8205-ctl", channels: 6, depth: 32, rate: 44100, frames: 8205, seed: 1002, amp: 0.28141665, specials: 7 },
    Overflow { name: "ovf-8ch-24b-4096-f1", channels: 8, depth: 24, rate: 44100, frames: 4096, seed: 1000, amp: 0.36641666, specials: 0 },
    Overflow { name: "ovf-8ch-24b-4109-f2nan", channels: 8, depth: 24, rate: 44100, frames: 4109, seed: 1002, amp: 0.49108332, specials: 7 },
    Overflow { name: "ovf-8ch-24b-4096-limit", channels: 8, depth: 24, rate: 44100, frames: 4096, seed: 1000, amp: 0.5194167, specials: 0 },
    Overflow { name: "ovf-8ch-24b-5120-loss", channels: 8, depth: 24, rate: 44100, frames: 5120, seed: 1002, amp: 0.51375, specials: 7 },
    Overflow { name: "ovf-8ch-24b-8192-multi", channels: 8, depth: 24, rate: 44100, frames: 8192, seed: 1003, amp: 0.5024167, specials: 0 },
    Overflow { name: "ovf-8ch-24b-8205-ctl", channels: 8, depth: 24, rate: 44100, frames: 8205, seed: 1002, amp: 0.28141665, specials: 7 },
    Overflow { name: "ovf-8ch-32b-4096-f1", channels: 8, depth: 32, rate: 48000, frames: 4096, seed: 1000, amp: 0.36641666, specials: 0 },
    Overflow { name: "ovf-8ch-32b-4109-f2nan", channels: 8, depth: 32, rate: 48000, frames: 4109, seed: 1002, amp: 0.49108332, specials: 7 },
    Overflow { name: "ovf-8ch-32b-4096-limit", channels: 8, depth: 32, rate: 48000, frames: 4096, seed: 1000, amp: 0.5194167, specials: 0 },
    Overflow { name: "ovf-8ch-32b-5120-loss", channels: 8, depth: 32, rate: 48000, frames: 5120, seed: 1002, amp: 0.51375, specials: 7 },
    Overflow { name: "ovf-8ch-32b-8192-multi", channels: 8, depth: 32, rate: 48000, frames: 8192, seed: 1003, amp: 0.5024167, specials: 0 },
    Overflow { name: "ovf-8ch-32b-8205-ctl", channels: 8, depth: 32, rate: 48000, frames: 8205, seed: 1002, amp: 0.28141665, specials: 7 },
];

/// An MP4 with its creation and modification times zeroed: the clock is the only part of an AudioToolbox
/// ALAC file that differs between two runs on the same input.
pub fn mask_times(mut bytes: Vec<u8>) -> Vec<u8> {
    for tag in [b"mvhd", b"tkhd", b"mdhd"] {
        let mut i = 0;
        while i + 16 <= bytes.len() {
            if &bytes[i..i + 4] == tag { bytes[i + 8..i + 16].fill(0); i += 16; } else { i += 1; }
        }
    }
    bytes
}
