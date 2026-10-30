//! Per-AU glue: budget block -> loop kernel -> AU size -> statistics/bias feedback, in the order of `_aacEncodeFrame` /
//! `AACEncoder::AACEncodeOutput`. The integrating encoder drives one [`Quantizer`] per stream.

use crate::portable::aac::quant::config::QuantConfig;
use crate::portable::aac::quant::kernel::LoopKernel;
use crate::portable::aac::quant::output::to_channel_stream;
use crate::portable::aac::quant::rate::{au_bytes, side_info_bits, BiasStep, ElementSide, RateState};
use crate::portable::aac::types::{ChannelStream, PsyOut};

/// Result of [`Quantizer::quantize_frame`].
#[derive(Clone, Debug)]
pub struct QuantizedFrame {
    /// AU size in bytes = `ceil((side + sum L)/8)`
    pub au_bytes: u32,
    /// every channel has `max_sfb == 0`
    pub silent: bool,
}

/// Quantiser stage of one encoder instance: persistent loop-kernel channels + rate state.
pub struct Quantizer {
    pub cfg: QuantConfig,
    pub kernel: LoopKernel,
    pub rate: RateState,
}

impl Quantizer {
    pub fn new(cfg: QuantConfig) -> Self {
        debug_assert!(cfg.acbf == 1 || cfg.acbf == 2, "only acbf=1 (LongTermAverage, engine) and acbf=2 are implemented");
        debug_assert_eq!(cfg.quality, 0x40, "only quality 0x40 is implemented");
        let kernel = LoopKernel::new(cfg.kernel.clone());
        let rate = RateState::new(&cfg.rate);
        Self { cfg, kernel, rate }
    }

    /// Budget block + `_AdvanceLoopKernel` for one AU. `psy[c]` = final psy-out of channel `c` (kernel channel order),
    /// `elements` = side-information description of every element (see [`ElementSide`]), `pce_bits` = `8*BSE[0x480]` in the first AU of
    /// PCE layouts (4 and 6.1 channels), else 0. Does not touch the statistics: call [`Quantizer::end_au`] after the AU is written.
    pub fn quantize_frame(&mut self, psy: &[&PsyOut], elements: &[ElementSide], pce_bits: i32) -> QuantizedFrame {
        let side = side_info_bits(0, pce_bits, elements);
        let budget = self.rate.kernel_budget(side);
        let nch = self.cfg.nch;
        let kout = self.kernel.advance_loop_kernel(&psy[..nch], &self.cfg.pam, &budget);
        let silent = psy[..nch].iter().all(|p| p.max_sfb == 0);
        QuantizedFrame { au_bytes: au_bytes(side, &kout.bits[..nch]), silent }
    }

    /// `AACEncodeOutput` statistics for the AU just written (`frame.au_bytes` bytes): returns the bias index `B` for the psy stage
    /// (`stepped` => re-run `_adaptPamParam`).
    pub fn end_au(&mut self, frame: &QuantizedFrame) -> BiasStep {
        self.rate.after_au(frame.au_bytes, frame.silent)
    }

    /// Writer-contract view of channel `c` after [`Quantizer::quantize_frame`].
    pub fn channel_stream(&self, c: usize, psy: &PsyOut) -> ChannelStream {
        to_channel_stream(&self.kernel.ch[c], psy)
    }
}
