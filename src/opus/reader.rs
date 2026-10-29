//! Ogg Opus demultiplexing and sequential decoding.
//!
//! `OpusReader::open` makes one validating pass over the whole file (every page
//! checksum, sequence number, serial, granule position and packet duration) so
//! that the exact decoded length and every unsupported variant are known before
//! any audio is requested. Decoding then re-reads the pages. Nothing is
//! allocated from a size the stream merely claims: the largest buffers are one
//! page, one audio packet (61,440 bytes) and the comment header (64 MiB cap).

use super::head::{self, CommentHeader, MAX_AUDIO_PACKET, MAX_COMMENT_HEADER, OpusHead};
use super::ogg::{FLAG_EOS, NO_GRANULE, Page, PageError, PageReader};
use super::{DecoderState, OpusError, OpusLibrary, SAMPLE_RATE, i24_to_f32, io_error};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

impl From<PageError> for OpusError {
    fn from(error: PageError) -> Self {
        match error {
            PageError::Io(source) => io_error("read", source),
            PageError::BadCapture { .. } => OpusError::Invalid("data is not an Ogg page (or garbage follows the stream)"),
            PageError::BadVersion { .. } => OpusError::Invalid("unsupported Ogg page version"),
            PageError::BadFlags { .. } => OpusError::Invalid("reserved Ogg page flags are set"),
            PageError::Truncated { .. } => OpusError::Invalid("an Ogg page is truncated"),
            PageError::BadChecksum { .. } => OpusError::Invalid("an Ogg page fails its checksum"),
        }
    }
}

/// Pages to packets for one logical stream, with page-level consistency checks.
pub(crate) struct Demux<R: Read> {
    pages: PageReader<R>,
    page: Page,
    serial: Option<u32>,
    sequence: Option<u32>,
    partial: Vec<u8>,
    in_progress: bool,
    /// Completed packets of the latest page, concatenated (a packet that began
    /// on an earlier page is whole here); `ends` are exclusive end offsets.
    pub(crate) bytes: Vec<u8>,
    pub(crate) ends: Vec<usize>,
    pub(crate) flags: u8,
    pub(crate) granule: u64,
    pub(crate) sequence_of_page: u32,
    pub(crate) next_offset: u64,
    eos: bool,
}
impl<R: Read> Demux<R> {
    pub(crate) fn new(pages: PageReader<R>) -> Self {
        Self { pages, page: Page::default(), serial: None, sequence: None, partial: Vec::new(), in_progress: false,
            bytes: Vec::new(), ends: Vec::new(), flags: 0, granule: NO_GRANULE, sequence_of_page: 0, next_offset: 0, eos: false }
    }
    /// Continue an already-identified stream (audio pages after the headers).
    pub(crate) fn resume(&mut self, serial: u32, previous_sequence: u32) {
        self.serial = Some(serial);
        self.sequence = Some(previous_sequence);
    }
    pub(crate) fn serial(&self) -> Option<u32> { self.serial }
    pub(crate) fn saw_eos(&self) -> bool { self.eos }
    pub(crate) fn packet_pending(&self) -> bool { self.in_progress }
    pub(crate) fn packet(&self, index: usize) -> &[u8] {
        let start = if index == 0 { 0 } else { self.ends[index - 1] };
        &self.bytes[start..self.ends[index]]
    }

    /// Read the next page of the stream. `limit` bounds any single packet.
    /// `Ok(false)` is a clean end of input.
    pub(crate) fn next_page(&mut self, limit: usize) -> Result<bool, OpusError> {
        if self.eos { return Err(OpusError::Invalid("a page follows the end-of-stream page")); }
        if !self.pages.read_page(&mut self.page)? { return Ok(false); }
        let page = &self.page;
        match self.serial {
            None => {
                if !page.is_bos() { return Err(OpusError::Invalid("the stream does not begin with a beginning-of-stream page")); }
                self.serial = Some(page.serial);
            }
            Some(serial) => {
                if page.is_bos() { return Err(OpusError::Unsupported("chained or multiplexed Ogg stream (a second beginning-of-stream page)")); }
                if page.serial != serial { return Err(OpusError::Unsupported("multiplexed Ogg stream (more than one serial number)")); }
            }
        }
        if let Some(previous) = self.sequence {
            if page.sequence != previous.wrapping_add(1) { return Err(OpusError::Invalid("Ogg page sequence numbers are not consecutive")); }
        }
        self.sequence = Some(page.sequence);
        if page.is_continued() != self.in_progress {
            return Err(OpusError::Invalid(if page.is_continued() {
                "a page continues a packet that did not begin on the previous page"
            } else { "a packet is not continued on the page that follows it" }));
        }
        self.bytes.clear();
        self.ends.clear();
        let mut cursor = 0usize;
        let mut run = 0usize;
        for &lace in &page.lacing {
            run += usize::from(lace);
            if lace == 255 { continue; }
            if self.partial.len() + run > limit { return Err(OpusError::Limit("an Ogg packet exceeds its size limit")); }
            self.bytes.extend_from_slice(&self.partial);
            self.partial.clear();
            self.bytes.extend_from_slice(&page.body[cursor..cursor + run]);
            self.ends.push(self.bytes.len());
            cursor += run;
            run = 0;
        }
        if page.lacing.last() == Some(&255) {
            if self.partial.len() + run > limit { return Err(OpusError::Limit("an Ogg packet exceeds its size limit")); }
            self.partial.extend_from_slice(&page.body[cursor..cursor + run]);
            self.in_progress = true;
        } else if !page.lacing.is_empty() {
            self.in_progress = false;
        }
        self.flags = page.flags;
        self.granule = page.granule;
        self.sequence_of_page = page.sequence;
        self.next_offset = self.pages.position();
        if page.is_eos() { self.eos = true; }
        Ok(true)
    }

    /// After the end-of-stream page: clean end of input is `Ok`, anything else is
    /// either a chained stream or trailing garbage.
    pub(crate) fn expect_end(&mut self) -> Result<(), OpusError> {
        let mut page = Page::default();
        match self.pages.read_page(&mut page) {
            Ok(false) => Ok(()),
            Ok(true) => Err(OpusError::Unsupported("chained Ogg stream (data follows the end-of-stream page)")),
            Err(e) => Err(e.into()),
        }
    }
}

/// What the validating scan learned about a file.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub head: OpusHead,
    pub serial: u32,
    /// Frames the decoder yields after pre-skip and end trimming.
    pub frames: u64,
    /// Samples produced by decoding every packet (before trimming).
    pub decoded_samples: u64,
    pub audio_packets: u64,
    pub audio_bytes: u64,
    pub pages: u64,
    /// Granule of the first audio page minus the samples completed on it.
    pub initial_granule: u64,
    pub final_granule: u64,
    pub end_trim: u64,
    /// File offset and page sequence number of the first audio page.
    pub audio_offset: u64,
    pub audio_sequence: u32,
    /// Size of the OpusTags packet.
    pub comment_header_bytes: u64,
    pub comment_count: u32,
}
impl StreamInfo {
    /// Average payload bitrate in bit/s (Opus packets only, no Ogg framing).
    pub fn bitrate(&self) -> u32 {
        let samples = self.decoded_samples.max(1);
        u32::try_from(self.audio_bytes.saturating_mul(8).saturating_mul(u64::from(SAMPLE_RATE)) / samples).unwrap_or(u32::MAX)
    }
}

/// Walk a whole stream. `keep_tags` returns the OpusTags packet.
pub(crate) fn scan<R: Read>(input: R, keep_tags: bool) -> Result<(StreamInfo, Option<Vec<u8>>), OpusError> {
    let mut demux = Demux::new(PageReader::new(input));
    // Page 0: OpusHead, alone.
    if !demux.next_page(1024)? { return Err(OpusError::Invalid("the file is empty")); }
    if demux.ends.len() != 1 || demux.packet_pending() {
        return Err(OpusError::Invalid("the OpusHead packet must be alone on the first page"));
    }
    let head = OpusHead::parse(demux.packet(0))?;
    if demux.granule != 0 { return Err(OpusError::Invalid("the OpusHead page must have granule position 0")); }
    let serial = demux.serial().expect("the first page sets the serial");
    // Comment header: any number of pages; the one that completes it holds nothing else.
    let mut pages = 1u64;
    let (comment_bytes, comment_count, kept, audio_offset, audio_sequence) = loop {
        if !demux.next_page(MAX_COMMENT_HEADER)? { return Err(OpusError::Invalid("the stream ends before its comment header")); }
        pages += 1;
        if demux.flags & FLAG_EOS != 0 { return Err(OpusError::Invalid("the stream ends before audio data")); }
        match demux.ends.len() {
            // RFC 7845 s4: a page entirely spanned by one packet has granule position -1.
            0 if demux.granule != NO_GRANULE => return Err(OpusError::Invalid("a comment page that completes no packet must have granule position -1")),
            0 => continue,
            1 if !demux.packet_pending() => {
                if demux.granule != 0 { return Err(OpusError::Invalid("the comment header must complete on a page with granule position 0")); }
                let packet = demux.packet(0);
                let count = CommentHeader::validate(packet)?;
                break (packet.len() as u64, count, keep_tags.then(|| packet.to_vec()), demux.next_offset, demux.sequence_of_page.wrapping_add(1));
            }
            _ => return Err(OpusError::Invalid("audio data shares a page with the comment header")),
        }
    };

    let mut decoded = 0u64; // samples of every completed packet so far (C)
    let mut audio_packets = 0u64;
    let mut audio_bytes = 0u64;
    let mut initial = 0u64;
    let mut previous_granule = 0u64;
    let mut first = true;
    let mut end_trim = 0u64;
    loop {
        if !demux.next_page(MAX_AUDIO_PACKET)? { return Err(OpusError::Invalid("the stream has no end-of-stream page (truncated)")); }
        pages += 1;
        let eos = demux.flags & FLAG_EOS != 0;
        let mut samples = 0u64;
        for index in 0..demux.ends.len() {
            let packet = demux.packet(index);
            if packet.is_empty() { return Err(OpusError::Invalid("a zero-length audio packet")); }
            let n = head::packet_samples(packet).ok_or(OpusError::Invalid("an audio packet is malformed (invalid table of contents)"))?;
            samples += u64::from(n);
            audio_bytes += packet.len() as u64;
            audio_packets += 1;
        }
        if demux.ends.is_empty() {
            if eos {
                // A final page without packets can only restate the running granule.
                if demux.granule != NO_GRANULE && demux.granule != previous_granule {
                    return Err(OpusError::Invalid("an empty final page changes the granule position"));
                }
                break;
            }
            // RFC 7845 s4: a page entirely spanned by one packet has granule position -1.
            if demux.granule != NO_GRANULE { return Err(OpusError::Invalid("an audio page that completes no packet must have granule position -1")); }
            continue;
        }
        let granule = demux.granule;
        if granule == NO_GRANULE || granule > i64::MAX as u64 { return Err(OpusError::Invalid("a page that completes packets has no valid granule position")); }
        decoded += samples;
        if first {
            if eos {
                // RFC 7845 s4.5: an end-trimmed first page works forward from zero.
                if granule < u64::from(head.pre_skip) { return Err(OpusError::Invalid("pre-skip exceeds the stream length")); }
                if granule >= samples { initial = granule - samples; } else { initial = 0; end_trim = samples - granule; }
            } else {
                if granule < samples { return Err(OpusError::Invalid("the first audio page's granule position is smaller than its sample count")); }
                initial = granule - samples;
            }
            first = false;
        } else if eos {
            if granule < previous_granule { return Err(OpusError::Invalid("granule positions decrease")); }
            let expected = initial + decoded;
            if granule > expected { return Err(OpusError::Invalid("the final granule position exceeds the decoded samples")); }
            end_trim = expected - granule;
        } else if granule != initial + decoded {
            return Err(OpusError::Invalid("a page's granule position disagrees with the durations of its packets"));
        }
        previous_granule = granule;
        if eos { break; }
    }
    // RFC 3533: a packet continued past the last page never completes; the audio it would carry is lost,
    // so the stream is damaged rather than silently shorter.
    if demux.packet_pending() { return Err(OpusError::Invalid("the end-of-stream page ends in an unfinished packet")); }
    if audio_packets == 0 { return Err(OpusError::Invalid("the stream has no audio packets")); }
    demux.expect_end()?;
    let frames = decoded.checked_sub(end_trim).and_then(|v| v.checked_sub(u64::from(head.pre_skip)))
        .ok_or(OpusError::Invalid("pre-skip exceeds the stream length"))?;
    let info = StreamInfo { head, serial, frames, decoded_samples: decoded, audio_packets, audio_bytes, pages,
        initial_granule: initial, final_granule: previous_granule, end_trim, audio_offset, audio_sequence,
        comment_header_bytes: comment_bytes, comment_count };
    Ok((info, kept))
}

pub(crate) fn scan_file(path: &Path, keep_tags: bool) -> Result<(StreamInfo, Option<Vec<u8>>), OpusError> {
    let file = File::open(path).map_err(|e| io_error("open", e))?;
    scan(BufReader::with_capacity(64 * 1024, file), keep_tags)
}

/// Sequential decoder with exact pre-skip and end-trim handling.
pub struct OpusReader {
    info: StreamInfo,
    demux: Demux<BufReader<File>>,
    decoder: DecoderState,
    channels: usize,
    next_packet: usize,
    pcm: Vec<i32>,
    ready_start: usize,
    ready_end: usize,
    skip: u64,
    remaining: u64,
    failed: bool,
    tags: Option<Vec<u8>>,
}

impl OpusReader {
    /// Open and fully validate `path`. `sample_rate` must be absent or 48 kHz.
    pub fn open(lib: &OpusLibrary, path: impl AsRef<Path>, sample_rate: Option<f64>) -> Result<Self, OpusError> {
        Self::open_with(lib, path, sample_rate, false)
    }
    /// As `open`; `keep_tags` retains the raw OpusTags packet (see `tags`).
    pub fn open_with(lib: &OpusLibrary, path: impl AsRef<Path>, sample_rate: Option<f64>, keep_tags: bool) -> Result<Self, OpusError> {
        if let Some(rate) = sample_rate {
            if rate != f64::from(SAMPLE_RATE) { return Err(OpusError::SampleRate { source: f64::from(SAMPLE_RATE), requested: rate }); }
        }
        let path = path.as_ref();
        let (info, tags) = scan_file(path, keep_tags)?;
        let mut file = File::open(path).map_err(|e| io_error("open", e))?;
        file.seek(SeekFrom::Start(info.audio_offset)).map_err(|e| io_error("seek", e))?;
        let channels = usize::from(info.head.channels);
        let decoder = DecoderState::new(lib, channels, info.head.output_gain)?;
        let mut demux = Demux::new(PageReader::at(BufReader::with_capacity(64 * 1024, file), info.audio_offset));
        demux.resume(info.serial, info.audio_sequence.wrapping_sub(1));
        Ok(Self { skip: u64::from(info.head.pre_skip), remaining: info.frames, demux, decoder, channels, next_packet: 0,
            pcm: vec![0; 5760 * channels], ready_start: 0, ready_end: 0, failed: false, info, tags })
    }

    pub fn info(&self) -> &StreamInfo { &self.info }
    pub fn channels(&self) -> usize { self.channels }
    /// The raw OpusTags packet, when requested at open time.
    pub fn tags(&self) -> Option<&[u8]> { self.tags.as_deref() }
    pub fn comment_header(&self) -> Result<Option<CommentHeader>, OpusError> {
        self.tags.as_deref().map(CommentHeader::parse).transpose()
    }
    pub fn close(self) -> Result<(), OpusError> { Ok(()) }

    /// Decode the next audible packet into `pcm[ready_start..ready_end]`; false
    /// at the end of the stream.
    fn refill(&mut self) -> Result<bool, OpusError> {
        loop {
            if self.remaining == 0 { return Ok(false); }
            if self.next_packet >= self.demux.ends.len() {
                if self.demux.saw_eos() || !self.demux.next_page(MAX_AUDIO_PACKET)? {
                    return Err(OpusError::Invalid("the stream ended before its declared length"));
                }
                self.next_packet = 0;
                continue;
            }
            let index = self.next_packet;
            self.next_packet += 1;
            let packet = self.demux.packet(index);
            let expected = head::packet_samples(packet).ok_or(OpusError::Invalid("an audio packet is malformed (invalid table of contents)"))? as usize;
            let frames = self.decoder.decode(packet, &mut self.pcm, 5760)?;
            if frames != expected { return Err(OpusError::Decode { operation: "packet duration", code: frames as i32 }); }
            let mut start = 0usize;
            if self.skip > 0 {
                let dropped = self.skip.min(frames as u64) as usize;
                self.skip -= dropped as u64;
                start = dropped;
            }
            let take = ((frames - start) as u64).min(self.remaining) as usize;
            if take == 0 { continue; }
            self.ready_start = start * self.channels;
            self.ready_end = (start + take) * self.channels;
            self.remaining -= take as u64;
            return Ok(true);
        }
    }

    /// Read up to `out.len() / channels` interleaved frames; 0 means end of stream.
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize, OpusError> {
        if self.failed { return Err(OpusError::Poisoned); }
        let channels = self.channels;
        let want = out.len() / channels;
        let mut produced = 0usize;
        let result = (|| -> Result<(), OpusError> {
            while produced < want {
                if self.ready_start == self.ready_end && !self.refill()? { break; }
                let n = ((self.ready_end - self.ready_start) / channels).min(want - produced);
                let source = &self.pcm[self.ready_start..self.ready_start + n * channels];
                for (dst, &src) in out[produced * channels..(produced + n) * channels].iter_mut().zip(source) { *dst = i24_to_f32(src); }
                self.ready_start += n * channels;
                produced += n;
            }
            Ok(())
        })();
        if let Err(e) = result { self.failed = true; return Err(e); }
        Ok(produced)
    }

    /// Read into two planes; mono is duplicated into both.
    pub fn read_stereo_planar(&mut self, left: &mut [f32], right: &mut [f32]) -> Result<usize, OpusError> {
        if self.failed { return Err(OpusError::Poisoned); }
        if left.len() != right.len() { return Err(OpusError::Invalid("stereo plane lengths differ")); }
        let channels = self.channels;
        let want = left.len();
        let mut produced = 0usize;
        let result = (|| -> Result<(), OpusError> {
            while produced < want {
                if self.ready_start == self.ready_end && !self.refill()? { break; }
                let n = ((self.ready_end - self.ready_start) / channels).min(want - produced);
                for i in 0..n {
                    let base = self.ready_start + i * channels;
                    let l = i24_to_f32(self.pcm[base]);
                    left[produced + i] = l;
                    right[produced + i] = if channels == 1 { l } else { i24_to_f32(self.pcm[base + 1]) };
                }
                self.ready_start += n * channels;
                produced += n;
            }
            Ok(())
        })();
        if let Err(e) = result { self.failed = true; return Err(e); }
        Ok(produced)
    }
}
