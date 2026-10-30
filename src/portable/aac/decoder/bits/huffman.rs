//! The 12 Huffman codes (11 spectral codebooks + scalefactor code), built from `crate::portable::aac::bits::tables` (proved identical to Apple's
//! `_hHCB_*` tables: every ISO codeword decodes to the same length/symbol in Apple's structure, all 12 codes are complete prefix codes,
//! so no bit pattern is invalid) as binary decode trees.
//!
//! Decoding peeks `maxlen` bits (bits past the end of the access unit are ones) and consumes exactly the codeword length, like Apple.

use crate::portable::aac::decoder::bits::bitreader::BitReader;
use crate::portable::aac::bits::tables as iso;
use std::sync::LazyLock;

const NONE: i32 = i32::MAX;

pub struct HuffTree {
    /// `nodes[n][bit]`: `>= 0` child node index, `< 0` leaf `!symbol`.
    nodes: Vec<[i32; 2]>,
    maxlen: u32,
}

impl HuffTree {
    fn build(codes: &[(u32, u8)]) -> Self {
        let mut nodes: Vec<[i32; 2]> = vec![[NONE, NONE]];
        let mut maxlen = 0u32;
        for (sym, &(code, len)) in codes.iter().enumerate() {
            assert!(len > 0, "zero-length codeword");
            maxlen = maxlen.max(u32::from(len));
            let mut node = 0usize;
            for d in 0..len {
                let bit = ((code >> (len - 1 - d)) & 1) as usize;
                if d + 1 == len {
                    assert_eq!(nodes[node][bit], NONE, "prefix collision");
                    nodes[node][bit] = !(sym as i32);
                } else {
                    if nodes[node][bit] == NONE {
                        nodes.push([NONE, NONE]);
                        nodes[node][bit] = (nodes.len() - 1) as i32;
                    }
                    let next = nodes[node][bit];
                    assert!(next >= 0, "codeword extends a shorter codeword");
                    node = next as usize;
                }
            }
        }
        for n in &nodes {
            assert!(n[0] != NONE && n[1] != NONE, "incomplete prefix code");
        }
        Self { nodes, maxlen }
    }

    /// Decode one symbol (its index in the ISO code table).
    #[inline]
    pub fn decode(&self, br: &mut BitReader) -> usize {
        let w = br.peek(self.maxlen);
        let mut node = 0usize;
        let mut depth = 0u32;
        loop {
            let bit = ((w >> (self.maxlen - 1 - depth)) & 1) as usize;
            depth += 1;
            let n = self.nodes[node][bit];
            if n < 0 {
                br.skip(depth as usize);
                return (!n) as usize;
            }
            node = n as usize;
        }
    }
}

pub struct HuffTables {
    pub sf: HuffTree,
    /// Spectral codebook `cb` (1..=11) is `spec[cb - 1]`.
    pub spec: [HuffTree; 11],
}

static TABLES: LazyLock<HuffTables> = LazyLock::new(|| {
    let sf: Vec<(u32, u8)> = iso::SF_CODE.iter().zip(iso::SF_LEN.iter()).map(|(&c, &l)| (c, l)).collect();
    let spec = std::array::from_fn(|i| {
        let v: Vec<(u32, u8)> = iso::SPEC_CODE[i].iter().zip(iso::SPEC_LEN[i].iter()).map(|(&c, &l)| (u32::from(c), l)).collect();
        HuffTree::build(&v)
    });
    HuffTables { sf: HuffTree::build(&sf), spec }
});

pub fn tables() -> &'static HuffTables {
    &TABLES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codeword_roundtrips() {
        let t = tables();
        let check = |tree: &HuffTree, codes: &[(u32, u8)]| {
            for (sym, &(code, len)) in codes.iter().enumerate() {
                for tail in [0u32, 0xffff_ffff] {
                    // codeword followed by a tail pattern, packed into 8 bytes
                    let mut v: u64 = 0;
                    let mut nbits = 0;
                    v = (v << len) | u64::from(code);
                    nbits += u32::from(len);
                    let tail_bits = 64 - nbits;
                    v = (v << tail_bits) | (u64::from(tail) & ((1u64 << tail_bits.min(32)) - 1));
                    let bytes = v.to_be_bytes();
                    let mut br = BitReader::new(&bytes);
                    assert_eq!(tree.decode(&mut br), sym);
                    assert_eq!(br.position(), len as usize);
                }
            }
        };
        let sf: Vec<(u32, u8)> = iso::SF_CODE.iter().zip(iso::SF_LEN.iter()).map(|(&c, &l)| (c, l)).collect();
        check(&t.sf, &sf);
        for i in 0..11 {
            let v: Vec<(u32, u8)> = iso::SPEC_CODE[i].iter().zip(iso::SPEC_LEN[i].iter()).map(|(&c, &l)| (u32::from(c), l)).collect();
            check(&t.spec[i], &v);
        }
    }

    #[test]
    fn odd_books_zero_tuple_is_single_zero_bit() {
        // the 1-bit codeword '0' of books 1,3,5,7,9 is the all-zero tuple (Apple's shortcut equals the table)
        let zero_sym = [40usize, 0, 40, 0, 0];
        for (k, cb) in [1usize, 3, 5, 7, 9].iter().enumerate() {
            let i = cb - 1;
            assert_eq!(iso::SPEC_LEN[i][zero_sym[k]], 1, "cb {cb}");
            assert_eq!(iso::SPEC_CODE[i][zero_sym[k]], 0, "cb {cb}");
        }
    }
}
