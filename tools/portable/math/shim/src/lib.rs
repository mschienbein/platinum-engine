//! Explicit C ABI for the portable math used by the pinned codec builds.
//!
//! The codec must opt into each `pn_codec_*` symbol at its call site. This
//! library does not interpose libc and does not alter the floating-point mode.
#[allow(dead_code)]
#[path = "../../../../../src/portable/math.rs"]
mod math;

macro_rules! unary {
    ($name:ident, $inner:ident, $ty:ty) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(value: $ty) -> $ty { math::$inner(value) }
    };
}
macro_rules! binary {
    ($name:ident, $inner:ident, $ty:ty) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(x: $ty, y: $ty) -> $ty { math::$inner(x, y) }
    };
}

unary!(pn_codec_expf, expf, f32);
unary!(pn_codec_exp2f, exp2f, f32);
unary!(pn_codec_exp10f, exp10f, f32);
unary!(pn_codec_logf, logf, f32);
unary!(pn_codec_log2f, log2f, f32);
unary!(pn_codec_log10f, log10f, f32);
unary!(pn_codec_sinf, sinf, f32);
unary!(pn_codec_cosf, cosf, f32);
unary!(pn_codec_floorf, floorf, f32);
unary!(pn_codec_ceilf, ceilf, f32);
unary!(pn_codec_roundf, roundf, f32);
unary!(pn_codec_truncf, truncf, f32);
unary!(pn_codec_sqrtf, sqrtf, f32);
binary!(pn_codec_atan2f, atan2f, f32);
binary!(pn_codec_fmodf, fmodf, f32);
binary!(pn_codec_powf, powf, f32);

unary!(pn_codec_log, log, f64);
unary!(pn_codec_log10, log10, f64);
unary!(pn_codec_exp, exp, f64);
binary!(pn_codec_pow, pow, f64);
unary!(pn_codec_exp2, exp2, f64);
unary!(pn_codec_exp10, exp10, f64);
unary!(pn_codec_sin, sin, f64);
unary!(pn_codec_cos, cos, f64);
unary!(pn_codec_tan, tan, f64);
unary!(pn_codec_atan, atan, f64);
unary!(pn_codec_logb, logb, f64);
unary!(pn_codec_sinh, sinh, f64);
unary!(pn_codec_cosh, cosh, f64);
unary!(pn_codec_tanh, tanh, f64);
unary!(pn_codec_asinh, asinh, f64);
unary!(pn_codec_log1p, log1p, f64);
unary!(pn_codec_floor, floor, f64);
unary!(pn_codec_ceil, ceil, f64);
unary!(pn_codec_round, round, f64);
unary!(pn_codec_trunc, trunc, f64);
unary!(pn_codec_sqrt, sqrt, f64);

#[unsafe(no_mangle)]
pub extern "C" fn pn_codec_lround(value: f64) -> std::ffi::c_long {
    math::lround(value) as std::ffi::c_long
}

#[unsafe(no_mangle)]
pub extern "C" fn pn_codec_scalbn(value: f64, exponent: i32) -> f64 {
    math::scalbn(value, exponent)
}

#[unsafe(no_mangle)]
pub extern "C" fn pn_codec_powi_f64(value: f64, exponent: i32) -> f64 {
    math::powi_f64(value, exponent)
}

/// # Safety
/// `exponent` must point to a writable C `int`, as for the C `frexp` API.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pn_codec_frexp(value: f64, exponent: *mut i32) -> f64 {
    let (fraction, power) = math::frexp(value);
    unsafe { exponent.write(power) };
    fraction
}
