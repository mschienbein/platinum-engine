use crate::portable::aac::bits::BitWriter;
use crate::portable::aac::types::{ElementKind, PceInfo};

/// `_aacMakeDefinedProgramConfig` (00047b18) with the process default (IsLeopardOrLater): sorts the channel elements of the stream into the
/// PCE front/side/back/LFE lists.  `elems` = (kind, tag) in element order; `nch` = total channels.
pub fn make_pce(elems: &[(ElementKind, u8)], nch: u32, tag: u8, sf_index: u8) -> PceInfo {
    let thr = 7;
    let mut front: Vec<(bool, u8)> = Vec::new();
    let mut side = Vec::new();
    let mut back = Vec::new();
    let mut lfe = Vec::new();
    let (mut i8, mut i9, mut i10) = (0u32, 0u32, 0u32);
    for &(kind, t) in elems {
        match kind {
            ElementKind::Sce => {
                if i10 < 3 {
                    front.push((false, t));
                    i10 += 1;
                } else {
                    back.push((false, t));
                }
            }
            ElementKind::Cpe => {
                if front.is_empty() {
                    i8 = 1;
                }
                if i8 + i10 < 3 {
                    front.push((true, t));
                    i10 += 2;
                } else if nch <= thr || i9 > 1 {
                    back.push((true, t));
                } else {
                    side.push((true, t));
                    i9 += 2;
                }
            }
            ElementKind::Lfe => lfe.push(t),
        }
    }
    PceInfo { tag, profile: 1, sf_index, front, side, back, lfe }
}

/// PCE body (from `element_instance_tag` through `comment_field_bytes`); `byte_alignment()` is relative to the start of `w` (the raw_data_block
/// for the in-band element, the AudioSpecificConfig for the embedded one - both start byte aligned).
pub(crate) fn write_pce_body(w: &mut BitWriter, pce: &PceInfo) {
    w.write(u32::from(pce.tag), 4);
    w.write(u32::from(pce.profile), 2);
    w.write(u32::from(pce.sf_index), 4);
    w.write(pce.front.len() as u32, 4);
    w.write(pce.side.len() as u32, 4);
    w.write(pce.back.len() as u32, 4);
    w.write(pce.lfe.len() as u32, 2);
    w.write(0, 3); // assoc data
    w.write(0, 4); // coupling channels
    w.write(0, 3); // mono / stereo / matrix mixdown present
    for list in [&pce.front, &pce.side, &pce.back] {
        for &(is_cpe, t) in list.iter() {
            w.write(u32::from(is_cpe), 1);
            w.write(u32::from(t), 4);
        }
    }
    for &t in &pce.lfe {
        w.write(u32::from(t), 4);
    }
    let pad = (8 - w.bit_len() % 8) % 8;
    w.write(0, pad as u32);
    w.write(0, 8); // comment_field_bytes
}

/// The PCE as it is embedded behind the 16-bit header of an AudioSpecificConfig with `channelConfiguration = 0`:
/// returns (bytes, valid bit count); feed to `crate::portable::aac::mp4::asc_with_pce`.
pub fn write_pce_asc_bits(pce: &PceInfo) -> (Vec<u8>, usize) {
    let mut w = BitWriter::new();
    write_pce_body(&mut w, pce);
    let n = w.bit_len();
    (w.finish(), n)
}
