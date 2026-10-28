//! Selected `CSpectralEnvelopeV3` and `CFFTSplitV3` ARM64 signal leaves.
//!
//! These are the PitchShiftUnit's vDSP backend and do not constitute the
//! complete pitch processor. The caller owns the factor scheduling and FPCR.

use super::fft::{
    RealFft, add_in_place, divide_in_place, exp_in_place, log_floor_in_place, magnitude,
    max_value, maximum_in_place, multiply_in_place, scale_in_place, subtract_in_place,
};

const PI: f32 = f32::from_bits(0x4049_0fdb);
const LOG_FLOOR: f32 = f32::from_bits(0x179a_be15);

#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" {
    #[link_name = "sinf"]
    fn host_sinf(value: f32) -> f32;
    #[link_name = "cosf"]
    fn host_cosf(value: f32) -> f32;
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::math::{sinf as host_sinf, cosf as host_cosf};

/// The cepstral envelope iteration at `0x3a1c0` for the selected split layout.
pub struct SpectralEnvelope {
    fft: RealFft,
    size: usize,
    threshold: f32,
    lifter: Vec<f32>,
    work: Vec<f32>,
    previous: Vec<f32>,
}

impl SpectralEnvelope {
    pub fn new(size: usize, lifter_bins: usize) -> Option<Self> {
        let mut this = Self {
            fft: RealFft::new(size)?,
            size,
            threshold: 1.0,
            lifter: vec![0.0; size],
            work: vec![0.0; size],
            previous: vec![0.0; size],
        };
        this.set_lifter_bins(lifter_bins);
        Some(this)
    }

    /// `0x3a510`: bins are clamped to N/2; the remainder is zero.
    pub fn set_lifter_bins(&mut self, bins: usize) {
        let bins = bins.min(self.size / 2);
        self.lifter.fill(0.0);
        if bins != 0 {
            let denominator = (bins * 2 - 1) as f32;
            for (index, value) in self.lifter[..bins].iter_mut().enumerate() {
                let angle = ((index as f32) * PI) / denominator;
                let cosine = unsafe { host_cosf(angle) };
                *value = cosine * cosine;
            }
        }
    }

    /// Input's first N/2 values are nonnegative magnitudes; like the original,
    /// the whole input is overwritten by its symmetric, iteratively raised log
    /// spectrum. Output receives N envelope values, with both halves equal.
    pub fn calculate(&mut self, input: &mut [f32], output: &mut [f32]) {
        assert_eq!(input.len(), self.size);
        assert_eq!(output.len(), self.size);
        let half = self.size / 2;
        log_floor_in_place(&mut input[..half], LOG_FLOOR);
        for index in 1..half {
            input[self.size - index] = input[index];
        }
        input[half] = input[half - 1];
        self.previous.fill(0.0);
        let mut maximum = 1.0e24_f32;
        while self.threshold < maximum {
            self.fft.forward_from(input, &mut self.work);
            subtract_in_place(&mut self.work, &self.previous);
            multiply_in_place(&mut self.work, &self.lifter);
            add_in_place(&mut self.work, &self.previous);
            self.previous.copy_from_slice(&self.work);
            self.fft.inverse_from(&self.work, output);
            scale_in_place(output, 1.0 / self.size as f32);
            maximum_in_place(input, output);
            self.work.copy_from_slice(input);
            subtract_in_place(&mut self.work, output);
            maximum = max_value(&self.work).0;
        }
        let (real, imaginary) = output.split_at_mut(half);
        imaginary.copy_from_slice(real);
        exp_in_place(output);
    }
}

struct Channel {
    input: Vec<f32>,
    full: Vec<f32>,
    high: Vec<f32>,
}

/// Stateful selected FFT splitter (`0x37f64`, `0x38198`, `0x38de0`).
/// The first output is the envelope-adjusted full signal; the second is the
/// independently tapered high-frequency component used by the PSOLA path.
pub struct FftSplitter {
    size: usize,
    hop: usize,
    queued: usize,
    cutoff_bin: usize,
    unchanged_bins: usize,
    ratio: f32,
    window: Vec<f32>,
    channels: Vec<Channel>,
    fft: RealFft,
    envelope: SpectralEnvelope,
    spectrum: Vec<f32>,
    magnitudes: Vec<f32>,
    envelope_values: Vec<f32>,
    temporary: Vec<f32>,
}

impl FftSplitter {
    /// The selected Pro constructor supplies cutoff 1500 Hz and N equal to
    /// one quarter of the phase-vocoder size. The safe interface rejects
    /// parameters that would make the original's unchecked array accesses
    /// leave its allocated buffers.
    pub fn new(sample_rate: u32, size: usize, channels: usize, cutoff_hz: f32) -> Option<Self> {
        if sample_rate < 8000 || size < 64 || size > i32::MAX as usize ||
           !size.is_power_of_two() || channels == 0 || channels > 48 ||
           !cutoff_hz.is_finite() || cutoff_hz < 0.0 || cutoff_hz > sample_rate as f32 * 0.5 {
            return None;
        }
        let hop = size / 2;
        let inverse_size = 1.0_f32 / size as f32;
        let root = inverse_size.sqrt();
        let mut window = vec![0.0; size];
        for (index, value) in window.iter_mut().enumerate() {
            // The 0x38198 ARM64 sequence is (f32(index)*pi)*inverse_size,
            // then libSystem sinf and a separate f32 multiply by sqrt(1/N).
            let angle = ((index as f32) * PI) * inverse_size;
            *value = unsafe { host_sinf(angle) } * root;
        }
        let cutoff_bin = ((size as f32 * cutoff_hz) / sample_rate as f32) as usize;
        let lifter_bins = (((sample_rate as f32 / 44100.0) * 128.0) as usize).min(size / 4);
        let unchanged_bins = if lifter_bins == 0 { 0 } else { size / lifter_bins };
        let mut this = Self {
            size,
            hop,
            queued: hop,
            cutoff_bin,
            unchanged_bins,
            ratio: 1.0,
            window,
            channels: (0..channels).map(|_| Channel {
                input: vec![0.0; size + hop], full: vec![0.0; size], high: vec![0.0; size],
            }).collect(),
            fft: RealFft::new(size)?,
            envelope: SpectralEnvelope::new(size, lifter_bins)?,
            spectrum: vec![0.0; size],
            magnitudes: vec![0.0; size],
            envelope_values: vec![0.0; size],
            temporary: vec![0.0; size],
        };
        this.reset();
        Some(this)
    }

    // Match the controlled native intervention on the splitter FFT only.
    #[cfg(test)]
    pub(super) fn use_offset48_for_test(&mut self) {
        self.fft.use_offset48_for_test();
    }

    /// Pro's reset calls `0x39ae4`, which restores ratio 1 and the half-window
    /// input/output history. The window and lifter coefficients are retained.
    pub fn reset(&mut self) {
        self.queued = self.hop;
        self.ratio = 1.0;
        for channel in &mut self.channels {
            channel.input[..self.hop].fill(0.0);
            channel.full[..self.hop].fill(0.0);
            channel.high[..self.hop].fill(0.0);
        }
        self.spectrum.fill(0.0);
    }

    /// Set the `0x39c04` envelope index factor. Pro supplies positive finite
    /// values; negative/nonfinite indices would access outside native buffers.
    pub fn set_envelope_ratio(&mut self, ratio: f32) {
        assert!(ratio.is_finite() && ratio > 0.0);
        self.ratio = ratio;
    }

    /// Number of frames `process` will emit for the supplied input count.
    pub fn output_frames_for(&self, frames: usize) -> usize {
        let total = self.queued.checked_add(frames).expect("splitter input overflow");
        if total < self.size { 0 } else { ((total - self.size) / self.hop + 1) * self.hop }
    }

    /// Consume equal-length planar channels and append complete half-window
    /// hops into caller-owned planar output slices. The count is returned.
    pub fn process(&mut self, input: &[&[f32]], full: &mut [&mut [f32]], high: &mut [&mut [f32]]) -> usize {
        assert_eq!(input.len(), self.channels.len());
        let frames = input[0].len();
        assert!(input.iter().all(|channel| channel.len() == frames));
        assert_eq!(full.len(), self.channels.len());
        assert_eq!(high.len(), self.channels.len());
        let produced = self.output_frames_for(frames);
        assert!(full.iter().all(|channel| channel.len() >= produced));
        assert!(high.iter().all(|channel| channel.len() >= produced));
        let total = self.queued.checked_add(frames).expect("splitter input overflow");
        for (index, channel) in self.channels.iter_mut().enumerate() {
            if channel.input.len() < total {
                channel.input.resize(total, 0.0);
            }
            for (destination, sample) in channel.input[self.queued..total].iter_mut().zip(input[index]) {
                // The selected call passes +0.0 in s0; preserve its FADD.
                *destination = *sample + 0.0;
            }
        }
        self.queued = total;
        let mut written = 0;
        while self.queued >= self.size {
            self.process_window();
            self.queued -= self.hop;
            for (index, channel) in self.channels.iter_mut().enumerate() {
                full[index][written..written + self.hop].copy_from_slice(&channel.full[..self.hop]);
                high[index][written..written + self.hop].copy_from_slice(&channel.high[..self.hop]);
                channel.input.copy_within(self.hop..self.hop + self.queued, 0);
                channel.input[self.queued..self.queued + self.hop].fill(0.0);
                channel.full.copy_within(self.hop..self.size, 0);
                channel.full[self.hop..].fill(0.0);
                channel.high.copy_within(self.hop..self.size, 0);
                channel.high[self.hop..].fill(0.0);
            }
            written += self.hop;
        }
        written
    }

    fn process_window(&mut self) {
        let half = self.hop;
        for channel in &mut self.channels {
            self.spectrum.copy_from_slice(&channel.input[..self.size]);
            multiply_in_place(&mut self.spectrum, &self.window);
            self.fft.forward(&mut self.spectrum);
            magnitude(&mut self.spectrum, &mut self.magnitudes[..half]);
            if self.ratio == 1.0 {
                self.temporary.copy_from_slice(&channel.input[..self.size]);
                scale_in_place(&mut self.temporary, self.size as f32);
                multiply_in_place(&mut self.temporary, &self.window);
            } else {
                self.envelope.calculate(&mut self.magnitudes, &mut self.envelope_values);
                divide_in_place(&mut self.spectrum, &self.envelope_values);
                apply_shifted_envelope(&mut self.spectrum, &self.envelope_values,
                                       self.unchanged_bins, self.ratio);
                self.fft.inverse_from(&self.spectrum, &mut self.temporary);
            }
            multiply_in_place(&mut self.temporary, &self.window);
            add_in_place(&mut channel.full, &self.temporary);
            apply_low_cut(&mut self.spectrum, self.cutoff_bin);
            self.fft.inverse(&mut self.spectrum);
            multiply_in_place(&mut self.spectrum, &self.window);
            add_in_place(&mut channel.high, &self.spectrum);
        }
    }
}

/// `0x39600`, backend id 10, split-real layout. Do not interpolate the
/// envelope: the selected code rounds the shifted index by adding 0.5 then
/// truncating, and zeroes bins whose shifted index is beyond N/2.
fn apply_shifted_envelope(spectrum: &mut [f32], envelope: &[f32], unchanged: usize, ratio: f32) {
    let half = spectrum.len() / 2;
    for index in 0..half {
        let source = if index < unchanged { index } else { ((index as f32 * ratio) + 0.5) as usize };
        let gain = if source < half { envelope[source] } else { 0.0 };
        spectrum[index] *= gain;
        spectrum[half + index] *= gain;
    }
}

/// `0x384c4`, backend id 10, start-bin zero and a short linear taper.
fn apply_low_cut(spectrum: &mut [f32], cutoff: usize) {
    let half = spectrum.len() / 2;
    let taper = ((spectrum.len() >> 6).min(16)) >> 1;
    let zero_end = cutoff.saturating_sub(taper).min(half);
    spectrum[..zero_end].fill(0.0);
    spectrum[half..half + zero_end].fill(0.0);
    for index in 0..taper {
        let bin = (cutoff as isize - taper as isize + index as isize).max(0) as usize;
        if bin >= half { break; }
        let scale = index as f32 / taper as f32;
        spectrum[bin] = scale * spectrum[bin];
        spectrum[half + bin] = scale * spectrum[half + bin];
    }
}
