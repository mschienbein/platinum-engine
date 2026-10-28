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
    Poisoned,
}
impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) => f.write_str(s),
            Self::NativePacketLimit { first_bytes, retry_bytes, capacity } => write!(f,
                "ALAC packet exceeds AudioToolbox output capacity: first {first_bytes} bytes, retry {retry_bytes} bytes, capacity {capacity}"),
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

fn predict_for_encoding(samples: &[i32], coefficients: &mut [i16], width: u32, out: &mut Vec<i32>) {
    let order = coefficients.len();
    out.resize(samples.len(), 0);
    if samples.is_empty() { return; }
    out[0] = samples[0];
    for i in 1..samples.len().min(order + 1) {
        out[i] = sign_extend(samples[i].wrapping_sub(samples[i - 1]) as u32, width);
    }
    for i in order + 1..samples.len() {
        let base = samples[i - order - 1];
        let mut sum = 256i32;
        for (j, &c) in coefficients.iter().enumerate() {
            sum = sum.wrapping_add(i32::from(c).wrapping_mul(samples[i - j - 1].wrapping_sub(base)));
        }
        let prediction = base.wrapping_add(sum >> 9);
        let error = sign_extend(samples[i].wrapping_sub(prediction) as u32, width);
        out[i] = error;
        let direction = error.signum();
        if direction != 0 {
            let mut remaining = error;
            for j in (0..order).rev() {
                let difference = base.wrapping_sub(samples[i - j - 1]);
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

#[derive(Default)]
struct EncoderScratch {
    u: Vec<i32>,
    v: Vec<i32>,
    predicted_u: Vec<i32>,
    predicted_v: Vec<i32>,
    previous_u: Vec<i32>,
    previous_v: Vec<i32>,
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
        Ok(Self { config, left: [[initial; 2]; 8], right: [[initial; 2]; 8], failed: false, scratch: EncoderScratch::default() })
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
        let channels = usize::from(self.config.channels);
        if samples.len() % channels != 0 || samples.len() / channels > self.config.frame_length as usize {
            return Err(EncodeError::Invalid("invalid ALAC packet sample count"));
        }
        if self.config.bit_depth < 32 {
            let bound = 1i64 << (self.config.bit_depth - 1);
            if samples.iter().any(|&s| i64::from(s) < -bound || i64::from(s) >= bound) {
                return Err(EncodeError::Invalid("ALAC sample exceeds bit depth"));
            }
        }
        // ExtAudioFile starts with at least 64 KiB, or the converter's
        // reported maximum packet size if larger. It retries one oversized
        // packet with the already-mutated predictor state before dropping it.
        let capacity = (4096 * channels * usize::from(self.config.bit_depth).div_ceil(8) + 8).max(65536);
        let first = self.encode_once(samples, std::mem::take(output));
        if first.len() <= capacity { *output = first; return Ok(()); }
        let first_bytes = first.len();
        let retry = self.encode_once(samples, first);
        let retry_bytes = retry.len();
        *output = retry;
        if retry_bytes <= capacity { return Ok(()); }
        output.clear();
        self.failed = true;
        Err(EncodeError::NativePacketLimit { first_bytes, retry_bytes, capacity })
    }

    fn encode_once(&mut self, samples: &[i32], mut reuse: Vec<u8>) -> Vec<u8> {
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
        let EncoderScratch { u, v, predicted_u, predicted_v, previous_u, previous_v } = &mut self.scratch;
        let mix = |res: u8, count: usize, a: &mut Vec<i32>, b: &mut Vec<i32>| {
            a.clear();
            b.clear();
            a.reserve(count);
            b.reserve(count);
            for frame in samples.chunks_exact(stride).take(count) {
                let l = frame[channel] >> shift;
                let r = frame[channel + 1] >> shift;
                if res == 0 { a.push(l); b.push(r); }
                else { a.push((i32::from(res) * l + (4 - i32::from(res)) * r) >> 2); b.push(l - r); }
            }
        };
        let mut best_mix = 0u8;
        if nchan == 2 {
            let mut best = usize::MAX;
            for res in 0..=4 {
                mix(res, frames / 8, u, v);
                predict_for_encoding(u, &mut self.left[channel][1], width, previous_u);
                predict_for_encoding(v, &mut self.right[channel][1], width, previous_v);
                let cost = encoded_residual_bits(previous_u, width) + encoded_residual_bits(previous_v, width);
                if cost < best { best = cost; best_mix = res; }
            }
            mix(best_mix, frames, u, v);
        } else {
            u.clear();
            u.extend(samples.chunks_exact(stride).map(|frame| frame[channel] >> shift));
            v.clear();
        }
        let mut best_u = 4usize;
        let mut best_v = 4usize;
        let mut cost_u = usize::MAX;
        let mut cost_v = usize::MAX;
        for (index, order) in [4usize, 8].into_iter().enumerate() {
            let rounds = if nchan == 1 { 7 } else { 8 };
            for _ in 0..rounds {
                predict_for_encoding(&u[..frames / 32], &mut self.left[channel][index][..order], width, predicted_u);
                if nchan == 2 {
                    predict_for_encoding(&v[..frames / 32], &mut self.right[channel][index][..order], width, predicted_v);
                }
            }
            if nchan == 1 {
                predict_for_encoding(&u[..frames / 8], &mut self.left[channel][index][..order], width, predicted_u);
            } else {
                // The search score uses the converged prefix and the scratch
                // tail left by the preceding mix search.
                predicted_u.extend_from_slice(&previous_u[predicted_u.len()..]);
                predicted_v.extend_from_slice(&previous_v[predicted_v.len()..]);
            }
            let bits_u = encoded_residual_bits(predicted_u, width) * 8 + 16 * order;
            if bits_u < cost_u { cost_u = bits_u; best_u = order; }
            if nchan == 2 {
                let bits_v = encoded_residual_bits(predicted_v, width) * 8 + 16 * order;
                if bits_v < cost_v { cost_v = bits_v; best_v = order; }
                std::mem::swap(previous_u, predicted_u);
                std::mem::swap(previous_v, predicted_v);
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
            predict_for_encoding(u, &mut self.left[channel][usize::from(best_u == 8)][..best_u], width, predicted_u);
            encode_residuals(predicted_u, width, out);
            if nchan == 2 {
                predict_for_encoding(v, &mut self.right[channel][usize::from(best_v == 8)][..best_v], width, predicted_v);
                encode_residuals(predicted_v, width, out);
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
