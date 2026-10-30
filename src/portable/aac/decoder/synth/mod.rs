//! Synthesis stage of Apple's AAC-LC decoder (AudioCodecs component, macOS 27.2, arm64): per-channel spectrum -> IMDCT (DCT-IV pre-twiddle, the
//! generic 512/64-point complex DFT of [`crate::portable::dft`], post-twiddle) -> window -> overlap-add, in the decoder's int16-scaled domain.
//!
//! * [`SynthesisFilterBank`]: one channel slot (`AACSynthesisFilterBank::FrequencyToTimeInPlace`, overlap state, previous window shape).
//! * [`Synthesizer`]: all channel slots of a decoder instance + `Reset`.
//! * [`output`]: the `2^-15` output scale.
//!
//! The 1024-line transform is the generic NEON kernel ("output class G"), the one the engine's read calls reach; the SME 512-point kernel Apple
//! runs when `ExtAudioFileRead` is entered with a differently aligned stack is not reproduced.
//!
//! Numerics: every placement of fused (`mul_add`) and separately rounded operations is taken from the machine code; no `f64`, no intrinsics, no host
//! dependent float behaviour: every finite result is identical on aarch64 and x86_64. A spectrum that overflowed to infinity (a resonant TNS filter)
//! makes NaNs here; they are NaNs on every host, but their sign bit is the host's arithmetic's (ARM's `fneg` flips it, so both `0x7fc00000` and
//! `0xffc00000` occur in the output on aarch64, and x86 makes `0xffc00000` of an invalid operation); the bits of non-finite output are not part of
//! the cross-platform guarantee.
//! The window and modulation tables are those of the encoder's filterbank ([`crate::portable::aac::fb::tables`]), which are Apple's.

mod filterbank;
mod imdct;
pub mod output;

pub(crate) use filterbank::{Synthesizer, FRAME};

/// `window_sequence` (`ICSInfo.byte[10] & 3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WindowSequence {
    OnlyLong = 0,
    LongStart = 1,
    EightShort = 2,
    LongStop = 3,
}

impl WindowSequence {
    /// `None` for values above 3 (Apple's F2T returns -2).
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::OnlyLong),
            1 => Some(Self::LongStart),
            2 => Some(Self::EightShort),
            3 => Some(Self::LongStop),
            _ => None,
        }
    }
}

/// `window_shape` (`ICSInfo.byte[10] >> 2 & 1`): 0 sine, 1 Kaiser-Bessel-derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum WindowShape {
    #[default]
    Sine = 0,
    Kbd = 1,
}

impl WindowShape {
    /// `None` for values above 1.
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Sine),
            1 => Some(Self::Kbd),
            _ => None,
        }
    }
}

/// Window parameters the spectral stage hands over are out of range (cannot happen for `decoder::bits` output).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SynthError {
    /// channel slot index >= channel count (Apple's F2T `-2`)
    ChannelIndex,
    /// raw `window_sequence` > 3 (Apple's F2T `-2`)
    IllegalWindowSequence,
    /// raw `window_shape` > 1
    IllegalWindowShape,
}

impl std::fmt::Display for SynthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SynthError::ChannelIndex => "channel slot beyond the channel count",
            SynthError::IllegalWindowSequence => "window sequence above 3",
            SynthError::IllegalWindowShape => "window shape above 1",
        })
    }
}

impl std::error::Error for SynthError {}
