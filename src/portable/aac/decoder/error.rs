//! Errors of the decoder, one variant per stage of Apple's call sequence (`ExtAudioFileOpenURL` -> `ClientDataFormat` -> `ExtAudioFileRead`), with
//! the `OSStatus` each reproduces (documented in `decoder::file::error`), and the typed refusals for what Apple decodes but this port does not.

use crate::portable::aac::decoder::bits::ConfigError;
use crate::portable::aac::decoder::file::{AacFile, ClientError, FormatId, OpenError, ReadError};
use std::fmt;

/// A stream Apple's AudioToolbox decodes (or reads in a way) that this port does not reproduce: refused instead of decoded differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// HE-AAC v1 / v2 (`'aach'` / `'aacp'`: SBR / PS signalled in the AudioSpecificConfig or by the container).
    HeAac,
    /// AAC-LD (`'aacl'`) and AAC-ELD (`'aace'`).
    LowDelayAac,
    /// MPEG-1 audio layer I / II object types inside an MP4 / CAF (`'.mp1'` / `'.mp2'`), and USAC.
    Mpeg1AudioOrUsac,
    /// The container declares fewer channels than the AudioSpecificConfig (Apple down-mixes inside the decoder).
    ChannelCountMismatch,
    /// The CAF `desc` sample rate differs from the AudioSpecificConfig's, or is fractional (Apple takes another path).
    RateMismatch,
    /// An in-band program config element with a matrix mix-down index that does not describe the stream's own 5.1 layout, in a 5.1 stream read with the stereo client.
    InBandPce,
    /// A CAF layered description (`ldsc`) whose first layer is no HE-AAC and differs from `desc` (rate, frames per packet, channels, format).
    LayeredFormat,
    /// A client-format change after the first read.
    ClientFormatChangeAfterRead,
    /// The decoder configuration cannot be built although the container was accepted.
    Configuration,
    /// A multi-block ADTS frame failed a planar read of a source whose stereo conversion is a channel selection (1, 3, 4 or 5 channels): what Apple
    /// makes of that error after an earlier failure is not reproduced.
    MultiBlockAdtsPlanar,
    /// A failure inside the priming skip of a stereo-converted read of a 1, 3, 4 or 5 channel stream (the converter loses it and goes on): what the
    /// calls after it see is not reproduced.
    PrimingFailureSwallowed,
    /// The packet table holds more packets than the decoder admits (a hostile or corrupt file).
    PacketTableTooLarge,
}

impl Unsupported {
    /// The refusal text for [`crate::portable::audio::CodecError::UnsupportedCodec`].
    pub fn reason(self) -> &'static str {
        match self {
            Self::HeAac => "HE-AAC (SBR/PS) decoding is not reproduced",
            Self::LowDelayAac => "AAC-LD and AAC-ELD decoding are not reproduced",
            Self::Mpeg1AudioOrUsac => "MPEG-1 audio and USAC object types inside an AAC container are not reproduced",
            Self::ChannelCountMismatch => "a file whose channel count differs from its AudioSpecificConfig is decoded by AudioToolbox with a down-mix that is not reproduced",
            Self::RateMismatch => "a CAF whose sample rate is not the stream's (or not a whole number of Hz) takes an AudioToolbox path that is not reproduced",
            Self::InBandPce => "an in-band program config element that does not describe the 5.1 layout of the stream sets AudioToolbox's stereo mix-down in a way that is not reproduced",
            Self::LayeredFormat => "a CAF layered description that names another format than its desc (other than HE-AAC) is presented by AudioToolbox with the layer's rate and a rescaled length, which is not reproduced",
            Self::ClientFormatChangeAfterRead => "changing the client format after a read is not reproduced",
            Self::Configuration => "the AAC decoder configuration cannot be built",
            Self::MultiBlockAdtsPlanar => "a multi-block ADTS frame in a stereo-converted read of a 1, 3, 4 or 5 channel stream is not reproduced",
            Self::PrimingFailureSwallowed => "a failure inside the priming skip of a stereo-converted read of a 1, 3, 4 or 5 channel stream is not reproduced",
            Self::PacketTableTooLarge => "the packet table of the file is too large",
        }
    }

    /// What Apple decodes of a stream that is not AAC-LC, `None` where Apple itself refuses it at the client format (`'fmt?'`).
    pub(crate) fn of_format(format: FormatId) -> Option<Self> {
        match format {
            FormatId::AacHe | FormatId::AacHeV2 => Some(Self::HeAac),
            FormatId::AacLd | FormatId::AacEld => Some(Self::LowDelayAac),
            FormatId::Usac | FormatId::Mpeg1Layer1 | FormatId::Mpeg1Layer2 => Some(Self::Mpeg1AudioOrUsac),
            FormatId::Aac | FormatId::AacMain | FormatId::AacSsr | FormatId::AacLtp => None,
        }
    }
}

/// Why a file could not be opened or its client format not be set.
#[derive(Clone, Debug, PartialEq)]
pub enum DecodeError {
    /// `ExtAudioFileOpenURL` failed (`'wht?' 'typ?' 'dta?' 'sync' 'chk?' 'fmt?' 'rate' '!dat'`).
    Open(OpenError),
    /// `kExtAudioFileProperty_ClientDataFormat` failed (`'fmt?'`, `'!dat'`, -50).
    Client(ClientError),
    /// Apple decodes the stream; this port refuses it.
    Unsupported(Unsupported),
    /// The file format declares a sample rate of 0 (a CAF `desc`): no client format can be derived from it.
    InvalidSampleRate,
    /// The CAF `desc` sample rate differs from the stream's: AudioToolbox converts the rate inside the decoder.  Carries the `desc` rate, which the
    /// caller compares with the client rate before it refuses.
    RateMismatch { file_rate: f64 },
    /// `decoder::bits` cannot build the element list although the file layer accepted the stream (an inconsistency between the modules; not an Apple
    /// status).
    Config(ConfigError),
}

impl DecodeError {
    /// The client-format failure of `file`: Apple's status, or a refusal where Apple would decode.
    pub(crate) fn client(file: &AacFile, error: ClientError) -> Self {
        match error {
            ClientError::NotAacLc => Unsupported::of_format(file.format_id()).map_or(DecodeError::Client(error), DecodeError::Unsupported),
            ClientError::UnverifiedConversion => DecodeError::Unsupported(Unsupported::ChannelCountMismatch),
            ClientError::RateMismatch => DecodeError::RateMismatch { file_rate: file.sample_rate_hz() },
            other => DecodeError::Client(other),
        }
    }
}

impl From<OpenError> for DecodeError {
    fn from(e: OpenError) -> Self {
        match e {
            OpenError::PacketTableTooLarge => DecodeError::Unsupported(Unsupported::PacketTableTooLarge),
            OpenError::ZeroSampleRate => DecodeError::InvalidSampleRate,
            e => DecodeError::Open(e),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Open(e) => write!(f, "open: {e:?} ({})", status_text(e.os_status())),
            DecodeError::Client(e) => write!(f, "client format: {e:?} ({})", status_text(e.os_status().unwrap_or(0))),
            DecodeError::Unsupported(u) => f.write_str(u.reason()),
            DecodeError::InvalidSampleRate => f.write_str("the file declares a sample rate of 0"),
            DecodeError::RateMismatch { .. } => f.write_str(Unsupported::RateMismatch.reason()),
            DecodeError::Config(e) => write!(f, "decoder configuration: {e:?}"),
        }
    }
}
impl std::error::Error for DecodeError {}

/// Why an `ExtAudioFileRead` call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadFailure {
    /// Apple's own failure (-50, -66567, `'bada'`, `'dta?'`, `'pck?'`).
    Apple(ReadError),
    /// Apple's result for this call is not reproduced; the reason is the refusal text.
    NotReproduced(Unsupported),
}

impl From<ReadError> for ReadFailure {
    fn from(e: ReadError) -> Self {
        ReadFailure::Apple(e)
    }
}

impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadFailure::Apple(e) => write!(f, "read: {e:?} ({})", status_text(e.os_status())),
            ReadFailure::NotReproduced(u) => f.write_str(u.reason()),
        }
    }
}
impl std::error::Error for ReadFailure {}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}
impl std::error::Error for Unsupported {}

/// `-50`, or the four characters of a fourcc status.
pub fn status_text(s: i32) -> String {
    let b = (s as u32).to_be_bytes();
    if b.iter().all(|c| (32..127).contains(c)) {
        format!("'{}' {s}", String::from_utf8_lossy(&b))
    } else {
        format!("{s}")
    }
}
