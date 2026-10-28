//! Streaming SHA-256 for platforms without CommonCrypto.
//!
//! This is the FIPS 180-4 SHA-256 compression and padding algorithm. The
//! digest is byte-identical to CommonCrypto, independent of host endianness.

const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];
const ROUND: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn compress(state: &mut [u32; 8], block: &[u8]) {
    let mut words = [0u32; 64];
    for (word, bytes) in words[..16].iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes(bytes.try_into().expect("four-byte chunk"));
    }
    for i in 16..64 {
        let x = words[i - 15];
        let y = words[i - 2];
        let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
        let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
        words[i] = words[i - 16].wrapping_add(s0).wrapping_add(words[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ (!e & g);
        let first = h.wrapping_add(s1).wrapping_add(choose).wrapping_add(ROUND[i]).wrapping_add(words[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let second = s0.wrapping_add(majority);
        h = g; g = f; f = e; e = d.wrapping_add(first);
        d = c; c = b; b = a; a = first.wrapping_add(second);
    }
    for (value, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *value = value.wrapping_add(add);
    }
}

/// Incremental SHA-256 state; inputs can be split at arbitrary byte boundaries.
#[derive(Clone)]
pub struct Sha256 { state: [u32; 8], block: [u8; 64], used: usize, bytes: u64 }
impl Default for Sha256 {
    fn default() -> Self { Self::new() }
}
impl Sha256 {
    pub fn new() -> Self { Self { state: INITIAL, block: [0; 64], used: 0, bytes: 0 } }

    pub fn update(&mut self, mut input: &[u8]) {
        self.bytes = self.bytes.wrapping_add(input.len() as u64);
        if self.used != 0 {
            let count = (64 - self.used).min(input.len());
            self.block[self.used..self.used + count].copy_from_slice(&input[..count]);
            self.used += count; input = &input[count..];
            if self.used < 64 { return; }
            compress(&mut self.state, &self.block); self.used = 0;
        }
        while input.len() >= 64 {
            compress(&mut self.state, &input[..64]);
            input = &input[64..];
        }
        self.block[..input.len()].copy_from_slice(input);
        self.used = input.len();
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bit_length = self.bytes.wrapping_mul(8).to_be_bytes();
        self.block[self.used] = 0x80;
        self.used += 1;
        if self.used > 56 {
            self.block[self.used..].fill(0);
            compress(&mut self.state, &self.block);
            self.used = 0;
        }
        self.block[self.used..56].fill(0);
        self.block[56..].copy_from_slice(&bit_length);
        compress(&mut self.state, &self.block);
        let mut digest = [0u8; 32];
        for (chunk, word) in digest.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }
}

/// SHA-256 of a complete byte slice.
pub fn sha256(input: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new(); digest.update(input); digest.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() }

    #[test]
    fn known_sha256_vectors_and_stream_boundaries() {
        for (input, expected) in [
            (b"".as_slice(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            (b"abc".as_slice(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            (b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".as_slice(),
             "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"),
        ] {
            assert_eq!(hex(&sha256(input)), expected);
            for width in [1, 2, 7, 55, 56, 63, 64, 65] {
                let mut digest = Sha256::new();
                for chunk in input.chunks(width) { digest.update(chunk); }
                assert_eq!(hex(&digest.finalize()), expected);
            }
        }
        let million = vec![b'a'; 1_000_000];
        assert_eq!(hex(&sha256(&million)), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn streaming_matches_commoncrypto_across_partitions() {
        use std::ffi::c_void;
        #[link(name = "System")]
        unsafe extern "C" { fn CC_SHA256(data: *const c_void, len: u32, digest: *mut u8) -> *mut u8; }
        let input: Vec<u8> = (0..131_137).map(|i| (i * 37 + i / 257) as u8).collect();
        for len in [0, 1, 55, 56, 63, 64, 65, 127, 128, 129, 4096, 131_137] {
            let mut expected = [0u8; 32];
            assert!(!unsafe { CC_SHA256(input.as_ptr().cast(), len as u32, expected.as_mut_ptr()) }.is_null());
            assert_eq!(sha256(&input[..len]), expected);
            for width in [1, 7, 63, 64, 65, 4096] {
                let mut digest = Sha256::new();
                for chunk in input[..len].chunks(width) { digest.update(chunk); }
                assert_eq!(digest.finalize(), expected, "length={len} width={width}");
            }
        }
    }
}
