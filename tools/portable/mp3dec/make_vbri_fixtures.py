#!/usr/bin/env python3
"""Generate the VBRI and truncation fixtures of the portable MP3 decoder tests (macOS only: needs apple_probe).

Reads the base fixtures written by make_fixtures.py in tests/fixtures/mp3 (plus one VBR payload that this script
encodes with lame), builds MP3 files that start with a Fraunhofer VBRI frame in front of the base file's audio frames
with chosen byte/frame/version/delay fields, cuts base files at every byte offset of their last frame, and records what
AudioToolbox (the engine's `apple_codec::Reader`, via apple_probe) reports for each: length property, bitrate property,
frames delivered and the samples.

    APPLE_PROBE=/path/to/apple_probe python3 tools/portable/mp3dec/make_vbri_fixtures.py tests/fixtures/mp3

Writes tests/fixtures/mp3/vbri/{NAME.mp3,NAME.apple.s16,manifest.json} and truncation/{NAME.apple.s16,manifest.json}.
"""
import json
import os
import struct
import subprocess
import sys
import wave

import numpy as np

BASE = sys.argv[1] if len(sys.argv) > 1 else "tests/fixtures/mp3"
PROBE = os.environ["APPLE_PROBE"]
VBRI_DIR = os.path.join(BASE, "vbri")
TRUNC_DIR = os.path.join(BASE, "truncation")
os.makedirs(VBRI_DIR, exist_ok=True)
os.makedirs(TRUNC_DIR, exist_ok=True)

BR = {3: [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
      2: [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
      0: [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160]}
SR = {3: [44100, 48000, 32000], 2: [22050, 24000, 16000], 0: [11025, 12000, 8000]}
SIDE = {(3, False): 32, (3, True): 17, (2, False): 17, (2, True): 9, (0, False): 17, (0, True): 9}


def run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"{cmd}: {r.stderr}")
    return r.stdout


def parse(h):
    word = struct.unpack(">I", h)[0]
    if word >> 21 != 0x7ff:
        return None
    ver, layer, bi, si = (word >> 19) & 3, (word >> 17) & 3, (word >> 12) & 15, (word >> 10) & 3
    if ver == 1 or layer != 1 or bi in (0, 15) or si == 3:
        return None
    br = BR[ver][bi] * 1000
    sr = SR[ver][si]
    size = (144 * br // sr if ver == 3 else 72 * br // sr) + ((word >> 9) & 1)
    return dict(ver=ver, br=br, sr=sr, size=size, mode=(word >> 6) & 3)


def split(data):
    """Return (frames, trailing_bytes) of a file made of Layer III frames."""
    pos, frames = 0, []
    if data[:3] == b"ID3":
        pos = 10 + ((data[6] << 21) | (data[7] << 14) | (data[8] << 7) | data[9])
    while pos + 4 <= len(data):
        h = parse(data[pos:pos + 4])
        if h is None or pos + h["size"] > len(data):
            break
        frames.append(data[pos:pos + h["size"]])
        pos += h["size"]
    return frames, data[pos:]


def has_xing(frame):
    h = parse(frame[:4])
    at = 4 + SIDE[(h["ver"], h["mode"] == 3)]
    return frame[at:at + 4] in (b"Xing", b"Info")


def audio_frames(path):
    frames, _ = split(open(path, "rb").read())
    return frames[1:] if has_xing(frames[0]) else frames


def apple(path, tag):
    info = json.loads(run([PROBE, "info", path]))
    out = f"/tmp/pn_make_vbri_{os.getpid()}_{tag}.f32"
    dump = json.loads(run([PROBE, "dump", path, out, "inter", "4096"]))
    assert info["ok"] and dump["ok"], (path, info, dump)
    samples = np.fromfile(out, dtype="<f4")
    os.remove(out)
    grid = np.round(samples.astype(np.float64) * 32768.0)
    assert np.all(np.abs(grid / 32768.0 - samples) < 1e-9)
    return dict(length=info["length_frames"], bitrate=info["bitrate"], delivered=dump["frames"], channels=info["channels"],
                samples=grid.clip(-32768, 32767).astype("<i2"))


def vbri_frame(template, byte_count, frame_count, version=1, delay=0, quality=0):
    f = bytearray(len(template))
    f[:4] = template[:4]
    f[36:40] = b"VBRI"
    f[40:46] = struct.pack(">HHH", version, delay, quality)
    f[46:54] = struct.pack(">II", byte_count, frame_count)
    f[54:62] = struct.pack(">HHHH", 1, 1, 4, 1)
    return bytes(f)


# A VBR payload for the first-ten-frames bit rate fallback (MPEG-1, 44.1 kHz).
vbr = os.path.join(VBRI_DIR, "payload_vbrV2_44100.mp3")
wav = vbr[:-4] + ".wav"
rate = 44100
t = np.arange(int(0.55 * rate)) / rate
rng = np.random.default_rng(21)
x = np.stack([0.3 * np.sin(2 * np.pi * 220 * t) * (0.5 + 0.5 * np.sin(2 * np.pi * 5 * t)),
              0.2 * np.sin(2 * np.pi * 330 * t + 1)], axis=1)
for _ in range(10):
    p = int(rng.integers(0, len(t) - 1500))
    x[p:p + 700] += rng.normal(0, 0.3, size=(700, 2))
x = np.clip(x + rng.normal(0, 0.0005, size=x.shape), -0.98, 0.98)
with wave.open(wav, "wb") as w:
    w.setnchannels(2)
    w.setsampwidth(2)
    w.setframerate(rate)
    w.writeframes(np.round(x * 32767).astype("<i2").tobytes())
run(["lame", "--silent", "-s", "44.1", "-V", "2", "-m", "j", wav, vbr])
os.remove(wav)

BASES = {  # layout -> source file
    "v1s": os.path.join(BASE, "lame_cbr128_44100_js.mp3"),
    "v1vbr": vbr,
    "v1m": os.path.join(BASE, "lame_cbr128_44100_mono.mp3"),
    "v2svbr": os.path.join(BASE, "lame_vbrV2_22050_js.mp3"),
    "v25m": os.path.join(BASE, "lame_cbr32_11025_mono.mp3"),
}
# name, layout, byte field, frame field, ignored extras, keep the samples?
# N = audio frames, F = VBRI frame size, P = bytes of the audio frames.
CASES = [
    ("vbri_v1s_normal", "v1s", "P+F", "N+1", {}, True),
    ("vbri_v1s_frames_short", "v1s", "P+F", "2", {}, False),
    ("vbri_v1s_frames_long", "v1s", "P+F", "N+6", {}, False),
    ("vbri_v1s_frames0", "v1s", "P+F", "0", {}, False),
    ("vbri_v1s_frames1", "v1s", "P+F", "1", {}, False),
    ("vbri_v1s_bytes0", "v1s", "0", "N+1", {}, False),
    ("vbri_v1s_bytes_eq_f", "v1s", "F", "N+1", {}, False),
    ("vbri_v1s_bytes_lt_f", "v1s", "100", "N+1", {}, False),
    ("vbri_v1s_frames_huge", "v1s", "P+F", "4294967295", {}, False),
    ("vbri_v1s_bytes_huge", "v1s", "4294967295", "N+1", {}, False),
    ("vbri_v1s_ignored_fields", "v1s", "P+F", "N+1", dict(version=2, delay=1105, quality=75), False),
    ("vbri_v1vbr_normal", "v1vbr", "P+F", "N+1", {}, True),
    ("vbri_v1vbr_bytes0", "v1vbr", "0", "N+1", {}, False),
    ("vbri_v1vbr_frames0", "v1vbr", "P+F", "0", {}, False),
    ("vbri_v1m_normal", "v1m", "P+F", "N+1", {}, True),
    ("vbri_v2svbr_normal", "v2svbr", "P+F", "N+1", {}, True),
    ("vbri_v2svbr_bytes0", "v2svbr", "0", "N+1", {}, False),
    ("vbri_v25m_normal", "v25m", "P+F", "N+1", {}, True),
]
manifest, reference = [], {}
for name, layout, bytes_expr, frames_expr, extra, store in CASES:
    aud = audio_frames(BASES[layout])
    env = dict(N=len(aud), F=len(aud[0]), P=sum(map(len, aud)))
    byte_count, frame_count = eval(bytes_expr, {}, env), eval(frames_expr, {}, env)
    path = os.path.join(VBRI_DIR, name + ".mp3")
    open(path, "wb").write(vbri_frame(aud[0], byte_count, frame_count, **extra) + b"".join(aud))
    a = apple(path, name)
    if layout in reference:
        assert np.array_equal(reference[layout], a["samples"]), f"{name}: AudioToolbox samples differ from the layout's others"
    else:
        reference[layout] = a["samples"]
    if store:
        a["samples"].tofile(os.path.join(VBRI_DIR, name + ".apple.s16"))
    manifest.append(dict(name=name, layout=layout, audio_frames=len(aud), vbri_bytes=byte_count, vbri_frames=frame_count,
                         vbri_extra=extra, channels=a["channels"], apple_length_property=a["length"],
                         apple_bitrate=a["bitrate"], apple_frames_delivered=a["delivered"],
                         samples_file=[c[0] for c in CASES if c[1] == layout and c[5]][0] + ".apple.s16"))
json.dump(manifest, open(os.path.join(VBRI_DIR, "manifest.json"), "w"), indent=1)
print(f"wrote {len(manifest)} VBRI fixtures")

# Truncation: cut a base file at every byte offset of its last frame.
TRUNC = ["lame_cbr128_44100_js", "tagless_cbr128_44100", "lame_cbr32_11025_mono", "lame_vbrV2_22050_js"]
tm = []
for name in TRUNC:
    data = open(os.path.join(BASE, name + ".mp3"), "rb").read()
    frames, trailing = split(data)
    assert not trailing, f"{name}: unexpected trailing bytes"
    last_start = len(data) - len(frames[-1])
    answers, first_samples = set(), None
    for k in range(len(frames[-1])):
        cut = f"/tmp/pn_trunc_{os.getpid()}.mp3"
        open(cut, "wb").write(data[:last_start + k])
        a = apple(cut, "t")
        answers.add((a["length"], a["bitrate"], a["delivered"]))
        if first_samples is None:
            first_samples = a["samples"]
        assert np.array_equal(first_samples, a["samples"]), f"{name}: samples differ at cut {k}"
        os.remove(cut)
    assert len(answers) == 1, f"{name}: AudioToolbox's answer depends on the cut: {answers}"
    (length, bitrate, delivered), = answers
    first_samples.tofile(os.path.join(TRUNC_DIR, name + ".apple.s16"))
    channels = json.loads(run([PROBE, "info", os.path.join(BASE, name + ".mp3")]))["channels"]
    tm.append(dict(name=name, last_frame_bytes=len(frames[-1]), cuts=len(frames[-1]), apple_length_property=length,
                   apple_bitrate=bitrate, apple_frames_delivered=delivered, samples_file=name + ".apple.s16", channels=channels))
json.dump(tm, open(os.path.join(TRUNC_DIR, "manifest.json"), "w"), indent=1)
print(f"wrote {len(tm)} truncation references")
