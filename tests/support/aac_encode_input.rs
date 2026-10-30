//! The deterministic input generator of the AAC encoder fixtures and the small MP4 access-unit reader, shared by
//! `tests/portable_aac_encode.rs` (through `tests/support/aac_encode.rs`) and the Apple recorder
//! `tools/portable/aac/encode_probe`. Both include this very file, so the float32 streams the recorder fed to
//! AudioToolbox are the ones the replay feeds to the portable writer.
//!
//! Determinism: the generator uses only integer arithmetic, `f32`/`f64` addition, subtraction, multiplication, division,
//! `sqrt` and `floor` (all correctly rounded by IEEE 754, so identical on every host; Rust never contracts into FMA),
//! casts, and a polynomial sine of its own. It calls no libm function and nothing of the engine. A stream depends only
//! on its [`Spec`], never on how it is cut into blocks. See `tests/fixtures/aac-encode/README.md` for each kind.
#![allow(dead_code)]

use platinum_engine::portable::digest::Sha256;
use serde_json::Value;
use std::ops::Range;

/// The engine's write size.
pub const BLOCK: usize = 512;
/// The creation/modification second (since 1904) every reference and every replay is normalised to.
pub const FIXED_CLOCK: u32 = 3_800_000_001;

pub fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }

// ---- integer PRNG ---------------------------------------------------------------------------------------------------

/// xorshift64* seeded through one splitmix64 round.
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self(if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z })
    }
    /// An independent stream of the same seed.
    pub fn stream(seed: u64, index: u64) -> Self { Self::new(seed ^ index.wrapping_add(1).wrapping_mul(0xD6E8_FEB8_6659_FD93)) }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [-1, 1) with 24 bits of resolution; every step is exact.
    pub fn uniform(&mut self) -> f32 { ((self.next_u64() >> 40) as f32) * (1.0 / 8_388_608.0) - 1.0 }
}

// ---- sine ------------------------------------------------------------------------------------------------------------

const TWO_PI: f64 = 6.283_185_307_179_586;

/// sin(2*pi*t) for any `t`, by quadrant reduction and a Taylor polynomial through x^15 (error < 1e-11), evaluated with
/// plain IEEE operations only.
pub fn sin_turns(t: f64) -> f64 {
    let mut t = t - t.floor();
    let mut sign = 1.0;
    if t >= 0.5 {
        t -= 0.5;
        sign = -1.0;
    }
    if t > 0.25 { t = 0.5 - t; }
    let x = t * TWO_PI;
    let x2 = x * x;
    let mut p = 1.0;
    for d in [14.0 * 15.0, 12.0 * 13.0, 10.0 * 11.0, 8.0 * 9.0, 6.0 * 7.0, 4.0 * 5.0, 2.0 * 3.0] { p = 1.0 - x2 / d * p; }
    sign * x * p
}

/// The phase, in turns, of a tone of `mhz` milli-hertz at frame `n`: integer until the final division.
fn turns(n: u64, mhz: i64, rate: u32) -> f64 {
    let modulus = u64::from(rate) * 1000;
    ((n * mhz as u64) % modulus) as f64 / modulus as f64
}

fn level(milli: i64) -> f32 { milli as f32 / 1000.0 }

// ---- the item specification ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub kind: String,
    pub rate: u32,
    pub channels: usize,
    pub frames: u64,
    pub seed: u64,
    /// Kind-specific integers (milli-units; see the README).
    pub params: [i64; 4],
}
impl Spec {
    pub fn from_json(item: &Value) -> Result<Spec, String> {
        let text = |k: &str| item[k].as_str().map(str::to_owned).ok_or_else(|| format!("missing {k}"));
        let int = |k: &str| item[k].as_u64().ok_or_else(|| format!("missing {k}"));
        let list = item["params"].as_array().ok_or("missing params")?;
        if list.len() != 4 { return Err("params must hold four integers".into()); }
        let mut params = [0i64; 4];
        for (slot, v) in params.iter_mut().zip(list) { *slot = v.as_i64().ok_or("params must be integers")?; }
        Ok(Spec { kind: text("kind")?, rate: int("rate")? as u32, channels: int("channels")? as usize, frames: int("frames")?, seed: int("seed")?, params })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Silence, Square, Nyquist, Dc, Tone, PerChannel, Sweep, White, BandNoise, Castanets, AmNoise, Corr, Uncorr, Clipped,
    OverRange, Subnormal, Special, RandBits, Burst, Lfe, Mix,
}
impl Kind {
    fn parse(name: &str) -> Option<Kind> {
        Some(match name {
            "silence" => Kind::Silence, "square" => Kind::Square, "nyquist" => Kind::Nyquist, "dc" => Kind::Dc, "tone" => Kind::Tone,
            "perchan" => Kind::PerChannel, "sweep" => Kind::Sweep, "white" => Kind::White, "bandnoise" => Kind::BandNoise,
            "castanets" => Kind::Castanets, "amnoise" => Kind::AmNoise, "corr" => Kind::Corr, "uncorr" => Kind::Uncorr,
            "clipped" => Kind::Clipped, "overrange" => Kind::OverRange, "subnormal" => Kind::Subnormal, "special" => Kind::Special,
            "randbits" => Kind::RandBits, "burst" => Kind::Burst, "lfe" => Kind::Lfe, "mix" => Kind::Mix,
            _ => return None,
        })
    }
}
pub const KINDS: [&str; 21] = [
    "silence", "square", "nyquist", "dc", "tone", "perchan", "sweep", "white", "bandnoise", "castanets", "amnoise", "corr", "uncorr",
    "clipped", "overrange", "subnormal", "special", "randbits", "burst", "lfe", "mix",
];

/// The non-finite values `special` can inject, selected by bit of `params[0]`.
const SPECIAL_VALUES: [(i64, u32); 8] = [
    (1, 0x7f80_0000), (2, 0xff80_0000), (4, 0x7fc0_0000), (8, 0xffc0_0000), (16, 0x7fc1_2345), (32, 0x7f80_0001), (64, 0xff80_0001), (128, 0xffbf_ffff),
];
/// `uncorr`: one-pole low-pass coefficient (1.0 = none) and level per channel.
const UNCORR_COEFF: [f32; 8] = [1.0, 0.30, 0.60, 0.15, 0.80, 0.45, 0.20, 0.90];
const UNCORR_LEVEL: [f32; 8] = [0.9, 0.30, 0.25, 0.35, 0.30, 0.30, 0.35, 0.30];
/// `mix`: partial ratios, amplitudes, envelope rates (mHz) and per-channel phase steps (turns).
const MIX_RATIO: [i64; 6] = [1, 2, 3, 5, 8, 13];
const MIX_AMP: [f32; 6] = [0.30, 0.22, 0.14, 0.10, 0.07, 0.05];
const MIX_ENV: [i64; 6] = [650, 1070, 1730, 2310, 3370, 4130];
const MIX_PHASE: [f64; 6] = [0.0, 0.11, 0.23, 0.31, 0.07, 0.19];

/// A stream of interleaved float32 frames described by a [`Spec`], produced block by block with bounded memory.
pub struct Source {
    spec: Spec,
    kind: Kind,
    n: u64,
    noise: Vec<Rng>,
    shared: Rng,
    special: Rng,
    /// One-pole filter states per channel (two stages and a high-pass tracker).
    filter: Vec<[f32; 3]>,
    phase: f64,
    env: f32,
}
impl Source {
    pub fn new(spec: &Spec) -> Result<Source, String> {
        let kind = Kind::parse(&spec.kind).ok_or_else(|| format!("unknown kind {}", spec.kind))?;
        if spec.channels == 0 || spec.channels > 8 { return Err("channels must be 1..=8".into()); }
        if spec.rate == 0 { return Err("rate".into()); }
        let noise = (0..spec.channels as u64).map(|c| Rng::stream(spec.seed, c)).collect();
        Ok(Source {
            spec: spec.clone(), kind, n: 0, noise, shared: Rng::stream(spec.seed, 100), special: Rng::stream(spec.seed, 101),
            filter: vec![[0.0; 3]; spec.channels], phase: 0.0, env: 0.0,
        })
    }
    pub fn spec(&self) -> &Spec { &self.spec }
    pub fn remaining(&self) -> u64 { self.spec.frames - self.n }

    /// Clear `out`, then append up to `max_frames` frames (fewer at the end of the stream); returns how many.
    pub fn next_block(&mut self, max_frames: usize, out: &mut Vec<f32>) -> usize {
        out.clear();
        let channels = self.spec.channels;
        let frames = (self.remaining().min(max_frames as u64)) as usize;
        out.resize(frames * channels, 0.0);
        for frame in out.chunks_exact_mut(channels) {
            self.frame(frame);
            self.n += 1;
        }
        frames
    }

    /// One-pole low-pass with the unit-variance gain, filter slot `slot` of channel `c`.
    fn lowpass(&mut self, c: usize, slot: usize, coefficient: f32, x: f32) -> f32 {
        let y = self.filter[c][slot] + coefficient * (x - self.filter[c][slot]);
        self.filter[c][slot] = y;
        y
    }
    /// White noise through a one-pole low-pass, scaled so the output has the variance of the input.
    fn coloured(&mut self, c: usize, coefficient: f32) -> f32 {
        let x = self.noise[c].uniform();
        if coefficient >= 1.0 { return x; }
        let gain = ((2.0 - f64::from(coefficient)) / f64::from(coefficient)).sqrt() as f32;
        self.lowpass(c, 0, coefficient, x) * gain
    }

    fn frame(&mut self, out: &mut [f32]) {
        let n = self.n;
        let rate = self.spec.rate;
        let p = self.spec.params;
        let channels = out.len();
        match self.kind {
            Kind::Silence => out.fill(0.0),
            Kind::Square => {
                let period = (p[0].max(2)) as u64;
                let v = level(p[1]);
                out.fill(if n % period < period / 2 { v } else { -v });
            }
            Kind::Nyquist => out.fill(if n % 2 == 0 { level(p[1]) } else { -level(p[1]) }),
            Kind::Dc => out.fill(level(p[1])),
            Kind::Tone => {
                let v = (sin_turns(turns(n, p[0], rate)) as f32) * level(p[1]);
                out.fill(v);
            }
            Kind::PerChannel => {
                for (c, slot) in out.iter_mut().enumerate() {
                    let phase = turns(n, p[0] + c as i64 * p[1], rate) + c as f64 * 0.125;
                    *slot = (sin_turns(phase) as f32) * level(p[2]) * (1.0 - c as f32 / 16.0);
                }
            }
            Kind::Sweep => {
                let (f0, f1) = (p[0] as f64, p[1] as f64);
                let f = f0 + (f1 - f0) * (n as f64) / (self.spec.frames.max(1) as f64);
                self.phase += f / f64::from(rate);
                self.phase -= self.phase.floor();
                for (c, slot) in out.iter_mut().enumerate() { *slot = (sin_turns(self.phase + c as f64 * 0.25) as f32) * level(p[2]); }
            }
            Kind::White => { for (c, slot) in out.iter_mut().enumerate() { *slot = self.noise[c].uniform() * level(p[0]); } }
            Kind::BandNoise => {
                let (lp, hp) = (level(p[0]), level(p[1]));
                for (c, slot) in out.iter_mut().enumerate() {
                    let x = self.noise[c].uniform();
                    let a = self.lowpass(c, 0, lp, x);
                    let mut y = self.lowpass(c, 1, lp, a);
                    if hp > 0.0 {
                        let tracked = self.lowpass(c, 2, hp, y);
                        y -= tracked;
                    }
                    *slot = y * level(p[3]) * level(p[2]);
                }
            }
            Kind::Castanets => {
                let period = p[0].max(1) as u64;
                if n % period == 0 { self.env = level(p[2]); }
                let e = self.env;
                for (c, slot) in out.iter_mut().enumerate() {
                    *slot = if p[3] == 1 { if n % period == 0 { e } else { 0.0 } } else { e * self.noise[c].uniform() };
                }
                let next = e * level(p[1]);
                self.env = if next < 1e-9 { 0.0 } else { next };
            }
            Kind::AmNoise => {
                let depth = level(p[1]);
                let s = sin_turns(turns(n, p[0], rate)) as f32;
                let am = (1.0 + depth * s) / (1.0 + depth);
                for (c, slot) in out.iter_mut().enumerate() { *slot = self.noise[c].uniform() * am * level(p[2]); }
            }
            Kind::Corr => {
                let rho = level(p[0]);
                let w_common = f64::from(rho.abs()).sqrt() as f32;
                let w_own = (1.0 - f64::from(rho.abs())).sqrt() as f32;
                let common = self.shared.uniform();
                for (c, slot) in out.iter_mut().enumerate() {
                    let sign = if rho < 0.0 && c % 2 == 1 { -1.0 } else { 1.0 };
                    let own = self.noise[c].uniform();
                    *slot = (sign * w_common * common + w_own * own) * level(p[1]);
                }
            }
            Kind::Uncorr => {
                for (c, slot) in out.iter_mut().enumerate() { *slot = self.coloured(c, UNCORR_COEFF[c]) * UNCORR_LEVEL[c] * level(p[0]); }
            }
            Kind::Clipped => {
                let s = (sin_turns(turns(n, p[0], rate)) as f32) * level(p[1]);
                out.fill(s.clamp(-1.0, 1.0));
            }
            Kind::OverRange => {
                let amplitude = p[0] as f32;
                if p[1] == 1 {
                    out.fill((sin_turns(turns(n, 1_000_000, rate)) as f32) * amplitude);
                } else {
                    for (c, slot) in out.iter_mut().enumerate() { *slot = self.noise[c].uniform() * amplitude; }
                }
            }
            Kind::Subnormal => {
                let position = n % 4096;
                for (c, slot) in out.iter_mut().enumerate() {
                    *slot = if position < 2048 || p[0] == 1 {
                        f32::from_bits((self.noise[c].next_u64() >> 32) as u32 & 0x807f_ffff)
                    } else if position < 3072 {
                        self.noise[c].uniform() * 1e-30
                    } else {
                        (sin_turns(turns(n, 1_000_000, rate)) as f32) * 1e-3
                    };
                }
            }
            Kind::Special => {
                for (c, slot) in out.iter_mut().enumerate() { *slot = self.noise[c].uniform() * 0.5; }
                let draw = self.special.next_u64();
                if draw % p[1].max(1) as u64 == 0 {
                    let chosen: Vec<u32> = SPECIAL_VALUES.iter().filter(|(bit, _)| p[0] & bit != 0).map(|(_, bits)| *bits).collect();
                    if !chosen.is_empty() {
                        out[((draw >> 20) % channels as u64) as usize] = f32::from_bits(chosen[((draw >> 40) % chosen.len() as u64) as usize]);
                    }
                }
            }
            Kind::RandBits => {
                for (c, slot) in out.iter_mut().enumerate() {
                    let mut bits = (self.noise[c].next_u64() >> 32) as u32;
                    if (bits >> 23) & 0xff == 0xff { bits &= !(1 << 23); }
                    *slot = f32::from_bits(bits);
                }
            }
            Kind::Burst => {
                let inside = n >= p[0] as u64 && n < p[1] as u64;
                for (c, slot) in out.iter_mut().enumerate() { *slot = if inside { self.noise[c].uniform() * level(p[2]) } else { 0.0 }; }
            }
            Kind::Lfe => {
                let lfe = p[0] as usize;
                let tone = sin_turns(turns(n, 50_000, rate)) as f32;
                for c in 0..channels {
                    out[c] = if c == lfe {
                        let low = self.coloured(c, 0.01);
                        (0.5 * tone + 0.15 * low) * level(p[1])
                    } else {
                        let band = self.coloured(c, 0.35);
                        let partial = sin_turns(turns(n, 300_000 + c as i64 * 110_000, rate)) as f32;
                        (0.3 * band + 0.1 * partial) * level(p[1])
                    };
                }
            }
            Kind::Mix => {
                let base = p[1];
                let mut env = [0.0f32; 6];
                for (k, slot) in env.iter_mut().enumerate() { *slot = 0.65 + 0.35 * (sin_turns(turns(n, MIX_ENV[k], rate)) as f32); }
                let position = n % 5513;
                let transient = if position < 120 { (120 - position) as f32 / 120.0 * 0.5 } else { 0.0 };
                for (c, slot) in out.iter_mut().enumerate() {
                    let mut sum = 0.0f32;
                    for k in 0..6 {
                        let phase = turns(n, base * MIX_RATIO[k], rate) + c as f64 * MIX_PHASE[k];
                        sum += MIX_AMP[k] * env[k] * (sin_turns(phase) as f32);
                    }
                    let noise = self.noise[c].uniform();
                    *slot = (sum + 0.01 * noise + transient * noise) * level(p[0]);
                }
            }
        }
    }
}

/// SHA-256 of the little-endian float32 byte stream of a whole item, produced block by block.
pub fn input_sha256(spec: &Spec) -> Result<String, String> {
    let mut source = Source::new(spec)?;
    let mut hash = Sha256::new();
    let mut block = Vec::new();
    let mut bytes = Vec::new();
    while source.next_block(4096, &mut block) > 0 {
        bytes.clear();
        for v in &block { bytes.extend_from_slice(&v.to_le_bytes()); }
        hash.update(&bytes);
    }
    Ok(hex(&hash.finalize()))
}

/// The whole stream in memory (short items only).
pub fn render(spec: &Spec) -> Result<Vec<f32>, String> {
    let mut source = Source::new(spec)?;
    let mut all = Vec::with_capacity(spec.frames as usize * spec.channels);
    let mut block = Vec::new();
    while source.next_block(4096, &mut block) > 0 { all.extend_from_slice(&block); }
    Ok(all)
}

// ---- MP4 reading ----------------------------------------------------------------------------------------------------------

fn be32(b: &[u8]) -> u32 { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) }
fn be64(b: &[u8]) -> u64 { u64::from_be_bytes(b[..8].try_into().unwrap()) }

/// The child boxes of `data` as `(type, payload range)`.
fn boxes(data: &[u8], start: usize, end: usize) -> Result<Vec<([u8; 4], Range<usize>)>, String> {
    let mut out = Vec::new();
    let mut at = start;
    while at < end {
        if end - at < 8 { return Err("truncated box header".into()); }
        let (size, header) = match be32(&data[at..]) {
            0 => (end - at, 8),
            1 => {
                if end - at < 16 { return Err("truncated large box header".into()); }
                (usize::try_from(be64(&data[at + 8..])).map_err(|_| "box too large")?, 16)
            }
            n => (n as usize, 8),
        };
        if size < header || size > end - at { return Err(format!("invalid box size {size} at {at}")); }
        out.push((data[at + 4..at + 8].try_into().unwrap(), at + header..at + size));
        at += size;
    }
    Ok(out)
}
fn find(list: &[([u8; 4], Range<usize>)], kind: &[u8; 4]) -> Result<Range<usize>, String> {
    list.iter().find(|(k, _)| k == kind).map(|(_, r)| r.clone()).ok_or_else(|| format!("no {} box", String::from_utf8_lossy(kind)))
}

/// Top-level box layout `(type, start, end)` of a file, for diagnostics.
pub fn layout(file: &[u8]) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while file.len() - at >= 8 {
        let size = match be32(&file[at..]) { 0 => file.len() - at, 1 if file.len() - at >= 16 => be64(&file[at + 8..]) as usize, n => n as usize };
        if size < 8 || size > file.len() - at { break; }
        out.push((String::from_utf8_lossy(&file[at + 4..at + 8]).into_owned(), at, at + size));
        at += size;
    }
    out
}

/// Where a byte offset lies: `ftyp+12`, `mdat+4096`, ...
pub fn locate(file: &[u8], offset: usize) -> String {
    for (kind, start, end) in layout(file) { if offset >= start && offset < end { return format!("{kind}+{}", offset - start); } }
    "outside the top-level boxes".into()
}

/// Byte ranges of every access unit of the first track, in decoding order.
pub fn access_units(file: &[u8]) -> Result<Vec<Range<usize>>, String> {
    let top = boxes(file, 0, file.len())?;
    let moov = find(&top, b"moov")?;
    let moov_children = boxes(file, moov.start, moov.end)?;
    let trak = find(&moov_children, b"trak")?;
    let mdia = find(&boxes(file, trak.start, trak.end)?, b"mdia")?;
    let minf = find(&boxes(file, mdia.start, mdia.end)?, b"minf")?;
    let stbl = find(&boxes(file, minf.start, minf.end)?, b"stbl")?;
    let tables = boxes(file, stbl.start, stbl.end)?;
    let stsz = find(&tables, b"stsz")?;
    let stsc = find(&tables, b"stsc")?;
    let chunk_offsets: Vec<u64> = if let Ok(r) = find(&tables, b"stco") {
        let count = be32(&file[r.start + 4..]) as usize;
        (0..count).map(|i| u64::from(be32(&file[r.start + 8 + 4 * i..]))).collect()
    } else if let Ok(r) = find(&tables, b"co64") {
        let count = be32(&file[r.start + 4..]) as usize;
        (0..count).map(|i| be64(&file[r.start + 8 + 8 * i..])).collect()
    } else {
        return Err("neither stco nor co64".into());
    };
    let (fixed, count) = (be32(&file[stsz.start + 4..]), be32(&file[stsz.start + 8..]) as usize);
    let size = |i: usize| if fixed != 0 { fixed } else { be32(&file[stsz.start + 12 + 4 * i..]) };
    let runs = be32(&file[stsc.start + 4..]) as usize;
    let runs: Vec<(usize, usize)> = (0..runs).map(|i| (be32(&file[stsc.start + 8 + 12 * i..]) as usize, be32(&file[stsc.start + 12 + 12 * i..]) as usize)).collect();
    let mut units = Vec::with_capacity(count);
    let mut sample = 0usize;
    for (chunk, offset) in chunk_offsets.iter().enumerate() {
        let per_chunk = runs.iter().rev().find(|(first, _)| *first <= chunk + 1).map(|(_, n)| *n).ok_or("stsc does not cover the chunk")?;
        let mut at = *offset as usize;
        for _ in 0..per_chunk {
            if sample == count { break; }
            let len = size(sample) as usize;
            if at + len > file.len() { return Err("access unit beyond the end of the file".into()); }
            units.push(at..at + len);
            at += len;
            sample += 1;
        }
    }
    if sample != count { return Err(format!("sample tables describe {sample} of {count} samples")); }
    Ok(units)
}

/// What a file's access units look like: the compact facts the manifest stores for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuSummary {
    pub count: usize,
    pub first_sha256: String,
    /// SHA-256 of the `.aus` serialisation: for each unit a little-endian u32 length followed by its bytes.
    pub stream_sha256: String,
    pub sizes: Vec<u32>,
    /// The first four bytes (eight hex digits) of each unit's SHA-256.
    pub sha8: Vec<String>,
}
pub fn summarize(file: &[u8]) -> Result<AuSummary, String> {
    let units = access_units(file)?;
    let mut stream = Sha256::new();
    let mut sizes = Vec::new();
    let mut sha8 = Vec::new();
    let mut first = String::new();
    for (i, unit) in units.iter().enumerate() {
        let bytes = &file[unit.clone()];
        stream.update(&(bytes.len() as u32).to_le_bytes());
        stream.update(bytes);
        sizes.push(bytes.len() as u32);
        let digest = platinum_engine::portable::digest::sha256(bytes);
        if i == 0 { first = hex(&digest); }
        sha8.push(hex(&digest[..4]));
    }
    Ok(AuSummary { count: units.len(), first_sha256: first, stream_sha256: hex(&stream.finalize()), sizes, sha8 })
}
