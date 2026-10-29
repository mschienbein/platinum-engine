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

// ---------------------------------------------------------------------------------------------------------------
// v09: WAV is an ID3-capable container under the opt-in; the ID3v2 size limit is exact.

#[path = "support/id3_art.rs"]
mod id3_art;
use id3_art::{FlacPicture, apic_frame, apic_overhead, id3v24_tag, text_frame};
use platinum_engine::tags::TagInspection;
use std::sync::Mutex;

/// What the engine's opt-in asks of the bridge: copy pictures and keep the chunks TagLib wrote into a WAV.
fn wav_options() -> TransferOptions { TransferOptions { copy_artwork: true, preserve_wav_chunks: true } }

fn id3_source_tag(pictures: &[Vec<u8>]) -> Vec<u8> {
    let mut frames = vec![text_frame(b"TIT2", "Art source"), text_frame(b"TPE1", "Art artist"), text_frame(b"TBPM", "128"), text_frame(b"TKEY", "Am")];
    frames.extend_from_slice(pictures);
    id3v24_tag(&frames)
}
fn mp3_with(tag: &[u8]) -> Vec<u8> { [tag, &mp3_destination()].concat() }

fn pictures_of<'a>(inspection: &'a TagInspection, container: &str) -> Vec<&'a platinum_engine::tags::ArtworkInspection> {
    inspection.artwork.iter().filter(|art| art.container == container).collect()
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn wav_destination_stores_pictures_from_every_source_with_the_opt_in() {
    let bridge = bridge();
    let marker = marker();
    let sources: [(&str, Format, Vec<u8>, u32, &str); 4] = [
        ("flac", Format::Flac, id3_art::flac_with_pictures(&[FlacPicture { kind: 3, mime: "image/png", description: "front", geometry: [1, 1, 32, 0], data: &PNG }]), 3, "front"),
        ("mp3", Format::Mp3, mp3_with(&id3_source_tag(&[apic_frame("image/png", 4, "back", &PNG, false)])), 4, "back"),
        ("aiff", Format::Aiff, id3_art::aiff(Some(&id3_source_tag(&[apic_frame("image/png", 5, "leaflet", &PNG, true)]))), 5, "leaflet"),
        ("m4a", Format::Mp4, mp4(&[(14, &PNG)]), 3, ""),
    ];
    for (ext, format, bytes, picture_type, description) in sources {
        let source = Temp::new(ext, &bytes);
        let base = id3_art::wav(None);
        let destination = Temp::new("wav", &base);
        let report = bridge.transfer_with_options(source.path(), destination.path(), format, Format::Wav, OutputChoice::Explicit, &marker, wav_options()).unwrap();
        assert!(report.is_success() && report.artwork_copied && report.wav_tags_preserved, "{ext}: {report:?}");
        let output = destination.bytes();
        let offset = id3_art::riff_id3_offset(&output, false).unwrap_or_else(|| panic!("{ext}: the ID3 chunk must survive the marker rewrite"));
        let apics = id3_art::parse_apics(&output, offset);
        assert_eq!(apics.len(), 1, "{ext}");
        assert_eq!(&output[apics[0].data.clone()], &PNG[..], "{ext}: the picture bytes are unchanged");
        assert_eq!((apics[0].picture_type as u32, apics[0].mime.as_str(), apics[0].description.as_str()), (picture_type, "image/png", description), "{ext}");
        assert_eq!(id3_art::riff_audio(&output, false), id3_art::riff_audio(&base, false), "{ext}: the audio payload is unchanged");
        let inspection = bridge.inspect(destination.path(), Format::Wav).unwrap();
        let art = pictures_of(&inspection, "id3v2");
        assert_eq!(art.len(), 1, "{ext}: {inspection:?}");
        assert_eq!((art[0].sha256.as_str(), art[0].byte_count), (sha256(&PNG).as_str(), PNG.len()), "{ext}");
        let stored = bridge.read_processing_marker(destination.path(), Format::Wav).unwrap().expect("the processed marker");
        let wanted = marker.encoded_bytes().unwrap().unwrap();
        assert!(stored.starts_with(&wanted) && stored.len() <= wanted.len() + 1, "{ext}: marker {stored:02x?}");
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn wav_source_pictures_reach_every_destination_with_the_opt_in() {
    let bridge = bridge();
    let tag = id3_source_tag(&[apic_frame("image/png", 4, "back", &PNG, true), apic_frame("image/webp", 6, "media", &WEBP, false)]);
    let source_bytes = id3_art::wav(Some(&tag));
    let source = Temp::new("wav", &source_bytes);
    // (extension, format, empty destination, container of its pictures, can it store WebP)
    let destinations: [(&str, Format, Vec<u8>, &str, bool); 4] = [
        ("flac", Format::Flac, id3_art::flac_with_pictures(&[]), "flac_picture", true),
        ("mp3", Format::Mp3, mp3_destination(), "id3v2", true),
        ("aiff", Format::Aiff, id3_art::aiff(None), "id3v2", true),
        ("m4a", Format::Mp4, mp4(&[]), "mp4_covr", false),
    ];
    for (ext, format, base, container, webp) in destinations {
        let destination = Temp::new(ext, &base);
        let attempt = bridge.transfer_with_options(source.path(), destination.path(), Format::Wav, format, OutputChoice::Explicit, &marker(), options());
        if !webp {
            // An MP4 covr atom cannot hold the WebP picture: refused before anything is written.
            match attempt.unwrap_err() {
                TagError::ArtworkTranscodeUnavailable(message) => assert!(message.contains("WebP"), "{message}"),
                other => panic!("{ext}: {other:?}"),
            }
            assert_eq!(destination.bytes(), base, "{ext}: a refused copy leaves the destination alone");
            continue;
        }
        let report = attempt.unwrap();
        assert!(report.is_success() && report.artwork_copied, "{ext}: {report:?}");
        let inspection = bridge.inspect(destination.path(), format).unwrap();
        let art = pictures_of(&inspection, container);
        assert_eq!(art.len(), 2, "{ext}: {inspection:?}");
        let shas = [sha256(&PNG), sha256(&WEBP)];
        for (index, (want_type, want_mime, want_description)) in [(4, "image/png", "back"), (6, "image/webp", "media")].into_iter().enumerate() {
            assert_eq!(art[index].sha256, shas[index], "{ext} picture {index}");
            assert_eq!((art[index].picture_type, art[index].mime_type.as_deref(), art[index].description.as_deref()), (Some(want_type), Some(want_mime), Some(want_description)), "{ext} picture {index}");
        }
    }
    assert_eq!(source.bytes(), source_bytes, "the source is never modified");
    // The one picture MP4 can store: the WebP is dropped from the set by the caller, so only the PNG travels.
    let png_only = Temp::new("wav", &id3_art::wav(Some(&id3_source_tag(&[apic_frame("image/png", 4, "back", &PNG, true)]))));
    let destination = Temp::new("m4a", &mp4(&[]));
    let report = bridge.transfer_with_options(png_only.path(), destination.path(), Format::Wav, Format::Mp4, OutputChoice::Explicit, &marker(), options()).unwrap();
    assert!(report.is_success() && report.artwork_copied, "{report:?}");
    let inspection = bridge.inspect(destination.path(), Format::Mp4).unwrap();
    let art = pictures_of(&inspection, "mp4_covr");
    assert_eq!((art.len(), art[0].sha256.as_str(), art[0].format_code), (1, sha256(&PNG).as_str(), Some(14)));
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn without_the_opt_in_no_picture_moves_into_or_out_of_a_wav() {
    let bridge = bridge();
    // FLAC -> WAV: the picture is ignored (not refused), as in the reference.
    let source = Temp::new("flac", &flac(Some((3, "image/png", "", [0; 4], &PNG))));
    let destination = Temp::new("wav", &id3_art::wav(None));
    let report = bridge.transfer(source.path(), destination.path(), Format::Flac, Format::Wav, OutputChoice::Explicit, &marker()).unwrap();
    assert!(report.is_success() && !report.artwork_copied, "{report:?}");
    assert!(bridge.inspect(destination.path(), Format::Wav).unwrap().artwork.is_empty());
    // WAV -> FLAC, MP3, AIFF, MP4: a WAV's APIC frame is not a source picture (nothing is refused, nothing is copied).
    let tag = id3_source_tag(&[apic_frame("image/png", 3, "front", &PNG, false)]);
    let wav_source = Temp::new("wav", &id3_art::wav(Some(&tag)));
    let destinations: [(&str, Format, Vec<u8>); 4] = [
        ("flac", Format::Flac, id3_art::flac_with_pictures(&[])), ("mp3", Format::Mp3, mp3_destination()),
        ("aiff", Format::Aiff, id3_art::aiff(None)), ("m4a", Format::Mp4, mp4(&[])),
    ];
    for (ext, format, base) in destinations {
        let destination = Temp::new(ext, &base);
        let report = bridge.transfer(wav_source.path(), destination.path(), Format::Wav, format, OutputChoice::Explicit, &marker()).unwrap();
        assert!(report.is_success() && !report.artwork_copied, "{ext}: {report:?}");
        let inspection = bridge.inspect(destination.path(), format).unwrap();
        assert!(inspection.artwork.is_empty(), "{ext}: {inspection:?}");
    }
    // The opt-in on the bridge call alone (no chunk preservation) changes what is stored, not what is refused.
    let destination = Temp::new("flac", &id3_art::flac_with_pictures(&[]));
    let report = bridge.transfer_with_options(wav_source.path(), destination.path(), Format::Wav, Format::Flac, OutputChoice::Explicit, &marker(), options()).unwrap();
    assert!(report.is_success() && report.artwork_copied, "{report:?}");
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension"]
fn wav_pictures_are_validated_like_mp3_and_aiff_pictures() {
    let bridge = bridge();
    let truncated = &PNG[..PNG.len() - 12];
    let bad_flac = id3_art::flac_with_pictures(&[FlacPicture { kind: 3, mime: "image/png", description: "", geometry: [0; 4], data: truncated }]);
    let bad_wav = id3_art::wav(Some(&id3_source_tag(&[apic_frame("image/png", 3, "", truncated, false)])));
    let cases: [(&str, Format, &Vec<u8>, &str, Format, Vec<u8>); 4] = [
        ("flac", Format::Flac, &bad_flac, "wav", Format::Wav, id3_art::wav(None)),
        ("wav", Format::Wav, &bad_wav, "mp3", Format::Mp3, mp3_destination()),
        ("wav", Format::Wav, &bad_wav, "flac", Format::Flac, id3_art::flac_with_pictures(&[])),
        ("wav", Format::Wav, &bad_wav, "wav", Format::Wav, id3_art::wav(None)),
    ];
    for (source_ext, source_format, source_bytes, ext, format, base) in cases {
        let source = Temp::new(source_ext, source_bytes);
        let destination = Temp::new(ext, &base);
        let before = destination.bytes();
        let attempt = bridge.transfer_with_options(source.path(), destination.path(), source_format, format, OutputChoice::Explicit, &marker(), wav_options());
        // A WAV to WAV transfer is a direct copy: the reference moves the frames unchanged and nothing is validated.
        if (source_format, format) == (Format::Wav, Format::Wav) {
            assert!(attempt.unwrap().is_success());
            continue;
        }
        match attempt.unwrap_err() {
            TagError::ArtworkTranscodeUnavailable(message) => assert!(message.contains("not a complete"), "{source_ext}->{ext}: {message}"),
            other => panic!("{source_ext}->{ext}: {other:?}"),
        }
        assert_eq!(destination.bytes(), before, "{source_ext}->{ext}: destination changed by a refused copy");
    }
}

/// Serializes the tests that build ~270 MB tags: their peak memory is several times their file size.
static HEAVY: Mutex<()> = Mutex::new(());

fn heavy() -> std::sync::MutexGuard<'static, ()> { HEAVY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) }

fn limit_destination(format: Format) -> (&'static str, Vec<u8>) {
    match format {
        Format::Mp3 => ("mp3", mp3_destination()),
        Format::Aiff => ("aiff", id3_art::aiff(None)),
        Format::Wav => ("wav", id3_art::wav(None)),
        other => panic!("{other:?} has no ID3v2 tag size limit"),
    }
}
fn tag_offset(output: &[u8], format: Format) -> usize {
    match format {
        Format::Mp3 => 0,
        Format::Aiff => id3_art::riff_id3_offset(output, true).expect("an ID3 chunk"),
        _ => id3_art::riff_id3_offset(output, false).expect("an ID3 chunk"),
    }
}
fn audio_of(output: &[u8], format: Format) -> Vec<u8> {
    match format {
        Format::Mp3 => output[id3_art::tag_field(output, 0).map_or(0, |field| field + 10)..].to_vec(),
        Format::Aiff => id3_art::riff_audio(output, true).to_vec(),
        _ => id3_art::riff_audio(output, false).to_vec(),
    }
}

/// The content bytes (frames, no padding) the bridge writes into a fresh destination of `format` for a source
/// without pictures: the text frames and, for MP3 and AIFF, the processed-marker frame.
fn blank_content(bridge: &TagBridge, format: Format) -> usize {
    let source = Temp::new("flac", &id3_art::flac_with_pictures(&[]));
    let (ext, base) = limit_destination(format);
    let destination = Temp::new(ext, &base);
    let report = bridge.transfer_with_options(source.path(), destination.path(), Format::Flac, format, OutputChoice::Explicit, &marker(), wav_options()).unwrap();
    assert!(report.is_success(), "{report:?}");
    let output = destination.bytes();
    id3_art::tag_field(&output, tag_offset(&output, format)).expect("an ID3 tag") - id3_art::ID3_PADDING
}

/// A copy whose rendered tag is `target` bytes of frames (plus the 1024-byte padding TagLib adds) once the picture sizes
/// make it so; `sizes` are picture data sizes, `descriptions` their descriptions (all Latin-1 or all UTF-16).
fn last_picture_size(blank: usize, target: usize, fixed: &[usize], descriptions: &[String], utf16: bool) -> usize {
    let overhead = |index: usize| apic_overhead("image/png", &descriptions[index], utf16);
    let used: usize = fixed.iter().enumerate().map(|(index, size)| overhead(index) + size).sum();
    let last = fixed.len();
    target - blank - used - overhead(last)
}

/// One copy at the size limit: sixteen near-maximal FLAC pictures (the case the v08 preflight let through) into `format`.
/// `over` is how many bytes the rendered tag exceeds the 268435455-byte limit (0 = exactly at it).
fn flac_pictures_into(bridge: &TagBridge, format: Format, blank: usize, over: usize) -> Result<(), String> {
    let descriptions: Vec<String> = (1..=16).map(|n| format!("cover {n}")).collect();
    let fixed: Vec<usize> = descriptions[..15].iter().map(|d| id3_art::FLAC_BLOCK_LIMIT - 32 - "image/png".len() - d.len()).collect();
    let target = id3_art::ID3_LIMIT - id3_art::ID3_PADDING + over;
    let last = last_picture_size(blank, target, &fixed, &descriptions, false);
    assert!(last >= PNG.len() && 32 + 9 + descriptions[15].len() + last <= id3_art::FLAC_BLOCK_LIMIT, "last picture of {last} bytes");
    let buffer = id3_art::padded(&PNG, id3_art::FLAC_BLOCK_LIMIT);
    let sizes: Vec<usize> = fixed.iter().copied().chain([last]).collect();
    let pictures: Vec<FlacPicture> = sizes.iter().zip(&descriptions)
        .map(|(size, description)| FlacPicture { kind: 3, mime: "image/png", description, geometry: [1, 1, 32, 0], data: &buffer[..*size] }).collect();
    let source = Temp::new("flac", &id3_art::flac_with_pictures(&pictures));
    drop(pictures);
    let (ext, base) = limit_destination(format);
    let destination = Temp::new(ext, &base);
    let result = bridge.transfer_with_options(source.path(), destination.path(), Format::Flac, format, OutputChoice::Explicit, &marker(), wav_options());
    if over > 0 {
        match result {
            Err(TagError::ArtworkTranscodeUnavailable(message)) => assert!(message.contains("ID3v2 tag size limit"), "{message}"),
            other => return Err(format!("expected the ID3v2 size refusal, got {other:?}")),
        }
        assert_eq!(destination.bytes(), base, "{format:?}: a refused copy writes nothing");
        return Ok(());
    }
    let report = result.map_err(|error| format!("the exact limit must be accepted: {error}"))?;
    assert!(report.is_success() && report.artwork_copied, "{report:?}");
    let output = destination.bytes();
    let offset = tag_offset(&output, format);
    assert_eq!(id3_art::tag_field(&output, offset), Some(id3_art::ID3_LIMIT), "the size field is exactly the limit, not wrapped");
    let apics = id3_art::parse_apics(&output, offset);
    assert_eq!(apics.len(), 16);
    for (index, (apic, size)) in apics.iter().zip(&sizes).enumerate() {
        assert_eq!(&output[apic.data.clone()], &buffer[..*size], "picture {}", index + 1);
        assert_eq!((apic.picture_type, apic.mime.as_str(), apic.description.as_str()), (3, "image/png", descriptions[index].as_str()));
    }
    assert_eq!(audio_of(&output, format), audio_of(&base, format), "the audio payload is unchanged");
    // TagLib re-reads the finished file.
    let inspection = bridge.inspect(destination.path(), format).unwrap();
    let art = pictures_of(&inspection, "id3v2");
    assert_eq!(art.len(), 16, "{format:?}");
    assert!(art.iter().zip(&sizes).all(|(a, size)| a.byte_count == *size));
    assert_eq!(art[15].sha256, sha256(&buffer[..sizes[15]]));
    let stored = bridge.read_processing_marker(destination.path(), format).unwrap().expect("the processed marker");
    assert!(stored.starts_with(&marker().encoded_bytes().unwrap().unwrap()));
    Ok(())
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension; builds ~270 MB tags"]
fn sixteen_flac_pictures_fit_the_id3_limit_exactly_and_one_byte_more_is_refused() {
    let _guard = heavy();
    let bridge = bridge();
    for format in [Format::Mp3, Format::Aiff, Format::Wav] {
        let blank = blank_content(&bridge, format);
        flac_pictures_into(&bridge, format, blank, 0).unwrap_or_else(|e| panic!("{format:?}: {e}"));
        flac_pictures_into(&bridge, format, blank, 1).unwrap_or_else(|e| panic!("{format:?}: {e}"));
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension; builds ~270 MB tags"]
fn pictures_beyond_the_id3_limit_by_their_bytes_alone_are_refused_before_any_write() {
    let _guard = heavy();
    let bridge = bridge();
    // Seventeen maximal FLAC pictures: more picture bytes than any ID3v2 tag can hold.
    let buffer = id3_art::padded(&PNG, id3_art::FLAC_BLOCK_LIMIT);
    let descriptions: Vec<String> = (1..=17).map(|n| format!("cover {n}")).collect();
    let pictures: Vec<FlacPicture> = descriptions.iter()
        .map(|d| FlacPicture { kind: 3, mime: "image/png", description: d, geometry: [1, 1, 32, 0], data: &buffer[..id3_art::FLAC_BLOCK_LIMIT - 32 - 9 - d.len()] }).collect();
    let source = Temp::new("flac", &id3_art::flac_with_pictures(&pictures));
    drop(pictures);
    for format in [Format::Mp3, Format::Aiff, Format::Wav] {
        let (ext, base) = limit_destination(format);
        let destination = Temp::new(ext, &base);
        match bridge.transfer_with_options(source.path(), destination.path(), Format::Flac, format, OutputChoice::Explicit, &marker(), wav_options()) {
            Err(TagError::ArtworkTranscodeUnavailable(message)) => assert!(message.contains("ID3v2 tag size limit"), "{format:?}: {message}"),
            other => panic!("{format:?}: {other:?}"),
        }
        assert_eq!(destination.bytes(), base, "{format:?}");
    }
}

/// Eight ID3 pictures with UTF-16 descriptions (two bytes per character, which the v08 preflight ignored) into a destination
/// that already holds a large non-picture frame: the rendered tag is exactly at the limit, or one byte over it.
fn utf16_pictures_into(bridge: &TagBridge, format: Format, blank: usize, over: usize) -> Result<(), String> {
    const PRIV: usize = 120_000_000;
    let description = "\u{8868}\u{7d19}\u{306e}\u{8aac}\u{660e} ".repeat(100_000);
    let descriptions: Vec<String> = (0..8).map(|n| description.chars().take(description.chars().count() - n).collect()).collect();
    let fixed = vec![8 * 1024 * 1024usize; 7];
    // The destination's own PRIV frame counts toward the tag.
    let priv_frame = id3_art::frame(b"PRIV", &[b"platinum-test\0".to_vec(), vec![0u8; PRIV]].concat());
    let target = id3_art::ID3_LIMIT - id3_art::ID3_PADDING + over;
    let last = last_picture_size(blank + priv_frame.len(), target, &fixed, &descriptions, true);
    assert!(last >= PNG.len(), "last picture of {last} bytes");
    let buffer = id3_art::padded(&PNG, fixed[0].max(last));
    let sizes: Vec<usize> = fixed.iter().copied().chain([last]).collect();
    let apics: Vec<Vec<u8>> = sizes.iter().zip(&descriptions).enumerate()
        .map(|(index, (size, d))| apic_frame("image/png", (index % 21) as u8, d, &buffer[..*size], true)).collect();
    // A source of another format than the destination, so that the copy is generic: a WAV (itself an ID3 picture source
    // since v09), or an AIFF into a WAV.
    let (source_format, source_ext, source_bytes) = if format == Format::Wav {
        (Format::Aiff, "aiff", id3_art::aiff(Some(&id3_source_tag(&apics))))
    } else {
        (Format::Wav, "wav", id3_art::wav(Some(&id3_source_tag(&apics))))
    };
    let source = Temp::new(source_ext, &source_bytes);
    drop(source_bytes);
    drop(apics);
    let base = {
        let tag = id3v24_tag(&[priv_frame]);
        match format {
            Format::Mp3 => [tag, mp3_destination()].concat(),
            Format::Aiff => id3_art::aiff(Some(&tag)),
            _ => id3_art::wav(Some(&tag)),
        }
    };
    let ext = limit_destination(format).0;
    let destination = Temp::new(ext, &base);
    let result = bridge.transfer_with_options(source.path(), destination.path(), source_format, format, OutputChoice::Explicit, &marker(), wav_options());
    if over > 0 {
        match result {
            Err(TagError::ArtworkTranscodeUnavailable(message)) => assert!(message.contains("ID3v2 tag size limit"), "{message}"),
            other => return Err(format!("expected the ID3v2 size refusal, got {other:?}")),
        }
        assert_eq!(destination.bytes(), base, "{format:?}: a refused copy writes nothing");
        return Ok(());
    }
    let report = result.map_err(|error| format!("the exact limit must be accepted: {error}"))?;
    assert!(report.is_success() && report.artwork_copied, "{report:?}");
    let output = destination.bytes();
    let offset = tag_offset(&output, format);
    assert_eq!(id3_art::tag_field(&output, offset), Some(id3_art::ID3_LIMIT));
    let stored = id3_art::parse_apics(&output, offset);
    assert_eq!(stored.len(), 8);
    for (index, (apic, size)) in stored.iter().zip(&sizes).enumerate() {
        assert_eq!(&output[apic.data.clone()], &buffer[..*size], "picture {}", index + 1);
        assert_eq!(apic.description, descriptions[index], "the UTF-16 description of picture {} is unchanged", index + 1);
    }
    let inspection = bridge.inspect(destination.path(), format).unwrap();
    assert_eq!(pictures_of(&inspection, "id3v2").len(), 8);
    Ok(())
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge with the artwork extension; builds ~270 MB tags"]
fn utf16_descriptions_and_destination_frames_count_toward_the_id3_limit() {
    let _guard = heavy();
    let bridge = bridge();
    for format in [Format::Mp3, Format::Wav] {
        let blank = blank_content(&bridge, format);
        utf16_pictures_into(&bridge, format, blank, 0).unwrap_or_else(|e| panic!("{format:?}: {e}"));
        utf16_pictures_into(&bridge, format, blank, 1).unwrap_or_else(|e| panic!("{format:?}: {e}"));
    }
}
