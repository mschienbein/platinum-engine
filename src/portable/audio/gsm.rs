//! GSM 06.10 synthesis for Microsoft WAV49 blocks.
//!
//! Arithmetic follows ETSI 300 961 section 5.3; WAV49 concatenates two
//! 260-bit frames with least-significant bit first and no intervening padding.

const INVERSE_A: [i32; 8] = [13107, 13107, 13107, 13107, 19223, 17476, 31454, 29708];
const B: [i32; 8] = [0, 0, 2048, -2560, 94, -1792, -341, -1144];
const MIN_LAR: [i32; 8] = [-32, -32, -16, -16, -8, -8, -4, -4];
const LAR_BITS: [u32; 8] = [6, 6, 5, 5, 4, 4, 3, 3];
const GAIN: [i32; 4] = [3277, 11469, 21299, 32767];
const FACTOR: [i32; 8] = [18431, 20479, 22527, 24575, 26623, 28671, 30719, 32767];

#[inline]
fn sat(x: i32) -> i32 { x.clamp(-32768, 32767) }
#[inline]
fn add(x: i32, y: i32) -> i32 { sat(x + y) }
#[inline]
fn sub(x: i32, y: i32) -> i32 { sat(x - y) }
#[inline]
fn mul_r(x: i32, y: i32) -> i32 { sat((x * y + 16384) >> 15) }

struct Bits<'a> { bytes: &'a [u8; 65], position: usize }
impl Bits<'_> {
    #[inline]
    fn take(&mut self, count: u32) -> i32 {
        let byte = self.position >> 3;
        let shift = self.position & 7;
        let mut value = u32::from(self.bytes[byte]) >> shift;
        if shift + count as usize > 8 {
            value |= u32::from(self.bytes[byte + 1]) << (8 - shift);
        }
        self.position += count as usize;
        (value & ((1 << count) - 1)) as i32
    }
}

/// A stateful decoder. Every complete 65-byte WAV49 packet produces 320
/// mono samples; retain the state between packets and reset it on a new stream.
#[derive(Clone)]
pub struct GsmDecoder {
    // The first 120 samples are history; the remaining 160 hold the
    // reconstructed residual for the current frame.
    residual: [i32; 280],
    lag: usize,
    previous_lar: [i32; 8],
    synthesis: [i32; 9],
    deemphasis: i32,
}
impl Default for GsmDecoder {
    fn default() -> Self {
        Self { residual: [0; 280], lag: 40, previous_lar: [0; 8], synthesis: [0; 9], deemphasis: 0 }
    }
}
impl GsmDecoder {
    pub fn new() -> Self { Self::default() }

    pub fn decode(&mut self, block: &[u8; 65], output: &mut [f32; 320]) {
        let mut bits = Bits { bytes: block, position: 0 };
        self.frame(&mut bits, &mut output[..160]);
        self.frame(&mut bits, &mut output[160..]);
    }

    fn frame(&mut self, bits: &mut Bits<'_>, output: &mut [f32]) {
        let mut lar = [0; 8];
        for i in 0..8 {
            let coded = bits.take(LAR_BITS[i]);
            let shifted = (coded + MIN_LAR[i]) << 10;
            let decoded = mul_r(INVERSE_A[i], sub(shifted, B[i] * 2));
            lar[i] = add(decoded, decoded);
        }
        for subframe in 0..4 {
            let raw_lag = bits.take(7) as usize;
            let gain = GAIN[bits.take(2) as usize];
            let grid = bits.take(2) as usize;
            let xmax = bits.take(6);
            let mut exp = if xmax > 15 { (xmax >> 3) - 1 } else { 0 };
            let mut mant = xmax - (exp << 3);
            if mant == 0 { exp = -4; mant = 7; }
            else {
                while mant <= 7 { mant = (mant << 1) + 1; exp -= 1; }
                mant -= 8;
            }
            let shift = (6 - exp) as u32;
            // The GSM arithmetic shift accepts negative counts: 1 << -1 is
            // therefore zero. This case occurs for xmax = 56..=63.
            let rounding = if shift == 0 { 0 } else { 1 << (shift - 1) };
            let mut pulses = [0; 40];
            for i in 0..13 {
                let code = bits.take(3);
                let scaled = (code * 2 - 7) << 12;
                let value = add(mul_r(FACTOR[mant as usize], scaled), rounding) >> shift;
                pulses[grid + 3 * i] = value;
            }
            if (40..=120).contains(&raw_lag) { self.lag = raw_lag; }
            for k in 0..40 {
                let index = 120 + subframe * 40 + k;
                let estimate = mul_r(gain, self.residual[index - self.lag]);
                self.residual[index] = add(pulses[k], estimate);
            }
        }
        for (start, end, phase) in [(0, 13, 0), (13, 27, 1), (27, 40, 2), (40, 160, 3)] {
            let mut reflection = [0; 8];
            for i in 0..8 {
                let old = self.previous_lar[i];
                let new = lar[i];
                let x = match phase {
                    0 => add(add(old >> 2, new >> 2), old >> 1),
                    1 => add(old >> 1, new >> 1),
                    2 => add(add(old >> 2, new >> 2), new >> 1),
                    _ => new,
                };
                let abs = if x == -32768 { 32767 } else { x.abs() };
                let magnitude = if abs < 11059 { abs << 1 }
                    else if abs < 20070 { add(abs, 11059) }
                    else { add(abs >> 2, 26112) };
                reflection[i] = if x < 0 { -magnitude } else { magnitude };
            }
            for k in start..end {
                let mut value = self.residual[120 + k];
                for i in (0..8).rev() {
                    value = sub(value, mul_r(reflection[i], self.synthesis[i]));
                    self.synthesis[i + 1] = add(self.synthesis[i], mul_r(reflection[i], value));
                }
                self.synthesis[0] = value;
                self.deemphasis = add(value, mul_r(self.deemphasis, 28180));
                let pcm = add(self.deemphasis, self.deemphasis) & !7;
                output[k] = pcm as f32 * (1.0 / 32768.0);
            }
        }
        self.previous_lar = lar;
        self.residual.copy_within(160..280, 0);
    }
}
