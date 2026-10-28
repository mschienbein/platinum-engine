use platinum_engine::tags::{transfer_mode, DirectCopy, Format, OutputChoice, TagError, TransferMode};

#[test]
fn pn_dispatch_distinguishes_same_format_and_mp4_to_mp3() {
    use Format::*;
    for (f, direct) in [(Mp3, DirectCopy::Id3), (Mp4, DirectCopy::Mp4), (Wav, DirectCopy::Wav),
                        (Aiff, DirectCopy::Aiff), (Flac, DirectCopy::Flac)] {
        assert_eq!(transfer_mode(f, f, OutputChoice::Explicit).unwrap(), TransferMode::Direct(direct));
        assert_eq!(transfer_mode(f, f, OutputChoice::MatchInput).unwrap(), TransferMode::Direct(direct));
    }
    assert_eq!(transfer_mode(Mp4, Mp3, OutputChoice::Explicit).unwrap(), TransferMode::Direct(DirectCopy::Mp4ToId3));
    for source in [Mp3, Mp4, Wav, Aiff, Flac] {
        for destination in [Mp3, Mp4, Wav, Aiff, Flac] {
            let expected = match source {
                Mp3 => DirectCopy::Id3, Mp4 => DirectCopy::Mp4, Wav => DirectCopy::Wav,
                Aiff => DirectCopy::Aiff, Flac => DirectCopy::Flac,
            };
            assert_eq!(transfer_mode(source, destination, OutputChoice::MatchInput).unwrap(), TransferMode::Direct(expected));
            if source != destination && (source != Mp4 || destination != Mp3) {
                assert_eq!(transfer_mode(source, destination, OutputChoice::Explicit).unwrap(), TransferMode::Generic);
            }
        }
    }
}

// Only synthetic byte fixtures are used here. The tests exercise the public
// TagLib candidate bridge; they do not invoke any proprietary binary and do
// not establish whole-file parity with the original application.
mod candidate_bridge {
    use super::*;
    use platinum_engine::metadata::{self, Marker, ProcessingMarker};
    use platinum_engine::tags::TagBridge;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new(ext: &str, data: &[u8]) -> Self {
            loop {
                let path = std::env::temp_dir().join(format!("platinum-tag-{}-{}.{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed), ext));
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(mut f) => { f.write_all(data).unwrap(); return Self(path); }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("create fixture: {e}"),
                }
            }
        }
        fn path(&self) -> &Path { &self.0 }
        fn bytes(&self) -> Vec<u8> { fs::read(&self.0).unwrap() }
    }
    impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_file(&self.0); } }
    fn library() -> TagBridge {
        let path = PathBuf::from(std::env::var_os("PN_TAG_BRIDGE").expect("set PN_TAG_BRIDGE to the trusted, separately built open-source candidate bridge"));
        let expected = platinum_engine::mp3::file_sha256(&path).unwrap();
        // SAFETY: opt-in test requires the caller to select a trusted bridge.
        let bridge = unsafe { TagBridge::load(&path).unwrap() };
        let observed: String = bridge.bridge_sha256().iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(observed, expected);
        bridge
    }
    fn marker() -> Marker {
        Marker::from_processing(ProcessingMarker {
            software_version: Some("10.5.0.1256"), backup_file_name: None,
            applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
            expansion_applied: 0, compression_applied: 1, volume_analysis_version: 4,
            target_volume: -8.0, input_volume: -12.0, output_volume: -8.0,
            clip_repair_count: 0, exciter_option: 0, input_pitch: 0.0, output_pitch: 0.0,
            speed_adjustment: 1.0,
        })
    }
    fn synchsafe(n: usize) -> [u8; 4] { [((n >> 21) & 127) as u8, ((n >> 14) & 127) as u8, ((n >> 7) & 127) as u8, (n & 127) as u8] }
    fn frame(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec(); f.extend_from_slice(&synchsafe(body.len())); f.extend_from_slice(&[0, 0]); f.extend_from_slice(body); f
    }
    fn tag(frames: &[Vec<u8>]) -> Vec<u8> {
        let bytes: Vec<u8> = frames.iter().flatten().copied().collect();
        let mut tag = b"ID3\x04\x00\x00".to_vec(); tag.extend_from_slice(&synchsafe(bytes.len())); tag.extend(bytes); tag
    }
    fn text(id: &[u8; 4], s: &str) -> Vec<u8> { let mut b = vec![3]; b.extend_from_slice(s.as_bytes()); frame(id, &b) }
    fn comment(description: &str, text: &str) -> Vec<u8> {
        let mut b = b"\x00eng".to_vec(); b.extend_from_slice(description.as_bytes()); b.push(0); b.extend_from_slice(text.as_bytes()); frame(b"COMM", &b)
    }
    fn geob(description: &str, object: &[u8]) -> Vec<u8> {
        let mut b = b"\x00application/json\0\0".to_vec(); b.extend_from_slice(description.as_bytes()); b.push(0); b.extend_from_slice(object); frame(b"GEOB", &b)
    }
    fn mp3(id3: &[u8]) -> Vec<u8> {
        let mut data = id3.to_vec();
        // TagLib is opened without audio-property decoding. This synthetic byte
        // sequence gives a recognizable MPEG sync without carrying user audio.
        for _ in 0..3 { data.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]); data.extend_from_slice(&[0u8; 413]); }
        data
    }
    fn riff_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec(); v.extend_from_slice(&(body.len() as u32).to_le_bytes()); v.extend_from_slice(body); if body.len() & 1 == 1 { v.push(0); } v
    }
    fn wav(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut body = b"WAVE".to_vec(); for c in chunks { body.extend(c); }
        let mut result = b"RIFF".to_vec(); result.extend_from_slice(&(body.len() as u32).to_le_bytes()); result.extend(body); result
    }
    fn base_wav(extra: &[Vec<u8>]) -> Vec<u8> {
        let fmt = [1,0,1,0,0x80,0xbb,0,0,0,0x77,1,0,2,0,16,0];
        let mut chunks = vec![riff_chunk(b"fmt ", &fmt), riff_chunk(b"data", &[0, 0, 0, 0])]; chunks.extend_from_slice(extra); wav(&chunks)
    }
    fn aiff_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec(); v.extend_from_slice(&(body.len() as u32).to_be_bytes()); v.extend_from_slice(body);
        if body.len() & 1 == 1 { v.push(0); } v
    }
    fn aiff(id3: Option<&[u8]>) -> Vec<u8> {
        let comm = [0,1, 0,0,0,2, 0,16, 0x40,0x0e,0xac,0x44,0,0,0,0,0,0];
        let mut body = b"AIFF".to_vec(); body.extend(aiff_chunk(b"COMM", &comm)); body.extend(aiff_chunk(b"SSND", &[0; 12]));
        if let Some(data) = id3 { body.extend(aiff_chunk(b"ID3 ", data)); }
        let mut result = b"FORM".to_vec(); result.extend_from_slice(&(body.len() as u32).to_be_bytes()); result.extend(body); result
    }
    fn aiff_id3(data: &[u8]) -> &[u8] {
        assert_eq!(&data[..4], b"FORM"); assert_eq!(&data[8..12], b"AIFF");
        assert_eq!(u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize + 8, data.len());
        let mut p = 12;
        while p + 8 <= data.len() {
            let n = u32::from_be_bytes(data[p+4..p+8].try_into().unwrap()) as usize; assert!(p + 8 + n <= data.len());
            if &data[p..p+4] == b"ID3 " { return &data[p+8..p+8+n]; } p += 8 + n + (n & 1);
        }
        panic!("missing AIFF ID3 chunk")
    }
    fn chunks(data: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        assert_eq!(&data[..4], b"RIFF"); assert_eq!(&data[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize + 8, data.len());
        let mut out = Vec::new(); let mut p = 12;
        while p + 8 <= data.len() {
            let id = data[p..p+4].try_into().unwrap(); let n = u32::from_le_bytes(data[p+4..p+8].try_into().unwrap()) as usize;
            assert!(p + 8 + n <= data.len()); out.push((id, data[p+8..p+8+n].to_vec())); p += 8 + n + (n & 1);
        }
        assert_eq!(p, data.len()); out
    }
    fn unsync(bytes: &[u8]) -> usize { bytes.iter().fold(0usize, |n, b| (n << 7) | usize::from(b & 0x7f)) }
    fn frames(data: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        assert_eq!(&data[..3], b"ID3"); assert_eq!(data[3], 4);
        let end = 10 + unsync(&data[6..10]); assert!(end <= data.len());
        let mut out = Vec::new(); let mut p = 10;
        while p + 10 <= end && data[p] != 0 {
            let id = data[p..p+4].try_into().unwrap(); let size = unsync(&data[p+4..p+8]);
            assert!(p + 10 + size <= end); out.push((id, data[p+10..p+10+size].to_vec())); p += 10 + size;
        }
        out
    }
    fn has_id(frames: &[([u8; 4], Vec<u8>)], id: &[u8; 4]) -> bool { frames.iter().any(|(k, _)| k == id) }
    fn contains(data: &[u8], needle: &[u8]) -> bool { data.windows(needle.len()).any(|w| w == needle) }

    fn atom(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec(); v.extend_from_slice(id); v.extend_from_slice(body); v
    }
    fn mp4_item(id: &[u8; 4], kind: u32, data: &[u8]) -> Vec<u8> {
        let mut body = kind.to_be_bytes().to_vec(); body.extend_from_slice(&[0; 4]); body.extend_from_slice(data);
        atom(id, &atom(b"data", &body))
    }
    fn freeform(name: &str, value: &[u8]) -> Vec<u8> {
        let mut mean = vec![0; 4]; mean.extend_from_slice(b"com.apple.iTunes");
        let mut name_data = vec![0; 4]; name_data.extend_from_slice(name.as_bytes());
        let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0]; data.extend_from_slice(value);
        let mut b = atom(b"mean", &mean); b.extend(atom(b"name", &name_data)); b.extend(atom(b"data", &data));
        atom(b"----", &b)
    }
    fn mp4(items: &[Vec<u8>]) -> Vec<u8> {
        let ilst: Vec<u8> = items.iter().flatten().copied().collect();
        let mut meta = vec![0; 4]; meta.extend(atom(b"ilst", &ilst));
        let mut out = atom(b"ftyp", b"M4A \0\0\0\0M4A ");
        out.extend(atom(b"moov", &atom(b"udta", &atom(b"meta", &meta))));
        out.extend(atom(b"mdat", &[0; 32])); out
    }
    fn flac(comments: &[&str], picture: Option<&[u8]>) -> Vec<u8> {
        fn block(kind: u8, last: bool, data: &[u8]) -> Vec<u8> {
            let n = data.len(); let mut v = vec![kind | if last { 128 } else { 0 }, (n >> 16) as u8, (n >> 8) as u8, n as u8];
            v.extend_from_slice(data); v
        }
        let mut xiph = 7u32.to_le_bytes().to_vec(); xiph.extend_from_slice(b"fixture");
        xiph.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for s in comments { xiph.extend_from_slice(&(s.len() as u32).to_le_bytes()); xiph.extend_from_slice(s.as_bytes()); }
        let mut data = b"fLaC".to_vec(); data.extend(block(0, false, &[0; 34])); data.extend(block(4, picture.is_none(), &xiph));
        if let Some(image) = picture {
            let mut p = 3u32.to_be_bytes().to_vec(); p.extend_from_slice(&9u32.to_be_bytes()); p.extend_from_slice(b"image/png");
            p.extend_from_slice(&0u32.to_be_bytes()); p.extend_from_slice(&[0; 16]); p.extend_from_slice(&(image.len() as u32).to_be_bytes()); p.extend_from_slice(image);
            data.extend(block(6, true, &p));
        }
        data.extend_from_slice(&[0x55; 256]); data
    }
    fn flac_block_types(data: &[u8]) -> Vec<u8> {
        assert_eq!(&data[..4], b"fLaC"); let mut p = 4; let mut types = Vec::new();
        loop {
            let flags = data[p]; let n = (usize::from(data[p+1]) << 16) | (usize::from(data[p+2]) << 8) | usize::from(data[p+3]);
            assert!(p + 4 + n <= data.len()); types.push(flags & 127); p += 4 + n; if flags & 128 != 0 { break; }
        }
        types
    }

    #[test]
    #[ignore = "requires a PN_TAG_BRIDGE with source-container status support"]
    fn unreadable_source_copies_no_tags_but_still_writes_marker() {
        let bridge = library(); let marker = marker();
        let source_bytes = b"OggS\0synthetic non-container bytes";
        for (extension, format) in [("wav", Format::Wav), ("aif", Format::Aiff), ("m4a", Format::Mp4)] {
            let src = Temp::new(extension, source_bytes);
            let dst = Temp::new("wav", &base_wav(&[]));
            assert!(matches!(bridge.inspect(src.path(), format), Err(TagError::SourceContainerUnavailable(_))));
            assert!(matches!(bridge.read_processing_marker(src.path(), format), Err(TagError::SourceContainerUnavailable(_))));
            let report = bridge.transfer(src.path(), dst.path(), format, Format::Wav, OutputChoice::Explicit, &marker).unwrap();
            assert!(report.is_success(), "{report:?}");
            let mut expected = marker.encoded_bytes().unwrap().unwrap();
            if expected.len() & 1 != 0 { expected.push(0); }
            assert_eq!(bridge.read_processing_marker(dst.path(), Format::Wav).unwrap().unwrap(), expected);
            assert_eq!(src.bytes(), source_bytes);
            let bad_dst = Temp::new("wav", b"invalid destination");
            let failure = bridge.transfer(src.path(), bad_dst.path(), format, Format::Wav, OutputChoice::Explicit, &marker);
            assert!(failure.is_err() || !failure.unwrap().is_success());
            assert_eq!(bad_dst.bytes(), b"invalid destination");
        }
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn direct_id3_filters_vendor_frames_and_saves_marker() {
        let bridge = library(); let marker = marker(); let encoded = marker.encoded_bytes().unwrap().unwrap();
        let original = mp3(&tag(&[
            text(b"TPE1", "Example Artist"), text(b"TIT2", "Example Title"), text(b"TCON", "(17)"),
            comment("iTunNORM", "exclude"), geob("Serato Autotags", b"exclude"), frame(b"XABC", b"opaque")
        ]));
        let src = Temp::new("mp3", &original); let dst = Temp::new("mp3", &mp3(&[]));
        let result = bridge.transfer(src.path(), dst.path(), Format::Mp3, Format::Mp3, OutputChoice::Explicit, &marker).unwrap();
        assert!(result.is_success(), "{result:?}"); assert_eq!(src.bytes(), original);
        let list = frames(&dst.bytes());
        assert!(has_id(&list, b"TPE1") && has_id(&list, b"TIT2") && has_id(&list, b"TCON"));
        assert!(has_id(&list, b"XABC")); assert!(!has_id(&list, b"COMM"));
        let geobs: Vec<_> = list.iter().filter(|(id, _)| id == b"GEOB").collect();
        assert_eq!(geobs.len(), 1); assert!(contains(&geobs[0].1, b"PlatinumNotes")); assert!(contains(&geobs[0].1, &encoded));
    }


    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn aiff_direct_copy_filters_frames_then_saves_geob() {
        let bridge = library(); let m = marker();
        let original = aiff(Some(&tag(&[
            text(b"TIT2", "AIFF title"), text(b"TCON", "(17)"), comment("iTunNORM", "omit"),
            geob("Serato Autotags", b"omit"), frame(b"XABC", b"opaque"),
        ])));
        let src = Temp::new("aiff", &original); let dst = Temp::new("aiff", &aiff(None));
        let report = bridge.transfer(src.path(), dst.path(), Format::Aiff, Format::Aiff, OutputChoice::Explicit, &m).unwrap();
        assert!(report.is_success(), "{report:?}"); assert_eq!(src.bytes(), original);
        let data = dst.bytes(); let f = frames(aiff_id3(&data));
        assert!(has_id(&f, b"TIT2") && has_id(&f, b"TCON") && has_id(&f, b"XABC"));
        assert!(!has_id(&f, b"COMM"));
        let geobs: Vec<_> = f.iter().filter(|(id, _)| id == b"GEOB").collect();
        assert_eq!(geobs.len(), 1); assert!(contains(&geobs[0].1, &m.encoded_bytes().unwrap().unwrap()));
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn cross_format_copies_only_generic_fields_and_writes_marker_in_same_save() {
        let bridge = library(); let marker = marker();
        let original = base_wav(&[riff_chunk(b"ID3 ", &tag(&[
            text(b"TPE1", "Artist"), text(b"TIT2", "Title"), text(b"TALB", "Album"),
            text(b"TBPM", "123"), text(b"TKEY", "Am"), text(b"TCON", "Rock"), frame(b"XABC", b"private")
        ]))]);
        let src = Temp::new("wav", &original); let dst = Temp::new("mp3", &mp3(&[]));
        let report = bridge.transfer(src.path(), dst.path(), Format::Wav, Format::Mp3, OutputChoice::Explicit, &marker).unwrap();
        assert!(report.is_success(), "{report:?}"); assert_eq!(src.bytes(), original);
        let f = frames(&dst.bytes());
        for id in [b"TPE1", b"TIT2", b"TALB", b"TBPM", b"TKEY", b"GEOB"] { assert!(has_id(&f, id), "missing {id:?}"); }
        assert!(!has_id(&f, b"TCON")); assert!(!has_id(&f, b"XABC"));
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn nil_marker_removes_existing_first_geob_and_same_file_is_rejected() {
        let bridge = library(); let mut marker = marker(); marker.target_volume = f64::NAN;
        assert!(marker.encoded_bytes().unwrap().is_none());
        let original = mp3(&tag(&[text(b"TIT2", "x"), geob("PlatinumNotes", b"old")]));
        let src = Temp::new("mp3", &original); let dst = Temp::new("mp3", &mp3(&[]));
        let result = bridge.transfer(src.path(), dst.path(), Format::Mp3, Format::Mp3, OutputChoice::Explicit, &marker).unwrap();
        assert!(result.is_success(), "{result:?}"); assert!(!has_id(&frames(&dst.bytes()), b"GEOB"));
        assert!(matches!(bridge.transfer(src.path(), src.path(), Format::Mp3, Format::Mp3, OutputChoice::Explicit, &marker), Err(TagError::SourceIsDestination)));
        assert_eq!(src.bytes(), original);
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn mp4_direct_copy_excludes_gapless_and_normalization_items() {
        let bridge = library(); let m = marker();
        let original = mp4(&[
            mp4_item(b"\xa9nam", 1, b"MP4 title"),
            freeform("iTunNORM", b"discard-normalization"),
            freeform("iTunSMPB", b"discard-gapless"),
            freeform("custom", b"keep-value"),
        ]);
        let destination = mp4(&[mp4_item(b"\xa9ART", 1, b"old-artist")]);
        let src = Temp::new("m4a", &original); let dst = Temp::new("m4a", &destination);
        let report = bridge.transfer(src.path(), dst.path(), Format::Mp4, Format::Mp4, OutputChoice::Explicit, &m).unwrap();
        assert!(report.is_success(), "{report:?}"); assert_eq!(src.bytes(), original);
        let result = dst.bytes();
        assert!(contains(&result, b"MP4 title")); assert!(contains(&result, b"keep-value"));
        assert!(!contains(&result, b"iTunNORM")); assert!(!contains(&result, b"iTunSMPB"));
        assert!(!contains(&result, b"old-artist"));
        assert!(contains(&result, &m.encoded_bytes().unwrap().unwrap()));
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn mp4_to_mp3_special_branch_preserves_picture_bytes_and_track_total() {
        let bridge = library(); let m = marker();
        let original = mp4(&[
            mp4_item(b"\xa9nam", 1, b"Title"),
            mp4_item(b"trkn", 0, &[0, 0, 0, 3, 0, 9, 0, 0]),
            mp4_item(b"tmpo", 21, &[0, 128]),
            mp4_item(b"cpil", 21, &[1]),
            mp4_item(b"covr", 13, b"\xff\xd8example-jpeg\xff\xd9"),
        ]);
        let src = Temp::new("m4a", &original); let dst = Temp::new("mp3", &mp3(&[]));
        let report = bridge.transfer(src.path(), dst.path(), Format::Mp4, Format::Mp3, OutputChoice::Explicit, &m).unwrap();
        assert!(report.is_success(), "{report:?}"); assert_eq!(src.bytes(), original);
        let f = frames(&dst.bytes());
        for id in [b"TRCK", b"TBPM", b"APIC", b"TCMP", b"GEOB"] { assert!(has_id(&f, id), "missing {id:?}"); }
        let apic = f.iter().find(|(id, _)| id == b"APIC").unwrap();
        assert!(contains(&apic.1, b"image/jpeg")); assert!(contains(&apic.1, b"example-jpeg"));
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn flac_direct_copies_xiph_without_copying_native_picture_block() {
        let bridge = library(); let m = marker();
        let original = flac(&["TITLE=FLAC source", "BPM=128", "CUSTOM=opaque"], Some(b"synthetic-picture"));
        let src = Temp::new("flac", &original); let dst = Temp::new("flac", &flac(&[], None));
        let report = bridge.transfer(src.path(), dst.path(), Format::Flac, Format::Flac, OutputChoice::Explicit, &m).unwrap();
        assert!(report.is_success(), "{report:?}"); assert_eq!(src.bytes(), original);
        let result = dst.bytes(); assert!(contains(&result, b"TITLE=FLAC source"));
        assert!(contains(&result, b"PLATINUMNOTES=")); assert!(contains(&result, &m.encoded_bytes().unwrap().unwrap()));
        assert!(!flac_block_types(&result).contains(&6)); assert!(!contains(&result, b"synthetic-picture"));
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn generic_artwork_is_decoded_and_reencoded_as_png() {
        let bridge = library(); let m = marker();
        // Minimal 1x1 GIF: generic transfer uses NSImage and ImageIO, unlike
        // the direct MP4-to-MP3 branch, which transfers raw covr bytes.
        let gif: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x21\xf9\x04\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";
        let mut image = b"\0image/gif\0\x03\0".to_vec(); image.extend_from_slice(gif);
        let original = mp3(&tag(&[text(b"TIT2", "Image"), frame(b"APIC", &image)]));
        let src = Temp::new("mp3", &original); let dst = Temp::new("m4a", &mp4(&[]));
        let report = bridge.transfer(src.path(), dst.path(), Format::Mp3, Format::Mp4, OutputChoice::Explicit, &m).unwrap();
        assert!(report.is_success(), "{report:?}"); assert_eq!(src.bytes(), original);
        let result = dst.bytes(); assert!(contains(&result, b"\x89PNG\r\n\x1a\n")); assert!(!contains(&result, gif));
    }

    #[cfg(any(target_os = "linux", feature = "portable"))]
    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the portable TagLib bridge"]
    fn portable_artwork_refusal_does_not_save_a_marker_fallback() {
        let bridge = library(); let marker = marker();
        let original = mp3(&tag(&[text(b"TIT2", "Image"), image_frame()]));
        let src = Temp::new("mp3", &original); let dst = Temp::new("m4a", &mp4(&[]));
        let before = dst.bytes();
        assert!(matches!(bridge.transfer(src.path(), dst.path(), Format::Mp3, Format::Mp4,
            OutputChoice::Explicit, &marker), Err(TagError::ArtworkTranscodeUnavailable(_))));
        assert_eq!(dst.bytes(), before);
        assert_eq!(src.bytes(), original);
    }

    // The original probe runs in a separate process: loading the candidate and
    // proprietary TagLib in one image would allow C++ symbol interposition.
    // A failed case keeps its generated directory for diagnosis.
    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x21\xf9\x04\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";
    fn image_frame() -> Vec<u8> {
        let mut image = b"\0image/gif\0\x03\0".to_vec(); image.extend_from_slice(GIF); frame(b"APIC", &image)
    }
    fn rich_id3() -> Vec<u8> {
        tag(&[
            text(b"TPE1", "Artist"), text(b"TALB", "Album"), text(b"TIT2", "Title"),
            text(b"TDRC", "2022"), text(b"TBPM", "123"), text(b"TKEY", "Am"), text(b"TCON", "(17)"),
            comment("", "plain comment"), comment("iTunNORM", "discard-normalization"),
            geob("Serato Autotags", b"discard-serato"), geob("PlatinumNotes", b"old-marker"),
            frame(b"XABC", b"unsupported-opaque"), image_frame(),
        ])
    }
    fn marker_atom(value: &[u8]) -> Vec<u8> {
        let mut mean = vec![0; 4]; mean.extend_from_slice(b"com.mixedinkey.platinumnotes");
        let mut name = vec![0; 4]; name.extend_from_slice(b"data");
        let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0]; data.extend_from_slice(value);
        let mut body = atom(b"mean", &mean); body.extend(atom(b"name", &name)); body.extend(atom(b"data", &data));
        atom(b"----", &body)
    }
    fn fixture(format: Format, source: bool, flac_id3: bool) -> Vec<u8> {
        let id3 = if source { rich_id3() } else { tag(&[
            text(b"TPE1", "old artist"), geob("PlatinumNotes", b"old-destination"), image_frame()
        ]) };
        match format {
            Format::Mp3 => mp3(&id3),
            Format::Aiff => aiff(Some(&id3)),
            Format::Wav => {
                let mut info = b"INFO".to_vec(); info.extend(riff_chunk(b"INAM", b"fixture\0"));
                base_wav(&[riff_chunk(b"ID3 ", &id3), riff_chunk(b"LIST", &info), riff_chunk(b"PLTN", b"old!")])
            }
            Format::Mp4 if source => mp4(&[
                mp4_item(b"\xa9nam", 1, b"Title"), mp4_item(b"\xa9ART", 1, b"Artist"),
                mp4_item(b"\xa9alb", 1, b"Album"), mp4_item(b"trkn", 0, &[0,0,0,3,0,9,0,0]),
                mp4_item(b"tmpo", 21, &[0,123]), mp4_item(b"covr", 13, GIF),
                freeform("iTunNORM", b"discard-normalization"), freeform("iTunSMPB", b"discard-gapless"),
                freeform("custom", b"unsupported-opaque"), marker_atom(b"old-marker"),
            ]),
            Format::Mp4 => mp4(&[mp4_item(b"\xa9ART", 1, b"old artist"), marker_atom(b"old-destination")]),
            Format::Flac => {
                let comments = if source { vec!["TITLE=Title", "ARTIST=Artist", "BPM=123", "INITIALKEY=Am",
                    "CUSTOM=unsupported-opaque", "PLATINUMNOTES=old-marker"] }
                    else { vec!["TITLE=old title", "PLATINUMNOTES=old-destination"] };
                let body = flac(&comments, Some(GIF));
                if flac_id3 { let mut out = id3; out.extend(body); out } else { body }
            }
        }
    }
    fn format_name(format: Format) -> (&'static str, &'static str) {
        match format {
            Format::Mp3 => ("mp3", "mp3"), Format::Mp4 => ("mp4", "m4a"),
            Format::Wav => ("wav", "wav"), Format::Aiff => ("aiff", "aiff"), Format::Flac => ("flac", "flac"),
        }
    }
    fn create_case() -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("local-artifacts/tag-transfer");
        fs::create_dir_all(&root).unwrap();
        loop {
            let dir = root.join(format!("reference-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&dir) {
                Ok(()) => return dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create generated reference case: {e}"),
            }
        }
    }
    fn write_fixture(path: &Path, bytes: &[u8]) {
        let mut f = OpenOptions::new().write(true).create_new(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
    }
    fn first_difference(a: &[u8], b: &[u8]) -> String {
        match a.iter().zip(b).position(|(x, y)| x != y) {
            Some(n) => format!("first difference {n}: original {:02x}, candidate {:02x}; lengths {} vs {}", a[n], b[n], a.len(), b.len()),
            None => format!("common prefix {}; lengths {} vs {}", a.len().min(b.len()), a.len(), b.len()),
        }
    }
    fn sha256(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        #[cfg(target_os = "macos")]
        let digest = {
            #[link(name = "System")]
            unsafe extern "C" {
                fn CC_SHA256(data: *const std::ffi::c_void, length: u32, out: *mut u8) -> *mut u8;
            }
            let length = u32::try_from(bytes.len()).unwrap();
            let mut digest = [0u8; 32];
            // SAFETY: the input and output buffers remain live for the call.
            assert!(!unsafe { CC_SHA256(bytes.as_ptr().cast(), length, digest.as_mut_ptr()) }.is_null());
            digest
        };
        #[cfg(target_os = "linux")]
        let digest = platinum_engine::portable::digest::sha256(bytes);
        let mut text = String::with_capacity(64);
        for byte in digest { write!(text, "{byte:02x}").unwrap(); }
        text
    }
    #[test]
    #[ignore = "requires PN_TAG_ORACLE and PN_TAG_BRIDGE; invokes pinned original on generated fixtures"]
    fn original_framework_complete_file_comparison() {
        let oracle = std::env::var_os("PN_TAG_ORACLE").expect("set PN_TAG_ORACLE to the reference-only probe");
        let bridge = library();
        let formats = [Format::Mp3, Format::Mp4, Format::Wav, Format::Aiff, Format::Flac];
        let mut cases = Vec::new();
        for source in formats { for destination in formats {
            cases.push((source, destination, OutputChoice::Explicit, false, false));
        }}
        for (i, destination) in formats.iter().copied().enumerate() {
            cases.push((destination, destination, OutputChoice::Explicit, true, false));
            cases.push((formats[(i + 1) % formats.len()], destination, OutputChoice::Explicit, true, false));
        }
        // PN match-input dispatches by source even for a different result format.
        cases.push((Format::Mp3, Format::Wav, OutputChoice::MatchInput, false, false));
        // Existing FLAC ID3 requires a second original JSON serialization.
        cases.push((Format::Mp3, Format::Flac, OutputChoice::Explicit, false, true));
        cases.push((Format::Mp3, Format::Flac, OutputChoice::Explicit, true, true));
        let mut failures = Vec::new();
        for (source_format, destination_format, choice, nil, flac_id3) in cases {
            let dir = create_case();
            let (src_name, src_ext) = format_name(source_format);
            let (dst_name, dst_ext) = format_name(destination_format);
            let src = dir.join(format!("source.{src_ext}"));
            let original = dir.join(format!("original.{dst_ext}"));
            let candidate = dir.join(format!("candidate.{dst_ext}"));
            let source = fixture(source_format, true, false);
            let destination = fixture(destination_format, false, flac_id3);
            write_fixture(&src, &source); write_fixture(&original, &destination); write_fixture(&candidate, &destination);
            let mut marker = marker(); if nil { marker.target_volume = f64::NAN; }
            let report = bridge.transfer(&src, &candidate, source_format, destination_format, choice, &marker);
            let output = std::process::Command::new(&oracle).arg(&dir).arg(src_name).arg(dst_name)
                .arg(if choice == OutputChoice::MatchInput { "match" } else { "explicit" })
                .arg(if nil { "nil" } else { "valid" })
                .env_remove("DYLD_INSERT_LIBRARIES").env_remove("DYLD_LIBRARY_PATH")
                .env_remove("DYLD_FRAMEWORK_PATH").output().expect("launch original oracle");
            let label = format!("{src_name}->{dst_name} {choice:?} {} flac_id3={flac_id3} [{}]",
                if nil { "nil" } else { "valid" }, dir.display());
            let mut errors = Vec::new();
            if !output.status.success() {
                errors.push(format!("oracle status {}; stderr {}", output.status, String::from_utf8_lossy(&output.stderr)));
            } else {
                match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    Err(e) => errors.push(format!("oracle status JSON: {e}; stdout {}", String::from_utf8_lossy(&output.stdout))),
                    Ok(status) => {
                        let copy_ok = status["copy_ok"].as_bool();
                        let marker_ok = status["marker_ok"].as_bool();
                        match &report {
                            Err(e) => errors.push(format!("candidate transfer error: {e}")),
                            Ok(candidate_report) => {
                                if copy_ok != Some(candidate_report.copy_error.is_none()) ||
                                   marker_ok != Some(candidate_report.marker_error.is_none()) {
                                    errors.push(format!("status original {status}; candidate {candidate_report:?}"));
                                }
                            }
                        }
                        let reference_snapshot = fs::read(dir.join("marker.reference")).ok();
                        let expected_snapshot = marker.encoded_bytes().unwrap();
                        if reference_snapshot != expected_snapshot {
                            errors.push(format!("out-of-band original marker snapshot differs: {}",
                                first_difference(reference_snapshot.as_deref().unwrap_or_default(), expected_snapshot.as_deref().unwrap_or_default())));
                        }
                        match bridge.read_processing_marker(&original, destination_format) {
                            Ok(raw) => {
                                if status["destination_readback_open"].as_bool() == Some(false) {
                                    errors.push("candidate opened a container rejected by original readback".into());
                                }
                                if status["result_marker_present"].as_bool() == Some(true) && raw.is_none() {
                                    errors.push("original parsed a marker but candidate reported absence".into());
                                }
                                if marker_ok == Some(true) && !nil && status["result_marker_present"].as_bool() != Some(true) {
                                    errors.push(format!("original marker readback is absent after a successful save: {status}"));
                                }
                                if marker_ok == Some(true) && !nil && status["result_marker_present"].as_bool() == Some(true) {
                                    if status["result_fields_equal"].as_bool() != Some(true) {
                                        errors.push(format!("original marker readback did not match assigned fields: {status}"));
                                    }
                                    let mut expected = reference_snapshot.clone();
                                    if destination_format == Format::Wav {
                                        if let Some(payload) = &mut expected {
                                            if payload.len() & 1 != 0 { payload.push(0); }
                                        }
                                    }
                                    if raw != expected {
                                        errors.push(format!("raw marker readback: {}",
                                            first_difference(expected.as_deref().unwrap_or_default(), raw.as_deref().unwrap_or_default())));
                                    }
                                }
                            }
                            Err(e) => {
                                if status["destination_readback_open"].as_bool() != Some(false) {
                                    errors.push(format!("candidate marker inspection failed: {e}"));
                                }
                            }
                        }
                        let reference_bytes = fs::read(&original).unwrap();
                        let candidate_bytes = fs::read(&candidate).unwrap();
                        eprintln!("{label}: source_sha256={} initial_destination_sha256={} original_sha256={} candidate_sha256={} original_status={status}",
                            sha256(&source), sha256(&destination), sha256(&reference_bytes), sha256(&candidate_bytes));
                        if reference_bytes != candidate_bytes {
                            errors.push(format!("complete file: {}", first_difference(&reference_bytes, &candidate_bytes)));
                        }
                        if copy_ok == Some(true) && marker_ok == Some(true) && !nil {
                            if source_format == Format::Mp3 && destination_format == Format::Mp4 &&
                               !contains(&reference_bytes, b"\x89PNG\r\n\x1a\n") {
                                errors.push("original generic MP3->MP4 did not emit a PNG".into());
                            }
                            if source_format == Format::Mp4 && destination_format == Format::Mp3 &&
                               !contains(&reference_bytes, GIF) {
                                errors.push("original direct MP4->MP3 did not retain raw cover bytes".into());
                            }
                            if source_format == Format::Mp3 && destination_format == Format::Mp3 &&
                               (contains(&reference_bytes, b"discard-normalization") ||
                                contains(&reference_bytes, b"discard-serato") ||
                                !contains(&reference_bytes, b"unsupported-opaque")) {
                                errors.push("original ID3 direct-copy filtering differed".into());
                            }
                            if source_format == Format::Mp4 && destination_format == Format::Mp4 &&
                               (contains(&reference_bytes, b"iTunNORM") || contains(&reference_bytes, b"iTunSMPB") ||
                                !contains(&reference_bytes, b"unsupported-opaque")) {
                                errors.push("original MP4 direct-copy filtering differed".into());
                            }
                            if source_format == Format::Mp3 && destination_format == Format::Mp4 &&
                               (contains(&reference_bytes, b"unsupported-opaque") || contains(&reference_bytes, GIF)) {
                                errors.push("original generic copy retained an unsupported ID3 field or raw GIF".into());
                            }
                        }
                    }
                }
            }
            if fs::read(&src).unwrap() != source { errors.push("original source fixture was modified".into()); }
            if errors.is_empty() {
                fs::remove_dir_all(&dir).unwrap();
                eprintln!("PASS {label}");
            } else {
                failures.push(format!("{label}: {}", errors.join("; ")));
            }
        }
        assert!(failures.is_empty(), "original/candidate differences:\n{}", failures.join("\n"));
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn native_empty_and_nil_payload_are_distinct_and_null_safe() {
        use std::ffi::{c_char, c_int, c_void, CString};
        use std::os::unix::ffi::OsStrExt;
        #[repr(C)]
        struct Payload { bytes: *const u8, length: usize, present: c_int }
        struct Context { present: c_int, calls: usize }
        unsafe extern "C" fn callback(context: *mut c_void, out: *mut Payload) -> c_int {
            let ctx = unsafe { &mut *context.cast::<Context>() }; ctx.calls += 1;
            unsafe { out.write(Payload { bytes: std::ptr::null(), length: 0, present: ctx.present }); }
            0
        }
        type Save = unsafe extern "C" fn(c_int, *const c_char, unsafe extern "C" fn(*mut c_void, *mut Payload) -> c_int,
                                         Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int>,
                                         *mut c_void, *mut c_char, usize) -> c_int;
        let library_path = std::env::var_os("PN_TAG_BRIDGE").expect("set PN_TAG_BRIDGE");
        let lib = unsafe { libloading::Library::new(library_path).unwrap() };
        let save: libloading::Symbol<'_, Save> = unsafe { lib.get(b"pn_tag_save_marker\0").unwrap() };
        for (format, ext, data, expected_calls) in [
            (Format::Mp3, "mp3", fixture(Format::Mp3, false, false), 1),
            (Format::Mp4, "m4a", fixture(Format::Mp4, false, false), 1),
            (Format::Aiff, "aiff", fixture(Format::Aiff, false, false), 1),
            (Format::Flac, "flac", fixture(Format::Flac, false, false), 1),
            (Format::Flac, "flac", fixture(Format::Flac, false, true), 2),
        ] {
            let mut outputs = Vec::new();
            for present in [0, 1] {
                let target = Temp::new(ext, &data);
                let path = CString::new(target.path().as_os_str().as_bytes()).unwrap();
                let mut context = Context { present, calls: 0 };
                let mut error = [0 as c_char; 1024];
                let status = unsafe { save(format as c_int, path.as_ptr(), callback, None,
                    (&mut context as *mut Context).cast(), error.as_mut_ptr(), error.len()) };
                let message = unsafe { std::ffi::CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
                assert_eq!(status, 0, "{format:?} present={present}: {message}");
                assert_eq!(context.calls, expected_calls, "{format:?} present={present}");
                outputs.push(target.bytes());
            }
            if format == Format::Mp3 {
                assert!(!has_id(&frames(&outputs[0]), b"GEOB"));
                assert!(has_id(&frames(&outputs[1]), b"GEOB"));
            }
        }
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn processing_marker_reader_preserves_presence_and_container_semantics() {
        let bridge = library();
        for (format, ext, bytes) in [
            (Format::Mp3, "mp3", mp3(&tag(&[]))),
            (Format::Mp4, "m4a", mp4(&[])),
            (Format::Aiff, "aiff", aiff(None)),
            (Format::Wav, "wav", base_wav(&[])),
            (Format::Flac, "flac", flac(&[], None)),
        ] {
            let file = Temp::new(ext, &bytes);
            assert_eq!(bridge.read_processing_marker(file.path(), format).unwrap(), None, "{format:?}");
            assert_eq!(file.bytes(), bytes, "reader modified {format:?}");
        }
        for (format, ext, bytes, expected) in [
            (Format::Mp3, "mp3", mp3(&tag(&[geob("PlatinumNotes", b""), geob("PlatinumNotes", b"second")])), Vec::new()),
            (Format::Aiff, "aiff", aiff(Some(&tag(&[geob("PlatinumNotes", b"malformed\0payload")]))), b"malformed\0payload".to_vec()),
            (Format::Mp4, "m4a", mp4(&[marker_atom(b"")]), Vec::new()),
            (Format::Wav, "wav", base_wav(&[riff_chunk(b"PLTN", b"")]), Vec::new()),
            (Format::Wav, "wav", base_wav(&[riff_chunk(b"PLTN", b"odd")]), b"odd".to_vec()),
            (Format::Wav, "wav", base_wav(&[riff_chunk(b"PLTN", b"odd\0")]), b"odd\0".to_vec()),
            (Format::Wav, "wav", base_wav(&[riff_chunk(b"PLTN", b"first"), riff_chunk(b"PLTN", b"last-value"), riff_chunk(b"PLTN", b"x")]), b"last-value".to_vec()),
            (Format::Flac, "flac", flac(&["PLATINUMNOTES="], None), Vec::new()),
            (Format::Flac, "flac", flac(&["PLATINUMNOTES=bad", "PLATINUMNOTES=payload"], None), b"bad payload".to_vec()),
        ] {
            let file = Temp::new(ext, &bytes);
            assert_eq!(bridge.read_processing_marker(file.path(), format).unwrap(), Some(expected), "{format:?}");
            assert_eq!(file.bytes(), bytes, "reader modified {format:?}");
        }
        let mut with_id3 = tag(&[geob("PlatinumNotes", b"fallback")]);
        with_id3.extend(flac(&[], None));
        let file = Temp::new("flac", &with_id3);
        assert_eq!(bridge.read_processing_marker(file.path(), Format::Flac).unwrap(), Some(b"fallback".to_vec()));
        let mut empty_xiph = tag(&[geob("PlatinumNotes", b"fallback")]);
        empty_xiph.extend(flac(&["PLATINUMNOTES="], None));
        let file = Temp::new("flac", &empty_xiph);
        assert_eq!(bridge.read_processing_marker(file.path(), Format::Flac).unwrap(), Some(Vec::new()));
        let mut truncated = base_wav(&[riff_chunk(b"PLTN", b"marker")]); truncated.pop();
        let file = Temp::new("wav", &truncated);
        assert!(bridge.read_processing_marker(file.path(), Format::Wav).is_err());
        let file = Temp::new("mp3", b"not a valid MPEG file");
        assert!(bridge.read_processing_marker(file.path(), Format::Mp3).is_err());
        let mut malformed_atom = marker_atom(b"opaque");
        // Locate the data child, not the freeform name's "data" value.
        let child = malformed_atom.windows(4).enumerate().find_map(|(i, w)| {
            (w == b"data" && i >= 4 && u32::from_be_bytes(malformed_atom[i - 4..i].try_into().ok()?) == 22).then_some(i)
        }).unwrap();
        malformed_atom[child + 7] = 0;
        let file = Temp::new("m4a", &mp4(&[malformed_atom]));
        assert!(bridge.read_processing_marker(file.path(), Format::Mp4).is_err());
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn inspect_returns_generic_fields_properties_and_unmodified_id3_artwork() {
        let bridge = library();
        for format in [Format::Mp3, Format::Wav, Format::Aiff] {
            let (_, ext) = format_name(format);
            let input = fixture(format, true, false);
            let file = Temp::new(ext, &input);
            let actual = bridge.inspect(file.path(), format).unwrap();
            assert_eq!(actual.artist.as_deref(), Some("Artist"));
            assert_eq!(actual.album.as_deref(), Some("Album"));
            assert_eq!(actual.year, 2022);
            assert_eq!(actual.title.as_deref(), Some("Title"));
            assert_eq!(actual.comment.as_deref(), Some("plain comment"));
            assert_eq!(actual.tempo, 123);
            assert_eq!(actual.initial_key.as_deref(), Some("Am"));
            assert_eq!(actual.properties.get("ARTIST"), Some(&vec!["Artist".to_owned()]));
            assert_eq!(actual.properties.get("TITLE"), Some(&vec!["Title".to_owned()]));
            assert!(actual.unsupported_data.iter().any(|s| s == "APIC"), "{:?}", actual.unsupported_data);
            assert!(actual.unsupported_data.iter().any(|s| s == "UNKNOWN/XABC"), "{:?}", actual.unsupported_data);
            let ids: Vec<_> = actual.id3_frames.as_ref().unwrap().iter().map(|f| f.id.as_str()).collect();
            assert_eq!(ids, ["TPE1", "TALB", "TIT2", "TDRC", "TBPM", "TKEY", "TCON",
                "COMM", "COMM", "GEOB", "GEOB", "XABC", "APIC"]);
            assert_eq!(actual.id3_frames.as_ref().unwrap().last().unwrap().byte_count, image_frame().len() - 10);
            assert_eq!(actual.artwork.len(), 1);
            let art = &actual.artwork[0];
            assert_eq!(art.container, "id3v2");
            assert_eq!(art.mime_type.as_deref(), Some("image/gif"));
            assert_eq!(art.picture_type, Some(3));
            assert_eq!(art.description.as_deref(), Some(""));
            assert_eq!(art.format_code, None);
            assert_eq!(art.byte_count, GIF.len());
            assert_eq!(art.sha256, sha256(GIF));
            assert_eq!(file.bytes(), input, "inspection modified {format:?}");
            let json = serde_json::to_value(&actual).unwrap();
            let roundtrip: platinum_engine::tags::TagInspection = serde_json::from_value(json).unwrap();
            assert_eq!(roundtrip, actual);
        }
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn inspect_flac_and_mp4_artwork_keeps_declared_values_and_original_bytes() {
        let bridge = library();
        let input = fixture(Format::Flac, true, true);
        let file = Temp::new("flac", &input);
        let actual = bridge.inspect(file.path(), Format::Flac).unwrap();
        assert_eq!(actual.artist.as_deref(), Some("Artist"));
        assert_eq!(actual.title.as_deref(), Some("Title"));
        assert_eq!(actual.tempo, 123);
        assert_eq!(actual.initial_key.as_deref(), Some("Am"));
        assert_eq!(actual.properties.get("CUSTOM"), Some(&vec!["unsupported-opaque".to_owned()]));
        assert!(actual.id3_frames.as_ref().unwrap().iter().any(|f| f.id == "APIC"));
        assert_eq!(actual.artwork.len(), 2);
        assert_eq!(actual.artwork[0].container, "id3v2");
        assert_eq!(actual.artwork[1].container, "flac_picture");
        assert_eq!(actual.artwork[1].mime_type.as_deref(), Some("image/png"));
        assert_eq!(actual.artwork[1].picture_type, Some(3));
        for art in &actual.artwork {
            assert_eq!(art.byte_count, GIF.len());
            assert_eq!(art.sha256, sha256(GIF));
        }
        assert_eq!(file.bytes(), input);

        let legacy = "COVERART=R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==";
        let mut block = 3u32.to_be_bytes().to_vec();
        block.extend_from_slice(&9u32.to_be_bytes()); block.extend_from_slice(b"image/gif");
        block.extend_from_slice(&4u32.to_be_bytes()); block.extend_from_slice(b"icon");
        block.extend_from_slice(&[0; 16]); block.extend_from_slice(&(GIF.len() as u32).to_be_bytes());
        block.extend_from_slice(GIF);
        let encoded: Vec<_> = metadata::encode_marker_base64(&block).unwrap().into_iter().filter(|b| *b != b'\n').collect();
        let modern = format!("METADATA_BLOCK_PICTURE={}", String::from_utf8(encoded).unwrap());
        let input = flac(&[legacy, &modern], None);
        let file = Temp::new("flac", &input);
        let actual = bridge.inspect(file.path(), Format::Flac).unwrap();
        assert_eq!(actual.artwork.len(), 2);
        assert_eq!(actual.artwork[0].container, "flac_xiph_coverart");
        assert_eq!(actual.artwork[0].mime_type, None);
        assert_eq!(actual.artwork[0].picture_type, None);
        assert_eq!(actual.artwork[0].description, None);
        assert_eq!(actual.artwork[1].container, "flac_xiph_picture");
        assert_eq!(actual.artwork[1].mime_type.as_deref(), Some("image/gif"));
        assert_eq!(actual.artwork[1].picture_type, Some(3));
        assert_eq!(actual.artwork[1].description.as_deref(), Some("icon"));
        for art in &actual.artwork {
            assert_eq!(art.byte_count, GIF.len());
            assert_eq!(art.sha256, sha256(GIF));
        }
        assert_eq!(file.bytes(), input);

        // 99 is an unknown covr data format that TagLib's parser drops. The
        // inspector must still retain its declared code and original bytes.
        let input = mp4(&[mp4_item(b"\xa9nam", 1, b"Title"),
            mp4_item(b"covr", 99, GIF), mp4_item(b"covr", 14, b"not-a-png")]);
        let file = Temp::new("m4a", &input);
        let actual = bridge.inspect(file.path(), Format::Mp4).unwrap();
        assert_eq!(actual.title.as_deref(), Some("Title"));
        assert_eq!(actual.id3_frames, None);
        assert_eq!(actual.artwork.len(), 2);
        assert_eq!(actual.artwork[0].container, "mp4_covr");
        assert_eq!(actual.artwork[0].format_code, Some(99));
        assert_eq!(actual.artwork[0].mime_type, None);
        assert_eq!(actual.artwork[0].picture_type, None);
        assert_eq!(actual.artwork[0].description, None);
        assert_eq!(actual.artwork[0].byte_count, GIF.len());
        assert_eq!(actual.artwork[0].sha256, sha256(GIF));
        assert_eq!(actual.artwork[1].format_code, Some(14));
        assert_eq!(actual.artwork[1].mime_type.as_deref(), Some("image/png"));
        assert_eq!(actual.artwork[1].sha256, sha256(b"not-a-png"));
        assert_eq!(file.bytes(), input);
    }

    #[test]
    #[ignore = "requires PN_TAG_BRIDGE built from the pinned public TagLib source"]
    fn inspect_distinguishes_absent_and_empty_id3_and_rejects_invalid_input() {
        let bridge = library();
        for (format, ext, data) in [
            (Format::Mp3, "mp3", mp3(&[])), (Format::Aiff, "aiff", aiff(None)),
            (Format::Wav, "wav", base_wav(&[])), (Format::Flac, "flac", flac(&[], None)),
            (Format::Mp4, "m4a", mp4(&[])),
        ] {
            let file = Temp::new(ext, &data);
            let actual = bridge.inspect(file.path(), format).unwrap();
            assert_eq!(actual.id3_frames, None, "{format:?}");
            assert!(actual.artwork.is_empty(), "{format:?}");
            assert_eq!(file.bytes(), data);
        }
        for (format, ext, data) in [
            (Format::Mp3, "mp3", mp3(&tag(&[]))),
            (Format::Aiff, "aiff", aiff(Some(&tag(&[])))),
            (Format::Wav, "wav", base_wav(&[riff_chunk(b"ID3 ", &tag(&[]))])),
        ] {
            let file = Temp::new(ext, &data);
            assert_eq!(bridge.inspect(file.path(), format).unwrap().id3_frames, Some(Vec::new()), "{format:?}");
            assert_eq!(file.bytes(), data);
        }
        let invalid = Temp::new("mp3", b"not an MPEG stream");
        assert!(bridge.inspect(invalid.path(), Format::Mp3).is_err());
        let mut malformed = flac(&[], Some(GIF));
        let picture = malformed.windows(9).position(|w| w == b"image/png").unwrap();
        malformed[picture - 4..picture].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        let file = Temp::new("flac", &malformed);
        assert!(bridge.inspect(file.path(), Format::Flac).is_err());
        let file = Temp::new("flac", &flac(&["COVERART=%%%"], None));
        assert!(bridge.inspect(file.path(), Format::Flac).is_err());
    }
}
