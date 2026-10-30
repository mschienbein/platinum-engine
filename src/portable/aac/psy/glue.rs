//! The EncodeFrame glue that belongs to the psychoacoustic stage (steps E5-E7, E9, E11, E14): power spectra from
//! the (TNS-filtered) MDCT, copy / zero-fill / band-edge weighting / TNS-gain scaling of the PAM thresholds, `_AdvanceCodChannelPam`,
//! `_getMdctEnergy_simd`, `_specWinLowpass`, `_AdvancePsyOut`, `_sortShortSpectrum`, `_calculateMaxSfb`, plus the per-channel band
//! configuration `_CreatePsyParam` / `_InitThresholdWeightingAtBandEdge` / `_CalculateSFBCenterFreqs`.
//!
//! Apple decompiles of `00040378`, `00040580`, `00043dcc`, `0004a1cc`, `00049f34`, `0004a130`, `00009ca0`, `00009f30`, `0000c3fc` and the
//! EncodeFrame sites of `AACEncoder::EncodeFrame`.

use crate::portable::aac::fb::bandwidth::lowpass_spec;
use crate::portable::aac::psy::param::{get_sfb_param, PamParam, PsyError, SfbInfo};
use crate::portable::aac::psy::pam::ThrChannel;
use crate::portable::aac::psy::vdsp::{vsmul_inplace, vsq, vthr_inplace, zvmags};
use crate::portable::aac::types::{PsyOut, TnsChannel, MAX_BANDS};

/// Per-element band configuration record `rec` (`PCF+0x10+0x30*ch`, float fields [0], [2], [3], [6]) produced by the bandwidth model:
/// `bw` = analysed audio bandwidth in Hz, `f2`/`f3` = the two helper bandwidths of `_SetAudioBandwidth`
/// (equal to `bw` in the normal case), `rate` = sample rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandwidthRec {
    pub bw: f32,
    pub f2: f32,
    pub f3: f32,
    pub rate: f32,
}

/// `PsyParam` (0x40 bytes, `rec+0x20` long / `rec+0x28` short): sfb layout, lowpass cutoff, taper window and the band-edge weight.
#[derive(Clone, Debug)]
pub struct PsyParam {
    pub sfb: SfbInfo,
    /// +0x0c lowpass cutoff line.
    pub cutoff: i32,
    /// +0x18 taper length L = cutoff - start (0 = no taper); the start line is +0x10.
    pub taper_len: i32,
    /// +0x20 Blackman taper `0.42 - 0.5 cos(x) + 0.08 cos(2x)`, `x = 2 pi (L+i)/(2L-1)`, i = 0..L.
    pub taper: Vec<f32>,
    /// +0x28 band-edge weights (`memset_pattern16` default 1.0, 1.0), +0x30 first affected sfb, +0x34 count (0 or 1).
    pub edge_weight: [f32; 2],
    pub edge_sfb: i32,
    pub edge_count: i32,
}

/// `_CreatePsyParam(rate, rec, 2N, 1024, kind)` followed by `_InitThresholdWeightingAtBandEdge(rec, psyparam)`.
pub fn create_psy_param(rec: &BandwidthRec, short: bool) -> Result<PsyParam, PsyError> {
    let sfb = get_sfb_param(rec.rate, short).ok_or(PsyError::UnsupportedSampleRate(rec.rate as u32))?;
    // cutoff line, taper start and the falling Blackman taper `0.42 - 0.5 cos(x) + 0.08 cos(2x)`, `x = 2 pi (L+i)/(2L-1)`
    let lp = lowpass_spec(rec.bw, rec.f2, rec.rate, sfb.n as u32);
    let (cutoff, taper_start) = (lp.cutoff as i32, lp.edge2 as i32);
    let mut pp = PsyParam {
        sfb,
        cutoff,
        taper_len: cutoff - taper_start,
        taper: lp.taper,
        edge_weight: [1.0, 1.0],
        edge_sfb: 0,
        edge_count: 0,
    };
    init_threshold_weighting_at_band_edge(rec, &mut pp);
    Ok(pp)
}

/// `_InitThresholdWeightingAtBandEdge` (0x9f30): weight (< 1) for the sfb straddling the lowpass cutoff.
fn init_threshold_weighting_at_band_edge(rec: &BandwidthRec, pp: &mut PsyParam) {
    pp.edge_weight = [1.0, 1.0];
    pp.edge_sfb = 0;
    pp.edge_count = 0;
    if rec.f2 == rec.bw && (rec.f2 <= rec.f3 || rec.f3 == 0.0) {
        let n = pp.sfb.n as f32;
        let x = (rec.bw * n) / (rec.rate * 0.5) + 0.5;
        let cut = (x as i32) as f32;
        if cut < n {
            let mut u = pp.sfb.nsfb;
            loop {
                if u < 2 {
                    break;
                }
                let nu = u - 1;
                // do { uVar4 = uVar2; if (uVar4 < 2) break; uVar2 = uVar4 - 1 } while (cut < offset[uVar4 - 1])
                if !(cut < pp.sfb.offsets[u - 1] as f32) {
                    break;
                }
                u = nu;
            }
            let cnt = (x as i32) - pp.sfb.offsets[u - 1] as i32;
            if cnt > 0 {
                pp.edge_weight[0] = (cnt as f32) / (pp.sfb.widths[u - 1] as f32);
                pp.edge_sfb = (u - 1) as i32;
                pp.edge_count = 1;
            }
        }
    }
}

/// Band configuration of one channel: the long and short `PsyParam` (`rec+0x20`, `rec+0x28`).
#[derive(Clone, Debug)]
pub struct PsyChannelParam {
    pub long: PsyParam,
    pub short: PsyParam,
}

impl PsyChannelParam {
    pub fn new(rec: BandwidthRec) -> Result<Self, PsyError> {
        Ok(PsyChannelParam { long: create_psy_param(&rec, false)?, short: create_psy_param(&rec, true)? })
    }
}

/// `_CalculateSFBCenterFreqs` (0xc3fc) result: sfb centre frequencies (Hz) for the long (49 entries used) and short tables, and the
/// first sfb whose centre is >= 18000 Hz (`[0xf2]` long, `[0xf1]` short).  Used by the bandwidth/TNS code, not by the PAM.
#[derive(Clone, Debug)]
pub struct SfbCenterFreqs {
    pub long: Vec<f32>,
    pub short: Vec<f32>,
    pub first_ge_18k_long: u32,
    pub first_ge_18k_short: u32,
}

/// `_CalculateSFBCenterFreqs(&out, rate, granule = 1024, is_ld = 0)`.  Centre = `((rate/2) * (offset + width*0.5 + 0.5)) / N` with
/// `N = 1024` (long) / `1024 * 0.125 = 128` (short); the asm evaluates `offset + width*0.5` then `+ 0.5` (unfused).
pub fn calculate_sfb_center_freqs(rate: u32) -> Result<SfbCenterFreqs, PsyError> {
    let rf = rate as f32;
    let long = get_sfb_param(rf, false).ok_or(PsyError::UnsupportedSampleRate(rate))?;
    let short = get_sfb_param(rf, true).ok_or(PsyError::UnsupportedSampleRate(rate))?;
    let centers = |s: &SfbInfo, n: f32| -> Vec<f32> {
        (0..s.nsfb)
            .map(|k| {
                let w = s.widths[k] as f32;
                let o = s.offsets[k] as f32;
                ((rf / 2.0) * (o + w * 0.5 + 0.5)) / n
            })
            .collect()
    };
    let cl = centers(&long, 1024.0);
    let cs = centers(&short, 1024.0 * 0.125);
    let thr = 18000.0f32;
    let first = |c: &[f32]| -> u32 {
        // `if (c[0] < thr) { scan from 1; first index with c >= thr; if none: nsfb } else 0`
        if c[0] < thr {
            for (i, &x) in c.iter().enumerate().skip(1) {
                if x >= thr {
                    return i as u32;
                }
            }
            c.len() as u32
        } else {
            0
        }
    };
    let (fl, fs) = (first(&cl), first(&cs));
    Ok(SfbCenterFreqs { long: cl, short: cs, first_ge_18k_long: fl, first_ge_18k_short: fs })
}

// ------------------------------------------------------------------------------------------------------------------------------
// E5: power spectra

/// E5, quality <= 0x40 (`PsyChannel+0x148 == +0x150`, MDST aliased to MDCT): `E = (x*x) * 2.0` via `vDSP_vsq` + `vDSP_vsmul(2.0)`.
/// Long: `re` = 1024 lines; short window `w`: its 128 lines.
pub fn power_spectrum_alias(re: &[f32], out: &mut [f32]) {
    vsq(re, out);
    vsmul_inplace(out, 2.0);
}

/// E5, quality 0x60 (separate MDST): `vDSP_zvmags(re, im)` into `out`; `out_align` = float misalignment of the OUTPUT pointer
/// modulo 4 (0 for the engine's buffers: everything is then the unfused 4-lane body for n % 4 == 0).
pub fn power_spectrum_zvmags(re: &[f32], im: &[f32], out: &mut [f32], out_align: usize) {
    zvmags(re, im, out, out_align);
}

/// E5 predicate: the 8 short-window energy spectra are computed when `PsyChannel+0x131 != 0 || +0x240 != 0 || wseq == 2`.
pub fn short_energies_needed(c131: bool, flags_240: u16, wseq: u32) -> bool {
    c131 || flags_240 != 0 || wseq == 2
}

// ------------------------------------------------------------------------------------------------------------------------------
// E7: PAM outputs -> thr7ec/en9cc blocks

/// `ThrChannel+0x7ec` (thresholds) and `+0x9cc` (energies): 8 windows x 15 floats (stride 0x3c); long frames use window 0 only.
#[derive(Clone, Debug)]
pub struct PamCopy {
    pub thr: [f32; 120],
    pub en: [f32; 120],
}

impl Default for PamCopy {
    fn default() -> Self {
        PamCopy { thr: [0.0; 120], en: [0.0; 120] }
    }
}

/// E7 part 1 (`EncodeFrame` raw L1587-1627): copy the model outputs after `_pamMonaural` and zero-fill up to the channel's sfb
/// count.  Long (wseq 0,1,3): `thr7ec[0..nS] = T9`, `en9cc[0..nS] = T7`; short (2): window w -> `T10[w]`/`T8[w]` at `15*w`.
/// `nsfb_long`/`nsfb_short` = sfb counts of the channel's `PsyParam` (`sfbinfo[1]`), zero-filled beyond `P.nS`/`P.nSh`.
pub fn copy_pam_outputs(p: &PamParam, t: &ThrChannel, wseq: u32, nsfb_long: usize, nsfb_short: usize, out: &mut PamCopy) {
    if wseq == 2 {
        for w in 0..8 {
            let o = 15 * w;
            out.thr[o..o + p.n_sh].copy_from_slice(&t.thr_short[w][..p.n_sh]);
            out.en[o..o + p.n_sh].copy_from_slice(&t.t8[w][..p.n_sh]);
            for k in p.n_sh..nsfb_short {
                out.thr[o + k] = 0.0;
                out.en[o + k] = 0.0;
            }
        }
    } else {
        out.thr[..p.n_s].copy_from_slice(&t.thr[..p.n_s]);
        out.en[..p.n_s].copy_from_slice(&t.t7[..p.n_s]);
        for k in p.n_s..nsfb_long {
            out.thr[k] = 0.0;
            out.en[k] = 0.0;
        }
    }
}

/// E7 part 2 (raw L1629-1656 long, L1820-1855 short): `thr[edge_sfb + i] *= weight[i]` for the band at the lowpass cutoff, then, if
/// the channel's TNS is active (`tns[0] == 1`), `thr[k] *= tns_weights[k]` for every band (`tns+0x790 + 4k` long;
/// `tns+0x790 + 0x1cc*w + 4k` short window w).  Apply per window for short blocks (`thr` = the 15-float window slice).
pub fn weight_thresholds(thr: &mut [f32], edge: &PsyParam, n_bands: usize, tns_weights: Option<&[f32]>) {
    for i in 0..edge.edge_count.max(0) as usize {
        let k = edge.edge_sfb as usize + i;
        thr[k] = edge.edge_weight[i] * thr[k];
    }
    if let Some(w) = tns_weights {
        for k in 0..n_bands {
            thr[k] = w[k] * thr[k];
        }
    }
}

/// Inputs of one channel's analysis (EncodeFrame E5-E7) besides the persistent state.
#[derive(Clone, Copy, Debug)]
pub struct PamInput<'a> {
    /// window sequence of the block (0 only-long, 1 start, 2 eight-short, 3 stop).
    pub wseq: u32,
    /// quality bucket (0x20/0x40/0x60): <= 0x40 aliases the MDST to the MDCT (`E = 2 x^2`), 0x60 uses the `im_*` spectra.
    pub quality: u32,
    /// the (TNS-filtered) real long spectrum `PsyChannel+0x148` (1024 lines; its energies are formed for every block type).
    pub re_long: &'a [f32],
    /// the second (MDST) long spectrum `PsyChannel+0x150` for quality 0x60; `None` when it aliases `re_long`.
    pub im_long: Option<&'a [f32]>,
    /// the 8 short-window real spectra (`+0x158+0x10w`) assembled as 8 x 128 contiguous lines; only read when the short energies are
    /// needed ([`short_energies_needed`]); may be empty otherwise.
    pub re_short: &'a [f32],
    /// short-window MDST spectra for quality 0x60 (same layout), `None` when aliased.
    pub im_short: Option<&'a [f32]>,
    /// `PsyChannel+0x131` (needs short energies for a long block).
    pub c131: bool,
    /// `tns+0x790` weights, only when the channel's TNS is active (`tns[0] == 1`): long `[k]`, short window w at `[w*115 + k]`.
    pub tns_weights: Option<&'a [f32]>,
}

/// EncodeFrame E5 + E6 + E7 for one channel: power spectra (`vsq`*2 or `zvmags`), `_pamMonaural`, copy/zero-fill of the thresholds
/// and energies into `out` (`ThrChannel+0x7ec`/`+0x9cc`) and band-edge / TNS-gain weighting of the thresholds.  `flags` =
/// `PsyChannel+0x240` (read and possibly cleared by the model).  The short-window grouping decision (E8/E10), `_AdvanceCodChannelPam`
/// (E9) and `_AdvancePsyOut` (E11) are separate calls.
pub fn analyse_channel(p: &PamParam, t: &mut ThrChannel, chan: &PsyChannelParam, inp: &PamInput, flags: &mut u16, out: &mut PamCopy) {
    let mut e_long = std::mem::take(&mut t.e_long);
    let mut e_short = std::mem::take(&mut t.e_short);
    match inp.im_long {
        None => power_spectrum_alias(&inp.re_long[..1024], &mut e_long[..1024]),
        Some(im) => power_spectrum_zvmags(&inp.re_long[..1024], &im[..1024], &mut e_long[..1024], 0),
    }
    if short_energies_needed(inp.c131, *flags, inp.wseq) {
        for w in 0..8 {
            let (r, o) = (&inp.re_short[w * 128..w * 128 + 128], &mut e_short[w * 128..w * 128 + 128]);
            match inp.im_short {
                None => power_spectrum_alias(r, o),
                Some(im) => power_spectrum_zvmags(r, &im[w * 128..w * 128 + 128], o, 0),
            }
        }
    }
    crate::portable::aac::psy::pam::pam_monaural(p, t, &e_long, &e_short, flags, inp.wseq, inp.quality);
    t.e_long = e_long;
    t.e_short = e_short;
    copy_pam_outputs(p, t, inp.wseq, chan.long.sfb.nsfb, chan.short.sfb.nsfb, out);
    if inp.wseq == 2 {
        for w in 0..8 {
            let tw = inp.tns_weights.map(|x| &x[w * 115..]);
            weight_thresholds(&mut out.thr[15 * w..15 * w + 15], &chan.short, p.n_sh, tw);
        }
    } else {
        weight_thresholds(&mut out.thr[..], &chan.long, p.n_s, inp.tns_weights);
    }
}

// ------------------------------------------------------------------------------------------------------------------------------
// E9: _AdvanceCodChannelPam

/// `_getMdctEnergy_simd(spec, &off, width, &out)` (0x40580): NEON sum of squares of `width & ~3` lines starting at `*off`
/// (8-wide blocks `acc += x[i..i+4]^2; acc += x[i+4..i+8]^2` as separate fmul/fadd, then 4-wide blocks), final reduce
/// `(a0 + a3) + (a2 + a1)`; `*off` advances by `width & ~3` (all engine widths are multiples of 4).
pub fn get_mdct_energy_simd(spec: &[f32], off: &mut usize, width: u32) -> f32 {
    let w8 = (width & 0xfff8) as usize;
    let w4 = (width & 0xfffc) as usize;
    let mut v = [0f32; 4];
    let mut p = *off;
    let mut done = 0usize;
    if w8 != 0 {
        let mut cnt = 0usize;
        loop {
            for l in 0..4 {
                let x = spec[p + l];
                v[l] += x * x;
            }
            for l in 0..4 {
                let x = spec[p + 4 + l];
                v[l] += x * x;
            }
            p += 8;
            cnt += 8;
            if !(cnt < w8) {
                break;
            }
        }
        done = ((w8 - 1) & !7) + 8;
    }
    while done < w4 {
        for l in 0..4 {
            let x = spec[p + l];
            v[l] += x * x;
        }
        p += 4;
        done += 4;
    }
    *off = p;
    // ext v1,v0,v0,#12 = (v3, v0, v1, v2); v0 += v1 -> (v0+v3, v1+v0, v2+v1, v3+v2); dup v1 = lane 2; lane0 = (v0+v3) + (v2+v1)
    let t0 = v[0] + v[3];
    let t2 = v[2] + v[1];
    t0 + t2
}

/// `_specWinLowpass(spec, cutoff, n, taper, len)` (0x43dcc): `spec[cutoff-len..cutoff) *= taper; spec[cutoff..n) = 0`
/// ([`crate::portable::aac::fb::bandwidth::spec_win_lowpass`] with the first `len` taper entries).
pub fn spec_win_lowpass(spec: &mut [f32], cutoff: i32, n: i32, taper: &[f32], len: i32) {
    crate::portable::aac::fb::bandwidth::spec_win_lowpass(spec, cutoff as usize, n as usize, &taper[..len.max(0) as usize]);
}

/// `CodChannel` (`psy+0xd0+8ch`): per-sfb energy `+0x08` and threshold `+0x1e8`; 8 windows x 15 floats (stride 0x3c).
#[derive(Clone, Debug)]
pub struct CodChannel {
    pub energy: [f32; 120],
    pub thr: [f32; 120],
}

impl Default for CodChannel {
    fn default() -> Self {
        CodChannel { energy: [0.0; 120], thr: [0.0; 120] }
    }
}

/// `_AdvanceCodChannelPam(cod, spec, thr7ec, wseq, cfg, lowpass)` (0x40378).  `spec` = the channel's final spectrum (L/R slot:
/// psyOut#1 spectrum, 1024 lines, short windows contiguous 8 x 128; for the M/S call: the M or S spectrum with `lowpass = false`),
/// `thr7ec` = thresholds in the `+0x7ec` layout (window w at `15*w`; the M/S call passes the M/S threshold arrays).  `lowpass` =
/// the call's last argument (1 for the regular L/R call): applies `_specWinLowpass` IN PLACE first.
///
/// Per band: `energy = sum x^2` of the final spectrum (`_getMdctEnergy_simd`, NOT x2), `thr = max(2 * thr7ec, 1.0)`
/// (`vDSP_vsmul` then `vDSP_vthr`).
pub fn advance_cod_channel_pam(cod: &mut CodChannel, spec: &mut [f32], thr7ec: &[f32], wseq: u32, cfg: &PsyChannelParam, lowpass: bool) {
    if wseq < 2 || wseq == 3 {
        let pp = &cfg.long;
        if lowpass {
            spec_win_lowpass(spec, pp.cutoff, pp.sfb.n as i32, &pp.taper, pp.taper_len);
        }
        let mut off = 0usize;
        for s in 0..pp.sfb.nsfb {
            cod.energy[s] = get_mdct_energy_simd(spec, &mut off, pp.sfb.widths[s] as u32);
        }
        let n = pp.sfb.nsfb;
        for s in 0..n {
            cod.thr[s] = thr7ec[s] * 2.0;
        }
        vthr_inplace(&mut cod.thr[..n], 1.0);
    } else if wseq == 2 {
        let pp = &cfg.short;
        let n_short = pp.sfb.n as usize;
        for w in 0..8 {
            let sw = &mut spec[w * n_short..(w + 1) * n_short];
            if lowpass {
                spec_win_lowpass(sw, pp.cutoff, pp.sfb.n as i32, &pp.taper, pp.taper_len);
            }
            let mut off = 0usize;
            let n = pp.sfb.nsfb;
            for k in 0..n {
                cod.energy[15 * w + k] = get_mdct_energy_simd(sw, &mut off, pp.sfb.widths[k] as u32);
            }
            for k in 0..n {
                cod.thr[15 * w + k] = thr7ec[15 * w + k] * 2.0;
            }
            vthr_inplace(&mut cod.thr[15 * w..15 * w + n], 1.0);
        }
    }
}

// ------------------------------------------------------------------------------------------------------------------------------
// E11/E13: _AdvancePsyOut

/// Window grouping of a short frame: `PsyChannel+0x248 -> +0x44[8]` group lengths and `+0x64` count (decided by EncodeFrame's grouping
/// code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortGrouping {
    pub group_len: [i32; 8],
    pub count: i32,
}

/// `_AdvancePsyOut(psyOut, PCF, is_pair, C, cod, tns, wseq, is_short)` (0x4a1cc): fills the geometry and the threshold/energy arrays of
/// `psy_out` from the `CodChannel` and clears the per-band flags.
///
/// * `is_pair` -> `+0x5b8`; `is_short` -> `+0x9d0` (EncodeFrame passes `wseq == 2` for the M/S candidate, the local short flag otherwise);
/// * `field_5bc` = `PsyChannel+0x23c`; `grouping` is only read for `wseq == 2`;
/// * `p1ac` = `P+0x1ac` (number of top short sfbs combined by min instead of sum; 0 for 44.1/48 kHz);
/// * `layout` = the channel's `PsyChannelParam` (long sfb table for wseq 0,1,3; short for 2);
/// * `tns` is stored at `+0xbc0` (Apple keeps a pointer; the caller passes the channel's TNS data or `None`).
///
/// Group `g` of `len_g` windows, short bands `k < nSh - p1ac`: `thr = sum_w thr_w[k]` (sequential from 0.0; SUM, not min),
/// `energy = (sum_w en_w[k]) * (1.0f / len_g)`; top `p1ac` bands: `thr = min_w thr_w[k] * len_g`, `energy = (1/len_g) * sum_w en_w[k]`.
#[allow(clippy::too_many_arguments)]
pub fn advance_psy_out(
    psy_out: &mut PsyOut,
    p1ac: i32,
    is_pair: bool,
    field_5bc: i32,
    cod: &CodChannel,
    tns: Option<TnsChannel>,
    wseq: u32,
    is_short: bool,
    grouping: &ShortGrouping,
    layout: &PsyChannelParam,
) {
    psy_out.is_short = is_short;
    psy_out.is_pair = is_pair;
    // `*(ushort*)**(PCF+0x30)` = sfbinfo[0] of the LONG PsyParam (1024) for every block type
    psy_out.lines_per_window = layout.long.sfb.n as u32;
    psy_out.wseq = wseq;
    psy_out.field_5bc = field_5bc;
    if wseq < 2 || wseq == 3 {
        psy_out.ngroups = 1;
        psy_out.group_len[0] = 1;
        let sfb = &layout.long.sfb;
        let n = sfb.nsfb;
        psy_out.total_bands = n as u32;
        psy_out.nsfb_per_group = n as u32;
        let mut acc = 0i32;
        for k in 0..n {
            let w = sfb.widths[k] as i32;
            psy_out.width[k] = w;
            psy_out.start[k] = acc;
            acc += w;
        }
        psy_out.start[n] = acc;
        psy_out.thr[..n].copy_from_slice(&cod.thr[..n]);
        psy_out.energy[..n].copy_from_slice(&cod.energy[..n]);
    } else if wseq == 2 {
        psy_out.ngroups = grouping.count as u32;
        psy_out.group_len = grouping.group_len;
        let sfb = &layout.short.sfb;
        let nsb = sfb.nsfb;
        let ng = grouping.count.max(0) as usize;
        psy_out.nsfb_per_group = nsb as u32;
        psy_out.total_bands = (ng * nsb) as u32;
        let mut acc = 0i32;
        for g in 0..ng {
            for k in 0..nsb {
                let w = grouping.group_len[g] * sfb.widths[k] as i32;
                psy_out.width[g * nsb + k] = w;
                psy_out.start[g * nsb + k] = acc;
                acc += w;
            }
        }
        psy_out.start[ng * nsb] = acc;
        let n_low = (nsb as i32 - p1ac).max(0) as usize;
        let mut win = 0usize;
        for g in 0..ng {
            let len = grouping.group_len[g] as usize;
            let inv = 1.0f32 / (grouping.group_len[g] as f32);
            let base = g * nsb;
            for k in 0..nsb {
                psy_out.thr[base + k] = 0.0;
                psy_out.energy[base + k] = 0.0;
            }
            for k in 0..n_low {
                let (mut ta, mut ea) = (0f32, 0f32);
                for w in 0..len {
                    ta = cod.thr[(win + w) * 15 + k] + ta;
                    ea = cod.energy[(win + w) * 15 + k] + ea;
                }
                psy_out.thr[base + k] = ta;
                psy_out.energy[base + k] = ea;
            }
            for k in 0..n_low {
                psy_out.energy[base + k] *= inv;
            }
            for k in n_low..nsb {
                let mut tmin = cod.thr[win * 15 + k];
                let mut ea = cod.energy[win * 15 + k];
                for w in 1..len {
                    let x = cod.thr[(win + w) * 15 + k];
                    if x <= tmin {
                        tmin = x;
                    }
                    ea = cod.energy[(win + w) * 15 + k] + ea;
                }
                psy_out.thr[base + k] = tmin * (len as f32);
                psy_out.energy[base + k] = inv * ea;
            }
            win += len;
        }
    }
    let total = psy_out.total_bands as usize;
    for f in psy_out.flags[..total.min(MAX_BANDS)].iter_mut() {
        *f = 0;
    }
    psy_out.intensity = None;
    psy_out.tns = tns;
}

// ------------------------------------------------------------------------------------------------------------------------------
// E14

/// `_sortShortSpectrum(src, nsfb, widths, ngroups, group_len, total, dst)` (0x49f34): reorders an 8x128 short spectrum into
/// group-sorted order: for each group, for each sfb, the `group_len` windows' lines of that band back to back; zero-fills the rest.
pub fn sort_short_spectrum(src: &[f32], nsfb: usize, widths: &[u16], ngroups: usize, group_len: &[i32], total: usize, dst: &mut [f32]) {
    let stride = (total / 8) as usize; // (param_6 + 7 if negative ...) >> 3 = 128
    let mut first = [0i32; 8];
    let mut acc = 0i32;
    for g in 0..ngroups {
        first[g] = acc;
        acc += group_len[g];
    }
    let mut dpos = 0usize;
    for g in 0..ngroups {
        let mut band_start = first[g] as usize * stride;
        for k in 0..nsfb {
            let w = widths[k] as usize;
            let mut s = band_start;
            for _ in 0..group_len[g] {
                dst[dpos..dpos + w].copy_from_slice(&src[s..s + w]);
                dpos += w;
                s += stride;
            }
            band_start += w;
        }
    }
    for x in &mut dst[dpos.min(total)..total] {
        *x = 0.0;
    }
}

/// `_calculateMaxSfb(nsfb, ngroups, width[], start[], spectrum)` (0x4a130): 1 + the highest band index (in any group) that has a
/// non-zero line, 0 for an all-zero spectrum.  `width`/`start` are indexed `g * nsfb + sfb` as in `PsyOut`.
/// (`EncodeFrame` first lowers `nsfb` past bands that start above the bandwidth line only when the SRC ratio is > 1: not the engine.)
pub fn calculate_max_sfb(nsfb: usize, ngroups: usize, width: &[i32], start: &[i32], spectrum: &[f32]) -> u32 {
    if nsfb == 0 {
        return 0;
    }
    let mut count = nsfb;
    loop {
        let sfb = count - 1;
        for g in 0..ngroups {
            let idx = sfb + nsfb * g;
            let w = width[idx];
            if w > 0 {
                let s0 = start[idx] as usize;
                for k in 0..w as usize {
                    if spectrum[s0 + k] != 0.0 {
                        return count as u32;
                    }
                }
            }
        }
        if count <= 1 {
            return 0;
        }
        count -= 1;
    }
}

/// E14 for one channel (raw L2540-2568): for an EIGHT_SHORT block copy `po.spectrum` into `scratch` (>= 1024 floats) and
/// `_sortShortSpectrum` it back into `po.spectrum` (group-sorted order, the order the quantizer reads), then
/// `po.max_sfb = _calculateMaxSfb(...)` on the final spectrum.  Pair unification ([`unify_pair_max_sfb`]) is a separate step.
pub fn finalize_psy_out(po: &mut PsyOut, layout: &PsyChannelParam, scratch: &mut [f32]) {
    if po.wseq == 2 {
        let n = po.lines_per_window as usize;
        scratch[..n].copy_from_slice(&po.spectrum[..n]);
        for x in &mut scratch[n..1024] {
            *x = f32::from_bits(0xffff_ffff); // memset(0xff)
        }
        let sfb = &layout.short.sfb;
        sort_short_spectrum(scratch, sfb.nsfb, sfb.widths, po.ngroups as usize, &po.group_len, 1024, &mut po.spectrum);
    }
    po.max_sfb = psy_out_max_sfb(po);
}

/// `max_sfb` of a finished `PsyOut` (`+0x5d0`) from its current spectrum.
pub fn psy_out_max_sfb(po: &PsyOut) -> u32 {
    calculate_max_sfb(po.nsfb_per_group as usize, po.ngroups as usize, &po.width, &po.start, &po.spectrum)
}

/// E14 pair unification (raw L2526-2533, 2577-2586): both channels of a CPE get `is_pair = 1` and `max_sfb = max(a, b)`.
pub fn unify_pair_max_sfb(a: &mut PsyOut, b: &mut PsyOut) {
    a.is_pair = true;
    b.is_pair = true;
    let m = a.max_sfb.max(b.max_sfb);
    a.max_sfb = m;
    b.max_sfb = m;
}
