//! AudioSpecificConfig (ISO/IEC 14496-3 1.6.2) and program_config_element (4.4.1.1) parsing, and the plain config structs handed to
//! `decoder::bits` (which owns the element list `CreateList`).

use crate::portable::aac::decoder::file::bits::BitReader;

/// `kMP4SampleRateTable` / ISO sampling_frequency_index 0..12.
pub const SAMPLE_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
/// `kMP4SampleRateBoundariesTable` (AudioCodecs 0x835f1c): the first entry `<= rate` selects the SFB table index.
pub const SAMPLE_RATE_BOUNDARIES: [u32; 11] = [92017, 75132, 55426, 46009, 37566, 27713, 23004, 18783, 13856, 11502, 9391];

/// One element of a PCE front/side/back/cc list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PceElement {
    /// `*_is_cpe` (channel pair element) / `cc_element_is_ind_sw` for the coupling list
    pub is_cpe: bool,
    /// `*_element_tag_select`
    pub tag: u8,
}

/// Parsed `program_config_element`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PceConfig {
    pub element_instance_tag: u8,
    pub object_type: u8,
    pub sfi: u8,
    pub front: Vec<PceElement>,
    pub side: Vec<PceElement>,
    pub back: Vec<PceElement>,
    pub lfe: Vec<u8>,
    pub assoc: Vec<u8>,
    pub cc: Vec<PceElement>,
    pub mono_mixdown: Option<u8>,
    pub stereo_mixdown: Option<u8>,
    pub matrix_mixdown: Option<(u8, bool)>,
}

/// What `decoder::file` hands to `decoder::bits`/`decoder` (only produced for streams Apple's AAC-LC decoder is created for).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoderConfig {
    /// sample rate of the ASC (explicit-rate aware), Hz
    pub sample_rate: u32,
    /// SFB table index `0..=11` the way Apple derives it (`sfi == 15`: exact table match, else nearest through the boundaries table);
    /// `None` for explicit rates above 96 kHz (no SFB table: every access unit fails)
    pub sfi: Option<u8>,
    /// ASC `channelConfiguration` 0..=14 (0 => `pce` is `Some`)
    pub channel_config: u8,
    /// number of output channels
    pub channels: u8,
    pub pce: Option<PceConfig>,
}

/// SBR/PS signalling found after the GASpecificConfig (backward-compatible HE-AAC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeSignal {
    /// extension sampling rate (Hz)
    pub ext_rate: u32,
    pub ps: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asc {
    /// `audioObjectType` (escape resolved: 32 + `audioObjectTypeExt`)
    pub aot: u32,
    /// raw 4-bit `samplingFrequencyIndex` (15 = explicit)
    pub sfi_raw: u8,
    /// sample rate in Hz: table value or the explicit 24-bit value
    pub sample_rate: u32,
    pub channel_config: u8,
    pub frame_length_flag: bool,
    pub depends_on_core: bool,
    pub extension_flag: bool,
    pub pce: Option<PceConfig>,
    pub he: Option<HeSignal>,
    /// bits consumed (through the GASpecificConfig/PCE and the SBR/PS extension when present); LATM needs it to find the end of an embedded ASC
    pub bits_consumed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AscError {
    /// ran out of bits inside a field that Apple needs to read
    Truncated,
    /// `samplingFrequencyIndex` 13 or 14
    ReservedSfi,
}

fn read_aot(r: &mut BitReader) -> Option<u32> {
    let a = r.read(5)?;
    if a == 31 {
        Some(32 + r.read(6)?)
    } else {
        Some(a)
    }
}

/// Parses a program_config_element body.  `r` is positioned at `element_instance_tag`; `byte_alignment()` is relative to the start of the
/// reader's slice (the ASC for the embedded PCE, the raw_data_block for an in-band one), exactly as in the standard.
pub(crate) fn parse_pce(r: &mut BitReader) -> Option<PceConfig> {
    let mut p = PceConfig::default();
    p.element_instance_tag = r.read(4)? as u8;
    p.object_type = r.read(2)? as u8;
    p.sfi = r.read(4)? as u8;
    let nf = r.read(4)? as usize;
    let ns = r.read(4)? as usize;
    let nb = r.read(4)? as usize;
    let nl = r.read(2)? as usize;
    let na = r.read(3)? as usize;
    let nc = r.read(4)? as usize;
    if r.read(1)? == 1 {
        p.mono_mixdown = Some(r.read(4)? as u8);
    }
    if r.read(1)? == 1 {
        p.stereo_mixdown = Some(r.read(4)? as u8);
    }
    if r.read(1)? == 1 {
        let idx = r.read(2)? as u8;
        let pseudo = r.read(1)? == 1;
        p.matrix_mixdown = Some((idx, pseudo));
    }
    let mut elems = |n: usize, out: &mut Vec<PceElement>| -> Option<()> {
        for _ in 0..n {
            let is_cpe = r.read(1)? == 1;
            let tag = r.read(4)? as u8;
            out.push(PceElement { is_cpe, tag });
        }
        Some(())
    };
    elems(nf, &mut p.front)?;
    elems(ns, &mut p.side)?;
    elems(nb, &mut p.back)?;
    for _ in 0..nl {
        p.lfe.push(r.read(4)? as u8);
    }
    for _ in 0..na {
        p.assoc.push(r.read(4)? as u8);
    }
    for _ in 0..nc {
        let is_ind = r.read(1)? == 1;
        let tag = r.read(4)? as u8;
        p.cc.push(PceElement { is_cpe: is_ind, tag });
    }
    r.align()?;
    let n = r.read(8)? as usize;
    r.skip(n * 8)?; // comment_field_data
    Some(p)
}

/// Parses an AudioSpecificConfig.  Only the fields Apple's file layer needs are interpreted; GASpecificConfig is parsed for the audio object
/// types it exists for (1, 2, 3, 4, 23), other object types (32, 33, 39, 42, ...) return after `channelConfiguration`.
///
/// SBR / PS signalling comes in two forms and yields the same [`HeSignal`]: hierarchical (`audioObjectType` 5 / 29, the core's rate and channel
/// configuration, the extension rate, then the core's `audioObjectType`) and backward compatible (behind the GASpecificConfig).  `aot`, `sample_rate`
/// and the rest are the core's in both.
pub fn parse_asc(bytes: &[u8]) -> Result<Asc, AscError> {
    let mut r = BitReader::new(bytes);
    let t = AscError::Truncated;
    let mut aot = read_aot(&mut r).ok_or(t)?;
    let sfi_raw = r.read(4).ok_or(t)? as u8;
    let sample_rate = match sfi_raw {
        0..=12 => SAMPLE_RATES[sfi_raw as usize],
        13 | 14 => return Err(AscError::ReservedSfi),
        _ => r.read(24).ok_or(t)?,
    };
    let channel_config = r.read(4).ok_or(t)? as u8;
    let mut explicit_he = None;
    if aot == 5 || aot == 29 {
        let esfi = r.read(4).ok_or(t)?;
        let ext_rate = match esfi {
            15 => r.read(24).ok_or(t)?,
            0..=12 => SAMPLE_RATES[esfi as usize],
            _ => 0, // reserved: no rate, the file does not open
        };
        explicit_he = Some(HeSignal { ext_rate, ps: aot == 29 });
        aot = read_aot(&mut r).ok_or(t)?;
    }
    let mut asc = Asc {
        aot,
        sfi_raw,
        sample_rate,
        channel_config,
        frame_length_flag: false,
        depends_on_core: false,
        extension_flag: false,
        pce: None,
        he: explicit_he,
        bits_consumed: 0,
    };
    asc.bits_consumed = r.bit_pos();
    if aot == 39 {
        // ELDSpecificConfig starts with frameLengthFlag (1 = 480, 0 = 512 samples)
        asc.frame_length_flag = r.read(1).ok_or(t)? == 1;
        return Ok(asc);
    }
    if !matches!(aot, 1 | 2 | 3 | 4 | 23) {
        return Ok(asc);
    }
    asc.frame_length_flag = r.read(1).ok_or(t)? == 1;
    asc.depends_on_core = r.read(1).ok_or(t)? == 1;
    // an ASC that ends inside coreCoderDelay still opens (macOS 27.2)
    let mut delay_cut = false;
    if asc.depends_on_core && r.read(14).is_none() {
        delay_cut = true;
        let rest = r.bits_left();
        let _ = r.skip(rest);
    }
    asc.extension_flag = match r.read(1) {
        Some(bit) => bit == 1,
        None if delay_cut => false,
        None => return Err(t),
    };
    if channel_config == 0 {
        // Apple only reaches the PCE for configurations it can open; a PCE that cannot be read leaves `pce == None` which the caller maps
        // to "channelConfiguration 0 without PCE".
        asc.pce = parse_pce(&mut r);
        if asc.pce.is_none() {
            return Ok(asc);
        }
    }
    if asc.extension_flag {
        if aot == 23 {
            // aacSection/ScalefactorDataResilienceFlag, aacSpectralDataResilienceFlag
            let _ = r.read(3);
        }
        let _ = r.read(1); // extensionFlag3
    }
    asc.bits_consumed = r.bit_pos();
    // backward-compatible SBR / PS signalling (HE-AAC)
    if asc.he.is_none() && r.bits_left() >= 16 {
        if let Some(0x2b7) = r.read(11) {
            if let Some(ext_aot) = read_aot(&mut r) {
                if ext_aot == 5 && r.read(1) == Some(1) {
                    if let Some(esfi) = r.read(4) {
                        let ext_rate = if esfi == 15 {
                            r.read(24).unwrap_or(0)
                        } else if (esfi as usize) < SAMPLE_RATES.len() {
                            SAMPLE_RATES[esfi as usize]
                        } else {
                            0
                        };
                        let mut ps = false;
                        if r.bits_left() >= 12 && r.read(11) == Some(0x548) {
                            ps = r.read(1) == Some(1);
                        }
                        asc.he = Some(HeSignal { ext_rate, ps });
                        asc.bits_consumed = r.bit_pos();
                    }
                }
            }
        }
    }
    Ok(asc)
}

/// `sfi` the decoder's SFB table is selected with: exact `kMP4SampleRateTable` match, else the nearest standard rate
/// through the boundaries table; rates above 96 kHz find no table.
pub fn sfb_table_index(rate: u32) -> Option<u8> {
    if let Some(i) = SAMPLE_RATES.iter().position(|&r| r == rate) {
        return Some(i as u8);
    }
    if rate > 96000 {
        return None;
    }
    for (i, &b) in SAMPLE_RATE_BOUNDARIES.iter().enumerate() {
        if rate >= b {
            return Some(i as u8);
        }
    }
    Some(11)
}
