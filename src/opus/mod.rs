//! Opus input and output (Ogg Opus, RFC 7845) through a pinned libopus.
//!
//! Platinum Notes refuses `.opus`; nothing here has a reference to match. The
//! goal is instead *determinism*: the same input and settings give the same
//! output bytes on every supported build (macOS arm64 default, macOS arm64
//! `portable`, Linux aarch64, Linux x86_64). That rests on three things:
//!
//! * libopus 1.6.1 built **fixed-point** with no SIMD, run-time CPU detection or
//!   float API (`opus_get_version_string()` ends in `-fixed`), so encoding and
//!   decoding are pure integer arithmetic with no libm calls;
//! * 24-bit PCM at the library boundary (`opus_encode24` / `opus_decode24`) with
//!   explicit, platform-independent float conversions;
//! * this crate's own Ogg layer with a fixed page layout and a content-derived
//!   serial number.
//!
//! The library is a separately built, SHA-256 pinned shared object, like FLAC and
//! LAME, and is additionally checked by a known-answer self-test at load time.

pub mod head;
pub mod ogg;
pub mod tags;
mod reader;
mod writer;
#[cfg(test)]
mod tests;

pub use head::{CommentHeader, OpusHead};
pub use reader::{OpusReader, StreamInfo};
pub use writer::{OpusWriter, repack_with_comments};

use libloading::Library;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The only libopus accepted: exact release and fixed-point configuration.
pub const EXPECTED_VERSION: &str = "libopus 1.6.1-fixed";
/// Opus always decodes and encodes at 48 kHz here; there is no resampler.
pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms packets.
pub const FRAME_SAMPLES: usize = 960;
/// Target bitrate per channel (VBR): stereo 256 kbit/s equals the engine's fixed
/// AAC target, mono 128 kbit/s. Like the MP3 and AAC targets it does not depend
/// on the source file's own bitrate.
pub const BITRATE_PER_CHANNEL: i32 = 128_000;
/// libopus encoder complexity (maximum).
pub const COMPLEXITY: i32 = 10;
/// Audio pages close after at least this many samples (one second).
pub const PAGE_SAMPLES: u64 = 48_000;
/// Marker comment name, as for FLAC's Xiph comments.
pub const MARKER_FIELD: &str = "PLATINUMNOTES";
/// Comments that describe the source's loudness; RFC 7845 s5.2.1 requires them
/// to be updated or removed when the gain changes, which processing always does.
pub const STALE_GAIN_FIELDS: [&str; 2] = ["R128_TRACK_GAIN", "R128_ALBUM_GAIN"];

/// Stated in every per-item report that involves Opus.
pub const NO_REFERENCE_NOTE: &str =
    "Platinum Notes refuses Opus files, so there is no Platinum Notes reference for this item; output is deterministic across Platinum Engine builds but unverified against the original application";

#[derive(Debug)]
pub enum OpusError {
    Io { operation: &'static str, source: io::Error },
    Library(String),
    #[cfg(target_os = "linux")]
    LibraryPolicy(crate::linux_libraries::LibraryPolicyError),
    InvalidDigest,
    DigestMismatch { expected: String, actual: String },
    IncompatibleLibrary { version: String },
    SelfTest(&'static str),
    /// A valid Ogg stream whose first packet is not OpusHead (Vorbis, FLAC, Speex, ...).
    NotOpus(&'static str),
    /// Malformed Ogg or Opus data.
    Invalid(&'static str),
    /// Valid Ogg Opus that this engine does not handle (chained, multistream, ...).
    Unsupported(&'static str),
    /// A size limit that protects memory was exceeded.
    Limit(&'static str),
    SampleRate { source: f64, requested: f64 },
    Decode { operation: &'static str, code: i32 },
    Encode { operation: &'static str, code: i32 },
    Poisoned,
}
impl fmt::Display for OpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "Opus {operation}: {source}"),
            Self::Library(e) => write!(f, "libopus: {e}"),
            #[cfg(target_os = "linux")]
            Self::LibraryPolicy(e) => write!(f, "{e}"),
            Self::InvalidDigest => f.write_str("expected libopus SHA-256 must be 64 hexadecimal digits"),
            Self::DigestMismatch { expected, actual } => write!(f, "libopus SHA-256 mismatch: expected {expected}, got {actual}"),
            Self::IncompatibleLibrary { version } => write!(f, "expected {EXPECTED_VERSION:?}, library reports {version:?}"),
            Self::SelfTest(e) => write!(f, "libopus known-answer self-test failed: {e}"),
            Self::NotOpus(e) => write!(f, "not an Opus stream: {e}"),
            Self::Invalid(e) => write!(f, "invalid Ogg Opus stream: {e}"),
            Self::Unsupported(e) => write!(f, "unsupported Ogg Opus stream: {e}"),
            Self::Limit(e) => write!(f, "Ogg Opus size limit: {e}"),
            Self::SampleRate { source, requested } => write!(f,
                "Opus is processed at 48000 Hz without sample-rate conversion; {source} Hz to {requested} Hz is not available"),
            Self::Decode { operation, code } => write!(f, "libopus {operation} failed ({code})"),
            Self::Encode { operation, code } => write!(f, "libopus {operation} failed ({code})"),
            Self::Poisoned => f.write_str("Opus stream is unusable after an earlier error"),
        }
    }
}
impl std::error::Error for OpusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            #[cfg(target_os = "linux")]
            Self::LibraryPolicy(e) => Some(e),
            _ => None,
        }
    }
}
pub(crate) fn io_error(operation: &'static str, source: io::Error) -> OpusError { OpusError::Io { operation, source } }

// libopus constants (opus_defines.h, 1.6.1).
const OPUS_OK: c_int = 0;
const OPUS_APPLICATION_AUDIO: c_int = 2049;
const OPUS_SET_BITRATE: c_int = 4002;
const OPUS_SET_MAX_BANDWIDTH: c_int = 4004;
const OPUS_SET_VBR: c_int = 4006;
const OPUS_SET_COMPLEXITY: c_int = 4010;
const OPUS_SET_INBAND_FEC: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC: c_int = 4014;
const OPUS_SET_DTX: c_int = 4016;
const OPUS_SET_VBR_CONSTRAINT: c_int = 4020;
const OPUS_SET_FORCE_CHANNELS: c_int = 4022;
const OPUS_SET_SIGNAL: c_int = 4024;
const OPUS_GET_LOOKAHEAD: c_int = 4027;
const OPUS_SET_GAIN: c_int = 4034;
const OPUS_SET_LSB_DEPTH: c_int = 4036;
const OPUS_SET_EXPERT_FRAME_DURATION: c_int = 4040;
const OPUS_SET_PREDICTION_DISABLED: c_int = 4042;
const OPUS_AUTO: c_int = -1000;
const OPUS_BANDWIDTH_FULLBAND: c_int = 1105;
const OPUS_SIGNAL_MUSIC: c_int = 3002;
const OPUS_FRAMESIZE_20_MS: c_int = 5004;

type VersionString = unsafe extern "C" fn() -> *const c_char;
type EncoderCreate = unsafe extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut c_void;
type Destroy = unsafe extern "C" fn(*mut c_void);
type Encode24 = unsafe extern "C" fn(*mut c_void, *const i32, c_int, *mut u8, i32) -> i32;
type Ctl = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type DecoderCreate = unsafe extern "C" fn(i32, c_int, *mut c_int) -> *mut c_void;
type Decode24 = unsafe extern "C" fn(*mut c_void, *const u8, i32, *mut i32, c_int, c_int) -> c_int;

struct Functions {
    version: VersionString,
    encoder_create: EncoderCreate, encoder_destroy: Destroy, encode24: Encode24, encoder_ctl: Ctl,
    decoder_create: DecoderCreate, decoder_destroy: Destroy, decode24: Decode24, decoder_ctl: Ctl,
}

/// Evidence about the loaded library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryIdentity {
    pub canonical_path: PathBuf,
    pub sha256: String,
    pub version: String,
}

struct Inner {
    _library: Arc<Library>,
    _pinned: crate::native_library::PinnedLibrary,
    f: Functions,
    identity: LibraryIdentity,
}
// SAFETY: the function pointers are plain C entry points of a library kept
// loaded by `_library`; libopus encoder/decoder states are independent objects.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

/// A loaded, pinned and self-tested libopus. Cloning shares the handle.
#[derive(Clone)]
pub struct OpusLibrary(Arc<Inner>);

unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, OpusError> {
    // SAFETY: the caller states the exported symbol's ABI and keeps `library` alive.
    unsafe { library.get::<T>(name) }.map(|s| *s).map_err(|e| OpusError::Library(e.to_string()))
}

/// Results of the known-answer test per library digest, so concurrent jobs run it once.
static SELF_TESTED: Mutex<Vec<([u8; 32], Result<(), String>)>> = Mutex::new(Vec::new());

impl OpusLibrary {
    /// Load exactly `path`, which must have the given SHA-256 and, on Linux, be
    /// on the build's allowlist. A library that is not libopus 1.6.1 built
    /// fixed-point, or that fails the known-answer test, is refused.
    pub fn load(path: impl AsRef<Path>, expected_sha256: &str) -> Result<Self, OpusError> {
        let library = Self::load_unchecked(path, expected_sha256)?;
        library.self_test()?;
        Ok(library)
    }

    /// Everything `load` does except the known-answer test. Used to compute the
    /// pinned digests; never reachable from the engine.
    #[doc(hidden)]
    pub fn load_unchecked(path: impl AsRef<Path>, expected_sha256: &str) -> Result<Self, OpusError> {
        if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OpusError::InvalidDigest);
        }
        let path = std::fs::canonicalize(path).map_err(|e| io_error("resolve library path", e))?;
        #[cfg(target_os = "macos")]
        let pinned = crate::native_library::PinnedLibrary::open(&path).map_err(|e| OpusError::Library(e.to_string()))?;
        #[cfg(target_os = "linux")]
        let pinned = crate::linux_libraries::open_approved(&path, crate::linux_libraries::LibraryKind::Opus).map_err(OpusError::LibraryPolicy)?;
        if !pinned.digest_matches(expected_sha256) {
            return Err(OpusError::DigestMismatch { expected: expected_sha256.to_ascii_lowercase(), actual: pinned.digest_hex() });
        }
        // SAFETY: the caller supplies a trusted library; loading runs its initializers.
        #[cfg(target_os = "macos")]
        let library = unsafe { pinned.load() }.map_err(|e| OpusError::Library(e.to_string()))?;
        #[cfg(target_os = "linux")]
        let library = unsafe { pinned.load() }.map_err(|e| OpusError::LibraryPolicy(e.into()))?;
        // SAFETY: libopus 1.6.1 defines these symbols with the declared C ABI.
        let f = unsafe {
            let version: VersionString = symbol(&library, b"opus_get_version_string\0")?;
            #[cfg(target_os = "macos")]
            pinned.verify_symbol(version as *const () as *const c_void).map_err(|e| OpusError::Library(e.to_string()))?;
            #[cfg(target_os = "linux")]
            pinned.verify_symbol(version as *const () as *const c_void).map_err(|e| OpusError::LibraryPolicy(e.into()))?;
            Functions {
                version,
                encoder_create: symbol(&library, b"opus_encoder_create\0")?,
                encoder_destroy: symbol(&library, b"opus_encoder_destroy\0")?,
                encode24: symbol(&library, b"opus_encode24\0")?,
                encoder_ctl: symbol(&library, b"opus_encoder_ctl\0")?,
                decoder_create: symbol(&library, b"opus_decoder_create\0")?,
                decoder_destroy: symbol(&library, b"opus_decoder_destroy\0")?,
                decode24: symbol(&library, b"opus_decode24\0")?,
                decoder_ctl: symbol(&library, b"opus_decoder_ctl\0")?,
            }
        };
        // SAFETY: opus_get_version_string returns a static NUL-terminated string.
        let version = unsafe { CStr::from_ptr((f.version)()) }.to_string_lossy().into_owned();
        if version != EXPECTED_VERSION { return Err(OpusError::IncompatibleLibrary { version }); }
        let identity = LibraryIdentity { canonical_path: path, sha256: pinned.digest_hex(), version };
        Ok(Self(Arc::new(Inner { _library: library, _pinned: pinned, f, identity })))
    }

    pub fn identity(&self) -> &LibraryIdentity { &self.0.identity }

    fn self_test(&self) -> Result<(), OpusError> {
        let key = self.0._pinned.digest();
        {
            let done = SELF_TESTED.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, result)) = done.iter().find(|(digest, _)| *digest == key) {
                return result.clone().map_err(|_| OpusError::SelfTest("an earlier load of this library failed the test"));
            }
        }
        let outcome = known_answer_test(self);
        let mut done = SELF_TESTED.lock().unwrap_or_else(|e| e.into_inner());
        done.push((key, outcome.as_ref().map(|_| ()).map_err(|e| (*e).to_owned())));
        outcome.map_err(OpusError::SelfTest)
    }
}

/// Raw encoder state; destroyed on drop.
pub(crate) struct EncoderState { lib: OpusLibrary, raw: *mut c_void }
// SAFETY: an encoder state is used from one thread at a time (moved with its owner).
unsafe impl Send for EncoderState {}
impl EncoderState {
    pub(crate) fn new(lib: &OpusLibrary, channels: usize) -> Result<Self, OpusError> {
        let mut error: c_int = OPUS_OK;
        // SAFETY: valid arguments; the returned pointer is checked.
        let raw = unsafe { (lib.0.f.encoder_create)(SAMPLE_RATE as i32, channels as c_int, OPUS_APPLICATION_AUDIO, &mut error) };
        if raw.is_null() || error != OPUS_OK { return Err(OpusError::Encode { operation: "opus_encoder_create", code: error }); }
        let state = Self { lib: lib.clone(), raw };
        let channels = channels as i32;
        // The complete, explicit setting list: nothing relies on a library default.
        state.set("bitrate", OPUS_SET_BITRATE, BITRATE_PER_CHANNEL * channels)?;
        state.set("vbr", OPUS_SET_VBR, 1)?;
        state.set("vbr constraint", OPUS_SET_VBR_CONSTRAINT, 0)?;
        state.set("complexity", OPUS_SET_COMPLEXITY, COMPLEXITY)?;
        state.set("signal", OPUS_SET_SIGNAL, OPUS_SIGNAL_MUSIC)?;
        state.set("max bandwidth", OPUS_SET_MAX_BANDWIDTH, OPUS_BANDWIDTH_FULLBAND)?;
        state.set("force channels", OPUS_SET_FORCE_CHANNELS, OPUS_AUTO)?;
        state.set("inband fec", OPUS_SET_INBAND_FEC, 0)?;
        state.set("packet loss", OPUS_SET_PACKET_LOSS_PERC, 0)?;
        state.set("dtx", OPUS_SET_DTX, 0)?;
        state.set("prediction", OPUS_SET_PREDICTION_DISABLED, 0)?;
        state.set("lsb depth", OPUS_SET_LSB_DEPTH, 24)?;
        state.set("frame duration", OPUS_SET_EXPERT_FRAME_DURATION, OPUS_FRAMESIZE_20_MS)?;
        Ok(state)
    }
    fn set(&self, name: &'static str, request: c_int, value: i32) -> Result<(), OpusError> {
        // SAFETY: SET requests take one opus_int32 argument.
        let code = unsafe { (self.lib.0.f.encoder_ctl)(self.raw, request, value as c_int) };
        if code == OPUS_OK { Ok(()) } else { Err(OpusError::Encode { operation: name, code }) }
    }
    /// Encoder delay in 48 kHz samples (the Ogg Opus pre-skip).
    pub(crate) fn lookahead(&self) -> Result<i32, OpusError> {
        let mut value: i32 = 0;
        // SAFETY: GET requests take a pointer to opus_int32.
        let code = unsafe { (self.lib.0.f.encoder_ctl)(self.raw, OPUS_GET_LOOKAHEAD, &mut value as *mut i32) };
        if code == OPUS_OK && value >= 0 { Ok(value) } else { Err(OpusError::Encode { operation: "lookahead", code }) }
    }
    /// Encode exactly one 20 ms interleaved 24-bit frame.
    pub(crate) fn encode(&mut self, pcm: &[i32], out: &mut [u8]) -> Result<usize, OpusError> {
        debug_assert_eq!(pcm.len() % FRAME_SAMPLES, 0);
        // SAFETY: pcm holds FRAME_SAMPLES * channels samples; out's length bounds the packet.
        let n = unsafe { (self.lib.0.f.encode24)(self.raw, pcm.as_ptr(), FRAME_SAMPLES as c_int, out.as_mut_ptr(), out.len() as i32) };
        if n < 1 { Err(OpusError::Encode { operation: "opus_encode24", code: n }) } else { Ok(n as usize) }
    }
}
impl Drop for EncoderState {
    fn drop(&mut self) {
        // SAFETY: raw came from opus_encoder_create and is destroyed exactly once.
        unsafe { (self.lib.0.f.encoder_destroy)(self.raw) }
    }
}

/// Raw decoder state; destroyed on drop.
pub(crate) struct DecoderState { lib: OpusLibrary, raw: *mut c_void }
// SAFETY: a decoder state is used from one thread at a time (moved with its owner).
unsafe impl Send for DecoderState {}
impl DecoderState {
    pub(crate) fn new(lib: &OpusLibrary, channels: usize, output_gain: i16) -> Result<Self, OpusError> {
        let mut error: c_int = OPUS_OK;
        // SAFETY: valid arguments; the returned pointer is checked.
        let raw = unsafe { (lib.0.f.decoder_create)(SAMPLE_RATE as i32, channels as c_int, &mut error) };
        if raw.is_null() || error != OPUS_OK { return Err(OpusError::Decode { operation: "opus_decoder_create", code: error }); }
        let state = Self { lib: lib.clone(), raw };
        // SAFETY: SET_GAIN takes one opus_int32 (Q8 dB).
        let code = unsafe { (lib.0.f.decoder_ctl)(raw, OPUS_SET_GAIN, c_int::from(output_gain)) };
        if code != OPUS_OK { return Err(OpusError::Decode { operation: "output gain", code }); }
        Ok(state)
    }
    /// Decode one packet into `out` (room for 5760 frames); returns frames.
    pub(crate) fn decode(&mut self, packet: &[u8], out: &mut [i32], max_frames: usize) -> Result<usize, OpusError> {
        let len = i32::try_from(packet.len()).map_err(|_| OpusError::Limit("audio packet exceeds 2 GiB"))?;
        // SAFETY: out holds at least max_frames * channels samples (checked by the caller).
        let n = unsafe { (self.lib.0.f.decode24)(self.raw, packet.as_ptr(), len, out.as_mut_ptr(), max_frames as c_int, 0) };
        if n < 1 { Err(OpusError::Decode { operation: "opus_decode24", code: n }) } else { Ok(n as usize) }
    }
}
impl Drop for DecoderState {
    fn drop(&mut self) {
        // SAFETY: raw came from opus_decoder_create and is destroyed exactly once.
        unsafe { (self.lib.0.f.decoder_destroy)(self.raw) }
    }
}

/// float -> signed 24-bit, round half to even, saturating; NaN is silence.
pub fn f32_to_i24(sample: f32) -> i32 {
    if sample.is_nan() { return 0; }
    // Multiplying by 2^23 is exact; rounding is explicit, so no platform mode matters.
    let scaled = (sample * 8_388_608.0).round_ties_even();
    if scaled >= 8_388_607.0 { 8_388_607 } else if scaled <= -8_388_608.0 { -8_388_608 } else { scaled as i32 }
}
/// signed 24-bit (decoder output may slightly exceed it) -> float; exact scaling by 2^-23.
pub fn i24_to_f32(sample: i32) -> f32 { (sample as f32) * (1.0 / 8_388_608.0) }

// ---- known-answer self-test -------------------------------------------------

/// Integer-only deterministic test signal (no libm): LCG noise plus a
/// triangle-wave tone whose frequency drifts.
fn kat_signal(frames: usize, channels: usize) -> Vec<i32> {
    let mut state: u32 = 0x1234_5678;
    let mut phase: u32 = 0;
    let mut out = Vec::with_capacity(frames * channels);
    for n in 0..frames {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = ((state >> 8) as i32 & 0xffff) - 0x8000; // +-2^15
        phase = phase.wrapping_add(0x0300_0000 + (n as u32) * 2_000);
        let tri = ((phase >> 8) as i32 & 0x00ff_ffff) - 0x0080_0000; // +-2^23 sawtooth
        let tri = if tri < 0 { -tri } else { tri } - 0x0040_0000; // triangle +-2^22
        for c in 0..channels {
            let tone = if c == 0 { tri } else { -(tri / 2) + (noise << 4) };
            out.push((tone / 2 + (noise << 3)).clamp(-8_388_608, 8_388_607));
        }
    }
    out
}

/// SHA-256 pins of the known-answer test, shared by every build: these digests
/// are identical on macOS arm64, Linux aarch64 and Linux x86_64 with gcc and clang.
const KAT_ENCODED_SHA256: &str = "90942f25fbcc428e56d7e58d3169d816a583ba4063e22134f261b30f302d9957";
const KAT_DECODED_SHA256: &str = "3da354a0ae0e35b547ffe424e94af797b5401a947138c0bf6aa6f9c5554ca7db";
const KAT_LOOKAHEAD: i32 = 312;

/// Compute `(encoded, decoded)` digests of the known-answer test with the
/// product settings (stereo, 25 packets).
pub fn known_answer_digests(lib: &OpusLibrary) -> Result<(String, String, i32), OpusError> {
    use crate::portable::digest::Sha256;
    const PACKETS: usize = 25;
    let pcm = kat_signal(PACKETS * FRAME_SAMPLES, 2);
    let mut encoder = EncoderState::new(lib, 2)?;
    let lookahead = encoder.lookahead()?;
    let mut decoder = DecoderState::new(lib, 2, 0)?;
    let mut encoded = Sha256::new();
    let mut decoded = Sha256::new();
    let mut packet = vec![0u8; 4000];
    let mut out = vec![0i32; 5760 * 2];
    for chunk in pcm.chunks_exact(FRAME_SAMPLES * 2) {
        let n = encoder.encode(chunk, &mut packet)?;
        encoded.update(&(n as u32).to_le_bytes());
        encoded.update(&packet[..n]);
        let frames = decoder.decode(&packet[..n], &mut out, 5760)?;
        for sample in &out[..frames * 2] { decoded.update(&sample.to_le_bytes()); }
    }
    let hex = |d: [u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok((hex(encoded.finalize()), hex(decoded.finalize()), lookahead))
}

fn known_answer_test(lib: &OpusLibrary) -> Result<(), &'static str> {
    let (encoded, decoded, lookahead) = known_answer_digests(lib).map_err(|_| "the library could not run the test")?;
    if lookahead != KAT_LOOKAHEAD { return Err("encoder delay differs from the pinned value"); }
    if encoded != KAT_ENCODED_SHA256 { return Err("encoded packets differ from the pinned digest"); }
    if decoded != KAT_DECODED_SHA256 { return Err("decoded samples differ from the pinned digest"); }
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn float_to_24_bit_rounding_and_saturation() {
        assert_eq!(f32_to_i24(0.0), 0);
        assert_eq!(f32_to_i24(-0.0), 0);
        assert_eq!(f32_to_i24(1.0), 8_388_607);
        assert_eq!(f32_to_i24(-1.0), -8_388_608);
        assert_eq!(f32_to_i24(2.0), 8_388_607);
        assert_eq!(f32_to_i24(-3.0), -8_388_608);
        assert_eq!(f32_to_i24(f32::NAN), 0);
        assert_eq!(f32_to_i24(f32::INFINITY), 8_388_607);
        assert_eq!(f32_to_i24(f32::NEG_INFINITY), -8_388_608);
        // Half-way cases round to even.
        assert_eq!(f32_to_i24(0.5 / 8_388_608.0), 0);
        assert_eq!(f32_to_i24(1.5 / 8_388_608.0), 2);
        assert_eq!(f32_to_i24(2.5 / 8_388_608.0), 2);
        assert_eq!(f32_to_i24(-0.5 / 8_388_608.0), 0);
        assert_eq!(f32_to_i24(-1.5 / 8_388_608.0), -2);
    }

    #[test]
    fn i24_conversion_round_trips_exactly() {
        for v in [-8_388_608, -8_388_607, -1, 0, 1, 12_345, 8_388_607] {
            assert_eq!(f32_to_i24(i24_to_f32(v)), v);
        }
        assert_eq!(i24_to_f32(8_388_608), 1.0);
    }

    #[test]
    fn kat_signal_is_deterministic_and_in_range() {
        let a = kat_signal(2000, 2);
        assert_eq!(a, kat_signal(2000, 2));
        assert!(a.iter().all(|s| (-8_388_608..=8_388_607).contains(s)));
        assert!(a.iter().any(|s| *s != 0));
    }
}

#[cfg(test)]
mod library_tests {
    use super::*;
    /// `PN_OPUS_LIB` / `PN_OPUS_SHA256` select a trusted fixed-point libopus 1.6.1.
    pub(crate) fn load_for_tests() -> OpusLibrary {
        let path = std::env::var_os("PN_OPUS_LIB").expect("PN_OPUS_LIB");
        let digest = std::env::var("PN_OPUS_SHA256").expect("PN_OPUS_SHA256");
        OpusLibrary::load(path, &digest).expect("approved libopus")
    }
}
