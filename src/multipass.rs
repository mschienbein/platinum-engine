//! Recovered Multipass controller, parameter mapping, and routing primitives.
//!
//! These primitives do not implement the full Multipass snapin processor or
//! dynamics engine.

use crate::math;
use crate::portable::arm_scalar::ArmFloat;

const LOG_200: f64 = f64::from_bits(0x4015_317a_1b94_9c53);

#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" {
    #[link_name = "exp"]
    fn system_exp(value: f64) -> f64;
    #[link_name = "sin"]
    fn system_sin(value: f64) -> f64;
    #[link_name = "expf"]
    fn system_expf(value: f32) -> f32;
    #[link_name = "logf"]
    fn system_logf(value: f32) -> f32;
    #[link_name = "__sincos_stret"]
    fn system_sincos(value: f64) -> SineCosine;
    #[link_name = "logb"]
    fn system_logb(value: f64) -> f64;
    #[link_name = "scalbn"]
    fn system_scalbn(value: f64, exponent: i32) -> f64;
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::math::{exp as system_exp, sin as system_sin, expf as system_expf,
    logf as system_logf, logb as system_logb, scalbn as system_scalbn};
#[cfg(any(target_os = "linux", feature = "portable"))]
fn system_sincos(value: f64) -> SineCosine {
    let pair = crate::portable::math::sincos(value);
    SineCosine { sine: pair.sine, cosine: pair.cosine }
}

// Darwin arm64 returns this homogeneous two-double aggregate in d0/d1.
#[repr(C)]
struct SineCosine {
    sine: f64,
    cosine: f64,
}

#[derive(Clone, Copy)]
enum CrossoverMode {
    Low,
    High,
    SummingAllpass,
}

fn crossover_coefficients(
    sample_rate: f32,
    frequency_hz: f32,
    mode: CrossoverMode,
) -> [f64; 5] {
    let rate = f64::from(sample_rate);
    let cutoff = (rate * f64::from_bits(0x3fdf_5c28_f5c2_8f5c)).arm_min(f64::from(frequency_hz));
    let angle = (cutoff * f64::from_bits(0x4019_21fb_5444_2d18)) / rate;
    // This libSystem primitive returns both values from one range reduction.
    let pair = unsafe { system_sincos(angle) };
    let q = f64::from_bits(0x3fe6_a09e_6000_0000);
    let alpha = pair.sine / (q + q);
    let (b0, b1, b2) = match mode {
        CrossoverMode::Low => {
            let edge = (-pair.cosine).arm_mul_add(0.5, 0.5);
            (edge, 1.0 - pair.cosine, edge)
        }
        CrossoverMode::High => {
            let edge = pair.cosine.arm_mul_add(0.5, 0.5);
            (edge, -(pair.cosine + 1.0), edge)
        }
        CrossoverMode::SummingAllpass => {
            (1.0 - alpha, pair.cosine * -2.0, alpha + 1.0)
        }
    };
    let denominator = alpha + 1.0;
    let middle = pair.cosine * -2.0;
    let reciprocal = 1.0 / denominator;
    [
        b0 * reciprocal,
        reciprocal * b1,
        reciprocal * b2,
        middle * reciprocal,
        (1.0 - alpha) * reciprocal,
    ]
}
/// Mode-7 allpass coefficients used by the Multipass summing path.
///
/// Inputs are the f32 sample rate and crossover selected by `0x9d4918`.
/// The caller must match HeartCore's scoped FPCR.FZ environment.
pub fn summing_allpass_coefficients(sample_rate: f32, frequency_hz: f32) -> [f64; 5] {
    crossover_coefficients(sample_rate, frequency_hz, CrossoverMode::SummingAllpass)
}


// Converts the five 0x65b600 biquad coefficients to the five parameters
// consumed by the state-variable loops at 0x9d79ec/0x9d7e0c/0x9d822c.
fn state_variable_coefficients(coefficients: [f64; 5]) -> [f64; 5] {
    let [b0, b1, b2, a1, a2] = coefficients;
    let sum = a2 + a1;
    let first = -1.0 - sum;
    let (real1, imaginary1) = if first > 0.0 {
        (first.sqrt(), 0.0)
    } else {
        (0.0, (-first).sqrt())
    };
    let second = (a1 - 1.0) - a2;
    let (real2, imaginary2) = if second > 0.0 {
        (second.sqrt(), 0.0)
    } else {
        (0.0, (-second).sqrt())
    };
    let product = real1.arm_mul_add(real2, -(imaginary1 * imaginary2));
    let exponent = (unsafe { system_logb(real2.arm_max(imaginary2)) } as i32).wrapping_neg();
    let scaled_real = unsafe { system_scalbn(real2, exponent) };
    let scaled_imaginary = unsafe { system_scalbn(imaginary2, exponent) };
    let magnitude = scaled_imaginary.arm_mul_add(scaled_imaginary, scaled_real * scaled_real);
    let numerator = imaginary1.arm_mul_add(scaled_imaginary, real1 * scaled_real);
    let g = unsafe { system_scalbn(numerator / magnitude, exponent) };
    let k = (a2 - 1.0) / product;
    let high = ((b0 - b1) + b2) / ((1.0 - a1) + a2);
    let band = ((b0 - b2) * -2.0) / product;
    let low = ((b1 + b0) + b2) / (sum + 1.0);
    [g, k, high, band, low]
}

#[derive(Clone, Copy)]
struct CrossoverSection {
    integrator_1: [f64; 2],
    integrator_2: [f64; 2],
    coefficients: [f64; 5],
    initial: bool,
}

impl CrossoverSection {
    const fn new() -> Self {
        Self {
            integrator_1: [0.0; 2],
            integrator_2: [0.0; 2],
            coefficients: [0.0; 5],
            initial: true,
        }
    }

    fn begin_chunk(&mut self, target: [f64; 5], count: usize) -> [f64; 5] {
        if self.initial {
            self.coefficients = target;
            self.initial = false;
            [0.0; 5]
        } else {
            let inverse_count = 1.0 / f64::from(count as i32);
            let mut increments = [0.0; 5];
            for index in 0..5 {
                increments[index] = (target[index] - self.coefficients[index]) * inverse_count;
            }
            increments
        }
    }

    fn frame(&mut self, input: [f32; 4]) -> [f32; 4] {
        let [g, k, high_gain, band_gain, low_gain] = self.coefficients;
        let k2_g = (k + k) + g;
        let scale = 1.0 / k2_g.arm_mul_add(g, 1.0);
        let scale_g = scale * g;
        let twice_g = g + g;
        let negative_twice_k = k * -2.0;
        let mut output = [0.0; 4];
        for channel in 0..2 {
            let x = f64::from(input[channel]);
            let s1 = self.integrator_1[channel];
            let s2 = self.integrator_2[channel];
            let temporary = x.arm_mul_add(g, s1);
            let mixed = temporary.arm_mul_add(k2_g, s2);
            let temporary = (-s2).arm_mul_add(g, temporary);
            let low = temporary.arm_mul_add(scale_g, s2);
            let difference = x - low;
            let high = (-mixed).arm_mul_add(scale, x);
            let band = high.arm_mul_add(g, s1);
            let feedback = band.arm_mul_add(negative_twice_k, difference);
            self.integrator_1[channel] = feedback.arm_mul_add(twice_g, s1);
            self.integrator_2[channel] = band.arm_mul_add(twice_g, s2);
            let value = low * low_gain;
            let value = high.arm_mul_add(high_gain, value);
            output[channel] = band.arm_mul_add(band_gain, value) as f32;
        }
        output
    }

    fn advance(&mut self, increments: [f64; 5]) {
        for index in 0..5 {
            self.coefficients[index] += increments[index];
        }
    }

    fn process_from(&mut self, target: [f64; 5], input: &[[f32; 4]], output: &mut [[f32; 4]]) {
        let increments = self.begin_chunk(target, input.len());
        for (source, destination) in input.iter().zip(output) {
            *destination = self.frame(*source);
            self.advance(increments);
        }
    }

    fn process_in_place(&mut self, target: [f64; 5], frames: &mut [[f32; 4]]) {
        let increments = self.begin_chunk(target, frames.len());
        for frame in frames {
            *frame = self.frame(*frame);
            self.advance(increments);
        }
    }
}

/// Root dry path's one allpass per compact crossover boundary (`0x9d4918`,
/// around `0x9d598c`/`0x9d5c30`). The channels share coefficients but retain
/// independent integrators. Its state must advance even when the current root
/// mix selects fully wet, because a later chunk can reuse the dry path.
pub(crate) struct RootDryAllpass {
    sample_rate: f32,
    sections: [CrossoverSection; 4],
    frames: [[f32; 4]; 64],
}

impl RootDryAllpass {
    pub(crate) fn new(sample_rate: f32) -> Self {
        Self { sample_rate, sections: [CrossoverSection::new(); 4], frames: [[0.0; 4]; 64] }
    }

    pub(crate) fn reset(&mut self) {
        self.sections = [CrossoverSection::new(); 4];
    }

    pub(crate) fn process(&mut self, frequencies: &[f32], left: &mut [f32], right: &mut [f32]) {
        assert_eq!(left.len(), right.len());
        assert!(left.len() <= 64 && frequencies.len() <= 4);
        for i in 0..left.len() {
            self.frames[i] = [left[i], right[i], 0.0, 0.0];
        }
        for (&frequency, section) in frequencies.iter().zip(self.sections.iter_mut()) {
            let coefficients = state_variable_coefficients(summing_allpass_coefficients(self.sample_rate, frequency));
            section.process_in_place(coefficients, &mut self.frames[..left.len()]);
        }
        for i in 0..left.len() {
            left[i] = self.frames[i][0];
            right[i] = self.frames[i][1];
        }
    }
}

/// One original Multipass crossover: two low sections, two high sections and
/// one summing allpass section for each of up to three earlier bands.
///
/// This does not establish the complete lane/snapin routing. Inputs and
/// outputs are the native 16-byte stereo frames; lanes 2 and 3 are written
/// as zero. Supply one positive event-bounded HeartCore DSP chunk and enter
/// the host's scoped FPCR.FZ environment before calling.
pub struct MultipassCrossover {
    sample_rate: f32,
    low: [CrossoverSection; 2],
    high: [CrossoverSection; 2],
    summing_allpass: [CrossoverSection; 3],
}

impl MultipassCrossover {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            low: [CrossoverSection::new(); 2],
            high: [CrossoverSection::new(); 2],
            summing_allpass: [CrossoverSection::new(); 3],
        }
    }

    /// The native prepare/reset path clears integrators and marks sections
    /// initial; stored coefficients are overwritten at the next invocation.
    pub fn reset(&mut self, sample_rate: f32) {
        *self = Self::new(sample_rate);
    }

    /// Split without aliasing the input and apply the summing allpasses to
    /// prior bands, in their original order. Caller-owned buffers are reused.
    pub fn process(
        &mut self,
        frequency_hz: f32,
        input: &[[f32; 4]],
        low: &mut [[f32; 4]],
        high: &mut [[f32; 4]],
        prior_bands: &mut [&mut [[f32; 4]]],
    ) {
        assert_eq!(input.len(), low.len());
        assert_eq!(input.len(), high.len());
        assert!(prior_bands.len() <= self.summing_allpass.len());
        assert!(prior_bands.iter().all(|band| band.len() == input.len()));

        let low_target = state_variable_coefficients(crossover_coefficients(
            self.sample_rate,
            frequency_hz,
            CrossoverMode::Low,
        ));
        self.low[0].process_from(low_target, input, low);
        self.low[1].process_in_place(low_target, low);
        let high_target = state_variable_coefficients(crossover_coefficients(
            self.sample_rate,
            frequency_hz,
            CrossoverMode::High,
        ));
        self.high[0].process_from(high_target, input, high);
        self.high[1].process_in_place(high_target, high);
        for (section, band) in self.summing_allpass.iter_mut().zip(prior_bands.iter_mut()) {
            let target = state_variable_coefficients(summing_allpass_coefficients(
                self.sample_rate,
                frequency_hz,
            ));
            section.process_in_place(target, band);
        }
    }
    /// The alias branch used after the first boundary: the low output is
    /// also the input, so the high path must run first.
    pub fn process_low_in_place(
        &mut self,
        frequency_hz: f32,
        low: &mut [[f32; 4]],
        high: &mut [[f32; 4]],
        prior_bands: &mut [&mut [[f32; 4]]],
    ) {
        assert_eq!(low.len(), high.len());
        assert!(prior_bands.len() <= self.summing_allpass.len());
        assert!(prior_bands.iter().all(|band| band.len() == low.len()));
        let high_target = state_variable_coefficients(crossover_coefficients(
            self.sample_rate,
            frequency_hz,
            CrossoverMode::High,
        ));
        self.high[0].process_from(high_target, low, high);
        self.high[1].process_in_place(high_target, high);
        let low_target = state_variable_coefficients(crossover_coefficients(
            self.sample_rate,
            frequency_hz,
            CrossoverMode::Low,
        ));
        self.low[0].process_in_place(low_target, low);
        self.low[1].process_in_place(low_target, low);
        for (section, band) in self.summing_allpass.iter_mut().zip(prior_bands.iter_mut()) {
            let target = state_variable_coefficients(summing_allpass_coefficients(
                self.sample_rate,
                frequency_hz,
            ));
            section.process_in_place(target, band);
        }
    }
}

/// The compact-band crossover chain at `0x9d6cf8`, excluding lane effects.
///
/// At most 64 stereo frames are processed per native host chunk. The caller
/// provides crossover frequencies in compact-band order and one nonalias
/// planar output pair per active band. Scratch and filter state are reused.
pub struct MultipassBandSplitter {
    crossovers: [MultipassCrossover; 4],
    packed_input: [[f32; 4]; 64],
    band_buffers: [[[f32; 4]; 64]; 5],
}

impl MultipassBandSplitter {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            crossovers: std::array::from_fn(|_| MultipassCrossover::new(sample_rate)),
            packed_input: [[0.0; 4]; 64],
            band_buffers: [[[0.0; 4]; 64]; 5],
        }
    }

    pub fn reset(&mut self, sample_rate: f32) {
        for crossover in &mut self.crossovers {
            crossover.reset(sample_rate);
        }
    }

    #[cfg(test)]
    pub(crate) fn first_low_section_state(&self) -> [u64; 9] {
        let section = &self.crossovers[0].low[0];
        [
            section.integrator_1[0].to_bits(), section.integrator_1[1].to_bits(),
            section.integrator_2[0].to_bits(), section.integrator_2[1].to_bits(),
            section.coefficients[0].to_bits(), section.coefficients[1].to_bits(),
            section.coefficients[2].to_bits(), section.coefficients[3].to_bits(),
            section.coefficients[4].to_bits(),
        ]
    }

    pub fn process(
        &mut self,
        input: [&[f32]; 2],
        crossover_frequencies: &[f32],
        output_bands: &mut [[&mut [f32]; 2]],
    ) {
        let [left, right] = input;
        let count = left.len();
        assert_eq!(right.len(), count);
        assert!(count <= 64);
        assert!((1..=5).contains(&output_bands.len()));
        assert_eq!(crossover_frequencies.len() + 1, output_bands.len());
        assert!(output_bands.iter().all(|band| band[0].len() == count && band[1].len() == count));
        if output_bands.len() == 1 {
            output_bands[0][0].copy_from_slice(left);
            output_bands[0][1].copy_from_slice(right);
            return;
        }

        for index in 0..count {
            self.packed_input[index] = [left[index], right[index], 0.0, 0.0];
        }
        {
            let (first, rest) = self.band_buffers.split_at_mut(1);
            self.crossovers[0].process(
                crossover_frequencies[0],
                &self.packed_input[..count],
                &mut first[0][..count],
                &mut rest[0][..count],
                &mut [],
            );
        }
        for boundary in 1..crossover_frequencies.len() {
            let (prior, current_and_later) = self.band_buffers.split_at_mut(boundary);
            let (current, later) = current_and_later.split_first_mut().unwrap();
            let mut prior_refs: [&mut [[f32; 4]]; 3] = [&mut [], &mut [], &mut []];
            for (reference, band) in prior_refs.iter_mut().zip(prior.iter_mut()) {
                *reference = &mut band[..count];
            }
            self.crossovers[boundary].process_low_in_place(
                crossover_frequencies[boundary],
                &mut current[..count],
                &mut later[0][..count],
                &mut prior_refs[..boundary],
            );
        }
        for (output, frames) in output_bands.iter_mut().zip(self.band_buffers.iter()) {
            let [out_left, out_right] = output;
            for index in 0..count {
                out_left[index] = frames[index][0];
                out_right[index] = frames[index][1];
            }
        }
    }
}

// Shared f32 interpolation state used by the original band router, dry/wet
// mixer, and left/right lane gain paths.
#[derive(Clone, Copy)]
struct ChunkRamp {
    counter: f32,
    step: f32,
    start: f32,
    target: f32,
    current: f32,
    initial: bool,
}

impl ChunkRamp {
    const fn new() -> Self {
        Self {
            counter: 0.0,
            step: 0.0,
            start: 0.0,
            target: 0.0,
            current: 0.0,
            initial: true,
        }
    }

    const fn lane_dry_wet() -> Self {
        Self {
            counter: 0.0,
            step: 0.0,
            start: 0.5,
            target: 0.5,
            current: 0.5,
            initial: false,
        }
    }

    fn prepare(&mut self, target: f32, count: usize) -> bool {
        let ramp = if self.initial {
            false
        } else {
            let epsilon = f64::from_bits(0x3cd2_03af_9ee7_5616);
            let difference = f64::from(target - self.target);
            !(difference > -epsilon && difference < epsilon)
        };
        self.counter = 0.0;
        if ramp {
            self.start = self.target;
            self.current = self.target;
            self.target = target;
            self.step = 1.0 / count as f32;
        } else {
            self.start = target;
            self.target = target;
            self.current = target;
            self.step = 0.0;
            self.initial = false;
        }
        ramp
    }

    fn advance(&mut self) -> f32 {
        let counter = self.counter;
        self.counter = counter + 1.0;
        let position = counter.arm_mul_add(self.step, self.step);
        self.current = position.arm_mul_add(self.target - self.start, self.start);
        self.current
    }
}

/// Shared stereo output gain immediately before the root dry/wet mixer in
/// `0x9d4918` (`0x9d5eac..0x9d5f38`). Both channels use the same ramp and
/// the first sample of a changed target uses the previous target.
pub(crate) struct RootStereoGain {
    ramp: ChunkRamp,
}

impl RootStereoGain {
    pub(crate) fn new() -> Self {
        Self { ramp: ChunkRamp::new() }
    }

    /// Root reset `0x9d38d0` marks the gain ramp initial, without needing to
    /// allocate or clear the previous target.
    pub(crate) fn reset(&mut self) {
        self.ramp.initial = true;
    }

    pub(crate) fn process(&mut self, gain: f32, left: &mut [f32], right: &mut [f32]) {
        assert_eq!(left.len(), right.len());
        let ramp = self.ramp.prepare(gain, left.len());
        let distance = (gain - 1.0).abs();
        if ramp || (!distance.is_nan() && distance > f32::from_bits(0x3580_0000)) {
            let mut value = self.ramp.current;
            for (left, right) in left.iter_mut().zip(right.iter_mut()) {
                *left *= value;
                *right *= value;
                value = self.ramp.advance();
            }
        }
    }
}

/// Native `0x659534` sets the silence byte after scanning both channels.
/// A sample on either threshold, infinity or NaN is non-silent.
pub(crate) fn channels_are_silent(left: &[f32], right: &[f32]) -> bool {
    assert_eq!(left.len(), right.len());
    let quiet = f32::from_bits(0x3580_0000);
    left.iter().zip(right).all(|(&l, &r)| {
        l < quiet && l > -quiet && r < quiet && r > -quiet
    })
}

/// Result of the common slot-15/16 silence dispatcher. The caller invokes the
/// effect-specific entry/wake hooks around the selected slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SleepDispatch {
    Active { woke: bool },
    Sleeping { entered: bool },
}

/// Common signed quiet-frame state at DSP `+0x58/+0x5c`. This is the
/// `0xc22118` / `0xc0ad80` dispatch, not an output-sample classifier.
pub(crate) struct NativeSleepGate {
    quiet_frames: i32,
    sleeping: bool,
}

impl NativeSleepGate {
    pub(crate) fn new() -> Self {
        Self { quiet_frames: i32::MAX, sleeping: false }
    }

    pub(crate) fn reset(&mut self) {
        self.quiet_frames = i32::MAX;
        self.sleeping = false;
    }

    /// Stable bypass sets the generic DSP sleeping flag after the output is
    /// classified silent; this flag is independent of the frame counter.
    pub(crate) fn mark_sleeping(&mut self) {
        self.sleeping = true;
    }

    /// Slot-9 output classification clears the counter independently of the
    /// sleeping flag (`0xc220e8`/`0xc0ad50` common wrapper).
    pub(crate) fn clear_quiet_frames(&mut self) {
        self.quiet_frames = 0;
    }

    pub(crate) fn is_sleeping(&self) -> bool {
        self.sleeping
    }

    pub(crate) fn select(&mut self, silent: bool, latency: i32, frames: usize) -> SleepDispatch {
        if !silent {
            let woke = self.sleeping;
            self.quiet_frames = 0;
            self.sleeping = false;
            return SleepDispatch::Active { woke };
        }
        if self.sleeping {
            return SleepDispatch::Sleeping { entered: false };
        }
        // Slot 10 returns 64 (`0x95eab0`); the native comparison is strictly
        // greater and is made before adding the current chunk length.
        if self.quiet_frames > latency.wrapping_add(64) {
            self.sleeping = true;
            return SleepDispatch::Sleeping { entered: true };
        }
        self.quiet_frames = self.quiet_frames.wrapping_add(frames as i32);
        SleepDispatch::Active { woke: false }
    }
}

/// Stateful two-destination band router at HeartCore `0x9d6f7c`.
///
/// `post` is the lane model's physical Post field, not its host-normalized
/// representation. Input and destination channels are planar and may not
/// alias. The two destinations must already contain their accumulations.
pub struct BandRouteMixer {
    ramp: ChunkRamp,
}

impl BandRouteMixer {
    pub fn new() -> Self {
        Self { ramp: ChunkRamp::new() }
    }

    pub fn process(
        &mut self,
        post: f32,
        input: [&[f32]; 2],
        first: [&mut [f32]; 2],
        second: [&mut [f32]; 2],
    ) {
        let [input_left, input_right] = input;
        let [first_left, first_right] = first;
        let [second_left, second_right] = second;
        let count = input_left.len();
        assert_eq!(input_right.len(), count);
        assert_eq!(first_left.len(), count);
        assert_eq!(first_right.len(), count);
        assert_eq!(second_left.len(), count);
        assert_eq!(second_right.len(), count);

        let ramp = self.ramp.prepare(post, count);

        if !ramp && (post.is_nan() || post < f32::from_bits(0x3580_0000)) {
            for index in 0..count {
                first_left[index] += input_left[index];
                first_right[index] += input_right[index];
            }
        } else if !ramp && post > f32::from_bits(0x3f7f_fff0) {
            for index in 0..count {
                second_left[index] += input_left[index];
                second_right[index] += input_right[index];
            }
        } else {
            let mut value = self.ramp.current;
            for index in 0..count {
                first_left[index] += (-value).arm_mul_add(input_left[index], input_left[index]);
                first_right[index] += (-value).arm_mul_add(input_right[index], input_right[index]);
                second_left[index] = input_left[index].arm_mul_add(value, second_left[index]);
                second_right[index] = input_right[index].arm_mul_add(value, second_right[index]);
                value = self.ramp.advance();
            }
        }
    }
}

impl Default for BandRouteMixer {
    fn default() -> Self {
        Self::new()
    }
}

/// The native stereo lane dry/wet interpolator at HeartCore `0x90d17c`.
///
/// The destination initially contains dry audio and is replaced in place;
/// `wet` must not alias it. Use the host's scoped FPCR.FZ environment and
/// original event-bounded DSP chunk boundaries.
pub struct LaneDryWetMixer {
    ramp: ChunkRamp,
}

impl LaneDryWetMixer {
    pub fn new() -> Self {
        Self { ramp: ChunkRamp::lane_dry_wet() }
    }

    /// Native silent-lane processing marks this ramp initial for its next use.
    pub fn mark_initial(&mut self) {
        self.ramp.initial = true;
    }

    pub fn process(&mut self, mix: f32, dry: [&mut [f32]; 2], wet: [&[f32]; 2]) {
        let [dry_left, dry_right] = dry;
        let [wet_left, wet_right] = wet;
        let count = dry_left.len();
        assert_eq!(dry_right.len(), count);
        assert_eq!(wet_left.len(), count);
        assert_eq!(wet_right.len(), count);

        let ramp = self.ramp.prepare(mix, count);
        if !ramp && mix > f32::from_bits(0x3f7f_fff0) {
            dry_left.copy_from_slice(wet_left);
            dry_right.copy_from_slice(wet_right);
        } else if ramp || (!mix.is_nan() && mix > f32::from_bits(0x3580_0000)) {
            let mut value = self.ramp.current;
            for index in 0..count {
                dry_left[index] = (wet_left[index] - dry_left[index]).arm_mul_add(value, dry_left[index]);
                dry_right[index] = (wet_right[index] - dry_right[index]).arm_mul_add(value, dry_right[index]);
                value = self.ramp.advance();
            }
        }
    }
}

impl Default for LaneDryWetMixer {
    fn default() -> Self {
        Self::new()
    }
}

/// Derive the two physical gains used by `0x9d9aac` after mute/solo gating.
/// The returned gains include the original f32 pan FMA, one libSystem
/// sin/cos evaluation and the original f64-to-f32 conversion boundaries.
pub fn lane_stereo_gains(
    gain: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    any_solo: bool,
) -> [f32; 2] {
    let gated = if any_solo {
        if solo { gain } else { 0.0 }
    } else if mute {
        0.0
    } else {
        gain
    };
    let fraction = pan.arm_mul_add(0.5, 0.5);
    let angle = f64::from(fraction) * f64::from_bits(0x3ff9_21fb_5444_2d18);
    let pair = unsafe { system_sincos(angle) };
    let root_two = f64::from_bits(0x3ff6_a09e_667f_3bcd);
    [
        gated * (pair.cosine * root_two) as f32,
        gated * (pair.sine * root_two) as f32,
    ]
}

#[derive(Clone, Copy)]
struct LaneGainSmoother {
    ramp: ChunkRamp,
}

impl LaneGainSmoother {
    const fn new() -> Self {
        Self { ramp: ChunkRamp::new() }
    }

    fn process(&mut self, target: f32, samples: &mut [f32]) {
        let ramp = self.ramp.prepare(target, samples.len());
        if samples.is_empty() {
            return;
        }
        if ramp || {
            let distance_from_unity = (target - 1.0).abs();
            !distance_from_unity.is_nan() && distance_from_unity > f32::from_bits(0x3580_0000)
        } {
            let mut value = self.ramp.current;
            for sample in samples {
                *sample *= value;
                value = self.ramp.advance();
            }
        }
    }
}

/// Physical lane gain/pan with separate left and right chunk-ramp state.
pub struct LanePanGain {
    channels: [LaneGainSmoother; 2],
}

impl LanePanGain {
    pub fn new() -> Self {
        Self { channels: [LaneGainSmoother::new(); 2] }
    }

    /// Native silent-lane processing marks both channel ramps initial.
    pub fn mark_initial(&mut self) {
        self.channels[0].ramp.initial = true;
        self.channels[1].ramp.initial = true;
    }

    /// Apply after lane dry/wet and both lane latency stages.
    pub fn process(
        &mut self,
        gain: f32,
        pan: f32,
        mute: bool,
        solo: bool,
        any_solo: bool,
        channels: [&mut [f32]; 2],
    ) {
        let [left, right] = channels;
        assert_eq!(left.len(), right.len());
        let [left_gain, right_gain] = lane_stereo_gains(gain, pan, mute, solo, any_solo);
        self.channels[0].process(left_gain, left);
        self.channels[1].process(right_gain, right);
    }
}

impl Default for LanePanGain {
    fn default() -> Self {
        Self::new()
    }
}

/// One native lane delay channel (`0x95eabc`) with caller-selected ring size.
///
/// The original prepare path allocates the ring before processing. Capacity
/// must leave room for two maximum-size chunks in addition to the requested
/// delay, matching the clamp at `0x9da384`.
pub struct LaneDelay {
    ring: Vec<f32>,
    max_chunk: usize,
    delay: usize,
    write_position: usize,
    valid_frames: usize,
    remaining_non_silent: usize,
}

impl LaneDelay {
    pub fn new(max_chunk: usize, capacity: usize) -> Self {
        assert!(capacity > 0 && capacity <= i32::MAX as usize);
        assert!(max_chunk.checked_mul(2).is_some_and(|size| size <= capacity));
        Self {
            ring: vec![0.0; capacity],
            max_chunk,
            delay: 0,
            write_position: 0,
            valid_frames: 0,
            remaining_non_silent: 0,
        }
    }

    /// The native lane configurer clamps to `[0, capacity-2*max_chunk]`.
    /// Changing to a positive delay resets the cursor and validity counters.
    pub fn set_delay(&mut self, requested: i32) {
        let maximum = self.ring.len() - 2 * self.max_chunk;
        let delay = (requested.max(0) as usize).min(maximum);
        if delay != self.delay {
            self.delay = delay;
            if delay > 0 {
                self.write_position = 0;
                self.valid_frames = 0;
                self.remaining_non_silent = 0;
            }
        }
    }

    /// Active clamped delay, used by the native lane's silence propagation.
    pub fn delay(&self) -> usize {
        self.delay
    }

    /// The lane dispatcher reads this counter *before* processing a chunk:
    /// a silent input remains silent only if no delayed non-silent frames
    /// were pending on entry (`0x9d9aac`, `0x9da2d0`).
    pub(crate) fn remaining_non_silent(&self) -> usize {
        self.remaining_non_silent
    }

    /// Reset validity and pending counts without moving the write cursor or
    /// changing the delay (`0x9d95bc` and `0x9d38d0`).
    pub(crate) fn reset_counters(&mut self) {
        self.valid_frames = 0;
        self.remaining_non_silent = 0;
    }

    /// Process one channel in place. `silent` is the host's input-silence
    /// flag, not a measurement of the sample values.
    pub fn process(&mut self, samples: &mut [f32], silent: bool) {
        let count = samples.len();
        assert!(count <= self.max_chunk);
        if self.delay == 0 || (silent && self.remaining_non_silent == 0) {
            self.valid_frames = 0;
            self.remaining_non_silent = 0;
            return;
        }

        let capacity = self.ring.len();
        let original_position = self.write_position;
        let mut offset = 0;
        while offset < count {
            let amount = (count - offset).min(capacity - self.write_position);
            self.ring[self.write_position..self.write_position + amount]
                .copy_from_slice(&samples[offset..offset + amount]);
            offset += amount;
            self.write_position += amount;
            if offset < count {
                self.write_position = 0;
            }
        }
        let mut read_position = (original_position + capacity - self.delay) % capacity;
        let zero_count = self.delay.saturating_sub(self.valid_frames).min(count);
        samples[..zero_count].fill(0.0);
        read_position = (read_position + zero_count) % capacity;
        offset = zero_count;
        while offset < count {
            let amount = (count - offset).min(capacity - read_position);
            samples[offset..offset + amount]
                .copy_from_slice(&self.ring[read_position..read_position + amount]);
            offset += amount;
            read_position = 0;
        }
        self.remaining_non_silent = if silent {
            self.remaining_non_silent.saturating_sub(count)
        } else {
            self.delay
        };
        if self.valid_frames <= 0x3fff_fffe {
            self.valid_frames += count;
        }
    }
}

impl Default for LaneDelay {
    fn default() -> Self {
        // 0x9d9410 loads {64, 10240} and allocates 0xa000 bytes per channel.
        Self::new(64, 10_240)
    }
}

/// Stereo lane delay pair and the original pre-call pending-counter silence
/// propagation at `0x9d9ae8..0x9d9b70`. The left and right channels receive
/// the same incoming silence byte; only the left pending count controls the
/// resulting byte, as in the native dispatcher.
pub(crate) struct LaneStereoDelay {
    left: LaneDelay,
    right: LaneDelay,
}

impl LaneStereoDelay {
    pub(crate) fn new() -> Self {
        Self { left: LaneDelay::default(), right: LaneDelay::default() }
    }

    pub(crate) fn configure(&mut self, frames: i32) {
        self.left.set_delay(frames);
        self.right.set_delay(frames);
    }

    pub(crate) fn reset_counters(&mut self) {
        self.left.reset_counters();
        self.right.reset_counters();
    }

    pub(crate) fn left_pending(&self) -> usize {
        self.left.remaining_non_silent()
    }

    pub(crate) fn process(&mut self, left: &mut [f32], right: &mut [f32], silent: &mut bool) {
        assert_eq!(left.len(), right.len());
        let input_silent = *silent;
        let left_was_empty = self.left.remaining_non_silent() == 0;
        self.left.process(left, input_silent);
        self.right.process(right, input_silent);
        *silent = input_silent && left_was_empty;
    }
}

/// Compact active Multipass band indices and the preceding crossover indices.
///
/// This is the layout calculation at HeartCore `0x9c6d4c`; it does not load a
/// preset or imply that the five input flags came from the wrapper flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveBands {
    pub band_count: usize,
    pub bands: [usize; 5],
    pub crossover_count: usize,
    pub crossovers: [usize; 4],
}

pub fn active_bands(enabled: [bool; 5]) -> ActiveBands {
    let mut layout = ActiveBands {
        band_count: 0,
        bands: [0; 5],
        crossover_count: 0,
        crossovers: [0; 4],
    };
    let mut previous = None;
    for (index, active) in enabled.into_iter().enumerate() {
        if !active {
            continue;
        }
        if let Some(crossover) = previous {
            layout.crossovers[layout.crossover_count] = crossover;
            layout.crossover_count += 1;
        }
        layout.bands[layout.band_count] = index;
        layout.band_count += 1;
        previous = Some(index);
    }
    layout
}

/// A descriptor coordinate in the Multipass editor model, not a proven
/// snapin or physical-control identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelParameter {
    pub node: u32,
    pub field: u32,
}

/// Map the native VST/AU parameter ID to its constructor-registered model
/// coordinate. The 179 tuples are the original arm64 table at `0x11d8ef4`,
/// compacted into their exact consecutive ranges. This does not resolve the
/// effect or modulation target of any node.
pub fn host_parameter_coordinate(id: u32) -> Option<ModelParameter> {
    let (node, field) = match id {
        0..=3 => (0, id + 1),
        4..=45 => (103 + (id - 4) / 6, 1 + (id - 4) % 6),
        46..=53 => (110 + id - 46, 0),
        54..=89 => (630 + (id - 54) / 9, (id - 54) % 9),
        90..=100 => (636 + id - 90, 0),
        101 => (102, 0),
        102 => (0, 7),
        103..=107 => (634, id - 103),
        108..=112 => (647 + id - 108, 0),
        113..=148 => (2797 + id - 113, 0),
        149 => (2861, 0),
        150 => (2861, 5),
        151..=178 => (2833 + id - 151, 0),
        _ => return None,
    };
    Some(ModelParameter { node, field })
}

/// Return the AutomationEditorModel slot whose Value is controlled by this
/// host ID. The slot's persisted ParameterPath supplies its eventual target.
pub fn automation_slot_for_host_parameter(id: u32) -> Option<usize> {
    let coordinate = host_parameter_coordinate(id)?;
    if coordinate.field == 0 && (2797..=2860).contains(&coordinate.node) {
        Some((coordinate.node - 2797) as usize)
    } else {
        None
    }
}


/// Raw lane controls before host descriptor replay and DSP event delivery.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShippedLane {
    pub enabled: bool,
    pub gain: f32,
    pub pan: f32,
    pub mix: f32,
    pub post: f32,
    pub mute: bool,
    pub solo: bool,
}

/// One of the four fixed presets shipped in Platinum Notes 10.5.0.1256.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShippedPreset {
    Dynamics,
    Gain,
    Warmth1,
    Warmth2,
}

/// Raw physical-domain controls, not the result of descriptor delivery.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShippedDynamicsParameters {
    pub enabled: bool,
    pub input_gain_db: f32,
    pub output_gain_db: f32,
    pub low_threshold_db: f32,
    pub low_ratio: f32,
    pub high_threshold_db: f32,
    pub high_ratio: f32,
    pub attack: f32,
    pub release: f32,
    pub knee: f32,
    pub mix: f32,
}

/// Raw physical-domain controls for the shipped Saturate-mode distortion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShippedDistortionParameters {
    pub enabled: bool,
    pub drive: f32,
    pub bias: f32,
    pub spread: f32,
    pub dynamics: f32,
    pub mix: f32,
    pub remove_dc: bool,
}

/// Raw physical-domain transient controls before descriptor conversion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShippedTransientParameters {
    pub enabled: bool,
    pub attack: f32,
    pub pump: f32,
    pub sustain: f32,
    pub speed: f32,
    pub clip: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShippedEffectParameters {
    Dynamics(ShippedDynamicsParameters),
    Distortion(ShippedDistortionParameters),
    TransientShaper(ShippedTransientParameters),
}

/// A fixed effect and its instance identifier for automation binding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShippedSnapin {
    pub instance: u16,
    pub parameters: ShippedEffectParameters,
}

/// Functional preset observations, before host writes or event delivery.
///
/// Float values retain the original raw bits; callers must replay descriptors
/// and preserve host event ordering instead of treating these as DSP values.
pub struct ShippedPresetSnapshot {
    /// Identity of the operator-supplied preset used for native comparisons.
    pub file_sha256: &'static str,
    pub crossover_hz: [f32; 4],
    pub speed: f32,
    pub lanes: [ShippedLane; 7],
    pub macro_value: f32,
    /// Initial source values for all 64 automation slots, before host writes.
    pub automation_values: [f32; 64],
    pub snapins: [&'static [ShippedSnapin]; 7],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShippedAutomationPath {
    pub instance_code: u16,
    pub event_id: u32,
}

impl ShippedPreset {
    pub fn snapshot(self) -> &'static ShippedPresetSnapshot {
        match self {
            Self::Dynamics => &DYNAMICS_SNAPSHOT,
            Self::Gain => &GAIN_SNAPSHOT,
            Self::Warmth1 => &WARMTH1_SNAPSHOT,
            Self::Warmth2 => &WARMTH2_SNAPSHOT,
        }
    }

    /// The observed one-code-unit path for an automation slot, if present.
    /// This does not assert a live registry binding or event delivery.
    pub fn automation_path(self, slot: usize) -> Option<ShippedAutomationPath> {
        if self != Self::Dynamics || slot >= 40 {
            return None;
        }
        const INSTANCES: [u16; 4] = [3, 1, 2, 4];
        const EVENT_IDS: [u32; 10] = [7, 4, 5, 422, 6, 0, 1, 2, 3, 423];
        Some(ShippedAutomationPath {
            instance_code: INSTANCES[slot % 4],
            event_id: EVENT_IDS[slot / 4],
        })
    }
}

const fn preset_lane(enabled: bool, gain: u32, mix: u32, post: u32) -> ShippedLane {
    ShippedLane {
        enabled,
        gain: f32::from_bits(gain),
        pan: 0.0,
        mix: f32::from_bits(mix),
        post: f32::from_bits(post),
        mute: false,
        solo: false,
    }
}

const fn preset_snapin(instance: u16, parameters: ShippedEffectParameters) -> ShippedSnapin {
    ShippedSnapin { instance, parameters }
}

const UNITY: u32 = 0x3f80_0000;
const DYNAMICS_PARAMETERS: ShippedDynamicsParameters = ShippedDynamicsParameters {
    enabled: true,
    input_gain_db: 0.0,
    output_gain_db: 0.0,
    low_threshold_db: -50.0,
    low_ratio: 1.0,
    high_threshold_db: 0.0,
    high_ratio: 1.0,
    attack: 1.0,
    release: 1.0,
    knee: 2.5,
    mix: 1.0,
};
const W1_DYNAMICS: ShippedDynamicsParameters = ShippedDynamicsParameters {
    low_threshold_db: f32::from_bits(0xc088_be30),
    low_ratio: f32::from_bits(0x3f70_aa8b),
    knee: f32::from_bits(0x4093_45d2),
    mix: f32::from_bits(0x3f1b_9f01),
    ..DYNAMICS_PARAMETERS
};
const W2_DYNAMICS: ShippedDynamicsParameters = ShippedDynamicsParameters {
    low_ratio: f32::from_bits(0x3f6a_710c),
    mix: 1.0,
    ..W1_DYNAMICS
};

const W1_DISTORTION: ShippedDistortionParameters = ShippedDistortionParameters {
    enabled: true,
    drive: f32::from_bits(0x3f40_d254),
    bias: 0.0,
    spread: 0.0,
    dynamics: f32::from_bits(0x3f46_2b52),
    mix: f32::from_bits(0x3e1e_b888),
    remove_dc: false,
};
const W2_DISTORTION: ShippedDistortionParameters = ShippedDistortionParameters {
    drive: f32::from_bits(0x3f5a_be70),
    dynamics: f32::from_bits(0x3f25_1eb3),
    mix: f32::from_bits(0x3ec3_b955),
    ..W1_DISTORTION
};

const W1_TRANSIENT_8: ShippedTransientParameters = ShippedTransientParameters {
    enabled: true,
    attack: f32::from_bits(0x3d27_9140),
    pump: f32::from_bits(0x3da6_5512),
    sustain: 0.0,
    speed: f32::from_bits(0x3f60_6818),
    clip: false,
};
const W1_TRANSIENT_3: ShippedTransientParameters = ShippedTransientParameters {
    attack: f32::from_bits(0x3c12_7800),
    pump: f32::from_bits(0x3dac_7692),
    speed: f32::from_bits(0x3f22_a921),
    ..W1_TRANSIENT_8
};
const W2_TRANSIENT_8: ShippedTransientParameters = ShippedTransientParameters {
    attack: f32::from_bits(0x3dba_2f00),
    pump: f32::from_bits(0x3ded_3451),
    speed: f32::from_bits(0x3f60_6a32),
    ..W1_TRANSIENT_8
};
const W2_TRANSIENT_3: ShippedTransientParameters = ShippedTransientParameters {
    attack: f32::from_bits(0x3d14_d440),
    pump: f32::from_bits(0x3e5d_40b6),
    speed: f32::from_bits(0x3f22_a6b8),
    ..W1_TRANSIENT_8
};
const W2_TRANSIENT_4: ShippedTransientParameters = ShippedTransientParameters {
    enabled: false, attack: 0.0, pump: 0.0, sustain: 0.0, speed: 1.0, clip: false,
};

// Slot source values before the wrapper's normalized host/event replay.
const DYNAMICS_AUTOMATION_VALUES: [f32; 64] = [
    0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0,
    0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.5, 0.0,
    0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    0.0, 0.0, 0.5, 0.5, 0.0, 0.0, f32::from_bits(0x3dcc_cccd), 0.0,
    0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
];

static DYN_L0: [ShippedSnapin; 1] = [preset_snapin(3, ShippedEffectParameters::Dynamics(DYNAMICS_PARAMETERS))];
static DYN_L1: [ShippedSnapin; 1] = [preset_snapin(1, ShippedEffectParameters::Dynamics(DYNAMICS_PARAMETERS))];
static DYN_L2: [ShippedSnapin; 1] = [preset_snapin(2, ShippedEffectParameters::Dynamics(DYNAMICS_PARAMETERS))];
static DYN_L3: [ShippedSnapin; 1] = [preset_snapin(4, ShippedEffectParameters::Dynamics(DYNAMICS_PARAMETERS))];
static W1_L0: [ShippedSnapin; 3] = [
    preset_snapin(2, ShippedEffectParameters::Distortion(W1_DISTORTION)),
    preset_snapin(8, ShippedEffectParameters::TransientShaper(W1_TRANSIENT_8)),
    preset_snapin(7, ShippedEffectParameters::Dynamics(W1_DYNAMICS)),
];
static W1_L1: [ShippedSnapin; 1] = [preset_snapin(3, ShippedEffectParameters::TransientShaper(W1_TRANSIENT_3))];
static W2_L0: [ShippedSnapin; 3] = [
    preset_snapin(2, ShippedEffectParameters::Distortion(W2_DISTORTION)),
    preset_snapin(8, ShippedEffectParameters::TransientShaper(W2_TRANSIENT_8)),
    preset_snapin(7, ShippedEffectParameters::Dynamics(W2_DYNAMICS)),
];
static W2_L1: [ShippedSnapin; 1] = [preset_snapin(3, ShippedEffectParameters::TransientShaper(W2_TRANSIENT_3))];
static W2_L2: [ShippedSnapin; 1] = [preset_snapin(4, ShippedEffectParameters::TransientShaper(W2_TRANSIENT_4))];

static DYNAMICS_SNAPSHOT: ShippedPresetSnapshot = ShippedPresetSnapshot {
    file_sha256: "837ce80bc5b94515f802351cc9cce1b3212a0bb995b4999226cf54aee612d464",
    crossover_hz: [
        f32::from_bits(0x4270_0001), f32::from_bits(0x4282_0e6f),
        f32::from_bits(0x428c_11ff), f32::from_bits(0x463b_8000),
    ],
    speed: 8.0,
    lanes: [
        preset_lane(true, UNITY, UNITY, UNITY),
        preset_lane(true, UNITY, UNITY, UNITY),
        preset_lane(true, UNITY, UNITY, UNITY),
        preset_lane(true, UNITY, UNITY, UNITY),
        preset_lane(false, UNITY, UNITY, UNITY),
        preset_lane(true, 0, UNITY, UNITY),
        preset_lane(true, 0, UNITY, UNITY),
    ],
    macro_value: 0.0,
    automation_values: DYNAMICS_AUTOMATION_VALUES,
    snapins: [&DYN_L0, &DYN_L1, &DYN_L2, &DYN_L3, &[], &[], &[]],
};

static GAIN_SNAPSHOT: ShippedPresetSnapshot = ShippedPresetSnapshot {
    file_sha256: "4eb841415b3cf6f1970fc7bd7e2fcd3b621f0f7ad495e684e91afe58b1a705db",
    crossover_hz: [
        f32::from_bits(0x42ef_21d6), f32::from_bits(0x43c8_1df2),
        f32::from_bits(0x44fa_2d67), f32::from_bits(0x459b_6cd4),
    ],
    speed: 8.0,
    lanes: [
        preset_lane(false, 0x3f89_541d, UNITY, UNITY),
        preset_lane(false, 0x3f84_431f, UNITY, UNITY),
        preset_lane(false, 0x3f80_0001, 0, UNITY),
        preset_lane(false, 0x3f80_0001, UNITY, UNITY),
        preset_lane(false, 0x3f80_0001, UNITY, UNITY),
        preset_lane(true, 0x3f4b_5917, 0, UNITY),
        preset_lane(true, UNITY, 0x3b23_d70a, UNITY),
    ],
    macro_value: f32::from_bits(0x3ca3_d70a),
    automation_values: [0.0; 64],
    snapins: [&[], &[], &[], &[], &[], &[], &[]],
};

static WARMTH1_SNAPSHOT: ShippedPresetSnapshot = ShippedPresetSnapshot {
    file_sha256: "f7e5ceb7e3cd0ca58e4933d6e23be239c47d221b2a63856751d51171f209bbee",
    crossover_hz: [
        f32::from_bits(0x42ef_21d6), f32::from_bits(0x43ae_6aaa),
        f32::from_bits(0x44fa_2d67), f32::from_bits(0x459b_6cd4),
    ],
    speed: 8.0,
    lanes: [
        preset_lane(true, 0x3f6e_e9c2, UNITY, 0x3f7f_ff84),
        preset_lane(true, 0x3f7b_09cf, UNITY, UNITY),
        preset_lane(true, 0x3f80_00d9, 0x35ac_7690, UNITY),
        preset_lane(false, 0x3f80_0001, UNITY, UNITY),
        preset_lane(false, 0x3f80_0001, UNITY, UNITY),
        preset_lane(true, 0x3f80_0001, 0, UNITY),
        preset_lane(true, 0x3f80_0001, UNITY, UNITY),
    ],
    macro_value: f32::from_bits(0x3ca3_d70a),
    automation_values: [0.0; 64],
    snapins: [&W1_L0, &W1_L1, &[], &[], &[], &[], &[]],
};

static WARMTH2_SNAPSHOT: ShippedPresetSnapshot = ShippedPresetSnapshot {
    file_sha256: "fc81e123d8178b679feb9b400ed97e4d39dbe01a533bc1257f985c06c5e1f039",
    crossover_hz: [
        f32::from_bits(0x42ef_21d6), f32::from_bits(0x43ae_6aaa),
        f32::from_bits(0x44fa_2d67), f32::from_bits(0x459b_6cd4),
    ],
    speed: 8.0,
    lanes: [
        preset_lane(true, 0x3f54_ee78, UNITY, UNITY),
        preset_lane(true, 0x3f82_fb43, UNITY, UNITY),
        preset_lane(true, 0x3f80_0001, 0, UNITY),
        preset_lane(false, 0x3f80_0001, UNITY, UNITY),
        preset_lane(false, 0x3f80_0001, UNITY, UNITY),
        preset_lane(true, 0x3f80_0001, 0, UNITY),
        preset_lane(true, 0x3f7d_a727, 0x3f7f_ff6f, UNITY),
    ],
    macro_value: 0.0,
    automation_values: [0.0; 64],
    snapins: [&W2_L0, &W2_L1, &W2_L2, &[], &[], &[], &[]],
};


/// The f32 values returned by the template's band getters.
#[derive(Clone, Copy)]
pub struct DynamicsBand {
    pub threshold_db: f32,
    pub ratio: f32,
    pub gain_db: f32,
}

/// One normalized Audio Unit parameter write, in original controller order.
#[derive(Clone, Copy, Default)]
pub struct ParameterWrite {
    pub id: u32,
    pub value: f32,
}

// FMAX(x, +0) followed by FMINNM(x, 1), as in the controller. FMAX
// propagates NaN, then FMINNM selects the numeric operand.
fn controller_clamp(value: f64) -> f64 {
    if value.is_nan() || value >= 1.0 {
        1.0
    } else if value <= 0.0 {
        0.0
    } else {
        value
    }
}

fn normalized_ratio(ratio: f64) -> f64 {
    const LOG_30: f64 = f64::from_bits(0x400b_35a6_f90b_d69b);
    // FCMP/B.PL takes the upper branch for an unordered ratio, whose final
    // FMINNM returns 1.0 after log(NaN).
    if ratio.is_nan() {
        return 1.0;
    }
    if ratio >= 1.0 {
        if ratio == 1.0 {
            0.5
        } else if ratio >= 30.0 {
            1.0
        } else {
            let value = controller_clamp(math::ln(ratio) / LOG_30);
            value * 0.5 + 0.5
        }
    } else {
        let inverse = 1.0 / ratio;
        let value = if inverse <= 1.0 {
            0.0
        } else if inverse >= 30.0 {
            0.5
        } else {
            controller_clamp(math::ln(inverse) / LOG_30) * 0.5
        };
        0.5 - value
    }
}

fn normalized_crossover(frequency: f64) -> f64 {
    if frequency <= 60.0 {
        0.0
    } else if frequency >= 12_000.0 {
        1.0
    } else {
        // The original subtracts log(60) *after* log(frequency); replacing
        // this with log(frequency / 60) changes rounding.
        let log60 = f64::from_bits(0xc010_609b_dc65_328b);
        controller_clamp((math::ln(frequency) + log60) / LOG_200)
    }
}

fn normalized_main_gain(db: f64) -> f64 {
    if db >= 10.0 {
        return 1.0;
    }
    if db < -200.0 {
        return 0.0;
    }
    let linear = math::exp10(db / 20.0) * 4.8;
    let curve = ((db + -10.1256) / 36.7989) + 1.0;
    let exponent = (db + 23.4702) * -0.276768;
    // The original calls libSystem exp (0x1000115e8).
    let blend = 1.0 / (unsafe { system_exp(exponent) } + 1.00013);
    let value = curve * blend + (1.0 - blend) * linear;
    controller_clamp(value)
}

// The lane Gain descriptor's physical range and shifted logarithmic endpoints.
const LANE_GAIN_MAX: f32 = f32::from_bits(0x404a_62c2);
const LANE_GAIN_LOG_MIN: f32 = f32::from_bits(0xc03f_ba14);
const LANE_GAIN_LOG_MAX: f32 = f32::from_bits(0x3f95_5f9c);
const LANE_GAIN_SHIFT: f32 = f32::from_bits(0x3d4c_cccd);

fn clamp_lane_gain(value: f32) -> f32 {
    // FMAXNM(+0,value) then FMINNM(result,max): a NaN selects +0.
    if value.is_nan() || value <= 0.0 {
        0.0
    } else if value >= LANE_GAIN_MAX {
        LANE_GAIN_MAX
    } else {
        value
    }
}

/// Shared root Split 1..4 logarithmic range object constructed at `0xa1965c`.
/// The original calls libSystem `logf` on both endpoints during initialization.
pub struct CrossoverFrequencyScale {
    log_min: f32,
    log_max: f32,
}

impl CrossoverFrequencyScale {
    pub fn new() -> Self {
        Self {
            log_min: unsafe { system_logf(60.0) },
            log_max: unsafe { system_logf(12_000.0) },
        }
    }

    /// Convert the host's f32 normalized value to the physical split in Hz.
    pub fn from_normalized(&self, normalized: f32) -> f32 {
        let exponent = (self.log_max - self.log_min).arm_mul_add(normalized, self.log_min);
        let frequency = unsafe { system_expf(exponent) } - 0.0;
        clamp_crossover_frequency(frequency)
    }

    /// Convert a physical f32 Hz value back to a host-normalized f32.
    pub fn to_normalized(&self, frequency_hz: f32) -> f32 {
        let frequency = clamp_crossover_frequency(frequency_hz);
        let logarithm = unsafe { system_logf(frequency + 0.0) };
        (logarithm - self.log_min) / (self.log_max - self.log_min)
    }
}

impl Default for CrossoverFrequencyScale {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp_crossover_frequency(frequency_hz: f32) -> f32 {
    // Native FMAXNM(min, value), then FMINNM(result, max).
    if frequency_hz.is_nan() || frequency_hz <= 60.0 {
        60.0
    } else if frequency_hz >= 12_000.0 {
        12_000.0
    } else {
        frequency_hz
    }
}

/// Lane Gain descriptor's normalized-to-physical mapping (`0x963ad8` /
/// `0x96405c`). The explicit f32 FMA matches the original ARM64 instruction.
/// This does not deliver an editor or DSP event.
pub fn lane_gain_from_normalized(normalized: f32) -> f32 {
    let span = LANE_GAIN_LOG_MAX - LANE_GAIN_LOG_MIN;
    let exponent = span.arm_mul_add(normalized, LANE_GAIN_LOG_MIN);
    let physical = unsafe { system_expf(exponent) } - LANE_GAIN_SHIFT;
    clamp_lane_gain(physical)
}

/// Lane Gain descriptor's physical-to-normalized mapping (`0x963ab4` /
/// `0x964028`), including the native pre-log physical clamp.
pub fn lane_gain_to_normalized(physical: f32) -> f32 {
    let physical = clamp_lane_gain(physical);
    let logarithm = unsafe { system_logf(physical + LANE_GAIN_SHIFT) };
    (logarithm - LANE_GAIN_LOG_MIN) / (LANE_GAIN_LOG_MAX - LANE_GAIN_LOG_MIN)
}

/// The native macro DSP (`0x662a60`, `0x662a1c`) forwards its current model
/// value, optionally transforming a bipolar or inverted output mode.
pub(crate) fn native_macro_source(value: f32, mode: i32) -> f32 {
    match mode {
        1 => (value + value) + -1.0,
        2 => 1.0 - value,
        _ => value,
    }
}

/// One enabled, zero-curve ordinary route in `0x6230d0`. Caller supplies the
/// already-interpolated source and the independent most-recent source; the
/// interpolation/history policy and event scheduling are not implicit here.
/// Native `0x6233a4..0x6233ac` rounds the depth product before both FMADDs.
pub(crate) fn native_linear_modulation(
    previous: [f32; 2],
    source: [f32; 2],
    depth: f32,
    secondary_depth: f32,
) -> [f32; 2] {
    let amount = secondary_depth * depth;
    [
        amount.arm_mul_add(source[0], previous[0]),
        amount.arm_mul_add(source[1], previous[1]),
    ]
}

/// Floating-point lane fields, in the model layout at `0x9e44f8`.
/// Enabled, mute, solo and latency are integer fields, not this mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneModelField {
    Gain,
    Pan,
    Mix,
    Post,
}

impl LaneModelField {
    /// Apply `0x6243f0`'s raw physical event and caller-supplied normalized
    /// modulation. Does not determine when either value is delivered.
    pub fn model_event_value(self, raw_physical: f32, normalized_modulation: f32) -> f32 {
        if self == Self::Gain {
            let normalized = lane_gain_to_normalized(raw_physical);
            let target = (normalized + normalized_modulation).arm_max(0.0).arm_min(1.0);
            return lane_gain_from_normalized(target);
        }
        let low = if self == Self::Pan { -1.0_f32 } else { 0.0_f32 };
        let physical = raw_physical.arm_max(low).arm_min(1.0);
        let normalized = (physical - low) / (1.0 - low);
        let target = (normalized + normalized_modulation).arm_max(0.0).arm_min(1.0);
        (1.0 - low).arm_mul_add(target, low).arm_max(low).arm_min(1.0)
    }
}

/// The four floating-point TransientShaper DSP-model fields whose descriptors
/// are installed by `0xc0d8e8..0xc0d984`. Clip is an integer field and is not
/// represented by this float conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransientModelField {
    Attack,
    Pump,
    Sustain,
    Speed,
}

impl TransientModelField {
    /// Apply `0x6243f0`'s unmodulated/raw-event conversion and the supplied
    /// additive normalized modulation. This models the descriptor only; the
    /// caller must determine event order and the actual per-chunk modulation.
    pub fn model_event_value(self, raw_physical: f32, normalized_modulation: f32) -> f32 {
        let (low, high) = match self {
            Self::Attack | Self::Sustain => (-1.0_f32, 1.0_f32),
            Self::Pump => (0.0_f32, 1.0_f32),
            Self::Speed => (f32::from_bits(0x3e4c_cccd), 5.0_f32),
        };
        let physical = raw_physical.arm_max(low).arm_min(high);
        let normalized = if self == Self::Speed {
            let log_min = f32::from_bits(0xbfce_0210);
            let log_max = f32::from_bits(0x3fce_0210);
            (math::logf(physical + 0.0) - log_min) / (log_max - log_min)
        } else {
            (physical - low) / (high - low)
        };
        let value = (normalized + normalized_modulation).arm_max(0.0).arm_min(1.0);
        let mapped = if self == Self::Speed {
            let log_min = f32::from_bits(0xbfce_0210);
            let log_max = f32::from_bits(0x3fce_0210);
            math::expf((log_max - log_min).arm_mul_add(value, log_min)) - 0.0
        } else {
            (high - low).arm_mul_add(value, low)
        };
        mapped.arm_max(low).arm_min(high)
    }
}

/// Writes made after loading `Gain.dat`. The input is the native f32 gain
/// argument, promoted to f64 before the original normalization helper.
pub fn gain_stage_parameter_writes(gain_db: f32) -> [ParameterWrite; 2] {
    [
        ParameterWrite { id: 34, value: normalized_main_gain(0.0) as f32 },
        ParameterWrite { id: 40, value: normalized_main_gain(f64::from(gain_db)) as f32 },
    ]
}

/// Optional writes made after loading the selected warmth preset.
///
/// A missing entry means the preset value is left untouched, not zeroed.
/// The caller must still apply the original stage gate for negative modes.
pub fn warmth_parameter_writes(input_gain_db: f64, mode: i32) -> [Option<ParameterWrite>; 2] {
    [
        (input_gain_db != 0.0).then(|| ParameterWrite {
            id: 34,
            value: normalized_main_gain(input_gain_db) as f32,
        }),
        (mode == 1).then(|| ParameterWrite {
            id: 40,
            value: normalized_main_gain(f64::from_bits(0x3fc7_c504_816f_0069)) as f32,
        }),
    ]
}

fn band_gain_correction(ratio: f64, threshold: f64, band: usize) -> f64 {
    const LOW: [f64; 4] = [-20.8, -24.7, -29.5, -39.4];
    const HIGH: [f64; 4] = [-8.3, -11.9, -14.0, -16.5];
    let low = LOW[band];
    let high = HIGH[band];
    if high <= threshold {
        return 0.0;
    }
    let slope = ratio - 1.0;
    if low >= threshold {
        let left = (slope * 0.5) * (low + high);
        let right = slope * threshold;
        left - right
    } else {
        let factor = slope * -0.5;
        let distance = threshold - high;
        let angle = (distance * std::f64::consts::PI) / (low - high);
        // The native controller calls libSystem sin (0x100011a14).
        let sine = unsafe { system_sin(angle) } * 0.25;
        factor * (distance + (high - low) * sine)
    }
}

/// Recover the 41 native writes made by the dynamics setup path.
///
/// `reciprocal_ratio` corresponds to the controller's expansion flag: its
/// reciprocal is computed in f32 before promotion. `correct_band_gain`
/// corresponds to the separate flag passed by the calling heal path; it
/// applies the controller's weighted gain correction, not a DSP knee. Both
/// master arguments are the original helper's f64 inputs. Attack and release
/// are intentionally not accepted: this observed setup path does not read
/// those template fields. These writes do not establish snapin ownership.
pub fn dynamics_parameter_writes(
    bands: [DynamicsBand; 4],
    crossovers_hz: [f32; 3],
    reciprocal_ratio: bool,
    correct_band_gain: bool,
    master_34_db: f64,
    master_40_db: f64,
) -> [ParameterWrite; 41] {
    let mut writes = [ParameterWrite::default(); 41];
    let mut cursor = 0;
    let weighted = ((f64::from(bands[0].gain_db) * 0.2
        + f64::from(bands[1].gain_db) * 0.25)
        + f64::from(bands[2].gain_db) * 0.35)
        + f64::from(bands[3].gain_db) * 0.2;
    for (band, controls) in bands.into_iter().enumerate() {
        let ratio = if reciprocal_ratio {
            f64::from(1.0_f32 / controls.ratio)
        } else {
            f64::from(controls.ratio)
        };
        let threshold = f64::from(controls.threshold_db);
        let mut gain = f64::from(controls.gain_db);
        if correct_band_gain {
            gain = (band_gain_correction(ratio, threshold, band) + gain) - weighted;
        }
        let threshold_value = controller_clamp((threshold + 50.0) / 50.0);
        let ratio_value = controller_clamp(
            (normalized_ratio(ratio) + -0.39810247645474695) / 0.601897523545253,
        ).arm_max(1e-8);
        let offset_gain = if gain < -24.0 { gain + 24.0 } else { 0.0 };
        let input_gain = controller_clamp((offset_gain + 24.0) / 48.0).arm_max(1e-8);
        let output_gain = controller_clamp((gain + 24.0) / 48.0).arm_max(1e-8);
        for (base, value) in [
            (117, threshold_value),
            (121, ratio_value),
            (125, 0.5),
            (129, 0.5),
            (133, input_gain),
            (137, output_gain),
            (141, 0.0),
            (145, 0.5),
            (151, 0.1),
        ] {
            writes[cursor] = ParameterWrite { id: base + band as u32, value: value as f32 };
            cursor += 1;
        }
    }
    // The original queries and writes crossovers in descending band order.
    for band in (0..3).rev() {
        writes[cursor] = ParameterWrite {
            id: band as u32,
            value: normalized_crossover(f64::from(crossovers_hz[band])) as f32,
        };
        cursor += 1;
    }
    writes[cursor] = ParameterWrite { id: 34, value: normalized_main_gain(master_34_db) as f32 };
    writes[cursor + 1] = ParameterWrite { id: 40, value: normalized_main_gain(master_40_db) as f32 };
    writes
}

#[cfg(test)]
mod tests {
    use super::{
        BandRouteMixer, CrossoverFrequencyScale, DynamicsBand, LaneDelay, LaneDryWetMixer,
        LanePanGain, ModelParameter,
        MultipassBandSplitter, MultipassCrossover,
        ShippedEffectParameters, ShippedPreset, active_bands, automation_slot_for_host_parameter,
        dynamics_parameter_writes,
        gain_stage_parameter_writes,
        host_parameter_coordinate, lane_gain_from_normalized, lane_gain_to_normalized, lane_stereo_gains,
        normalized_crossover, normalized_ratio, summing_allpass_coefficients, warmth_parameter_writes,
    };

    #[test]
    fn root_output_gain_uses_one_shared_previous_target_ramp() {
        let mut gain = super::RootStereoGain::new();
        let (mut left, mut right) = ([1.0; 4], [2.0; 4]);
        gain.process(1.0, &mut left, &mut right);
        gain.process(0.0, &mut left, &mut right);
        assert_eq!(left, [1.0, 0.75, 0.5, 0.25]);
        assert_eq!(right, [2.0, 1.5, 1.0, 0.5]);
        gain.reset();
        gain.process(1.0, &mut left, &mut right);
        assert_eq!(left, [1.0, 0.75, 0.5, 0.25]);
    }

    #[test]
    fn silence_classifier_is_strict_and_delay_reset_keeps_cursor() {
        let epsilon = f32::from_bits(0x3580_0000);
        assert!(super::channels_are_silent(&[0.0, epsilon * 0.5], &[-0.0, 0.0]));
        assert!(!super::channels_are_silent(&[epsilon], &[0.0]));
        assert!(!super::channels_are_silent(&[0.0], &[f32::NAN]));
        let mut delay = LaneDelay::new(4, 16);
        delay.set_delay(3);
        delay.process(&mut [1.0], false);
        assert_eq!(delay.remaining_non_silent(), 3);
        delay.process(&mut [0.0], true);
        assert_eq!(delay.remaining_non_silent(), 2);
        let position = delay.write_position;
        delay.reset_counters();
        assert_eq!(delay.remaining_non_silent(), 0);
        assert_eq!(delay.write_position, position);
        assert_eq!(delay.delay(), 3);
    }

    #[test]
    fn stereo_lane_delay_silence_uses_pre_chunk_pending_state() {
        let mut delay = super::LaneStereoDelay::new();
        delay.configure(2);
        let mut silence = false;
        let (mut left, mut right) = ([1.0, 2.0], [3.0, 4.0]);
        delay.process(&mut left, &mut right, &mut silence);
        assert_eq!(left, [0.0, 0.0]);
        assert_eq!(right, [0.0, 0.0]);
        let (mut left, mut right) = ([0.0, 0.0], [0.0, 0.0]);
        silence = true;
        delay.process(&mut left, &mut right, &mut silence);
        assert_eq!(left, [1.0, 2.0]);
        assert_eq!(right, [3.0, 4.0]);
        assert!(!silence);
        silence = true;
        delay.process(&mut left, &mut right, &mut silence);
        assert!(silence);
        delay.reset_counters();
    }

    #[test]
    fn sleep_dispatch_uses_strict_pre_chunk_threshold_and_explicit_wake() {
        use super::{NativeSleepGate, SleepDispatch};
        let mut gate = NativeSleepGate::new();
        assert_eq!(gate.select(true, 0, 64), SleepDispatch::Sleeping { entered: true });
        assert_eq!(gate.select(true, 0, 64), SleepDispatch::Sleeping { entered: false });
        assert_eq!(gate.select(false, 0, 64), SleepDispatch::Active { woke: true });
        assert_eq!(gate.select(true, 0, 64), SleepDispatch::Active { woke: false });
        assert_eq!(gate.select(true, 0, 64), SleepDispatch::Active { woke: false });
        assert_eq!(gate.select(true, 0, 64), SleepDispatch::Sleeping { entered: true });
        gate.reset();
        gate.mark_sleeping();
        assert_eq!(gate.select(false, 0, 1), SleepDispatch::Active { woke: true });
    }

    #[test]
    fn lane_and_transient_float_domains_apply_normalized_modulation() {
        use super::{LaneModelField, TransientModelField};
        assert_eq!(LaneModelField::Mix.model_event_value(0.0, 0.25), 0.25);
        assert_eq!(LaneModelField::Post.model_event_value(0.5, 1.0), 1.0);
        assert_eq!(LaneModelField::Pan.model_event_value(-1.0, 0.5), 0.0);
        assert_eq!(TransientModelField::Attack.model_event_value(-1.0, 0.5), 0.0);
        assert_eq!(TransientModelField::Pump.model_event_value(0.0, 0.25), 0.25);
        assert_eq!(TransientModelField::Sustain.model_event_value(1.0, -1.0), -1.0);
    }

    #[test]
    fn macro_route_matches_first_observed_source_and_amount() {
        let source = super::native_macro_source(f32::from_bits(0x3ca3_d70a), 0);
        let value = super::native_linear_modulation(
            [0.0; 2], [source; 2], f32::from_bits(0x3f7f_15f2), 1.0,
        );
        assert_eq!(value.map(f32::to_bits), [0x3ca3_413e; 2]);
        assert_eq!(super::native_macro_source(0.25, 1), -0.5);
        assert_eq!(super::native_macro_source(0.25, 2), 0.75);
    }

    #[test]
    fn shipped_automation_bindings_target_the_effect_in_each_lane() {
        let preset = ShippedPreset::Dynamics;
        let snapshot = preset.snapshot();
        for slot in 0..40 {
            let path = preset.automation_path(slot).unwrap();
            let snapin = &snapshot.snapins[slot % 4][0];
            assert_eq!(snapin.instance, path.instance_code);
            assert!(matches!(snapin.parameters, ShippedEffectParameters::Dynamics(_)));
        }
        assert!(preset.automation_path(40).is_none());
        for other in [ShippedPreset::Gain, ShippedPreset::Warmth1, ShippedPreset::Warmth2] {
            assert!(other.automation_path(0).is_none());
        }
    }

    #[test]
    fn crossover_host_range_uses_physical_clamps_before_inverse_conversion() {
        let scale = CrossoverFrequencyScale::new();
        assert_eq!(scale.from_normalized(f32::NEG_INFINITY), 60.0);
        assert_eq!(scale.from_normalized(f32::INFINITY), 12_000.0);
        assert_eq!(scale.from_normalized(f32::NAN), 60.0);
        assert_eq!(scale.to_normalized(0.0).to_bits(), 0.0_f32.to_bits());
        assert_eq!(scale.to_normalized(12_001.0).to_bits(), 1.0_f32.to_bits());
        assert_eq!(scale.to_normalized(f32::NAN).to_bits(), 0.0_f32.to_bits());
    }

    #[test]
    #[ignore = "requires the separately generated native crossover-range reference"]
    fn crossover_range_matches_native_conversion() {
        let reference = std::fs::read_to_string(
            "local-artifacts/multipass-reconstruction/crossover-range-reference.txt",
        )
        .expect("generate the native oracle reference first");
        let scale = CrossoverFrequencyScale::new();
        let mut count = 0;
        for line in reference.lines() {
            count += 1;
            let mut words = line.split_whitespace();
            let input = f32::from_bits(u32::from_str_radix(words.next().unwrap(), 16).unwrap());
            let expected_physical = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
            let expected_inverse = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
            let physical = scale.from_normalized(input);
            assert_eq!(physical.to_bits(), expected_physical);
            assert_eq!(scale.to_normalized(physical).to_bits(), expected_inverse);
            assert!(words.next().is_none());
        }
        assert_eq!(count, 6);
    }

    #[test]
    #[ignore = "requires the separately generated native band-split reference"]
    fn compact_band_chain_matches_native_frames_and_state() {
        let reference = std::fs::read_to_string(
            "local-artifacts/multipass-reconstruction/band-split-reference.txt",
        )
        .expect("generate the native oracle reference first");
        let mut lines = reference.lines();
        let counts = [64_usize, 33, 32, 1];
        let mut splitter = MultipassBandSplitter::new(48_000.0);
        let mut input_left = [0.0_f32; 64];
        let mut input_right = [0.0_f32; 64];
        let mut output_left = [[0.0_f32; 64]; 5];
        let mut output_right = [[0.0_f32; 64]; 5];
        for block in 0..counts.len() {
            let count = counts[block];
            let third_frequency = if block < 2 { 1000.0 } else { 2300.0 };
            for index in 0..count {
                let i = index as i32;
                let b = block as i32;
                input_left[index] = ((i + b * 3) % 17 - 8) as f32 * 0.03125;
                input_right[index] = ((i * 3 + b) % 19 - 9) as f32 * 0.015625;
            }
            {
                let [l0, l1, l2, l3, l4] = &mut output_left;
                let [r0, r1, r2, r3, r4] = &mut output_right;
                let mut bands = [
                    [&mut l0[..count], &mut r0[..count]],
                    [&mut l1[..count], &mut r1[..count]],
                    [&mut l2[..count], &mut r2[..count]],
                    [&mut l3[..count], &mut r3[..count]],
                    [&mut l4[..count], &mut r4[..count]],
                ];
                splitter.process(
                    [&input_left[..count], &input_right[..count]],
                    &[60.0, 200.0, third_frequency, 12000.0],
                    &mut bands,
                );
            }
            for index in 0..count {
                let mut words = lines.next().expect("frame line").split_whitespace();
                assert_eq!(words.next(), Some("frame"));
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), index);
                for band in 0..5 {
                    for value in [output_left[band][index], output_right[band][index]] {
                        let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                        assert_eq!(value.to_bits(), expected, "block {block} frame {index} band {band}");
                    }
                }
                assert!(words.next().is_none());
            }
            for (boundary, crossover) in splitter.crossovers.iter().enumerate() {
                let sections = [
                    &crossover.low[0], &crossover.low[1],
                    &crossover.high[0], &crossover.high[1],
                    &crossover.summing_allpass[0], &crossover.summing_allpass[1],
                    &crossover.summing_allpass[2],
                ];
                for (index, section) in sections.into_iter().enumerate() {
                    let mut words = lines.next().expect("state line").split_whitespace();
                    assert_eq!(words.next(), Some("state"));
                    assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
                    assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), boundary);
                    assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), index);
                    let values = section.integrator_1.into_iter()
                        .chain(section.integrator_2)
                        .chain(section.coefficients);
                    for value in values {
                        let expected = u64::from_str_radix(words.next().unwrap(), 16).unwrap();
                        assert_eq!(value.to_bits(), expected, "block {block} boundary {boundary} section {index}");
                    }
                    assert_eq!(words.next(), Some(if section.initial { "01" } else { "00" }));
                    assert!(words.next().is_none());
                }
            }
        }
        assert!(lines.next().is_none());
    }

    #[test]
    #[ignore = "requires the separately generated native lane-delay reference"]
    fn lane_delay_matches_native_samples_ring_and_silence_state() {
        let reference = std::fs::read_to_string(
            "local-artifacts/multipass-reconstruction/lane-delay-reference.txt",
        )
        .expect("generate the native oracle reference first");
        let mut lines = reference.lines();
        let counts = [5_usize, 8, 3, 6, 8, 7, 5, 8];
        let delays = [7, 7, 7, 7, 7, 4, 0, 9];
        let silence = [false, true, true, true, false, false, false, false];
        let mut delay = LaneDelay::new(8, 37);
        for block in 0..counts.len() {
            delay.set_delay(delays[block]);
            let mut frames = [0.0_f32; 8];
            if !silence[block] {
                for (index, sample) in frames[..counts[block]].iter_mut().enumerate() {
                    *sample = (block * 8 + index + 1) as f32 * 0.015625;
                }
            }
            delay.process(&mut frames[..counts[block]], silence[block]);
            let mut words = lines.next().expect("frame line").split_whitespace();
            assert_eq!(words.next(), Some("frame"));
            assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
            for value in &frames[..counts[block]] {
                let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                assert_eq!(value.to_bits(), expected, "block {block} frame");
            }
            assert!(words.next().is_none());
            let mut words = lines.next().expect("state line").split_whitespace();
            assert_eq!(words.next(), Some("state"));
            assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
            for value in [delay.delay, delay.write_position, delay.valid_frames, delay.remaining_non_silent] {
                assert_eq!(value, words.next().unwrap().parse::<usize>().unwrap(), "block {block} state");
            }
            assert!(words.next().is_none());
            let mut words = lines.next().expect("ring line").split_whitespace();
            assert_eq!(words.next(), Some("ring"));
            assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
            for value in &delay.ring {
                let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                assert_eq!(value.to_bits(), expected, "block {block} ring");
            }
            assert!(words.next().is_none());
        }
        assert!(lines.next().is_none());
    }

    #[test]
    #[ignore = "requires the separately generated native lane-pan reference"]
    fn lane_pan_matches_original_empty_lane_frames_and_ramp_state() {
        let reference = std::fs::read_to_string(
            "local-artifacts/multipass-reconstruction/lane-pan-reference.txt",
        )
        .expect("generate the native oracle reference first");
        let mut lines = reference.lines();
        let counts = [5_usize, 33, 64, 7, 3, 4, 4];
        let gains = [1.0_f32, 0.75, 0.75, 0.5, 1.0, 1.0, 1.0];
        let pans = [0.0_f32, -0.5, -0.5, 0.7, 0.8, 0.8, -1.0];
        let muted = [false, false, false, true, true, false, false];
        let solo = [false, false, false, false, true, false, false];
        let any_solo = [false, false, false, false, true, true, false];
        let mut gain = LanePanGain::new();
        let mut left = [0.0_f32; 64];
        let mut right = [0.0_f32; 64];
        for block in 0..counts.len() {
            let count = counts[block];
            for index in 0..count {
                let i = index as i32;
                let b = block as i32;
                left[index] = ((i + b * 3) % 17 + 1) as f32 * 0.03125;
                right[index] = ((i * 3 + b) % 19 + 1) as f32 * -0.015625;
            }
            gain.process(
                gains[block], pans[block], muted[block], solo[block], any_solo[block],
                [&mut left[..count], &mut right[..count]],
            );
            for index in 0..count {
                let mut words = lines.next().expect("frame line").split_whitespace();
                assert_eq!(words.next(), Some("frame"));
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), index);
                for value in [left[index], right[index]] {
                    let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                    assert_eq!(value.to_bits(), expected, "block {block} frame {index}");
                }
                assert!(words.next().is_none());
            }
            for (channel, smoother) in gain.channels.iter().enumerate() {
                let mut words = lines.next().expect("state line").split_whitespace();
                assert_eq!(words.next(), Some("state"));
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), channel);
                let state = &smoother.ramp;
                for value in [state.counter, state.step, state.start, state.target, state.current] {
                    let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                    assert_eq!(value.to_bits(), expected, "block {block} channel {channel}");
                }
                assert_eq!(words.next(), Some(if state.initial { "01" } else { "00" }));
                assert!(words.next().is_none());
            }
        }
        assert!(lines.next().is_none());
    }

    #[test]
    fn lane_dry_wet_starts_at_half_and_silent_reset_snaps_next_target() {
        let mut mixer = LaneDryWetMixer::new();
        let mut left = [1.0; 4];
        let mut right = [1.0; 4];
        let wet = [0.0; 4];
        mixer.process(0.0, [&mut left, &mut right], [&wet, &wet]);
        assert_eq!(left, [0.5, 0.625, 0.75, 0.875]);
        assert_eq!(right, left);
        mixer.mark_initial();
        left.fill(1.0);
        right.fill(1.0);
        mixer.process(0.0, [&mut left, &mut right], [&wet, &wet]);
        assert_eq!(left, [1.0; 4]);
        assert_eq!(right, [1.0; 4]);
    }

    #[test]
    fn lane_gain_gating_and_ramp_start_follow_the_native_control_flow() {
        let root_two = f32::from_bits(0x3fb5_04f3);
        assert_eq!(lane_stereo_gains(1.0, -1.0, false, false, false), [root_two, 0.0]);
        assert_eq!(lane_stereo_gains(1.0, -1.0, true, false, false), [0.0, 0.0]);
        assert_eq!(lane_stereo_gains(1.0, -1.0, true, true, true), [root_two, 0.0]);
        assert_eq!(lane_stereo_gains(1.0, -1.0, false, false, true), [0.0, 0.0]);

        let mut smoother = LanePanGain::new();
        let mut left = [1.0; 4];
        let mut right = [1.0; 4];
        smoother.process(0.0, -1.0, false, false, false, [&mut left, &mut right]);
        left.fill(1.0);
        right.fill(1.0);
        smoother.process(1.0, -1.0, false, false, false, [&mut left, &mut right]);
        assert_eq!(left, [0.0, root_two * 0.25, root_two * 0.5, root_two * 0.75]);
        assert_eq!(right, [0.0; 4]);
    }

    #[test]
    #[ignore = "requires the separately generated native lane-dry-wet reference"]
    fn dry_wet_mixer_matches_native_frames_and_state_across_ramps() {
        let reference = std::fs::read_to_string(
            "local-artifacts/multipass-reconstruction/lane-dry-wet-reference.txt",
        )
        .expect("generate the native oracle reference first");
        let mut lines = reference.lines();
        let counts = [5_usize, 33, 64, 7, 3, 4, 4];
        let targets = [0.0_f32, 0.25, 0.25, 1.0, 1.0, 0.0, 0.0];
        let mut mixer = LaneDryWetMixer::new();
        let mut dry = [[0.0_f32; 64]; 2];
        let mut wet = [[0.0_f32; 64]; 2];
        for block in 0..counts.len() {
            let count = counts[block];
            for index in 0..count {
                let i = index as i32;
                let b = block as i32;
                dry[0][index] = ((i + b) % 17 - 8) as f32 * 0.03125;
                dry[1][index] = ((i * 2 + b) % 19 - 9) as f32 * 0.015625;
                wet[0][index] = ((i + b * 2) % 13 - 6) as f32 * 0.0625;
                wet[1][index] = ((i * 3 + b) % 11 - 5) as f32 * 0.015625;
            }
            {
                let [dry_left, dry_right] = &mut dry;
                let [wet_left, wet_right] = &wet;
                mixer.process(
                    targets[block],
                    [&mut dry_left[..count], &mut dry_right[..count]],
                    [&wet_left[..count], &wet_right[..count]],
                );
            }
            for index in 0..count {
                let mut words = lines.next().expect("frame line").split_whitespace();
                assert_eq!(words.next(), Some("frame"));
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), index);
                for value in [dry[0][index], dry[1][index]] {
                    let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                    assert_eq!(value.to_bits(), expected, "block {block} frame {index}");
                }
                assert!(words.next().is_none());
            }
            let mut words = lines.next().expect("state line").split_whitespace();
            assert_eq!(words.next(), Some("state"));
            assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
            for value in [mixer.ramp.counter, mixer.ramp.step, mixer.ramp.start, mixer.ramp.target, mixer.ramp.current] {
                let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                assert_eq!(value.to_bits(), expected, "block {block} state");
            }
            assert_eq!(words.next(), Some(if mixer.ramp.initial { "01" } else { "00" }));
            assert!(words.next().is_none());
        }
        assert!(lines.next().is_none());
    }

    #[test]
    #[ignore = "requires the separately generated native band-route reference"]
    fn band_router_matches_native_frames_and_state_across_ramps() {
        let reference = std::fs::read_to_string(
            "local-artifacts/multipass-reconstruction/band-route-reference.txt",
        )
        .expect("generate the native oracle reference first");
        let mut lines = reference.lines();
        let counts = [5_usize, 33, 64, 7, 3, 4, 4];
        let targets = [0.0_f32, 0.25, 0.25, 1.0, 1.0, 0.0, 0.0];
        let mut mixer = BandRouteMixer::new();
        let mut input = [[0.0_f32; 64]; 2];
        let mut first = [[0.0_f32; 64]; 2];
        let mut second = [[0.0_f32; 64]; 2];
        for block in 0..counts.len() {
            let count = counts[block];
            for index in 0..count {
                let i = index as i32;
                let b = block as i32;
                input[0][index] = ((i + b) % 17 - 8) as f32 * 0.03125;
                input[1][index] = ((i * 2 + b) % 19 - 9) as f32 * 0.015625;
                first[0][index] = ((i + 2) % 7 - 3) as f32 * 0.0078125;
                first[1][index] = ((i + 3) % 7 - 3) as f32 * 0.0078125;
                second[0][index] = ((i + 4) % 7 - 3) as f32 * 0.0078125;
                second[1][index] = ((i + 5) % 7 - 3) as f32 * 0.0078125;
            }
            {
                let [input_left, input_right] = &input;
                let [first_left, first_right] = &mut first;
                let [second_left, second_right] = &mut second;
                mixer.process(
                    targets[block],
                    [&input_left[..count], &input_right[..count]],
                    [&mut first_left[..count], &mut first_right[..count]],
                    [&mut second_left[..count], &mut second_right[..count]],
                );
            }
            for index in 0..count {
                let mut words = lines.next().expect("frame line").split_whitespace();
                assert_eq!(words.next(), Some("frame"));
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), index);
                for value in [first[0][index], first[1][index], second[0][index], second[1][index]] {
                    let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                    assert_eq!(value.to_bits(), expected, "block {block} frame {index}");
                }
                assert!(words.next().is_none());
            }
            let mut words = lines.next().expect("state line").split_whitespace();
            assert_eq!(words.next(), Some("state"));
            assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
            for value in [mixer.ramp.counter, mixer.ramp.step, mixer.ramp.start, mixer.ramp.target, mixer.ramp.current] {
                let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                assert_eq!(value.to_bits(), expected, "block {block} state");
            }
            assert_eq!(words.next(), Some(if mixer.ramp.initial { "01" } else { "00" }));
            assert!(words.next().is_none());
        }
        assert!(lines.next().is_none());
    }

    #[test]
    #[ignore = "requires the separately generated native split-filter reference"]
    fn crossover_matches_native_frames_and_state_across_retargeting() {
        let reference = std::fs::read_to_string(
            "local-artifacts/multipass-reconstruction/split-filter-reference.txt",
        )
        .expect("generate the native oracle reference first");
        let mut lines = reference.lines();
        let mut splitter = MultipassCrossover::new(48_000.0);
        let counts = [33_usize, 32, 64, 7];
        let frequencies = [1000.0_f32, 1000.0, 2300.0, 300.0];
        let mut input = [[0.0; 4]; 64];
        let mut low = [[0.0; 4]; 64];
        let mut high = [[0.0; 4]; 64];
        let mut previous = [[[0.0; 4]; 64]; 3];
        for block in 0..4 {
            let count = counts[block];
            for index in 0..count {
                let i = index as i32;
                let b = block as i32;
                input[index] = [
                    ((i + b * 3) % 17 - 8) as f32 * 0.03125,
                    ((i * 3 + b) % 19 - 9) as f32 * 0.015625,
                    9.0,
                    -9.0,
                ];
                for (j, band) in previous.iter_mut().enumerate() {
                    let j = j as i32;
                    band[index] = [
                        ((i + j * 5 + b) % 13 - 6) as f32 * 0.03125,
                        ((i * 2 + j + b) % 11 - 5) as f32 * 0.015625,
                        7.0,
                        -7.0,
                    ];
                }
            }
            {
                let [first, second, third] = &mut previous;
                let mut prior: [&mut [[f32; 4]]; 3] = [
                    &mut first[..count],
                    &mut second[..count],
                    &mut third[..count],
                ];
                splitter.process(
                    frequencies[block],
                    &input[..count],
                    &mut low[..count],
                    &mut high[..count],
                    &mut prior,
                );
            }
            for index in 0..count {
                let mut words = lines.next().expect("frame line").split_whitespace();
                assert_eq!(words.next(), Some("frame"));
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), index);
                for frame in [&low[index], &high[index], &previous[0][index], &previous[1][index], &previous[2][index]] {
                    for sample in frame {
                        let expected = u32::from_str_radix(words.next().unwrap(), 16).unwrap();
                        assert_eq!(sample.to_bits(), expected, "block {block} frame {index}");
                    }
                }
                assert!(words.next().is_none());
            }
            let sections = [
                &splitter.low[0], &splitter.low[1],
                &splitter.high[0], &splitter.high[1],
                &splitter.summing_allpass[0], &splitter.summing_allpass[1],
                &splitter.summing_allpass[2],
            ];
            for (index, section) in sections.into_iter().enumerate() {
                let mut words = lines.next().expect("state line").split_whitespace();
                assert_eq!(words.next(), Some("state"));
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), block);
                assert_eq!(words.next().unwrap().parse::<usize>().unwrap(), index);
                let values = section.integrator_1.into_iter()
                    .chain(section.integrator_2)
                    .chain(section.coefficients);
                for value in values {
                    let expected = u64::from_str_radix(words.next().unwrap(), 16).unwrap();
                    assert_eq!(value.to_bits(), expected, "block {block} section {index}");
                }
                assert_eq!(words.next(), Some(if section.initial { "01" } else { "00" }));
                assert!(words.next().is_none());
            }
        }
        assert!(lines.next().is_none());
    }

    #[test]
    fn lane_gain_mapping_clamps_physical_boundaries() {
        let max = f32::from_bits(0x404a_62c2);
        assert_eq!(lane_gain_from_normalized(-100.0).to_bits(), 0.0_f32.to_bits());
        assert_eq!(lane_gain_from_normalized(100.0).to_bits(), max.to_bits());
        assert_eq!(lane_gain_to_normalized(-1.0).to_bits(), lane_gain_to_normalized(0.0).to_bits());
        assert_eq!(lane_gain_to_normalized(f32::INFINITY).to_bits(), lane_gain_to_normalized(max).to_bits());
        // Expected from the peer's libSystem f32/fmaf evaluation, not an
        // observed render of the original application.
        assert_eq!(lane_gain_from_normalized(f32::from_bits(0x3f3c_84fc)).to_bits(), 0x3f82_d73e);
    }


    #[test]
    fn host_parameter_coordinates_preserve_native_group_boundaries() {
        let cases = [
            (0, 0, 1), (3, 0, 4), (4, 103, 1), (33, 107, 6),
            (34, 108, 1), (40, 109, 1), (45, 109, 6), (46, 110, 0),
            (53, 117, 0), (54, 630, 0), (89, 633, 8), (90, 636, 0),
            (100, 646, 0), (101, 102, 0), (102, 0, 7), (103, 634, 0),
            (107, 634, 4), (108, 647, 0), (112, 651, 0), (113, 2797, 0),
            (117, 2801, 0), (148, 2832, 0), (149, 2861, 0), (150, 2861, 5),
            (151, 2833, 0), (154, 2836, 0), (178, 2860, 0),
        ];
        for (id, node, field) in cases {
            assert_eq!(host_parameter_coordinate(id), Some(ModelParameter { node, field }));
        }
        assert_eq!(host_parameter_coordinate(179), None);
    }

    #[test]
    fn host_automation_paths_use_the_interleaved_native_descriptor_order() {
        assert_eq!(automation_slot_for_host_parameter(113), Some(0));
        assert_eq!(automation_slot_for_host_parameter(148), Some(35));
        assert_eq!(automation_slot_for_host_parameter(149), None);
        assert_eq!(automation_slot_for_host_parameter(150), None);
        assert_eq!(automation_slot_for_host_parameter(151), Some(36));
        assert_eq!(automation_slot_for_host_parameter(178), Some(63));
    }

    #[test]
    fn summing_allpass_respects_native_frequency_cap() {
        let at_cap = summing_allpass_coefficients(100.0, 49.0);
        let above_cap = summing_allpass_coefficients(100.0, 60.0);
        assert_eq!(at_cap.map(f64::to_bits), above_cap.map(f64::to_bits));
        assert_eq!(at_cap[0].to_bits(), at_cap[4].to_bits());
        assert_eq!(at_cap[1].to_bits(), at_cap[3].to_bits());
        assert!(at_cap.iter().all(|coefficient| coefficient.is_finite()));
    }

    #[test]
    fn sparse_bands_select_preceding_crossover_indices() {
        let layout = active_bands([false, true, false, true, true]);
        assert_eq!(&layout.bands[..layout.band_count], &[1, 3, 4]);
        assert_eq!(&layout.crossovers[..layout.crossover_count], &[1, 3]);
        let empty = active_bands([false; 5]);
        assert_eq!((empty.band_count, empty.crossover_count), (0, 0));
    }

    #[test]
    fn gain_and_warmth_writes_distinguish_missing_from_zero() {
        let gain = gain_stage_parameter_writes(0.0);
        assert_eq!((gain[0].id, gain[1].id), (34, 40));
        assert_eq!(gain[0].value, gain[1].value);
        let untouched = warmth_parameter_writes(0.0, 0);
        assert!(untouched[0].is_none() && untouched[1].is_none());
        let boosted = warmth_parameter_writes(0.0, 1);
        assert!(boosted[0].is_none());
        let write = boosted[1].unwrap();
        assert_eq!(write.id, 40);
        assert_eq!(write.value.to_bits(), 0x3f3c_84fc);
    }

    #[test]
    fn controller_ratio_and_crossover_edges_follow_native_branches() {
        assert_eq!(normalized_ratio(1.0), 0.5);
        assert_eq!(normalized_ratio(30.0), 1.0);
        assert_eq!(normalized_ratio(1.0 / 30.0), 0.0);
        assert_eq!(normalized_ratio(-1.0), 0.5);
        assert_eq!(normalized_ratio(f64::NAN), 1.0);
        assert_eq!(normalized_crossover(60.0), 0.0);
        assert_eq!(normalized_crossover(12_000.0), 1.0);
        assert_eq!(normalized_crossover(f64::NAN), 1.0);
    }

    #[test]
    fn controller_gain_correction_and_parameter_limits_are_observable() {
        let mut bands = [DynamicsBand { threshold_db: -30.0, ratio: 2.0, gain_db: 0.0 }; 4];
        bands[1].gain_db = -48.0;
        let uncorrected =
            dynamics_parameter_writes(bands, [60.0, 1200.0, 12_000.0], false, false, -201.0, 10.0);
        assert_eq!((uncorrected[9 + 4].id, uncorrected[9 + 4].value), (134, 1e-8_f32));
        assert_eq!((uncorrected[9 + 5].id, uncorrected[9 + 5].value), (138, 1e-8_f32));
        assert_eq!((uncorrected[36].id, uncorrected[36].value), (2, 1.0));
        assert_eq!((uncorrected[38].id, uncorrected[38].value), (0, 0.0));
        assert_eq!((uncorrected[39].id, uncorrected[39].value), (34, 0.0));
        assert_eq!((uncorrected[40].id, uncorrected[40].value), (40, 1.0));

        let bands = [DynamicsBand { threshold_db: -30.0, ratio: 2.0, gain_db: 0.0 }; 4];
        let corrected = dynamics_parameter_writes(bands, [60.0; 3], false, true, 0.0, 0.0);
        assert_eq!(corrected[5].id, 137);
        assert_eq!(corrected[5].value, ((15.45_f64 + 24.0) / 48.0) as f32);
    }

}
