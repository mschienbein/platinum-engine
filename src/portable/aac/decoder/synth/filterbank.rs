//! `AACSynthesisFilterBank::FrequencyToTimeInPlace(ch, buf, shape, seq)` (0x20010) for ONE channel slot: IMDCT, windowing, overlap-add and the new overlap
//! state, plus the decoder-level [`Synthesizer`] that owns one bank per channel slot.

use crate::portable::aac::decoder::synth::imdct::{imdct_1024, imdct_128};
use crate::portable::aac::decoder::synth::{SynthError, WindowSequence, WindowShape};
use crate::portable::aac::fb::tables::{long_window as win_long, short_window as win_short};

/// Number of time samples (and spectral lines) per access unit and channel.
pub const FRAME: usize = 1024;

/// Overlap state and previous window shape of one channel slot (`state + 1024 * ch` and `prev_shape[ch]` of Apple's `AACSynthesisFilterBank`).
///
/// Initial state = state after `IMDCTFilterBank::Reset`: overlap all `+0.0`, previous shape 0 (sine).  Does not allocate.
#[derive(Clone, Debug)]
pub struct SynthesisFilterBank {
    overlap: [f32; FRAME],
    prev_shape: WindowShape,
}

impl SynthesisFilterBank {
    /// A bank in the reset state.
    pub fn new() -> Self {
        Self { overlap: [0.0; FRAME], prev_shape: WindowShape::Sine }
    }

    /// `IMDCTFilterBank::Reset` (0x1aa6c): zero overlap state, previous shape sine.
    pub fn reset(&mut self) {
        self.overlap = [0.0; FRAME];
        self.prev_shape = WindowShape::Sine;
    }

    /// One `FrequencyToTimeInPlace` call.  `spec` holds the dequantised spectrum on entry (ONLY_LONG / LONG_START / LONG_STOP: 1024 lines; EIGHT_SHORT:
    /// 8 windows x 128 lines, window-major, groups already de-interleaved) in Apple's int16-scaled domain, and the 1024 time-domain samples (same scale,
    /// before the `2^-15` output stage) on exit.  `shape` is the `window_shape` of the *current* frame; the left window half uses the shape of the
    /// previous call on this slot (for EIGHT_SHORT only window 0 does; windows 1..7 use the current shape).
    pub fn frequency_to_time_in_place(&mut self, spec: &mut [f32; FRAME], seq: WindowSequence, shape: WindowShape) {
        match seq {
            WindowSequence::EightShort => self.eight_short(spec, shape),
            _ => self.long(spec, seq, shape),
        }
        self.prev_shape = shape;
    }

    fn long(&mut self, spec: &mut [f32; FRAME], seq: WindowSequence, shape: WindowShape) {
        let (ps, cs) = (self.prev_shape as u8, shape as u8);
        let mut q = [0f32; FRAME];
        imdct_1024(spec, &mut q);
        let s = &mut self.overlap;
        let wl_cur = win_long(cs);
        match seq {
            WindowSequence::LongStop => {
                let ws_prev = win_short(ps);
                // the left window half is 0 on [0, 448), the short rising half on [448, 576) and 1 on [576, 1024)
                spec[..448].copy_from_slice(&s[..448]);
                for i in 448..576 {
                    spec[i] = spec[i] * ws_prev[i - 448] + s[i];
                }
                for i in 576..FRAME {
                    spec[i] += s[i];
                }
                for i in 0..FRAME {
                    s[i] = q[i] * wl_cur[FRAME - 1 - i];
                }
            }
            _ => {
                let wl_prev = win_long(ps);
                for i in 0..FRAME {
                    spec[i] = spec[i] * wl_prev[i] + s[i];
                }
                if seq == WindowSequence::LongStart {
                    let ws_cur = win_short(cs);
                    s[..448].copy_from_slice(&q[..448]);
                    for i in 0..128 {
                        s[448 + i] = q[448 + i] * ws_cur[127 - i];
                    }
                    s[576..].fill(0.0);
                } else {
                    for i in 0..FRAME {
                        s[i] = q[i] * wl_cur[FRAME - 1 - i];
                    }
                }
            }
        }
    }

    fn eight_short(&mut self, spec: &mut [f32; FRAME], shape: WindowShape) {
        let (ps, cs) = (self.prev_shape as u8, shape as u8);
        let (wl0, wc) = (win_short(ps), win_short(cs));
        // tmp[128 k ..] receives window k's contribution; tmp[i] maps to output position 448 + i, positions >= 1024 go to the overlap state
        let mut tmp = [0f32; 1152];
        let mut q = [0f32; 128];
        for k in 0..8 {
            let p: &mut [f32; 128] = (&mut spec[128 * k..128 * k + 128]).try_into().unwrap();
            imdct_128(p, &mut q);
            let wl = if k == 0 { wl0 } else { wc };
            for i in 0..128 {
                // vDSP_vma (unfused: product rounded, then added); window 0 adds to the cleared buffer
                let base = if k == 0 { 0.0 } else { tmp[128 * k + i] };
                tmp[128 * k + i] = p[i] * wl[i] + base;
            }
            for i in 0..128 {
                tmp[128 * (k + 1) + i] = q[i] * wc[127 - i];
            }
        }
        let s = &mut self.overlap;
        spec[..448].copy_from_slice(&s[..448]);
        for i in 0..576 {
            spec[448 + i] = tmp[i] + s[448 + i];
        }
        s[..576].copy_from_slice(&tmp[576..1152]);
        s[576..].fill(0.0);
    }
}

/// Decoder-level synthesis: one [`SynthesisFilterBank`] per channel slot (F2T call-counter order).
#[derive(Clone, Debug)]
pub(crate) struct Synthesizer {
    banks: Vec<SynthesisFilterBank>,
}

impl Synthesizer {
    /// `AACSynthesisFilterBank::Initialize(1024, channels)` (zero state).
    pub fn new(channels: usize) -> Self {
        Self { banks: (0..channels).map(|_| SynthesisFilterBank::new()).collect() }
    }

    /// `AACDecoder::Reset` / `IMDCTFilterBank::Reset`: zero every slot's overlap state and previous shape.  Apple also runs it after a decode error
    /// (`ProduceOutputBufferList` returns 0 frames and resets); a failed `Deserialize` makes no F2T call at all, so the caller resets afterwards.
    pub fn reset(&mut self) {
        self.banks.iter_mut().for_each(SynthesisFilterBank::reset);
    }

    /// One F2T call on slot `ch` with Apple's raw arguments (`seq` = ICS `byte[10]` & 3, `shape` = (`byte[10]` >> 2) & 1); `ch >= channels` and
    /// `seq > 3` are Apple's `-2` and touch nothing.
    pub fn frequency_to_time_raw(&mut self, ch: usize, spec: &mut [f32; FRAME], seq: u8, shape: u8) -> Result<(), SynthError> {
        let seq = WindowSequence::from_u8(seq).ok_or(SynthError::IllegalWindowSequence)?;
        let shape = WindowShape::from_u8(shape).ok_or(SynthError::IllegalWindowShape)?;
        self.banks.get_mut(ch).ok_or(SynthError::ChannelIndex)?.frequency_to_time_in_place(spec, seq, shape);
        Ok(())
    }
}
