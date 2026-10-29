#!/usr/bin/env python3
"""record_readsizes.py - record what AudioToolbox delivers for read sizes other than 512, for the MP3 golden fixtures.

Repo location : tools/portable/mp3/record_readsizes.py
Output        : tests/fixtures/mp3-exact/readsizes.json   (checked by tests/portable_mp3.rs)
Python        : >= 3.8, stdlib only, deterministic (no timestamps; files in manifest order).
Needs         : macOS arm64 and the `apple_probe` binary built from tools/portable/mp3/apple_probe (RECORD only).

The golden manifest stores Apple's results for 512-frame reads. Apple's result depends on the read size when a read
fails (the frames the failed read had gathered are lost; the packet-group reads of the converter shift), so this file
records the same fixtures at three more sizes, through the engine's own `apple_codec::Reader` via `apple_probe`,
exactly as the engine reads: `apple_probe dump FILE OUT planar|inter BLOCK` (planar = two planes at the source rate,
mono duplicated; inter = interleaved at the source channel count; reads of BLOCK frames until a read returns 0 or fails;
the frames of a failed read are not delivered).

    RECORD   record_readsizes.py --probe PATH/apple_probe --pack PACK --manifest MANIFEST --out readsizes.json
    VERIFY   record_readsizes.py --verify readsizes.json --pack PACK --manifest MANIFEST

RECORD first re-reads every fixture with BLOCK 512 and requires the result to equal the manifest's `planar512` /
`inter512` record (that proves the procedure reproduces the stored reference), then records the other sizes. Selected
files: every fixture with a read error, every `family:stream_change` fixture, every `blocksize_sensitive` fixture, and
every 9th remaining fixture (clean streams, for contrast). VERIFY checks the file against the pack and manifest without
Apple. Every inconsistency is fatal (exit status 1).
"""
import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile

SCHEMA = "platinum-mp3-exact-readsizes-v1"
GENERATOR = "tools/portable/mp3/record_readsizes.py"
SIZES = (100, 513, 4096)
CONTROL = 512
SHAPES = (("planar", "ExtAudioFileRead(planar stereo)"), ("inter", "ExtAudioFileRead"))
EVERY = 9
_ERR = re.compile(r"^(.*?) failed with OSStatus (-?\d+)(?: \(0x[0-9a-fA-F]+\))?$")
_HEX64 = re.compile(r"^[0-9a-f]{64}$")
EMPTY = hashlib.sha256(b"").hexdigest()


class Fatal(Exception):
    pass


def fail(msg):
    raise Fatal(msg)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def load(path):
    with open(path, "rb") as f:
        return json.loads(f.read().decode("utf-8"))


def status_i32(n):
    n &= 0xFFFFFFFF
    return n - (1 << 32) if n & 0x80000000 else n


def read_files(pack_path, manifest_path):
    with open(pack_path, "rb") as f:
        pack = f.read()
    manifest = load(manifest_path)
    if manifest.get("schema") != "platinum-mp3-exact-fixtures-v1":
        fail("manifest schema %r" % manifest.get("schema"))
    if sha(pack) != manifest["pack"]["sha256"]:
        fail("pack.bin does not match the manifest")
    return pack, manifest


def selected(manifest):
    out = []
    clean = 0
    for e in manifest["files"]:
        if not e["open"]["ok"]:
            continue
        has_error = e["planar512"]["error"] is not None or e["inter512"]["error"] is not None
        sensitive = "blocksize_sensitive" in e["features"] or e["family"] == "stream_change"
        if has_error or sensitive:
            out.append(e["id"])
        else:
            if clean % EVERY == 0:
                out.append(e["id"])
            clean += 1
    return out


def run_probe(probe, path, shape, block, workdir):
    out = os.path.join(workdir, "out-%s-%d.f32" % (shape, block))
    proc = subprocess.run([probe, "dump", path, out, shape, str(block)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=300)
    if proc.returncode != 0:
        fail("apple_probe %s %d on %s exited %d: %s" % (shape, block, path, proc.returncode, proc.stderr.decode(errors="replace")))
    line = proc.stdout.decode("utf-8").strip().splitlines()[-1]
    j = json.loads(line)
    if j.get("stage") == "open":
        fail("%s: apple_probe could not open the file: %s" % (path, j.get("error")))
    with open(out, "rb") as f:
        data = f.read()
    os.remove(out)
    return j, data


def record_one(probe, entry, data, blocks):
    result = {"id": entry["id"], "sha256": entry["sha256"]}
    with tempfile.TemporaryDirectory(prefix="mp3-readsizes-") as workdir:
        path = os.path.join(workdir, "input.mp3")
        with open(path, "wb") as f:
            f.write(data)
        channels = entry["info"]["channels"]
        for shape, op in SHAPES:
            rows = {}
            for block in blocks:
                j, raw = run_probe(probe, path, shape, block, workdir)
                frames = j["frames"]
                per_frame = 8 if shape == "planar" else 4 * channels
                if len(raw) != frames * per_frame:
                    fail("%s %s %d: %d bytes written for %d frames" % (entry["id"], shape, block, len(raw), frames))
                if j["error"] is None:
                    if j["ok"] is not True:
                        fail("%s %s %d: ok=false without an error" % (entry["id"], shape, block))
                    err = None
                else:
                    m = _ERR.match(j["error"])
                    if not m or m.group(1) != op:
                        fail("%s %s %d: unexpected error text %r" % (entry["id"], shape, block, j["error"]))
                    err = {"operation": op, "status": status_i32(int(m.group(2)))}
                rows[block] = {"frames": frames, "error": err, "sha256": sha(raw)}
            result[shape] = rows
    return result


def control_matches(entry, rec):
    for shape, key in (("planar", "planar512"), ("inter", "inter512")):
        if rec[shape][CONTROL] != entry[key]:
            fail("%s: %s at %d frames is %r but the manifest records %r" % (entry["id"], shape, CONTROL, rec[shape][CONTROL], entry[key]))


def record(args):
    pack, manifest = read_files(args.pack, args.manifest)
    by_id = {e["id"]: e for e in manifest["files"]}
    ids = selected(manifest)

    def work(fid):
        e = by_id[fid]
        data = pack[e["offset"]:e["offset"] + e["length"]]
        rec = record_one(args.probe, e, data, (CONTROL,) + SIZES)
        control_matches(e, rec)
        for shape, _ in SHAPES:
            del rec[shape][CONTROL]
        return rec

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        files = list(pool.map(work, ids))     # manifest order
    doc = {
        "schema": SCHEMA, "generator": GENERATOR,
        "apple": {k: manifest["apple"][k] for k in ("host", "tool", "probe_sha256")},
        "pack_sha256": manifest["pack"]["sha256"], "block_sizes": list(SIZES), "files": files,
    }
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(dumps(doc))
    print("wrote %s: %d files x %d sizes x 2 shapes; the 512-frame control matched the manifest for every file" % (args.out, len(files), len(SIZES)))


def dumps(doc):
    # one line per file keeps the diff small; stable key order
    head = {k: v for k, v in doc.items() if k != "files"}
    text = json.dumps(head, indent=1, ensure_ascii=True)
    assert text.endswith("}")
    lines = [json.dumps(f, separators=(",", ":"), ensure_ascii=True) for f in doc["files"]]
    return text[:-2] + ',\n "files": [\n  ' + ",\n  ".join(lines) + "\n ]\n}\n"


def verify(args):
    _, manifest = read_files(args.pack, args.manifest)
    by_id = {e["id"]: e for e in manifest["files"]}
    doc = load(args.verify)
    if doc.get("schema") != SCHEMA or doc.get("generator") != GENERATOR:
        fail("schema or generator mismatch")
    if doc["pack_sha256"] != manifest["pack"]["sha256"]:
        fail("recorded against a different pack")
    if doc["block_sizes"] != list(SIZES):
        fail("block sizes %r" % doc["block_sizes"])
    if doc["apple"] != {k: manifest["apple"][k] for k in ("host", "tool", "probe_sha256")}:
        fail("apple provenance differs from the manifest")
    want = selected(manifest)
    got = [f["id"] for f in doc["files"]]
    if got != want:
        fail("selection differs from the rule: %d recorded, %d selected" % (len(got), len(want)))
    for f in doc["files"]:
        e = by_id[f["id"]]
        if f["sha256"] != e["sha256"]:
            fail("%s: input sha256" % f["id"])
        if list(f) != ["id", "sha256", "planar", "inter"]:
            fail("%s: keys %r" % (f["id"], list(f)))
        for shape, op in SHAPES:
            if list(f[shape]) != [str(b) for b in SIZES]:
                fail("%s: %s sizes %r" % (f["id"], shape, list(f[shape])))
            for block, r in f[shape].items():
                if list(r) != ["frames", "error", "sha256"] or not _HEX64.match(r["sha256"]):
                    fail("%s %s %s: malformed" % (f["id"], shape, block))
                if r["error"] is not None and (list(r["error"]) != ["operation", "status"] or r["error"]["operation"] != op):
                    fail("%s %s %s: malformed error" % (f["id"], shape, block))
                if r["frames"] == 0 and r["sha256"] != EMPTY:
                    fail("%s %s %s: zero frames but not the empty hash" % (f["id"], shape, block))
    print("OK: %d files, sizes %s, consistent with the pack and manifest" % (len(doc["files"]), list(SIZES)))


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--probe")
    ap.add_argument("--pack", required=True)
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--out")
    ap.add_argument("--verify")
    ap.add_argument("--workers", type=int, default=6)
    args = ap.parse_args(argv)
    try:
        if args.verify:
            verify(args)
        elif args.probe and args.out:
            record(args)
        else:
            ap.error("give --verify FILE, or --probe and --out")
    except Fatal as e:
        print("record_readsizes: %s" % e, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
