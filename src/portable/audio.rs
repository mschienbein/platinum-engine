//! Portable audio I/O for the subset whose AudioToolbox behavior is established.
//!
//! No resampling is substituted for AudioConverter: a differing client rate
//! returns a typed error. Decoding is outside the engine's scoped FPCR.FZ DSP
//! callbacks. This module does not call platform audio or math libraries.
//!
//! MP3, MP2 and MP1 elementary streams are read by [`mp3`], the bit-exact
//! port of AudioToolbox's MPEG audio path (container, priming trim, decoder and
//! read errors); like every other codec here it needs no opt-in.
//!
//! AAC output is written by [`Writer`] with the encoder of [`super::aac`]: the file AudioToolbox's `ExtAudioFile` writes for the engine's call sequence,
//! byte for byte except for the close-time clock, for the formats AudioToolbox takes at 256 kbit/s at their exact sample rates. Every other rate and
//! channel count is answered as the macOS writer answers it (a native status, or the `AacBitRateNotSet` warning), and what AudioToolbox serves but
//! this build does not reproduce (rates that only round to an AAC rate, fractional rates) is refused with a typed error; see [`super::aac::encoder`].
//!
//! AAC-LC in MP4/M4A, ADTS, LOAS/LATM and CAF is read by [`aac_read`] on top of the
//! exact decoder of [`crate::portable::aac::decoder`], with the same contract: the
//! samples, the frames written behind a short read, the statuses of failing opens
//! and reads, and `file_length_frames` / `bitrate` are AudioToolbox's; streams it
//! decodes that the port does not reproduce (HE-AAC, AAC-LD/ELD, ...) are typed
//! refusals.

#[path = "audio/alac.rs"]
pub mod alac;
#[path = "audio/gsm.rs"]
pub mod gsm;
#[path = "audio/mp4.rs"]
pub mod mp4;
#[path = "audio/mp3/mod.rs"]
pub mod mp3;
#[path = "audio/aac_read.rs"]
mod aac_read;

use std::error::Error;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::aac;

const LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const ALAC: u32 = u32::from_be_bytes(*b"alac");
const FLAC: u32 = u32::from_be_bytes(*b"flac");

/// Portable copy of AudioStreamBasicDescription.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StreamDescription {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

/// The recorder depth selected by the controller for a source ASBD.
pub fn recorder_bit_depth(source: &StreamDescription) -> u32 {
    if !matches!(source.format_id, LPCM | ALAC | FLAC) { return 0; }
    match source.bits_per_channel { 0..=16 => 16, 17..=24 => 24, _ => 32 }
}

/// Errors which distinguish Apple-only codecs and unavailable rate conversion
/// from malformed or otherwise unsupported containers.
#[derive(Debug)]
pub enum CodecError {
    InvalidInput(&'static str),
    UnsupportedFormat(&'static str),
    UnsupportedCodec { codec: &'static str, operation: &'static str, reason: &'static str },
    UnsupportedSampleRateConversion { source_rate: f64, client_rate: f64 },
    Io { operation: &'static str, path: PathBuf, source: io::Error },
    /// A reproduced AudioToolbox error, not a call to the native framework.
    Native { operation: &'static str, status: i32 },
    AlacDecode(alac::DecodeError),
    AlacEncode(alac::EncodeError),
    Mp4Read(mp4::Mp4Error),
    Mp4Write(mp4::WriterError),
    /// The AAC encoder failed on a block of input.
    AacEncode(aac::encoder::EncodeError),
    /// The AAC `.m4a` container cannot hold the stream (more than 32 bits of offsets or duration) or was given inconsistent parameters.
    AacContainer(aac::mp4::Mp4Error),
    Cleanup { primary: Box<CodecError>, source: io::Error },
    PublishedButCleanupFailed { path: PathBuf, source: io::Error },
    Poisoned,
}
impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid audio codec input: {message}"),
            Self::UnsupportedFormat(message) => write!(f, "unsupported audio container or format: {message}"),
            Self::UnsupportedCodec { codec, operation, reason } => write!(f, "{codec} {operation} is unavailable: {reason}"),
            Self::UnsupportedSampleRateConversion { source_rate, client_rate } => write!(f,
                "AudioToolbox sample-rate conversion from {source_rate} Hz to {client_rate} Hz is not reproduced on this platform"),
            Self::Io { operation, path, source } => write!(f, "{operation} {}: {source}", path.display()),
            Self::Native { operation, status } => write!(f, "{operation} failed with OSStatus {status} ({:#010x})", *status as u32),
            Self::AlacDecode(error) => write!(f, "ALAC decode: {error}"),
            Self::AlacEncode(error) => write!(f, "ALAC encode: {error}"),
            Self::Mp4Read(error) => write!(f, "ALAC container: {error}"),
            Self::Mp4Write(error) => write!(f, "ALAC container output: {error}"),
            Self::AacEncode(error) => write!(f, "AAC encode: {error}"),
            Self::AacContainer(error) => write!(f, "AAC container output: {error}"),
            Self::Cleanup { primary, source } => write!(f, "{primary}; additionally, cleanup failed: {source}"),
            Self::PublishedButCleanupFailed { path, source } => write!(f, "{} was published, but temporary cleanup failed: {source}", path.display()),
            Self::Poisoned => f.write_str("audio stream is unusable after an earlier codec failure"),
        }
    }
}
impl Error for CodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Cleanup { source, .. } | Self::PublishedButCleanupFailed { source, .. } => Some(source),
            Self::AlacDecode(source) => Some(source),
            Self::AlacEncode(source) => Some(source),
            Self::Mp4Read(source) => Some(source),
            Self::Mp4Write(source) => Some(source),
            Self::AacEncode(source) => Some(source),
            Self::AacContainer(source) => Some(source),
            _ => None,
        }
    }
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> CodecError {
    CodecError::Io { operation, path: path.to_owned(), source }
}

/// Convert one planar libFLAC i32 channel to the float32 values produced by
/// the reference's same-rate AudioConverter. The low `bits/8` bytes are used,
/// matching its FLAC callback. AudioConverterNew accepted 8/16/24/32 and
/// rejected every other tested depth from 8 through 32; 1/2/6-channel probes
/// and their generated samples are in `audio-01/flac-integer-conversion`.
pub fn convert_flac_pcm(input: &[i32], bit_depth: u32, output: &mut [f32]) -> Result<(), CodecError> {
    if input.len() != output.len() { return Err(CodecError::InvalidInput("FLAC plane lengths differ")); }
    match bit_depth {
        8 => for (dst, &sample) in output.iter_mut().zip(input) { *dst = sample as i8 as f32 * (1.0 / 128.0); },
        16 => for (dst, &sample) in output.iter_mut().zip(input) { *dst = sample as i16 as f32 * (1.0 / 32_768.0); },
        24 => for (dst, &sample) in output.iter_mut().zip(input) { *dst = (sample << 8 >> 8) as f32 * (1.0 / 8_388_608.0); },
        32 => for (dst, &sample) in output.iter_mut().zip(input) { *dst = sample as f32 * (1.0 / 2_147_483_648.0); },
        _ => return Err(CodecError::UnsupportedFormat("FLAC integer depth is not supported by the reference AudioConverter")),
    }
    Ok(())
}

fn same_rate(source: f64, requested: Option<f64>) -> Result<f64, CodecError> {
    let requested = requested.unwrap_or(source);
    if !requested.is_finite() || requested <= 0.0 {
        return Err(CodecError::InvalidInput("sample rate must be positive and finite"));
    }
    if requested != source {
        return Err(CodecError::UnsupportedSampleRateConversion { source_rate: source, client_rate: requested });
    }
    Ok(requested)
}

fn client_format(rate: f64, channels: u32, planar: bool) -> Result<StreamDescription, CodecError> {
    let bytes = channels.checked_mul(4).filter(|n| *n != 0).ok_or(CodecError::InvalidInput("invalid channel count"))?;
    Ok(StreamDescription { sample_rate: rate, format_id: LPCM, format_flags: if planar { 0x29 } else { 9 },
        bytes_per_packet: if planar { 4 } else { bytes }, frames_per_packet: 1,
        bytes_per_frame: if planar { 4 } else { bytes }, channels_per_frame: channels,
        bits_per_channel: 32, reserved: 0 })
}

#[derive(Clone, Copy)]
enum Encoding {
    Unsigned8,
    MuLaw,
    ALaw,
    Ima4,
    WavIma,
    WavMs,
    WavGsm,
    Alac,
    Signed { bits: u32, big_endian: bool },
    Float32 { big_endian: bool },
    Float64 { big_endian: bool },
}
impl Encoding {
    fn width(self) -> usize {
        match self {
            Self::Unsigned8 | Self::MuLaw | Self::ALaw => 1,
            Self::Ima4 | Self::WavIma | Self::WavMs | Self::WavGsm | Self::Alac => 0,
            Self::Signed { bits, .. } => bits.div_ceil(8) as usize,
            Self::Float32 { .. } => 4,
            Self::Float64 { .. } => 8,
        }
    }
    fn decode(self, bytes: &[u8]) -> f32 {
        match self {
            Self::Unsigned8 => (i32::from(bytes[0]) - 128) as f32 * (1.0 / 128.0),
            Self::MuLaw => {
                let code = !bytes[0];
                let magnitude = ((i32::from(code & 15) << 3) + 132) << ((code >> 4) & 7);
                let sample = if code & 0x80 != 0 { 132 - magnitude } else { magnitude - 132 };
                sample as f32 * (1.0 / 32_768.0)
            }
            Self::ALaw => {
                let code = bytes[0] ^ 0x55;
                let base = i32::from(code & 15) << 4;
                let segment = (code >> 4) & 7;
                let magnitude = match segment { 0 => base + 8, 1 => base + 264,
                    _ => (base + 264) << (segment - 1) };
                let sample = if code & 0x80 != 0 { magnitude } else { -magnitude };
                sample as f32 * (1.0 / 32_768.0)
            }
            Self::Ima4 | Self::WavIma | Self::WavMs | Self::WavGsm | Self::Alac => unreachable!("compressed audio is decoded by packet"),
            Self::Signed { big_endian, .. } => {
                let mut raw = 0u32;
                if big_endian { for &byte in bytes { raw = (raw << 8) | u32::from(byte); } }
                else { for (i, &byte) in bytes.iter().enumerate() { raw |= u32::from(byte) << (i * 8); } }
                let container_bits = bytes.len() as u32 * 8;
                let shift = 32 - container_bits;
                let value = ((raw << shift) as i32) >> shift;
                let scale = match container_bits { 8 => 1.0 / 128.0, 16 => 1.0 / 32_768.0,
                    24 => 1.0 / 8_388_608.0, 32 => 1.0 / 2_147_483_648.0, _ => unreachable!() };
                value as f32 * scale
            }
            Self::Float32 { big_endian } => {
                let b: [u8; 4] = bytes.try_into().expect("validated float32 width");
                f32::from_bits(if big_endian { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
            }
            Self::Float64 { big_endian } => {
                let b: [u8; 8] = bytes.try_into().expect("validated float64 width");
                let bits = if big_endian { u64::from_be_bytes(b) } else { u64::from_le_bytes(b) };
                let fraction = bits & 0x000f_ffff_ffff_ffff;
                if bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 && fraction != 0 {
                    // FCVT quiets a double NaN and carries its high payload bits.
                    f32::from_bits(((bits >> 32) as u32 & 0x8000_0000) | 0x7fc0_0000 | (fraction >> 29) as u32)
                } else { f64::from_bits(bits) as f32 }
            }
        }
    }
}

struct ParsedPcm {
    source: StreamDescription,
    encoding: Encoding,
    data_offset: u64,
    frames: u64,
    coefficients: Vec<(i16, i16)>,
}

fn ima_step(predictor: &mut i32, index: &mut i32, code: u8) {
    const STEPS: [i32; 89] = [
        7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
        50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230,
        253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
        1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327,
        3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442,
        11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
    ];
    const CHANGE: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];
    let step = STEPS[*index as usize];
    let mut delta = step >> 3;
    if code & 1 != 0 { delta += step >> 2; }
    if code & 2 != 0 { delta += step >> 1; }
    if code & 4 != 0 { delta += step; }
    *predictor = (*predictor + if code & 8 != 0 { -delta } else { delta }).clamp(-32_768, 32_767);
    *index = (*index + CHANGE[(code & 7) as usize]).clamp(0, 88);
}

struct ImaState {
    packet: [u8; 68],
    samples: [f32; 128],
    position: usize,
}
impl ImaState {
    fn new() -> Self { Self { packet: [0; 68], samples: [0.0; 128], position: 64 } }
    fn decode_packet(&mut self, channels: usize) {
        for channel in 0..channels {
            let packet = &self.packet[channel * 34..channel * 34 + 34];
            let header = u16::from_be_bytes([packet[0], packet[1]]);
            let mut predictor = i32::from(header as i16) & !127;
            let mut index = i32::from(header & 127).min(88);
            for frame in 0..64 {
                let packed = packet[2 + frame / 2];
                let code = if frame % 2 == 0 { packed & 15 } else { packed >> 4 };
                ima_step(&mut predictor, &mut index, code);
                self.samples[frame * channels + channel] = predictor as f32 * (1.0 / 32_768.0);
            }
        }
        self.position = 0;
    }
}

fn ms_adpcm_step(one: &mut i32, two: &mut i32, delta: &mut i32, coeff: (i16, i16), code: u8) -> f32 {
    const ADAPT: [i32; 16] = [230, 230, 230, 230, 307, 409, 512, 614, 768, 614, 512, 409, 307, 230, 230, 230];
    let prediction = (i64::from(*one) * i64::from(coeff.0) + i64::from(*two) * i64::from(coeff.1)) >> 8;
    let signed_code = if code < 8 { i64::from(code) } else { i64::from(code) - 16 };
    let sample = (prediction + signed_code * i64::from(*delta)).clamp(-32_768, 32_767) as i32;
    *two = *one;
    *one = sample;
    *delta = (((ADAPT[code as usize] * *delta) >> 8) as i16 as i32).max(16);
    sample as f32 * (1.0 / 32_768.0)
}

struct WavAdpcmState {
    encoding: Encoding,
    packet: Vec<u8>,
    samples: Vec<f32>,
    coefficients: Vec<(i16, i16)>,
    frames: usize,
    position: usize,
}
impl WavAdpcmState {
    fn new(encoding: Encoding, packet_bytes: usize, frames: usize, channels: usize, coefficients: Vec<(i16, i16)>) -> Self {
        Self { encoding, packet: vec![0; packet_bytes], samples: vec![0.0; frames * channels],
            coefficients, frames, position: frames }
    }
    fn decode_packet(&mut self, channels: usize) -> Result<(), CodecError> {
        match self.encoding {
            Encoding::WavIma => {
                let mut predictors = [0i32; 2];
                let mut indices = [0i32; 2];
                for channel in 0..channels {
                    let start = channel * 4;
                    predictors[channel] = i32::from(i16::from_le_bytes([self.packet[start], self.packet[start + 1]]));
                    indices[channel] = i32::from(self.packet[start + 2]).min(88);
                    self.samples[channel] = predictors[channel] as f32 * (1.0 / 32_768.0);
                }
                for (group, bytes) in self.packet[4 * channels..].chunks_exact(4 * channels).enumerate() {
                    for channel in 0..channels {
                        for (i, &packed) in bytes[channel * 4..channel * 4 + 4].iter().enumerate() {
                            for nibble in 0..2 {
                                let code = if nibble == 0 { packed & 15 } else { packed >> 4 };
                                ima_step(&mut predictors[channel], &mut indices[channel], code);
                                let frame = 1 + group * 8 + i * 2 + nibble;
                                self.samples[frame * channels + channel] = predictors[channel] as f32 * (1.0 / 32_768.0);
                            }
                        }
                    }
                }
            }
            Encoding::WavMs => {
                let mut coeff = [(0i16, 0i16); 2];
                let mut delta = [0i32; 2];
                let mut one = [0i32; 2];
                let mut two = [0i32; 2];
                for channel in 0..channels {
                    coeff[channel] = *self.coefficients.get(self.packet[channel] as usize)
                        .ok_or(CodecError::InvalidInput("MS ADPCM predictor index is out of range"))?;
                }
                let mut offset = channels;
                for values in [&mut delta, &mut one, &mut two] {
                    for value in values.iter_mut().take(channels) {
                        *value = i32::from(i16::from_le_bytes([self.packet[offset], self.packet[offset + 1]]));
                        offset += 2;
                    }
                }
                for channel in 0..channels {
                    self.samples[channel] = two[channel] as f32 * (1.0 / 32_768.0);
                    self.samples[channels + channel] = one[channel] as f32 * (1.0 / 32_768.0);
                }
                for (i, &packed) in self.packet[offset..].iter().enumerate() {
                    if channels == 1 {
                        self.samples[2 + 2 * i] = ms_adpcm_step(&mut one[0], &mut two[0], &mut delta[0], coeff[0], packed >> 4);
                        self.samples[3 + 2 * i] = ms_adpcm_step(&mut one[0], &mut two[0], &mut delta[0], coeff[0], packed & 15);
                    } else {
                        self.samples[(2 + i) * 2] = ms_adpcm_step(&mut one[0], &mut two[0], &mut delta[0], coeff[0], packed >> 4);
                        self.samples[(2 + i) * 2 + 1] = ms_adpcm_step(&mut one[1], &mut two[1], &mut delta[1], coeff[1], packed & 15);
                    }
                }
            }
            _ => unreachable!("WAVE ADPCM packet state"),
        }
        self.position = 0;
        Ok(())
    }
}

struct GsmState {
    decoder: gsm::GsmDecoder,
    packet: [u8; 65],
    samples: [f32; 320],
    position: usize,
}
impl GsmState {
    fn new() -> Self {
        Self { decoder: gsm::GsmDecoder::new(), packet: [0; 65], samples: [0.0; 320], position: 320 }
    }
}

enum PacketState {
    Ima4(Box<ImaState>),
    Wave(Box<WavAdpcmState>),
    Gsm(Box<GsmState>),
}
impl PacketState {
    fn position(&self) -> usize {
        match self { Self::Ima4(state) => state.position, Self::Wave(state) => state.position, Self::Gsm(state) => state.position }
    }
    fn frames(&self) -> usize {
        match self { Self::Ima4(_) => 64, Self::Wave(state) => state.frames, Self::Gsm(_) => 320 }
    }
    fn samples(&self) -> &[f32] {
        match self { Self::Ima4(state) => &state.samples, Self::Wave(state) => &state.samples, Self::Gsm(state) => &state.samples }
    }
    fn advance(&mut self, frames: usize) {
        match self { Self::Ima4(state) => state.position += frames, Self::Wave(state) => state.position += frames, Self::Gsm(state) => state.position += frames }
    }
    fn refill(&mut self, file: &mut File, path: &Path, channels: usize) -> Result<(), CodecError> {
        match self {
            Self::Ima4(state) => {
                file.read_exact(&mut state.packet[..34 * channels]).map_err(|e| io_error("read IMA4 packet", path, e))?;
                state.decode_packet(channels);
                Ok(())
            }
            Self::Wave(state) => {
                file.read_exact(&mut state.packet).map_err(|e| io_error("read WAVE ADPCM packet", path, e))?;
                state.decode_packet(channels)
            }
            Self::Gsm(state) => {
                file.read_exact(&mut state.packet).map_err(|e| io_error("read WAVE GSM packet", path, e))?;
                state.decoder.decode(&state.packet, &mut state.samples);
                state.position = 0;
                Ok(())
            }
        }
    }
}

struct AlacReadState {
    config: alac::Config,
    decoder: alac::Decoder,
    packets: Vec<mp4::Packet>,
    next_packet: usize,
    packet_bytes: Vec<u8>,
    samples: Vec<i32>,
    sample_frames: usize,
    position: usize,
    bitrate: Option<u32>,
    failed: bool,
}
impl AlacReadState {
    fn new(info: mp4::AlacFile) -> Self {
        let bytes: u128 = info.packets.iter().map(|p| u128::from(p.size)).sum();
        let bitrate = if info.packets.is_empty() { Some(0) } else {
            let numerator = bytes * 8 * u128::from(info.config.sample_rate);
            let denominator = info.packets.len() as u128 * u128::from(info.config.frame_length);
            u32::try_from(numerator / denominator).ok()
        };
        Self { config: info.config, decoder: alac::Decoder::new(), packets: info.packets, next_packet: 0, packet_bytes: Vec::new(),
            samples: Vec::new(), sample_frames: 0, position: 0, bitrate, failed: false }
    }
    fn refill(&mut self, file: &mut File, path: &Path) -> Result<(), CodecError> {
        while let Some(packet) = self.packets.get(self.next_packet).copied() {
            if packet.frames == 0 { self.next_packet += 1; continue; }
            self.packet_bytes.resize(packet.size as usize, 0);
            read_at(file, packet.offset, &mut self.packet_bytes, path)?;
            self.decoder.decode_packet_into(self.config, &self.packet_bytes, &mut self.samples).map_err(CodecError::AlacDecode)?;
            let sample_frames = packet.frames as usize;
            let required = sample_frames.checked_mul(usize::from(self.config.channels))
                .ok_or(CodecError::InvalidInput("ALAC packet sample count overflows"))?;
            if self.samples.len() < required { return Err(CodecError::InvalidInput("ALAC packet has fewer samples than declared")); }
            self.next_packet += 1;
            self.sample_frames = sample_frames;
            self.position = 0;
            return Ok(());
        }
        Err(CodecError::InvalidInput("ALAC packet table ended before the declared frame count"))
    }
    fn scale(&self) -> f32 {
        match self.config.bit_depth {
            16 => 1.0 / 32_768.0,
            20 => 1.0 / 524_288.0,
            24 => 1.0 / 8_388_608.0,
            32 => 1.0 / 2_147_483_648.0,
            _ => unreachable!("validated ALAC depth"),
        }
    }
}

fn read_at(file: &mut File, offset: u64, bytes: &mut [u8], path: &Path) -> Result<(), CodecError> {
    file.seek(SeekFrom::Start(offset)).and_then(|_| file.read_exact(bytes)).map_err(|e| io_error("read audio", path, e))
}
/// The first bytes of the file, zero padded when it is shorter: every sniff
/// below needs a full match, so a short file simply matches none of them.
fn read_prefix(file: &mut File, bytes: &mut [u8; 12], path: &Path) -> Result<(), CodecError> {
    file.seek(SeekFrom::Start(0)).map_err(|e| io_error("read audio", path, e))?;
    let mut filled = 0;
    while filled < bytes.len() {
        match file.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_error("read audio", path, e)),
        }
    }
    Ok(())
}
fn be32(bytes: &[u8]) -> u32 { u32::from_be_bytes(bytes.try_into().expect("four bytes")) }
fn le32(bytes: &[u8]) -> u32 { u32::from_le_bytes(bytes.try_into().expect("four bytes")) }
fn be16(bytes: &[u8]) -> u16 { u16::from_be_bytes(bytes.try_into().expect("two bytes")) }
fn le16(bytes: &[u8]) -> u16 { u16::from_le_bytes(bytes.try_into().expect("two bytes")) }

fn mp4_audio_entry(data: &[u8], depth: usize) -> Result<Option<[u8; 4]>, CodecError> {
    if depth > 8 { return Err(CodecError::InvalidInput("MP4 atom nesting is too deep")); }
    let mut offset = 0usize;
    let mut found = None;
    while offset < data.len() {
        let remaining = &data[offset..];
        if remaining.len() < 8 { return Err(CodecError::InvalidInput("truncated MP4 atom")); }
        let kind: [u8; 4] = remaining[4..8].try_into().expect("four bytes");
        let (size, header) = match be32(&remaining[..4]) {
            0 => (remaining.len(), 8),
            1 => {
                if remaining.len() < 16 { return Err(CodecError::InvalidInput("truncated MP4 extended atom")); }
                let length = u64::from_be_bytes(remaining[8..16].try_into().expect("eight bytes"));
                (usize::try_from(length).map_err(|_| CodecError::InvalidInput("MP4 atom is too large"))?, 16)
            }
            n => (n as usize, 8),
        };
        if size < header || size > remaining.len() { return Err(CodecError::InvalidInput("invalid MP4 atom size")); }
        let payload = &remaining[header..size];
        let candidate = if matches!(&kind, b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl") {
            mp4_audio_entry(payload, depth + 1)?
        } else if &kind == b"stsd" {
            if payload.len() < 8 { return Err(CodecError::InvalidInput("truncated MP4 sample description")); }
            mp4_audio_entry(&payload[8..], depth + 1)?
        } else if matches!(&kind, b"alac" | b"mp4a") { Some(kind) }
        else { None };
        if let Some(candidate) = candidate {
            if found.is_some_and(|previous| previous != candidate) {
                return Err(CodecError::UnsupportedFormat("MP4 contains multiple audio codec types"));
            }
            found = Some(candidate);
        }
        offset += size;
    }
    Ok(found)
}

pub(crate) use mp4::MAX_SCANNED_BOXES;

fn unsupported_mp4(file: &mut File, path: &Path, length: u64) -> Result<CodecError, CodecError> {
    const MAX_MOOV: u64 = 64 * 1024 * 1024;
    let mut offset = 0u64;
    let mut seen = 0usize;
    while length.saturating_sub(offset) >= 8 {
        seen += 1;
        if seen > MAX_SCANNED_BOXES { return Err(CodecError::UnsupportedFormat("MP4 with more top-level atoms than are scanned")); }
        let mut header = [0u8; 16];
        read_at(file, offset, &mut header[..8], path)?;
        let (size, header_size) = match be32(&header[..4]) {
            0 => (length - offset, 8u64),
            1 => {
                read_at(file, offset + 8, &mut header[8..], path)?;
                (u64::from_be_bytes(header[8..16].try_into().expect("eight bytes")), 16)
            }
            n => (u64::from(n), 8),
        };
        if size < header_size || size > length - offset { return Err(CodecError::InvalidInput("invalid MP4 top-level atom")); }
        if &header[4..8] == b"moov" {
            if size > MAX_MOOV { return Err(CodecError::UnsupportedFormat("MP4 movie atom exceeds the supported inspection size")); }
            let mut moov = vec![0u8; (size - header_size) as usize];
            read_at(file, offset + header_size, &mut moov, path)?;
            return Ok(match mp4_audio_entry(&moov, 0)? {
                // an `mp4a` entry exists, but not as the first sample entry of the first sound track (the AAC reader takes that one only)
                Some(kind) if &kind == b"mp4a" => CodecError::UnsupportedCodec {
                    codec: "AAC/MP4A", operation: "decoding", reason: "the AAC reader takes the first sound track of an MP4 only, and its first sample entry is not mp4a",
                },
                Some(kind) if &kind == b"alac" => CodecError::UnsupportedCodec {
                    codec: "ALAC", operation: "decoding", reason: "portable lossless decoding is unavailable",
                },
                _ => CodecError::UnsupportedFormat("MP4 audio sample entry is not recognized"),
            });
        }
        offset += size;
    }
    Ok(CodecError::UnsupportedFormat("MP4 movie atom not found"))
}

fn aiff_rate(b: &[u8]) -> Result<f64, CodecError> {
    let exponent = be16(&b[..2]);
    if exponent & 0x8000 != 0 || exponent == 0 || exponent == 0x7fff {
        return Err(CodecError::UnsupportedFormat("AIFF sample rate is not a positive normal extended float"));
    }
    let mantissa = u64::from_be_bytes(b[2..10].try_into().expect("extended mantissa"));
    let power = i32::from(exponent) - 16_383 - 63;
    if mantissa >> 63 == 0 || !(-1022..=1023).contains(&power) {
        return Err(CodecError::UnsupportedFormat("AIFF sample rate is outside the supported normal range"));
    }
    let factor = f64::from_bits(((power + 1023) as u64) << 52);
    let rate = mantissa as f64 * factor;
    if !rate.is_finite() || rate <= 0.0 { return Err(CodecError::UnsupportedFormat("invalid AIFF sample rate")); }
    Ok(rate)
}

fn parse_caf(file: &mut File, path: &Path, length: u64) -> Result<ParsedPcm, CodecError> {
    let mut header = [0u8; 8];
    read_at(file, 0, &mut header, path)?;
    if &header[..4] != b"caff" || be16(&header[4..6]) != 1 {
        return Err(CodecError::UnsupportedFormat("unsupported CAF header"));
    }
    let mut offset = 8u64;
    let mut format = None;
    let mut data = None;
    let mut seen = 0usize;
    while offset.checked_add(12).is_some_and(|n| n <= length) {
        seen += 1;
        if seen > MAX_SCANNED_BOXES { return Err(CodecError::UnsupportedFormat("CAF with more chunks than are scanned")); }
        let mut chunk = [0u8; 12];
        read_at(file, offset, &mut chunk, path)?;
        let raw_size = i64::from_be_bytes(chunk[4..12].try_into().expect("CAF chunk size"));
        let start = offset + 12;
        let size = if raw_size == -1 && &chunk[..4] == b"data" { length - start }
            else if raw_size >= 0 { raw_size as u64 }
            else { return Err(CodecError::InvalidInput("invalid CAF chunk length")); };
        let available = size.min(length - start);
        if &chunk[..4] == b"desc" {
            if available < 32 { return Err(CodecError::InvalidInput("truncated CAF stream description")); }
            let mut b = [0u8; 32]; read_at(file, start, &mut b, path)?;
            let rate = f64::from_bits(u64::from_be_bytes(b[..8].try_into().expect("CAF sample rate")));
            let format_id = be32(&b[8..12]); let flags = be32(&b[12..16]);
            let packet_bytes = be32(&b[16..20]); let packet_frames = be32(&b[20..24]);
            let channels = be32(&b[24..28]); let bits = be32(&b[28..32]);
            if format_id != LPCM || flags & !3 != 0 || flags & 1 == 0 || bits != 32 || packet_frames != 1 {
                return Err(CodecError::UnsupportedFormat("CAF intermediate must contain packed float32 PCM"));
            }
            if !rate.is_finite() || rate <= 0.0 || channels == 0 || channels.checked_mul(4) != Some(packet_bytes) {
                return Err(CodecError::InvalidInput("invalid CAF float32 stream description"));
            }
            let big_endian = flags & 2 == 0;
            let source = StreamDescription { sample_rate: rate, format_id: LPCM,
                format_flags: if big_endian { 11 } else { 9 }, bytes_per_packet: packet_bytes,
                frames_per_packet: 1, bytes_per_frame: packet_bytes, channels_per_frame: channels,
                bits_per_channel: 32, reserved: 0 };
            format = Some((Encoding::Float32 { big_endian }, source));
        } else if &chunk[..4] == b"data" {
            if available < 4 { return Err(CodecError::InvalidInput("truncated CAF data chunk")); }
            data = Some((start + 4, available - 4));
        }
        if format.is_some() && data.is_some() { break; }
        let Some(next) = start.checked_add(size) else { return Err(CodecError::InvalidInput("CAF chunk offset overflows")); };
        if next > length { break; }
        offset = next;
    }
    let (encoding, source) = format.ok_or(CodecError::InvalidInput("missing CAF stream description"))?;
    let (data_offset, bytes) = data.ok_or(CodecError::InvalidInput("missing CAF audio data"))?;
    Ok(ParsedPcm { source, encoding, data_offset, frames: bytes / u64::from(source.bytes_per_frame), coefficients: Vec::new() })
}

fn parse_pcm(file: &mut File, path: &Path, length: u64, header: &[u8; 12]) -> Result<ParsedPcm, CodecError> {
    if &header[..4] == b"caff" { return parse_caf(file, path, length); }
    let wav = &header[..4] == b"RIFF" && &header[8..12] == b"WAVE";
    let aifc = &header[..4] == b"FORM" && &header[8..12] == b"AIFC";
    let aiff = (&header[..4] == b"FORM" && &header[8..12] == b"AIFF") || aifc;
    if !wav && !aiff {
        if &header[4..8] == b"ftyp" { return Err(unsupported_mp4(file, path, length)?); }
        // `Reader::open` sends everything that is not PCM, MP4 or ADTS to the MP3 reader first.
        return Err(CodecError::UnsupportedFormat("expected uncompressed RIFF/WAVE or AIFF PCM"));
    }
    let mut offset = 12u64;
    let mut format: Option<(Encoding, StreamDescription)> = None;
    let mut data: Option<(u64, u64)> = None;
    let mut coefficients = Vec::new();
    while offset.checked_add(8).is_some_and(|n| n <= length) {
        let mut chunk = [0u8; 8];
        read_at(file, offset, &mut chunk, path)?;
        let size = u64::from(if wav { le32(&chunk[4..8]) } else { be32(&chunk[4..8]) });
        let start = offset + 8;
        let available = length.saturating_sub(start).min(size);
        if wav && &chunk[..4] == b"fmt " {
            if available < 16 { return Err(CodecError::InvalidInput("truncated WAVE format chunk")); }
            let mut b = [0u8; 16]; read_at(file, start, &mut b, path)?;
            let mut tag = le16(&b[..2]); let channels = u32::from(le16(&b[2..4]));
            let rate = le32(&b[4..8]) as f64; let align = u32::from(le16(&b[12..14]));
            let bits = u32::from(le16(&b[14..16]));
            let extensible = tag == 0xfffe;
            let mut gsm_extensible_supported = false;
            if tag == 0xfffe {
                if available < 40 { return Err(CodecError::InvalidInput("truncated WAVE extensible format")); }
                let mut extension = [0u8; 24]; read_at(file, start + 16, &mut extension, path)?;
                if le16(&extension[..2]) < 22 || extension[12..24] != [0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71] {
                    return Err(CodecError::UnsupportedFormat("unsupported WAVE extensible subtype"));
                }
                let subtype = le32(&extension[8..12]);
                let mask = le32(&extension[4..8]);
                gsm_extensible_supported = subtype == 0x31 && bits != 0 && bits % 8 == 0 &&
                    (mask & !0x0003_ffff == 0 || mask & 0x8000_0000 != 0);
                tag = u16::try_from(subtype).map_err(|_| CodecError::UnsupportedFormat("unsupported WAVE extensible subtype"))?;
            }
            let encoding = match (tag, bits) {
                (1, 8) => Encoding::Unsigned8,
                (1, 1..=32) => Encoding::Signed { bits, big_endian: false },
                (3, 32) => Encoding::Float32 { big_endian: false },
                (3, 64) => Encoding::Float64 { big_endian: false },
                (6, 8) => Encoding::ALaw,
                (7, 8) => Encoding::MuLaw,
                (0x11, 4) => Encoding::WavIma,
                (0x02, 4) => Encoding::WavMs,
                (0x31, _) if !extensible || gsm_extensible_supported => Encoding::WavGsm,
                (0x55, _) => return Err(CodecError::UnsupportedCodec { codec: "MP3", operation: "decoding", reason: "MP3 inside a WAVE container is not reproduced; only elementary MP3 streams are" }),
                (0xff, _) => return Err(CodecError::UnsupportedCodec { codec: "AAC", operation: "decoding", reason: "AAC inside a WAVE container is not reproduced; only AAC files and elementary streams are" }),
                _ => return Err(CodecError::UnsupportedFormat("WAVE encoding or bit depth has not been reproduced")),
            };
            if channels == 0 || rate == 0.0 { return Err(CodecError::InvalidInput("invalid WAVE sample rate or channel count")); }
            let packetized = matches!(encoding, Encoding::WavIma | Encoding::WavMs | Encoding::WavGsm);
            let packet_frames = if matches!(encoding, Encoding::WavGsm) {
                if channels != 1 {
                    return Err(CodecError::UnsupportedFormat("WAVE GSM is supported only for mono streams"));
                }
                // AudioToolbox normalizes tag 0x31 to WAV49's fixed geometry,
                // ignoring the WAVE header's align, bit depth and extensions.
                320
            } else if packetized {
                if channels > 2 || available < 20 { return Err(CodecError::UnsupportedFormat("unsupported WAVE ADPCM channel count or header")); }
                let mut extra = [0u8; 4]; read_at(file, start + 16, &mut extra, path)?;
                let extra_size = usize::from(le16(&extra[..2]));
                let frames = u32::from(le16(&extra[2..4]));
                let overhead = if matches!(encoding, Encoding::WavIma) { 4 * channels } else { 7 * channels };
                if extra_size < 2 || align < overhead || (align - overhead) * 2 % channels != 0 {
                    return Err(CodecError::InvalidInput("invalid WAVE ADPCM block geometry"));
                }
                if matches!(encoding, Encoding::WavIma) && (!align.is_power_of_two() || align < 256 ||
                    (align - overhead) % (4 * channels) != 0) {
                    return Err(CodecError::UnsupportedFormat("unsupported WAVE IMA ADPCM block alignment"));
                }
                let expected = (align - overhead) * 2 / channels + if matches!(encoding, Encoding::WavIma) { 1 } else { 2 };
                if frames != expected { return Err(CodecError::UnsupportedFormat("WAVE ADPCM samples-per-block does not match block geometry")); }
                if matches!(encoding, Encoding::WavMs) {
                    if available < 22 || extra_size < 4 { return Err(CodecError::InvalidInput("truncated MS ADPCM coefficients")); }
                    let mut count = [0u8; 2]; read_at(file, start + 20, &mut count, path)?;
                    let count = usize::from(le16(&count));
                    let bytes = count.checked_mul(4).ok_or(CodecError::InvalidInput("MS ADPCM coefficient count overflows"))?;
                    if count == 0 || extra_size < 4 + bytes || available < (22 + bytes) as u64 {
                        return Err(CodecError::InvalidInput("invalid MS ADPCM coefficients"));
                    }
                    let mut raw = vec![0u8; bytes]; read_at(file, start + 22, &mut raw, path)?;
                    coefficients = raw.chunks_exact(4).map(|v| (i16::from_le_bytes([v[0], v[1]]), i16::from_le_bytes([v[2], v[3]]))).collect();
                }
                frames
            } else {
                if align != channels * encoding.width() as u32 {
                    return Err(CodecError::InvalidInput("invalid WAVE frame alignment"));
                }
                1
            };
            let (format_id, flags) = match encoding {
                Encoding::Unsigned8 => (LPCM, 8),
                Encoding::Signed { .. } if bits % 8 != 0 => (LPCM, 20),
                Encoding::Signed { .. } => (LPCM, 12),
                Encoding::MuLaw => (u32::from_be_bytes(*b"ulaw"), 0),
                Encoding::ALaw => (u32::from_be_bytes(*b"alaw"), 0),
                Encoding::WavIma => (0x6d73_0011, 0),
                Encoding::WavMs => (0x6d73_0002, 0),
                Encoding::WavGsm => (0x6d73_0031, 0),
                _ => (LPCM, 9),
            };
            let source = StreamDescription { sample_rate: rate, format_id, format_flags: flags,
                bytes_per_packet: if matches!(encoding, Encoding::WavGsm) { 65 } else { align },
                frames_per_packet: packet_frames, bytes_per_frame: if packetized { 0 } else { align },
                channels_per_frame: channels, bits_per_channel: if packetized { 0 } else { bits }, reserved: 0 };
            format = Some((encoding, source));
        } else if aiff && &chunk[..4] == b"COMM" {
            let needed = if aifc { 22 } else { 18 };
            if available < needed { return Err(CodecError::InvalidInput("truncated AIFF common chunk")); }
            let mut b = [0u8; 22]; read_at(file, start, &mut b[..needed as usize], path)?;
            let channels = u32::from(be16(&b[..2]));
            let bits = u32::from(be16(&b[6..8])); let rate = aiff_rate(&b[8..18])?;
            if channels == 0 { return Err(CodecError::InvalidInput("AIFF channel count is zero")); }
            let kind = if aifc { &b[18..22] } else { b"NONE" };
            let (encoding, format_id, flags) = match (kind, bits) {
                (b"NONE" | b"twos", 1..=32) => (Encoding::Signed { bits, big_endian: true }, LPCM, if bits % 8 == 0 { 14 } else { 22 }),
                (b"sowt", 1..=32) => (Encoding::Signed { bits, big_endian: false }, LPCM, if bits % 8 == 0 { 12 } else { 20 }),
                (b"fl32" | b"FL32", 32) => (Encoding::Float32 { big_endian: true }, LPCM, 11),
                (b"fl64" | b"FL64", 64) => (Encoding::Float64 { big_endian: true }, LPCM, 11),
                (b"ulaw", 8) => (Encoding::MuLaw, u32::from_be_bytes(*b"ulaw"), 0),
                (b"alaw", 8) => (Encoding::ALaw, u32::from_be_bytes(*b"alaw"), 0),
                (b"ima4", 16) if channels <= 2 => (Encoding::Ima4, u32::from_be_bytes(*b"ima4"), 0),
                _ => return Err(CodecError::UnsupportedFormat("AIFF-C encoding or bit depth has not been reproduced")),
            };
            let source = if matches!(encoding, Encoding::Ima4) {
                StreamDescription { sample_rate: rate, format_id, format_flags: flags, bytes_per_packet: channels * 34,
                    frames_per_packet: 64, bytes_per_frame: 0, channels_per_frame: channels, bits_per_channel: 0, reserved: 0 }
            } else {
                let align = channels * encoding.width() as u32;
                StreamDescription { sample_rate: rate, format_id, format_flags: flags, bytes_per_packet: align,
                    frames_per_packet: 1, bytes_per_frame: align, channels_per_frame: channels, bits_per_channel: bits, reserved: 0 }
            };
            format = Some((encoding, source));
        } else if wav && &chunk[..4] == b"data" {
            data = Some((start, available));
        } else if aiff && &chunk[..4] == b"SSND" {
            if available < 8 { return Err(CodecError::InvalidInput("truncated AIFF sound chunk")); }
            let mut b = [0u8; 8]; read_at(file, start, &mut b, path)?;
            let padding = u64::from(be32(&b[..4]));
            if padding > available - 8 { return Err(CodecError::InvalidInput("invalid AIFF sound offset")); }
            data = Some((start + 8 + padding, available - 8 - padding));
        }
        if format.is_some() && data.is_some() { break; }
        let Some(next) = start.checked_add(size).and_then(|n| n.checked_add(size & 1)) else {
            return Err(CodecError::InvalidInput("audio chunk offset overflows"));
        };
        if next > length { break; }
        offset = next;
    }
    let (encoding, source) = format.ok_or(CodecError::InvalidInput("missing PCM format chunk"))?;
    let (data_offset, bytes) = data.ok_or(CodecError::InvalidInput("missing PCM data chunk"))?;
    let packetized = matches!(encoding, Encoding::Ima4 | Encoding::WavIma | Encoding::WavMs | Encoding::WavGsm);
    // In the observed PCM, G.711 and ADPCM paths, AudioToolbox reports and
    // reads the available data rather than limiting it to COMM/fact counts.
    let frames = if packetized {
        (bytes / u64::from(source.bytes_per_packet)) * u64::from(source.frames_per_packet)
    } else { bytes / u64::from(source.bytes_per_frame) };
    Ok(ParsedPcm { source, encoding, data_offset, frames, coefficients })
}

/// Sequential same-rate PCM, ALAC, companded, ADPCM, GSM and float32 CAF reader.
///
/// macOS arm64 comparisons cover WAVE/AIFF integer depths 1..=32 (all 8/16-bit
/// words; mono for other non-byte-aligned depths; 1/2/6 channels for
/// 8/16/24/32), float32/64, selected WAVE extensible layouts and all 256 G.711
/// codes in WAVE and AIFF-C. AIFF-C IMA4, WAVE IMA/MS ADPCM and mono WAVE GSM
/// were compared on the packet domains in `audio-01/aifc-g711-parity-06` and
/// `gsm-01`; incomplete final compressed packets are discarded. Same-rate ALAC
/// and float32 CAF comparisons are recorded in `audio-01/alac-reader-parity-05`
/// and `audio-01/caf-parity`. Linux arm64 and x86_64 also ran captured-oracle
/// corpora; these are bounded tests, not proof for arbitrary files. Unverified
/// variants and differing rates are refused.
///
/// Mono WAVE GSM tag `0x31` is normalized to 65-byte/320-frame packets even
/// when its legacy header fields disagree. The verified extensible subtype
/// requires nonzero byte-aligned depth and either a low-18-bit channel mask or
/// bit 31 set. Other masks are refused, including some accepted by Apple; see
/// `gsm-01/refusal-summary.json` for the sampled limitation.
///
/// Which reader opens a file is decided as AudioToolbox's `AudioFile` decides it. A name ending in `.mp1`, `.mp2`, `.mp3`,
/// `.mpa` or `.mpeg` (any case) commits to the MPEG audio types: whatever the content is, only the MPEG reader looks at
/// it (a WAVE or an MP4 named `.mp3` is an MPEG stream with junk in front of it, or `'dta?'`). Any other name leaves the
/// choice to the content: leading ID3v2/ID3v1 blocks are stepped over, then WAVE, AIFF, CAF, MP4 and ADTS are told by
/// their signatures, FLAC and Ogg are typed refusals (AudioToolbox decodes them, this port does not), a valid layer I/II/III
/// frame header (or nothing at all, `'wht?'`) selects the MPEG reader and anything else is `'typ?'` (AudioToolbox's
/// answer when no type recognises the data; free-format MPEG, ID3v2 tags with a footer and one to three stray bytes are
/// refused this way).
///
/// MP3, MP2 and MP1 elementary streams open through [`mp3::reader::Mp3Reader`]: the file is read into memory
/// (at most 2 GiB), `source_format`, `file_length_frames`, `bitrate` and the
/// trimmed samples are what `ExtAudioFileRead` delivers on macOS, and a file
/// Apple refuses is refused with its `OSStatus` (`'dta?'`, `'wht?'`, -39,
/// `'fmt?'`). The contract is AudioToolbox's for the same sequence of read
/// sizes: sequential reads of any size deliver the frames, and fail with the
/// status, that `ExtAudioFileRead` does for those sizes (the frames a failed
/// read had gathered are lost, so the number delivered before an error depends
/// on the sizes, as it does on macOS). Not modelled: seeking, a client rate
/// other than the file's (refused), and reading from a reader on which
/// `file_length_frames` or `bitrate` was called first (AudioToolbox's length
/// query scans the whole file and changes what later reads see; the engine
/// queries on a reader it never reads from).
pub struct Reader {
    file: File,
    path: PathBuf,
    source: StreamDescription,
    client: StreamDescription,
    encoding: Encoding,
    frames: u64,
    position: u64,
    raw: Vec<u8>,
    packets: Option<PacketState>,
    alac: Option<AlacReadState>,
    mp3: Option<Box<Mp3ReadState>>,
    aac: Option<Box<aac_read::AacReadState>>,
}
impl Reader {
    pub fn open(path: impl AsRef<Path>, sample_rate: Option<f64>) -> Result<Self, CodecError> {
        let path = path.as_ref();
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b"flac") {
            return Err(CodecError::UnsupportedFormat("the reference uses its separate FLAC decoder for paths ending in flac"));
        }
        let mut file = File::open(path).map_err(|e| io_error("open input", path, e))?;
        let length = file.metadata().map_err(|e| io_error("inspect audio", path, e))?.len();
        // ExtAudioFileOpenURL names no file type, so AudioFile takes it from the file name extension, and an MPEG audio
        // extension commits to the MPEG audio types: whatever the content is (a WAVE, an MP4, ADTS, FLAC or random bytes),
        // only the MPEG reader looks at it, and its refusal is final. Any other name leaves the choice to the content.
        if has_mpeg_extension(path) { return Self::open_mp3(file, path, length, sample_rate); }
        // `.aac .adts .loas .latm` commit to the ADTS / LOAS types the same way: whatever the content is, only those parsers look at it.
        if aac_read::has_adts_extension(path) { return Self::open_aac(file, path, length, sample_rate); }
        // `.ogg .oga .opus` commit to the Ogg types, which fail `'dta?'` on anything that is not an Ogg stream, and `.sd2` to Sound Designer II, which
        // fails with an end-of-file error on every file that is not empty.
        if length != 0 {
            if aac_read::has_ogg_extension(path) {
                let mut magic = [0u8; 4];
                read_at(&mut file, 0, &mut magic[..length.min(4) as usize], path)?;
                if &magic != b"OggS" { return Err(open_refusal(DTA)); }
            } else if aac_read::has_extension(path, "sd2") {
                return Err(open_refusal(END_OF_FILE));
            }
        }
        let skipped = leading_tag_length(&mut file, length, path)?;
        if skipped != 0 { return Self::open_behind_tags(file, path, length, skipped, sample_rate); }
        let mut header = [0u8; 12]; read_prefix(&mut file, &mut header, path)?;
        // A `.mov` name commits AudioFile to its QuickTime type, which refuses every file that is not an MP4 / QuickTime one.
        if length != 0 && aac_read::has_extension(path, "mov") && !matches!(&header[4..8], b"ftyp" | b"moov" | b"free" | b"wide" | b"mdat") {
            return Err(open_refusal(NOT_QUICKTIME));
        }
        if matches!(&header[4..8], b"ftyp" | b"moov" | b"free" | b"wide" | b"mdat") {
            // The sample entry of the first sound track decides the reader: AAC is decoded by the AAC reader, everything else is judged as before.
            if aac_read::first_sound_entry_is_mp4a(&mut file, length) { return Self::open_aac(file, path, length, sample_rate); }
            let info = match mp4::read_alac(&mut file) {
                Ok(info) => info,
                Err(error) => {
                    // A container that is damaged for any codec fails as AudioToolbox says; whatever else is wrong with the file is described as before.
                    let reason = match unsupported_mp4(&mut file, path, length) {
                        Ok(reason) => reason,
                        Err(failure) => return Err(match aac_read::container_verdict(&mut file, path, length) {
                            aac_read::Verdict::Status(status) => status,
                            aac_read::Verdict::Aac => return Self::open_aac(file, path, length, sample_rate),
                            aac_read::Verdict::Silent => failure,
                        }),
                    };
                    let alac = matches!(reason, CodecError::UnsupportedCodec { codec: "ALAC", .. });
                    if !alac {
                        match aac_read::container_verdict(&mut file, path, length) {
                            aac_read::Verdict::Status(status) => return Err(status),
                            aac_read::Verdict::Aac => return Self::open_aac(file, path, length, sample_rate),
                            aac_read::Verdict::Silent => {}
                        }
                    }
                    return Err(if alac { CodecError::Mp4Read(error) } else { reason });
                }
            };
            let config = info.config;
            if !(1..=8).contains(&config.channels) || config.frame_length == 0 || config.sample_rate == 0 {
                return Err(CodecError::UnsupportedFormat("ALAC channel count, rate or frame length is not verified"));
            }
            if config.channels == 6 {
                if let Some(layout) = info.channel_layout {
                    let known = layout.bitmap == 0 && layout.descriptions == 0 && layout.tag & 0xffff == 6 &&
                        matches!(layout.tag >> 16, 121 | 122 | 123 | 124 | 147 | 65_535);
                    if !known { return Err(CodecError::UnsupportedFormat("ALAC six-channel layout has not been verified")); }
                }
            }
            if matches!(config.channels, 3 | 4 | 5 | 7 | 8) {
                let tag = match config.channels { 3 => 114, 4 => 116, 5 => 120, 7 => 142, 8 => 127, _ => unreachable!() };
                let expected = (tag << 16) | u32::from(config.channels);
                if !info.channel_layout.is_some_and(|layout| layout.tag == expected && layout.bitmap == 0 && layout.descriptions == 0) {
                    return Err(CodecError::UnsupportedFormat("ALAC multichannel layout has not been verified"));
                }
            }
            let flags = match config.bit_depth {
                16 => 1, 20 => 2, 24 => 3, 32 => 4,
                _ => return Err(CodecError::UnsupportedFormat("ALAC bit depth is not supported")),
            };
            let rate = same_rate(f64::from(config.sample_rate), sample_rate)?;
            let source = StreamDescription { sample_rate: rate, format_id: ALAC, format_flags: flags,
                bytes_per_packet: 0, frames_per_packet: config.frame_length, bytes_per_frame: 0,
                channels_per_frame: u32::from(config.channels), bits_per_channel: 0, reserved: 0 };
            let client = client_format(rate, source.channels_per_frame, false)?;
            let frames = info.frames;
            return Ok(Self { file, path: path.to_owned(), source, client, encoding: Encoding::Alac,
                frames, position: 0, raw: Vec::new(), packets: None, alac: Some(AlacReadState::new(info)), mp3: None, aac: None });
        }
        if &header[..4] == b"caff" && aac_read::caf_format_is_aac(&mut file, length) { return Self::open_aac(file, path, length, sample_rate); }
        if adts_sniff(&header, length) { return Self::open_aac(file, path, length, sample_rate); }
        if !is_pcm_container(&header) {
            return Self::open_unrecognised(file, path, length, &header, sample_rate);
        }
        let parsed = match parse_pcm(&mut file, path, length, &header) {
            Ok(parsed) => parsed,
            // a CAF that is damaged for any codec fails as AudioToolbox says
            Err(error) if &header[..4] == b"caff" => return Err(match aac_read::container_verdict(&mut file, path, length) {
                aac_read::Verdict::Status(status) => status,
                aac_read::Verdict::Aac => return Self::open_aac(file, path, length, sample_rate),
                aac_read::Verdict::Silent => error,
            }),
            Err(error) => return Err(error),
        };
        let rate = same_rate(parsed.source.sample_rate, sample_rate)?;
        let client = client_format(rate, parsed.source.channels_per_frame, false)?;
        file.seek(SeekFrom::Start(parsed.data_offset)).map_err(|e| io_error("seek audio data", path, e))?;
        let packets = match parsed.encoding {
            Encoding::Ima4 => Some(PacketState::Ima4(Box::new(ImaState::new()))),
            Encoding::WavGsm => Some(PacketState::Gsm(Box::new(GsmState::new()))),
            Encoding::WavIma | Encoding::WavMs => Some(PacketState::Wave(Box::new(WavAdpcmState::new(
                parsed.encoding, parsed.source.bytes_per_packet as usize, parsed.source.frames_per_packet as usize,
                parsed.source.channels_per_frame as usize, parsed.coefficients,
            )))),
            _ => None,
        };
        Ok(Self { file, path: path.to_owned(), source: parsed.source, client, encoding: parsed.encoding,
            frames: parsed.frames, position: 0, raw: Vec::new(), packets, alac: None, mp3: None, aac: None })
    }
    pub fn source_format(&self) -> StreamDescription { self.source }
    pub fn client_format(&self) -> StreamDescription { self.client }
    pub fn file_length_frames(&self) -> Result<i64, CodecError> {
        if let Some(mp3) = &self.mp3 { return Ok(mp3.reader.file_length_frames()); }
        if self.aac.is_some() { return self.aac_file_length_frames(); }
        i64::try_from(self.frames).map_err(|_| CodecError::InvalidInput("frame count exceeds i64"))
    }
    pub fn bitrate(&self) -> Result<u32, CodecError> {
        if let Some(mp3) = &self.mp3 { return Ok(mp3.reader.bitrate()); }
        if self.aac.is_some() { return self.aac_bitrate(); }
        if let Some(alac) = &self.alac {
            return alac.bitrate.ok_or(CodecError::InvalidInput("ALAC bitrate exceeds u32"));
        }
        let value = self.source.sample_rate * f64::from(self.source.bytes_per_packet) * 8.0 /
            f64::from(self.source.frames_per_packet);
        if value > u32::MAX as f64 { return Err(CodecError::InvalidInput("PCM bitrate exceeds u32")); }
        Ok(value as u32)
    }
    pub fn set_stereo_planar(&mut self, sample_rate: f64) -> Result<(), CodecError> {
        if self.poisoned() { return Err(CodecError::Poisoned); }
        let rate = if self.aac.is_some() { self.aac_set_stereo_planar(sample_rate)? } else { same_rate(self.source.sample_rate, Some(sample_rate))? };
        self.client = client_format(rate, 2, true)?;
        Ok(())
    }
    fn fetch(&mut self, requested: usize) -> Result<usize, CodecError> {
        let frames = (self.frames - self.position).min(requested as u64) as usize;
        let bytes = frames.checked_mul(self.source.bytes_per_frame as usize).ok_or(CodecError::InvalidInput("buffer size overflows"))?;
        self.raw.resize(bytes, 0);
        self.file.read_exact(&mut self.raw).map_err(|e| io_error("read audio data", &self.path, e))?;
        self.position += frames as u64;
        Ok(frames)
    }
    fn read_packets(&mut self, output: &mut [f32], mut right: Option<&mut [f32]>) -> Result<usize, CodecError> {
        let channels = self.source.channels_per_frame as usize;
        let requested = if right.is_some() { output.len() } else { output.len() / channels };
        let frames = (self.frames - self.position).min(requested as u64) as usize;
        let mut written = 0usize;
        while written < frames {
            let state = self.packets.as_mut().expect("ADPCM packet state");
            if state.position() == state.frames() { state.refill(&mut self.file, &self.path, channels)?; }
            let count = (state.frames() - state.position()).min(frames - written);
            let start = state.position() * channels;
            let samples = state.samples();
            if let Some(right) = right.as_deref_mut() {
                for i in 0..count {
                    output[written + i] = samples[start + i * channels];
                    right[written + i] = if channels == 1 { output[written + i] } else { samples[start + i * channels + 1] };
                }
            } else {
                output[written * channels..(written + count) * channels]
                    .copy_from_slice(&samples[start..start + count * channels]);
            }
            state.advance(count);
            written += count;
        }
        self.position += frames as u64;
        Ok(frames)
    }
    fn read_alac(&mut self, output: &mut [f32], mut right: Option<&mut [f32]>) -> Result<usize, CodecError> {
        let channels = self.source.channels_per_frame as usize;
        let requested = if right.is_some() { output.len() } else { output.len() / channels };
        let state = self.alac.as_mut().expect("ALAC state");
        if requested != 0 && self.frames == 0 && state.packets.is_empty() {
            state.failed = true;
            return Err(CodecError::Native {
                operation: if right.is_some() { "ExtAudioFileRead(planar stereo)" } else { "ExtAudioFileRead" },
                status: -66_567,
            });
        }
        let frames = (self.frames - self.position).min(requested as u64) as usize;
        let mut written = 0usize;
        while written < frames {
            if state.position == state.sample_frames {
                if let Err(error) = state.refill(&mut self.file, &self.path) {
                    // A failed refill may have consumed input or modified the
                    // decoder/sample buffers, including after a partial read.
                    state.failed = true;
                    return Err(error);
                }
            }
            let count = (state.sample_frames - state.position).min(frames - written);
            let start = state.position * channels;
            let scale = state.scale();
            if let Some(right) = right.as_deref_mut() {
                let left_channel = match channels { 3..=7 => 1, 8 => 3, _ => 0 };
                for i in 0..count {
                    output[written + i] = state.samples[start + i * channels + left_channel] as f32 * scale;
                    right[written + i] = if channels == 1 { output[written + i] }
                        else { state.samples[start + i * channels + left_channel + 1] as f32 * scale };
                }
            } else {
                for (dst, &value) in output[written * channels..(written + count) * channels].iter_mut()
                    .zip(&state.samples[start..start + count * channels]) {
                    *dst = value as f32 * scale;
                }
            }
            state.position += count;
            written += count;
        }
        self.position += frames as u64;
        Ok(frames)
    }
    /// Read complete interleaved frames; the unused output tail is untouched (an MP3 read that reaches the end of the valid
    /// frames fills it with the decoder's padding, as AudioToolbox does).
    pub fn read(&mut self, output: &mut [f32]) -> Result<usize, CodecError> {
        if self.poisoned() { return Err(CodecError::Poisoned); }
        if self.client.format_flags & 0x20 != 0 { return Err(CodecError::InvalidInput("use read_stereo_planar for a planar reader")); }
        let channels = self.source.channels_per_frame as usize;
        if output.len() % channels != 0 { return Err(CodecError::InvalidInput("buffer does not contain whole interleaved frames")); }
        if output.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) { return Err(CodecError::InvalidInput("buffer is too large")); }
        if self.alac.is_some() { return self.read_alac(output, None); }
        if self.mp3.is_some() { return self.read_mp3(output, None); }
        if self.aac.is_some() { return self.read_aac(output, None); }
        if self.packets.is_some() { return self.read_packets(output, None); }
        let frames = self.fetch(output.len() / channels)?;
        let width = self.encoding.width();
        for (out, bytes) in output[..frames * channels].iter_mut().zip(self.raw.chunks_exact(width)) {
            *out = self.encoding.decode(bytes);
        }
        Ok(frames)
    }
    /// Read stereo planes, duplicating mono. PCM uses the first two channels;
    /// ALAC follows the verified default layout's left/right channel positions. The unused tail is untouched, except for an
    /// MP3 read that reaches the end of the valid frames (see [`Reader::read`]).
    pub fn read_stereo_planar(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<usize, CodecError> {
        if self.poisoned() { return Err(CodecError::Poisoned); }
        if self.client.format_flags & 0x20 == 0 { return Err(CodecError::InvalidInput("reader is not configured for planar stereo")); }
        if left.len() != right.len() { return Err(CodecError::InvalidInput("stereo plane lengths differ")); }
        if left.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) { return Err(CodecError::InvalidInput("stereo plane is too large")); }
        if self.alac.is_some() { return self.read_alac(left, Some(right)); }
        if self.mp3.is_some() { return self.read_mp3(left, Some(right)); }
        if self.aac.is_some() { return self.read_aac(left, Some(right)); }
        if self.packets.is_some() { return self.read_packets(left, Some(right)); }
        let frames = self.fetch(left.len())?;
        let width = self.encoding.width();
        let frame_width = self.source.bytes_per_frame as usize;
        for (i, frame) in self.raw.chunks_exact(frame_width).enumerate() {
            left[i] = self.encoding.decode(&frame[..width]);
            right[i] = if self.source.channels_per_frame == 1 { left[i] } else { self.encoding.decode(&frame[width..2 * width]) };
        }
        Ok(frames)
    }
    pub fn close(self) -> Result<(), CodecError> { Ok(()) }
}

/// MP3 decoding state: the exact reader plus the first-failure poisoning every
/// codec here has.
struct Mp3ReadState {
    reader: mp3::reader::Mp3Reader,
    /// int16 samples of the current read, interleaved at the source channel count.
    scratch: Vec<i16>,
    failed: bool,
}

/// The largest MP3 input that is decoded: the whole file is held in memory.
const MAX_MP3_BYTES: u64 = 1 << 31;

/// WAVE, AIFF/AIFC and CAF inputs are PCM containers; under a name without an MPEG audio extension they are never opened as MP3.
fn is_pcm_container(header: &[u8; 12]) -> bool {
    (&header[..4] == b"RIFF" && &header[8..12] == b"WAVE")
        || (&header[..4] == b"FORM" && matches!(&header[8..12], b"AIFF" | b"AIFC"))
        || &header[..4] == b"caff"
}

/// ADTS framing: a 12-bit sync and layer bits 00, which no MPEG audio layer
/// uses.
fn is_adts_aac(header: &[u8; 12]) -> bool { header[0] == 0xff && header[1] & 0xf6 == 0xf0 }

/// AudioFile's ADTS answer when the file starts with ADTS framing: `'typ?'` unless the header can belong to a stream. Fitted on
/// 17,000 random headers on macOS 27.2 (4 outliers, all with a frame length of 8 or 9): at least seven bytes, a frame length
/// of eight or more, a sampling-frequency index below 13, and no profile 3 in the MPEG-2 (ID bit set) variant.
fn adts_sniff(header: &[u8; 12], remaining: u64) -> bool {
    if remaining < 7 || !is_adts_aac(header) { return false; }
    let frame = (usize::from(header[3] & 3) << 11) | (usize::from(header[4]) << 3) | usize::from(header[5] >> 5);
    frame >= 8 && (header[2] >> 2) & 15 < 13 && !(header[1] & 8 != 0 && header[2] >> 6 == 3)
}

/// File name extensions that make AudioFile choose its MPEG audio types (`.mp1 .mp2 .mp3 .mpa .mpeg`, any case). Observed on
/// macOS 27.2 with random data: these answer `'dta?'`, the names the engine hands to this reader otherwise `'typ?'`
/// (`.ogg .oga .opus` also commit, to the Ogg types, and `.aac .adts`, `.loas .latm` to the ADTS and LOAS types the AAC reader opens).
fn has_mpeg_extension(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        let extension = extension.as_encoded_bytes();
        [&b"mp1"[..], b"mp2", b"mp3", b"mpa", b"mpeg"].iter().any(|name| extension.eq_ignore_ascii_case(name))
    })
}

const DTA: i32 = 0x6474_613f; // 'dta?': recognised as a type, but the data does not open
const TYP: i32 = 0x7479_703f; // 'typ?': no file type recognises the data
const END_OF_FILE: i32 = -39; // `kAudioFileEndOfFileError`
const NOT_QUICKTIME: i32 = -12_848; // a `.mov` name with data that is not an MP4 / QuickTime file (observed on macOS 27.2)
fn open_refusal(status: i32) -> CodecError { CodecError::Native { operation: "ExtAudioFileOpenURL", status } }

/// Bytes of leading ID3v2 and ID3v1 blocks, which AudioFile steps over before it identifies the format (same size rule as
/// the MPEG container: `ID3` + syncsafe size + 10 without footer, `TAG` 128). May exceed the length of the file.  The heads are read through a window of the
/// file: a chain of empty tags costs a read per window, not per tag.
fn leading_tag_length(file: &mut File, length: u64, path: &Path) -> Result<u64, CodecError> {
    let mut offset = 0u64;
    let mut window = aac_read::Window::new(file);
    for _ in 0..crate::portable::aac::decoder::MAX_TAG_BLOCKS {
        if offset >= length { break; }
        let wanted = (length - offset).min(10) as usize;
        let mut head = [0u8; 10];
        match window.at(offset, wanted) {
            Some(bytes) => head[..wanted].copy_from_slice(bytes),
            None => return Err(io_error("read audio", path, io::Error::from(io::ErrorKind::UnexpectedEof))),
        }
        let size = mp3::container::id3_size(&head[..wanted]);
        if size == 0 { break; }
        offset = offset.saturating_add(size as u64);
    }
    Ok(offset)
}

/// What AudioFile's per-type content sniffing makes of the data at the start of a file (or behind its leading tags) whose
/// name carries no MPEG audio extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content { Adts, Loas, Flac, Ogg, Wave, Aiff, Caf, Mp4, Mpeg, Unknown }

/// `remaining` is the number of bytes from the start of `header` to the end of the file. The MPEG types answer yes to a
/// valid layer I/II/III frame header in the first four bytes (`MPEGAudioFormat::FileDataIsThisFormat`) and to nothing at
/// all (the reader then answers `'wht?'`); one to three bytes, a free-format or any invalid header answer no.
fn identify(header: &[u8; 12], remaining: u64) -> Content {
    if remaining == 0 { return Content::Mpeg; }
    if adts_sniff(header, remaining) { return Content::Adts; }
    if header[0] == 0x56 && header[1] & 0xe0 == 0xe0 { return Content::Loas; }
    if header.starts_with(b"fLaC") { return Content::Flac; }
    if header.starts_with(b"OggS") { return Content::Ogg; }
    if &header[..4] == b"RIFF" && &header[8..12] == b"WAVE" { return Content::Wave; }
    if &header[..4] == b"FORM" && matches!(&header[8..12], b"AIFF" | b"AIFC") { return Content::Aiff; }
    if &header[..4] == b"caff" { return Content::Caf; }
    if matches!(&header[4..8], b"ftyp" | b"moov" | b"free" | b"wide" | b"mdat") { return Content::Mp4; }
    if remaining >= 4 && mp3::container::parse_sync_word(be32(&header[..4])).is_some() { return Content::Mpeg; }
    Content::Unknown
}

fn flac_refusal() -> CodecError {
    CodecError::UnsupportedCodec { codec: "FLAC", operation: "decoding", reason: "AudioToolbox's own FLAC reader, used for a FLAC stream whose file name does not end in flac, is not reproduced" }
}
fn ogg_refusal() -> CodecError {
    CodecError::UnsupportedFormat("Ogg audio (Opus, FLAC and the other codecs AudioToolbox opens by content) is not decoded by this build")
}

/// The native call that failed while opening: the container parser
/// (`ExtAudioFileOpenURL`) or the client-format setter, with Apple's status.
fn mp3_open_error(error: mp3::reader::ReaderError) -> CodecError {
    let operation = match error {
        mp3::reader::ReaderError::Open(_) => "ExtAudioFileOpenURL",
        _ => "ExtAudioFileSetProperty(client format)",
    };
    CodecError::Native { operation, status: error.status() }
}

fn mp3_read_error(operation: &'static str, error: mp3::reader::ReaderError) -> CodecError {
    match error {
        mp3::reader::ReaderError::Poisoned => CodecError::Poisoned,
        other => CodecError::Native { operation, status: other.status() },
    }
}

impl Reader {
    /// A reader over the exact MP3 decoder; its source description is the one
    /// AudioToolbox reports for MPEG audio files (flags, packet and frame bytes
    /// and bit depth are zero).
    fn open_mp3(mut file: File, path: &Path, length: u64, sample_rate: Option<f64>) -> Result<Self, CodecError> {
        const TOO_LARGE: CodecError = CodecError::UnsupportedFormat("MP3 input larger than 2 GiB is not supported: the file is decoded from memory");
        if length > MAX_MP3_BYTES { return Err(TOO_LARGE); }
        let mut data = Vec::new();
        data.try_reserve_exact(length as usize)
            .map_err(|_| io_error("read audio", path, io::ErrorKind::OutOfMemory.into()))?;
        file.seek(SeekFrom::Start(0)).map_err(|e| io_error("read audio", path, e))?;
        (&mut file).take(MAX_MP3_BYTES + 1).read_to_end(&mut data).map_err(|e| io_error("read audio", path, e))?;
        if data.len() as u64 > MAX_MP3_BYTES { return Err(TOO_LARGE); }
        let reader = mp3::reader::Mp3Reader::open(data).map_err(mp3_open_error)?;
        let format = reader.source_format();
        let source = StreamDescription { sample_rate: f64::from(format.sample_rate), format_id: format.format_id,
            format_flags: 0, bytes_per_packet: 0, frames_per_packet: format.frames_per_packet, bytes_per_frame: 0,
            channels_per_frame: format.channels, bits_per_channel: 0, reserved: 0 };
        let rate = same_rate(source.sample_rate, sample_rate)?;
        let client = client_format(rate, format.channels, false)?;
        // `frames` and `position` serve the PCM and ALAC paths only; an MP3 answers `file_length_frames` from its
        // container model. Open issues no length query: on AudioToolbox that query scans the whole file and so changes
        // what later reads see, which the model does not reproduce (the engine queries length and bitrate on a reader
        // it does not read from).
        Ok(Self { file, path: path.to_owned(), source, client, encoding: Encoding::Float32 { big_endian: false },
            frames: 0, position: 0, raw: Vec::new(), packets: None, alac: None,
            mp3: Some(Box::new(Mp3ReadState { reader, scratch: Vec::new(), failed: false })), aac: None })
    }
    /// A file with no MPEG audio extension whose start is not WAVE, AIFF, CAF, MP4 or ADTS: what is left of AudioFile's
    /// content sniffing is FLAC and Ogg (typed refusals: AudioToolbox decodes them, this port does not), the MPEG types
    /// (`identify`) and, when no type recognises the data, `'typ?'`.
    fn open_unrecognised(file: File, path: &Path, length: u64, header: &[u8; 12], sample_rate: Option<f64>) -> Result<Self, CodecError> {
        match identify(header, length) {
            Content::Mpeg => Self::open_mp3(file, path, length, sample_rate),
            Content::Loas => Self::open_aac(file, path, length, sample_rate),
            Content::Flac => Err(flac_refusal()),
            Content::Ogg => Err(ogg_refusal()),
            _ => Err(open_refusal(TYP)),
        }
    }
    /// The same for a file that starts with ID3v2/ID3v1 blocks (`skipped` bytes), which AudioFile steps over before it sniffs.
    /// ADTS and LOAS are then decoded by the AAC reader, FLAC and Ogg are refused as above, and WAVE data is refused too (its parser here starts at
    /// the first byte); AIFF, CAF and MP4 are sniffed but do not open (`'dta?'`).
    fn open_behind_tags(file: File, path: &Path, length: u64, skipped: u64, sample_rate: Option<f64>) -> Result<Self, CodecError> {
        let remaining = length.saturating_sub(skipped);
        let mut header = [0u8; 12];
        let mut file = file;
        if remaining != 0 {
            let wanted = remaining.min(12) as usize;
            read_at(&mut file, skipped, &mut header[..wanted], path)?;
        }
        let content = identify(&header, remaining);
        if remaining != 0 && content != Content::Mp4 && aac_read::has_extension(path, "mov") { return Err(open_refusal(NOT_QUICKTIME)); }
        match content {
            Content::Mpeg => Self::open_mp3(file, path, length, sample_rate),
            Content::Adts | Content::Loas => Self::open_aac(file, path, length, sample_rate),
            Content::Flac => Err(flac_refusal()),
            Content::Ogg => Err(ogg_refusal()),
            Content::Wave => Err(CodecError::UnsupportedFormat("RIFF/WAVE data behind a leading ID3 tag has not been reproduced")),
            Content::Aiff | Content::Caf | Content::Mp4 => Err(open_refusal(DTA)),
            Content::Unknown => Err(open_refusal(TYP)),
        }
    }
    /// One `ExtAudioFileRead`: interleaved at the source channel count, or two planes (mono duplicated). The float samples
    /// are the int16 ones over 32768. Like AudioToolbox's converter, a read at the end of the valid frames fills the buffer
    /// with every frame it decoded (the stream's padding) and returns the lower count: the engine processes whole blocks.
    fn read_mp3(&mut self, output: &mut [f32], right: Option<&mut [f32]>) -> Result<usize, CodecError> {
        let state = self.mp3.as_mut().expect("MP3 state");
        let channels = state.reader.channels();
        let planar = right.is_some();
        let requested = if planar { output.len() } else { output.len() / channels };
        if requested == 0 { return Ok(0); }
        state.scratch.resize(requested * channels, 0);
        let frames = match state.reader.read(&mut state.scratch, requested) {
            Ok(frames) => frames,
            Err(error) => {
                state.failed = true;
                return Err(mp3_read_error(if planar { "ExtAudioFileRead(planar stereo)" } else { "ExtAudioFileRead" }, error));
            }
        };
        let samples = &state.scratch[..state.reader.last_written() * channels];
        match right {
            Some(right) => for (i, frame) in samples.chunks_exact(channels).enumerate() {
                output[i] = f32::from(frame[0]) / 32_768.0;
                right[i] = f32::from(frame[channels - 1]) / 32_768.0;
            },
            None => for (dst, &sample) in output.iter_mut().zip(samples) { *dst = f32::from(sample) / 32_768.0; },
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod alac_reader_tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "pn-alac-reader-test-{}-{}", std::process::id(), NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            let path = directory.join("input.m4a");
            let mut writer = Writer::create(&path, WriterConfig {
                format: OutputFormat::Alac, sample_rate: 44_100.0, channels: 1, bit_depth: 16,
            }).unwrap();
            writer.write(&vec![0.25; 4097]).unwrap();
            writer.finish().unwrap();
            Self(directory)
        }
        fn path(&self) -> PathBuf { self.0.join("input.m4a") }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn assert_poisoned(reader: &mut Reader) {
        for _ in 0..2 {
            assert!(matches!(reader.read(&mut []), Err(CodecError::Poisoned)));
            assert!(matches!(reader.read(&mut [0.0]), Err(CodecError::Poisoned)));
            assert!(matches!(reader.read_stereo_planar(&mut [], &mut []), Err(CodecError::Poisoned)));
            assert!(matches!(reader.read_stereo_planar(&mut [0.0], &mut [0.0]), Err(CodecError::Poisoned)));
            assert!(matches!(reader.set_stereo_planar(44_100.0), Err(CodecError::Poisoned)));
        }
    }

    #[test]
    fn alac_zero_packet_read_errors_poison_interleaved_and_planar_readers() {
        let fixture = Fixture::new();
        let path = fixture.0.join("empty.m4a");
        Writer::create(&path, WriterConfig {
            format: OutputFormat::Alac, sample_rate: 44_100.0, channels: 1, bit_depth: 16,
        }).unwrap().finish().unwrap();
        for planar in [false, true] {
            let mut reader = Reader::open(&path, None).unwrap();
            assert_eq!(reader.file_length_frames().unwrap(), 0);
            if planar {
                reader.set_stereo_planar(44_100.0).unwrap();
                assert_eq!(reader.read_stereo_planar(&mut [], &mut []).unwrap(), 0);
                let mut left = [3.0];
                let mut right = [7.0];
                assert!(matches!(reader.read_stereo_planar(&mut left, &mut right),
                    Err(CodecError::Native { operation: "ExtAudioFileRead(planar stereo)", status: -66_567 })));
                assert_eq!(left, [3.0]);
                assert_eq!(right, [7.0]);
            } else {
                assert_eq!(reader.read(&mut []).unwrap(), 0);
                let mut output = [3.0];
                assert!(matches!(reader.read(&mut output),
                    Err(CodecError::Native { operation: "ExtAudioFileRead", status: -66_567 })));
                assert_eq!(output, [3.0]);
            }
            assert_poisoned(&mut reader);
        }
    }

    #[test]
    fn alac_failed_decode_poisoned_even_after_partial_output() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut reader = Reader::open(&path, None).unwrap();
        let packet = reader.alac.as_ref().unwrap().packets[1];
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(packet.offset)).unwrap();
        file.write_all(&[0xe0]).unwrap(); // End element with no audio channels.
        let mut output = vec![77.0; 4097];
        assert!(matches!(reader.read(&mut output), Err(CodecError::AlacDecode(_))));
        assert_eq!(output[0], 0.25);
        assert_eq!(output[4096], 77.0);
        assert_poisoned(&mut reader);
    }

    #[test]
    fn alac_short_decoded_packet_is_error_then_poisoned() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut reader = Reader::open(&path, None).unwrap();
        let state = reader.alac.as_ref().unwrap();
        let packet = state.packets[1];
        let empty = alac::Encoder::new(state.config).unwrap().encode_packet(&[]).unwrap();
        assert!(empty.len() <= packet.size as usize);
        // Change the already-indexed packet to a valid zero-frame packet.
        // The index still declares one frame, so the decoder must refuse.
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(packet.offset)).unwrap();
        file.write_all(&empty).unwrap();
        let mut output = vec![77.0; 4097];
        assert!(matches!(reader.read(&mut output),
            Err(CodecError::InvalidInput("ALAC packet has fewer samples than declared"))));
        assert_eq!(output[0], 0.25);
        assert_eq!(output[4096], 77.0);
        assert_poisoned(&mut reader);
    }

    #[test]
    fn alac_failed_packet_io_poisoned_for_planar_reads() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut reader = Reader::open(&path, None).unwrap();
        let offset = reader.alac.as_ref().unwrap().packets[1].offset;
        OpenOptions::new().write(true).open(path).unwrap().set_len(offset).unwrap();
        reader.set_stereo_planar(44_100.0).unwrap();
        let mut left = vec![77.0; 4097];
        let mut right = vec![77.0; 4097];
        assert!(matches!(reader.read_stereo_planar(&mut left, &mut right), Err(CodecError::Io { .. })));
        assert_eq!(left[0], 0.25);
        assert_eq!(right[0], 0.25);
        assert_eq!(left[4096], 77.0);
        assert_poisoned(&mut reader);
    }

    #[test]
    fn alac_missing_packet_table_poisoned() {
        let fixture = Fixture::new();
        let mut reader = Reader::open(fixture.path(), None).unwrap();
        reader.alac.as_mut().unwrap().packets.truncate(1);
        let mut output = vec![77.0; 4097];
        assert!(matches!(reader.read(&mut output),
            Err(CodecError::InvalidInput("ALAC packet table ended before the declared frame count"))));
        assert_poisoned(&mut reader);
    }
}

/// Output recorder selected by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat { Wav, Aiff, Alac, Aac }

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WriterConfig {
    pub format: OutputFormat,
    pub sample_rate: f64,
    pub channels: u32,
    pub bit_depth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecWarning {
    AacConverterUnavailable { status: i32 },
    AacBitRateNotSet { status: i32 },
}

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

struct Workspace {
    directory: PathBuf,
    file: PathBuf,
    identity: Option<File>,
    auxiliary: Option<(PathBuf, File)>,
}
impl Workspace {
    fn create(target: &Path) -> Result<Self, CodecError> {
        let parent = target.parent().ok_or(CodecError::InvalidInput("target has no parent directory"))?;
        let name = target.file_name().ok_or(CodecError::InvalidInput("target has no file name"))?;
        for _ in 0..128 {
            let nonce = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
            let directory = parent.join(format!(".pn-portable-codec-{}-{nonce:x}", std::process::id()));
            if directory == target { continue; }
            match DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => return Ok(Self { file: directory.join(name), directory, identity: None, auxiliary: None }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io_error("create codec workspace", &directory, e)),
            }
        }
        Err(CodecError::InvalidInput("could not allocate a unique codec workspace"))
    }
    fn open_file(&mut self) -> Result<File, CodecError> {
        let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&self.file)
            .map_err(|e| io_error("create codec file", &self.file, e))?;
        self.identity = Some(file);
        self.identity.as_ref().unwrap().try_clone().map_err(|e| io_error("pin codec file", &self.file, e))
    }
    /// The one private spool file of a codec that cannot hold its output in memory: `name` inside the workspace (`-1` appended if it is the output's name).
    fn open_spool(&mut self, name: &str, create: &'static str, pin: &'static str) -> Result<File, CodecError> {
        let mut path = self.directory.join(name);
        if path == self.file { path = self.directory.join(format!("{name}-1")); }
        let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)
            .map_err(|e| io_error(create, &path, e))?;
        self.auxiliary = Some((path, file));
        let (path, file) = self.auxiliary.as_ref().unwrap();
        file.try_clone().map_err(|e| io_error(pin, path, e))
    }
    fn check_identity(path: &Path, identity: Option<&File>) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        let held = identity.ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "codec file is not pinned"))?.metadata()?;
        if metadata.file_type().is_file() && (metadata.dev(), metadata.ino()) == (held.dev(), held.ino()) { Ok(()) }
        else { Err(io::Error::new(io::ErrorKind::PermissionDenied, "codec workspace file was replaced")) }
    }
    fn check_owned_file(&self) -> io::Result<()> {
        Self::check_identity(&self.file, self.identity.as_ref())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        let file_result = if self.identity.is_some() {
            match self.check_owned_file() {
                Ok(()) => fs::remove_file(&self.file),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        } else { Ok(()) };
        let auxiliary_result = if let Some((path, identity)) = &self.auxiliary {
            match Self::check_identity(path, Some(identity)) {
                Ok(()) => fs::remove_file(path),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        } else { Ok(()) };
        let directory_result = match fs::remove_dir(&self.directory) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        file_result.and(auxiliary_result).and(directory_result)
    }
}
impl Drop for Workspace { fn drop(&mut self) { let _ = self.cleanup(); } }
fn with_workspace_cleanup(primary: CodecError, workspace: &mut Workspace) -> CodecError {
    match workspace.cleanup() {
        Ok(()) => primary,
        Err(source) => CodecError::Cleanup { primary: Box::new(primary), source },
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, CodecError> {
    if path.as_os_str().as_encoded_bytes().is_empty() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(CodecError::InvalidInput("path is empty or contains NUL"));
    }
    if path.is_absolute() { Ok(path.to_owned()) }
    else { std::env::current_dir().map(|dir| dir.join(path)).map_err(|e| io_error("resolve output", path, e)) }
}

fn extended_rate(rate: f64) -> Result<[u8; 10], CodecError> {
    let bits = rate.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    if !rate.is_finite() || rate <= 0.0 || exp == 0 {
        return Err(CodecError::InvalidInput("sample rate must be positive, finite, and normal"));
    }
    let exponent = (exp - 1023 + 16_383) as u16;
    let mantissa = ((bits & ((1u64 << 52) - 1)) | (1u64 << 52)) << 11;
    let mut result = [0u8; 10];
    result[..2].copy_from_slice(&exponent.to_be_bytes());
    result[2..].copy_from_slice(&mantissa.to_be_bytes());
    Ok(result)
}

fn pcm_header(config: WriterConfig, frames: u64, data_bytes: u64) -> Result<[u8; 4096], CodecError> {
    let bits = config.bit_depth.max(16);
    let size = data_bytes.checked_add(4088).and_then(|n| u32::try_from(n).ok())
        .ok_or(CodecError::InvalidInput("PCM container exceeds its 32-bit size"))?;
    let bytes = u32::try_from(data_bytes).map_err(|_| CodecError::InvalidInput("PCM data exceeds its 32-bit size"))?;
    let channels = u16::try_from(config.channels).map_err(|_| CodecError::InvalidInput("too many PCM channels"))?;
    let frame_bytes = config.channels.checked_mul(bits / 8).and_then(|n| u16::try_from(n).ok())
        .ok_or(CodecError::InvalidInput("PCM frame size exceeds u16"))?;
    let mut header = [0u8; 4096];
    match config.format {
        OutputFormat::Wav => {
            if !config.sample_rate.is_finite() || config.sample_rate < 1.0 || config.sample_rate >= 4_294_967_296.0 {
                return Err(CodecError::InvalidInput("WAVE sample rate is outside its 32-bit range"));
            }
            let rate = config.sample_rate as u32;
            let byte_rate = rate.checked_mul(u32::from(frame_bytes)).ok_or(CodecError::InvalidInput("WAVE byte rate overflows"))?;
            header[..4].copy_from_slice(b"RIFF"); header[4..8].copy_from_slice(&size.to_le_bytes());
            header[8..16].copy_from_slice(b"WAVEfmt "); header[16..20].copy_from_slice(&16u32.to_le_bytes());
            header[20..22].copy_from_slice(&1u16.to_le_bytes()); header[22..24].copy_from_slice(&channels.to_le_bytes());
            header[24..28].copy_from_slice(&rate.to_le_bytes()); header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
            header[32..34].copy_from_slice(&frame_bytes.to_le_bytes()); header[34..36].copy_from_slice(&(bits as u16).to_le_bytes());
            header[36..40].copy_from_slice(b"FLLR"); header[40..44].copy_from_slice(&4044u32.to_le_bytes());
            header[4088..4092].copy_from_slice(b"data"); header[4092..4096].copy_from_slice(&bytes.to_le_bytes());
        }
        OutputFormat::Aiff => {
            let count = u32::try_from(frames).map_err(|_| CodecError::InvalidInput("AIFF frame count overflows"))?;
            let chunk_size = bytes.checked_add(8).ok_or(CodecError::InvalidInput("AIFF sound chunk overflows"))?;
            header[..4].copy_from_slice(b"FORM"); header[4..8].copy_from_slice(&size.to_be_bytes());
            header[8..16].copy_from_slice(b"AIFFCOMM"); header[16..20].copy_from_slice(&18u32.to_be_bytes());
            header[20..22].copy_from_slice(&channels.to_be_bytes()); header[22..26].copy_from_slice(&count.to_be_bytes());
            header[26..28].copy_from_slice(&(bits as u16).to_be_bytes()); header[28..38].copy_from_slice(&extended_rate(config.sample_rate)?);
            header[38..42].copy_from_slice(b"FLLR"); header[42..46].copy_from_slice(&4034u32.to_be_bytes());
            header[4080..4084].copy_from_slice(b"SSND"); header[4084..4088].copy_from_slice(&chunk_size.to_be_bytes());
        }
        _ => return Err(CodecError::UnsupportedFormat("not a PCM recorder")),
    }
    Ok(header)
}

// WAVE's native-endian 16/32 converter rounds to nearest, ties away from
// zero. The swapped and 24-bit converters use a biased signed 32-bit
// intermediate before discarding the low bits. The power-of-two product is
// exact in f64 for every finite f32, so separate operations equal the observed
// fused multiply-add without depending on a platform libm.
fn quantize(sample: f32, format: OutputFormat, bits: u32) -> i32 {
    if format == OutputFormat::Wav && bits != 24 {
        let scale = (1u64 << (bits - 1)) as f64;
        let value = f64::from(sample) * scale;
        let rounded = if value < 0.0 { value - 0.5 } else { value + 0.5 };
        let integer = rounded as i64;
        let max = (1i64 << (bits - 1)) - 1;
        return integer.clamp(-max - 1, max) as i32;
    }
    let bias = if bits == 32 { 0.0 } else { (1u64 << (31 - bits)) as f64 };
    let value = f64::from(sample) * 2_147_483_648.0 + bias;
    if value >= 2_147_483_647.0 { return ((1i64 << (bits - 1)) - 1) as i32; }
    if value <= -2_147_483_648.0 { return (-(1i64 << (bits - 1))) as i32; }
    let integer = (value as i32) - i32::from(value < 0.0);
    integer >> (32 - bits)
}

fn alac_config(config: WriterConfig) -> Result<alac::Config, CodecError> {
    if !(1..=8).contains(&config.channels) || !config.sample_rate.is_finite() ||
        !(22_050.0..=96_000.0).contains(&config.sample_rate) {
        return Err(CodecError::UnsupportedFormat("ALAC output channel count or sample rate is not verified"));
    }
    Ok(alac::Config { frame_length: 4096, bit_depth: config.bit_depth.max(16) as u8,
        pb: 40, mb: 10, kb: 14, channels: config.channels as u8, sample_rate: config.sample_rate as u32 })
}

struct AlacWriteState {
    config: alac::Config,
    stream: alac::Stream,
    spool: File,
    spool_path: PathBuf,
    quantized: Vec<i32>,
    packets: Vec<mp4::EncodedPacket>,
    /// Set when the stream is closed: AudioToolbox's tally over all encode attempts, for the cookie.
    stats: alac::AttemptStats,
}
impl AlacWriteState {
    fn new(config: alac::Config, workspace: &mut Workspace) -> Result<Self, CodecError> {
        let stream = alac::Stream::new(config).map_err(CodecError::AlacEncode)?;
        let spool = workspace.open_spool(".pn-alac-packets", "create ALAC packet spool", "pin ALAC packet spool")?;
        let spool_path = workspace.auxiliary.as_ref().expect("created packet spool").0.clone();
        Ok(Self { config, stream, spool, spool_path, quantized: Vec::new(), packets: Vec::new(), stats: alac::AttemptStats::default() })
    }
    /// Append one packet of the stream to the spool and the packet table.
    fn store(spool: &mut File, spool_path: &Path, packets: &mut Vec<mp4::EncodedPacket>, packet: &[u8], frames: u32) -> Result<(), CodecError> {
        let size = u32::try_from(packet.len()).map_err(|_| CodecError::InvalidInput("ALAC packet exceeds u32"))?;
        spool.write_all(packet).map_err(|e| io_error("write ALAC packet spool", spool_path, e))?;
        packets.push(mp4::EncodedPacket { size, frames });
        Ok(())
    }
    /// One write call. AudioToolbox encodes (and, for an oversized packet, re-encodes) per call, so the call boundary is
    /// handed to the stream; the audio itself is quantized and queued a packet at a time.
    fn write(&mut self, input: &[f32]) -> Result<(), CodecError> {
        if input.is_empty() { return Ok(()); }
        let Self { config, stream, spool, spool_path, quantized, packets, .. } = self;
        let mut emit = |packet: &[u8], frames: u32| Self::store(spool, spool_path, packets, packet, frames);
        stream.begin_call();
        for chunk in input.chunks(4096 * usize::from(config.channels)) {
            quantized.resize(chunk.len(), 0);
            alac::quantize_into(chunk, config.bit_depth, quantized).map_err(CodecError::AlacEncode)?;
            stream.push(quantized, &mut emit).map_err(Self::error)?;
        }
        Ok(())
    }
    /// Close the stream: the last packet, and the finalization packet after an exact multiple of 4096 frames.
    fn finish(&mut self) -> Result<(), CodecError> {
        let Self { stream, spool, spool_path, packets, stats, .. } = self;
        *stats = stream.finish(&mut |packet: &[u8], frames: u32| Self::store(spool, spool_path, packets, packet, frames)).map_err(Self::error)?;
        Ok(())
    }
    fn error(error: alac::StreamError<CodecError>) -> CodecError {
        match error { alac::StreamError::Encode(e) => CodecError::AlacEncode(e), alac::StreamError::Sink(e) => e }
    }
}

/// OSStatus values AudioToolbox answers for an AAC output its converter cannot serve: `'fmt?'` (no such format), `paramErr`, and `'!dat'` (`'brat'` 256000
/// is above the converter's maximum for the format).
const FORMAT_NOT_SUPPORTED: i32 = 0x666d_743f;
const PARAM_ERROR: i32 = -50;
const BIT_RATE_REFUSED: i32 = 0x2164_6174;
const CLIENT_FORMAT: &str = "ExtAudioFileSetProperty(client format)";

/// What the macOS writer does with an AAC output of this rate and channel count. Measured on macOS: every integer rate from 1 to 200,000 Hz on 1 to 8
/// channels, the edges of the rate windows, the grid in `tests/fixtures/aac-encode`, and the region around 3,000,000 Hz (2,999,998 to 3,000,001 Hz, the
/// neighbouring doubles of 3,000,000, and 250,000 and 1,000,000 Hz, on 0 to 1,025 channels; the `create` command of `encode_probe` re-measures it).
/// AudioToolbox serves the request as one of nine AAC sample rates when it lies within 10 Hz of it and the channel count is 1 to 8, and takes `'brat'`
/// 256000 for the formats listed at [`aac::encoder::accepts`]. Everything else is a native failure: of `ExtAudioFileCreateWithURL` (`'fmt?'` for a rate
/// above 3,000,000 Hz) or of the client-format property (`paramErr` for exactly 3,000,000 Hz and for more than 1,024 channels, `'fmt?'` otherwise).
enum AacPlan {
    /// An exact AAC rate and channel count the encoder reproduces.
    Encode { rate: u32 },
    /// The converter exists but took no 256 kbit/s: the macOS writer carries a `'!dat'` warning, its first write of a frame fails with that status and a stream of no
    /// frames is its default-bitrate empty file.
    BitrateRefused { rate: u32 },
}

fn aac_plan(rate: f64, channels: u32) -> Result<AacPlan, CodecError> {
    let native = |operation, status| CodecError::Native { operation, status };
    if rate > 3_000_000.0 { return Err(native("ExtAudioFileCreateWithURL", FORMAT_NOT_SUPPORTED)); }
    // exactly 3,000,000 Hz fails the client format with paramErr, whatever the channel count; below it only more than 1,024 channels does
    if rate == 3_000_000.0 || channels > 1024 { return Err(native(CLIENT_FORMAT, PARAM_ERROR)); }
    let Some(nominal) = aac::encoder::nominal_rate(rate).filter(|_| channels <= aac::encoder::MAX_CHANNELS) else {
        return Err(native(CLIENT_FORMAT, FORMAT_NOT_SUPPORTED));
    };
    // AudioToolbox creates the converter for a rate near an AAC rate and rounds the file to it, but its access units follow the exact rate
    if rate != f64::from(nominal) {
        return Err(CodecError::UnsupportedFormat(if rate.fract() != 0.0 { "fractional AAC sample rates are not reproduced" }
            else { "AAC output is reproduced only at the exact sample rates 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100 and 48000 Hz" }));
    }
    match aac::encoder::accepts(nominal, channels) {
        Ok(()) => Ok(AacPlan::Encode { rate: nominal }),
        Err(aac::encoder::ConfigError::Refused { .. }) => Ok(AacPlan::BitrateRefused { rate: nominal }),
        Err(_) => Err(CodecError::UnsupportedFormat("AAC encoder configuration")),
    }
}

/// Frames per slice of an AAC write (16 blocks of 1024): the most the encoder's queue holds before it is drained.
const AAC_WRITE_SLICE_FRAMES: usize = 16 * 1024;

/// AAC output: the encoder, and its access units spooled to the workspace as they appear (only their sizes stay in memory) until the container can be written.
struct AacWriteState {
    rate: u32,
    channels: u32,
    /// `None` for a converter that refused 256 kbit/s.
    stream: Option<AacStream>,
}
struct AacStream {
    encoder: aac::encoder::AacEncoder,
    spool: File,
    spool_path: PathBuf,
    /// the size of every access unit spooled so far
    au_sizes: Vec<u32>,
    /// the bytes spooled so far
    bytes: u64,
    /// test hook: the spool write of the access unit with this index fails with ENOSPC
    #[cfg(test)]
    spool_fault_at: Option<usize>,
}
impl AacStream {
    /// Move the access units the encoder has finished to the spool; a stream the container cannot hold fails as soon as it grows past the limit.
    fn spool_units(&mut self, rate: u32) -> Result<(), CodecError> {
        #[cfg(test)]
        let fault_at = self.spool_fault_at;
        let Self { encoder, spool, spool_path, au_sizes, bytes, .. } = self;
        encoder.drain_access_units(|unit| {
            let size = u32::try_from(unit.len()).map_err(|_| CodecError::AacContainer(aac::mp4::Mp4Error::FileTooLarge))?;
            #[cfg(test)]
            if fault_at == Some(au_sizes.len()) {
                return Err(io_error("write AAC access-unit spool", spool_path, io::Error::from_raw_os_error(28)));
            }
            spool.write_all(unit).map_err(|e| io_error("write AAC access-unit spool", spool_path, e))?;
            au_sizes.push(size);
            *bytes += u64::from(size);
            aac::mp4::check_limits(rate, au_sizes.len() as u64, *bytes).map_err(CodecError::AacContainer)
        })
    }
}
impl AacWriteState {
    fn new(plan: AacPlan, channels: u32, workspace: &mut Workspace) -> Result<Self, CodecError> {
        match plan {
            AacPlan::BitrateRefused { rate } => Ok(Self { rate, channels, stream: None }),
            AacPlan::Encode { rate } => {
                let encoder = aac::encoder::AacEncoder::new(rate, channels).map_err(|_| CodecError::UnsupportedFormat("AAC encoder configuration"))?;
                let spool = workspace.open_spool(".pn-aac-units", "create AAC access-unit spool", "pin AAC access-unit spool")?;
                let spool_path = workspace.auxiliary.as_ref().expect("created access-unit spool").0.clone();
                Ok(Self { rate, channels, stream: Some(AacStream { encoder, spool, spool_path, au_sizes: Vec::new(), bytes: 0,
                    #[cfg(test)] spool_fault_at: None }) })
            }
        }
    }
    /// `Some` for the converter that refused 256 kbit/s: the status of its failures.
    fn refusal(&self) -> Option<i32> { self.stream.is_none().then_some(BIT_RATE_REFUSED) }
    /// Encode complete interleaved frames (not none), a slice at a time: the encoder's queue is drained into the spool after every slice, so the access
    /// units waiting in memory stay bounded by the slice however large the buffer of the caller is.
    fn write(&mut self, input: &[f32]) -> Result<(), CodecError> {
        let rate = self.rate;
        let Some(stream) = &mut self.stream else {
            return Err(CodecError::Native { operation: "ExtAudioFileWrite", status: BIT_RATE_REFUSED });
        };
        // the slices are the ones the encoder cuts its input into itself: the access units do not depend on them
        for part in input.chunks(AAC_WRITE_SLICE_FRAMES * self.channels as usize) {
            stream.encoder.push_interleaved(part).map_err(CodecError::AacEncode)?;
            stream.spool_units(rate)?;
        }
        Ok(())
    }
    /// Finish the stream and write the whole `.m4a` into `file` (the staged output, empty so far) with the close-time clock.
    fn finish(&mut self, workspace: &Workspace, file: &mut File, frames: u64) -> Result<(), CodecError> {
        let clock = mp4_close_time()?;
        let output = &workspace.file;
        file.seek(SeekFrom::Start(0)).map_err(|e| io_error("seek AAC output", output, e))?;
        let mut out = io::BufWriter::with_capacity(256 * 1024, &mut *file);
        let (rate, channels) = (self.rate, self.channels);
        let (result, spool_path) = match &mut self.stream {
            Some(stream) => {
                stream.encoder.finish().map_err(CodecError::AacEncode)?;
                stream.spool_units(rate)?;
                let (path, identity) = workspace.auxiliary.as_ref().expect("AAC spool identity");
                Workspace::check_identity(path, Some(identity)).map_err(|e| io_error("inspect AAC access-unit spool", path, e))?;
                stream.spool.seek(SeekFrom::Start(0)).map_err(|e| io_error("rewind AAC access-unit spool", &stream.spool_path, e))?;
                (aac::encoder::m4a::write_file(&mut out, &mut stream.spool, &stream.au_sizes, rate, channels, frames, clock), stream.spool_path.clone())
            }
            None => (aac::encoder::m4a::write_file(&mut out, &mut io::empty(), &[], rate, channels, frames, clock), PathBuf::new()),
        };
        result.map_err(|error| match error {
            aac::mp4::WriteError::Layout(e) => CodecError::AacContainer(e),
            aac::mp4::WriteError::Output(e) => io_error("write AAC output", output, e),
            aac::mp4::WriteError::Source(e) => io_error("read AAC access-unit spool", &spool_path, e),
        })?;
        out.flush().map_err(|e| io_error("write AAC output", output, e))?;
        drop(out);
        file.sync_all().map_err(|e| io_error("sync AAC output", output, e))
    }
}

fn mp4_close_time() -> Result<u32, CodecError> {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| CodecError::InvalidInput("system clock precedes the Unix epoch"))?.as_secs();
    unix.checked_add(2_082_844_800).and_then(|n| u32::try_from(n).ok())
        .ok_or(CodecError::InvalidInput("MP4 clock is outside its 32-bit range"))
}

fn invalid_mp4(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn mp4_ranges(data: &[u8]) -> io::Result<Vec<([u8; 4], std::ops::Range<usize>)>> {
    let mut result = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let rest = &data[offset..];
        if rest.len() < 8 { return Err(invalid_mp4("truncated MP4 atom")); }
        let kind = rest[4..8].try_into().expect("four-byte atom type");
        let (size, header) = match be32(&rest[..4]) {
            0 => (rest.len(), 8usize),
            1 => {
                if rest.len() < 16 { return Err(invalid_mp4("truncated extended MP4 atom")); }
                let size = u64::from_be_bytes(rest[8..16].try_into().expect("eight-byte atom size"));
                (usize::try_from(size).map_err(|_| invalid_mp4("MP4 atom too large"))?, 16)
            }
            n => (n as usize, 8),
        };
        if size < header || size > rest.len() { return Err(invalid_mp4("invalid MP4 atom size")); }
        result.push((kind, offset + header..offset + size));
        offset += size;
    }
    Ok(result)
}
fn only_mp4_range(data: &[u8], kind: &[u8; 4]) -> io::Result<std::ops::Range<usize>> {
    let mut matches = mp4_ranges(data)?.into_iter().filter(|(tag, _)| tag == kind).map(|(_, range)| range);
    match (matches.next(), matches.next()) {
        (Some(range), None) => Ok(range),
        (None, _) => Err(invalid_mp4("required MP4 atom missing")),
        _ => Err(invalid_mp4("duplicate MP4 atom")),
    }
}
fn patch_mp4_full_box(data: &mut [u8], seconds: u32) -> io::Result<()> {
    match data.first() {
        Some(0) if data.len() >= 12 => {
            data[4..8].copy_from_slice(&seconds.to_be_bytes());
            data[8..12].copy_from_slice(&seconds.to_be_bytes());
        }
        Some(1) if data.len() >= 20 => {
            data[4..12].copy_from_slice(&u64::from(seconds).to_be_bytes());
            data[12..20].copy_from_slice(&u64::from(seconds).to_be_bytes());
        }
        _ => return Err(invalid_mp4("unsupported MP4 time header")),
    }
    Ok(())
}

/// Set creation and modification times on mvhd and every track's tkhd/mdhd.
/// This is the diagnostic controlled-clock operation used after MP4 encoding.
/// Atom structure is checked before the modified movie atom is written.
pub fn set_mp4_header_times(path: &Path, seconds_since_1904: u32) -> io::Result<usize> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    patch_mp4_times_file(&mut file, seconds_since_1904)
}
fn patch_mp4_times_file(file: &mut File, seconds_since_1904: u32) -> io::Result<usize> {
    const MAX_MOOV: u64 = 64 * 1024 * 1024;
    let length = file.metadata()?.len();
    let mut offset = 0u64;
    let (start, size) = loop {
        if length.saturating_sub(offset) < 8 { return Err(invalid_mp4("MP4 movie atom missing")); }
        let mut header = [0u8; 16];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut header[..8])?;
        let (size, header_size) = match be32(&header[..4]) {
            0 => (length - offset, 8u64),
            1 => {
                file.read_exact(&mut header[8..])?;
                (u64::from_be_bytes(header[8..16].try_into().expect("eight-byte atom size")), 16)
            }
            n => (u64::from(n), 8),
        };
        if size < header_size || size > length - offset { return Err(invalid_mp4("invalid MP4 top-level atom")); }
        if &header[4..8] == b"moov" { break (offset + header_size, size - header_size); }
        offset += size;
    };
    if size > MAX_MOOV { return Err(invalid_mp4("MP4 movie atom too large")); }
    let mut moov = vec![0u8; size as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut moov)?;
    let mvhd = only_mp4_range(&moov, b"mvhd")?;
    patch_mp4_full_box(&mut moov[mvhd], seconds_since_1904)?;
    let tracks: Vec<_> = mp4_ranges(&moov)?.into_iter().filter(|(tag, _)| tag == b"trak").map(|(_, range)| range).collect();
    if tracks.is_empty() { return Err(invalid_mp4("MP4 contains no track")); }
    for range in &tracks {
        let track = &mut moov[range.clone()];
        let tkhd = only_mp4_range(track, b"tkhd")?;
        patch_mp4_full_box(&mut track[tkhd], seconds_since_1904)?;
        let mdia = only_mp4_range(track, b"mdia")?;
        let media = &mut track[mdia];
        let mdhd = only_mp4_range(media, b"mdhd")?;
        patch_mp4_full_box(&mut media[mdhd], seconds_since_1904)?;
    }
    file.seek(SeekFrom::Start(start))?;
    file.write_all(&moov)?;
    file.sync_all()?;
    Ok(2 + tracks.len() * 4)
}

/// Synchronous WAV, AIFF, ALAC and AAC writer with no-replace publication.
///
/// WAVE/AIFF comparisons cover 16/24/32-bit files, 1/2/6 channels, all six
/// requested rates, boundary frames and random float inputs. The extended
/// matrix covers channels 1..=8, depth 0/16/24/32 and selected fractional
/// rates and other rates from 8,000 to 192,000. ALAC output accepts finite
/// rates 22,050..=96,000 and 1..=8 channels: reservation was checked for every
/// integer in that interval, while fractional rates and full-file contents
/// were sampled across both clock modes, bit depths and packet boundaries.
/// See the `audio-01` and `alac-01` receipts for exact coverage. Unverified
/// ALAC rates are refused; oversized packets fail closed.
///
/// AAC output is the encoder of [`aac::encoder`] and its `.m4a` container: the
/// file AudioToolbox's `ExtAudioFile` writes for the engine's call sequence
/// (`'brat'` 256000, write class A, generic tail), byte for byte except for the
/// close-time clock. Every write size gives the same file, and a caller that
/// writes in flush-to-zero mode gets AudioToolbox's file for that mode
/// (subnormal input samples are flushed). The access units are
/// spooled to the private workspace as they appear (a write of any size is
/// encoded and drained a slice at a time) and the container is
/// written when the writer finishes (its `moov` in front, or behind `mdat` for
/// streams of more than about 300 seconds), so memory stays bounded; a stream
/// that needs 64-bit offsets or a 64-bit duration fails closed. The spool and
/// the staged output live in the same directory as the destination and exist
/// side by side while the container is written, so the filesystem needs room
/// for about twice the size of the stream. Finishing copies the spool into the
/// container and cannot be cancelled in portable builds (the engine's cancel
/// flag is not consulted between `finish` and the publication). Where the macOS
/// writer was measured (see [`AacPlan`]), this writer answers as it does: the
/// same native status where AudioToolbox cannot create the converter, the same
/// `AacBitRateNotSet` warning (status `'!dat'`) where it takes no 256 kbit/s
/// (its first write then fails with that status, as on macOS, and a writer that
/// never writes still publishes Apple's default-bitrate empty file). Rates that
/// AudioToolbox serves but rounds, including every fractional rate, are refused
/// with [`CodecError::UnsupportedFormat`]: its access units follow the exact rate.
pub struct Writer {
    file: Option<File>,
    workspace: Workspace,
    target: PathBuf,
    config: WriterConfig,
    warnings: Vec<CodecWarning>,
    frames: u64,
    data_bytes: u64,
    alac: Option<AlacWriteState>,
    aac: Option<AacWriteState>,
    failed: bool,
}
impl Writer {
    pub fn create(path: impl AsRef<Path>, config: WriterConfig) -> Result<Self, CodecError> {
        Self::create_impl(path, config, #[cfg(test)] false)
    }
    fn create_impl(path: impl AsRef<Path>, config: WriterConfig,
        #[cfg(test)] fail_after_create: bool) -> Result<Self, CodecError> {
        if config.format == OutputFormat::Aac {
            // the order of the macOS writer's file description: rate, channel count (its checks come before the recorder depth), then the depth
            if !config.sample_rate.is_finite() || config.sample_rate <= 0.0 {
                return Err(CodecError::InvalidInput("sample rate must be positive and finite"));
            }
            if config.channels.checked_mul(4).is_none_or(|bytes| bytes == 0) { return Err(CodecError::InvalidInput("invalid channel count")); }
        }
        if !matches!(config.bit_depth, 0 | 16 | 24 | 32) {
            return Err(CodecError::InvalidInput("recorder bit depth must be 0, 16, 24 or 32"));
        }
        if config.channels == 0 { return Err(CodecError::InvalidInput("invalid channel count")); }
        let alac_config = if config.format == OutputFormat::Alac { Some(alac_config(config)?) } else { None };
        let header = if matches!(config.format, OutputFormat::Wav | OutputFormat::Aiff) { Some(pcm_header(config, 0, 0)?) } else { None };
        let target = absolute_path(path.as_ref())?;
        match fs::symlink_metadata(&target) {
            Ok(_) => return Err(io_error("create new output", &target, io::Error::new(io::ErrorKind::AlreadyExists, "output already exists"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error("inspect output", &target, e)),
        }
        let mut workspace = Workspace::create(&target)?;
        let aac_plan = if config.format == OutputFormat::Aac {
            match aac_plan(config.sample_rate, config.channels) {
                Ok(plan) => Some(plan),
                Err(error) => return Err(with_workspace_cleanup(error, &mut workspace)),
            }
        } else { None };
        let mut file = match workspace.open_file() {
            Ok(file) => file, Err(error) => return Err(with_workspace_cleanup(error, &mut workspace)),
        };
        #[cfg(test)] if fail_after_create {
            drop(file);
            return Err(with_workspace_cleanup(CodecError::InvalidInput("injected codec initialization failure"), &mut workspace));
        }
        if let Some(header) = header {
            if let Err(error) = file.write_all(&header) {
                drop(file);
                return Err(with_workspace_cleanup(io_error("write PCM header", &workspace.file, error), &mut workspace));
            }
        }
        let alac = match alac_config.map(|config| AlacWriteState::new(config, &mut workspace)).transpose() {
            Ok(alac) => alac, Err(error) => { drop(file); return Err(with_workspace_cleanup(error, &mut workspace)); }
        };
        let aac = match aac_plan.map(|plan| AacWriteState::new(plan, config.channels, &mut workspace)).transpose() {
            Ok(aac) => aac, Err(error) => { drop(file); return Err(with_workspace_cleanup(error, &mut workspace)); }
        };
        let warnings = if aac.as_ref().and_then(AacWriteState::refusal).is_some() {
            vec![CodecWarning::AacBitRateNotSet { status: BIT_RATE_REFUSED }]
        } else { Vec::new() };
        Ok(Self { file: Some(file), workspace, target, config, warnings, frames: 0, data_bytes: 0, failed: false, alac, aac })
    }
    pub fn config(&self) -> WriterConfig { self.config }
    pub fn warnings(&self) -> &[CodecWarning] { &self.warnings }
    /// Append complete interleaved float32 frames.
    pub fn write(&mut self, input: &[f32]) -> Result<(), CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        let channels = self.config.channels as usize;
        if input.len() % channels != 0 { return Err(CodecError::InvalidInput("buffer does not contain whole interleaved frames")); }
        if input.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) { return Err(CodecError::InvalidInput("buffer is too large")); }
        let frames = self.frames.checked_add((input.len() / channels) as u64).ok_or(CodecError::InvalidInput("frame count overflows"))?;
        if let Some(aac) = &mut self.aac {
            // like the macOS writer, an empty buffer never reaches the encoder
            if input.is_empty() { return Ok(()); }
            if let Err(error) = aac.write(input) { self.failed = true; return Err(error); }
            self.frames = frames;
            return Ok(());
        }
        if let Some(alac) = &mut self.alac {
            if let Err(error) = alac.write(input) { self.failed = true; return Err(error); }
            self.frames = frames;
            return Ok(());
        }
        let width = self.config.bit_depth.max(16) as usize / 8;
        let bytes = self.data_bytes.checked_add((input.len() as u64) * width as u64).ok_or(CodecError::InvalidInput("PCM data size overflows"))?;
        if bytes > u64::from(u32::MAX) - 4088 {
            return Err(CodecError::InvalidInput("PCM container exceeds its 32-bit size"));
        }
        if self.config.format == OutputFormat::Aiff && frames > u64::from(u32::MAX) {
            return Err(CodecError::InvalidInput("AIFF frame count overflows"));
        }
        let mut buffer = [0u8; 8192];
        let mut used = 0usize;
        let file = self.file.as_mut().expect("live PCM writer");
        let result = (|| -> io::Result<()> {
            for &sample in input {
                if used + width > buffer.len() { file.write_all(&buffer[..used])?; used = 0; }
                let sample = quantize(sample, self.config.format, (width * 8) as u32);
                let encoded = if self.config.format == OutputFormat::Wav { sample.to_le_bytes() } else { sample.to_be_bytes() };
                if self.config.format == OutputFormat::Wav { buffer[used..used + width].copy_from_slice(&encoded[..width]); }
                else { buffer[used..used + width].copy_from_slice(&encoded[4 - width..]); }
                used += width;
            }
            file.write_all(&buffer[..used])
        })();
        if let Err(e) = result {
            self.failed = true;
            return Err(io_error("write PCM data", &self.workspace.file, e));
        }
        self.frames = frames;
        self.data_bytes = bytes;
        Ok(())
    }
    /// The error of a writer that failed before: the macOS writer reports a converter that refused 256 kbit/s as unable to close.
    fn poisoned(&self) -> CodecError {
        match self.aac.as_ref().and_then(AacWriteState::refusal) {
            Some(status) => CodecError::Cleanup { primary: Box::new(CodecError::Poisoned),
                source: io::Error::other(CodecError::Native { operation: "ExtAudioFileDispose", status }) },
            None => CodecError::Poisoned,
        }
    }
    fn finalize_data(&mut self) -> Result<(), CodecError> {
        if self.failed { return Err(self.poisoned()); }
        if let Some(state) = &mut self.aac {
            let file = self.file.as_mut().expect("live AAC writer");
            state.finish(&self.workspace, file, self.frames)?;
        } else if let Some(state) = &mut self.alac {
            state.finish()?;
            let (path, identity) = self.workspace.auxiliary.as_ref().expect("ALAC spool identity");
            Workspace::check_identity(path, Some(identity)).map_err(|e| io_error("inspect ALAC packet spool", path, e))?;
            state.spool.seek(SeekFrom::Start(0)).map_err(|e| io_error("rewind ALAC packet spool", &state.spool_path, e))?;
            let file = self.file.as_mut().expect("live ALAC writer");
            file.seek(SeekFrom::Start(0)).map_err(|e| io_error("seek ALAC output", &self.workspace.file, e))?;
            mp4::write_alac_with_stats(file, &mut state.spool, state.config, self.config.sample_rate, &state.packets, state.stats, 0).map_err(CodecError::Mp4Write)?;
            let clock = mp4_close_time()?;
            patch_mp4_times_file(file, clock).map_err(|e| io_error("set ALAC close time", &self.workspace.file, e))?;
        } else {
            let header = pcm_header(self.config, self.frames, self.data_bytes)?;
            let file = self.file.as_mut().expect("live PCM writer");
            if self.config.format == OutputFormat::Aiff && self.data_bytes & 1 != 0 {
                file.write_all(&[0]).map_err(|e| io_error("pad AIFF data", &self.workspace.file, e))?;
            }
            file.seek(SeekFrom::Start(0)).and_then(|_| file.write_all(&header)).and_then(|_| file.sync_all())
                .map_err(|e| io_error("finalize PCM data", &self.workspace.file, e))?;
        }
        Ok(())
    }
    fn discard_error(&mut self, primary: CodecError) -> CodecError {
        drop(self.file.take());
        drop(self.alac.take());
        drop(self.aac.take());
        match self.workspace.cleanup() {
            Ok(()) => primary,
            Err(source) => CodecError::Cleanup { primary: Box::new(primary), source },
        }
    }
    /// Finalize and publish without replacing any existing destination.
    pub fn finish(self) -> Result<(), CodecError> { self.finish_registered(|_| Ok(())) }
    pub(crate) fn finish_registered(mut self, register: impl FnOnce(&File) -> io::Result<()>) -> Result<(), CodecError> {
        if let Err(error) = self.finalize_data() { return Err(self.discard_error(error)); }
        let file = self.file.take().expect("live codec output");
        drop(self.alac.take());
        drop(self.aac.take());
        if let Err(error) = self.workspace.check_owned_file() {
            let error = io_error("inspect codec output before publication", &self.workspace.file, error);
            return Err(self.discard_error(error));
        }
        if let Err(error) = register(&file) {
            let error = io_error("register codec staging file", &self.target, error);
            return Err(self.discard_error(error));
        }
        if let Err(error) = fs::hard_link(&self.workspace.file, &self.target) {
            let error = io_error("publish new output", &self.target, error);
            return Err(self.discard_error(error));
        }
        self.workspace.cleanup().map_err(|source| CodecError::PublishedButCleanupFailed { path: self.target.clone(), source })?;
        Ok(())
    }
    pub fn abort(mut self) -> Result<(), CodecError> {
        // a converter that refused 256 kbit/s and then failed a write cannot be disposed of: the macOS writer reports that after removing its workspace
        let dispose = if self.failed { self.aac.as_ref().and_then(AacWriteState::refusal) } else { None };
        drop(self.file.take());
        drop(self.alac.take());
        drop(self.aac.take());
        let cleanup = self.workspace.cleanup();
        match dispose {
            Some(status) => {
                let primary = CodecError::Native { operation: "ExtAudioFileDispose", status };
                Err(match cleanup { Ok(()) => primary, Err(source) => CodecError::Cleanup { primary: Box::new(primary), source } })
            }
            None => cleanup.map_err(|e| io_error("remove codec workspace", &self.workspace.directory, e)),
        }
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.file.take());
        drop(self.alac.take());
        drop(self.aac.take());
    }
}

#[cfg(test)]
mod writer_ownership_tests {
    use super::*;

    #[test]
    fn partial_initialization_and_registration_failures_cleanup_all_codecs() {
        let dir = std::env::temp_dir().join(format!("pn-portable-ownership-{}-{}", std::process::id(), NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).unwrap();
        for (format, name) in [(OutputFormat::Wav, "out.wav"), (OutputFormat::Aiff, "out.aiff"), (OutputFormat::Alac, "out.m4a"), (OutputFormat::Aac, "out.m4a")] {
            let path = dir.join(name);
            let config = WriterConfig { format, sample_rate: 48_000.0, channels: 2, bit_depth: 16 };
            assert!(Writer::create_impl(&path, config, true).is_err());
            assert!(!path.exists());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
            let mut writer = Writer::create(&path, config).unwrap();
            writer.write(&vec![0.0; 8192]).unwrap();
            assert!(writer.finish_registered(|_| Err(io::Error::other("injected registration failure"))).is_err());
            assert!(!path.exists());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        }
        fs::remove_dir(dir).unwrap();
    }
}

#[cfg(test)]
mod aac_writer_tests {
    use super::*;

    /// A private directory that is removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("pn-aac-writer-{}-{}", std::process::id(), NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn target(&self) -> PathBuf { self.0.join("out.m4a") }
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        }
        fn assert_empty(&self, what: &str) { assert_eq!(self.names(), Vec::<String>::new(), "{what}"); }
    }
    impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    const CLOCK: u32 = 3_800_000_001;

    fn config(rate: f64, channels: u32) -> WriterConfig { WriterConfig { format: OutputFormat::Aac, sample_rate: rate, channels, bit_depth: 0 } }

    /// Deterministic interleaved test signal: one tone per channel and a little noise.
    fn pcm(frames: usize, channels: usize) -> Vec<f32> {
        let mut state = 0x9e37_79b9u32;
        (0..frames * channels).map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            0.2 * ((i / channels) as f32 * (0.03 + 0.005 * (i % channels) as f32)).sin() + 0.02 * ((state >> 9) as f32 / 8_388_608.0 - 1.0)
        }).collect()
    }

    /// Write `data` in `block`-frame writes and return the published file with its clock fixed.
    fn encode(scratch: &Scratch, config: WriterConfig, data: &[f32], block: usize) -> Vec<u8> {
        let mut writer = Writer::create(scratch.target(), config).unwrap();
        assert!(writer.warnings().is_empty());
        for chunk in data.chunks(block * config.channels as usize) { writer.write(chunk).unwrap(); }
        writer.finish().unwrap();
        assert_eq!(scratch.names(), ["out.m4a"], "a finished writer leaves exactly its target");
        set_mp4_header_times(&scratch.target(), CLOCK).unwrap();
        let bytes = fs::read(scratch.target()).unwrap();
        fs::remove_file(scratch.target()).unwrap();
        bytes
    }

    fn top_level(file: &[u8]) -> Vec<String> {
        let mut kinds = Vec::new();
        let mut at = 0;
        while at < file.len() {
            kinds.push(String::from_utf8_lossy(&file[at + 4..at + 8]).into_owned());
            at += be32(&file[at..at + 4]) as usize;
        }
        assert_eq!(at, file.len());
        kinds
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn native(error: &CodecError) -> Option<(&'static str, i32)> {
        if let CodecError::Native { operation, status } = error { Some((operation, *status)) } else { None }
    }

    #[test]
    fn every_write_size_gives_the_same_file() {
        let scratch = Scratch::new();
        for (rate, channels, frames) in [(44_100.0, 2u32, 5000usize), (48_000.0, 6, 3000)] {
            let data = pcm(frames, channels as usize);
            let reference = encode(&scratch, config(rate, channels), &data, 512);
            assert_eq!(top_level(&reference), ["ftyp", "moov", "free", "mdat"]);
            for block in [1usize, 7, 1025, frames] {
                assert_eq!(encode(&scratch, config(rate, channels), &data, block), reference, "{rate} Hz x {channels} ch in writes of {block} frames");
            }
        }
    }

    #[test]
    fn the_file_describes_exactly_the_frames_written() {
        let scratch = Scratch::new();
        let file = encode(&scratch, config(44_100.0, 2), &pcm(4096, 2), 512);
        let text = String::from_utf8_lossy(&file);
        assert!(text.contains(" 00000000 00000840 000003C0 0000000000001000 "), "iTunSMPB: priming 2112, 960 frames of padding, 4096 valid frames");
        let stsz = file.windows(4).position(|w| w == b"stsz").unwrap();
        assert_eq!(u32_at(&file, stsz + 12), 7, "ceil((4096 + 2112) / 1024) access units");
    }

    #[test]
    fn no_frame_written_publishes_apples_empty_file() {
        let scratch = Scratch::new();
        for (channels, length) in [(1u32, 557usize), (2, 557), (4, 564), (7, 557), (8, 557)] {
            let mut writer = Writer::create(scratch.target(), config(44_100.0, channels)).unwrap();
            writer.write(&[]).unwrap();
            writer.finish().unwrap();
            let file = fs::read(scratch.target()).unwrap();
            assert_eq!(file.len(), length, "{channels} channels");
            assert_eq!(top_level(&file), ["ftyp", "moov"]);
            fs::remove_file(scratch.target()).unwrap();
        }
        scratch.assert_empty("after the empty files");
    }

    #[test]
    fn creation_is_classified_like_the_macos_writer() {
        let scratch = Scratch::new();
        let create = |rate: f64, channels: u32| Writer::create(scratch.target(), config(rate, channels));
        // the measured acceptance grid: 44.1/48 kHz 1..8, 32 kHz 3..8, 24/22.05 kHz 4..8, 16 kHz 7..8 take 256 kbit/s; the rest of the nine rates warn
        for rate in aac::encoder::NOMINAL_RATES {
            for channels in 1..=8u32 {
                let writer = create(f64::from(rate), channels).unwrap();
                let refused = aac::encoder::accepts(rate, channels).is_err();
                let expected: &[CodecWarning] = if refused { &[CodecWarning::AacBitRateNotSet { status: 0x2164_6174 }] } else { &[] };
                assert_eq!(writer.warnings(), expected, "{rate} Hz x {channels} ch");
                writer.abort().unwrap();
                scratch.assert_empty("after an abort");
            }
        }
        // formats with no converter: the client-format property fails with 'fmt?', and for exactly 3,000,000 Hz or above 1,024 channels with paramErr
        for (rate, channels, operation, status) in [
            (7350.0, 2, CLIENT_FORMAT, 0x666d_743f), (64_000.0, 2, CLIENT_FORMAT, 0x666d_743f), (96_000.0, 1, CLIENT_FORMAT, 0x666d_743f), (1.0, 1, CLIENT_FORMAT, 0x666d_743f),
            (44_100.0, 9, CLIENT_FORMAT, 0x666d_743f), (48_000.0, 64, CLIENT_FORMAT, 0x666d_743f), (44_100.0, 1024, CLIENT_FORMAT, 0x666d_743f), (44_100.0, 1025, CLIENT_FORMAT, -50),
            (44_110.5, 2, CLIENT_FORMAT, 0x666d_743f), (43_989.9, 2, CLIENT_FORMAT, 0x666d_743f), (384_000.0, 2, CLIENT_FORMAT, 0x666d_743f),
            (3_000_000.0, 2, CLIENT_FORMAT, -50), (3_000_000.5, 2, "ExtAudioFileCreateWithURL", 0x666d_743f), (1e300, 2, "ExtAudioFileCreateWithURL", 0x666d_743f),
        ] {
            let error = create(rate, channels).err().unwrap_or_else(|| panic!("{rate} Hz x {channels} ch was created"));
            assert_eq!(native(&error), Some((operation, status)), "{rate} Hz x {channels} ch: {error:?}");
            scratch.assert_empty("after a refused create");
        }
        // rates AudioToolbox serves as an AAC rate but encodes at the exact value: refused, never encoded as the nominal rate
        for rate in [44_100.5, 44_099.99999, 44_100.00001, 47_999.999, 8_000.25] {
            assert!(matches!(create(rate, 2), Err(CodecError::UnsupportedFormat(m)) if m.contains("fractional")), "{rate}");
        }
        for rate in [44_101.0, 44_090.0, 47_990.0, 8_010.0] {
            assert!(matches!(create(rate, 2), Err(CodecError::UnsupportedFormat(m)) if m.contains("exact sample rates")), "{rate}");
        }
        scratch.assert_empty("after the refusals");
    }

    /// The cells measured with the macOS writer around its 3,000,000 Hz limit (`encode_probe create`): below the limit a rate with no converter fails the
    /// client-format property with 'fmt?' (paramErr above 1,024 channels); exactly at it the property fails with paramErr whatever the channel count;
    /// above it the create call fails with 'fmt?'.
    #[test]
    fn the_region_around_3_000_000_hz_is_classified_like_the_macos_writer() {
        let scratch = Scratch::new();
        let rates = [250_000.0, 1_000_000.0, 2_999_998.0, 2_999_999.0, 2_999_999.5, 2_999_999.999_999_999_5, 3_000_000.0, 3_000_000.000_000_001,
            3_000_000.000_000_1, 3_000_000.5, 3_000_001.0];
        for rate in rates {
            for channels in [1u32, 2, 3, 4, 7, 8, 9, 16, 1024, 1025] {
                let expected = if rate > 3_000_000.0 { ("ExtAudioFileCreateWithURL", 0x666d_743f) }
                    else if rate == 3_000_000.0 || channels > 1024 { (CLIENT_FORMAT, -50) }
                    else { (CLIENT_FORMAT, 0x666d_743f) };
                let error = Writer::create(scratch.target(), config(rate, channels)).err().unwrap_or_else(|| panic!("{rate} Hz x {channels} ch was created"));
                assert_eq!(native(&error), Some(expected), "{rate} Hz x {channels} ch: {error:?}");
                scratch.assert_empty("after a refused create");
            }
        }
    }

    #[test]
    fn create_checks_run_in_the_macos_order() {
        let scratch = Scratch::new();
        let target = scratch.target();
        let message = |result: Result<Writer, CodecError>| match result { Err(CodecError::InvalidInput(m)) => m, other => panic!("{:?}", other.err()) };
        let depth7 = |rate: f64, channels: u32| WriterConfig { bit_depth: 7, ..config(rate, channels) };
        assert_eq!(message(Writer::create(&target, depth7(f64::NAN, 2))), "sample rate must be positive and finite");
        assert_eq!(message(Writer::create(&target, config(f64::NAN, 0))), "sample rate must be positive and finite");
        assert_eq!(message(Writer::create(&target, config(-1.0, 2))), "sample rate must be positive and finite");
        assert_eq!(message(Writer::create(&target, depth7(44_100.0, 0))), "invalid channel count");
        assert_eq!(message(Writer::create(&target, config(44_100.0, 1 << 30))), "invalid channel count");
        assert_eq!(message(Writer::create(&target, depth7(44_100.0, 2))), "recorder bit depth must be 0, 16, 24 or 32");
        assert_eq!(message(Writer::create("", config(44_100.0, 2))), "path is empty or contains NUL");
        assert_eq!(message(Writer::create("a\0b.m4a", config(44_100.0, 2))), "path is empty or contains NUL");
        // an existing target comes before the converter's verdict, a missing directory before it as well
        fs::write(&target, b"existing").unwrap();
        assert!(matches!(Writer::create(&target, config(192_000.0, 2)), Err(CodecError::Io { operation: "create new output", .. })));
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        fs::remove_file(&target).unwrap();
        assert!(matches!(Writer::create(scratch.0.join("missing").join("out.m4a"), config(192_000.0, 2)), Err(CodecError::Io { operation: "create codec workspace", .. })));
        scratch.assert_empty("after the refusals");
    }

    #[test]
    fn a_converter_that_refuses_256_kbit_fails_its_first_write_like_the_macos_writer() {
        let scratch = Scratch::new();
        let refused = config(32_000.0, 2);
        // the first write of a frame fails with '!dat', then the writer is poisoned (even for an empty write)
        let mut writer = Writer::create(scratch.target(), refused).unwrap();
        assert_eq!(writer.warnings(), [CodecWarning::AacBitRateNotSet { status: 0x2164_6174 }]);
        writer.write(&[]).unwrap();
        let error = writer.write(&[0.0; 4]).unwrap_err();
        assert_eq!(native(&error), Some(("ExtAudioFileWrite", 0x2164_6174)));
        assert!(matches!(writer.write(&[0.0; 4]), Err(CodecError::Poisoned)));
        assert!(matches!(writer.write(&[]), Err(CodecError::Poisoned)));
        // it cannot be disposed of: abort reports that after removing its workspace; finish reports the poisoning with it
        assert!(matches!(writer.abort(), Err(e) if native(&e) == Some(("ExtAudioFileDispose", 0x2164_6174))));
        scratch.assert_empty("after an abort");
        let mut writer = Writer::create(scratch.target(), refused).unwrap();
        writer.write(&[0.0; 4]).unwrap_err();
        match writer.finish() {
            Err(CodecError::Cleanup { primary, source }) => {
                assert!(matches!(*primary, CodecError::Poisoned));
                assert!(source.to_string().contains("ExtAudioFileDispose"), "{source}");
            }
            other => panic!("{other:?}"),
        }
        scratch.assert_empty("after a failed finish");
        // a refused writer that never wrote: abort is clean and finish publishes the empty file with Apple's default bitrate (96 kbit/s for 2 channels at 32 kHz)
        Writer::create(scratch.target(), refused).unwrap().abort().unwrap();
        scratch.assert_empty("after a clean abort");
        Writer::create(scratch.target(), refused).unwrap().finish().unwrap();
        let file = fs::read(scratch.target()).unwrap();
        assert_eq!(top_level(&file), ["ftyp", "moov"]);
        let esds = file.windows(4).position(|w| w == b"esds").unwrap() + 4 + 4 + 5 + 3 + 5 + 2 + 3;
        assert_eq!((u32_at(&file, esds), u32_at(&file, esds + 4)), (96_000, 96_000));
    }

    #[test]
    fn aborted_dropped_and_failed_writers_leave_nothing() {
        let scratch = Scratch::new();
        let data = pcm(3000, 2);
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        writer.write(&data).unwrap();
        assert_eq!(scratch.names().len(), 1, "only the private workspace exists while writing");
        assert!(!scratch.target().exists(), "the target appears only when the writer finishes");
        drop(writer);
        scratch.assert_empty("after a drop");
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        writer.write(&data).unwrap();
        writer.abort().unwrap();
        scratch.assert_empty("after an abort");
        // a registration failure at the last step
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        writer.write(&data).unwrap();
        assert!(writer.finish_registered(|_| Err(io::Error::other("injected"))).is_err());
        scratch.assert_empty("after a failed registration");
        // the encoder's queue is drained into the spool after every write: only sizes stay in memory
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        for chunk in data.chunks(1000) {
            writer.write(chunk).unwrap();
            let stream = writer.aac.as_ref().unwrap().stream.as_ref().unwrap();
            assert_eq!(stream.encoder.pending_access_units(), 0);
            assert_eq!(stream.au_sizes.len() as u64, stream.encoder.frames_pushed() / 1024);
        }
        writer.abort().unwrap();
    }

    #[test]
    fn a_published_destination_is_never_replaced() {
        let scratch = Scratch::new();
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        writer.write(&pcm(2000, 2)).unwrap();
        fs::write(scratch.target(), b"someone else's file").unwrap();
        assert!(matches!(writer.finish(), Err(CodecError::Io { operation: "publish new output", .. })));
        assert_eq!(fs::read(scratch.target()).unwrap(), b"someone else's file");
        assert_eq!(scratch.names(), ["out.m4a"], "the private workspace is gone");
    }

    #[test]
    fn a_swapped_spool_file_is_detected_before_anything_is_published() {
        let scratch = Scratch::new();
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        writer.write(&pcm(3000, 2)).unwrap();
        let path = writer.workspace.auxiliary.as_ref().unwrap().0.clone();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"foreign").unwrap();
        match writer.finish() {
            Err(CodecError::Cleanup { primary, .. }) => assert!(matches!(*primary, CodecError::Io { operation: "inspect AAC access-unit spool", .. }), "{primary:?}"),
            other => panic!("{other:?}"),
        }
        assert!(!scratch.target().exists());
        // the foreign file is not ours to delete: it stays, in the workspace directory
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
    }

    #[test]
    fn a_stream_the_container_cannot_hold_fails_closed_and_poisons_the_writer() {
        let scratch = Scratch::new();
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        writer.write(&pcm(2048, 2)).unwrap();
        // pretend the spool already holds all but a few bytes of what 32-bit offsets can address
        writer.aac.as_mut().unwrap().stream.as_mut().unwrap().bytes = u64::from(u32::MAX) - aac::mp4::data_offset(44_100) - 10;
        let error = writer.write(&pcm(2048, 2)).unwrap_err();
        assert!(matches!(error, CodecError::AacContainer(aac::mp4::Mp4Error::FileTooLarge)), "{error:?}");
        assert!(matches!(writer.write(&pcm(10, 2)), Err(CodecError::Poisoned)));
        assert!(matches!(writer.finish(), Err(CodecError::Poisoned)));
        scratch.assert_empty("after the failed finish");
    }

    #[test]
    fn a_large_write_is_drained_a_slice_at_a_time() {
        let scratch = Scratch::new();
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        // one write of three slices: the queue of the encoder is empty afterwards (it held every unit of the buffer before, 48 of them here)
        let frames = 3 * AAC_WRITE_SLICE_FRAMES + 123;
        writer.write(&pcm(frames, 2)).unwrap();
        let stream = writer.aac.as_ref().unwrap().stream.as_ref().unwrap();
        assert_eq!(stream.encoder.pending_access_units(), 0);
        assert_eq!(stream.au_sizes.len(), frames / 1024);
        writer.abort().unwrap();
        scratch.assert_empty("after the abort");
    }

    fn assert_spool_failure(error: &CodecError) {
        match error {
            CodecError::Io { operation: "write AAC access-unit spool", source, .. } => assert_eq!(source.raw_os_error(), Some(28), "{source}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failing_spool_write_fails_the_writer_cleanly_while_writing_and_while_finishing() {
        // while writing: the third access unit cannot be written
        let scratch = Scratch::new();
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        writer.aac.as_mut().unwrap().stream.as_mut().unwrap().spool_fault_at = Some(2);
        assert_spool_failure(&writer.write(&pcm(8 * 1024, 2)).unwrap_err());
        assert!(matches!(writer.write(&pcm(10, 2)), Err(CodecError::Poisoned)));
        assert!(matches!(writer.finish(), Err(CodecError::Poisoned)));
        scratch.assert_empty("after a spool failure while writing");
        // while finishing: the units of the zero-padded tail and the flush blocks cannot be written (au_count(2500) = 5, two are spooled by the write)
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        writer.write(&pcm(2500, 2)).unwrap();
        assert_eq!(writer.aac.as_ref().unwrap().stream.as_ref().unwrap().au_sizes.len(), 2);
        writer.aac.as_mut().unwrap().stream.as_mut().unwrap().spool_fault_at = Some(3);
        assert_spool_failure(&writer.finish().unwrap_err());
        scratch.assert_empty("after a spool failure while finishing");
        assert!(!scratch.target().exists());
    }

    #[test]
    fn more_access_units_than_the_container_can_count_is_a_typed_error_through_the_writer() {
        let scratch = Scratch::new();
        let mut writer = Writer::create(scratch.target(), config(44_100.0, 2)).unwrap();
        // the 32-bit duration holds 1024 * 4_194_303 frames: pretend that many units are spooled already
        writer.aac.as_mut().unwrap().stream.as_mut().unwrap().au_sizes = vec![0; 4_194_303];
        let error = writer.write(&pcm(2048, 2)).unwrap_err();
        assert!(matches!(error, CodecError::AacContainer(aac::mp4::Mp4Error::TooManyAccessUnits(4_194_304))), "{error:?}");
        assert!(matches!(writer.write(&pcm(10, 2)), Err(CodecError::Poisoned)));
        assert!(matches!(writer.finish(), Err(CodecError::Poisoned)));
        scratch.assert_empty("after the failed finish");
    }

    #[test]
    fn unusable_destinations_fail_at_create_and_leave_nothing_behind() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{symlink, PermissionsExt};
        let scratch = Scratch::new();
        // a dangling symlink is an existing destination: never followed, never replaced
        let link = scratch.0.join("link.m4a");
        symlink(scratch.0.join("nowhere.m4a"), &link).unwrap();
        let error = Writer::create(&link, config(44_100.0, 2)).err().expect("a dangling symlink is taken");
        assert!(matches!(error, CodecError::Io { operation: "create new output", ref source, .. } if source.kind() == io::ErrorKind::AlreadyExists), "{error:?}");
        assert_eq!(fs::read_link(&link).unwrap(), scratch.0.join("nowhere.m4a"));
        assert_eq!(scratch.names(), ["link.m4a"]);
        fs::remove_file(&link).unwrap();
        // a directory that cannot be written to (skipped when the process may write there anyway, e.g. as root)
        let locked = scratch.0.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        if fs::write(locked.join("probe"), b"").is_err() {
            let error = Writer::create(locked.join("out.m4a"), config(44_100.0, 2)).err().expect("a locked directory is refused");
            assert!(matches!(error, CodecError::Io { operation: "create codec workspace", .. }), "{error:?}");
            assert_eq!(fs::read_dir(&locked).unwrap().count(), 0);
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&locked).unwrap();
        // a destination name that is not UTF-8: published under exactly that name where the filesystem takes such names, a typed error otherwise
        let name = std::ffi::OsStr::from_bytes(b"out-\xff\xfe.m4a");
        let target = scratch.0.join(name);
        let supported = fs::write(&target, b"").is_ok();
        if supported { fs::remove_file(&target).unwrap(); }
        let created = Writer::create(&target, config(44_100.0, 2));
        if supported {
            let mut writer = created.unwrap();
            writer.write(&pcm(1500, 2)).unwrap();
            writer.finish().unwrap();
            assert_eq!(fs::read_dir(&scratch.0).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>(), [name.to_owned()]);
            assert_eq!(&fs::read(&target).unwrap()[4..8], b"ftyp");
        } else {
            assert!(matches!(created.err().expect("refused by the filesystem"), CodecError::Io { .. }));
            scratch.assert_empty("after the refused name");
        }
    }
}

/// Float32 CAF intermediate for the healing/final-pass boundary.
///
/// It stores interleaved little-endian float bits unchanged. Its data chunk
/// uses the CAF unknown-length form, so its on-disk layout is not claimed to
/// match AudioToolbox's intermediate bytes. The engine reopens it as audio and
/// observes frame counts and sample bits, not its raw container layout.
pub struct CafWriter {
    file: File,
    path: PathBuf,
    channels: usize,
    failed: bool,
}
impl CafWriter {
    pub fn new(path: impl AsRef<Path>, sample_rate: f64, channels: usize) -> Result<Self, CodecError> {
        let path = path.as_ref().to_owned();
        Self::create_with(&path, sample_rate, channels, || OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path))
    }
    #[cfg(any(not(target_os = "macos"), feature = "portable"))]
    pub(crate) fn from_file(path: &Path, file: File, sample_rate: f64, channels: usize) -> Result<Self, CodecError> {
        Self::create_with(path, sample_rate, channels, || Ok(file))
    }
    fn create_with(path: &Path, sample_rate: f64, channels: usize, open: impl FnOnce() -> io::Result<File>) -> Result<Self, CodecError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(CodecError::InvalidInput("sample rate must be positive and finite"));
        }
        let channels_u32 = u32::try_from(channels).ok().filter(|n| *n != 0)
            .ok_or(CodecError::InvalidInput("invalid channel count"))?;
        let frame_bytes = channels_u32.checked_mul(4).ok_or(CodecError::InvalidInput("CAF frame size overflows"))?;
        let path = path.to_owned();
        let mut file = open().map_err(|e| io_error("create CAF intermediate", &path, e))?;
        let mut header = [0u8; 68];
        header[..4].copy_from_slice(b"caff"); header[4..6].copy_from_slice(&1u16.to_be_bytes());
        header[8..12].copy_from_slice(b"desc"); header[12..20].copy_from_slice(&32u64.to_be_bytes());
        header[20..28].copy_from_slice(&sample_rate.to_bits().to_be_bytes());
        header[28..32].copy_from_slice(b"lpcm"); header[32..36].copy_from_slice(&3u32.to_be_bytes());
        header[36..40].copy_from_slice(&frame_bytes.to_be_bytes()); header[40..44].copy_from_slice(&1u32.to_be_bytes());
        header[44..48].copy_from_slice(&channels_u32.to_be_bytes()); header[48..52].copy_from_slice(&32u32.to_be_bytes());
        header[52..56].copy_from_slice(b"data"); header[56..64].copy_from_slice(&(-1i64).to_be_bytes());
        file.write_all(&header).map_err(|e| io_error("write CAF header", &path, e))?;
        Ok(Self { file, path, channels, failed: false })
    }
    /// Write complete interleaved frames without modifying float bit patterns.
    pub fn write(&mut self, samples: &[f32]) -> Result<(), CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        if samples.len() % self.channels != 0 { return Err(CodecError::InvalidInput("buffer does not contain whole interleaved frames")); }
        if samples.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) {
            return Err(CodecError::InvalidInput("buffer is too large"));
        }
        let mut bytes = [0u8; 8192];
        let result = (|| -> io::Result<()> {
            for chunk in samples.chunks(bytes.len() / 4) {
                for (sample, dest) in chunk.iter().zip(bytes.chunks_exact_mut(4)) {
                    dest.copy_from_slice(&sample.to_bits().to_le_bytes());
                }
                self.file.write_all(&bytes[..chunk.len() * 4])?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.failed = true;
            return Err(io_error("write CAF intermediate", &self.path, error));
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<(), CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        self.file.flush().map_err(|e| io_error("finish CAF intermediate", &self.path, e))
    }
}

/// Same-rate interleaved float32 to stereo signed-16 conversion used before
/// LAME. The engine uses one or two input channels; 1..=64 channel maps were
/// additionally probed. Dither was observed disabled on the tested macOS
/// converter. The sample conversion uses the WAVE/native-endian quantizer.
pub struct Pcm16StereoConverter { input_channels: usize }
impl Pcm16StereoConverter {
    pub fn new(sample_rate: f64, input_channels: u32) -> Result<Self, CodecError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(CodecError::InvalidInput("sample rate must be positive and finite"));
        }
        if !(1..=64).contains(&input_channels) { return Err(CodecError::InvalidInput("invalid channel count")); }
        Ok(Self { input_channels: input_channels as usize })
    }
    pub fn convert(&mut self, input: &[f32], output: &mut [i16]) -> Result<usize, CodecError> {
        if input.len() % self.input_channels != 0 {
            return Err(CodecError::InvalidInput("buffer does not contain whole interleaved frames"));
        }
        if input.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) {
            return Err(CodecError::InvalidInput("input buffer is too large"));
        }
        let frames = input.len() / self.input_channels;
        if frames.checked_mul(2).is_none_or(|n| n > output.len()) {
            return Err(CodecError::InvalidInput("output buffer is too small"));
        }
        if frames.checked_mul(4).is_none_or(|n| n > u32::MAX as usize) {
            return Err(CodecError::InvalidInput("output buffer is too large"));
        }
        for (source, destination) in input.chunks_exact(self.input_channels).zip(output.chunks_exact_mut(2)) {
            destination[0] = quantize(source[0], OutputFormat::Wav, 16) as i16;
            destination[1] = if self.input_channels == 1 { destination[0] }
                else { quantize(source[1], OutputFormat::Wav, 16) as i16 };
        }
        Ok(frames)
    }
    pub fn close(self) -> Result<(), CodecError> { Ok(()) }
}

/// Same-rate conversion from the graph's two planar float32 channels to its
/// source-channel-count interleaved output. The tested 1..=64 channel default
/// mapping takes left for mono, left/right for stereo, and fills additional
/// channels with +0; it does not mix or allocate.
pub struct ChannelConverter { channels: usize }
impl ChannelConverter {
    pub fn new(sample_rate: f64, channels: usize) -> Result<Self, CodecError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(CodecError::InvalidInput("sample rate must be positive and finite"));
        }
        if !(1..=64).contains(&channels) { return Err(CodecError::InvalidInput("invalid channel count")); }
        Ok(Self { channels })
    }
    pub fn convert(&mut self, left: &[f32], right: &[f32], output: &mut [f32]) -> Result<(), CodecError> {
        if left.len() != right.len() || left.len().checked_mul(self.channels) != Some(output.len()) {
            return Err(CodecError::InvalidInput("invalid channel buffer shape"));
        }
        if left.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) ||
            output.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) {
            return Err(CodecError::InvalidInput("channel buffer is too large"));
        }
        for (i, frame) in output.chunks_exact_mut(self.channels).enumerate() {
            frame[0] = left[i];
            if self.channels > 1 { frame[1] = right[i]; frame[2..].fill(0.0); }
        }
        Ok(())
    }
    pub fn dispose(&mut self) -> Result<(), CodecError> { Ok(()) }
}
