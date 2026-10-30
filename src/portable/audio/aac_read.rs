//! AAC-LC input of [`Reader`]: the exact decoder of [`crate::portable::aac::decoder`] behind the `Reader` API, with the first-failure poisoning every
//! codec here has.
//!
//! Which files get here is decided by `Reader::open` (`mp4a` MP4s, ADTS / LOAS streams, CAFs whose `desc` format is an AAC one). Everything AudioToolbox
//! answers with an `OSStatus` is answered with the same status (`ExtAudioFileOpenURL`, `ExtAudioFileSetProperty(client format)`, `ExtAudioFileRead`); what
//! AudioToolbox decodes but the port does not reproduce (HE-AAC, AAC-LD/ELD, MPEG-1 audio object types, a down-mixing channel mismatch, sample-rate
//! conversion) is a typed refusal, never a different result.

use super::*;
use crate::portable::aac::decoder::{AacDecoder, AacFile, ClientError, DecodeError, OpenError, ReadFailure, Unspecified, Unsupported};

/// The largest AAC input that is decoded: the whole file is held in memory (the limit of the MP3 reader).
pub(super) const MAX_AAC_BYTES: u64 = MAX_MP3_BYTES;
const TOO_LARGE: CodecError = CodecError::UnsupportedFormat("AAC input larger than 2 GiB is not supported: the file is decoded from memory");

/// AAC decoding state: the decoder plus the first-failure poisoning.
pub(super) struct AacReadState {
    decoder: AacDecoder,
    pub(super) failed: bool,
}

/// The refusal for a stream AudioToolbox decodes that the port does not reproduce.
fn refusal(reason: Unsupported) -> CodecError {
    CodecError::UnsupportedCodec { codec: "AAC", operation: "decoding", reason: reason.reason() }
}

/// The native call that failed while opening: the container parser (`ExtAudioFileOpenURL`) or the client-format setter, with Apple's status, or a
/// refusal where Apple opens the file.
fn open_error(error: DecodeError) -> CodecError {
    match error {
        DecodeError::Open(e) => CodecError::Native { operation: "ExtAudioFileOpenURL", status: e.os_status() },
        DecodeError::Client(e) => match e.os_status() {
            Some(status) => CodecError::Native { operation: "ExtAudioFileSetProperty(client format)", status },
            None => refusal(match e {
                ClientError::RateMismatch => Unsupported::RateMismatch,
                ClientError::LayeredFormat => Unsupported::LayeredFormat,
                _ => Unsupported::ChannelCountMismatch,
            }),
        },
        DecodeError::Unsupported(reason) => refusal(reason),
        DecodeError::InvalidSampleRate => CodecError::InvalidInput("sample rate must be positive and finite"),
        DecodeError::RateMismatch { .. } => refusal(Unsupported::RateMismatch),
        DecodeError::Config(_) => refusal(Unsupported::Configuration),
    }
}

fn read_error(operation: &'static str, failure: ReadFailure) -> CodecError {
    match failure {
        ReadFailure::Apple(e) => CodecError::Native { operation, status: e.os_status() },
        ReadFailure::NotReproduced(reason) => refusal(reason),
    }
}

/// Bytes the sniffers read at a time (the box headers of a file made of small boxes cost one read per window, not two system calls per box).
const SNIFF_WINDOW: usize = 64 * 1024;
/// Boxes or chunks a sniffer looks at before it gives up (a real file has a handful before its movie atom or its `desc` chunk).
const SNIFF_STEPS: usize = 1 << 20;

/// A window of the file for the sniffers.
pub(super) struct Window<'a> {
    file: &'a mut File,
    start: u64,
    bytes: Vec<u8>,
}

impl<'a> Window<'a> {
    pub(super) fn new(file: &'a mut File) -> Self {
        Self { file, start: 0, bytes: Vec::new() }
    }

    /// `len` bytes at `offset`; `None` when the file ends before them or cannot be read.
    pub(super) fn at(&mut self, offset: u64, len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(len as u64)?;
        if !(self.start <= offset && end <= self.start + self.bytes.len() as u64) {
            self.file.seek(SeekFrom::Start(offset)).ok()?;
            self.bytes.clear();
            self.bytes.resize(SNIFF_WINDOW.max(len), 0);
            let mut filled = 0;
            while filled < self.bytes.len() {
                match self.file.read(&mut self.bytes[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return None,
                }
            }
            self.bytes.truncate(filled);
            self.start = offset;
            if filled < len {
                return None;
            }
        }
        let from = (offset - self.start) as usize;
        Some(&self.bytes[from..from + len])
    }
}

/// Whether the first sound track of an MP4-family file has an `mp4a` sample entry. Any trouble reading the movie atom answers no: the other readers
/// then judge the file as they always did.
pub(super) fn first_sound_entry_is_mp4a(file: &mut File, length: u64) -> bool {
    const MAX_MOOV: u64 = 64 * 1024 * 1024;
    let path = Path::new("");
    let mut window = Window::new(file);
    let mut offset = 0u64;
    for _ in 0..SNIFF_STEPS {
        if length.saturating_sub(offset) < 8 { return false; }
        let Some(header) = window.at(offset, 8) else { return false };
        let kind = [header[4], header[5], header[6], header[7]];
        let (size, header_size) = match be32(&header[..4]) {
            0 => (length - offset, 8u64),
            1 => {
                let Some(wide) = window.at(offset + 8, 8) else { return false };
                (u64::from_be_bytes(wide.try_into().expect("eight bytes")), 16)
            }
            n => (u64::from(n), 8),
        };
        if size < header_size || size >= 1 << 63 { return false; }
        // a movie atom that runs past the end of the file is read as far as the file goes
        let size = size.min(length - offset);
        if size < header_size { return false; }
        if &kind == b"moov" {
            if size > MAX_MOOV { return false; }
            let mut moov = vec![0u8; (size - header_size) as usize];
            if read_at(window.file, offset + header_size, &mut moov, path).is_err() { return false; }
            return crate::portable::aac::decoder::first_sound_entry(&moov).is_some_and(|kind| &kind == b"mp4a");
        }
        offset += size;
    }
    false
}

/// Whether the `desc` chunk of a CAF file names an AAC family format (`aac `, `aach`, `aacp`, `aacl`, `aace`): those are the AAC reader's, any other
/// format stays with the PCM reader.
pub(super) fn caf_format_is_aac(file: &mut File, length: u64) -> bool {
    let mut window = Window::new(file);
    let mut offset = 8u64;
    for _ in 0..SNIFF_STEPS {
        if !offset.checked_add(12).is_some_and(|n| n <= length) { return false; }
        let Some(chunk) = window.at(offset, 12) else { return false };
        let kind = [chunk[0], chunk[1], chunk[2], chunk[3]];
        let raw_size = i64::from_be_bytes(chunk[4..12].try_into().expect("CAF chunk size"));
        let start = offset + 12;
        if &kind == b"desc" {
            return length - start >= 12 && window.at(start + 8, 4).is_some_and(|format| matches!(format, b"aac " | b"aach" | b"aacp" | b"aacl" | b"aace"));
        }
        if raw_size < 0 { return false; }
        match start.checked_add(raw_size as u64) { Some(next) if next <= length => offset = next, _ => return false }
    }
    false
}

/// Whether the file name ends in `.name` (any case).
pub(super) fn has_extension(path: &Path, name: &str) -> bool {
    name_hint(path).as_deref() == Some(name)
}

/// File name extensions that make AudioFile choose its ADTS and LOAS types (`.aac .adts .loas .latm`, any case).
pub(super) fn has_adts_extension(path: &Path) -> bool {
    matches!(name_hint(path).as_deref(), Some("aac" | "adts" | "loas" | "latm"))
}

/// What the AAC container model makes of a file the other readers could not open.
pub(super) enum Verdict {
    /// it is an AAC file after all
    Aac,
    /// the container is damaged in a way AudioToolbox reports whatever the codec is: its status
    Status(CodecError),
    /// the model has no say (not AAC, too large, or a failure that depends on the codec): the caller's own error stands
    Silent,
}

/// Asks the AAC container model about an MP4 or CAF file that failed to open as ALAC or PCM. Structural failures (`'typ?'`, `'dta?'`, `'chk?'`, `'wht?'` for a missing sample table) are
/// the same for every codec, so a damaged ALAC or PCM file gets AudioToolbox's status instead of a description.
pub(super) fn container_verdict(file: &mut File, path: &Path, length: u64) -> Verdict {
    const MAX_INSPECTED: u64 = 256 * 1024 * 1024;
    if length > MAX_INSPECTED { return Verdict::Silent; }
    let mut data = Vec::new();
    if data.try_reserve_exact(length as usize).is_err() || file.seek(SeekFrom::Start(0)).is_err() || (&mut *file).take(length).read_to_end(&mut data).is_err() {
        return Verdict::Silent;
    }
    match AacFile::parse_with_hint(data, name_hint(path).as_deref()) {
        Ok(_) => Verdict::Aac,
        // a sound entry that is no AAC: AudioToolbox answers `'wht?'` for all but the types it opens as other codecs (the ones the other readers judge)
        Err(OpenError::NotMp4a(entry)) if !matches!(&entry, b"alac" | b"ac-3" | b"ec-3" | b"drms" | b"samr" | b"sawb") => Verdict::Status(open_refusal(OpenError::NotMp4a(entry).os_status())),
        Err(e @ (OpenError::InvalidFile | OpenError::UnsupportedFileType | OpenError::InvalidChunk | OpenError::BadMovieChild { .. }
            | OpenError::Unspecified(Unspecified::BadStructure | Unspecified::FileShorterThanAudio))) => {
            Verdict::Status(open_refusal(e.os_status()))
        }
        Err(_) => Verdict::Silent,
    }
}

/// File name extensions that make AudioFile choose its Ogg types (`.ogg .oga .opus`, any case).
pub(super) fn has_ogg_extension(path: &Path) -> bool {
    matches!(name_hint(path).as_deref(), Some("ogg" | "oga" | "opus"))
}

/// The file-name extension AudioFile takes as a type hint (lower case; `None` without one or when it is not UTF-8).
fn name_hint(path: &Path) -> Option<String> {
    path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase)
}

impl Reader {
    /// An earlier codec failure made the stream unusable.
    pub(super) fn poisoned(&self) -> bool {
        self.alac.as_ref().is_some_and(|state| state.failed) || self.mp3.as_ref().is_some_and(|state| state.failed)
            || self.aac.as_ref().is_some_and(|state| state.failed)
    }

    /// A reader over the exact AAC-LC decoder; its source description is the one AudioToolbox reports for AAC files (`'aac '`, no flags, packet and
    /// frame bytes and bit depth zero, 1024 frames per packet).
    pub(super) fn open_aac(mut file: File, path: &Path, length: u64, sample_rate: Option<f64>) -> Result<Self, CodecError> {
        if length > MAX_AAC_BYTES { return Err(TOO_LARGE); }
        let mut data = Vec::new();
        data.try_reserve_exact(length as usize)
            .map_err(|_| io_error("read audio", path, io::ErrorKind::OutOfMemory.into()))?;
        file.seek(SeekFrom::Start(0)).map_err(|e| io_error("read audio", path, e))?;
        // no more than the length says (and one byte to see that there is more): a special file of length 0 that never ends is no 2 GiB read
        (&mut file).take(length + 1).read_to_end(&mut data).map_err(|e| io_error("read audio", path, e))?;
        if data.len() as u64 > length {
            return Err(io_error("read audio", path, io::Error::new(io::ErrorKind::InvalidData, "the file holds more bytes than its length says")));
        }
        Self::open_aac_bytes(file, path, data, name_hint(path).as_deref(), sample_rate)
    }

    /// The part of [`open_aac`](Self::open_aac) behind the file read (the unit tests drive it with in-memory images).
    pub(super) fn open_aac_bytes(file: File, path: &Path, data: Vec<u8>, hint: Option<&str>, sample_rate: Option<f64>) -> Result<Self, CodecError> {
        let decoder = AacDecoder::open(data, hint).map_err(|error| match error {
            // another client rate than the file's is refused as a rate conversion before the stream is refused for the rate it converts inside
            DecodeError::RateMismatch { file_rate } => same_rate(file_rate, sample_rate).err().unwrap_or_else(|| refusal(Unsupported::RateMismatch)),
            other => open_error(other),
        })?;
        let info = decoder.file();

        let source = StreamDescription { sample_rate: info.sample_rate_hz(), format_id: u32::from_be_bytes(info.format_id().fourcc()),
            format_flags: info.format_flags(), bytes_per_packet: 0, frames_per_packet: info.frames_per_packet(), bytes_per_frame: 0,
            channels_per_frame: info.channels(), bits_per_channel: 0, reserved: 0 };
        let rate = same_rate(source.sample_rate, sample_rate)?;
        let client = client_format(rate, source.channels_per_frame, false)?;
        // `frames` and `position` serve the PCM and ALAC paths only; an AAC file answers `file_length_frames` from its container model. Open issues
        // no length query: on AudioToolbox that query scans the file and so may change what later reads see.
        Ok(Self { file, path: path.to_owned(), source, client, encoding: Encoding::Float32 { big_endian: false },
            frames: 0, position: 0, raw: Vec::new(), packets: None, alac: None, mp3: None,
            aac: Some(Box::new(AacReadState { decoder, failed: false })) })
    }

    pub(super) fn aac_file_length_frames(&self) -> Result<i64, CodecError> {
        let state = self.aac.as_ref().expect("AAC state");
        i64::try_from(state.decoder.file_length_frames()).map_err(|_| CodecError::InvalidInput("frame count exceeds i64"))
    }

    /// `AudioFileGetProperty(kAudioFilePropertyBitRate)`.
    pub(super) fn aac_bitrate(&self) -> Result<u32, CodecError> {
        let file = self.aac.as_ref().expect("AAC state").decoder.file();
        // the rate divides by the frames of the packets: a CAF `desc` without frames per packet may have none to divide by (`'dta?'`)
        if !file.bitrate_defined() {
            return Err(CodecError::Native { operation: "AudioFileGetProperty(bit rate)", status: OpenError::InvalidFile.os_status() });
        }
        Ok(file.bitrate())
    }

    /// The client format change of `set_stereo_planar`: Apple's status when the converter cannot be built.
    pub(super) fn aac_set_stereo_planar(&mut self, sample_rate: f64) -> Result<f64, CodecError> {
        let state = self.aac.as_mut().expect("AAC state");
        state.decoder.check_stereo_planar().map_err(open_error)?;
        let rate = same_rate(self.source.sample_rate, Some(sample_rate))?;
        state.decoder.set_stereo_planar().map_err(open_error)?;
        Ok(rate)
    }

    /// One `ExtAudioFileRead`: interleaved at the source channel count, or two planes (mono duplicated, other layouts mixed by the converter). The
    /// frames the decoder wrote behind the returned count (the padding of the last access units) are left in the buffer, as AudioToolbox leaves them.
    pub(super) fn read_aac(&mut self, output: &mut [f32], right: Option<&mut [f32]>) -> Result<usize, CodecError> {
        let state = self.aac.as_mut().expect("AAC state");
        let planar = right.is_some();
        let channels = self.source.channels_per_frame as usize;
        let requested = if planar { output.len() } else { output.len() / channels };
        if requested == 0 { return Ok(0); }
        let read = match right {
            Some(right) => state.decoder.read_stereo_planar(requested, output, right),
            None => state.decoder.read_interleaved(requested, output),
        };
        match read {
            Ok(read) => Ok(read.frames),
            Err(failure) => {
                state.failed = true;
                Err(read_error(if planar { "ExtAudioFileRead(planar stereo)" } else { "ExtAudioFileRead" }, failure))
            }
        }
    }
}

#[cfg(test)]
#[path = "aac_read_tests.rs"]
mod tests;
