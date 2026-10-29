//! Random-but-valid Layer III bitstream synthesizer for decoder validation.
//!
//! Real encoders never exercise intensity stereo, mixed blocks, every Huffman
//! table, escape lengths, scalefac_scale/preflag or the LSF scale-factor
//! partitions. This tool writes streams that do, so the portable decoder can
//! be compared with other decoders (and AudioToolbox) on those paths. It uses
//! the decoder's own constant tables and writes the syntax the decoder reads;
//! agreement with independent decoders is what validates both.
//!
//! Build (scratch, not part of the engine build):
//!   rustc --edition 2024 -O tools/portable/mp3dec/synth.rs -o /tmp/mp3synth
//! Run:
//!   mp3synth OUT.mp3 SEED VERSION RATE_INDEX MODE MODE_EXT FRAMES SWITCHED_PCT MIXED_PCT DENSITY
//!     VERSION 1|2|25, RATE_INDEX 0..2, MODE 0 stereo 1 joint 2 dual 3 mono,
//!     MODE_EXT 0..3 or 9 for random per frame, DENSITY 1..100.
//! Environment knobs (experiments only): PN_SYNTH_TRACE=1 prints every granule's parameters;
//!   PN_SYNTH_NOSBG / PN_SYNTH_NOSCALE disable subblock_gain / scalefac_scale; PN_SYNTH_SBG=a,b,c fixes
//!   subblock_gain; PN_SYNTH_WIDE_SBG allows 0..7; PN_SYNTH_MAXMAG=N caps quantized magnitudes;
//!   PN_SYNTH_LINES=lo|hi limits short/mixed granules to lines below/above 36; PN_SYNTH_P23=+-N skews
//!   the last granule's claimed part2_3_length; PN_SYNTH_FREE drops the block-type state machine (invalid
//!   window sequences); PN_SYNTH_LOUD makes some frames target overs (peaks above full scale).
#![allow(dead_code, clippy::too_many_arguments)]

#[path = "../../../src/portable/audio/mp3_tables.rs"]
mod mp3_tables;
#[path = "../../../src/portable/audio/mp3_math.rs"]
mod mp3_math;
#[path = "../../../src/portable/audio/mp3_l3.rs"]
mod l3;

use mp3_tables::{BIG_TABLES, COUNT1_A_CODES, COUNT1_A_LENS, COUNT1_B_CODES, COUNT1_B_LENS, LINBITS};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12; self.0 ^= self.0 << 25; self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u32) -> u32 { (self.next() >> 33) as u32 % n.max(1) }
    fn chance(&mut self, pct: u32) -> bool { self.below(100) < pct }
}

#[derive(Default, Clone)]
struct BitW { bytes: Vec<u8>, bits: usize }
impl BitW {
    fn put(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            if self.bits % 8 == 0 { self.bytes.push(0); }
            if (value >> i) & 1 != 0 { let last = self.bytes.len() - 1; self.bytes[last] |= 0x80 >> (self.bits % 8); }
            self.bits += 1;
        }
    }
    fn append(&mut self, other: &BitW) {
        for i in 0..other.bits { self.put(u32::from(other.bytes[i / 8] >> (7 - i % 8)) & 1, 1); }
    }
}

fn capacity(table: usize) -> i32 {
    let t = BIG_TABLES[table].as_ref().expect("valid table");
    if LINBITS[table] > 0 { 15 + (1 << LINBITS[table]) - 1 } else { i32::from(t.dim) - 1 }
}
const VALID: [usize; 30] = [1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 0];

fn put_pair(w: &mut BitW, table: usize, x: i32, y: i32) {
    let t = BIG_TABLES[table].as_ref().expect("table");
    let dim = i32::from(t.dim);
    let lin = u32::from(LINBITS[table]);
    let (ax, ay) = (x.abs(), y.abs());
    let (sx, sy) = (if lin > 0 && ax >= 15 { 15 } else { ax }, if lin > 0 && ay >= 15 { 15 } else { ay });
    let idx = (sx * dim + sy) as usize;
    w.put(u32::from(t.codes[idx]), u32::from(t.lens[idx]));
    if lin > 0 && ax >= 15 { w.put((ax - 15) as u32, lin); }
    if ax != 0 { w.put(u32::from(x < 0), 1); }
    if lin > 0 && ay >= 15 { w.put((ay - 15) as u32, lin); }
    if ay != 0 { w.put(u32::from(y < 0), 1); }
}

#[derive(Clone)]
struct Gr {
    switched: bool, block_type: u8, mixed: bool,
    table_select: [u8; 3], subblock_gain: [u8; 3], r0c: u32, r1c: u32,
    preflag: bool, scale: bool, count1_table: bool,
    scalefac_compress: u32, global_gain: u32, big_values: u32, part23: u32,
    data: BitW,
    sf: Vec<u8>,
    ints: Vec<i32>,
}

struct Cfg { v1: bool, band_set: usize, switched_pct: u32, mixed_pct: u32, density: u32, is_stereo: bool, ms: bool }

fn random_ints(rng: &mut Rng, lines: usize, density: u32) -> Vec<i32> {
    let mut v = vec![0i32; 576];
    for (i, slot) in v.iter_mut().enumerate().take(lines) {
        let falloff = 100 - (i as u32 * 70 / 576);
        if !rng.chance(density * falloff / 100) { continue; }
        let mag = match rng.below(100) {
            0..=54 => 1, 55..=79 => 2, 80..=89 => 3 + rng.below(3) as i32, 90..=96 => 6 + rng.below(10) as i32,
            _ => { let wide = rng.chance(30); 16 + rng.below(if wide { 3000 } else { 120 }) as i32 }
        };
        let cap: i32 = std::env::var("PN_SYNTH_MAXMAG").ok().and_then(|v| v.parse().ok()).unwrap_or(i32::MAX);
        let mag = mag.min(cap);
        *slot = if rng.chance(50) { -mag } else { mag };
    }
    v
}

fn next_type(rng: &mut Rng, state: u8, switched_pct: u32) -> u8 {
    if std::env::var_os("PN_SYNTH_FREE").is_some() { return if rng.chance(switched_pct) { 1 + rng.below(3) as u8 } else { 0 }; }
    match state {
        0 | 3 => if rng.chance(switched_pct) { 1 } else { 0 },
        1 => 2,
        _ => if rng.chance(50) { 3 } else { 2 },
    }
}

fn make_gr(rng: &mut Rng, cfg: &Cfg, right_intensity: bool, prev: Option<&Gr>, scfsi: &mut [bool; 4], lines_cap: usize, state: u8, force: Option<(u8, bool)>) -> Gr {
    let chosen = match force { Some((t, _)) => t, None => next_type(rng, state, cfg.switched_pct) };
    let switched = chosen != 0;
    let block_type = chosen;
    let mixed = match force { Some((_, m)) => m, None => switched && block_type == 2 && rng.chance(cfg.mixed_pct) };
    let short = switched && block_type == 2;
    let band = cfg.band_set;
    let long_edges = &l3::SFB_LONG[band];
    let lines = (36 + rng.below(540) as usize).min(lines_cap);
    let mut ints = random_ints(rng, lines, cfg.density);
    if mixed || switched {
        match std::env::var("PN_SYNTH_LINES").as_deref() {
            Ok("lo") => for v in ints.iter_mut().skip(36) { *v = 0; },
            Ok("hi") => for v in ints.iter_mut().take(36) { *v = 0; },
            _ => {}
        }
    }
    // Spectral partition into pairs and quads.
    let last_big = ints.iter().rposition(|v| v.abs() >= 2);
    let last_nz = ints.iter().rposition(|v| *v != 0);
    let mut big_end = last_big.map_or(0, |i| (i + 2) & !1);
    if let Some(nz) = last_nz {
        let c1_end = big_end + 4 * (nz + 1).saturating_sub(big_end).div_ceil(4);
        if c1_end > 576 { big_end = (nz + 2) & !1; }
    }
    let c1_end = last_nz.map_or(big_end, |nz| (big_end + 4 * (nz + 1).saturating_sub(big_end).div_ceil(4)).max(big_end));
    for v in ints.iter_mut().skip(c1_end) { *v = 0; }
    // Region split (mirrors the decoder).
    let band_edges = |i: usize| usize::from(long_edges[i.min(22)]);
    let (r0c, r1c) = (rng.below(16), rng.below(8));
    let (r1s, r2s) = if !switched {
        (band_edges(r0c as usize + 1), band_edges(r0c as usize + r1c as usize + 2))
    } else {
        let v1 = cfg.v1;
        let r = match (block_type == 2, v1, band == 8) { (true, _, false) | (_, true, _) => 36, (true, _, true) => 72, (false, false, false) => 54, (false, false, true) => 108 };
        (r, 576)
    };
    let mut table_select = [0u8; 3];
    let mut max = [0i32; 3];
    for line in (0..big_end).step_by(2) {
        let region = if line < r1s { 0 } else if line < r2s { 1 } else { 2 };
        max[region] = max[region].max(ints[line].abs()).max(ints[line + 1].abs());
    }
    for region in 0..3 {
        let candidates: Vec<usize> = VALID.iter().copied().filter(|&t| t == 0 && max[region] == 0 || t != 0 && capacity(t) >= max[region]).collect();
        table_select[region] = candidates[rng.below(candidates.len() as u32) as usize] as u8;
    }
    let count1_table = rng.chance(50);
    // Scale factors.
    let mut subblock_gain = [0u8; 3];
    if short && std::env::var_os("PN_SYNTH_NOSBG").is_none() { for g in subblock_gain.iter_mut() { *g = if rng.chance(60) { 0 } else { rng.below(if std::env::var_os("PN_SYNTH_WIDE_SBG").is_some() { 8 } else { 3 }) as u8 }; } }
    if short {
        if let Ok(v) = std::env::var("PN_SYNTH_SBG") {
            let parts: Vec<u8> = v.split(',').filter_map(|t| t.parse().ok()).collect();
            if parts.len() == 3 { subblock_gain = [parts[0], parts[1], parts[2]]; }
        }
    }
    let scale = rng.chance(40) && std::env::var_os("PN_SYNTH_NOSCALE").is_none();
    let preflag = cfg.v1 && !switched && rng.chance(40);
    let (scalefac_compress, sf_slots): (u32, Vec<(u32, bool)>);
    // sf_slots: (slen, copied) in transmission order.
    let mut sf: Vec<u8> = Vec::new();
    if cfg.v1 {
        let sc = if right_intensity { rng.below(14) } else { rng.below(16) };
        scalefac_compress = sc;
        let (s1, s2) = (u32::from(l3::SLEN1[sc as usize]), u32::from(l3::SLEN2[sc as usize]));
        if short {
            let n1 = if mixed { 8 + 9 } else { 18 };
            let n2 = if mixed { 18 } else { 18 };
            sf_slots = (0..n1).map(|_| (s1, false)).chain((0..n2).map(|_| (s2, false))).collect();
        } else {
            let groups = [(0, 6, s1), (6, 11, s1), (11, 16, s2), (16, 21, s2)];
            let mut slots = Vec::new();
            let can_copy = prev.is_some_and(|p| !(p.switched && p.block_type == 2));
            for (gi, &(a, b, len)) in groups.iter().enumerate() {
                let copy = can_copy && scfsi[gi];
                for _ in a..b { slots.push((len, copy)); }
            }
            sf_slots = slots;
        }
    } else {
        // LSF: derive slen/partitions exactly like the decoder.
        let mut sc = rng.below(512) as usize;
        if right_intensity { sc = rng.below(512) as usize; }
        let (slen, table, pre): ([u32; 4], usize, bool);
        let s = sc;
        if right_intensity {
            let h = s >> 1;
            if h < 180 { slen = [(h / 36) as u32, ((h % 36) / 6) as u32, ((h % 36) % 6) as u32, 0]; table = 3; }
            else if h < 244 { let t = h - 180; slen = [((t & 63) >> 4) as u32, ((t & 15) >> 2) as u32, (t & 3) as u32, 0]; table = 4; }
            else { let t = h - 244; slen = [(t / 3) as u32, (t % 3) as u32, 0, 0]; table = 5; }
            pre = false;
        } else if s < 400 { slen = [((s >> 4) / 5) as u32, ((s >> 4) % 5) as u32, ((s & 15) >> 2) as u32, (s & 3) as u32]; table = 0; pre = false; }
        else if s < 500 { let t = s - 400; slen = [((t >> 2) / 5) as u32, ((t >> 2) % 5) as u32, (t & 3) as u32, 0]; table = 1; pre = false; }
        else { let t = s - 500; slen = [(t / 3) as u32, (t % 3) as u32, 0, 0]; table = 2; pre = true; }
        let _ = pre;
        let kind = if short { if mixed { 2 } else { 1 } } else { 0 };
        let counts = l3::NR_OF_SFB[table][kind];
        let mut slots = Vec::new();
        for part in 0..4 { for _ in 0..counts[part] { slots.push((slen[part], false)); } }
        scalefac_compress = sc as u32;
        sf_slots = slots;
    }
    for (i, &(slen, copied)) in sf_slots.iter().enumerate() {
        let value = if copied { prev.map_or(0, |p| p.sf.get(i).copied().unwrap_or(0)) } else if slen == 0 { 0 } else { rng.below(1 << slen) as u8 };
        sf.push(value);
    }
    // Encode scale factors (MPEG-1 long granule 1 skips copied groups) then spectrum.
    let mut data = BitW::default();
    for (i, &(slen, copied)) in sf_slots.iter().enumerate() { if !copied { data.put(u32::from(sf[i]), slen); } }
    for line in (0..big_end).step_by(2) {
        let region = if line < r1s { 0 } else if line < r2s { 1 } else { 2 };
        let table = usize::from(table_select[region]);
        if table != 0 { put_pair(&mut data, table, ints[line], ints[line + 1]); }
    }
    let (codes, lens) = if count1_table { (&COUNT1_B_CODES, &COUNT1_B_LENS) } else { (&COUNT1_A_CODES, &COUNT1_A_LENS) };
    for q in (big_end..c1_end).step_by(4) {
        let idx = (usize::from(ints[q] != 0) << 3) | (usize::from(ints[q + 1] != 0) << 2) | (usize::from(ints[q + 2] != 0) << 1) | usize::from(ints[q + 3] != 0);
        data.put(u32::from(codes[idx]), u32::from(lens[idx]));
        for k in 0..4 { if ints[q + k] != 0 { data.put(u32::from(ints[q + k] < 0), 1); } }
    }
    // Pick a gain that keeps the output near half scale.
    let energy: f64 = ints.iter().map(|&v| { let a = f64::from(v.abs()); a.powf(8.0 / 3.0) }).sum::<f64>().max(1.0);
    let target = 0.3 / (energy.sqrt() * 3.2);
    let mut gg = (210.0 + 4.0 * target.log2()).round() as i32 + rng.below(5) as i32 - 2 + if short { 8 * i32::from(subblock_gain[0]) } else { 0 }
        + if scale { 6 } else { 0 };
    gg = gg.clamp(60, 250);
    let part23 = data.bits as u32;
    Gr { switched, block_type, mixed, table_select, subblock_gain, r0c, r1c, preflag, scale, count1_table, scalefac_compress,
         global_gain: gg as u32, big_values: (big_end / 2) as u32, part23, data, sf, ints }
}

fn write_side(w: &mut BitW, cfg: &Cfg, channels: usize, grs: &[Vec<Gr>], scfsi: &[[bool; 4]; 2]) {
    let granules = grs.len();
    w.put(0, if cfg.v1 { 9 } else { 8 });
    w.put(0, match (cfg.v1, channels) { (true, 1) => 5, (true, _) => 3, (false, 1) => 1, (false, _) => 2 });
    if cfg.v1 { for ch in 0..channels { for g in 0..4 { w.put(u32::from(scfsi[ch][g]), 1); } } }
    for gr in 0..granules {
        for ch in 0..channels {
            let g = &grs[gr][ch];
            // PN_SYNTH_P23 skews the claimed part2_3_length of the last granule/channel (an experiment knob).
            let skew: i32 = if gr + 1 == granules && ch + 1 == channels { std::env::var("PN_SYNTH_P23").ok().and_then(|v| v.parse().ok()).unwrap_or(0) } else { 0 };
            w.put((g.part23 as i32 + skew).clamp(0, 4095) as u32, 12); w.put(g.big_values, 9); w.put(g.global_gain, 8);
            w.put(g.scalefac_compress, if cfg.v1 { 4 } else { 9 });
            w.put(u32::from(g.switched), 1);
            if g.switched {
                w.put(u32::from(g.block_type), 2); w.put(u32::from(g.mixed), 1);
                w.put(u32::from(g.table_select[0]), 5); w.put(u32::from(g.table_select[1]), 5);
                for s in 0..3 { w.put(u32::from(g.subblock_gain[s]), 3); }
            } else {
                for t in 0..3 { w.put(u32::from(g.table_select[t]), 5); }
                w.put(g.r0c, 4); w.put(g.r1c, 3);
            }
            if cfg.v1 { w.put(u32::from(g.preflag), 1); }
            w.put(u32::from(g.scale), 1); w.put(u32::from(g.count1_table), 1);
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 11 { eprintln!("usage: mp3synth OUT SEED VERSION(1|2|25) RATE_INDEX MODE MODE_EXT FRAMES SWITCHED_PCT MIXED_PCT DENSITY"); std::process::exit(2); }
    let out = &a[1];
    let mut rng = Rng(a[2].parse::<u64>().unwrap().wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let version: u32 = a[3].parse().unwrap();
    let rate_index: u32 = a[4].parse().unwrap();
    let mode: u32 = a[5].parse().unwrap();
    let mode_ext_arg: u32 = a[6].parse().unwrap();
    let frames: usize = a[7].parse().unwrap();
    let switched_pct: u32 = a[8].parse().unwrap();
    let mixed_pct: u32 = a[9].parse().unwrap();
    let density: u32 = a[10].parse().unwrap();
    let v1 = version == 1;
    let version_bits = match version { 1 => 3u32, 2 => 2, _ => 0 };
    let channels = if mode == 3 { 1 } else { 2 };
    let sample_rate = match (version, rate_index) { (1, 0) => 44_100, (1, 1) => 48_000, (1, _) => 32_000, (2, 0) => 22_050, (2, 1) => 24_000, (2, _) => 16_000, (_, 0) => 11_025, (_, 1) => 12_000, _ => 8_000 };
    let band_set = (match version { 1 => 3, 2 => 0, _ => 6 }) + rate_index as usize;
    let bitrates: [u32; 14] = if v1 { [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320] } else { [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160] };
    let si_len = match (v1, channels) { (true, 2) => 32usize, (true, _) => 17, (false, 2) => 17, (false, _) => 9 };
    let mut file = Vec::new();
    let granules = if v1 { 2 } else { 1 };
    let mut prev_frame: Option<Vec<Vec<Gr>>> = None;
    let mut state = [0u8; 2];
    let mut dec = l3::Decoder::new();
    let mut pcm = l3::Pcm::new();
    for _ in 0..frames {
        let mode_ext = if mode == 1 { if mode_ext_arg == 9 { rng.below(4) } else { mode_ext_arg } } else { 0 };
        let cfg = Cfg { v1, band_set, switched_pct, mixed_pct, density, is_stereo: mode == 1 && mode_ext & 1 != 0, ms: mode == 1 && mode_ext & 2 != 0 };
        let mut scfsi = [[false; 4]; 2];
        for ch in 0..channels { for g in 0..4 { scfsi[ch][g] = v1 && rng.chance(35); } }
        let mut attempt = 0;
        let (grs, bytes_needed) = loop {
            let mut grs: Vec<Vec<Gr>> = Vec::new();
            let mut st = state;
            let mut density_cfg = Cfg { density: (density >> attempt).max(1), ..Cfg { v1, band_set, switched_pct, mixed_pct, density, is_stereo: cfg.is_stereo, ms: cfg.ms } };
            density_cfg.ms = cfg.ms;
            for gr in 0..granules {
                let mut row: Vec<Gr> = Vec::new();
                for ch in 0..channels {
                    let prev = if gr == 1 { Some(&grs[0][ch]) } else { None };
                    let mut sf = scfsi[ch];
                    let cap = if cfg.is_stereo && ch == 1 { 36 + rng.below(250) as usize } else { 576 };
                    let force = if mode == 1 && ch == 1 { Some((row[0].block_type, row[0].mixed)) } else { None };
                    let g = make_gr(&mut rng, &density_cfg, cfg.is_stereo && ch == 1 && !v1, prev, &mut sf, cap, st[ch], force);
                    st[ch] = g.block_type;
                    scfsi[ch] = sf;
                    row.push(g);
                }
                grs.push(row);
            }
            for ch in 0..channels {
                if grs.iter().any(|row| row[ch].switched && row[ch].block_type == 2) { scfsi[ch] = [false; 4]; }
            }
            let total: usize = grs.iter().flatten().map(|g| g.data.bits).sum();
            if total / 8 + 1 <= 1400 && grs.iter().flatten().all(|g| g.part23 <= 4095) { break (grs, (total + 7) / 8); }
            attempt += 1;
        };
        // Smallest bitrate whose frame holds the main data.
        let mut chosen = None;
        for (i, &br) in bitrates.iter().enumerate() {
            let size = (if v1 { 144 } else { 72 }) * br as usize * 1000 / sample_rate as usize;
            if size >= 4 + si_len + bytes_needed { chosen = Some((i, size)); break; }
        }
        let Some((bi, size)) = chosen else { continue };
        for ch in 0..channels { state[ch] = grs[granules - 1][ch].block_type; }
        let mut grs = grs;
        let assemble = |grs: &Vec<Vec<Gr>>| -> Vec<u8> {
            let mut w = BitW::default();
            w.put(0x7ff, 11); w.put(version_bits, 2); w.put(1, 2); w.put(1, 1);
            w.put(bi as u32 + 1, 4); w.put(rate_index, 2); w.put(0, 1); w.put(0, 1);
            w.put(mode, 2); w.put(mode_ext, 2); w.put(0, 1); w.put(0, 1); w.put(0, 2);
            write_side(&mut w, &cfg, channels, grs, &scfsi);
            for gr in 0..granules { for ch in 0..channels { let d = grs[gr][ch].data.clone(); w.append(&d); } }
            let mut bytes = w.bytes;
            bytes.resize(size, 0);
            bytes
        };
        // Steer the output level with the global gains (a side-info field only).
        let target = if std::env::var_os("PN_SYNTH_LOUD").is_some() && rng.chance(15) { 1.2 + f64::from(rng.below(280)) / 100.0 } else { 0.03 + f64::from(rng.below(330)) / 1000.0 };
        for _ in 0..16 {
            let bytes = assemble(&grs);
            let header = l3::Header::parse(u32::from_be_bytes(bytes[..4].try_into().unwrap())).expect("header");
            let mut trial = dec.clone();
            if trial.decode_frame(&header, &bytes[4..], &mut pcm).is_err() { break; }
            let mut peak = (0..channels).flat_map(|c| pcm.data[c][..granules * 576].iter()).fold(0.0f64, |m, v| m.max(v.abs()));
            // The IMDCT overlap and filterbank memory play out in the following frame: include it.
            let mut silent = bytes.clone();
            for b in silent.iter_mut().skip(4) { *b = 0; }
            if trial.decode_frame(&header, &silent[4..], &mut pcm).is_ok() {
                peak = peak.max((0..channels).flat_map(|c| pcm.data[c][..granules * 576].iter()).fold(0.0f64, |m, v| m.max(v.abs())));
            }
            if peak <= 0.0 { break; }
            let delta = (4.0 * (target / peak).log2()).round() as i32;
            if delta == 0 { break; }
            for g in grs.iter_mut().flatten() { g.global_gain = (g.global_gain as i32 + delta).clamp(0, 255) as u32; }
        }
        if std::env::var_os("PN_SYNTH_TRACE").is_some() {
            for (gi, row) in grs.iter().enumerate() { for (ci, g) in row.iter().enumerate() {
                eprintln!("frame {} gr{gi} ch{ci}: switched {} type {} mixed {} part23 {} bv {} gg {} sc {} tables {:?} r0c {} r1c {} count1 {} sbg {:?} pre {} scale {} scfsi {:?}",
                    file.len() / 417, g.switched, g.block_type, g.mixed, g.part23, g.big_values, g.global_gain, g.scalefac_compress, g.table_select, g.r0c, g.r1c, g.count1_table, g.subblock_gain, g.preflag, g.scale, scfsi[ci]);
            } }
        }
        let bytes = assemble(&grs);
        let header = l3::Header::parse(u32::from_be_bytes(bytes[..4].try_into().unwrap())).expect("header");
        dec.decode_frame(&header, &bytes[4..], &mut pcm).expect("decode");
        file.extend_from_slice(&bytes);
        prev_frame = Some(grs);
    }
    let _ = prev_frame;
    std::fs::write(out, &file).unwrap();
    // Sanity: every frame must parse with the decoder's own header reader.
    let mut pos = 0; let mut n = 0;
    while pos + 4 <= file.len() {
        let h = l3::Header::parse(u32::from_be_bytes(file[pos..pos + 4].try_into().unwrap())).expect("valid header");
        pos += h.frame_bytes(); n += 1;
    }
    eprintln!("{out}: {n} frames, {} bytes", file.len());
}
