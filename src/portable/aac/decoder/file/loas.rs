//! LOAS / LATM (`AudioSyncStream` carrying an `AudioMuxElement`) reader.
//!
//! Observed behaviour (afconvert / ffmpeg LOAS streams): no priming (P = 0), `FileLengthFrames = packets * 1024`, no
//! packet table (`'pty?'`), `audio_data_bytes` = file size, `max_packet_size` = largest LOAS frame INCLUDING the 3-byte sync header
//! (588 = 3 + 585 for a 581-byte access unit).  The raw access unit handed to the decoder is the bit-shifted `PayloadMux` slot.
//! Junk handling follows ADTS (no resync): scanning ends at the first byte that is not an `0x2B7` sync word, or at a frame that runs past the end of
//! the file; when that is the first frame and its StreamMuxConfig and payload length are complete, the file opens with no packets (a read ends at once).
//! Supported: audioMuxVersion 0/1, one program, one layer, frameLengthType 0 (everything else is refused as `'fmt?'`).

use crate::portable::aac::decoder::file::asc::{parse_asc, Asc};
use crate::portable::aac::decoder::file::bits::BitReader;
use crate::portable::aac::decoder::file::error::{OpenError, Unspecified};
use crate::portable::aac::decoder::file::mp4::MAX_PACKETS;

#[derive(Clone, Debug)]
pub(crate) struct LoasStream {
    pub asc: Asc,
    /// concatenated, byte-aligned access units
    pub payload: Vec<u8>,
    /// (offset into `payload`, length) of every frame; a frame that fails has length 0
    pub frames: Vec<(u64, u32)>,
    /// indices (ascending) of the frames behind the first that cannot be read as an access unit: the read that reaches one fails `'fmt?'`
    pub failing: Vec<usize>,
    /// largest `3 + audioMuxLengthBytes`
    pub max_frame: u32,
    /// offset of the first sync word (ID3v2 tags before it are not audio data)
    pub start: usize,
}

pub(crate) fn is_loas_sync(d: &[u8], pos: usize) -> bool {
    matches!((d.get(pos), d.get(pos + 1)), (Some(0x56), Some(b)) if b & 0xE0 == 0xE0)
}

pub(crate) fn looks_like_loas(d: &[u8]) -> bool {
    is_loas_sync(d, crate::portable::aac::decoder::file::adts::skip_tags(d))
}

fn latm_value(r: &mut BitReader) -> Option<u32> {
    let n = r.read(2)?;
    r.read(8 * (n + 1))
}

/// `StreamMuxConfig` for one program / one layer; returns the ASC bytes.
fn stream_mux_config(r: &mut BitReader) -> Option<Vec<u8>> {
    let version = r.read(1)?;
    if version == 1 {
        if r.read(1)? == 1 {
            return None; // audioMuxVersionA
        }
        latm_value(r)?; // taraBufferFullness
    }
    r.read(1)?; // allStreamsSameTimeFraming
    r.read(6)?; // numSubFrames
    if r.read(4)? != 0 || r.read(3)? != 0 {
        return None; // numProgram / numLayer
    }
    let asc_bytes = if version == 0 {
        let tmp = r.peek_aligned(2048);
        let asc = parse_asc(&tmp).ok()?;
        r.skip(asc.bits_consumed)?;
        let nbytes = asc.bits_consumed.div_ceil(8);
        let mut b = tmp[..nbytes].to_vec();
        let rem = asc.bits_consumed % 8;
        if rem != 0 {
            b[nbytes - 1] &= 0xffu8 << (8 - rem);
        }
        b
    } else {
        let len = latm_value(r)? as usize;
        if len > r.bits_left() {
            return None; // an ASC the frame cannot hold (a 32-bit length must not make a 512 MiB buffer)
        }
        let mut bytes = vec![0u8; len.div_ceil(8)];
        for i in 0..len {
            bytes[i >> 3] |= (r.read(1)? as u8) << (7 - (i & 7));
        }
        bytes
    };
    if r.read(3)? != 0 {
        return None; // frameLengthType != 0
    }
    r.read(8)?; // latmBufferFullness
    if r.read(1)? == 1 {
        // otherDataPresent
        if version == 1 {
            latm_value(r)?;
        } else {
            loop {
                let esc = r.read(1)?;
                r.read(8)?;
                if esc == 0 {
                    break;
                }
            }
        }
    }
    if r.read(1)? == 1 {
        r.read(8)?; // crcCheckSum
    }
    Some(asc_bytes)
}

/// Parses the frames of the LOAS stream `d`.  The access units are bit-shifted inside their frames; each one is moved down to a byte boundary in
/// `d` itself, behind the end of the previous unit and in front of its own frame (nothing that is still to be read is overwritten), and `payload` is
/// that buffer cut behind the last unit: repacking a file of up to 2 GiB needs no second copy of it.
///
/// A complete frame behind the first that cannot be read as an access unit is a packet all the same (macOS 27.2: the table counts it, the length and the
/// bit rate include it) and the read that reaches it fails `'fmt?'`: a StreamMuxConfig other than the first one's, a damaged or cut one, a missing payload
/// length, a payload longer than the frame, an empty frame.  Its index is in [`LoasStream::failing`] and its size 0.  The first frame must be readable (else
/// the open fails `'fmt?'`).
pub(crate) fn parse_loas(mut d: Vec<u8>) -> Result<LoasStream, OpenError> {
    let mut pos = crate::portable::aac::decoder::file::adts::skip_tags(&d);
    let start = pos;
    // every LOAS open failure (no sync at the start, no StreamMuxConfig in the first frame, unsupported mux config) is 'fmt?'
    let unspec = OpenError::UnsupportedDataFormat;
    if !is_loas_sync(&d, pos) {
        return Err(unspec);
    }
    let mut asc_bytes: Option<Vec<u8>> = None;
    let mut written = 0usize;
    let mut frames: Vec<(u64, u32)> = Vec::new();
    let mut failing: Vec<usize> = Vec::new();
    let mut max_frame = 0u32;
    let mut first_cut = false;
    while is_loas_sync(&d, pos) && pos + 3 <= d.len() {
        let len = (((d[pos + 1] & 0x1f) as usize) << 8) | d[pos + 2] as usize;
        let avail = d.len() - (pos + 3);
        // a frame that runs past the end of the file is not a frame; only the first one is still read as far as it goes, for its configuration
        let cut = len > avail;
        if cut && !frames.is_empty() {
            break;
        }
        let first = asc_bytes.is_none();
        let body = &d[pos + 3..pos + 3 + len.min(avail)];
        let mut r = BitReader::new(body);
        let mut bad = false;
        match r.read(1) {
            None => bad = true,
            Some(0) => match stream_mux_config(&mut r) {
                Some(a) => match &asc_bytes {
                    Some(config) => bad = *config != a,
                    None => asc_bytes = Some(a),
                },
                None => bad = true,
            },
            Some(_) => bad = first, // no configuration before the first payload
        }
        let mut slot = 0usize;
        let mut length_known = false;
        if !bad {
            loop {
                let Some(t) = r.read(8) else { break };
                slot += t as usize;
                if t != 255 {
                    length_known = true;
                    break;
                }
            }
        }
        if first && (bad || (!cut && (!length_known || slot * 8 > r.bits_left()))) {
            return Err(unspec);
        }
        if cut {
            // the configuration and the length of the payload are all the open needs: the stream has no frame
            if !length_known {
                return Err(unspec);
            }
            first_cut = true;
            break;
        }
        if frames.len() >= MAX_PACKETS {
            return Err(OpenError::PacketTableTooLarge);
        }
        max_frame = max_frame.max(3 + len as u32);
        if bad || !length_known || slot * 8 > r.bits_left() {
            failing.push(frames.len());
            frames.push((written as u64, 0));
            pos += 3 + len;
            continue;
        }
        // the unit starts `bit_pos` bits into the body; `written <= pos` and the unit is read from `pos + 3` on, so a byte is written only behind
        // the bytes it was made from
        let from = (pos + 3) * 8 + r.bit_pos();
        let (byte, shift) = (from >> 3, (from & 7) as u32);
        for i in 0..slot {
            let hi = d[byte + i];
            d[written + i] = if shift == 0 { hi } else { (hi << shift) | (d[byte + i + 1] >> (8 - shift)) };
        }
        frames.push((written as u64, slot as u32));
        written += slot;
        pos += 3 + len;
    }
    let asc_bytes = asc_bytes.ok_or(unspec)?;
    // no frame at all is a stream only when the first frame's configuration was complete (it ended the scan as a cut frame)
    if frames.is_empty() && !first_cut {
        return Err(unspec);
    }
    let asc = parse_asc(&asc_bytes).map_err(|_| OpenError::Unspecified(Unspecified::BadAsc))?;
    d.truncate(written);
    Ok(LoasStream { asc, payload: d, frames, failing, max_frame, start })
}
