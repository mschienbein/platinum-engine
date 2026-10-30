//! Decoder configuration of the spectral stage: sampling-frequency index, `channel_configuration` / PCE and Apple's element list
//! (`AACSyntacticElement::CreateList` 0x2f0a4c; list order == output channel order == F2T counter order).

pub const ID_PCE: u8 = 5;
pub const ID_FIL: u8 = 6;
pub const ID_END: u8 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElementKind {
    /// single_channel_element, 1 channel.
    Sce = 0,
    /// channel_pair_element, 2 channels (left, right).
    Cpe = 1,
    /// lfe_channel_element, 1 channel (no PNS / TNS / stereo processing).
    Lfe = 3,
}

impl ElementKind {
    #[inline]
    pub fn id(self) -> u8 {
        self as u8
    }

    #[inline]
    pub fn channels(self) -> usize {
        if self == ElementKind::Cpe { 2 } else { 1 }
    }
}

/// One entry of the element list: `(type, element_instance_tag)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementSpec {
    pub kind: ElementKind,
    pub tag: u8,
}

const fn sce(tag: u8) -> ElementSpec {
    ElementSpec { kind: ElementKind::Sce, tag }
}
const fn cpe(tag: u8) -> ElementSpec {
    ElementSpec { kind: ElementKind::Cpe, tag }
}
const fn lfe(tag: u8) -> ElementSpec {
    ElementSpec { kind: ElementKind::Lfe, tag }
}

/// The program-config-element data `CreateList` needs (from the ASC for `channel_configuration == 0`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PceLists {
    /// `(is_cpe, element_tag)` in PCE order.
    pub front: Vec<(bool, u8)>,
    pub side: Vec<(bool, u8)>,
    pub back: Vec<(bool, u8)>,
    pub lfe: Vec<u8>,
    /// `num_valid_cc`; non-zero makes the decoder uncreatable ("Unsupported coupling channel element").
    pub num_cc: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// sampling-frequency index 12 (7350 Hz, no SFB table entry), 13, 14.
    UnsupportedSamplingIndex(u8),
    /// channel_configuration 8, 9, 10, 13, 15 (refused at open).
    UnsupportedChannelConfig(u8),
    /// `channel_configuration == 0` without a PCE.
    MissingPce,
    /// PCE with coupling channel elements.
    CouplingChannel,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::UnsupportedSamplingIndex(index) => write!(f, "sampling-frequency index {index} has no scalefactor-band table"),
            ConfigError::UnsupportedChannelConfig(config) => write!(f, "channel configuration {config} is not decoded"),
            ConfigError::MissingPce => f.write_str("channel configuration 0 without a program config element"),
            ConfigError::CouplingChannel => f.write_str("the program config element lists coupling channel elements"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Configuration of the spectral decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpectralConfig {
    /// Index into the SFB tables (0..=11).  `None`: explicit-rate ASC above 96 kHz (or 7350 Hz): no SFB table, every access unit fails.
    pub sfi: Option<u8>,
    /// `channel_configuration` of the ASC (0 = PCE).  `!= 0` enables the element-tag adoption of the first decoded AU.
    pub channel_config: u8,
    /// Element list in Apple's `CreateList` order.
    pub elements: Vec<ElementSpec>,
}

impl SpectralConfig {
    /// `CreateList` for `channel_config` 1..=7, 11, 12, 14 and (`channel_config == 0`) the PCE lists.
    pub fn new(sfi: Option<u8>, channel_config: u8, pce: Option<&PceLists>) -> Result<Self, ConfigError> {
        if let Some(s) = sfi {
            if s > 11 {
                return Err(ConfigError::UnsupportedSamplingIndex(s));
            }
        }
        let elements = match channel_config {
            0 => element_list_from_pce(pce.ok_or(ConfigError::MissingPce)?)?,
            1 => vec![sce(0)],
            2 => vec![cpe(0)],
            3 => vec![sce(0), cpe(0)],
            4 => vec![sce(0), cpe(0), sce(1)],
            5 => vec![sce(0), cpe(0), cpe(1)],
            6 => vec![sce(0), cpe(0), cpe(1), lfe(0)],
            7 | 12 => vec![sce(0), cpe(0), cpe(1), cpe(2), lfe(0)],
            11 => vec![sce(0), cpe(0), cpe(1), sce(1), lfe(0)],
            14 => vec![sce(0), cpe(0), cpe(1), lfe(0), cpe(2)],
            c => return Err(ConfigError::UnsupportedChannelConfig(c)),
        };
        Ok(Self { sfi, channel_config, elements })
    }
}

/// `CreateList` for `channel_configuration == 0`: front list, side list, back list (each `SCE` or `CPE` with its tag, in PCE order),
/// then the LFEs.
fn element_list_from_pce(pce: &PceLists) -> Result<Vec<ElementSpec>, ConfigError> {
    if pce.num_cc != 0 {
        return Err(ConfigError::CouplingChannel);
    }
    let mut v = Vec::new();
    for &(is_cpe, tag) in pce.front.iter().chain(pce.side.iter()).chain(pce.back.iter()) {
        v.push(if is_cpe { cpe(tag) } else { sce(tag) });
    }
    for &tag in &pce.lfe {
        v.push(lfe(tag));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coupling_channels_are_refused() {
        let mut pce = PceLists { front: vec![(false, 0), (true, 0)], lfe: vec![0], ..Default::default() };
        assert_eq!(SpectralConfig::new(Some(4), 0, Some(&pce)).unwrap().elements, vec![sce(0), cpe(0), lfe(0)]);
        pce.num_cc = 1;
        assert_eq!(SpectralConfig::new(Some(4), 0, Some(&pce)), Err(ConfigError::CouplingChannel));
    }

    #[test]
    fn lists() {
        let c = SpectralConfig::new(Some(4), 6, None).unwrap();
        assert_eq!(c.elements.iter().map(|e| e.kind.channels()).sum::<usize>(), 6);
        assert_eq!(c.elements[3].kind, ElementKind::Lfe);
        assert!(SpectralConfig::new(Some(4), 8, None).is_err());
        assert!(SpectralConfig::new(Some(12), 2, None).is_err());
        assert_eq!(SpectralConfig::new(None, 2, None).unwrap().sfi, None);
    }
}
