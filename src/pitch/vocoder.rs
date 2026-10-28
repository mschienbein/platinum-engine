//! Selected CPhaseVocoderV3 state and five-phase channel step machine.
//!
//! The implementation is restricted to the selected near-unity factor and
//! 1/2-channel PN domain. Native comparison has not yet exercised these
//! translations, and this core does not replace the enclosing Eff/Pro scheduler.
//! The reference runs in the host callback's FPCR flush-to-zero scope; these
//! operations do not change FPCR.

use super::fft::{self, RealFft};

#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" {
    #[link_name = "sinf"]
    fn host_sinf(value: f32) -> f32;
    #[link_name = "cosf"]
    fn host_cosf(value: f32) -> f32;
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::math::{sinf as host_sinf, cosf as host_cosf};

const PI: f32 = f32::from_bits(0x4049_0fdb);
const TWO_PI: f32 = f32::from_bits(0x40c9_0fdb);
const NEG_INV_TWO_PI: f32 = f32::from_bits(0xbe22_f983);
const WINDOW_SCALE: f32 = f32::from_bits(0xbeec_5c14);
const WINDOW_OFFSET: f32 = f32::from_bits(0x3f09_d1f6);

fn selected_frame_len(n: usize) -> bool {
    (2048..=32768).contains(&n) && n.is_power_of_two()
}

/// The selected normal operation, or the input-only negative warmup operation.
/// A peak operation on channel 1 is deliberately represented: the reference
/// enters stage 2 for that channel but performs peak selection only on channel 0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    PrimeInput,
    ShiftAndForward,
    MagnitudesAndPhases,
    Peaks,
    Propagate,
    InverseAndOverlap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Step {
    pub channel: usize,
    pub stage: Stage,
}

/// Decode the indices passed to `0x2a6fc` by the selected 1/2-channel core.
/// Negative indices `-1`, `-2` prime channels 0, 1 without the forward FFT.
/// More-negative internal sentinel values are outside this selected call path.
pub fn decode_step(step: i32, channels: usize) -> Option<Step> {
    if !(1..=2).contains(&channels) {
        return None;
    }
    if step < 0 {
        let channel = (!step) as usize;
        return (channel < channels).then_some(Step { channel, stage: Stage::PrimeInput });
    }
    let step = step as usize;
    let channel = step / 5;
    if channel >= channels {
        return None;
    }
    let stage = match step % 5 {
        0 => Stage::ShiftAndForward,
        1 => Stage::MagnitudesAndPhases,
        2 => Stage::Peaks,
        3 => Stage::Propagate,
        _ => Stage::InverseAndOverlap,
    };
    Some(Step { channel, stage })
}

/// Stage markers `+0x524` and `+0x528` from `0x2a6fc`. They are distinct:
/// propagation reads the prior marker while the current marker is already 3.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StageTracking {
    pub current: i32,
    pub previous: i32,
}

impl StageTracking {
    /// Set the current marker before executing the selected operation.
    pub fn begin(&mut self, raw_step: i32, channels: usize) -> Option<Step> {
        let decoded = decode_step(raw_step, channels)?;
        self.current = if raw_step < 0 { 0 } else { raw_step % 5 };
        Some(decoded)
    }

    /// Commit the marker after the operation. Negative warmup commits 0x200,
    /// not zero; this affects the next channel-0 transient decision.
    pub fn finish(&mut self, raw_step: i32) {
        self.previous = if raw_step < 0 { 0x200 } else { self.current };
    }
}

/// Construct the cosine and linear-ramp arrays at `0x2664c`. All four slices
/// are distinct, N-sample buffers; `phase` and `sine` are scratch. N is one of
/// the selected 8–384 kHz core frame lengths, not an arbitrary FFT size.
pub fn initialize_analysis_tables(
    phase: &mut [f32],
    sine: &mut [f32],
    cosine: &mut [f32],
    ramp: &mut [f32],
) {
    let n = phase.len();
    assert!(selected_frame_len(n));
    assert_eq!(sine.len(), n);
    assert_eq!(cosine.len(), n);
    assert_eq!(ramp.len(), n);
    let increment = TWO_PI / (n - 1) as f32;
    for (i, value) in phase.iter_mut().enumerate() {
        *value = increment * i as f32;
    }
    fft::sincos(phase, sine, cosine);
    let increment = 2.0f32 / n as f32;
    for (i, value) in ramp.iter_mut().enumerate() {
        let scaled = increment * i as f32;
        *value = scaled + -1.0;
    }
}

/// `0x27bbc`: update the analysis window, preserving both buffers on an early
/// return. `previous_factor` is the *current* `+0x530` state, not necessarily
/// the newest requested factor; reset sets it to zero. `simple` is the low bit
/// of the helper argument (`new_input_hop > frame_len/8` in `0x2ab2c`).
/// The selected finite controller-factor domain is near 1; the two finite
/// threshold branches are retained because the helper implements them.
/// Returns whether the native helper writes the buffers.
pub fn update_analysis_window(
    cosine: &[f32],
    work: &mut [f32],
    window: &mut [f32],
    previous_factor: f32,
    requested_factor: f32,
    old_input_hop: usize,
    simple: bool,
) -> bool {
    let n = cosine.len();
    assert!(selected_frame_len(n));
    assert_eq!(work.len(), n);
    assert_eq!(window.len(), n);
    assert!(previous_factor.is_finite() && requested_factor.is_finite());
    assert!(old_input_hop <= n);
    if !simple && old_input_hop < n / 4 {
        if requested_factor >= 1.5 {
            if previous_factor >= 1.5 {
                return false;
            }
        } else if previous_factor > 0.0 && previous_factor < 1.5 {
            return false;
        }
    }
    work.copy_from_slice(cosine);
    fft::scale_in_place(work, WINDOW_SCALE);
    fft::add_scalar_in_place(work, WINDOW_OFFSET);
    window.copy_from_slice(work);
    if !simple {
        fft::multiply_in_place(window, work);
        fft::multiply_in_place(window, work);
        if requested_factor < 1.5 {
            fft::multiply_in_place(window, work);
        }
    }
    true
}

/// The selected near-unity state touched by `0x2ab2c`. Other vocoder stages
/// must update `previous_factor`, `frame_count`, `relative_transient`, and
/// `bypass_propagation` at their own reference-defined transition points.
pub struct FactorState<'a> {
    pub previous_factor: f32,
    pub target_factor: f32,
    pub input_hop: usize,
    pub internal_hop: usize,
    pub output_hop: usize,
    pub frame_count: i32,
    pub relative_transient: i32,
    pub aligned_transient: i32,
    pub normalization_position: i32,
    pub peak_gain: f32,
    pub bypass_propagation: bool,
    pub side_taper_enabled: bool,
    pub cosine: &'a [f32],
    pub work: &'a mut [f32],
    pub expected_advance: &'a mut [f32],
    pub analysis_window: &'a mut [f32],
    pub synthesis_window: &'a mut [f32],
    pub normalization_window: &'a mut [f32],
    pub side_window: &'a mut [f32],
}

/// Selected finite near-unity branch of `0x2ab2c`, including reset's zero
/// previous factor and optional side-window preparation. A same-hop request
/// does not install the supplied factor: the native routine reuses its stored
/// target and only clears bypass if that stored target is non-unity.
/// Returns true when hop-dependent state is rebuilt.
pub fn update_factor_and_hops(
    state: &mut FactorState<'_>,
    requested_factor: f32,
    input_hop: usize,
    internal_hop: usize,
) -> bool {
    let n = state.cosine.len();
    assert!(selected_frame_len(n));
    assert!((0.75..1.5).contains(&requested_factor));
    assert!(state.previous_factor == 0.0 || (0.75..1.5).contains(&state.previous_factor));
    assert!(input_hop > 0 && input_hop <= n / 2);
    assert!(internal_hop > 0 && internal_hop <= n / 2);
    assert!(state.input_hop <= n / 2 && state.internal_hop <= n / 2);
    assert_eq!(state.work.len(), n);
    assert_eq!(state.expected_advance.len(), n);
    assert_eq!(state.analysis_window.len(), n);
    assert_eq!(state.synthesis_window.len(), n);
    assert_eq!(state.normalization_window.len(), n);
    assert_eq!(state.side_window.len(), n);
    if state.input_hop == input_hop && state.internal_hop == internal_hop {
        if state.target_factor != 1.0 {
            state.bypass_propagation = false;
        }
        return false;
    }
    if state.input_hop != input_hop {
        initialize_expected_advance(state.expected_advance, input_hop);
    }
    update_analysis_window(
        state.cosine, state.work, state.analysis_window, state.previous_factor,
        requested_factor, state.input_hop, input_hop > n / 8,
    );
    if state.previous_factor < 0.75 {
        state.synthesis_window.copy_from_slice(state.analysis_window);
    }
    state.normalization_window.copy_from_slice(state.analysis_window);
    fft::multiply_in_place(state.normalization_window, state.synthesis_window);
    state.normalization_position = (n / 2 + internal_hop) as i32;
    if state.side_taper_enabled && internal_hop < n / 2 {
        let half = n / 2;
        state.side_window[half..].fill(0.0);
        let increment = PI / (internal_hop * 2 - 1) as f32;
        for i in 0..internal_hop {
            let angle = increment * i as f32;
            // The original calls libSystem sinf independently for each entry.
            state.side_window[half + internal_hop - 1 - i] = unsafe { host_sinf(angle) };
        }
    }
    state.input_hop = input_hop;
    state.internal_hop = internal_hop;
    state.output_hop = internal_hop;
    state.target_factor = requested_factor;
    let mut total = 0.0f32;
    let mut hop_total = 0.0f32;
    for (i, &value) in state.analysis_window[n / 2..].iter().enumerate() {
        total += value;
        if i < internal_hop {
            hop_total += value;
        }
    }
    let gain = total / hop_total;
    state.peak_gain = if gain > 4.0 { 4.0 } else { gain };
    let center = (input_hop / 2) as i32;
    let shifted = state.relative_transient.wrapping_add(center);
    let remainder = shifted % input_hop as i32;
    state.aligned_transient = (remainder - center).max(-center);
    if state.frame_count == 0 {
        state.previous_factor = requested_factor;
    }
    if requested_factor != 1.0 {
        state.bypass_propagation = false;
    }
    true
}

/// Rebuild `+0x310` when `0x2ab2c` changes the input hop. The reference writes
/// all N entries, using a vDSP scalar multiply after the integer-to-f32 pass.
pub fn initialize_expected_advance(expected: &mut [f32], input_hop: usize) {
    let n = expected.len();
    assert!(selected_frame_len(n));
    assert!(input_hop <= n);
    for (i, value) in expected.iter_mut().enumerate() {
        *value = i as f32;
    }
    let scale = (input_hop as f32 * TWO_PI) / n as f32;
    fft::scale_in_place(expected, scale);
}

/// The stage-0 input shift from `0x2a6fc`, also used for negative warmup
/// steps. `history` is the first N entries of the channel's 2N allocation.
/// The transient position is shared, and only channel 0 advances it.
pub fn shift_input(
    channel: usize,
    history: &mut [f32],
    input: &[f32],
    relative_transient: &mut i32,
) {
    let n = history.len();
    assert!(selected_frame_len(n));
    assert!(channel < 2);
    assert!(input.len() <= n);
    let hop = input.len();
    history.copy_within(hop..n, 0);
    history[n - hop..].copy_from_slice(input);
    if channel == 0 && *relative_transient > -((n / 2) as i32) {
        *relative_transient -= hop as i32;
    }
}

/// The windowed and half-rotated forward transforms from `0x28fd8`, after the
/// caller performs `shift_input`. `scratch` and `transient_capture` correspond
/// to the two `+0x2f0` planes and are both cleared on *each* channel. Only
/// channel 0 writes the weighted packed transform and transient time capture.
#[allow(clippy::too_many_arguments)]
pub fn forward_analysis(
    fft_setup: &mut RealFft,
    channel: usize,
    input_history: &[f32],
    analysis_window: &[f32],
    ramp: &[f32],
    packed: &mut [f32],
    weighted_packed: &mut [f32],
    scratch: &mut [f32],
    transient_capture: &mut [f32],
) {
    let n = input_history.len();
    assert!(selected_frame_len(n));
    assert!(channel < 2);
    assert_eq!(analysis_window.len(), n);
    assert_eq!(ramp.len(), n);
    assert_eq!(packed.len(), n);
    assert_eq!(weighted_packed.len(), n);
    assert_eq!(scratch.len(), n);
    assert_eq!(transient_capture.len(), n);
    let half = n / 2;
    scratch.fill(0.0);
    transient_capture.fill(0.0);
    scratch[..half].copy_from_slice(&input_history[half..]);
    fft::multiply_in_place(&mut scratch[..half], &analysis_window[half..]);
    scratch[half..].copy_from_slice(&input_history[..half]);
    fft::multiply_in_place(&mut scratch[half..], &analysis_window[..half]);
    fft_setup.forward_from(scratch, packed);
    fft::scale_in_place(packed, 1.0f32 / n as f32);
    if channel == 0 {
        transient_capture.copy_from_slice(scratch);
        fft::multiply_in_place(&mut scratch[..half], &ramp[half..]);
        fft::multiply_in_place(&mut scratch[half..], &ramp[..half]);
        fft_setup.forward_from(scratch, weighted_packed);
        fft::scale_in_place(weighted_packed, 1.0f32 / n as f32);
        fft::multiply_in_place(weighted_packed, packed);
        for i in 1..half {
            weighted_packed[i] = weighted_packed[i] + weighted_packed[half + i];
        }
    }
}

/// Initial phase-increment-to-bin map in `0x2800c` (before peak selection
/// overwrites the map). `active_bins` is the current `+0x510`, not the FFT
/// half-length. The instruction at `0x2810c` is FCVTMS: round toward minus
/// infinity, with ARM's saturating conversion, rather than truncation.
pub fn estimate_frequency_bins(
    frame_len: usize,
    input_hop: usize,
    active_bins: usize,
    instantaneous_advance: &[f32],
    bins: &mut [i32],
) {
    assert!(selected_frame_len(frame_len));
    assert!(input_hop > 0 && input_hop <= frame_len);
    let half = frame_len / 2;
    assert!(active_bins <= half);
    assert!(instantaneous_advance.len() >= active_bins && bins.len() >= active_bins);
    let scale = frame_len as f32 / (input_hop as f32 * TWO_PI);
    for i in 0..active_bins {
        let shifted = scale * instantaneous_advance[i] + 0.5;
        let estimated = shifted.floor() as i32;
        bins[i] = if estimated <= 0 {
            i as i32
        } else if estimated >= half as i32 {
            half as i32 - 1
        } else {
            estimated
        };
    }
}

/// Working and persistent arrays used by the channel-0 peak step. `scratch`
/// and `peaks` retain their prior contents between frames; the reference
/// reads some entries outside the new candidate prefix.
pub struct PeakState<'a> {
    pub frame_len: usize,
    pub input_hop: usize,
    pub active_bins: usize,
    pub independent_bins: bool,
    pub instantaneous_advance: &'a [f32],
    pub magnitude: &'a [f32],
    pub previous_flags: &'a [i32],
    pub peaks: &'a mut [i32],
    pub assignments: &'a mut [i32],
    pub scratch: &'a mut [i32],
}

/// `0x2800c`: identify channel-0 peaks and replace each active bin's initial
/// frequency estimate with its assigned peak. The selected near-unity
/// frequency cutoff keeps at least `2*N/256` bins. The returned peak count is
/// the `+0x520` value; the caller increments `+0x51c` separately.
pub fn identify_peaks(state: PeakState<'_>) -> usize {
    let n = state.frame_len;
    assert!(selected_frame_len(n));
    let half = n / 2;
    let span = n >> 8;
    let active = state.active_bins;
    assert!(active >= span * 2 && active <= half);
    assert_eq!(state.peaks.len(), half);
    assert_eq!(state.assignments.len(), half);
    assert_eq!(state.scratch.len(), half);
    assert_eq!(state.previous_flags.len(), half);
    assert!(state.instantaneous_advance.len() >= active);
    assert!(state.magnitude.len() >= half + 1);
    state.scratch[0] = 1;
    if state.independent_bins {
        for i in 0..active {
            state.peaks[i] = i as i32;
            state.assignments[i] = i as i32;
        }
        return active;
    }
    estimate_frequency_bins(n, state.input_hop, active, state.instantaneous_advance, state.assignments);
    for i in 0..span - 1 {
        state.peaks[i] = (i + 1) as i32;
    }
    let mut candidate_count = span - 1;
    for i in span..active {
        let difference = state.assignments[i] - i as i32;
        state.scratch[i] = difference;
        let previous = state.scratch[i - 1];
        let crossing = difference <= 0 && if i < span * 2 { previous >= 0 } else { previous > 0 };
        if crossing || (state.previous_flags[i] & !1) == 2 {
            state.peaks[candidate_count] = i as i32;
            candidate_count += 1;
        }
    }

    let mut count = 0usize;
    let mut index = 0usize;
    if state.peaks[0] < (n >> 6) as i32 {
        loop {
            state.scratch[count] = state.peaks[index];
            count += 1;
            index += 1;
            if state.peaks[index] >= (n >> 6) as i32 || index >= candidate_count {
                break;
            }
        }
    }
    if index == 0 {
        index = 1;
    }
    if index == 1 {
        let current = state.peaks[1] as usize;
        let next = state.peaks[2] as usize;
        let previous = state.peaks[0] as usize;
        if state.magnitude[next] <= state.magnitude[current]
            || state.magnitude[previous] > state.magnitude[current]
        {
            state.scratch[count] = current as i32;
            count += 1;
        }
        index = 2;
    }
    let mut current = state.peaks[index];
    let mut bin = (current as usize).max(1);
    loop {
        let value = state.assignments[bin];
        let distance_current = (value - current).unsigned_abs();
        let distance_previous = (value - state.peaks[index - 1]).unsigned_abs();
        if distance_current >= distance_previous || bin == 0 {
            break;
        }
        bin -= 1;
    }
    if current < (n >> 3) as i32 && index < candidate_count {
        loop {
            let left = bin;
            let peak = (current as usize).max(1);
            index += 1;
            let next = state.peaks[index];
            bin = peak;
            loop {
                let value = state.assignments[bin];
                let distance_current = (value - current).unsigned_abs();
                let distance_next = (value - next).unsigned_abs();
                if distance_next <= distance_current || bin >= half - 1 {
                    break;
                }
                let prior = bin;
                bin += 1;
                if prior >= active - 2 {
                    break;
                }
            }
            let right = (bin - 1).min(active - 2);
            bin = right;
            if state.magnitude[left + 1] <= state.magnitude[peak]
                && state.magnitude[right] <= state.magnitude[peak]
            {
                state.scratch[count] = peak as i32;
                count += 1;
            }
            current = next;
            if current >= (n >> 3) as i32 || index >= candidate_count {
                break;
            }
        }
    }
    index = index.max(1);
    while index < candidate_count && state.peaks[index] < active as i32 {
        let peak = state.peaks[index] as usize;
        let next = state.peaks[index + 1] as usize;
        let previous = state.peaks[index - 1] as usize;
        let next_greater = state.magnitude[next] > state.magnitude[peak];
        if (!next_greater && state.magnitude[previous] <= state.magnitude[peak])
            || state.previous_flags[peak] == 2
            || state.previous_flags[index] == 3
        {
            state.scratch[count] = peak as i32;
            count += 1;
        }
        index += 1;
    }
    let terminal = count.max(1) - 1;
    state.scratch[terminal] = active as i32 - 1;
    if count == 0 {
        count = candidate_count.max(1);
        state.peaks[count - 1] = active as i32 - 1;
    } else {
        state.peaks[..count].copy_from_slice(&state.scratch[..count]);
    }
    let mut start = 1usize;
    for i in 0..active {
        if start > count {
            continue;
        }
        let estimate = state.assignments[i];
        let mut cursor = start;
        loop {
            let previous = state.peaks[cursor - 1];
            let next = state.peaks[cursor];
            if (estimate - previous).unsigned_abs() <= (estimate - next).unsigned_abs()
                || next == 0
                || cursor == count
            {
                state.assignments[i] = previous;
                start = cursor.max(4) - 3;
                break;
            }
            cursor += 1;
        }
    }
    count
}

/// Channel-0 transient classification state for `0x28660`. The magnitude
/// history is the same rotating 32-frame array updated by `0x2927c`.
pub struct TransientState<'a> {
    pub frame_len: usize,
    pub input_hop: usize,
    pub frame_count: i32,
    pub previous_factor: f32,
    pub target_factor: f32,
    pub independent_bins: bool,
    pub relative_position: i32,
    pub aligned_position: i32,
    pub external_position_enabled: bool,
    pub detected_position: &'a mut i32,
    pub bypass_propagation: &'a mut bool,
    pub peaks: &'a [i32],
    pub peak_count: usize,
    pub assignments: &'a [i32],
    pub magnitude: &'a [f32],
    pub magnitude_history: &'a [Vec<f32>],
    pub weighted_spectrum: &'a [f32],
    pub peak_difference: &'a mut [f32],
    pub spectral_flux: &'a mut [f32],
    pub peak_energy: &'a mut [f32],
    pub flags: &'a mut [i32],
    pub previous_flags: &'a [i32],
    pub transient_capture: &'a mut [f32],
}

/// Evaluate the selected-channel peak transients and update their flags,
/// temporal position and possible unity bypass. The ARM64 code uses scalar
/// f32 operations, `vDSP_vabs`, and `vDSP_maxvi`; no generic onset detector
/// or independent per-channel transient history is substituted here.
pub fn classify_transients(state: TransientState<'_>) {
    let n = state.frame_len;
    assert!(selected_frame_len(n));
    let half = n / 2;
    let span = n >> 8;
    assert!(state.input_hop > 0 && state.input_hop <= n / 2);
    assert!(state.frame_count >= 1);
    assert!(state.peak_count > 0 && state.peak_count <= half);
    assert!(state.peaks.len() >= state.peak_count);
    assert_eq!(state.assignments.len(), half);
    assert!(state.magnitude.len() >= half + 1);
    assert_eq!(state.magnitude_history.len(), 32);
    assert!(state.magnitude_history.iter().all(|frame| frame.len() >= half + 1));
    assert!(state.weighted_spectrum.len() >= half);
    assert_eq!(state.peak_difference.len(), half);
    assert_eq!(state.spectral_flux.len(), half);
    assert_eq!(state.peak_energy.len(), half);
    assert_eq!(state.flags.len(), half);
    assert_eq!(state.previous_flags.len(), half);
    assert_eq!(state.transient_capture.len(), n);
    let history_index = ((state.frame_count.max(3) - 1) as usize)
        .min(((n / state.input_hop) >> 2) + 1);
    assert!(history_index < state.magnitude_history.len());
    let old = &state.magnitude_history[history_index];
    let tiny = f32::from_bits(0x179a_be15);
    let delta_floor = f32::from_bits(0x2834_24dc);
    state.flags[0] = 0;
    state.peak_difference[0] = 0.0;
    state.spectral_flux[0] = 0.0;
    let mut previous_energy = tiny;
    let mut current_energy = tiny;
    let mut high_energy = tiny;
    let mut bass_energy = 0.0f32;
    let mut weighted_total = 0.0f32;
    let mut high_weighted = 0.0f32;
    for i in 1..span {
        let difference = state.magnitude[i] - old[i];
        let squared_difference = difference * difference;
        if squared_difference > delta_floor {
            state.peak_difference[0] += squared_difference;
        }
        previous_energy += old[i] * old[i];
        weighted_total -= state.weighted_spectrum[i];
        let square = state.magnitude[i] * state.magnitude[i];
        current_energy += square;
        if i <= span / 2 {
            bass_energy += square;
        } else {
            high_energy += square;
            high_weighted -= state.weighted_spectrum[i];
        }
    }
    state.peak_difference[0] /= previous_energy;
    state.spectral_flux[0] = weighted_total / current_energy;
    let bass_average = bass_energy / (span - 1) as f32;
    for i in 0..span - 1 {
        state.peak_difference[i] = state.peak_difference[0];
        state.spectral_flux[i] = state.spectral_flux[0];
        state.peak_energy[i] = bass_average;
    }
    let mut bin = span - 2;
    for peak_index in span - 2..state.peak_count {
        let peak = state.peaks[peak_index] as usize;
        state.peak_difference[peak_index] = 0.0;
        state.spectral_flux[peak] = 0.0;
        let mut old_energy = tiny;
        let mut energy = tiny;
        let mut weighted = 0.0f32;
        while bin < half && (peak as i32) >= state.assignments[bin] {
            let difference = state.magnitude[bin] - old[bin];
            let squared_difference = difference * difference;
            if squared_difference > delta_floor {
                state.peak_difference[peak_index] += squared_difference;
            }
            old_energy += old[bin] * old[bin];
            weighted -= state.weighted_spectrum[bin];
            energy += state.magnitude[bin] * state.magnitude[bin];
            bin += 1;
        }
        state.peak_energy[peak_index] = energy;
        high_energy += energy;
        state.peak_difference[peak_index] /= old_energy;
        high_weighted += weighted;
        state.spectral_flux[peak] = weighted / energy;
    }
    let mut hop_scale = state.input_hop as f32 / (n >> 3) as f32;
    if hop_scale > 1.0 {
        hop_scale = 1.0;
    }
    let global_flux = high_weighted / (high_energy + tiny);
    let mode = if state.independent_bins { 1 } else { 0 };
    let threshold_low = [f32::from_bits(0xbe4c_cccd), -0.5][mode];
    let threshold_high = [f32::from_bits(0x3dcc_cccd), 0.5][mode];
    let global_threshold = if state.independent_bins {
        -0.5
    } else if state.target_factor >= 1.5 {
        -0.25
    } else {
        f32::from_bits(0xbe66_6666)
    };
    let interval = state.input_hop / (n >> 2);
    let mut weight_two = 0.0f32;
    let mut weight_one = 0.0f32;
    let mut weight_three = 0.0f32;
    let mut has_position = false;
    let first_frame = state.frame_count == 1;
    for i in 0..state.peak_count {
        let peak = state.peaks[i] as usize;
        let difference = state.peak_difference[i];
        let flux = state.spectral_flux[peak];
        let old_flag = state.previous_flags[peak];
        // ARM's B.LE on an unordered floating comparison takes the branch.
        if !(difference > 0.75) {
            state.flags[peak] = 0;
        } else if !(flux > hop_scale * threshold_low) || !(global_flux > global_threshold) {
            state.flags[peak] = if first_frame { 2 } else { 1 };
            weight_one += state.peak_energy[i];
        } else if flux < hop_scale * threshold_high || old_flag == 1 {
            let qualifies = if difference > 5.0 { old_flag != 2 } else { old_flag == 1 };
            let inside = if first_frame {
                interval < 1
            } else {
                interval < state.frame_count as usize
            };
            if qualifies && inside {
                state.flags[peak] = 2;
                weight_two += state.peak_energy[i];
                has_position = true;
            } else {
                state.flags[peak] = 0;
            }
        } else if state.target_factor != state.previous_factor && old_flag == 2 {
            state.flags[peak] = 1;
            weight_one += state.peak_energy[i];
        } else if state.previous_factor >= 0.5 {
            state.flags[peak] = if first_frame { 2 } else { 0 };
        } else {
            state.flags[peak] = 3;
            weight_three += state.peak_energy[i];
        }
    }
    if has_position {
        fft::abs_in_place(state.transient_capture);
        let (_, index) = fft::max_value(state.transient_capture);
        let mut position = index as i32;
        if position > state.input_hop as i32 {
            position = if position > (n - state.input_hop) as i32 { position - n as i32 } else { 0 };
        }
        *state.detected_position = position;
    } else {
        *state.detected_position = 0;
    }
    let relative = state.relative_position;
    let aligned = state.aligned_position;
    let half_hop = (state.input_hop / 2) as i32;
    if relative <= half as i32 && relative >= aligned - half_hop && state.external_position_enabled {
        if relative <= aligned + half_hop {
            *state.detected_position = aligned;
            for &peak in &state.peaks[..state.peak_count] {
                let flag = &mut state.flags[peak as usize];
                if *flag == 1 { *flag = 2; }
            }
            return;
        }
        for &peak in &state.peaks[..state.peak_count] {
            let flag = &mut state.flags[peak as usize];
            if *flag == 2 { *flag = 1; }
        }
        if state.target_factor == 1.0 {
            *state.bypass_propagation = true;
        }
        return;
    }
    let total_energy = bass_energy + high_energy;
    let mut set_bypass = false;
    if weight_two / total_energy > 0.2 {
        for &peak in &state.peaks[..state.peak_count] {
            let flag = &mut state.flags[peak as usize];
            if *flag == 1 { *flag = 2; }
        }
        set_bypass = state.target_factor == 1.0 && relative > -(half as i32);
    }
    if weight_one / total_energy > 0.2
        && global_flux < hop_scale * threshold_low
        && weight_one > weight_two
    {
        for &peak in &state.peaks[..state.peak_count] {
            let flag = &mut state.flags[peak as usize];
            if *flag == 2 { *flag = 1; }
        }
        return;
    }
    if weight_three / total_energy > 0.4 {
        for &peak in &state.peaks[..state.peak_count] {
            let flag = &mut state.flags[peak as usize];
            if *flag == 0 { *flag = 3; }
        }
    }
    if set_bypass {
        *state.bypass_propagation = true;
    }
}

/// `0x202a4` uses FRINTM (round toward minus infinity), not an integer cast
/// or `% 2π`. Every multiply/add is separately rounded in the reference.
#[inline]
fn wrap_one(value: f32) -> f32 {
    let shifted = value + PI;
    let turns = (shifted * NEG_INV_TWO_PI).floor();
    let offset = turns * TWO_PI;
    let wrapped = shifted + offset;
    wrapped + PI
}

/// Separate-input phase wrapping, as used by `0x2927c`.
pub fn wrap_phase(input: &[f32], output: &mut [f32]) {
    assert_eq!(input.len(), output.len());
    for (out, &value) in output.iter_mut().zip(input) {
        *out = wrap_one(value);
    }
}

/// In-place phase wrapping, as used by `0x29550`.
pub fn wrap_phase_in_place(values: &mut [f32]) {
    for value in values {
        *value = wrap_one(*value);
    }
}

/// Complete per-channel inputs and shared work arrays for `0x29550`.
/// Before calling channel 0 with `previous_stage < current_stage` and a
/// nonzero peak count, execute `classify_transients` on the same state; the
/// native function invokes that helper at precisely that point.
pub struct PropagationState<'a> {
    pub frame_len: usize,
    pub channel: usize,
    pub channels: usize,
    pub active_bins: usize,
    pub side_cutoff: usize,
    pub side_enabled: bool,
    pub frame_count: i32,
    pub current_stage: i32,
    pub previous_stage: i32,
    pub previous_factor: &'a mut f32,
    pub target_factor: f32,
    pub detected_position: &'a mut i32,
    pub peak_gain: f32,
    pub peaks: &'a [i32],
    pub peak_count: usize,
    pub assignments: &'a [i32],
    pub flags: &'a mut [i32],
    pub previous_flags: &'a mut [i32],
    pub current_phase: &'a [f32],
    pub synthesis_history: &'a mut [f32],
    pub magnitude: &'a [f32],
    pub held_magnitude: &'a mut [f32],
    pub packed: &'a [f32],
    pub output: &'a mut [f32],
    pub expected_advance: &'a [f32],
    pub instantaneous_advance: &'a mut [f32],
    pub synthesis_peak_phase: &'a mut [f32],
    pub rotation_phase: &'a mut [f32],
    pub rotation_cosine: &'a mut [f32],
    pub rotation_sine: &'a mut [f32],
    pub peak_phase: &'a mut [f32],
    pub sincos_angles: &'a mut [f32],
    pub sincos_sine: &'a mut [f32],
    pub sincos_cosine: &'a mut [f32],
    pub spectral_flux: &'a [f32],
    pub normalization_window: &'a [f32],
    pub side_mask: &'a mut [f32],
    pub main_mask: &'a mut [f32],
}

/// Propagate phases and produce the packed inverse spectrum (`0x29550`).
/// Its channel-0 transient helper is supplied separately as described above;
/// shared rotations persist into channel 1 and the last channel commits the
/// prior per-bin flags and factor.
pub fn propagate_phases(state: PropagationState<'_>) {
    let n = state.frame_len;
    assert!(selected_frame_len(n));
    let half = n / 2;
    assert!((1..=2).contains(&state.channels) && state.channel < state.channels);
    assert!(state.active_bins <= half && state.side_cutoff <= half);
    assert!(state.peak_count <= half && state.peaks.len() >= state.peak_count);
    assert_eq!(state.assignments.len(), half);
    assert_eq!(state.flags.len(), half);
    assert_eq!(state.previous_flags.len(), half);
    assert_eq!(state.current_phase.len(), half + 1);
    assert_eq!(state.synthesis_history.len(), half + 1);
    assert_eq!(state.magnitude.len(), half + 1);
    assert_eq!(state.held_magnitude.len(), half + 1);
    assert_eq!(state.packed.len(), n);
    assert_eq!(state.output.len(), n);
    assert!(state.expected_advance.len() >= half);
    assert_eq!(state.instantaneous_advance.len(), half);
    assert!(state.synthesis_peak_phase.len() >= half);
    assert_eq!(state.rotation_phase.len(), half);
    assert_eq!(state.rotation_cosine.len(), half);
    assert_eq!(state.rotation_sine.len(), half);
    assert_eq!(state.peak_phase.len(), half);
    assert_eq!(state.sincos_angles.len(), half);
    assert_eq!(state.sincos_sine.len(), half);
    assert_eq!(state.sincos_cosine.len(), half);
    assert_eq!(state.spectral_flux.len(), half);
    assert_eq!(state.normalization_window.len(), n);
    assert_eq!(state.side_mask.len(), n);
    assert_eq!(state.main_mask.len(), n);
    let mut detected = *state.detected_position;
    if state.frame_count == 1 {
        detected = 0;
        *state.detected_position = 0;
    }
    let mut side_count = 0usize;
    if state.peak_count == 0 {
        state.output.fill(0.0);
    } else {
        if state.channel == 0 {
            for j in 0..state.peak_count {
                let peak = state.peaks[j] as usize;
                let old = state.synthesis_history[peak];
                let value = if state.flags[peak] == 2 {
                    if state.previous_stage >= state.current_stage {
                        state.flags[peak] = 0;
                        let advance = state.instantaneous_advance[peak] * *state.previous_factor;
                        old + advance
                    } else {
                        let expected = state.expected_advance[peak];
                        let difference = (state.current_phase[peak] - old) - expected;
                        let advance = expected + wrap_one(difference);
                        state.instantaneous_advance[peak] = advance;
                        old + advance
                    }
                } else {
                    let advance = state.instantaneous_advance[peak] * *state.previous_factor;
                    old + advance
                };
                state.peak_phase[j] = value;
            }
            wrap_phase_in_place(&mut state.peak_phase[..state.peak_count]);
            for j in 0..state.peak_count {
                let peak = state.peaks[j] as usize;
                let phase = state.peak_phase[j];
                state.synthesis_peak_phase[peak] = phase;
                let rotation = phase - state.current_phase[peak];
                state.rotation_phase[peak] = rotation;
                state.sincos_angles[j] = rotation;
            }
            fft::sincos(
                &state.sincos_angles[..state.peak_count],
                &mut state.sincos_sine[..state.peak_count],
                &mut state.sincos_cosine[..state.peak_count],
            );
            for j in 0..state.peak_count {
                let peak = state.peaks[j] as usize;
                state.rotation_cosine[peak] = state.sincos_cosine[j];
                state.rotation_sine[peak] = state.sincos_sine[j];
            }
        }
        state.synthesis_history.copy_from_slice(state.current_phase);
        state.output.fill(0.0);
        let detected_f = detected as f32;
        let phase_offset = detected_f - *state.previous_factor * detected_f;
        let phase_step = (phase_offset * TWO_PI) / n as f32;
        for bin in 1..state.active_bins {
            let peak = state.assignments[bin] as usize;
            let phase = state.rotation_phase[peak] + state.synthesis_history[bin];
            state.synthesis_history[bin] = phase;
            let flag = state.flags[peak];
            if flag == 0 || flag == 3 {
                if state.previous_flags[bin] != 2 {
                    state.held_magnitude[bin] = state.magnitude[bin];
                }
                let real = state.packed[bin];
                let imaginary = state.packed[half + bin];
                let cosine = state.rotation_cosine[peak];
                let sine = state.rotation_sine[peak];
                state.output[bin] = real * cosine - imaginary * sine;
                state.output[half + bin] = real * sine + imaginary * cosine;
                if state.side_enabled && state.channel == 0 && bin >= state.side_cutoff
                    && state.previous_flags[bin] != 2 && state.spectral_flux[peak] < 0.3
                    && state.flags[bin] != 3
                {
                    side_count += 1;
                    state.side_mask[bin] = 1.0;
                    state.side_mask[half + bin] = 1.0;
                    state.main_mask[bin] = 0.0;
                    state.main_mask[half + bin] = 0.0;
                }
            } else if flag == 1 {
                // The reference uses libSystem's scalar cosf/sinf in this arm.
                let cosine = unsafe { host_cosf(phase) };
                state.output[bin] = cosine * state.held_magnitude[bin];
                let sine = unsafe { host_sinf(phase) };
                state.output[half + bin] = state.held_magnitude[bin] * sine;
            } else {
                if phase_offset == 0.0 {
                    let real = state.packed[bin];
                    let imaginary = state.packed[half + bin];
                    let cosine = state.rotation_cosine[peak];
                    let sine = state.rotation_sine[peak];
                    state.output[bin] = real * cosine - imaginary * sine;
                    state.output[half + bin] = real * sine + imaginary * cosine;
                } else {
                    let adjusted = phase_step * bin as f32 + phase;
                    state.synthesis_history[bin] = adjusted;
                    let cosine = unsafe { host_cosf(adjusted) };
                    state.output[bin] = cosine * state.magnitude[bin];
                    let sine = unsafe { host_sinf(adjusted) };
                    state.output[half + bin] = state.magnitude[bin] * sine;
                }
                let offset_index = (half as i32 + *state.detected_position) as usize;
                assert!(offset_index < n);
                let ratio = state.normalization_window[half] / state.normalization_window[offset_index];
                let ratio = if ratio > 1.5 { 1.5 } else { ratio };
                let real_gain = state.peak_gain * ratio;
                let imag_gain = ratio * state.peak_gain;
                state.output[bin] = real_gain * state.output[bin];
                state.output[half + bin] = imag_gain * state.output[half + bin];
            }
        }
        let dc = state.magnitude[0] + state.magnitude[0];
        let nyquist = state.magnitude[half] + state.magnitude[half];
        state.output[0] = dc * unsafe { host_cosf(state.synthesis_history[0]) };
        state.output[half] = nyquist * unsafe { host_cosf(state.synthesis_history[half]) };
    }
    if state.channel == 0 && state.side_enabled && state.side_cutoff < state.active_bins {
        let high_bins = state.active_bins - state.side_cutoff;
        if ((high_bins - side_count) as f32) / (high_bins as f32) < 0.25 {
            for bin in state.side_cutoff..state.active_bins {
                state.side_mask[bin] = 1.0;
                state.side_mask[half + bin] = 1.0;
                state.main_mask[bin] = 0.0;
                state.main_mask[half + bin] = 0.0;
            }
        }
    }
    if state.channel + 1 == state.channels {
        for bin in 1..state.active_bins {
            state.previous_flags[bin] = state.flags[state.assignments[bin] as usize];
        }
        *state.previous_factor = state.target_factor;
    }
}

/// Shared channel-0 histories required by `0x2927c`. Ring entries are rotated
/// by ownership, exactly as the native pointer array rotates; no samples are
/// moved except the new magnitude frame into the recycled slot.
pub struct ChannelZeroPhase<'a> {
    pub expected_advance: &'a [f32],
    pub instantaneous_advance: &'a mut [f32],
    pub magnitude_history: &'a mut [Vec<f32>],
}

/// Extract one channel's packed-real magnitude and phase (`0x2927c`). The
/// selected table's phase helper mutates exact-zero real bins *before* the
/// magnitude call; packed DC and Nyquist then receive special handling.
/// Channel 0 alone updates shared phase increments and the 32-frame history.
pub fn extract_phase_and_magnitude(
    channel: usize,
    packed: &mut [f32],
    previous_phase: &mut [f32],
    magnitude: &mut [f32],
    phase_scratch: &mut [f32],
    zero: Option<ChannelZeroPhase<'_>>,
) {
    let n = packed.len();
    assert!(selected_frame_len(n));
    assert!(channel < 2);
    assert_eq!(zero.is_some(), channel == 0);
    let half = n / 2;
    assert_eq!(previous_phase.len(), half + 1);
    assert_eq!(magnitude.len(), half + 1);
    assert_eq!(phase_scratch.len(), half + 1);
    fft::phase(packed, &mut phase_scratch[..half]);
    fft::magnitude(packed, &mut magnitude[..half]);
    magnitude[0] = packed[0].abs() * 0.5;
    magnitude[half] = packed[half].abs() * 0.5;
    if let Some(zero) = zero {
        assert_eq!(zero.expected_advance.len(), half);
        assert_eq!(zero.instantaneous_advance.len(), half);
        assert_eq!(zero.magnitude_history.len(), 32);
        assert!(zero.magnitude_history.iter().all(|frame| frame.len() == half + 1));
        fft::add_in_place(&mut previous_phase[..half], zero.expected_advance);
        fft::scale_in_place(&mut previous_phase[..half], -1.0);
        fft::add_in_place(&mut previous_phase[..half], &phase_scratch[..half]);
        wrap_phase(&previous_phase[..half], zero.instantaneous_advance);
        zero.instantaneous_advance[0] = 0.0;
        fft::add_in_place(zero.instantaneous_advance, zero.expected_advance);
        zero.magnitude_history.rotate_right(1);
        zero.magnitude_history[0].copy_from_slice(magnitude);
    }
    phase_scratch[0] = if packed[0] < 0.0 { PI } else { 0.0 };
    phase_scratch[half] = if packed[half] < 0.0 { PI } else { 0.0 };
    previous_phase.copy_from_slice(phase_scratch);
}

/// Buffers and counters consumed by `0x29fa0`. The normalization state and
/// window are shared across channels; output overlap is per channel.
pub struct InverseFrame<'a> {
    pub channel: usize,
    pub bypass_propagation: bool,
    pub input_packed: &'a [f32],
    pub synthesis_packed: &'a mut [f32],
    pub synthesis_window: &'a [f32],
    pub output_overlap: &'a mut [f32],
    pub normalization_window: &'a [f32],
    pub normalization_history: &'a mut [f32],
    pub normalization_gain: &'a mut [f32],
    pub normalization_position: &'a mut i32,
    /// The `+0x500` output-consumption hop.
    pub output_hop: usize,
    /// The separate `+0x508` normalization/output multiplication length.
    pub internal_hop: usize,
}

/// Inverse transform and overlap/normalization from `0x29fa0`. This leaves the
/// per-channel output queue shift and the optional side-input path to the
/// surrounding step machine; it is not a complete pitch-synthesis stage.
pub fn inverse_and_overlap(fft_setup: &mut RealFft, frame: InverseFrame<'_>) {
    let n = frame.synthesis_packed.len();
    assert!(selected_frame_len(n));
    assert!(frame.channel < 2);
    assert_eq!(frame.input_packed.len(), n);
    assert_eq!(frame.synthesis_window.len(), n);
    assert_eq!(frame.output_overlap.len(), n);
    assert_eq!(frame.normalization_window.len(), n);
    assert_eq!(frame.normalization_history.len(), n);
    assert_eq!(frame.normalization_gain.len(), n);
    assert!(frame.output_hop <= n && frame.internal_hop <= n);
    if frame.bypass_propagation {
        fft_setup.inverse_from(frame.input_packed, frame.synthesis_packed);
    } else {
        fft_setup.inverse(frame.synthesis_packed);
    }
    let half = n / 2;
    let (first, second) = frame.synthesis_packed.split_at_mut(half);
    let (out_first, out_second) = frame.output_overlap.split_at_mut(half);
    fft::multiply_in_place(second, &frame.synthesis_window[..half]);
    fft::add_in_place(out_first, second);
    fft::multiply_in_place(first, &frame.synthesis_window[half..]);
    fft::add_in_place(out_second, first);
    if frame.channel == 0 && *frame.normalization_position > -(half as i32) {
        fft::add_in_place(frame.normalization_history, frame.normalization_window);
        frame.normalization_gain[..frame.internal_hop].fill(1.0);
        let floor = f32::from_bits(0x3727_c5ac);
        for value in &mut frame.normalization_history[..frame.internal_hop] {
            if *value < floor {
                *value = floor;
            }
        }
        fft::divide_in_place(
            &mut frame.normalization_gain[..frame.internal_hop],
            &frame.normalization_history[..frame.internal_hop],
        );
        frame.normalization_history.copy_within(frame.output_hop..n, 0);
        frame.normalization_history[n - frame.output_hop..].fill(0.0);
        *frame.normalization_position -= frame.output_hop as i32;
    }
    fft::multiply_in_place(
        &mut frame.output_overlap[..frame.internal_hop],
        &frame.normalization_gain[..frame.internal_hop],
    );
}

/// Buffers for `0x2a2e8` after stage 1 has extracted the main spectrum.
pub struct SideAnalysis<'a> {
    pub channel: usize,
    pub input: &'a [f32],
    pub history: &'a mut [f32],
    pub window: &'a [f32],
    pub packed: &'a mut [f32],
    pub side_mask: &'a mut [f32],
    pub main_mask: &'a mut [f32],
}

/// The side-input transform and channel-0 mask reset from `0x2a2e8`.
/// The caller is responsible for the original `+0x54a && side != null` guard.
pub fn analyze_side(fft_setup: &mut RealFft, side: SideAnalysis<'_>) {
    let n = side.history.len();
    assert!(selected_frame_len(n));
    assert!(side.channel < 2);
    assert!(side.input.len() <= n / 2);
    assert_eq!(side.window.len(), n);
    assert_eq!(side.packed.len(), n);
    assert_eq!(side.side_mask.len(), n);
    assert_eq!(side.main_mask.len(), n);
    if side.channel == 0 {
        side.side_mask.fill(0.0);
        side.side_mask[..2].fill(1.0);
        side.main_mask.fill(1.0);
    }
    let half = n / 2;
    side.history[half..half + side.input.len()].copy_from_slice(side.input);
    side.packed.copy_from_slice(side.history);
    fft::multiply_in_place(side.packed, side.window);
    fft_setup.forward(side.packed);
    fft::scale_in_place(side.packed, 1.0f32 / n as f32);
}

/// Apply the two per-bin masks immediately after side-aware phase propagation.
pub fn apply_side_masks(
    side_packed: &mut [f32],
    synthesis_packed: &mut [f32],
    side_mask: &[f32],
    main_mask: &[f32],
) {
    assert_eq!(side_packed.len(), synthesis_packed.len());
    assert!(selected_frame_len(side_packed.len()));
    fft::multiply_in_place(side_packed, side_mask);
    fft::multiply_in_place(synthesis_packed, main_mask);
}

/// State used by the side-input branch of stage 4 in `0x2a6fc`.
pub struct SideInverse<'a> {
    pub packed: &'a mut [f32],
    pub window: &'a [f32],
    pub side_overlap: &'a mut [f32],
    pub side_history: &'a mut [f32],
    pub output_overlap: &'a mut [f32],
    pub bypass_propagation: bool,
    pub output_hop: usize,
    pub internal_hop: usize,
}

/// Invert and overlap-add the side spectrum, then shift its overlap and input
/// histories. The main output is still copied/shifted by `finish_output`.
pub fn inverse_side_and_advance(fft_setup: &mut RealFft, side: SideInverse<'_>) {
    let n = side.packed.len();
    assert!(selected_frame_len(n));
    assert_eq!(side.window.len(), n);
    assert_eq!(side.side_overlap.len(), n);
    assert_eq!(side.side_history.len(), n);
    assert_eq!(side.output_overlap.len(), n);
    assert!(side.output_hop <= n);
    assert!(side.internal_hop <= n / 2);
    fft_setup.inverse(side.packed);
    fft::multiply_in_place(side.packed, side.window);
    fft::add_in_place(side.side_overlap, side.packed);
    if !side.bypass_propagation {
        fft::add_in_place(
            &mut side.output_overlap[..side.internal_hop],
            &side.side_overlap[..side.internal_hop],
        );
    }
    side.side_overlap.copy_within(side.output_hop..n, 0);
    let half = n / 2;
    side.side_overlap[half..].fill(0.0);
    side.side_history.copy_within(half..n, half - side.internal_hop);
    side.side_history[half..].fill(0.0);
}

/// Finish stage 4: copy and shift the main overlap, then update the shared
/// side window only on the last channel at the exact normalization boundary.
pub fn finish_output(
    channel: usize,
    channels: usize,
    normalization_position: i32,
    internal_hop: usize,
    output_overlap: &mut [f32],
    output: &mut [f32],
    side_window: &mut [f32],
) {
    let n = output_overlap.len();
    assert!(selected_frame_len(n));
    assert!((1..=2).contains(&channels) && channel < channels);
    assert_eq!(side_window.len(), n);
    assert!(output.len() <= n && internal_hop <= n / 2);
    let output_hop = output.len();
    output.copy_from_slice(&output_overlap[..output_hop]);
    output_overlap.copy_within(output_hop..n, 0);
    output_overlap[n - output_hop..].fill(0.0);
    let half = n / 2;
    if channel + 1 == channels && normalization_position == half as i32 && internal_hop < half {
        side_window[..half].fill(0.0);
        for i in 0..internal_hop {
            side_window[half - internal_hop + i] = side_window[half + internal_hop - 1 - i];
        }
    }
}

struct ChannelState {
    input: Vec<f32>,
    packed: Vec<f32>,
    side_history: Vec<f32>,
    output: Vec<f32>,
    current_phase: Vec<f32>,
    synthesis_history: Vec<f32>,
    held_magnitude: Vec<f32>,
    side_output: Vec<f32>,
    magnitude: Vec<f32>,
}

impl ChannelState {
    fn new(n: usize) -> Self {
        let bins = n / 2 + 1;
        Self {
            input: vec![0.0; n],
            packed: vec![0.0; n],
            side_history: vec![0.0; n],
            output: vec![0.0; n],
            current_phase: vec![0.0; bins],
            synthesis_history: vec![0.0; bins],
            held_magnitude: vec![0.0; bins],
            side_output: vec![0.0; n],
            magnitude: vec![0.0; bins],
        }
    }
}

/// Observable scalar step/state values; spectral histories remain owned by
/// `PhaseVocoder` so snapshots do not allocate or copy signal buffers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VocoderState {
    pub input_hop: usize,
    pub internal_hop: usize,
    pub output_hop: usize,
    pub active_bins: usize,
    pub side_cutoff: usize,
    pub frame_count: i32,
    pub peak_count: usize,
    pub stages: StageTracking,
    pub previous_factor: f32,
    pub target_factor: f32,
    pub peak_gain: f32,
    pub relative_position: i32,
    pub aligned_position: i32,
    pub detected_position: i32,
    pub normalization_position: i32,
    pub independent_bins: bool,
    pub side_taper_enabled: bool,
    pub side_enabled: bool,
    pub bypass_propagation: bool,
    pub external_position_enabled: bool,
}

/// Owned selected `CPhaseVocoderV3` state. It has no internal input/output
/// FIFO: the direct adapter supplies one hop and receives one hop for each
/// channel's stage-4 call. This is separate from the Eff and Pro schedulers.
pub struct PhaseVocoder {
    n: usize,
    channels: Vec<ChannelState>,
    fft: RealFft,
    input_hop: usize,
    internal_hop: usize,
    output_hop: usize,
    active_bins: usize,
    side_cutoff: usize,
    frame_count: i32,
    peak_count: usize,
    stages: StageTracking,
    previous_factor: f32,
    target_factor: f32,
    peak_gain: f32,
    relative_position: i32,
    aligned_position: i32,
    detected_position: i32,
    normalization_position: i32,
    independent_bins: bool,
    side_taper_enabled: bool,
    side_enabled: bool,
    bypass_propagation: bool,
    external_position_enabled: bool,
    history: Vec<Vec<f32>>,
    inverse_packed: Vec<f32>,
    flags: Vec<i32>,
    previous_flags: Vec<i32>,
    phase_scratch: Vec<f32>,
    synthesis_peak_phase: Vec<f32>,
    peak_difference: Vec<f32>,
    spectral_flux: Vec<f32>,
    rotation_phase: Vec<f32>,
    rotation_cosine: Vec<f32>,
    rotation_sine: Vec<f32>,
    instantaneous_advance: Vec<f32>,
    weighted_spectrum: Vec<f32>,
    peak_energy: Vec<f32>,
    work: Vec<f32>,
    transient_capture: Vec<f32>,
    expected_advance: Vec<f32>,
    analysis_window: Vec<f32>,
    synthesis_window: Vec<f32>,
    normalization_window: Vec<f32>,
    normalization_gain: Vec<f32>,
    normalization_history: Vec<f32>,
    sincos_angles: Vec<f32>,
    sincos_sine: Vec<f32>,
    sincos_cosine: Vec<f32>,
    cosine: Vec<f32>,
    peak_phase: Vec<f32>,
    side_window: Vec<f32>,
    side_mask: Vec<f32>,
    main_mask: Vec<f32>,
    ramp: Vec<f32>,
    peaks: Vec<i32>,
    assignments: Vec<i32>,
    peak_scratch: Vec<i32>,
}

impl PhaseVocoder {
    /// Construct and reset the selected 1/2-channel core, initially using
    /// N/8 input and output hops. N is one of 2048, 4096, 8192, 16384, 32768.
    pub fn new(frame_len: usize, channels: usize) -> Option<Self> {
        if !selected_frame_len(frame_len) || !(1..=2).contains(&channels) {
            return None;
        }
        let fft = RealFft::new(frame_len)?;
        let n = frame_len;
        let half = n / 2;
        let mut state = Self {
            n, channels: (0..channels).map(|_| ChannelState::new(n)).collect(), fft,
            input_hop: 0, internal_hop: 0, output_hop: 0, active_bins: half, side_cutoff: half,
            frame_count: 0, peak_count: 0, stages: StageTracking::default(),
            previous_factor: 0.0, target_factor: 1.0, peak_gain: 0.0,
            relative_position: 0, aligned_position: 0, detected_position: 0,
            normalization_position: 0, independent_bins: false, side_taper_enabled: false,
            side_enabled: false, bypass_propagation: true, external_position_enabled: true,
            history: (0..32).map(|_| vec![0.0; half + 1]).collect(),
            inverse_packed: vec![0.0; n], flags: vec![0; half], previous_flags: vec![0; half],
            phase_scratch: vec![0.0; half + 1], synthesis_peak_phase: vec![0.0; half + 1],
            peak_difference: vec![0.0; half], spectral_flux: vec![0.0; half],
            rotation_phase: vec![0.0; half], rotation_cosine: vec![0.0; half],
            rotation_sine: vec![0.0; half], instantaneous_advance: vec![0.0; half],
            weighted_spectrum: vec![0.0; n], peak_energy: vec![0.0; half],
            work: vec![0.0; n], transient_capture: vec![0.0; n], expected_advance: vec![0.0; n],
            analysis_window: vec![0.0; n], synthesis_window: vec![0.0; n],
            normalization_window: vec![0.0; n], normalization_gain: vec![0.0; n],
            normalization_history: vec![0.0; n], sincos_angles: vec![0.0; half],
            sincos_sine: vec![0.0; half], sincos_cosine: vec![0.0; half],
            cosine: vec![0.0; n], peak_phase: vec![0.0; half], side_window: vec![0.0; n],
            side_mask: vec![0.0; n], main_mask: vec![0.0; n], ramp: vec![0.0; n],
            peaks: vec![0; half], assignments: vec![0; half], peak_scratch: vec![0; half],
        };
        initialize_analysis_tables(&mut state.work, &mut state.transient_capture, &mut state.cosine, &mut state.ramp);
        state.reset(n / 8, n / 8);
        Some(state)
    }

    pub fn frame_len(&self) -> usize { self.n }
    pub fn channels(&self) -> usize { self.channels.len() }
    pub fn input_hop(&self) -> usize { self.input_hop }
    pub fn output_hop(&self) -> usize { self.output_hop }
    pub fn steps_per_frame(&self) -> usize { self.channels.len() * 5 }

    pub fn state(&self) -> VocoderState {
        VocoderState {
            input_hop: self.input_hop, internal_hop: self.internal_hop, output_hop: self.output_hop,
            active_bins: self.active_bins, side_cutoff: self.side_cutoff, frame_count: self.frame_count,
            peak_count: self.peak_count, stages: self.stages, previous_factor: self.previous_factor,
            target_factor: self.target_factor, peak_gain: self.peak_gain, relative_position: self.relative_position,
            aligned_position: self.aligned_position, detected_position: self.detected_position,
            normalization_position: self.normalization_position, independent_bins: self.independent_bins,
            side_taper_enabled: self.side_taper_enabled, side_enabled: self.side_enabled,
            bypass_propagation: self.bypass_propagation, external_position_enabled: self.external_position_enabled,
        }
    }

    /// The `0x3320c` reset for the supplied current Eff-core hops.
    pub fn reset(&mut self, input_hop: usize, output_hop: usize) {
        let n = self.n;
        assert!(input_hop > 0 && input_hop <= n / 2 && output_hop > 0 && output_hop <= n / 2);
        for channel in &mut self.channels {
            channel.input.fill(0.0);
            channel.side_history.fill(0.0);
            channel.output.fill(0.0);
            channel.current_phase.fill(0.0);
            channel.synthesis_history.fill(0.0);
            channel.held_magnitude.fill(0.0);
            channel.side_output.fill(0.0);
        }
        for frame in &mut self.history { frame.fill(0.0); }
        self.flags.fill(0);
        self.previous_flags.fill(0);
        self.peaks.fill(0);
        self.assignments.fill(0);
        self.peak_scratch.fill(0);
        self.normalization_gain.fill(0.0);
        self.normalization_history.fill(f32::from_bits(0x0080_0000));
        self.side_window.fill(0.0);
        self.input_hop = 0;
        self.internal_hop = 0;
        self.output_hop = 0;
        self.active_bins = n / 2;
        self.side_cutoff = n / 2;
        self.frame_count = 0;
        self.peak_count = 0;
        self.stages = StageTracking::default();
        self.previous_factor = 0.0;
        self.normalization_position = 0;
        self.relative_position = 0;
        self.aligned_position = 0;
        self.independent_bins = false;
        self.side_taper_enabled = true;
        self.side_enabled = false;
        self.bypass_propagation = true;
        self.external_position_enabled = true;
        self.set_factor_hops(1.0, input_hop, output_hop);
        self.side_taper_enabled = false;
    }

    /// Install the already-quantized selected core factor and hops; this does
    /// not perform the Eff/Pro factor quantization or reset the stream.
    pub fn set_factor_hops(&mut self, factor: f32, input_hop: usize, internal_hop: usize) -> bool {
        let mut state = FactorState {
            previous_factor: self.previous_factor, target_factor: self.target_factor,
            input_hop: self.input_hop, internal_hop: self.internal_hop, output_hop: self.output_hop,
            frame_count: self.frame_count, relative_transient: self.relative_position,
            aligned_transient: self.aligned_position, normalization_position: self.normalization_position,
            peak_gain: self.peak_gain, bypass_propagation: self.bypass_propagation,
            side_taper_enabled: self.side_taper_enabled,
            cosine: &self.cosine, work: &mut self.work, expected_advance: &mut self.expected_advance,
            analysis_window: &mut self.analysis_window, synthesis_window: &mut self.synthesis_window,
            normalization_window: &mut self.normalization_window, side_window: &mut self.side_window,
        };
        let rebuilt = update_factor_and_hops(&mut state, factor, input_hop, internal_hop);
        self.previous_factor = state.previous_factor;
        self.target_factor = state.target_factor;
        self.input_hop = state.input_hop;
        self.internal_hop = state.internal_hop;
        self.output_hop = state.output_hop;
        self.aligned_position = state.aligned_transient;
        self.normalization_position = state.normalization_position;
        self.peak_gain = state.peak_gain;
        self.bypass_propagation = state.bypass_propagation;
        rebuilt
    }

    /// `0x2b10c`; enabling independent bins also cancels unity bypass.
    pub fn set_independent_bins(&mut self, enabled: bool) {
        self.independent_bins = enabled;
        if enabled { self.bypass_propagation = false; }
    }

    /// `0x2b11c`, the side-transient/taper flag at `+0x549`.
    pub fn set_side_taper_enabled(&mut self, enabled: bool) {
        self.side_taper_enabled = enabled;
    }

    /// Set the `+0x54a` side-path guard. The side pointer for the individual
    /// step must also be present before the native side path executes.
    pub fn set_side_enabled(&mut self, enabled: bool) {
        self.side_enabled = enabled;
    }

    /// `0x2b0f4`/`0x2b0ec` on the selected near-unity cutoff domain.
    pub fn set_cutoffs(&mut self, active_bins: usize, side_cutoff: usize) {
        assert!(active_bins >= (self.n >> 7) && active_bins <= self.n / 2);
        assert!(side_cutoff <= self.n / 2);
        self.active_bins = active_bins;
        self.side_cutoff = side_cutoff;
    }

    /// `0x2b12c`: install the external relative transient position and its
    /// enable byte, then update the hop-aligned position.
    pub fn set_transient_position(&mut self, position: i32, enabled: bool) {
        self.relative_position = position;
        self.external_position_enabled = enabled;
        let hop = self.input_hop as i32;
        let center = hop >> 1;
        let shifted = position.wrapping_add(center);
        let remainder = if hop == 0 { shifted } else { shifted % hop };
        self.aligned_position = (remainder - center).max(-center);
    }

    /// Execute one explicit native step. Input planes are required only for
    /// stage 0/negative primes; optional side planes are consumed at stage 1;
    /// output planes are written only at stage 4. Each relevant plane has
    /// exactly the corresponding current hop length. The return is always
    /// the native output hop, not a claim that this step produced a frame.
    pub fn process_step(
        &mut self,
        raw_step: i32,
        input: &[&[f32]],
        side: Option<&[&[f32]]>,
        output: &mut [&mut [f32]],
    ) -> usize {
        let channel_count = self.channels.len();
        let step = self.stages.begin(raw_step, channel_count).expect("step outside selected channel domain");
        let ch = step.channel;
        let side_active = self.side_enabled && side.is_some();
        match step.stage {
            Stage::PrimeInput | Stage::ShiftAndForward => {
                assert_eq!(input.len(), channel_count);
                assert_eq!(input[ch].len(), self.input_hop);
                shift_input(ch, &mut self.channels[ch].input, input[ch], &mut self.relative_position);
                if step.stage == Stage::ShiftAndForward {
                    let channel = &mut self.channels[ch];
                    forward_analysis(
                        &mut self.fft, ch, &channel.input, &self.analysis_window, &self.ramp,
                        &mut channel.packed, &mut self.weighted_spectrum, &mut self.work,
                        &mut self.transient_capture,
                    );
                }
            }
            Stage::MagnitudesAndPhases => {
                let channel = &mut self.channels[ch];
                let zero = if ch == 0 {
                    Some(ChannelZeroPhase {
                        expected_advance: &self.expected_advance[..self.n / 2],
                        instantaneous_advance: &mut self.instantaneous_advance,
                        magnitude_history: &mut self.history,
                    })
                } else { None };
                extract_phase_and_magnitude(
                    ch, &mut channel.packed, &mut channel.current_phase,
                    &mut channel.magnitude, &mut self.phase_scratch, zero,
                );
                if side_active {
                    let planes = side.expect("guarded side planes");
                    assert_eq!(planes.len(), channel_count);
                    assert_eq!(planes[ch].len(), self.internal_hop);
                    analyze_side(&mut self.fft, SideAnalysis {
                        channel: ch, input: planes[ch], history: &mut channel.side_history,
                        window: &self.side_window, packed: &mut self.work,
                        side_mask: &mut self.side_mask, main_mask: &mut self.main_mask,
                    });
                }
            }
            Stage::Peaks => {
                if ch == 0 {
                    self.peak_count = identify_peaks(PeakState {
                        frame_len: self.n, input_hop: self.input_hop, active_bins: self.active_bins,
                        independent_bins: self.independent_bins, instantaneous_advance: &self.instantaneous_advance,
                        magnitude: &self.channels[0].magnitude, previous_flags: &self.previous_flags,
                        peaks: &mut self.peaks, assignments: &mut self.assignments, scratch: &mut self.peak_scratch,
                    });
                    self.frame_count = self.frame_count.wrapping_add(1);
                }
            }
            Stage::Propagate => {
                if ch == 0 && self.peak_count != 0 && self.stages.previous < self.stages.current {
                    classify_transients(TransientState {
                        frame_len: self.n, input_hop: self.input_hop, frame_count: self.frame_count,
                        previous_factor: self.previous_factor, target_factor: self.target_factor,
                        independent_bins: self.independent_bins, relative_position: self.relative_position,
                        aligned_position: self.aligned_position, external_position_enabled: self.external_position_enabled,
                        detected_position: &mut self.detected_position, bypass_propagation: &mut self.bypass_propagation,
                        peaks: &self.peaks, peak_count: self.peak_count, assignments: &self.assignments,
                        magnitude: &self.channels[0].magnitude, magnitude_history: &self.history,
                        weighted_spectrum: &self.weighted_spectrum, peak_difference: &mut self.peak_difference,
                        spectral_flux: &mut self.spectral_flux, peak_energy: &mut self.peak_energy,
                        flags: &mut self.flags, previous_flags: &self.previous_flags,
                        transient_capture: &mut self.transient_capture,
                    });
                }
                let channel = &mut self.channels[ch];
                propagate_phases(PropagationState {
                    frame_len: self.n, channel: ch, channels: channel_count, active_bins: self.active_bins,
                    side_cutoff: self.side_cutoff, side_enabled: side_active && self.side_taper_enabled,
                    frame_count: self.frame_count, current_stage: self.stages.current, previous_stage: self.stages.previous,
                    previous_factor: &mut self.previous_factor, target_factor: self.target_factor,
                    detected_position: &mut self.detected_position, peak_gain: self.peak_gain,
                    peaks: &self.peaks, peak_count: self.peak_count, assignments: &self.assignments,
                    flags: &mut self.flags, previous_flags: &mut self.previous_flags,
                    current_phase: &channel.current_phase, synthesis_history: &mut channel.synthesis_history,
                    magnitude: &channel.magnitude, held_magnitude: &mut channel.held_magnitude,
                    packed: &channel.packed, output: &mut self.inverse_packed,
                    expected_advance: &self.expected_advance, instantaneous_advance: &mut self.instantaneous_advance,
                    synthesis_peak_phase: &mut self.synthesis_peak_phase, rotation_phase: &mut self.rotation_phase,
                    rotation_cosine: &mut self.rotation_cosine, rotation_sine: &mut self.rotation_sine,
                    peak_phase: &mut self.peak_phase, sincos_angles: &mut self.sincos_angles,
                    sincos_sine: &mut self.sincos_sine, sincos_cosine: &mut self.sincos_cosine,
                    spectral_flux: &self.spectral_flux, normalization_window: &self.normalization_window,
                    side_mask: &mut self.side_mask, main_mask: &mut self.main_mask,
                });
                if side_active {
                    apply_side_masks(&mut self.work, &mut self.inverse_packed, &self.side_mask, &self.main_mask);
                }
            }
            Stage::InverseAndOverlap => {
                assert_eq!(output.len(), channel_count);
                assert_eq!(output[ch].len(), self.output_hop);
                let channel = &mut self.channels[ch];
                inverse_and_overlap(&mut self.fft, InverseFrame {
                    channel: ch, bypass_propagation: self.bypass_propagation, input_packed: &channel.packed,
                    synthesis_packed: &mut self.inverse_packed, synthesis_window: &self.synthesis_window,
                    output_overlap: &mut channel.output, normalization_window: &self.normalization_window,
                    normalization_history: &mut self.normalization_history, normalization_gain: &mut self.normalization_gain,
                    normalization_position: &mut self.normalization_position,
                    output_hop: self.output_hop, internal_hop: self.internal_hop,
                });
                if side_active {
                    inverse_side_and_advance(&mut self.fft, SideInverse {
                        packed: &mut self.work, window: &self.side_window, side_overlap: &mut channel.side_output,
                        side_history: &mut channel.side_history, output_overlap: &mut channel.output,
                        bypass_propagation: self.bypass_propagation, output_hop: self.output_hop,
                        internal_hop: self.internal_hop,
                    });
                }
                finish_output(ch, channel_count, self.normalization_position, self.internal_hop,
                              &mut channel.output, output[ch], &mut self.side_window);
            }
        }
        self.stages.finish(raw_step);
        self.output_hop
    }

    /// Direct-adapter form with the same main work planes used as stage-0
    /// input and stage-4 output. The vocoder reads side planes only at stage 1;
    /// any Eff stereo conversion remains the caller's in-place operation.
    /// Plane-reference arrays are stack allocated, not copied sample buffers;
    /// each plane may be longer than the hop consumed or produced on that step.
    pub fn process_step_in_place(
        &mut self,
        raw_step: i32,
        main: &mut [&mut [f32]],
        side: Option<&mut [&mut [f32]]>,
    ) -> usize {
        let channels = self.channels.len();
        assert_eq!(main.len(), channels);
        if let Some(planes) = side.as_ref() {
            assert_eq!(planes.len(), channels);
        }
        let step = decode_step(raw_step, channels).expect("step outside selected channel domain");
        match step.stage {
            Stage::PrimeInput | Stage::ShiftAndForward => match main {
                [left] => {
                    let inputs: [&[f32]; 1] = [&left[..self.input_hop]];
                    self.process_step(raw_step, &inputs, None, &mut [])
                }
                [left, right] => {
                    let inputs: [&[f32]; 2] = [&left[..self.input_hop], &right[..self.input_hop]];
                    self.process_step(raw_step, &inputs, None, &mut [])
                }
                _ => unreachable!(),
            },
            Stage::MagnitudesAndPhases => match side {
                None => self.process_step(raw_step, &[], None, &mut []),
                Some([left]) => {
                    let inputs: [&[f32]; 1] = [&left[..self.internal_hop]];
                    self.process_step(raw_step, &[], Some(&inputs), &mut [])
                }
                Some([left, right]) => {
                    let inputs: [&[f32]; 2] = [&left[..self.internal_hop], &right[..self.internal_hop]];
                    self.process_step(raw_step, &[], Some(&inputs), &mut [])
                }
                _ => unreachable!(),
            },
            Stage::InverseAndOverlap => {
                let marker: Option<&[&[f32]]> = if side.is_some() { Some(&[]) } else { None };
                match main {
                    [left] => {
                        let mut outputs: [&mut [f32]; 1] = [&mut left[..self.output_hop]];
                        self.process_step(raw_step, &[], marker, &mut outputs)
                    }
                    [left, right] => {
                        let mut outputs: [&mut [f32]; 2] =
                            [&mut left[..self.output_hop], &mut right[..self.output_hop]];
                        self.process_step(raw_step, &[], marker, &mut outputs)
                    }
                    _ => unreachable!(),
                }
            }
            _ => {
                let marker: Option<&[&[f32]]> = if side.is_some() { Some(&[]) } else { None };
                self.process_step(raw_step, &[], marker, &mut [])
            }
        }
    }
}
