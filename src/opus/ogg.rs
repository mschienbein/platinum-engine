//! Bounded Ogg (RFC 3533) page reading and a deterministic page writer.
//!
//! The reader is strictly sequential and never resynchronizes: any byte that is
//! not a valid, CRC-correct page is an error, so a damaged file can never be
//! silently shortened. Every allocation is bounded by one page (at most
//! 27 + 255 + 255 * 255 bytes) regardless of what the stream claims.

use std::io::{self, Read, Write};

pub const CAPTURE: [u8; 4] = *b"OggS";
pub const FLAG_CONTINUED: u8 = 0x01;
pub const FLAG_BOS: u8 = 0x02;
pub const FLAG_EOS: u8 = 0x04;
pub const HEADER_LEN: usize = 27;
pub const MAX_SEGMENTS: usize = 255;
pub const MAX_PAGE_BODY: usize = MAX_SEGMENTS * 255;
/// Granule position value of a page on which no packet completes.
pub const NO_GRANULE: u64 = u64::MAX;

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04c1_1db7 } else { r << 1 };
            bit += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}
static CRC_TABLE: [u32; 256] = crc_table();

/// Ogg's CRC-32: polynomial 0x04C11DB7, zero initial value, not reflected, no
/// final inversion.
pub fn crc32(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc = (crc << 8) ^ CRC_TABLE[usize::from((crc >> 24) as u8 ^ byte)];
    }
    crc
}

/// One page. `lacing` holds the segment table; `body` the concatenated segments.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub flags: u8,
    pub granule: u64,
    pub serial: u32,
    pub sequence: u32,
    pub lacing: Vec<u8>,
    pub body: Vec<u8>,
    /// Byte offset of the capture pattern in the source.
    pub offset: u64,
}
impl Page {
    pub fn encoded_len(&self) -> usize { HEADER_LEN + self.lacing.len() + self.body.len() }
    pub fn is_continued(&self) -> bool { self.flags & FLAG_CONTINUED != 0 }
    pub fn is_bos(&self) -> bool { self.flags & FLAG_BOS != 0 }
    pub fn is_eos(&self) -> bool { self.flags & FLAG_EOS != 0 }
    /// Serialize with a freshly computed checksum.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&CAPTURE);
        out.push(0);
        out.push(self.flags);
        out.extend_from_slice(&self.granule.to_le_bytes());
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.push(self.lacing.len() as u8);
        out.extend_from_slice(&self.lacing);
        out.extend_from_slice(&self.body);
        let crc = crc32(0, &out);
        out[22..26].copy_from_slice(&crc.to_le_bytes());
        out
    }
}

#[derive(Debug)]
pub enum PageError {
    Io(io::Error),
    /// Not a page where one must begin.
    BadCapture { offset: u64 },
    BadVersion { offset: u64 },
    BadFlags { offset: u64 },
    Truncated { offset: u64 },
    BadChecksum { offset: u64 },
}
impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "Ogg read: {e}"),
            Self::BadCapture { offset } => write!(f, "no Ogg page at byte {offset}"),
            Self::BadVersion { offset } => write!(f, "unsupported Ogg page version at byte {offset}"),
            Self::BadFlags { offset } => write!(f, "reserved Ogg page flag bits at byte {offset}"),
            Self::Truncated { offset } => write!(f, "Ogg page at byte {offset} is truncated"),
            Self::BadChecksum { offset } => write!(f, "Ogg page at byte {offset} fails its checksum"),
        }
    }
}
impl std::error::Error for PageError {}
impl From<io::Error> for PageError { fn from(e: io::Error) -> Self { Self::Io(e) } }

/// Sequential page reader over any byte source.
pub struct PageReader<R: Read> {
    input: R,
    position: u64,
}
impl<R: Read> PageReader<R> {
    pub fn new(input: R) -> Self { Self { input, position: 0 } }
    /// Continue counting offsets from `position` (used after a seek).
    pub fn at(input: R, position: u64) -> Self { Self { input, position } }
    pub fn position(&self) -> u64 { self.position }
    pub fn into_inner(self) -> R { self.input }

    fn fill(&mut self, buffer: &mut [u8]) -> Result<usize, io::Error> {
        let mut filled = 0;
        while filled < buffer.len() {
            match self.input.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }

    /// Read the next page into `page`, reusing its buffers. `Ok(false)` is a
    /// clean end of input exactly on a page boundary.
    pub fn read_page(&mut self, page: &mut Page) -> Result<bool, PageError> {
        let offset = self.position;
        let mut header = [0u8; HEADER_LEN];
        let got = self.fill(&mut header)?;
        if got == 0 { return Ok(false); }
        if got < 4 || header[..4] != CAPTURE {
            return Err(PageError::BadCapture { offset });
        }
        if got < HEADER_LEN { return Err(PageError::Truncated { offset }); }
        if header[4] != 0 { return Err(PageError::BadVersion { offset }); }
        if header[5] & !(FLAG_CONTINUED | FLAG_BOS | FLAG_EOS) != 0 { return Err(PageError::BadFlags { offset }); }
        let segments = usize::from(header[26]);
        page.lacing.clear();
        page.lacing.resize(segments, 0);
        if self.fill(&mut page.lacing)? != segments { return Err(PageError::Truncated { offset }); }
        let body: usize = page.lacing.iter().map(|&n| usize::from(n)).sum();
        page.body.clear();
        page.body.resize(body, 0);
        if self.fill(&mut page.body)? != body { return Err(PageError::Truncated { offset }); }
        let stored = u32::from_le_bytes(header[22..26].try_into().expect("four bytes"));
        header[22..26].fill(0);
        let mut crc = crc32(0, &header);
        crc = crc32(crc, &page.lacing);
        crc = crc32(crc, &page.body);
        if crc != stored { return Err(PageError::BadChecksum { offset }); }
        page.flags = header[5];
        page.granule = u64::from_le_bytes(header[6..14].try_into().expect("eight bytes"));
        page.serial = u32::from_le_bytes(header[14..18].try_into().expect("four bytes"));
        page.sequence = u32::from_le_bytes(header[18..22].try_into().expect("four bytes"));
        page.offset = offset;
        self.position = offset + (HEADER_LEN + segments + body) as u64;
        Ok(true)
    }
}

/// Lacing values for a packet of `len` bytes: `len / 255` values of 255 and a
/// terminating value below 255 (zero when `len` is a multiple of 255).
pub fn lacing_for(len: usize) -> impl Iterator<Item = u8> {
    let full = len / 255;
    std::iter::repeat_n(255u8, full).chain(std::iter::once((len % 255) as u8))
}
pub fn segments_for(len: usize) -> usize { len / 255 + 1 }

/// Deterministic page packer. The layout is a pure function of the packet
/// sequence and the two parameters below:
///
/// * the stream's first page holds only the first packet and has BOS set;
/// * a packet that cannot share a page spans pages in 65,025-byte slices;
/// * the page on which a header packet completes is flushed;
/// * audio pages close once they hold at least `page_samples` samples of
///   completed audio or when the next packet would exceed 255 segments.
pub struct PageWriter<W: Write> {
    out: W,
    serial: u32,
    sequence: u32,
    pending: Vec<u8>,
    pending_lacing: Vec<u8>,
    pending_samples: u64,
    pending_granule: u64,
    granule: u64,
    began: bool,
    ended: bool,
    bytes: u64,
}
impl<W: Write> PageWriter<W> {
    pub fn new(out: W, serial: u32) -> Self {
        Self { out, serial, sequence: 0, pending: Vec::new(), pending_lacing: Vec::new(), pending_samples: 0,
            pending_granule: 0, granule: 0, began: false, ended: false, bytes: 0 }
    }
    pub fn bytes_written(&self) -> u64 { self.bytes }
    pub fn into_inner(self) -> W { self.out }
    pub fn sequence(&self) -> u32 { self.sequence }

    fn emit(&mut self, flags: u8, granule: u64, lacing: &[u8], body: &[u8]) -> io::Result<()> {
        debug_assert!(lacing.len() <= MAX_SEGMENTS);
        let mut flags = flags;
        if !self.began { flags |= FLAG_BOS; self.began = true; }
        let page = Page { flags, granule, serial: self.serial, sequence: self.sequence, lacing: lacing.to_vec(), body: body.to_vec(), offset: 0 };
        self.sequence = self.sequence.checked_add(1).ok_or_else(|| io::Error::other("Ogg page sequence overflow"))?;
        let bytes = page.encode();
        self.out.write_all(&bytes)?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    /// Write a header packet (OpusHead / OpusTags): alone on its page(s), the
    /// completing page flushed. Pages that do not complete the packet carry the
    /// "no granule" value; the completing page has granule 0.
    pub fn write_header_packet(&mut self, packet: &[u8]) -> io::Result<()> {
        debug_assert!(self.pending.is_empty());
        let mut rest = packet;
        let mut continued = false;
        loop {
            // Slice at most 255 whole segments; a final slice shorter than that completes the packet.
            if rest.len() >= MAX_PAGE_BODY {
                let (slice, tail) = rest.split_at(MAX_PAGE_BODY);
                let lacing = [255u8; MAX_SEGMENTS];
                // The packet continues on the next page (even when `tail` is empty a
                // zero lacing value must follow, which the next page carries).
                self.emit(if continued { FLAG_CONTINUED } else { 0 }, NO_GRANULE, &lacing, slice)?;
                rest = tail;
                continued = true;
            } else {
                let lacing: Vec<u8> = lacing_for(rest.len()).collect();
                self.emit(if continued { FLAG_CONTINUED } else { 0 }, 0, &lacing, rest)?;
                return Ok(());
            }
        }
    }

    /// Queue one audio packet of `samples` 48 kHz samples; the page granule is
    /// the running total of all samples queued so far.
    pub fn push_audio(&mut self, packet: &[u8], samples: u64, page_samples: u64) -> io::Result<()> {
        let needed = segments_for(packet.len());
        // A full page is only closed when the *next* packet arrives, so the final
        // page is always still pending when the stream is finished.
        if !self.pending_lacing.is_empty()
            && (self.pending_samples >= page_samples || self.pending_lacing.len() + needed > MAX_SEGMENTS) {
            self.flush(0)?;
        }
        self.pending.extend_from_slice(packet);
        self.pending_lacing.extend(lacing_for(packet.len()));
        self.pending_samples += samples;
        self.granule += samples;
        self.pending_granule = self.granule;
        Ok(())
    }

    fn flush(&mut self, flags: u8) -> io::Result<()> {
        if self.pending_lacing.is_empty() { return Ok(()); }
        let body = std::mem::take(&mut self.pending);
        let lacing = std::mem::take(&mut self.pending_lacing);
        let granule = self.pending_granule;
        self.pending_samples = 0;
        self.emit(flags, granule, &lacing, &body)?;
        Ok(())
    }

    /// Close the stream: the pending page is flushed with EOS and
    /// `final_granule` (which may trim the end of the last packet).
    pub fn finish(&mut self, final_granule: u64) -> io::Result<()> {
        if self.ended { return Ok(()); }
        self.pending_granule = final_granule;
        self.flush(FLAG_EOS)?;
        self.ended = true;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_ogg_vector() {
        // RFC 3533 CRC: "123456789" under the non-reflected 0x04C11DB7 polynomial.
        assert_eq!(crc32(0, b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn page_round_trip_and_checksum_detection() {
        let page = Page { flags: FLAG_BOS, granule: 7, serial: 0x1234_5678, sequence: 3,
            lacing: lacing_for(300).collect(), body: vec![0xa5; 300], offset: 0 };
        let bytes = page.encode();
        let mut reader = PageReader::new(&bytes[..]);
        let mut read = Page::default();
        assert!(reader.read_page(&mut read).unwrap());
        assert_eq!((read.flags, read.granule, read.serial, read.sequence), (FLAG_BOS, 7, 0x1234_5678, 3));
        assert_eq!(read.body, page.body);
        assert!(!reader.read_page(&mut read).unwrap());
        let mut damaged = bytes.clone();
        damaged[40] ^= 1;
        assert!(matches!(PageReader::new(&damaged[..]).read_page(&mut read), Err(PageError::BadChecksum { offset: 0 })));
        assert!(matches!(PageReader::new(&bytes[..bytes.len() - 1]).read_page(&mut read), Err(PageError::Truncated { .. })));
        assert!(matches!(PageReader::new(&b"Ogg"[..]).read_page(&mut read), Err(PageError::BadCapture { .. })));
    }

    #[test]
    fn lacing_of_exact_multiples_ends_with_zero() {
        assert_eq!(lacing_for(0).collect::<Vec<_>>(), [0]);
        assert_eq!(lacing_for(255).collect::<Vec<_>>(), [255, 0]);
        assert_eq!(lacing_for(256).collect::<Vec<_>>(), [255, 1]);
        assert_eq!(segments_for(510), 3);
    }

    #[test]
    fn header_packet_spanning_pages_round_trips() {
        for len in [0usize, 1, 254, 255, 65_024, 65_025, 65_026, 130_050, 200_000] {
            let packet: Vec<u8> = (0..len).map(|i| (i * 7 + 1) as u8).collect();
            let mut out = Vec::new();
            let mut writer = PageWriter::new(&mut out, 1);
            writer.write_header_packet(&packet).unwrap();
            writer.finish(0).unwrap();
            let mut reader = PageReader::new(&out[..]);
            let mut page = Page::default();
            let mut joined = Vec::new();
            let mut completed = false;
            while reader.read_page(&mut page).unwrap() {
                joined.extend_from_slice(&page.body);
                if page.lacing.last().is_some_and(|&n| n < 255) { completed = true; assert_eq!(page.granule, 0); }
                else { assert_eq!(page.granule, NO_GRANULE); }
            }
            assert!(completed, "len {len}");
            assert_eq!(joined, packet, "len {len}");
        }
    }
}
