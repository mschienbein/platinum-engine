//! ARM scalar floating-point value semantics for the engine's Rust methods.
//!
//! The explicit `fmadd_*`, `fminnm_*`, and `fmaxnm_*` functions model ARM
//! instructions with FPCR.DN clear, round-to-nearest ties-to-even, and the
//! requested FZ mode. Exception flags and traps are not reproduced. The FMA
//! implementation is shared with the NEON-style helpers in `arm`.
//!
//! Rust's `min`/`max` are *not* identical to FMINNM/FMAXNM for signaling NaNs:
//! the Rust method returns the numeric operand when exactly one input is NaN.
//! [`ArmFloat`] emulates the scalar Mac Rust operations on x86. On AArch64 its
//! methods are ordinary Rust float operations, so the compiler and current
//! FPCR retain their original behavior. LLVM may reorder FMA multiplicands in
//! vectorized code; if multiple NaNs carry different payloads, that can change
//! the selected payload relative to the scalar ARM FMADD modeled on x86.

pub use super::arm::{fmadd_f32, fmadd_f32_fz, fmadd_f64, fmadd_f64_fz};
pub use super::arm::{min_number as fminnm_f32, min_number_fz as fminnm_f32_fz};
pub use super::arm::{max_number as fmaxnm_f32, max_number_fz as fmaxnm_f32_fz};

#[inline]
fn number64(a: f64, b: f64, fz: bool, maximum: bool) -> f64 {
    const SIGN: u64 = 1 << 63;
    const MIN_NORMAL: u64 = 1 << 52;
    const INF: u64 = 0x7ff0_0000_0000_0000;
    const QUIET: u64 = 1 << 51;
    let mut a = a.to_bits();
    let mut b = b.to_bits();
    let mut aa = a & !SIGN;
    let mut bb = b & !SIGN;
    if fz {
        if aa < MIN_NORMAL { a &= SIGN; aa = 0; }
        if bb < MIN_NORMAL { b &= SIGN; bb = 0; }
    }
    let a_nan = aa > INF;
    let b_nan = bb > INF;
    let a_signaling = a_nan && a & QUIET == 0;
    let b_signaling = b_nan && b & QUIET == 0;
    let result = if a_signaling { a | QUIET }
        else if b_signaling { b | QUIET }
        else if a_nan && b_nan { a | QUIET }
        else if a_nan { b }
        else if b_nan { a }
        else {
            let key = |x: u64| if x & SIGN != 0 { !x } else { x | SIGN };
            if (key(a) >= key(b)) == maximum { a } else { b }
        };
    f64::from_bits(result)
}

/// Scalar FMINNM, with gradual underflow.
#[inline]
pub fn fminnm_f64(a: f64, b: f64) -> f64 { number64(a, b, false, false) }
/// Scalar FMINNM, with flush-to-zero.
#[inline]
pub fn fminnm_f64_fz(a: f64, b: f64) -> f64 { number64(a, b, true, false) }
/// Scalar FMAXNM, with gradual underflow.
#[inline]
pub fn fmaxnm_f64(a: f64, b: f64) -> f64 { number64(a, b, false, true) }
/// Scalar FMAXNM, with flush-to-zero.
#[inline]
pub fn fmaxnm_f64_fz(a: f64, b: f64) -> f64 { number64(a, b, true, true) }

// The engine sets and restores FTZ and DAZ together. A partial MXCSR setting
// has no corresponding ARM FZ mode and is deliberately not treated as FZ.
#[cfg(target_arch = "x86_64")]
#[inline]
fn current_fz() -> bool {
    let mut control = 0_u32;
    unsafe {
        std::arch::asm!("stmxcsr [{p}]", p = in(reg) &mut control, options(nostack, preserves_flags));
    }
    control & ((1 << 15) | (1 << 6)) == ((1 << 15) | (1 << 6))
}

/// Float operations modeled on the Mac ARM scalar Rust methods.
///
/// On x86 the current MXCSR FTZ+DAZ setting chooses the ARM FZ variant. The
/// engine's `with_flush_to_zero` scope sets both bits together. Other rounding
/// modes, FPCR.DN, partial x86 flush settings, exception flags, and traps are
/// outside this value-semantics contract.
pub trait ArmFloat: Sized {
    /// Fused `self * multiplier + addend`.
    fn arm_mul_add(self, multiplier: Self, addend: Self) -> Self;
    /// Mac Rust `min`, including its NaN-selection behavior.
    fn arm_min(self, other: Self) -> Self;
    /// Mac Rust `max`, including its NaN-selection behavior.
    fn arm_max(self, other: Self) -> Self;
}

macro_rules! float_impl {
    ($float:ty, $fma:ident, $fma_fz:ident, $rust_number:ident) => {
        impl ArmFloat for $float {
            #[inline(always)]
            fn arm_mul_add(self, multiplier: Self, addend: Self) -> Self {
                #[cfg(target_arch = "aarch64")]
                { self.mul_add(multiplier, addend) }
                #[cfg(target_arch = "x86_64")]
                { if current_fz() { $fma_fz(self, multiplier, addend) } else { $fma(self, multiplier, addend) } }
            }
            #[inline(always)]
            fn arm_min(self, other: Self) -> Self {
                #[cfg(target_arch = "aarch64")]
                { self.min(other) }
                #[cfg(target_arch = "x86_64")]
                { $rust_number(self, other, current_fz(), false) }
            }
            #[inline(always)]
            fn arm_max(self, other: Self) -> Self {
                #[cfg(target_arch = "aarch64")]
                { self.max(other) }
                #[cfg(target_arch = "x86_64")]
                { $rust_number(self, other, current_fz(), true) }
            }
        }
    };
}

// Implemented separately by width because Rust has no generic float bit API.
macro_rules! rust_number_impl {
    ($name:ident, $float:ty, $uint:ty, $inf:expr, $quiet:expr, $minimum:expr, $sign:expr,
     $min:ident, $min_fz:ident, $max:ident, $max_fz:ident) => {
        #[cfg(target_arch = "x86_64")]
        #[inline]
        fn $name(a: $float, b: $float, fz: bool, maximum: bool) -> $float {
            let ab: $uint = a.to_bits();
            let bb: $uint = b.to_bits();
            let an = (ab & !$sign) > $inf;
            let bn = (bb & !$sign) > $inf;
            if an && bn { return <$float>::from_bits(ab | $quiet); }
            if an || bn {
                let bits = if an { bb } else { ab };
                return <$float>::from_bits(if fz && (bits & !$sign) < $minimum { bits & $sign } else { bits });
            }
            match (maximum, fz) {
                (false, false) => $min(a, b), (false, true) => $min_fz(a, b),
                (true, false) => $max(a, b), (true, true) => $max_fz(a, b),
            }
        }
    };
}
rust_number_impl!(rust_number32, f32, u32, 0x7f80_0000_u32, 0x0040_0000_u32, 0x0080_0000_u32, 0x8000_0000_u32,
    fminnm_f32, fminnm_f32_fz, fmaxnm_f32, fmaxnm_f32_fz);
rust_number_impl!(rust_number64, f64, u64, 0x7ff0_0000_0000_0000_u64, 0x0008_0000_0000_0000_u64, 0x0010_0000_0000_0000_u64, 0x8000_0000_0000_0000_u64,
    fminnm_f64, fminnm_f64_fz, fmaxnm_f64, fmaxnm_f64_fz);

float_impl!(f32, fmadd_f32, fmadd_f32_fz, rust_number32);
float_impl!(f64, fmadd_f64, fmadd_f64_fz, rust_number64);
