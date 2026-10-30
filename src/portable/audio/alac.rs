//! Portable ALAC packet decoding.
//!
//! Packet syntax and the adaptive predictor follow the Apple Lossless format.
//! No AudioToolbox calls are made here. Inputs outside this implementation's
//! checked bitstream subset are rejected rather than silently approximated.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub frame_length: u32,
    pub bit_depth: u8,
    pub pb: u8,
    pub mb: u8,
    pub kb: u8,
    pub channels: u8,
    pub sample_rate: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);
impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.0) }
}
impl std::error::Error for DecodeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    Invalid(&'static str),
    NativePacketLimit { first_bytes: usize, retry_bytes: usize, capacity: usize },
    /// An oversized packet that AudioToolbox could not place by its third attempt of a write call: it
    /// discards the audio of that call and the file would decode to fewer frames than were written.
    NativeFrameLoss { attempts: u32, capacity: usize },
    Poisoned,
}
impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) => f.write_str(s),
            Self::NativePacketLimit { first_bytes, retry_bytes, capacity } => write!(f,
                "ALAC packet exceeds AudioToolbox output capacity: first {first_bytes} bytes, retry {retry_bytes} bytes, capacity {capacity}"),
            Self::NativeFrameLoss { attempts, capacity } => write!(f,
                "ALAC packet still exceeds the AudioToolbox output capacity of {capacity} bytes at attempt {attempts}: AudioToolbox would discard audio"),
            Self::Poisoned => f.write_str("ALAC encoder cannot continue after an oversized packet"),
        }
    }
}
impl std::error::Error for EncodeError {}

type Result<T, E = DecodeError> = std::result::Result<T, E>;

struct Bits<'a> { data: &'a [u8], pos: usize }
impl Bits<'_> {
    fn read(&mut self, mut count: u32) -> Result<u32> {
        if count > 32 || self.pos.checked_add(count as usize).is_none_or(|end| end > self.data.len() * 8) {
            return Err(DecodeError("truncated ALAC packet"));
        }
        let mut value = 0u32;
        while count > 0 {
            let bit = self.pos & 7;
            let take = count.min((8 - bit) as u32);
            let shift = 8 - bit - take as usize;
            let mask = (1u32 << take) - 1;
            value = (value << take) | ((u32::from(self.data[self.pos / 8]) >> shift) & mask);
            self.pos += take as usize;
            count -= take;
        }
        Ok(value)
    }
    fn signed(&mut self, count: u32) -> Result<i32> {
        if count == 0 || count > 32 { return Err(DecodeError("invalid ALAC sample width")); }
        Ok(sign_extend(self.read(count)?, count))
    }
}

fn sign_extend(value: u32, bits: u32) -> i32 {
    ((value << (32 - bits)) as i32) >> (32 - bits)
}

// ALAC's modified Rice alphabet has a unary prefix, followed by either a
// truncated remainder or a verbatim unsigned word after nine leading ones.
fn rice(bits: &mut Bits<'_>, k: u32, escape_width: u32) -> Result<u32> {
    if !(1..=31).contains(&k) || escape_width == 0 || escape_width > 32 {
        return Err(DecodeError("invalid ALAC Rice parameter"));
    }
    let mut prefix = 0u32;
    while prefix < 9 && bits.read(1)? != 0 { prefix += 1; }
    if prefix == 9 { return bits.read(escape_width); }
    let modulus = (1u32 << k) - 1;
    let mut value = prefix.checked_mul(modulus).ok_or(DecodeError("ALAC Rice value overflows"))?;
    if k > 1 {
        let remainder = bits.read(k - 1)?;
        if remainder != 0 {
            let suffix = ((remainder << 1) | bits.read(1)?) - 1;
            value = value.checked_add(suffix).ok_or(DecodeError("ALAC Rice value overflows"))?;
        }
    }
    Ok(value)
}

fn residuals(bits: &mut Bits<'_>, count: usize, width: u32, config: Config, factor: u8, result: &mut Vec<i32>) -> Result<()> {
    result.clear();
    result.reserve(count);
    let mut mean = u32::from(config.mb);
    let weight = u32::from(config.pb) * u32::from(factor) / 4;
    let mut bias = 0u32;
    while result.len() < count {
        let magnitude = (mean >> 9).saturating_add(3);
        let k = (31 - magnitude.leading_zeros()).min(u32::from(config.kb));
        let n = rice(bits, k, width)?;
        let signed = n.wrapping_add(bias);
        result.push(if signed & 1 == 0 { (signed >> 1) as i32 } else { !((signed >> 1) as i32) });
        mean = weight.wrapping_mul(signed).wrapping_add(mean).wrapping_sub(weight.wrapping_mul(mean) >> 9);
        if n > 65535 { mean = 65535; }
        bias = 0;
        if mean < 128 && result.len() < count {
            let run_k = (mean.leading_zeros() as i32 - 24 + ((mean + 16) >> 6) as i32) as u32;
            let run = rice(bits, run_k, 16)? as usize;
            if run > count - result.len() { return Err(DecodeError("ALAC zero run exceeds frame")); }
            result.resize(result.len() + run, 0);
            bias = u32::from(run < 65535);
            mean = 0;
        }
    }
    Ok(())
}

fn predictor(residual: &mut [i32], coefficients: &mut [i16], width: u32, shift: u8) -> Result<()> {
    if residual.is_empty() { return Ok(()); }
    let order = coefficients.len();
    if order == 0 { return Ok(()); }
    if order == 31 {
        for i in 1..residual.len() {
            residual[i] = sign_extend(residual[i].wrapping_add(residual[i - 1]) as u32, width);
        }
        return Ok(());
    }
    if usize::from(shift) == 0 || order >= residual.len() {
        return Err(DecodeError("invalid ALAC predictor order or shift"));
    }
    for i in 1..=order {
        residual[i] = sign_extend(residual[i].wrapping_add(residual[i - 1]) as u32, width);
    }
    for i in (order + 1)..residual.len() {
        let base = residual[i - order - 1];
        let mut sum = 1i32 << (shift - 1);
        for (j, &coefficient) in coefficients.iter().enumerate() {
            sum = sum.wrapping_add(i32::from(coefficient).wrapping_mul(residual[i - j - 1].wrapping_sub(base)));
        }
        let error = residual[i];
        let prediction = base.wrapping_add(sum >> shift);
        residual[i] = sign_extend(prediction.wrapping_add(error) as u32, width);
        let direction = error.signum();
        if direction != 0 {
            let mut remaining = error;
            for j in (0..order).rev() {
                let difference = base.wrapping_sub(residual[i - j - 1]);
                let sign = difference.signum().wrapping_mul(direction);
                coefficients[j] = coefficients[j].wrapping_sub(sign as i16);
                let correction = sign.wrapping_mul(difference) >> shift;
                remaining = remaining.wrapping_sub(((order - j) as i32).wrapping_mul(correction));
                if (direction > 0 && remaining <= 0) || (direction < 0 && remaining >= 0) { break; }
            }
        }
    }
    Ok(())
}

struct Filter { mode: u8, shift: u8, factor: u8, order: usize, coefficients: [i16; 31] }
fn filter(bits: &mut Bits<'_>) -> Result<Filter> {
    let mode = bits.read(4)? as u8;
    let shift = bits.read(4)? as u8;
    let factor = bits.read(3)? as u8;
    let order = bits.read(5)? as usize;
    let mut coefficients = [0; 31];
    for coefficient in &mut coefficients[..order] { *coefficient = bits.read(16)? as i16; }
    Ok(Filter { mode, shift, factor, order, coefficients })
}

fn filtered(bits: &mut Bits<'_>, count: usize, width: u32, config: Config, mut f: Filter, values: &mut Vec<i32>) -> Result<()> {
    residuals(bits, count, width, config, f.factor, values)?;
    if f.mode != 0 {
        let mut differential = [0i16; 31];
        predictor(values, &mut differential, width, 0)?;
    }
    predictor(values, &mut f.coefficients[..f.order], width, f.shift)
}

/// Reusable decoding buffers for one ALAC stream.
#[derive(Default)]
pub struct Decoder {
    first: Vec<i32>,
    second: Vec<i32>,
    lows: Vec<u32>,
}
impl Decoder {
    pub fn new() -> Self { Self::default() }

    /// Decode into a caller-owned buffer. Scratch allocations are retained
    /// across packets; the output may be partially written on invalid input.
    pub fn decode_packet_into(&mut self, config: Config, packet: &[u8], output: &mut Vec<i32>) -> Result<()> {
        output.clear();
        if config.frame_length == 0 || config.frame_length > 65536 || !matches!(config.bit_depth, 16 | 20 | 24 | 32)
            || !(1..=8).contains(&config.channels) || config.kb == 0 || config.kb > 31 {
            return Err(DecodeError("unsupported ALAC configuration"));
        }
        let mut bits = Bits { data: packet, pos: 0 };
        let mut channels = 0usize;
        let mut frame_count = None;
        loop {
            let tag = bits.read(3)?;
            if tag == 7 {
                if channels != usize::from(config.channels) { return Err(DecodeError("ALAC packet has too few channels")); }
                return Ok(());
            }
            let nchan = match tag { 0 | 3 => 1usize, 1 => 2, _ => return Err(DecodeError("unsupported ALAC packet element")) };
            if channels + nchan > usize::from(config.channels) { return Err(DecodeError("ALAC packet has too many channels")); }
            let _instance = bits.read(4)?;
            if bits.read(12)? != 0 { return Err(DecodeError("nonzero ALAC reserved bits")); }
            let partial = bits.read(1)? != 0;
            let shift_bytes = bits.read(2)?;
            let escape = bits.read(1)? != 0;
            if shift_bytes == 3 || (escape && shift_bytes != 0) { return Err(DecodeError("invalid ALAC shift")); }
            let count = if partial { bits.read(32)? } else { config.frame_length };
            if (count == 0 && !escape) || count > config.frame_length || frame_count.is_some_and(|n| n != count) {
                return Err(DecodeError("invalid or inconsistent ALAC frame length"));
            }
            if frame_count.is_none() {
                frame_count = Some(count);
                output.resize(count as usize * usize::from(config.channels), 0);
            }
            let count = count as usize;
            if escape {
                for frame in 0..count {
                    for channel in 0..nchan {
                        output[frame * usize::from(config.channels) + channels + channel] = bits.signed(u32::from(config.bit_depth))?;
                    }
                }
            } else {
                let width = u32::from(config.bit_depth).checked_sub(shift_bytes * 8)
                    .and_then(|n| n.checked_add((nchan - 1) as u32))
                    .filter(|n| (1..=32).contains(n)).ok_or(DecodeError("unsupported ALAC predictor width"))?;
                let mix_shift = bits.read(8)?;
                let mix_weight = bits.read(8)? as u8 as i8 as i32;
                if mix_shift > 31 { return Err(DecodeError("invalid ALAC mix shift")); }
                let a = filter(&mut bits)?;
                let b = if nchan == 2 { Some(filter(&mut bits)?) } else { None };
                let low_bits = shift_bytes * 8;
                self.lows.clear();
                if low_bits != 0 {
                    self.lows.reserve(count * nchan);
                    for _ in 0..count * nchan { self.lows.push(bits.read(low_bits)?); }
                }
                filtered(&mut bits, count, width, config, a, &mut self.first)?;
                if let Some(f) = b { filtered(&mut bits, count, width, config, f, &mut self.second)?; }
                for frame in 0..count {
                    let mut left = self.first[frame];
                    let mut right = if nchan == 2 { self.second[frame] } else { 0 };
                    if nchan == 2 && mix_weight != 0 {
                        left = left.wrapping_add(right).wrapping_sub(mix_weight.wrapping_mul(right) >> mix_shift);
                        right = left.wrapping_sub(right);
                    }
                    let combine = |v: i32, low: u32| -> i32 {
                        sign_extend(((v as u32) << low_bits) | low, u32::from(config.bit_depth))
                    };
                    let low = |i: usize| if low_bits == 0 { 0 } else { self.lows[frame * nchan + i] };
                    let dst = frame * usize::from(config.channels) + channels;
                    output[dst] = combine(left, low(0));
                    if nchan == 2 { output[dst + 1] = combine(right, low(1)); }
                }
            }
            channels += nchan;
        }
    }
}

/// Decode one complete packet into signed, right-justified, interleaved samples.
pub fn decode_packet(config: Config, packet: &[u8]) -> Result<Vec<i32>> {
    let mut output = Vec::new();
    Decoder::new().decode_packet_into(config, packet, &mut output)?;
    Ok(output)
}

/// Decode one packet without retaining scratch for a later packet.
pub fn decode_packet_into(config: Config, packet: &[u8], output: &mut Vec<i32>) -> Result<()> {
    Decoder::new().decode_packet_into(config, packet, output)
}

/// Convert right-justified integer PCM samples to the float scale used by the
/// unresampled AudioToolbox client format. The caller supplies the ALAC depth.
pub fn samples_to_float(samples: &[i32], bit_depth: u8) -> Result<Vec<f32>> {
    let scale = match bit_depth {
        16 => 1.0 / 32768.0,
        20 => 1.0 / 524288.0,
        24 => 1.0 / 8388608.0,
        32 => 1.0 / 2147483648.0,
        _ => return Err(DecodeError("unsupported ALAC bit depth")),
    };
    Ok(samples.iter().map(|&v| (v as f32) * scale).collect())
}

/// Convert interleaved float samples to the right-justified PCM consumed by
/// the ALAC encoder. The 20-bit stream is derived from the native 24-bit
/// conversion; the packed 24-bit path has a distinct negative rounding rule.
pub fn quantize_into(input: &[f32], bit_depth: u8, output: &mut [i32]) -> Result<(), EncodeError> {
    if input.len() != output.len() { return Err(EncodeError::Invalid("ALAC PCM buffer lengths differ")); }
    if !matches!(bit_depth, 16 | 20 | 24 | 32) { return Err(EncodeError::Invalid("unsupported ALAC bit depth")); }
    for (&sample, dst) in input.iter().zip(output.iter_mut()) {
        *dst = if matches!(bit_depth, 20 | 24) {
            let biased = f64::from(sample) * 2_147_483_648.0 + 128.0;
            let value = if biased >= 2_147_483_647.0 { 8_388_607 }
                else if biased <= -2_147_483_648.0 { -8_388_608 }
                else { ((biased as i32) - i32::from(biased < 0.0)) >> 8 };
            if bit_depth == 20 { value >> 4 } else { value }
        } else {
            let scale = (1u64 << (bit_depth - 1)) as f64;
            let value = f64::from(sample) * scale;
            let rounded = if value < 0.0 { value - 0.5 } else { value + 0.5 };
            let max = (1i64 << (bit_depth - 1)) - 1;
            (rounded as i64).clamp(-max - 1, max) as i32
        };
    }
    Ok(())
}

struct BitWriter { data: Vec<u8>, pos: usize, count_only: bool }
impl BitWriter {
    fn write(&mut self, value: u32, mut width: u32) {
        if self.count_only { self.pos += width as usize; return; }
        while width != 0 {
            if self.pos % 8 == 0 { self.data.push(0); }
            let take = width.min((8 - (self.pos % 8)) as u32);
            let shift = width - take;
            let mask = (1u32 << take) - 1;
            let place = 8 - self.pos % 8 - take as usize;
            *self.data.last_mut().expect("allocated output byte") |= (((value >> shift) & mask) as u8) << place;
            self.pos += take as usize;
            width -= take;
        }
    }
    fn truncate(&mut self, pos: usize) {
        self.pos = pos;
        self.data.truncate(pos.div_ceil(8));
        if pos % 8 != 0 {
            if let Some(last) = self.data.last_mut() { *last &= 0xff << (8 - pos % 8); }
        }
    }
}

fn put_rice(bits: &mut BitWriter, n: u32, k: u32, modulus: u32, raw_width: u32, cap: usize) {
    let quotient = n / modulus;
    let rem = n % modulus;
    let encoded_width = quotient as usize + k as usize + usize::from(rem != 0);
    if quotient >= 9 || encoded_width > cap {
        bits.write(0x1ff, 9);
        bits.write(n, raw_width);
    } else {
        if quotient != 0 { bits.write((1u32 << quotient) - 1, quotient); }
        bits.write(0, 1);
        if rem == 0 {
            bits.write(0, k - 1);
        } else {
            bits.write(rem + 1, k);
        }
    }
}

fn encode_residuals(samples: &[i32], width: u32, output: &mut BitWriter) {
    let mut mean = 10u32;
    let mut bias = 0u32;
    let mut index = 0usize;
    while index < samples.len() {
        let value = samples[index];
        let mapped = if value < 0 { (value as u32).wrapping_neg().wrapping_mul(2).wrapping_sub(1) }
            else { (value as u32).wrapping_mul(2) };
        let n = mapped.wrapping_sub(bias);
        let k = (31 - ((mean >> 9) + 3).leading_zeros()).min(14);
        put_rice(output, n, k, (1 << k) - 1, width, 25);
        mean = mean.wrapping_add(40u32.wrapping_mul(mapped)).wrapping_sub(40u32.wrapping_mul(mean) >> 9);
        if n > 65535 { mean = 65535; }
        index += 1;
        bias = 0;
        if mean < 128 && index < samples.len() {
            let start = index;
            while index < samples.len() && samples[index] == 0 && index - start < 65535 { index += 1; }
            let run = (index - start) as u32;
            let k = (mean.leading_zeros() as i32 - 24 + ((mean + 16) >> 6) as i32) as u32;
            let modulus = ((1u32 << k) - 1) & ((1 << 14) - 1);
            put_rice(output, run, k, modulus, 16, 25);
            bias = u32::from(run < 65535);
            mean = 0;
        }
    }
}

/// The reference block predictor over `input[..num]`, into `out`. It always emits the
/// `coefficients.len()` differential samples after the first, even when `num` ends sooner: those
/// positions are read from `input` and written to `out` whatever the block length, so a short block
/// (a partial packet's training and scoring prefixes) rewrites `out[..=order]`, not `out[..num]`.
/// Adaptation of `coefficients` happens only for samples past the differential head.
fn predict_block(input: &[i32], out: &mut [i32], num: usize, coefficients: &mut [i16], width: u32) {
    let order = coefficients.len();
    out[0] = input[0];
    for i in 1..=order {
        out[i] = sign_extend(input[i].wrapping_sub(input[i - 1]) as u32, width);
    }
    for i in order + 1..num {
        let base = input[i - order - 1];
        let mut sum = 256i32;
        for (j, &c) in coefficients.iter().enumerate() {
            sum = sum.wrapping_add(i32::from(c).wrapping_mul(input[i - j - 1].wrapping_sub(base)));
        }
        let prediction = base.wrapping_add(sum >> 9);
        let error = sign_extend(input[i].wrapping_sub(prediction) as u32, width);
        out[i] = error;
        let direction = error.signum();
        if direction != 0 {
            let mut remaining = error;
            for j in (0..order).rev() {
                let difference = base.wrapping_sub(input[i - j - 1]);
                let sign = difference.signum().wrapping_mul(direction);
                coefficients[j] = coefficients[j].wrapping_sub(sign as i16);
                remaining = remaining.wrapping_sub(((order - j) as i32).wrapping_mul(sign.wrapping_mul(difference) >> 9));
                if (direction > 0 && remaining <= 0) || (direction < 0 && remaining >= 0) { break; }
            }
        }
    }
}

fn encoded_residual_bits(samples: &[i32], width: u32) -> usize {
    let mut counter = BitWriter { data: Vec::new(), pos: 0, count_only: true };
    encode_residuals(samples, width, &mut counter);
    counter.pos
}

/// The reference encoder's four work buffers: mixed samples and predictor output for the first and
/// second channel of an element. The order search scores the first eighth of the predictor output
/// after rewriting only its first thirty-second and the differential head, so within an element the
/// part in between is what the mix search left there. Every position that is scored is written by
/// the element itself: nothing is read from an earlier element or packet.
struct EncoderScratch {
    u: Vec<i32>,
    v: Vec<i32>,
    pu: Vec<i32>,
    pv: Vec<i32>,
}

impl EncoderScratch {
    /// `frame_length` samples plus the span of the longest predictor's differential head.
    fn new(frame_length: usize) -> Self {
        let length = frame_length + 16;
        Self { u: vec![0; length], v: vec![0; length], pu: vec![0; length], pv: vec![0; length] }
    }
}

/// What AudioToolbox keeps over every encode attempt of a stream, the attempts whose packet it then
/// rejected included. The cookie's `maxFrameBytes` is the largest attempt and its `avgBitRate` follows
/// from the mean attempt size over a nominal 4096 frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttemptStats { pub attempts: u64, pub bytes: u64, pub max_bytes: u32 }
impl AttemptStats {
    /// The statistics of a stream in which every attempt was accepted.
    pub fn of_packets(sizes: impl IntoIterator<Item = u32>) -> Self {
        let mut stats = Self::default();
        for size in sizes { stats.record(size as usize); }
        stats
    }
    fn record(&mut self, size: usize) {
        self.attempts += 1;
        self.bytes += size as u64;
        self.max_bytes = self.max_bytes.max(u32::try_from(size).unwrap_or(u32::MAX));
    }
}

/// A failure of [`Stream`]: its own refusal, or one of the caller's packet sink.
#[derive(Debug)]
pub enum StreamError<E> { Encode(EncodeError), Sink(E) }

/// The packets AudioToolbox writes for an ALAC stream delivered in separate write calls.
///
/// A packet is encoded once its 4096th frame is in. When the result is larger than the converter's
/// output buffer AudioToolbox neither writes nor consumes it: the audio stays queued, the predictor
/// coefficients keep what the attempt did to them, and the packet is encoded again, once per later
/// write call and once when the file is closed. Measured against AudioToolbox on macOS (write calls
/// of 1 to 9000 frames, 1 to 8 channels, 16 to 32 bits):
///
/// * attempts 0 and 1 of a packet that is still queued leave the audio of their write calls queued;
///   a call that is still unable to place the packet at its third attempt or later has its audio
///   discarded, and the file then decodes to fewer frames than were written;
/// * a packet that cannot be placed by the close of the file is not written, nor is any packet after it.
///
/// Both end in a refusal here ([`EncodeError::NativeFrameLoss`], [`EncodeError::NativePacketLimit`]),
/// where the macOS writer refuses at its own post-write check. A packet that is placed has the bytes of
/// the attempt that placed it whatever the schedule, and the cookie statistics count every attempt.
/// The schedule is exact for write calls that divide the packet length (512-frame calls do); the engine's first call of a
/// span is `512 - (limiter latency mod 512)` frames and its last is cut to the target, so real runs can host more retry
/// attempts than modelled, which at worst turns a file AudioToolbox would write into a refusal here, never a different file.
pub struct Stream {
    encoder: Encoder,
    channels: usize,
    capacity: usize,
    /// Interleaved audio from `head`, the first frame no accepted packet contains.
    pending: Vec<i32>,
    head: usize,
    /// Failed attempts of the packet at `head`, and the size of the first of them.
    failures: u32,
    first_oversize: usize,
    /// Whether a packet already failed an attempt during the current write call.
    failed_in_call: bool,
    frames: u64,
    stats: AttemptStats,
    packet: Vec<u8>,
    ended: bool,
}

impl Stream {
    pub fn new(config: Config) -> Result<Self, EncodeError> {
        let encoder = Encoder::new(config)?;
        let capacity = encoder.capacity();
        Ok(Self { encoder, channels: usize::from(config.channels), capacity, pending: Vec::new(), head: 0, failures: 0,
            first_oversize: 0, failed_in_call: false, frames: 0, stats: AttemptStats::default(), packet: Vec::new(), ended: false })
    }

    /// Start a write call: the audio of the `push` calls that follow belongs to one `ExtAudioFileWrite`.
    pub fn begin_call(&mut self) { self.failed_in_call = false; }

    /// One write call: `begin_call` and a single `push`. Empty calls do not exist for AudioToolbox.
    pub fn write<E>(&mut self, pcm: &[i32], emit: &mut impl FnMut(&[u8], u32) -> std::result::Result<(), E>) -> std::result::Result<(), StreamError<E>> {
        self.begin_call();
        self.push(pcm, emit)
    }

    /// Add interleaved right-justified PCM to the current write call and emit every packet it places,
    /// as `emit(packet bytes, frames)` in stream order.
    pub fn push<E>(&mut self, pcm: &[i32], emit: &mut impl FnMut(&[u8], u32) -> std::result::Result<(), E>) -> std::result::Result<(), StreamError<E>> {
        if self.ended || self.encoder.failed { return Err(StreamError::Encode(EncodeError::Poisoned)); }
        self.encoder.check_pcm(pcm).map_err(StreamError::Encode)?;
        self.frames += (pcm.len() / self.channels) as u64;
        self.pending.extend_from_slice(pcm);
        let packet = 4096 * self.channels;
        while self.pending.len() - self.head >= packet && !self.failed_in_call {
            let mut out = std::mem::take(&mut self.packet);
            out = self.encoder.attempt(&self.pending[self.head..self.head + packet], out);
            self.stats.record(out.len());
            let accepted = out.len() <= self.capacity;
            if accepted {
                let sent = emit(&out, 4096);
                self.packet = out;
                self.head += packet;
                self.failures = 0;
                if let Err(error) = sent { self.encoder.failed = true; return Err(StreamError::Sink(error)); }
            } else {
                let size = out.len();
                self.packet = out;
                if self.failures == 0 { self.first_oversize = size; }
                if self.failures >= 2 {
                    self.encoder.failed = true;
                    return Err(StreamError::Encode(EncodeError::NativeFrameLoss { attempts: self.failures + 1, capacity: self.capacity }));
                }
                self.failures += 1;
                self.failed_in_call = true;
            }
        }
        if self.head > 0 { self.pending.drain(..self.head); self.head = 0; }
        Ok(())
    }

    /// Close the file: encode what is left, the finalization packet after an exact multiple of the
    /// packet length included, and return the statistics for the cookie.
    pub fn finish<E>(&mut self, emit: &mut impl FnMut(&[u8], u32) -> std::result::Result<(), E>) -> std::result::Result<AttemptStats, StreamError<E>> {
        if self.ended || self.encoder.failed { return Err(StreamError::Encode(EncodeError::Poisoned)); }
        self.ended = true;
        if self.frames == 0 { return Ok(self.stats); }
        loop {
            let frames = ((self.pending.len() - self.head) / self.channels).min(4096);
            if frames == 0 && self.frames % 4096 != 0 { break; }
            let end = self.head + frames * self.channels;
            let mut out = std::mem::take(&mut self.packet);
            out = self.encoder.attempt(&self.pending[self.head..end], out);
            self.stats.record(out.len());
            if out.len() > self.capacity {
                self.encoder.failed = true;
                let first_bytes = if self.failures == 0 { out.len() } else { self.first_oversize };
                return Err(StreamError::Encode(EncodeError::NativePacketLimit { first_bytes, retry_bytes: out.len(), capacity: self.capacity }));
            }
            let sent = emit(&out, frames as u32);
            self.packet = out;
            self.head = end;
            self.failures = 0;
            if let Err(error) = sent { self.encoder.failed = true; return Err(StreamError::Sink(error)); }
            if frames == 0 { break; }
        }
        Ok(self.stats)
    }
}

/// Stateful packet encoder. Predictor coefficients are deliberately retained
/// across calls; constructing a new encoder for each packet changes bytes.
pub struct Encoder {
    config: Config,
    left: [[[i16; 8]; 2]; 8],
    right: [[[i16; 8]; 2]; 8],
    failed: bool,
    scratch: EncoderScratch,
}

impl Encoder {
    pub fn new(config: Config) -> Result<Self, EncodeError> {
        if config.frame_length != 4096 || !matches!(config.bit_depth, 16 | 20 | 24 | 32)
            || !(1..=8).contains(&config.channels) || config.pb != 40 || config.mb != 10 || config.kb != 14 {
            return Err(EncodeError::Invalid("unsupported ALAC encoder configuration"));
        }
        let initial = [1216, -928, -64, 0, 0, 0, 0, 0];
        Ok(Self { config, left: [[initial; 2]; 8], right: [[initial; 2]; 8], failed: false, scratch: EncoderScratch::new(config.frame_length as usize) })
    }

    /// Encode interleaved, signed right-justified PCM. An empty packet is the
    /// finalization packet AudioToolbox writes after an exact full frame; it is
    /// distinct from finalizing a stream to which no samples were ever written.
    pub fn encode_packet(&mut self, samples: &[i32]) -> Result<Vec<u8>, EncodeError> {
        let mut output = Vec::new();
        self.encode_packet_into(samples, &mut output)?;
        Ok(output)
    }

    /// Encode into a caller-owned buffer, reusing it across retries and packets.
    /// Invalid inputs leave the buffer unchanged; a failed size retry clears it.
    pub fn encode_packet_into(&mut self, samples: &[i32], output: &mut Vec<u8>) -> Result<(), EncodeError> {
        if self.failed { return Err(EncodeError::Poisoned); }
        self.check(samples)?;
        // A stream written in one call and then closed: ExtAudioFile encodes the packet when the call's
        // audio completes it and once more when the file is closed, with the predictor state the first
        // attempt left behind. `Stream` models the longer schedules of several write calls.
        let capacity = self.capacity();
        let first = self.attempt(samples, std::mem::take(output));
        if first.len() <= capacity { *output = first; return Ok(()); }
        let first_bytes = first.len();
        let retry = self.attempt(samples, first);
        let retry_bytes = retry.len();
        *output = retry;
        if retry_bytes <= capacity { return Ok(()); }
        output.clear();
        self.failed = true;
        Err(EncodeError::NativePacketLimit { first_bytes, retry_bytes, capacity })
    }

    /// Whole interleaved frames, at most one packet, each sample within the bit depth.
    fn check(&self, samples: &[i32]) -> Result<(), EncodeError> {
        if samples.len() / usize::from(self.config.channels) > self.config.frame_length as usize {
            return Err(EncodeError::Invalid("invalid ALAC packet sample count"));
        }
        self.check_pcm(samples)
    }

    /// Whole interleaved frames, each sample within the bit depth.
    fn check_pcm(&self, samples: &[i32]) -> Result<(), EncodeError> {
        if samples.len() % usize::from(self.config.channels) != 0 {
            return Err(EncodeError::Invalid("invalid ALAC packet sample count"));
        }
        if self.config.bit_depth < 32 {
            let bound = 1i64 << (self.config.bit_depth - 1);
            if samples.iter().any(|&s| i64::from(s) < -bound || i64::from(s) >= bound) {
                return Err(EncodeError::Invalid("ALAC sample exceeds bit depth"));
            }
        }
        Ok(())
    }

    /// The largest packet ExtAudioFile accepts: its output buffer starts at 64 KiB, or at the converter's
    /// reported maximum packet size if larger.
    fn capacity(&self) -> usize {
        (4096 * usize::from(self.config.channels) * usize::from(self.config.bit_depth).div_ceil(8) + 8).max(65536)
    }

    /// One encode attempt. A packet larger than `capacity()` is not accepted by the caller, but the
    /// attempt has changed the predictor coefficients all the same, and the next attempt starts from them.
    fn attempt(&mut self, samples: &[i32], mut reuse: Vec<u8>) -> Vec<u8> {
        reuse.clear();
        let channels = usize::from(self.config.channels);
        let frames = samples.len() / channels;
        let maps: [&[usize]; 8] = [&[1], &[2], &[1, 2], &[1, 2, 1], &[1, 2, 2],
            &[1, 2, 2, 1], &[1, 2, 2, 1, 1], &[1, 2, 2, 2, 1]];
        let mut bits = BitWriter { data: reuse, pos: 0, count_only: false };
        let mut channel = 0usize;
        let mut mono_tag = 0u32;
        let mut pair_tag = 0u32;
        for &nchan in maps[channels - 1] {
            bits.write(if nchan == 1 { 0 } else { 1 }, 3);
            if nchan == 1 { bits.write(mono_tag, 4); mono_tag += 1; }
            else { bits.write(pair_tag, 4); pair_tag += 1; }
            self.encode_element(&mut bits, samples, frames, channels, channel, nchan);
            channel += nchan;
        }
        bits.write(7, 3);
        bits.data
    }

    fn encode_element(&mut self, out: &mut BitWriter, samples: &[i32], frames: usize, stride: usize, channel: usize, nchan: usize) {
        let start = out.pos;
        let depth = u32::from(self.config.bit_depth);
        let shift = if depth == 32 { 16 } else if depth >= 24 { 8 } else { 0 };
        let width = depth - shift + (nchan - 1) as u32;
        let partial = frames != self.config.frame_length as usize;
        // The search scores a prefix of one eighth of the packet and trains on one thirty-second.
        let (n8, n32) = (frames / 8, frames / 32);
        let EncoderScratch { u, v, pu, pv } = &mut self.scratch;
        let mix = |res: u8, count: usize, a: &mut [i32], b: &mut [i32]| {
            for (i, frame) in samples.chunks_exact(stride).take(count).enumerate() {
                let l = frame[channel] >> shift;
                let r = frame[channel + 1] >> shift;
                if res == 0 { a[i] = l; b[i] = r; }
                else { a[i] = (i32::from(res) * l + (4 - i32::from(res)) * r) >> 2; b[i] = l - r; }
            }
        };
        let mut best_mix = 0u8;
        if nchan == 2 {
            let mut best = usize::MAX;
            for res in 0..=4 {
                mix(res, n8, u, v);
                predict_block(u, pu, n8, &mut self.left[channel][1], width);
                predict_block(v, pv, n8, &mut self.right[channel][1], width);
                let cost = encoded_residual_bits(&pu[..n8], width) + encoded_residual_bits(&pv[..n8], width);
                if cost < best { best = cost; best_mix = res; }
            }
            mix(best_mix, frames, u, v);
        } else {
            for (slot, frame) in u.iter_mut().zip(samples.chunks_exact(stride)) { *slot = frame[channel] >> shift; }
        }
        let mut best_u = 4usize;
        let mut best_v = 4usize;
        let mut cost_u = usize::MAX;
        let mut cost_v = usize::MAX;
        for (index, order) in [4usize, 8].into_iter().enumerate() {
            let rounds = if nchan == 1 { 7 } else { 8 };
            for _ in 0..rounds {
                predict_block(u, pu, n32, &mut self.left[channel][index][..order], width);
                if nchan == 2 { predict_block(v, pv, n32, &mut self.right[channel][index][..order], width); }
            }
            // A stereo element scores the eighth-packet prefix without a final pass: the part of it
            // beyond the trained thirty-second is whatever the buffers already hold.
            if nchan == 1 { predict_block(u, pu, n8, &mut self.left[channel][index][..order], width); }
            let bits_u = encoded_residual_bits(&pu[..n8], width) * 8 + 16 * order;
            if bits_u < cost_u { cost_u = bits_u; best_u = order; }
            if nchan == 2 {
                let bits_v = encoded_residual_bits(&pv[..n8], width) * 8 + 16 * order;
                if bits_v < cost_v { cost_v = bits_v; best_v = order; }
            }
        }
        let escape_size = frames * depth as usize * nchan + usize::from(partial) * 32 + 16;
        let estimated = cost_u + if nchan == 2 { cost_v + 64 } else { 32 }
            + usize::from(partial) * 32 + frames * shift as usize * nchan;
        if estimated < escape_size {
            out.write(0, 12);
            out.write((u32::from(partial) << 3) | (shift / 8 << 1), 4);
            if partial { out.write(frames as u32, 32); }
            out.write(if nchan == 2 { 2 } else { 0 }, 8);
            out.write(u32::from(best_mix), 8);
            for (order, coefficients) in [(best_u, &self.left[channel][usize::from(best_u == 8)]),
                (best_v, &self.right[channel][usize::from(best_v == 8)])].into_iter().take(nchan) {
                out.write(9, 8);
                out.write(0x80 | order as u32, 8);
                for &c in &coefficients[..order] { out.write(c as u16 as u32, 16); }
            }
            if shift != 0 {
                let mask = (1u32 << shift) - 1;
                for frame in samples.chunks_exact(stride) {
                    for c in 0..nchan { out.write(frame[channel + c] as u32 & mask, shift); }
                }
            }
            predict_block(u, pu, frames, &mut self.left[channel][usize::from(best_u == 8)][..best_u], width);
            encode_residuals(&pu[..frames], width, out);
            if nchan == 2 {
                predict_block(v, pv, frames, &mut self.right[channel][usize::from(best_v == 8)][..best_v], width);
                encode_residuals(&pv[..frames], width, out);
            }
            if out.pos - start < escape_size { return; }
            out.truncate(start);
        }
        out.write(0, 12);
        out.write((u32::from(partial) << 3) | 1, 4);
        if partial { out.write(frames as u32, 32); }
        for frame in samples.chunks_exact(stride) {
            for c in 0..nchan { out.write(frame[channel + c] as u32, depth); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIAL: [i16; 4] = [1216, -928, -64, 0];

    #[test]
    fn short_block_still_writes_the_differential_head() {
        let input: Vec<i32> = (0..16).map(|i| 3 * i * i - 40).collect();
        let mut out = [i32::MIN; 16];
        let mut coefficients = INITIAL;
        // Two samples are asked for; the four differences after the first are produced anyway.
        predict_block(&input, &mut out, 2, &mut coefficients, 16);
        assert_eq!(out[..5], [input[0], 3, 9, 15, 21]);
        assert!(out[5..].iter().all(|&v| v == i32::MIN), "nothing past the differential head is touched");
        assert_eq!(coefficients, INITIAL, "no sample was adapted on");
    }

    #[test]
    fn empty_block_writes_the_first_sample_and_the_head() {
        let input: Vec<i32> = (0..8).map(|i| 100 - 7 * i).collect();
        let mut out = [0; 8];
        let mut coefficients = INITIAL;
        predict_block(&input, &mut out, 0, &mut coefficients, 16);
        assert_eq!(out[..5], [100, -7, -7, -7, -7]);
        assert_eq!(coefficients, INITIAL);
    }

    #[test]
    fn adaptation_starts_after_the_differential_head() {
        let input: Vec<i32> = (0..8).map(|i| 10 * i).collect();
        let mut out = [i32::MIN; 8];
        let mut coefficients = INITIAL;
        predict_block(&input, &mut out, 5, &mut coefficients, 16);
        assert_eq!(coefficients, INITIAL, "five samples end with the head");
        assert_eq!(out[5], i32::MIN);
        predict_block(&input, &mut out, 6, &mut coefficients, 16);
        // The prediction from the initial weights is 0 + (256 + 1216 * 40 - 928 * 30 - 64 * 20) / 512 = 38.
        assert_eq!(out[5], 50 - 38);
        assert_ne!(coefficients, INITIAL, "a nonzero error adapts the weights");
        assert_eq!(out[6], i32::MIN);
    }

    #[test]
    fn scoring_never_reads_stale_buffers() {
        // Short packets score positions beyond the frames they train on, from buffers shared with
        // earlier elements. Pre-filling every buffer with junk must change nothing.
        let config = Config { frame_length: 4096, bit_depth: 16, pb: 40, mb: 10, kb: 14, channels: 5, sample_rate: 44_100 };
        let mut clean = Encoder::new(config).unwrap();
        let mut dirty = Encoder::new(config).unwrap();
        let mut state = 0x2545_f491u32;
        let mut sample = |amp: i32| { state ^= state << 13; state ^= state >> 17; state ^= state << 5; (state % (2 * amp as u32 + 1)) as i32 - amp };
        for (frames, amp) in [(4096, 900), (13, 30), (27, 3000), (40, 7), (71, 12000), (150, 90), (287, 5), (9, 1000), (4096, 20), (31, 400)] {
            let pcm: Vec<i32> = (0..frames * 5).map(|_| sample(amp)).collect();
            let scratch = &mut dirty.scratch;
            for buffer in [&mut scratch.u, &mut scratch.v, &mut scratch.pu, &mut scratch.pv] {
                for value in buffer.iter_mut() { *value = sample(1 << 20); }
            }
            assert_eq!(clean.encode_packet(&pcm).unwrap(), dirty.encode_packet(&pcm).unwrap(), "{frames} frames of amplitude {amp}");
        }
    }
    fn noise(channels: usize, frames: usize, amp: i32, seed: u32) -> Vec<i32> {
        let mut state = seed | 1;
        (0..frames * channels).map(|_| { state ^= state << 13; state ^= state >> 17; state ^= state << 5; (state % (2 * amp as u32 + 1)) as i32 - amp }).collect()
    }

    fn collect(packets: &mut Vec<(Vec<u8>, u32)>) -> impl FnMut(&[u8], u32) -> std::result::Result<(), ()> + '_ {
        move |bytes, frames| { packets.push((bytes.to_vec(), frames)); Ok(()) }
    }

    /// 4-channel 24-bit-in-32 noise whose first packet needs `failures` attempts before it fits.
    fn oversized(config: Config, failures: u32) -> Vec<i32> {
        let channels = usize::from(config.channels);
        let capacity = Encoder::new(config).unwrap().capacity();
        for amp in (2_000_000..6_000_000).step_by(7_919) {
            let pcm: Vec<i32> = noise(channels, 4096, amp, 77).iter().map(|v| v << 8).collect();
            let mut encoder = Encoder::new(config).unwrap();
            let mut tried = 0;
            while encoder.attempt(&pcm, Vec::new()).len() > capacity && tried <= failures { tried += 1; }
            if tried == failures { return pcm; }
        }
        panic!("no noise level needing {failures} failed attempts");
    }

    fn wide() -> Config { Config { frame_length: 4096, bit_depth: 32, pb: 40, mb: 10, kb: 14, channels: 4, sample_rate: 44_100 } }

    #[test]
    fn stream_without_oversized_packets_is_packetwise_encoding() {
        let config = Config { frame_length: 4096, bit_depth: 16, pb: 40, mb: 10, kb: 14, channels: 2, sample_rate: 44_100 };
        let pcm = noise(2, 4096 * 2 + 700, 3000, 5);
        let mut reference = Encoder::new(config).unwrap();
        let expected: Vec<Vec<u8>> = pcm.chunks(2 * 4096).map(|c| reference.encode_packet(c).unwrap()).collect();
        for calls in [vec![512usize], vec![100, 4000, 1], vec![9000], vec![1, 4095]] {
            let mut stream = Stream::new(config).unwrap();
            let mut packets = Vec::new();
            let (mut position, mut call) = (0, 0);
            while position < pcm.len() / 2 {
                let take = calls[call % calls.len()].min(pcm.len() / 2 - position);
                stream.write(&pcm[position * 2..(position + take) * 2], &mut collect(&mut packets)).unwrap();
                position += take;
                call += 1;
            }
            let stats = stream.finish(&mut collect(&mut packets)).unwrap();
            assert_eq!(packets.iter().map(|p| p.0.clone()).collect::<Vec<_>>(), expected, "calls {calls:?}");
            assert_eq!(packets.iter().map(|p| p.1).collect::<Vec<_>>(), [4096, 4096, 700]);
            assert_eq!(stats, AttemptStats::of_packets(expected.iter().map(|p| p.len() as u32)));
        }
        // An exact multiple of the packet length ends with the empty finalization packet, which counts.
        let mut stream = Stream::new(config).unwrap();
        let mut packets = Vec::new();
        stream.write(&pcm[..2 * 4096], &mut collect(&mut packets)).unwrap();
        let stats = stream.finish(&mut collect(&mut packets)).unwrap();
        assert_eq!(packets.iter().map(|p| p.1).collect::<Vec<_>>(), [4096, 0]);
        assert_eq!(stats.attempts, 2);
        // A stream to which nothing was written has no packets and no statistics.
        let mut stream = Stream::new(config).unwrap();
        assert_eq!(stream.finish(&mut collect(&mut packets)).unwrap(), AttemptStats::default());
    }

    #[test]
    fn oversized_packet_is_retried_once_per_write_call_and_counted() {
        let config = wide();
        let pcm = oversized(config, 1);
        let mut stream = Stream::new(config).unwrap();
        let mut packets = Vec::new();
        for call in pcm.chunks(4 * 512) { stream.write(call, &mut collect(&mut packets)).unwrap(); }
        assert!(packets.is_empty(), "the packet waits for its retry");
        let stats = stream.finish(&mut collect(&mut packets)).unwrap();
        // Two attempts, the first of which was rejected, and then the finalization packet.
        let mut encoder = Encoder::new(config).unwrap();
        let first = encoder.attempt(&pcm, Vec::new());
        let second = encoder.attempt(&pcm, Vec::new());
        assert!(first.len() > encoder.capacity() && second.len() <= encoder.capacity());
        assert_eq!(packets[0], (second.clone(), 4096));
        assert_eq!(decode_packet(config, &packets[0].0).unwrap(), pcm);
        assert_eq!(packets.len(), 2);
        let empty = packets[1].0.len();
        assert_eq!(stats, AttemptStats { attempts: 3, bytes: (first.len() + second.len() + empty) as u64, max_bytes: first.len() as u32 });
    }

    #[test]
    fn a_third_attempt_needs_a_write_call_after_the_second() {
        let config = wide();
        let pcm = oversized(config, 2);
        // One call, then close: the close is the only retry, and the packet still does not fit.
        let mut stream = Stream::new(config).unwrap();
        let mut packets = Vec::new();
        stream.write(&pcm, &mut collect(&mut packets)).unwrap();
        let error = stream.finish(&mut collect(&mut packets));
        assert!(matches!(error, Err(StreamError::Encode(EncodeError::NativePacketLimit { capacity: 65544, .. }))), "{error:?}");
        assert!(matches!(stream.write(&pcm[..4], &mut collect(&mut packets)), Err(StreamError::Encode(EncodeError::Poisoned))));
        // A later call hosts the second attempt and the close the third.
        let mut stream = Stream::new(config).unwrap();
        let mut packets = Vec::new();
        stream.write(&pcm, &mut collect(&mut packets)).unwrap();
        stream.write(&noise(4, 13, 1000, 3), &mut collect(&mut packets)).unwrap();
        assert!(packets.is_empty());
        let stats = stream.finish(&mut collect(&mut packets)).unwrap();
        assert_eq!(packets.iter().map(|p| p.1).collect::<Vec<_>>(), [4096, 13]);
        assert_eq!(decode_packet(config, &packets[0].0).unwrap(), pcm);
        // Three attempts at the first packet, then the 13 frames.
        assert_eq!(stats.attempts, 4);
        assert_eq!(stats.max_bytes, 65545);
    }

    #[test]
    fn audio_written_while_a_packet_is_still_rejected_at_its_third_attempt_is_a_refusal() {
        let config = wide();
        let pcm = oversized(config, 3);
        let mut stream = Stream::new(config).unwrap();
        let mut packets = Vec::new();
        for (n, call) in pcm.chunks(4 * 512).enumerate() {
            assert!(stream.write(call, &mut collect(&mut packets)).is_ok(), "completing call {n}");
        }
        // The call after the completing one hosts the second attempt, the one after it the third.
        stream.write(&noise(4, 512, 1000, 3), &mut collect(&mut packets)).unwrap();
        let result = stream.write(&noise(4, 512, 1000, 3), &mut collect(&mut packets));
        assert!(matches!(result, Err(StreamError::Encode(EncodeError::NativeFrameLoss { attempts: 3, capacity: 65544 }))), "{result:?}");
        assert!(matches!(stream.finish(&mut collect(&mut packets)), Err(StreamError::Encode(EncodeError::Poisoned))));
        assert!(packets.is_empty());
    }

    #[test]
    fn a_failing_sink_ends_the_stream() {
        let config = Config { frame_length: 4096, bit_depth: 16, pb: 40, mb: 10, kb: 14, channels: 1, sample_rate: 44_100 };
        let mut stream = Stream::new(config).unwrap();
        let pcm = noise(1, 4096, 100, 9);
        assert!(matches!(stream.write(&pcm, &mut |_: &[u8], _| Err("disk full")), Err(StreamError::Sink("disk full"))));
        assert!(matches!(stream.finish(&mut |_: &[u8], _| Ok::<(), &str>(())), Err(StreamError::Encode(EncodeError::Poisoned))));
        let mut stream = Stream::new(config).unwrap();
        assert!(matches!(stream.write(&[40_000], &mut |_: &[u8], _| Ok::<(), ()>(())), Err(StreamError::Encode(EncodeError::Invalid(_)))));
    }
}
