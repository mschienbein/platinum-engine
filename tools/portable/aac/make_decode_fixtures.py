#!/usr/bin/env python3
"""make_decode_fixtures.py - build / verify the Platinum AAC decoder golden fixtures (tests/fixtures/aac-decode).

Every fixture is one input file and what the macOS reference did with it: the reference is the engine's own
`apple_codec::Reader` (AudioToolbox ExtAudioFile, pinned to decoder class G like the engine) driven by
`tools/portable/aac/decode_probe` (see its header). The replay tests (`tests/portable_aac_decode.rs`,
`tests/portable_aac_hostile.rs`) compare the portable `Reader` with the recordings; nothing Apple is needed there.

commands (python 3.9+; `build` and `readsizes` need macOS arm64 with the engine's toolchain, ffmpeg is not needed):

  build       --corpus CORPUS_DIR --probe decode_probe --work SCRATCH_DIR --out tests/fixtures/aac-decode
      Collects the candidate inputs (a selection from generated decoder corpora and container probes that tools outside this
      repository produced and CORPUS_DIR holds: `corpus-a/manifest.json`, `corpus-b/manifest.json`, `probes/probes.tsv` with
      `probes/probes/<id>`; fresh files encoded with the engine's Apple writer and with afconvert from generated tones, and
      deterministic container/stream mutations of those), records every candidate twice through decode_probe (the two recordings
      must be identical), classifies each (`expect`), selects within the byte and audio-time budgets, and writes pack-N.bin and
      manifest.json.  Refuses to overwrite an existing --out.  The container-rule cases are appended by
      `make_decode_rule_fixtures.py extend`, which needs no corpora.
  readsizes   --probe decode_probe --work SCRATCH_DIR --dir tests/fixtures/aac-decode
      Records the 100-, 513- and 4096-frame reads for the inputs whose result depends on the read size and a sample of the
      rest (planar and interleaved), after checking that decode_probe at 512 frames reproduces the manifest for every file,
      and writes readsizes.json (create-only).
  verify-pack --dir tests/fixtures/aac-decode
      Checks pack-N.bin against manifest.json (and readsizes.json against both) on any machine, no Apple, no corpora.
  crosscheck  --corpus CORPUS_DIR --probe decode_probe --work SCRATCH_DIR
      Compares decode_probe (the engine reader) with the recordings that came with the corpora (manifest ext_info, the
      `oracle/*.tsv` files) for every file of the corpora and probes.

The generators are deterministic (seeded). Recorded facts are copied from decode_probe's JSON lines untouched.
"""
import argparse
import collections
import concurrent.futures as cf
import hashlib
import json
import os
import random
import re
import shutil
import struct
import subprocess
import sys

SCHEMA = "platinum-aac-decode-fixtures-v1"
SIZES_SCHEMA = "platinum-aac-decode-readsizes-v1"
GENERATOR = "tools/portable/aac/make_decode_fixtures.py"
BLOCK = 512
READ_SIZES = (100, 513, 4096)
CHUNK_SIZES = (7, 576, 1000, 1024, 1025, 4096, 100000)
MAX_FILE_BYTES = 60_000
MAX_PART_BYTES = 1_400_000
MAX_PACK_BYTES = 4_200_000
MAX_AUDIO_SECONDS = 105.0
MAX_FEATURES = 40
FIXED_SECONDS = 3_800_000_000

FOURCC = lambda s: int.from_bytes(s.encode("latin1"), "big")
AAC_LC = FOURCC("aac ")


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load_json(path):
    with open(path, "rb") as f:
        return json.loads(f.read().decode("utf-8"))


def fourcc(n):
    n &= 0xFFFFFFFF
    b = n.to_bytes(4, "big")
    return b.decode("latin1") if all(32 <= x < 127 for x in b) else hex(n)


class FixtureError(Exception):
    pass


def fail(msg):
    raise FixtureError(msg)


# --------------------------------------------------------------------------------------------
# containers: MP4, ADTS, CAF, LOAS (structure only: enough to cut, damage and relabel files)
# --------------------------------------------------------------------------------------------
MP4_CONTAINERS = {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"udta", b"dinf", b"mvex", b"moof", b"traf"}


class Node:
    """One MP4 box; `kids` is None for leaves. `pre` holds the bytes between a container's header and its children. `ext`
    writes a 64-bit size field (size 1), `eof` a size field of 0 (the box runs to the end of the file)."""

    def __init__(self, typ, data=b"", kids=None, pre=b"", ext=False, eof=False):
        self.typ, self.data, self.kids, self.pre, self.ext, self.eof = typ, data, kids, pre, ext, eof

    def clone(self):
        return Node(self.typ, self.data, None if self.kids is None else [k.clone() for k in self.kids], self.pre, self.ext, self.eof)

    def serialize(self, ctx):
        header = 16 if self.ext else 8
        start = ctx["pos"]
        if self.typ == b"mdat" and "mdat_start" not in ctx:
            ctx["mdat_start"] = start + header
        ctx["pos"] = start + header + (len(self.pre) if self.kids is not None else 0)
        if self.kids is None:
            out = self.data
        else:
            out = self.pre + b"".join(k.serialize(ctx) for k in self.kids)
        ctx["pos"] = start + header + len(out)
        if self.ext:
            return struct.pack(">I4sQ", 1, self.typ, 16 + len(out)) + out
        return struct.pack(">I4s", 0 if self.eof else 8 + len(out), self.typ) + out

    def find(self, *path):
        node = self
        for typ in path:
            node = next((k for k in (node.kids or []) if k.typ == typ), None)
            if node is None:
                return None
        return node


def parse_mp4(data, pos=0, end=None, depth=0):
    """-> (nodes, tail): boxes parsed until the first malformed one; `tail` is what could not be parsed."""
    end = len(data) if end is None else end
    nodes = []
    while pos + 8 <= end:
        size = int.from_bytes(data[pos:pos + 4], "big")
        typ = bytes(data[pos + 4:pos + 8])
        hdr = 8
        if size == 1:
            if pos + 16 > end:
                break
            size = int.from_bytes(data[pos + 8:pos + 16], "big")
            hdr = 16
        elif size == 0:
            size = end - pos
        if size < hdr or pos + size > end:
            break
        body = data[pos + hdr:pos + size]
        if typ in MP4_CONTAINERS and depth < 8:
            kids, tail = parse_mp4(data, pos + hdr, pos + size, depth + 1)
            nodes.append(Node(typ, b"", kids, b"") if not tail else Node(typ, bytes(body)))
        elif typ == b"meta" and depth < 8 and len(body) >= 4:
            kids, tail = parse_mp4(data, pos + hdr + 4, pos + size, depth + 1)
            nodes.append(Node(typ, b"", kids, bytes(body[:4])) if not tail else Node(typ, bytes(body)))
        else:
            nodes.append(Node(typ, bytes(body)))
        pos += size
    return nodes, bytes(data[pos:end])


def mp4_bytes(nodes, tail=b"", fix_offsets_from=None):
    """Serialize; with `fix_offsets_from` (the original first-mdat data position) every stco/co64 entry is moved by the
    displacement of the first mdat."""
    ctx = {"pos": 0}
    out = b"".join(n.serialize(ctx) for n in nodes) + tail
    if fix_offsets_from is None or "mdat_start" not in ctx:
        return out
    delta = ctx["mdat_start"] - fix_offsets_from
    if delta == 0:
        return out
    shifted = [n.clone() for n in nodes]
    for stbl in iter_nodes(shifted, b"stbl"):
        for box in stbl.kids or []:
            if box.typ == b"stco" and len(box.data) >= 8:
                n = struct.unpack(">I", box.data[4:8])[0]
                entries = [struct.unpack(">I", box.data[8 + 4 * i:12 + 4 * i])[0] + delta for i in range(min(n, (len(box.data) - 8) // 4))]
                box.data = box.data[:8] + b"".join(struct.pack(">I", e & 0xFFFFFFFF) for e in entries) + box.data[8 + 4 * len(entries):]
            elif box.typ == b"co64" and len(box.data) >= 8:
                n = struct.unpack(">I", box.data[4:8])[0]
                entries = [struct.unpack(">Q", box.data[8 + 8 * i:16 + 8 * i])[0] + delta for i in range(min(n, (len(box.data) - 8) // 8))]
                box.data = box.data[:8] + b"".join(struct.pack(">Q", e & 0xFFFFFFFFFFFFFFFF) for e in entries) + box.data[8 + 8 * len(entries):]
    return mp4_bytes(shifted, tail)


def iter_nodes(nodes, typ=None):
    for n in nodes:
        if typ is None or n.typ == typ:
            yield n
        if n.kids:
            yield from iter_nodes(n.kids, typ)


def first_mdat_start(data):
    nodes, _ = parse_mp4(data)
    ctx = {"pos": 0}
    for n in nodes:
        n.serialize(ctx)
    return ctx.get("mdat_start")


def box_offsets(data, pos=0, end=None, depth=0, out=None):
    """Every box boundary of the file: (start, end) pairs, containers recursively."""
    end = len(data) if end is None else end
    out = [] if out is None else out
    while pos + 8 <= end:
        size = int.from_bytes(data[pos:pos + 4], "big")
        typ = bytes(data[pos + 4:pos + 8])
        hdr = 8
        if size == 1:
            size = int.from_bytes(data[pos + 8:pos + 16], "big")
            hdr = 16
        elif size == 0:
            size = end - pos
        if size < hdr or pos + size > end:
            break
        out.append((pos, pos + size, typ.decode("latin1"), depth))
        if typ in MP4_CONTAINERS and depth < 6:
            box_offsets(data, pos + hdr, pos + size, depth + 1, out)
        pos += size
    return out


def mp4_samples(data):
    """-> [(offset, size)] of the first track's samples (stsz/stsc/stco), or []."""
    nodes, _ = parse_mp4(data)
    for trak in iter_nodes(nodes, b"trak"):
        stbl = trak.find(b"mdia", b"minf", b"stbl")
        if stbl is None:
            continue
        stsz, stsc, stco = stbl.find(b"stsz"), stbl.find(b"stsc"), stbl.find(b"stco")
        if not (stsz and stsc and stco):
            continue
        try:
            fixed, count = struct.unpack(">II", stsz.data[4:12])
            sizes = [fixed] * count if fixed else list(struct.unpack(">%dI" % count, stsz.data[12:12 + 4 * count]))
            n = struct.unpack(">I", stsc.data[4:8])[0]
            runs = [struct.unpack(">III", stsc.data[8 + 12 * i:20 + 12 * i]) for i in range(n)]
            m = struct.unpack(">I", stco.data[4:8])[0]
            chunks = list(struct.unpack(">%dI" % m, stco.data[8:8 + 4 * m]))
        except struct.error:
            continue
        out, idx = [], 0
        for ci, off in enumerate(chunks):
            per = [r for r in runs if r[0] <= ci + 1][-1][1]
            for _ in range(per):
                if idx >= len(sizes):
                    break
                out.append((off, sizes[idx]))
                off += sizes[idx]
                idx += 1
        return out
    return []


def stsz_position(data):
    """Offset of the first track's stsz sample-size table (after version/flags, sample_size, sample_count) or None."""
    for start, end, typ, depth in box_offsets(data):
        if typ == "stsz":
            return start + 8
    return None


def adts_frames(data):
    """-> [(offset, header_length, frame_length)] of consecutive ADTS frames from the start."""
    out, pos = [], 0
    while pos + 7 <= len(data) and data[pos] == 0xFF and data[pos + 1] & 0xF6 == 0xF0:
        length = ((data[pos + 3] & 3) << 11) | (data[pos + 4] << 3) | (data[pos + 5] >> 5)
        hdr = 7 if data[pos + 1] & 1 else 9
        if length < hdr or pos + length > len(data):
            break
        out.append((pos, hdr, length))
        pos += length
    return out


def caf_chunks(data):
    """-> [(type, start_of_payload, size)] of a CAF file."""
    out, pos = [], 8
    while pos + 12 <= len(data):
        typ = bytes(data[pos:pos + 4]).decode("latin1")
        size = struct.unpack(">q", data[pos + 4:pos + 12])[0]
        if size < 0:
            size = len(data) - pos - 12
        out.append((typ, pos + 12, size))
        pos += 12 + size
    return out


def sniff(data):
    """Container of a byte string as a file-type sniffer sees it: mp4/qt, adts, loas, caf, id3(+inner), riff, other."""
    d = bytes(data[:16])
    if d[:3] == b"ID3":
        return "id3"
    if d[4:8] in (b"ftyp", b"moov", b"free", b"wide", b"mdat", b"skip"):
        return "mp4"
    if d[:4] == b"caff":
        return "caf"
    if len(d) >= 2 and d[0] == 0xFF and d[1] & 0xF6 == 0xF0:
        return "adts"
    if len(d) >= 2 and d[0] == 0x56 and d[1] & 0xE0 == 0xE0:
        return "loas"
    if d[:4] == b"RIFF":
        return "riff"
    if d[:4] == b"OggS":
        return "ogg"
    if d[:4] == b"fLaC":
        return "flac"
    return "other"


def id3v2(size, version=4, flags=0, payload=None):
    payload = bytes(size) if payload is None else payload
    ss = bytes([(size >> 21) & 0x7F, (size >> 14) & 0x7F, (size >> 7) & 0x7F, size & 0x7F])
    return b"ID3" + bytes([version, 0, flags]) + ss + payload


# --------------------------------------------------------------------------------------------
# the recorder
# --------------------------------------------------------------------------------------------
def run_probe(probe, args, timeout=120):
    try:
        r = subprocess.run([probe] + list(args), capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"crash": "timeout"}
    if r.returncode != 0:
        return {"crash": "exit %s" % r.returncode, "stderr": r.stderr.decode(errors="replace")[:300]}
    try:
        return json.loads(r.stdout.decode())
    except ValueError:
        return {"crash": "bad json", "stdout": r.stdout.decode(errors="replace")[:300]}


def record_files(probe, paths, extra=(), workers=8):
    """paths: {key: path} -> {key: recording}; each file is recorded twice and the recordings must be identical."""
    def one(path):
        a = run_probe(probe, ["record", path] + list(extra))
        b = run_probe(probe, ["record", path] + list(extra))
        if a != b:
            return {"crash": "nondeterministic"}
        return a
    out = {}
    with cf.ThreadPoolExecutor(workers) as ex:
        futures = {ex.submit(one, p): k for k, p in paths.items()}
        for f in cf.as_completed(futures):
            out[futures[f]] = f.result()
    return out


def status_i32(n):
    n &= 0xFFFFFFFF
    return n - (1 << 32) if n & 0x80000000 else n


def err_of(e):
    """decode_probe error object -> manifest form."""
    if e is None:
        return None
    if "operation" in e:
        return {"operation": e["operation"], "status": status_i32(e["status"])}
    return {"other": e["other"]}


def clean_prop(p):
    return {"value": p["value"]} if "value" in p else {"error": err_of(p["error"])}


def clean_stream(s):
    out = {"frames": s["frames"], "error": err_of(s["error"]), "sha256": s["sha256"], "reads": s["reads"], "short": s["short"],
           "lens_sha256": s["lens_sha256"], "tails": s["tails"]}
    return out


def clean_open(o):
    if not o["ok"]:
        return {"ok": False, **err_of(o["error"])}, None
    info = {"source": o["source"], "client": o["client"], "length": clean_prop(o["length"]), "bitrate": clean_prop(o["bitrate"])}
    return {"ok": True}, info


def asbd_sfi(rate):
    table = {96000: 0, 88200: 1, 64000: 2, 48000: 3, 44100: 4, 32000: 5, 24000: 6, 22050: 7, 16000: 8, 12000: 9, 11025: 10, 8000: 11, 7350: 12}
    return table.get(int(rate))


# --------------------------------------------------------------------------------------------
# candidates
# --------------------------------------------------------------------------------------------
class Cand:
    """One input file under the name it will have in the replay."""

    def __init__(self, key, name, data, family, origin, tags=(), meta=None):
        self.key, self.name, self.data, self.family, self.origin = key, name, bytes(data), family, origin
        self.tags = list(tags)
        self.meta = meta or {}
        self.rec = None
        self.path = None
        self.features = set()
        self.sig = None
        self.cost = 0.0
        self.decode_seconds = 0.0

    @property
    def sha256(self):
        return sha256_bytes(self.data)


def rng_for(*parts):
    return random.Random(int(hashlib.sha256("|".join(str(p) for p in parts).encode()).hexdigest()[:16], 16))


CONTENTS = ("tone", "noise", "clicks", "mix", "gated", "loud", "low", "chirp", "steps")

# what the engine's Apple writer accepts at 256 kbit/s (measured): 44.1/48 kHz 1..8 channels,
# 32 kHz from 3 channels, 24/22.05 kHz from 4, 16 kHz from 7. The converter exists for more, but its 256 kbit/s setting fails
# and so does the first write ('!dat'): the other rates come from afconvert.
ACCEPTED = ([(44100, c) for c in range(1, 9)] + [(48000, c) for c in range(1, 9)] + [(32000, c) for c in range(3, 9)]
            + [(24000, c) for c in range(4, 9)] + [(22050, c) for c in range(4, 9)] + [(16000, 7), (16000, 8)])
LENGTHS = (1, 2, 100, 511, 512, 513, 1023, 1024, 1025, 1536, 2047, 2048, 2049, 3000)
FRAMES_CYCLE = (11025, 22050, 4410, 33075, 17000, 26000, 8820, 15000)


def fresh_specs():
    """(name, rate, channels, frames, content, seed) of every file encoded with the engine's Apple writer."""
    specs = []
    for i, (rate, ch) in enumerate(ACCEPTED):
        content = CONTENTS[(i + ch) % len(CONTENTS)]
        frames = FRAMES_CYCLE[i % len(FRAMES_CYCLE)]
        if ch >= 6:
            frames = min(frames, 15000)
        specs.append(("acc_%d_%dch_%s" % (rate, ch, content), rate, ch, frames, content, 100 + i, "fresh-accepted"))
    for i, content in enumerate(CONTENTS):
        specs.append(("st44_%s" % content, 44100, 2, 16000 + 1000 * i, content, 500 + i, "fresh-content"))
    for i, content in enumerate(("tone", "clicks", "mix", "loud", "gated")):
        specs.append(("m48_%s" % content, 48000, 1, 14000 + 700 * i, content, 600 + i, "fresh-content"))
    for i, frames in enumerate(LENGTHS):
        specs.append(("len44_%05d" % frames, 44100, 2, frames, "tone" if i % 2 == 0 else "mix", 700 + i, "fresh-length"))
    for i, frames in enumerate((1, 1024, 1500)):
        specs.append(("len48m_%05d" % frames, 48000, 1, frames, "tone", 800 + i, "fresh-length"))
    return specs


def shrink_free(data):
    """The Apple writer reserves tens of kilobytes behind `moov`; keep the box (the layout is part of the input) but not the zeros."""
    nodes, tail = parse_mp4(data)
    old = first_mdat_start(data)
    for n in nodes:
        if n.typ == b"free" and len(n.data) > 32:
            n.data = bytes(32)
    return mp4_bytes(nodes, tail, fix_offsets_from=old)


# bases of the mutation families: short enough to decode quickly, long enough to have a dozen access units
BASE_SPECS = (("mutbase_st44", 44100, 2, 12000, "mix", 900), ("mutbase_m48", 48000, 1, 12000, "tone", 901), ("mutbase_6ch", 44100, 6, 8000, "mix", 902))


def generate_fresh(probe, workdir):
    out = []
    gen = os.path.join(workdir, "gen")
    os.makedirs(gen, exist_ok=True)
    for name, rate, ch, frames, content, seed, family in fresh_specs() + [b + ("mutation-base",) for b in BASE_SPECS]:
        path = os.path.join(gen, name + ".m4a")
        if os.path.exists(path):
            os.unlink(path)
        r = run_probe(probe, ["gen", path, str(rate), str(ch), str(frames), content, str(seed)])
        if not r.get("ok"):
            print("gen %s refused: %s" % (name, r), file=sys.stderr)
            continue
        with open(path, "rb") as f:
            data = shrink_free(f.read())
        out.append(Cand("fresh/" + name, name + ".m4a", data, family,
                        {"kind": "engine-writer", "rate": rate, "channels": ch, "frames": frames, "content": content, "seed": seed, "warnings": r["warnings"]},
                        tags=["encoder:engine-writer"]))
    return out


# --- afconvert (Apple's command line encoder) from generated tones ---------------------------------------------------------
def tone_samples(rate, ch, frames, seed=1):
    import math
    out = []
    for i in range(frames):
        t = i / rate
        for c in range(ch):
            f0 = 220 + 97 * c + seed * 13
            v = 0.3 * math.sin(6.283185307179586 * f0 * t) + 0.1 * math.sin(6.283185307179586 * 2.01 * f0 * t) + 0.04 * math.sin(6.283185307179586 * (3000 + 400 * c) * t)
            out.append(max(-32768, min(32767, int(round(v * 32767)))))
    return out


def write_wav16(path, rate, ch, frames, seed=1):
    import wave
    samples = tone_samples(rate, ch, frames, seed)
    with wave.open(path, "wb") as w:
        w.setnchannels(ch)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(struct.pack("<%dh" % len(samples), *samples))


# (name, rate, channels, frames, afconvert arguments, extension)
AFCONVERT = (
    ("he1_m4a_44s", 44100, 2, 22050, ["-f", "m4af", "-d", "aach", "-b", "48000"], ".m4a"),
    ("he1_m4a_48s", 48000, 2, 24000, ["-f", "m4af", "-d", "aach", "-b", "64000"], ".m4a"),
    ("he1_m4a_44m", 44100, 1, 22050, ["-f", "m4af", "-d", "aach", "-b", "32000"], ".m4a"),
    ("he1_m4a_32s", 32000, 2, 16000, ["-f", "m4af", "-d", "aach", "-b", "40000"], ".m4a"),
    ("he1_adts_44s", 44100, 2, 22050, ["-f", "adts", "-d", "aach", "-b", "48000"], ".aac"),
    ("he1_adts_48m", 48000, 1, 24000, ["-f", "adts", "-d", "aach", "-b", "32000"], ".aac"),
    ("he1_caf_44s", 44100, 2, 22050, ["-f", "caff", "-d", "aach", "-b", "48000"], ".caf"),
    ("he1_loas_44s", 44100, 2, 22050, ["-f", "loas", "-d", "aach", "-b", "48000"], ".loas"),
    ("he1_mp4_44s", 44100, 2, 22050, ["-f", "mp4f", "-d", "aach", "-b", "48000"], ".mp4"),
    ("he2_m4a_44s", 44100, 2, 22050, ["-f", "m4af", "-d", "aacp", "-b", "32000"], ".m4a"),
    ("he2_m4a_48s", 48000, 2, 24000, ["-f", "m4af", "-d", "aacp", "-b", "24000"], ".m4a"),
    ("he2_adts_44s", 44100, 2, 22050, ["-f", "adts", "-d", "aacp", "-b", "32000"], ".aac"),
    ("he2_caf_44s", 44100, 2, 22050, ["-f", "caff", "-d", "aacp", "-b", "32000"], ".caf"),
    ("he2_loas_44s", 44100, 2, 22050, ["-f", "loas", "-d", "aacp", "-b", "32000"], ".loas"),
    ("ld_m4a_44m", 44100, 1, 22050, ["-f", "m4af", "-d", "aacl", "-b", "48000"], ".m4a"),
    ("ld_m4a_48s", 48000, 2, 24000, ["-f", "m4af", "-d", "aacl", "-b", "96000"], ".m4a"),
    ("ld_caf_44m", 44100, 1, 22050, ["-f", "caff", "-d", "aacl", "-b", "48000"], ".caf"),
    ("eld_m4a_44s", 44100, 2, 22050, ["-f", "m4af", "-d", "aace", "-b", "64000"], ".m4a"),
    ("eld_m4a_48m", 48000, 1, 24000, ["-f", "m4af", "-d", "aace", "-b", "48000"], ".m4a"),
    ("eld_caf_44s", 44100, 2, 22050, ["-f", "caff", "-d", "aace", "-b", "64000"], ".caf"),
    ("eldsbr_m4a_44s", 44100, 2, 22050, ["-f", "m4af", "-d", "aacf", "-b", "48000"], ".m4a"),
    ("usac_m4a_48s", 48000, 2, 24000, ["-f", "m4af", "-d", "usac", "-b", "64000"], ".m4a"),
    ("usac_m4a_44m", 44100, 1, 22050, ["-f", "m4af", "-d", "usac", "-b", "32000"], ".m4a"),
    ("usac_loas_48s", 48000, 2, 24000, ["-f", "loas", "-d", "usac", "-b", "64000"], ".loas"),
    ("usac_mp4_48s", 48000, 2, 24000, ["-f", "mp4f", "-d", "usac", "-b", "64000"], ".mp4"),
    ("mp1_m4a_44s", 44100, 2, 22050, ["-f", "m4af", "-d", ".mp1", "-b", "192000"], ".m4a"),
    ("mp2_m4a_44s", 44100, 2, 22050, ["-f", "m4af", "-d", ".mp2", "-b", "160000"], ".m4a"),
    ("lc_m4a_cbr_44s", 44100, 2, 20000, ["-f", "m4af", "-d", "aac", "-b", "128000", "-s", "0"], ".m4a"),
    ("lc_m4a_vbr_44s", 44100, 2, 20000, ["-f", "m4af", "-d", "aac", "-s", "3", "-q", "90"], ".m4a"),
    ("lc_m4a_cvbr_48s", 48000, 2, 20000, ["-f", "m4af", "-d", "aac", "-b", "96000", "-s", "2"], ".m4a"),
    ("lc_m4a_16m", 16000, 1, 9000, ["-f", "m4af", "-d", "aac", "-b", "32000"], ".m4a"),
    ("lc_m4a_8m", 8000, 1, 5000, ["-f", "m4af", "-d", "aac", "-b", "16000"], ".m4a"),
    ("lc_m4a_22m", 22050, 1, 11025, ["-f", "m4af", "-d", "aac", "-b", "48000"], ".m4a"),
    ("lc_m4a_24s", 24000, 2, 12000, ["-f", "m4af", "-d", "aac", "-b", "64000"], ".m4a"),
    ("lc_m4a_11m", 11025, 1, 5500, ["-f", "m4af", "-d", "aac", "-b", "24000"], ".m4a"),
    ("lc_m4a_12m", 12000, 1, 6000, ["-f", "m4af", "-d", "aac", "-b", "24000"], ".m4a"),
    ("lc_m4a_32m", 32000, 1, 16000, ["-f", "m4af", "-d", "aac", "-b", "64000"], ".m4a"),
    ("lc_m4a_44m", 44100, 1, 22050, ["-f", "m4af", "-d", "aac", "-b", "64000"], ".m4a"),
    ("lc_m4b_44s", 44100, 2, 12000, ["-f", "m4bf", "-d", "aac", "-b", "64000"], ".m4b"),
    ("lc_mp4_44s", 44100, 2, 12000, ["-f", "mp4f", "-d", "aac", "-b", "64000"], ".mp4"),
    ("lc_3gp_44s", 44100, 2, 12000, ["-f", "3gpp", "-d", "aac", "-b", "64000"], ".3gp"),
    ("lc_3g2_44s", 44100, 2, 12000, ["-f", "3gp2", "-d", "aac", "-b", "64000"], ".3g2"),
    ("lc_adts_44s", 44100, 2, 22050, ["-f", "adts", "-d", "aac", "-b", "96000"], ".aac"),
    ("lc_adts_48m", 48000, 1, 12000, ["-f", "adts", "-d", "aac", "-b", "48000"], ".aac"),
    ("lc_adts_16m", 16000, 1, 9000, ["-f", "adts", "-d", "aac", "-b", "32000"], ".aac"),
    ("lc_adts_8m", 8000, 1, 5000, ["-f", "adts", "-d", "aac", "-b", "16000"], ".aac"),
    ("lc_adts_22s", 22050, 2, 11025, ["-f", "adts", "-d", "aac", "-b", "64000"], ".aac"),
    ("lc_loas_44s", 44100, 2, 22050, ["-f", "loas", "-d", "aac", "-b", "96000"], ".loas"),
    ("lc_loas_48m", 48000, 1, 12000, ["-f", "loas", "-d", "aac", "-b", "48000"], ".loas"),
    ("lc_caf_44s", 44100, 2, 22050, ["-f", "caff", "-d", "aac", "-b", "96000"], ".caf"),
    ("lc_caf_48m", 48000, 1, 12000, ["-f", "caff", "-d", "aac", "-b", "48000"], ".caf"),
    ("lc_caf_22s", 22050, 2, 11025, ["-f", "caff", "-d", "aac", "-b", "64000"], ".caf"),
    ("lc_caf_16m", 16000, 1, 9000, ["-f", "caff", "-d", "aac", "-b", "32000"], ".caf"),
)


def aac_asc_patch_core(data, core_asc):
    """Replace the AudioSpecificConfig inside an esds by `core_asc` padded with zeros to its old length (no sizes change):
    HE-AAC with implicit signalling."""
    at = find_asc(data)
    if at is None:
        return None
    off, length = at
    if len(core_asc) > length:
        return None
    return data[:off] + core_asc + bytes(length - len(core_asc)) + data[off + length:]


def descr_length(data, pos):
    """MPEG-4 descriptor length at `pos` (1-4 bytes, 7 bits each) -> (length, bytes used)."""
    n = 0
    for i in range(4):
        b = data[pos + i]
        n = (n << 7) | (b & 0x7F)
        if not b & 0x80:
            return n, i + 1
    return n, 4


def find_asc(data):
    """(offset, length) of the AudioSpecificConfig (DecoderSpecificInfo, tag 5) of the first esds in an MP4/CAF-free file."""
    i = data.find(b"esds")
    if i < 0:
        return None
    pos = i + 8  # fourcc + version/flags
    try:
        if data[pos] == 3:
            n, used = descr_length(data, pos + 1)
            pos += 1 + used + 3  # ES_ID(2) + flags(1)
        if data[pos] == 4:
            n, used = descr_length(data, pos + 1)
            pos += 1 + used + 13  # objectType, streamType, bufferSize(3), max(4), avg(4)
        if data[pos] == 5:
            n, used = descr_length(data, pos + 1)
            return pos + 1 + used, n
    except IndexError:
        return None
    return None


def parse_asc(asc):
    """-> dict(aot, sfi, rate, channels, ext_aot, sbr, ps) for the common AudioSpecificConfig layouts (best effort)."""
    bits = "".join("{:08b}".format(b) for b in asc)

    def take(n, at):
        return int(bits[at:at + n] or "0", 2), at + n
    try:
        aot, p = take(5, 0)
        if aot == 31:
            ext, p = take(6, p)
            aot = 32 + ext
        sfi, p = take(4, p)
        rate = None
        if sfi == 15:
            rate, p = take(24, p)
        ch, p = take(4, p)
        out = {"aot": aot, "sfi": sfi, "rate": rate, "channels": ch}
        if aot in (5, 29):
            esfi, p = take(4, p)
            out["ext_sfi"] = esfi
            out["sbr"] = True
            out["ps"] = aot == 29
        return out
    except ValueError:
        return {}


def pin_mp4_times(data, seconds=FIXED_SECONDS):
    """afconvert stamps the wall clock into mvhd/tkhd/mdhd: pin it so that the generated file is reproducible (what
    `set_mp4_header_times` does for the Apple writer's files)."""
    if sniff(data) != "mp4":
        return data
    nodes, tail = parse_mp4(data)
    old = first_mdat_start(data)
    for typ in (b"mvhd", b"tkhd", b"mdhd"):
        for box in iter_nodes(nodes, typ):
            d = box.data
            if d[:1] == b"\x00" and len(d) >= 12:
                box.data = d[:4] + struct.pack(">II", seconds, seconds) + d[12:]
            elif d[:1] == b"\x01" and len(d) >= 20:
                box.data = d[:4] + struct.pack(">QQ", seconds, seconds) + d[20:]
    return mp4_bytes(nodes, tail, fix_offsets_from=old) if not tail else data


def generate_afconvert(workdir):
    out = []
    if shutil.which("afconvert") is None:
        return out
    af = os.path.join(workdir, "af")
    os.makedirs(af, exist_ok=True)
    wavs = {}
    for name, rate, ch, frames, args, ext in AFCONVERT:
        key = (rate, ch, frames)
        if key not in wavs:
            wavs[key] = os.path.join(af, "in_%d_%d_%d.wav" % key)
            write_wav16(wavs[key], rate, ch, frames, seed=3)
        path = os.path.join(af, name + ext)
        if os.path.exists(path):
            os.unlink(path)
        r = subprocess.run(["afconvert"] + args + [wavs[key], path], capture_output=True, text=True)
        if r.returncode != 0 or not os.path.exists(path):
            print("afconvert %s refused: %s" % (name, r.stderr.strip()[:120]), file=sys.stderr)
            continue
        with open(path, "rb") as f:
            data = pin_mp4_times(f.read())
        family = "afconvert-he" if name.startswith(("he1", "he2")) else "afconvert-nonlc" if name.startswith(("ld", "eld", "usac", "mp1", "mp2")) else "afconvert-lc"
        out.append(Cand("afconvert/" + name, name + ext, data, family,
                        {"kind": "afconvert", "args": args, "rate": rate, "channels": ch, "frames": frames}, tags=["encoder:afconvert"]))
        # HE-AAC with implicit signalling: the AudioSpecificConfig names the core codec only, the SBR/PS data is an extension payload
        if name in ("he1_m4a_44s", "he1_m4a_44m", "he1_m4a_48s", "he2_m4a_44s") and ext == ".m4a":
            at = find_asc(data)
            if at:
                asc = parse_asc(data[at[0]:at[0] + at[1]])
                core = data[at[0]:at[0] + 2]
                if asc.get("aot") == 2:
                    # AOT 2 with explicit signalling behind it: keep the first two bytes only
                    patched = aac_asc_patch_core(data, core)
                    if patched is not None:
                        out.append(Cand("afconvert/" + name + "_implicit", name + "_implicit" + ext, patched, "afconvert-he",
                                        {"kind": "afconvert+asc-core", "of": name}, tags=["he:implicit-signalling"]))
    return out


# --------------------------------------------------------------------------------------------
# deterministic mutations of container files and streams
# --------------------------------------------------------------------------------------------
def flip_bit(data, bit):
    d = bytearray(data)
    d[bit // 8] ^= 0x80 >> (bit % 8)
    return bytes(d)


def mp4_mutations(tag, data):
    """name -> bytes: structure edits, table edits, size fields, ASC damage, truncations at every box boundary, access-unit damage."""
    out = {}
    nodes, tail = parse_mp4(data)
    if not nodes or tail:
        return out
    old = first_mdat_start(data)

    def edit(fn):
        ns = [n.clone() for n in nodes]
        fn(ns)
        return mp4_bytes(ns, tail, fix_offsets_from=old)

    def moov_of(ns):
        return next(n for n in ns if n.typ == b"moov")

    def stbl_of(ns):
        return moov_of(ns).find(b"trak", b"mdia", b"minf", b"stbl")

    def reorder(ns):
        head = [n for n in ns if n.typ == b"ftyp"]
        rest = [n for n in ns if n.typ not in (b"ftyp", b"moov")]
        ns[:] = head + rest + [moov_of(ns)]
    out["moov_last"] = edit(reorder)
    out["free_head"] = edit(lambda ns: ns.insert(1, Node(b"free", bytes(24))))
    out["wide_before_mdat"] = edit(lambda ns: ns.insert(next(i for i, n in enumerate(ns) if n.typ == b"mdat"), Node(b"wide")))
    out["skip_tail"] = edit(lambda ns: ns.append(Node(b"skip", bytes(12))))
    out["uuid_top"] = edit(lambda ns: ns.insert(1, Node(b"uuid", bytes(range(16)) + b"payload!")))
    out["free_in_moov"] = edit(lambda ns: moov_of(ns).kids.insert(1, Node(b"free", bytes(10))))
    out["free_in_stbl"] = edit(lambda ns: stbl_of(ns).kids.insert(0, Node(b"free", bytes(6))))
    out["udta_gone"] = edit(lambda ns: moov_of(ns).kids.__setitem__(slice(None), [k for k in moov_of(ns).kids if k.typ != b"udta"]))
    out["no_free"] = edit(lambda ns: ns.__setitem__(slice(None), [n for n in ns if n.typ != b"free"]))
    out["mdat_size64"] = edit(lambda ns: setattr(next(n for n in ns if n.typ == b"mdat"), "ext", True))
    out["moov_last_mdat_eof"] = edit(lambda ns: (reorder(ns), None)[1] or setattr(next(n for n in ns if n.typ == b"mdat"), "eof", True))

    def dup_trak(ns):
        moov = moov_of(ns)
        i = next(i for i, k in enumerate(moov.kids) if k.typ == b"trak")
        moov.kids.insert(i + 1, moov.kids[i].clone())
    out["two_tracks"] = edit(dup_trak)

    def drop(typ):
        def f(ns):
            for node in iter_nodes(ns):
                if node.kids:
                    node.kids[:] = [k for k in node.kids if k.typ != typ]
        return f
    for typ in (b"stco", b"stsz", b"stsc", b"stts", b"stsd", b"mdhd", b"hdlr", b"tkhd", b"mvhd", b"smhd", b"dinf", b"stbl", b"minf", b"mdia", b"trak"):
        out["drop_" + typ.decode()] = edit(drop(typ))

    def table(typ, fn):
        def f(ns):
            box = next(iter_nodes(ns, typ))
            box.data = fn(box.data)
        return edit(f)
    out["stsz_count_plus1"] = table(b"stsz", lambda d: d[:8] + struct.pack(">I", struct.unpack(">I", d[8:12])[0] + 1) + d[12:])
    out["stsz_count_minus1"] = table(b"stsz", lambda d: d[:8] + struct.pack(">I", max(0, struct.unpack(">I", d[8:12])[0] - 1)) + d[12:])
    out["stsz_count_zero"] = table(b"stsz", lambda d: d[:8] + struct.pack(">I", 0) + d[12:])
    out["stsz_count_huge"] = table(b"stsz", lambda d: d[:8] + struct.pack(">I", 0x7FFFFFFF) + d[12:])
    out["stco_first_past_eof"] = table(b"stco", lambda d: d[:8] + struct.pack(">I", struct.unpack(">I", d[8:12])[0] + 0x100000) + d[12:])
    out["stco_first_zero"] = table(b"stco", lambda d: d[:8] + struct.pack(">I", 0) + d[12:])
    out["stco_count_zero"] = table(b"stco", lambda d: d[:4] + struct.pack(">I", 0) + d[8:])
    out["stts_count_zero"] = table(b"stts", lambda d: d[:4] + struct.pack(">I", 0) + d[8:])
    out["stts_delta_zero"] = table(b"stts", lambda d: d[:12] + struct.pack(">I", 0) + d[16:])
    out["stts_delta_512"] = table(b"stts", lambda d: d[:12] + struct.pack(">I", 512) + d[16:])
    out["stsc_first_chunk_2"] = table(b"stsc", lambda d: d[:8] + struct.pack(">I", 2) + d[12:])
    out["stsc_per_chunk_zero"] = table(b"stsc", lambda d: d[:12] + struct.pack(">I", 0) + d[16:])

    # raw box-size fields: giant, off by one, impossible, 64-bit, to-end
    spots = [(s, e, t) for s, e, t, depth in box_offsets(data) if depth == 0]
    for start, end, typ in spots:
        if typ not in ("moov", "mdat", "ftyp", "free"):
            continue
        for label, value in (("ffffffff", 0xFFFFFFFF), ("plus1", end - start + 1), ("minus1", end - start - 1), ("three", 3), ("zero", 0)):
            d = bytearray(data)
            d[start:start + 4] = struct.pack(">I", value)
            out["size_%s_%s" % (typ, label)] = bytes(d)
        d = bytearray(data)
        d[start:start + 4] = struct.pack(">I", 1)
        out["size_%s_ext_huge" % typ] = bytes(d[:start + 8]) + struct.pack(">Q", 0x7FFFFFFFFFFFFFFF) + bytes(d[start + 8:])

    # AudioSpecificConfig damage and esds removal
    at = find_asc(data)
    if at:
        for bit in (0, 3, 4, 5, 7, 8, 9, 12, 13, 14, 15):
            out["asc_flip_%02d" % bit] = flip_bit(data, at[0] * 8 + bit)
        out["asc_len0"] = data[:at[0] - 1] + b"\x00" + data[at[0]:]
        out["no_esds_renamed"] = data.replace(b"esds", b"xsds", 1)
    # truncations at every box boundary
    cuts = set()
    for start, end, typ, depth in box_offsets(data):
        if depth > 4:
            continue
        for c in (start, start + 1, start + 4, start + 7, start + 8, start + 12, (start + end) // 2, end - 1, end + 1):
            if 0 <= c < len(data):
                cuts.add(c)
    for c in sorted(cuts):
        out["trunc_%05d" % c] = data[:c]
    # access-unit damage
    aus = mp4_samples(data)
    if aus:
        stsz = stsz_position(data)
        n = len(aus)
        rng = rng_for("mp4", tag)
        picks = [(0, 1), (1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (n // 2, 1), (n // 2, 3), (n - 2, 1), (n - 1, 1), (1, 2), (1, 4), (2, 6)]
        for k, cnt in picks:
            if k < 0 or k + cnt > n:
                continue
            d = bytearray(data)
            for j in range(k, k + cnt):
                off, size = aus[j]
                d[off:off + size] = bytes(rng.randrange(256) for _ in range(size))
            out["au_random_%d_x%d" % (k, cnt)] = bytes(d)
        for k in (1, n // 2):
            off, size = aus[k]
            d = bytearray(data)
            d[off:off + size] = bytes(size)
            out["au_zero_%d" % k] = bytes(d)
            d = bytearray(data)
            d[off:off + size] = b"\xff" * size
            out["au_ones_%d" % k] = bytes(d)
            d = bytearray(data)
            d[off:off + size] = b"\xe0" + bytes(size - 1)
            out["au_end_only_%d" % k] = bytes(d)
            for s in range(3):
                d = bytearray(data)
                for _ in range(1 + 3 * s):
                    bit = rng.randrange(size * 8)
                    d[off + bit // 8] ^= 0x80 >> (bit % 8)
                out["au_bitflip_%d_%d" % (k, s)] = bytes(d)
        if stsz is not None and struct.unpack(">I", data[stsz:stsz + 4])[0] == 0:
            first = stsz + 8
            channels = 2
            for k in (1, n // 2, n - 1):
                for label, value in (("0", 0), ("1", 1), ("2", 2), ("over", 1024 * channels + 1), ("over2", 4096), ("huge", 70000), ("max", 0xFFFFFFFF)):
                    d = bytearray(data)
                    d[first + 4 * k:first + 4 * k + 4] = struct.pack(">I", value)
                    out["stsz_%d_%s" % (k, label)] = bytes(d)
    return out


def adts_mutations(tag, data):
    out = {}
    frames = adts_frames(data)
    if len(frames) < 4:
        return out
    rng = rng_for("adts", tag)
    n = len(frames)
    for j in (1, n // 2, n - 1):
        start, hdr, length = frames[j]
        for delta in (0, 1, 3, 6, 7, length // 2, length - 1):
            c = start + delta
            out["trunc_f%d_%d" % (j, delta)] = data[:c]
    out["trunc_end_plus1"] = data[:frames[-1][0] + frames[-1][2] - 1]
    # header fields of one frame
    def patch(j, fn):
        d = bytearray(data)
        s = frames[j][0]
        fn(d, s)
        return bytes(d)
    for j in (0, n // 2):
        for profile in (0, 1, 3):
            out["profile%d_f%d" % (profile, j)] = patch(j, lambda d, s, p=profile: d.__setitem__(s + 2, (d[s + 2] & 0x3F) | (p << 6)))
        for sfi in (0, 3, 7, 11, 12, 13, 15):
            out["sfi%d_f%d" % (sfi, j)] = patch(j, lambda d, s, v=sfi: d.__setitem__(s + 2, (d[s + 2] & 0xC3) | (v << 2)))
        for cfg in (0, 1, 3, 6, 7):
            out["chan%d_f%d" % (cfg, j)] = patch(j, lambda d, s, v=cfg: (d.__setitem__(s + 2, (d[s + 2] & 0xFE) | (v >> 2)), d.__setitem__(s + 3, (d[s + 3] & 0x3F) | ((v & 3) << 6))))
        out["id_toggle_f%d" % j] = patch(j, lambda d, s: d.__setitem__(s + 1, d[s + 1] ^ 0x08))
        out["prot_toggle_f%d" % j] = patch(j, lambda d, s: d.__setitem__(s + 1, d[s + 1] ^ 0x01))
        for delta in (-1, 1, 100, 0x1FFF):
            def f(d, s, delta=delta):
                length = ((d[s + 3] & 3) << 11) | (d[s + 4] << 3) | (d[s + 5] >> 5)
                v = (length + delta) & 0x1FFF if delta != 0x1FFF else 0x1FFF
                d[s + 3] = (d[s + 3] & 0xFC) | (v >> 11)
                d[s + 4] = (v >> 3) & 0xFF
                d[s + 5] = (d[s + 5] & 0x1F) | ((v & 7) << 5)
            out["length%+d_f%d" % (delta, j)] = patch(j, f)
        for blocks in (1, 2, 3):
            out["blocks%d_f%d" % (blocks, j)] = patch(j, lambda d, s, b=blocks: d.__setitem__(s + 6, (d[s + 6] & 0xFC) | b))
    # damage between and inside frames
    for j in (0, n // 2):
        start = frames[j][0]
        for size in (1, 3, 100):
            out["garbage%d_before%d" % (size, j)] = data[:start] + bytes(rng.randrange(256) for _ in range(size)) + data[start:]
        out["nosync_f%d" % j] = patch(j, lambda d, s: d.__setitem__(s, 0))
    for j, cnt in ((0, 1), (1, 1), (2, 2), (2, 3), (3, 4), (1, 5)):
        if j + cnt > n:
            continue
        d = bytearray(data)
        for k in range(j, j + cnt):
            s, hdr, length = frames[k]
            d[s + hdr:s + length] = bytes(rng.randrange(256) for _ in range(length - hdr))
        out["au_random_%d_x%d" % (j, cnt)] = bytes(d)
    s0, h0, l0 = frames[0]
    out["drop_frame1"] = data[:frames[1][0]] + data[frames[2][0]:]
    out["dup_frame0"] = data[:s0 + l0] + data[s0:s0 + l0] + data[s0 + l0:]
    out["id3v2_prefix"] = id3v2(20) + data
    out["id3v1_suffix"] = data + b"TAG" + bytes(125)
    return out


def caf_mutations(tag, data):
    out = {}
    chunks = caf_chunks(data)
    if len(chunks) < 3 or data[:4] != b"caff":
        return out
    rng = rng_for("caf", tag)
    by = {}
    for typ, start, size in chunks:
        by.setdefault(typ, (start, size))
    for typ, start, size in chunks:
        for c in (start - 12, start - 5, start, start + size // 2, start + size - 1, start + size):
            if 0 < c < len(data):
                out["trunc_%s_%d" % (typ.strip(), c)] = data[:c]
    if "desc" in by:
        s = by["desc"][0]
        def put(off, raw):
            d = bytearray(data)
            d[s + off:s + off + len(raw)] = raw
            return bytes(d)
        out["rate_x2"] = put(0, struct.pack(">d", struct.unpack(">d", data[s:s + 8])[0] * 2))
        out["rate_48000"] = put(0, struct.pack(">d", 48000.0))
        out["rate_zero"] = put(0, struct.pack(">d", 0.0))
        out["rate_nan"] = put(0, struct.pack(">Q", 0x7FF8000000000000))
        out["format_alac"] = put(8, b"alac")
        out["format_lpcm"] = put(8, b"lpcm")
        out["format_aach"] = put(8, b"aach")
        out["format_flags_1"] = put(12, struct.pack(">I", 1))
        out["bytes_per_packet_100"] = put(16, struct.pack(">I", 100))
        for v in (0, 960, 2048, 512):
            out["frames_per_packet_%d" % v] = put(20, struct.pack(">I", v))
        for v in (0, 1, 3, 9):
            out["channels_%d" % v] = put(24, struct.pack(">I", v))
        out["bits_16"] = put(28, struct.pack(">I", 16))
    for typ in ("kuki", "pakt", "chan", "info", "free"):
        if typ in by:
            start, size = by[typ]
            out["drop_" + typ] = data[:start - 12] + data[start + size:]
    if "data" in by:
        start, size = by["data"]
        for label, value in (("minus1", -1), ("zero", 0), ("huge", 1 << 40), ("short", max(0, size - 5)), ("plus", size + 7)):
            d = bytearray(data)
            d[start - 8:start] = struct.pack(">q", value)
            out["data_size_" + label] = bytes(d)
        d = bytearray(data)
        for k in range(8):
            d[start + 4 + rng.randrange(max(1, size - 4))] ^= 1 << rng.randrange(8)
        out["data_bitflips"] = bytes(d)
    if "desc" in by and "data" in by:
        # chunks in another order
        parts = [(t, data[s - 12:s + sz]) for t, s, sz in chunks]
        order = sorted(parts, key=lambda p: 0 if p[0] == "data" else 1)
        out["data_first"] = data[:8] + b"".join(p[1] for p in order)
    if "pakt" in by:
        start, size = by["pakt"]
        for k, bit in enumerate((0, 8 * 8, 8 * 16, 8 * 24 + 3)):
            if bit // 8 < size:
                out["pakt_flip_%d" % k] = flip_bit(data, start * 8 + bit)
    out["id3v2_prefix"] = id3v2(20) + data
    return out


def loas_mutations(tag, data):
    out = {}
    if len(data) < 64:
        return out
    rng = rng_for("loas", tag)
    for c in (1, 2, 3, 5, 8, 12, 40, len(data) // 2, len(data) - 1):
        out["trunc_%d" % c] = data[:c]
    for bit in (11, 13, 16, 20, 27, 32, 40):
        out["flip_%d" % bit] = flip_bit(data, bit)
    out["garbage_prefix"] = bytes(rng.randrange(256) for _ in range(5)) + data
    out["doubled"] = data + data
    out["id3v2_prefix"] = id3v2(20) + data
    for k, (off, cnt) in enumerate(((40, 30), (100, 60), (len(data) // 2, 100))):
        d = bytearray(data)
        for i in range(off, min(len(d), off + cnt)):
            d[i] = rng.randrange(256)
        out["noise_%d" % k] = bytes(d)
    return out


def id3_variants(data):
    """Leading ID3v2 tags of several shapes before the same stream (AudioFile steps over them before it sniffs)."""
    return {
        "id3_empty": id3v2(0) + data,
        "id3_10": id3v2(10) + data,
        "id3_1000": id3v2(1000) + data,
        "id3_v3": id3v2(40, version=3) + data,
        "id3_footer": id3v2(20, flags=0x10) + bytes(10) + data,
        "id3_chain": id3v2(16) + id3v2(8) + data,
        "id3_huge_size": b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f" + data,
        "id3v1_prefix": b"TAG" + bytes(125) + data,
    }


RENAMES = ("mp3", "MP3", "mp2", "mp1", "mpa", "mpeg", "wav", "aif", "aiff", "bin", "", "caf", "aac", "adts", "mp4", "m4b", "3gp", "m4a", "loas", "latm", "ogg", "opus", "txt")


# --------------------------------------------------------------------------------------------
# the generated corpora of CORPUS_DIR (made outside this repository) as candidate inputs
# --------------------------------------------------------------------------------------------
CORPUS_FAMILY = {"ffn": "corpus-a-ffmpeg", "ffat": "corpus-a-ffmpeg-at", "afc": "corpus-a-afconvert", "aref": "corpus-a-apple-writer",
               "syn": "corpus-a-synthetic", "var": "corpus-a-variant", "apple": "corpus-a-apple-runs"}
# probes the replays single out: always part of the pack
PINNED_PROBES = {"caf_rate48k_asc44k", "caf_truncated", "adts_nrdb1_mid"}


def load_corpora(corpus, max_bytes=MAX_FILE_BYTES):
    """-> (candidates, syntax feature dict per key). Every corpus file of at most `max_bytes`."""
    cands, corpus_syntax = [], {}
    m1 = load_json(os.path.join(corpus, "corpus-a", "manifest.json"))["items"]
    for it in m1:
        path = os.path.join(corpus, it["path"])
        if not os.path.exists(path) or os.path.getsize(path) > max_bytes:
            continue
        with open(path, "rb") as f:
            data = f.read()
        prefix = it["id"].split("_")[0]
        key = "corpus-a/" + it["id"]
        cands.append(Cand(key, it["id"] + os.path.splitext(path)[1], data, CORPUS_FAMILY.get(prefix, "corpus-a-other"),
                          {"set": "corpus-a", "id": it["id"], "category": it["category"]}))
        ms = (it.get("features") or {}).get("ms", {})
        mask_tags = ["MS-mask1"] * bool(ms.get("1")) + ["MS-mask2"] * bool(ms.get("2"))
        corpus_syntax[key] = {"tags": list(it.get("syntax_tags", [])) + mask_tags, "scope": it.get("scope"), "asc": (it.get("features") or {}).get("asc")}
    m2 = load_json(os.path.join(corpus, "corpus-b", "manifest.json"))
    for it in m2:
        path = os.path.join(corpus, "corpus-b", it["file"]) if "file" in it else os.path.join(corpus, it["path"])
        if not os.path.exists(path) or os.path.getsize(path) > max_bytes:
            continue
        with open(path, "rb") as f:
            data = f.read()
        key = "corpus-b/" + it["id"]
        cands.append(Cand(key, it["id"] + os.path.splitext(path)[1], data, "corpus-b-ffmpeg" if "file" in it else "corpus-b-apple-runs",
                          {"set": "corpus-b", "id": it["id"]}))
    probes_dir = os.path.join(corpus, "probes")
    with open(os.path.join(probes_dir, "probes.tsv")) as f:
        header = f.readline().rstrip("\n").split("\t")
        for line in f:
            row = dict(zip(header, line.rstrip("\n").split("\t")))
            path = os.path.join(probes_dir, "probes", row["id"])
            if not os.path.exists(path) or os.path.getsize(path) > max_bytes:
                continue
            with open(path, "rb") as f2:
                data = f2.read()
            prefix = re.split(r"[_0-9]", row["id"])[0]
            ext = "." + row["hint"] if row["hint"] else ""
            c = Cand("probe/" + row["id"], row["id"] + ext, data, "probe-" + prefix, {"set": "container-probes", "id": row["id"]})
            if row["id"] in PINNED_PROBES:
                c.meta["pinned"] = True
            cands.append(c)
    return cands, corpus_syntax


def syntax_of(corpus, path):
    """The bitstream analyzer that comes with the corpora (`analyzer/aacdump.py`) as a tagger (only used to label inputs for the selection)."""
    sys.path.insert(0, os.path.join(corpus, "analyzer"))
    import aacdump
    try:
        aus, sfi, chcfg, inf = aacdump.read_aus(path)
        rate = aacdump.SR_TABLE[sfi] if sfi is not None else None
        parsed = [aacdump.parse_au(a, sfi, collect_spectrum=False) for a in aus]
        S = aacdump.summarize(parsed, inf, rate)
    except (Exception, SystemExit):
        return {"tags": [], "scope": None, "asc": None}
    els, ms, ws = S.get("elements", {}), S.get("ms", {}), S.get("window_sequence", {})
    t = []
    if S.get("pns_bands"): t.append("PNS")
    if S.get("intensity_bands"): t.append("INTENSITY")
    if S.get("tns", {}).get("present"): t.append("TNS")
    if ms.get("1") or ms.get(1): t.append("MS-mask1")
    if ms.get("2") or ms.get(2): t.append("MS-mask2")
    if ws.get("EIGHT_SHORT"): t.append("SHORT")
    if ws.get("LONG_START") or ws.get("LONG_STOP"): t.append("START-STOP")
    if S.get("window_shape", {}).get("1"): t.append("KBD")
    if S.get("pulse_channels"): t.append("PULSE")
    for k in ("DSE", "FIL", "PCE", "LFE"):
        if k in els: t.append(k)
    asc = ((inf or {}).get("esds") or {}).get("asc")
    if (asc or {}).get("channel_config") == 0: t.append("ASC-PCE")
    if sum(els.get(k, 0) > 0 for k in ("SCE", "CPE", "LFE")) > 1: t.append("MULTI-ELEMENT")
    return {"tags": t, "scope": "lc", "asc": asc}


# --------------------------------------------------------------------------------------------
# features and expectations
# --------------------------------------------------------------------------------------------
EXT_CONTAINER = {"m4a": "mp4", "mp4": "mp4", "m4b": "mp4", "3gp": "mp4", "3g2": "mp4", "mov": "mp4", "aac": "adts", "adts": "adts",
                 "loas": "loas", "latm": "loas", "caf": "caf"}
MPEG_EXTENSIONS = {"mp1", "mp2", "mp3", "mpa", "mpeg"}


def name_ext(name):
    return name.rsplit(".", 1)[1].lower() if "." in name else ""


def inner_container(data):
    """Container behind leading ID3v2 / ID3v1 blocks, and the number of bytes skipped."""
    pos = 0
    while pos + 10 <= len(data) and data[pos:pos + 3] == b"ID3":
        size = ((data[pos + 6] & 0x7F) << 21) | ((data[pos + 7] & 0x7F) << 14) | ((data[pos + 8] & 0x7F) << 7) | (data[pos + 9] & 0x7F)
        pos += 10 + size + (10 if data[pos + 5] & 0x10 else 0)
        if pos > len(data):
            return "other", pos
    if pos == 0 and data[:3] == b"TAG":
        pos = 128
    return sniff(data[pos:pos + 16]), pos


def structure_tags(c):
    data, tags = c.data, set()
    kind, skipped = inner_container(data)
    tags.add("container:" + kind)
    if skipped:
        tags.add("id3:leading")
    ext = name_ext(c.name)
    tags.add("name:" + (ext or "none"))
    if EXT_CONTAINER.get(ext) not in (None, kind) or (ext in ("mp3", "mp2", "mp1", "mpa", "mpeg", "wav", "aif", "aiff", "bin", "", "ogg", "opus", "txt") and kind in ("mp4", "adts", "loas", "caf")):
        tags.add("misnamed")
    body = data[skipped:] if skipped and skipped <= len(data) else data
    if kind == "mp4":
        nodes, tail = parse_mp4(body)
        tops = [n.typ.decode("latin1") for n in nodes]
        if "moov" in tops and "mdat" in tops:
            tags.add("mp4:moov-last" if tops.index("moov") > tops.index("mdat") else "mp4:moov-first")
        for t in ("free", "wide", "skip", "uuid", "moof", "mdat"):
            if t in tops:
                tags.add("mp4:" + t)
        if tops.count("mdat") > 1:
            tags.add("mp4:multiple-mdat")
        tracks = sum(1 for _ in iter_nodes(nodes, b"trak"))
        tags.add("mp4:tracks=%d" % min(tracks, 3))
        if tail or not nodes:
            tags.add("mp4:damaged")
        if b"esds" not in body:
            tags.add("mp4:no-esds")
        if b"iTunSMPB" in body:
            tags.add("trim:itunsmpb")
        if b"elst" in body:
            tags.add("trim:elst")
        if b"sgpd" in body and b"roll" in body:
            tags.add("trim:roll-group")
        if b"co64" in body:
            tags.add("mp4:co64")
        brand = body[8:12].decode("latin1") if body[4:8] == b"ftyp" else None
        if brand:
            tags.add("mp4:brand:" + brand.strip())
        at = find_asc(body)
        if at:
            asc = parse_asc(body[at[0]:at[0] + at[1]])
            if asc.get("sfi") is not None:
                tags.add("asc:sfi:%d" % asc["sfi"])
            if asc.get("aot") is not None:
                tags.add("asc:aot:%d" % asc["aot"])
            if asc.get("channels") is not None:
                tags.add("asc:chcfg:%d" % asc["channels"])
            if asc.get("sbr"):
                tags.add("asc:sbr")
        if tail:
            tags.add("truncated")
    elif kind == "adts":
        frames = adts_frames(body)
        if frames:
            first = body[frames[0][0]:frames[0][0] + 7]
            tags.add("asc:sfi:%d" % ((first[2] >> 2) & 15))
            tags.add("adts:profile:%d" % (first[2] >> 6))
            if any(not body[f[0] + 1] & 1 for f in frames):
                tags.add("adts:crc")
            if any(body[f[0] + 6] & 3 for f in frames):
                tags.add("adts:multi-block")
            if body[frames[0][0] + 1] & 8:
                tags.add("adts:mpeg2-id")
            if len({(body[f[0] + 2] & 0xFD, body[f[0] + 3] & 0xC0) for f in frames}) > 1:
                tags.add("adts:cfg-change")
            if sum(f[2] for f in frames) < len(body):
                tags.add("truncated")
        else:
            tags.add("adts:no-frame")
    elif kind == "caf":
        chunks = caf_chunks(body)
        types = {t for t, s, z in chunks}
        for t in ("kuki", "pakt", "chan", "info", "free"):
            if t in types:
                tags.add("caf:" + t)
        if chunks and chunks[-1][1] + chunks[-1][2] > len(body):
            tags.add("truncated")
    return tags


def outcome_tags(rec):
    tags = set()
    o = rec["open"]
    if not o["ok"]:
        e = o["error"]
        tags.add("open:err:%s" % (fourcc(e["status"]) if "status" in e else "engine"))
        tags.add("open:refused")
        return tags
    s = o["source"]
    tags.add("open:ok")
    tags.add("fmt:" + fourcc(s["format_id"]).strip())
    if fourcc(s["format_id"]) in (".mp1", ".mp2", ".mp3"):
        tags.add("reader:mpeg")
    tags.add("rate:%d" % int(s["sample_rate"]))
    tags.add("ch:%d" % s["channels"])
    tags.add("fpp:%d" % s["frames_per_packet"])
    sfi = asbd_sfi(s["sample_rate"])
    if sfi is not None:
        tags.add("sfi:%d" % sfi)
    for shape in ("planar", "inter"):
        e = rec[shape]["error"]
        if e:
            tags.add("%s:err:%s" % (shape, fourcc(e["status"]) if "status" in e else "engine"))
    if rec["inter"]["error"]:
        tags.add("read:err:%s" % fourcc(rec["inter"]["error"]["status"]))
    else:
        tags.add("read:ok")
    if rec["inter"]["frames"] == 0:
        tags.add("frames:0")
    if rec["inter"]["short"] >= 1:
        tags.add("read:short-last")
    if rec["inter"]["short"] >= 2:
        tags.add("read:short-mid-stream")
    if s["sample_rate"] <= 22050:
        tags.add("rate:le22050")
    return tags


def classify(c):
    """-> (expect, reason): what the portable reader must do for this file, derived from Apple's recording."""
    o = c.rec["open"]
    if not o["ok"]:
        e = o["error"]
        if "status" in e:
            return "apple_error", "Apple refuses: %s %s" % (e["operation"], fourcc(e["status"]))
        return "apple_error", "the engine's reader refuses before AudioToolbox sees the file: %s" % e["other"]
    fmt = fourcc(o["source"]["format_id"])
    if fmt in (".mp1", ".mp2", ".mp3"):
        # AudioFile commits to its MPEG audio types for these names and reads the bytes as MPEG frames, whatever they are:
        # the port's exact MP3 reader is that reader
        if name_ext(c.name) in MPEG_EXTENSIONS:
            return "exact", "the name selects the MPEG audio reader: Apple reads the AAC bytes as MPEG frames, the port's exact MP3 reader must too"
        return "refuse_codec", "Apple opens it as '%s' (MPEG audio inside a container the port reads only for AAC and ALAC): typed refusal" % fmt
    if o["source"]["format_id"] == AAC_LC:
        return "exact", "AAC-LC: identical to Apple at the file's own rate"
    return "refuse_codec", "Apple decodes '%s' (not AAC-LC): typed refusal" % fmt


def audio_seconds(rec):
    """Seconds of audio a replay decodes for this recording (the longer of the two shapes)."""
    o = rec["open"]
    if not o["ok"] or not o["source"]["sample_rate"]:
        return 0.0
    return max(rec["planar"]["frames"], rec["inter"]["frames"]) / o["source"]["sample_rate"]


# --------------------------------------------------------------------------------------------
# generated families: bases, mutations, renames, ID3 prefixes
# --------------------------------------------------------------------------------------------
def generate_stream_bases(workdir):
    """ADTS / CAF / LOAS bases of the mutation families (afconvert, AAC-LC stereo 44.1 kHz, 64 kbit/s)."""
    out = []
    if shutil.which("afconvert") is None:
        return out
    d = os.path.join(workdir, "bases")
    os.makedirs(d, exist_ok=True)
    wav = os.path.join(d, "b.wav")
    write_wav16(wav, 44100, 2, 12000, seed=5)
    for fmt, ext in (("adts", ".aac"), ("caff", ".caf"), ("loas", ".loas")):
        path = os.path.join(d, "b" + ext)
        r = subprocess.run(["afconvert", "-f", fmt, "-d", "aac", "-b", "64000", wav, path], capture_output=True, text=True)
        if r.returncode != 0:
            fail("afconvert base %s: %s" % (fmt, r.stderr))
        with open(path, "rb") as f:
            out.append(Cand("afconvert/mutbase_" + fmt, "mutbase_%s%s" % (fmt, ext), pin_mp4_times(f.read()), "mutation-base",
                            {"kind": "afconvert", "args": ["-f", fmt, "-d", "aac", "-b", "64000"], "rate": 44100, "channels": 2, "frames": 12000}))
    return out


def mutation_candidates(by_key):
    out = []
    plans = (("fresh/mutbase_st44", mp4_mutations, "mut-mp4", ".m4a"), ("fresh/mutbase_m48", mp4_mutations, "mut-mp4", ".m4a"),
             ("fresh/mutbase_6ch", mp4_mutations, "mut-mp4", ".m4a"), ("afconvert/mutbase_adts", adts_mutations, "mut-adts", ".aac"),
             ("afconvert/mutbase_caff", caf_mutations, "mut-caf", ".caf"), ("afconvert/mutbase_loas", loas_mutations, "mut-loas", ".loas"))
    for key, fn, family, ext in plans:
        base = by_key.get(key)
        if base is None:
            continue
        stem = base.name.rsplit(".", 1)[0]
        for op, data in fn(stem, base.data).items():
            out.append(Cand("mut/%s/%s" % (stem, op), "%s__%s%s" % (stem, op, ext), data, family,
                            {"kind": "mutation", "base": key, "op": op}, tags=["op:" + re.sub(r"[0-9]+", "#", op)] + bad_au_tags(op)))
    return out


def bad_au_tags(op):
    m = re.match(r"au_random_\d+_x(\d+)$", op)
    tags = []
    if m:
        tags.append("bad-au:consecutive:%d" % min(int(m.group(1)), 4))
    if op.startswith("stsz_") and op.endswith(("_over", "_over2", "_huge", "_max")):
        tags.append("au:oversize")
    return tags


def rename_and_id3_candidates(by_key):
    out = []
    for key in ("fresh/st44_tone", "afconvert/lc_adts_44s", "afconvert/lc_caf_44s", "afconvert/lc_loas_44s", "afconvert/lc_mp4_44s", "afconvert/he1_m4a_44s"):
        base = by_key.get(key)
        if base is None:
            continue
        stem, own = base.name.rsplit(".", 1)
        for ext in RENAMES:
            if ext == own:
                continue
            out.append(Cand("rename/%s/%s" % (stem, ext or "none"), stem + ("." + ext if ext else ""), base.data, "rename",
                            {"kind": "rename", "base": key, "extension": ext}, tags=["rename"]))
    for key in ("fresh/mutbase_st44", "afconvert/mutbase_adts", "afconvert/mutbase_caff", "afconvert/mutbase_loas", "afconvert/lc_mp4_44s"):
        base = by_key.get(key)
        if base is None:
            continue
        stem, ext = base.name.rsplit(".", 1)
        for op, data in id3_variants(base.data).items():
            out.append(Cand("id3/%s/%s" % (stem, op), "%s__%s.%s" % (stem, op, ext), data, "id3", {"kind": "id3-prefix", "base": key, "op": op}, tags=["op:" + op]))
    return out


def probe_name_tags(c):
    tags = set()
    if c.origin.get("set") == "container-probes":
        pid = c.origin["id"]
        tags.add("probe:" + re.split(r"[_0-9]", pid)[0])
        m = re.match(r"badau_(lead|mid)_(\d+)$", pid)
        if m:
            tags.add("bad-au:consecutive:%d" % min(int(m.group(2)), 4))
        if pid.startswith("badau"):
            tags.add("bad-au:probe")
        if "oversize" in pid or "huge" in pid or "big" in pid:
            tags.add("au:oversize")
        if pid.startswith("zero") or "zero_packets" in pid or "pakt_zero" in pid:
            tags.add("zero-packets")
    return tags


# --------------------------------------------------------------------------------------------
# features, selection
# --------------------------------------------------------------------------------------------
def syntax_worker(args):
    """One bitstream analysis in a child interpreter (the analyzer is slow Python; children keep the pool parallel)."""
    corpus, path = args
    try:
        r = subprocess.run([sys.executable, os.path.abspath(__file__), "syntax", "--corpus", corpus, "--path", path], capture_output=True, timeout=60)
        return json.loads(r.stdout.decode())
    except (subprocess.TimeoutExpired, ValueError):
        return {"tags": [], "scope": None, "asc": None}


def compute_features(cands, corpus, corpus_syntax, workers=8):
    todo = {}
    for c in cands:
        if c.key in corpus_syntax:
            continue
        o = c.rec["open"]
        if o["ok"] and o["source"]["format_id"] == AAC_LC and not c.family.startswith(("mut-", "id3", "probe-", "rename")):
            todo[c.key] = c.path
    syn = {}
    if corpus and todo:
        with cf.ThreadPoolExecutor(workers) as ex:
            for key, res in zip(todo, ex.map(syntax_worker, [(corpus, p) for p in todo.values()])):
                syn[key] = res
    for c in cands:
        s = corpus_syntax.get(c.key) or syn.get(c.key) or {}
        feats = set(c.tags) | structure_tags(c) | outcome_tags(c.rec) | probe_name_tags(c)
        feats |= {"syntax:" + t.lower() for t in s.get("tags", [])}
        asc = s.get("asc")
        if asc and asc.get("channel_config") is not None:
            feats.add("asc:chcfg:%d" % asc["channel_config"])
        feats.add("family:" + c.family)
        feats.add("set:" + c.origin.get("set", c.origin.get("kind", "?")))
        c.features = feats
        c.sig = signature(c)
        # only what the port decodes costs replay time; a refused file costs its open
        c.decode_seconds = audio_seconds(c.rec) if classify(c)[0] == "exact" else 0.0
        c.cost = 1.0 * c.decode_seconds + len(c.data) / 40000.0


def signature(c):
    """What the outcome of a file looks like, coarsely: open result / format / read result, and the kind of damage."""
    o = c.rec["open"]
    if not o["ok"]:
        e = o["error"]
        outcome = ("refused", e.get("operation", "")[-14:], e.get("status"))
    else:
        outcome = ("ok", o["source"]["format_id"], o["source"]["channels"], c.rec["inter"]["error"] and c.rec["inter"]["error"].get("status"),
                   c.rec["planar"]["error"] and c.rec["planar"]["error"].get("status"))
    return (c.origin.get("kind", c.family.split("-")[0]), c.origin.get("op") and re.sub(r"[0-9]+", "#", c.origin["op"]), outcome)


# tags that must be present with at least this many files wherever candidates exist (everything the task asks for)
REQUIRED = {}
for _t in range(13):
    REQUIRED["asc:sfi:%d" % _t] = 2
for _t in range(1, 9):
    REQUIRED["ch:%d" % _t] = 4
for _t in ("pns", "intensity", "ms-mask1", "ms-mask2", "tns", "short", "start-stop", "pulse", "kbd", "fil", "dse", "lfe", "multi-element", "asc-pce", "pce"):
    REQUIRED["syntax:" + _t] = 4
for _t in ("mp4", "adts", "loas", "caf"):
    REQUIRED["container:" + _t] = 8
for _t in ("m4a", "aac", "mp4", "m4b", "3gp", "3g2", "caf", "loas", "latm", "mov", "mka", "mp3", "wav", "aiff", "bin", "none", "mpa", "mpeg", "mp2", "mp1", "ogg"):
    REQUIRED["name:" + _t] = 2
for _t in ("mp4:moov-first", "mp4:moov-last", "mp4:free", "mp4:wide", "mp4:tracks=2", "mp4:no-esds", "truncated", "trim:itunsmpb", "trim:elst", "trim:roll-group",
           "adts:crc", "adts:multi-block", "adts:mpeg2-id", "adts:profile:0", "adts:profile:2", "adts:profile:3", "adts:cfg-change",
           "bad-au:consecutive:1", "bad-au:consecutive:2", "bad-au:consecutive:3", "bad-au:consecutive:4", "au:oversize", "zero-packets", "misnamed", "id3:leading",
           "fmt:aach", "fmt:aacp", "fmt:aacl", "fmt:aace", "rate:le22050", "read:err:bada", "frames:0", "caf:kuki", "caf:pakt", "caf:chan", "he:implicit-signalling"):
    REQUIRED[_t] = 4
for _t in ("wht?", "dta?", "typ?", "fmt?", "sync", "rate", "chk?", "!dat", "prof"):
    REQUIRED["open:err:" + _t] = 2
for _t in (8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000):
    REQUIRED["rate:%d" % _t] = 3
REQUIRED["read:err:bada"] = 12
REQUIRED["read:err:pck?"] = 2
REQUIRED["misnamed"] = 24
REQUIRED["id3:leading"] = 10

# (family prefix predicate, number of files to choose by greedy coverage); `ALL` families are taken whole
ALL_FAMILIES = ("fresh-accepted", "fresh-length", "mutation-base", "afconvert-he", "afconvert-nonlc")
POOLS = (
    ("fresh-content", 8), ("afconvert-lc", 14),
    ("corpus-a", 78), ("corpus-b-ffmpeg", 36), ("corpus-b-apple-runs", 3), ("probe", 72),
    ("mut-mp4", 48), ("mut-adts", 20), ("mut-caf", 18), ("mut-loas", 8), ("rename", 32), ("id3", 12),
)


def select(cands):
    chosen, counts, sigs = {}, collections.Counter(), collections.Counter()

    def add(c):
        chosen[c.key] = c
        counts.update(c.features)
        sigs[c.sig] += 1

    def score(c):
        tag_score = sum(1.0 / (1 + counts[t]) for t in sorted(c.features) if not t.startswith(("family:", "set:", "name:")))
        return (tag_score + 4.0 / (1 + sigs[c.sig])) / (1.0 + c.cost), c.key
    ordered = sorted(cands, key=lambda c: c.key)
    for c in ordered:
        if c.family in ALL_FAMILIES or c.meta.get("pinned"):
            add(c)
    for tag, minimum in sorted(REQUIRED.items()):
        while counts[tag] < minimum:
            pool = [c for c in ordered if tag in c.features and c.key not in chosen]
            if not pool:
                break
            add(min(pool, key=lambda c: (c.cost, c.key)))
    for prefix, k in POOLS:
        pool = [c for c in ordered if c.family.startswith(prefix) and c.key not in chosen]
        have = sum(1 for c in chosen.values() if c.family.startswith(prefix))
        for _ in range(max(0, k - have)):
            if not pool:
                break
            best = max(pool, key=score)
            pool.remove(best)
            add(best)
    return [chosen[k] for k in sorted(chosen)]


def check_required(chosen, available):
    counts = collections.Counter(t for c in chosen for t in c.features)
    have_any = collections.Counter(t for c in available for t in c.features)
    return {t: (counts[t], m, have_any[t]) for t, m in REQUIRED.items() if counts[t] < min(m, have_any[t])}, \
        sorted(t for t in REQUIRED if have_any[t] == 0)


# --------------------------------------------------------------------------------------------
# build
# --------------------------------------------------------------------------------------------
def host_description():
    def sh(*a):
        return subprocess.run(a, capture_output=True, text=True).stdout.strip()
    return "macOS %s (%s) %s %s" % (sh("sw_vers", "-productVersion"), sh("sw_vers", "-buildVersion"), sh("uname", "-m"), sh("sysctl", "-n", "machdep.cpu.brand_string"))


def stage(cands, root):
    paths = {}
    for c in cands:
        d = os.path.join(root, hashlib.sha1(c.key.encode()).hexdigest()[:14])
        os.makedirs(d, exist_ok=True)
        c.path = os.path.join(d, c.name)
        with open(c.path, "wb") as f:
            f.write(c.data)
        paths[c.key] = c.path
    return paths


def classify_variant(base_expect, rate, rec):
    o = rec["open"]
    if not o["ok"]:
        e = o["error"]
        return ("apple_error", "Apple refuses at this client rate: %s" % (fourcc(e["status"]) if "status" in e else e["other"]))
    s = o["source"]
    if base_expect == "apple_error":
        fail("Apple opens a file at a client rate that it refuses at its own: no expectation defined")
    if s["format_id"] != AAC_LC:
        return ("refuse_codec", "not AAC-LC")
    if rate == s["sample_rate"]:
        return ("exact", "client rate equals the file rate")
    return ("refuse_src", "Apple converts %d Hz to %d Hz; the port refuses sample-rate conversion" % (s["sample_rate"], rate))


def pick_variants(index, c, expect):
    """client rates (besides the file's own) recorded for this file"""
    o = c.rec["open"]
    if not o["ok"]:
        e = o["error"]
        return [44100, 8000] if index % 4 == 0 and "status" in e else []
    rate = int(o["source"]["sample_rate"])
    out = []
    if rate <= 22050:
        out.append(44100)
        if index % 3 == 0:
            out.append(48000)
    if index % 9 == 0 and rate > 22050:
        out.append(48000 if rate != 48000 else 44100)
    if index % 27 == 0:
        out.append(22050 if rate > 22050 else 8000)
    return sorted(set(out))


def entry_of(c, expect, reason):
    open_, info = clean_open(c.rec["open"])
    e = {"id": c.key, "name": c.name, "family": c.family, "origin": c.origin, "sha256": c.sha256, "length": len(c.data),
         "features": sorted(c.features)[:MAX_FEATURES * 3], "expect": expect, "expect_reason": reason, "open": open_}
    if info is not None:
        e["info"] = info
        e["planar512"] = clean_stream(c.rec["planar"])
        e["inter512"] = clean_stream(c.rec["inter"])
        if "inter_after_planar_refusal" in c.rec:
            e["inter_after_planar_refusal"] = clean_stream(c.rec["inter_after_planar_refusal"])
        if "after_failure" in c.rec:
            a = c.rec["after_failure"]
            e["after_failure"] = None if a is None else {"first_status": status_i32(a["first_status"]), "later": [[status_i32(s), f] for s, f in a["later"]]}
    return e


def write_pack(chosen, out):
    parts, placed, current = [], {}, bytearray()

    def flush():
        if current:
            name = "pack-%d.bin" % len(parts)
            with open(os.path.join(out, name), "xb") as f:
                f.write(current)
            parts.append({"file": name, "bytes": len(current), "sha256": sha256_bytes(bytes(current))})
            current.clear()
    for c in sorted(chosen, key=lambda c: c.key):
        if c.sha256 in placed:
            continue
        if current and len(current) + len(c.data) > MAX_PART_BYTES:
            flush()
        placed[c.sha256] = (len(parts), len(current))
        current.extend(c.data)
    flush()
    return parts, placed


def build(args):
    out = os.path.abspath(args.out)
    if os.path.exists(out) and os.listdir(out):
        fail("%s exists and is not empty: fixtures are create-only" % out)
    work = os.path.abspath(args.work)
    os.makedirs(work, exist_ok=True)
    probe = os.path.abspath(args.probe)
    corpus = os.path.abspath(args.corpus)

    cands, corpus_syntax = load_corpora(corpus)
    print("corpus candidates: %d" % len(cands), file=sys.stderr)
    cands += generate_fresh(probe, work)
    cands += generate_afconvert(work)
    cands += generate_stream_bases(work)
    by_key = {c.key: c for c in cands}
    cands += mutation_candidates(by_key)
    cands += rename_and_id3_candidates(by_key)
    seen, unique = set(), []
    for c in cands:
        if (c.name, c.sha256) in seen or len(c.data) > MAX_FILE_BYTES:
            continue
        seen.add((c.name, c.sha256))
        unique.append(c)
    cands = unique
    print("candidates: %d (%d bytes)" % (len(cands), sum(len(c.data) for c in cands)), file=sys.stderr)

    paths = stage(cands, os.path.join(work, "stage"))
    recs = record_files(probe, paths)
    bad = [k for k, r in recs.items() if "crash" in r]
    if bad:
        print("excluded (recorder crash / nondeterminism): %s" % ", ".join("%s=%s" % (k, recs[k]["crash"]) for k in bad[:20]), file=sys.stderr)
    cands = [c for c in cands if "crash" not in recs[c.key]]
    for c in cands:
        c.rec = recs[c.key]
    compute_features(cands, corpus, corpus_syntax)
    chosen = select(cands)
    short, missing = check_required(chosen, cands)
    total = sum(len(c.data) for c in {c.sha256: c for c in chosen}.values())
    seconds = sum(c.decode_seconds for c in chosen)
    print("selected %d files, %d bytes, %.1f s of decoded audio; below quota: %s; no candidate: %s" % (len(chosen), total, seconds, short, missing), file=sys.stderr)
    if total > MAX_PACK_BYTES or seconds > MAX_AUDIO_SECONDS:
        fail("over budget: %d bytes (max %d), %.1f s (max %.1f)" % (total, MAX_PACK_BYTES, seconds, MAX_AUDIO_SECONDS))

    entries = []
    classes = {c.key: classify(c) for c in chosen}
    variant_jobs, owners = {}, {}
    for i, c in enumerate(chosen):
        for rate in pick_variants(i, c, classes[c.key][0]):
            variant_jobs[(c.key, rate)] = c.path
            owners[(c.key, rate)] = c
    variants = {}
    for rate in sorted({r for _, r in variant_jobs}):
        group = {k: p for (k, r), p in variant_jobs.items() if r == rate}
        for k, rec in record_files(probe, group, extra=["--rate", str(rate)]).items():
            if "crash" in rec:
                fail("rate variant %s %d: %s" % (k, rate, rec["crash"]))
            variants[(k, rate)] = rec
    os.makedirs(out, exist_ok=True)
    parts, placed = write_pack(chosen, out)
    for c in chosen:
        expect, reason = classes[c.key]
        e = entry_of(c, expect, reason)
        part, offset = placed[c.sha256]
        e["part"], e["offset"] = part, offset
        vs = []
        for (k, rate), rec in sorted(variants.items()):
            if k != c.key:
                continue
            v_open, v_info = clean_open(rec["open"])
            v_expect, v_reason = classify_variant(expect, rate, rec)
            v = {"client_rate": rate, "expect": v_expect, "expect_reason": v_reason, "open": v_open}
            if v_info is not None:
                v["info"] = v_info
                v["planar512"] = clean_stream(rec["planar"])
                v["inter512"] = clean_stream(rec["inter"])
            vs.append(v)
        if vs:
            e["rate_variants"] = vs
        entries.append(e)
    all_bytes = b"".join(open(os.path.join(out, p["file"]), "rb").read() for p in parts)
    manifest = {
        "schema": SCHEMA, "generator": GENERATOR, "block": BLOCK,
        "apple": {"host": host_description(), "probe_sha256": sha256_file(probe)},
        "pack": {"parts": parts, "bytes": len(all_bytes), "sha256": sha256_bytes(all_bytes)},
        "files": entries,
    }
    with open(os.path.join(out, "manifest.json"), "x") as f:
        f.write(dumps(manifest))
    summarize(manifest)


def dumps(doc):
    return json.dumps(doc, indent=1, ensure_ascii=True) + "\n"


def summarize(manifest):
    files = manifest["files"]
    print("%d files, %d pack bytes in %d parts" % (len(files), manifest["pack"]["bytes"], len(manifest["pack"]["parts"])))
    print("expect:", dict(collections.Counter(f["expect"] for f in files)))
    print("families:", dict(sorted(collections.Counter(f["family"] for f in files).items())))


# --------------------------------------------------------------------------------------------
# read sizes
# --------------------------------------------------------------------------------------------
def load_pack(dirpath):
    manifest = load_json(os.path.join(dirpath, "manifest.json"))
    parts = []
    for p in manifest["pack"]["parts"]:
        with open(os.path.join(dirpath, p["file"]), "rb") as f:
            parts.append(f.read())
    return manifest, parts


def file_bytes(entry, parts):
    return parts[entry["part"]][entry["offset"]:entry["offset"] + entry["length"]]


def readsizes(args):
    dirpath = os.path.abspath(args.dir)
    target = os.path.join(dirpath, "readsizes.json")
    if os.path.exists(target):
        fail("%s exists: fixtures are create-only" % target)
    probe = os.path.abspath(args.probe)
    manifest, parts = load_pack(dirpath)
    work = os.path.abspath(args.work)
    os.makedirs(work, exist_ok=True)
    paths, entries = {}, [e for e in manifest["files"] if e["open"]["ok"]]
    for i, e in enumerate(entries):
        d = os.path.join(work, "rs", "%04d" % i)
        os.makedirs(d, exist_ok=True)
        paths[e["id"]] = os.path.join(d, e["name"])
        with open(paths[e["id"]], "wb") as f:
            f.write(file_bytes(e, parts))
    # the control: the recorder at 512 frames must reproduce the manifest for every file it is asked to record
    control = record_files(probe, paths)
    for e in entries:
        rec = control[e["id"]]
        if "crash" in rec or clean_stream(rec["planar"]) != e["planar512"] or clean_stream(rec["inter"]) != e["inter512"]:
            fail("%s: decode_probe at 512 frames does not reproduce the manifest" % e["id"])
    # the replay's chunk-size test (tests/portable_aac_decode.rs, reads_of_any_size_deliver_the_same_stream_when_nothing_fails)
    # asserts that other read sizes deliver the 512-frame stream whenever nothing fails: Apple must agree for the files it uses
    chunk_files = []
    for e in entries:
        pl, it = e["planar512"], e["inter512"]
        seconds = pl["frames"] / e["info"]["source"]["sample_rate"]
        if e["expect"] == "exact" and not pl["error"] and not it["error"] and pl["frames"] and seconds <= 0.6:
            chunk_files.append(e)
        if len(chunk_files) == 30:
            break
    for size in CHUNK_SIZES:
        results = record_files(probe, {e["id"]: paths[e["id"]] for e in chunk_files}, extra=["--block", str(size)])
        for e in chunk_files:
            rec = results[e["id"]]
            if "crash" in rec or rec["planar"]["sha256"] != e["planar512"]["sha256"] or rec["inter"]["sha256"] != e["inter512"]["sha256"] or rec["planar"]["error"] or rec["inter"]["error"]:
                fail("%s: Apple's stream differs at %d-frame reads: the chunk-size test would be wrong" % (e["id"], size))
    print("chunk-size check: %d clean files x %d sizes identical to the 512-frame stream" % (len(chunk_files), len(CHUNK_SIZES)))
    # every file whose result depends on the read size (a read failure, a short read in mid-stream, a documented deviation), then a
    # sample of the clean ones for contrast, within a budget of decoded audio
    selected, budget = [], 40.0

    def seconds(e):
        return e["planar512"]["frames"] / e["info"]["source"]["sample_rate"]
    for i, e in enumerate(entries):
        if e["expect"] != "exact":
            continue
        sensitive = bool(e["planar512"]["error"] or e["inter512"]["error"]) or e["inter512"]["short"] >= 2
        if sensitive or (i % 6 == 0 and seconds(e) <= 0.8 and budget > 0):
            selected.append(e)
            budget -= 0 if sensitive else seconds(e)
    files = []
    for size in READ_SIZES:
        results = record_files(probe, {e["id"]: paths[e["id"]] for e in selected}, extra=["--block", str(size)])
        for e in selected:
            rec = results[e["id"]]
            if "crash" in rec:
                fail("%s at %d frames: %s" % (e["id"], size, rec["crash"]))
            files.append((e["id"], size, rec))
    by_id = collections.OrderedDict((e["id"], {"id": e["id"], "sha256": e["sha256"], "planar": {}, "inter": {}}) for e in selected)
    for fid, size, rec in files:
        by_id[fid]["planar"][str(size)] = clean_stream(rec["planar"])
        by_id[fid]["inter"][str(size)] = clean_stream(rec["inter"])
    doc = {"schema": SIZES_SCHEMA, "generator": GENERATOR, "block_sizes": list(READ_SIZES), "pack_sha256": manifest["pack"]["sha256"],
           "apple": {"probe_sha256": manifest["apple"]["probe_sha256"]}, "files": list(by_id.values())}
    head = {k: v for k, v in doc.items() if k != "files"}
    text = json.dumps(head, indent=1)
    lines = [json.dumps(f, separators=(",", ":")) for f in doc["files"]]
    with open(target, "x") as f:
        f.write(text[:-2] + ',\n "files": [\n  ' + ",\n  ".join(lines) + "\n ]\n}\n")
    differing = sum(1 for f in doc["files"] for shape in ("planar", "inter") for s in READ_SIZES
                    if f[shape][str(s)] != next(e for e in entries if e["id"] == f["id"])[shape + "512"])
    print("wrote %s: %d files x %d sizes x 2 shapes, %d recordings differ from the 512-frame one" % (target, len(doc["files"]), len(READ_SIZES), differing))


# --------------------------------------------------------------------------------------------
# verification without Apple
# --------------------------------------------------------------------------------------------
EXPECTS = ("exact", "refuse_codec", "refuse_src", "apple_error")


def verify_pack(args):
    dirpath = os.path.abspath(args.dir)
    manifest, parts = load_pack(dirpath)
    if manifest["schema"] != SCHEMA:
        fail("schema %r" % manifest["schema"])
    pack = manifest["pack"]
    for meta, data in zip(pack["parts"], parts):
        if len(data) != meta["bytes"] or sha256_bytes(data) != meta["sha256"]:
            fail("%s does not match the manifest" % meta["file"])
        if len(data) > 1_500_000:
            fail("%s is larger than 1.5 MB" % meta["file"])
    whole = b"".join(parts)
    if len(whole) != pack["bytes"] or sha256_bytes(whole) != pack["sha256"]:
        fail("pack bytes/sha256 differ")
    ids = [e["id"] for e in manifest["files"]]
    if ids != sorted(ids) or len(set(ids)) != len(ids):
        fail("manifest is not sorted by unique id")
    spans = set()
    for e in manifest["files"]:
        data = file_bytes(e, parts)
        if len(data) != e["length"] or sha256_bytes(data) != e["sha256"]:
            fail("%s: bytes differ from the manifest" % e["id"])
        if e["length"] > MAX_FILE_BYTES:
            fail("%s: larger than %d bytes" % (e["id"], MAX_FILE_BYTES))
        if e["expect"] not in EXPECTS:
            fail("%s: expect %r" % (e["id"], e["expect"]))
        spans.add((e["part"], e["offset"], e["length"]))
        for shape in ("planar512", "inter512"):
            if e["open"]["ok"] and shape not in e:
                fail("%s: no %s recording" % (e["id"], shape))
        for v in e.get("rate_variants", []):
            if v["expect"] not in EXPECTS:
                fail("%s: variant expect %r" % (e["id"], v["expect"]))
    end = collections.defaultdict(int)
    for part, offset, length in sorted(spans):
        if offset != end[part]:
            fail("part %d is not contiguous at %d" % (part, offset))
        end[part] = offset + length
    for i, data in enumerate(parts):
        if end[i] != len(data):
            fail("part %d has %d unreferenced bytes" % (i, len(data) - end[i]))
    target = os.path.join(dirpath, "readsizes.json")
    if os.path.exists(target):
        doc = load_json(target)
        if doc["schema"] != SIZES_SCHEMA or doc["pack_sha256"] != pack["sha256"] or doc["block_sizes"] != list(READ_SIZES):
            fail("readsizes.json was recorded against another pack")
        by_id = {e["id"]: e for e in manifest["files"]}
        for f in doc["files"]:
            e = by_id.get(f["id"])
            if e is None or e["sha256"] != f["sha256"] or not e["open"]["ok"]:
                fail("readsizes.json: %s" % f["id"])
            for shape in ("planar", "inter"):
                if sorted(f[shape]) != sorted(str(s) for s in READ_SIZES):
                    fail("readsizes.json: %s sizes" % f["id"])
    print("OK: %d files, %d unique inputs, %d bytes in %d parts" % (len(ids), len(spans), len(whole), len(parts)))
    summarize(manifest)


# --------------------------------------------------------------------------------------------
# crosscheck against the recordings that came with the corpora
# --------------------------------------------------------------------------------------------
def load_oracle(path):
    rows = {}
    with open(path) as f:
        header = f.readline().rstrip("\n").split("\t")
        for line in f:
            d = dict(zip(header, line.rstrip("\n").split("\t")))
            rows[(d["id"], d["variant"])] = d
    return rows


def compare_oracle(oracle, recs):
    """-> Counter of match/mismatch classes and the mismatching ids (interleaved and planar class-G rows)."""
    res, bad = collections.Counter(), []
    for fid, r in recs.items():
        ilv, pl = oracle.get((fid, "ilv-G")), oracle.get((fid, "pl-G"))
        if ilv is None:
            res["no-oracle"] += 1
            continue
        o = r["open"]
        if not o["ok"]:
            ok = ilv["status"] in ("open_failed", "set_client_format_failed") and int(ilv["code"]) == o["error"].get("status")
            res["refused-match" if ok else "refused-MISMATCH"] += 1
            if not ok:
                bad.append(fid)
            continue
        i, p = r["inter"], r["planar"]
        ok_i = (int(ilv["frames"]) == i["frames"] and (ilv["status"] == "ok") == (i["error"] is None) and ilv["sha256"] == i["sha256"]
                and (i["error"] is None or int(ilv["code"]) == i["error"]["status"]))
        if pl["status"] == "set_client_format_failed":
            ok_p = p["error"] is not None and p["frames"] == 0 and int(pl["code"]) == p["error"]["status"]
        else:
            ok_p = (int(pl["frames"]) == p["frames"] and (pl["status"] == "ok") == (p["error"] is None) and pl["plane_sha256"] == ",".join(p["planes"])
                    and (p["error"] is None or int(pl["code"]) == p["error"]["status"]))
        res["interleaved-match" if ok_i else "interleaved-MISMATCH"] += 1
        res["planar-match" if ok_p else "planar-MISMATCH"] += 1
        if not (ok_i and ok_p):
            bad.append(fid)
    return res, bad


def crosscheck(args):
    corpus, probe = os.path.abspath(args.corpus), os.path.abspath(args.probe)
    work = os.path.abspath(args.work)
    os.makedirs(work, exist_ok=True)
    oracle_dir = os.path.join(corpus, "oracle")
    items1 = load_json(os.path.join(corpus, "corpus-a", "manifest.json"))["items"]
    items2 = load_json(os.path.join(corpus, "corpus-b", "manifest.json"))
    cands = []
    for it in items1:
        path = os.path.join(corpus, it["path"])
        if os.path.exists(path):
            cands.append(Cand("corpus-a/" + it["id"], it["id"] + os.path.splitext(path)[1], open(path, "rb").read(), "x", {}))
    for it in items2:
        path = os.path.join(corpus, "corpus-b", it["file"]) if "file" in it else os.path.join(corpus, it["path"])
        if os.path.exists(path):
            cands.append(Cand("corpus-b/" + it["id"], it["id"] + os.path.splitext(path)[1], open(path, "rb").read(), "x", {}))
    probes_dir = os.path.join(corpus, "probes")
    with open(os.path.join(probes_dir, "probes.tsv")) as f:
        header = f.readline().rstrip("\n").split("\t")
        for line in f:
            row = dict(zip(header, line.rstrip("\n").split("\t")))
            path = os.path.join(probes_dir, "probes", row["id"])
            cands.append(Cand("probe/" + row["id"], row["id"] + ("." + row["hint"] if row["hint"] else ""), open(path, "rb").read(), "x", {}))
    paths = stage(cands, os.path.join(work, "cross"))
    recs = record_files(probe, paths)
    o1 = load_oracle(os.path.join(oracle_dir, "oracle-corpus-a.tsv"))
    op = load_oracle(os.path.join(oracle_dir, "oracle-probes.tsv"))
    r1 = {k.split("/", 1)[1]: v for k, v in recs.items() if k.startswith("corpus-a/")}
    rp = {k.split("/", 1)[1]: v for k, v in recs.items() if k.startswith("probe/")}
    for label, oracle, subset in (("corpus-a vs oracle-corpus-a.tsv", o1, r1), ("container probes vs oracle-probes.tsv", op, rp)):
        res, bad = compare_oracle(oracle, subset)
        print(label, dict(res), "mismatches:", bad[:10])
    # corpus-b: the manifest carries the class-G interleaved hash
    match = mismatch = refused = 0
    for it in items2:
        rec = recs.get("corpus-b/" + it["id"])
        if rec is None:
            continue
        if "sha256_G" not in it:
            refused += 1 if not rec["open"]["ok"] else 0
            continue
        i = rec["inter"] if rec["open"]["ok"] else None
        if i and i["error"] is None and i["sha256"] == it["sha256_G"] and i["frames"] == it["frames"]:
            match += 1
        else:
            mismatch += 1
    print("corpus-b class-G hashes: %d match, %d mismatch, %d identical refusals" % (match, mismatch, refused))
    # the manifest's ext_info (what AudioFile/ExtAudioFile reported for corpus-a) against the engine reader's facts
    diffs = collections.Counter()
    for it in items1:
        rec = recs.get("corpus-a/" + it["id"])
        if rec is None:
            continue
        e, o = it["ext_info"], rec["open"]
        if e["ext_open_status"] != 0:
            diffs["recording refuses" if not o["ok"] else "open differs"] += 1
            continue
        if not o["ok"]:
            diffs["engine refuses at %s (recording opened)" % o["error"].get("operation", "?")[-22:]] += 1
            continue
        ff, s = e["file_format"], o["source"]
        same = ((ff["rate"], ff["flags"], ff["bytes_per_packet"], ff["frames_per_packet"], ff["bytes_per_frame"], ff["channels"], ff["bits"], FOURCC(ff["fmt"]) if len(ff["fmt"]) == 4 else None)
                == (s["sample_rate"], s["format_flags"], s["bytes_per_packet"], s["frames_per_packet"], s["bytes_per_frame"], s["channels"], s["bits"], s["format_id"]))
        diffs["source format equal" if same else "source format DIFFERS"] += 1
        fl, br = e["file_length_frames"], e["bitrate"]
        diffs["length equal" if (fl["status"] == 0 and o["length"].get("value") == fl["value"]) or (fl["status"] != 0 and "error" in o["length"]) else "length DIFFERS"] += 1
        diffs["bitrate equal" if (br["status"] == 0 and o["bitrate"].get("value") == br["value"]) or (br["status"] != 0 and "error" in o["bitrate"]) else "bitrate DIFFERS"] += 1
    print("corpus-a ext_info vs engine reader:", dict(diffs))


def syntax_cmd(args):
    print(json.dumps(syntax_of(os.path.abspath(args.corpus), args.path)))


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--corpus", required=True)
    b.add_argument("--probe", required=True)
    b.add_argument("--work", required=True)
    b.add_argument("--out", required=True)
    r = sub.add_parser("readsizes")
    r.add_argument("--probe", required=True)
    r.add_argument("--work", required=True)
    r.add_argument("--dir", required=True)
    v = sub.add_parser("verify-pack")
    v.add_argument("--dir", required=True)
    y = sub.add_parser("syntax")
    y.add_argument("--corpus", required=True)
    y.add_argument("--path", required=True)
    x = sub.add_parser("crosscheck")
    x.add_argument("--corpus", required=True)
    x.add_argument("--probe", required=True)
    x.add_argument("--work", required=True)
    args = ap.parse_args(argv)
    try:
        {"build": build, "readsizes": readsizes, "verify-pack": verify_pack, "crosscheck": crosscheck, "syntax": syntax_cmd}[args.cmd](args)
    except FixtureError as e:
        print("error: %s" % e, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
