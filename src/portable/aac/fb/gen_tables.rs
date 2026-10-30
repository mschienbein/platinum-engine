//! Constant tables of the filterbank stage (generated; do not edit by hand).
#![allow(clippy::all)]

/// `TNS_MAX_BANDS` rows `(long, short)` for frame length 1024 (0x80c280; ISO 14496-3 table 4.139), indexed by `sampling_frequency_index` 0..=11 (96000 Hz .. 8000 Hz).
pub static TNS_MAX_BANDS_1024: [(i32, i32); 12] = [(31, 9), (31, 9), (34, 10), (40, 14), (42, 14), (51, 14), (46, 14), (46, 14), (42, 14), (42, 14), (42, 14), (39, 14)];

/// `_tnsInfoTab` (0x9a0b80): `(bitrate_lo, bitrate_hi, profile for idx 0, profile for idx > 0)`, profile = `(direction a, flags b, lag sigma long c, lag sigma short d)`.
pub static TNS_INFO_TAB: [(u32, u32, (u16, u16, f32, f32), (u16, u16, f32, f32)); 6] = [
    (4000, 12000, (1, 1, f32::from_bits(0x3f19999a), f32::from_bits(0x3f19999a)), (1, 1, f32::from_bits(0x3f19999a), f32::from_bits(0x3f19999a))),
    (12001, 17999, (1, 1, f32::from_bits(0x3f19999a), f32::from_bits(0x3f19999a)), (1, 1, f32::from_bits(0x3f19999a), f32::from_bits(0x3f19999a))),
    (18000, 28000, (1, 1, f32::from_bits(0x3f000000), f32::from_bits(0x3f000000)), (1, 1, f32::from_bits(0x3f000000), f32::from_bits(0x3f000000))),
    (28001, 36000, (1, 3, f32::from_bits(0x3f4ccccd), f32::from_bits(0x3e99999a)), (1, 3, f32::from_bits(0x3f000000), f32::from_bits(0x3e99999a))),
    (36001, 52000, (1, 3, f32::from_bits(0x3f000000), f32::from_bits(0x3dcccccd)), (1, 3, f32::from_bits(0x3f000000), f32::from_bits(0x3dcccccd))),
    (52001, 320000, (1, 3, f32::from_bits(0x3e99999a), f32::from_bits(0x3dcccccd)), (1, 3, f32::from_bits(0x3e99999a), f32::from_bits(0x3dcccccd))),
];

/// `_defaultParaLong` (0x636058) / `_defaultParaShort` (0x6360a0): `(max order, min prediction gain bits, f32 2.5 bits, narrow start Hz, stop Hz, wide start Hz, ..)`.
pub static DEFAULT_PARA_LONG: [i32; 7] = [12, 1068792545, 1075838976, 2500, 16000, 1275, 1275];
pub static DEFAULT_PARA_SHORT: [i32; 7] = [7, 1068792545, 1075838976, 3750, 16000, 2750, 0];

pub static IQ_TABLE: [f32; 4] = [f32::from_bits(0x3f860a92), f32::from_bits(0x3ee5c8fa), f32::from_bits(0x3e567750), f32::from_bits(0x3dcf8c3d)];
pub static IQ_TABLE_M: [f32; 4] = [f32::from_bits(0x3f20d97c), f32::from_bits(0x3eb2b8c2), f32::from_bits(0x3e3d3c1a), f32::from_bits(0x3dc2f81a)];

/// Monotone decision thresholds replacing `asin` in `_TnsEncodeSingleFilter` for 4-bit coefficients (exact; host libm and correctly rounded asin agree).
pub static ASIN_POS_THRESHOLDS: [f32; 7] = [f32::from_bits(0x3dd61304), f32::from_bits(0x3e9e377a), f32::from_bits(0x3f000000), f32::from_bits(0x3f2b4c25), f32::from_bits(0x3f4f1bbd), f32::from_bits(0x3f69de1e), f32::from_bits(0x3f7a67e2)];
pub static ASIN_NEG_THRESHOLDS: [f32; 8] = [f32::from_bits(0xbdbcf732), f32::from_bits(0xbe8c1d8e), f32::from_bits(0xbee437d2), f32::from_bits(0xbf1a4644), f32::from_bits(0xbf3d2fb1), f32::from_bits(0xbf59a7d5), f32::from_bits(0xbf6eb681), f32::from_bits(0xbf7ba421)];

// ---- bandwidth (`_SetAudioBandwidth`) ----
/// `bitsPerSampleTable_MediumBW` (0x62a18c).
pub static BPS_MEDIUM_BW: [f32; 11] = [f32::from_bits(0x40000000), f32::from_bits(0x3fc00000), f32::from_bits(0x3fe66666), f32::from_bits(0x3fc00000), f32::from_bits(0x3fcccccd), f32::from_bits(0x3fcccccd), f32::from_bits(0x3fd33333), f32::from_bits(0x3fd33333), f32::from_bits(0x3fc66666), f32::from_bits(0x3fc66666), f32::from_bits(0x3fcccccd)];
/// `bitsPerSampleTable_LargeBW` (0x62a1b8).
pub static BPS_LARGE_BW: [f32; 11] = [f32::from_bits(0x3fd9999a), f32::from_bits(0x3faa3d71), f32::from_bits(0x3fcccccd), f32::from_bits(0x3faa3d71), f32::from_bits(0x3fb9999a), f32::from_bits(0x3fb9999a), f32::from_bits(0x3fc00000), f32::from_bits(0x3fc00000), f32::from_bits(0x3faf5c29), f32::from_bits(0x3faf5c29), f32::from_bits(0x3fb33333)];

/// Float constants of `_SetAudioBandwidth` / `_ApplyBandwidthReduction` / `_SetBandwidthParametersXEld` by file address.
pub const C_00922688: f32 = f32::from_bits(0x46bb8200);
pub const C_009226B8: f32 = f32::from_bits(0x3f75c28f);
pub const C_009226C8: f32 = f32::from_bits(0x466a6000);
pub const C_009226CC: f32 = f32::from_bits(0x473b8000);
pub const C_009226D0: f32 = f32::from_bits(0x40d55547);
pub const C_009226D4: f32 = f32::from_bits(0xc66a6000);
pub const C_009226D8: f32 = f32::from_bits(0x472c4400);
pub const C_009226DC: f32 = f32::from_bits(0x3f916873);
pub const C_009226E0: f32 = f32::from_bits(0x46628c00);
pub const C_009226E4: f32 = f32::from_bits(0x3f5d2f1a); // also the literal at 0x920e0c
pub const C_009226E8: f32 = f32::from_bits(0x3f6f1aa0); // also the literal at 0x920e08
pub const C_009226EC: f32 = f32::from_bits(0x46866000);
pub const C_009225D8: f32 = f32::from_bits(0x3f666666);
pub const C_009226F0: f32 = f32::from_bits(0x469c1d9a);
pub const C_00922720: f32 = f32::from_bits(0x3ed70a3d);
pub const C_00922724: f32 = f32::from_bits(0x3da3d70a);
