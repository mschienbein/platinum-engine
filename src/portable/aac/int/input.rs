//! Inputs of the intensity stage that are not part of `crate::portable::aac::types::PsyOut`: the per-line energy / pair arrays that Apple keeps next to the
//! `psyOut` (`+0x08`, `+0x10`, `+0x9c0`, `+0x9d0`) and the TNS filter ranges (`+0xbc0`). They are produced by `AACEncoder::EncodeFrame` upstream.

/// Per-channel line data hanging off a `psyOut`.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelLines {
    /// `psyOut+0x08` (= `CH+0x1e0`): 1024 per-line power of the long-window psy spectrum, `2*a_long^2` in every block type (for EIGHT_SHORT frames it is
    /// the long-window analysis of the same segment, NOT the short spectrum; band layout `tabShort[k]*8` for `_estimateIldDistortionShort`). Equals
    /// `0.5*spectrum^2` of the coded spectrum only below the bandwidth lowpass (not at 48 kHz lines >= 929).
    pub en: Vec<f32>,
    /// `psyOut+0x10[w]` (= `CH+0x1e8+8w`; 8 windows x 128 lines, window-major): power `2*a_short[w]^2` of the 8 short-window psy spectra of the current block
    /// (computed in every frame; used by the short-block analysis and by the stationarity term of long-type blocks).
    pub winen: Vec<f32>,
    /// `psyOut+0x9c0` pair array, real parts "A": the long-window psy spectrum `a_long` (1024 entries) for long-type blocks; the 8 short-window psy spectra
    /// `a_short` (8 x 128, window-major, UNSORTED window layout) for EIGHT_SHORT.
    pub pair_re: Vec<f32>,
    /// Imaginary parts (same layout) when the pair arrays are not aliased (`pair[0].re != pair[0].im`); `None` = aliased (`re == im`),
    /// which is the only case the encoder ever produces for cdqu <= 0x40 (LC engine).
    pub pair_im: Option<Vec<f32>>,
    /// `psyOut+0x9d0`: set by `_AdvancePsyOut(.., param_8)`; enables the stationarity term for long-type blocks. 1 in every frame the engine encodes.
    pub psy_spectrum_flag: bool,
}

impl ChannelLines {
    /// Build the lines from the two psychoacoustic spectra that `EncodeFrame` keeps per channel:
    /// `a_long` = `*(chan+0x148)`: the 0.5-scaled, TNS-filtered LONG-window MDCT spectrum (1024; for EIGHT_SHORT frames the long-window spectrum of
    /// the same segment computed by bank B), `a_short` = `chan+0x1d8`: the 0.5-scaled 8 short-window MDCTs of the current block (8 x 128, window-major,
    /// computed in every frame). Bit-exact with Apple's values: `en == 2*a_long^2` and `winen == 2*a_short^2`
    /// (E5: `vsq` + `vsmul 2.0` on the aliased split-complex spectrum), and `pair_re = short ? a_short : a_long`.
    pub fn from_psy_spectra(a_long: &[f32], a_short: &[f32], eight_short: bool, psy_spectrum_flag: bool) -> Self {
        let en: Vec<f32> = a_long[..1024].iter().map(|&x| 2.0 * (x * x)).collect();
        let winen: Vec<f32> = a_short[..1024].iter().map(|&x| 2.0 * (x * x)).collect();
        let pair_re = if eight_short { a_short[..1024].to_vec() } else { a_long[..1024].to_vec() };
        Self { en, winen, pair_re, pair_im: None, psy_spectrum_flag }
    }
}

/// TNS filter ranges of one channel in sfb units as stored in Apple's TNS info (`psyOut+0xbc0`): defined in `crate::portable::aac::types` (produced by `fb`,
/// carried in `TnsChannel::ranges`). `present` = byte 0 of the TNS struct `== 1`; per window `count` filters (the analysis honours at most 3), each
/// `range = (start_sfb, end_sfb)` (e.g. `(4,14)` = a 6th-order filter over short sfb 4..13; `end` = top sfb, `start = end - length`).
pub use crate::portable::aac::types::TnsRanges;
