//! Cross-platform replay of the portable MP3 decoder over a directory of files.
//!
//! Build: `rustc --edition 2024 -O replay.rs -o mp3replay` (std only).
//! Run:   `mp3replay DIR`
//!
//! For every `*.mp3` in DIR (sorted by name) this prints one tab-separated
//! line: file name, `ok` or `refused`, rate, channels, the length property in
//! frames, the number of frames actually decoded and the SHA-256 of the decoded
//! planar f32 bit patterns (left then right sample per frame, little endian).
//! Each file is decoded twice with different request sizes and the run aborts
//! if the results differ. A refused file prints the typed error instead.
//! Running this on macOS (`--features portable` source), Linux arm64 and Linux
//! x86_64 and diffing the output proves the decode is bit-identical across
//! hosts and independent of the read size.

#![allow(dead_code)]

#[path = "../../../src/portable/digest.rs"]
mod digest;
#[path = "../../../src/portable/audio/mp3.rs"]
mod mp3;

use std::fs::File;
use std::path::Path;

fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode(path: &Path, chunk: usize) -> Result<(mp3::Mp3Info, u64, String), mp3::Mp3Error> {
    let mut stream = mp3::Mp3Stream::open(File::open(path).map_err(|e| mp3::Mp3Error::Io("open", e))?)?;
    let info = stream.info();
    let mut hash = digest::Sha256::new();
    let (mut left, mut right) = (vec![0f32; chunk], vec![0f32; chunk]);
    let mut bytes = Vec::with_capacity(chunk * 8);
    let mut total = 0u64;
    loop {
        let n = stream.read_planar(&mut left, &mut right)?;
        if n == 0 { break; }
        bytes.clear();
        for k in 0..n {
            bytes.extend_from_slice(&left[k].to_bits().to_le_bytes());
            bytes.extend_from_slice(&right[k].to_bits().to_le_bytes());
        }
        hash.update(&bytes);
        total += n as u64;
    }
    Ok((info, total, hex(hash.finalize())))
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: mp3replay DIR");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("readable directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "mp3"))
        .collect();
    files.sort();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        match decode(&path, 4096).and_then(|(info, frames, a)| decode(&path, 997).map(|(_, again, b)| (info, frames, again, a, b))) {
            Ok((info, frames, again, a, b)) => {
                assert_eq!((frames, &a), (again, &b), "{name}: decode depends on the read size");
                println!("{name}\tok\t{}\t{}\t{}\t{frames}\t{a}", info.sample_rate, info.channels, info.length_frames);
            }
            Err(e) => println!("{name}\trefused\t{e:?}"),
        }
    }
}
