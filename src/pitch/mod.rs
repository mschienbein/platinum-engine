//! Recovered PitchShiftUnit primitives and adapter scheduling.
//!
//! The selected one-group AU/Pro/Eff pipeline has bounded synthetic-component
//! comparisons, not general Elastique or GUI file-parity validation. The host
//! supplies source exhaustion/zero padding and owns the surrounding FP mode.

pub mod fft;
pub mod interpolate;
pub(crate) mod psola;
pub mod spectral;
pub mod vocoder;

/// The two modes selected by the Platinum Notes controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PitchMode {
    PreserveTempo,
    Turntable,
}

/// Base FFT size chosen after the AU converts its sample rate to f32.
pub fn fft_size_for_rate(sample_rate: f32) -> Option<usize> {
    if !sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&sample_rate) {
        return None;
    }
    Some(if sample_rate < 32_000.0 { 2048 }
    else if sample_rate < 64_000.0 { 4096 }
    else if sample_rate < 128_000.0 { 8192 }
    else if sample_rate < 256_000.0 { 16384 }
    else { 32768 })
}

/// Result of the Eff-core hop quantizer at `0x32a6c`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuantizedFactor {
    pub input_hop: i32,
    pub output_hop: i32,
    pub factor: f32,
}

/// Apply the original quantizer to an already formed f32 product. `groups`
/// and `maximum_single_group` are the current core state, not mode defaults.
/// The comparison thresholds are binary64 in the ARM64 implementation.
pub fn quantize_factor(
    fft_size: usize,
    product: f32,
    groups: i32,
    maximum_single_group: f32,
) -> Option<QuantizedFactor> {
    if !matches!(fft_size, 2048 | 4096 | 8192 | 16384 | 32768)
        || !product.is_finite() || product <= 0.0 || groups < 1
        || !maximum_single_group.is_finite() || maximum_single_group <= 0.0
    {
        return None;
    }
    let p = f64::from(product);
    let shift = if groups > 1 || maximum_single_group < product {
        5
    } else if p <= 2.2 {
        3
    } else if p <= 3.2 {
        4
    } else if p <= 5.2 {
        5
    } else if product <= 10.0 {
        6
    } else {
        2
    };
    let input_hop = (fft_size >> shift) as i32;
    let output_hop = (product * input_hop as f32) as i32;
    let factor = output_hop as f32 / input_hop as f32;
    Some(QuantizedFactor { input_hop, output_hop, factor })
}

/// The two pointer values passed to Pro `0x3dbd8` after successful
/// single-group quantization, together with the AU's Buffer Size factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectedFactors {
    pub initial_product: f32,
    pub quantized: QuantizedFactor,
    pub first: f32,
    pub second: f32,
    pub au_mode_factor: f32,
}

/// Evaluate the selected AU mode's single-group factor arithmetic. Returns
/// `None` when the product is outside the Pro instance's 0.1..=6.0
/// single-group domain; this does not implement multi-group state changes.
pub fn selected_single_group_factors(
    sample_rate: f32,
    ratio: f32,
    mode: PitchMode,
) -> Option<SelectedFactors> {
    let fft_size = fft_size_for_rate(sample_rate)?;
    if !ratio.is_finite() || ratio <= 0.0 { return None; }
    let (mut first, mut second) = match mode {
        PitchMode::PreserveTempo => (1.0, ratio),
        PitchMode::Turntable => (1.0 / ratio, ratio),
    };
    let initial_product = second * first;
    if !initial_product.is_finite() || !(0.1..=6.0).contains(&initial_product) { return None; }
    let quantized = quantize_factor(fft_size, initial_product, 1, 6.0)?;
    let au_mode_factor = match mode {
        PitchMode::PreserveTempo => {
            second = quantized.factor / first;
            1.0
        }
        PitchMode::Turntable => {
            first = quantized.factor * (1.0 / second);
            first
        }
    };
    Some(SelectedFactors { initial_product, quantized, first, second, au_mode_factor })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StereoLengthMismatch;

/// Eff-core `0x31da0`, for its enabled two-channel coupling branch.
pub fn couple_stereo(left: &mut [f32], right: &mut [f32]) -> Result<(), StereoLengthMismatch> {
    if left.len() != right.len() { return Err(StereoLengthMismatch); }
    for (l, r) in left.iter_mut().zip(right) {
        let a = *l * f32::from_bits(0x3f19_999a);
        let b = *r * f32::from_bits(0x3ecc_cccd);
        *l = a + b;
        *r = a - b;
    }
    Ok(())
}

/// Eff-core `0x31f70`, for its enabled two-channel coupling branch.
pub fn uncouple_stereo(left: &mut [f32], right: &mut [f32]) -> Result<(), StereoLengthMismatch> {
    if left.len() != right.len() { return Err(StereoLengthMismatch); }
    for (l, r) in left.iter_mut().zip(right) {
        let a = *l;
        let b = *r;
        *l = (a + b) / f32::from_bits(0x3f99_999a);
        *r = (a - b) / f32::from_bits(0x3f4c_cccd);
    }
    Ok(())
}

/// Explicit state read by the direct-adapter demand routine `0x36300`.
#[derive(Clone, Copy, Debug)]
pub struct DirectDemandState {
    pub special_mode: bool,
    pub target: i32,
    pub produced: i32,
    pub current_factor: f32,
    pub previous_factor: f32,
    pub output_hop: i32,
    pub input_hop: i32,
    pub ordinary_input: i32,
}

/// Input frames needed for one future core iteration. The callback implements
/// the Pro/Eff core's actual `+0x180` input-needed predicate; in special mode
/// it is queried again for each internal step beginning at zero.
pub fn direct_input_demand(
    state: DirectDemandState,
    iteration: i32,
    mut accepts_input: impl FnMut(i32) -> bool,
) -> i32 {
    if !accepts_input(iteration) { return 0; }
    if !state.special_mode { return state.ordinary_input; }
    let difference = state.produced.wrapping_sub(state.target);
    if difference > 0 { return 0; }
    // The native FCSEL selects the second operand for unordered comparison.
    let factor = if state.current_factor > state.previous_factor {
        state.current_factor
    } else {
        state.previous_factor
    };
    // Despite the decompiler's f32 rendering, 0x363d4–0x363f0 use f64.
    let reciprocal = 1.0f64 / f64::from(factor);
    let mut phase = f64::from(difference);
    let mut total = 0i32;
    let mut step = 0i32;
    loop {
        if accepts_input(step) { total = total.wrapping_add(state.input_hop); }
        phase += reciprocal * f64::from(state.output_hop);
        step = step.wrapping_add(1);
        if (phase as i32) >= 1 { return total; }
    }
}

/// The control-flow decision in one `0x3e370` Pro-core step. Signal work and
/// the persistent state transitions remain the responsibility of the core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProStepAction {
    Split,
    Analyse,
    Synthesize,
    PushAndAdvance,
    Advance,
    Retrieve,
    Finalize,
    Blocked,
}

/// Return the native next-step index and action for the current scheduling
/// fields. `nested_start` is Pro `+0x154`, `nested_steps` is `+0x14c`, and
/// `blocked` is the FIFO-underflow flag `+0x15c`.
pub fn pro_step_action(
    step: i32,
    total_steps: i32,
    nested_start: i32,
    nested_steps: i32,
    blocked: bool,
) -> (i32, ProStepAction) {
    let incremented = step.wrapping_add(1);
    let next = if total_steps == 0 { incremented } else { incremented.checked_rem(total_steps).unwrap_or(0) };
    let action = match step {
        0 => ProStepAction::Split,
        1 => ProStepAction::Analyse,
        2 => ProStepAction::Synthesize,
        _ if blocked => ProStepAction::Blocked,
        _ if step < total_steps.wrapping_sub(1) => {
            let relative = step.wrapping_sub(nested_start);
            let period = nested_steps.wrapping_add(1);
            let quotient = relative.checked_div(period).unwrap_or(if period == -1 { i32::MIN } else { 0 });
            let remainder = relative.wrapping_sub(quotient.wrapping_mul(period));
            if remainder == 0 { ProStepAction::PushAndAdvance }
            else if remainder == nested_steps { ProStepAction::Retrieve }
            else { ProStepAction::Advance }
        }
        _ => ProStepAction::Finalize,
    };
    (next, action)
}

/// Count the nested Eff operations selected by Pro `0x3df14`. The callback
/// returns the current Eff step count for a full (`true`, slot `+0x170`) or
/// reduced (`false`, slot `+0x168`) internal frame.
pub fn pro_nested_step_count(
    input_hop: i32,
    processing_hop: i32,
    phase: i32,
    groups: i32,
    reverse_phase: bool,
    mut eff_steps: impl FnMut(bool) -> i32,
) -> i32 {
    let span = processing_hop.max(input_hop);
    let iterations = span.checked_div(input_hop).unwrap_or(if input_hop == -1 { i32::MIN } else { 0 });
    let mut total = 0i32;
    for frame in 0..iterations {
        let full = if reverse_phase {
            frame.wrapping_neg() == phase
        } else {
            let position = phase.wrapping_add(frame);
            let quotient = position.checked_div(groups).unwrap_or(if groups == -1 { i32::MIN } else { 0 });
            position == quotient.wrapping_mul(groups)
        };
        total = total.wrapping_add(eff_steps(full)).wrapping_add(1);
    }
    total
}

/// Pro `0x3ee0c` input-needed predicate, including the queued splitter-frame
/// check for the first matching internal group in this future iteration.
pub fn pro_accepts_input(
    iteration: i32,
    input_hop: i32,
    processing_hop: i32,
    phase: i32,
    groups: i32,
    always: bool,
    queued_frames: i32,
) -> bool {
    if always { return true; }
    let span = processing_hop.max(input_hop);
    let count = span.checked_div(input_hop).unwrap_or(if input_hop == -1 { i32::MIN } else { 0 });
    let mut frame = count.wrapping_mul(iteration);
    let end = count.wrapping_mul(iteration.wrapping_add(1));
    if frame >= end { return false; }
    let mut remaining = count;
    loop {
        let position = phase.wrapping_add(frame);
        let quotient = position.checked_div(groups).unwrap_or(if groups == -1 { i32::MIN } else { 0 });
        if position == quotient.wrapping_mul(groups) {
            let threshold = processing_hop.wrapping_add(processing_hop.wrapping_mul(iteration));
            return queued_frames < threshold;
        }
        frame = frame.wrapping_add(1);
        remaining = remaining.wrapping_sub(1);
        if remaining == 0 { return false; }
    }
}

/// Position fields updated by the final Pro-core step. These correspond to
/// offsets `+0x80`, `+0x88`, `+0x90` and `+0x30` respectively.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProPositions {
    pub base: f64,
    pub actual: f64,
    pub target: f64,
    pub peak_error: f32,
}

impl ProPositions {
    /// `0x32164` followed by the position updates at `0x3e6a0–0x3e6ec`.
    /// The returned f32 is the effective factor written to Pro `+0x10`.
    pub fn advance(&mut self, output_hop: i32, desired_factor: f32) -> f32 {
        let factor = self.correct(self.actual - self.target, output_hop, desired_factor);
        let desired_step = output_hop as f32 / desired_factor;
        let actual_step = output_hop as f32 / factor;
        self.target += f64::from(desired_step);
        self.base += f64::from(desired_step);
        self.actual += f64::from(actual_step);
        factor
    }

    /// Update the positional factor state on a successful Pro setter
    /// (`0x3de4c`/`0x3de88`). `smooth` is the selected branch condition after
    /// accounting for the setter's transition flag and first-frame flag.
    /// This adjusts positions only; it does not reset signal history or FIFOs.
    pub fn set_factor_position(
        &mut self,
        frame_size: i32,
        previous_input_hop: i32,
        input_hop: i32,
        output_hop: i32,
        desired_factor: f32,
        smooth: bool,
    ) -> f32 {
        let span = (frame_size >> 1).wrapping_sub(previous_input_hop).wrapping_add(input_hop) as f32;
        let reciprocal = 1.0 / desired_factor;
        let displacement = reciprocal * span - span;
        self.target = self.base - f64::from(displacement);
        if smooth {
            self.correct(self.actual - self.target, output_hop, desired_factor)
        } else {
            self.actual = self.target;
            desired_factor
        }
    }

    fn correct(&mut self, difference: f64, output_hop: i32, desired_factor: f32) -> f32 {
        if difference as i32 == 0 {
            self.peak_error = 0.0;
            desired_factor
        } else {
            let error = (difference.abs() as f32) * 1.25;
            self.peak_error = if self.peak_error > error { self.peak_error } else { error };
            let nominal = output_hop.wrapping_mul(3) as f32;
            let window = if self.peak_error < nominal { nominal } else { self.peak_error };
            let adjustment = ((f64::from(window) + difference) / f64::from(window)).clamp(0.5, 2.0);
            (adjustment * f64::from(desired_factor)) as f32
        }
    }
}

// This narrow boundary represents the actual Pro direct-adapter calls. It is
// intentionally private: there is no substitute production processor behind it.
trait ProDirectAdapter {
    fn output_hop(&self) -> i32;
    fn current_factor(&self) -> f32;
    fn startup_input(&self) -> i32;
    fn startup_discard(&self) -> i32;
    fn needed_input(&self, iteration: i32) -> i32;
    fn startup(&mut self, work: &mut [Vec<f32>], input_frames: usize) -> i32;
    fn step(&mut self, work: &mut [Vec<f32>], input_frames: usize) -> i32;
    fn read_position(&self) -> i64;
    fn set_factors(&mut self, first: &mut f32, second: &mut f32, selector: i32) -> bool;
    fn reset(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SchedulingError {
    InvalidPlanes,
    InsufficientInput,
    InvalidCoreOutput,
    CoreStartup(i32),
    PullFailed(i32),
    PullMadeNoProgress,
}

// Separate circular queues preserve the native AU and adapter boundaries
// without moving an unread tail on each callback.
struct PlanarFifo {
    planes: Vec<std::collections::VecDeque<f32>>,
}

impl PlanarFifo {
    fn new(channels: usize, capacity: usize) -> Self {
        Self { planes: (0..channels).map(|_| std::collections::VecDeque::with_capacity(capacity)).collect() }
    }
    fn len(&self) -> usize { self.planes[0].len() }
    fn clear(&mut self) {
        for plane in &mut self.planes { plane.clear(); }
    }
    fn append(&mut self, planes: &[Vec<f32>], start: usize, count: usize) -> Result<(), SchedulingError> {
        if planes.len() != self.planes.len() || planes.iter().any(|p| start > p.len() || count > p.len() - start) {
            return Err(SchedulingError::InvalidPlanes);
        }
        for (dest, source) in self.planes.iter_mut().zip(planes) {
            dest.extend(source[start..start + count].iter().copied());
        }
        Ok(())
    }
    fn take_vecs(&mut self, planes: &mut [Vec<f32>], count: usize) -> Result<(), SchedulingError> {
        if planes.len() != self.planes.len() { return Err(SchedulingError::InvalidPlanes); }
        if count > self.len() { return Err(SchedulingError::InsufficientInput); }
        for (dest, source) in planes.iter_mut().zip(&mut self.planes) {
            let (first, second) = source.as_slices();
            let split = count.min(first.len());
            dest.clear();
            dest.extend_from_slice(&first[..split]);
            dest.extend_from_slice(&second[..count - split]);
            drop(source.drain(..count));
        }
        Ok(())
    }
    // `0x12278` is all-or-nothing: on underflow neither the destination nor
    // the FIFO cursor changes. The native work buffer retains its old tail.
    fn try_take_prefix(&mut self, planes: &mut [Vec<f32>], count: usize) -> bool {
        assert_eq!(planes.len(), self.planes.len());
        if count > self.len() { return false; }
        for (dest, source) in planes.iter_mut().zip(&mut self.planes) {
            assert!(dest.len() >= count);
            let (first, second) = source.as_slices();
            let split = count.min(first.len());
            dest[..split].copy_from_slice(&first[..split]);
            dest[split..count].copy_from_slice(&second[..count - split]);
            drop(source.drain(..count));
        }
        true
    }
    fn take_slices(&mut self, planes: &mut [&mut [f32]], count: usize) -> Result<(), SchedulingError> {
        if planes.len() != self.planes.len() || planes.iter().any(|p| p.len() < count) {
            return Err(SchedulingError::InvalidPlanes);
        }
        if count > self.len() { return Err(SchedulingError::InsufficientInput); }
        for (dest, source) in planes.iter_mut().zip(&mut self.planes) {
            let (first, second) = source.as_slices();
            let split = count.min(first.len());
            dest[..split].copy_from_slice(&first[..split]);
            dest[split..count].copy_from_slice(&second[..count - split]);
            drop(source.drain(..count));
        }
        Ok(())
    }
}

// Native Pro/Eff-core slots called by the selected direct-adapter path; this
// is not a general plugin interface.
trait ProSignalCore {
    fn output_hop(&self) -> i32;                // +0xf0
    fn current_factor(&self) -> f32;           // +0x118
    fn initial_input(&self) -> i32;              // +0xc8
    fn startup_discard(&self) -> i32;            // +0x148
    fn accepts_input(&self, iteration: i32) -> bool; // +0x180
    fn ordinary_input(&self) -> i32;             // +0xe8
    fn input_block(&self) -> i32;                // +0xe0
    fn input_hop(&self) -> i32;                  // +0xd0
    fn internal_output_hop(&self) -> i32;        // +0xd8
    fn group_count(&self) -> i32;                // +0x130
    fn position_factor(&self) -> f32;            // +0x120
    fn initial_position(&self) -> i64;           // +0x88
    fn read_position(&self) -> i64;              // +0x90
    fn step_count(&self, extra: i32) -> i32;     // +0x50
    fn step_index(&self) -> i32;                 // +0x58
    fn prime(&mut self, work: &mut [Vec<f32>]);  // +0x48
    fn process_step(&mut self, work: &mut [Vec<f32>], side: Option<&mut [Vec<f32>]>, index: i32) -> i32; // +0x38
    fn set_factors(&mut self, first: &mut f32, second: &mut f32, selector: i32) -> bool;
    fn reset(&mut self);
}

// Eff-core state for the selected PN Pro call path: one group, stereo mode 0,
// and a nested scalar stretch of exactly 1.0. 0x329ac's other group modes are
// not selected by the current controller and are intentionally not exposed.
struct EffCore {
    rate: f32,
    frame: usize,
    channels: usize,
    vocoder: vocoder::PhaseVocoder,
    interpolator: interpolate::HermiteInterpolator,
    input_hop: i32,
    output_hop: i32,
    previous_input_hop: i32,
    source_position: f64,
    position_baseline: i32,
    position_offset: i32,
    positions: ProPositions,
    effective_factor: f32,
    step: i32,
    first_frame: bool,
    bypass_final_interpolator: bool,
    maximum_cutoff_hz: f32,
    active_bins: usize,
    side_cutoff: usize,
}

impl EffCore {
    fn new(rate: f32, channels: usize) -> Option<Self> {
        let frame = fft_size_for_rate(rate)?;
        if !(1..=2).contains(&channels) { return None; }
        let hop = (frame >> 3) as i32;
        Some(Self {
            rate, frame, channels,
            vocoder: vocoder::PhaseVocoder::new(frame, channels)?,
            interpolator: interpolate::HermiteInterpolator::new(channels).ok()?,
            input_hop: hop, output_hop: hop, previous_input_hop: hop,
            source_position: -((frame >> 1) as f64), position_baseline: 0,
            position_offset: 0, positions: ProPositions::default(), effective_factor: 1.0,
            step: 0, first_frame: true, bypass_final_interpolator: false,
            maximum_cutoff_hz: rate * 0.5, active_bins: frame >> 1, side_cutoff: frame >> 1,
        })
    }

    // 0x3d8e0 selects the Eff no-resampling flag and the 1500 Hz side cutoff
    // after the direct adapter resets the nested core.
    fn configure_pro(&mut self) {
        self.bypass_final_interpolator = true;
        self.set_side_cutoff_hz(1500.0);
    }

    fn set_side_cutoff_hz(&mut self, frequency: f32) {
        let normalized = (frequency / self.rate).clamp(0.0, 1.0);
        self.side_cutoff = (normalized * self.frame as f32) as usize;
        self.vocoder.set_cutoffs(self.active_bins, self.side_cutoff);
    }

    // Eff `0x33084` sets the retained cutoff only when flag bit 0 is clear.
    // It converts the supplied Hz to bins with binary32 divide/multiply, then
    // `0x2b0f4` clamps the count at N/2.
    fn set_active_cutoff_hz(&mut self, frequency: f32, update_maximum: bool) {
        if update_maximum { self.maximum_cutoff_hz = frequency; }
        let normalized = (frequency / self.rate).clamp(0.0, 1.0);
        self.active_bins = ((normalized * self.frame as f32) as usize).min(self.frame >> 1);
        self.vocoder.set_cutoffs(self.active_bins, self.side_cutoff);
    }

    fn set_side_enabled(&mut self, enabled: bool) { self.vocoder.set_side_enabled(enabled); }
    fn set_side_taper_enabled(&mut self, enabled: bool) { self.vocoder.set_side_taper_enabled(enabled); }
    fn side_taper_enabled(&self) -> bool { self.vocoder.state().side_taper_enabled }

    fn stereo_forward(planes: &mut [Vec<f32>], count: usize) {
        if planes.len() == 2 {
            let (left, right) = planes.split_at_mut(1);
            couple_stereo(&mut left[0][..count], &mut right[0][..count]).expect("same hop");
        }
    }

    fn stereo_inverse(planes: &mut [Vec<f32>], count: usize) {
        if planes.len() == 2 {
            let (left, right) = planes.split_at_mut(1);
            uncouple_stereo(&mut left[0][..count], &mut right[0][..count]).expect("same hop");
        }
    }

    // The native main input/output aliases. The vocoder reads main only in
    // phase 0 and writes it only in phase 4, so these borrows do not copy audio.
    fn vocoder_step(&mut self, index: i32, work: &mut [Vec<f32>], side: Option<&mut [Vec<f32>]>) {
        assert_eq!(work.len(), self.channels);
        match work {
            [main] => {
                let mut main = [main.as_mut_slice()];
                match side {
                    Some([aux]) => {
                        let mut aux = [aux.as_mut_slice()];
                        self.vocoder.process_step_in_place(index, &mut main, Some(&mut aux));
                    }
                    None => { self.vocoder.process_step_in_place(index, &mut main, None); }
                    _ => panic!("native side channel count"),
                }
            }
            [left, right] => {
                let mut main = [left.as_mut_slice(), right.as_mut_slice()];
                match side {
                    Some([aux_left, aux_right]) => {
                        let mut aux = [aux_left.as_mut_slice(), aux_right.as_mut_slice()];
                        self.vocoder.process_step_in_place(index, &mut main, Some(&mut aux));
                    }
                    None => { self.vocoder.process_step_in_place(index, &mut main, None); }
                    _ => panic!("native side channel count"),
                }
            }
            _ => panic!("native main channel count"),
        }
    }
}

impl ProSignalCore for EffCore {
    fn output_hop(&self) -> i32 { self.output_hop }
    fn current_factor(&self) -> f32 { self.effective_factor }
    fn initial_input(&self) -> i32 { (self.frame >> 1) as i32 }
    fn startup_discard(&self) -> i32 {
        (self.frame >> 1) as i32 + if self.bypass_final_interpolator { 0 } else { 2 }
    }
    fn accepts_input(&self, _: i32) -> bool { true }
    fn ordinary_input(&self) -> i32 { self.input_hop }
    fn input_block(&self) -> i32 { self.input_hop }
    fn input_hop(&self) -> i32 { self.input_hop }
    fn internal_output_hop(&self) -> i32 { self.output_hop }
    fn group_count(&self) -> i32 { 1 }
    fn position_factor(&self) -> f32 { 1.0 }
    fn initial_position(&self) -> i64 { self.source_position as i64 }
    fn read_position(&self) -> i64 { (self.source_position as i64).wrapping_add(i64::from(self.position_offset)) }
    fn step_count(&self, extra: i32) -> i32 {
        assert_eq!(extra, 0, "selected PN direct-adapter step query");
        (self.channels * 5) as i32
    }
    fn step_index(&self) -> i32 { self.step }

    // Eff `0x32f78`: one coupled input block primes each channel once.
    fn prime(&mut self, work: &mut [Vec<f32>]) {
        Self::stereo_forward(work, self.input_hop as usize);
        self.source_position += f64::from(self.input_hop);
        for channel in 0..self.channels {
            self.vocoder_step(-1 - channel as i32, work, None);
        }
    }

    // Eff `0x32b88`, selected one-group mode. The direct adapter passes its
    // persistent main work buffer as both native input and output.
    fn process_step(&mut self, work: &mut [Vec<f32>], mut side: Option<&mut [Vec<f32>]>, index: i32) -> i32 {
        assert_eq!(index, self.step);
        if index == 0 {
            self.source_position += f64::from(self.input_hop);
            Self::stereo_forward(work, self.input_hop as usize);
            if let Some(aux) = side.as_deref_mut() {
                Self::stereo_forward(aux, self.output_hop as usize);
            }
        }
        self.vocoder_step(index, work, side);
        let steps = (self.channels * 5) as i32;
        self.step = (index + 1) % steps;
        if self.step != 0 { return 0; }
        self.effective_factor = self.positions.advance(self.output_hop, 1.0);
        self.first_frame = false;
        Self::stereo_inverse(work, self.output_hop as usize);
        if self.bypass_final_interpolator {
            self.output_hop
        } else {
            self.interpolator.process_in_place(work, self.output_hop as usize, self.effective_factor)
                .expect("native Eff interpolation work capacity") as i32
        }
    }

    // Pro `0x3dbd8` calls the nested Eff setter with scalar 1.0 and selector 0.
    // Both selected controller modes and the detector's finite tuning range
    // remain in the one-group product domain.
    fn set_factors(&mut self, first: &mut f32, second: &mut f32, selector: i32) -> bool {
        if selector != 0 || *second != 1.0 { return false; }
        let product = *second * *first;
        if !product.is_finite() || !(0.1..=6.0).contains(&product) { return false; }
        let q = quantize_factor(self.frame, product, 1, 6.0).expect("selected Eff FFT size");
        if self.first_frame {
            self.source_position = -((self.frame >> 1) as f64);
            self.previous_input_hop = q.input_hop;
        }
        self.input_hop = q.input_hop;
        self.output_hop = q.output_hop;
        self.vocoder.set_factor_hops(q.factor, q.input_hop as usize, q.output_hop as usize);
        let requested_cutoff = (self.rate * 0.5) * (1.0 / *second);
        let cutoff = if self.maximum_cutoff_hz < requested_cutoff { self.maximum_cutoff_hz } else { requested_cutoff };
        self.set_active_cutoff_hz(cutoff, false);
        self.vocoder.set_independent_bins(false);
        *first = (q.factor * (1.0 / *second)) * 1.0;
        let half = (self.frame >> 1) as i32;
        let span = q.input_hop.wrapping_sub(self.previous_input_hop).wrapping_add(half) as f32;
        let read_correction = span - (1.0 / q.factor) * half as f32;
        if self.first_frame {
            self.position_baseline = read_correction as i32;
            self.position_offset = 0;
        } else {
            self.position_offset = (read_correction - self.position_baseline as f32) as i32;
        }
        self.effective_factor = self.positions.set_factor_position(
            self.frame as i32, self.previous_input_hop, q.input_hop, q.output_hop, 1.0, false
        );
        true
    }

    fn reset(&mut self) {
        let hop = (self.frame >> 3) as i32;
        self.input_hop = hop;
        self.output_hop = hop;
        self.previous_input_hop = hop;
        self.source_position = -((self.frame >> 1) as f64);
        self.position_baseline = 0;
        self.position_offset = 0;
        self.positions = ProPositions::default();
        self.effective_factor = 1.0;
        self.step = 0;
        self.first_frame = true;
        self.bypass_final_interpolator = false;
        self.maximum_cutoff_hz = self.rate * 0.5;
        self.active_bins = self.frame >> 1;
        self.side_cutoff = self.frame >> 1;
        self.interpolator.reset();
        self.vocoder.reset(hop as usize, hop as usize);
    }
}

// Selected normal direct-adapter path, including the nested Eff side input.
// Work lengths are the corresponding native initialization buffer capacities.
struct ProDirect<C> {
    core: C,
    input: PlanarFifo,
    side_input: PlanarFifo,
    scratch: Vec<Vec<f32>>,
    side_scratch: Vec<Vec<f32>>,
    side_active: bool,
    started: bool,
    produced: i32,
    emitted: i32,
    read_offset: i32,
}

impl<C: ProSignalCore> ProDirect<C> {
    fn new(core: C, channels: usize, work_frames: usize, side_frames: usize) -> Self {
        assert!((1..=2).contains(&channels));
        assert!(work_frames >= core.input_block() as usize);
        assert!(side_frames >= core.internal_output_hop() as usize);
        let emitted = core.startup_discard().wrapping_neg();
        Self {
            core, input: PlanarFifo::new(channels, work_frames), side_input: PlanarFifo::new(channels, side_frames),
            scratch: (0..channels).map(|_| vec![0.0; work_frames]).collect(),
            side_scratch: (0..channels).map(|_| vec![0.0; side_frames]).collect(),
            side_active: false, started: false, produced: 0, emitted, read_offset: 0,
        }
    }

    fn take_input_block(&mut self) -> bool {
        let count = self.core.input_block() as usize;
        self.input.try_take_prefix(&mut self.scratch, count)
    }

    // `0x35ccc` selects a complete new input block when one is available;
    // otherwise the existing scratch contents remain in place.
    fn push_input(&mut self, input: &[Vec<f32>], count: usize) {
        self.started = true;
        self.input.append(input, 0, count).expect("native direct-adapter input planes");
        self.take_input_block();
    }

    // `0x35d34` feeds one side hop before pushing the main input. Side state
    // remains active for subsequent individual step calls until reset.
    fn push_with_side(&mut self, input: &[Vec<f32>], count: usize, side: &[Vec<f32>]) {
        self.push_with_side_at(input, count, side, 0);
    }

    fn push_with_side_at(&mut self, input: &[Vec<f32>], count: usize, side: &[Vec<f32>], offset: usize) {
        let side_frames = self.core.internal_output_hop() as usize;
        self.side_input.append(side, offset, side_frames).expect("native side-input planes");
        self.side_input.try_take_prefix(&mut self.side_scratch, side_frames);
        self.side_active = true;
        self.push_input(input, count);
    }

    fn advance_one(&mut self) {
        let index = self.core.step_index();
        let side = if self.side_active { Some(self.side_scratch.as_mut_slice()) } else { None };
        self.produced = self.core.process_step(&mut self.scratch, side, index);
    }

    fn process_steps(&mut self) {
        let count = self.core.step_count(0);
        for _ in 0..count {
            self.advance_one();
        }
    }

    // `0x3747c`: synchronous side push, complete step sequence and retrieve.
    fn step_with_side(&mut self, input: &[Vec<f32>], count: usize, side: &[Vec<f32>], output: &mut [Vec<f32>]) -> i32 {
        self.push_with_side(input, count, side);
        self.process_steps();
        self.retrieve(output)
    }

    fn retrieve(&mut self, output: &mut [Vec<f32>]) -> i32 {
        assert!(self.produced >= 0);
        let count = self.produced as usize;
        assert_eq!(output.len(), self.scratch.len());
        for (dest, source) in output.iter_mut().zip(&self.scratch) {
            assert!(count <= source.len());
            dest.clear();
            dest.extend_from_slice(&source[..count]);
        }
        self.emitted = self.emitted.wrapping_add(self.produced);
        self.produced
    }

    fn retrieve_at(&mut self, output: &mut [Vec<f32>], offset: usize) -> i32 {
        assert!(self.produced >= 0);
        let count = self.produced as usize;
        assert_eq!(output.len(), self.scratch.len());
        for (dest, source) in output.iter_mut().zip(&self.scratch) {
            assert!(count <= source.len() && offset <= dest.len() && count <= dest.len() - offset);
            dest[offset..offset + count].copy_from_slice(&source[..count]);
        }
        self.emitted = self.emitted.wrapping_add(self.produced);
        self.produced
    }

    fn starting_position(&self) -> i64 {
        self.core.initial_position().wrapping_add(i64::from(self.read_offset))
    }
}

impl<C: ProSignalCore> ProDirectAdapter for ProDirect<C> {
    fn output_hop(&self) -> i32 { self.core.output_hop() }
    fn current_factor(&self) -> f32 { self.core.current_factor() }
    fn startup_input(&self) -> i32 { if self.started { 0 } else { self.core.initial_input() } }
    fn startup_discard(&self) -> i32 { self.core.startup_discard() }
    fn needed_input(&self, iteration: i32) -> i32 {
        if self.core.accepts_input(iteration) { self.core.ordinary_input() } else { 0 }
    }

    // `0x364f8`: prime all but the last input block, run the core's full step
    // sequence on the last work block, then compute the f32 read correction.
    fn startup(&mut self, work: &mut [Vec<f32>], input_frames: usize) -> i32 {
        if self.started { return 0; }
        let discard = self.core.startup_discard();
        let input_block = self.core.input_block();
        let blocks = self.core.initial_input() / input_block;
        self.emitted = discard.wrapping_neg();
        self.input.append(work, 0, input_frames).expect("native direct-adapter input planes");
        for _ in 1..blocks {
            if self.take_input_block() { self.core.prime(&mut self.scratch); }
        }
        self.take_input_block();
        self.process_steps();
        let produced = self.retrieve(work);
        let denominator = self.core.position_factor() * self.core.internal_output_hop() as f32;
        let hop_ratio = self.core.input_hop() as f32 / self.core.group_count() as f32;
        let delta = discard.wrapping_sub(produced);
        let correction = (-(delta as f32) / denominator) * hop_ratio;
        self.read_offset = (correction - self.core.initial_position() as f32) as i32;
        self.started = true;
        produced
    }

    // `0x373ec` -> `0x35ccc`/`0x35de0`/`0x35fd0`, ordinary path.
    fn step(&mut self, work: &mut [Vec<f32>], input_frames: usize) -> i32 {
        self.push_input(work, input_frames);
        self.process_steps();
        self.retrieve(work)
    }

    fn read_position(&self) -> i64 {
        self.core.read_position().wrapping_add(i64::from(self.read_offset))
    }

    fn set_factors(&mut self, first: &mut f32, second: &mut f32, selector: i32) -> bool {
        self.core.set_factors(first, second, selector)
    }

    fn reset(&mut self) {
        self.core.reset();
        self.input.clear();
        for plane in &mut self.scratch { plane.fill(0.0); }
        // 0x35bb4 clears the main and output queues, but not the side queue.
        self.side_active = false;
        self.started = false;
        self.produced = 0;
        self.emitted = self.core.startup_discard().wrapping_neg();
        self.read_offset = 0;
    }
}

// The Pro scheduler for the selected PN one-group near-unity path. Its
// splitter, PSOLA and Eff/vocoder leaves are independent static translations;
// end-to-end bit parity remains subject to the component oracle.
struct ProCore {
    frame: usize,
    channels: usize,
    splitter_hop: i32,
    splitter: spectral::FftSplitter,
    psola: psola::Psola,
    nested: ProDirect<EffCore>,
    interpolator: interpolate::HermiteInterpolator,
    split_fifo: PlanarFifo,
    full: Vec<Vec<f32>>,
    high: Vec<Vec<f32>>,
    split_frames: usize,
    input_hop: i32,
    inner_output_hop: i32,
    processing_hop: i32,
    output_hop: i32,
    previous_input_hop: i32,
    product: f32,
    desired_factor: f32,
    inverse_desired: f32,
    effective_factor: f32,
    positions: ProPositions,
    step: i32,
    nested_start: i32,
    nested_steps: i32,
    output_offset: usize,
    blocked: bool,
}

impl ProCore {
    fn new(rate: f32, channels: usize) -> Option<Self> {
        let frame = fft_size_for_rate(rate)?;
        if !(1..=2).contains(&channels) { return None; }
        let eff = EffCore::new(rate, channels)?;
        // `0x359c4` resets the nested direct adapter before `0x3d8e0`
        // enables the Eff bypass flag. Its emitted-position counter therefore
        // includes the initial two-sample interpolator latency.
        let mut nested = ProDirect::new(eff, channels, frame, frame);
        nested.core.configure_pro();
        let hop = (frame >> 3) as i32;
        Some(Self {
            frame, channels, splitter_hop: hop,
            splitter: spectral::FftSplitter::new(rate as u32, frame >> 2, channels, 1500.0)?,
            psola: psola::Psola::new(rate, channels, hop as usize)?,
            nested, interpolator: interpolate::HermiteInterpolator::new(channels).ok()?,
            split_fifo: PlanarFifo::new(channels, frame >> 1),
            full: (0..channels).map(|_| vec![0.0; frame]).collect(),
            high: (0..channels).map(|_| vec![0.0; frame]).collect(),
            split_frames: 0,
            input_hop: hop, inner_output_hop: hop, processing_hop: hop,
            output_hop: hop, previous_input_hop: hop,
            product: 1.0, desired_factor: 1.0, inverse_desired: 1.0, effective_factor: 1.0,
            positions: ProPositions::default(), step: 0, nested_start: 0,
            nested_steps: (channels * 5) as i32, output_offset: 0, blocked: false,
        })
    }

    fn split(&mut self, input: &[Vec<f32>], frames: usize) -> usize {
        assert_eq!(input.len(), self.channels);
        assert!(input.iter().all(|plane| plane.len() >= frames));
        if self.channels == 1 {
            self.splitter.process(&[&input[0][..frames]],
                &mut [self.full[0].as_mut_slice()], &mut [self.high[0].as_mut_slice()])
        } else {
            let (full_left, full_right) = self.full.split_at_mut(1);
            let (high_left, high_right) = self.high.split_at_mut(1);
            self.splitter.process(&[&input[0][..frames], &input[1][..frames]],
                &mut [full_left[0].as_mut_slice(), full_right[0].as_mut_slice()],
                &mut [high_left[0].as_mut_slice(), high_right[0].as_mut_slice()])
        }
    }

    fn analyse_high(&mut self, frames: usize) {
        if self.channels == 1 {
            self.psola.analyse(&[&self.high[0][..frames]]);
        } else {
            self.psola.analyse(&[&self.high[0][..frames], &self.high[1][..frames]]);
        }
    }

    fn analyse_high_prime(&mut self, frames: usize) {
        if self.channels == 1 {
            self.psola.analyse_prime(&[&self.high[0][..frames]]);
        } else {
            self.psola.analyse_prime(&[&self.high[0][..frames], &self.high[1][..frames]]);
        }
    }

    fn synthesize_high(&mut self, frames: usize, output_frames: usize, position: f64) {
        // 0x3e370 ignores the return of 0x3c2ac. The leaf retains any native
        // partial state on MissingMarkers; do not substitute audio.
        let _native_ignored = self.psola.process_in_place(&mut self.high, frames, output_frames, position);
    }

    fn nested_cost(&self) -> i32 {
        let full = (self.channels * 5) as i32;
        let reduced = (self.channels * 2) as i32;
        pro_nested_step_count(self.input_hop, self.processing_hop, 0, 1, false,
            |is_full| if is_full { full } else { reduced })
    }

    fn interpolate_output(&mut self, output: &mut [Vec<f32>]) -> i32 {
        let frames = self.output_hop as usize;
        if self.channels == 1 {
            self.interpolator.process_block(&[&self.high[0][..frames]],
                &mut [output[0].as_mut_slice()], self.effective_factor)
                .expect("native Pro interpolation work capacity") as i32
        } else {
            let (left, right) = output.split_at_mut(1);
            self.interpolator.process_block(&[&self.high[0][..frames], &self.high[1][..frames]],
                &mut [left[0].as_mut_slice(), right[0].as_mut_slice()], self.effective_factor)
                .expect("native Pro interpolation work capacity") as i32
        }
    }
}

impl ProSignalCore for ProCore {
    fn output_hop(&self) -> i32 { self.output_hop }
    fn current_factor(&self) -> f32 { self.effective_factor }
    fn initial_input(&self) -> i32 { (self.frame >> 1) as i32 + self.splitter_hop }
    fn startup_discard(&self) -> i32 {
        let nested = self.nested.core.startup_discard();
        let extra = if self.splitter_hop == self.input_hop { 0.0 } else { self.splitter_hop as f32 };
        if self.positions.target - self.positions.actual == 0.0 {
            let base = (f64::from(nested) - self.positions.target) as f32;
            let correction = self.inverse_desired * ((extra * self.product) * self.inverse_desired);
            ((correction + base) + 2.0) as i32
        } else {
            let correction = (extra * self.product) * self.inverse_desired;
            ((nested as f32 - correction) as i32).wrapping_add(2)
        }
    }
    fn accepts_input(&self, iteration: i32) -> bool {
        pro_accepts_input(iteration, self.input_hop, self.processing_hop, 0, 1, false, self.split_fifo.len() as i32)
    }
    fn ordinary_input(&self) -> i32 { self.processing_hop }
    fn input_block(&self) -> i32 {
        if self.accepts_input(0) { self.processing_hop.max(self.input_hop) } else { 0 }
    }
    fn input_hop(&self) -> i32 { self.input_hop }
    fn internal_output_hop(&self) -> i32 { self.inner_output_hop }
    fn group_count(&self) -> i32 { 1 }
    fn position_factor(&self) -> f32 { self.inverse_desired }
    fn initial_position(&self) -> i64 { self.nested.starting_position().wrapping_sub(i64::from(self.splitter_hop)) }
    fn read_position(&self) -> i64 { self.nested.read_position().wrapping_sub(i64::from(self.splitter_hop)) }
    fn step_count(&self, extra: i32) -> i32 {
        assert_eq!(extra, 0, "selected PN direct-adapter step query");
        4i32.wrapping_add(self.nested_cost())
    }
    fn step_index(&self) -> i32 { self.step }

    // Pro `0x3e870`: splitter, PSOLA analysis/zero-hop synthesis, then prime
    // every complete nested input block from the full-band FIFO.
    fn prime(&mut self, work: &mut [Vec<f32>]) {
        let frames = self.input_block() as usize;
        let produced = self.split(work, frames);
        self.analyse_high_prime(produced);
        let _native_ignored = self.psola.prime_in_place(&mut self.high, produced);
        self.split_fifo.append(&self.full, 0, produced).expect("native splitter FIFO");
        let nested_input = self.nested.core.input_block() as usize;
        while self.split_fifo.try_take_prefix(&mut self.high, nested_input) {
            self.nested.core.prime(&mut self.high);
        }
    }

    fn process_step(&mut self, work: &mut [Vec<f32>], side: Option<&mut [Vec<f32>]>, index: i32) -> i32 {
        assert!(side.is_none(), "selected AU path has no Pro side input");
        let total = self.step_count(0);
        let (next, action) = pro_step_action(index, total, self.nested_start, self.nested_steps, self.blocked);
        self.step = next;
        match action {
            ProStepAction::Split => {
                let frames = self.input_block() as usize;
                self.split_frames = self.split(work, frames);
                self.split_fifo.append(&self.full, 0, self.split_frames).expect("native splitter FIFO");
                self.nested.core.set_side_taper_enabled(true);
                self.nested.core.set_side_enabled(true);
                self.output_offset = 0;
            }
            ProStepAction::Analyse => self.analyse_high(self.split_frames),
            ProStepAction::Synthesize => {
                let position = self.nested.starting_position().wrapping_add(i64::from(self.input_hop)) as f64;
                self.synthesize_high(self.split_frames, self.output_hop as usize, position);
                self.split_frames = 0;
                self.blocked = false;
                self.nested_start = 3;
            }
            ProStepAction::PushAndAdvance => {
                let input = self.nested.needed_input(0) as usize;
                if !self.split_fifo.try_take_prefix(work, input) {
                    self.blocked = true;
                } else {
                    self.nested.push_with_side_at(work, input, &self.high, self.output_offset);
                    self.nested.advance_one();
                }
            }
            ProStepAction::Advance => self.nested.advance_one(),
            ProStepAction::Retrieve => {
                self.nested.retrieve_at(&mut self.high, self.output_offset);
                self.nested_start = self.nested_start.wrapping_add(self.nested_steps).wrapping_add(1);
                self.nested_steps = self.nested.core.step_count(0);
                self.output_offset += self.inner_output_hop as usize;
            }
            ProStepAction::Finalize => {
                self.nested_steps = self.nested.core.step_count(0);
                self.effective_factor = self.positions.advance(self.output_hop, self.desired_factor);
                return self.interpolate_output(work);
            }
            ProStepAction::Blocked => {}
        }
        0
    }

    // `0x3dbd8`, current PN modes: Pro passes the product to Eff with stretch
    // 1.0, then writes back the selector's quantized pointer without reset.
    fn set_factors(&mut self, first: &mut f32, second: &mut f32, selector: i32) -> bool {
        let requested = *second * *first;
        if !requested.is_finite() || requested < 0.1 { return false; }
        self.desired_factor = *second;
        self.inverse_desired = 1.0 / *second;
        self.product = requested;
        let previous_taper = self.nested.core.side_taper_enabled();
        self.nested.core.set_side_taper_enabled(true);
        let mut nested_product = self.product;
        let mut nested_stretch = 1.0;
        if !self.nested.set_factors(&mut nested_product, &mut nested_stretch, 0) { return false; }
        self.nested.core.set_side_taper_enabled(previous_taper);
        self.input_hop = self.nested.core.input_hop();
        self.inner_output_hop = self.nested.core.internal_output_hop();
        self.product = nested_product;
        if selector == 0 {
            *first = self.product * self.inverse_desired;
        } else {
            self.desired_factor = self.product / *first;
            self.inverse_desired = 1.0 / self.desired_factor;
            *second = self.desired_factor;
        }
        self.psola.set_stretch(self.product);
        // 0x3dbd8 invokes Pro +0x70 after the nested setter. For
        // preserve-tempo Q>1 this lowers the Eff active spectrum below N/2.
        let maximum_cutoff = (self.nested.core.rate * 0.5) * self.inverse_desired;
        self.nested.core.set_active_cutoff_hz(maximum_cutoff, true);
        self.processing_hop = self.splitter_hop.max(self.input_hop);
        let count = (self.processing_hop / self.input_hop).max(1);
        self.output_hop = self.inner_output_hop.wrapping_mul(count);
        let cutoff = if *first <= 2.0 {
            1500.0
        } else {
            let scale = (*first - 2.0) + 1.0;
            let square = scale * scale;
            let candidate = (scale * square) * 1500.0;
            let nyquist = self.nested.core.rate * 0.5;
            if candidate < nyquist { candidate } else { nyquist }
        };
        self.nested.core.set_side_cutoff_hz(cutoff);
        self.effective_factor = self.positions.set_factor_position(
            self.frame as i32, self.previous_input_hop, self.input_hop,
            self.output_hop, self.desired_factor, false
        );
        self.nested_steps = self.nested.core.step_count(0);
        true
    }

    fn reset(&mut self) {
        self.nested.reset();
        self.nested.core.configure_pro();
        self.splitter.reset();
        self.psola.reset();
        self.interpolator.reset();
        self.split_fifo.clear();
        for plane in &mut self.full { plane.fill(0.0); }
        for plane in &mut self.high { plane.fill(0.0); }
        let hop = (self.frame >> 3) as i32;
        self.split_frames = 0;
        self.input_hop = hop;
        self.inner_output_hop = hop;
        self.processing_hop = hop;
        self.output_hop = hop;
        self.previous_input_hop = hop;
        self.product = 1.0;
        self.desired_factor = 1.0;
        self.inverse_desired = 1.0;
        self.effective_factor = 1.0;
        self.positions = ProPositions::default();
        self.step = 0;
        self.nested_start = 4;
        self.nested_steps = self.nested.core.step_count(0);
        self.output_offset = 0;
        self.blocked = false;
    }
}

struct ElastiqueAdapter<C> {
    core: C,
    input: PlanarFifo,
    output: PlanarFifo,
    work: Vec<Vec<f32>>,
    startup: bool,
    discard: i32,
    required_input: i32,
    factor: f32,
    read_factor: f32,
}

impl<C: ProDirectAdapter> ElastiqueAdapter<C> {
    fn new(core: C, channels: usize) -> Self {
        assert!((1..=2).contains(&channels));
        let input_capacity = (core.startup_input().max(0) as usize).saturating_mul(2).max(512);
        let output_capacity = (core.output_hop().max(512) as usize).saturating_mul(2);
        Self {
            core, input: PlanarFifo::new(channels, input_capacity), output: PlanarFifo::new(channels, output_capacity),
            work: (0..channels).map(|_| Vec::with_capacity(input_capacity)).collect(), startup: true,
            discard: 0, required_input: 0, factor: 1.0, read_factor: 1.0,
        }
    }

    // `0x34f84`: the iteration argument is part of the direct adapter's input
    // schedule. A zero demand is valid and must not be replaced with a hop.
    fn required_input(&mut self) -> usize {
        let out_step = (self.core.output_hop() as f32 / self.core.current_factor() - 1.0) as i32;
        let mut available = self.output.len() as i32;
        let mut needed = (self.input.len() as i32).wrapping_neg();
        if self.startup {
            available = available.wrapping_add(out_step).wrapping_sub(self.core.startup_discard());
            needed = needed.wrapping_add(self.core.startup_input());
        }
        let mut iteration = 0i32;
        while available < 512 {
            needed = needed.wrapping_add(self.core.needed_input(iteration));
            available = available.wrapping_add(out_step);
            iteration = iteration.wrapping_add(1);
        }
        self.required_input = needed.max(0);
        if needed > 0 { self.read_factor = self.factor; }
        self.required_input as usize
    }

    // The AU's two setters pass different pointer/selector combinations. The
    // boolean is the core setter result; it does not reset an active stream.
    fn set_factors(&mut self, mode: PitchMode, ratio: f32) -> (f32, bool) {
        let (mut first, mut second, selector) = match mode {
            PitchMode::PreserveTempo => (1.0, ratio, 1),
            PitchMode::Turntable => (1.0 / ratio, ratio, 0),
        };
        let accepted = self.core.set_factors(&mut first, &mut second, selector);
        let mode_factor = match mode {
            PitchMode::PreserveTempo => 1.0,
            PitchMode::Turntable => first,
        };
        if accepted {
            self.factor = match mode {
                PitchMode::PreserveTempo => 1.0,
                PitchMode::Turntable => first,
            };
            if self.startup { self.discard = self.core.startup_discard().wrapping_neg(); }
        }
        (mode_factor, accepted)
    }

    fn process(&mut self, input: &[Vec<f32>], out: &mut [Vec<f32>]) -> Result<(), SchedulingError> {
        if input.len() != self.work.len() || input.iter().any(|p| p.len() != input[0].len()) || out.len() != self.work.len() {
            return Err(SchedulingError::InvalidPlanes);
        }
        if input[0].len() < self.required_input as usize { return Err(SchedulingError::InsufficientInput); }
        self.input.append(input, 0, input[0].len())?;
        if self.startup {
            let n = self.core.startup_input() as usize;
            self.input.take_vecs(&mut self.work, n)?;
            let produced = self.core.startup(&mut self.work, n);
            self.discard = self.discard.wrapping_add(produced);
            if self.discard > 0 {
                self.output.append(&self.work, 0, self.discard as usize)?;
            }
            if produced < 0 { return Err(SchedulingError::CoreStartup(produced)); }
            self.startup = false;
        }
        while self.output.len() < 512 {
            let n = self.core.needed_input(0) as usize;
            self.input.take_vecs(&mut self.work, n)?;
            let produced = self.core.step(&mut self.work, n);
            if produced < 0 { return Err(SchedulingError::InvalidCoreOutput); }
            let previous = self.discard;
            self.discard = previous.wrapping_add(produced);
            if self.discard > 0 {
                let skip = if previous < 0 { produced.wrapping_sub(self.discard) as usize } else { 0 };
                let count = produced.min(self.discard) as usize;
                self.output.append(&self.work, skip, count)?;
            }
        }
        self.output.take_vecs(out, 512)
    }

    fn read_position(&self) -> i64 {
        (self.core.read_position() as f32 - self.output.len() as f32 / self.read_factor) as i64
    }

    fn reset(&mut self) {
        self.input.clear();
        self.output.clear();
        for plane in &mut self.work { plane.clear(); }
        self.discard = 0;
        self.required_input = 0;
        self.startup = true;
        self.core.reset();
    }
}

struct AuScheduler<C> {
    adapter: ElastiqueAdapter<C>,
    input: PlanarFifo,
    output: PlanarFifo,
    pull_work: Vec<Vec<f32>>,
    input_work: Vec<Vec<f32>>,
    block_work: Vec<Vec<f32>>,
    max_pull: usize,
    pulled_position: f64,
    mode_factor: f32,
}

impl<C: ProDirectAdapter> AuScheduler<C> {
    fn new(core: C, channels: usize, maximum_frames_per_slice: usize) -> Self {
        assert!((1..=2).contains(&channels));
        assert!(maximum_frames_per_slice > 0);
        let max_pull = maximum_frames_per_slice.min(512);
        let input_capacity = (core.startup_input().max(0) as usize).saturating_mul(2).max(512);
        let buffers = |capacity| (0..channels).map(|_| Vec::with_capacity(capacity)).collect();
        Self {
            adapter: ElastiqueAdapter::new(core, channels),
            input: PlanarFifo::new(channels, input_capacity), output: PlanarFifo::new(channels, 512 + max_pull),
            pull_work: buffers(max_pull), input_work: buffers(input_capacity), block_work: buffers(512),
            max_pull, pulled_position: 0.0, mode_factor: 1.0,
        }
    }

    fn set_factors(&mut self, mode: PitchMode, ratio: f32) -> bool {
        let (factor, accepted) = self.adapter.set_factors(mode, ratio);
        // The AU writes this local even when the underlying setter failed.
        self.mode_factor = factor;
        accepted
    }

    /// `pull` returns the actual number of supplied frames, which may be less
    /// than the requested slice; it receives the AU's input sample timestamp.
    fn render<F>(&mut self, out: &mut [&mut [f32]], mut pull: F) -> Result<bool, SchedulingError>
    where F: FnMut(f64, usize, &mut [Vec<f32>]) -> Result<usize, i32> {
        if out.len() != self.pull_work.len() || out.iter().any(|p| p.len() != out[0].len()) {
            return Err(SchedulingError::InvalidPlanes);
        }
        let frames = out[0].len();
        while self.output.len() < frames {
            let needed = self.adapter.required_input();
            while self.input.len() < needed {
                for plane in &mut self.pull_work { plane.resize(self.max_pull, 0.0); }
                let supplied = pull(self.pulled_position, self.max_pull, &mut self.pull_work)
                    .map_err(SchedulingError::PullFailed)?;
                if supplied == 0 { return Err(SchedulingError::PullMadeNoProgress); }
                if supplied > self.max_pull { return Err(SchedulingError::InvalidPlanes); }
                self.input.append(&self.pull_work, 0, supplied)?;
                self.pulled_position += supplied as f64;
            }
            self.input.take_vecs(&mut self.input_work, needed)?;
            self.adapter.process(&self.input_work, &mut self.block_work)?;
            self.output.append(&self.block_work, 0, 512)?;
        }
        self.output.take_slices(out, frames)?;
        Ok(out.iter().all(|plane| plane.iter().all(|&sample| sample == 0.0)))
    }

    fn buffer_size(&self) -> f32 {
        let mut size = (self.pulled_position - self.adapter.read_position() as f64) as f32;
        if self.mode_factor != 0.0 {
            size += self.output.len() as f32 / self.mode_factor;
        }
        size
    }

    fn reset(&mut self) {
        self.input.clear();
        self.output.clear();
        self.adapter.reset();
        self.pulled_position = 0.0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PitchError {
    InvalidSampleRate,
    InvalidChannelCount,
    InvalidMaximumSlice,
    UnsupportedRatio,
    SetupFailed,
    InvalidPlanes,
    InsufficientInput,
    InvalidCoreOutput,
    CoreStartup(i32),
    PullFailed(i32),
    PullMadeNoProgress,
}

impl std::fmt::Display for PitchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PitchError {}

impl From<SchedulingError> for PitchError {
    fn from(value: SchedulingError) -> Self {
        match value {
            SchedulingError::InvalidPlanes => Self::InvalidPlanes,
            SchedulingError::InsufficientInput => Self::InsufficientInput,
            SchedulingError::InvalidCoreOutput => Self::InvalidCoreOutput,
            SchedulingError::CoreStartup(code) => Self::CoreStartup(code),
            SchedulingError::PullFailed(code) => Self::PullFailed(code),
            SchedulingError::PullMadeNoProgress => Self::PullMadeNoProgress,
        }
    }
}

/// Selected Platinum Notes AU/Pro/Eff pipeline for the detector-constrained
/// tuning range and the two visible modes. Some synthetic component captures
/// match, but the original GUI's splitter scratch alignment is not established,
/// and the pinned aligned-0 profile is not a universal byte-parity claim.
/// The host supplies any zero-filled source tail and decides when to stop;
/// no EOS/flush protocol is assumed.
pub struct PitchProcessor {
    scheduler: AuScheduler<ProDirect<ProCore>>,
    mode: PitchMode,
    ratio: f32,
}

impl PitchProcessor {
    fn selected_ratio(ratio: f32) -> bool {
        let low = 440.0f32 / 452.893f32;
        let high = 440.0f32 / 427.474f32;
        ratio.is_finite() && ratio >= low && ratio <= high
    }

    pub fn new(
        sample_rate: f32,
        channels: usize,
        maximum_frames_per_slice: usize,
        mode: PitchMode,
        ratio: f32,
    ) -> Result<Self, PitchError> {
        let frame = fft_size_for_rate(sample_rate).ok_or(PitchError::InvalidSampleRate)?;
        if !(1..=2).contains(&channels) { return Err(PitchError::InvalidChannelCount); }
        if maximum_frames_per_slice == 0 { return Err(PitchError::InvalidMaximumSlice); }
        if !Self::selected_ratio(ratio) { return Err(PitchError::UnsupportedRatio); }
        let core = ProCore::new(sample_rate, channels).ok_or(PitchError::SetupFailed)?;
        let direct = ProDirect::new(core, channels, frame, frame);
        let mut scheduler = AuScheduler::new(direct, channels, maximum_frames_per_slice);
        if !scheduler.set_factors(mode, ratio) { return Err(PitchError::UnsupportedRatio); }
        Ok(Self { scheduler, mode, ratio })
    }

    pub fn mode(&self) -> PitchMode { self.mode }
    pub fn ratio(&self) -> f32 { self.ratio }

    /// Apply a mode update without clearing any audio FIFO or DSP history.
    /// The bool reports the internal setter result; AU parameter storage and
    /// its mode-factor local are updated even if the internal setter rejects.
    pub fn set_mode(&mut self, mode: PitchMode) -> bool {
        self.mode = mode;
        self.scheduler.set_factors(mode, self.ratio)
    }

    /// Apply a detector-derived ratio update without resetting the stream.
    pub fn set_ratio(&mut self, ratio: f32) -> Result<bool, PitchError> {
        if !Self::selected_ratio(ratio) { return Err(PitchError::UnsupportedRatio); }
        self.ratio = ratio;
        Ok(self.scheduler.set_factors(self.mode, ratio))
    }

    /// Render one planar callback. `pull` sees the native input timestamp and
    /// equally sized planes and returns the actual number of supplied frames.
    /// The bool reports whether every returned sample compares equal to zero,
    /// matching the AU's output-is-silence decision.
    pub fn render<F>(&mut self, output: &mut [&mut [f32]], mut pull: F) -> Result<bool, PitchError>
    where F: FnMut(f64, &mut [&mut [f32]]) -> Result<usize, i32> {
        self.scheduler.render(output, |timestamp, _, planes| match planes {
            [mono] => pull(timestamp, &mut [mono.as_mut_slice()]),
            [left, right] => pull(timestamp, &mut [left.as_mut_slice(), right.as_mut_slice()]),
            _ => Err(-1),
        }).map_err(PitchError::from)
    }

    /// AU parameter 2, not the separately advertised zero latency property.
    pub fn buffer_size(&self) -> f32 { self.scheduler.buffer_size() }

    /// Native AU reset clears queues, resets the core, and restarts the input
    /// timestamp; the stored ratio/mode and AU mode factor remain unchanged.
    pub fn reset(&mut self) { self.scheduler.reset(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint_hash(samples: &[f32]) -> u64 {
        let mut hash = 14695981039346656037u64;
        for sample in samples {
            hash ^= u64::from(sample.to_bits());
            hash = hash.wrapping_mul(1099511628211);
        }
        hash
    }

    fn compare_checkpoint(
        adapter: &ElastiqueAdapter<ProDirect<ProCore>>,
        expected: &serde_json::Value,
        block: i32,
        mismatch_count: &mut usize,
    ) {
        use serde_json::{Value, json};
        let direct = &adapter.core;
        let pro = &direct.core;
        let nested = &pro.nested;
        let eff = &nested.core;
        let voc = eff.vocoder.state();
        let (interp_position, interp_fraction, interp_index) = pro.interpolator.reference_state();
        let mut fields: Vec<(&str, Value)> = vec![
            ("outer_discard", json!(adapter.discard)),
            ("outer_required", json!(adapter.required_input)),
            ("outer_startup", json!(u8::from(adapter.startup))),
            ("outer_factor", json!(adapter.factor.to_bits())),
            ("outer_read_factor", json!(adapter.read_factor.to_bits())),
            ("outer_input_queued", json!(adapter.input.len())),
            ("outer_output_queued", json!(adapter.output.len())),
            ("outer_direct_produced", json!(direct.produced)),
            ("outer_direct_read_offset", json!(direct.read_offset)),
            ("outer_direct_emitted", json!(direct.emitted)),
            ("outer_direct_started", json!(u8::from(direct.started))),
            ("pro_product", json!(pro.product.to_bits())),
            ("pro_desired", json!(pro.desired_factor.to_bits())),
            ("pro_effective", json!(pro.effective_factor.to_bits())),
            ("pro_inverse", json!(pro.inverse_desired.to_bits())),
            ("pro_peak", json!(pro.positions.peak_error.to_bits())),
            ("pro_input_hop", json!(pro.input_hop)),
            ("pro_previous_input_hop", json!(pro.previous_input_hop)),
            ("pro_inner_output_hop", json!(pro.inner_output_hop)),
            ("pro_processing_hop", json!(pro.processing_hop)),
            ("pro_output_hop", json!(pro.output_hop)),
            ("pro_step", json!(pro.step)),
            ("pro_base", json!(pro.positions.base.to_bits())),
            ("pro_actual", json!(pro.positions.actual.to_bits())),
            ("pro_target", json!(pro.positions.target.to_bits())),
            ("pro_split_frames", json!(pro.split_frames)),
            ("pro_output_offset", json!(pro.output_offset)),
            ("pro_nested_steps", json!(pro.nested_steps)),
            ("pro_nested_start", json!(pro.nested_start)),
            ("pro_blocked", json!(u8::from(pro.blocked))),
            ("pro_split_queued", json!(pro.split_fifo.len())),
            ("nested_produced", json!(nested.produced)),
            ("nested_read_offset", json!(nested.read_offset)),
            ("nested_emitted", json!(nested.emitted)),
            ("nested_started", json!(u8::from(nested.started))),
            ("nested_side_active", json!(u8::from(nested.side_active))),
            ("eff_product", json!(voc.target_factor.to_bits())),
            ("eff_desired", json!(1.0f32.to_bits())),
            ("eff_effective", json!(eff.effective_factor.to_bits())),
            ("eff_input_hop", json!(eff.input_hop)),
            ("eff_previous_input_hop", json!(eff.previous_input_hop)),
            ("eff_output_hop", json!(eff.output_hop)),
            ("eff_step", json!(eff.step)),
            ("eff_position_offset", json!(eff.position_offset)),
            ("eff_position_baseline", json!(eff.position_baseline)),
            ("eff_source_position", json!(eff.source_position.to_bits())),
            ("eff_first_frame", json!(u8::from(eff.first_frame))),
            ("eff_bypass_interpolator", json!(u8::from(eff.bypass_final_interpolator))),
            ("interp_position", json!(interp_position.to_bits())),
            ("interp_fraction", json!(interp_fraction.to_bits())),
            ("interp_index", json!(interp_index)),
            ("vocoder_frame_count", json!(voc.frame_count)),
            ("vocoder_active_bins", json!(voc.active_bins)),
            ("vocoder_side_cutoff", json!(voc.side_cutoff)),
            ("vocoder_target_factor", json!(voc.target_factor.to_bits())),
            ("vocoder_previous_factor", json!(voc.previous_factor.to_bits())),
            ("vocoder_taper", json!(u8::from(voc.side_taper_enabled))),
            ("vocoder_side", json!(u8::from(voc.side_enabled))),
            ("vocoder_bypass", json!(u8::from(voc.bypass_propagation))),
        ];
        if block >= 0 {
            let hop = pro.output_hop as usize;
            fields.extend([
                ("pro_high_l_hash", json!(checkpoint_hash(&pro.high[0][..hop]))),
                ("pro_high_r_hash", json!(checkpoint_hash(&pro.high[1][..hop]))),
                ("pro_full_l_hash", json!(checkpoint_hash(&pro.full[0][..hop]))),
                ("pro_full_r_hash", json!(checkpoint_hash(&pro.full[1][..hop]))),
            ]);
            for (key, samples) in [
                ("pro_high_l_bits", &pro.high[0][..hop]),
                ("pro_high_r_bits", &pro.high[1][..hop]),
            ] {
                let native = expected[key].as_array().expect("controlled native high-buffer bits");
                assert_eq!(native.len(), samples.len(), "{key} native hop length");
                let mut count = 0usize;
                let mut first = 0usize;
                let mut last = 0usize;
                for (index, (actual, original)) in samples.iter().zip(native).enumerate() {
                    let original = original.as_u64().expect("native f32 bits") as u32;
                    if actual.to_bits() != original {
                        if count == 0 { first = index; }
                        last = index;
                        if count < 8 {
                            eprintln!("checkpoint block {block} {key}[{index}]: original {original:#010x} Rust {:#010x}", actual.to_bits());
                        }
                        count += 1;
                    }
                }
                if count != 0 {
                    eprintln!("checkpoint block {block} {key}: {count} differing samples, range {first}..={last}");
                    *mismatch_count += 1;
                }
            }
        }
        for (key, actual) in fields {
            if expected[key] != actual {
                eprintln!("checkpoint block {block} {key}: original {} Rust {actual}", expected[key]);
                *mismatch_count += 1;
            }
        }
    }

    /// Compare state after actual original and Rust 512-frame adapter calls.
    /// Each run must identify one controlled native splitter scratch profile;
    /// this does not infer the original GUI's allocator or claim file parity.
    #[cfg(target_arch = "aarch64")]
    #[test]
    #[ignore = "requires PITCH_SCHEDULER_ORACLE from state-oracle.cpp"]
    fn scheduler_checkpoints_match_original_component() {
        unsafe extern "C" {
            #[link_name = "sin"]
            fn host_sin(value: f64) -> f64;
        }
        let path = std::env::var_os("PITCH_SCHEDULER_ORACLE").expect("PITCH_SCHEDULER_ORACLE");
        let source = std::fs::read_to_string(path).expect("native state capture");
        let entries: Vec<serde_json::Value> = source.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let config = &entries[0];
        assert_eq!(config["kind"], "config");
        let scratch_offset = match config["scratch_mode"].as_str() {
            Some("aligned0") => 0,
            Some("offset48") => 48,
            _ => panic!("checkpoint must identify a controlled splitter scratch intervention"),
        };
        for entry in entries.iter().skip(1) {
            assert_eq!(entry["split_fft_real_mod64"], scratch_offset);
            assert_eq!(entry["split_fft_imag_mod64"], scratch_offset);
            for key in ["split_fft_time_mod64", "voc_fft_real_mod64", "voc_fft_imag_mod64", "voc_fft_time_mod64"] {
                assert_eq!(entry[key], 0, "unexpected non-intervened scratch alignment: {key}");
            }
        }
        let mode = match config["mode"].as_u64().unwrap() {
            0 => PitchMode::PreserveTempo,
            1 => PitchMode::Turntable,
            _ => panic!("native mode outside selected domain"),
        };
        let ratio = f32::from_bits(config["ratio_bits"].as_u64().unwrap() as u32);
        let signal = config["signal"].as_str().unwrap();
        let frames = config["source_frames"].as_u64().unwrap();
        let blocks = config["blocks"].as_u64().unwrap() as usize;
        assert_eq!(entries.len(), blocks + 2);
        let au_audio = std::env::var_os("PITCH_UNIT_AU_REFERENCE").map(|prefix| {
            [".left.f32", ".right.f32"].map(|suffix| {
                let mut path = prefix.clone();
                path.push(suffix);
                std::fs::read(path).expect("native AU channel capture")
            })
        });
        let mut processor = PitchProcessor::new(48_000.0, 2, 512, mode, ratio).unwrap();
        if scratch_offset == 48 {
            processor.scheduler.adapter.core.core.splitter.use_offset48_for_test();
        }
        let adapter = &mut processor.scheduler.adapter;
        let mut mismatches = 0usize;
        compare_checkpoint(adapter, &entries[1], -1, &mut mismatches);
        let mut consumed = 0u64;
        let mut pulled = 0u64;
        let mut output = vec![Vec::new(), Vec::new()];
        for block in 0..blocks {
            let expected = &entries[block + 2];
            let needed = adapter.required_input();
            let native_needed = expected["demand"].as_u64().unwrap() as usize;
            assert_eq!(needed, native_needed, "native input demand at block {block}");
            while pulled - consumed < needed as u64 { pulled += 512; }
            let mut input = vec![vec![0.0; needed], vec![0.0; needed]];
            for channel in 0..2 {
                for i in 0..needed {
                    let frame = consumed + i as u64;
                    input[channel][i] = if frame >= frames { 0.0 } else {
                        match signal {
                            "tone" => {
                                let angle = 6.283185307179586476925286766559f64 * 440.0 * frame as f64 / 48000.0;
                                ((0.25 * unsafe { host_sin(angle) }) as f32) * if channel == 0 { 1.0 } else { 0.5 }
                            }
                            "pattern" => (((frame * 17 + channel as u64 * 29) % 101) as i32 - 50) as f32 / 128.0,
                            "impulse" if channel == 0 && frame == 0 => 0.75,
                            "impulse" if channel == 1 && frame == frames / 2 => -0.5,
                            "impulse" => 0.0,
                            _ => panic!("non-synthetic native signal"),
                        }
                    };
                }
            }
            for (channel, key) in ["input_l_hash", "input_r_hash"].iter().enumerate() {
                let original = expected[*key].as_u64().expect("native input hash");
                let actual = checkpoint_hash(&input[channel]);
                if original != actual {
                    eprintln!("checkpoint block {block} {key}: original {original} Rust {actual}");
                    mismatches += 1;
                }
            }
            let before: u64;
            unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) before, options(nostack, preserves_flags)); }
            adapter.process(&input, &mut output).unwrap();
            let after: u64;
            unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) after, options(nostack, preserves_flags)); }
            for (key, actual) in [("fpcr_before", before), ("fpcr_after", after)] {
                let original = expected[key].as_u64().expect("native FPCR");
                if original != actual {
                    eprintln!("checkpoint block {block} {key}: original {original:#x} Rust {actual:#x}");
                    mismatches += 1;
                }
            }
            for (channel, key) in ["output_l_bits", "output_r_bits"].iter().enumerate() {
                let original = expected[*key].as_array().expect("native output bits");
                assert_eq!(original.len(), 512);
                    let mut different = 0usize;
                    let mut versus_au = 0usize;
                    for (index, (native, rust)) in original.iter().zip(&output[channel]).enumerate() {
                        let native = native.as_u64().expect("native f32 bits") as u32;
                        if native != rust.to_bits() {
                            if different < 3 { eprintln!("checkpoint block {block} {key}[{index}]: original {native:#010x} Rust {:#010x}", rust.to_bits()); }
                            different += 1;
                        }
                        if let Some(reference) = &au_audio {
                            let offset = (block * 512 + index) * 4;
                            let au = u32::from_ne_bytes(reference[channel][offset..offset + 4].try_into().unwrap());
                            if native != au {
                                if versus_au < 3 { eprintln!("checkpoint block {block} {key}[{index}]: direct original {native:#010x} AU original {au:#010x}"); }
                                versus_au += 1;
                            }
                        }
                    }
                eprintln!("checkpoint block {block} {key}: {different} original-direct/Rust differences, {versus_au} original-direct/AU differences");
                if different != 0 || versus_au != 0 { mismatches += 1; }
            }
            consumed += needed as u64;
            assert_eq!(expected["consumed"].as_u64(), Some(consumed));
            assert_eq!(expected["pulled"].as_u64(), Some(pulled));
            compare_checkpoint(adapter, expected, block as i32, &mut mismatches);
        }
        assert_eq!(mismatches, 0, "native scheduler checkpoint mismatches");
    }

    /// The two captures are original-only, with deliberately substituted
    /// scratch addresses. A positive difference is evidence of alignment
    /// sensitivity, not a Rust-vs-original pass and not an app A/A result.
    #[test]
    #[ignore = "requires controlled PITCH_SCHEDULER_ALIGNED_ORACLE and PITCH_SCHEDULER_OFFSET48_ORACLE"]
    fn original_splitter_scratch_alignment_changes_output_bits() {
        let load = |key: &str| -> Vec<serde_json::Value> {
            let path = std::env::var_os(key).unwrap_or_else(|| panic!("{key}"));
            std::fs::read_to_string(path).expect("native controlled capture").lines()
                .map(|line| serde_json::from_str(line).expect("native JSONL")).collect()
        };
        let aligned = load("PITCH_SCHEDULER_ALIGNED_ORACLE");
        let offset = load("PITCH_SCHEDULER_OFFSET48_ORACLE");
        assert_eq!(aligned.len(), offset.len());
        assert!(aligned.len() > 2);
        assert_eq!(aligned[0]["scratch_mode"], "aligned0");
        assert_eq!(offset[0]["scratch_mode"], "offset48");
        for key in ["mode", "ratio_bits", "signal", "blocks", "source_frames", "writeback_bits"] {
            assert_eq!(aligned[0][key], offset[0][key], "controlled configuration {key}");
        }
        let mut left_differences = 0usize;
        let mut right_differences = 0usize;
        for (a, b) in aligned.iter().skip(1).zip(offset.iter().skip(1)) {
            assert_eq!(a["block"], b["block"]);
            assert_eq!(a["split_fft_real_mod64"], 0);
            assert_eq!(a["split_fft_imag_mod64"], 0);
            assert_eq!(b["split_fft_real_mod64"], 48);
            assert_eq!(b["split_fft_imag_mod64"], 48);
            for key in ["split_fft_time_mod64", "voc_fft_real_mod64", "voc_fft_imag_mod64", "voc_fft_time_mod64"] {
                assert_eq!(a[key], 0, "aligned control {key}");
                assert_eq!(b[key], 0, "offset control {key}");
            }
            for key in ["fpcr_before", "fpcr_after", "demand", "consumed", "pulled"] {
                assert_eq!(a[key], b[key], "controlled checkpoint {key}");
            }
            if a["block"].as_i64().unwrap() < 0 { continue; }
            for key in ["input_l_hash", "input_r_hash", "pro_full_l_hash", "pro_full_r_hash"] {
                assert_eq!(a[key], b[key], "controlled checkpoint {key}");
            }
            for (key, total) in [("output_l_bits", &mut left_differences), ("output_r_bits", &mut right_differences)] {
                let left = a[key].as_array().expect("aligned native output bits");
                let right = b[key].as_array().expect("offset native output bits");
                assert_eq!(left.len(), 512);
                assert_eq!(left.len(), right.len());
                *total += left.iter().zip(right).filter(|(x, y)| x != y).count();
            }
        }
        eprintln!("original controlled alignment produced {left_differences} left and {right_differences} right different f32 samples");
        assert!(left_differences > 0 && right_differences > 0,
            "this captured scenario did not demonstrate alignment sensitivity");
    }

    #[test]
    fn direct_input_demand_uses_iteration_predicates_and_binary64_phase() {
        let mut state = DirectDemandState {
            special_mode: false, target: 10, produced: 0, current_factor: 1.2,
            previous_factor: 1.0, output_hop: 4, input_hop: 5, ordinary_input: 7,
        };
        assert_eq!(direct_input_demand(state, 3, |_| false), 0);
        assert_eq!(direct_input_demand(state, 3, |_| true), 7);
        state.special_mode = true;
        let mut calls = Vec::new();
        let count = direct_input_demand(state, 7, |step| {
            calls.push(step);
            step == 7 || step % 2 == 0
        });
        assert_eq!(calls, vec![7, 0, 1, 2, 3]);
        assert_eq!(count, 10);
        state.produced = 11;
        assert_eq!(direct_input_demand(state, 0, |_| true), 0);
    }

    #[test]
    fn planar_fifo_wrap_preserves_both_channels_and_underflow_state() {
        let mut fifo = PlanarFifo::new(2, 4);
        fifo.append(&[vec![0.0, 1.0, 2.0], vec![10.0, 11.0, 12.0]], 0, 3).unwrap();
        let mut left = [0.0; 2];
        let mut right = [0.0; 2];
        fifo.take_slices(&mut [&mut left, &mut right], 2).unwrap();
        assert_eq!(left, [0.0, 1.0]);
        assert_eq!(right, [10.0, 11.0]);
        fifo.append(&[vec![3.0, 4.0, 5.0], vec![13.0, 14.0, 15.0]], 0, 3).unwrap();
        let mut scratch = vec![vec![-1.0; 5], vec![-1.0; 5]];
        assert!(!fifo.try_take_prefix(&mut scratch, 5));
        assert_eq!(scratch, vec![vec![-1.0; 5], vec![-1.0; 5]]);
        assert_eq!(fifo.len(), 4);
        assert!(fifo.try_take_prefix(&mut scratch, 4));
        assert_eq!(&scratch[0][..4], &[2.0, 3.0, 4.0, 5.0]);
        assert_eq!(&scratch[1][..4], &[12.0, 13.0, 14.0, 15.0]);
        assert_eq!(fifo.len(), 0);
    }

    #[test]
    fn pro_step_dispatch_retains_block_and_nested_phase_order() {
        use ProStepAction::*;
        assert_eq!(pro_step_action(0, 17, 3, 4, false), (1, Split));
        assert_eq!(pro_step_action(1, 17, 3, 4, false), (2, Analyse));
        assert_eq!(pro_step_action(2, 17, 3, 4, true), (3, Synthesize));
        assert_eq!(pro_step_action(3, 17, 3, 4, false), (4, PushAndAdvance));
        assert_eq!(pro_step_action(4, 17, 3, 4, false), (5, Advance));
        assert_eq!(pro_step_action(7, 17, 3, 4, false), (8, Retrieve));
        assert_eq!(pro_step_action(8, 17, 8, 4, false), (9, PushAndAdvance));
        assert_eq!(pro_step_action(16, 17, 8, 4, false), (0, Finalize));
        assert_eq!(pro_step_action(16, 17, 8, 4, true), (0, Blocked));
        assert_eq!(pro_step_action(3, 0, 3, 4, false), (4, Finalize));
    }

    #[test]
    fn pro_nested_cost_respects_group_and_reverse_phase() {
        let mut full = Vec::new();
        let cost = pro_nested_step_count(512, 1536, 0, 2, false, |is_full| {
            full.push(is_full);
            if is_full { 10 } else { 8 }
        });
        assert_eq!(full, [true, false, true]);
        assert_eq!(cost, 31);
        let mut reverse = Vec::new();
        let cost = pro_nested_step_count(512, 1536, -1, 2, true, |is_full| {
            reverse.push(is_full);
            if is_full { 10 } else { 8 }
        });
        assert_eq!(reverse, [false, true, false]);
        assert_eq!(cost, 29);
        assert_eq!(pro_nested_step_count(0, 512, 0, 1, false, |_| 1), 0);
    }

    #[test]
    fn pro_input_predicate_counts_queued_splitter_frames() {
        assert!(pro_accepts_input(0, 512, 512, 0, 1, false, 0));
        assert!(pro_accepts_input(1, 512, 512, 0, 1, false, 512));
        assert!(!pro_accepts_input(1, 512, 512, 0, 1, false, 1024));
        assert!(pro_accepts_input(0, 512, 1024, 1, 2, false, 1023));
        assert!(!pro_accepts_input(0, 512, 1024, 1, 2, false, 1024));
        assert!(!pro_accepts_input(0, 512, 512, 1, 4, false, 0));
        assert!(pro_accepts_input(0, 0, 512, 1, 4, true, 0));
    }

    #[test]
    fn pro_position_uses_native_truncation_and_f32_hop_divisions() {
        let mut fractional = ProPositions { base: 0.0, actual: 0.75, target: 0.0, peak_error: 20.0 };
        assert_eq!(fractional.advance(512, 1.0).to_bits(), 1.0f32.to_bits());
        assert_eq!(fractional.peak_error, 0.0);
        assert_eq!((fractional.base, fractional.actual, fractional.target), (512.0, 512.75, 512.0));
        let mut displaced = ProPositions { base: 0.0, actual: 1.5, target: 0.0, peak_error: 0.0 };
        let factor = displaced.advance(512, 1.0);
        assert_eq!(factor.to_bits(), 1.0009765625f32.to_bits());
        assert_eq!(displaced.peak_error.to_bits(), 1.875f32.to_bits());
        assert_eq!(displaced.base, 512.0);
        assert_eq!(displaced.target, 512.0);
        assert_eq!(displaced.actual, 1.5 + f64::from(512.0f32 / factor));
    }

    #[test]
    fn successful_factor_update_changes_positions_without_resetting_stream() {
        let mut immediate = ProPositions { base: 1024.0, actual: 900.0, target: 800.0, peak_error: 7.0 };
        assert_eq!(immediate.set_factor_position(4096, 512, 512, 512, 1.0, false), 1.0);
        assert_eq!((immediate.base, immediate.actual, immediate.target), (1024.0, 1024.0, 1024.0));
        assert_eq!(immediate.peak_error, 7.0);
        let mut smooth = ProPositions { base: 1024.0, actual: 1025.5, target: 800.0, peak_error: 0.0 };
        let factor = smooth.set_factor_position(4096, 512, 512, 512, 1.0, true);
        assert_eq!(factor.to_bits(), 1.0009765625f32.to_bits());
        assert_eq!((smooth.base, smooth.actual, smooth.target), (1024.0, 1025.5, 1024.0));
        assert_eq!(smooth.peak_error, 1.875);
    }

    #[test]
    fn rate_boundaries_and_quantizer_use_reference_precision() {
        assert_eq!(fft_size_for_rate(31_999.0), Some(2048));
        assert_eq!(fft_size_for_rate(32_000.0), Some(4096));
        assert_eq!(fft_size_for_rate(64_000.0), Some(8192));
        assert_eq!(fft_size_for_rate(128_000.0), Some(16384));
        assert_eq!(fft_size_for_rate(256_000.0), Some(32768));
        assert_eq!(fft_size_for_rate(f32::NAN), None);
        let q = quantize_factor(4096, 0.999, 1, 6.0).unwrap();
        assert_eq!((q.input_hop, q.output_hop), (512, 511));
        assert_eq!(q.factor.to_bits(), (511.0f32 / 512.0).to_bits());
        // 2.2f32 is above the binary64 literal 2.2.
        assert_eq!(quantize_factor(4096, 2.2, 1, 6.0).unwrap().input_hop, 256);
        assert_eq!(quantize_factor(4096, 1.0, 2, 6.0).unwrap().input_hop, 128);
    }

    #[test]
    fn selected_modes_keep_the_native_f32_product_and_pointer_writeback() {
        let ratio = 0.999f32;
        let preserve = selected_single_group_factors(48_000.0, ratio, PitchMode::PreserveTempo).unwrap();
        assert_eq!(preserve.initial_product.to_bits(), ratio.to_bits());
        assert_eq!(preserve.quantized.output_hop, 511);
        assert_eq!(preserve.second.to_bits(), (511.0f32 / 512.0).to_bits());
        assert_eq!(preserve.first.to_bits(), 1.0f32.to_bits());
        assert_eq!(preserve.au_mode_factor.to_bits(), 1.0f32.to_bits());
        let turntable = selected_single_group_factors(48_000.0, ratio, PitchMode::Turntable).unwrap();
        let reciprocal = 1.0f32 / ratio;
        assert_eq!(turntable.initial_product.to_bits(), (ratio * reciprocal).to_bits());
        assert_eq!(turntable.second.to_bits(), ratio.to_bits());
        let updated_first = turntable.quantized.factor * reciprocal;
        assert_eq!(turntable.first.to_bits(), updated_first.to_bits());
        assert_eq!(turntable.au_mode_factor.to_bits(), updated_first.to_bits());
    }

    #[test]
    fn stereo_coupling_uses_two_weighted_products_then_sum_and_difference() {
        let mut left = [1.0, -0.25, 0.0];
        let mut right = [0.0, 0.5, -1.0];
        couple_stereo(&mut left, &mut right).unwrap();
        let a = -0.25f32 * f32::from_bits(0x3f19_999a);
        let b = 0.5f32 * f32::from_bits(0x3ecc_cccd);
        assert_eq!(left[1].to_bits(), (a + b).to_bits());
        assert_eq!(right[1].to_bits(), (a - b).to_bits());
        uncouple_stereo(&mut left, &mut right).unwrap();
        assert_eq!(left[0].to_bits(), ((0.6f32 + 0.6) / 1.2).to_bits());
    }

    #[test]
    fn eff_selected_factor_hops_keep_stream_state() {
        let mut eff = EffCore::new(48_000.0, 2).expect("native FFT setup");
        eff.configure_pro();
        assert_eq!((eff.input_hop(), eff.output_hop()), (512, 512));
        assert_eq!(eff.initial_input(), 2048);
        assert_eq!(eff.startup_discard(), 2048);
        assert_eq!(eff.side_cutoff, 128);
        let mut pitch = 0.999f32;
        let mut stretch = 1.0f32;
        assert!(eff.set_factors(&mut pitch, &mut stretch, 0));
        assert_eq!(pitch.to_bits(), (511.0f32 / 512.0).to_bits());
        assert_eq!((eff.input_hop(), eff.output_hop()), (512, 511));
        assert_eq!(eff.read_position(), -2048);
        eff.set_side_enabled(true);
        let mut main = vec![vec![0.0; 1024], vec![0.0; 1024]];
        let mut side = vec![vec![0.0; 1024], vec![0.0; 1024]];
        for step in 0..10 {
            let produced = eff.process_step(&mut main, Some(&mut side), step);
            assert_eq!(produced, if step == 9 { 511 } else { 0 });
        }
        assert_eq!(eff.step_index(), 0);
        assert_eq!(eff.read_position(), -1536);
        let frame_count = eff.vocoder.state().frame_count;
        pitch = 1.001;
        assert!(eff.set_factors(&mut pitch, &mut stretch, 0));
        assert_eq!(eff.vocoder.state().frame_count, frame_count);
        assert_eq!(eff.input_hop(), 512);
        assert_eq!(eff.output_hop(), 512);
    }

    #[test]
    fn pro_factor_updates_eff_active_cutoff_before_side_cutoff() {
        let mut core = ProCore::new(48_000.0, 2).expect("native FFT setup");
        // Native direct reset runs before Pro enables the Eff bypass flag.
        assert_eq!(core.nested.emitted, -2050);
        assert_eq!(core.nested.core.startup_discard(), 2048);
        assert_eq!(core.nested_start, 0);
        let mut pitch = 1.0f32;
        let mut stretch = 1.01f32;
        assert!(core.set_factors(&mut pitch, &mut stretch, 1));
        assert_eq!(stretch.to_bits(), (517.0f32 / 512.0).to_bits());
        assert_eq!(core.nested.core.active_bins, 2028);
        assert_eq!(core.nested.core.vocoder.state().active_bins, 2028);
        assert_eq!(core.nested.core.side_cutoff, 128);
        // Updating the separate side cutoff must not undo the active cutoff.
        core.nested.core.set_side_cutoff_hz(1500.0);
        assert_eq!(core.nested.core.vocoder.state().active_bins, 2028);
        // A subsequent unity update restores the original full-band limit.
        pitch = 1.0;
        stretch = 1.0;
        assert!(core.set_factors(&mut pitch, &mut stretch, 1));
        assert_eq!(core.nested.core.active_bins, 2048);
        assert_eq!(core.nested.core.vocoder.state().active_bins, 2048);
    }


    #[test]
    fn direct_startup_primes_then_runs_one_full_step_sequence() {
        struct SignalCore { index: i32, position: i64, primed: Vec<f32>, side_starts: Vec<Option<f32>> }
        impl ProSignalCore for SignalCore {
            fn output_hop(&self) -> i32 { 4 }
            fn current_factor(&self) -> f32 { 1.0 }
            fn initial_input(&self) -> i32 { 12 }
            fn startup_discard(&self) -> i32 { 6 }
            fn accepts_input(&self, iteration: i32) -> bool { iteration == 0 }
            fn ordinary_input(&self) -> i32 { 4 }
            fn input_block(&self) -> i32 { 4 }
            fn input_hop(&self) -> i32 { 4 }
            fn internal_output_hop(&self) -> i32 { 4 }
            fn group_count(&self) -> i32 { 1 }
            fn position_factor(&self) -> f32 { 1.0 }
            fn initial_position(&self) -> i64 { 0 }
            fn read_position(&self) -> i64 { self.position }
            fn step_count(&self, _: i32) -> i32 { 2 }
            fn step_index(&self) -> i32 { self.index }
            fn prime(&mut self, work: &mut [Vec<f32>]) {
                self.primed.push(work[0][0]);
                self.position += 4;
            }
            fn process_step(&mut self, _: &mut [Vec<f32>], side: Option<&mut [Vec<f32>]>, index: i32) -> i32 {
                self.side_starts.push(side.map(|planes| planes[0][0]));
                self.index = (index + 1) % 2;
                if index == 1 { self.position += 4; 4 } else { 0 }
            }
            fn set_factors(&mut self, _: &mut f32, _: &mut f32, _: i32) -> bool { true }
            fn reset(&mut self) { self.index = 0; self.position = 0; self.primed.clear(); self.side_starts.clear(); }
        }
        let core = SignalCore { index: 0, position: 0, primed: Vec::new(), side_starts: Vec::new() };
        let mut direct = ProDirect::new(core, 1, 8, 8);
        assert_eq!(direct.startup_input(), 12);
        assert_eq!(direct.needed_input(0), 4);
        assert_eq!(direct.needed_input(1), 0);
        let mut work = vec![(0..12).map(|x| x as f32).collect::<Vec<_>>()];
        assert_eq!(direct.startup(&mut work, 12), 4);
        assert_eq!(direct.core.primed, [0.0, 4.0]);
        assert_eq!(work[0], [8.0, 9.0, 10.0, 11.0]);
        assert_eq!(direct.read_offset, -2);
        assert_eq!(direct.read_position(), 10);
        assert_eq!(direct.emitted, -2);
        work[0] = vec![12.0, 13.0, 14.0, 15.0];
        assert_eq!(direct.step(&mut work, 4), 4);
        assert_eq!(work[0], [12.0, 13.0, 14.0, 15.0]);
        assert_eq!(direct.read_position(), 14);
        assert_eq!(direct.emitted, 2);
        let input = vec![vec![16.0, 17.0, 18.0, 19.0]];
        let side = vec![vec![100.0, 101.0, 102.0, 103.0]];
        let mut output = vec![Vec::new()];
        assert_eq!(direct.step_with_side(&input, 4, &side, &mut output), 4);
        assert_eq!(output[0], [16.0, 17.0, 18.0, 19.0]);
        assert_eq!(&direct.core.side_starts[4..], &[Some(100.0), Some(100.0)]);
        assert_eq!(direct.emitted, 6);
        let empty_input = vec![Vec::new()];
        direct.push_input(&empty_input, 0);
        direct.process_steps();
        assert_eq!(direct.retrieve(&mut output), 4);
        assert_eq!(output[0], [16.0, 17.0, 18.0, 19.0]);
        assert_eq!(&direct.core.side_starts[6..], &[Some(100.0), Some(100.0)]);
        direct.reset();
        assert_eq!(direct.startup_input(), 12);
        assert_eq!(direct.read_position(), 0);
        assert!(!direct.side_active);
    }
    // This core is a deterministic scheduler test double, not a pitch effect.
    // It tests queue/discard/partition behavior without claiming audio parity.
    struct TestCore { at: i64, accept: bool }
    impl ProDirectAdapter for TestCore {
        fn output_hop(&self) -> i32 { 4 }
        fn current_factor(&self) -> f32 { 1.0 }
        fn startup_input(&self) -> i32 { 4 }
        fn startup_discard(&self) -> i32 { 2 }
        fn needed_input(&self, _: i32) -> i32 { 4 }
        fn startup(&mut self, work: &mut [Vec<f32>], n: usize) -> i32 { self.step(work, n) }
        fn step(&mut self, _: &mut [Vec<f32>], n: usize) -> i32 { self.at += n as i64; n as i32 }
        fn read_position(&self) -> i64 { self.at }
        fn set_factors(&mut self, _: &mut f32, _: &mut f32, _: i32) -> bool { self.accept }
        fn reset(&mut self) { self.at = 0; }
    }

    #[test]
    fn adapter_demand_and_signed_startup_discard_are_distinct() {
        let mut adapter = ElastiqueAdapter::new(TestCore { at: 0, accept: true }, 1);
        assert!(adapter.set_factors(PitchMode::PreserveTempo, 1.0).1);
        assert_eq!(adapter.required_input(), 688);
        let input = vec![(0..688).map(|x| x as f32).collect::<Vec<_>>()];
        let mut out = vec![Vec::new()];
        adapter.process(&input, &mut out).unwrap();
        assert_eq!(out[0].len(), 512);
        // Startup arm queues its positive count from the beginning of the work
        // buffer, unlike a later negative-to-positive crossing.
        assert_eq!(&out[0][..6], &[0.0, 1.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(adapter.output.len(), 2);
        assert_eq!(adapter.input.len(), 172);
    }

    #[test]
    fn later_discard_crossing_removes_the_negative_prefix() {
        struct CrossingCore(TestCore);
        impl ProDirectAdapter for CrossingCore {
            fn output_hop(&self) -> i32 { self.0.output_hop() }
            fn current_factor(&self) -> f32 { self.0.current_factor() }
            fn startup_input(&self) -> i32 { self.0.startup_input() }
            fn startup_discard(&self) -> i32 { 6 }
            fn needed_input(&self, iteration: i32) -> i32 { self.0.needed_input(iteration) }
            fn startup(&mut self, work: &mut [Vec<f32>], n: usize) -> i32 { self.0.startup(work, n) }
            fn step(&mut self, work: &mut [Vec<f32>], n: usize) -> i32 { self.0.step(work, n) }
            fn read_position(&self) -> i64 { self.0.read_position() }
            fn set_factors(&mut self, first: &mut f32, second: &mut f32, selector: i32) -> bool {
                self.0.set_factors(first, second, selector)
            }
            fn reset(&mut self) { self.0.reset(); }
        }
        let mut adapter = ElastiqueAdapter::new(CrossingCore(TestCore { at: 0, accept: true }), 1);
        assert!(adapter.set_factors(PitchMode::PreserveTempo, 1.0).1);
        assert_eq!(adapter.required_input(), 692);
        let input = vec![(0..692).map(|x| x as f32).collect::<Vec<_>>()];
        let mut out = vec![Vec::new()];
        adapter.process(&input, &mut out).unwrap();
        assert_eq!(&out[0][..6], &[6.0, 7.0, 8.0, 9.0, 10.0, 11.0]);
    }

    #[test]
    fn fresh_unity_48k_sizing_matches_the_recorded_first_pull_count() {
        // The first pull count is captured; the internal discard below is a
        // static prediction, not measured by that capture.
        struct SizingCore;
        impl ProDirectAdapter for SizingCore {
            fn output_hop(&self) -> i32 { 512 }
            fn current_factor(&self) -> f32 { 1.0 }
            fn startup_input(&self) -> i32 { 2560 }
            fn startup_discard(&self) -> i32 { 2050 }
            fn needed_input(&self, _: i32) -> i32 { 512 }
            fn startup(&mut self, _: &mut [Vec<f32>], _: usize) -> i32 { unreachable!("sizing test does not process audio") }
            fn step(&mut self, _: &mut [Vec<f32>], _: usize) -> i32 { unreachable!("sizing test does not process audio") }
            fn read_position(&self) -> i64 { 0 }
            fn set_factors(&mut self, _: &mut f32, _: &mut f32, _: i32) -> bool { true }
            fn reset(&mut self) {}
        }
        let mut adapter = ElastiqueAdapter::new(SizingCore, 2);
        assert!(adapter.set_factors(PitchMode::PreserveTempo, 1.0).1);
        assert_eq!(adapter.required_input(), 5120);
    }

    #[test]
    fn ratio_and_mode_updates_do_not_clear_queued_output() {
        let mut au = AuScheduler::new(TestCore { at: 0, accept: true }, 1, 256);
        assert!(au.set_factors(PitchMode::PreserveTempo, 1.0));
        let mut first = [0.0; 17];
        au.render(&mut [&mut first], |start, count, planes| {
            for i in 0..count { planes[0][i] = (start as usize + i) as f32; }
            Ok(count)
        }).unwrap();
        assert_eq!(au.output.len(), 495);
        assert_eq!(au.adapter.read_position(), 514);
        assert_eq!(au.buffer_size().to_bits(), 749.0f32.to_bits());
        assert!(au.set_factors(PitchMode::Turntable, 1.01));
        assert_eq!(au.mode_factor.to_bits(), (1.0f32 / 1.01f32).to_bits());
        let expected_pending = 254.0f32 + 495.0f32 / (1.0f32 / 1.01f32);
        assert_eq!(au.buffer_size().to_bits(), expected_pending.to_bits());
        let mut next = [0.0; 17];
        let mut pulled = false;
        au.render(&mut [&mut next], |_, count, _| {
            pulled = true;
            Ok(count)
        }).unwrap();
        assert!(!pulled);
        assert_eq!(au.output.len(), 478);
    }

    #[test]
    fn rejected_setter_keeps_engine_factor_but_au_stores_its_local() {
        let mut au = AuScheduler::new(TestCore { at: 0, accept: false }, 2, 512);
        assert!(!au.set_factors(PitchMode::Turntable, 1.01));
        assert_eq!(au.mode_factor.to_bits(), (1.0f32 / 1.01f32).to_bits());
        assert_eq!(au.adapter.factor.to_bits(), 1.0f32.to_bits());
        assert_eq!(au.adapter.read_factor.to_bits(), 1.0f32.to_bits());
        assert_eq!(au.adapter.discard, 0);
        assert!(au.adapter.startup);
    }

    #[test]
    fn au_pulls_full_slices_and_reset_restarts_input_timestamp() {
        let mut au = AuScheduler::new(TestCore { at: 0, accept: true }, 1, 256);
        assert!(au.set_factors(PitchMode::PreserveTempo, 1.0));
        let mut left = [0.0; 17];
        let mut pulls = Vec::new();
        au.render(&mut [&mut left], |start, count, planes| {
            pulls.push((start as usize, count));
            for i in 0..count { planes[0][i] = (start as usize + i) as f32; }
            Ok(count)
        }).unwrap();
        assert_eq!(pulls, vec![(0, 256), (256, 256), (512, 256)]);
        au.reset();
        let mut after_reset = [0.0; 1];
        let mut first = None;
        au.render(&mut [&mut after_reset], |start, count, _| {
            first.get_or_insert((start as usize, count));
            Ok(count)
        }).unwrap();
        assert_eq!(first, Some((0, 256)));
    }
}
