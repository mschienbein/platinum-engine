//! macOS AudioToolbox codec boundary used by the reference's Apple recorders.
//!
//! This module produces codec/container bytes only: it does not copy tags or
//! write the Platinum Notes marker. Full-file compatibility remains unproven.
#![cfg(target_os = "macos")]

use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
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
            Some(p) => native_status("ExtAudioFileDispose", unsafe { ExtAudioFileDispose(p.as_ptr()) }),
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
        let mut list = StereoBufferList {
            number_buffers: 2,
            buffers: [
                AudioBuffer { number_channels: 1, data_byte_size: bytes, data: left.as_mut_ptr().cast() },
                AudioBuffer { number_channels: 1, data_byte_size: bytes, data: right.as_mut_ptr().cast() },
            ],
        };
        // AudioBufferList's variable-size ABI uses its count, not the C/Rust
        // declaration's trailing array length. Both entries are live here.
        let status = unsafe { ExtAudioFileRead(self.handle.ptr(), &mut frames, (&mut list as *mut StereoBufferList).cast()) };
        if status != 0 { self.failed = true; return Err(CodecError::Native { operation: "ExtAudioFileRead(planar stereo)", status }); }
        if frames > requested || list.number_buffers != 2 || list.buffers.iter().any(|buffer| buffer.data_byte_size > bytes) {
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
        let mut list = audio_buffer(self.client.channels_per_frame, bytes, output.as_mut_ptr().cast());
        let status = unsafe { ExtAudioFileRead(self.handle.ptr(), &mut frames, &mut list) };
        if status != 0 { self.failed = true; return Err(CodecError::Native { operation: "ExtAudioFileRead", status }); }
        if frames > requested || list.buffers[0].data_byte_size > bytes {
            self.failed = true;
            return Err(CodecError::UnexpectedNativeResult("read exceeded supplied buffer"));
        }
        Ok(frames as usize)
    }
    /// Explicit close makes native disposal errors observable; Drop is best effort.
    pub fn close(mut self) -> Result<(), CodecError> { self.handle.close() }
}

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

// The reference requests erase-existing (1); we deliberately pass zero so
// AudioToolbox must fail on an existing path. Publication is a no-replace hard
// link from a private sibling workspace after successful native finalization.
struct Workspace {
    directory: PathBuf,
    file: PathBuf,
    identity: Option<(u64, u64)>,
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
        let metadata = fs::symlink_metadata(&self.file).map_err(|e| io_error("inspect created codec file", &self.file, e))?;
        if !metadata.file_type().is_file() {
            return Err(CodecError::UnexpectedNativeResult("created codec output is not a regular file"));
        }
        self.identity = Some((metadata.dev(), metadata.ino()));
        Ok(())
    }
    fn check_owned_file(&self) -> io::Result<()> {
        let metadata = fs::symlink_metadata(&self.file)?;
        if metadata.file_type().is_file() && Some((metadata.dev(), metadata.ino())) == self.identity {
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
}

impl Writer {
    pub fn create(path: impl AsRef<Path>, config: WriterConfig) -> Result<Self, CodecError> {
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
            let primary = CodecError::Native { operation: "ExtAudioFileCreateWithURL", status };
            let close = match NonNull::new(raw) {
                Some(p) => native_status("ExtAudioFileDispose", unsafe { ExtAudioFileDispose(p.as_ptr()) }),
                None => Ok(()),
            };
            return Err(with_cleanup(with_native_cleanup(primary, close), &mut workspace));
        }
        let mut handle = match ExtHandle::new(raw) { Ok(handle) => handle, Err(e) => return Err(with_cleanup(e, &mut workspace)) };
        if let Err(error) = workspace.remember_created_file() {
            let primary = with_native_cleanup(error, handle.close());
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
        Ok(Self { handle, workspace, target, config, warnings, failed: false })
    }
    pub fn config(&self) -> WriterConfig { self.config }
    pub fn warnings(&self) -> &[CodecWarning] { &self.warnings }
    /// Writes one complete interleaved float32 block. The controller normally
    /// supplies 512-frame blocks; the last block can be shorter.
    pub fn write(&mut self, input: &[f32]) -> Result<(), CodecError> {
        if self.failed { return Err(CodecError::Poisoned); }
        let (frames, bytes) = buffer_length(input.len(), self.config.channels, size_of::<f32>())?;
        if frames == 0 { return Ok(()); }
        let list = audio_buffer(self.config.channels, bytes, input.as_ptr().cast_mut().cast());
        let status = unsafe { ExtAudioFileWrite(self.handle.ptr(), frames, &list) };
        if status != 0 { self.failed = true; return Err(CodecError::Native { operation: "ExtAudioFileWrite", status }); }
        Ok(())
    }
    /// Dispose/flush the codec and publish without replacing any existing path.
    /// A hard-link failure (including an existing target) is returned as an
    /// error; filesystems that do not support hard links are not silently altered.
    pub fn finish(mut self) -> Result<(), CodecError> {
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
