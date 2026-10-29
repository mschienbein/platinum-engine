//! Sequential input selected the same way as MIKAudio: paths ending in
//! `flac`, case-insensitively, use its separate native-FLAC path; everything
//! else uses AudioToolbox. The caller supplies a trusted public libFLAC 1.4.2;
//! the original framework is never loaded by this module.
//!
//! MIKAudio converts decoded FLAC integers through a system AudioConverter,
//! rather than an assumed integer-to-float formula. Its FLAC stream-format
//! setter is a no-op: even when a different processing rate is requested, its
//! output remains at the source rate. Its intermediate buffer holds at most two
//! channels. A two-plane read copies both planes: mono's second plane is
//! zero, while multichannel input contributes only its first two channels.
//! For a source with more channels the native interleaved copy packs only the
//! first two channels at the start of the supplied buffer and leaves the rest
//! untouched; neither path is a surround mix.

use crate::apple_codec::{self, StreamDescription};
use crate::flac::{FlacError, FlacLibrary, LibraryIdentity};
use libloading::Library;
use std::error::Error;
use std::ffi::{CStr, CString, OsStr, c_char, c_int, c_void};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};

const LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const MORE: i32 = u32::from_be_bytes(*b"more") as i32;
const END_OF_STREAM: i32 = 4;
const MAX_CHANNELS: usize = 8;
const CONVERTER_FRAMES: usize = 4096;

/// No library is loaded for non-FLAC inputs. Supply a trusted open-source
/// libFLAC 1.4.2 path and the expected full-file SHA-256. A pin does not prove
/// source provenance; the caller is responsible for trusting the actual build.
///
/// `allow_non_identical` lets Linux and `portable` builds decode MP3 with the
/// open-source decoder instead of refusing it. It is ignored by macOS default
/// builds, which always use AudioToolbox.
#[derive(Clone, Debug, Default)]
pub struct InputLibraries {
    pub flac: Option<PathBuf>,
    pub flac_sha256: Option<String>,
    pub allow_non_identical: bool,
}

#[derive(Debug)]
pub enum AudioInputError {
    Apple(apple_codec::CodecError),
    FlacLibrary(FlacError),
    MissingFlacLibrary,
    MissingFlacDigest,
    DigestMismatch { expected: String, actual: String },
    Library(String),
    InvalidInput(&'static str),
    Allocation,
    Decoder { operation: &'static str, result: i32, state: i32 },
    DecoderData(i32),
    Converter { operation: &'static str, status: i32 },
    UnexpectedNativeResult(&'static str),
    Unsupported(&'static str),
    Poisoned,
}
impl fmt::Display for AudioInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Apple(e) => write!(f, "{e}"),
            Self::FlacLibrary(e) => write!(f, "{e}"),
            Self::MissingFlacLibrary => f.write_str("a trusted libFLAC 1.4.2 path is required for FLAC input"),
            Self::MissingFlacDigest => f.write_str("an explicit SHA-256 pin is required for FLAC input"),
            Self::DigestMismatch { expected, actual } => write!(f, "libFLAC SHA-256 mismatch: expected {expected}, got {actual}"),
            Self::Library(e) => write!(f, "libFLAC input: {e}"),
            Self::InvalidInput(e) => write!(f, "invalid audio input: {e}"),
            Self::Allocation => f.write_str("could not allocate FLAC input buffer"),
            Self::Decoder { operation, result, state } => write!(f, "libFLAC {operation}: result {result}, decoder state {state}"),
            Self::DecoderData(status) => write!(f, "libFLAC reported a stream error (status {status})"),
            Self::Converter { operation, status } => write!(f, "{operation}: OSStatus {status} ({:#010x})", *status as u32),
            Self::UnexpectedNativeResult(e) => write!(f, "unexpected native audio input result: {e}"),
            Self::Unsupported(e) => write!(f, "unsupported audio input operation: {e}"),
            Self::Poisoned => f.write_str("audio input is unusable after a native decoding failure"),
        }
    }
}
impl Error for AudioInputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self { Self::Apple(e) => Some(e), Self::FlacLibrary(e) => Some(e), _ => None }
    }
}
impl From<apple_codec::CodecError> for AudioInputError { fn from(e: apple_codec::CodecError) -> Self { Self::Apple(e) } }
impl From<FlacError> for AudioInputError { fn from(e: FlacError) -> Self { Self::FlacLibrary(e) } }

/// Sequential input facade supporting the interleaved source-channel reader
/// and the two-plane render-input reader. FLAC reads use MIKAudio's two-channel
/// intermediate buffer; empty buffers do not advance the decoder.
pub enum AudioReader {
    Apple(apple_codec::Reader),
    Flac(FlacReader),
}
impl AudioReader {
    pub fn open(path: impl AsRef<Path>, processing_rate: Option<f64>, libraries: &InputLibraries) -> Result<Self, AudioInputError> {
        let path = path.as_ref();
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b"flac") {
            let lib = libraries.flac.as_ref().ok_or(AudioInputError::MissingFlacLibrary)?;
            let digest = libraries.flac_sha256.as_deref().ok_or(AudioInputError::MissingFlacDigest)?;
            Ok(Self::Flac(FlacReader::open(path, lib, digest)?))
        } else {
            #[cfg(any(target_os = "linux", feature = "portable"))]
            let reader = apple_codec::Reader::open_with(path, processing_rate,
                apple_codec::ReaderOptions { non_identical_mp3: libraries.allow_non_identical })?;
            #[cfg(not(any(target_os = "linux", feature = "portable")))]
            let reader = apple_codec::Reader::open(path, processing_rate)?;
            Ok(Self::Apple(reader))
        }
    }
    /// Whether a path that is not proven byte-identical to the macOS reference
    /// (the open-source MP3 decoder) produces this reader's samples.
    pub fn non_identical(&self) -> bool {
        match self {
            #[cfg(any(target_os = "linux", feature = "portable"))]
            Self::Apple(r) => r.non_identical(),
            #[cfg(not(any(target_os = "linux", feature = "portable")))]
            Self::Apple(_) => false,
            Self::Flac(_) => false,
        }
    }
    pub fn source_format(&self) -> StreamDescription {
        match self { Self::Apple(r) => r.source_format(), Self::Flac(r) => r.source }
    }
    pub fn client_format(&self) -> StreamDescription {
        match self { Self::Apple(r) => r.client_format(), Self::Flac(r) => r.client }
    }
    /// Configure the graph's two-plane float input. For FLAC, MIKAudio's native
    /// setter ignores the requested rate and never reconfigures the converter;
    /// the exposed client description records the actual source rate and the
    /// two-plane destination shape. For Apple input the setter is forwarded.
    pub fn set_stereo_planar(&mut self, sample_rate: f64) -> Result<(), AudioInputError> {
        match self {
            Self::Apple(r) => Ok(r.set_stereo_planar(sample_rate)?),
            Self::Flac(r) => {
                r.client = StreamDescription { sample_rate: r.source.sample_rate, format_id: LPCM, format_flags: 0x29,
                    bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4, channels_per_frame: 2,
                    bits_per_channel: 32, reserved: 0 };
                Ok(())
            }
        }
    }
    /// Read into two equal-sized planes, leaving the unreturned tail untouched.
    /// MIKAudio's FLAC path copies its two internal planes even for mono; the
    /// second is zero, not a duplicate. Its output remains at the source rate.
    pub fn read_stereo_planar(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<usize, AudioInputError> {
        match self { Self::Apple(r) => Ok(r.read_stereo_planar(left, right)?), Self::Flac(r) => r.read_stereo_planar(left, right) }
    }
    /// STREAMINFO's 36-bit total for FLAC, or AudioToolbox's #frm property.
    /// A FLAC total of zero is the format's unknown/empty value.
    pub fn file_length_frames(&self) -> Result<i64, AudioInputError> {
        match self { Self::Apple(r) => Ok(r.file_length_frames()?), Self::Flac(r) => Ok(r.total_samples as i64) }
    }
    /// MIKAudio provides its AudioFile bitrate query only for its Apple path;
    /// it has no FLAC bitrate operation, so no synthetic estimate is returned.
    pub fn bitrate(&self) -> Result<u32, AudioInputError> {
        match self {
            Self::Apple(r) => Ok(r.bitrate()?),
            Self::Flac(_) => Err(AudioInputError::Unsupported("MIKAudio exposes no FLAC bitrate query")),
        }
    }
    pub fn flac_library_identity(&self) -> Option<&LibraryIdentity> {
        match self { Self::Flac(r) => Some(r.validation.identity()), Self::Apple(_) => None }
    }
    /// Read interleaved frames. FLAC uses the source channel count regardless
    /// of the most recent planar setter; above two source channels only the
    /// packed first-two-channel prefix is written, like the native one-buffer
    /// path. A short final read does not write any unreturned tail.
    pub fn read(&mut self, output: &mut [f32]) -> Result<usize, AudioInputError> {
        match self { Self::Apple(r) => Ok(r.read(output)?), Self::Flac(r) => r.read(output) }
    }
    pub fn close(self) -> Result<(), AudioInputError> {
        match self { Self::Apple(r) => Ok(r.close()?), Self::Flac(r) => r.close() }
    }
}

// Public libFLAC 1.4.2 ABI. FLAC__Frame begins with FLAC__FrameHeader.
#[repr(C)]
struct FramePrefix { blocksize: u32, sample_rate: u32, channels: u32 }
#[repr(C)]
struct StreamInfo {
    min_blocksize: u32, max_blocksize: u32, min_framesize: u32, max_framesize: u32,
    sample_rate: u32, channels: u32, bits_per_sample: u32, total_samples: u64,
    md5: [u8; 16],
}
#[repr(C)]
struct StreamMetadata { kind: c_int, is_last: c_int, length: u32, info: StreamInfo }
type Decoder = c_void;
type WriteCallback = unsafe extern "C" fn(*const Decoder, *const c_void, *const *const i32, *mut c_void) -> c_int;
type MetadataCallback = unsafe extern "C" fn(*const Decoder, *const StreamMetadata, *mut c_void);
type ErrorCallback = unsafe extern "C" fn(*const Decoder, c_int, *mut c_void);
type New = unsafe extern "C" fn() -> *mut Decoder;
type Delete = unsafe extern "C" fn(*mut Decoder);
type SetMd5 = unsafe extern "C" fn(*mut Decoder, c_int) -> c_int;
type InitFile = unsafe extern "C" fn(*mut Decoder, *const c_char, Option<WriteCallback>, Option<MetadataCallback>, Option<ErrorCallback>, *mut c_void) -> c_int;
type Process = unsafe extern "C" fn(*mut Decoder) -> c_int;
type GetState = unsafe extern "C" fn(*const Decoder) -> c_int;
struct DecoderFunctions { new: New, delete: Delete, set_md5: SetMd5, init_file: InitFile, metadata: Process, single: Process, state: GetState }
unsafe fn get_symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, AudioInputError> {
    // The caller-selected library's handle outlives every function pointer.
    unsafe { library.get::<T>(name) }.map(|s| *s).map_err(|e| AudioInputError::Library(e.to_string()))
}
impl DecoderFunctions {
    unsafe fn load(library: &Library) -> Result<Self, AudioInputError> {
        Ok(Self { new: unsafe { get_symbol(library, b"FLAC__stream_decoder_new\0")? },
            delete: unsafe { get_symbol(library, b"FLAC__stream_decoder_delete\0")? },
            set_md5: unsafe { get_symbol(library, b"FLAC__stream_decoder_set_md5_checking\0")? },
            init_file: unsafe { get_symbol(library, b"FLAC__stream_decoder_init_file\0")? },
            metadata: unsafe { get_symbol(library, b"FLAC__stream_decoder_process_until_end_of_metadata\0")? },
            single: unsafe { get_symbol(library, b"FLAC__stream_decoder_process_single\0")? },
            state: unsafe { get_symbol(library, b"FLAC__stream_decoder_get_state\0")? } })
    }
}
#[repr(C)]
struct DlInfo { filename: *const c_char, base: *mut c_void, symbol: *const c_char, address: *mut c_void }
unsafe extern "C" { fn dladdr(address: *const c_void, info: *mut DlInfo) -> c_int; }

fn check_digest(path: &Path, expected: &str) -> Result<(), AudioInputError> {
    let actual = crate::mp3::file_sha256(path).map_err(|e| AudioInputError::Library(e.to_string()))?;
    // These are the installed signed MIKAudio universal binary and its arm64
    // extraction, not open-source libraries. Reject before dlopen even if a
    // caller supplies either as its pin.
    if matches!(actual.as_str(),
        "f793e89ef220486f6aea96b33654145fed4ce232f2a69edf2f5b54e5f40c2a24" |
        "05b607da99a54400095075f8384982b4b56e03c6d0ecb1f8e42851e629adffa5") {
        return Err(AudioInputError::Unsupported("original MIKAudio is reference-only, not a production FLAC library"));
    }
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(AudioInputError::DigestMismatch { expected: expected.to_owned(), actual });
    }
    Ok(())
}

fn verify_loaded_image(address: New, path: &Path) -> Result<(), AudioInputError> {
    let mut info = DlInfo { filename: ptr::null(), base: ptr::null_mut(), symbol: ptr::null(), address: ptr::null_mut() };
    if unsafe { dladdr(address as *const () as *const c_void, &mut info) } == 0 || info.filename.is_null() {
        return Err(AudioInputError::Library("could not identify loaded decoder image".into()));
    }
    let loaded = Path::new(OsStr::from_bytes(unsafe { CStr::from_ptr(info.filename) }.to_bytes()))
        .canonicalize().map_err(|e| AudioInputError::Library(e.to_string()))?;
    if loaded != path { return Err(AudioInputError::Library(format!("decoder resolved to unexpected image {}", loaded.display()))); }
    Ok(())
}


#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer { channels: u32, size: u32, data: *mut c_void }
#[cfg(all(target_os = "macos", not(feature = "portable")))]
impl Default for AudioBuffer { fn default() -> Self { Self { channels: 0, size: 0, data: ptr::null_mut() } } }
#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[repr(C)]
struct AudioBufferList { count: u32, buffers: [AudioBuffer; MAX_CHANNELS] }
#[cfg(all(target_os = "macos", not(feature = "portable")))]
type ConverterInput = unsafe extern "C" fn(*mut c_void, *mut u32, *mut AudioBufferList, *mut *mut c_void, *mut c_void) -> i32;
#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(source: *const StreamDescription, destination: *const StreamDescription, converter: *mut *mut c_void) -> i32;
    fn AudioConverterFillComplexBuffer(converter: *mut c_void, callback: Option<ConverterInput>, context: *mut c_void, packets: *mut u32, output: *mut AudioBufferList, descriptions: *mut c_void) -> i32;
    fn AudioConverterDispose(converter: *mut c_void) -> i32;
}

#[derive(Clone, Copy)]
struct Metadata { rate: u32, channels: u32, bits: u32, total: u64 }
#[cfg(all(target_os = "macos", not(feature = "portable")))]
struct InputBlock { channels: usize, pointers: [*const u8; MAX_CHANNELS], frames: u32, bytes_per_sample: u32, sent: bool }
struct DecodeContext {
    metadata: Option<Metadata>,
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    converter: Option<NonNull<c_void>>,
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    packed: [Vec<u8>; MAX_CHANNELS],
    converted: Vec<f32>,
    queue: Vec<f32>,
    queue_offset: usize,
    error: Option<AudioInputError>,
}
impl DecodeContext {
    fn new() -> Self {
        Self {
            metadata: None,
            #[cfg(all(target_os = "macos", not(feature = "portable")))]
            converter: None,
            #[cfg(all(target_os = "macos", not(feature = "portable")))]
            packed: std::array::from_fn(|_| Vec::new()),
            converted: Vec::new(), queue: Vec::new(), queue_offset: 0, error: None
        }
    }
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    fn setup_converter(&mut self, source: &StreamDescription) -> Result<(), AudioInputError> {
        let planar = StreamDescription { sample_rate: source.sample_rate, format_id: LPCM, format_flags: 0x29,
            bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4,
            channels_per_frame: source.channels_per_frame, bits_per_channel: 32, reserved: 0 };
        let mut converter = ptr::null_mut();
        let status = unsafe { AudioConverterNew(source, &planar, &mut converter) };
        self.converter = NonNull::new(converter);
        if status != 0 { return Err(AudioInputError::Converter { operation: "AudioConverterNew(FLAC)", status }); }
        if self.converter.is_none() { return Err(AudioInputError::UnexpectedNativeResult("null FLAC AudioConverter")); }
        let count = source.channels_per_frame as usize * CONVERTER_FRAMES;
        self.converted.try_reserve_exact(count).map_err(|_| AudioInputError::Allocation)?;
        self.converted.resize(count, 0.0);
        Ok(())
    }
    #[cfg(any(target_os = "linux", feature = "portable"))]
    fn setup_converter(&mut self, source: &StreamDescription) -> Result<(), AudioInputError> {
        if !matches!(source.bits_per_channel, 8 | 16 | 24 | 32) {
            return Err(AudioInputError::Unsupported("FLAC PCM conversion is verified only for 8-, 16-, 24- and 32-bit sources"));
        }
        Ok(())
    }
    fn queue_frames(&self) -> usize {
        let channels = self.metadata.map_or(1, |m| (m.channels as usize).min(2));
        (self.queue.len() - self.queue_offset) / channels
    }
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    fn convert_block(&mut self, frame: *const c_void, channels: *const *const i32) -> Result<(), AudioInputError> {
        let metadata = self.metadata.ok_or(AudioInputError::UnexpectedNativeResult("FLAC frame before STREAMINFO"))?;
        if frame.is_null() || channels.is_null() { return Err(AudioInputError::UnexpectedNativeResult("null FLAC frame")); }
        if metadata.bits < 8 { return Err(AudioInputError::Unsupported("MIKAudio FLAC write callback aborts for bit depths below 8")); }
        let header = unsafe { &*frame.cast::<FramePrefix>() };
        let frames = header.blocksize as usize;
        if frames == 0 || frames > 65_535 { return Err(AudioInputError::UnexpectedNativeResult("invalid FLAC blocksize")); }
        let n_channels = metadata.channels as usize;
        if header.channels < metadata.channels || header.channels as usize > MAX_CHANNELS {
            return Err(AudioInputError::UnexpectedNativeResult("FLAC frame has fewer channel buffers than STREAMINFO"));
        }
        let width = (metadata.bits / 8) as usize;
        let bytes = frames.checked_mul(width).ok_or(AudioInputError::Allocation)?;
        let mut block = InputBlock { channels: n_channels, pointers: [ptr::null(); MAX_CHANNELS], frames: frames as u32, bytes_per_sample: width as u32, sent: false };
        for c in 0..n_channels {
            let input = unsafe { *channels.add(c) };
            if input.is_null() { return Err(AudioInputError::UnexpectedNativeResult("null FLAC channel")); }
            let data = &mut self.packed[c];
            data.clear();
            data.try_reserve(bytes).map_err(|_| AudioInputError::Allocation)?;
            // The reference copies the least-significant floor(bits/8) bytes
            // of each signed i32 in little-endian order, without shifting.
            for i in 0..frames {
                let sample = unsafe { *input.add(i) }.to_le_bytes();
                data.extend_from_slice(&sample[..width]);
            }
            block.pointers[c] = data.as_ptr();
        }
        let emitted_channels = n_channels.min(2);
        let converter = self.converter.ok_or(AudioInputError::UnexpectedNativeResult("FLAC converter unavailable"))?.as_ptr();
        let mut empty_success = false;
        loop {
            let mut list = AudioBufferList { count: n_channels as u32, buffers: [AudioBuffer::default(); MAX_CHANNELS] };
            for c in 0..n_channels {
                list.buffers[c] = AudioBuffer { channels: 1, size: (CONVERTER_FRAMES * 4) as u32,
                    data: unsafe { self.converted.as_mut_ptr().add(c * CONVERTER_FRAMES) }.cast() };
            }
            let mut count = CONVERTER_FRAMES as u32;
            let status = unsafe { AudioConverterFillComplexBuffer(converter, Some(converter_input), (&mut block as *mut InputBlock).cast(), &mut count, &mut list, ptr::null_mut()) };
            if status != 0 && status != MORE { return Err(AudioInputError::Converter { operation: "AudioConverterFillComplexBuffer(FLAC)", status }); }
            if count as usize > CONVERTER_FRAMES { return Err(AudioInputError::UnexpectedNativeResult("FLAC converter exceeded output capacity")); }
            let count = count as usize;
            self.queue.try_reserve(count * emitted_channels).map_err(|_| AudioInputError::Allocation)?;
            if emitted_channels == 1 { self.queue.extend_from_slice(&self.converted[..count]); }
            else {
                for i in 0..count {
                    self.queue.push(self.converted[i]);
                    self.queue.push(self.converted[CONVERTER_FRAMES + i]);
                }
            }
            if status == MORE { break; }
            if count == 0 {
                if empty_success { return Err(AudioInputError::UnexpectedNativeResult("FLAC converter made no progress")); }
                empty_success = true;
            } else {
                empty_success = false;
            }
        }
        Ok(())
    }
    #[cfg(any(target_os = "linux", feature = "portable"))]
    fn convert_block(&mut self, frame: *const c_void, channels: *const *const i32) -> Result<(), AudioInputError> {
        let metadata = self.metadata.ok_or(AudioInputError::UnexpectedNativeResult("FLAC frame before STREAMINFO"))?;
        if frame.is_null() || channels.is_null() { return Err(AudioInputError::UnexpectedNativeResult("null FLAC frame")); }
        let header = unsafe { &*frame.cast::<FramePrefix>() };
        let frames = header.blocksize as usize;
        if frames == 0 || frames > 65_535 { return Err(AudioInputError::UnexpectedNativeResult("invalid FLAC blocksize")); }
        let n_channels = metadata.channels as usize;
        if header.channels < metadata.channels || header.channels as usize > MAX_CHANNELS {
            return Err(AudioInputError::UnexpectedNativeResult("FLAC frame has fewer channel buffers than STREAMINFO"));
        }
        let emitted_channels = n_channels.min(2);
        let samples = frames.checked_mul(emitted_channels).ok_or(AudioInputError::Allocation)?;
        self.converted.try_reserve(samples.saturating_sub(self.converted.len())).map_err(|_| AudioInputError::Allocation)?;
        self.converted.resize(samples, 0.0);
        for c in 0..n_channels {
            let input = unsafe { *channels.add(c) };
            if input.is_null() { return Err(AudioInputError::UnexpectedNativeResult("null FLAC channel")); }
            if c < emitted_channels {
                // libFLAC lends each full channel plane for the duration of this callback.
                let input = unsafe { std::slice::from_raw_parts(input, frames) };
                apple_codec::convert_flac_pcm(input, metadata.bits, &mut self.converted[c * frames..(c + 1) * frames])?;
            }
        }
        self.queue.try_reserve(samples).map_err(|_| AudioInputError::Allocation)?;
        if emitted_channels == 1 { self.queue.extend_from_slice(&self.converted[..frames]); }
        else {
            for i in 0..frames { self.queue.push(self.converted[i]); self.queue.push(self.converted[frames + i]); }
        }
        Ok(())
    }
}

unsafe extern "C" fn metadata_callback(_decoder: *const Decoder, metadata: *const StreamMetadata, data: *mut c_void) {
    if metadata.is_null() || data.is_null() { return; }
    if unsafe { (*metadata).kind } == 0 {
        let m = unsafe { &*ptr::addr_of!((*metadata).info) };
        let ctx = unsafe { &mut *data.cast::<DecodeContext>() };
        ctx.metadata = Some(Metadata { rate: m.sample_rate, channels: m.channels,
            bits: m.bits_per_sample, total: m.total_samples });
    }
}
unsafe extern "C" fn error_callback(_decoder: *const Decoder, status: c_int, data: *mut c_void) {
    if !data.is_null() {
        let ctx = unsafe { &mut *data.cast::<DecodeContext>() };
        if ctx.error.is_none() { ctx.error = Some(AudioInputError::DecoderData(status)); }
    }
}
unsafe extern "C" fn write_callback(_decoder: *const Decoder, frame: *const c_void, channels: *const *const i32, data: *mut c_void) -> c_int {
    if data.is_null() { return 1; }
    let ctx = unsafe { &mut *data.cast::<DecodeContext>() };
    match ctx.convert_block(frame, channels) {
        Ok(()) => 0,
        Err(error) => { ctx.error = Some(error); 1 }
    }
}
#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" fn converter_input(_converter: *mut c_void, packets: *mut u32, list: *mut AudioBufferList, _descriptions: *mut *mut c_void, data: *mut c_void) -> i32 {
    if packets.is_null() || list.is_null() || data.is_null() { return -1; }
    let block = unsafe { &mut *data.cast::<InputBlock>() };
    if block.sent { unsafe { *packets = 0 }; return MORE; }
    // CoreAudio's list is a variable-sized C allocation, not our 8-element
    // backing struct. Read only the count and the buffers actually supplied.
    let count = unsafe { ptr::addr_of!((*list).count).read() } as usize;
    if count > block.channels || count > MAX_CHANNELS { unsafe { *packets = 0 }; return -1; }
    let buffers = unsafe { ptr::addr_of_mut!((*list).buffers).cast::<AudioBuffer>() };
    for c in 0..count {
        let buffer = unsafe { buffers.add(c) };
        unsafe {
            ptr::addr_of_mut!((*buffer).data).write(block.pointers[c].cast_mut().cast());
            ptr::addr_of_mut!((*buffer).size).write(block.frames * block.bytes_per_sample);
        }
    }
    unsafe { *packets = block.frames };
    block.sent = true;
    0
}

/// Streaming native-FLAC implementation behind [`AudioReader`].
pub struct FlacReader {
    decoder: NonNull<Decoder>,
    functions: DecoderFunctions,
    // The validated descriptor-backed handle supplies both encoder identity
    // and the decoder ABI, so the two cannot refer to different images.
    validation: FlacLibrary,
    context: Box<DecodeContext>,
    source: StreamDescription,
    client: StreamDescription,
    total_samples: u64,
    failed: bool,
}
impl FlacReader {
    fn open(path: &Path, library_path: &Path, expected_sha256: &str) -> Result<Self, AudioInputError> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() { return Err(AudioInputError::InvalidInput("path is empty")); }
        let filename = CString::new(bytes).map_err(|_| AudioInputError::InvalidInput("path contains NUL"))?;
        if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AudioInputError::InvalidInput("FLAC SHA-256 pin must contain 64 hexadecimal digits"));
        }
        let canonical = library_path.canonicalize().map_err(|e| AudioInputError::Library(e.to_string()))?;
        check_digest(&canonical, expected_sha256)?;
        let validation = FlacLibrary::load(&canonical, expected_sha256)?;
        // SAFETY: the pinned, validated library remains live in this reader.
        let functions = unsafe { DecoderFunctions::load(validation.decoder_library())? };
        verify_loaded_image(functions.new, &canonical)?;
        check_digest(&canonical, expected_sha256)?;
        let decoder = NonNull::new(unsafe { (functions.new)() }).ok_or(AudioInputError::Allocation)?;
        let mut reader = Self { decoder, functions, validation, context: Box::new(DecodeContext::new()),
            source: StreamDescription::default(), client: StreamDescription::default(), total_samples: 0, failed: false };
        let decoder = reader.decoder.as_ptr();
        if unsafe { (reader.functions.set_md5)(decoder, 1) } == 0 { return Err(reader.failure("set MD5 checking", 0)); }
        let status = unsafe { (reader.functions.init_file)(decoder, filename.as_ptr(), Some(write_callback), Some(metadata_callback), Some(error_callback), (&mut *reader.context as *mut DecodeContext).cast()) };
        if status != 0 { return Err(reader.failure("init_file", status)); }
        let status = unsafe { (reader.functions.metadata)(decoder) };
        if let Some(error) = reader.context.error.take() { return Err(error); }
        if status == 0 { return Err(reader.failure("process metadata", status)); }
        let m = reader.context.metadata.ok_or(AudioInputError::UnexpectedNativeResult("missing FLAC STREAMINFO"))?;
        if m.rate == 0 || !(1..=MAX_CHANNELS as u32).contains(&m.channels) || !(4..=32).contains(&m.bits) {
            return Err(AudioInputError::UnexpectedNativeResult("invalid FLAC STREAMINFO"));
        }
        reader.source = StreamDescription { sample_rate: m.rate as f64, format_id: LPCM, format_flags: 0x24,
            bytes_per_packet: m.bits / 8, frames_per_packet: 1, bytes_per_frame: m.bits / 8,
            channels_per_frame: m.channels, bits_per_channel: m.bits, reserved: 0 };
        reader.client = StreamDescription { sample_rate: m.rate as f64, format_id: LPCM, format_flags: 0x09,
            bytes_per_packet: m.channels * 4, frames_per_packet: 1, bytes_per_frame: m.channels * 4,
            channels_per_frame: m.channels, bits_per_channel: 32, reserved: 0 };
        reader.total_samples = m.total;
        reader.context.setup_converter(&reader.source)?;
        Ok(reader)
    }
    fn failure(&self, operation: &'static str, result: i32) -> AudioInputError {
        let state = unsafe { (self.functions.state)(self.decoder.as_ptr()) };
        AudioInputError::Decoder { operation, result, state }
    }
    fn read(&mut self, output: &mut [f32]) -> Result<usize, AudioInputError> {
        if self.failed { return Err(AudioInputError::Poisoned); }
        let channels = self.source.channels_per_frame as usize;
        if output.len() % channels != 0 { return Err(AudioInputError::InvalidInput("output does not contain whole frames")); }
        if output.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) {
            return Err(AudioInputError::InvalidInput("output exceeds native buffer size"));
        }
        let requested = output.len() / channels;
        if requested == 0 { return Ok(0); }
        let emitted_channels = channels.min(2);
        self.read_with(requested, |source, written, frames| {
            let samples = frames * emitted_channels;
            output[written * emitted_channels..written * emitted_channels + samples].copy_from_slice(source);
        })
    }
    fn read_stereo_planar(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<usize, AudioInputError> {
        if self.failed { return Err(AudioInputError::Poisoned); }
        if left.len() != right.len() { return Err(AudioInputError::InvalidInput("stereo planes have different frame capacities")); }
        if left.len().checked_mul(4).is_none_or(|n| n > u32::MAX as usize) {
            return Err(AudioInputError::InvalidInput("stereo plane exceeds native buffer size"));
        }
        let channels = (self.source.channels_per_frame as usize).min(2);
        self.read_with(left.len(), |source, written, frames| {
            let l = &mut left[written..written + frames];
            let r = &mut right[written..written + frames];
            if channels == 1 {
                l.copy_from_slice(source);
                r.fill(0.0);
            } else {
                for ((l, r), frame) in l.iter_mut().zip(r.iter_mut()).zip(source.chunks_exact(2)) {
                    *l = frame[0];
                    *r = frame[1];
                }
            }
        })
    }
    fn read_with(&mut self, requested: usize, mut consume: impl FnMut(&[f32], usize, usize)) -> Result<usize, AudioInputError> {
        let emitted_channels = (self.source.channels_per_frame as usize).min(2);
        let mut written = 0;
        while written < requested {
            let available = self.context.queue_frames();
            if available != 0 {
                let frames = available.min(requested - written);
                let samples = frames * emitted_channels;
                let start = self.context.queue_offset;
                consume(&self.context.queue[start..start + samples], written, frames);
                self.context.queue_offset += samples;
                written += frames;
                continue;
            }
            self.context.queue.clear();
            self.context.queue_offset = 0;
            if unsafe { (self.functions.state)(self.decoder.as_ptr()) } == END_OF_STREAM { break; }
            let status = unsafe { (self.functions.single)(self.decoder.as_ptr()) };
            if let Some(error) = self.context.error.take() { self.failed = true; return Err(error); }
            if status == 0 { self.failed = true; return Err(self.failure("process_single", status)); }
        }
        Ok(written)
    }
    fn close(mut self) -> Result<(), AudioInputError> {
        #[cfg(all(target_os = "macos", not(feature = "portable")))]
        if let Some(converter) = self.context.converter.take() {
            let status = unsafe { AudioConverterDispose(converter.as_ptr()) };
            if status != 0 { return Err(AudioInputError::Converter { operation: "AudioConverterDispose(FLAC)", status }); }
        }
        // Decoder delete calls finish, as the original does; it ignores the
        // decoder finish/MD5 result. Drop below deletes it exactly once.
        Ok(())
    }
}
impl Drop for FlacReader {
    fn drop(&mut self) {
        unsafe { (self.functions.delete)(self.decoder.as_ptr()); }
        #[cfg(all(target_os = "macos", not(feature = "portable")))]
        if let Some(converter) = self.context.converter.take() { unsafe { AudioConverterDispose(converter.as_ptr()); } }
    }
}

#[cfg(all(test, any(target_os = "linux", feature = "portable")))]
mod tests {
    use super::*;

    #[test]
    fn portable_flac_rejects_every_unverified_pcm_depth() {
        for bits in 4..=32 {
            let mut context = DecodeContext::new();
            let source = StreamDescription { bits_per_channel: bits, ..Default::default() };
            let result = context.setup_converter(&source);
            if matches!(bits, 8 | 16 | 24 | 32) { assert!(result.is_ok(), "depth {bits}"); }
            else { assert!(matches!(result, Err(AudioInputError::Unsupported(_))), "depth {bits}"); }
        }
    }
}
