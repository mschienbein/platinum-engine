//! The decoder core: access unit -> time blocks.  `decoder::bits` (bytes -> per-channel spectrum + window info) feeds `decoder::synth` (IMDCT, window,
//! overlap-add) slot by slot in F2T-counter order; this is `AACDecoder::DecodeFrame` + `ACMP4AACBaseDecoder::ProduceOutputBufferList`.
//!
//! * A rejected access unit (`status != 0`) makes **no** F2T call and yields 0 frames; Apple then runs `ACMP4AACBaseDecoder::Reset`
//!   (`ProduceOutputBufferList` status 1): the synthesis state (overlap, previous window shape) is zeroed and the tag-adoption flag of the bits
//!   stage is restored; the PNS LCG is NOT touched.
//! * The blocks returned by [`AuDecoder::block`] are in the decoder's int16-scaled domain (before `2^-15`), which is where Apple's client-format mixer
//!   (`DownMixWithCoefficients`) runs.

use crate::portable::aac::decoder::bits::{AuResult, ConfigError, PceLists, SpectralConfig, SpectralDecoder};
use crate::portable::aac::decoder::file::DecoderConfig;
use crate::portable::aac::decoder::synth::{Synthesizer, FRAME};

/// `decoder::file`'s [`DecoderConfig`] -> the element list of `decoder::bits` (Apple's `CreateList`).
pub(crate) fn spectral_config(cfg: &DecoderConfig) -> Result<SpectralConfig, ConfigError> {
    let pce = cfg.pce.as_ref().map(|p| PceLists {
        front: p.front.iter().map(|e| (e.is_cpe, e.tag)).collect(),
        side: p.side.iter().map(|e| (e.is_cpe, e.tag)).collect(),
        back: p.back.iter().map(|e| (e.is_cpe, e.tag)).collect(),
        lfe: p.lfe.clone(),
        num_cc: p.cc.len().min(255) as u8,
    });
    SpectralConfig::new(cfg.sfi, cfg.channel_config, pce.as_ref())
}

/// Access-unit decoder: spectral stage + synthesis, one instance per stream.  `Send`, no shared state.
pub(crate) struct AuDecoder {
    bits: SpectralDecoder,
    synth: Synthesizer,
    au: AuResult,
    /// time blocks of the last decoded access unit, one per output channel, int16-scaled domain
    src: Vec<[f32; FRAME]>,
}

impl AuDecoder {
    pub fn new(cfg: SpectralConfig) -> Self {
        let bits = SpectralDecoder::new(cfg);
        let n = bits.num_channels();
        AuDecoder { bits, synth: Synthesizer::new(n), au: AuResult::default(), src: vec![[0.0; FRAME]; n] }
    }

    /// Output channels (decoder element-list order).
    pub fn channels(&self) -> usize {
        self.src.len()
    }

    /// The matrix mixdown index of the last in-band PCE seen (see `SpectralDecoder::matrix_mixdown`).
    pub fn matrix_mixdown(&self) -> Option<u8> {
        self.bits.matrix_mixdown()
    }

    /// See `SpectralDecoder::pce_unreproduced`.
    pub fn pce_unreproduced(&self) -> bool {
        self.bits.pce_unreproduced()
    }

    /// Time blocks (`channels() x 1024`, int16-scaled) of the last successfully decoded access unit.
    pub fn block(&self) -> &[[f32; FRAME]] {
        &self.src
    }

    /// Decode one access unit.  `false` = rejected (no frames; the decoder is reset as Apple does: synthesis state zero, previous shape sine, tag
    /// adoption restored, PNS LCG kept).
    pub fn decode_block(&mut self, au: &[u8]) -> bool {
        self.bits.decode_au_into(au, &mut self.au);
        if self.au.status != 0 {
            self.synth.reset();
            self.bits.reset();
            return false;
        }
        debug_assert_eq!(self.au.channels.len(), self.src.len());
        for (i, ch) in self.au.channels.iter().enumerate() {
            let buf = &mut self.src[i];
            *buf = ch.coeffs;
            self.synth
                .frequency_to_time_raw(i, buf, ch.window_sequence, ch.window_shape)
                .expect("decoder::bits yields window sequence <= 3, shape <= 1 and channel < num_channels");
        }
        true
    }
}
