//! Ogg Opus output: a streaming encoder with a fixed packet and page layout,
//! and the final comment-header rewrite that fixes the stream's serial number.
//!
//! The layout is a pure function of the encoded packets: 20 ms packets; page 0
//! holds OpusHead; the OpusTags packet follows on pages of its own; audio pages
//! close after one second of audio or 255 segments; the last page carries EOS
//! and the exact end granule `pre_skip + frames`. The serial number is the first
//! four bytes (little endian) of a SHA-256 over the headers and every audio
//! packet, so identical streams always share a serial and distinct streams
//! almost never do (which matters when files are concatenated into a chain).

use super::head::{self, CommentHeader, MAX_AUDIO_PACKET, OpusHead};
use super::ogg::{PageReader, PageWriter};
use super::reader::{Demux, scan_file};
use super::{EncoderState, FRAME_SAMPLES, OpusError, OpusLibrary, PAGE_SAMPLES, SAMPLE_RATE, f32_to_i24, io_error};
use crate::portable::digest::Sha256;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Serial used until the final rewrite replaces it.
const PROVISIONAL_SERIAL: u32 = 0x5355_504f;

/// Streaming Ogg Opus encoder. Input is interleaved float at 48 kHz.
pub struct OpusWriter {
    encoder: EncoderState,
    out: PageWriter<BufWriter<File>>,
    channels: usize,
    pre_skip: u16,
    block: Vec<i32>,
    filled: usize,
    input_frames: u64,
    encoded_frames: u64,
    packet: Vec<u8>,
    failed: bool,
}

impl OpusWriter {
    /// `file` must be a new, empty, writable file. The headers are written at
    /// once with a comment-free OpusTags; `repack_with_comments` replaces it.
    pub fn create(lib: &OpusLibrary, file: File, channels: usize) -> Result<Self, OpusError> {
        if !(1..=2).contains(&channels) { return Err(OpusError::Unsupported("Opus output supports mono and stereo only")); }
        let encoder = EncoderState::new(lib, channels)?;
        let lookahead = encoder.lookahead()?;
        let pre_skip = u16::try_from(lookahead).map_err(|_| OpusError::Encode { operation: "lookahead", code: lookahead })?;
        let mut out = PageWriter::new(BufWriter::with_capacity(64 * 1024, file), PROVISIONAL_SERIAL);
        let head = OpusHead { version: 1, channels: channels as u8, pre_skip, input_sample_rate: SAMPLE_RATE, output_gain: 0 };
        let tags = CommentHeader { vendor: lib.identity().version.clone().into_bytes(), comments: Vec::new(), trailing: Vec::new() };
        out.write_header_packet(&head.encode()).map_err(|e| io_error("write headers", e))?;
        out.write_header_packet(&tags.encode()?).map_err(|e| io_error("write headers", e))?;
        Ok(Self { encoder, out, channels, pre_skip, block: vec![0; FRAME_SAMPLES * channels], filled: 0,
            input_frames: 0, encoded_frames: 0, packet: vec![0; 4000], failed: false })
    }

    pub fn pre_skip(&self) -> u16 { self.pre_skip }
    pub fn input_frames(&self) -> u64 { self.input_frames }

    fn flush_block(&mut self) -> Result<(), OpusError> {
        let n = self.encoder.encode(&self.block, &mut self.packet)?;
        let packet = &self.packet[..n];
        if n > MAX_AUDIO_PACKET { return Err(OpusError::Encode { operation: "packet size", code: n as i32 }); }
        self.out.push_audio(packet, FRAME_SAMPLES as u64, PAGE_SAMPLES).map_err(|e| io_error("write audio", e))?;
        self.encoded_frames += FRAME_SAMPLES as u64;
        self.filled = 0;
        Ok(())
    }

    fn push_i24(&mut self, value: i32) -> Result<(), OpusError> {
        self.block[self.filled] = value;
        self.filled += 1;
        if self.filled == self.block.len() { self.flush_block()?; }
        Ok(())
    }

    /// Append interleaved frames (a whole number of frames).
    pub fn write(&mut self, samples: &[f32]) -> Result<(), OpusError> {
        if self.failed { return Err(OpusError::Poisoned); }
        if !samples.len().is_multiple_of(self.channels) { return Err(OpusError::Invalid("input is not a whole number of frames")); }
        let result = (|| -> Result<(), OpusError> {
            for &sample in samples { self.push_i24(f32_to_i24(sample))?; }
            self.input_frames += (samples.len() / self.channels) as u64;
            Ok(())
        })();
        if result.is_err() { self.failed = true; }
        result
    }

    /// Pad to cover the encoder delay and a whole packet, write the end-of-stream
    /// page with the exact granule, and return the file.
    pub fn finish(mut self) -> Result<File, OpusError> {
        if self.failed { return Err(OpusError::Poisoned); }
        let pre = u64::from(self.pre_skip);
        let needed = (self.input_frames + pre).div_ceil(FRAME_SAMPLES as u64).max(1) * FRAME_SAMPLES as u64;
        let pending = (self.filled / self.channels) as u64;
        let mut produced = self.encoded_frames + pending;
        while produced < needed {
            for _ in 0..self.channels { self.push_i24(0)?; }
            produced += 1;
        }
        debug_assert_eq!(self.filled, 0);
        self.out.finish(pre + self.input_frames).map_err(|e| io_error("finish", e))?;
        let writer = self.out.into_inner();
        writer.into_inner().map_err(|e| io_error("flush", e.into_error()))
    }
}

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// An anonymous (already unlinked), owner-only scratch file.
fn scratch_file() -> Result<File, OpusError> {
    let dir = std::env::temp_dir();
    for _ in 0..64 {
        let name = format!(".pn-opus-{}-{:x}", std::process::id(), NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed));
        let path = dir.join(name);
        match OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path) {
            Ok(file) => {
                std::fs::remove_file(&path).map_err(|e| io_error("remove scratch file", e))?;
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error("create scratch file", e)),
        }
    }
    Err(io_error("create scratch file", std::io::Error::other("no unique scratch name")))
}

/// Visit every audio packet of an already validated stream.
fn for_each_audio_packet(path: &Path, info: &super::reader::StreamInfo,
                         mut visit: impl FnMut(&[u8], u32) -> Result<(), OpusError>) -> Result<(), OpusError> {
    let mut file = File::open(path).map_err(|e| io_error("open", e))?;
    file.seek(SeekFrom::Start(info.audio_offset)).map_err(|e| io_error("seek", e))?;
    let mut demux = Demux::new(PageReader::at(BufReader::with_capacity(64 * 1024, file), info.audio_offset));
    demux.resume(info.serial, info.audio_sequence.wrapping_sub(1));
    while !demux.saw_eos() {
        if !demux.next_page(MAX_AUDIO_PACKET)? { return Err(OpusError::Invalid("the stream ended unexpectedly")); }
        for index in 0..demux.ends.len() {
            let packet = demux.packet(index);
            let samples = head::packet_samples(packet).ok_or(OpusError::Invalid("an audio packet is malformed (invalid table of contents)"))?;
            visit(packet, samples)?;
        }
    }
    Ok(())
}

/// Rewrite the Ogg Opus file at `path` in place with a new comment header: the
/// audio packets, pre-skip and exact length are unchanged, the page layout and
/// serial number are recomputed by the layout rules above. The file keeps its
/// inode. `comments` may be any valid header (see `CommentHeader`).
pub fn repack_with_comments(path: &Path, comments: &CommentHeader) -> Result<(), OpusError> {
    let tags = comments.encode()?;
    let (info, _) = scan_file(path, false)?;
    let head = OpusHead { version: 1, channels: info.head.channels, pre_skip: info.head.pre_skip,
        input_sample_rate: info.head.input_sample_rate, output_gain: info.head.output_gain };
    let head_bytes = head.encode();
    let final_granule = u64::from(info.head.pre_skip) + info.frames;
    // Pass 1: the serial number depends on the whole stream.
    let mut hasher = Sha256::new();
    hasher.update(b"platinum-engine ogg-opus serial v1");
    hasher.update(&head_bytes);
    hasher.update(&(tags.len() as u64).to_le_bytes());
    hasher.update(&tags);
    for_each_audio_packet(path, &info, |packet, _| {
        hasher.update(&(packet.len() as u32).to_le_bytes());
        hasher.update(packet);
        Ok(())
    })?;
    hasher.update(&final_granule.to_le_bytes());
    let digest = hasher.finalize();
    let serial = u32::from_le_bytes(digest[..4].try_into().expect("four bytes"));
    // Pass 2: write the new stream to an anonymous scratch file.
    let mut scratch = BufWriter::with_capacity(256 * 1024, scratch_file()?);
    {
        let mut out = PageWriter::new(&mut scratch, serial);
        out.write_header_packet(&head_bytes).map_err(|e| io_error("write headers", e))?;
        out.write_header_packet(&tags).map_err(|e| io_error("write headers", e))?;
        for_each_audio_packet(path, &info, |packet, samples| {
            out.push_audio(packet, u64::from(samples), PAGE_SAMPLES).map_err(|e| io_error("write audio", e))
        })?;
        out.finish(final_granule).map_err(|e| io_error("finish", e))?;
    }
    let mut scratch = scratch.into_inner().map_err(|e| io_error("flush scratch", e.into_error()))?;
    scratch.seek(SeekFrom::Start(0)).map_err(|e| io_error("rewind scratch", e))?;
    // Copy over the original inode.
    let mut target = OpenOptions::new().write(true).open(path).map_err(|e| io_error("open for rewrite", e))?;
    target.set_len(0).map_err(|e| io_error("truncate", e))?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut target_writer = BufWriter::with_capacity(256 * 1024, &mut target);
    loop {
        let n = scratch.read(&mut buffer).map_err(|e| io_error("read scratch", e))?;
        if n == 0 { break; }
        target_writer.write_all(&buffer[..n]).map_err(|e| io_error("rewrite", e))?;
    }
    target_writer.flush().map_err(|e| io_error("rewrite", e))?;
    drop(target_writer);
    target.sync_all().map_err(|e| io_error("sync", e))?;
    Ok(())
}
