//! Container rules recorded on macOS 27.2 with generated variants of the stereo fixture: every test says what AudioToolbox answered for the same bytes
//! (`ExtAudioFileOpenURL` and `kExtAudioFileProperty_FileLengthFrames`), and the port must answer the same.  The variants are rebuilt here from
//! `tests/fixtures/aac-reader/tiny-44100-2.m4a` (the layout is asserted), so the cases need no stored file.

use super::*;
use crate::portable::aac::decoder::file::error::{STATUS_FORMAT_NOT_SUPPORTED, STATUS_UNSPECIFIED};
use crate::portable::aac::decoder::file::mp4::UNPLACED_OFFSET;
use crate::portable::aac::decoder::testutil::*;
use std::time::{Duration, Instant};

fn be(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn open(file: Vec<u8>) -> Result<AacFile, OpenError> {
    AacFile::parse_with_hint(file, None)
}

/// The layout of `STEREO_M4A` the byte surgery below relies on: `ftyp` 0..28, `moov` 28..890 (`mvhd` 36..144, `trak` 144..640, `udta` 640..890 whose
/// iTunSMPB item `----` is 702..890 and its `data` 758..890), a `free` 890..4088, then `mdat`.
fn check_layout() {
    for (at, kind) in [(4usize, b"ftyp"), (32, b"moov"), (40, b"mvhd"), (148, b"trak"), (644, b"udta"), (706, b"----"), (762, b"data"), (894, b"free"), (4092, b"mdat")] {
        assert_eq!(&STEREO_M4A[at..at + 4], kind, "atom type at {at}");
    }
}

/// The fixture with a `free` atom of `pad` bytes at the end of the movie atom; the top-level `free` gives up the same bytes, so `mdat` stays in place.
fn with_padded_moov(pad: usize) -> Vec<u8> {
    check_layout();
    let mut f = STEREO_M4A.to_vec();
    let free = u32::from_be_bytes(f[890..894].try_into().unwrap()) as usize;
    f[890..894].copy_from_slice(&be((free - pad) as u32));
    let mut padding = be(pad as u32).to_vec();
    padding.extend_from_slice(b"free");
    padding.resize(pad, 0);
    f.splice(890..890, padding);
    f.drain(890 + pad + 8..890 + pad + 8 + pad);
    f[28..32].copy_from_slice(&be(862 + pad as u32));
    assert_eq!(f.len(), STEREO_M4A.len());
    f
}

/// A movie atom with a first track that has no handler of its own, whose `mdia` declares `mdia_size` bytes, and a stray `hdlr` (sound) behind it; the
/// real track follows.
fn with_decoy_track(mdia_size: u32) -> Vec<u8> {
    check_layout();
    let (mvhd, tkhd, trak, hdlr, udta) = (&STEREO_M4A[36..144], &STEREO_M4A[152..244], &STEREO_M4A[144..640], &STEREO_M4A[284..318], &STEREO_M4A[640..890]);
    assert_eq!(&hdlr[4..8], b"hdlr");
    let mut decoy = tkhd.to_vec();
    decoy.extend_from_slice(&be(mdia_size));
    decoy.extend_from_slice(b"mdia");
    let moov = atom(b"moov", &[mvhd, &atom(b"trak", &decoy), hdlr, trak, udta].concat());
    let mut f = STEREO_M4A[..28].to_vec();
    f.extend(moov);
    let pad = 4088 - f.len();
    f.extend(atom(b"free", &vec![0; pad - 8]));
    f.extend_from_slice(&STEREO_M4A[4088..]);
    f
}

/// What AudioToolbox answered: the track whose `mdia` runs past its `trak` (to the end of the file, over the stray `hdlr` behind it, or not at all) is
/// skipped, the real track is the sound track.  A scan of the children of an atom never leaves the atom.
#[test]
fn a_track_whose_media_atom_runs_past_it_is_not_the_sound_track() {
    for mdia_size in [0x00ff_ffff, 8 + 34, 8] {
        let f = open(with_decoy_track(mdia_size)).unwrap();
        assert_eq!((f.packet_count(), f.file_length_frames()), (11, 8820), "mdia of {mdia_size} bytes");
    }
}

/// What AudioToolbox answered: an iTunSMPB `data` atom that runs past its parent (the item, here; by 4 bytes as by 60) is cut off there and the tag is not
/// used, even when 60 text characters are left; the same bytes without the damage give the tag's frame count.
#[test]
fn an_itunsmpb_atom_that_runs_past_its_parent_is_not_used() {
    assert_eq!(open(with_padded_moov(100)).unwrap().file_length_frames(), 8820, "the undamaged tag is used");
    // (size of the `----` item, size of its `data` atom): the file has 188 and 132
    for (item, data) in [(188, 136), (184, 132), (188, 172), (128, 132), (102, 132), (132, 132)] {
        let mut f = with_padded_moov(100);
        f[702..706].copy_from_slice(&be(item));
        f[758..762].copy_from_slice(&be(data));
        assert_eq!(open(f).unwrap().file_length_frames(), 9152, "item {item}, data {data}: the default priming, 11 x 1024 - 2112");
    }
}

fn with_brands(major: &[u8; 4], minor: u32, compat: [&[u8; 4]; 3]) -> Vec<u8> {
    let mut f = STEREO_M4A.to_vec();
    f[8..12].copy_from_slice(major);
    f[12..16].copy_from_slice(&be(minor));
    for (i, brand) in compat.iter().enumerate() {
        f[16 + 4 * i..20 + 4 * i].copy_from_slice(*brand);
    }
    f
}

/// What AudioToolbox answered for the same bytes under other `ftyp` brands: the iTunSMPB tag counts for the `m4af` / `m4bf` types only, that is when a
/// compatible brand is `M4A `, `M4V ` or `M4B ` (or the major brand is one of them) and no compatible brand is `3gp4`..`3gp9` or `3g2a`.  Brands are
/// compared byte for byte; the minor version does not matter.
#[test]
fn the_itunsmpb_tag_is_read_for_the_m4a_and_m4b_brands_only() {
    let read = 8820; // the tag: 8820 valid frames
    let ignored = 9152; // the default priming: 11 x 1024 - 2112
    let cases: &[(&[u8; 4], [&[u8; 4]; 3], u64)] = &[
        (b"M4A ", [b"M4A ", b"mp42", b"isom"], read),
        (b"M4B ", [b"M4B ", b"mp42", b"isom"], read),
        (b"M4A ", [b"mp42", b"isom", b"mp42"], read),
        (b"M4V ", [b"mp42", b"mp42", b"mp42"], read),
        (b"mp42", [b"mp42", b"M4V ", b"isom"], read),
        (b"mp42", [b"isom", b"M4A ", b"isom"], read),
        (b"mp42", [b"isom", b"M4B ", b"M4A "], read),
        (b"3gp5", [b"M4A ", b"isom", b"isom"], read),
        (b"3g2a", [b"M4A ", b"isom", b"isom"], read),
        (b"mp42", [b"3gp3", b"M4A ", b"isom"], read),
        (b"mp42", [b"3g2b", b"M4A ", b"isom"], read),
        (b"mp42", [b"3gpp", b"M4A ", b"isom"], read),
        (b"mp42", [b"mp42", b"isom", b"mp42"], ignored),
        (b"isom", [b"isom", b"mp42", b"isom"], ignored),
        (b"3gp5", [b"3gp5", b"3gp4", b"isom"], ignored),
        (b"3g2a", [b"3g2a", b"isom", b"isom"], ignored),
        (b"M4P ", [b"M4P ", b"mp42", b"isom"], ignored),
        (b"dash", [b"iso6", b"mp41", b"iso6"], ignored),
        (b"iso5", [b"iso5", b"iso5", b"iso5"], ignored),
        (b"f4a ", [b"f4a ", b"mp42", b"isom"], ignored),
        (b"xxxx", [b"yyyy", b"zzzz", b"yyyy"], ignored),
        (b"m4a ", [b"mp42", b"isom", b"mp42"], ignored),
        (b"M4A\0", [b"mp42", b"isom", b"mp42"], ignored),
        (b"M4A ", [b"3gp5", b"isom", b"isom"], ignored),
        (b"M4A ", [b"3g2a", b"isom", b"isom"], ignored),
        (b"M4B ", [b"3g2a", b"isom", b"isom"], ignored),
        (b"mp42", [b"3gp4", b"M4A ", b"isom"], ignored),
        (b"mp42", [b"M4A ", b"3gp6", b"isom"], ignored),
        (b"mp42", [b"3gp9", b"M4A ", b"isom"], ignored),
        (b"mp42", [b"M4B ", b"3gp4", b"isom"], ignored),
        (b"mp42", [b"3gp4", b"3g2a", b"M4A "], ignored),
    ];
    for (major, compat, expected) in cases {
        for minor in [0, 0xffff_ffff] {
            let f = open(with_brands(major, minor, *compat)).unwrap();
            assert_eq!(f.file_length_frames(), *expected, "{} {} {}", String::from_utf8_lossy(*major), minor, compat.map(|b| String::from_utf8_lossy(b).into_owned()).join(","));
        }
    }
}

/// What AudioToolbox answered for an `ftyp` that declares `declared` bytes while the file goes on: `'dta?'` (a recognised MP4 file, the movie atom hides
/// inside the box) up to 99 bytes, `'typ?'` from 100, and for a declared size beyond the file the box is what the file holds of it.
#[test]
fn an_ftyp_box_of_100_bytes_or_more_is_no_mp4_file() {
    for declared in [20u32, 32, 60, 96, 97, 98, 99] {
        let mut f = STEREO_M4A.to_vec();
        f[0..4].copy_from_slice(&be(declared));
        assert_eq!(open(f).err(), Some(OpenError::InvalidFile), "declared {declared}");
    }
    for declared in [100u32, 104, 200, 0x1_0000, 0x7fff_ffff, 0x8000_0000, 0xffff_fffe] {
        let mut f = STEREO_M4A.to_vec();
        f[0..4].copy_from_slice(&be(declared));
        assert_eq!(open(f).err(), Some(OpenError::UnsupportedFileType), "declared {declared}");
    }
    for (held, expected) in [(19usize, OpenError::UnsupportedFileType), (20, OpenError::InvalidFile), (60, OpenError::InvalidFile), (99, OpenError::InvalidFile), (100, OpenError::UnsupportedFileType), (101, OpenError::UnsupportedFileType)] {
        let mut f = STEREO_M4A[..held].to_vec();
        f[0..4].copy_from_slice(&be(0x7fff_ffff));
        assert_eq!(open(f).err(), Some(expected), "a file of {held} bytes whose ftyp declares 2^31 - 1");
    }
}

// ---- movie fragments --------------------------------------------------------------------------------------------------------------

/// A fragmented MP4: the fixture's track without samples, `mvex` with a `trex` of `default_size`, then the fragments.
fn fragmented(default_size: u32, fragments: &[Vec<u8>]) -> Vec<u8> {
    let plain = mp4_with(&[], &Mp4Parts { sizes: Vec::new(), constant: None, asc: vec![0x12, 0x10], entry: *b"mp4a" });
    let moov_size = u32::from_be_bytes(plain[28..32].try_into().unwrap()) as usize;
    // the track has no `tkhd` (track id 0), and so do the headers of the fragments
    let mvex = atom(b"mvex", &full(b"trex", 0, &[be(0), be(1), be(1024), be(default_size), be(0)].concat()));
    let mut f = plain[..28].to_vec();
    f.extend_from_slice(&be((moov_size + mvex.len()) as u32));
    f.extend_from_slice(&plain[32..28 + moov_size]);
    f.extend(mvex);
    for fragment in fragments {
        f.extend_from_slice(fragment);
    }
    f
}

/// `moof` + `mdat` of one fragment: a `trun` with `flags` and `count` samples whose entries are `sizes` (when the flags announce them), data offset
/// pointing at the `mdat` payload.  `tfhd` is the header of the track fragment.
fn fragment(trun_flags: u32, count: u32, sizes: &[u32], payload: &[u8], tfhd: &[u8]) -> Vec<u8> {
    let make = |data_offset: u32| {
        let mut trun = be(count).to_vec();
        if trun_flags & 1 != 0 {
            trun.extend_from_slice(&be(data_offset));
        }
        for size in sizes {
            if trun_flags & 0x100 != 0 {
                trun.extend_from_slice(&be(1024));
            }
            if trun_flags & 0x200 != 0 {
                trun.extend_from_slice(&be(*size));
            }
        }
        atom(b"moof", &[full(b"mfhd", 0, &be(1)), atom(b"traf", &[tfhd.to_vec(), full(b"trun", trun_flags, &trun)].concat())].concat())
    };
    let moof_size = make(0).len() as u32;
    let mut out = make(moof_size + 8);
    out.extend(atom(b"mdat", payload));
    out
}

fn tfhd_plain() -> Vec<u8> {
    full(b"tfhd", 0x02_0000, &be(0))
}

/// What AudioToolbox answered: all the samples of a fragment are read; a `trun` whose count its atom cannot hold is skipped as if it were not there and
/// the other fragments are read; a `trun` without sizes (no size field and a default size of 0) fails the open (`'wht?'`).
#[test]
fn trun_rules_follow_what_audiotoolbox_did() {
    let aus = access_units(STEREO_M4A);
    let sizes: Vec<u32> = aus.iter().map(|a| a.len() as u32).collect();
    let (first, second) = (&aus[..5], &aus[5..]);
    let join = |aus: &[Vec<u8>]| aus.concat();

    let whole = open(fragmented(0, &[fragment(0x301, 11, &sizes, &join(&aus), &tfhd_plain())])).unwrap();
    assert_eq!(whole.packet_count(), 11);
    assert!((0..11).all(|i| whole.packet(i).unwrap() == aus[i].as_slice()), "the packets are the fixture's access units");

    // a count one above what the atom holds: skipped (no packet), whatever the count is
    for count in [12u32, 1000, 1_000_000] {
        let f = open(fragmented(0, &[fragment(0x301, count, &sizes, &join(&aus), &tfhd_plain())])).unwrap();
        assert_eq!(f.packet_count(), 0, "count {count}");
    }
    // the same fragment inflated in front of a good one: only the good one is read
    let inflated = fragment(0x301, 6, &sizes[..5], &join(first), &tfhd_plain());
    let good = fragment(0x301, 6, &sizes[5..], &join(second), &tfhd_plain());
    let f = open(fragmented(0, &[inflated, good.clone()])).unwrap();
    assert_eq!(f.packet_count(), 6);
    assert!((0..6).all(|i| f.packet(i).unwrap() == second[i].as_slice()));
    // a count the atom holds exactly, and zero samples
    assert_eq!(open(fragmented(0, &[fragment(0x301, 5, &sizes[..5], &join(first), &tfhd_plain()), good.clone()])).unwrap().packet_count(), 11);
    assert_eq!(open(fragmented(0, &[fragment(0x301, 0, &[], &[], &tfhd_plain()), good])).unwrap().packet_count(), 6);

    // no size anywhere: 'wht?', also for a count that cannot be allocated
    for count in [3u32, 1_000_000, u32::MAX] {
        assert_eq!(open(fragmented(0, &[fragment(0x001, count, &[], &[0; 64], &tfhd_plain())])).err(), Some(OpenError::Unspecified(Unspecified::BadStructure)), "count {count}");
    }
    // a default size makes a table of any count: bounded by the file, which the audio must fit in
    let f = fragmented(100, &[fragment(0x001, 3, &[], &[0; 400], &tfhd_plain())]);
    assert_eq!(open(f).unwrap().packet_count(), 3);
    let f = fragmented(1, &[fragment(0x001, 16_000_000, &[], &[0; 64], &tfhd_plain())]);
    assert_eq!(open(f).err(), Some(OpenError::Unspecified(Unspecified::FileShorterThanAudio)), "a few bytes make no table of millions of samples");
}

/// A `tfhd` base data offset near 2^63 plus a data offset must not overflow (a panic in debug builds): the packets lie beyond the file.
#[test]
fn a_huge_base_data_offset_wraps_instead_of_panicking() {
    let aus = access_units(STEREO_M4A);
    let sizes: Vec<u32> = aus.iter().map(|a| a.len() as u32).collect();
    for base in [0x7fff_ffff_ffff_fff0u64, 0x8000_0000_0000_0000, 0xffff_ffff_ffff_0000] {
        let tfhd = full(b"tfhd", 0x00_0001, &[be(0).to_vec(), base.to_be_bytes().to_vec()].concat());
        let f = open(fragmented(0, &[fragment(0x301, 11, &sizes, &aus.concat(), &tfhd)])).unwrap();
        assert_eq!(f.packet_count(), 11);
        assert_eq!(f.packet(0).err(), Some(ReadError::PacketBeyondEof), "base {base:#x}");
    }
}

// ---- the other containers ---------------------------------------------------------------------------------------------------------

/// The 19-byte LOAS file whose StreamMuxConfig (audioMuxVersion 1) claims an ASC of 2^32 - 1 bits: refused `'fmt?'` without making a 512 MiB buffer.
#[test]
fn a_loas_mux_config_that_claims_more_bits_than_the_frame_has_is_refused() {
    let forged = [0x56, 0xe0, 0x10, 0x40, 0x00, 0x00, 0x1f, 0xff, 0xff, 0xff, 0xff, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    for hint in [None, Some("loas"), Some("latm")] {
        assert_eq!(AacFile::parse_with_hint(forged.to_vec(), hint).err(), Some(OpenError::UnsupportedDataFormat), "{hint:?}");
    }
}

/// A FIL element with the count 15 and an escape count of 0 (a count of 14) in the first access unit of an ADTS stream: no arithmetic underflow.
#[test]
fn an_adts_fil_element_with_a_zero_escape_count_is_read_not_panicked_on() {
    let stream = [0xff, 0xf1, 0x50, 0x80, 0x01, 0x3f, 0xfc, 0xde, 0x00];
    for hint in [None, Some("aac")] {
        assert_eq!(AacFile::parse_with_hint(stream.to_vec(), hint).err(), Some(OpenError::UnsupportedDataFormat), "{hint:?}");
    }
}

/// The parser and the sniff of `Reader::open` skip the same number of leading tag blocks.
#[test]
fn leading_tag_blocks_are_skipped_up_to_the_cap_and_no_further() {
    let tag = b"ID3\x04\x00\x00\x00\x00\x00\x00"; // an empty ID3v2 tag: 10 bytes
    let data = tag.repeat(MAX_TAG_BLOCKS + 5);
    assert_eq!(adts::skip_tags(&data), 10 * MAX_TAG_BLOCKS);
    assert_eq!(adts::skip_tags(&tag.repeat(3)), 30);
}

/// `kAudioFilePropertyPacketSizeUpperBound` is known when the file is opened: asking for it must not scan the packet table (every read asks).
#[test]
fn the_packet_size_bound_is_no_scan_of_the_packet_table() {
    const N: usize = 200_000;
    let au = [0u8; 1];
    let aus = vec![&au[..]; N];
    let mut parts = Mp4Parts { sizes: vec![1; N], constant: None, asc: vec![0x12, 0x10], entry: *b"mp4a" };
    parts.sizes[N / 2] = 40;
    let big = vec![0u8; 40];
    let mut refs = aus;
    refs[N / 2] = &big;
    let f = open(mp4_with(&refs, &parts)).unwrap();
    assert_eq!(f.packet_count(), N);
    let started = Instant::now();
    let mut sum = 0u64;
    for _ in 0..N {
        sum += u64::from(f.packet_size_upper_bound());
    }
    assert_eq!(sum, 40 * N as u64);
    assert!(started.elapsed() < Duration::from_secs(2), "{N} bounds of {N} packets took {:?}", started.elapsed());
}

/// `aac ` over a cookie that signals SBR (or PS) is the HE-AAC refusal, whatever the `desc` rate and frames per packet say (AudioToolbox decodes all of
/// them, as HE-AAC).
#[test]
fn a_caf_desc_aac_over_an_he_cookie_is_the_he_refusal() {
    // ASC: AAC-LC core at 22.05 kHz, stereo, with explicit SBR signalling to 44.1 kHz
    let asc = [0x13, 0x90, 0x56, 0xe5, 0xa0];
    let cookie = esds(&asc)[12..].to_vec();
    for (rate, fpp) in [(22_050.0f64, 1024u32), (44_100.0, 1024), (44_100.0, 2048), (48_000.0, 1024)] {
        let caf = caf_edited(|chunks| {
            for (kind, body) in chunks.iter_mut() {
                match &kind[..] {
                    b"kuki" => *body = cookie.clone(),
                    b"desc" => {
                        body[0..8].copy_from_slice(&rate.to_be_bytes());
                        body[20..24].copy_from_slice(&fpp.to_be_bytes());
                    }
                    _ => {}
                }
            }
        });
        let f = open(caf).unwrap();
        assert_eq!(f.decoder_config().err(), Some(ClientError::NotAacLc), "{rate} Hz, {fpp} frames per packet");
        assert_eq!(f.format_id(), FormatId::AacHe, "{rate} Hz, {fpp} frames per packet");
    }
}

// ---- the sample table and the sound description -----------------------------------------------------------------------------------

/// The generated one-track file of `testutil` (11 access units) with the payload of its first `kind` atom replaced; the sizes of the enclosing atoms
/// and the chunk offset follow, so the access units stay where they are.
fn with_payload(file: &[u8], kind: &[u8; 4], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let find = |f: &[u8], kind: &[u8]| f.windows(4).position(|w| w == kind).expect("atom") - 4;
    let at = find(file, kind);
    let old = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
    let mut payload = file[at + 8..at + old].to_vec();
    edit(&mut payload);
    let delta = payload.len() as i64 + 8 - old as i64;
    let mut f = file.to_vec();
    f.splice(at..at + old, atom(kind, &payload));
    for parent in [b"moov", b"trak", b"mdia", b"minf", b"stbl"] {
        let p = find(&f, parent);
        if p < at {
            let size = i64::from(u32::from_be_bytes(f[p..p + 4].try_into().unwrap())) + delta;
            f[p..p + 4].copy_from_slice(&be(size as u32));
        }
    }
    if kind != b"stco" {
        let c = find(&f, b"stco") + 16;
        let offset = i64::from(u32::from_be_bytes(f[c..c + 4].try_into().unwrap())) + delta;
        f[c..c + 4].copy_from_slice(&be(offset as u32));
    }
    f
}

fn eleven_units() -> Vec<u8> {
    let au = [0u8; 40];
    mp4(&vec![&au[..]; 11])
}

fn bad_structure() -> Option<OpenError> {
    Some(OpenError::Unspecified(Unspecified::BadStructure))
}

/// What AudioToolbox answered for `stz2` in place of `stsz`: it is read when its field size is 16 (the reserved bytes in front of the field size do not
/// matter, a count the table cannot hold fails, a smaller one is read as far as it goes), and every other field size (4, 8, 12, 32, 0) fails the open.
#[test]
fn the_compact_sample_size_table_is_read_for_16_bit_fields_only() {
    let base = eleven_units();
    let stz2 = |field: u8, count: u32, entries: usize, reserved: [u8; 3]| {
        let mut f = with_payload(&base, b"stsz", |p| {
            p.clear();
            p.extend_from_slice(&be(0));
            p.extend_from_slice(&reserved);
            p.push(field);
            p.extend_from_slice(&be(count));
            for _ in 0..entries {
                p.extend_from_slice(&40u16.to_be_bytes());
            }
        });
        let at = f.windows(4).position(|w| w == b"stsz").unwrap();
        f[at..at + 4].copy_from_slice(b"stz2");
        open(f)
    };
    assert_eq!(stz2(16, 11, 11, [0; 3]).unwrap().packet_count(), 11);
    assert_eq!(stz2(16, 11, 11, [1, 2, 3]).unwrap().packet_count(), 11);
    assert_eq!(stz2(16, 5, 11, [0; 3]).unwrap().packet_count(), 5);
    assert_eq!(stz2(16, 100, 11, [0; 3]).err(), bad_structure());
    for field in [4, 8, 12, 32, 0] {
        assert_eq!(stz2(field, 11, 11, [0; 3]).err(), bad_structure(), "field size {field}");
    }
}

/// What AudioToolbox answered: an `stsd` announcing no entry has none (`'wht?'`) although an entry follows; counts above 1 over one entry open.
#[test]
fn an_stsd_that_announces_no_entry_has_none() {
    let base = eleven_units();
    let count = |n: u32| open(with_payload(&base, b"stsd", |p| p[4..8].copy_from_slice(&be(n))));
    assert_eq!(count(0).err(), bad_structure());
    for n in [1, 2, u32::MAX] {
        assert_eq!(count(n).unwrap().packet_count(), 11, "count {n}");
    }
}

/// What AudioToolbox answered: the media timescale decides, and only it: zero fails (`'wht?'`), a header that ends right behind it opens (version 0:
/// 16 bytes of payload, version 1: 24), a header that ends before it fails.
#[test]
fn the_media_header_needs_a_timescale_and_nothing_behind_it() {
    let base = eleven_units();
    let mdhd = |edit: &dyn Fn(&mut Vec<u8>)| open(with_payload(&base, b"mdhd", |p| edit(p)));
    assert_eq!(mdhd(&|p| p[12..16].copy_from_slice(&be(0))).err(), bad_structure());
    assert_eq!(mdhd(&|_| {}).unwrap().packet_count(), 11);
    assert_eq!(mdhd(&|p| p.truncate(16)).unwrap().packet_count(), 11);
    assert_eq!(mdhd(&|p| p.truncate(12)).err(), bad_structure());
    let v1 = |len: usize| {
        mdhd(&|p| {
            let (timescale, duration) = (p[12..16].to_vec(), p[16..20].to_vec());
            *p = [&[1, 0, 0, 0][..], &[0; 16], &timescale, &[0; 4], &duration].concat();
            p.truncate(len);
        })
    };
    assert_eq!(v1(32).unwrap().packet_count(), 11);
    assert_eq!(v1(24).unwrap().packet_count(), 11);
    assert_eq!(v1(20).err(), bad_structure());
    assert_eq!(mdhd(&|p| p[0] = 2).err(), bad_structure());
}

/// What AudioToolbox answered: a chunk offset table too short for its header fails the open (`'wht?'`), a header with no entry opens and leaves every
/// packet unplaced, whatever the entry count says.
#[test]
fn a_chunk_offset_table_needs_its_header() {
    let base = eleven_units();
    let stco = |payload: &[u8]| open(with_payload(&base, b"stco", |p| *p = payload.to_vec()));
    assert_eq!(stco(&[0; 4]).err(), bad_structure());
    assert_eq!(stco(&[]).err(), bad_structure());
    let empty = stco(&[0; 8]).unwrap();
    assert_eq!((empty.packet_count(), empty.packet_extent(0)), (11, Some((UNPLACED_OFFSET, 40))));
    let lying = stco(&[0, 0, 0, 0, 0, 0, 0, 9]).unwrap();
    assert_eq!(lying.packet_extent(0), Some((UNPLACED_OFFSET, 40)));
}

// ---- SBR and PS signalling --------------------------------------------------------------------------------------------------------

/// Bytes of the given (value, width) fields, most significant bit first, padded with zeros.
fn bits(fields: &[(u32, usize)]) -> Vec<u8> {
    let mut s: String = fields.iter().map(|&(v, w)| format!("{v:0w$b}")).collect();
    while s.len() % 8 != 0 {
        s.push('0');
    }
    s.as_bytes().chunks(8).map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap()).collect()
}

/// An ASC of a core at sampling index `core_sfi` with SBR (and PS) signalled for the rate of index `ext_sfi`, written hierarchically
/// (`audioObjectType` 5 / 29 in front) or as the backward-compatible extension behind the GASpecificConfig.
fn he_asc(hierarchical: bool, core_aot: u32, core_sfi: u32, cfg: u32, ext_sfi: u32, ps: bool) -> Vec<u8> {
    let ga = [(0, 1), (0, 1), (0, 1)];
    let mut f: Vec<(u32, usize)> = Vec::new();
    if hierarchical {
        f.extend([(if ps { 29 } else { 5 }, 5), (core_sfi, 4), (cfg, 4), (ext_sfi, 4), (core_aot, 5)]);
        f.extend(ga);
    } else {
        f.extend([(core_aot, 5), (core_sfi, 4), (cfg, 4)]);
        f.extend(ga);
        f.extend([(0x2b7, 11), (5, 5), (1, 1), (ext_sfi, 4)]);
        if ps {
            f.extend([(0x548, 11), (1, 1)]);
        }
    }
    bits(&f)
}

fn classified(asc: &[u8]) -> Result<Classified, OpenError> {
    classify(&asc::parse_asc(asc).expect("ASC parses"))
}

/// What AudioToolbox answered for SBR and PS signalling (macOS 27.2, 22.05 kHz core, 44.1 kHz extension unless said otherwise), the same for the
/// hierarchical and the backward-compatible form.
#[test]
fn sbr_and_ps_signalling_follows_the_recorded_rules() {
    const SBR_OPEN: [u32; 8] = [1, 2, 4, 6, 7, 11, 12, 14];
    const PS_PLAIN_MONO: [u32; 6] = [2, 6, 7, 11, 12, 14];
    for hierarchical in [false, true] {
        for cfg in 0..16u32 {
            let what = format!("hierarchical {hierarchical}, channel configuration {cfg}");
            // SBR
            let sbr = classified(&he_asc(hierarchical, 2, 7, cfg, 4, false));
            if SBR_OPEN.contains(&cfg) {
                let c = sbr.unwrap_or_else(|e| panic!("{what}: {e:?}"));
                assert_eq!((c.format, c.rate, c.client_error), (FormatId::AacHe, 44_100, None), "SBR, {what}");
            } else if cfg == 3 || cfg == 5 {
                let c = sbr.unwrap_or_else(|e| panic!("{what}: {e:?}"));
                assert_eq!((c.format, c.client_error), (FormatId::AacHe, Some(ClientError::FormatMismatch)), "SBR, {what}");
            } else {
                assert!(sbr.is_err(), "SBR, {what}");
            }
            // PS, the rate doubled
            let ps = classified(&he_asc(hierarchical, 2, 7, cfg, 4, true));
            match cfg {
                0 => assert!(ps.is_err(), "PS, {what}"),
                1 => {
                    let c = ps.unwrap();
                    assert_eq!((c.format, c.rate, c.channels, c.client_error), (FormatId::AacHeV2, 44_100, 2, None), "PS, {what}");
                }
                c if PS_PLAIN_MONO.contains(&c) => {
                    let c = ps.unwrap();
                    assert_eq!((c.format, c.rate, c.channels, c.client_error), (FormatId::Aac, 22_050, 1, None), "PS, {what}");
                }
                _ => {
                    let c = ps.unwrap();
                    assert_eq!((c.format, c.client_error), (FormatId::AacHeV2, Some(ClientError::NoDecoder)), "PS, {what}");
                }
            }
        }
        // PS over a mono core without a rate change: the client format fails
        let c = classified(&he_asc(hierarchical, 2, 7, 1, 7, true)).unwrap();
        assert_eq!((c.format, c.client_error), (FormatId::AacHeV2, Some(ClientError::FormatMismatch)), "hierarchical {hierarchical}");
        // the extension rate is the core rate or twice it; indices 13 and 14 are reserved
        for ext in [3, 5, 13, 14] {
            assert_eq!(classified(&he_asc(hierarchical, 2, 7, 2, ext, false)).err(), Some(OpenError::Unspecified(Unspecified::BadSbrRate)), "extension {ext}");
            assert_eq!(classified(&he_asc(hierarchical, 2, 7, 2, ext, true)).err(), Some(OpenError::Unspecified(Unspecified::BadSbrRate)), "extension {ext}");
        }
        // SBR counts for AAC-LC only: over an LD core the file is plain LD at the core rate
        let ld = classified(&he_asc(hierarchical, 23, 7, 2, 4, false)).unwrap();
        assert_eq!((ld.format, ld.rate, ld.client_error), (FormatId::AacLd, 22_050, None), "hierarchical {hierarchical}");
    }
    // a hierarchical core that is SBR itself is no audio object type of this reader
    let nested = bits(&[(5, 5), (7, 4), (2, 4), (4, 4), (5, 5), (0, 3)]);
    assert_eq!(classified(&nested).err(), Some(OpenError::Unspecified(Unspecified::UnsupportedAot)));
}

/// What AudioToolbox answered: an ASC that ends inside coreCoderDelay (the two bytes of a core-coder-dependent AAC-LC configuration) opens as plain
/// AAC-LC; one byte is no ASC.
#[test]
fn an_asc_that_ends_inside_the_core_coder_delay_opens() {
    let c = classified(&[0x12, 0x12]).unwrap();
    assert_eq!((c.format, c.rate, c.channels), (FormatId::Aac, 44_100, 2));
    assert!(asc::parse_asc(&[0x12]).is_err());
}

// ---- LOAS: a changed configuration -------------------------------------------------------------------------------------------------

/// A LOAS frame (audioMuxVersion 0, one program, one layer, one access unit): the whole configuration when `config` is given, else "same as before".
fn loas_frame(config: Option<[u8; 2]>, au: &[u8]) -> Vec<u8> {
    let mut f: Vec<(u32, usize)> = Vec::new();
    match config {
        None => f.push((1, 1)),
        Some(asc) => {
            f.extend([(0, 1), (0, 1), (1, 1), (0, 6), (0, 4), (0, 3), (u32::from(asc[0]), 8), (u32::from(asc[1]), 8), (0, 3), (0xff, 8), (0, 1), (0, 1)]);
        }
    }
    f.push((au.len() as u32, 8));
    f.extend(au.iter().map(|&b| (u32::from(b), 8)));
    let body = bits(&f);
    let mut out = vec![0x56, 0xe0 | (body.len() >> 8) as u8, body.len() as u8];
    out.extend(body);
    out
}

/// What AudioToolbox answered for a LOAS stream whose frame 5 carries another configuration (a zeroed one): the reads deliver the frames up to the
/// frame and the read that needs it fails `'fmt?'`.  A frame that repeats the first configuration is no change.
#[test]
fn a_loas_frame_with_another_stream_mux_config_fails_the_read_that_needs_it() {
    let (a, b) = ([0x12, 0x10], [0x11, 0x88]);
    let au = [7u8; 20];
    let stream: Vec<u8> = [loas_frame(Some(a), &au), loas_frame(None, &au), loas_frame(Some(b), &au), loas_frame(None, &au), loas_frame(Some(a), &au)].concat();
    let f = open(stream).unwrap();
    assert_eq!(f.packet_count(), 5);
    assert_eq!(f.packet(1).map(<[u8]>::len), Ok(20));
    assert_eq!(f.packet(2).err(), Some(ReadError::BadFrame));
    assert_eq!(ReadError::BadFrame.os_status(), STATUS_FORMAT_NOT_SUPPORTED);
    assert_eq!(f.packet(3).map(<[u8]>::len), Ok(20));
    assert_eq!(f.packet(4).map(<[u8]>::len), Ok(20));
}

/// The fields of a StreamMuxConfig for `asc` (audioMuxVersion 0, one program, one layer), with `flt` as the frameLengthType and `programs` as numProgram.
fn mux_config_fields(asc: [u8; 2], flt: u32, programs: u32) -> Vec<(u32, usize)> {
    vec![(0, 1), (1, 1), (0, 6), (programs, 4), (0, 3), (u32::from(asc[0]), 8), (u32::from(asc[1]), 8), (flt, 3), (0xff, 8), (0, 1), (0, 1)]
}

fn loas_raw(body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![0x56, 0xe0 | (body.len() >> 8) as u8, body.len() as u8];
    out.extend(body);
    out
}

/// What AudioToolbox answered for a complete LOAS frame behind the first that cannot be read as an access unit (a damaged, cut or unsupported
/// StreamMuxConfig, no payload length, a payload longer than the frame, an empty frame, the length escape running out): the file opens, the table counts the
/// frame as a packet (the length and the bit rate include it), and the read that reaches it fails `'fmt?'`; the frames around it are read.  A zero-length payload
/// is a packet like any other (the decoder rejects it).  Before, a frame like that ended the scan, or panicked the open when its configuration differed.
#[test]
fn a_complete_loas_frame_that_cannot_be_read_stays_in_the_table_and_fails_its_read() {
    let a = [0x12, 0x10];
    let au = [7u8; 20];
    let cfg = |flt: u32, programs: u32| mux_config_fields(a, flt, programs);
    let with = |fields: &[(u32, usize)]| -> Vec<(u32, usize)> { [vec![(0, 1)], fields.to_vec()].concat() };
    let bad: Vec<(&str, Vec<u8>)> = vec![
        ("configuration cut after 19 bits", loas_raw(bits(&with(&cfg(0, 0)[..19.min(cfg(0, 0).len())]).iter().copied().take(5).collect::<Vec<_>>()))),
        ("configuration without payload length", loas_raw(bits(&with(&cfg(0, 0))))),
        ("same configuration, no payload length", loas_raw(vec![0x80])),
        ("frameLengthType 1", loas_raw(bits(&[with(&cfg(1, 0)), vec![(20, 8)], vec![(0, 64)]].concat()))),
        ("numProgram 1", loas_raw(bits(&[with(&cfg(0, 1)), vec![(20, 8)], vec![(0, 64)]].concat()))),
        ("empty frame", loas_raw(Vec::new())),
        ("length escape runs out", loas_raw(bits(&[(1, 1), (255, 8), (255, 8), (255, 8), (0, 40)]))),
        ("payload longer than the frame", loas_raw(bits(&[(1, 1), (200, 8), (0, 64)]))),
        ("other configuration and a payload longer than the frame", loas_raw(bits(&[with(&mux_config_fields([0x11, 0x88], 0, 0)), vec![(200, 8)], vec![(0, 64)]].concat()))),
        ("the same configuration and a payload longer than the frame", loas_raw(bits(&[with(&cfg(0, 0)), vec![(200, 8)], vec![(0, 64)]].concat()))),
    ];
    for (what, frame) in &bad {
        let mut stream = vec![loas_frame(Some(a), &au)];
        stream.extend((0..4).map(|_| loas_frame(None, &au)));
        stream.push(frame.clone());
        stream.extend((0..4).map(|_| loas_frame(None, &au)));
        let f = open(stream.concat()).unwrap_or_else(|e| panic!("{what}: {e:?}"));
        assert_eq!(f.packet_count(), 10, "{what}");
        assert_eq!(f.packet(4).map(<[u8]>::len), Ok(20), "{what}");
        assert_eq!(f.packet(5).err(), Some(ReadError::BadFrame), "{what}");
        assert_eq!(f.packet(6).map(<[u8]>::len), Ok(20), "{what}");
    }
    // a zero-length payload is a packet that reads as zero bytes
    let stream: Vec<u8> = [loas_frame(Some(a), &au), loas_raw(bits(&[(1, 1), (0, 8)])), loas_frame(None, &au)].concat();
    let f = open(stream).unwrap();
    assert_eq!((f.packet_count(), f.packet(1).map(<[u8]>::len), f.packet(2).map(<[u8]>::len)), (3, Ok(0), Ok(20)));
    // the first frame must be readable
    for frame in [loas_raw(bits(&[(1, 1), (200, 8), (0, 64)])), loas_raw(bits(&with(&cfg(1, 0)))), loas_raw(Vec::new())] {
        assert_eq!(open([frame, loas_frame(None, &au)].concat()).err(), Some(OpenError::UnsupportedDataFormat));
    }
}

/// The 60-byte stream a second review found: the second frame carries another configuration and a payload longer than it holds.  It panicked the open (the
/// frame was counted as changed before it was found not to be one).  AudioToolbox opens it and fails the first read `'fmt?'` after no frames.
#[test]
fn a_changed_configuration_with_a_payload_that_does_not_fit_does_not_panic_the_open() {
    let hex = "56e01b200012101fe0a0383838383838383838383838383838383838383856e01b200011881fe6403838383838383838383838383838383838383838";
    let bytes: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
    for hint in [None, Some("loas"), Some("m4a")] {
        let f = AacFile::parse_with_hint(bytes.clone(), hint).unwrap();
        assert_eq!(f.packet_count(), 2);
        assert_eq!(f.packet(1).err(), Some(ReadError::BadFrame));
    }
}

// ---- sound entry types and the bytes of a bit rate -----------------------------------------------------------------------------------

/// What AudioToolbox answered for sound entry types that are not `mp4a`: `'wht?'` for every type but the ones it opens as other codecs; the port names the type
/// so that the reader can tell them apart.
#[test]
fn a_sound_entry_that_is_not_mp4a_names_its_type() {
    let au = [0u8; 40];
    for entry in [*b"np4a", *b"sowt", *b"Opus", *b"alac", *b"ac-3"] {
        let parts = Mp4Parts { sizes: vec![40; 11], constant: None, asc: vec![0x12, 0x10], entry };
        let error = open(mp4_with(&vec![&au[..]; 11], &parts)).err();
        assert_eq!(error, Some(OpenError::NotMp4a(entry)), "entry {entry:?}");
        assert_eq!(OpenError::NotMp4a(entry).os_status(), STATUS_UNSPECIFIED);
    }
}

/// What AudioToolbox answered for the bit rate of fragments with a zero-size sample: the bytes of a `trun` count up to its first zero-size sample, the
/// samples behind it do not (they are still packets), and the next `trun` counts again.
#[test]
fn the_bit_rate_stops_counting_at_a_zero_size_sample_of_a_trun() {
    let aus = access_units(STEREO_M4A);
    let sizes: Vec<u32> = aus.iter().map(|a| a.len() as u32).collect();
    let (first, second) = (sizes[..5].to_vec(), sizes[5..].to_vec());
    let data = |sizes: &[u32]| vec![0u8; sizes.iter().sum::<u32>() as usize];
    let fragments = |zero_in_first: Option<usize>| {
        let mut f = first.clone();
        if let Some(i) = zero_in_first {
            f[i] = 0;
        }
        let one = fragment(0x301, 5, &f, &data(&f), &tfhd_plain());
        let two = fragment(0x301, 6, &second, &data(&second), &tfhd_plain());
        open(fragmented(0, &[one, two])).unwrap()
    };
    let whole: u64 = sizes.iter().map(|&s| u64::from(s)).sum();
    assert_eq!(fragments(None).audio_data_bytes(), whole);
    let f = fragments(Some(2));
    assert_eq!((f.packet_count(), f.audio_data_bytes()), (11, u64::from(first[0] + first[1]) + second.iter().map(|&s| u64::from(s)).sum::<u64>()));
    let last = fragments(Some(4));
    assert_eq!(last.audio_data_bytes(), whole - u64::from(first[4]));
}

// ---- size fields that are no atoms --------------------------------------------------------------------------------------------------

fn at(file: &[u8], kind: &[u8; 4]) -> usize {
    file.windows(4).position(|w| w == kind).expect("atom") - 4
}

fn with_size(kind: &[u8; 4], size: u32) -> Vec<u8> {
    let mut f = STEREO_M4A.to_vec();
    let p = at(&f, kind);
    f[p..p + 4].copy_from_slice(&be(size));
    f
}

/// What AudioToolbox answered: the children of the movie atom are walked one after the other and one whose size field is 0, 2..=7 fails the open, `-1` for the first
/// child and `-2` for a later one, whichever atom it is; an `mvhd` of 8 bytes makes its payload the next child (version and flags 0: size 0); a movie atom of size 1
/// (a 64-bit size that is no size) and one with a zero payload fail like a first child.
#[test]
fn a_child_of_the_movie_atom_that_is_no_atom_fails_the_open_by_its_place() {
    check_layout();
    for (kind, first) in [(b"mvhd", true), (b"trak", false), (b"udta", false)] {
        for size in [0, 2, 7] {
            assert_eq!(open(with_size(kind, size)).err(), Some(OpenError::BadMovieChild { first }), "{} of {size}", String::from_utf8_lossy(kind));
        }
    }
    assert_eq!(open(with_size(b"mvhd", 8)).err(), Some(OpenError::BadMovieChild { first: false }));
    let mut f = STEREO_M4A.to_vec();
    f[28..32].copy_from_slice(&be(1));
    assert_eq!(open(f).err(), Some(OpenError::BadMovieChild { first: true }));
    let mut f = STEREO_M4A.to_vec();
    f[36..890].fill(0);
    assert_eq!(open(f).err(), Some(OpenError::BadMovieChild { first: true }));
    assert_eq!(OpenError::BadMovieChild { first: true }.os_status(), -1);
    assert_eq!(OpenError::BadMovieChild { first: false }.os_status(), -2);
}

/// What AudioToolbox answered for a size field of 0 below the movie atom: the child is lost with the siblings behind it (`'dta?'` where the sound track goes with
/// it, `'wht?'` where the sample table does), and a sample table or data information that has such a child is spoilt; a handler atom too short to hold its type names
/// no handler.
#[test]
fn a_child_of_size_zero_inside_a_track_is_lost_with_what_follows_it() {
    let invalid = Some(OpenError::InvalidFile);
    for kind in [b"mdia", b"hdlr", b"mdhd", b"tkhd"] {
        assert_eq!(open(with_size(kind, 0)).err(), invalid, "{}", String::from_utf8_lossy(kind));
    }
    for kind in [b"minf", b"smhd", b"dinf", b"dref", b"stbl", b"stsd", b"stts", b"stsc", b"stsz", b"stco", b"mp4a", b"esds"] {
        assert_eq!(open(with_size(kind, 0)).err().map(|e| e.os_status()), Some(STATUS_UNSPECIFIED), "{}", String::from_utf8_lossy(kind));
    }
    // a `dref` with a size of 1 whose 64-bit size is 1, and a handler atom of 8, 12 and 16 bytes
    assert_eq!(open(with_size(b"dref", 1)).err(), bad_structure());
    for size in [8, 12, 16] {
        assert_eq!(open(with_size(b"hdlr", size)).err(), invalid, "hdlr of {size}");
    }
    // (a handler atom of 20 bytes holds its type: the sound track is found, and the 14 bytes behind it end the scan of the media atom before `minf`)
    assert_eq!(open(with_size(b"hdlr", 20)).err(), bad_structure());
    // the iTunSMPB items: a size of 0 loses the tag, the file opens with the default priming
    for kind in [b"meta", b"ilst", b"----"] {
        assert_eq!(open(with_size(kind, 0)).unwrap().file_length_frames(), 9152, "{}", String::from_utf8_lossy(kind));
    }
}

/// What AudioToolbox answered: the first child of a `udta` is read from 8 bytes behind the `udta`, bounded by the movie atom, whatever size the `udta` declares
/// (8, 9, 12 bytes, or a size field of 1), so the tag is still found; a `udta` whose first child is not `meta` hides it.
#[test]
fn the_first_child_of_a_udta_is_read_from_behind_it_whatever_the_udta_declares() {
    for size in [8, 9, 12, 1, 250, 0xffff] {
        assert_eq!(open(with_size(b"udta", size)).unwrap().file_length_frames(), 8820, "udta of {size}");
    }
}

/// Several `udta` atoms with a tag each (macOS 27.2): the last usable tag counts; one that is none (another item, an empty udta, a text of 0 valid frames)
/// leaves the earlier one; within a `udta` the first item decides.
#[test]
fn the_last_usable_itunsmpb_of_several_udta_atoms_counts() {
    check_layout();
    let udta = STEREO_M4A[640..890].to_vec();
    let tagged = |frames: u64| {
        let mut u = udta.clone();
        let at = u.windows(8).position(|w| w == b"iTunSMPB").unwrap() + 8 + 4 + 4 + 4 + 4;
        u[at + 28..at + 44].copy_from_slice(format!("{frames:016X}").as_bytes());
        u
    };
    let with_udtas = |udtas: &[Vec<u8>]| {
        let mut f = STEREO_M4A[..640].to_vec();
        for u in udtas {
            f.extend_from_slice(u);
        }
        let moov = 28 + (f.len() - 28);
        f[28..32].copy_from_slice(&be((moov - 28) as u32));
        f.extend_from_slice(&STEREO_M4A[890..]);
        f
    };
    let length = |udtas: &[Vec<u8>]| open(with_udtas(udtas)).unwrap().file_length_frames();
    let (a, b, c) = (tagged(0x1000), tagged(0x2000), tagged(0x3000));
    assert_eq!(length(&[a.clone()]), 0x1000);
    assert_eq!(length(&[a.clone(), b.clone()]), 0x2000);
    assert_eq!(length(&[a.clone(), b.clone(), c.clone()]), 0x3000);
    // an empty udta, another item, a text that is no tag: the earlier tag stays
    let other = {
        let mut u = a.clone();
        let at = u.windows(8).position(|w| w == b"iTunSMPB").unwrap();
        u[at..at + 8].copy_from_slice(b"iTunXXXX");
        u
    };
    assert_eq!(length(&[a.clone(), atom(b"udta", &[])]), 0x1000);
    assert_eq!(length(&[a.clone(), other]), 0x1000);
    assert_eq!(length(&[a.clone(), tagged(0)]), 0x1000);
    // a first one that is no tag does not hide a later one
    assert_eq!(length(&[tagged(0), c]), 0x3000);
}

/// A CAF layered description (`ldsc`): HE-AAC in its first layer makes the file HE-AAC whatever `desc` and the cookie say; one of fewer than 36 bytes is
/// ignored, the first of several counts, another format that differs from `desc` is not reproduced.
#[test]
fn a_layered_description_names_the_format_of_a_caf() {
    let asbd = |rate: f64, format: &[u8; 4], fpp: u32, channels: u32| {
        let mut b = rate.to_be_bytes().to_vec();
        b.extend_from_slice(format);
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(&be(fpp));
        b.extend_from_slice(&be(channels));
        b.extend_from_slice(&be(0));
        b.extend_from_slice(&be(0x0065_0002));
        b
    };
    let with_ldsc = |bodies: &[Vec<u8>], keep_cookie: bool| {
        caf_edited(|chunks| {
            if !keep_cookie {
                chunks.retain(|c| &c.0 != b"kuki");
            }
            for (i, body) in bodies.iter().enumerate() {
                chunks.insert(2 + i, (*b"ldsc", body.clone()));
            }
        })
    };
    let he = asbd(44100.0, b"aach", 2048, 2);
    let state = |file: Vec<u8>| open(file).map(|f| (f.format, f.extra_client_error));
    assert_eq!(state(with_ldsc(&[he.clone()], false)).unwrap(), (FormatId::AacHe, None));
    assert_eq!(state(with_ldsc(&[asbd(44100.0, b"aacp", 2048, 2)], false)).unwrap(), (FormatId::AacHeV2, None));
    // over the plain cookie: no decoder ('!dat')
    assert_eq!(state(with_ldsc(&[he.clone()], true)).unwrap(), (FormatId::AacHe, Some(ClientError::NoDecoder)));
    // too short, or a layer that is the same format as `desc`: nothing changes
    assert_eq!(state(with_ldsc(&[he[..35].to_vec()], false)).unwrap().0, FormatId::Aac);
    assert_eq!(state(with_ldsc(&[asbd(44100.0, b"aac ", 1024, 2), he.clone()], false)).unwrap(), (FormatId::Aac, None));
    // the first one counts
    assert_eq!(state(with_ldsc(&[he.clone(), asbd(44100.0, b"aac ", 1024, 2)], false)).unwrap().0, FormatId::AacHe);
    // another format than HE-AAC that differs from `desc` (here the rate): not reproduced
    assert_eq!(state(with_ldsc(&[asbd(22050.0, b"aac ", 1024, 2)], false)).unwrap().1, Some(ClientError::LayeredFormat));
    // `desc` is needed
    assert!(matches!(open(caf_edited(|c| {
        c.retain(|x| &x.0 != b"desc");
        c.insert(1, (*b"ldsc", he.clone()));
    })), Err(OpenError::InvalidFile)));
}

/// `aacp` without a cookie (no `kuki`): AudioToolbox opens it for 1 or 2 channels at 2048 frames per packet, any other shape fails `'fmt?'`.
#[test]
fn an_aacp_desc_without_a_cookie_opens_for_two_thousand_forty_eight_frames() {
    let desc = |fpp: u32, channels: u32| {
        caf_edited(|chunks| {
            chunks.retain(|c| &c.0 != b"kuki");
            for c in chunks.iter_mut().filter(|c| &c.0 == b"desc") {
                c.1[8..12].copy_from_slice(b"aacp");
                c.1[20..24].copy_from_slice(&be(fpp));
                c.1[24..28].copy_from_slice(&be(channels));
            }
        })
    };
    for (fpp, channels, opens) in [(2048, 1, true), (2048, 2, true), (2048, 3, false), (1024, 2, false), (1024, 1, false), (4096, 2, false)] {
        let f = open(desc(fpp, channels)).unwrap();
        assert_eq!(f.extra_client_error.is_none(), opens, "{channels} channels, {fpp} frames per packet");
        if !opens {
            assert_eq!(f.extra_client_error, Some(ClientError::FormatMismatch));
        }
    }
}

/// The size field of a container of the track shortened by a few bytes (macOS 27.2): the tables of the sample table are bounded by the end of the TRACK, not by
/// the `stbl`, `minf` or `mdia` that hold them; shortening the `trak` cuts the last table, a `stbl` short by more than the table's header cannot find it.
#[test]
fn a_container_short_by_a_few_bytes_still_gives_its_last_table_whole() {
    check_layout();
    let at = |kind: &[u8; 4]| STEREO_M4A.windows(4).position(|w| w == kind).unwrap() - 4;
    let short = |kind: &[u8; 4], by: u32| {
        let mut f = STEREO_M4A.to_vec();
        let size = u32::from_be_bytes(f[at(kind)..at(kind) + 4].try_into().unwrap());
        f[at(kind)..at(kind) + 4].copy_from_slice(&be(size - by));
        f
    };
    // the fixture's `stco` is the last child of its `stbl` (12 bytes: header and one chunk offset of 4 bytes)
    let packets = |f: Vec<u8>| open(f).map(|f| f.packet_count());
    assert_eq!(packets(STEREO_M4A.to_vec()).unwrap(), 11);
    for kind in [b"stbl", b"minf", b"mdia"] {
        for by in [1, 3, 4] {
            assert_eq!(packets(short(kind, by)).unwrap(), 11, "{} short by {by}", String::from_utf8_lossy(kind));
            let f = open(short(kind, by)).unwrap();
            assert_eq!(f.packet(0).map(<[u8]>::len).unwrap(), 6, "the first packet is located (its chunk offset is whole)");
        }
    }
    // the table's header must start inside the stbl: 12 + 8 bytes short of its end loses it
    assert!(open(short(b"stbl", 12 + 8)).is_err());
    // a track that is short cuts the table off: the one chunk offset is lost with its last 4 bytes (the packets are then read from the start of the file)
    let intact = open(STEREO_M4A.to_vec()).unwrap();
    assert_ne!(open(short(b"trak", 4)).unwrap().packet(0).unwrap(), intact.packet(0).unwrap());
}
