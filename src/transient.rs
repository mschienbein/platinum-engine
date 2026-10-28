//! HeartCore's stereo TransientShaper DSP kernel.
//!
//! This implements the active and sleeping DSP methods, not the surrounding
//! snapin's bypass/delay routing, editor-to-DSP parameter mapping, or host. The
//! caller supplies effective DSP-model values, the actual detector channels,
//! and native event-aligned DSP chunks while holding HeartCore's FPCR.FZ mode.
//! Serialized preset values are not automatically effective DSP values.

use crate::math;
use crate::portable::arm_scalar::ArmFloat;

const TIME_EPSILON: f32 = f32::from_bits(0x3580_0000);
const ENERGY_FLOOR: f32 = f32::from_bits(0x36a0_0000);
const ENVELOPE_MINIMUM: f32 = f32::from_bits(0x3c23_d70a); // 0.01

#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" {
    #[link_name = "powf"]
    fn system_powf(base: f32, exponent: f32) -> f32;
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::math::powf as system_powf;

#[cfg(target_arch = "x86_64")]
use crate::portable::arm::{
    max_number_fz as max_number, min_number_fz as min_number,
    max_propagating_fz as max_propagating, min_propagating_fz as min_propagating,
    arm_lt_fz as arm_lt,
};

#[inline]
fn power(base: f32, exponent: f32) -> f32 {
    // HeartCore calls the macOS binary32 libSystem primitive directly.
    unsafe { system_powf(base, exponent) }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn max_number(a: f32, b: f32) -> f32 {
    let result: f32;
    // Preserve the original ARM64 FMAXNM, including its NaN and zero rules.
    unsafe {
        core::arch::asm!(
            "fmaxnm {result:s}, {a:s}, {b:s}",
            result = lateout(vreg) result, a = in(vreg) a, b = in(vreg) b,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn min_number(a: f32, b: f32) -> f32 {
    let result: f32;
    unsafe {
        core::arch::asm!(
            "fminnm {result:s}, {a:s}, {b:s}",
            result = lateout(vreg) result, a = in(vreg) a, b = in(vreg) b,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn max_propagating(a: f32, b: f32) -> f32 {
    let result: f32;
    unsafe {
        core::arch::asm!(
            "fmax {result:s}, {a:s}, {b:s}",
            result = lateout(vreg) result, a = in(vreg) a, b = in(vreg) b,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn min_propagating(a: f32, b: f32) -> f32 {
    let result: f32;
    unsafe {
        core::arch::asm!(
            "fmin {result:s}, {a:s}, {b:s}",
            result = lateout(vreg) result, a = in(vreg) a, b = in(vreg) b,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    result
}

#[inline]
fn clamp_unit(value: f32) -> f32 {
    min_number(max_number(value, 0.0), 1.0)
}

#[inline]
fn coefficient(time: f32, rate: f32) -> f32 {
    if time > 0.0 {
        1.0 - math::exp2f(-1.0 / (time * rate))
    } else {
        1.0
    }
}

#[inline]
fn changed(a: f32, b: f32) -> bool {
    (a - b).abs() > TIME_EPSILON
}

// ARM64 FCMP followed by the LT condition also selects LT for unordered input.
#[cfg(target_arch = "aarch64")]
#[inline]
fn arm_lt(a: f32, b: f32) -> bool {
    a < b || a.is_nan() || b.is_nan()
}

#[inline]
fn shape(value: f32) -> f32 {
    let t = clamp_unit(value);
    let square = t * t;
    square * (3.0 - (t + t))
}

#[derive(Clone, Copy)]
struct Follower {
    rise_time: f32,
    fall_time: f32,
    rise: f32,
    fall: f32,
    value: f32,
}

impl Follower {
    fn new() -> Self {
        Self {
            rise_time: f32::from_bits(0x3b03_126f), // 0.002
            fall_time: f32::from_bits(0x3d4c_cccd), // 0.05
            rise: 0.0,
            fall: 0.0,
            value: ENVELOPE_MINIMUM,
        }
    }

    #[inline]
    fn follow(&mut self, target: f32) -> f32 {
        let coeff = if arm_lt(self.value, target) { self.rise } else { self.fall };
        self.value = (target - self.value).arm_mul_add(coeff, self.value);
        self.value
    }
}

/// Values already present in the TransientShaper DSP model, not host-normalized
/// controls and not a promise that on-disk preset fields reach the model unchanged.
#[derive(Clone, Copy, Debug)]
pub struct TransientParameters {
    pub attack: f32,
    pub pump: f32,
    pub sustain: f32,
    pub speed: f32,
    pub clip: bool,
}

/// The three meter values written by the corresponding native DSP method.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransientMeters {
    pub transient: f32,
    pub sustain: f32,
    pub pump: f32,
}

/// One native stereo DSP instance. There is no sample delay in the active
/// sample kernel; generic snapin latency/bypass compensation belongs to its
/// caller. Zero input can still change its IIR state.
#[derive(Clone)]
pub struct TransientShaper {
    params: TransientParameters,
    rate: f32,
    follower_rate: f32,
    slow_time: f32,
    slow_coefficient: f32,
    slow_energy: f32,
    fast_time: f32,
    fast_coefficient: f32,
    fast_energy: f32,
    transient: Follower,
    sustain: Follower,
}

impl TransientShaper {
    /// Constructs and initializes the native state for an effective sample rate.
    /// Rates must be finite and greater than 2^-20; normal audio rates satisfy
    /// this and initialize both native followers.
    pub fn new(sample_rate: f32, params: TransientParameters) -> Self {
        let mut result = Self {
            params,
            rate: 0.0,
            follower_rate: 0.0,
            slow_time: f32::from_bits(0x3dcc_cccd), // constructor: 0.1
            slow_coefficient: 0.0,
            slow_energy: 0.0,
            fast_time: f32::from_bits(0x3dcc_cccd),
            fast_coefficient: 0.0,
            fast_energy: 0.0,
            transient: Follower::new(),
            sustain: Follower::new(),
        };
        result.set_sample_rate(sample_rate);
        result
    }

    /// Corresponds to the sample-rate method at 0xc09c68. The existing time
    /// constants and envelope states survive a rate change.
    pub fn set_sample_rate(&mut self, rate: f32) {
        assert!(rate.is_finite() && rate > TIME_EPSILON, "invalid transient sample rate");
        self.rate = rate;
        self.slow_coefficient = coefficient(self.slow_time, rate);
        self.fast_coefficient = coefficient(self.fast_time, rate);
        if changed(self.follower_rate, rate) {
            self.follower_rate = rate;
            self.transient.rise = coefficient(self.transient.rise_time, rate);
            self.transient.fall = coefficient(self.transient.fall_time, rate);
            self.sustain.rise = coefficient(self.sustain.rise_time, rate);
            self.sustain.fall = coefficient(self.sustain.fall_time, rate);
        }
    }

    /// Writes model values for the next active DSP chunk; it does not perform
    /// host normalization, clamping, automation or snapin enablement.
    pub fn set_parameters(&mut self, params: TransientParameters) {
        self.params = params;
    }

    /// Corresponds to 0xc09ba8; coefficient caches and parameter values remain.
    pub fn reset(&mut self) {
        self.slow_energy = 0.0;
        self.fast_energy = 0.0;
        self.transient.value = ENVELOPE_MINIMUM;
        self.sustain.value = ENVELOPE_MINIMUM;
    }

    fn prepare_active_chunk(&mut self) {
        let slow_time = f32::from_bits(0x3ba3_d70a); // 0.005
        if changed(self.slow_time, slow_time) {
            self.slow_time = slow_time;
            self.slow_coefficient = 1.0 - math::exp2f(-200.0 / self.rate);
        }
        let fast_time = f32::from_bits(0x3a83_126f); // 0.001
        if changed(self.fast_time, fast_time) {
            self.fast_time = fast_time;
            self.fast_coefficient = 1.0 - math::exp2f(f32::from_bits(0xc479_ffff) / self.rate);
        }
        if self.transient.rise_time.abs() > TIME_EPSILON {
            self.transient.rise_time = 0.0;
            self.transient.rise = 1.0;
        }
        let release = f32::from_bits(0x3ccc_cccd) / self.params.speed;
        if changed(self.transient.fall_time, release) {
            self.transient.fall_time = release;
            self.transient.fall = if self.params.speed > 0.0 {
                coefficient(release, self.follower_rate)
            } else {
                1.0
            };
        }
        if self.sustain.rise_time.abs() > TIME_EPSILON {
            self.sustain.rise_time = 0.0;
            self.sustain.rise = 1.0;
        }
        let sustain_release = release * 4.0;
        if changed(self.sustain.fall_time, sustain_release) {
            self.sustain.fall_time = sustain_release;
            self.sustain.fall = coefficient(sustain_release, self.follower_rate);
        }
    }

    /// Processes exactly one native DSP chunk in place. `detector` selects a
    /// separate stereo sidechain; `None` uses the two unmodified input samples
    /// for each frame. All four slices must have equal length. The native host,
    /// not this kernel, selects the sidechain, enters sleep, establishes FPCR.FZ,
    /// and performs its balanced event-boundary subdivision.
    pub fn process_chunk(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        detector: Option<(&[f32], &[f32])>,
    ) -> TransientMeters {
        assert_eq!(left.len(), right.len());
        if let Some((detector_left, detector_right)) = detector {
            assert_eq!(left.len(), detector_left.len());
            assert_eq!(left.len(), detector_right.len());
        }
        self.prepare_active_chunk();
        let pump_exponent = self.params.pump + self.params.pump;
        let attack_exponent = self.params.attack * 3.0;
        let sustain_exponent = self.params.sustain * 3.0;
        let mut last_pump_base = 0.0;
        let mut last_one_minus_sustain = 0.0;

        for i in 0..left.len() {
            let (detector_left, detector_right) = match detector {
                Some((l, r)) => (l[i], r[i]),
                None => (left[i], right[i]),
            };
            let amplitude = max_number(detector_left.abs(), detector_right.abs());
            let energy = amplitude * amplitude;
            self.slow_energy = (energy - self.slow_energy).arm_mul_add(self.slow_coefficient, self.slow_energy);
            self.fast_energy = (energy - self.fast_energy).arm_mul_add(self.fast_coefficient, self.fast_energy);
            let slow = max_number(self.slow_energy, ENERGY_FLOOR);
            let fast = max_number(self.fast_energy, ENERGY_FLOOR);
            let onset = (fast / slow).sqrt().arm_mul_add(1.5, -2.0);
            let transient_target = max_number(ENVELOPE_MINIMUM, shape(onset).abs());
            let transient = self.transient.follow(transient_target);
            let sustain_target = max_number(ENVELOPE_MINIMUM, transient.abs());
            let sustain = self.sustain.follow(sustain_target);
            let one_minus_sustain = 1.0 - sustain;
            let pump_base = one_minus_sustain + transient;
            let pump_gain = power(pump_base, pump_exponent);
            let attack_gain = power(transient + 1.0, attack_exponent);
            let sustain_gain = power(2.0 - sustain, sustain_exponent);
            let gain = (sustain_gain * attack_gain) * pump_gain;
            left[i] = gain * left[i];
            right[i] = gain * right[i];

            last_pump_base = pump_base;
            last_one_minus_sustain = one_minus_sustain;
        }

        let (last_transient, last_sustain_difference, last_pump_meter) = if left.is_empty() {
            (0.0, 0.0, 0.0)
        } else {
            let t = clamp_unit(self.sustain.value.arm_mul_add(10.0, f32::from_bits(0xbdcc_cccd)));
            (
                self.transient.value,
                1.0 - last_pump_base,
                ((t * t) * last_one_minus_sustain) * (3.0 - (t + t)),
            )
        };

        if self.params.clip {
            // The native SIMD pass runs after the gain loop. It operates on
            // four-frame padded storage; only the requested frames are visible.
            for sample in left.iter_mut().chain(right.iter_mut()) {
                *sample = soft_clip(*sample);
            }
        }
        TransientMeters {
            transient: clamp_unit(last_transient),
            sustain: clamp_unit(last_sustain_difference),
            pump: clamp_unit(last_pump_meter),
        }
    }

    /// Advances one chunk already classified as sleeping by the enclosing
    /// snapin dispatcher. This is 0xc09df8, not equivalent to processing a
    /// buffer of zeros through `process_chunk`: it uses integer powers and
    /// advances the first follower once per chunk. The dispatcher decides when
    /// sleep starts and must handle bypass, delay and any output buffer itself.
    pub fn advance_sleeping_chunk(&mut self, frames: u32) -> TransientMeters {
        assert!(frames <= i32::MAX as u32);
        let n = frames as i32;
        let slow_decay = math::powi_f64(f64::from(1.0_f32 - self.slow_coefficient), n) as f32;
        self.slow_energy *= slow_decay;
        let fast_decay = math::powi_f64(f64::from(1.0_f32 - self.fast_coefficient), n) as f32;
        self.fast_energy *= fast_decay;
        let slow = max_number(self.slow_energy, ENERGY_FLOOR);
        let fast = max_number(self.fast_energy, ENERGY_FLOOR);
        let onset = if slow > ENERGY_FLOOR && fast > ENERGY_FLOOR {
            (fast / slow).sqrt().arm_mul_add(1.5, -2.0)
        } else {
            -0.5
        };
        let target = max_number(ENVELOPE_MINIMUM, shape(onset).abs());
        let transient = self.transient.follow(target);
        let sustain_decay = math::powi_f64(f64::from(1.0_f32 - self.sustain.fall), n) as f32;
        self.sustain.value *= sustain_decay;
        let sustain = self.sustain.value;
        let t = clamp_unit(sustain.arm_mul_add(10.0, f32::from_bits(0xbdcc_cccd)));
        let square = t * t;
        let pump = (-sustain).arm_mul_add(square, square) * (3.0 - (t + t));
        TransientMeters {
            transient: clamp_unit(transient),
            sustain: clamp_unit(sustain - transient),
            pump: clamp_unit(pump),
        }
    }
}

#[inline]
fn soft_clip(sample: f32) -> f32 {
    let magnitude = f32::from_bits(sample.to_bits() & 0x7fff_ffff);
    let clipped_base = min_propagating(magnitude, f32::from_bits(0x3f66_6666));
    let excess = max_propagating(magnitude.arm_mul_add(10.0, -9.0), 0.0);
    let curved = excess.arm_mul_add(excess, excess);
    let magnitude = ((curved * f32::from_bits(0x3dcc_cccd)) / (curved + 1.0)) + clipped_base;
    f32::from_bits((sample.to_bits() & 0x8000_0000) | magnitude.to_bits())
}
