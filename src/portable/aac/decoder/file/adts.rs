//! ADTS reader.
//!
//! Observed Apple behaviour:
//! * any number of ID3v2 / ID3v1 blocks may precede the first frame; anything else (junk, even one byte) before the first sync word fails with `'sync'`;
//! * a frame header is accepted when `b0 == 0xFF` and `b1 & 0xF6 == 0xF0` (the ID and protection_absent bits are free, layer must be 0);
//!   scanning stops at the first byte that is not a sync word (junk between frames ends the stream there), at a frame whose
//!   `frame_length <= header size` (7, or 9 with CRC), and at a frame that extends beyond the end of the file (dropped);
//! * profile, sampling index and channel_configuration are taken from the FIRST frame only (later changes are ignored), except that a later header
//!   with profile 3, a sampling index of 13..15, or protection_absent 0 behind a first frame with protection_absent 1 ends the stream there; the
//!   header size (7, or 9 with CRC) is the first frame's for the whole stream;
//!   a first MPEG-2 header (`ID` 1) with profile 3 fails `'prof'`;
//! * profile != LC (`'fmt?'`), sampling index 12 (`'fmt?'`), 13..15 (`'rate'`), channel_configuration 0 without in-band PCE (`'!dat'`);
//! * `number_of_raw_data_blocks_in_frame != 0` anywhere: the open succeeds, the packet table the properties see is empty, and the reads deliver the
//!   frames before it and fail `'dta?'` when their (batched) scan reaches it;
//! * the first access unit must be consistent with channel_configuration (observed: stereo data with cfg 1, 3..7 and mono data with cfg 3..7
//!   are refused `'fmt?'`; mono data with cfg 2 is accepted and reported as mono).  Only the first syntactic element id can be checked
//!   here (a full check needs the element parser, see `decoder::elements`).
//! * no priming/packet table is reported (`'pty?'`); `FileLengthFrames = packets * 1024`; max packet size = largest raw access unit.

use crate::portable::aac::decoder::file::asc::{parse_pce, PceConfig};
use crate::portable::aac::decoder::file::bits::BitReader;
use crate::portable::aac::decoder::file::error::OpenError;
use crate::portable::aac::decoder::file::mp4::MAX_PACKETS;

#[derive(Clone, Debug)]
pub(crate) struct AdtsStream {
    pub profile: u8,
    pub sfi: u8,
    pub channel_config: u8,
    /// (offset of the raw_data_block, length)
    pub frames: Vec<(u64, u32)>,
    /// index of the first frame that announced more than one raw_data_block: the scan stops there, a read that reaches it fails `'dta?'`
    pub multi_block_at: Option<usize>,
    /// in-band PCE of the first access unit (channel_configuration 0)
    pub pce: Option<PceConfig>,
    /// id (0 SCE, 1 CPE, ...) of the first syntactic element after an optional PCE
    pub first_element: Option<u8>,
    pub max_au: u32,
    /// offset of the first sync word (bytes before it are ID3v2 tags, not audio data)
    pub start: usize,
}

pub(crate) fn is_sync(d: &[u8], pos: usize) -> bool {
    matches!((d.get(pos), d.get(pos + 1)), (Some(0xFF), Some(b)) if b & 0xF6 == 0xF0)
}

/// Bytes of one leading tag block at the start of `d`: an ID3v2 tag (`ID3`, version bytes other than 0xff, syncsafe size + 10; a footer is not
/// counted) or an ID3v1 block (`TAG`, 128 bytes); 0 when `d` starts with neither.  The rule of AudioFile's generic tag skip, which every file type
/// sees behind (the same as `portable::audio::mp3::container::id3_size`).
fn tag_size(d: &[u8]) -> usize {
    let h = &d[..d.len().min(10)];
    if h.len() > 9 && &h[..3] == b"ID3" && h[3] != 0xff && h[4] != 0xff {
        return (((h[6] as usize) & 0x7f) << 21 | ((h[7] as usize) & 0x7f) << 14 | ((h[8] as usize) & 0x7f) << 7 | (h[9] as usize) & 0x7f) + 10;
    }
    if h.len() > 2 && &h[..3] == b"TAG" {
        return 128;
    }
    0
}

/// Most leading tag blocks that are skipped (the sniff of `Reader::open` and the parser use the same bound, so they cannot disagree about where the
/// audio starts in a file made of nothing but tags).
pub(crate) const MAX_TAG_BLOCKS: usize = 1_000_000;

/// Skips the leading ID3v2 / ID3v1 blocks (any order, at most [`MAX_TAG_BLOCKS`]); returns the offset of the first byte behind them (the length of
/// `d` when they reach or pass its end).
pub(crate) fn skip_tags(d: &[u8]) -> usize {
    let mut pos = 0usize;
    for _ in 0..MAX_TAG_BLOCKS {
        if pos >= d.len() {
            break;
        }
        let size = tag_size(&d[pos..]);
        if size == 0 {
            break;
        }
        pos = pos.saturating_add(size);
    }
    pos.min(d.len())
}
pub(crate) fn looks_like_adts(d: &[u8]) -> bool {
    is_sync(d, skip_tags(d))
}

pub(crate) fn parse_adts(d: &[u8]) -> Result<AdtsStream, OpenError> {
    let mut pos = skip_tags(d);
    if !is_sync(d, pos) {
        // a sync word whose layer bits are not 00 is an MPEG audio frame
        return Err(if d.get(pos) == Some(&0xFF) && d.get(pos + 1).is_some_and(|b| b & 0xF0 == 0xF0) { OpenError::WrongLayer } else { OpenError::NoSync });
    }
    let mut s = AdtsStream { profile: 0, sfi: 0, channel_config: 0, frames: Vec::new(), multi_block_at: None, pce: None, first_element: None, max_au: 0, start: pos };
    let mut seen = false;
    let mut first_protection_absent = true;
    while is_sync(d, pos) && pos + 7 <= d.len() {
        let h = &d[pos..pos + 7];
        let protection_absent = h[1] & 1 == 1;
        let profile = h[2] >> 6;
        let sfi = (h[2] >> 2) & 0xf;
        let cfg = ((h[2] & 1) << 2) | (h[3] >> 6);
        let flen = (((h[3] & 3) as usize) << 11) | ((h[4] as usize) << 3) | ((h[5] >> 5) as usize);
        let nrdb = h[6] & 3;
        // the header size is the first frame's: a later frame of the other kind is cut where the first one's header would end
        let hdr = if seen { if first_protection_absent { 7 } else { 9 } } else if protection_absent { 7 } else { 9 };
        if flen <= hdr || pos + flen > d.len() {
            break;
        }
        if !seen {
            seen = true;
            // the MPEG-2 header (ID 1) has no profile 3
            if profile == 3 && h[1] & 8 != 0 {
                return Err(OpenError::UnsupportedProfile);
            }
            s.profile = profile;
            s.sfi = sfi;
            s.channel_config = cfg;
            first_protection_absent = protection_absent;
        } else if profile == 3 || sfi >= 13 || (first_protection_absent && !protection_absent) {
            // a later header that cannot be a frame of this stream ends it (a CRC-protected frame after unprotected ones, the reserved profile or
            // sampling index); after a protected first frame the protection bit is free
            break;
        }
        if nrdb != 0 {
            s.multi_block_at = Some(s.frames.len());
            break;
        }
        if s.frames.len() >= MAX_PACKETS {
            return Err(OpenError::PacketTableTooLarge);
        }
        let au = ((pos + hdr) as u64, (flen - hdr) as u32);
        s.max_au = s.max_au.max(au.1);
        s.frames.push(au);
        pos += flen;
    }
    if !seen {
        return Err(OpenError::Unspecified(crate::portable::aac::decoder::file::error::Unspecified::NoFrames));
    }
    match s.sfi {
        13..=15 => return Err(OpenError::UnsupportedRate),
        12 => return Err(OpenError::UnsupportedDataFormat),
        _ => {}
    }
    if s.profile != 1 {
        return Err(OpenError::UnsupportedDataFormat);
    }
    if s.multi_block_at.is_some() {
        return Ok(s);
    }
    // first access unit: optional in-band PCE, then the first element id
    let (off, len) = s.frames[0];
    let au = &d[off as usize..off as usize + len as usize];
    let mut r = BitReader::new(au);
    // skip PCE / DSE / FIL (ffmpeg starts every access unit with a FIL element) to the first channel element
    let mut first = None;
    loop {
        match r.read(3) {
            Some(5) => match parse_pce(&mut r) {
                Some(p) => {
                    if s.pce.is_none() {
                        s.pce = Some(p);
                    }
                }
                None => break,
            },
            Some(4) => {
                let ok = (|| {
                    r.read(4)?;
                    let align = r.read(1)? == 1;
                    let mut cnt = r.read(8)? as usize;
                    if cnt == 255 {
                        cnt += r.read(8)? as usize;
                    }
                    if align {
                        r.align()?;
                    }
                    r.skip(cnt * 8)
                })();
                if ok.is_none() {
                    break;
                }
            }
            Some(6) => {
                let ok = (|| {
                    let mut cnt = r.read(4)? as usize;
                    if cnt == 15 {
                        cnt = cnt + r.read(8)? as usize - 1; // esc_count 0 is a count of 14 (no underflow: cnt is 15 here)
                    }
                    r.skip(cnt * 8)
                })();
                if ok.is_none() {
                    break;
                }
            }
            other => {
                first = other;
                break;
            }
        }
    }
    s.first_element = first.map(|v| v as u8);
    match s.channel_config {
        0 if s.pce.is_none() => return Err(OpenError::NoDecoder),
        0 => {}
        1 | 2 => match s.first_element {
            Some(0) => {} // SCE: mono (cfg 2 with an SCE is reported as mono)
            Some(1) if s.channel_config == 2 => {}
            _ => return Err(OpenError::UnsupportedDataFormat),
        },
        3..=7 => {
            if s.first_element != Some(0) {
                return Err(OpenError::UnsupportedDataFormat);
            }
        }
        _ => return Err(OpenError::UnsupportedDataFormat),
    }
    Ok(s)
}
