//! Client channel conversion of `ExtAudioFile`'s converter: identity (client channels == source channels) and the planar/interleaved STEREO
//! client from a multichannel source (the engine's render input), `DownMixWithCoefficients` (AudioCodecs 0x2cd140).
//!
//! `out_o = in_f * k[o][f]` for the first input `f` with a non-zero coefficient, then for every further input in channel order: `k == 0`
//! skipped, `k == 1` `acc + in_i`, otherwise the fused `fma(in_i, k, acc)` (bit-exact against the recorded AudioToolbox output of every verified layout tag).
//! Coefficients: `c = 0.707f32`, `h = f32(c*c)` = 0.499849, `c + h` (an f32 add; the 7.1 `Lc`/`Rc` terms).  Matrices are keyed by the layout
//! tag and applied POSITIONALLY to the decoder's output order (that is what Apple does: for the PCE shape front C,L R,X X / back Ls Rs / LFE
//! (tag AAC_7_1_C) the decoder's list order differs from the tag's label order, the tag's matrix is applied anyway - probed).
//!
//! A 5.1 source whose decoder has seen an in-band PCE with a `matrix_mixdown_idx` is mixed with the ISO matrix of that index instead ([`matrix_mixdown_matrix`]).
//!
//! Scale: the planes may be in the decoder's int16-scaled domain or after the `2^-15` output scale; the scale is a power of two, so the
//! result commutes bit-exactly (barring denormals).

use crate::portable::aac::decoder::file::error::ClientError;
use crate::portable::aac::decoder::file::layout::*;

/// 2 x N mixing matrix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoMatrix {
    pub n: usize,
    pub l: [f32; 8],
    pub r: [f32; 8],
}

/// The coefficient constants.
pub fn coefficients() -> (f32, f32, f32) {
    let c = 0.707f32;
    let h = c * c;
    (c, h, c + h)
}

/// Stereo downmix matrix for a source layout tag.  `Err(NoDecoder)` = Apple refuses the planar/stereo client (`'!dat'`: AAC_6_0, AAC_7_0,
/// Octagonal); `Err(UnverifiedConversion)` = tag unknown to this module.
pub fn stereo_matrix(tag: u32) -> Result<StereoMatrix, ClientError> {
    let (c, h, ch) = coefficients();
    let (n, l, r): (usize, [f32; 8], [f32; 8]) = match tag {
        TAG_MONO => (1, [1., 0., 0., 0., 0., 0., 0., 0.], [1., 0., 0., 0., 0., 0., 0., 0.]),
        TAG_AAC_3_0 => (3, [0., 1., 0., 0., 0., 0., 0., 0.], [0., 0., 1., 0., 0., 0., 0., 0.]),
        TAG_AAC_4_0 | TAG_QUADRAPHONIC => (4, [1., 0., 0., 0., 0., 0., 0., 0.], [0., 1., 0., 0., 0., 0., 0., 0.]),
        TAG_AAC_5_0 => (5, [0., 1., 0., 0., 0., 0., 0., 0.], [0., 0., 1., 0., 0., 0., 0., 0.]),
        TAG_AAC_5_1 => (6, [c, 1., 0., c, 0., 0., 0., 0.], [c, 0., 1., 0., c, 0., 0., 0.]),
        TAG_AAC_6_1 => (7, [c, 1., 0., h, 0., h, 0., 0.], [c, 0., 1., 0., h, h, 0., 0.]),
        TAG_AAC_7_1 => (8, [c, ch, h, 1., 0., c, 0., 0.], [c, h, ch, 0., 1., 0., c, 0.]),
        TAG_AAC_7_1_B => (8, [c, 1., 0., h, 0., h, 0., 0.], [c, 0., 1., 0., h, 0., h, 0.]),
        TAG_AAC_7_1_C => (8, [c, c, 0., c, 0., 0., c, 0.], [c, 0., c, 0., c, 0., 0., c]),
        TAG_AAC_6_0 | TAG_AAC_7_0 | TAG_AAC_OCTAGONAL => return Err(ClientError::NoDecoder),
        _ => return Err(ClientError::UnverifiedConversion),
    };
    Ok(StereoMatrix { n, l, r })
}

/// The stereo matrix of the ISO 14496-3 matrix mixdown (`matrix_mixdown_idx` of an in-band PCE) for a 5.1 source (C L R Ls Rs LFE):
/// `L' = (L + c C + a Ls) / (1 + c + a)` with `c = 1/sqrt(2)` and `a` = 1/sqrt(2), 1/2, 1/(2 sqrt(2)), 0 for the index 0..=3, in `f32` as AudioToolbox
/// builds it (coefficients `norm * c` and `norm * a`, `norm = 1 / ((1 + c) + a)`); the LFE is left out.  Bit-exact against recordings of all four.
pub fn matrix_mixdown_matrix(index: u8) -> StereoMatrix {
    let c = std::f32::consts::FRAC_1_SQRT_2;
    let a = [c, 0.5, c * 0.5, 0.0][usize::from(index & 3)];
    let norm = 1.0 / ((1.0 + c) + a);
    let (kc, ka) = (norm * c, norm * a);
    StereoMatrix { n: 6, l: [kc, norm, 0., ka, 0., 0., 0., 0.], r: [kc, 0., norm, 0., ka, 0., 0., 0.] }
}

impl StereoMatrix {
    /// Mixes `inputs[0..n]` (equal-length planes) into `out_l` / `out_r`.
    ///
    /// The accumulation starts at the FIRST NON-ZERO coefficient (`acc = in_i * k_i`), later zero coefficients are skipped, coefficient 1
    /// adds, others fuse.  This is what the recorded AudioToolbox output says: a leading zero coefficient must not turn a `-0.0`
    /// input into `+0.0` (`in_0 * 0 + in_1` would; the recordings with `-0.0` samples prove it).
    pub fn apply(&self, inputs: &[&[f32]], out_l: &mut [f32], out_r: &mut [f32]) {
        for (k, out) in [(&self.l, out_l), (&self.r, out_r)] {
            let first = (0..self.n).find(|&i| k[i] != 0.0).unwrap_or(0);
            for t in 0..out.len() {
                let mut acc = inputs[first][t] * k[first];
                for i in first + 1..self.n {
                    let kk = k[i];
                    if kk == 0.0 {
                        continue;
                    } else if kk == 1.0 {
                        acc += inputs[i][t];
                    } else {
                        acc = inputs[i][t].mul_add(kk, acc);
                    }
                }
                out[t] = acc;
            }
        }
    }
}

/// Converter between the decoder's output planes and the client planes.
#[derive(Clone, Debug)]
pub struct ClientConverter {
    in_channels: usize,
    kind: Kind,
}

#[derive(Clone, Debug)]
enum Kind {
    Identity,
    /// The matrix of the layout tag, and whether the source is a 5.1 stream (the only one whose matrix an in-band PCE changes).
    Stereo { matrix: StereoMatrix, five_one: bool },
}

impl ClientConverter {
    /// Identity: client channel count == source channel count.
    pub fn identity(channels: usize) -> Self {
        ClientConverter { in_channels: channels, kind: Kind::Identity }
    }
    /// `client_channels == layout.channels` -> identity; `client_channels == 2` from another layout -> the verified downmix; anything else
    /// is `UnverifiedConversion`.
    pub fn new(layout: &Layout, client_channels: usize) -> Result<Self, ClientError> {
        let n = layout.channels as usize;
        if client_channels == n {
            return Ok(Self::identity(n));
        }
        if client_channels == 2 {
            return Ok(ClientConverter { in_channels: n, kind: Kind::Stereo { matrix: stereo_matrix(layout.tag)?, five_one: layout.tag == TAG_AAC_5_1 } });
        }
        Err(ClientError::UnverifiedConversion)
    }
    /// Whether this converter mixes a 5.1 source to stereo, the one mix-down an in-band PCE can change.
    pub fn applies_matrix_mixdown(&self) -> bool {
        matches!(self.kind, Kind::Stereo { five_one: true, .. })
    }
    pub fn out_channels(&self) -> usize {
        match self.kind {
            Kind::Identity => self.in_channels,
            Kind::Stereo { .. } => 2,
        }
    }
    pub fn is_identity(&self) -> bool {
        matches!(self.kind, Kind::Identity)
    }
    /// `inputs.len() == in_channels`, `outputs.len() == out_channels`, all planes equally long.  `matrix_mixdown` is the index of the last in-band PCE
    /// that had one: a 5.1 source is then mixed with the ISO matrix of that index.
    pub fn convert(&self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], matrix_mixdown: Option<u8>) {
        match &self.kind {
            Kind::Identity => {
                for (o, i) in outputs.iter_mut().zip(inputs) {
                    o.copy_from_slice(i);
                }
            }
            Kind::Stereo { matrix, five_one } => {
                let iso;
                let m = match matrix_mixdown {
                    Some(index) if *five_one => {
                        iso = matrix_mixdown_matrix(index);
                        &iso
                    }
                    _ => matrix,
                };
                let (a, b) = outputs.split_at_mut(1);
                m.apply(inputs, &mut a[0], &mut b[0]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(tag: u32) -> &'static Layout {
        layout_for_tag(tag).expect("a known tag")
    }

    /// Six planes (C L R Ls Rs LFE) of 3 samples with distinct values.
    fn planes() -> Vec<Vec<f32>> {
        (0..6).map(|ch| (0..3).map(|t| 0.1 * (ch as f32 + 1.0) + 0.01 * t as f32).collect()).collect()
    }

    fn mix(layout: &Layout, matrix_mixdown: Option<u8>, planes: &[Vec<f32>]) -> [Vec<f32>; 2] {
        let converter = ClientConverter::new(layout, 2).unwrap();
        let ins: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
        let (mut l, mut r) = (vec![0.0f32; 3], vec![0.0f32; 3]);
        converter.convert(&ins, &mut [&mut l, &mut r], matrix_mixdown);
        [l, r]
    }

    #[test]
    fn the_matrix_mixdown_of_a_5_1_stream_is_the_iso_one() {
        let c = std::f64::consts::FRAC_1_SQRT_2;
        let planes = planes();
        for (index, a) in [c, 0.5, c / 2.0, 0.0].into_iter().enumerate() {
            let [l, r] = mix(layout(TAG_AAC_5_1), Some(index as u8), &planes);
            for t in 0..3 {
                let x = |ch: usize| f64::from(planes[ch][t]);
                let norm = 1.0 / (1.0 + c + a);
                assert!((f64::from(l[t]) - norm * (x(1) + c * x(0) + a * x(3))).abs() < 1e-6, "index {index} L[{t}]");
                assert!((f64::from(r[t]) - norm * (x(2) + c * x(0) + a * x(4))).abs() < 1e-6, "index {index} R[{t}]");
            }
        }
        // without an index (or with a PCE that had none) the matrix of the layout tag stays: L + 0.707 (C + Ls), not normalised
        let default = mix(layout(TAG_AAC_5_1), None, &planes);
        for t in 0..3 {
            let x = |ch: usize| f64::from(planes[ch][t]);
            assert!((f64::from(default[0][t]) - (x(1) + 0.707 * (x(0) + x(3)))).abs() < 1e-6);
        }
        assert_ne!(default, mix(layout(TAG_AAC_5_1), Some(3), &planes));
    }

    #[test]
    fn the_other_layouts_keep_their_matrices_whatever_the_index() {
        let planes = planes();
        for tag in [TAG_AAC_3_0, TAG_AAC_4_0, TAG_AAC_5_0] {
            let layout = layout(tag);
            let n = usize::from(layout.channels);
            let short = &planes[..n];
            for index in 0..4u8 {
                assert_eq!(mix(layout, Some(index), short), mix(layout, None, short), "{:#x} index {index}", layout.tag);
            }
        }
        let eight: Vec<Vec<f32>> = (0..8).map(|ch| (0..3).map(|t| 0.1 * (ch as f32 + 1.0) + 0.01 * t as f32).collect()).collect();
        for tag in [TAG_AAC_7_1, TAG_AAC_7_1_B, TAG_AAC_7_1_C] {
            for index in 0..4u8 {
                assert_eq!(mix(layout(tag), Some(index), &eight), mix(layout(tag), None, &eight), "{tag:#x} index {index}");
            }
        }
    }

    #[test]
    fn the_matrix_mixdown_coefficients_are_the_f32_ones_audiotoolbox_uses() {
        let c = std::f32::consts::FRAC_1_SQRT_2;
        for (index, a) in [c, 0.5, c * 0.5, 0.0].into_iter().enumerate() {
            let m = matrix_mixdown_matrix(index as u8);
            let norm = 1.0 / ((1.0 + c) + a);
            assert_eq!((m.n, m.l, m.r), (6, [c * norm, norm, 0., a * norm, 0., 0., 0., 0.], [c * norm, 0., norm, 0., a * norm, 0., 0., 0.]));
        }
        // index 3 leaves the surround channels out altogether
        assert_eq!(matrix_mixdown_matrix(3).l[3], 0.0);
    }
}
