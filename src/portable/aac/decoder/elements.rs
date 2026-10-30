//! Element sequence of an access unit in stream order (`0` SCE, `1` CPE, `3` LFE; PCE / DSE / FIL excluded) - the input of
//! `AacFile::check_adts_elements` (ADTS: the first access unit must agree with the header's `channel_configuration`).
//!
//! The spectral stage owns the bit syntax, so the sequence is taken from it: a scratch `SpectralDecoder` with a configuration that lists every
//! (kind, tag) pair decodes the access unit and reports the elements it decoded.

use crate::portable::aac::decoder::bits::{AuError, AuResult, PceLists, SpectralConfig, SpectralDecoder};

/// What parsing an access unit found: its channel elements in stream order and the `bs_header_flag` of the first SBR extension payload, if any.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AuShape {
    pub elements: Vec<u8>,
    pub sbr: Option<bool>,
}

/// The shape of access unit `au`, or `None` when it does not parse up to its `ID_END`.
pub(crate) fn element_sequence(au: &[u8], sfi: Option<u8>) -> Option<AuShape> {
    let tags: Vec<u8> = (0..16).collect();
    let pce = PceLists {
        front: tags.iter().map(|&t| (false, t)).collect(),
        side: tags.iter().map(|&t| (true, t)).collect(),
        back: Vec::new(),
        lfe: tags,
        num_cc: 0,
    };
    let cfg = SpectralConfig::new(sfi, 0, Some(&pce)).ok()?;
    let mut dec = SpectralDecoder::new(cfg);
    let mut res = AuResult::default();
    dec.decode_au_into(au, &mut res);
    match res.error {
        // reaching `ID_END` with fewer elements than the (permissive) list has is the normal end of the scan
        None | Some(AuError::UnmatchedElementCount) => Some(AuShape { elements: dec.decoded_elements().to_vec(), sbr: dec.sbr_extension() }),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `bits` (a string of `0` / `1`) MSB first, zero padded to whole bytes.
    fn pack(bits: &str) -> Vec<u8> {
        let mut out = vec![0u8; bits.bytes().filter(|c| *c != b' ').count().div_ceil(8)];
        for (i, c) in bits.bytes().filter(|c| *c != b' ').enumerate() {
            if c == b'1' {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        out
    }

    #[test]
    fn empty_access_unit_has_no_elements() {
        // ID_END only (the access unit is then padded): the raw_data_block ends at once with fewer elements than the permissive list holds
        let au = pack("111");
        let shape = element_sequence(&au, Some(4)).unwrap();
        assert_eq!((shape.elements, shape.sbr), (vec![], None));
    }

    #[test]
    fn an_sbr_extension_payload_reports_its_header_flag() {
        // fill_element of 2 bytes, then ID_END: extension_type 13 (SBR data) with bs_header_flag 1 / 0; type 14 (with CRC) after 10 CRC bits; another type
        for (payload, expected) in [("1101 1 00000000000", Some(true)), ("1101 0 11111111111", Some(false)), ("1110 0000000000 1 0", Some(true)), ("1011 1 00000000000", None)] {
            let au = pack(&format!("110 0010 {payload} 111"));
            let shape = element_sequence(&au, Some(4)).unwrap();
            assert_eq!((shape.elements.as_slice(), shape.sbr), (&[][..], expected), "{payload}");
        }
    }

    #[test]
    fn undetermined_when_the_unit_does_not_parse() {
        // an LFE element whose ICS runs out of bits
        let au = pack("011 0000 1");
        assert_eq!(element_sequence(&au.clone(), Some(4)), None);
    }
}
