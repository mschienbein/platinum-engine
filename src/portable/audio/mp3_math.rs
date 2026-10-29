//! Deterministic constant tables for the portable MP3 decoder.
//!
//! Every value is produced with IEEE-754 `+ - * /` and `sqrt` only, in a fixed
//! order, from integer arguments. No platform libm function, no fused
//! multiply-add and no flush-to-zero or denormals-are-zero dependence is
//! involved, so the tables are bit-identical on every supported host.

use super::mp3_tables::{WINDOW_H, BIG_TABLES, COUNT1_A_CODES, COUNT1_A_LENS, COUNT1_B_CODES, COUNT1_B_LENS};
use std::sync::LazyLock;

const PI: f64 = std::f64::consts::PI;

/// sin(x) for |x| <= pi/4 by its Maclaurin series (terms until below 1e-19).
fn sin_small(x: f64) -> f64 {
    let x2 = x * x;
    let mut term = x;
    let mut sum = x;
    let mut n = 1.0f64;
    for _ in 0..12 {
        term = -term * x2 / ((n + 1.0) * (n + 2.0));
        sum += term;
        n += 2.0;
    }
    sum
}

/// cos(x) for |x| <= pi/4.
fn cos_small(x: f64) -> f64 {
    let x2 = x * x;
    let mut term = 1.0f64;
    let mut sum = 1.0f64;
    let mut n = 0.0f64;
    for _ in 0..12 {
        term = -term * x2 / ((n + 1.0) * (n + 2.0));
        sum += term;
        n += 2.0;
    }
    sum
}

/// cos(pi * m / d) for a positive denominator `d`. The argument is reduced
/// exactly in integers, then evaluated on [0, pi/4].
pub(super) fn cos_pi_frac(m: i64, d: i64) -> f64 {
    let two_d = 2 * d;
    let mut r = m.rem_euclid(two_d);
    if r > d { r = two_d - r; } // cos(2pi - t) = cos t
    // now angle = pi * r / d in [0, pi]
    let (r, negative) = if 2 * r > d { (d - r, true) } else { (r, false) };
    // angle in [0, pi/2]; use sin(pi/2 - t) beyond pi/4
    let value = if 4 * r > d {
        let num = d - 2 * r; // pi/2 - angle = pi * num / (2d)
        sin_small(PI * (num as f64) / (two_d as f64))
    } else {
        cos_small(PI * (r as f64) / (d as f64))
    };
    if negative { -value } else { value }
}

/// sin(pi * m / d).
pub(super) fn sin_pi_frac(m: i64, d: i64) -> f64 {
    // sin(t) = cos(pi/2 - t) = cos(pi * (d - 2m) / (2d))
    cos_pi_frac(d - 2 * m, 2 * d)
}

/// Cube root by Newton iteration with basic arithmetic only (x > 0).
fn cbrt_positive(x: f64) -> f64 {
    if x == 0.0 { return 0.0; }
    let mut y = if x >= 1.0 { x } else { 1.0 };
    for _ in 0..80 {
        y = (2.0 * y + x / (y * y)) / 3.0;
    }
    y
}

/// Huffman decoding tree. A child entry is 0 (invalid), a node index, or
/// `0x8000 | symbol`.
pub(super) struct Tree { pub nodes: Vec<[u16; 2]> }
impl Tree {
    fn build(items: impl Iterator<Item = (u16, u8, u16)>) -> Tree {
        let mut nodes: Vec<[u16; 2]> = vec![[0, 0]];
        for (code, len, symbol) in items {
            if len == 0 { continue; }
            let mut at = 0usize;
            for bit_index in (0..len).rev() {
                let bit = usize::from(((u32::from(code) >> bit_index) & 1) as u8);
                if bit_index == 0 {
                    nodes[at][bit] = 0x8000 | symbol;
                } else {
                    let next = nodes[at][bit];
                    if next == 0 {
                        nodes.push([0, 0]);
                        let new_index = (nodes.len() - 1) as u16;
                        nodes[at][bit] = new_index;
                        at = new_index as usize;
                    } else {
                        at = usize::from(next);
                    }
                }
            }
        }
        Tree { nodes }
    }
}

/// All constant data used by the Layer III decoder.
pub(super) struct Tables {
    /// |x|^(4/3) for x in 0..8207 (largest escape: 15 + 8191).
    pub pow43: Vec<f64>,
    /// 2^(e/4) for e in -QUARTER_BIAS..QUARTER_LIMIT.
    pub quarter: Vec<f64>,
    pub alias_cs: [f64; 8],
    pub alias_ca: [f64; 8],
    /// win[i] * cos(pi/72 * (2i + 19) * (2k + 1)), indexed [block_type][k][i].
    pub imdct36: [Vec<f64>; 4],
    /// win_short[i] * cos(pi/24 * (2i + 7) * (2k + 1)), indexed [k][i].
    pub imdct12: Vec<f64>,
    /// MPEG-1 intensity stereo: left and right gains for is_pos 0..=6.
    pub is_left: [f64; 7],
    pub is_right: [f64; 7],
    /// cos(pi * m * (2k + 1) / 64), indexed [m][k] for m in 0..32.
    pub matrix: Vec<f64>,
    /// Synthesis window arranged as D[64a + j] and D[64a + 32 + j] per a, j.
    pub window_even: [[f64; 32]; 8],
    pub window_odd: [[f64; 32]; 8],
    pub big_trees: Vec<Option<Tree>>,
    pub count1_trees: [Tree; 2],
}

pub(super) const QUARTER_BIAS: i32 = 512;
pub(super) const QUARTER_LIMIT: i32 = 128;

static TABLES: LazyLock<Tables> = LazyLock::new(Tables::new);

pub(super) fn tables() -> &'static Tables {
    &TABLES
}

impl Tables {
    fn new() -> Tables {
        let mut pow43 = Vec::with_capacity(8207);
        for i in 0..8207u32 {
            let x = f64::from(i);
            pow43.push(if i == 0 { 0.0 } else { x * cbrt_positive(x) });
        }
        // 2^(1/4) = sqrt(sqrt(2)); exact power-of-two scaling does the rest.
        let q1 = 2.0f64.sqrt().sqrt();
        let q2 = 2.0f64.sqrt();
        let q3 = q1 * q2;
        let steps = [1.0, q1, q2, q3];
        let mut quarter = Vec::with_capacity((QUARTER_BIAS + QUARTER_LIMIT) as usize);
        for e in -QUARTER_BIAS..QUARTER_LIMIT {
            let whole = e.div_euclid(4);
            let part = e.rem_euclid(4) as usize;
            quarter.push(pow2_int(whole) * steps[part]);
        }
        let ci = [-0.6f64, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037];
        let mut alias_cs = [0.0; 8];
        let mut alias_ca = [0.0; 8];
        for i in 0..8 {
            let sq = (1.0 + ci[i] * ci[i]).sqrt();
            alias_cs[i] = 1.0 / sq;
            alias_ca[i] = ci[i] / sq;
        }
        // IMDCT windows. sin(pi/36 (i + 1/2)) = cos(pi (35 - 2i) / 72).
        let sin36 = |i: i64| cos_pi_frac(35 - 2 * i, 72);
        let sin12 = |i: i64| cos_pi_frac(11 - 2 * i, 24);
        let mut win_long = [[0.0f64; 36]; 4];
        for i in 0..36i64 {
            let u = i as usize;
            win_long[0][u] = sin36(i);
            win_long[1][u] = if i < 18 { sin36(i) } else if i < 24 { 1.0 } else if i < 30 { sin12(i - 18) } else { 0.0 };
            win_long[3][u] = if i < 6 { 0.0 } else if i < 12 { sin12(i - 6) } else if i < 18 { 1.0 } else { sin36(i) };
        }
        let mut win_short = [0.0f64; 12];
        for i in 0..12i64 { win_short[i as usize] = sin12(i); }
        let imdct36 = [0usize, 1, 2, 3].map(|block_type| {
            let mut table = vec![0.0f64; 18 * 36];
            if block_type != 2 {
                for k in 0..18i64 {
                    for i in 0..36i64 {
                        // Window folded into the basis, per the usual IMDCT definition.
                        table[(k * 36 + i) as usize] = win_long[block_type][i as usize] * cos_pi_frac((2 * i + 19) * (2 * k + 1), 72);
                    }
                }
            }
            table
        });
        let mut imdct12 = vec![0.0f64; 6 * 12];
        for k in 0..6i64 {
            for i in 0..12i64 {
                imdct12[(k * 12 + i) as usize] = win_short[i as usize] * cos_pi_frac((2 * i + 7) * (2 * k + 1), 24);
            }
        }
        let mut is_left = [0.0f64; 7];
        let mut is_right = [0.0f64; 7];
        for pos in 0..7i64 {
            let s = sin_pi_frac(pos, 12);
            let c = cos_pi_frac(pos, 12);
            // ratio = tan(pos*pi/12); L = ratio/(1+ratio), R = 1/(1+ratio)
            is_left[pos as usize] = s / (s + c);
            is_right[pos as usize] = c / (s + c);
        }
        let mut matrix = vec![0.0f64; 32 * 32];
        for m in 0..32i64 {
            for k in 0..32i64 {
                matrix[(m * 32 + k) as usize] = cos_pi_frac(m * (2 * k + 1), 64);
            }
        }
        let window = |i: usize| -> f64 {
            let h = f64::from(WINDOW_H[if i <= 256 { i } else { 512 - i }]) / 65536.0;
            if (i / 64) % 2 == 1 { -h } else { h }
        };
        let mut window_even = [[0.0f64; 32]; 8];
        let mut window_odd = [[0.0f64; 32]; 8];
        for a in 0..8 {
            for j in 0..32 {
                window_even[a][j] = window(64 * a + j);
                window_odd[a][j] = window(64 * a + 32 + j);
            }
        }
        let big_trees = (0..32usize).map(|n| {
            BIG_TABLES[n].as_ref().map(|t| {
                Tree::build(t.codes.iter().zip(t.lens.iter()).enumerate().map(|(i, (&c, &l))| (c, l, i as u16)))
            })
        }).collect();
        let count1_trees = [
            Tree::build(COUNT1_A_CODES.iter().zip(COUNT1_A_LENS.iter()).enumerate().map(|(i, (&c, &l))| (c, l, i as u16))),
            Tree::build(COUNT1_B_CODES.iter().zip(COUNT1_B_LENS.iter()).enumerate().map(|(i, (&c, &l))| (c, l, i as u16))),
        ];
        Tables { pow43, quarter, alias_cs, alias_ca, imdct36, imdct12, is_left, is_right,
            matrix, window_even, window_odd, big_trees, count1_trees }
    }
}

/// 2^n for -1022 < n < 1023 by constructing the exponent field.
fn pow2_int(n: i32) -> f64 {
    f64::from_bits(((i64::from(n) + 1023) as u64) << 52)
}
