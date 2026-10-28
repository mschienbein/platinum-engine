//! Portable reproduction of Accelerate's aligned SME complex FFT kernels.
//!
//! The two callers use a real FFT of length 4096 or 8192; the corresponding
//! split-complex transforms here have length 2048 or 4096.  An inverse uses
//! this same forward transform with the two planes swapped by the caller.
//!
//! Each transform is a 16-point DIF, a twiddle, an 8- or 16-point matrix,
//! and a second 16-point DIF.  The matrix accumulation orders and the rounded
//! binary32 coefficients matter: replacing them with a conventional FFT or
//! computing trigonometric constants at runtime changes output bits.

use super::arm::FpMode;
#[cfg(not(target_arch = "aarch64"))]
use super::arm;

// Captured binary32 coefficients are little-endian. The blob contains, in
// order: the shared 16-point matrices; the 2048-point middle matrices; the
// 4096-point middle matrices; and the 4096-point interstage twiddles. The
// 2048-point twiddles are the even-angle subset of the last table.
const DATA: &[u8; 82688] = include_bytes!("fft_sme_coefficients.bin");

const fn decode<const N: usize>(offset: usize) -> [f32; N] {
    let mut values = [0.0; N];
    let mut i = 0;
    while i < N {
        let p = 4 * (offset + i);
        values[i] = f32::from_bits(u32::from_le_bytes([
            DATA[p], DATA[p + 1], DATA[p + 2], DATA[p + 3],
        ]));
        i += 1;
    }
    values
}

static FIRST: [f32; 192] = decode(0);
static MIDDLE_2048: [f32; 4096] = decode(192);
static MIDDLE_4096: [f32; 8192] = decode(4288);
static TWIDDLE: [f32; 8192] = decode(12480);

// The native matrix visits paired/quartet blocks in this order and writes
// them in bit-reversed order for the final DIF.
const SOURCE_BLOCK: [usize; 16] = [0, 15, 1, 8, 2, 4, 3, 12, 5, 10, 6, 9, 7, 14, 11, 13];
const TARGET_BLOCK: [usize; 16] = [0, 15, 8, 1, 4, 2, 12, 3, 10, 5, 6, 9, 14, 7, 13, 11];

// SME ZA-targeting additions and FMAs always use default-NaN behavior, even
// when FPCR.DN is clear. The caller's FZ mode still applies. On ARM the caller
// supplies the mode corresponding to the ambient FPCR; on x86 the ARM helpers
// reproduce FZ's input and before-rounding output rules explicitly.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn canonical(value: f32) -> f32 {
    if value.to_bits() & 0x7fff_ffff > 0x7f80_0000 {
        f32::from_bits(0x7fc0_0000)
    } else {
        value
    }
}

#[inline(always)]
fn add(a: f32, b: f32, mode: FpMode) -> f32 {
    #[cfg(target_arch = "aarch64")]
    { let _ = mode; canonical(a + b) }
    #[cfg(not(target_arch = "aarch64"))]
    { arm::fadd_f32_with_mode(a, b, mode) }
}

#[inline(always)]
fn sub(a: f32, b: f32, mode: FpMode) -> f32 {
    #[cfg(target_arch = "aarch64")]
    { let _ = mode; canonical(a - b) }
    #[cfg(not(target_arch = "aarch64"))]
    { arm::fsub_f32_with_mode(a, b, mode) }
}

#[inline(always)]
fn fma(a: f32, b: f32, c: f32, mode: FpMode) -> f32 {
    #[cfg(target_arch = "aarch64")]
    { let _ = mode; canonical(a.mul_add(b, c)) }
    #[cfg(not(target_arch = "aarch64"))]
    { arm::fmadd_f32_with_mode(a, b, c, mode) }
}

type Complex = (f32, f32);

#[inline(always)]
fn radix4(v: [Complex; 4], mode: FpMode) -> [Complex; 4] {
    let [(a, w), (b, x), (c, y), (d, z)] = v;
    [
        (add(add(add(a, b, mode), c, mode), d, mode), add(add(add(w, x, mode), y, mode), z, mode)),
        (sub(sub(add(a, x, mode), c, mode), z, mode), add(sub(sub(w, b, mode), y, mode), d, mode)),
        (sub(add(sub(a, b, mode), c, mode), d, mode), sub(add(sub(w, x, mode), y, mode), z, mode)),
        (add(sub(sub(a, x, mode), c, mode), z, mode), sub(sub(add(w, b, mode), y, mode), d, mode)),
    ]
}

#[inline]
fn fft16(input: &[Complex; 16], mode: FpMode) -> [Complex; 16] {
    let first = radix4([input[0], input[4], input[8], input[12]], mode);
    let mut output = [(0.0, 0.0); 16];
    for k in 0..4 {
        let mut values = [(0.0, 0.0); 4];
        values[0] = first[k];
        for j in 1..4 {
            let (mut real, mut imag) = (0.0, 0.0);
            for t in 0..4 {
                let p = (j - 1) * 64 + t * 16;
                let (cos, sin) = (FIRST[p + k], FIRST[p + 8 + k]);
                let (r, i) = input[j + 4 * t];
                real = fma(cos, r, real, mode);
                real = fma(sin, i, real, mode);
                imag = fma(-sin, r, imag, mode);
                imag = fma(cos, i, imag, mode);
            }
            values[j] = (real, imag);
        }
        let result = radix4(values, mode);
        for q in 0..4 { output[k + q * 4] = result[q]; }
    }
    output
}

/// Forward unnormalized split-complex transform for real-FFT log2 sizes 12/13.
/// `mode` must describe the caller's ambient floating-point mode. The slices
/// must have exactly `1 << (log2n - 1)` elements.
pub(super) fn complex(log2n: usize, real: &mut [f32], imag: &mut [f32], mode: FpMode) {
    let mode = FpMode { default_nan: true, ..mode };
    match log2n {
        12 => run::<2048>(real, imag, mode),
        13 => run::<4096>(real, imag, mode),
        _ => panic!("unsupported SME FFT size"),
    }
}

fn initial(real: &[f32], imag: &[f32], x: &mut [f32], y: &mut [f32], mode: FpMode) {
    let stride = real.len() / 16;
    for p in 0..stride {
        let input = std::array::from_fn(|t| (real[p + t * stride], imag[p + t * stride]));
        let output = fft16(&input, mode);
        for k in 0..16 {
            let dst = (p % 16) * stride + (p / 16) * 16 + k;
            (x[dst], y[dst]) = output[k];
        }
    }
}

fn twiddle(x: &mut [f32], y: &mut [f32], mode: FpMode) {
    let size = x.len();
    let stride = size / 16;
    for j in 0..size {
        let n = (j / stride) + (j % stride / 16) * 16;
        let k = j % 16;
        let n4096 = if size == 2048 { 2 * n } else { n };
        let index = (n4096 % 16) * 256 + (n4096 / 16) * 16 + k;
        let offset = (index / 64) * 128 + index % 64;
        let (cos, sin) = (TWIDDLE[offset], TWIDDLE[offset + 64]);
        let (r, i) = (x[j], y[j]);
        x[j] = fma(-sin, i, fma(cos, r, 0.0, mode), mode);
        y[j] = fma(sin, r, fma(cos, i, 0.0, mode), mode);
    }
}

fn middle(x: &[f32], y: &[f32], real: &mut [f32], imag: &mut [f32], mode: FpMode) {
    let size = x.len();
    let stride = size / 16;
    for block in 0..16 {
        let source = SOURCE_BLOCK[block];
        let target = TARGET_BLOCK[block];
        for k1 in 0..size / 256 {
            for k0 in 0..16 {
                let (mut a, mut b) = (0.0, 0.0);
                if size == 2048 {
                    // The 8-point matrix accumulates two real products before
                    // the corresponding pair of imaginary products.
                    for pair in 0..4 {
                        let t0 = (2 * pair + 2 * (block % 4)) % 8;
                        let t1 = (t0 + 1) % 8;
                        let p0 = source * stride + t0 * 16 + k0;
                        let p1 = source * stride + t1 * 16 + k0;
                        let c0 = block * 256 + t0 * 16 + k1;
                        let c1 = block * 256 + t1 * 16 + k1;
                        a = fma(MIDDLE_2048[c0], x[p0], a, mode);
                        a = fma(MIDDLE_2048[c1], x[p1], a, mode);
                        b = fma(MIDDLE_2048[c0 + 8], x[p0], b, mode);
                        b = fma(MIDDLE_2048[c1 + 8], x[p1], b, mode);
                        a = fma(MIDDLE_2048[c0 + 128], y[p0], a, mode);
                        a = fma(MIDDLE_2048[c1 + 128], y[p1], a, mode);
                        b = fma(MIDDLE_2048[c0 + 136], y[p0], b, mode);
                        b = fma(MIDDLE_2048[c1 + 136], y[p1], b, mode);
                    }
                } else {
                    // The second matrix of each pair starts at row two.
                    for z in 0..16 {
                        let t = (z + 2 * (block % 2)) % 16;
                        let p = source * stride + t * 16 + k0;
                        let c = block * 512 + t * 32 + k1;
                        let (cos, sin) = (MIDDLE_4096[c], MIDDLE_4096[c + 16]);
                        a = fma(cos, x[p], a, mode);
                        a = fma(-sin, y[p], a, mode);
                        b = fma(cos, y[p], b, mode);
                        b = fma(sin, x[p], b, mode);
                    }
                }
                let p = target * stride + k1 * 16 + k0;
                real[p] = a;
                imag[p] = b;
            }
        }
    }
}

fn final_stage(real: &mut [f32], imag: &mut [f32], mode: FpMode) {
    let stride = real.len() / 16;
    for p in 0..stride {
        let input = std::array::from_fn(|t: usize| {
            let reversed = t.reverse_bits() >> (usize::BITS - 4);
            (real[p + reversed * stride], imag[p + reversed * stride])
        });
        let output = fft16(&input, mode);
        for k in 0..16 {
            let dst = k * stride + p;
            (real[dst], imag[dst]) = output[k];
        }
    }
}

fn run<const N: usize>(real: &mut [f32], imag: &mut [f32], mode: FpMode) {
    assert_eq!(real.len(), N);
    assert_eq!(imag.len(), N);
    let (mut x, mut y) = ([0.0; N], [0.0; N]);
    initial(real, imag, &mut x, &mut y, mode);
    twiddle(&mut x, &mut y, mode);
    middle(&x, &y, real, imag, mode);
    final_stage(real, imag, mode);
}
