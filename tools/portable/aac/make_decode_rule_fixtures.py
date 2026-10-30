#!/usr/bin/env python3
"""make_decode_rule_fixtures.py - append the container-rule cases to the decoder golden fixtures (tests/fixtures/aac-decode).

`make_decode_fixtures.py build` made the first parts of the pack from generated corpora.  The cases here pin down rules of the
macOS reader (AudioToolbox `ExtAudioFile`, driven through `decode_probe` like every other fixture) that those corpora did not
reach, each as a handful of small inputs derived from one base file of this repository:

  brand      the `ftyp` brands under which the iTunSMPB tag counts (only the 28 `ftyp` bytes of one file differ)
  ftyp       the size window of the `ftyp` box (20..99 bytes is an MP4 file, 100 and more is not) and its declared size
  atoms      a track whose media atom runs past its parent, an iTunSMPB `data` atom that runs past its parent
  trun       movie fragments: counts above what the atom holds, no sizes, zero-size samples, huge base data offsets
  cafhe      CAF `aac ` over a cookie that signals SBR / PS
  asc        explicit (hierarchical) and implicit SBR / PS signalling, ASCs that end early
  sizes      size fields that are no atoms (0, 1, 2..7), at every place of the movie atom's tree
  damage     sample tables (`stz2`, zero entries, counts that lie, flipped sample-to-chunk map), `stsd` / `mdhd` fields, `moov`
             sizes, sample entry types
  loas       LOAS frames that carry another StreamMuxConfig, or cannot be read as an access unit at all
  sgpd       sample group descriptions (`roll`, `prol`, other types; versions 0 to 3; truncated headers and entries, counts that lie, several of them)
  smpb       iTunSMPB frame counts above 2^53 (AudioToolbox carries them as doubles) and above 2^63 - 1
  short      the size field of a container of the track (`stbl`, `minf`, `mdia`, `trak`, `moov`) shortened by a few bytes, so that its last child runs past it
  cafpakt    CAF packet-table valid-frame counts from 2^52 on (the length goes through a double and half a frame, as the iTunSMPB one does)
  udta       several `udta` atoms with an iTunSMPB each (the last usable one counts)
  ldsc       a CAF layered description (`ldsc`): its first stream description replaces the `desc` one (HE-AAC files whose cookie is damaged or gone)
  pcemix     in-band program config elements with a `matrix_mixdown_idx` (and without), in 5.1 streams cut from fixtures of this pack: the stereo
             downmix of a 5.1 stream follows the index of the last one (any position, any access unit, also a rejected one)

commands (python 3.9+; `extend` needs macOS arm64 with the engine's toolchain):

  extend --probe decode_probe --work SCRATCH --dir tests/fixtures/aac-decode --reader tests/fixtures/aac-reader
      Generates the inputs, records each twice through decode_probe (the recordings must be identical), classifies them, appends
      them as new pack parts and manifest entries (sorted by id), and extends readsizes.json with their other-read-size
      recordings.  The cases the manifest has stay as they are (their inputs must still be what the generators make); only the others are added.
  list --dir tests/fixtures/aac-decode --reader tests/fixtures/aac-reader
      Prints the generated inputs (id, name, bytes); needs no probe.

The generators are deterministic.  Recorded facts are copied from decode_probe's JSON lines untouched.
"""
import argparse
import collections
import json
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import make_decode_fixtures as M  # noqa: E402

GROUP_ORIGIN = "container-rule"


def be32(v):
    return struct.pack(">I", v & 0xFFFFFFFF)


def be16(v):
    return struct.pack(">H", v)


class Case:
    def __init__(self, group, name, ext, data, note, tags=(), refusal=None, base=None):
        self.group, self.name, self.ext, self.data, self.note, self.tags = group, name, ext, bytes(data), note, tuple(tags)
        self.base = base or "tiny-44100-2 (aac-reader fixtures)"
        # when Apple opens the input as AAC-LC and the port must still refuse it (the output is not the one of AAC-LC): why
        self.refusal = refusal

    @property
    def key(self):
        return "rule/%s-%s" % (self.group, self.name)

    @property
    def filename(self):
        return "%s_%s.%s" % (self.group, self.name.replace("-", "_"), self.ext) if self.ext else "%s_%s" % (self.group, self.name.replace("-", "_"))


# ---- bases ---------------------------------------------------------------------------------------------------------------------


class Bases:
    """The files the cases are cut from."""

    def __init__(self, reader_dir, manifest, parts):
        with open(os.path.join(reader_dir, "tiny-44100-2.m4a"), "rb") as f:
            self.m4a = f.read()
        with open(os.path.join(reader_dir, "tiny-44100-2.loas"), "rb") as f:
            self.loas = f.read()
        with open(os.path.join(reader_dir, "tiny-44100-2.caf"), "rb") as f:
            self.caf = f.read()
        self.manifest, self.parts = manifest, parts
        self.fmp4 = self.pack_file("probe/fmp4_no_mvex")
        nodes, tail = M.parse_mp4(self.m4a)
        assert not tail and [n.typ for n in nodes] == [b"ftyp", b"moov", b"free", b"mdat"]
        # the base file without its `free` padding, parsed again so that the chunk offsets are those of the compact layout
        compact = M.mp4_bytes([n for n in nodes if n.typ != b"free"], fix_offsets_from=M.first_mdat_start(self.m4a))
        self.nodes, tail = M.parse_mp4(compact)
        assert not tail and [n.typ for n in self.nodes] == [b"ftyp", b"moov", b"mdat"]
        self.origin = M.first_mdat_start(compact)
        assert self.mp4(self.clone()) == compact

    def pack_file(self, fixture_id):
        return M.file_bytes(next(e for e in self.manifest["files"] if e["id"] == fixture_id), self.parts)

    def clone(self):
        return [n.clone() for n in self.nodes]

    def mp4(self, nodes):
        """Serialized with the chunk offsets moved with `mdat`."""
        return M.mp4_bytes(nodes, fix_offsets_from=self.origin)


def moov(nodes):
    return next(n for n in nodes if n.typ == b"moov")


def stbl_box(nodes, typ):
    return moov(nodes).find(b"trak", b"mdia", b"minf", b"stbl", typ)


def mdhd_box(nodes):
    return moov(nodes).find(b"trak", b"mdia", b"mdhd")


def ftyp_data(major, minor, compat):
    return major + be32(minor) + b"".join(compat)


# ---- audio specific configs -----------------------------------------------------------------------------------------------------


def bits(fields):
    v, n = 0, 0
    for val, width in fields:
        v = (v << width) | val
        n += width
    pad = (-n) % 8
    return (v << pad).to_bytes((n + pad) // 8, "big")


def descriptor(tag, body):
    assert len(body) < 128
    return bytes([tag, len(body)]) + body


def es_descriptor(asc):
    decoder = bytes([0x40, 0x15, 0, 0, 0]) + bytes(8) + descriptor(5, asc)
    return descriptor(3, bytes([0, 1, 0]) + descriptor(4, decoder) + descriptor(6, bytes([2])))


def set_asc(nodes, asc):
    """The sound entry's `esds` carries `asc` (the entry is the first one of the `stsd`, `esds` its last child)."""
    stsd = stbl_box(nodes, b"stsd")
    p = stsd.data
    at = p.find(b"esds") - 4
    esds = be32(12 + len(es_descriptor(asc))) + b"esds" + be32(0) + es_descriptor(asc)
    entry = p[8:at] + esds
    stsd.data = p[:8] + be32(len(entry)) + entry[4:]


# ---- case groups ------------------------------------------------------------------------------------------------------------------

BRANDS = [
    ("m4a", b"M4A ", 0, [b"M4A ", b"mp42", b"isom"]),
    ("m4b", b"M4B ", 0, [b"M4B ", b"mp42", b"isom"]),
    ("m4v", b"M4V ", 0, [b"M4V ", b"mp42", b"isom"]),
    ("mp42", b"mp42", 0, [b"mp42", b"isom", b"mp41"]),
    ("isom", b"isom", 0, [b"isom", b"iso2", b"mp41"]),
    ("mp42-compat-m4a", b"mp42", 0, [b"mp42", b"M4A ", b"isom"]),
    ("mp42-compat-m4b", b"mp42", 0, [b"mp42", b"M4B ", b"isom"]),
    ("mp42-compat-m4v", b"mp42", 0, [b"M4V ", b"mp42", b"isom"]),
    ("mp42-compat-m4p", b"mp42", 0, [b"isom", b"mp41", b"M4P "]),
    ("3gp4", b"3gp4", 0, [b"3gp4", b"isom", b"mp42"]),
    ("3gp5", b"3gp5", 0, [b"3gp5", b"3gp4", b"isom"]),
    ("m4a-compat-3gp4", b"M4A ", 0, [b"M4A ", b"3gp4", b"isom"]),
    ("m4a-compat-3gp9", b"M4A ", 0, [b"M4A ", b"3gp9", b"isom"]),
    ("m4a-compat-3g2a", b"M4A ", 0, [b"M4A ", b"3g2a", b"isom"]),
    ("m4a-compat-3gp6", b"mp42", 0, [b"M4A ", b"3gp6", b"mp42"]),
    ("m4a-compat-3gp3", b"M4A ", 0, [b"M4A ", b"3gp3", b"isom"]),
    ("lowercase-m4a", b"m4a ", 0, [b"m4a ", b"mp42", b"isom"]),
    ("m4a-minor", b"M4A ", 0x20000, [b"M4A ", b"mp42", b"isom"]),
    ("m4a-isom-only", b"M4A ", 0, [b"M4A ", b"isom", b"isom"]),
    ("dash", b"iso6", 0, [b"iso6", b"mp41", b"dash"]),
    ("mp42-compat-3gp7", b"mp42", 0, [b"mp42", b"isom", b"3gp7"]),
    ("3gp9-compat-m4a", b"3gp9", 0, [b"3gp9", b"M4A ", b"isom"]),
    ("m4a-m4b-m4v", b"M4A ", 0, [b"M4A ", b"M4B ", b"M4V "]),
    ("3g2a-compat-m4a", b"3g2a", 0, [b"3g2a", b"M4A ", b"isom"]),
]


def brand_cases(b):
    out = []
    for name, major, minor, compat in BRANDS:
        nodes = b.clone()
        nodes[0].data = ftyp_data(major, minor, compat)
        data = b.mp4(nodes)
        assert len(data) == len(b.mp4(b.clone()))
        out.append(Case("brand", name, "m4a", data, "ftyp brands %s / %s, minor %#x; the rest of the file is the base file's" % (major.decode(), ", ".join(c.decode() for c in compat), minor), ["rule:brand"]))
    return out


def ftyp_cases(b):
    out = []
    def compat(n):
        return [b"M4A "] + [b"isom"] * (n - 1)
    for size in (20, 24, 96, 97, 99, 100, 104, 1000):
        brands = (size - 16) // 4
        nodes = b.clone()
        nodes[0].data = ftyp_data(b"mp42", 0, compat(brands)) + bytes((size - 16) % 4)
        assert len(nodes[0].data) + 8 == size
        out.append(Case("ftyp", "size-%d" % size, "m4a", b.mp4(nodes), "an ftyp box of %d bytes holding %d compatible brands" % (size, brands), ["rule:ftyp"]))
    base = b.mp4(b.clone())
    for declared in (20, 60, 99, 100, 200, 0x10000, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFE):
        d = bytearray(base)
        d[0:4] = be32(declared)
        out.append(Case("ftyp", "declared-%#x" % declared, "m4a", d, "the base file with the ftyp size field changed to %d" % declared, ["rule:ftyp"]))
    for held in (19, 20, 60, 99, 100, 101):
        d = bytearray(base[:held])
        d[0:4] = be32(0x7FFFFFFF)
        out.append(Case("ftyp", "held-%d" % held, "m4a", d, "the first %d bytes of the base file, the ftyp size field 2^31 - 1" % held, ["rule:ftyp"]))
    return out


def atom_cases(b):
    out = []
    for mdia_size in (0x00FFFFFF, 8 + 34, 8):
        nodes = b.clone()
        mo = moov(nodes)
        real = mo.find(b"trak")
        tkhd, hdlr = real.find(b"tkhd"), real.find(b"mdia", b"hdlr")
        decoy = M.Node(b"trak", be32(8 + len(tkhd.data)) + b"tkhd" + tkhd.data + be32(mdia_size) + b"mdia")
        at = mo.kids.index(real)
        mo.kids[at:at] = [decoy, hdlr.clone()]
        out.append(Case("atoms", "decoy-track-mdia-%d" % mdia_size, "m4a", b.mp4(nodes),
                        "a first track without handler whose mdia atom declares %d bytes, a stray sound hdlr behind it, then the real track" % mdia_size, ["rule:atoms"]))
    # iTunSMPB `data` atom running past its parent (the file: item 188 bytes at 702, data 132 bytes at 758); a free atom inside the movie
    # atom keeps the bytes behind the item readable
    for item, data in ((188, 132), (188, 136), (184, 132), (188, 172), (128, 132), (102, 132), (132, 132)):
        nodes = b.clone()
        moov(nodes).kids.append(M.Node(b"free", bytes(92)))
        d = bytearray(b.mp4(nodes))
        assert d[706:710] == b"----" and d[762:766] == b"data", "layout of the base file"
        d[702:706] = be32(item)
        d[758:762] = be32(data)
        out.append(Case("atoms", "itunsmpb-item-%d-data-%d" % (item, data), "m4a", d, "iTunSMPB item of %d bytes holding a data atom of %d bytes (undamaged: 188 and 132)" % (item, data), ["rule:atoms"]))
    return out


def trun_cases(b):
    out = []
    trex = M.Node(b"trex", be32(0) + be32(1) + be32(1) + be32(1024) + be32(0) + be32(0))

    def build(edit=None):
        nodes, tail = M.parse_mp4(b.fmp4)
        assert not tail
        moov(nodes).kids.append(M.Node(b"mvex", b"", [trex.clone()]))
        if edit:
            edit([n for n in nodes if n.typ == b"moof"])
        data = bytearray(M.mp4_bytes(nodes))
        return bytes(data)

    def trun_of(moofs, i=0):
        return moofs[i].find(b"traf", b"trun")

    def patch(i=0, count=None, flags=None):
        def edit(moofs):
            t = trun_of(moofs, i)
            d = bytearray(t.data)
            if flags is not None:
                d[0:4] = be32(flags)
            if count is not None:
                d[4:8] = be32(count)
            t.data = bytes(d)
        return edit

    out.append(Case("trun", "plain", "m4a", build(), "the fragmented file with a trex added to its movie atom", ["rule:trun"]))
    for count in (0, 7, 1000, 1000000):
        out.append(Case("trun", "count-%d" % count, "m4a", build(patch(0, count=count)), "first fragment's trun declares %d samples (it holds 6)" % count, ["rule:trun"]))
    out.append(Case("trun", "count-1000-second", "m4a", build(patch(1, count=1000)), "second fragment's trun declares 1000 samples (it holds 5)", ["rule:trun"]))
    for flags in (0x101, 0x001, 0x201):
        for count in (6, 1000000):
            out.append(Case("trun", "flags-%x-count-%d" % (flags, count), "m4a", build(patch(0, count=count, flags=flags)),
                            "first fragment's trun with flags %#x declares %d samples" % (flags, count), ["rule:trun"]))

    def zero_size(index):
        def edit(moofs):
            t = trun_of(moofs)
            d = bytearray(t.data)
            at = 12 + 8 * index + 4
            d[at:at + 4] = be32(0)
            t.data = bytes(d)
        return edit
    for index in (2, 5, 8):
        out.append(Case("trun", "zero-size-sample-%d" % index, "m4a", build(zero_size(index)), "sample %d of the first fragment has size 0" % index, ["rule:trun"]))

    def base_offset(value):
        def edit(moofs):
            traf = moofs[0].find(b"traf")
            tfhd = traf.find(b"tfhd")
            tfhd.data = be32(0x000001) + tfhd.data[4:8] + struct.pack(">Q", value)
            trun = traf.find(b"trun")
            d = bytearray(trun.data)
            d[8:12] = be32(0)
            trun.data = bytes(d)
        return edit
    for value in (0, 0x7FFFFFFFFFFFFFF0, 0x8000000000000000, 0xFFFFFFFFFFFF0000):
        out.append(Case("trun", "base-offset-%x" % value, "m4a", build(base_offset(value)), "first fragment's tfhd carries base data offset %#x (data offset 0)" % value, ["rule:trun"]))
    return out


def caf_he_cases(b):
    """The HE-AAC CAF files of the pack with the `desc` format changed to 'aac ' (a plain AAC-LC description over a cookie that signals SBR / PS), at
    other rates and frames per packet."""
    out = []
    for source in ("afconvert/he1_caf_44s", "afconvert/he2_caf_44s"):
        data = b.pack_file(source)
        chunks, pos = [], 8
        while pos + 12 <= len(data):
            size = struct.unpack(">q", data[pos + 4:pos + 12])[0]
            end = len(data) if size < 0 else pos + 12 + size
            chunks.append((data[pos:pos + 4], data[pos + 12:end]))
            pos = end
        assert pos == len(data) and data[:8] == b"caff\x00\x01\x00\x00"
        label = source.split("/")[1].split("_")[0]
        for rate, fpp in ((44100.0, 1024), (22050.0, 1024), (88200.0, 1024), (48000.0, 1024), (44100.0, 2048)):
            body = bytearray(data[:8])
            for typ, chunk in chunks:
                if typ == b"desc":
                    chunk = struct.pack(">d", rate) + b"aac " + chunk[12:20] + be32(fpp) + chunk[24:]
                body += typ + struct.pack(">q", len(chunk)) + chunk
            out.append(Case("cafhe", "%s-rate-%d-fpp-%d" % (label, rate, fpp), "caf", bytes(body),
                            "%s with its desc changed to 'aac ' at %d Hz, %d frames per packet (the cookie signals SBR%s)" % (source, rate, fpp, " + PS" if label == "he2" else ""), ["rule:cafhe"],
                            refusal="Apple decodes it as HE-AAC; the port refuses a desc 'aac ' over a cookie that signals SBR or PS"))
    return out


SFI = {96000: 0, 88200: 1, 64000: 2, 48000: 3, 44100: 4, 32000: 5, 24000: 6, 22050: 7, 16000: 8, 12000: 9, 11025: 10, 8000: 11}


def hierarchical_asc(ps, core_aot, core_sfi, cfg, ext_sfi, ext_rate=None):
    f = [(29 if ps else 5, 5), (core_sfi, 4), (cfg, 4), (ext_sfi, 4)]
    if ext_sfi == 15:
        f.append((ext_rate, 24))
    return bits(f + [(core_aot, 5), (0, 1), (0, 1), (0, 1)])


def implicit_asc(ps, core_aot, core_sfi, cfg, ext_sfi, ext_rate=None):
    f = [(core_aot, 5), (core_sfi, 4), (cfg, 4), (0, 1), (0, 1), (0, 1), (0x2B7, 11), (5, 5), (1, 1), (ext_sfi, 4)]
    if ext_sfi == 15:
        f.append((ext_rate, 24))
    if ps:
        f += [(0x548, 11), (1, 1)]
    return bits(f)


def asc_cases(b):
    out = []

    def add(kind, name, asc, note):
        nodes = b.clone()
        set_asc(nodes, asc)
        out.append(Case("asc", "%s-%s" % (kind, name), "m4a", b.mp4(nodes), "ASC %s (%s): %s" % (asc.hex(), kind, note), ["rule:asc"]))
    grid = []
    for cfg in (0, 1, 2, 3, 5, 7):
        grid.append(("sbr-cfg%d" % cfg, False, 2, 7, cfg, 4, None, "SBR, AAC-LC core at 22.05 kHz, channel configuration %d, extension 44.1 kHz" % cfg))
    grid += [
        ("sbr-same-rate", False, 2, 4, 2, 4, None, "SBR, core and extension at 44.1 kHz"),
        ("sbr-ratio-3", False, 2, 4, 2, 3, None, "SBR, core 44.1 kHz, extension 48 kHz (neither the core rate nor twice it)"),
        ("sbr-reserved-13", False, 2, 7, 2, 13, None, "SBR, reserved extension rate index 13"),
        ("sbr-escape-rate", False, 2, 7, 2, 15, 44100, "SBR, extension rate 44100 as an escape value"),
        ("sbr-ld-core", False, 23, 7, 2, 4, None, "SBR signalled over an AAC-LD core"),
        ("sbr-main-core", False, 1, 7, 2, 4, None, "SBR signalled over an AAC-Main core"),
    ]
    for cfg in (1, 2, 3, 6):
        grid.append(("ps-cfg%d" % cfg, True, 2, 7, cfg, 4, None, "SBR + PS, core at 22.05 kHz, channel configuration %d, extension 44.1 kHz" % cfg))
    grid += [("ps-cfg1-same-rate", True, 2, 4, 1, 4, None, "SBR + PS, channel configuration 1, core and extension at 44.1 kHz")]
    for name, ps, aot, core_sfi, cfg, ext_sfi, ext_rate, note in grid:
        add("implicit", name, implicit_asc(ps, aot, core_sfi, cfg, ext_sfi, ext_rate), note)
        add("hierarchical", name, hierarchical_asc(ps, aot, core_sfi, cfg, ext_sfi, ext_rate), note)
    # explicit hierarchical signalling around a nested SBR core: no such audio object type
    add("hierarchical", "nested-sbr-core", bits([(5, 5), (7, 4), (2, 4), (4, 4), (5, 5), (0, 3)]), "the core of the hierarchical signalling is audio object type 5 itself")
    for name, asc, note in (
        ("depends-core-truncated", bits([(2, 5), (4, 4), (2, 4), (0, 1), (1, 1), (0, 1)]), "dependsOnCoreCoder set, the ASC ends inside coreCoderDelay"),
        ("depends-core-full", bits([(2, 5), (4, 4), (2, 4), (0, 1), (1, 1), (1234, 14), (0, 1)]), "dependsOnCoreCoder set with its delay"),
        ("depends-core-zero-delay", bits([(2, 5), (4, 4), (2, 4), (0, 1), (1, 1), (0, 14), (0, 1)]), "dependsOnCoreCoder set, delay 0"),
        ("extension-flag", bits([(2, 5), (4, 4), (2, 4), (0, 1), (0, 1), (1, 1), (0, 1)]), "extensionFlag set"),
        ("one-byte", bytes([0x12]), "a one-byte ASC"),
        ("frame-960", bits([(2, 5), (4, 4), (2, 4), (1, 1), (0, 1), (0, 1)]), "frameLengthFlag set (960-sample frames)"),
    ):
        add("flags", name, asc, note)
    return out


def damage_cases(b):
    out = []

    def case(name, edit, note, ext="m4a"):
        nodes = b.clone()
        edit(nodes)
        out.append(Case("damage", name, ext, b.mp4(nodes), note, ["rule:damage"]))

    def raw(name, fn, note):
        d = bytearray(b.mp4(b.clone()))
        fn(d)
        out.append(Case("damage", name, "m4a", bytes(d), note, ["rule:damage"]))

    def edit(box_of, fn):
        def go(nodes):
            box = box_of(nodes)
            box.data = fn(bytearray(box.data))
        return go

    def put(offset, value):
        def fn(p):
            p[offset:offset + len(value)] = value
            return bytes(p)
        return fn

    def stsz(n):
        return stbl_box(n, b"stsz")

    def stsd(n):
        return stbl_box(n, b"stsd")

    def stco(n):
        return stbl_box(n, b"stco")

    def stsc(n):
        return stbl_box(n, b"stsc")

    sizes = struct.unpack(">11I", stsz(b.clone()).data[12:56])
    # sample size table
    case("stsz-zero-entries", edit(stsz, lambda p: bytes(p[:12]) + bytes(44)), "every entry of the sample size table is 0")
    case("stsz-zero-first", edit(stsz, put(12, be32(0))), "the first sample size is 0")
    case("stsz-zero-fourth", edit(stsz, put(24, be32(0))), "the fourth sample size is 0")
    case("stsz-constant-0-no-table", edit(stsz, lambda p: bytes(p[:4]) + be32(0) + be32(11)), "sample_size 0 and a count of 11, no table")

    def stz2(field, count, entries, reserved=b"\0\0\0"):
        def fn(p):
            body = b""
            if field == 16:
                body = b"".join(be16(s) for s in entries)
            elif field == 8:
                body = bytes(s & 255 for s in entries)
            elif field == 4:
                v = [s & 15 for s in entries] + [0]
                body = bytes((v[i] << 4) | v[i + 1] for i in range(0, len(entries), 2))
            return be32(0) + reserved + bytes([field]) + be32(count) + body
        return fn

    def stz2_case(name, field, count, entries=sizes, reserved=b"\0\0\0", note=""):
        def go(nodes):
            box = stsz(nodes)
            box.typ = b"stz2"
            box.data = stz2(field, count, entries, reserved)(box.data)
        case(name, go, note)
    stz2_case("stz2-16", 16, 11, note="compact sample size table with 16-bit entries")
    stz2_case("stz2-16-reserved-bytes", 16, 11, reserved=b"\x01\x02\x03", note="16-bit stz2 whose reserved bytes are not 0")
    stz2_case("stz2-16-count-5", 16, 5, note="16-bit stz2 declaring 5 entries over a table of 11")
    stz2_case("stz2-16-count-100", 16, 100, note="16-bit stz2 declaring 100 entries over a table of 11")
    stz2_case("stz2-8", 8, 11, entries=[s % 256 for s in sizes], note="8-bit stz2")
    stz2_case("stz2-4", 4, 11, entries=[s % 16 for s in sizes], note="4-bit stz2, 11 entries")
    stz2_case("stz2-4-count-10", 4, 10, entries=[s % 16 for s in sizes][:10], note="4-bit stz2, 10 entries")
    stz2_case("stz2-field-12", 12, 11, note="stz2 with a field size of 12")
    stz2_case("stz2-field-0", 0, 11, note="stz2 with a field size of 0")
    stz2_case("stz2-field-32", 32, 11, note="stz2 with a field size of 32")
    # stsd and mdhd
    case("stsd-count-0", edit(stsd, put(4, be32(0))), "stsd announces no entry")
    case("stsd-count-2", edit(stsd, put(4, be32(2))), "stsd announces 2 entries and holds 1")
    case("stsd-count-max", edit(stsd, put(4, be32(0xFFFFFFFF))), "stsd announces 2^32 - 1 entries and holds 1")
    case("mdhd-timescale-0", edit(mdhd_box, put(12, be32(0))), "media timescale 0")
    case("mdhd-all-zero", edit(mdhd_box, lambda p: bytes(len(p))), "mdhd of zeros")
    for length in (12, 16, 20, 23):
        case("mdhd-v0-cut-%d" % length, edit(mdhd_box, lambda p, length=length: bytes(p[:length])), "version 0 mdhd cut to %d bytes of payload" % length)
    for length in (20, 24, 32):
        case("mdhd-v1-cut-%d" % length, edit(mdhd_box, lambda p, length=length: (bytes([1, 0, 0, 0]) + bytes(16) + bytes(p[12:16]) + struct.pack(">Q", struct.unpack(">I", bytes(p[16:20]))[0]) + bytes(p[20:]))[:length]),
             "version 1 mdhd cut to %d bytes of payload" % length)
    case("mdhd-v1", edit(mdhd_box, lambda p: bytes([1, 0, 0, 0]) + bytes(16) + bytes(p[12:16]) + struct.pack(">Q", struct.unpack(">I", bytes(p[16:20]))[0]) + bytes(p[20:])), "a complete version 1 mdhd")
    case("mdhd-v2", edit(mdhd_box, put(0, bytes([2]))), "mdhd of version 2")

    # chunk tables: three chunks (4, 4, 3 samples) so that entry counts matter
    def three_chunks(nodes):
        stsc(nodes).data = be32(0) + be32(1) + be32(1) + be32(4) + be32(1)
        first = stco(nodes)
        start = struct.unpack(">I", first.data[8:12])[0]
        sizes_ = struct.unpack(">11I", stsz(nodes).data[12:56])
        offsets = [start, start + sum(sizes_[:4]), start + sum(sizes_[:8])]
        first.data = be32(0) + be32(3) + b"".join(be32(o) for o in offsets)
        return offsets
    case("stco-three-chunks", lambda n: three_chunks(n), "samples in 3 chunks of 4, 4 and 3 (the unmodified table)")
    for count in (0, 1, 2, 9, 0xFFFFFFFF):
        def go(nodes, count=count):
            three_chunks(nodes)
            stco(nodes).data = stco(nodes).data[:4] + be32(count) + stco(nodes).data[8:]
        case("stco-count-%d" % count, go, "stco declares %d chunk offsets over a table of 3" % count)

    def co64(nodes):
        three_chunks(nodes)
        box = stco(nodes)
        offsets = struct.unpack(">3I", box.data[8:20])
        box.typ = b"co64"
        box.data = be32(0) + be32(3) + b"".join(struct.pack(">Q", o) for o in offsets)
    case("co64-three-chunks", co64, "the same three chunks in a co64")

    def co64_count(count):
        def go(nodes):
            co64(nodes)
            box = stbl_box(nodes, b"co64")
            box.data = box.data[:4] + be32(count) + box.data[8:]
        return go
    for count in (0, 1, 0xFFFFFFFF):
        case("co64-count-%d" % count, co64_count(count), "co64 declares %d chunk offsets over a table of 3" % count)
    for name, value in (("max", 2**64 - 1), ("max-1", 2**64 - 2), ("2-63", 2**63), ("beyond-file", 1 << 40)):
        for chunk in (0, 1):
            def go(nodes, value=value, chunk=chunk):
                co64(nodes)
                box = stbl_box(nodes, b"co64")
                at = 8 + 8 * chunk
                box.data = box.data[:at] + struct.pack(">Q", value) + box.data[at + 8:]
            case("co64-offset-%s-chunk-%d" % (name, chunk), go, "chunk %d of the three-chunk co64 table starts at %d" % (chunk, value))
    case("stco-zero-entries", edit(stco, lambda p: bytes(p[:8]) + bytes(len(p) - 8)), "every chunk offset is 0")
    case("stco-header-only", edit(stco, lambda p: bytes(p[:8])), "stco that holds its header and no entry")
    case("stco-header-only-count-9", edit(stco, lambda p: bytes(p[:4]) + be32(9)), "stco that holds its header and no entry, declaring 9")
    case("stco-cut-4", edit(stco, lambda p: bytes(p[:4])), "stco of 4 bytes of payload")
    case("stco-empty", edit(stco, lambda p: b""), "stco without payload")
    case("stsc-flip-samples-per-chunk", edit(stsc, lambda p: bytes(p[:12]) + be32(struct.unpack(">I", bytes(p[12:16]))[0] ^ 1) + bytes(p[16:])), "the samples per chunk of the first run flipped in bit 0")
    case("stsc-flip-samples-per-chunk-hi", edit(stsc, lambda p: bytes(p[:12]) + be32(struct.unpack(">I", bytes(p[12:16]))[0] ^ 0x80) + bytes(p[16:])), "the samples per chunk of the first run flipped in bit 7")

    # movie atom
    d0 = bytearray(b.mp4(b.clone()))
    for name, fn, note in (
        ("moov-size-0", lambda d: d.__setitem__(slice(28, 32), be32(0)), "the movie atom's size field is 0 (to the end of the file), movie atom first"),
        ("moov-size-8", lambda d: d.__setitem__(slice(28, 32), be32(8)), "the movie atom's size field is 8 (no content)"),
    ):
        d = bytearray(d0)
        fn(d)
        out.append(Case("damage", name, "m4a", bytes(d), note, ["rule:damage"]))
    nodes = b.clone()
    last = [nodes[0], nodes[2], nodes[1]]
    d = bytearray(M.mp4_bytes(last, fix_offsets_from=b.origin))
    out.append(Case("damage", "moov-last", "m4a", bytes(d), "the movie atom behind mdat", ["rule:damage"]))
    assert d[len(d) - 858:len(d) - 854] == b"moov"
    d[len(d) - 862:len(d) - 858] = be32(0)
    out.append(Case("damage", "moov-last-size-0", "m4a", bytes(d), "the movie atom behind mdat, its size field 0 (to the end of the file)", ["rule:damage"]))
    d = bytearray(d)
    d += bytes(40)
    out.append(Case("damage", "moov-last-size-0-trailing", "m4a", bytes(d), "the same with 40 zero bytes behind it", ["rule:damage"]))

    # sound entry types
    def entry_type(t):
        def fn(d):
            at = bytes(d).find(b"mp4a")
            d[at:at + 4] = t
        return fn
    # (AudioToolbox answers 'wht?' for these; the types it opens as other codecs - alac, ac-3, ec-3, drms, samr, sawb - are not AAC and not cases here)
    for t in (b"np4a", b"mp4b", b"MP4A", b"sowt", b"lpcm", b"enca", b"zzzz", b"\0\0\0\0", b".mp3", b"Opus", b"fLaC", b"ima4", b"mp4v"):
        raw("entry-%s" % t.hex(), entry_type(t), "the sound sample entry's type is %r instead of mp4a" % t)
    return out


def size_cases(b):
    """Size fields that are no atoms, at every place of the movie atom's tree: 0 for each atom, and a few other values where the answer is of its own."""
    out = []
    base = b.mp4(b.clone())
    containers = {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"dinf", b"udta", b"edts"}
    atoms = []

    def walk(lo, hi, path):
        pos = lo
        while pos + 8 <= hi:
            size, typ = struct.unpack(">I4s", base[pos:pos + 8])
            if size < 8 or pos + size > hi:
                break
            name = path + "." + typ.decode("latin1")
            atoms.append((pos, typ, name))
            inner = {b"stsd": 16, b"mp4a": 8 + 28, b"meta": 12, b"ilst": 8}.get(typ, 8 if typ in containers else None)
            if inner is not None:
                walk(pos + inner, pos + size, name)
            pos += size
    moov_size = struct.unpack(">I", base[28:32])[0]
    assert base[32:36] == b"moov"
    walk(36, 28 + moov_size, "moov")

    def put(name, pos, size, note):
        d = bytearray(base)
        d[pos:pos + 4] = be32(size)
        out.append(Case("sizes", name, "m4a", bytes(d), note, ["rule:sizes"]))
    for pos, typ, name in atoms:
        put("zero-" + name.replace("/", "."), pos, 0, "the size field of %s is 0" % name)
    at = {name: pos for pos, typ, name in atoms}
    for atom, sizes in (("moov.udta", (1, 2, 7, 8, 9, 12)), ("moov.mvhd", (2, 7, 8)), ("moov.trak", (2, 8)), ("moov.trak.mdia.hdlr", (8, 12, 16, 20)),
                        ("moov.trak.mdia.minf.dinf.dref", (1,)), ("moov.trak.mdia.minf.stbl.stsd.mp4a", (1, 8))):
        for size in sizes:
            put("%s-%d" % (atom, size), at[atom], size, "the size field of %s is %d" % (atom, size))
    d = bytearray(base)
    d[28:32] = be32(1)
    out.append(Case("sizes", "moov-1", "m4a", bytes(d), "the size field of the movie atom is 1 (no 64-bit size follows)", ["rule:sizes"]))
    d = bytearray(base)
    d[36:28 + moov_size] = bytes(moov_size - 8)
    out.append(Case("sizes", "moov-zero-payload", "m4a", bytes(d), "the payload of the movie atom is all zeros", ["rule:sizes"]))
    return out


def loas_frames(d):
    out, pos = [], 0
    while pos + 3 <= len(d) and d[pos] == 0x56:
        n = ((d[pos + 1] & 0x1F) << 8) | d[pos + 2]
        out.append(d[pos + 3:pos + 3 + n])
        pos += 3 + n
    return out


def bit_string(b):
    return "".join("{:08b}".format(x) for x in b)


def from_bits(s):
    s += "0" * (-len(s) % 8)
    return bytes(int(s[i:i + 8], 2) for i in range(0, len(s), 8))


def loas_mux(frames):
    return b"".join(bytes([0x56, 0xE0 | (len(f) >> 8), len(f) & 255]) + f for f in frames)


def config_bits(asc):
    """The StreamMuxConfig of a LOAS stream (an AudioSpecificConfig of `asc` bytes, or as a bit string): audioMuxVersion 0, allStreamsSameTimeFraming 1,
    6 + 4 + 3 zero bits, the ASC, frameLengthType 0, latmBufferFullness 0xff, no other data, no CRC."""
    asc_bits = asc if isinstance(asc, str) else bit_string(asc)
    return "0" + "1" + "000000" + "0000" + "000" + asc_bits + "000" + "11111111" + "0" + "0"


def loas_cases(b):
    out = []
    frames = loas_frames(b.loas)
    assert len(frames) == 10 and bit_string(frames[0])[0] == "0"

    assert bit_string(frames[0])[1:1 + len(config_bits(bytes([0x12, 0x10])))] == config_bits(bytes([0x12, 0x10])), "layout of the base LOAS frame"

    def with_config(index, asc):
        new = list(frames)
        body = bit_string(frames[index])
        assert body[0] == "1"
        new[index] = from_bits("0" + config_bits(asc) + body[1:])
        return loas_mux(new)
    out.append(Case("loas", "plain", "loas", b.loas, "the base stream", ["rule:loas"]))
    out.append(Case("loas", "same-config-frame-3", "loas", with_config(3, bytes([0x12, 0x10])), "frame 3 repeats the first StreamMuxConfig", ["rule:loas"]))
    out.append(Case("loas", "config-48k-frame-3", "loas", with_config(3, bytes([0x11, 0x90])), "frame 3 carries a StreamMuxConfig for 48 kHz stereo", ["rule:loas"]))
    out.append(Case("loas", "config-48k-frame-9", "loas", with_config(9, bytes([0x11, 0x90])), "the last frame carries a StreamMuxConfig for 48 kHz stereo", ["rule:loas"]))
    out.append(Case("loas", "config-mono-frame-3", "loas", with_config(3, bytes([0x12, 0x08])), "frame 3 carries a StreamMuxConfig for mono", ["rule:loas"]))
    zeroed = list(frames)
    zeroed[5] = bytes(len(frames[5]))
    out.append(Case("loas", "zeroed-frame-5", "loas", loas_mux(zeroed), "frame 5 is all zeros (a zero StreamMuxConfig)", ["rule:loas"]))
    # complete frames that cannot be read as access units, as frame 5 of ten (the table counts them, the read that reaches one fails 'fmt?')
    cfg_a = config_bits(bytes([0x12, 0x10]))
    body5 = bit_string(frames[5])

    def with_length(bits, at, length):
        return bits[:at] + "{:08b}".format(length) + bits[at + 8:]
    bad = (
        ("cfg-cut", from_bits("0" + cfg_a[:19]), "a StreamMuxConfig cut after 19 bits"),
        ("cfg-no-length", from_bits("0" + cfg_a), "a StreamMuxConfig without a payload length"),
        ("same-no-length", from_bits("1"), "'same configuration' and nothing else"),
        ("frame-length-type-1", from_bits("0" + cfg_a[:-13] + "001" + cfg_a[-10:] + "00001000" + "0" * 64), "frameLengthType 1"),
        ("num-program-1", from_bits("0" + cfg_a[:8] + "0001" + cfg_a[12:] + "00001000" + "0" * 64), "numProgram 1"),
        ("empty", b"", "an empty frame"),
        ("length-escape", from_bits("1" + "11111111" * 3 + "0" * 40), "a payload length of 255 + 255 + 255 + the bits that are left"),
        ("over-long-same", from_bits("1" + with_length(body5[1:], 0, 200)), "a payload length of 200 bytes in a frame that holds fewer"),
        ("over-long-new-config", from_bits("0" + config_bits(bytes([0x11, 0x90])) + "{:08b}".format(200) + "0" * 64), "another configuration and a payload length of 200 bytes in a frame that holds fewer"),
    )
    for name, body, note in bad:
        new = list(frames)
        new[5] = body
        out.append(Case("loas", "bad-" + name, "loas", loas_mux(new), "frame 5 is " + note, ["rule:loas"]))
    new = list(frames[:5]) + [from_bits("1" + with_length(body5[1:], 0, 200))]
    out.append(Case("loas", "bad-last-frame", "loas", loas_mux(new), "the last frame (frame 5) has a payload length of 200 bytes and holds fewer", ["rule:loas"]))
    for name, body, note in (("same", from_bits("1" + "00000000"), "'same configuration' and a payload length of 0"), ("cfg", from_bits("0" + cfg_a + "00000000"), "the first configuration again and a payload length of 0")):
        new = list(frames)
        new[5] = body
        out.append(Case("loas", "zero-length-" + name, "loas", loas_mux(new), "frame 5 has " + note, ["rule:loas"]))
    new = list(frames)
    new[0] = from_bits("0" + cfg_a + "{:08b}".format(250) + bit_string(frames[0])[1 + len(cfg_a) + 8:])
    out.append(Case("loas", "first-frame-over-long", "loas", loas_mux(new), "the first frame has a payload length of 250 bytes and holds fewer", ["rule:loas"]))
    # a frame that runs past the end of the file is no frame, whatever it carries
    changed = with_config(5, bytes([0x11, 0x90]))
    cut_at = sum(3 + len(f) for f in frames[:5]) + 3
    out.append(Case("loas", "cut-frame-new-config", "loas", changed[:cut_at + len(frames[5]) // 2], "the file ends inside frame 5, which carries another configuration", ["rule:loas"]))
    out.append(Case("loas", "cut-frame-cfg-cut", "loas", changed[:cut_at + 3], "the file ends inside the configuration of frame 5", ["rule:loas"]))
    two = bytes.fromhex("56e01b200012101fe0a0383838383838383838383838383838383838383856e01b200011881fe6403838383838383838383838383838383838383838")
    out.append(Case("loas", "two-frames-second-new-config-over-long", "loas", two, "a 60-byte stream: the second frame carries another configuration and a payload length it cannot hold", ["rule:loas"]))
    return out


# ---- in-band program config elements (the matrix mixdown of a 5.1 stream) --------------------------------------------------------


def _mp4_boxes(d, lo=0, hi=None):
    hi = len(d) if hi is None else hi
    while lo + 8 <= hi:
        size = struct.unpack(">I", d[lo:lo + 4])[0]
        head = 8
        if size == 1:
            size, head = struct.unpack(">Q", d[lo + 8:lo + 16])[0], 16
        elif size == 0:
            size = hi - lo
        yield d[lo + 4:lo + 8], lo + head, lo + size
        lo += size


def _mp4_find(d, path, lo=0, hi=None):
    for kind, start, end in _mp4_boxes(d, lo, hi):
        if kind == path[0]:
            found = (start, end) if len(path) == 1 else _mp4_find(d, path[1:], start, end)
            if found:
                return found
    return None


def mp4_access_units(d):
    """(AudioSpecificConfig, access units) of the first track of an MP4 file (`stsz`, `stsc`, `stco`, `esds`)."""
    lo, hi = _mp4_find(d, [b"moov", b"trak", b"mdia", b"minf", b"stbl"])

    def box(kind):
        start, end = _mp4_find(d, [kind], lo, hi)
        return d[start:end]
    stsz, stsc, stco, stsd = box(b"stsz"), box(b"stsc"), box(b"stco"), box(b"stsd")
    count, fixed = struct.unpack(">I", stsz[8:12])[0], struct.unpack(">I", stsz[4:8])[0]
    sizes = [fixed] * count if fixed else list(struct.unpack(">%dI" % count, stsz[12:12 + 4 * count]))
    runs = [struct.unpack(">III", stsc[8 + 12 * i:20 + 12 * i]) for i in range(struct.unpack(">I", stsc[4:8])[0])]
    chunks = list(struct.unpack(">%dI" % struct.unpack(">I", stco[4:8])[0], stco[8:]))
    units, index = [], 0
    for number, offset in enumerate(chunks, 1):
        for _ in range([r for r in runs if r[0] <= number][-1][1]):
            if index < count:
                units.append(d[offset:offset + sizes[index]])
                offset += sizes[index]
                index += 1

    def descriptor(at):
        tag, at, length = stsd[at], at + 1, 0
        while True:
            byte, at = stsd[at], at + 1
            length = length << 7 | byte & 0x7F
            if not byte & 0x80:
                return tag, at, length
    at = stsd.index(b"esds") + 8
    _, at, _ = descriptor(at)
    at += 3
    _, at, _ = descriptor(at)
    at += 13
    _, at, length = descriptor(at)
    return stsd[at:at + length], units


def pce_bits(start, front=(), side=(), back=(), lfe=(), mono=None, stereo=None, matrix=None, sfi=5, comment=b""):
    """A program config element (its 4-bit instance tag first, as an in-band element has it) that starts `start` bits into its access unit (the byte
    alignment before the comment counts from the access unit).  `front`, `side`, `back`: (is_cpe, tag); `matrix`: (index, pseudo surround)."""
    s = "{:04b}{:02b}{:04b}{:04b}{:04b}{:04b}{:02b}{:03b}{:04b}".format(0, 1, sfi, len(front), len(side), len(back), len(lfe), 0, 0)
    s += ("1{:04b}".format(mono) if mono is not None else "0") + ("1{:04b}".format(stereo) if stereo is not None else "0")
    s += "1{:02b}{:01b}".format(*matrix) if matrix is not None else "0"
    for entries in (front, side, back):
        s += "".join("{:01b}{:04b}".format(*e) for e in entries)
    s += "".join("{:04b}".format(t) for t in lfe)
    s += "0" * (-(start + len(s)) % 8)
    return s + "{:08b}".format(len(comment)) + bit_string(comment)


def latm_length(n):
    return "11111111" * (n // 255) + "{:08b}".format(n % 255)


def loas_of_units(asc, units):
    """LOAS of access units given as bit strings (a whole number of bytes each is not needed): the first frame carries the StreamMuxConfig."""
    frames = []
    for i, bits in enumerate(units):
        bits += "0" * (-len(bits) % 8)
        frames.append(from_bits(("0" + config_bits(asc) if i == 0 else "1") + latm_length(len(bits) // 8) + bits))
    return loas_mux(frames)


def adts_of_units(sfi, channel_config, units):
    out = b""
    for bits in units:
        body = from_bits(bits)
        header = (0xFFF << 44) | (1 << 40) | (1 << 38) | (sfi << 34) | (channel_config << 30) | ((len(body) + 7) << 13) | (0x7FF << 2)
        out += header.to_bytes(7, "big") + body
    return out


def pcemix_cases(b):
    out = []
    base = "fresh/acc_32000_6ch_chirp (this pack)"
    asc, units = mp4_access_units(b.pack_file("fresh/acc_32000_6ch_chirp"))
    assert asc == bytes([0x12, 0xB0]) and len(units) == 17 and [len(u) for u in units[:5]] == [19, 468, 380, 235, 254]
    units = [bit_string(u) for u in units[:10]]
    five_one = dict(front=[(0, 0), (1, 0)], back=[(1, 1)], lfe=[0])
    inband = "101"      # ID_PCE; the element's own 4-bit tag follows

    def insert(index, at, **fields):
        new = list(units)
        new[index] = units[index][:at] + inband + pce_bits(at + 3, **fields) + units[index][at:]
        return new

    def case(name, ext, data, note):
        out.append(Case("pcemix", name, ext, data, note, ["rule:pcemix"], base=base))
    for index in range(4):
        case("idx%d" % index, "loas", loas_of_units(asc, insert(3, 0, matrix=(index, 0), **five_one)), "access unit 3 starts with a PCE of the 5.1 shape and matrix_mixdown_idx %d" % index)
    case("idx0-pseudo", "loas", loas_of_units(asc, insert(3, 0, matrix=(0, 1), **five_one)), "as idx0 with pseudo_surround_enable 1")
    case("first-au-idx3", "loas", loas_of_units(asc, insert(0, 0, matrix=(3, 0), **five_one)), "the first access unit starts with a PCE with matrix_mixdown_idx 3")
    new = insert(3, 0, matrix=(3, 0), **five_one)
    new[7] = units[7][:0] + inband + pce_bits(3, matrix=(1, 0), **five_one) + units[7]
    case("index-replaced", "loas", loas_of_units(asc, new), "a PCE with index 3 in unit 3, one with index 1 in unit 7")
    new = insert(3, 0, matrix=(3, 0), **five_one)
    new[6] = inband + pce_bits(3, **five_one) + units[6]
    case("pce-without-index-keeps-it", "loas", loas_of_units(asc, new), "a PCE with index 3 in unit 3, one without a matrix mixdown in unit 6")
    new = insert(3, 0, mono=0, stereo=1, **five_one)
    case("mono-stereo-tags-only", "loas", loas_of_units(asc, new), "a PCE with mono and stereo mixdown element tags and no matrix mixdown")
    new = list(units)
    new[3] = inband + pce_bits(3, matrix=(3, 0), **five_one) + "111"
    case("in-rejected-au", "loas", loas_of_units(asc, new), "unit 3 is a PCE with index 3 and the end of the block (no channel elements: the decoder rejects it), the units behind it are intact")
    # inside unit 3 (element starts, from the decoder's own parse: SCE 0, CPE 418, CPE 1078, LFE 1796)
    for at in (418, 1078, 1796):
        case("inside-au-%d" % at, "loas", loas_of_units(asc, insert(3, at, matrix=(3, 0), **five_one)), "a PCE with index 3 between the elements of unit 3, %d bits in" % at)
    # a PCE in the configuration (channelConfiguration 0) has no such effect
    ascbits = "00010" + "0101" + "0000" + "000"
    ascbits += pce_bits(len(ascbits), matrix=(3, 0), **five_one)
    case("config-pce-idx3", "loas", loas_of_units(ascbits, units), "channelConfiguration 0 with a PCE in the AudioSpecificConfig that has matrix_mixdown_idx 3: no effect")
    # ADTS: the PCE of channelConfiguration 0 sits in the first access unit
    case("adts-config0-idx3", "aac", adts_of_units(5, 0, insert(0, 0, matrix=(3, 0), **five_one)), "ADTS, channel_configuration 0, a PCE with index 3 in the first access unit")
    case("adts-config6-idx3", "aac", adts_of_units(5, 6, insert(3, 0, matrix=(3, 0), **five_one)), "ADTS, channel_configuration 6, a PCE with index 3 in unit 3")
    # other layouts keep their matrices
    for fixture, name, label in (("fresh/acc_22050_8ch_mix", "layout-7-1", "a 7.1 stream (channelConfiguration 7)"), ("fresh/acc_24000_5ch_noise", "layout-5-0", "a 5.0 stream")):
        other_asc, other_units = mp4_access_units(b.pack_file(fixture))
        other = [bit_string(u) for u in other_units[:6]]
        other[3] = inband + pce_bits(3, front=[(0, 0)], matrix=(3, 0)) + other[3]
        out.append(Case("pcemix", name + "-idx3", "loas", loas_of_units(other_asc, other), "%s with a PCE of matrix_mixdown_idx 3 in unit 3: its matrix does not change" % label, ["rule:pcemix"], base=fixture + " (this pack)"))
    return out


# ---- sample group descriptions, iTunSMPB frame counts ------------------------------------------------------------------------------


def with_stbl_edit(fixture, data, edit):
    """`data` (the file of the pack entry `fixture`) with `edit(stbl)` applied to its sample table, chunk offsets moved with the `mdat`."""
    nodes, tail = M.parse_mp4(data)
    assert not tail
    edit(moov(nodes).find(b"trak", b"mdia", b"minf", b"stbl"))
    return M.mp4_bytes(nodes, fix_offsets_from=M.first_mdat_start(data))


def sgpd_cases(b):
    out = []
    fixture = "afconvert/lc_mp4_44s"
    data = b.pack_file(fixture)
    nodes, _ = M.parse_mp4(data)
    stbl = moov(nodes).find(b"trak", b"mdia", b"minf", b"stbl")
    assert [k.typ for k in stbl.kids] == [b"stsd", b"sgpd", b"sbgp", b"stts", b"stsc", b"stsz", b"stco"] and stbl.find(b"sgpd").data.hex() == "01000000726f6c6c0000000200000001ffff"
    base = fixture + " (this pack)"

    def one(name, payload, note):
        def edit(stbl):
            stbl.find(b"sgpd").data = bytes.fromhex(payload.replace(" ", ""))
        out.append(Case("sgpd", name, "m4a", with_stbl_edit(fixture, data, edit), note, ["rule:sgpd"], base=base))

    def several(name, payloads, note):
        def edit(stbl):
            at = [k.typ for k in stbl.kids].index(b"sgpd")
            stbl.kids[at:at + 1] = [M.Node(b"sgpd", bytes.fromhex(p)) for p in payloads]
        out.append(Case("sgpd", name, "m4a", with_stbl_edit(fixture, data, edit), note, ["rule:sgpd"], base=base))
    negative, positive = "01000000726f6c6c0000000200000001ffff", "01000000726f6c6c00000002000000010001"
    # headers: a payload of 4 bytes is all of a version and flags, any other must hold the grouping type, the length fields its version has and the entry count
    one("version-flags-only", "00000000", "an sgpd of the version and flags and nothing else: no description, the file opens")
    one("three-bytes", "000000", "an sgpd of 3 bytes: the open fails 'wht?'")
    one("grouping-type-cut", "0000000072 6f", "an sgpd that ends inside the grouping type: the open fails")
    one("count-missing", "00000000 78787878", "an sgpd of another type that ends before its entry count: the open fails")
    one("other-type-count-only", "00000000 78787878 00000007", "an sgpd of another type that holds its header and none of the 7 entries it announces: opens")
    one("v1-count-missing", "01000000 78787878 00000002", "a version 1 sgpd of another type that ends before its entry count: the open fails")
    one("v2-count-missing", "02000000 78787878 00000002 00000001 000000", "a version 2 sgpd of another type that ends inside its entry count: the open fails")
    one("v2-roll-truncated", "02000000726f6c6c0000000200000001ffff", "a version 2 roll description cut where a version 1 one is complete (default length, index, then 2 bytes where the count belongs): the open fails")
    # roll entries
    one("roll-negative", "00000000726f6c6c00000001ffff", "version 0, one negative roll distance: gate on")
    one("roll-non-negative", "00000000726f6c6c000000010001", "version 0, one positive roll distance: no gate")
    one("roll-entry-missing", "00000000726f6c6c00000002ffff", "version 0, 2 entries announced and 1 held: the open fails")
    one("roll-six-entries-missing", "00000000726f6c6c00000007ffff", "version 0, 7 entries announced and 1 held (12 bytes missing): the open fails")
    one("roll-seven-entries-missing", "00000000726f6c6c00000008ffff", "version 0, 8 entries announced and 1 held (14 bytes missing): the entries are not read, gate on")
    one("roll-count-huge", "00000000726f6c6cffffffff", "version 0, a count of 2^32 - 1 and no entry: gate on")
    one("roll-count-huge-positive", "00000000726f6c6cfffffff00001", "version 0, a count of 2^32 - 16 and a positive entry: not read, gate on")
    one("roll-positive-ends-it", "00000000726f6c6c000000030001", "version 0, 3 announced, the first held entry positive: no gate, no failure")
    one("roll-length-4", "01000000726f6c6c0000000400000001ffffffff", "version 1, default length 4: no usable description")
    one("roll-length-0-entries", "01000000726f6c6c000000000000000100000002ffff", "version 1, default length 0 and a length in front of the entry: gate on")
    one("roll-length-0-entry-length-4", "01000000726f6c6c000000000000000100000004ffff0000", "version 1, default length 0 and an entry of 4 bytes: no usable description")
    one("roll-length-0-truncated", "01000000726f6c6c000000000000000100000002", "version 1, default length 0 and an entry without its distance: the open fails")
    one("roll-version-3", "03000000726f6c6c000000020000000000000001ffff", "version 3: read as version 2, gate on")
    # prol: read like roll while the distances are positive, never a gate
    one("prol-entries-missing", "0000000070726f6c00000002", "a pre-roll description (prol) with 2 entries announced and none held: the open fails")
    one("prol-positive-missing", "0000000070726f6c000000020001", "prol with a positive first entry and a missing second one: the open fails")
    one("prol-negative-missing", "0000000070726f6c00000002ffff", "prol with a negative first entry and a missing second one: opens")
    one("prol-zero-missing", "0000000070726f6c000000020000", "prol with a zero first entry and a missing second one: opens")
    several("two-roll-positive-negative", [positive, negative], "two roll descriptions, the first with a positive distance, the second negative: gate on")
    several("two-roll-negative-positive", [negative, positive], "two roll descriptions, the first negative, the second positive: gate on")
    several("good-then-cut", [negative, "00000000 78787878".replace(" ", "")], "a good roll description and behind it an sgpd of another type that ends before its count: the open fails")
    several("cut-then-good", ["00000000 78787878".replace(" ", ""), negative], "an sgpd of another type that ends before its count and behind it a good roll description: the open fails")

    def sbgp(stbl):
        stbl.find(b"sbgp").data = bytes.fromhex("0000")
    out.append(Case("sgpd", "sbgp-cut", "m4a", with_stbl_edit(fixture, data, sbgp), "the sample-to-group map cut to 2 bytes: nobody's business, the gate stays on", ["rule:sgpd"], base=base))
    return out


def smpb_cases(b):
    out = []
    fixture = "fresh/len44_00001"
    data = b.pack_file(fixture)
    at = data.find(b"iTunSMPB") + 24
    assert data[at:at + 44] == b" 00000000 00000840 000003BF 0000000000000001", data[at:at + 44]
    base = fixture + " (this pack)"
    for name, valid, note in (
        ("2p53-plus-1", (1 << 53) + 1, "valid frames 2^53 + 1: the length is the nearest double, 2^53"),
        ("2p53-plus-3", (1 << 53) + 3, "valid frames 2^53 + 3: the length is 2^53 + 4"),
        ("2p54-plus-5", (1 << 54) + 5, "valid frames 2^54 + 5: the length is 2^54 + 4"),
        ("ordinary-large", 0x00E00000000007B2, "valid frames 63050394783188914: the length is 63050394783188912"),
        ("2p63-minus-512", (1 << 63) - 0x200, "valid frames 2^63 - 512: rounds up to 2^63 and saturates at 2^63 - 1"),
        ("int64-max", (1 << 63) - 1, "valid frames 2^63 - 1"),
        ("2p63", 1 << 63, "valid frames 2^63: no iTunSMPB, the default priming"),
        ("2p52-plus-1", (1 << 52) + 1, "valid frames 2^52 + 1: an odd count from 2^52 on rounds up (the length is int64(double + 0.5)): 2^52 + 2"),
        ("2p52-plus-5149", (1 << 52) + 5149, "valid frames 2^52 + 5149: the length is 2^52 + 5150"),
        ("2p53-minus-1", (1 << 53) - 1, "valid frames 2^53 - 1: the length is 2^53"),
    ):
        text = data[:at + 28] + "{:016X}".format(valid).encode() + data[at + 44:]
        assert len(text) == len(data)
        out.append(Case("smpb", name, "m4a", text, note, ["rule:smpb"], base=base))
    # the fields are read at fixed places (priming 9.., remainder 18.., valid frames 27..), whatever stands at the separators
    standard = b" 00000000 00000100 00000123 0000000000002000"
    assert len(standard) == 44

    def text_with(edits, insert=None):
        t = bytearray(standard)
        for place, byte in edits.items():
            t[place] = byte
        if insert:
            t[insert[0]:insert[0]] = insert[1]
        return data[:at] + bytes(t) + data[at + 44:]
    for place, label in ((9, "priming"), (18, "remainder"), (27, "valid frames")):
        for name, byte, what in (("bang", 0x21, "'!'"), ("nul", 0x00, "a NUL"), ("ff", 0xFF, "0xFF"), ("digit", 0x30, "a hex digit '0'"), ("a", 0x41, "a hex digit 'A'"),
                                 ("plus", 0x2B, "'+'"), ("minus", 0x2D, "'-'"), ("tab", 0x09, "a tab")):
            out.append(Case("smpb", "sep%d-%s" % (place, name), "m4a", text_with({place: byte}),
                            "the separator in front of the %s field (offset %d) is %s: the field is read from there all the same (as a number: a sign, a digit, junk = 0)" % (label, place, what), ["rule:smpb"], base=base))
    out.append(Case("smpb", "lead-junk", "m4a", text_with({3: 0x21, 4: 0x00}), "junk inside the first 9 characters, which are not looked at", ["rule:smpb"], base=base))
    out.append(Case("smpb", "priming-9-digits", "m4a", text_with({}, (11, b"0")), "a priming of 9 digits (every later field moves one place: read from its fixed place)", ["rule:smpb"], base=base))
    out.append(Case("smpb", "no-whitespace-between", "m4a", text_with({9: 0x30, 18: 0x30, 27: 0x30}), "no white space between the fields", ["rule:smpb"], base=base))
    return out


# ---- several udta atoms, CAF layered descriptions ----------------------------------------------------------------------------------


def udta_cases(b):
    out = []
    fixture = "fresh/len44_00001"
    data = b.pack_file(fixture)
    nodes, _ = M.parse_mp4(data)
    udta0 = moov(nodes).find(b"udta")
    meta0, = [k for k in udta0.kids if k.typ == b"meta"]
    ilst0, hdlr0 = meta0.find(b"ilst"), meta0.find(b"hdlr")
    old = b" 00000000 00000840 000003BF 0000000000000001"
    assert old in ilst0.data
    base = fixture + " (this pack)"

    def ilst_text(text):
        return ilst0.data.replace(old, text)

    def ilst(valid):
        return ilst_text(b" 00000000 00000840 000003BF " + b"%016X" % valid)

    def meta_of(ilst_data, hdlr=hdlr0.data):
        return M.Node(b"meta", b"", [M.Node(b"hdlr", hdlr), M.Node(b"ilst", ilst_data)], meta0.pre)

    def udta(*kids):
        return M.Node(b"udta", b"", list(kids))

    def case(name, udtas, note):
        def edit(moov_node):
            at = [k.typ for k in moov_node.kids].index(b"udta")
            moov_node.kids[at:at + 1] = udtas
        nodes2, _ = M.parse_mp4(data)
        edit(moov(nodes2))
        out.append(Case("udta", name, "m4a", M.mp4_bytes(nodes2, fix_offsets_from=M.first_mdat_start(data)), note, ["rule:udta"], base=base))
    a, b2, c = 0x1000, 0x2000, 0x3000
    zero = ilst(0)      # valid frames 0: no usable text
    no_mdir = bytearray(hdlr0.data)
    at = no_mdir.index(b"mdir")
    no_mdir[at:at + 4] = b"xxxx"
    case("one", [udta(meta_of(ilst(a)))], "one udta with a tag of 4096 frames")
    case("two-last-wins", [udta(meta_of(ilst(a))), udta(meta_of(ilst(b2)))], "two udta atoms with tags of 4096 and 8192 frames: the last one counts")
    case("three-last-wins", [udta(meta_of(ilst(a))), udta(meta_of(ilst(b2))), udta(meta_of(ilst(c)))], "three udta atoms: the last tag counts")
    case("empty-last", [udta(meta_of(ilst(a))), udta()], "a tag, then an empty udta: the tag stays")
    case("other-tag-last", [udta(meta_of(ilst(a))), udta(meta_of(ilst(b2).replace(b"iTunSMPB", b"iTunXXXX")))], "a tag, then a udta whose item is another one: the tag stays")
    case("two-meta-in-one-udta", [udta(meta_of(ilst(a)), meta_of(ilst(b2)))], "one udta with two meta atoms: only the first (the first child) is read")
    case("two-meta-then-udta", [udta(meta_of(ilst(a)), meta_of(ilst(b2))), udta(meta_of(ilst(c)))], "a udta with two meta atoms, then another udta: the last udta's tag counts")
    case("zero-last", [udta(meta_of(ilst(a))), udta(meta_of(zero))], "a tag, then a udta whose text is no usable tag (0 valid frames): the tag stays")
    case("zero-first", [udta(meta_of(zero)), udta(meta_of(ilst(a)))], "a udta whose text is no usable tag, then a tag: the tag counts")
    case("first-item-unusable", [udta(meta_of(zero + ilst(a)))], "one meta whose first iTunSMPB item is unusable and a second one is fine: the first decides, no tag")
    case("later-udta-item-unusable", [udta(meta_of(ilst(a))), udta(meta_of(zero + ilst(b2)))], "a tag, then a udta whose first item is unusable (its second would be fine): the tag stays")
    case("no-mdir-last", [udta(meta_of(ilst(a))), udta(meta_of(ilst(b2), bytes(no_mdir)))], "a tag, then a udta whose meta has no mdir handler: the tag stays")
    case("free-before-meta-last", [udta(meta_of(ilst(a))), udta(M.Node(b"free", bytes(8)), meta_of(ilst(b2)))], "a tag, then a udta whose first child is a free atom: its meta is not looked at")
    return out


def ldsc_cases(b):
    out = []
    chunks = []
    pos = 8
    while pos + 12 <= len(b.caf):
        size = struct.unpack(">q", b.caf[pos + 4:pos + 12])[0]
        chunks.append([b.caf[pos:pos + 4], b.caf[pos + 12:pos + 12 + size]])
        pos += 12 + size
    assert [c[0] for c in chunks] == [b"desc", b"kuki", b"chan", b"info", b"pakt", b"free", b"data"]

    def asbd(rate, fmt, fpp, channels):
        return struct.pack(">d4s5I", rate, fmt, 0, 0, fpp, channels, 0)

    def layers(*entries):
        return b"".join(asbd(*e) + struct.pack(">I", 0x00650002) for e in entries)

    def join(parts):
        return b.caf[:8] + b"".join(c[0] + struct.pack(">q", len(c[1])) + c[1] for c in parts)

    def case(name, ldscs, note, keep_kuki=False, drop_desc=False, at=2, base=None, source=None):
        parts = [[c[0], c[1]] for c in (source or chunks) if (keep_kuki or c[0] != b"kuki") and not (drop_desc and c[0] == b"desc")]
        for i, body in enumerate(ldscs):
            parts.insert(at + i, [b"ldsc", body])
        out.append(Case("ldsc", name, "caf", join(parts), note, ["rule:ldsc"], base=base))
    he = (44100.0, b"aach", 2048, 2)
    core = (44100.0, b"aac ", 1024, 2)
    case("aach-no-cookie", [layers(he)], "an ldsc whose first layer is 'aach' 44100 Hz 2048 frames, no cookie: HE-AAC (Apple opens it as such)")
    case("aacp-no-cookie", [layers((44100.0, b"aacp", 2048, 2))], "as above with 'aacp'")
    case("aach-plain-cookie", [layers(he)], "'aach' in the ldsc over the plain AAC-LC cookie: the open fails '!dat'", keep_kuki=True)
    case("aach-mono-16000", [layers((16000.0, b"aach", 2048, 1))], "the first layer decides rate, frames per packet and channels: 16 kHz mono")
    case("aach-fpp-1024", [layers((44100.0, b"aach", 1024, 2))], "'aach' with 1024 frames per packet")
    case("35-bytes-ignored", [layers(he)[:35]], "an ldsc of 35 bytes (less than a stream description and a layout tag): ignored")
    case("two-first-counts", [layers(core), layers(he)], "two ldsc chunks: the first one counts (here a plain 'aac ' layer)")
    case("two-aach-first", [layers(he), layers(core)], "two ldsc chunks: the first one counts (here 'aach')")
    case("after-pakt", [layers(he)], "the ldsc behind the packet table", at=5)
    case("without-desc", [layers(he)], "an ldsc and no desc: 'dta?'", drop_desc=True)
    for fixture, name in (("afconvert/he1_caf_44s", "he1-without-cookie"), ("afconvert/he2_caf_44s", "he2-without-cookie")):
        data = b.pack_file(fixture)
        src, pos = [], 8
        while pos + 12 <= len(data):
            size = struct.unpack(">q", data[pos + 4:pos + 12])[0]
            src.append([data[pos:pos + 4], data[pos + 12:pos + 12 + size]])
            pos += 12 + size
        parts = [c for c in src if c[0] != b"kuki"]
        out.append(Case("ldsc", name, "caf", b.caf[:8] + b"".join(c[0] + struct.pack(">q", len(c[1])) + c[1] for c in parts),
                        "the HE-AAC file %s without its cookie (the ldsc names 'aach' / 'aacp'): Apple decodes it as HE-AAC" % fixture, ["rule:ldsc"], base=fixture + " (this pack)"))
    return out


def cafpakt_cases(b):
    out = []
    pos = 8
    chunks = []
    while pos + 12 <= len(b.caf):
        size = struct.unpack(">q", b.caf[pos + 4:pos + 12])[0]
        chunks.append([b.caf[pos:pos + 4], b.caf[pos + 12:pos + 12 + size]])
        pos += 12 + size
    for name, valid, note in (
        ("2p52-plus-5149", (1 << 52) + 5149, "valid frames 2^52 + 5149: odd, rounds up to 2^52 + 5150"),
        ("2p52-plus-1", (1 << 52) + 1, "valid frames 2^52 + 1: 2^52 + 2"),
        ("2p52-plus-2", (1 << 52) + 2, "valid frames 2^52 + 2: exact"),
        ("2p53-minus-1", (1 << 53) - 1, "valid frames 2^53 - 1: 2^53"),
        ("2p53-plus-1", (1 << 53) + 1, "valid frames 2^53 + 1: the double is 2^53"),
        ("2p53-plus-3", (1 << 53) + 3, "valid frames 2^53 + 3: 2^53 + 4"),
        ("2p62-plus-7", (1 << 62) + 7, "valid frames 2^62 + 7: 2^62"),
        ("int64-max", (1 << 63) - 1, "valid frames 2^63 - 1: saturates at 2^63 - 1"),
        ("below-2p52", (1 << 51) + 12345, "valid frames 2^51 + 12345: exact"),
    ):
        parts = [[c[0], bytes(c[1])] for c in chunks]
        for c in parts:
            if c[0] == b"pakt":
                c[1] = c[1][:8] + struct.pack(">q", valid) + c[1][16:]
        out.append(Case("cafpakt", name, "caf", b.caf[:8] + b"".join(c[0] + struct.pack(">q", len(c[1])) + c[1] for c in parts), note, ["rule:cafpakt"]))
    return out


def short_cases(b):
    out = []
    fixture = "rule/damage-stco-three-chunks"
    data = b.pack_file(fixture)
    paths = {"stbl": [b"moov", b"trak", b"mdia", b"minf", b"stbl"], "minf": [b"moov", b"trak", b"mdia", b"minf"], "mdia": [b"moov", b"trak", b"mdia"],
             "trak": [b"moov", b"trak"], "moov": [b"moov"]}
    assert _mp4_find(data, paths["stbl"])[1] - _mp4_find(data, paths["stbl"])[0] > 28
    for name, amounts in (("stbl", (1, 4, 12, 20, 21)), ("minf", (1, 4, 12, 20)), ("mdia", (1, 4, 12, 21)), ("trak", (1, 4, 5, 12, 21)), ("moov", (1, 4, 12))):
        start = _mp4_find(data, paths[name])[0] - 8
        for k in amounts:
            d = bytearray(data)
            d[start:start + 4] = struct.pack(">I", struct.unpack(">I", data[start:start + 4])[0] - k)
            out.append(Case("short", "%s-minus-%d" % (name, k), "m4a", bytes(d),
                            "the size field of the %s atom is %d byte%s short: its last child (the chunk offsets of the sample table, 28 bytes) runs past it" % (name, k, "" if k == 1 else "s"),
                            ["rule:short"], base=fixture + " (this pack)"))
    return out


def all_cases(b):
    cases = []
    for group in (brand_cases, ftyp_cases, atom_cases, size_cases, trun_cases, caf_he_cases, asc_cases, damage_cases, loas_cases, pcemix_cases, sgpd_cases, smpb_cases, udta_cases, ldsc_cases, cafpakt_cases, short_cases):
        cases += group(b)
    keys = [c.key for c in cases]
    assert len(keys) == len(set(keys)), [k for k, n in collections.Counter(keys).items() if n > 1]
    assert all(len(c.data) <= M.MAX_FILE_BYTES for c in cases)
    return cases


# ---- extend ----------------------------------------------------------------------------------------------------------------------


def to_cand(c):
    return M.Cand(c.key, c.filename, c.data, "rule-" + c.group, {"kind": GROUP_ORIGIN, "group": c.group, "base": c.base, "edit": c.note}, c.tags)


# Inputs whose recordings at 4096-frame reads differ from the port's: where the packet table is damaged (a chunk the table does not reach, a zero-size sample
# in front of other samples) AudioToolbox delivers 6080 frames without a failure for the reads of that size, the port fails 'bada' as it does at 512, 513
# and 100 frames.  They are replayed at 512 frames, where the port is exact, and not at the other sizes.
READSIZES_SKIP = {"rule/damage-stco-count-1", "rule/damage-co64-count-1", "rule/trun-zero-size-sample-2", "rule/damage-co64-offset-max-chunk-1",
                  "rule/damage-co64-offset-max-1-chunk-1", "rule/damage-co64-offset-2-63-chunk-1",
                  "rule/short-trak-minus-5"}


def extend(args):
    out = os.path.abspath(args.dir)
    manifest, parts = M.load_pack(out)
    probe = os.path.abspath(args.probe)
    work = os.path.abspath(args.work)
    os.makedirs(work, exist_ok=True)
    bases = Bases(os.path.abspath(args.reader), manifest, parts)
    # the cases the manifest has stay as they are (and must still be what the generators make); the others are added
    have = {e["id"]: e for e in manifest["files"] if e["id"].startswith("rule/")}
    drift = [c.key for c in all_cases(bases) if c.key in have and have[c.key]["sha256"] != M.sha256_bytes(c.data)]
    if drift:
        M.fail("the generators no longer make the recorded inputs: %s" % ", ".join(drift[:10]))
    cases = [c for c in all_cases(bases) if c.key not in have]
    if not cases:
        M.fail("every case is in the manifest already: nothing to add")
    cands = [to_cand(c) for c in cases]
    case_of = {c.key: c for c in cases}
    paths = M.stage(cands, os.path.join(work, "stage"))
    recs = M.record_files(probe, paths)
    bad = [k for k, r in recs.items() if "crash" in r]
    if bad:
        M.fail("recorder crash / nondeterminism: %s" % ", ".join("%s=%s" % (k, recs[k]["crash"]) for k in bad[:20]))
    for c in cands:
        c.rec = recs[c.key]
        c.features = set(c.tags) | M.structure_tags(c) | M.outcome_tags(c.rec) | {"family:" + c.family, "set:rule"}
    existing = {e["sha256"]: (e["part"], e["offset"]) for e in manifest["files"]}
    # new bytes go into new parts behind the old ones
    new_parts, placed, current = [], {}, bytearray()
    first_part = len(manifest["pack"]["parts"])

    def flush():
        if current:
            name = "pack-%d.bin" % (first_part + len(new_parts))
            with open(os.path.join(out, name), "xb") as f:
                f.write(current)
            new_parts.append({"file": name, "bytes": len(current), "sha256": M.sha256_bytes(bytes(current))})
            current.clear()
    for c in sorted(cands, key=lambda c: c.key):
        if c.sha256 in existing or c.sha256 in placed:
            continue
        if current and len(current) + len(c.data) > M.MAX_PART_BYTES:
            flush()
        placed[c.sha256] = (first_part + len(new_parts), len(current))
        current.extend(c.data)
    flush()
    entries = []
    for c in cands:
        expect, reason = M.classify(c)
        if case_of[c.key].refusal and expect == "exact":
            expect, reason = "refuse_codec", case_of[c.key].refusal
        e = M.entry_of(c, expect, reason)
        e["part"], e["offset"] = existing.get(c.sha256) or placed[c.sha256]
        entries.append(e)
    manifest["files"] = sorted(manifest["files"] + entries, key=lambda e: e["id"])
    assert len({e["id"] for e in manifest["files"]}) == len(manifest["files"])
    manifest["pack"]["parts"] += new_parts
    all_bytes = b"".join(open(os.path.join(out, p["file"]), "rb").read() for p in manifest["pack"]["parts"])
    manifest["pack"]["bytes"] = len(all_bytes)
    manifest["pack"]["sha256"] = M.sha256_bytes(all_bytes)
    manifest["extended_by"] = "tools/portable/aac/make_decode_rule_fixtures.py"
    probe_sha = M.sha256_file(probe)
    if manifest["apple"].get("rule_probe_sha256", probe_sha) != probe_sha:
        manifest["apple"].setdefault("rule_probe_sha256_added", [])
        if probe_sha not in manifest["apple"]["rule_probe_sha256_added"]:
            manifest["apple"]["rule_probe_sha256_added"].append(probe_sha)
    else:
        manifest["apple"]["rule_probe_sha256"] = probe_sha
    with open(os.path.join(out, "manifest.json"), "w") as f:
        f.write(M.dumps(manifest))
    # the other read sizes of the new inputs that open, appended to readsizes.json
    by_key = {c.key: c for c in cands}
    chosen = []
    for i, e in enumerate(entries):
        if e["expect"] != "exact" or e["id"] in READSIZES_SKIP:
            continue
        sensitive = bool(e["planar512"]["error"] or e["inter512"]["error"]) or e["inter512"]["short"] >= 2
        if sensitive or i % 4 == 0:
            chosen.append(e)
    sizes = {}
    for size in M.READ_SIZES:
        results = M.record_files(probe, {e["id"]: by_key[e["id"]].path for e in chosen}, extra=["--block", str(size)])
        for e in chosen:
            if "crash" in results[e["id"]]:
                M.fail("%s at %d frames: %s" % (e["id"], size, results[e["id"]]["crash"]))
            sizes[(e["id"], size)] = results[e["id"]]
    target = os.path.join(out, "readsizes.json")
    doc = M.load_json(target)
    for e in chosen:
        doc["files"].append({"id": e["id"], "sha256": e["sha256"],
                             "planar": {str(s): M.clean_stream(sizes[(e["id"], s)]["planar"]) for s in M.READ_SIZES},
                             "inter": {str(s): M.clean_stream(sizes[(e["id"], s)]["inter"]) for s in M.READ_SIZES}})
    doc["files"].sort(key=lambda f: f["id"])
    doc["pack_sha256"] = manifest["pack"]["sha256"]
    head = {k: v for k, v in doc.items() if k != "files"}
    text = json.dumps(head, indent=1)
    lines = [json.dumps(f, separators=(",", ":")) for f in doc["files"]]
    with open(target, "w") as f:
        f.write(text[:-2] + ',\n "files": [\n  ' + ",\n  ".join(lines) + "\n ]\n}\n")
    M.summarize(manifest)
    print("added %d inputs (%d new bytes in %d parts); readsizes.json: %d files" % (len(entries), sum(p["bytes"] for p in new_parts), len(new_parts), len(doc["files"])))
    by_class = collections.Counter((e["id"].split("-")[0], e["expect"]) for e in entries)
    for k in sorted(by_class):
        print("  %-20s %-14s %d" % (k[0], k[1], by_class[k]))


def list_cases(args):
    manifest, parts = M.load_pack(os.path.abspath(args.dir))
    for c in all_cases(Bases(os.path.abspath(args.reader), manifest, parts)):
        print("%-48s %-44s %6d  %s" % (c.key, c.filename, len(c.data), c.note))


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("extend")
    e.add_argument("--probe", required=True)
    e.add_argument("--work", required=True)
    e.add_argument("--dir", required=True)
    e.add_argument("--reader", required=True)
    ls = sub.add_parser("list")
    ls.add_argument("--dir", required=True)
    ls.add_argument("--reader", required=True)
    args = ap.parse_args(argv)
    try:
        {"extend": extend, "list": list_cases}[args.cmd](args)
    except M.FixtureError as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
