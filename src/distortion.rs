//! DistortionDsp's type-1 (Saturate) stereo kernel and slot-9 wrapper.
//!
//! Inputs to this module are the **effective DSP model fields**, not the
//! serialized preset floats or the host's normalized parameter values. Their
//! conversion and automation are owned by the surrounding Multipass model.
//! `Saturate::process_subblock` is the direct slot-16 boundary;
//! `DistortionProcessor::process_chunk` also handles the slot-9 wrapper.
//! The caller owns event boundaries, the host's balanced <=64-frame
//! subdivision, and the scoped ARM64 FPCR.FZ setting. The coefficients and
//! sample operations below are statically transcribed from the pinned macOS
//! ARM64 binary; independent sample/coefficients comparison is pending.
//! Other Distortion modes are deliberately not represented as Saturate.

use crate::math;
use crate::portable::arm_scalar::ArmFloat;

unsafe extern "C" {
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    #[link_name = "__sincos_stret"]
    fn system_sincos(value: f64) -> SineCosine;
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    #[link_name = "asinh"]
    fn system_asinh(value: f64) -> f64;
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    #[link_name = "sinh"]
    fn system_sinh(value: f64) -> f64;
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    #[link_name = "cosh"]
    fn system_cosh(value: f64) -> f64;
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    #[link_name = "logb"]
    fn system_logb(value: f64) -> f64;
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    #[link_name = "scalbn"]
    fn system_scalbn(value: f64, exponent: i32) -> f64;
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::math::{asinh as system_asinh, cosh as system_cosh, sinh as system_sinh, logb as system_logb, scalbn as system_scalbn};
#[cfg(any(target_os = "linux", feature = "portable"))]
fn system_sincos(value: f64) -> SineCosine {
    let pair = crate::portable::math::sincos(value);
    SineCosine { sine: pair.sine, cosine: pair.cosine }
}

// Darwin arm64 returns this homogeneous pair in d0 and d1.
#[repr(C)]
struct SineCosine {
    sine: f64,
    cosine: f64,
}

/// Values at HeartCore model offsets bf94, bfd4, c014, c094 and c0d4.
/// `remove_dc` is the low bit at c114. These are not normalized UI fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SaturateParameters {
    pub drive: f32,
    pub bias: f32,
    pub spread: f32,
    pub dynamics: f32,
    pub mix: f32,
    pub remove_dc: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistortionError {
    InvalidSampleRate,
    /// The native first rate update does not initialize envelope coefficients
    /// at or below its 2^-20 change threshold.
    UnsupportedSampleRateInitialization,
    ChannelLengthMismatch,
    SubblockTooLong,
    ChunkTooLarge,
}
/// Descriptors used by the Distortion model's normalized parameter bridge.
/// The preset reader stores raw physical-domain values. Which modulation
/// and synchronization events reach the DSP model remains to be established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatDomain {
    Drive,
    Bias,
    Spread,
    Dynamics,
    Mix,
}

impl FloatDomain {
    fn bounds(self) -> (f32, f32) {
        match self {
            Self::Drive => (0.5, 10.0),
            Self::Bias => (-1.0, 1.0),
            Self::Spread | Self::Dynamics | Self::Mix => (0.0, 1.0),
        }
    }

    /// Descriptor slots 0/12: clamp a physical f32, then normalize it.
    pub fn normalize(self, physical: f32) -> f32 {
        let (low, high) = self.bounds();
        let physical = physical.arm_max(low).arm_min(high);
        match self {
            Self::Drive => {
                let log_low = f32::from_bits(0xbf31_7218);
                let log_high = f32::from_bits(0x4013_5d8e);
                (math::logf(physical + 0.0) - log_low) / (log_high - log_low)
            }
            _ => (physical - low) / (high - low),
        }
    }

    /// Descriptor slots 1/13: convert a normalized f32 and clamp physically.
    pub fn denormalize(self, normalized: f32) -> f32 {
        let (low, high) = self.bounds();
        let physical = match self {
            Self::Drive => {
                let log_low = f32::from_bits(0xbf31_7218);
                let log_high = f32::from_bits(0x4013_5d8e);
                math::expf((log_high - log_low).arm_mul_add(normalized, log_low)) - 0.0
            }
            _ => (high - low).arm_mul_add(normalized, low),
        };
        physical.arm_max(low).arm_min(high)
    }
}

/// Integer descriptor 0x963ce4 followed by 0x963bf4 for the type domain.
pub fn type_from_normalized(normalized: f32) -> i32 {
    (5.0_f32.arm_mul_add(normalized, 0.5) as i32).clamp(0, 5)
}

/// The DC switch uses the same integer descriptor with bounds zero and one.
pub fn dc_from_normalized(normalized: f32) -> bool {
    (1.0_f32.arm_mul_add(normalized, 0.5) as i32).clamp(0, 1) != 0
}


/// Coefficient order is b0, b1, b2, a1, a2, as stored by HeartCore.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BiquadCoefficients(pub [f64; 5]);

// The 0x65bb7c builder uses these libSystem functions, including the
// explicitly scaled complex divisions. Replacing them with ordinary complex
// division or algebraically simplified roots changes rounding.
fn scale_exponent(real: f64, imaginary: f64) -> i32 {
    let magnitude = real.abs().arm_max(imaginary.abs());
    // FCVTZS saturates to signed i32; NEG is a wrapping integer operation.
    (unsafe { system_logb(magnitude) } as i32).wrapping_neg()
}

fn scale(value: f64, exponent: i32) -> f64 {
    unsafe { system_scalbn(value, exponent) }
}

fn inverse_complex(real: f64, imaginary: f64) -> (f64, f64) {
    let exponent = scale_exponent(real, imaginary);
    let real = scale(real, exponent);
    let imaginary = scale(imaginary, exponent);
    let denominator = imaginary.arm_mul_add(imaginary, real * real);
    let out_imaginary = scale(-imaginary / denominator, exponent);
    let out_real = scale(real / denominator, exponent);
    (out_real, out_imaginary)
}

// 0x65bd78..0x65be6c: (1 + z) / (1 - z), with the native
// exponent-scaled complex divide and its explicitly fused operations.
fn bilinear_root(real: f64, imaginary: f64) -> (f64, f64) {
    let divisor_real = 1.0 - real;
    let exponent = scale_exponent(divisor_real, imaginary);
    let divisor_real = scale(divisor_real, exponent);
    let divisor_imaginary = scale(-imaginary, exponent);
    let denominator = divisor_imaginary.arm_mul_add(divisor_imaginary, divisor_real * divisor_real);
    let first_imaginary = real.arm_mul_add(divisor_imaginary, divisor_imaginary);
    // ARM64 FNMSUB is product minus addend.
    let numerator_imaginary = divisor_real.arm_mul_add(imaginary, -first_imaginary);
    let first_real = real.arm_mul_add(divisor_real, divisor_real);
    let numerator_real = divisor_imaginary.arm_mul_add(imaginary, first_real);
    let out_imaginary = scale(numerator_imaginary / denominator, exponent);
    let out_real = scale(numerator_real / denominator, exponent);
    (out_real, out_imaginary)
}

fn lowpass_coefficients(rate: f32, cutoff: f32) -> [BiquadCoefficients; 5] {
    let angle = (f64::from(cutoff) * std::f64::consts::PI) / f64::from(rate);
    let prewarp = math::tan(angle);
    let attenuation = 55.0_f64 * f64::from_bits(0x3fb9_9999_9999_999a);
    let epsilon = (math::exp10(attenuation) - 1.0).sqrt();
    let hyperbolic_angle = unsafe { system_asinh(epsilon) } / 10.0;
    let neg_cosh = -unsafe { system_cosh(hyperbolic_angle) };
    let neg_sinh = -unsafe { system_sinh(hyperbolic_angle) };
    let mut coefficients = [BiquadCoefficients([0.0; 5]); 5];
    let reciprocal_order = 1.0_f64 / 10.0;
    for (index, coefficient) in coefficients.iter_mut().enumerate() {
        let theta = (index as f64).arm_mul_add(std::f64::consts::PI, std::f64::consts::FRAC_PI_2)
            * reciprocal_order;
        let pair = unsafe { system_sincos(theta) };
        let pole = inverse_complex(pair.sine * neg_sinh, pair.cosine * neg_cosh);
        let zero = inverse_complex(0.0, -pair.cosine);
        let pole = bilinear_root(pole.0 * prewarp, pole.1 * prewarp);
        let zero = bilinear_root(zero.0 * prewarp, zero.1 * prewarp);
        let b1_unscaled = zero.0 * -2.0;
        let b2_unscaled = zero.0.arm_mul_add(zero.0, zero.1 * zero.1);
        let a1 = pole.0 * -2.0;
        let a2 = pole.0.arm_mul_add(pole.0, pole.1 * pole.1);
        let numerator_dc = (b1_unscaled + 1.0) + b2_unscaled;
        let denominator_dc = (a1 + 1.0) + a2;
        let exponent = scale_exponent(denominator_dc, 0.0);
        let real = scale(denominator_dc, exponent);
        let imaginary = scale(0.0, exponent);
        let denominator = imaginary.arm_mul_add(imaginary, real * real);
        let dc_ratio = scale((numerator_dc * real) / denominator, exponent);
        let b0 = 1.0 / dc_ratio;
        *coefficient = BiquadCoefficients([b0, b1_unscaled * b0, b2_unscaled * b0, a1, a2]);
    }
    coefficients
}

// 0x65b600 type 2, called by 0xae97c8 with 30 Hz and this exact Q.
fn dc_highpass_coefficients(sample_rate: f32) -> BiquadCoefficients {
    let rate = f64::from(sample_rate);
    let cutoff = (rate * f64::from_bits(0x3fdf_5c28_f5c2_8f5c)).arm_min(30.0);
    let angle = (cutoff * std::f64::consts::TAU) / rate;
    let pair = unsafe { system_sincos(angle) };
    let q = f64::from_bits(0x3fe6_a09e_6000_0000);
    let alpha = pair.sine / (q + q);
    let sum = pair.cosine + 1.0;
    let numerator = pair.cosine.arm_mul_add(0.5, 0.5);
    let denominator = alpha + 1.0;
    let a1 = pair.cosine * -2.0;
    let a2 = 1.0 - alpha;
    let reciprocal = 1.0 / denominator;
    BiquadCoefficients([
        numerator * reciprocal,
        reciprocal * -sum,
        numerator * reciprocal,
        a1 * reciprocal,
        a2 * reciprocal,
    ])
}

#[derive(Clone, Copy, Default)]
struct DirectForm {
    // Both samples and feedback histories are f64; each stage emits f32.
    x1: [f64; 2],
    x2: [f64; 2],
    y1: [f64; 2],
    y2: [f64; 2],
}

impl DirectForm {
    fn process(&mut self, frame: &mut [f32; 2], coefficients: BiquadCoefficients) {
        let [b0, b1, b2, a1, a2] = coefficients.0;
        for (channel, sample) in frame.iter_mut().enumerate() {
            let input = f64::from(*sample);
            let feedback = self.y2[channel].arm_mul_add(a2, self.y1[channel] * a1);
            let numerator = self.x1[channel].arm_mul_add(b1, input * b0);
            let numerator = self.x2[channel].arm_mul_add(b2, numerator);
            let output = numerator - feedback;
            self.x2[channel] = self.x1[channel];
            self.x1[channel] = input;
            self.y2[channel] = self.y1[channel];
            self.y1[channel] = output;
            *sample = output as f32;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Highpass {
    previous: [f64; 2],
    older: [f64; 2],
}

impl Highpass {
    fn process(&mut self, frame: &mut [f32; 2], coefficients: BiquadCoefficients) {
        let [b0, b1, b2, a1, a2] = coefficients.0;
        for (channel, sample) in frame.iter_mut().enumerate() {
            let feedback = self.previous[channel].arm_mul_add(a1, self.older[channel] * a2);
            let state = f64::from(*sample) - feedback;
            let output = self.previous[channel].arm_mul_add(b1, state * b0);
            let output = self.older[channel].arm_mul_add(b2, output);
            self.older[channel] = self.previous[channel];
            self.previous[channel] = state;
            *sample = output as f32;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ScalarRamp {
    count: f32,
    step: f32,
    start: f32,
    target: f32,
    initialized: bool,
}

impl ScalarRamp {
    fn begin(&mut self, target: f32, samples: i32) {
        self.count = 0.0;
        let difference = target - self.target;
        if self.initialized && (difference.is_nan() || (difference as f64) <= -1e-15 || (difference as f64) >= 1e-15) {
            self.start = self.target;
            self.target = target;
            self.step = 1.0 / samples as f32;
        } else {
            self.start = target;
            self.target = target;
            self.step = 0.0;
            self.initialized = true;
        }
    }

    fn next(&mut self) -> f32 {
        let progress = self.count.arm_mul_add(self.step, self.step);
        self.count += 1.0;
        progress.arm_mul_add(self.target - self.start, self.start)
    }
}

#[derive(Clone, Copy, Default)]
struct PairRamp {
    count: f32,
    step: f32,
    start: [f32; 2],
    target: [f32; 2],
    initialized: bool,
}

impl PairRamp {
    fn begin(&mut self, target: [f32; 2], samples: i32) {
        self.count = 0.0;
        if self.initialized {
            self.start = self.target;
            self.target = target;
            self.step = 1.0 / samples as f32;
        } else {
            self.start = target;
            self.target = target;
            self.step = 0.0;
            self.initialized = true;
        }
    }

    fn next(&mut self) -> [f32; 2] {
        let progress = self.count.arm_mul_add(self.step, self.step);
        self.count += 1.0;
        let complement = 1.0 - progress;
        [
            self.start[0].arm_mul_add(complement, self.target[0] * progress),
            self.start[1].arm_mul_add(complement, self.target[1] * progress),
        ]
    }
}

// FMIN/FMAX, not the NaN-suppressing FMINNM/FMAXNM. Bounds are nonzero, so
// finite signed-zero behavior is preserved. A signaling NaN is quieted.
fn clamp_saturate(value: f32) -> f32 {
    if value.is_nan() {
        return f32::from_bits(value.to_bits() | 0x0040_0000);
    }
    value.arm_max(-1.0).arm_min(1.0)
}

// 0xae9a28..0xae9a74: compensation at each channel's bias target.
fn compensation(offset: f32) -> f32 {
    let t = clamp_saturate(offset).arm_mul_add(0.5, 0.5);
    let polynomial = t.arm_mul_add(-6.0, 15.0).arm_mul_add(t, -10.0);
    let cubic = (t * t) * (t + t);
    cubic.arm_mul_add(polynomial, 1.0)
}

// 0xaea334..0xaea374: the actual sample branch, including its distinct
// arithmetic order from the negated compensation computation.
fn saturate_sample(value: f32, correction: f32, gain: f32) -> f32 {
    let t = clamp_saturate(value).arm_mul_add(0.5, 0.5);
    let polynomial = t.arm_mul_add(6.0, -15.0).arm_mul_add(t, 10.0);
    let cubic = (t * t) * (t + t);
    cubic.arm_mul_add(polynomial, correction - 1.0) * gain
}

/// Stateful implementation of the Saturate branch of `DistortionDsp`.
///
/// This deliberately exposes effective model state and DSP subblocks, not a
/// fabricated mapping from serialized presets or a complete Multipass effect.
pub struct Saturate {
    params: SaturateParameters,
    factor: usize,
    lowpass: [BiquadCoefficients; 5],
    highpass: BiquadCoefficients,
    up: [DirectForm; 5],
    down: [DirectForm; 5],
    dc: Highpass,
    dc_was_enabled: bool,
    attack: f32,
    release: f32,
    envelope: [f32; 2],
    drive: ScalarRamp,
    dynamics: ScalarRamp,
    mix: ScalarRamp,
    offset: PairRamp,
    correction: PairRamp,
    silent_frames: i32,
    sleeping: bool,
}

impl Saturate {
    pub fn new(sample_rate: f32, params: SaturateParameters) -> Result<Self, DistortionError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(DistortionError::InvalidSampleRate);
        }
        if sample_rate <= f32::from_bits(0x3580_0000) {
            return Err(DistortionError::UnsupportedSampleRateInitialization);
        }
        // The oversampler starts at rate 1 and factor 1; 0x658d70 only
        // recalculates the factor when the f32 rate differs by > 0.001.
        let factor = if (sample_rate - 1.0).abs() > f32::from_bits(0x3a83_126f) {
            (150_000.0_f32 / sample_rate).ceil().arm_max(1.0).arm_min(16.0) as usize
        } else {
            1
        };
        let processing_rate = sample_rate * factor as f32;
        let cutoff = (f64::from(sample_rate) * f64::from_bits(0x3fe1_9999_9999_999a)).arm_min(25_000.0) as f32;
        let lowpass = lowpass_coefficients(processing_rate, cutoff);
        let attack_time = f32::from_bits(0x3b83_126f);
        let release_time = f32::from_bits(0x3dcc_cccd);
        let attack = 1.0 - math::exp2f(-1.0 / (attack_time * sample_rate));
        let release = 1.0 - math::exp2f(-1.0 / (release_time * sample_rate));
        Ok(Self {
            params,
            factor,
            lowpass,
            highpass: dc_highpass_coefficients(sample_rate),
            up: [DirectForm::default(); 5],
            down: [DirectForm::default(); 5],
            dc: Highpass::default(),
            dc_was_enabled: false,
            attack,
            release,
            envelope: [f32::from_bits(0x38d1_b717); 2],
            drive: ScalarRamp::default(),
            dynamics: ScalarRamp::default(),
            mix: ScalarRamp::default(),
            offset: PairRamp::default(),
            correction: PairRamp::default(),
            silent_frames: i32::MAX,
            sleeping: false,
        })
    }

    pub fn set_parameters(&mut self, params: SaturateParameters) {
        self.params = params;
    }

    pub fn oversampling_factor(&self) -> usize {
        self.factor
    }

    /// The same five generated coefficients feed both filter banks.
    pub fn oversampling_coefficients(&self) -> &[BiquadCoefficients; 5] {
        &self.lowpass
    }

    pub fn dc_coefficients(&self) -> BiquadCoefficients {
        self.highpass
    }

    /// Clears histories and resets all parameter ramps as slot 17 does.
    pub fn reset(&mut self) {
        self.up = [DirectForm::default(); 5];
        self.down = [DirectForm::default(); 5];
        self.dc = Highpass::default();
        self.dc_was_enabled = false;
        self.envelope = [f32::from_bits(0x38d1_b717); 2];
        self.drive = ScalarRamp::default();
        self.dynamics = ScalarRamp::default();
        self.mix = ScalarRamp::default();
        self.offset = PairRamp::default();
        self.correction = PairRamp::default();
    }
    /// Resets the enclosing 0xae98e8 dispatcher as well as slot 17.
    pub fn reset_dispatcher(&mut self) {
        self.silent_frames = i32::MAX;
        self.sleeping = false;
        self.reset();
    }

    /// Process the active-path dispatcher 0xaeade8, including its native
    /// silence sleep/wake branch. `known_silent` is the upstream context
    /// byte, not a measurement made by this method; `model_delay` is the
    /// enclosing model's signed delay value. This is not the slot-9 bypass,
    /// dry-delay or crossfade wrapper. Do not interleave this method with
    /// direct `process_subblock` calls for the same stream.
    pub fn process_sleep_aware_subblock(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        known_silent: bool,
        model_delay: i32,
    ) -> Result<(), DistortionError> {
        if left.len() != right.len() {
            return Err(DistortionError::ChannelLengthMismatch);
        }
        if left.len() > i32::MAX as usize / self.factor {
            return Err(DistortionError::SubblockTooLong);
        }
        if known_silent {
            if self.sleeping {
                return Ok(());
            }
            if self.silent_frames > model_delay.wrapping_add(64) {
                // Slots 18 and 15 are both empty for DistortionDsp.
                self.sleeping = true;
                return Ok(());
            }
            self.silent_frames = self.silent_frames.wrapping_add(left.len() as i32);
        } else {
            self.silent_frames = 0;
            if self.sleeping {
                // Slot 19 marks all five ramps for direct initialization;
                // the other filter/envelope histories are not reset here.
                self.drive.initialized = false;
                self.dynamics.initialized = false;
                self.mix.initialized = false;
                self.offset.initialized = false;
                self.correction.initialized = false;
            }
            self.sleeping = false;
        }
        self.process_subblock(left, right)
    }


    /// Process exactly one host-scheduled stereo DSP subblock in place.
    ///
    /// The caller must set FPCR.FZ around the host processing scope and
    /// restore it afterward, just as HeartCore does. Empty subblocks are
    /// admitted, but still initialize the native ramps and DC transition.
    pub fn process_subblock(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<(), DistortionError> {
        if left.len() != right.len() {
            return Err(DistortionError::ChannelLengthMismatch);
        }
        if left.len() > i32::MAX as usize / self.factor {
            return Err(DistortionError::SubblockTooLong);
        }
        let samples = (left.len() * self.factor) as i32;
        let offsets = [self.params.bias + self.params.spread, self.params.bias - self.params.spread];
        self.mix.begin(self.params.mix, samples);
        self.drive.begin(self.params.drive, samples);
        self.offset.begin(offsets, samples);
        self.correction.begin([compensation(offsets[0]), compensation(offsets[1])], samples);
        self.dynamics.begin(self.params.dynamics, samples);
        if !self.params.remove_dc && self.dc_was_enabled {
            self.dc = Highpass::default();
            self.dc_was_enabled = false;
        }
        let floor = f32::from_bits(0x38d1_b717);
        let mut oversampled = [[0.0_f32; 2]; 16];
        for (left, right) in left.iter_mut().zip(right.iter_mut()) {
            let sample = [*left, *right];
            oversampled[..self.factor].fill(sample);
            for (stage, coefficient) in self.up.iter_mut().zip(self.lowpass) {
                for frame in &mut oversampled[..self.factor] {
                    stage.process(frame, coefficient);
                }
            }
            for frame in &mut oversampled[..self.factor] {
                let mix = self.mix.next();
                let drive = self.drive.next();
                let offset = self.offset.next();
                let correction = self.correction.next();
                let dynamics = self.dynamics.next();
                for channel in 0..2 {
                    let input = frame[channel];
                    let magnitude = input.abs();
                    let magnitude = if magnitude.is_nan() {
                        f32::from_bits(magnitude.to_bits() | 0x0040_0000)
                    } else {
                        magnitude.arm_max(floor)
                    };
                    // FCMP followed by LE also selects release for unordered.
                    let coefficient = if magnitude.is_nan() || self.envelope[channel].is_nan() || magnitude <= self.envelope[channel] { self.release } else { self.attack };
                    self.envelope[channel] = coefficient.arm_mul_add(magnitude - self.envelope[channel], self.envelope[channel]);
                    let gain = self.envelope[channel].arm_mul_add(dynamics, -dynamics) + 1.0;
                    let driven = (input * drive) / gain;
                    let wet = saturate_sample(driven + offset[channel], correction[channel], gain);
                    frame[channel] = (wet - input).arm_mul_add(mix, input);
                }
            }
            for (stage, coefficient) in self.down.iter_mut().zip(self.lowpass) {
                for frame in &mut oversampled[..self.factor] {
                    stage.process(frame, coefficient);
                }
            }
            let mut frame = oversampled[0];
            if self.params.remove_dc {
                self.dc.process(&mut frame, self.highpass);
            }
            *left = frame[0];
            *right = frame[1];
        }
        if self.params.remove_dc {
            self.dc_was_enabled = true;
        }
        Ok(())
    }
}

/// Generic model gates consulted by slot 9, kept separate from the six
/// Saturate model fields. The three switches are read by their low bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistortionRouting {
    pub enabled: i32,
    pub host_enabled: i32,
    pub host_bypass: i32,
    pub dry_latency_frames: i32,
}

/// The native block's silence and bypass flags. Slot 9 may clear `silent`
/// after checking the resulting samples against the native quiet threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistortionContext {
    pub silent: bool,
    pub bypass: bool,
}

/// The slot-9 wrapper surrounding a Saturate kernel and two native dry rings.
/// The host must schedule model events and balance event-bounded segments into
/// at most 64 frames before each call, with FPCR.FZ scoped externally.
pub struct DistortionProcessor {
    kernel: Saturate,
    dry_left: crate::limiter::DryDelay,
    dry_right: crate::limiter::DryDelay,
    previous_active: bool,
    synchronize_active: bool,
}

impl DistortionProcessor {
    pub fn new(rate: f32, params: SaturateParameters) -> Result<Self, DistortionError> {
        Ok(Self {
            kernel: Saturate::new(rate, params)?,
            dry_left: crate::limiter::DryDelay::new(),
            dry_right: crate::limiter::DryDelay::new(),
            previous_active: true,
            synchronize_active: true,
        })
    }

    pub fn set_parameters(&mut self, params: SaturateParameters) {
        self.kernel.set_parameters(params);
    }

    pub fn kernel(&self) -> &Saturate {
        &self.kernel
    }

    pub fn is_sleeping(&self) -> bool {
        self.kernel.sleeping
    }

    /// Reproduce the generic wrapper reset followed by distortion slot 17.
    pub fn reset(&mut self) {
        self.kernel.reset_dispatcher();
        self.previous_active = true;
        self.synchronize_active = true;
        self.dry_left.reset_counters();
        self.dry_right.reset_counters();
    }

    pub fn process_chunk(
        &mut self,
        routing: DistortionRouting,
        context: &mut DistortionContext,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), DistortionError> {
        if left.len() != right.len() {
            return Err(DistortionError::ChannelLengthMismatch);
        }
        if left.len() > 64 {
            return Err(DistortionError::ChunkTooLarge);
        }
        // Both rings have 10,240 samples with two 64-sample margins.
        let delay = routing.dry_latency_frames.max(0).min(10_112) as usize;
        self.dry_left.configure(delay);
        self.dry_right.configure(delay);
        let active = !context.bypass
            && routing.enabled & 1 != 0
            && routing.host_enabled & 1 != 0
            && routing.host_bypass & 1 == 0;
        if self.synchronize_active {
            self.synchronize_active = false;
            self.previous_active = active;
        }
        if !active && !self.previous_active {
            self.dry_left.process(left);
            self.dry_right.process(right);
            self.previous_active = false;
        } else {
            if active && !self.previous_active {
                self.reset();
            }
            self.dry_left.remember(left);
            self.dry_right.remember(right);
            self.kernel.process_sleep_aware_subblock(left, right, context.silent, routing.dry_latency_frames)?;
            if !active {
                self.reset();
            }
            self.previous_active = active;
        }
        if context.silent {
            let quiet = f32::from_bits(0x3580_0000);
            context.silent = left.iter().zip(right.iter()).all(|(&l, &r)| {
                l > -quiet && l < quiet && r > -quiet && r < quiet
            });
        }
        if !context.silent {
            self.kernel.silent_frames = 0;
        } else if !active {
            self.kernel.sleeping = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{compensation, saturate_sample};
    use std::hint::black_box;

    // Exercise both modes even when the test runner inherited an enabled FZ bit.
    struct RestoreFloatControl(u64);
    impl RestoreFloatControl {
        fn clear() -> Self {
            #[cfg(target_arch = "aarch64")]
            {
                let previous: u64;
                unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) previous, options(nostack, preserves_flags)); }
                let cleared = previous & !(1 << 24);
                unsafe { std::arch::asm!("msr fpcr, {value}", value = in(reg) cleared, options(nostack, preserves_flags)); }
                Self(previous)
            }
            #[cfg(target_arch = "x86_64")]
            {
                let mut previous = 0u32;
                unsafe { std::arch::asm!("stmxcsr [{address}]", address = in(reg) &mut previous, options(nostack, preserves_flags)); }
                let cleared = previous & !((1 << 15) | (1 << 6));
                unsafe { std::arch::asm!("ldmxcsr [{address}]", address = in(reg) &cleared, options(nostack, preserves_flags)); }
                Self(u64::from(previous))
            }
        }
    }
    impl Drop for RestoreFloatControl {
        fn drop(&mut self) {
            #[cfg(target_arch = "aarch64")]
            unsafe { std::arch::asm!("msr fpcr, {value}", value = in(reg) self.0, options(nostack, preserves_flags)); }
            #[cfg(target_arch = "x86_64")]
            {
                let previous = self.0 as u32;
                unsafe { std::arch::asm!("ldmxcsr [{address}]", address = in(reg) &previous, options(nostack, preserves_flags)); }
            }
        }
    }

    fn lowpass_asinh_chain() -> [u64; 4] {
        let attenuation = black_box(55.0_f64) * black_box(f64::from_bits(0x3fb9_9999_9999_999a));
        let exp = super::math::exp10(black_box(attenuation));
        let epsilon = black_box(exp - 1.0).sqrt();
        let angle = unsafe { super::system_asinh(black_box(epsilon)) } / black_box(10.0);
        [attenuation.to_bits(), exp.to_bits(), epsilon.to_bits(), angle.to_bits()]
    }

    #[test]
    fn lowpass_asinh_constant_chain_matches_macos_in_both_fz_modes() {
        let _restore = RestoreFloatControl::clear();
        let expected = [0x4016_0000_0000_0000, 0x4113_4d0f_1066_b7cc,
                        0x4081_92b9_3689_2703, 0x3fe6_7b16_d722_2c0a];
        let disabled = lowpass_asinh_chain();
        let enabled = crate::fp_mode::with_flush_to_zero(lowpass_asinh_chain);
        println!("lowpass asinh chain FZ=0: {disabled:016x?}; FZ=1: {enabled:016x?}");
        assert_eq!(disabled, expected, "FZ disabled");
        assert_eq!(enabled, expected, "FZ enabled");
    }

    #[test]
    fn saturate_polynomial_has_instruction_derived_exact_binary_vectors() {
        // These dyadic values are exact under every indicated FMA; they are
        // derived from the instructions, not recorded original output.
        for (input, expected) in [
            (-1.0_f32, -1.0_f32),
            (-0.5, -0.792_968_75),
            (0.0, 0.0),
            (0.5, 0.792_968_75),
            (1.0, 1.0),
        ] {
            assert_eq!(saturate_sample(input, 0.0, 1.0).to_bits(), expected.to_bits());
        }
        assert_eq!(compensation(-1.0).to_bits(), 1.0_f32.to_bits());
        assert_eq!(compensation(0.0).to_bits(), 0.0_f32.to_bits());
        assert_eq!(compensation(0.5).to_bits(), (-0.792_968_75_f32).to_bits());
        assert_eq!(compensation(1.0).to_bits(), (-1.0_f32).to_bits());
        assert_eq!(saturate_sample(0.5, compensation(0.5), 1.0).to_bits(), 0.0_f32.to_bits());
    }
}
