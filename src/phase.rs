//! KiloheartsUnit's reverse-window phase correction, not HeartCore DSP.
//!
//! Parameters supplied here are the effective first four host values: resolve
//! the native negative-cache fallback through the host getter before using
//! them. Call `prepare` before a phase pass (and before querying its latency)
//! after host events, with the current f32-stored rate and channel count.
//! The host still owns activation, hosted processing, and end-of-stream policy.

use crate::math;

/// Number of frames retained between successive reverse windows.
pub const OVERLAP: usize = 4_096;
/// Number of newly consumed frames per complete subsequent window.
pub const HOP: usize = 16_384;
/// Window size and additional phase latency when at least one section exists.
pub const WINDOW: usize = HOP + OVERLAP;

const LOG_200: f64 = f64::from_bits(0x4015_317a_1b94_9c53);
// 0x14a9c uses the literal one ULP below the usual rounded sqrt(2).
const WRAPPER_SQRT_2: f64 = f64::from_bits(0x3ff6_a09e_667f_3bcc);

unsafe extern "C" {
    #[link_name = "exp"]
    fn system_exp(value: f64) -> f64;
}

#[derive(Clone, Copy, Default)]
struct Allpass {
    a1: f64,
    a2: f64,
}

impl Allpass {
    fn new(frequency: f64, sample_rate: f32) -> Self {
        // 0x14c54..0x14c88: FMADD and FMSUB are fused, while the
        // surrounding multiplies/adds/divides are separate ARM64 operations.
        let angle = (frequency * std::f64::consts::PI) / f64::from(sample_rate);
        let k = 1.0 / math::tan(angle);
        let k2 = k * k;
        let denominator = k.mul_add(WRAPPER_SQRT_2, 1.0) + k2;
        let scale = 1.0 / denominator;
        let a2 = ((-k).mul_add(WRAPPER_SQRT_2, 1.0) + k2) * scale;
        let a1 = (1.0 - k2) * (scale + scale);
        Self { a1, a2 }
    }

    #[inline]
    fn sample(self, x: f64, negative_a1: f64, negative_a2: f64,
              state0: &mut f64, state1: &mut f64) -> f64 {
        let y;
        // 0x14960..0x14974. Rust may turn y * -a into FNMUL, which
        // changes NaN sign propagation versus the original FNEG + FMUL.
        // Preserve the operand order and all six arithmetic instructions.
        unsafe {
            std::arch::asm!(
                "fmadd {y:d}, {a2:d}, {x:d}, {state0:d}",
                "fmul {temporary:d}, {y:d}, {negative_a1:d}",
                "fmadd {temporary:d}, {a1:d}, {x:d}, {temporary:d}",
                "fadd {state0:d}, {state1:d}, {temporary:d}",
                "fmul {temporary:d}, {y:d}, {negative_a2:d}",
                "fmadd {state1:d}, {one:d}, {x:d}, {temporary:d}",
                y = out(vreg) y,
                temporary = out(vreg) _,
                a1 = in(vreg) self.a1,
                a2 = in(vreg) self.a2,
                x = in(vreg) x,
                negative_a1 = in(vreg) negative_a1,
                negative_a2 = in(vreg) negative_a2,
                one = in(vreg) 1.0_f64,
                state0 = inout(vreg) *state0,
                state1 = inout(vreg) *state1,
                options(nomem, nostack, preserves_flags),
            );
        }
        y
    }

    fn backwards(self, input: &[f32], output: &mut [f32]) {
        let mut state0 = 0.0_f64;
        let mut state1 = 0.0_f64;
        let negative_a1 = -self.a1;
        let negative_a2 = -self.a2;
        for index in (0..WINDOW).rev() {
            let x = f64::from(input[index]);
            let y = self.sample(x, negative_a1, negative_a2, &mut state0, &mut state1);
            output[index] = y as f32;
        }
    }

    fn backwards_in_place(self, samples: &mut [f32]) {
        let mut state0 = 0.0_f64;
        let mut state1 = 0.0_f64;
        let negative_a1 = -self.a1;
        let negative_a2 = -self.a2;
        for index in (0..WINDOW).rev() {
            let x = f64::from(samples[index]);
            let y = self.sample(x, negative_a1, negative_a2, &mut state0, &mut state1);
            samples[index] = y as f32;
        }
    }
}

#[derive(Default)]
struct Channel {
    input: Vec<f32>,
    output: Vec<f32>,
    written: usize,
    read: usize,
}

impl Channel {
    fn reset(&mut self, active: bool) {
        self.written = 0;
        self.read = 0;
        if active {
            self.input.resize(WINDOW, 0.0);
            self.output.resize(WINDOW, 0.0);
            self.input.fill(0.0);
            self.output.fill(0.0);
        } else {
            self.input.clear();
            self.output.clear();
        }
    }
}

/// Independent per-channel reverse-window correction from the AU wrapper.
///
/// `bands` are wrapper flags 0..4, not the hosted Multipass model flags;
/// `parameters` are effective f32 host values, not hertz. Each section uses
/// the parameter belonging to the previous of two successive enabled bands.
/// Configuration events are explicit so that even an unchanged host write
/// invalidates the state at the same time as the wrapper.
pub struct PhaseCorrection {
    bands: [bool; 5],
    parameters: [f32; 4],
    sample_rate: f32,
    dirty: bool,
    sections: [Allpass; 4],
    section_count: usize,
    channels: Vec<Channel>,
}

impl PhaseCorrection {
    /// Construct the first set of objects. The native wrapper clamps zero
    /// channels to one; a rate is stored as f32 before promotion to f64.
    pub fn new(sample_rate: f32, bands: [bool; 5], parameters: [f32; 4], channels: usize) -> Self {
        let mut phase = Self {
            bands,
            parameters,
            sample_rate,
            dirty: true,
            sections: [Allpass::default(); 4],
            section_count: 0,
            channels: Vec::new(),
        };
        phase.rebuild(sample_rate, channels.max(1));
        phase
    }

    /// Record an effective value for a host write to parameter 0..3. Every
    /// accepted write dirties the correction, even when the value is equal.
    /// Returns false for other parameter IDs, which do not invalidate it.
    pub fn write_parameter(&mut self, id: usize, effective_value: f32) -> bool {
        let Some(value) = self.parameters.get_mut(id) else { return false };
        *value = effective_value;
        self.dirty = true;
        true
    }

    /// Record a wrapper band flag. Only a changed flag dirties the correction.
    /// Returns whether a valid flag actually changed.
    pub fn write_band(&mut self, index: usize, enabled: bool) -> bool {
        let Some(band) = self.bands.get_mut(index) else { return false };
        if *band == enabled {
            return false;
        }
        *band = enabled;
        self.dirty = true;
        true
    }

    /// Record a preset reload after resolving all four effective host values.
    pub fn preset_loaded(&mut self, parameters: [f32; 4]) {
        self.parameters = parameters;
        self.dirty = true;
    }

    /// Mark the correction dirty after a successful host sample-rate
    /// reconfiguration, even if the stored rate is numerically unchanged.
    pub fn sample_rate_reconfigured(&mut self) {
        self.dirty = true;
    }

    /// Apply pending invalidation or a changed channel count/rate. Returns
    /// whether objects were rebuilt. NaN rates compare unequal, as in 0x6ab8.
    /// Rebuilding discards every channel's queued audio without crossfading.
    pub fn prepare(&mut self, sample_rate: f32, channels: usize) -> bool {
        let count = channels.max(1);
        if self.dirty || self.channels.len() != count || self.sample_rate != sample_rate {
            self.rebuild(sample_rate, count);
            true
        } else {
            false
        }
    }

    /// Explicitly rebuild with new settings, preserving the object count.
    pub fn reconfigure(&mut self, sample_rate: f32, bands: [bool; 5], parameters: [f32; 4]) {
        self.bands = bands;
        self.parameters = parameters;
        self.rebuild(sample_rate, self.channels.len());
    }

    fn rebuild(&mut self, sample_rate: f32, channels: usize) {
        self.sample_rate = sample_rate;
        self.section_count = 0;
        // 0x6cc0 first resolves every selected frequency, even when the
        // rate later makes 0x14a9c leave its section list empty.
        let mut frequencies = [0.0_f64; 4];
        let mut frequency_count = 0;
        let mut previous = None;
        for (band, enabled) in self.bands.into_iter().enumerate() {
            if !enabled {
                continue;
            }
            if let Some(index) = previous {
                // A previous enabled flag must be 0..3 when a later one exists.
                let normalized = f64::from(self.parameters[index]);
                // SAFETY: libSystem exp accepts every floating-point input.
                frequencies[frequency_count] = unsafe { system_exp(normalized * LOG_200) } * 60.0;
                frequency_count += 1;
            }
            previous = Some(band);
        }
        // The original FCMP/B.LS excludes ordered <= 0, but admits NaN.
        if !(sample_rate <= 0.0) {
            for &frequency in &frequencies[..frequency_count] {
                self.sections[self.section_count] = Allpass::new(frequency, sample_rate);
                self.section_count += 1;
            }
        }
        self.channels.resize_with(channels, Channel::default);
        for channel in &mut self.channels {
            channel.reset(self.section_count != 0);
        }
        self.dirty = false;
    }

    /// Number of allocated wrapper channel objects (at least one).
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Whether a host event still requires `prepare`.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Number of active reverse allpass sections after `prepare`.
    pub fn section_count(&self) -> usize {
        assert!(!self.dirty, "prepare phase correction after host changes");
        self.section_count
    }

    /// Native coefficient record `[1, a1, a2, a2, a1, 1]`, if present.
    pub fn section_coefficients(&self, index: usize) -> Option<[f64; 6]> {
        assert!(!self.dirty, "prepare phase correction after host changes");
        if index >= self.section_count {
            return None;
        }
        let Allpass { a1, a2 } = self.sections[index];
        Some([1.0, a1, a2, a2, a1, 1.0])
    }

    /// Additional latency reported by the wrapper, not total host latency.
    pub fn latency_frames(&self) -> usize {
        assert!(!self.dirty, "prepare phase correction after host changes");
        if self.section_count == 0 { 0 } else { WINDOW }
    }

    /// Process one channel in place after `prepare`. Each channel has its own
    /// windows. Host calls have positive signed-32-bit frame counts; larger
    /// slices must be presented as separate host blocks.
    pub fn process_channel(&mut self, channel: usize, samples: &mut [f32]) {
        assert!(!self.dirty, "prepare phase correction after host changes");
        assert!(channel < self.channels.len(), "invalid phase channel");
        assert!(samples.len() <= i32::MAX as usize, "phase host block is too large");
        if self.section_count == 0 || samples.is_empty() {
            return;
        }
        let state = &mut self.channels[channel];
        let mut consumed = 0;
        while consumed < samples.len() {
            let count = (WINDOW - state.written).min(samples.len() - consumed);
            let end = consumed + count;
            state.input[state.written..state.written + count].copy_from_slice(&samples[consumed..end]);
            samples[consumed..end].copy_from_slice(&state.output[state.read..state.read + count]);
            state.written += count;
            state.read += count;
            consumed = end;
            if state.written == WINDOW {
                state.read = 0;
                // 0x14dc0 clears all section states at every window; each
                // successive section consumes the preceding rounded f32 output.
                self.sections[0].backwards(&state.input, &mut state.output);
                for section in &self.sections[1..self.section_count] {
                    section.backwards_in_place(&mut state.output);
                }
                state.input.copy_within(HOP..WINDOW, 0);
                state.written -= HOP;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Allpass, WINDOW};

    #[test]
    fn reverse_state_update_retains_the_fused_residual() {
        // For a1=1+2^-52 and x=1-2^-23, the rounded product a1*x is
        // x+2^-52, but the exact product is smaller by 2^-75. 0x14968
        // fuses a1*x with the separately rounded negative product, retaining
        // -2^-75 instead of producing zero as an unfused update would.
        let section = Allpass { a1: f64::from_bits(0x3ff0_0000_0000_0001), a2: 1.0 };
        let mut input = vec![0.0; WINDOW];
        input[WINDOW - 1] = f32::from_bits(0x3f7f_fffe);
        let mut output = vec![0.0; WINDOW];
        section.backwards(&input, &mut output);
        assert_eq!(output[WINDOW - 2].to_bits(), 0x9a00_0000);
        section.backwards_in_place(&mut input);
        assert_eq!(input[WINDOW - 2].to_bits(), 0x9a00_0000);
    }
}
