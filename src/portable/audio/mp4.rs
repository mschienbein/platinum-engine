//! Read and write the non-fragmented ALAC MP4 subset whose AudioToolbox layout
//! is established. Indexing does not load the whole media file into memory.
//! Fragmented MP4 and non-identity edit lists are rejected rather than approximated.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use super::alac::Config;

#[derive(Debug)]
pub enum Mp4Error { Io(io::Error), Invalid(&'static str), Unsupported(&'static str) }
impl fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self { Self::Io(e) => write!(f, "MP4 I/O: {e}"), Self::Invalid(s) => write!(f, "invalid MP4: {s}"), Self::Unsupported(s) => write!(f, "unsupported MP4: {s}") }
    }
}
impl std::error::Error for Mp4Error { fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { if let Self::Io(e) = self { Some(e) } else { None } } }
impl From<io::Error> for Mp4Error { fn from(e: io::Error) -> Self { Self::Io(e) } }
type Result<T, E = Mp4Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet { pub offset: u64, pub size: u32, pub frames: u32 }
/// Layout metadata carried by the sample entry. Unknown tags are preserved;
/// their channel-mixing semantics must not be inferred from this structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelLayout { pub tag: u32, pub bitmap: u32, pub descriptions: u32 }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlacFile { pub config: Config, pub channel_layout: Option<ChannelLayout>, pub frames: u64, pub packets: Vec<Packet> }

#[derive(Clone, Copy)]
struct Atom<'a> { kind: [u8; 4], data: &'a [u8] }
fn u16be(data: &[u8], i: usize) -> Result<u16> {
    let bytes = data.get(i..i + 2).ok_or(Mp4Error::Invalid("truncated integer"))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}
fn u32be(data: &[u8], i: usize) -> Result<u32> {
    let bytes = data.get(i..i + 4).ok_or(Mp4Error::Invalid("truncated integer"))?;
    Ok(u32::from_be_bytes(bytes.try_into().expect("four-byte slice")))
}
fn u64be(data: &[u8], i: usize) -> Result<u64> {
    let bytes = data.get(i..i + 8).ok_or(Mp4Error::Invalid("truncated integer"))?;
    Ok(u64::from_be_bytes(bytes.try_into().expect("eight-byte slice")))
}
fn atoms(mut data: &[u8]) -> Result<Vec<Atom<'_>>> {
    let mut list = Vec::new();
    while !data.is_empty() {
        if data.len() < 8 { return Err(Mp4Error::Invalid("truncated atom header")); }
        let short = u32be(data, 0)?;
        let kind = data[4..8].try_into().expect("four-byte slice");
        let (size, header) = match short {
            0 => (data.len(), 8usize),
            1 => (usize::try_from(u64be(data, 8)?).map_err(|_| Mp4Error::Invalid("atom size overflows"))?, 16usize),
            n => (n as usize, 8usize),
        };
        if size < header || size > data.len() { return Err(Mp4Error::Invalid("atom extends past parent")); }
        list.try_reserve(1).map_err(|_| Mp4Error::Unsupported("atom index allocation failed"))?;
        list.push(Atom { kind, data: &data[header..size] });
        data = &data[size..];
    }
    Ok(list)
}
fn one<'a>(children: &[Atom<'a>], kind: &[u8; 4]) -> Result<Atom<'a>> {
    let mut matches = children.iter().filter(|a| &a.kind == kind);
    let atom = *matches.next().ok_or(Mp4Error::Invalid("required atom missing"))?;
    if matches.next().is_some() { return Err(Mp4Error::Unsupported("duplicate atom")); }
    Ok(atom)
}
fn full_box(data: &[u8]) -> Result<()> {
    if u32be(data, 0)? != 0 { return Err(Mp4Error::Unsupported("unsupported full-box version or flags")); }
    Ok(())
}
fn entries(data: &[u8], width: usize) -> Result<usize> {
    full_box(data)?;
    let count = u32be(data, 4)? as usize;
    let need = count.checked_mul(width).and_then(|n| n.checked_add(8)).ok_or(Mp4Error::Invalid("sample table too large"))?;
    if need != data.len() { return Err(Mp4Error::Invalid("sample table size mismatch")); }
    Ok(count)
}
fn config_from_stsd(data: &[u8]) -> Result<(Config, Option<ChannelLayout>)> {
    full_box(data)?;
    if u32be(data, 4)? != 1 { return Err(Mp4Error::Unsupported("multiple sample descriptions")); }
    let descriptions = atoms(data.get(8..).ok_or(Mp4Error::Invalid("truncated stsd"))?)?;
    if descriptions.len() != 1 || descriptions[0].kind != *b"alac" { return Err(Mp4Error::Unsupported("not a single ALAC description")); }
    let entry = descriptions[0].data;
    if entry.len() < 28 || u16be(entry, 8)? != 0 { return Err(Mp4Error::Unsupported("non-version-zero ALAC sample entry")); }
    let extensions = atoms(&entry[28..])?;
    let raw = one(&extensions, b"alac")?.data;
    full_box(raw)?;
    let cookie = raw.get(4..28).ok_or(Mp4Error::Invalid("truncated ALAC cookie"))?;
    if cookie[4] != 0 { return Err(Mp4Error::Unsupported("ALAC cookie version")); }
    let config = Config { frame_length: u32be(cookie, 0)?, bit_depth: cookie[5], pb: cookie[6], mb: cookie[7], kb: cookie[8], channels: cookie[9], sample_rate: u32be(cookie, 20)? };
    if config.frame_length == 0 || config.frame_length > 65536 || !matches!(config.bit_depth, 16 | 20 | 24 | 32)
        || !(1..=8).contains(&config.channels) || config.kb == 0 || config.kb > 31 || config.sample_rate == 0 {
        return Err(Mp4Error::Unsupported("ALAC cookie configuration"));
    }
    let mut layouts = extensions.iter().filter(|a| a.kind == *b"chan");
    let channel_layout = if let Some(layout) = layouts.next() {
        if layouts.next().is_some() { return Err(Mp4Error::Unsupported("multiple ALAC channel layouts")); }
        full_box(layout.data)?;
        let tag = u32be(layout.data, 4)?;
        let bitmap = u32be(layout.data, 8)?;
        let descriptions = u32be(layout.data, 12)?;
        let expected = (descriptions as usize).checked_mul(20).and_then(|n| n.checked_add(16))
            .ok_or(Mp4Error::Invalid("channel layout size overflow"))?;
        if layout.data.len() != expected { return Err(Mp4Error::Invalid("channel layout size mismatch")); }
        Some(ChannelLayout { tag, bitmap, descriptions })
    } else { None };
    Ok((config, channel_layout))
}

fn gapless_frames(movie: &[Atom<'_>]) -> Result<Option<u64>> {
    let Some(udta) = movie.iter().find(|a| a.kind == *b"udta") else { return Ok(None); };
    let udta_children = atoms(udta.data)?;
    let Some(meta) = udta_children.iter().find(|a| a.kind == *b"meta") else { return Ok(None); };
    full_box(meta.data)?;
    let metadata = atoms(&meta.data[4..])?;
    let Some(ilst) = metadata.iter().find(|a| a.kind == *b"ilst") else { return Ok(None); };
    let items = atoms(ilst.data)?;
    let mut found = None;
    for item in items.iter().filter(|a| a.kind == *b"----") {
        let fields = atoms(item.data)?;
        let (Some(mean), Some(name), Some(data)) = (
            fields.iter().find(|a| a.kind == *b"mean"),
            fields.iter().find(|a| a.kind == *b"name"),
            fields.iter().find(|a| a.kind == *b"data"),
        ) else { continue };
        if mean.data.get(4..) != Some(&b"com.apple.iTunes"[..]) || name.data.get(4..) != Some(&b"iTunSMPB"[..]) { continue; }
        if found.is_some() { return Err(Mp4Error::Unsupported("multiple gapless metadata items")); }
        if data.data.get(..8) != Some(&[0, 0, 0, 1, 0, 0, 0, 0]) { return Err(Mp4Error::Unsupported("gapless metadata encoding")); }
        let text = std::str::from_utf8(&data.data[8..]).map_err(|_| Mp4Error::Invalid("gapless metadata is not UTF-8"))?;
        let mut fields = text.split_ascii_whitespace();
        let missing = Mp4Error::Invalid("truncated gapless metadata");
        fields.next().ok_or(Mp4Error::Invalid("truncated gapless metadata"))?;
        let priming = fields.next().ok_or(Mp4Error::Invalid("truncated gapless metadata"))?;
        fields.next().ok_or(Mp4Error::Invalid("truncated gapless metadata"))?;
        let valid = fields.next().ok_or(missing)?;
        let parse = |s: &str| u64::from_str_radix(s, 16).map_err(|_| Mp4Error::Invalid("invalid gapless metadata number"));
        if parse(priming)? != 0 { return Err(Mp4Error::Unsupported("ALAC priming frames")); }
        found = Some(parse(valid)?);
    }
    Ok(found)
}

fn packet_frame_length<R: Read + Seek>(reader: &mut R, packet: Packet, default: u32) -> Result<u32> {
    if packet.size < 3 { return Err(Mp4Error::Invalid("truncated ALAC packet header")); }
    let mut header = [0u8; 7];
    reader.seek(SeekFrom::Start(packet.offset))?;
    reader.read_exact(&mut header[..3])?;
    let tag = header[0] >> 5;
    if !matches!(tag, 0 | 1 | 3) { return Err(Mp4Error::Unsupported("initial ALAC packet element")); }
    if header[2] & 0x10 == 0 { return Ok(default); }
    if packet.size < 7 { return Err(Mp4Error::Invalid("truncated partial ALAC packet header")); }
    reader.read_exact(&mut header[3..])?;
    let word = header.iter().fold(0u64, |v, &byte| (v << 8) | u64::from(byte));
    Ok(((word >> 1) & 0xffff_ffff) as u32)
}

// A single zero-offset, unit-rate edit covering the entire media is a no-op.
// Other edits need timeline remapping and are deliberately refused.
fn validate_identity_edit(track: &[Atom<'_>], movie: &[Atom<'_>], rate: u32, frames: u64) -> Result<()> {
    let Some(edit) = track.iter().find(|a| a.kind == *b"edts") else { return Ok(()); };
    if track.iter().filter(|a| a.kind == *b"edts").count() != 1 { return Err(Mp4Error::Unsupported("multiple edit lists")); }
    let children = atoms(edit.data)?;
    let list = one(&children, b"elst")?.data;
    if list.len() < 8 || list[1..4] != [0, 0, 0] || u32be(list, 4)? != 1 {
        return Err(Mp4Error::Unsupported("non-identity MP4 edit list"));
    }
    let (duration, media_time, speed) = match list[0] {
        0 if list.len() == 20 => (u64::from(u32be(list, 8)?), u64::from(u32be(list, 12)?), u32be(list, 16)?),
        1 if list.len() == 28 => (u64be(list, 8)?, u64be(list, 16)?, u32be(list, 24)?),
        _ => return Err(Mp4Error::Unsupported("MP4 edit-list version or size")),
    };
    let mvhd = one(movie, b"mvhd")?.data;
    let scale = match mvhd.first() {
        Some(0) => u32be(mvhd, 12)?,
        Some(1) => u32be(mvhd, 20)?,
        _ => return Err(Mp4Error::Unsupported("movie-header version")),
    };
    if media_time != 0 || speed != 0x00010000 || scale == 0
        || u128::from(duration) * u128::from(rate) != u128::from(frames) * u128::from(scale) {
        return Err(Mp4Error::Unsupported("non-identity MP4 edit list"));
    }
    Ok(())
}

fn sample_size(stsz: &[u8], fixed: u32, index: usize) -> Result<u32> {
    if fixed != 0 { Ok(fixed) } else { u32be(stsz, 12 + index * 4) }
}

/// Index one non-fragmented ALAC track. Other tracks are not selected
/// by guesswork: a file with more than one ALAC track is refused.
pub fn read_alac<R: Read + Seek>(reader: &mut R) -> Result<AlacFile> {
    const MAX_MOOV: u64 = 64 * 1024 * 1024;
    let length = reader.seek(SeekFrom::End(0))?;
    let mut pos = 0u64;
    let mut moov = None;
    let mut media = Vec::new();
    while pos < length {
        if length - pos < 8 { return Err(Mp4Error::Invalid("truncated top-level atom")); }
        reader.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 16];
        reader.read_exact(&mut header[..8])?;
        let short = u32::from_be_bytes(header[..4].try_into().expect("four-byte slice"));
        let kind: [u8; 4] = header[4..8].try_into().expect("four-byte slice");
        let (size, hlen) = match short {
            0 => (length - pos, 8u64),
            1 => { reader.read_exact(&mut header[8..])?; (u64::from_be_bytes(header[8..].try_into().expect("eight-byte slice")), 16) },
            n => (u64::from(n), 8),
        };
        if size < hlen || size > length - pos { return Err(Mp4Error::Invalid("invalid top-level atom size")); }
        if &kind == b"moof" { return Err(Mp4Error::Unsupported("fragmented MP4")); }
        if &kind == b"mdat" {
            media.try_reserve(1).map_err(|_| Mp4Error::Unsupported("media range allocation failed"))?;
            media.push((pos + hlen, pos + size));
        }
        if &kind == b"moov" {
            if moov.is_some() { return Err(Mp4Error::Unsupported("multiple movie atoms")); }
            if size > MAX_MOOV { return Err(Mp4Error::Unsupported("movie atom exceeds 64 MiB")); }
            let payload_size = usize::try_from(size - hlen).map_err(|_| Mp4Error::Invalid("movie atom size overflows"))?;
            let mut data = Vec::new();
            data.try_reserve_exact(payload_size).map_err(|_| Mp4Error::Unsupported("movie atom allocation failed"))?;
            data.resize(payload_size, 0);
            reader.read_exact(&mut data)?;
            moov = Some(data);
        }
        pos += size;
    }
    let movie = moov.ok_or(Mp4Error::Invalid("missing movie atom"))?;
    let movie_children = atoms(&movie)?;
    if movie_children.iter().any(|a| a.kind == *b"mvex") { return Err(Mp4Error::Unsupported("fragmented MP4")); }
    let mut selected = None;
    for track in movie_children.iter().filter(|a| a.kind == *b"trak") {
        let track_children = atoms(track.data)?;
        let mdia = one(&track_children, b"mdia")?;
        let mdia_children = atoms(mdia.data)?;
        let handler = one(&mdia_children, b"hdlr")?.data;
        if handler.get(8..12) != Some(&b"soun"[..]) { continue; }
        let minf = one(&mdia_children, b"minf")?;
        let minf_children = atoms(minf.data)?;
        let stbl = one(&minf_children, b"stbl")?;
        let table = atoms(stbl.data)?;
        let description = one(&table, b"stsd")?;
        let (config, channel_layout) = match config_from_stsd(description.data) {
            Ok(c) => c,
            Err(Mp4Error::Unsupported("not a single ALAC description")) => continue,
            Err(e) => return Err(e),
        };
        if selected.is_some() { return Err(Mp4Error::Unsupported("multiple ALAC tracks")); }
        let mdhd = one(&mdia_children, b"mdhd")?.data;
        let version = *mdhd.first().ok_or(Mp4Error::Invalid("truncated media header"))?;
        let timescale = match version { 0 => u32be(mdhd, 12)?, 1 => u32be(mdhd, 20)?, _ => return Err(Mp4Error::Unsupported("media-header version")) };
        if timescale != config.sample_rate { return Err(Mp4Error::Unsupported("media timescale differs from ALAC sample rate")); }

        let stsz = one(&table, b"stsz")?.data;
        full_box(stsz)?;
        let fixed = u32be(stsz, 4)?;
        let count = u32be(stsz, 8)? as usize;
        if count > 16_000_000 { return Err(Mp4Error::Unsupported("too many ALAC packets")); }
        let expected = 12usize.checked_add(if fixed == 0 { count.checked_mul(4).ok_or(Mp4Error::Invalid("sample sizes overflow"))? } else { 0 }).ok_or(Mp4Error::Invalid("sample sizes overflow"))?;
        if stsz.len() != expected { return Err(Mp4Error::Invalid("sample-size table length")); }
        let media_bytes = media.iter().try_fold(0u64, |total, &(start, end)|
            total.checked_add(end - start).ok_or(Mp4Error::Invalid("media size overflows")))?;
        // A decodable packet has at least a three-byte element header. Even
        // constant-size stsz tables must be bounded by actual media bytes.
        if (count as u64) * 3 > media_bytes {
            return Err(Mp4Error::Invalid("sample count exceeds media data"));
        }
        if count != 0 && fixed != 0 {
            if fixed < 3 { return Err(Mp4Error::Invalid("truncated ALAC packet")); }
            if (count as u64) * u64::from(fixed) > media_bytes {
                return Err(Mp4Error::Invalid("sample sizes exceed media data"));
            }
        }

        let stts = one(&table, b"stts")?.data;
        let nruns = entries(stts, 8)?;
        let mut timed = 0usize;
        for i in 0..nruns {
            let repeat = u32be(stts, 8 + i * 8)? as usize;
            let duration = u32be(stts, 12 + i * 8)?;
            if repeat == 0 || duration == 0 || duration > config.frame_length || repeat > count - timed {
                return Err(Mp4Error::Invalid("invalid ALAC time-to-sample entry"));
            }
            timed += repeat;
        }
        if timed != count { return Err(Mp4Error::Invalid("ALAC duration count mismatch")); }

        let chunk_atom = match (table.iter().find(|a| a.kind == *b"stco"), table.iter().find(|a| a.kind == *b"co64")) {
            (Some(a), None) => (*a, 4usize), (None, Some(a)) => (*a, 8usize),
            _ => return Err(Mp4Error::Invalid("expected one chunk-offset table")),
        };
        let nchunks = entries(chunk_atom.0.data, chunk_atom.1)?;
        let stsc = one(&table, b"stsc")?.data;
        let nmap = entries(stsc, 12)?;
        if (count == 0 && (nchunks != 0 || nmap != 0))
            || (count != 0 && (nchunks == 0 || nmap == 0)) {
            return Err(Mp4Error::Invalid("inconsistent empty sample table"));
        }
        if nchunks > count || nmap > nchunks {
            return Err(Mp4Error::Invalid("chunk count exceeds sample count"));
        }
        // Verify all runs and the total sample count before allocating an
        // index, rather than expanding a hostile run into per-sample vectors.
        let mut mapped = 0usize;
        let mut previous = 0u32;
        for i in 0..nmap {
            let first = u32be(stsc, 8 + 12 * i)?;
            let per = u32be(stsc, 12 + 12 * i)?;
            let description = u32be(stsc, 16 + 12 * i)?;
            let next = if i + 1 < nmap { u32be(stsc, 8 + 12 * (i + 1))? } else { nchunks as u32 + 1 };
            if first == 0 || first as usize > nchunks || per == 0 || description != 1
                || (i == 0 && first != 1) || first <= previous || next <= first || next as usize > nchunks + 1 {
                return Err(Mp4Error::Invalid("invalid sample-to-chunk entry"));
            }
            let samples = ((next - first) as usize).checked_mul(per as usize)
                .ok_or(Mp4Error::Invalid("chunk sample count overflows"))?;
            if samples > count - mapped { return Err(Mp4Error::Invalid("chunk table contains too many samples")); }
            mapped += samples;
            previous = first;
        }
        if mapped != count { return Err(Mp4Error::Invalid("chunk table contains too few samples")); }

        // Check each complete chunk against the sorted top-level mdat ranges,
        // and the aggregate sample bytes against all media data, before the
        // only per-packet allocation. Chunk overlap remains otherwise allowed.
        let mut map_index = 0usize;
        let mut sample_index = 0usize;
        let mut sample_bytes = 0u64;
        for chunk in 1..=nchunks {
            while map_index + 1 < nmap && u32be(stsc, 8 + 12 * (map_index + 1))? as usize <= chunk { map_index += 1; }
            let per = u32be(stsc, 12 + 12 * map_index)? as usize;
            let offset = if chunk_atom.1 == 4 { u64::from(u32be(chunk_atom.0.data, 8 + (chunk - 1) * 4)?) }
                else { u64be(chunk_atom.0.data, 8 + (chunk - 1) * 8)? };
            let chunk_bytes = if fixed != 0 { u64::from(fixed) * per as u64 } else {
                let mut bytes = 0u64;
                for index in sample_index..sample_index + per {
                    let size = sample_size(stsz, fixed, index)?;
                    if size < 3 { return Err(Mp4Error::Invalid("truncated ALAC packet")); }
                    bytes = bytes.checked_add(u64::from(size)).ok_or(Mp4Error::Invalid("chunk size overflows"))?;
                }
                bytes
            };
            sample_bytes = sample_bytes.checked_add(chunk_bytes).ok_or(Mp4Error::Invalid("sample sizes overflow"))?;
            if sample_bytes > media_bytes { return Err(Mp4Error::Invalid("sample sizes exceed media data")); }
            let end = offset.checked_add(chunk_bytes).ok_or(Mp4Error::Invalid("packet offset overflow"))?;
            let media_index = media.partition_point(|&(_, stop)| stop <= offset);
            if !media.get(media_index).is_some_and(|&(start, stop)| offset >= start && end <= stop) {
                return Err(Mp4Error::Invalid("ALAC packet lies outside media data"));
            }
            sample_index += per;
        }

        let mut packets = Vec::new();
        packets.try_reserve_exact(count).map_err(|_| Mp4Error::Unsupported("ALAC packet index allocation failed"))?;
        let mut map_index = 0usize;
        let mut time_index = 0usize;
        let mut time_remaining = 0usize;
        let mut duration = 0u32;
        for chunk in 1..=nchunks {
            while map_index + 1 < nmap && u32be(stsc, 8 + 12 * (map_index + 1))? as usize <= chunk { map_index += 1; }
            let per = u32be(stsc, 12 + 12 * map_index)? as usize;
            let mut offset = if chunk_atom.1 == 4 { u64::from(u32be(chunk_atom.0.data, 8 + (chunk - 1) * 4)?) }
                else { u64be(chunk_atom.0.data, 8 + (chunk - 1) * 8)? };
            for _ in 0..per {
                if time_remaining == 0 {
                    time_remaining = u32be(stts, 8 + time_index * 8)? as usize;
                    duration = u32be(stts, 12 + time_index * 8)?;
                    time_index += 1;
                }
                let size = sample_size(stsz, fixed, packets.len())?;
                packets.push(Packet { offset, size, frames: duration });
                offset += u64::from(size);
                time_remaining -= 1;
            }
        }
        if let Some(last) = packets.last_mut() {
            let actual = packet_frame_length(reader, *last, config.frame_length)?;
            if actual > config.frame_length { return Err(Mp4Error::Invalid("ALAC packet frame count exceeds cookie")); }
            if actual < last.frames {
                if actual != 0 || last.frames != config.frame_length {
                    return Err(Mp4Error::Unsupported("ALAC packet duration exceeds decoded frame count"));
                }
                // ExtAudioFile writes a zero-frame escape packet at exact
                // 4096-frame boundaries, but records a nominal stts duration
                // of 4096 for it. It contributes no decoded samples.
                last.frames = 0;
            }
        }
        let frames: u64 = packets.iter().map(|p| u64::from(p.frames)).sum();
        validate_identity_edit(&track_children, &movie_children, config.sample_rate, frames)?;
        if gapless_frames(&movie_children)?.is_some_and(|valid| valid != frames) {
            return Err(Mp4Error::Unsupported("gapless frame count requires trimming"));
        }
        selected = Some(AlacFile { config, channel_layout, frames, packets });
    }
    selected.ok_or(Mp4Error::Unsupported("no ALAC audio track"))
}

#[derive(Debug)]
pub enum WriterError { Io(io::Error), Invalid(&'static str), Unsupported(&'static str) }
impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "MP4 write: {e}"),
            Self::Invalid(s) => write!(f, "invalid ALAC output: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported ALAC output: {s}"),
        }
    }
}
impl std::error::Error for WriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Io(e) = self { Some(e) } else { None }
    }
}
impl From<io::Error> for WriterError { fn from(e: io::Error) -> Self { Self::Io(e) } }

#[derive(Debug, Clone, Copy)]
pub struct EncodedPacket { pub size: u32, pub frames: u32 }

fn put32(dst: &mut Vec<u8>, value: u32) { dst.extend_from_slice(&value.to_be_bytes()); }
fn atom(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    put32(&mut out, (data.len() + 8) as u32);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out
}
fn append(dst: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) { dst.extend_from_slice(&atom(kind, data)); }
fn matrix(dst: &mut Vec<u8>) {
    for x in [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] { put32(dst, x); }
}
fn movie_header(rate: u32, duration: u32, clock: u32) -> Vec<u8> {
    let mut d = Vec::new();
    for x in [0, clock, clock, rate, duration, 0x00010000, 0x01000000, 0, 0] { put32(&mut d, x); }
    matrix(&mut d);
    for _ in 0..6 { put32(&mut d, 0); }
    put32(&mut d, 2);
    atom(b"mvhd", &d)
}
fn track_header(duration: u32, clock: u32) -> Vec<u8> {
    let mut d = Vec::new();
    for x in [7, clock, clock, 1, 0, duration, 0, 0, 0, 0x01000000] { put32(&mut d, x); }
    matrix(&mut d);
    put32(&mut d, 0);
    put32(&mut d, 0);
    atom(b"tkhd", &d)
}
fn media_header(rate: u32, duration: u32, clock: u32) -> Vec<u8> {
    let mut d = Vec::new();
    for x in [0, clock, clock, rate, duration, 0] { put32(&mut d, x); }
    atom(b"mdhd", &d)
}
fn handler(kind: &[u8; 4], vendor: &[u8; 4]) -> Vec<u8> {
    let mut d = vec![0; 8];
    d.extend_from_slice(kind);
    d.extend_from_slice(vendor);
    d.extend_from_slice(&[0; 10]);
    atom(b"hdlr", &d)
}
fn sample_description(config: Config, max_frame: u32, average: u32) -> Vec<u8> {
    let mut config_data = Vec::new();
    put32(&mut config_data, 0);
    put32(&mut config_data, 4096);
    config_data.extend_from_slice(&[0, config.bit_depth, 40, 10, 14, config.channels, 0, 255]);
    put32(&mut config_data, max_frame);
    put32(&mut config_data, average);
    put32(&mut config_data, config.sample_rate);
    let mut entry = vec![0; 6];
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0; 8]);
    entry.extend_from_slice(&2u16.to_be_bytes());
    entry.extend_from_slice(&16u16.to_be_bytes());
    put32(&mut entry, 0);
    put32(&mut entry, 44100 << 16);
    append(&mut entry, b"alac", &config_data);
    if config.channels > 2 {
        let tag = match config.channels { 3 => 114, 4 => 116, 5 => 120, 6 => 124, 7 => 142, 8 => 127, _ => unreachable!() };
        let mut channel = vec![0; 4];
        put32(&mut channel, (tag << 16) | u32::from(config.channels));
        put32(&mut channel, 0);
        put32(&mut channel, 0);
        append(&mut entry, b"chan", &channel);
    }
    let mut d = vec![0; 4];
    put32(&mut d, 1);
    append(&mut d, b"alac", &entry);
    atom(b"stsd", &d)
}
fn gapless(valid: u64, remainder: u64) -> Vec<u8> {
    let text = format!(" 00000000 00000000 {remainder:08X} {valid:016X} 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000");
    let mut mean = vec![0; 4];
    mean.extend_from_slice(b"com.apple.iTunes");
    let mut name = vec![0; 4];
    name.extend_from_slice(b"iTunSMPB");
    let mut value = Vec::new();
    put32(&mut value, 1);
    put32(&mut value, 0);
    value.extend_from_slice(text.as_bytes());
    let mut freeform = Vec::new();
    append(&mut freeform, b"mean", &mean);
    append(&mut freeform, b"name", &name);
    append(&mut freeform, b"data", &value);
    let mut ilst = Vec::new();
    append(&mut ilst, b"----", &freeform);
    let mut meta = vec![0; 4];
    meta.extend_from_slice(&handler(b"mdir", b"appl"));
    append(&mut meta, b"ilst", &ilst);
    let mut udta = Vec::new();
    append(&mut udta, b"meta", &meta);
    atom(b"udta", &udta)
}

/// Write an untagged AudioToolbox-style M4A from sequential packet bytes.
/// `clock` is the close-time second since 1904, or the controlled test second.
/// Packet frame counts are actual decoded frames; a final zero-frame packet is
/// recorded with the nominal 4096-frame table duration, as AudioToolbox does.
/// The original `source_rate` determines the cookie bitrate; the serialized
/// sample rate and table timescale are its truncated integer.
pub fn write_alac_with_rate<W: Write, R: Read>(out: &mut W, source: &mut R, config: Config,
    source_rate: f64, packets: &[EncodedPacket], clock: u32) -> Result<(), WriterError> {
    if config.frame_length != 4096 || !matches!(config.bit_depth, 16 | 20 | 24 | 32)
        || !(1..=8).contains(&config.channels) || config.pb != 40 || config.mb != 10 || config.kb != 14 {
        return Err(WriterError::Unsupported("ALAC configuration"));
    }
    if !source_rate.is_finite() || !(22050.0..=96000.0).contains(&source_rate) || source_rate as u32 != config.sample_rate {
        return Err(WriterError::Unsupported("sample rate mismatch or outside verified range"));
    }
    // AudioToolbox reserves a 301-second sample table before the media data.
    // It computes chunk capacity with integer division and an additional
    // packet, including at rates exactly divisible by 8192. The constant is
    // the observed fixed box/reservation overhead for this ALAC layout.
    let per_chunk = config.sample_rate as usize / 8192 + 1;
    let estimated_packets = 301 * config.sample_rate as usize / 4096;
    let data_offset = (5537 + 4 * estimated_packets + 4 * (estimated_packets / per_chunk)) & !4095;
    if packets.len() > 1_000_000 { return Err(WriterError::Unsupported("too many packets")); }
    let mut bytes = 0u64;
    let mut actual = 0u64;
    let mut nominal = 0u64;
    let mut max_size = 0u32;
    for (i, p) in packets.iter().enumerate() {
        if p.size == 0 || p.frames > 4096 || (i + 1 < packets.len() && p.frames != 4096)
            || (p.frames == 0 && (i == 0 || i + 1 != packets.len())) {
            return Err(WriterError::Invalid("invalid packet shape"));
        }
        bytes += u64::from(p.size);
        actual += u64::from(p.frames);
        nominal += u64::from(if p.frames == 0 { 4096 } else { p.frames });
        max_size = max_size.max(p.size);
    }
    if bytes > u64::from(u32::MAX - 8) || nominal > u64::from(u32::MAX) { return Err(WriterError::Unsupported("large MP4")); }
    if packets.is_empty() { max_size = 4096 * u32::from(config.channels) * 5 + 1; }
    let average = if packets.is_empty() { 0 } else {
        let denominator = (packets.len() * 4096) as f64;
        let value = (source_rate * bytes as f64) * 8.0 / denominator;
        if value >= u32::MAX as f64 { return Err(WriterError::Unsupported("bitrate overflow")); }
        value as u32
    };
    let mut chunks = Vec::new();
    let mut offset = data_offset as u64;
    for group in packets.chunks(per_chunk) {
        chunks.push((offset, group.len()));
        for p in group { offset += u64::from(p.size); }
    }
    if offset > u64::from(u32::MAX) { return Err(WriterError::Unsupported("64-bit chunk offsets")); }
    let mut stts = vec![0; 4];
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for p in packets {
        let dur = if p.frames == 0 { 4096 } else { p.frames };
        if let Some(last) = runs.last_mut() {
            if last.1 == dur { last.0 += 1; continue; }
        }
        runs.push((1, dur));
    }
    put32(&mut stts, runs.len() as u32);
    for (count, dur) in runs { put32(&mut stts, count); put32(&mut stts, dur); }
    let mut stsc = vec![0; 4];
    let mut maps = Vec::new();
    for (i, (_, n)) in chunks.iter().enumerate() {
        if i == 0 || chunks[i - 1].1 != *n { maps.push((i as u32 + 1, *n as u32)); }
    }
    put32(&mut stsc, maps.len() as u32);
    for (first, n) in maps { put32(&mut stsc, first); put32(&mut stsc, n); put32(&mut stsc, 1); }
    let mut stsz = vec![0; 4];
    put32(&mut stsz, 0);
    put32(&mut stsz, packets.len() as u32);
    for p in packets { put32(&mut stsz, p.size); }
    let mut stco = vec![0; 4];
    put32(&mut stco, chunks.len() as u32);
    for (pos, _) in chunks { put32(&mut stco, pos as u32); }
    let mut stbl = sample_description(config, max_size, average);
    append(&mut stbl, b"stts", &stts);
    append(&mut stbl, b"stsc", &stsc);
    append(&mut stbl, b"stsz", &stsz);
    append(&mut stbl, b"stco", &stco);
    let mut dref = vec![0; 4];
    put32(&mut dref, 1);
    append(&mut dref, b"url ", &[0, 0, 0, 1]);
    let mut dinf = Vec::new();
    append(&mut dinf, b"dref", &dref);
    let mut minf = atom(b"smhd", &[0; 8]);
    append(&mut minf, b"dinf", &dinf);
    append(&mut minf, b"stbl", &stbl);
    let mut mdia = media_header(config.sample_rate, nominal as u32, clock);
    mdia.extend_from_slice(&handler(b"soun", &[0; 4]));
    append(&mut mdia, b"minf", &minf);
    let mut trak = track_header(nominal as u32, clock);
    append(&mut trak, b"mdia", &mdia);
    let mut moov = movie_header(config.sample_rate, nominal as u32, clock);
    append(&mut moov, b"trak", &trak);
    if !packets.is_empty() {
        let remainder = (packets.len() as u64) * 4096 - actual;
        moov.extend_from_slice(&gapless(actual, remainder));
    }
    let ftyp = atom(b"ftyp", b"M4A \0\0\0\0M4A mp42isom");
    let moov = atom(b"moov", &moov);
    let available = data_offset - 8 - ftyp.len();
    // AudioToolbox puts moov last when it does not fit, or when the leftover
    // front gap is 1–7 bytes (too short for a free atom). An exact fit is
    // valid and needs no free atom at all.
    let front_moov = packets.is_empty() || (moov.len() <= available
        && (available - moov.len() == 0 || available - moov.len() >= 8));
    out.write_all(&ftyp)?;
    if front_moov { out.write_all(&moov)?; }
    if !packets.is_empty() {
        let free_size = available - if front_moov { moov.len() } else { 0 };
        if free_size != 0 {
            out.write_all(&(free_size as u32).to_be_bytes())?;
            out.write_all(b"free")?;
            let zero = [0u8; 4096];
            let mut remaining = free_size - 8;
            while remaining > 0 {
                let n = remaining.min(zero.len());
                out.write_all(&zero[..n])?;
                remaining -= n;
            }
        }
        out.write_all(&((bytes as u32) + 8).to_be_bytes())?;
        out.write_all(b"mdat")?;
        let copied = io::copy(&mut source.take(bytes), out)?;
        if copied != bytes { return Err(WriterError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "encoded packet source truncated"))); }
        if !front_moov { out.write_all(&moov)?; }
    }
    Ok(())
}
