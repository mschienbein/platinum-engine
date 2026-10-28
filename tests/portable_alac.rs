//! ALAC packet/container tests. Heavy native comparisons are explicitly ignored.
use platinum_engine::portable::audio::alac::{self, Config};

#[path = "../tools/portable/alac/mp4_bomb_fixtures.rs"]
mod mp4_bomb_fixtures;

fn config(channels: u8, bit_depth: u8) -> Config {
    Config { frame_length: 4096, bit_depth, pb: 40, mb: 10, kb: 14, channels, sample_rate: 44_100 }
}

#[test]
fn one_sample_native_escape_packet_and_truncation() {
    let packet = [0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x03, 0x9c, 0x01, 0xc0];
    let result = alac::decode_packet(config(1, 16), &packet).unwrap();
    assert_eq!(result, [-12800]);
    assert_eq!(alac::samples_to_float(&result, 16).unwrap(), [-0.390625]);
    for end in 0..packet.len() {
        assert!(alac::decode_packet(config(1, 16), &packet[..end]).is_err(), "accepted truncated packet of {end} bytes");
    }
}

#[test]
fn empty_final_packet_and_quantizer_boundaries() {
    let packet = [0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x01, 0xc0];
    assert!(alac::decode_packet(config(1, 16), &packet).unwrap().is_empty());
    assert_eq!(alac::Encoder::new(config(1, 16)).unwrap().encode_packet(&[]).unwrap(), packet);
    let samples = [f32::from_bits(0xb3800000), f32::from_bits(0xb3800001), f32::from_bits(0xb4400000), 1.0, -1.0, f32::NAN];
    let mut result = [0; 6];
    alac::quantize_into(&samples, 24, &mut result).unwrap();
    assert_eq!(result, [0, -1, -2, 8_388_607, -8_388_608, 0]);
}

#[test]
fn packet_buffers_reuse_allocations() {
    let expected = [0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x03, 0x9c, 0x01, 0xc0];
    let mut encoded = Vec::with_capacity(128);
    let encoded_ptr = encoded.as_ptr();
    alac::Encoder::new(config(1, 16)).unwrap().encode_packet_into(&[-12800], &mut encoded).unwrap();
    assert_eq!(encoded, expected);
    assert_eq!(encoded.as_ptr(), encoded_ptr);
    let mut decoded = Vec::with_capacity(128);
    let decoded_ptr = decoded.as_ptr();
    let mut decoder = alac::Decoder::new();
    decoder.decode_packet_into(config(1, 16), &encoded, &mut decoded).unwrap();
    assert_eq!(decoded, [-12800]);
    assert_eq!(decoded.as_ptr(), decoded_ptr);
    assert!(decoder.decode_packet_into(config(1, 16), &encoded[..3], &mut decoded).is_err());
    decoder.decode_packet_into(config(1, 16), &encoded, &mut decoded).unwrap();
    assert_eq!(decoded, [-12800]);
}

#[test]
fn reusable_decoder_handles_compressed_packets_after_error() {
    let cfg = config(2, 24);
    let pcm: Vec<i32> = (0..8192).map(|i| (((i * 17) % 101) - 50) * 4096 + (i & 255)).collect();
    let mut encoder = alac::Encoder::new(cfg).unwrap();
    let mut decoder = alac::Decoder::new();
    let mut packet = Vec::new();
    let mut decoded = Vec::new();
    for _ in 0..3 {
        encoder.encode_packet_into(&pcm, &mut packet).unwrap();
        decoder.decode_packet_into(cfg, &packet, &mut decoded).unwrap();
        assert_eq!(decoded, pcm);
    }
    assert!(decoder.decode_packet_into(cfg, &packet[..packet.len() / 2], &mut decoded).is_err());
    decoder.decode_packet_into(cfg, &packet, &mut decoded).unwrap();
    assert_eq!(decoded, pcm);
}

#[test]
fn oversized_packet_retries_or_refuses() {
    let mut rng = 0x5eed_1234u32;
    let mut floats = Vec::with_capacity(4096 * 6);
    for _ in 0..4096 * 6 {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        floats.push((rng as i32) as f32 * (1.0 / 2_147_483_648.0));
    }
    let mut pcm = vec![0; floats.len()];
    alac::quantize_into(&floats, 24, &mut pcm).unwrap();
    let mut encoder = alac::Encoder::new(config(6, 24)).unwrap();
    assert!(matches!(encoder.encode_packet(&pcm), Err(alac::EncodeError::NativePacketLimit { capacity: 73736, .. })));
    assert_eq!(encoder.encode_packet(&[]), Err(alac::EncodeError::Poisoned));

    // The same high-entropy frame with a short silent prefix in its last
    // channel exceeds the buffer on the first attempt but fits on retry.
    for frame in 0..140 { floats[frame * 6 + 5] = 0.0; }
    alac::quantize_into(&floats, 24, &mut pcm).unwrap();
    let packet = alac::Encoder::new(config(6, 24)).unwrap().encode_packet(&pcm).unwrap();
    assert_eq!(packet.len(), 73734);
    let digest: String = platinum_engine::portable::digest::sha256(&packet).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(digest, "29efdd2cd2daa06e6e8e33bc6cacdbe71523a2cdd003e131add28bf1e5ced6f8");
}

#[test]
fn mp4_index_tracks_actual_frames_and_layout() {
    use platinum_engine::portable::audio::mp4::{self, EncodedPacket};
    use std::io::Cursor;
    let cfg = config(6, 16);
    let mut encoder = alac::Encoder::new(cfg).unwrap();
    let first = encoder.encode_packet(&vec![0; 4096 * 6]).unwrap();
    let last = encoder.encode_packet(&[]).unwrap();
    let packets = [EncodedPacket { size: first.len() as u32, frames: 4096 },
        EncodedPacket { size: last.len() as u32, frames: 0 }];
    let mut payload = first;
    payload.extend_from_slice(&last);
    let mut bytes = Vec::new();
    mp4::write_alac_with_rate(&mut bytes, &mut Cursor::new(payload), cfg, f64::from(cfg.sample_rate), &packets, 0x1234_5678).unwrap();
    let mut source = Cursor::new(bytes);
    let index = mp4::read_alac(&mut source).unwrap();
    assert_eq!(index.frames, 4096);
    assert_eq!(index.packets.iter().map(|p| p.frames).collect::<Vec<_>>(), [4096, 0]);
    assert_eq!(index.channel_layout.unwrap().tag, (124 << 16) | 6);
    for (channels, tag) in [(3, 114), (4, 116), (5, 120), (7, 142), (8, 127)] {
        let mut bytes = Vec::new();
        mp4::write_alac_with_rate(&mut bytes, &mut Cursor::new([]), config(channels, 16), 44100.0, &[], 0).unwrap();
        let index = mp4::read_alac(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(index.channel_layout.unwrap().tag, (tag << 16) | u32::from(channels));
    }
    let mut invalid = cfg;
    invalid.sample_rate = 22049;
    let mut out = Vec::new();
    assert!(matches!(mp4::write_alac_with_rate(&mut out, &mut Cursor::new([]), invalid, f64::from(invalid.sample_rate), &[], 0),
        Err(mp4::WriterError::Unsupported(_))));
    assert!(out.is_empty());
}

#[test]
fn mp4_rejects_nonempty_chunk_table_without_mapping() {
    use platinum_engine::portable::audio::mp4::{self, EncodedPacket, Mp4Error};
    use std::io::Cursor;
    let cfg = config(1, 16);
    let packet = alac::Encoder::new(cfg).unwrap().encode_packet(&[0]).unwrap();
    let meta = [EncodedPacket { size: packet.len() as u32, frames: 1 }];
    let mut bytes = Vec::new();
    mp4::write_alac_with_rate(&mut bytes, &mut Cursor::new(packet), cfg, 44100.0, &meta, 0).unwrap();
    let stsc = bytes.windows(4).position(|b| b == b"stsc").unwrap();
    // Preserve the parent size and mdat offset: shorten stsc and use its
    // original entry as a separate, well-formed free atom.
    bytes[stsc - 4..stsc].copy_from_slice(&16u32.to_be_bytes());
    bytes[stsc + 8..stsc + 12].copy_from_slice(&0u32.to_be_bytes());
    bytes[stsc + 12..stsc + 16].copy_from_slice(&12u32.to_be_bytes());
    bytes[stsc + 16..stsc + 20].copy_from_slice(b"free");
    bytes[stsc + 20..stsc + 24].fill(0);
    assert!(matches!(mp4::read_alac(&mut Cursor::new(bytes)),
        Err(Mp4Error::Invalid("inconsistent empty sample table"))));
}

#[test]
fn mp4_rejects_hostile_sample_table_counts() {
    use platinum_engine::portable::audio::mp4::{self, EncodedPacket, Mp4Error};
    use std::io::Cursor;
    let cfg = config(1, 16);
    let packet = alac::Encoder::new(cfg).unwrap().encode_packet(&[0]).unwrap();
    let meta = [EncodedPacket { size: packet.len() as u32, frames: 1 }];
    let mut bytes = Vec::new();
    mp4::write_alac_with_rate(&mut bytes, &mut Cursor::new(packet), cfg, 44100.0, &meta, 0).unwrap();
    let (valid, valid_fixed, malformed) = mp4_bomb_fixtures::fixtures(bytes);
    for file in [valid, valid_fixed] {
        assert_eq!(mp4::read_alac(&mut Cursor::new(file)).unwrap().frames, 1);
    }
    for (name, file) in malformed {
        assert!(matches!(mp4::read_alac(&mut Cursor::new(file)), Err(Mp4Error::Invalid(_))), "{name}");
    }
}

#[test]
fn mp4_rate_reservation_and_exact_chunk_boundaries() {
    use platinum_engine::portable::audio::mp4::{self, EncodedPacket};
    use std::io::Cursor;
    for (rate, expected) in [(57343, 24576), (57344, 20480)] {
        let mut cfg = config(1, 16);
        cfg.sample_rate = rate;
        let packet = alac::Encoder::new(cfg).unwrap().encode_packet(&[0]).unwrap();
        let meta = [EncodedPacket { size: packet.len() as u32, frames: 1 }];
        let mut bytes = Vec::new();
        mp4::write_alac_with_rate(&mut bytes, &mut Cursor::new(packet), cfg, f64::from(rate), &meta, 0).unwrap();
        let index = mp4::read_alac(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(index.packets[0].offset, expected);
    }
    let mut cfg = config(1, 16);
    cfg.sample_rate = 57344;
    let mut encoder = alac::Encoder::new(cfg).unwrap();
    let full = encoder.encode_packet(&[0; 4096]).unwrap();
    let partial = encoder.encode_packet(&[0]).unwrap();
    let mut payload = Vec::new();
    let mut packets = Vec::new();
    for _ in 0..16 {
        payload.extend_from_slice(&full);
        packets.push(EncodedPacket { size: full.len() as u32, frames: 4096 });
    }
    payload.extend_from_slice(&partial);
    packets.push(EncodedPacket { size: partial.len() as u32, frames: 1 });
    let mut bytes = Vec::new();
    mp4::write_alac_with_rate(&mut bytes, &mut Cursor::new(payload), cfg, f64::from(cfg.sample_rate), &packets, 0).unwrap();
    let stsc = bytes.windows(4).position(|b| b == b"stsc").unwrap();
    let first_chunk_packets = u32::from_be_bytes(bytes[stsc + 16..stsc + 20].try_into().unwrap());
    assert_eq!(first_chunk_packets, 8);
    assert_eq!(mp4::read_alac(&mut Cursor::new(bytes)).unwrap().frames, 65537);
}

#[test]
fn mp4_movie_relocation_handles_exact_and_tiny_gaps() {
    use platinum_engine::portable::audio::mp4::{self, EncodedPacket};
    use std::io::Cursor;
    let cfg = config(1, 16);
    let mut encoder = alac::Encoder::new(cfg).unwrap();
    let full = encoder.encode_packet(&[0; 4096]).unwrap();
    let partial = encoder.encode_packet(&[0]).unwrap();
    for (count, first_kind) in [(4207usize, b"free"), (4208, b"moov"), (4209, b"free")] {
        let mut packets = vec![EncodedPacket { size: full.len() as u32, frames: 4096 }; count - 1];
        packets.push(EncodedPacket { size: partial.len() as u32, frames: 1 });
        let mut payload = Vec::with_capacity(full.len() * (count - 1) + partial.len());
        for _ in 0..count - 1 { payload.extend_from_slice(&full); }
        payload.extend_from_slice(&partial);
        let mut bytes = Vec::new();
        mp4::write_alac_with_rate(&mut bytes, &mut Cursor::new(payload), cfg, 44100.0, &packets, 0).unwrap();
        assert_eq!(&bytes[32..36], first_kind);
        if count == 4208 {
            let moov_size = u32::from_be_bytes(bytes[28..32].try_into().unwrap()) as usize;
            assert_eq!(&bytes[28 + moov_size + 4..28 + moov_size + 8], b"mdat");
        }
        let index = mp4::read_alac(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(index.frames, ((count - 1) * 4096 + 1) as u64);
    }
}

#[test]
fn fractional_rate_uses_original_rate_in_cookie() {
    use platinum_engine::portable::audio::mp4::{self, EncodedPacket};
    use std::io::Cursor;
    let cfg = config(2, 24);
    let mut state = 0x5eed_1234u32;
    let mut samples = Vec::with_capacity(8192);
    for _ in 0..8192 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        samples.push((state as i32) >> 8);
    }
    let mut encoder = alac::Encoder::new(cfg).unwrap();
    let packet = encoder.encode_packet(&samples).unwrap();
    let tail = encoder.encode_packet(&[]).unwrap();
    let sizes = [EncodedPacket { size: packet.len() as u32, frames: 4096 },
        EncodedPacket { size: tail.len() as u32, frames: 0 }];
    let total = packet.len() + tail.len();
    let mut payload = packet;
    payload.extend_from_slice(&tail);
    let mut file = Vec::new();
    mp4::write_alac_with_rate(&mut file, &mut Cursor::new(payload), cfg, 44100.9, &sizes, 0).unwrap();
    let cookie = file.windows(8).position(|b| b == b"\0\0\0\x24alac").unwrap();
    let bitrate = u32::from_be_bytes(file[cookie + 28..cookie + 32].try_into().unwrap());
    assert_eq!(bitrate, ((44100.9 * total as f64) * 8.0 / (2.0 * 4096.0)) as u32);
    let mut empty = Vec::new();
    for rate in [44101.0, 22049.9, 96000.1, f64::INFINITY, f64::NAN] {
        assert!(matches!(mp4::write_alac_with_rate(&mut empty, &mut Cursor::new([]), cfg, rate, &[], 0),
            Err(mp4::WriterError::Unsupported(_))));
    }
    assert!(empty.is_empty());
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use platinum_engine::apple_codec::Reader;
    use platinum_engine::portable::audio::mp4;
    use serde_json::{Value, json};
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    fn hash(path: &Path) -> String { platinum_engine::mp3::file_sha256(path).unwrap() }
    fn portable_decode(path: &Path) -> Result<(mp4::AlacFile, Vec<f32>), String> {
        let mut file = File::open(path).map_err(|e| e.to_string())?;
        let index = mp4::read_alac(&mut file).map_err(|e| e.to_string())?;
        let mut floats = Vec::new();
        for p in &index.packets {
            file.seek(SeekFrom::Start(p.offset)).map_err(|e| e.to_string())?;
            let mut bytes = vec![0; p.size as usize];
            file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
            let decoded = alac::decode_packet(index.config, &bytes).map_err(|e| format!("packet offset {}: {e}", p.offset))?;
            let want = p.frames as usize * index.config.channels as usize;
            if decoded.len() < want { return Err(format!("packet offset {} is shorter than its stts duration", p.offset)); }
            floats.extend(alac::samples_to_float(&decoded[..want], index.config.bit_depth).map_err(|e| e.to_string())?);
        }
        Ok((index, floats))
    }
    fn native_decode(path: &Path) -> Result<Vec<f32>, String> {
        let mut reader = Reader::open(path, None).map_err(|e| e.to_string())?;
        let channels = reader.client_format().channels_per_frame as usize;
        let mut block = vec![0.0f32; 512 * channels];
        let mut samples = Vec::new();
        loop {
            let frames = reader.read(&mut block).map_err(|e| e.to_string())?;
            if frames == 0 { break; }
            samples.extend_from_slice(&block[..frames * channels]);
        }
        reader.close().map_err(|e| e.to_string())?;
        Ok(samples)
    }
    fn compare(path: &Path) -> Value {
        let portable = portable_decode(path);
        let native = native_decode(path);
        match (portable, native) {
            (Ok((index, portable)), Ok(native)) => {
                let first = portable.iter().zip(&native).position(|(a, b)| a.to_bits() != b.to_bits());
                json!({"path":path,"sha256":hash(path),"config":{"depth":index.config.bit_depth,"channels":index.config.channels,"rate":index.config.sample_rate},
                    "packets":index.packets.len(),"frames":index.frames,"portable_samples":portable.len(),"native_samples":native.len(),
                    "equal":first.is_none() && portable.len() == native.len(),"first_mismatch":first.map(|i| json!({"sample":i,"portable_bits":format!("{:08x}",portable[i].to_bits()),"native_bits":format!("{:08x}",native[i].to_bits())}))})
            }
            (Ok((index, portable)), Err(error)) if index.packets.is_empty() && portable.is_empty()
                && error.contains("OSStatus -66567") => json!({"path":path,"sha256":hash(path),
                    "equal":null,"native_empty_read_error":true,"native_error":error}),
            (portable, native) => json!({"path":path,"sha256":hash(path),"equal":false,
                "portable_error":portable.err(),"native_error":native.err()}),
        }
    }
    #[test]
    #[ignore = "compares local AudioToolbox and retained ALAC fixtures; writes create-only evidence"]
    fn decode_native_and_retained() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let output = PathBuf::from(std::env::var_os("PN_ALAC_ARTIFACTS").expect("PN_ALAC_ARTIFACTS"));
        let run = std::env::var("PN_ALAC_RUN").expect("unique PN_ALAC_RUN");
        let dir = output.join(run);
        fs::create_dir(&dir).expect("evidence run directory must be absent");
        let mut paths = Vec::new();
        let fixture_root = root.join("local-artifacts/portable-parity/audio-01/native-alac");
        for bits in [16, 24, 32] { for channels in [1, 2, 6] { for rate in [22_050, 44_100, 96_000] { for frames in [0, 1, 511, 512, 513, 8193] {
            paths.push(fixture_root.join(format!("alac-{bits}-{channels}-{rate}-{frames}.m4a")));
        }}}}
        for relative in [
            "alac-input-wav-01/input/fixture.m4a",
            "alac-match-input-01/input/fixture.m4a",
            "coverage-ext-01/x04-alac-tags-flac/input/tagged-alac.m4a",
            "family-fullctx-01/c04-alac-match/input/fixture.m4a",
            "timestamp-phase-probe-01/alac.m4a",
            "codec-matrix-smoke-01/alac/reference_PN.m4a",
            "codec-matrix-smoke-01/candidate-a/alac/reference_PN.m4a",
        ] {
            paths.push(root.join("local-artifacts/hash-parity").join(relative));
        }
        for (case, labels) in [
            ("matrix-alac-01", &["original-a", "original-b", "original-c-controlled", "original-d-controlled", "rust-a", "rust-current"][..]),
            ("alac-match-input-01", &["original-a", "original-b", "rust-current"][..]),
            ("family-fullctx-01/c04-alac-match", &["original-a", "original-b", "rust-clock-a", "rust-clock-b", "rust-final-09", "rust-final-10", "rust-release-09", "rust-release-10"][..]),
        ] {
            for label in labels {
                paths.push(root.join("local-artifacts/hash-parity").join(case).join("parked").join(label).join("fixture_PN.m4a"));
            }
        }
        let rows: Vec<_> = paths.iter().map(|p| compare(p)).collect();
        let pass = rows.iter().filter(|v| v["equal"] == true).count();
        let expected_empty_errors = rows.iter().filter(|v| v["native_empty_read_error"] == true).count();
        let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","comparison":"AudioToolbox interleaved f32 bit patterns; integer ALAC reconstructed first",
            "fixture_generation":"audio-01 native-alac-receipt.json", "decoder_sha256":hash(&root.join("src/portable/audio/alac.rs")),
            "parser_sha256":hash(&root.join("src/portable/audio/mp4.rs")),"cases":rows.len(),"passed":pass,
            "native_empty_read_errors":expected_empty_errors,"rows":rows});
        let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
        file.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap();
        file.sync_all().unwrap();
        assert_eq!(pass + expected_empty_errors, paths.len(), "native ALAC comparisons failed; see {}", dir.display());
    }

    #[test]
    #[ignore = "compares controlled MP4 header seconds against native files; writes create-only evidence"]
    fn controlled_clock_whole_file() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let output = PathBuf::from(std::env::var_os("PN_ALAC_ARTIFACTS").expect("PN_ALAC_ARTIFACTS"));
        let run = std::env::var("PN_ALAC_RUN").expect("unique PN_ALAC_RUN");
        let dir = output.join(run);
        fs::create_dir(&dir).expect("evidence run directory must be absent");
        let fixtures = root.join("local-artifacts/portable-parity/alac-01/native-signals-01/outputs");
        let names = [
            "silence-1-0-16-22050.m4a", "silence-1-4096-16-22050.m4a",
            "tone-2-4097-24-44100.m4a", "noise-6-4095-32-96000.m4a",
            "transient-6-65537-20-44100.m4a",
        ];
        let mut rows = Vec::new();
        for (case, name) in names.iter().enumerate() {
            let original = fixtures.join(name);
            for second in [0u32, 1, 0x1234_5678, 0xe707_c5bc] {
                let native = dir.join(format!("{case}-{second:08x}-native.m4a"));
                let candidate = dir.join(format!("{case}-{second:08x}-portable.m4a"));
                let mut new_file = OpenOptions::new().write(true).create_new(true).open(&native).unwrap();
                std::io::copy(&mut File::open(&original).unwrap(), &mut new_file).unwrap();
                new_file.sync_all().unwrap();
                drop(new_file);
                let patched = platinum_engine::apple_codec::set_mp4_header_times(&native, second).unwrap();
                let mut source = File::open(&native).unwrap();
                let index = mp4::read_alac(&mut source).unwrap();
                let mut encoded = Vec::new();
                let mut packets = Vec::new();
                for p in &index.packets {
                    source.seek(SeekFrom::Start(p.offset)).unwrap();
                    let start = encoded.len();
                    encoded.resize(start + p.size as usize, 0);
                    source.read_exact(&mut encoded[start..]).unwrap();
                    packets.push(mp4::EncodedPacket { size: p.size, frames: p.frames });
                }
                let mut output = OpenOptions::new().write(true).create_new(true).open(&candidate).unwrap();
                mp4::write_alac_with_rate(&mut output, &mut std::io::Cursor::new(encoded), index.config, f64::from(index.config.sample_rate), &packets, second).unwrap();
                output.sync_all().unwrap();
                let same = fs::read(&native).unwrap() == fs::read(&candidate).unwrap();
                rows.push(json!({"source":original,"source_sha256":hash(&original),"native_controlled":native,
                    "native_sha256":hash(&native),"portable":candidate,"portable_sha256":hash(&candidate),
                    "second_1904":second,"patched_fields":patched,"equal":same}));
            }
        }
        let pass = rows.iter().filter(|row| row["equal"] == true && row["patched_fields"] == 6).count();
        let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","cases":rows.len(),"passed":pass,"rows":rows});
        let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
        file.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap();
        file.sync_all().unwrap();
        assert_eq!(pass, names.len() * 4, "controlled MP4 comparison failed: {}", dir.display());
    }

    #[test]
    #[ignore = "records native planar output for modified six-channel layout tags"]
    fn six_channel_layout_variants() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let parent = PathBuf::from(std::env::var_os("PN_ALAC_ARTIFACTS").expect("PN_ALAC_ARTIFACTS"));
        let dir = parent.join(std::env::var("PN_ALAC_RUN").expect("unique PN_ALAC_RUN"));
        fs::create_dir(&dir).expect("evidence run directory must be absent");
        let original = root.join("local-artifacts/portable-parity/alac-01/native-signals-01/outputs/tone-6-4097-24-44100.m4a");
        let original_bytes = fs::read(&original).unwrap();
        let chan = original_bytes.windows(4).position(|b| b == b"chan").expect("native chan atom");
        let mut rows = Vec::new();
        for (name, tag) in [
            ("mpeg-a", Some((121u32 << 16) | 6)), ("mpeg-b", Some((122u32 << 16) | 6)),
            ("mpeg-c", Some((123u32 << 16) | 6)), ("mpeg-d", Some((124u32 << 16) | 6)),
            ("discrete", Some((147u32 << 16) | 6)), ("unknown", Some(0xffff_0006)),
            ("absent", None),
        ] {
            let mut bytes = original_bytes.clone();
            if let Some(tag) = tag { bytes[chan + 8..chan + 12].copy_from_slice(&tag.to_be_bytes()); }
            else { bytes[chan..chan + 4].copy_from_slice(b"free"); }
            let path = dir.join(format!("{name}.m4a"));
            let mut f = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
            f.write_all(&bytes).unwrap();
            f.sync_all().unwrap();
            drop(f);
            let observed = (|| -> Result<Value, String> {
                let mut interleaved = Reader::open(&path, None).map_err(|e| e.to_string())?;
                let mut samples = vec![0.0f32; 512 * 6];
                let n = interleaved.read(&mut samples).map_err(|e| e.to_string())?;
                interleaved.close().map_err(|e| e.to_string())?;
                let (portable_index, decoded) = portable_decode(&path)?;
                let interleaved_first_difference = decoded.iter().zip(&samples[..n * 6])
                    .position(|(a, b)| a.to_bits() != b.to_bits());
                let mut planar = Reader::open(&path, None).map_err(|e| e.to_string())?;
                planar.set_stereo_planar(44_100.0).map_err(|e| e.to_string())?;
                let mut left = vec![0.0; 512];
                let mut right = vec![0.0; 512];
                let m = planar.read_stereo_planar(&mut left, &mut right).map_err(|e| e.to_string())?;
                planar.close().map_err(|e| e.to_string())?;
                let matches = |plane: &[f32]| -> Vec<usize> {
                    (0..6).filter(|&channel| n == m && (0..m).all(|i| plane[i].to_bits() == samples[i * 6 + channel].to_bits())).collect()
                };
                Ok(json!({"interleaved_frames":n,"planar_frames":m,"left_matches":matches(&left),"right_matches":matches(&right),
                    "portable_layout":portable_index.channel_layout.map(|layout| json!({"tag":layout.tag,"bitmap":layout.bitmap,"descriptions":layout.descriptions})),
                    "portable_interleaved_first_difference":interleaved_first_difference,
                    "left_first_bits":format!("{:08x}",left[0].to_bits()),"right_first_bits":format!("{:08x}",right[0].to_bits())}))
            })();
            let (observation, error) = match observed { Ok(value) => (Some(value), None), Err(error) => (None, Some(error)) };
            rows.push(json!({"name":name,"requested_tag":tag,"path":path,"sha256":hash(&path),
                "observation":observation,"error":error}));
        }
        let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","source":original,"source_sha256":hash(&original),"rows":rows});
        let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
        f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap();
        f.sync_all().unwrap();
    }


    #[test]
    #[ignore = "records native stereo-planar routing for additional ALAC channel counts"]
    fn additional_channel_planar_routing() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let parent = PathBuf::from(std::env::var_os("PN_ALAC_ARTIFACTS").expect("PN_ALAC_ARTIFACTS"));
        let dir = parent.join(std::env::var("PN_ALAC_RUN").expect("unique PN_ALAC_RUN"));
        fs::create_dir(&dir).expect("evidence run directory must be absent");
        let corpus = root.join("local-artifacts/portable-parity/alac-01/native-channels-01/outputs");
        let mut rows = Vec::new();
        for channels in [3usize, 4, 5, 7, 8] {
            let path = corpus.join(format!("tone-{channels}-4097-24-44100.m4a"));
            let mut interleaved = Reader::open(&path, None).unwrap();
            let mut samples = vec![0.0f32; 512 * channels];
            let n = interleaved.read(&mut samples).unwrap();
            interleaved.close().unwrap();
            let mut planar = Reader::open(&path, None).unwrap();
            planar.set_stereo_planar(44_100.0).unwrap();
            let mut left = vec![0.0; 512];
            let mut right = vec![0.0; 512];
            let m = planar.read_stereo_planar(&mut left, &mut right).unwrap();
            planar.close().unwrap();
            let matching = |plane: &[f32]| -> Vec<usize> {
                (0..channels).filter(|&c| n == m && (0..m).all(|i| plane[i].to_bits() == samples[i * channels + c].to_bits())).collect()
            };
            let (index, decoded) = portable_decode(&path).unwrap();
            let difference = decoded.iter().zip(&samples[..n * channels]).position(|(a, b)| a.to_bits() != b.to_bits());
            rows.push(json!({"channels":channels,"path":path,"sha256":hash(&path),"frames":n,
                "layout":index.channel_layout.map(|layout| layout.tag),"left_matches":matching(&left),
                "right_matches":matching(&right),"portable_interleaved_first_difference":difference}));
        }
        let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","rows":rows});
        let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
        f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
    #[test]
    #[ignore = "compares full-width native integer-derived ALAC against AudioToolbox float output"]
    fn decode_full_width_native() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let parent = PathBuf::from(std::env::var_os("PN_ALAC_ARTIFACTS").expect("PN_ALAC_ARTIFACTS"));
        let dir = parent.join(std::env::var("PN_ALAC_RUN").expect("unique PN_ALAC_RUN"));
        fs::create_dir(&dir).expect("evidence run directory must be absent");
        let corpus = root.join("local-artifacts/portable-parity/alac-01/native-integers-01");
        let manifest = fs::read_to_string(corpus.join("manifest.tsv")).unwrap();
        let mut rows = Vec::new();
        for line in manifest.lines().skip(3) {
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(fields.len(), 6);
            assert_eq!(fields[5], "ok");
            rows.push(compare(&corpus.join(fields[4])));
        }
        let pass = rows.iter().filter(|r| r["equal"] == true).count();
        let native_drops = rows.iter().filter(|r| r["native_empty_read_error"] == true).count();
        let receipt = json!({"model":"Anthropic claude-opus-5-5 xhigh","cases":rows.len(),"passed":pass,
            "native_silent_drops":native_drops,"rows":rows});
        let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
        f.write_all(serde_json::to_string_pretty(&receipt).unwrap().as_bytes()).unwrap();
        f.sync_all().unwrap();
        assert_eq!(pass + native_drops, rows.len(), "integer-derived native decode mismatch");
    }
}
