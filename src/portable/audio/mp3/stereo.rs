//! Stereo processing leaves: Apple's `_mp3d_MSStereo` (0x23d9c).  The intensity-stereo / per-band glue that
//! calls it lives in `back.rs` (it is inlined in `DecodeFrame`, 0x13f23c..0x13f4a8).

/// `_mp3d_MSStereo(a, n)`: `for i in 0..n { m = a[i]; s = a[i + 0x240]; a[i] = m + s; a[i + 0x240] = m - s }`
/// (32-bit wrapping), with the exact loop shape of the machine code: a *do-while* that always executes
/// the body once and then continues while `n - 1 > 0` (signed `b.gt` after `subs`), i.e. `n <= 1` processes one element.
/// `base` is the int index of `a[0]` inside `buf`; the second channel is at `base + 0x240`.
///
/// Total behaviour: Apple would run into whatever memory follows the buffers when `n` is larger than the spectrum;
/// here the loop stops at the first element whose pair does not lie inside `buf` (nothing happens if even the first
/// does not).  Real streams never call it with more than 576 elements per channel.
pub fn ms_stereo(buf: &mut [i32], base: usize, n: i32) {
    let mut i = base;
    let mut w2 = n;
    loop {
        let Some(j) = i.checked_add(0x240) else {
            break;
        };
        if j >= buf.len() {
            break;
        }
        let m = buf[i] as u32;
        let s = buf[j] as u32;
        let sum = m.wrapping_add(s);
        let diff = sum.wrapping_sub(s << 1); // sub w11,w5,w11,lsl #1
        buf[i] = sum as i32;
        buf[j] = diff as i32;
        i += 1;
        // subs w2,w2,#1 ; b.gt
        let (res, ov) = w2.overflowing_sub(1);
        w2 = res;
        let gt = res != 0 && ((res < 0) == ov);
        if !gt {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ms_stereo;

    #[test]
    fn matches_definition_and_is_total() {
        let orig: Vec<i32> = (0..0x500).map(|i| (i as i32).wrapping_mul(0x0123_4567)).collect();
        let mut buf = orig.clone();
        ms_stereo(&mut buf, 3, 5);
        for k in 0..5 {
            let (m, s) = (orig[3 + k], orig[3 + k + 0x240]);
            assert_eq!(buf[3 + k], m.wrapping_add(s));
            assert_eq!(buf[3 + k + 0x240], m.wrapping_sub(s));
        }
        assert_eq!(buf[8], orig[8]);
        // do-while quirk: n <= 1 processes exactly one element
        for n in [i32::MIN, -1, 0, 1] {
            let mut b = orig.clone();
            ms_stereo(&mut b, 0, n);
            assert_eq!(b[0], orig[0].wrapping_add(orig[0x240]));
            assert_eq!(b[1], orig[1]);
        }
        // out of range: clamped to the elements whose pair exists, never panics
        let mut b = orig.clone();
        ms_stereo(&mut b, 0x4f0, 5);
        assert_eq!(b, orig);
        ms_stereo(&mut b, usize::MAX, i32::MAX);
        assert_eq!(b, orig);
        ms_stereo(&mut b, 0x100, i32::MAX);
        let last = 0x500 - 0x240 - 1;
        assert_eq!(b[last], orig[last].wrapping_add(orig[last + 0x240]));
        assert_eq!(b[last + 1], orig[last + 1]);
    }
}
