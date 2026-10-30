//! S2/S3: the M/S block of `_aacEncodeFrame` (`AdvanceMsStereo`, ms_stereo.c; asm 0x3a164 .. 0x3b1b4) for one channel pair:
//! M/S threshold set (Precise path), `PE(M)`/`PE(S)`, the per-band PE decision with its energy-ratio gate above the cutoff, the global
//! all-on/all-off rule, `_setMSAllOff`, the flag word and the copy-back of the chosen M/S data into the L/R slots.
//! Arithmetic order checked against the decompile.
//!
//! No cross-frame state: [`MsTool::flags`] (the `MSTool+0x10` words) is rewritten for every (group, band < max_sfb) of every call.

use crate::portable::aac::ms::pe::{eval_pe, MsError};
use crate::portable::aac::ms::tables::*;
use crate::portable::aac::ms::thresholds::*;
use crate::portable::arm::arm_lt;
use crate::portable::aac::types::PsyOut;

/// Which adjust-table set the encoder selected (`(bitrate >> 7) > 0x176` where `bitrate` is the per-full-bandwidth-channel rate).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableSet {
    Hi,
    Lo,
}

/// `(bitrate_per_fbw_channel >> 7) > 0x176` (aacOpen asm 0x6e40-0x6e94): Hi tables from 48 000 bit/s per channel, Lo below.
pub fn table_set_for_bitrate(bitrate_per_fbw_channel: u32) -> TableSet {
    if (bitrate_per_fbw_channel >> 7) > 0x176 { TableSet::Hi } else { TableSet::Lo }
}

/// `thrmode` of the MS tool (aacOpen asm 0x6e00-0x6e2c, 0x6fc4-0x6fd8): `16000 <= rate <= 48000 && (bitrate >> 5) > 0x176 && cdqu > 0x20`.
/// 1 = "Precise" thresholds + cutoff gate + bias 1.0 (engine), 0 = legacy.
pub fn thrmode_for(rate: u32, bitrate_per_fbw_channel: u32, quality: u32) -> i32 {
    ((16000..=48000).contains(&rate) && (bitrate_per_fbw_channel >> 5) > 0x176 && quality > 0x20) as i32
}

fn tab(bits: &[u32; 25]) -> [f32; 25] {
    let mut t = [0f32; 25];
    for i in 0..25 {
        t[i] = f32::from_bits(bits[i]);
    }
    t
}

/// Configuration of the MS tool of one encoder instance (`MSTool` +0x00..+0x228 and the psyconf/psyparam values it points at).
#[derive(Clone, Debug)]
pub struct MsConfig {
    /// `enc+0xe0` (quality code derived from `cdqu`): 0x20, 0x40 (engine) or 0x60.
    pub quality: u32,
    /// `MSTool+0x1f0`: 1 = Precise path (engine).  0 = legacy: the M/S thresholds arrive in `m.thr`/`s.thr` from `EncodeFrame` (not ported).
    pub thrmode: i32,
    /// `MSTool+4`: LR-PE bias (1.0 when `thrmode != 0`, 1.2 otherwise).
    pub bias: f32,
    /// `MSTool+8`: debug mode 0 = PE decision (engine), 1 = all bands M/S, 2 = M/S on odd flat idx.
    pub mode: i32,
    /// `MSTool+0xc`: global all-on/all-off rule (calloc'ed to 1).
    pub global_rule: bool,
    /// `MSTool+0x1f8` = `psyparam+0xb0`: per-sfb floor of the M/S thresholds (49 long entries; engine = `PamParam::quiet_long_pow`).
    pub floor_long: Vec<f32>,
    /// `MSTool+0x200` = `psyparam+0xb8` (14 short entries; `PamParam::quiet_short_pow`).
    pub floor_short: Vec<f32>,
    pub tab_mid_long: [f32; 25],
    pub tab_mid_short: [f32; 25],
    pub tab_side_long: [f32; 25],
    pub tab_side_short: [f32; 25],
    /// `psyconf+0x3c8` (float, an integer value): number of long sfbs whose centre frequency is below 18 kHz (45 @44.1 kHz, 43 @48 kHz).
    pub cutoff_long: f32,
    /// `psyconf+0x3c4`: same for the short sfbs (12).
    pub cutoff_short: f32,
    /// `psyconf+0` / `psyconf+0x1e0`: sfb centre frequencies (Hz) handed to `_addLRThresholdOffsets` (identity); empty = 0.0.
    pub freq_long: Vec<f32>,
    pub freq_short: Vec<f32>,
}

impl MsConfig {
    /// aacOpen's MS tool setup for the engine path (rate 16-48 kHz, per-fbw-channel bitrate, quality 0x20/0x40/0x60).
    /// `floor_*` = `PamParam::quiet_{long,short}_pow`; `first_ge_18k_*` = `SfbCenterFreqs::first_ge_18k_{long,short}` (the cutoff bands);
    /// `freq_*` = `SfbCenterFreqs::{long,short}` (may be empty: they only feed identity calls).
    pub fn new(rate: u32, bitrate_per_fbw_channel: u32, quality: u32, floor_long: &[f32], floor_short: &[f32], first_ge_18k_long: u32, first_ge_18k_short: u32,
               freq_long: &[f32], freq_short: &[f32]) -> Self {
        let thrmode = thrmode_for(rate, bitrate_per_fbw_channel, quality);
        let set = table_set_for_bitrate(bitrate_per_fbw_channel);
        let (mid, side) = match set {
            TableSet::Hi => (tab(&MS_TAB_MID_HI_BITS), tab(&MS_TAB_SIDE_HI_BITS)),
            TableSet::Lo => (tab(&MS_TAB_MID_LO_BITS), tab(&MS_TAB_SIDE_LO_BITS)),
        };
        MsConfig {
            quality,
            thrmode,
            bias: if thrmode != 0 { 1.0 } else { f32::from_bits(0x3f99_999a) },
            mode: 0,
            global_rule: true,
            floor_long: floor_long.to_vec(),
            floor_short: floor_short.to_vec(),
            tab_mid_long: mid,
            tab_mid_short: mid,
            tab_side_long: side,
            tab_side_short: side,
            cutoff_long: first_ge_18k_long as f32,
            cutoff_short: first_ge_18k_short as f32,
            freq_long: freq_long.to_vec(),
            freq_short: freq_short.to_vec(),
        }
    }
}

/// Outcome of one `advance_ms_stereo` call (diagnostics; the state lives in the psyOuts and in [`MsTool::flags`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MsStereoResult {
    /// `ncut`: number of bands that used the Precise thresholds (`min(max_sfb, cutoff)`), 0 in legacy/placeholder modes.
    pub ncut: usize,
    /// total gain / loss of the decision loop (`sg`, `sl`).
    pub s_gain: f32,
    pub s_loss: f32,
    /// the global rule fired.
    pub all_on: bool,
    pub all_off: bool,
}

/// The MS tool of one CPE (`MSTool`, 0x228 bytes, `calloc` in aacOpen).
#[derive(Clone, Debug)]
pub struct MsTool {
    pub cfg: MsConfig,
    /// `MSTool+0x10`: per (group, band) word, bit 0 = the M/S decision as it was before the intensity stage.
    pub flags: [u32; 120],
}

#[inline]
fn ncut_of(max_sfb: usize, cutoff: f32) -> usize {
    // fcvtzs(fmin(float(max_sfb), cutoff))
    let m = max_sfb as f32;
    (if m < cutoff { m } else { cutoff }) as i32 as usize
}

impl MsTool {
    pub fn new(cfg: MsConfig) -> Self {
        MsTool { cfg, flags: [0; 120] }
    }

    /// The M/S block for one channel pair.  `l`/`r` = psyOut#1 of the CPE (L/R slots, `pe` already set by [`crate::portable::aac::ms::pe::calc_channel_pe`], `max_sfb`
    /// unified, `is_pair`), `m`/`s` = the M/S candidate psyOuts (psyOut#2, spectra = `get_ms_mdct`, energies from `advance_cod_channel_pam`,
    /// geometry from `advance_psy_out`/`finalize`), `pl`/`pr` = the per-line power arrays of the L/R channels (`psyOut+8` = `p1`, 1024 lines, for
    /// long/start/stop blocks; for EIGHT_SHORT the eight `p2[w]` windows of 128 lines concatenated window-major, UNSORTED).
    ///
    /// On return `m.thr/s.thr/m.pe/s.pe` hold the M/S thresholds and PEs of every (g, b < max_sfb), `l.flags[idx] bit 0` the decision, and for every
    /// flagged band the M/S data (thr, energy, pe, spectrum lines) has replaced the L/R slot data (`l` <- M, `r` <- S).
    pub fn advance_ms_stereo(&mut self, l: &mut PsyOut, r: &mut PsyOut, m: &mut PsyOut, s: &mut PsyOut, pl: &[f32], pr: &[f32]) -> Result<MsStereoResult, MsError> {
        let nsfb = l.max_sfb as usize;
        let ngrp = l.ngroups as usize;
        let stride = l.nsfb_per_group as usize;
        let short = l.wseq == 2;
        let cfg = &self.cfg;

        // --- thresholds
        let mut ncut = 0usize;
        if cfg.thrmode != 0 && cfg.thrmode < 3 {
            ncut = if short { thresholds_short(cfg, l, r, m, s, pl, pr) } else { thresholds_long(cfg, l, r, m, s, pl, pr) };
        }

        // --- PE(M), PE(S)
        for g in 0..ngrp {
            for b in 0..nsfb {
                let idx = g * stride + b;
                let st = l.start[idx] as usize;
                let n = l.width[idx] as u32;
                m.pe[idx] = eval_pe(cfg.quality, m.thr[idx], &m.spectrum[st..], n)?;
                s.pe[idx] = eval_pe(cfg.quality, s.thr[idx], &s.spectrum[st..], n)?;
            }
        }

        // --- decision
        let mut res = MsStereoResult { ncut, s_gain: 0.0, s_loss: 0.0, all_on: false, all_off: false };
        match cfg.mode {
            0 => {
                let cutoff: i32 = if cfg.thrmode == 0 { 0 } else { (if short { cfg.cutoff_short } else { cfg.cutoff_long }) as i32 };
                let mut sg = 0.0f32;
                let mut sl = 0.0f32;
                for g in 0..ngrp {
                    for b in 0..nsfb {
                        let idx = g * stride + b;
                        let t = cfg.bias.mul_add(r.pe[idx] + l.pe[idx], -m.pe[idx]);
                        let gain = t - s.pe[idx];
                        if arm_lt(gain, 0.0) {
                            sl -= gain;
                            continue;
                        }
                        let set = if (cfg.quality | 0x20) == 0x60 {
                            if cfg.thrmode != 0 && (b as i32) < cutoff {
                                true
                            } else {
                                let (el, er) = (l.energy[idx], r.energy[idx]);
                                let mx = if el < er { er } else { el };
                                (el - er).abs() < mx * f32::from_bits(0x3f66_6666)
                            }
                        } else {
                            true
                        };
                        if set {
                            l.flags[idx] |= 1;
                        }
                        sg += gain;
                    }
                }
                if cfg.global_rule {
                    let lim = ((nsfb * ngrp) as f32) * 0.75;
                    if lim <= sl {
                        if (sg.is_nan() || sg != 0.0) && sg < lim {
                            set_ms_all_off(ngrp, nsfb, stride, &mut l.flags);
                            res.all_off = true;
                        }
                    } else {
                        for g in 0..ngrp {
                            for b in 0..nsfb {
                                l.flags[g * stride + b] |= 1;
                            }
                        }
                        res.all_on = true;
                    }
                }
                res.s_gain = sg;
                res.s_loss = sl;
            }
            1 => {
                for g in 0..ngrp {
                    for b in 0..nsfb {
                        l.flags[g * stride + b] |= 1;
                    }
                }
            }
            2 => {
                for g in 0..ngrp {
                    for b in 0..nsfb {
                        let idx = g * stride + b;
                        if idx & 1 != 0 {
                            l.flags[idx] |= 1;
                        }
                    }
                }
            }
            _ => return Err(MsError::InvalidMsDebugMode),
        }

        // --- MSTool flag word, copy-back
        for g in 0..ngrp {
            for b in 0..nsfb {
                let idx = g * stride + b;
                self.flags[idx] = (self.flags[idx] & !1) | (l.flags[idx] as u32 & 1);
            }
        }
        for g in 0..ngrp {
            for b in 0..nsfb {
                let idx = g * stride + b;
                if self.flags[idx] & 1 == 0 {
                    l.flags[idx] &= !1;
                } else {
                    let st = l.start[idx] as usize;
                    let n = l.width[idx] as usize;
                    l.flags[idx] |= 1;
                    l.thr[idx] = m.thr[idx];
                    r.thr[idx] = s.thr[idx];
                    l.energy[idx] = m.energy[idx];
                    r.energy[idx] = s.energy[idx];
                    l.pe[idx] = m.pe[idx];
                    r.pe[idx] = s.pe[idx];
                    l.spectrum[st..st + n].copy_from_slice(&m.spectrum[st..st + n]);
                    r.spectrum[st..st + n].copy_from_slice(&s.spectrum[st..st + n]);
                }
            }
        }
        Ok(res)
    }
}

/// `_setMSAllOff(ngroups, max_sfb, stride, flags)` (0x44c44): clears bit 0 of the flag word of every (group, band < max_sfb).
pub fn set_ms_all_off(ngroups: usize, max_sfb: usize, stride: usize, flags: &mut [i32]) {
    for g in 0..ngroups {
        for b in 0..max_sfb {
            let f = &mut flags[g * stride + b];
            if *f & 1 != 0 {
                *f &= !1;
            }
        }
    }
}

#[inline]
fn freq_at(v: &[f32], i: usize) -> f32 {
    if i < v.len() { v[i] } else { 0.0 }
}

/// Long/start/stop thresholds (asm 0x3a89c..0x3abf8): returns `ncut`.
fn thresholds_long(cfg: &MsConfig, l: &PsyOut, r: &PsyOut, m: &mut PsyOut, s: &mut PsyOut, pl: &[f32], pr: &[f32]) -> usize {
    let nsfb = l.max_sfb as usize;
    let ncut = ncut_of(nsfb, cfg.cutoff_long);
    let mut corr = [0f32; 130];
    let mut ldiff = [0f32; 130];
    let mut b = 0;
    while b < ncut {
        let b2 = b + 2;
        let e = nsfb.min(b2);
        let c = corr_pair(pl, pr, l.start[b] as usize, l.start[e] as usize);
        let v = corr_map(c);
        corr[b] = v;
        corr[b + 1] = v;
        b = b2;
    }
    for i in 0..ncut {
        ldiff[i] = ldiff_val(l.thr[i], r.thr[i]);
    }
    for i in 0..ncut {
        let (mut tl, mut tr) = (l.thr[i], r.thr[i]);
        add_lr_threshold_offsets(freq_at(&cfg.freq_long, i), &mut tl, &mut tr);
        let (tm, ts) = get_ms_thresholds_precise(tl, tr, corr[i], ldiff[i], cfg.floor_long[i], 1.0, &cfg.tab_mid_long, &cfg.tab_side_long);
        m.thr[i] = tm;
        s.thr[i] = ts;
        // `_modifyLRThresholds` (0x44f68) follows here in the original: every multiplier in it is +-0.0, so its factor is exactly 1.0 and the L/R
        // thresholds written back are unchanged; it is not ported. (`_addLRThresholdOffsets` has the same factor 1.0; it is kept because its
        // result feeds the M/S thresholds above.)
    }
    for i in ncut..nsfb {
        let t = if r.thr[i] <= l.thr[i] { r.thr[i] } else { l.thr[i] };
        m.thr[i] = t;
        s.thr[i] = t;
    }
    ncut
}

/// EIGHT_SHORT thresholds (asm 0x3a1e0..0x3a89c): per group the correlation is the mean over the group's windows of the per-window
/// band-pair correlation (`ws[b] = start[b] / group_len[0]` window-local line offsets); returns `ncut`.
fn thresholds_short(cfg: &MsConfig, l: &PsyOut, r: &PsyOut, m: &mut PsyOut, s: &mut PsyOut, pl: &[f32], pr: &[f32]) -> usize {
    let nsfb = l.max_sfb as usize;
    let ngrp = l.ngroups as usize;
    let stride = l.nsfb_per_group as usize;
    let ncut = ncut_of(nsfb, cfg.cutoff_short);
    let g0 = l.group_len[0].max(1);
    let mut ws = [0usize; 20];
    for b in 0..=nsfb.min(19) {
        ws[b] = (l.start[b] / g0) as usize;
    }
    let mut corr = [[0f32; 16]; 8];
    let mut ldiff = [[0f32; 16]; 8];
    let mut w0 = 0usize;
    for g in 0..ngrp {
        let mut acc = [0f32; 16];
        for k in 0..l.group_len[g] as usize {
            let w = w0 + k;
            let (wl, wr) = (&pl[w * 128..], &pr[w * 128..]);
            let mut b = 0;
            while b < ncut {
                let b2 = b + 2;
                let e = nsfb.min(b2);
                let c = corr_pair(wl, wr, ws[b], ws[e]);
                acc[b] += c;
                b = b2;
            }
        }
        let inv = 1.0 / (l.group_len[g] as f32);
        let mut b = 0;
        while b < ncut {
            let v = clamp_corr(inv * acc[b]);
            let mm = corr_map(v);
            corr[g][b] = mm;
            corr[g][b + 1] = mm;
            b += 2;
        }
        w0 += l.group_len[g] as usize;
        for i in 0..ncut {
            ldiff[g][i] = ldiff_val(l.thr[g * stride + i], r.thr[g * stride + i]);
        }
    }
    for g in 0..ngrp {
        for i in 0..ncut {
            let idx = g * stride + i;
            let (mut tl, mut tr) = (l.thr[idx], r.thr[idx]);
            add_lr_threshold_offsets(freq_at(&cfg.freq_short, i), &mut tl, &mut tr);
            let (tm, ts) = get_ms_thresholds_precise(tl, tr, corr[g][i], ldiff[g][i], cfg.floor_short[i], 1.0, &cfg.tab_mid_short, &cfg.tab_side_short);
            m.thr[idx] = tm;
            s.thr[idx] = ts;
            // `_modifyLRThresholds` (0x44f68) follows here in the original: every multiplier in it is +-0.0, so its factor is exactly 1.0 and the L/R
            // thresholds written back are unchanged; it is not ported.
        }
        for i in ncut..nsfb {
            let idx = g * stride + i;
            let t = if r.thr[idx] <= l.thr[idx] { r.thr[idx] } else { l.thr[idx] };
            m.thr[idx] = t;
            s.thr[idx] = t;
        }
    }
    ncut
}
