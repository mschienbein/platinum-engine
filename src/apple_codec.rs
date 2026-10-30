//! macOS AudioToolbox codec boundary used by the reference's Apple recorders.
//!
//! This module produces codec/container bytes only: it does not copy tags or
//! write the Platinum Notes marker. Full-file compatibility remains unproven.
#![cfg(target_os = "macos")]

use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};

const LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const ALAC: u32 = u32::from_be_bytes(*b"alac");
const AAC: u32 = u32::from_be_bytes(*b"aac ");
const FLAC: u32 = u32::from_be_bytes(*b"flac");
const FILE_FORMAT: u32 = u32::from_be_bytes(*b"ffmt");
const CLIENT_FORMAT: u32 = u32::from_be_bytes(*b"cfmt");
const FILE_LENGTH_FRAMES: u32 = u32::from_be_bytes(*b"#frm");
const AUDIO_CONVERTER: u32 = u32::from_be_bytes(*b"acnv");
const AUDIO_FILE: u32 = u32::from_be_bytes(*b"afil");
const BIT_RATE: u32 = u32::from_be_bytes(*b"brat");
const FLOAT_PACKED: u32 = 0x09;
const SIGNED_PACKED: u32 = 0x0c;
const SIGNED_PACKED_BIG_ENDIAN: u32 = 0x0e;
const FAIL_IF_EXISTS: u32 = 0;

/// ABI-compatible AudioStreamBasicDescription, returned exactly as AudioToolbox reports it.
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

/// The four Apple output recorders. AIFF and AIF use the same file type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Wav,
    Aiff,
    Alac,
    Aac,
}

/// Immutable recorder settings. `bit_depth` is the controller's recorder
/// argument: 0 for non-PCM/non-lossless input, otherwise 16, 24 or 32.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WriterConfig {
    pub format: OutputFormat,
    pub sample_rate: f64,
    pub channels: u32,
    pub bit_depth: u32,
}

/// The reference logs these AAC configuration failures but continues. They
/// are observable here so the caller can decide whether to accept that output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecWarning {
    AacConverterUnavailable { status: i32 },
    AacBitRateNotSet { status: i32 },
}

#[derive(Debug)]
pub enum CodecError {
    InvalidInput(&'static str),
    Unsupported(&'static str),
    /// The completed ALAC file did not decode to the submitted frame count.
    AlacFrameMismatch { submitted: u64, decoded: u64 },
    /// ALAC verification was cancelled before the private output was published.
    Cancelled,
    Native { operation: &'static str, status: i32 },
    Io { operation: &'static str, path: PathBuf, source: io::Error },
    /// A primary failure was followed by a native or filesystem cleanup failure.
    Cleanup { primary: Box<CodecError>, source: io::Error },
    /// The target was published successfully, but an owned temporary could not be removed.
    PublishedButCleanupFailed { path: PathBuf, source: io::Error },
    Poisoned,
    UnexpectedNativeResult(&'static str),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid audio codec input: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported audio codec input: {message}"),
            Self::AlacFrameMismatch { submitted, decoded } => write!(f, "ALAC output decoded {decoded} frames, expected {submitted}; refusing publication"),
            Self::Cancelled => f.write_str("ALAC postflight cancelled"),
            Self::Native { operation, status } => write!(f, "{operation} failed with OSStatus {status} ({:#010x})", *status as u32),
            Self::Io { operation, path, source } => write!(f, "{operation} {}: {source}", path.display()),
            Self::Cleanup { primary, source } => write!(f, "{primary}; additionally, cleanup failed: {source}"),
            Self::PublishedButCleanupFailed { path, source } => write!(f, "{} was published, but temporary cleanup failed: {source}", path.display()),
            Self::Poisoned => f.write_str("audio stream is unusable after a native codec failure"),
            Self::UnexpectedNativeResult(message) => write!(f, "unexpected AudioToolbox result: {message}"),
        }
    }
}

impl Error for CodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Cleanup { source, .. } | Self::PublishedButCleanupFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[repr(C)]
pub(crate) struct AudioBuffer {
    pub(crate) number_channels: u32,
    pub(crate) data_byte_size: u32,
    pub(crate) data: *mut c_void,
}

#[repr(C)]
pub(crate) struct AudioBufferList {
    pub(crate) number_buffers: u32,
    pub(crate) buffers: [AudioBuffer; 1],
}

#[repr(C)]
pub(crate) struct StereoBufferList {
    pub(crate) number_buffers: u32,
    pub(crate) buffers: [AudioBuffer; 2],
}

// On the tested macOS arm64 runtime, AudioToolbox AAC output depends on the
// stack alignment at ExtAudioFileRead (see the read pin below). Aligning the live
// list stabilizes that call boundary without changing its ABI or the caller's
// sample buffers. This is a bounded runtime observation, not a cross-version parity guarantee.
#[repr(align(64))]
struct AlignedStereoBufferList(StereoBufferList);
/// The same 64-byte alignment for the interleaved read: without it the AAC
/// waveform samples (analysis of AAC input) varied with the caller's frame sizes.
#[repr(align(64))]
struct AlignedBufferList(AudioBufferList);

// AudioToolbox's AAC encoder runs 512-point DFTs on stack temporaries. The kernel it
// uses (SME or NEON) depends on the 64-byte alignment of those temporaries, which derive
// from the stack pointer at the call: ExtAudioFileWrite uses the SME kernel when
// `sp % 64 == 48` at entry, and the final drain inside ExtAudioFileDispose when
// `sp % 64 == 32`. The two kernels round differently, so without a fixed stack pointer the
// encoded packets (the last ones, for Dispose) varied with the frame sizes of our callers.
// These calls go through `call_with_sp_residue`, which fixes `sp % 64` at the callee's entry.
// Residue 0 is neither of those two SME cases, so both calls run the NEON kernel, whose
// output matches the native application's AAC references (docs/VERIFICATION.md) and the
// pre-0.2.0 releases' output on the same inputs (the stack-layout study behind the choice
// of residue is part of the release evidence). This is a bounded runtime observation of
// the tested macOS arm64 AudioToolbox, not a cross-version guarantee.
const WRITE_SP_RESIDUE: usize = 0;
const DISPOSE_SP_RESIDUE: usize = 0;

// AudioToolbox's AAC decoder has the same two output classes as the encoder, selected per
// `ExtAudioFileRead` call: the SME 512-point inverse-transform kernel runs iff its stack
// temporaries are 64-byte aligned, i.e. `sp % 64 == 16` at the read's entry (class S);
// every other residue runs the generic NEON kernel (class G). The two round differently
// (3.7 to 85 % of the float words differ, by at most 6e-7). The native application and the
// engine's reads have been class G, but only through frame layout: the stack residue at the
// call was whatever the compiler's frame for the caller produced (measured as 0 in the
// shipped v0.2.0 binary; the AAC research recorded 32 for the interleaved read of another
// build), so a different frame layout could have selected class S. Both reads now go through
// `call_with_sp_residue` with residue 0, which is class G by construction (the AAC decoder
// research that found the two classes is part of the release evidence; the v0.3.0
// proof compared 43,380 engine results against the v0.2.0 binary without a difference). The pin
// only removes the dependence on compiler frame layout. A bounded runtime observation of the
// tested macOS arm64 AudioToolbox.
const READ_SP_RESIDUE: usize = 0;

/// `ExtAudioFileRead` entered with `sp % 64 == READ_SP_RESIDUE`.
///
/// # Safety
/// The same contract as the native call: `file` is a live ExtAudioFile, `frames` and `list`
/// point to valid, writable storage that outlives the call.
unsafe fn read_pinned(file: *mut c_void, frames: *mut u32, list: *mut AudioBufferList) -> i32 {
    // SAFETY: ExtAudioFileRead is a C-ABI function of three pointer arguments.
    unsafe { read_pinned_via(ExtAudioFileRead as *const () as usize, file, frames, list) }
}

/// The read pin applied to any three-pointer C function (the unit tests pass a stack probe).
unsafe fn read_pinned_via(function: usize, file: *mut c_void, frames: *mut u32, list: *mut AudioBufferList) -> i32 {
    // SAFETY: the caller supplies a C-ABI function of three pointer arguments.
    unsafe { call_with_sp_residue(function, file as usize, frames as usize, list as usize, READ_SP_RESIDUE) }
}

/// Call a C function of up to three integer arguments with `sp % 64 == residue` at its entry.
/// The previous stack pointer is held in `x20`, which is declared clobbered (callee-saved,
/// so the compiler preserves it), and is restored after the call; the callee's frames live
/// below at least 256 bytes of unused stack.
#[cfg_attr(test, unsafe(no_mangle))]
#[inline(never)]
pub(crate) unsafe fn call_with_sp_residue(function: usize, a0: usize, a1: usize, a2: usize, residue: usize) -> i32 {
    debug_assert!(residue < 64 && residue % 16 == 0, "stack pointers stay 16-byte aligned");
    let status: usize;
    // SAFETY: `function` is a C-ABI function taking at most three integer arguments; the asm
    // switches to a lower 16-byte-aligned stack position, calls it, and restores `sp` from
    // `x20` before returning. Caller-saved registers (including lr) are clobbered by the ABI.
    unsafe {
        std::arch::asm!(
            "mov x20, sp",
            "sub x9, sp, #256",
            "and x9, x9, #0xffffffffffffffc0",
            "add x9, x9, x11",
            "mov sp, x9",
            "blr x10",
            "mov sp, x20",
            in("x10") function, in("x11") residue,
            inout("x0") a0 => status, in("x1") a1, in("x2") a2,
            out("x20") _,
            clobber_abi("C"),
        );
    }
    status as i32
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn ExtAudioFileOpenURL(url: *const c_void, file: *mut *mut c_void) -> i32;
    pub(crate) fn ExtAudioFileCreateWithURL(url: *const c_void, file_type: u32, format: *const StreamDescription, layout: *const c_void, flags: u32, file: *mut *mut c_void) -> i32;
    fn ExtAudioFileGetProperty(file: *mut c_void, property: u32, size: *mut u32, data: *mut c_void) -> i32;
    pub(crate) fn ExtAudioFileSetProperty(file: *mut c_void, property: u32, size: u32, data: *const c_void) -> i32;
    fn ExtAudioFileRead(file: *mut c_void, frames: *mut u32, data: *mut AudioBufferList) -> i32;
    pub(crate) fn ExtAudioFileWrite(file: *mut c_void, frames: u32, data: *const AudioBufferList) -> i32;
    pub(crate) fn ExtAudioFileDispose(file: *mut c_void) -> i32;
    fn AudioFileGetProperty(file: *mut c_void, property: u32, size: *mut u32, data: *mut c_void) -> i32;
    pub(crate) fn AudioConverterNew(input: *const StreamDescription, output: *const StreamDescription, converter: *mut *mut c_void) -> i32;
    fn AudioConverterSetProperty(converter: *mut c_void, property: u32, size: u32, data: *const c_void) -> i32;
    fn AudioConverterConvertBuffer(converter: *mut c_void, input_size: u32, input: *const c_void, output_size: *mut u32, output: *mut c_void) -> i32;
    pub(crate) fn AudioConverterDispose(converter: *mut c_void) -> i32;
    pub(crate) fn AudioConverterConvertComplexBuffer(converter: *mut c_void, frames: u32, input: *const AudioBufferList, output: *mut AudioBufferList) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CFURLCreateFromFileSystemRepresentation(allocator: *const c_void, bytes: *const u8, length: isize, directory: u8) -> *const c_void;
    pub(crate) fn CFRelease(object: *const c_void);
}

fn native_status(operation: &'static str, status: i32) -> Result<(), CodecError> {
    if status == 0 { Ok(()) } else { Err(CodecError::Native { operation, status }) }
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> CodecError {
    CodecError::Io { operation, path: path.to_owned(), source }
}

fn with_native_cleanup(primary: CodecError, cleanup: Result<(), CodecError>) -> CodecError {
    match cleanup {
        Ok(()) => primary,
        Err(error) => CodecError::Cleanup { primary: Box::new(primary), source: io::Error::other(error) },
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, CodecError> {
    if path.as_os_str().as_bytes().is_empty() || path.as_os_str().as_bytes().contains(&0) {
        return Err(CodecError::InvalidInput("path is empty or contains NUL"));
    }
    if path.is_absolute() { return Ok(path.to_owned()); }
    let cwd = std::env::current_dir().map_err(|e| io_error("resolve current directory", path, e))?;
    Ok(cwd.join(path))
}

struct Url(NonNull<c_void>);
impl Url {
    fn new(path: &Path) -> Result<Self, CodecError> {
        let bytes = path.as_os_str().as_bytes();
        let length = isize::try_from(bytes.len()).map_err(|_| CodecError::InvalidInput("path is too long"))?;
        // Core Foundation copies the filesystem representation before returning.
        let url = unsafe { CFURLCreateFromFileSystemRepresentation(ptr::null(), bytes.as_ptr(), length, 0) };
        NonNull::new(url.cast_mut()).map(Self).ok_or(CodecError::InvalidInput("could not form file URL"))
    }
    fn as_ptr(&self) -> *const c_void { self.0.as_ptr() }
}
impl Drop for Url {
    fn drop(&mut self) { unsafe { CFRelease(self.as_ptr()) } }
}

struct ExtHandle(Option<NonNull<c_void>>);
impl ExtHandle {
    fn new(raw: *mut c_void) -> Result<Self, CodecError> {
        NonNull::new(raw).map(|p| Self(Some(p))).ok_or(CodecError::UnexpectedNativeResult("null ExtAudioFileRef after successful call"))
    }
    fn ptr(&self) -> *mut c_void { self.0.expect("live ExtAudioFileRef").as_ptr() }
    fn close(&mut self) -> Result<(), CodecError> {
        match self.0.take() {
            Some(p) => native_status("ExtAudioFileDispose", unsafe {
                call_with_sp_residue(ExtAudioFileDispose as *const () as usize, p.as_ptr() as usize, 0, 0, DISPOSE_SP_RESIDUE)
            }),
            None => Ok(()),
        }
    }
    fn get<T: Copy>(&self, property: u32, operation: &'static str) -> Result<T, CodecError> {
        let mut result = MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>() as u32;
        native_status(operation, unsafe { ExtAudioFileGetProperty(self.ptr(), property, &mut size, result.as_mut_ptr().cast()) })?;
        if size as usize != size_of::<T>() { return Err(CodecError::UnexpectedNativeResult("unexpected property size")); }
        Ok(unsafe { result.assume_init() })
    }
    fn set_client(&self, format: &StreamDescription) -> Result<(), CodecError> {
        native_status("ExtAudioFileSetProperty(client format)", unsafe {
            ExtAudioFileSetProperty(self.ptr(), CLIENT_FORMAT, size_of::<StreamDescription>() as u32, (format as *const StreamDescription).cast())
        })
    }
}
impl Drop for ExtHandle { fn drop(&mut self) { let _ = self.close(); } }

fn float_format(rate: f64, channels: u32) -> Result<StreamDescription, CodecError> {
    if !rate.is_finite() || rate <= 0.0 { return Err(CodecError::InvalidInput("sample rate must be positive and finite")); }
    let bytes = channels.checked_mul(4).filter(|n| *n != 0).ok_or(CodecError::InvalidInput("invalid channel count"))?;
    Ok(StreamDescription { sample_rate: rate, format_id: LPCM, format_flags: FLOAT_PACKED, bytes_per_packet: bytes, frames_per_packet: 1, bytes_per_frame: bytes, channels_per_frame: channels, bits_per_channel: 32, reserved: 0 })
}

/// The bit-depth argument built by the final output controller, not the
/// encoded format's reported bit depth. A compressed non-lossless source is 0.
pub fn recorder_bit_depth(source: &StreamDescription) -> u32 {
    if !matches!(source.format_id, LPCM | ALAC | FLAC) { return 0; }
    match source.bits_per_channel {
        0..=16 => 16,
        17..=24 => 24,
        _ => 32,
    }
}

fn file_description(config: WriterConfig) -> Result<(u32, StreamDescription), CodecError> {
    let client = float_format(config.sample_rate, config.channels)?;
    if !matches!(config.bit_depth, 0 | 16 | 24 | 32) {
        return Err(CodecError::InvalidInput("recorder bit depth must be 0, 16, 24 or 32"));
    }
    let mut file = StreamDescription { sample_rate: client.sample_rate, channels_per_frame: client.channels_per_frame, ..Default::default() };
    let file_type = match config.format {
        OutputFormat::Wav | OutputFormat::Aiff => {
            let bits = config.bit_depth.max(16);
            let bytes = config.channels.checked_mul((bits + 7) / 8).ok_or(CodecError::InvalidInput("audio frame size overflows"))?;
            file.format_id = LPCM;
            file.format_flags = if config.format == OutputFormat::Wav { SIGNED_PACKED } else { SIGNED_PACKED_BIG_ENDIAN };
            file.bytes_per_packet = bytes;
            file.frames_per_packet = 1;
            file.bytes_per_frame = bytes;
            file.bits_per_channel = bits;
            if config.format == OutputFormat::Wav { u32::from_be_bytes(*b"WAVE") } else { u32::from_be_bytes(*b"AIFF") }
        }
        OutputFormat::Alac => {
            file.format_id = ALAC;
            file.format_flags = if config.bit_depth > 31 { 4 } else if config.bit_depth > 23 { 3 } else { 1 };
            u32::from_be_bytes(*b"m4af")
        }
        OutputFormat::Aac => {
            file.format_id = AAC;
            u32::from_be_bytes(*b"m4af")
        }
    };
    Ok((file_type, file))
}

fn buffer_length(samples: usize, channels: u32, bytes_per_sample: usize) -> Result<(u32, u32), CodecError> {
    let channels = channels as usize;
    if samples % channels != 0 { return Err(CodecError::InvalidInput("buffer does not contain whole interleaved frames")); }
    let frames = u32::try_from(samples / channels).map_err(|_| CodecError::InvalidInput("too many frames in one call"))?;
    let bytes = samples.checked_mul(bytes_per_sample).and_then(|n| u32::try_from(n).ok()).ok_or(CodecError::InvalidInput("buffer is too large for AudioToolbox"))?;
    Ok((frames, bytes))
}

fn audio_buffer(channels: u32, bytes: u32, data: *mut c_void) -> AudioBufferList {
    AudioBufferList { number_buffers: 1, buffers: [AudioBuffer { number_channels: channels, data_byte_size: bytes, data }] }
}

/// Sequential ExtAudioFile decoder. FLAC-suffixed paths are intentionally
/// rejected: MIKAudio selects its separate FLAC decoder for those paths.
pub struct Reader {
    handle: ExtHandle,
    source: StreamDescription,
    client: StreamDescription,
    failed: bool,
}

impl Reader {
    /// `sample_rate` is the desired processing rate; `None` preserves the file
    /// rate. Channels always come from the source ASBD and remain interleaved.
    pub fn open(path: impl AsRef<Path>, sample_rate: Option<f64>) -> Result<Self, CodecError> {
        let path = absolute_path(path.as_ref())?;
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b"flac") {
            return Err(CodecError::Unsupported("the reference uses a separate FLAC decoder for paths ending in flac"));
        }
        Self::open_audio_toolbox(path, sample_rate)
    }
    fn open_audio_toolbox(path: PathBuf, sample_rate: Option<f64>) -> Result<Self, CodecError> {
        let url = Url::new(&path)?;
        let mut raw = ptr::null_mut();
        let status = unsafe { ExtAudioFileOpenURL(url.as_ptr(), &mut raw) };
        // Match MIKExtAudioFileOpen: release the URL before querying or
        // configuring the decoder, rather than extending its native lifetime.
        drop(url);
        if status != 0 {
            let primary = CodecError::Native { operation: "ExtAudioFileOpenURL", status };
            let cleanup = match NonNull::new(raw) {
                Some(p) => native_status("ExtAudioFileDispose", unsafe { ExtAudioFileDispose(p.as_ptr()) }),
                None => Ok(()),
            };
            return Err(with_native_cleanup(primary, cleanup));
        }
        let mut handle = ExtHandle::new(raw)?;
        let formats = (|| {
            let source: StreamDescription = handle.get(FILE_FORMAT, "ExtAudioFileGetProperty(file format)")?;
            let client = float_format(sample_rate.unwrap_or(source.sample_rate), source.channels_per_frame)?;
            handle.set_client(&client)?;
            Ok::<_, CodecError>((source, client))
        })();
        let (source, client) = match formats {
            Ok(formats) => formats,
            Err(error) => return Err(with_native_cleanup(error, handle.close())),
        };
        Ok(Self { handle, source, client, failed: false })
    }
    pub fn source_format(&self) -> StreamDescription { self.source }
    pub fn client_format(&self) -> StreamDescription { self.client }
    /// The native file-length property; it is not a count of already-decoded frames.
    pub fn file_length_frames(&self) -> Result<i64, CodecError> {
        self.handle.get(FILE_LENGTH_FRAMES, "ExtAudioFileGetProperty(file length)")
    }
    /// MIKAudio's underlying AudioFile bitrate query; some codecs report an approximation.
    pub fn bitrate(&self) -> Result<u32, CodecError> {
        let audio_file: *mut c_void = self.handle.get(AUDIO_FILE, "ExtAudioFileGetProperty(audio file)")?;
        if audio_file.is_null() { return Err(CodecError::UnexpectedNativeResult("null underlying AudioFileID")); }
        let mut bitrate = 0u32;
        let mut size = size_of::<u32>() as u32;
        native_status("AudioFileGetProperty(bit rate)", unsafe {
            AudioFileGetProperty(audio_file, BIT_RATE, &mut size, (&mut bitrate as *mut u32).cast())
        })?;
        if size as usize != size_of::<u32>() { return Err(CodecError::UnexpectedNativeResult("unexpected bitrate property size")); }
        Ok(bitrate)
    }

    /// Configure the observed first-AU input: packed planar float32 stereo.
    /// AudioToolbox, not a manual channel mixer, performs source conversion.
    pub fn set_stereo_planar(&mut self, sample_rate: f64) -> Result<(), CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        let mut client = float_format(sample_rate, 2)?;
        client.format_flags |= 0x20;
        client.bytes_per_frame = 4;
        client.bytes_per_packet = 4;
        self.handle.set_client(&client)?;
        self.client = client;
        Ok(())
    }

    /// Read into the two mono planes requested by the original processing AU.
    /// Both planes have identical capacity; the returned count is frames.
    pub fn read_stereo_planar(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<usize, CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        if self.client.format_flags & 0x20 == 0 || self.client.channels_per_frame != 2 {
            return Err(CodecError::InvalidInput("reader is not configured for planar stereo"));
        }
        if left.len() != right.len() { return Err(CodecError::InvalidInput("stereo plane lengths differ")); }
        let (mut frames, bytes) = buffer_length(left.len(), 1, size_of::<f32>())?;
        if frames == 0 { return Ok(0); }
        let requested = frames;
        let mut list = AlignedStereoBufferList(StereoBufferList {
            number_buffers: 2,
            buffers: [
                AudioBuffer { number_channels: 1, data_byte_size: bytes, data: left.as_mut_ptr().cast() },
                AudioBuffer { number_channels: 1, data_byte_size: bytes, data: right.as_mut_ptr().cast() },
            ],
        });
        // AudioBufferList's variable-size ABI uses its count, not the C/Rust
        // declaration's trailing array length. Both entries are live here.
        let status = unsafe { read_pinned(self.handle.ptr(), &mut frames, (&mut list.0 as *mut StereoBufferList).cast()) };
        if status != 0 { self.failed = true; return Err(CodecError::Native { operation: "ExtAudioFileRead(planar stereo)", status }); }
        if frames > requested || list.0.number_buffers != 2 || list.0.buffers.iter().any(|buffer| buffer.data_byte_size > bytes) {
            self.failed = true;
            return Err(CodecError::UnexpectedNativeResult("planar read exceeded supplied buffers"));
        }
        Ok(frames as usize)
    }
    /// Read whole interleaved frames. With a nonempty buffer, zero means EOF;
    /// an empty buffer returns zero without calling AudioToolbox.
    pub fn read(&mut self, output: &mut [f32]) -> Result<usize, CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        if self.client.format_flags & 0x20 != 0 {
            return Err(CodecError::InvalidInput("use read_stereo_planar for a planar reader"));
        }
        let (mut frames, bytes) = buffer_length(output.len(), self.client.channels_per_frame, size_of::<f32>())?;
        if frames == 0 { return Ok(0); }
        let requested = frames;
        let mut list = AlignedBufferList(audio_buffer(self.client.channels_per_frame, bytes, output.as_mut_ptr().cast()));
        let status = unsafe { read_pinned(self.handle.ptr(), &mut frames, &mut list.0) };
        if status != 0 { self.failed = true; return Err(CodecError::Native { operation: "ExtAudioFileRead", status }); }
        if frames > requested || list.0.buffers[0].data_byte_size > bytes {
            self.failed = true;
            return Err(CodecError::UnexpectedNativeResult("read exceeded supplied buffer"));
        }
        Ok(frames as usize)
    }
    /// Explicit close makes native disposal errors observable; Drop is best effort.
    pub fn close(mut self) -> Result<(), CodecError> { self.handle.close() }
}

fn verify_alac_frames(path: &Path, submitted: u64, cancelled: &mut impl FnMut() -> bool) -> Result<(), CodecError> {
    if cancelled() { return Err(CodecError::Cancelled); }
    let mut reader = Reader::open_audio_toolbox(absolute_path(path)?, None)?;
    let decoded = (|| {
        if submitted == 0 {
            // ExtAudioFileRead returns -66567 for a valid zero-packet ALAC.
            // With no writes submitted, validate its zero-frame container instead.
            let frames = reader.file_length_frames()?;
            if cancelled() { return Err(CodecError::Cancelled); }
            return u64::try_from(frames).map_err(|_| CodecError::UnexpectedNativeResult("negative ALAC frame count"));
        }
        let channels = reader.client_format().channels_per_frame as usize;
        let samples = channels.checked_mul(4096).filter(|&n| n != 0)
            .ok_or(CodecError::InvalidInput("invalid ALAC channel count"))?;
        let mut buffer = vec![0.0; samples];
        let mut count = 0u64;
        loop {
            if cancelled() { return Err(CodecError::Cancelled); }
            let frames = reader.read(&mut buffer)?;
            if cancelled() { return Err(CodecError::Cancelled); }
            if frames == 0 { break; }
            count = count.checked_add(frames as u64).ok_or(CodecError::InvalidInput("decoded ALAC frame count overflows"))?;
            if count > submitted { break; }
        }
        Ok::<_, CodecError>(count)
    })();
    let close = reader.close();
    let decoded = match decoded {
        Ok(decoded) => { close?; decoded }
        Err(error) => return Err(with_native_cleanup(error, close)),
    };
    if decoded != submitted { return Err(CodecError::AlacFrameMismatch { submitted, decoded }); }
    Ok(())
}

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

// The reference requests erase-existing (1); we deliberately pass zero so
// AudioToolbox must fail on an existing path. Publication is a no-replace hard
// link from a private sibling workspace after successful native finalization.
struct Workspace {
    directory: PathBuf,
    file: PathBuf,
    identity: Option<File>,
}
impl Workspace {
    fn create(target: &Path) -> Result<Self, CodecError> {
        let parent = target.parent().ok_or(CodecError::InvalidInput("target has no parent directory"))?;
        let name = target.file_name().ok_or(CodecError::InvalidInput("target has no file name"))?;
        for _ in 0..128 {
            let nonce = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
            let directory = parent.join(format!(".pn-codec-{}-{nonce:x}", std::process::id()));
            if directory == target { continue; }
            match DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => return Ok(Self { file: directory.join(name), directory, identity: None }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io_error("create codec workspace", &directory, e)),
            }
        }
        Err(CodecError::InvalidInput("could not allocate a unique codec workspace"))
    }
    fn remember_created_file(&mut self) -> Result<(), CodecError> {
        let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&self.file).map_err(|e| io_error("pin created codec file", &self.file, e))?;
        self.identity = Some(file);
        let metadata = self.identity.as_ref().unwrap().metadata().map_err(|e| io_error("inspect created codec file", &self.file, e))?;
        let named = fs::symlink_metadata(&self.file).map_err(|e| io_error("inspect named codec file", &self.file, e))?;
        if !metadata.file_type().is_file() || !named.file_type().is_file()
            || (metadata.dev(), metadata.ino()) != (named.dev(), named.ino())
            || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(CodecError::UnexpectedNativeResult("created codec output identity or owner changed"));
        }
        Ok(())
    }
    fn check_owned_file(&self) -> io::Result<()> {
        let metadata = fs::symlink_metadata(&self.file)?;
        let held = self.identity.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "codec file is not pinned"))?.metadata()?;
        if held.file_type().is_file() && metadata.file_type().is_file() && (metadata.dev(), metadata.ino()) == (held.dev(), held.ino()) {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "codec workspace file was replaced; refusing to use or remove it"))
        }
    }
    fn cleanup(&mut self) -> io::Result<()> {
        // A failed CreateWithURL may have left a file, but we cannot establish
        // its ownership. Do not unlink it; report the nonempty workspace.
        let file = if self.identity.is_some() {
            match self.check_owned_file() {
                Ok(()) => fs::remove_file(&self.file),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        } else { Ok(()) };
        let directory = match fs::remove_dir(&self.directory) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", self.directory.display()))),
        };
        file.and(directory)
    }
}
impl Drop for Workspace { fn drop(&mut self) { let _ = self.cleanup(); } }

fn with_cleanup(primary: CodecError, workspace: &mut Workspace) -> CodecError {
    match workspace.cleanup() {
        Ok(()) => primary,
        Err(source) => CodecError::Cleanup { primary: Box::new(primary), source },
    }
}

/// A single synchronous encoder; configuration and output destination cannot
/// change after creation. The target is created only by successful `finish`.
pub struct Writer {
    handle: ExtHandle,
    workspace: Workspace,
    target: PathBuf,
    config: WriterConfig,
    warnings: Vec<CodecWarning>,
    failed: bool,
    submitted_frames: u64,
}

impl Writer {
    pub fn create(path: impl AsRef<Path>, config: WriterConfig) -> Result<Self, CodecError> {
        Self::create_impl(path, config, #[cfg(test)] false)
    }
    fn create_impl(path: impl AsRef<Path>, config: WriterConfig,
        #[cfg(test)] fail_after_create: bool) -> Result<Self, CodecError> {
        let (file_type, file_format) = file_description(config)?;
        let client = float_format(config.sample_rate, config.channels)?;
        let target = absolute_path(path.as_ref())?;
        match fs::symlink_metadata(&target) {
            Ok(_) => return Err(io_error("create new output", &target, io::Error::new(io::ErrorKind::AlreadyExists, "output already exists"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {},
            Err(e) => return Err(io_error("inspect output", &target, e)),
        }
        let mut workspace = Workspace::create(&target)?;
        let url = match Url::new(&workspace.file) { Ok(url) => url, Err(e) => return Err(with_cleanup(e, &mut workspace)) };
        let mut raw = ptr::null_mut();
        let status = unsafe { ExtAudioFileCreateWithURL(url.as_ptr(), file_type, &file_format, ptr::null(), FAIL_IF_EXISTS, &mut raw) };
        if status != 0 {
            let mut primary = CodecError::Native { operation: "ExtAudioFileCreateWithURL", status };
            // The private directory was empty and inaccessible to other users.
            // Native creation can leave a partial file even on failure.
            if fs::symlink_metadata(&workspace.file).is_ok() {
                if let Err(error) = workspace.remember_created_file() {
                    primary = CodecError::Cleanup { primary: Box::new(primary), source: io::Error::other(error) };
                }
            }
            let close = match NonNull::new(raw) {
                Some(p) => native_status("ExtAudioFileDispose", unsafe { ExtAudioFileDispose(p.as_ptr()) }),
                None => Ok(()),
            };
            return Err(with_cleanup(with_native_cleanup(primary, close), &mut workspace));
        }
        if let Err(error) = workspace.remember_created_file() {
            let close = match NonNull::new(raw) {
                Some(p) => native_status("ExtAudioFileDispose", unsafe { ExtAudioFileDispose(p.as_ptr()) }),
                None => Ok(()),
            };
            return Err(with_cleanup(with_native_cleanup(error, close), &mut workspace));
        }
        let mut handle = match ExtHandle::new(raw) { Ok(handle) => handle, Err(e) => return Err(with_cleanup(e, &mut workspace)) };
        #[cfg(test)] if fail_after_create {
            let primary = with_native_cleanup(CodecError::InvalidInput("injected codec initialization failure"), handle.close());
            return Err(with_cleanup(primary, &mut workspace));
        }
        if let Err(e) = handle.set_client(&client) {
            let primary = with_native_cleanup(e, handle.close());
            return Err(with_cleanup(primary, &mut workspace));
        }
        let mut warnings = Vec::new();
        if config.format == OutputFormat::Aac {
            let mut converter: *mut c_void = ptr::null_mut();
            let mut size = size_of::<*mut c_void>() as u32;
            let status = unsafe { ExtAudioFileGetProperty(handle.ptr(), AUDIO_CONVERTER, &mut size, (&mut converter as *mut *mut c_void).cast()) };
            if status != 0 || converter.is_null() || size as usize != size_of::<*mut c_void>() {
                warnings.push(CodecWarning::AacConverterUnavailable { status });
            } else {
                let bitrate: u32 = 256_000;
                let status = unsafe { AudioConverterSetProperty(converter, BIT_RATE, size_of::<u32>() as u32, (&bitrate as *const u32).cast()) };
                if status != 0 { warnings.push(CodecWarning::AacBitRateNotSet { status }); }
            }
            // The reference does not set kExtAudioFileProperty_ConverterConfig.
        }
        Ok(Self { handle, workspace, target, config, warnings, failed: false, submitted_frames: 0 })
    }
    pub fn config(&self) -> WriterConfig { self.config }
    pub fn warnings(&self) -> &[CodecWarning] { &self.warnings }
    /// Writes one complete interleaved float32 block. The controller normally
    /// supplies 512-frame blocks; the last block can be shorter.
    pub fn write(&mut self, input: &[f32]) -> Result<(), CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        let (frames, bytes) = buffer_length(input.len(), self.config.channels, size_of::<f32>())?;
        if frames == 0 { return Ok(()); }
        let submitted = self.submitted_frames.checked_add(u64::from(frames))
            .ok_or(CodecError::InvalidInput("submitted frame count overflows"))?;
        let list = audio_buffer(self.config.channels, bytes, input.as_ptr().cast_mut().cast());
        let status = unsafe {
            call_with_sp_residue(ExtAudioFileWrite as *const () as usize, self.handle.ptr() as usize, frames as usize,
                &list as *const AudioBufferList as usize, WRITE_SP_RESIDUE)
        };
        if status != 0 { self.failed = true; return Err(CodecError::Native { operation: "ExtAudioFileWrite", status }); }
        self.submitted_frames = submitted;
        Ok(())
    }
    /// Dispose/flush the codec and publish without replacing any existing path.
    /// A hard-link failure (including an existing target) is returned as an
    /// error; filesystems that do not support hard links are not silently altered.
    pub fn finish(self) -> Result<(), CodecError> {
        self.finish_cancellable(|| false)
    }
    /// Finish as above, checking cancellation around each ALAC postflight
    /// read. A cancellation closes the decoder and removes the workspace.
    pub fn finish_cancellable(self, cancelled: impl FnMut() -> bool) -> Result<(), CodecError> {
        self.finish_cancellable_registered(cancelled, |_| Ok(()))
    }
    pub(crate) fn finish_cancellable_registered(mut self, mut cancelled: impl FnMut() -> bool,
        register: impl FnOnce(&File) -> io::Result<()>) -> Result<(), CodecError> {
        let primary = match (self.failed, self.handle.close()) {
            (true, Err(close)) => Some(CodecError::Cleanup { primary: Box::new(CodecError::Poisoned), source: io::Error::other(close) }),
            (true, Ok(())) => Some(CodecError::Poisoned),
            (false, result) => result.err(),
        };
        if let Some(error) = primary { return Err(with_cleanup(error, &mut self.workspace)); }
        if let Err(e) = self.workspace.check_owned_file() {
            let error = io_error("inspect codec output before publication", &self.workspace.file, e);
            return Err(with_cleanup(error, &mut self.workspace));
        }
        if self.config.format == OutputFormat::Alac {
            if let Err(error) = verify_alac_frames(&self.workspace.file, self.submitted_frames, &mut cancelled) {
                return Err(with_cleanup(error, &mut self.workspace));
            }
            if let Err(error) = self.workspace.check_owned_file() {
                let error = io_error("reinspect codec output after ALAC verification", &self.workspace.file, error);
                return Err(with_cleanup(error, &mut self.workspace));
            }
            if cancelled() { return Err(with_cleanup(CodecError::Cancelled, &mut self.workspace)); }
        }
        if let Err(error) = register(self.workspace.identity.as_ref().expect("pinned codec output")) {
            let error = io_error("register codec staging file", &self.target, error);
            return Err(with_cleanup(error, &mut self.workspace));
        }
        if let Err(e) = fs::hard_link(&self.workspace.file, &self.target) {
            let error = io_error("publish new output", &self.target, e);
            return Err(with_cleanup(error, &mut self.workspace));
        }
        if let Err(source) = self.workspace.cleanup() {
            return Err(CodecError::PublishedButCleanupFailed { path: self.target.clone(), source });
        }
        Ok(())
    }
    /// Finalize and remove the private unfinished file without publishing it.
    pub fn abort(mut self) -> Result<(), CodecError> {
        let close = self.handle.close();
        if let Err(error) = close { return Err(with_cleanup(error, &mut self.workspace)); }
        self.workspace.cleanup().map_err(|source| io_error("remove codec workspace", &self.workspace.directory, source))
    }
}
// Field drop order disposes the handle before removing the workspace. Explicit
// finish/abort report errors; an abandoned writer performs best-effort cleanup.

/// The persistent PCM conversion used by PN's MP3 recorder before LAME.
/// No channel map, dither, or other converter property is overridden.
pub struct Pcm16StereoConverter {
    handle: Option<NonNull<c_void>>,
    input_channels: u32,
    failed: bool,
}
impl Pcm16StereoConverter {
    pub fn new(sample_rate: f64, input_channels: u32) -> Result<Self, CodecError> {
        let input = float_format(sample_rate, input_channels)?;
        let output = StreamDescription { sample_rate, format_id: LPCM, format_flags: SIGNED_PACKED, bytes_per_packet: 4, frames_per_packet: 1, bytes_per_frame: 4, channels_per_frame: 2, bits_per_channel: 16, reserved: 0 };
        let mut raw = ptr::null_mut();
        let status = unsafe { AudioConverterNew(&input, &output, &mut raw) };
        if status != 0 {
            let primary = CodecError::Native { operation: "AudioConverterNew", status };
            let close = match NonNull::new(raw) {
                Some(p) => native_status("AudioConverterDispose", unsafe { AudioConverterDispose(p.as_ptr()) }),
                None => Ok(()),
            };
            return Err(with_native_cleanup(primary, close));
        }
        let handle = NonNull::new(raw).ok_or(CodecError::UnexpectedNativeResult("null AudioConverterRef after successful call"))?;
        Ok(Self { handle: Some(handle), input_channels, failed: false })
    }
    /// Converts a block into the first `frames * 2` output samples. Returns
    /// the number of complete stereo frames written.
    pub fn convert(&mut self, input: &[f32], output: &mut [i16]) -> Result<usize, CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        let (frames, input_bytes) = buffer_length(input.len(), self.input_channels, size_of::<f32>())?;
        let output_samples = (frames as usize).checked_mul(2).ok_or(CodecError::InvalidInput("output frame size overflows"))?;
        if output.len() < output_samples { return Err(CodecError::InvalidInput("output buffer is too small")); }
        let mut output_bytes = frames.checked_mul(4).ok_or(CodecError::InvalidInput("output buffer is too large"))?;
        if frames == 0 { return Ok(0); }
        let status = unsafe { AudioConverterConvertBuffer(self.handle.expect("live AudioConverterRef").as_ptr(), input_bytes, input.as_ptr().cast(), &mut output_bytes, output.as_mut_ptr().cast()) };
        if status != 0 { self.failed = true; return Err(CodecError::Native { operation: "AudioConverterConvertBuffer", status }); }
        if output_bytes != frames * 4 {
            self.failed = true;
            return Err(CodecError::UnexpectedNativeResult("PCM converter did not produce one stereo frame per input frame"));
        }
        Ok(frames as usize)
    }
    pub fn close(mut self) -> Result<(), CodecError> { self.dispose() }
    fn dispose(&mut self) -> Result<(), CodecError> {
        match self.handle.take() {
            Some(p) => native_status("AudioConverterDispose", unsafe { AudioConverterDispose(p.as_ptr()) }),
            None => Ok(()),
        }
    }
}
impl Drop for Pcm16StereoConverter { fn drop(&mut self) { let _ = self.dispose(); } }

/// Diagnostic clock control for AudioToolbox MP4 output. The recorder stores
/// its wall-clock close second (since 1904) in the creation and modification
/// fields of `mvhd` and of every track's `tkhd` and `mdhd`. Replacing them with
/// a supplied second makes a candidate comparable to a native export whose
/// recorded value is known. Returns the number of patched fields.
pub fn set_mp4_header_times(path: &Path, seconds_since_1904: u32) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom, Write};
    const MAX_MOOV: u64 = 64 * 1024 * 1024;
    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let length = file.metadata()?.len();
    let mut offset = 0u64;
    let (start, size) = loop {
        if length.saturating_sub(offset) < 8 { return Err(invalid_mp4("moov box not found")); }
        let mut header = [0u8; 16];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut header[..8])?;
        let (size, header_len) = match u32::from_be_bytes(header[..4].try_into().expect("four bytes")) {
            0 => (length - offset, 8),
            1 => { file.read_exact(&mut header[8..])?; (u64::from_be_bytes(header[8..].try_into().expect("eight bytes")), 16) }
            n => (u64::from(n), 8),
        };
        if size < header_len || size > length - offset { return Err(invalid_mp4("invalid top-level box size")); }
        if &header[4..8] == b"moov" { break (offset + header_len, size - header_len); }
        offset += size;
    };
    if size > MAX_MOOV { return Err(invalid_mp4("moov box exceeds the supported size")); }
    let mut moov = vec![0u8; size as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut moov)?;
    let patched = patch_moov_times(&mut moov, seconds_since_1904)?;
    file.seek(SeekFrom::Start(start))?;
    file.write_all(&moov)?;
    file.sync_all()?;
    Ok(patched)
}

fn invalid_mp4(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn mp4_children(data: &[u8]) -> io::Result<Vec<([u8; 4], std::ops::Range<usize>)>> {
    let mut children = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let rest = &data[offset..];
        if rest.len() < 8 { return Err(invalid_mp4("truncated child box header")); }
        let kind: [u8; 4] = rest[4..8].try_into().expect("four bytes");
        let (size, header_len) = match u32::from_be_bytes(rest[..4].try_into().expect("four bytes")) {
            0 => (rest.len(), 8),
            1 => {
                if rest.len() < 16 { return Err(invalid_mp4("truncated large box header")); }
                let size = u64::from_be_bytes(rest[8..16].try_into().expect("eight bytes"));
                (usize::try_from(size).map_err(|_| invalid_mp4("child box is too large"))?, 16)
            }
            n => (n as usize, 8),
        };
        if size < header_len || size > rest.len() { return Err(invalid_mp4("invalid child box size")); }
        children.push((kind, offset + header_len..offset + size));
        offset += size;
    }
    Ok(children)
}

fn only_child(data: &[u8], kind: &[u8; 4]) -> io::Result<std::ops::Range<usize>> {
    let mut found = mp4_children(data)?.into_iter().filter(|(k, _)| k == kind).map(|(_, r)| r);
    match (found.next(), found.next()) {
        (Some(range), None) => Ok(range),
        (None, _) => Err(invalid_mp4("required MP4 header box is missing")),
        (Some(_), Some(_)) => Err(invalid_mp4("MP4 header box is duplicated")),
    }
}

fn patch_full_box_times(payload: &mut [u8], seconds: u32) -> io::Result<()> {
    match payload.first() {
        Some(0) if payload.len() >= 12 => {
            payload[4..8].copy_from_slice(&seconds.to_be_bytes());
            payload[8..12].copy_from_slice(&seconds.to_be_bytes());
        }
        Some(1) if payload.len() >= 20 => {
            payload[4..12].copy_from_slice(&u64::from(seconds).to_be_bytes());
            payload[12..20].copy_from_slice(&u64::from(seconds).to_be_bytes());
        }
        _ => return Err(invalid_mp4("unsupported or truncated MP4 time header")),
    }
    Ok(())
}

fn patch_moov_times(moov: &mut [u8], seconds: u32) -> io::Result<usize> {
    let mvhd = only_child(moov, b"mvhd")?;
    patch_full_box_times(&mut moov[mvhd], seconds)?;
    let tracks: Vec<_> = mp4_children(moov)?.into_iter().filter(|(k, _)| k == b"trak").map(|(_, r)| r).collect();
    if tracks.is_empty() { return Err(invalid_mp4("MP4 has no track")); }
    for trak in &tracks {
        let trak = &mut moov[trak.clone()];
        let tkhd = only_child(trak, b"tkhd")?;
        patch_full_box_times(&mut trak[tkhd], seconds)?;
        let mdia = only_child(trak, b"mdia")?;
        let mdia = &mut trak[mdia];
        let mdhd = only_child(mdia, b"mdhd")?;
        patch_full_box_times(&mut mdia[mdhd], seconds)?;
    }
    Ok(2 + tracks.len() * 4)
}

#[cfg(test)]
mod writer_ownership_tests {
    use super::*;

    #[test]
    fn partial_initialization_and_registration_failures_cleanup_all_codecs() {
        let dir = std::env::temp_dir().join(format!("pn-apple-ownership-{}-{}", std::process::id(), NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).unwrap();
        for (format, name) in [(OutputFormat::Wav, "out.wav"), (OutputFormat::Aiff, "out.aiff"), (OutputFormat::Alac, "out.m4a")] {
            let path = dir.join(name);
            let config = WriterConfig { format, sample_rate: 48_000.0, channels: 2, bit_depth: 16 };
            assert!(Writer::create_impl(&path, config, true).is_err());
            assert!(!path.exists());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
            let mut writer = Writer::create(&path, config).unwrap();
            writer.write(&vec![0.0; 8192]).unwrap();
            assert!(writer.finish_cancellable_registered(|| false, |_| Err(io::Error::other("injected registration failure"))).is_err());
            assert!(!path.exists());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        }
        fs::remove_dir(dir).unwrap();
    }
}

#[cfg(test)]
mod planar_alignment_tests {
    use super::{AlignedStereoBufferList, StereoBufferList};

    #[test]
    fn planar_list_alignment_preserves_native_buffer_list_layout() {
        assert_eq!(std::mem::align_of::<AlignedStereoBufferList>(), 64);
        assert_eq!(std::mem::offset_of!(StereoBufferList, buffers), 8);
        assert_eq!(std::mem::size_of::<StereoBufferList>(), 40);
    }
}

#[cfg(test)]
mod alac_postflight_tests {
    use super::*;
    use crate::file_workflow::Cancellation;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "pn-alac-postflight-test-{}-{}", std::process::id(), NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }
    fn writer(dir: &TestDir) -> Writer {
        let mut writer = Writer::create(dir.0.join("output.m4a"), WriterConfig {
            format: OutputFormat::Alac, sample_rate: 44_100.0, channels: 2, bit_depth: 16,
        }).unwrap();
        writer.write(&vec![0.25; 8192 * 2]).unwrap();
        writer
    }

    #[test]
    fn cancellation_during_postflight_cleans_workspace_without_publication() {
        let dir = TestDir::new();
        let writer = writer(&dir);
        let cancel = Cancellation::new();
        let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel(0);
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::sync_channel(0);
        let mut checks = 0;
        let result = std::thread::scope(|scope| {
            let token = &cancel;
            scope.spawn(move || {
                decoded_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                token.cancel();
                cancelled_tx.send(()).unwrap();
            });
            writer.finish_cancellable(|| {
                checks += 1;
                // The third check is immediately after the first decoded block.
                if checks == 3 {
                    decoded_tx.send(()).unwrap();
                    cancelled_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                }
                cancel.is_cancelled()
            })
        });
        assert!(matches!(result, Err(CodecError::Cancelled)));
        assert_eq!(checks, 3);
        assert!(!dir.0.join("output.m4a").exists());
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn cancellation_preserves_workspace_cleanup_failure() {
        let dir = TestDir::new();
        let writer = writer(&dir);
        let staged = writer.workspace.file.clone();
        let retained = writer.workspace.directory.join("retained-original");
        let mut checks = 0;
        let result = writer.finish_cancellable(|| {
            checks += 1;
            if checks == 3 {
                fs::rename(&staged, &retained).unwrap();
                fs::write(&staged, b"replacement").unwrap();
                true
            } else { false }
        });
        match result {
            Err(CodecError::Cleanup { primary, source }) => {
                assert!(matches!(*primary, CodecError::Cancelled));
                assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected cancellation with cleanup failure, got {other:?}"),
        }
        assert!(!dir.0.join("output.m4a").exists());
        assert_eq!(fs::read(&staged).unwrap(), b"replacement");
        assert!(retained.exists());
    }
}

#[cfg(test)]
mod mp4_time_tests {
    use super::*;

    fn full_box(kind: &[u8; 4], version: u8, body: usize) -> Vec<u8> {
        let mut b = ((8 + 4 + body) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(&[version, 0, 0, 0]);
        b.extend((0..body).map(|i| 0xa0 ^ i as u8));
        b
    }
    fn container(kind: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = children.concat();
        let mut b = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend(body);
        b
    }

    #[test]
    fn patches_exactly_the_six_time_fields_of_versions_0_and_1() {
        let moov = container(b"moov", &[full_box(b"mvhd", 0, 96),
            container(b"trak", &[full_box(b"tkhd", 1, 92), container(b"mdia", &[full_box(b"mdhd", 0, 20), full_box(b"hdlr", 0, 25)])])]);
        let mut moov_body = moov[8..].to_vec();
        let before = moov_body.clone();
        assert_eq!(patch_moov_times(&mut moov_body, 0xE706_F05D).unwrap(), 6);
        let changed: Vec<usize> = (0..before.len()).filter(|&i| before[i] != moov_body[i]).collect();
        // Offsets in the moov body: mvhd v0 creation at 12; trak at 108, tkhd v1
        // creation at 108+8+12; mdia after the 104-byte tkhd, mdhd v0 creation +20.
        let mvhd = 12;
        let tkhd = 108 + 8 + 12;
        let mdhd = tkhd + 92 + 8 + 12;
        assert_eq!(&moov_body[mvhd..mvhd + 8], &[0xE7, 0x06, 0xF0, 0x5D, 0xE7, 0x06, 0xF0, 0x5D]);
        assert_eq!(&moov_body[tkhd..tkhd + 16], &[0, 0, 0, 0, 0xE7, 0x06, 0xF0, 0x5D, 0, 0, 0, 0, 0xE7, 0x06, 0xF0, 0x5D]);
        assert_eq!(&moov_body[mdhd..mdhd + 8], &[0xE7, 0x06, 0xF0, 0x5D, 0xE7, 0x06, 0xF0, 0x5D]);
        assert!(changed.iter().all(|&i| (mvhd..mvhd + 8).contains(&i) || (tkhd..tkhd + 16).contains(&i) || (mdhd..mdhd + 8).contains(&i)));
    }

    #[test]
    fn rejects_missing_or_duplicated_time_headers() {
        let missing = container(b"moov", &[full_box(b"mvhd", 0, 96), container(b"trak", &[full_box(b"tkhd", 0, 80)])]);
        let duplicated = container(b"moov", &[full_box(b"mvhd", 0, 96), full_box(b"mvhd", 0, 96)]);
        for moov in [missing, duplicated] {
            assert_eq!(patch_moov_times(&mut moov[8..].to_vec(), 1).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }
}

#[cfg(test)]
mod sp_pin_tests {
    use super::call_with_sp_residue;

    std::arch::global_asm!(
        ".text",
        ".p2align 2",
        // usize pn_test_probe_sp(): the stack pointer at entry (no prologue touches it).
        ".globl _pn_test_probe_sp",
        "_pn_test_probe_sp:",
        "mov x0, sp",
        "ret",
        ".globl _pn_test_sum3",
        "_pn_test_sum3:",
        "add x0, x0, x1",
        "add x0, x0, x2",
        "ret",
        // i32 pn_test_check_callee_saved(function, a0, a1, a2, residue): runs the real trampoline
        // with x19-x28 and d8-d15 holding known values; returns its result, or -1 if any changed.
        ".globl _pn_test_check_callee_saved",
        "_pn_test_check_callee_saved:",
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        "stp x19, x20, [sp, #-16]!",
        "stp x21, x22, [sp, #-16]!",
        "stp x23, x24, [sp, #-16]!",
        "stp x25, x26, [sp, #-16]!",
        "stp x27, x28, [sp, #-16]!",
        "stp d8, d9, [sp, #-16]!",
        "stp d10, d11, [sp, #-16]!",
        "stp d12, d13, [sp, #-16]!",
        "stp d14, d15, [sp, #-16]!",
        "mov x19, #19", "mov x20, #20", "mov x21, #21", "mov x22, #22", "mov x23, #23",
        "mov x24, #24", "mov x25, #25", "mov x26, #26", "mov x27, #27", "mov x28, #28",
        "fmov d8, x19", "fmov d9, x20", "fmov d10, x21", "fmov d11, x22",
        "fmov d12, x23", "fmov d13, x24", "fmov d14, x25", "fmov d15, x26",
        "bl _call_with_sp_residue",
        "mov x9, x0",
        "cmp x19, #19", "b.ne 1f", "cmp x20, #20", "b.ne 1f", "cmp x21, #21", "b.ne 1f",
        "cmp x22, #22", "b.ne 1f", "cmp x23, #23", "b.ne 1f", "cmp x24, #24", "b.ne 1f",
        "cmp x25, #25", "b.ne 1f", "cmp x26, #26", "b.ne 1f", "cmp x27, #27", "b.ne 1f",
        "cmp x28, #28", "b.ne 1f",
        "fmov x10, d8", "cmp x10, #19", "b.ne 1f", "fmov x10, d9", "cmp x10, #20", "b.ne 1f",
        "fmov x10, d10", "cmp x10, #21", "b.ne 1f", "fmov x10, d11", "cmp x10, #22", "b.ne 1f",
        "fmov x10, d12", "cmp x10, #23", "b.ne 1f", "fmov x10, d13", "cmp x10, #24", "b.ne 1f",
        "fmov x10, d14", "cmp x10, #25", "b.ne 1f", "fmov x10, d15", "cmp x10, #26", "b.ne 1f",
        "b 2f",
        "1:",
        "mov x9, #-1",
        "2:",
        "mov x0, x9",
        "ldp d14, d15, [sp], #16",
        "ldp d12, d13, [sp], #16",
        "ldp d10, d11, [sp], #16",
        "ldp d8, d9, [sp], #16",
        "ldp x27, x28, [sp], #16",
        "ldp x25, x26, [sp], #16",
        "ldp x23, x24, [sp], #16",
        "ldp x21, x22, [sp], #16",
        "ldp x19, x20, [sp], #16",
        "ldp x29, x30, [sp], #16",
        "ret",
    );
    unsafe extern "C" {
        fn pn_test_probe_sp() -> usize;
        fn pn_test_sum3(a: usize, b: usize, c: usize) -> usize;
        fn pn_test_check_callee_saved(function: usize, a0: usize, a1: usize, a2: usize, residue: usize) -> i32;
    }

    fn current_sp() -> usize {
        let sp: usize;
        // SAFETY: reads the stack pointer register only.
        unsafe { std::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags)) };
        sp
    }

    #[test]
    fn the_callee_enters_with_the_requested_residue_whatever_the_callers_alignment() {
        fn check<const PAD: usize>(residue: usize) {
            let mut pad = [0u8; PAD];
            std::hint::black_box(&mut pad);
            let before = current_sp();
            // SAFETY: the probe is a C-ABI function without arguments; it returns its entry sp.
            let seen = unsafe { call_with_sp_residue(pn_test_probe_sp as *const () as usize, 0, 0, 0, residue) };
            assert_eq!(current_sp(), before, "the caller's stack pointer is restored (pad {PAD}, residue {residue})");
            // The i32 result keeps the low 32 bits of the entry sp, which holds `sp % 64`.
            assert_eq!(seen as u32 as usize % 64, residue, "entry residue (pad {PAD})");
        }
        for residue in [0usize, 16, 32, 48] {
            check::<0>(residue); check::<16>(residue); check::<32>(residue); check::<48>(residue);
            check::<64>(residue); check::<96>(residue); check::<128>(residue); check::<200>(residue);
        }
    }

    #[test]
    fn arguments_and_the_result_pass_through_unchanged() {
        for residue in [0usize, 16, 32, 48] {
            // SAFETY: `pn_test_sum3` is a C-ABI function of three integers.
            let sum = unsafe { call_with_sp_residue(pn_test_sum3 as *const () as usize, 1, 2, 3, residue) };
            assert_eq!(sum, 6);
            let sum = unsafe { call_with_sp_residue(pn_test_sum3 as *const () as usize, 40_000, 2, 0x7fff_0000, residue) };
            assert_eq!(sum, 0x7fff_0000 + 40_002);
        }
    }

    #[test]
    fn callee_saved_registers_survive_the_trampoline() {
        for residue in [0usize, 16, 32, 48] {
            // SAFETY: the shim saves and restores every register it uses and calls the real trampoline.
            let result = unsafe { pn_test_check_callee_saved(pn_test_sum3 as *const () as usize, 1, 2, 3, residue) };
            assert_eq!(result, 6, "x19-x28 and d8-d15 must be unchanged (residue {residue})");
        }
    }

    #[test]
    fn the_pins_are_16_byte_aligned_residues() {
        for residue in [super::WRITE_SP_RESIDUE, super::DISPOSE_SP_RESIDUE, super::READ_SP_RESIDUE] {
            assert!(residue < 64 && residue % 16 == 0);
        }
    }

    /// The SME kernels run at `sp % 64 == 48` (encoder write), `32` (encoder drain in dispose) and
    /// `16` (decoder read). Every pin must stay off its own class so all three calls run the NEON kernel.
    #[test]
    fn no_pin_selects_the_sme_class_of_its_call() {
        assert_ne!(super::WRITE_SP_RESIDUE, 48);
        assert_ne!(super::DISPOSE_SP_RESIDUE, 32);
        assert_ne!(super::READ_SP_RESIDUE, 16);
    }

    /// The read wrapper is the same trampoline: the callee sees the read pin's residue whatever the
    /// caller's alignment, and the call's three pointer arguments and its status pass through unchanged.
    #[test]
    fn the_read_pin_enters_its_callee_at_the_read_residue() {
        fn check<const PAD: usize>() {
            let mut pad = [0u8; PAD];
            std::hint::black_box(&mut pad);
            // SAFETY: the probe is a C-ABI function that ignores its arguments and returns its entry sp.
            let seen = unsafe {
                super::read_pinned_via(pn_test_probe_sp as *const () as usize, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            };
            assert_eq!(seen as u32 as usize % 64, super::READ_SP_RESIDUE, "pad {PAD}");
            // SAFETY: `pn_test_sum3` adds its three integer arguments; the pointers are never dereferenced.
            let sum = unsafe {
                super::read_pinned_via(pn_test_sum3 as *const () as usize, 7 as *mut _, 11 as *mut _, 13 as *mut _)
            };
            assert_eq!(sum, 31, "pad {PAD}");
        }
        check::<0>(); check::<16>(); check::<32>(); check::<48>(); check::<64>(); check::<112>(); check::<200>();
    }
}
