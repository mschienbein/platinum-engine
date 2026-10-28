//! PNFLACRecorder-compatible float conversion and libFLAC encoder wrapper.
//!
//! This is an encoder-stage implementation, not a complete Platinum Notes
//! output file: later tag copying and the PlatinumNotes marker are separate.
//! The caller supplies a trusted, open-source libFLAC 1.4.2 and a new, empty,
//! seekable staging file. The library's on-disk identity is recorded; matching
//! the version and visible defaults does not prove matching build options or
//! byte parity with the application's statically linked copy.

use libloading::Library;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

const VERSION: &str = "1.4.2";
const VENDOR: &str = "reference libFLAC 1.4.2 20221022";
const ENCODER_UNINITIALIZED: c_int = 1;

/// Evidence about the supplied library, not proof of its source or build flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryIdentity {
    pub canonical_path: PathBuf,
    pub file_sha256: [u8; 32],
    pub version: String,
    pub vendor: String,
    pub ogg_supported: bool,
}

#[derive(Debug)]
pub enum FlacError {
    Io { operation: &'static str, source: io::Error },
    Library(String),
    #[cfg(target_os = "linux")]
    LibraryPolicy(crate::linux_libraries::LibraryPolicyError),
    InvalidDigest,
    DigestMismatch { expected: String, actual: String },
    IncompatibleLibrary { version: String, vendor: String },
    UnexpectedDefault { name: &'static str, expected: u64, actual: u64 },
    InvalidStagingFile(&'static str),
    Allocation,
    IncompleteFrame { samples: usize, channels: u32 },
    TooManyFrames(usize),
    Encoder { operation: &'static str, code: i32, state: i32 },
    EncoderFailed,
}

impl fmt::Display for FlacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "{operation}: {source}"),
            Self::Library(message) => write!(f, "libFLAC: {message}"),
            #[cfg(target_os = "linux")]
            Self::LibraryPolicy(error) => write!(f, "{error}"),
            Self::InvalidDigest => f.write_str("expected libFLAC SHA-256 must be 64 hexadecimal digits"),
            Self::DigestMismatch { expected, actual } => write!(f, "libFLAC SHA-256 mismatch: expected {expected}, got {actual}"),
            Self::IncompatibleLibrary { version, vendor } => write!(f, "expected libFLAC {VERSION} and vendor {VENDOR:?}, got version {version:?}, vendor {vendor:?}"),
            Self::UnexpectedDefault { name, expected, actual } => write!(f, "libFLAC {name}: expected {expected}, got {actual}"),
            Self::InvalidStagingFile(message) => write!(f, "invalid FLAC staging file: {message}"),
            Self::Allocation => write!(f, "could not allocate encoder memory"),
            Self::IncompleteFrame { samples, channels } => write!(f, "{samples} samples do not form complete {channels}-channel frames"),
            Self::TooManyFrames(frames) => write!(f, "{frames} frames exceed libFLAC's per-call limit"),
            Self::Encoder { operation, code, state } => write!(f, "libFLAC {operation} failed: result {code}, encoder state {state}"),
            Self::EncoderFailed => write!(f, "FLAC encoding previously failed; output must not be published"),
        }
    }
}

impl std::error::Error for FlacError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            #[cfg(target_os = "linux")]
            Self::LibraryPolicy(error) => Some(error),
            _ => None,
        }
    }
}

fn io_error(operation: &'static str, source: io::Error) -> FlacError {
    FlacError::Io { operation, source }
}

type Encoder = c_void;
type New = unsafe extern "C" fn() -> *mut Encoder;
type Delete = unsafe extern "C" fn(*mut Encoder);
type SetU32 = unsafe extern "C" fn(*mut Encoder, u32) -> c_int;
type GetU32 = unsafe extern "C" fn(*const Encoder) -> u32;
type GetU64 = unsafe extern "C" fn(*const Encoder) -> u64;
type GetState = unsafe extern "C" fn(*const Encoder) -> c_int;
type WriteCallback = unsafe extern "C" fn(*const Encoder, *const u8, usize, u32, u32, *mut c_void) -> c_int;
type SeekCallback = unsafe extern "C" fn(*const Encoder, u64, *mut c_void) -> c_int;
type TellCallback = unsafe extern "C" fn(*const Encoder, *mut u64, *mut c_void) -> c_int;
type MetadataCallback = unsafe extern "C" fn(*const Encoder, *const c_void, *mut c_void);
type InitStream = unsafe extern "C" fn(*mut Encoder, Option<WriteCallback>, Option<SeekCallback>, Option<TellCallback>, Option<MetadataCallback>, *mut c_void) -> c_int;
type Process = unsafe extern "C" fn(*mut Encoder, *const i32, u32) -> c_int;
type Finish = unsafe extern "C" fn(*mut Encoder) -> c_int;

struct Functions {
    new: New,
    delete: Delete,
    channels: SetU32,
    sample_rate: SetU32,
    bits_per_sample: SetU32,
    init_stream: InitStream,
    process: Process,
    finish: Finish,
    get_state: GetState,
}

/// A caller-selected open-source libFLAC. Loading a dylib executes its normal
/// initializers; the caller is responsible for choosing a trusted file.
pub struct FlacLibrary {
    library: std::sync::Arc<Library>,
    _pinned: crate::native_library::PinnedLibrary,
    functions: Functions,
    identity: LibraryIdentity,
}

unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, FlacError> {
    // SAFETY: callers specify the exported symbol's ABI and keep the Library
    // alive for all uses of the returned pointer.
    unsafe { library.get::<T>(name) }
        .map(|value| *value)
        .map_err(|error| FlacError::Library(error.to_string()))
}

unsafe fn string_symbol(library: &Library, name: &[u8]) -> Result<String, FlacError> {
    // dlsym returns the address of the exported `const char *` object.
    let object: *const *const c_char = unsafe { symbol(library, name)? };
    if object.is_null() {
        return Err(FlacError::Library("null libFLAC identity symbol".into()));
    }
    // SAFETY: libFLAC exports a live pointer object at this address.
    let value = unsafe { *object };
    if value.is_null() {
        return Err(FlacError::Library("null libFLAC identity string".into()));
    }
    // SAFETY: libFLAC exposes static, NUL-terminated strings until unload.
    let value = unsafe { CStr::from_ptr(value) };
    value.to_str().map(str::to_owned).map_err(|error| FlacError::Library(error.to_string()))
}

impl FlacLibrary {
    /// Load exactly the specified library; no application-bundled lookup or
    /// fallback occurs. A version/vendor match is necessary, not sufficient,
    /// for reference equivalence.
    pub fn load(path: impl AsRef<Path>, expected_sha256: &str) -> Result<Self, FlacError> {
        if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(FlacError::InvalidDigest);
        }
        let path = std::fs::canonicalize(path).map_err(|e| io_error("resolve libFLAC path", e))?;
        #[cfg(target_os = "macos")]
        let pinned = crate::native_library::PinnedLibrary::open(&path).map_err(|e| FlacError::Library(e.to_string()))?;
        #[cfg(target_os = "linux")]
        let pinned = crate::linux_libraries::open_approved(&path, crate::linux_libraries::LibraryKind::Flac).map_err(FlacError::LibraryPolicy)?;
        let sha256 = pinned.digest();
        if !pinned.digest_matches(expected_sha256) {
            return Err(FlacError::DigestMismatch {
                expected: expected_sha256.to_ascii_lowercase(), actual: pinned.digest_hex(),
            });
        }
        // SAFETY: the caller explicitly supplies a trusted compatible library.
        #[cfg(target_os = "macos")]
        let library = unsafe { pinned.load() }.map_err(|e| FlacError::Library(e.to_string()))?;
        #[cfg(target_os = "linux")]
        let library = unsafe { pinned.load() }.map_err(|e| FlacError::LibraryPolicy(e.into()))?;
        // SAFETY: libFLAC defines these symbols with the declared C ABI.
        let (version, vendor, ogg_supported, functions) = unsafe {
            let new: New = symbol(&library, b"FLAC__stream_encoder_new\0")?;
            #[cfg(target_os = "macos")]
            pinned.verify_symbol(new as *const () as *const c_void).map_err(|e| FlacError::Library(e.to_string()))?;
            #[cfg(target_os = "linux")]
            pinned.verify_symbol(new as *const () as *const c_void).map_err(|e| FlacError::LibraryPolicy(e.into()))?;
            let version = string_symbol(&library, b"FLAC__VERSION_STRING\0")?;
            let vendor = string_symbol(&library, b"FLAC__VENDOR_STRING\0")?;
            if version != VERSION || vendor != VENDOR {
                return Err(FlacError::IncompatibleLibrary { version, vendor });
            }
            let ogg: *const c_int = symbol(&library, b"FLAC_API_SUPPORTS_OGG_FLAC\0")?;
            if ogg.is_null() {
                return Err(FlacError::Library("null libFLAC Ogg capability symbol".into()));
            }
            let ogg_supported = *ogg != 0;
            let functions = Functions {
                new,
                delete: symbol(&library, b"FLAC__stream_encoder_delete\0")?,
                channels: symbol(&library, b"FLAC__stream_encoder_set_channels\0")?,
                sample_rate: symbol(&library, b"FLAC__stream_encoder_set_sample_rate\0")?,
                bits_per_sample: symbol(&library, b"FLAC__stream_encoder_set_bits_per_sample\0")?,
                init_stream: symbol(&library, b"FLAC__stream_encoder_init_stream\0")?,
                process: symbol(&library, b"FLAC__stream_encoder_process_interleaved\0")?,
                finish: symbol(&library, b"FLAC__stream_encoder_finish\0")?,
                get_state: symbol(&library, b"FLAC__stream_encoder_get_state\0")?,
            };
            (version, vendor, ogg_supported, functions)
        };
        Ok(Self { library, functions, identity: LibraryIdentity { canonical_path: path, file_sha256: sha256, version, vendor, ogg_supported }, _pinned: pinned })
    }

    pub fn identity(&self) -> &LibraryIdentity { &self.identity }
}

/// PN clamps the unsigned requested depth before passing it to libFLAC.
pub fn effective_bit_depth(requested: u32) -> u32 { requested.clamp(16, 24) }

/// Exact ARM64 sample conversion, including the native unordered/NaN branch.
/// Only depth 24 uses the 24-bit multiplier; all other clamped depths use
/// 32767 and the 16-bit clamp. libFLAC's streamable-subset check may reject a
/// depth after this clamp (for example 17 or 23).
pub fn quantize_sample(sample: f32, requested_depth: u32) -> i32 {
    let (maximum, minimum) = if effective_bit_depth(requested_depth) == 24 {
        (8_388_607.0_f32, -8_388_608.0_f32)
    } else {
        (32_767.0_f32, -32_768.0_f32)
    };
    let scaled = sample * maximum;
    // FCMP/FCSEL MI selects the maximum for unordered values, unlike
    // Rust's float-to-int conversion of NaN to zero.
    let bounded_above = if scaled < maximum { scaled } else { maximum };
    let bounded = if bounded_above > minimum { bounded_above } else { minimum };
    bounded as i32
}

struct Sink {
    file: Option<File>,
    error: Option<io::Error>,
}

impl Sink {
    fn fail(&mut self, error: io::Error) -> c_int {
        if self.error.is_none() { self.error = Some(error); }
        1
    }
}

unsafe extern "C" fn write_callback(_encoder: *const Encoder, bytes: *const u8, len: usize, _samples: u32, _frame: u32, client: *mut c_void) -> c_int {
    // SAFETY: init_stream receives the stable address of a live Box<Sink>;
    // libFLAC invokes callbacks synchronously and supplies `len` readable bytes.
    let sink = unsafe { &mut *client.cast::<Sink>() };
    if len == 0 { return 0; }
    if bytes.is_null() { return sink.fail(io::Error::new(io::ErrorKind::InvalidData, "null FLAC write buffer")); }
    let data = unsafe { std::slice::from_raw_parts(bytes, len) };
    match sink.file.as_mut().expect("live FLAC sink").write_all(data) {
        Ok(()) => 0,
        Err(error) => sink.fail(error),
    }
}

unsafe extern "C" fn seek_callback(_encoder: *const Encoder, offset: u64, client: *mut c_void) -> c_int {
    // SAFETY: the client pointer is the stable Box<Sink> supplied at init.
    let sink = unsafe { &mut *client.cast::<Sink>() };
    if offset > i64::MAX as u64 { return sink.fail(io::Error::new(io::ErrorKind::InvalidInput, "FLAC seek offset exceeds off_t")); }
    match sink.file.as_mut().expect("live FLAC sink").seek(SeekFrom::Start(offset)) {
        Ok(_) => 0,
        Err(error) => sink.fail(error),
    }
}

unsafe extern "C" fn tell_callback(_encoder: *const Encoder, offset: *mut u64, client: *mut c_void) -> c_int {
    // SAFETY: the client pointer is the stable Box<Sink> supplied at init.
    let sink = unsafe { &mut *client.cast::<Sink>() };
    if offset.is_null() { return sink.fail(io::Error::new(io::ErrorKind::InvalidInput, "null FLAC tell destination")); }
    match sink.file.as_mut().expect("live FLAC sink").stream_position() {
        Ok(position) => {
            // SAFETY: libFLAC provided a writable uint64_t destination.
            unsafe { *offset = position; }
            0
        }
        Err(error) => sink.fail(error),
    }
}

/// Streaming encoder. Dropping without `finish` abandons the staging file; it
/// does not produce a successful result or publish/remove that file.
pub struct FlacEncoder<'a> {
    library: &'a FlacLibrary,
    encoder: NonNull<Encoder>,
    sink: Box<Sink>,
    channels: u32,
    bit_depth: u32,
    scratch: Vec<i32>,
    failed: bool,
}

impl<'a> FlacEncoder<'a> {
    /// `file` must be a freshly created, empty regular file positioned at
    /// zero, opened for writing without append, with no concurrent access via
    /// other handles. The caller owns its pathname and is responsible for
    /// cleanup or publication after successful finish. Sample rate follows the
    /// native f64-to-u32 saturating truncation.
    pub fn new(library: &'a FlacLibrary, mut file: File, sample_rate: f64, channels: u32, requested_depth: u32) -> Result<Self, FlacError> {
        validate_staging_file(&mut file)?;
        // SAFETY: the function pointer comes from the retained libFLAC handle.
        let encoder = NonNull::new(unsafe { (library.functions.new)() }).ok_or(FlacError::Allocation)?;
        let mut this = Self {
            library, encoder, sink: Box::new(Sink { file: Some(file), error: None }),
            channels, bit_depth: effective_bit_depth(requested_depth), scratch: Vec::new(), failed: false,
        };
        this.check_defaults()?;
        this.set("set channels", library.functions.channels, channels)?;
        this.set("set sample rate", library.functions.sample_rate, sample_rate as u32)?;
        this.set("set bits per sample", library.functions.bits_per_sample, this.bit_depth)?;
        // SAFETY: pointers and callbacks stay valid until delete; init is
        // synchronous. Sink has a stable address even if this struct moves.
        let result = unsafe {
            (library.functions.init_stream)(encoder.as_ptr(), Some(write_callback), Some(seek_callback), Some(tell_callback), None, (&mut *this.sink as *mut Sink).cast())
        };
        if result != 0 { return Err(this.error("initialize", result)); }
        this.check_u32(b"FLAC__stream_encoder_get_blocksize\0", "initialized blocksize", 4096)?;
        this.check_u32(b"FLAC__stream_encoder_get_qlp_coeff_precision\0", "initialized QLP precision", if this.bit_depth == 16 { 12 } else { 15 })?;
        Ok(this)
    }

    pub fn bit_depth(&self) -> u32 { self.bit_depth }

    /// Encode channel-interleaved f32 frames. Partitions need not align to
    /// FLAC's 4096-frame blocks; an empty call is a no-op like the recorder.
    pub fn process_interleaved(&mut self, input: &[f32]) -> Result<(), FlacError> {
        if self.failed { return Err(FlacError::EncoderFailed); }
        if self.channels == 0 || input.len() % self.channels as usize != 0 {
            return Err(FlacError::IncompleteFrame { samples: input.len(), channels: self.channels });
        }
        let frames = input.len() / self.channels as usize;
        let frames_u32 = u32::try_from(frames).map_err(|_| FlacError::TooManyFrames(frames))?;
        if frames == 0 { return Ok(()); }
        self.scratch.clear();
        self.scratch.try_reserve(input.len()).map_err(|_| FlacError::Allocation)?;
        self.scratch.extend(input.iter().map(|&sample| quantize_sample(sample, self.bit_depth)));
        // SAFETY: scratch contains channels * frames valid i32 samples and
        // the encoder and retained library live throughout the call.
        let result = unsafe { (self.library.functions.process)(self.encoder.as_ptr(), self.scratch.as_ptr(), frames_u32) };
        if result == 0 {
            self.failed = true;
            return Err(self.error("process interleaved", result));
        }
        Ok(())
    }

    /// Flush the final frame and perform the native seek-and-rewrite of
    /// STREAMINFO. Returns the same caller-owned file on success; its position
    /// is left where libFLAC's final metadata write ended, not at EOF.
    pub fn finish(mut self) -> Result<File, FlacError> {
        if self.failed { return Err(FlacError::EncoderFailed); }
        // SAFETY: the encoder is initialized and its callback sink is alive.
        let result = unsafe { (self.library.functions.finish)(self.encoder.as_ptr()) };
        let state = self.state();
        if result == 0 || state != ENCODER_UNINITIALIZED || self.sink.error.is_some() {
            return Err(self.error("finish", result));
        }
        let mut file = self.sink.file.take().expect("live FLAC sink");
        file.flush().map_err(|e| io_error("flush FLAC staging file", e))?;
        Ok(file)
    }

    fn set(&mut self, operation: &'static str, setter: SetU32, value: u32) -> Result<(), FlacError> {
        // SAFETY: this is a live uninitialized libFLAC encoder.
        let result = unsafe { setter(self.encoder.as_ptr(), value) };
        if result == 0 { Err(self.error(operation, result)) } else { Ok(()) }
    }

    fn state(&self) -> c_int {
        // SAFETY: this is a live libFLAC encoder and library.
        unsafe { (self.library.functions.get_state)(self.encoder.as_ptr()) }
    }

    fn error(&mut self, operation: &'static str, code: i32) -> FlacError {
        if let Some(source) = self.sink.error.take() { io_error(operation, source) }
        else { FlacError::Encoder { operation, code, state: self.state() } }
    }

    fn check_u32(&self, symbol_name: &[u8], name: &'static str, expected: u32) -> Result<(), FlacError> {
        // SAFETY: each symbol below is a libFLAC uint32/bool getter; the
        // pointer and its associated library remain alive for this call.
        let getter: GetU32 = unsafe { symbol(&self.library.library, symbol_name)? };
        let actual = unsafe { getter(self.encoder.as_ptr()) };
        if actual != expected { Err(FlacError::UnexpectedDefault { name, expected: expected.into(), actual: actual.into() }) } else { Ok(()) }
    }

    fn check_defaults(&self) -> Result<(), FlacError> {
        // The original constructor's protected fields correspond to 1.4.2's
        // level-5 defaults. The application does not call a compression setter.
        for (symbol_name, name, expected) in [
            (&b"FLAC__stream_encoder_get_verify\0"[..], "verify", 0),
            (&b"FLAC__stream_encoder_get_streamable_subset\0"[..], "streamable subset", 1),
            (&b"FLAC__stream_encoder_get_do_md5\0"[..], "MD5", 1),
            (&b"FLAC__stream_encoder_get_blocksize\0"[..], "default blocksize", 0),
            (&b"FLAC__stream_encoder_get_do_mid_side_stereo\0"[..], "mid-side stereo", 1),
            (&b"FLAC__stream_encoder_get_loose_mid_side_stereo\0"[..], "loose mid-side stereo", 0),
            (&b"FLAC__stream_encoder_get_max_lpc_order\0"[..], "max LPC order", 8),
            (&b"FLAC__stream_encoder_get_qlp_coeff_precision\0"[..], "default QLP precision", 0),
            (&b"FLAC__stream_encoder_get_do_qlp_coeff_prec_search\0"[..], "QLP precision search", 0),
            (&b"FLAC__stream_encoder_get_do_escape_coding\0"[..], "escape coding", 0),
            (&b"FLAC__stream_encoder_get_do_exhaustive_model_search\0"[..], "exhaustive model search", 0),
            (&b"FLAC__stream_encoder_get_min_residual_partition_order\0"[..], "min residual partition order", 0),
            (&b"FLAC__stream_encoder_get_max_residual_partition_order\0"[..], "max residual partition order", 5),
            (&b"FLAC__stream_encoder_get_rice_parameter_search_dist\0"[..], "Rice search distance", 0),
            (&b"FLAC__stream_encoder_get_limit_min_bitrate\0"[..], "limit minimum bitrate", 0),
        ] { self.check_u32(symbol_name, name, expected)?; }
        // SAFETY: this getter returns a uint64_t from the live encoder.
        let getter: GetU64 = unsafe { symbol(&self.library.library, b"FLAC__stream_encoder_get_total_samples_estimate\0")? };
        let actual = unsafe { getter(self.encoder.as_ptr()) };
        if actual != 0 { return Err(FlacError::UnexpectedDefault { name: "total samples estimate", expected: 0, actual }); }
        Ok(())
    }
}

impl Drop for FlacEncoder<'_> {
    fn drop(&mut self) {
        // libFLAC delete abandons an unfinished stream and does not emit the
        // final block or metadata updates. Sink and library still outlive it.
        unsafe { (self.library.functions.delete)(self.encoder.as_ptr()); }
    }
}

#[cfg_attr(target_os = "macos", link(name = "System"))]
unsafe extern "C" { fn fcntl(fd: c_int, command: c_int, ...) -> c_int; }

fn validate_staging_file(file: &mut File) -> Result<(), FlacError> {
    let metadata = file.metadata().map_err(|e| io_error("inspect FLAC staging file", e))?;
    if !metadata.is_file() || metadata.len() != 0 { return Err(FlacError::InvalidStagingFile("must be an empty regular file")); }
    if file.stream_position().map_err(|e| io_error("inspect FLAC staging position", e))? != 0 {
        return Err(FlacError::InvalidStagingFile("position must be zero"));
    }
    // SAFETY: F_GETFL takes no variadic argument and the fd is live.
    let flags = unsafe { fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 { return Err(io_error("inspect FLAC staging flags", io::Error::last_os_error())); }
    if flags & libc::O_ACCMODE == libc::O_RDONLY { return Err(FlacError::InvalidStagingFile("file is not open for writing")); }
    if flags & libc::O_APPEND != 0 { return Err(FlacError::InvalidStagingFile("append mode prevents STREAMINFO rewriting")); }
    Ok(())
}

impl FlacLibrary {
    pub(crate) fn decoder_library(&self) -> &Library { &self.library }
}
