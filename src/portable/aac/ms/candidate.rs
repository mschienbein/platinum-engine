//! S1: the M/S candidate set built by `AACEncoder::EncodeFrame` (asm 0x131c3c..0x131d60, 0x131fa4..0x132130)
//! for every CPE with `common_window && (ms || is enabled)`:
//!
//! ```text
//! _getMsMDCT(1024, L, R, M, S)                                   // L/R = psyOut#1 spectra after E11: lowpassed, TNS-filtered, UNSORTED (window-major)
//! _AdvanceCodChannelPam(codM, M, thrM_in = 0, wseq, PCF(ch0), lowpass = 0)   // energy = sum x^2, thr = max(2*0, 1.0) = 1.0 (thrmode != 0)
//! _AdvanceCodChannelPam(codS, S, thrS_in = 0, wseq, PCF(ch0), lowpass = 0)
//! _AdvancePsyOut(psyOut#2[ch0], psyparam, is_pair = 1, psychan[ch0], codM, .., wseq, wseq == 2)
//! _AdvancePsyOut(psyOut#2[ch1], psyparam, is_pair = 1, psychan[ch1], codS, .., wseq, wseq == 2)
//! _ProcessPsyOutMS(psyOut#2[ch0]) ; _ProcessPsyOutMS(psyOut#2[ch1])   // short: sort the spectrum into (group, band, window, line) order; max_sfb
//! max_sfb(M) = max_sfb(S) = max(max_sfb(M), max_sfb(S))
//! ```
//! All pieces live in `psy` (`advance_cod_channel_pam`, `advance_psy_out`, `finalize_psy_out`, `unify_pair_max_sfb`); this module only
//! sequences them with the M/S arguments (placeholder thresholds, no lowpass) and `get_ms_mdct`.  In the engine config (`thrmode != 0`) the legacy
//! `_getMsEnergy`/`_getMsThresholds_Neon` kernels are not executed.
//!
//! The candidate must be built from the UNSORTED L/R spectra, i.e. before `finalize_psy_out` runs on psyOut#1.

use crate::portable::aac::ms::mdct::get_ms_mdct;
use crate::portable::aac::psy::glue::{advance_cod_channel_pam, advance_psy_out, finalize_psy_out, unify_pair_max_sfb, CodChannel, PsyChannelParam, ShortGrouping};
use crate::portable::aac::types::PsyOut;

/// Scratch of one CPE candidate build (the `MSChannel` CodChannels `+0xb40/+0xb48` and the sort buffer); contents never survive a call.
#[derive(Clone, Debug)]
pub struct MsCandidateScratch {
    pub cod_m: CodChannel,
    pub cod_s: CodChannel,
    sort: Vec<f32>,
    zero_thr: [f32; 120],
}

impl MsCandidateScratch {
    pub fn new() -> Self {
        MsCandidateScratch { cod_m: CodChannel::default(), cod_s: CodChannel::default(), sort: vec![0.0; 1024], zero_thr: [0.0; 120] }
    }
}

impl Default for MsCandidateScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds psyOut#2 (`m` = M candidate, `s` = S candidate) of one CPE from psyOut#1 `l`/`r` (E11 state: spectra unsorted, geometry set).
/// * `layout` = `PsyChannelParam` of the CPE's first channel (Apple passes `PCF + ch0 * 0x30` to both M and S calls);
/// * `p1ac` = `PamParam::p1ac` (`P+0x1ac`, 0 at 44.1/48 kHz), `field_5bc` = `PsyChannel+0x23c` (window shape), `wseq` = the pair's window sequence;
/// * `grouping` = the pair's short grouping (only read for `wseq == 2`; after the CPE DP both channels share it).
/// On return `m`/`s` equal Apple's M/S slots at the entry of the M/S block except for their stale `pe`: sorted spectra, `energy`, placeholder `thr`
/// (1.0 per window, summed per group), geometry, `max_sfb` unified, `flags` cleared.
pub fn build_ms_candidate(l: &PsyOut, r: &PsyOut, layout: &PsyChannelParam, p1ac: i32, field_5bc: i32, wseq: u32, grouping: &ShortGrouping, m: &mut PsyOut, s: &mut PsyOut,
                          sc: &mut MsCandidateScratch) {
    let n = layout.long.sfb.n as usize;
    get_ms_mdct(n, &l.spectrum, &r.spectrum, &mut m.spectrum, &mut s.spectrum);
    advance_cod_channel_pam(&mut sc.cod_m, &mut m.spectrum, &sc.zero_thr, wseq, layout, false);
    advance_cod_channel_pam(&mut sc.cod_s, &mut s.spectrum, &sc.zero_thr, wseq, layout, false);
    let is_short = wseq == 2;
    advance_psy_out(m, p1ac, true, field_5bc, &sc.cod_m, None, wseq, is_short, grouping, layout);
    advance_psy_out(s, p1ac, true, field_5bc, &sc.cod_s, None, wseq, is_short, grouping, layout);
    finalize_psy_out(m, layout, &mut sc.sort);
    finalize_psy_out(s, layout, &mut sc.sort);
    unify_pair_max_sfb(m, s);
}
