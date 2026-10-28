//! The reference uses macOS libSystem math, not interchangeable approximations.

#[cfg(all(target_os = "macos", not(feature = "portable")))]
mod native {
unsafe extern "C" {
    fn __exp10(value: f64) -> f64;
    #[link_name = "exp2"]
    fn native_exp2(value: f64) -> f64;
    #[link_name = "log"]
    fn native_log(value: f64) -> f64;
    #[link_name = "tan"]
    fn native_tan(value: f64) -> f64;
    #[link_name = "expf"]
    fn native_expf(value: f32) -> f32;
    #[link_name = "exp2f"]
    fn native_exp2f(value: f32) -> f32;
    #[link_name = "logf"]
    fn native_logf(value: f32) -> f32;
    #[link_name = "__powidf2"]
    fn native_powi(value: f64, exponent: i32) -> f64;
}

#[inline]
pub(crate) fn exp10(value: f64) -> f64 {
    // libSystem accepts all f64 inputs and has no pointer preconditions.
    unsafe { __exp10(value) }
}

#[inline]
pub(crate) fn exp2(value: f64) -> f64 {
    // Use the same host primitive as the original release/envelope calculation.
    unsafe { native_exp2(value) }
}

#[inline]
pub(crate) fn ln(value: f64) -> f64 {
    // Match the reference's natural-log call rather than folding log10.
    unsafe { native_log(value) }
}

#[inline]
pub(crate) fn tan(value: f64) -> f64 {
    // The K-weighting coefficient builder calls this host function.
    unsafe { native_tan(value) }
}

#[inline]
pub(crate) fn expf(value: f32) -> f32 {
    // The limiter calls the binary32 primitive directly, without widening.
    unsafe { native_expf(value) }
}

#[inline]
pub(crate) fn exp2f(value: f32) -> f32 {
    unsafe { native_exp2f(value) }
}

#[inline]
pub(crate) fn logf(value: f32) -> f32 {
    unsafe { native_logf(value) }
}

#[inline]
pub(crate) fn powi_f64(value: f64, exponent: i32) -> f64 {
    // HeartCore imports this compiler-runtime primitive from libSystem.
    unsafe { native_powi(value, exponent) }
}
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
pub(crate) use native::{exp10, exp2, exp2f, expf, ln, logf, powi_f64, tan};
#[cfg(any(target_os = "linux", feature = "portable"))]
pub(crate) use crate::portable::math::{exp10, exp2, exp2f, expf, ln, logf, powi_f64, tan};
