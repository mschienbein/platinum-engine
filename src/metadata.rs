//! Platinum Notes marker construction and platform serialization.
//!
//! JSON order and number formatting are deliberately delegated to the current
//! macOS Foundation runtime, using the original 20-entry constructor order and
//! `NSJSONSerialization` options 0. This is not a promise that another macOS
//! build will emit the same bytes. Full-container and whole-file parity require
//! the tag writer and an original-file oracle as well.
#![cfg(target_os = "macos")]

use std::error::Error;
use std::ffi::{c_char, c_void, CStr};
use std::fmt;
use std::ptr;
use std::slice;

/// An NSString's contents, either ordinary Unicode text or exact UTF-16 code
/// units. The latter can express isolated surrogates so Foundation, rather than
/// a lossy conversion, determines whether the original serializer returns nil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerText {
    Utf8(String),
    Utf16(Vec<u16>),
}

impl From<String> for MarkerText {
    fn from(value: String) -> Self { Self::Utf8(value) }
}

impl From<&str> for MarkerText {
    fn from(value: &str) -> Self { Self::Utf8(value.to_owned()) }
}

/// A `MIKTagPlatinumNotes` value. Strings may be nil; its JSONDict method emits
/// nil strings as empty strings. The stored floating-point properties are
/// doubles even when the controller originally supplied a widened f32.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub os_type: Option<MarkerText>,
    pub software_version: Option<MarkerText>,
    pub backup_file_name: Option<MarkerText>,
    pub is_processed: bool,
    pub applied_volume_adjustment: bool,
    pub applied_clip_repair: bool,
    pub applied_pitch_correction: bool,
    pub applied_warmth: bool,
    pub applied_expansion: bool,
    pub applied_compression: bool,
    pub volume_analysis_version: i32,
    pub volume_units: Option<MarkerText>,
    pub target_volume: f64,
    pub input_volume: f64,
    pub output_volume: f64,
    pub clip_repair_count: i32,
    pub warmth_preset: i32,
    pub input_pitch: f64,
    pub output_pitch: f64,
    pub speed_adjustment: f64,
}

/// Inputs read by PNFile when constructing the post-processing marker.
/// `backup_file_name` is its backupPath, not automatically the source path.
/// These convenience fields accept UTF-8; when an original NSString contains
/// arbitrary UTF-16 units, set the corresponding `MarkerText::Utf16` field on
/// the returned `Marker` without a lossy conversion.
#[derive(Debug, Clone, Copy)]
pub struct ProcessingMarker<'a> {
    pub software_version: Option<&'a str>,
    pub backup_file_name: Option<&'a str>,
    pub applied_clip_repair: bool,
    pub applied_pitch_correction: bool,
    pub applied_warmth: bool,
    pub expansion_applied: i32,
    pub compression_applied: i32,
    pub volume_analysis_version: i32,
    pub target_volume: f32,
    pub input_volume: f32,
    pub output_volume: f32,
    pub clip_repair_count: i32,
    pub exciter_option: i32,
    pub input_pitch: f32,
    pub output_pitch: f32,
    pub speed_adjustment: f64,
}

impl Marker {
    /// Reproduce the observed controller assignments, including the f32-to-f64
    /// widening and warmth option mapping. Inputs are not inferred from presets.
    pub fn from_processing(info: ProcessingMarker<'_>) -> Self {
        let warmth_preset = if info.applied_warmth {
            match info.exciter_option { 0 => 1, 2 => 2, _ => 0 }
        } else { 0 };
        Self {
            os_type: Some("Mac".into()),
            software_version: info.software_version.map(MarkerText::from),
            backup_file_name: info.backup_file_name.map(MarkerText::from),
            is_processed: true,
            applied_volume_adjustment: true,
            applied_clip_repair: info.applied_clip_repair,
            applied_pitch_correction: info.applied_pitch_correction,
            applied_warmth: info.applied_warmth,
            applied_expansion: info.expansion_applied == 1,
            applied_compression: info.compression_applied == 1,
            volume_analysis_version: info.volume_analysis_version,
            volume_units: Some(if info.volume_analysis_version < 4 { "dB" } else { "LUFS" }.into()),
            target_volume: f64::from(info.target_volume),
            input_volume: f64::from(info.input_volume),
            output_volume: f64::from(info.output_volume),
            clip_repair_count: info.clip_repair_count,
            warmth_preset,
            input_pitch: f64::from(info.input_pitch),
            output_pitch: f64::from(info.output_pitch),
            speed_adjustment: info.speed_adjustment,
        }
    }

    /// Return Foundation's unencoded JSON bytes. `Ok(None)` is the original
    /// tagData nil outcome: either `isValidJSONObject:` was false or the
    /// serialization returned nil or an NSError. In particular, on the probed
    /// runtime NaN and infinities fail validation; they are not JSON null.
    pub fn json_bytes(&self) -> Result<Option<Vec<u8>>, MarkerError> {
        with_foundation_json(self, |bytes| Ok(bytes.to_vec()))
    }

    /// Return the original line-wrapped Base64 of Foundation's JSON bytes.
    /// `Ok(None)` has the same nil semantics as `json_bytes`.
    pub fn encoded_bytes(&self) -> Result<Option<Vec<u8>>, MarkerError> {
        with_foundation_json(self, encode_marker_base64)
    }

    /// Decode a present container payload. Absence must be decided by the
    /// container reader; an empty or malformed present marker is an error.
    pub fn from_encoded_bytes(payload: &[u8]) -> Result<Self, MarkerError> {
        marker_from_json(&decode_marker_base64(payload))
    }
}

/// Foundation infrastructure failures or cases not established by the source.
/// Serializer rejection is `Ok(None)`, not one of these errors.
#[derive(Debug)]
pub enum MarkerError {
    FoundationUnavailable(&'static str),
    FoundationObjectNil(&'static str),
    FoundationDataInvalid,
    UnobservedShortBase64 { length: usize },
    SizeOverflow,
    InvalidWave(&'static str),
    InvalidMarker(&'static str),
    InvalidField(&'static str),
    Io { operation: &'static str, source: std::io::Error },
}

impl fmt::Display for MarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FoundationUnavailable(name) => write!(f, "Foundation class {name} unavailable"),
            Self::FoundationObjectNil(operation) => write!(f, "Foundation returned nil from {operation}"),
            Self::FoundationDataInvalid => f.write_str("Foundation returned nonempty NSData with null bytes"),
            Self::UnobservedShortBase64 { length } => write!(f, "MIKTag Base64 behavior for {length} input bytes is unobserved and its original allocation is unsafe"),
            Self::SizeOverflow => f.write_str("marker or container size exceeds the supported range"),
            Self::InvalidWave(message) => write!(f, "unsupported or invalid WAV: {message}"),
            Self::InvalidMarker(message) => write!(f, "invalid processing marker: {message}"),
            Self::InvalidField(field) => write!(f, "processing marker has an invalid {field} field"),
            Self::Io { operation, source } => write!(f, "{operation}: {source}"),
        }
    }
}

impl Error for MarkerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self { Self::Io { source, .. } => Some(source), _ => None }
    }
}

/// MIKTag's standard-alphabet encoder, with LF after each 18 complete
/// three-byte groups and an additional unconditional final LF. The original
/// wrapper's allocation is unsafe for 0 or 1 input bytes, so those cases are
/// explicitly unobserved rather than claimed to match runtime behavior.
pub fn encode_marker_base64(data: &[u8]) -> Result<Vec<u8>, MarkerError> {
    if data.len() < 2 { return Err(MarkerError::UnobservedShortBase64 { length: data.len() }); }
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let groups = data.len() / 3;
    let tail = data.len() % 3;
    let capacity = groups.checked_mul(4)
        .and_then(|v| v.checked_add(if tail == 0 { 0 } else { 4 }))
        .and_then(|v| v.checked_add(groups / 18))
        .and_then(|v| v.checked_add(1))
        .ok_or(MarkerError::SizeOverflow)?;
    let mut out = Vec::with_capacity(capacity);
    let mut chunks = data.chunks_exact(3);
    for (index, chunk) in chunks.by_ref().enumerate() {
        out.push(ALPHABET[(chunk[0] >> 2) as usize]);
        out.push(ALPHABET[(((chunk[0] & 3) << 4) | (chunk[1] >> 4)) as usize]);
        out.push(ALPHABET[(((chunk[1] & 15) << 2) | (chunk[2] >> 6)) as usize]);
        out.push(ALPHABET[(chunk[2] & 63) as usize]);
        if (index + 1) % 18 == 0 { out.push(b'\n'); }
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        out.push(ALPHABET[(rest[0] >> 2) as usize]);
        out.push(ALPHABET[(((rest[0] & 3) << 4) | if rest.len() == 2 { rest[1] >> 4 } else { 0 }) as usize]);
        out.push(if rest.len() == 2 { ALPHABET[((rest[1] & 15) << 2) as usize] } else { b'=' });
        out.push(b'=');
    }
    out.push(b'\n');
    Ok(out)
}

// MIKTag's 0x701c decoder skips every negative lookup-table entry, including
// whitespace, '=' and the WAV writer's declared zero pad. It emits each full
// byte as soon as enough sextets arrive; it does not require complete quartets.
fn decode_marker_base64(payload: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(payload.len() / 4 * 3);
    let mut pending = 0u16;
    let mut bits = 0u8;
    for &byte in payload {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        pending = (pending << 6) | u16::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((pending >> bits) as u8);
            pending &= (1 << bits) - 1;
        }
    }
    decoded
}

/// RIFF PLTN chunk as written by the PN inserter. Unlike ordinary RIFF chunk
/// lengths, this writer stores the payload length *rounded up to even* and
/// includes the zero pad in that declared length. `None` models nil tagData.
pub fn wav_pltn_chunk(payload: Option<&[u8]>) -> Result<Vec<u8>, MarkerError> {
    let bytes = payload.unwrap_or_default();
    let padded = bytes.len().checked_add(bytes.len() & 1).ok_or(MarkerError::SizeOverflow)?;
    let length = u32::try_from(padded).map_err(|_| MarkerError::SizeOverflow)?;
    let capacity = 8usize.checked_add(padded).ok_or(MarkerError::SizeOverflow)?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(b"PLTN");
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(bytes);
    if bytes.len() & 1 != 0 { out.push(0); }
    Ok(out)
}

fn wav_io(operation: &'static str, source: std::io::Error) -> MarkerError {
    MarkerError::Io { operation, source }
}

// The parser stops on invalid/truncated child headers and skips the remaining
// RIFF bytes. A short LIST receives no OnLIST callback and is also dropped.
// It only recognizes alphanumeric and space bytes in a chunk identifier.
fn visit_wav_chunks<R: std::io::Read + std::io::Seek>(
    input: &mut R,
    end: u64,
    mut visit: impl FnMut(&mut R, [u8; 8], u64) -> Result<(), MarkerError>,
) -> Result<(), MarkerError> {
    let mut position = 12u64;
    while position < end {
        if position & 1 != 0 { position += 1; }
        if end - position < 8 { break; }
        input.seek(std::io::SeekFrom::Start(position)).map_err(|e| wav_io("seek WAV chunk", e))?;
        let mut header = [0u8; 8];
        input.read_exact(&mut header).map_err(|e| wav_io("read WAV chunk header", e))?;
        position += 8;
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64;
        if !header[..4].iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b' ')
            || length > end - position
        {
            break;
        }
        if &header[..4] != b"PLTN" && !(&header[..4] == b"LIST" && length < 4) {
            visit(input, header, length)?;
        }
        position += length;
    }
    Ok(())
}

/// Stream a fresh WAV from `input` to `output`, replacing any existing PLTN
/// chunks with a new PLTN at the end. Call after TagLib's ID3 save and write to
/// a separately created/truncated output: this does not replace a file or
/// manage temporary-file publication. The input is scanned before any output
/// is written and must not change during the operation.
///
/// The source's parser copies recognized top-level chunks, drops all PLTN
/// chunks, copies a LIST only when it has at least its four-byte type, replaces
/// each copied odd chunk's external pad with zero, and discards bytes after the
/// declared RIFF or after an invalid/truncated child header. The new PLTN
/// size includes its even padding. `None` models nil serialized tagData from a
/// present PN object, not a request to omit marker writing.
pub fn rewrite_wav_marker<R: std::io::Read + std::io::Seek, W: std::io::Write>(
    input: &mut R,
    output: &mut W,
    payload: Option<&[u8]>,
) -> Result<(), MarkerError> {
    use std::io::{Read, SeekFrom};

    let file_length = input.seek(SeekFrom::End(0)).map_err(|e| wav_io("measure WAV", e))?;
    if file_length < 12 { return Err(MarkerError::InvalidWave("missing RIFF/WAVE header")); }
    input.seek(SeekFrom::Start(0)).map_err(|e| wav_io("seek WAV header", e))?;
    let mut header = [0u8; 12];
    input.read_exact(&mut header).map_err(|e| wav_io("read WAV header", e))?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return Err(MarkerError::InvalidWave("expected little-endian RIFF/WAVE"));
    }
    let riff_length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64;
    if riff_length < 4 { return Err(MarkerError::InvalidWave("RIFF size is shorter than the WAVE type")); }
    let end = riff_length + 8;
    if end > file_length { return Err(MarkerError::InvalidWave("RIFF extends beyond input")); }

    let bytes = payload.unwrap_or_default();
    let payload_len = u64::try_from(bytes.len()).map_err(|_| MarkerError::SizeOverflow)?;
    let padded = payload_len.checked_add(payload_len & 1).ok_or(MarkerError::SizeOverflow)?;
    let pltn_length = u32::try_from(padded).map_err(|_| MarkerError::SizeOverflow)?;
    let mut result_length = 12u64.checked_add(8).and_then(|v| v.checked_add(padded)).ok_or(MarkerError::SizeOverflow)?;
    visit_wav_chunks(input, end, |_, _, length| {
        result_length = result_length.checked_add(8)
            .and_then(|v| v.checked_add(length))
            .and_then(|v| v.checked_add(length & 1))
            .ok_or(MarkerError::SizeOverflow)?;
        Ok(())
    })?;
    let result_riff_length = u32::try_from(result_length - 8).map_err(|_| MarkerError::SizeOverflow)?;
    header[4..8].copy_from_slice(&result_riff_length.to_le_bytes());
    output.write_all(&header).map_err(|e| wav_io("write WAV header", e))?;
    visit_wav_chunks(input, end, |reader, chunk_header, length| {
        output.write_all(&chunk_header).map_err(|e| wav_io("write WAV chunk header", e))?;
        let mut body = (&mut *reader).take(length);
        let copied = std::io::copy(&mut body, &mut *output).map_err(|e| wav_io("copy WAV chunk", e))?;
        if copied != length {
            return Err(MarkerError::InvalidWave("input changed or ended while copying chunk"));
        }
        if length & 1 != 0 { output.write_all(&[0]).map_err(|e| wav_io("write WAV chunk padding", e))?; }
        Ok(())
    })?;
    output.write_all(b"PLTN").map_err(|e| wav_io("write PLTN identifier", e))?;
    output.write_all(&pltn_length.to_le_bytes()).map_err(|e| wav_io("write PLTN length", e))?;
    output.write_all(bytes).map_err(|e| wav_io("write PLTN payload", e))?;
    if payload_len & 1 != 0 { output.write_all(&[0]).map_err(|e| wav_io("write PLTN padding", e))?; }
    Ok(())
}

/// TagLib-facing identity and object bytes. TagLib owns the actual GEOB frame,
/// freeform atom, Xiph comment, and their save/layout ordering.
pub const ID3_GEOB_DESCRIPTION: &str = "PlatinumNotes";
pub const ID3_GEOB_MIME: &str = "application/json";
pub const MP4_FREEFORM_KEY: &str = "----:com.mixedinkey.platinumnotes:data";
pub const FLAC_XIPH_KEY: &str = "PLATINUMNOTES";

const KEYS: [&str; 20] = [
    "osType", "softwareVersion", "backupFileName", "isProcessed",
    "appliedVolumeAdjustment", "appliedClipRepair", "appliedPitchCorrection",
    "appliedWarmth", "appliedExpansion", "appliedCompression", "volumeAnalysisVersion",
    "volumeUnits", "targetVolume", "inputVolume", "outputVolume", "clipRepairCount",
    "warmthPreset", "inputPitch", "outputPitch", "speedAdjustment",
];

type Id = *mut c_void;
type Sel = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(token: *mut c_void);
}

// Referencing Foundation directly ensures its classes are loaded before objc_getClass.
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    fn NSClassFromString(name: Id) -> Id;
}

struct Pool(*mut c_void);
impl Pool {
    fn new() -> Self {
        // SAFETY: the token remains on this thread and is popped once in Drop.
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: the owning scope has not transferred threads or popped the token.
        unsafe { objc_autoreleasePoolPop(self.0) }
    }
}

fn selector(name: &CStr) -> Sel {
    // SAFETY: the static selector is NUL-terminated and remains valid.
    unsafe { sel_registerName(name.as_ptr()) }
}

fn string(class: Id, value: &str) -> Result<Id, MarkerError> {
    // SAFETY: correct Objective-C ABI for +stringWithBytes:length:encoding:;
    // Rust's UTF-8 bytes are readable for the duration of the call. Explicit
    // length preserves embedded NUL characters.
    let result = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, *const u8, usize, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(class, selector(c"stringWithBytes:length:encoding:"), value.as_ptr(), value.len(), 4)
    };
    if result.is_null() { Err(MarkerError::FoundationObjectNil("NSString stringWithBytes:length:encoding:")) } else { Ok(result) }
}

fn marker_string(class: Id, value: Option<&MarkerText>) -> Result<Id, MarkerError> {
    match value {
        None => string(class, ""),
        Some(MarkerText::Utf8(text)) => string(class, text),
        Some(MarkerText::Utf16(units)) => {
            // SAFETY: unichar is a native-endian u16 and units remains readable
            // through this call; NSString copies the supplied code units.
            let result = unsafe {
                let send: unsafe extern "C" fn(Id, Sel, *const u16, usize) -> Id =
                    std::mem::transmute(objc_msgSend as *const ());
                send(class, selector(c"stringWithCharacters:length:"), units.as_ptr(), units.len())
            };
            if result.is_null() {
                Err(MarkerError::FoundationObjectNil("NSString stringWithCharacters:length:"))
            } else {
                Ok(result)
            }
        }
    }
}

fn class(string_class: Id, name: &'static str) -> Result<Id, MarkerError> {
    let name_string = string(string_class, name)?;
    // SAFETY: name_string is an NSString owned by this scope's autorelease pool.
    let class = unsafe { NSClassFromString(name_string) };
    if class.is_null() { Err(MarkerError::FoundationUnavailable(name)) } else { Ok(class) }
}

fn number_bool(class: Id, value: bool) -> Result<Id, MarkerError> {
    // SAFETY: correct Objective-C ABI for +numberWithBool:.
    let result = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, i8) -> Id = std::mem::transmute(objc_msgSend as *const ());
        send(class, selector(c"numberWithBool:"), if value { 1 } else { 0 })
    };
    if result.is_null() { Err(MarkerError::FoundationObjectNil("NSNumber numberWithBool:")) } else { Ok(result) }
}

fn number_int(class: Id, value: i32) -> Result<Id, MarkerError> {
    // SAFETY: correct Objective-C ABI for +numberWithInt:.
    let result = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, i32) -> Id = std::mem::transmute(objc_msgSend as *const ());
        send(class, selector(c"numberWithInt:"), value)
    };
    if result.is_null() { Err(MarkerError::FoundationObjectNil("NSNumber numberWithInt:")) } else { Ok(result) }
}

fn number_double(class: Id, value: f64) -> Result<Id, MarkerError> {
    // SAFETY: correct Objective-C ABI for +numberWithDouble: including the FP register.
    let result = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, f64) -> Id = std::mem::transmute(objc_msgSend as *const ());
        send(class, selector(c"numberWithDouble:"), value)
    };
    if result.is_null() { Err(MarkerError::FoundationObjectNil("NSNumber numberWithDouble:")) } else { Ok(result) }
}

fn marker_from_json(bytes: &[u8]) -> Result<Marker, MarkerError> {
    if bytes.is_empty() { return Err(MarkerError::InvalidMarker("empty decoded payload")); }
    let _pool = Pool::new();
    let string_class = unsafe { objc_getClass(c"NSString".as_ptr()) };
    if string_class.is_null() { return Err(MarkerError::FoundationUnavailable("NSString")); }
    let data_class = class(string_class, "NSData")?;
    let dictionary_class = class(string_class, "NSDictionary")?;
    let null_class = class(string_class, "NSNull")?;
    let serializer_class = class(string_class, "NSJSONSerialization")?;
    // NSData copies the input and all returned objects stay within this pool.
    let data = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, *const u8, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(data_class, selector(c"dataWithBytes:length:"), bytes.as_ptr(), bytes.len())
    };
    if data.is_null() { return Err(MarkerError::FoundationObjectNil("NSData dataWithBytes:length:")); }
    let mut error = ptr::null_mut();
    let dict = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, Id, usize, *mut Id) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(serializer_class, selector(c"JSONObjectWithData:options:error:"), data, 0, &mut error)
    };
    if dict.is_null() || !error.is_null() || !is_kind(dict, dictionary_class) {
        return Err(MarkerError::InvalidMarker("expected a JSON object"));
    }
    let mut values = [ptr::null_mut(); 20];
    for (value, key) in values.iter_mut().zip(KEYS) {
        let key = string(string_class, key)?;
        // NSDictionary lookup returns nil for absent keys, as in the original.
        *value = unsafe {
            let send: unsafe extern "C" fn(Id, Sel, Id) -> Id =
                std::mem::transmute(objc_msgSend as *const ());
            send(dict, selector(c"objectForKeyedSubscript:"), key)
        };
    }
    let text = |index| marker_text(values[index], string_class, null_class, KEYS[index]);
    let boolean = |index| {
        check_numeric_selector(values[index], c"boolValue", KEYS[index])?;
        let value = unsafe {
            let send: unsafe extern "C" fn(Id, Sel) -> i8 = std::mem::transmute(objc_msgSend as *const ());
            send(values[index], selector(c"boolValue")) != 0
        };
        Ok::<_, MarkerError>(value)
    };
    let integer = |index| {
        check_numeric_selector(values[index], c"intValue", KEYS[index])?;
        let value = unsafe {
            let send: unsafe extern "C" fn(Id, Sel) -> i32 = std::mem::transmute(objc_msgSend as *const ());
            send(values[index], selector(c"intValue"))
        };
        Ok::<_, MarkerError>(value)
    };
    let double = |index| {
        check_numeric_selector(values[index], c"doubleValue", KEYS[index])?;
        let value = unsafe {
            let send: unsafe extern "C" fn(Id, Sel) -> f64 = std::mem::transmute(objc_msgSend as *const ());
            send(values[index], selector(c"doubleValue"))
        };
        Ok::<_, MarkerError>(value)
    };
    Ok(Marker {
        os_type: text(0)?, software_version: text(1)?, backup_file_name: text(2)?,
        is_processed: boolean(3)?, applied_volume_adjustment: boolean(4)?,
        applied_clip_repair: boolean(5)?, applied_pitch_correction: boolean(6)?,
        applied_warmth: boolean(7)?, applied_expansion: boolean(8)?, applied_compression: boolean(9)?,
        volume_analysis_version: integer(10)?, volume_units: text(11)?,
        target_volume: double(12)?, input_volume: double(13)?, output_volume: double(14)?,
        clip_repair_count: integer(15)?, warmth_preset: integer(16)?,
        input_pitch: double(17)?, output_pitch: double(18)?, speed_adjustment: double(19)?,
    })
}

fn is_kind(object: Id, class: Id) -> bool {
    unsafe {
        let send: unsafe extern "C" fn(Id, Sel, Id) -> i8 = std::mem::transmute(objc_msgSend as *const ());
        send(object, selector(c"isKindOfClass:"), class) != 0
    }
}

fn check_numeric_selector(object: Id, method: &CStr, key: &'static str) -> Result<(), MarkerError> {
    if object.is_null() { return Ok(()); }
    let supported = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, Sel) -> i8 = std::mem::transmute(objc_msgSend as *const ());
        send(object, selector(c"respondsToSelector:"), selector(method)) != 0
    };
    if supported { Ok(()) } else { Err(MarkerError::InvalidField(key)) }
}

fn marker_text(object: Id, string_class: Id, null_class: Id, key: &'static str)
    -> Result<Option<MarkerText>, MarkerError>
{
    if object.is_null() || is_kind(object, null_class) { return Ok(None); }
    if !is_kind(object, string_class) { return Err(MarkerError::InvalidField(key)); }
    let count = unsafe {
        let send: unsafe extern "C" fn(Id, Sel) -> usize = std::mem::transmute(objc_msgSend as *const ());
        send(object, selector(c"length"))
    };
    if count == 0 { return Ok(None); }
    let mut units = vec![0u16; count];
    #[repr(C)]
    struct Range { location: usize, length: usize }
    unsafe {
        let send: unsafe extern "C" fn(Id, Sel, *mut u16, Range) =
            std::mem::transmute(objc_msgSend as *const ());
        send(object, selector(c"getCharacters:range:"), units.as_mut_ptr(), Range { location: 0, length: count });
    }
    Ok(Some(match String::from_utf16(&units) {
        Ok(text) => MarkerText::Utf8(text),
        Err(_) => MarkerText::Utf16(units),
    }))
}

fn with_foundation_json<T>(marker: &Marker, use_bytes: impl FnOnce(&[u8]) -> Result<T, MarkerError>) -> Result<Option<T>, MarkerError> {
    let _pool = Pool::new();
    // SAFETY: NSString is loaded from the linked Foundation framework; class
    // and all autoreleased values are only used on the current thread.
    let string_class = unsafe { objc_getClass(c"NSString".as_ptr()) };
    if string_class.is_null() { return Err(MarkerError::FoundationUnavailable("NSString")); }
    let number_class = class(string_class, "NSNumber")?;
    let dictionary_class = class(string_class, "NSDictionary")?;
    let serializer_class = class(string_class, "NSJSONSerialization")?;
    let mut keys = [ptr::null_mut(); 20];
    for (key, name) in keys.iter_mut().zip(KEYS) { *key = string(string_class, name)?; }
    let values = [
        marker_string(string_class, marker.os_type.as_ref())?,
        marker_string(string_class, marker.software_version.as_ref())?,
        marker_string(string_class, marker.backup_file_name.as_ref())?,
        number_bool(number_class, marker.is_processed)?,
        number_bool(number_class, marker.applied_volume_adjustment)?,
        number_bool(number_class, marker.applied_clip_repair)?,
        number_bool(number_class, marker.applied_pitch_correction)?,
        number_bool(number_class, marker.applied_warmth)?,
        number_bool(number_class, marker.applied_expansion)?,
        number_bool(number_class, marker.applied_compression)?,
        number_int(number_class, marker.volume_analysis_version)?,
        marker_string(string_class, marker.volume_units.as_ref())?,
        number_double(number_class, marker.target_volume)?,
        number_double(number_class, marker.input_volume)?,
        number_double(number_class, marker.output_volume)?,
        number_int(number_class, marker.clip_repair_count)?,
        number_int(number_class, marker.warmth_preset)?,
        number_double(number_class, marker.input_pitch)?,
        number_double(number_class, marker.output_pitch)?,
        number_double(number_class, marker.speed_adjustment)?,
    ];
    // SAFETY: all 20 keys and values are valid objects held by this pool, with
    // the exact constructor argument order and ABI of NSDictionary.
    let dict = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, *const Id, *const Id, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(dictionary_class, selector(c"dictionaryWithObjects:forKeys:count:"), values.as_ptr(), keys.as_ptr(), keys.len())
    };
    if dict.is_null() { return Err(MarkerError::FoundationObjectNil("NSDictionary dictionaryWithObjects:forKeys:count:")); }
    // SAFETY: selector and receiver are NSJSONSerialization class and NSDictionary.
    let valid = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, Id) -> i8 = std::mem::transmute(objc_msgSend as *const ());
        send(serializer_class, selector(c"isValidJSONObject:"), dict) != 0
    };
    if !valid { return Ok(None); }
    let mut error: Id = ptr::null_mut();
    // SAFETY: error is a writable NSError**; dictionary and resulting NSData
    // live through the callback and are released when the pool drains.
    let data = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, Id, usize, *mut Id) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(serializer_class, selector(c"dataWithJSONObject:options:error:"), dict, 0, &mut error)
    };
    if data.is_null() || !error.is_null() { return Ok(None); }
    // SAFETY: data is a live NSData; exact method signatures are used.
    let (ptr, len) = unsafe {
        let length: unsafe extern "C" fn(Id, Sel) -> usize = std::mem::transmute(objc_msgSend as *const ());
        let bytes: unsafe extern "C" fn(Id, Sel) -> *const u8 = std::mem::transmute(objc_msgSend as *const ());
        (bytes(data, selector(c"bytes")), length(data, selector(c"length")))
    };
    if len > isize::MAX as usize || (ptr.is_null() && len != 0) { return Err(MarkerError::FoundationDataInvalid); }
    let bytes = if len == 0 { &[] } else {
        // SAFETY: NSData guarantees `length` bytes valid until the pool drains.
        unsafe { slice::from_raw_parts(ptr, len) }
    };
    use_bytes(bytes).map(Some)
}
