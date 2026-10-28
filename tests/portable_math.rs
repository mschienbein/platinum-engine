//! Fast bitwise regressions for the portable math primitives. Exhaustive and
//! billion-sample receipts are produced by tools/portable/math/parity.rs.
use platinum_engine::portable::math;

#[test]
fn fixed_cross_platform_regressions() {
    assert_eq!(math::sinf(f32::INFINITY).to_bits(), 0x7fc0_0000);
    assert_eq!(math::cosf(f32::NEG_INFINITY).to_bits(), 0x7fc0_0000);
    assert_eq!(math::sin(f64::INFINITY).to_bits(), 0x7ff8_0000_0000_0000);
    assert_eq!(math::cos(f64::NEG_INFINITY).to_bits(), 0x7ff8_0000_0000_0000);
    assert_eq!(math::logf(f32::from_bits(0x7f80_0001)).to_bits(), 0x7f80_0001);
    assert_eq!(math::atan2f(f32::from_bits(0x7fc0_0000), f32::from_bits(0xff80_0001)).to_bits(), 0xffc0_0001);
    assert_eq!(math::fmodf(f32::from_bits(0x7fc0_0000), f32::from_bits(0xff80_0001)).to_bits(), 0xffc0_0001);
    assert_eq!(math::cosf(f32::from_bits(0x3f55_d64e)).to_bits(), 0x3f2b_c3a9);
    assert_eq!(math::powf(f32::from_bits(0x5e32_6ca0), f32::from_bits(0xb7e6_377f)).to_bits(), 0x3f7f_b366);
    assert_eq!(math::pow(f64::from_bits(0x3fda_9b23_dd54_da4c), 4.0).to_bits(), 0x3f9e_9598_df5c_8b75);
    assert_eq!(math::frexp(5.0), (0.625, 3));
    assert_eq!(math::frexp(f64::from_bits(1)), (0.5, -1073));
    assert_eq!(math::lround(2.5), 3);
    assert_eq!(math::lround(-2.5), -3);
    assert_eq!(math::lround(f64::INFINITY), i64::MAX);
    assert_eq!(math::lround(f64::NEG_INFINITY), i64::MIN);
    assert_eq!(math::lround(f64::NAN), i64::MIN);
    assert_eq!(math::powi_f64(-2.0, -3).to_bits(), (-0.125_f64).to_bits());
    assert_eq!(math::floor(-0.5), -1.0);
    assert_eq!(math::ceil(-0.5).to_bits(), (-0.0_f64).to_bits());
    assert_eq!(math::round(0.5), 1.0);
    assert_eq!(math::trunc(-0.5).to_bits(), (-0.0_f64).to_bits());
    assert_eq!(math::sqrt(-1.0).to_bits(), 0x7ff8_0000_0000_0000);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod mac_reference {
    use super::math;
    use platinum_engine::fp_mode::with_flush_to_zero;
    use std::hint::black_box;

    unsafe extern "C" {
        #[link_name="expf"] fn expf(x:f32)->f32;
        #[link_name="exp2f"] fn exp2f(x:f32)->f32;
        #[link_name="__exp10f"] fn exp10f(x:f32)->f32;
        #[link_name="logf"] fn logf(x:f32)->f32;
        #[link_name="log2f"] fn log2f(x:f32)->f32;
        #[link_name="log10f"] fn log10f(x:f32)->f32;
        #[link_name="sinf"] fn sinf(x:f32)->f32;
        #[link_name="cosf"] fn cosf(x:f32)->f32;
        #[link_name="floorf"] fn floorf(x:f32)->f32;
        #[link_name="ceilf"] fn ceilf(x:f32)->f32;
        #[link_name="roundf"] fn roundf(x:f32)->f32;
        #[link_name="truncf"] fn truncf(x:f32)->f32;
        #[link_name="sqrtf"] fn sqrtf(x:f32)->f32;
        #[link_name="__sincosf_stret"] fn sincosf(x:f32)->math::SineCosineF32;
        #[link_name="atan2f"] fn atan2f(x:f32,y:f32)->f32;
        #[link_name="fmodf"] fn fmodf(x:f32,y:f32)->f32;
        #[link_name="powf"] fn powf(x:f32,y:f32)->f32;
        #[link_name="log"] fn log(x:f64)->f64;
        #[link_name="log10"] fn log10(x:f64)->f64;
        #[link_name="exp"] fn exp(x:f64)->f64;
        #[link_name="exp2"] fn exp2(x:f64)->f64;
        #[link_name="__exp10"] fn exp10(x:f64)->f64;
        #[link_name="sin"] fn sin(x:f64)->f64;
        #[link_name="cos"] fn cos(x:f64)->f64;
        #[link_name="tan"] fn tan(x:f64)->f64;
        #[link_name="atan"] fn atan(x:f64)->f64;
        #[link_name="floor"] fn floor(x:f64)->f64;
        #[link_name="ceil"] fn ceil(x:f64)->f64;
        #[link_name="round"] fn round(x:f64)->f64;
        #[link_name="trunc"] fn trunc(x:f64)->f64;
        #[link_name="sqrt"] fn sqrt(x:f64)->f64;
        #[link_name="sinh"] fn sinh(x:f64)->f64;
        #[link_name="cosh"] fn cosh(x:f64)->f64;
        #[link_name="tanh"] fn tanh(x:f64)->f64;
        #[link_name="asinh"] fn asinh(x:f64)->f64;
        #[link_name="log1p"] fn log1p(x:f64)->f64;
        #[link_name="__sincos_stret"] fn sincos(x:f64)->math::SineCosineF64;
        #[link_name="pow"] fn pow(x:f64,y:f64)->f64;
        #[link_name="logb"] fn logb(x:f64)->f64;
        #[link_name="scalbn"] fn scalbn(x:f64,n:i32)->f64;
        #[link_name="__powidf2"] fn powi(x:f64,n:i32)->f64;
        #[link_name="frexp"] fn frexp(x:f64,n:*mut i32)->f64;
        #[link_name="lround"] fn lround(x:f64)->i64;
    }
    type U32Pair = (&'static str, unsafe extern "C" fn(f32)->f32, fn(f32)->f32);
    type U64Pair = (&'static str, unsafe extern "C" fn(f64)->f64, fn(f64)->f64);
    const UNARY32: [U32Pair;13] = [
        ("expf",expf,math::expf),("exp2f",exp2f,math::exp2f),("exp10f",exp10f,math::exp10f),
        ("logf",logf,math::logf),("log2f",log2f,math::log2f),("log10f",log10f,math::log10f),
        ("sinf",sinf,math::sinf),("cosf",cosf,math::cosf),
        ("floorf",floorf,math::floorf),("ceilf",ceilf,math::ceilf),("roundf",roundf,math::roundf),
        ("truncf",truncf,math::truncf),("sqrtf",sqrtf,math::sqrtf),
    ];
    const UNARY64: [U64Pair;21] = [
        ("log",log,math::log),("log10",log10,math::log10),("exp",exp,math::exp),
        ("exp2",exp2,math::exp2),("exp10",exp10,math::exp10),("sin",sin,math::sin),
        ("cos",cos,math::cos),("tan",tan,math::tan),("atan",atan,math::atan),("logb",logb,math::logb),
        ("ln",log,math::ln),
        ("floor",floor,math::floor),("ceil",ceil,math::ceil),("round",round,math::round),
        ("trunc",trunc,math::trunc),("sqrt",sqrt,math::sqrt),
        ("sinh",sinh,math::sinh),("cosh",cosh,math::cosh),("tanh",tanh,math::tanh),
        ("asinh",asinh,math::asinh),("log1p",log1p,math::log1p),
    ];
    const SPECIAL32: [u32;16] = [0,0x80000000,1,0x80000001,0x007fffff,0x807fffff,
        0x00800000,0x80800000,0x3f800000,0xbf800000,0x7f800000,0xff800000,
        0x7fc00000,0xffc00000,0x7f800001,0xff800001];
    const SPECIAL64: [u64;16] = [0,0x8000000000000000,1,0x8000000000000001,
        0x000fffffffffffff,0x800fffffffffffff,0x0010000000000000,0x8010000000000000,
        0x3ff0000000000000,0xbff0000000000000,0x7ff0000000000000,0xfff0000000000000,
        0x7ff8000000000000,0xfff8000000000000,0x7ff0000000000001,0xfff0000000000001];
    fn next(state:&mut u64)->u64 {
        *state=state.wrapping_add(0x9e3779b97f4a7c15);
        let mut x=*state; x=(x^(x>>30)).wrapping_mul(0xbf58476d1ce4e5b9);
        x=(x^(x>>27)).wrapping_mul(0x94d049bb133111eb); x^(x>>31)
    }
    fn compare() {
        let mut state=0x8d85b019a8ec7231_u64;
        for i in 0..4096 {
            let bits=if i<SPECIAL32.len() { SPECIAL32[i] } else { next(&mut state) as u32 };
            let x=black_box(f32::from_bits(bits));
            for (name,native,portable) in UNARY32 {
                assert_eq!(unsafe { native(x) }.to_bits(),portable(x).to_bits(),"{name} x={bits:08x}");
            }
            let a=unsafe { sincosf(x) }; let b=math::sincosf(x);
            assert_eq!((a.sine.to_bits(),a.cosine.to_bits()),(b.sine.to_bits(),b.cosine.to_bits()),"sincosf x={bits:08x}");
            let bits64=if i<SPECIAL64.len() { SPECIAL64[i] } else { next(&mut state) };
            let x64=black_box(f64::from_bits(bits64));
            for (name,native,portable) in UNARY64 {
                assert_eq!(unsafe { native(x64) }.to_bits(),portable(x64).to_bits(),"{name} x={bits64:016x}");
            }
            let a=unsafe { sincos(x64) }; let b=math::sincos(x64);
            assert_eq!((a.sine.to_bits(),a.cosine.to_bits()),(b.sine.to_bits(),b.cosine.to_bits()),"sincos x={bits64:016x}");
            let mut exponent=0;
            let fraction=unsafe { frexp(x64,&mut exponent) };
            let portable=math::frexp(x64);
            assert_eq!((fraction.to_bits(),exponent),(portable.0.to_bits(),portable.1),"frexp x={bits64:016x}");
            assert_eq!(unsafe { lround(x64) },math::lround(x64),"lround x={bits64:016x}");
            let n=(next(&mut state)%4401) as i32-2200;
            assert_eq!(unsafe { scalbn(x64,n) }.to_bits(),math::scalbn(x64,n).to_bits(),"scalbn x={bits64:016x} n={n}");
            assert_eq!(unsafe { powi(x64,n) }.to_bits(),math::powi_f64(x64,n).to_bits(),"powi x={bits64:016x} n={n}");
        }
        for &xb in &SPECIAL32 { for &yb in &SPECIAL32 {
            let x=black_box(f32::from_bits(xb)); let y=black_box(f32::from_bits(yb));
            for (name,native,portable) in [("atan2f",atan2f as unsafe extern "C" fn(f32,f32)->f32,math::atan2f as fn(f32,f32)->f32),
                ("fmodf",fmodf,math::fmodf),("powf",powf,math::powf)] {
                assert_eq!(unsafe { native(x,y) }.to_bits(),portable(x,y).to_bits(),"{name} x={xb:08x} y={yb:08x}");
            }
        }}
        for &xb in &SPECIAL64 { for &yb in &SPECIAL64 {
            let x=black_box(f64::from_bits(xb)); let y=black_box(f64::from_bits(yb));
            assert_eq!(unsafe { pow(x,y) }.to_bits(),math::pow(x,y).to_bits(),"pow x={xb:016x} y={yb:016x}");
        }}
    }
    #[test] fn sampled_system_bits() { compare(); }
    #[test] fn sampled_system_bits_flush_to_zero() { with_flush_to_zero(compare); }
}
