//! Builders of small containers for the unit tests: an MP4 around given access units, an ADTS stream, and the generated fixtures (a 0.2 s two-channel
//! tone and a 0.2 s mono tone, AAC-LC, 96 / 48 kbit/s, written by the macOS AudioToolbox encoder; the LOAS stream was written by FFmpeg's AAC encoder).

pub const STEREO_M4A: &[u8] = include_bytes!("../../../../tests/fixtures/aac-reader/tiny-44100-2.m4a");
pub const MONO_M4A: &[u8] = include_bytes!("../../../../tests/fixtures/aac-reader/tiny-48000-1.m4a");
pub const STEREO_ADTS: &[u8] = include_bytes!("../../../../tests/fixtures/aac-reader/tiny-44100-2.adts");
pub const STEREO_CAF: &[u8] = include_bytes!("../../../../tests/fixtures/aac-reader/tiny-44100-2.caf");
pub const STEREO_LOAS: &[u8] = include_bytes!("../../../../tests/fixtures/aac-reader/tiny-44100-2.loas");

pub fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

pub fn full(kind: &[u8; 4], version_flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = version_flags.to_be_bytes().to_vec();
    b.extend_from_slice(body);
    atom(kind, &b)
}

fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag, 0x80, 0x80, 0x80, body.len() as u8];
    out.extend_from_slice(body);
    out
}

/// The `esds` atom of an AAC track with the given AudioSpecificConfig.
pub fn esds(asc: &[u8]) -> Vec<u8> {
    let mut decoder = vec![0x40, 0x15, 0, 0, 0];
    decoder.extend_from_slice(&[0; 8]);
    decoder.extend(descriptor(5, asc));
    let mut es = vec![0, 1, 0];
    es.extend(descriptor(4, &decoder));
    es.extend(descriptor(6, &[2]));
    full(b"esds", 0, &descriptor(3, &es))
}

/// Sample table parts a test may corrupt.
pub struct Mp4Parts {
    pub sizes: Vec<u32>,
    /// `stsz` as a constant size (`sample_size`, count) instead of the table
    pub constant: Option<(u32, u32)>,
    pub asc: Vec<u8>,
    pub entry: [u8; 4],
}

/// An MP4 (`ftyp`, `moov` > `trak` > `mdia` > `minf` > `stbl`, one chunk in `mdat`) holding `aus` for a stereo 44.1 kHz AAC-LC track.
pub fn mp4(aus: &[&[u8]]) -> Vec<u8> {
    let parts = Mp4Parts { sizes: aus.iter().map(|a| a.len() as u32).collect(), constant: None, asc: vec![0x12, 0x10], entry: *b"mp4a" };
    mp4_with(aus, &parts)
}

pub fn mp4_with(aus: &[&[u8]], p: &Mp4Parts) -> Vec<u8> {
    let payload: Vec<u8> = aus.iter().flat_map(|a| a.iter().copied()).collect();
    let build = |chunk_offset: u32| {
        let mut entry = vec![0u8; 6];
        entry.extend_from_slice(&1u16.to_be_bytes());
        entry.extend_from_slice(&[0; 8]);
        entry.extend_from_slice(&2u16.to_be_bytes());
        entry.extend_from_slice(&16u16.to_be_bytes());
        entry.extend_from_slice(&[0; 4]);
        entry.extend_from_slice(&(44_100u32 << 16).to_be_bytes());
        entry.extend(esds(&p.asc));
        let stsd = full(b"stsd", 0, &[&1u32.to_be_bytes()[..], &atom(&p.entry, &entry)].concat());
        let stsz = match p.constant {
            Some((size, count)) => full(b"stsz", 0, &[size.to_be_bytes(), count.to_be_bytes()].concat()),
            None => {
                let mut b = [0u32.to_be_bytes(), (p.sizes.len() as u32).to_be_bytes()].concat();
                for s in &p.sizes {
                    b.extend_from_slice(&s.to_be_bytes());
                }
                full(b"stsz", 0, &b)
            }
        };
        let stsc = full(b"stsc", 0, &[1u32.to_be_bytes(), 1u32.to_be_bytes(), (p.sizes.len().max(1) as u32).to_be_bytes(), 1u32.to_be_bytes()].concat());
        let stco = full(b"stco", 0, &[1u32.to_be_bytes(), chunk_offset.to_be_bytes()].concat());
        let stbl = atom(b"stbl", &[stsd, stsz, stsc, stco].concat());
        let minf = atom(b"minf", &stbl);
        let mdhd = full(b"mdhd", 0, &[0u32.to_be_bytes(), 0u32.to_be_bytes(), 44_100u32.to_be_bytes(), 0u32.to_be_bytes(), 0x55c4_0000u32.to_be_bytes()].concat());
        let hdlr = full(b"hdlr", 0, &[&[0u8; 4][..], b"soun", &[0u8; 13]].concat());
        let mdia = atom(b"mdia", &[mdhd, hdlr, minf].concat());
        atom(b"moov", &atom(b"trak", &mdia))
    };
    let ftyp = atom(b"ftyp", b"M4A \0\0\0\0M4A mp42isom");
    let moov_len = build(0).len();
    let offset = (ftyp.len() + moov_len + 8) as u32;
    let mut out = ftyp;
    out.extend(build(offset));
    out.extend(atom(b"mdat", &payload));
    out
}

/// The access units of an opened file.
pub fn access_units(file: &[u8]) -> Vec<Vec<u8>> {
    let f = crate::portable::aac::decoder::AacFile::parse_with_hint(file.to_vec(), None).expect("fixture opens");
    (0..f.packet_count()).map(|i| f.packet(i).expect("packet").to_vec()).collect()
}

/// An ADTS stream (MPEG-4, LC, no CRC) of `aus` with the given sampling-frequency index and channel configuration.
pub fn adts(aus: &[Vec<u8>], sfi: u8, channel_config: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for au in aus {
        let len = au.len() + 7;
        out.extend_from_slice(&[
            0xff,
            0xf1,
            (1 << 6) | (sfi << 2) | (channel_config >> 2),
            ((channel_config & 3) << 6) | (len >> 11) as u8,
            (len >> 3) as u8,
            (((len & 7) << 5) as u8) | 0x1f,
            0xfc,
        ]);
        out.extend_from_slice(au);
    }
    out
}

/// The stereo CAF fixture with its `desc` channel count and `chan` layout tag replaced and, optionally, its `kuki` chunk removed.
pub fn caf_variant(drop_kuki: bool, desc_channels: u32, layout_tag: u32) -> Vec<u8> {
    let mut out = STEREO_CAF[..8].to_vec();
    let mut pos = 8;
    while pos + 12 <= STEREO_CAF.len() {
        let kind = &STEREO_CAF[pos..pos + 4];
        let size = i64::from_be_bytes(STEREO_CAF[pos + 4..pos + 12].try_into().unwrap()) as usize;
        let mut body = STEREO_CAF[pos + 12..pos + 12 + size].to_vec();
        match kind {
            b"kuki" if drop_kuki => {
                pos += 12 + size;
                continue;
            }
            b"desc" => body[24..28].copy_from_slice(&desc_channels.to_be_bytes()),
            b"chan" => body[0..4].copy_from_slice(&layout_tag.to_be_bytes()),
            _ => {}
        }
        out.extend_from_slice(kind);
        out.extend_from_slice(&(body.len() as i64).to_be_bytes());
        out.extend_from_slice(&body);
        pos += 12 + size;
    }
    out
}

/// The chunks of a CAF file as (type, body); the last one with size -1 runs to the end of the file.
pub fn caf_chunks(file: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 8;
    while pos + 12 <= file.len() {
        let size = i64::from_be_bytes(file[pos + 4..pos + 12].try_into().unwrap());
        let end = if size < 0 { file.len() } else { pos + 12 + size as usize };
        out.push((file[pos..pos + 4].try_into().unwrap(), file[pos + 12..end].to_vec()));
        pos = end;
    }
    out
}

/// A CAF file (version 1) from chunks; the sizes are the bodies' lengths.
pub fn caf_join(chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = b"caff\x00\x01\x00\x00".to_vec();
    for (kind, body) in chunks {
        out.extend_from_slice(kind);
        out.extend_from_slice(&(body.len() as i64).to_be_bytes());
        out.extend_from_slice(body);
    }
    out
}

/// The stereo CAF fixture with `edit` applied to its chunk list.
pub fn caf_edited(edit: impl FnOnce(&mut Vec<([u8; 4], Vec<u8>)>)) -> Vec<u8> {
    let mut chunks = caf_chunks(STEREO_CAF);
    edit(&mut chunks);
    caf_join(&chunks)
}

/// `au` with a `fill_element` of `payload_bits` (a string of `0` / `1`, whole bytes) in front of its first element.
pub fn with_fill_element(au: &[u8], payload_bits: &str) -> Vec<u8> {
    assert!(payload_bits.len() % 8 == 0 && (8..=112).contains(&payload_bits.len()));
    let mut bits = format!("110{:04b}{payload_bits}", payload_bits.len() / 8);
    bits.extend(au.iter().map(|b| format!("{b:08b}")));
    bits.extend(std::iter::repeat_n('0', (8 - bits.len() % 8) % 8));
    (0..bits.len()).step_by(8).map(|i| u8::from_str_radix(&bits[i..i + 8], 2).unwrap()).collect()
}

/// The access units of a 5.1 stream (AAC-LC, 32 kHz, `channelConfiguration` 6, 17 units, generated audio): `fresh/acc_32000_6ch_chirp` of the decode fixture pack.
pub fn six_channel_aus() -> Vec<Vec<u8>> {
    let pack = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aac-decode/pack-0.bin")).expect("the decode fixture pack");
    let file = &pack[469_076..469_076 + 7_463];
    assert_eq!(&file[4..8], b"ftyp", "the 5.1 fixture is where the manifest says");
    let aus = access_units(file);
    assert_eq!(aus.len(), 17);
    aus
}

/// `au` with an in-band program config element in front of its first element: object type 1, sampling index 5, the `front` and `back` element lists
/// ((is_cpe, tag) each), `side` side elements, `lfe` LFEs, and `matrix` = (matrix_mixdown_idx, pseudo_surround_enable) if it has one.
pub fn with_pce(au: &[u8], front: &[(bool, u8)], side: usize, back: &[(bool, u8)], lfe: usize, matrix: Option<(u8, u8)>) -> Vec<u8> {
    let mut bits = format!("101{:04b}{:02b}{:04b}{:04b}{:04b}{:04b}{:02b}{:03b}{:04b}00", 0, 1, 5, front.len(), side, back.len(), lfe, 0, 0);
    match matrix {
        Some((index, pseudo)) => bits.push_str(&format!("1{index:02b}{pseudo}")),
        None => bits.push('0'),
    }
    for (is_cpe, tag) in front {
        bits.push_str(&format!("{}{tag:04b}", u8::from(*is_cpe)));
    }
    bits.push_str(&"00000".repeat(side));
    for (is_cpe, tag) in back {
        bits.push_str(&format!("{}{tag:04b}", u8::from(*is_cpe)));
    }
    bits.push_str(&"0000".repeat(lfe));
    // byte alignment (counted from the start of the access unit, which is where this starts), then no comment
    bits.extend(std::iter::repeat_n('0', (8 - bits.len() % 8) % 8));
    bits.push_str("00000000");
    bits.extend(au.iter().map(|b| format!("{b:08b}")));
    bits.extend(std::iter::repeat_n('0', (8 - bits.len() % 8) % 8));
    (0..bits.len()).step_by(8).map(|i| u8::from_str_radix(&bits[i..i + 8], 2).unwrap()).collect()
}
