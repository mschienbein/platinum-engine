//! tONaRT tuning analysis used by the first Platinum Notes processing pass.
//!
//! This is the ARM64/vDSP path, separate from Elastique pitch correction. Each
//! call to `process_interleaved` is one original render callback: the reference
//! resampler's output-count rounding is callback-partition sensitive. The
//! reference does not flush a partial final analysis window automatically.
//! Exact-file parity still requires comparison with the pinned application.

use std::error::Error;
use std::ffi::c_void;
use std::fmt;

use crate::math;
use crate::portable::arm_scalar::ArmFloat;

const FRAME: usize = 4096;
const HALF: usize = FRAME / 2;
const HOP: usize = 512;
const BINS: usize = 256;
const MAX_CALLBACK: usize = 16384;
const DETECTOR_RATE: f32 = 11025.0;
const FIRST_FREQUENCY: f32 = f32::from_bits(0x41cf_a6fe);
const PI: f32 = f32::from_bits(0x4049_0fdb);
const TWO_PI: f32 = f32::from_bits(0x40c9_0fdb);
const LN_2: f32 = f32::from_bits(0x3f31_7218);
const RMS_GATE: f32 = f32::from_bits(0x3800_0000);
const WHITEN_GATE: f32 = f32::from_bits(0x38d1_b717);
const QUALITY: usize = 6;
const SINC_TABLE: usize = QUALITY * 256;

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[repr(C)]
struct SplitComplex {
    real: *mut f32,
    imaginary: *mut f32,
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::vdsp::{SplitComplex, vDSP_ctoz, vDSP_zvabs, vDSP_dotpr, vDSP_rmsqv,
    vDSP_maxvi, vDSP_vadd, vDSP_vsub, vDSP_vmul, vDSP_vdiv, vDSP_vsmul};

#[repr(C)]
struct Sincos {
    sine: f32,
    cosine: f32,
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn vDSP_create_fftsetup(log2n: usize, radix: i32) -> *mut c_void;
    fn vDSP_destroy_fftsetup(setup: *mut c_void);
    fn vDSP_ctoz(input: *const f32, input_stride: isize, output: *mut SplitComplex,
                 output_stride: isize, length: usize);
    fn vDSP_fft_zrip(setup: *mut c_void, data: *mut SplitComplex, stride: isize,
                     log2n: usize, direction: i32);
    fn vDSP_zvabs(data: *const SplitComplex, input_stride: isize, output: *mut f32,
                  output_stride: isize, length: usize);
    fn vDSP_dotpr(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                  result: *mut f32, length: usize);
    fn vDSP_rmsqv(a: *const f32, stride: isize, result: *mut f32, length: usize);
    fn vDSP_maxvi(a: *const f32, stride: isize, value: *mut f32,
                  index: *mut usize, length: usize);
    fn vDSP_vadd(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vsub(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vmul(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vdiv(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vsmul(input: *const f32, input_stride: isize, factor: *const f32,
                  output: *mut f32, output_stride: isize, length: usize);
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" {
    #[link_name = "sinf"] fn host_sinf(value: f32) -> f32;
    #[link_name = "cosf"] fn host_cosf(value: f32) -> f32;
    #[link_name = "atan2f"] fn host_atan2f(y: f32, x: f32) -> f32;
    #[link_name = "fmodf"] fn host_fmodf(x: f32, y: f32) -> f32;
    #[link_name = "__sincosf_stret"] fn host_sincosf(value: f32) -> Sincos;
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::math::{sinf as host_sinf, cosf as host_cosf, atan2f as host_atan2f, fmodf as host_fmodf};
#[cfg(any(target_os = "linux", feature = "portable"))]
fn host_sincosf(value: f32) -> Sincos {
    let pair = crate::portable::math::sincosf(value);
    Sincos { sine: pair.sine, cosine: pair.cosine }
}

#[inline] fn sinf(x: f32) -> f32 { unsafe { host_sinf(x) } }
#[inline] fn cosf(x: f32) -> f32 { unsafe { host_cosf(x) } }
#[inline] fn atan2f(y: f32, x: f32) -> f32 { unsafe { host_atan2f(y, x) } }
#[inline] fn fmodf(x: f32, y: f32) -> f32 { unsafe { host_fmodf(x, y) } }

#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut result = 0.0;
    // Both ranges contain `len` floats. The reference calls this exact API.
    unsafe { vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut result, a.len()) };
    result
}

#[inline]
fn add_in_place(a: &mut [f32], b: &[f32]) {
    debug_assert_eq!(a.len(), b.len());
    unsafe { vDSP_vadd(a.as_ptr(), 1, b.as_ptr(), 1, a.as_mut_ptr(), 1, a.len()) };
}

#[inline]
fn sub_in_place(a: &mut [f32], b: &[f32]) {
    debug_assert_eq!(a.len(), b.len());
    // vDSP_vsub computes its second vector minus its first.
    unsafe { vDSP_vsub(b.as_ptr(), 1, a.as_ptr(), 1, a.as_mut_ptr(), 1, a.len()) };
}

#[inline]
fn mul_in_place(a: &mut [f32], b: &[f32]) {
    debug_assert_eq!(a.len(), b.len());
    unsafe { vDSP_vmul(a.as_ptr(), 1, b.as_ptr(), 1, a.as_mut_ptr(), 1, a.len()) };
}

#[inline]
fn scale_in_place(a: &mut [f32], factor: f32) {
    unsafe { vDSP_vsmul(a.as_ptr(), 1, &factor, a.as_mut_ptr(), 1, a.len()) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PitchDetectionError {
    InvalidSampleRate,
    InvalidChannelCount,
    IncompleteFrame,
    CallbackTooLarge,
    AllocationFailed,
    FftSetupFailed,
}

impl fmt::Display for PitchDetectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidSampleRate => "unsupported tuning detector sample rate",
            Self::InvalidChannelCount => "tuning detector channel count must be a positive i32",
            Self::IncompleteFrame => "interleaved samples contain an incomplete frame",
            Self::CallbackTooLarge => "tuning detector callbacks are limited to 16384 frames",
            Self::AllocationFailed => "unable to allocate tuning detector state",
            Self::FftSetupFailed => "unable to create the vDSP tuning FFT",
        })
    }
}
impl Error for PitchDetectionError {}

fn zeros(len: usize) -> Result<Vec<f32>, PitchDetectionError> {
    let mut data = Vec::new();
    data.try_reserve_exact(len).map_err(|_| PitchDetectionError::AllocationFailed)?;
    data.resize(len, 0.0);
    Ok(data)
}

#[repr(C, align(64))]
struct PackedBlock([f32; 16]);
struct PackedBuffer(Box<[PackedBlock]>);
impl PackedBuffer {
    fn new() -> Result<Self, PitchDetectionError> {
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(FRAME / 16).map_err(|_| PitchDetectionError::AllocationFailed)?;
        blocks.resize_with(FRAME / 16, || PackedBlock([0.0; 16]));
        Ok(Self(blocks.into_boxed_slice()))
    }
}
impl std::ops::Deref for PackedBuffer {
    type Target = [f32];
    fn deref(&self) -> &[f32] {
        // PackedBlock has exactly 16 contiguous initialized f32s and no padding.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast(), FRAME) }
    }
}
impl std::ops::DerefMut for PackedBuffer {
    fn deref_mut(&mut self) -> &mut [f32] {
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), FRAME) }
    }
}

// 0x10007d17c/1cc/474: the selected six-lobe sinc/Bessel table and
// consecutive first differences. The Bessel termination comparison is f64.
fn sinc_table() -> ([f32; SINC_TABLE], [f32; SINC_TABLE]) {
    let mut table = [0.0; SINC_TABLE];
    let mut difference = [0.0; SINC_TABLE];
    let cutoff = (((QUALITY as f32) * 0.025) * 0.03125) + 0.9;
    table[0] = cutoff;
    let mut denominator = 1.0f32;
    let mut term = 1.0f32;
    let mut n = 1;
    loop {
        let t = (9.0f32 * 0.5) / n as f32;
        term *= t * t;
        denominator += term;
        if (denominator as f64) * 1.0e-21 > term as f64 { break; }
        n += 1;
    }
    let inverse = 1.0f32 / denominator;
    for i in 1..SINC_TABLE {
        let p = (i as f32) / 256.0;
        let p_pi = p * PI;
        let sinc = sinf(p_pi * cutoff) / p_pi;
        let u = (1.0 / ((SINC_TABLE - 1) as f32)) * (i as f32);
        let argument = ((1.0 - u * u).sqrt() * 9.0) * 0.5;
        let mut sum = 1.0f32;
        let mut term = 1.0f32;
        let mut n = 1;
        loop {
            let t = argument / n as f32;
            term *= t * t;
            sum += term;
            if (sum as f64) * 1.0e-21 > term as f64 { break; }
            n += 1;
        }
        let coefficient = sinc * (inverse * sum);
        table[i] = coefficient;
        difference[i - 1] = coefficient - table[i - 1];
    }
    (table, difference)
}

struct Resampler {
    ratio: f32,
    step: f32,
    width: i32,
    buffered_prefix: i32,
    consumed: i32,
    pending: i32,
    position: f64,
    first: bool,
    base: usize,
    data: Vec<f32>,
    output: Vec<f32>,
    table: [f32; SINC_TABLE],
    difference: [f32; SINC_TABLE],
}

impl Resampler {
    fn new(rate: f32) -> Result<Self, PitchDetectionError> {
        let ratio = DETECTOR_RATE / rate;
        let step = 1.0 / ratio;
        if !ratio.is_finite() || ratio <= 0.0 || !step.is_finite() || step <= 0.0 {
            return Err(PitchDetectionError::InvalidSampleRate);
        }
        // The selected integer phase increment must be positive; otherwise the
        // native inner loop cannot progress. Reject arithmetic overflow as well.
        let increment = ((ratio * 256.0).arm_min(256.0) * 128.0) as f64 + 0.5;
        let required = (step.arm_max(1.0) * 7.0) + 1.0;
        if increment < 1.0 || required > (i32::MAX / 8) as f32 {
            return Err(PitchDetectionError::InvalidSampleRate);
        }
        let width = 14i32.max(required as i32);
        let output_size = ((MAX_CALLBACK as f64) * (ratio as f64)).ceil();
        if !output_size.is_finite() || output_size > (i32::MAX - 512) as f64 {
            return Err(PitchDetectionError::InvalidSampleRate);
        }
        let headroom = 7001usize.max((width as usize) * 2 + 32);
        let data_size = headroom.checked_add(MAX_CALLBACK)
            .and_then(|n| n.checked_add((width as usize) * 4 + 64))
            .ok_or(PitchDetectionError::InvalidSampleRate)?;
        let data = zeros(data_size)?;
        let mut output = Vec::new();
        output.try_reserve_exact(output_size as usize + 512)
            .map_err(|_| PitchDetectionError::AllocationFailed)?;
        let (table, difference) = sinc_table();
        Ok(Self { ratio, step, width: 14, buffered_prefix: 14, consumed: 14,
                  pending: 0, position: 14.0, first: true, base: headroom,
                  data, output, table, difference })
    }

    fn coefficient(&self, phase: i32) -> f32 {
        let index = (phase >> 7) as usize;
        let fraction = ((phase & 127) as f32) * 0.0078125;
        self.table[index] + self.difference[index] * fraction
    }

    fn process(&mut self, input: &[f32]) {
        // State names correspond to 0x10007d53c's +10,+14,+18,+28,+50.
        let wanted = ((self.step.arm_max(1.0) * 7.0) + 1.0).arm_max(self.width as f32) as i32;
        let growth = wanted - self.width;
        if growth != 0 {
            self.width = wanted;
            self.buffered_prefix += growth;
            self.consumed += growth;
            self.position += growth as f64;
            self.base -= growth as usize;
        }
        let at = (self.base as i64 + (self.buffered_prefix + self.pending) as i64) as usize;
        self.data[at..at + input.len()].copy_from_slice(input);
        self.pending += input.len() as i32;
        let mut advance = self.pending;
        if self.first {
            advance -= self.width;
            self.first = false;
        }
        self.pending += self.buffered_prefix;
        let phase_increment = (((self.ratio * 256.0).arm_min(256.0) * 128.0) as f64 + 0.5) as i32;
        let gain = if self.ratio < 1.0 { self.ratio * f32::from_bits(0x3f7d_70a4) }
                   else { f32::from_bits(0x3f7d_70a4) };
        self.output.clear();
        let target = if self.ratio >= 1.0 {
            (((advance as f32) * self.ratio) * 100.0).round_ties_even() as i64
        } else {
            (advance as i64) * 100
        };
        let mut count = 0i64;
        loop {
            let progress = if self.ratio >= 1.0 {
                count * 100
            } else {
                ((self.step * (count as f32)) * 100.0).round_ties_even() as i64
            };
            if progress >= target { break; }
            let fixed = (self.position * 32768.0) as i64;
            let index = fixed >> 15;
            let fraction = (fixed as i32) & 0x7fff;
            let mut left_phase = (fraction * phase_increment) >> 15;
            let mut left = 0.0f32;
            let mut pos = (self.base as i64 + index) as usize;
            while left_phase < (SINC_TABLE as i32) * 128 {
                let contribution = self.data[pos] * self.coefficient(left_phase);
                left += contribution;
                pos -= 1;
                left_phase += phase_increment;
            }
            let complement = (-fraction) & 0x7fff;
            let mut right_phase = ((complement * phase_increment) >> 15)
                + if complement == 0 { phase_increment } else { 0 };
            let mut right = 0.0f32;
            let mut pos = (self.base as i64 + index + 1) as usize;
            while right_phase < ((SINC_TABLE as i32) * 128) - 128 {
                let contribution = self.data[pos] * self.coefficient(right_phase);
                right += contribution;
                pos += 1;
                right_phase += phase_increment;
            }
            self.output.push((left + right) * gain);
            self.position += self.step as f64;
            count += 1;
        }
        self.pending -= 2 * self.width + advance;
        self.position -= advance as f64;
        self.consumed += advance;
        let integer = (self.position - self.width as f64) as i32;
        if integer != 0 {
            self.position -= integer as f64;
            self.consumed += integer;
        }
        let retained = advance + 3 * self.width - self.consumed;
        let start = self.base as i64 - self.width as i64;
        let end = self.base as i64 + (retained + self.pending) as i64;
        if end > start {
            let source = start + (self.consumed - self.width) as i64;
            self.data.copy_within(source as usize..(source + end - start) as usize, start as usize);
        }
        self.buffered_prefix = retained;
        self.consumed = self.width;
    }
}

struct Band {
    start: usize,
    table_offset: usize,
    length: usize,
}

fn make_bands() -> (Vec<Band>, Vec<f32>) {
    // Original 80-times oversampled integration of neighboring FFT bins.
    let mut triangle = [0.0f32; 159];
    for (i, w) in triangle.iter_mut().enumerate() {
        let x = (((i as f32 + 1.0) - 80.0) * TWO_PI) / 160.0;
        *w = cosf(x) * 0.5 + 0.5;
    }
    let mut bands = Vec::with_capacity(BINS);
    let mut coefficients = Vec::with_capacity(BINS * 16);
    let mut density = Vec::with_capacity(4096);
    let denominator = 36.0f32;
    let lower = math::exp2f(-1.0 / denominator);
    let upper = math::exp2f(1.0 / denominator);
    for bin in 0..BINS {
        let center = math::exp2f(bin as f32 / denominator) * FIRST_FREQUENCY;
        let low = (((lower * center) * 327680.0) / DETECTOR_RATE).ceil() as i32;
        let high = (((upper * center) * 327680.0) / DETECTOR_RATE).floor() as i32;
        let first = ((low as f32) / 80.0).floor() as i32;
        let last = ((high as f32) / 80.0).ceil() as i32;
        let count = (last - first + 1) as usize;
        let center_log = math::logf(center) / LN_2;
        let width = LN_2 / denominator;
        density.clear();
        for j in low..=high {
            let hz = (DETECTOR_RATE * j as f32) / 327680.0;
            let angle = ((((math::logf(hz) / LN_2) - center_log) * denominator) * TWO_PI) * 0.5;
            density.push((cosf(angle) * 0.5 + 0.5) / (width * hz));
        }
        let offset = coefficients.len();
        for k in first..=last {
            let tri_start = (k - 1) * 80 + 1;
            let begin = low.max(tri_start);
            let end = (high + 1).min((k + 1) * 80);
            let n = (end - begin).max(0) as usize;
            let value = if n == 0 { 0.0 } else {
                let a = (begin - tri_start) as usize;
                let b = (begin - low) as usize;
                dot(&triangle[a..a + n], &density[b..b + n])
            };
            coefficients.push(value);
        }
        bands.push(Band { start: first as usize, table_offset: offset, length: count });
    }
    (bands, coefficients)
}

struct Spectrum {
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    fft: *mut c_void,
    #[cfg(any(target_os = "linux", feature = "portable"))]
    fft: crate::portable::fft::FftSetup,
    frame: Vec<f32>,
    window: Vec<f32>,
    packed: PackedBuffer,
    bands: Vec<Band>,
    band_coefficients: Vec<f32>,
    smoothing: [f32; 19],
    smoothing_sum: f32,
    bin_window: [f32; BINS],
    partial_offsets: [usize; 20],
    partial_weights: [f32; 20],
    partial_suppress: [f32; 20],
    partial_count: [usize; BINS],
    mean: [f32; BINS],
    deviation: [f32; BINS],
    temp: [f32; BINS],
    residual: [f32; BINS],
    peaks: [f32; BINS],
    extracted: [f32; BINS],
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
impl Drop for Spectrum {
    fn drop(&mut self) { unsafe { vDSP_destroy_fftsetup(self.fft) }; }
}

impl Spectrum {
    fn new() -> Result<Self, PitchDetectionError> {
        let frame = zeros(FRAME)?;
        let packed = PackedBuffer::new()?;
        let mut window = zeros(FRAME)?;
        for (i, w) in window.iter_mut().enumerate() {
            let angle = ((i as f32 + 1.0) * TWO_PI) / 4097.0;
            *w = (1.0 - cosf(angle)) * 0.5;
        }
        let (bands, band_coefficients) = make_bands();
        let mut smoothing = [0.0; 19];
        let mut smoothing_sum = 0.0;
        for (i, w) in smoothing.iter_mut().enumerate() {
            let angle = (((i as i32 - 9) as f32) * TWO_PI) / 18.0;
            *w = cosf(angle) * 0.46 + 0.54;
            smoothing_sum += *w;
        }
        let mut bin_window = [0.0; BINS];
        for (i, w) in bin_window.iter_mut().enumerate() {
            let angle = ((i as f32 + 1.0) * TWO_PI) / 257.0;
            *w = (1.0 - cosf(angle)) * 0.5;
        }
        let mut partial_offsets = [0; 20];
        let mut partial_weights = [0.0; 20];
        let mut partial_suppress = [0.0; 20];
        for i in 0..20 {
            let harmonic = (i + 1) as f32;
            partial_offsets[i] = ((math::logf(harmonic) * 36.0) / LN_2 + 0.5) as usize;
            partial_weights[i] = 1.0 / ((i + 1) * (i + 1)) as f32;
            partial_suppress[i] = 1.0 - partial_weights[i];
        }
        let mut partial_count = [0; BINS];
        for (i, count) in partial_count.iter_mut().enumerate() {
            let distance = (255 - i) as i32;
            let h = math::exp2f(distance as f32 / 36.0) - 1.0;
            let upper = h.ceil() as i32;
            let roundtrip = ((math::logf((upper + 1) as f32) * 36.0) / LN_2 + 0.5) as i32;
            let chosen = if distance == roundtrip { upper } else { h.floor() as i32 };
            *count = ((chosen + 1) as usize).min(20);
        }
        // FFT_RADIX2 == 0; the selected reference calls create_fftsetup(12, 0).
        #[cfg(all(target_os = "macos", not(feature = "portable")))]
        let fft = {
            let fft = unsafe { vDSP_create_fftsetup(12, 0) };
            if fft.is_null() { return Err(PitchDetectionError::FftSetupFailed); }
            fft
        };
        #[cfg(any(target_os = "linux", feature = "portable"))]
        let fft = crate::portable::fft::FftSetup::new(12);
        Ok(Self { fft, frame, window, packed, bands, band_coefficients,
                  smoothing, smoothing_sum, bin_window, partial_offsets,
                  partial_weights, partial_suppress, partial_count,
                  mean: [0.0; BINS], deviation: [0.0; BINS], temp: [0.0; BINS],
                  residual: [0.0; BINS], peaks: [0.0; BINS], extracted: [0.0; BINS] })
    }

    fn smooth(input: &[f32; BINS], kernel: &[f32; 19], sum: f32, output: &mut [f32; BINS]) {
        for i in 0..BINS {
            let begin = i.saturating_sub(9);
            let end = (i + 10).min(BINS);
            let k = if i < 9 { 19 - (end - begin) } else { 0 };
            output[i] = dot(&input[begin..end], &kernel[k..k + end - begin]);
        }
        scale_in_place(output, 1.0 / sum);
    }

    fn analyze(&mut self, frame: &[f32; FRAME], result: &mut [f32; BINS]) {
        self.frame.copy_from_slice(frame);
        mul_in_place(&mut self.frame, &self.window);
        {
            // Both planes share one raw provenance; no slice reborrow occurs
            // until the descriptor and its pointers are out of use.
            let base = self.packed.as_mut_ptr();
            let mut split = SplitComplex { real: base, imaginary: unsafe { base.add(HALF) } };
            unsafe { vDSP_ctoz(self.frame.as_ptr(), 2, &mut split, 1, HALF); }
            #[cfg(all(target_os = "macos", not(feature = "portable")))]
            unsafe { vDSP_fft_zrip(self.fft, &mut split, 1, 12, 1); }
        }
        #[cfg(any(target_os = "linux", feature = "portable"))]
        {
            let (real, imaginary) = self.packed.split_at_mut(HALF);
            self.fft.forward(real, imaginary);
        }
        scale_in_place(&mut self.packed, 0.5);
        let nyquist = self.packed[HALF];
        self.packed[HALF] = 0.0;
        {
            // Earlier slice accesses invalidate the old raw pointers. Derive
            // the in-place destination and both input planes from a fresh base.
            let base = self.packed.as_mut_ptr();
            let split = SplitComplex { real: base, imaginary: unsafe { base.add(HALF) } };
            unsafe { vDSP_zvabs(&split, 1, base, 1, HALF) };
        }
        self.packed[HALF] = nyquist;
        scale_in_place(&mut self.packed, 2.0 / FRAME as f32);
        for (i, band) in self.bands.iter().enumerate() {
            result[i] = dot(&self.packed[band.start..band.start + band.length],
                            &self.band_coefficients[band.table_offset..band.table_offset + band.length]);
        }
        self.whiten(result);
        self.extract_harmonics(result);
        result.copy_from_slice(&self.extracted);
        add_in_place(result, &self.residual);
        mul_in_place(result, &self.bin_window);
    }

    fn whiten(&mut self, result: &mut [f32; BINS]) {
        let mut total = 0.0f32;
        for &v in result.iter() { total += v; }
        if total > 0.0 {
            Self::smooth(result, &self.smoothing, self.smoothing_sum, &mut self.mean);
            self.temp.copy_from_slice(result);
            sub_in_place(&mut self.temp, &self.mean);
            let p = self.temp.as_mut_ptr();
            unsafe { vDSP_vmul(p, 1, p, 1, p, 1, BINS) };
            Self::smooth(&self.temp, &self.smoothing, self.smoothing_sum, &mut self.deviation);
            for x in &mut self.deviation {
                if *x < f32::MIN_POSITIVE { *x = 0.0; }
                *x = x.sqrt();
                if *x < f32::MIN_POSITIVE { *x = f32::MIN_POSITIVE; }
            }
            for i in 0..9 {
                self.mean[i] = self.mean[9];
                self.deviation[i] = self.deviation[9];
                // Native 0x10007ca1c uses N - half_width, not the last full window.
                self.mean[BINS - 1 - i] = self.mean[BINS - 9];
                self.deviation[BINS - 1 - i] = self.deviation[BINS - 9];
            }
            sub_in_place(result, &self.mean);
            sub_in_place(result, &self.deviation);
            for x in result.iter_mut() { if *x < WHITEN_GATE { *x = 0.0; } }
            unsafe {
                vDSP_vdiv(self.deviation.as_ptr(), 1, result.as_ptr(), 1,
                          result.as_mut_ptr(), 1, BINS)
            };
        }
    }

    fn extract_harmonics(&mut self, result: &[f32; BINS]) {
        self.residual.copy_from_slice(result);
        self.extracted.fill(0.0);
        let mut remaining = 0.0f32;
        for &x in result.iter() { remaining += x; }
        let stop = remaining * 0.01;
        if remaining != 0.0 {
            for _ in 0..30 {
                if !(remaining > stop) { break; }
                self.peaks.fill(0.0);
                for i in 0..BINS {
                    let mut score = 0.0f32;
                    for h in 0..self.partial_count[i] {
                        score += self.residual[i + self.partial_offsets[h]] * self.partial_weights[h];
                    }
                    self.peaks[i] = score;
                }
                let mut value = f32::from_bits(0xfcf0_bdc2);
                let mut peak = 0usize;
                unsafe { vDSP_maxvi(self.peaks.as_ptr(), 1, &mut value, &mut peak, BINS) };
                let mut removed = 0.0f32;
                for h in 0..self.partial_count[peak] {
                    let index = peak + self.partial_offsets[h];
                    let old = self.residual[index];
                    removed += old * (1.0 - self.partial_suppress[h]);
                    self.residual[index] = old * self.partial_suppress[h];
                }
                self.extracted[peak] += removed;
                remaining -= removed;
            }
        } else {
            self.residual.fill(0.0);
        }
    }
}

/// A fresh detector for one analysis pass. No proprietary code is called.
pub struct PitchDetector {
    channels: usize,
    resampler: Resampler,
    spectrum: Spectrum,
    mono: Vec<f32>,
    channel: Vec<f32>,
    ring: [f32; FRAME],
    ring_head: usize,
    ring_len: usize,
    frame: [f32; FRAME],
    current: [f32; BINS],
    raw_sum: [f32; BINS],
    normalized_sum: [f32; BINS],
    frequencies: [f32; BINS],
    tuning_real: [f32; BINS],
    tuning_imaginary: [f32; BINS],
    tuning_phase: f32,
}

impl PitchDetector {
    pub fn new(sample_rate: f32, channels: usize) -> Result<Self, PitchDetectionError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(PitchDetectionError::InvalidSampleRate);
        }
        if channels == 0 || channels > i32::MAX as usize {
            return Err(PitchDetectionError::InvalidChannelCount);
        }
        let resampler = Resampler::new(sample_rate)?;
        let spectrum = Spectrum::new()?;
        let mono = zeros(MAX_CALLBACK)?;
        let channel = zeros(MAX_CALLBACK)?;
        let mut frequencies = [0.0; BINS];
        let mut tuning_real = [0.0; BINS];
        let mut tuning_imaginary = [0.0; BINS];
        let step = TWO_PI / 3.0;
        for i in 0..BINS {
            frequencies[i] = FIRST_FREQUENCY * math::exp2f(i as f32 / 36.0);
            let exponent = (step * -0.0) * i as f32;
            let angle = (-step) * i as f32;
            let magnitude = math::expf(exponent);
            let pair = unsafe { host_sincosf(angle) };
            tuning_real[i] = magnitude * pair.cosine;
            tuning_imaginary[i] = magnitude * pair.sine;
        }
        let mut phase = math::logf(440.0 / FIRST_FREQUENCY) / LN_2;
        phase *= 12.0;
        phase *= 3.0;
        phase = (-TWO_PI / 3.0) * phase;
        phase = fmodf(phase + PI, -TWO_PI);
        Ok(Self { channels, resampler, spectrum, mono, channel,
                  ring: [0.0; FRAME], ring_head: 0, ring_len: HALF,
                  frame: [0.0; FRAME], current: [0.0; BINS],
                  raw_sum: [0.0; BINS], normalized_sum: [0.0; BINS],
                  frequencies, tuning_real, tuning_imaginary, tuning_phase: phase })
    }

    /// Feed one callback. Call with the original callback partitioning.
    pub fn process_interleaved(&mut self, input: &[f32]) -> Result<(), PitchDetectionError> {
        if input.len() % self.channels != 0 { return Err(PitchDetectionError::IncompleteFrame); }
        let frames = input.len() / self.channels;
        if frames > MAX_CALLBACK { return Err(PitchDetectionError::CallbackTooLarge); }
        self.mono[..frames].fill(0.0);
        for ch in 0..self.channels {
            for i in 0..frames { self.channel[i] = input[i * self.channels + ch]; }
            add_in_place(&mut self.mono[..frames], &self.channel[..frames]);
        }
        scale_in_place(&mut self.mono[..frames], 1.0 / self.channels as f32);
        self.resampler.process(&self.mono[..frames]);
        for i in 0..self.resampler.output.len() {
            let sample = self.resampler.output[i];
            let at = (self.ring_head + self.ring_len) % FRAME;
            self.ring[at] = sample;
            self.ring_len += 1;
            if self.ring_len == FRAME {
                let first = FRAME - self.ring_head;
                self.frame[..first].copy_from_slice(&self.ring[self.ring_head..]);
                self.frame[first..].copy_from_slice(&self.ring[..self.ring_head]);
                self.ring_head = (self.ring_head + HOP) % FRAME;
                self.ring_len -= HOP;
                let mut rms = 0.0;
                unsafe { vDSP_rmsqv(self.frame.as_ptr(), 1, &mut rms, FRAME) };
                // ARM B.MI skips strictly lower RMS; unordered NaN does not skip.
                if rms < RMS_GATE { continue; }
                self.spectrum.analyze(&self.frame, &mut self.current);
                add_in_place(&mut self.raw_sum, &self.current);
                let mut sum = 0.0f32;
                for &x in &self.current { sum += x; }
                if sum != 0.0 {
                    let mut absolute_sum = 0.0f64;
                    for &x in &self.current { absolute_sum += x.abs() as f64; }
                    scale_in_place(&mut self.current, 1.0 / absolute_sum as f32);
                }
                add_in_place(&mut self.normalized_sum, &self.current);
            }
        }
        Ok(())
    }

    /// Reference tuning estimate in Hz, before the controller's `< 1 Hz` fallback.
    /// There is no separate tuning-confidence threshold in this result path.
    pub fn tuning_hz(&self) -> f32 {
        self.tuning_hz_for_aggregate(&self.raw_sum)
    }

    /// Evaluate the native tuning estimator for 256 already accumulated
    /// detector bins. Useful for comparison with a reference-stage capture.
    pub fn tuning_hz_for_aggregate(&self, aggregate: &[f32; BINS]) -> f32 {
        let real = dot(&self.tuning_real, aggregate);
        let imaginary = dot(&self.tuning_imaginary, aggregate);
        let angle = atan2f(imaginary, real);
        let wrapped = fmodf((self.tuning_phase - angle) + PI, -TWO_PI);
        math::exp2f(wrapped / f32::from_bits(0x4296_cbe4)) * 440.0
    }

    /// Apply the controller's failure rule. NaN takes the success branch on ARM.
    pub fn input_pitch_hz(&self) -> f32 {
        let frequency = self.tuning_hz();
        if frequency < 1.0 { 440.0 } else { frequency }
    }

    /// Twelve normalized pitch classes from the per-window normalized sum.
    /// These are not a tuning-confidence measure.
    pub fn pitch_classes(&self) -> [f32; 12] {
        let mut classes = [0.0f32; 12];
        for i in 0..BINS {
            let x = math::logf(self.frequencies[i] / 440.0);
            let x = ((x * 12.0) / LN_2 + 69.0) + 0.5;
            let x = fmodf(x, 12.0);
            let index = ((x - 0.5) + 0.5) as usize;
            classes[index] += self.normalized_sum[i];
        }
        let mut sum = 0.0f32;
        for &x in &classes { sum += x; }
        if sum != 0.0 {
            let mut absolute_sum = 0.0f64;
            for &x in &classes { absolute_sum += x.abs() as f64; }
            scale_in_place(&mut classes, 1.0 / absolute_sum as f32);
        }
        classes
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn detector_fft_planes_are_64_byte_aligned() {
        let packed = super::PackedBuffer::new().unwrap();
        assert_eq!(packed.as_ptr() as usize % 64, 0);
        assert_eq!(unsafe { packed.as_ptr().add(super::HALF) } as usize % 64, 0);
    }

    #[test]
    fn spectrum_analysis_reuses_both_packed_planes() {
        let mut spectrum = super::Spectrum::new().unwrap();
        let mut frame = [0.0f32; super::FRAME];
        for (i, sample) in frame.iter_mut().enumerate() {
            *sample = ((i * 37 % 251) as f32 - 125.0) / 128.0;
        }
        let mut first = [0.0; super::BINS];
        let mut second = [0.0; super::BINS];
        spectrum.analyze(&frame, &mut first);
        spectrum.packed.fill(123.0);
        spectrum.analyze(&frame, &mut second);
        assert!(first.iter().any(|value| *value != 0.0));
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn whitening_right_edge_matches_native_synthetic_output() {
        let mut spectrum = super::Spectrum::new().unwrap();
        let mut bins = [0.0f32; super::BINS];
        bins[0] = 1.0;
        bins[9] = 0.5;
        bins[246] = 0.25;
        bins[247] = 0.75;
        bins[255] = 1.0;
        spectrum.whiten(&mut bins);

        // Golden words from the pinned original whitening probe's "edges"
        // vector. Bin 246 differs from 247, so a symmetric-edge regression
        // cannot pass merely because both candidate statistics are equal.
        assert_eq!(spectrum.mean[246].to_bits(), 0x3ddd_5994);
        assert_eq!(spectrum.deviation[246].to_bits(), 0x3e71_d8df);
        for index in 247..256 {
            assert_eq!(spectrum.mean[index].to_bits(), 0x3de6_0bc5, "mean bin {index}");
            assert_eq!(spectrum.deviation[index].to_bits(), 0x3e7b_031e, "deviation bin {index}");
        }
        for (index, value) in bins.iter().enumerate() {
            let expected = match index {
                0 => 0x408f_546e,
                9 => 0x3fc8_99ca,
                247 => 0x3fcc_f9e1,
                255 => 0x4027_c282,
                _ => 0,
            };
            assert_eq!(value.to_bits(), expected, "whitened bin {index}");
        }
    }

    #[test]
    #[ignore = "requires freshly captured --sinc oracle stdout in PITCH_SINC_ORACLE_OUTPUT"]
    fn original_sinc_table_matches_all_coefficients() {
        let path = std::env::var_os("PITCH_SINC_ORACLE_OUTPUT").expect("sinc oracle output path");
        let capture = std::fs::read_to_string(path).expect("read sinc oracle output");
        let (coefficients, differences) = super::sinc_table();
        let mut count = 0;
        for line in capture.lines() {
            let mut fields = line.split_whitespace();
            let index: usize = fields.next().expect("index").parse().unwrap();
            assert_eq!(index, count);
            let coefficient = u32::from_str_radix(fields.next().expect("coefficient"), 16).unwrap();
            let difference = u32::from_str_radix(fields.next().expect("difference"), 16).unwrap();
            assert_eq!(coefficients[index].to_bits(), coefficient, "coefficient {index}");
            assert_eq!(differences[index].to_bits(), difference, "difference {index}");
            count += 1;
        }
        assert_eq!(count, super::SINC_TABLE);
    }

    #[test]
    #[ignore = "requires freshly captured --resample stdout in PITCH_RESAMPLE_ORACLE_OUTPUT"]
    fn original_resampler_matches_partitioned_input() {
        let path = std::env::var_os("PITCH_RESAMPLE_ORACLE_OUTPUT").expect("resample oracle path");
        let capture = std::fs::read_to_string(path).expect("read resample oracle output");
        let mut lines = capture.lines();
        for rate in [8000u32, 11025, 44100, 48000, 96000, 192000] {
            let mut resampler = super::Resampler::new(rate as f32).unwrap();
            let mut position = 0u32;
            for frames in [0usize, 1, 5, 16, 512, 16384, 511, 1, 0] {
                let mut fields = lines.next().expect("resample callback").split_whitespace();
                assert_eq!(fields.next().unwrap().parse::<u32>().unwrap(), rate);
                assert_eq!(fields.next().unwrap().parse::<usize>().unwrap(), frames);
                let count = fields.next().unwrap().parse::<usize>().unwrap();
                let mut input = Vec::with_capacity(frames);
                for _ in 0..frames {
                    let value = position.wrapping_mul(1664525).wrapping_add(1013904223);
                    position = position.wrapping_add(1);
                    input.push(((value >> 16) as i32 - 32768) as f32 / 32768.0);
                }
                resampler.process(&input);
                assert_eq!(resampler.output.len(), count, "rate {rate}, frames {frames}");
                for (i, actual) in resampler.output.iter().enumerate() {
                    let expected = u32::from_str_radix(fields.next().expect("sample"), 16).unwrap();
                    assert_eq!(actual.to_bits(), expected, "rate {rate}, frames {frames}, sample {i}");
                }
                assert!(fields.next().is_none());
            }
        }
        assert!(lines.next().is_none());
    }

    #[test]
    #[ignore = "requires freshly captured --harmonic stdout in PITCH_HARMONIC_ORACLE_OUTPUT"]
    fn original_harmonic_removal_matches_synthetic_bins() {
        let path = std::env::var_os("PITCH_HARMONIC_ORACLE_OUTPUT").expect("harmonic oracle path");
        let capture = std::fs::read_to_string(path).expect("read harmonic oracle output");
        let mut spectrum = super::Spectrum::new().unwrap();
        let mut lines = capture.lines();
        for name in ["zero", "dense", "sparse", "tie", "negative", "nan"] {
            let mut input = [0.0f32; super::BINS];
            match name {
                "dense" => for (i, value) in input.iter_mut().enumerate() {
                    *value = ((i * 73 + 11) % 257) as f32 / 256.0;
                },
                "sparse" => { input[10] = 1.0; input[46] = 0.5; input[67] = 0.25; input[154] = 0.125; },
                "tie" => { input[0] = 1.0; input[255] = 1.0; },
                "negative" => input.fill(-1.0),
                "nan" => input[31] = f32::NAN,
                _ => {}
            }
            let mut fields = lines.next().expect("harmonic case").split_whitespace();
            assert_eq!(fields.next().unwrap(), name);
            spectrum.extract_harmonics(&input);
            for (label, values) in [("residual", &spectrum.residual), ("extracted", &spectrum.extracted)] {
                for (i, actual) in values.iter().enumerate() {
                    let expected = u32::from_str_radix(fields.next().expect("bin"), 16).unwrap();
                    assert_eq!(actual.to_bits(), expected, "{name} {label} {i}");
                }
            }
            assert!(fields.next().is_none());
        }
        assert!(lines.next().is_none());
    }
}
