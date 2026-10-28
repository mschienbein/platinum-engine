//! Bitwise ARM instruction regressions, including FPCR and host-mode boundaries.
use platinum_engine::portable::arm::*;

fn mode(fz: bool, dn: bool) -> FpMode { FpMode { flush_to_zero: fz, default_nan: dn } }

#[test]
fn scalar_nan_zero_and_flush_rules() {
    let q1 = f32::from_bits(0xffc0_1234);
    let s2 = f32::from_bits(0x7f80_5678);
    assert_eq!(max_number(q1, 7.0).to_bits(), 7.0_f32.to_bits());
    assert_eq!(min_number(7.0, s2).to_bits(), 0x7fc0_5678);
    assert_eq!(max_propagating(q1, s2).to_bits(), 0x7fc0_5678);
    assert_eq!(max_propagating(q1, 1.0).to_bits(), q1.to_bits());
    assert_eq!(max_number_with_mode(s2, q1, mode(false, true)).to_bits(), 0x7fc0_0000);
    assert_eq!(max_number(-0.0, 0.0).to_bits(), 0);
    assert_eq!(min_number(0.0, -0.0).to_bits(), 0x8000_0000);
    assert_eq!(max_number_fz(f32::from_bits(1), -0.0).to_bits(), 0);
    assert_eq!(min_number_fz(f32::from_bits(0x8000_0001), 0.0).to_bits(), 0x8000_0000);
    assert!(!arm_lt(-0.0, 0.0));
    assert!(arm_lt(q1, 0.0));
    assert!(arm_lt(0.0, f32::from_bits(1)));
    assert!(!arm_lt_fz(0.0, f32::from_bits(1)));
    assert!(arm_lt(f32::from_bits(0x8000_0001), -0.0));
    assert!(!arm_lt_fz(f32::from_bits(0x8000_0001), -0.0));
    assert_eq!(vabs_f32([f32::from_bits(0xff80_1234), f32::from_bits(0x8000_0001)]).map(f32::to_bits), [0x7f80_1234, 1]);
}

#[test]
fn fma_nan_priority_and_tininess_before_rounding() {
    let q1 = f32::from_bits(0x7fc0_0001);
    let q2 = f32::from_bits(0xffc0_0002);
    let q3 = f32::from_bits(0x7fc0_0003);
    let s1 = f32::from_bits(0x7f80_0001);
    let s2 = f32::from_bits(0xff80_0002);
    let s3 = f32::from_bits(0x7f80_0003);
    assert_eq!(fmadd_f32(q1, q2, q3).to_bits(), q3.to_bits());
    assert_eq!(fmadd_f32(s1, s2, s3).to_bits(), 0x7fc0_0003);
    assert_eq!(fmadd_f32(q1, s2, q3).to_bits(), 0xffc0_0002);
    assert_eq!(fmadd_f32(f32::INFINITY, 0.0, q3).to_bits(), 0x7fc0_0000);
    assert_eq!(fmadd_f32(f32::INFINITY, 0.0, s3).to_bits(), 0x7fc0_0003);
    assert_eq!(fmadd_f32(f32::INFINITY, 1.0, f32::NEG_INFINITY).to_bits(), 0x7fc0_0000);
    let just_below_one = f32::from_bits(0x3f7f_ffff);
    assert_eq!(fmul_f32(f32::MIN_POSITIVE, just_below_one).to_bits(), 0x0080_0000);
    assert_eq!(fmul_f32_fz(f32::MIN_POSITIVE, just_below_one).to_bits(), 0);
    assert_eq!(fmul_f64(f64::MIN_POSITIVE, f64::from_bits(0x3fef_ffff_ffff_ffff)).to_bits(), 0x0010_0000_0000_0000);
    assert_eq!(fmul_f64_fz(f64::MIN_POSITIVE, f64::from_bits(0x3fef_ffff_ffff_ffff)).to_bits(), 0);
    assert_eq!(fmadd_f32_fz(-f32::MIN_POSITIVE, just_below_one, -0.0).to_bits(), 0x8000_0000);
}

#[test]
fn lane_moves_preserve_payloads_and_order() {
    let input = [f32::from_bits(0x7f80_0123), -0.0, 2.0, -3.0];
    let loaded = unsafe { vld1q_f32(input.as_ptr()) };
    assert_eq!(loaded.map(f32::to_bits), input.map(f32::to_bits));
    let mut bytes = [0_u8; 17];
    for (index, value) in input.iter().enumerate() {
        bytes[1 + 4 * index..5 + 4 * index].copy_from_slice(&value.to_bits().to_ne_bytes());
    }
    let unaligned = unsafe { bytes.as_ptr().add(1).cast::<f32>() };
    assert_eq!(unsafe { vld1q_f32(unaligned) }.map(f32::to_bits), input.map(f32::to_bits));
    assert_eq!(vdupq_laneq_f32::<0>(input).map(f32::to_bits), [0x7f80_0123; 4]);
    assert_eq!(vgetq_lane_f32::<1>(input).to_bits(), 0x8000_0000);
    assert_eq!(vget_lane_f32::<1>(vdup_n_f32(-0.0)).to_bits(), 0x8000_0000);
    assert_eq!(vsetq_lane_f32::<2>(4.0, input)[2], 4.0);
    let base = [1.0, 2.0, 3.0, 4.0];
    assert_eq!(vfmaq_f32(base, [2.0; 4], [0.5; 4]), [2.0, 3.0, 4.0, 5.0]);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn x86_flush_controls_do_not_change_the_requested_arm_mode() {
    use std::arch::asm;
    use std::hint::black_box;
    struct Restore(u32);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe { asm!("ldmxcsr [{p}]", p=in(reg) &self.0, options(nostack, preserves_flags)); }
        }
    }
    let mut original = 0_u32;
    unsafe { asm!("stmxcsr [{p}]", p=in(reg) &mut original, options(nostack, preserves_flags)); }
    let _restore = Restore(original);
    for host in [0, 1 << 15, 1 << 6, (1 << 15) | (1 << 6)] {
        let control = (original & !((1 << 15) | (1 << 6) | (3 << 13))) | host;
        unsafe { asm!("ldmxcsr [{p}]", p=in(reg) &control, options(nostack, preserves_flags)); }
        let sub32 = black_box(f32::from_bits(1));
        let sub64 = black_box(f64::from_bits(1));
        assert_eq!(fmadd_f32(sub32, black_box(1.0), 0.0).to_bits(), 1);
        assert_eq!(fmadd_f64(sub64, black_box(1.0), 0.0).to_bits(), 1);
        assert_eq!(fadd_f32(black_box(f32::MIN_POSITIVE), sub32).to_bits(), 0x0080_0001);
        assert_eq!(fadd_f64(black_box(f64::MIN_POSITIVE), sub64).to_bits(), 0x0010_0000_0000_0001);
        assert_eq!(max_number(sub32, 0.0).to_bits(), 1);
        assert_eq!(max_number_fz(sub32, -0.0).to_bits(), 0);
        assert!(arm_lt(0.0, sub32));
        assert!(!arm_lt_fz(0.0, sub32));
        assert_eq!(fmul_f32_fz(black_box(f32::MIN_POSITIVE), black_box(f32::from_bits(0x3f7f_ffff))).to_bits(), 0);
        assert_eq!(fmul_f64_fz(black_box(f64::MIN_POSITIVE), black_box(f64::from_bits(0x3fef_ffff_ffff_ffff))).to_bits(), 0);
    }
}

#[cfg(target_arch = "aarch64")]
mod native {
    use super::*;
    use std::arch::asm;
    use std::hint::black_box;

    struct Restore(u64);
    impl Restore {
        fn new() -> Self {
            let value: u64;
            unsafe { asm!("mrs {value}, fpcr", value=out(reg) value, options(nostack, preserves_flags)); }
            Self(value)
        }
        fn set(&self, mode: FpMode) {
            let bits = (self.0 & !((1 << 24) | (1 << 25) | (3 << 22)))
                | ((mode.flush_to_zero as u64) << 24) | ((mode.default_nan as u64) << 25);
            unsafe { asm!("msr fpcr, {bits}", bits=in(reg) bits, options(nostack, preserves_flags)); }
        }
    }
    impl Drop for Restore {
        fn drop(&mut self) { unsafe { asm!("msr fpcr, {bits}", bits=in(reg) self.0, options(nostack, preserves_flags)); } }
    }

    macro_rules! binary32 {
        ($op:literal, $name:ident) => {
            fn $name(a: f32, b: f32) -> f32 {
                let out: f32;
                unsafe { asm!(concat!($op, " {out:s}, {a:s}, {b:s}"), out=lateout(vreg) out, a=in(vreg) a, b=in(vreg) b, options(nomem, nostack)); }
                out
            }
        };
    }
    macro_rules! binary64 {
        ($op:literal, $name:ident) => {
            fn $name(a: f64, b: f64) -> f64 {
                let out: f64;
                unsafe { asm!(concat!($op, " {out:d}, {a:d}, {b:d}"), out=lateout(vreg) out, a=in(vreg) a, b=in(vreg) b, options(nomem, nostack)); }
                out
            }
        };
    }
    binary32!("fmaxnm", maxnm); binary32!("fminnm", minnm);
    binary32!("fmax", max); binary32!("fmin", min);
    binary32!("fadd", add32); binary32!("fsub", sub32); binary32!("fmul", mul32);
    binary64!("fadd", add64); binary64!("fsub", sub64); binary64!("fmul", mul64);
    fn cmp_lt(a: f32, b: f32) -> bool {
        let out: u32;
        unsafe { asm!("fcmp {a:s}, {b:s}", "cset {out:w}, lt",
            out=lateout(reg) out, a=in(vreg) a, b=in(vreg) b, options(nomem,nostack)); }
        out != 0
    }
    fn fma32(a: f32, b: f32, c: f32) -> f32 {
        let out: f32;
        unsafe { asm!("fmadd {out:s}, {a:s}, {b:s}, {c:s}", out=lateout(vreg) out, a=in(vreg) a, b=in(vreg) b, c=in(vreg) c, options(nomem, nostack)); }
        out
    }
    fn fma64(a: f64, b: f64, c: f64) -> f64 {
        let out: f64;
        unsafe { asm!("fmadd {out:d}, {a:d}, {b:d}, {c:d}", out=lateout(vreg) out, a=in(vreg) a, b=in(vreg) b, c=in(vreg) c, options(nomem, nostack)); }
        out
    }
    const SPECIAL32: [u32; 24] = [
        0, 0x8000_0000, 1, 0x8000_0001, 0x007f_ffff, 0x807f_ffff,
        0x0080_0000, 0x8080_0000, 0x0080_0001, 0x8080_0001,
        0x3f7f_ffff, 0xbf7f_ffff, 0x3f80_0000, 0xbf80_0000,
        0x7f7f_ffff, 0xff7f_ffff, 0x7f80_0000, 0xff80_0000,
        0x7f80_0001, 0xff80_0123, 0x7fbf_ffff, 0xffbf_ffff, 0x7fc0_0001, 0xffc0_0123,
    ];
    const SPECIAL64: [u64; 24] = [
        0, 0x8000_0000_0000_0000, 1, 0x8000_0000_0000_0001, 0x000f_ffff_ffff_ffff, 0x800f_ffff_ffff_ffff,
        0x0010_0000_0000_0000, 0x8010_0000_0000_0000, 0x0010_0000_0000_0001, 0x8010_0000_0000_0001,
        0x3fef_ffff_ffff_ffff, 0xbfef_ffff_ffff_ffff, 0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000,
        0x7fef_ffff_ffff_ffff, 0xffef_ffff_ffff_ffff, 0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
        0x7ff0_0000_0000_0001, 0xfff0_0000_0000_0123, 0x7ff7_ffff_ffff_ffff, 0xfff7_ffff_ffff_ffff,
        0x7ff8_0000_0000_0001, 0xfff8_0000_0000_0123,
    ];
    fn equal32(name: &str, native: f32, portable: f32, a: u32, b: u32, c: u32, mode: FpMode) {
        assert_eq!(native.to_bits(), portable.to_bits(), "{name} {a:08x} {b:08x} {c:08x} {mode:?}");
    }
    fn equal64(name: &str, native: f64, portable: f64, a: u64, b: u64, c: u64, mode: FpMode) {
        assert_eq!(native.to_bits(), portable.to_bits(), "{name} {a:016x} {b:016x} {c:016x} {mode:?}");
    }

    #[test]
    fn all_special_pairs_and_triples_match_real_instructions() {
        let restore = Restore::new();
        for fz in [false, true] { for dn in [false, true] {
            let mode = mode(fz, dn); restore.set(mode);
            for &ai in &SPECIAL32 { for &bi in &SPECIAL32 {
                let (a,b)=(f32::from_bits(ai),f32::from_bits(bi));
                assert_eq!(arm_lt_with_mode(a,b,mode),cmp_lt(a,b),"FCMP LT {ai:08x} {bi:08x} {mode:?}");
                for (name, x, y) in [
                    ("fmaxnm", maxnm(a,b), max_number_with_mode(a,b,mode)),
                    ("fminnm", minnm(a,b), min_number_with_mode(a,b,mode)),
                    ("fmax", max(a,b), max_propagating_with_mode(a,b,mode)),
                    ("fmin", min(a,b), min_propagating_with_mode(a,b,mode)),
                    ("fadd", add32(a,b), fadd_f32_with_mode(a,b,mode)),
                    ("fsub", sub32(a,b), fsub_f32_with_mode(a,b,mode)),
                    ("fmul", mul32(a,b), fmul_f32_with_mode(a,b,mode)),
                ] { equal32(name,x,y,ai,bi,0,mode); }
                for &ci in &SPECIAL32 {
                    let c=f32::from_bits(ci);
                    equal32("fmadd", fma32(a,b,c), fmadd_f32_with_mode(a,b,c,mode), ai,bi,ci,mode);
                }
            }}
            for &ai in &SPECIAL64 { for &bi in &SPECIAL64 {
                let (a,b)=(f64::from_bits(ai),f64::from_bits(bi));
                for (name,x,y) in [
                    ("fadd", add64(a,b), fadd_f64_with_mode(a,b,mode)),
                    ("fsub", sub64(a,b), fsub_f64_with_mode(a,b,mode)),
                    ("fmul", mul64(a,b), fmul_f64_with_mode(a,b,mode)),
                ] { equal64(name,x,y,ai,bi,0,mode); }
                for &ci in &SPECIAL64 {
                    let c=f64::from_bits(ci);
                    equal64("fmadd", fma64(a,b,c), fmadd_f64_with_mode(a,b,c,mode), ai,bi,ci,mode);
                }
            }}
        }}
    }

    #[test]
    fn emulated_mode_is_independent_of_host_fz_and_dn() {
        let restore = Restore::new();
        for fz in [false, true] { for dn in [false, true] {
            let target=mode(fz,dn); restore.set(target);
            let mut expected32=Vec::new();
            let mut expected64=Vec::new();
            for &a in &SPECIAL32 { for &b in &SPECIAL32 { for &c in &SPECIAL32 {
                expected32.push(fma32(f32::from_bits(a), f32::from_bits(b), f32::from_bits(c)).to_bits());
            }}}
            for &a in &SPECIAL64 { for &b in &SPECIAL64 { for &c in &SPECIAL64 {
                expected64.push(fma64(f64::from_bits(a), f64::from_bits(b), f64::from_bits(c)).to_bits());
            }}}
            for host_fz in [false,true] { for host_dn in [false,true] {
                restore.set(mode(host_fz,host_dn));
                let mut i=0;
                for &a in &SPECIAL32 { for &b in &SPECIAL32 { for &c in &SPECIAL32 {
                    let actual=fmadd_f32_with_mode(black_box(f32::from_bits(a)),black_box(f32::from_bits(b)),black_box(f32::from_bits(c)),target).to_bits();
                    assert_eq!(actual,expected32[i],"target={target:?} host={:?} a={a:08x} b={b:08x} c={c:08x}",mode(host_fz,host_dn)); i+=1;
                }}}
                i=0;
                for &a in &SPECIAL64 { for &b in &SPECIAL64 { for &c in &SPECIAL64 {
                    let actual=fmadd_f64_with_mode(black_box(f64::from_bits(a)),black_box(f64::from_bits(b)),black_box(f64::from_bits(c)),target).to_bits();
                    assert_eq!(actual,expected64[i],"target={target:?} host={:?} a={a:016x} b={b:016x} c={c:016x}",mode(host_fz,host_dn)); i+=1;
                }}}
            }}
        }}
    }
    #[test]
    fn vector_and_phase_sequences_match_native() {
        use core::arch::aarch64 as neon;
        let restore = Restore::new();
        let mut seed = 0x94d0_49bb_1331_11eb_u64;
        let mut next = || {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed
        };
        for fz in [false, true] { for dn in [false, true] {
            let mode = mode(fz, dn);
            restore.set(mode);
            for i in 0..30_000 {
                let mut av = [0.0_f32; 4]; let mut bv = av; let mut cv = av;
                for lane in 0..4 {
                    av[lane] = f32::from_bits(if i < 24 { SPECIAL32[(i + lane) % 24] } else { next() as u32 });
                    bv[lane] = f32::from_bits(if i < 24 { SPECIAL32[(i + lane * 3 + 7) % 24] } else { next() as u32 });
                    cv[lane] = f32::from_bits(if i < 24 { SPECIAL32[(i + lane * 5 + 13) % 24] } else { next() as u32 });
                }
                unsafe {
                    let a=neon::vld1q_f32(av.as_ptr()); let b=neon::vld1q_f32(bv.as_ptr());
                    let c=neon::vld1q_f32(cv.as_ptr());
                    // Pin FMLA operand order: LLVM may commute the intrinsic's
                    // multiplicands, which changes the selected NaN payload.
                    let mut fmla = c;
                    asm!("fmla {c:v}.4s, {a:v}.4s, {b:v}.4s",
                        c=inout(vreg) fmla, a=in(vreg) a, b=in(vreg) b,
                        options(nomem, nostack, preserves_flags));
                    let mut result=[0.0_f32; 4];
                    for (name, native, portable) in [
                        ("vmaxq", neon::vmaxq_f32(a,b), vmaxq_f32_with_mode(av,bv,mode)),
                        ("vsubq", neon::vsubq_f32(a,b), vsubq_f32_with_mode(av,bv,mode)),
                        ("vfmaq", fmla, vfmaq_f32_with_mode(cv,av,bv,mode)),
                    ] {
                        neon::vst1q_f32(result.as_mut_ptr(),native);
                        assert_eq!(result.map(f32::to_bits),portable.map(f32::to_bits),
                            "{name} {mode:?} a={:x?} b={:x?} c={:x?}",av.map(f32::to_bits),bv.map(f32::to_bits),cv.map(f32::to_bits));
                    }
                    let a=neon::vld1_f32(av.as_ptr()); let b=neon::vld1_f32(bv.as_ptr());
                    let mut result=[0.0_f32; 2];
                    for (name,native,portable) in [
                        ("vabs",neon::vabs_f32(a),vabs_f32([av[0],av[1]])),
                        ("vmaxnm",neon::vmaxnm_f32(a,b),vmaxnm_f32_with_mode([av[0],av[1]],[bv[0],bv[1]],mode)),
                        ("vminnm",neon::vminnm_f32(a,b),vminnm_f32_with_mode([av[0],av[1]],[bv[0],bv[1]],mode)),
                    ] {
                        neon::vst1_f32(result.as_mut_ptr(),native);
                        assert_eq!(result.map(f32::to_bits),portable.map(f32::to_bits),"{name} {mode:?}");
                    }
                }
                let mut values=[0.0_f64; 7];
                for (j,value) in values.iter_mut().enumerate() {
                    *value=f64::from_bits(if i < 24 { SPECIAL64[(i + j * 7) % 24] } else { next() });
                }
                let [a1,a2,x,negative_a1,negative_a2,mut state0,mut state1]=values;
                let (mut expected0,mut expected1)=(state0,state1);
                let y: f64;
                unsafe { asm!(
                    "fmadd {y:d}, {a2:d}, {x:d}, {state0:d}",
                    "fmul {tmp:d}, {y:d}, {negative_a1:d}",
                    "fmadd {tmp:d}, {a1:d}, {x:d}, {tmp:d}",
                    "fadd {state0:d}, {state1:d}, {tmp:d}",
                    "fmul {tmp:d}, {y:d}, {negative_a2:d}",
                    "fmadd {state1:d}, {one:d}, {x:d}, {tmp:d}",
                    y=out(vreg) y, tmp=out(vreg) _, a1=in(vreg) a1, a2=in(vreg) a2,
                    x=in(vreg) x, negative_a1=in(vreg) negative_a1, negative_a2=in(vreg) negative_a2,
                    one=in(vreg) 1.0_f64, state0=inout(vreg) expected0, state1=inout(vreg) expected1,
                    options(nomem, nostack, preserves_flags),
                ); }
                let actual=phase_sample_with_mode(a1,a2,x,negative_a1,negative_a2,&mut state0,&mut state1,mode);
                assert_eq!([actual.to_bits(),state0.to_bits(),state1.to_bits()],
                    [y.to_bits(),expected0.to_bits(),expected1.to_bits()],
                    "phase {mode:?} inputs={:x?}",values.map(f64::to_bits));
            }
        }}
    }
    #[test]
    fn vector_fmla_exhausts_special_operand_triples() {
        use core::arch::aarch64 as neon;
        let restore = Restore::new();
        for fz in [false, true] { for dn in [false, true] {
            let mode = mode(fz,dn);
            restore.set(mode);
            for &a in &SPECIAL32 { for &b in &SPECIAL32 { for &c in &SPECIAL32 {
                let av = [a,b,c,a^0x8000_0000].map(f32::from_bits);
                let bv = [b,c,a,b^0x8000_0000].map(f32::from_bits);
                let cv = [c,a,b,c^0x8000_0000].map(f32::from_bits);
                let mut expected = [0.0_f32;4];
                unsafe {
                    let va=neon::vld1q_f32(av.as_ptr());
                    let vb=neon::vld1q_f32(bv.as_ptr());
                    let mut vc=neon::vld1q_f32(cv.as_ptr());
                    asm!("fmla {c:v}.4s, {a:v}.4s, {b:v}.4s",
                        c=inout(vreg) vc,a=in(vreg) va,b=in(vreg) vb,
                        options(nomem,nostack,preserves_flags));
                    neon::vst1q_f32(expected.as_mut_ptr(),vc);
                }
                let actual=vfmaq_f32_with_mode(cv,av,bv,mode);
                assert_eq!(actual.map(f32::to_bits),expected.map(f32::to_bits),
                    "FMLA {mode:?} a={a:08x} b={b:08x} c={c:08x}");
            }}}
        }}
    }
}
