//! Individual-channel-stream syntax: `ICSInfo`, section data, scalefactor data (float gains), pulse data, TNS data, spectral data with
//! dequantisation and short-window de-interleave, pulse application (executable model: `ICSInfo.parse`, `parse_sections`,
//! `parse_scalefactors`, `parse_pulse`, `parse_tns`, `parse_spectrum`, `apply_pulse_tab`).  All float arithmetic is plain IEEE f32
//! (no contraction); the gain is built by integer arithmetic on float bits.

use crate::portable::math::powf;
use crate::portable::aac::decoder::bits::bitreader::BitReader;
use crate::portable::aac::decoder::bits::huffman::tables as huff;
use crate::portable::aac::decoder::bits::tables::{gain_f32, swb_long, swb_short, POW43, PULSEF};
use crate::portable::aac::decoder::bits::AuError;

pub(crate) const MAX_BANDS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IcsInfo {
    pub sfi: u8,
    pub window_sequence: u8,
    pub window_shape: u8,
    pub max_sfb: u8,
    /// Number of window groups (EIGHT_SHORT) / 1 (long); `0` when `max_sfb == 0` (Apple stores 0 in the window-info nibble).
    pub num_groups: u8,
    /// Windows per group.
    pub glen: [u8; 8],
}

impl IcsInfo {
    pub fn new(sfi: u8) -> Self {
        Self { sfi, window_sequence: 0, window_shape: 0, max_sfb: 0, num_groups: 0, glen: [1, 0, 0, 0, 0, 0, 0, 0] }
    }

    #[inline]
    pub fn is_short(&self) -> bool {
        self.window_sequence == 2
    }

    #[inline]
    pub fn offsets(&self) -> &'static [u16] {
        if self.is_short() { swb_short(self.sfi) } else { swb_long(self.sfi) }
    }

    /// `ICSInfo::Deserialize` 0x1f184.
    pub fn parse(&mut self, br: &mut BitReader) -> Result<(), AuError> {
        let v = br.read(4);
        // bit 3 (ics_reserved_bit) is read and ignored
        self.window_shape = (v & 1) as u8;
        self.window_sequence = ((v >> 1) & 3) as u8;
        if self.window_sequence == 2 {
            let max_sfb = br.read(4) as usize;
            let grouping = br.read(7);
            if max_sfb > swb_short(self.sfi).len() - 1 {
                return Err(AuError::MaxSfbExceeded);
            }
            self.max_sfb = max_sfb as u8;
            self.glen = [0; 8];
            let mut g = 0usize;
            self.glen[0] = 1;
            for i in 0..7 {
                if grouping & (1 << (6 - i)) != 0 {
                    self.glen[g] += 1;
                } else {
                    g += 1;
                    self.glen[g] = 1;
                }
            }
            self.num_groups = if max_sfb != 0 { (g + 1) as u8 } else { 0 };
        } else {
            let x = br.read(7);
            let max_sfb = (x >> 1) as usize;
            if max_sfb > swb_long(self.sfi).len() - 1 {
                return Err(AuError::MaxSfbExceeded);
            }
            if x & 1 != 0 {
                return Err(AuError::PredictionPresent);
            }
            self.max_sfb = max_sfb as u8;
            self.glen = [1, 0, 0, 0, 0, 0, 0, 0];
            self.num_groups = if max_sfb != 0 { 1 } else { 0 };
        }
        Ok(())
    }
}

/// Section data: per group a list of `(codebook, length)`.
#[derive(Clone)]
pub(crate) struct Sections {
    pub n: [u8; 8],
    pub sec: [[(u8, u8); MAX_BANDS]; 8],
    pub has_noise: bool,
    pub has_intensity: bool,
}

impl Sections {
    pub fn new() -> Self {
        Self { n: [0; 8], sec: [[(0, 0); MAX_BANDS]; 8], has_noise: false, has_intensity: false }
    }

    #[inline]
    pub fn group(&self, g: usize) -> &[(u8, u8)] {
        &self.sec[g][..self.n[g] as usize]
    }

    /// `SectionDataParser::Deserialize` 0x1f4f4.  `left`: "intensity forbidden" (SCE, LFE, left channel of a CPE).
    pub fn parse(&mut self, br: &mut BitReader, info: &IcsInfo, left: bool) -> Result<(), AuError> {
        self.n = [0; 8];
        self.has_noise = false;
        self.has_intensity = false;
        if info.max_sfb == 0 {
            return Ok(());
        }
        let bits = if info.is_short() { 3 } else { 5 };
        let esc = (1u32 << bits) - 1;
        let max = u32::from(info.max_sfb);
        for g in 0..info.num_groups as usize {
            let mut end = 0u32;
            let mut n = 0usize;
            while end < max {
                let cb = br.read(4);
                if cb == 13 {
                    self.has_noise = true;
                } else if cb > 13 {
                    if left {
                        return Err(AuError::IntensityInLeftChannel);
                    }
                    self.has_intensity = true;
                }
                let mut ln = br.read(bits);
                let mut tot = ln;
                while ln == esc {
                    ln = br.read(bits);
                    tot += ln;
                    if max < tot + end {
                        return Err(AuError::SectionLengthExceedsMaxSfb);
                    }
                }
                if tot == 0 {
                    return Err(AuError::SectionLengthZero);
                }
                self.sec[g][n] = (cb as u8, tot as u8);
                n += 1;
                end += tot;
            }
            if end > max {
                return Err(AuError::SectionLengthExceedsMaxSfb);
            }
            self.n[g] = n as u8;
        }
        Ok(())
    }
}

/// Float gains `gain[group][sfb]` (Apple: `float gains[band + 15*group]`).
pub(crate) type Gains = [[f32; MAX_BANDS]; 8];

/// `FloatScaleFactorDataParser::Deserialize` 0x2d00c0.
pub(crate) fn parse_scalefactors(br: &mut BitReader, global_gain: u32, info: &IcsInfo, sec: &Sections, gains: &mut Gains) {
    *gains = [[0.0; MAX_BANDS]; 8];
    if info.max_sfb == 0 {
        return;
    }
    let gg = global_gain as i32;
    // normal / noise energy / intensity position accumulators
    let mut acc = [gg, gg - 346, 0];
    let mut first_noise = true;
    let sf = &huff().sf;
    for g in 0..info.num_groups as usize {
        let mut sfb = 0usize;
        for &(cb, ln) in sec.group(g) {
            if cb == 0 {
                sfb += ln as usize;
                continue;
            }
            let kind = if cb < 13 { 0 } else if cb == 13 { 1 } else { 2 };
            let mut cur = acc[kind];
            for _ in 0..ln {
                let n = if kind == 1 && first_noise {
                    first_noise = false;
                    cur += br.read(9) as i32; // noise_pcm: no clamp
                    cur
                } else {
                    cur += sf.decode(br) as i32 - 60;
                    cur = cur.clamp(-256, 255);
                    match kind {
                        0 => cur - 100,
                        1 => cur,
                        _ => -cur,
                    }
                };
                gains[g][sfb] = gain_f32(n);
                sfb += 1;
            }
            acc[kind] = cur;
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Pulse {
    pub n: u8,
    pub start: u8,
    /// `(offset, amplitude)`.
    pub vals: [(u8, u8); 4],
}

impl Pulse {
    /// `pulse_data()` after `pulse_data_present == 1`; EIGHT_SHORT is rejected.
    pub fn parse(br: &mut BitReader, info: &IcsInfo) -> Result<Self, AuError> {
        if info.is_short() {
            return Err(AuError::PulseInShortWindow);
        }
        let v = br.read(17);
        let mut p = Pulse { n: (((v >> 15) & 3) + 1) as u8, start: ((v >> 9) & 63) as u8, vals: [(0, 0); 4] };
        p.vals[0] = (((v >> 4) & 31) as u8, (v & 15) as u8);
        for i in 1..p.n as usize {
            let w = br.read(9);
            p.vals[i] = ((w >> 4) as u8, (w & 15) as u8);
        }
        Ok(p)
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TnsFilter {
    pub dir: u8,
    pub length: u8,
    pub order: u8,
    /// `kSinCoefs` index: `(coef + 8) | (coef_res << 4)`.
    pub coefs: [u8; 12],
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Tns {
    pub present: bool,
    /// long: number of filters; short: bit `7 - w` set when window `w` has a filter.
    pub mask: u8,
    /// long: filter `i` at `[i]`; short: the filter of window `w` at `[w]`.
    pub filters: [TnsFilter; 8],
}

impl Tns {
    /// `TNSData::ParseTNSData` 0x206b8 (after `tns_data_present == 1`).  The order is clamped to 12 (long) / 7 (short) BEFORE the
    /// coefficients are read (bitstream desync, no error); a filter length of 0 is an error.
    pub fn parse(&mut self, br: &mut BitReader, info: &IcsInfo) -> Result<(), AuError> {
        self.present = true;
        self.mask = 0;
        let short = info.is_short();
        let max_order = if short { 7 } else { 12 };
        let nwin = if short { 8 } else { 1 };
        for w in 0..nwin {
            let nf = br.read(if short { 1 } else { 2 });
            self.mask = (self.mask << 1) | nf as u8;
            if nf != 0 {
                let res = br.read(1);
                for i in 0..nf as usize {
                    let idx = if short { w } else { i };
                    let length = br.read(if short { 4 } else { 6 });
                    let order = br.read(if short { 3 } else { 5 }).min(max_order);
                    if length == 0 {
                        return Err(AuError::TnsLengthZero);
                    }
                    let f = &mut self.filters[idx];
                    f.length = length as u8;
                    f.order = order as u8;
                    f.dir = 0;
                    if order != 0 {
                        let dc = br.read(2);
                        f.dir = ((dc >> 1) & 1) as u8;
                        let comp = dc & 1;
                        let nb = res + 3 - comp;
                        for j in 0..order as usize {
                            let mut v = br.read(nb) as i32;
                            if v >= 1 << (nb - 1) {
                                v -= 1 << nb;
                            }
                            f.coefs[j] = ((v + 8) | ((res as i32) << 4)) as u8;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Scratch buffers of the spectral stage (shared by all channels of a decoder).
pub(crate) struct Scratch {
    /// Signed quantised values of the finished spectrum (flat order), consumed by the pulse stage.
    pub q: [i32; 1024],
    /// Coded-order group buffers.
    pub tmp: [f32; 1024],
    pub tmpq: [i32; 1024],
}

impl Scratch {
    pub fn new() -> Box<Self> {
        Box::new(Self { q: [0; 1024], tmp: [0.0; 1024], tmpq: [0; 1024] })
    }
}

/// Codebook geometry `(dim, modulus, offset, unsigned)`.
#[inline]
fn geometry(cb: u8) -> (usize, usize, i32, bool) {
    match cb {
        1 | 2 => (4, 3, 1, false),
        3 | 4 => (4, 3, 0, true),
        5 | 6 => (2, 9, 4, false),
        7 | 8 => (2, 8, 0, true),
        9 | 10 => (2, 13, 0, true),
        _ => (2, 17, 0, true),
    }
}

/// `nvals` values of codebook `cb` (1..=11) into `out[pos..]`, dequantised with `gain` (`out = gain * (+-POW43[|q|])`, one rounding).
fn decode_codebook_section(br: &mut BitReader, cb: u8, nvals: usize, gain: f32, out: &mut [f32], qout: &mut [i32], mut pos: usize) {
    let (dim, modulus, offset, unsigned) = geometry(cb);
    let tree = &huff().spec[cb as usize - 1];
    for _ in 0..nvals / dim {
        let mut idx = tree.decode(br);
        let mut vals = [0i32; 4];
        for k in (0..dim).rev() {
            vals[k] = (idx % modulus) as i32;
            idx /= modulus;
        }
        if unsigned {
            for v in vals.iter_mut().take(dim) {
                if *v != 0 && br.read_bit() {
                    *v = -*v;
                }
            }
            if cb == 11 {
                for v in vals.iter_mut().take(2) {
                    if v.abs() == 16 {
                        // escape: leading ones counted up to a cap of 8; the terminator bit is consumed unconditionally
                        let mut lz = 0u32;
                        while lz < 8 && br.peek(lz + 1) == (1u32 << (lz + 1)) - 1 {
                            lz += 1;
                        }
                        br.skip(lz as usize + 1);
                        let e = (br.read(lz + 4) + (16u32 << lz)) as i32;
                        *v = if *v < 0 { -e } else { e };
                    }
                }
            }
        } else {
            for v in vals.iter_mut().take(dim) {
                *v -= offset;
            }
        }
        for k in 0..dim {
            let v = vals[k];
            let mut t = POW43[v.unsigned_abs() as usize];
            if v < 0 {
                t = -t;
            }
            out[pos + k] = gain * t;
            qout[pos + k] = v;
        }
        pos += dim;
    }
}

/// `SpectralData::Deserialize` 0x2d7aac: Huffman + escape + dequantisation + short-window de-interleave.  `spec` is window-major
/// (EIGHT_SHORT: 8 consecutive 128-line windows); lines above the coded bandwidth are `+0.0`.  `scratch.q` receives the signed
/// quantised values in the same order.
pub(crate) fn parse_spectrum(br: &mut BitReader, info: &IcsInfo, sec: &Sections, gains: &Gains, scratch: &mut Scratch, spec: &mut [f32; 1024]) -> Result<(), AuError> {
    *spec = [0.0; 1024];
    scratch.q = [0; 1024];
    if info.max_sfb == 0 {
        return Ok(());
    }
    let off = info.offsets();
    let wl = off[off.len() - 1] as usize;
    let nswb = off.len() - 1;
    let mut win_base = 0usize;
    for g in 0..info.num_groups as usize {
        let gl = info.glen[g] as usize;
        scratch.tmp[..gl * wl].fill(0.0);
        scratch.tmpq[..gl * wl].fill(0);
        let mut sfb = 0usize;
        for &(cb, ln) in sec.group(g) {
            let e = sfb + ln as usize;
            if matches!(cb, 0 | 13 | 14 | 15) {
                sfb = e;
                continue; // zero fill (noise / intensity / zero bands are filled later)
            }
            if cb == 12 {
                return Err(AuError::Codebook12);
            }
            for b in sfb..e {
                let w = (off[b + 1] - off[b]) as usize;
                decode_codebook_section(br, cb, gl * w, gains[g][b], &mut scratch.tmp, &mut scratch.tmpq, gl * off[b] as usize);
            }
            sfb = e;
        }
        for j in 0..gl {
            for b in 0..nswb {
                let w = (off[b + 1] - off[b]) as usize;
                let dst = (win_base + j) * wl + off[b] as usize;
                let src = gl * off[b] as usize + j * w;
                spec[dst..dst + w].copy_from_slice(&scratch.tmp[src..src + w]);
                scratch.q[dst..dst + w].copy_from_slice(&scratch.tmpq[src..src + w]);
            }
        }
        win_base += gl;
    }
    Ok(())
}

/// One pulse in Apple's own form (`PulseData::Apply` 0x2d0860): `q = x / g; n = round_half_away(powf(|q|, 0.75)); r = powf(n + amp,
/// 1.3333334f); r = (q >= 0 && n != 0) ? r : -r; x' = g * r`.  Returns `(x', tracked signed integer magnitude)`.  `crate::portable::math::powf`
/// is the bit-exact Apple libm `powf`.  Used for the (pathological) states outside the proved range of the table form and as the
/// reference in tests.
pub(crate) fn pulse_faithful(x: f32, g: f32, amp: u8) -> (f32, i32) {
    let q = x / g;
    let aq = if 0.0 <= q { q } else { -q };
    let fv = powf(aq, 0.75);
    // fcvtas: round to nearest, ties away from zero (fv >= 0); non-finite -> 0 (cannot occur for finite spectra)
    let n: i64 = if fv.is_finite() { ((f64::from(fv) + 0.5).floor() as i64).min(1 << 40) } else { 0 };
    let m = n + i64::from(amp);
    let fv2 = powf(m as f32, f32::from_bits(0x3faa_aaab));
    let pos = q >= 0.0 && n != 0;
    let r = if pos { fv2 } else { -fv2 };
    (g * r, if pos { m as i32 } else { -(m as i32) })
}

/// `PulseData::Apply` 0x2d0860 (long windows only).  For every start magnitude `|q| < 8192` the result is the finite-table form
/// `spec[k] = g * (+-)PULSEF[|q| + amp]` (proved equal to Apple's `powf` form for all gain mantissas); a pulse on a zero line gives a NEGATIVE value;
/// zero-gain bands are skipped; a pulse in a band `>= max_sfb` silently ends the list.
pub(crate) fn apply_pulse(spec: &mut [f32; 1024], scratch: &mut Scratch, info: &IcsInfo, gains: &Gains, p: &Pulse) -> Result<(), AuError> {
    let q = &mut scratch.q;
    let off = info.offsets();
    let nswb = off.len() - 1;
    if p.start as usize >= nswb {
        return Err(AuError::PulseStartSfb);
    }
    let mut k = off[p.start as usize] as usize;
    let mut band = p.start as usize;
    for &(o, a) in p.vals.iter().take(p.n as usize) {
        k += o as usize;
        let mut b = band;
        while b < nswb && k >= off[b + 1] as usize {
            b += 1;
        }
        band = b;
        if band >= info.max_sfb as usize {
            break;
        }
        let g = gains[0][band];
        if g != 0.0 {
            let qv = q[k];
            if qv.unsigned_abs() < 8192 {
                let m = qv.unsigned_abs() as usize + a as usize;
                let pos = qv > 0;
                let r = if pos { PULSEF[m] } else { -PULSEF[m] };
                spec[k] = g * r;
                q[k] = if pos { m as i32 } else { -(m as i32) };
            } else {
                let (x, t) = pulse_faithful(spec[k], g, a);
                spec[k] = x;
                q[k] = t;
            }
        }
    }
    Ok(())
}

/// Working state of one channel (`IndividualChannelStream` minus the PNS LCG, which lives in the element).
pub(crate) struct ChannelWork {
    pub info: IcsInfo,
    pub sec: Sections,
    pub gains: Gains,
    pub tns: Tns,
    /// The spectrum (Apple: `valarray<float>` at ICS+0x100), processed in place by every stage.
    pub spec: [f32; 1024],
}

impl ChannelWork {
    pub fn new(sfi: u8) -> Box<Self> {
        Box::new(Self { info: IcsInfo::new(sfi), sec: Sections::new(), gains: [[0.0; MAX_BANDS]; 8], tns: Tns::default(), spec: [0.0; 1024] })
    }
}

/// `IndividualChannelStream::Deserialize` 0x1eac8: `global_gain`, [`ics_info` unless `common`], section data, scalefactor data,
/// pulse data, TNS data, `gain_control_data_present` (must be 0), spectral data, pulse application.  `left` = "intensity forbidden"
/// (SCE, LFE, left channel of a CPE); for `common` the caller has stored the shared `ics_info` in `w.info`.
pub(crate) fn ics_deserialize(br: &mut BitReader, w: &mut ChannelWork, scratch: &mut Scratch, left: bool, common: bool) -> Result<(), AuError> {
    let global_gain = br.read(8);
    if !common {
        w.info.parse(br)?;
    }
    w.sec.parse(br, &w.info, left)?;
    parse_scalefactors(br, global_gain, &w.info, &w.sec, &mut w.gains);
    let pulse = if br.read_bit() { Some(Pulse::parse(br, &w.info)?) } else { None };
    w.tns.present = false;
    w.tns.mask = 0;
    if br.read_bit() {
        w.tns.parse(br, &w.info)?;
    }
    if br.read_bit() {
        return Err(AuError::GainControl);
    }
    parse_spectrum(br, &w.info, &w.sec, &w.gains, scratch, &mut w.spec)?;
    if let Some(p) = pulse {
        apply_pulse(&mut w.spec, scratch, &w.info, &w.gains, &p)?;
    }
    Ok(())
}
