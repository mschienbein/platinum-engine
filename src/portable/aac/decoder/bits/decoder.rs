//! `AACDecoder::Deserialize` (0x2cdb84): the `raw_data_block` loop, element dispatch, tag adoption, PCE/FIL/DSE skipping, error
//! semantics, and the per-element stage order (SCE / CPE / LFE `Deserialize`).

use crate::portable::aac::decoder::bits::bitreader::BitReader;
use crate::portable::aac::decoder::bits::config::{ElementKind, SpectralConfig, ID_END, ID_FIL, ID_PCE};
use crate::portable::aac::decoder::bits::ics::{ChannelWork, Scratch, ics_deserialize};
use crate::portable::aac::decoder::bits::stages::{apply_intensity, apply_ms, apply_pns, apply_tns};
use crate::portable::aac::decoder::bits::tables::PNS_SEED;
use crate::portable::aac::decoder::bits::{AuError, AuResult, ChannelSpectrum};

struct Element {
    kind: ElementKind,
    /// Current instance tag (adopted from the stream in the first successfully decoded AU when `channel_configuration != 0`).
    tag: u8,
    seen: bool,
    /// PNS LCG state per channel (`ICS+0xf0`), persistent for the life of the decoder.
    rng: [[u32; 4]; 2],
    /// Index of the element's first channel in `work`.
    first_ch: usize,
}

/// Stateful decoder of the spectral stage (access-unit bytes -> per-channel float32[1024] spectrum for the inverse MDCT).
pub struct SpectralDecoder {
    cfg: SpectralConfig,
    sfi: Option<u8>,
    elems: Vec<Element>,
    adopt_tags: bool,
    work: Vec<Box<ChannelWork>>,
    scratch: Box<Scratch>,
    /// element ids decoded by the last access unit, in stream order
    order: Vec<u8>,
    /// the first SBR extension payload of the last access unit: its `bs_header_flag`
    sbr: Option<bool>,
    /// the `matrix_mixdown_idx` of the last in-band PCE that carried one.  Persistent: `reset` and an access unit that fails later keep it (probed).
    matrix_mixdown: Option<u8>,
    /// an in-band PCE with a `matrix_mixdown_idx` that does not describe the 5.1 layout has been seen: what AudioToolbox makes of the stereo mix-down then is not
    /// reproduced (sticky)
    pce_unreproduced: bool,
}

impl SpectralDecoder {
    pub fn new(cfg: SpectralConfig) -> Self {
        // sfi 12 (7350 Hz) / 13..15 have no SFB table in Apple's decoder: every access unit fails
        let sfi = cfg.sfi.filter(|&s| s <= 11);
        let mut elems = Vec::with_capacity(cfg.elements.len());
        let mut nch = 0;
        for e in &cfg.elements {
            elems.push(Element { kind: e.kind, tag: e.tag, seen: false, rng: [PNS_SEED; 2], first_ch: nch });
            nch += e.kind.channels();
        }
        let work = (0..nch).map(|_| ChannelWork::new(sfi.unwrap_or(0))).collect();
        let adopt_tags = cfg.channel_config != 0;
        Self { cfg, sfi, elems, adopt_tags, work, scratch: Scratch::new(), order: Vec::new(), sbr: None, matrix_mixdown: None, pce_unreproduced: false }
    }

    /// Output channels per access unit (F2T counter range).
    pub fn num_channels(&self) -> usize {
        self.work.len()
    }

    /// `AACDecoder::Reset` (seek / flush): restores the tag-adoption flag to its initial value.  The PNS LCG state is NOT touched
    /// (verified against Apple: after `ExtAudioFileSeek(0)` the re-decoded AU 0 starts from the advanced state).  Adopted tags stay.
    pub fn reset(&mut self) {
        self.adopt_tags = self.cfg.channel_config != 0;
        for e in &mut self.elems {
            e.seen = false;
        }
    }

    /// Decode one access unit (one `raw_data_block`) into `out`.  On error the whole AU is dropped (`status != 0`, no channels); the PNS state of
    /// already finished elements still advances.
    pub fn decode_au_into(&mut self, au: &[u8], out: &mut AuResult) {
        out.channels.clear();
        out.status = 0;
        out.error = None;
        self.order.clear();
        self.sbr = None;
        let mut br = BitReader::new(au);
        let r = if self.sfi.is_none() { Err(AuError::NoSfbTable) } else { self.raw_data_block(&mut br) };
        for e in &mut self.elems {
            e.seen = false;
        }
        match r {
            Ok(()) => {
                for e in &self.elems {
                    for c in 0..e.kind.channels() {
                        let w = &self.work[e.first_ch + c];
                        out.channels.push(ChannelSpectrum { coeffs: w.spec, window_sequence: w.info.window_sequence, window_shape: w.info.window_shape });
                    }
                }
            }
            Err(e) => {
                out.status = e.status();
                out.error = Some(e);
            }
        }
    }

    /// The channel elements (`0` SCE, `1` CPE, `3` LFE) the last access unit handed to [`decode_au_into`](Self::decode_au_into) decoded, in stream
    /// order.
    pub fn decoded_elements(&self) -> &[u8] {
        &self.order
    }

    /// The `matrix_mixdown_idx` (0..=3) of the last in-band program config element that had one, whichever access unit it was in and whether or not
    /// that unit decoded.  AudioToolbox's decoder builds the stereo downmix of a 5.1 stream from it (the ISO 14496-3 matrix mixdown); a PCE in the
    /// configuration (ASC) has no such effect.
    pub fn matrix_mixdown(&self) -> Option<u8> {
        self.matrix_mixdown
    }

    /// Whether an in-band PCE with a matrix mix-down index has been seen that is not "well formed": it does not describe the 5.1 layout of valid streams (front SCE and
    /// CPE, back CPE, one LFE, nothing else).  The probes of damaged streams that Apple's stereo mix-down answered differently from every rule found (macOS 27.2) all
    /// had such a PCE; the reader refuses them instead of mixing.  (A well-formed PCE in an access unit that fails is mixed: the port and Apple agree.)
    pub fn pce_unreproduced(&self) -> bool {
        self.pce_unreproduced
    }

    /// `Some(bs_header_flag)` when the last access unit carried an SBR extension payload (`fill_element` of extension type 13 or 14).
    pub fn sbr_extension(&self) -> Option<bool> {
        self.sbr
    }

    fn raw_data_block(&mut self, br: &mut BitReader) -> Result<(), AuError> {
        let mut ndes = 0usize;
        loop {
            let id = br.read(3) as u8;
            if id == ID_END {
                if ndes != self.elems.len() {
                    return Err(AuError::UnmatchedElementCount);
                }
                br.align();
                self.adopt_tags = false;
                return Ok(());
            }
            let tag = br.read(4) as u8;
            if id < 4 {
                if self.adopt_tags {
                    for e in self.elems.iter_mut() {
                        if e.kind.id() == id && e.tag == tag && !e.seen {
                            break;
                        }
                        if e.kind.id() == id && !e.seen {
                            e.tag = tag;
                            break;
                        }
                    }
                }
                // an unlisted / duplicate element consumes nothing (CCE is never in the list)
                if let Some(i) = self.elems.iter().position(|e| e.kind.id() == id && e.tag == tag && !e.seen) {
                    self.decode_element(i, br)?;
                    self.order.push(self.elems[i].kind.id());
                    self.elems[i].seen = true;
                    ndes += 1;
                }
            } else if id == ID_PCE {
                let pce = read_pce(br);
                if let Some(index) = pce.matrix {
                    self.matrix_mixdown = Some(index);
                    self.pce_unreproduced |= !pce.five_one;
                }
            } else if id == ID_FIL {
                let mut cnt = tag as usize;
                if cnt == 15 {
                    cnt = br.read(8) as usize + 14;
                }
                if br.bits_left() <= (cnt * 8) as isize {
                    return Err(AuError::TooFewBitsLeft);
                }
                // a payload starts with its 4-bit extension_type; SBR data (13) goes on with bs_header_flag, SBR data with CRC (14) after 10 CRC bits
                let mut used = 0;
                if cnt > 0 {
                    used = 4;
                    let ty = br.read(4);
                    if self.sbr.is_none() && (ty == 13 && cnt * 8 >= 5 || ty == 14 && cnt * 8 >= 15) {
                        if ty == 14 {
                            br.skip(10);
                            used += 10;
                        }
                        self.sbr = Some(br.read_bit());
                        used += 1;
                    }
                }
                br.skip(cnt * 8 - used);
            } else {
                // data_stream_element
                let align = br.read_bit();
                let mut cnt = br.read(8) as usize;
                if cnt == 255 {
                    cnt += br.read(8) as usize;
                }
                if align {
                    br.align();
                }
                if br.bits_left() <= (cnt * 8) as isize {
                    return Err(AuError::TooFewBitsLeft);
                }
                br.skip(cnt * 8);
            }
            if br.bits_left() <= 2 {
                return Err(AuError::TooFewBitsLeft);
            }
        }
    }

    fn decode_element(&mut self, i: usize, br: &mut BitReader) -> Result<(), AuError> {
        let Self { elems, work, scratch, .. } = self;
        let e = &mut elems[i];
        let c0 = e.first_ch;
        match e.kind {
            ElementKind::Sce | ElementKind::Lfe => {
                let w = &mut work[c0];
                ics_deserialize(br, w, scratch, true, false)?;
                if e.kind == ElementKind::Sce {
                    apply_pns(w, &mut e.rng[0]);
                    apply_tns(w);
                }
                Ok(())
            }
            ElementKind::Cpe => {
                let (a, b) = work[c0..c0 + 2].split_at_mut(1);
                let (l, r) = (&mut a[0], &mut b[0]);
                let [rng_l, rng_r] = &mut e.rng;
                let common = br.read_bit();
                if !common {
                    ics_deserialize(br, l, scratch, true, false)?;
                    ics_deserialize(br, r, scratch, false, false)?;
                    apply_pns(l, rng_l);
                    apply_pns(r, rng_r);
                    apply_tns(l);
                    apply_tns(r);
                    return Ok(());
                }
                let mut info = l.info;
                info.parse(br)?;
                // JointStereo::Deserialize 0x2d55d8
                let present = br.read(2) as u8;
                let mut mask = [0u64; 8];
                if present != 0 && info.max_sfb != 0 {
                    if present == 3 {
                        return Err(AuError::MsMaskPresent3);
                    }
                    for g in 0..info.num_groups as usize {
                        if present == 2 {
                            mask[g] = (1u64 << info.max_sfb) - 1;
                        } else {
                            for b in 0..info.max_sfb {
                                if br.read_bit() {
                                    mask[g] |= 1u64 << b;
                                }
                            }
                        }
                    }
                }
                l.info = info;
                r.info = info;
                ics_deserialize(br, l, scratch, true, true)?;
                ics_deserialize(br, r, scratch, false, true)?;
                apply_ms(&info, present, &mask, &mut l.spec, &mut r.spec);
                apply_pns(l, rng_l);
                apply_pns(r, rng_r);
                apply_intensity(&info, present, &mask, &l.spec, r);
                apply_tns(l);
                apply_tns(r);
                Ok(())
            }
        }
    }
}

/// In-band `program_config_element` (the 4-bit tag is already consumed): parsed and skipped, no effect on the elements decoded; returns the
/// `matrix_mixdown_idx` if the PCE has one (taken when it is read: a PCE that runs out of bits afterwards has set it, bits behind the end read as 0).
fn read_pce(br: &mut BitReader) -> Pce {
    // object_type(2) sampling_frequency_index(4) num_front(4) num_side(4) num_back(4) num_lfe(2) num_assoc(3) num_cc(4)
    let v = br.read(27);
    let (nf, ns, nb, nl, na, nc) = (((v >> 17) & 15) as usize, ((v >> 13) & 15) as usize, ((v >> 9) & 15) as usize, ((v >> 7) & 3) as usize, ((v >> 4) & 7) as usize, (v & 15) as usize);
    if br.read_bit() {
        br.skip(4);
    }
    if br.read_bit() {
        br.skip(4);
    }
    let matrix = if br.read_bit() {
        let index = br.read(2) as u8;
        br.skip(1); // pseudo_surround_enable: no effect (probed)
        Some(index)
    } else {
        None
    };
    // the front and back lists: (is_cpe, tag) each
    let (mut front, mut back) = ([false; 15], [false; 15]);
    for flag in front.iter_mut().take(nf) {
        *flag = br.read_bit();
        br.skip(4);
    }
    br.skip(5 * ns);
    for flag in back.iter_mut().take(nb) {
        *flag = br.read_bit();
        br.skip(4);
    }
    br.skip(4 * nl + 4 * na + 5 * nc);
    br.align();
    let cnt = br.read(8) as usize;
    br.skip(cnt * 8);
    let five_one = (nf, ns, nb, nl, na, nc) == (2, 0, 1, 1, 0, 0) && !front[0] && front[1] && back[0];
    Pce { matrix, five_one }
}

/// What the reader keeps of an in-band program config element.
struct Pce {
    /// its `matrix_mixdown_idx`, if it has one
    matrix: Option<u8>,
    /// its element lists are those of a 5.1 stream: front SCE and CPE, back CPE, one LFE, no associated data, no coupling channel
    five_one: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::aac::decoder::bits::config::PceLists;

    /// MSB first, zero padded to whole bytes.
    fn pack(bits: &str) -> Vec<u8> {
        let bits: Vec<u8> = bits.bytes().filter(|c| *c != b' ').collect();
        let mut out = vec![0u8; bits.len().div_ceil(8)];
        for (i, c) in bits.iter().enumerate() {
            if *c == b'1' {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        out
    }

    /// An in-band PCE (`ID_PCE` and the element) with `matrix` = (index, pseudo surround) or none, no element lists, no comment.
    fn pce(matrix: Option<(u8, u8)>) -> String {
        let matrix = matrix.map_or("0".to_owned(), |(index, pseudo)| format!("1 {index:02b} {pseudo}"));
        // ID_PCE, tag 0, object type 1, sfi 4, 0 front/side/back, 0 lfe, 0 assoc, 0 cc, no mono / stereo mixdown, the matrix, byte alignment of the 3 + 4 + 2 + ... bits, 0 comment bytes
        let head = format!("101 0000 01 0100 0000 0000 0000 00 000 0000 0 0 {matrix}");
        let bits = head.bytes().filter(|c| *c != b' ').count();
        format!("{head} {} 00000000", "0".repeat(bits.next_multiple_of(8) - bits))
    }

    fn decoder() -> SpectralDecoder {
        let tags: Vec<u8> = (0..16).collect();
        let lists = PceLists { front: tags.iter().map(|&t| (false, t)).collect(), side: Vec::new(), back: Vec::new(), lfe: Vec::new(), num_cc: 0 };
        SpectralDecoder::new(SpectralConfig::new(Some(4), 0, Some(&lists)).unwrap())
    }

    fn decode(dec: &mut SpectralDecoder, bits: &str) {
        let mut result = AuResult::default();
        dec.decode_au_into(&pack(bits), &mut result);
    }

    #[test]
    fn the_matrix_mixdown_index_of_an_in_band_pce_is_kept_by_the_decoder() {
        let mut dec = decoder();
        assert_eq!(dec.matrix_mixdown(), None);
        // a PCE without a matrix mixdown sets nothing; the pseudo surround bit has no effect on the index
        decode(&mut dec, &format!("{} 111", pce(None)));
        assert_eq!(dec.matrix_mixdown(), None);
        for (index, pseudo) in [(2, 1), (0, 0), (3, 1), (1, 0)] {
            decode(&mut dec, &format!("{} 111", pce(Some((index, pseudo)))));
            assert_eq!(dec.matrix_mixdown(), Some(index));
        }
        // a PCE without one leaves it, `reset` too, and so does an access unit that does not decode
        decode(&mut dec, &format!("{} 111", pce(None)));
        dec.reset();
        decode(&mut dec, "111");
        decode(&mut dec, "0000000011");
        assert_eq!(dec.matrix_mixdown(), Some(1));
    }

    /// An in-band PCE with element lists: `front` and `back` entries are (is_cpe, tag), then `side` count, `lfe` count, `assoc` count, `cc` count.
    fn pce_with_lists(matrix: Option<(u8, u8)>, front: &[(bool, u8)], side: usize, back: &[(bool, u8)], lfe: usize, assoc: usize, cc: usize) -> String {
        let matrix = matrix.map_or("0".to_owned(), |(index, pseudo)| format!("1 {index:02b} {pseudo}"));
        let mut head = format!("101 0000 01 0100 {:04b} {:04b} {:04b} {:02b} {:03b} {:04b} 0 0 {matrix}", front.len(), side, back.len(), lfe, assoc, cc);
        for (cpe, tag) in front {
            head.push_str(&format!(" {}{tag:04b}", u8::from(*cpe)));
        }
        head.push_str(&" 00000".repeat(side));
        for (cpe, tag) in back {
            head.push_str(&format!(" {}{tag:04b}", u8::from(*cpe)));
        }
        head.push_str(&" 0000".repeat(lfe + assoc));
        head.push_str(&" 00000".repeat(cc));
        let bits = head.bytes().filter(|c| *c != b' ').count();
        format!("{head} {} 00000000", "0".repeat(bits.next_multiple_of(8) - bits))
    }

    #[test]
    fn only_a_pce_of_the_5_1_shape_is_well_formed() {
        let sce_cpe = [(false, 0), (true, 0)];
        let well_formed = pce_with_lists(Some((3, 0)), &sce_cpe, 0, &[(true, 1)], 1, 0, 0);
        // (a decoder of a stream without channel elements, so that an access unit of a PCE and the end decodes)
        let empty = || SpectralDecoder::new(SpectralConfig::new(Some(4), 0, Some(&PceLists::default())).unwrap());
        let mut dec = empty();
        decode(&mut dec, &format!("{well_formed} 111"));
        assert_eq!((dec.matrix_mixdown(), dec.pce_unreproduced()), (Some(3), false));
        // every other shape with an index is not: counts, kinds, associated data, coupling channels, a PCE of no lists
        for (name, pce) in [
            ("1 front", pce_with_lists(Some((0, 0)), &[(false, 0)], 0, &[(true, 1)], 1, 0, 0)),
            ("3 front", pce_with_lists(Some((0, 0)), &[(false, 0), (true, 0), (true, 1)], 0, &[(true, 1)], 1, 0, 0)),
            ("a side element", pce_with_lists(Some((0, 0)), &sce_cpe, 1, &[(true, 1)], 1, 0, 0)),
            ("2 back", pce_with_lists(Some((0, 0)), &sce_cpe, 0, &[(true, 1), (true, 2)], 1, 0, 0)),
            ("a back SCE", pce_with_lists(Some((0, 0)), &sce_cpe, 0, &[(false, 1)], 1, 0, 0)),
            ("no LFE", pce_with_lists(Some((0, 0)), &sce_cpe, 0, &[(true, 1)], 0, 0, 0)),
            ("2 LFE", pce_with_lists(Some((0, 0)), &sce_cpe, 0, &[(true, 1)], 2, 0, 0)),
            ("assoc data", pce_with_lists(Some((0, 0)), &sce_cpe, 0, &[(true, 1)], 1, 1, 0)),
            ("a coupling channel", pce_with_lists(Some((0, 0)), &sce_cpe, 0, &[(true, 1)], 1, 0, 1)),
            ("CPE then SCE in front", pce_with_lists(Some((0, 0)), &[(true, 0), (false, 0)], 0, &[(true, 1)], 1, 0, 0)),
            ("no lists", pce(Some((2, 0)))),
        ] {
            let mut dec = empty();
            decode(&mut dec, &format!("{pce} 111"));
            assert!(dec.pce_unreproduced(), "{name}");
            assert!(dec.matrix_mixdown().is_some(), "{name}");
        }
        // a PCE without an index changes nothing, whatever it describes
        let mut dec = empty();
        decode(&mut dec, &format!("{} 111", pce_with_lists(None, &[(false, 0)], 3, &[], 0, 5, 7)));
        assert!(!dec.pce_unreproduced() && dec.matrix_mixdown().is_none());
        // a well-formed PCE in an access unit that fails is mixed all the same (Apple and the port agree)
        let mut dec = empty();
        decode(&mut dec, &format!("{well_formed} 0000000011"));
        assert!(!dec.pce_unreproduced() && dec.matrix_mixdown() == Some(3));
    }

    #[test]
    fn a_pce_that_runs_out_of_bits_has_set_its_index_already() {
        // the access unit ends right behind the matrix fields; the lists that follow are 15 front elements the unit does not hold
        let mut dec = decoder();
        let cut = "101 0000 01 0100 1111 0000 0000 00 000 0000 0 0 1 10 0";
        decode(&mut dec, cut);
        assert_eq!(dec.matrix_mixdown(), Some(2));
    }
}
