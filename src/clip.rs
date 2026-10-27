//! One-channel scalar translation of the ARM64 ClipRepairUnit kernel.
//!
//! The input and output are planar float32 samples. Each instance owns one
//! channel's 4096-sample ring, delay, detector, and event history; it captures
//! the supplied parameters at construction. Instruction anchors are recorded
//! inline; public verification scope is documented in `docs/VERIFICATION.md`.

use crate::math;
use std::fmt;

const RING_SIZE: usize = 4096;
const RING_MASK: u32 = (RING_SIZE - 1) as u32;
const DELAY: i32 = 2560;

/// Initial Audio Unit parameter values, captured once for this channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipParameters {
    pub headroom_db: f32,
    pub threshold_percent: f32,
}

/// Invalid configuration or buffer shape; an error leaves the stream unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipError {
    InvalidSampleRate,
    InvalidHeadroom,
    InvalidThreshold,
    BufferLengthMismatch { input: usize, output: usize },
    BlockTooLarge { frames: usize },
}

impl fmt::Display for ClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => write!(f, "sample rate must be finite and positive"),
            Self::InvalidHeadroom => write!(f, "headroom must be finite"),
            Self::InvalidThreshold => write!(f, "threshold must be finite"),
            Self::BufferLengthMismatch { input, output } => {
                write!(f, "input has {input} frames but output has {output}")
            }
            Self::BlockTooLarge { frames } => {
                write!(f, "block of {frames} frames exceeds the native signed frame count")
            }
        }
    }
}

impl std::error::Error for ClipError {}

#[derive(Clone, Copy)]
struct Rails {
    low: f32,
    high: f32,
    loose_low: f32,
    loose_high: f32,
    width: f32,
    at_13: f64,
    at_43: f64,
    at_57: f64,
    at_87: f64,
}

impl Rails {
    fn new(threshold_percent: f32) -> Self {
        // 0x3ebc-0x3fe8; these constants are the original binary64 literals.
        let threshold = threshold_percent / 100.0_f32;
        let cap = 90.0_f32 / 100.0_f32;
        let loose = if cap > threshold { threshold } else { cap };
        let gap = ((1.0_f64 - f64::from(threshold)) * 0.9999847412109375_f64) as f32;
        let loose_gap = ((1.0_f64 - f64::from(loose)) * 0.9999847412109375_f64) as f32;
        let low = gap + -0.999969482421875_f32;
        let high = 0.99993896484375_f32 - gap;
        let loose_low = loose_gap + -0.999969482421875_f32;
        let loose_high = 0.99993896484375_f32 - loose_gap;
        let width = high - low;
        let w = f64::from(width);
        let lo = f64::from(low);
        let at_13 = w * 0.12999999523162842_f64 + lo;
        let at_43 = w * 0.4299999997019768_f64 + lo;
        let at_57 = w * 0.5700000002980232_f64 + lo;
        let at_87 = w * 0.8700000047683716_f64 + lo;
        Self { low, high, loose_low, loose_high, width, at_13, at_43, at_57, at_87 }
    }
}

/// Native per-channel kernel state. Construct a new instance for a new stream.
///
/// The event-time view is sorted like the Audio Unit property, including after
/// the signed frame counter wraps. The native kernel's private vector itself
/// appends in encounter order. To expose a borrowed property view without
/// allocating during a query, this implementation keeps the view sorted as
/// events are recorded. Callers aggregating channels must sort the combined
/// records and retain duplicates. `event_count` is the native wrapping scalar,
/// which can differ from the history length after 2^32 events.
pub struct ClipRepair {
    ring: [f32; RING_SIZE],
    write_index: u32,
    holdoff: i32,
    frame_counter: u32,
    event_count: u32,
    event_times: Vec<f64>,
    sample_rate: f64,
    parameters: ClipParameters,
}

impl ClipRepair {
    /// Capture parameters and initialize a zero-filled ring and empty history.
    ///
    /// The reference advertises headroom 0..=10 and threshold 95..=100, but
    /// template import can supply float values directly. Finite values outside
    /// those advertised ranges are accepted rather than silently clamped;
    /// their native behavior has not been measured.
    pub fn new(sample_rate: f64, parameters: ClipParameters) -> Result<Self, ClipError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(ClipError::InvalidSampleRate);
        }
        if !parameters.headroom_db.is_finite() {
            return Err(ClipError::InvalidHeadroom);
        }
        if !parameters.threshold_percent.is_finite() {
            return Err(ClipError::InvalidThreshold);
        }
        Ok(Self {
            ring: [0.0; RING_SIZE],
            write_index: u32::MAX,
            holdoff: 0,
            frame_counter: 0,
            event_count: 0,
            event_times: Vec::new(),
            sample_rate,
            parameters,
        })
    }

    pub const fn latency_frames(&self) -> usize {
        DELAY as usize
    }

    pub const fn event_count(&self) -> u32 {
        self.event_count
    }

    pub fn event_times(&self) -> &[f64] {
        &self.event_times
    }

    /// Apply the wrapper's seconds × sample-rate, truncating unsigned conversion.
    /// This can be one sample below the signed event numerator after round-trip.
    pub fn event_positions(&self) -> Vec<u64> {
        self.event_times.iter().map(|&time| (self.sample_rate * time) as u64).collect()
    }

    /// Process one planar channel, retaining all state across blocks.
    ///
    /// Equal empty slices are a no-op, as is the native kernel's nonpositive
    /// frame count. The native per-channel reset hook at 0x3328 is itself a
    /// no-op; the outer wrapper also resets separate bookkeeping. This API
    /// exposes neither that outer reset nor the separate native bypass setter.
    /// Construct a new instance to start an independent stream.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ClipError> {
        if input.len() != output.len() {
            return Err(ClipError::BufferLengthMismatch { input: input.len(), output: output.len() });
        }
        if input.len() > i32::MAX as usize {
            return Err(ClipError::BlockTooLarge { frames: input.len() });
        }
        if input.is_empty() {
            return Ok(());
        }

        // 0x3e9c-0x3eb8: both libSystem calls occur once per nonempty block.
        let negative_headroom = -f64::from(self.parameters.headroom_db);
        let gain = math::exp10(negative_headroom / 20.0_f64) as f32;
        let inverse = math::exp10(negative_headroom / -20.0_f64) as f32;
        if gain >= 1.0 || gain.is_nan() {
            // 0x3ef0: bit-copy delay, including signed zeros and NaN payloads.
            for (&x, out) in input.iter().zip(output.iter_mut()) {
                self.write_index = self.write_index.wrapping_add(1);
                self.ring[(self.write_index & RING_MASK) as usize] = x;
                *out = self.at(-DELAY);
            }
        } else {
            let rails = Rails::new(self.parameters.threshold_percent);
            for (i, (&x, out)) in input.iter().zip(output.iter_mut()).enumerate() {
                self.write_index = self.write_index.wrapping_add(1);
                self.ring[(self.write_index & RING_MASK) as usize] = x;
                // The current output is loaded before the interval is rewritten.
                let delayed = self.at(-DELAY);
                if self.holdoff <= 0 {
                    let next = self.at(-DELAY + 1);
                    let sign = if delayed < rails.high && next >= rails.high {
                        1
                    } else if delayed > rails.low && next <= rails.low {
                        -1
                    } else {
                        0
                    };
                    if sign != 0 {
                        if let Some(end) = self.find_end(sign, rails) {
                            self.record_event(i as u32, end);
                            self.holdoff = end + DELAY;
                            self.repair(end, sign, rails, inverse);
                        }
                    }
                }
                if self.holdoff > 0 {
                    self.holdoff -= 1;
                }
                let scaled = delayed * gain;
                *out = scaled + 0.0_f32;
            }
        }
        self.frame_counter = self.frame_counter.wrapping_add(input.len() as u32);
        Ok(())
    }

    #[inline]
    fn at(&self, offset: i32) -> f32 {
        let index = self.write_index.wrapping_add(offset as u32) & RING_MASK;
        self.ring[index as usize]
    }

    #[inline]
    fn set(&mut self, offset: i32, value: f32) {
        let index = self.write_index.wrapping_add(offset as u32) & RING_MASK;
        self.ring[index as usize] = value;
    }

    fn find_end(&self, sign: i32, rails: Rails) -> Option<i32> {
        // 0x4054-0x4138: stop at the first loose-rail return, even if too early.
        for j in 0..1023_i32 {
            let value = self.at(-2558 + j);
            let returned = if sign > 0 { value < rails.loose_high } else { value > rails.loose_low };
            if returned {
                if j < 2 {
                    return None;
                }
                let mut end = -2558 + j;
                while end >= -2555 {
                    let previous = self.at(end - 1);
                    let backtrack = if sign > 0 { previous < rails.high } else { previous > rails.low };
                    if !backtrack {
                        break;
                    }
                    end -= 1;
                }
                return Some(end.max(-2556));
            }
        }
        None
    }

    fn record_event(&mut self, block_index: u32, end: i32) {
        // W-register additions wrap; SDIV-by-two sequence truncates toward zero.
        let delta = (end - DELAY) / 2;
        let frame = self.frame_counter.wrapping_add(block_index).wrapping_add(delta as u32) as i32;
        self.event_count = self.event_count.wrapping_add(1);
        let time = f64::from(frame) / self.sample_rate;
        if self.event_times.last().is_some_and(|&last| time < last) {
            let index = self.event_times.partition_point(|&previous| previous <= time);
            self.event_times.insert(index, time);
        } else {
            self.event_times.push(time);
        }
    }

    fn mean(&self, start: i32, width: i32) -> f32 {
        let mut sum = 0.0_f32;
        for offset in start..start + width {
            sum += self.at(offset);
        }
        sum / width as f32
    }

    fn curvature(&self, start: i32) -> f32 {
        let middle = f64::from(self.at(start + 1));
        let doubled = middle + middle;
        let first = doubled - f64::from(self.at(start));
        (first - f64::from(self.at(start + 2))) as f32
    }

    fn smoothness(&self, start: i32, count: i32, width: f32) -> f32 {
        if count < 3 {
            return 0.0;
        }
        let mut previous = self.curvature(start);
        if count == 3 {
            return previous.abs() / width;
        }
        let mut sum = 0.0_f32;
        for j in 1..count - 2 {
            let current = self.curvature(start + j);
            let difference = (previous - current).abs();
            sum += difference;
            previous = current;
        }
        let average = sum / (count - 3) as f32;
        average / width
    }

    fn window(&self, end: i32, sign: i32, rails: Rails, leading: bool) -> i32 {
        let n = end + 2559;
        let initial = (n / 10).max(1);
        let limit = (12 * n) / 16;
        let half = n / 2;
        let mut width = initial;
        while width < limit {
            let far_start = if leading { -2559 - 3 * width } else { end + 2 * width };
            let far = f64::from(self.mean(far_start, width));
            let half_stop = if sign > 0 { rails.at_57 >= far } else { rails.at_43 <= far };
            if width >= half && half_stop {
                break;
            }
            // ARM PL/GT conditions deliberately admit unordered comparisons here.
            let eligible = if sign > 0 { !(rails.at_87 < far) } else { !(rails.at_13 > far) };
            if eligible {
                let start = if leading { -2559 - 3 * width } else { end };
                let smooth = self.smoothness(start, 3 * width, rails.width);
                if !(smooth > 0.014000000432133675_f32) {
                    let (a, b) = if leading {
                        (self.mean(-2559 - 2 * width, width), self.mean(-2559 - width, width))
                    } else {
                        (self.mean(end, width), self.mean(end + width, width))
                    };
                    let delta = b - a;
                    let gradient = delta / width as f32;
                    let directed = gradient * sign as f32;
                    let stop = if leading { !(directed < 0.0) } else { !(directed > 0.0) };
                    if stop {
                        break;
                    }
                }
            }
            width += 1;
        }
        width.max(1)
    }

    fn slopes(&self, end: i32, sign: i32, rails: Rails) -> (f32, f32) {
        let a = self.window(end, sign, rails, true);
        let b = self.window(end, sign, rails, false);
        let left_a = self.mean(-2559 - 3 * a, a);
        let left_b = self.mean(-2559 - 2 * a, a);
        let left_c = self.mean(-2559 - a, a);
        let right_d = self.mean(end, b);
        let right_e = self.mean(end + b, b);
        let right_f = self.mean(end + 2 * b, b);
        let p = (left_b - left_a) / a as f32;
        let q = (left_c - left_b) / a as f32;
        let u = (right_e - right_d) / b as f32;
        let v = (right_f - right_e) / b as f32;
        let directed_p = p * sign as f32;
        let directed_q = q * sign as f32;
        let left_ref = if directed_p < directed_q { q } else { p };
        let directed_u = u * sign as f32;
        let directed_v = v * sign as f32;
        let right_ref = if directed_u < directed_v { u } else { v };
        let left_delta = q - left_ref;
        let right_delta = u - right_ref;
        let left_ratio = (f64::from(a) + -0.5_f64) / f64::from(a);
        let right_ratio = (f64::from(b) + -0.5_f64) / f64::from(b);
        let left = (left_ratio * f64::from(left_delta) + f64::from(q)) as f32;
        let right = (right_ratio * f64::from(right_delta) + f64::from(u)) as f32;
        constrain_slopes(left, right, sign)
    }

    fn repair(&mut self, end: i32, sign: i32, rails: Rails, inverse: f32) {
        let y0 = self.at(-2560);
        let y1 = self.at(end);
        let (left, right) = self.slopes(end, sign, rails);
        let rail = if sign > 0 { rails.high } else { rails.low };
        let cubic = Cubic::constrained(end, y0, y1, left, right, rail, sign);
        let mut maximum = 0.0_f32;
        let mut minimum = 0.0_f32;
        for k in -2559..end {
            let value = cubic.at(k);
            if value > maximum { maximum = value; }
            if value < minimum { minimum = value; }
            self.set(k, value);
        }
        self.shape(end, sign, rail, inverse, minimum, maximum);
    }

    fn shape(&mut self, end: i32, sign: i32, rail: f32, inverse: f32, minimum: f32, maximum: f32) {
        // 0x4a1c-0x4c40: both polarities share the two shaping equations.
        let target = rail * inverse + 0.0_f32;
        let magnitude;
        let cap;
        if sign > 0 {
            if !(maximum > target) { return; }
            magnitude = maximum - rail;
            if magnitude <= 0.0 { return; }
            cap = target - rail;
        } else {
            if !(minimum < target) { return; }
            magnitude = rail - minimum;
            if magnitude <= 0.0 { return; }
            cap = rail - target;
        }
        let square = magnitude * magnitude;
        let difference = magnitude - cap;
        let doubled = f64::from(magnitude) + f64::from(magnitude);
        let mode = doubled * f64::from(difference);
        let saturated = mode > f64::from(square);
        let factor = if saturated {
            let denominator = f64::from(cap) * 4.0_f64;
            (-1.0_f64 / denominator) as f32
        } else {
            (cap - magnitude) / square
        };
        let twice_cap = f64::from(cap) + f64::from(cap);
        for k in -2559..end {
            let sample = self.at(k);
            let distance = if sign > 0 { sample - rail } else { rail - sample };
            let shaped = if saturated && twice_cap < f64::from(distance) {
                cap
            } else {
                let product = if saturated { distance * factor } else { factor * distance };
                let shifted = f64::from(product) + 1.0_f64;
                (shifted * f64::from(distance)) as f32
            };
            let value = if sign > 0 { rail + shaped } else { rail - shaped };
            self.set(k, value);
        }
    }
}

fn constrain_slopes(mut left: f32, mut right: f32, sign: i32) -> (f32, f32) {
    // 0x4768-0x4868, including the ARM unordered condition selections.
    let directed_left = sign as f32 * left;
    let directed_right = sign as f32 * right;
    let left_le = !(directed_left > 0.0);
    let left_gt = directed_left > 0.0;
    let right_mi = directed_right < 0.0;
    let right_pl = !(directed_right < 0.0);
    if !left_gt && !right_pl {
        left = (f64::from(-right) * 0.4_f64) as f32;
    }
    if !left_le && !right_mi {
        right = (f64::from(-left) * 0.4_f64) as f32;
    }
    if !left_gt && !right_mi {
        left = 0.0;
        right = 0.0;
    }
    if sign > 0 {
        let lower = right * -0.2_f32;
        if left < lower { left = lower; }
        let positive = left * 0.2_f32;
        if positive > -right { right = left * -0.2_f32; }
    } else {
        let positive = right * 0.2_f32;
        if positive > -left { left = right * -0.2_f32; }
        let lower = left * -0.2_f32;
        if right < lower { right = lower; }
    }
    let left = if left * sign as f32 > 0.0 { left } else { 0.0 };
    let right = if right * (sign as f32) < 0.0 { right } else { 0.0 };
    (left, right)
}

#[derive(Clone, Copy)]
struct Cubic {
    start: i32,
    end: i32,
    y0: f32,
    a: f32,
    b: f32,
    c: f32,
}

impl Cubic {
    // The first reanchoring adds the L*S term before 2*y0, unlike the other two paths.
    fn coefficients(y0: f32, y1: f32, left: f32, sum: f32, length: f32, first_reanchor: bool, last_reanchor: bool) -> (f32, f32, f32) {
        let b = if last_reanchor { left * length } else { length * left };
        let term = if last_reanchor { sum * length } else { length * sum };
        let twice_y0 = y0 + y0;
        let twice_y1 = y1 + y1;
        let total = if first_reanchor { term + twice_y0 } else { twice_y0 + term };
        let a = total - twice_y1;
        let c = ((y1 - a) - b) - y0;
        (a, b, c)
    }

    fn evaluate(y0: f32, a: f32, b: f32, c: f32, t: f32, first_test: bool) -> f32 {
        let mut at = if first_test { t * a } else { a * t };
        at = t * at;
        at = t * at;
        let mut ct = if first_test { t * c } else { c * t };
        ct = t * ct;
        let ct_at = ct + at;
        let bt = if first_test { t * b } else { b * t };
        let tail = bt + ct_at;
        y0 + tail
    }

    fn constrained(end: i32, mut y0: f32, mut y1: f32, left: f32, right: f32, rail: f32, sign: i32) -> Self {
        let mut start = -2560;
        let mut last = end;
        let sum = right + left;
        let length = end as f32 + 2560.0_f32;
        let (mut a, mut b, mut c) = Self::coefficients(y0, y1, left, sum, length, false, false);
        let first_t = 1.0_f32 / length;
        let first = Self::evaluate(y0, a, b, c, first_t, true);
        let directed_rail = rail * sign as f32;
        if first * (sign as f32) < directed_rail {
            let adjustment = f64::from(left) * 0.5_f64;
            y0 = (adjustment + f64::from(rail)) as f32;
            start = -2559;
            let length = end as f32 + 2559.0_f32;
            (a, b, c) = Self::coefficients(y0, y1, left, sum, length, true, false);
        }
        let numerator = (end - 1) as f32 - start as f32;
        let denominator = end as f32 - start as f32;
        let t = numerator / denominator;
        let penultimate = Self::evaluate(y0, a, b, c, t, false);
        if penultimate * (sign as f32) < directed_rail {
            let adjustment = f64::from(right) * -0.5_f64;
            y1 = (f64::from(rail) + adjustment) as f32;
            last = end - 1;
            let length = last as f32 - start as f32;
            (a, b, c) = Self::coefficients(y0, y1, left, sum, length, false, true);
        }
        Self { start, end: last, y0, a, b, c }
    }

    fn at(self, index: i32) -> f32 {
        let numerator = index as f32 - self.start as f32;
        let denominator = self.end as f32 - self.start as f32;
        let t = numerator / denominator;
        Self::evaluate(self.y0, self.a, self.b, self.c, t, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel() -> ClipRepair {
        ClipRepair::new(48_000.0, ClipParameters { headroom_db: 1.5, threshold_percent: 99.7 }).unwrap()
    }

    #[test]
    fn default_rail_bits_match_the_static_literal_recovery() {
        let rails = Rails::new(99.7);
        assert_eq!(rails.low.to_bits(), 0xbf7f_3965);
        assert_eq!(rails.high.to_bits(), 0x3f7f_3765);
        assert_eq!(rails.loose_low.to_bits(), 0xbf66_6480);
        assert_eq!(rails.loose_high.to_bits(), 0x3f66_6280);
    }

    #[test]
    fn smoothness_uses_double_curvature_and_float_accumulation() {
        let mut clip = kernel();
        for (offset, value) in [0.0, 1.0, 0.0, 1.0].into_iter().enumerate() {
            clip.set(offset as i32, value);
        }
        assert_eq!(clip.smoothness(0, 2, 1.0).to_bits(), 0.0_f32.to_bits());
        assert_eq!(clip.smoothness(0, 3, 1.0), 2.0);
        assert_eq!(clip.smoothness(0, 4, 1.0), 4.0);
        for offset in 0..3 {
            clip.set(offset, f32::MAX);
        }
        assert_eq!(clip.smoothness(0, 3, 1.0).to_bits(), 0.0_f32.to_bits());
    }

    #[test]
    fn window_search_exercises_smoothness_limit_and_half_guard() {
        let rails = Rails::new(99.7);
        let end = -2539; // N=20: initial=2, limit=15, half=10.
        for sign in [1, -1] {
            let mut clip = kernel();
            assert_eq!(clip.window(end, sign, rails, true), 2);
            assert_eq!(clip.window(end, sign, rails, false), 2);
            clip.ring.fill(sign as f32);
            assert_eq!(clip.window(end, sign, rails, true), 15);
            assert_eq!(clip.window(end, sign, rails, false), 15);
            for k in -2589..-2579 {
                clip.set(k, -(sign as f32));
            }
            for k in end + 20..end + 30 {
                clip.set(k, -(sign as f32));
            }
            assert_eq!(clip.window(end, sign, rails, true), 10);
            assert_eq!(clip.window(end, sign, rails, false), 10);
        }
    }

    #[test]
    fn slope_sign_repair_and_ratio_clamps_cover_both_polarities() {
        let vectors = [
            ((-1.0, -2.0), (0.8_f32, -2.0_f32)),
            ((2.0, 1.0), (2.0, -0.8)),
            ((-1.0, 1.0), (0.0, 0.0)),
            ((0.1, -2.0), (0.4, -2.0)),
            ((2.0, -0.1), (2.0, -0.4)),
        ];
        for ((left, right), (expected_left, expected_right)) in vectors {
            let positive = constrain_slopes(left, right, 1);
            let negative = constrain_slopes(-left, -right, -1);
            assert_eq!(positive, (expected_left, expected_right));
            assert_eq!(negative, (-expected_left, -expected_right));
        }
    }

    #[test]
    fn endpoint_constraints_cover_all_four_branches() {
        for sign in [1, -1] {
            let s = sign as f32;
            for (left, right, expected_start, expected_end) in [
                (0.0, 0.0, -2559, -2557),
                (0.5, -2.0, -2559, -2556),
                (2.0, -0.5, -2560, -2557),
                (2.0, -2.0, -2560, -2556),
            ] {
                let cubic = Cubic::constrained(-2556, 0.0, 0.0, left * s, right * s, s, sign);
                assert_eq!((cubic.start, cubic.end), (expected_start, expected_end));
                if left == 0.0 && right == 0.0 {
                    for k in -2559..-2556 {
                        assert_eq!(cubic.at(k).to_bits(), s.to_bits());
                    }
                }
            }
        }
    }

    #[test]
    fn peak_shaping_covers_quadratic_equality_and_saturation() {
        for sign in [1, -1] {
            let s = sign as f32;
            for (values, expected) in [
                (vec![0.5_f32, 0.625, 0.875], vec![0x3f00_0000_u32, 0x3f1c_71c7, 0x3f40_0000]),
                (vec![0.5, 0.75, 1.0], vec![0x3f00_0000, 0x3f30_0000, 0x3f40_0000]),
                (vec![0.5, 0.75, 1.0, 1.125], vec![0x3f00_0000, 0x3f30_0000, 0x3f40_0000, 0x3f40_0000]),
            ] {
                let mut clip = kernel();
                for (i, &value) in values.iter().enumerate() {
                    clip.set(-2559 + i as i32, value * s);
                }
                let extreme = values.last().copied().unwrap() * s;
                let (minimum, maximum) = if sign > 0 { (0.0, extreme) } else { (extreme, 0.0) };
                clip.shape(-2559 + values.len() as i32, sign, 0.5 * s, 1.5, minimum, maximum);
                for (i, expected_bits) in expected.into_iter().enumerate() {
                    let expected_bits = if sign > 0 { expected_bits } else { expected_bits ^ 0x8000_0000 };
                    assert_eq!(clip.at(-2559 + i as i32).to_bits(), expected_bits);
                }
            }
        }
    }

    #[test]
    fn event_and_ring_counters_wrap_like_arm_w_registers() {
        let mut clip = kernel();
        clip.frame_counter = 0x8000_09fd;
        clip.event_count = u32::MAX;
        clip.record_event(0, -2556);
        assert_eq!(clip.event_count(), 0);
        clip.frame_counter = 0x8000_09fe;
        clip.record_event(0, -2556);
        assert_eq!(clip.event_count(), 1);
        assert_eq!(clip.event_times(), &[-2147483648.0 / 48_000.0, 2147483647.0 / 48_000.0]);
        assert_eq!(clip.event_positions(), vec![0, 2147483647]);

        let mut bypass = ClipRepair::new(48_000.0, ClipParameters { headroom_db: 0.0, threshold_percent: 99.7 }).unwrap();
        bypass.write_index = 0xffff_fffe;
        bypass.ring[1535..1538].copy_from_slice(&[0.25, -0.5, 0.75]);
        let mut output = [0.0; 3];
        bypass.process(&[1.0, 2.0, 3.0], &mut output).unwrap();
        assert_eq!(output, [0.25, -0.5, 0.75]);
        assert_eq!(bypass.write_index, 1);
        assert_eq!(bypass.ring[4095], 1.0);
        assert_eq!(bypass.ring[0..2], [2.0, 3.0]);
    }
}
