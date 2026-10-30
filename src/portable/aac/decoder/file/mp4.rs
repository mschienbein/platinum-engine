//! ISO base media / QuickTime (MP4, M4A, M4B, 3GP, 3G2, MOV, fragmented MP4) reader exactly as far as `ExtAudioFileOpenURL` /
//! `AudioFileReadPacketData` observably use it.
//!
//! What Apple ignores (verified): `stts`, `ctts`, `mdhd`/`tkhd` durations, the sample-entry channel count and rate, `chan`, `btrt`,
//! `sbgp`, sample-group contents other than the `roll` description, `free`/`skip`/`wide`/unknown atoms, `moov`/`mdat` order.

use crate::portable::aac::decoder::file::asc::{parse_asc, Asc};
use crate::portable::aac::decoder::file::error::{OpenError, Unspecified};

pub(crate) fn be16(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
pub(crate) fn be32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(o..o + 4)?.try_into().ok()?))
}
pub(crate) fn be64(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_be_bytes(d.get(o..o + 8)?.try_into().ok()?))
}

/// One parsed box: `body` is the byte range after the 8/16-byte header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Atom {
    pub typ: [u8; 4],
    pub start: usize,
    pub body_start: usize,
    pub end: usize,
    /// where the declared size ends (unclamped by the parent, bounded by the data)
    pub declared_end: usize,
    /// the declared size ran past the end of the data: clamped to it
    pub clamped: bool,
    /// the declared size was 0 ("to the end of the parent")
    pub to_end: bool,
    /// the size came from the 64-bit field behind a size of 1
    pub wide: bool,
}

pub(crate) struct Atoms<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
    /// inside another atom: a size field of 0 is no atom (at the top level of the file it means "to the end of the file")
    nested: bool,
}

/// The atoms that start inside `start..end` of `data` (`end` is the end of the parent, clamped to `data`).  An atom whose declared size runs past
/// the end of its PARENT is cut off there, whatever follows the parent (macOS 27.2: a `mdia` that declares more than its `trak` holds does not read
/// the siblings behind the `trak`, and an iTunSMPB `data` that runs past its item is as good as cut off); this also bounds every scan of the
/// children of an atom by the atom itself, so the walks of the children of one atom cost a few visits per byte of it (the one scan that is not bounded by
/// its parent, the first child of a `udta`, has an allowance of its own: see `find_itunsmpb`).
///
/// A size field of 0 inside an atom ends the scan there (macOS 27.2: a child of size 0 is lost with the siblings behind it, whichever atom it is:
/// a `hdlr` of size 0 leaves no sound track, a `minf` of size 0 no sample table); the sizes 2..=7 end it too, as they are no atoms.
pub(crate) fn atoms(data: &[u8], start: usize, end: usize) -> Atoms<'_> {
    Atoms { data, pos: start, end: end.min(data.len()), nested: true }
}

/// The top-level atoms of a file.
pub(crate) fn top_atoms(data: &[u8]) -> Atoms<'_> {
    Atoms { data, pos: 0, end: data.len(), nested: false }
}

/// Whether the scan of the children of `a` reaches its end without meeting a size field that is no atom (0 or 2..=7).
fn children_ok(data: &[u8], a: &Atom) -> bool {
    let mut scan = atoms(data, a.body_start, a.end);
    for _ in &mut scan {}
    scan.pos + 8 > scan.end
        || match be32(data, scan.pos) {
            Some(0 | 2..=7) => false,
            Some(1) => be64(data, scan.pos + 8).is_some_and(|wide| (16..1 << 63).contains(&wide)),
            _ => true,
        }
}

impl Iterator for Atoms<'_> {
    type Item = Atom;
    fn next(&mut self) -> Option<Atom> {
        if self.pos + 8 > self.end {
            return None;
        }
        let size32 = be32(self.data, self.pos)? as u64;
        let typ: [u8; 4] = self.data[self.pos + 4..self.pos + 8].try_into().ok()?;
        let (hdr, size, to_end) = match size32 {
            0 if self.nested => return None,
            0 => (8usize, (self.end - self.pos) as u64, true),
            1 => (16usize, be64(self.data, self.pos + 8)?, false),
            n => (8usize, n, false),
        };
        // a 64-bit size with the top bit set is negative to AudioToolbox: no atom
        if size < hdr as u64 || size >= 1 << 63 {
            return None;
        }
        let start = self.pos;
        let clamped = size > (self.end - start) as u64;
        let end = if clamped { self.end } else { start + size as usize };
        let declared_end = if clamped { start.saturating_add(usize::try_from(size).unwrap_or(usize::MAX)).min(self.data.len()) } else { end };
        self.pos = end;
        Some(Atom { typ, start, body_start: start + hdr, end, declared_end, clamped, to_end, wide: size32 == 1 })
    }
}

fn find_child(data: &[u8], a: &Atom, typ: &[u8; 4]) -> Option<Atom> {
    atoms(data, a.body_start, a.end).find(|c| &c.typ == typ)
}

/// A table of the sample table (`stsz`, `stz2`, `stsc`, `stco`, `co64`) as AudioToolbox finds it (macOS 27.2): what bounds a table is the end of its TRACK, not the
/// end of the `stbl` (or `minf`, `mdia`) that holds it: shortening the size field of the `stbl`, `minf` or `mdia` by up to the size of the last table leaves all of
/// it (as long as the header of that table still starts inside the `stbl`: one byte more and the table is gone and the open fails), shortening that of the `trak`
/// cuts the table off.
fn find_table(data: &[u8], stbl: &Atom, trak_end: usize, typ: &[u8; 4]) -> Option<Atom> {
    atoms(data, stbl.body_start, trak_end).take_while(|c| c.start + 8 <= stbl.declared_end).find(|c| &c.typ == typ)
}

/// `AudioFileTypeID` as reported by Apple (`kAudioFilePropertyFileFormat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    /// `'m4af'`
    M4a,
    /// `'m4bf'`
    M4b,
    /// `'mp4f'`
    Mp4,
    /// `'3gpp'`
    ThreeGpp,
    /// `'3gp2'`
    ThreeGpp2,
    /// `'MooV'` (QuickTime)
    Mov,
    /// `'caff'`
    Caf,
    /// `'adts'`
    Adts,
    /// `'loas'`
    Loas,
}

/// Greatest `ftyp` size AudioToolbox recognises plus one (macOS 27.2: a box of 20 to 99 bytes, as far as the file holds it, is an MP4 file; from 100
/// bytes the file is `'typ?'`).  It bounds the brand list at 19 brands.
pub(crate) const FTYP_LIMIT: usize = 100;

/// File type from the `ftyp` brands (recorded on macOS 27.2): the compatible brands are scanned, byte for byte (`M4A\0` and `m4a ` are no brands)
/// with the priority 3g2a > 3gp4..9 > M4A/M4V > M4B; the major brand only counts when it is `M4A `, `M4B ` or `M4V `; major `qt  ` is QuickTime;
/// everything else is `mp4f`.  The minor version does not matter.
fn file_type_from_brands<'a>(major: &[u8], compat: impl Iterator<Item = &'a [u8]>) -> FileType {
    if major == b"qt  " {
        return FileType::Mov;
    }
    let mut best = (0u8, FileType::Mp4);
    let mut consider = |b: &[u8]| {
        let cand = match b {
            b"3g2a" => (5, FileType::ThreeGpp2),
            b"3gp4" | b"3gp5" | b"3gp6" | b"3gp7" | b"3gp8" | b"3gp9" => (4, FileType::ThreeGpp),
            b"M4A " | b"M4V " => (3, FileType::M4a),
            b"M4B " => (2, FileType::M4b),
            _ => return,
        };
        if cand.0 > best.0 {
            best = cand;
        }
    };
    if major == b"M4A " || major == b"M4B " || major == b"M4V " {
        consider(major);
    }
    compat.for_each(consider);
    best.1
}

impl FileType {
    /// Whether the `iTunSMPB` tag (priming, remainder and valid frames) is read: AudioToolbox ignores it for the plain MP4 and the 3GPP / 3GPP2 file
    /// types, that is for every file whose brands do not make it `m4af` / `m4bf` (macOS 27.2: the same bytes under another `ftyp` give the length the
    /// default priming gives).  The edit list with a negative-roll sample group is read under every type.
    pub(crate) fn reads_itunsmpb(self) -> bool {
        !matches!(self, FileType::Mp4 | FileType::ThreeGpp | FileType::ThreeGpp2)
    }
}

/// An AudioSpecificConfig is read from its first bytes only (the longest one the parsers consume is a few hundred bytes); a descriptor that holds
/// more has the rest ignored instead of copied.
pub(crate) const MAX_ASC_BYTES: usize = 4096;

/// `esds` contents Apple's file layer needs.
#[derive(Clone, Debug)]
pub(crate) struct Esds {
    pub asc: Vec<u8>,
}

fn read_desc_len(d: &[u8], pos: &mut usize) -> Option<usize> {
    let mut len = 0usize;
    for _ in 0..4 {
        let b = *d.get(*pos)?;
        *pos += 1;
        len = (len << 7) | (b & 0x7f) as usize;
        if b & 0x80 == 0 {
            return Some(len);
        }
    }
    Some(len)
}

/// Parses an `ES_Descriptor` chain (the body of `esds` after its version/flags, or the CAF `kuki`).  Apple requires: the `0x03` wrapper,
/// `streamType` audio (5), `objectTypeIndication` 0x40 (MPEG-4 audio) or 0x67 (MPEG-2 AAC LC), a DecoderSpecificInfo with at least one byte,
/// and no OCR stream.
pub(crate) fn parse_es_descriptor(d: &[u8]) -> Result<Esds, OpenError> {
    let bad = OpenError::Unspecified(Unspecified::BadEsds);
    let mut pos = 0usize;
    if *d.get(pos).ok_or(bad)? != 0x03 {
        return Err(bad);
    }
    pos += 1;
    let es_len = read_desc_len(d, &mut pos).ok_or(bad)?;
    let es_end = pos + es_len;
    if es_end > d.len() {
        return Err(bad); // a descriptor longer than what is there
    }
    pos += 2; // ES_ID
    let flags = *d.get(pos).ok_or(bad)?;
    pos += 1;
    if flags & 0x80 != 0 {
        pos += 2; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        let n = *d.get(pos).ok_or(bad)? as usize;
        pos += 1 + n; // URL
    }
    if flags & 0x20 != 0 {
        return Err(bad); // OCR_ES_ID: refused by Apple
    }
    let mut asc: Option<Vec<u8>> = None;
    while pos < es_end {
        let tag = d[pos];
        pos += 1;
        let len = read_desc_len(d, &mut pos).ok_or(bad)?;
        let body_end = pos + len;
        if body_end > es_end {
            return Err(bad);
        }
        if tag == 0x04 {
            let oti = *d.get(pos).ok_or(bad)?;
            let st = *d.get(pos + 1).ok_or(bad)?;
            if oti != 0x40 && oti != 0x67 {
                return Err(bad);
            }
            if st >> 2 != 5 {
                return Err(bad);
            }
            // objectTypeIndication(1) streamType(1) bufferSizeDB(3) maxBitrate(4) avgBitrate(4)
            let mut q = pos + 13;
            while q < body_end {
                let t2 = d[q];
                q += 1;
                let l2 = read_desc_len(d, &mut q).ok_or(bad)?;
                let e2 = q + l2;
                if e2 > body_end {
                    return Err(bad);
                }
                if t2 == 0x05 && asc.is_none() && q <= e2 {
                    asc = Some(d[q..e2.min(q + MAX_ASC_BYTES)].to_vec());
                }
                q = e2;
            }
        }
        pos = body_end;
    }
    match asc {
        Some(a) if !a.is_empty() => Ok(Esds { asc: a }),
        _ => Err(bad),
    }
}

/// One edit-list entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    /// movie-timescale units; 0 = "to the end"
    pub segment_duration: u64,
    /// media-timescale units; negative = empty edit
    pub media_time: i64,
}

/// `iTunSMPB` words as parsed (see [`parse_itunsmpb`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Itun {
    pub priming: u32,
    pub remainder: u32,
    pub valid_frames: u64,
}

/// One iTunSMPB field as Apple reads it: C `strtoull(base 16)` over the `width` characters at `from` (macOS 27.2): white space, an optional sign (a minus
/// negates modulo 2^64), an optional `0x` / `0X` in front of a hex digit, hex digits up to the first other character; no digits give 0, more than 64 bits
/// give 2^64 - 1.
fn smpb_field(t: &[u8], from: usize, width: usize) -> u64 {
    let s = t.get(from..(from + width).min(t.len())).unwrap_or(&[]);
    let is_ws = |c: u8| c == b' ' || (9..=13).contains(&c);
    let digit = |c: u8| (c as char).to_digit(16);
    let mut i = 0;
    while i < s.len() && is_ws(s[i]) {
        i += 1;
    }
    let negative = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    if s.get(i) == Some(&b'0') && s.get(i + 1).is_some_and(|c| c | 0x20 == b'x') && s.get(i + 2).is_some_and(|&c| digit(c).is_some()) {
        i += 2;
    }
    let mut v: u64 = 0;
    while let Some(d) = s.get(i).and_then(|&c| digit(c)) {
        match v.checked_mul(16).and_then(|x| x.checked_add(u64::from(d))) {
            Some(x) => v = x,
            None => return u64::MAX,
        }
        i += 1;
    }
    if negative { v.wrapping_neg() } else { v }
}

/// Apple's iTunSMPB reader (macOS 27.2).  The standard text is ` 00000000 PPPPPPPP RRRRRRRR VVVVVVVVVVVVVVVV ...`; the three fields are read at FIXED
/// places, whatever stands between them: priming from 9 (9 characters), remainder from 18 (9 characters), valid frames from 27 (17 characters), each as a
/// number (see [`smpb_field`]) and the first two cut to 32 bits.  The characters in front of them (offsets 0..9) are not looked at, and a separator that is
/// no white space is part of the number it starts: junk there gives 0 (priming 0, remainder 0, or no valid frames = absent), a hex digit there is a digit
/// (a leading `A` is cut off with the 32-bit cut).  Text shorter than 44 bytes, priming/remainder >= 2^31, valid frames >= 2^63 or == 0 mean "absent".
pub fn parse_itunsmpb(text: &[u8]) -> Option<Itun> {
    if text.len() < 44 {
        return None;
    }
    let p = smpb_field(text, 9, 9) & 0xffff_ffff;
    let r = smpb_field(text, 18, 9) & 0xffff_ffff;
    let v = smpb_field(text, 27, 17);
    if p > 0x7fff_ffff || r > 0x7fff_ffff || v > 0x7fff_ffff_ffff_ffff || v == 0 {
        return None;
    }
    // AudioToolbox carries the count through a double (and half a frame): see `through_a_double`
    Some(Itun { priming: p as u32, remainder: r as u32, valid_frames: crate::portable::aac::decoder::file::trim::through_a_double(v) })
}

/// The children of the movie atom are walked one after the other; one whose size field is 0 or 2..=7 is no atom, whichever atom it is: the open fails `-1` when it
/// is the first child, `-2` behind another (macOS 27.2: a zero-filled movie atom, `mvhd` / `trak` / `udta` with such a size, an `mvhd` of 8 bytes whose
/// payload then reads as a child of size 0).  A size of 1 is the 64-bit form; a child that does not fit ends the walk.
fn check_movie_children(d: &[u8], moov: &Atom) -> Result<(), OpenError> {
    let (mut pos, mut index) = (moov.body_start, 0usize);
    while pos + 8 <= moov.end {
        let size = match be32(d, pos) {
            Some(0) | Some(2..=7) => return Err(OpenError::BadMovieChild { first: index == 0 }),
            Some(1) => match be64(d, pos + 8) {
                Some(wide) if wide >= 16 => wide,
                _ => return Ok(()),
            },
            Some(n) => u64::from(n),
            None => return Ok(()),
        };
        if size > (moov.end - pos) as u64 {
            return Ok(());
        }
        pos += size as usize;
        index += 1;
    }
    Ok(())
}

/// A shared allowance of atom visits for scans that overlap on purpose.
struct Allowance(std::cell::Cell<usize>);

impl Allowance {
    fn atoms<'a>(&'a self, d: &'a [u8], start: usize, end: usize) -> impl Iterator<Item = Atom> + 'a {
        atoms(d, start, end).take_while(move |_| {
            let left = self.0.get();
            left > 0 && {
                self.0.set(left - 1);
                true
            }
        })
    }

    fn child(&self, d: &[u8], a: &Atom, typ: &[u8; 4]) -> Option<Atom> {
        self.atoms(d, a.body_start, a.end).find(|c| &c.typ == typ)
    }
}

fn find_itunsmpb(d: &[u8], moov: &Atom) -> Option<Itun> {
    // The first child of a udta is bounded by the movie atom, not by the udta (below), so the `meta` of every udta of a chain of udta atoms runs over all the
    // later ones and the scans of its children walk them again, once per udta.  They draw on one allowance of visits instead, a multiple of the size of the
    // movie atom that a real file does not come near (an atom is 8 bytes at least, and a scan visits an atom a handful of times): the walk of the movie
    // atom stays linear, and what a file that uses the allowance up carries as a tag is not read.
    let allow = Allowance(std::cell::Cell::new((moov.end - moov.body_start) / 2 + 64));
    // Every `udta` is looked at and the LAST usable tag wins (macOS 27.2: two or three `udta` each with an `iTunSMPB`: the last one; a later one that has none,
    // or whose first `iTunSMPB` item is no usable text, does not take an earlier one away).  Within a `udta` the first `iTunSMPB` item decides.
    let mut found = None;
    for udta in allow.atoms(d, moov.body_start, moov.end).filter(|a| &a.typ == b"udta") {
        // only the FIRST child of a udta is looked at (a `free` or `\xa9nam` before `meta` hides it; a first udta holding only a `free`
        // does not hide a later udta).  It starts 8 bytes behind the udta and is bounded by the movie atom, not by the size the udta declares (macOS 27.2:
        // a `meta` behind a udta of 8, 9, 12 bytes, or of size field 1, is found all the same)
        for meta in allow.atoms(d, udta.start + 8, moov.end).take(1).filter(|a| &a.typ == b"meta") {
            let (ms, me) = (meta.body_start + 4, meta.end); // full box
            if ms > me {
                continue;
            }
            let has_mdir = allow.atoms(d, ms, me).any(|a| &a.typ == b"hdlr" && d.get(a.body_start + 8..a.body_start + 12) == Some(b"mdir".as_slice()));
            if !has_mdir {
                continue;
            }
            let Some(ilst) = allow.atoms(d, ms, me).find(|a| &a.typ == b"ilst") else { continue };
            for item in allow.atoms(d, ilst.body_start, ilst.end).filter(|a| &a.typ == b"----") {
                let mean = allow.child(d, &item, b"mean");
                let name = allow.child(d, &item, b"name");
                let data = allow.child(d, &item, b"data");
                let (Some(mean), Some(name), Some(data)) = (mean, name, data) else { continue };
                if mean.end <= mean.body_start + 4 {
                    continue; // empty mean string
                }
                if d.get(name.body_start + 4..name.end) != Some(b"iTunSMPB".as_slice()) {
                    continue;
                }
                if data.clamped {
                    continue; // the value is cut off by the end of the movie atom
                }
                let text = d.get(data.body_start + 8..data.end).unwrap_or(&[]);
                found = parse_itunsmpb(text).or(found);
                break;
            }
        }
    }
    found
}

/// Everything the MP4 layer yields for the (first audio) track.
#[derive(Clone, Debug)]
pub(crate) struct Mp4Track {
    pub file_type: FileType,
    pub asc: Asc,
    pub media_ts: u32,
    pub movie_ts: u32,
    /// the first edit with a non-negative media time
    pub first_edit: Option<Edit>,
    /// a `sgpd` `roll` description exists and every entry is negative (rule R3)
    pub roll_gate: bool,
    /// the iTunSMPB tag, for the file types that read it
    pub itun: Option<Itun>,
    /// (absolute file offset, size) per sample, in decode order
    pub samples: Vec<(u64, u32)>,
    /// the first `unlocated` samples (those of the sample table, not the fragments') have no position: the table has no sample-to-chunk map, every read of
    /// them fails `'pck?'`
    pub unlocated: usize,
    /// the bytes the bit rate property counts: the sizes of all samples, except that behind a zero-size sample of a `trun` its remaining samples
    /// are not counted (macOS 27.2)
    pub data_bytes: u64,
}

/// Most access units a file may hold (a corrupt packet table can claim billions; the table is materialised in memory).
pub(crate) const MAX_PACKETS: usize = 1 << 24;

/// `stsz`: `'wht?'` when the declared sample count exceeds the table; a count beyond [`MAX_PACKETS`] is refused.
fn parse_stsz(d: &[u8], a: &Atom, file_len: usize) -> Result<Vec<u32>, OpenError> {
    let bad = OpenError::Unspecified(Unspecified::BadStructure);
    let b = a.body_start;
    let size = be32(d, b + 4).ok_or(bad)?;
    let count = be32(d, b + 8).ok_or(bad)? as usize;
    if size != 0 {
        // constant sample size: the samples must fit into the file (and be countable)
        if (count as u128) * u128::from(size) > file_len as u128 {
            return Err(bad);
        }
        if count > MAX_PACKETS {
            return Err(OpenError::PacketTableTooLarge);
        }
        return Ok(vec![size; count]);
    }
    let avail = (a.end.saturating_sub(b + 12)) / 4;
    if count > avail {
        return Err(bad);
    }
    if count > MAX_PACKETS {
        return Err(OpenError::PacketTableTooLarge);
    }
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        v.push(be32(d, b + 12 + 4 * i).ok_or(bad)?);
    }
    Ok(v)
}

/// `stz2`, the compact sample size table: only the 16-bit field size is read (macOS 27.2: 8- and 4-bit tables and every other field size fail the open
/// `'wht?'`; the reserved bits in front of the field size and the entry count behave as in `stsz`: a count the table cannot hold fails, a smaller one is
/// read as far as it goes).
fn parse_stz2(d: &[u8], a: &Atom) -> Result<Vec<u32>, OpenError> {
    let bad = OpenError::Unspecified(Unspecified::BadStructure);
    let b = a.body_start;
    if *d.get(b + 7).ok_or(bad)? != 16 {
        return Err(bad);
    }
    let count = be32(d, b + 8).ok_or(bad)? as usize;
    if count > (a.end.saturating_sub(b + 12)) / 2 {
        return Err(bad);
    }
    if count > MAX_PACKETS {
        return Err(OpenError::PacketTableTooLarge);
    }
    (0..count).map(|i| be16(d, b + 12 + 2 * i).map(u32::from).ok_or(bad)).collect()
}

/// Where AudioToolbox reads a packet that no chunk of the table reaches (macOS 27.2): at offset 0, the start of the file, whatever the packet's size
/// is.  What is read there is no access unit: the converter rejects it, and the fourth rejection in a row fails the read (`'bada'`); a single such
/// packet at the end of the table ends the stream silently.
pub(crate) const UNPLACED_OFFSET: u64 = 0;

/// The samples of the sample table, and whether they can be located at all: without a sample-to-chunk map the table exists but no packet has a position (every read
/// of it fails `'pck?'`; the samples are then listed at offset 0).
fn parse_sample_table(d: &[u8], file_len: usize, stbl: &Atom, trak_end: usize, fragmented: bool) -> Result<(Vec<(u64, u32)>, bool), OpenError> {
    let bad = OpenError::Unspecified(Unspecified::BadStructure);
    let sizes = if let Some(stsz) = find_table(d, stbl, trak_end, b"stsz") {
        parse_stsz(d, &stsz, file_len)?
    } else if let Some(stz2) = find_table(d, stbl, trak_end, b"stz2") {
        parse_stz2(d, &stz2)?
    } else {
        return if fragmented { Ok((Vec::new(), true)) } else { Err(bad) };
    };
    sample_locations(d, stbl, trak_end, sizes, fragmented).ok_or(bad)
}

/// The (offset, size) of every sample: `stsc` / `stco` / `co64` applied to the sizes of `stsz`; the samples the chunks do not reach come last, at
/// [`UNPLACED_OFFSET`].
fn sample_locations(d: &[u8], stbl: &Atom, trak_end: usize, sizes: Vec<u32>, fragmented: bool) -> Option<(Vec<(u64, u32)>, bool)> {
    let stsc = find_table(d, stbl, trak_end, b"stsc");
    let stco = find_table(d, stbl, trak_end, b"stco");
    let co64 = find_table(d, stbl, trak_end, b"co64");
    let mut runs: Vec<(u32, u32)> = Vec::new();
    if let Some(stsc) = stsc {
        let n = be32(d, stsc.body_start + 4)? as usize;
        let held = (stsc.end.saturating_sub(stsc.body_start + 8)) / 12;
        // an entry count that the atom cannot hold makes the whole map unusable
        for i in 0..if n > held { 0 } else { n } {
            let o = stsc.body_start + 8 + 12 * i;
            runs.push((be32(d, o)?, be32(d, o + 4)?));
        }
    }
    if runs.is_empty() {
        // no sample-to-chunk map: the packet table exists but no packet can be located (reads fail 'pck?')
        return Some((sizes.into_iter().map(|s| (UNPLACED_OFFSET, s)).collect(), false));
    }
    // The chunk offsets are read where they lie in the file image, as many as the atom holds: AudioToolbox ignores the entry count of `stco` and
    // `co64` (macOS 27.2: counts of 0, 1, 2, 9 and 2^32 - 1 over a table of three entries read all three).  (entries, 64-bit entries, position of the first)
    let (chunks, wide, first) = if let Some(c) = stco {
        ((c.end.saturating_sub(c.body_start + 8)) / 4, false, c.body_start + 8)
    } else if let Some(c) = co64 {
        ((c.end.saturating_sub(c.body_start + 8)) / 8, true, c.body_start + 8)
    } else if fragmented {
        return Some((Vec::new(), true));
    } else {
        return None;
    };
    // a chunk offset table that cannot hold its header is no table (macOS 27.2: `'wht?'`; with a header and no entry, every packet is unplaced)
    if stco.or(co64).is_some_and(|c| c.end.saturating_sub(c.body_start) < 8) {
        return None;
    }
    if chunks == 0 && !fragmented {
        // no chunk at all: every packet is unplaced
        return Some((sizes.into_iter().map(|s| (UNPLACED_OFFSET, s)).collect(), true));
    }
    let offset = |i: usize| if wide { be64(d, first + 8 * i) } else { be32(d, first + 4 * i).map(u64::from) };
    // The run of a chunk is the LAST run (in table order) whose first chunk is not above the chunk's number, the first run when there is none.  The
    // chunks come in ascending order, so the runs are swept once, in the order of their first chunks: a run enters when its first chunk is reached and
    // the run in force is the greatest table index that has entered (O(runs log runs + chunks), not runs x chunks).
    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by_key(|&i| runs[i].0);
    let (mut entered, mut active) = (0usize, 0usize);
    let mut out = Vec::with_capacity(sizes.len());
    let mut si = 0usize;
    for ci in 0..chunks {
        if si >= sizes.len() {
            break;
        }
        let chunk_no = ci as u32 + 1;
        while entered < order.len() && runs[order[entered]].0 <= chunk_no {
            active = active.max(order[entered]);
            entered += 1;
        }
        let spc = runs[active].1;
        if spc == 0 && entered == order.len() {
            break; // every run is in force and this one holds no sample: neither does any later chunk
        }
        let mut pos = offset(ci)?;
        for _ in 0..spc {
            if si >= sizes.len() {
                break;
            }
            out.push((pos, sizes[si]));
            pos = pos.saturating_add(u64::from(sizes[si]));
            si += 1;
        }
    }
    // When no sample is reached (every run holds zero samples, as with a missing sample-to-chunk map) every read of the table fails 'pck?'.
    // Otherwise the samples the runs do not reach stay in the table, unplaced (a `samples_per_chunk` one short: the stream ends silently).
    if out.is_empty() {
        return Some((sizes.into_iter().map(|s| (UNPLACED_OFFSET, s)).collect(), false));
    }
    out.extend(sizes[si..].iter().map(|&s| (UNPLACED_OFFSET, s)));
    Some((out, true))
}

/// The first edit with a non-negative media time, the only one the trim reads (`None`: no `elst`, or no such entry).
fn parse_elst(d: &[u8], edts: &Atom) -> Option<Edit> {
    let elst = find_child(d, edts, b"elst")?;
    let b = elst.body_start;
    let version = (be32(d, b)? >> 24) as u8;
    let n = be32(d, b + 4).unwrap_or(0) as usize;
    let esz = if version == 1 { 20 } else { 12 };
    (0..n.min(elst.end.saturating_sub(b + 8) / esz)).find_map(|i| {
        let o = b + 8 + i * esz;
        let (segment_duration, media_time) = if version == 1 {
            (be64(d, o)?, be64(d, o + 8)? as i64)
        } else {
            (u64::from(be32(d, o)?), i64::from(be32(d, o + 4)? as i32))
        };
        (media_time >= 0).then_some(Edit { segment_duration, media_time })
    })
}

/// The sample group descriptions (`sgpd`) of the sample table, read like AudioToolbox reads them (macOS 27.2).  Every `sgpd` must hold its header
/// (a payload of exactly 4 bytes, the version and flags, is all there is; anything else needs the grouping type, the `default_length` of version 1 and
/// above, a further word for version 2 and above, and the entry count) or the open fails `'wht?'`.  Only the grouping types `roll` and `prol` are read
/// further (the same way); the result is whether some `roll` description exists whose entries are all negative (zero entries: vacuously so): see
/// [`roll_entries`].
fn roll_gate(d: &[u8], stbl: &Atom) -> Result<bool, OpenError> {
    let mut gate = false;
    for a in atoms(d, stbl.body_start, stbl.end).filter(|a| &a.typ == b"sgpd") {
        let payload = d.get(a.body_start..a.end).ok_or(OpenError::Unspecified(Unspecified::BadStructure))?;
        gate |= roll_entries(payload).ok_or(OpenError::Unspecified(Unspecified::BadStructure))?;
    }
    Ok(gate)
}

/// One `sgpd` payload: `None` when it is too short for what it announces (the open fails), else whether it is a `roll` description of negative distances
/// (a `prol` one is read as far, and is none).
///
/// The entries are `roll_distance`s of 2 bytes: a version 0 box has nothing else, a version 1 or 2 box has `default_length` (2, or 0 for a length in
/// front of every entry); any other entry length means no usable description (`false`, nothing more is read).  The entries are read one by one up to the
/// first non-negative one (`false`; for `prol`: the first one that is not positive); a missing entry fails the open, except when the announced entries could not fit by more than 12 bytes: such a count is
/// not read at all (`true`, as for no entries) - macOS 27.2: for 1 to 6 missing 2-byte entries `'wht?'`, for 7 or more none, whatever the entries say.
fn roll_entries(p: &[u8]) -> Option<bool> {
    let word = |o: usize| p.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    match p.len() {
        0..=3 => return None,
        4 => return Some(false),
        5..=7 => return None,
        _ => {}
    }
    let version = p[0];
    let mut o = 8;
    let mut length = 0u32;
    if version >= 1 {
        length = word(o)?;
        o += 4;
    }
    if version >= 2 {
        word(o)?;
        o += 4;
    }
    let count = word(o)?;
    o += 4;
    if !matches!(&p[4..8], b"roll" | b"prol") {
        return Some(false);
    }
    let roll = &p[4..8] == b"roll";
    let smallest = if version == 0 { 2 } else if length == 0 { 4 } else { u64::from(length) };
    if u64::from(count) * smallest > (p.len() - o) as u64 + 12 {
        return Some(roll);
    }
    for _ in 0..count {
        let size = if version >= 1 && length == 0 {
            let size = word(o)?;
            o += 4;
            size
        } else if version >= 1 {
            length
        } else {
            2
        };
        if size != 2 {
            return Some(false);
        }
        let entry = p.get(o..o + 2)?;
        o += 2;
        // a `roll` description goes on while the distances are negative, a `prol` one while they are positive
        let distance = i16::from_be_bytes([entry[0], entry[1]]);
        if if roll { distance >= 0 } else { distance <= 0 } {
            return Some(false);
        }
    }
    Some(roll)
}

#[derive(Clone, Copy, Default)]
struct Trex {
    track_id: u32,
    default_size: u32,
}

/// The samples of the movie fragments (`moof` > `traf` > `trun`) of track `track_id`, appended to `samples`.
///
/// Rules recorded on macOS 27.2: a `trun` whose sample count its atom cannot hold (`count` x the 4-byte fields its flags announce > the bytes behind
/// its fixed fields) is skipped as if it were not there, the other fragments are read; a `trun` without sizes (no `0x200` flag, no default size)
/// fails the open (`'wht?'`).  A table whose samples would hold more bytes than the file ends there (the caller refuses it as `'wht?'`, whatever
/// the count claims), so a few bytes can never make a table of millions of entries.
fn parse_fragments(d: &[u8], track_id: u32, trex: &[Trex], samples: &mut Vec<(u64, u32)>, data_bytes: &mut u64) -> Result<(), OpenError> {
    let mut total: u64 = samples.iter().map(|s| u64::from(s.1)).sum();
    // the default size of the track's `trex` (the first one, whatever the list holds): looked up once, not once per `traf`
    let trex_size = trex.iter().find(|t| t.track_id == track_id).map(|t| t.default_size).unwrap_or(0);
    for moof in top_atoms(d).filter(|a| &a.typ == b"moof") {
        for traf in atoms(d, moof.body_start, moof.end).filter(|a| &a.typ == b"traf") {
            let Some(tfhd) = find_child(d, &traf, b"tfhd") else { continue };
            let b = tfhd.body_start;
            let (Some(vf), Some(id)) = (be32(d, b), be32(d, b + 4)) else { continue };
            if id != track_id {
                continue;
            }
            let flags = vf & 0x00ff_ffff;
            let mut o = b + 8;
            let mut base = moof.start as u64;
            if flags & 0x1 != 0 {
                base = be64(d, o).unwrap_or(base);
                o += 8;
            }
            if flags & 0x2 != 0 {
                o += 4;
            }
            if flags & 0x8 != 0 {
                o += 4;
            }
            let mut default_size = trex_size;
            if flags & 0x10 != 0 {
                default_size = be32(d, o).unwrap_or(default_size);
            }
            let mut next = base;
            for trun in atoms(d, traf.body_start, traf.end).filter(|a| &a.typ == b"trun") {
                let b = trun.body_start;
                let (Some(vf), Some(count)) = (be32(d, b), be32(d, b + 4)) else { continue };
                let f = vf & 0x00ff_ffff;
                let mut o = b + 8;
                let mut pos = next;
                if f & 0x1 != 0 {
                    let off = i64::from(be32(d, o).unwrap_or(0) as i32);
                    pos = base.wrapping_add_signed(off);
                    o += 4;
                }
                if f & 0x4 != 0 {
                    o += 4;
                }
                // the per-sample fields (duration, size, flags, composition offset) are 4 bytes each and must lie inside the atom
                let per_sample = 4 * (f & 0xf00).count_ones() as usize;
                if per_sample * count as usize > trun.end.saturating_sub(o) {
                    continue;
                }
                if count != 0 && f & 0x200 == 0 && default_size == 0 {
                    return Err(OpenError::Unspecified(Unspecified::BadStructure));
                }
                // room for the samples at once (the table doubles from there on, not on the way): a sample is at least a byte of the file, and the file holds
                // `d.len()` of them at most
                samples.reserve((count as usize).min(d.len()).min(MAX_PACKETS));
                let mut counted = true;
                for _ in 0..count {
                    if samples.len() >= MAX_PACKETS {
                        return Err(OpenError::PacketTableTooLarge);
                    }
                    if f & 0x100 != 0 {
                        o += 4;
                    }
                    let mut size = default_size;
                    if f & 0x200 != 0 {
                        let Some(s) = be32(d, o) else { return Ok(()) };
                        size = s;
                        o += 4;
                    }
                    if f & 0x400 != 0 {
                        o += 4;
                    }
                    if f & 0x800 != 0 {
                        o += 4;
                    }
                    samples.push((pos, size));
                    if counted {
                        *data_bytes += u64::from(size);
                        counted = size != 0;
                    }
                    pos = pos.saturating_add(u64::from(size));
                    total += u64::from(size);
                    if total > d.len() as u64 {
                        return Ok(()); // more audio than file: refused by the caller
                    }
                }
                next = pos;
            }
        }
    }
    Ok(())
}

/// Finds the `esds` of an `mp4a` sample entry (plain, QuickTime version 1/2 with `wave`).
fn find_esds(d: &[u8], entry: &Atom) -> Option<Atom> {
    let ver = be16(d, entry.body_start + 8).unwrap_or(0);
    let skip = match ver {
        1 => 44,
        2 => 64,
        _ => 28,
    };
    let cs = entry.body_start + skip;
    for c in atoms(d, cs, entry.end) {
        if &c.typ == b"esds" {
            return Some(c);
        }
        if &c.typ == b"wave" {
            if let Some(e) = find_child(d, &c, b"esds") {
                return Some(e);
            }
        }
    }
    None
}

/// Whether the handler atom names the sound handler: its type field (12 bytes into the atom) must lie inside the atom (macOS 27.2: a `hdlr` of 8 to 19
/// bytes is no sound handler, whatever the bytes behind it say).
fn is_sound_handler(d: &[u8], hdlr: &Atom) -> bool {
    hdlr.end >= hdlr.body_start + 12 && d.get(hdlr.body_start + 8..hdlr.body_start + 12) == Some(b"soun".as_slice())
}

/// The sample-entry type of the first `soun` track in the body of a `moov` atom (`None` when there is none, or the structure is unusable): the
/// track Apple opens decides the codec.
pub(crate) fn first_sound_entry(moov_body: &[u8]) -> Option<[u8; 4]> {
    let d = moov_body;
    for trak in atoms(d, 0, d.len()).filter(|a| &a.typ == b"trak") {
        let Some(mdia) = find_child(d, &trak, b"mdia") else { continue };
        let Some(hdlr) = find_child(d, &mdia, b"hdlr") else { continue };
        if !is_sound_handler(d, &hdlr) {
            continue;
        }
        let minf = find_child(d, &mdia, b"minf")?;
        let stbl = find_child(d, &minf, b"stbl")?;
        let stsd = find_child(d, &stbl, b"stsd")?;
        return atoms(d, stsd.body_start + 8, stsd.end).next().map(|entry| entry.typ);
    }
    None
}

/// Result of recognising + parsing an MP4-family file.
pub(crate) fn parse_mp4(file: &[u8]) -> Result<Mp4Track, OpenError> {
    let bad = OpenError::Unspecified(Unspecified::BadStructure);
    let d = file;
    // recognition: the first atom is `ftyp` with at least one compatible brand
    let first = top_atoms(d).next().ok_or(OpenError::UnsupportedFileType)?;
    // `ftyp` is recognised with a plain 32-bit size from 20 up to 99 bytes, as far as the file holds it (a cut file keeps its brand, the rest of it is
    // what is missing); from 100 bytes it is `'typ?'` (macOS 27.2)
    let declared = be32(d, 0).unwrap_or(0);
    if &first.typ != b"ftyp" || first.wide || first.to_end || declared < 20 || declared > 0x7fff_ffff || !(20..FTYP_LIMIT).contains(&(first.end - first.start)) {
        return Err(OpenError::UnsupportedFileType);
    }
    let major = &d[first.body_start..first.body_start + 4];
    let compat = (first.body_start + 8..first.end.saturating_sub(3)).step_by(4).map(|o| &d[o..o + 4]);
    let file_type = file_type_from_brands(major, compat);

    let mut moov: Option<Atom> = None;
    for a in top_atoms(d) {
        if (&a.typ == b"mdat" || &a.typ == b"moov") && a.to_end {
            return Err(OpenError::InvalidFile);
        }
        if &a.typ == b"moov" && moov.is_none() {
            moov = Some(a);
        }
    }
    // no (complete) moov, no soun track, a track without mdia/hdlr: 'dta?' (recorded)
    let invalid = OpenError::InvalidFile;
    let moov = moov.ok_or(invalid)?;
    // the movie atom is read into memory as far as the file holds it, and its contents are held to that: an atom that does not fit is not there
    let d = &file[..moov.end];
    let movie_ts = atoms(d, moov.body_start, moov.end)
        .find(|a| &a.typ == b"mvhd")
        .and_then(|mvhd| match be32(d, mvhd.body_start)? >> 24 {
            1 => be32(d, mvhd.body_start + 20),
            _ => be32(d, mvhd.body_start + 12),
        })
        .unwrap_or(0);

    check_movie_children(d, &moov)?;

    // trex defaults (fragmented files)
    let mut trex: Vec<Trex> = Vec::new();
    let mut fragmented = false;
    if let Some(mvex) = find_child(d, &moov, b"mvex") {
        fragmented = true;
        for t in atoms(d, mvex.body_start, mvex.end).filter(|a| &a.typ == b"trex") {
            let b = t.body_start;
            if let (Some(id), Some(ds)) = (be32(d, b + 4), be32(d, b + 16)) {
                trex.push(Trex { track_id: id, default_size: ds });
            }
        }
    }

    // first `soun` track
    let mut chosen = None;
    for trak in atoms(d, moov.body_start, moov.end).filter(|a| &a.typ == b"trak") {
        let Some(mdia) = find_child(d, &trak, b"mdia") else { continue };
        let Some(hdlr) = find_child(d, &mdia, b"hdlr") else { continue };
        if is_sound_handler(d, &hdlr) {
            chosen = Some((trak, mdia));
            break;
        }
    }
    let (trak, mdia) = chosen.ok_or(invalid)?;
    let track_id = find_child(d, &trak, b"tkhd")
        .and_then(|t| match be32(d, t.body_start)? >> 24 {
            1 => be32(d, t.body_start + 20),
            _ => be32(d, t.body_start + 12),
        })
        .unwrap_or(0);
    // `mdhd`: version 0 or 1, and a non-zero timescale inside the atom (the fields behind it are not read; macOS 27.2)
    let media_ts = find_child(d, &mdia, b"mdhd")
        .and_then(|m| {
            let at = match be32(d, m.body_start)? >> 24 {
                0 => 12,
                1 => 20,
                _ => return None,
            };
            if m.end - m.body_start < at + 4 {
                return None;
            }
            be32(d, m.body_start + at).filter(|&timescale| timescale != 0)
        })
        .ok_or(bad)?;
    let minf = find_child(d, &mdia, b"minf").ok_or(bad)?;
    let stbl = find_child(d, &minf, b"stbl").ok_or(bad)?;
    // the data information and the sample table are read whole: a child of either that is no atom spoils it (macOS 27.2: `'wht?'`, whichever child)
    if !children_ok(d, &stbl) || find_child(d, &minf, b"dinf").is_some_and(|dinf| !children_ok(d, &dinf)) {
        return Err(bad);
    }
    let stsd = find_child(d, &stbl, b"stsd").ok_or(bad)?;
    // an `stsd` that announces no entry has none, whatever it holds (macOS 27.2: `'wht?'`; counts of 2 and 2^32 - 1 over one entry open)
    if be32(d, stsd.body_start + 4) == Some(0) {
        return Err(bad);
    }
    let entry = atoms(d, stsd.body_start + 8, stsd.end).next().ok_or(bad)?;
    if &entry.typ != b"mp4a" {
        return Err(OpenError::NotMp4a(entry.typ));
    }
    // sound-description version: QuickTime files (major brand `qt  `) need QuickTime's version 1 (2) entry ('dta?' for version 0); ISO files
    // refuse any entry with a non-zero version ('wht?') (recorded)
    let entry_version = be16(d, entry.body_start + 8).unwrap_or(0);
    if file_type == FileType::Mov {
        if entry_version == 0 {
            return Err(invalid);
        }
    } else if entry_version != 0 {
        return Err(bad);
    }
    let esds = find_esds(d, &entry).ok_or(OpenError::Unspecified(Unspecified::BadEsds))?;
    let es = parse_es_descriptor(d.get(esds.body_start + 4..esds.end).unwrap_or(&[]))?;
    let asc = parse_asc(&es.asc).map_err(|e| match e {
        crate::portable::aac::decoder::file::asc::AscError::ReservedSfi => OpenError::Unspecified(Unspecified::BadSampleRate),
        crate::portable::aac::decoder::file::asc::AscError::Truncated => OpenError::Unspecified(Unspecified::BadAsc),
    })?;

    let roll_gate = roll_gate(d, &stbl)?;
    let first_edit = find_child(d, &trak, b"edts").and_then(|e| parse_elst(d, &e));
    let (mut samples, located) = parse_sample_table(d, file.len(), &stbl, trak.end, fragmented)?;
    let unlocated = if located { 0 } else { samples.len() };
    let mut data_bytes: u64 = samples.iter().map(|s| u64::from(s.1)).sum();
    if fragmented {
        parse_fragments(file, track_id, &trex, &mut samples, &mut data_bytes)?;
    }
    let itun = if file_type.reads_itunsmpb() { find_itunsmpb(d, &moov) } else { None };
    Ok(Mp4Track {
        file_type,
        asc,
        media_ts,
        movie_ts,
        first_edit,
        roll_gate,
        itun,
        samples,
        unlocated,
        data_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::aac::decoder::testutil::{atom, full};

    fn hex(s: &str) -> Vec<u8> {
        let s: Vec<u8> = s.bytes().filter(|c| *c != b' ').collect();
        s.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap()).collect()
    }

    /// The sample group descriptions AudioToolbox read on macOS 27.2: the payload of one `sgpd` -> `None` (the open fails `'wht?'`) or whether it is a
    /// `roll` description of negative distances.  (The recordings are those of `rule/sgpd-*` and a randomised comparison of 3,000 payloads.)
    #[test]
    fn a_sample_group_description_is_read_as_far_as_audiotoolbox_reads_it() {
        let cases: &[(&str, Option<bool>)] = &[
            // headers: 4 bytes are all of a version and flags; the grouping type, default length(s) and the entry count must be there
            ("", None), ("00", None), ("000000", None), ("00000000", Some(false)), ("0000000072", None), ("00000000726f6c", None),
            ("0000000078787878", None), ("0000000078787878000000", None), ("000000007878787800000001", Some(false)),
            ("01000000787878780000000200000001", Some(false)), ("0100000078787878000000020000", None),
            ("02000000787878780000000200000001000000", None), ("02000000787878780000000200000001000000010000", Some(false)),
            // roll, version 0: negative entries go on, the first non-negative one ends it, a missing entry fails
            ("00000000726f6c6c00000000", Some(true)), ("00000000726f6c6c00000001ffff", Some(true)), ("00000000726f6c6c000000010001", Some(false)),
            ("00000000726f6c6c000000010000", Some(false)), ("00000000726f6c6c00000001", None), ("00000000726f6c6c00000002ffff", None),
            ("00000000726f6c6c00000002ffff0001", Some(false)), ("00000000726f6c6c000000020001", Some(false)), ("00000000726f6c6c0000000200", None),
            ("00000000726f6c6c00000003ffffffffffff", Some(true)), ("00000000726f6c6c00000004ffffffffffffff", None),
            // more than 12 bytes missing: not read at all
            ("00000000726f6c6c00000007ffff", None), ("00000000726f6c6c00000008ffff", Some(true)), ("00000000726f6c6c00000008", Some(true)),
            ("00000000726f6c6c000000080001", Some(true)), ("00000000726f6c6cffffffff", Some(true)), ("00000000726f6c6c80000000", Some(true)),
            // roll, versions 1 and 2: a default length of 2, an entry length in front of every entry (0), or another length (no description)
            ("01000000726f6c6c0000000200000001ffff", Some(true)), ("01000000726f6c6c000000020000000100010000", Some(false)),
            ("01000000726f6c6c0000000400000001ffffffff", Some(false)), ("01000000726f6c6c0000000100000003", Some(false)),
            ("01000000726f6c6c000000040000000400", Some(true)), ("01000000726f6c6c0000000400000001", Some(false)),
            ("01000000726f6c6c000000000000000100000002ffff", Some(true)), ("01000000726f6c6c000000000000000100000004ffff0000", Some(false)),
            ("01000000726f6c6c0000000000000002000000020001", Some(false)), ("01000000726f6c6c00000000000000020000000200", None),
            ("01000000726f6c6c000000000000000100", None), ("01000000726f6c6c00000000", None), ("01000000726f6c6c0000000000000004", Some(true)),
            ("02000000726f6c6c000000020000000100000001ffff", Some(true)), ("02000000726f6c6c000000020000000000000002ffff", None),
            ("03000000726f6c6c000000020000000000000001ffff", Some(true)),
            // prol: read like roll, goes on while the distances are positive, never the gate
            ("0000000070726f6c00000002", None), ("0000000070726f6c000000020000", Some(false)),
            ("0000000070726f6c000000020001", None), ("0000000070726f6c000000027fff", None), ("0000000070726f6c00000002ffff", Some(false)),
            ("0000000070726f6c000000028000", Some(false)), ("0000000070726f6c00000001ffff", Some(false)), ("0000000070726f6c0000000100010001", Some(false)),
            ("0000000070726f6c00000008", Some(false)),
        ];
        for (text, expected) in cases {
            assert_eq!(roll_entries(&hex(text)), *expected, "{text}");
        }
    }

    /// Every description of the table is read, and the gate is the `or` of the `roll` ones.
    #[test]
    fn every_sample_group_description_counts() {
        let sgpd = |payload: &str| atom(b"sgpd", &hex(payload));
        let table = |boxes: &[Vec<u8>]| {
            let stbl = atom(b"stbl", &boxes.concat());
            let a = atoms(&stbl, 0, stbl.len()).next().unwrap();
            roll_gate(&stbl, &a).ok()
        };
        let negative = "01000000726f6c6c0000000200000001ffff";
        let positive = "01000000726f6c6c000000020000000100 01".replace(' ', "");
        assert_eq!(table(&[]), Some(false));
        assert_eq!(table(&[sgpd(negative)]), Some(true));
        assert_eq!(table(&[sgpd(&positive)]), Some(false));
        assert_eq!(table(&[sgpd(&positive), sgpd(negative)]), Some(true));
        assert_eq!(table(&[sgpd(negative), sgpd(&positive)]), Some(true));
        // a description that fails the open fails it behind or in front of a good one
        assert_eq!(table(&[sgpd(negative), sgpd("0000000078787878")]), None);
        assert_eq!(table(&[sgpd("0000000078787878"), sgpd(negative)]), None);
        // a truncated sample-to-group map is nobody's business
        assert_eq!(table(&[sgpd(negative), atom(b"sbgp", &hex("0000"))]), Some(true));
    }

    /// The three fields are read at fixed places (priming 9.., remainder 18.., valid frames 27.., as `strtoull`): what stands at a separator is part of
    /// the number it starts (macOS 27.2: 2 x 3,500 mutated texts compared, no difference).
    #[test]
    fn the_itunsmpb_fields_are_read_at_fixed_places() {
        let standard = b" 00000000 00000100 00000123 0000000000002000 00000000 00000000".to_vec();
        let with = |edits: &[(usize, u8)]| {
            let mut t = standard.clone();
            for &(at, byte) in edits {
                t[at] = byte;
            }
            parse_itunsmpb(&t).map(|i| (i.priming, i.remainder, i.valid_frames))
        };
        assert_eq!(with(&[]), Some((0x100, 0x123, 0x2000)));
        // junk at a separator: a number of no digits (0), the later fields are untouched
        assert_eq!(with(&[(9, b'!')]), Some((0, 0x123, 0x2000)));
        assert_eq!(with(&[(9, 0xff)]), Some((0, 0x123, 0x2000)));
        assert_eq!(with(&[(18, 0)]), Some((0x100, 0, 0x2000)));
        assert_eq!(with(&[(27, b'!')]), None, "no valid frames");
        // a digit is a digit (a ninth digit of the 32-bit fields is cut off), a sign is a sign
        assert_eq!(with(&[(9, b'0')]), Some((0x100, 0x123, 0x2000)));
        assert_eq!(with(&[(9, b'A')]), Some((0x100, 0x123, 0x2000)));
        assert_eq!(with(&[(18, b'F')]), Some((0x100, 0x123, 0x2000)));
        assert_eq!(with(&[(27, b'0')]), Some((0x100, 0x123, 0x2000)));
        assert_eq!(with(&[(27, b'+')]), Some((0x100, 0x123, 0x2000)));
        assert_eq!(with(&[(9, b'+')]), Some((0x100, 0x123, 0x2000)));
        assert_eq!(with(&[(9, b'-')]), None, "-0x100 is 0xFFFFFF00 in 32 bits");
        assert_eq!(with(&[(18, b'-')]), None);
        assert_eq!(with(&[(27, b'-')]), None, "a negated count is above 2^63");
        assert_eq!(with(&[(27, b'A')]), None, "17 hex digits overflow");
        assert_eq!(with(&[(27, b'\t')]), Some((0x100, 0x123, 0x2000)), "white space in front of the digits");
        // the first 9 characters are not looked at
        assert_eq!(with(&[(3, b'!'), (4, 0), (0, b'x')]), Some((0x100, 0x123, 0x2000)));
        // a text of less than 44 bytes has no fields
        assert_eq!(parse_itunsmpb(&standard[..43]), None);
    }

    #[test]
    fn an_itunsmpb_frame_count_goes_through_a_double() {
        let text = |valid: u64| format!(" 00000000 00000840 0000000E {valid:016X} 00000000 00000000 00000000 00000000").into_bytes();
        // recorded on macOS 27.2: (written, read back as the length)
        for (written, length) in [
            (4096u64, 4096u64),
            ((1 << 52) + 1, (1 << 52) + 2),
            ((1 << 52) + 2, (1 << 52) + 2),
            ((1 << 52) + 5149, (1 << 52) + 5150),
            ((1 << 53) - 1, 1 << 53),
            (1 << 53, 1 << 53),
            ((1 << 53) + 1, 1 << 53),
            ((1 << 53) + 2, (1 << 53) + 2),
            ((1 << 53) + 3, (1 << 53) + 4),
            ((1 << 54) + 3, (1 << 54) + 4),
            ((1 << 54) + 5, (1 << 54) + 4),
            (0x00E0_0000_0000_07B2, 0x00E0_0000_0000_07B0),
            ((1 << 63) - 0x200, i64::MAX as u64),
            (i64::MAX as u64, i64::MAX as u64),
        ] {
            let itun = parse_itunsmpb(&text(written)).expect("a usable text");
            assert_eq!((itun.priming, itun.remainder, itun.valid_frames), (0x840, 0xE, length), "{written:#x}");
        }
        // 2^63 and above mean no iTunSMPB
        assert!(parse_itunsmpb(&text(1 << 63)).is_none() && parse_itunsmpb(&text(u64::MAX)).is_none());
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// The rule before the sweep, the plain way: every chunk takes the last run (table order) whose first chunk is not above its number, the first
    /// run when there is none; the samples no chunk reaches follow, unplaced (all of them when there is no chunk).
    fn plain(runs: &[(u32, u32)], offsets: &[u64], sizes: &[u32]) -> (Vec<(u64, u32)>, bool) {
        let (mut out, mut si) = (Vec::new(), 0usize);
        for (ci, &off) in offsets.iter().enumerate() {
            if si >= sizes.len() {
                break;
            }
            let number = ci as u32 + 1;
            let per_chunk = runs.iter().rev().find(|r| r.0 <= number).unwrap_or(&runs[0]).1;
            let mut pos = off;
            for _ in 0..per_chunk {
                if si >= sizes.len() {
                    break;
                }
                out.push((pos, sizes[si]));
                pos = pos.saturating_add(u64::from(sizes[si]));
                si += 1;
            }
        }
        if out.is_empty() && !offsets.is_empty() {
            return (sizes.iter().map(|&s| (UNPLACED_OFFSET, s)).collect(), false);
        }
        out.extend(sizes[si..].iter().map(|&s| (UNPLACED_OFFSET, s)));
        (out, true)
    }

    #[test]
    fn the_sweep_over_the_runs_equals_the_scan_for_every_chunk() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for case in 0..600 {
            let first_chunks = [0u32, 1, 1, 2, 3, 5, 8, 13, u32::MAX];
            let counts = [0u32, 0, 1, 2, 3, 7, 1 << 31];
            let runs: Vec<(u32, u32)> = (0..1 + rng.below(6)).map(|_| (first_chunks[rng.below(9) as usize], counts[rng.below(7) as usize])).collect();
            let wide = case % 2 == 1;
            let offsets: Vec<u64> = (0..rng.below(12)).map(|_| rng.below(1 << 20)).collect();
            let sizes: Vec<u32> = (0..rng.below(30)).map(|_| 1 + rng.below(500) as u32).collect();
            // the entry count of the chunk table is whatever it is: the entries the atom holds are the chunks
            let claimed = [offsets.len() as u32, 0, 1, u32::MAX, 7][rng.below(5) as usize];
            let stsc = full(b"stsc", 0, &[(runs.len() as u32).to_be_bytes().to_vec(), runs.iter().flat_map(|r| [r.0.to_be_bytes(), r.1.to_be_bytes(), 1u32.to_be_bytes()].concat()).collect()].concat());
            let table: Vec<u8> = offsets.iter().flat_map(|&o| if wide { o.to_be_bytes().to_vec() } else { (o as u32).to_be_bytes().to_vec() }).collect();
            let chunks = full(if wide { b"co64" } else { b"stco" }, 0, &[claimed.to_be_bytes().to_vec(), table].concat());
            let d = atom(b"stbl", &[stsc, chunks].concat());
            let stbl = top_atoms(&d).next().unwrap();
            let located = sample_locations(&d, &stbl, d.len(), sizes.clone(), false);
            assert_eq!(located, Some(plain(&runs, &offsets, &sizes)), "case {case}: runs {runs:?}, {} chunks (claimed {claimed}), {} samples", offsets.len(), sizes.len());
        }
    }

    #[test]
    fn a_million_runs_and_two_million_chunks_cost_no_product() {
        // the lookup for every chunk used to scan every run: 10^6 x 2 x 10^6 steps
        let runs: u32 = 1_000_000;
        let mut table = Vec::new();
        for i in 0..runs {
            table.extend_from_slice(&(if i == 0 { 1 } else { u32::MAX }).to_be_bytes());
            table.extend_from_slice(&0u32.to_be_bytes());
            table.extend_from_slice(&1u32.to_be_bytes());
        }
        let stsc = full(b"stsc", 0, &[runs.to_be_bytes().to_vec(), table].concat());
        let chunks = full(b"stco", 0, &[2_000_000u32.to_be_bytes().to_vec(), vec![0u8; 8_000_000]].concat());
        let d = atom(b"stbl", &[stsc, chunks].concat());
        let stbl = top_atoms(&d).next().unwrap();
        let started = std::time::Instant::now();
        let located = sample_locations(&d, &stbl, d.len(), vec![100], false).unwrap();
        assert_eq!(located, (vec![(UNPLACED_OFFSET, 100)], false));
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
    }

    fn edit_list(version: u8, entries: &[(u64, i64)]) -> (Vec<u8>, Atom) {
        let mut body = (entries.len() as u32).to_be_bytes().to_vec();
        for &(duration, media) in entries {
            if version == 1 {
                body.extend_from_slice(&duration.to_be_bytes());
                body.extend_from_slice(&media.to_be_bytes());
            } else {
                body.extend_from_slice(&(duration as u32).to_be_bytes());
                body.extend_from_slice(&(media as i32).to_be_bytes());
            }
            body.extend_from_slice(&[0, 1, 0, 0]);
        }
        let d = atom(b"edts", &full(b"elst", u32::from(version) << 24, &body));
        let edts = top_atoms(&d).next().unwrap();
        (d, edts)
    }

    #[test]
    fn the_edit_list_yields_its_first_non_negative_entry() {
        for version in [0u8, 1] {
            let (d, edts) = edit_list(version, &[(10, -1), (20, -1), (30, 5), (40, 7)]);
            assert_eq!(parse_elst(&d, &edts), Some(Edit { segment_duration: 30, media_time: 5 }), "version {version}");
            let (d, edts) = edit_list(version, &[(10, -1), (20, -1)]);
            assert_eq!(parse_elst(&d, &edts), None, "version {version}");
            let (d, edts) = edit_list(version, &[]);
            assert_eq!(parse_elst(&d, &edts), None, "version {version}");
        }
        // a count the atom cannot hold is read as far as the atom goes
        let (mut d, edts) = edit_list(0, &[(10, -1), (30, 5)]);
        let at = edts.body_start + 8 + 4;
        d[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(parse_elst(&d, &top_atoms(&d).next().unwrap()), Some(Edit { segment_duration: 30, media_time: 5 }));
    }

    #[test]
    fn an_atom_that_runs_past_its_parent_ends_where_the_parent_does() {
        let child = [&100u32.to_be_bytes()[..], b"kid ", &[0; 4]].concat();
        let d = [atom(b"par ", &child), vec![0xee; 200]].concat();
        let parent = top_atoms(&d).next().unwrap();
        let kid = atoms(&d, parent.body_start, parent.end).next().unwrap();
        assert_eq!((kid.end, kid.clamped), (parent.end, true));
        let whole = atom(b"kid ", &[0; 4]);
        let d = atom(b"par ", &whole);
        let parent = top_atoms(&d).next().unwrap();
        let kid = atoms(&d, parent.body_start, parent.end).next().unwrap();
        assert_eq!((kid.end, kid.clamped), (parent.end, false));
    }
}
