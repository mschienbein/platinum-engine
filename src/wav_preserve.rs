//! Opt-in WAV tag preservation (`parity.allow_non_identical`; Linux and `portable` builds).
//!
//! The reference marker writer rewrites a WAV from inside the tag bridge's save, while TagLib's
//! stream is still open. TagLib appends a chunk and updates the RIFF size last, through a buffered
//! write, so at that moment the file holds the chunk but its header still declares the old length.
//! The reference parser stops at the declared end and drops the bytes after it, so the chunk
//! TagLib saved last (the `ID3 ` chunk, or the INFO `LIST` when both exist) never reaches the
//! output. Native Platinum Notes behaves the same, so the default policy keeps doing it exactly.
//!
//! With the opt-in the rewrite instead trusts the physical chunk sequence when the bytes after
//! the declared end are nothing but strictly valid chunks reaching exactly the end of the file:
//! the same walker runs over a view of the file whose RIFF size is corrected, so every other rule
//! (PLTN replaced, short LIST dropped, padding normalized, invalid headers stop the walk) is
//! unchanged. Anything else after the declared end is dropped as before.

use crate::metadata::{self, MarkerError};
use std::cell::Cell;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// The `non_identical_paths` member for an output that kept chunks the reference drops.
pub const NON_IDENTICAL_WAV_TAG_PRESERVATION: &str = "wav_tag_preservation";

/// TagLib appends one or two chunks; more than this after the declared end is not its work.
const MAX_TRAILING_CHUNKS: usize = 64;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static KEPT: Cell<bool> = const { Cell::new(false) };
}

/// While alive on this thread, marker rewrites keep the chunks TagLib wrote (when `enabled`).
/// The native bridge calls the rewrite synchronously on the calling thread, so a thread-local is
/// the whole channel between `TagBridge::transfer_with_options` and the callback.
pub(crate) struct Scope { previous: bool }

pub(crate) fn scope(enabled: bool) -> Scope {
    let previous = ACTIVE.replace(enabled);
    KEPT.set(false);
    Scope { previous }
}

impl Scope {
    /// Whether a rewrite in this scope kept at least one chunk the reference would have dropped.
    pub(crate) fn kept(&self) -> bool { KEPT.get() }
}

/// Appends the preservation member (after `artwork_copy`, the order the contract documents) when chunks were kept.
pub(crate) fn with_member(mut paths: Vec<&'static str>, kept: bool) -> Vec<&'static str> {
    if kept { paths.push(NON_IDENTICAL_WAV_TAG_PRESERVATION); }
    paths
}

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.set(self.previous);
        KEPT.set(false);
    }
}

/// `metadata::rewrite_wav_marker`, keeping the trailing chunks TagLib wrote when the scope is on.
pub(crate) fn rewrite_wav_marker<R: Read + Seek, W: Write>(
    input: &mut R, output: &mut W, payload: Option<&[u8]>,
) -> Result<(), MarkerError> {
    if !ACTIVE.get() { return metadata::rewrite_wav_marker(input, output, payload); }
    let Some(trailing) = trailing_chunks(input)? else { return metadata::rewrite_wav_marker(input, output, payload); };
    let mut view = CorrectedRiffSize { inner: input, size: trailing.riff_size.to_le_bytes(), position: 0 };
    metadata::rewrite_wav_marker(&mut view, output, payload)?;
    if trailing.copied { KEPT.set(true); }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Trailing {
    /// The RIFF size that makes the declared end equal the end of the file.
    riff_size: u32,
    /// At least one trailing chunk is one the rewrite copies (not a PLTN, not a LIST shorter than its type).
    copied: bool,
}

fn io_error(operation: &'static str, source: io::Error) -> MarkerError { MarkerError::Io { operation, source } }

fn is_chunk_id(id: &[u8]) -> bool { id.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b' ') }

/// `Some` only when the file is `RIFF....WAVE`, declares an end before the end of the file, and the bytes
/// in between are a bounded run of well-formed chunks that ends exactly at the end of the file.
fn trailing_chunks<R: Read + Seek>(input: &mut R) -> Result<Option<Trailing>, MarkerError> {
    let file_length = input.seek(SeekFrom::End(0)).map_err(|e| io_error("measure WAV", e))?;
    if file_length < 20 || file_length - 8 > u64::from(u32::MAX) { return Ok(None); }
    input.seek(SeekFrom::Start(0)).map_err(|e| io_error("seek WAV header", e))?;
    let mut header = [0u8; 12];
    input.read_exact(&mut header).map_err(|e| io_error("read WAV header", e))?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" { return Ok(None); }
    let riff_length = u64::from(u32::from_le_bytes([header[4], header[5], header[6], header[7]]));
    let declared_end = riff_length + 8;
    if riff_length < 4 || declared_end >= file_length || declared_end & 1 != 0 { return Ok(None); }

    let mut position = declared_end;
    let mut copied = false;
    for _ in 0..MAX_TRAILING_CHUNKS {
        if position == file_length { return Ok(Some(Trailing { riff_size: (file_length - 8) as u32, copied })); }
        if file_length - position < 8 { return Ok(None); }
        input.seek(SeekFrom::Start(position)).map_err(|e| io_error("seek WAV trailing chunk", e))?;
        let mut chunk = [0u8; 8];
        input.read_exact(&mut chunk).map_err(|e| io_error("read WAV trailing chunk header", e))?;
        if !is_chunk_id(&chunk[..4]) { return Ok(None); }
        let length = u64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
        let body = position + 8;
        let available = file_length - body;
        if length > available { return Ok(None); }
        // An odd chunk at the very end of the file may lack its pad byte.
        let padded = length + (length & 1);
        if &chunk[..4] != b"PLTN" && !(&chunk[..4] == b"LIST" && length < 4) { copied = true; }
        position = body + padded.min(available);
    }
    Ok(if position == file_length { Some(Trailing { riff_size: (file_length - 8) as u32, copied }) } else { None })
}

/// Reads the file as it is, except that the RIFF size field (bytes 4..8) reads as `size`.
struct CorrectedRiffSize<'a, R> { inner: &'a mut R, size: [u8; 4], position: u64 }

impl<R: Read> Read for CorrectedRiffSize<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        for (offset, byte) in buffer[..count].iter_mut().enumerate() {
            let at = self.position + offset as u64;
            if (4..8).contains(&at) { *byte = self.size[(at - 4) as usize]; }
        }
        self.position += count as u64;
        Ok(count)
    }
}

impl<R: Seek> Seek for CorrectedRiffSize<'_, R> {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        self.position = self.inner.seek(target)?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() & 1 != 0 { out.push(0); }
        out
    }

    /// A WAV whose header declares the RIFF size of `declared` and whose file also holds `trailing`.
    fn stale_wav(declared: &[Vec<u8>], trailing: &[u8]) -> Vec<u8> {
        let body: Vec<u8> = declared.concat();
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(&body);
        wav.extend_from_slice(trailing);
        wav
    }

    fn base() -> Vec<Vec<u8>> { vec![chunk(b"fmt ", &[1, 0, 1, 0, 0x44, 0xac, 0, 0, 0x88, 0x58, 1, 0, 2, 0, 16, 0]), chunk(b"data", &[0u8; 8])] }

    fn rewrite(file: &[u8], enabled: bool) -> (Vec<u8>, bool) {
        let scope = scope(enabled);
        let mut out = Vec::new();
        rewrite_wav_marker(&mut Cursor::new(file), &mut out, Some(b"{}")).unwrap();
        (out, scope.kept())
    }

    fn ids(wav: &[u8]) -> Vec<String> {
        let declared = u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize + 8;
        assert_eq!(declared, wav.len(), "the rewritten RIFF size must match the file");
        let (mut position, mut out) = (12, Vec::new());
        while position + 8 <= wav.len() {
            let length = u32::from_le_bytes(wav[position + 4..position + 8].try_into().unwrap()) as usize;
            out.push(String::from_utf8_lossy(&wav[position..position + 4]).into_owned());
            position += 8 + length + (length & 1);
        }
        out
    }

    #[test]
    fn the_default_policy_drops_every_chunk_after_the_declared_end() {
        let id3 = chunk(b"ID3 ", b"ID3 tag bytes");
        let file = stale_wav(&base(), &id3);
        let (plain, kept) = rewrite(&file, false);
        assert!(!kept);
        assert_eq!(ids(&plain), ["fmt ", "data", "PLTN"]);
        assert_eq!(plain, metadata_rewrite(&file), "off must be exactly the unchanged walker");
    }

    fn metadata_rewrite(file: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        metadata::rewrite_wav_marker(&mut Cursor::new(file), &mut out, Some(b"{}")).unwrap();
        out
    }

    #[test]
    fn the_opt_in_keeps_the_chunks_taglib_wrote_after_the_declared_end() {
        let list = chunk(b"LIST", b"INFOINAM\x04\0\0\0abc\0");
        for (label, id) in [("uppercase", b"ID3 "), ("lowercase", b"id3 ")] {
            let id3 = chunk(id, b"ID3 tag bytes!");
            for (trailing, expected) in [
                (id3.clone(), vec!["fmt ", "data", id_text(id), "PLTN"]),
                (list.clone(), vec!["fmt ", "data", "LIST", "PLTN"]),
                ([id3.clone(), list.clone()].concat(), vec!["fmt ", "data", id_text(id), "LIST", "PLTN"]),
            ] {
                let file = stale_wav(&base(), &trailing);
                let (kept_output, kept) = rewrite(&file, true);
                assert!(kept, "{label}");
                assert_eq!(ids(&kept_output), expected, "{label}");
                // The kept bytes are the chunks themselves, byte for byte.
                for piece in [&id3, &list] {
                    if trailing.windows(piece.len()).any(|window| window == piece.as_slice()) {
                        assert!(kept_output.windows(piece.len()).any(|window| window == piece.as_slice()), "{label}");
                    }
                }
            }
        }
    }

    fn id_text(id: &[u8; 4]) -> &'static str { if id == b"ID3 " { "ID3 " } else { "id3 " } }

    #[test]
    fn a_consistent_file_gives_the_same_bytes_with_and_without_the_opt_in() {
        let consistent = stale_wav(&[base(), vec![chunk(b"ID3 ", b"tag")]].concat(), &[]);
        let (off, kept_off) = rewrite(&consistent, false);
        let (on, kept_on) = rewrite(&consistent, true);
        assert_eq!(off, on);
        assert!(!kept_off && !kept_on);
        assert_eq!(ids(&on), ["fmt ", "data", "ID3 ", "PLTN"]);
    }

    #[test]
    fn only_strictly_valid_chunks_reaching_the_end_of_the_file_are_kept() {
        let good = chunk(b"ID3 ", b"tag bytes");
        let mut bad_id = good.clone(); bad_id[0] = 0;
        let mut too_long = good.clone(); too_long[4..8].copy_from_slice(&1000u32.to_le_bytes());
        let mut partial_header = good.clone(); partial_header.extend_from_slice(b"LIS");
        let mut unaligned = good.clone(); unaligned.push(0);
        let cases: [(&str, Vec<u8>); 4] = [
            ("non-alphanumeric identifier", bad_id), ("length beyond the file", too_long),
            ("partial header at the end", partial_header), ("a stray byte after the last chunk", unaligned),
        ];
        for (label, trailing) in cases {
            let file = stale_wav(&base(), &trailing);
            let (kept_output, kept) = rewrite(&file, true);
            assert!(!kept, "{label}");
            assert_eq!(kept_output, metadata_rewrite(&file), "{label}: anything else is dropped exactly as without the opt-in");
        }
        // An odd RIFF size cannot start a chunk boundary.
        let mut odd = stale_wav(&base(), &good);
        let declared = u32::from_le_bytes(odd[4..8].try_into().unwrap()) + 1;
        odd[4..8].copy_from_slice(&declared.to_le_bytes());
        let (_, kept) = rewrite(&odd, true);
        assert!(!kept);
    }

    #[test]
    fn trailing_chunks_the_rewrite_drops_anyway_are_not_reported_as_kept() {
        let file = stale_wav(&base(), &[chunk(b"PLTN", b"old marker"), chunk(b"LIST", b"IN")].concat());
        let (output, kept) = rewrite(&file, true);
        assert!(!kept, "a PLTN is replaced and a LIST shorter than its type is dropped");
        assert_eq!(ids(&output), ["fmt ", "data", "PLTN"]);
    }

    #[test]
    fn a_final_odd_chunk_without_its_pad_byte_is_kept_and_padded() {
        let mut id3 = chunk(b"ID3 ", b"seven b");
        assert_eq!(id3.len() % 2, 0);
        id3.pop(); // the pad byte TagLib normally writes
        let file = stale_wav(&base(), &id3);
        let (output, kept) = rewrite(&file, true);
        assert!(kept);
        assert_eq!(ids(&output), ["fmt ", "data", "ID3 ", "PLTN"]);
    }

    #[test]
    fn more_than_a_handful_of_trailing_chunks_is_not_what_taglib_writes() {
        let many: Vec<u8> = (0..MAX_TRAILING_CHUNKS + 1).flat_map(|_| chunk(b"JUNK", b"")).collect();
        let file = stale_wav(&base(), &many);
        let (output, kept) = rewrite(&file, true);
        assert!(!kept);
        assert_eq!(ids(&output), ["fmt ", "data", "PLTN"]);
    }

    #[test]
    fn the_scope_restores_the_previous_state_and_reports_only_inside_itself() {
        let file = stale_wav(&base(), &chunk(b"ID3 ", b"tag"));
        {
            let outer = scope(true);
            {
                let inner = scope(false);
                let mut sink = Vec::new();
                rewrite_wav_marker(&mut Cursor::new(&file), &mut sink, None).unwrap();
                assert!(!inner.kept());
            }
            let mut sink = Vec::new();
            rewrite_wav_marker(&mut Cursor::new(&file), &mut sink, None).unwrap();
            assert!(outer.kept());
        }
        let mut sink = Vec::new();
        rewrite_wav_marker(&mut Cursor::new(&file), &mut sink, None).unwrap();
        assert_eq!(ids(&sink), ["fmt ", "data", "PLTN"], "outside every scope the reference policy applies");
    }
}
