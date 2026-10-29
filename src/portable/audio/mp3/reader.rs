//! Apple-equivalent MP3 reader: `Container` (AudioFile) + `Mp3Codec<SpiritDecoder>` (ACMP3Decoder) + the converter /
//! `ExtAudioFileRead` protocol around them, assembled so that it returns exactly what the engine's `apple_codec::Reader`
//! obtains from `ExtAudioFileRead` for an MP3 file: the same trimmed int16 samples (float32 = int16 / 32768 exactly), the
//! same lengths, the same open refusals and the same read errors, for every read size.
//!
//! # The protocol (verified with lldb traces of `ExtAudioFileRead` / `ReadInputProc` / `AudioCodec*`, see REPORT.md)
//!
//! `ExtAudioFileRead(F)` (AudioToolboxCore `_ExtAudioFileRead` 0x19e9bd70c):
//!
//! 1. while priming frames are left to skip (`Seek(0)` armed them): ask the converter for `chunk = min(skip, F)` frames
//!    (`AudioConverterFillComplexBuffer`), with `ceil(chunk / fpp)` as the packet budget of its input callback, and discard
//!    them; a call that yields 0 frames ends the read with 0 frames;
//! 2. ask the converter for the `F` frames of the read with a packet budget of `F / fpp + 1` (integer division), whatever
//!    the valid-frame clamp still allows; then cut what lies behind the clamp (`total`): the position never passes `total`,
//!    frames beyond it are consumed and dropped (a read at the clamp is a full converter call that returns 0 frames).
//!
//! The converter (`acv2::AudioConverterChain` -> `CodecConverter`) keeps decoded frames and queued packets between calls.
//! One call (`AudioConverterFillComplexBuffer`) works like this:
//!
//! * while it holds fewer than the requested frames it takes the next queued packet (`AppendInputData` of one packet +
//!   `ProduceOutputPackets`, decoding lazily: packets that are not needed yet stay queued);
//! * a failed produce (-50) skips the packet and counts as a failure ([`BadDataGuard`]: the fifth consecutive failure
//!   aborts the call with `'bada'`, the fourth when the call has no output yet);
//! * when no packet is queued it calls `ExtAudioFile::ReadInputProc` (0x19e9013c0), which reads
//!   `min(budget, 65536 / packet size upper bound)` packets with `AudioFileReadPacketData`
//!   (`AudioFileObject::ReadPacketDataVBR` 0x19eb25a8c, modelled by [`Container::read_packets`]): it scans the packet
//!   table up to the first packet of the group, and when the table does not yet reach the end of the group it scans a second
//!   time up to ONE PACKET BEYOND the group (a group of two or more packets needs the successor of its last packet);
//!   a scan that hits the failure of the first ten packets makes the WHOLE group read fail with `'dta?'`; a group that
//!   runs past the end of the table is shortened, a start at the end returns 0 packets;
//! * a failing input callback aborts the call at once while it has no output yet; otherwise (it already holds frames) the
//!   callback is called a second time (the failure deleted the packet table: the retry scans afresh, and a stream whose
//!   scan only ended by the ten-candidates rule then succeeds) unless the failure counter has reached its limit; a second
//!   failure aborts the call with the callback's status (`'dta?'`);
//! * at the end of the data (0 packets) it flushes the codec (`Produce`, `AppendInputData(0)`, `Produce` = the 529 zero
//!   frames of the decoder-delay tail) and answers with what it has (0 frames forever after).
//!
//! A failed read poisons the reader (the engine does the same).

use super::codec::{CodecError, Layer, Mp3Codec, STATUS_FAILURE};
use super::container::{self, Container, Refusal, SourceFormat, TableState, FORMAT_MP1, FORMAT_MP2};
use super::decoder::SpiritDecoder;
use super::gapless::{BadDataGuard, Gapless};

/// Why a reader call failed. `status()` is the `OSStatus` Apple would have returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderError {
    /// `ExtAudioFileOpenURL` refused the file.
    Open(Refusal),
    /// `ExtAudioFileSetProperty(ClientDataFormat)` failed (`'fmt?'`).
    ClientFormat(i32),
    /// `ExtAudioFileRead` failed (`'dta?'` for a failed packet scan, `'bada'` for undecodable data).
    Read(i32),
    /// A read after a failed read.
    Poisoned,
    /// The codec shim rejected a call (not reachable with a well-formed container).
    Codec(CodecError),
}

impl ReaderError {
    /// The `OSStatus` of the native call (0 for [`ReaderError::Poisoned`], which has no native equivalent).
    pub fn status(&self) -> i32 {
        match *self {
            ReaderError::Open(r) => r.os_status(),
            ReaderError::ClientFormat(s) | ReaderError::Read(s) => s,
            ReaderError::Poisoned => 0,
            ReaderError::Codec(e) => e.0,
        }
    }
}

/// Default size of `ExtAudioFile`'s packet buffer (`+0x58`); the input callback never reads more packets than fit.
const IO_BUFFER_BYTES: u32 = 65536;

/// The pure-Rust equivalent of the engine's MP3 `Reader`.
pub struct Mp3Reader {
    data: Vec<u8>,
    container: Container,
    codec: Mp3Codec<SpiritDecoder>,
    channels: usize,
    /// Frames per packet of the file format (`ExtAudioFile` +0xbc).
    fpp: usize,
    /// Most packets one input callback may deliver (`ExtAudioFile` +0x5c).
    max_group: usize,
    /// Next packet `AudioFileReadPacketData` is asked for (`ExtAudioFile` +0x30).
    file_pos: usize,
    /// The lazily built AudioFile packet table.
    table: TableState,
    /// Packets `[queue_next, queue_end)` were read from the file and are not yet fed to the codec.
    queue_next: usize,
    queue_end: usize,
    /// The input callback returned 0 packets and the converter flushed the codec.
    eof: bool,
    guard: BadDataGuard,
    buf: Vec<i16>,
    /// Converter output (interleaved source channels, before priming / clamp trimming) not yet consumed by a read.
    out: Vec<i16>,
    out_head: usize,
    /// Priming frames still to skip (`ExtAudioFile` +0x54).
    skip: u64,
    /// Frames delivered so far, clamped to `total` (`ExtAudioFile` +0x40).
    pos: i64,
    /// Valid-frame clamp, 0 = none (`ExtAudioFile` +0x48).
    total: i64,
    failed: bool,
    /// Frames the last read put into the caller's buffer (delivered and behind the clamp).
    written: usize,
    trace: Option<Vec<String>>,
}

impl Mp3Reader {
    /// `ExtAudioFileOpenURL` + set client format (same rate as the file, float): everything the engine's `Reader::open` does.
    pub fn open(data: Vec<u8>) -> Result<Self, ReaderError> {
        let container = container::parse(&data).map_err(ReaderError::Open)?;
        let status = container.client_format_status();
        if status != 0 {
            return Err(ReaderError::ClientFormat(status));
        }
        let s = container.source_format();
        let layer = match s.format_id {
            FORMAT_MP1 => Layer::I,
            FORMAT_MP2 => Layer::II,
            _ => Layer::III,
        };
        let codec = Mp3Codec::<SpiritDecoder>::open_like_converter_for(layer, f64::from(s.sample_rate), s.channels, s.frames_per_packet)
            .map_err(ReaderError::Codec)?;
        let buf = vec![0i16; (s.frames_per_packet * 2 * s.channels / 2) as usize];
        let trim = Gapless::new(&container);
        Ok(Mp3Reader {
            data,
            channels: s.channels as usize,
            fpp: s.frames_per_packet as usize,
            max_group: (IO_BUFFER_BYTES / container.packet_size_upper_bound.max(1)).max(1) as usize,
            container,
            codec,
            file_pos: 0,
            table: TableState::default(),
            queue_next: 0,
            queue_end: 0,
            eof: false,
            guard: BadDataGuard::new(),
            buf,
            out: Vec::new(),
            out_head: 0,
            skip: trim.priming(),
            pos: 0,
            total: trim.total_frames(),
            failed: false,
            written: 0,
            trace: None,
        })
    }

    /// `kExtAudioFileProperty_FileDataFormat`.
    pub fn source_format(&self) -> SourceFormat {
        self.container.source_format()
    }

    /// `kExtAudioFileProperty_FileLengthFrames`.
    pub fn file_length_frames(&self) -> i64 {
        self.container.file_length_frames()
    }

    /// `kAudioFilePropertyBitRate` (the engine's `Reader::bitrate`).
    pub fn bitrate(&self) -> u32 {
        self.container.bitrate()
    }

    /// Number of source channels (1 for mono, else 2).
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The container model (tags, packets, packet table info).
    pub fn container(&self) -> &Container {
        &self.container
    }

    /// Diagnostics: record the converter call structure (one entry per `ExtAudioFileRead`: `X<frames>`, then `F<frames>`
    /// per converter call, `R(max<n>)-><k>` per input callback, `a`/`p<frames>`/`pF` per Append/Produce) in the notation of
    /// `tools/inp_summary.py`, which prints the same for the real converter (lldb trace).
    pub fn set_trace(&mut self, on: bool) {
        self.trace = if on { Some(Vec::new()) } else { None };
    }

    /// Diagnostics: the lines recorded since [`Self::set_trace`] / the last call.
    pub fn take_trace(&mut self) -> Vec<String> {
        self.trace.as_mut().map(std::mem::take).unwrap_or_default()
    }

    fn note(&mut self, s: impl FnOnce() -> String) {
        if let Some(t) = self.trace.as_mut() {
            t.push(s());
        }
    }

    fn out_frames(&self) -> usize {
        (self.out.len() - self.out_head) / self.channels
    }

    fn drop_frames(&mut self, n: usize) {
        self.out_head += n * self.channels;
        if self.out_head == self.out.len() {
            self.out.clear();
            self.out_head = 0;
        } else if self.out_head > 1 << 20 {
            self.out.drain(..self.out_head);
            self.out_head = 0;
        }
    }

    /// One `AudioCodecProduceOutputPackets`; its frames join the converter output, a failure is counted.
    fn produce(&mut self) -> Result<(), ReaderError> {
        let p = self.codec.produce(&mut self.buf).map_err(ReaderError::Codec)?;
        if p.status == STATUS_FAILURE {
            self.note(|| "pF".into());
            return self.guard.produce_failed().map_err(ReaderError::Read);
        }
        let n = p.bytes as usize / 2 / self.channels;
        self.note(|| format!("p{n}"));
        self.guard.produced(n);
        self.out.extend_from_slice(&self.buf[..n * self.channels]);
        Ok(())
    }

    /// `ExtAudioFile::ReadInputProc` for a budget of `budget` packets: `AudioFileReadPacketData(file_pos, budget)`.
    fn read_group(&mut self, budget: usize) -> Result<usize, ReaderError> {
        let budget = budget.clamp(1, self.max_group);
        match self.container.read_packets(&self.data, &mut self.table, self.file_pos, budget) {
            Err(status) => {
                self.note(|| format!("R(max{budget})->dta?"));
                Err(ReaderError::Read(status))
            }
            Ok(count) => {
                self.note(|| format!("R(max{budget})->{count}"));
                self.queue_next = self.file_pos;
                self.queue_end = self.file_pos + count;
                self.file_pos += count;
                Ok(count)
            }
        }
    }

    /// The converter's handling of a failing input callback: when the call has no output yet it is aborted at once;
    /// otherwise the callback is called a second time (the failed read deleted the packet table, so the retry scans
    /// afresh) unless the failure counter has already reached its limit.
    fn read_group_retrying(&mut self, budget: usize) -> Result<usize, ReaderError> {
        match self.read_group(budget) {
            Err(e) if self.out_frames() > 0 && !self.guard.failure() => {
                self.note(|| "retry".into());
                self.read_group(budget).map_err(|_| e)
            }
            r => r,
        }
    }

    /// `AudioConverterFillComplexBuffer(want frames)` with an input budget of `budget` packets: decode until `want` frames
    /// are available or the stream has ended.
    fn fill(&mut self, want: usize, budget: usize) -> Result<(), ReaderError> {
        self.note(|| format!("F{want}"));
        self.guard.begin_call(self.out_frames() > 0);
        while self.out_frames() < want && !self.eof {
            if self.queue_next >= self.queue_end && self.read_group_retrying(budget)? == 0 {
                // end of data: flush the codec (the 529 zero frames of the decoder-delay tail)
                self.produce()?;
                self.note(|| "a".into());
                self.codec.append_eof().map_err(ReaderError::Codec)?;
                self.produce()?;
                self.eof = true;
                break;
            }
            let p = self.table.packet(self.queue_next).ok_or(ReaderError::Codec(CodecError(super::codec::ERR_DATA)))?;
            self.queue_next += 1;
            self.note(|| "a".into());
            let pkt = self.data.get(p.offset as usize..p.offset as usize + p.size as usize).ok_or(ReaderError::Codec(CodecError(super::codec::ERR_DATA)))?;
            self.codec.append_packet(pkt).map_err(ReaderError::Codec)?;
            self.produce()?;
        }
        Ok(())
    }

    /// `ExtAudioFileRead` for `frames` frames into `dst` (interleaved int16, `channels()` samples per frame; `dst` must hold
    /// `frames * channels()` samples). Returns the frames delivered: `frames` unless the stream ends, then fewer, then 0.
    ///
    /// The converter fills the caller's buffer with every frame it decoded for the call and `ExtAudioFile` only lowers the
    /// returned count at the valid-frame clamp: the frames behind the clamp (the decoder's padding and the 529-frame delay
    /// tail, up to the requested size; each read at the clamp gets the next ones) are in `dst` after the delivered ones,
    /// [`Self::last_written`] counts them. The engine pulls whole blocks and processes them, so they reach its effects.
    pub fn read(&mut self, dst: &mut [i16], frames: usize) -> Result<usize, ReaderError> {
        if self.failed {
            return Err(ReaderError::Poisoned);
        }
        let want = frames.min(dst.len() / self.channels);
        if want == 0 {
            self.written = 0;
            return Ok(0);
        }
        self.note(|| format!("X{want}"));
        let r = self.read_frames(dst, want);
        if r.is_err() {
            self.failed = true;
        }
        r
    }

    /// Frames in `dst` after the last [`Self::read`]: the delivered ones and the decoded frames behind the valid-frame clamp.
    pub fn last_written(&self) -> usize {
        self.written
    }

    fn read_frames(&mut self, dst: &mut [i16], want: usize) -> Result<usize, ReaderError> {
        let ch = self.channels;
        self.written = 0;
        while self.skip > 0 {
            let chunk = (self.skip as usize).min(want);
            self.fill(chunk, chunk.div_ceil(self.fpp))?;
            let got = chunk.min(self.out_frames());
            // The priming frames are decoded into the caller's buffer too (and overwritten by what follows, except past
            // the end of the stream: a stream that ends inside the priming leaves them there).
            dst[..got * ch].copy_from_slice(&self.out[self.out_head..self.out_head + got * ch]);
            self.written = self.written.max(got);
            self.drop_frames(got);
            if got == 0 {
                return Ok(0);
            }
            self.skip -= got as u64;
        }
        self.fill(want, want / self.fpp + 1)?;
        let got = want.min(self.out_frames());
        let end = self.pos + got as i64;
        let mut deliver = got;
        if self.total != 0 && self.total <= end {
            deliver = got - (end - self.total).min(got as i64) as usize;
            self.pos = self.total;
        } else {
            self.pos = end;
        }
        dst[..got * ch].copy_from_slice(&self.out[self.out_head..self.out_head + got * ch]);
        self.written = self.written.max(got);
        self.drop_frames(got);
        Ok(deliver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::container::STATUS_DAT;
    use super::super::gapless::STATUS_BAD_DATA;

    /// MPEG-1 Layer III 128 kbps 44.1 kHz joint-stereo frames with an all-zero payload (silence), 417/418 bytes.
    fn frames(n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut err = 0u32;
        for _ in 0..n {
            err += 96;
            let pad = u32::from(err >= 100);
            if pad == 1 {
                err -= 100;
            }
            let w = (0x7ffu32 << 21) | (3 << 19) | (1 << 17) | (1 << 16) | (9 << 12) | (pad << 9) | (1 << 6);
            out.extend_from_slice(&w.to_be_bytes());
            out.resize(out.len() + 413 + pad as usize, 0);
        }
        out
    }

    /// `k` frames, 40 foreign headers (the successor search of frame `k - 1` fails), 20 more frames.
    fn broken(k: usize) -> Vec<u8> {
        let mut s = frames(k);
        for _ in 0..40 {
            s.extend_from_slice(&[0xff, 0xfb, 0x94, 0x04]);
        }
        s.extend(frames(20));
        s
    }

    /// (frames delivered, final status) of a read loop with a constant block size, until a read returns 0 or fails.
    fn drain(data: Vec<u8>, block: usize) -> (usize, i32) {
        let mut r = Mp3Reader::open(data).unwrap();
        let mut pcm = vec![0i16; block * r.channels()];
        let mut total = 0;
        loop {
            match r.read(&mut pcm, block) {
                Ok(0) => return (total, 0),
                Ok(n) => total += n,
                Err(e) => return (total, e.status()),
            }
        }
    }

    // The expectations below were measured with ExtAudioFileRead (engine call order, planar float client) on macOS 27.2
    // for exactly these byte streams (tools/rdseq.c); they are the read-size dependence of a failed packet scan.
    #[test]
    fn failed_scan_fails_the_whole_group_read() {
        let cases: [(usize, [(usize, usize); 6]); 2] = [
            (5, [(1, 4079), (100, 4000), (512, 3584), (1153, 1153), (2305, 2305), (4609, 0)]),
            (8, [(1, 7535), (100, 7500), (512, 7168), (1153, 5765), (2305, 4610), (4609, 4609)]),
        ];
        for (k, rows) in cases {
            for (block, frames) in rows {
                assert_eq!(drain(broken(k), block), (frames, STATUS_DAT), "{k} frames, block {block}");
            }
        }
    }

    #[test]
    fn ten_candidate_end_is_a_clean_end_for_every_block_size() {
        for block in [1, 100, 512, 1153, 2305, 4609] {
            assert_eq!(drain(broken(11), block), (11 * 1152, 0), "block {block}");
            assert_eq!(drain(frames(20), block), (20 * 1152, 0), "block {block}");
        }
    }

    #[test]
    fn mixed_read_sizes_deliver_the_same_stream() {
        let mut a = Mp3Reader::open(frames(20)).unwrap();
        let mut b = Mp3Reader::open(frames(20)).unwrap();
        let mut whole = Vec::new();
        let mut buf = vec![0i16; 8192 * 2];
        while let Ok(n) = b.read(&mut buf, 4096) {
            if n == 0 {
                break;
            }
            whole.extend_from_slice(&buf[..n * 2]);
        }
        let mut pieces = Vec::new();
        for size in [1usize, 700, 3, 1153, 5000, 64].iter().cycle().take(400) {
            match a.read(&mut buf, *size) {
                Ok(0) | Err(_) => break,
                Ok(n) => pieces.extend_from_slice(&buf[..n * 2]),
            }
        }
        assert_eq!(pieces, whole);
        assert_eq!(whole.len(), 20 * 1152 * 2);
    }

    #[test]
    fn undecodable_frames_abort_after_four_or_five_in_a_row() {
        // 6 good frames (priming-free tag-less stream), then frames whose side info claims 511 big values in every granule
        // and channel: the decoder rejects them (-50); four are tolerated after output, the fifth aborts with 'bada'.
        let bad = |n: usize| {
            let mut s = frames(6);
            for _ in 0..n {
                let mut f = frames(1);
                for g in 0..2 {
                    for c in 0..2 {
                        let o = 32 + 20 + (g * 2 + c) * 59 + 12;
                        for i in 0..9 {
                            let p = o + i;
                            f[p / 8] |= 0x80 >> (p % 8);
                        }
                    }
                }
                s.extend(f);
            }
            s.extend(frames(8));
            s
        };
        assert_eq!(drain(bad(4), 512).1, 0);
        assert_eq!(drain(bad(5), 512).1, STATUS_BAD_DATA);
    }
}
