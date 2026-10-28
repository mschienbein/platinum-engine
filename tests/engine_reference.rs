//! Isolated generated fixtures for the streaming engine. These do not establish
//! byte identity against the original application. Native comparisons belong
//! to the parent-controlled reference capture and complete-file SHA-256 gate.

use platinum_engine::engine::{analyze_file, inspect_file, prepare_file, process_file, read_restore_marker};
use platinum_engine::file_workflow::{self, AudioFormat, Cancellation, OutputMode};
use platinum_engine::settings::{Settings, TemplateRef};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        loop {
            let p = std::env::temp_dir().join(format!("platinum-engine-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&p) {
                Ok(()) => return Self(p),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("cannot create fixture: {e}"),
            }
        }
    }
    fn input(&self, name: &str, frames: u32, channels: u16, malformed_marker: bool) -> PathBuf {
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF"); data.extend_from_slice(&0u32.to_le_bytes()); data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes()); data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&channels.to_le_bytes()); data.extend_from_slice(&48_000u32.to_le_bytes());
        data.extend_from_slice(&(48_000u32 * u32::from(channels) * 2).to_le_bytes());
        data.extend_from_slice(&(channels * 2).to_le_bytes()); data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data"); data.extend_from_slice(&(frames * u32::from(channels) * 2).to_le_bytes());
        for i in 0..frames {
            let sample = ((i as f64 * (std::f64::consts::TAU * 437.0 / 48_000.0)).sin() * 20_000.0) as i16;
            for channel in 0..channels { data.extend_from_slice(&(if channel == 0 { sample } else { sample / 2 }).to_le_bytes()); }
        }
        if malformed_marker { data.extend_from_slice(b"PLTN\x04\x00\x00\x00!!!!"); }
        let length = u32::try_from(data.len() - 8).unwrap(); data[4..8].copy_from_slice(&length.to_le_bytes());
        let path = self.0.join(name); fs::write(&path, data).unwrap(); path
    }
    fn input_with_marker(&self, name: &str, payload: &[u8]) -> PathBuf {
        let path = self.input(name, 48_513, 2, false);
        let mut data = fs::read(&path).unwrap();
        data.extend_from_slice(&platinum_engine::metadata::wav_pltn_chunk(Some(payload)).unwrap());
        let length = u32::try_from(data.len() - 8).unwrap();
        data[4..8].copy_from_slice(&length.to_le_bytes());
        fs::write(&path, data).unwrap();
        path
    }
    fn settings(&self) -> Settings {
        let mut s = Settings::defaults(&self.0, 1);
        s.output.mode = OutputMode::SpecificFolder { directory: self.0.clone(), mirror: false };
        s.output.format = AudioFormat::Wav;
        s.libraries.tag_bridge = Some(PathBuf::from(std::env::var_os("PN_TAG_BRIDGE").expect("set PN_TAG_BRIDGE to trusted ABI-5 public TagLib bridge")));
        s
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
fn discovered(path: &Path) -> file_workflow::DiscoveredFile {
    file_workflow::discover_inputs(&[path.to_owned()]).unwrap().remove(0)
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn embedded_mpeg_sync_does_not_hide_misnamed_wav_or_aiff_markers() {
    use platinum_engine::metadata::{Marker, encode_marker_base64, wav_pltn_chunk};
    use platinum_engine::tags::{Format, OutputChoice, TagBridge};

    let f = Fixture::new(); let mut s = f.settings();
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    let source = f.input("source.wav", 48_513, 2, false);
    let mut wave = fs::read(&source).unwrap();
    // The MP3 bridge accepts this complete-looking MPEG frame inside PCM.
    let wave_sync = wave.len() - 1024;
    wave[wave_sync..wave_sync + 4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    wave[wave_sync + 4..wave_sync + 417].fill(0);
    fs::write(&source, &wave).unwrap();
    let marker = Marker::from_encoded_bytes(&encode_marker_base64(br#"{"isProcessed":true}"#).unwrap()).unwrap();
    let mut marked_wave = wave.clone();
    marked_wave.extend_from_slice(&wav_pltn_chunk(marker.encoded_bytes().unwrap().as_deref()).unwrap());
    let wave_size = u32::try_from(marked_wave.len() - 8).unwrap();
    marked_wave[4..8].copy_from_slice(&wave_size.to_le_bytes());

    let frames = 48_513u32;
    let mut aiff = b"FORM\0\0\0\0AIFFCOMM\0\0\0\x12".to_vec();
    aiff.extend_from_slice(&2u16.to_be_bytes());
    aiff.extend_from_slice(&frames.to_be_bytes());
    aiff.extend_from_slice(&16u16.to_be_bytes());
    aiff.extend_from_slice(&[0x40,0x0e,0xbb,0x80,0,0,0,0,0,0]);
    aiff.extend_from_slice(b"SSND");
    aiff.extend_from_slice(&(8 + frames * 4).to_be_bytes());
    aiff.extend_from_slice(&[0; 8]);
    for sample in wave[44..].chunks_exact(2) { aiff.extend_from_slice(&[sample[1], sample[0]]); }
    let aiff_sync = aiff.len() - 1024;
    aiff[aiff_sync..aiff_sync + 4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    aiff[aiff_sync + 4..aiff_sync + 417].fill(0);
    let aiff_size = u32::try_from(aiff.len() - 8).unwrap();
    aiff[4..8].copy_from_slice(&aiff_size.to_be_bytes());
    let marked_aiff_path = f.0.join("marked.aif");
    fs::write(&marked_aiff_path, &aiff).unwrap();
    let transfer = bridge.transfer(&source, &marked_aiff_path, Format::Wav, Format::Aiff, OutputChoice::Explicit, &marker).unwrap();
    assert!(transfer.is_success(), "{transfer:?}");
    let marked_aiff = fs::read(&marked_aiff_path).unwrap();
    let backup = f.0.join("backups"); fs::create_dir(&backup).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    for (label, actual, unmarked, marked) in [
        ("wave", Format::Wav, wave.as_slice(), marked_wave.as_slice()),
        ("aiff", Format::Aiff, aiff.as_slice(), marked_aiff.as_slice()),
    ] {
        for (is_marked, bytes) in [(false, unmarked), (true, marked)] {
            let input = f.0.join(format!("{label}-{is_marked}.mp3"));
            fs::write(&input, bytes).unwrap();
            assert!(bridge.inspect(&input, Format::Mp3).is_ok(), "{label} {is_marked}");
            assert!(bridge.read_processing_marker(&input, Format::Mp3).unwrap().is_none(), "{label} {is_marked}");
            assert_eq!(bridge.read_processing_marker(&input, actual).unwrap().is_some(), is_marked);
            let inspected = inspect_file(&input, &s);
            #[cfg(all(target_os = "macos", not(feature = "portable")))]
            if let Err(error) = &inspected {
                // AudioToolbox can reject a .mp3-named PCM container entirely;
                // in that case no output is planned or published.
                assert_eq!(error.stage, "open input", "{label} {is_marked}: {error}");
                for mode in [
                    OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false },
                    OutputMode::Replace { backup_directory: backup.clone() },
                ] {
                    s.output.mode = mode;
                    for allow in [false, true] {
                        assert_eq!(prepare_file(&discovered(&input), &s, allow).unwrap_err().stage, "open input");
                    }
                }
                assert_eq!(fs::read(&input).unwrap(), bytes);
                continue;
            }
            let report = inspected.unwrap_or_else(|error| panic!("{label} {is_marked}: {error}"));
            assert!(report.tags.is_some() && report.tags_unavailable.is_none());
            assert_eq!(report.processed, Some(is_marked));
            if is_marked {
                for mode in [
                    OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false },
                    OutputMode::Replace { backup_directory: backup.clone() },
                ] {
                    s.output.mode = mode;
                    for allow in [false, true] {
                        let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
                        assert_eq!(error.stage, "plan output");
                        assert!(error.message.contains("already processed"));
                    }
                }
            } else {
                s.output.mode = OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false };
                let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
                let result = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
                assert!(result.source_tags_unavailable.is_none());
                assert_eq!(result.processing_marker_status, "present");
                assert_eq!(inspect_file(&result.output_path, &s).unwrap().processed, Some(true));
            }
            assert_eq!(fs::read(&input).unwrap(), bytes);
        }
    }
    assert_eq!(fs::read_dir(&backup).unwrap().count(), 0);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn aiff_duplicate_id3_chunks_never_hide_a_later_processed_marker() {
    use platinum_engine::apple_codec::{OutputFormat, Writer, WriterConfig};
    use platinum_engine::metadata::{Marker, encode_marker_base64};
    use platinum_engine::tags::{Format, OutputChoice, TagBridge};

    let f = Fixture::new(); let mut s = f.settings();
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    let wav = f.input("tag-source.wav", 48_513, 2, false);
    let aiff = f.0.join("marked-source.aif");
    let mut writer = Writer::create(&aiff, WriterConfig { format: OutputFormat::Aiff, sample_rate: 48_000.0, channels: 2, bit_depth: 16 }).unwrap();
    let samples: Vec<f32> = (0..48_513).flat_map(|i| {
        let value = (i as f32 * std::f32::consts::TAU * 437.0 / 48_000.0).sin() * 0.5;
        [value, value * 0.5]
    }).collect();
    writer.write(&samples).unwrap();
    writer.finish().unwrap();
    let marker = Marker::from_encoded_bytes(&encode_marker_base64(br#"{"isProcessed":true}"#).unwrap()).unwrap();
    let transfer = bridge.transfer(&wav, &aiff, Format::Wav, Format::Aiff, OutputChoice::Explicit, &marker).unwrap();
    assert!(transfer.is_success(), "{transfer:?}");
    let original = fs::read(&aiff).unwrap();
    let mut position = 12usize;
    while position + 8 <= original.len() && !matches!(&original[position..position + 4], b"ID3 " | b"id3 ") {
        let size = u32::from_be_bytes(original[position + 4..position + 8].try_into().unwrap()) as usize;
        position += 8 + size + (size & 1);
    }
    assert!(position + 8 <= original.len(), "marked AIFF must contain an ID3 chunk");
    let backup = f.0.join("duplicate-backups"); fs::create_dir(&backup).unwrap();
    for (first, second) in [(b"ID3 ", b"id3 "), (b"id3 ", b"ID3 ")] {
        for trailing in [false, true] {
            let mut bytes = original.clone();
            bytes[position..position + 4].copy_from_slice(second);
            let mut empty = first.to_vec();
            empty.extend_from_slice(&10u32.to_be_bytes());
            empty.extend_from_slice(b"ID3\x04\0\0\0\0\0\0");
            bytes.splice(position..position, empty);
            let declared = if trailing { position + 18 - 8 } else { bytes.len() - 8 };
            bytes[4..8].copy_from_slice(&u32::try_from(declared).unwrap().to_be_bytes());
            for extension in ["aif", "wav"] {
                let input = f.0.join(format!("duplicate-{}-{}-{trailing}.{extension}",
                    std::str::from_utf8(first).unwrap().trim(), std::str::from_utf8(second).unwrap().trim()));
                fs::write(&input, &bytes).unwrap();
                assert!(bridge.read_processing_marker(&input, Format::Aiff).unwrap().is_none());
                assert_eq!(inspect_file(&input, &s).unwrap_err().stage, "decode marker");
                if extension == "aif" {
                    assert_eq!(read_restore_marker(&input, &s).unwrap_err().stage, "decode marker");
                }
                for mode in [
                    OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false },
                    OutputMode::Replace { backup_directory: backup.clone() },
                ] {
                    s.output.mode = mode;
                    for allow in [false, true] {
                        let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
                        assert_eq!(error.stage, "decode marker");
                        assert!(error.message.contains("multiple AIFF ID3"));
                    }
                }
                assert_eq!(fs::read(&input).unwrap(), bytes);
            }
        }
    }
    assert_eq!(fs::read_dir(backup).unwrap().count(), 0);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn misnamed_mp4_leading_atoms_preserve_marker_detection() {
    use platinum_engine::apple_codec::{OutputFormat, Writer, WriterConfig};
    use platinum_engine::metadata::{Marker, encode_marker_base64};
    use platinum_engine::tags::{Format, OutputChoice, TagBridge};

    fn rearrange(bytes: &[u8], first: &[u8; 4]) -> Vec<u8> {
        let mut atoms: Vec<([u8; 4], usize, Vec<u8>)> = Vec::new();
        let mut position = 0usize;
        while position < bytes.len() {
            assert!(bytes.len() - position >= 8);
            let short = u32::from_be_bytes(bytes[position..position + 4].try_into().unwrap());
            let size = match short {
                0 => bytes.len() - position,
                1 => usize::try_from(u64::from_be_bytes(bytes[position + 8..position + 16].try_into().unwrap())).unwrap(),
                n => n as usize,
            };
            assert!(size >= 8 && size <= bytes.len() - position);
            atoms.push((bytes[position + 4..position + 8].try_into().unwrap(), position, bytes[position..position + size].to_vec()));
            position += size;
        }
        let media: Vec<_> = atoms.iter().filter(|atom| &atom.0 == b"mdat").collect();
        assert_eq!(media.len(), 1);
        let old_media = media[0].1;
        let media_size = media[0].2.len();
        if first == b"moov" || first == b"mdat" {
            let index = atoms.iter().position(|atom| &atom.0 == first).unwrap();
            let atom = atoms.remove(index); atoms.insert(0, atom);
        }
        let mut result = Vec::new();
        if first != b"moov" && first != b"mdat" {
            result.extend_from_slice(&8u32.to_be_bytes()); result.extend_from_slice(first);
        }
        let mut new_media = None;
        let mut movie = None;
        for (kind, _, atom) in atoms {
            if &kind == b"mdat" { new_media = Some(result.len()); }
            if &kind == b"moov" { movie = Some((result.len(), atom.len())); }
            result.extend(atom);
        }
        let new_media = new_media.unwrap();
        let (movie_start, movie_size) = movie.unwrap();
        let matches: Vec<_> = result[movie_start..movie_start + movie_size].windows(4).enumerate()
            .filter(|(_, value)| *value == b"stco" || *value == b"co64")
            .map(|(index, _)| movie_start + index).collect();
        assert_eq!(matches.len(), 1);
        let table = matches[0];
        let is_64 = &result[table..table + 4] == b"co64";
        let count = u32::from_be_bytes(result[table + 8..table + 12].try_into().unwrap()) as usize;
        let width = if is_64 { 8 } else { 4 };
        for i in 0..count {
            let at = table + 12 + i * width;
            let offset = if is_64 { u64::from_be_bytes(result[at..at + 8].try_into().unwrap()) }
                else { u64::from(u32::from_be_bytes(result[at..at + 4].try_into().unwrap())) };
            assert!(offset >= old_media as u64 && offset < (old_media + media_size) as u64);
            let adjusted = offset - old_media as u64 + new_media as u64;
            if is_64 { result[at..at + 8].copy_from_slice(&adjusted.to_be_bytes()); }
            else { result[at..at + 4].copy_from_slice(&u32::try_from(adjusted).unwrap().to_be_bytes()); }
        }
        result
    }

    let f = Fixture::new(); let mut s = f.settings();
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    let wav = f.input("source.wav", 48_513, 2, false);
    let mp4 = f.0.join("unmarked.m4a");
    let mut writer = Writer::create(&mp4, WriterConfig { format: OutputFormat::Alac, sample_rate: 48_000.0, channels: 2, bit_depth: 16 }).unwrap();
    let samples: Vec<f32> = (0..48_513).flat_map(|i| {
        let value = (i as f32 * std::f32::consts::TAU * 437.0 / 48_000.0).sin() * 0.5;
        [value, value * 0.5]
    }).collect();
    writer.write(&samples).unwrap(); writer.finish().unwrap();
    let unmarked = fs::read(&mp4).unwrap();
    assert!(bridge.read_processing_marker(&mp4, Format::Mp4).unwrap().is_none());
    let marked_path = f.0.join("marked.m4a"); fs::copy(&mp4, &marked_path).unwrap();
    let marker = Marker::from_encoded_bytes(&encode_marker_base64(br#"{"isProcessed":true}"#).unwrap()).unwrap();
    let transfer = bridge.transfer(&wav, &marked_path, Format::Wav, Format::Mp4, OutputChoice::Explicit, &marker).unwrap();
    assert!(transfer.is_success(), "{transfer:?}");
    let marked = fs::read(&marked_path).unwrap();
    let backup = f.0.join("mp4-backups"); fs::create_dir(&backup).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    for first in [b"free", b"wide", b"skip", b"moov", b"mdat"] {
        for extension in ["wav", "aif"] {
            for (is_marked, original) in [(false, &unmarked), (true, &marked)] {
                let bytes = rearrange(original, first);
                let label = std::str::from_utf8(first).unwrap();
                let input = f.0.join(format!("{label}-{extension}-{is_marked}.{extension}"));
                fs::write(&input, &bytes).unwrap();
                assert_eq!(bridge.read_processing_marker(&input, Format::Mp4).unwrap().is_some(), is_marked, "{label} {extension}");
                let inspected = inspect_file(&input, &s);
                if let Err(error) = &inspected {
                    // Some platform readers refuse a layout they cannot decode;
                    // the marker path must still fail closed before publication.
                    #[cfg(any(target_os = "linux", feature = "portable"))]
                    assert_eq!(first, b"skip", "the portable decoder admits the other fixture layouts");
                    assert_eq!(error.stage, "open input", "{label} {extension}: {error}");
                    assert_eq!(prepare_file(&discovered(&input), &s, false).unwrap_err().stage, "open input");
                    assert_eq!(fs::read(&input).unwrap(), bytes);
                    continue;
                }
                let report = inspected.unwrap();
                assert_eq!(report.processed, Some(is_marked), "{label} {extension}");
                assert!(report.tags.is_none() && report.tags_unavailable.is_some());
                if is_marked {
                    for mode in [
                        OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false },
                        OutputMode::Replace { backup_directory: backup.clone() },
                    ] {
                        s.output.mode = mode;
                        for allow in [false, true] {
                            let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
                            assert_eq!(error.stage, "plan output");
                            assert!(error.message.contains("already processed"));
                        }
                    }
                } else {
                    s.output.mode = OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false };
                    let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
                    let result = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
                    assert!(result.source_tags_unavailable.is_some());
                    assert_eq!(result.processing_marker_status, "present");
                    assert_eq!(inspect_file(&result.output_path, &s).unwrap().processed, Some(true));
                }
                assert_eq!(fs::read(&input).unwrap(), bytes);
            }
        }
    }
    assert_eq!(fs::read_dir(backup).unwrap().count(), 0);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn unknown_source_container_is_refused_when_tag_reader_is_unavailable() {
    let f = Fixture::new(); let s = f.settings();
    let input = f.0.join("unknown.wav");
    let bytes = b"not a recognized audio container";
    fs::write(&input, bytes).unwrap();
    let error = inspect_file(&input, &s).unwrap_err();
    assert_eq!(error.stage, "inspect tags");
    assert!(error.message.contains("unknown container"));
    for allow in [false, true] {
        let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
        assert_eq!(error.stage, "inspect tags");
        assert!(error.message.contains("unknown container"));
    }
    assert_eq!(fs::read(input).unwrap(), bytes);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_FLAC_LIB, PN_FLAC_SHA256 and source-container status support"]
fn id3_prefixed_flac_uses_flac_marker_not_accidental_mpeg_frame() {
    use platinum_engine::tags::{Format, TagBridge};

    let f = Fixture::new(); let mut s = f.settings();
    s.libraries.flac = Some(PathBuf::from(std::env::var_os("PN_FLAC_LIB").expect("trusted public FLAC")));
    s.libraries.flac_sha256 = Some(std::env::var("PN_FLAC_SHA256").expect("FLAC SHA-256 pin"));
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    let backup = f.0.join("flac-backups"); fs::create_dir(&backup).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    for is_marked in [false, true] {
        let source = f.input(if is_marked { "tone.wav" } else { "silence.wav" }, 48_513, 2, false);
        if !is_marked {
            let mut bytes = fs::read(&source).unwrap();
            bytes[44..].fill(0);
            fs::write(&source, bytes).unwrap();
        }
        s.output.mode = OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false };
        s.output.format = AudioFormat::Flac;
        let prepared = prepare_file(&discovered(&source), &s, false).unwrap();
        let result = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
        let flac = fs::read(&result.output_path).unwrap();
        assert_eq!(&flac[..4], b"fLaC");
        assert_eq!(flac[4] & 0x7f, 0);
        assert_eq!(flac[4] & 0x80, 0, "fixture needs another metadata block");
        let info_len = (usize::from(flac[5]) << 16) | (usize::from(flac[6]) << 8) | usize::from(flac[7]);
        let info_end = 8 + info_len;
        let mut padded = flac[..info_end].to_vec();
        padded.extend_from_slice(&[1, 0, 1, 0xa1]); // 417-byte FLAC padding block
        padded.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        padded.extend_from_slice(&[0; 413]);
        padded.extend_from_slice(&flac[info_end..]);
        for prefix_count in [1, 2] {
            let mut bytes = Vec::new();
            for _ in 0..prefix_count { bytes.extend_from_slice(b"ID3\x04\0\0\0\0\0\0"); }
            bytes.extend_from_slice(&padded);
            for extension in ["flac", "mp3"] {
                let input = f.0.join(format!("id3-{prefix_count}-{is_marked}-{extension}.{extension}"));
                fs::write(&input, &bytes).unwrap();
                assert!(bridge.inspect(&input, Format::Mp3).is_ok(), "fixture must fool the MPEG parser");
                assert!(bridge.read_processing_marker(&input, Format::Mp3).unwrap().is_none());
                assert_eq!(bridge.read_processing_marker(&input, Format::Flac).unwrap().is_some(), is_marked);
                s.output.format = AudioFormat::Wav;
                s.output.mode = OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false };
                let inspected = inspect_file(&input, &s);
                if let Err(error) = &inspected {
                    // A platform's decoder may refuse ID3-prefixed FLAC even
                    // though its marker reader could identify the container.
                    assert!(extension != "flac" || prefix_count != 1, "single-prefix FLAC must be readable");
                    #[cfg(all(target_os = "macos", not(feature = "portable")))]
                    assert!(extension == "flac" && prefix_count == 2, "the remaining macOS fixtures must be readable");
                    assert_eq!(error.stage, "open input", "{extension} {prefix_count} {is_marked}: {error}");
                    assert_eq!(prepare_file(&discovered(&input), &s, false).unwrap_err().stage, "open input");
                } else {
                    let report = inspected.unwrap();
                    assert_eq!(report.processed, Some(is_marked), "{extension} {prefix_count}");
                    if is_marked {
                        for mode in [
                            OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false },
                            OutputMode::Replace { backup_directory: backup.clone() },
                        ] {
                            s.output.mode = mode;
                            for allow in [false, true] {
                                let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
                                assert_eq!(error.stage, "plan output");
                                assert!(error.message.contains("already processed"));
                            }
                        }
                    } else {
                        let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
                        let result = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
                        assert_eq!(result.processing_marker_status, "nil_payload_empty_wav_chunk");
                    }
                }
                assert_eq!(fs::read(&input).unwrap(), bytes);
            }
        }
        if !is_marked {
            let payload = platinum_engine::metadata::encode_marker_base64(br#"{"isProcessed":true}"#).unwrap();
            let mut body = b"\0application/json\0\0PlatinumNotes\0".to_vec(); body.extend_from_slice(&payload);
            assert!(body.len() < 128 && body.len() + 10 < 128);
            let mut second = b"ID3\x04\0\0\0\0\0".to_vec(); second.push((body.len() + 10) as u8);
            second.extend_from_slice(b"GEOB\0\0\0"); second.push(body.len() as u8);
            second.extend_from_slice(&[0, 0]); second.extend_from_slice(&body);
            let mut bytes = b"ID3\x04\0\0\0\0\0\0".to_vec(); bytes.extend_from_slice(&second); bytes.extend_from_slice(&padded);
            let input = f.0.join("hidden-second-id3.flac"); fs::write(&input, &bytes).unwrap();
            assert!(bridge.read_processing_marker(&input, Format::Flac).unwrap().is_none(), "TagLib ignores the second ID3 tag");
            let error = prepare_file(&discovered(&input), &s, false).unwrap_err();
            assert_eq!(error.stage, "decode marker");
            assert!(error.message.contains("additional nonempty leading ID3"));
            assert_eq!(fs::read(input).unwrap(), bytes);
        }
    }
    assert_eq!(fs::read_dir(backup).unwrap().count(), 0);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn repeated_id3_and_unknown_mpeg_sources_fail_closed() {
    use platinum_engine::metadata::encode_marker_base64;
    use platinum_engine::tags::{Format, TagBridge};

    fn synchsafe(n: usize) -> [u8; 4] {
        [((n >> 21) & 127) as u8, ((n >> 14) & 127) as u8, ((n >> 7) & 127) as u8, (n & 127) as u8]
    }
    let f = Fixture::new(); let s = f.settings();
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    let payload = encode_marker_base64(br#"{"isProcessed":true}"#).unwrap();
    let mut body = b"\0application/json\0\0PlatinumNotes\0".to_vec(); body.extend_from_slice(&payload);
    let mut frame = b"GEOB".to_vec(); frame.extend_from_slice(&synchsafe(body.len())); frame.extend_from_slice(&[0, 0]); frame.extend_from_slice(&body);
    let mut second = b"ID3\x04\0\0".to_vec(); second.extend_from_slice(&synchsafe(frame.len())); second.extend_from_slice(&frame);
    let mut bytes = b"ID3\x04\0\0\0\0\0\0".to_vec(); bytes.extend_from_slice(&second);
    for _ in 0..3 { bytes.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]); bytes.extend_from_slice(&[0; 413]); }
    let input = f.0.join("duplicate.mp3"); fs::write(&input, &bytes).unwrap();
    assert!(bridge.inspect(&input, Format::Mp3).is_ok());
    assert!(bridge.read_processing_marker(&input, Format::Mp3).unwrap().is_none(), "TagLib reads only the first ID3 tag");
    assert!(inspect_file(&input, &s).unwrap_err().message.contains("additional nonempty leading ID3"));
    for allow in [false, true] {
        let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
        assert_eq!(error.stage, "decode marker");
        assert!(error.message.contains("additional nonempty leading ID3"));
    }
    assert_eq!(fs::read(&input).unwrap(), bytes);

    let unknown = f.0.join("unknown.mp3");
    let mut unknown_bytes = b"unknown!".to_vec();
    unknown_bytes.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]); unknown_bytes.extend_from_slice(&[0; 413]);
    fs::write(&unknown, &unknown_bytes).unwrap();
    assert!(bridge.inspect(&unknown, Format::Mp3).is_ok());
    let error = inspect_file(&unknown, &s).unwrap_err();
    assert_eq!(error.stage, "inspect tags");
    assert!(error.message.contains("unknown container"));
    assert_eq!(fs::read(&unknown).unwrap(), unknown_bytes);
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_LAME_LIB and PN_LAME_SHA256"]
fn mp3_with_leading_padding_junk_or_post_id3_junk_remains_admitted() {
    use platinum_engine::tags::{Format, TagBridge};

    let f = Fixture::new(); let mut s = f.settings();
    s.libraries.lame = Some(PathBuf::from(std::env::var_os("PN_LAME_LIB").expect("trusted public LAME")));
    s.libraries.lame_sha256 = Some(std::env::var("PN_LAME_SHA256").expect("LAME SHA-256 pin"));
    let source = f.input("mpeg-source.wav", 96_000, 2, false);
    let template = s.template(TemplateRef::Index(0)).unwrap();
    s.output.format = AudioFormat::Mp3;
    let prepared = prepare_file(&discovered(&source), &s, false).unwrap();
    let encoded = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
    let original = fs::read(&encoded.output_path).unwrap();
    assert_eq!(&original[..3], b"ID3");
    let size = original[6..10].iter().fold(0usize, |n, byte| (n << 7) | usize::from(*byte));
    assert_eq!(original[5] & 0x10, 0);
    assert!(10 + size < original.len());
    let tag = &original[..10 + size]; let audio = &original[10 + size..];
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    s.output.format = AudioFormat::Wav;
    let backup = f.0.join("mpeg-backups"); fs::create_dir(&backup).unwrap();
    for (name, prefix, marked) in [
        ("zeros", vec![0; 31], false),
        ("junk", b"leading-junk".to_vec(), false),
        ("id3-junk", b"ID3\x04\0\0\0\0\0\0post-id3-junk".to_vec(), false),
        ("marked-id3-junk", [tag, b"post-id3-junk"].concat(), true),
    ] {
        let input = f.0.join(format!("{name}.mp3"));
        let mut bytes = prefix; bytes.extend_from_slice(audio);
        fs::write(&input, &bytes).unwrap();
        assert!(bridge.inspect(&input, Format::Mp3).is_ok(), "{name}");
        assert_eq!(bridge.read_processing_marker(&input, Format::Mp3).unwrap().is_some(), marked, "{name}");
        let info = inspect_file(&input, &s).unwrap();
        assert_eq!(info.processed, Some(marked), "{name}");
        if marked {
            for mode in [
                OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false },
                OutputMode::Replace { backup_directory: backup.clone() },
            ] {
                s.output.mode = mode;
                for allow in [false, true] {
                    let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
                    assert_eq!(error.stage, "plan output");
                    assert!(error.message.contains("already processed"));
                }
            }
        } else {
            s.output.mode = OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false };
            let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
            let output = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
            assert_eq!(output.processing_marker_status, "present");
        }
        assert_eq!(fs::read(input).unwrap(), bytes);
    }
    assert_eq!(fs::read_dir(backup).unwrap().count(), 0);
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_LAME_LIB and PN_LAME_SHA256"]
fn decodable_mp3_first_frame_payload_may_equal_mp4_atom_names() {
    use platinum_engine::tags::{Format, TagBridge};
    let f = Fixture::new(); let mut s = f.settings();
    s.libraries.lame = Some(PathBuf::from(std::env::var_os("PN_LAME_LIB").expect("trusted public LAME")));
    s.libraries.lame_sha256 = Some(std::env::var("PN_LAME_SHA256").expect("LAME SHA-256 pin"));
    let source = f.input("collision-source.wav", 96_000, 2, false);
    let template = s.template(TemplateRef::Index(0)).unwrap();
    s.output.format = AudioFormat::Mp3;
    let prepared = prepare_file(&discovered(&source), &s, false).unwrap();
    let encoded = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
    let original = fs::read(&encoded.output_path).unwrap();
    assert_eq!(&original[..3], b"ID3");
    let size = original[6..10].iter().fold(0usize, |n, byte| (n << 7) | usize::from(*byte));
    assert_eq!(original[5] & 0x10, 0);
    assert!(10 + size + 8 < original.len());
    let tag = &original[..10 + size];
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    s.output.format = AudioFormat::Wav;
    for atom in [b"ftyp", b"moov", b"mdat"] {
        let name = std::str::from_utf8(atom).unwrap();
        let mut audio = original[10 + size..].to_vec();
        assert_eq!(audio[0], 0xff); assert_eq!(audio[1] & 0xe0, 0xe0);
        audio[4..8].copy_from_slice(atom);
        for (prefix, marked, label) in [
            (b"".as_slice(), false, "raw"),
            (b"ID3\x04\0\0\0\0\0\0".as_slice(), false, "id3"),
            (tag, true, "marked"),
        ] {
            let input = f.0.join(format!("{name}-{label}.mp3"));
            let mut bytes = prefix.to_vec(); bytes.extend_from_slice(&audio);
            fs::write(&input, &bytes).unwrap();
            assert_eq!(bridge.read_processing_marker(&input, Format::Mp3).unwrap().is_some(), marked);
            let info = inspect_file(&input, &s).unwrap();
            assert_eq!(info.processed, Some(marked), "{name} {label}");
            if marked {
                let error = prepare_file(&discovered(&input), &s, false).unwrap_err();
                assert_eq!(error.stage, "plan output");
                assert!(error.message.contains("already processed"));
            } else {
                let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
                let report = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
                assert_eq!(report.processing_marker_status, "present");
            }
            assert_eq!(fs::read(input).unwrap(), bytes);
        }
    }
}



#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn misnamed_wav_marker_is_inspected_before_separate_or_replacement_planning() {
    use platinum_engine::metadata::encode_marker_base64;

    let f = Fixture::new();
    let backup = f.0.join("backups");
    fs::create_dir(&backup).unwrap();
    let mut s = f.settings();
    let marker = encode_marker_base64(br#"{"isProcessed":true,"backupFileName":"/original/backup.wav"}"#).unwrap();
    for extension in ["aif", "m4a"] {
        let input = f.input_with_marker(&format!("marked.{extension}"), &marker);
        let before = fs::read(&input).unwrap();
        let report = inspect_file(&input, &s).unwrap();
        assert!(report.tags.is_none() && report.tags_unavailable.is_some());
        assert_eq!(report.processed, Some(true));
        assert_eq!(report.processing_state, "marked");
        assert_eq!(report.backup_path.as_deref(), Some(Path::new("/original/backup.wav")));
        assert_eq!(report.marker_json.unwrap()["isProcessed"], true);
        for mode in [
            OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false },
            OutputMode::Replace { backup_directory: backup.clone() },
        ] {
            s.output.mode = mode;
            for allow in [false, true] {
                let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
                assert_eq!(error.stage, "plan output");
                assert!(error.message.contains("already processed"));
                assert_eq!(fs::read(&input).unwrap(), before);
            }
        }
    }
    assert_eq!(fs::read_dir(&backup).unwrap().count(), 0);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn unmarked_misnamed_wav_processes_without_source_tags_and_can_replace() {
    let f = Fixture::new();
    let mut s = f.settings();
    let backup = f.0.join("backups");
    fs::create_dir(&backup).unwrap();
    for replace in [false, true] {
        let input = f.input(if replace { "replace.aif" } else { "separate.aif" }, 48_513, 2, false);
        // The real WAV contains a title, but the unavailable extension-selected
        // AIFF reader must not cause that source metadata to be copied.
        let mut bytes = fs::read(&input).unwrap();
        let title = b"must not be copied\0";
        let mut info = b"INFOINAM".to_vec();
        info.extend_from_slice(&(title.len() as u32).to_le_bytes());
        info.extend_from_slice(title);
        if title.len() & 1 != 0 { info.push(0); }
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&(info.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&info);
        let length = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        fs::write(&input, bytes).unwrap();
        let bridge = unsafe { platinum_engine::tags::TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
        assert_eq!(bridge.inspect(&input, platinum_engine::tags::Format::Wav).unwrap().title.as_deref(), Some("must not be copied"));
        let before = fs::read(&input).unwrap();
        s.output.mode = if replace { OutputMode::Replace { backup_directory: backup.clone() } }
            else { OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false } };
        let inspection = inspect_file(&input, &s).unwrap();
        assert!(inspection.tags.is_none() && inspection.tags_unavailable.is_some());
        assert_eq!(inspection.processed, Some(false));
        assert_eq!(inspection.processing_state, "unprocessed");
        assert!(inspection.marker.is_none());
        let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
        let template = s.template(TemplateRef::Index(0)).unwrap();
        let report = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
        assert!(report.source_tags_unavailable.is_some());
        assert_eq!(report.processing_marker_status, "present");
        if replace {
            assert_eq!(fs::read(report.backup_path.as_ref().unwrap()).unwrap(), before);
            assert_eq!(report.output_path, fs::canonicalize(&input).unwrap());
        } else {
            assert_eq!(fs::read(&input).unwrap(), before);
        }
        assert_eq!(inspect_file(&report.output_path, &s).unwrap().processed, Some(true));
        let output_tags = inspect_file(&report.output_path, &s).unwrap().tags.unwrap();
        assert_ne!(output_tags.title.as_deref(), Some("must not be copied"));
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn misnamed_ambiguous_wav_keeps_override_and_source_tag_diagnostic() {
    let f = Fixture::new(); let s = f.settings();
    let input = f.input_with_marker("ambiguous.aif", b"");
    let before = fs::read(&input).unwrap();
    assert_eq!(prepare_file(&discovered(&input), &s, false).unwrap_err().stage, "decode marker");
    assert_eq!(inspect_file(&input, &s).unwrap_err().stage, "decode marker");
    let prepared = prepare_file(&discovered(&input), &s, true).unwrap();
    assert_eq!(prepared.input_marker_override(), Some("empty_wav_pltn"));
    let template = s.template(TemplateRef::Index(0)).unwrap();
    let report = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
    assert_eq!(report.input_marker_override, Some("empty_wav_pltn"));
    assert!(report.source_tags_unavailable.is_some());
    assert_eq!(fs::read(&input).unwrap(), before);
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_LAME_LIB, PN_LAME_SHA256 and source-container status support"]
fn misnamed_mp3_marker_is_inspected_and_unmarked_stream_can_be_processed() {
    let f = Fixture::new(); let mut s = f.settings();
    s.libraries.lame = Some(PathBuf::from(std::env::var_os("PN_LAME_LIB").expect("trusted public LAME")));
    s.libraries.lame_sha256 = Some(std::env::var("PN_LAME_SHA256").expect("LAME SHA-256 pin"));
    let original = f.input("mp3-source.wav", 96_000, 2, false);
    s.output.format = AudioFormat::Mp3;
    let prepared = prepare_file(&discovered(&original), &s, false).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    let encoded = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
    assert_eq!(encoded.processing_marker_status, "present");
    let marked = fs::read(&encoded.output_path).unwrap();
    assert_eq!(&marked[..3], b"ID3");
    let size = marked[6..10].iter().fold(0usize, |n, byte| (n << 7) | usize::from(*byte));
    assert_eq!(marked[5] & 0x10, 0, "fixture ID3 footer would need removing too");
    assert!(10 + size < marked.len());
    let renamed = f.0.join("marked-mp3.wav");
    fs::write(&renamed, &marked).unwrap();
    s.output.format = AudioFormat::Wav;
    let info = inspect_file(&renamed, &s).unwrap();
    assert!(info.tags.is_none() && info.tags_unavailable.is_some());
    assert_eq!(info.processed, Some(true));
    assert_eq!(info.processing_state, "marked");
    let backup = f.0.join("mp3-backups"); fs::create_dir(&backup).unwrap();
    for mode in [
        OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false },
        OutputMode::Replace { backup_directory: backup.clone() },
    ] {
        s.output.mode = mode;
        for allow in [false, true] {
            let error = prepare_file(&discovered(&renamed), &s, allow).unwrap_err();
            assert_eq!(error.stage, "plan output");
            assert!(error.message.contains("already processed"));
        }
    }
    assert_eq!(fs::read(&renamed).unwrap(), marked);
    assert_eq!(fs::read_dir(&backup).unwrap().count(), 0);
    let unmarked = f.0.join("unmarked-mp3.wav");
    fs::write(&unmarked, &marked[10 + size..]).unwrap();
    s.output.mode = OutputMode::SpecificFolder { directory: f.0.clone(), mirror: false };
    let info = inspect_file(&unmarked, &s).unwrap();
    assert_eq!(info.processed, Some(false));
    assert!(info.marker.is_none() && info.tags_unavailable.is_some());
    let prepared = prepare_file(&discovered(&unmarked), &s, false).unwrap();
    let result = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
    assert!(result.source_tags_unavailable.is_some());
    assert_eq!(result.processing_marker_status, "present");
    assert_eq!(inspect_file(&result.output_path, &s).unwrap().processed, Some(true));
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_FLAC_LIB, PN_FLAC_SHA256 and source-container status support"]
fn misnamed_aiff_mp4_and_flac_markers_are_not_lost() {
    let f = Fixture::new(); let mut s = f.settings();
    s.libraries.flac = Some(PathBuf::from(std::env::var_os("PN_FLAC_LIB").expect("trusted public FLAC")));
    s.libraries.flac_sha256 = Some(std::env::var("PN_FLAC_SHA256").expect("FLAC SHA-256 pin"));
    let original = f.input("source-codecs.wav", 48_513, 2, false);
    let template = s.template(TemplateRef::Index(0)).unwrap();
    for (format, label) in [(AudioFormat::Aiff, "aiff"), (AudioFormat::Alac, "alac"), (AudioFormat::Flac, "flac")] {
        s.output.format = format;
        let prepared = prepare_file(&discovered(&original), &s, false).unwrap();
        let encoded = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
        assert_eq!(encoded.processing_marker_status, "present");
        let input = f.0.join(format!("marked-{label}.wav"));
        let bytes = fs::read(&encoded.output_path).unwrap();
        fs::write(&input, &bytes).unwrap();
        let inspection = inspect_file(&input, &s).unwrap();
        assert_eq!(inspection.processed, Some(true), "{label}");
        assert_eq!(inspection.processing_state, "marked", "{label}");
        assert!(inspection.tags.is_none() && inspection.tags_unavailable.is_some(), "{label}");
        let error = prepare_file(&discovered(&input), &s, false).unwrap_err();
        assert_eq!(error.stage, "plan output", "{label}");
        assert!(error.message.contains("already processed"), "{label}");
        assert_eq!(fs::read(&input).unwrap(), bytes);
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn malformed_matching_marker_chunks_and_garbage_are_refused() {
    use platinum_engine::metadata::encode_marker_base64;
    let f = Fixture::new(); let s = f.settings();
    for (container, marker_id) in [(b"AIFF", b"ID3 "), (b"AIFC", b"id3 "), (b"AIFF", b"PLTN")] {
        let aiff = f.0.join(format!("bad-{}-{}.aif", std::str::from_utf8(container).unwrap(), std::str::from_utf8(marker_id).unwrap().trim()));
        let mut bytes = b"FORM\xff\xff\xff\xff".to_vec();
        bytes.extend_from_slice(container);
        bytes.extend_from_slice(b"COMM\0\0\0\x12");
        bytes.extend_from_slice(&[0,1, 0,0,0,2, 0,16, 0x40,0x0e,0xac,0x44,0,0,0,0,0,0]);
        bytes.extend_from_slice(b"SSND\0\0\0\x0c");
        bytes.extend_from_slice(&[0; 12]);
        bytes.extend_from_slice(marker_id);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        fs::write(&aiff, &bytes).unwrap();
        for allow in [false, true] {
            let error = prepare_file(&discovered(&aiff), &s, allow).unwrap_err();
            assert_eq!(error.stage, "inspect tags");
            assert!(error.message.contains("possible PLTN or ID3"));
        }
        assert_eq!(fs::read(&aiff).unwrap(), bytes);
    }
    let marker = encode_marker_base64(br#"{"isProcessed":true}"#).unwrap();
    let wave = f.input_with_marker("bad.wav", &marker);
    let mut wave_bytes = fs::read(&wave).unwrap();
    wave_bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&wave, &wave_bytes).unwrap();
    for allow in [false, true] {
        let error = prepare_file(&discovered(&wave), &s, allow).unwrap_err();
        assert_eq!(error.stage, "inspect tags");
    }
    assert_eq!(fs::read(&wave).unwrap(), wave_bytes);
    let misnamed = f.0.join("bad-misnamed.aif");
    fs::write(&misnamed, &wave_bytes).unwrap();
    assert_eq!(prepare_file(&discovered(&misnamed), &s, true).unwrap_err().stage, "read marker");
    assert_eq!(fs::read(&misnamed).unwrap(), wave_bytes);

    let id3 = f.input("bad-id3.wav", 2048, 2, false);
    let mut id3_bytes = fs::read(&id3).unwrap();
    id3_bytes.extend_from_slice(b"id3 \0\0\0\0");
    id3_bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&id3, &id3_bytes).unwrap();
    assert_eq!(prepare_file(&discovered(&id3), &s, true).unwrap_err().stage, "inspect tags");

    let garbage = f.input("garbage-chunk.wav", 2048, 2, false);
    let mut bytes = fs::read(&garbage).unwrap();
    bytes.extend_from_slice(&[0xff; 8]);
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&garbage, &bytes).unwrap();
    let error = prepare_file(&discovered(&garbage), &s, false).unwrap_err();
    assert_eq!(error.stage, "inspect tags");
    assert_eq!(fs::read(&garbage).unwrap(), bytes);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn truncated_unmarked_wav_walks_audio_then_processes_without_source_tags() {
    let f = Fixture::new(); let s = f.settings();
    let input = f.input("truncated.wav", 48_513, 2, false);
    let mut bytes = fs::read(&input).unwrap();
    let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
    bytes[4..8].copy_from_slice(&(riff + 100).to_le_bytes());
    bytes[40..44].copy_from_slice(&(data + 100).to_le_bytes());
    // This occurs inside PCM, not at a chunk boundary.
    bytes[44..48].copy_from_slice(b"PLTN");
    fs::write(&input, &bytes).unwrap();
    let inspection = inspect_file(&input, &s).unwrap();
    assert_eq!(inspection.processed, Some(false));
    assert!(inspection.tags.is_none() && inspection.tags_unavailable.is_some());
    let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    let result = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
    assert!(result.source_tags_unavailable.is_some());
    assert_eq!(result.processing_marker_status, "present");
    assert_eq!(fs::read(&input).unwrap(), bytes);
    assert_eq!(inspect_file(&result.output_path, &s).unwrap().processed, Some(true));
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with source-container status support"]
fn truncated_unmarked_aiff_walks_audio_then_processes_without_source_tags() {
    let f = Fixture::new(); let s = f.settings();
    let frames = 48_513u32;
    let wave = f.input("aiff-pcm.wav", frames, 2, false);
    let wave_bytes = fs::read(&wave).unwrap();
    let input = f.0.join("truncated.aif");
    let mut bytes = b"FORM\0\0\0\0AIFFCOMM\0\0\0\x12".to_vec();
    bytes.extend_from_slice(&2u16.to_be_bytes());
    bytes.extend_from_slice(&frames.to_be_bytes());
    bytes.extend_from_slice(&16u16.to_be_bytes());
    bytes.extend_from_slice(&[0x40,0x0e,0xbb,0x80,0,0,0,0,0,0]);
    bytes.extend_from_slice(b"SSND");
    bytes.extend_from_slice(&(8 + frames * 4).to_be_bytes());
    bytes.extend_from_slice(&[0; 8]);
    for sample in wave_bytes[44..].chunks_exact(2) { bytes.extend_from_slice(&[sample[1], sample[0]]); }
    let length = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&(length + 100).to_be_bytes());
    fs::write(&input, &bytes).unwrap();
    let inspection = inspect_file(&input, &s).unwrap();
    assert_eq!(inspection.processed, Some(false));
    assert!(inspection.tags.is_none() && inspection.tags_unavailable.is_some());
    let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    let result = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
    assert!(result.source_tags_unavailable.is_some());
    assert_eq!(result.processing_marker_status, "present");
    assert_eq!(fs::read(&input).unwrap(), bytes);
    assert_eq!(inspect_file(&result.output_path, &s).unwrap().processed, Some(true));
}


#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn inspection_and_analysis_use_real_source_and_distinguish_malformed_markers() {
    let f = Fixture::new(); let s = f.settings();
    let input = f.input("source.wav", 48_513, 2, false);
    let bytes = fs::read(&input).unwrap();
    let info = inspect_file(&input, &s).unwrap();
    assert_eq!(info.input_format, "wav/pcm"); assert_eq!(info.channels, 2);
    assert_eq!(info.frames, 48_513); assert_eq!(info.sample_rate, 48_000.0);
    assert_eq!(info.processed, Some(false)); assert!(info.marker.is_none());
    let result = analyze_file(&input, &s, &Cancellation::new()).unwrap();
    assert_eq!(result.frames, 48_513);
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    {
        let waveform = result.waveform.as_ref().unwrap();
        assert_eq!(waveform.sample_rate, 44_100.0);
        assert_eq!(waveform.requested_sample_rate, 44_100.0);
        assert_eq!(waveform.bins[0].frames, 512);
        assert_eq!(waveform.bins.iter().map(|bin| u64::from(bin.frames)).sum::<u64>(), waveform.frames);
        assert!(waveform.bins.iter().all(|bin| (1..=512).contains(&bin.frames)));
    }
    #[cfg(any(target_os = "linux", feature = "portable"))]
    assert!(result.waveform.is_none() && result.waveform_unavailable.is_some());
    assert!(result.volume.is_finite()); assert!(result.pitch_hz.is_finite());
    assert_eq!(result.clip_count, None); assert_eq!(fs::read(&input).unwrap(), bytes);
    let invalid = f.input("invalid.wav", 2_048, 2, true);
    let error = inspect_file(&invalid, &s).unwrap_err();
    assert_eq!(error.stage, "decode marker");
    assert!(prepare_file(&discovered(&invalid), &s, false).is_err());
    assert_eq!(fs::read(&input).unwrap(), bytes);
}

#[cfg(any(target_os = "linux", feature = "portable"))]
#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn portable_refuses_unverified_rates_and_unavailable_encoders_before_publication() {
    let f = Fixture::new();
    let mut settings = f.settings();
    let input = f.input("low-rate.wav", 1024, 2, false);
    let mut bytes = fs::read(&input).unwrap();
    bytes[24..28].copy_from_slice(&22_050u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&(22_050u32 * 4).to_le_bytes());
    fs::write(&input, &bytes).unwrap();
    let error = analyze_file(&input, &settings, &Cancellation::new()).unwrap_err();
    assert_eq!(error.stage, "analysis sample-rate conversion");
    assert!(error.message.contains("sample-rate"));
    assert_eq!(fs::read(&input).unwrap(), bytes);

    let input = f.input("encoder.wav", 1024, 2, false);
    for (format, stage) in [(AudioFormat::Mp3, "load MP3 encoder"), (AudioFormat::Flac, "load FLAC encoder")] {
        settings.output.format = format;
        let plan = prepare_file(&discovered(&input), &settings, false).unwrap();
        let destination = plan.output_plan().destination().to_owned();
        let template = settings.template(TemplateRef::Index(0)).unwrap();
        let error = process_file(&plan, &template, &settings, &Cancellation::new()).unwrap_err();
        assert_eq!(error.stage, stage);
        assert!(!destination.exists());
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn stereo_process_exercises_all_stages_and_publishes_marked_wav_without_clobber() {
    let f = Fixture::new(); let s = f.settings();
    let input = f.input("stage-test.wav", 96_513, 2, false);
    let original = fs::read(&input).unwrap();
    let mut t = s.template(TemplateRef::Index(0)).unwrap();
    t.clip.enabled = true; t.clip.low = -200.0; t.clip.high = 100.0;
    t.pitch.enabled = true; t.pitch.threshold_cents = 0; t.pitch.option = 1;
    t.exciter_option = 0;
    t.expansion.enabled = true; t.expansion.low = -200.0; t.expansion.high = 100.0;
    t.compression.enabled = true; t.compression.low = -200.0; t.compression.high = 100.0;
    let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
    let result = process_file(&prepared, &t, &s, &Cancellation::new()).unwrap();
    for name in ["clip_repair", "pitch", "warmth", "expansion", "compression", "limiter"] {
        assert!(result.applied_stages.iter().any(|v| v == name), "missing {name}");
    }
    assert_eq!(result.input_frames, 96_513); assert!(result.healed_frames > 0 && result.output_frames > 0);
    assert_eq!(result.processing_marker_status, "present");
    assert_eq!(result.input_format, "wav/pcm"); assert_eq!(result.output_format, "wav/pcm");
    assert_eq!(fs::read(&input).unwrap(), original);
    let output = inspect_file(&result.output_path, &s).unwrap();
    assert_eq!(output.processed, Some(true)); assert!(output.marker.as_ref().unwrap().is_processed);
    let marked = output.marker.unwrap(); assert!(marked.applied_clip_repair && marked.applied_pitch_correction);
    assert!(marked.applied_warmth && marked.applied_expansion && marked.applied_compression);
    let produced = fs::read(&result.output_path).unwrap();
    let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
    assert!(process_file(&prepared, &t, &s, &Cancellation::new()).is_err());
    assert_eq!(fs::read(&result.output_path).unwrap(), produced);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn replacement_retains_verified_backup_and_cancellation_does_not_publish() {
    let f = Fixture::new(); let mut s = f.settings();
    let input = f.input("replacement.wav", 96_000, 1, false);
    let original = fs::read(&input).unwrap();
    let t = s.template(TemplateRef::Index(0)).unwrap();
    s.output.mode = OutputMode::Replace { backup_directory: f.0.clone() };
    let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
    let cancel = Cancellation::new(); cancel.cancel();
    let cancelled = process_file(&prepared, &t, &s, &cancel).unwrap_err();
    assert!(cancelled.cancelled); assert_eq!(fs::read(&input).unwrap(), original);
    let report = process_file(&prepared, &t, &s, &Cancellation::new()).unwrap();
    assert_eq!(fs::read(report.backup_path.as_ref().unwrap()).unwrap(), original);
    let inspected = inspect_file(&report.output_path, &s).unwrap();
    assert_eq!(inspected.marker.as_ref().unwrap().backup_file_name.as_ref().unwrap(),
        &platinum_engine::metadata::MarkerText::from(report.backup_path.unwrap().to_str().unwrap()));
    let analysis = analyze_file(&report.output_path, &s, &Cancellation::new()).unwrap();
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    assert!(analysis.original_waveform.is_some());
    #[cfg(any(target_os = "linux", feature = "portable"))]
    assert!(analysis.original_waveform.is_none() && analysis.original_waveform_unavailable.is_some());
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn changed_source_after_preflight_is_rejected_before_transaction() {
    let f = Fixture::new(); let mut s = f.settings();
    s.output.mode = OutputMode::Replace { backup_directory: f.0.clone() };
    let input = f.input("changed.wav", 48_000, 1, false);
    let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
    file_workflow::validate_batch(&[prepared.output_plan()], &[discovered(&input)]).unwrap();
    let destination = prepared.output_plan().destination().to_owned();
    f.input("changed.wav", 48_001, 1, false);
    let changed = fs::read(&input).unwrap();
    let mut before: Vec<_> = fs::read_dir(&f.0).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    before.sort();
    let t = s.template(TemplateRef::Index(0)).unwrap();
    let error = process_file(&prepared, &t, &s, &Cancellation::new()).unwrap_err();
    assert_eq!(error.stage, "plan source identity");
    assert!(!error.cancelled && error.retained_backup.is_none() && !error.partial_publication);
    assert_eq!(destination, fs::canonicalize(&input).unwrap());
    assert_eq!(fs::read(&input).unwrap(), changed);
    let mut after: Vec<_> = fs::read_dir(&f.0).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    after.sort();
    assert_eq!(after, before, "a stale plan created a backup, workspace, or reservation");
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn short_and_silent_wav_publish_native_nil_marker_without_fake_measurements() {
    use platinum_engine::tags::{Format, TagBridge};
    let f = Fixture::new(); let s = f.settings();
    let bridge = unsafe { TagBridge::load(s.libraries.tag_bridge.as_ref().unwrap()).unwrap() };
    for (name, frames, silent) in [("short.wav", 513, false), ("silent.wav", 48_000, true)] {
        let input = f.input(name, frames, 2, false);
        if silent {
            let mut bytes = fs::read(&input).unwrap();
            bytes[44..].fill(0);
            fs::write(&input, bytes).unwrap();
        }
        let before = fs::read(&input).unwrap();
        let prepared = prepare_file(&discovered(&input), &s, false).unwrap();
        let t = s.template(TemplateRef::Index(0)).unwrap();
        let report = process_file(&prepared, &t, &s, &Cancellation::new()).unwrap();
        assert_eq!(report.processing_marker_status, "nil_payload_empty_wav_chunk");
        assert_eq!(report.input_volume, f32::NEG_INFINITY);
        assert_eq!(serde_json::to_value(&report).unwrap()["input_volume"], "-Infinity");
        assert_eq!(bridge.read_processing_marker(&report.output_path, Format::Wav).unwrap(), Some(Vec::new()));
        assert_eq!(inspect_file(&report.output_path, &s).unwrap_err().stage, "decode marker");
        assert_eq!(fs::read(&input).unwrap(), before);
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE and a caller-selected filesystem for identity stress"]
fn repeated_synthetic_wav_publication_keeps_the_tagged_inode_pinned() {
    let f = Fixture::new();
    let settings = f.settings();
    let source = f.input("repeated.wav", 513, 2, false);
    let template = settings.template(TemplateRef::Index(0)).unwrap();
    let iterations = std::env::var("PN_ENGINE_PIN_STRESS").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(200);
    for index in 0..iterations {
        let prepared = prepare_file(&discovered(&source), &settings, false).unwrap();
        let report = process_file(&prepared, &template, &settings, &Cancellation::new())
            .unwrap_or_else(|e| panic!("iteration {index}: {e}"));
        assert_eq!(report.processing_marker_status, "nil_payload_empty_wav_chunk");
        fs::remove_file(report.output_path).unwrap();
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn ambiguous_wav_markers_need_process_override_without_weakening_inspection() {
    use platinum_engine::metadata::encode_marker_base64;
    let f = Fixture::new(); let s = f.settings();
    let incomplete = encode_marker_base64(br#"{"isProcessed":"#).unwrap();
    let partial_true = encode_marker_base64(br#"{"isProcessed":true"#).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    for (name, payload, kind) in [
        ("empty.wav", b"".as_slice(), "empty_wav_pltn"),
        ("incomplete.wav", incomplete.as_slice(), "incomplete_json"),
        ("partial-true.wav", partial_true.as_slice(), "incomplete_json"),
    ] {
        let input = f.input_with_marker(name, payload);
        let original = fs::read(&input).unwrap();
        let denied = prepare_file(&discovered(&input), &s, false).unwrap_err();
        assert_eq!(denied.stage, "decode marker");
        assert!(denied.message.contains("--allow-ambiguous-marker"));
        assert!(denied.message.contains("may reprocess"));
        assert_eq!(inspect_file(&input, &s).unwrap_err().stage, "decode marker");
        assert_eq!(read_restore_marker(&input, &s).unwrap_err().stage, "decode marker");
        assert_eq!(analyze_file(&input, &s, &Cancellation::new()).unwrap_err().stage, "decode marker");

        let prepared = prepare_file(&discovered(&input), &s, true).unwrap();
        assert_eq!(prepared.input_marker_override(), Some(kind));
        let report = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
        assert_eq!(report.input_marker_override, Some(kind));
        assert_eq!(serde_json::to_value(&report).unwrap()["input_marker_override"], kind);
        assert_eq!(fs::read(&input).unwrap(), original);
        let output = fs::read(&report.output_path).unwrap();
        let repeat = prepare_file(&discovered(&input), &s, true).unwrap();
        assert!(process_file(&repeat, &template, &s, &Cancellation::new()).is_err());
        assert_eq!(fs::read(&report.output_path).unwrap(), output);
    }

    let valid_processed = encode_marker_base64(br#"{"isProcessed":true}"#).unwrap();
    let input = f.input_with_marker("valid-processed.wav", &valid_processed);
    for allow in [false, true] {
        let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
        assert_eq!(error.stage, "plan output");
        assert!(error.message.contains("already processed"));
    }

    let input = f.input_with_marker("suffixed_PN.wav", b"");
    let error = prepare_file(&discovered(&input), &s, true).unwrap_err();
    assert_eq!(error.stage, "plan output");
    assert!(error.message.contains("already processed"));

    let malformed = encode_marker_base64(br#"{"isProcessed":truX}"#).unwrap();
    for (name, payload) in [("invalid-base64.wav", b"!!!!".as_slice()), ("invalid-json.wav", malformed.as_slice())] {
        let input = f.input_with_marker(name, payload);
        assert_eq!(prepare_file(&discovered(&input), &s, true).unwrap_err().stage, "decode marker");
    }

    let input = f.input_with_marker("duplicate-markers.wav", &valid_processed);
    let mut bytes = fs::read(&input).unwrap();
    bytes.extend_from_slice(&platinum_engine::metadata::wav_pltn_chunk(Some(&incomplete)).unwrap());
    let length = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    fs::write(&input, &bytes).unwrap();
    let error = prepare_file(&discovered(&input), &s, true).unwrap_err();
    assert_eq!(error.stage, "decode marker");
    assert!(error.message.contains("multiple WAV PLTN"));
    assert_eq!(fs::read(&input).unwrap(), bytes);

    let valid_unprocessed = encode_marker_base64(br#"{"isProcessed":false}"#).unwrap();
    let input = f.input_with_marker("duplicate-valid-markers.wav", &valid_processed);
    let mut bytes = fs::read(&input).unwrap();
    bytes.extend_from_slice(&platinum_engine::metadata::wav_pltn_chunk(Some(&valid_unprocessed)).unwrap());
    let length = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    fs::write(&input, &bytes).unwrap();
    // Strict inspection and restore still expose the bridge's selected last marker.
    assert!(!inspect_file(&input, &s).unwrap().marker.unwrap().is_processed);
    assert!(!read_restore_marker(&input, &s).unwrap().1.unwrap().is_processed);
    for allow in [false, true] {
        let error = prepare_file(&discovered(&input), &s, allow).unwrap_err();
        assert_eq!(error.stage, "decode marker");
        assert!(error.message.contains("multiple WAV PLTN"));
    }
    assert_eq!(fs::read(&input).unwrap(), bytes);

    for (name, payload) in [("empty-trailing.wav", b"".as_slice()), ("incomplete-trailing.wav", incomplete.as_slice())] {
        let input = f.input_with_marker(name, payload);
        let mut bytes = fs::read(&input).unwrap();
        // This extra valid processed PLTN lies outside the declared RIFF.
        bytes.extend_from_slice(&platinum_engine::metadata::wav_pltn_chunk(Some(&valid_processed)).unwrap());
        fs::write(&input, &bytes).unwrap();
        let error = prepare_file(&discovered(&input), &s, true).unwrap_err();
        assert_eq!(error.stage, "decode marker");
        assert!(!error.partial_publication && error.retained_backup.is_none());
        assert_eq!(inspect_file(&input, &s).unwrap_err().stage, "decode marker");
        assert_eq!(read_restore_marker(&input, &s).unwrap_err().stage, "decode marker");
        assert_eq!(fs::read(&input).unwrap(), bytes);
    }

    let absent_trailer = f.input("absent-with-marker-trailer.wav", 48_513, 2, false);
    let unprocessed_trailer = f.input_with_marker("unprocessed-with-marker-trailer.wav", &valid_unprocessed);
    for (input, expected_marker) in [(&absent_trailer, None), (&unprocessed_trailer, Some(false))] {
        let mut bytes = fs::read(input).unwrap();
        bytes.extend_from_slice(&platinum_engine::metadata::wav_pltn_chunk(Some(&valid_processed)).unwrap());
        fs::write(input, &bytes).unwrap();
        assert_eq!(inspect_file(input, &s).unwrap().marker.map(|marker| marker.is_processed), expected_marker);
        assert_eq!(read_restore_marker(input, &s).unwrap().1.map(|marker| marker.is_processed), expected_marker);
        for allow in [false, true] {
            let error = prepare_file(&discovered(input), &s, allow).unwrap_err();
            assert_eq!(error.stage, "decode marker");
            assert!(!error.partial_publication && error.retained_backup.is_none());
        }
        assert_eq!(fs::read(input).unwrap(), bytes);
    }

    let benign_absent = f.input("absent-benign-trailer.wav", 48_513, 2, false);
    let benign_unprocessed = f.input_with_marker("unprocessed-benign-trailer.wav", &valid_unprocessed);
    for input in [&benign_absent, &benign_unprocessed] {
        let mut bytes = fs::read(input).unwrap();
        bytes.extend_from_slice(b"helloP");
        fs::write(input, &bytes).unwrap();
        for allow in [false, true] {
            let prepared = prepare_file(&discovered(input), &s, allow).unwrap();
            assert_eq!(prepared.input_marker_override(), None);
        }
        assert_eq!(fs::read(input).unwrap(), bytes);
    }

    let unmarked = f.input("ordinary.wav", 48_513, 2, false);
    let unprocessed = f.input_with_marker("valid-unprocessed.wav", &valid_unprocessed);
    for allow in [false, true] {
        for input in [&unmarked, &unprocessed] {
            let prepared = prepare_file(&discovered(input), &s, allow).unwrap();
            assert_eq!(prepared.input_marker_override(), None);
        }
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn ambiguous_override_keeps_source_identity_and_batch_collision_checks() {
    let f = Fixture::new(); let s = f.settings();
    fs::create_dir(f.0.join("left")).unwrap();
    fs::create_dir(f.0.join("right")).unwrap();
    let left = f.input_with_marker("left/song.wav", b"");
    let right = f.input_with_marker("right/song.wav", b"");
    let first = prepare_file(&discovered(&left), &s, true).unwrap();
    let second = prepare_file(&discovered(&right), &s, true).unwrap();
    assert!(matches!(file_workflow::validate_batch(&[first.output_plan(), second.output_plan()], &[discovered(&left), discovered(&right)]),
        Err(file_workflow::WorkflowError::DestinationConflict { .. })));

    let input = f.input_with_marker("changed-ambiguous.wav", b"");
    let prepared = prepare_file(&discovered(&input), &s, true).unwrap();
    let destination = prepared.output_plan().destination().to_owned();
    let mut changed = fs::read(&input).unwrap(); changed[44] ^= 1; fs::write(&input, &changed).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    let error = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap_err();
    assert_eq!(error.stage, "plan source identity");
    assert!(!error.partial_publication && error.retained_backup.is_none());
    assert!(!destination.exists());
    assert_eq!(fs::read(&input).unwrap(), changed);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE with ABI 5 built from public TagLib"]
fn ambiguous_replacement_keeps_verified_backup_and_strict_restore() {
    let f = Fixture::new(); let mut s = f.settings();
    let backups = f.0.join("backups"); fs::create_dir(&backups).unwrap();
    let input = f.input_with_marker("replacement-ambiguous.wav", b"");
    let original = fs::read(&input).unwrap();
    assert_eq!(read_restore_marker(&input, &s).unwrap_err().stage, "decode marker");
    s.output.mode = OutputMode::Replace { backup_directory: backups };
    let prepared = prepare_file(&discovered(&input), &s, true).unwrap();
    let template = s.template(TemplateRef::Index(0)).unwrap();
    let report = process_file(&prepared, &template, &s, &Cancellation::new()).unwrap();
    assert_eq!(report.input_marker_override, Some("empty_wav_pltn"));
    let backup = report.backup_path.unwrap();
    assert_eq!(fs::read(&backup).unwrap(), original);
    let (identity, marker) = read_restore_marker(&input, &s).unwrap();
    assert!(marker.as_ref().unwrap().is_processed);
    let restored = file_workflow::restore(&input, &backup, marker.as_ref(), file_workflow::RestoreStatus::Inactive, &identity, &Cancellation::new()).unwrap();
    assert!(restored.backup_removed);
    assert_eq!(fs::read(&input).unwrap(), original);
    assert!(!backup.exists());
}
