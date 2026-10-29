#!/usr/bin/env python3
"""Container writers/parsers for the artwork-copy matrix that do not use TagLib.

Everything here is plain Python over bytes so that fixtures are authored, and
bridge outputs are read back, by code independent of the bridge's TagLib.
"""
from __future__ import annotations

import struct
from typing import Any

# ---------------------------------------------------------------------------
# ID3v2

ID3_ENCODINGS = {"latin1": 0, "utf16": 1, "utf16be": 2, "utf8": 3}
_PY_CODEC = {0: "latin-1", 1: "utf-16", 2: "utf-16-be", 3: "utf-8"}


def syncsafe(value: int) -> bytes:
    if value >= 1 << 28:
        raise ValueError("value too large for a synchsafe integer")
    return bytes([(value >> 21) & 0x7F, (value >> 14) & 0x7F, (value >> 7) & 0x7F, value & 0x7F])


def unsyncsafe(raw: bytes) -> int:
    return (raw[0] << 21) | (raw[1] << 14) | (raw[2] << 7) | raw[3]


def _terminator(encoding: int) -> bytes:
    return b"\0\0" if encoding in (1, 2) else b"\0"


def id3_text(text: str, encoding: str) -> bytes:
    code = ID3_ENCODINGS[encoding]
    return text.encode(_PY_CODEC[code])


def id3_frame(frame_id: str, body: bytes, version: int = 4) -> bytes:
    size = syncsafe(len(body)) if version == 4 else struct.pack(">I", len(body))
    return frame_id.encode("ascii") + size + b"\0\0" + body


def apic_frame(mime: str, picture_type: int, description: str, data: bytes,
               encoding: str = "latin1", version: int = 4) -> bytes:
    code = ID3_ENCODINGS[encoding]
    body = (bytes([code]) + mime.encode("latin-1") + b"\0" + bytes([picture_type]) +
            id3_text(description, encoding) + _terminator(code) + data)
    return id3_frame("APIC", body, version)


def text_frame(frame_id: str, text: str, version: int = 4) -> bytes:
    return id3_frame(frame_id, b"\x03" + text.encode("utf-8") + b"\0", version)


def comm_frame(text: str, version: int = 4) -> bytes:
    return id3_frame("COMM", b"\x03" + b"eng" + b"\0" + text.encode("utf-8"), version)


def id3v2_tag(frames: list[bytes], version: int = 4, padding: int = 0) -> bytes:
    body = b"".join(frames) + b"\0" * padding
    return b"ID3" + bytes([version, 0, 0]) + syncsafe(len(body)) + body


def id3v2_total_size(data: bytes, offset: int = 0) -> int:
    if data[offset:offset + 3] != b"ID3":
        return 0
    flags = data[offset + 5]
    return 10 + unsyncsafe(data[offset + 6:offset + 10]) + (10 if flags & 0x10 else 0)


def _decode_text(raw: bytes, encoding: int) -> str:
    return raw.decode(_PY_CODEC[encoding])


def parse_apic(body: bytes) -> dict[str, Any]:
    encoding = body[0]
    end = body.index(b"\0", 1)
    mime = body[1:end].decode("latin-1")
    picture_type = body[end + 1]
    position = end + 2
    terminator = _terminator(encoding)
    if encoding in (1, 2):
        # UTF-16 terminators are aligned to two bytes.
        scan = position
        while True:
            at = body.index(b"\0\0", scan)
            if (at - position) % 2 == 0:
                break
            scan = at + 1
        description = _decode_text(body[position:at], encoding)
        position = at + 2
    else:
        at = body.index(terminator, position)
        description = _decode_text(body[position:at], encoding)
        position = at + 1
    return {"mime": mime, "type": picture_type, "description": description, "data": body[position:],
            "text_encoding": encoding}


def parse_id3v2_pictures(data: bytes, offset: int = 0) -> list[dict[str, Any]]:
    """Every APIC frame of the ID3v2.3/2.4 tag at `offset`, in order."""
    if data[offset:offset + 3] != b"ID3":
        return []
    version = data[offset + 3]
    flags = data[offset + 5]
    size = unsyncsafe(data[offset + 6:offset + 10])
    body = data[offset + 10:offset + 10 + size]
    if flags & 0x80:  # whole-tag unsynchronisation
        body = body.replace(b"\xff\x00", b"\xff")
    position = 0
    if flags & 0x40:  # extended header
        ext = unsyncsafe(body[:4]) if version == 4 else struct.unpack(">I", body[:4])[0] + 4
        position = ext
    out = []
    while position + 10 <= len(body):
        frame_id = body[position:position + 4]
        if frame_id[0] == 0:
            break
        frame_size = unsyncsafe(body[position + 4:position + 8]) if version == 4 else struct.unpack(">I", body[position + 4:position + 8])[0]
        frame_flags = body[position + 8:position + 10]
        payload = body[position + 10:position + 10 + frame_size]
        if frame_id == b"APIC":
            if version == 4 and frame_flags[1] & 0x02:
                payload = payload.replace(b"\xff\x00", b"\xff")
            out.append(parse_apic(payload))
        position += 10 + frame_size
    return out


def mp3_with_id3(audio: bytes, tag: bytes) -> bytes:
    return tag + audio


# ---------------------------------------------------------------------------
# FLAC

FLAC_PICTURE = 6


def flac_picture_body(picture_type: int, mime: str, description: str, width: int, height: int,
                      depth: int, colors: int, data: bytes) -> bytes:
    mime_raw, description_raw = mime.encode("utf-8"), description.encode("utf-8")
    return (struct.pack(">II", picture_type, len(mime_raw)) + mime_raw +
            struct.pack(">I", len(description_raw)) + description_raw +
            struct.pack(">IIIII", width, height, depth, colors, len(data)) + data)


def flac_blocks(data: bytes) -> tuple[int, list[tuple[int, bytes]], int]:
    """(offset of the fLaC marker, [(type, body)], offset of the first audio frame)."""
    start = id3v2_total_size(data)
    if data[start:start + 4] != b"fLaC":
        raise ValueError("not a FLAC stream")
    position = start + 4
    blocks = []
    while True:
        header = data[position]
        last, kind = bool(header & 0x80), header & 0x7F
        size = int.from_bytes(data[position + 1:position + 4], "big")
        blocks.append((kind, data[position + 4:position + 4 + size]))
        position += 4 + size
        if last:
            return start, blocks, position


def flac_assemble(blocks: list[tuple[int, bytes]]) -> bytes:
    out = b"fLaC"
    for index, (kind, body) in enumerate(blocks):
        last = 0x80 if index == len(blocks) - 1 else 0
        if len(body) > 0xFFFFFF:
            raise ValueError("FLAC block too large")
        out += bytes([kind | last]) + len(body).to_bytes(3, "big") + body
    return out


def flac_with_pictures(base: bytes, pictures: list[bytes]) -> bytes:
    """Insert picture blocks (already-rendered bodies) before the final block."""
    start, blocks, audio = flac_blocks(base)
    new = blocks[:-1] + [(FLAC_PICTURE, body) for body in pictures] + blocks[-1:]
    return base[:start] + flac_assemble(new) + base[audio:]


def parse_flac_pictures(data: bytes) -> list[dict[str, Any]]:
    _, blocks, _ = flac_blocks(data)
    out = []
    for kind, body in blocks:
        if kind != FLAC_PICTURE:
            continue
        picture_type, mime_length = struct.unpack(">II", body[:8])
        position = 8
        mime = body[position:position + mime_length].decode("utf-8")
        position += mime_length
        (description_length,) = struct.unpack(">I", body[position:position + 4])
        position += 4
        description = body[position:position + description_length].decode("utf-8")
        position += description_length
        width, height, depth, colors, length = struct.unpack(">IIIII", body[position:position + 20])
        position += 20
        picture = body[position:position + length]
        if len(picture) != length or position + length != len(body):
            raise ValueError("FLAC picture block length mismatch")
        out.append({"type": picture_type, "mime": mime, "description": description, "width": width,
                    "height": height, "depth": depth, "colors": colors, "data": picture})
    return out


def flac_audio(data: bytes) -> bytes:
    _, _, audio = flac_blocks(data)
    end = len(data)
    if data[end - 128:end - 125] == b"TAG":
        end -= 128
    return data[audio:end]


# ---------------------------------------------------------------------------
# MP4

MP4_TYPES = {"jpeg": 13, "png": 14, "gif": 12, "bmp": 27, "implicit": 0}


def _atom(name: bytes, body: bytes) -> bytes:
    return struct.pack(">I", 8 + len(body)) + name + body


def mp4_children(data: bytes, start: int, end: int) -> list[tuple[bytes, int, int, int]]:
    """[(name, offset, size, header_length)]"""
    out = []
    position = start
    while position + 8 <= end:
        size = struct.unpack(">I", data[position:position + 4])[0]
        name = data[position + 4:position + 8]
        header = 8
        if size == 1:
            size = struct.unpack(">Q", data[position + 8:position + 16])[0]
            header = 16
        elif size == 0:
            size = end - position
        if size < header or position + size > end:
            raise ValueError(f"bad MP4 atom {name!r} at {position}")
        out.append((name, position, size, header))
        position += size
    return out


def mp4_path(data: bytes, *names: bytes) -> list[tuple[bytes, int, int, int]]:
    chain: list[tuple[bytes, int, int, int]] = []
    start, end = 0, len(data)
    for name in names:
        found = next((atom for atom in mp4_children(data, start, end) if atom[0] == name), None)
        if found is None:
            raise KeyError(name)
        chain.append(found)
        _, offset, size, header = found
        start = offset + header + (4 if name == b"meta" else 0)
        end = offset + size
    return chain


def _patch_size(data: bytearray, atom: tuple[bytes, int, int, int], delta: int) -> None:
    _, offset, size, header = atom
    if header != 8:
        raise ValueError("64-bit atoms are not used by these fixtures")
    data[offset:offset + 4] = struct.pack(">I", size + delta)


def mp4_with_covers(base: bytes, covers: list[tuple[int, bytes]]) -> bytes:
    chain = mp4_path(base, b"moov", b"udta", b"meta", b"ilst")
    ilst = chain[-1]
    covr = _atom(b"covr", b"".join(
        struct.pack(">I", 16 + len(payload)) + b"data" + struct.pack(">I", flag) + b"\0\0\0\0" + payload
        for flag, payload in covers))
    insert_at = ilst[1] + ilst[2]
    out = bytearray(base[:insert_at] + covr + base[insert_at:])
    for atom in chain:
        _patch_size(out, atom, len(covr))
    moov = chain[0]
    mdat = next((a for a in mp4_children(base, 0, len(base)) if a[0] == b"mdat"), None)
    if mdat is not None and mdat[1] > moov[1]:
        _shift_chunk_offsets(out, moov, len(covr))
    return bytes(out)


def _shift_chunk_offsets(data: bytearray, moov: tuple[bytes, int, int, int], delta: int) -> None:
    def walk(start: int, end: int) -> None:
        for name, offset, size, header in mp4_children(bytes(data), start, end):
            body = offset + header
            if name in (b"trak", b"mdia", b"minf", b"stbl"):
                walk(body, offset + size)
            elif name == b"stco":
                count = struct.unpack(">I", data[body + 4:body + 8])[0]
                for i in range(count):
                    at = body + 8 + 4 * i
                    data[at:at + 4] = struct.pack(">I", struct.unpack(">I", data[at:at + 4])[0] + delta)
            elif name == b"co64":
                count = struct.unpack(">I", data[body + 4:body + 8])[0]
                for i in range(count):
                    at = body + 8 + 8 * i
                    data[at:at + 8] = struct.pack(">Q", struct.unpack(">Q", data[at:at + 8])[0] + delta)
    walk(moov[1] + moov[3], moov[1] + moov[2])


def parse_mp4_covers(data: bytes) -> list[dict[str, Any]]:
    try:
        chain = mp4_path(data, b"moov", b"udta", b"meta", b"ilst")
    except KeyError:
        return []
    ilst = chain[-1]
    out = []
    for name, offset, size, header in mp4_children(data, ilst[1] + ilst[3], ilst[1] + ilst[2]):
        if name != b"covr":
            continue
        for dname, doffset, dsize, dheader in mp4_children(data, offset + header, offset + size):
            if dname != b"data":
                raise ValueError("covr child is not a data atom")
            body = data[doffset + dheader:doffset + dsize]
            out.append({"flag": struct.unpack(">I", body[:4])[0] & 0xFFFFFF, "data": body[8:]})
    return out


def mp4_mdat_payload(data: bytes) -> bytes:
    return b"".join(data[offset + header:offset + size]
                    for name, offset, size, header in mp4_children(data, 0, len(data)) if name == b"mdat")


# ---------------------------------------------------------------------------
# RIFF/AIFF with an ID3 chunk

def aiff_with_id3(base: bytes, tag: bytes) -> bytes:
    if base[:4] != b"FORM":
        raise ValueError("not AIFF")
    pad = b"\0" if len(tag) & 1 else b""
    chunk = b"ID3 " + struct.pack(">I", len(tag)) + tag + pad
    end = struct.unpack(">I", base[4:8])[0] + 8
    return b"FORM" + struct.pack(">I", end - 8 + len(chunk)) + base[8:end] + chunk + base[end:]


def wav_with_id3(base: bytes, tag: bytes) -> bytes:
    if base[:4] != b"RIFF":
        raise ValueError("not WAV")
    pad = b"\0" if len(tag) & 1 else b""
    chunk = b"id3 " + struct.pack("<I", len(tag)) + tag + pad
    end = struct.unpack("<I", base[4:8])[0] + 8
    return b"RIFF" + struct.pack("<I", end - 8 + len(chunk)) + base[8:end] + chunk + base[end:]


def riff_chunks(data: bytes, big_endian: bool) -> list[tuple[bytes, int, int]]:
    """[(id, payload offset, payload size)] of a RIFF/FORM container."""
    fmt = ">I" if big_endian else "<I"
    end = struct.unpack(fmt, data[4:8])[0] + 8
    position, out = 12, []
    while position + 8 <= end:
        size = struct.unpack(fmt, data[position + 4:position + 8])[0]
        out.append((data[position:position + 4], position + 8, size))
        position += 8 + size + (size & 1)
    return out


def parse_riff_id3_pictures(data: bytes, big_endian: bool) -> list[dict[str, Any]]:
    for chunk_id, offset, size in riff_chunks(data, big_endian):
        if chunk_id.lower() == b"id3 ":
            return parse_id3v2_pictures(data[offset:offset + size])
    return []


def riff_audio(data: bytes, big_endian: bool) -> bytes:
    wanted = b"SSND" if big_endian else b"data"
    for chunk_id, offset, size in riff_chunks(data, big_endian):
        if chunk_id == wanted:
            return data[offset:offset + size]
    raise ValueError("no audio chunk")


def mp3_audio(data: bytes) -> bytes:
    start = id3v2_total_size(data)
    end = len(data)
    if data[end - 128:end - 125] == b"TAG":
        end -= 128
    return data[start:end]
