//! Minimal MSB-first bit reader for AudioSpecificConfig / PCE / ADTS / LATM parsing.

pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn bit_pos(&self) -> usize {
        self.pos
    }
    pub fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }
    /// Reads `n <= 32` bits; `None` when fewer bits remain.
    pub fn read(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if self.bits_left() < n as usize {
            return None;
        }
        let mut v: u64 = 0;
        for _ in 0..n {
            let byte = self.data[self.pos >> 3];
            v = (v << 1) | u64::from((byte >> (7 - (self.pos & 7))) & 1);
            self.pos += 1;
        }
        Some(v as u32)
    }
    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.bits_left() < n {
            return None;
        }
        self.pos += n;
        Some(())
    }
    /// Advances to the next byte boundary (relative to the start of the slice).
    pub fn align(&mut self) -> Option<()> {
        let r = (8 - (self.pos & 7)) & 7;
        self.skip(r)
    }
}

impl BitReader<'_> {
    /// Copies up to `max_bits` bits starting at the current position into a fresh byte-aligned buffer (zero padded at the end).
    pub fn peek_aligned(&self, max_bits: usize) -> Vec<u8> {
        let n = max_bits.min(self.bits_left());
        let bytes = n.div_ceil(8);
        let (first, shift) = (self.pos >> 3, (self.pos & 7) as u32);
        let mut out: Vec<u8> = (0..bytes)
            .map(|i| {
                let hi = self.data[first + i];
                if shift == 0 { hi } else { (hi << shift) | (self.data.get(first + i + 1).copied().unwrap_or(0) >> (8 - shift)) }
            })
            .collect();
        if n % 8 != 0 {
            out[bytes - 1] &= 0xffu8 << (8 - n % 8); // zero padding behind the last bit
        }
        out
    }
}
