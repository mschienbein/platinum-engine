//! Portable value semantics for the ARM64 floating-point instructions used by
//! the engine. Operations assume round-to-nearest, ties-to-even, as in the
//! reference host. Floating-point exception flags and traps are not emulated.
//!
//! [`FpMode`] makes FPCR.FZ and FPCR.DN explicit. The ordinary convenience
//! functions select gradual underflow with DN off; their `_fz` counterparts
//! select flush-to-zero with DN off. `_with_mode` additionally permits DN.
//! No function reads or changes the host's floating-point control register.
//!
//! ARM FZ flushes a nonzero exact result when it is tiny *before* rounding.
//! x86 FTZ may instead round first, and FTZ also need not flush an exact tiny
//! result in the same circumstances. The routines check the result and use
//! integer arithmetic at the boundary, independently of the host FTZ rule.
//! Subnormal inputs are flushed in software in FZ mode. The rare gradual-mode
//! cases which could be corrupted by the host's FTZ/DAZ are also evaluated in
//! integer arithmetic; ordinary fused operations use `mul_add`.

/// ARM FPCR value controls relevant to these operations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FpMode {
    pub flush_to_zero: bool,
    pub default_nan: bool,
}

impl FpMode {
    /// The ordinary IEEE mode used outside the host's flush scope.
    pub const IEEE: Self = Self { flush_to_zero: false, default_nan: false };
    /// Flush-to-zero mode used inside the reference host's flush scope.
    pub const FZ: Self = Self { flush_to_zero: true, default_nan: false };
}

#[derive(Clone, Copy)]
struct Format {
    fraction: u32,
    exponent: u32,
    bias: i32,
}

const SINGLE: Format = Format { fraction: 23, exponent: 8, bias: 127 };
const DOUBLE: Format = Format { fraction: 52, exponent: 11, bias: 1023 };

impl Format {
    #[inline(always)]
    fn sign(self) -> u64 { 1_u64 << (self.fraction + self.exponent) }
    #[inline(always)]
    fn fraction_mask(self) -> u64 { (1_u64 << self.fraction) - 1 }
    #[inline(always)]
    fn infinity(self) -> u64 { ((1_u64 << self.exponent) - 1) << self.fraction }
    #[inline(always)]
    fn min_normal(self) -> u64 { 1_u64 << self.fraction }
    #[inline(always)]
    fn quiet(self) -> u64 { 1_u64 << (self.fraction - 1) }
    #[inline(always)]
    fn default_nan(self) -> u64 { self.infinity() | self.quiet() }
    #[inline(always)]
    fn abs(self, bits: u64) -> u64 { bits & !self.sign() }
    #[inline(always)]
    fn is_nan(self, bits: u64) -> bool { self.abs(bits) > self.infinity() }
    #[inline(always)]
    fn is_snan(self, bits: u64) -> bool { self.is_nan(bits) && bits & self.quiet() == 0 }
    #[inline(always)]
    fn is_subnormal(self, bits: u64) -> bool {
        let mag = self.abs(bits);
        mag != 0 && mag < self.min_normal()
    }
    #[inline(always)]
    fn flush(self, bits: u64, mode: FpMode) -> u64 {
        if mode.flush_to_zero && self.is_subnormal(bits) { bits & self.sign() } else { bits }
    }
    #[inline(always)]
    fn processed_nan(self, bits: u64, mode: FpMode) -> u64 {
        if mode.default_nan { self.default_nan() } else { bits | self.quiet() }
    }

    // FPProcessNaNs: all signaling NaNs take priority over quiet NaNs.
    #[inline(always)]
    fn nan2(self, a: u64, b: u64, mode: FpMode) -> Option<u64> {
        if self.is_snan(a) { Some(self.processed_nan(a, mode)) }
        else if self.is_snan(b) { Some(self.processed_nan(b, mode)) }
        else if self.is_nan(a) { Some(self.processed_nan(a, mode)) }
        else if self.is_nan(b) { Some(self.processed_nan(b, mode)) }
        else { None }
    }

    // FPMulAdd calls FPProcessNaNs3(addend, multiplicand1, multiplicand2).
    #[inline(always)]
    fn nan3(self, a: u64, b: u64, c: u64, mode: FpMode) -> Option<u64> {
        if self.is_snan(c) { Some(self.processed_nan(c, mode)) }
        else if self.is_snan(a) { Some(self.processed_nan(a, mode)) }
        else if self.is_snan(b) { Some(self.processed_nan(b, mode)) }
        // An invalid infinity * zero takes precedence over a quiet addend NaN.
        else if self.is_nan(c) && self.invalid_product(a, b) { Some(self.default_nan()) }
        else if self.is_nan(c) { Some(self.processed_nan(c, mode)) }
        else if self.is_nan(a) { Some(self.processed_nan(a, mode)) }
        else if self.is_nan(b) { Some(self.processed_nan(b, mode)) }
        else { None }
    }

    #[inline(always)]
    fn invalid_product(self, a: u64, b: u64) -> bool {
        (self.abs(a) == self.infinity() && self.abs(b) == 0)
            || (self.abs(b) == self.infinity() && self.abs(a) == 0)
    }

    #[inline(always)]
    fn min_max(self, a: u64, b: u64, mode: FpMode, maximum: bool, number: bool) -> u64 {
        let a = self.flush(a, mode);
        let b = self.flush(b, mode);
        if number && !self.is_snan(a) && !self.is_snan(b) {
            if self.is_nan(a) && !self.is_nan(b) { return b; }
            if self.is_nan(b) && !self.is_nan(a) { return a; }
        }
        if let Some(nan) = self.nan2(a, b, mode) { return nan; }
        // A monotone integer key orders finite floats, infinities, and -0 < +0
        // without letting the host's DAZ change comparisons with subnormals.
        let key = |x: u64| if x & self.sign() != 0 { !x & (self.sign() | (self.sign() - 1)) } else { x | self.sign() };
        if (key(a) >= key(b)) == maximum { a } else { b }
    }

    // Returns a significand and a power of two: value = sig * 2^power.
    #[inline]
    fn parts(self, bits: u64) -> (u128, i32) {
        let exponent = (self.abs(bits) >> self.fraction) as i32;
        let fraction = bits & self.fraction_mask();
        if exponent == 0 {
            (u128::from(fraction), 1 - self.bias - self.fraction as i32)
        } else {
            (u128::from(fraction | self.min_normal()), exponent - self.bias - self.fraction as i32)
        }
    }
}

// The widest alignment is a product at 2^-2148 and an addend with its
// highest bit at 2^1023: 3172 significant bit positions, plus a carry limb.
// This stack-only fallback is used for subnormal/flush-boundary cases.
const LIMBS: usize = 51;

fn put_shifted(words: &mut [u64; LIMBS], value: u128, shift: usize) {
    let word = shift / 64;
    let bit = shift % 64;
    let lo = value as u64;
    let hi = (value >> 64) as u64;
    words[word] |= lo << bit;
    if word + 1 < LIMBS {
        words[word + 1] |= if bit == 0 { hi } else { (hi << bit) | (lo >> (64 - bit)) };
    }
    if bit != 0 && word + 2 < LIMBS { words[word + 2] |= hi >> (64 - bit); }
}

fn rounded(words: &[u64; LIMBS], shift: i32) -> u64 {
    if shift <= 0 { return words[0] << (-shift as u32); }
    let shift = shift as usize;
    let index = shift / 64;
    let bit = shift % 64;
    let low = if index < LIMBS { words[index] >> bit } else { 0 };
    let high = if bit != 0 && index + 1 < LIMBS { words[index + 1] << (64 - bit) } else { 0 };
    let mut result = low | high;
    let guard_index = (shift - 1) / 64;
    let guard_bit = (shift - 1) % 64;
    let guard = guard_index < LIMBS && words[guard_index] & (1_u64 << guard_bit) != 0;
    let sticky = words[..guard_index.min(LIMBS)].iter().any(|&word| word != 0)
        || (guard_index < LIMBS && guard_bit != 0 && words[guard_index] & ((1_u64 << guard_bit) - 1) != 0);
    if guard && (sticky || result & 1 != 0) { result += 1; }
    result
}

/// Exact integer fallback for a finite fused operation, including its one
/// rounding. The format and all operands have already been classified.
#[cold]
#[inline(never)]
fn exact_fma(format: Format, a: u64, b: u64, c: u64, mode: FpMode) -> u64 {
    let (ma, ea) = format.parts(a);
    let (mb, eb) = format.parts(b);
    let (mc, ec) = format.parts(c);
    let product = ma * mb;
    let ep = ea + eb;
    let product_sign = (a ^ b) & format.sign();
    let addend_sign = c & format.sign();
    if product == 0 && mc == 0 {
        return if product_sign == addend_sign { product_sign } else { 0 };
    }
    let base = match (product == 0, mc == 0) {
        (true, false) => ec,
        (false, true) => ep,
        (false, false) => ep.min(ec),
        (true, true) => unreachable!(),
    };
    let mut p = [0_u64; LIMBS];
    let mut q = [0_u64; LIMBS];
    if product != 0 { put_shifted(&mut p, product, (ep - base) as usize); }
    if mc != 0 { put_shifted(&mut q, mc, (ec - base) as usize); }
    let product_bits = if product == 0 { 0 } else { (ep - base) as usize + 128 - product.leading_zeros() as usize };
    let addend_bits = if mc == 0 { 0 } else { (ec - base) as usize + 128 - mc.leading_zeros() as usize };
    let used = (product_bits.max(addend_bits) + 63) / 64 + 1;
    let same_sign = product_sign == addend_sign;
    let (result, other, sign) = if same_sign {
        (&mut p, &q, product_sign)
    } else {
        match p[..used].iter().rev().cmp(q[..used].iter().rev()) {
            std::cmp::Ordering::Equal => return 0,
            std::cmp::Ordering::Less => (&mut q, &p, addend_sign),
            std::cmp::Ordering::Greater => (&mut p, &q, product_sign),
        }
    };
    if same_sign {
        let mut carry = false;
        for (x, &y) in result[..used].iter_mut().zip(&other[..used]) {
            let (sum, c1) = x.overflowing_add(y);
            let (sum, c2) = sum.overflowing_add(u64::from(carry));
            *x = sum;
            carry = c1 || c2;
        }
        debug_assert!(!carry);
    } else {
        let mut borrow = false;
        for (x, &y) in result[..used].iter_mut().zip(&other[..used]) {
            let (difference, b1) = x.overflowing_sub(y);
            let (difference, b2) = difference.overflowing_sub(u64::from(borrow));
            *x = difference;
            borrow = b1 || b2;
        }
        debug_assert!(!borrow);
    }
    let Some(index) = result[..used].iter().rposition(|&word| word != 0) else { return sign; };
    let highest = (index * 64 + 63 - result[index].leading_zeros() as usize) as i32;
    let mut exponent = base + highest;
    let minimum_exponent = 1 - format.bias;
    let maximum_exponent = format.bias;
    if exponent < minimum_exponent {
        if mode.flush_to_zero { return sign; }
        let shift = minimum_exponent - format.fraction as i32 - base;
        return sign | rounded(result, shift);
    }
    if exponent > maximum_exponent { return sign | format.infinity(); }
    let mut significand = rounded(result, highest - format.fraction as i32);
    if significand == 1_u64 << (format.fraction + 1) {
        significand >>= 1;
        exponent += 1;
        if exponent > maximum_exponent { return sign | format.infinity(); }
    }
    sign | (((exponent + format.bias) as u64) << format.fraction) | (significand & format.fraction_mask())
}

macro_rules! arithmetic {
    ($float:ty, $uint:ty, $format:ident, $fma:ident, $fma_fz:ident, $fma_mode:ident,
     $add:ident, $add_fz:ident, $add_mode:ident, $sub:ident, $sub_fz:ident, $sub_mode:ident,
     $mul:ident, $mul_fz:ident, $mul_mode:ident) => {
        /// ARM fused multiply-add in gradual-underflow mode.
        #[inline]
        pub fn $fma(a: $float, b: $float, c: $float) -> $float { $fma_mode(a, b, c, FpMode::IEEE) }
        /// ARM fused multiply-add in flush-to-zero mode.
        #[inline]
        pub fn $fma_fz(a: $float, b: $float, c: $float) -> $float { $fma_mode(a, b, c, FpMode::FZ) }

        /// ARM fused multiply-add, with explicitly selected FZ and DN behavior.
        #[inline]
        pub fn $fma_mode(a: $float, b: $float, c: $float, mode: FpMode) -> $float {
            let a = $format.flush(a.to_bits() as u64, mode);
            let b = $format.flush(b.to_bits() as u64, mode);
            let c = $format.flush(c.to_bits() as u64, mode);
            if let Some(nan) = $format.nan3(a, b, c, mode) { return <$float>::from_bits(nan as $uint); }
            if $format.invalid_product(a, b) { return <$float>::from_bits($format.default_nan() as $uint); }
            let product_sign = (a ^ b) & $format.sign();
            if $format.abs(a) == $format.infinity() || $format.abs(b) == $format.infinity() {
                if $format.abs(c) == $format.infinity() && (c & $format.sign()) != product_sign {
                    return <$float>::from_bits($format.default_nan() as $uint);
                }
                return <$float>::from_bits(($format.infinity() | product_sign) as $uint);
            }
            if $format.abs(c) == $format.infinity() { return <$float>::from_bits(c as $uint); }
            if !mode.flush_to_zero && ($format.is_subnormal(a) || $format.is_subnormal(b) || $format.is_subnormal(c)) {
                return <$float>::from_bits(exact_fma($format, a, b, c, mode) as $uint);
            }
            let result = <$float>::from_bits(a as $uint).mul_add(<$float>::from_bits(b as $uint), <$float>::from_bits(c as $uint));
            let bits = result.to_bits() as u64;
            let magnitude = $format.abs(bits);
            if magnitude <= $format.min_normal() {
                if mode.flush_to_zero {
                    if magnitude < $format.min_normal() { return <$float>::from_bits((bits & $format.sign()) as $uint); }
                    return <$float>::from_bits(exact_fma($format, a, b, c, mode) as $uint);
                }
                // A host FTZ result of zero may hide a representable subnormal.
                if magnitude == 0 && $format.abs(a) != 0 && $format.abs(b) != 0 {
                    return <$float>::from_bits(exact_fma($format, a, b, c, mode) as $uint);
                }
            }
            result
        }

        /// ARM floating addition in gradual-underflow mode.
        #[inline]
        pub fn $add(a: $float, b: $float) -> $float { $add_mode(a, b, FpMode::IEEE) }
        /// ARM floating addition in flush-to-zero mode.
        #[inline]
        pub fn $add_fz(a: $float, b: $float) -> $float { $add_mode(a, b, FpMode::FZ) }
        /// ARM floating addition, with explicit mode.
        #[inline]
        pub fn $add_mode(a: $float, b: $float, mode: FpMode) -> $float {
            if let Some(nan) = $format.nan2(a.to_bits() as u64, b.to_bits() as u64, mode) {
                return <$float>::from_bits(nan as $uint);
            }
            $fma_mode(a, 1.0, b, mode)
        }
        /// ARM floating subtraction in gradual-underflow mode.
        #[inline]
        pub fn $sub(a: $float, b: $float) -> $float { $sub_mode(a, b, FpMode::IEEE) }
        /// ARM floating subtraction in flush-to-zero mode.
        #[inline]
        pub fn $sub_fz(a: $float, b: $float) -> $float { $sub_mode(a, b, FpMode::FZ) }
        /// ARM floating subtraction. NaN selection precedes negating operand two.
        #[inline]
        pub fn $sub_mode(a: $float, b: $float, mode: FpMode) -> $float {
            if let Some(nan) = $format.nan2(a.to_bits() as u64, b.to_bits() as u64, mode) {
                return <$float>::from_bits(nan as $uint);
            }
            $fma_mode(a, 1.0, <$float>::from_bits(b.to_bits() ^ $format.sign() as $uint), mode)
        }
        /// ARM floating multiplication in gradual-underflow mode.
        #[inline]
        pub fn $mul(a: $float, b: $float) -> $float { $mul_mode(a, b, FpMode::IEEE) }
        /// ARM floating multiplication in flush-to-zero mode.
        #[inline]
        pub fn $mul_fz(a: $float, b: $float) -> $float { $mul_mode(a, b, FpMode::FZ) }
        /// ARM floating multiplication, with explicit mode.
        #[inline]
        pub fn $mul_mode(a: $float, b: $float, mode: FpMode) -> $float {
            if let Some(nan) = $format.nan2(a.to_bits() as u64, b.to_bits() as u64, mode) {
                return <$float>::from_bits(nan as $uint);
            }
            let zero = <$float>::from_bits((a.to_bits() ^ b.to_bits()) & $format.sign() as $uint);
            $fma_mode(a, b, zero, mode)
        }
    };
}

arithmetic!(f32, u32, SINGLE, fmadd_f32, fmadd_f32_fz, fmadd_f32_with_mode,
    fadd_f32, fadd_f32_fz, fadd_f32_with_mode, fsub_f32, fsub_f32_fz, fsub_f32_with_mode,
    fmul_f32, fmul_f32_fz, fmul_f32_with_mode);
arithmetic!(f64, u64, DOUBLE, fmadd_f64, fmadd_f64_fz, fmadd_f64_with_mode,
    fadd_f64, fadd_f64_fz, fadd_f64_with_mode, fsub_f64, fsub_f64_fz, fsub_f64_with_mode,
    fmul_f64, fmul_f64_fz, fmul_f64_with_mode);

/// Scalar FMAXNM, including signaling NaNs and the signed-zero order.
#[inline]
pub fn max_number(a: f32, b: f32) -> f32 { max_number_with_mode(a, b, FpMode::IEEE) }
/// Scalar FMAXNM in flush-to-zero mode.
#[inline]
pub fn max_number_fz(a: f32, b: f32) -> f32 { max_number_with_mode(a, b, FpMode::FZ) }
/// Scalar FMAXNM with explicit FZ and DN.
#[inline]
pub fn max_number_with_mode(a: f32, b: f32, mode: FpMode) -> f32 {
    f32::from_bits(SINGLE.min_max(u64::from(a.to_bits()), u64::from(b.to_bits()), mode, true, true) as u32)
}
/// Scalar FMINNM.
#[inline]
pub fn min_number(a: f32, b: f32) -> f32 { min_number_with_mode(a, b, FpMode::IEEE) }
/// Scalar FMINNM in flush-to-zero mode.
#[inline]
pub fn min_number_fz(a: f32, b: f32) -> f32 { min_number_with_mode(a, b, FpMode::FZ) }
/// Scalar FMINNM with explicit FZ and DN.
#[inline]
pub fn min_number_with_mode(a: f32, b: f32, mode: FpMode) -> f32 {
    f32::from_bits(SINGLE.min_max(u64::from(a.to_bits()), u64::from(b.to_bits()), mode, false, true) as u32)
}
/// Scalar FMAX, which propagates quiet NaNs as well as signaling NaNs.
#[inline]
pub fn max_propagating(a: f32, b: f32) -> f32 { max_propagating_with_mode(a, b, FpMode::IEEE) }
/// Scalar FMAX in flush-to-zero mode.
#[inline]
pub fn max_propagating_fz(a: f32, b: f32) -> f32 { max_propagating_with_mode(a, b, FpMode::FZ) }
/// Scalar FMAX with explicit FZ and DN.
#[inline]
pub fn max_propagating_with_mode(a: f32, b: f32, mode: FpMode) -> f32 {
    f32::from_bits(SINGLE.min_max(u64::from(a.to_bits()), u64::from(b.to_bits()), mode, true, false) as u32)
}
/// Scalar FMIN, which propagates quiet NaNs as well as signaling NaNs.
#[inline]
pub fn min_propagating(a: f32, b: f32) -> f32 { min_propagating_with_mode(a, b, FpMode::IEEE) }
/// Scalar FMIN in flush-to-zero mode.
#[inline]
pub fn min_propagating_fz(a: f32, b: f32) -> f32 { min_propagating_with_mode(a, b, FpMode::FZ) }
/// Scalar FMIN with explicit FZ and DN.
#[inline]
pub fn min_propagating_with_mode(a: f32, b: f32, mode: FpMode) -> f32 {
    f32::from_bits(SINGLE.min_max(u64::from(a.to_bits()), u64::from(b.to_bits()), mode, false, false) as u32)
}

/// ARM FCMP followed by the LT condition. Unordered input also selects LT.
#[inline]
pub fn arm_lt(a: f32, b: f32) -> bool { arm_lt_with_mode(a, b, FpMode::IEEE) }
/// ARM FCMP/LT in flush-to-zero mode.
#[inline]
pub fn arm_lt_fz(a: f32, b: f32) -> bool { arm_lt_with_mode(a, b, FpMode::FZ) }
/// ARM FCMP/LT with explicit mode. No floating comparison is used, so the
/// host's DAZ setting cannot change the selected condition.
#[inline]
pub fn arm_lt_with_mode(a: f32, b: f32, mode: FpMode) -> bool {
    let a = SINGLE.flush(u64::from(a.to_bits()), mode) as u32;
    let b = SINGLE.flush(u64::from(b.to_bits()), mode) as u32;
    if SINGLE.is_nan(u64::from(a)) || SINGLE.is_nan(u64::from(b)) { return true; }
    if (a & 0x7fff_ffff) == 0 && (b & 0x7fff_ffff) == 0 { return false; }
    let key = |x: u32| if x & 0x8000_0000 != 0 { !x } else { x | 0x8000_0000 };
    key(a) < key(b)
}

/// The six ordered f64 instructions in the phase section's sample method.
#[inline]
pub fn phase_sample(a1: f64, a2: f64, x: f64, negative_a1: f64, negative_a2: f64,
                    state0: &mut f64, state1: &mut f64) -> f64 {
    phase_sample_with_mode(a1, a2, x, negative_a1, negative_a2, state0, state1, FpMode::IEEE)
}
/// The phase sample sequence in flush-to-zero mode.
#[inline]
pub fn phase_sample_fz(a1: f64, a2: f64, x: f64, negative_a1: f64, negative_a2: f64,
                       state0: &mut f64, state1: &mut f64) -> f64 {
    phase_sample_with_mode(a1, a2, x, negative_a1, negative_a2, state0, state1, FpMode::FZ)
}
/// The phase sample sequence with an explicitly selected mode.
#[inline]
pub fn phase_sample_with_mode(a1: f64, a2: f64, x: f64, negative_a1: f64, negative_a2: f64,
                              state0: &mut f64, state1: &mut f64, mode: FpMode) -> f64 {
    let y = fmadd_f64_with_mode(a2, x, *state0, mode);
    let temporary = fmul_f64_with_mode(y, negative_a1, mode);
    let temporary = fmadd_f64_with_mode(a1, x, temporary, mode);
    *state0 = fadd_f64_with_mode(*state1, temporary, mode);
    let temporary = fmul_f64_with_mode(y, negative_a2, mode);
    *state1 = fmadd_f64_with_mode(1.0, x, temporary, mode);
    y
}

/// NEON FABS is a bitwise sign clear; it neither flushes nor quiets NaNs.
#[inline]
pub fn vabs_f32(a: [f32; 2]) -> [f32; 2] { a.map(|x| f32::from_bits(x.to_bits() & 0x7fff_ffff)) }
#[inline]
pub fn vdup_n_f32(value: f32) -> [f32; 2] { [value; 2] }
#[inline]
pub fn vdupq_n_f32(value: f32) -> [f32; 4] { [value; 4] }
#[inline]
pub fn vdupq_laneq_f32<const LANE: i32>(value: [f32; 4]) -> [f32; 4] { [value[LANE as usize]; 4] }
#[inline]
pub fn vget_lane_f32<const LANE: i32>(value: [f32; 2]) -> f32 { value[LANE as usize] }
#[inline]
pub fn vgetq_lane_f32<const LANE: i32>(value: [f32; 4]) -> f32 { value[LANE as usize] }
#[inline]
pub fn vsetq_lane_f32<const LANE: i32>(value: f32, mut vector: [f32; 4]) -> [f32; 4] {
    vector[LANE as usize] = value;
    vector
}
/// Load four consecutive f32 values, like NEON VLD1.
///
/// # Safety
/// `pointer` must be valid and initialized for 16 bytes. Alignment is not
/// required, matching the hardware load.
#[inline]
pub unsafe fn vld1q_f32(pointer: *const f32) -> [f32; 4] {
    unsafe { pointer.cast::<[f32; 4]>().read_unaligned() }
}

macro_rules! lanes {
    ($name:ident, $fz:ident, $with:ident, $scalar:ident, $n:expr) => {
        #[inline]
        pub fn $name(a: [f32; $n], b: [f32; $n]) -> [f32; $n] { $with(a, b, FpMode::IEEE) }
        #[inline]
        pub fn $fz(a: [f32; $n], b: [f32; $n]) -> [f32; $n] { $with(a, b, FpMode::FZ) }
        #[inline]
        pub fn $with(a: [f32; $n], b: [f32; $n], mode: FpMode) -> [f32; $n] {
            std::array::from_fn(|i| $scalar(a[i], b[i], mode))
        }
    };
}
lanes!(vmaxnm_f32, vmaxnm_f32_fz, vmaxnm_f32_with_mode, max_number_with_mode, 2);
lanes!(vminnm_f32, vminnm_f32_fz, vminnm_f32_with_mode, min_number_with_mode, 2);
lanes!(vmaxq_f32, vmaxq_f32_fz, vmaxq_f32_with_mode, max_propagating_with_mode, 4);
lanes!(vsubq_f32, vsubq_f32_fz, vsubq_f32_with_mode, fsub_f32_with_mode, 4);

/// NEON FMLA: each accumulator lane is the addend, not the first multiplicand.
#[inline]
pub fn vfmaq_f32(accumulator: [f32; 4], a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    vfmaq_f32_with_mode(accumulator, a, b, FpMode::IEEE)
}
/// NEON FMLA in flush-to-zero mode.
#[inline]
pub fn vfmaq_f32_fz(accumulator: [f32; 4], a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    vfmaq_f32_with_mode(accumulator, a, b, FpMode::FZ)
}
/// NEON FMLA with explicit mode.
#[inline]
pub fn vfmaq_f32_with_mode(accumulator: [f32; 4], a: [f32; 4], b: [f32; 4], mode: FpMode) -> [f32; 4] {
    std::array::from_fn(|i| fmadd_f32_with_mode(a[i], b[i], accumulator[i], mode))
}

// Estimate and Newton-step instructions. The 8-bit estimates come from the
// ARM ARM's `RecipEstimate` and `RecipSqrtEstimate`, evaluated in integer
// arithmetic, so nothing here depends on the host's estimate instructions.

/// `RecipEstimate(a)` for the 9-bit argument `a` in 256..=511: result in 256..=511.
const fn recip_estimates() -> [u16; 512] {
    let mut table = [0_u16; 512];
    let mut a = 256_u32;
    while a < 512 {
        table[a as usize] = ((((1_u32 << 19) / (2 * a + 1)) + 1) / 2) as u16;
        a += 1;
    }
    table
}

/// `RecipSqrtEstimate(a)` for the 9-bit argument `a` in 128..=511: result in 256..=511.
const fn rsqrt_estimates() -> [u16; 512] {
    let mut table = [0_u16; 512];
    let mut a = 128_u32;
    while a < 512 {
        let scaled = (if a < 256 { 2 * a + 1 } else { 2 * (((a >> 1) << 1) + 1) }) as u64;
        let mut b = 512_u64;
        while scaled * (b + 1) * (b + 1) < (1_u64 << 28) { b += 1; }
        table[a as usize] = ((b + 1) / 2) as u16;
        a += 1;
    }
    table
}

static RECIP_ESTIMATE: [u16; 512] = recip_estimates();
static RSQRT_ESTIMATE: [u16; 512] = rsqrt_estimates();

/// Scalar FRECPE: the reciprocal estimate with 8 fraction bits.
#[inline]
pub fn frecpe_f32(x: f32) -> f32 { frecpe_f32_with_mode(x, FpMode::IEEE) }
/// Scalar FRECPE in flush-to-zero mode.
#[inline]
pub fn frecpe_f32_fz(x: f32) -> f32 { frecpe_f32_with_mode(x, FpMode::FZ) }
/// Scalar FRECPE with explicit FZ and DN. A subnormal operand is zero in FZ
/// mode, and an operand of 2^126 or more gives zero rather than a subnormal.
#[inline]
pub fn frecpe_f32_with_mode(x: f32, mode: FpMode) -> f32 {
    let bits = SINGLE.flush(u64::from(x.to_bits()), mode) as u32;
    let sign = bits & 0x8000_0000;
    // A normal operand whose reciprocal is normal as well takes the table directly.
    let biased = (bits >> 23) & 0xff;
    if biased.wrapping_sub(1) < 252 {
        let estimate = u32::from(RECIP_ESTIMATE[(0x100 | ((bits >> 15) & 0xff)) as usize]) & 0xff;
        return f32::from_bits(sign | ((253 - biased) << 23) | (estimate << 15));
    }
    let mut exponent = ((bits >> 23) & 0xff) as i32;
    let mut fraction = bits & 0x007f_ffff;
    if exponent == 0xff {
        return f32::from_bits(if fraction != 0 { SINGLE.processed_nan(u64::from(bits), mode) as u32 } else { sign });
    }
    // Zero, or a magnitude below 2^-128: the reciprocal overflows.
    if exponent == 0 && fraction < 0x0020_0000 { return f32::from_bits(sign | 0x7f80_0000); }
    if mode.flush_to_zero && exponent >= 253 { return f32::from_bits(sign); }
    if exponent == 0 {
        if fraction & 0x0040_0000 == 0 {
            exponent = -1;
            fraction = (fraction << 2) & 0x007f_ffff;
        } else {
            fraction = (fraction << 1) & 0x007f_ffff;
        }
    }
    let estimate = u32::from(RECIP_ESTIMATE[(0x100 | (fraction >> 15)) as usize]) & 0xff;
    let mut result_exponent = 253 - exponent;
    let mut result_fraction = estimate << 15;
    if result_exponent == 0 {
        result_fraction = 0x0040_0000 | (result_fraction >> 1);
    } else if result_exponent == -1 {
        result_fraction = 0x0020_0000 | (result_fraction >> 2);
        result_exponent = 0;
    }
    f32::from_bits(sign | ((result_exponent as u32) << 23) | result_fraction)
}

/// Scalar FRSQRTE: the reciprocal square root estimate with 8 fraction bits.
#[inline]
pub fn frsqrte_f32(x: f32) -> f32 { frsqrte_f32_with_mode(x, FpMode::IEEE) }
/// Scalar FRSQRTE in flush-to-zero mode.
#[inline]
pub fn frsqrte_f32_fz(x: f32) -> f32 { frsqrte_f32_with_mode(x, FpMode::FZ) }
/// Scalar FRSQRTE with explicit FZ and DN. Negative operands, including
/// negative infinity, give the default NaN whatever DN says.
#[inline]
pub fn frsqrte_f32_with_mode(x: f32, mode: FpMode) -> f32 {
    let bits = SINGLE.flush(u64::from(x.to_bits()), mode) as u32;
    let sign = bits & 0x8000_0000;
    // A positive normal operand takes the table directly.
    let biased = (bits >> 23) & 0xff;
    if sign == 0 && biased.wrapping_sub(1) < 254 {
        let scaled = if biased & 1 == 0 { 0x100 | ((bits >> 15) & 0xff) } else { 0x80 | ((bits >> 16) & 0x7f) };
        let estimate = u32::from(RSQRT_ESTIMATE[scaled as usize]) & 0xff;
        return f32::from_bits((((380 - biased) / 2) << 23) | (estimate << 15));
    }
    let mut exponent = ((bits >> 23) & 0xff) as i32;
    let fraction = bits & 0x007f_ffff;
    if exponent == 0xff && fraction != 0 {
        return f32::from_bits(SINGLE.processed_nan(u64::from(bits), mode) as u32);
    }
    if exponent == 0 && fraction == 0 { return f32::from_bits(sign | 0x7f80_0000); }
    if sign != 0 { return f32::from_bits(SINGLE.default_nan() as u32); }
    if exponent == 0xff { return 0.0; }
    // The fraction as a 52-bit register; a subnormal operand is normalised.
    const MASK: u64 = (1 << 52) - 1;
    let mut fraction = u64::from(fraction) << 29;
    if exponent == 0 {
        while fraction & (1 << 51) == 0 {
            fraction = (fraction << 1) & MASK;
            exponent -= 1;
        }
        fraction = (fraction << 1) & MASK;
    }
    let scaled = if exponent & 1 == 0 { 0x100 | (fraction >> 44) } else { 0x80 | (fraction >> 45) };
    let estimate = u32::from(RSQRT_ESTIMATE[scaled as usize]) & 0xff;
    f32::from_bits((((380 - exponent) / 2) as u32) << 23 | (estimate << 15))
}

#[inline]
fn is_zero_or_infinity(bits: u32) -> (bool, bool) {
    (bits & 0x7fff_ffff == 0, bits & 0x7fff_ffff == 0x7f80_0000)
}

/// Scalar FRECPS: the Newton step `2 - a * b` with a single rounding.
#[inline]
pub fn frecps_f32(a: f32, b: f32) -> f32 { frecps_f32_with_mode(a, b, FpMode::IEEE) }
/// Scalar FRECPS in flush-to-zero mode.
#[inline]
pub fn frecps_f32_fz(a: f32, b: f32) -> f32 { frecps_f32_with_mode(a, b, FpMode::FZ) }
/// Scalar FRECPS with explicit FZ and DN. The first operand is negated before
/// NaN selection, so a NaN result carries the negated sign of a NaN `a`. An
/// infinity times a zero gives exactly 2.0 rather than a NaN.
#[inline]
pub fn frecps_f32_with_mode(a: f32, b: f32, mode: FpMode) -> f32 {
    let a = SINGLE.flush(u64::from(a.to_bits()), mode) as u32 ^ 0x8000_0000;
    let b = SINGLE.flush(u64::from(b.to_bits()), mode) as u32;
    let ((a_zero, a_inf), (b_zero, b_inf)) = (is_zero_or_infinity(a), is_zero_or_infinity(b));
    if (a_inf && b_zero) || (a_zero && b_inf) { return 2.0; }
    fmadd_f32_with_mode(f32::from_bits(a), f32::from_bits(b), 2.0, mode)
}

/// Scalar FRSQRTS: the Newton step `(3 - a * b) / 2` with a single rounding.
#[inline]
pub fn frsqrts_f32(a: f32, b: f32) -> f32 { frsqrts_f32_with_mode(a, b, FpMode::IEEE) }
/// Scalar FRSQRTS in flush-to-zero mode.
#[inline]
pub fn frsqrts_f32_fz(a: f32, b: f32) -> f32 { frsqrts_f32_with_mode(a, b, FpMode::FZ) }
/// Scalar FRSQRTS with explicit FZ and DN. NaN selection and the infinity-times-zero
/// result (exactly 1.5) are those of [`frecps_f32_with_mode`]; an exact zero is +0.
#[inline]
pub fn frsqrts_f32_with_mode(a: f32, b: f32, mode: FpMode) -> f32 {
    let a = SINGLE.flush(u64::from(a.to_bits()), mode) as u32 ^ 0x8000_0000;
    let b = SINGLE.flush(u64::from(b.to_bits()), mode) as u32;
    let ((a_zero, a_inf), (b_zero, b_inf)) = (is_zero_or_infinity(a), is_zero_or_infinity(b));
    if (a_inf && b_zero) || (a_zero && b_inf) { return 1.5; }
    let (a, b) = (f32::from_bits(a), f32::from_bits(b));
    let result = if SINGLE.is_nan(u64::from(a.to_bits())) || SINGLE.is_nan(u64::from(b.to_bits())) || a_inf || b_inf {
        // The NaN and infinity results do not depend on the constant.
        fmadd_f32_with_mode(a, b, 3.0, mode)
    } else if a.to_bits() & 0x7fff_ffff >= 0x0100_0000 {
        // Halving `a` is exact from 2^-125 up, and keeps (3 + a*b) / 2 from overflowing.
        fmadd_f32_with_mode(f32::from_bits(a.to_bits() - 0x0080_0000), b, 1.5, mode)
    } else {
        // |a| < 2^-125 and |b| <= 2^128: the product is small, and the sum is exact to halve.
        fmadd_f32_with_mode(a, b, 3.0, mode) * 0.5
    };
    if result == 0.0 { 0.0 } else { result }
}
