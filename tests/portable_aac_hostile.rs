//! Hostile-input tests for the portable AAC reader: whatever the bytes, opening and reading must neither panic, hang, allocate
//! without bound nor deliver an impossible number of frames; every refusal and read failure must be a typed refusal or one of the
//! `OSStatus` values Apple produces for such files, and the outcome must be deterministic.
//!
//! Seeded mutation of the golden fixtures (`tests/fixtures/aac-decode`) and of a fragmented MP4: truncation, byte and bit damage,
//! insertions and deletions, repeated and swapped ranges, spliced streams (another file's header, rate or channel layout in the
//! middle), forged headers of every container (MP4 `ftyp`, ADTS, LOAS, CAF), damaged and giant box and chunk sizes, inserted and
//! removed boxes, absurd table counts, noise over access units, leading ID3 chains (including a version byte of 0xff, which hangs
//! Apple's MPEG parser) and pure noise. 6 000 cases run through the public `Reader` on files, each under a wall-clock guard, some of
//! them opened with a client rate; a subset runs twice (same read size: identical outcome) and a subset again with other read sizes
//! and flush-to-zero (identical stream). A counting allocator measures every case: no single allocation (17 bytes per input byte) and
//! no total (40 per byte) beyond what the input justifies, plus the fixed state of a decoder (a small constant: a table sized for
//! the input never hides behind it). `scaled_inputs_cost_in_proportion` builds inputs of megabytes whose tables are made to cost a
//! product of two sizes if the reader scanned them naively; `every_structure_costs_in_proportion_to_its_size` builds seven of those
//! structures in two sizes (the second four times the first) and compares CPU times (a quadratic walk costs sixteen times as much, the
//! limit is eight times plus a slack), and `chains_beyond_every_scan_limit_cost_no_more_than_their_size` the chains longer than any scan.
//! Three structure-aware generators (LOAS/LATM frames, CAF chunks, movie fragments) produce 20 000 / 8 000 / 8 000 streams from the
//! grammar of their container, with real access units, configurations of every kind, payload and frame lengths that are exact, short or
//! over-long, and cuts; the LOAS one made the reader panic on a frame behind the first that carries another configuration and
//! a payload its frame cannot hold. None of this compares with Apple; `portable_aac_decode.rs` does.

#[path = "support/aac_decode.rs"]
mod aac_decode;

use aac_decode::{BLOCK, CLIENT, Expect, Fixtures, OPEN, READ, READ_PLANAR, Scratch, allocation_counters, reset_allocation_counters};
use platinum_engine::portable::audio::{CodecError, Reader};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const ITERATIONS: usize = 6_000;
/// Every 8th case is run twice to compare the outcomes, every 8th (the 4th after it) is run again with other read sizes.
const REPEAT_EVERY: usize = 8;
/// The longest any single case may take; a hang would never come back at all.
const CASE_LIMIT: Duration = Duration::from_secs(20);
/// Mutated inputs stay small: damage matters, not size.
const MAX_BYTES: usize = 4_096;
/// Size fields of the golden MP4 fixtures grown to the maximum: how many were grown and how many of the grown files still decode.
const CHECKED_GIANT_FIELDS: usize = 320;
const DECODED_GIANT_FIELDS: usize = 216;
/// The failing call of the bit-rate property.
const BIT_RATE: &str = "AudioFileGetProperty(bit rate)";

/// The most any case allocated at once, and its largest single allocation (over all cases, for the report); the counting allocator is the one of
/// `support/aac_decode.rs`.
static MAX_LIVE: AtomicUsize = AtomicUsize::new(0);
static MAX_LARGEST: AtomicUsize = AtomicUsize::new(0);
/// What the cases held beyond the per-byte limits (the part that does not scale with the input), for the report.
static MAX_FIXED_LIVE: AtomicUsize = AtomicUsize::new(0);
static MAX_FIXED_LARGEST: AtomicUsize = AtomicUsize::new(0);

/// What a reader may allocate for an input of `n` bytes: the file as read, and for a file of tiny packets 16 bytes per packet of it (a packet is a byte
/// at least), in one allocation or all at once, plus the fixed state of a decoder (its buffers and tables) and nothing a damaged size field claims.
const LARGEST_PER_BYTE: usize = 17;
const LIVE_PER_BYTE: usize = 40;
const FIXED_LARGEST: usize = 256 << 10;
const FIXED_LIVE: usize = 1 << 20;

/// What one case cost: the CPU time of its thread and its allocation peaks.
#[derive(Clone, Copy, Debug)]
struct CaseStats { cpu: Duration, largest: usize, live: usize }

/// CPU time of the calling thread (independent of how loaded the machine is).
fn thread_cpu() -> Duration {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `clock_gettime` writes the `timespec` it is given and nothing else.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    assert_eq!(status, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID)");
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize }
    fn byte(&mut self) -> u8 { (self.next() >> 24) as u8 }
}

fn find_all(data: &[u8], needle: &[u8]) -> Vec<usize> {
    if data.len() < needle.len() { return Vec::new(); }
    (0..=data.len() - needle.len()).filter(|&i| &data[i..i + needle.len()] == needle).collect()
}

const BOXES: [&[u8; 4]; 14] = [b"ftyp", b"moov", b"mdat", b"trak", b"mdia", b"minf", b"stbl", b"stsz", b"stco", b"stts", b"stsc", b"esds", b"free", b"udta"];
const COUNT_BOXES: [&[u8; 4]; 4] = [b"stsz", b"stco", b"stts", b"stsc"];

/// Offsets of the size fields of the boxes of a (possibly damaged) MP4 file: where a known box type sits, four bytes behind its start.
fn box_starts(data: &[u8]) -> Vec<usize> {
    BOXES.iter().flat_map(|t| find_all(data, *t)).filter(|&i| i >= 4).map(|i| i - 4).collect()
}

fn adts_syncs(data: &[u8]) -> Vec<usize> {
    (0..data.len().saturating_sub(6)).filter(|&i| data[i] == 0xff && data[i + 1] & 0xf6 == 0xf0).collect()
}

fn put_u32(data: &mut [u8], at: usize, value: u32) { if at + 4 <= data.len() { data[at..at + 4].copy_from_slice(&value.to_be_bytes()); } }

fn mutate(rng: &mut Rng, original: &[u8], other: &[u8], case: usize) -> Vec<u8> {
    let mut data = original.to_vec();
    if data.is_empty() { data = vec![rng.byte()]; }
    match case % 20 {
        0 => data.truncate(rng.below(data.len())),
        1 => for _ in 0..1 + rng.below(30) { let i = rng.below(data.len()); data[i] = rng.byte(); },
        2 => { let at = rng.below(data.len()); let junk: Vec<u8> = (0..rng.below(1000)).map(|_| rng.byte()).collect(); data.splice(at..at, junk); }
        3 => { let at = rng.below(data.len()); let end = (at + rng.below(1500)).min(data.len()); data.drain(at..end); }
        4 => data = (0..rng.below(4000)).map(|_| rng.byte()).collect(),
        5 => for _ in 0..1 + rng.below(6) { let i = rng.below(data.len().min(1200)); data[i] ^= 1 << rng.below(8); },
        6 => { let at = rng.below(data.len()); let end = (at + rng.below(1200)).min(data.len()); let copy = data[at..end].to_vec(); data.splice(at..at, copy); }
        7 => { let at = rng.below(data.len()); let end = (at + rng.below(600)).min(data.len()); let fill = if rng.below(2) == 0 { 0xff } else { 0x00 }; data[at..end].fill(fill); }
        8 => { // splice a range of another stream in (another header, rate or channel layout in the middle)
            let from = rng.below(other.len().max(1)); let to = (from + rng.below(3000)).min(other.len());
            let at = rng.below(data.len()); data.splice(at..at, other[from..to].iter().copied());
        }
        9 => { // leading ID3v2 header(s): random version/flags/size, chained, sometimes 0xff version bytes
            let mut prefix = Vec::new();
            for _ in 0..1 + rng.below(3) {
                let size = [0usize, 1, 10, 100, rng.below(5000)][rng.below(5)];
                prefix.extend_from_slice(b"ID3");
                prefix.extend_from_slice(&[[2, 3, 4, 0xff, rng.byte()][rng.below(5)], [0, 0, 0xff, rng.byte()][rng.below(4)], [0, 0x10, 0x40, 0x80, rng.byte()][rng.below(5)]]);
                prefix.extend_from_slice(&[(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
                prefix.extend((0..size.min(600)).map(|_| rng.byte()));
            }
            data.splice(0..0, prefix);
        }
        10 => { // swap two ranges
            let a = rng.below(data.len()); let b = rng.below(data.len());
            let len = rng.below(800).min(data.len() - a.max(b));
            for k in 0..len { data.swap(a + k, b + k); }
        }
        11 => { // a box size field becomes something else
            let starts = box_starts(&data);
            if let Some(&at) = starts.get(rng.below(starts.len().max(1))) {
                let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap_or([0; 4]));
                let value = [0, 1, 7, 8, 0xffff_ffff, size.wrapping_add(1), size.wrapping_sub(1), size / 2, rng.next() as u32, 0x7fff_ffff][rng.below(10)];
                put_u32(&mut data, at, value);
            }
        }
        12 => { // insert a box of another type, or cut one out
            let starts = box_starts(&data);
            if !starts.is_empty() {
                let at = starts[rng.below(starts.len())];
                if rng.below(2) == 0 {
                    let kind: &[u8; 4] = [b"free", b"wide", b"skip", b"uuid", b"mdat", b"moov", b"trak"][rng.below(7)];
                    let size = rng.below(200);
                    let mut boxed = ((size + 8) as u32).to_be_bytes().to_vec();
                    boxed.extend_from_slice(kind);
                    boxed.extend((0..size).map(|_| rng.byte()));
                    data.splice(at..at, boxed);
                } else {
                    let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap_or([0; 4])) as usize;
                    let end = (at + size.max(8)).min(data.len());
                    data.drain(at..end);
                }
            }
        }
        13 => { // ADTS frame headers: flip bits of the profile, rate, channel or length fields
            let syncs = adts_syncs(&data);
            for _ in 0..1 + rng.below(4) {
                if syncs.is_empty() { break; }
                let at = syncs[rng.below(syncs.len())];
                data[at + 1 + rng.below(5)] ^= 1 << rng.below(8);
            }
        }
        14 => { // absurd counts in the sample tables and chunk sizes
            let kinds = COUNT_BOXES.iter().flat_map(|t| find_all(&data, *t)).collect::<Vec<_>>();
            if let Some(&at) = kinds.get(rng.below(kinds.len().max(1))) {
                let value = [0, 1, 0x7fff_ffff, 0xffff_ffff, 0x0100_0000, rng.next() as u32][rng.below(6)];
                put_u32(&mut data, at + 8 + 4 * rng.below(2), value);
            } else if data.starts_with(b"caff") {
                let at = 8 + 4 + rng.below(8);
                if at < data.len() { data[at] = rng.byte(); }
            }
        }
        15 => { // noise over access-unit sized ranges
            for _ in 0..1 + rng.below(5) {
                let at = rng.below(data.len()); let end = (at + 40 + rng.below(500)).min(data.len());
                let kind = rng.below(3);
                for b in &mut data[at..end] { *b = match kind { 0 => rng.byte(), 1 => 0xff, _ => 0 }; }
            }
        }
        16 => { // forge the first bytes: any container's header followed by what was there
            let head: Vec<u8> = match rng.below(4) {
                0 => { let mut h = vec![0, 0, 0, 0x1c]; h.extend_from_slice(b"ftyp"); h.extend_from_slice([b"M4A ", b"mp42", b"isom", b"3gp4", b"qt  "][rng.below(5)]); h.extend((0..16).map(|_| rng.byte())); h }
                1 => vec![0xff, 0xf0 | (rng.byte() & 0x0f), rng.byte(), rng.byte(), rng.byte(), rng.byte(), rng.byte()],
                2 => { let mut h = b"caff\x00\x01\x00\x00desc".to_vec(); h.extend((0..40).map(|_| rng.byte())); h }
                _ => vec![0x56, 0xe0 | (rng.byte() & 0x1f), rng.byte(), rng.byte(), rng.byte()],
            };
            let keep = rng.below(2) == 0;
            if keep { data.splice(0..0, head); } else { let n = head.len().min(data.len()); data[..n].copy_from_slice(&head[..n]); }
        }
        17 => { // truncate exactly at a box boundary, +-1
            let starts = box_starts(&data);
            if !starts.is_empty() {
                let at = starts[rng.below(starts.len())];
                data.truncate((at + [0usize, 1, 4, 7, 8, 12][rng.below(6)]).min(data.len()));
            } else { data.truncate(rng.below(data.len())); }
        }
        18 => { // CAF / LOAS: damage the fixed header region
            for _ in 0..1 + rng.below(4) { let i = rng.below(data.len().min(80)); data[i] = rng.byte(); }
        }
        _ => { // forge the sampling-frequency / channel fields of an AudioSpecificConfig-like byte pair or of an ADTS header
            let at = find_all(&data, b"esds").first().copied().map(|i| i + 15 + rng.below(12)).or_else(|| adts_syncs(&data).first().map(|&i| i + 2));
            if let Some(at) = at { if at + 2 <= data.len() { data[at] = rng.byte(); data[at + 1] = rng.byte(); } }
        }
    }
    if data.len() > MAX_BYTES { data.truncate(MAX_BYTES); }
    data
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Refused(String),
    Decoded { frames: u64, checksum: u64, error: Option<String> },
}

fn fold(checksum: u64, sample: u32) -> u64 { checksum.wrapping_mul(0x100_0000_01b3) ^ u64::from(sample) }

/// How a refusal or failure is worded: a typed refusal of the port, a `Native` status Apple produced for such files, or a defect.
fn classify(error: &CodecError, allowed: &BTreeSet<i32>, operations: &[&str]) -> Result<String, String> {
    match error {
        CodecError::Native { operation, status } => {
            if !operations.contains(operation) { return Err(format!("native failure of the unexpected call {operation}")); }
            if !allowed.contains(status) { return Err(format!("{operation} failed with {status} ({:#010x}), a status Apple did not produce for any fixture", *status as u32)); }
            Ok(format!("{operation}/{status}"))
        }
        CodecError::UnsupportedCodec { codec, operation: "decoding", .. } if *codec == "AAC" || codec.starts_with("AAC/") => Ok("unsupported-codec".into()),
        CodecError::UnsupportedFormat(_) => Ok("unsupported-format".into()),
        CodecError::InvalidInput(_) => Ok("invalid-input".into()),
        CodecError::UnsupportedSampleRateConversion { .. } => Ok("unsupported-rate-conversion".into()),
        other => Err(format!("unexpected error {other:?}")),
    }
}

static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// A file of its own for every case: a case that never returns keeps its file, and must not disturb the next one.
fn write_case(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let slot = dir.join(format!("{:06}", NEXT_SLOT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&slot).expect("create case directory");
    let path = slot.join(name);
    std::fs::write(&path, bytes).expect("write case file");
    path
}

/// How one case is run: the read shape, the read size, the client rate the reader is opened with, the denormal mode, and the time it may take.
#[derive(Clone, Copy)]
struct Mode { interleaved: bool, block: usize, rate: Option<f64>, ftz: bool, limit: Duration }
impl Mode {
    const fn new() -> Mode { Mode { interleaved: false, block: BLOCK, rate: None, ftz: false, limit: CASE_LIMIT } }
}

fn run_case(dir: &Path, allowed: &BTreeSet<i32>, tag: &str, name: &str, bytes: &[u8], mode: Mode) -> (Outcome, CaseStats) {
    let start = Instant::now();
    if mode.ftz { flush_denormals_to_zero(); }
    let path = write_case(dir, name, bytes);
    reset_allocation_counters();
    let cpu_start = thread_cpu();
    let outcome = decode_case(allowed, tag, bytes, &path, mode, start);
    let cpu = thread_cpu() - cpu_start;
    let (largest, live) = allocation_counters();
    MAX_LARGEST.fetch_max(largest, Ordering::Relaxed);
    MAX_LIVE.fetch_max(live, Ordering::Relaxed);
    MAX_FIXED_LARGEST.fetch_max(largest.saturating_sub(LARGEST_PER_BYTE * bytes.len()), Ordering::Relaxed);
    MAX_FIXED_LIVE.fetch_max(live.saturating_sub(LIVE_PER_BYTE * bytes.len()), Ordering::Relaxed);
    assert!(largest <= LARGEST_PER_BYTE * bytes.len() + FIXED_LARGEST, "{tag}: one allocation of {largest} bytes for an input of {} bytes", bytes.len());
    assert!(live <= LIVE_PER_BYTE * bytes.len() + FIXED_LIVE, "{tag}: {live} bytes live at once for an input of {} bytes", bytes.len());
    assert!(start.elapsed() < mode.limit, "{tag}: took {:?} for {} bytes", start.elapsed(), bytes.len());
    (outcome, CaseStats { cpu, largest, live })
}

fn decode_case(allowed: &BTreeSet<i32>, tag: &str, bytes: &[u8], path: &Path, mode: Mode, start: Instant) -> Outcome {
    let mut reader = match Reader::open(path, mode.rate) {
        Err(error) => match classify(&error, allowed, &[OPEN, CLIENT]) {
            Ok(kind) => return Outcome::Refused(kind),
            Err(defect) => panic!("{tag}: open: {defect}"),
        },
        Ok(reader) => reader,
    };
    // the properties of an opened reader are answers too: a value, or a failure of the kinds the stream can end in
    match reader.file_length_frames() {
        Ok(length) => assert!(length >= 0, "{tag}: a file length of {length} frames"),
        Err(e) => assert!(classify(&e, allowed, &[OPEN, CLIENT, READ, READ_PLANAR, BIT_RATE]).is_ok(), "{tag}: file length: {e:?}"),
    }
    if let Err(e) = reader.bitrate() { assert!(classify(&e, allowed, &[OPEN, CLIENT, READ, READ_PLANAR, BIT_RATE]).is_ok(), "{tag}: bit rate: {e:?}"); }
    let source = reader.source_format();
    assert!(source.channels_per_frame >= 1 && source.sample_rate > 0.0 && source.sample_rate.is_finite(), "{tag}: an opened reader describes {source:?}");
    let rate = source.sample_rate;
    // Frames cannot come from nowhere: one access unit takes at least a byte of data and a byte of table, and carries at most 1024 frames (x4 blocks per ADTS frame).
    let limit = bytes.len() as u64 * 600 + 16_384;
    let mono = source.channels_per_frame == 1;
    let (mut frames, mut checksum, mut error) = (0u64, 0u64, None);
    if !mode.interleaved {
        match reader.set_stereo_planar(rate) {
            Err(e) => match classify(&e, allowed, &[CLIENT]) { Ok(kind) => error = Some(kind), Err(defect) => panic!("{tag}: planar client format: {defect}") },
            Ok(()) => {
                let (mut left, mut right) = (vec![0f32; mode.block], vec![0f32; mode.block]);
                loop {
                    match reader.read_stereo_planar(&mut left, &mut right) {
                        Ok(0) => break,
                        Ok(n) => {
                            assert!(n <= mode.block, "{tag}: a read of {} frames returned {n}", mode.block);
                            for i in 0..n {
                                assert!(!mono || left[i].to_bits() == right[i].to_bits(), "{tag}: mono must be duplicated into both planes");
                                checksum = fold(fold(checksum, left[i].to_bits()), right[i].to_bits());
                            }
                            frames += n as u64;
                            assert!(frames <= limit, "{tag}: {frames} frames from {} bytes", bytes.len());
                            assert!(start.elapsed() < mode.limit, "{tag}: still decoding after {:?} ({frames} frames from {} bytes)", start.elapsed(), bytes.len());
                        }
                        Err(e) => match classify(&e, allowed, &[READ_PLANAR]) {
                            Ok(kind) => { error = Some(kind); break; }
                            Err(defect) => panic!("{tag}: read: {defect}"),
                        },
                    }
                }
                if error.is_some() {
                    // the reader is poisoned at the first failure, whatever the damage
                    assert!(matches!(reader.read_stereo_planar(&mut left, &mut right), Err(CodecError::Poisoned)), "{tag}: a failed reader must stay poisoned");
                }
            }
        }
    } else {
        let channels = source.channels_per_frame as usize;
        let mut buffer = vec![0f32; mode.block * channels];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    assert!(n <= mode.block, "{tag}: a read of {} frames returned {n}", mode.block);
                    for sample in &buffer[..n * channels] { checksum = fold(checksum, sample.to_bits()); }
                    frames += n as u64;
                    assert!(frames <= limit, "{tag}: interleaved reads delivered {frames} frames from {} bytes", bytes.len());
                    assert!(start.elapsed() < mode.limit, "{tag}: still decoding after {:?}", start.elapsed());
                }
                Err(e) => match classify(&e, allowed, &[READ]) {
                    Ok(kind) => { error = Some(kind); break; }
                    Err(defect) => panic!("{tag}: interleaved read: {defect}"),
                },
            }
        }
        if error.is_some() {
            assert!(matches!(reader.read(&mut buffer), Err(CodecError::Poisoned)), "{tag}: a failed reader must stay poisoned");
        }
    }
    Outcome::Decoded { frames, checksum, error }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload.downcast_ref::<&str>().map(|s| (*s).to_owned()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "a panic".to_owned())
}

fn keep_input(tag: &str, name: &str, bytes: &[u8]) -> PathBuf {
    let kept = std::env::temp_dir().join(format!("platinum-aac-hostile-{}.{}", tag.replace(' ', "-"), name.rsplit('.').next().unwrap_or("bin")));
    let _ = std::fs::write(&kept, bytes);
    kept
}

/// Run a case on a thread of its own under a watchdog: a reader that hangs (in `open` as much as in a read) is reported with its case
/// and input instead of stalling the test for ever, and a panic is named and its input kept.
fn guarded(scratch: &Scratch, allowed: &Arc<BTreeSet<i32>>, tag: &str, name: &str, bytes: &[u8], mode: Mode) -> Outcome {
    guarded_stats(scratch, allowed, tag, name, bytes, mode).0
}

/// [`guarded`], with what the case cost.
fn guarded_stats(scratch: &Scratch, allowed: &Arc<BTreeSet<i32>>, tag: &str, name: &str, bytes: &[u8], mode: Mode) -> (Outcome, CaseStats) {
    let (sender, receiver) = channel();
    let (dir, allowed_for_case, tag_owned, name_owned, bytes_owned) = (scratch.0.clone(), Arc::clone(allowed), tag.to_owned(), name.to_owned(), bytes.to_vec());
    std::thread::Builder::new().name(tag.to_owned()).stack_size(16 << 20).spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| run_case(&dir, &allowed_for_case, &tag_owned, &name_owned, &bytes_owned, mode)));
        let _ = sender.send(result.map_err(panic_message));
    }).expect("spawn the case thread");
    match receiver.recv_timeout(mode.limit + Duration::from_secs(5)) {
        Ok(Ok(result)) => result,
        Ok(Err(message)) => panic!("{tag} ({name}): {message}\nthe input was kept at {}", keep_input(tag, name, bytes).display()),
        Err(RecvTimeoutError::Timeout) => panic!("{tag} ({name}): no answer after {:?}, the reader hangs\nthe input was kept at {}", mode.limit, keep_input(tag, name, bytes).display()),
        Err(RecvTimeoutError::Disconnected) => panic!("{tag} ({name}): the case thread died without an answer"),
    }
}

/// Sets flush-to-zero for the calling thread: the decoder's output must not depend on how the host treats denormals.
#[cfg(target_arch = "aarch64")]
fn flush_denormals_to_zero() {
    // SAFETY: reads and writes the floating-point control register of this thread; FZ (bit 24) only changes how denormals are treated.
    unsafe {
        let mut control: u64;
        std::arch::asm!("mrs {0}, fpcr", out(reg) control, options(nomem, nostack));
        std::arch::asm!("msr fpcr, {0}", in(reg) control | (1 << 24), options(nomem, nostack));
    }
}
#[cfg(not(target_arch = "aarch64"))]
fn flush_denormals_to_zero() {
    // SAFETY: sets the FTZ and DAZ bits of this thread's MXCSR, which only change how denormals are treated.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        #[allow(deprecated)]
        std::arch::x86_64::_mm_setcsr(std::arch::x86_64::_mm_getcsr() | 0x8040);
    }
}

/// The statuses AudioToolbox answered for generated damaged inputs of the kinds below (written to files and recorded on macOS 27.2): 'wht?', 'dta?',
/// 'typ?', 'fmt?', 'sync', 'chk?', 'layr', 'rate', 'prof', '!dat', '!siz', 'pck?', 'bada', paramErr, and a few errno-like and codec errors.  Apple's
/// MPEG reader hangs on an MP4 file named `.mpa`; the port must refuse it.
const APPLE_HOSTILE_STATUSES: [i32; 19] = [-66567, -12848, -12847, -50, -2, -1, 560226676, 561211770, 1650549857, 1667787583, 1685348671, 1718449215,
    1818327410, 1885563711, 1886547814, 1918989413, 1937337955, 1954115647, 2003334207];

/// Every status Apple answered for a fixture (opening, reading, and the raw handle after a failure) or for the hostile cases.
fn statuses_apple_produced(fixtures: &Fixtures) -> Arc<BTreeSet<i32>> {
    let mut allowed: BTreeSet<i32> = APPLE_HOSTILE_STATUSES.into_iter().collect();
    for f in &fixtures.files {
        let streams = [f.planar512.as_ref(), f.inter512.as_ref()];
        for e in f.open_error.iter().chain(streams.iter().flatten().filter_map(|s| s.error.as_ref())) { allowed.insert(e.status); }
        for v in &f.variants { for e in v.open_error.iter() { allowed.insert(e.status); } }
        if let Some(a) = &f.after_failure { allowed.insert(a.first_status); allowed.extend(a.later.iter().map(|(s, _)| *s).filter(|s| *s != 0)); }
    }
    allowed.remove(&0);
    Arc::new(allowed)
}

fn extension_for(rng: &mut Rng, original: &str) -> String {
    // mostly the name the file came with, sometimes another: the name decides which reader looks first
    const OTHERS: [&str; 10] = ["m4a", "aac", "caf", "loas", "mp4", "mp3", "wav", "bin", "", "m4b"];
    if rng.below(4) != 0 { return original.to_owned(); }
    let ext = OTHERS[rng.below(OTHERS.len())];
    if ext.is_empty() { "case".to_owned() } else { format!("case.{ext}") }
}

/// The same stream, however it is asked for: refusals alike, streams that end without failure alike in frames and samples; a failure may lose a
/// different number of frames when the reads are cut differently (the frames of the failing call are lost), so failures are compared by kind only,
/// and only against another failure.
fn same_stream(a: &Outcome, b: &Outcome) -> bool {
    match (a, b) {
        (Outcome::Refused(x), Outcome::Refused(y)) => x == y,
        (Outcome::Decoded { frames: f1, checksum: c1, error: None }, Outcome::Decoded { frames: f2, checksum: c2, error: None }) => f1 == f2 && c1 == c2,
        (Outcome::Decoded { error: Some(e1), .. }, Outcome::Decoded { error: Some(e2), .. }) => e1 == e2,
        _ => false,
    }
}

/// Fragmented MP4 as a mutation source: the fixture without `mvex` (opens as an empty stream) gets a `mvex` with a `trex` at the end of its `moov`, so
/// that the mutations reach the `moof`/`traf`/`trun` parsing.
fn with_mvex(original: &[u8]) -> Vec<u8> {
    let moov = find_all(original, b"moov")[0] - 4;
    let size = u32::from_be_bytes(original[moov..moov + 4].try_into().unwrap()) as usize;
    // trex: version and flags, track 1, description 1, duration 1024, size 0, flags 0
    let mvex = atom(b"mvex", &atom(b"trex", &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    let mut file = original[..moov + size].to_vec();
    file.extend_from_slice(&mvex);
    file.extend_from_slice(&original[moov + size..]);
    put_u32(&mut file, moov, (size + mvex.len()) as u32);
    file
}

#[test]
fn six_thousand_mutated_inputs_never_panic_hang_or_misreport() {
    let fixtures = Fixtures::load();
    let allowed = statuses_apple_produced(fixtures);
    assert!(allowed.len() >= 6, "{allowed:?}");
    // Streams Apple decodes (container and codec families alike), small enough that a mutated copy keeps its headers; and the fragmented file with a
    // `mvex` added.
    let fragmented = fixtures.files.iter().find(|f| f.id == "probe/fmp4_no_mvex").map(|f| with_mvex(fixtures.bytes(f))).expect("the fragmented fixture");
    let mut sources: Vec<(&str, &[u8])> = fixtures.files.iter()
        .filter(|f| f.expect == Expect::Exact && f.length >= 120 && f.length <= 12_000)
        .map(|f| (f.name.as_str(), fixtures.bytes(f))).collect();
    sources.push(("fragmented.m4a", &fragmented));
    assert!(sources.len() >= 100, "{} small AAC fixtures to mutate", sources.len());
    let extensions: BTreeSet<&str> = sources.iter().map(|(n, _)| n.rsplit('.').next().unwrap_or("")).collect();
    for needed in ["m4a", "aac", "caf"] { assert!(extensions.contains(needed), "no {needed} source"); }

    // the client rate the reader is opened with: none (the source rate), or a rate the converter must refuse or convert
    const RATES: [Option<f64>; 16] = [None, None, None, None, None, None, None, None, None, None, None, None, Some(44_100.0), Some(48_000.0), Some(8_000.0), Some(96_000.0)];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut refused, mut decoded, mut failed, mut nonempty, mut repeated, mut recut) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut kinds = BTreeSet::new();
    let started = Instant::now();
    let scratch = Scratch::new("hostile");
    for case in 0..ITERATIONS {
        let (name, original) = sources[rng.below(sources.len())];
        let (_, other) = sources[rng.below(sources.len())];
        let data = mutate(&mut rng, original, other, case);
        let replay_name = extension_for(&mut rng, name);
        let tag = format!("case {case}");
        let mode = Mode { interleaved: case % 4 == 0, rate: RATES[(case / 3) % RATES.len()], ..Mode::new() };
        let first = guarded(&scratch, &allowed, &tag, &replay_name, &data, mode);
        match &first {
            Outcome::Refused(kind) => { refused += 1; kinds.insert(kind.clone()); }
            Outcome::Decoded { frames, error, .. } => {
                decoded += 1;
                failed += usize::from(error.is_some());
                nonempty += usize::from(*frames > 0);
                if let Some(kind) = error { kinds.insert(kind.clone()); }
            }
        }
        if case % REPEAT_EVERY == 0 {
            repeated += 1;
            assert_eq!(first, guarded(&scratch, &allowed, &tag, &replay_name, &data, mode), "{tag} ({replay_name}): decoding is not deterministic");
        }
        if case % REPEAT_EVERY == REPEAT_EVERY / 2 {
            // another read size, and denormals flushed to zero: the stream does not depend on either
            recut += 1;
            let other_mode = Mode { block: 333, ftz: true, ..mode };
            let second = guarded(&scratch, &allowed, &tag, &replay_name, &data, other_mode);
            assert!(same_stream(&first, &second), "{tag} ({replay_name}): {first:?} read in blocks of {BLOCK}, {second:?} in blocks of 333 with flush-to-zero");
        }
    }
    let (largest, live) = (MAX_LARGEST.load(Ordering::Relaxed), MAX_LIVE.load(Ordering::Relaxed));
    println!("{ITERATIONS} cases in {:.1?}: {refused} refused at open, {decoded} opened ({nonempty} with audio, {failed} ending in a failure), {} kinds of outcome {kinds:?}; {repeated} repeated, {recut} re-read in other blocks; largest allocation {largest} bytes, most live {live} bytes; beyond the per-byte limits at most {} and {} bytes",
        started.elapsed(), kinds.len(), MAX_FIXED_LARGEST.load(Ordering::Relaxed), MAX_FIXED_LIVE.load(Ordering::Relaxed));
    assert!(refused > 600 && decoded > 600 && failed > 100 && nonempty > 400, "refused {refused}, opened {decoded}, failed {failed}, with audio {nonempty}");
    assert!(kinds.len() >= 5, "only {kinds:?}: the mutations do not reach enough of the reader");
}

#[test]
fn degenerate_inputs_are_refused_cleanly() {
    let fixtures = Fixtures::load();
    let allowed = statuses_apple_produced(fixtures);
    let scratch = Scratch::new("degenerate");
    let empty: &[u8] = &[];
    let zeros = [0u8; 5000];
    let ones = [0xffu8; 5000];
    for (name, bytes) in [("empty", empty), ("one", &[0xff][..]), ("adts-sync-only", &[0xff, 0xf1]), ("zeros", &zeros[..]), ("ones", &ones[..]), ("mp4-header-only", b"\0\0\0\x08ftyp"),
        ("caf-header-only", b"caff\0\x01\0\0"), ("loas-sync-only", &[0x56, 0xe0])] {
        for extension in ["m4a", "aac", "caf", "loas", "mp3", "bin"] {
            let outcome = guarded(&scratch, &allowed, name, &format!("{name}.{extension}"), bytes, Mode { interleaved: true, ..Mode::new() });
            assert!(matches!(outcome, Outcome::Refused(_)), "{name}.{extension}: {outcome:?}");
        }
    }
    // a claimed gigantic ID3v2 tag with nothing behind it
    let mut id3 = b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f".to_vec();
    id3.extend([0u8; 64]);
    for extension in ["m4a", "aac", "mp3"] {
        let outcome = guarded(&scratch, &allowed, "id3-huge", &format!("t.{extension}"), &id3, Mode { interleaved: true, ..Mode::new() });
        assert!(matches!(outcome, Outcome::Refused(_)), "id3-huge.{extension}: {outcome:?}");
    }
    // Apple hangs on a chained ID3 header whose version byte is 0xff; the port must refuse it, not hang
    let mut chain = b"ID3\x04\x00\x00\x00\x00\x00\x14".to_vec(); // a 20-byte tag, then a second header
    chain.extend([0u8; 20]);
    chain.extend_from_slice(b"ID3\xff\x00\x00\x00\x00\x00\x00");
    chain.extend_from_slice(&[0xff, 0xf1, 0x50, 0x80, 0x01, 0x1f, 0xfc]);
    chain.resize(1000, 0);
    for extension in ["aac", "m4a", "bin"] {
        let outcome = guarded(&scratch, &allowed, "id3-chain-ff", &format!("t.{extension}"), &chain, Mode { interleaved: true, ..Mode::new() });
        assert!(matches!(outcome, Outcome::Refused(_)), "id3-chain-ff.{extension}: {outcome:?}");
    }
    // a FIL element with the escape count 0 in the first access unit of an ADTS stream (once an arithmetic underflow): refused, whatever the name
    for extension in ["aac", "adts", "bin"] {
        let outcome = guarded(&scratch, &allowed, "fil-escape-0", &format!("t.{extension}"), &[0xff, 0xf1, 0x50, 0x80, 0x01, 0x3f, 0xfc, 0xde, 0x00], Mode::new());
        assert!(matches!(outcome, Outcome::Refused(_)), "fil-escape-0.{extension}: {outcome:?}");
    }

    // A box that claims gigabytes in front of real data delivers only what exists: grow the size fields of real inputs to the maximum.
    let (mut checked, mut grown) = (0usize, 0usize);
    // (box type, offset of the field from the type's first byte): a box size, a constant sample size, a sample count, a chunk count
    let fields: [(&[u8], isize); 8] = [(b"moov", -4), (b"mdat", -4), (b"trak", -4), (b"stbl", -4), (b"stsz", 8), (b"stsz", 12), (b"stco", 8), (b"stts", 8)];
    for fixture in fixtures.files.iter().filter(|f| f.expect == Expect::Exact && f.has("container:mp4") && f.length < 12_000).take(40) {
        let original = fixtures.bytes(fixture);
        for (needle, offset) in fields {
            for at in find_all(original, needle).into_iter().filter(|&i| i >= 4).take(1) {
                let mut file = original.to_vec();
                put_u32(&mut file, (at as isize + offset) as usize, u32::MAX);
                let limit = file.len() as u64 * 600 + 16_384;
                match guarded(&scratch, &allowed, "giant", &fixture.name, &file, Mode::new()) {
                    Outcome::Decoded { frames, .. } => { assert!(frames <= limit, "{}: {frames} frames from {} bytes", fixture.id, file.len()); grown += 1; }
                    Outcome::Refused(_) => {}
                }
                checked += 1;
            }
        }
    }
    assert_eq!((checked, grown), (CHECKED_GIANT_FIELDS, DECODED_GIANT_FIELDS), "grown size fields, and those that still decode");
}

// ---- inputs of megabytes ----------------------------------------------------------------------------------------------------------

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], version_flags: u32, body: &[u8]) -> Vec<u8> { atom(kind, &[&version_flags.to_be_bytes()[..], body].concat()) }

fn be(value: u32) -> [u8; 4] { value.to_be_bytes() }

fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> { [&[tag, body.len() as u8][..], body].concat() }

/// The `esds` atom of an AAC-LC stereo 44.1 kHz track.
fn esds() -> Vec<u8> {
    let decoder = [&[0x40, 0x15, 0, 0, 0][..], &[0; 8], &descriptor(5, &[0x12, 0x10])].concat();
    let es = [&[0, 1, 0][..], &descriptor(4, &decoder), &descriptor(6, &[2])].concat();
    full(b"esds", 0, &descriptor(3, &es))
}

/// A one-track MP4 (stereo, 44.1 kHz, AAC-LC) of which a case varies the parts that grow: the atoms in front of the track, the edit list, the sample
/// tables, the movie fragments.
struct Mp4 {
    ftyp: Vec<u8>,
    /// top-level atoms between the `ftyp` and the `moov`
    before_moov: Vec<u8>,
    before_trak: Vec<u8>,
    /// children of the `moov` behind the track and the `mvex`
    moov_tail: Vec<u8>,
    edts: Vec<u8>,
    stsz: Vec<u8>,
    stsc: Vec<u8>,
    chunks: usize,
    mvex: Vec<u8>,
    /// `moof` atoms, each followed by the `mdat` they point into
    fragments: Vec<u8>,
    mdat: usize,
}

impl Mp4 {
    fn new() -> Mp4 {
        Mp4 {
            ftyp: atom(b"ftyp", b"M4A \0\0\0\0M4A mp42isom"),
            before_moov: Vec::new(),
            before_trak: Vec::new(),
            moov_tail: Vec::new(),
            edts: Vec::new(),
            stsz: full(b"stsz", 0, &[be(1), be(1000)].concat()),
            stsc: full(b"stsc", 0, &[be(1), be(1), be(1000), be(1)].concat()),
            chunks: 1,
            mvex: Vec::new(),
            fragments: Vec::new(),
            mdat: 1000,
        }
    }

    fn build(&self) -> Vec<u8> {
        let moov = |mdat_at: u32| {
            let entry = [&[0u8; 6][..], &1u16.to_be_bytes(), &[0; 8], &2u16.to_be_bytes(), &16u16.to_be_bytes(), &[0; 4], &(44_100u32 << 16).to_be_bytes(), &esds()].concat();
            let stsd = full(b"stsd", 0, &[&be(1)[..], &atom(b"mp4a", &entry)].concat());
            let mut stco = be(self.chunks as u32).to_vec();
            for i in 0..self.chunks {
                stco.extend_from_slice(&be(mdat_at + (i % 64) as u32));
            }
            let stbl = atom(b"stbl", &[stsd, self.stsz.clone(), self.stsc.clone(), full(b"stco", 0, &stco)].concat());
            let mdhd = full(b"mdhd", 0, &[be(0), be(0), be(44_100), be(0), be(0x55c4_0000)].concat());
            let hdlr = full(b"hdlr", 0, &[&[0u8; 4][..], b"soun", &[0u8; 13]].concat());
            let mdia = atom(b"mdia", &[mdhd, hdlr, atom(b"minf", &stbl)].concat());
            atom(b"moov", &[self.before_trak.clone(), atom(b"trak", &[self.edts.clone(), mdia].concat()), self.mvex.clone(), self.moov_tail.clone()].concat())
        };
        let mdat_at = (self.ftyp.len() + self.before_moov.len() + moov(0).len() + 8) as u32;
        let mut out = self.ftyp.clone();
        out.extend_from_slice(&self.before_moov);
        out.extend(moov(mdat_at));
        out.extend(atom(b"mdat", &vec![0u8; self.mdat]));
        out.extend_from_slice(&self.fragments);
        out
    }
}

/// An edit list of `count` entries (a segment of 1000 units from media time 0 at rate 1, over and over).
fn elst(count: usize) -> Vec<u8> {
    let mut body = be(count as u32).to_vec();
    for _ in 0..count {
        body.extend_from_slice(&[be(1000), be(0), be(1 << 16)].concat());
    }
    atom(b"edts", &full(b"elst", 0, &body))
}

/// A fragment (`moof` + `mdat`) of one `trun` of `count` one-byte samples, sizes in the table.
fn one_byte_fragment(count: usize) -> Vec<u8> {
    let make = |data_offset: u32| {
        let mut trun = [be(count as u32), be(data_offset)].concat();
        for _ in 0..count {
            trun.extend_from_slice(&be(1));
        }
        atom(b"moof", &[full(b"mfhd", 0, &be(1)), atom(b"traf", &[full(b"tfhd", 0x02_0000, &be(0)), full(b"trun", 0x201, &trun)].concat())].concat())
    };
    let moof = make(0).len() as u32;
    let mut out = make(moof + 8);
    out.extend(atom(b"mdat", &vec![0u8; count]));
    out
}

/// Inputs whose tables, scanned the naive way, cost the product of two sizes (or more), with sizes that make the product take minutes.  Every one
/// must open and read in time proportional to its size, and under the allocation limits of `run_case`.
#[test]
fn scaled_inputs_cost_in_proportion() {
    const LOAS: &[u8] = include_bytes!("fixtures/aac-reader/tiny-44100-2.loas");
    let fixtures = Fixtures::load();
    let allowed = statuses_apple_produced(fixtures);
    let scratch = Scratch::new("scaled");
    let mode = Mode { limit: Duration::from_secs(if cfg!(debug_assertions) { 60 } else { 10 }), ..Mode::new() };
    let run = |name: &str, extension: &str, file: &[u8]| {
        let started = Instant::now();
        let outcome = guarded(&scratch, &allowed, name, &format!("{name}.{extension}"), file, mode);
        println!("{name}: {} bytes, {:.2?}: {outcome:?}", file.len(), started.elapsed());
        outcome
    };
    let opened = |outcome: &Outcome| matches!(outcome, Outcome::Decoded { .. });
    let refused_with = |outcome: &Outcome, status: &str| matches!(outcome, Outcome::Refused(kind) if kind.ends_with(&format!("/{status}")));

    // 4 MiB of tracks that have no media atom of their own (its size runs past them) in front of the real track
    let mut decoys = Mp4::new();
    decoys.before_trak = [&be(16)[..], b"trak", &be(0x00ff_ffff), b"mdia"].concat().repeat(262_144);
    assert!(opened(&run("decoy-tracks", "m4a", &decoys.build())));

    // 200 000 sample-to-chunk runs over 400 000 chunks of a million samples
    let mut chunky = Mp4::new();
    chunky.stsz = full(b"stsz", 0, &[be(1), be(1_000_000)].concat());
    chunky.stsc = full(b"stsc", 0, &[be(200_000).to_vec(), (0..200_000u32).flat_map(|i| [be(2 * i + 1), be(1), be(1)].concat()).collect()].concat());
    chunky.chunks = 400_000;
    chunky.mdat = 1_000_000;
    assert!(opened(&run("stsc-times-stco", "m4a", &chunky.build())));

    // an 8 MiB `ftyp`: not an MP4 file ('typ?'), refused without reading the brands one by one
    let mut brands = Mp4::new();
    brands.ftyp = atom(b"ftyp", &[b"M4A \0\0\0\0".to_vec(), vec![0u8; 8 << 20]].concat());
    assert!(refused_with(&run("ftyp-8mib", "m4a", &brands.build()), "1954115647"));

    // an edit list of a million entries
    let mut edits = Mp4::new();
    edits.edts = elst(1_000_000);
    assert!(opened(&run("elst-1m", "m4a", &edits.build())));

    // a fragment of a million samples, and a `trun` that claims many more than its atom holds
    let mut fragments = Mp4::new();
    fragments.stsz = full(b"stsz", 0, &[be(0), be(0)].concat());
    fragments.stsc = full(b"stsc", 0, &[be(1), be(1), be(1), be(1)].concat());
    fragments.mdat = 0;
    fragments.mvex = atom(b"mvex", &full(b"trex", 0, &[be(0), be(1), be(1024), be(0), be(0)].concat()));
    fragments.fragments = one_byte_fragment(1_000_000);
    assert!(opened(&run("trun-1m", "m4a", &fragments.build())));
    let mut claims = one_byte_fragment(10);
    let at = find_all(&claims, b"trun")[0] + 4 + 4;
    put_u32(&mut claims, at, 100_000_000);
    fragments.fragments = claims;
    assert!(opened(&run("trun-claims-100m", "m4a", &fragments.build())));

    // 8 MiB of LOAS frames (1 MiB in an unoptimised build, which decodes it at a tenth of the speed), and the 19-byte file that claims a configuration of 2^32 - 1 bits
    let big = LOAS.repeat(if cfg!(debug_assertions) { 1 << 20 } else { 8 << 20 } / LOAS.len() + 1);
    assert!(opened(&run("loas-8mib", "loas", &big)));
    let forged = [0x56, 0xe0, 0x10, 0x40, 0x00, 0x00, 0x1f, 0xff, 0xff, 0xff, 0xff, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    // (the name `.aac` sends the file to the ADTS reader, which finds no ADTS sync)
    for extension in ["loas", "latm"] {
        assert!(refused_with(&run("loas-forged", extension, &forged), "1718449215"), "loas-forged.{extension}");
    }
}

// ---- cost in proportion to size, structure by structure ------------------------------------------------------------------------------

/// Something a reader has to walk through, in `k` units of it (an atom, a table entry, a chunk or a tag).
struct Family { name: &'static str, extension: &'static str, small: usize, build: fn(usize) -> Vec<u8> }

fn decoy_tracks(k: usize) -> Vec<u8> {
    let mut mp4 = Mp4::new();
    mp4.before_trak = [&be(16)[..], b"trak", &be(0x00ff_ffff), b"mdia"].concat().repeat(k);
    mp4.build()
}

/// `udta` atoms whose first child is a `meta` with a size that runs to the end of the movie (every one of them looks at the others).
fn udta_chain(k: usize) -> Vec<u8> {
    let mut mp4 = Mp4::new();
    mp4.moov_tail = [&be(20)[..], b"udta", &be(0x7fff_ffff), b"meta", &be(0)].concat().repeat(k);
    mp4.build()
}

/// `k` `trex` atoms and one `moof` of `k` track fragments that none of them describes.
fn trex_times_traf(k: usize) -> Vec<u8> {
    let mut mp4 = Mp4::new();
    mp4.stsz = full(b"stsz", 0, &[be(0), be(0)].concat());
    mp4.mdat = 0;
    let trex: Vec<u8> = (1..=k as u32).flat_map(|id| full(b"trex", 0, &[be(id), be(1), be(1024), be(0), be(0)].concat())).collect();
    mp4.mvex = atom(b"mvex", &trex);
    let traf = atom(b"traf", &full(b"tfhd", 0x02_0000, &be(0)));
    mp4.fragments = atom(b"moof", &[full(b"mfhd", 0, &be(1)), traf.repeat(k)].concat());
    mp4.build()
}

/// `k` sample-to-chunk runs over `2k` chunks of `2k` samples of one byte.
fn runs_times_chunks(k: usize) -> Vec<u8> {
    let mut mp4 = Mp4::new();
    mp4.stsz = full(b"stsz", 0, &[be(1), be(2 * k as u32)].concat());
    mp4.stsc = full(b"stsc", 0, &[be(k as u32).to_vec(), (0..k as u32).flat_map(|i| [be(2 * i + 1), be(1), be(1)].concat()).collect()].concat());
    mp4.chunks = 2 * k;
    mp4.mdat = 2 * k;
    mp4.build()
}

fn free_atoms(k: usize) -> Vec<u8> {
    let mut mp4 = Mp4::new();
    mp4.before_moov = [&be(8)[..], b"free"].concat().repeat(k);
    mp4.build()
}

fn tag_chain(k: usize) -> Vec<u8> { [b"ID3\x04\0\0\0\0\0\0".repeat(k), Mp4::new().build()].concat() }

fn caf_chunks(k: usize) -> Vec<u8> {
    const CAF: &[u8] = include_bytes!("fixtures/aac-reader/tiny-44100-2.caf");
    [&CAF[..8], &[b"free".as_slice(), &[0; 8]].concat().repeat(k), &CAF[8..]].concat()
}

/// A reader's cost is its CPU time (the thread's, so a loaded machine does not show): the least of two runs.
fn cost(scratch: &Scratch, allowed: &Arc<BTreeSet<i32>>, name: &str, extension: &str, file: &[u8]) -> (Outcome, Duration) {
    let mode = Mode { limit: Duration::from_secs(60), ..Mode::new() };
    let (outcome, first) = guarded_stats(scratch, allowed, name, &format!("{name}.{extension}"), file, mode);
    let (_, second) = guarded_stats(scratch, allowed, name, &format!("{name}.{extension}"), file, mode);
    (outcome, first.cpu.min(second.cpu))
}

/// Every structure of a file that a reader walks, built in two sizes (one and four times): the CPU time of the larger must stay within a small multiple
/// of the smaller's (a quadratic walk costs sixteen times as much), and the allocation limits of `run_case` (proportional to the input) hold for both.
/// The smaller sizes are those at which the naive walk of each structure takes a fifth of a second or more.
#[test]
fn every_structure_costs_in_proportion_to_its_size() {
    const FAMILIES: [Family; 7] = [
        Family { name: "decoy-tracks", extension: "m4a", small: 16_384, build: decoy_tracks },
        Family { name: "udta-chain", extension: "m4a", small: 32_768, build: udta_chain },
        Family { name: "trex-times-traf", extension: "m4a", small: 32_768, build: trex_times_traf },
        Family { name: "stsc-times-stco", extension: "m4a", small: 16_384, build: runs_times_chunks },
        Family { name: "free-atoms", extension: "m4a", small: 8_192, build: free_atoms },
        Family { name: "tag-chain", extension: "m4a", small: 65_536, build: tag_chain },
        Family { name: "caf-chunks", extension: "caf", small: 8_192, build: caf_chunks },
    ];
    const SLACK: Duration = Duration::from_millis(150);
    let allowed = statuses_apple_produced(Fixtures::load());
    let scratch = Scratch::new("proportion");
    for family in &FAMILIES {
        let (small, large) = ((family.build)(family.small), (family.build)(family.small * 4));
        let (outcome_small, cpu_small) = cost(&scratch, &allowed, family.name, family.extension, &small);
        let (outcome_large, cpu_large) = cost(&scratch, &allowed, family.name, family.extension, &large);
        println!("{}: {} bytes {cpu_small:.2?}, {} bytes {cpu_large:.2?} ({outcome_small:?})", family.name, small.len(), large.len());
        assert!(cpu_large <= 8 * cpu_small + SLACK, "{}: {cpu_small:?} for {} bytes but {cpu_large:?} for {}", family.name, small.len(), large.len());
        assert!(cpu_large < Duration::from_secs(2), "{}: {cpu_large:?} for {} bytes", family.name, large.len());
        // the same kind of answer at both sizes
        assert_eq!(std::mem::discriminant(&outcome_small), std::mem::discriminant(&outcome_large), "{}: {outcome_small:?} against {outcome_large:?}", family.name);
    }
}

/// Chains longer than the readers look at (the AAC sniffers stop at 2^20 atoms or chunks, the others at 2^16): the file still opens as the AAC container
/// model sees it, in time proportional to its size.
#[test]
fn chains_beyond_every_scan_limit_cost_no_more_than_their_size() {
    let allowed = statuses_apple_produced(Fixtures::load());
    let scratch = Scratch::new("beyond");
    let mode = Mode { limit: Duration::from_secs(60), ..Mode::new() };
    for (name, extension, file) in [("free-atoms-2m", "m4a", free_atoms(2_000_000)), ("caf-chunks-2m", "caf", caf_chunks(2_000_000)), ("tag-chain-1m", "m4a", tag_chain(1_200_000))] {
        let (outcome, stats) = guarded_stats(&scratch, &allowed, name, &format!("{name}.{extension}"), &file, mode);
        println!("{name}: {} bytes, {:.2?} CPU, largest allocation {}, live {}: {outcome:?}", file.len(), stats.cpu, stats.largest, stats.live);
        assert!(stats.cpu < Duration::from_secs(3), "{name}: {:?} CPU for {} bytes", stats.cpu, file.len());
        if name != "tag-chain-1m" {
            assert!(matches!(outcome, Outcome::Decoded { .. }), "{name}: {outcome:?}");
        }
    }
}

// ---- structure-aware grammars ---------------------------------------------------------------------------------------------------------

impl Rng {
    fn chance(&mut self, one_in: usize) -> bool { self.below(one_in) == 0 }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T { items[self.below(items.len())] }
    fn bytes(&mut self, count: usize) -> Vec<u8> { (0..count).map(|_| self.byte()).collect() }
    /// A number below `normal`, one time in `one_in` below `rare`.
    fn scaled(&mut self, normal: usize, one_in: usize, rare: usize) -> usize { let limit = if self.chance(one_in) { rare } else { normal }; self.below(limit) }
    /// Up to `limit` random bytes.
    fn noise(&mut self, limit: usize) -> Vec<u8> { let count = self.below(limit); self.bytes(count) }
}

/// An MSB-first bit writer.
#[derive(Default)]
struct Bits { bytes: Vec<u8>, used: usize }
impl Bits {
    fn bit(&mut self, set: bool) {
        if self.used == 0 { self.bytes.push(0); }
        if set { *self.bytes.last_mut().expect("a byte") |= 0x80 >> self.used; }
        self.used = (self.used + 1) % 8;
    }
    fn put(&mut self, value: u64, width: usize) { for shift in (0..width).rev() { self.bit(value >> shift & 1 == 1); } }
    fn bytes(&mut self, data: &[u8]) { for &byte in data { self.put(u64::from(byte), 8); } }
}

/// Audio specific configurations: AAC-LC 44.1 kHz stereo, 48 kHz stereo, 44.1 kHz mono, 24 kHz stereo, Main profile, and 3-channel LC.
const CONFIGS: [[u8; 2]; 6] = [[0x12, 0x10], [0x11, 0x90], [0x12, 0x08], [0x13, 0x10], [0x0a, 0x10], [0x12, 0x18]];

/// The access units of an AAC-LC 44.1 kHz stereo stream (those of the LOAS fixture), for grammar cases that must decode.
fn units() -> &'static [Vec<u8>] {
    static UNITS: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    UNITS.get_or_init(|| {
        const LOAS: &[u8] = include_bytes!("fixtures/aac-reader/tiny-44100-2.loas");
        let (mut at, mut units): (usize, Vec<Vec<u8>>) = (0, Vec::new());
        while at + 3 <= LOAS.len() {
            let length = usize::from(LOAS[at + 1] & 0x1f) << 8 | usize::from(LOAS[at + 2]);
            let body = &LOAS[at + 3..at + 3 + length];
            let read = |from: usize, count: usize| (from..from + count).fold(0usize, |acc, bit| acc << 1 | usize::from(body[bit / 8] >> (7 - bit % 8) & 1));
            // `useSameStreamMux`, then (without it) the 44 bits of the configuration, then the payload length in one byte
            let start = if read(0, 1) == 0 { 45 } else { 1 };
            let payload = read(start, 8);
            assert!(payload < 255);
            units.push((0..payload).map(|byte| read(start + 8 + 8 * byte, 8) as u8).collect());
            at += 3 + length;
        }
        assert!(units.len() >= 8 && units.iter().all(|unit| !unit.is_empty()));
        units
    })
}

/// What a grammar case has made of the stream, for the coverage check of the test.
#[derive(Default, Debug)]
struct Coverage { frames: usize, later_new_config: usize, later_over_long: usize, later_new_config_over_long: usize, later_unreadable: usize, cut: usize }

/// A LATM/LOAS stream of one to nine frames: a configuration (version 0 or 1, other program or layer counts, other length types) or "same as before"
/// in each, a payload length that is exact, short, over-long, escaped, zero or missing, a frame length that is exact, short or over-long, and the whole
/// stream cut at any byte.
fn loas_stream(rng: &mut Rng, coverage: &mut Coverage) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous = false;
    for index in 0..1 + rng.below(9) {
        let new_config = index == 0 || rng.chance(3);
        let mut bits = Bits::default();
        bits.bit(!new_config);
        let mut readable = true;
        if new_config {
            let version = u64::from(rng.chance(8));
            bits.put(version, 1);
            if version == 1 { bits.bit(rng.chance(30)); bits.put(0, 2); bits.put(u64::from(rng.byte()), 8); }
            bits.bit(true);
            bits.put(0, 6);
            let programs = if rng.chance(20) { 1 + rng.below(2) } else { 0 };
            bits.put(programs as u64, 4);
            bits.put(0, 3 * usize::from(programs == 0 || rng.chance(2)));
            let config = if rng.chance(12) { [rng.byte(), rng.byte()] } else { rng.pick(&CONFIGS) };
            if version == 1 { bits.put(0, 2); bits.put(16, 8); }
            bits.bytes(&config);
            let length_type = if rng.chance(15) { rng.pick(&[1u64, 2, 3, 7]) } else { 0 };
            bits.put(length_type, 3);
            bits.put(0xff, 8);
            bits.bit(rng.chance(25));
            bits.bit(rng.chance(25));
            readable = programs == 0 && length_type == 0 && version == 0 && CONFIGS.contains(&config);
        }
        let au = if rng.chance(4) { rng.noise(700) } else { units()[rng.below(units().len())].clone() };
        let mut over_long = false;
        match rng.below(20) {
            0 => { let claim = au.len() + 1 + rng.below(300); push_length(&mut bits, claim); bits.bytes(&au); over_long = true; }
            1 => { let claim = au.len().saturating_sub(1 + rng.below(4)); push_length(&mut bits, claim); bits.bytes(&au); }
            2 => push_length(&mut bits, 0),
            3 => { bits.put(0xffff_ff, 24); readable = false; }
            4 => {}
            _ => { push_length(&mut bits, au.len()); bits.bytes(&au); }
        }
        let body = bits.bytes;
        let claimed = match rng.below(40) { 0 => (body.len() + 1 + rng.below(200)).min(8191), 1 => body.len().saturating_sub(1 + rng.below(3)), _ => body.len() }.min(8191);
        if index > 0 {
            coverage.later_new_config += usize::from(new_config);
            coverage.later_over_long += usize::from(over_long);
            coverage.later_new_config_over_long += usize::from(new_config && over_long);
            coverage.later_unreadable += usize::from(!readable || over_long);
        }
        coverage.frames += 1;
        out.extend_from_slice(&[0x56, 0xe0 | (claimed >> 8) as u8, claimed as u8]);
        out.extend_from_slice(&body);
        previous = new_config;
    }
    let _ = previous;
    if rng.chance(4) {
        let at = rng.below(out.len() + 1);
        out.truncate(at);
        coverage.cut += 1;
    }
    if rng.chance(20) { out.extend(rng.noise(20)); }
    out
}

/// A payload length in the LATM escape form: 255s and a last byte below 255.
fn push_length(bits: &mut Bits, length: usize) {
    for _ in 0..length / 255 { bits.put(255, 8); }
    bits.put((length % 255) as u64, 8);
}

/// A CAF file of AAC: the chunks in an order, with omissions and repeats, sizes that are exact, short, long, -1 or absurd, a packet table that is exact
/// or claims too much or too little, a cookie as an `esds` body or as a bare configuration, and a cut.
fn caf_stream(rng: &mut Rng) -> Vec<u8> {
    fn chunk(rng: &mut Rng, kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size: i64 = match rng.below(60) { 0 => body.len() as i64 + 1 + rng.below(100) as i64, 1 => (body.len() as i64 - 1 - rng.below(5) as i64).max(0), 2 => -1, 3 => i64::MAX, 4 => -2, _ => body.len() as i64 };
        [&kind[..], &size.to_be_bytes(), body].concat()
    }
    let sane = rng.chance(2);
    let channels = if sane { 2 } else { rng.pick(&[1u32, 2, 2, 3, 6, 8, 0, 99]) };
    let rate = if sane { 44_100.0 } else { rng.pick(&[44_100.0f64, 48_000.0, 8_000.0, 96_000.0, 0.0, f64::NAN, 1e300]) };
    let packet_frames = if sane { 1024 } else { rng.pick(&[1024u32, 960, 0, 4096, 123]) };
    let format = if sane { *b"aac " } else { rng.pick(&[*b"aac ", *b"aach", *b"aacp", *b"alac", *b"lpcm"]) };
    let desc = [&rate.to_bits().to_be_bytes()[..], &format, &be(rng.pick(&[0, 0, 0, 2])), &be(0), &be(packet_frames), &be(channels), &be(0)].concat();
    let config = if sane || rng.chance(2) { CONFIGS[0] } else { rng.pick(&CONFIGS) };
    let cookie = if rng.chance(2) {
        let decoder = [&[0x40, 0x15, 0, 0, 0][..], &[0; 8], &descriptor(5, &config)].concat();
        descriptor(3, &[&[0, 1, 0][..], &descriptor(4, &decoder), &descriptor(6, &[2])].concat())
    } else { config.to_vec() };
    // mostly real access units (a few of them zero-sized), now and then sizes of nothing
    let real = !rng.chance(8);
    let count = rng.scaled(40, 8, 3000);
    let picked: Vec<Vec<u8>> = (0..count).map(|_| if rng.chance(30) { Vec::new() } else if real { units()[rng.below(units().len())].clone() } else { rng.noise(400) }).collect();
    let sizes: Vec<usize> = picked.iter().map(Vec::len).collect();
    let mut table = Vec::new();
    for &size in &sizes {
        let groups = (0..5).rev().find(|&g| size >> (7 * g) != 0).unwrap_or(0);
        for g in (0..=groups).rev() { table.push(((size >> (7 * g)) & 0x7f) as u8 | if g > 0 { 0x80 } else { 0 }); }
    }
    let claimed = match rng.below(12) { 0 => count as i64 + 1 + rng.below(50) as i64, 1 => (count as i64 - 1).max(0), 2 => i64::MAX, 3 => -5, _ => count as i64 };
    let priming = rng.pick(&[0i32, 0, 2112, 1024, -1, i32::MAX]);
    let remainder = rng.pick(&[0i32, 0, 100, 1023, -1, i32::MAX]);
    let valid = match rng.below(6) { 0 => 0, 1 => i64::MAX, 2 => rng.below(100_000) as i64, _ => (count as i64 * 1024 - i64::from(priming.max(0)) - i64::from(remainder.max(0))).max(0) };
    let pakt = [&claimed.to_be_bytes()[..], &valid.to_be_bytes(), &priming.to_be_bytes(), &remainder.to_be_bytes(), &table].concat();
    let mut payload: Vec<u8> = picked.concat();
    match rng.below(8) { 0 => payload.truncate(payload.len() / 2), 1 => payload.extend(rng.noise(500)), _ => {} }
    let data = [&be(0)[..], &payload].concat();
    let mut order: Vec<(&[u8; 4], Vec<u8>)> = vec![(b"desc", desc), (b"kuki", cookie), (b"pakt", pakt), (b"data", data)];
    if rng.chance(5) { let at = rng.below(order.len() + 1); order.insert(at, (b"free", rng.noise(30))); }
    if rng.chance(8) { let at = rng.below(order.len() + 1); order.insert(at, (b"chan", [&be(0x0065_0002)[..], &be(0), &be(0)].concat())); }
    if rng.chance(6) { let at = rng.below(order.len() + 1); order.insert(at, (b"info", rng.bytes(24))); }
    match rng.below(10) {
        0 => { order.swap(0, 3); }
        1 => { order.remove(rng.below(order.len())); }
        2 => { let again = order[rng.below(order.len())].clone(); let at = rng.below(order.len() + 1); order.insert(at, again); }
        3 => { order.swap(2, 3); }
        _ => {}
    }
    let mut out = [&b"caff"[..], &[0, 1, 0, 0, 0, 0, 0, 0][..]].concat();
    out.truncate(8);
    for (kind, body) in &order { let bytes = chunk(rng, kind, body); out.extend(bytes); }
    if rng.chance(5) {
        let at = rng.below(out.len() + 1);
        out.truncate(at);
    }
    out
}

/// A fragmented MP4: a movie with an empty sample table, a `mvex` of any number of `trex` atoms (ids that match the track and ids that do not), up to five
/// fragments of up to four track fragments each, every `tfhd` and `trun` with any subset of its optional fields, counts that the atom holds, does not
/// hold or that are 2^32 - 1, data offsets before, inside and far beyond the file, and a damaged atom size now and then.
fn fragmented_stream(rng: &mut Rng) -> Vec<u8> {
    let mut mp4 = Mp4::new();
    mp4.stsz = full(b"stsz", 0, &[be(0), be(0)].concat());
    mp4.mdat = 0;
    let trex: Vec<u8> = (0..rng.below(5)).flat_map(|_| full(b"trex", 0, &[be(rng.pick(&[0, 0, 1, 2, 0xffff_ffff])), be(1), be(rng.pick(&[0, 1024, 1024])), be(rng.pick(&[0, 1, 50, 70_000])), be(rng.pick(&[0, 0x10000]))].concat())).collect();
    if !trex.is_empty() || rng.chance(2) { mp4.mvex = atom(b"mvex", &trex); }
    let mut fragments = Vec::new();
    for sequence in 0..rng.below(6) {
        if rng.chance(2) {
            // a fragment that holds real access units
            let picked: Vec<&Vec<u8>> = (0..1 + rng.below(4)).map(|_| &units()[rng.below(units().len())]).collect();
            let make = |data_offset: u32| {
                let mut trun = [be(picked.len() as u32), be(data_offset)].concat();
                for unit in &picked { trun.extend(be(unit.len() as u32)); }
                atom(b"moof", &[full(b"mfhd", 0, &be(sequence as u32 + 1)), atom(b"traf", &[full(b"tfhd", 0x02_0000, &be(0)), full(b"trun", 0x201, &trun)].concat())].concat())
            };
            let moof = make(0).len() as u32;
            fragments.extend(make(moof + 8));
            fragments.extend(atom(b"mdat", &picked.iter().flat_map(|unit| unit.iter().copied()).collect::<Vec<u8>>()));
            continue;
        }
        let mut moof = full(b"mfhd", 0, &be(sequence as u32 + 1));
        for _ in 0..rng.below(5) {
            let flags = rng.pick(&[0u32, 0x1, 0x2, 0x8, 0x10, 0x20, 0x2_0000, 0x1_0000, 0x3a, 0x3b, 0x2_003a]) | if rng.chance(3) { 0x2_0000 } else { 0 };
            let mut tfhd = be(rng.pick(&[0, 0, 0, 1, 7])).to_vec();
            if flags & 1 != 0 { tfhd.extend(rng.pick(&[0u64, 8, 100, u64::MAX, 1 << 63]).to_be_bytes()); }
            for (bit, value) in [(2u32, 1u32), (8, 1024), (0x10, rng.pick(&[0, 3, 100, u32::MAX])), (0x20, 0)] { if flags & bit != 0 { tfhd.extend(be(value)); } }
            let mut traf = full(b"tfhd", flags, &tfhd);
            for _ in 0..rng.below(3) {
                let flags = rng.pick(&[0u32, 0x1, 0x4, 0x100, 0x200, 0x400, 0x800, 0x201, 0x301, 0x205, 0xf01, 0xf05]);
                let count = rng.pick(&[0u32, 1, 2, 3, 10, 64, 1000, u32::MAX, 1 << 31]);
                let mut trun = be(count).to_vec();
                if flags & 1 != 0 { trun.extend(rng.pick(&[0i32, 8, 100, -100, i32::MAX, i32::MIN]).to_be_bytes()); }
                if flags & 4 != 0 { trun.extend(be(0)); }
                let written = if rng.chance(4) { 4 } else { (count as usize).min(64) };
                for _ in 0..written {
                    for (bit, value) in [(0x100u32, 1024u32), (0x200, rng.pick(&[0, 1, 2, 17, 100, u32::MAX])), (0x400, 0), (0x800, rng.pick(&[0, 1, u32::MAX]))] { if flags & bit != 0 { trun.extend(be(value)); } }
                }
                traf.extend(full(b"trun", flags, &trun));
            }
            moof.extend(atom(b"traf", &traf));
        }
        fragments.extend(atom(b"moof", &moof));
        if !rng.chance(5) { fragments.extend(atom(b"mdat", &rng.noise(400))); }
    }
    mp4.fragments = fragments;
    let mut out = mp4.build();
    if rng.chance(10) {
        let starts = box_starts(&out);
        if !starts.is_empty() {
            let at = starts[rng.below(starts.len())];
            put_u32(&mut out, at, rng.pick(&[0, 1, 7, 8, 12, 0x7fff_ffff, 0xffff_ffff]));
        }
    }
    if rng.chance(8) {
        let at = rng.below(out.len() + 1);
        out.truncate(at);
    }
    out
}

/// Run `cases` generated inputs through the reader (both read shapes, some with a client rate); returns how many were refused at open, opened, ended in
/// a read failure and delivered audio.
fn run_grammar(tag: &str, cases: usize, seed: u64, extensions: &[&str], mut make: impl FnMut(&mut Rng) -> Vec<u8>) -> (usize, usize, usize, usize) {
    let allowed = statuses_apple_produced(Fixtures::load());
    let scratch = Scratch::new(tag);
    let mut rng = Rng(seed);
    let (mut refused, mut decoded, mut failed, mut audible) = (0, 0, 0, 0);
    let started = Instant::now();
    for case in 0..cases {
        let data = make(&mut rng);
        let name = format!("g.{}", extensions[case % extensions.len()]);
        let mode = Mode { interleaved: case % 3 == 0, rate: if case % 11 == 0 { Some(48_000.0) } else { None }, ..Mode::new() };
        match guarded(&scratch, &allowed, &format!("{tag} {case}"), &name, &data, mode) {
            Outcome::Refused(_) => refused += 1,
            Outcome::Decoded { frames, error, .. } => { decoded += 1; failed += usize::from(error.is_some()); audible += usize::from(frames > 0); }
        }
    }
    println!("{tag}: {cases} cases in {:.1?}: {refused} refused at open, {decoded} opened ({audible} with audio, {failed} ending in a failure)", started.elapsed());
    (refused, decoded, failed, audible)
}

/// In debug builds a fifth of the cases.
fn grammar_cases(release: usize) -> usize { if cfg!(debug_assertions) { release / 5 } else { release } }

#[test]
fn loas_streams_of_any_frame_never_panic_hang_or_misreport() {
    let mut coverage = Coverage::default();
    let cases = grammar_cases(20_000);
    let (refused, decoded, failed, audible) = run_grammar("loas-grammar", cases, 0x1b4f_97c3_a105_55d1, &["loas", "latm", "bin"], |rng| loas_stream(rng, &mut coverage));
    println!("{coverage:?}");
    // the grammar must reach what it is for: frames behind the first that carry another configuration or a payload the frame cannot hold, and cut streams
    assert!(coverage.later_new_config > cases / 10 && coverage.later_over_long > cases / 100 && coverage.later_new_config_over_long > cases / 1000 && coverage.cut > cases / 10, "{coverage:?}");
    assert!(decoded > cases / 3 && failed > cases / 10 && refused > cases / 100 && audible > cases / 10, "refused {refused}, opened {decoded}, failed {failed}, with audio {audible}");
}

#[test]
fn caf_files_of_any_chunk_never_panic_hang_or_misreport() {
    let cases = grammar_cases(8_000);
    let (refused, decoded, _failed, audible) = run_grammar("caf-grammar", cases, 0x5d1e_0cf3_77a9_c001, &["caf"], caf_stream);
    assert!(refused > cases / 20 && decoded > cases / 10 && audible > cases / 20, "refused {refused}, opened {decoded}, with audio {audible}");
}

#[test]
fn fragmented_mp4_of_any_fragment_never_panic_hang_or_misreport() {
    let cases = grammar_cases(8_000);
    let (refused, decoded, _failed, audible) = run_grammar("fragment-grammar", cases, 0x0f4a_6713_9be2_77d5, &["m4a", "mp4"], fragmented_stream);
    assert!(refused > cases / 100 && decoded > cases / 5 && audible > cases / 10, "refused {refused}, opened {decoded}, with audio {audible}");
}
