//! Platinum Notes' LAME recorder and source-dependent Xing/LAME handling.
//!
//! This is an adapter for an explicitly supplied, open-source LAME 3.100 dylib,
//! not a claim that every 3.100 build produces the reference bytes. A complete
//! output also needs the original processing, AudioToolbox conversion, sample
//! scheduling and metadata handling. See `docs/VERIFICATION.md` for scope.
#![cfg(target_os = "macos")]

use libloading::Library;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};

const SCAN_BYTES: usize = 65_536;

#[derive(Debug)]
pub enum Mp3Error {
    Io(io::Error),
    Load(String),
    InvalidDigest,
    DigestMismatch { expected: String, actual: String },
    UnsupportedVersion(String),
    InvalidConfiguration(&'static str),
    InvalidBuffer(&'static str),
    Lame { operation: &'static str, code: i32 },
    NativeStateRequired,
    StreamFailed,
    TagRewriteFailed,
}
impl fmt::Display for Mp3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "MP3 I/O: {e}"),
            Self::Load(e) => write!(f, "LAME library: {e}"),
            Self::InvalidDigest => write!(f, "expected SHA-256 must be 64 hexadecimal digits"),
            Self::DigestMismatch { expected, actual } => write!(f, "LAME SHA-256 mismatch: expected {expected}, got {actual}"),
            Self::UnsupportedVersion(v) => write!(f, "LAME 3.100 required; library reports {v:?}"),
            Self::InvalidConfiguration(s) | Self::InvalidBuffer(s) => f.write_str(s),
            Self::Lame { operation, code } => write!(f, "LAME {operation} failed ({code})"),
            Self::NativeStateRequired => write!(f, "native header update reads a caller stack byte before initializing it as a result; its actual value is required for this source"),
            Self::StreamFailed => write!(f, "MP3 stream was invalidated by an earlier error"),
            Self::TagRewriteFailed => write!(f, "LAME tag rewrite did not write its reported frame"),
        }
    }
}
impl std::error::Error for Mp3Error {}
impl From<io::Error> for Mp3Error { fn from(e: io::Error) -> Self { Self::Io(e) } }

#[repr(C)]
struct Sha256Context { count: [u32; 2], hash: [u32; 8], wbuf: [u32; 16] }
#[repr(C)]
struct DlInfo { filename: *const c_char, base: *mut c_void, symbol: *const c_char, address: *mut c_void }
unsafe extern "C" {
    fn CC_SHA256_Init(ctx: *mut Sha256Context) -> c_int;
    fn CC_SHA256_Update(ctx: *mut Sha256Context, data: *const c_void, len: u32) -> c_int;
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
    _library: Library,
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
        let digest = file_sha256(&path)?;
        if !digest.eq_ignore_ascii_case(expected_sha256) {
            return Err(Mp3Error::DigestMismatch { expected: expected_sha256.to_ascii_lowercase(), actual: digest });
        }
        // SAFETY: the path is explicitly supplied by the caller and pinned above;
        // the caller is responsible for trusting executable library initializers.
        let library = unsafe { Library::new(&path) }.map_err(|e| Mp3Error::Load(e.to_string()))?;
        unsafe fn symbol<T: Copy>(lib: &Library, name: &'static [u8]) -> Result<T, Mp3Error> {
            // SAFETY: names and ABI signatures below are declared by LAME's public API.
            unsafe { lib.get::<T>(name) }.map(|s| *s).map_err(|e| Mp3Error::Load(e.to_string()))
        }
        // SAFETY: each symbol is copied with its public C ABI; the library stays owned here.
        unsafe {
            let version_fn: Version = symbol(&library, b"get_lame_version\0")?;
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
                _library: library,
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
    /// Number of channels in the graph source (1 or 2), also passed to LAME.
    pub channels: u8,
    pub mode: Mp3Mode,
    pub source: SourceHeader,
    /// Oracle/debug context only; never surface as a normal CLI setting.
    /// The original updater reads this byte before initializing it in some
    /// branches. If unavailable, finish fails rather than guessing.
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
        if !matches!(config.channels, 1 | 2) { return Err(Mp3Error::InvalidConfiguration("MP3 source channel count must be 1 or 2")); }
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
        set("set_num_channels", library.set_channels, i32::from(config.channels))?;
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
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(staging)?;
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

fn frame_size(word: u32) -> usize {
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

fn update_source_header(file: &mut File, source: &Mp3Header, scratch_byte: Option<u8>) -> Result<bool, Mp3Error> {
    let mut window = read_scan(file)?;
    let Some(loc) = locate_header(&window.data) else { return Ok(false); };
    let data = &mut window.data;
    let first_mode = (loc.word >> 6) & 3;
    let tag = loc.xing + 120;
    let recognized = recognized_lame(data, tag);
    let mut end = loc.frame + 4;
    data[loc.xing..loc.xing + 4].copy_from_slice(if source.xing { b"Xing" } else { b"Info" });

    let has_tag = if recognized && source.lame_identifier.is_some() {
        true
    } else {
        if recognized {
            let available = loc.frame_size.saturating_sub(tag - loc.frame).min(36);
            if available != 0 {
                data[tag..tag + available].fill(0);
                end = end.max(tag + available);
            }
        }
        scratch_byte.ok_or(Mp3Error::NativeStateRequired)? != 0
    };
    if has_tag {
        if let Some(id) = source.lame_identifier {
            if id[0] != 0 {
                data[tag..tag + 9].fill(0);
                let count = id.iter().position(|&b| b == 0).unwrap_or(9);
                data[tag..tag + count].copy_from_slice(&id[..count]);
                end = end.max(tag + 9);
            }
        }
    }
    let mode = if source.stereo_mode_change {
        if first_mode == loc.second_mode { loc.second_mode ^ 1 } else { first_mode }
    } else { loc.second_mode };
    let new_word = (loc.word & !(3 << 6)) | (mode << 6);
    data[loc.frame..loc.frame + 4].copy_from_slice(&new_word.to_be_bytes());
    if has_tag {
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
    if end > loc.frame {
        file.seek(SeekFrom::Start(window.base + loc.frame as u64))?;
        file.write_all(&data[loc.frame..end])?;
    }
    Ok(true)
}
