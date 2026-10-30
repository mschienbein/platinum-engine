//! Front end of `AACEncoder::EncodeFrame` (steps E1-E11): filterbank (`fb`) -> psychoacoustic model per channel
//! (`psy`) -> short-window grouping -> `_AdvanceCodChannelPam` (bandwidth lowpass + band energies) -> `_AdvancePsyOut` (psyOut#1).
//!
//! Everything here depends only on `fb` + `psy` (no stereo stages, no quantiser, no rate feedback except the bias `B` that
//! [`Front::adapt`] hands to `_adaptPamParam`).

use crate::portable::aac::encoder::config::{ConfigError, EngineConfig};
use crate::portable::aac::fb::blocksw::TrdeCfg;
use crate::portable::aac::fb::grouping::{decide_channel_grouping, pair_merge};
use crate::portable::aac::fb::{ChannelFb, FbState};
use crate::portable::aac::psy::{
    advance_cod_channel_pam, advance_psy_out, analyse_channel, finalize_psy_out, short_energies_needed, unify_pair_max_sfb, CodChannel, PamCopy, PamInput, PamParam, PsyChannelParam, ShortGrouping,
    ThrChannel,
};
use crate::portable::aac::types::{ElementKind, PsyOut};

/// One channel after E11: everything the later stages need.
pub(super) struct ChannelFrame {
    /// Filterbank output (coded / psy spectra, TNS object, pre-echo mask, window sequence).
    pub(super) fb: ChannelFb,
    /// `PsyChannel+0x240` as the block switch left it (before the PAM may clear it): drives the `psyOut+0x9d0` flag.
    flags_in: u16,
    /// `thr_chan+0x7ec` / `+0x9cc` rows (8 windows x 15).
    copy: PamCopy,
    /// `CodChannel` of the L/R slot.
    cod: CodChannel,
    /// psyOut#1 after `_AdvancePsyOut`: geometry, energy, thr; `spectrum` = coding spectrum after TNS and bandwidth lowpass in window order
    /// (not yet group-sorted), `max_sfb` not yet computed.
    pub(super) psy: PsyOut,
    /// Window grouping applied to this channel (count 1 for long-type frames).
    pub(super) grouping: ShortGrouping,
}

/// Output of [`Front::analyse`] for one access unit.
pub(super) struct FrontFrame {
    pub(super) au_index: u64,
    pub(super) channels: Vec<ChannelFrame>,
}

fn rows(a: &[f32; 120]) -> [[f32; 15]; 8] {
    std::array::from_fn(|w| std::array::from_fn(|k| a[15 * w + k]))
}

/// Per-stream state of the front end.
pub(super) struct Front {
    cfg: EngineConfig,
    fb: FbState,
    pub(super) pam: PamParam,
    thr: Vec<ThrChannel>,
    pub(super) psy_ch: Vec<PsyChannelParam>,
}

impl Front {
    /// `initial_b` = the bias index of the rate state at stream start.
    pub(super) fn new(cfg: &EngineConfig, initial_b: i32) -> Result<Self, ConfigError> {
        let pam = PamParam::new(cfg.pam, initial_b).map_err(|_| ConfigError::Stage("PamParam::new"))?;
        // the PAM computes the TRDE block (`P+0x1b0`); it is B-independent, so one hand-over at open is enough
        let w: [u32; 16] = std::array::from_fn(|i| pam.trde[i].to_bits());
        let fb = FbState::new(&cfg.fb, TrdeCfg::from_words(&w));
        let thr = (0..cfg.channels).map(|_| ThrChannel::new(&pam)).collect();
        let psy_ch = cfg.psy_channels()?;
        Ok(Self { cfg: cfg.clone(), fb, pam, thr, psy_ch })
    }

    /// `_adaptPamParam(B)` after an AU in which the bias index changed.
    pub(super) fn adapt(&mut self, b: i32) {
        self.pam.adapt(b);
    }

    /// E1-E11 for one block of `1024` client frames per channel (planar, unscaled float32).
    pub(super) fn analyse(&mut self, input: &[&[f32]]) -> FrontFrame {
        let nch = self.cfg.channels;
        let fbf = self.fb.process_frame(input);
        let quality = self.cfg.quality;
        let mut channels: Vec<ChannelFrame> = Vec::with_capacity(nch);
        // E5-E7 (+ E8 per channel): power spectra, PAM, copy/weighting; short grouping decision
        let mut per_channel_group: Vec<ShortGrouping> = Vec::with_capacity(nch);
        for (c, fbc) in fbf.channels.into_iter().enumerate() {
            let own_a = self.cfg.fb.channel[c].own_bank_a;
            let flags_in = fbc.pre_echo_mask;
            // the PAM clears bit 0 of its copy when the LF/HF energy ratio gate fails; nothing later reads the cleared word
            let mut pam_flags = flags_in;
            let tns_w = fbc.tns.as_ref().filter(|t| t.active).map(|t| &t.weights[..]);
            let empty: [f32; 0] = [];
            let re_short: &[f32] = match &fbc.psy_short {
                Some(s) => &s[..],
                None => &empty,
            };
            let inp = PamInput {
                wseq: fbc.wseq as u32,
                quality,
                re_long: &fbc.psy_long[..],
                im_long: None,
                re_short,
                im_short: None,
                c131: own_a,
                tns_weights: tns_w,
            };
            let mut copy = PamCopy::default();
            analyse_channel(&self.pam, &mut self.thr[c], &self.psy_ch[c], &inp, &mut pam_flags, &mut copy);
            let g = if fbc.wseq == 2 {
                let (lens, _) = decide_channel_grouping(&rows(&copy.thr), self.psy_ch[c].short.sfb.nsfb);
                let mut gl = [0i32; 8];
                for (i, &l) in lens.iter().enumerate() {
                    gl[i] = l as i32;
                }
                ShortGrouping { group_len: gl, count: lens.len() as i32 }
            } else {
                ShortGrouping { group_len: [1, 0, 0, 0, 0, 0, 0, 0], count: 1 }
            };
            per_channel_group.push(g);
            let mut psy = PsyOut::new();
            psy.spectrum.copy_from_slice(&fbc.coded);
            channels.push(ChannelFrame { fb: fbc, flags_in, copy, cod: CodChannel::default(), psy, grouping: g });
        }
        // E9: _AdvanceCodChannelPam (lowpass in place on the coding spectrum)
        for (c, ch) in channels.iter_mut().enumerate() {
            advance_cod_channel_pam(&mut ch.cod, &mut ch.psy.spectrum, &ch.copy.thr, ch.fb.wseq as u32, &self.psy_ch[c], true);
        }
        // E10: CPE common-window grouping (DP over `_regionBoundaryTab` on the PAM energies)
        for el in &self.cfg.elements {
            if el.kind != ElementKind::Cpe {
                continue;
            }
            let (a, b) = (el.first_channel, el.first_channel + 1);
            if channels[a].fb.wseq == 2 && channels[b].fb.wseq == 2 {
                let nsfb = self.psy_ch[a].short.sfb.nsfb;
                let (lens, _) = pair_merge(&rows(&channels[a].copy.en), &rows(&channels[b].copy.en), nsfb, per_channel_group[a].count as usize, per_channel_group[b].count as usize);
                let mut gl = [0i32; 8];
                for (i, &l) in lens.iter().enumerate() {
                    gl[i] = l as i32;
                }
                let g = ShortGrouping { group_len: gl, count: lens.len() as i32 };
                channels[a].grouping = g;
                channels[b].grouping = g;
            }
        }
        // E11: _AdvancePsyOut(psyOut#1)
        for (c, ch) in channels.iter_mut().enumerate() {
            let is_short = short_energies_needed(self.cfg.fb.channel[c].own_bank_a, ch.flags_in, ch.fb.wseq as u32);
            let tns = ch.fb.tns.as_ref().map(|t| t.to_types());
            advance_psy_out(&mut ch.psy, self.pam.p1ac, false, 0, &ch.cod, tns, ch.fb.wseq as u32, is_short, &ch.grouping, &self.psy_ch[c]);
        }
        FrontFrame { au_index: fbf.au_index, channels }
    }

    /// E14: `_sortShortSpectrum` (EIGHT_SHORT) + `_calculateMaxSfb` per channel, CPE pair unification, and the TNS clear for `max_sfb == 0`
    /// (EncodeFrame 0x13225c) on psyOut#1 of every channel. The LFE's 3 (resp. 2 at 16 kHz) coded bands follow from its 12-line psy lowpass
    /// ([`EngineConfig::psy_channels`]).
    pub(super) fn finalize(&self, ff: &mut FrontFrame) {
        let mut scratch = vec![0.0f32; 1024];
        for (c, ch) in ff.channels.iter_mut().enumerate() {
            finalize_psy_out(&mut ch.psy, &self.psy_ch[c], &mut scratch);
        }
        for el in &self.cfg.elements {
            if el.kind == ElementKind::Cpe {
                let (lo, hi) = ff.channels.split_at_mut(el.first_channel + 1);
                unify_pair_max_sfb(&mut lo[el.first_channel].psy, &mut hi[0].psy);
            }
        }
        for ch in ff.channels.iter_mut() {
            if ch.psy.max_sfb == 0 && let Some(t) = ch.psy.tns.as_mut() {
                t.active = false;
                t.ranges.present = false;
            }
        }
    }
}
