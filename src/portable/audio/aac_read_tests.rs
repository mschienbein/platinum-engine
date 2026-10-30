//! Tests of the AAC input of [`Reader`]: AudioToolbox's results on generated fixtures (the whole client buffer after every call, hashed), the routing
//! of every container, typed refusals, AudioToolbox's statuses and the failure poisoning.

use super::aac_read::MAX_AAC_BYTES;
use super::*;
use crate::portable::aac::decoder::testutil::*;

struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pn-aac-reader-{tag}-{}-{}", std::process::id(), NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).unwrap();
        Dir(dir)
    }
    fn file(&self, name: &str, data: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, data).unwrap();
        path
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(0x100_0000_01b3);
    }
}

/// One read scenario of the reference probe, named `ilv|pl` + frames per call + `s|z`: interleaved or planar stereo reads into buffers prefilled with a
/// sentinel (`s`) or zeros (`z`); the whole buffer of every call is hashed with its returned count; the loop ends after two calls that return 0 or at
/// the first failure. Returns (frames, calls, hash, failure as `native/<operation>/<status>`), the failure empty for a read that went through.
fn scenario(path: &Path, name: &str) -> (u64, u64, u64, String) {
    let planar = name.starts_with("pl");
    let block: usize = name.trim_start_matches(|c: char| c.is_ascii_alphabetic()).trim_end_matches(['s', 'z']).parse().unwrap();
    let rate = Reader::open(path, None).unwrap().source_format().sample_rate;
    let mut reader = Reader::open(path, planar.then_some(rate)).unwrap();
    let channels = reader.source_format().channels_per_frame as usize;
    if planar {
        reader.set_stereo_planar(rate).unwrap();
    }
    let fill = if name.ends_with('s') { f32::from_bits(0x7fc0_5a5a) } else { 0.0 };
    let mut buf = vec![fill; block * channels];
    let (mut left, mut right) = (vec![fill; block], vec![fill; block]);
    let (mut frames, mut calls, mut hash, mut zeros) = (0u64, 0u64, 0xcbf2_9ce4_8422_2325u64, 0);
    loop {
        buf.fill(fill);
        left.fill(fill);
        right.fill(fill);
        let result = if planar { reader.read_stereo_planar(&mut left, &mut right) } else { reader.read(&mut buf) };
        calls += 1;
        let n = match result {
            Ok(n) => n,
            Err(CodecError::Native { operation, status }) => return (frames, calls, hash, format!("native/{operation}/{status}")),
            Err(CodecError::UnsupportedCodec { codec: "AAC", operation: "decoding", .. }) => return (frames, calls, hash, "refused".to_owned()),
            Err(other) => panic!("{other}"),
        };
        let words: Vec<f32> = if planar { left.iter().chain(right.iter()).copied().collect() } else { buf.clone() };
        fnv(&mut hash, &(n as u64).to_le_bytes());
        fnv(&mut hash, &words.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect::<Vec<u8>>());
        if n == 0 {
            zeros += 1;
            if zeros >= 2 {
                return (frames, calls, hash, String::new());
            }
            continue;
        }
        frames += n as u64;
    }
}

fn fixture(name: &str) -> &'static [u8] {
    match name {
        "tiny-44100-2.m4a" => STEREO_M4A,
        "tiny-48000-1.m4a" => MONO_M4A,
        "tiny-44100-2.adts" => STEREO_ADTS,
        "tiny-44100-2.caf" => STEREO_CAF,
        "tiny-44100-2.loas" => STEREO_LOAS,
        other => panic!("fixture {other}"),
    }
}

/// (file, scenario, frames, calls, hash) as `ExtAudioFileRead` of macOS 27.2 delivered them through the engine's `apple_codec::Reader`.
const GOLDEN: &[(&str, &str, u64, u64, u64)] = &[
    ("tiny-44100-2.m4a", "ilv512s", 8820, 20, 0x3bbb3042c72976e2),
    ("tiny-44100-2.m4a", "ilv512z", 8820, 20, 0x3026701229caa3e2),
    ("tiny-44100-2.m4a", "ilv1025s", 8820, 11, 0xfb3e43fe66fb93b2),
    ("tiny-44100-2.m4a", "ilv4096s", 8820, 5, 0x82eaab609d7fca56),
    ("tiny-44100-2.m4a", "ilv1024s", 8820, 11, 0x81b7995bb08f3576),
    ("tiny-44100-2.m4a", "pl512z", 8820, 20, 0xdee7cfd1c397136a),
    ("tiny-44100-2.m4a", "pl512s", 8820, 20, 0x1b6a4f44ddc6f16a),
    ("tiny-44100-2.m4a", "pl4096s", 8820, 5, 0xca6c6535051d5896),
    ("tiny-44100-2.m4a", "pl1025s", 8820, 11, 0x482501bed27f91f6),
    ("tiny-44100-2.m4a", "ilv7s", 8820, 1262, 0xd55ab0a07938c430),
    ("tiny-44100-2.m4a", "ilv333s", 8820, 29, 0x731fe8736f446aa2),
    ("tiny-44100-2.m4a", "pl333s", 8820, 29, 0xb6b1adb368f7b98e),
    ("tiny-44100-2.m4a", "ilv1s", 8820, 8822, 0x13b5db25048ccb2c),
    ("tiny-44100-2.m4a", "pl1s", 8820, 8822, 0x13b5db25048ccb2c),
    ("tiny-48000-1.m4a", "ilv512s", 9600, 21, 0x0f842d73934d072c),
    ("tiny-48000-1.m4a", "ilv512z", 9600, 21, 0x3aaaafc489b5daac),
    ("tiny-48000-1.m4a", "ilv1025s", 9600, 12, 0x61c42fd768c9f09a),
    ("tiny-48000-1.m4a", "ilv4096s", 9600, 5, 0xf6e70b795913f8dc),
    ("tiny-48000-1.m4a", "ilv1024s", 9600, 12, 0xb674dfbf4dca29e4),
    ("tiny-48000-1.m4a", "pl512z", 9600, 21, 0x00d75fba375a1dea),
    ("tiny-48000-1.m4a", "pl512s", 9600, 21, 0x1eb4cf9b070e96ea),
    ("tiny-48000-1.m4a", "pl4096s", 9600, 5, 0x3c87a1b7fe37f29e),
    ("tiny-48000-1.m4a", "pl1025s", 9600, 12, 0x10d8fa20f8f83310),
    ("tiny-48000-1.m4a", "ilv7s", 9600, 1374, 0xe8cb6afcece8c3f0),
    ("tiny-48000-1.m4a", "ilv333s", 9600, 31, 0xfd4e07b31c836925),
    ("tiny-48000-1.m4a", "pl333s", 9600, 31, 0x36c8b423056a5802),
    ("tiny-48000-1.m4a", "ilv1s", 9600, 9602, 0xdd9446726d7f04ab),
    ("tiny-48000-1.m4a", "pl1s", 9600, 9602, 0x1a4fc289dc0d3f6d),
    ("tiny-44100-2.adts", "ilv512s", 11264, 24, 0xf5e81b59e7dbee33),
    ("tiny-44100-2.adts", "ilv512z", 11264, 24, 0x8e9074d35b4e3e33),
    ("tiny-44100-2.adts", "ilv1025s", 11264, 13, 0x9805f9f06ce77750),
    ("tiny-44100-2.adts", "ilv4096s", 11264, 5, 0x9cf1237da3da0cab),
    ("tiny-44100-2.adts", "ilv1024s", 11264, 13, 0x571454ef38622dcb),
    ("tiny-44100-2.adts", "pl512z", 11264, 24, 0xe3ae6bb1afea6baf),
    ("tiny-44100-2.adts", "pl512s", 11264, 24, 0x14612b0200741baf),
    ("tiny-44100-2.adts", "pl4096s", 11264, 5, 0x99e1dfb492cc976b),
    ("tiny-44100-2.adts", "pl1025s", 11264, 13, 0x2285bcb6e1bfca74),
    ("tiny-44100-2.adts", "ilv7s", 11264, 1612, 0x44aa9cc153432df1),
    ("tiny-44100-2.adts", "ilv333s", 11264, 36, 0x2f02900a5833ecdd),
    ("tiny-44100-2.adts", "pl333s", 11264, 36, 0x54d6796771e224dd),
    ("tiny-44100-2.adts", "ilv1s", 11264, 11266, 0x0ec9bc22821f91b3),
    ("tiny-44100-2.adts", "pl1s", 11264, 11266, 0x0ec9bc22821f91b3),
    ("tiny-44100-2.caf", "ilv512s", 8820, 20, 0x3bbb3042c72976e2),
    ("tiny-44100-2.caf", "ilv512z", 8820, 20, 0x3026701229caa3e2),
    ("tiny-44100-2.caf", "ilv1025s", 8820, 11, 0xfb3e43fe66fb93b2),
    ("tiny-44100-2.caf", "ilv4096s", 8820, 5, 0x82eaab609d7fca56),
    ("tiny-44100-2.caf", "ilv1024s", 8820, 11, 0x81b7995bb08f3576),
    ("tiny-44100-2.caf", "pl512z", 8820, 20, 0xdee7cfd1c397136a),
    ("tiny-44100-2.caf", "pl512s", 8820, 20, 0x1b6a4f44ddc6f16a),
    ("tiny-44100-2.caf", "pl4096s", 8820, 5, 0xca6c6535051d5896),
    ("tiny-44100-2.caf", "pl1025s", 8820, 11, 0x482501bed27f91f6),
    ("tiny-44100-2.caf", "ilv7s", 8820, 1262, 0xd55ab0a07938c430),
    ("tiny-44100-2.caf", "ilv333s", 8820, 29, 0x731fe8736f446aa2),
    ("tiny-44100-2.caf", "pl333s", 8820, 29, 0xb6b1adb368f7b98e),
    ("tiny-44100-2.caf", "ilv1s", 8820, 8822, 0x13b5db25048ccb2c),
    ("tiny-44100-2.caf", "pl1s", 8820, 8822, 0x13b5db25048ccb2c),
    ("tiny-44100-2.loas", "ilv512s", 10240, 22, 0x9ababb963636e68f),
    ("tiny-44100-2.loas", "ilv512z", 10240, 22, 0x497b6046800d368f),
    ("tiny-44100-2.loas", "ilv1025s", 10240, 12, 0xc72e48c87dda57d0),
    ("tiny-44100-2.loas", "ilv4096s", 10240, 5, 0x958495e7bd5be7bb),
    ("tiny-44100-2.loas", "ilv1024s", 10240, 12, 0xd6f8aee9d808464b),
    ("tiny-44100-2.loas", "pl512z", 10240, 22, 0xf04adff016e27597),
    ("tiny-44100-2.loas", "pl512s", 10240, 22, 0x34fff2b18a542597),
    ("tiny-44100-2.loas", "pl4096s", 10240, 5, 0xb323df38fc105843),
    ("tiny-44100-2.loas", "pl1025s", 10240, 12, 0x139cc2c648b3d794),
    ("tiny-44100-2.loas", "ilv7s", 10240, 1465, 0xde064f84644eef81),
    ("tiny-44100-2.loas", "ilv333s", 10240, 33, 0x4f1ed8df6801de1d),
    ("tiny-44100-2.loas", "pl333s", 10240, 33, 0x743a1fa0ee75fd39),
    ("tiny-44100-2.loas", "ilv1s", 10240, 10242, 0xb45f493c501fbf03),
    ("tiny-44100-2.loas", "pl1s", 10240, 10242, 0xb45f493c501fbf03),
];

#[test]
fn reads_match_audiotoolbox_including_the_padding_behind_the_returned_frames() {
    let dir = Dir::new("golden");
    for &(name, scn, frames, calls, hash) in GOLDEN {
        let path = dir.file(name, fixture(name));
        assert_eq!(scenario(&path, scn), (frames, calls, hash, String::new()), "{name} {scn}");
    }
}

#[test]
fn facts_match_audiotoolbox() {
    let dir = Dir::new("facts");
    // (file, rate, channels, file length frames, bitrate, client bytes per frame)
    for (name, rate, channels, length, bitrate) in [
        ("tiny-44100-2.m4a", 44_100.0, 2, 8820, 94_244),
        ("tiny-48000-1.m4a", 48_000.0, 1, 9600, 42_562),
        ("tiny-44100-2.adts", 44_100.0, 2, 11_264, 96_656),
        ("tiny-44100-2.caf", 44_100.0, 2, 8820, 94_244),
        ("tiny-44100-2.loas", 44_100.0, 2, 10_240, 65_081),
    ] {
        let reader = Reader::open(dir.file(name, fixture(name)), None).unwrap();
        let source = reader.source_format();
        assert_eq!((source.sample_rate, source.format_id, source.format_flags, source.bytes_per_packet), (rate, u32::from_be_bytes(*b"aac "), 0, 0), "{name}");
        assert_eq!((source.frames_per_packet, source.bytes_per_frame, source.channels_per_frame, source.bits_per_channel), (1024, 0, channels, 0), "{name}");
        let client = reader.client_format();
        assert_eq!((client.sample_rate, client.format_id, client.format_flags, client.channels_per_frame), (rate, LPCM, 9, channels), "{name}");
        assert_eq!((client.bytes_per_packet, client.frames_per_packet, client.bytes_per_frame, client.bits_per_channel), (4 * channels, 1, 4 * channels, 32));
        assert_eq!(reader.file_length_frames().unwrap(), length, "{name}");
        assert_eq!(reader.bitrate().unwrap(), bitrate, "{name}");
    }
}

#[test]
fn every_container_and_prefix_is_routed_to_the_aac_reader() {
    let dir = Dir::new("routes");
    let mut tag = b"ID3\x04\x00\x00\x00\x00\x00\x28".to_vec();
    tag.resize(50, 0);
    let mut v1 = b"TAG".to_vec();
    v1.resize(128, 0);
    let opens = |name: &str, data: &[u8]| Reader::open(dir.file(name, data), None);
    for name in ["a", "a.m4a", "a.mp4", "a.bin", "a.wav", "a.caf", "a.3gp"] {
        // the same content opens whatever the name, except where the name commits to another type
        assert_eq!(opens(name, STEREO_M4A).unwrap().file_length_frames().unwrap(), 8820, "{name}");
        assert_eq!(opens(name, STEREO_CAF).unwrap().file_length_frames().unwrap(), 8820, "{name}");
        assert_eq!(opens(name, STEREO_ADTS).unwrap().file_length_frames().unwrap(), 11_264, "{name}");
        assert_eq!(opens(name, STEREO_LOAS).unwrap().file_length_frames().unwrap(), 10_240, "{name}");
        assert_eq!(opens(name, &[tag.clone(), STEREO_ADTS.to_vec()].concat()).unwrap().file_length_frames().unwrap(), 11_264, "{name}");
        assert_eq!(opens(name, &[v1.clone(), tag.clone(), STEREO_LOAS.to_vec()].concat()).unwrap().file_length_frames().unwrap(), 10_240, "{name}");
    }
    for name in ["a.aac", "a.adts", "a.AAC"] {
        assert_eq!(opens(name, STEREO_ADTS).unwrap().file_length_frames().unwrap(), 11_264, "{name}");
        // an ADTS name does not look at MP4 / CAF / LOAS data
        for data in [STEREO_M4A, STEREO_CAF, STEREO_LOAS] {
            assert!(matches!(opens(name, data), Err(CodecError::Native { operation: "ExtAudioFileOpenURL", status }) if status == i32::from_be_bytes(*b"sync")), "{name}");
        }
    }
    for name in ["a.loas", "a.latm"] {
        assert_eq!(opens(name, STEREO_LOAS).unwrap().file_length_frames().unwrap(), 10_240, "{name}");
        assert!(matches!(opens(name, STEREO_ADTS), Err(CodecError::Native { status, .. }) if status == i32::from_be_bytes(*b"fmt?")), "{name}");
    }
    // an MPEG audio name commits to the MPEG reader: whatever it makes of AAC bytes, it is not the AAC reader's result
    assert!(opens("a.mp3", STEREO_M4A).map_or(true, |r| r.source_format().format_id != u32::from_be_bytes(*b"aac ")));
    // a QuickTime name refuses what is not an MP4 file
    assert!(matches!(opens("a.mov", STEREO_ADTS), Err(CodecError::Native { status: -12_848, .. })));
    assert!(opens("a.mov", STEREO_M4A).is_ok());
    // MP4 and CAF behind a tag are sniffed but do not open
    assert!(matches!(opens("a", &[tag.clone(), STEREO_M4A.to_vec()].concat()), Err(CodecError::Native { status, .. }) if status == i32::from_be_bytes(*b"dta?")));
    assert!(matches!(opens("a", &[tag, STEREO_CAF.to_vec()].concat()), Err(CodecError::Native { status, .. }) if status == i32::from_be_bytes(*b"dta?")));
}

#[test]
fn audiotoolbox_statuses_of_broken_files_are_reproduced() {
    let dir = Dir::new("status");
    let status = |name: &str, data: &[u8]| match Reader::open(dir.file(name, data), None) {
        Err(CodecError::Native { operation: "ExtAudioFileOpenURL", status }) => status,
        other => panic!("{name}: {:?}", other.err()),
    };
    let four = |s: &[u8; 4]| i32::from_be_bytes(*s);
    assert_eq!(status("empty.m4a", &[]), four(b"wht?"));
    assert_eq!(status("junk", &[0x55; 40]), four(b"typ?"));
    assert_eq!(status("header.aac", &[0xff, 0xf1, 0x50, 0x80, 0x01, 0xa0, 0, 0, 0, 0, 0, 0]), four(b"wht?"));
    assert_eq!(status("header", &[0xff, 0xf1, 0x50, 0x80, 0x01, 0xa0, 0, 0, 0, 0, 0, 0]), four(b"wht?"));
    // an ADTS file whose first access unit does not parse: 'fmt?'
    let mut bad = STEREO_ADTS.to_vec();
    bad[12] ^= 0xff;
    bad[13] ^= 0x55;
    assert_eq!(status("bad.aac", &bad), four(b"fmt?"));
    // a movie atom that is cut off: 'dta?'
    let cut = &STEREO_M4A[..STEREO_M4A.windows(4).position(|w| w == b"moov").unwrap() + 40];
    assert_eq!(status("cut.m4a", cut), four(b"dta?"));
}

/// Damage to the container is the same for every codec: `ftyp` 0..28, `moov` 28..890, `free` 890..4088, `mdat` after it.
#[test]
fn structural_damage_fails_with_the_status_of_the_container_not_a_description() {
    let dir = Dir::new("structure");
    let status = |name: &str, data: &[u8]| match Reader::open(dir.file(name, data), None) {
        Err(CodecError::Native { operation: "ExtAudioFileOpenURL", status }) => status,
        other => panic!("{name}: {:?}", other.err()),
    };
    let four = |s: &[u8; 4]| i32::from_be_bytes(*s);
    let patch = |at: usize, value: &[u8]| {
        let mut d = STEREO_M4A.to_vec();
        d[at..at + value.len()].copy_from_slice(value);
        d
    };
    assert_eq!(status("a.m4a", &patch(0, &[0, 0, 0, 3])), four(b"typ?"), "ftyp of 3 bytes");
    assert_eq!(status("b.m4a", &patch(0, &[0xff; 4])), four(b"typ?"), "ftyp beyond the file");
    assert_eq!(status("c.m4a", &patch(3, &[27])), four(b"dta?"), "ftyp one byte short: the next atom is garbage");
    assert_eq!(status("d.m4a", &STEREO_M4A[..8]), four(b"typ?"));
    assert_eq!(status("e.m4a", &STEREO_M4A[..28]), four(b"dta?"), "no movie atom");
    assert_eq!(status("f.m4a", &patch(28, &[0, 0, 0, 3])), four(b"dta?"), "movie atom of 3 bytes");
    assert_eq!(status("h.m4a", &patch(0, &[0, 0, 0, 0])), four(b"typ?"), "ftyp to the end of the file");
    // a movie atom beyond the end of the file reads as far as the file goes; one that is a byte short cuts the `data` atom of the iTunSMPB, which is
    // then ignored: the default priming applies
    for size in [0xffffu32, 0xffff_ffff] {
        let mut d = patch(28, &size.to_be_bytes());
        assert_eq!(Reader::open(dir.file("g.m4a", &d), None).unwrap().file_length_frames().unwrap(), 8820, "{size}");
        d[28..32].copy_from_slice(&861u32.to_be_bytes());
        assert_eq!(Reader::open(dir.file("g2.m4a", &d), None).unwrap().file_length_frames().unwrap(), 9152);
    }
    // a 64-bit size runs to the end of the file however large it is, as long as it is not negative
    let wide = |size: u64| {
        let mut d = STEREO_M4A[..28].to_vec();
        d.extend_from_slice(&[0, 0, 0, 1]);
        d.extend_from_slice(b"moov");
        d.extend_from_slice(&size.to_be_bytes());
        d.extend_from_slice(&STEREO_M4A[36..890]);
        d.extend_from_slice(&3190u32.to_be_bytes()); // the free atom gives the 8 bytes back: the media data stays where the chunk offsets say
        d.extend_from_slice(&STEREO_M4A[894..898]);
        d.extend_from_slice(&STEREO_M4A[906..]);
        d
    };
    for size in [10_000, 1 << 40, (1 << 63) - 1] {
        assert_eq!(Reader::open(dir.file("wide.m4a", &wide(size)), None).unwrap().file_length_frames().unwrap(), 8820, "{size}");
    }
    assert_eq!(status("neg.m4a", &wide(1 << 63)), four(b"dta?"));
    assert_eq!(status("neg2.m4a", &wide(u64::MAX)), four(b"dta?"));
    // a CAF cut inside its chunks: open with what is there, or 'dta?' / 'chk?'
    let caf = |n: usize| Reader::open(dir.file("cut.caf", &STEREO_CAF[..n]), None);
    assert_eq!(caf(100).unwrap().file_length_frames().unwrap(), 0);
    for (n, expected) in [(20, b"dta?"), (51, b"dta?"), (270, b"chk?"), (4091, b"chk?")] {
        assert!(matches!(caf(n), Err(CodecError::Native { operation: "ExtAudioFileOpenURL", status }) if status == four(expected)), "cut at {n}");
    }
    let mut cut = caf(100).unwrap();
    assert!(matches!(cut.read(&mut [0.0; 16]), Err(CodecError::Native { operation: "ExtAudioFileRead", status: -66567 })));
}

#[test]
fn a_caf_desc_the_converter_cannot_use_fails_where_audiotoolbox_fails() {
    let dir = Dir::new("desc");
    let four = |s: &[u8; 4]| i32::from_be_bytes(*s);
    let open = |name: &str, edit: &dyn Fn(&mut Vec<u8>)| Reader::open(dir.file(name, &caf_edited(|c| edit(&mut c.iter_mut().find(|c| &c.0 == b"desc").unwrap().1))), None);
    // no frames per packet: the rate property and the first read fail
    let mut zero = open("zero.caf", &|d| d[20..24].fill(0)).unwrap();
    assert_eq!(zero.source_format().frames_per_packet, 0);
    assert_eq!(zero.file_length_frames().unwrap(), 8820);
    assert!(matches!(zero.bitrate(), Err(CodecError::Native { operation: "AudioFileGetProperty(bit rate)", status }) if status == four(b"dta?")));
    assert!(matches!(zero.read(&mut [0.0; 1024]), Err(CodecError::Native { operation: "ExtAudioFileRead", status }) if status == four(b"bada")));
    assert!(matches!(zero.read(&mut [0.0; 1024]), Err(CodecError::Poisoned)));
    // bits per channel: the client format fails
    assert!(matches!(open("bits.caf", &|d| d[28..32].copy_from_slice(&16u32.to_be_bytes())), Err(CodecError::Native { operation: "ExtAudioFileSetProperty(client format)", status }) if status == four(b"fmt?")));
    // the format flags are reported
    assert_eq!(open("flags.caf", &|d| d[12..16].copy_from_slice(&5u32.to_be_bytes())).unwrap().source_format().format_flags, 5);
}

#[test]
fn names_that_commit_to_other_types_fail_before_the_content_is_looked_at() {
    let dir = Dir::new("names");
    let status = |name: &str, data: &[u8]| match Reader::open(dir.file(name, data), None) {
        Err(CodecError::Native { operation: "ExtAudioFileOpenURL", status }) => status,
        other => panic!("{name}: {:?}", other.err()),
    };
    let four = |s: &[u8; 4]| i32::from_be_bytes(*s);
    for name in ["a.ogg", "a.oga", "a.opus", "A.OGG"] {
        for data in [STEREO_M4A, STEREO_ADTS, STEREO_CAF, &[1u8, 2, 3][..]] {
            assert_eq!(status(name, data), four(b"dta?"), "{name}");
        }
        assert_eq!(status(name, &[]), four(b"wht?"), "{name}");
    }
    for data in [STEREO_M4A, STEREO_ADTS, &[1u8][..]] {
        assert_eq!(status("a.sd2", data), -39);
    }
    assert_eq!(status("a.sd2", &[]), four(b"wht?"));
}

#[test]
fn what_audiotoolbox_decodes_but_the_port_does_not_is_refused_with_a_typed_error() {
    let dir = Dir::new("refusals");
    let aus = access_units(STEREO_M4A);
    let refs: Vec<&[u8]> = aus.iter().map(|a| a.as_slice()).collect();
    let sizes: Vec<u32> = aus.iter().map(|a| a.len() as u32).collect();
    let with = |asc: &[u8]| mp4_with(&refs, &Mp4Parts { sizes: sizes.clone(), constant: None, asc: asc.to_vec(), entry: *b"mp4a" });
    for (name, asc) in [("he.m4a", &[0x13u8, 0x90, 0x56, 0xe5, 0xa0][..]), ("ld.m4a", &[0xb9, 0x90]), ("eld.m4a", &[0xf8, 0xe8, 0x40, 0x00])] {
        assert!(matches!(Reader::open(dir.file(name, &with(asc)), None), Err(CodecError::UnsupportedCodec { codec: "AAC", operation: "decoding", .. })), "{name}");
    }
    // Main profile is refused by AudioToolbox itself
    assert!(matches!(Reader::open(dir.file("main.m4a", &with(&[0x0a, 0x10])), None), Err(CodecError::Native { operation: "ExtAudioFileSetProperty(client format)", .. })));
    // a CAF whose sample rate is not exactly the stream's
    let mut caf = STEREO_CAF.to_vec();
    let desc = caf.windows(4).position(|w| w == b"desc").unwrap() + 12;
    caf[desc..desc + 8].copy_from_slice(&44_100.5f64.to_be_bytes());
    assert!(matches!(Reader::open(dir.file("rate.caf", &caf), None), Err(CodecError::UnsupportedCodec { codec: "AAC", operation: "decoding", .. })));
    // sample-rate conversion
    let path = dir.file("a.m4a", STEREO_M4A);
    assert!(matches!(Reader::open(&path, Some(48_000.0)), Err(CodecError::UnsupportedSampleRateConversion { source_rate: 44_100.0, client_rate: 48_000.0 })));
    let mut reader = Reader::open(&path, None).unwrap();
    assert!(matches!(reader.set_stereo_planar(22_050.0), Err(CodecError::UnsupportedSampleRateConversion { .. })));
    assert_eq!(reader.client_format().format_flags, 9);
    reader.set_stereo_planar(44_100.0).unwrap();
    assert_eq!((reader.client_format().format_flags, reader.client_format().channels_per_frame), (0x29, 2));
}

#[test]
fn a_failed_read_poisons_the_reader_like_the_native_one() {
    let dir = Dir::new("poison");
    let aus = access_units(STEREO_M4A);
    let mut sizes: Vec<u32> = aus.iter().map(|a| a.len() as u32).collect();
    sizes[4] = 2049; // above the 1024 bytes per channel the decoder takes
    let mut padded = aus.clone();
    padded[4].resize(2049, 0);
    let refs: Vec<&[u8]> = padded.iter().map(|a| a.as_slice()).collect();
    let file = mp4_with(&refs, &Mp4Parts { sizes, constant: None, asc: vec![0x12, 0x10], entry: *b"mp4a" });
    let path = dir.file("oversize.m4a", &file);
    for planar in [false, true] {
        let mut reader = Reader::open(&path, None).unwrap();
        if planar {
            reader.set_stereo_planar(44_100.0).unwrap();
        }
        let (mut interleaved, mut l, mut r) = (vec![0f32; 1024], vec![0f32; 512], vec![0f32; 512]);
        let mut failure = None;
        for _ in 0..64 {
            let result = if planar { reader.read_stereo_planar(&mut l, &mut r) } else { reader.read(&mut interleaved) };
            match result {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        let operation = if planar { "ExtAudioFileRead(planar stereo)" } else { "ExtAudioFileRead" };
        assert!(matches!(failure, Some(CodecError::Native { operation: o, status: -50 }) if o == operation), "{failure:?}");
        for _ in 0..2 {
            assert!(matches!(reader.read(&mut interleaved), Err(CodecError::Poisoned)));
            assert!(matches!(reader.read_stereo_planar(&mut l, &mut r), Err(CodecError::Poisoned)));
            assert!(matches!(reader.set_stereo_planar(44_100.0), Err(CodecError::Poisoned)));
        }
    }
}

#[test]
fn reads_validate_their_buffers_and_the_client_format() {
    let dir = Dir::new("buffers");
    let path = dir.file("a.m4a", STEREO_M4A);
    let mut reader = Reader::open(&path, None).unwrap();
    assert!(matches!(reader.read(&mut [0.0; 3]), Err(CodecError::InvalidInput(_))));
    assert!(matches!(reader.read_stereo_planar(&mut [0.0; 4], &mut [0.0; 4]), Err(CodecError::InvalidInput(_))));
    assert_eq!(reader.read(&mut []).unwrap(), 0);
    let mut first = [0f32; 1024];
    assert_eq!(reader.read(&mut first).unwrap(), 512);
    // the client format cannot change once the stream is being read
    assert!(matches!(reader.set_stereo_planar(44_100.0), Err(CodecError::UnsupportedCodec { codec: "AAC", .. })));
    assert_eq!(reader.client_format().format_flags, 9);
}

#[test]
fn inputs_over_two_gibibytes_are_refused_before_they_are_read() {
    let devnull = File::open("/dev/null").unwrap();
    for length in [MAX_AAC_BYTES + 1, u64::MAX] {
        let result = Reader::open_aac(devnull.try_clone().unwrap(), Path::new("big.m4a"), length, None);
        assert!(matches!(result, Err(CodecError::UnsupportedFormat(message)) if message.contains("2 GiB")));
    }
    assert_eq!(MAX_AAC_BYTES, 1 << 31);
}

/// Through the `Reader`: the stereo (planar) read of a 5.1 file whose unit 3 carries an in-band PCE with a matrix mix-down index that is not the stream's own layout
/// is refused with the typed `UnsupportedCodec` (the units in front of it are delivered), the interleaved read of the same file decodes to the end, and a PCE of the
/// stream's own layout is mixed.
#[test]
fn a_pce_that_is_not_the_streams_own_refuses_the_stereo_read_of_a_5_1_file_through_the_reader() {
    let dir = Dir::new("pce-refusal");
    let aus = six_channel_aus();
    let write = |name: &str, unit: Vec<u8>| {
        let mut changed = aus.clone();
        changed[3] = unit;
        dir.file(name, &adts(&changed, 5, 6))
    };
    let odd = write("odd.aac", with_pce(&aus[3], &[(false, 0)], 0, &[(true, 1), (true, 2)], 1, Some((1, 0))));
    let own = write("own.aac", with_pce(&aus[3], &[(false, 0), (true, 0)], 0, &[(true, 1)], 1, Some((3, 0))));
    let stereo = |path: &Path| {
        let mut reader = Reader::open(path, Some(32_000.0)).unwrap();
        reader.set_stereo_planar(32_000.0).unwrap();
        let (mut left, mut right) = (vec![0f32; 512], vec![0f32; 512]);
        let mut frames = 0;
        loop {
            match reader.read_stereo_planar(&mut left, &mut right) {
                Ok(0) => return Ok(frames),
                Ok(n) => frames += n,
                Err(e) => return Err((frames, e)),
            }
        }
    };
    match stereo(&odd) {
        Err((frames, CodecError::UnsupportedCodec { codec: "AAC", reason, .. })) => {
            assert_eq!(frames, 3 * 1024, "the three units in front of the PCE are delivered");
            assert!(reason.contains("in-band program config element"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(stereo(&own).unwrap(), 17 * 1024);
    // the interleaved read does not care
    for path in [&odd, &own] {
        let mut reader = Reader::open(path, None).unwrap();
        let mut buffer = vec![0f32; 512 * 6];
        let mut frames = 0;
        loop {
            match reader.read(&mut buffer).unwrap() {
                0 => break,
                n => frames += n,
            }
        }
        assert_eq!(frames, 17 * 1024);
    }
}

/// A file of `ftyp` and `count` atoms (or, for a CAF, chunks) of 8 (12) bytes of the given kind and nothing else.
fn chain(kind: &[u8; 4], count: usize, caf: bool) -> Vec<u8> {
    if caf {
        [&b"caff\0\x01\0\0\0\0\0\0"[..8], &[&kind[..], &[0; 8]].concat().repeat(count)].concat()
    } else {
        [&[0, 0, 0, 20][..], b"ftypM4A \0\0\0\0M4A ", &[&[0, 0, 0, 8][..], &kind[..]].concat().repeat(count)].concat()
    }
}

#[test]
fn walks_that_cost_a_system_call_per_atom_stop_at_their_limit() {
    let dir = Dir::new("walk-limits");
    let at_limit = MAX_SCANNED_BOXES;
    // the `.mov` refusal: one atom more than the limit is given up (the AAC container model decides), the limit itself is walked to the end
    for (count, given_up) in [(at_limit + 1, true), (at_limit - 2, false)] {
        let bytes = chain(b"free", count, false);
        let path = dir.file("walk.m4a", &bytes);
        let verdict = unsupported_mp4(&mut File::open(&path).unwrap(), &path, bytes.len() as u64);
        let message = "MP4 with more top-level atoms than are scanned";
        assert_eq!(matches!(verdict, Err(CodecError::UnsupportedFormat(m)) if m == message), given_up, "{count} atoms: {verdict:?}");
    }
    // the PCM CAF reader
    for (count, given_up) in [(at_limit + 1, true), (at_limit - 2, false)] {
        let bytes = chain(b"free", count, true);
        let path = dir.file("walk.caf", &bytes);
        let verdict = parse_caf(&mut File::open(&path).unwrap(), &path, bytes.len() as u64).map(|_| ());
        let message = "CAF with more chunks than are scanned";
        assert_eq!(matches!(verdict, Err(CodecError::UnsupportedFormat(m)) if m == message), given_up, "{count} chunks: {verdict:?}");
    }
    // the ALAC reader (over any seekable stream)
    for (count, given_up) in [(at_limit + 1, true), (at_limit - 2, false)] {
        let verdict = mp4::read_alac(&mut std::io::Cursor::new(chain(b"free", count, false)));
        let given_up_here = matches!(&verdict, Err(mp4::Mp4Error::Unsupported(m)) if m.contains("more top-level atoms"));
        assert_eq!(given_up_here, given_up, "{count} atoms: {:?}", verdict.map(|_| ()));
    }
}

#[test]
fn a_file_of_more_atoms_than_the_walks_look_at_still_opens_by_the_container_model() {
    // 100 000 `free` atoms in front of the movie atom: every walk behind the AAC sniffer gives up, the sniffer (limit 2^20) does not
    let tiny = fixture("tiny-44100-2.m4a");
    let ftyp_len = u32::from_be_bytes(tiny[..4].try_into().unwrap()) as usize;
    let dir = Dir::new("many-atoms");
    // (the chunk offsets no longer hold, which only a read notices)
    let padded = [&tiny[..ftyp_len], &[0, 0, 0, 8][..], b"free", &chain(b"free", 100_000, false)[20..], &tiny[ftyp_len..]].concat();
    let path = dir.file("many.m4a", &padded);
    let reader = Reader::open(&path, None).expect("the movie atom is behind 100,000 atoms and is found");
    assert_eq!(reader.source_format().channels_per_frame, 2);
}

#[test]
fn a_chain_of_id3_blocks_is_measured_through_a_window() {
    // tags of every size from 10 bytes to 300 KB, so that heads fall at both ends of the 64 KiB windows
    let dir = Dir::new("tag-window");
    let mut bytes = Vec::new();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..40 {
        state ^= state << 13; state ^= state >> 7; state ^= state << 17;
        let size = [0usize, 1, 117, 4_000, 65_526, 65_527, 65_536, 131_081, 300_000][(state % 9) as usize];
        bytes.extend_from_slice(&[b'I', b'D', b'3', 4, 0, 0, (size >> 21 & 0x7f) as u8, (size >> 14 & 0x7f) as u8, (size >> 7 & 0x7f) as u8, (size & 0x7f) as u8]);
        bytes.resize(bytes.len() + size, 0);
    }
    let tags = bytes.len() as u64;
    bytes.extend_from_slice(&[0xff; 64]);
    let path = dir.file("tags.bin", &bytes);
    let length = bytes.len() as u64;
    assert_eq!(leading_tag_length(&mut File::open(&path).unwrap(), length, &path).unwrap(), tags);
    // a tag that claims more than the file holds ends the chain beyond the file
    let cut = &bytes[..tags as usize - 100];
    let path = dir.file("cut.bin", cut);
    assert!(leading_tag_length(&mut File::open(&path).unwrap(), cut.len() as u64, &path).unwrap() >= cut.len() as u64);
}

/// The tiny MP4 (stereo, or mono with `v-m-`) damaged the way the file name says: `v-cut<N>` loses its last N bytes (the audio data is the last atom),
/// `v-over<i>` declares packet i as one byte above the decoder's limit (1024 per channel), `v-zero<i>[_<j>]` zeroes the access units i to j.
fn variant(name: &str) -> Vec<u8> {
    let spec = name.trim_start_matches("v-").trim_end_matches(".m4a");
    // `m-` marks the mono fixture, whose oversize limit is 1024 bytes
    let (spec, source, oversize) = match spec.strip_prefix("m-") {
        Some(rest) => (rest, MONO_M4A, 1025u32),
        None => (spec, STEREO_M4A, 2049),
    };
    let mut file = source.to_vec();
    let extents: Vec<(u64, u32)> = {
        let f = crate::portable::aac::decoder::AacFile::parse_with_hint(source.to_vec(), None).unwrap();
        (0..f.packet_count()).map(|i| f.packet_extent(i).unwrap()).collect()
    };
    if let Some(n) = spec.strip_prefix("cut") {
        file.truncate(file.len() - n.parse::<usize>().unwrap());
    } else if let Some(i) = spec.strip_prefix("over") {
        let table = file.windows(4).position(|w| w == b"stsz").unwrap() + 4 + 12;
        let at = table + 4 * i.parse::<usize>().unwrap();
        file[at..at + 4].copy_from_slice(&oversize.to_be_bytes());
    } else if let Some(range) = spec.strip_prefix("zero") {
        let (first, last) = range.split_once('_').map_or((range, range), |(a, b)| (a, b));
        for &(offset, size) in &extents[first.parse::<usize>().unwrap()..=last.parse::<usize>().unwrap()] {
            file[offset as usize..offset as usize + size as usize].fill(0);
        }
    } else {
        panic!("variant {name}");
    }
    file
}

/// (variant, scenario, status, frames before the failure or in total, calls, hash, failure) as AudioToolbox answered: packets beyond the end of the
/// file and oversize packets fail the call that pulls the batch holding them (a batch holds `floor(frames per call / 1024) + 1` packets), the 4th
/// access unit in a row that the decoder rejects fails with 'bada', fewer are skipped. The planar client of the mono fixture (a channel selection) loses
/// a failure that arrives after its inner 4096-frame request produced frames.
const VARIANTS: &[(&str, &str, &str, u64, u64, u64, &str)] = &[
    ("v-cut1.m4a", "ilv512s", "err", 7680, 16, 0x72ebf8f02cb85401, "native/ExtAudioFileRead/-50"),
    ("v-cut1.m4a", "ilv512z", "err", 7680, 16, 0x72ebf8f02cb85401, "native/ExtAudioFileRead/-50"),
    ("v-cut1.m4a", "ilv1025s", "err", 7175, 8, 0x0d3a759bbb875e3a, "native/ExtAudioFileRead/-50"),
    ("v-cut1.m4a", "ilv4096s", "err", 4096, 2, 0x95bce76f7008c5ca, "native/ExtAudioFileRead/-50"),
    ("v-cut1.m4a", "ilv1024s", "err", 6144, 7, 0x0219dc37a144de83, "native/ExtAudioFileRead/-50"),
    ("v-cut1.m4a", "pl512z", "err", 7680, 16, 0xeb817edd2b06cd81, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut1.m4a", "pl512s", "err", 7680, 16, 0xeb817edd2b06cd81, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut1.m4a", "pl4096s", "err", 4096, 2, 0x7835ff09c4e4a9c2, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut1.m4a", "pl1025s", "err", 7175, 8, 0x3a914e2429009522, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut1.m4a", "ilv7s", "err", 8127, 1162, 0xeed19399dedbf98e, "native/ExtAudioFileRead/-50"),
    ("v-cut1.m4a", "ilv333s", "err", 7992, 25, 0x53a029d4f4f4a015, "native/ExtAudioFileRead/-50"),
    ("v-cut1.m4a", "pl333s", "err", 7992, 25, 0x7817ab18b1342051, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut1.m4a", "ilv1s", "err", 8128, 8129, 0x898e682168fdd635, "native/ExtAudioFileRead/-50"),
    ("v-cut1.m4a", "pl1s", "err", 8128, 8129, 0x898e682168fdd635, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut100.m4a", "ilv512s", "err", 7680, 16, 0x72ebf8f02cb85401, "native/ExtAudioFileRead/-50"),
    ("v-cut100.m4a", "ilv512z", "err", 7680, 16, 0x72ebf8f02cb85401, "native/ExtAudioFileRead/-50"),
    ("v-cut100.m4a", "ilv1025s", "err", 7175, 8, 0x0d3a759bbb875e3a, "native/ExtAudioFileRead/-50"),
    ("v-cut100.m4a", "ilv4096s", "err", 4096, 2, 0x95bce76f7008c5ca, "native/ExtAudioFileRead/-50"),
    ("v-cut100.m4a", "ilv1024s", "err", 6144, 7, 0x0219dc37a144de83, "native/ExtAudioFileRead/-50"),
    ("v-cut100.m4a", "pl512z", "err", 7680, 16, 0xeb817edd2b06cd81, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut100.m4a", "pl512s", "err", 7680, 16, 0xeb817edd2b06cd81, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut100.m4a", "pl4096s", "err", 4096, 2, 0x7835ff09c4e4a9c2, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut100.m4a", "pl1025s", "err", 7175, 8, 0x3a914e2429009522, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut100.m4a", "ilv7s", "err", 8127, 1162, 0xeed19399dedbf98e, "native/ExtAudioFileRead/-50"),
    ("v-cut100.m4a", "ilv333s", "err", 7992, 25, 0x53a029d4f4f4a015, "native/ExtAudioFileRead/-50"),
    ("v-cut100.m4a", "pl333s", "err", 7992, 25, 0x7817ab18b1342051, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut100.m4a", "ilv1s", "err", 8128, 8129, 0x898e682168fdd635, "native/ExtAudioFileRead/-50"),
    ("v-cut100.m4a", "pl1s", "err", 8128, 8129, 0x898e682168fdd635, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut500.m4a", "ilv512s", "err", 6656, 14, 0x946a351a2ae386d5, "native/ExtAudioFileRead/-50"),
    ("v-cut500.m4a", "ilv512z", "err", 6656, 14, 0x946a351a2ae386d5, "native/ExtAudioFileRead/-50"),
    ("v-cut500.m4a", "ilv1025s", "err", 5125, 6, 0x1476266604e6dca0, "native/ExtAudioFileRead/-50"),
    ("v-cut500.m4a", "ilv4096s", "err", 4096, 2, 0x95bce76f7008c5ca, "native/ExtAudioFileRead/-50"),
    ("v-cut500.m4a", "ilv1024s", "err", 6144, 7, 0x0219dc37a144de83, "native/ExtAudioFileRead/-50"),
    ("v-cut500.m4a", "pl512z", "err", 6656, 14, 0x156503c677da1b75, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut500.m4a", "pl512s", "err", 6656, 14, 0x156503c677da1b75, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut500.m4a", "pl4096s", "err", 4096, 2, 0x7835ff09c4e4a9c2, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut500.m4a", "pl1025s", "err", 5125, 6, 0x825721e3c969ff88, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut500.m4a", "ilv7s", "err", 7098, 1015, 0xe0179c5aa2dd3dda, "native/ExtAudioFileRead/-50"),
    ("v-cut500.m4a", "ilv333s", "err", 6993, 22, 0x1e00256c5bb7122e, "native/ExtAudioFileRead/-50"),
    ("v-cut500.m4a", "pl333s", "err", 6993, 22, 0xe4027f9dc2c4e666, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut500.m4a", "ilv1s", "err", 7104, 7105, 0x8d2ab13675c56e75, "native/ExtAudioFileRead/-50"),
    ("v-cut500.m4a", "pl1s", "err", 7104, 7105, 0x8d2ab13675c56e75, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut900.m4a", "ilv512s", "err", 4608, 10, 0xe2cc0a120bb54a09, "native/ExtAudioFileRead/-50"),
    ("v-cut900.m4a", "ilv512z", "err", 4608, 10, 0xe2cc0a120bb54a09, "native/ExtAudioFileRead/-50"),
    ("v-cut900.m4a", "ilv1025s", "err", 3075, 4, 0x890fe5253d648639, "native/ExtAudioFileRead/-50"),
    ("v-cut900.m4a", "ilv4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-cut900.m4a", "ilv1024s", "err", 4096, 5, 0xa4aafe61e77159e2, "native/ExtAudioFileRead/-50"),
    ("v-cut900.m4a", "pl512z", "err", 4608, 10, 0x321e5d697a067981, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut900.m4a", "pl512s", "err", 4608, 10, 0x321e5d697a067981, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut900.m4a", "pl4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut900.m4a", "pl1025s", "err", 3075, 4, 0xc2aded7326849801, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut900.m4a", "ilv7s", "err", 5054, 723, 0x44505bd3691d3d26, "native/ExtAudioFileRead/-50"),
    ("v-cut900.m4a", "ilv333s", "err", 4995, 16, 0x25001993ead6b77b, "native/ExtAudioFileRead/-50"),
    ("v-cut900.m4a", "pl333s", "err", 4995, 16, 0xcae40a29afd68a03, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-cut900.m4a", "ilv1s", "err", 5056, 5057, 0x2f07de8a23e6004c, "native/ExtAudioFileRead/-50"),
    ("v-cut900.m4a", "pl1s", "err", 5056, 5057, 0x2f07de8a23e6004c, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over3.m4a", "ilv512s", "err", 512, 2, 0xe79d997f7d9b40c7, "native/ExtAudioFileRead/-50"),
    ("v-over3.m4a", "ilv512z", "err", 512, 2, 0xe79d997f7d9b40c7, "native/ExtAudioFileRead/-50"),
    ("v-over3.m4a", "ilv1025s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-over3.m4a", "ilv4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-over3.m4a", "ilv1024s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-over3.m4a", "pl512z", "err", 512, 2, 0xbcd34fc3fecc283f, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over3.m4a", "pl512s", "err", 512, 2, 0xbcd34fc3fecc283f, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over3.m4a", "pl4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over3.m4a", "pl1025s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over3.m4a", "ilv7s", "err", 959, 138, 0xe9a349061b7d10f0, "native/ExtAudioFileRead/-50"),
    ("v-over3.m4a", "ilv333s", "err", 666, 3, 0x52f2070b3da80ea5, "native/ExtAudioFileRead/-50"),
    ("v-over3.m4a", "pl333s", "err", 666, 3, 0x4feebccb7ccdfc71, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over3.m4a", "ilv1s", "err", 960, 961, 0xdc07ebe124713b4f, "native/ExtAudioFileRead/-50"),
    ("v-over3.m4a", "pl1s", "err", 960, 961, 0xdc07ebe124713b4f, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over9.m4a", "ilv512s", "err", 6656, 14, 0x946a351a2ae386d5, "native/ExtAudioFileRead/-50"),
    ("v-over9.m4a", "ilv512z", "err", 6656, 14, 0x946a351a2ae386d5, "native/ExtAudioFileRead/-50"),
    ("v-over9.m4a", "ilv1025s", "err", 5125, 6, 0x1476266604e6dca0, "native/ExtAudioFileRead/-50"),
    ("v-over9.m4a", "ilv4096s", "err", 4096, 2, 0x95bce76f7008c5ca, "native/ExtAudioFileRead/-50"),
    ("v-over9.m4a", "ilv1024s", "err", 6144, 7, 0x0219dc37a144de83, "native/ExtAudioFileRead/-50"),
    ("v-over9.m4a", "pl512z", "err", 6656, 14, 0x156503c677da1b75, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over9.m4a", "pl512s", "err", 6656, 14, 0x156503c677da1b75, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over9.m4a", "pl4096s", "err", 4096, 2, 0x7835ff09c4e4a9c2, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over9.m4a", "pl1025s", "err", 5125, 6, 0x825721e3c969ff88, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over9.m4a", "ilv7s", "err", 7098, 1015, 0xe0179c5aa2dd3dda, "native/ExtAudioFileRead/-50"),
    ("v-over9.m4a", "ilv333s", "err", 6993, 22, 0x1e00256c5bb7122e, "native/ExtAudioFileRead/-50"),
    ("v-over9.m4a", "pl333s", "err", 6993, 22, 0xe4027f9dc2c4e666, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over9.m4a", "ilv1s", "err", 7104, 7105, 0x8d2ab13675c56e75, "native/ExtAudioFileRead/-50"),
    ("v-over9.m4a", "pl1s", "err", 7104, 7105, 0x8d2ab13675c56e75, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over10.m4a", "ilv512s", "err", 7680, 16, 0x72ebf8f02cb85401, "native/ExtAudioFileRead/-50"),
    ("v-over10.m4a", "ilv512z", "err", 7680, 16, 0x72ebf8f02cb85401, "native/ExtAudioFileRead/-50"),
    ("v-over10.m4a", "ilv1025s", "err", 7175, 8, 0x0d3a759bbb875e3a, "native/ExtAudioFileRead/-50"),
    ("v-over10.m4a", "ilv4096s", "err", 4096, 2, 0x95bce76f7008c5ca, "native/ExtAudioFileRead/-50"),
    ("v-over10.m4a", "ilv1024s", "err", 6144, 7, 0x0219dc37a144de83, "native/ExtAudioFileRead/-50"),
    ("v-over10.m4a", "pl512z", "err", 7680, 16, 0xeb817edd2b06cd81, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over10.m4a", "pl512s", "err", 7680, 16, 0xeb817edd2b06cd81, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over10.m4a", "pl4096s", "err", 4096, 2, 0x7835ff09c4e4a9c2, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over10.m4a", "pl1025s", "err", 7175, 8, 0x3a914e2429009522, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over10.m4a", "ilv7s", "err", 8127, 1162, 0xeed19399dedbf98e, "native/ExtAudioFileRead/-50"),
    ("v-over10.m4a", "ilv333s", "err", 7992, 25, 0x53a029d4f4f4a015, "native/ExtAudioFileRead/-50"),
    ("v-over10.m4a", "pl333s", "err", 7992, 25, 0x7817ab18b1342051, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-over10.m4a", "ilv1s", "err", 8128, 8129, 0x898e682168fdd635, "native/ExtAudioFileRead/-50"),
    ("v-over10.m4a", "pl1s", "err", 8128, 8129, 0x898e682168fdd635, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-zero5.m4a", "ilv512s", "ok", 8128, 18, 0x0592d6e87129378b, ""),
    ("v-zero5.m4a", "ilv512z", "ok", 8128, 18, 0x6ec7a1b4c4f11c8b, ""),
    ("v-zero5.m4a", "ilv1025s", "ok", 8128, 10, 0x10f5a7b37ea27ec7, ""),
    ("v-zero5.m4a", "ilv4096s", "ok", 8128, 4, 0xe26590fa8da7b333, ""),
    ("v-zero5.m4a", "ilv1024s", "ok", 8128, 10, 0xec9d0b16270c98f7, ""),
    ("v-zero5.m4a", "pl512z", "ok", 8128, 18, 0x674e59a332d7949b, ""),
    ("v-zero5.m4a", "pl512s", "ok", 8128, 18, 0x76406d3cc700149b, ""),
    ("v-zero5.m4a", "pl4096s", "ok", 8128, 4, 0x5885025e35186513, ""),
    ("v-zero5.m4a", "pl1025s", "ok", 8128, 10, 0xe745979d8c145b9f, ""),
    ("v-zero5.m4a", "ilv7s", "ok", 8128, 1164, 0x2cbdd24d6aa4a048, ""),
    ("v-zero5.m4a", "ilv333s", "ok", 8128, 27, 0xa6b958433a550112, ""),
    ("v-zero5.m4a", "pl333s", "ok", 8128, 27, 0x1b16abccc67c3b6a, ""),
    ("v-zero5.m4a", "ilv1s", "ok", 8128, 8130, 0x9bda27169fee54ee, ""),
    ("v-zero5.m4a", "pl1s", "ok", 8128, 8130, 0x9bda27169fee54ee, ""),
    ("v-zero5_7.m4a", "ilv512s", "ok", 6080, 14, 0x2a1f6d480f349441, ""),
    ("v-zero5_7.m4a", "ilv512z", "ok", 6080, 14, 0xa14567d9f3e48741, ""),
    ("v-zero5_7.m4a", "ilv1025s", "ok", 6080, 8, 0x1079808a43d8a6c7, ""),
    ("v-zero5_7.m4a", "ilv4096s", "ok", 6080, 4, 0x4a68fdc919ac3781, ""),
    ("v-zero5_7.m4a", "ilv1024s", "ok", 6080, 8, 0xa080b7cdf12c06f9, ""),
    ("v-zero5_7.m4a", "pl512z", "ok", 6080, 14, 0x635d41228c771655, ""),
    ("v-zero5_7.m4a", "pl512s", "ok", 6080, 14, 0xe4d6adc824efc655, ""),
    ("v-zero5_7.m4a", "pl4096s", "ok", 6080, 4, 0x37cbd67a6679e509, ""),
    ("v-zero5_7.m4a", "pl1025s", "ok", 6080, 8, 0xea1dd395787341c7, ""),
    ("v-zero5_7.m4a", "ilv7s", "ok", 6080, 871, 0xeccd34f65ada0d60, ""),
    ("v-zero5_7.m4a", "ilv333s", "ok", 6080, 21, 0x8fe4b86dcb036972, ""),
    ("v-zero5_7.m4a", "pl333s", "ok", 6080, 21, 0xc01db265c41eef8a, ""),
    ("v-zero5_7.m4a", "ilv1s", "ok", 6080, 6082, 0x827f29e0c7ad7e2c, ""),
    ("v-zero5_7.m4a", "pl1s", "ok", 6080, 6082, 0x827f29e0c7ad7e2c, ""),
    ("v-zero4_7.m4a", "ilv512s", "err", 1536, 4, 0xfbd706fe883b4638, "native/ExtAudioFileRead/1650549857"),
    ("v-zero4_7.m4a", "ilv512z", "err", 1536, 4, 0xfbd706fe883b4638, "native/ExtAudioFileRead/1650549857"),
    ("v-zero4_7.m4a", "ilv1025s", "err", 1025, 2, 0x402741c5bc1fdba8, "native/ExtAudioFileRead/1650549857"),
    ("v-zero4_7.m4a", "ilv4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/1650549857"),
    ("v-zero4_7.m4a", "ilv1024s", "err", 1024, 2, 0xe468bd463dea701a, "native/ExtAudioFileRead/1650549857"),
    ("v-zero4_7.m4a", "pl512z", "err", 1536, 4, 0x7c728ef7d6efa430, "native/ExtAudioFileRead(planar stereo)/1650549857"),
    ("v-zero4_7.m4a", "pl512s", "err", 1536, 4, 0x7c728ef7d6efa430, "native/ExtAudioFileRead(planar stereo)/1650549857"),
    ("v-zero4_7.m4a", "pl4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/1650549857"),
    ("v-zero4_7.m4a", "pl1025s", "err", 1025, 2, 0xa46bd78a5252c33c, "native/ExtAudioFileRead(planar stereo)/1650549857"),
    ("v-zero4_7.m4a", "ilv7s", "err", 1981, 284, 0x2bc86b57b23aa9a2, "native/ExtAudioFileRead/1650549857"),
    ("v-zero4_7.m4a", "ilv333s", "err", 1665, 6, 0x1e90c17b63c8e8c3, "native/ExtAudioFileRead/1650549857"),
    ("v-zero4_7.m4a", "pl333s", "err", 1665, 6, 0x37267468d7ba6a87, "native/ExtAudioFileRead(planar stereo)/1650549857"),
    ("v-zero4_7.m4a", "ilv1s", "err", 1984, 1985, 0x505e0aada084c879, "native/ExtAudioFileRead/1650549857"),
    ("v-zero4_7.m4a", "pl1s", "err", 1984, 1985, 0x505e0aada084c879, "native/ExtAudioFileRead(planar stereo)/1650549857"),
    ("v-zero8_10.m4a", "ilv512s", "ok", 6080, 14, 0x953b6dcabc25ac95, ""),
    ("v-zero8_10.m4a", "ilv512z", "ok", 6080, 14, 0xa660f33104205f95, ""),
    ("v-zero8_10.m4a", "ilv1025s", "ok", 6080, 8, 0x7b91c30af48f8cab, ""),
    ("v-zero8_10.m4a", "ilv4096s", "ok", 6080, 4, 0x7a0a541764f635bd, ""),
    ("v-zero8_10.m4a", "ilv1024s", "ok", 6080, 8, 0xb12de203639f2e81, ""),
    ("v-zero8_10.m4a", "pl512z", "ok", 6080, 14, 0xe7056e406211ce7d, ""),
    ("v-zero8_10.m4a", "pl512s", "ok", 6080, 14, 0x3eb66b6a6851637d, ""),
    ("v-zero8_10.m4a", "pl4096s", "ok", 6080, 4, 0x8171ef427d518af9, ""),
    ("v-zero8_10.m4a", "pl1025s", "ok", 6080, 8, 0x2d1f98c43275b4a7, ""),
    ("v-zero8_10.m4a", "ilv7s", "ok", 6080, 871, 0xf31e00a4b839aaa8, ""),
    ("v-zero8_10.m4a", "ilv333s", "ok", 6080, 21, 0x7c70fdc9bfd36d5a, ""),
    ("v-zero8_10.m4a", "pl333s", "ok", 6080, 21, 0x18599e1a29c1e6f2, ""),
    ("v-zero8_10.m4a", "ilv1s", "ok", 6080, 6082, 0x8b935a95965d1abc, ""),
    ("v-zero8_10.m4a", "pl1s", "ok", 6080, 6082, 0x8b935a95965d1abc, ""),
    ("v-m-cut50.m4a", "ilv512s", "err", 8704, 18, 0x9a374304b1eb5e86, "native/ExtAudioFileRead/-50"),
    ("v-m-cut50.m4a", "ilv512z", "err", 8704, 18, 0x9a374304b1eb5e86, "native/ExtAudioFileRead/-50"),
    ("v-m-cut50.m4a", "ilv1025s", "err", 7175, 8, 0xe22eeb199c914365, "native/ExtAudioFileRead/-50"),
    ("v-m-cut50.m4a", "ilv4096s", "err", 4096, 2, 0x7f907d7644f052d3, "native/ExtAudioFileRead/-50"),
    ("v-m-cut50.m4a", "ilv1024s", "err", 8192, 9, 0xf71eb3fd2f208c0d, "native/ExtAudioFileRead/-50"),
    ("v-m-cut50.m4a", "pl512z", "ok", 9152, 20, 0x62c1b518722fc4a4, ""),
    ("v-m-cut50.m4a", "pl512s", "ok", 9152, 20, 0x9e75f414e9083ca4, ""),
    ("v-m-cut50.m4a", "pl4096s", "ok", 6080, 4, 0x8ef125b2e6bb0d8c, ""),
    ("v-m-cut50.m4a", "pl1025s", "ok", 8128, 10, 0xc1fc735e54163138, ""),
    ("v-m-cut50.m4a", "ilv7s", "err", 9149, 1308, 0x09f30fbbccda960c, "native/ExtAudioFileRead/-50"),
    ("v-m-cut50.m4a", "ilv333s", "err", 8991, 28, 0xc52d9b28e4c7071b, "native/ExtAudioFileRead/-50"),
    ("v-m-cut50.m4a", "pl333s", "ok", 9152, 30, 0xda789cedcb4e7a36, ""),
    ("v-m-cut50.m4a", "ilv1s", "err", 9152, 9153, 0xd7fc667a7bb1f2a1, "native/ExtAudioFileRead/-50"),
    ("v-m-cut50.m4a", "pl1s", "err", 9152, 9153, 0x7b9e17320e5fe66d, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-cut300.m4a", "ilv512s", "err", 6656, 14, 0x31dc86098e555ce0, "native/ExtAudioFileRead/-50"),
    ("v-m-cut300.m4a", "ilv512z", "err", 6656, 14, 0x31dc86098e555ce0, "native/ExtAudioFileRead/-50"),
    ("v-m-cut300.m4a", "ilv1025s", "err", 5125, 6, 0x86b9cc0172986f26, "native/ExtAudioFileRead/-50"),
    ("v-m-cut300.m4a", "ilv4096s", "err", 4096, 2, 0x7f907d7644f052d3, "native/ExtAudioFileRead/-50"),
    ("v-m-cut300.m4a", "ilv1024s", "err", 6144, 7, 0xa8202a16398a67ca, "native/ExtAudioFileRead/-50"),
    ("v-m-cut300.m4a", "pl512z", "err", 6656, 14, 0xaa0c69346351942b, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-cut300.m4a", "pl512s", "err", 6656, 14, 0xaa0c69346351942b, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-cut300.m4a", "pl4096s", "ok", 6080, 4, 0x8ef125b2e6bb0d8c, ""),
    ("v-m-cut300.m4a", "pl1025s", "err", 5125, 6, 0x47ae8f302c50f0ac, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-cut300.m4a", "ilv7s", "err", 7098, 1015, 0x9a115256b319cd00, "native/ExtAudioFileRead/-50"),
    ("v-m-cut300.m4a", "ilv333s", "err", 6993, 22, 0x20abcc18505b3c7d, "native/ExtAudioFileRead/-50"),
    ("v-m-cut300.m4a", "pl333s", "err", 6993, 22, 0xc81dc7891ef12863, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-cut300.m4a", "ilv1s", "err", 7104, 7105, 0xc3c50a3882381f2a, "native/ExtAudioFileRead/-50"),
    ("v-m-cut300.m4a", "pl1s", "err", 7104, 7105, 0x29ddcb1331a96c75, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over2.m4a", "ilv512s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over2.m4a", "ilv512z", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over2.m4a", "ilv1025s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over2.m4a", "ilv4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over2.m4a", "ilv1024s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over2.m4a", "pl512z", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over2.m4a", "pl512s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over2.m4a", "pl4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over2.m4a", "pl1025s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over2.m4a", "ilv7s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over2.m4a", "ilv333s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over2.m4a", "pl333s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over2.m4a", "ilv1s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over2.m4a", "pl1s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over5.m4a", "ilv512s", "err", 2560, 6, 0x73a15c3c5f4efeb3, "native/ExtAudioFileRead/-50"),
    ("v-m-over5.m4a", "ilv512z", "err", 2560, 6, 0x73a15c3c5f4efeb3, "native/ExtAudioFileRead/-50"),
    ("v-m-over5.m4a", "ilv1025s", "err", 1025, 2, 0xce8b78dfe0df5d81, "native/ExtAudioFileRead/-50"),
    ("v-m-over5.m4a", "ilv4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/-50"),
    ("v-m-over5.m4a", "ilv1024s", "err", 2048, 3, 0x1ab101f610ecad67, "native/ExtAudioFileRead/-50"),
    ("v-m-over5.m4a", "pl512z", "err", 2560, 6, 0x25ff03837a6d550f, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over5.m4a", "pl512s", "err", 2560, 6, 0x25ff03837a6d550f, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over5.m4a", "pl4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over5.m4a", "pl1025s", "err", 1025, 2, 0xbadf3e03c9e837ec, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over5.m4a", "ilv7s", "err", 3003, 430, 0x3f65f5a37d0ed9f0, "native/ExtAudioFileRead/-50"),
    ("v-m-over5.m4a", "ilv333s", "err", 2997, 10, 0xc3c143283634775b, "native/ExtAudioFileRead/-50"),
    ("v-m-over5.m4a", "pl333s", "err", 2997, 10, 0xf48b62152832889b, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over5.m4a", "ilv1s", "err", 3008, 3009, 0xf8e0d405c97cae6b, "native/ExtAudioFileRead/-50"),
    ("v-m-over5.m4a", "pl1s", "err", 3008, 3009, 0xd6855c7af7eb4c65, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-over11.m4a", "ilv512s", "err", 8704, 18, 0x9a374304b1eb5e86, "native/ExtAudioFileRead/-50"),
    ("v-m-over11.m4a", "ilv512z", "err", 8704, 18, 0x9a374304b1eb5e86, "native/ExtAudioFileRead/-50"),
    ("v-m-over11.m4a", "ilv1025s", "err", 7175, 8, 0xe22eeb199c914365, "native/ExtAudioFileRead/-50"),
    ("v-m-over11.m4a", "ilv4096s", "err", 4096, 2, 0x7f907d7644f052d3, "native/ExtAudioFileRead/-50"),
    ("v-m-over11.m4a", "ilv1024s", "err", 8192, 9, 0xf71eb3fd2f208c0d, "native/ExtAudioFileRead/-50"),
    ("v-m-over11.m4a", "pl512z", "ok", 9152, 20, 0x62c1b518722fc4a4, ""),
    ("v-m-over11.m4a", "pl512s", "ok", 9152, 20, 0x9e75f414e9083ca4, ""),
    ("v-m-over11.m4a", "pl4096s", "ok", 6080, 4, 0x8ef125b2e6bb0d8c, ""),
    ("v-m-over11.m4a", "pl1025s", "ok", 8128, 10, 0xc1fc735e54163138, ""),
    ("v-m-over11.m4a", "ilv7s", "err", 9149, 1308, 0x09f30fbbccda960c, "native/ExtAudioFileRead/-50"),
    ("v-m-over11.m4a", "ilv333s", "err", 8991, 28, 0xc52d9b28e4c7071b, "native/ExtAudioFileRead/-50"),
    ("v-m-over11.m4a", "pl333s", "ok", 9152, 30, 0xda789cedcb4e7a36, ""),
    ("v-m-over11.m4a", "ilv1s", "err", 9152, 9153, 0xd7fc667a7bb1f2a1, "native/ExtAudioFileRead/-50"),
    ("v-m-over11.m4a", "pl1s", "err", 9152, 9153, 0x7b9e17320e5fe66d, "native/ExtAudioFileRead(planar stereo)/-50"),
    ("v-m-zero4_7.m4a", "ilv512s", "err", 1536, 4, 0xbb9ba53b5a610869, "native/ExtAudioFileRead/1650549857"),
    ("v-m-zero4_7.m4a", "ilv512z", "err", 1536, 4, 0xbb9ba53b5a610869, "native/ExtAudioFileRead/1650549857"),
    ("v-m-zero4_7.m4a", "ilv1025s", "err", 1025, 2, 0xce8b78dfe0df5d81, "native/ExtAudioFileRead/1650549857"),
    ("v-m-zero4_7.m4a", "ilv4096s", "err", 0, 1, 0xcbf29ce484222325, "native/ExtAudioFileRead/1650549857"),
    ("v-m-zero4_7.m4a", "ilv1024s", "err", 1024, 2, 0xc5fec21f13e18229, "native/ExtAudioFileRead/1650549857"),
    ("v-m-zero4_7.m4a", "pl512z", "ok", 6080, 14, 0x176462c54d875cd0, ""),
    ("v-m-zero4_7.m4a", "pl512s", "ok", 6080, 14, 0x420a180152ed35d0, ""),
    ("v-m-zero4_7.m4a", "pl4096s", "ok", 6080, 4, 0x9bb6c231ad6b4cd8, ""),
    ("v-m-zero4_7.m4a", "pl1025s", "ok", 6080, 8, 0xc86032588316a03a, ""),
    ("v-m-zero4_7.m4a", "ilv7s", "err", 1981, 284, 0x086dc3c019bed0b7, "native/ExtAudioFileRead/1650549857"),
    ("v-m-zero4_7.m4a", "ilv333s", "err", 1665, 6, 0x90e159a940ee85a5, "native/ExtAudioFileRead/1650549857"),
    ("v-m-zero4_7.m4a", "pl333s", "ok", 6080, 21, 0x89a349c97c4629a7, ""),
    ("v-m-zero4_7.m4a", "ilv1s", "err", 1984, 1985, 0xe9eb34681d808c22, "native/ExtAudioFileRead/1650549857"),
    ("v-m-zero4_7.m4a", "pl1s", "err", 1984, 1985, 0xe33b6e3008743b3d, "native/ExtAudioFileRead(planar stereo)/1650549857"),
    ("v-m-zero9_10.m4a", "ilv512s", "ok", 8128, 18, 0x7ab4afb2914c024d, ""),
    ("v-m-zero9_10.m4a", "ilv512z", "ok", 8128, 18, 0xc5ecb26e8af148cd, ""),
    ("v-m-zero9_10.m4a", "ilv1025s", "ok", 8128, 10, 0x6aaaa4d4665e8741, ""),
    ("v-m-zero9_10.m4a", "ilv4096s", "ok", 8128, 4, 0x00c403229a72c985, ""),
    ("v-m-zero9_10.m4a", "ilv1024s", "ok", 8128, 10, 0x5033e01563409fb1, ""),
    ("v-m-zero9_10.m4a", "pl512z", "ok", 8128, 18, 0x656884985138a974, ""),
    ("v-m-zero9_10.m4a", "pl512s", "ok", 8128, 18, 0x5e123bba80dc8a74, ""),
    ("v-m-zero9_10.m4a", "pl4096s", "ok", 8128, 4, 0xfdb566e2f2b79820, ""),
    ("v-m-zero9_10.m4a", "pl1025s", "ok", 8128, 10, 0xd2d23c72cc27d7b8, ""),
    ("v-m-zero9_10.m4a", "ilv7s", "ok", 8128, 1164, 0xffbba7ba24deb5da, ""),
    ("v-m-zero9_10.m4a", "ilv333s", "ok", 8128, 27, 0x7869e270d1e36b69, ""),
    ("v-m-zero9_10.m4a", "pl333s", "ok", 8128, 27, 0x05c2b5454ea1a8f1, ""),
    ("v-m-zero9_10.m4a", "ilv1s", "ok", 8128, 8130, 0x0dce34a48db7ac28, ""),
    ("v-m-zero9_10.m4a", "pl1s", "ok", 8128, 8130, 0x32cc87b199d6c661, ""),
];

#[test]
fn damaged_files_fail_when_the_converter_pulls_the_bad_packet() {
    let dir = Dir::new("variants");
    let mut refused = 0;
    for &(name, scn, status, frames, calls, hash, failure) in VARIANTS {
        let path = dir.file(name, &variant(name));
        let got = scenario(&path, scn);
        if got.3 == "refused" {
            // a planar read of a mono stream whose failure falls inside the priming skip: the one outcome that is refused instead of reproduced
            assert!(scn.starts_with("pl") && name.starts_with("v-m-"), "{name} {scn}");
            refused += 1;
            continue;
        }
        assert_eq!((got.0, got.1, got.2, got.3.as_str()), (frames, calls, hash, failure), "{name} {scn}");
        assert_eq!(status == "err", !got.3.is_empty());
    }
    assert!(refused < VARIANTS.len() / 10, "{refused} refused rows");
}
