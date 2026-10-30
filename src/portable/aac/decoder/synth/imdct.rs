//! `MDCT::InverseInPlaceTransform(P, Q)` (0x204b8) with `DCT_IV::PreModulation` (0x16f62c) and `DCT_IV::PostModulationExtend` (0x16f92c), N = 1024 / 128.
//!
//! `P[0..N)` holds the spectrum on entry and the first half of the 2N windowed-ready time samples on exit, `Q[0..N)` is scratch on entry and the second
//! half on exit (closed forms, checked against the instruction-level model).  The 512-point (long) / 64-point (short)
//! complex DFT is Apple's `vDSP_DFT_Execute`, the generic NEON kernel.

use crate::portable::aac::fb::tables::{MOD_LONG, MOD_SHORT};
use crate::portable::dft::{DftSetup, Direction};
use std::sync::LazyLock;

/// The three `vDSP_DFT_Setup`s of the decoder (forward direction only: Apple creates the inverse plan but never executes it).
struct Dft {
    d512: DftSetup,
    d64: DftSetup,
}

static DFT: LazyLock<Dft> = LazyLock::new(|| Dft {
    d512: DftSetup::new(512, Direction::Forward).expect("512-point plan"),
    d64: DftSetup::new(64, Direction::Forward).expect("64-point plan"),
});

/// An opaque `0x8000_0000`: the negations that Apple's code applies *after* an `fmla` (`fmla` then `fneg`) are done as an XOR with this value.
///
/// `-fma(a,b,c)` and `fma(-a,b,-c)` differ in the sign of an exact-zero result (`-0.0` vs `+0.0`).  The first Rust port and its C model were
/// rewritten by LLVM from the first form into the second and mismatched Apple on silent frames.  Whether a given
/// compiler version does so depends on the surrounding code; a mask the optimiser cannot see through (a plain vector XOR, loops stay vectorisable)
/// keeps the sign flip applied to the materialised fma result by construction.  The exact signs are pinned by whole-stream comparison with Apple.
#[inline(always)]
fn sign_mask() -> u32 {
    core::hint::black_box(0x8000_0000u32)
}

/// `fneg` of an `fmla` result (see [`sign_mask`]).
#[inline(always)]
fn flip(x: f32, mask: u32) -> f32 {
    f32::from_bits(x.to_bits() ^ mask)
}

/// `DCT_IV::PreModulation`: spectrum `inp[N]` (scaled by `s` = 1/N, exact) -> DFT input planes `zre[N/2]`, `zim[N/2]`.
#[inline(always)]
fn premod<const N: usize, const H: usize>(t: &[f32; N], inp: &[f32; N], s: f32, zre: &mut [f32; H], zim: &mut [f32; H]) {
    for k in 0..N / 4 {
        let a = s * inp[2 * k];
        let b = s * inp[N - 1 - 2 * k];
        let c = t[2 * k];
        let sn = t[2 * k + 1];
        zre[k] = sn.mul_add(b, a * c);
        zim[k] = c.mul_add(b, a * -sn);
        let d = s * inp[2 * k + 1];
        let cc = s * inp[N - 2 - 2 * k];
        let te = t[N - 2 - 2 * k];
        let to = t[N - 1 - 2 * k];
        zre[H - 1 - k] = te.mul_add(cc, d * to);
        zim[H - 1 - k] = te.mul_add(d, cc * -to);
    }
}

/// `DCT_IV::PostModulationExtend`: DFT output `r`, `i` (N/2 each) -> `p[N]`, `q[N]` (the 2N IMDCT points).
#[inline(always)]
fn postmod<const N: usize, const H: usize>(t: &[f32; N], r: &[f32; H], i: &[f32; H], p: &mut [f32; N], q: &mut [f32; N]) {
    let m = sign_mask();
    for u in 0..N / 4 {
        let v = H - 1 - u;
        let a_u = t[2 * u + 1].mul_add(i[u], r[u] * t[2 * u]);
        let b_u = t[2 * u + 1].mul_add(r[u], i[u] * -t[2 * u]);
        let a_v = t[2 * v + 1].mul_add(i[v], r[v] * t[2 * v]);
        let c_v = t[2 * v].mul_add(i[v], r[v] * -t[2 * v + 1]);
        p[H + 2 * u] = flip(b_u, m);
        p[H - 1 - 2 * u] = b_u;
        p[H - 2 - 2 * u] = a_v;
        p[H + 1 + 2 * u] = flip(a_v, m);
        q[H + 2 * u] = flip(a_u, m);
        q[H - 1 - 2 * u] = flip(a_u, m);
        q[H - 2 - 2 * u] = c_v;
        q[H + 1 + 2 * u] = c_v;
    }
}

/// 1024-line IMDCT (long, START, STOP blocks).
pub fn imdct_1024(p: &mut [f32; 1024], q: &mut [f32; 1024]) {
    let (zre, zim) = q.split_at_mut(512);
    let (zre, zim): (&mut [f32; 512], &mut [f32; 512]) = (zre.try_into().unwrap(), zim.try_into().unwrap());
    premod::<1024, 512>(&MOD_LONG, p, 1.0 / 1024.0, zre, zim);
    let mut or = [0f32; 512];
    let mut oi = [0f32; 512];
    let dft = &*DFT;
    dft.d512.execute(zre, zim, &mut or, &mut oi);
    postmod::<1024, 512>(&MOD_LONG, &or, &oi, p, q);
}

/// 128-line IMDCT (one EIGHT_SHORT window).
pub fn imdct_128(p: &mut [f32; 128], q: &mut [f32; 128]) {
    let (zre, zim) = q.split_at_mut(64);
    let (zre, zim): (&mut [f32; 64], &mut [f32; 64]) = (zre.try_into().unwrap(), zim.try_into().unwrap());
    premod::<128, 64>(&MOD_SHORT, p, 1.0 / 128.0, zre, zim);
    let mut or = [0f32; 64];
    let mut oi = [0f32; 64];
    DFT.d64.execute(zre, zim, &mut or, &mut oi);
    postmod::<128, 64>(&MOD_SHORT, &or, &oi, p, q);
}
