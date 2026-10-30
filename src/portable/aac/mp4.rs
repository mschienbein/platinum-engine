//! mp4: the `.m4a` file that AudioToolbox's ExtAudioFile (`m4af`, `'aac '` file format) writes around the access units of its AAC-LC encoder,
//! reproduced byte for byte from the access-unit sizes alone: `ftyp` `moov` `free` `mdat` (or `ftyp` `free` `mdat` `moov` when the sample
//! tables outgrow the space reserved in front of the media data).
//!
//! The layout rules (first sample offset, moov placement, chunking, esds peak bitrate, iTunSMPB, the empty-file layout) were observed in files
//! AudioToolbox wrote on one reference machine and are checked against them byte for byte. Pure integer logic; std only. The file is written
//! sequentially to any [`Write`] ([`write_m4a`], [`write_empty_m4a`]): only the sizes of the access units are held in memory, the access units
//! themselves are streamed from a [`Read`] (the writer spools them to disk while it encodes).
//!
//! Timestamps: Apple stamps mvhd/tkhd/mdhd creation and modification with the close-time second since 1904; `clock` is that second (the caller
//! supplies it, exactly as the ALAC writer of the engine does). The atoms that are identical in AudioToolbox's ALAC and AAC files (`ftyp`,
//! `mvhd`, `tkhd`, `mdhd`, `hdlr`, `minf` without `stbl`, the iTunSMPB `udta`) come from [`crate::portable::audio::mp4`]; the sample table
//! (`mp4a`/`esds` entry, `stts`/`stsc`) and the sizes of everything are AAC-specific.
#![forbid(unsafe_code)]

use std::fmt;
use std::io::{self, Read, Write};

use crate::portable::aac::bits::sf_index_of_rate;
use crate::portable::aac::fb::input::{FRAME_LEN, PRIMING_FRAMES as PRIMING};
use crate::portable::audio::mp4::{append, atom, ftyp, gapless, movie, packets_per_chunk, put32, reserved_data_offset, sound_entry, write_free};

/// Samples per AAC access unit.
pub const AU_FRAMES: u32 = FRAME_LEN as u32;
/// Encoder priming that iTunSMPB reports (`0x840`).
pub const PRIMING_FRAMES: u32 = PRIMING as u32;
/// `ftyp` is a constant 28 bytes.
pub const FTYP_LEN: usize = 28;

/// A stream the container cannot describe, or inconsistent parameters.
#[derive(Debug, PartialEq, Eq)]
pub enum Mp4Error {
    /// no sampling-frequency row, reservation constant or peak-bitrate statistic exists for this rate: a file with access units is laid out for 16, 22.05,
    /// 24, 32, 44.1 and 48 kHz (8, 11.025 and 12 kHz have a reservation constant and a default bitrate, which serve the empty file, but no measured tail rule)
    UnsupportedRate(u32),
    /// no `channelConfiguration` / default bitrate exists for this channel count (1..=8 are laid out)
    UnsupportedChannels(u32),
    /// no access unit was given to [`write_m4a`] (an empty stream is [`write_empty_m4a`])
    NoAccessUnits,
    /// more access units than the 32-bit tables and the 32-bit media duration (`1024 * access units`) can describe
    TooManyAccessUnits(u64),
    /// the first sample offset plus the media data does not fit the 32-bit `stco` offsets / `mdat` size; the 64-bit variants are not produced
    FileTooLarge,
    /// the access-unit count does not belong to the number of client frames (caller bug)
    InconsistentFrameCount { frames: u64, access_units: usize },
    /// ASC longer than the 7-bit descriptor length field allows (cannot happen for real configs)
    AscTooLong,
}

impl fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedRate(rate) => write!(f, "no AAC file layout is verified for sample rate {rate} Hz"),
            Self::UnsupportedChannels(channels) => write!(f, "no AAC file layout exists for {channels} channels"),
            Self::NoAccessUnits => f.write_str("no access unit to put into the file"),
            Self::TooManyAccessUnits(n) => write!(f, "{n} access units exceed the 32-bit duration of the file"),
            Self::FileTooLarge => f.write_str("the file would need 64-bit chunk offsets or a 64-bit mdat size"),
            Self::InconsistentFrameCount { frames, access_units } => write!(f, "{access_units} access units do not belong to {frames} client frames"),
            Self::AscTooLong => f.write_str("AudioSpecificConfig is too long for its descriptor"),
        }
    }
}
impl std::error::Error for Mp4Error {}

/// A layout error, or a failure of the destination or of the access-unit source while streaming.
#[derive(Debug)]
pub enum WriteError {
    Layout(Mp4Error),
    /// writing the file failed
    Output(io::Error),
    /// reading the access units failed, or the source holds fewer bytes than the sizes announce
    Source(io::Error),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(e) => write!(f, "{e}"),
            Self::Output(e) => write!(f, "writing the file: {e}"),
            Self::Source(e) => write!(f, "reading the access units: {e}"),
        }
    }
}
impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Layout(e) => Some(e),
            Self::Output(e) | Self::Source(e) => Some(e),
        }
    }
}
impl From<Mp4Error> for WriteError {
    fn from(e: Mp4Error) -> Self {
        Self::Layout(e)
    }
}

/// Everything the file writer needs besides the access units.
#[derive(Clone, Copy, Debug)]
pub struct FileParams<'a> {
    pub sample_rate: u32,
    /// client frames written to the file (the iTunSMPB valid-frame count)
    pub input_frames: u64,
    /// `AudioSpecificConfig` bytes (see [`asc_simple`], [`asc_with_pce`])
    pub asc: &'a [u8],
    /// close-time second since 1904-01-01
    pub clock: u32,
    /// esds average bitrate: the `'brat'` of the stream, or [`default_bitrate`] for an empty file
    pub avg_bitrate: u32,
}

/// Number of trailing access units the writer leaves out of the peak-bitrate statistic (measured at every rate that has access units).
pub fn tail_exclude(rate: u32) -> Option<usize> {
    Some(match rate {
        16000 => 1,
        22050 => 11,
        24000 => 9,
        32000 => 1,
        44100 => 21,
        48000 => 18,
        _ => return None,
    })
}

/// Packets per chunk (about half a second of audio).
pub fn chunk_packets(rate: u32) -> usize {
    packets_per_chunk(rate, AU_FRAMES)
}

/// First sample byte offset: the space reserved for `ftyp`+`moov`+`free` (`n` = packets in 301 s; verified for 9 rates 8 kHz..48 kHz), computed as for
/// ALAC with 1024-frame packets ([`reserved_data_offset`]).
pub fn data_offset(rate: u32) -> u64 {
    reserved_data_offset(rate, AU_FRAMES)
}

/// Peak bits over windows of `floor(rate/1024)` consecutive AUs (shorter at the start) ending before the last [`tail_exclude`] AUs.
pub fn max_bitrate(sizes: &[u32], rate: u32) -> Result<u32, Mp4Error> {
    let w = (rate / 1024) as usize;
    let end = sizes.len().saturating_sub(tail_exclude(rate).ok_or(Mp4Error::UnsupportedRate(rate))?);
    let mut best: u64 = 0;
    let mut sum: u64 = 0;
    for i in 0..end {
        sum += u64::from(sizes[i]);
        if i >= w {
            sum -= u64::from(sizes[i - w]);
        }
        best = best.max(sum);
    }
    u32::try_from(best * 8).map_err(|_| Mp4Error::FileTooLarge)
}

/// Channel configuration Apple signals in the ASC for `channels` (0 = in-band program_config_element, ASC carries the PCE).
pub fn channel_configuration(channels: u32) -> Option<u8> {
    Some(match channels {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 0,
        5 => 5,
        6 => 6,
        7 => 11,
        8 => 7,
        _ => return None,
    })
}

/// `AudioSpecificConfig` for configurations without a PCE (AOT 2, frame length 1024, no core coder, no extension).
pub fn asc_simple(rate: u32, channels: u32) -> Option<Vec<u8>> {
    let sfi = u16::from(sf_index_of_rate(rate)?);
    let cc = u16::from(channel_configuration(channels)?);
    if cc == 0 {
        return None;
    }
    let v = (2u16 << 11) | (sfi << 7) | (cc << 3);
    Some(v.to_be_bytes().to_vec())
}

/// ASC with `chcfg = 0` followed by the PCE bit string (`pce`, `nbits` valid bits MSB-first, starting at the element_instance_tag), zero padded.
pub fn asc_with_pce(rate: u32, pce: &[u8], nbits: usize) -> Option<Vec<u8>> {
    let sfi = u16::from(sf_index_of_rate(rate)?);
    let v = (2u16 << 11) | (sfi << 7);
    let mut out = v.to_be_bytes().to_vec();
    out.extend_from_slice(pce.get(..nbits.div_ceil(8))?);
    if !nbits.is_multiple_of(8) {
        let last = out.len() - 1;
        out[last] &= 0xFFu8 << (8 - nbits % 8);
    }
    Some(out)
}

/// Apple's default `'brat'` for the file ASBD (rate, channels) = what an empty file's esds carries: 64000 per unit of {1,2,3,4,5,5,6,7} channels at 44.1/48 kHz,
/// scaled by 3/4 at 32 kHz, 1/2 at 24/22.05 kHz, 3/8 at 16 kHz, 5/16 at 12/11.025 kHz and 1/4 at 8 kHz (measured at all 9 rates x 8 channel counts).
pub fn default_bitrate(rate: u32, channels: u32) -> Option<u32> {
    let units = *[1u32, 2, 3, 4, 5, 5, 6, 7].get(channels.checked_sub(1)? as usize)?;
    let base = 64000 * units;
    Some(match rate {
        44100 | 48000 => base,
        32000 => base / 4 * 3,
        24000 | 22050 => base / 2,
        16000 => base / 8 * 3,
        12000 | 11025 => base / 16 * 5,
        8000 => base / 4,
        _ => return None,
    })
}

/// Do `access_units` access units holding `mdat_bytes` bytes of a `rate` Hz stream fit the 32-bit file structures? The duration
/// `1024 * access_units` (mvhd/tkhd/mdhd, 32 bits) and the end of the media data (`stco` offsets and the `mdat` size, 32 bits) must not overflow.
/// A writer that spools access units calls this with its running totals so that an oversized stream fails when it grows past the limit.
pub fn check_limits(rate: u32, access_units: u64, mdat_bytes: u64) -> Result<(), Mp4Error> {
    if access_units.checked_mul(u64::from(AU_FRAMES)).is_none_or(|duration| duration > u64::from(u32::MAX)) {
        return Err(Mp4Error::TooManyAccessUnits(access_units));
    }
    if mdat_bytes.checked_add(data_offset(rate)).is_none_or(|end| end > u64::from(u32::MAX)) {
        return Err(Mp4Error::FileTooLarge);
    }
    Ok(())
}

/// `descriptor` header of the esds: tag, three `0x80` continuation bytes, 7-bit length.
fn descriptor(tag: u8, len: usize) -> Result<[u8; 5], Mp4Error> {
    if len > 0x7F {
        return Err(Mp4Error::AscTooLong);
    }
    Ok([tag, 0x80, 0x80, 0x80, len as u8])
}

/// `stsd` with the `mp4a` entry and its `esds` (AAC-specific: the ALAC entry has other codec boxes and a fixed 44100 in the rate field).
fn sample_description(p: &FileParams<'_>, max_bitrate: u32) -> Result<Vec<u8>, Mp4Error> {
    let asc = p.asc;
    let mut esds = vec![0; 4];
    esds.extend_from_slice(&descriptor(3, 32 + asc.len())?);
    esds.extend_from_slice(&[0, 0, 0]);
    esds.extend_from_slice(&descriptor(4, 18 + asc.len())?);
    esds.extend_from_slice(&[0x40, 0x14]);
    esds.extend_from_slice(&6144u32.to_be_bytes()[1..]);
    put32(&mut esds, max_bitrate);
    put32(&mut esds, p.avg_bitrate);
    esds.extend_from_slice(&descriptor(5, asc.len())?);
    esds.extend_from_slice(asc);
    esds.extend_from_slice(&descriptor(6, 1)?);
    esds.push(2);
    let rate = u16::try_from(p.sample_rate).map_err(|_| Mp4Error::UnsupportedRate(p.sample_rate))?;
    // the channel count of the entry is always 2: the real layout lives in the ASC
    let mut entry = sound_entry(rate);
    append(&mut entry, b"esds", &esds);
    let mut stsd = vec![0; 4];
    put32(&mut stsd, 1);
    append(&mut stsd, b"mp4a", &entry);
    Ok(atom(b"stsd", &stsd))
}

/// The `moov` atom for `sizes` (one entry per access unit; none = the empty file) and the chunk offsets derived from them.
fn moov(sizes: &[u32], chunk_offsets: &[u32], p: &FileParams<'_>) -> Result<Vec<u8>, Mp4Error> {
    let n = sizes.len() as u32;
    let duration = n * AU_FRAMES;
    // an empty stream never gets its statistics: Apple leaves the creation-time placeholders (default bitrate) in both fields
    let max = if sizes.is_empty() { p.avg_bitrate } else { max_bitrate(sizes, p.sample_rate)? };
    let mut stbl = sample_description(p, max)?;

    let mut stts = vec![0; 4];
    if n == 0 {
        put32(&mut stts, 0);
    } else {
        put32(&mut stts, 1);
        put32(&mut stts, n);
        put32(&mut stts, AU_FRAMES);
    }
    append(&mut stbl, b"stts", &stts);

    let per = chunk_packets(p.sample_rate);
    let (full_chunks, rem) = (sizes.len() / per, sizes.len() % per);
    let mut stsc = vec![0; 4];
    put32(&mut stsc, u32::from(full_chunks > 0) + u32::from(rem > 0));
    if full_chunks > 0 {
        for v in [1, per as u32, 1] {
            put32(&mut stsc, v);
        }
    }
    if rem > 0 {
        for v in [full_chunks as u32 + 1, rem as u32, 1] {
            put32(&mut stsc, v);
        }
    }
    append(&mut stbl, b"stsc", &stsc);

    let mut stsz = vec![0; 4];
    put32(&mut stsz, 0);
    put32(&mut stsz, n);
    sizes.iter().for_each(|&s| put32(&mut stsz, s));
    append(&mut stbl, b"stsz", &stsz);

    let mut stco = vec![0; 4];
    put32(&mut stco, chunk_offsets.len() as u32);
    chunk_offsets.iter().for_each(|&c| put32(&mut stco, c));
    append(&mut stbl, b"stco", &stco);

    // no udta/iTunSMPB in an empty file
    let udta = if sizes.is_empty() {
        Vec::new()
    } else {
        let used = u64::from(PRIMING_FRAMES) + p.input_frames;
        if used > u64::from(duration) || u64::from(duration) - used >= u64::from(AU_FRAMES) {
            return Err(Mp4Error::InconsistentFrameCount { frames: p.input_frames, access_units: sizes.len() });
        }
        gapless(PRIMING_FRAMES, p.input_frames, u64::from(duration) - used)
    };
    Ok(movie(p.sample_rate, duration, p.clock, &stbl, &udta))
}

/// Copy exactly `bytes` bytes from `source` to `out`; a short source is an error of the source.
fn stream_media<W: Write, R: Read>(out: &mut W, source: &mut R, mut bytes: u64) -> Result<(), WriteError> {
    let mut buffer = vec![0u8; 64 * 1024];
    while bytes > 0 {
        let want = buffer.len().min(usize::try_from(bytes).unwrap_or(usize::MAX));
        let got = loop {
            match source.read(&mut buffer[..want]) {
                Ok(0) => return Err(WriteError::Source(io::Error::new(io::ErrorKind::UnexpectedEof, "the access-unit source ends before the announced sizes are read"))),
                Ok(k) => break k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(WriteError::Source(e)),
            }
        };
        out.write_all(&buffer[..got]).map_err(WriteError::Output)?;
        bytes -= got as u64;
    }
    Ok(())
}

/// The file Apple writes when no frame was ever written: `ftyp` + `moov` with empty tables, no `free`/`mdat`/`udta`; durations 0.
/// `p.avg_bitrate` must be [`default_bitrate`] (the creation-time esds placeholder, not the `'brat'` set later) and `p.input_frames` 0.
pub fn write_empty_m4a<W: Write>(out: &mut W, p: &FileParams<'_>) -> Result<(), WriteError> {
    if p.input_frames != 0 {
        return Err(Mp4Error::InconsistentFrameCount { frames: p.input_frames, access_units: 0 }.into());
    }
    out.write_all(&ftyp()).map_err(WriteError::Output)?;
    out.write_all(&moov(&[], &[], p)?).map_err(WriteError::Output)
}

/// Write the complete `.m4a` for the access units whose sizes are `sizes`, in order; their bytes (`sum(sizes)` in total) are read from `mdat`.
///
/// The output is produced strictly sequentially: `ftyp`, then `moov` + `free` when the sample tables fit in front of the first sample offset
/// ([`data_offset`]) and a `free` atom alone otherwise, then `mdat` (header and media data), then the trailing `moov` of the second case.
/// Streams whose offsets or sizes need more than 32 bits are refused ([`check_limits`]) before anything is written.
pub fn write_m4a<W: Write, R: Read>(out: &mut W, mdat: &mut R, sizes: &[u32], p: &FileParams<'_>) -> Result<(), WriteError> {
    if sizes.is_empty() {
        return Err(Mp4Error::NoAccessUnits.into());
    }
    let total: u64 = sizes.iter().map(|&s| u64::from(s)).sum();
    check_limits(p.sample_rate, sizes.len() as u64, total)?;
    let off = data_offset(p.sample_rate);
    let per = chunk_packets(p.sample_rate);
    let mut chunk_offsets = Vec::with_capacity(sizes.len().div_ceil(per));
    let mut pos = off;
    for chunk in sizes.chunks(per) {
        chunk_offsets.push(pos as u32);
        pos += chunk.iter().map(|&s| u64::from(s)).sum::<u64>();
    }
    let moov = moov(sizes, &chunk_offsets, p)?;

    let free_total = off as usize - 8 - FTYP_LEN; // everything between ftyp and the mdat header
    let in_front = free_total >= moov.len() + 8;
    out.write_all(&ftyp()).map_err(WriteError::Output)?;
    let free_size = if in_front { free_total - moov.len() } else { free_total };
    if in_front {
        out.write_all(&moov).map_err(WriteError::Output)?;
    }
    write_free(out, free_size).map_err(WriteError::Output)?;
    out.write_all(&((8 + total) as u32).to_be_bytes()).map_err(WriteError::Output)?;
    out.write_all(b"mdat").map_err(WriteError::Output)?;
    stream_media(out, mdat, total)?;
    if !in_front {
        out.write_all(&moov).map_err(WriteError::Output)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASC_44100_2: [u8; 2] = [0x12, 0x10];

    fn params(rate: u32, frames: u64) -> FileParams<'static> {
        FileParams { sample_rate: rate, input_frames: frames, asc: &ASC_44100_2, clock: 0x1234_5678, avg_bitrate: 256_000 }
    }

    /// Deterministic access units: AU `i` is `size(i)` bytes of `i as u8`.
    fn units(n: usize, size: impl Fn(usize) -> u32) -> (Vec<u32>, Vec<u8>) {
        let sizes: Vec<u32> = (0..n).map(&size).collect();
        let mut bytes = Vec::new();
        for (i, &s) in sizes.iter().enumerate() {
            bytes.extend(std::iter::repeat_n(i as u8, s as usize));
        }
        (sizes, bytes)
    }

    fn build(rate: u32, frames: u64, sizes: &[u32], bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_m4a(&mut out, &mut &bytes[..], sizes, &params(rate, frames)).unwrap();
        out
    }

    /// Top-level atoms: (type, start, size).
    fn top_level(file: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut atoms = Vec::new();
        let mut at = 0;
        while at < file.len() {
            let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
            atoms.push((file[at + 4..at + 8].try_into().unwrap(), at, size));
            at += size;
        }
        assert_eq!(at, file.len(), "atoms tile the file");
        atoms
    }

    fn kinds(file: &[u8]) -> Vec<String> {
        top_level(file).iter().map(|(k, _, _)| String::from_utf8_lossy(k).into_owned()).collect()
    }

    /// First occurrence of `needle` in `hay`.
    fn find(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).position(|w| w == needle).expect("pattern present")
    }

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn data_offsets_match_apple() {
        let apple = [(8000, 16384), (11025, 20480), (12000, 20480), (16000, 24576), (22050, 32768), (24000, 32768), (32000, 45056), (44100, 57344), (48000, 61440)];
        for (r, o) in apple {
            assert_eq!(data_offset(r), o, "rate {r}");
        }
    }

    #[test]
    fn default_bitrates() {
        assert_eq!(default_bitrate(44100, 2), Some(128000));
        assert_eq!(default_bitrate(48000, 8), Some(448000));
        assert_eq!(default_bitrate(32000, 4), Some(192000));
        assert_eq!(default_bitrate(22050, 6), Some(160000));
        assert_eq!(default_bitrate(16000, 8), Some(168000));
        assert_eq!(default_bitrate(12000, 5), Some(100000));
        assert_eq!(default_bitrate(11025, 8), Some(140000));
        assert_eq!(default_bitrate(8000, 8), Some(112000));
        assert_eq!(default_bitrate(7350, 2), None);
        assert_eq!(default_bitrate(44100, 0), None);
        assert_eq!(default_bitrate(44100, 9), None);
    }

    #[test]
    fn simple_asc() {
        assert_eq!(asc_simple(44100, 2).unwrap(), [0x12, 0x10]);
        assert_eq!(asc_simple(44100, 1).unwrap(), [0x12, 0x08]);
        assert_eq!(asc_simple(44100, 7).unwrap(), [0x12, 0x58]);
        assert_eq!(asc_simple(44100, 8).unwrap(), [0x12, 0x38]);
        assert_eq!(asc_simple(44100, 6).unwrap(), [0x12, 0x30]);
        assert_eq!(asc_simple(48000, 5).unwrap(), [0x11, 0xA8]);
        assert!(asc_simple(44100, 4).is_none());
    }

    #[test]
    fn peak_bitrate_windows() {
        // 44.1 kHz: window 43, tail 21: n=22 -> only AU0 counts
        let mut s = vec![6u32; 22];
        s[21] = 5000;
        assert_eq!(max_bitrate(&s, 44100), Ok(48));
        assert_eq!(max_bitrate(&s[..21], 44100), Ok(0));
        assert_eq!(max_bitrate(&s, 7350), Err(Mp4Error::UnsupportedRate(7350)));
    }

    #[test]
    fn peak_bitrate_window_slides_and_excludes_the_tail() {
        // window of 43 AUs, the last 21 AUs are not counted: with 100 AUs the windows end at AU 0..=78
        let mut s = vec![100u32; 100];
        s[78] = 1100; // inside: the window ending at 78 holds 42 * 100 + 1100
        s[79] = 90_000; // excluded tail
        assert_eq!(max_bitrate(&s, 44100), Ok((42 * 100 + 1100) * 8));
        // a spike more than a window behind a later AU drops out of the sum
        let mut t = vec![0u32; 100];
        t[0] = 1000;
        t[50] = 10;
        assert_eq!(max_bitrate(&t, 44100), Ok(1000 * 8));
        // 48 kHz: window 46, tail 18
        let u = vec![7u32; 200];
        assert_eq!(max_bitrate(&u, 48000), Ok(46 * 7 * 8));
    }

    #[test]
    fn limits_are_32_bit() {
        assert_eq!(check_limits(44100, 4_194_303, 0), Ok(()));
        assert_eq!(check_limits(44100, 4_194_304, 0), Err(Mp4Error::TooManyAccessUnits(4_194_304)));
        assert_eq!(check_limits(44100, u64::MAX, 0), Err(Mp4Error::TooManyAccessUnits(u64::MAX)));
        let room = u64::from(u32::MAX) - data_offset(44100);
        assert_eq!(check_limits(44100, 10, room), Ok(()));
        assert_eq!(check_limits(44100, 10, room + 1), Err(Mp4Error::FileTooLarge));
        assert_eq!(check_limits(44100, 10, u64::MAX), Err(Mp4Error::FileTooLarge));
    }

    #[test]
    fn oversized_streams_are_refused_before_anything_is_written() {
        let mut out = Vec::new();
        let sizes = [0x7FFF_FFFFu32, 0x7FFF_FFFF, 0x7FFF_FFFF];
        let err = write_m4a(&mut out, &mut io::empty(), &sizes, &params(44100, 1)).unwrap_err();
        assert!(matches!(err, WriteError::Layout(Mp4Error::FileTooLarge)), "{err:?}");
        assert!(out.is_empty());
        let many = vec![1u32; 4_194_304];
        let err = write_m4a(&mut out, &mut io::empty(), &many, &params(44100, 1)).unwrap_err();
        assert!(matches!(err, WriteError::Layout(Mp4Error::TooManyAccessUnits(4_194_304))), "{err:?}");
        assert!(out.is_empty());
        // exactly 4 GiB of media data does not fit either, one byte less than the room does (checked, not streamed)
        let room = u64::from(u32::MAX) - data_offset(44100);
        assert_eq!(check_limits(44100, 1, room), Ok(()));
    }

    #[test]
    fn empty_file_is_ftyp_and_moov() {
        let p = FileParams { avg_bitrate: 128_000, ..params(44100, 0) };
        let mut out = Vec::new();
        write_empty_m4a(&mut out, &p).unwrap();
        assert_eq!(kinds(&out), ["ftyp", "moov"]);
        assert_eq!(&out[..FTYP_LEN], &ftyp()[..]);
        assert!(!out.windows(4).any(|w| w == b"mdat" || w == b"free" || w == b"udta"));
        // esds: max = avg = the default bitrate, durations 0, empty tables
        let e = find(&out, b"esds");
        assert_eq!(be32(&out, e + 4 + 4 + 5 + 3 + 5 + 2 + 3), 128_000);
        assert_eq!(be32(&out, e + 4 + 4 + 5 + 3 + 5 + 2 + 3 + 4), 128_000);
        let mvhd = find(&out, b"mvhd");
        assert_eq!(be32(&out, mvhd + 4 + 4 + 8), 44100, "timescale");
        assert_eq!(be32(&out, mvhd + 4 + 4 + 12), 0, "duration");
        for table in [&b"stts"[..], b"stsc", b"stco"] {
            let at = find(&out, table);
            assert_eq!(be32(&out, at - 4), 16, "{table:?} is an empty full box");
            assert_eq!(be32(&out, at + 8), 0);
        }
        let stsz = find(&out, b"stsz");
        assert_eq!(be32(&out, stsz - 4), 20);
        assert_eq!((be32(&out, stsz + 8), be32(&out, stsz + 12)), (0, 0));
        // an empty file has no frames
        assert!(matches!(write_empty_m4a(&mut Vec::new(), &params(44100, 1)), Err(WriteError::Layout(Mp4Error::InconsistentFrameCount { .. }))));
        assert!(matches!(write_m4a(&mut Vec::new(), &mut io::empty(), &[], &params(44100, 0)), Err(WriteError::Layout(Mp4Error::NoAccessUnits))));
    }

    /// Smallest AU count at `rate` whose moov no longer fits in front (every AU adds 4 bytes to `stsz`, every chunk 4 to `stco`).
    fn first_trailing_moov(rate: u32) -> usize {
        let front = |n: usize| {
            let (sizes, bytes) = units(n, |_| 3);
            kinds(&build(rate, (n as u64 - 3) * 1024, &sizes, &bytes)) == ["ftyp", "moov", "free", "mdat"]
        };
        let (mut lo, mut hi) = (10usize, 60_000usize);
        assert!(front(lo) && !front(hi));
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if front(mid) { lo = mid } else { hi = mid }
        }
        hi
    }

    #[test]
    fn moov_moves_behind_the_media_exactly_when_the_reservation_is_full() {
        for rate in [44100u32, 48000, 32000] {
            let free_total = data_offset(rate) as usize - 8 - FTYP_LEN;
            let first_end = first_trailing_moov(rate);
            for n in first_end - 3..first_end + 3 {
                let (sizes, bytes) = units(n, |_| 3);
                let file = build(rate, (n as u64 - 3) * 1024, &sizes, &bytes);
                let moov_len = top_level(&file).iter().find(|(kind, _, _)| kind == b"moov").unwrap().2;
                // in front iff moov plus a free atom of at least 8 bytes fits in the reservation
                let in_front = free_total >= moov_len + 8;
                assert_eq!(in_front, n < first_end, "{rate} Hz, n = {n}");
                let want = if in_front { ["ftyp", "moov", "free", "mdat"] } else { ["ftyp", "free", "mdat", "moov"] };
                assert_eq!(kinds(&file), want, "{rate} Hz, n = {n}");
                check_layout(&file, rate, &sizes, &bytes);
            }
            // recorded with Apple: the files of 13,509 (44.1 kHz) and 14,541 (48 kHz) access units are the first with the moov behind the media data
            match rate {
                44100 => assert_eq!(first_end, 13_509),
                48000 => assert_eq!(first_end, 14_541),
                _ => assert!((10_000..11_000).contains(&first_end), "{rate} Hz switches at {first_end}"),
            }
        }
    }

    /// Check the structure shared by both placements and return (moov start, mdat payload start).
    fn check_layout(file: &[u8], rate: u32, sizes: &[u32], bytes: &[u8]) -> (usize, usize) {
        let atoms = top_level(file);
        let off = data_offset(rate) as usize;
        let mdat = atoms.iter().find(|(k, _, _)| k == b"mdat").unwrap();
        assert_eq!(mdat.1 + 8, off, "media data starts at the reserved first sample offset");
        assert_eq!(mdat.2, 8 + bytes.len());
        assert_eq!(&file[mdat.1 + 8..mdat.1 + mdat.2], bytes, "mdat holds the access units back to back");
        let moov = atoms.iter().find(|(k, _, _)| k == b"moov").unwrap();
        // chunk table: p AUs per chunk, offsets point at the first AU of every chunk
        let per = chunk_packets(rate);
        let stco = moov.1 + find(&file[moov.1..moov.1 + moov.2], b"stco");
        let count = be32(file, stco + 8) as usize;
        assert_eq!(count, sizes.len().div_ceil(per));
        let mut want = off as u32;
        for (c, chunk) in sizes.chunks(per).enumerate() {
            assert_eq!(be32(file, stco + 12 + 4 * c), want, "chunk {c}");
            want += chunk.iter().sum::<u32>();
        }
        (moov.1, mdat.1 + 8)
    }

    #[test]
    fn both_placements_hold_the_same_media_and_chunk_offsets() {
        for (n, expect_front) in [(300usize, true), (20_000, false)] {
            let (sizes, bytes) = units(n, |i| 5 + (i as u32 % 7));
            let file = build(44100, (n as u64 - 3) * 1024 + 17, &sizes, &bytes);
            let (moov_start, _) = check_layout(&file, 44100, &sizes, &bytes);
            assert_eq!(moov_start == FTYP_LEN, expect_front, "n = {n}");
            // the free atom fills the rest of the reservation with zeros
            let free = find(&file, b"free") - 4;
            let size = be32(&file, free) as usize;
            assert!(file[free + 8..free + size].iter().all(|&b| b == 0));
            // streaming in odd pieces gives the same bytes
            struct Trickle<'a>(&'a [u8]);
            impl Read for Trickle<'_> {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    let k = buf.len().min(self.0.len()).min(7);
                    buf[..k].copy_from_slice(&self.0[..k]);
                    self.0 = &self.0[k..];
                    Ok(k)
                }
            }
            let mut again = Vec::new();
            write_m4a(&mut again, &mut Trickle(&bytes), &sizes, &params(44100, (n as u64 - 3) * 1024 + 17)).unwrap();
            assert_eq!(again, file);
        }
    }

    #[test]
    fn chunking_follows_the_half_second_rule() {
        // 44.1 kHz: 22 AUs per chunk; 50 AUs = 2 full chunks + 6; the stsc table has one entry for the full chunks and one for the remainder
        let (sizes, bytes) = units(50, |i| 10 + i as u32);
        let file = build(44100, 47 * 1024, &sizes, &bytes);
        check_layout(&file, 44100, &sizes, &bytes);
        let stsc = find(&file, b"stsc");
        assert_eq!(be32(&file, stsc + 8), 2);
        assert_eq!([be32(&file, stsc + 12), be32(&file, stsc + 16), be32(&file, stsc + 20)], [1, 22, 1]);
        assert_eq!([be32(&file, stsc + 24), be32(&file, stsc + 28), be32(&file, stsc + 32)], [3, 6, 1]);
        // an exact multiple has no remainder entry
        let (sizes, bytes) = units(44, |_| 9);
        let file = build(44100, 41 * 1024, &sizes, &bytes);
        let stsc = find(&file, b"stsc");
        assert_eq!(be32(&file, stsc + 8), 1);
        // fewer AUs than one chunk: only the remainder entry, starting at chunk 1
        let (sizes, bytes) = units(5, |_| 9);
        let file = build(44100, 2 * 1024, &sizes, &bytes);
        let stsc = find(&file, b"stsc");
        assert_eq!(be32(&file, stsc + 8), 1);
        assert_eq!([be32(&file, stsc + 12), be32(&file, stsc + 16)], [1, 5]);
        // 48 kHz packs 24 AUs, 16 kHz 8
        assert_eq!(chunk_packets(48000), 24);
        assert_eq!(chunk_packets(16000), 8);
    }

    #[test]
    fn itunsmpb_text_and_durations() {
        let (sizes, bytes) = units(7, |_| 11);
        let file = build(44100, 4096, &sizes, &bytes);
        let data = find(&file, b"data");
        let size = be32(&file, data - 4) as usize;
        let text = std::str::from_utf8(&file[data + 4 + 8..data - 4 + size]).unwrap();
        assert_eq!(text, " 00000000 00000840 000003C0 0000000000001000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000");
        assert_eq!(be32(&file, data + 4), 1, "UTF-8 text type");
        // the tag sits in udta/meta/ilst/---- behind mean = com.apple.iTunes and name = iTunSMPB
        assert!(find(&file, b"com.apple.iTunes") < find(&file, b"iTunSMPB") && find(&file, b"iTunSMPB") < data);
        // durations: 7 AUs of 1024 frames in mvhd, tkhd and mdhd
        for header in [&b"mvhd"[..], b"mdhd"] {
            let at = find(&file, header);
            assert_eq!(be32(&file, at + 4 + 4 + 8), 44100, "{header:?} timescale");
            assert_eq!(be32(&file, at + 4 + 4 + 12), 7 * 1024, "{header:?} duration");
        }
        let tkhd = find(&file, b"tkhd");
        assert_eq!(be32(&file, tkhd + 4 + 4 + 12 + 4), 7 * 1024);
        // the close-time clock is stamped into all three headers
        for header in [&b"mvhd"[..], b"tkhd", b"mdhd"] {
            let at = find(&file, header);
            assert_eq!((be32(&file, at + 8), be32(&file, at + 12)), (0x1234_5678, 0x1234_5678), "{header:?}");
        }
    }

    #[test]
    fn itunsmpb_counts_must_match_the_access_units() {
        let (sizes, bytes) = units(7, |_| 11);
        // 7 AUs hold 7168 - 2112 = 5056 frames, the last AU is partly used: 4033..=5056 frames
        for ok in [4033u64, 5056] {
            assert!(write_m4a(&mut Vec::new(), &mut &bytes[..], &sizes, &params(44100, ok)).is_ok(), "{ok}");
        }
        for bad in [0u64, 4032, 5057, 100_000] {
            let err = write_m4a(&mut Vec::new(), &mut &bytes[..], &sizes, &params(44100, bad)).unwrap_err();
            assert!(matches!(err, WriteError::Layout(Mp4Error::InconsistentFrameCount { .. })), "{bad}: {err:?}");
        }
    }

    #[test]
    fn esds_peak_bitrate_and_average() {
        // 100 AUs at 44.1 kHz: the windows end before AU 79 (tail 21)
        let (mut sizes, _) = units(100, |_| 100);
        sizes[78] = 1100;
        sizes[79] = 60_000;
        let bytes = vec![0u8; sizes.iter().map(|&s| s as usize).sum()];
        let file = build(44100, 97 * 1024, &sizes, &bytes);
        let e = find(&file, b"esds");
        let at = e + 4 + 4 + 5 + 3 + 5 + 2 + 3;
        assert_eq!(be32(&file, at), (42 * 100 + 1100) * 8, "peak over a 43-AU window, tail excluded");
        assert_eq!(be32(&file, at + 4), 256_000, "average = the 'brat' of the stream");
        // bufferSizeDB is the constant 6144
        assert_eq!(&file[e + 4 + 4 + 5 + 3 + 5 + 2..e + 4 + 4 + 5 + 3 + 5 + 2 + 3], &[0, 0x18, 0]);
    }

    #[test]
    fn short_or_failing_sources_are_reported_as_source_errors() {
        let (sizes, bytes) = units(5, |_| 9);
        let err = write_m4a(&mut Vec::new(), &mut &bytes[..bytes.len() - 1], &sizes, &params(44100, 2 * 1024)).unwrap_err();
        assert!(matches!(&err, WriteError::Source(e) if e.kind() == io::ErrorKind::UnexpectedEof), "{err:?}");
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("source failed"))
            }
        }
        assert!(matches!(write_m4a(&mut Vec::new(), &mut Broken, &sizes, &params(44100, 2 * 1024)), Err(WriteError::Source(_))));
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(matches!(write_m4a(&mut Full, &mut &bytes[..], &sizes, &params(44100, 2 * 1024)), Err(WriteError::Output(_))));
        assert!(matches!(write_empty_m4a(&mut Full, &FileParams { avg_bitrate: 128_000, ..params(44100, 0) }), Err(WriteError::Output(_))));
    }

    #[test]
    fn unsupported_rates_are_refused_not_guessed() {
        let (sizes, bytes) = units(5, |_| 9);
        let err = write_m4a(&mut Vec::new(), &mut &bytes[..], &sizes, &params(7350, 2 * 1024)).unwrap_err();
        assert!(matches!(err, WriteError::Layout(Mp4Error::UnsupportedRate(7350))), "{err:?}");
        // the 16-bit sample-rate field of the sample entry cannot hold 96000 Hz
        let err = write_empty_m4a(&mut Vec::new(), &FileParams { sample_rate: 96000, ..params(96000, 0) }).unwrap_err();
        assert!(matches!(err, WriteError::Layout(Mp4Error::UnsupportedRate(96000))), "{err:?}");
        // a descriptor longer than 127 bytes does not fit its length field
        let asc = [0u8; 100];
        let err = write_empty_m4a(&mut Vec::new(), &FileParams { asc: &asc, ..params(44100, 0) }).unwrap_err();
        assert!(matches!(err, WriteError::Layout(Mp4Error::AscTooLong)), "{err:?}");
    }
}
