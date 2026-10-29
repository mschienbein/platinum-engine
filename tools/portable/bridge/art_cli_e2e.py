#!/usr/bin/env python3
"""End-to-end check of cover art into and out of WAV through the real `platinum` CLI (not the bridge alone).

  gen  host: ffmpeg + art_containers write 1-second FLAC/MP3/ALAC(M4A)/AIFF/WAV sources, with and without pictures
       (no tags at all, so the ID3 chunk TagLib saves into a WAV is the last chunk and only the opt-in keeps it).
  run  host or image: `platinum process --format F` for every source and destination format, with
       `parity.allow_non_identical` on and off, and verify the pictures of every published file with stdlib parsers.

With the opt-in on, pictures reach every destination byte for byte, WAV included, in both directions; with it off a
picture is silently not carried into or out of a WAV (the macOS reference behaviour), and the generic copies that
need the transcoder are still refused with `artwork_conversion_unavailable`.

  art_cli_e2e.py gen --art ART_TREE --out SOURCES_DIR
  art_cli_e2e.py run --sources SOURCES_DIR --binary platinum --tag-bridge LIB --flac LIB --lame LIB --work NEW_DIR --out RECEIPT.json
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import art_containers as ac  # noqa: E402

FORMATS = ("flac", "mp3", "m4a", "aiff", "wav")
DEST = {"wav": "wav", "flac": "flac", "mp3": "mp3", "aiff": "aiff", "alac": "m4a"}  # --format value -> output container
SOURCE_OF = {"flac": "flac", "mp3": "mp3", "m4a": "alac", "aiff": "aiff", "wav": "wav"}  # source container -> its own --format
MIME = {"png": "image/png", "jpeg": "image/jpeg", "gif": "image/gif", "bmp": "image/bmp", "webp": "image/webp"}
MP4_FLAG = {"jpeg": 13, "png": 14, "gif": 12, "bmp": 27}

# picture sets: name -> [(image file under fixtures/images, kind, picture type, description)]
SETS: dict[str, list[tuple[str, str, int, str]]] = {
    "png": [("png-rgb", "png", 3, "front cover")],
    "jpeg": [("jpeg-baseline", "jpeg", 4, "back cover")],
    "webp": [("webp-lossy", "webp", 3, "webp cover")],
    "multi": [("jpeg-baseline", "jpeg", 3, "front"), ("png-rgb", "png", 4, "back"), ("gif-single", "gif", 6, "media"), ("bmp-24", "bmp", 20, "logo")],
    "none": [],
}


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sine(path: Path, fmt: str) -> bytes:
    codec = {"mp3": ["-c:a", "libmp3lame", "-b:a", "128k", "-write_id3v2", "0", "-write_xing", "0", "-f", "mp3"],
             "m4a": ["-c:a", "alac", "-f", "ipod"], "wav": ["-c:a", "pcm_s16le", "-f", "wav"],
             "aiff": ["-c:a", "pcm_s16be", "-f", "aiff"], "flac": ["-c:a", "flac", "-f", "flac"]}[fmt]
    subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-f", "lavfi", "-i", "sine=frequency=431:sample_rate=44100:duration=1",
                    "-ac", "2", "-map_metadata", "-1", "-fflags", "+bitexact", "-flags:a", "+bitexact", *codec, str(path)],
                   check=True, capture_output=True)
    return path.read_bytes()


def gen(args: argparse.Namespace) -> None:
    out: Path = args.out
    out.mkdir()
    scratch = out / "tmp"
    scratch.mkdir()
    audio = {fmt: sine(scratch / f"base.{fmt}", fmt) for fmt in FORMATS}
    # ffmpeg writes an empty ID3v2 tag even with -write_id3v2 0; the sources carry exactly the tag they are given.
    if audio["mp3"][:3] == b"ID3":
        audio["mp3"] = audio["mp3"][ac.id3v2_total_size(audio["mp3"]):]
    shutil.rmtree(scratch)
    images = {f.name: f.read_bytes() for f in (args.art / "fixtures" / "images").iterdir()}
    manifest: dict[str, Any] = {}
    for set_name, pictures in SETS.items():
        for fmt in FORMATS:
            if fmt == "m4a" and set_name == "webp":
                continue  # MP4 covr cannot hold WebP; covered as a refused destination below
            name = f"{fmt}-{set_name}.{fmt}"
            specs = [{"sha256": sha(images[i]), "size": len(images[i]), "kind": kind, "type": t, "description": d, "mime": MIME[kind]}
                     for i, kind, t, d in pictures]
            if fmt in ("mp3", "aiff", "wav"):
                frames = [ac.apic_frame(MIME[kind], t, d, images[i], "utf16" if d == "back" else "latin1") for i, kind, t, d in pictures]
                tag = ac.id3v2_tag(frames)
                data = {"mp3": ac.mp3_with_id3, "aiff": ac.aiff_with_id3, "wav": ac.wav_with_id3}[fmt](audio[fmt], tag) if frames else audio[fmt]
            elif fmt == "flac":
                bodies = [ac.flac_picture_body(t, MIME[kind], d, 0, 0, 0, 0, images[i]) for i, kind, t, d in pictures]
                data = ac.flac_with_pictures(audio["flac"], bodies) if bodies else audio["flac"]
            else:
                covers = [(MP4_FLAG[kind], images[i]) for i, kind, t, d in pictures]
                data = ac.mp4_with_covers(audio["m4a"], covers) if covers else audio["m4a"]
            (out / name).write_bytes(data)
            manifest[name] = {"fmt": fmt, "set": set_name, "pictures": specs, "sha256": sha(data)}
    (out / "manifest.json").write_text(json.dumps(manifest, indent=1, sort_keys=True))
    print(f"{len(manifest)} sources")


class Cli:
    def __init__(self, args: argparse.Namespace) -> None:
        self.binary = args.binary
        self.work: Path = args.work
        self.home = self.work / "home"
        self.state = self.work / "state"
        self.home.mkdir()
        for key, value in (("libraries.tag_bridge", args.tag_bridge), ("libraries.flac", args.flac), ("libraries.flac_sha256", sha(Path(args.flac).read_bytes())),
                           ("libraries.lame", args.lame), ("libraries.lame_sha256", sha(Path(args.lame).read_bytes()))):
            self.run(["config", "set", key, json.dumps(str(value))], ok=True)

    def run(self, argv: list[str], ok: bool = False) -> dict[str, Any]:
        done = subprocess.run([self.binary, "--state-dir", str(self.state), "--json", *argv], capture_output=True, text=True,
                              env={**os.environ, "HOME": str(self.home)})
        try:
            value = json.loads(done.stdout)
        except json.JSONDecodeError:
            raise SystemExit(f"no JSON from {argv}: {done.stdout[:300]!r} {done.stderr[:300]!r}")
        if ok and value.get("status") != "ok":
            raise SystemExit(f"{argv} failed: {value}")
        return value


def pictures_of(fmt: str, data: bytes) -> list[dict[str, Any]]:
    if fmt == "mp3":
        return ac.parse_id3v2_pictures(data)
    if fmt == "aiff":
        return ac.parse_riff_id3_pictures(data, True)
    if fmt == "wav":
        return ac.parse_riff_id3_pictures(data, False)
    if fmt == "flac":
        return ac.parse_flac_pictures(data)
    return ac.parse_mp4_covers(data)


def expect(source: dict[str, Any], dest_fmt: str, opt_in: bool) -> dict[str, Any]:
    """The documented behaviour for this source, destination and setting."""
    src = source["fmt"]
    pictures = source["pictures"]
    direct = SOURCE_OF[src] == dest_fmt or (src, dest_fmt) == ("m4a", "mp3")
    container = DEST[dest_fmt]
    if not pictures:
        return {"ok": True, "pictures": []}
    if direct:
        if src == "flac":  # FLAC to FLAC: the reference drops picture blocks; the opt-in carries them
            return {"ok": True, "pictures": pictures if opt_in else [], "paths": ["artwork_copy"] if opt_in else []}
        if src == "wav":  # WAV to WAV: the ID3 chunk TagLib saved last is the one the reference marker rewrite drops
            return {"ok": True, "pictures": pictures if opt_in else [], "paths": ["wav_tag_preservation"] if opt_in else []}
        return {"ok": True, "pictures": pictures, "paths": [], "m4a_to_mp3": src == "m4a" and dest_fmt == "mp3"}
    if container == "wav" or src == "wav":
        # The reference never moves a picture into or out of a WAV on a generic copy.
        if not opt_in:
            return {"ok": True, "pictures": [], "paths": []}
    elif not opt_in:
        return {"ok": False, "code": "artwork_conversion_unavailable"}
    if container == "m4a" and any(p["kind"] == "webp" for p in pictures):
        return {"ok": False, "code": "artwork_conversion_unavailable"}
    paths = ["artwork_copy"] + (["wav_tag_preservation"] if container == "wav" else [])
    return {"ok": True, "pictures": pictures, "paths": paths, "reencoded_type": src == "m4a"}


def check(container: str, expected: dict[str, Any], data: bytes, source: dict[str, Any], problems: list[str]) -> None:
    found = pictures_of(container, data)
    wanted = expected["pictures"]
    if len(found) != len(wanted):
        problems.append(f"{len(found)} pictures in the output, expected {len(wanted)}")
        return
    for index, (want, got) in enumerate(zip(wanted, found), 1):
        if sha(got["data"]) != want["sha256"] or len(got["data"]) != want["size"]:
            problems.append(f"picture {index}: bytes differ")
        if container == "m4a":
            if got["flag"] != MP4_FLAG[want["kind"]]:
                problems.append(f"picture {index}: covr flag {got['flag']}")
            continue
        picture_type = 3 if source["fmt"] == "m4a" else want["type"]
        description = "" if source["fmt"] == "m4a" else want["description"]
        mime = want["mime"]
        if expected.get("m4a_to_mp3"):  # the reference's direct M4A to MP3 copy: no type, no description, JPEG or "image/png"
            picture_type, description, mime = 0, "", "image/jpeg" if want["kind"] == "jpeg" else "image/png"
        for key, value in (("type", picture_type), ("mime", mime), ("description", description)):
            if got[key] != value:
                problems.append(f"picture {index}: {key} {got[key]!r} != {value!r}")


def run(args: argparse.Namespace) -> None:
    manifest = json.loads((args.sources / "manifest.json").read_text())
    args.work.mkdir()
    cli = Cli(args)
    records: list[dict[str, Any]] = []
    failures = 0
    for opt_in in (True, False):
        cli.run(["config", "set", "parity.allow_non_identical", "true" if opt_in else "false"], ok=True)
        for name, source in sorted(manifest.items()):
            for dest_fmt, container in DEST.items():
                if source["fmt"] == "m4a" and dest_fmt == "alac" and source["set"] == "webp":
                    continue
                expected = expect(source, dest_fmt, opt_in)
                out_dir = args.work / f"out-{len(records):04d}"
                out_dir.mkdir()
                outcome = cli.run(["process", str(args.sources / name), "--format", dest_fmt, "--output-dir", str(out_dir)])
                result = outcome["results"][0]
                problems: list[str] = []
                record: dict[str, Any] = {"source": name, "format": dest_fmt, "opt_in": opt_in, "status": result["status"]}
                if result["status"] == "ok":
                    report = result["report"]
                    path = Path(report["output_path"])
                    data = path.read_bytes()
                    record["non_identical_paths"] = report["non_identical_paths"]
                    record["output_pictures"] = len(pictures_of(container, data))
                    if not expected["ok"]:
                        problems.append(f"succeeded, expected {expected['code']}")
                    else:
                        check(container, expected, data, source, problems)
                        if sorted(report["non_identical_paths"]) != sorted(expected.get("paths", [])) and source["pictures"]:
                            problems.append(f"non_identical_paths {report['non_identical_paths']} != {expected.get('paths')}")
                        if container == "wav":
                            chunks = [chunk_id for chunk_id, _, _ in ac.riff_chunks(data, False)]
                            if b"PLTN" not in chunks or b"fmt " not in chunks or b"data" not in chunks:
                                problems.append(f"WAV chunks {chunks}")
                else:
                    code = result["error"]["code"]
                    record["error_code"] = code
                    if expected["ok"]:
                        problems.append(f"failed with {code}: {result['error'].get('message')}")
                    elif code != expected["code"]:
                        problems.append(f"failed with {code}, expected {expected['code']}")
                    if list(out_dir.iterdir()):
                        problems.append("a failed item left files behind")
                record["problems"] = problems
                failures += bool(problems)
                records.append(record)
                if problems:
                    print(json.dumps(record), flush=True)
                shutil.rmtree(out_dir) if not problems else None
    report = {"binary": str(args.binary), "tag_bridge_sha256": sha(Path(args.tag_bridge).read_bytes()),
              "machine": os.uname().machine, "runs": len(records), "failures": failures, "records": records}
    with args.out.open("x") as handle:
        json.dump(report, handle, indent=1, sort_keys=True)
    print(f"{len(records)} runs, {failures} failures")
    if failures:
        raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    g = sub.add_parser("gen")
    g.add_argument("--art", type=Path, required=True)
    g.add_argument("--out", type=Path, required=True)
    r = sub.add_parser("run")
    r.add_argument("--sources", type=Path, required=True)
    r.add_argument("--binary", type=Path, required=True)
    r.add_argument("--tag-bridge", required=True)
    r.add_argument("--flac", required=True)
    r.add_argument("--lame", required=True)
    r.add_argument("--work", type=Path, required=True)
    r.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    gen(args) if args.command == "gen" else run(args)


if __name__ == "__main__":
    main()
