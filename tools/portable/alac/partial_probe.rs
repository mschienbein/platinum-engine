//! Compare AudioToolbox and portable ALAC packets for the cases of `partial_fixtures.rs` and
//! print the expected-value tables that `tests/portable_alac_packets.rs` pins: the packets of the
//! partial-packet cases, and for the oversized-packet cases the whole file (clock zeroed) or the
//! refusal AudioToolbox's writer ends in.
//! Usage: partial-probe EXISTING_ABSOLUTE_SCRATCH_DIRECTORY
//! Exit status 1 when any portable packet differs from the AudioToolbox packet.
#![cfg(target_os = "macos")]
#[path = "native_probe.rs"] mod native;
#[path = "partial_fixtures.rs"] mod fixtures;
#[path = "../../../src/portable/audio/alac.rs"] mod alac;
#[path = "../../../src/portable/audio/mp4.rs"] mod mp4;
#[path = "../../../src/portable/digest.rs"] mod digest;
use std::fs::{self, File};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

fn hex(bytes: &[u8]) -> String { digest::sha256(bytes).iter().map(|b| format!("{b:02x}")).collect() }

fn native_packets(scratch: &Path, case: &fixtures::Case, pcm: &[i32]) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    let floats = alac::samples_to_float(pcm, case.depth)?;
    let path = scratch.join("case.m4a");
    native::encode(&path, 44_100, u32::from(case.channels), u32::from(case.depth), &floats)?;
    let mut file = File::open(&path)?;
    let index = mp4::read_alac(&mut file)?;
    let mut packets = Vec::new();
    for packet in &index.packets {
        file.seek(SeekFrom::Start(packet.offset))?;
        let mut bytes = vec![0; packet.size as usize];
        file.read_exact(&mut bytes)?;
        packets.push(bytes);
    }
    drop(file);
    fs::remove_file(&path)?;
    Ok(packets)
}

fn portable_packets(case: &fixtures::Case, pcm: &[i32]) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    let config = alac::Config { frame_length: 4096, bit_depth: case.depth, pb: 40, mb: 10, kb: 14, channels: case.channels, sample_rate: 44_100 };
    let mut encoder = alac::Encoder::new(config)?;
    let mut packets = Vec::new();
    for chunk in pcm.chunks(4096 * usize::from(case.channels)) { packets.push(encoder.encode_packet(chunk)?); }
    if case.frames != 0 && case.frames % 4096 == 0 { packets.push(encoder.encode_packet(&[])?); }
    Ok(packets)
}

/// The cookie's `maxFrameBytes` and `avgBitRate` of an ALAC file.
fn cookie(bytes: &[u8]) -> Result<(u32, u32), Box<dyn std::error::Error>> {
    let entry = bytes.windows(4).enumerate().filter(|(_, w)| *w == b"alac").map(|(i, _)| i).nth(1).ok_or("no ALAC cookie")?;
    let config = &bytes[entry + 8..];
    Ok((u32::from_be_bytes(config[12..16].try_into()?), u32::from_be_bytes(config[16..20].try_into()?)))
}

/// What the portable stream writes for the 512-frame calls of the engine: the file, or `None` for a refusal.
fn portable_file(config: alac::Config, floats: &[f32]) -> Option<Vec<u8>> {
    let channels = usize::from(config.channels);
    let mut stream = alac::Stream::new(config).ok()?;
    let (mut spool, mut packets) = (Vec::new(), Vec::new());
    let mut emit = |packet: &[u8], frames: u32| -> Result<(), ()> { spool.extend_from_slice(packet); packets.push(mp4::EncodedPacket { size: packet.len() as u32, frames }); Ok(()) };
    for call in floats.chunks(channels * 512) {
        let mut pcm = vec![0; call.len()];
        alac::quantize_into(call, config.bit_depth, &mut pcm).ok()?;
        stream.write(&pcm, &mut emit).ok()?;
    }
    let stats = stream.finish(&mut emit).ok()?;
    let mut file = Vec::new();
    mp4::write_alac_with_stats(&mut file, &mut Cursor::new(spool), config, f64::from(config.sample_rate), &packets, stats, 0).ok()?;
    Some(file)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scratch = PathBuf::from(std::env::args_os().nth(1).ok_or("scratch directory required")?);
    if !scratch.is_absolute() || !scratch.is_dir() { return Err("provide an existing absolute scratch directory".into()); }
    let mut mismatches = 0usize;
    println!("// CASES: name, packets, last packet bytes, SHA-256 of the length-framed packet stream");
    for case in fixtures::CASES {
        let pcm = fixtures::pcm(case);
        let native = native_packets(&scratch, case, &pcm)?;
        let portable = portable_packets(case, &pcm)?;
        if native != portable { mismatches += 1; eprintln!("MISMATCH {case:?}"); }
        println!("    ({:?}, {}, {}, {:?}),", case.name, native.len(), native.last().map_or(0, Vec::len), hex(&fixtures::framed(&native)));
    }
    println!("// SERIES: size, last packet bytes, SHA-256 of the last packet; first packet SHA-256 once");
    let mut first = String::new();
    for size in fixtures::SERIES_SIZES {
        let case = fixtures::series_case(size);
        let pcm = fixtures::pcm(&case);
        let native = native_packets(&scratch, &case, &pcm)?;
        let portable = portable_packets(&case, &pcm)?;
        if native != portable { mismatches += 1; eprintln!("MISMATCH {case:?}"); }
        assert_eq!(native.len(), 2);
        if first.is_empty() { first = hex(&native[0]); }
        assert_eq!(first, hex(&native[0]), "the series shares its first packet");
        println!("    ({size}, {}, {:?}),", native[1].len(), hex(&native[1]));
    }
    println!("// SERIES first packet: {first:?}");
    println!("// OVERFLOW: name, packets, cookie maxFrameBytes, cookie avgBitRate, SHA-256 of the file with its clock zeroed; None where AudioToolbox refuses");
    for case in fixtures::OVERFLOW {
        let floats = fixtures::overflow_floats(case);
        let path = scratch.join("case.m4a");
        let written = native::encode(&path, case.rate, u32::from(case.channels), u32::from(case.depth), &floats);
        let bytes = if written.is_ok() { fs::read(&path)? } else { Vec::new() };
        if path.exists() { fs::remove_file(&path)?; }
        // The macOS writer reads the finished file back and refuses one that decodes to fewer frames than it was given.
        let expected = match mp4::read_alac(&mut Cursor::new(bytes.clone())) {
            Ok(index) if written.is_ok() && index.frames == case.frames as u64 && !index.packets.is_empty() => {
                let (max, average) = cookie(&bytes)?;
                Some((index.packets.len(), max, average, hex(&fixtures::mask_times(bytes))))
            }
            _ => None,
        };
        let config = alac::Config { frame_length: 4096, bit_depth: case.depth, pb: 40, mb: 10, kb: 14, channels: case.channels, sample_rate: case.rate };
        let portable = portable_file(config, &floats);
        let same = match (&expected, &portable) {
            (None, None) => true,
            (Some((_, _, _, digest)), Some(file)) => *digest == hex(&fixtures::mask_times(file.clone())),
            _ => false,
        };
        if !same { mismatches += 1; eprintln!("MISMATCH {case:?}: AudioToolbox {} / portable {}", expected.is_some(), portable.is_some()); }
        match expected {
            Some((packets, max, average, digest)) => println!("    ({:?}, Some(({packets}, {max}, {average}, {digest:?}))),", case.name),
            None => println!("    ({:?}, None),", case.name),
        }
    }
    println!("// mismatches={mismatches}");
    if mismatches != 0 { std::process::exit(1); }
    Ok(())
}
