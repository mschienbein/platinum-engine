//! Platinum Notes' LAME recorder and source-dependent Xing/LAME handling.
//!
//! This is an adapter for an explicitly supplied, open-source LAME 3.100 dylib,
//! not a claim that every 3.100 build produces the reference bytes. A complete
//! output also needs the original processing, AudioToolbox conversion, sample
//! scheduling and metadata handling. See `docs/VERIFICATION.md` for scope.

use libloading::Library;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};

const SCAN_BYTES: usize = 65_536;

#[derive(Debug)]
pub enum Mp3Error {
    Io(io::Error),
    Load(String),
    #[cfg(target_os = "linux")]
    LibraryPolicy(crate::linux_libraries::LibraryPolicyError),
    InvalidDigest,
    DigestMismatch { expected: String, actual: String },
    UnsupportedVersion(String),
    InvalidConfiguration(&'static str),
    InvalidBuffer(&'static str),
    Lame { operation: &'static str, code: i32 },
    StreamFailed,
    TagRewriteFailed,
}
impl fmt::Display for Mp3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "MP3 I/O: {e}"),
            Self::Load(e) => write!(f, "LAME library: {e}"),
            #[cfg(target_os = "linux")]
            Self::LibraryPolicy(error) => write!(f, "{error}"),
            Self::InvalidDigest => write!(f, "expected SHA-256 must be 64 hexadecimal digits"),
            Self::DigestMismatch { expected, actual } => write!(f, "LAME SHA-256 mismatch: expected {expected}, got {actual}"),
            Self::UnsupportedVersion(v) => write!(f, "LAME 3.100 required; library reports {v:?}"),
            Self::InvalidConfiguration(s) | Self::InvalidBuffer(s) => f.write_str(s),
            Self::Lame { operation, code } => write!(f, "LAME {operation} failed ({code})"),
            Self::StreamFailed => write!(f, "MP3 stream was invalidated by an earlier error"),
            Self::TagRewriteFailed => write!(f, "LAME tag rewrite did not write its reported frame"),
        }
    }
}
impl std::error::Error for Mp3Error {
    #[cfg(target_os = "linux")]
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self { Self::LibraryPolicy(error) => Some(error), _ => None }
    }
}
impl From<io::Error> for Mp3Error { fn from(e: io::Error) -> Self { Self::Io(e) } }

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[repr(C)]
struct Sha256Context { count: [u32; 2], hash: [u32; 8], wbuf: [u32; 16] }
#[repr(C)]
struct DlInfo { filename: *const c_char, base: *mut c_void, symbol: *const c_char, address: *mut c_void }
unsafe extern "C" {
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    fn CC_SHA256_Init(ctx: *mut Sha256Context) -> c_int;
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    fn CC_SHA256_Update(ctx: *mut Sha256Context, data: *const c_void, len: u32) -> c_int;
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    fn CC_SHA256_Final(md: *mut u8, ctx: *mut Sha256Context) -> c_int;
    fn dladdr(address: *const c_void, info: *mut DlInfo) -> c_int;
    fn fdopen(fd: c_int, mode: *const c_char) -> *mut c_void;
    fn close(fd: c_int) -> c_int;
    fn fwrite(data: *const c_void, size: usize, n: usize, file: *mut c_void) -> usize;
    fn fflush(file: *mut c_void) -> c_int;
    fn ferror(file: *mut c_void) -> c_int;
    fn fclose(file: *mut c_void) -> c_int;
}

/// SHA-256 of the entire supplied file, using macOS CommonCrypto.
#[cfg(all(target_os = "macos", not(feature = "portable")))]
pub fn file_sha256(path: impl AsRef<Path>) -> Result<String, Mp3Error> {
    let mut file = File::open(path)?;
    let mut context = Sha256Context { count: [0; 2], hash: [0; 8], wbuf: [0; 16] };
    // SAFETY: the context layout and function signatures are the public CommonCrypto ABI.
    if unsafe { CC_SHA256_Init(&mut context) } != 1 { return Err(Mp3Error::Load("CC_SHA256_Init failed".into())); }
    let mut block = [0u8; 32768];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 { break; }
        // SAFETY: n is at most the size of block and fits CC_LONG.
        if unsafe { CC_SHA256_Update(&mut context, block.as_ptr().cast(), n as u32) } != 1 {
            return Err(Mp3Error::Load("CC_SHA256_Update failed".into()));
        }
    }
    let mut digest = [0u8; 32];
    // SAFETY: both output and context point to their complete valid objects.
    if unsafe { CC_SHA256_Final(digest.as_mut_ptr(), &mut context) } != 1 {
        return Err(Mp3Error::Load("CC_SHA256_Final failed".into()));
    }
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in digest { let _ = write!(hex, "{byte:02x}"); }
    Ok(hex)
}

#[cfg(any(target_os = "linux", feature = "portable"))]
pub fn file_sha256(path: impl AsRef<Path>) -> Result<String, Mp3Error> {
    let mut file = File::open(path)?;
    let mut digest = crate::portable::digest::Sha256::new();
    let mut block = [0u8; 32768];
    loop {
        let count = file.read(&mut block)?;
        if count == 0 { break; }
        digest.update(&block[..count]);
    }
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in digest.finalize() { let _ = write!(hex, "{byte:02x}"); }
    Ok(hex)
}

type Init = unsafe extern "C" fn() -> *mut c_void;
type Set = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type InitParams = unsafe extern "C" fn(*mut c_void) -> c_int;
type Encode = unsafe extern "C" fn(*mut c_void, *mut i16, c_int, *mut u8, c_int) -> c_int;
type Flush = unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int;
type Tags = unsafe extern "C" fn(*mut c_void, *mut c_void);
type CloseLame = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetInt = unsafe extern "C" fn(*const c_void) -> c_int;
type GetTag = unsafe extern "C" fn(*const c_void, *mut u8, usize) -> usize;
type Version = unsafe extern "C" fn() -> *const c_char;

#[derive(Debug, Clone)]
pub struct LibraryIdentity {
    pub canonical_path: PathBuf,
    pub sha256: String,
    pub version: String,
}

/// Loaded only after an explicit digest pin and version check. The caller must
/// supply a trusted LGPL-compatible open-source build and record its source and
/// build provenance. Path, version and digest are not proof of reference parity.
pub struct LameLibrary {
    _library: std::sync::Arc<Library>,
    _pinned: crate::native_library::PinnedLibrary,
    identity: LibraryIdentity,
    init: Init, set_channels: Set, set_rate: Set, set_mode: Set,
    set_brate: Set, set_quality: Set, set_vbr: Set, set_vbr_mean: Set,
    set_vbr_q: Set, set_write_tag: Set, init_params: InitParams,
    encode: Encode, flush: Flush, tags: Tags, close: CloseLame,
    get_write_tag: GetInt, get_lametag_frame: GetTag,
}
impl LameLibrary {
    /// `expected_sha256` identifies the complete dylib file, not just a Mach-O slice.
    /// Loading a dylib executes its initializers; it must be from a trusted source.
    pub fn load(path: impl AsRef<Path>, expected_sha256: &str) -> Result<Self, Mp3Error> {
        if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(Mp3Error::InvalidDigest);
        }
        let path = path.as_ref().canonicalize()?;
        #[cfg(target_os = "macos")]
        let pinned = crate::native_library::PinnedLibrary::open(&path).map_err(|e| Mp3Error::Load(e.to_string()))?;
        #[cfg(target_os = "linux")]
        let pinned = crate::linux_libraries::open_approved(&path, crate::linux_libraries::LibraryKind::Lame).map_err(Mp3Error::LibraryPolicy)?;
        let digest = pinned.digest_hex();
        if !digest.eq_ignore_ascii_case(expected_sha256) {
            return Err(Mp3Error::DigestMismatch { expected: expected_sha256.to_ascii_lowercase(), actual: digest });
        }
        // SAFETY: the path is explicitly supplied by the caller and pinned above;
        // the caller is responsible for trusting executable library initializers.
        #[cfg(target_os = "macos")]
        let library = unsafe { pinned.load() }.map_err(|e| Mp3Error::Load(e.to_string()))?;
        #[cfg(target_os = "linux")]
        let library = unsafe { pinned.load() }.map_err(|e| Mp3Error::LibraryPolicy(e.into()))?;
        unsafe fn symbol<T: Copy>(lib: &Library, name: &'static [u8]) -> Result<T, Mp3Error> {
            // SAFETY: names and ABI signatures below are declared by LAME's public API.
            unsafe { lib.get::<T>(name) }.map(|s| *s).map_err(|e| Mp3Error::Load(e.to_string()))
        }
        // SAFETY: each symbol is copied with its public C ABI; the library stays owned here.
        unsafe {
            let version_fn: Version = symbol(&library, b"get_lame_version\0")?;
            #[cfg(target_os = "macos")]
            pinned.verify_symbol(version_fn as *const () as *const c_void).map_err(|e| Mp3Error::Load(e.to_string()))?;
            #[cfg(target_os = "linux")]
            pinned.verify_symbol(version_fn as *const () as *const c_void).map_err(|e| Mp3Error::LibraryPolicy(e.into()))?;
            let mut info = DlInfo { filename: ptr::null(), base: ptr::null_mut(), symbol: ptr::null(), address: ptr::null_mut() };
            if dladdr(version_fn as *const () as *const c_void, &mut info) == 0 || info.filename.is_null() {
                return Err(Mp3Error::Load("cannot identify loaded LAME image".into()));
            }
            let loaded = Path::new(std::ffi::OsStr::from_bytes(CStr::from_ptr(info.filename).to_bytes())).canonicalize()?;
            if loaded != path {
                return Err(Mp3Error::Load(format!("LAME symbol resolved to unexpected image {}", loaded.display())));
            }
            let version_ptr = version_fn();
            if version_ptr.is_null() { return Err(Mp3Error::UnsupportedVersion("null".into())); }
            let version = CStr::from_ptr(version_ptr).to_string_lossy().into_owned();
            if version != "3.100" { return Err(Mp3Error::UnsupportedVersion(version)); }
            // A second digest check catches ordinary replacement during loading; callers
            // must still keep the trusted library file immutable during this operation.
            let after = file_sha256(&path)?;
            if after != digest { return Err(Mp3Error::DigestMismatch { expected: digest, actual: after }); }
            Ok(Self {
                init: symbol(&library, b"lame_init\0")?,
                set_channels: symbol(&library, b"lame_set_num_channels\0")?,
                set_rate: symbol(&library, b"lame_set_in_samplerate\0")?,
                set_mode: symbol(&library, b"lame_set_mode\0")?,
                set_brate: symbol(&library, b"lame_set_brate\0")?,
                set_quality: symbol(&library, b"lame_set_quality\0")?,
                set_vbr: symbol(&library, b"lame_set_VBR\0")?,
                set_vbr_mean: symbol(&library, b"lame_set_VBR_mean_bitrate_kbps\0")?,
                set_vbr_q: symbol(&library, b"lame_set_VBR_q\0")?,
                set_write_tag: symbol(&library, b"lame_set_bWriteVbrTag\0")?,
                init_params: symbol(&library, b"lame_init_params\0")?,
                encode: symbol(&library, b"lame_encode_buffer_interleaved\0")?,
                flush: symbol(&library, b"lame_encode_flush\0")?,
                tags: symbol(&library, b"lame_mp3_tags_fid\0")?,
                close: symbol(&library, b"lame_close\0")?,
                get_write_tag: symbol(&library, b"lame_get_bWriteVbrTag\0")?,
                get_lametag_frame: symbol(&library, b"lame_get_lametag_frame\0")?,
                identity: LibraryIdentity { canonical_path: path, sha256: digest, version },
                _library: library, _pinned: pinned,
            })
        }
    }
    pub fn identity(&self) -> &LibraryIdentity { &self.identity }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mp3Mode { Cbr320, Vbr192 }

/// Controller output rate selection. Resampling is performed upstream, not here.
pub fn output_sample_rate(processing_rate: f64) -> u32 {
    if processing_rate == 32_000.0 || processing_rate == 22_050.0 ||
        processing_rate == 44_100.0 || processing_rate == 48_000.0 {
        processing_rate as u32
    } else { 44_100 }
}

/// Codec identity plus the native header scan, which runs even for non-MP3
/// inputs. Non-MP3 always requests a tag, but a detected Xing/Info still
/// participates in the later source-header update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceHeader { Other(Mp3Header), Mp3(Mp3Header) }
impl SourceHeader {
    pub fn write_vbr_tag(&self) -> bool {
        match self { Self::Other(_) => true, Self::Mp3(h) => h.xing || h.info }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mp3Header {
    pub valid_layer3: bool,
    pub xing: bool,
    pub info: bool,
    pub lame_identifier: Option<[u8; 9]>,
    pub encoder_delay: u32,
    pub encoder_padding: u32,
    pub start_skip_samples: u32,
    pub stereo_mode_change: bool,
    pub stereo_mode: u32,
    pub correct_crc: bool,
}

#[derive(Debug, Clone)]
pub struct Mp3Config {
    /// Actual final-render rate; controller-selected MP3 rates are 22050, 32000, 44100, 48000.
    pub sample_rate: u32,
    /// Number of source channels, converted to stereo PCM before LAME.
    pub channels: u8,
    pub mode: Mp3Mode,
    pub source: SourceHeader,
    /// Oracle/debug context only; never surface as a normal CLI setting.
    /// The original updater reads this byte before initializing it in some
    /// branches. `None` applies the nonzero branch: static analysis shows the
    /// reused caller slot holds the limiter AudioUnit handle, whose byte 3 is
    /// 0x80..0xff on the in-process AudioToolbox creation path, and that
    /// branch matches the retained native exports.
    #[doc(hidden)]
    pub native_update_result_byte_3: Option<u8>,
}

struct LameState<'a> { api: &'a LameLibrary, flags: NonNull<c_void> }
impl Drop for LameState<'_> {
    fn drop(&mut self) {
        // SAFETY: flags came from lame_init and is closed exactly once.
        unsafe { (self.api.close)(self.flags.as_ptr()); }
    }
}
struct CFile(NonNull<c_void>);
impl CFile {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Mp3Error> {
        if bytes.is_empty() { return Ok(()); }
        // SAFETY: the C stream remains open; bytes is valid for its length.
        let n = unsafe { fwrite(bytes.as_ptr().cast(), 1, bytes.len(), self.0.as_ptr()) };
        if n != bytes.len() { return Err(Mp3Error::Io(io::Error::last_os_error())); }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Mp3Error> {
        // SAFETY: stream remains open and no other handle performs I/O concurrently.
        if unsafe { fflush(self.0.as_ptr()) } != 0 { return Err(Mp3Error::Io(io::Error::last_os_error())); }
        Ok(())
    }
    fn finish(self) -> Result<(), Mp3Error> {
        let ptr = self.0.as_ptr();
        std::mem::forget(self);
        // SAFETY: ownership has been transferred out of self, so this closes once.
        if unsafe { fclose(ptr) } != 0 { return Err(Mp3Error::Io(io::Error::last_os_error())); }
        Ok(())
    }
}
impl Drop for CFile {
    fn drop(&mut self) { unsafe { fclose(self.0.as_ptr()); } }
}

/// A new, exclusively created staging file. Dropping an unfinished encoder
/// closes it but leaves the partial file for diagnosis; it never publishes it.
pub struct Mp3Encoder<'a> {
    state: Option<LameState<'a>>,
    cfile: Option<CFile>,
    file: File,
    config: Mp3Config,
    effective_tag: bool,
    buffer: Vec<u8>,
    failed: bool,
    bytes_written: u64,
}
#[derive(Debug, Clone, Copy)]
pub struct EncodeReport {
    pub requested_vbr_tag: bool,
    pub configured_vbr_tag: bool,
    /// Bytes in the final frame reported by this build, or zero if it emitted none.
    pub lametag_frame_bytes: usize,
    /// Encoded bytes before any later tag/metadata additions.
    pub encoded_bytes: u64,
    pub source_header_patch_applied: bool,
}
impl<'a> Mp3Encoder<'a> {
    pub fn create_new(library: &'a LameLibrary, staging: impl AsRef<Path>, config: Mp3Config) -> Result<Self, Mp3Error> {
        Self::create_with(library, config, || OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(staging))
    }
    pub(crate) fn from_file(library: &'a LameLibrary, file: File, config: Mp3Config) -> Result<Self, Mp3Error> {
        Self::create_with(library, config, || Ok(file))
    }
    fn create_with(library: &'a LameLibrary, config: Mp3Config, open: impl FnOnce() -> io::Result<File>) -> Result<Self, Mp3Error> {
        if !(1..=64).contains(&config.channels) { return Err(Mp3Error::InvalidConfiguration("MP3 source channel count must be in 1..=64")); }
        if !matches!(config.sample_rate, 22_050 | 32_000 | 44_100 | 48_000) {
            return Err(Mp3Error::InvalidConfiguration("MP3 sample rate must be the controller-selected final rate"));
        }
        // SAFETY: public LAME initializer returns either null or an owned context.
        let flags = NonNull::new(unsafe { (library.init)() }).ok_or(Mp3Error::Lame { operation: "init", code: -1 })?;
        let state = LameState { api: library, flags };
        let set = |op: &'static str, setter: Set, value: i32| -> Result<(), Mp3Error> {
            // SAFETY: state owns an unclosed LAME context and setters have the declared ABI.
            let code = unsafe { setter(state.flags.as_ptr(), value) };
            if code < 0 { Err(Mp3Error::Lame { operation: op, code }) } else { Ok(()) }
        };
        set("set_num_channels", library.set_channels, i32::from(config.channels.min(2)))?;
        set("set_in_samplerate", library.set_rate, config.sample_rate as i32)?;
        set("set_mode", library.set_mode, if config.channels == 1 { 3 } else { 0 })?;
        match config.mode {
            Mp3Mode::Cbr320 => { set("set_brate", library.set_brate, 320)?; set("set_quality", library.set_quality, 4)?; }
            Mp3Mode::Vbr192 => {
                set("set_VBR", library.set_vbr, 3)?;
                set("set_VBR_mean_bitrate_kbps", library.set_vbr_mean, 192)?;
                set("set_VBR_q", library.set_vbr_q, 5)?;
            }
        }
        set("set_bWriteVbrTag", library.set_write_tag, i32::from(config.source.write_vbr_tag()))?;
        // SAFETY: context is valid, all setters precede init_params as in the recorder.
        let result = unsafe { (library.init_params)(state.flags.as_ptr()) };
        if result < 0 { return Err(Mp3Error::Lame { operation: "init_params", code: result }); }
        let effective_tag = unsafe { (library.get_write_tag)(state.flags.as_ptr()) } != 0;
        let file = open()?;
        let fd = file.try_clone()?.into_raw_fd();
        // SAFETY: fd is newly duplicated and transferred to fdopen on success.
        let c_ptr = unsafe { fdopen(fd, c"r+".as_ptr()) };
        let cfile = match NonNull::new(c_ptr) {
            Some(p) => CFile(p),
            None => {
                let error = io::Error::last_os_error();
                unsafe { close(fd); }
                return Err(Mp3Error::Io(error));
            }
        };
        Ok(Self { state: Some(state), cfile: Some(cfile), file, config, effective_tag, buffer: Vec::new(), failed: false, bytes_written: 0 })
    }

    /// Consumes one AudioConverter output block in-place. The slice is always
    /// stereo interleaved signed 16-bit, including for `channels == 1`; pass
    /// exactly the original graph frame count, not twice that count, to LAME.
    /// The C API accepts a mutable pointer, so the buffer must not be shared.
    pub fn write_converted_stereo(&mut self, samples: &mut [i16]) -> Result<(), Mp3Error> {
        if self.failed { return Err(Mp3Error::StreamFailed); }
        if samples.len() % 2 != 0 { return Err(Mp3Error::InvalidBuffer("stereo PCM must contain complete frames")); }
        let frames = samples.len() / 2;
        if frames == 0 { return Ok(()); }
        let capacity = frames.checked_mul(5).and_then(|n| n.checked_div(2)).and_then(|n| n.checked_add(7200))
            .filter(|&n| n <= i32::MAX as usize)
            .ok_or(Mp3Error::InvalidBuffer("PCM block exceeds native buffer limits"))?;
        if frames > i32::MAX as usize { return Err(Mp3Error::InvalidBuffer("PCM frame count exceeds LAME limit")); }
        self.buffer.resize(capacity, 0);
        let state = self.state.as_ref().ok_or(Mp3Error::StreamFailed)?;
        // SAFETY: the context and mutable PCM/output blocks are valid; each
        // block contains 2*frames samples even when LAME's input mode is mono.
        let n = unsafe { (state.api.encode)(state.flags.as_ptr(), samples.as_mut_ptr(), frames as i32, self.buffer.as_mut_ptr(), capacity as i32) };
        if n < 0 || n as usize > capacity {
            self.failed = true;
            return Err(Mp3Error::Lame { operation: "encode_buffer_interleaved", code: n });
        }
        if let Err(e) = self.cfile.as_mut().ok_or(Mp3Error::StreamFailed)?.write_all(&self.buffer[..n as usize]) {
            self.failed = true; return Err(e);
        }
        self.bytes_written += n as u64;
        Ok(())
    }

    /// Flush, run LAME's actual FILE* tag writer, close it, then perform the
    /// source-dependent MIK header update. Later metadata copying is separate.
    pub fn finish(mut self) -> Result<EncodeReport, Mp3Error> {
        if self.failed { return Err(Mp3Error::StreamFailed); }
        let state = self.state.as_ref().ok_or(Mp3Error::StreamFailed)?;
        let mut out = [0u8; 10_000];
        // SAFETY: LAME context is active and out has the native flush capacity.
        let n = unsafe { (state.api.flush)(state.flags.as_ptr(), out.as_mut_ptr(), out.len() as i32) };
        if n < 0 || n as usize > out.len() { return Err(Mp3Error::Lame { operation: "encode_flush", code: n }); }
        let cfile = self.cfile.as_mut().ok_or(Mp3Error::StreamFailed)?;
        cfile.write_all(&out[..n as usize])?;
        self.bytes_written += n as u64;
        cfile.flush()?;
        // Obtain the expected final tag before closing the context. This is a
        // diagnostic check on the otherwise void `lame_mp3_tags_fid` API.
        let tag_size = unsafe { (state.api.get_lametag_frame)(state.flags.as_ptr(), ptr::null_mut(), 0) };
        let mut tag = vec![0; tag_size];
        if tag_size != 0 {
            let actual = unsafe { (state.api.get_lametag_frame)(state.flags.as_ptr(), tag.as_mut_ptr(), tag.len()) };
            if actual != tag_size { return Err(Mp3Error::TagRewriteFailed); }
        }
        unsafe { (state.api.tags)(state.flags.as_ptr(), cfile.0.as_ptr()); }
        if unsafe { ferror(cfile.0.as_ptr()) } != 0 { return Err(Mp3Error::Io(io::Error::last_os_error())); }
        cfile.flush()?;
        // Preserve original ordering: LAME close then fclose, then MIK update.
        self.state.take();
        self.cfile.take().ok_or(Mp3Error::StreamFailed)?.finish()?;
        if !tag.is_empty() {
            let mut actual = vec![0u8; tag.len()];
            self.file.seek(SeekFrom::Start(0))?;
            self.file.read_exact(&mut actual)?;
            if actual != tag { return Err(Mp3Error::TagRewriteFailed); }
        }
        let patch_applied = match &self.config.source {
            SourceHeader::Mp3(h) | SourceHeader::Other(h) if (h.xing || h.info) && self.config.source.write_vbr_tag() => {
                update_source_header(&mut self.file, h, self.config.native_update_result_byte_3)?
            }
            _ => false,
        };
        self.file.flush()?;
        Ok(EncodeReport { requested_vbr_tag: self.config.source.write_vbr_tag(), configured_vbr_tag: self.effective_tag,
            lametag_frame_bytes: tag_size, encoded_bytes: self.bytes_written, source_header_patch_applied: patch_applied })
    }
}

// FFmpeg MPEG tables embedded in MIKAudio, with 15 entries per layer.
const BITRATES: [[[u16; 15]; 3]; 2] = [
    [
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    ],
    [
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ],
];

pub(crate) fn frame_size(word: u32) -> usize {
    if word <= 0xffdf_ffff || word & 0xf000 == 0xf000 || word & 0x60000 == 0 ||
        word & 0xc00 == 0xc00 {
        return 0;
    }
    let bitrate_index = ((word >> 12) & 15) as usize;
    if bitrate_index == 0 { return 0; }
    let version_bit = (word >> 20) & 1;
    let low_version = if version_bit == 0 { 1 } else { 1 ^ ((word >> 19) & 1) };
    let layer = (word >> 17) & 3;
    let rate_index = ((word >> 10) & 3) as usize;
    let rate = [44_100u32, 48_000, 32_000][rate_index] >> (low_version + (version_bit ^ 1));
    let bitrate = u32::from(BITRATES[low_version as usize][(layer ^ 3) as usize][bitrate_index]);
    let padding = (word >> 9) & 1;
    if rate == 0 { return 0; }
    match layer {
        3 => ((bitrate * 12_000 / rate + padding) * 4) as usize,
        2 => (bitrate * 144_000 / rate + padding) as usize,
        _ => (bitrate * 144_000 / (rate << low_version) + padding) as usize,
    }
}
/// Metadata required by the output report, obtained without decoding audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncodedMp3Info {
    pub sample_rate: u32,
    pub channels: u32,
    pub frames_per_packet: u32,
    pub frames: u64,
}

#[derive(Clone, Copy)]
struct OutputFrame {
    size: u64,
    rate: u32,
    channels: u32,
    samples: u32,
    bitrate: u32,
    crc_bytes: usize,
    side_bytes: usize,
}

fn output_frame(word: u32) -> Option<OutputFrame> {
    let version = (word >> 19) & 3;
    let layer = (word >> 17) & 3;
    let bitrate_index = ((word >> 12) & 15) as usize;
    let rate_index = ((word >> 10) & 3) as usize;
    if word >> 21 != 0x7ff || version == 1 || layer != 1 ||
        bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    let low_version = usize::from(version != 3);
    let rate = [44_100, 48_000, 32_000][rate_index] >> match version { 3 => 0, 2 => 1, _ => 2 };
    let channels = if (word >> 6) & 3 == 3 { 1 } else { 2 };
    let samples = if version == 3 { 1152 } else { 576 };
    let bitrate = u32::from(BITRATES[low_version][2][bitrate_index]) * 1000;
    let size = u64::from(frame_size(word) as u32);
    let side_bytes = match (version == 3, channels == 1) {
        (true, true) => 17, (true, false) => 32,
        (false, true) => 9, (false, false) => 17,
    };
    Some(OutputFrame { size, rate, channels, samples, bitrate,
        crc_bytes: if word & (1 << 16) == 0 { 2 } else { 0 }, side_bytes })
}

enum OutputTag { Info, Xing }

/// How AudioToolbox classifies the encoder identifier of a Xing/Info tag. The
/// classification, not the text, decides how the tag is trusted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TagEncoder {
    /// `LAME…`: the delay and padding are trusted without checking the tag CRC.
    Lame,
    /// `Lavf…`/`Lavc…`: the delay and padding are trusted only when the tag
    /// CRC16 verifies, and an `Info` tag still reports its declared frame count.
    Other,
    /// The native source-header update erased the identifier. The delay and
    /// padding are trusted only when the tag CRC16 verifies, but an `Info` tag
    /// is still extrapolated from its declared byte count like a LAME tag.
    Erased,
}

struct OutputGapless {
    kind: OutputTag,
    encoder: TagEncoder,
    declared_frames: u32,
    declared_bytes: Option<u32>,
    delay: u32,
    padding: u32,
}

fn output_gapless(frame: &[u8], header: OutputFrame) -> Result<Option<OutputGapless>, Mp3Error> {
    let offset = 4 + header.crc_bytes + header.side_bytes;
    let Some(marker) = frame.get(offset..offset + 4) else { return Ok(None); };
    let kind = match marker { b"Info" => OutputTag::Info, b"Xing" => OutputTag::Xing, _ => return Ok(None) };
    let invalid = || Mp3Error::InvalidBuffer("unsupported or truncated MP3 gapless header");
    let read_u32 = |at: usize| -> Result<u32, Mp3Error> {
        Ok(u32::from_be_bytes(frame.get(at..at + 4).ok_or_else(invalid)?.try_into().map_err(|_| invalid())?))
    };
    let flags = read_u32(offset + 4)?;
    if flags & 1 == 0 || flags & !15 != 0 { return Err(invalid()); }
    let declared_frames = read_u32(offset + 8)?;
    let declared_bytes = if flags & 2 != 0 { Some(read_u32(offset + 12)?) } else { None };
    let lame = offset + 8 + 4 + if flags & 2 != 0 { 4 } else { 0 }
        + if flags & 4 != 0 { 100 } else { 0 } + if flags & 8 != 0 { 4 } else { 0 };
    let encoder = match frame.get(lame..lame + 4).ok_or_else(invalid)? {
        b"LAME" => TagEncoder::Lame,
        b"Lavc" | b"Lavf" => TagEncoder::Other,
        [0, 0, 0, 0] => TagEncoder::Erased,
        _ => return Err(invalid()),
    };
    let delay_padding = frame.get(lame + 21..lame + 24).ok_or_else(invalid)?;
    let checksum = frame.get(lame + 34..lame + 36).ok_or_else(invalid)?;
    // The checksum is CRC-16/ARC from the frame header through the tag's music
    // length field, stored big-endian. A LAME tag is trusted regardless.
    let trusted = encoder == TagEncoder::Lame
        || crc16(&frame[..lame + 34]) == u16::from_be_bytes([checksum[0], checksum[1]]);
    // An `Info` tag of a LAME or erased identifier has its gapless trim applied
    // only together with the byte-count extrapolation; with no byte count the
    // declared frame count stands untrimmed.
    let extrapolated = matches!(kind, OutputTag::Info) && encoder != TagEncoder::Other;
    let packed = if trusted && (declared_bytes.is_some() || !extrapolated) {
        (u32::from(delay_padding[0]) << 16) | (u32::from(delay_padding[1]) << 8) | u32::from(delay_padding[2])
    } else { 0 };
    Ok(Some(OutputGapless { kind, encoder, declared_frames, declared_bytes,
        delay: packed >> 12, padding: packed & 0xfff }))
}

/// Validate a staged LAME output and reproduce the native report's MP3
/// properties without making MP3 input decoding available.
pub(crate) fn inspect_encoded_output(path: &Path) -> Result<EncodedMp3Info, Mp3Error> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    inspect_encoded_reader(file, file_len)
}

fn inspect_encoded_reader<R: Read + Seek>(file: R, file_len: u64) -> Result<EncodedMp3Info, Mp3Error> {
    let mut input = BufReader::new(file);
    let mut prefix = [0u8; 10];
    input.read_exact(&mut prefix)?;
    let start = if &prefix[..3] == b"ID3" {
        if prefix[6..10].iter().any(|byte| byte & 0x80 != 0) {
            return Err(Mp3Error::InvalidBuffer("invalid MP3 ID3 size"));
        }
        let size = (u64::from(prefix[6]) << 21) | (u64::from(prefix[7]) << 14)
            | (u64::from(prefix[8]) << 7) | u64::from(prefix[9]);
        10 + size + if prefix[5] & 0x10 != 0 { 10 } else { 0 }
    } else { 0 };
    if start >= file_len { return Err(Mp3Error::InvalidBuffer("MP3 output has no audio frames")); }
    input.seek(SeekFrom::Start(start))?;
    let mut offset = start;
    let mut frame_count = 0u64;
    // Bitrate of the first frame after the tag frame: the one AudioToolbox
    // extrapolates the stream length from for an `Info` tag.
    let mut first_audio_bitrate = None;
    let mut first = None;
    let mut gapless = None;
    while offset < file_len {
        if file_len - offset < 4 { return Err(Mp3Error::InvalidBuffer("truncated MP3 frame header")); }
        let mut bytes = [0u8; 4];
        input.read_exact(&mut bytes)?;
        if &bytes[..3] == b"TAG" && file_len - offset == 128 { break; }
        let header = output_frame(u32::from_be_bytes(bytes))
            .ok_or(Mp3Error::InvalidBuffer("invalid MP3 frame header"))?;
        if header.size < 4 || header.size > file_len - offset {
            return Err(Mp3Error::InvalidBuffer("truncated MP3 frame"));
        }
        if let Some(initial) = first {
            let initial: OutputFrame = initial;
            if header.rate != initial.rate || header.channels != initial.channels || header.samples != initial.samples {
                return Err(Mp3Error::InvalidBuffer("MP3 output changes stream parameters"));
            }
            first_audio_bitrate.get_or_insert(header.bitrate);
            input.seek_relative((header.size - 4) as i64)?;
        } else {
            let size = usize::try_from(header.size).map_err(|_| Mp3Error::InvalidBuffer("MP3 frame too large"))?;
            let mut frame = [0u8; 2048];
            if size > frame.len() { return Err(Mp3Error::InvalidBuffer("MP3 frame too large")); }
            frame[..4].copy_from_slice(&bytes);
            input.read_exact(&mut frame[4..size])?;
            gapless = output_gapless(&frame[..size], header)?;
            first = Some(header);
        }
        frame_count = frame_count.checked_add(1).ok_or(Mp3Error::InvalidBuffer("MP3 frame count overflow"))?;
        offset += header.size;
    }
    let initial = first.ok_or(Mp3Error::InvalidBuffer("MP3 output has no audio frames"))?;
    let frames = if let Some(gapless) = gapless {
        if u64::from(gapless.declared_frames) != frame_count - 1 {
            return Err(Mp3Error::InvalidBuffer("MP3 gapless frame count does not match the file"));
        }
        let packets = match (&gapless.kind, gapless.encoder, gapless.declared_bytes) {
            // AudioToolbox reads an `Info` tag as constant bitrate: it never
            // counts the stream, it extrapolates the duration from the tag's
            // declared byte count (less the tag frame) and the bitrate of the
            // first frame after the tag frame. A variable-bitrate stream that
            // kept `Info` therefore reports a length that follows only that one
            // frame's bitrate. The binary64 operation order matters: an
            // integral packet count can round slightly upward before the ceil.
            (OutputTag::Info, TagEncoder::Lame | TagEncoder::Erased, Some(bytes)) => {
                let audio_bytes = i128::from(bytes) - i128::from(initial.size);
                let bitrate = first_audio_bitrate.unwrap_or(initial.bitrate);
                let duration = (audio_bytes as f64 * 8.0) / f64::from(bitrate);
                let packets_per_second = f64::from(initial.rate) / f64::from(initial.samples);
                (duration * packets_per_second).ceil() as i128
            }
            _ => i128::from(gapless.declared_frames),
        };
        let samples = packets * i128::from(initial.samples);
        let trimmed = samples - i128::from(gapless.delay) - i128::from(gapless.padding);
        if trimmed < 0 { return Err(Mp3Error::InvalidBuffer("MP3 length is negative after gapless trimming")); }
        u64::try_from(trimmed).map_err(|_| Mp3Error::InvalidBuffer("MP3 frame count overflow"))?
    } else {
        frame_count.checked_mul(u64::from(initial.samples))
            .ok_or(Mp3Error::InvalidBuffer("MP3 frame count overflow"))?
    };
    Ok(EncodedMp3Info { sample_rate: initial.rate, channels: initial.channels,
        frames_per_packet: initial.samples, frames })
}

fn word_at(data: &[u8], pos: usize) -> u32 {
    data.get(pos..pos.saturating_add(4)).and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes).unwrap_or(0)
}

/// Native scanner requires three consecutive valid MPEG frames within its
/// zero-filled 65536-byte window; it does not merely find a sync bit pattern.
fn find_frame(data: &[u8]) -> Option<usize> {
    if data.len() <= 4 { return None; }
    for start in 0..data.len() - 4 {
        let mut at = start;
        let mut accepted = 0;
        while accepted < 3 {
            let size = frame_size(word_at(data, at));
            if size == 0 { break; }
            at = match at.checked_add(size) { Some(v) if v < data.len() => v, _ => break };
            accepted += 1;
        }
        if accepted == 3 { return Some(start); }
    }
    None
}

struct ScanWindow { base: u64, data: Vec<u8> }
fn read_scan<R: Read + Seek>(reader: &mut R) -> Result<ScanWindow, Mp3Error> {
    reader.seek(SeekFrom::Start(0))?;
    let mut first = [0u8; 10];
    let mut count = 0;
    while count < first.len() {
        let n = reader.read(&mut first[count..])?;
        if n == 0 { break; }
        count += n;
    }
    // The native code shifts the raw size bytes without masking their top bits.
    let base = if &first[..3] == b"ID3" {
        u64::from((u32::from(first[6]) << 21) | (u32::from(first[7]) << 14) |
            (u32::from(first[8]) << 7) | u32::from(first[9])) + 10
    } else { 0 };
    let mut data = vec![0u8; SCAN_BYTES];
    data[..10].copy_from_slice(&first);
    reader.seek(SeekFrom::Start(base))?;
    let mut read = 0;
    while read < data.len() {
        let n = reader.read(&mut data[read..])?;
        if n == 0 { break; }
        read += n;
    }
    Ok(ScanWindow { base, data })
}

#[derive(Clone, Copy)]
struct LocatedHeader {
    frame: usize,
    word: u32,
    frame_size: usize,
    xing: usize,
    second_mode: u32,
    second_size: usize,
}
fn locate_header(data: &[u8]) -> Option<LocatedHeader> {
    let frame = find_frame(data)?;
    let word = word_at(data, frame);
    let first_size = frame_size(word);
    if first_size == 0 || word & 0x60000 != 0x20000 || frame + 200 > data.len() { return None; }
    let low_version = if word & 0x100000 == 0 { 1 } else { 1 ^ ((word >> 19) & 1) };
    let first_mode = (word >> 6) & 3;
    let side_info = match (low_version, first_mode == 3) {
        (0, false) => 32, (0, true) => 17, (_, false) => 17, (_, true) => 9,
    };
    let xing = frame + 4 + side_info;
    let marker = data.get(xing..xing + 4)?;
    if marker != b"Xing" && marker != b"Info" { return None; }
    let second = word_at(data, frame + first_size);
    Some(LocatedHeader { frame, word, frame_size: first_size, xing,
        second_mode: (second >> 6) & 3, second_size: frame_size(second) })
}
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 { crc = if crc & 1 != 0 { (crc >> 1) ^ 0xa001 } else { crc >> 1 }; }
    }
    crc
}
fn recognized_lame(data: &[u8], pos: usize) -> bool {
    data.get(pos..pos + 4).is_some_and(|v| v == b"LAME" || v == b"Lavf" || v == b"Lavc")
}

/// Run MIKAudio's MP3/Xing/LAME source-header inspection against a regular
/// file. The routine reads no more than the ID3 header and a 65536-byte scan.
pub fn inspect_mp3_header(path: impl AsRef<Path>) -> Result<Mp3Header, Mp3Error> {
    let mut file = File::open(path)?;
    inspect_mp3_reader(&mut file)
}
/// The byte-slice form is useful for independently generated header fixtures.
pub fn inspect_mp3_header_bytes(bytes: &[u8]) -> Result<Mp3Header, Mp3Error> {
    inspect_mp3_reader(&mut io::Cursor::new(bytes))
}
fn inspect_mp3_reader<R: Read + Seek>(reader: &mut R) -> Result<Mp3Header, Mp3Error> {
    let window = read_scan(reader)?;
    let data = &window.data;
    let mut result = Mp3Header::default();
    if let Some(frame) = find_frame(data) {
        let word = word_at(data, frame);
        if frame_size(word) != 0 && word & 0x60000 == 0x20000 {
            result.valid_layer3 = true;
            result.stereo_mode = (word >> 6) & 3;
        }
    }
    let Some(loc) = locate_header(data) else { return Ok(result); };
    result.xing = &data[loc.xing..loc.xing + 4] == b"Xing";
    result.info = !result.xing;
    let first_mode = (loc.word >> 6) & 3;
    result.stereo_mode_change = first_mode != loc.second_mode ||
        (loc.frame_size <= 156 && loc.second_size >= 157);
    result.stereo_mode = loc.second_mode;
    let tag = loc.xing + 120;
    if recognized_lame(data, tag) {
        let mut identifier = [0u8; 9];
        identifier.copy_from_slice(&data[tag..tag + 9]);
        result.lame_identifier = Some(identifier);
        let packed = (u32::from(data[tag + 21]) << 16) | (u32::from(data[tag + 22]) << 8) | u32::from(data[tag + 23]);
        result.encoder_delay = packed >> 12;
        result.encoder_padding = packed & 4095;
        result.start_skip_samples = result.encoder_delay + 529;
        let expected_crc = u16::from_be_bytes([data[tag + 34], data[tag + 35]]);
        result.correct_crc = expected_crc == crc16(&data[loc.frame..tag + 34]);
    } else {
        result.encoder_delay = 576;
        result.encoder_padding = 0;
        result.start_skip_samples = 1105;
    }
    Ok(result)
}

fn prepare_source_header_update(
    data: &mut [u8],
    loc: LocatedHeader,
    source: &Mp3Header,
    scratch_byte: Option<u8>,
) -> Result<usize, Mp3Error> {
    let first_mode = (loc.word >> 6) & 3;
    let tag = loc.xing + 120;
    let recognized = recognized_lame(data, tag);
    let forced_tag = recognized && source.lame_identifier.is_some();
    let mut end = loc.frame + 4;
    data[loc.xing..loc.xing + 4].copy_from_slice(if source.xing { b"Xing" } else { b"Info" });
    if recognized && source.lame_identifier.is_none() {
        let available = loc.frame_size.saturating_sub(tag - loc.frame).min(36);
        if available != 0 {
            data[tag..tag + available].fill(0);
            end = end.max(tag + available);
        }
    }
    let mode = if source.stereo_mode_change {
        if first_mode == loc.second_mode { loc.second_mode ^ 1 } else { first_mode }
    } else { loc.second_mode };
    let new_word = (loc.word & !(3 << 6)) | (mode << 6);
    data[loc.frame..loc.frame + 4].copy_from_slice(&new_word.to_be_bytes());
    // The native updater reads result byte 3 uninitialized on this path; an
    // unspecified caller byte selects the statically established nonzero branch.
    let has_tag = forced_tag || scratch_byte.map_or(true, |byte| byte != 0);
    if has_tag {
        if let Some(id) = source.lame_identifier {
            if id[0] != 0 {
                data[tag..tag + 9].fill(0);
                let count = id.iter().position(|&b| b == 0).unwrap_or(9);
                data[tag..tag + count].copy_from_slice(&id[..count]);
                end = end.max(tag + 9);
            }
        }
        // The header word above is also part of the CRC computed below.
        let old = (u32::from(data[tag + 21]) << 16) | (u32::from(data[tag + 22]) << 8) | u32::from(data[tag + 23]);
        let delay = source.encoder_delay.min(4095);
        let packed = (delay << 12) | (old & 4095);
        data[tag + 21] = (packed >> 16) as u8;
        data[tag + 22] = (packed >> 8) as u8;
        data[tag + 23] = packed as u8;
        let value = crc16(&data[loc.frame..tag + 34]);
        let written_crc = if source.correct_crc { value } else { u16::from(value == 0) };
        data[tag + 34..tag + 36].copy_from_slice(&written_crc.to_be_bytes());
        end = end.max(tag + 36);
    }
    Ok(end)
}

fn update_source_header<R: Read + Write + Seek>(file: &mut R, source: &Mp3Header, scratch_byte: Option<u8>) -> Result<bool, Mp3Error> {
    let mut window = read_scan(file)?;
    let Some(loc) = locate_header(&window.data) else { return Ok(false); };
    let end = prepare_source_header_update(&mut window.data, loc, source, scratch_byte)?;
    if end > loc.frame {
        file.seek(SeekFrom::Start(window.base + loc.frame as u64))?;
        file.write_all(&window.data[loc.frame..end])?;
    }
    Ok(true)
}

#[cfg(test)]
mod source_update_tests {
    use super::*;

    fn source_without_lame(xing: bool) -> Mp3Header {
        Mp3Header {
            valid_layer3: true,
            xing,
            info: !xing,
            encoder_delay: 576,
            start_skip_samples: 1105,
            stereo_mode_change: true,
            stereo_mode: 1,
            ..Mp3Header::default()
        }
    }

    // The short MPEG-1 frame ends before the fixed-offset LAME fields, which
    // are therefore in the second frame. The native erase then clears zero
    // bytes. These synthetic files contain no audio from an application.
    fn output_fixture(short: bool, id3: bool, xing: bool, recognized: bool) -> Vec<u8> {
        let prefix = if id3 { 15 } else { 0 };
        let size = if short { 104 } else { 417 };
        let word = if short { 0xfffb_1000u32 } else { 0xfffb_9000u32 };
        let mut bytes = vec![0; prefix + size * 4];
        if id3 { bytes[..15].copy_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x05abcde"); }
        for frame in 0..4 {
            let mode = u32::from(frame != 0);
            let pos = prefix + frame * size;
            bytes[pos..pos + 4].copy_from_slice(&(word | mode << 6).to_be_bytes());
        }
        let marker = prefix + 36;
        bytes[marker..marker + 4].copy_from_slice(if xing { b"Xing" } else { b"Info" });
        let tag = marker + 120;
        if recognized { bytes[tag..tag + 9].copy_from_slice(b"LAME3.100"); }
        let packed = (576u32 << 12) | 1000;
        bytes[tag + 21] = (packed >> 16) as u8;
        bytes[tag + 22] = (packed >> 8) as u8;
        bytes[tag + 23] = packed as u8;
        let crc = u16::from(crc16(&bytes[prefix..tag + 34]) == 0);
        bytes[tag + 34..tag + 36].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    fn updated(input: &[u8], source: &Mp3Header, scratch: Option<u8>) -> Result<Vec<u8>, Mp3Error> {
        let mut file = io::Cursor::new(input.to_vec());
        update_source_header(&mut file, source, scratch)?;
        Ok(file.into_inner())
    }

    #[test]
    fn unknown_scratch_accepts_identical_complete_file_effects() {
        for id3 in [false, true] {
            for xing in [false, true] {
                for recognized in [false, true] {
                    let input = output_fixture(true, id3, xing, recognized);
                    let source = source_without_lame(xing);
                    let zero = updated(&input, &source, Some(0)).unwrap();
                    // The pinned native updater left these same fixtures
                    // byte-identical for both controlled scratch branches.
                    assert_eq!(zero, input);
                    for byte in [1, 2, 128, 255] {
                        assert_eq!(updated(&input, &source, Some(byte)).unwrap(), zero);
                    }
                    assert_eq!(updated(&input, &source, None).unwrap(), zero);
                }
            }
        }
    }

    #[test]
    fn unknown_scratch_uses_the_nonzero_branch_when_effects_differ() {
        let source = source_without_lame(true);
        let mut bad_crc = output_fixture(true, false, true, true);
        bad_crc[36 + 120 + 35] ^= 1;
        let erased_tag = output_fixture(false, false, true, true);
        let changed_marker = output_fixture(true, false, false, true);
        for input in [&bad_crc, &erased_tag, &changed_marker] {
            let zero = updated(input, &source, Some(0)).unwrap();
            let one = updated(input, &source, Some(1)).unwrap();
            assert_ne!(zero, one);
            assert_eq!(updated(input, &source, None).unwrap(), one);
        }
    }

    #[test]
    fn recognized_source_and_output_force_the_tag_branch() {
        let mut source = source_without_lame(true);
        source.lame_identifier = Some(*b"LAME3.100");
        source.correct_crc = true;
        let input = output_fixture(false, true, true, true);
        let zero = updated(&input, &source, Some(0)).unwrap();
        assert_eq!(updated(&input, &source, Some(255)).unwrap(), zero);
        assert_eq!(updated(&input, &source, None).unwrap(), zero);

        let unrecognized = output_fixture(false, false, true, false);
        assert_ne!(updated(&unrecognized, &source, Some(0)).unwrap(),
            updated(&unrecognized, &source, Some(1)).unwrap());
        assert_eq!(updated(&unrecognized, &source, None).unwrap(),
            updated(&unrecognized, &source, Some(1)).unwrap());
    }

    #[test]
    fn output_without_xing_or_info_needs_no_scratch_state() {
        let source = source_without_lame(true);
        for id3 in [false, true] {
            let mut input = output_fixture(true, id3, true, true);
            let marker = if id3 { 51 } else { 36 };
            input[marker..marker + 4].copy_from_slice(b"NONE");
            let mut file = io::Cursor::new(input.clone());
            assert!(!update_source_header(&mut file, &source, None).unwrap());
            assert_eq!(file.into_inner(), input);
        }
    }

    #[test]
    fn unknown_scratch_nonzero_branch_extends_a_short_file() {
        let mut input = vec![0u8; 163];
        for (frame, pos) in [0usize, 26, 52].into_iter().enumerate() {
            let word = 0xfff3_1000u32 | (u32::from(frame != 0) << 6);
            input[pos..pos + 4].copy_from_slice(&word.to_be_bytes());
        }
        input[21..25].copy_from_slice(b"Xing");
        input[162] = 0x24; // 576 << 12; the remaining delay/padding bytes are EOF zeros.
        let window = read_scan(&mut io::Cursor::new(&input)).unwrap();
        if crc16(&window.data[..175]) == 0 { input[120] ^= 1; }
        let source = source_without_lame(true);
        let zero = updated(&input, &source, Some(0)).unwrap();
        let one = updated(&input, &source, Some(1)).unwrap();
        assert_eq!(zero.len(), 163);
        assert_eq!(one.len(), 177);
        assert_eq!(&one[..zero.len()], zero.as_slice());
        assert!(one[zero.len()..].iter().all(|&byte| byte == 0));
        assert_eq!(updated(&input, &source, None).unwrap(), one);
    }
}

#[cfg(test)]
mod output_inspection_tests {
    use super::*;

    fn fixture(tagged: bool, id3: bool) -> Vec<u8> {
        let frame_size = 417usize;
        let prefix = if id3 { 15 } else { 0 };
        let mut bytes = vec![0u8; prefix + frame_size * 4];
        if id3 { bytes[..15].copy_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x05abcde"); }
        for index in 0..4 {
            let offset = prefix + index * frame_size;
            bytes[offset..offset + 4].copy_from_slice(&0xfffb_9000u32.to_be_bytes());
        }
        if tagged {
            let tag = prefix + 36;
            bytes[tag..tag + 4].copy_from_slice(b"Info");
            bytes[tag + 4..tag + 8].copy_from_slice(&15u32.to_be_bytes());
            bytes[tag + 8..tag + 12].copy_from_slice(&3u32.to_be_bytes());
            bytes[tag + 12..tag + 16].copy_from_slice(&((frame_size * 4) as u32).to_be_bytes());
            let lame = tag + 120;
            bytes[lame..lame + 9].copy_from_slice(b"LAME3.100");
            let packed = (576u32 << 12) | 720;
            bytes[lame + 21..lame + 24].copy_from_slice(&packed.to_be_bytes()[1..]);
        }
        bytes
    }

    #[test]
    fn encoded_inspection_reads_gapless_and_plain_streams_and_rejects_truncation() {
        for id3 in [false, true] {
            for tagged in [false, true] {
                let bytes = fixture(tagged, id3);
                let parsed = inspect_encoded_reader(io::Cursor::new(&bytes), bytes.len() as u64).unwrap();
                assert_eq!(parsed, EncodedMp3Info { sample_rate: 44_100, channels: 2,
                    frames_per_packet: 1152, frames: if tagged { 2160 } else { 4608 } });
                let mut truncated = bytes.clone();
                truncated.pop();
                assert!(inspect_encoded_reader(io::Cursor::new(&truncated), truncated.len() as u64).is_err());
                if tagged {
                    let mut bad_count = bytes.clone();
                    let count = usize::from(id3) * 15 + 36 + 8;
                    bad_count[count..count + 4].copy_from_slice(&2u32.to_be_bytes());
                    assert!(inspect_encoded_reader(io::Cursor::new(&bad_count), bad_count.len() as u64).is_err());
                    let mut erased = bytes.clone();
                    let identifier = usize::from(id3) * 15 + 36 + 120;
                    // Outside a LAME tag the delay and padding count only when the
                    // tag's CRC16 verifies, and an Info tag reports its declared
                    // frame count rather than a byte-based estimate.
                    for name in [b"Lavc", b"Lavf"] {
                        let mut alias = bytes.clone();
                        alias[identifier..identifier + 4].copy_from_slice(name);
                        assert_eq!(inspect_encoded_reader(io::Cursor::new(&alias), alias.len() as u64).unwrap().frames, 3456,
                            "{name:?}: an unverified tag does not trim");
                        let crc = crc16(&alias[usize::from(id3) * 15..identifier + 34]);
                        alias[identifier + 34..identifier + 36].copy_from_slice(&crc.to_be_bytes());
                        assert_eq!(inspect_encoded_reader(io::Cursor::new(&alias), alias.len() as u64).unwrap().frames, 2160,
                            "{name:?}: a verified tag trims");
                    }
                    erased[identifier..identifier + 9].fill(0);
                    let parsed = inspect_encoded_reader(io::Cursor::new(&erased), erased.len() as u64).unwrap();
                    assert_eq!(parsed.frames, 3456, "an erased identifier without a verified CRC does not trim");
                    let crc = crc16(&erased[usize::from(id3) * 15..identifier + 34]);
                    erased[identifier + 34..identifier + 36].copy_from_slice(&crc.to_be_bytes());
                    let parsed = inspect_encoded_reader(io::Cursor::new(&erased), erased.len() as u64).unwrap();
                    assert_eq!(parsed.frames, 2160, "an erased identifier with a verified CRC trims");
                }
            }
        }
    }

    #[test]
    fn cbr_info_length_matches_native_byte_based_packet_rounding() {
        let mut bytes = Vec::new();
        for index in 0..11 {
            let padded = index >= 2 && index % 2 == 0;
            let size = 522 + usize::from(padded);
            let offset = bytes.len();
            bytes.resize(offset + size, 0);
            let word = 0xfff3_e0c4u32 | (u32::from(padded) << 9);
            bytes[offset..offset + 4].copy_from_slice(&word.to_be_bytes());
        }
        let tag = 13;
        bytes[tag..tag + 4].copy_from_slice(b"Info");
        bytes[tag + 4..tag + 8].copy_from_slice(&15u32.to_be_bytes());
        bytes[tag + 8..tag + 12].copy_from_slice(&10u32.to_be_bytes());
        let stream_bytes = bytes.len() as u32;
        bytes[tag + 12..tag + 16].copy_from_slice(&stream_bytes.to_be_bytes());
        let lame = tag + 120;
        bytes[lame..lame + 9].copy_from_slice(b"LAME3.100");
        let packed = (576u32 << 12) | 1088;
        bytes[lame + 21..lame + 24].copy_from_slice(&packed.to_be_bytes()[1..]);
        let parsed = inspect_encoded_reader(io::Cursor::new(&bytes), bytes.len() as u64).unwrap();
        assert_eq!(parsed, EncodedMp3Info { sample_rate: 22_050, channels: 1,
            frames_per_packet: 576, frames: 4672 });
    }

    #[test]
    fn cbr_info_preserves_native_binary64_packet_rounding() {
        // AudioToolbox reports 30 packets for 29 exact 32 kHz/320 kbps
        // packets because its binary64 duration calculation rounds upward.
        for (declared, estimated) in [(13u32, 14u64), (23, 23), (29, 30), (31, 32), (32, 32)] {
            let frame_size = 1440usize;
            let mut bytes = vec![0u8; frame_size * (declared as usize + 1)];
            for offset in (0..bytes.len()).step_by(frame_size) {
                bytes[offset..offset + 4].copy_from_slice(&0xfffb_e8c4u32.to_be_bytes());
            }
            let tag = 21;
            bytes[tag..tag + 4].copy_from_slice(b"Info");
            bytes[tag + 4..tag + 8].copy_from_slice(&15u32.to_be_bytes());
            bytes[tag + 8..tag + 12].copy_from_slice(&declared.to_be_bytes());
            let stream_bytes = bytes.len() as u32;
            bytes[tag + 12..tag + 16].copy_from_slice(&stream_bytes.to_be_bytes());
            let lame = tag + 120;
            bytes[lame..lame + 9].copy_from_slice(b"LAME3.100");
            let packed = (576u32 << 12) | 832;
            bytes[lame + 21..lame + 24].copy_from_slice(&packed.to_be_bytes()[1..]);
            let parsed = inspect_encoded_reader(io::Cursor::new(&bytes), bytes.len() as u64).unwrap();
            assert_eq!(parsed, EncodedMp3Info { sample_rate: 32_000, channels: 1,
                frames_per_packet: 1152, frames: estimated * 1152 - 576 - 832 });
        }
    }

    /// A zero-bodied stream laid out like the engine's output: a LAME-style tag
    /// frame (`tag_word`, tag at `4 + side`) followed by one frame per word in
    /// `audio`. AudioToolbox and the inspector read only frame headers and the
    /// tag. The tag declares one packet per audio frame, 576 samples of delay and
    /// 1000 of padding; its CRC16 verifies only when `crc_valid`.
    fn tagged_stream(tag_word: u32, side: usize, audio: &[u32], marker: &[u8; 4], ident: [u8; 4], crc_valid: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        for word in std::iter::once(&tag_word).chain(audio) {
            let start = bytes.len();
            bytes.resize(start + frame_size(*word), 0);
            bytes[start..start + 4].copy_from_slice(&word.to_be_bytes());
        }
        let tag = 4 + side;
        bytes[tag..tag + 4].copy_from_slice(marker);
        bytes[tag + 4..tag + 8].copy_from_slice(&15u32.to_be_bytes());
        bytes[tag + 8..tag + 12].copy_from_slice(&(audio.len() as u32).to_be_bytes());
        let stream_bytes = bytes.len() as u32;
        bytes[tag + 12..tag + 16].copy_from_slice(&stream_bytes.to_be_bytes());
        let lame = tag + 120;
        bytes[lame..lame + 4].copy_from_slice(&ident);
        let packed = (576u32 << 12) | 1000;
        bytes[lame + 21..lame + 24].copy_from_slice(&packed.to_be_bytes()[1..]);
        let crc = crc16(&bytes[..lame + 34]);
        bytes[lame + 34..lame + 36].copy_from_slice(&(if crc_valid { crc } else { !crc }).to_be_bytes());
        bytes
    }

    /// MPEG-1 Layer III 44.1 kHz joint stereo: a 128 kbps tag frame, then audio
    /// frames from this bitrate-index ladder (192, 320, 64, … kbps) with the
    /// padding bit set on every odd frame.
    fn vbr_44k_stereo(marker: &[u8; 4], ident: [u8; 4], crc_valid: bool) -> Vec<u8> {
        const LADDER: [u32; 20] = [11, 14, 5, 13, 9, 7, 12, 10, 14, 5, 9, 11, 13, 7, 10, 12, 9, 14, 5, 11];
        let audio: Vec<u32> = LADDER.iter().enumerate()
            .map(|(index, &step)| 0xfffb_0040 | (step << 12) | (((index as u32) & 1) << 9)).collect();
        tagged_stream(0xfffb_9040, 32, &audio, marker, ident, crc_valid)
    }

    /// MPEG-2 Layer III 22.05 kHz mono: a 64 kbps tag frame, then audio frames
    /// from this ladder (96, 160, 8, … kbps), padding on every odd frame.
    fn vbr_22k_mono(marker: &[u8; 4], ident: [u8; 4], crc_valid: bool) -> Vec<u8> {
        const LADDER: [u32; 20] = [10, 14, 1, 12, 8, 11, 4, 13, 14, 5, 9, 10, 7, 12, 3, 14, 6, 11, 8, 10];
        let audio: Vec<u32> = LADDER.iter().enumerate()
            .map(|(index, &step)| 0xfff3_00c4 | (step << 12) | (((index as u32) & 1) << 9)).collect();
        tagged_stream(0xfff3_80c4, 9, &audio, marker, ident, crc_valid)
    }

    fn inspected_frames(bytes: &[u8]) -> u64 {
        inspect_encoded_reader(io::Cursor::new(bytes), bytes.len() as u64).unwrap().frames
    }

    // Lengths AudioToolbox reports for the streams above, observed with the
    // macOS default build's reader for every (marker, identifier, CRC) cell:
    // (`Info`?, identifier, CRC verifies?, frames). The delay and padding are 576
    // and 1000 samples and the tag declares 20 packets. Only an `Info` tag of a
    // LAME or erased identifier is extrapolated from the first audio frame's
    // bitrate; everything else reports the declared packets, trimmed when the
    // identifier is LAME or the CRC verifies.
    const VBR_44K: [(bool, [u8; 4], bool, u64); 16] = [
        (true, *b"LAME", true, 20_312), (true, *b"LAME", false, 20_312),
        (true, *b"Lavf", true, 21_464), (true, *b"Lavf", false, 23_040),
        (true, *b"Lavc", true, 21_464), (true, *b"Lavc", false, 23_040),
        (true, [0; 4], true, 20_312), (true, [0; 4], false, 21_888),
        (false, *b"LAME", true, 21_464), (false, *b"LAME", false, 21_464),
        (false, *b"Lavf", true, 21_464), (false, *b"Lavf", false, 23_040),
        (false, *b"Lavc", true, 21_464), (false, *b"Lavc", false, 23_040),
        (false, [0; 4], true, 21_464), (false, [0; 4], false, 23_040),
    ];
    const VBR_22K: [(bool, [u8; 4], bool, u64); 16] = [
        (true, *b"LAME", true, 9_368), (true, *b"LAME", false, 9_368),
        (true, *b"Lavf", true, 9_944), (true, *b"Lavf", false, 11_520),
        (true, *b"Lavc", true, 9_944), (true, *b"Lavc", false, 11_520),
        (true, [0; 4], true, 9_368), (true, [0; 4], false, 10_944),
        (false, *b"LAME", true, 9_944), (false, *b"LAME", false, 9_944),
        (false, *b"Lavf", true, 9_944), (false, *b"Lavf", false, 11_520),
        (false, *b"Lavc", true, 9_944), (false, *b"Lavc", false, 11_520),
        (false, [0; 4], true, 9_944), (false, [0; 4], false, 11_520),
    ];

    #[test]
    fn info_tagged_variable_bitrate_streams_match_native_lengths() {
        for (build, table, rate, channels, spp) in [
            (vbr_44k_stereo as fn(&[u8; 4], [u8; 4], bool) -> Vec<u8>, &VBR_44K, 44_100, 2, 1152),
            (vbr_22k_mono, &VBR_22K, 22_050, 1, 576),
        ] {
            for &(info, ident, crc_valid, frames) in table {
                let bytes = build(if info { b"Info" } else { b"Xing" }, ident, crc_valid);
                let parsed = inspect_encoded_reader(io::Cursor::new(&bytes), bytes.len() as u64)
                    .unwrap_or_else(|e| panic!("info={info} {ident:?} crc={crc_valid}: {e}"));
                assert_eq!(parsed, EncodedMp3Info { sample_rate: rate, channels, frames_per_packet: spp, frames },
                    "info={info} {ident:?} crc={crc_valid}");
            }
        }
    }

    #[test]
    fn variable_bitrate_info_extrapolates_from_the_first_audio_frame_not_the_tag_frame() {
        // The stream's first audio frame is 192 kbps; the tag frame is 128 kbps
        // and the 20 audio frames average about 160 kbps. Only the first audio
        // frame's bitrate reproduces AudioToolbox's estimate.
        let bytes = vbr_44k_stereo(b"Info", *b"LAME", true);
        let audio_bytes: u64 = bytes.len() as u64 - frame_size(0xfffb_9040) as u64;
        let estimate = |kbps: f64| ((audio_bytes as f64 * 8.0) / (kbps * 1000.0) * (44_100.0 / 1152.0)).ceil() as u64;
        let frames = inspected_frames(&bytes);
        assert_eq!(frames, estimate(192.0) * 1152 - 576 - 1000);
        assert_ne!(estimate(128.0), estimate(192.0));
    }

    #[test]
    fn info_extrapolation_follows_the_declared_byte_count_not_the_stream() {
        // Observed lengths for the 44.1 kHz LAME `Info` stream above after
        // rewriting the tag's declared byte count (offset 36 + 12).
        let bytes = vbr_44k_stereo(b"Info", *b"LAME", true);
        let length = bytes.len() as u32;
        for (declared, frames) in [(length, 20_312), (length * 2, 42_200), (length / 2, 8_792), (length + 1000, 22_616)] {
            let mut patched = bytes.clone();
            patched[48..52].copy_from_slice(&declared.to_be_bytes());
            assert_eq!(inspected_frames(&patched), frames, "declared bytes {declared}");
        }
        // A byte count below the tag frame extrapolates a negative length.
        let mut empty = bytes.clone();
        empty[48..52].copy_from_slice(&0u32.to_be_bytes());
        assert!(inspect_encoded_reader(io::Cursor::new(&empty), empty.len() as u64).is_err());

        // Without the byte-count field the declared packet count stands and no
        // delay or padding is trimmed, even for a LAME tag.
        let tag = 36;
        let lame = tag + 120;
        let mut no_bytes = bytes.clone();
        no_bytes.copy_within(tag + 16..lame + 36, tag + 12);
        no_bytes[lame + 32..lame + 36].fill(0);
        no_bytes[tag + 4..tag + 8].copy_from_slice(&13u32.to_be_bytes());
        let crc = crc16(&no_bytes[..lame - 4 + 34]);
        no_bytes[lame - 4 + 34..lame - 4 + 36].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(inspected_frames(&no_bytes), 20 * 1152);
    }

    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    #[test]
    #[ignore = "requires PN_MP3_POSTFLIGHT_CORPORA and a new PN_MP3_POSTFLIGHT_RECEIPT"]
    fn encoded_inspection_matches_audiotoolbox_corpora() {
        use crate::audio_input::{AudioReader, InputLibraries};
        use serde_json::json;
        use std::fs;

        let roots = std::env::var_os("PN_MP3_POSTFLIGHT_CORPORA").expect("PN_MP3_POSTFLIGHT_CORPORA");
        let receipt = std::env::var_os("PN_MP3_POSTFLIGHT_RECEIPT").expect("PN_MP3_POSTFLIGHT_RECEIPT");
        let mut rows = Vec::new();
        let mut root_counts = Vec::new();
        let mut mismatches = 0usize;
        for root in std::env::split_paths(&roots) {
            let mut paths: Vec<_> = fs::read_dir(&root).unwrap().map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("mp3")))
                .collect();
            paths.sort();
            assert!(!paths.is_empty(), "empty MP3 corpus {}", root.display());
            root_counts.push(json!({"root":root, "files":paths.len()}));
            for path in paths {
                let native = (|| -> Result<Vec<u64>, String> {
                    let reader = AudioReader::open(&path, None, &InputLibraries::default()).map_err(|e| e.to_string())?;
                    let stream = reader.source_format();
                    let frames = reader.file_length_frames().map_err(|e| e.to_string())?;
                    reader.close().map_err(|e| e.to_string())?;
                    let frames = u64::try_from(frames).map_err(|e| e.to_string())?;
                    Ok(vec![stream.sample_rate.to_bits(), u64::from(stream.format_id), u64::from(stream.format_flags),
                        u64::from(stream.bytes_per_packet), u64::from(stream.frames_per_packet),
                        u64::from(stream.bytes_per_frame), u64::from(stream.channels_per_frame),
                        u64::from(stream.bits_per_channel), u64::from(stream.reserved), frames])
                })();
                let candidate = inspect_encoded_output(&path).map(|info| vec![
                    f64::from(info.sample_rate).to_bits(), u64::from(u32::from_be_bytes(*b".mp3")), 0, 0,
                    u64::from(info.frames_per_packet), 0, u64::from(info.channels), 0, 0, info.frames
                ]).map_err(|e| e.to_string());
                let equal = native == candidate;
                if !equal { mismatches += 1; }
                rows.push(json!({"path":path, "sha256":file_sha256(&path).unwrap(),
                    "native":native, "candidate":candidate, "equal":equal}));
            }
        }
        let count = rows.len();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mp3.rs");
        let output = json!({"schema":"platinum-mp3-postflight-v1", "source_sha256":file_sha256(source).unwrap(),
            "fields":["sample_rate_bits","format_id","format_flags","bytes_per_packet","frames_per_packet",
                "bytes_per_frame","channels_per_frame","bits_per_channel","reserved","file_length_frames"],
            "corpora":root_counts, "files":count, "mismatches":mismatches, "rows":rows});
        let mut file = OpenOptions::new().write(true).create_new(true).open(receipt).unwrap();
        serde_json::to_writer(&mut file, &output).unwrap();
        file.write_all(b"\n").unwrap();
        assert_eq!(mismatches, 0, "{mismatches}/{count} MP3 postflight mismatches");
    }
}
