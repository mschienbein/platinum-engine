// Constant tables of the quantiser (generated from the tables of the original; the scalefactor length table is derived from the ISO code lengths).
#![allow(clippy::all)]
pub static K_INV_POW2_TAB: [f32; 40] = [
    f32::from_bits(0x3f800000),
    f32::from_bits(0x3f000000),
    f32::from_bits(0x3e800000),
    f32::from_bits(0x3e000000),
    f32::from_bits(0x3d800000),
    f32::from_bits(0x3d000000),
    f32::from_bits(0x3c800000),
    f32::from_bits(0x3c000000),
    f32::from_bits(0x3b800000),
    f32::from_bits(0x3b000000),
    f32::from_bits(0x3a800000),
    f32::from_bits(0x3a000000),
    f32::from_bits(0x39800000),
    f32::from_bits(0x39000000),
    f32::from_bits(0x38800000),
    f32::from_bits(0x38000000),
    f32::from_bits(0x37800000),
    f32::from_bits(0x37000000),
    f32::from_bits(0x36800000),
    f32::from_bits(0x36000000),
    f32::from_bits(0x35800000),
    f32::from_bits(0x35000000),
    f32::from_bits(0x34800000),
    f32::from_bits(0x34000000),
    f32::from_bits(0x33800000),
    f32::from_bits(0x33000000),
    f32::from_bits(0x32800000),
    f32::from_bits(0x32000000),
    f32::from_bits(0x31800000),
    f32::from_bits(0x31000000),
    f32::from_bits(0x30800000),
    f32::from_bits(0x30000000),
    f32::from_bits(0x2f800000),
    f32::from_bits(0x2f000000),
    f32::from_bits(0x2e800000),
    f32::from_bits(0x2e000000),
    f32::from_bits(0x2d800000),
    f32::from_bits(0x2d000000),
    f32::from_bits(0x2c800000),
    f32::from_bits(0x2c000000),
];
pub static K_INV_POW2_TO_00625_TAB: [f32; 16] = [
    f32::from_bits(0x3f800000),
    f32::from_bits(0x3f75257e),
    f32::from_bits(0x3f6ac0c7),
    f32::from_bits(0x3f60cce0),
    f32::from_bits(0x3f5744fd),
    f32::from_bits(0x3f4e248d),
    f32::from_bits(0x3f45672a),
    f32::from_bits(0x3f3d08a4),
    f32::from_bits(0x3f3504f3),
    f32::from_bits(0x3f2d583f),
    f32::from_bits(0x3f25fed7),
    f32::from_bits(0x3f1ef532),
    f32::from_bits(0x3f1837ef),
    f32::from_bits(0x3f11c3d3),
    f32::from_bits(0x3f0b95c2),
    f32::from_bits(0x3f05aac3),
];
pub static K_POW2_TAB: [f32; 40] = [
    f32::from_bits(0x3f800000),
    f32::from_bits(0x40000000),
    f32::from_bits(0x40800000),
    f32::from_bits(0x41000000),
    f32::from_bits(0x41800000),
    f32::from_bits(0x42000000),
    f32::from_bits(0x42800000),
    f32::from_bits(0x43000000),
    f32::from_bits(0x43800000),
    f32::from_bits(0x44000000),
    f32::from_bits(0x44800000),
    f32::from_bits(0x45000000),
    f32::from_bits(0x45800000),
    f32::from_bits(0x46000000),
    f32::from_bits(0x46800000),
    f32::from_bits(0x47000000),
    f32::from_bits(0x47800000),
    f32::from_bits(0x48000000),
    f32::from_bits(0x48800000),
    f32::from_bits(0x49000000),
    f32::from_bits(0x49800000),
    f32::from_bits(0x4a000000),
    f32::from_bits(0x4a800000),
    f32::from_bits(0x4b000000),
    f32::from_bits(0x4b800000),
    f32::from_bits(0x4c000000),
    f32::from_bits(0x4c800000),
    f32::from_bits(0x4d000000),
    f32::from_bits(0x4d800000),
    f32::from_bits(0x4e000000),
    f32::from_bits(0x4e800000),
    f32::from_bits(0x4f000000),
    f32::from_bits(0x4f800000),
    f32::from_bits(0x50000000),
    f32::from_bits(0x50800000),
    f32::from_bits(0x51000000),
    f32::from_bits(0x51800000),
    f32::from_bits(0x52000000),
    f32::from_bits(0x52800000),
    f32::from_bits(0x53000000),
];
pub static K_POW2_TO_00625_TAB: [f32; 16] = [
    f32::from_bits(0x3f800000),
    f32::from_bits(0x3f85aac3),
    f32::from_bits(0x3f8b95c2),
    f32::from_bits(0x3f91c3d3),
    f32::from_bits(0x3f9837f0),
    f32::from_bits(0x3f9ef532),
    f32::from_bits(0x3fa5fed7),
    f32::from_bits(0x3fad583f),
    f32::from_bits(0x3fb504f3),
    f32::from_bits(0x3fbd08a4),
    f32::from_bits(0x3fc5672a),
    f32::from_bits(0x3fce248c),
    f32::from_bits(0x3fd744fe),
    f32::from_bits(0x3fe0ccdf),
    f32::from_bits(0x3feac0c7),
    f32::from_bits(0x3ff5257d),
];
pub static K_INV_POW2_TO_025_TAB: [f32; 4] = [
    f32::from_bits(0x3f800000),
    f32::from_bits(0x3f5744fd),
    f32::from_bits(0x3f3504f3),
    f32::from_bits(0x3f1837f0),
];
pub static K_POW2_025: [f32; 4] = [
    f32::from_bits(0x3f800000),
    f32::from_bits(0x3f9837f0),
    f32::from_bits(0x3fb504f3),
    f32::from_bits(0x3fd744fd),
];
/// `huff_ltabscfLoops`: the scalefactor code lengths the loop kernel's bit estimate indexes as `12 + (delta + 60)`: the ISO code lengths
/// ([`SF_LEN`](crate::portable::aac::bits::tables::SF_LEN), 121 entries for the deltas -60..=60) at 12..=132, `0x4000_0000` ("cannot be coded") in the
/// 12 entries on either side, and a closing 0 at index 145.
pub static HUFF_LTAB_SCF_LOOPS: [i32; 146] = huff_ltab_scf_loops();

const fn huff_ltab_scf_loops() -> [i32; 146] {
    let mut table = [1 << 30; 146];
    let mut i = 0;
    while i < crate::portable::aac::bits::tables::SF_LEN.len() {
        table[12 + i] = crate::portable::aac::bits::tables::SF_LEN[i] as i32;
        i += 1;
    }
    table[145] = 0;
    table
}
