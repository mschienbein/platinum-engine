#!/usr/bin/env python3
"""ID3v2 size-limit cells of the portable bridge's opt-in artwork copy (PN_TAG_COPY_ARTWORK_BYTES).

An ID3v2 tag size is a 28-bit synchsafe integer (frames plus padding, not the 10-byte header). TagLib 1.11.1 writes it
modulo 2^28, so a destination tag above 268435455 bytes is corrupt. v08 counted only 64 bytes + the picture bytes per
picture; v09 renders the destination tag in memory (pictures, descriptions in the encoding they are written in, frame
headers, every other frame already in the destination, TagLib's padding) and, after the marker frame is set, once more.

Cells (generated here, stdlib only, deleted after each run): a source with pictures, a destination MP3 / AIFF / WAV, and a
picture size chosen so that the rendered tag lands exactly on the limit:

  fit        the tag, marker frame and padding together are exactly 268435455: status 0, the output re-reads
  post_over  one byte more, only because of the marker frame: refused after the serialize callback, nothing written
  post_edge  the pictures and padding alone are exactly 268435455, the marker frame pushes over: refused likewise
  pre_over   the pictures alone are one byte over: refused before the serialize callback, nothing written

Configurations: sixteen near-maximal FLAC pictures (the review finding), one huge ID3 picture, UTF-16 descriptions (twice
as wide as the descriptions v08 ignored), and a destination that already holds a 200 MB non-picture frame.

  art_limits.py --root ART --bridge LIB [--old LIB] --work NEW_DIR --out RESULT.json [--configs ...] [--dests ...]

ART is an `art_matrix.py gen` tree (bases and a PNG). `--old` runs the over-limit cells through the v08 bridge as
evidence only (no verdict): it accepts them and writes a wrapped size field.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import art_containers as ac  # noqa: E402
import art_matrix as am  # noqa: E402

LIMIT = 0x0FFFFFFF
PAD = 1024
FLAC_BLOCK = 0xFFFFFF
MIME = "image/png"
FMT = am.FMT
EXT = am.EXT
MIB = 1 << 20

POSITIONS = ("fit", "post_over", "post_edge", "pre_over")


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# (family of the source container, picture count, data size of all but the last picture or None for "fill", description,
#  description encoding, size of a non-picture PRIV frame the destination already holds)
CONFIGS: dict[str, dict[str, Any]] = {
    "flac16": {"src": "flac", "count": 16, "fixed": "flac-max", "description": lambda i: f"cover {i + 1}", "encoding": "latin1", "priv": 0},
    "id3-single": {"src": "mp3", "count": 1, "fixed": None, "description": lambda i: "one huge picture", "encoding": "latin1", "priv": 0},
    "id3-utf16": {"src": "mp3", "count": 8, "fixed": 32 * MIB, "description": lambda i: ("\u8868\u7d19\u306e\u8aac\u660e " * 120_000)[:1_000_000 - i], "encoding": "utf16", "priv": 0},
    "dest-priv": {"src": "flac", "count": 8, "fixed": 8 * MIB, "description": lambda i: f"cover {i + 1}", "encoding": "latin1", "priv": 200_000_000},
}


class Cells:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.root: Path = args.root
        self.work: Path = args.work
        self.png = (self.root / "fixtures/images/png-1x1").read_bytes()
        self.flac_base = (self.root / "fixtures/sources/flac-noart.flac").read_bytes()
        self.bases = {fmt: (self.root / "fixtures/bases" / f"base.{EXT[fmt]}").read_bytes() for fmt in ("mp3", "aiff", "wav")}
        self.parity = am.load_parity()
        self.new = am.ExtBridge(self.parity, args.bridge)
        self.old = am.ExtBridge(self.parity, args.old) if args.old else None
        self.audio = {fmt: self.audio_of(fmt, data) for fmt, data in self.bases.items()}
        self.counter = 0

    @staticmethod
    def audio_of(fmt: str, data: bytes) -> bytes:
        return ac.mp3_audio(data) if fmt == "mp3" else ac.riff_audio(data, fmt == "aiff")

    # -- fixtures ---------------------------------------------------------------------------------------------

    def picture(self, size: int) -> bytes:
        """A complete PNG (the bridge validates structure only) followed by `size - len(png)` zero bytes."""
        if size < len(self.png):
            raise ValueError(f"picture size {size} below the {len(self.png)}-byte PNG")
        return self.png + bytes(size - len(self.png))

    def description(self, config: dict[str, Any], index: int) -> str:
        return config["description"](index)

    def frame_overhead(self, config: dict[str, Any], index: int) -> int:
        """Bytes an APIC frame costs on top of its picture bytes as the bridge renders it."""
        return len(ac.apic_frame(MIME, 3, self.description(config, index), b"", config["encoding"]))

    def fixed_size(self, config: dict[str, Any], index: int) -> int:
        if config["fixed"] == "flac-max":
            return FLAC_BLOCK - 32 - len(MIME) - len(self.description(config, index).encode("utf-8"))
        return int(config["fixed"])

    def source_bytes(self, config: dict[str, Any], sizes: list[int], dest_fmt: str) -> tuple[bytes, str, list[dict[str, Any]]]:
        """(source file bytes, source format, expected pictures)."""
        pictures = []
        for index, size in enumerate(sizes):
            data = self.picture(size)
            pictures.append({"data": data, "type": 3, "mime": MIME, "description": self.description(config, index), "sha256": sha(data), "size": size})
        if config["src"] == "flac":
            bodies = [ac.flac_picture_body(p["type"], p["mime"], p["description"], 0, 0, 0, 0, p["data"]) for p in pictures]
            data = ac.flac_with_pictures(self.flac_base, bodies) if bodies else self.flac_base
            return data, "flac", pictures
        frames = am.id3_tag_frames() + [ac.apic_frame(p["mime"], p["type"], p["description"], p["data"], config["encoding"]) for p in pictures]
        return ac.mp3_with_id3(self.bases["mp3"], ac.id3v2_tag(frames)), "mp3", pictures

    def dest_bytes(self, fmt: str, priv: int) -> bytes:
        base = self.bases[fmt]
        if not priv:
            return base
        tag = ac.id3v2_tag([ac.id3_frame("PRIV", b"platinum-test\0" + bytes(priv))])
        return {"mp3": ac.mp3_with_id3, "aiff": ac.aiff_with_id3, "wav": ac.wav_with_id3}[fmt](base, tag)

    # -- measurements -------------------------------------------------------------------------------------------

    @staticmethod
    def tag_field(fmt: str, data: bytes) -> int | None:
        """The synchsafe size field of the output's ID3v2 tag."""
        if fmt == "mp3":
            return ac.unsyncsafe(data[6:10]) if data[:3] == b"ID3" else None
        for chunk_id, offset, _size in ac.riff_chunks(data, fmt == "aiff"):
            if chunk_id.lower() == b"id3 ":
                return ac.unsyncsafe(data[offset + 6:offset + 10]) if data[offset:offset + 3] == b"ID3" else None
        return None

    def execute(self, bridge: am.ExtBridge, config: dict[str, Any], sizes: list[int], dest_fmt: str, name: str,
                marker: bytes | None = am.MARKER) -> dict[str, Any]:
        self.counter += 1
        source, src_fmt, pictures = self.source_bytes(config, sizes, dest_fmt)
        src = self.work / f"{self.counter:03d}-{name}-src.{EXT[src_fmt]}"
        dst = self.work / f"{self.counter:03d}-{name}-dst.{EXT[dest_fmt]}"
        src.write_bytes(source)
        del source
        initial = self.dest_bytes(dest_fmt, config["priv"])
        with dst.open("xb") as handle:
            handle.write(initial)
        started = time.time()
        outcome = bridge.generic("new_ex", 1, FMT[src_fmt], FMT[dest_fmt], src, dst, "keep", marker)
        outcome["elapsed_s"] = round(time.time() - started, 1)
        output = dst.read_bytes()
        outcome.update({"dst_unchanged": output == initial, "output_size": len(output), "source_picture_bytes": sum(sizes),
                        "tag_field": self.tag_field(dest_fmt, output) if outcome["status"] == 0 else None})
        outcome["_output"] = output
        outcome["_pictures"] = pictures
        outcome["_initial"] = initial
        for path in (src, dst):
            path.unlink()
        return outcome

    def noart(self, config: dict[str, Any], dest_fmt: str, marker: bytes | None) -> int:
        """Content bytes (frames, no padding) the bridge writes into this destination for a picture-less source."""
        out = self.execute(self.new, config, [], dest_fmt, f"noart-{'m' if marker is not None else 'n'}", marker)
        if out["status"] != 0 or out["tag_field"] is None:
            raise SystemExit(f"calibration copy failed: {out['status']} {out['error']}")
        return out["tag_field"] - PAD

    # -- cells --------------------------------------------------------------------------------------------------

    def sizes_for(self, config: dict[str, Any], target_pre_marker: int, c_nomarker: int) -> list[int]:
        """Picture sizes whose rendered tag, before the marker frame, has exactly `target_pre_marker` content bytes."""
        count = config["count"]
        sizes = [self.fixed_size(config, i) for i in range(count - 1)] if config["fixed"] is not None else []
        used = sum(self.frame_overhead(config, i) + s for i, s in enumerate(sizes))
        last = target_pre_marker - c_nomarker - used - self.frame_overhead(config, count - 1)
        if last < len(self.png):
            raise SystemExit(f"configuration too large: the last picture would be {last} bytes")
        return [*sizes, last]

    def verify_readback(self, dest_fmt: str, path: Path, pictures: list[dict[str, Any]], problems: list[str]) -> dict[str, Any]:
        status, error, text = self.new.base.inspect(FMT[dest_fmt], path)
        facts: dict[str, Any] = {"inspect_status": status}
        if status != 0 or not text:
            problems.append(f"TagLib inspection failed: {status} {error}")
            return facts
        art = [a for a in json.loads(text)["artwork"] if a["container"] == "id3v2"]
        facts["taglib_pictures"] = len(art)
        if len(art) != len(pictures):
            problems.append(f"TagLib sees {len(art)} pictures, expected {len(pictures)}")
        else:
            for index, (want, got) in enumerate(zip(pictures, art), 1):
                if got["sha256"] != want["sha256"] or got["byte_count"] != want["size"]:
                    problems.append(f"TagLib picture {index}: bytes differ")
        return facts

    def run(self) -> dict[str, Any]:
        args = self.args
        results: list[dict[str, Any]] = []
        calibration: dict[str, Any] = {}
        for config_name in args.configs:
            config = CONFIGS[config_name]
            for dest_fmt in args.dests:
                key = f"{config_name}/{dest_fmt}"
                c_no = self.noart(config, dest_fmt, None)
                c_with = self.noart(config, dest_fmt, am.MARKER)
                marker_frame = c_with - c_no
                # Model check: a small copy renders exactly as predicted.
                probe_sizes = [len(self.png) + 100 * (i + 1) for i in range(config["count"])] if config["count"] < 9 else [len(self.png) + 100] * config["count"]
                probe = self.execute(self.new, config, probe_sizes, dest_fmt, "probe")
                predicted = c_with + sum(self.frame_overhead(config, i) + s for i, s in enumerate(probe_sizes))
                measured = probe["tag_field"] - PAD if probe["tag_field"] is not None else None
                calibration[key] = {"content_without_pictures_nomarker": c_no, "content_without_pictures": c_with, "marker_frame_bytes": marker_frame,
                                    "probe_predicted": predicted, "probe_measured": measured}
                if measured != predicted:
                    raise SystemExit(f"model mismatch for {key}: predicted {predicted}, measured {measured}")
                del probe
                for position in args.positions:
                    if marker_frame == 0 and position in ("post_over", "post_edge"):
                        continue  # a WAV carries the marker as a PLTN chunk, not as an ID3 frame: these coincide with fit / pre_over
                    pre_marker = {"fit": LIMIT - PAD - marker_frame, "post_over": LIMIT - PAD - marker_frame + 1,
                                  "post_edge": LIMIT - PAD, "pre_over": LIMIT - PAD + 1}[position]
                    total = pre_marker + marker_frame
                    pre_fits, fits = pre_marker + PAD <= LIMIT, total + PAD <= LIMIT
                    sizes = self.sizes_for(config, pre_marker, c_no)
                    case_id = f"{config_name}-{position}-to-{dest_fmt}"
                    problems: list[str] = []
                    out = self.execute(self.new, config, sizes, dest_fmt, case_id)
                    record: dict[str, Any] = {"id": case_id, "config": config_name, "dest": dest_fmt, "position": position,
                                              "pictures": len(sizes), "picture_bytes": sum(sizes), "pre_marker_content": pre_marker,
                                              "rendered_size_field": total + PAD, "expect": "success" if fits else "status 2", "bridge": "v09"}
                    serialize_calls = (1 if marker_frame else 0) if pre_fits else 0
                    if fits:
                        if out["status"] != 0:
                            problems.append(f"status {out['status']} ({out['error']}) instead of 0")
                        else:
                            if out["result_flags"] != 1:
                                problems.append(f"result_flags {out['result_flags']}")
                            if out["tag_field"] != LIMIT:
                                problems.append(f"tag size field {out['tag_field']} != {LIMIT}")
                            if out["callbacks"] != {"serialize": serialize_calls, "rewrite": 1 if dest_fmt == "wav" else 0}:
                                problems.append(f"callbacks {out['callbacks']}")
                            output = out["_output"]
                            pictures = out["_pictures"]
                            if dest_fmt == "mp3":
                                parsed = ac.parse_id3v2_pictures(output)
                                audio = ac.mp3_audio(output)
                            else:
                                parsed = ac.parse_riff_id3_pictures(output, dest_fmt == "aiff")
                                audio = ac.riff_audio(output, dest_fmt == "aiff")
                            if len(parsed) != len(pictures):
                                problems.append(f"independent parser sees {len(parsed)} pictures, expected {len(pictures)}")
                            else:
                                for index, (want, got) in enumerate(zip(pictures, parsed), 1):
                                    if sha(got["data"]) != want["sha256"] or len(got["data"]) != want["size"]:
                                        problems.append(f"picture {index}: bytes differ")
                                    for k in ("type", "mime", "description"):
                                        if got[k] != want[k]:
                                            problems.append(f"picture {index}: {k} {got[k]!r} != {want[k]!r}")
                            if audio != self.audio[dest_fmt]:
                                problems.append("audio payload changed")
                            record["parser_pictures"] = len(parsed)
                            # TagLib re-reads the file (inspection) and the marker survives (a WAV's PLTN chunk is padded to even).
                            path = self.work / f"{case_id}-reread.{EXT[dest_fmt]}"
                            path.write_bytes(output)
                            record.update(self.verify_readback(dest_fmt, path, pictures, problems))
                            marker_result = self.new.base.marker(FMT[dest_fmt], path)
                            record["marker_readback"] = [marker_result[0], marker_result[2], marker_result[3].hex() if marker_result[3] is not None else None]
                            expected_marker = am.MARKER + (b"\0" if dest_fmt == "wav" and len(am.MARKER) & 1 else b"")
                            if marker_result[0] != 0 or marker_result[3] != expected_marker:
                                problems.append(f"marker read back as {marker_result}")
                            path.unlink()
                            del output, parsed
                    else:
                        if out["status"] != 2:
                            problems.append(f"status {out['status']} ({out['error']}) instead of 2")
                        if "ID3v2 tag size limit" not in out["error"]:
                            problems.append(f"error text {out['error']!r}")
                        if out["result_flags"] != 0:
                            problems.append(f"result_flags {out['result_flags']} on refusal")
                        if not out["dst_unchanged"]:
                            problems.append("destination changed on refusal")
                        if out["callbacks"] != {"serialize": serialize_calls, "rewrite": 0}:
                            problems.append(f"callbacks {out['callbacks']}, expected serialize {serialize_calls}")
                    for private in ("_output", "_pictures", "_initial"):
                        out.pop(private, None)
                    record.update({k: v for k, v in out.items() if k not in ("callback_errors",)})
                    record["problems"] = problems
                    record["pass"] = not problems
                    results.append(record)
                    print(f"{case_id}: status {out['status']} callbacks {out['callbacks']} {'ok' if not problems else problems}", flush=True)
                    del out
                    # Evidence: what the previous bridge (v08) did with the same over-limit cell. It counted 64 + picture bytes only.
                    if self.old is not None and not fits:
                        old = self.execute(self.old, config, sizes, dest_fmt, f"{case_id}-v08")
                        old_record = {"id": f"{case_id}-v08", "bridge": "v08", "config": config_name, "dest": dest_fmt, "position": position,
                                      "status": old["status"], "error": old["error"], "callbacks": old["callbacks"],
                                      "tag_field": old["tag_field"], "rendered_size_field": total + PAD,
                                      "tag_field_wrapped_modulo_2_28": old["tag_field"] == (total + PAD) % (1 << 28) if old["tag_field"] is not None else None,
                                      "output_size": old["output_size"]}
                        results.append(old_record)
                        print(f"{old_record['id']}: status {old['status']} tag_field {old['tag_field']} (true size field {old_record['rendered_size_field']})", flush=True)
                        del old
        failures = [r["id"] for r in results if r.get("bridge") == "v09" and not r["pass"]]
        return {"bridge_sha256": sha(args.bridge.read_bytes()), "old_bridge_sha256": sha(args.old.read_bytes()) if args.old else None,
                "machine": os.uname().machine, "limit": LIMIT, "padding": PAD, "calibration": calibration,
                "cells": sum(1 for r in results if r.get("bridge") == "v09"), "failures": failures, "results": results}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True, help="art_matrix.py gen tree")
    parser.add_argument("--bridge", type=Path, required=True)
    parser.add_argument("--old", type=Path)
    parser.add_argument("--work", type=Path, required=True, help="new directory for the large temporary files")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--configs", nargs="+", default=list(CONFIGS), choices=list(CONFIGS))
    parser.add_argument("--dests", nargs="+", default=["mp3", "aiff", "wav"], choices=["mp3", "aiff", "wav"])
    parser.add_argument("--positions", nargs="+", default=list(POSITIONS), choices=list(POSITIONS))
    args = parser.parse_args()
    args.work.mkdir()
    try:
        report = Cells(args).run()
    finally:
        shutil.rmtree(args.work, ignore_errors=True)
    with args.out.open("x") as handle:
        json.dump(report, handle, indent=1, sort_keys=True)
    print(f"{report['cells']} cells on {report['machine']}: {len(report['failures'])} failures {report['failures']}")
    if report["failures"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
