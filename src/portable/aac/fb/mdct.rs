//! F1: `AACAnalysisFilterBank` (0x16a4d4 `TimeToFrequencyInPlace`, 0x16a5e4 `PrepareTransform`) and `MDCT::ForwardInPlaceTransform`
//! (0x16fcec) / `DCT_IV::PostModulation` (0x16f7b8).
//!
//! Bit-exactness notes (checked by the encoder fixtures, with the generic NEON DFT of `crate::portable::dft`):
//! * every window product is a single f32 multiply (`vDSP_vmul`);
//! * fold: `u[n] = -(Q[h+n] + Q[h-1-n])`, `v[n] = P[h-1-n] - P[h+n]` (plain add/sub);
//! * pre-twiddle: `fma(Co, v, u*Ce)` / `fma(Ce, v, u*(-Co))` (a `fmul` and then one fused `fmla`), `Ce = 2.0*t[..]` is exact;
//! * post-twiddle: `fma(t[2k+1], I[k], R[k]*t[2k])`, `fma(t[N-1-2k], R[h-1-k], I[h-1-k]*(-t[N-2-2k]))`;
//! * the DFT is the forward 512-/64-point complex DFT of `crate::portable::dft::DftSetup` (generic NEON kernel = write class A).

use crate::portable::aac::fb::tables::{long_window, short_window, MOD_LONG, MOD_SHORT};
use crate::portable::dft::{DftSetup, Direction};

pub const SEQ_ONLY_LONG: u8 = 0;
pub const SEQ_LONG_START: u8 = 1;
pub const SEQ_EIGHT_SHORT: u8 = 2;
pub const SEQ_LONG_STOP: u8 = 3;

/// `MDCTFilterBank` scale (`this+0x68 / +0x90`), set in `MDCTFilterBank::Initialize`.
const MDCT_SCALE: f32 = 2.0;

/// Overlap state of one channel as saved by `MDCTFilterBank::GetState` (0x16a368: 1024 floats + the previous window-shape byte).
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelState {
    pub block: [f32; 1024],
    pub shape: u8,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self { block: [0.0; 1024], shape: 0 }
    }
}

/// One `AACAnalysisFilterBank` object (0x98 bytes in Apple's encoder): per-channel overlap state + previous window shape, and the two
/// DFT plans (`MultiRadixFFT` 512 / 64 points). The encoder owns two such objects, bank A (`psy+0x270`, psy short transforms) and bank B
/// (`psy+0x268`, coding transform); mono/SCE channels use bank B for both (see [`crate::portable::aac::fb::FbState`]).
pub struct FilterBank {
    state: Vec<ChannelState>,
    dft_long: DftSetup,
    dft_short: DftSetup,
}

impl FilterBank {
    /// `AACAnalysisFilterBank::Initialize(nch, 1024)` + `Reset()`: zero overlap state, shape bytes 0.
    pub fn new(nch: usize) -> Self {
        Self {
            state: vec![ChannelState::default(); nch],
            dft_long: DftSetup::new(512, Direction::Forward).expect("512-point setup"),
            dft_short: DftSetup::new(64, Direction::Forward).expect("64-point setup"),
        }
    }

    /// `MDCTFilterBank::GetState` (0x16a368).
    pub fn get_state(&self, ch: usize) -> ChannelState {
        self.state[ch].clone()
    }

    /// `MDCTFilterBank::SetState` (0x16a440): restores the overlap block and the shape byte.
    pub fn set_state(&mut self, ch: usize, st: &ChannelState) {
        self.state[ch] = st.clone();
    }

    /// `MDCTFilterBank::SetWindowShape` (0x16a430).
    pub fn set_window_shape(&mut self, ch: usize, shape: u8) {
        self.state[ch].shape = shape;
    }

    /// `AACAnalysisFilterBank::TimeToFrequencyInPlace(ch, shape, seq, buf)` (0x16a4d4): `buf` (1024 floats) is the time block on entry and
    /// the 1024 coefficients on return (`seq == 2`: 8 consecutive blocks of 128 in window order). Coefficients are in the ISO scale
    /// `X[k] = 2 sum w x cos(..)` (the psy copies are 0.5 X, taken by the caller).
    pub fn time_to_frequency_in_place(&mut self, ch: usize, shape: u8, seq: u8, buf: &mut [f32; 1024]) {
        let mut tmp = [0.0f32; 1024]; // Apple: stack array memset 0xff; every used element is overwritten (Q planes / DFT output planes)
        self.prepare_transform(ch, seq, shape, buf, &mut tmp);
        if seq == SEQ_EIGHT_SHORT {
            for k in 0..8 {
                let (p, q) = (&mut buf[128 * k..128 * k + 128], &mut tmp[128 * k..128 * k + 128]);
                let dft = &self.dft_short;
                mdct_forward(128, p, q, &MOD_SHORT, |ir, ii, or, oi| dft.execute(ir, ii, or, oi));
            }
        } else {
            let dft = &self.dft_long;
            mdct_forward(1024, buf, &mut tmp, &MOD_LONG, |ir, ii, or, oi| dft.execute(ir, ii, or, oi));
        }
    }

    /// `AACAnalysisFilterBank::PrepareTransform(ch, seq, shape, buf, tmp)` (0x16a5e4). On return `buf` holds the windowed
    /// first halves `P` and `tmp` the windowed second halves `Q` (for `seq == 2`: eight (P, Q) pairs of 128 at offset `128 k`).
    pub fn prepare_transform(&mut self, ch: usize, seq: u8, shape: u8, buf: &mut [f32; 1024], tmp: &mut [f32; 1024]) {
        let st = &mut self.state[ch];
        let ps = st.shape;
        let cur: [f32; 1024] = *buf;
        if seq == SEQ_EIGHT_SHORT {
            // T = S_c[448..1024] ++ cur[0..576] (1152 floats); S_c = cur
            let mut t = [0.0f32; 1152];
            t[..576].copy_from_slice(&st.block[448..1024]);
            t[576..].copy_from_slice(&cur[..576]);
            st.block = cur;
            let wcur = short_window(shape);
            for k in 0..8 {
                let wl = if k == 0 { short_window(ps) } else { wcur };
                for i in 0..128 {
                    buf[128 * k + i] = t[128 * k + i] * wl[i];
                    tmp[128 * k + i] = t[128 * k + 128 + i] * wcur[127 - i];
                }
            }
        } else {
            let mut p = st.block; // vDSP_vswap(buf, state)
            st.block = cur;
            if seq == SEQ_LONG_STOP {
                let wl = short_window(ps);
                for v in p[..448].iter_mut() {
                    *v = 0.0;
                }
                for i in 0..128 {
                    p[448 + i] *= wl[i];
                }
            } else {
                let wl = long_window(ps);
                for i in 0..1024 {
                    p[i] *= wl[i];
                }
            }
            if seq == SEQ_LONG_START {
                let ws = short_window(shape);
                tmp[..448].copy_from_slice(&cur[..448]);
                for i in 0..128 {
                    tmp[448 + i] = cur[448 + i] * ws[127 - i];
                }
                for v in tmp[576..1024].iter_mut() {
                    *v = 0.0;
                }
            } else {
                let wl = long_window(shape);
                for i in 0..1024 {
                    tmp[i] = cur[i] * wl[1023 - i];
                }
            }
            *buf = p;
        }
        st.shape = shape;
    }
}

/// `MDCT::ForwardInPlaceTransform(DCT_IV d, P, Q)` (0x16fcec) for `n = 1024` / `128` with the DFT supplied by the caller
/// (`dft(in_re, in_im, out_re, out_im)`, all slices `n/2` long: forward complex DFT, unnormalised). `p` holds the first-half windowed
/// block on entry and the `n` coefficients on return; `q` the second-half windowed block on entry (afterwards the DFT output planes).
pub fn mdct_forward<F>(n: usize, p: &mut [f32], q: &mut [f32], t: &[f32], dft: F)
where
    F: FnOnce(&[f32], &[f32], &mut [f32], &mut [f32]),
{
    debug_assert!(n == 1024 || n == 128);
    let h = n / 2;
    let mut zre = [0.0f32; 512];
    let mut zim = [0.0f32; 512];
    prefold(n, p, q, t, &mut zre[..h], &mut zim[..h]);
    let (qr, qi) = q[..n].split_at_mut(h);
    dft(&zre[..h], &zim[..h], qr, qi);
    postmod(n, qr, qi, t, p);
}

/// Fold + pre-twiddle (`mdct_prefold` of the C model): the two DFT input planes.
pub fn prefold(n: usize, p: &[f32], q: &[f32], t: &[f32], zre: &mut [f32], zim: &mut [f32]) {
    let h = n / 2;
    let mut u = [0.0f32; 512];
    let mut v = [0.0f32; 512];
    for i in 0..h {
        u[i] = -(q[h + i] + q[h - 1 - i]); // fadd, fneg
        v[i] = p[h - 1 - i] - p[h + i]; // fsub
    }
    let s = MDCT_SCALE;
    for k in 0..h / 2 {
        let ce = s * t[2 * k];
        let co = s * t[2 * k + 1];
        let (ue, ve) = (u[2 * k], v[2 * k]);
        zre[k] = co.mul_add(ve, ue * ce);
        zim[k] = ce.mul_add(ve, ue * (-co));
        let m = 2 * k + 1;
        let ce2 = s * t[n - 1 - m];
        let co2 = s * t[n - m];
        zre[h - 1 - k] = co2.mul_add(u[m], v[m] * ce2);
        zim[h - 1 - k] = ce2.mul_add(u[m], v[m] * (-co2));
    }
}

/// `DCT_IV::PostModulation` (0x16f7b8): `r`/`i` are the DFT output planes (n/2 each), `y` receives the n coefficients.
pub fn postmod(n: usize, r: &[f32], i: &[f32], t: &[f32], y: &mut [f32]) {
    let h = n / 2;
    for k in 0..h {
        y[2 * k] = t[2 * k + 1].mul_add(i[k], r[k] * t[2 * k]);
        y[2 * k + 1] = t[n - 1 - 2 * k].mul_add(r[h - 1 - k], i[h - 1 - k] * (-t[n - 2 - 2 * k]));
    }
}
