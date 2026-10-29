//! `ExtAudioFile` gapless parameters and the converter's decode-failure tolerance, as `ExtAudioFileRead` applies them to
//! an MP3 source. The read protocol itself (priming skip, clamp, packet groups, error propagation) lives in
//! [`super::reader`]; this module holds the two pieces of state it needs from the container layer.
//!
//! The AudioCodec (`ACMP3Decoder`) delivers `frames_per_packet` frames per AudioFile packet (the first packet is
//! `frames_per_packet - 529` frames: the decoder delay is dropped inside the codec, and 529 zero frames follow at the
//! end of the stream). `ExtAudioFile` reads the packet table info once, when the client format is set
//! (`UpdateInternals` / `Seek(0)`): priming frames `p` (skipped from the start of the stream) and the valid frame count
//! `n` (`(long)(valid + 0.5)`, ratio 1.0); an MP3 without table info (`pnfo` answers 'chk?') has `p = 0` and no clamp.
//! `n == 0` means "no clamp" (the whole stream), `n < 0` delivers nothing; the `FileLengthFrames` property is
//! independent of that and keeps its own value.

use super::container::{Container, PacketTableInfo};

/// Priming skip and valid-frame clamp fixed at set-client time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gapless {
    skip: u64,
    /// `ExtAudioFile` +0x48: 0 = no clamp, negative = nothing is ever delivered.
    total: i64,
}

impl Gapless {
    /// The trim `ExtAudioFile` computed when the client format was set on `c`.
    pub fn new(c: &Container) -> Self {
        Self::from_table(c.packet_table_info())
    }

    /// Same from an explicit packet table (`None` = the AudioFile answered 'chk?').
    pub fn from_table(info: Option<PacketTableInfo>) -> Self {
        match info {
            None => Gapless { skip: 0, total: 0 },
            Some(p) => {
                // +0x50 = (int)(ratio * priming + 0.5); Seek(0) clamps the skip to >= 0
                let skip = (f64::from(p.priming) + 0.5) as i32;
                // +0x48 = (long)(valid * ratio * ratio + 0.5)
                Gapless { skip: skip.max(0) as u64, total: (p.valid_frames as f64 + 0.5) as i64 }
            }
        }
    }

    /// Frames of the codec stream that are discarded before the first delivered one.
    pub fn priming(&self) -> u64 {
        self.skip
    }

    /// The valid-frame clamp `ExtAudioFile` keeps (+0x48): 0 = no clamp, negative = nothing is delivered.
    pub fn total_frames(&self) -> i64 {
        self.total
    }
}

/// `'bada'` (kAudioCodecBadDataError): what `ExtAudioFileRead` returns when the decoder keeps failing.
pub const STATUS_BAD_DATA: i32 = 0x6261_6461;

/// The converter's tolerance for failed steps inside one `AudioConverterFillComplexBuffer` call (observed with lldb, see
/// REPORT.md): a `ProduceOutputPackets` that returns an error (-50 for an undecodable frame) or an input callback that
/// fails counts as one failure; the fifth consecutive failure aborts the call, the fourth when the call has not yet put a
/// single frame into its output (the counter starts at 1 then). Any Produce that returns frames resets the count. The status
/// of the abort is that of the failing step: [`STATUS_BAD_DATA`] for a decode failure, the callback's error otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadDataGuard {
    consecutive: u32,
}

impl Default for BadDataGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl BadDataGuard {
    /// Failures in a row that abort the call.
    pub const LIMIT: u32 = 5;

    /// A call that has no output yet.
    pub const fn new() -> Self {
        BadDataGuard { consecutive: 1 }
    }

    /// A new converter call starts; `has_output` = frames are already available for its output (left over from the
    /// previous call).
    pub fn begin_call(&mut self, has_output: bool) {
        self.consecutive = if has_output { 0 } else { 1 };
    }

    /// One more failed step; true when the call must be aborted.
    pub fn failure(&mut self) -> bool {
        self.consecutive += 1;
        self.consecutive >= Self::LIMIT
    }

    /// A `ProduceOutputPackets` call failed; `Err(STATUS_BAD_DATA)` when the call must be aborted.
    pub fn produce_failed(&mut self) -> Result<(), i32> {
        if self.failure() { Err(STATUS_BAD_DATA) } else { Ok(()) }
    }

    /// A `ProduceOutputPackets` call delivered `frames` frames (0 = "no output": the counter is untouched).
    pub fn produced(&mut self, frames: usize) {
        if frames > 0 {
            self.consecutive = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(valid: i64, priming: i32) -> Option<PacketTableInfo> {
        Some(PacketTableInfo { valid_frames: valid, priming, remainder: 0 })
    }

    #[test]
    fn lame_like_parameters() {
        let g = Gapless::from_table(info(66150, 576));
        assert_eq!((g.priming(), g.total_frames()), (576, 66150));
    }

    #[test]
    fn no_table_means_no_trim() {
        let g = Gapless::from_table(None);
        assert_eq!((g.priming(), g.total_frames()), (0, 0));
    }

    #[test]
    fn zero_valid_means_no_clamp_negative_means_nothing() {
        assert_eq!(Gapless::from_table(info(0, 100)).total_frames(), 0);
        // (long)(-1818 + 0.5) truncates toward zero
        assert_eq!(Gapless::from_table(info(-1818, 100)).total_frames(), -1817);
        assert_eq!(Gapless::from_table(info(-1, 0)).total_frames(), 0);
    }

    #[test]
    fn guard_tolerates_four_failures_after_output_and_three_in_a_call_without_output() {
        let mut g = BadDataGuard::new();
        assert!(g.produce_failed().is_ok() && g.produce_failed().is_ok() && g.produce_failed().is_ok());
        assert_eq!(g.produce_failed(), Err(STATUS_BAD_DATA));
        let mut g = BadDataGuard::new();
        g.produced(1152);
        for _ in 0..4 {
            assert!(g.produce_failed().is_ok());
        }
        assert_eq!(g.produce_failed(), Err(STATUS_BAD_DATA));
        // a call that starts with frames left over behaves like one that already produced
        let mut g = BadDataGuard::new();
        g.begin_call(true);
        for _ in 0..4 {
            assert!(g.produce_failed().is_ok());
        }
        assert_eq!(g.produce_failed(), Err(STATUS_BAD_DATA));
        g.begin_call(false);
        g.produced(0); // a produce without frames does not reset the run
        assert!(g.produce_failed().is_ok() && g.produce_failed().is_ok() && g.produce_failed().is_ok());
        assert_eq!(g.produce_failed(), Err(STATUS_BAD_DATA));
    }
}
