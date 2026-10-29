//! Shared by `portable_mp3.rs`: the recorded macOS file-type admission (`admission.json`) and buffer tails (`tails.json`),
//! and the recipe evaluator that rebuilds the generated inputs behind them. See `tests/fixtures/mp3-exact/README.md`
//! and the `about` member of each document. Nothing here needs Apple at run time.
#![allow(dead_code)]

use super::mp3_exact::{Fixtures, fixture_dir};
use serde_json::Value;
use std::fs;

pub fn load(name: &str, schema: &str) -> Value {
    let document: Value = serde_json::from_slice(&fs::read(fixture_dir().join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))).unwrap();
    assert_eq!(document["schema"], schema, "{name}");
    document
}

fn hex_bytes(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    assert!(digits.len() % 2 == 0, "odd hex length");
    digits.chunks(2).map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
}

fn lcg(seed: u64, count: usize) -> Vec<u8> {
    let mut x = (seed & 0xffff_ffff) as u32;
    (0..count).map(|_| { x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); (x >> 24) as u8 }).collect()
}

/// IEEE 754 extended precision (80 bits, big endian) of a positive integer.
fn extended(rate: u64) -> [u8; 10] {
    let bits = 64 - rate.leading_zeros();
    let mut out = [0u8; 10];
    out[..2].copy_from_slice(&((16_383 + bits - 1) as u16).to_be_bytes());
    out[2..].copy_from_slice(&(rate << (64 - bits)).to_be_bytes());
    out
}

fn sample(i: u64, channel: u64, bits: u64) -> i32 {
    let value = ((i * 7919 + channel * 13) % 2001) as i32 - 1000;
    if bits >= 16 { value << (bits - 16) } else { value }
}

fn pcm_data(rate: u64, channels: u64, bits: u64, frames: u64, big_endian: bool) -> Vec<u8> {
    let _ = rate;
    let width = (bits / 8) as usize;
    let mut data = Vec::new();
    for i in 0..frames {
        for c in 0..channels {
            let v = sample(i, c, bits);
            if big_endian { data.extend_from_slice(&v.to_be_bytes()[4 - width..]); } else { data.extend_from_slice(&v.to_le_bytes()[..width]); }
        }
    }
    data
}

fn wav(rate: u64, channels: u64, bits: u64, frames: u64) -> Vec<u8> {
    let data = pcm_data(rate, channels, bits, frames, false);
    let mut body = b"WAVEfmt ".to_vec();
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&(channels as u16).to_le_bytes());
    body.extend_from_slice(&(rate as u32).to_le_bytes());
    body.extend_from_slice(&((rate * channels * bits / 8) as u32).to_le_bytes());
    body.extend_from_slice(&((channels * bits / 8) as u16).to_le_bytes());
    body.extend_from_slice(&(bits as u16).to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&data);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn aiff(rate: u64, channels: u64, bits: u64, frames: u64) -> Vec<u8> {
    let data = pcm_data(rate, channels, bits, frames, true);
    let mut comm = (channels as u16).to_be_bytes().to_vec();
    comm.extend_from_slice(&(frames as u32).to_be_bytes());
    comm.extend_from_slice(&(bits as u16).to_be_bytes());
    comm.extend_from_slice(&extended(rate));
    let mut ssnd = vec![0u8; 8];
    ssnd.extend_from_slice(&data);
    let mut body = b"AIFFCOMM".to_vec();
    body.extend_from_slice(&(comm.len() as u32).to_be_bytes());
    body.extend_from_slice(&comm);
    body.extend_from_slice(b"SSND");
    body.extend_from_slice(&(ssnd.len() as u32).to_be_bytes());
    body.extend_from_slice(&ssnd);
    let mut out = b"FORM".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// Evaluate one recipe of `admission.json` (grammar in `tools/make_admission_fixture.py` of the evidence directory).
pub fn build(recipe: &Value, document: &Value, fixtures: &Fixtures) -> Vec<u8> {
    let (kind, arg) = recipe.as_object().unwrap().iter().next().unwrap();
    let n = |v: &Value| v.as_u64().unwrap();
    match kind.as_str() {
        "pack" => fixtures.bytes(fixtures.get(arg.as_str().unwrap())).to_vec(),
        "blob" => hex_bytes(document["blobs"][arg.as_str().unwrap()].as_str().unwrap()),
        "hex" => hex_bytes(arg.as_str().unwrap()),
        "zeros" => vec![0; n(arg) as usize],
        "lcg" => lcg(n(&arg[0]), n(&arg[1]) as usize),
        "cat" => arg.as_array().unwrap().iter().flat_map(|r| build(r, document, fixtures)).collect(),
        "head" => { let mut b = build(&arg[0], document, fixtures); b.truncate(n(&arg[1]) as usize); b }
        "skip" => { let b = build(&arg[0], document, fixtures); b[(n(&arg[1]) as usize).min(b.len())..].to_vec() }
        "id3" => {
            let (version, flags, size) = (n(&arg[0]) as u8, n(&arg[1]) as u8, n(&arg[2]) as usize);
            let mut b = b"ID3".to_vec();
            b.extend_from_slice(&[version, 0, flags, (size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
            b.resize(10 + size, 0);
            b
        }
        "wav" => wav(n(&arg[0]), n(&arg[1]), n(&arg[2]), n(&arg[3])),
        "aiff" => aiff(n(&arg[0]), n(&arg[1]), n(&arg[2]), n(&arg[3])),
        "flip" => { let mut b = build(&arg[0], document, fixtures); b[n(&arg[1]) as usize] ^= n(&arg[2]) as u8; b }
        other => panic!("unknown recipe {other}"),
    }
}

/// The sentinel the tail probe pre-fills every buffer with (`examples/tail_json.rs` of the recording).
pub const SENTINEL: f32 = 12345.5;

/// FNV-1a over 32-bit words, as the recording folds the float bits.
pub fn fold(hash: u64, bits: u32) -> u64 { (hash ^ u64::from(bits)).wrapping_mul(0x100_0000_01b3) }
pub const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;
