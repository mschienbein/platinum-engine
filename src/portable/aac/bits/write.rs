use crate::portable::aac::bits::bitwriter::BitWriter;
use crate::portable::aac::bits::pce::write_pce_body;
use crate::portable::aac::bits::tables::{SF_CODE, SF_LEN, SPEC_CODE, SPEC_LEN, SWB_LONG, SWB_SHORT};
use crate::portable::aac::types::{AccessUnit, ChannelStream, Element, ElementKind, IcsInfo, TnsChannel};

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    BadSfIndex(u8),
    Inconsistent(&'static str),
    /// scalefactor/intensity delta outside +-60 or codebook 13 (PNS) / 12 (reserved)
    Unsupported(&'static str),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSfIndex(index) => write!(f, "no sampling-frequency index {index}"),
            Self::Inconsistent(what) => write!(f, "inconsistent access unit: {what}"),
            Self::Unsupported(what) => write!(f, "access unit cannot be coded: {what}"),
        }
    }
}
impl std::error::Error for WriteError {}

/// Window group lengths for an EIGHT_SHORT `scale_factor_grouping` (bit `6-i` set: window `i+1` joins the group of window `i`).
fn group_lengths(grouping: u8) -> Vec<u8> {
    let mut lens = vec![1u8];
    for i in 0..7 {
        if grouping & (1 << (6 - i)) != 0 {
            *lens.last_mut().unwrap() += 1;
        } else {
            lens.push(1);
        }
    }
    lens
}

/// Inverse of [`group_lengths`]: the 7 coded grouping bits for group lengths summing to 8.
pub fn grouping_bits(group_len: &[i32]) -> u8 {
    let mut acc: u32 = 0;
    for &l in group_len {
        acc <<= 1;
        for _ in 1..l {
            acc = (acc << 1) | 1;
        }
    }
    (acc & 0x7f) as u8
}

fn write_ics_info(w: &mut BitWriter, info: &IcsInfo) {
    w.write(0, 1);
    w.write(u32::from(info.window_sequence), 2);
    w.write(u32::from(info.window_shape), 1);
    if info.window_sequence == 2 {
        w.write(u32::from(info.max_sfb), 4);
        w.write(u32::from(info.scale_factor_grouping), 7);
    } else {
        w.write(u32::from(info.max_sfb), 6);
        w.write(0, 1); // predictor_data_present
    }
}

fn write_tns(w: &mut BitWriter, tns: &TnsChannel, short: bool) {
    let nw = if short { 8 } else { 1 };
    for win in &tns.windows[..nw] {
        w.write(u32::from(win.n_filt), if short { 1 } else { 2 });
        if win.n_filt > 0 {
            w.write(u32::from(win.coef_res), 1);
            let f = &win.filter; // the engine uses at most one filter per window
            w.write(u32::from(f.length), if short { 4 } else { 6 });
            w.write(u32::from(f.order), if short { 3 } else { 5 });
            if f.order > 0 {
                w.write(u32::from(f.direction), 1);
                w.write(u32::from(f.coef_compress), 1);
                let nb = u32::from(win.coef_res) + 3 - u32::from(f.coef_compress);
                for &c in &f.coefs[..usize::from(f.order)] {
                    w.write(u32::from(c), nb);
                }
            }
        }
    }
}

#[inline]
fn spec_idx(cb: usize, q: &[i32]) -> usize {
    match cb {
        1 | 2 => q.iter().fold(0usize, |i, &x| i * 3 + (x + 1) as usize),
        3 | 4 => q.iter().fold(0usize, |i, &x| i * 3 + x.unsigned_abs() as usize),
        5 | 6 => ((q[0] + 4) * 9 + (q[1] + 4)) as usize,
        7 | 8 => (q[0].unsigned_abs() * 8 + q[1].unsigned_abs()) as usize,
        9 | 10 => (q[0].unsigned_abs() * 13 + q[1].unsigned_abs()) as usize,
        _ => (q[0].unsigned_abs().min(16) * 17 + q[1].unsigned_abs().min(16)) as usize,
    }
}

/// One section of coding-order values with codebook `cb` (1..=11).
fn write_spectral(w: &mut BitWriter, cb: usize, vals: &[i32]) {
    let code = SPEC_CODE[cb - 1];
    let len = SPEC_LEN[cb - 1];
    match cb {
        1 | 2 => {
            for q in vals.chunks_exact(4) {
                let i = spec_idx(cb, q);
                w.write(u32::from(code[i]), u32::from(len[i]));
            }
        }
        3 | 4 => {
            for q in vals.chunks_exact(4) {
                let i = spec_idx(cb, q);
                w.write(u32::from(code[i]), u32::from(len[i]));
                for &v in q {
                    if v != 0 {
                        w.write(u32::from(v < 0), 1);
                    }
                }
            }
        }
        5 | 6 => {
            for q in vals.chunks_exact(2) {
                let i = spec_idx(cb, q);
                w.write(u32::from(code[i]), u32::from(len[i]));
            }
        }
        7..=10 => {
            for q in vals.chunks_exact(2) {
                let i = spec_idx(cb, q);
                w.write(u32::from(code[i]), u32::from(len[i]));
                for &v in q {
                    if v != 0 {
                        w.write(u32::from(v < 0), 1);
                    }
                }
            }
        }
        _ => {
            for q in vals.chunks_exact(2) {
                let i = spec_idx(11, q);
                w.write(u32::from(code[i]), u32::from(len[i]));
                for &v in q {
                    if v != 0 {
                        w.write(u32::from(v < 0), 1);
                    }
                }
                for &v in q {
                    let av = v.unsigned_abs();
                    if av >= 16 {
                        // escape: N ones (N = floor(log2 av) - 4), a zero, then the N+4 low bits of av
                        let n = (31 - av.leading_zeros()) - 4;
                        for _ in 0..n {
                            w.write(1, 1);
                        }
                        w.write(0, 1);
                        w.write(av - (1 << (n + 4)), n + 4);
                    }
                }
            }
        }
    }
}

fn write_ics(w: &mut BitWriter, ch: &ChannelStream, info: &IcsInfo, write_info: bool, sfi: u8) -> Result<(), WriteError> {
    w.write(u32::from(ch.global_gain), 8);
    if write_info {
        write_ics_info(w, info);
    }
    let short = info.window_sequence == 2;
    let group_len: Vec<u8> = if short { group_lengths(info.scale_factor_grouping) } else { vec![1] };
    let swb: &[u16] = if short { SWB_SHORT[usize::from(sfi)] } else { SWB_LONG[usize::from(sfi)] };
    let max_sfb = usize::from(info.max_sfb);
    if max_sfb + 1 > swb.len() || ch.sections.len() != group_len.len() || ch.sf.len() != group_len.len() {
        return Err(WriteError::Inconsistent("max_sfb / group count"));
    }
    let (esc, nb) = if short { (7u32, 3u32) } else { (31, 5) };
    // section data; build the per-band codebook map on the way
    let mut cbs = vec![vec![0u8; max_sfb]; group_len.len()];
    for (g, secs) in ch.sections.iter().enumerate() {
        let mut k = 0usize;
        for s in secs {
            if usize::from(s.start) != k || s.end <= s.start || usize::from(s.end) > max_sfb || s.cb == 12 || s.cb > 15 {
                return Err(WriteError::Inconsistent("section layout"));
            }
            w.write(u32::from(s.cb), 4);
            let mut n = u32::from(s.end - s.start);
            while n >= esc {
                w.write(esc, nb);
                n -= esc;
            }
            w.write(n, nb);
            for b in s.start..s.end {
                cbs[g][usize::from(b)] = s.cb;
            }
            k = usize::from(s.end);
        }
        if k != max_sfb {
            return Err(WriteError::Inconsistent("sections do not cover max_sfb"));
        }
    }
    // scale-factor data (DPCM; intensity positions run separately from 0)
    let mut sf = i32::from(ch.global_gain);
    let mut is_pos = 0i32;
    for (g, row) in cbs.iter().enumerate() {
        if ch.sf[g].len() < max_sfb {
            return Err(WriteError::Inconsistent("scalefactor row length"));
        }
        for (b, &cb) in row.iter().enumerate() {
            let v = ch.sf[g][b];
            let d = match cb {
                0 => continue,
                13 => return Err(WriteError::Unsupported("PNS")),
                14 | 15 => {
                    let d = v - is_pos;
                    is_pos = v;
                    d
                }
                _ => {
                    let d = v - sf;
                    sf = v;
                    d
                }
            };
            if !(-60..=60).contains(&d) {
                return Err(WriteError::Unsupported("scalefactor delta out of range"));
            }
            let i = (d + 60) as usize;
            w.write(SF_CODE[i], u32::from(SF_LEN[i]));
        }
    }
    w.write(0, 1); // pulse_data_present
    match &ch.tns {
        Some(t) => {
            let windows = if short { 8 } else { 1 };
            if t.windows[..windows].iter().any(|win| win.n_filt > 0 && usize::from(win.filter.order) > win.filter.coefs.len()) {
                return Err(WriteError::Inconsistent("TNS filter order"));
            }
            w.write(1, 1);
            write_tns(w, t, short);
        }
        None => w.write(0, 1),
    }
    w.write(0, 1); // gain_control_data_present
    if ch.q.len() < 1024 {
        return Err(WriteError::Inconsistent("spectrum shorter than 1024"));
    }
    let mut win0 = 0usize;
    for (g, &l) in group_len.iter().enumerate() {
        let l = usize::from(l);
        for s in &ch.sections[g] {
            if matches!(s.cb, 0 | 13 | 14 | 15) {
                continue;
            }
            let a = win0 * 128 + usize::from(swb[usize::from(s.start)]) * l;
            let b = win0 * 128 + usize::from(swb[usize::from(s.end)]) * l;
            write_spectral(w, usize::from(s.cb), &ch.q[a..b]);
        }
        win0 += l;
    }
    Ok(())
}

fn write_element(w: &mut BitWriter, el: &Element, sfi: u8) -> Result<(), WriteError> {
    w.write(
        match el.kind {
            ElementKind::Sce => 0,
            ElementKind::Cpe => 1,
            ElementKind::Lfe => 3,
        },
        3,
    );
    w.write(u32::from(el.tag), 4);
    match el.kind {
        ElementKind::Cpe => {
            if el.channels.len() != 2 {
                return Err(WriteError::Inconsistent("CPE needs two channels"));
            }
            w.write(u32::from(el.common_window), 1);
            if el.common_window {
                write_ics_info(w, &el.ics[0]);
                w.write(u32::from(el.ms_mask_present), 2);
                if el.ms_mask_present == 1 {
                    for row in &el.ms_used {
                        for &b in row {
                            w.write(u32::from(b), 1);
                        }
                    }
                }
                for ch in &el.channels {
                    write_ics(w, ch, &el.ics[0], false, sfi)?;
                }
            } else {
                for (c, ch) in el.channels.iter().enumerate() {
                    write_ics(w, ch, &el.ics[c], true, sfi)?;
                }
            }
        }
        _ => {
            if el.channels.len() != 1 {
                return Err(WriteError::Inconsistent("SCE/LFE needs one channel"));
            }
            write_ics(w, &el.channels[0], &el.ics[0], true, sfi)?;
        }
    }
    Ok(())
}

/// Serialise one access unit: optional in-band PCE, the elements in order, `ID_END`, zero padding to a byte boundary.
pub fn write_access_unit(au: &AccessUnit, sf_index: u8) -> Result<Vec<u8>, WriteError> {
    if usize::from(sf_index) >= SWB_LONG.len() {
        return Err(WriteError::BadSfIndex(sf_index));
    }
    let mut w = BitWriter::new();
    if let Some(p) = &au.pce {
        w.write(5, 3); // ID_PCE
        write_pce_body(&mut w, p);
    }
    for el in &au.elements {
        write_element(&mut w, el, sf_index)?;
    }
    w.write(7, 3); // ID_END
    Ok(w.finish())
}
