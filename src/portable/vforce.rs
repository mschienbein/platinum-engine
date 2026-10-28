//! Portable equivalents of the Accelerate vForce vector math the pitch path calls.

use super::math::TRIG_REDUCTION;

// Reciprocal centers and base-two logarithms for the nine intervals used by
// the vector logarithm. Storing their binary32 encodings avoids host parsing
// and excess precision changing the approximation.
const LOG_RECIP: [u32; 9] = [
    0x3f80_0000, 0x3f64_2f47, 0x3f4d_34fa, 0x3f3a_8deb, 0x3f2a_d23b,
    0x3f1d_7339, 0x3f12_c6ac, 0x3f08_27e9, 0x3f00_0000,
];
const LOG_CENTER: [u32; 9] = [
    0, 0x3e29_ecac, 0x3ea3_5c65, 0x3ee9_c09e, 0xbed5_2af4,
    0xbe98_f5dc, 0xbe4a_3655, 0xbdb6_8248, 0,
];

#[inline]
fn f(bits: u32) -> f32 {
    f32::from_bits(bits)
}

// ARM flushes tiny narrowing results before rounding; x86 can instead round
// such a value up to MIN_POSITIVE with FTZ enabled.
#[inline]
fn narrow_f32(value: f64) -> f32 {
    let result = value as f32;
    #[cfg(target_arch = "x86_64")]
    if result.to_bits() & 0x7fff_ffff == 0x0080_0000
        && value.to_bits() & 0x7fff_ffff_ffff_ffff < 0x3810_0000_0000_0000
        && (std::hint::black_box(f32::MIN_POSITIVE) * std::hint::black_box(0.5_f32))
            .to_bits() & 0x7fff_ffff == 0
    {
        return f(result.to_bits() & 0x8000_0000);
    }
    result
}

// The logarithm handles denormals by integer normalization; its floating-point
// zero comparison intentionally observes the caller's flush mode.
#[inline]
fn log_element(x: f32) -> f32 {
    let bits = x.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    if magnitude == 0 || (magnitude < 0x0080_0000 && x == 0.0) {
        return f(0xff80_0000);
    }
    if magnitude > 0x7f80_0000 || bits >> 31 != 0 {
        return f(0x7fc0_0000);
    }
    if magnitude == 0x7f80_0000 {
        return f(0x7f80_0000);
    }

    let (mantissa, exponent) = if magnitude < 0x0080_0000 {
        let leading = magnitude.leading_zeros();
        let mantissa = (magnitude << (leading - 8)) & 0x007f_ffff;
        let exponent = ((mantissa.wrapping_add(0xc0c8_0000) as i32) >> 23)
            - leading as i32 + 9;
        (mantissa, exponent)
    } else {
        (bits & 0x007f_ffff, (bits.wrapping_add(0xc0c8_0000) as i32) >> 23)
    };
    let index = ((mantissa + 0x0008_0000) >> 20) as usize;
    let normalized = f(0x3f80_0000 | mantissa);
    let r = normalized.mul_add(f(LOG_RECIP[index]), -1.0);
    let square = r * r;
    let polynomial = square.mul_add(
        f(0xbeb9_2e1d).mul_add(r, f(0x3ef6_3993)),
        f(0xbf38_aa2d).mul_add(r, f(0x3fb8_aa3b)),
    );
    let base = f(LOG_CENTER[index]) + exponent as f32;
    let correction = (square * square).mul_add(r * f(0x3e92_0aa5), base);
    (r * polynomial + correction) * f(0x3f31_7218)
}

#[inline]
fn exp_element(x: f32) -> f32 {
    let bits = x.to_bits();
    if bits & 0x7fff_ffff > 0x7f80_0000 {
        return f(bits | 0x0040_0000);
    }
    let x = x.max(f(0xc2cf_f1b5)).min(f(0x42b1_7218));
    let exponent = (x * f(0x3fb8_aa3b)).floor();
    let r = exponent.mul_add(f(0xbf31_8000), x);
    let r = exponent.mul_add(f(0x395e_8083), r);
    let polynomial = f(0x3aea_6bc0).mul_add(r, f(0x3c04_5bfb));
    let polynomial = polynomial.mul_add(r, f(0x3d2a_ee9a));
    let polynomial = polynomial.mul_add(r, f(0x3e2a_a92b));
    let polynomial = polynomial.mul_add(r, f(0x3f00_0001));
    let polynomial = polynomial.mul_add(r, 1.0);
    let polynomial = r.mul_add(polynomial, 1.0);

    let exponent = exponent as i32;
    let tiny = exponent + 127 <= 0;
    let scale = if tiny { exponent + 254 } else { exponent + 127 };
    let scaled = polynomial * f((scale as u32) << 23);
    if tiny { scaled * f(0x0040_0000) } else { scaled }
}

#[inline]
fn fast_sine(x: f32) -> f32 {
    let square = x * x;
    let polynomial = f(0x362e_def8).mul_add(square, f(0xb94f_b7ff));
    let polynomial = polynomial.mul_add(square, f(0x3c08_8766));
    let polynomial = polynomial.mul_add(square, f(0xbe2a_aaa6));
    polynomial.mul_add(square * x, x)
}

#[inline]
fn slow_sine(x: f32) -> f32 {
    let square = x * x;
    let polynomial = f(0x362e_c33b).mul_add(square, f(0xb94f_b6c5));
    let polynomial = polynomial.mul_add(square, f(0x3c08_8767));
    let polynomial = polynomial.mul_add(square, f(0xbe2a_aaa6));
    (square * polynomial).mul_add(x, x)
}

#[inline]
fn reduce_pi(x: f32, multiple: f32) -> f32 {
    let r = (-multiple).mul_add(f(0x4049_0fdb), x);
    let r = (-multiple).mul_add(f(0xb3bb_bd2e), r);
    (-multiple).mul_add(f(0xa777_2ced), r)
}

#[inline]
fn sincos_fast(x: f32) -> (f32, f32) {
    let bits = x.to_bits();
    let magnitude = f(bits & 0x7fff_ffff);
    let quadrant = (magnitude * f(0x3ea2_f983)).round_ties_even() as i32;
    let multiple = quadrant as f32;
    let sine_remainder = reduce_pi(magnitude, multiple);
    let half = f(0x3f00_0000 | (sine_remainder.to_bits() & 0x8000_0000));
    let cosine_remainder = reduce_pi(magnitude, multiple + half);
    let parity = (quadrant as u32) << 31;
    let s = f(sine_remainder.to_bits() ^ parity);
    let c = f(cosine_remainder.to_bits()
        ^ (sine_remainder.to_bits() & 0x8000_0000) ^ parity ^ 0x8000_0000);
    (f(fast_sine(s).to_bits() ^ (bits & 0x8000_0000)), fast_sine(c))
}

// The large-input path computes both reductions in binary64, then narrows
// before applying its own binary32 polynomial. An infinity's generated NaN
// sign differs between ARM and x86, so preserve the ARM result explicitly.
#[inline]
fn sincos_slow(x: f32) -> (f32, f32) {
    let bits = x.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    if magnitude == 0x7f80_0000 {
        let nan = f(0xffc0_0000 ^ (bits & 0x8000_0000));
        return (nan, nan);
    }
    let index = ((magnitude >> 24) as usize).saturating_sub(64).min(63);
    let (hi, lo) = TRIG_REDUCTION[index];
    let hi = f64::from_bits(hi);
    let lo = f64::from_bits(lo);
    let x = x as f64;
    let sine_product = x * hi;
    let sine_integer = sine_product.round_ties_even() as i64;
    let sine_fraction = x.mul_add(lo, sine_product - sine_integer as f64);
    let sine_fraction = f64::from_bits(sine_fraction.to_bits() ^ ((sine_integer as u64) << 63));
    let cosine_product = x.mul_add(hi, 0.5);
    let cosine_integer = cosine_product.round_ties_even() as i64;
    let cosine_fraction = x.mul_add(lo, cosine_product - cosine_integer as f64);
    let cosine_fraction = f64::from_bits(cosine_fraction.to_bits() ^ ((cosine_integer as u64) << 63));
    (
        slow_sine(narrow_f32(sine_fraction) * f(0x4049_0fdb)),
        slow_sine(narrow_f32(cosine_fraction) * f(0x4049_0fdb)),
    )
}

/// Compute vector sine and cosine, matching the reference's 16-element group
/// selection. A single large or non-finite lane selects the slow reduction
/// for its whole group, including the final partial group.
///
/// All three slices must have the same length and may be empty.
pub fn vvsincosf(sine: &mut [f32], cosine: &mut [f32], input: &[f32]) {
    assert_eq!(sine.len(), input.len(), "vvsincosf sine length");
    assert_eq!(cosine.len(), input.len(), "vvsincosf cosine length");
    for ((sines, cosines), xs) in sine.chunks_mut(16).zip(cosine.chunks_mut(16)).zip(input.chunks(16)) {
        let slow = xs.iter().any(|x| x.to_bits() & 0x7fff_ffff > 0x4974_2400);
        for ((s, c), &x) in sines.iter_mut().zip(cosines).zip(xs) {
            (*s, *c) = if slow { sincos_slow(x) } else { sincos_fast(x) };
        }
    }
}

/// Compute the vector natural logarithm into an equally sized disjoint output.
pub fn vvlogf(output: &mut [f32], input: &[f32]) {
    assert_eq!(output.len(), input.len(), "vvlogf length");
    for (out, &x) in output.iter_mut().zip(input) {
        *out = log_element(x);
    }
}

/// Compute the vector natural logarithm in place.
pub fn vvlogf_in_place(data: &mut [f32]) {
    for x in data {
        *x = log_element(*x);
    }
}

/// Compute the vector exponential into an equally sized disjoint output.
pub fn vvexpf(output: &mut [f32], input: &[f32]) {
    assert_eq!(output.len(), input.len(), "vvexpf length");
    for (out, &x) in output.iter_mut().zip(input) {
        *out = exp_element(x);
    }
}

/// Compute the vector exponential in place.
pub fn vvexpf_in_place(data: &mut [f32]) {
    for x in data {
        *x = exp_element(*x);
    }
}
