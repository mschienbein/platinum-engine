//! Frame accounting: priming discard, valid-frame trim, `FileLengthFrames` ([`TrimPlan`]).
//!
//! Notation: `T` = packets x frames_per_packet (frames the decoder produces; what a bare
//! `AudioConverter` returns), `P` = priming, `V` = `FileLengthFrames`.  `ExtAudioFileRead` returns `decoded[skip .. skip + min(V, T - skip))`
//! with `skip = min(P, T)`, independent of the read-call chunking.

/// Where `P` / `V` came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimSource {
    /// no usable metadata: `P = min(2112, T)`, remainder 0, `V = T - P`
    Default,
    /// iTunSMPB words (read for the `m4af` / `m4bf` / QuickTime file types only, see `FileType::reads_itunsmpb`)
    ITunSmpb,
    /// first non-empty edit-list entry, honoured only with a negative `roll` sample group (every file type)
    EditList,
    /// CAF `pakt` header
    CafPacketTable,
    /// ADTS / LOAS: no packet table (`'pty?'`), no trimming, `V = T`
    None,
}

/// Default encoder delay Apple assumes when a file carries no priming metadata.
pub const DEFAULT_PRIMING: u64 = 2112;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrimPlan {
    pub source: TrimSource,
    pub packets: u64,
    pub frames_per_packet: u32,
    /// `T`
    pub decoded_frames: u64,
    /// `mPrimingFrames` as Apple reports it (NOT clamped for iTunSMPB: may exceed `T`)
    pub priming: u64,
    /// `mRemainderFrames` as reported
    pub remainder: u64,
    /// `mNumberValidFrames` = `FileLengthFrames`; for iTunSMPB it may exceed `T - skip`
    pub valid_frames: u64,
    /// `false` for ADTS / LOAS: `PacketTableInfo` is unavailable (`'pty?'`)
    pub has_packet_table: bool,
    /// CAF `mNumberValidFrames == 0`: reported as 0, but reads deliver everything after the priming
    pub valid_unknown: bool,
}

/// A valid-frame count as AudioToolbox reports it (`FileLengthFrames`): through a double and half a frame, `int64(double(v) + 0.5)`.  Exact below 2^52; from
/// there odd counts round up to the even number above them (`v + 0.5` is a tie between two neighbours), from 2^53 the double itself rounds to even; saturated at
/// 2^63 - 1 (macOS 27.2: iTunSMPB and CAF packet-table counts alike).
pub(crate) fn through_a_double(v: u64) -> u64 {
    ((v as f64) + 0.5) as i64 as u64
}

impl TrimPlan {
    /// Decoded frames discarded at the start: `min(P, T)`.
    pub fn skip_frames(&self) -> u64 {
        self.priming.min(self.decoded_frames)
    }
    /// `kExtAudioFileProperty_FileLengthFrames`.
    pub fn file_length_frames(&self) -> u64 {
        self.valid_frames
    }

    pub(crate) fn default_with(packets: u64, fpp: u32, priming: u64) -> TrimPlan {
        let t = packets * u64::from(fpp);
        let p = priming.min(t);
        TrimPlan { source: TrimSource::Default, packets, frames_per_packet: fpp, decoded_frames: t, priming: p, remainder: 0, valid_frames: t - p, has_packet_table: true, valid_unknown: false }
    }
    pub(crate) fn untrimmed(packets: u64, fpp: u32) -> TrimPlan {
        let t = packets * u64::from(fpp);
        TrimPlan { source: TrimSource::None, packets, frames_per_packet: fpp, decoded_frames: t, priming: 0, remainder: 0, valid_frames: t, has_packet_table: false, valid_unknown: false }
    }
}
