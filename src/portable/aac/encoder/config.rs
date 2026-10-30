//! Engine configuration derivation: `(rate, channels)` -> every derived parameter of the encoder
//! (`ExtAudioFile` `m4af`, `'brat'` 256000, `'cdqu'` 0x40, `'acbf'` 1 = LongTermAverage, AAC-LC, 1024-line frames).
//!
//! Provenance (checked against the original's `_aacOpen` and by the encoder fixtures):
//!
//! * channel -> element layout and element instance tags: AudioToolbox's default layout tag for N channels:
//!   1 `[SCE]`, 2 `[CPE]`, 3 `[SCE,CPE]`, 4 `PCE [CPE,CPE]`, 5 `[SCE,CPE,CPE]`, 5.1 `[SCE,CPE,CPE,LFE]`, 6.1 `PCE [SCE,CPE,CPE,SCE,LFE]`,
//!   7.1 `[SCE,CPE,CPE,CPE,LFE]`; tags are per-kind instance counters (SCE 0,1; CPE 0,1,2; LFE 0), channel order = element order;
//! * per-channel bitrate `R = brat / #full-bandwidth channels` (the LFE is excluded for 6/7/8 channels), integer division;
//! * bandwidth record per channel = `_SetAudioBandwidth` run with the engine arguments (`crate::portable::aac::fb::bandwidth`); the PAM bandwidth ratio
//!   `psyConf+0x198` = `_getAverageBandWidthRatio` = f32 sum of the ratios of the SCE/CPE channels (LFE skipped, element order) divided by
//!   their count (decompile 0xa024);
//! * TRDE words come from [`crate::portable::aac::psy::PamParam`] (`_adaptPamParam`), TNS/bandwidth/layout from [`crate::portable::aac::fb::FbConfig`], the quantiser/rate
//!   configuration from [`crate::portable::aac::quant::QuantConfig`] (built here for every accepted rate),
//!   the intensity configuration from [`crate::portable::aac::int::IntensityConfig`].
//!
//! AudioToolbox's AAC encoder serves a request for `rate` Hz and `channels` channels when `rate` lies within [`RATE_TOLERANCE_HZ`] of one of its nine
//! sample rates ([`NOMINAL_RATES`]) and `channels` is 1 to 8; every other format has no converter (`'fmt?'`). The engine then sets `'brat'` 256000, which the
//! converter takes only for 44.1/48 kHz x 1..8 channels, 32 kHz x 3..8, 24/22.05 kHz x 4..8 and 16 kHz x 7..8 ([`ConfigError::Refused`] for the rest, as
//! AudioToolbox answers `'!dat'`). The access units depend on the exact rate, not on the rate AudioToolbox rounds to for the file, so only the nine exact
//! rates are reproduced ([`ConfigError::NotReproduced`] for the others).
//!
//! Not covered: any other `'brat'`, `'cdqu'` or `'acbf'`, HE-AAC and ELD encoders, 64-bit-offset files, and the write alignment "class B" in which AudioToolbox
//! switches the 512-point DFT of its encoder to an SME kernel.

use crate::portable::aac::bits::{make_pce, sf_index_of_rate};
use crate::portable::aac::fb::bandwidth::{engine_element_args, set_audio_bandwidth, BandwidthArgs};
use crate::portable::aac::fb::FbConfig;
use crate::portable::aac::int::{intensity_enabled, IntensityConfig};
use crate::portable::aac::psy::{BandwidthRec, PamConfig, PamParam, PsyChannelParam};
use crate::portable::aac::quant::{KernelConfig, PamParams, QuantConfig, RateConfig};
use crate::portable::aac::types::{ElementKind, PceInfo};

/// `'brat'` of the engine.
pub const ENGINE_BRAT: u32 = 256_000;
/// `'cdqu'` bucket of the engine (default property value 0x40).
pub const ENGINE_QUALITY: u32 = 0x40;
/// Coded long-window lines of the LFE channel (hard lowpass of its psy record).
pub const LFE_CUTOFF_LINES: u32 = 12;
/// `'acbf'` of the engine (LongTermAverage).
pub const ENGINE_ACBF: u8 = 1;

/// The sample rates of AudioToolbox's AAC encoder.
pub const NOMINAL_RATES: [u32; 9] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];
/// A request this close (inclusive) to a [`NOMINAL_RATES`] entry is served as that rate (measured at every integer rate and at the edges of the windows).
pub const RATE_TOLERANCE_HZ: f64 = 10.0;
/// Most channels the AAC converter takes.
pub const MAX_CHANNELS: u32 = 8;

/// The sample rate AudioToolbox serves a request for `rate` Hz as: the [`NOMINAL_RATES`] entry within [`RATE_TOLERANCE_HZ`] of it, if any.
pub fn nominal_rate(rate: f64) -> Option<u32> {
    NOMINAL_RATES.into_iter().find(|&nominal| (rate - f64::from(nominal)).abs() <= RATE_TOLERANCE_HZ)
}

/// Why `(rate, channels)` is not an encoder configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Not an AudioToolbox AAC sample rate / channel count at all (`'fmt?'`).
    Unsupported { rate: u32, channels: u32 },
    /// Converter exists but `'brat'` = 256000 is above its applicable maximum (`'!dat'`).
    Refused { rate: u32, channels: u32 },
    /// AudioToolbox encodes this format, but not at one of its nine exact sample rates, which is all that is reproduced.
    NotReproduced { rate: u32, channels: u32 },
    /// A stage's set-up refused the configuration: the rate has no scalefactor-band table (centre frequencies, PAM parameters, psy bands) or one of
    /// `_SetAudioBandwidth`, `PamParam::new` and `PsyChannelParam::new` failed; the string names the stage.
    Stage(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Unsupported { rate, channels } => write!(f, "unsupported format {rate} Hz x {channels} ch (AudioToolbox 'fmt?')"),
            ConfigError::Refused { rate, channels } => write!(f, "brat 256000 refused for {rate} Hz x {channels} ch (AudioToolbox '!dat')"),
            ConfigError::NotReproduced { rate, channels } => write!(f, "{rate} Hz x {channels} ch is not one of the exact AAC sample rates {NOMINAL_RATES:?}"),
            ConfigError::Stage(s) => write!(f, "stage configuration error: {s}"),
        }
    }
}
impl std::error::Error for ConfigError {}

/// Is `(rate, channels)` accepted by AudioToolbox with `'brat'` = 256000 at an exact AAC sample rate (the set the encoder reproduces)?
pub fn accepts(rate: u32, channels: u32) -> Result<(), ConfigError> {
    let nominal = nominal_rate(f64::from(rate)).filter(|_| (1..=MAX_CHANNELS).contains(&channels)).ok_or(ConfigError::Unsupported { rate, channels })?;
    let taken = match nominal {
        44100 | 48000 => true,
        32000 => channels >= 3,
        22050 | 24000 => channels >= 4,
        16000 => channels >= 7,
        _ => false,
    };
    if !taken {
        Err(ConfigError::Refused { rate, channels })
    } else if rate != nominal {
        Err(ConfigError::NotReproduced { rate, channels })
    } else {
        Ok(())
    }
}

/// One syntactic element of the access unit (PCE placeholder elements are not listed; the PCE is [`EngineConfig::pce`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementCfg {
    pub kind: ElementKind,
    /// `element_instance_tag`: per-kind instance counter in element order.
    pub tag: u8,
    /// first coded channel of the element (channel order = element order).
    pub first_channel: usize,
}

impl ElementCfg {
    pub fn nch(&self) -> usize {
        if self.kind == ElementKind::Cpe { 2 } else { 1 }
    }
}

/// Element kinds of the default layout for `channels` channels (AudioToolbox default layout tags 0x640001, 0x650002, 0x720003, 0x6c0004, 0x780005,
/// 0x7c0006, 0x8e0007, 0x7f0008).
pub fn layout_kinds(channels: usize) -> Option<Vec<ElementKind>> {
    use ElementKind::*;
    Some(match channels {
        1 => vec![Sce],
        2 => vec![Cpe],
        3 => vec![Sce, Cpe],
        4 => vec![Cpe, Cpe],
        5 => vec![Sce, Cpe, Cpe],
        6 => vec![Sce, Cpe, Cpe, Lfe],
        7 => vec![Sce, Cpe, Cpe, Sce, Lfe],
        8 => vec![Sce, Cpe, Cpe, Cpe, Lfe],
        _ => return None,
    })
}

/// In-band / ASC program config of the 4- and 7-channel layouts (`channelConfiguration` 0 resp. PCE in AU 0), derived from the layout only (no rate acceptance check):
/// `None` for the other channel counts.
pub fn layout_pce(rate: u32, channels: usize) -> Option<PceInfo> {
    if channels != 4 && channels != 7 {
        return None;
    }
    let kinds = layout_kinds(channels)?;
    let sf_index = sf_index_of_rate(rate)?;
    let (mut n_sce, mut n_cpe, mut n_lfe) = (0u8, 0u8, 0u8);
    let elems: Vec<(ElementKind, u8)> = kinds
        .iter()
        .map(|&k| {
            let c = match k {
                ElementKind::Sce => &mut n_sce,
                ElementKind::Cpe => &mut n_cpe,
                ElementKind::Lfe => &mut n_lfe,
            };
            let t = *c;
            *c += 1;
            (k, t)
        })
        .collect();
    Some(make_pce(&elems, channels as u32, 0, sf_index))
}

/// Number of full-bandwidth channels the bitrate is split over (`fbwChansInCfg`; the LFE of 6/7/8-channel layouts is excluded).
pub fn full_bandwidth_channels(channels: usize) -> u32 {
    match channels {
        6..=8 => channels as u32 - 1,
        _ => channels as u32,
    }
}

/// Bits of the in-band PCE (`ID_PCE` + body, byte aligned relative to the raw_data_block, `comment_field_bytes`) = `8 * BSE[0x480]`.
pub fn pce_inband_bits(pce: &PceInfo) -> i32 {
    let n = pce.front.len() + pce.side.len() + pce.back.len();
    // 3 (ID_PCE) + tag 4 + profile 2 + sf 4 + counts (4+4+4+2) + assoc 3 + cc 4 + mixdown flags 3 = 37 bits before the element lists
    let bits = 37 + 5 * n + 4 * pce.lfe.len();
    (bits.div_ceil(8) * 8 + 8) as i32
}

/// Everything derived from `(rate, channels)` for the engine.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub channels: usize,
    pub quality: u32,
    /// MPEG-4 sampling-frequency index.
    pub sf_index: u8,
    /// per-full-bandwidth-channel bitrate `R`.
    pub per_channel_bitrate: u32,
    pub elements: Vec<ElementCfg>,
    /// element index of every coded channel.
    pub chan_element: Vec<usize>,
    /// `_SetAudioBandwidth` struct `{bw, ratio, bw2, f3, f4, 60.0, fs, R}` of every coded channel.
    pub bandwidth: Vec<[f32; 8]>,
    pub pam: PamConfig,
    pub fb: FbConfig,
    pub quant: QuantConfig,
    /// intensity configuration of every CPE element (None for SCE/LFE), indexed by element.
    pub intensity: Vec<Option<IntensityConfig>>,
    /// Program config element of access unit 0: written in-band into AU 0 of 4- and 7-channel streams. The 4-channel layout also carries it in the
    /// `AudioSpecificConfig` (`channelConfiguration` 0); 7 channels signal `channelConfiguration` 11 and have the PCE in AU 0 only.
    pub pce: Option<PceInfo>,
}

impl EngineConfig {
    /// Derive the engine configuration. `Err` for formats AudioToolbox refuses.
    pub fn new(rate: u32, channels: u32) -> Result<Self, ConfigError> {
        accepts(rate, channels)?;
        let channels = channels as usize;
        let kinds = layout_kinds(channels).ok_or(ConfigError::Unsupported { rate, channels: channels as u32 })?;
        let sf_index = sf_index_of_rate(rate).ok_or(ConfigError::Unsupported { rate, channels: channels as u32 })?;
        let brat = ENGINE_BRAT;
        let per_channel_bitrate = brat / full_bandwidth_channels(channels);

        // elements, tags, channel -> element map
        let mut elements = Vec::new();
        let mut chan_element = Vec::new();
        let (mut n_sce, mut n_cpe, mut n_lfe) = (0u8, 0u8, 0u8);
        let mut c = 0usize;
        for (e, &kind) in kinds.iter().enumerate() {
            let counter = match kind {
                ElementKind::Sce => &mut n_sce,
                ElementKind::Cpe => &mut n_cpe,
                ElementKind::Lfe => &mut n_lfe,
            };
            let el = ElementCfg { kind, tag: *counter, first_channel: c };
            *counter += 1;
            for _ in 0..el.nch() {
                chan_element.push(e);
            }
            c += el.nch();
            elements.push(el);
        }
        debug_assert_eq!(c, channels);

        // bandwidth structs per channel (_SetAudioBandwidth with the engine arguments)
        let mut bandwidth = Vec::with_capacity(channels);
        for ch in 0..channels {
            let kind = elements[chan_element[ch]].kind;
            let cpe_of_3 = channels == 3 && kind == ElementKind::Cpe;
            let (idx, red, r) = engine_element_args(channels, cpe_of_3, brat);
            let mut st = [0.0f32; 8];
            st[6] = rate as f32;
            st[7] = f32::from_bits(r);
            let args = BandwidthArgs { reduction: red, factor: 1.0, fs: rate, idx, p6: 0, p7: 1, quality: ENGINE_QUALITY, p9: 0, p10: 0, p11: false };
            set_audio_bandwidth(&args, &mut st).map_err(|_| ConfigError::Stage("_SetAudioBandwidth"))?;
            bandwidth.push(st);
        }

        // _getAverageBandWidthRatio (0xa024): element order, LFE skipped, f32 sum / count
        let mut sum = 0.0f32;
        let mut count = 0i32;
        for el in &elements {
            match el.kind {
                ElementKind::Sce => {
                    sum += bandwidth[el.first_channel][1];
                    count += 1;
                }
                ElementKind::Cpe => {
                    sum += bandwidth[el.first_channel][1];
                    sum += bandwidth[el.first_channel + 1][1];
                    count += 2;
                }
                ElementKind::Lfe => {}
            }
        }
        let bw_ratio = if count < 1 { f32::NAN } else { sum / count as f32 };
        let pam = PamConfig { rate, nch: channels as u32, bitrate_per_channel: per_channel_bitrate, quality: ENGINE_QUALITY, bw_ratio, acbf: ENGINE_ACBF as u16 };

        let fb = FbConfig::engine(rate, channels, brat, ENGINE_QUALITY);

        // quantiser / rate configuration: the element index of every coded channel
        let mut tags = [0i32; 8];
        for (e, el) in elements.iter().enumerate() {
            for j in 0..el.nch() {
                tags[el.first_channel + j] = e as i32;
            }
        }
        let quant = QuantConfig {
            nch: channels,
            quality: ENGINE_QUALITY,
            acbf: ENGINE_ACBF,
            kernel: KernelConfig { nch: channels, nlines: 1024, quality: ENGINE_QUALITY, ngroups: elements.len(), tags },
            rate: RateConfig { sample_rate: rate, brat, frame_len: 1024, nch: channels, acbf: ENGINE_ACBF },
            pam: PamParams::default(),
        };

        // intensity: one IC per CPE; sfb tables from the PAM's tables, quiet thresholds = `_initThresholdInQuiet` outputs
        let mut intensity = Vec::new();
        let pp = PamParam::new(pam, 0).map_err(|_| ConfigError::Stage("PamParam (sfb table)"))?;
        let enabled = intensity_enabled(channels as u32, ENGINE_QUALITY);
        for el in &elements {
            if el.kind == ElementKind::Cpe {
                let off_long: Vec<u16> = pp.sfb_off_long.iter().map(|&v| v as u16).collect();
                // short offsets of one window (the PAM keeps them x 8 in the 1024-line domain)
                let off_short: Vec<u16> = pp.sfb_off_short_x8.iter().map(|&v| (v / 8) as u16).collect();
                intensity.push(Some(IntensityConfig::new(enabled, &off_long, &off_short, &pp.quiet_long_pow, &pp.quiet_short_pow, per_channel_bitrate)));
            } else {
                intensity.push(None);
            }
        }

        let pce = layout_pce(rate, channels);

        Ok(EngineConfig { channels, quality: ENGINE_QUALITY, sf_index, per_channel_bitrate, elements, chan_element, bandwidth, pam, fb, quant, intensity, pce })
    }

    /// `BandwidthRec` of coded channel `ch` (`PCF+0x10+0x30*ch`: floats `[0], [2], [3], [6]`).
    pub fn bandwidth_rec(&self, ch: usize) -> BandwidthRec {
        let st = &self.bandwidth[ch];
        BandwidthRec { bw: st[0], f2: st[2], f3: st[3], rate: st[6] }
    }

    /// Per-channel psychoacoustic band layouts (`_CreatePsyParam` x2 + band-edge weights).
    ///
    /// The LFE channel is band-limited to [`LFE_CUTOFF_LINES`] long-window lines: its psy record has a hard lowpass at line 12 (no taper, no band-edge
    /// threshold weight), which `_AdvanceCodChannelPam` applies before the band energies and which makes `_calculateMaxSfb` return the number of sfbs
    /// starting below line 12 (3 at 32/44.1/48 kHz, 2 at 16 kHz). Evidence: the `max_sfb` of the LFE access units of the 6/7/8-channel fixtures and the
    /// byte-identical 16 kHz 7-channel streams (band 1 straddles the cutoff, so energies, thresholds and quantised lines pin the model down).
    pub fn psy_channels(&self) -> Result<Vec<PsyChannelParam>, ConfigError> {
        (0..self.channels)
            .map(|c| {
                let mut rec = self.bandwidth_rec(c);
                let lfe = self.elements[self.chan_element[c]].kind == ElementKind::Lfe;
                if lfe {
                    let bw = LFE_CUTOFF_LINES as f32 * rec.rate / 2048.0;
                    rec.bw = bw;
                    rec.f2 = bw;
                }
                let mut p = PsyChannelParam::new(rec).map_err(|_| ConfigError::Stage("PsyChannelParam"))?;
                if lfe {
                    p.long.edge_count = 0;
                    p.short.edge_count = 0;
                }
                Ok(p)
            })
            .collect()
    }

    /// Number of bits of the in-band PCE (0 without PCE).
    pub fn pce_bits(&self) -> i32 {
        self.pce.as_ref().map(pce_inband_bits).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fewest channels at which AudioToolbox takes `'brat'` 256000, per rate (measured at every channel count 1..=8; none at 8, 11.025 and 12 kHz).
    const FEWEST_CHANNELS: [(u32, Option<u32>); 9] =
        [(8000, None), (11025, None), (12000, None), (16000, Some(7)), (22050, Some(4)), (24000, Some(4)), (32000, Some(3)), (44100, Some(1)), (48000, Some(1))];

    #[test]
    fn the_accepted_set_is_the_measured_one() {
        let mut accepted = 0;
        for (rate, fewest) in FEWEST_CHANNELS {
            for channels in 1..=8 {
                let verdict = accepts(rate, channels);
                if fewest.is_some_and(|f| channels >= f) {
                    assert_eq!(verdict, Ok(()), "{rate} Hz x {channels}");
                    accepted += 1;
                } else {
                    assert_eq!(verdict, Err(ConfigError::Refused { rate, channels }), "{rate} Hz x {channels}");
                }
            }
        }
        assert_eq!(accepted, 34, "16 + 6 + 5 + 5 + 2 configurations");
    }

    #[test]
    fn formats_without_a_converter_are_unsupported() {
        for (rate, channels) in [(44100, 0), (44100, 9), (48000, 64), (7350, 2), (64000, 2), (96000, 2), (1, 1), (0, 2), (44110 + 1, 2), (u32::MAX, 2)] {
            assert_eq!(accepts(rate, channels), Err(ConfigError::Unsupported { rate, channels }), "{rate} Hz x {channels}");
        }
    }

    #[test]
    fn a_rate_within_ten_hertz_of_an_aac_rate_is_served_but_not_reproduced() {
        // AudioToolbox rounds these to the AAC rate for its file but encodes with the exact value: refused unless the converter refuses 256 kbit/s anyway
        for (rate, channels) in [(44090, 2), (44101, 2), (44110, 8), (47990, 1), (48010, 6), (32001, 3), (16010, 7)] {
            assert_eq!(accepts(rate, channels), Err(ConfigError::NotReproduced { rate, channels }), "{rate} Hz x {channels}");
        }
        assert_eq!(accepts(32001, 2), Err(ConfigError::Refused { rate: 32001, channels: 2 }));
        assert_eq!(accepts(8010, 1), Err(ConfigError::Refused { rate: 8010, channels: 1 }));
    }

    #[test]
    fn the_rate_window_is_ten_hertz_inclusive() {
        for nominal in NOMINAL_RATES {
            let n = f64::from(nominal);
            assert_eq!(nominal_rate(n), Some(nominal));
            assert_eq!(nominal_rate(n - 10.0), Some(nominal));
            assert_eq!(nominal_rate(n + 10.0), Some(nominal));
            assert_eq!(nominal_rate(n + 10.01), None, "{nominal}");
            assert_eq!(nominal_rate(n - 10.01), None, "{nominal}");
        }
        assert_eq!(nominal_rate(44100.5), Some(44100));
        assert_eq!(nominal_rate(f64::NAN), None);
        assert_eq!(nominal_rate(f64::INFINITY), None);
        assert_eq!(nominal_rate(-48000.0), None);
    }

    #[test]
    fn layouts_follow_the_channel_count() {
        use ElementKind::*;
        let kinds = |c| layout_kinds(c).unwrap();
        assert_eq!(kinds(1), [Sce]);
        assert_eq!(kinds(2), [Cpe]);
        assert_eq!(kinds(6), [Sce, Cpe, Cpe, Lfe]);
        assert_eq!(kinds(8), [Sce, Cpe, Cpe, Cpe, Lfe]);
        assert!(layout_kinds(0).is_none() && layout_kinds(9).is_none());
        // 4 and 7 channels carry a program config element, the others none
        for channels in 1..=8 {
            assert_eq!(layout_pce(44100, channels).is_some(), channels == 4 || channels == 7, "{channels}");
        }
        assert_eq!(full_bandwidth_channels(6), 5);
        assert_eq!(full_bandwidth_channels(8), 7);
        assert_eq!(full_bandwidth_channels(5), 5);
        // the per-channel bitrate of the engine's 256 kbit/s
        let eight = EngineConfig::new(48000, 8).unwrap();
        assert_eq!(eight.per_channel_bitrate, 256_000 / 7);
        assert_eq!(eight.elements.len(), 5);
        assert_eq!(eight.pce_bits(), 0);
        assert!(EngineConfig::new(48000, 7).unwrap().pce_bits() > 0);
    }
}
