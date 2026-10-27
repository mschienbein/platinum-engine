//! PSOLA analysis, marker history and synthesis selected by PitchShiftUnit.
//!
//! This is the fixed-length (`-1`, detector disabled) `0x400c0` analysis and
//! `0x3a890` synthesis.  Correlation uses the same packed-real Accelerate FFT
//! as the other pitch leaves.  FPCR is owned by the enclosing host callback.

use super::fft::{conjugate_in_place, multiply_spectra_in_place, scale_in_place, RealFft};

const PAD: usize = 8192;
const COEFFICIENTS: usize = 768;
const FADE_SIZE: usize = 16384;
const CUTOFF: f32 = f32::from_bits(0x3f67_0000);
const BETA: f32 = 9.0;

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn vvsincosf(sine: *mut f32, cosine: *mut f32, input: *const f32, count: *const i32);
    fn vDSP_vmul(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, count: usize);
}
unsafe extern "C" {
    #[link_name = "sinf"]
    fn native_sinf(value: f32) -> f32;
    #[link_name = "tanh"]
    fn native_tanh(value: f64) -> f64;
}

#[derive(Clone, Copy, Default, Debug)]
struct Marker {
    position: i32,
    energy: f32,
    transient: f32,
    voiced: f32,
    auxiliary: f32,
    period: f32,
}

impl Marker {
    fn at(position: i32) -> Self { Self { position, ..Self::default() } }
}

struct Tables {
    coefficients: Vec<f32>,
    differences: Vec<f32>,
    fade: Vec<f32>,
}

impl Tables {
    // 0x3ac20: both Bessel sums are f32, but the convergence comparison is f64.
    fn bessel(half_argument: f32) -> f32 {
        let mut term = 1.0_f32;
        let mut sum = 1.0_f32;
        let mut k = 1_i32;
        loop {
            let ratio = half_argument / k as f32;
            let ratio_squared = ratio * ratio;
            term = term * ratio_squared;
            sum = sum + term;
            k += 1;
            if (sum as f64) * 1.0e-21_f64 > term as f64 { break; }
        }
        sum
    }

    fn new() -> Self {
        let mut coefficients = vec![0.0; COEFFICIENTS];
        let mut differences = vec![0.0; COEFFICIENTS];
        coefficients[0] = CUTOFF;
        let inverse_last = 1.0_f32 / (COEFFICIENTS - 1) as f32;
        let inverse_denominator = 1.0_f32 / Self::bessel(BETA * 0.5);
        for i in 1..COEFFICIENTS {
            let x = i as f32 / 256.0_f32;
            let angle = x * f32::from_bits(0x4049_0fdb);
            // The source calls libSystem sinf, not the vector sine routine.
            let sinc = unsafe { native_sinf(angle * CUTOFF) } / angle;
            let window_position = inverse_last * i as f32;
            let radicand = 1.0_f32 - window_position * window_position;
            let half_window_argument = (radicand.sqrt() * BETA) * 0.5;
            let numerator = Self::bessel(half_window_argument);
            let window = inverse_denominator * numerator;
            let coefficient = sinc * window;
            coefficients[i] = coefficient;
            differences[i - 1] = coefficient - coefficients[i - 1];
        }
        let step = f32::from_bits(0x3fc9_0fdb) / (FADE_SIZE - 1) as f32;
        let mut angles = vec![0.0; FADE_SIZE];
        for (i, angle) in angles.iter_mut().enumerate() { *angle = i as f32 * step; }
        let mut sine = vec![0.0; FADE_SIZE];
        let mut cosine = vec![0.0; FADE_SIZE];
        let count = FADE_SIZE as i32;
        // 0x203ec writes both vectors; 0x3ac20 copies sine and multiplies it
        // by the same sine vector twice using the installed vDSP_vmul wrapper.
        unsafe {
            vvsincosf(sine.as_mut_ptr(), cosine.as_mut_ptr(), angles.as_ptr(), &count);
        }
        let mut fade = angles;
        fade.copy_from_slice(&sine);
        unsafe {
            vDSP_vmul(fade.as_ptr(), 1, sine.as_ptr(), 1, fade.as_mut_ptr(), 1, FADE_SIZE);
            vDSP_vmul(fade.as_ptr(), 1, sine.as_ptr(), 1, fade.as_mut_ptr(), 1, FADE_SIZE);
        }
        Self { coefficients, differences, fade }
    }
}

struct Analysis {
    base: usize,
    length: i32,
    half: i32,
    radius: i32,
    prior_offset: i32,
    previous_position: i32,
    previous_period: i32,
    current_transient: bool,
    previous_transient: bool,
    highpass_input: f32,
    lowpass_output: f32,
    previous_high_peak: f32,
    previous_low_peak: f32,
    input: Vec<f32>,
    spectrum_a: Vec<f32>,
    spectrum_b: Vec<f32>,
    correlation: Vec<f32>,
    fft: RealFft,
}

impl Analysis {
    fn new(rate: f32) -> Option<Self> {
        let base = if rate < 32000.0 { 256 } else if rate < 64000.0 { 512 }
            else if rate < 128000.0 { 1024 } else if rate < 256000.0 { 2048 } else { 4096 };
        Some(Self {
            base, length: base as i32, half: (base / 2) as i32, radius: (base / 2) as i32,
            prior_offset: 0, previous_position: 0, previous_period: (base / 2) as i32,
            current_transient: false, previous_transient: false,
            highpass_input: 0.0, lowpass_output: 0.0,
            previous_high_peak: 0.0, previous_low_peak: 0.0,
            input: Vec::with_capacity(base * 4), spectrum_a: vec![0.0; base * 2],
            spectrum_b: vec![0.0; base * 2], correlation: vec![0.0; base * 2],
            fft: RealFft::new(base * 2)?,
        })
    }

    fn reset(&mut self) {
        self.length = self.base as i32;
        self.half = (self.base / 2) as i32;
        self.radius = (self.base / 2) as i32;
        self.prior_offset = 0;
        self.previous_position = 0;
        self.previous_period = self.half;
        self.current_transient = false;
        self.previous_transient = false;
        self.previous_high_peak = 0.0;
        self.previous_low_peak = 0.0;
        self.input.clear();
        // 0x40500 does not reset either 0x460b4 or 0x46250's filter history.
    }

    fn correlate(&mut self) {
        let length = self.length as usize;
        let half = self.half as usize;
        let n = self.base * 2;
        self.spectrum_a.fill(0.0);
        self.spectrum_b.fill(0.0);
        self.spectrum_a[..length].copy_from_slice(&self.input[..length]);
        self.spectrum_b[..length].copy_from_slice(&self.input[half..half + length]);
        self.fft.forward(&mut self.spectrum_a);
        self.fft.forward(&mut self.spectrum_b);
        conjugate_in_place(&mut self.spectrum_b);
        multiply_spectra_in_place(&mut self.spectrum_a, &self.spectrum_b);
        scale_in_place(&mut self.spectrum_a, 1.0_f32 / n as f32);
        self.fft.inverse(&mut self.spectrum_a);
        // 0x41554 scales the correlation by the outer correlation-buffer length.
        scale_in_place(&mut self.spectrum_a, n as f32);
        self.correlation.fill(0.0);
        let first = n - length + 1;
        let tail = length - 1;
        self.correlation[1..1 + tail].copy_from_slice(&self.spectrum_a[first..]);
        self.correlation[1 + tail..1 + tail + length]
            .copy_from_slice(&self.spectrum_a[..length]);
    }

    fn analyse(&mut self, channels: &[&[f32]], markers: &mut Vec<Marker>) {
        let frames = channels[0].len();
        self.input.reserve(frames);
        for i in 0..frames {
            let mut sum = 0.0_f32;
            for channel in channels { sum = sum + channel[i]; }
            self.input.push(sum);
        }
        while self.input.len() >= 2 * self.base {
            let mut offset = self.prior_offset;
            let mut peak = -f32::MAX;
            for &sample in &self.input[..self.length as usize] {
                let magnitude = sample.abs();
                if magnitude > peak { peak = magnitude; }
            }
            if (peak as f64) < 1.0e-24_f64 {
                offset = 0;
            } else {
                self.correlate();
                let center = self.base as i32;
                let mut first = center + offset - self.radius;
                let mut last = center + offset + self.radius;
                if last > center + self.half - 1 {
                    last = center + self.half - 2;
                    first = last - 2 * self.radius;
                }
                if center + offset - self.radius < center - (self.half - 1) {
                    first = center - (self.half - 1) - 1;
                    last = first + 2 * self.radius;
                }
                let mut best = 0.0_f32;
                let mut best_index = -1;
                for index in first..=last {
                    // 0x410f0 is FRINTM, not integer truncation.
                    let value = (self.correlation[index as usize] * 10000.0_f32).floor();
                    if value > best { best = value; best_index = index; }
                }
                if best_index != -1 { offset = best_index - center; }
            }
            self.prior_offset = offset;
            let mut position = self.previous_position + self.half - offset;
            let mut period = position - self.previous_position;
            let last_position = markers.last().map_or(-1, |m| m.position);
            if position - last_position < (self.base as i32 / 4) >> 2 {
                period = self.previous_period;
                position = self.previous_position + period;
                if self.base >= 1024 {
                    self.length = self.base as i32;
                    self.half = self.length / 2;
                    self.radius = self.half;
                    self.prior_offset = self.half - period;
                }
            }
            assert!(period > 0, "nonpositive PSOLA analysis period");
            if period as usize > self.input.len() { return; }
            let n = period as usize;
            let mut high_peak = -f32::MAX;
            let mut low_peak = -f32::MAX;
            let mut energy = 0.0_f32;
            // The original first computes an entire high-pass segment, then the
            // low-pass segment, before summing energy. Their histories are separate.
            for &sample in &self.input[..n] {
                let value = sample * 0.5_f32 + (-0.5_f32 * self.highpass_input);
                self.highpass_input = sample;
                if value.abs() > high_peak { high_peak = value.abs(); }
            }
            let high_score = if high_peak > self.previous_high_peak && (high_peak as f64) > 0.01
                && !self.current_transient && !self.previous_transient {
                transient_score(high_peak, self.previous_high_peak)
            } else { 0.0 };
            self.previous_high_peak = high_peak;
            // 1 + (-0.9f), rather than decimal 0.1f, is the constructed coefficient.
            let low_coefficient = 1.0_f32 + f32::from_bits(0xbf66_6666);
            let feedback = -f32::from_bits(0xbf66_6666);
            for &sample in &self.input[..n] {
                let value = sample * low_coefficient + feedback * self.lowpass_output;
                let value = value + f32::from_bits(0x179a_be15);
                self.lowpass_output = value;
                if value.abs() > low_peak { low_peak = value.abs(); }
            }
            let low_score = if low_peak > self.previous_low_peak && (low_peak as f64) > 0.1
                && !self.current_transient && !self.previous_transient {
                transient_score(low_peak, self.previous_low_peak)
            } else { 0.0 };
            self.previous_low_peak = low_peak;
            let transient = if high_score > low_score { high_score } else { low_score };
            for &sample in &self.input[..n] { energy = energy + sample * sample; }
            energy = energy / period as f32;
            let voiced = if (low_peak as f64) < 0.02 { 0.0 } else { 12.0 };
            self.input.copy_within(n.., 0);
            self.input.truncate(self.input.len() - n);
            self.previous_position = position;
            markers.push(Marker { position, energy, transient, voiced, auxiliary: 0.0,
                                  period: (self.half - self.prior_offset) as f32 });
            self.previous_period = period;
            self.previous_transient = self.current_transient;
            self.current_transient = transient > 0.7_f32;
        }
    }
}

fn transient_score(current: f32, previous: f32) -> f32 {
    let epsilon = 1.0e-31_f64;
    let ratio = ((current as f64) + epsilon) / ((previous as f64) + epsilon);
    let difference = (current - previous) as f64;
    let sum = (current + previous) as f64;
    let argument = (ratio * difference + epsilon) / (sum + epsilon);
    unsafe { native_tanh(argument) as f32 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PsolaError { MissingMarkers }

struct Synthesis {
    channels: usize,
    tables: Tables,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    input_len: i32,
    output_end: i32,
    output_ready: i32,
    total_input: i32,
    output_hop: i32,
    overlap: i32,
    period: i32,
    previous_transient_position: i32,
    marker_position: i32,
    marker_index: i32,
    previous_index: i32,
    repetitions: i32,
    transient_length: i32,
    prior_was_transient: bool,
    interpolate: bool,
    transients: bool,
    stretch: f32,
    inverse_stretch: f32,
    pitch: f32,
    inverse_pitch: f32,
    transient_threshold: f32,
    pitch_bound: f32,
    stretch_bound: f32,
    interpolation_gain: f32,
    coefficient_step: i32,
    position: f64,
}

impl Synthesis {
    fn new(channels: usize, base: usize, hop: usize) -> Self {
        let capacity = base * 16 + PAD;
        let mut result = Self {
            channels, tables: Tables::new(),
            input: (0..channels).map(|_| vec![0.0; capacity]).collect(),
            output: (0..channels).map(|_| vec![0.0; capacity]).collect(),
            input_len: 0, output_end: 0, output_ready: 0, total_input: 0,
            output_hop: hop as i32, overlap: 0, period: 0, previous_transient_position: 0,
            marker_position: 0, marker_index: 0, previous_index: 0, repetitions: 1,
            transient_length: 1024, prior_was_transient: false,
            // Pro constructor calls 0x3cb38(1), disabling resampling at this leaf.
            interpolate: false, transients: false, stretch: 1.0, inverse_stretch: 1.0,
            pitch: 1.0, inverse_pitch: 1.0, transient_threshold: 0.7,
            pitch_bound: 1.0, stretch_bound: 1.0, interpolation_gain: 0.99,
            coefficient_step: 32768, position: 0.0,
        };
        result.set_pitch(1.0);
        result
    }

    fn reset(&mut self) {
        self.input_len = 0; self.output_end = 0; self.output_ready = 0; self.total_input = 0;
        self.overlap = 0; self.period = 0; self.previous_transient_position = 0;
        self.marker_position = 0; self.marker_index = 0; self.previous_index = 0;
        self.repetitions = 1; self.prior_was_transient = false; self.position = 0.0;
        self.set_stretch(1.0);
        self.set_pitch(1.0);
        // 0x3c95c intentionally does not clear the allocated sample buffers.
    }

    fn set_stretch(&mut self, stretch: f32) {
        self.stretch = stretch;
        self.inverse_stretch = 1.0_f32 / stretch;
        self.stretch_bound = if self.inverse_stretch > 1.0 { self.inverse_stretch } else { 1.0 };
    }

    fn set_pitch(&mut self, pitch: f32) {
        self.pitch = pitch;
        self.inverse_pitch = 1.0_f32 / pitch;
        // 0x3c89c..0x3c8bc caps the phase-table advance before widening
        // to binary64 for the +0.5 integer conversion.
        let table_step = (self.inverse_pitch * 256.0_f32).min(256.0_f32) * 128.0_f32;
        self.coefficient_step = (f64::from(table_step) + 0.5_f64) as i32;
        self.pitch_bound = if self.inverse_pitch > 1.0 { self.inverse_pitch } else { 1.0 };
        self.interpolation_gain = if self.inverse_pitch > 1.0 { 0.99_f32 }
                                  else { self.inverse_pitch * 0.99_f32 };
    }

    fn set_position(&mut self, position: f64) {
        if position < self.position { self.previous_index = 0; }
        self.position = position + (self.inverse_stretch * self.output_end as f32) as f64;
    }

    fn ensure_input(&mut self, end: i32) {
        let need = PAD + end.max(0) as usize;
        for channel in &mut self.input { if channel.len() < need { channel.resize(need, 0.0); } }
    }
    fn ensure_output(&mut self, end: i32) {
        let need = PAD + end.max(0) as usize;
        for channel in &mut self.output { if channel.len() < need { channel.resize(need, 0.0); } }
    }
    fn index(offset: i32) -> usize {
        usize::try_from(PAD as i32 + offset).expect("PSOLA history exceeds native guard")
    }

    fn nearest(markers: &[Marker], start: i32, position: i32) -> i32 {
        if start < 0 || markers.len() < 2 || start as usize >= markers.len() - 1 { return -1; }
        let mut index = start as usize;
        let mut distance = (markers[index].position - position).unsigned_abs();
        loop {
            if index + 1 == markers.len() { return index as i32; }
            let next = (markers[index + 1].position - position).unsigned_abs();
            if next >= distance { return index as i32; }
            distance = next;
            index += 1;
        }
    }

    fn marker_period(markers: &[Marker], index: i32) -> i32 {
        if index < 0 || index as usize + 1 >= markers.len() { return -1; }
        markers[index as usize + 1].position - markers[index as usize].position
    }
    fn marker_position(markers: &[Marker], index: i32) -> i32 {
        markers.get(index as usize).map_or(-1, |m| m.position)
    }
    fn marker_transient(markers: &[Marker], index: i32) -> f32 {
        markers.get(index as usize).map_or(-1.0, |m| m.transient)
    }
    fn distance_until(markers: &[Marker], index: i32, limit: i32) -> Option<(i32, i32)> {
        if index < 0 || index as usize >= markers.len() { return None; }
        let start = markers[index as usize].position;
        if limit < 0 { return Some((index, 0)); }
        for i in index as usize + 1..markers.len() {
            if markers[i].position > start + limit {
                return Some((i as i32, markers[i].position - start));
            }
        }
        None
    }

    // Native fixed-point table interpolation, shared by both interpolating
    // branches. Each accumulator and multiply is separately rounded to f32.
    fn interpolate_at(&self, channel: usize, anchor: i32, fixed: i64) -> f32 {
        let fractional = (fixed as i32) & 0x7fff;
        let integral = (fixed >> 15) as i32;
        let mut step = (self.coefficient_step * fractional) >> 15;
        let mut left = 0.0_f32;
        let mut source = anchor + integral;
        while step < (COEFFICIENTS as i32) * 128 {
            let index = (step >> 7) as usize;
            let fraction = (step & 127) as f32 * 0.0078125_f32;
            let coefficient = self.tables.coefficients[index] + self.tables.differences[index] * fraction;
            left = left + self.input[channel][Self::index(source)] * coefficient;
            source -= 1;
            step += self.coefficient_step;
        }
        let complement = (-fractional) & 0x7fff;
        step = if complement == 0 { self.coefficient_step } else { 0 }
             + ((self.coefficient_step * complement) >> 15);
        let mut right = 0.0_f32;
        source = anchor + integral + 1;
        while step < (COEFFICIENTS as i32 - 1) * 128 {
            let index = (step >> 7) as usize;
            let fraction = (step & 127) as f32 * 0.0078125_f32;
            let coefficient = self.tables.coefficients[index] + self.tables.differences[index] * fraction;
            right = right + self.input[channel][Self::index(source)] * coefficient;
            source += 1;
            step += self.coefficient_step;
        }
        left + right
    }

    fn fade_index_double(step: f32, sample: i32) -> usize {
        ((step * sample as f32) as f64 + 0.5_f64) as usize
    }
    fn fade_index_float(step: f32, sample: i32) -> usize {
        (step * sample as f32 + 0.5_f32).floor() as usize
    }
    fn fade_step(length: i32) -> f32 {
        if length < 2 { FADE_SIZE as f32 } else { (FADE_SIZE - 1) as f32 / (length - 1) as f32 }
    }

    // 0x3b28c, non-interpolating ordinary marker.
    fn overlap_add(&mut self, mut overlap: i32, period: i32, marker: i32) {
        let overlap_as_f = overlap as f32;
        let scaled_overlap = (self.pitch * overlap_as_f + 0.5).floor();
        if self.pitch < 1.0 { overlap = overlap_as_f.min(scaled_overlap) as i32; }
        let scaled_period = (self.inverse_pitch * period as f32 + 0.5).floor();
        let fade_out_len = scaled_period.min(period as f32) as i32;
        let start = self.input_len + marker - self.total_input;
        let end = self.output_end;
        self.ensure_input(start + fade_out_len + 1);
        self.ensure_output(end + (self.inverse_pitch * period as f32 + 1.5).floor() as i32 + 1);
        let step = Self::fade_step(overlap);
        for channel in 0..self.channels {
            for i in 0..overlap {
                let weight = self.tables.fade[Self::fade_index_double(step, i)];
                let source = self.input[channel][Self::index(start - overlap + i)];
                let dest = Self::index(end - overlap + i);
                self.output[channel][dest] = weight * source + self.output[channel][dest];
            }
        }
        self.output_ready = end;
        let step = Self::fade_step(fade_out_len);
        let clear = (self.inverse_pitch * period as f32 + 1.5).floor() as i32 - fade_out_len;
        for channel in 0..self.channels {
            for i in 0..fade_out_len {
                let weight = self.tables.fade[Self::fade_index_double(step, i)];
                self.output[channel][Self::index(end + i)] =
                    (1.0_f32 - weight) * self.input[channel][Self::index(start + i)];
            }
            if clear > 0 {
                let first = Self::index(end + fade_out_len);
                self.output[channel][first..first + clear as usize].fill(0.0);
            }
        }
        self.output_end = end + (self.inverse_pitch * period as f32 + 0.5).floor() as i32;
    }

    // 0x3b5b4, transient segment with a 1/16-duration crossfade.
    fn transient_copy(&mut self, overlap: i32, length: i32, marker: i32) {
        let crossfade = overlap >> 4;
        let step = Self::fade_step(crossfade);
        let start = self.input_len + marker - self.total_input - overlap;
        let dest = self.output_end - overlap;
        let remaining = length + overlap - crossfade;
        assert!(remaining >= 0, "native transient copy has negative length");
        self.ensure_input(start + crossfade + remaining + 1);
        self.ensure_output(dest + crossfade + remaining + 1);
        for channel in 0..self.channels {
            for i in 0..crossfade {
                let weight = self.tables.fade[Self::fade_index_double(step, i)];
                let at = Self::index(dest + i);
                let old = (1.0_f32 - weight) * self.output[channel][at];
                self.output[channel][at] = old + weight * self.input[channel][Self::index(start + i)];
            }
            if remaining > 0 {
                let input = Self::index(start + crossfade);
                let output = Self::index(dest + crossfade);
                self.output[channel][output..output + remaining as usize]
                    .copy_from_slice(&self.input[channel][input..input + remaining as usize]);
            }
        }
        self.output_end = dest + crossfade + remaining;
        self.output_ready = self.output_end - overlap;
    }

    // 0x3b748, ordinary marker with 15-bit interpolated source positions.
    fn overlap_interpolated(&mut self, overlap: i32, period: i32, marker: i32) {
        let end = self.output_end;
        let anchor = self.input_len + marker - self.total_input;
        let shifted = anchor - (self.pitch * overlap as f32 + 0.5).floor() as i32;
        let generated = (self.inverse_pitch * period as f32 + 0.5).floor() as i32;
        self.ensure_input(anchor + (self.pitch * generated as f32).ceil() as i32 + 16);
        self.ensure_output(end + generated + 1);
        let step = Self::fade_step(overlap);
        for channel in 0..self.channels {
            for i in 0..overlap {
                let weight = self.tables.fade[Self::fade_index_float(step, i)];
                let fixed = ((self.pitch * i as f32) * 32768.0_f32 + 0.5_f32) as i64;
                let interpolated = self.interpolate_at(channel, shifted, fixed);
                let at = Self::index(end - overlap + i);
                let old = (1.0_f32 - weight) * self.output[channel][at];
                self.output[channel][at] = old + (weight * self.interpolation_gain) * interpolated;
            }
        }
        self.output_ready = end;
        for channel in 0..self.channels {
            for i in 0..generated {
                let fixed = ((i as f32 * self.pitch) * 32768.0_f32 + 0.5_f32).floor() as i64;
                let value = self.interpolate_at(channel, anchor, fixed);
                self.output[channel][Self::index(end + i)] = self.interpolation_gain * value;
            }
        }
        self.output_end = end + generated;
    }

    // 0x3bb58, transient branch; unlike 0x3b748 the 15-bit position truncates.
    fn transient_interpolated(&mut self, overlap: i32, length: i32, marker: i32) {
        let crossfade = overlap >> 4;
        let step = Self::fade_step(crossfade);
        let start = self.input_len + marker - self.total_input - overlap;
        let dest = self.output_end - overlap;
        let source_len = length + overlap - crossfade;
        let generated = (self.inverse_pitch * source_len as f32) as i32;
        let actual_generated = (self.inverse_pitch * source_len as f32).ceil().max(0.0) as i32;
        let second_start = start + (self.pitch * crossfade as f32) as i32;
        self.ensure_input(second_start + (self.pitch * actual_generated as f32).ceil() as i32 + 16);
        self.ensure_output(dest + crossfade + actual_generated + 1);
        for channel in 0..self.channels {
            for i in 0..crossfade {
                let weight = self.tables.fade[Self::fade_index_double(step, i)];
                let fixed = ((self.pitch * i as f32) * 32768.0_f32) as i64;
                let value = self.interpolate_at(channel, start, fixed);
                let at = Self::index(dest + i);
                let old = (1.0_f32 - weight) * self.output[channel][at];
                self.output[channel][at] = old + (weight * self.interpolation_gain) * value;
            }
            for i in 0..actual_generated {
                let fixed = ((i as f32 * self.pitch) * 32768.0_f32) as i64;
                let value = self.interpolate_at(channel, second_start, fixed);
                self.output[channel][Self::index(dest + crossfade + i)] = self.interpolation_gain * value;
            }
        }
        self.output_end = dest + crossfade + generated;
        self.output_ready = self.output_end - (self.inverse_pitch * overlap as f32) as i32;
    }

    fn append_input(&mut self, input: &[&[f32]]) {
        let frames = input[0].len() as i32;
        self.ensure_input(self.input_len + frames);
        if frames > 0 {
            for channel in 0..self.channels {
                let at = Self::index(self.input_len);
                self.input[channel][at..at + frames as usize].copy_from_slice(input[channel]);
            }
            self.input_len += frames;
            self.total_input += frames;
        }
    }

    fn process_buffered(&mut self, markers: &[Marker], output: &mut [&mut [f32]]) -> Result<(), PsolaError> {
        if self.input_len > 0 {
            loop {
                let deferred = if self.transients { self.overlap * self.repetitions } else { 0 };
                if self.output_ready >= self.output_hop + deferred { break; }
                let mut index = Self::nearest(markers, self.previous_index, self.position as i32);
                if index < 0 {
                    if self.repetitions != 1 || self.previous_index < 0 { return Err(PsolaError::MissingMarkers); }
                    index = self.previous_index;
                }
                if self.transients && self.previous_index + 1 < index
                    && Self::marker_transient(markers, index - 1) > self.transient_threshold {
                    index -= 1;
                }
                self.marker_index = index;
                let period = Self::marker_period(markers, index);
                self.period = if period < 0 { (self.pitch * self.overlap as f32 + 0.5) as i32 } else { period };
                self.marker_position = Self::marker_position(markers, index);
                let transient = !self.prior_was_transient && self.transients
                    && Self::marker_transient(markers, index) > self.transient_threshold;
                if transient {
                    let limit = (self.inverse_pitch * self.transient_length as f32) as i32;
                    // Unlike the sizing predictor, 0x3c2ac does not reject
                    // 0x45dcc's -1 and leaves the supplied marker index unchanged.
                    let (next, mut length) = Self::distance_until(markers, index, limit)
                        .unwrap_or((index, -1));
                    for later in index + 1..next {
                        if Self::marker_transient(markers, later) > self.transient_threshold {
                            length = Self::marker_position(markers, later) - self.marker_position;
                            break;
                        }
                    }
                    let overlap = (self.overlap * self.repetitions)
                        .min((self.marker_position - self.previous_transient_position) >> 1);
                    if self.interpolate && self.pitch != 1.0 {
                        self.transient_interpolated(overlap, length, self.marker_position);
                    } else { self.transient_copy(overlap, length, self.marker_position); }
                    self.period = length;
                    self.previous_transient_position = self.marker_position;
                    self.prior_was_transient = true;
                } else {
                    if self.interpolate && self.pitch != 1.0 {
                        self.overlap_interpolated(self.overlap, self.period, self.marker_position);
                    } else { self.overlap_add(self.overlap, self.period, self.marker_position); }
                    self.prior_was_transient = false;
                }
                self.repetitions = if index == self.previous_index { self.repetitions + 1 } else { 1 };
                self.previous_index = index;
                self.overlap = ((self.inverse_pitch * self.period as f32 + 0.5).floor() as i32).max(0);
                self.position += (self.inverse_stretch as f64) * self.overlap as f64;
            }
            let repeated = self.overlap * self.repetitions;
            let discard = self.input_len - (repeated + self.total_input) + self.marker_position - PAD as i32;
            if discard > 0 {
                self.input_len -= discard;
                for channel in &mut self.input {
                    let from = Self::index(discard);
                    channel.copy_within(from..from + self.input_len as usize, PAD);
                }
            }
        } else if self.output_ready < self.output_hop {
            return Ok(());
        }
        self.output_end -= self.output_hop;
        self.output_ready -= self.output_hop;
        for channel in 0..self.channels {
            output[channel].copy_from_slice(&self.output[channel][PAD..PAD + self.output_hop as usize]);
            self.output[channel].copy_within(PAD + self.output_hop as usize..
                PAD + self.output_hop as usize + self.output_end as usize, PAD);
        }
        Ok(())
    }

    // 0x3bf60 predicts source extent without mutating the synthesis state.
    fn required_input(&self, markers: &[Marker], hop: i32, target_position: f64) -> Option<usize> {
        if markers.is_empty() { return None; }
        let mut overlap = self.overlap;
        let mut previous = if target_position < self.position { 0 } else { self.previous_index };
        let mut repetitions = self.repetitions;
        let mut previous_transient = self.previous_transient_position;
        let mut was_transient = self.prior_was_transient;
        let mut position = target_position + (self.inverse_stretch * self.output_end as f32) as f64;
        let mut end = self.output_end;
        let mut ready = self.output_ready;
        let mut required = 0_i32;
        if ready >= hop.wrapping_add(if self.transients { overlap.wrapping_mul(repetitions) } else { 0 }) {
            return Some(0);
        }
        end -= ready;
        loop {
            let mut index = Self::nearest(markers, previous, position as i32);
            let strength = Self::marker_transient(markers, index);
            if index == previous && strength > self.transient_threshold { index = previous + 1; }
            if index < 0 { return None; }
            if self.transients && previous + 1 < index
                && Self::marker_transient(markers, index - 1) > self.transient_threshold { index -= 1; }
            let period = Self::marker_period(markers, index);
            if period < 0 { return None; }
            let mut amount = end as f32;
            let prospective_period;
            if !was_transient && self.transients && Self::marker_transient(markers, index) > self.transient_threshold {
                let (next, mut length) = Self::distance_until(markers, index,
                    (self.inverse_pitch * self.transient_length as f32) as i32)?;
                let marker = Self::marker_position(markers, index);
                for later in index + 1..next {
                    if Self::marker_transient(markers, later) > self.transient_threshold {
                        length = Self::marker_position(markers, later) - marker;
                        break;
                    }
                }
                let span = (overlap * repetitions).min((marker - previous_transient) >> 1);
                amount = amount + self.inverse_pitch * (length - span) as f32;
                let advance = (self.inverse_stretch * (length - span) as f32) * self.inverse_pitch;
                position += advance as f64;
                prospective_period = span;
                required = length + marker - self.total_input;
                previous_transient = marker;
                was_transient = true;
            } else {
                let marker = Self::marker_position(markers, index);
                let extra = ((self.pitch_bound * period as f32) * self.stretch_bound) as i32;
                required = required.max(marker - self.total_input + extra);
                prospective_period = period;
                was_transient = false;
            }
            repetitions = if index == previous { repetitions.wrapping_add(1) } else { 1 };
            overlap = ((self.inverse_pitch * prospective_period as f32 + 0.5).floor() as i32).max(2);
            let advance = ((self.inverse_stretch * self.inverse_pitch) * self.period as f32) as i32 as f32;
            position += advance as f64;
            ready = ready.wrapping_add(amount as i32);
            end = overlap;
            previous = index;
            if ready >= hop.wrapping_add(if self.transients { overlap.wrapping_mul(repetitions) } else { 0 }) {
                break;
            }
        }
        Some(required.max(0) as usize)
    }
}

/// Shared PSOLA leaf. Inputs are the splitter's one or two planar f32 channels.
pub(crate) struct Psola {
    analysis: Analysis,
    synthesis: Synthesis,
    markers: Vec<Marker>,
}

impl Psola {
    pub(crate) fn new(rate: f32, channels: usize, output_hop: usize) -> Option<Self> {
        if !(8000.0..=384000.0).contains(&rate) || !(1..=2).contains(&channels)
            || output_hop > i32::MAX as usize { return None; }
        let analysis = Analysis::new(rate)?;
        let synthesis = Synthesis::new(channels, analysis.base, output_hop);
        Some(Self { analysis, synthesis, markers: vec![Marker::at(0)] })
    }

    pub(crate) fn reset(&mut self) {
        self.analysis.reset();
        self.markers.clear();
        self.markers.push(Marker::at(0));
        self.synthesis.reset();
    }

    pub(crate) fn set_stretch(&mut self, quantized_product: f32) {
        self.synthesis.set_stretch(quantized_product);
    }

    /// 0x3e870's prime path analyzes without the normal step-1 queue trimming.
    pub(crate) fn analyse_prime(&mut self, input: &[&[f32]]) {
        assert_eq!(input.len(), self.synthesis.channels);
        assert!(input.iter().all(|c| c.len() == input[0].len()));
        self.analysis.analyse(input, &mut self.markers);
    }

    pub(crate) fn analyse(&mut self, input: &[&[f32]]) {
        self.analyse_prime(input);
        if self.markers.len() > 256 && self.synthesis.previous_index > 128 {
            let keep = self.markers.len() - 128;
            self.markers.copy_within(128.., 0);
            self.markers.truncate(keep);
            self.synthesis.marker_index -= 128;
            self.synthesis.previous_index -= 128;
        }
    }

    pub(crate) fn required_input(&self, output_hop: usize, position: f64) -> Option<usize> {
        self.synthesis.required_input(&self.markers, i32::try_from(output_hop).ok()?, position)
    }

    pub(crate) fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]], position: f64)
        -> Result<(), PsolaError> {
        assert_eq!(input.len(), self.synthesis.channels);
        assert_eq!(output.len(), self.synthesis.channels);
        assert!(input.iter().all(|c| c.len() == input[0].len()));
        assert!(output.iter().all(|c| c.len() == output[0].len()));
        self.synthesis.set_position(position);
        self.synthesis.output_hop = i32::try_from(output[0].len()).expect("PSOLA output hop exceeds i32");
        self.synthesis.append_input(input);
        self.synthesis.process_buffered(&self.markers, output)
    }

    /// The native step reads and writes the same planar work buffer. Input is
    /// first appended to synthesis history, before any output plane is written.
    pub(crate) fn process_in_place(&mut self, planes: &mut [Vec<f32>], input_frames: usize,
                                   output_frames: usize, position: f64) -> Result<(), PsolaError> {
        self.process_in_place_impl(planes, input_frames, output_frames, Some(position))
    }

    /// 0x3e870 sets output hop to zero but does not update synthesis position.
    pub(crate) fn prime_in_place(&mut self, planes: &mut [Vec<f32>], input_frames: usize)
        -> Result<(), PsolaError> {
        self.process_in_place_impl(planes, input_frames, 0, None)
    }

    fn process_in_place_impl(&mut self, planes: &mut [Vec<f32>], input_frames: usize,
                             output_frames: usize, position: Option<f64>) -> Result<(), PsolaError> {
        assert_eq!(planes.len(), self.synthesis.channels);
        assert!(planes.iter().all(|plane| plane.len() >= input_frames.max(output_frames)));
        if let Some(position) = position { self.synthesis.set_position(position); }
        self.synthesis.output_hop = i32::try_from(output_frames).expect("PSOLA output hop exceeds i32");
        match planes {
            [mono] => {
                self.synthesis.append_input(&[&mono[..input_frames]]);
                self.synthesis.process_buffered(&self.markers, &mut [&mut mono[..output_frames]])
            }
            [left, right] => {
                self.synthesis.append_input(&[&left[..input_frames], &right[..input_frames]]);
                self.synthesis.process_buffered(&self.markers,
                    &mut [&mut left[..output_frames], &mut right[..output_frames]])
            }
            _ => unreachable!("PSOLA only accepts one or two channels"),
        }
    }
    #[cfg(test)]
    pub(crate) fn reference_tables(&self) -> (&[f32], &[f32], &[f32]) {
        (&self.synthesis.tables.coefficients, &self.synthesis.tables.differences,
         &self.synthesis.tables.fade)
    }

    #[cfg(test)]
    pub(crate) fn reference_markers(&self) -> Vec<[u32; 6]> {
        self.markers.iter().map(|m| [m.position as u32, m.energy.to_bits(),
            m.transient.to_bits(), m.voiced.to_bits(), m.auxiliary.to_bits(),
            m.period.to_bits()]).collect()
    }

    #[cfg(test)]
    pub(crate) fn reference_state(&self) -> (i32, i32, bool) {
        (self.synthesis.previous_index, self.synthesis.output_ready,
         self.synthesis.prior_was_transient)
    }

    #[cfg(test)]
    pub(crate) fn reference_position(&self) -> u64 { self.synthesis.position.to_bits() }

    #[cfg(test)]
    pub(crate) fn reference_branches(&mut self, pitch: f32, interpolate: bool, transients: bool) {
        self.synthesis.set_pitch(pitch);
        self.synthesis.interpolate = interpolate;
        self.synthesis.transients = transients;
    }
}
