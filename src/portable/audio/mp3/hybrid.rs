//! Hybrid filterbank leaves: Apple's `_IDCT9` (0x1703e4), `_imdct36` (0x1701dc) and `_IMDCT12`
//! (0x170508), integer Q16 fixed point.
//!
//! Arithmetic conventions of the machine code, kept verbatim:
//! * `smull xd, wn, wm ; lsr xd, xd, #16` is a *logical* shift of the signed 64-bit product; only
//!   bits 16..47 of the product are ever consumed by the following 32-bit instruction (`mul16`).
//! * 32-bit `add/sub/neg` wrap (`wrapping_*`), `asr` is arithmetic.
//! * `imdct36` mixes 64-bit accumulators (`add x17, x13, w17, sxtw`, `mneg x13, x17, x16`) with 32-bit
//!   register writes; the 64-bit values are not truncated between steps.

use super::back_tables::{LONG_WINDOW, SHORT_WINDOW};

/// `smull x, wa, wb ; lsr x, x, #16` as a u64 (logical shift of the 64-bit product).
#[inline(always)]
fn mul16(a: u32, b: u32) -> u64 {
    ((a as i32 as i64).wrapping_mul(b as i32 as i64) as u64) >> 16
}

/// `_IDCT9`: 9-point inverse DCT (0x1703e4), `x0 = in`, `x1 = out`.
pub fn idct9(i: &[i32; 9]) -> [i32; 9] {
    let inp = |k: usize| i[k] as u32;
    let c_ddb4: u32 = 0xddb4;
    let w9 = inp(0);
    let w10 = inp(1);
    let w12 = inp(4);
    let w11 = inp(5);
    let w13 = w11.wrapping_add(w10);
    let w14 = inp(2);
    let w15 = inp(3);
    let w16 = w12.wrapping_add(w14);
    let w17 = inp(8);
    let w2 = w17.wrapping_add(w14);
    let w3 = inp(6);
    let w0 = inp(7);
    let w4 = w0.wrapping_add(w10);
    let w5 = w17.wrapping_sub(w12);
    let w6 = w11.wrapping_sub(w0);
    let w11 = w11.wrapping_add(w0);
    let w0 = w10.wrapping_sub(w11);
    let x10 = mul16(w15, c_ddb4);
    let w11 = w12.wrapping_add(w17);
    let w11 = w14.wrapping_sub(w11);
    let x8 = mul16(w0, c_ddb4);
    let x14 = mul16(w16, 0xc41b);
    let x17 = mul16(w2, 0x2c74);
    let x12 = mul16(w5, 0xc41b);
    let x15 = mul16(w16, 0x2c74);
    let x0 = mul16(w6, 0xa48e);
    let x4 = mul16(w4, 0xfc1c);
    let x13 = mul16(w13, 0xa48e);
    let x16 = mul16(w6, 0xfc1c);
    let w10 = x10 as u32; // `lsr x10, x10, #16` overwrote w10 (in[1] is dead from here on)
    let w2 = w9.wrapping_add(((w3 as i32) >> 1) as u32);
    let w5 = w2.wrapping_add(w10);
    let w9 = w9.wrapping_sub(w3);
    let w3 = w9.wrapping_sub(w11);
    let w9 = w9.wrapping_add(((w11 as i32) >> 1) as u32);
    let w11 = (x17 as u32).wrapping_add(x14 as u32);
    let w14 = w5.wrapping_add(w11);
    let w12 = (x12 as u32).wrapping_sub(x15 as u32);
    let w15 = w11.wrapping_add(w12);
    let w17 = (x0 as u32).wrapping_add(x4 as u32);
    let w14 = w14.wrapping_add(w17);
    let w13 = (x13 as u32).wrapping_sub(x16 as u32);
    let w16 = w13.wrapping_sub(w17);
    let w10 = w2.wrapping_sub(w10);
    let w11 = w10.wrapping_add(w11);
    let w0 = w5.wrapping_add(w12);
    let w2 = w9.wrapping_add(x8 as u32);
    let o0 = w14;
    let o1 = w2;
    let w12 = w10.wrapping_add(w12);
    let w12 = w12.wrapping_add(w13);
    let w14 = w15.wrapping_add(w16);
    let w10 = w10.wrapping_sub(w14);
    let o2 = w12;
    let o3 = w10;
    let w10 = w5.wrapping_sub(w15);
    let w10 = w10.wrapping_add(w16);
    let o4 = w3;
    let o5 = w10;
    let w10 = w0.wrapping_sub(w13);
    let w8 = w9.wrapping_sub(x8 as u32);
    let o6 = w10;
    let o7 = w8;
    let o8 = w11.wrapping_sub(w17);
    [o0 as i32, o1 as i32, o2 as i32, o3 as i32, o4 as i32, o5 as i32, o6 as i32, o7 as i32, o8 as i32]
}

/// `_imdct36(in, overlap, window, nblocks)` (0x1701dc).
///
/// `buf[base ..]` holds `nblocks` consecutive 18-int subband spectra; the 18 time samples of block `b`
/// replace the 18 ints at `buf[base + 18*b - 18 ..]` (the output lags the input by one block, which is
/// why Apple's spectrum buffer starts 18 ints after the work buffer origin; `base >= 18`).  `ovl`
/// holds 9 ints per block.  `stop_window` selects `_g_aShortWindow` for the 2nd rotation of the last 3 taps
/// instead of the long window (window pointer `!= 0x66df78`, i.e. block type 3).
///
/// Total behaviour: nothing happens for `nblocks < 1` (as in Apple) or `base < 18`; blocks that do not fit in `buf` / `ovl`
/// are not processed (Apple would run past its buffers; the front half never asks for more than 32 blocks).
pub fn imdct36(buf: &mut [i32], base: usize, ovl: &mut [i32], stop_window: bool, nblocks: i32) {
    if nblocks < 1 || base < 18 {
        return; // cmp w3,#1 ; b.lt
    }
    let fit = (buf.len().saturating_sub(base) / 18).min(ovl.len() / 9);
    for blk in 0..(nblocks as usize).min(fit) {
        let x21 = base + 18 * blk;
        let o = 9 * blk;
        // --- fold the 18 inputs into two 9-point IDCT inputs A (-> buf[x21-18..x21-9]) and B (-> buf[x21-9..x21])
        let mut inp = [0i32; 18];
        inp.copy_from_slice(&buf[x21..x21 + 18]);
        let mut a = [0i32; 9];
        let mut b = [0i32; 9];
        a[0] = inp[0].wrapping_neg();
        for i in 0..4 {
            let (p, q, r, s) = (inp[1 + 4 * i], inp[2 + 4 * i], inp[3 + 4 * i], inp[4 + 4 * i]);
            b[8 - 2 * i] = p.wrapping_sub(q);
            a[1 + 2 * i] = q.wrapping_add(p);
            b[7 - 2 * i] = s.wrapping_sub(r);
            a[2 + 2 * i] = r.wrapping_add(s).wrapping_neg();
        }
        b[0] = inp[17];
        let ya = idct9(&a);
        let yb = idct9(&b);
        buf[x21..x21 + 9].copy_from_slice(&ya);
        buf[x21 + 9..x21 + 18].copy_from_slice(&yb);
        for k in [10usize, 12, 14, 16] {
            buf[x21 + k] = buf[x21 + k].wrapping_neg();
        }
        // --- windowing: two lifting rotations per tap pair
        let mut xo = x21 - 18; // x22, ascending
        let mut xh = x21; // x9, pre-decrement
        for k in 0..9usize {
            let x13 = buf[x21 + 9 + k] as i64 as u64;
            let x14 = buf[x21 + k] as i64 as u64;
            let p = 8 - k;
            let w15 = LONG_WINDOW[2 * p] as u64; // ldrh w15,[x11]
            let w16 = LONG_WINDOW[2 * p + 1] as u64; // ldrh w16,[x11,#2]
            let mut x17 = ((w15 as i64).wrapping_mul(x14 as u32 as i32 as i64) as u64) >> 16;
            x17 = x13.wrapping_add(x17 as u32 as i32 as i64 as u64);
            let mut x13n = 0u64.wrapping_sub(x17.wrapping_mul(w16)) >> 16; // mneg x13,x17,x16 ; lsr 16
            x13n = x14.wrapping_add(x13n as u32 as i32 as i64 as u64);
            let x14n = x13n.wrapping_mul(w15) >> 16;
            let mut w15 = (x17 as u32).wrapping_add(x14n as u32);
            let mut w14 = ovl[o + k] as u32;
            if !(stop_window && k < 6) {
                let (c, s) = if stop_window {
                    let q = k - 6;
                    (SHORT_WINDOW[2 * q] as u32, SHORT_WINDOW[2 * q + 1] as u32)
                } else {
                    (LONG_WINDOW[2 * k] as u32, LONG_WINDOW[2 * k + 1] as u32)
                };
                let t0 = mul16(c, w14);
                w15 = w15.wrapping_add(t0 as u32);
                let w0 = w15.wrapping_neg();
                let t1 = mul16(s, w0);
                w14 = w14.wrapping_add(t1 as u32);
                let t2 = mul16(c, w14);
                w15 = w15.wrapping_add(t2 as u32);
            }
            buf[xo] = w14 as i32;
            xo += 1;
            xh -= 1;
            buf[xh] = w15 as i32;
            ovl[o + k] = x13n as u32 as i32;
        }
    }
}

/// `_IMDCT12(in, out, ovl)` (0x170508): one 12-point window of a short block.  Reads `inp[0,1,6,7,12,13]`,
/// writes `out[0..6]` and updates `ovl[0..3]`.  Total behaviour: does nothing unless `inp.len() >= 14`, `out.len() >= 6`
/// and `ovl.len() >= 3`.
pub fn imdct12(inp: &[i32], out: &mut [i32], ovl: &mut [i32]) {
    if inp.len() < 14 || out.len() < 6 || ovl.len() < 3 {
        return;
    }
    let i = |k: usize| inp[k] as u32;
    let c_ddb4: u32 = 0xddb4;
    let w9 = i(0);
    let w11 = i(1);
    let w12 = i(6);
    let w10 = i(7);
    let w13 = w11.wrapping_add(w12);
    let x13 = mul16(w13, c_ddb4);
    let w15 = i(12);
    let w16 = i(13);
    let w17 = w10.wrapping_add(w15);
    let w14 = w17.wrapping_sub(w9);
    let w9 = w9.wrapping_add(((w17 as i32) >> 1) as u32);
    let w17 = (x13 as u32).wrapping_sub(w9);
    let w9 = w15.wrapping_sub(w10);
    let x10 = mul16(w9, c_ddb4);
    let w8 = w11.wrapping_sub(w12);
    let w15 = w8.wrapping_sub(w16);
    let w11 = w16.wrapping_add(((w8 as i32) >> 1) as u32);
    let c_56e6: u32 = 0x56e6;
    let x9 = mul16(w17, c_56e6);
    let w12 = w11.wrapping_add(x10 as u32);
    let w16 = w12.wrapping_add(x9 as u32);
    let w12 = w16.wrapping_neg();
    let c_9bd8: u32 = 0x9bd8;
    let x12 = mul16(w12, c_9bd8);
    let w0 = w17.wrapping_add(x12 as u32);
    let w12 = w17.wrapping_sub((x13 as u32) << 1);
    let x17 = mul16(w0, c_56e6);
    let x3 = ovl[0] as i64 as u64;
    let c_10c7: u32 = 0x10c7;
    let x4 = mul16(x3 as u32, c_10c7);
    let w16 = w16.wrapping_add(x4 as u32);
    let w16 = w16.wrapping_add(x17 as u32);
    let w17 = w16.wrapping_neg();
    let c_216a: u32 = 0x216a;
    let x17 = ((w17 as i32 as i64).wrapping_mul(c_216a as i64)) as u64; // smull x17,w17,w4 (no shift)
    let x17 = x3.wrapping_add(((x17 as i64) >> 16) as u64);
    let x3 = x17.wrapping_mul(c_10c7 as u64) >> 16;
    let w16 = w16.wrapping_add(x3 as u32);
    out[0] = x17 as u32 as i32;
    out[5] = w16 as i32;
    ovl[0] = w0 as i32;
    let c_32ec: u32 = 0x32ec;
    let x17 = mul16(w14, c_32ec);
    let w15 = w15.wrapping_add(x17 as u32);
    let w17 = w15.wrapping_neg();
    let c_61f8: u32 = 0x61f8;
    let x17 = mul16(w17, c_61f8);
    let w14 = w14.wrapping_add(x17 as u32);
    let x17 = mul16(w14, c_32ec);
    let x3 = ovl[1] as i64 as u64;
    let x5 = mul16(x3 as u32, c_32ec);
    let w15 = w15.wrapping_add(x5 as u32);
    let w15 = w15.wrapping_add(x17 as u32);
    let w17 = w15.wrapping_neg();
    let x17 = ((w17 as i32 as i64).wrapping_mul(c_61f8 as i64)) as u64;
    let x17 = x3.wrapping_add(((x17 as i64) >> 16) as u64);
    let x16 = x17.wrapping_mul(c_32ec as u64) >> 16;
    let w15 = w15.wrapping_add(x16 as u32);
    out[1] = x17 as u32 as i32;
    out[4] = w15 as i32;
    ovl[1] = w14 as i32;
    let x14 = mul16(w12, c_10c7);
    let w10 = w11.wrapping_sub(x10 as u32);
    let w10 = w10.wrapping_add(x14 as u32);
    let w11 = w10.wrapping_neg();
    let x11 = mul16(w11, c_216a);
    let w11 = w12.wrapping_add(x11 as u32);
    let x12 = mul16(w11, c_10c7);
    let x13 = ovl[2] as i64 as u64;
    let x14 = mul16(x13 as u32, c_56e6);
    let w10 = w10.wrapping_add(x14 as u32);
    let w10 = w10.wrapping_add(x12 as u32);
    let w12 = w10.wrapping_neg();
    let x9 = mul16(w12, c_9bd8);
    let x9 = x13.wrapping_add(x9 as u32 as i32 as i64 as u64);
    let x8 = x9.wrapping_mul(c_56e6 as u64) >> 16;
    let w8 = w10.wrapping_add(x8 as u32);
    out[2] = x9 as u32 as i32;
    out[3] = w8 as i32;
    ovl[2] = w11 as i32;
}
