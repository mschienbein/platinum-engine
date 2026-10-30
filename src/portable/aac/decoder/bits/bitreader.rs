//! `TBitstreamReader<uint32>` of Apple's decoder: big-endian bit reader over one access unit.
//!
//! Past-end semantics (decisive for truncated / corrupt access units, `FillCacheFrom` 0x34d1f4): every bit beyond the last byte reads as
//! `1` (the cache is padded with `0xff`), for arbitrarily long runs.  `bits_left()` is signed and becomes negative past the end.

pub struct BitReader<'a> {
    data: &'a [u8],
    /// Bit position from the start of `data` (may exceed `8 * data.len()`).
    pos: usize,
}

impl<'a> BitReader<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The next `n` bits (0..=32) without consuming them; bytes past the end are `0xff`.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let word = if byte + 8 <= self.data.len() {
            u64::from_be_bytes(self.data[byte..byte + 8].try_into().unwrap())
        } else {
            let mut w = 0u64;
            for i in 0..8 {
                let b = if byte + i < self.data.len() { self.data[byte + i] } else { 0xff };
                w = (w << 8) | u64::from(b);
            }
            w
        };
        ((word << (self.pos & 7)) >> (64 - n)) as u32
    }

    #[inline]
    pub fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    #[inline]
    pub fn read_bit(&mut self) -> bool {
        self.read(1) != 0
    }

    #[inline]
    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// `AlignToNextByte`.
    #[inline]
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// Bits between the read position and the end of the access unit (negative past the end).
    #[inline]
    pub fn bits_left(&self) -> isize {
        (self.data.len() * 8) as isize - self.pos as isize
    }

    #[cfg(test)]
    pub(crate) fn position(&self) -> usize {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_past_end_ones() {
        let d = [0b1010_0101, 0x0f, 0x00];
        let mut r = BitReader::new(&d);
        assert_eq!(r.read(3), 0b101);
        assert_eq!(r.read(7), 0b001_0100);
        assert_eq!(r.bits_left(), 24 - 10);
        r.skip(14);
        assert_eq!(r.bits_left(), 0);
        assert_eq!(r.read(32), 0xffff_ffff);
        assert_eq!(r.bits_left(), -32);
        r.skip(1000);
        assert_eq!(r.read(17), 0x1ffff);
        r.align();
        assert_eq!(r.position() % 8, 0);
    }

    #[test]
    fn straddling_reads() {
        let d = [0xde, 0xad, 0xbe, 0xef, 0x12, 0x34, 0x56, 0x78, 0x9a];
        for start in 0..40usize {
            for n in 1..=32u32 {
                let mut r = BitReader::new(&d);
                r.skip(start);
                let got = r.read(n);
                let mut exp = 0u64;
                for i in 0..n as usize {
                    let p = start + i;
                    let bit = if p / 8 < d.len() { (d[p / 8] >> (7 - p % 8)) & 1 } else { 1 };
                    exp = (exp << 1) | u64::from(bit);
                }
                assert_eq!(u64::from(got), exp, "start {start} n {n}");
            }
        }
    }
}
