//! WAV49 regressions against macOS ExtAudioFile float32 output. Reference
//! captures and the larger replay corpus live in portable-parity/gsm-01.
use platinum_engine::portable::audio::{CodecError, Reader, gsm::GsmDecoder};
use platinum_engine::portable::digest::sha256;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const PATTERN_HASH: &str = "49cca083d66aeb6eacae37589b79ae345f5daefe7d11280831669edd23b54047";

fn pattern() -> Vec<u8> {
    (0..4 * 65).map(|i| ((i * 73 + (i >> 3) * 19 + 37) & 255) as u8).collect()
}
fn float_hash(samples: &[f32]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect();
    sha256(&bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn decode(decoder: &mut GsmDecoder, bytes: &[u8]) -> Vec<f32> {
    let mut result = Vec::new();
    for packet in bytes.chunks_exact(65) {
        let mut samples = [0.0; 320];
        decoder.decode(packet.try_into().unwrap(), &mut samples);
        result.extend_from_slice(&samples);
    }
    result
}

#[test]
fn native_packet_vectors_and_state_continuity() {
    // Each reference covers all 320 raw float32 words, not just a few samples.
    for (byte, hash) in [
        (0x00, "9c366981232e18ff7a1171cdbec14efec170da2f2fc405d171d5947448ea7366"),
        (0x55, "cde87214b8d94bc95a21708eafd5acdd8aadcb084501b41a32d9d2c07bfc4664"),
        (0xaa, "ed44a9f857a466469d33dff1f8aa7c445078883ac5adaa2e35c0f45e11b8d97a"),
        (0xff, "ddb559558c68c874587d08d5a166607854b3b4993ab8f08496309ec692e9131e"),
    ] {
        assert_eq!(float_hash(&decode(&mut GsmDecoder::new(), &[byte; 65])), hash);
    }
    let data = pattern();
    let all = decode(&mut GsmDecoder::new(), &data);
    assert_eq!(all.len(), 1280);
    assert_eq!(float_hash(&all), PATTERN_HASH);

    let mut decoder = GsmDecoder::default();
    assert_eq!(decode(&mut decoder, &data[..65]), all[..320]);
    let mut copy = decoder.clone();
    let next = decode(&mut decoder, &data[65..130]);
    assert_eq!(next, all[320..640]);
    assert_eq!(decode(&mut copy, &data[65..130]), next);
    assert_ne!(decode(&mut GsmDecoder::new(), &data[65..130]), next);
}

fn wav(rate: u32, channels: u16, data: &[u8]) -> Vec<u8> {
    let size = data.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF"); out.extend_from_slice(&(52 + size + (size & 1)).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt "); out.extend_from_slice(&20u32.to_le_bytes());
    out.extend_from_slice(&0x31u16.to_le_bytes()); out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes()); out.extend_from_slice(&(rate * 65 / 320).to_le_bytes());
    out.extend_from_slice(&65u16.to_le_bytes()); out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); out.extend_from_slice(&320u16.to_le_bytes());
    out.extend_from_slice(b"fact"); out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&9999u32.to_le_bytes());
    out.extend_from_slice(b"data"); out.extend_from_slice(&size.to_le_bytes()); out.extend_from_slice(data);
    if size & 1 != 0 { out.push(0); }
    out
}
struct TempFile(PathBuf);
impl TempFile {
    fn new(bytes: &[u8]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("platinum-gsm-{}-{id}.wav", std::process::id()));
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        file.write_all(bytes).unwrap();
        Self(path)
    }
}
impl Drop for TempFile { fn drop(&mut self) { let _ = fs::remove_file(&self.0); } }

#[test]
fn reader_matches_native_across_rates_tails_and_read_sizes() {
    for rate in [8000, 22050, 44100] {
        for tail in [0, 1, 64] {
            let mut data = pattern();
            data.extend((0..tail).map(|i| ((i * 19 + 3) & 255) as u8));
            let file = TempFile::new(&wav(rate, 1, &data));
            for block_size in [1, 13, 257, 320, 511] {
                let mut reader = Reader::open(&file.0, Some(f64::from(rate))).unwrap();
                let format = reader.source_format();
                assert_eq!((format.format_id, format.bytes_per_packet, format.frames_per_packet, format.channels_per_frame),
                    (0x6d73_0031, 65, 320, 1));
                assert_eq!(reader.file_length_frames().unwrap(), 1280);
                let mut samples = Vec::new();
                let mut block = vec![f32::from_bits(0x7fc1_2345); block_size];
                loop {
                    block.fill(f32::from_bits(0x7fc1_2345));
                    let count = reader.read(&mut block).unwrap();
                    assert!(block[count..].iter().all(|v| v.to_bits() == 0x7fc1_2345));
                    samples.extend_from_slice(&block[..count]);
                    if count == 0 { break; }
                }
                assert_eq!(samples.len(), 1280);
                assert_eq!(float_hash(&samples), PATTERN_HASH, "rate={rate} tail={tail} read={block_size}");
                reader.close().unwrap();
            }
            let mut reader = Reader::open(&file.0, None).unwrap();
            reader.set_stereo_planar(f64::from(rate)).unwrap();
            let mut left = [0.0; 257]; let mut right = [0.0; 257];
            let mut samples = Vec::new();
            loop {
                let count = reader.read_stereo_planar(&mut left, &mut right).unwrap();
                assert_eq!(left[..count], right[..count]);
                samples.extend_from_slice(&left[..count]);
                if count == 0 { break; }
            }
            assert_eq!(float_hash(&samples), PATTERN_HASH);
        }
    }
}

#[test]
fn reader_normalizes_native_gsm_header_geometry() {
    // AudioToolbox normalizes each of these header variations to 65-byte,
    // 320-frame mono packets. The native captures include the entire output.
    let variants = [
        (20, 0, 0, 2, 320), (20, 1, 0, 2, 320), (20, 66, 0, 2, 320),
        (20, 130, 0, 2, 320), (20, 255, 0, 2, 320),
        (20, 65, 1, 2, 320), (20, 65, 16, 2, 320), (20, 65, 32, 2, 320),
        (20, 65, 0, 0, 320), (20, 65, 0, 1, 320), (20, 65, 0, 65535, 320),
        (20, 65, 0, 2, 0), (20, 65, 0, 2, 160), (20, 65, 0, 2, 640),
        (16, 65, 0, 2, 320), (18, 65, 0, 2, 320),
    ];
    for (fmt_len, align, bits, cb, spb) in variants {
        let mut data = wav(8000, 1, &pattern());
        data[32..34].copy_from_slice(&u16::to_le_bytes(align));
        data[34..36].copy_from_slice(&u16::to_le_bytes(bits));
        data[36..38].copy_from_slice(&u16::to_le_bytes(cb));
        data[38..40].copy_from_slice(&u16::to_le_bytes(spb));
        if fmt_len < 20 {
            data.drain(20 + fmt_len..40);
            data[16..20].copy_from_slice(&(fmt_len as u32).to_le_bytes());
            let riff_size = (data.len() - 8) as u32;
            data[4..8].copy_from_slice(&riff_size.to_le_bytes());
        }
        let file = TempFile::new(&data);
        let mut reader = Reader::open(&file.0, None).unwrap();
        let source = reader.source_format();
        assert_eq!((source.format_id, source.bytes_per_packet, source.frames_per_packet,
                    source.bytes_per_frame, source.channels_per_frame, source.bits_per_channel),
                   (0x6d73_0031, 65, 320, 0, 1, 0));
        assert_eq!(reader.file_length_frames().unwrap(), 1280);
        assert_eq!(reader.bitrate().unwrap(), 13000);
        let mut output = [0.0; 1280];
        assert_eq!(reader.read(&mut output).unwrap(), 1280);
        assert_eq!(float_hash(&output), PATTERN_HASH, "fmt={fmt_len} align={align} bits={bits} cb={cb} spb={spb}");
    }
}

fn extensible_wav(bits: u16, valid: u16, mask: u32) -> Vec<u8> {
    let mut data = wav(8000, 1, &pattern());
    data.splice(40..40, [0; 20]);
    data[16..20].copy_from_slice(&40u32.to_le_bytes());
    data[20..22].copy_from_slice(&0xfffeu16.to_le_bytes());
    data[34..36].copy_from_slice(&bits.to_le_bytes());
    data[36..38].copy_from_slice(&22u16.to_le_bytes());
    data[38..40].copy_from_slice(&valid.to_le_bytes());
    data[40..44].copy_from_slice(&mask.to_le_bytes());
    data[44..60].copy_from_slice(&[0x31, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
    let riff_size = (data.len() - 8) as u32;
    data[4..8].copy_from_slice(&riff_size.to_le_bytes());
    data
}

#[test]
fn extensible_gsm_matches_native_or_refuses_unverified_masks() {
    for bits in [8, 16, 24, 64, 72, 65528] {
        for mask in [0, 1, 2, 3, 4, 8, 0x3ffff, 0x8000_0000, 0xc5cf_7340, 0xffff_ffff] {
            let file = TempFile::new(&extensible_wav(bits, 320, mask));
            let mut reader = Reader::open(&file.0, None).unwrap();
            let source = reader.source_format();
            assert_eq!((source.format_id, source.bytes_per_packet, source.frames_per_packet, source.bits_per_channel),
                (0x6d73_0031, 65, 320, 0));
            assert_eq!(reader.file_length_frames().unwrap(), 1280);
            assert_eq!(reader.bitrate().unwrap(), 13000);
            let mut output = [0.0; 1280];
            assert_eq!(reader.read(&mut output).unwrap(), 1280);
            assert_eq!(float_hash(&output), PATTERN_HASH, "bits={bits} mask={mask:#010x}");
        }
    }
    // Apple accepts 0x001fffff and 0x60000000 but their wider mask class
    // contains native rejections, so the portable path deliberately refuses.
    for mask in [0x0004_0000, 0x001f_ffff, 0x6000_0000] {
        let file = TempFile::new(&extensible_wav(8, 0, mask));
        assert!(matches!(Reader::open(&file.0, None), Err(CodecError::UnsupportedFormat(_))));
    }
}

#[test]
fn incomplete_blocks_and_stereo_are_not_decoded() {
    for size in [0, 1, 31, 32, 64] {
        let file = TempFile::new(&wav(8000, 1, &vec![0x5a; size]));
        let mut reader = Reader::open(&file.0, None).unwrap();
        assert_eq!(reader.file_length_frames().unwrap(), 0);
        let mut output = [7.0; 4];
        assert_eq!(reader.read(&mut output).unwrap(), 0);
        assert_eq!(output, [7.0; 4]);
    }
    for rate in [8000, 22050, 44100] {
        for channels in [0, 2, 3, 6] {
            let file = TempFile::new(&wav(rate, channels, &pattern()));
            assert!(Reader::open(&file.0, None).is_err());
        }
    }
}
