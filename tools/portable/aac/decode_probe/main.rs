// Drives the engine's own apple_codec::Reader and Writer (macOS only), exactly as the engine does: the producer of the Apple
// reference in tests/fixtures/aac-decode. Build: `cargo build --release --offline` in this directory.
//
//   decode_probe record FILE [--rate R] [--block N] [--shapes planar,inter]
//       one JSON line: the open outcome (source description, client format, file-length and bitrate properties, or the failing
//       call and its OSStatus), then per shape (planar = the analysis path: two planes at the client rate, mono duplicated;
//       inter = interleaved at the source channel count) the frames delivered, the read failure that ended the stream, the
//       SHA-256 of the float32 stream (little endian, planar: L,R per frame) and of each plane, a digest of the read lengths, and what
//       every read that returned fewer frames than asked for wrote into the (sentinel-filled) buffer behind them: Apple's converter
//       delivers the decoder's padding there.
//       For an interleaved stream that ends in a failure the raw AudioToolbox behaviour of the reads that follow (the engine's
//       Reader refuses them as poisoned); for a refused planar client format the interleaved read of that same reader.
//   decode_probe gen OUT.m4a RATE CHANNELS FRAMES CONTENT SEED
//       encode a generated signal with the engine's Writer (256 kbit/s, 512-frame writes) and pin the MP4 header times.
//   decode_probe dump FILE OUT.f32 planar|inter [BLOCK] [RATE]
//       the delivered float32 stream itself (as apple_probe of the MP3 fixtures does), for cross-checks.
#![allow(dead_code)]
#[path = "../../../../src/apple_codec.rs"]
mod apple_codec;
use apple_codec::{CodecError, OutputFormat, Reader, Writer, WriterConfig, set_mp4_header_times};
use std::ffi::c_void;
use std::io::Write;
use std::path::Path;

/// The creation/modification time every generated MP4 is pinned to (seconds since 1904-01-01).
const FIXED_SECONDS: u32 = 3_800_000_000;

// ---- SHA-256 ---------------------------------------------------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[derive(Clone)]
struct Sha256 { state: [u32; 8], buffer: Vec<u8>, length: u64 }
impl Sha256 {
    fn new() -> Self {
        Sha256 { state: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19], buffer: Vec::new(), length: 0 }
    }
    fn block(state: &mut [u32; 8], chunk: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 { w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap()); }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g; g = f; f = e; e = d.wrapping_add(t1); d = c; c = b; b = a; a = t1.wrapping_add(t2);
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) { *s = s.wrapping_add(v); }
    }
    fn update(&mut self, data: &[u8]) {
        self.length += data.len() as u64;
        self.buffer.extend_from_slice(data);
        let whole = self.buffer.len() / 64 * 64;
        for chunk in self.buffer[..whole].chunks_exact(64) { Self::block(&mut self.state, chunk); }
        self.buffer.drain(..whole);
    }
    fn hex(mut self) -> String {
        let bits = self.length * 8;
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 { self.buffer.push(0); }
        self.buffer.extend_from_slice(&bits.to_be_bytes());
        for chunk in self.buffer.chunks_exact(64) { Self::block(&mut self.state, chunk); }
        self.state.iter().map(|v| format!("{v:08x}")).collect()
    }
}

// ---- JSON ------------------------------------------------------------------------------------------------------------------

fn q(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn error_json(error: &CodecError) -> String {
    match error {
        CodecError::Native { operation, status } => format!("{{\"operation\":{},\"status\":{status}}}", q(operation)),
        other => format!("{{\"other\":{}}}", q(&other.to_string())),
    }
}

fn number(value: f64) -> String {
    if value.is_finite() && value == value.trunc() && value.abs() < 1e15 { format!("{}", value as i64) } else { format!("{value:?}") }
}

fn asbd_json(s: &apple_codec::StreamDescription) -> String {
    format!("{{\"sample_rate\":{},\"format_id\":{},\"format_flags\":{},\"bytes_per_packet\":{},\"frames_per_packet\":{},\"bytes_per_frame\":{},\"channels\":{},\"bits\":{},\"reserved\":{}}}",
        number(s.sample_rate), s.format_id, s.format_flags, s.bytes_per_packet, s.frames_per_packet, s.bytes_per_frame, s.channels_per_frame, s.bits_per_channel, s.reserved)
}

// ---- reads -----------------------------------------------------------------------------------------------------------------

struct Stream {
    frames: u64,
    reads: u64,
    short: u64,
    lens: Sha256,
    error: Option<CodecError>,
    all: Sha256,
    planes: Option<(Sha256, Sha256)>,
    /// `[frames returned, frames written behind them, FNV-1a of what was written]` for every read that returned fewer frames than asked for.
    tails: Vec<String>,
}
impl Stream {
    fn json(self) -> String {
        let planes = match self.planes {
            Some((l, r)) => format!(",\"planes\":[\"{}\",\"{}\"]", l.hex(), r.hex()),
            None => String::new(),
        };
        format!("{{\"frames\":{},\"reads\":{},\"short\":{},\"lens_sha256\":\"{}\",\"error\":{},\"sha256\":\"{}\"{planes},\"tails\":[{}]}}",
            self.frames, self.reads, self.short, self.lens.hex(), self.error.as_ref().map(error_json).unwrap_or_else(|| "null".into()), self.all.hex(), self.tails.join(","))
    }
}

const SENTINEL: f32 = 12345.5;
const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;
fn fold(hash: u64, bits: u32) -> u64 { (hash ^ u64::from(bits)).wrapping_mul(0x100_0000_01b3) }

/// What a read that returned `n < block` frames wrote behind them into the sentinel-filled planes: the frames up to the last
/// one that differs from the sentinel, and the FNV-1a of their float bits (L then R per frame).
fn tail_planar(left: &[f32], right: &[f32], n: usize) -> String {
    let (mut written, mut hash) = (0usize, FNV_START);
    for i in n..left.len() { if left[i] != SENTINEL || right[i] != SENTINEL { written = i + 1 - n; } }
    for i in n..n + written { hash = fold(hash, left[i].to_bits()); hash = fold(hash, right[i].to_bits()); }
    format!("[{n},{written},\"{hash:016x}\"]")
}

fn tail_inter(buffer: &[f32], channels: usize, n: usize) -> String {
    let (mut written, mut hash) = (0usize, FNV_START);
    for i in n * channels..buffer.len() { if buffer[i] != SENTINEL { written = (i + channels) / channels - n; } }
    for v in &buffer[n * channels..(n + written) * channels] { hash = fold(hash, v.to_bits()); }
    format!("[{n},{written},\"{hash:016x}\"]")
}

/// Reads of `block` frames until one returns 0 or fails; the frames of a failed read are not delivered. The buffers are
/// pre-filled with a sentinel so that what a read writes behind the frames it returns is recorded too.
fn decode_planar(reader: &mut Reader, client_rate: f64, block: usize) -> Stream {
    let mut stream = Stream { frames: 0, reads: 0, short: 0, lens: Sha256::new(), error: None, all: Sha256::new(), planes: Some((Sha256::new(), Sha256::new())), tails: Vec::new() };
    if let Err(e) = reader.set_stereo_planar(client_rate) { stream.error = Some(e); return stream; }
    let (mut left, mut right) = (vec![SENTINEL; block], vec![SENTINEL; block]);
    let mut bytes = Vec::with_capacity(block * 8);
    let (mut plane_l, mut plane_r) = (Vec::with_capacity(block * 4), Vec::with_capacity(block * 4));
    loop {
        left.fill(SENTINEL);
        right.fill(SENTINEL);
        match reader.read_stereo_planar(&mut left, &mut right) {
            Ok(n) => {
                if n < block { stream.tails.push(tail_planar(&left, &right, n)); }
                if n == 0 { break; }
                bytes.clear();
                plane_l.clear();
                plane_r.clear();
                for i in 0..n {
                    bytes.extend_from_slice(&left[i].to_le_bytes());
                    bytes.extend_from_slice(&right[i].to_le_bytes());
                    plane_l.extend_from_slice(&left[i].to_le_bytes());
                    plane_r.extend_from_slice(&right[i].to_le_bytes());
                }
                stream.all.update(&bytes);
                let planes = stream.planes.as_mut().unwrap();
                planes.0.update(&plane_l);
                planes.1.update(&plane_r);
                stream.lens.update(&(n as u32).to_le_bytes());
                stream.reads += 1;
                stream.short += u64::from(n < block);
                stream.frames += n as u64;
            }
            Err(e) => { stream.error = Some(e); break; }
        }
    }
    stream
}

fn decode_inter(reader: &mut Reader, block: usize) -> Stream {
    let channels = reader.source_format().channels_per_frame as usize;
    let mut stream = Stream { frames: 0, reads: 0, short: 0, lens: Sha256::new(), error: None, all: Sha256::new(), planes: None, tails: Vec::new() };
    let mut buffer = vec![SENTINEL; block * channels];
    let mut bytes = Vec::with_capacity(block * channels * 4);
    loop {
        buffer.fill(SENTINEL);
        match reader.read(&mut buffer) {
            Ok(n) => {
                if n < block { stream.tails.push(tail_inter(&buffer, channels, n)); }
                if n == 0 { break; }
                bytes.clear();
                for sample in &buffer[..n * channels] { bytes.extend_from_slice(&sample.to_le_bytes()); }
                stream.all.update(&bytes);
                stream.lens.update(&(n as u32).to_le_bytes());
                stream.reads += 1;
                stream.short += u64::from(n < block);
                stream.frames += n as u64;
            }
            Err(e) => { stream.error = Some(e); break; }
        }
    }
    stream
}

// ---- raw AudioToolbox: what a read does after a failed read (the engine's Reader refuses to find out) ------------------------

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn ExtAudioFileOpenURL(url: *const c_void, file: *mut *mut c_void) -> i32;
    fn ExtAudioFileSetProperty(file: *mut c_void, property: u32, size: u32, data: *const c_void) -> i32;
    fn ExtAudioFileRead(file: *mut c_void, frames: *mut u32, data: *mut apple_codec::AudioBufferList) -> i32;
    fn ExtAudioFileDispose(file: *mut c_void) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(allocator: *const c_void, bytes: *const u8, length: isize, directory: u8) -> *const c_void;
    fn CFRelease(object: *const c_void);
}

#[repr(align(64))]
struct Aligned(apple_codec::AudioBufferList);

/// Reads `block` frames until the first failure, then up to `more` further reads on the same raw handle: `(status, frames)` each.
fn raw_after_failure(path: &Path, rate: f64, channels: u32, block: usize, more: usize) -> Option<(i32, Vec<(i32, u32)>)> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 0);
        let mut raw: *mut c_void = std::ptr::null_mut();
        let status = ExtAudioFileOpenURL(url, &mut raw);
        CFRelease(url);
        if status != 0 || raw.is_null() { return None; }
        let client = apple_codec::StreamDescription { sample_rate: rate, format_id: u32::from_be_bytes(*b"lpcm"), format_flags: 9, bytes_per_packet: channels * 4,
            frames_per_packet: 1, bytes_per_frame: channels * 4, channels_per_frame: channels, bits_per_channel: 32, reserved: 0 };
        let status = ExtAudioFileSetProperty(raw, u32::from_be_bytes(*b"cfmt"), std::mem::size_of::<apple_codec::StreamDescription>() as u32, (&client as *const apple_codec::StreamDescription).cast());
        if status != 0 { ExtAudioFileDispose(raw); return None; }
        let mut buffer = vec![0f32; block * channels as usize];
        let read = |buffer: &mut Vec<f32>| -> (i32, u32) {
            let mut frames = block as u32;
            let mut list = Aligned(apple_codec::AudioBufferList { number_buffers: 1, buffers: [apple_codec::AudioBuffer {
                number_channels: channels, data_byte_size: (block * channels as usize * 4) as u32, data: buffer.as_mut_ptr().cast() }] });
            let status = apple_codec::call_with_sp_residue(ExtAudioFileRead as *const () as usize, raw as usize, (&mut frames as *mut u32) as usize,
                (&mut list.0 as *mut apple_codec::AudioBufferList) as usize, 0);
            (status, frames)
        };
        let mut first = None;
        for _ in 0..1_000_000 {
            let (status, frames) = read(&mut buffer);
            if status != 0 { first = Some(status); break; }
            if frames == 0 { break; }
        }
        let result = first.map(|status| {
            let mut later = Vec::new();
            for _ in 0..more { later.push(read(&mut buffer)); }
            (status, later)
        });
        ExtAudioFileDispose(raw);
        result
    }
}

// ---- record ----------------------------------------------------------------------------------------------------------------

fn record(path: &str, rate: Option<f64>, block: usize, shapes: &str) {
    let open = Reader::open(path, rate);
    let reader = match open {
        Ok(reader) => reader,
        Err(error) => { println!("{{\"open\":{{\"ok\":false,\"error\":{}}}}}", error_json(&error)); return; }
    };
    let source = reader.source_format();
    let client_rate = rate.unwrap_or(source.sample_rate);
    let length = match reader.file_length_frames() { Ok(v) => format!("{{\"value\":{v}}}"), Err(e) => format!("{{\"error\":{}}}", error_json(&e)) };
    let bitrate = match reader.bitrate() { Ok(v) => format!("{{\"value\":{v}}}"), Err(e) => format!("{{\"error\":{}}}", error_json(&e)) };
    let mut line = format!("{{\"open\":{{\"ok\":true,\"source\":{},\"client\":{},\"length\":{length},\"bitrate\":{bitrate}}}", asbd_json(&source), asbd_json(&reader.client_format()));
    match reader.close() { Ok(()) => {} Err(e) => line.push_str(&format!(",\"close_error\":{}", error_json(&e))) }
    if shapes.contains("planar") {
        let mut reader = Reader::open(path, rate).expect("second open");
        let stream = decode_planar(&mut reader, client_rate, block);
        // A refused planar client format leaves the reader usable: what an interleaved read of that same reader delivers.
        let refused = matches!(&stream.error, Some(CodecError::Native { operation: "ExtAudioFileSetProperty(client format)", .. })) && stream.reads == 0;
        line.push_str(&format!(",\"planar\":{}", stream.json()));
        if refused { line.push_str(&format!(",\"inter_after_planar_refusal\":{}", decode_inter(&mut reader, block).json())); }
    }
    if shapes.contains("inter") {
        let mut reader = Reader::open(path, rate).expect("second open");
        let stream = decode_inter(&mut reader, block);
        let failed = stream.error.is_some();
        line.push_str(&format!(",\"inter\":{}", stream.json()));
        if failed {
            match raw_after_failure(Path::new(path), client_rate, source.channels_per_frame, block, 6) {
                Some((first, later)) => {
                    let later: Vec<String> = later.iter().map(|(s, f)| format!("[{s},{f}]")).collect();
                    line.push_str(&format!(",\"after_failure\":{{\"first_status\":{first},\"later\":[{}]}}", later.join(",")));
                }
                None => line.push_str(",\"after_failure\":null"),
            }
        }
    }
    line.push('}');
    println!("{line}");
}

// ---- dump ------------------------------------------------------------------------------------------------------------------

fn dump(path: &str, out: &str, planar: bool, block: usize, rate: Option<f64>) {
    let mut reader = match Reader::open(path, rate) { Ok(r) => r, Err(e) => { println!("{{\"ok\":false,\"stage\":\"open\",\"error\":{}}}", error_json(&e)); return; } };
    let source = reader.source_format();
    let channels = source.channels_per_frame as usize;
    let client_rate = rate.unwrap_or(source.sample_rate);
    let mut sink = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
    let (mut total, mut error) = (0usize, None);
    if planar {
        if let Err(e) = reader.set_stereo_planar(client_rate) { println!("{{\"ok\":false,\"stage\":\"planar\",\"error\":{}}}", error_json(&e)); return; }
        let (mut left, mut right) = (vec![0f32; block], vec![0f32; block]);
        loop {
            left.fill(0.0);
            right.fill(0.0);
            match reader.read_stereo_planar(&mut left, &mut right) {
                Ok(0) => break,
                Ok(n) => { for i in 0..n { sink.write_all(&left[i].to_le_bytes()).unwrap(); sink.write_all(&right[i].to_le_bytes()).unwrap(); } total += n; }
                Err(e) => { error = Some(e); break; }
            }
        }
    } else {
        let mut buffer = vec![0f32; block * channels];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => { for v in &buffer[..n * channels] { sink.write_all(&v.to_le_bytes()).unwrap(); } total += n; }
                Err(e) => { error = Some(e); break; }
            }
        }
    }
    sink.flush().unwrap();
    println!("{{\"ok\":{},\"frames\":{total},\"channels\":{channels},\"error\":{}}}", error.is_none(), error.as_ref().map(error_json).unwrap_or_else(|| "null".into()));
}

// ---- gen -------------------------------------------------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    /// Uniform in [-1, 1).
    fn uniform(&mut self) -> f64 { (self.next() >> 11) as f64 / (1u64 << 52) as f64 - 1.0 }
}

/// Interleaved float32 test signals. Each channel carries its own pitch, phase and level so that a channel mix-up is audible
/// in the decoded hashes; every content kind is a pure function of its arguments.
fn signal(content: &str, seed: u64, rate: f64, channels: usize, frames: usize) -> Vec<f32> {
    let mut out = vec![0f32; frames * channels];
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let tau = std::f64::consts::TAU;
    for c in 0..channels {
        let f0 = 150.0 + 83.0 * c as f64 + (seed % 11) as f64 * 17.0;
        let level = 1.0 - 0.07 * c as f64;
        for i in 0..frames {
            let t = i as f64 / rate;
            let tone = 0.30 * (tau * f0 * t).sin() + 0.12 * (tau * 2.01 * f0 * t).sin() + 0.05 * (tau * 3.5 * f0 * t).sin();
            let v = match content {
                "tone" => tone,
                "noise" => 0.25 * rng.uniform(),
                "clicks" => {
                    let period = 1777 + 131 * c;
                    let k = i % period;
                    let decay = if k < 400 { (-(k as f64) / 60.0).exp() * if (i / period) % 2 == 0 { 0.9 } else { -0.9 } } else { 0.0 };
                    decay + 0.03 * (tau * 90.0 * t).sin()
                }
                "mix" => {
                    let burst = if (i / 3000) % 4 == 2 { 0.35 * rng.uniform() } else { 0.0 };
                    let click = if i % 5003 < 30 { 0.7 * rng.uniform() } else { 0.0 };
                    0.5 * tone + burst + click
                }
                "gated" => if (i / 2500) % 3 == 1 { tone } else { 0.0 },
                "loud" => { let s = tone * 3.0 + 0.4 * (tau * 55.0 * t).sin(); s.clamp(-1.0, 1.0) }
                "low" => 0.0001 * rng.uniform(),
                "chirp" => 0.35 * (tau * (80.0 + 1800.0 * t * (1.0 + 0.1 * c as f64)) * t).sin(),
                "steps" => if (i / 4000) % 2 == 0 { 0.2 + tone * 0.2 } else { -0.15 + tone * 0.1 },
                other => panic!("unknown content {other}"),
            };
            out[i * channels + c] = (v * level) as f32;
        }
    }
    out
}

fn generate(out: &str, rate: f64, channels: u32, frames: usize, content: &str, seed: u64) {
    let config = WriterConfig { format: OutputFormat::Aac, sample_rate: rate, channels, bit_depth: 0 };
    let mut writer = match Writer::create(out, config) { Ok(w) => w, Err(e) => { println!("{{\"ok\":false,\"error\":{}}}", error_json(&e)); return; } };
    let warnings: Vec<String> = writer.warnings().iter().map(|w| q(&format!("{w:?}"))).collect();
    let samples = signal(content, seed, rate, channels as usize, frames);
    for block in samples.chunks(512 * channels as usize) {
        if let Err(e) = writer.write(block) { println!("{{\"ok\":false,\"error\":{}}}", error_json(&e)); return; }
    }
    if let Err(e) = writer.finish() { println!("{{\"ok\":false,\"error\":{}}}", error_json(&e)); return; }
    let patched = set_mp4_header_times(Path::new(out), FIXED_SECONDS).map(|n| n as i64).unwrap_or(-1);
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    println!("{{\"ok\":true,\"warnings\":[{}],\"patched\":{patched},\"bytes\":{bytes}}}", warnings.join(","));
}

// ---- main ------------------------------------------------------------------------------------------------------------------

fn arg(args: &[String], i: usize) -> &str { args.get(i).map(|s| s.as_str()).unwrap_or("") }
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match arg(&args, 1) {
        "record" => {
            let rate = flag(&args, "--rate").map(|s| s.parse::<f64>().expect("rate"));
            let block = flag(&args, "--block").map(|s| s.parse().expect("block")).unwrap_or(512);
            record(arg(&args, 2), rate, block, flag(&args, "--shapes").unwrap_or("planar,inter"));
        }
        "dump" => {
            let block = arg(&args, 5).parse().unwrap_or(512);
            let rate = args.get(6).and_then(|s| s.parse().ok());
            dump(arg(&args, 2), arg(&args, 3), arg(&args, 4) == "planar", block, rate);
        }
        "gen" => generate(arg(&args, 2), arg(&args, 3).parse().expect("rate"), arg(&args, 4).parse().expect("channels"), arg(&args, 5).parse().expect("frames"),
            arg(&args, 6), arg(&args, 7).parse().expect("seed")),
        _ => eprintln!("usage: decode_probe record|dump|gen ..."),
    }
}
