//! Opt-in WAV tag preservation (`parity.allow_non_identical`; Linux and `portable` builds).
//!
//! The reference marker writer rewrites a WAV from inside the tag bridge's save, while TagLib's
//! stream is still open. TagLib appends a chunk and updates the RIFF size last, through a buffered
//! write, so at that moment the file holds the chunk but its header still declares the old length.
//! The reference parser stops at the declared end and drops the bytes after it, so the chunk
//! TagLib saved last (the `ID3 ` chunk, or the INFO `LIST` when both exist) never reaches the
//! output. Native Platinum Notes behaves the same, so the default policy keeps doing it exactly.
//!
//! With the opt-in the rewrite instead trusts the physical chunk sequence when the chunks of the head tile it exactly up to the declared end and the bytes
//! after the declared end are nothing but strictly valid chunks reaching exactly the end of the file,
//! and keeps only the ones that carry source data. TagLib writes `TBPM` and `TKEY` frames on every
//! cross-format copy (a zero tempo and an empty key for a source without them), so an `ID3 ` chunk
//! holding nothing but those two empty frames, and an INFO `LIST` without a value, are dropped;
//! any other chunk, and any tag chunk this module cannot prove empty, is kept byte for byte. The
//! same walker as the default policy then runs over a segmented view of the file (the head up to
//! the declared end plus the physical ranges of the kept chunks) whose RIFF size is corrected, so
//! every other rule (PLTN replaced, short LIST dropped, padding normalized, invalid headers stop
//! the walk) is unchanged. When nothing is kept the default rewrite runs on the file itself, so the
//! output is byte-identical to the default policy. Anything else after the declared end is dropped
//! as before.

use crate::metadata::{self, MarkerError};
use std::cell::Cell;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

/// The `non_identical_paths` member for an output that kept chunks the reference drops.
pub const NON_IDENTICAL_WAV_TAG_PRESERVATION: &str = "wav_tag_preservation";

/// TagLib appends one or two chunks; more than this after the declared end is not its work.
const MAX_TRAILING_CHUNKS: usize = 64;
/// TagLib writes two frames (`TBPM`, `TKEY`) into a tag that holds nothing; more than this is not an empty tag.
const MAX_ID3_FRAMES: usize = 32;
/// A `TBPM`/`TKEY` text that is empty or `0` is a few bytes in any encoding; a longer frame carries something.
const MAX_TEXT_FRAME: usize = 16;
/// INFO sub-chunks inspected before the list counts as data (an empty list has none, TagLib skips empty fields).
const MAX_INFO_ENTRIES: usize = 64;

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

/// `metadata::rewrite_wav_marker`, keeping the trailing chunks TagLib wrote that carry source data when the scope is on.
pub(crate) fn rewrite_wav_marker<R: Read + Seek, W: Write>(
    input: &mut R, output: &mut W, payload: Option<&[u8]>,
) -> Result<(), MarkerError> {
    if !ACTIVE.get() { return metadata::rewrite_wav_marker(input, output, payload); }
    let Some(trailing) = trailing_chunks(input)? else { return metadata::rewrite_wav_marker(input, output, payload); };
    // Nothing after the declared end carries source data: the output is the default policy's, byte for byte.
    if trailing.kept.is_empty() { return metadata::rewrite_wav_marker(input, output, payload); }
    let mut view = SegmentedView::new(input, &trailing);
    metadata::rewrite_wav_marker(&mut view, output, payload)?;
    // Only a walk that read past the declared end can have copied a kept chunk (a damaged head stops it earlier).
    if view.reached_tail { KEPT.set(true); }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Trailing {
    /// Where the RIFF declares its end; the head of the file is `0..declared_end`.
    declared_end: u64,
    /// The RIFF size of the view: the head plus the kept chunks.
    riff_size: u32,
    /// Physical ranges (the pad byte included when the file has it) of the trailing chunks that carry source data, in file order.
    kept: Vec<Range<u64>>,
}

fn io_error(operation: &'static str, source: io::Error) -> MarkerError { MarkerError::Io { operation, source } }

fn is_chunk_id(id: &[u8]) -> bool { id.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b' ') }

/// `Some` only when the file is `RIFF....WAVE`, declares an end before the end of the file, the chunks of the head (up to the declared end) tile it
/// exactly, and the bytes after it are a bounded run of well-formed chunks that ends exactly at the end of the file. Reports the
/// ranges of the chunks of that run that carry source data.
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
    if !head_tiles_exactly(input, declared_end)? { return Ok(None); }

    let mut position = declared_end;
    let mut kept = Vec::new();
    for _ in 0..MAX_TRAILING_CHUNKS {
        if position == file_length { return Ok(trailing(declared_end, kept)); }
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
        let end = body + padded.min(available);
        if carries_data(input, &chunk[..4], body, length)? { kept.push(position..end); }
        position = end;
    }
    Ok(if position == file_length { trailing(declared_end, kept) } else { None })
}

/// Whether the chunks of the head (`12..declared_end`) tile it exactly: every header well formed, every chunk inside the head, the last one (pad byte
/// included) ending at `declared_end`. The default walk stops at a head chunk that crosses the declared end; the segmented view extends the walk's end past
/// it, so a file whose head does not tile must not get a view (its `data` chunk could then swallow the chunks after the declared end).
fn head_tiles_exactly<R: Read + Seek>(input: &mut R, declared_end: u64) -> Result<bool, MarkerError> {
    let mut position = 12u64;
    while position < declared_end {
        if declared_end - position < 8 { return Ok(false); }
        input.seek(SeekFrom::Start(position)).map_err(|e| io_error("seek WAV chunk", e))?;
        let mut chunk = [0u8; 8];
        input.read_exact(&mut chunk).map_err(|e| io_error("read WAV chunk header", e))?;
        if !is_chunk_id(&chunk[..4]) { return Ok(false); }
        let length = u64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
        let body = position + 8;
        if length > declared_end - body { return Ok(false); }
        position = body + length + (length & 1);
    }
    Ok(position == declared_end)
}

fn trailing(declared_end: u64, kept: Vec<Range<u64>>) -> Option<Trailing> {
    let total = declared_end + kept.iter().map(|range| range.end - range.start).sum::<u64>();
    Some(Trailing { declared_end, riff_size: u32::try_from(total - 8).ok()?, kept })
}

/// Whether the rewrite keeps the trailing chunk `id`, whose body is `length` bytes at `body`. Every chunk is kept
/// except the ones the reference rewrite drops itself (a `PLTN`, a `LIST` shorter than its type) and the tag chunks
/// TagLib wrote that provably hold no source data: an ID3v2 tag with nothing but an empty tempo and key, an INFO
/// list without a value. A tag chunk that cannot be parsed, or that ends early, is kept.
fn carries_data<R: Read + Seek>(input: &mut R, id: &[u8], body: u64, length: u64) -> Result<bool, MarkerError> {
    let list = match id {
        b"PLTN" => return Ok(false),
        b"LIST" if length < 4 => return Ok(false),
        b"LIST" => true,
        b"ID3 " | b"id3 " => false,
        _ => return Ok(true),
    };
    input.seek(SeekFrom::Start(body)).map_err(|e| io_error("seek WAV tag chunk", e))?;
    let mut region = Region { input, left: length };
    match if list { info_holds_nothing(&mut region) } else { id3_holds_nothing(&mut region) } {
        Ok(empty) => Ok(!empty),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(true),
        Err(error) => Err(io_error("read WAV tag chunk", error)),
    }
}

/// The next `left` bytes of `input`, read front to back; reading past them is an `UnexpectedEof`.
struct Region<'a, R> { input: &'a mut R, left: u64 }

impl<R: Read> Region<'_, R> {
    fn fill(&mut self, buffer: &mut [u8]) -> io::Result<()> {
        let count = buffer.len() as u64;
        if count > self.left { return Err(io::ErrorKind::UnexpectedEof.into()); }
        self.input.read_exact(buffer)?;
        self.left -= count;
        Ok(())
    }

    /// Whether the next `count` bytes are all zero.
    fn zeros(&mut self, count: u64) -> io::Result<bool> {
        let mut buffer = [0u8; 4096];
        let mut remaining = count;
        while remaining != 0 {
            let step = &mut buffer[..remaining.min(4096) as usize];
            self.fill(step)?;
            if step.iter().any(|byte| *byte != 0) { return Ok(false); }
            remaining -= step.len() as u64;
        }
        Ok(true)
    }

    fn skip(&mut self, count: u64) -> io::Result<()> {
        let mut buffer = [0u8; 4096];
        let mut remaining = count;
        while remaining != 0 {
            let step = &mut buffer[..remaining.min(4096) as usize];
            self.fill(step)?;
            remaining -= step.len() as u64;
        }
        Ok(())
    }
}

fn syncsafe(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0u64, |value, byte| if byte & 0x80 == 0 { Some(value << 7 | u64::from(*byte)) } else { None })
}

/// What a text frame holds: nothing, the decimal `0`, or something else.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Text { Empty, Zero, Other }

/// The text of an ID3v2 text frame body (encoding byte, then the text) as far as this module needs it. `None` for
/// anything that is not a plain string of the tag's version: no encoding byte, an encoding the version lacks, an
/// odd UTF-16 length, UTF-16 without a byte-order mark.
fn frame_text(version: u8, body: &[u8]) -> Option<Text> {
    let (&encoding, text) = body.split_first()?;
    match (encoding, version) {
        (0, _) | (3, 4) => {
            let end = text.iter().rposition(|byte| *byte != 0).map_or(0, |last| last + 1);
            Some(match &text[..end] { [] => Text::Empty, [b'0'] => Text::Zero, _ => Text::Other })
        }
        (1, _) => wide_text(text, None),
        (2, 4) => wide_text(text, Some(true)),
        _ => None,
    }
}

/// UTF-16 text: with a byte-order mark (`fixed_big_endian` is `None`) or big-endian without one. Terminating
/// NUL units are not part of the text.
fn wide_text(text: &[u8], fixed_big_endian: Option<bool>) -> Option<Text> {
    if text.len() & 1 != 0 { return None; }
    let (big_endian, units) = match (fixed_big_endian, text) {
        (Some(big_endian), _) => (big_endian, text),
        (None, []) => return Some(Text::Empty),
        (None, [0xff, 0xfe, rest @ ..]) => (false, rest),
        (None, [0xfe, 0xff, rest @ ..]) => (true, rest),
        (None, _) => return None,
    };
    let unit = |index: usize| {
        let pair = [units[2 * index], units[2 * index + 1]];
        if big_endian { u16::from_be_bytes(pair) } else { u16::from_le_bytes(pair) }
    };
    let end = (0..units.len() / 2).rev().find(|index| unit(*index) != 0).map_or(0, |last| last + 1);
    Some(match end { 0 => Text::Empty, 1 if unit(0) == u16::from(b'0') => Text::Zero, _ => Text::Other })
}

/// Whether the chunk body is an ID3v2.2, 2.3 or 2.4 tag that holds nothing but `TBPM` frames whose text is empty or
/// `0` and `TKEY` frames whose text is empty, followed by zero padding. Anything else (another frame, a frame flag,
/// a header flag, a size that does not fit, bytes after the padding that are not zero) is not provably empty.
fn id3_holds_nothing<R: Read>(tag: &mut Region<'_, R>) -> io::Result<bool> {
    let mut header = [0u8; 10];
    tag.fill(&mut header)?;
    let version = header[3];
    if &header[..3] != b"ID3" || !(2..=4).contains(&version) || header[5] != 0 { return Ok(false); }
    let Some(size) = syncsafe(&header[6..]) else { return Ok(false); };
    if size > tag.left { return Ok(false); }
    // Whatever the chunk holds after the tag must be zero as well.
    let after = tag.left - size;
    let width = if version == 2 { 6 } else { 10 };
    let (mut left, mut frames) = (size, 0);
    loop {
        if left < width as u64 { return tag.zeros(left + after); }
        let mut frame = [0u8; 10];
        tag.fill(&mut frame[..width])?;
        left -= width as u64;
        if frame[0] == 0 { return Ok(frame[..width].iter().all(|byte| *byte == 0) && tag.zeros(left + after)?); }
        frames += 1;
        if frames > MAX_ID3_FRAMES { return Ok(false); }
        let (id, length, plain) = match version {
            2 => (&frame[..3], Some(u64::from(frame[3]) << 16 | u64::from(frame[4]) << 8 | u64::from(frame[5])), true),
            3 => (&frame[..4], Some(u64::from(u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]))), frame[8..10] == [0, 0]),
            _ => (&frame[..4], syncsafe(&frame[4..8]), frame[8..10] == [0, 0]),
        };
        let Some(length) = length.filter(|_| plain) else { return Ok(false); };
        let tempo = match (version, id) {
            (2, b"TBP") | (3 | 4, b"TBPM") => true,
            (2, b"TKE") | (3 | 4, b"TKEY") => false,
            _ => return Ok(false),
        };
        if length == 0 || length > MAX_TEXT_FRAME as u64 || length > left { return Ok(false); }
        let mut text = [0u8; MAX_TEXT_FRAME];
        let text = &mut text[..length as usize];
        tag.fill(text)?;
        left -= length;
        match frame_text(version, text) {
            Some(Text::Empty) => {}
            Some(Text::Zero) if tempo => {}
            _ => return Ok(false),
        }
    }
}

/// Whether the chunk body is an INFO list in which no sub-chunk has a value (a value of nothing but NUL bytes is
/// empty). A list of another type, a damaged sub-chunk, or more entries than are inspected is not provably empty.
fn info_holds_nothing<R: Read>(list: &mut Region<'_, R>) -> io::Result<bool> {
    let mut form = [0u8; 4];
    list.fill(&mut form)?;
    if &form != b"INFO" { return Ok(false); }
    let mut entries = 0;
    loop {
        if list.left == 0 { return Ok(true); }
        if list.left < 8 { return list.zeros(list.left); }
        let mut entry = [0u8; 8];
        list.fill(&mut entry)?;
        let length = u64::from(u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]));
        if !is_chunk_id(&entry[..4]) || length > list.left {
            // Not a sub-chunk: only zero padding may be left.
            return Ok(entry.iter().all(|byte| *byte == 0) && list.zeros(list.left)?);
        }
        entries += 1;
        if entries > MAX_INFO_ENTRIES { return Ok(false); }
        if !list.zeros(length)? { return Ok(false); }
        // The pad byte of an odd value; the last entry may lack it.
        if length & 1 != 0 && list.left != 0 { list.skip(1)?; }
    }
}

/// One physical byte range of the file, `length` bytes long, that starts at `logical` in the view.
struct Segment { physical: u64, length: u64, logical: u64 }

/// The file as the default rewrite should see it: the head up to the declared end followed by the kept chunks, with the
/// RIFF size field (bytes 4..8) reading as the size of that whole. The dropped chunks are not in the view at all and
/// no chunk is copied into memory.
struct SegmentedView<'a, R> {
    inner: &'a mut R,
    segments: Vec<Segment>,
    declared_end: u64,
    riff_size: [u8; 4],
    total: u64,
    position: u64,
    /// Where `inner` is positioned, when known, so sequential reads need no seek.
    physical: Option<u64>,
    /// A read returned a byte at or past the declared end: the walk reached the kept chunks.
    reached_tail: bool,
}

impl<'a, R> SegmentedView<'a, R> {
    fn new(inner: &'a mut R, trailing: &Trailing) -> Self {
        let mut segments = Vec::with_capacity(1 + trailing.kept.len());
        segments.push(Segment { physical: 0, length: trailing.declared_end, logical: 0 });
        let mut total = trailing.declared_end;
        for range in &trailing.kept {
            let length = range.end - range.start;
            segments.push(Segment { physical: range.start, length, logical: total });
            total += length;
        }
        Self { inner, segments, declared_end: trailing.declared_end, riff_size: trailing.riff_size.to_le_bytes(), total, position: 0, physical: None, reached_tail: false }
    }
}

impl<R: Read + Seek> Read for SegmentedView<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.position >= self.total { return Ok(0); }
        let position = self.position;
        let Some(segment) = self.segments.iter().find(|segment| position < segment.logical + segment.length) else { return Ok(0); };
        let offset = position - segment.logical;
        let physical = segment.physical + offset;
        let wanted = usize::try_from(segment.length - offset).map_or(buffer.len(), |left| left.min(buffer.len()));
        if self.physical != Some(physical) {
            self.physical = None;
            self.inner.seek(SeekFrom::Start(physical))?;
        }
        let count = match self.inner.read(&mut buffer[..wanted]) {
            Ok(count) => count,
            Err(error) => { self.physical = None; return Err(error); }
        };
        self.physical = Some(physical + count as u64);
        if position < 8 {
            for (index, byte) in buffer[..count].iter_mut().enumerate() {
                let at = position + index as u64;
                if (4..8).contains(&at) { *byte = self.riff_size[(at - 4) as usize]; }
            }
        }
        if position + count as u64 > self.declared_end { self.reached_tail = true; }
        self.position += count as u64;
        Ok(count)
    }
}

impl<R> Seek for SegmentedView<'_, R> {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let (base, delta) = match target {
            SeekFrom::Start(position) => { self.position = position; return Ok(position); }
            SeekFrom::End(delta) => (self.total, delta),
            SeekFrom::Current(delta) => (self.position, delta),
        };
        self.position = base.checked_add_signed(delta)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the WAV"))?;
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

    /// A head chunk whose length crosses the declared end and whose payload there parses as chunks to the end of the file must not make the opt-in
    /// copy it: the default walk rejects the crossing chunk, so the opt-in's output stays the default policy's.
    #[test]
    fn a_head_chunk_crossing_the_declared_end_gets_the_default_output() {
        let id3 = chunk(b"ID3 ", b"ID3 tag bytes!");
        let list = chunk(b"LIST", b"INFOINAM\x04\0\0\0abc\0");
        let trailing = [id3.clone(), list].concat();
        let fmt = base().remove(0);
        let crossing = |extra: u32| {
            let mut data = b"data".to_vec();
            data.extend_from_slice(&(8 + extra).to_le_bytes());
            data.extend_from_slice(&[0u8; 8]);
            stale_wav(&[fmt.clone(), data], &trailing)
        };
        // the `data` length reaches exactly the end of the file, or only into the middle of the first trailing chunk
        for file in [crossing(trailing.len() as u32), crossing(id3.len() as u32 - 2), crossing(trailing.len() as u32 + 100)] {
            let (on, kept) = rewrite(&file, true);
            assert!(!kept, "a crossing head chunk is not a tail the opt-in keeps");
            assert_eq!(on, metadata_rewrite(&file), "the opt-in must give the default policy's bytes");
            assert_eq!(on, rewrite(&file, false).0);
        }
        // the unmodified chunks still tile: the same trailing chunks are kept when the head is well formed
        let (kept_output, kept) = rewrite(&stale_wav(&base(), &trailing), true);
        assert!(kept);
        assert_eq!(ids(&kept_output), ["fmt ", "data", "ID3 ", "LIST", "PLTN"]);
    }

    /// Head chunks that do not end exactly at the declared end (slack bytes, a truncated last header) get the default output too.
    #[test]
    fn a_head_that_does_not_tile_the_declared_end_gets_the_default_output() {
        let id3 = chunk(b"ID3 ", b"ID3 tag bytes!");
        for slack in [&[0u8; 2][..], &[0u8; 6][..]] {
            let mut head = base();
            head.push(slack.to_vec());
            let file = stale_wav(&head, &id3);
            let (on, kept) = rewrite(&file, true);
            assert!(!kept);
            assert_eq!(on, metadata_rewrite(&file));
        }
    }

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

    // --- Only the tag chunks that carry source data are kept ---------------------------------------------------

    fn syncsafe_bytes(size: usize) -> [u8; 4] {
        [(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]
    }

    /// An ID3v2.`version` (2, 3 or 4) frame without flags.
    fn id3_frame(version: u8, id: &str, body: &[u8]) -> Vec<u8> {
        let mut frame = id.as_bytes().to_vec();
        match version {
            2 => frame.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]),
            3 => { frame.extend_from_slice(&(body.len() as u32).to_be_bytes()); frame.extend_from_slice(&[0, 0]); }
            _ => { frame.extend_from_slice(&syncsafe_bytes(body.len())); frame.extend_from_slice(&[0, 0]); }
        }
        frame.extend_from_slice(body);
        frame
    }

    /// An ID3v2 tag with the frames and `padding` zero bytes counted in its size, as TagLib lays one out.
    fn id3_tag(version: u8, frames: &[Vec<u8>], padding: usize) -> Vec<u8> {
        let mut body = frames.concat();
        body.resize(body.len() + padding, 0);
        let mut tag = vec![b'I', b'D', b'3', version, 0, 0];
        tag.extend_from_slice(&syncsafe_bytes(body.len()));
        tag.extend(body);
        tag
    }

    fn tempo_id(version: u8) -> &'static str { if version == 2 { "TBP" } else { "TBPM" } }
    fn key_id(version: u8) -> &'static str { if version == 2 { "TKE" } else { "TKEY" } }
    fn title_id(version: u8) -> &'static str { if version == 2 { "TT2" } else { "TIT2" } }
    fn latin1(text: &str) -> Vec<u8> { [&[0u8][..], text.as_bytes()].concat() }
    fn contains(haystack: &[u8], needle: &[u8]) -> bool { haystack.windows(needle.len()).any(|window| window == needle) }

    /// The `ID3 ` chunk TagLib 1.11 writes for a cross-format copy (captured from the bridge in the Linux image): a
    /// v2.4 tag with `TKEY`, then `TBPM`, then 1,024 bytes of padding, which makes the chunk body 1,057 bytes plus a pad byte.
    fn taglib_chunk(tempo: &str, key: &str) -> Vec<u8> {
        chunk(b"ID3 ", &id3_tag(4, &[id3_frame(4, "TKEY", &latin1(key)), id3_frame(4, "TBPM", &latin1(tempo))], 1024))
    }

    fn data_id3() -> Vec<u8> {
        chunk(b"ID3 ", &id3_tag(4, &[id3_frame(4, "TKEY", &latin1("")), id3_frame(4, "TBPM", &latin1("0")), id3_frame(4, "TIT2", &latin1("Title"))], 1024))
    }

    fn info(entries: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = b"INFO".to_vec();
        for (id, value) in entries { body.extend(chunk(id, value)); }
        body
    }

    /// A WAV in the chunk layout of `output`: every chunk with its range, asserting that the RIFF size matches the file
    /// and that the chunks (with their pad bytes) tile the whole body.
    fn well_formed(wav: &[u8]) -> Vec<(String, Range<usize>)> {
        assert!(wav.len() >= 12 && &wav[..4] == b"RIFF" && &wav[8..12] == b"WAVE", "RIFF/WAVE header");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize + 8, wav.len(), "the RIFF size matches the file");
        let (mut position, mut chunks) = (12, Vec::new());
        while position < wav.len() {
            assert!(position + 8 <= wav.len(), "a chunk header fits");
            let id = &wav[position..position + 4];
            assert!(is_chunk_id(id), "a valid chunk identifier");
            let length = u32::from_le_bytes(wav[position + 4..position + 8].try_into().unwrap()) as usize;
            assert!(position + 8 + length <= wav.len(), "a chunk body fits");
            chunks.push((String::from_utf8_lossy(id).into_owned(), position..position + 8 + length));
            position += 8 + length + (length & 1);
        }
        assert_eq!(position, wav.len(), "the chunks end exactly at the end of the file");
        chunks
    }

    /// The opt-in policy over `trailing` (whole chunks as they sit after the declared end), checked against its
    /// definition: the output is the default rewrite of a consistent file whose chunks are the head and the kept
    /// chunks only, each dropped chunk is physically absent, and nothing kept is the default policy's output and no report.
    fn assert_policy(label: &str, trailing: &[Vec<u8>], kept: &[usize]) -> Vec<u8> {
        let file = stale_wav(&base(), &trailing.concat());
        let (output, reported) = rewrite(&file, true);
        let consistent = [base(), kept.iter().map(|index| trailing[*index].clone()).collect()].concat();
        assert_eq!(output, metadata_rewrite(&stale_wav(&consistent, &[])), "{label}");
        assert_eq!(reported, !kept.is_empty(), "{label}");
        if kept.is_empty() { assert_eq!(output, metadata_rewrite(&file), "{label}: nothing kept is the default policy's output"); }
        for (index, piece) in trailing.iter().enumerate() {
            // The reference rewrite replaces a PLTN with its own; every other chunk is in the output exactly when it is kept.
            if &piece[..4] == b"PLTN" { continue; }
            let written = &piece[..8 + u32::from_le_bytes(piece[4..8].try_into().unwrap()) as usize];
            let wanted = kept.iter().any(|other| trailing[*other] == *piece);
            assert_eq!(contains(&output, written), wanted, "{label}: chunk {index}");
        }
        let ids = ids(&output);
        let expected: Vec<String> = ["fmt ", "data"].iter().map(|id| id.to_string())
            .chain(kept.iter().map(|index| String::from_utf8_lossy(&trailing[*index][..4]).into_owned())).chain(["PLTN".to_owned()]).collect();
        assert_eq!(ids, expected, "{label}");
        output
    }

    #[test]
    fn the_chunk_taglib_writes_for_a_source_without_tempo_or_key_is_dropped() {
        let chunk = taglib_chunk("0", "");
        assert_eq!(&chunk[..18], [0x49, 0x44, 0x33, 0x20, 0x21, 0x04, 0, 0, 0x49, 0x44, 0x33, 0x04, 0, 0, 0, 0, 0x08, 0x17], "the captured header");
        assert_eq!(&chunk[18..41], [0x54, 0x4b, 0x45, 0x59, 0, 0, 0, 1, 0, 0, 0, 0x54, 0x42, 0x50, 0x4d, 0, 0, 0, 2, 0, 0, 0, 0x30], "the captured frames");
        assert_eq!(chunk.len(), 8 + 1057 + 1);
        assert_policy("TagLib's chunk for no tempo and no key", &[chunk], &[]);
        // The same layout with a tempo or a key is the source's data.
        for (tempo, key) in [("128", ""), ("0", "Am"), ("128", "Am"), ("1", ""), ("0", " ")] {
            assert_policy(&format!("tempo {tempo:?}, key {key:?}"), &[taglib_chunk(tempo, key)], &[0]);
        }
    }

    #[test]
    fn an_id3_chunk_holding_only_an_empty_tempo_and_key_is_dropped_in_every_encoding_and_version() {
        // Bodies whose text is empty or `0` (the tempo may be `0`, the key must be empty); `(encoding, bodies)`.
        let zero_or_empty: &[&[u8]] = &[
            &[0], &[0, b'0'], &[0, b'0', 0], &[0, 0], &[0, 0, 0],
            &[1], &[1, 0xff, 0xfe], &[1, 0xfe, 0xff], &[1, 0xff, 0xfe, 0x30, 0], &[1, 0xfe, 0xff, 0, 0x30], &[1, 0xff, 0xfe, 0x30, 0, 0, 0],
            &[1, 0xff, 0xfe, 0, 0], &[1, 0xfe, 0xff, 0, 0, 0, 0],
            &[3], &[3, b'0'], &[3, b'0', 0], &[3, 0], &[2], &[2, 0, 0x30], &[2, 0, 0x30, 0, 0], &[2, 0, 0],
        ];
        let empty_only: &[&[u8]] = &[
            &[0], &[0, 0], &[0, 0, 0], &[1], &[1, 0xff, 0xfe], &[1, 0xfe, 0xff], &[1, 0xff, 0xfe, 0, 0], &[1, 0xfe, 0xff, 0, 0, 0, 0],
            &[3], &[3, 0], &[2], &[2, 0, 0],
        ];
        // Encodings 2 and 3 exist from ID3v2.4 on only.
        let encodable = |version: u8, body: &[u8]| version == 4 || body[0] < 2;
        let mut cases = 0;
        for version in [2u8, 3, 4] {
            for id in [b"ID3 ", b"id3 "] {
                for tempo_body in zero_or_empty.iter().filter(|body| encodable(version, body)) {
                    for key_body in empty_only.iter().filter(|body| encodable(version, body)) {
                        let tempo = id3_frame(version, tempo_id(version), tempo_body);
                        let key = id3_frame(version, key_id(version), key_body);
                        let orders = [vec![tempo.clone(), key.clone()], vec![key.clone(), tempo.clone()], vec![tempo.clone()], vec![key.clone()],
                            vec![tempo.clone(), tempo.clone(), key.clone(), key.clone()], Vec::new()];
                        for frames in orders {
                            for padding in [0, 1, 7, 1024] {
                                let label = format!("v2.{version} {} tempo {tempo_body:02x?} key {key_body:02x?} frames {} padding {padding}", String::from_utf8_lossy(id), frames.len());
                                assert_policy(&label, &[chunk(id, &id3_tag(version, &frames, padding))], &[]);
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(cases > 5000, "{cases}");
    }

    #[test]
    fn zero_bytes_after_the_tag_and_inside_the_padding_do_not_make_it_carry_data() {
        for version in [3u8, 4] {
            let frames = [id3_frame(version, tempo_id(version), &latin1("0")), id3_frame(version, key_id(version), &latin1(""))];
            let mut tag = id3_tag(version, &frames, 5);
            tag.extend_from_slice(&[0; 9]);
            assert_policy("zero bytes after the tag", &[chunk(b"ID3 ", &tag)], &[]);
            let mut odd_header_gap = id3_tag(version, &frames, 0);
            odd_header_gap.extend_from_slice(&[0; 3]);
            assert_policy("zero bytes after a tag without padding", &[chunk(b"ID3 ", &odd_header_gap)], &[]);
        }
        // 32 frames is the most an empty tag may hold.
        let frames = |count: usize| (0..count).map(|_| id3_frame(4, "TBPM", &latin1(""))).collect::<Vec<_>>();
        assert_policy("32 empty frames", &[chunk(b"ID3 ", &id3_tag(4, &frames(32), 0))], &[]);
        assert_policy("33 empty frames", &[chunk(b"ID3 ", &id3_tag(4, &frames(33), 0))], &[0]);
    }

    #[test]
    fn a_tag_that_carries_source_data_or_cannot_be_parsed_is_kept_byte_for_byte() {
        let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
        let utf16 = |text: &str| [&[1u8, 0xff, 0xfe][..], &text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>()[..]].concat();
        for version in [2u8, 3, 4] {
            let tag = |frames: &[Vec<u8>]| id3_tag(version, frames, 16);
            let tempo = |body: &[u8]| id3_frame(version, tempo_id(version), body);
            let key = |body: &[u8]| id3_frame(version, key_id(version), body);
            let title = id3_frame(version, title_id(version), &latin1("Title"));
            let other = |id: &str, body: &[u8]| id3_frame(version, id, body);
            let mut add = |label: &str, body: Vec<u8>| cases.push((format!("v2.{version} {label}"), body));
            add("a title", tag(&[title.clone()]));
            add("a title after the empty frames", tag(&[tempo(&latin1("0")), key(&latin1("")), title.clone()]));
            add("a title before the empty frames", tag(&[title.clone(), tempo(&latin1("0")), key(&latin1(""))]));
            add("a comment", tag(&[other(if version == 2 { "COM" } else { "COMM" }, b"\0eng\0note")]));
            add("a picture", tag(&[other(if version == 2 { "PIC" } else { "APIC" }, &[&[0u8][..], b"PNG\0\x03\0", &[0x89, 0x50, 0x4e, 0x47]].concat())]));
            add("a user text frame", tag(&[other(if version == 2 { "TXX" } else { "TXXX" }, b"\0label\0value")]));
            add("an unknown frame", tag(&[other(if version == 2 { "XAB" } else { "XABC" }, b"\0private")]));
            add("an empty unknown frame", tag(&[other(if version == 2 { "XAB" } else { "XABC" }, &[])]));
            for value in ["128", "1", "00", "0 ", " 0", "-0", "0.0", "0\u{0}0", "٠"] {
                add(&format!("tempo {value:?}"), tag(&[tempo(&latin1(value)), key(&latin1(""))]));
            }
            for value in ["Am", " ", "0", "A\u{0}B"] {
                add(&format!("key {value:?}"), tag(&[tempo(&latin1("0")), key(&latin1(value))]));
            }
            add("a UTF-16 tempo of 128", tag(&[tempo(&utf16("128")), key(&latin1(""))]));
            add("a UTF-16 tempo of 00", tag(&[tempo(&utf16("00"))]));
            add("a UTF-16 key", tag(&[tempo(&latin1("0")), key(&utf16("Am"))]));
            add("a key after a NUL", tag(&[key(&[0, 0, b'A'])]));
            add("a tempo of two strings", tag(&[tempo(&[0, b'0', 0, b'0'])]));
            add("UTF-16 without a byte-order mark", tag(&[tempo(&[1, 0x30, 0])]));
            add("UTF-16 of an odd length", tag(&[tempo(&[1, 0xff, 0xfe, 0x30])]));
            add("UTF-16 with a second byte-order mark", tag(&[key(&[1, 0xff, 0xfe, 0, 0, 0xff, 0xfe])]));
            add("an unknown encoding", tag(&[tempo(&[4, b'0'])]));
            add("a frame without an encoding byte", tag(&[tempo(&[])]));
            add("a tempo longer than a zero can be", tag(&[tempo(&[&[0u8, b'0'][..], &[0u8; MAX_TEXT_FRAME][..]].concat())]));
            add("a lowercase frame identifier", tag(&[id3_frame(version, &tempo_id(version).to_ascii_lowercase(), &latin1("0"))]));
            add("a frame longer than the tag", { let mut frame = tempo(&latin1("0")); if version == 2 { frame[3..6].copy_from_slice(&[0, 0xff, 0xff]); } else { frame[4..8].copy_from_slice(&[0, 0, 0x7f, 0x7f]); } tag(&[frame]) });
            add("a frame with a huge size", { let mut frame = tempo(&latin1("0")); if version == 2 { frame[3..6].copy_from_slice(&[0xff, 0xff, 0xff]); } else { frame[4..8].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]); } tag(&[frame]) });
            add("a frame header cut off by the end of the tag", { let mut frames = tempo(&latin1("0")); frames.extend_from_slice(b"TBP"); id3_tag(version, &[frames], 0) });
            add("a padding byte that is not zero", { let mut tag = id3_tag(version, &[tempo(&latin1("0"))], 12); let last = tag.len() - 1; tag[last] = 1; tag });
            add("a padding that starts with a header of mostly zeros", { let mut frames = tempo(&latin1("0")); frames.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 7]); id3_tag(version, &[frames], 0) });
            add("bytes after the tag that are not zero", { let mut tag = id3_tag(version, &[tempo(&latin1("0"))], 3); tag.extend_from_slice(&[0, 0, 5]); tag });
            add("a stray byte after a tag without padding", { let mut tag = id3_tag(version, &[tempo(&latin1("0"))], 0); tag.push(0x41); tag });
            add("33 frames", tag(&(0..33).map(|_| tempo(&latin1(""))).collect::<Vec<_>>()));
            if version >= 3 {
                add("a frame with flags", { let mut frame = tempo(&latin1("0")); frame[9] = 0x01; tag(&[frame]) });
                add("a frame with a status flag", { let mut frame = tempo(&latin1("0")); frame[8] = 0x40; tag(&[frame]) });
            }
            if version < 4 { add("UTF-8 where the version has none", tag(&[tempo(&[3, b'0'])])); add("UTF-16BE where the version has none", tag(&[tempo(&[2, 0, 0x30])])); }
            if version == 4 { add("UTF-16BE of an odd length", tag(&[tempo(&[2, 0x30])])); }
            if version == 4 {
                add("a frame size that is not synchsafe", { let mut frame = tempo(&latin1("0")); frame[4..8].copy_from_slice(&[0, 0, 0, 0x82]); tag(&[frame]) });
            }
        }
        // Header-level damage, whatever the frames.
        let clean = id3_tag(4, &[id3_frame(4, "TBPM", &latin1("0"))], 8);
        let mut header = |label: &str, change: &dyn Fn(&mut Vec<u8>)| { let mut tag = clean.clone(); change(&mut tag); cases.push((label.to_owned(), tag)); };
        header("a version the parser does not know (5)", &|tag| tag[3] = 5);
        header("version 1", &|tag| tag[3] = 1);
        header("version 0", &|tag| tag[3] = 0);
        header("another magic", &|tag| tag[2] = b'4');
        for flag in [0x80, 0x40, 0x20, 0x10, 0x01] { header(&format!("header flag {flag:#x}"), &move |tag| tag[5] = flag); }
        header("a size that is not synchsafe", &|tag| tag[9] |= 0x80);
        header("a size beyond the chunk", &|tag| tag[9] = tag[9].wrapping_add(20) & 0x7f);
        header("a size of 2^28 - 1", &|tag| tag[6..10].copy_from_slice(&[0x7f; 4]));
        header("a size that cuts the first frame", &|tag| tag[9] = 8);
        for length in 0..10 { header(&format!("a chunk of {length} bytes"), &move |tag| tag.truncate(length)); }
        header("a chunk that ends inside the frames", &|tag| tag.truncate(14));
        for (label, body) in &cases {
            for id in [b"ID3 ", b"id3 "] {
                let piece = chunk(id, body);
                let output = assert_policy(label, &[piece.clone()], &[0]);
                assert!(contains(&output, &piece[..8 + body.len()]), "{label}: the chunk is kept exactly as written");
            }
        }
        assert!(cases.len() > 130, "{}", cases.len());
    }

    #[test]
    fn an_info_list_is_dropped_when_no_entry_has_a_value() {
        let mut padded = info(&[(b"INAM", b"")]);
        padded.extend_from_slice(&[0; 3]);
        let mut zero_entry = info(&[]);
        zero_entry.extend_from_slice(&[0; 8]);
        let mut zero_tail = info(&[(b"IART", b"\0")]);
        zero_tail.extend_from_slice(&[0; 21]);
        let many = |count: usize| info(&(0..count).map(|_| (b"ICMT", &b""[..])).collect::<Vec<_>>());
        let empty: Vec<(&str, Vec<u8>)> = vec![
            ("no entries", info(&[])), ("an empty value", info(&[(b"INAM", b"")])), ("a NUL value", info(&[(b"INAM", b"\0")])),
            ("NUL values", info(&[(b"INAM", b"\0\0\0"), (b"IART", b""), (b"ICMT", b"\0\0")])),
            ("trailing zero bytes", padded), ("a zero entry", zero_entry), ("a zero tail", zero_tail), ("64 empty entries", many(64)),
        ];
        for (label, body) in empty { assert_policy(label, &[chunk(b"LIST", &body)], &[]); }
        // A LIST shorter than its type is dropped by the reference rewrite itself.
        assert_policy("a LIST shorter than its type", &[chunk(b"LIST", b"IN")], &[]);
        assert_policy("an empty LIST", &[chunk(b"LIST", b"")], &[]);
    }

    #[test]
    fn an_info_list_with_a_value_or_that_cannot_be_read_is_kept() {
        let mut late_value = info(&[(b"INAM", b""), (b"IART", b"\0\0\0")]);
        late_value.extend(chunk(b"ICMT", b"\0\0a"));
        let mut garbage_tail = info(&[(b"INAM", b"")]);
        garbage_tail.extend_from_slice(&[0, 0, 7]);
        let mut zero_tail_with_data = info(&[(b"INAM", b"")]);
        zero_tail_with_data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let mut oversized = info(&[(b"INAM", b"")]);
        oversized.extend_from_slice(b"IART\xff\xff\xff\x7f");
        let mut bad_id = info(&[]);
        bad_id.extend_from_slice(&[0xff, 0xfe, b'A', b'B', 0, 0, 0, 0]);
        let many = |count: usize| info(&(0..count).map(|_| (b"ICMT", &b""[..])).collect::<Vec<_>>());
        let mut other_type = b"adtl".to_vec();
        other_type.extend(chunk(b"labl", b"\x01\0\0\0cue\0"));
        let kept: Vec<(&str, Vec<u8>)> = vec![
            ("a title", info(&[(b"INAM", b"Title\0")])), ("a title after an empty entry", info(&[(b"IART", b""), (b"INAM", b"T")])),
            ("a one-byte value", info(&[(b"INAM", b"a")])), ("a space", info(&[(b"INAM", b" ")])),
            ("a value after NUL bytes", late_value), ("bytes after the entries that are not zero", garbage_tail),
            ("a zero tail that ends in data", zero_tail_with_data), ("a size beyond the list", oversized), ("an identifier that is not one", bad_id),
            ("65 empty entries", many(65)), ("a list of another type", other_type), ("a lowercase type", [b"info".to_vec(), chunk(b"INAM", b"")].concat()),
            ("a type only that is not INFO", b"adtl".to_vec()),
        ];
        for (label, body) in kept {
            let piece = chunk(b"LIST", &body);
            let output = assert_policy(label, &[piece.clone()], &[0]);
            assert!(contains(&output, &piece[..8 + body.len()]), "{label}: the list is kept exactly as written");
        }
        // Only the identifier `LIST` is an INFO list: any other chunk is kept as before.
        for id in [b"list", b"JUNK", b"bext", b"fact", b"smpl"] {
            assert_policy("another chunk", &[chunk(id, &info(&[(b"INAM", b"")]))], &[0]);
        }
    }

    #[test]
    fn runs_of_tag_chunks_keep_exactly_the_ones_that_carry_data_in_any_order() {
        let (empty_id3, data_id3, empty_list) = (taglib_chunk("0", ""), data_id3(), chunk(b"LIST", &info(&[(b"INAM", b"")])));
        let data_list = chunk(b"LIST", &info(&[(b"INAM", b"Title\0"), (b"IART", b"Artist\0")]));
        let junk = chunk(b"JUNK", b"abc");
        let old_marker = chunk(b"PLTN", b"old marker");
        let short_list = chunk(b"LIST", b"IN");
        let v3_empty = chunk(b"id3 ", &id3_tag(3, &[id3_frame(3, "TKEY", &latin1("")), id3_frame(3, "TBPM", &latin1("0"))], 17));
        let runs: Vec<(&str, Vec<Vec<u8>>, Vec<usize>)> = vec![
            ("[empty ID3, data LIST]", vec![empty_id3.clone(), data_list.clone()], vec![1]),
            ("[data ID3, empty LIST]", vec![data_id3.clone(), empty_list.clone()], vec![0]),
            ("[empty LIST, data ID3]", vec![empty_list.clone(), data_id3.clone()], vec![1]),
            ("[data LIST, empty ID3]", vec![data_list.clone(), empty_id3.clone()], vec![0]),
            ("[empty ID3, empty LIST]", vec![empty_id3.clone(), empty_list.clone()], vec![]),
            ("[empty LIST, empty ID3]", vec![empty_list.clone(), empty_id3.clone()], vec![]),
            ("[data ID3, data LIST]", vec![data_id3.clone(), data_list.clone()], vec![0, 1]),
            ("[empty ID3, JUNK, empty LIST]", vec![empty_id3.clone(), junk.clone(), empty_list.clone()], vec![1]),
            ("[data, empty, data]", vec![data_id3.clone(), empty_id3.clone(), data_list.clone()], vec![0, 2]),
            ("[empty, data, empty, data, empty]", vec![empty_id3.clone(), data_id3.clone(), empty_list.clone(), data_list.clone(), v3_empty.clone()], vec![1, 3]),
            ("[old PLTN, empty ID3, data LIST]", vec![old_marker.clone(), empty_id3.clone(), data_list.clone()], vec![2]),
            ("[data ID3, old PLTN, short LIST]", vec![data_id3.clone(), old_marker.clone(), short_list.clone()], vec![0]),
            ("[short LIST, old PLTN]", vec![short_list.clone(), old_marker.clone()], vec![]),
            ("[v2.3 empty, empty ID3]", vec![v3_empty.clone(), empty_id3.clone()], vec![]),
            ("[v2.3 empty, JUNK, v2.3 empty]", vec![v3_empty.clone(), junk.clone(), v3_empty.clone()], vec![1]),
            ("[JUNK, empty ID3]", vec![junk.clone(), empty_id3.clone()], vec![0]),
        ];
        for (label, trailing, kept) in runs { assert_policy(label, &trailing, &kept); }
    }

    #[test]
    fn odd_lengths_and_a_missing_final_pad_byte_are_handled_across_dropped_chunks() {
        fn unpadded(mut piece: Vec<u8>) -> Vec<u8> {
            let length = u32::from_le_bytes(piece[4..8].try_into().unwrap());
            assert_eq!(length % 2, 1, "only an odd chunk can lack its pad byte");
            piece.pop();
            piece
        }
        let empty_id3 = taglib_chunk("0", ""); // 1,057 bytes of body: odd
        let odd_data = chunk(b"JUNK", b"seven b");
        let odd_list = chunk(b"LIST", &info(&[(b"INAM", b"abc")])); // value "abc" + pad inside; the body length is even
        assert_eq!(u32::from_le_bytes(empty_id3[4..8].try_into().unwrap()) % 2, 1);
        assert_eq!(u32::from_le_bytes(odd_data[4..8].try_into().unwrap()) % 2, 1);
        // A dropped chunk with a pad byte before a kept odd chunk, and the other way round.
        assert_policy("[empty odd, odd data]", &[empty_id3.clone(), odd_data.clone()], &[1]);
        assert_policy("[odd data, empty odd, odd data]", &[odd_data.clone(), empty_id3.clone(), odd_data.clone()], &[0, 2]);
        // The last chunk of the file lacks its pad byte: kept, dropped, or the only one.
        assert_policy("[empty odd, odd data without pad]", &[empty_id3.clone(), unpadded(odd_data.clone())], &[1]);
        assert_policy("[odd data, empty odd without pad]", &[odd_data.clone(), unpadded(empty_id3.clone())], &[0]);
        assert_policy("[empty odd without pad]", &[unpadded(empty_id3.clone())], &[]);
        assert_policy("[odd data without pad]", &[unpadded(odd_data.clone())], &[0]);
        assert_policy("[data, empty odd, empty list, odd data without pad]", &[odd_list.clone(), empty_id3.clone(), chunk(b"LIST", &info(&[(b"INAM", b"")])), unpadded(odd_data.clone())], &[0, 3]);
        // A tag whose chunk is not padded to an even length by its own padding.
        let odd_tag = chunk(b"ID3 ", &id3_tag(4, &[id3_frame(4, "TBPM", &latin1("0"))], 0)); // 10 + 12 + 0: even body; make it odd
        assert_policy("an empty tag with an even body", &[odd_tag], &[]);
        let odd_body = chunk(b"ID3 ", &id3_tag(4, &[id3_frame(4, "TKEY", &latin1(""))], 0));
        assert_eq!(u32::from_le_bytes(odd_body[4..8].try_into().unwrap()) % 2, 1);
        assert_policy("an empty tag with an odd body", &[odd_body.clone(), odd_data.clone()], &[1]);
        assert_policy("an empty tag with an odd body, last and unpadded", &[odd_data.clone(), unpadded(odd_body)], &[0]);
    }

    #[test]
    fn the_cap_on_trailing_chunks_counts_dropped_chunks_too() {
        let empty = chunk(b"ID3 ", &id3_tag(4, &[], 0));
        let junk = chunk(b"JUNK", b"");
        let shape = |trailing: &[Vec<u8>]| trailing_chunks(&mut Cursor::new(stale_wav(&base(), &trailing.concat()))).unwrap();
        for count in [1usize, 63, 64] {
            let run = vec![empty.clone(); count];
            assert_eq!(shape(&run).map(|found| found.kept.len()), Some(0), "{count} dropped chunks");
            assert_policy(&format!("{count} empty tags"), &run, &[]);
        }
        // 65 chunks is not TagLib's work whatever they hold: the default policy applies.
        let mut run = vec![empty.clone(); 65];
        assert_eq!(shape(&run), None);
        assert_policy_is_default("65 empty tags", &run);
        run[64] = junk.clone();
        assert_eq!(shape(&run), None);
        assert_policy_is_default("64 empty tags and a JUNK chunk", &run);
        // 64 chunks with data in the middle and at the end.
        let mut run = vec![empty.clone(); 64];
        run[10] = junk.clone();
        run[63] = chunk(b"LIST", &info(&[(b"INAM", b"x")]));
        assert_eq!(shape(&run).map(|found| found.kept.len()), Some(2));
        assert_policy("64 chunks, two with data", &run, &[10, 63]);
        // 64 chunks that are all kept, and 65 that are not.
        let all: Vec<usize> = (0..64).collect();
        assert_policy("64 JUNK chunks", &vec![junk.clone(); 64], &all);
        assert_policy_is_default("65 JUNK chunks", &vec![junk; 65]);
    }

    /// `trailing` is outside the bounded shape: the output is the default policy's and nothing is reported.
    fn assert_policy_is_default(label: &str, trailing: &[Vec<u8>]) {
        let file = stale_wav(&base(), &trailing.concat());
        let (output, reported) = rewrite(&file, true);
        assert!(!reported, "{label}");
        assert_eq!(output, metadata_rewrite(&file), "{label}");
    }

    #[test]
    fn a_damaged_head_stops_the_walk_before_the_kept_chunks_and_nothing_is_reported() {
        // The head's second chunk has an identifier that is not one: the walk stops there, as the default policy's does.
        let mut damaged = chunk(b"data", &[0u8; 8]);
        damaged[0] = 0;
        let file = stale_wav(&[base().remove(0), damaged], &chunk(b"JUNK", b"kept in the file, never reached"));
        let (output, reported) = rewrite(&file, true);
        assert!(!reported, "no kept chunk reached the output");
        assert_eq!(output, metadata_rewrite(&file));
        well_formed(&output);
    }

    /// A reader that returns at most `step` bytes per call.
    struct Trickle { inner: Cursor<Vec<u8>>, step: usize }
    impl Read for Trickle {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let limit = buffer.len().min(self.step);
            self.inner.read(&mut buffer[..limit])
        }
    }
    impl Seek for Trickle { fn seek(&mut self, target: SeekFrom) -> io::Result<u64> { self.inner.seek(target) } }

    #[test]
    fn short_reads_from_the_file_change_nothing() {
        let trailing = [taglib_chunk("0", ""), data_id3(), chunk(b"LIST", &info(&[(b"INAM", b"Title\0")])), taglib_chunk("0", "")].concat();
        let file = stale_wav(&base(), &trailing);
        let (expected, _) = rewrite(&file, true);
        for step in [1, 2, 3, 7, 64] {
            let scope = scope(true);
            let mut output = Vec::new();
            rewrite_wav_marker(&mut Trickle { inner: Cursor::new(file.clone()), step }, &mut output, Some(b"{}")).unwrap();
            assert!(scope.kept(), "step {step}");
            assert_eq!(output, expected, "step {step}");
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
        fn below(&mut self, bound: usize) -> usize { (self.next() % bound as u64) as usize }
    }

    #[test]
    fn the_segmented_view_reads_the_head_and_the_kept_ranges_with_the_new_size() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let physical: Vec<u8> = (0..4096).map(|_| rng.next() as u8).collect();
        let trailing = Trailing { declared_end: 100, riff_size: 297 - 8, kept: vec![200..300, 1000..1001, 4000..4096] };
        let mut expected = physical[..100].to_vec();
        expected[4..8].copy_from_slice(&289u32.to_le_bytes());
        for range in &trailing.kept { expected.extend_from_slice(&physical[range.start as usize..range.end as usize]); }
        assert_eq!(expected.len(), 297);
        let mut source = Cursor::new(physical);
        let mut view = SegmentedView::new(&mut source, &trailing);
        let mut all = Vec::new();
        view.read_to_end(&mut all).unwrap();
        assert_eq!(all, expected);
        // Random access through every segment boundary, with buffers that cross them.
        for _ in 0..4000 {
            let start = rng.below(expected.len() + 8);
            let length = 1 + rng.below(80);
            assert_eq!(view.seek(SeekFrom::Start(start as u64)).unwrap(), start as u64);
            let (mut got, mut buffer) = (Vec::new(), vec![0u8; length]);
            while got.len() < length {
                let count = view.read(&mut buffer[..length - got.len()]).unwrap();
                if count == 0 { break; }
                got.extend_from_slice(&buffer[..count]);
            }
            assert_eq!(got, expected[start.min(expected.len())..(start + length).min(expected.len())], "start {start} length {length}");
        }
        assert_eq!(view.seek(SeekFrom::End(0)).unwrap(), 297);
        assert_eq!(view.seek(SeekFrom::End(-97)).unwrap(), 200);
        assert_eq!(view.seek(SeekFrom::Current(5)).unwrap(), 205);
        assert_eq!(view.seek(SeekFrom::Current(-205)).unwrap(), 0);
        assert!(view.seek(SeekFrom::Current(-1)).is_err());
        assert!(view.seek(SeekFrom::End(-298)).is_err());
        assert_eq!(view.seek(SeekFrom::Start(5000)).unwrap(), 5000);
        assert_eq!(view.read(&mut [0u8; 4]).unwrap(), 0, "nothing past the end");
        assert_eq!(view.read(&mut []).unwrap(), 0);
    }

    #[test]
    fn the_view_notices_that_the_walk_read_past_the_declared_end() {
        let physical = vec![7u8; 400];
        let trailing = Trailing { declared_end: 100, riff_size: 150 - 8, kept: vec![200..250] };
        let mut source = Cursor::new(physical);
        let mut view = SegmentedView::new(&mut source, &trailing);
        let mut head = [0u8; 100];
        view.read_exact(&mut head).unwrap();
        assert!(!view.reached_tail, "the head alone is not the tail");
        assert_eq!(&head[4..8], 142u32.to_le_bytes(), "the size field reads as the new size");
        view.read_exact(&mut [0u8; 1]).unwrap();
        assert!(view.reached_tail);
    }

    #[test]
    fn a_deterministic_hostile_loop_never_panics_and_always_gives_a_well_formed_wav_or_the_default_output() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        // Chunks that hold nothing (dropped), and chunks that carry data (kept).
        let empty_pieces: Vec<Vec<u8>> = vec![
            chunk(b"ID3 ", &id3_tag(4, &[id3_frame(4, "TKEY", &latin1("")), id3_frame(4, "TBPM", &latin1("0"))], 24)),
            chunk(b"id3 ", &id3_tag(3, &[id3_frame(3, "TKEY", &latin1("")), id3_frame(3, "TBPM", &[1, 0xff, 0xfe, 0x30, 0])], 33)),
            chunk(b"ID3 ", &id3_tag(2, &[id3_frame(2, "TKE", &latin1("")), id3_frame(2, "TBP", &latin1("0"))], 5)),
            chunk(b"LIST", &info(&[(b"INAM", b"")])), chunk(b"LIST", b"IN"), chunk(b"PLTN", b"old"),
        ];
        let data_pieces: Vec<Vec<u8>> = vec![
            taglib_chunk("128", ""), taglib_chunk("0", "Am"), data_id3(),
            chunk(b"LIST", &info(&[(b"INAM", b"Title\0"), (b"IART", b"Artist\0")])),
            chunk(b"JUNK", b"abc"), chunk(b"fact", &[1, 0, 0, 0]),
        ];
        let mutate = |rng: &mut Rng, bytes: &mut Vec<u8>| {
            if bytes.is_empty() { bytes.push(rng.next() as u8); return; }
            let at = rng.below(bytes.len());
            // Most mutations keep the chunk structure so the policy, not the framing, is what is exercised.
            let operation = if rng.below(10) < 8 { [0, 1, 2, 7, 8][rng.below(5)] } else { 3 + rng.below(4) };
            match operation {
                0 => bytes[at] ^= 1 << rng.below(8),
                1 => bytes[at] = rng.next() as u8,
                2 => bytes[at] = [0x00, 0x7f, 0x80, 0xff][rng.below(4)],
                3 => bytes.truncate(at),
                4 => { for _ in 0..1 + rng.below(24) { bytes.push(rng.next() as u8); } }
                5 => { let end = (at + 1 + rng.below(16)).min(bytes.len()); bytes.drain(at..end); }
                6 => { let end = (at + 1 + rng.below(32)).min(bytes.len()); let copy = bytes[at..end].to_vec(); let to = rng.below(bytes.len() + 1); bytes.splice(to..to, copy); }
                7 => {
                    let value: u32 = [0, 1, 2, 3, 4, 8, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, bytes.len() as u32, bytes.len() as u32 + 1][rng.below(11)];
                    for (offset, byte) in value.to_le_bytes().iter().enumerate() { if let Some(slot) = bytes.get_mut(at + offset) { *slot = *byte; } }
                }
                _ => { for (offset, byte) in [b'I', b'D', b'3', 0x04].iter().enumerate() { if let Some(slot) = bytes.get_mut(at + offset) { *slot = *byte; } } }
            }
        };
        let default_policy = |file: &[u8]| {
            let mut output = Vec::new();
            metadata::rewrite_wav_marker(&mut Cursor::new(file), &mut output, Some(b"{}")).map(|()| output)
        };
        let (mut fallbacks, mut dropped, mut kept_cases, mut head_cases) = (0, 0, 0, 0);
        for case in 0..8000 {
            // A third of the runs hold nothing but empty chunks, a third mix, a third only data.
            let mode = rng.below(3);
            let mut trailing: Vec<u8> = (0..1 + rng.below(4)).flat_map(|_| {
                let from_empty = match mode { 0 => true, 1 => rng.below(2) == 0, _ => false };
                let source = if from_empty { &empty_pieces } else { &data_pieces };
                source[rng.below(source.len())].clone()
            }).collect();
            for _ in 0..rng.below(3) { mutate(&mut rng, &mut trailing); }
            let mut file = stale_wav(&base(), &trailing);
            // One case in ten also damages the head (the RIFF size, a chunk header): only validity is asserted then.
            let damaged_head = case % 10 == 9;
            if damaged_head {
                let head_end = file.len() - trailing.len();
                let at = rng.below(head_end);
                file[at] = rng.next() as u8;
                head_cases += 1;
            }
            let class = match trailing_chunks(&mut Cursor::new(&file)).unwrap() { None => 0, Some(found) if found.kept.is_empty() => 1, Some(_) => 2 };
            let scope = scope(true);
            let mut on = Vec::new();
            let on_result = rewrite_wav_marker(&mut Cursor::new(&file), &mut on, Some(b"{}"));
            let reported = scope.kept();
            drop(scope);
            let off = default_policy(&file);
            assert_eq!(on_result.is_ok(), off.is_ok(), "case {case}: the opt-in refuses exactly what the default policy refuses");
            let Ok(off) = off else { continue };
            let chunks = well_formed(&on);
            well_formed(&off);
            if !reported { assert_eq!(on, off, "case {case}: nothing kept is the default policy's output"); }
            if damaged_head { continue; }
            assert_eq!(reported, class == 2, "case {case}");
            match class {
                0 => fallbacks += 1,
                1 => dropped += 1,
                _ => kept_cases += 1,
            }
            if reported {
                assert_ne!(on, off, "case {case}");
                assert!(chunks.len() > 3 && chunks[chunks.len() - 1].0 == "PLTN" && chunks[0].0 == "fmt " && chunks[1].0 == "data", "case {case}: {chunks:?}");
                // Every kept chunk is one of the trailing chunks, byte for byte.
                for (id, range) in &chunks[2..chunks.len() - 1] {
                    assert!(contains(&trailing, &on[range.clone()]), "case {case}: {id} is not a verbatim trailing chunk");
                }
            }
        }
        assert!(fallbacks > 300 && dropped > 300 && kept_cases > 300, "the loop must exercise every outcome: {fallbacks} fallbacks, {dropped} dropped, {kept_cases} kept");
        assert_eq!(head_cases, 800);
    }
}
