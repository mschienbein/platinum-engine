//! Bit-exact float32 helpers shared by the quantiser stages.
//!
//! Everything here is host independent (same bits on aarch64 and x86_64): fused operations use `f32::mul_add` (exactly rounded
//! single-step `fma`) and conversions follow the ARM rounding of the instruction Apple's code uses (`fcvtzs/fcvtzu` truncate,
//! `fcvtps` ceil, `fcvtms` floor; Rust `as` casts saturate and map NaN to 0 exactly like the ARM instructions). The estimate
//! instructions `FRECPE`/`FRSQRTE` are `crate::portable::arm::{frecpe_f32, frsqrte_f32}`, the `dB()` approximation is
//! [`crate::portable::aac::psy::db`] and `vDSP_maxvi` is [`crate::portable::vdsp::maxvi`].
//!
//! FZ behaviour: no function reads or writes FPCR/MXCSR. Results equal Apple's default-FPCR results; with FZ set on the host they can
//! differ only when a subnormal operand/result occurs.

/// ARM `FMADD`/`FMLA` (single rounding).  `f32::mul_add` is the idiom of the portable tree; `crate::portable::arm::fmadd_f32` returns the same
/// bits for every operand but a NaN (which the encoder never reaches) at about 2.5 times the cost per operation.
#[inline(always)]
pub fn fma(a: f32, b: f32, c: f32) -> f32 {
    a.mul_add(b, c)
}

/// ARM `FCVTZS` (truncate toward zero, saturating, NaN -> 0).
#[inline(always)]
pub fn fcvtzs(x: f32) -> i32 {
    x as i32
}

/// ARM `FCVTPS` (round toward +infinity, saturating, NaN -> 0).
#[inline(always)]
pub fn fcvtps(x: f32) -> i32 {
    x.ceil() as i32
}

/// ARM `FCVTMS` (round toward -infinity, saturating, NaN -> 0).
#[inline(always)]
pub fn fcvtms(x: f32) -> i32 {
    x.floor() as i32
}

/// 7.6943736f = 16/(3 ln 2) (`DAT_0092267c`).
pub const LN_GAIN_SCALE: f32 = f32::from_bits(0x40f6_384f);
/// -69.3324f (`DAT_00922678`).
pub const LN_GAIN_BIAS: f32 = f32::from_bits(0xc28a_aa30);
/// +69.3324f (`DAT_00922730`).
pub const LN_GAIN_BIAS_POS: f32 = f32::from_bits(0x428a_aa30);
/// quantiser rounding offset 0.4054f (`DAT_00922680`).
pub const QUANT_OFFSET: f32 = f32::from_bits(0x3ecf_9097);

