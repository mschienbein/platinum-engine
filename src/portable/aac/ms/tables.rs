//! Raw bit patterns of the constant tables of Apple's M/S stage (generated; AudioCodecs arm64e slice
//! SHA-256 70e5d4a5...1626; addresses in the comments).  Use through `f32::from_bits`.  (`stic::logMantissa` and `stic::pow2table` are
//! the tables of `crate::portable::aac::psy::tables`.)

/// `MsThresAdjustTable_CorrGrid` (0x62a58c).
pub static CORR_GRID_BITS: [u32; 5] = [
    0x00000000, 0x3e051eb8, 0x3ecccccd, 0x3f266666, 0x3f800000,
];

/// `MsThresAdjustTable_LdiffGrid` (0x62a5a0).
pub static LDIFF_GRID_BITS: [u32; 5] = [
    0x00000000, 0x3d8f5c29, 0x3e800000, 0x3ed80347, 0x3f800000,
];

/// `LrThresAdjustOffsetIdx` (0x62a580): {300, 1600, 18000} Hz.
pub static LR_OFFSET_IDX_BITS: [u32; 3] = [
    0x43960000, 0x44c80000, 0x468ca000,
];

/// `MsThresAdjustTable_{Long,Short}Mid_HiBitrate` (0x62a260/0x62a2c4; the long and short tables have identical bytes): 5x5, `[corr_cell * 5 + ldiff_cell]`.
pub static MS_TAB_MID_HI_BITS: [u32; 25] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x3e4ccccd,
    0x3e4ccccd, 0x3dcccccd, 0x00000000, 0x00000000, 0x3ecccccd, 0x3e99999a, 0x3e4ccccd, 0x3dcccccd,
    0x00000000,
];

/// `MsThresAdjustTable_{Long,Short}Side_HiBitrate` (0x62a328/0x62a38c; the long and short tables have identical bytes): 5x5, `[corr_cell * 5 + ldiff_cell]`.
pub static MS_TAB_SIDE_HI_BITS: [u32; 25] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00000000, 0x00000000, 0x3f800000, 0xbf333333, 0xbf333333, 0xbf000000, 0x00000000, 0x3f800000,
    0xbf333333, 0xbf333333, 0xbf19999a, 0x00000000, 0x3f800000, 0xbf19999a, 0xbf333333, 0xbf19999a,
    0x00000000,
];

/// `MsThresAdjustTable_{Long,Short}Mid_LoBitrate` (0x62a3f0/0x62a454; the long and short tables have identical bytes): 5x5, `[corr_cell * 5 + ldiff_cell]`.
pub static MS_TAB_MID_LO_BITS: [u32; 25] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x3dcccccd,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x3e4ccccd, 0x3dcccccd, 0x00000000, 0x00000000,
    0x00000000,
];

/// `MsThresAdjustTable_{Long,Short}Side_LoBitrate` (0x62a4b8/0x62a51c; the long and short tables have identical bytes): 5x5, `[corr_cell * 5 + ldiff_cell]`.
pub static MS_TAB_SIDE_LO_BITS: [u32; 25] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00000000, 0x00000000, 0x3f800000, 0xbeb33333, 0xbeb33333, 0xbe19999a, 0x00000000, 0x3f800000,
    0xbeb33333, 0xbeb33333, 0xbe800000, 0x00000000, 0x3f800000, 0xbe800000, 0xbeb33333, 0xbe800000,
    0x00000000,
];
