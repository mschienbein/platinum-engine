//! Full-band integrated loudness and the reference's simulated-gain search.
//!
//! This is the interleaved block path of Platinum Notes 10.5.0.1256. A meter
//! belongs to one pass: create a new instance for the next pass rather than
//! treating a reference reset as a fresh constructor. In particular, the
//! reference reset retains the limiter ring index. Instruction anchors and
//! block/scalar distinctions are recorded inline; public verification scope
//! is documented in `docs/VERIFICATION.md`.

use std::error::Error;
use std::fmt;

use crate::math;

const MAX_CHANNELS: usize = 64;
const MAX_FEED_FRAMES: usize = 1024;
const ABSOLUTE_GATE: f64 = f64::from_bits(0x3e7f_791e_c6e1_d5b7);
const LN_10: f64 = 2.302585092994046;
const POINT_GAINS: [f64; 9] = [3.0, 6.0, 9.0, 12.0, 15.0, 18.0, 21.0, 27.0, 50.0];
const REFINED_POINTS: usize = 145;

/// A configuration or interleaved-buffer error, or failure to allocate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoudnessError {
    InvalidSampleRate,
    InvalidChannelCount,
    IncompleteFrame,
    AllocationFailed,
}

impl fmt::Display for LoudnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => f.write_str("unsupported loudness sample rate"),
            Self::InvalidChannelCount => f.write_str("loudness channel count must be 1..=64"),
            Self::IncompleteFrame => f.write_str("interleaved samples contain an incomplete frame"),
            Self::AllocationFailed => f.write_str("unable to allocate loudness state"),
        }
    }
}

impl Error for LoudnessError {}

#[derive(Clone, Copy)]
struct Configuration {
    channels: usize,
    integer_rate: usize,
    hop: usize,
    capacity: usize,
}

impl Configuration {
    fn new(sample_rate: f64, channels: usize) -> Result<Self, LoudnessError> {
        if !(1..=MAX_CHANNELS).contains(&channels) {
            return Err(LoudnessError::InvalidChannelCount);
        }
        if !sample_rate.is_finite() || sample_rate < 0.0 {
            return Err(LoudnessError::InvalidSampleRate);
        }
        // The statistics constructor receives the unsigned-truncated rate;
        // the limiter coefficients use the original double rate instead.
        let integer_rate = sample_rate as usize;
        if !(16..=2_822_400).contains(&integer_rate) {
            return Err(LoudnessError::InvalidSampleRate);
        }
        let hop = (integer_rate + 5) / 10;
        let base = (400 * integer_rate) / 1000;
        let capacity = base.div_ceil(hop) * hop;
        // At 16 and 17 Hz the reference's allocated ring is shorter than
        // its initial four-hop write. Do not reproduce an out-of-bounds write.
        if capacity < 4 * hop {
            return Err(LoudnessError::InvalidSampleRate);
        }
        Ok(Self { channels, integer_rate, hop, capacity })
    }

    fn weight(&self, channel: usize) -> f64 {
        match self.channels {
            1..=3 => 1.0,
            4 => if channel < 2 { 1.0 } else { 1.41 },
            5 => if channel < 3 { 1.0 } else { 1.41 },
            _ => match channel {
                0..=2 => 1.0,
                4..=5 => 1.41,
                _ => 0.0,
            },
        }
    }
}

#[derive(Clone, Copy)]
struct Coefficients {
    a: [f64; 4],
    b: [f64; 5],
}

impl Coefficients {
    fn new(integer_rate: usize) -> Self {
        // These parentheses and repeated additions match 0x10004bf18, not
        // algebraically equivalent expressions from a generic K-filter.
        let fs = integer_rate as f64;
        let k = math::tan(5284.078578647628 / fs);
        let k2 = k * k;
        let kq = k / 0.7071752369554196;
        let d = k2 + (kq + 1.0);
        let kv = (k * 1.2587209302325617) / 0.7071752369554196;
        let vh = 1.5848647011308556;
        let s0 = (k2 + (kv + vh)) / d;
        let s1_base = k2 + -vh;
        let s1 = (s1_base + s1_base) / d;
        let s2 = (k2 + (vh - kv)) / d;
        let r1_base = k2 + -1.0;
        let r1 = (r1_base + r1_base) / d;
        let r2 = (k2 + (1.0 - kq)) / d;

        let h = math::tan(119.8061151453059 / fs);
        let h2 = h * h;
        let t1_base = h2 + -1.0;
        let hq = h / 0.5003270373238773;
        let e = h2 + (hq + 1.0);
        let t1 = (t1_base + t1_base) / e;
        let t2 = (h2 + (1.0 - hq)) / e;

        Self {
            b: [s0, s1 - (s0 + s0), s2 + (s0 - (s1 + s1)), s1 - (s2 + s2), s2],
            a: [r1 + t1, r2 + (t2 + r1 * t1), r1 * t2 + r2 * t1, r2 * t2],
        }
    }
}

#[derive(Default)]
struct GatedBlocks {
    energies: Vec<f64>,
}

impl GatedBlocks {
    fn record(&mut self, energy: f64) -> Result<(), LoudnessError> {
        // ARM's GE is false on an unordered comparison, so NaN is excluded.
        if energy >= ABSOLUTE_GATE {
            self.energies.try_reserve(1).map_err(|_| LoudnessError::AllocationFailed)?;
            self.energies.push(energy);
        }
        Ok(())
    }

    fn integrated(&self) -> f64 {
        if self.energies.is_empty() {
            return f64::NEG_INFINITY;
        }
        let mut absolute_sum = 0.0;
        for &energy in &self.energies {
            absolute_sum = energy + absolute_sum;
        }
        let relative_gate = (absolute_sum / self.energies.len() as f64) * 0.1;
        let mut count = 0usize;
        let mut sum = 0.0;
        for &energy in &self.energies {
            if energy >= relative_gate {
                count += 1;
                sum = sum + energy;
            }
        }
        if count == 0 {
            return f64::NEG_INFINITY;
        }
        let log = math::ln(sum / count as f64);
        (log / LN_10) * 10.0 + -0.691
    }
}

struct Statistics {
    config: Configuration,
    coefficients: Coefficients,
    state: [[f64; 4]; MAX_CHANNELS],
    ring: Vec<f64>,
    position: usize,
    until_block: usize,
    blocks: GatedBlocks,
}

impl Statistics {
    fn new(config: Configuration) -> Result<Self, LoudnessError> {
        let len = config.capacity.checked_mul(config.channels).ok_or(LoudnessError::AllocationFailed)?;
        let mut ring = Vec::new();
        ring.try_reserve_exact(len).map_err(|_| LoudnessError::AllocationFailed)?;
        ring.resize(len, 0.0);
        Ok(Self {
            config,
            coefficients: Coefficients::new(config.integer_rate),
            state: [[0.0; 4]; MAX_CHANNELS],
            ring,
            position: 0,
            until_block: 4 * config.hop,
            blocks: GatedBlocks::default(),
        })
    }

    fn feed(&mut self, samples: &[f32]) -> Result<(), LoudnessError> {
        let channels = self.config.channels;
        let mut offset = 0;
        let frames = samples.len() / channels;
        while offset < frames {
            let take = (frames - offset).min(self.until_block);
            self.filter(&samples[offset * channels..(offset + take) * channels], take);
            self.position += take;
            self.until_block -= take;
            offset += take;
            if self.until_block == 0 {
                let energy = self.block_energy();
                self.blocks.record(energy)?;
                self.until_block = self.config.hop;
                // The reference calculates the window before wrapping.
                if self.position == self.config.capacity {
                    self.position = 0;
                }
            }
        }
        Ok(())
    }

    fn filter(&mut self, samples: &[f32], frames: usize) {
        let channels = self.config.channels;
        let Coefficients { a, b } = self.coefficients;
        for channel in 0..channels {
            if self.config.weight(channel) == 0.0 {
                continue;
            }
            let [mut w1, mut w2, mut w3, mut w4] = self.state[channel];
            for frame in 0..frames {
                let mut w = samples[frame * channels + channel] as f64;
                w = w - a[0] * w1;
                w = w - a[1] * w2;
                w = w - a[2] * w3;
                w = w - a[3] * w4;
                let mut y = b[0] * w + b[1] * w1;
                y = y + b[2] * w2;
                y = y + b[3] * w3;
                y = y + b[4] * w4;
                self.ring[(self.position + frame) * channels + channel] = y;
                w4 = w3;
                w3 = w2;
                w2 = w1;
                w1 = w;
            }
            // FCMP/FCSEL MI retains NaNs, but flushes both signs of zero and
            // subnormal state. This happens at each filter-call boundary.
            for value in [&mut w1, &mut w2, &mut w3, &mut w4] {
                if value.abs() < f64::MIN_POSITIVE {
                    *value = 0.0;
                }
            }
            self.state[channel] = [w1, w2, w3, w4];
        }
    }

    fn block_energy(&self) -> f64 {
        let channels = self.config.channels;
        let window = 4 * self.config.hop;
        let mut sum = 0.0;
        for channel in 0..channels {
            let weight = self.config.weight(channel);
            if weight == 0.0 {
                continue;
            }
            let mut channel_sum = 0.0;
            // 0x10004c344 sums the beginning of a wrapped ring first, then
            // its tail. Chronological summation would round differently.
            if self.position < window {
                for frame in 0..self.position {
                    let y = self.ring[frame * channels + channel];
                    channel_sum = channel_sum + y * y;
                }
                let start = self.config.capacity - (window - self.position);
                for frame in start..self.config.capacity {
                    let y = self.ring[frame * channels + channel];
                    channel_sum = channel_sum + y * y;
                }
            } else {
                for frame in self.position - window..self.position {
                    let y = self.ring[frame * channels + channel];
                    channel_sum = channel_sum + y * y;
                }
            }
            if weight == 1.41 {
                channel_sum = channel_sum * 1.41;
            }
            sum = sum + channel_sum;
        }
        sum / window as f64
    }
}

/// Full-band K-weighted, gated integrated meter for one interleaved pass.
pub struct LoudnessMeter {
    config: Configuration,
    statistics: Option<Statistics>,
}

impl LoudnessMeter {
    /// Constructs the meter. Positive sample rates are truncated for the
    /// integrated filter, as in the reference. Rates truncating to 16 or 17
    /// are rejected because the reference's initial ring write exceeds its
    /// allocation at those rates.
    pub fn new(sample_rate: f64, channels: usize) -> Result<Self, LoudnessError> {
        Ok(Self { config: Configuration::new(sample_rate, channels)?, statistics: None })
    }

    /// Feeds complete interleaved frames. Empty feeds do not initialize the
    /// meter; partial frames are rejected without changing it.
    pub fn process_interleaved(&mut self, samples: &[f32]) -> Result<(), LoudnessError> {
        if samples.len() % self.config.channels != 0 {
            return Err(LoudnessError::IncompleteFrame);
        }
        for chunk in samples.chunks(MAX_FEED_FRAMES * self.config.channels) {
            self.process_chunk(chunk)?;
        }
        Ok(())
    }

    fn process_chunk(&mut self, samples: &[f32]) -> Result<(), LoudnessError> {
        if samples.is_empty() {
            return Ok(());
        }
        if self.statistics.is_none() {
            self.statistics = Some(Statistics::new(self.config)?);
        }
        self.statistics.as_mut().expect("statistics just initialized").feed(samples)
    }

    /// Returns -200 before the first nonempty feed, and negative infinity
    /// when initialized but no complete block passes the absolute gate.
    pub fn integrated_loudness(&self) -> f64 {
        match &self.statistics {
            None => -200.0,
            Some(stats) => stats.blocks.integrated(),
        }
    }
}

struct Simulation {
    meter: LoudnessMeter,
    gain_db: f64,
    linear_gain: f32,
    release: f64,
    attack: f64,
    delay: usize,
    peaks: Vec<f32>,
    index: usize,
    envelope: f32,
}

impl Simulation {
    fn new(sample_rate: f64, channels: usize, release_seconds: f64, gain_db: f64) -> Result<Self, LoudnessError> {
        let meter = LoudnessMeter::new(sample_rate, channels)?;
        let inverse = -1.0 / sample_rate;
        // FCSEL HI keeps NaN as well as positive release values.
        let effective_release = if release_seconds > 0.0 || release_seconds.is_nan() {
            release_seconds
        } else {
            0.023
        };
        let release = 1.0 - math::exp2(inverse / effective_release);
        let attack = 1.0 - math::exp2(inverse / 1e-9);
        // FCVTAS is nearest integer, ties away from zero, not truncation.
        let delay = (sample_rate * 0.002).round() as usize;
        let mut peaks = Vec::new();
        peaks.try_reserve_exact(delay + 8).map_err(|_| LoudnessError::AllocationFailed)?;
        peaks.resize(delay + 8, 0.0);
        Ok(Self {
            meter,
            gain_db,
            linear_gain: math::exp10(gain_db / 20.0) as f32,
            release,
            attack,
            delay,
            peaks,
            index: 0,
            envelope: 1.0,
        })
    }

    fn feed(&mut self, samples: &[f32], scratch: &mut [f32]) -> Result<(), LoudnessError> {
        let channels = self.meter.config.channels;
        for (input, output) in samples.chunks_exact(channels).zip(scratch.chunks_exact_mut(channels)) {
            for (source, scaled) in input.iter().zip(output.iter_mut()) {
                *scaled = *source * self.linear_gain;
            }
            let mut peak = output[0].abs();
            for &sample in &output[1..] {
                let candidate = sample.abs();
                // FCMP/FCSEL MI keeps the first operand on unordered input.
                if peak < candidate {
                    peak = candidate;
                }
            }
            if self.delay > 0 {
                let future = (self.index + self.delay) % self.peaks.len();
                self.peaks[future] = peak;
                let delayed = self.peaks[self.index];
                if peak < delayed {
                    peak = delayed;
                }
                self.index = (self.index + 1) % self.peaks.len();
            }
            // ARM FMAX, unlike f32::max, propagates a NaN operand.
            let at_least_one = if peak.is_nan() || peak > 1.0 { peak } else { 1.0 };
            let (target, coefficient) = if peak > self.envelope {
                (peak, self.attack)
            } else {
                (at_least_one, self.release)
            };
            let delta = target - self.envelope;
            self.envelope = (coefficient * delta as f64 + self.envelope as f64) as f32;
            for sample in output.iter_mut() {
                *sample = *sample / self.envelope;
            }
        }
        self.meter.process_chunk(scratch)
    }
}

/// Primary integrated meter and, when requested, nine limiter simulations.
/// Keep the healing-pass instance alive to query its target-dependent gain.
pub struct GainAnalyzer {
    primary: LoudnessMeter,
    simulations: Vec<Simulation>,
    scratch: Vec<f32>,
}

impl GainAnalyzer {
    /// `release_seconds` is the already-converted reference release value.
    /// A nonpositive release selects 0.023 seconds; NaN follows the original
    /// unordered comparison. `simulate = false` disables the simulation bank.
    pub fn new(sample_rate: f64, channels: usize, release_seconds: f64, simulate: bool) -> Result<Self, LoudnessError> {
        let primary = LoudnessMeter::new(sample_rate, channels)?;
        let mut simulations = Vec::new();
        let mut scratch = Vec::new();
        if simulate {
            simulations.try_reserve_exact(POINT_GAINS.len()).map_err(|_| LoudnessError::AllocationFailed)?;
            for gain in POINT_GAINS {
                simulations.push(Simulation::new(sample_rate, channels, release_seconds, gain)?);
            }
            let len = MAX_FEED_FRAMES * channels;
            scratch.try_reserve_exact(len).map_err(|_| LoudnessError::AllocationFailed)?;
            scratch.resize(len, 0.0);
        }
        Ok(Self { primary, simulations, scratch })
    }

    /// Feeds complete interleaved frames to the primary and every simulation.
    /// No sample scratch is allocated per block.
    pub fn process_interleaved(&mut self, samples: &[f32]) -> Result<(), LoudnessError> {
        let channels = self.primary.config.channels;
        if samples.len() % channels != 0 {
            return Err(LoudnessError::IncompleteFrame);
        }
        for chunk in samples.chunks(MAX_FEED_FRAMES * channels) {
            self.primary.process_chunk(chunk)?;
            for simulation in &mut self.simulations {
                simulation.feed(chunk, &mut self.scratch[..chunk.len()])?;
            }
        }
        Ok(())
    }

    pub fn integrated_loudness(&self) -> f64 {
        self.primary.integrated_loudness()
    }

    /// Returns the reference's target-dependent gain in dB, including its
    /// interpolation, repeated-addition searches and strict tie selection.
    pub fn gain_for_target(&self, target: f64) -> f64 {
        if self.simulations.is_empty() {
            return 0.0;
        }
        let baseline = self.integrated_loudness();
        // The reference branches into the search for unordered comparisons.
        if baseline > target {
            return target - baseline;
        }
        let mut points = [(0.0, 0.0); REFINED_POINTS];
        points[0] = (0.0, baseline);
        for (point, simulation) in points[1..10].iter_mut().zip(&self.simulations) {
            *point = (simulation.gain_db, simulation.meter.integrated_loudness());
        }
        search_gain(points, target)
    }
}

fn refine(points: &[(f64, f64); REFINED_POINTS], len: usize, output: &mut [(f64, f64); REFINED_POINTS]) -> usize {
    for i in 0..len {
        let (x0, y0) = points[i];
        output[2 * i] = (x0, y0);
        if i + 1 == len {
            break;
        }
        let (x1, y1) = points[i + 1];
        let (xp, yp) = if i == 0 {
            (x0 + (x0 - x1), y0 + (y0 - y1))
        } else {
            points[i - 1]
        };
        let dx = x1 - x0;
        let (xn, yn) = if i + 2 < len {
            points[i + 2]
        } else {
            (x1 + dx, y1 + (y1 - y0))
        };
        let previous = (y0 - yp) / (x0 - xp);
        let current = (y1 - y0) / dx;
        let next = (yn - y1) / (xn - x1);
        let m0 = previous + ((x0 - xp) * (current - previous)) / (x1 - xp);
        let m1 = current + (dx * (next - current)) / (xn - x0);
        let left_term = y0 + (dx * 0.5) * m0;
        let right_term = y1 + ((x0 - x1) * 0.5) * m1;
        let midpoint = (y0 + y1) * 0.5;
        let left = (midpoint + left_term) * 0.5;
        let right = (midpoint + right_term) * 0.5;
        let raw = (left + right) * 0.5;
        // MI then GT, deliberately not f64::clamp/f64::max for NaNs or
        // reversed endpoints.
        let lower = if raw < y0 { y0 } else { raw };
        let y = if lower > y1 { y1 } else { lower };
        output[2 * i + 1] = ((x0 + x1) * 0.5, y);
    }
    2 * len - 1
}

fn interpolate(points: &[(f64, f64); REFINED_POINTS], len: usize, x: f64) -> f64 {
    let mut i = 0;
    if x >= points[1].0 {
        i = len - 2;
        for j in 2..len {
            if points[j].0 > x {
                i = j - 1;
                break;
            }
        }
    }
    let (x0, y0) = points[i];
    let (x1, y1) = points[i + 1];
    y0 + ((x - x0) * (y1 - y0)) / (x1 - x0)
}

fn search_gain(mut points: [(f64, f64); REFINED_POINTS], target: f64) -> f64 {
    let mut other = [(0.0, 0.0); REFINED_POINTS];
    let mut len = 10;
    for _ in 0..4 {
        len = refine(&points, len, &mut other);
        std::mem::swap(&mut points, &mut other);
    }
    let mut best_score = -1_000_000_000.0;
    let mut best = 0.0;
    let mut x = 0.0;
    while x <= 50.0001 {
        let score = x * -0.0 - (interpolate(&points, len, x) - target).abs();
        if score > best_score {
            best_score = score;
            best = x;
        }
        x = x + 0.1;
    }
    let start = best - 0.2;
    let end = best + 0.2;
    x = start;
    while x <= end {
        let score = x * -0.0 - (interpolate(&points, len, x) - target).abs();
        if score > best_score {
            best_score = score;
            best = x;
        }
        x = x + 0.001;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_gate_is_inclusive_and_rejects_nan() {
        let mut blocks = GatedBlocks::default();
        blocks.record(f64::from_bits(ABSOLUTE_GATE.to_bits() - 1)).unwrap();
        blocks.record(ABSOLUTE_GATE).unwrap();
        blocks.record(f64::NAN).unwrap();
        assert_eq!(blocks.energies.len(), 1);
        assert_eq!(blocks.energies[0].to_bits(), ABSOLUTE_GATE.to_bits());
        let expected = (math::ln(ABSOLUTE_GATE) / LN_10) * 10.0 + -0.691;
        assert_eq!(blocks.integrated().to_bits(), expected.to_bits());
    }

    #[test]
    fn relative_gate_is_inclusive_and_uses_block_energies() {
        let inclusive = GatedBlocks { energies: vec![19.0, 1.0] };
        let expected = (math::ln(10.0) / LN_10) * 10.0 + -0.691;
        assert_eq!(inclusive.integrated().to_bits(), expected.to_bits());

        let below_one = f64::from_bits(1.0f64.to_bits() - 1);
        let exclusive = GatedBlocks { energies: vec![19.0, below_one] };
        let expected = (math::ln(19.0) / LN_10) * 10.0 + -0.691;
        assert_eq!(exclusive.integrated().to_bits(), expected.to_bits());
    }

    #[test]
    fn rounded_hop_and_five_hop_storage_select_only_latest_window() {
        // 1004 -> h=100, floor(400*fs/1000)=401, ring capacity=500.
        let config = Configuration::new(1004.0, 1).unwrap();
        assert_eq!(config.hop, 100);
        assert_eq!(config.capacity, 500);
        let mut stats = Statistics::new(config).unwrap();
        stats.ring.fill(2.0);
        stats.ring[100..200].fill(1000.0);
        stats.position = 100;
        // Wrapped window = [0,100) followed by [200,500); [100,200)
        // is the older fifth hop and must not contribute.
        assert_eq!(stats.block_energy().to_bits(), 4.0f64.to_bits());
    }

    #[test]
    fn simulator_rounds_delay_and_reads_peak_after_delay_frames() {
        assert_eq!(Simulation::new(249.0, 1, 0.023, 3.0).unwrap().delay, 0);
        let mut sim = Simulation::new(250.0, 1, 0.023, 3.0).unwrap();
        assert_eq!(sim.delay, 1);
        assert_eq!(sim.peaks.len(), 9);
        let mut scratch = [0.0; 3];
        sim.feed(&[2.0, 0.0, 0.0], &mut scratch).unwrap();
        assert_eq!(sim.index, 3);
        assert_eq!(sim.peaks[1], 2.0 * sim.linear_gain);
        // The second frame reads the peak written by the first. An
        // eight-frame interpretation would already release on frame two.
        let peak = 2.0 * sim.linear_gain;
        let first = (sim.attack * ((peak - 1.0) as f64) + 1.0) as f32;
        let second = (sim.release * ((peak - first) as f64) + first as f64) as f32;
        let third = (sim.release * ((1.0 - second) as f64) + second as f64) as f32;
        assert_eq!(sim.envelope.to_bits(), third.to_bits());
        assert_eq!(Simulation::new(750.0, 1, 0.023, 3.0).unwrap().delay, 2);
    }

    #[test]
    fn limiter_peak_ring_wraps_and_preserves_index_across_feeds() {
        let mut sim = Simulation::new(250.0, 1, 0.023, 3.0).unwrap();
        let mut scratch = [0.0; 7];
        sim.feed(&[0.0; 7], &mut scratch).unwrap();
        assert_eq!(sim.index, 7);
        sim.feed(&[0.0; 7], &mut scratch).unwrap();
        assert_eq!(sim.index, 5);
        assert!(sim.peaks.iter().all(|&peak| peak == 0.0));
    }

    #[test]
    fn interpolation_handles_reversed_points_and_nan_as_arm_selects() {
        let mut points = [(0.0, 0.0); REFINED_POINTS];
        points[0] = (0.0, 5.0);
        points[1] = (1.0, 3.0);
        let mut output = [(0.0, 0.0); REFINED_POINTS];
        assert_eq!(refine(&points, 2, &mut output), 3);
        assert_eq!(output[1], (0.5, 3.0));
        points[0].1 = f64::NAN;
        refine(&points, 2, &mut output);
        assert!(output[1].1.is_nan());
    }

    #[test]
    fn search_keeps_first_tie_and_refines_a_linear_curve() {
        let mut points = [(0.0, 0.0); REFINED_POINTS];
        for (point, &gain) in points[1..10].iter_mut().zip(&POINT_GAINS) {
            *point = (gain, 0.0);
        }
        assert_eq!(search_gain(points, 0.0).to_bits(), 0.0f64.to_bits());

        for point in &mut points[..10] {
            point.1 = point.0;
        }
        assert!((search_gain(points, 2.3456) - 2.346).abs() < 1e-10);
        assert_eq!(search_gain(points, f64::NAN).to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn filter_flushes_subnormal_state_but_preserves_nan() {
        let config = Configuration::new(48_000.0, 1).unwrap();
        let mut stats = Statistics::new(config).unwrap();
        stats.state[0] = [f64::from_bits(1), -f64::from_bits(1), f64::NAN, f64::MIN_POSITIVE];
        // A zero-frame filter call is useful here to isolate its ARM select.
        stats.filter(&[], 0);
        assert_eq!(stats.state[0][0].to_bits(), 0.0f64.to_bits());
        assert_eq!(stats.state[0][1].to_bits(), 0.0f64.to_bits());
        assert!(stats.state[0][2].is_nan());
        assert_eq!(stats.state[0][3].to_bits(), f64::MIN_POSITIVE.to_bits());
    }
}
