//! Scalar ARM instruction and Mac Rust-method regressions.
use platinum_engine::portable::arm_scalar::*;
use std::hint::black_box;

struct FpControl {
    #[cfg(target_arch = "aarch64")]
    previous: u64,
    #[cfg(target_arch = "x86_64")]
    previous: u32,
}
impl FpControl {
    fn new(fz: bool) -> Self {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            let previous: u64;
            std::arch::asm!("mrs {v}, fpcr", v=out(reg) previous, options(nostack, preserves_flags));
            let control = (previous & !((1 << 24) | (1 << 25) | (3 << 22))) | ((fz as u64) << 24);
            std::arch::asm!("msr fpcr, {v}", v=in(reg) control, options(nostack, preserves_flags));
            Self { previous }
        }
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let mut previous = 0_u32;
            std::arch::asm!("stmxcsr [{p}]", p=in(reg) &mut previous, options(nostack, preserves_flags));
            let control = (previous & !((1 << 15) | (1 << 6) | (3 << 13))) | if fz { (1 << 15) | (1 << 6) } else { 0 };
            std::arch::asm!("ldmxcsr [{p}]", p=in(reg) &control, options(nostack, preserves_flags));
            Self { previous }
        }
    }
}
impl Drop for FpControl {
    fn drop(&mut self) {
        #[cfg(target_arch = "aarch64")]
        unsafe { std::arch::asm!("msr fpcr, {v}", v=in(reg) self.previous, options(nostack, preserves_flags)); }
        #[cfg(target_arch = "x86_64")]
        unsafe { std::arch::asm!("ldmxcsr [{p}]", p=in(reg) &self.previous, options(nostack, preserves_flags)); }
    }
}

#[test]
fn instructions_have_arm_nan_and_zero_rules() {
    let s = f32::from_bits(0xff80_1234);
    let q = f64::from_bits(0x7ff8_0000_0000_0011);
    let s64 = f64::from_bits(0xfff0_0000_0000_0022);
    assert_eq!(fminnm_f32(s, 1.0).to_bits(), 0xffc0_1234);
    assert_eq!(fmaxnm_f64(1.0, s64).to_bits(), 0xfff8_0000_0000_0022);
    assert_eq!(fminnm_f64(q, 1.0).to_bits(), 1.0_f64.to_bits());
    assert_eq!(fminnm_f64(0.0, -0.0).to_bits(), (-0.0_f64).to_bits());
    assert_eq!(fmaxnm_f64(-0.0, 0.0).to_bits(), 0);
    assert_eq!(fminnm_f64_fz(f64::from_bits(0x8000_0000_0000_0001), 0.0).to_bits(), (-0.0_f64).to_bits());
    assert_eq!(fmadd_f32(f32::INFINITY, 0.0, 1.0).to_bits(), 0x7fc0_0000);
    assert_eq!(fmadd_f64(f64::INFINITY, 0.0, s64).to_bits(), 0xfff8_0000_0000_0022);
    assert_eq!(fmadd_f64_fz(f64::MIN_POSITIVE, f64::from_bits(0x3fef_ffff_ffff_ffff), 0.0).to_bits(), 0);
    assert_eq!(fmadd_f32_fz(-f32::MIN_POSITIVE, f32::from_bits(0x3f7f_ffff), -0.0).to_bits(), 0x8000_0000);
}

#[test]
fn rust_methods_track_current_fz_and_preserve_native_nan_selection() {
    for fz in [false, true] {
        let _mode = FpControl::new(fz);
        let sub32 = black_box(f32::from_bits(1));
        let sub64 = black_box(f64::from_bits(1));
        let s32 = black_box(f32::from_bits(0xff80_1234));
        let q32 = black_box(f32::from_bits(0x7fc0_5678));
        let s64 = black_box(f64::from_bits(0xfff0_0000_0000_1234));
        let expected32 = u32::from(!fz);
        let expected64 = u64::from(!fz);
        assert_eq!(sub32.arm_min(q32).to_bits(), expected32);
        assert_eq!(s32.arm_max(sub32).to_bits(), expected32);
        assert_eq!(s32.arm_min(q32).to_bits(), 0xffc0_1234);
        assert_eq!(sub64.arm_max(s64).to_bits(), expected64);
        assert_eq!(black_box(0.0_f32).arm_min(black_box(-0.0)).to_bits(), 0x8000_0000);
        assert_eq!(black_box(-0.0_f64).arm_max(black_box(0.0)).to_bits(), 0);
        assert_eq!(sub32.arm_mul_add(black_box(1.0), black_box(0.0)).to_bits(), expected32);
        assert_eq!(sub64.arm_mul_add(black_box(1.0), black_box(0.0)).to_bits(), expected64);
    }
}
