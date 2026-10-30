//! Private models of the Accelerate/vDSP primitives that the psychoacoustic code calls (macOS 27.2, Apple M4), checked against the library
//! itself.  All functions are plain scalar Rust: identical bits on every host, no intrinsics.
//!
//! * element-wise and UNFUSED (single IEEE operation per element): `vsq`, `vsmul_inplace`; `vDSP_vthr(A,s)` is `FMAX(A, s)`.  (`vDSP_vmin`/`vmax` are
//!   `crate::portable::arm::{min_propagating, max_propagating}` with swapped operands; `vmul`/`vsadd` are plain `*`/`+`.)
//! * `vDSP_zvmags`: body in groups of 4 lanes UNFUSED (`re*re + im*im`, two roundings), head elements (until the OUTPUT pointer is
//!   16-byte aligned) and tail elements (`n mod 4` after the body) scalar FUSED `fma(re, re, im*im)`; input alignment is irrelevant.
//! * `vDSP_sve` (stride 1): summation tree below (alignment independent).
//!
//! The engine's own `crate::portable::vdsp` models the unsafe pointer-and-stride entry points; `dotpr` and `maxvi` come from there.
//! `vsq`, `vsmul_inplace` and `zvmags` stay here: the first two are one IEEE multiply per element (the engine's `vDSP_vmul`/`vDSP_vsmul` give the same
//! bits but would need `unsafe` or new wrappers for no gain), and the engine has no `vDSP_zvmags`.

use crate::portable::arm::max_propagating;

/// `vDSP_vsq(A, 1, C, 1, n)`: `C[i] = A[i] * A[i]`.
#[inline]
pub fn vsq(a: &[f32], c: &mut [f32]) {
    for (o, &x) in c.iter_mut().zip(a) {
        *o = x * x;
    }
}

#[inline]
pub fn vsmul_inplace(a: &mut [f32], s: f32) {
    for x in a.iter_mut() {
        *x *= s;
    }
}

/// `vDSP_vthr(A, 1, &s, C, 1, n)` in place: `C[i] = max(A[i], s)` with ARM `FMAX` semantics (NaNs propagate, `-0 < +0`).  Identical to the
/// library for every NaN-free vector; which NaN payload the library keeps depends on the position inside the vector.
#[inline]
pub fn vthr_inplace(a: &mut [f32], s: f32) {
    for x in a.iter_mut() {
        *x = max_propagating(*x, s);
    }
}

/// `vDSP_zvmags(&{re,im}, 1, C, 1, n)`: `C[i] = re[i]^2 + im[i]^2` with the library's fused/unfused pattern.
/// `out_align` = (address of `C[0]` / 4) mod 4, i.e. the number of floats the output pointer is past a 16-byte boundary
/// (0 for the engine's malloc'd power-spectrum buffers).  head = `(4 - out_align) % 4` scalar fused elements, then groups of 4
/// unfused lanes, then the remaining `< 4` elements fused (measured on the library for `n = 12`, output offset 3: fused head, 8 unfused, 1 fused tail).
pub fn zvmags(re: &[f32], im: &[f32], out: &mut [f32], out_align: usize) {
    let n = out.len();
    let head = ((4 - (out_align & 3)) & 3).min(n);
    for i in 0..head {
        out[i] = re[i].mul_add(re[i], im[i] * im[i]);
    }
    let body = ((n - head) / 4) * 4;
    for i in head..head + body {
        out[i] = re[i] * re[i] + im[i] * im[i];
    }
    for i in head + body..n {
        out[i] = re[i].mul_add(re[i], im[i] * im[i]);
    }
}

#[inline(always)]
fn add4(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]]
}
#[inline(always)]
fn ld(a: &[f32], i: usize) -> [f32; 4] {
    [a[i], a[i + 1], a[i + 2], a[i + 3]]
}

#[inline(always)]
fn sve_tail(a: &[f32], v0: [f32; 4], mut p: usize, x3: i64) -> f32 {
    let mut v0 = v0;
    if x3 != 0 {
        let mut v1 = [0.0f32; 4];
        let mut v2 = [0.0f32; 4];
        if x3 & 2 != 0 {
            v1[0] = a[p];
            v1[1] = a[p + 1];
            p += 2;
        }
        if x3 & 1 != 0 {
            v2[0] = a[p];
        }
        v0 = add4(v0, v1);
        v0 = add4(v0, v2);
    }
    let h0 = v0[0] + v0[1];
    let h1 = v0[2] + v0[3];
    h0 + h1
}

/// `vDSP_sve(a, 1, &r, n)` of macOS 27.2 (arm64e libvDSP): exact summation tree transcribed from the disassembly
/// (n<16: one 4-lane accumulator; 16..31: two; 32..127: four; >=128: eight accumulators + epilogue; scalar tail lanes; final `(l0+l1)+(l2+l3)`).
/// Independent of pointer alignment.  Checked against the library on random, subnormal, extreme-magnitude and signed-zero inputs (e.g. a sum of `-0.0`
/// is `-0.0`); which payload a propagated NaN keeps is not reproduced.
pub fn sve(a: &[f32]) -> f32 {
    let n = a.len();
    if n == 0 {
        return 0.0;
    }
    let mut v = [[0.0f32; 4]; 24];
    let mut p: usize = 0;
    let mut x3: i64 = n as i64;
    if x3 < 4 {
        return sve_tail(a, v[0], p, x3);
    }
    v[0] = ld(a, p);
    p += 4;
    x3 -= 4;
    if x3 == 0 {
        return sve_tail(a, v[0], p, x3);
    }
    x3 -= 4;
    if x3 < 0 {
        return sve_tail(a, v[0], p, x3 + 4);
    }
    v[1] = ld(a, p);
    p += 4;
    if x3 >= 8 {
        v[2] = ld(a, p);
        v[3] = ld(a, p + 4);
        p += 8;
        x3 -= 0x18;
        if x3 >= 0 {
            v[4] = ld(a, p);
            v[5] = ld(a, p + 4);
            p += 16;
            x3 -= 0x20;
            v[6] = ld(a, p - 8);
            v[7] = ld(a, p - 4);
            if x3 >= 0 {
                v[16] = ld(a, p);
                p += 32;
                v[0] = add4(v[0], v[4]);
                v[17] = ld(a, p - 28);
                v[1] = add4(v[1], v[5]);
                v[18] = ld(a, p - 24);
                v[2] = add4(v[2], v[6]);
                v[19] = ld(a, p - 20);
                v[3] = add4(v[3], v[7]);
                x3 -= 0x20;
                if x3 >= 0 {
                    for kk in 20..24 {
                        v[kk] = [0.0; 4];
                    }
                    loop {
                        v[4] = ld(a, p - 16);
                        v[0] = add4(v[0], v[16]);
                        v[5] = ld(a, p - 12);
                        v[1] = add4(v[1], v[17]);
                        v[6] = ld(a, p - 8);
                        v[2] = add4(v[2], v[18]);
                        v[7] = ld(a, p - 4);
                        v[3] = add4(v[3], v[19]);
                        v[16] = ld(a, p);
                        p += 32;
                        v[20] = add4(v[20], v[4]);
                        v[17] = ld(a, p - 28);
                        v[21] = add4(v[21], v[5]);
                        v[18] = ld(a, p - 24);
                        v[22] = add4(v[22], v[6]);
                        v[19] = ld(a, p - 20);
                        v[23] = add4(v[23], v[7]);
                        x3 -= 0x20;
                        if x3 < 0 {
                            break;
                        }
                    }
                    v[0] = add4(v[0], v[20]);
                    v[1] = add4(v[1], v[21]);
                    v[2] = add4(v[2], v[22]);
                    v[3] = add4(v[3], v[23]);
                }
                v[4] = ld(a, p - 16);
                v[0] = add4(v[0], v[16]);
                v[5] = ld(a, p - 12);
                v[1] = add4(v[1], v[17]);
                v[6] = ld(a, p - 8);
                v[2] = add4(v[2], v[18]);
                v[7] = ld(a, p - 4);
                v[3] = add4(v[3], v[19]);
            }
            x3 += 0x10;
            v[0] = add4(v[0], v[4]);
            v[1] = add4(v[1], v[5]);
            v[2] = add4(v[2], v[6]);
            v[3] = add4(v[3], v[7]);
        }
        x3 += 0x10;
        v[0] = add4(v[0], v[2]);
        v[1] = add4(v[1], v[3]);
    }
    v[0] = add4(v[0], v[1]);
    x3 -= 4;
    if x3 >= 0 {
        loop {
            v[1] = ld(a, p);
            p += 4;
            v[0] = add4(v[0], v[1]);
            x3 -= 4;
            if x3 < 0 {
                break;
            }
        }
    }
    x3 += 4;
    sve_tail(a, v[0], p, x3)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sign of a sum of negative zeros as `vDSP_sve` returns it (recorded on the reference machine): `-0.0` for 4, 8, 12, 16, 32 and 64
    /// elements, `+0.0` for every other length tried.
    #[test]
    fn sve_sign_of_negative_zero_sums() {
        for n in [0usize, 1, 2, 3, 5, 6, 7, 15, 17, 31, 33, 127, 128, 129, 200] {
            assert_eq!(sve(&vec![-0.0; n]).to_bits(), 0, "n = {n}");
        }
        for n in [4usize, 8, 12, 16, 32, 64] {
            assert_eq!(sve(&vec![-0.0; n]).to_bits(), 0x8000_0000, "n = {n}");
        }
    }

    #[test]
    fn vthr_is_fmax() {
        let mut v = [-0.0f32, 0.5, 1.0, 2.0, f32::from_bits(1), f32::NAN, -5.0];
        vthr_inplace(&mut v, 1.0);
        assert_eq!(v[..5].iter().map(|x| x.to_bits()).collect::<Vec<_>>(), [1.0f32, 1.0, 1.0, 2.0, 1.0].map(f32::to_bits));
        assert!(v[5].is_nan());
        assert_eq!(v[6].to_bits(), 1.0f32.to_bits());
        let mut z = [-0.0f32, 0.0];
        vthr_inplace(&mut z, 0.0);
        assert_eq!(z.map(f32::to_bits), [0, 0]);
    }
}
