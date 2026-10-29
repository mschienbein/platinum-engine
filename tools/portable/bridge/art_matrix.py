#!/usr/bin/env python3
"""Artwork byte-copy matrix for the portable TagLib bridge (ABI-5 `_ex` extension).

  gen     host: author fixtures with independent writers and write cases.json,
          including the expected result of every case (the oracle).
  run     container/host: execute every case against a bridge, create-only.
  check   container/host: verify results with stdlib parsers that do not use
          TagLib, and with the bridge's own TagLib inspection as readback.
  hostcheck  host: cross-check outputs with ffprobe/ffmpeg and metaflac.

Only generated audio and generated images are used.
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import io
import json
import os
import re
import random
import shutil
import struct
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import art_containers as ac  # noqa: E402

MARKER = b'{"x":1}\n\x00\x7f\xff'
FMT = {"mp3": 1, "m4a": 2, "wav": 3, "aiff": 4, "flac": 5}
EXT = {"mp3": "mp3", "m4a": "m4a", "wav": "wav", "aiff": "aiff", "flac": "flac"}
ID3_FORMATS = ("mp3", "aiff")
MP4_FLAGS = {"jpeg": 13, "png": 14, "gif": 12, "bmp": 27}
MIME = {"jpeg": "image/jpeg", "png": "image/png", "gif": "image/gif", "bmp": "image/bmp", "webp": "image/webp"}
FLAC_BLOCK_LIMIT = 0xFFFFFF


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# ---------------------------------------------------------------------------
# Independent image facts (written separately from the bridge's validator)

def image_kind(data: bytes) -> str | None:
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return "png"
    if data[:2] == b"\xff\xd8":
        return "jpeg"
    if data[:4] == b"GIF8":
        return "gif"
    if data[:2] == b"BM":
        return "bmp"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "webp"
    return None


def png_facts(data: bytes) -> dict[str, int]:
    width, height, bit_depth, color_type = struct.unpack(">IIBB", data[16:26])
    channels = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[color_type]
    colors = 0
    position = 8
    while position + 8 <= len(data):
        length = struct.unpack(">I", data[position:position + 4])[0]
        if data[position + 4:position + 8] == b"PLTE":
            colors = length // 3
        position += 12 + length
    return {"width": width, "height": height, "depth": bit_depth * channels, "colors": colors if color_type == 3 else 0}


def jpeg_facts(data: bytes) -> dict[str, int]:
    position = 2
    while position < len(data):
        while data[position] == 0xFF:
            position += 1
        marker = data[position]
        position += 1
        length = struct.unpack(">H", data[position:position + 2])[0]
        if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
            precision, height, width, components = struct.unpack(">BHHB", data[position + 2:position + 8])
            return {"width": width, "height": height, "depth": precision * components, "colors": 0}
        position += length
    raise ValueError("no SOF")


def bmp_facts(data: bytes) -> dict[str, int]:
    dib = struct.unpack("<I", data[14:18])[0]
    if dib == 12:
        width, height, _, bpp = struct.unpack("<HHHH", data[18:26])
        used = 0
    else:
        width, height, _, bpp, _, _, _, _, used = struct.unpack("<iiHHIIiiI", data[18:50])
    return {"width": width, "height": abs(height), "depth": bpp, "colors": (used or 1 << bpp) if bpp <= 8 else 0}


def facts_for(data: bytes) -> dict[str, int | None]:
    kind = image_kind(data)
    from PIL import Image
    image = Image.open(io.BytesIO(data))
    result: dict[str, int | None] = {"width": image.width, "height": image.height, "depth": None, "colors": None}
    try:
        detail = {"png": png_facts, "jpeg": jpeg_facts, "bmp": bmp_facts}.get(kind or "")
        if detail:
            result.update(detail(data))
    except Exception:  # noqa: BLE001 - facts are best-effort oracle data
        pass
    return result


# ---------------------------------------------------------------------------
# Fixture generation

def noise_image(width: int, height: int, seed: int):
    from PIL import Image
    return Image.frombytes("RGB", (width, height), random.Random(seed).randbytes(width * height * 3))


def pattern_image(mode: str, width: int = 64, height: int = 48):
    from PIL import Image
    channels = len(Image.new(mode if mode != "I;16" else "L", (1, 1)).getbands())
    if mode == "I;16":
        image = Image.new("I;16", (width, height))
        image.putdata([(x * 977 + y * 131) & 0xFFFF for y in range(height) for x in range(width)])
        return image
    if mode == "1":
        image = Image.new("1", (width, height))
        image.putdata([((x // 4) + (y // 4)) & 1 and 255 for y in range(height) for x in range(width)])
        return image
    raw = bytes(((x * 7 + y * 13 + c * 61) & 255) for y in range(height) for x in range(width) for c in range(channels))
    return Image.frombytes(mode, (width, height), raw)


def encode(image, fmt: str, **options: Any) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, fmt, **options)
    return buffer.getvalue()


def valid_images() -> dict[str, bytes]:
    from PIL import Image
    out: dict[str, bytes] = {}
    rgb, rgba, gray = pattern_image("RGB"), pattern_image("RGBA"), pattern_image("L")
    out["jpeg-baseline"] = encode(rgb, "JPEG", quality=90)
    out["jpeg-progressive"] = encode(rgb, "JPEG", quality=85, progressive=True)
    out["jpeg-gray"] = encode(gray, "JPEG", quality=90)
    out["jpeg-cmyk"] = encode(pattern_image("CMYK"), "JPEG", quality=90)
    out["jpeg-optimized-444"] = encode(rgb, "JPEG", quality=95, optimize=True, subsampling=0)
    out["jpeg-420-icc-exif"] = encode(rgb, "JPEG", quality=80, subsampling=2, icc_profile=bytes(range(200)),
                                      exif=b"Exif\0\0MM\0*\0\0\0\x08\0\0\0\0\0\0")
    out["jpeg-restart"] = encode(rgb, "JPEG", quality=90, restart_marker_blocks=2)
    out["jpeg-1x1"] = encode(Image.new("RGB", (1, 1), (9, 9, 9)), "JPEG")
    out["jpeg-trailing-garbage"] = out["jpeg-baseline"] + b"\0\0trailing bytes after EOI"
    out["png-rgb"] = encode(rgb, "PNG")
    out["png-rgba"] = encode(rgba, "PNG")
    out["png-gray"] = encode(gray, "PNG")
    out["png-gray-alpha"] = encode(pattern_image("LA"), "PNG")
    out["png-1bit"] = encode(pattern_image("1"), "PNG")
    out["png-16bit"] = encode(pattern_image("I;16"), "PNG")
    palette = rgb.quantize(16)
    out["png-palette16"] = encode(palette, "PNG")
    out["png-palette-trns"] = encode(palette, "PNG", transparency=0)
    out["png-optimized-text-icc"] = encode(rgb, "PNG", optimize=True, icc_profile=bytes(range(120)))
    out["png-apng"] = encode(rgb, "PNG", save_all=True, append_images=[rgba.convert("RGB"), gray.convert("RGB")], duration=50)
    out["png-1x1"] = encode(Image.new("RGB", (1, 1), (1, 2, 3)), "PNG")
    out["png-trailing-garbage"] = out["png-rgb"] + b"trailing bytes after IEND"
    out["gif-single"] = encode(palette, "GIF")
    out["gif-animated"] = encode(palette, "GIF", save_all=True, append_images=[rgb.quantize(32), gray.convert("P")], duration=80, loop=0)
    out["gif-transparent"] = encode(palette, "GIF", transparency=1)
    out["gif-1x1"] = encode(Image.new("P", (1, 1), 0), "GIF")
    out["gif-trailing-garbage"] = out["gif-single"] + b"trailing bytes after trailer"
    out["bmp-24"] = encode(rgb, "BMP")
    out["bmp-32"] = encode(rgba, "BMP")
    out["bmp-8"] = encode(gray, "BMP")
    out["bmp-palette"] = encode(palette, "BMP")
    out["bmp-1bit"] = encode(pattern_image("1"), "BMP")
    out["bmp-1x1"] = encode(Image.new("RGB", (1, 1), (4, 5, 6)), "BMP")
    out["webp-lossy"] = encode(rgb, "WEBP", quality=80)
    out["webp-lossless"] = encode(rgb, "WEBP", lossless=True)
    out["webp-lossy-alpha"] = encode(rgba, "WEBP", quality=80)
    out["webp-lossless-alpha"] = encode(rgba, "WEBP", lossless=True)
    out["webp-animated"] = encode(rgb, "WEBP", save_all=True, append_images=[rgba.convert("RGB"), gray.convert("RGB")], duration=60, lossless=True)
    out["webp-icc-exif"] = encode(rgb, "WEBP", quality=80, icc_profile=bytes(range(100)), exif=b"Exif\0\0MM\0*\0\0\0\x08\0\0\0\0\0\0")
    out["webp-1x1"] = encode(Image.new("RGB", (1, 1), (7, 7, 7)), "WEBP", lossless=True)
    return out


def magick_bmp(version: str, image: bytes, path: Path) -> bytes:
    source = path.with_suffix(".png")
    source.write_bytes(image)
    subprocess.run(["magick", str(source), "-define", f"bmp:format={version}", f"bmp:{path}"], check=True, capture_output=True)
    data = path.read_bytes()
    source.unlink()
    path.unlink()
    return data


def target_size(make, target: int, low: float, high: float, start: int) -> tuple[bytes, int]:
    side = start
    for _ in range(6):
        data = make(side)
        if low <= len(data) <= high:
            return data, side
        side = max(16, int(side * (target / len(data)) ** 0.5))
    raise RuntimeError(f"could not reach {target} bytes")


def large_images() -> dict[str, bytes]:
    out: dict[str, bytes] = {}
    target, low, high = 10_000_000, 9_400_000, 10_600_000
    out["jpeg"], _ = target_size(lambda s: encode(noise_image(s, s, 11), "JPEG", quality=95), target, low, high, 1500)
    out["png"], _ = target_size(lambda s: encode(noise_image(s, s, 12), "PNG", compress_level=1), target, low, high, 1800)
    out["gif"], _ = target_size(lambda s: encode(palette_noise(s, 13), "GIF"), target, low, high, 3000)
    out["bmp"], _ = target_size(lambda s: encode(noise_image(s, s, 14), "BMP"), target, low, high, 1826)
    out["webp"], _ = target_size(lambda s: encode(noise_image(s, s, 15), "WEBP", lossless=True, method=0), target, low, high, 1800)
    return out


def palette_noise(side: int, seed: int):
    from PIL import Image
    image = Image.frombytes("P", (side, side), random.Random(seed).randbytes(side * side))
    image.putpalette(bytes(value for entry in range(256) for value in (entry, (entry * 7) & 255, (entry * 13) & 255)))
    return image


def huge_bmp() -> bytes:
    side = 2400  # 2400*2400*3 = 17.3 MB, over the 16 MiB-1 FLAC block limit
    return encode(noise_image(side, side, 16), "BMP")


def bad_images(valid: dict[str, bytes]) -> dict[str, bytes]:
    def cut(data: bytes) -> bytes:
        return data[:len(data) - max(32, len(data) // 10)]
    bad = {
        "truncated-jpeg": cut(valid["jpeg-baseline"]),
        "truncated-jpeg-progressive": cut(valid["jpeg-progressive"]),
        "truncated-png": cut(valid["png-rgb"]),
        "truncated-png-apng": cut(valid["png-apng"]),
        "truncated-gif": cut(valid["gif-animated"]),
        "truncated-bmp": cut(valid["bmp-24"]),
        "truncated-webp": cut(valid["webp-lossless"]),
        "truncated-webp-lossy": cut(valid["webp-lossy"]),
        "jpeg-header-only": valid["jpeg-baseline"][:20],
        "png-signature-only": b"\x89PNG\r\n\x1a\n",
        "garbage": random.Random(99).randbytes(777),
        "text-not-image": b"not an image\0\xff",
        "tiff-unsupported": encode(pattern_image("RGB"), "TIFF"),
    }
    zero = bytearray(valid["png-rgb"])
    zero[16:24] = b"\0\0\0\0\0\0\0\0"
    bad["png-zero-dimensions"] = bytes(zero)
    no_idat = valid["png-rgb"]
    first = 8 + 8 + 13 + 4
    bad["png-no-idat"] = no_idat[:first] + no_idat[-12:]
    bad["gif-zero-dimensions"] = valid["gif-single"][:6] + b"\0\0\0\0" + valid["gif-single"][10:]
    return bad


def write_new(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as out:
        out.write(data)


def ffmpeg_audio(path: Path, fmt: str, metadata: dict[str, str] | None = None) -> bytes:
    codec = {"mp3": ["-c:a", "libmp3lame", "-write_id3v2", "0", "-write_xing", "0", "-f", "mp3"],
             "m4a": ["-c:a", "alac", "-f", "ipod"], "wav": ["-c:a", "pcm_s16le", "-f", "wav"],
             "aiff": ["-c:a", "pcm_s16be", "-f", "aiff"], "flac": ["-c:a", "flac", "-f", "flac"]}[fmt]
    command = ["ffmpeg", "-v", "error", "-nostdin", "-f", "lavfi", "-i", "sine=frequency=431:sample_rate=44100:duration=0.5",
               "-ac", "2", "-map_metadata", "-1", "-fflags", "+bitexact", "-flags:a", "+bitexact"]
    for key, value in (metadata or {}).items():
        command += ["-metadata", f"{key}={value}"]
    command += codec + [str(path)]
    subprocess.run(command, check=True, capture_output=True)
    return path.read_bytes()


TAGS = {"title": "Source Title é", "artist": "Source Artist", "album": "Source Album", "comment": "source comment", "date": "2024"}
VORBIS_EXTRA = {"BPM": "128", "INITIALKEY": "Am"}


def id3_tag_frames(version: int = 4) -> list[bytes]:
    return [ac.text_frame("TIT2", TAGS["title"], version), ac.text_frame("TPE1", TAGS["artist"], version),
            ac.text_frame("TALB", TAGS["album"], version), ac.comm_frame(TAGS["comment"], version),
            ac.text_frame("TDRC" if version == 4 else "TYER", TAGS["date"], version),
            ac.text_frame("TBPM", "128", version), ac.text_frame("TKEY", "Am", version)]


def id3_picture_frames(pictures: list[dict[str, Any]], images: dict[str, bytes]) -> list[bytes]:
    return [ac.apic_frame(p["mime"], p["type"], p["description"], images[p["image"]] if not p.get("empty") else b"",
                          p.get("encoding", "latin1")) for p in pictures]


def build_source(fmt: str, audio: dict[str, bytes], pictures: list[dict[str, Any]], images: dict[str, bytes]) -> bytes:
    if fmt == "mp3":
        return ac.mp3_with_id3(audio["mp3"], ac.id3v2_tag(id3_tag_frames() + id3_picture_frames(pictures, images)))
    if fmt == "aiff":
        return ac.aiff_with_id3(audio["aiff"], ac.id3v2_tag(id3_tag_frames() + id3_picture_frames(pictures, images)))
    if fmt == "wav":
        return ac.wav_with_id3(audio["wav"], ac.id3v2_tag(id3_tag_frames() + id3_picture_frames(pictures, images)))
    if fmt == "flac":
        bodies = []
        for p in pictures:
            data = images[p["image"]] if not p.get("empty") else b""
            geometry = p.get("geometry") or (0, 0, 0, 0)
            bodies.append(ac.flac_picture_body(p["type"], p["mime"], p["description"], *geometry, data))
        return ac.flac_with_pictures(audio["flac-tagged"], bodies)
    if fmt == "m4a":
        covers = [(p.get("mp4_flag", MP4_FLAGS.get(p["kind"], 0)), images[p["image"]] if not p.get("empty") else b"")
                  for p in pictures]
        return ac.mp4_with_covers(audio["m4a-tagged"], covers) if covers else audio["m4a-tagged"]
    raise ValueError(fmt)


def pic(image: str, kind: str, picture_type: int = 3, description: str = "", mime: str | None = None,
        encoding: str = "latin1", geometry: tuple[int, int, int, int] | None = None, empty: bool = False,
        mp4_flag: int | None = None) -> dict[str, Any]:
    spec: dict[str, Any] = {"image": image, "kind": kind, "type": picture_type, "description": description,
                            "mime": MIME[kind] if mime is None else mime, "encoding": encoding, "empty": empty}
    if geometry:
        spec["geometry"] = geometry
    if mp4_flag is not None:
        spec["mp4_flag"] = mp4_flag
    return spec


def kind_of(name: str) -> str:
    return name.split("-", 1)[0]


# ---------------------------------------------------------------------------
# Oracle

def expected_direct(pictures: list[dict[str, Any]], src_fmt: str, dst_fmt: str) -> dict[str, Any]:
    """Direct copies the flag must not change: the reference already moves
    ID3 frames / MP4 items unchanged (and relabels covr for MP4 -> ID3)."""
    if src_fmt in ("mp3", "aiff", "wav") and dst_fmt == src_fmt:
        return {"status": 0, "result_flags": 0, "passthrough": True, "pictures": [
            {"type": p["type"], "mime": p["mime"], "description": p["description"],
             "mislabeled": p["mime"] not in MIME.values(),
             "sha256": p["sha256"], "size": p["size"]} for p in pictures]}
    if src_fmt == "m4a" and dst_fmt == "m4a":
        return {"status": 0, "result_flags": 0, "passthrough": True, "pictures": [
            {"flag": p["mp4_flag"], "sha256": p["sha256"], "size": p["size"]} for p in pictures]}
    if src_fmt == "m4a" and dst_fmt == "mp3":
        return {"status": 0, "result_flags": 0, "passthrough": True, "pictures": [
            {"type": 0, "mime": "image/jpeg" if p["mp4_flag"] == 13 else "image/png", "description": "",
             "mislabeled": p["mp4_flag"] not in (13, 14),
             "sha256": p["sha256"], "size": p["size"]} for p in pictures]}
    raise ValueError((src_fmt, dst_fmt))


def expected_for(pictures: list[dict[str, Any]], src_fmt: str, dst_fmt: str, direct_flac: bool,
                 info: dict[str, dict[str, Any]], rewrite: str = "keep") -> dict[str, Any]:
    """What the bridge must do for these source pictures, derived from the spec. A WAV destination is an ID3 container
    like MP3 and AIFF; the picture reaches the finished file only when the caller's rewrite callback keeps the ID3
    chunk TagLib saved last (`rewrite == "keep"`, the engine's opt-in); the reference-style rewrite drops it."""
    live = [p for p in pictures if not p.get("empty")]
    if not live:
        return {"status": 0, "result_flags": 0, "pictures": None}
    for index, p in enumerate(pictures, 1):
        if not p.get("empty") and not info[p["image"]]["valid"]:
            return {"status": 2, "refusal": f"picture {index} is not a complete"}
    out = []
    for p in live:
        facts = info[p["image"]]
        kind = facts["kind"]
        if dst_fmt == "m4a":
            if kind == "webp":
                return {"status": 2, "refusal": "WebP, which an MP4 covr atom cannot store"}
            out.append({"flag": MP4_FLAGS[kind], "sha256": facts["sha256"], "size": facts["size"]})
            continue
        picture_type = 3 if src_fmt == "m4a" else p["type"]
        description = "" if src_fmt == "m4a" else p["description"]
        mime = MIME[kind]
        entry = {"type": picture_type, "mime": mime, "description": description, "sha256": facts["sha256"], "size": facts["size"]}
        if dst_fmt == "flac":
            if src_fmt == "flac":
                width, height, depth, colors = p.get("geometry") or (0, 0, 0, 0)
            else:
                width, height, depth, colors = facts["width"], facts["height"], facts.get("depth"), facts.get("colors")
            entry.update({"width": width, "height": height, "depth": depth, "colors": colors})
            block = 32 + len(mime.encode()) + len(description.encode()) + facts["size"]
            if block > FLAC_BLOCK_LIMIT:
                return {"status": 2, "refusal": "FLAC metadata block limit"}
        out.append(entry)
    if dst_fmt == "wav" and rewrite == "reference":
        # Whether the chunk TagLib saved last survives the reference-style rewrite depends on when the C library
        # flushes TagLib's buffered RIFF size update (it is dropped with glibc, as the documented default policy says);
        # either the picture is gone or it is all there.
        return {"status": 0, "result_flags": 1, "pictures": None, "wav_rewrite_drops_chunk": True, "pictures_if_kept": out}
    return {"status": 0, "result_flags": 1, "pictures": out}


def generate(root: Path) -> None:
    root.mkdir()
    fixtures = root / "fixtures"
    (fixtures / "images").mkdir(parents=True)
    (fixtures / "sources").mkdir()
    (fixtures / "bases").mkdir()
    valid = valid_images()
    for name, path in (("bmp4", "bmp-v4"), ("bmp5", "bmp-v5")):
        valid[path] = magick_bmp(name, valid["png-rgba"], root / "tmp-magick.bmp")
    large = {f"large-{kind}": data for kind, data in large_images().items()}
    huge = {"huge-bmp": huge_bmp()}
    bad = bad_images(valid)
    images: dict[str, bytes] = {**valid, **large, **huge, **bad}
    info: dict[str, dict[str, Any]] = {}
    for name, data in images.items():
        is_bad = name in bad
        kind = image_kind(data) or ("tiff" if name == "tiff-unsupported" else "unknown")
        record: dict[str, Any] = {"kind": kind if kind != "unknown" else kind_of(name), "valid": not is_bad,
                                  "size": len(data), "sha256": sha(data), "file": f"images/{name}"}
        if not is_bad:
            record.update(facts_for(data))
        info[name] = record
        write_new(fixtures / "images" / name, data)
    # Audio bases and tagged source bases.
    scratch = root / "tmp-audio"
    scratch.mkdir()
    audio: dict[str, bytes] = {}
    for fmt in FMT:
        audio[fmt] = ffmpeg_audio(scratch / f"base.{EXT[fmt]}", fmt)
    tagged_meta = dict(TAGS)
    audio["m4a-tagged"] = ffmpeg_audio(scratch / "tagged.m4a", "m4a", tagged_meta)
    audio["flac-tagged"] = ffmpeg_audio(scratch / "tagged.flac", "flac", {**tagged_meta, **VORBIS_EXTRA})
    shutil.rmtree(scratch)
    for fmt in FMT:
        write_new(fixtures / "bases" / f"base.{EXT[fmt]}", audio[fmt])
    # Destination bases that already carry a PNG, to prove replacement semantics.
    existing = [pic("png-rgb", "png", 3, "old cover")]
    for fmt in ("mp3", "m4a", "aiff", "flac", "wav"):
        base = dict(audio)
        base["m4a-tagged"], base["flac-tagged"] = audio["m4a"], audio["flac"]
        data = build_source(fmt, base, existing, images)
        write_new(fixtures / "bases" / f"base-art.{EXT[fmt]}", data)
    # Source files.
    sources: dict[str, dict[str, Any]] = {}

    def add_source(set_name: str, fmt: str, pictures: list[dict[str, Any]]) -> str:
        rel = f"sources/{fmt}-{set_name}.{EXT[fmt]}"
        data = build_source(fmt, audio, pictures, images)
        write_new(fixtures / rel, data)
        sources[rel] = {"fmt": fmt, "set": set_name, "pictures": pictures, "sha256": sha(data), "size": len(data)}
        return rel

    sets: dict[str, list[dict[str, Any]]] = {"noart": []}
    for kind in ("jpeg", "png", "gif", "bmp", "webp"):
        sets[f"one-{kind}"] = [pic(f"{kind}-baseline" if kind == "jpeg" else {"png": "png-rgb", "gif": "gif-single", "bmp": "bmp-24", "webp": "webp-lossy"}[kind], kind, 3, "cover")]
    sets["types"] = [pic("jpeg-baseline" if t % 2 else "png-rgb", "jpeg" if t % 2 else "png", t, f"type {t}") for t in range(21)]
    sets["unicode"] = [
        pic("jpeg-baseline", "jpeg", 3, "Café ☕ 表紙 🎨", encoding="utf8"),
        pic("png-rgb", "png", 4, "日本語のカバー", encoding="utf16"),
        pic("gif-single", "gif", 5, "Ünïcödé àéîõü", encoding="latin1"),
        pic("bmp-24", "bmp", 6, "", encoding="utf16be"),
        pic("jpeg-progressive", "jpeg", 0, "emoji 🎵🎶 and CJK 漢字", encoding="utf8"),
    ]
    sets["multi"] = [pic("jpeg-baseline", "jpeg", 3, "front"), pic("png-rgb", "png", 4, "back"),
                     pic("gif-single", "gif", 6, "media"), pic("bmp-24", "bmp", 20, "publisher logo"),
                     pic("webp-lossy", "webp", 8, "artist")]
    sets["multi-mp4safe"] = [pic("jpeg-baseline", "jpeg", 3, "front"), pic("png-rgb", "png", 4, "back"),
                             pic("gif-single", "gif", 6, "media"), pic("bmp-24", "bmp", 20, "publisher logo")]
    sets["mixed-empty"] = [pic("jpeg-baseline", "jpeg", 3, "", empty=True), pic("jpeg-baseline", "jpeg", 3, "kept 1"),
                           pic("png-rgb", "png", 3, "", empty=True), pic("png-rgb", "png", 4, "kept 2")]
    sets["all-empty"] = [pic("jpeg-baseline", "jpeg", 3, "", empty=True), pic("png-rgb", "png", 3, "", empty=True)]
    sets["empty-mime"] = [pic("jpeg-baseline", "jpeg", 3, "no declared mime", mime=""), pic("png-rgb", "png", 3, "alias", mime="image/PNG")]
    sets["flac-geometry"] = [pic("jpeg-baseline", "jpeg", 3, "geometry kept", geometry=(640, 480, 24, 0)),
                             pic("gif-single", "gif", 3, "geometry kept 2", geometry=(321, 123, 8, 256))]
    for name in valid:
        sets[f"corpus-{name}"] = [pic(name, kind_of(name) if kind_of(name) != "bmp" else "bmp", 3, "corpus")]
    for name in bad:
        sets[f"bad-{name}"] = [pic(name, image_kind(images[name]) or "jpeg", 3, "bad")]
    sets["bad-mixed"] = [pic("jpeg-baseline", "jpeg", 3, "good first"), pic("truncated-png", "png", 4, "bad second")]
    for kind in ("jpeg", "png", "gif", "bmp", "webp"):
        sets[f"large-{kind}"] = [pic(f"large-{kind}", kind, 3, "large cover")]
    sets["huge-bmp"] = [pic("huge-bmp", "bmp", 3, "huge")]

    case_list: list[dict[str, Any]] = []

    def src_for(fmt: str, set_name: str) -> str:
        key = f"sources/{fmt}-{set_name}.{EXT[fmt]}"
        if key not in sources:
            pictures = sets[set_name]
            if fmt == "m4a":
                pictures = [dict(p, mp4_flag=MP4_FLAGS.get(p["kind"], 0)) for p in pictures]
            add_source(set_name, fmt, pictures)
        return key

    def add_case(case_id: str, mode: str, flags: int | None, impl: str, src_fmt: str, dst_fmt: str, set_name: str,
                 dst_base: str | None = None, group: str | None = None, expect: dict[str, Any] | None = None,
                 reference: bool = True, note: str = "", rewrite: str | None = None) -> None:
        src = src_for(src_fmt, set_name)
        dst = dst_base or f"bases/base.{EXT[dst_fmt]}"
        op = None
        if mode == "direct":
            op = 6 if (src_fmt, dst_fmt) == ("m4a", "mp3") else FMT[src_fmt]
        entry: dict[str, Any] = {"id": case_id, "mode": mode, "impl": impl, "flags": flags, "sf": FMT[src_fmt], "df": FMT[dst_fmt],
                                 "op": op, "src": src, "dst": dst, "set": set_name, "src_fmt": src_fmt, "dst_fmt": dst_fmt,
                                 "group": group, "note": note}
        if rewrite:
            entry["rewrite"] = rewrite
        if expect is not None:
            entry["expect"] = expect
        elif flags:
            pictures = sets[set_name]
            if src_fmt == "m4a":
                pictures = [dict(p, mp4_flag=MP4_FLAGS.get(p["kind"], 0)) for p in pictures]
            direct_flac = mode == "direct" and (src_fmt, dst_fmt) == ("flac", "flac")
            if mode == "direct" and not direct_flac:
                full = [dict(p, sha256=info[p["image"]]["sha256"], size=0 if p.get("empty") else info[p["image"]]["size"])
                        for p in pictures]
                for p in full:
                    if p.get("empty"):
                        p["sha256"] = sha(b"")
                entry["expect"] = expected_direct(full, src_fmt, dst_fmt)
            else:
                entry["expect"] = expected_for(pictures, src_fmt, dst_fmt, direct_flac, info, rewrite or "keep")
        if reference and entry.get("expect", {}).get("status") == 0 and flags:
            entry["reference_src"] = src_for(src_fmt, "noart")
        case_list.append(entry)

    def is_direct(src_fmt: str, dst_fmt: str) -> bool:
        return src_fmt == dst_fmt or (src_fmt, dst_fmt) == ("m4a", "mp3")

    def add_pair(case_id: str, src_fmt: str, dst_fmt: str, set_name: str, **kw: Any) -> None:
        mode = "direct" if is_direct(src_fmt, dst_fmt) else "generic"
        add_case(case_id, mode, 1, "new_ex", src_fmt, dst_fmt, set_name, **kw)

    # C1: kind x pair matrix (flag on).
    for kind in ("jpeg", "png", "gif", "bmp", "webp"):
        for s in ("mp3", "m4a", "aiff", "flac", "wav"):
            if kind == "webp" and s == "m4a":
                continue  # authored separately below as an implicit-type covr
            for d in FMT:
                add_pair(f"kind-{kind}-{s}-to-{d}", s, d, f"one-{kind}")
    sets["m4a-webp"] = [pic("webp-lossy", "webp", 3, "", mp4_flag=0)]
    for d in FMT:
        add_pair(f"kind-webp-m4a-to-{d}", "m4a", d, "m4a-webp")
    # A WAV source's ID3 chunk is read like any other ID3 source (the kind-* cells above and set-* below cover the pairs);
    # the direct WAV copy keeps its APIC as it always did.
    add_case("wav-source-direct-wav", "direct", 1, "new_ex", "wav", "wav", "one-jpeg")
    # C2: every picture type, unicode descriptions, order, empty handling, MIME derivation, FLAC geometry.
    for set_name in ("types", "unicode", "multi", "mixed-empty", "all-empty", "empty-mime", "flac-geometry", "multi-mp4safe"):
        for s in ("mp3", "m4a", "aiff", "flac", "wav"):
            if set_name in ("flac-geometry", "empty-mime") and s == "m4a":
                continue
            if set_name in ("multi", "unicode") and s == "m4a":
                continue  # contain WebP/unrepresentable-description sets; covered via multi-mp4safe
            if set_name == "multi-mp4safe" and s != "m4a":
                pass
            for d in FMT:
                add_pair(f"set-{set_name}-{s}-to-{d}", s, d, set_name)
    # C3: ~10 MB pictures.
    for kind in ("jpeg", "png", "gif", "bmp", "webp"):
        for s, ds in (("flac", ("mp3", "m4a", "aiff", "flac", "wav")), ("mp3", ("flac", "m4a", "aiff", "mp3", "wav")),
                      ("wav", ("flac", "m4a", "aiff", "mp3"))):
            for d in ds:
                add_pair(f"large-{kind}-{s}-to-{d}", s, d, f"large-{kind}")
    for s, ds in (("mp3", ("flac", "m4a", "aiff", "mp3", "wav")), ("m4a", ("flac", "mp3", "aiff", "m4a", "wav")),
                  ("wav", ("flac", "m4a", "aiff", "mp3"))):
        for d in ds:
            add_pair(f"huge-bmp-{s}-to-{d}", s, d, "huge-bmp")
    # C5: malformed pictures.
    for name in list(bad) + ["mixed"]:
        set_name = f"bad-{name}"
        for s, d in (("flac", "mp3"), ("mp3", "flac"), ("flac", "m4a"), ("m4a", "aiff"), ("flac", "flac"), ("mp3", "mp3"),
                     ("aiff", "flac"), ("flac", "aiff"), ("flac", "wav"), ("wav", "flac"), ("mp3", "wav"), ("wav", "mp3"),
                     ("m4a", "wav"), ("wav", "m4a")):
            add_pair(f"bad-{name}-{s}-to-{d}", s, d, set_name,
                     reference=False)
    # C6: valid-image corpus acceptance (false-refusal check).
    for name in valid:
        add_pair(f"corpus-{name}-flac-to-mp3", "flac", "mp3", f"corpus-{name}")
        add_pair(f"corpus-{name}-mp3-to-flac", "mp3", "flac", f"corpus-{name}")
        add_pair(f"corpus-{name}-flac-to-wav", "flac", "wav", f"corpus-{name}")
        add_pair(f"corpus-{name}-wav-to-flac", "wav", "flac", f"corpus-{name}")
        if image_kind(images[name]) != "webp":
            add_pair(f"corpus-{name}-flac-to-m4a", "flac", "m4a", f"corpus-{name}")
    # C8: destination already has artwork.
    for s, d in (("flac", "mp3"), ("mp3", "flac"), ("flac", "m4a"), ("mp3", "aiff"), ("aiff", "flac"), ("m4a", "flac"),
                 ("flac", "wav"), ("mp3", "wav"), ("aiff", "wav"), ("m4a", "wav"), ("wav", "flac"), ("wav", "mp3"),
                 ("wav", "aiff"), ("wav", "m4a")):
        add_pair(f"replace-art-{s}-to-{d}", s, d, "multi-mp4safe", dst_base=f"bases/base-art.{EXT[d]}")
        add_case(f"keep-art-noart-{s}-to-{d}", "generic", 1, "new_ex", s, d, "noart", dst_base=f"bases/base-art.{EXT[d]}",
                 expect={"status": 0, "result_flags": 0, "pictures": "dst_unchanged_art"}, reference=False)
    add_pair("replace-art-flac-to-flac-direct", "flac", "flac", "one-jpeg", dst_base="bases/base-art.flac")
    # A WAV destination whose rewrite drops the chunk TagLib saved last (the reference marker rewrite) carries no picture,
    # exactly as before the bridge stored pictures there; the bridge still reports the copy it made.
    for s in ("flac", "mp3", "m4a"):
        add_pair(f"wav-reference-rewrite-{s}-to-wav", s, "wav", "multi-mp4safe", rewrite="reference")
    add_pair("wav-reference-rewrite-flac-to-wav-one-jpeg", "flac", "wav", "one-jpeg", rewrite="reference")
    # C7: flag-off equivalence: v07, new legacy symbols and _ex(0) agree.
    for set_name in ("one-jpeg", "types", "all-empty", "mixed-empty", "noart", "bad-garbage", "one-webp", "huge-bmp"):
        for s, d in (("flac", "mp3"), ("mp3", "flac"), ("flac", "m4a"), ("m4a", "aiff"), ("aiff", "m4a"), ("mp3", "wav"),
                     ("flac", "flac"), ("mp3", "mp3"), ("m4a", "m4a"), ("m4a", "mp3"), ("aiff", "aiff"), ("wav", "wav")):
            if set_name == "huge-bmp" and s == "flac":
                continue
            mode = "direct" if is_direct(s, d) else "generic"
            group = f"equiv-{set_name}-{s}-to-{d}"
            for impl, flags in (("v07", None), ("new_legacy", None), ("new_ex", 0)):
                add_case(f"{group}-{impl}", mode, flags, impl, s, d, set_name, group=group, expect={"equiv": True}, reference=False)
    # C9: flag-on direct copies must equal flag-off everywhere except FLAC -> FLAC.
    for s in FMT:
        for d in FMT:
            set_name = "one-jpeg"
            if s == "flac" and d == "flac":
                continue
            group = f"direct-{s}-to-{d}"
            for impl, flags in (("v07", None), ("new_ex", 1)):
                add_case(f"{group}-{impl}", "direct", flags, impl, s, d, set_name, group=group, expect={"equiv": True}, reference=False,
                         note="op by source family, destination may be another family")
    # C10: unknown flags.
    add_case("flags-unknown-generic", "generic", 2, "new_ex", "flac", "mp3", "one-jpeg", expect={"status": 1, "refusal": "unsupported tag copy flags"}, reference=False)
    add_case("flags-unknown-direct", "direct", 0x80000001, "new_ex", "flac", "flac", "one-jpeg", expect={"status": 1, "refusal": "unsupported tag copy flags"}, reference=False)

    # Refusals must say why; flag-off art copies are v07 status 2 (covered by the equivalence groups).
    manifest = {"images": info, "sources": sources, "sets": {k: v for k, v in sets.items() if not k.startswith("corpus-")}}
    with (root / "manifest.json").open("x") as out:
        json.dump(manifest, out, indent=1, sort_keys=True)
    with (root / "cases.json").open("x") as out:
        json.dump(case_list, out, indent=1, sort_keys=True)
    print(f"images {len(images)} sources {len(sources)} cases {len(case_list)}")


# ---------------------------------------------------------------------------
# Running

SERIALIZE_T = None


def load_parity():
    import importlib.util
    path = Path(__file__).resolve().parent / "parity.py"
    spec = importlib.util.spec_from_file_location("bridge_parity", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def trailing_riff_size(data: bytes) -> int | None:
    """Port of `wav_preserve::trailing_chunks`: the RIFF size that makes the declared end the end of the file, when the
    bytes after the declared end are a bounded run of well-formed chunks that reach exactly the end of the file."""
    length = len(data)
    if length < 20 or length - 8 > 0xFFFFFFFF or data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        return None
    riff = struct.unpack("<I", data[4:8])[0]
    end = riff + 8
    if riff < 4 or end >= length or end & 1:
        return None
    position = end
    for _ in range(64):
        if position == length:
            return length - 8
        if length - position < 8:
            return None
        chunk_id = data[position:position + 4]
        if not all(48 <= c <= 57 or 65 <= c <= 90 or 97 <= c <= 122 or c == 32 for c in chunk_id):
            return None
        size = struct.unpack("<I", data[position + 4:position + 8])[0]
        body = position + 8
        available = length - body
        if size > available:
            return None
        position = body + min(size + (size & 1), available)
    return length - 8 if position == length else None


def rewrite_wav_keeping(parity, path: Path, marker: bytes | None) -> None:
    """The engine's rewrite_wav callback under `parity.allow_non_identical` (`wav_preserve::rewrite_wav_marker`): the chunks
    TagLib appended after the declared RIFF end survive, then the reference marker rewrite runs. Like the reference fixture
    it writes a new file over `path` (TagLib's open stream keeps the old inode)."""
    data = path.read_bytes()
    corrected = trailing_riff_size(data)
    if corrected is None:
        parity.rewrite_wav_fixture(path, marker)
        return
    patched = bytearray(data)
    patched[4:8] = struct.pack("<I", corrected)
    scratch = Path(os.fspath(path) + ".keep")
    mode = path.stat().st_mode & 0o7777
    fd = os.open(scratch, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode & 0o777)
    with os.fdopen(fd, "wb") as out:
        out.write(patched)
    parity.rewrite_wav_fixture(scratch, marker)
    os.replace(scratch, path)


class ExtBridge:
    def __init__(self, parity, path: Path) -> None:
        self.parity = parity
        self.base = parity.Bridge(path)
        lib = self.base.lib
        c_uint, c_int, c_size, c_char = ctypes.c_uint, ctypes.c_int, ctypes.c_size_t, ctypes.c_char_p
        self.has_ex = hasattr(lib, "pn_tag_copy_generic_ex")
        if self.has_ex:
            lib.pn_tag_bridge_features.restype = ctypes.c_uint32
            lib.pn_tag_bridge_features.argtypes = []
            lib.pn_tag_copy_direct_ex.restype = c_int
            lib.pn_tag_copy_direct_ex.argtypes = [c_uint, c_int, c_char, c_char, ctypes.POINTER(c_uint), c_char, c_size]
            lib.pn_tag_copy_generic_ex.restype = c_int
            lib.pn_tag_copy_generic_ex.argtypes = [c_uint, c_int, c_int, c_char, c_char, parity.SERIALIZE, parity.REWRITE,
                                                    ctypes.c_void_p, ctypes.POINTER(c_uint), c_char, c_size]

    def features(self) -> int | None:
        return self.base.lib.pn_tag_bridge_features() if self.has_ex else None

    def direct(self, impl: str, flags: int | None, op: int, src: Path, dst: Path) -> dict[str, Any]:
        if impl == "new_ex":
            result = ctypes.c_uint(0xDEADBEEF)
            error = ctypes.create_string_buffer(1024)
            status = self.base.lib.pn_tag_copy_direct_ex(flags, op, os.fsencode(src), os.fsencode(dst), ctypes.byref(result), error, len(error))
            return {"status": status, "error": error.value.decode("utf-8", "replace"), "result_flags": result.value,
                    "callbacks": {"serialize": 0, "rewrite": 0}}
        status, message = self.base.copy_direct(op, src, dst)
        return {"status": status, "error": message, "result_flags": None, "callbacks": {"serialize": 0, "rewrite": 0}}

    def generic(self, impl: str, flags: int | None, sf: int, df: int, src: Path, dst: Path, rewrite_mode: str = "reference",
                marker: bytes | None = MARKER) -> dict[str, Any]:
        parity = self.parity
        if impl != "new_ex" and rewrite_mode == "reference" and marker == MARKER:
            out = self.base.write("generic", sf, df, src, dst, MARKER)
            out["result_flags"] = None
            return out
        buffer = ctypes.create_string_buffer(marker) if marker else None
        counts = {"serialize": 0, "rewrite": 0}
        errors: list[str] = []

        @parity.SERIALIZE
        def serialize(_context: int, payload: Any) -> int:
            counts["serialize"] += 1
            payload.contents.present = int(marker is not None)
            payload.contents.length = len(marker) if marker is not None else 0
            payload.contents.bytes = ctypes.cast(buffer, ctypes.POINTER(ctypes.c_uint8)) if buffer is not None else None
            return 0

        @parity.REWRITE
        def rewrite(_context: int, raw_path: bytes) -> int:
            counts["rewrite"] += 1
            try:
                path = Path(os.fsdecode(raw_path))
                if rewrite_mode == "keep":
                    rewrite_wav_keeping(parity, path, MARKER)
                else:
                    parity.rewrite_wav_fixture(path, MARKER)
                return 0
            except Exception as exc:  # noqa: BLE001
                errors.append(repr(exc))
                return 1

        error = ctypes.create_string_buffer(1024)
        if impl == "new_ex":
            result = ctypes.c_uint(0xDEADBEEF)
            status = self.base.lib.pn_tag_copy_generic_ex(flags, sf, df, os.fsencode(src), os.fsencode(dst), serialize, rewrite,
                                                          None, ctypes.byref(result), error, len(error))
            result_flags: int | None = result.value
        else:
            status = self.base.lib.pn_tag_copy_generic(sf, df, os.fsencode(src), os.fsencode(dst), serialize, rewrite, None,
                                                       error, len(error))
            result_flags = None
        return {"status": status, "error": error.value.decode("utf-8", "replace"), "result_flags": result_flags,
                "callbacks": counts, "callback_errors": errors}


def run_cases(args: argparse.Namespace) -> None:
    parity = load_parity()
    root = args.root
    cases = json.loads((root / "cases.json").read_text())
    new = ExtBridge(parity, args.bridge)
    old = ExtBridge(parity, args.old) if args.old else None
    out_root = args.out
    out_root.mkdir()
    (out_root / "files").mkdir()
    results = []
    started = time.time()
    def execute(bridge: ExtBridge, impl: str, flags: int | None, case: dict[str, Any], src_rel: str, name: str) -> dict[str, Any]:
        base = root / "fixtures" / case["dst"]
        src = root / "fixtures" / src_rel
        dst = out_root / "files" / f"{name}.{EXT[case['dst_fmt']]}"
        parity.copy_new(base, dst)
        before = dst.read_bytes()
        t0 = time.time()
        if case["mode"] == "direct":
            outcome = bridge.direct(impl, flags, case["op"], src, dst)
        else:
            outcome = bridge.generic(impl, flags, case["sf"], case["df"], src, dst,
                                     case.get("rewrite") or ("keep" if case["flags"] else "reference"))
        outcome["elapsed_ms"] = round((time.time() - t0) * 1000)
        after = dst.read_bytes()
        outcome.update({"id": name, "output_sha256": sha(after), "output_size": len(after), "dst_unchanged": after == before})
        if case["mode"] == "generic" and outcome["status"] == 0:
            m = bridge.base.marker(case["df"], dst)
            outcome["marker"] = [m[0], m[1], m[2], m[3].hex() if m[3] is not None else None]
        status, error, text = bridge.base.inspect(case["df"], dst)
        outcome["inspect"] = {"status": status, "error": error, "json": json.loads(text) if text else None}
        return outcome

    for case in cases:
        impl = case["impl"]
        bridge = old if impl == "v07" else new
        if bridge is None:
            raise SystemExit(f"case {case['id']} needs --old")
        results.append(execute(bridge, "new_ex" if impl == "new_ex" else "old", case["flags"], case, case["src"], case["id"]))
        if case.get("reference_src"):
            # Same tags without pictures through the unchanged ABI-5 entry point.
            results.append(execute(new, "old", None, case, case["reference_src"], f"{case['id']}~ref"))
    report = {"bridge": str(args.bridge), "bridge_sha256": sha(args.bridge.read_bytes()), "features": new.features(),
              "old_bridge_sha256": sha(args.old.read_bytes()) if args.old else None, "machine": os.uname().machine,
              "cases": len(results), "seconds": round(time.time() - started, 1), "results": results}
    with (out_root / "results.json").open("x") as out:
        json.dump(report, out, indent=1, sort_keys=True)
    print(f"ran {len(results)} cases in {report['seconds']} s on {report['machine']}")


# ---------------------------------------------------------------------------
# Checking

def parse_output(fmt: str, data: bytes) -> dict[str, Any]:
    if fmt == "mp3":
        return {"kind": "id3", "pictures": ac.parse_id3v2_pictures(data), "audio": ac.mp3_audio(data)}
    if fmt == "aiff":
        return {"kind": "id3", "pictures": ac.parse_riff_id3_pictures(data, True), "audio": ac.riff_audio(data, True)}
    if fmt == "wav":
        return {"kind": "id3", "pictures": ac.parse_riff_id3_pictures(data, False), "audio": ac.riff_audio(data, False)}
    if fmt == "flac":
        return {"kind": "flac", "pictures": ac.parse_flac_pictures(data), "audio": ac.flac_audio(data)}
    return {"kind": "mp4", "pictures": ac.parse_mp4_covers(data), "audio": ac.mp4_mdat_payload(data)}


def inspect_pictures(inspection: dict[str, Any] | None, kind: str) -> list[dict[str, Any]]:
    if not inspection:
        return []
    container = {"id3": "id3v2", "flac": "flac_picture", "mp4": "mp4_covr"}[kind]
    return [a for a in inspection["artwork"] if a["container"] == container]


def pictures_match(expected: list[dict[str, Any]], parsed: list[dict[str, Any]], kind: str, fmt: str, problems: list[str],
                   images: dict[str, dict[str, Any]]) -> None:
    if len(expected) != len(parsed):
        problems.append(f"picture count {len(parsed)} != expected {len(expected)}")
        return
    for index, (want, got) in enumerate(zip(expected, parsed), 1):
        label = f"picture {index}"
        if sha(got["data"]) != want["sha256"] or len(got["data"]) != want["size"]:
            problems.append(f"{label}: bytes differ")
        if kind == "mp4":
            if got["flag"] != want["flag"]:
                problems.append(f"{label}: covr flag {got['flag']} != {want['flag']}")
            continue
        for key in ("type", "mime", "description"):
            if got[key] != want[key]:
                problems.append(f"{label}: {key} {got[key]!r} != {want[key]!r}")
        if kind == "flac":
            for key in ("width", "height", "depth", "colors"):
                if want.get(key) is not None and got[key] != want[key]:
                    problems.append(f"{label}: {key} {got[key]} != {want[key]}")


def strip_art(inspection: dict[str, Any] | None) -> Any:
    if not inspection:
        return None
    copy = {k: v for k, v in inspection.items() if k != "artwork"}
    if copy.get("id3_frames") is not None:
        copy["id3_frames"] = [f for f in copy["id3_frames"] if f["id"] != "APIC"]
    if copy.get("unsupported_data") is not None:
        copy["unsupported_data"] = [f for f in copy["unsupported_data"] if f not in ("APIC", "covr")]
    return copy


def check_cases(args: argparse.Namespace) -> None:
    root = args.root
    cases = {c["id"]: c for c in json.loads((root / "cases.json").read_text())}
    manifest = json.loads((root / "manifest.json").read_text())
    images = manifest["images"]
    report = json.loads((args.run / "results.json").read_text())
    results = {r["id"]: r for r in report["results"]}
    failures: list[dict[str, Any]] = []
    summary = {"success_with_art": 0, "success_no_art": 0, "refused": 0, "equiv_groups": 0, "direct_passthrough": 0,
               "flag_errors": 0}
    base_audio: dict[str, bytes] = {}
    for fmt in FMT:
        base_audio[fmt] = parse_output(fmt, (root / "fixtures" / "bases" / f"base.{EXT[fmt]}").read_bytes())["audio"]
    for case_id, case in cases.items():
        result = results[case_id]
        expect = case.get("expect")
        problems: list[str] = []
        if expect and expect.get("equiv"):
            continue
        out_path = args.run / "files" / f"{case_id}.{EXT[case['dst_fmt']]}"
        data = out_path.read_bytes()
        if expect.get("passthrough"):
            summary["direct_passthrough"] += 1
        want_status = expect["status"]
        if result["status"] != want_status:
            problems.append(f"status {result['status']} != {want_status} ({result['error']})")
        if want_status in (1, 2):
            summary["refused" if want_status == 2 else "flag_errors"] += 1
            if expect.get("refusal") and expect["refusal"] not in result["error"]:
                problems.append(f"error text {result['error']!r} lacks {expect['refusal']!r}")
            if result["result_flags"] != 0:
                problems.append(f"result_flags {result['result_flags']} on refusal")
            if not result["dst_unchanged"]:
                problems.append("destination changed on refusal")
            if result["callbacks"]["serialize"] or result["callbacks"]["rewrite"]:
                problems.append("callbacks invoked on refusal")
        elif want_status == 0 and not problems:
            if result["result_flags"] != expect["result_flags"]:
                problems.append(f"result_flags {result['result_flags']} != {expect['result_flags']}")
            parsed = parse_output(case["dst_fmt"], data)
            if parsed["audio"] != base_audio[case["dst_fmt"]]:
                problems.append("audio stream bytes changed")
            if result["inspect"]["status"] != 0:
                problems.append(f"inspection failed: {result['inspect']['error']}")
            wanted = expect["pictures"]
            if wanted == "dst_unchanged_art":
                base = parse_output(case["dst_fmt"], (root / "fixtures" / case["dst"]).read_bytes())
                if parsed["pictures"] != base["pictures"]:
                    problems.append("destination artwork changed although the source had none")
                summary["success_no_art"] += 1
            elif wanted is None:
                if parsed["pictures"] and expect.get("wav_rewrite_drops_chunk"):
                    pictures_match(expect["pictures_if_kept"], parsed["pictures"], parsed["kind"], case["dst_fmt"], problems, images)
                    summary["wav_chunk_kept_by_reference_rewrite"] = summary.get("wav_chunk_kept_by_reference_rewrite", 0) + 1
                elif parsed["pictures"]:
                    problems.append("pictures present although none should be stored")
                else:
                    summary["wav_chunk_dropped_by_reference_rewrite"] = summary.get("wav_chunk_dropped_by_reference_rewrite", 0) + (1 if expect.get("wav_rewrite_drops_chunk") else 0)
                summary["success_no_art"] += 1
            else:
                summary["success_with_art"] += 1
                pictures_match(wanted, parsed["pictures"], parsed["kind"], case["dst_fmt"], problems, images)
                # TagLib readback through the bridge's inspector.
                kind = parsed["kind"]
                readback = inspect_pictures(result["inspect"]["json"], kind)
                if len(readback) != len(wanted):
                    problems.append(f"TagLib readback count {len(readback)} != {len(wanted)}")
                else:
                    for index, (want, got) in enumerate(zip(wanted, readback), 1):
                        if got["sha256"] != want["sha256"] or got["byte_count"] != want["size"]:
                            problems.append(f"TagLib readback picture {index}: bytes differ")
                        if kind == "mp4":
                            if got["format_code"] != want["flag"]:
                                problems.append(f"TagLib readback picture {index}: format {got['format_code']}")
                        else:
                            if got["picture_type"] != want["type"] or got["description"] != want["description"] or got["mime_type"] != want["mime"]:
                                problems.append(f"TagLib readback picture {index}: fields {got['picture_type']},{got['mime_type']!r},{got['description']!r}")
            # Non-artwork metadata equals the no-art reference run through the original function.
            reference = case.get("reference_src")
            if reference:
                ref_id = f"{case_id}~ref"
                ref = results.get(ref_id)
                if ref is None:
                    problems.append("missing reference result")
                else:
                    if strip_art(ref["inspect"]["json"]) != strip_art(result["inspect"]["json"]):
                        problems.append("non-artwork inspection differs from the no-art reference")
                    if ref.get("marker") != result.get("marker"):
                        problems.append("marker readback differs from the no-art reference")
                    if ref["status"] != 0:
                        problems.append(f"reference status {ref['status']}")
        if problems:
            failures.append({"case": case_id, "problems": problems, "status": result["status"], "error": result["error"]})
    # Equivalence groups: v07, new legacy symbols, `_ex(0)` / flag-on direct must agree exactly.
    groups: dict[str, list[dict[str, Any]]] = {}
    for case in cases.values():
        if case.get("group"):
            groups.setdefault(case["group"], []).append(case)
    for name, members in groups.items():
        summary["equiv_groups"] += 1
        keys = ["status", "error", "output_sha256", "output_size", "dst_unchanged", "callbacks", "marker", "inspect"]
        base_result = results[members[0]["id"]]
        for member in members[1:]:
            other = results[member["id"]]
            for key in keys:
                if base_result.get(key) != other.get(key):
                    failures.append({"case": member["id"], "problems": [f"{key} differs from {members[0]['id']}: "
                                                                        f"{base_result.get(key)!r} vs {other.get(key)!r}"][:1]})
            if member["impl"] == "new_ex" and member["flags"] == 0 and other["result_flags"] != 0 and other["status"] == 0:
                failures.append({"case": member["id"], "problems": ["result_flags set with flags=0"]})
    out = {"cases": len(cases), "failures": failures, "summary": summary, "machine": report["machine"],
           "bridge_sha256": report["bridge_sha256"], "features": report["features"]}
    with (args.run / "check.json").open("x") as handle:
        json.dump(out, handle, indent=1, sort_keys=True)
    print(f"checked {len(cases)} cases: {len(failures)} failures; {summary}")
    if failures:
        for failure in failures[:15]:
            print(json.dumps(failure)[:400])
        raise SystemExit(1)


def metaflac_pictures(path: Path) -> list[dict[str, Any]]:
    text = subprocess.run(["metaflac", "--list", "--block-type=PICTURE", str(path)], check=True, capture_output=True, text=True).stdout
    pictures: list[dict[str, Any]] = []
    for block in text.split("METADATA block #")[1:]:
        def field(name: str, block: str = block) -> str:
            match = next(line for line in block.splitlines() if line.strip().startswith(name + ":"))
            return match.split(":", 1)[1].strip()
        # `type:` appears twice: the block type (6) and then the picture type.
        types = re.findall(r"^\s*type: (\d+)", block, re.M)
        pictures.append({"type": int(types[1]), "mime": field("MIME type"), "description": field("description"),
                         "width": int(field("width")), "height": int(field("height")), "depth": int(field("depth")),
                         "colors": int(field("colors").split()[0]), "size": int(field("data length"))})
    return pictures


def stream_codecs(path: Path) -> list[str]:
    out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type,codec_name", "-of", "csv=p=0", str(path)],
                         check=True, capture_output=True, text=True).stdout.split()
    return [line.split(",")[0] for line in out if line.endswith(",video")]


def extract_stream(path: Path, index: int) -> bytes:
    return subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-i", str(path), "-map", f"0:v:{index}", "-c", "copy", "-f", "rawvideo", "-"],
                          check=True, capture_output=True).stdout


def audio_md5(path: Path) -> str:
    return subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-i", str(path), "-map", "0:a:0", "-f", "md5", "-"],
                          check=True, capture_output=True, text=True).stdout.strip()


def host_check(args: argparse.Namespace) -> None:
    cases = {c["id"]: c for c in json.loads((args.root / "cases.json").read_text())}
    results = {r["id"]: r for r in json.loads((args.run / "results.json").read_text())["results"]}
    base_md5 = {fmt: audio_md5(args.root / "fixtures" / "bases" / f"base.{EXT[fmt]}") for fmt in FMT}
    failures: list[dict[str, Any]] = []
    checked = {"outputs": 0, "pictures_extracted": 0, "metaflac_pictures": 0, "audio_md5": 0}
    for case_id, case in cases.items():
        expect = case.get("expect") or {}
        result = results[case_id]
        if case.get("impl") != "new_ex" or result["status"] != 0 or not isinstance(expect.get("pictures"), list):
            continue
        path = args.run / "files" / f"{case_id}.{EXT[case['dst_fmt']]}"
        wanted = expect["pictures"]
        problems: list[str] = []
        checked["outputs"] += 1
        # ffmpeg ignores empty pictures, reads only JPEG/PNG/BMP covr atoms, and trusts the MIME label, so
        # it cannot be asked about pass-through copies of deliberately bad bytes or the reference's PNG label.
        ffmpeg_wanted = [w for w in wanted if w["size"] > 0]
        if case["dst_fmt"] == "m4a":
            ffmpeg_wanted = [w for w in ffmpeg_wanted if w.get("flag") in (13, 14, 27)]
        # ffmpeg's webp decoder cannot open animated files, even for a stream copy.
        skip_ffmpeg = (case["set"].startswith("bad-") or case["set"] == "corpus-webp-animated" or
                       any(w.get("mislabeled") for w in wanted) or
                       (case["dst_fmt"] == "m4a" and any(w["size"] == 0 for w in wanted)))
        try:
            if not skip_ffmpeg:
                streams = stream_codecs(path)
                if len(streams) != len(ffmpeg_wanted):
                    problems.append(f"ffprobe sees {len(streams)} pictures, expected {len(ffmpeg_wanted)}")
                else:
                    for index, want in enumerate(ffmpeg_wanted):
                        payload = extract_stream(path, index)
                        checked["pictures_extracted"] += 1
                        if sha(payload) != want["sha256"]:
                            problems.append(f"ffmpeg picture {index + 1}: bytes differ")
            if audio_md5(path) != base_md5[case["dst_fmt"]]:
                problems.append("decoded audio differs from the destination base")
            checked["audio_md5"] += 1
            if case["dst_fmt"] == "flac":
                listed = metaflac_pictures(path)
                checked["metaflac_pictures"] += len(listed)
                if len(listed) != len(wanted):
                    problems.append(f"metaflac lists {len(listed)} pictures, expected {len(wanted)}")
                else:
                    for index, (want, got) in enumerate(zip(wanted, listed), 1):
                        for key in ("type", "mime", "description", "width", "height"):
                            if want.get(key) is not None and got[key] != want[key]:
                                problems.append(f"metaflac picture {index}: {key} {got[key]!r} != {want[key]!r}")
                        if got["size"] != want["size"]:
                            problems.append(f"metaflac picture {index}: size {got['size']} != {want['size']}")
        except subprocess.CalledProcessError as error:
            problems.append(f"tool failed: {error.cmd[0]}: {(error.stderr or b'')[:200]!r}")
        if problems:
            failures.append({"case": case_id, "problems": problems})
    out = {"checked": checked, "failures": failures, "tools": {
        "ffmpeg": subprocess.run(["ffmpeg", "-version"], capture_output=True, text=True).stdout.splitlines()[0],
        "metaflac": subprocess.run(["metaflac", "--version"], capture_output=True, text=True).stdout.strip()}}
    with (args.run / "hostcheck.json").open("x") as handle:
        json.dump(out, handle, indent=1, sort_keys=True)
    print(f"hostcheck {checked}: {len(failures)} failures")
    for failure in failures[:15]:
        print(json.dumps(failure)[:400])
    if failures:
        raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    gen = sub.add_parser("gen")
    gen.add_argument("--root", type=Path, required=True)
    run = sub.add_parser("run")
    run.add_argument("--root", type=Path, required=True)
    run.add_argument("--bridge", type=Path, required=True)
    run.add_argument("--old", type=Path)
    run.add_argument("--out", type=Path, required=True)
    run.add_argument("--keep-all", action="store_true")
    check = sub.add_parser("check")
    check.add_argument("--root", type=Path, required=True)
    check.add_argument("--run", type=Path, required=True)
    host = sub.add_parser("hostcheck")
    host.add_argument("--root", type=Path, required=True)
    host.add_argument("--run", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "gen":
        generate(args.root)
    elif args.command == "run":
        run_cases(args)
    elif args.command == "check":
        check_cases(args)
    elif args.command == "hostcheck":
        host_check(args)


if __name__ == "__main__":
    main()
