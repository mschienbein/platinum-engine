//! Constant tables of the intensity stage (dumped from Apple's AudioCodecs arm64e slice; see `tables_data.rs`, generated).
use crate::portable::aac::int::tables_data as d;

const fn conv<const N: usize>(b: [u32; N]) -> [f32; N] {
    let mut o = [0.0f32; N];
    let mut i = 0;
    while i < N {
        o[i] = f32::from_bits(b[i]);
        i += 1;
    }
    o
}

/// `_isPositionTable` (0x80ee7c): `IS_POSITION[k] = 2^((2k+1)/4)`, k = 0..29 (decision boundaries of the position quantiser).
pub static IS_POSITION: [f32; 30] = conv(d::IS_POSITION_TABLE_BITS);
/// `_invQuantIsPositionTable` (0x80eef4): `INV_QUANT[p] = 2^(p/4)`, p = 0..30.
pub static INV_QUANT: [f32; 31] = conv(d::INV_QUANT_IS_POSITION_TABLE_BITS);

/// Literal-pool constants (`DAT_009225d8..` of `_aacEncodeFrame`, bit patterns).
pub mod k {
    pub const C_0_9: f32 = f32::from_bits(0x3f666666);
    pub const C_1E30: f32 = crate::portable::aac::types::F_1E30;
    pub const C_NEG_1E30: f32 = -crate::portable::aac::types::F_1E30;
    pub const C_0_001: f32 = crate::portable::aac::types::F_0_001;
    pub const T_0_1: f32 = crate::portable::aac::types::F_0_1;
    pub const POS_UP: f32 = f32::from_bits(0x3f9837f0);
    pub const POS_DOWN: f32 = f32::from_bits(0x3f5744fd);
    pub const POS_MAX: f32 = f32::from_bits(0x46d744fd);
    pub const C_1_4: f32 = f32::from_bits(0x3fb33333);
    pub const C_1E_4: f32 = f32::from_bits(0x38d1b717);
    pub const C_1_3: f32 = f32::from_bits(0x3fa66666);
    pub const C_0_009: f32 = f32::from_bits(0x3c1374bc);
    pub const C_0_10000002: f32 = f32::from_bits(0x3dccccd0);
    pub const C_1_05: f32 = f32::from_bits(0x3f866666);
    pub const C_NEG_0_4: f32 = f32::from_bits(0xbecccccd);
    pub const C_NEG_0_98067: f32 = f32::from_bits(0xbf7b0d65);
    pub const C_0_42849: f32 = f32::from_bits(0x3edb62f1);
    pub const C_0_35: f32 = f32::from_bits(0x3eb33333);
    pub const C_0_7: f32 = f32::from_bits(0x3f333333);
    pub const C_1E_20: f32 = crate::portable::aac::types::F_1E_20;
    pub const C_2_88539: f32 = f32::from_bits(0x4038aa3b);
    pub const C_1E8: f32 = f32::from_bits(0x4cbebc20);
    pub const C_0_8: f32 = f32::from_bits(0x3f4ccccd);
    pub const C_NEG_0_2: f32 = f32::from_bits(0xbe4ccccc);
    /// `FLT_MIN`
    pub const FLT_MIN: f32 = f32::from_bits(0x00800000);
}
