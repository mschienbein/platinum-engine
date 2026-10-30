//! Portable counterparts for selected vDSP operations.
//!
//! These entry points use the pointer and stride conventions of the existing
//! Accelerate bindings. Their individual documentation describes their domain;
//! they are not a replacement for the complete vDSP API.
//!
//! The reference is macOS arm64 in round-to-nearest/ties-even mode with
//! FPCR.DN clear. The caller owns FZ; on x86 its matching scope must set both
//! FTZ and DAZ. Exception flags and nondefault rounding modes are not modeled.
//! Verification covers engine-used lengths/strides and sampled finite,
//! subnormal, zero, infinity and NaN inputs; it is not an exhaustive claim for
//! the complete vDSP API or every possible allocator/CPU dispatch profile.

#![allow(non_snake_case)]

/// The split-complex memory layout used by the existing bindings.
#[repr(C)]
pub struct SplitComplex {
    pub real: *mut f32,
    pub imaginary: *mut f32,
}

// ARM's IEEE operations give signaling NaNs priority and retain a specific
// operand's payload. Rust and x86 do not promise that ordering for arithmetic
// instructions, so make it explicit when exceptional operands are present.
#[inline]
fn is_signaling_nan(bits: u32) -> bool {
    bits & 0x7fc0_0000 == 0x7f80_0000 && bits & 0x003f_ffff != 0
}

#[inline]
fn is_nan_bits(bits: u32) -> bool {
    bits & 0x7fff_ffff > 0x7f80_0000
}

#[inline]
fn arm_add(a: f32, b: f32) -> f32 {
    let aa = a.to_bits();
    let bb = b.to_bits();
    if (aa & 0x7f80_0000) == 0x7f80_0000 || (bb & 0x7f80_0000) == 0x7f80_0000 {
        for bits in [aa, bb] {
            if is_signaling_nan(bits) { return f32::from_bits(bits | 0x0040_0000); }
        }
        for bits in [aa, bb] {
            if is_nan_bits(bits) { return f32::from_bits(bits); }
        }
        let result = a + b;
        return if result.is_nan() { f32::from_bits(0x7fc0_0000) } else { result };
    }
    a + b
}

macro_rules! arm_binary {
    ($name:ident, $op:tt) => {
        #[inline]
        fn $name(a: f32, b: f32) -> f32 {
            let aa = a.to_bits();
            let bb = b.to_bits();
            if (aa & 0x7f80_0000) == 0x7f80_0000 || (bb & 0x7f80_0000) == 0x7f80_0000 {
                for bits in [aa, bb] {
                    if is_signaling_nan(bits) { return f32::from_bits(bits | 0x0040_0000); }
                }
                for bits in [aa, bb] {
                    if is_nan_bits(bits) { return f32::from_bits(bits); }
                }
                let result = a $op b;
                return if result.is_nan() { f32::from_bits(0x7fc0_0000) } else { result };
            }
            let result = a $op b;
            if result.is_nan() { f32::from_bits(0x7fc0_0000) } else { result }
        }
    };
}

arm_binary!(arm_sub, -);
arm_binary!(arm_mul_raw, *);
arm_binary!(arm_div_raw, /);

#[inline]
fn arm_mul(a: f32, b: f32) -> f32 {
    finish_fma(a, b, 0.0, arm_mul_raw(a, b))
}

#[inline]
fn arm_div(a: f32, b: f32) -> f32 {
    finish_div(a, b, arm_div_raw(a, b))
}

#[inline]
fn flush_operand(value: f32) -> f32 {
    let bits = value.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    if magnitude != 0 && magnitude < 0x0080_0000 {
        // Probe only for subnormals: the enclosing callback owns the floating
        // point environment, which is not exposed by a portable Rust API.
        let normal = std::hint::black_box(f32::MIN_POSITIVE);
        let half = std::hint::black_box(0.5f32);
        if normal * half == 0.0 {
            return f32::from_bits(bits & 0x8000_0000);
        }
    }
    value
}

#[inline]
fn arm_max(a: f32, b: f32) -> f32 {
    let aa = a.to_bits();
    let bb = b.to_bits();
    for bits in [aa, bb] {
        if is_signaling_nan(bits) { return f32::from_bits(bits | 0x0040_0000); }
    }
    for bits in [aa, bb] {
        if is_nan_bits(bits) { return f32::from_bits(bits); }
    }
    let a = flush_operand(a);
    let b = flush_operand(b);
    if a > b { a }
    else if b > a { b }
    else if a == 0.0 {
        f32::from_bits(a.to_bits() & b.to_bits())
    } else { a }
}

#[inline]
fn arm_fma(a: f32, b: f32, addend: f32) -> f32 {
    let aa = a.to_bits();
    let bb = b.to_bits();
    let cc = addend.to_bits();
    if (aa & 0x7f80_0000) == 0x7f80_0000 ||
        (bb & 0x7f80_0000) == 0x7f80_0000 ||
        (cc & 0x7f80_0000) == 0x7f80_0000
    {
        for bits in [cc, aa, bb] {
            if is_signaling_nan(bits) { return f32::from_bits(bits | 0x0040_0000); }
        }
        let a_inf = aa & 0x7fff_ffff == 0x7f80_0000;
        let b_inf = bb & 0x7fff_ffff == 0x7f80_0000;
        if (a_inf && b == 0.0) || (b_inf && a == 0.0) {
            return f32::from_bits(0x7fc0_0000);
        }
        for bits in [cc, aa, bb] {
            if is_nan_bits(bits) { return f32::from_bits(bits); }
        }
        let result = a.mul_add(b, addend);
        return if result.is_nan() { f32::from_bits(0x7fc0_0000) } else { finish_fma(a, b, addend, result) };
    }
    finish_fma(a, b, addend, a.mul_add(b, addend))
}

// ARM FZ tests tininess before rounding. An x86 software FMA can round an
// exact result just below MIN_POSITIVE up to MIN_POSITIVE instead of flushing.
// This rare boundary path compares the exact signed binary32 product and
// addend with 2^-126 using fixed-width integer limbs. Inputs subnormal under
// FZ have already become signed zero for the ARM operation.
#[cfg(target_arch = "x86_64")]
fn fma_is_tiny(a: f32, b: f32, c: f32) -> bool {
    fn parts(value: f32) -> (u64, i32) {
        let bits = value.to_bits();
        let exponent = ((bits >> 23) & 0xff) as i32;
        if exponent == 0 { (0, 0) }
        else { (u64::from((bits & 0x007f_ffff) | 0x0080_0000), exponent - 150) }
    }
    fn put(limbs: &mut [u64; 9], value: u64, shift: usize) {
        if value == 0 { return; }
        let word = shift / 64;
        let bit = shift % 64;
        limbs[word] = value << bit;
        if bit != 0 { limbs[word + 1] = value >> (64 - bit); }
    }
    let (asig, aexp) = parts(a);
    let (bsig, bexp) = parts(b);
    let (csig, cexp) = parts(c);
    let mut product = [0u64; 9];
    let mut addend = [0u64; 9];
    if asig != 0 && bsig != 0 {
        put(&mut product, asig * bsig, (aexp + bexp + 298) as usize);
    }
    if csig != 0 { put(&mut addend, csig, (cexp + 298) as usize); }
    let product_sign = (a.to_bits() ^ b.to_bits()) & 0x8000_0000;
    let addend_sign = c.to_bits() & 0x8000_0000;
    let mut magnitude = [0u64; 9];
    if product_sign == addend_sign {
        let mut carry = false;
        for i in 0..9 {
            let (sum, first) = product[i].overflowing_add(addend[i]);
            let (sum, second) = sum.overflowing_add(u64::from(carry));
            magnitude[i] = sum;
            carry = first || second;
        }
    } else {
        let product_larger = product.iter().rev().cmp(addend.iter().rev()) != std::cmp::Ordering::Less;
        let (larger, smaller) = if product_larger { (&product, &addend) } else { (&addend, &product) };
        let mut borrow = false;
        for i in 0..9 {
            let (difference, first) = larger[i].overflowing_sub(smaller[i]);
            let (difference, second) = difference.overflowing_sub(u64::from(borrow));
            magnitude[i] = difference;
            borrow = first || second;
        }
    }
    magnitude[3..].iter().all(|&word| word == 0) && magnitude[2] < (1u64 << 44)
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn finish_fma(a: f32, b: f32, addend: f32, result: f32) -> f32 {
    let bits = result.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    if magnitude != 0 && magnitude <= 0x0080_0000 &&
        flush_operand(f32::from_bits(1)).to_bits() == 0 &&
        (magnitude < 0x0080_0000 || fma_is_tiny(a, b, addend))
    {
        f32::from_bits(bits & 0x8000_0000)
    } else {
        result
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn finish_fma(_a: f32, _b: f32, _addend: f32, result: f32) -> f32 {
    result
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn finish_div(numerator: f32, denominator: f32, result: f32) -> f32 {
    let bits = result.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    if magnitude != 0 && magnitude <= 0x0080_0000 &&
        flush_operand(f32::from_bits(1)).to_bits() == 0
    {
        let tiny = magnitude < 0x0080_0000 || {
            let a = widen_f32(flush_operand(numerator)).abs();
            let b = widen_f32(flush_operand(denominator)).abs();
            a < b * (f32::MIN_POSITIVE as f64)
        };
        if tiny { return f32::from_bits(bits & 0x8000_0000); }
    }
    result
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn finish_div(_numerator: f32, _denominator: f32, result: f32) -> f32 {
    result
}

// On the reference machine the large, unit-stride elementwise kernels use
// an accelerator that performs gradual underflow independently of FPCR.FZ and
// returns a default NaN. Binary64 represents each binary32 operand exactly;
// its product is exact, and its sum has sufficient precision for a correctly
// rounded binary32 result. Perform the final narrowing with integer rounding.
#[inline]
fn widen_f32(value: f32) -> f64 {
    let bits = value.to_bits();
    let sign = ((bits as u64) & 0x8000_0000) << 32;
    let exponent = (bits >> 23) & 0xff;
    let fraction = bits & 0x007f_ffff;
    let result = if exponent == 0 {
        if fraction == 0 { sign } else {
            let high = 31 - fraction.leading_zeros();
            let e = (high as i32 - 149 + 1023) as u64;
            let mantissa = ((fraction ^ (1 << high)) as u64) << (52 - high);
            sign | (e << 52) | mantissa
        }
    } else if exponent == 0xff {
        sign | 0x7ff0_0000_0000_0000 | ((fraction as u64) << 29)
    } else {
        sign | (((exponent as u64) + 896) << 52) | ((fraction as u64) << 29)
    };
    f64::from_bits(result)
}

#[inline]
fn round_shift_even(value: u64, shift: u32) -> u64 {
    if shift == 0 { return value; }
    if shift >= 64 { return 0; }
    let upper = value >> shift;
    let remainder = value & ((1u64 << shift) - 1);
    let half = 1u64 << (shift - 1);
    upper + u64::from(remainder > half || (remainder == half && upper & 1 != 0))
}

#[inline]
fn narrow_f64_gradual(value: f64) -> f32 {
    let bits = value.to_bits();
    let sign = ((bits >> 32) as u32) & 0x8000_0000;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & 0x000f_ffff_ffff_ffff;
    if exp == 0x7ff {
        return f32::from_bits(if fraction == 0 { sign | 0x7f80_0000 } else { 0x7fc0_0000 });
    }
    if exp == 0 { return f32::from_bits(sign); }
    let unbiased = exp - 1023;
    let significand = fraction | (1u64 << 52);
    if unbiased >= -126 {
        let mut mantissa = round_shift_even(significand, 29);
        let mut target_exp = unbiased + 127;
        if mantissa == 1 << 24 {
            mantissa >>= 1;
            target_exp += 1;
        }
        if target_exp >= 255 { return f32::from_bits(sign | 0x7f80_0000); }
        return f32::from_bits(sign | ((target_exp as u32) << 23) | (mantissa as u32 & 0x007f_ffff));
    }
    let shift = (-unbiased - 97) as u32;
    f32::from_bits(sign | round_shift_even(significand, shift) as u32)
}

#[inline]
fn accelerator_binary(a: f32, b: f32, operation: u8, host_fz: bool) -> f32 {
    let aa = a.to_bits();
    let bb = b.to_bits();
    if is_nan_bits(aa) || is_nan_bits(bb) {
        return f32::from_bits(0x7fc0_0000);
    }
    let amag = aa & 0x7fff_ffff;
    let bmag = bb & 0x7fff_ffff;
    let input_denormal = (amag != 0 && amag < 0x0080_0000) ||
                         (bmag != 0 && bmag < 0x0080_0000);
    if !host_fz || !input_denormal {
        let result = match operation {
            0 => a * b,
            1 => a + b,
            _ => a - b,
        };
        let result_bits = result.to_bits();
        if is_nan_bits(result_bits) { return f32::from_bits(0x7fc0_0000); }
        if !host_fz || result_bits & 0x7fff_ffff != 0 {
            return result;
        }
        let exact_zero = match operation {
            0 => amag == 0 || bmag == 0,
            1 => (amag == 0 && bmag == 0) || (aa ^ bb == 0x8000_0000),
            _ => (amag == 0 && bmag == 0) || aa == bb,
        };
        if exact_zero { return result; }
    }
    // FZ can have discarded an input or a tiny result. Recover that rare case
    // with exact binary64 intermediates and explicit gradual narrowing.
    let a = widen_f32(a);
    let b = widen_f32(b);
    let result = match operation {
        0 => a * b,
        1 => a + b,
        _ => a - b,
    };
    narrow_f64_gradual(result)
}

// AArch64's reciprocal estimate is specified using a rounded 9-bit mantissa.
// Reconstruct that estimate with integer arithmetic rather than depending on
// the current machine's reciprocal-estimate instruction.
const fn reciprocal_estimates() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0usize;
    while i < table.len() {
        let scaled = 256 + i as u32;
        let estimate = (((1u32 << 19) / (2 * scaled + 1)) + 1) / 2;
        table[i] = (estimate & 255) as u8;
        i += 1;
    }
    table
}
const RECIPROCAL_ESTIMATES: [u8; 256] = reciprocal_estimates();

#[inline]
fn arm_recip_estimate(value: f32) -> f32 {
    let bits = flush_operand(value).to_bits();
    let sign = bits & 0x8000_0000;
    let magnitude = bits & 0x7fff_ffff;
    if magnitude > 0x7f80_0000 { return f32::from_bits(bits | 0x0040_0000); }
    if magnitude == 0x7f80_0000 { return f32::from_bits(sign); }
    if magnitude < 0x0020_0000 { return f32::from_bits(sign | 0x7f80_0000); }
    let mut exponent = (magnitude >> 23) as i32;
    let mut fraction = magnitude & 0x007f_ffff;
    if exponent == 0 {
        if fraction & 0x0040_0000 == 0 {
            exponent = -1;
            fraction = (fraction << 2) & 0x007f_ffff;
        } else {
            fraction = (fraction << 1) & 0x007f_ffff;
        }
    }
    if exponent >= 253 && flush_operand(f32::from_bits(1)).to_bits() == 0 {
        return f32::from_bits(sign);
    }
    let mut mantissa = u32::from(RECIPROCAL_ESTIMATES[(fraction >> 15) as usize]) << 15;
    let mut result_exponent = 253 - exponent;
    if result_exponent <= 0 {
        mantissa = (0x0080_0000 | mantissa) >> (1 - result_exponent);
        result_exponent = 0;
    }
    f32::from_bits(sign | ((result_exponent as u32) << 23) | mantissa)
}

#[inline]
fn arm_recip_step(a: f32, b: f32) -> f32 {
    let aa = a.to_bits() ^ 0x8000_0000;
    let bb = b.to_bits();
    for bits in [aa, bb] {
        if is_signaling_nan(bits) { return f32::from_bits(bits | 0x0040_0000); }
    }
    for bits in [aa, bb] {
        if is_nan_bits(bits) { return f32::from_bits(bits); }
    }
    if (aa & 0x7fff_ffff == 0x7f80_0000 && b == 0.0) ||
        (bb & 0x7fff_ffff == 0x7f80_0000 && a == 0.0)
    {
        2.0
    } else {
        arm_fma(-a, b, 2.0)
    }
}

/// Multiplies corresponding binary32 samples. The engine uses stride 1;
/// output is disjoint from an input or exactly aliases that input.
///
/// # Safety
/// All pointers must cover their strided ranges. Partial overlap is unsupported.
pub unsafe fn vDSP_vmul(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                        output: *mut f32, output_stride: isize, length: usize) {
    let accelerator = length >= 20480 && a_stride == 1 && b_stride == 1 && output_stride == 1;
    let accelerator_fz = accelerator && flush_operand(f32::from_bits(1)).to_bits() == 0;
    for i in 0..length {
        let a = unsafe { a.offset(i as isize * a_stride).read() };
        let b = unsafe { b.offset(i as isize * b_stride).read() };
        let result = if accelerator { accelerator_binary(b, a, 0, accelerator_fz) } else { arm_mul(b, a) };
        unsafe { output.offset(i as isize * output_stride).write(result) };
    }
}

/// Adds corresponding binary32 samples. The engine uses stride 1; output is
/// disjoint from an input or exactly aliases that input.
///
/// # Safety
/// All pointers must cover their strided ranges. Partial overlap is unsupported.
pub unsafe fn vDSP_vadd(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                        output: *mut f32, output_stride: isize, length: usize) {
    let accelerator = length >= 20480 && a_stride == 1 && b_stride == 1 && output_stride == 1;
    let accelerator_fz = accelerator && flush_operand(f32::from_bits(1)).to_bits() == 0;
    for i in 0..length {
        let a = unsafe { a.offset(i as isize * a_stride).read() };
        let b = unsafe { b.offset(i as isize * b_stride).read() };
        let result = if accelerator { accelerator_binary(b, a, 1, accelerator_fz) } else { arm_add(b, a) };
        unsafe { output.offset(i as isize * output_stride).write(result) };
    }
}

/// Subtracts the first input from the second. The engine uses stride 1; output
/// is disjoint from an input or exactly aliases that input.
///
/// # Safety
/// All pointers must cover their strided ranges. Partial overlap is unsupported.
pub unsafe fn vDSP_vsub(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                        output: *mut f32, output_stride: isize, length: usize) {
    let accelerator = length >= 20480 && a_stride == 1 && b_stride == 1 && output_stride == 1;
    let accelerator_fz = accelerator && flush_operand(f32::from_bits(1)).to_bits() == 0;
    for i in 0..length {
        let a = unsafe { a.offset(i as isize * a_stride).read() };
        let b = unsafe { b.offset(i as isize * b_stride).read() };
        let result = if accelerator { accelerator_binary(b, a, 2, accelerator_fz) } else { arm_sub(b, a) };
        unsafe { output.offset(i as isize * output_stride).write(result) };
    }
}

/// Divides the second input by the first. The engine uses stride 1; output is
/// disjoint from an input or exactly aliases that input.
///
/// # Safety
/// All pointers must cover their strided ranges. Partial overlap is unsupported.
pub unsafe fn vDSP_vdiv(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                        output: *mut f32, output_stride: isize, length: usize) {
    for i in 0..length {
        let a = unsafe { a.offset(i as isize * a_stride).read() };
        let b = unsafe { b.offset(i as isize * b_stride).read() };
        unsafe { output.offset(i as isize * output_stride).write(arm_div(b, a)) };
    }
}

/// Multiplies by the scalar addressed by `factor`. The engine uses stride 1
/// and either distinct buffers or exact input/output aliasing.
///
/// # Safety
/// All pointers must cover their strided ranges; `factor` must be readable.
/// Partial input/output overlap is unsupported.
pub unsafe fn vDSP_vsmul(input: *const f32, input_stride: isize, factor: *const f32,
                          output: *mut f32, output_stride: isize, length: usize) {
    if length == 0 { return; }
    let factor = unsafe { factor.read() };
    let accelerator = length >= 20480 && input_stride == 1 && output_stride == 1;
    let accelerator_fz = accelerator && flush_operand(f32::from_bits(1)).to_bits() == 0;
    for i in 0..length {
        let value = unsafe { input.offset(i as isize * input_stride).read() };
        let result = if accelerator { accelerator_binary(value, factor, 0, accelerator_fz) } else { arm_mul(value, factor) };
        unsafe { output.offset(i as isize * output_stride).write(result) };
    }
}

/// Takes the IEEE maximum of each pair. The engine uses stride 1; output is
/// disjoint from an input or exactly aliases that input.
///
/// # Safety
/// All pointers must cover their strided ranges. Partial overlap is unsupported.
pub unsafe fn vDSP_vmax(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                        output: *mut f32, output_stride: isize, length: usize) {
    for i in 0..length {
        let a = unsafe { a.offset(i as isize * a_stride).read() };
        let b = unsafe { b.offset(i as isize * b_stride).read() };
        unsafe { output.offset(i as isize * output_stride).write(arm_max(b, a)) };
    }
}

/// Finds the first maximum using the engine's unit-stride reduction profile.
/// A NaN within a four-aligned vector block makes that block unordered, while
/// NaNs in a scalar tail are ignored. Zero length returns `(-infinity, 0)`.
///
/// # Safety
/// `input` must cover `length` unit-stride samples. `value` and `index` must be
/// writable, disjoint from each other and from the input.
pub unsafe fn vDSP_maxvi(input: *const f32, stride: isize, value: *mut f32,
                          index: *mut usize, length: usize) {
    assert_eq!(stride, 1, "unsupported vDSP_maxvi stride");
    if length == 0 {
        unsafe { value.write(f32::NEG_INFINITY); index.write(0); }
        return;
    }
    let mut best = f32::NEG_INFINITY;
    let mut best_index = 0usize;
    let mut any = false;
    let block_size = length.clamp(4, 256) & !3;
    let vector_end = if length < 4 { 0 } else { length & !3 };
    let mut start = 0usize;
    while start < vector_end {
        let end = (start + block_size).min(vector_end);
        let mut block_max = f32::NEG_INFINITY;
        let mut block_index = start;
        let mut has_nan = false;
        for i in start..end {
            let current = unsafe { input.add(i).read() };
            if is_nan_bits(current.to_bits()) {
                has_nan = true;
            } else if current > block_max {
                block_max = current;
                block_index = i;
            }
        }
        if !has_nan && block_max > best {
            best = block_max;
            best_index = block_index;
            any = true;
        }
        start = end;
    }
    for i in vector_end..length {
        let current = unsafe { input.add(i).read() };
        if current > best {
            best = current;
            best_index = i;
            any = true;
        }
    }
    if any {
        unsafe { value.write(input.add(best_index).read()); index.write(best_index); }
    } else if length < 4 {
        let first = unsafe { input.read() };
        unsafe {
            value.write(if is_nan_bits(first.to_bits()) { f32::NEG_INFINITY } else { first });
            index.write(0);
        }
    } else {
        unsafe { value.write(f32::NEG_INFINITY); index.write(0); }
    }
}

/// Clears the sign bit of each sample. The engine uses stride 1 and either
/// distinct buffers or exact input/output aliasing.
///
/// # Safety
/// All pointers must cover their strided ranges. Partial overlap is unsupported.
pub unsafe fn vDSP_vabs(input: *const f32, input_stride: isize, output: *mut f32,
                         output_stride: isize, length: usize) {
    for i in 0..length {
        let value = unsafe { input.offset(i as isize * input_stride).read() };
        unsafe { output.offset(i as isize * output_stride).write(value.abs()) };
    }
}

/// Computes split-complex magnitudes with a rounded real square followed by
/// a fused imaginary square and square root. The engine uses unit strides and
/// writes either to a separate buffer or exactly over the real plane.
///
/// # Safety
/// All pointers must cover their strided ranges. Partial overlap is unsupported.
pub unsafe fn vDSP_zvabs(input: *const SplitComplex, input_stride: isize,
                          output: *mut f32, output_stride: isize, length: usize) {
    let (real, imaginary) = unsafe { ((*input).real, (*input).imaginary) };
    for i in 0..length {
        let offset = i as isize * input_stride;
        let x = unsafe { real.offset(offset).read() };
        let y = unsafe { imaginary.offset(offset).read() };
        let square = arm_mul(x, x);
        let magnitude = arm_fma(y, y, square).sqrt();
        unsafe { output.offset(i as isize * output_stride).write(magnitude) };
    }
}

#[inline]
fn phase_lane(real: f32, imaginary: f32) -> f32 {
    let estimate = arm_recip_estimate(real);
    let refined = arm_mul(estimate, arm_recip_step(real, estimate));
    let correction = arm_recip_step(real, refined);
    let ratio = arm_mul(correction, arm_mul(imaginary, refined));
    let magnitude = ratio.abs();
    let large = magnitude > f32::from_bits(0x401a_827a);
    let middle = magnitude > f32::from_bits(0x3ed4_13cd);
    let reduced = if middle {
        let numerator = if large { -1.0 } else { arm_add(magnitude, -1.0) };
        let denominator = if large { magnitude } else { arm_add(magnitude, 1.0) };
        let reciprocal = arm_recip_estimate(denominator);
        let mut quotient = arm_mul(reciprocal, numerator);
        let error = arm_fma(-denominator, reciprocal, 1.0);
        quotient = arm_fma(quotient, error, quotient);
        let squared = arm_mul(error, error);
        quotient = arm_fma(quotient, squared, quotient);
        let fourth = arm_mul(squared, squared);
        quotient = arm_fma(quotient, fourth, quotient);
        if reciprocal == 0.0 { reciprocal } else { quotient }
    } else {
        magnitude
    };
    let offset = if large {
        f32::from_bits(0x3fc9_0fdb)
    } else if middle {
        f32::from_bits(0x3f49_0fdb)
    } else {
        0.0
    };
    let square = arm_mul(reduced, reduced);
    let p = arm_fma(f32::from_bits(0x3da4_f0d1), square, f32::from_bits(0xbe0e_1b85));
    let p = arm_fma(p, square, f32::from_bits(0x3e4c_925f));
    let p = arm_fma(p, square, f32::from_bits(0xbeaa_aa2a));
    let angle = arm_fma(arm_mul(square, p), reduced, reduced);
    let angle = arm_add(offset, angle);
    let angle = f32::from_bits(angle.to_bits() | (ratio.to_bits() & 0x8000_0000));
    let mut axis = if real < 0.0 { f32::from_bits(0x4049_0fdb) } else { -0.0 };
    if imaginary < 0.0 { axis = -axis; }
    let mut result = arm_add(axis, angle);
    if imaginary == 0.0 { result = axis; }
    if real == 0.0 {
        result = if imaginary == 0.0 { -0.0 } else { f32::from_bits(0x3fc9_0fdb) };
        if imaginary < 0.0 { result = -result; }
    }
    result
}

/// Computes phase using the reference's aligned vector and scalar profiles.
/// The pitch vocoder uses unit strides with a 16-byte aligned output.
///
/// # Safety
/// All pointers must cover their strided ranges, and the output must not
/// overlap either input plane.
pub unsafe fn vDSP_zvphas(input: *const SplitComplex, input_stride: isize,
                           output: *mut f32, output_stride: isize, length: usize) {
    let (real, imaginary) = unsafe { ((*input).real, (*input).imaginary) };
    let mut i = 0usize;
    if input_stride == 1 && output_stride == 1 {
        while i < length && unsafe { output.add(i) } as usize % 16 != 0 {
            let x = unsafe { real.add(i).read() };
            let y = unsafe { imaginary.add(i).read() };
            unsafe { output.add(i).write(crate::portable::math::atan2f(y, x)) };
            i += 1;
        }
        while i + 4 <= length {
            for lane in 0..4 {
                let x = unsafe { real.add(i + lane).read() };
                let y = unsafe { imaginary.add(i + lane).read() };
                unsafe { output.add(i + lane).write(phase_lane(x, y)) };
            }
            i += 4;
        }
    }
    while i < length {
        let offset = i as isize * input_stride;
        let x = unsafe { real.offset(offset).read() };
        let y = unsafe { imaginary.offset(offset).read() };
        let result = crate::portable::math::atan2f(y, x);
        unsafe { output.offset(i as isize * output_stride).write(result) };
        i += 1;
    }
}

/// Deinterleave complex samples. The engine uses input stride 2 and split
/// stride 1, with distinct source and destination allocations.
///
/// # Safety
/// The pointers must address all samples implied by the strides and length;
/// source and destinations must not overlap.
pub unsafe fn vDSP_ctoz(
    input: *const f32,
    input_stride: isize,
    output: *mut SplitComplex,
    output_stride: isize,
    length: usize,
) {
    let (real, imaginary) = unsafe { ((*output).real, (*output).imaginary) };
    for i in 0..length {
        let src = unsafe { input.offset((i as isize) * input_stride) };
        let dst = (i as isize) * output_stride;
        unsafe {
            real.offset(dst).write(src.read());
            imaginary.offset(dst).write(src.add(1).read());
        }
    }
}

/// Interleave split-complex samples. The engine uses split stride 1 and output
/// stride 2, with distinct source and destination allocations.
///
/// # Safety
/// The pointers must address all samples implied by the strides and length;
/// source and destination must not overlap.
pub unsafe fn vDSP_ztoc(
    input: *const SplitComplex,
    input_stride: isize,
    output: *mut f32,
    output_stride: isize,
    length: usize,
) {
    let (real, imaginary) = unsafe { ((*input).real, (*input).imaginary) };
    for i in 0..length {
        let src = (i as isize) * input_stride;
        let dst = unsafe { output.offset((i as isize) * output_stride) };
        unsafe {
            dst.write(real.offset(src).read());
            dst.add(1).write(imaginary.offset(src).read());
        }
    }
}

/// Dot product with the accumulation order used for unit-stride vectors.
/// The tuning detector passes unit strides and both short band windows and
/// 256-element aggregates. This function does not change the FP environment.
///
/// # Safety
/// Both input pointers must cover `length` elements at their respective
/// strides, and `result` must be writable and disjoint from the inputs.
pub unsafe fn vDSP_dotpr(
    a: *const f32,
    a_stride: isize,
    b: *const f32,
    b_stride: isize,
    result: *mut f32,
    length: usize,
) {
    assert_eq!((a_stride, b_stride), (1, 1), "unsupported vDSP_dotpr strides");
    if length == 0 {
        unsafe { result.write(0.0) };
        return;
    }
    if length < 4 {
        let mut sum = 0.0f32;
        for i in 0..length {
            let av = unsafe { a.offset((i as isize) * a_stride).read() };
            let bv = unsafe { b.offset((i as isize) * b_stride).read() };
            sum = arm_fma(bv, av, sum);
        }
        unsafe { result.write(sum) };
        return;
    }

    let mut accum = [[0.0f32; 4]; 8];
    let mut position = 0usize;
    while position + 32 <= length {
        for (group, offset) in [8usize, 12, 16, 20, 24, 28, 4, 0].into_iter().enumerate() {
            for lane in 0..4 {
                let index = position + offset + lane;
                let av = unsafe { a.add(index).read() };
                let bv = unsafe { b.add(index).read() };
                accum[group][lane] = arm_fma(bv, av, accum[group][lane]);
            }
        }
        position += 32;
    }
    if position != 0 {
        for lane in 0..4 {
            let p0 = arm_add(accum[0][lane], accum[1][lane]);
            let p1 = arm_add(accum[2][lane], accum[3][lane]);
            let p2 = arm_add(accum[4][lane], accum[5][lane]);
            let p3 = arm_add(accum[6][lane], accum[7][lane]);
            accum[0][lane] = arm_add(p0, p1);
            accum[1][lane] = arm_add(p2, p3);
        }
    }
    while position + 8 <= length {
        for lane in 0..4 {
            for group in 0..2 {
                let index = position + group * 4 + lane;
                let av = unsafe { a.add(index).read() };
                let bv = unsafe { b.add(index).read() };
                accum[group][lane] = arm_fma(bv, av, accum[group][lane]);
            }
        }
        position += 8;
    }
    for lane in 0..4 {
        accum[0][lane] = arm_add(accum[0][lane], accum[1][lane]);
    }
    if position + 4 <= length {
        for lane in 0..4 {
            let av = unsafe { a.add(position + lane).read() };
            let bv = unsafe { b.add(position + lane).read() };
            accum[0][lane] = arm_fma(bv, av, accum[0][lane]);
        }
        position += 4;
    }
    let mut lo = arm_add(accum[0][0], accum[0][1]);
    let mut hi = arm_add(accum[0][2], accum[0][3]);
    if position + 2 <= length {
        lo = arm_fma(unsafe { a.add(position).read() }, unsafe { b.add(position).read() }, lo);
        hi = arm_fma(unsafe { a.add(position + 1).read() }, unsafe { b.add(position + 1).read() }, hi);
        position += 2;
    }
    let mut sum = arm_add(lo, hi);
    if position < length {
        sum = arm_fma(unsafe { b.add(position).read() }, unsafe { a.add(position).read() }, sum);
    }
    unsafe { result.write(sum) };
}

/// Root mean square for the tuning detector's 4096-sample, unit-stride frame.
/// This entry point deliberately rejects other shapes rather than guessing
/// their potentially different reduction order.
///
/// # Safety
/// `input` must address 4096 initialized samples and `result` must be writable
/// and disjoint from the input.
pub unsafe fn vDSP_rmsqv(input: *const f32, stride: isize, result: *mut f32, length: usize) {
    assert_eq!((stride, length), (1, 4096), "unsupported vDSP_rmsqv shape");
    let mut accum = [[0.0f32; 4]; 8];
    for group in 0..8 {
        for lane in 0..4 {
            let x = unsafe { input.add(group * 4 + lane).read() };
            accum[group][lane] = arm_mul(x, x);
        }
    }
    for start in (32..length - 32).step_by(32) {
        for group in 0..8 {
            for lane in 0..4 {
                let x = unsafe { input.add(start + group * 4 + lane).read() };
                accum[group][lane] = arm_fma(x, x, accum[group][lane]);
            }
        }
    }
    let start = length - 32;
    for group in 0..4 {
        for lane in 0..4 {
            let x = unsafe { input.add(start + group * 4 + lane).read() };
            let y = unsafe { input.add(start + 16 + group * 4 + lane).read() };
            let first = arm_fma(x, x, accum[group][lane]);
            let folded = arm_add(first, accum[group + 4][lane]);
            accum[group][lane] = arm_fma(y, y, folded);
        }
    }
    let mut lanes = [0.0f32; 4];
    for lane in 0..4 {
        let left = arm_add(accum[0][lane], accum[2][lane]);
        let right = arm_add(accum[1][lane], accum[3][lane]);
        lanes[lane] = arm_add(left, right);
    }
    let sum = arm_add(arm_add(lanes[0], lanes[1]), arm_add(lanes[2], lanes[3]));
    unsafe { result.write((sum / length as f32).sqrt()) };
}

struct FftHandle {
    log2n: usize,
    plan: crate::portable::fft::FftSetup,
}

/// Creates a radix-2 setup for the engine's real FFT sizes 2^9 through 2^15.
/// Other sizes or radix values return null. The selected reference profile is
/// the Mac's 64-byte-aligned split scratch path.
///
/// # Safety
/// The returned non-null pointer must be passed exactly once to
/// [`vDSP_destroy_fftsetup`] after all transforms using it have completed.
pub unsafe fn vDSP_create_fftsetup(log2n: usize, radix: i32) -> *mut std::ffi::c_void {
    if radix != 0 || !(9..=15).contains(&log2n) {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(FftHandle {
        log2n,
        plan: crate::portable::fft::FftSetup::new(log2n),
    })).cast()
}

/// Releases one setup returned by [`vDSP_create_fftsetup`]. Null is a no-op.
///
/// # Safety
/// A non-null `setup` must be a live pointer returned by this module, and
/// no transform may still be using it.
pub unsafe fn vDSP_destroy_fftsetup(setup: *mut std::ffi::c_void) {
    if !setup.is_null() {
        unsafe { drop(Box::from_raw(setup.cast::<FftHandle>())) };
    }
}

/// Applies the selected aligned-0 real FFT in place. The engine passes unit
/// stride and the setup's own log2 size; direction is +1 forward or -1 inverse.
/// Forward produces twice the usual real DFT, with Nyquist in imaginary[0];
/// inverse is unnormalized. This does not alter the caller's floating mode.
///
/// # Safety
/// `setup` must be live and created by this module. `data` must contain two
/// disjoint, writable, 64-byte-aligned planes of `2^(log2n-1)` samples.
pub unsafe fn vDSP_fft_zrip(
    setup: *mut std::ffi::c_void,
    data: *mut SplitComplex,
    stride: isize,
    log2n: usize,
    direction: i32,
) {
    assert!(!setup.is_null() && !data.is_null(), "null FFT setup or data");
    let setup = unsafe { &*setup.cast::<FftHandle>() };
    assert_eq!(log2n, setup.log2n, "FFT size does not match its setup");
    assert_eq!(stride, 1, "unsupported FFT stride");
    assert!(direction == 1 || direction == -1, "unsupported FFT direction");
    let (real, imaginary) = unsafe { ((*data).real, (*data).imaginary) };
    assert!(real as usize % 64 == 0 && imaginary as usize % 64 == 0,
            "FFT requires the selected aligned-0 profile");
    let len = 1usize << (log2n - 1);
    let real = unsafe { std::slice::from_raw_parts_mut(real, len) };
    let imaginary = unsafe { std::slice::from_raw_parts_mut(imaginary, len) };
    if direction == 1 {
        setup.plan.forward(real, imaginary);
    } else {
        setup.plan.inverse(real, imaginary);
    }
}

/// Safe unit-stride form of [`vDSP_dotpr`] for vectors of equal length, with the same accumulation order.
///
/// # Panics
/// If the slices differ in length.
pub fn dotpr(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "dotpr needs vectors of equal length");
    let mut result = 0.0;
    // SAFETY: both slices cover `a.len()` unit-stride samples and `result` is a distinct local.
    unsafe { vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut result, a.len()) };
    result
}

/// Safe unit-stride form of [`vDSP_maxvi`]: the maximum and the index of its first occurrence, `(-infinity, 0)` for an empty slice.
pub fn maxvi(input: &[f32]) -> (f32, usize) {
    let (mut value, mut index) = (0.0, 0);
    // SAFETY: the slice covers `input.len()` unit-stride samples; `value` and `index` are distinct locals.
    unsafe { vDSP_maxvi(input.as_ptr(), 1, &mut value, &mut index, input.len()) };
    (value, index)
}
