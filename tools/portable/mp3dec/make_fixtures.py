#!/usr/bin/env python3
"""Regenerate tests/fixtures/mp3/ for the portable MP3 decoder tests.

All audio is generated here (sines, noise bursts, a clipped sweep); no real media is involved.
Needs `lame`, `ffmpeg` and numpy. Apple references additionally need the macOS
`apple_probe` binary built from tools/portable/mp3dec/apple_probe (set APPLE_PROBE).

    APPLE_PROBE=/path/to/apple_probe python3 tools/portable/mp3dec/make_fixtures.py tests/fixtures/mp3

Per fixture NAME it writes NAME.mp3 and NAME.apple.s16 (AudioToolbox's decode as little-endian
int16, channels interleaved, exactly the channel count AudioToolbox reports), and a
manifest.json with the encoder command and AudioToolbox's reported length.
"""
import json, os, subprocess, sys, wave
import numpy as np

OUT = sys.argv[1] if len(sys.argv) > 1 else "tests/fixtures/mp3"
PROBE = os.environ.get("APPLE_PROBE")
os.makedirs(OUT, exist_ok=True)


def signal(seconds, rate, channels, seed, loud=False):
    rng = np.random.default_rng(seed)
    n = int(round(seconds * rate))
    t = np.arange(n) / rate
    out = np.zeros((n, channels))
    for c in range(channels):
        f0 = 220 * (1 + 0.5 * c)
        x = 0.25 * np.sin(2 * np.pi * f0 * t) + 0.12 * np.sin(2 * np.pi * f0 * 1.5 * t + c) + 0.06 * np.sin(2 * np.pi * f0 * 3.01 * t)
        out[:, c] = x * 0.5 * (1 + np.sin(2 * np.pi * 7.1 * t + c))
    for _ in range(max(1, int(seconds * 12))):           # noise bursts force short blocks
        p = int(rng.integers(0, max(1, n - 1500)))
        length = min(int(rng.integers(100, 900)), n - p)
        out[p:p + length, :] += rng.normal(0, 0.25, size=(length, channels))
    out += rng.normal(0, 0.0005, size=out.shape)
    if loud:                                             # drive the encoder into full scale
        out = np.sign(out) * np.minimum(np.abs(out) * 6.0, 1.0)
        out = out + 0.2 * np.sin(2 * np.pi * 3000 * t)[:, None]
    return np.clip(out, -0.98, 0.98)


def write_wav(path, x, rate):
    with wave.open(path, "wb") as w:
        w.setnchannels(x.shape[1]); w.setsampwidth(2); w.setframerate(rate)
        w.writeframes(np.round(x * 32767).astype("<i2").tobytes())


def run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"{cmd}: {r.stderr}")
    return r.stdout


# name, encoder, rate, channels, seconds, seed, loud, args
CASES = [
    ("lame_cbr128_44100_js",     "lame", 44100, 2, 0.30, 1, False, ["--cbr", "-b", "128", "-m", "j"]),
    ("lame_cbr128_44100_mono",   "lame", 44100, 1, 0.30, 2, False, ["--cbr", "-b", "128", "-m", "m"]),
    ("lame_cbr192_48000_stereo", "lame", 48000, 2, 0.30, 3, False, ["--cbr", "-b", "192", "-m", "s"]),
    ("lame_abr96_32000_js",      "lame", 32000, 2, 0.30, 4, False, ["--abr", "96", "-m", "j"]),
    ("lame_vbrV2_22050_js",      "lame", 22050, 2, 0.30, 5, False, ["-V", "2", "-m", "j"]),
    ("lame_cbr32_11025_mono",    "lame", 11025, 1, 0.30, 6, False, ["--cbr", "-b", "32", "-m", "m"]),
    ("lame_cbr128_44100_crc",    "lame", 44100, 2, 0.30, 7, False, ["--cbr", "-b", "128", "-m", "j", "-p"]),
    ("lame_loud_cbr128",         "lame", 44100, 2, 0.30, 8, True,  ["--cbr", "-b", "128", "-m", "j"]),
    ("ffmpeg_cbr128_44100",      "ffmpeg", 44100, 2, 0.30, 9, False, ["-b:a", "128k"]),
    ("tagless_cbr128_44100",     "ffmpeg", 44100, 2, 0.30, 10, False, ["-b:a", "128k", "-write_xing", "0"]),
]

manifest = []
for name, enc, rate, ch, secs, seed, loud, args in CASES:
    wav = os.path.join(OUT, name + ".wav")
    mp3 = os.path.join(OUT, name + ".mp3")
    write_wav(wav, signal(secs, rate, ch, seed, loud), rate)
    if enc == "lame":
        run(["lame", "--silent", "-s", str(rate / 1000), *args, wav, mp3])
    else:
        run(["ffmpeg", "-y", "-loglevel", "error", "-i", wav, "-c:a", "libmp3lame", *args, mp3])
    os.remove(wav)
    entry = {"name": name, "encoder": enc, "args": args, "rate": rate, "channels": ch}
    if PROBE:
        info = json.loads(run([PROBE, "info", mp3]))
        ref = os.path.join(OUT, name + ".apple.f32")
        dump = json.loads(run([PROBE, "dump", mp3, ref, "inter", "4096"]))
        assert dump["ok"], dump
        samples = np.fromfile(ref, dtype="<f4")
        os.remove(ref)
        grid = np.round(samples.astype(np.float64) * 32768.0)
        assert np.all(np.abs(grid / 32768.0 - samples) < 1e-9), "AudioToolbox output is not on the int16 grid"
        grid.clip(-32768, 32767).astype("<i2").tofile(os.path.join(OUT, name + ".apple.s16"))
        entry.update(apple_channels=info["channels"], apple_length_property=info["length_frames"],
                     apple_frames_delivered=dump["frames"], apple_bitrate=info["bitrate"])
    manifest.append(entry)
json.dump(manifest, open(os.path.join(OUT, "manifest.json"), "w"), indent=1)
print(f"wrote {len(manifest)} fixtures to {OUT}")
