//! HeartCore DynamicsDsp's active, stereo, at-most-64-frame sample kernel.
//!
//! Parameters here are the *physical f32 DSP-model values*, not Platinum Notes
//! template values or normalized Multipass host IDs. The caller must resolve
//! the preset, host parameters, model events, lane routing, bypass/sleep, and
//! chunking before invoking this kernel. See `docs/VERIFICATION.md` for scope.
//! This module does not host the proprietary processor.

use crate::math;
use crate::portable::arm_scalar::ArmFloat;

#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" {
    // Mach-O imports ___exp10f and _log10f; C-level symbol names below.
    fn __exp10f(value: f32) -> f32;
    fn log10f(value: f32) -> f32;
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::math::{exp10f as __exp10f, log10f};

#[inline]
fn exp10f(value: f32) -> f32 {
    unsafe { __exp10f(value) }
}

#[inline]
fn log10(value: f32) -> f32 {
    unsafe { log10f(value) }
}

/// Values loaded by the active DSP at model offsets `0xbf94..0xc1d4`.
/// The two ratios are internal physical values, not UI/template ratios.
#[derive(Clone, Copy, Debug)]
pub struct DynamicsParameters {
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

/// The ten mutable physical fields of a DynamicsDspModel. Event IDs are ksot
/// internal IDs, not the outer Multipass host parameter IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynamicsField {
    InputGain,
    OutputGain,
    LowThreshold,
    LowRatio,
    HighThreshold,
    HighRatio,
    Attack,
    Release,
    Knee,
    Mix,
}

#[derive(Clone, Copy)]
enum Mapping {
    Linear,
    Log { first_log: f32, last_log: f32 },
}

#[derive(Clone, Copy)]
struct Descriptor {
    first: f32,
    last: f32,
    mapping: Mapping,
}

impl Descriptor {
    #[inline]
    fn clamp_physical(self, value: f32) -> f32 {
        value.arm_max(self.first.arm_min(self.last)).arm_min(self.first.arm_max(self.last))
    }

    // Model descriptor slots 0 and 12: clamp physical input first.
    #[inline]
    fn normalize(self, value: f32) -> f32 {
        let value = self.clamp_physical(value);
        match self.mapping {
            Mapping::Linear => (value - self.first) / (self.last - self.first),
            Mapping::Log { first_log, last_log } => {
                (math::logf(value + 0.0) - first_log) / (last_log - first_log)
            }
        }
    }

    // Model descriptor slots 1 and 13: convert then clamp the physical result.
    #[inline]
    fn physical(self, normalized: f32) -> f32 {
        let value = match self.mapping {
            Mapping::Linear => (self.last - self.first).arm_mul_add(normalized, self.first),
            Mapping::Log { first_log, last_log } => {
                let exponent = (last_log - first_log).arm_mul_add(normalized, first_log);
                math::expf(exponent) - 0.0
            }
        };
        self.clamp_physical(value)
    }
}

impl DynamicsField {
    /// The identifier used by the snapin's editor and DSP event tables.
    pub const fn event_id(self) -> u32 {
        match self {
            Self::InputGain => 0,
            Self::OutputGain => 1,
            Self::LowThreshold => 2,
            Self::LowRatio => 3,
            Self::HighThreshold => 4,
            Self::HighRatio => 5,
            Self::Release => 6,
            Self::Mix => 7,
            Self::Attack => 422,
            Self::Knee => 423,
        }
    }

    fn descriptor(self) -> Descriptor {
        use DynamicsField::*;
        let linear = |first, last| Descriptor { first, last, mapping: Mapping::Linear };
        let log = |first, last, first_log, last_log| Descriptor {
            first, last, mapping: Mapping::Log { first_log, last_log },
        };
        match self {
            InputGain | OutputGain => linear(-24.0, 24.0),
            LowThreshold | HighThreshold => linear(-50.0, 0.0),
            LowRatio => log(30.0, f32::from_bits(0x3d08_8889),
                f32::from_bits(0x4059_ad38), f32::from_bits(0xc059_ad38)),
            HighRatio => log(2.0, f32::from_bits(0x3d08_8889),
                f32::from_bits(0x3f31_7218), f32::from_bits(0xc059_ad38)),
            Attack | Release => log(f32::from_bits(0x3e4c_cccd), 5.0,
                f32::from_bits(0xbfce_0210), f32::from_bits(0x3fce_0210)),
            Knee => linear(0.0, 25.0),
            Mix => linear(0.0, 1.0),
        }
    }

    /// Apply the model's descriptor conversion to a normalized value.
    /// This is the DSP-model descriptor, not proof of the outer host mapping.
    pub fn model_physical_from_normalized(self, normalized: f32) -> f32 {
        self.descriptor().physical(normalized)
    }

    /// Convert one raw physical event to the value loaded by the active DSP,
    /// for the constructor's normal model flags and supplied normalized
    /// additive modulation. The native event may arrive later than the write;
    /// caller owns queue ordering and modulation state.
    pub fn model_event_value(self, raw_physical: f32, normalized_modulation: f32) -> f32 {
        let descriptor = self.descriptor();
        let base = descriptor.normalize(raw_physical);
        let normalized = (base + normalized_modulation).arm_max(0.0).arm_min(1.0);
        descriptor.physical(normalized)
    }
}

impl DynamicsParameters {
    /// Apply one already-delivered ksot model event, not a Multipass host ID.
    pub fn apply_model_event(&mut self, field: DynamicsField, raw_physical: f32, normalized_modulation: f32) {
        let value = field.model_event_value(raw_physical, normalized_modulation);
        match field {
            DynamicsField::InputGain => self.input_gain_db = value,
            DynamicsField::OutputGain => self.output_gain_db = value,
            DynamicsField::LowThreshold => self.low_threshold_db = value,
            DynamicsField::LowRatio => self.low_ratio = value,
            DynamicsField::HighThreshold => self.high_threshold_db = value,
            DynamicsField::HighRatio => self.high_ratio = value,
            DynamicsField::Attack => self.attack = value,
            DynamicsField::Release => self.release = value,
            DynamicsField::Knee => self.knee = value,
            DynamicsField::Mix => self.mix = value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynamicsError {
    InvalidSampleRate,
    InvalidBlock,
}

/// Compute the controller's signed constructor argument from the current
/// child-editor Sample Rate. The controller publishes this same *unclamped*
/// value as latency; `Dynamics::new` applies the DSP constructor's 0..=896
/// delay clamp separately. The caller must supply the value effective at
/// construction, which need not equal a later rate-setting event.
pub fn controller_lookahead_argument(editor_sample_rate: f32) -> i32 {
    (editor_sample_rate * f32::from_bits(0x3b3e_37c6)).round() as i32
}

/// Last values published by the active sample loop to its model's meters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicsMeters {
    pub detector_db: f32,
    pub gain_db: f32,
}

// `0x95eabc`: on this kernel's <=64-frame calls the native buffer has capacity
// 1024 and the constructor clamps the requested delay to [0, 896]. Therefore
// writing the input block before reading it is equivalent to this sample-wise
// ring: no live unread value can be overwritten even when the ring wraps.
struct Delay {
    samples: [f32; 1024],
    position: usize,
    seen: usize,
    frames: usize,
}

impl Delay {
    fn new(frames: usize) -> Self {
        Self { samples: [0.0; 1024], position: 0, seen: 0, frames }
    }

    fn reset(&mut self) {
        // The native reset clears the cursors, not the backing allocation.
        self.position = 0;
        self.seen = 0;
    }

    #[inline]
    fn next(&mut self, sample: f32) -> f32 {
        if self.frames == 0 {
            return sample;
        }
        let read = (self.position + 1024 - self.frames) & 1023;
        let output = if self.seen < self.frames { 0.0 } else { self.samples[read] };
        self.samples[self.position] = sample;
        self.position = (self.position + 1) & 1023;
        self.seen = (self.seen + 1).min(self.frames);
        output
    }
}

// The three `0xc8/0xe0/0xf8` objects use previous target values, not the last
// rounded intermediate sample, as the start of the next block's ramp.
#[derive(Clone, Copy)]
struct Ramp {
    counter: f32,
    step: f32,
    start: f32,
    target: f32,
    current: f32,
    first: bool,
}

impl Ramp {
    fn new() -> Self {
        Self { counter: 0.0, step: 0.0, start: 0.0, target: 0.0, current: 0.0, first: true }
    }

    fn reset(&mut self) {
        self.first = true;
    }

    #[inline]
    fn begin(&mut self, target: f32, frames: usize) {
        let previous = self.target;
        self.counter = 0.0;
        let difference = f64::from(target - previous);
        if self.first || (difference > -1.0e-15 && difference < 1.0e-15) {
            self.first = false;
            self.step = 0.0;
            self.start = target;
            self.current = target;
        } else {
            self.step = 1.0 / frames as f32;
            self.start = previous;
            self.current = previous;
        }
        self.target = target;
    }

    #[inline]
    fn next(&mut self) -> f32 {
        let phase = self.counter.arm_mul_add(self.step, self.step);
        self.counter += 1.0;
        self.current = phase.arm_mul_add(self.target - self.start, self.start);
        self.current
    }
}

// ARM FCMP + B.LE treats unordered values as taking the first branch.
#[inline]
fn soft_knee(distance: f32, width: f32) -> f32 {
    if !(distance > -width) {
        0.0
    } else if distance < width {
        let sum = distance + width;
        ((sum * sum) * 0.25) / width
    } else {
        distance
    }
}

/// Independent state for one stereo DynamicsDsp active processing slot.
///
/// `lookahead_frames` is the constructor's signed delay argument; the native
/// constructor clamps it to 0..=896. The actual argument for a particular
/// hosted preset is separate from this kernel and must not be guessed.
pub struct Dynamics {
    sample_rate: f32,
    detector_coefficient: f32,
    energy: f32,
    detector_db: f32,
    gain_db: f32,
    low_slope: f32,
    high_slope: f32,
    low_knee: f32,
    high_knee: f32,
    low_threshold: f32,
    high_threshold: f32,
    input_gain: Ramp,
    output_gain: Ramp,
    mix: Ramp,
    left_delay: Delay,
    right_delay: Delay,
}

impl Dynamics {
    pub fn new(sample_rate: f32, lookahead_frames: i32, parameters: DynamicsParameters) -> Result<Self, DynamicsError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(DynamicsError::InvalidSampleRate);
        }
        let frames = lookahead_frames.clamp(0, 896) as usize;
        let mut result = Self {
            sample_rate,
            detector_coefficient: 0.0,
            energy: 0.0,
            detector_db: 0.0,
            gain_db: 0.0,
            low_slope: 0.0,
            high_slope: 0.0,
            low_knee: 0.0,
            high_knee: 0.0,
            low_threshold: 0.0,
            high_threshold: 0.0,
            input_gain: Ramp::new(),
            output_gain: Ramp::new(),
            mix: Ramp::new(),
            left_delay: Delay::new(frames),
            right_delay: Delay::new(frames),
        };
        result.set_sample_rate(sample_rate)?;
        result.reset(parameters);
        Ok(result)
    }

    /// Recalculate the detector coefficient without resetting its envelope.
    /// The native rate setter also resets a separate, unused-by-slot-16 ring.
    pub fn set_sample_rate(&mut self, sample_rate: f32) -> Result<(), DynamicsError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(DynamicsError::InvalidSampleRate);
        }
        self.sample_rate = sample_rate;
        let time = f32::from_bits(0x3a3e_37c6);
        self.detector_coefficient = 1.0 - math::exp2f(-1.0 / (time * sample_rate));
        Ok(())
    }

    /// Model the DSP's slot-17 reset. Parameters must be the current model
    /// values at the time the host resets the effect, not a future update.
    pub fn reset(&mut self, parameters: DynamicsParameters) {
        self.energy = 0.0;
        self.left_delay.reset();
        self.right_delay.reset();
        self.low_slope = 1.0 - parameters.low_ratio;
        self.high_slope = parameters.high_ratio + -1.0;
        self.low_knee = parameters.knee / (parameters.low_ratio + 1.0);
        self.high_knee = parameters.knee / (parameters.high_ratio + 1.0);
        self.low_threshold = parameters.low_threshold_db;
        self.high_threshold = parameters.high_threshold_db;
        self.detector_db = f32::from_bits(0xc288_5461);
        let low = soft_knee(self.low_threshold - self.detector_db, self.low_knee);
        let high = soft_knee(self.detector_db - self.high_threshold, self.high_knee);
        self.gain_db = low.arm_mul_add(self.low_slope, high * self.high_slope);
        self.input_gain.reset();
        self.output_gain.reset();
        self.mix.reset();
    }

    pub fn lookahead_frames(&self) -> usize {
        self.left_delay.frames
    }

    pub fn meters(&self) -> DynamicsMeters {
        DynamicsMeters { detector_db: self.detector_db, gain_db: self.gain_db }
    }

    #[cfg(test)]
    pub(crate) fn debug_kernel_words(&self) -> [u32; 9] {
        [self.low_slope, self.high_slope, self.low_knee, self.high_knee,
         self.low_threshold, self.high_threshold, self.detector_db,
         self.gain_db, self.energy].map(f32::to_bits)
    }

    /// Process exactly one native active-slot chunk, in place.
    ///
    /// Both channels must have equal lengths in 1..=64. The original host's
    /// event segmentation and balanced subdivision determine these boundaries;
    /// arbitrary caller repartitioning can change control ramps and results.
    pub fn process_block(&mut self, parameters: DynamicsParameters, left: &mut [f32], right: &mut [f32]) -> Result<(), DynamicsError> {
        let frames = left.len();
        if frames == 0 || frames > 64 || right.len() != frames {
            return Err(DynamicsError::InvalidBlock);
        }
        let input_target = exp10f(parameters.input_gain_db * f32::from_bits(0x3d4c_cccd));
        let output_target = exp10f(parameters.output_gain_db * f32::from_bits(0x3d4c_cccd));
        self.input_gain.begin(input_target, frames);
        self.output_gain.begin(output_target, frames);
        self.mix.begin(parameters.mix, frames);

        let inverse_frames = 1.0 / frames as f32;
        let low_sum = parameters.low_ratio + self.low_slope;
        let low_step = (-low_sum).arm_mul_add(inverse_frames, inverse_frames);
        let high_step = ((parameters.high_ratio + -1.0) - self.high_slope) * inverse_frames;
        let low_knee_step = ((parameters.knee / (parameters.low_ratio + 1.0)) - self.low_knee) * inverse_frames;
        let high_knee_step = ((parameters.knee / (parameters.high_ratio + 1.0)) - self.high_knee) * inverse_frames;
        let low_threshold_step = (parameters.low_threshold_db - self.low_threshold) * inverse_frames;
        let high_threshold_step = (parameters.high_threshold_db - self.high_threshold) * inverse_frames;

        let attack_rate = self.sample_rate * parameters.attack;
        let attack_below = math::exp2f(f32::from_bits(0xc4f9_ffff) / attack_rate);
        let attack_above = math::exp2f(f32::from_bits(0xc479_ffff) / attack_rate);
        let release_rate = parameters.release * (f32::from_bits(0x3ca3_d70a) * self.sample_rate);
        let release = math::exp2f(-1.0 / release_rate);

        for (left, right) in left.iter_mut().zip(right.iter_mut()) {
            let input_gain = self.input_gain.next();
            let output_gain = self.output_gain.next();
            let mix = self.mix.next();
            self.low_slope += low_step;
            self.high_slope += high_step;
            self.low_knee += low_knee_step;
            self.high_knee += high_knee_step;
            self.low_threshold += low_threshold_step;
            self.high_threshold += high_threshold_step;

            let dry_left = self.left_delay.next(*left);
            let dry_right = self.right_delay.next(*right);
            let detect_left = *left * input_gain;
            let detect_right = *right * input_gain;
            let energy = ((detect_left * detect_left) + (detect_right * detect_right)) * 0.5;
            self.energy = (energy - self.energy).arm_mul_add(self.detector_coefficient, self.energy);
            let level = log10(self.energy.arm_max(f32::from_bits(0x3423_d70a))) * 10.0;
            let attack = if self.detector_db < self.high_threshold { attack_below } else { attack_above };
            let coefficient = if self.detector_db < level { attack } else { release };
            let delta = level - self.detector_db;
            self.detector_db += (-coefficient).arm_mul_add(delta, delta);

            let low = soft_knee(self.low_threshold - self.detector_db, self.low_knee);
            let high = soft_knee(self.detector_db - self.high_threshold, self.high_knee);
            self.gain_db = high.arm_mul_add(self.high_slope, low * self.low_slope);
            let gain = exp10f(self.gain_db * f32::from_bits(0x3d4c_cccd));
            let combined = gain * (input_gain * output_gain);
            let difference = combined.arm_mul_add(mix, -mix);
            *left = difference.arm_mul_add(dry_left, dry_left);
            *right = difference.arm_mul_add(dry_right, dry_right);
        }
        Ok(())
    }
}
