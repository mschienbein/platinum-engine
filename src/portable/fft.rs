//! Portable, aligned-0 equivalent of the real `vDSP_fft_zrip` profile used by
//! the pitch pipeline on the reference macOS arm64 machine.
//!
//! Input and output each consist of N/2 real and imaginary words. Forward
//! produces twice the ordinary real DFT, with DC in `real[0]` and Nyquist in
//! `imaginary[0]`; inverse is unnormalized. This code does not change the
//! caller's floating-point mode. The reference dispatch and arithmetic depend
//! on scratch alignment and on the Mac's SME-capable Accelerate backend; this
//! implements the aligned-0 profile, not every Mac's possible dispatch path.

use crate::portable::{arm, fft_sme, math};
use std::sync::Mutex;

#[derive(Clone, Copy)]
struct Twiddle {
    cosine: f32,
    sine: f32,
}

// The fixed 16-point matrix has exact cardinal zeroes, unlike the separately
// generated setup twiddles. These are the binary32 rounded 16th roots.
const COS_16: [u32; 16] = [
    0x3f80_0000, 0x3f6c_835e, 0x3f35_04f3, 0x3ec3_ef15,
    0x0000_0000, 0xbec3_ef15, 0xbf35_04f3, 0xbf6c_835e,
    0xbf80_0000, 0xbf6c_835e, 0xbf35_04f3, 0xbec3_ef15,
    0x0000_0000, 0x3ec3_ef15, 0x3f35_04f3, 0x3f6c_835e,
];
const SIN_16: [u32; 16] = [
    0x8000_0000, 0xbec3_ef15, 0xbf35_04f3, 0xbf6c_835e,
    0xbf80_0000, 0xbf6c_835e, 0xbf35_04f3, 0xbec3_ef15,
    0x8000_0000, 0x3ec3_ef15, 0x3f35_04f3, 0x3f6c_835e,
    0x3f80_0000, 0x3f6c_835e, 0x3f35_04f3, 0x3ec3_ef15,
];

#[inline]
fn fixed_root(t: usize, k: usize) -> Twiddle {
    let index = (t * k) & 15;
    Twiddle { cosine: f32::from_bits(COS_16[index]), sine: f32::from_bits(SIN_16[index]) }
}

// This polynomial is only used for the bounded setup angles, not as a general
// trigonometric implementation. Split-pi reduction preserves the tiny values
// at cardinal angles, which are present in the reference setup tables.
fn setup_reduction(angle: f64) -> (f64, i32) {
    let quadrant = (angle * 0.63661977236758134308).round() as i32;
    let remainder = (-(quadrant as f64)).mul_add(
        6.12323399573676603587e-17,
        (-(quadrant as f64)).mul_add(1.57079632679489655800, angle),
    );
    (remainder, quadrant)
}

fn setup_sine_polynomial(remainder: f64) -> f64 {
    let z = remainder * remainder;
    (((((((((-1.0 / 121645100408832000.0 * z + 1.0 / 355687428096000.0) * z
        - 1.0 / 1307674368000.0) * z + 1.0 / 6227020800.0) * z - 1.0 / 39916800.0) * z
        + 1.0 / 362880.0) * z - 1.0 / 5040.0) * z + 1.0 / 120.0) * z - 1.0 / 6.0) * z + 1.0) * remainder
}

fn setup_cosine_polynomial(remainder: f64) -> f64 {
    let z = remainder * remainder;
    (((((((((1.0 / 2432902008176640000.0 * z - 1.0 / 6402373705728000.0) * z
        + 1.0 / 20922789888000.0) * z - 1.0 / 87178291200.0) * z + 1.0 / 479001600.0) * z
        - 1.0 / 3628800.0) * z + 1.0 / 40320.0) * z - 1.0 / 720.0) * z + 1.0 / 24.0) * z - 1.0 / 2.0) * z + 1.0
}

fn setup_sin_cos(angle: f64) -> (f64, f64) {
    let (remainder, quadrant) = setup_reduction(angle);
    let sine = setup_sine_polynomial(remainder);
    let cosine = setup_cosine_polynomial(remainder);
    match quadrant.rem_euclid(4) {
        0 => (sine, cosine),
        1 => (cosine, -sine),
        2 => (-sine, -cosine),
        _ => (-cosine, sine),
    }
}

fn setup_sine(angle: f64) -> f64 {
    let (remainder, quadrant) = setup_reduction(angle);
    match quadrant.rem_euclid(4) {
        0 => setup_sine_polynomial(remainder),
        1 => setup_cosine_polynomial(remainder),
        2 => -setup_sine_polynomial(remainder),
        _ => -setup_cosine_polynomial(remainder),
    }
}

#[inline]
fn reversed_fraction(index: usize) -> f64 {
    (index as u64).reverse_bits() as f64 * (1.0 / 18446744073709551616.0)
}

// The scope in fp_mode.rs is owned by the caller. Detect it once per transform
// rather than modifying FPCR/MXCSR, including the ARM default-NaN setting.
#[inline]
fn active_mode() -> arm::FpMode {
    let min_normal = std::hint::black_box(f32::MIN_POSITIVE);
    let half = std::hint::black_box(0.5f32);
    let payload_nan = std::hint::black_box(f32::from_bits(0x7fc0_1234));
    let zero = std::hint::black_box(0.0f32);
    arm::FpMode {
        flush_to_zero: (min_normal * half).to_bits() & 0x7fff_ffff == 0,
        default_nan: (payload_nan + zero).to_bits() == 0x7fc0_0000,
    }
}
// SME ZA arithmetic forces default-NaN even when the surrounding FPCR does
// not. On ARM the hardware already implements FZ; canonicalize only the NaN
// result when the modeled instruction uses that forced mode.
#[cfg(target_arch = "aarch64")]
#[inline]
fn canonicalize_nan(value: f32, mode: arm::FpMode) -> f32 {
    if mode.default_nan && value.to_bits() & 0x7fff_ffff > 0x7f80_0000 {
        f32::from_bits(0x7fc0_0000)
    } else {
        value
    }
}


#[inline]
fn add(a: f32, b: f32, mode: arm::FpMode) -> f32 {
    #[cfg(target_arch = "aarch64")]
    { canonicalize_nan(a + b, mode) }
    #[cfg(not(target_arch = "aarch64"))]
    { arm::fadd_f32_with_mode(a, b, mode) }
}
#[inline]
fn sub(a: f32, b: f32, mode: arm::FpMode) -> f32 {
    #[cfg(target_arch = "aarch64")]
    { canonicalize_nan(a - b, mode) }
    #[cfg(not(target_arch = "aarch64"))]
    { arm::fsub_f32_with_mode(a, b, mode) }
}
#[inline]
fn mul(a: f32, b: f32, mode: arm::FpMode) -> f32 {
    #[cfg(target_arch = "aarch64")]
    { canonicalize_nan(a * b, mode) }
    #[cfg(not(target_arch = "aarch64"))]
    { arm::fmul_f32_with_mode(a, b, mode) }
}
#[inline]
fn fma(a: f32, b: f32, c: f32, mode: arm::FpMode) -> f32 {
    #[cfg(target_arch = "aarch64")]
    { canonicalize_nan(a.mul_add(b, c), mode) }
    #[cfg(not(target_arch = "aarch64"))]
    { arm::fmadd_f32_with_mode(a, b, c, mode) }
}

struct Workspace {
    real: Vec<f32>,
    imaginary: Vec<f32>,
}

enum Plan {
    Small { interstage: Vec<Twiddle> },
    General { matrix: Vec<Twiddle>, interstage: Vec<Twiddle>, workspace: Mutex<Workspace> },
    Specialized,
}

/// Reusable setup for aligned unit-stride real transforms of 2^9 through 2^15.
/// Its workspace is synchronized so the same setup may serve independent calls.
pub struct FftSetup {
    log2n: usize,
    real_twiddles: Vec<Twiddle>,
    plan: Plan,
}

impl FftSetup {
    /// Create a radix-2 real-transform setup for one of the engine's sizes.
    pub fn new(log2n: usize) -> Self {
        assert!((9..=15).contains(&log2n), "unsupported real FFT size");
        let n = 1usize << log2n;
        let complex_len = n / 2;
        let step = f32::from_bits(0x40c9_0fdb) / n as f32;
        let real_twiddles = (0..n / 4).map(|j| {
            // The system computes a binary32 angle, then calls cosf(angle)
            // and the binary64 sin(angle), narrowing the latter to binary32.
            let angle = step * (j + 1) as f32;
            Twiddle { cosine: math::cosf(angle), sine: setup_sine(angle as f64) as f32 }
        }).collect();
        let plan = match log2n {
            9 => {
                let interstage = (0..256).map(|j| {
                    let angle = (std::f64::consts::TAU * ((j / 16 * (j % 16)) as f64)) / 256.0;
                    let (sine, cosine) = setup_sin_cos(angle);
                    Twiddle { cosine: cosine as f32, sine: -(sine as f32) }
                }).collect();
                Plan::Small { interstage }
            }
            12 | 13 => Plan::Specialized,
            _ => {
                let mut matrix = Vec::with_capacity((complex_len / 256) * 240);
                for block in 0..complex_len / 256 {
                    let angles: [f64; 16] = std::array::from_fn(|k| {
                        reversed_fraction(block * 16 + k) * std::f64::consts::TAU
                    });
                    for t in 1..16 {
                        for &angle in &angles {
                            let (sine, cosine) = setup_sin_cos(angle * t as f64);
                            matrix.push(Twiddle { cosine: cosine as f32, sine: -(sine as f32) });
                        }
                    }
                }
                let mut interstage = Vec::with_capacity(complex_len);
                for group in (0..complex_len).step_by(16) {
                    let fraction = reversed_fraction(group);
                    for lane in 0..16 {
                        let angle = (fraction * lane as f64) * std::f64::consts::TAU;
                        let (sine, cosine) = setup_sin_cos(angle);
                        interstage.push(Twiddle { cosine: cosine as f32, sine: -(sine as f32) });
                    }
                }
                let workspace = Mutex::new(Workspace {
                    real: vec![0.0; complex_len], imaginary: vec![0.0; complex_len],
                });
                Plan::General { matrix, interstage, workspace }
            }
        };
        Self { log2n, real_twiddles, plan }
    }

    /// Transform packed even/odd real input to the vDSP packed spectrum.
    pub fn forward(&self, real: &mut [f32], imaginary: &mut [f32]) {
        self.check_lengths(real, imaginary);
        let mode = active_mode();
        self.complex(real, imaginary, mode);
        self.real_stage(real, imaginary, false, mode);
    }

    /// Transform a vDSP packed spectrum to unnormalized even/odd real output.
    pub fn inverse(&self, real: &mut [f32], imaginary: &mut [f32]) {
        self.check_lengths(real, imaginary);
        let mode = active_mode();
        self.real_stage(real, imaginary, true, mode);
        self.complex(imaginary, real, mode);
    }

    fn check_lengths(&self, real: &[f32], imaginary: &[f32]) {
        let expected = 1usize << (self.log2n - 1);
        assert_eq!(real.len(), expected, "real FFT plane length");
        assert_eq!(imaginary.len(), expected, "imaginary FFT plane length");
    }

    fn complex(&self, real: &mut [f32], imaginary: &mut [f32], mode: arm::FpMode) {
        let mode = arm::FpMode { default_nan: true, ..mode };
        match &self.plan {
            Plan::Small { interstage } => Self::small_complex(real, imaginary, interstage, mode),
            Plan::General { matrix, interstage, workspace } => {
                let mut workspace = workspace.lock().expect("FFT workspace poisoned");
                let Workspace { real: temp_real, imaginary: temp_imaginary } = &mut *workspace;
                self.general_complex(real, imaginary, matrix, interstage, temp_real, temp_imaginary, mode);
            }
            Plan::Specialized => fft_sme::complex(self.log2n, real, imaginary, mode),
        }
    }

    fn small_complex(real: &mut [f32], imaginary: &mut [f32], interstage: &[Twiddle], mode: arm::FpMode) {
        let (mut temp_real, mut temp_imaginary) = ([[0.0f32; 16]; 16], [[0.0f32; 16]; 16]);
        for p in 0..16 {
            let seed_real = fma(real[p], 1.0, 0.0, mode);
            let seed_imaginary = fma(imaginary[p], 1.0, 0.0, mode);
            for k in 0..16 {
                let mut ar = seed_real;
                let mut ai = seed_imaginary;
                let (mut br, mut bi) = (0.0, 0.0);
                for t in 1..8 {
                    let Twiddle { cosine, sine } = fixed_root(t, k);
                    let (a, b) = (real[p + 16 * t], imaginary[p + 16 * t]);
                    let (a2, b2) = (real[p + 16 * (16 - t)], imaginary[p + 16 * (16 - t)]);
                    br = fma(a, cosine, br, mode); bi = fma(a, sine, bi, mode);
                    br = fma(-b, sine, br, mode); bi = fma(b, cosine, bi, mode);
                    ar = fma(a2, cosine, ar, mode); ai = fma(-a2, sine, ai, mode);
                    ar = fma(b2, sine, ar, mode); ai = fma(b2, cosine, ai, mode);
                }
                let sign = if k & 1 == 0 { 1.0 } else { -1.0 };
                ar = fma(real[p + 128], sign, ar, mode);
                ai = fma(imaginary[p + 128], sign, ai, mode);
                let (a, b) = (add(ar, br, mode), add(ai, bi, mode));
                let Twiddle { cosine, sine } = interstage[k * 16 + p];
                temp_real[p][k] = fma(-b, sine, fma(a, cosine, 0.0, mode), mode);
                temp_imaginary[p][k] = fma(a, sine, fma(b, cosine, 0.0, mode), mode);
            }
        }
        const ORDER: [usize; 14] = [4, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15];
        for k1 in 0..16 {
            let seed_real = fma(temp_real[0][k1], 1.0, 0.0, mode);
            let seed_imaginary = fma(temp_imaginary[0][k1], 1.0, 0.0, mode);
            for k2 in 0..16 {
                let mut ar = seed_real;
                let mut ai = seed_imaginary;
                let sign = if k2 & 1 == 0 { 1.0 } else { -1.0 };
                ar = fma(sign, temp_real[8][k1], ar, mode);
                ai = fma(sign, temp_imaginary[8][k1], ai, mode);
                for p in ORDER {
                    let (a, b) = (temp_real[p][k1], temp_imaginary[p][k1]);
                    let t = if p < 8 { p } else { 16 - p };
                    let Twiddle { cosine, sine } = fixed_root(t, k2);
                    ar = fma(cosine, a, ar, mode); ai = fma(cosine, b, ai, mode);
                    if p < 8 { ar = fma(-sine, b, ar, mode); ai = fma(sine, a, ai, mode); }
                    else { ar = fma(sine, b, ar, mode); ai = fma(-sine, a, ai, mode); }
                }
                real[k2 * 16 + k1] = ar;
                imaginary[k2 * 16 + k1] = ai;
            }
        }
    }

    fn general_complex(&self, real: &mut [f32], imaginary: &mut [f32], matrix: &[Twiddle],
                       interstage: &[Twiddle], temp_real: &mut [f32], temp_imaginary: &mut [f32], mode: arm::FpMode) {
        let n = real.len();
        let bits = self.log2n - 1;
        let radix = if bits % 4 == 1 { 2 } else { 4 };
        let stride = n / radix;
        for p in 0..stride {
            if radix == 2 {
                let (a, b) = (real[p], real[p + stride]);
                real[p] = add(a, b, mode); real[p + stride] = sub(a, b, mode);
                let (a, b) = (imaginary[p], imaginary[p + stride]);
                imaginary[p] = add(a, b, mode); imaginary[p + stride] = sub(a, b, mode);
            } else {
                let (a, b, c, d) = (real[p], real[p + stride], real[p + 2 * stride], real[p + 3 * stride]);
                let (w, x, y, z) = (imaginary[p], imaginary[p + stride], imaginary[p + 2 * stride], imaginary[p + 3 * stride]);
                real[p] = add(add(add(a, b, mode), c, mode), d, mode);
                imaginary[p] = add(add(add(w, x, mode), y, mode), z, mode);
                real[p + stride] = sub(add(sub(a, b, mode), c, mode), d, mode);
                imaginary[p + stride] = sub(add(sub(w, x, mode), y, mode), z, mode);
                real[p + 2 * stride] = sub(sub(add(a, x, mode), c, mode), z, mode);
                imaginary[p + 2 * stride] = add(sub(sub(w, b, mode), y, mode), d, mode);
                real[p + 3 * stride] = add(sub(sub(a, x, mode), c, mode), z, mode);
                imaginary[p + 3 * stride] = sub(sub(add(w, b, mode), y, mode), d, mode);
            }
        }
        // Each radix-16 matrix uses one accumulator per frequency. Columns
        // are independent, so writing each completed column is safe in place.
        let mut block = n / radix;
        while block >= 256 {
            let stride = block / 16;
            for b in 0..n / block {
                for p in 0..stride {
                    let base = b * block + p;
                    let (a, z) = (real[base], imaginary[base]);
                    let mut ar = [fma(1.0, a, 0.0, mode); 16];
                    let mut ai = [fma(1.0, z, 0.0, mode); 16];
                    for t in 1..16 {
                        let (a, z) = (real[base + t * stride], imaginary[base + t * stride]);
                        for k in 0..16 {
                            let Twiddle { cosine, sine } = matrix[b * 240 + (t - 1) * 16 + k];
                            ar[k] = fma(cosine, a, ar[k], mode); ai[k] = fma(cosine, z, ai[k], mode);
                            ar[k] = fma(-sine, z, ar[k], mode); ai[k] = fma(sine, a, ai[k], mode);
                        }
                    }
                    for k in 0..16 { real[base + k * stride] = ar[k]; imaginary[base + k * stride] = ai[k]; }
                }
            }
            block /= 16;
        }
        let columns = n / 16;
        for j in 0usize..n {
            let Twiddle { cosine, sine } = interstage[j];
            let (a, b) = (real[j], imaginary[j]);
            let destination = (j % 16) * columns + ((j % columns) / 16) * 16
                + (j / columns).reverse_bits().wrapping_shr(usize::BITS - 4);
            temp_real[destination] = fma(-sine, b, fma(cosine, a, 0.0, mode), mode);
            temp_imaginary[destination] = fma(cosine, b, fma(sine, a, 0.0, mode), mode);
        }
        let group_bits = bits - 8;
        for k1 in 0usize..columns {
            let seed_real = fma(1.0, temp_real[k1], 0.0, mode);
            let seed_imaginary = fma(1.0, temp_imaginary[k1], 0.0, mode);
            let group = (k1 / 16).reverse_bits().wrapping_shr(usize::BITS - group_bits as u32);
            for k2 in 0..16 {
                let sign = if k2 & 1 == 0 { 1.0 } else { -1.0 };
                let mut ar = seed_real;
                let mut ai = seed_imaginary;
                ar = fma(sign, temp_real[8 * columns + k1], ar, mode);
                ai = fma(sign, temp_imaginary[8 * columns + k1], ai, mode);
                for t in 1..8 {
                    let Twiddle { cosine, sine } = fixed_root(t, k2);
                    let (a, b) = (temp_real[t * columns + k1], temp_imaginary[t * columns + k1]);
                    ar = fma(cosine, a, ar, mode); ai = fma(cosine, b, ai, mode);
                    ar = fma(-sine, b, ar, mode); ai = fma(sine, a, ai, mode);
                    let (a, b) = (temp_real[(16 - t) * columns + k1], temp_imaginary[(16 - t) * columns + k1]);
                    ar = fma(cosine, a, ar, mode); ai = fma(cosine, b, ai, mode);
                    ar = fma(sine, b, ar, mode); ai = fma(-sine, a, ai, mode);
                }
                let destination = k2 * columns + group * 16 + k1 % 16;
                real[destination] = ar;
                imaginary[destination] = ai;
            }
        }
    }

    fn real_stage(&self, real: &mut [f32], imaginary: &mut [f32], inverse: bool, mode: arm::FpMode) {
        let n = real.len();
        let quarter = n / 2;
        let (a, b) = if inverse { (real[0], imaginary[0]) }
            else { (add(real[0], real[0], mode), add(imaginary[0], imaginary[0], mode)) };
        real[0] = add(a, b, mode);
        imaginary[0] = sub(a, b, mode);
        let specialized = matches!(self.plan, Plan::Specialized);
        let mode = if specialized { arm::FpMode { default_nan: true, ..mode } } else { mode };
        for k in 1..=quarter {
            let (a, b, c, d) = (real[k], imaginary[k], real[n - k], imaginary[n - k]);
            let Twiddle { cosine, sine } = self.real_twiddles[k - 1];
            let x = add(a, c, mode);
            let y = sub(b, d, mode);
            let (low_r, low_i, high_r, high_i) = if specialized {
                if inverse {
                    let low_r = fma(sine, c, fma(-sine, a, fma(-cosine, d, fma(-cosine, b, x, mode), mode), mode), mode);
                    let high_r = fma(-sine, c, fma(sine, a, fma(cosine, d, fma(cosine, b, x, mode), mode), mode), mode);
                    let low_i = fma(-sine, d, fma(-sine, b, fma(-cosine, c, fma(cosine, a, y, mode), mode), mode), mode);
                    let high_i = fma(-sine, d, fma(-sine, b, fma(-cosine, c, fma(cosine, a, sub(d, b, mode), mode), mode), mode), mode);
                    (low_r, low_i, high_r, high_i)
                } else {
                    let low_r = fma(-sine, a, fma(sine, c, fma(cosine, d, fma(cosine, b, x, mode), mode), mode), mode);
                    let high_r = fma(sine, a, fma(-sine, c, fma(-cosine, d, fma(-cosine, b, x, mode), mode), mode), mode);
                    let low_i = fma(-sine, d, fma(-sine, b, fma(-cosine, a, fma(cosine, c, y, mode), mode), mode), mode);
                    let high_i = fma(-sine, d, fma(-sine, b, fma(-cosine, a, fma(cosine, c, sub(d, b, mode), mode), mode), mode), mode);
                    (low_r, low_i, high_r, high_i)
                }
            } else {
                let z = add(b, d, mode);
                let t = if inverse { sub(a, c, mode) } else { sub(c, a, mode) };
                let u = add(mul(cosine, z, mode), mul(sine, t, mode), mode);
                let v = sub(mul(cosine, t, mode), mul(sine, z, mode), mode);
                let low_r = if inverse { sub(x, u, mode) } else { add(x, u, mode) };
                let high_r = if inverse { add(x, u, mode) } else { sub(x, u, mode) };
                (low_r, add(v, y, mode), high_r, sub(v, y, mode))
            };
            if k < quarter { real[k] = low_r; imaginary[k] = low_i; }
            real[n - k] = high_r;
            imaginary[n - k] = high_i;
        }
    }
}
