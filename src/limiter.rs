//! HeartCore `LimiterDsp` for one stereo processing instance.
//!
//! This is the recovered native DSP and its active/sleep/dry dispatcher, not a
//! host for the proprietary library. The caller supplies the native context
//! flags and calls `process_chunk` on the host's event-bounded, at-most-64-frame
//! chunks. The host owns event timing, the 50 ms wrapper warmup, mono scratch
//! buffers, file trim/tail policy, and the scoped ARM FPCR flush-to-zero mode.
//! The constructor and normalized editor conversion run outside that scope;
//! model-event application and processing run inside it, as in the reference.
//! See `docs/VERIFICATION.md` for public verification scope and limits.

use core::arch::aarch64::{
    vabs_f32, vdup_n_f32, vdupq_laneq_f32, vdupq_n_f32, vfmaq_f32,
    vget_lane_f32, vgetq_lane_f32, vld1q_f32, vmaxnm_f32, vmaxq_f32,
    vminnm_f32, vsetq_lane_f32, vsubq_f32,
};

use crate::math;

/// Maximum native DSP call size. The host balances larger event segments into
/// `ceil(frames / 64)` chunks before invoking this processor.
pub const MAX_CHUNK_FRAMES: usize = 64;
const AUDIO_RING_FRAMES: usize = 1024;
const AUDIO_RING_MASK: usize = AUDIO_RING_FRAMES - 1;
const DRY_RING_FRAMES: usize = 10_240;
const DRY_DELAY_MAX: usize = DRY_RING_FRAMES - 128;
const RATE_TOLERANCE: f32 = f32::from_bits(0x3580_0000);
const QUIET_PEAK: f32 = f32::from_bits(0x38d1_b717);
const QUIET_SAMPLE: f32 = f32::from_bits(0x3580_0000);
const LOOKAHEAD_SECONDS: f32 = f32::from_bits(0x3b03_126f);
const GAIN_MIN: f32 = f32::from_bits(0x3dcc_cccd);
const GAIN_MAX: f32 = 10.0;
const CEILING_MAX: f32 = f32::from_bits(0x3fff_64ae);
const LOG_GAIN_MIN: f32 = f32::from_bits(0xc013_5d8e);
const LOG_GAIN_MAX: f32 = f32::from_bits(0x4013_5d8e);
const LOG_CEILING_MAX: f32 = f32::from_bits(0x3f30_d697);
const RELEASE_MIN: f32 = f32::from_bits(0x3a83_126f);
const RELEASE_MAX: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimiterError {
    /// A nonfinite, nonpositive, or too-small initial sample rate cannot
    /// initialize the native coefficient caches reproducibly.
    InvalidSampleRate,
    UnequalChannelLengths,
    ChunkTooLarge,
    /// An increased window would read weights not initialized by any reset.
    ResetRequired,
}

/// Physical model values used by the native DSP; gains and ceiling are linear,
/// and release is in seconds. The integer gates use their low bit, as native.
#[derive(Clone, Copy, Debug)]
pub struct LimiterParameters {
    pub enabled: i32,
    pub input_gain: f32,
    pub output_gain: f32,
    pub ceiling: f32,
    pub release_seconds: f32,
    pub host_enabled: i32,
    pub host_bypass: i32,
    pub dry_latency_frames: i32,
}

impl Default for LimiterParameters {
    fn default() -> Self {
        Self {
            enabled: 1,
            input_gain: 1.0,
            output_gain: 1.0,
            ceiling: 1.0,
            release_seconds: f32::from_bits(0x3cbc_6a7f),
            host_enabled: 1,
            host_bypass: 0,
            dry_latency_frames: 0,
        }
    }
}

/// The three values published by the DSP model. `attenuation` is the last
/// active block's reciprocal limiter factor, and is reset to one in sleep.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterMeters {
    pub detector: f32,
    pub held_peak: f32,
    pub attenuation: f32,
}

impl Default for LimiterMeters {
    fn default() -> Self {
        Self { detector: 0.0, held_peak: 0.0, attenuation: 1.0 }
    }
}

/// One native render-context projection. `silent` is the incoming context's
/// silence byte and is updated after processing. `bypass` is context+0x40;
/// it is distinct from the model's two host gate words.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimiterContext {
    pub silent: bool,
    pub bypass: bool,
}

/// Typed physical value returned by the host editor's normalized converter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LimiterValue {
    Enabled(i32),
    InputGain(f32),
    OutputGain(f32),
    Ceiling(f32),
    ReleaseSeconds(f32),
}

#[derive(Clone, Copy)]
struct Domain {
    minimum: f32,
    maximum: f32,
    log_minimum: f32,
    log_maximum: f32,
}

impl Domain {
    #[inline]
    fn decode(self, normalized: f32) -> f32 {
        let width = self.log_maximum - self.log_minimum;
        let value = math::expf(normalized.mul_add(width, self.log_minimum)) - 0.0;
        min_num(max_num(self.minimum, value), self.maximum)
    }

    #[inline]
    fn encode(self, value: f32) -> f32 {
        let value = min_num(max_num(self.minimum, value), self.maximum);
        let logarithm = math::logf(value + 0.0);
        (logarithm - self.log_minimum) / (self.log_maximum - self.log_minimum)
    }
}

/// Mutable DSP model. Notifications are actual native parameter IDs, not
/// audio events. A host may clear the queue and acknowledge individual dirty
/// flags independently, matching the two distinct native pieces of state.
pub struct LimiterModel {
    pub parameters: LimiterParameters,
    pub meters: LimiterMeters,
    pub modulation: [f32; 4],
    release_domain: Domain,
    meter_dirty: [bool; 3],
    notifications: Vec<u32>,
}

impl Default for LimiterModel {
    fn default() -> Self {
        Self::new()
    }
}

impl LimiterModel {
    pub fn new() -> Self {
        Self {
            parameters: LimiterParameters::default(),
            meters: LimiterMeters::default(),
            modulation: [0.0; 4],
            release_domain: Domain {
                minimum: RELEASE_MIN,
                maximum: RELEASE_MAX,
                log_minimum: math::logf(RELEASE_MIN),
                log_maximum: math::logf(RELEASE_MAX),
            },
            meter_dirty: [false; 3],
            notifications: Vec::with_capacity(3),
        }
    }

    #[inline]
    fn gain_domain(&self) -> Domain {
        Domain { minimum: GAIN_MIN, maximum: GAIN_MAX, log_minimum: LOG_GAIN_MIN, log_maximum: LOG_GAIN_MAX }
    }

    #[inline]
    fn ceiling_domain(&self) -> Domain {
        Domain { minimum: GAIN_MIN, maximum: CEILING_MAX, log_minimum: LOG_GAIN_MIN, log_maximum: LOG_CEILING_MAX }
    }

    /// Convert a VST/AU normalized parameter to the editor's physical value.
    /// This conversion occurs outside the host process-call FTZ scope.
    pub fn decode_editor_parameter(&self, index: u32, normalized: f32) -> Option<LimiterValue> {
        match index {
            0 => {
                let value = normalized.mul_add(1.0, 0.5) as i32;
                Some(LimiterValue::Enabled(value.clamp(0, 1)))
            }
            1 => Some(LimiterValue::InputGain(self.gain_domain().decode(normalized))),
            2 => Some(LimiterValue::OutputGain(self.gain_domain().decode(normalized))),
            3 => Some(LimiterValue::Ceiling(self.ceiling_domain().decode(normalized))),
            4 => Some(LimiterValue::ReleaseSeconds(self.release_domain.decode(normalized))),
            _ => None,
        }
    }

    /// Apply a delivered editor value through the DSP model's native
    /// physical→normalized→physical path. Call at the host event boundary,
    /// inside the same FTZ scope as processing. The modulation entries are the
    /// current model offsets, not normalized UI settings.
    pub fn apply_editor_value(&mut self, value: LimiterValue) {
        match value {
            LimiterValue::Enabled(value) => self.parameters.enabled = value.clamp(0, 1),
            LimiterValue::InputGain(value) => {
                self.parameters.input_gain = model_round_trip(self.gain_domain(), value, self.modulation[0]);
            }
            LimiterValue::OutputGain(value) => {
                self.parameters.output_gain = model_round_trip(self.gain_domain(), value, self.modulation[1]);
            }
            LimiterValue::Ceiling(value) => {
                self.parameters.ceiling = model_round_trip(self.ceiling_domain(), value, self.modulation[2]);
            }
            LimiterValue::ReleaseSeconds(value) => {
                self.parameters.release_seconds = model_round_trip(self.release_domain, value, self.modulation[3]);
            }
        }
    }

    pub fn pending_notifications(&self) -> &[u32] {
        &self.notifications
    }

    /// Clear the model's queued IDs while retaining storage. This does not
    /// clear the per-meter dirty bits; acknowledge them separately when the
    /// host's event consumer does so.
    pub fn clear_notification_queue(&mut self) {
        self.notifications.clear();
    }

    /// Clear one native meter dirty bit. IDs are 5, 312 and 313.
    pub fn acknowledge_meter(&mut self, id: u32) -> bool {
        let index = match id { 5 => 0, 312 => 1, 313 => 2, _ => return false };
        self.meter_dirty[index] = false;
        true
    }

    #[inline]
    fn notify_meter(&mut self, index: usize, id: u32) {
        if !self.meter_dirty[index] {
            self.meter_dirty[index] = true;
            self.notifications.push(id);
        }
    }
}

#[inline]
fn model_round_trip(domain: Domain, value: f32, modulation: f32) -> f32 {
    let normalized = domain.encode(value);
    let modulated = min_num(max_num(normalized + modulation, 0.0), 1.0);
    domain.decode(modulated)
}

// The original uses FMAXNM/FMINNM for these operations, including their NaN,
// signed-zero and current-FPCR behavior. The AArch64 target always has NEON.
#[inline]
fn max_num(a: f32, b: f32) -> f32 {
    // SAFETY: this crate is restricted to macOS AArch64, where NEON is baseline.
    unsafe { vget_lane_f32::<0>(vmaxnm_f32(vdup_n_f32(a), vdup_n_f32(b))) }
}

#[inline]
fn min_num(a: f32, b: f32) -> f32 {
    // SAFETY: this crate is restricted to macOS AArch64, where NEON is baseline.
    unsafe { vget_lane_f32::<0>(vminnm_f32(vdup_n_f32(a), vdup_n_f32(b))) }
}

#[inline]
fn linked_peak(left: f32, right: f32) -> f32 {
    // SAFETY: this crate is restricted to macOS AArch64, where NEON is baseline.
    unsafe {
        let left = vabs_f32(vdup_n_f32(left));
        let right = vabs_f32(vdup_n_f32(right));
        vget_lane_f32::<0>(vmaxnm_f32(left, right))
    }
}

#[derive(Clone, Copy)]
struct Ramp {
    counter: f32,
    step: f32,
    start: f32,
    target: f32,
    first: bool,
}

impl Ramp {
    fn new() -> Self {
        Self { counter: 0.0, step: 0.0, start: 0.0, target: 0.0, first: true }
    }

    #[inline]
    fn begin(&mut self, target: f32, frames: usize) {
        let previous = self.target;
        self.counter = 0.0;
        let delta = f64::from(target - previous);
        if self.first || (delta > -1.0e-15 && delta < 1.0e-15) {
            self.first = false;
            self.step = 0.0;
            self.start = target;
        } else {
            self.step = 1.0 / frames as f32;
            self.start = previous;
        }
        self.target = target;
    }

    #[inline]
    fn next(&mut self) -> f32 {
        let phase = self.counter.mul_add(self.step, self.step);
        self.counter += 1.0;
        phase.mul_add(self.target - self.start, self.start)
    }
}

pub(crate) struct DryDelay {
    samples: Box<[f32]>,
    position: usize,
    delay: usize,
    seen: i32,
    pending: i32,
}

impl DryDelay {
    pub(crate) fn new() -> Self {
        Self { samples: vec![0.0; DRY_RING_FRAMES].into_boxed_slice(), position: 0, delay: 0, seen: 0, pending: 0 }
    }

    pub(crate) fn configure(&mut self, delay: usize) {
        if self.delay != delay && delay > 0 {
            self.position = 0;
            self.seen = 0;
            self.pending = 0;
        }
        self.delay = delay;
    }

    pub(crate) fn reset_counters(&mut self) {
        self.seen = 0;
        self.pending = 0;
    }

    #[inline]
    fn advance_seen(&mut self, frames: usize) {
        if self.seen < 0x3fff_ffff {
            self.seen = self.seen.wrapping_add(frames as i32);
        }
    }

    pub(crate) fn remember(&mut self, input: &[f32]) {
        if self.delay == 0 {
            self.reset_counters();
            return;
        }
        for &sample in input {
            self.samples[self.position] = sample;
            self.position += 1;
            if self.position == DRY_RING_FRAMES { self.position = 0; }
        }
        self.pending = self.delay as i32;
        self.advance_seen(input.len());
    }

    pub(crate) fn process(&mut self, samples: &mut [f32]) {
        if self.delay == 0 {
            self.reset_counters();
            return;
        }
        // delay <= capacity-128 and the host supplies <=64 frames. Thus this
        // sample-wise form is equivalent to the native whole-block write then
        // read, including wrap, without overwriting unread history.
        let start_seen = self.seen;
        for (i, sample) in samples.iter_mut().enumerate() {
            let input = *sample;
            let read = (self.position + DRY_RING_FRAMES - self.delay) % DRY_RING_FRAMES;
            let output = if i64::from(start_seen) + i as i64 >= self.delay as i64 {
                self.samples[read]
            } else {
                0.0
            };
            self.samples[self.position] = input;
            self.position += 1;
            if self.position == DRY_RING_FRAMES { self.position = 0; }
            *sample = output;
        }
        self.pending = self.delay as i32;
        self.advance_seen(samples.len());
    }
}

/// Independent state for one native stereo `LimiterDsp` instance.
pub struct LimiterDsp {
    sample_rate: f32,
    detector_rate: f32,
    held_rate: f32,
    detector_attack: f32,
    detector_release: f32,
    held_attack: f32,
    held_release: f32,
    detector: f32,
    held: f32,
    hold: f32,
    window_len: usize,
    initialized_weights: usize,
    weights: [f32; AUDIO_RING_FRAMES],
    left_ring: [f32; AUDIO_RING_FRAMES],
    right_ring: [f32; AUDIO_RING_FRAMES],
    peaks: [f32; AUDIO_RING_FRAMES],
    position: usize,
    peak_state: f32,
    input_ramp: Ramp,
    output_ramp: Ramp,
    dry_left: DryDelay,
    dry_right: DryDelay,
    previous_active: bool,
    synchronize_active: bool,
    sleeping: bool,
    silence_frames: i32,
}

impl LimiterDsp {
    /// Create and reset one processor. The caller owns the model and must set
    /// `dry_latency_frames` from the controller's observed latency event.
    pub fn new(sample_rate: f32, model: &mut LimiterModel) -> Result<Self, LimiterError> {
        if !sample_rate.is_finite() || sample_rate <= RATE_TOLERANCE {
            return Err(LimiterError::InvalidSampleRate);
        }
        let mut dsp = Self {
            sample_rate: 1.0,
            detector_rate: 0.0,
            held_rate: 0.0,
            detector_attack: 0.0,
            detector_release: 0.0,
            held_attack: 0.0,
            held_release: 0.0,
            detector: 0.01,
            held: 0.01,
            hold: f32::MAX,
            window_len: 0,
            initialized_weights: 0,
            weights: [0.0; AUDIO_RING_FRAMES],
            left_ring: [0.0; AUDIO_RING_FRAMES],
            right_ring: [0.0; AUDIO_RING_FRAMES],
            peaks: [0.0; AUDIO_RING_FRAMES],
            position: 0,
            peak_state: 0.0,
            input_ramp: Ramp::new(),
            output_ramp: Ramp::new(),
            dry_left: DryDelay::new(),
            dry_right: DryDelay::new(),
            previous_active: true,
            synchronize_active: true,
            sleeping: false,
            silence_frames: i32::MAX,
        };
        dsp.set_sample_rate(sample_rate)?;
        dsp.reset(model);
        Ok(dsp)
    }

    /// Update rate-dependent coefficients and window length without resetting
    /// state, as native slot 8 does. Call `reset` when the host does. If a
    /// larger window has never had its weights initialized, `process_chunk`
    /// returns `ResetRequired` rather than fabricating native uninitialized
    /// memory.
    pub fn set_sample_rate(&mut self, sample_rate: f32) -> Result<(), LimiterError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(LimiterError::InvalidSampleRate);
        }
        self.sample_rate = sample_rate;
        if (self.detector_rate - sample_rate).abs() > RATE_TOLERANCE {
            self.detector_rate = sample_rate;
            self.detector_attack = 1.0;
            self.detector_release = 1.0 - math::exp2f(-1.0 / (0.01 * sample_rate));
        }
        if (self.held_rate - sample_rate).abs() > RATE_TOLERANCE {
            self.held_rate = sample_rate;
            self.held_attack = 1.0;
            self.held_release = 1.0 - math::exp2f(-1.0 / (0.05 * sample_rate));
        }
        self.window_len = lookahead_window(sample_rate);
        Ok(())
    }

    /// The controller's static `N−1` latency calculation for this rate.
    pub fn latency_frames(&self) -> usize {
        self.window_len - 1
    }

    /// Native slot 7 followed by slot 17. Dry ring contents and positions are
    /// retained while their counters reset, as in the reference.
    pub fn reset(&mut self, model: &mut LimiterModel) {
        self.sleeping = false;
        self.silence_frames = i32::MAX;
        self.previous_active = true;
        self.synchronize_active = true;
        self.dry_left.reset_counters();
        self.dry_right.reset_counters();
        self.reset_kernel(model);
    }

    fn reset_kernel(&mut self, model: &mut LimiterModel) {
        self.left_ring.fill(0.0);
        self.right_ring.fill(0.0);
        self.peaks.fill(0.0);
        let n = self.window_len;
        let inverse = 1.0f32 / n as f32;
        for i in 0..n {
            let x = f64::from((n - i) as f32 * inverse);
            let y = f64::from((n - i - 1) as f32 * inverse);
            let xx = x * x;
            let yy = y * y;
            let sx = xx * (3.0 - (x + x));
            let sy = yy * (3.0 - (y + y));
            self.weights[i] = ((sx - sy) / (1.0 - sy)) as f32;
        }
        self.initialized_weights = self.initialized_weights.max(n);
        self.position = 0;
        self.peak_state = 0.0;
        self.input_ramp.first = true;
        self.output_ramp.first = true;
        model.meters.detector = 0.0;
        model.notify_meter(0, 5);
        model.meters.held_peak = 0.0;
        model.notify_meter(1, 312);
        model.meters.attenuation = 1.0;
        model.notify_meter(2, 313);
        self.detector = 0.01;
        self.held = 0.01;
        self.hold = f32::MAX;
    }

    /// Process one native DSP chunk in-place. Both slices are stereo working
    /// buffers of equal length, 0..=64. The caller supplies the native context
    /// flags and has already applied eligible model events. A missing right
    /// input is handled by the host before this boundary, not here.
    pub fn process_chunk(
        &mut self,
        model: &mut LimiterModel,
        context: &mut LimiterContext,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), LimiterError> {
        if left.len() != right.len() { return Err(LimiterError::UnequalChannelLengths); }
        if left.len() > MAX_CHUNK_FRAMES { return Err(LimiterError::ChunkTooLarge); }
        if self.window_len > self.initialized_weights { return Err(LimiterError::ResetRequired); }
        let delay = (model.parameters.dry_latency_frames.max(0) as usize).min(DRY_DELAY_MAX);
        self.dry_left.configure(delay);
        self.dry_right.configure(delay);
        let active = !context.bypass
            && model.parameters.enabled & 1 != 0
            && model.parameters.host_enabled & 1 != 0
            && model.parameters.host_bypass & 1 == 0;
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
                self.reset(model);
            }
            self.dry_left.remember(left);
            self.dry_right.remember(right);
            self.process_active_or_sleep(model, context, left, right);
            if !active {
                self.reset(model);
            }
            self.previous_active = active;
        }
        if context.silent {
            context.silent = left.iter().zip(right.iter()).all(|(&l, &r)| {
                l > -QUIET_SAMPLE && l < QUIET_SAMPLE && r > -QUIET_SAMPLE && r < QUIET_SAMPLE
            });
        }
        if !context.silent {
            self.silence_frames = 0;
        } else if !active {
            self.sleeping = true;
        }
        Ok(())
    }

    fn process_active_or_sleep(
        &mut self,
        model: &mut LimiterModel,
        context: &LimiterContext,
        left: &mut [f32],
        right: &mut [f32],
    ) {
        // ARM FCMP + CSET LE includes unordered; `!(q > threshold)` matches it.
        let may_sleep = context.silent && !(self.peak_state > QUIET_PEAK);
        if !may_sleep {
            self.silence_frames = 0;
            if self.sleeping {
                self.input_ramp.first = true;
                self.output_ramp.first = true;
            }
            self.sleeping = false;
        } else if self.sleeping {
            self.sleep(model, left.len());
            return;
        } else if self.silence_frames > model.parameters.dry_latency_frames.wrapping_add(64) {
            self.sleeping = true;
            self.sleep(model, left.len());
            return;
        } else {
            self.silence_frames = self.silence_frames.wrapping_add(left.len() as i32);
        }
        self.process_active(model, left, right);
    }

    fn sleep(&mut self, model: &mut LimiterModel, frames: usize) {
        self.hold += frames as f32 / self.sample_rate;
        if self.hold > 0.5 {
            let factor = math::powi_f64(f64::from(1.0 - self.held_release), frames as i32) as f32;
            self.held *= factor;
            model.meters.held_peak = self.held;
            model.notify_meter(1, 312);
        }
        let factor = math::powi_f64(f64::from(1.0 - self.detector_release), frames as i32) as f32;
        self.detector *= factor;
        model.meters.detector = self.detector;
        model.notify_meter(0, 5);
        model.meters.attenuation = 1.0;
        model.notify_meter(2, 313);
    }

    fn process_active(&mut self, model: &mut LimiterModel, left: &mut [f32], right: &mut [f32]) {
        let parameters = model.parameters;
        let input_target = parameters.input_gain / parameters.ceiling;
        let output_target = parameters.ceiling * parameters.output_gain;
        let decay = 1.0 - math::exp2f(-1.0 / (self.sample_rate * parameters.release_seconds));
        self.input_ramp.begin(input_target, left.len());
        self.output_ramp.begin(output_target, left.len());
        let reciprocal_rate = 1.0 / self.sample_rate;
        let mut last_attenuation = 1.0;
        for (left, right) in left.iter_mut().zip(right.iter_mut()) {
            let input_gain = self.input_ramp.next();
            let output_gain = self.output_ramp.next();
            let l = *left * input_gain;
            let r = *right * input_gain;
            self.left_ring[self.position] = l;
            self.right_ring[self.position] = r;
            let peak = linked_peak(l, r);
            self.peaks[self.position] = peak - 1.0;
            self.position = (self.position + 1) & AUDIO_RING_MASK;

            let measured = peak * parameters.ceiling;
            let coefficient = if !(self.detector >= measured) { self.detector_attack } else { self.detector_release };
            self.detector = coefficient.mul_add(measured - self.detector, self.detector);
            let target = if self.detector > model.meters.held_peak {
                self.hold = 0.0;
                self.detector
            } else if !(self.hold >= 0.5) {
                model.meters.held_peak
            } else {
                self.detector
            };
            self.hold += reciprocal_rate;
            let coefficient = if !(self.held >= target) { self.held_attack } else { self.held_release };
            self.held = coefficient.mul_add(target - self.held, self.held);
            model.meters.held_peak = self.held;
            model.notify_meter(1, 312);
            model.meters.detector = self.detector;
            model.notify_meter(0, 5);

            let first = self.position.wrapping_sub(self.window_len) & AUDIO_RING_MASK;
            let (weighted, raw) = self.history_maxima(first);
            last_attenuation = 1.0 / (weighted + 1.0);
            let lower = if raw > weighted { weighted } else { raw };
            self.peak_state = (lower - weighted).mul_add(decay, weighted);
            *left = last_attenuation * (self.left_ring[first] * output_gain);
            *right = last_attenuation * (self.right_ring[first] * output_gain);
        }
        if !left.is_empty() {
            model.meters.attenuation = last_attenuation;
            model.notify_meter(2, 313);
        }
    }

    #[inline]
    fn history_maxima(&self, first: usize) -> (f32, f32) {
        // Native processes four independent FMAX lanes and reduces them in
        // pairs, preserving the original NaN and FPCR behavior.
        // SAFETY: NEON is baseline on the target; `window_len` is a multiple
        // of four at most 1024, so each four-weight load is in bounds.
        unsafe {
            let base = vdupq_n_f32(self.peak_state);
            let mut weighted = base;
            let mut raw = vdupq_n_f32(0.0);
            for i in (0..self.window_len).step_by(4) {
                let mut history = vdupq_n_f32(self.peaks[(first + i) & AUDIO_RING_MASK]);
                history = vsetq_lane_f32::<1>(self.peaks[(first + i + 1) & AUDIO_RING_MASK], history);
                history = vsetq_lane_f32::<2>(self.peaks[(first + i + 2) & AUDIO_RING_MASK], history);
                history = vsetq_lane_f32::<3>(self.peaks[(first + i + 3) & AUDIO_RING_MASK], history);
                let weights = vld1q_f32(self.weights.as_ptr().add(i));
                raw = vmaxq_f32(raw, history);
                let candidate = vfmaq_f32(base, vsubq_f32(history, base), weights);
                weighted = vmaxq_f32(weighted, candidate);
            }
            let w01 = vmaxq_f32(vdupq_laneq_f32::<0>(weighted), vdupq_laneq_f32::<1>(weighted));
            let w23 = vmaxq_f32(vdupq_laneq_f32::<2>(weighted), vdupq_laneq_f32::<3>(weighted));
            let r01 = vmaxq_f32(vdupq_laneq_f32::<0>(raw), vdupq_laneq_f32::<1>(raw));
            let r23 = vmaxq_f32(vdupq_laneq_f32::<2>(raw), vdupq_laneq_f32::<3>(raw));
            (
                vgetq_lane_f32::<0>(vmaxq_f32(w01, w23)),
                vgetq_lane_f32::<0>(vmaxq_f32(r01, r23)),
            )
        }
    }
}

#[inline]
fn lookahead_window(sample_rate: f32) -> usize {
    let n = sample_rate.mul_add(LOOKAHEAD_SECONDS, 0.5) as i32;
    let n = n.clamp(1, AUDIO_RING_FRAMES as i32) as usize;
    n + (n.wrapping_neg() & 3)
}
