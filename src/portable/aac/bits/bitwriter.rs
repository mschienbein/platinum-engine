/// MSB-first bit sink (`_WriteBits` 0003dd5c: keeps the low `n` bits of the value).
#[derive(Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }
    /// Append the low `bits` bits (0..=32) of `value`.
    #[inline]
    pub fn write(&mut self, value: u32, bits: u32) {
        debug_assert!(bits <= 32);
        if bits == 0 {
            return;
        }
        let v = u64::from(value) & ((1u64 << bits) - 1);
        self.acc = (self.acc << bits) | v;
        self.n += bits;
        while self.n >= 8 {
            self.bytes.push((self.acc >> (self.n - 8)) as u8);
            self.n -= 8;
        }
        self.acc &= (1u64 << self.n) - 1;
    }
    /// Number of bits written so far.
    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8 + self.n as usize
    }
    /// Zero bits up to the next byte boundary.
    pub fn align(&mut self) {
        let pad = (8 - (self.n & 7)) & 7;
        self.write(0, pad);
    }
    /// Pad with zero bits to a byte boundary and return the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.align();
        self.bytes
    }
}
