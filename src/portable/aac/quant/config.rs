//! Configuration of the quantiser stage for the engine (`ExtAudioFile m4af`, `brat` 256000, `cdqu` 0x40, `acbf` 1).
//!
//! `encoder::config` derives these from `(rate, layout, brat, cdqu, acbf)`; this module only holds the pieces the quantiser needs.

use crate::portable::aac::quant::kernel::{KernelConfig, PamParams};
use crate::portable::aac::quant::rate::RateConfig;

/// Everything the quantiser needs about the encoder configuration.
#[derive(Clone, Debug)]
pub struct QuantConfig {
    /// coded channels including the LFE (`LK+0xc0`, `BSE+0x35c`)
    pub nch: usize,
    /// quality mode (`H[0xe0]`): 0x40, the engine value; the other buckets (0x20, 0x60) are not an engine configuration.
    pub quality: u32,
    /// `'acbf'`: 1 (LongTermAverage), the engine value; other values are not an engine configuration.
    pub acbf: u8,
    pub kernel: KernelConfig,
    pub rate: RateConfig,
    pub pam: PamParams,
}
