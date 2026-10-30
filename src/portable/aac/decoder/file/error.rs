//! Error taxonomy.  Every variant documents the Apple `OSStatus` it corresponds to.
//!
//! Apple's call sequence is `ExtAudioFileOpenURL` (-> [`OpenError`]) -> `kExtAudioFileProperty_ClientDataFormat` (-> [`ClientError`]) ->
//! `ExtAudioFileRead` (-> [`ReadError`]).

use std::fmt;

/// Four-character-code of `s`: big-endian `u32`.
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

/// `'dta?'` `kAudioFileInvalidFileError`.
pub const STATUS_INVALID_FILE: i32 = fourcc(b"dta?") as i32;
/// `'typ?'` `kAudioFileUnsupportedFileTypeError`.
pub const STATUS_UNSUPPORTED_FILE_TYPE: i32 = fourcc(b"typ?") as i32;
/// `'chk?'` `kAudioFileInvalidChunkError` (CAF without `data`, frames per packet 0, packet count beyond the packet table).
pub const STATUS_INVALID_CHUNK: i32 = fourcc(b"chk?") as i32;
/// `'wht?'` `kAudioFileUnspecifiedError`.
pub const STATUS_UNSPECIFIED: i32 = fourcc(b"wht?") as i32;
/// `'fmt?'` `kAudioFileUnsupportedDataFormatError` / `kAudioConverterErr_FormatNotSupported`.
pub const STATUS_FORMAT_NOT_SUPPORTED: i32 = fourcc(b"fmt?") as i32;
/// `'sync'` (ADTS/LOAS file whose first bytes are not a sync word; only seen with an `.aac` style name hint).
pub const STATUS_SYNC: i32 = fourcc(b"sync") as i32;
/// `'layr'` (ADTS name hint with an MPEG audio frame header, i.e. a sync word with a non-zero layer, where the ADTS sync is expected).
pub const STATUS_LAYER: i32 = fourcc(b"layr") as i32;
/// `'prof'` (ADTS header of MPEG-2 `ID` with the reserved profile 3).
pub const STATUS_PROFILE: i32 = fourcc(b"prof") as i32;
/// `'rate'` (ADTS header with a reserved sampling_frequency_index 13..15).
pub const STATUS_RATE: i32 = fourcc(b"rate") as i32;
/// `'!dat'` `kAudioCodecUnsupportedFormatError`: "no decoder can be built" (ADTS channel_configuration 0 without in-band PCE, coupling channels in a
/// PCE, planar-stereo mixdown of AAC_6_0).
pub const STATUS_NO_DECODER: i32 = fourcc(b"!dat") as i32;
/// `'bada'` `kAudioCodecBadDataError`: the access units do not match the declared decoder configuration.
pub const STATUS_BAD_DATA: i32 = fourcc(b"bada") as i32;
/// `'pck?'` `kAudioFileInvalidPacketOffsetError`.
pub const STATUS_INVALID_PACKET_OFFSET: i32 = fourcc(b"pck?") as i32;
/// `paramErr`: access unit larger than `1024 * channels` bytes, or bytes of the access unit lie beyond the end of the file.
pub const STATUS_PARAM_ERR: i32 = -50;
/// `kExtAudioFileError_MaxPacketSizeUnknown`: first `ExtAudioFileRead` on a stream with zero packets.
pub const STATUS_MAX_PACKET_SIZE_UNKNOWN: i32 = -66567;

/// Why `ExtAudioFileOpenURL` fails with `'wht?'`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unspecified {
    /// a required sample-table atom (`stsd`, `stsz`, `stco`/`co64`, `mdhd`) is missing, or `stsz` declares more samples than it holds
    BadStructure,
    /// `esds` missing, malformed, no DecoderSpecificInfo (empty ASC), `objectTypeIndication` not 0x40/0x67, `streamType` not audio,
    /// or OCR stream flag set
    BadEsds,
    /// AudioSpecificConfig unreadable (shorter than its fixed fields)
    BadAsc,
    /// `audioObjectType` that Apple's file layer does not open at all (anything except 1,2,3,4,23,32,33,39,42)
    UnsupportedAot,
    /// sampling_frequency_index 13/14 (reserved), or explicit rate 0 / above 3,000,000 Hz
    BadSampleRate,
    /// channel_configuration 8, 9, 10, 13, 15 or 0 without a program_config_element
    BadChannelConfig,
    /// PCE whose element shape has no CoreAudio layout tag (2.1, 2 LFE, side-5.1, ffmpeg 7.0, ...) or whose sampling_frequency_index differs
    /// from the ASC's
    UnmappedPceLayout,
    /// the file is shorter than the sum of its access-unit sizes
    FileShorterThanAudio,
    /// ADTS/LOAS file without any complete frame
    NoFrames,
    /// SBR / PS signalling whose extension rate is neither the core rate nor twice it (or is a reserved index)
    BadSbrRate,
}

/// `ExtAudioFileOpenURL` failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// zero-byte file (any extension): `'wht?'`
    EmptyFile,
    /// no recognised container: `'typ?'` (Matroska, a file without `ftyp` first, garbage)
    UnsupportedFileType,
    /// structurally invalid file: `'dta?'` (`ftyp` without a complete `moov`, no `soun` track, `mdat` with size 0)
    InvalidFile,
    /// ADTS/LOAS extension hint but no sync word at byte 0 (after an ID3v2 prefix): `'sync'`
    NoSync,
    /// ADTS name hint, and the first bytes are the sync word of an MPEG audio frame (layer bits not 00): `'layr'`
    WrongLayer,
    /// CAF: no `data` chunk, `desc` frames per packet 0, or a `pakt` packet count larger than its size table: `'chk?'`
    InvalidChunk,
    /// `'wht?'`
    Unspecified(Unspecified),
    /// The first sound track of the MP4 file has a sample entry that is not `mp4a`.  AudioToolbox answers `'wht?'` for every type but the ones it
    /// opens as other codecs (`alac`, `ac-3`, `ec-3`, `drms`; `samr` and `sawb` fail the client format); those are the other readers' business.
    NotMp4a([u8; 4]),
    /// A CAF `desc` that declares a sample rate of 0: AudioToolbox opens it, the consumer of the file format cannot use it.  NOT an Apple status.
    ZeroSampleRate,
    /// The packet table holds more than [`crate::portable::aac::decoder::file::mp4::MAX_PACKETS`] access units: not an Apple status, the file is
    /// refused (`os_status` is never reported for it).
    PacketTableTooLarge,
    /// `'fmt?'`: ADTS header profile != LC, sampling_frequency_index 12, first-access-unit layout inconsistent with the header
    UnsupportedDataFormat,
    /// `'rate'`: ADTS sampling_frequency_index 13..15
    UnsupportedRate,
    /// `'!dat'`: ADTS channel_configuration 0 without a PCE in the first access unit
    NoDecoder,
    /// `'prof'`: ADTS (MPEG-2 `ID`) header with the reserved profile 3
    UnsupportedProfile,
    /// A child of the movie atom whose size field is 0 or 2..=7 (no atom, whatever the atom is): `-1` when it is the first child, `-2` behind another.
    BadMovieChild { first: bool },
}

impl OpenError {
    pub fn os_status(&self) -> i32 {
        match self {
            OpenError::InvalidFile => STATUS_INVALID_FILE,
            OpenError::EmptyFile => STATUS_UNSPECIFIED,
            OpenError::UnsupportedFileType => STATUS_UNSUPPORTED_FILE_TYPE,
            OpenError::NoSync => STATUS_SYNC,
            OpenError::WrongLayer => STATUS_LAYER,
            OpenError::InvalidChunk => STATUS_INVALID_CHUNK,
            OpenError::Unspecified(_) | OpenError::NotMp4a(_) | OpenError::PacketTableTooLarge | OpenError::ZeroSampleRate => STATUS_UNSPECIFIED,
            OpenError::UnsupportedDataFormat => STATUS_FORMAT_NOT_SUPPORTED,
            OpenError::UnsupportedRate => STATUS_RATE,
            OpenError::NoDecoder => STATUS_NO_DECODER,
            OpenError::UnsupportedProfile => STATUS_PROFILE,
            OpenError::BadMovieChild { first: true } => -1,
            OpenError::BadMovieChild { first: false } => -2,
        }
    }
}

/// `kExtAudioFileProperty_ClientDataFormat` failure (Apple opened the file, but no decoder/converter for the requested client format).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// `'fmt?'`: the file format id is not `'aac '` (Main `aacm`, SSR `aacr`, LTP `aact`, LD `aacl`, ELD `aace`, HE `aach`/`aacp`, USAC, MPEG-1 audio)
    NotAacLc,
    /// `'fmt?'`: `frameLengthFlag == 1` (960-sample frames)
    FrameLength960,
    /// `'fmt?'`: 7350 Hz (no client format exists), or a rate from 110852 Hz up to 2,999,999 Hz
    UnsupportedRate,
    /// `paramErr` (-50): an explicit ASC rate of exactly 3,000,000 Hz (3,000,001 and above is refused at open)
    InvalidRate,
    /// `'!dat'`: coupling channel elements in the PCE; or an AAC_6_0 source with a stereo client (no downmix matrix)
    NoDecoder,
    /// `paramErr` (-50): the file's `chan` layout has more channels than the stream (CAF)
    InvalidLayout,
    /// `'fmt?'`: more than 8 channels
    UnsupportedChannelCount,
    /// `'fmt?'`: a CAF `desc` names HE-AAC v2, AAC-LD or AAC-ELD where the cookie describes another format (or there is none)
    FormatMismatch,
    /// `paramErr` (-50): a CAF `desc` of a million frames per packet or more
    InvalidPacketFrames,
    /// `'pck?'`: a CAF `desc` without frames per packet and without a packet table: the largest packet cannot be told
    NoPacketTable,
    /// the CAF `desc` sample rate differs from the stream's (the ASC's): AudioToolbox decodes it on another path.  NOT an Apple status.
    RateMismatch,
    /// a CAF layered description (`ldsc`) names a format other than HE-AAC and differs from `desc`: Apple presents the layer's format and rate (and scales the
    /// file length by the ratio), which is not reproduced.  NOT an Apple status.
    LayeredFormat,
    /// the client channel count / interleaving combination is not one of the verified ones (identity, or 2 channels from a source with a
    /// verified stereo downmix).  NOT an Apple status: Apple accepts every 1..8 channel interleaved and 1/2/3/6/8 plane client
    /// (`e7`) but only the verified combinations are reproduced bit-exactly.
    UnverifiedConversion,
}

impl ClientError {
    pub fn os_status(&self) -> Option<i32> {
        match self {
            ClientError::NotAacLc
            | ClientError::FrameLength960
            | ClientError::FormatMismatch
            | ClientError::UnsupportedRate => Some(STATUS_FORMAT_NOT_SUPPORTED),
            ClientError::InvalidRate | ClientError::InvalidLayout | ClientError::InvalidPacketFrames => Some(STATUS_PARAM_ERR),
            ClientError::NoPacketTable => Some(STATUS_INVALID_PACKET_OFFSET),
            ClientError::UnsupportedChannelCount => Some(STATUS_FORMAT_NOT_SUPPORTED),
            ClientError::NoDecoder => Some(STATUS_NO_DECODER),
            ClientError::UnverifiedConversion | ClientError::RateMismatch | ClientError::LayeredFormat => None,
        }
    }
}

/// `ExtAudioFileRead` failure.  A failing call delivers no frames (the frames decoded inside that call are lost).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// zero packets: `-66567` (`kExtAudioFileError_MaxPacketSizeUnknown`)
    NoPackets,
    /// access unit larger than `1024 * channels` bytes: `-50`
    PacketTooLarge,
    /// the access unit lies (partly) beyond the end of the file: `-50`
    PacketBeyondEof,
    /// the 4th consecutive access unit failed to decode inside the call: `'bada'` (`kAudioCodecBadDataError`)
    BadData,
    /// ADTS file with a multi-block frame: the open succeeded with an empty packet table, the first read fails `'dta?'`
    InvalidFile,
    /// the sample table has no sample-to-chunk map, the packet cannot be located: `'pck?'` (`kAudioFileInvalidPacketOffsetError`)
    InvalidPacketOffset,
    /// the converter has no client format: a planar client format that was refused leaves the handle without one, every read fails `-50`
    NoClientFormat,
    /// a LOAS frame behind the first that cannot be read as an access unit (another StreamMuxConfig than the first one's, a damaged one, a payload longer than
    /// the frame, ...): the converter cannot be reconfigured, `'fmt?'`
    BadFrame,
}

impl ReadError {
    pub fn os_status(&self) -> i32 {
        match self {
            ReadError::NoPackets => STATUS_MAX_PACKET_SIZE_UNKNOWN,
            ReadError::PacketTooLarge | ReadError::PacketBeyondEof => STATUS_PARAM_ERR,
            ReadError::BadData => STATUS_BAD_DATA,
            ReadError::InvalidFile => STATUS_INVALID_FILE,
            ReadError::InvalidPacketOffset => STATUS_INVALID_PACKET_OFFSET,
            ReadError::NoClientFormat => STATUS_PARAM_ERR,
            ReadError::BadFrame => STATUS_FORMAT_NOT_SUPPORTED,
        }
    }
}

macro_rules! impl_display {
    ($t:ty) => {
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{self:?}")
            }
        }
        impl std::error::Error for $t {}
    };
}
impl_display!(OpenError);
impl_display!(ClientError);
impl_display!(ReadError);
