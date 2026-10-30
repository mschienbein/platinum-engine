//! Core Audio Format reader for AAC (`desc` + `kuki` + `pakt` + `data` + `chan`, and the layered description `ldsc` that replaces `desc`).
//!
//! Observed Apple behaviour (CAF variants of the container probes):
//! * the file data format (rate, channels, frames per packet, format id) is the `desc` chunk, NOT the cookie: a 48 kHz `desc` over a 44.1 kHz
//!   ASC reports 48000; any frames-per-packet value other than the codec's (1024) opens and fails the client format (`'fmt?'`; from a million
//!   frames `-50`), except 0, which opens and fails the first read (`'bada'`; the bit rate property fails `'dta?'`); a bytes-per-packet or
//!   bits-per-channel other than 0 fails the client format `'fmt?'` (a non-zero bytes-per-packet behind a `pakt` chunk fails the open
//!   `'chk?'`); a non-AAC format id fails `'fmt?'`; a missing `desc` is `'dta?'`, file version != 1 `'dta?'`; the format flags are reported as
//!   they are;
//! * chunks are read in order; a chunk whose declared size runs past the end of the file is ignored and ends the scan (a cut file opens with what
//!   it has: without `data` it holds no packets, except that a complete `pakt` without `data` fails the open `'chk?'`);
//! * `data`: a `data` chunk that ends the file with fewer than 4 bytes behind its header (not even the edit count) holds no packets, whatever
//!   `pakt` says; a complete one must reach the start of the last packet of `pakt` (`size - 4 >= sum of the other sizes`), else the open fails
//!   `'chk?'`;
//! * `kuki` holds the ES descriptor chain (`03 80 80 80 ..`); one that does not parse (or whose ASC does not) opens and fails the client format
//!   `'!dat'`; without it the decoder is configured from `desc` (channels 1..8 map to
//!   channelConfiguration, 0 is refused at open, > 8 at the client format);
//! * with a `desc` without frames per packet the `pakt` list holds (size, frames) pairs: a list of an even count of numbers serves at most half as
//!   many packets as it has numbers (more is `'chk?'`), an odd count is accepted for any packet count; the bit rate divides by the frames of the
//!   first packets' pairs, and does not exist when the list is shorter than the pairs the table announces;
//!   `pakt` supplies priming / remainder / valid frames AS IS (valid == 0 reads as "to the end", priming > total gives nothing); without `pakt`
//!   there are NO packets (first read -66567); a packet count larger than the size list is `'chk?'`, a smaller one is honoured;
//! * packets whose bytes lie beyond the end of the file end the stream silently (no error, unlike MP4); bytes behind the end of a complete `data`
//!   chunk are not audio data, and a packet that needs them fails the read like a packet in a cut file;
//! * `chan` (anywhere in the file) gives the reported layout tag (none: tag 0); a tag with more channels than the stream fails the client
//!   format with -50, fewer is ignored;
//! * `audio_data_bytes` = `data` payload size (without the 4-byte edit count; `data` size -1 = to the end of the file).

use crate::portable::aac::decoder::file::error::OpenError;
use crate::portable::aac::decoder::file::mp4::{be32, be64, parse_es_descriptor, MAX_PACKETS};

#[derive(Clone, Debug)]
pub(crate) struct CafAudio {
    pub sample_rate: f64,
    pub format_id: [u8; 4],
    pub frames_per_packet: u32,
    pub channels: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub bits_per_channel: u32,
    /// ASC from the cookie; `None` without `kuki` (or with one that holds none)
    pub asc_bytes: Option<Vec<u8>>,
    /// a `kuki` chunk that is no ES descriptor chain with a decoder configuration
    pub bad_cookie: bool,
    pub chan_tag: Option<u32>,
    pub priming: i32,
    pub remainder: i32,
    pub valid_frames: i64,
    pub has_pakt: bool,
    /// numbers in the `pakt` size list
    pub table_numbers: usize,
    /// `desc` without frames per packet: the frames of the packets, summed over the table's (size, frames) pairs; `None` when the list is too short
    pub pair_frames: Option<u64>,
    /// end of a complete `data` chunk: packet bytes behind it are not audio data (`None`: the chunk runs to the end of the file)
    pub data_end: Option<u64>,
    /// (absolute offset, size)
    pub packets: Vec<(u64, u32)>,
    pub data_bytes: u64,
    /// the file has a layered description whose first layer is not HE-AAC and differs from `desc` in format, rate, frames per packet or channels
    pub layered_unreproduced: bool,
}

pub(crate) fn looks_like_caf(d: &[u8]) -> bool {
    d.len() >= 4 && &d[0..4] == b"caff"
}

/// Where the audio payload of a scanned file is.
enum Payload {
    /// no `data` chunk, or one cut before its 4-byte edit count: no packets
    None,
    /// `data` chunk: body offset, the declared size, and whether the file holds all of it
    Data { body: usize, declared: u64, complete: bool },
}

pub(crate) fn parse_caf(d: &[u8]) -> Result<CafAudio, OpenError> {
    let invalid = OpenError::InvalidFile;
    if d.len() < 8 || d[4] != 0 || d[5] != 1 {
        return Err(invalid);
    }
    let mut pos = 8usize;
    let mut desc: Option<(f64, [u8; 4], [u32; 5])> = None;
    // the first layered description (`ldsc`) that holds a stream description and a layout tag
    let mut layered: Option<(f64, [u8; 4], [u32; 5])> = None;
    let mut kuki: Option<&[u8]> = None;
    let mut chan: Option<u32> = None;
    let mut pakt: Option<(usize, usize)> = None;
    let mut payload: Option<Payload> = None;
    while pos + 12 <= d.len() {
        let typ: [u8; 4] = d[pos..pos + 4].try_into().unwrap();
        let size = be64(d, pos + 4).ok_or(invalid)? as i64;
        let body = pos + 12;
        let left = d.len() - body;
        let complete = size < 0 || size as u64 <= left as u64;
        if !complete && &typ != b"data" {
            break;
        }
        let end = if size < 0 || !complete { d.len() } else { body + size as usize };
        match &typ {
            b"desc" if end - body >= 32 => {
                let rate = f64::from_bits(be64(d, body).ok_or(invalid)?);
                let fid: [u8; 4] = d[body + 8..body + 12].try_into().unwrap();
                // mFormatFlags +12, mBytesPerPacket +16, mFramesPerPacket +20, mChannelsPerFrame +24, mBitsPerChannel +28
                let field = |o: usize| be32(d, body + o).ok_or(invalid);
                desc = Some((rate, fid, [field(12)?, field(16)?, field(20)?, field(24)?, field(28)?]));
            }
            b"ldsc" if layered.is_none() && end - body >= 36 => {
                let rate = f64::from_bits(be64(d, body).ok_or(invalid)?);
                let fid: [u8; 4] = d[body + 8..body + 12].try_into().unwrap();
                let field = |o: usize| be32(d, body + o).ok_or(invalid);
                layered = Some((rate, fid, [field(12)?, field(16)?, field(20)?, field(24)?, field(28)?]));
            }
            b"kuki" => kuki = Some(&d[body..end]),
            b"chan" if end - body >= 4 => chan = be32(d, body),
            b"pakt" => pakt = Some((body, end)),
            b"data" if payload.is_none() => {
                // `audio_data_bytes` is the DECLARED payload size (a cut file still reports it)
                let declared = if size < 0 { left as u64 } else { size as u64 };
                // a chunk with fewer than 4 bytes behind its header (the file ends in it, or with it) holds not even the edit count
                let empty = declared.min(left as u64) < 4 && (!complete || end == d.len());
                payload = Some(if empty { Payload::None } else { Payload::Data { body, declared, complete } });
            }
            _ => {}
        }
        if size < 0 {
            break;
        }
        pos = end;
    }
    // The file's data format is the first layer of the layered description when the file has one (HE-AAC files from afconvert: `desc` is the core, `ldsc` the
    // full format 'aach' / 'aacp' at the full rate and 2048 frames per packet); a file without `desc` is invalid whatever else it has.  macOS 27.2: every field
    // of that stream description replaces the `desc` one, a chunk of fewer than 36 bytes (stream description and layout tag) is ignored, the first chunk counts.
    // A first layer that is no HE-AAC is presented the same way (with a length rescaled to its rate), which is not reproduced: such a file is refused.
    let desc = desc.ok_or(invalid)?;
    let is_he = |fid: &[u8; 4]| matches!(fid, b"aach" | b"aacp");
    // only an HE-AAC first layer is taken for the file's format (a refusal in the end); any other that differs from `desc` is not reproduced
    let layered_unreproduced = layered.is_some_and(|l| !is_he(&l.1) && (l.0.to_bits() != desc.0.to_bits() || l.1 != desc.1 || l.2[2] != desc.2[2] || l.2[3] != desc.2[3]));
    let (rate, fid, [flags, bpp, fpp, channels, bits]) = layered.filter(|l| is_he(&l.1)).unwrap_or(desc);
    // (a rate that is not a number is opened, and refused like a rate of 0 by whoever needs the rate)
    if channels == 0 || !(rate.is_nan() || (rate.is_finite() && rate >= 0.0 && rate <= 3_000_000.0)) {
        return Err(OpenError::UnsupportedDataFormat);
    }
    // a complete packet table without audio data
    if payload.is_none() && pakt.is_some() {
        return Err(OpenError::InvalidChunk);
    }
    // a packet size per packet cannot go with a packet table
    if bpp != 0 && fpp != 0 && pakt.is_some() {
        return Err(OpenError::InvalidChunk);
    }
    let asc_bytes = kuki.and_then(|k| parse_es_descriptor(k).ok()).map(|e| e.asc);
    let bad_cookie = kuki.is_some() && asc_bytes.is_none();
    let (data_bytes, data_body) = match payload {
        Some(Payload::Data { body, declared, .. }) => (declared.saturating_sub(4), Some(body)),
        _ => (0, None),
    };
    let mut out = CafAudio {
        sample_rate: rate,
        format_id: fid,
        frames_per_packet: fpp,
        channels,
        format_flags: flags,
        bytes_per_packet: bpp,
        bits_per_channel: bits,
        asc_bytes,
        bad_cookie,
        chan_tag: chan,
        priming: 0,
        remainder: 0,
        valid_frames: 0,
        has_pakt: false,
        table_numbers: 0,
        pair_frames: None,
        data_end: None,
        packets: Vec::new(),
        data_bytes,
        layered_unreproduced,
    };
    if let Some(Payload::Data { body, declared, complete: true }) = &payload {
        out.data_end = Some(*body as u64 + declared).filter(|&end| end < d.len() as u64);
    }
    if let (Some((ps, pe)), Some(ds)) = (pakt, data_body) {
        if pe - ps >= 24 {
            out.has_pakt = true;
            let n = be64(d, ps).ok_or(invalid)? as i64;
            out.valid_frames = be64(d, ps + 8).ok_or(invalid)? as i64;
            out.priming = be32(d, ps + 16).ok_or(invalid)? as i32;
            out.remainder = be32(d, ps + 20).ok_or(invalid)? as i32;
            // one number ends at every byte below 0x80, and a last one may be cut off: the table is allocated once, at its size
            let numbers = d[ps + 24..pe].iter().filter(|&&b| b & 0x80 == 0).count() + usize::from(d[ps + 24..pe].last().is_some_and(|&b| b & 0x80 != 0));
            let mut sizes: Vec<u32> = Vec::with_capacity(numbers.min(MAX_PACKETS));
            let mut p = ps + 24;
            while p < pe {
                let mut v: u32 = 0;
                loop {
                    if p >= pe {
                        break;
                    }
                    let b = d[p];
                    p += 1;
                    v = (v << 7) | u32::from(b & 0x7f);
                    if b & 0x80 == 0 {
                        break;
                    }
                }
                if sizes.len() >= MAX_PACKETS {
                    return Err(OpenError::PacketTableTooLarge);
                }
                sizes.push(v);
            }
            if n < 0 {
                return Err(OpenError::InvalidChunk);
            }
            out.table_numbers = sizes.len();
            // without frames per packet the table holds a (size, frames) pair per packet: an even list serves half as many packets as it has
            // numbers, an odd one (a lone size at its end) any number; with them a count above the list is as wrong
            let too_many = if fpp == 0 { sizes.len() % 2 == 0 && n as usize > sizes.len() / 2 } else { n as usize > sizes.len() };
            if too_many {
                return Err(OpenError::InvalidChunk);
            }
            if fpp == 0 && 2 * (n as usize) <= sizes.len() {
                out.pair_frames = Some((0..n as usize).map(|i| u64::from(sizes[2 * i + 1])).sum());
            }
            let sizes = &sizes[..(n as usize).min(sizes.len())];
            // a complete `data` chunk holds at least the start of the last packet
            if let (Some(Payload::Data { declared, complete: true, .. }), Some((_, head))) = (&payload, sizes.split_last()) {
                if *declared < 4 + head.iter().map(|&s| u64::from(s)).sum::<u64>() {
                    return Err(OpenError::InvalidChunk);
                }
            }
            let mut off = (ds + 4) as u64; // 4-byte edit count
            out.packets.reserve_exact(sizes.len());
            for &s in sizes {
                out.packets.push((off, s));
                off += u64::from(s);
            }
        }
    }
    Ok(out)
}
