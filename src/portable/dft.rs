//! Portable, bit-exact equivalent of Accelerate's generic (non-SME) split-complex
//! `vDSP_DFT_Execute` kernel for the 64- and 512-point `vDSP_DFT_zop` setups that
//! Apple's AAC encoder (`AudioCodecs`, `MultiRadixFFT`) creates.
//!
//! The reference is libvDSP's NEON mixed-radix kernel, reached when the setup has no
//! output-aligned kernel or an output plane is not 64-byte aligned. That is every
//! 64-point call and every 512-point call from the engine's own AAC writer: the
//! SME 512-point kernel is a different algorithm and is **not** modelled here.
//! Inputs, outputs and special values are
//! reproduced bit for bit, including operand order of each rounding step, so NaN
//! payloads and signed zeros follow the instructions; `FPCR.FZ` is honoured on
//! AArch64 (the instructions follow the ambient FPCR) and emulated through the
//! `arm` helpers on other architectures (mode detected once per transform, exactly
//! like `fft.rs`; default-NaN mode is assumed off there).
//!
//! # Structure (recovered from libvDSP disassembly and symbolic lifting)
//!
//! The kernel computes one transform `W_k = sum w_n e^{+2 pi i nk/N}` on planes
//! `(P, Q)` and reaches both vDSP directions by plane choice: forward passes the
//! imaginary plane as `P` and the real plane as `Q`, inverse the other way round.
//! The stages (radix 8 for 512 / radix 4 for 64, then radix-4 stages, last one with
//! a bit-reversed unscramble) are:
//!
//! | stage | 512 points | 64 points |
//! |---|---|---|
//! | 1 | radix 8, no twiddles, stride 64 | radix 4, no twiddles, stride 16 |
//! | 2 | 8 blocks of 64: block 0 plain radix 4, blocks 1..7 twiddled radix 4 | - |
//! | 3 | 32 blocks of 16: block 0 plain, block 1 pi/4 special form, others twiddled | 4 blocks of 16 (same) |
//! | 4 | 128 columns: twiddled radix 4 over rows of 4, output bit-reversed | 16 columns (same) |
//!
//! A twiddled radix-4 butterfly is stored as the six binary32 values
//! `[cos t, tan t, cos 2t, tan 2t, cos 3t / cos t, tan 3t]` computed in binary64 and
//! narrowed; stage blocks use `t = 2 pi bitrev(b) / (4 * blocks)` and the final
//! columns `t = 2 pi k / N`. The multiplications are fused (`fmla`/`fmls`) exactly
//! as in the machine code.

#[cfg(not(target_arch = "aarch64"))]
use crate::portable::arm;
use crate::portable::math;

/// `vDSP_DFT_FORWARD` (+1) or `vDSP_DFT_INVERSE` (-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Inverse,
}

/// `[cos t, tan t, cos 2t, tan 2t, cos 3t / cos t, tan 3t]`.
type Rotation = [f32; 6];

fn rotation(theta: f64) -> Rotation {
    let c1 = math::cos(theta);
    [
        c1 as f32,
        math::tan(theta) as f32,
        math::cos(2.0 * theta) as f32,
        math::tan(2.0 * theta) as f32,
        (math::cos(3.0 * theta) / c1) as f32,
        math::tan(3.0 * theta) as f32,
    ]
}

#[inline]
fn bit_reverse(value: usize, bits: u32) -> usize {
    if bits == 0 { 0 } else { value.reverse_bits() >> (usize::BITS - bits) }
}

// ---------------------------------------------------------------- arithmetic

/// Scalar AArch64 floating-point instructions with the operand order fixed by inline
/// assembly. Plain Rust operators would be sufficient for the numeric result, but LLVM is
/// free to commute `fadd`/`fmul` operands and to move a negation from `fma(-a, b, c)` to
/// `fma(a, -b, c)`; both change which NaN payload or sign the hardware propagates, and the
/// reference code propagates them in instruction order (`fmls` negates its first multiplicand,
/// NaNs included). The instructions follow the ambient FPCR (FZ, DN) like the NEON originals.
#[cfg(target_arch = "aarch64")]
mod hw {
    use core::arch::asm;

    macro_rules! binary {
        ($name:ident, $mnemonic:literal) => {
            #[inline(always)]
            pub fn $name(a: f32, b: f32) -> f32 {
                let r: f32;
                // SAFETY: register-only arithmetic instruction.
                unsafe {
                    asm!(concat!($mnemonic, " {r:s}, {a:s}, {b:s}"),
                        r = lateout(vreg) r, a = in(vreg) a, b = in(vreg) b,
                        options(nomem, nostack, preserves_flags));
                }
                r
            }
        };
    }
    macro_rules! fused {
        ($name:ident, $mnemonic:literal) => {
            /// `$mnemonic r, n, m, a`
            #[inline(always)]
            pub fn $name(n: f32, m: f32, a: f32) -> f32 {
                let r: f32;
                // SAFETY: register-only arithmetic instruction.
                unsafe {
                    asm!(concat!($mnemonic, " {r:s}, {n:s}, {m:s}, {a:s}"),
                        r = lateout(vreg) r, n = in(vreg) n, m = in(vreg) m, a = in(vreg) a,
                        options(nomem, nostack, preserves_flags));
                }
                r
            }
        };
    }
    binary!(fadd, "fadd");
    binary!(fsub, "fsub");
    binary!(fmul, "fmul");
    fused!(fmadd, "fmadd");
    fused!(fmsub, "fmsub");
}

/// Rounding steps in the order the NEON code performs them. On AArch64 these are
/// the hardware instructions (and follow the caller's FPCR); elsewhere they are the
/// ARM emulations selected by the detected flush mode.
#[derive(Clone, Copy)]
struct Fp {
    #[cfg(not(target_arch = "aarch64"))]
    mode: arm::FpMode,
}

impl Fp {
    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    fn current() -> Self { Self {} }

    #[cfg(not(target_arch = "aarch64"))]
    #[inline]
    fn current() -> Self {
        let min_normal = std::hint::black_box(f32::MIN_POSITIVE);
        let half = std::hint::black_box(0.5f32);
        Self {
            mode: arm::FpMode {
                flush_to_zero: (min_normal * half).to_bits() & 0x7fff_ffff == 0,
                default_nan: false,
            },
        }
    }

    /// `fadd`
    #[inline(always)]
    fn add(self, a: f32, b: f32) -> f32 {
        #[cfg(target_arch = "aarch64")]
        { hw::fadd(a, b) }
        #[cfg(not(target_arch = "aarch64"))]
        { arm::fadd_f32_with_mode(a, b, self.mode) }
    }

    /// `fsub`
    #[inline(always)]
    fn sub(self, a: f32, b: f32) -> f32 {
        #[cfg(target_arch = "aarch64")]
        { hw::fsub(a, b) }
        #[cfg(not(target_arch = "aarch64"))]
        { arm::fsub_f32_with_mode(a, b, self.mode) }
    }

    /// `fmul`
    #[inline(always)]
    fn mul(self, a: f32, b: f32) -> f32 {
        #[cfg(target_arch = "aarch64")]
        { hw::fmul(a, b) }
        #[cfg(not(target_arch = "aarch64"))]
        { arm::fmul_f32_with_mode(a, b, self.mode) }
    }

    /// `fmla acc, a, b`: `acc + a * b` with a single rounding.
    #[inline(always)]
    fn fma(self, a: f32, b: f32, acc: f32) -> f32 {
        #[cfg(target_arch = "aarch64")]
        { hw::fmadd(a, b, acc) }
        #[cfg(not(target_arch = "aarch64"))]
        { arm::fmadd_f32_with_mode(a, b, acc, self.mode) }
    }

    /// `fmls acc, a, b`: `acc - a * b` with a single rounding (the multiplicand is negated).
    #[inline(always)]
    fn fms(self, a: f32, b: f32, acc: f32) -> f32 {
        #[cfg(target_arch = "aarch64")]
        { hw::fmsub(a, b, acc) }
        #[cfg(not(target_arch = "aarch64"))]
        { arm::fmadd_f32_with_mode(-a, b, acc, self.mode) }
    }
}

// ---------------------------------------------------------------- butterflies

/// One element of the kernel's complex pair: plane `P` and plane `Q`.
#[derive(Clone, Copy)]
struct Cx {
    p: f32,
    q: f32,
}

/// Radix-4 butterfly with no twiddles (first stages, block 0 of later stages).
#[inline(always)]
fn radix4_plain(fp: Fp, x: [Cx; 4]) -> [Cx; 4] {
    let a = fp.add(x[0].p, x[2].p);
    let b = fp.add(x[1].p, x[3].p);
    let c = fp.sub(x[0].p, x[2].p);
    let d = fp.sub(x[1].q, x[3].q);
    let e = fp.add(x[0].q, x[2].q);
    let f = fp.add(x[1].q, x[3].q);
    let g = fp.sub(x[0].q, x[2].q);
    let h = fp.sub(x[1].p, x[3].p);
    [
        Cx { p: fp.add(a, b), q: fp.add(e, f) },
        Cx { p: fp.sub(a, b), q: fp.sub(e, f) },
        Cx { p: fp.sub(c, d), q: fp.add(g, h) },
        Cx { p: fp.add(c, d), q: fp.sub(g, h) },
    ]
}

/// First 512-point stage: radix 8 without twiddles; the 1/sqrt(2) factor is a plain `fmul`.
#[inline(always)]
fn radix8_plain(fp: Fp, x: [Cx; 8]) -> [Cx; 8] {
    let c = f32::from_bits(0x3f35_04f3);
    let pe: [f32; 4] = std::array::from_fn(|k| fp.add(x[k].p, x[k + 4].p));
    let pd: [f32; 4] = std::array::from_fn(|k| fp.sub(x[k].p, x[k + 4].p));
    let qe: [f32; 4] = std::array::from_fn(|k| fp.add(x[k].q, x[k + 4].q));
    let qd: [f32; 4] = std::array::from_fn(|k| fp.sub(x[k].q, x[k + 4].q));
    let even = radix4_plain(
        fp,
        [
            Cx { p: pe[0], q: qe[0] },
            Cx { p: pe[1], q: qe[1] },
            Cx { p: pe[2], q: qe[2] },
            Cx { p: pe[3], q: qe[3] },
        ],
    );
    // Odd outputs 4/5 and 6/7 share a pair of 45-degree rotations of the inputs.
    let a = fp.sub(pd[0], qd[2]);
    let b = fp.add(qd[0], pd[2]);
    let t = fp.sub(pd[1], qd[3]);
    let u = fp.add(qd[1], pd[3]);
    let um = fp.mul(fp.sub(t, u), c);
    let vm = fp.mul(fp.add(t, u), c);
    let a2 = fp.add(pd[0], qd[2]);
    let b2 = fp.sub(qd[0], pd[2]);
    let t2 = fp.add(pd[1], qd[3]);
    let u2 = fp.sub(qd[1], pd[3]);
    let u2m = fp.mul(fp.add(t2, u2), c);
    let v2m = fp.mul(fp.sub(t2, u2), c);
    [
        even[0],
        even[1],
        even[2],
        even[3],
        Cx { p: fp.add(a, um), q: fp.add(b, vm) },
        Cx { p: fp.sub(a, um), q: fp.sub(b, vm) },
        Cx { p: fp.sub(a2, u2m), q: fp.add(b2, v2m) },
        Cx { p: fp.add(a2, u2m), q: fp.sub(b2, v2m) },
    ]
}

/// Twiddled radix-4 butterfly: inputs 1..3 are rotated by `(1 + i tan)` factors and
/// recombined with the cosine scales, all with fused multiply-adds.
#[inline(always)]
fn radix4_twiddled(fp: Fp, x: [Cx; 4], w: &Rotation) -> [Cx; 4] {
    let [c0, t1, c2, t3, c4, t5] = *w;
    let u1r = fp.fms(x[1].q, t1, x[1].p);
    let u1i = fp.fma(x[1].p, t1, x[1].q);
    let u2r = fp.fms(x[2].q, t3, x[2].p);
    let u2i = fp.fma(x[2].p, t3, x[2].q);
    let u3r = fp.fms(x[3].q, t5, x[3].p);
    let u3i = fp.fma(x[3].p, t5, x[3].q);
    let s2r = fp.fma(u2r, c2, x[0].p);
    let s2i = fp.fma(u2i, c2, x[0].q);
    let zr = fp.fma(u3r, c4, u1r);
    let zi = fp.fma(u3i, c4, u1i);
    let s2r_neg = fp.fms(u2r, c2, x[0].p);
    let s2i_neg = fp.fms(u2i, c2, x[0].q);
    let zr_neg = fp.fms(u3r, c4, u1r);
    let zi_neg = fp.fms(u3i, c4, u1i);
    [
        Cx { p: fp.fma(zr, c0, s2r), q: fp.fma(zi, c0, s2i) },
        Cx { p: fp.fms(zr, c0, s2r), q: fp.fms(zi, c0, s2i) },
        Cx { p: fp.fms(zi_neg, c0, s2r_neg), q: fp.fma(zr_neg, c0, s2i_neg) },
        Cx { p: fp.fma(zi_neg, c0, s2r_neg), q: fp.fms(zr_neg, c0, s2i_neg) },
    ]
}

/// The pi/4 butterfly of the third stage's block 1: the three twiddles are
/// `e^{i pi/4}`, `i` and `e^{3 i pi/4}`, so only `cos(pi/4)` is multiplied (fused).
#[inline(always)]
fn radix4_eighth(fp: Fp, x: [Cx; 4], c: f32) -> [Cx; 4] {
    let s1 = fp.sub(fp.sub(x[1].p, x[1].q), fp.add(x[3].p, x[3].q));
    let t1 = fp.sub(x[0].p, x[2].q);
    let s2 = fp.sub(fp.add(x[1].q, x[1].p), fp.sub(x[3].q, x[3].p));
    let t2 = fp.add(x[0].q, x[2].p);
    let s3 = fp.add(fp.add(x[1].q, x[1].p), fp.sub(x[3].q, x[3].p));
    let t3 = fp.add(x[0].p, x[2].q);
    let s4 = fp.add(fp.sub(x[1].p, x[1].q), fp.add(x[3].p, x[3].q));
    let t4 = fp.sub(x[0].q, x[2].p);
    [
        Cx { p: fp.fma(s1, c, t1), q: fp.fma(s2, c, t2) },
        Cx { p: fp.fms(s1, c, t1), q: fp.fms(s2, c, t2) },
        Cx { p: fp.fms(s3, c, t3), q: fp.fma(s4, c, t4) },
        Cx { p: fp.fma(s3, c, t3), q: fp.fms(s4, c, t4) },
    ]
}

#[derive(Clone, Copy)]
enum Block<'a> {
    Plain,
    Eighth(f32),
    Twiddled(&'a Rotation),
}

/// In-place radix-4 pass over `4 * m` elements: elements `j + k*m` (k < 4) are combined and
/// written back at `q*m + j`, where output `q` is the butterfly's q-th result.
#[inline(always)]
fn radix4_block(fp: Fp, p: &mut [f32], q: &mut [f32], m: usize, kind: Block<'_>) {
    for j in 0..m {
        let x: [Cx; 4] = std::array::from_fn(|k| Cx { p: p[j + k * m], q: q[j + k * m] });
        let y = match kind {
            Block::Plain => radix4_plain(fp, x),
            Block::Eighth(c) => radix4_eighth(fp, x, c),
            Block::Twiddled(w) => radix4_twiddled(fp, x, w),
        };
        for (k, v) in y.iter().enumerate() {
            p[k * m + j] = v.p;
            q[k * m + j] = v.q;
        }
    }
}

// ---------------------------------------------------------------- setup and execution

/// A reusable setup for one length and direction (the analogue of `vDSP_DFT_Setup`
/// from `vDSP_DFT_zop_CreateSetup`). Supported lengths are the AAC encoder's 64 and 512.
#[derive(Clone, Debug)]
pub struct DftSetup {
    len: usize,
    direction: Direction,
    /// One rotation per 16-element block of stages 2 and 3 (index `b`).
    blocks: Vec<Rotation>,
    /// One rotation per final-stage column (index `k`, `t = 2 pi k / len`).
    columns: Vec<Rotation>,
}

impl DftSetup {
    /// Equivalent of `vDSP_DFT_zop_CreateSetup(NULL, len, direction)` for 64 or 512
    /// points; other lengths are not modelled and return `None`.
    pub fn new(len: usize, direction: Direction) -> Option<Self> {
        if len != 64 && len != 512 {
            return None;
        }
        let block_count = len / 16;
        let block_bits = block_count.trailing_zeros();
        let blocks = (0..block_count)
            .map(|b| {
                let exponent = bit_reverse(b, block_bits);
                rotation(2.0 * std::f64::consts::PI * exponent as f64 / (4 * block_count) as f64)
            })
            .collect();
        let columns = (0..len / 4)
            .map(|k| rotation(2.0 * std::f64::consts::PI * k as f64 / len as f64))
            .collect();
        Some(Self { len, direction, blocks, columns })
    }

    /// The twiddles of the 16-element block stages: element `b` is
    /// `[cos t, tan t, cos 2t, tan 2t, cos 3t / cos t, tan 3t]` with `t = 2 pi bitrev(b) / (4 * len / 16)`.
    pub fn block_twiddles(&self) -> &[[f32; 6]] { &self.blocks }

    /// The twiddles of the final stage: element `k` uses `t = 2 pi k / len` (`k < len / 4`).
    pub fn column_twiddles(&self) -> &[[f32; 6]] { &self.columns }

    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }
    pub fn direction(&self) -> Direction { self.direction }

    /// Out-of-place split-complex transform (`vDSP_DFT_Execute`): unit stride, no scaling.
    /// Forward computes `Y[k] = sum x[n] e^{-2 pi i nk/N}`, inverse the conjugate kernel.
    /// All four planes must hold exactly `len()` values.
    pub fn execute(&self, input_real: &[f32], input_imag: &[f32], output_real: &mut [f32], output_imag: &mut [f32]) {
        let n = self.len;
        assert!(
            input_real.len() == n && input_imag.len() == n && output_real.len() == n && output_imag.len() == n,
            "DFT plane length"
        );
        let fp = Fp::current();
        // The kernel is an e^{+i} transform of (P, Q); forward swaps the planes.
        let (p_in, q_in, p_out, q_out) = match self.direction {
            Direction::Forward => (input_imag, input_real, output_imag, output_real),
            Direction::Inverse => (input_real, input_imag, output_real, output_imag),
        };
        match n {
            512 => self.kernel::<512>(fp, p_in, q_in, p_out, q_out),
            _ => self.kernel::<64>(fp, p_in, q_in, p_out, q_out),
        }
    }

    #[inline(always)]
    fn kernel<const N: usize>(&self, fp: Fp, p_in: &[f32], q_in: &[f32], p_out: &mut [f32], q_out: &mut [f32]) {
        let mut p = [0.0f32; N];
        let mut q = [0.0f32; N];
        if N == 512 {
            // Stage 1: radix 8, input stride 64, output at k * 64 + j.
            for j in 0..64 {
                let x: [Cx; 8] = std::array::from_fn(|k| Cx { p: p_in[j + 64 * k], q: q_in[j + 64 * k] });
                let y = radix8_plain(fp, x);
                for (k, v) in y.iter().enumerate() {
                    p[k * 64 + j] = v.p;
                    q[k * 64 + j] = v.q;
                }
            }
            // Stage 2: eight blocks of 64; block b uses the b-th stage rotation (block 0 has none).
            for b in 0..8 {
                let (pb, qb) = (&mut p[64 * b..64 * b + 64], &mut q[64 * b..64 * b + 64]);
                let kind = if b == 0 { Block::Plain } else { Block::Twiddled(&self.blocks[b]) };
                radix4_block(fp, pb, qb, 16, kind);
            }
        } else {
            // Stage 1: radix 4, input stride 16.
            p.copy_from_slice(p_in);
            q.copy_from_slice(q_in);
            radix4_block(fp, &mut p, &mut q, 16, Block::Plain);
        }
        // Stage 3: blocks of 16 elements, butterflies across stride 4.
        for b in 0..N / 16 {
            let (pb, qb) = (&mut p[16 * b..16 * b + 16], &mut q[16 * b..16 * b + 16]);
            let kind = match b {
                0 => Block::Plain,
                1 => Block::Eighth(self.blocks[1][0]),
                _ => Block::Twiddled(&self.blocks[b]),
            };
            radix4_block(fp, pb, qb, 4, kind);
        }
        // Stage 4: column `col` combines the four consecutive elements of row
        // `bitrev(col)` and writes result `q` at `bitrev2(q) * N/4 + col`.
        let groups = N / 4;
        let bits = groups.trailing_zeros();
        for col in 0..groups {
            let g = bit_reverse(col, bits);
            let x: [Cx; 4] = std::array::from_fn(|m| Cx { p: p[4 * g + m], q: q[4 * g + m] });
            let y = radix4_twiddled(fp, x, &self.columns[col]);
            for (r, v) in y.iter().enumerate() {
                let pos = bit_reverse(r, 2) * groups + col;
                p_out[pos] = v.p;
                q_out[pos] = v.q;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unmodelled_lengths() {
        for n in [0usize, 1, 16, 32, 128, 256, 1024] {
            assert!(DftSetup::new(n, Direction::Forward).is_none());
        }
    }

    #[test]
    fn impulse_and_zero() {
        for n in [64usize, 512] {
            let setup = DftSetup::new(n, Direction::Forward).unwrap();
            let mut re = vec![0.0f32; n];
            let im = vec![0.0f32; n];
            re[0] = 1.0;
            let (mut or, mut oi) = (vec![9.0f32; n], vec![9.0f32; n]);
            setup.execute(&re, &im, &mut or, &mut oi);
            assert!(or.iter().all(|&v| v == 1.0) && oi.iter().all(|&v| v == 0.0));
        }
    }
}
