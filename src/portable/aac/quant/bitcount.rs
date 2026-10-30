//! Integer bit counting and section selection: Apple's `_AACBitCount` (00045044), `_AACBitCountFast` (000458c0), `_CalcMergeGain`
//! (00045dcc), the `_count*` family (0003f1cc..00040038) and `_TnsBitCount` (0003e0e4).
//!
//! Pure integer logic, ported 1:1 from the decompiles. The persistent result structure (`LC+0x6be8`, 0x2798 bytes in Apple) is [`BitRes`]; both counters work **in place** on it
//! like the binary, because `_calcBitrateNMR` reads record fields of calls made earlier (stale records).
//!
//! Huffman code *lengths* only (code words belong to the writer module).

use crate::portable::aac::bits::tables::{SF_LEN, SPEC_LEN};
use crate::portable::aac::types::TnsChannel;
use std::sync::LazyLock;

/// "codebook cannot represent the band" cost / saturation value of every count function.
pub const SENT: i32 = 0x1fff_ffff;
const GAIN_INIT: i32 = 100_000;
/// `INT_MIN` = "no intensity / PNS value" in the `ipos`/`pns` arrays.
pub const INT_MIN: i32 = i32::MIN;
/// Number of 17-int section records in Apple's result struct (0x78).
pub const BITRES_RECS: usize = 0x78;

/// `LC+0x6be8`: result struct of `_AACBitCount`/`_AACBitCountFast`.
/// Layout (ints): `hdr[0..5]` = {window_sequence, stride, max_sfb, ngroups, count}; record `i` at `+0x14+0x44*i` =
/// {cb, start band k, length in bands, side bits, best (spectral) bits, cost[0..12]}; totals at `+0x1ff4` = {side, spectral, scalefactor,
/// intensity, PNS bits}. (The merge-gain scratch at `+0x2008` and the two table pointers at `+0x2788/0x2790` are not modelled: scratch is
/// always written before it is read.)
#[derive(Clone, Debug)]
pub struct BitRes {
    pub hdr: [i32; 5],
    pub rec: [[i32; 17]; BITRES_RECS],
    pub tot: [i32; 5],
}

impl BitRes {
    pub fn new() -> Self {
        Self { hdr: [0; 5], rec: [[0; 17]; BITRES_RECS], tot: [0; 5] }
    }
    /// Number of sections after `_AACBitCount` (`hdr[4]`).
    pub fn count(&self) -> usize {
        self.hdr[4].max(0) as usize
    }
}

impl Default for BitRes {
    fn default() -> Self {
        Self::new()
    }
}

/// Band geometry arrays indexed by `k = g*stride+s` (Apple: `LC+0x56f4` start, `LC+0x58d4` width, `LC+0x5ab4` max position).
#[derive(Clone, Copy)]
pub struct Sfb<'a> {
    pub start: &'a [i32],
    pub width: &'a [i32],
    pub maxpos: &'a [i32],
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Huffman length tables: the code lengths of the ISO tables the writer uses (`crate::portable::aac::bits::tables`).

struct LenTabs {
    /// `len[cb]` for cb 1..=11.
    len: [Vec<u16>; 12],
}

static LEN: LazyLock<LenTabs> = LazyLock::new(|| {
    let mut len: [Vec<u16>; 12] = Default::default();
    for cb in 1..=11 {
        len[cb] = SPEC_LEN[cb - 1].iter().map(|&x| u16::from(x)).collect();
    }
    LenTabs { len }
});

/// Length in bits of the scalefactor-delta code for `delta + 60`.
#[inline]
fn scf_len(index: usize) -> i32 {
    i32::from(SF_LEN[index])
}

/// Cost of the escape sequence for `|v| >= 16` (prefix ones + 0 + (N+4)-bit mantissa).
#[inline]
fn esc_bits(v: i32) -> i32 {
    if v < 16 {
        return 0;
    }
    let mut n = 4;
    while v >= (2 << n) {
        n += 1;
    }
    2 * (n - 4) + 5
}

#[inline]
fn sgn_idx(cb: usize, v: &[i32]) -> usize {
    match cb {
        1 | 2 => {
            let mut i = 0i32;
            for &x in &v[..4] {
                i = i * 3 + (x + 1);
            }
            i as usize
        }
        3 | 4 => {
            let mut i = 0i32;
            for &x in &v[..4] {
                i = i * 3 + x.abs();
            }
            i as usize
        }
        5 | 6 => ((v[0] + 4) * 9 + (v[1] + 4)) as usize,
        7 | 8 => (v[0].abs() * 8 + v[1].abs()) as usize,
        9 | 10 => (v[0].abs() * 13 + v[1].abs()) as usize,
        _ => (v[0].abs().min(16) * 17 + v[1].abs().min(16)) as usize,
    }
}

#[inline]
fn len_at(cb: usize, idx: usize) -> i32 {
    let t = &LEN.len[cb];
    // Out-of-range indices are only reachable with inconsistent (maxpos, lines) input, i.e. undefined behaviour in Apple's code.
    t[idx.min(t.len() - 1)] as i32
}

/// `_count*` family: writes `cost[1..=11]` of a band (`cost[0]` is set by the caller). `a = |lines[start+maxpos]|` selects the function
/// exactly like `_countFuncTable[min(a,16)]`; the band's `vals` are summed with the tables valid for that function.
pub fn band_costs(vals: &[i32], a: i32, cost: &mut [i32; 12]) {
    let idx = a.min(16);
    let first_valid: usize = match idx {
        0 | 1 => 1,
        2 => 3,
        3 | 4 => 5,
        5..=7 => 7,
        8..=12 => 9,
        _ => 11,
    };
    let n = vals.len();
    let nnz = vals.iter().filter(|&&x| x != 0).count() as i32;
    for c in cost.iter_mut().skip(1) {
        *c = SENT;
    }
    for cb in first_valid..=11 {
        let tot: i32 = if cb <= 4 {
            let mut t = 0i32;
            let mut i = 0;
            while i < n {
                t += len_at(cb, sgn_idx(cb, &vals[i..i + 4]));
                i += 4;
            }
            if cb >= 3 {
                t += nnz;
            }
            t
        } else {
            let mut t = 0i32;
            let mut i = 0;
            while i < n {
                t += len_at(cb, sgn_idx(cb, &vals[i..i + 2]));
                i += 2;
            }
            if cb >= 7 {
                t += nnz;
            }
            if cb == 11 && idx == 16 {
                for &x in vals {
                    t += esc_bits(x.abs());
                }
            }
            t
        };
        cost[cb] = tot;
    }
}

/// `_count{1_2,3_4,5_6,7_8,9_10,11,Esc}_S` ("signed" Fast variants): cheaper codebook of the pair selected by `min(a,16)`;
/// ties select the lower-numbered codebook. Returns (codebook, bits).
fn best_signed(vals: &[i32], idx: i32) -> (i32, i32) {
    let n = vals.len();
    let lo: usize = match idx {
        i if i <= 1 => 1,
        2 => 3,
        3 | 4 => 5,
        5..=7 => 7,
        8..=12 => 9,
        _ => 11,
    };
    if lo == 11 {
        let mut tot = 0i32;
        let mut i = 0;
        while i < n {
            let (a, b) = (vals[i], vals[i + 1]);
            tot += len_at(11, (a.abs().min(16) * 17 + b.abs().min(16)) as usize) + (a != 0) as i32 + (b != 0) as i32;
            if idx == 16 {
                tot += esc_bits(a.abs()) + esc_bits(b.abs());
            }
            i += 2;
        }
        return (11, tot);
    }
    let (mut c_lo, mut c_hi) = (0i32, 0i32);
    if lo == 1 || lo == 3 {
        let mut i = 0;
        while i < n {
            let q = &vals[i..i + 4];
            let nz = if lo == 3 { q.iter().filter(|&&x| x != 0).count() as i32 } else { 0 };
            c_lo += len_at(lo, sgn_idx(lo, q)) + nz;
            c_hi += len_at(lo + 1, sgn_idx(lo + 1, q)) + nz;
            i += 4;
        }
    } else {
        let mut i = 0;
        while i < n {
            let q = &vals[i..i + 2];
            let nz = if lo >= 7 { q.iter().filter(|&&x| x != 0).count() as i32 } else { 0 };
            c_lo += len_at(lo, sgn_idx(lo, q)) + nz;
            c_hi += len_at(lo + 1, sgn_idx(lo + 1, q)) + nz;
            i += 2;
        }
    }
    if c_lo > c_hi { (lo as i32 + 1, c_hi) } else { (lo as i32, c_lo) }
}

/// Section side-information bits for a section of `n` bands: `4 + w*(n/esc + 1)`, (esc,w) = (31,5) long / (7,3) short.
/// Apple tabulates it in `LK+0x1b8` (n<53, long) / `LK+0x28c` (n<16, short).
#[inline]
pub fn sect_len_bits(bt: u32, n: i32) -> i32 {
    let (esc, w) = if bt == 2 { (7, 3) } else { (31, 5) };
    4 + w * (n / esc + 1)
}

/// `_CalcMergeGain(OUT, tbl, gains, cur, nxt)`: `gains[cur] = {gain, newcb, newside, newbest}` (last three only when gain > 0).
fn calc_merge_gain(rec: &[[i32; 17]; BITRES_RECS], bt: u32, gains: &mut [[i32; 4]], cur: usize, nxt: usize) {
    let c = &rec[cur];
    let n = &rec[nxt];
    let side_m = sect_len_bits(bt, n[2] + c[2]);
    let mut best = GAIN_INIT;
    let mut bcb = -1i32;
    for i in 0..12 {
        let s = n[5 + i].wrapping_add(c[5 + i]);
        if s < best {
            best = s;
            bcb = i as i32;
        }
    }
    let gain = c[3].wrapping_add(c[4]).wrapping_add(n[3]).wrapping_add(n[4]).wrapping_sub(side_m).wrapping_sub(best);
    let g = &mut gains[cur];
    g[0] = gain;
    if gain > 0 {
        g[1] = bcb;
        g[2] = side_m;
        g[3] = best;
    }
}

#[inline]
fn merge_costs(dst: &mut [i32; 17], src: &[i32; 17]) {
    for q in 0..12 {
        let s = src[5 + q].wrapping_add(dst[5 + q]);
        dst[5 + q] = if s > 0x1fff_fffe { SENT } else { s };
    }
}

/// `_AACBitCount` (00045044). `lines`: signed quantised spectrum in coding order; `scf[k]`/`flags[k]`/`ipos[k]`/`idir[k]`/`pns[k]`
/// indexed by `k = g*stride+s`; `total = ngroups*stride`; `max_sfb` = bands coded per group; `res` = persistent result struct.
/// Returns side + spectral + scalefactor + PNS + intensity bits.
#[allow(clippy::too_many_arguments)]
pub fn aac_bit_count(
    lines: &[i32],
    scf: &[i32],
    ipos: Option<&[i32]>,
    idir: Option<&[i32]>,
    flags: &[i32],
    pns: Option<&[i32]>,
    bt: u32,
    total: i32,
    max_sfb: i32,
    ngroups: i32,
    sfb: &Sfb,
    res: &mut BitRes,
) -> i32 {
    let stride = if ngroups != 0 { total / ngroups } else { 0 };
    res.hdr[0] = bt as i32;
    res.hdr[1] = stride;
    res.hdr[2] = max_sfb;
    res.hdr[3] = ngroups;
    let fl = |k: i32| -> i32 { flags[k as usize] };
    let mut prev = scf[0];
    // ---- A. one record per band
    for g in 0..ngroups {
        for s in 0..max_sfb {
            let k = g * stride + s;
            let r = &mut res.rec[k as usize];
            r[1] = k;
            r[2] = 1;
            if let Some(p) = pns.filter(|p| p[k as usize] != INT_MIN) {
                let _ = p;
                r[4] = 0;
                r[0] = 13;
                r[5..17].fill(SENT);
                continue;
            }
            if let Some(ip) = ipos.filter(|ip| ip[k as usize] != INT_MIN) {
                let _ = ip;
                r[4] = 0;
                r[0] = if idir.map_or(0, |d| d[k as usize]) == 1 { 14 } else { 15 };
                r[5..17].fill(SENT);
                continue;
            }
            r[4] = SENT;
            let st = sfb.start[k as usize] as usize;
            let wd = sfb.width[k as usize] as usize;
            let mp = sfb.maxpos[k as usize] as usize;
            let v = lines[st + mp];
            let mut a = v.wrapping_abs();
            if (fl(k) >> 5) & 1 == 0 {
                if v != 0 {
                    prev = scf[k as usize];
                } else {
                    // zero band: look ahead for the next band whose flag bit5 is clear; bridge with codebook >= 1 when |prev-scf| >= 61
                    let (mut sf2, mut base) = (s, g * stride);
                    let mut j;
                    loop {
                        let s2 = sf2 + 1;
                        let add;
                        if s2 < max_sfb {
                            add = 0;
                            sf2 = s2;
                        } else {
                            add = stride;
                            sf2 = 0;
                        }
                        base += add;
                        j = base + sf2;
                        if j >= total - 1 {
                            break;
                        }
                        if (fl(j) >> 5) & 1 == 0 {
                            break;
                        }
                    }
                    let mut bridge = false;
                    if j < total && (fl(j) >> 5) & 1 == 0 {
                        let d = prev.wrapping_sub(scf[j as usize]);
                        if d >= 61 || d <= -61 {
                            bridge = true;
                        }
                    }
                    if bridge {
                        a = 1;
                        prev = scf[k as usize];
                    } else {
                        a = 0;
                    }
                }
            }
            let mut costs = [0i32; 12];
            band_costs(&lines[st..st + wd], a, &mut costs);
            costs[0] = if a == 0 { 0 } else { SENT };
            res.rec[k as usize][5..17].copy_from_slice(&costs);
        }
    }
    // ---- B. per group
    let mut cnt = max_sfb * ngroups;
    res.hdr[4] = cnt;
    res.tot[0] = 0;
    res.tot[1] = 0;
    let mut gains = [[0i32; 4]; BITRES_RECS + 2];
    if max_sfb != 0 {
        for g in 0..ngroups {
            let base = (g * stride) as usize;
            let ms = max_sfb as usize;
            // B1 argmin over the 12 costs (strict <, lowest codebook on ties)
            for s in 0..ms {
                let r = &mut res.rec[base + s];
                if r[4] == SENT {
                    let mut bv = SENT;
                    let mut bi = r[0];
                    for i in 0..12 {
                        if r[5 + i] < bv {
                            bv = r[5 + i];
                            bi = i as i32;
                        }
                    }
                    r[4] = bv;
                    r[0] = bi;
                }
            }
            // B2 merge runs of equal codebooks
            let mut cur = base;
            for i in 1..ms {
                let nx = res.rec[base + i];
                if res.rec[cur][0] == nx[0] {
                    let c = &mut res.rec[cur];
                    c[2] += 1;
                    c[4] = c[4].wrapping_add(nx[4]);
                    cnt -= 1;
                    merge_costs(c, &nx);
                } else {
                    res.rec[cur][3] = sect_len_bits(bt, res.rec[cur][2]);
                    cur = base + i;
                }
            }
            res.rec[cur][3] = sect_len_bits(bt, res.rec[cur][2]);
            // B3 greedy gain merge
            if (res.rec[base][2] as usize) < ms {
                let mut pos = 0usize;
                while res.rec[base + pos][2] as usize + pos < ms {
                    let c = base + pos;
                    let nx = c + res.rec[c][2] as usize;
                    calc_merge_gain(&res.rec, bt, &mut gains, c, nx);
                    pos += res.rec[c][2] as usize;
                }
                while (res.rec[base][2] as usize) < ms {
                    let mut bestg = 0i32;
                    let mut bidx = 0usize;
                    let mut bprev: i64 = -1;
                    let mut pos = 0usize;
                    let mut prev_idx: i64 = -1;
                    let mut nxt_pos = res.rec[base][2] as usize;
                    loop {
                        let cidx = base + pos;
                        if gains[cidx][0] > bestg {
                            bestg = gains[cidx][0];
                            bidx = cidx;
                            bprev = prev_idx;
                        }
                        prev_idx = cidx as i64;
                        pos = nxt_pos;
                        nxt_pos += res.rec[base + pos][2] as usize;
                        if nxt_pos >= ms {
                            break;
                        }
                    }
                    if bestg == 0 {
                        break;
                    }
                    let nx = res.rec[bidx + res.rec[bidx][2] as usize];
                    let gn = gains[bidx];
                    {
                        let c = &mut res.rec[bidx];
                        c[2] += nx[2];
                        c[0] = gn[1];
                        c[3] = gn[2];
                        c[4] = gn[3];
                        cnt -= 1;
                        merge_costs(c, &nx);
                    }
                    if bprev != -1 {
                        calc_merge_gain(&res.rec, bt, &mut gains, bprev as usize, bidx);
                    }
                    let clen = res.rec[bidx][2] as usize;
                    if (clen + bidx) - base < ms {
                        calc_merge_gain(&res.rec, bt, &mut gains, bidx, clen + bidx);
                    }
                }
            }
        }
        res.hdr[4] = cnt;
    } else {
        // max_sfb == 0: Apple returns zero totals and writes nothing else
        res.hdr[4] = 0;
    }
    // ---- C. compaction of the section heads to records 0..cnt (in place)
    res.tot[0] = 0;
    res.tot[1] = 0;
    if max_sfb != 0 && res.hdr[4] > 0 {
        let mut off = 0i32;
        let mut gb = 0i32;
        for j in 0..res.hdr[4] as usize {
            let src = (gb + off) as usize;
            let r = res.rec[src];
            res.rec[j] = r;
            res.tot[0] = res.tot[0].wrapping_add(r[3]);
            res.tot[1] = res.tot[1].wrapping_add(r[4]);
            let noff = r[2] + off;
            if noff != max_sfb {
                off = noff;
            } else {
                off = 0;
                gb += stride;
            }
        }
    }
    // ---- D. scalefactor / intensity / PNS bits
    res.tot[2] = 0;
    res.tot[3] = 0;
    res.tot[4] = 0;
    let (mut first_pns, mut first_scf) = (true, true);
    let (mut prev_p, mut prev_i, mut prev_s) = (0i32, 0i32, 0i32);
    for si in 0..res.hdr[4].max(0) as usize {
        let r = res.rec[si];
        let cb = r[0];
        let (k0, len) = (r[1], r[2]);
        if cb == 14 || cb == 15 {
            if let Some(ip) = ipos {
                for k in k0..k0 + len {
                    let d = ip[k as usize].wrapping_sub(prev_i);
                    res.tot[3] += scf_len((d + 60) as usize);
                    prev_i = ip[k as usize];
                }
            }
        } else if cb == 0 {
        } else if cb == 13 {
            if let Some(pn) = pns {
                for k in k0..k0 + len {
                    if first_pns {
                        res.tot[4] += 9;
                        first_pns = false;
                    } else {
                        res.tot[4] += scf_len((pn[k as usize] - prev_p + 60) as usize);
                    }
                    prev_p = pn[k as usize];
                }
            }
        } else {
            for k in k0..k0 + len {
                if first_scf {
                    res.tot[2] += scf_len(60);
                    first_scf = false;
                } else {
                    res.tot[2] += scf_len((prev_s - scf[k as usize] + 60) as usize);
                }
                prev_s = scf[k as usize];
            }
        }
    }
    res.tot[0] + res.tot[1] + res.tot[2] + res.tot[4] + res.tot[3]
}

/// `_AACBitCountFast` (000458c0): per-band estimator, one record per band at index `g*max_sfb+s`, no merging. `lines` are the
/// quantiser's magnitudes (`LC+0`). Writes `hdr`, fields 0..5 of the first `ngroups*max_sfb` records and the totals of `res`
/// (scalefactor bits use `prev = scf[0]` initially and no forced zero first delta, unlike `_AACBitCount`).
#[allow(clippy::too_many_arguments)]
pub fn aac_bit_count_fast(
    lines: &[i32],
    scf: &[i32],
    ipos: Option<&[i32]>,
    flags: &[i32],
    bt: u32,
    total: i32,
    max_sfb: i32,
    ngroups: i32,
    sfb: &Sfb,
    res: &mut BitRes,
) {
    let stride = if ngroups != 0 { total / ngroups } else { 0 };
    res.hdr[0] = bt as i32;
    res.hdr[1] = stride;
    res.hdr[2] = max_sfb;
    res.hdr[3] = ngroups;
    let nrec = max_sfb * ngroups;
    res.tot = [0; 5];
    res.hdr[4] = nrec;
    let side1 = if bt == 2 { 7 } else { 9 };
    if max_sfb > 0 && ngroups > 0 {
        for g in 0..ngroups {
            for s in 0..max_sfb {
                let r = &mut res.rec[(g * max_sfb + s) as usize];
                r[1] = g * stride + s;
                r[2] = 1;
                r[3] = side1;
                r[4] = 0x7fff_ffff;
            }
        }
        let fl = |k: i32| -> i32 { flags[k as usize] };
        let mut prev = scf[0];
        for ri in 0..nrec as usize {
            let k = res.rec[ri][1];
            let mut mx = lines[(sfb.start[k as usize] + sfb.maxpos[k as usize]) as usize].wrapping_abs();
            let width = sfb.width[k as usize] as usize;
            let bit5 = (fl(k) >> 5) & 1 != 0;
            let mut do_count = false;
            if bit5 {
                if mx != 0 {
                    do_count = true;
                }
            } else if mx != 0 {
                do_count = true;
                prev = scf[k as usize];
            } else {
                // zero band: next record (compact order) whose band has flag bit5 clear; bridge when |prev - scf| >= 61
                let mut j = ri + 1;
                while (j as i32) < nrec - 1 && (fl(res.rec[j][1]) >> 5) & 1 != 0 {
                    j += 1;
                }
                if (j as i32) < nrec && (fl(res.rec[j][1]) >> 5) & 1 == 0 {
                    let kn = res.rec[j][1];
                    if (prev.wrapping_sub(scf[kn as usize])).wrapping_abs() >= 61 {
                        mx = 1;
                        do_count = true;
                        prev = scf[k as usize];
                    }
                }
            }
            let (cb, cost) = if do_count {
                let st = sfb.start[k as usize] as usize;
                best_signed(&lines[st..st + width], mx.min(16))
            } else {
                (0, 0)
            };
            res.rec[ri][0] = cb;
            res.rec[ri][4] = cost;
        }
        let mut side = 0i32;
        let mut spec = 0i32;
        for ri in 0..nrec as usize {
            side = side.wrapping_add(res.rec[ri][3]);
            spec = spec.wrapping_add(res.rec[ri][4]);
        }
        res.tot[0] = side;
        res.tot[1] = spec;
        // scalefactor / intensity bits
        let mut scf_bits = 0i32;
        let mut int_bits = 0i32;
        let mut prev_s = scf[0];
        let mut prev_i = 0i32;
        for ri in 0..nrec as usize {
            let (cb, k) = (res.rec[ri][0], res.rec[ri][1]);
            if cb == 14 || cb == 15 {
                if let Some(ip) = ipos {
                    let d = ip[k as usize] - prev_i;
                    int_bits += scf_len((d + 60) as usize);
                    prev_i = ip[k as usize];
                }
            } else if cb != 0 && cb != 13 {
                scf_bits += scf_len((prev_s - scf[k as usize] + 60) as usize);
                prev_s = scf[k as usize];
            }
        }
        res.tot[2] = scf_bits;
        res.tot[3] = int_bits;
        res.tot[4] = 0;
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// TNS side bits

/// `_TnsBitCount(tns)` evaluated on the *coded* representation of `types` (`TnsChannel` = what the bitstream carries): `nwin` = number of windows
/// (1 long / 8 short); returns the TNS data bits without the `tns_data_present` flag bit. The engine uses at most one filter per window. The
/// coefficient width written for a filter is `coef_res_bits + 3 - coef_compress` (Apple's writer re-derives `coef_compress` with the range rule of
/// the count: all signed indices within [-4, 3] resp. [-2, 1]), so the counted bits are the written bits. `coef_res` of a `TnsWindow` is the bitstream
/// bit (0 = 3-bit, 1 = 4-bit coefficients; values 3/4 are also accepted as widths).
pub fn tns_bit_count_coded(tns: &TnsChannel, nwin: usize) -> i32 {
    if !tns.active {
        return 0;
    }
    let nf_bits = if nwin == 1 { 2 } else { 1 };
    let lo = if nwin == 1 { 11 } else { 7 };
    let mut bits = 0;
    for w in tns.windows.iter().take(nwin) {
        if w.n_filt < 1 {
            bits += nf_bits;
        } else {
            bits += nf_bits + 1;
            let o = w.filter.order as i32;
            if o < 1 {
                bits += lo;
            } else {
                let res_bits = if w.coef_res >= 3 { w.coef_res as i32 } else { 3 + w.coef_res as i32 };
                let nb = res_bits - w.filter.coef_compress as i32;
                bits += lo + 2 + nb * o;
            }
        }
    }
    bits
}
