//! Tags and artwork for Opus, in Rust: OpusTags is a Vorbis comment, the same
//! data model as FLAC's Xiph comment, so no native bridge change is needed.
//!
//! * Marker: the field `PLATINUMNOTES`, holding the same line-wrapped Base64 text
//!   as in a FLAC Xiph comment (`native/taglib_bridge_portable.cpp: flac_marker`).
//! * Opus -> Opus and FLAC -> Opus copy every user comment verbatim and in order
//!   (FLAC PICTURE blocks become `METADATA_BLOCK_PICTURE` comments with the
//!   identical block bytes), drop only the stale marker and the two Opus
//!   loudness-gain tags, and append the new marker.
//! * Other sources reach Opus through a FLAC "tag carrier" so that the existing
//!   bridge supplies its usual seven-key (and optional byte-for-byte artwork)
//!   transfer; see `carrier_*`.
//! * Opus sources reach other formats through the same carrier.

use super::head::{CommentHeader, comment_is, comment_value};
use super::reader::scan_file;
use super::{MARKER_FIELD, OpusError, STALE_GAIN_FIELDS};
use crate::metadata::Marker;
use crate::portable::digest::sha256;
use crate::tags::{ArtworkInspection, Format, OutputChoice, TagBridge, TagError, TagInspection, TransferOptions, TransferReport, transfer_mode};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

impl From<OpusError> for TagError { fn from(e: OpusError) -> Self { TagError::Opus(e) } }

/// Everything user-visible that can be carried to an Opus output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    pub vendor: Vec<u8>,
    pub comments: Vec<Vec<u8>>,
    /// Opus-only binary data after the comment list that RFC 7845 asks editors to keep.
    pub preserved_trailing: Vec<u8>,
}

fn is_dropped(comment: &[u8]) -> Option<&'static str> {
    if comment_is(comment, MARKER_FIELD) { return Some(MARKER_FIELD); }
    STALE_GAIN_FIELDS.iter().copied().find(|name| comment_is(comment, name))
}

impl Source {
    /// Remove the old marker and the stale gain tags; return the names removed
    /// (gain tags only: the marker is replaced, not lost).
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut dropped = Vec::new();
        self.comments.retain(|comment| match is_dropped(comment) {
            Some(MARKER_FIELD) => false,
            Some(name) => { dropped.push(name.to_owned()); false }
            None => true,
        });
        dropped
    }
}

// ---- reading ---------------------------------------------------------------

/// The comment header of an Ogg Opus file (no libopus needed).
pub fn read_comment_header(path: &Path) -> Result<CommentHeader, OpusError> {
    let (_, tags) = scan_file(path, true)?;
    CommentHeader::parse(&tags.expect("tags were requested"))
}

pub fn source_from_opus(path: &Path) -> Result<Source, OpusError> {
    let header = read_comment_header(path)?;
    let preserved_trailing = header.preserved_trailing().to_vec();
    Ok(Source { vendor: header.vendor, comments: header.comments, preserved_trailing })
}

/// Marker payload as the FLAC Xiph reader defines it: every comment named
/// PLATINUMNOTES contributes; non-empty values are joined by one space; present
/// but empty is `Some(vec![])`.
pub fn marker_from_comments(comments: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut seen = false;
    let mut have_value = false;
    let mut value = Vec::new();
    for comment in comments {
        if let Some(v) = comment_value(comment, MARKER_FIELD) {
            seen = true;
            if !v.is_empty() {
                if have_value { value.push(b' '); }
                value.extend_from_slice(v);
                have_value = true;
            }
        }
    }
    seen.then_some(value)
}

pub fn read_marker(path: &Path) -> Result<Option<Vec<u8>>, TagError> {
    Ok(marker_from_comments(&read_comment_header(path)?.comments))
}

// ---- base64 (RFC 4648, padded, no whitespace) --------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63]);
        out.push(B64[(n >> 12) as usize & 63]);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] } else { b'=' });
    }
    out
}

pub fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(4) { return None; }
    let digit = |c: u8| -> Option<u32> {
        match c { b'A'..=b'Z' => Some(u32::from(c - b'A')), b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52), b'+' => Some(62), b'/' => Some(63), _ => None }
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let chunks = text.len() / 4;
    for (index, chunk) in text.chunks_exact(4).enumerate() {
        let last = index + 1 == chunks;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) { return None; }
        let a = digit(chunk[0])?;
        let b = digit(chunk[1])?;
        let c = if pad == 2 { 0 } else { digit(chunk[2])? };
        let d = if pad >= 1 { 0 } else { digit(chunk[3])? };
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((n >> 16) as u8);
        if pad < 2 { out.push((n >> 8) as u8); }
        if pad < 1 { out.push(n as u8); }
    }
    Some(out)
}

// ---- FLAC picture block / Opus METADATA_BLOCK_PICTURE ------------------------

/// Fields of a FLAC `PICTURE` block (also the payload of METADATA_BLOCK_PICTURE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub kind: u32,
    pub mime: Vec<u8>,
    pub description: Vec<u8>,
    pub data_offset: usize,
    pub data_len: usize,
}

pub fn parse_picture(block: &[u8]) -> Option<Picture> {
    let mut cursor = 0usize;
    let take = |cursor: &mut usize| -> Option<usize> {
        let end = cursor.checked_add(4).filter(|e| *e <= block.len())?;
        let v = u32::from_be_bytes(block[*cursor..end].try_into().ok()?) as usize;
        *cursor = end;
        Some(v)
    };
    let kind = take(&mut cursor)? as u32;
    let mime_len = take(&mut cursor)?;
    let mime = block.get(cursor..cursor.checked_add(mime_len)?)?.to_vec();
    cursor += mime_len;
    let desc_len = take(&mut cursor)?;
    let description = block.get(cursor..cursor.checked_add(desc_len)?)?.to_vec();
    cursor += desc_len;
    for _ in 0..4 { take(&mut cursor)?; }
    let data_len = take(&mut cursor)?;
    (cursor.checked_add(data_len)? == block.len()).then_some(Picture { kind, mime, description, data_offset: cursor, data_len })
}

fn picture_entry(comment: &[u8]) -> Option<(Vec<u8>, bool)> {
    // (decoded bytes, true when METADATA_BLOCK_PICTURE and false when legacy COVERART)
    if let Some(value) = comment_value(comment, "METADATA_BLOCK_PICTURE") {
        let block = base64_decode(value)?;
        parse_picture(&block)?;
        return Some((block, true));
    }
    if let Some(value) = comment_value(comment, "COVERART") {
        if value.len() > 3 { return base64_decode(value).filter(|d| !d.is_empty()).map(|d| (d, false)); }
    }
    None
}

// ---- inspection (the bridge's TagInspection shape, TagLib 1.11.1 semantics) ---

fn tag_to_int(text: &str) -> i64 {
    // wcstol(base 10) followed by static_cast<int>, as TagLib 1.11.1's String::toInt.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') { i += 1; }
    let negative = match bytes.get(i) { Some(b'-') => { i += 1; true } Some(b'+') => { i += 1; false } _ => false };
    let mut value: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        value = value.saturating_mul(10).saturating_add(i64::from(bytes[i] - b'0'));
        i += 1;
    }
    let value = if negative { -value } else { value };
    i64::from(value as i32)
}

pub fn inspect(path: &Path) -> Result<TagInspection, TagError> {
    let header = read_comment_header(path)?;
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut artwork = Vec::new();
    for comment in &header.comments {
        if let Some((decoded, is_block)) = picture_entry(comment) {
            if is_block {
                let picture = parse_picture(&decoded).expect("validated by picture_entry");
                artwork.push(ArtworkInspection {
                    container: "opus_picture".into(),
                    mime_type: Some(String::from_utf8_lossy(&picture.mime).into_owned()),
                    picture_type: Some(picture.kind),
                    description: Some(String::from_utf8_lossy(&picture.description).into_owned()),
                    format_code: None,
                    byte_count: picture.data_len,
                    sha256: hex(&sha256(&decoded[picture.data_offset..picture.data_offset + picture.data_len])),
                });
            } else {
                artwork.push(ArtworkInspection { container: "opus_coverart".into(), mime_type: None, picture_type: None,
                    description: None, format_code: None, byte_count: decoded.len(), sha256: hex(&sha256(&decoded)) });
            }
            continue;
        }
        let Some(eq) = comment.iter().position(|&b| b == b'=') else { continue };
        if eq < 1 { continue; }
        let key: String = String::from_utf8_lossy(&comment[..eq]).to_ascii_uppercase();
        let value = String::from_utf8_lossy(&comment[eq + 1..]).into_owned();
        if value.is_empty() { continue; }
        fields.entry(key).or_default().push(value);
    }
    let joined = |key: &str| fields.get(key).map(|v| v.join(" ")).filter(|s| !s.is_empty());
    let comment = joined("DESCRIPTION").or_else(|| joined("COMMENT"));
    let year = fields.get("DATE").and_then(|v| v.first()).map(|s| tag_to_int(s))
        .or_else(|| fields.get("YEAR").and_then(|v| v.first()).map(|s| tag_to_int(s))).unwrap_or(0);
    let tempo = joined("BPM").map(|s| tag_to_int(&s)).filter(|n| *n > 0).unwrap_or(0);
    Ok(TagInspection {
        artist: joined("ARTIST"), album: joined("ALBUM"), year: i64::from(year as i32 as u32), title: joined("TITLE"),
        comment, tempo, initial_key: joined("INITIALKEY"),
        properties: fields, unsupported_data: Vec::new(), id3_frames: None, artwork,
    })
}

fn hex(digest: &[u8; 32]) -> String { digest.iter().map(|b| format!("{b:02x}")).collect() }

// ---- FLAC reading and the carrier -------------------------------------------

/// Read the Vorbis comments and pictures of a FLAC file that begins with `fLaC`.
/// `Ok(None)` means the file does not (e.g. an ID3v2 prefix): use the carrier path.
pub fn source_from_flac(path: &Path) -> Result<Option<Source>, TagError> {
    let io = |source| TagError::Io { operation: "read FLAC tags", source };
    let mut file = File::open(path).map_err(io)?;
    let length = file.metadata().map_err(io)?.len();
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() || &magic != b"fLaC" { return Ok(None); }
    let mut position = 4u64;
    let mut first = true;
    let mut vorbis: Option<Vec<u8>> = None;
    let mut pictures: Vec<Vec<u8>> = Vec::new();
    let mut metadata_bytes = 0usize;
    loop {
        if length - position < 4 { return Err(TagError::SourceContainerUnavailable("truncated FLAC metadata header".into())); }
        let mut header = [0u8; 4];
        file.read_exact(&mut header).map_err(io)?;
        position += 4;
        let last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let size = (u32::from(header[1]) << 16) | (u32::from(header[2]) << 8) | u32::from(header[3]);
        if u64::from(size) > length - position { return Err(TagError::SourceContainerUnavailable("truncated FLAC metadata block".into())); }
        if first && kind != 0 { return Err(TagError::SourceContainerUnavailable("FLAC STREAMINFO is not first".into())); }
        first = false;
        metadata_bytes = metadata_bytes.saturating_add(size as usize);
        if metadata_bytes > super::head::MAX_COMMENT_HEADER { return Err(TagError::Opus(OpusError::Limit("FLAC metadata exceeds the supported size"))); }
        match kind {
            4 if vorbis.is_none() => {
                let mut body = vec![0u8; size as usize];
                file.read_exact(&mut body).map_err(io)?;
                vorbis = Some(body);
            }
            6 => {
                let mut body = vec![0u8; size as usize];
                file.read_exact(&mut body).map_err(io)?;
                pictures.push(body);
            }
            _ => { file.seek(SeekFrom::Current(i64::from(size))).map_err(io)?; }
        }
        position += u64::from(size);
        if last { break; }
    }
    let mut packet = b"OpusTags".to_vec();
    packet.extend_from_slice(&vorbis.unwrap_or_else(|| vec![0, 0, 0, 0, 0, 0, 0, 0]));
    let header = CommentHeader::parse(&packet).map_err(|_| TagError::SourceContainerUnavailable("invalid FLAC Vorbis comment block".into()))?;
    let mut comments = header.comments;
    for block in pictures {
        let mut entry = b"METADATA_BLOCK_PICTURE=".to_vec();
        entry.extend_from_slice(&base64_encode(&block));
        comments.push(entry);
    }
    Ok(Some(Source { vendor: header.vendor, comments, preserved_trailing: Vec::new() }))
}

const FLAC_MAX_BLOCK: usize = 0x00ff_ffff;

/// Write a FLAC file that carries `source`'s tags and no audio: STREAMINFO, one
/// Vorbis comment block and one PICTURE block per `METADATA_BLOCK_PICTURE`.
/// Comments that are not pictures and the stale tags are copied as they are.
pub fn write_carrier(path: &Path, source: &Source) -> Result<(), TagError> {
    let mut plain: Vec<&[u8]> = Vec::new();
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for comment in &source.comments {
        if is_dropped(comment).is_some() { continue; }
        if comment_value(comment, "METADATA_BLOCK_PICTURE").is_some() {
            let block = picture_entry(comment).filter(|(_, is_block)| *is_block).map(|(b, _)| b)
                .ok_or_else(|| TagError::ArtworkTranscodeUnavailable("an Opus picture is not a valid METADATA_BLOCK_PICTURE".into()))?;
            if block.len() > FLAC_MAX_BLOCK { return Err(TagError::ArtworkTranscodeUnavailable("an Opus picture exceeds the 16777215-byte FLAC block limit".into())); }
            blocks.push(block);
        } else { plain.push(comment); }
    }
    let mut vorbis = Vec::new();
    vorbis.extend_from_slice(&(source.vendor.len() as u32).to_le_bytes());
    vorbis.extend_from_slice(&source.vendor);
    vorbis.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    for comment in plain {
        vorbis.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        vorbis.extend_from_slice(comment);
    }
    if vorbis.len() > FLAC_MAX_BLOCK { return Err(TagError::Library("the comments exceed the FLAC metadata block limit".into())); }
    let mut out = b"fLaC".to_vec();
    // STREAMINFO: 4096-sample blocks, 48 kHz stereo 16-bit, zero samples, no MD5.
    let mut info = [0u8; 34];
    info[0..2].copy_from_slice(&4096u16.to_be_bytes());
    info[2..4].copy_from_slice(&4096u16.to_be_bytes());
    let packed: u64 = (48_000u64 << 44) | (1u64 << 41) | (15u64 << 36);
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    let block = |out: &mut Vec<u8>, last: bool, kind: u8, body: &[u8]| {
        out.push(if last { 0x80 | kind } else { kind });
        out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(body);
    };
    block(&mut out, false, 0, &info);
    block(&mut out, blocks.is_empty(), 4, &vorbis);
    let count = blocks.len();
    for (index, picture) in blocks.iter().enumerate() { block(&mut out, index + 1 == count, 6, picture); }
    let mut file = std::fs::OpenOptions::new().write(true).open(path).map_err(|source| TagError::Io { operation: "write tag carrier", source })?;
    file.set_len(0).map_err(|source| TagError::Io { operation: "write tag carrier", source })?;
    file.write_all(&out).and_then(|()| file.sync_all()).map_err(|source| TagError::Io { operation: "write tag carrier", source })
}

// ---- output ------------------------------------------------------------------

/// Build the final comment header of an Opus output and rewrite `staging` with it.
/// Returns the names of the loudness tags that were not carried over.
pub fn finalize(staging: &Path, mut source: Source, marker: Option<&[u8]>) -> Result<Vec<String>, TagError> {
    let dropped = source.sanitize();
    let existing = read_comment_header(staging)?;
    let mut comments = source.comments;
    if let Some(marker) = marker {
        let mut entry = format!("{MARKER_FIELD}=").into_bytes();
        entry.extend_from_slice(marker);
        comments.push(entry);
    }
    let header = CommentHeader { vendor: existing.vendor, comments, trailing: source.preserved_trailing };
    super::repack_with_comments(staging, &header)?;
    Ok(dropped)
}

// ---- the bridge side -----------------------------------------------------------

/// A private directory holding one carrier file; removed on drop.
struct Carrier { workspace: crate::file_workflow::Workspace }
impl Carrier {
    fn new() -> Result<Self, TagError> {
        let workspace = crate::file_workflow::Workspace::create(&std::env::temp_dir(), "flac")
            .map_err(|e| TagError::Io { operation: "create tag carrier", source: std::io::Error::other(e.to_string()) })?;
        let file = workspace.create_stage()
            .map_err(|e| TagError::Io { operation: "create tag carrier", source: std::io::Error::other(e.to_string()) })?;
        drop(file);
        Ok(Self { workspace })
    }
    fn path(&self) -> &Path { self.workspace.staging_path() }
}
impl Drop for Carrier {
    fn drop(&mut self) { let _ = self.workspace.cleanup(false); }
}

impl TagBridge {
    /// Every transfer in which the source or destination is Opus. Opus -> Opus and
    /// FLAC -> Opus are pure Rust; anything else goes through a FLAC tag carrier so
    /// the bridge's own generic or direct copy defines the result.
    pub(crate) fn transfer_opus(&self, source: &Path, destination: &Path, source_format: Format, destination_format: Format,
                                choice: OutputChoice, marker: &Marker, options: TransferOptions) -> Result<TransferReport, TagError> {
        use std::os::unix::fs::MetadataExt;
        let mode = transfer_mode(source_format, destination_format, choice)?;
        let src_meta = std::fs::metadata(source).map_err(|source| TagError::Io { operation: "inspect source tag file", source })?;
        let dst_meta = std::fs::metadata(destination).map_err(|source| TagError::Io { operation: "inspect staging tag file", source })?;
        if src_meta.dev() == dst_meta.dev() && src_meta.ino() == dst_meta.ino() { return Err(TagError::SourceIsDestination); }
        let payload = marker.encoded_bytes()?;
        let mut report = TransferReport { mode, copy_error: None, marker_error: None, artwork_copied: false,
            #[cfg(any(target_os = "linux", feature = "portable"))] wav_tags_preserved: false, dropped_tags: Vec::new() };
        if destination_format == Format::Opus {
            let tags = match source_format {
                Format::Opus => source_from_opus(source)?,
                Format::Flac => match source_from_flac(source)? {
                    Some(tags) => tags,
                    None => self.tags_through_carrier(source, source_format, marker, options, &mut report)?,
                },
                other => self.tags_through_carrier(source, other, marker, options, &mut report)?,
            };
            report.dropped_tags = finalize(destination, tags, payload.as_deref())?;
        } else {
            let mut tags = source_from_opus(source)?;
            report.dropped_tags = tags.clone().sanitize();
            tags.preserved_trailing.clear();
            let carrier = Carrier::new()?;
            write_carrier(carrier.path(), &tags)?;
            let inner = self.transfer_with_options(carrier.path(), destination, Format::Flac, destination_format,
                OutputChoice::Explicit, marker, options)?;
            report.copy_error = inner.copy_error;
            report.marker_error = inner.marker_error;
            report.artwork_copied = inner.artwork_copied;
            #[cfg(any(target_os = "linux", feature = "portable"))] { report.wav_tags_preserved = inner.wav_tags_preserved; }
        }
        Ok(report)
    }

    /// Tags of a non-Xiph source (MP3, MP4, WAV, AIFF, or a FLAC with an ID3 prefix)
    /// as the bridge's transfer into a FLAC carrier defines them. Artwork must be
    /// copied unchanged (the only result that is the same on every build), so a
    /// source with embedded pictures is refused unless the bridge can do that.
    fn tags_through_carrier(&self, source: &Path, format: Format, marker: &Marker, options: TransferOptions,
                            report: &mut TransferReport) -> Result<Source, TagError> {
        let inspection = self.inspect(source, format)?;
        if !inspection.artwork.is_empty() && !(options.copy_artwork && self.supports_artwork_copy()) {
            return Err(TagError::ArtworkTranscodeUnavailable(
                "Opus output carries artwork unchanged, and this source has embedded artwork that this build cannot copy byte-for-byte (Linux and portable builds need parity.allow_non_identical; macOS default builds refuse)".into()));
        }
        let carrier = Carrier::new()?;
        write_carrier(carrier.path(), &Source::default())?;
        let inner = self.transfer_with_options(source, carrier.path(), format, Format::Flac, OutputChoice::Explicit, marker, options)?;
        report.copy_error = inner.copy_error;
        report.marker_error = inner.marker_error;
        report.artwork_copied = inner.artwork_copied;
        source_from_flac(carrier.path())?
            .ok_or_else(|| TagError::SourceContainerUnavailable("the tag carrier is not a FLAC file".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors_and_rejects_malformed_text() {
        for (plain, encoded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64_encode(plain.as_bytes()), encoded.as_bytes());
            assert_eq!(base64_decode(encoded.as_bytes()).unwrap(), plain.as_bytes());
        }
        for bad in ["Zg=", "Z===", "Zm9v\n", "Zm=v", "=m9v", "Zm9vYg==Zm9v", "Zm9*"] {
            assert!(base64_decode(bad.as_bytes()).is_none(), "{bad:?}");
        }
    }

    fn picture_block(data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes());
        b.extend_from_slice(&9u32.to_be_bytes());
        b.extend_from_slice(b"image/png");
        b.extend_from_slice(&4u32.to_be_bytes());
        b.extend_from_slice(b"Back");
        for v in [10u32, 20, 24, 0] { b.extend_from_slice(&v.to_be_bytes()); }
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn picture_blocks_parse_exactly_and_reject_inconsistent_lengths() {
        let block = picture_block(b"\x89PNGdata");
        let p = parse_picture(&block).unwrap();
        assert_eq!((p.kind, &p.mime[..], &p.description[..], p.data_len), (3, &b"image/png"[..], &b"Back"[..], 8));
        assert!(parse_picture(&block[..block.len() - 1]).is_none());
        let mut longer = block.clone();
        longer.push(0);
        assert!(parse_picture(&longer).is_none());
        assert!(parse_picture(&[0, 0, 0]).is_none());
    }

    #[test]
    fn marker_semantics_follow_the_flac_xiph_reader() {
        let c = |s: &str| s.as_bytes().to_vec();
        assert_eq!(marker_from_comments(&[c("TITLE=x")]), None);
        assert_eq!(marker_from_comments(&[c("PLATINUMNOTES=abc")]), Some(c("abc")));
        assert_eq!(marker_from_comments(&[c("platinumnotes=a"), c("PlatinumNotes="), c("PLATINUMNOTES=b")]), Some(c("a b")));
        assert_eq!(marker_from_comments(&[c("PLATINUMNOTES=")]), Some(Vec::new()));
        assert_eq!(marker_from_comments(&[c("PLATINUMNOTESX=1")]), None);
    }

    #[test]
    fn to_int_matches_wcstol_cast_to_int() {
        for (text, want) in [("2019", 2019), ("2019-05-01", 2019), (" 12abc", 12), ("-7x", -7), ("+5", 5), ("abc", 0), ("", 0),
            ("4294967296", 0), ("2147483648", i64::from(i32::MIN)), ("99999999999999999999", -1)] {
            assert_eq!(tag_to_int(text), want, "{text:?}");
        }
    }

    #[test]
    fn sanitize_removes_marker_and_loudness_tags_only() {
        let c = |s: &str| s.as_bytes().to_vec();
        let mut source = Source { vendor: c("v"), comments: vec![c("TITLE=a"), c("R128_TRACK_GAIN=-573"), c("replaygain_track_gain=-3 dB"),
            c("PLATINUMNOTES=old"), c("r128_album_gain=1"), c("ARTIST=b")], preserved_trailing: Vec::new() };
        assert_eq!(source.sanitize(), ["R128_TRACK_GAIN", "R128_ALBUM_GAIN"]);
        assert_eq!(source.comments, vec![c("TITLE=a"), c("replaygain_track_gain=-3 dB"), c("ARTIST=b")]);
    }
}
