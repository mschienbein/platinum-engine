//! Opt-in artwork byte copy through the public `TagBridge` API.
//!
//! Needs a portable bridge that exports the artwork extension:
//! `PN_TAG_BRIDGE=/path/libpn-tag-bridge.so cargo test --test portable_art_copy -- --ignored`
//! (macOS: build with `--features portable` and point at the portable dylib).
//! Only synthetic byte fixtures are used.
#![cfg(any(target_os = "linux", feature = "portable"))]

use platinum_engine::metadata::{Marker, ProcessingMarker};
use platinum_engine::tags::{Format, OutputChoice, TagBridge, TagError, TransferOptions};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0,
    0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x60, 0x60, 0x60, 0, 0, 0, 5, 0, 1,
    0xa5, 0xf6, 0x45, 0x40, 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
const WEBP: [u8; 38] = [
    0x52, 0x49, 0x46, 0x46, 0x1e, 0, 0, 0, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x4c, 0x11, 0, 0, 0, 0x2f, 0, 0, 0, 0, 7, 0xd0,
    0x83, 0x1e, 0xf4, 0xa0, 0xff, 0x81, 0x88, 0xe8, 0x7f, 0, 0,
];

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new(ext: &str, data: &[u8]) -> Self {
        loop {
            let path = std::env::temp_dir().join(format!("platinum-art-{}-{}.{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed), ext));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => { file.write_all(data).unwrap(); return Self(path); }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        }
    }
    fn path(&self) -> &Path { &self.0 }
    fn bytes(&self) -> Vec<u8> { fs::read(&self.0).unwrap() }
}
impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_file(&self.0); } }

fn bridge() -> TagBridge {
    let path = std::env::var_os("PN_TAG_BRIDGE").expect("set PN_TAG_BRIDGE to a portable bridge with the artwork extension");
    // SAFETY: the opt-in test requires the caller to select a trusted bridge.
    let bridge = unsafe { TagBridge::load(path).unwrap() };
    assert!(bridge.supports_artwork_copy(), "the selected bridge lacks the artwork-copy extension");
    bridge
}
fn marker() -> Marker {
    Marker::from_processing(ProcessingMarker {
        software_version: Some("10.5.0.1256"), backup_file_name: None,
        applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
        expansion_applied: 0, compression_applied: 1, volume_analysis_version: 4,
        target_volume: -8.0, input_volume: -12.0, output_volume: -8.0,
        clip_repair_count: 0, exciter_option: 0, input_pitch: 0.0, output_pitch: 0.0, speed_adjustment: 1.0,
    })
}
fn sha256(data: &[u8]) -> String {
    platinum_engine::portable::digest::sha256(data).iter().map(|byte| format!("{byte:02x}")).collect()
}
fn options() -> TransferOptions { TransferOptions { copy_artwork: true, ..TransferOptions::default() } }

fn flac(picture: Option<(u32, &str, &str, [u32; 4], &[u8])>) -> Vec<u8> {
    fn block(kind: u8, last: bool, data: &[u8]) -> Vec<u8> {
        let n = data.len();
        let mut v = vec![kind | if last { 128 } else { 0 }, (n >> 16) as u8, (n >> 8) as u8, n as u8];
        v.extend_from_slice(data);
        v
    }
    let mut xiph = 7u32.to_le_bytes().to_vec();
    xiph.extend_from_slice(b"fixture");
    xiph.extend_from_slice(&1u32.to_le_bytes());
    let title = b"TITLE=Art source";
    xiph.extend_from_slice(&(title.len() as u32).to_le_bytes());
    xiph.extend_from_slice(title);
    let mut data = b"fLaC".to_vec();
    data.extend(block(0, false, &[0; 34]));
    data.extend(block(4, picture.is_none(), &xiph));
    if let Some((kind, mime, description, geometry, image)) = picture {
        let mut p = kind.to_be_bytes().to_vec();
        p.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        p.extend_from_slice(mime.as_bytes());
        p.extend_from_slice(&(description.len() as u32).to_be_bytes());
        p.extend_from_slice(description.as_bytes());
        for value in geometry { p.extend_from_slice(&value.to_be_bytes()); }
        p.extend_from_slice(&(image.len() as u32).to_be_bytes());
        p.extend_from_slice(image);
        data.extend(block(6, true, &p));
    }
    data.extend_from_slice(&[0x55; 256]);
    data
}
fn mp3_destination() -> Vec<u8> {
    let mut data = Vec::new();
    for _ in 0..3 { data.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]); data.extend_from_slice(&[0u8; 413]); }
    data
}
fn atom(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(name);
    v.extend_from_slice(body);
    v
}
fn mp4(covers: &[(u32, &[u8])]) -> Vec<u8> {
    let mut covr = Vec::new();
    for (flag, data) in covers {
        let mut body = flag.to_be_bytes().to_vec();
        body.extend_from_slice(&[0; 4]);
        body.extend_from_slice(data);
        covr.extend(atom(b"data", &body));
    }
    let ilst = if covers.is_empty() { Vec::new() } else { atom(b"covr", &covr) };
    let mut meta = vec![0; 4];
    meta.extend(atom(b"ilst", &ilst));
    let mut out = atom(b"ftyp", b"M4A \0\0\0\0M4A ");
    out.extend(atom(b"moov", &atom(b"udta", &atom(b"meta", &meta))));
    out.extend(atom(b"mdat", &[0; 32]));
    out
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn default_policy_still_refuses_generic_artwork() {
    let bridge = bridge();
    let source = Temp::new("flac", &flac(Some((3, "image/png", "", [0; 4], &PNG))));
    let destination = Temp::new("mp3", &mp3_destination());
    let before = destination.bytes();
    let error = bridge.transfer(source.path(), destination.path(), Format::Flac, Format::Mp3, OutputChoice::Explicit, &marker()).unwrap_err();
    assert!(matches!(error, TagError::ArtworkTranscodeUnavailable(_)), "{error:?}");
    assert_eq!(destination.bytes(), before);
    assert!(!TransferOptions::default().copy_artwork);
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn generic_copy_keeps_picture_bytes_type_mime_and_description() {
    let bridge = bridge();
    let description = "Caf\u{e9} \u{8868}\u{7d19}";
    let source = Temp::new("flac", &flac(Some((4, "image/jpg", description, [1, 1, 32, 0], &PNG))));
    let destination = Temp::new("mp3", &mp3_destination());
    let marker = marker();
    let report = bridge.transfer_with_options(source.path(), destination.path(), Format::Flac, Format::Mp3, OutputChoice::Explicit, &marker, options()).unwrap();
    assert!(report.is_success() && report.artwork_copied, "{report:?}");
    let inspection = bridge.inspect(destination.path(), Format::Mp3).unwrap();
    assert_eq!(inspection.artwork.len(), 1);
    let art = &inspection.artwork[0];
    assert_eq!(art.sha256, sha256(&PNG));
    assert_eq!(art.byte_count, PNG.len());
    assert_eq!(art.picture_type, Some(4));
    assert_eq!(art.description.as_deref(), Some(description));
    // The label of the validated bytes wins over the source's "image/jpg".
    assert_eq!(art.mime_type.as_deref(), Some("image/png"));
    assert!(destination.bytes().windows(PNG.len()).any(|w| w == PNG));
    assert_eq!(bridge.read_processing_marker(destination.path(), Format::Mp3).unwrap(), marker.encoded_bytes().unwrap());
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn flac_direct_copy_carries_picture_blocks_and_reports_it() {
    let bridge = bridge();
    let original = flac(Some((6, "image/png", "media", [640, 480, 24, 0], &PNG)));
    let source = Temp::new("flac", &original);
    let destination = Temp::new("flac", &flac(None));
    let plain = bridge.transfer(source.path(), destination.path(), Format::Flac, Format::Flac, OutputChoice::MatchInput, &marker()).unwrap();
    assert!(plain.is_success() && !plain.artwork_copied);
    assert!(bridge.inspect(destination.path(), Format::Flac).unwrap().artwork.iter().all(|a| a.container != "flac_picture"));
    let destination = Temp::new("flac", &flac(None));
    let report = bridge.transfer_with_options(source.path(), destination.path(), Format::Flac, Format::Flac, OutputChoice::MatchInput, &marker(), options()).unwrap();
    assert!(report.is_success() && report.artwork_copied, "{report:?}");
    assert_eq!(source.bytes(), original);
    let pictures: Vec<_> = bridge.inspect(destination.path(), Format::Flac).unwrap().artwork.into_iter().filter(|a| a.container == "flac_picture").collect();
    assert_eq!(pictures.len(), 1);
    assert_eq!((pictures[0].sha256.as_str(), pictures[0].picture_type, pictures[0].description.as_deref()), (sha256(&PNG).as_str(), Some(6), Some("media")));
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn mp4_cover_becomes_a_front_cover_picture() {
    let bridge = bridge();
    let source = Temp::new("m4a", &mp4(&[(14, &PNG)]));
    let destination = Temp::new("flac", &flac(None));
    let report = bridge.transfer_with_options(source.path(), destination.path(), Format::Mp4, Format::Flac, OutputChoice::Explicit, &marker(), options()).unwrap();
    assert!(report.is_success() && report.artwork_copied, "{report:?}");
    let pictures: Vec<_> = bridge.inspect(destination.path(), Format::Flac).unwrap().artwork.into_iter().filter(|a| a.container == "flac_picture").collect();
    assert_eq!(pictures.len(), 1);
    assert_eq!((pictures[0].picture_type, pictures[0].mime_type.as_deref(), pictures[0].sha256.as_str()), (Some(3), Some("image/png"), sha256(&PNG).as_str()));
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn refusals_name_the_reason_and_leave_the_destination_untouched() {
    let bridge = bridge();
    let truncated = &PNG[..PNG.len() - 12];
    let cases: [(&str, Format, Vec<u8>, &str); 2] = [
        ("mp3", Format::Mp3, flac(Some((3, "image/png", "", [0; 4], truncated))), "not a complete"),
        ("m4a", Format::Mp4, flac(Some((3, "image/webp", "", [0; 4], &WEBP))), "WebP"),
    ];
    for (ext, format, source_bytes, reason) in cases {
        let source = Temp::new("flac", &source_bytes);
        let destination = Temp::new(ext, &if ext == "mp3" { mp3_destination() } else { mp4(&[]) });
        let before = destination.bytes();
        let error = bridge.transfer_with_options(source.path(), destination.path(), Format::Flac, format, OutputChoice::Explicit, &marker(), options()).unwrap_err();
        match &error {
            TagError::ArtworkTranscodeUnavailable(message) => assert!(message.contains(reason), "{message}"),
            other => panic!("unexpected error {other:?}"),
        }
        assert_eq!(destination.bytes(), before, "{ext}: destination changed by a refused copy");
    }
}
