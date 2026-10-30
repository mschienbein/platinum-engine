//! Constant tables of the decoder stage (extracted from, or derived from, Apple's AudioCodecs binary by static analysis).
//!
//! The binary blobs in `data/` hold the values of those tables byte for byte; each is embedded with `include_bytes!`.

use crate::portable::aac::bits::tables as iso;

const fn load_f32<const N: usize>(bytes: &[u8]) -> [f32; N] {
    assert!(bytes.len() == N * 4);
    let mut out = [0f32; N];
    let mut i = 0;
    while i < N {
        out[i] = f32::from_bits(u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]]));
        i += 1;
    }
    out
}

/// `POW43[n]` for `0 <= n < 8192`: entries `< 1024` are Apple's table at 0x9434d0 (entry 2 is `0x40214517`, NOT correctly rounded),
/// entries `>= 1024` are `(float)pow((double)n, 4/3)` (libm; correctly rounded).  Enough for every `|q| <= 8191` (escape prefix capped at 8).
pub static POW43: [f32; 8192] = load_f32(include_bytes!("data/pow43_8192.f32"));

/// `PULSEF[i] = powf(i, 1.3333334f)` for `i < 8208` (the float pulse table: NOT `POW43`; 8188 of 8192 entries differ).
pub static PULSEF: [f32; 8208] = load_f32(include_bytes!("../../bits/data/powf43_8208.f32"));

/// `kSinCoefs` (0x9444d0), indexed `(coef + 8) | (coef_res << 4)`.
pub static SIN_COEFS: [f32; 32] = load_f32(include_bytes!("data/kSinCoefs_32.f32"));

/// `kNormFactorTable` (0x80ea08): `float(4e-10 / sqrt(n))` for `n = 1..=32`.
pub static NORM_FACTOR: [f32; 32] = load_f32(include_bytes!("data/kNormFactorTable_32.f32"));

/// `kPow2_025` (0x8f50d0): bit patterns of `2^(k/4)`, `k = 0..3`.
pub const POW2_025_BITS: [u32; 4] = [0x3f80_0000, 0x3f98_37f0, 0x3fb5_04f3, 0x3fd7_44fd];

/// Initial PNS LCG lanes (0x923d40), loaded into every channel's ICS at construction and never re-initialised.
pub const PNS_SEED: [u32; 4] = [0x3c6e_f35f, 0x4750_2932, 0xd1cc_f6e9, 0xaaf9_5334];
/// PNS LCG: `s = s * PNS_MUL + PNS_ADD` (u32 wrap) per lane.
pub const PNS_MUL: u32 = 0x0979_e791;
pub const PNS_ADD: u32 = 0xaaf9_5334;

/// `kTNSMaxBandsAOT_2` (0x80ecd0): maximum number of bands TNS may touch, per sampling-frequency index 0..=11.
pub const TNS_MAX_BANDS_LONG: [u8; 12] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39];
pub const TNS_MAX_BANDS_SHORT: [u8; 12] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14];

/// Scalefactor-band offsets (`kAACSFBOffsetTable` 0x9b4810 == ISO `swb_1024` / `swb_128` for sfi 0..=11; 88.2 kHz uses the 96 kHz table
/// layout of the ISO standard as given in `crate::portable::aac::bits::tables`).  Includes the closing offset (1024 / 128).
#[inline]
pub fn swb_long(sfi: u8) -> &'static [u16] {
    iso::SWB_LONG[sfi as usize]
}

#[inline]
pub fn swb_short(sfi: u8) -> &'static [u16] {
    iso::SWB_SHORT[sfi as usize]
}

/// `gain = 2^(n/4)` as a float, built exactly like Apple: `kPow2_025[n & 3] + ((n >> 2) * 0x800000)` as a wrapping u32 add on the bit
/// pattern (`n` signed, arithmetic shift).
#[inline]
pub fn gain_f32(n: i32) -> f32 {
    let bits = POW2_025_BITS[(n & 3) as usize].wrapping_add(((n >> 2) as u32).wrapping_mul(0x80_0000));
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_matches_pow2() {
        assert_eq!(gain_f32(0), 1.0);
        assert_eq!(gain_f32(4), 2.0);
        assert_eq!(gain_f32(-4), 0.5);
        assert_eq!(gain_f32(1).to_bits(), 0x3f98_37f0);
        assert_eq!(gain_f32(-3).to_bits(), 0x3f98_37f0 - (1 << 23));
        assert_eq!(gain_f32(-356).to_bits(), 0x3f80_0000 - (89 << 23));
    }

    #[test]
    fn pow43_entry2() {
        assert_eq!(POW43[2].to_bits(), 0x4021_4517);
        assert_eq!(POW43[0], 0.0);
        assert_eq!(POW43[1], 1.0);
    }
}
