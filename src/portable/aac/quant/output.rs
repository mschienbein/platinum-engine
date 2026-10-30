//! Conversion of the kernel result of a channel to the writer-side contract `crate::portable::aac::types::ChannelStream` (1:1 with what the
//! parser of Apple access units reports for the same channel), plus a few helpers around it.

use crate::portable::aac::quant::channel::LoopChannel;
use crate::portable::aac::types::{ChannelStream, PsyOut, Section, TnsChannel};

/// Window count of the TNS data of a channel (`1` long / `8` EIGHT_SHORT), as `_TnsBitCount` iterates it.
pub fn tns_windows(wseq: u32) -> usize {
    if wseq == 2 { 8 } else { 1 }
}

/// `_TnsBitCount` of a channel's psy-out TNS data (0 without TNS).
pub fn tns_bits_of(psy: &PsyOut) -> i32 {
    match &psy.tns {
        Some(t) => crate::portable::aac::quant::bitcount::tns_bit_count_coded(t, tns_windows(psy.wseq)),
        None => 0,
    }
}

/// Convert a quantised channel (after [`crate::portable::aac::quant::kernel::LoopKernel::advance_loop_kernel`]) to the writer contract.
///
/// * `global_gain = gg + 100`
/// * `sections[g]` = the merged sections of the last `_AACBitCount` of window group `g` (sfb index ranges, codebook 0..=15)
/// * `sf[g][b]` = coded scalefactor `gg - sf + 100` (first coded band == `global_gain`); codebook 14/15 bands carry the intensity
///   position; length `nsfbw` (= `max_sfb`) per group
/// * `q` = the signed quantised spectrum of the last exact pass (`LK` iq buffer) with the lines of non-coded bands (codebook 0, 13, 14,
///   15) cleared, in the quantiser's coding order (window groups sorted, bands ascending, windows of a group interleaved per band)
/// * `tns` = the psy-out TNS data
pub fn to_channel_stream(ch: &LoopChannel, psy: &PsyOut) -> ChannelStream {
    let ng = ch.ng;
    let stride = ch.stride.max(1);
    let mut sections: Vec<Vec<Section>> = vec![Vec::new(); ng];
    let mut cb_of_band = [0u8; crate::portable::aac::quant::channel::NB];
    for i in 0..ch.res.count() {
        let r = &ch.res.rec[i];
        let k = r[1] as usize;
        let (g, s0) = (k / stride, k % stride);
        let len = r[2] as usize;
        if g < ng {
            sections[g].push(Section { cb: r[0] as u8, start: s0 as u16, end: (s0 + len) as u16 });
        }
        for j in k..k + len {
            cb_of_band[j] = r[0] as u8;
        }
    }
    let mut sf = vec![vec![0i32; ch.nsfbw]; ng];
    for g in 0..ng {
        for b in 0..ch.nsfbw {
            let k = g * stride + b;
            sf[g][b] = match cb_of_band[k] {
                14 | 15 => ch.ipos[k],
                _ => ch.gg - ch.sf[k] + 100,
            };
        }
    }
    let mut q = ch.iq.to_vec();
    for g in 0..ng {
        for b in 0..ch.nsfbw {
            let k = g * stride + b;
            if matches!(cb_of_band[k], 0 | 13 | 14 | 15) {
                let (a, w) = (ch.start[k] as usize, ch.width[k] as usize);
                for v in &mut q[a..a + w] {
                    *v = 0;
                }
            }
        }
    }
    // lines of bands above max_sfb are not coded either
    if ch.nsfbw < ch.stride {
        for g in 0..ng {
            for b in ch.nsfbw..ch.stride {
                let k = g * stride + b;
                let (a, w) = (ch.start[k] as usize, ch.width[k] as usize);
                if a + w <= q.len() {
                    for v in &mut q[a..a + w] {
                        *v = 0;
                    }
                }
            }
        }
    }
    let tns: Option<TnsChannel> = psy.tns.clone();
    ChannelStream { global_gain: (ch.gg + 100).clamp(0, 255) as u8, sections, sf, tns, q }
}
