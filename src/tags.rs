//! Source-to-staging metadata transfer with a caller-selected TagLib bridge.
//!
//! The bridge must be built separately from the pinned, public TagLib source
//! described in `docs/BUILD.md`. The candidate is not
//! the proprietary library and its identity is not proof of byte parity.
//! Source files are opened read-only; destination must be an already encoded,
//! caller-owned staging file. After any reported error it must not be published.

use crate::metadata::{self, Marker, MarkerError};
use libloading::Library;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// These ABI values describe container families, not PN's input type codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Format { Mp3 = 1, Mp4 = 2, Wav = 3, Aiff = 4, Flac = 5 }

/// Match input uses the source family's direct-copy branch. The caller must
/// first resolve the actual output codec and container as PN's recorder does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputChoice { Explicit, MatchInput }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum DirectCopy { Id3 = 1, Mp4 = 2, Wav = 3, Aiff = 4, Flac = 5, Mp4ToId3 = 6 }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode { Direct(DirectCopy), Generic }

/// Read-only source metadata as projected by the pinned public TagLib bridge.
/// The seven scalar fields are the same getters used by generic transfer.
/// Properties and unsupported identifiers are TagLib's actual `PropertyMap`,
/// not a claim that all container fields can be represented in that map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagInspection {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: i64,
    pub title: Option<String>,
    pub comment: Option<String>,
    pub tempo: i64,
    pub initial_key: Option<String>,
    pub properties: BTreeMap<String, Vec<String>>,
    pub unsupported_data: Vec<String>,
    /// None means no on-disk ID3v2 tag; Some(empty) is a present empty tag.
    /// Frame identifiers are the ordered IDs reported by TagLib's parser.
    pub id3_frames: Option<Vec<Id3FrameInspection>>,
    pub artwork: Vec<ArtworkInspection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Id3FrameInspection {
    pub id: String,
    /// Frame-body size reported from the parsed ID3 frame header.
    pub byte_count: usize,
}

/// Metadata and SHA-256 of the encoded embedded image bytes; no image decode
/// or PNG re-encoding is performed. `container` identifies ID3, MP4 covr, FLAC
/// native-picture, or its `flac_xiph_picture`/`flac_xiph_coverart` comment.
/// The MIME is the declared type, not content sniffing. `picture_type` is an
/// ID3/FLAC picture role; `format_code` is an MP4 data type. None means that
/// the container does not supply the field; legacy Xiph COVERART has no
/// embedded MIME, description, or role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkInspection {
    pub container: String,
    pub mime_type: Option<String>,
    pub picture_type: Option<u32>,
    pub description: Option<String>,
    pub format_code: Option<u32>,
    pub byte_count: usize,
    pub sha256: String,
}

/// Select PN's direct-copy or seven-key generic branch. In match-input mode
/// PN dispatches by source path type even when the recorder's detected codec
/// produced a different destination family; a failed direct copy is reported
/// and the actual destination still receives a marker-save attempt.
pub fn transfer_mode(source: Format, destination: Format, choice: OutputChoice) -> Result<TransferMode, TagError> {
    use DirectCopy as D;
    use Format as F;
    if choice == OutputChoice::MatchInput {
        return Ok(TransferMode::Direct(match source {
            F::Mp3 => D::Id3, F::Mp4 => D::Mp4, F::Wav => D::Wav,
            F::Aiff => D::Aiff, F::Flac => D::Flac,
        }));
    }
    let direct = match (source, destination) {
        (F::Mp3, F::Mp3) => Some(D::Id3), (F::Mp4, F::Mp4) => Some(D::Mp4),
        (F::Wav, F::Wav) => Some(D::Wav), (F::Aiff, F::Aiff) => Some(D::Aiff),
        (F::Flac, F::Flac) => Some(D::Flac),
        (F::Mp4, F::Mp3) => Some(D::Mp4ToId3),
        _ => None,
    };
    Ok(direct.map_or(TransferMode::Generic, TransferMode::Direct))
}

#[derive(Debug)]
pub enum TagError {
    Io { operation: &'static str, source: io::Error },
    Library(String),
    WrongBridgeAbi(u32),
    PathContainsNul,
    SourceIsDestination,
    Marker(MarkerError),
    MarkerCallbackPanicked,
    MarkerReadCallbackPanicked,
    InspectionCallbackPanicked,
}
impl fmt::Display for TagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "{operation}: {source}"),
            Self::Library(e) => write!(f, "TagLib bridge: {e}"),
            Self::WrongBridgeAbi(n) => write!(f, "unsupported TagLib bridge ABI {n}"),
            Self::PathContainsNul => write!(f, "tag file path contains NUL"),
            Self::SourceIsDestination => write!(f, "source and destination are the same file"),
            Self::Marker(e) => write!(f, "marker serialization: {e}"),
            Self::MarkerCallbackPanicked => write!(f, "marker serialization panicked inside native callback"),
            Self::MarkerReadCallbackPanicked => write!(f, "marker reader panicked inside native callback"),
            Self::InspectionCallbackPanicked => write!(f, "tag inspection panicked inside native callback"),
        }
    }
}
impl std::error::Error for TagError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self { Self::Io { source, .. } => Some(source), Self::Marker(e) => Some(e), _ => None }
    }
}
impl From<MarkerError> for TagError { fn from(e: MarkerError) -> Self { Self::Marker(e) } }
fn io_error(operation: &'static str, source: io::Error) -> TagError { TagError::Io { operation, source } }

/// Direct-copy and generic-save failures are distinct because PN logs these
/// failures and still attempts marker saving. Successful return of this struct
/// is NOT proof that all operations succeeded: callers must check `is_success`.
#[derive(Debug)]
pub struct TransferReport {
    pub mode: TransferMode,
    pub copy_error: Option<String>,
    pub marker_error: Option<String>,
}
impl TransferReport {
    pub fn is_success(&self) -> bool { self.copy_error.is_none() && self.marker_error.is_none() }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Payload { bytes: *const u8, length: usize, present: c_int }
impl Payload {
    fn from_option(value: Option<&[u8]>) -> Self {
        match value {
            Some(bytes) => Self { bytes: bytes.as_ptr(), length: bytes.len(), present: 1 },
            None => Self { bytes: std::ptr::null(), length: 0, present: 0 },
        }
    }
}
type Version = unsafe extern "C" fn() -> u32;
type CopyDirect = unsafe extern "C" fn(c_int, *const c_char, *const c_char, *mut c_char, usize) -> c_int;
type SerializeMarker = unsafe extern "C" fn(*mut c_void, *mut Payload) -> c_int;
type RewriteWav = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type CopyGeneric = unsafe extern "C" fn(c_int, c_int, *const c_char, *const c_char, SerializeMarker, RewriteWav, *mut c_void, *mut c_char, usize) -> c_int;
type SaveMarker = unsafe extern "C" fn(c_int, *const c_char, SerializeMarker, RewriteWav, *mut c_void, *mut c_char, usize) -> c_int;
type ReceiveMarker = unsafe extern "C" fn(*mut c_void, *const u8, usize, c_int) -> c_int;
type ReadMarker = unsafe extern "C" fn(c_int, *const c_char, ReceiveMarker, *mut c_void, *mut c_char, usize) -> c_int;
type ReceiveInspection = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> c_int;
type Inspect = unsafe extern "C" fn(c_int, *const c_char, ReceiveInspection, *mut c_void, *mut c_char, usize) -> c_int;

struct InspectionCallback { result: Option<TagInspection>, failure: Option<TagError> }
unsafe extern "C" fn receive_inspection(context: *mut c_void, bytes: *const u8, length: usize) -> c_int {
    if context.is_null() { return 1; }
    // SAFETY: inspect passes a live exclusive context; bytes are borrowed only
    // for this synchronous callback under the caller-trusted bridge contract.
    let state = unsafe { &mut *context.cast::<InspectionCallback>() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if state.result.is_some() || (length != 0 && bytes.is_null()) || length > isize::MAX as usize {
            return Err(TagError::Library("invalid tag inspection callback".into()));
        }
        let json = if length == 0 { &[][..] } else {
            // SAFETY: the trusted bridge supplies length readable bytes.
            unsafe { std::slice::from_raw_parts(bytes, length) }
        };
        serde_json::from_slice(json).map_err(|e| TagError::Library(format!("invalid tag inspection JSON: {e}")))
    }));
    match result {
        Ok(Ok(value)) => { state.result = Some(value); 0 }
        Ok(Err(error)) => { state.failure = Some(error); 1 }
        Err(_) => { state.failure = Some(TagError::InspectionCallbackPanicked); 1 }
    }
}

struct ReadCallback { result: Option<Option<Vec<u8>>>, failure: Option<TagError> }
unsafe extern "C" fn receive_marker(context: *mut c_void, bytes: *const u8, length: usize, present: c_int) -> c_int {
    if context.is_null() { return 1; }
    // SAFETY: read_processing_marker provides a live context; the trusted
    // bridge calls synchronously with bytes borrowed for this invocation.
    let state = unsafe { &mut *context.cast::<ReadCallback>() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if state.result.is_some() || !matches!(present, 0 | 1) ||
            (present == 0 && length != 0) || (length != 0 && bytes.is_null()) || length > isize::MAX as usize {
            return Err(TagError::Library("invalid marker reader callback".into()));
        }
        if present == 0 { return Ok(None); }
        let mut data = Vec::new();
        data.try_reserve_exact(length).map_err(|e| TagError::Library(format!("allocate marker payload: {e}")))?;
        if length != 0 {
            // SAFETY: the caller-trusted bridge promises a readable range for
            // the callback duration; the bounds were checked above.
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(bytes, length) });
        }
        Ok(Some(data))
    }));
    match result {
        Ok(Ok(value)) => { state.result = Some(value); 0 }
        Ok(Err(error)) => { state.failure = Some(error); 1 }
        Err(_) => { state.failure = Some(TagError::MarkerReadCallbackPanicked); 1 }
    }
}

struct MarkerCallback<'a> {
    marker: &'a Marker,
    bytes: Option<Vec<u8>>,
    failure: Option<TagError>,
}

unsafe extern "C" fn serialize_marker(context: *mut c_void, out: *mut Payload) -> c_int {
    if context.is_null() || out.is_null() { return 1; }
    // SAFETY: native_with_marker passes a live, exclusively borrowed context;
    // the bridge calls synchronously and does not retain either pointer.
    let state = unsafe { &mut *context.cast::<MarkerCallback<'_>>() };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.marker.encoded_bytes())) {
        Ok(Ok(bytes)) => {
            state.bytes = bytes;
            // SAFETY: the bridge supplies writable storage for one Payload.
            unsafe { out.write(Payload::from_option(state.bytes.as_deref())) };
            0
        }
        Ok(Err(error)) => { state.failure = Some(TagError::Marker(error)); 1 }
        Err(_) => { state.failure = Some(TagError::MarkerCallbackPanicked); 1 }
    }
}

unsafe extern "C" fn rewrite_wav_marker(context: *mut c_void, path: *const c_char) -> c_int {
    if context.is_null() || path.is_null() { return 1; }
    // The bridge lends its destination path and callback state synchronously.
    // Its FileStream must still be open: closing it first changes RIFF bytes
    // visible to the original marker writer by flushing buffered header writes.
    let state = unsafe { &mut *context.cast::<MarkerCallback<'_>>() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
        let path = Path::new(std::ffi::OsStr::from_bytes(bytes));
        let payload = state.marker.encoded_bytes()?;
        rewrite_wav(path, payload.as_deref())
    }));
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => { state.failure = Some(error); 1 }
        Err(_) => { state.failure = Some(TagError::MarkerCallbackPanicked); 1 }
    }
}

fn native_with_marker(marker: &Marker, call: impl FnOnce(SerializeMarker, RewriteWav, *mut c_void, *mut c_char, usize) -> c_int)
    -> Result<Option<String>, TagError> {
    let mut context = MarkerCallback { marker, bytes: None, failure: None };
    let error = native_call(|out, len| call(serialize_marker, rewrite_wav_marker, (&mut context as *mut MarkerCallback<'_>).cast(), out, len));
    match context.failure { Some(error) => Err(error), None => Ok(error) }
}

/// The native bridge and its open-source TagLib dependency must be trusted: a
/// dynamic library can execute initializers. The handle remains alive for all
/// calls. A bridge ABI/version string does not establish source provenance.
pub struct TagBridge {
    _library: Library,
    path: PathBuf,
    direct: CopyDirect,
    generic: CopyGeneric,
    marker: SaveMarker,
    read_marker: ReadMarker,
    inspect: Inspect,
}
impl TagBridge {
    /// # Safety
    /// `path` must name a trusted native bridge implementing taglib_bridge.h
    /// and built against the intended open-source TagLib; loading it executes
    /// its normal dynamic-library initializers.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, TagError> {
        let path = fs::canonicalize(path).map_err(|e| io_error("resolve bridge", e))?;
        // SAFETY: trust and ABI compatibility are the caller's preconditions.
        let lib = unsafe { Library::new(&path) }.map_err(|e| TagError::Library(e.to_string()))?;
        // SAFETY: the caller guarantees this exported version function's ABI.
        let version: Version = unsafe { *lib.get(b"pn_tag_bridge_abi\0").map_err(|e| TagError::Library(e.to_string()))? };
        // SAFETY: the caller guarantees the function obtained above.
        let abi = unsafe { version() };
        if abi != 5 { return Err(TagError::WrongBridgeAbi(abi)); }
        // SAFETY: the version was checked and the caller guarantees this
        // trusted bridge's C ABI. The library remains in the returned object.
        let (direct, generic, marker, read_marker, inspect) = unsafe {
            let direct: CopyDirect = *lib.get(b"pn_tag_copy_direct\0").map_err(|e| TagError::Library(e.to_string()))?;
            let generic: CopyGeneric = *lib.get(b"pn_tag_copy_generic\0").map_err(|e| TagError::Library(e.to_string()))?;
            let marker: SaveMarker = *lib.get(b"pn_tag_save_marker\0").map_err(|e| TagError::Library(e.to_string()))?;
            let read_marker: ReadMarker = *lib.get(b"pn_tag_read_marker\0").map_err(|e| TagError::Library(e.to_string()))?;
            let inspect: Inspect = *lib.get(b"pn_tag_inspect\0").map_err(|e| TagError::Library(e.to_string()))?;
            (direct, generic, marker, read_marker, inspect)
        };
        Ok(Self { _library: lib, path, direct, generic, marker, read_marker, inspect })
    }
    pub fn bridge_path(&self) -> &Path { &self.path }

    /// Read the source's generic values, actual TagLib properties/unsupported
    /// identifiers, ordered ID3v2 frames and embedded artwork inventory.
    /// Opens read-only; no proprietary framework is loaded. An unparseable
    /// container, artwork record or callback is an error rather than a
    /// fabricated empty inventory.
    pub fn inspect(&self, path: impl AsRef<Path>, format: Format) -> Result<TagInspection, TagError> {
        let path = CString::new(path.as_ref().as_os_str().as_bytes()).map_err(|_| TagError::PathContainsNul)?;
        let mut context = InspectionCallback { result: None, failure: None };
        let error = native_call(|out, len| {
            // SAFETY: the trusted bridge lends the JSON only during this call.
            unsafe { (self.inspect)(format as c_int, path.as_ptr(), receive_inspection,
                (&mut context as *mut InspectionCallback).cast(), out, len) }
        });
        if let Some(error) = context.failure { return Err(error); }
        if let Some(error) = error { return Err(TagError::Library(error)); }
        context.result.ok_or_else(|| TagError::Library("tag inspector returned no callback".into()))
    }

    /// Inspect the existing source without modifying it. `None` means the
    /// container was inspected and no marker was found; `Some([])` is a
    /// present empty payload, not absence. This returns encoded marker bytes,
    /// not decoded JSON. Malformed payloads remain present for the caller to
    /// reject; container and I/O failures are errors.
    ///
    /// ID3 selects the first PlatinumNotes GEOB. MP4 returns the freeform
    /// UTF-8 string-list projection; unsupported atom types return an error.
    /// FLAC prefers its first Vorbis comment block,
    /// preserves an empty Xiph value, and otherwise falls back to ID3. WAV
    /// returns the last PLTN payload of at least four bytes (as in the native
    /// parser); if only shorter PLTN chunks exist it retains the last one.
    /// WAV's payload includes any padding included in the declared chunk size.
    pub fn read_processing_marker(&self, path: impl AsRef<Path>, format: Format) -> Result<Option<Vec<u8>>, TagError> {
        let path = CString::new(path.as_ref().as_os_str().as_bytes()).map_err(|_| TagError::PathContainsNul)?;
        let mut context = ReadCallback { result: None, failure: None };
        let error = native_call(|out, len| {
            // SAFETY: the bridge, ABI and synchronous callback are part of
            // load's caller-trust contract; all arguments outlive the call.
            unsafe { (self.read_marker)(format as c_int, path.as_ptr(), receive_marker,
                (&mut context as *mut ReadCallback).cast(), out, len) }
        });
        if let Some(error) = context.failure { return Err(error); }
        if let Some(error) = error { return Err(TagError::Library(error)); }
        context.result.ok_or_else(|| TagError::Library("marker reader returned no callback".into()))
    }

    /// Transfers into a separate, pre-existing destination staging file.
    /// `marker` is constructed by the caller from the actual processing result.
    /// Generic copying and marker insertion share one native save; direct copy
    /// commits before reopening the destination for marker insertion.
    pub fn transfer(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>, source_format: Format,
                    destination_format: Format, choice: OutputChoice, marker: &Marker) -> Result<TransferReport, TagError> {
        let source = source.as_ref(); let destination = destination.as_ref();
        let mode = transfer_mode(source_format, destination_format, choice)?;
        let src_meta = fs::metadata(source).map_err(|e| io_error("inspect source tag file", e))?;
        let dst_meta = fs::metadata(destination).map_err(|e| io_error("inspect staging tag file", e))?;
        if src_meta.dev() == dst_meta.dev() && src_meta.ino() == dst_meta.ino() { return Err(TagError::SourceIsDestination); }
        let src = CString::new(source.as_os_str().as_bytes()).map_err(|_| TagError::PathContainsNul)?;
        let dst = CString::new(destination.as_os_str().as_bytes()).map_err(|_| TagError::PathContainsNul)?;
        let mut report = TransferReport { mode, copy_error: None, marker_error: None };
        match mode {
            TransferMode::Direct(op) => {
                report.copy_error = native_call(|out, len| {
                    // SAFETY: function came from a caller-trusted bridge, paths
                    // and writable output buffer live through this call.
                    unsafe { (self.direct)(op as c_int, src.as_ptr(), dst.as_ptr(), out, len) }
                });
                report.marker_error = self.save_marker(&dst, destination_format, marker)?;
            }
            TransferMode::Generic => {
                report.copy_error = native_with_marker(marker, |callback, rewrite, context, out, len| {
                    // SAFETY: the bridge is trusted, all borrowed pointers
                    // remain live during this synchronous callback-based call.
                    unsafe { (self.generic)(source_format as c_int, destination_format as c_int, src.as_ptr(), dst.as_ptr(), callback, rewrite, context, out, len) }
                })?;
                if report.copy_error.is_some() {
                    report.marker_error = self.save_marker(&dst, destination_format, marker)?;
                }
            }
        }
        Ok(report)
    }
    fn save_marker(&self, c_path: &CStr, format: Format, marker: &Marker) -> Result<Option<String>, TagError> {
        native_with_marker(marker, |callback, rewrite, context, out, len| {
            // SAFETY: pointers and callback context remain live through the call.
            unsafe { (self.marker)(format as c_int, c_path.as_ptr(), callback, rewrite, context, out, len) }
        })
    }
}
fn native_call(call: impl FnOnce(*mut c_char, usize) -> c_int) -> Option<String> {
    let mut buffer = [0u8; 1024];
    let status = call(buffer.as_mut_ptr().cast(), buffer.len());
    if status == 0 { return None; }
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    let message = String::from_utf8_lossy(&buffer[..end]);
    Some(if message.is_empty() { format!("native bridge failed with status {status}") } else { message.into_owned() })
}

/// MIKTag uses the destination path plus `_`. Do not overwrite a pre-existing
/// sibling: it may belong to the user or a concurrent process. The temporary
/// file is created exclusively and removed on errors; replacement occurs only
/// after the whole rewrite has succeeded.
fn rewrite_wav(path: &Path, payload: Option<&[u8]>) -> Result<(), TagError> {
    let mut name = path.as_os_str().to_os_string(); name.push("_");
    let temp = PathBuf::from(name);
    let mut input = fs::File::open(path).map_err(|e| io_error("open WAV for marker", e))?;
    let mut output = OpenOptions::new().write(true).create_new(true).open(&temp)
        .map_err(|e| io_error("create WAV marker sibling", e))?;
    let result = (|| {
        metadata::rewrite_wav_marker(&mut input, &mut output, payload)?;
        output.flush().map_err(|e| io_error("flush WAV marker sibling", e))?;
        drop(output);
        fs::rename(&temp, path).map_err(|e| io_error("publish WAV marker staging file", e))?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}
