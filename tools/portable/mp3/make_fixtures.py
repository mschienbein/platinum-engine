#!/usr/bin/env python3
"""make_fixtures.py - rebuild / verify the Platinum MP3 exact-reader golden fixtures.

Repo location : tools/portable/mp3/make_fixtures.py
Fixtures      : tests/fixtures/mp3-exact/{pack.bin,manifest.json}
Python        : >= 3.8, stdlib only, deterministic (no timestamps, no randomness, files sorted by id).

What it does
------------
REBUILD (needs the three corpora and their Apple reference dumps)

    make_fixtures.py --corpus DIR --corpus-ref DIR --fuzz DIR --fuzz-ref DIR \\
                     --held DIR --held-ref DIR \\
                     --manifest tests/fixtures/mp3-exact/manifest.json --out OUTDIR [--update]

  Reads the id list (and the per-file ``features`` tokens, which cannot be re-derived without the
  coverage tools) from the stored MANIFEST, re-reads every input file and every ``reference.json``,
  re-derives *every other* field (offsets, lengths, sha256, Apple open/info/planar512/inter512
  facts, source hashes), verifies all of it and writes OUTDIR/pack.bin + OUTDIR/manifest.json.
  If anything re-derived differs from the stored manifest nothing is written and the exit status
  is non-zero, unless --update is given (then the new manifest replaces the stored one in OUTDIR).

VERIFY A PACK (needs no corpora)

    make_fixtures.py --verify-pack PACK MANIFEST

  Checks the schema, that PACK is exactly the concatenation described by MANIFEST (offsets
  contiguous from 0, lengths, per-file sha256, total size and sha256) and that the Apple facts are
  internally consistent (refusal/read-error operation names and status codes, frames, hashes).

Every inconsistency is fatal (exit status 1, message on stderr).  Nothing is ever repaired silently.

Facts are never invented: every value in a manifest entry is copied from the stored Apple reference
(``<set>-ref/reference.json`` produced by ``apple_probe``) or computed from the input bytes.
"""
import argparse
import gzip
import hashlib
import json
import os
import re
import sys

SCHEMA = "platinum-mp3-exact-fixtures-v1"
GENERATOR = "tools/portable/mp3/make_fixtures.py"
BLOCK_FRAMES = 512
SETS = ("corpus", "fuzz", "held")
OPEN_OPS = ("ExtAudioFileOpenURL", "ExtAudioFileSetProperty(client format)")
PLANAR_READ_OP = "ExtAudioFileRead(planar stereo)"
INTER_READ_OP = "ExtAudioFileRead"
MAX_FEATURES = 8
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
_ERR_RE = re.compile(r"^(.*?) failed with OSStatus (-?\d+)(?: \(0x[0-9a-fA-F]+\))?$")
_HEX64 = re.compile(r"^[0-9a-f]{64}$")


class FixtureError(Exception):
    pass


def fail(msg):
    raise FixtureError(msg)


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def to_i32(n):
    """OSStatus decimal (signed or unsigned text) -> signed 32-bit integer; refuses out-of-range values."""
    if not -(1 << 31) <= n < (1 << 32):
        fail("OSStatus %d does not fit 32 bits" % n)
    n &= 0xFFFFFFFF
    return n - (1 << 32) if n & 0x80000000 else n


def parse_os_error(text, what):
    """'<operation> failed with OSStatus N (0x...)' -> (operation, signed int status)."""
    if not isinstance(text, str):
        fail("%s: error text is not a string: %r" % (what, text))
    m = _ERR_RE.match(text)
    if not m:
        fail("%s: cannot parse OSStatus error text: %r" % (what, text))
    return m.group(1), to_i32(int(m.group(2)))


def hex_status_matches(text, status):
    m = re.search(r"\(0x([0-9a-fA-F]+)\)", text)
    return m is None or (int(m.group(1), 16) & 0xFFFFFFFF) == (status & 0xFFFFFFFF)


def load_json(path):
    with open(path, "rb") as f:
        return json.loads(f.read().decode("utf-8"))


def dumps_manifest(manifest):
    return json.dumps(manifest, indent=1, sort_keys=False, ensure_ascii=True) + "\n"


# --------------------------------------------------------------------------------------------
# source sets
# --------------------------------------------------------------------------------------------
class SourceSet:
    """One corpus (input dir + Apple reference dir + reference.json)."""

    def __init__(self, name, indir, refdir):
        self.name = name
        self.indir = indir
        self.refdir = refdir
        ref_path = os.path.join(refdir, "reference.json")
        if not os.path.isfile(ref_path):
            fail("%s: missing %s" % (name, ref_path))
        self.reference_sha256 = sha256_file(ref_path)
        self.reference = load_json(ref_path)
        if not isinstance(self.reference.get("files"), dict):
            fail("%s: reference.json has no 'files' object" % name)
        for key in ("host", "tool", "probe_sha256"):
            if not isinstance(self.reference.get(key), str):
                fail("%s: reference.json lacks string key %r" % (name, key))
        man_path = os.path.join(indir, "manifest.json")
        if os.path.isfile(man_path):
            self.manifest_sha256 = sha256_file(man_path)
            self._root_manifest = load_json(man_path)
        else:
            self.manifest_sha256 = None
            self._root_manifest = None
        self._root_sha = None
        self._fam_manifests = {}

    def root_manifest_sha(self, family, name):
        """sha256 recorded by the generator's own manifest for this file, or None when no manifest lists it."""
        if self._root_manifest is not None:
            if self._root_sha is None:
                self._root_sha = {}
                for e in self._root_manifest.get("files", []):
                    if isinstance(e, dict) and "sha256" in e:
                        key = e.get("id") or e.get("file") or e.get("name")
                        if key is not None:
                            self._root_sha[key] = e["sha256"]
            for key in ("%s/%s" % (family, name), "%s/%s.mp3" % (family, name)):
                if key in self._root_sha:
                    return self._root_sha[key]
        # per-family manifest (fuzz / held layout): <dir>/<family>/manifest.json, files[].{name,sha256}
        if family not in self._fam_manifests:
            p = os.path.join(self.indir, family, "manifest.json")
            table = None
            if os.path.isfile(p):
                try:
                    table = {}
                    for e in load_json(p).get("files", []):
                        if isinstance(e, dict) and "name" in e and "sha256" in e:
                            table[e["name"]] = e["sha256"]
                except (ValueError, AttributeError):
                    table = None
            self._fam_manifests[family] = table
        table = self._fam_manifests[family]
        if table is not None:
            for key in (name, name + ".mp3"):
                if key in table:
                    return table[key]
        return None

    def input_path(self, family, name):
        return os.path.join(self.indir, family, name + ".mp3")


def open_sets(args):
    sets = {}
    for s in SETS:
        indir = getattr(args, s)
        refdir = getattr(args, s + "_ref")
        if not indir or not refdir:
            fail("--%s and --%s-ref are required for a rebuild" % (s, s))
        sets[s] = SourceSet(s, indir, refdir)
    apple = None
    for s in SETS:
        r = sets[s].reference
        cur = (r["host"], r["tool"], r["probe_sha256"])
        if apple is None:
            apple = cur
        elif apple != cur:
            fail("reference.json of %s was produced by a different Apple probe: %r != %r" % (s, cur, apple))
    return sets, apple


# --------------------------------------------------------------------------------------------
# per-file derivation
# --------------------------------------------------------------------------------------------
def split_id(fid):
    parts = fid.split("/")
    if len(parts) != 3 or parts[0] not in SETS or not all(parts):
        fail("bad fixture id %r (want <set>/<family>/<name>)" % fid)
    return parts


def _int(v, what):
    if isinstance(v, bool) or not isinstance(v, int):
        fail("%s is not an integer: %r" % (what, v))
    return v


def derive_entry(fid, features, sset, data):
    """Re-derive one manifest entry (without offset/length/sha256 positions) from reference + bytes."""
    _, family, name = split_id(fid)
    key = "%s/%s" % (family, name)
    ref = sset.reference["files"].get(key)
    if ref is None:
        fail("%s: not present in %s reference.json" % (fid, sset.name))
    if ref.get("file") != key + ".mp3":
        fail("%s: reference 'file' is %r, expected %r" % (fid, ref.get("file"), key + ".mp3"))
    digest = sha256_bytes(data)
    if ref.get("sha256_mp3") != digest:
        fail("%s: input sha256 %s != reference sha256_mp3 %s" % (fid, digest, ref.get("sha256_mp3")))
    man_sha = sset.root_manifest_sha(family, name)
    if man_sha is not None and man_sha != digest:
        fail("%s: input sha256 %s != generator manifest sha256 %s" % (fid, digest, man_sha))
    if ref.get("deterministic") is not True:
        fail("%s: Apple reference is not marked deterministic" % fid)
    audiofile = ref.get("audiofile") or {}
    if audiofile.get("file_bytes") is not None and audiofile["file_bytes"] != len(data):
        fail("%s: audiofile.file_bytes %r != input length %d" % (fid, audiofile["file_bytes"], len(data)))

    info = ref.get("info")
    pj = ref.get("planar512_json")
    ij = ref.get("inter512_json")
    if not isinstance(info, dict) or not isinstance(pj, dict) or not isinstance(ij, dict):
        fail("%s: reference lacks info / planar512_json / inter512_json" % fid)
    entry = {
        "id": fid, "set": sset.name, "family": family, "name": name,
        "offset": None, "length": len(data), "sha256": digest,
        "features": list(features),
    }
    open_ok = ref.get("open_ok")
    if open_ok is not (info.get("ok") is True):
        fail("%s: open_ok %r disagrees with info.ok %r" % (fid, open_ok, info.get("ok")))

    if not open_ok:
        op, status = parse_os_error(info.get("error"), fid + " info.error")
        if op not in OPEN_OPS:
            fail("%s: unexpected refusal operation %r" % (fid, op))
        for label, j in (("planar512_json", pj), ("inter512_json", ij)):
            if j.get("ok") is not False or j.get("stage") != "open":
                fail("%s: %s should be an open-stage failure: %r" % (fid, label, j))
            if j.get("error") != info["error"]:
                fail("%s: %s.error %r != info.error %r" % (fid, label, j.get("error"), info["error"]))
        if not hex_status_matches(info["error"], status):
            fail("%s: decimal and hex OSStatus disagree in %r" % (fid, info["error"]))
        for label in ("f32_sha256", "inter512_sha256", "planar4096_sha256"):
            if ref.get(label) is not None:
                fail("%s: refused file has %s=%r" % (fid, label, ref.get(label)))
        entry["open"] = {"ok": False, "operation": op, "status": status}
        return entry

    # open ok ------------------------------------------------------------------------------
    entry["open"] = {"ok": True}
    need = ("rate", "format_id", "frames_per_packet", "channels", "length_frames", "bitrate")
    for k in need:
        _int(info.get(k), "%s info.%s" % (fid, k))
    entry["info"] = {
        "sample_rate": info["rate"], "format_id": info["format_id"],
        "frames_per_packet": info["frames_per_packet"], "channels": info["channels"],
        "length_frames": info["length_frames"], "bitrate": info["bitrate"],
    }

    def read_part(label, j, op_name, sha):
        if j.get("channels") != info["channels"]:
            fail("%s: %s.channels %r != info.channels %r" % (fid, label, j.get("channels"), info["channels"]))
        frames = _int(j.get("frames"), "%s %s.frames" % (fid, label))
        err = j.get("error")
        if err is None:
            if j.get("ok") is not True:
                fail("%s: %s ok=%r without error text" % (fid, label, j.get("ok")))
            e = None
        else:
            if j.get("ok") is not False:
                fail("%s: %s has an error but ok=%r" % (fid, label, j.get("ok")))
            op, status = parse_os_error(err, "%s %s.error" % (fid, label))
            if op != op_name:
                fail("%s: %s read-error operation %r, expected %r" % (fid, label, op, op_name))
            if not hex_status_matches(err, status):
                fail("%s: decimal and hex OSStatus disagree in %r" % (fid, err))
            e = {"operation": op, "status": status}
        if not isinstance(sha, str) or not _HEX64.match(sha):
            fail("%s: %s stream sha256 missing/invalid: %r" % (fid, label, sha))
        return {"frames": frames, "error": e, "sha256": sha}

    entry["planar512"] = read_part("planar512_json", pj, PLANAR_READ_OP, ref.get("f32_sha256"))
    entry["inter512"] = read_part("inter512_json", ij, INTER_READ_OP, ref.get("inter512_sha256"))

    # the stored planar stream must hash to f32_sha256 and have the byte length the reference states
    gz_name = ref.get("ref_file")
    if gz_name:
        gz_path = os.path.join(sset.refdir, family, gz_name)
        if os.path.isfile(gz_path):
            with gzip.open(gz_path, "rb") as f:
                raw = f.read()
            if sha256_bytes(raw) != ref["f32_sha256"]:
                fail("%s: gunzip(%s) sha256 %s != f32_sha256 %s" % (fid, gz_name, sha256_bytes(raw), ref["f32_sha256"]))
            if ref.get("f32_bytes") is not None and ref["f32_bytes"] != len(raw):
                fail("%s: stored stream is %d bytes, reference says f32_bytes=%r" % (fid, len(raw), ref["f32_bytes"]))
            if len(raw) != 8 * entry["planar512"]["frames"]:
                fail("%s: stored stream has %d bytes != 8 * frames %d" % (fid, len(raw), entry["planar512"]["frames"]))
        else:
            fail("%s: reference stream %s is missing" % (fid, gz_path))
    else:
        fail("%s: open ok but the reference has no ref_file" % fid)
    if entry["planar512"]["frames"] == 0 and entry["planar512"]["sha256"] != EMPTY_SHA256:
        fail("%s: zero frames but the stream hash is not the empty-input hash" % fid)
    return entry


def build(ids_features, sets, apple):
    """ids_features: list of (id, features) -> (manifest dict, pack bytes).  Files sorted by id."""
    seen = set()
    ordered = sorted(ids_features, key=lambda t: t[0])
    chunks = []
    files = []
    offset = 0
    for fid, features in ordered:
        if fid in seen:
            fail("duplicate id %s" % fid)
        seen.add(fid)
        if len(features) > MAX_FEATURES:
            fail("%s: %d feature tokens (max %d)" % (fid, len(features), MAX_FEATURES))
        s, family, name = split_id(fid)
        sset = sets[s]
        path = sset.input_path(family, name)
        if not os.path.isfile(path):
            fail("%s: input file %s does not exist" % (fid, path))
        with open(path, "rb") as f:
            data = f.read()
        entry = derive_entry(fid, features, sset, data)
        entry["offset"] = offset
        offset += len(data)
        chunks.append(data)
        files.append(entry)
    pack = b"".join(chunks)
    manifest = {
        "schema": SCHEMA,
        "generator": GENERATOR,
        "pack": {"file": "pack.bin", "bytes": len(pack), "sha256": sha256_bytes(pack)},
        "apple": {"host": apple[0], "tool": apple[1], "probe_sha256": apple[2], "block_frames": BLOCK_FRAMES},
        "sources": {
            s: {"reference_sha256": sets[s].reference_sha256, "manifest_sha256": sets[s].manifest_sha256}
            for s in SETS
        },
        "files": files,
    }
    return manifest, pack


def write_outputs(manifest, pack, outdir):
    os.makedirs(outdir, exist_ok=True)
    with open(os.path.join(outdir, "pack.bin"), "wb") as f:
        f.write(pack)
    with open(os.path.join(outdir, "manifest.json"), "w", encoding="utf-8", newline="\n") as f:
        f.write(dumps_manifest(manifest))


# --------------------------------------------------------------------------------------------
# verification of a pack against a manifest (no corpora needed)
# --------------------------------------------------------------------------------------------
def _check_error(fid, label, e, op_name):
    if e is None:
        return
    if not isinstance(e, dict) or set(e) != {"operation", "status"}:
        fail("%s: %s.error must be null or {operation,status}: %r" % (fid, label, e))
    if e["operation"] != op_name:
        fail("%s: %s.error.operation %r != %r" % (fid, label, e["operation"], op_name))
    s = _int(e["status"], "%s %s.error.status" % (fid, label))
    if not -(1 << 31) <= s < (1 << 31):
        fail("%s: %s.error.status %d is not a signed 32-bit value" % (fid, label, s))


def verify_pack(pack_path, manifest_path):
    manifest = load_json(manifest_path)
    if manifest.get("schema") != SCHEMA:
        fail("schema is %r, expected %r" % (manifest.get("schema"), SCHEMA))
    if list(manifest) != ["schema", "generator", "pack", "apple", "sources", "files"]:
        fail("manifest top-level keys/order are %r" % list(manifest))
    if manifest["generator"] != GENERATOR:
        fail("generator is %r" % manifest["generator"])
    apple = manifest["apple"]
    if apple.get("block_frames") != BLOCK_FRAMES or not all(isinstance(apple.get(k), str) for k in ("host", "tool", "probe_sha256")):
        fail("bad apple block: %r" % apple)
    if list(manifest["sources"]) != list(SETS):
        fail("sources keys are %r" % list(manifest["sources"]))
    for s, src in manifest["sources"].items():
        if not _HEX64.match(src.get("reference_sha256", "")):
            fail("sources.%s.reference_sha256 invalid" % s)
        ms = src.get("manifest_sha256")
        if ms is not None and not _HEX64.match(ms):
            fail("sources.%s.manifest_sha256 invalid" % s)
    with open(pack_path, "rb") as f:
        pack = f.read()
    p = manifest["pack"]
    if p.get("file") != "pack.bin":
        fail("pack.file is %r" % p.get("file"))
    if p["bytes"] != len(pack):
        fail("pack.bin has %d bytes, manifest says %d" % (len(pack), p["bytes"]))
    if p["sha256"] != sha256_bytes(pack):
        fail("pack.bin sha256 %s != manifest %s" % (sha256_bytes(pack), p["sha256"]))
    files = manifest["files"]
    ids = [e["id"] for e in files]
    if ids != sorted(ids) or len(set(ids)) != len(ids):
        fail("ids are not unique and sorted")
    expected_keys = ["id", "set", "family", "name", "offset", "length", "sha256", "features", "open"]
    offset = 0
    stats = {"files": 0, "refused": 0, "read_errors": 0}
    for e in files:
        fid = e["id"]
        s, family, name = split_id(fid)
        if e["set"] != s or e["family"] != family or e["name"] != name:
            fail("%s: set/family/name fields disagree with id" % fid)
        keys = list(e)
        if e["open"].get("ok") is True:
            want = expected_keys + ["info", "planar512", "inter512"]
        else:
            want = expected_keys
        if keys != want:
            fail("%s: keys %r, expected %r" % (fid, keys, want))
        if e["offset"] != offset:
            fail("%s: offset %d, expected contiguous %d" % (fid, e["offset"], offset))
        chunk = pack[e["offset"]:e["offset"] + e["length"]]
        if len(chunk) != e["length"]:
            fail("%s: entry runs past the end of the pack" % fid)
        if sha256_bytes(chunk) != e["sha256"]:
            fail("%s: sha256 of pack slice differs from manifest" % fid)
        offset += e["length"]
        feats = e["features"]
        if not isinstance(feats, list) or len(feats) > MAX_FEATURES or not all(isinstance(t, str) and t for t in feats):
            fail("%s: bad features %r" % (fid, feats))
        o = e["open"]
        if o["ok"] is True:
            if list(o) != ["ok"]:
                fail("%s: open object has extra keys" % fid)
            info = e["info"]
            if list(info) != ["sample_rate", "format_id", "frames_per_packet", "channels", "length_frames", "bitrate"]:
                fail("%s: info keys %r" % (fid, list(info)))
            for k, v in info.items():
                _int(v, "%s info.%s" % (fid, k))
            if info["channels"] not in (1, 2):
                fail("%s: info.channels=%r" % (fid, info["channels"]))
            for label, op_name in (("planar512", PLANAR_READ_OP), ("inter512", INTER_READ_OP)):
                part = e[label]
                if list(part) != ["frames", "error", "sha256"]:
                    fail("%s: %s keys %r" % (fid, label, list(part)))
                _int(part["frames"], "%s %s.frames" % (fid, label))
                if part["frames"] < 0 or not _HEX64.match(part["sha256"]):
                    fail("%s: bad %s" % (fid, label))
                _check_error(fid, label, part["error"], op_name)
                if part["frames"] == 0 and part["sha256"] != EMPTY_SHA256:
                    fail("%s: %s has 0 frames but a non-empty-stream hash" % (fid, label))
            if e["planar512"]["error"] is not None:
                stats["read_errors"] += 1
        else:
            if o["ok"] is not False or list(o) != ["ok", "operation", "status"]:
                fail("%s: bad refusal object %r" % (fid, o))
            if o["operation"] not in OPEN_OPS:
                fail("%s: bad refusal operation %r" % (fid, o["operation"]))
            _int(o["status"], "%s open.status" % fid)
            if not -(1 << 31) <= o["status"] < (1 << 31):
                fail("%s: refusal status is not signed 32-bit" % fid)
            stats["refused"] += 1
        stats["files"] += 1
    if offset != len(pack):
        fail("entries cover %d bytes, pack has %d (padding or trailing bytes)" % (offset, len(pack)))
    return stats


# --------------------------------------------------------------------------------------------
# command line
# --------------------------------------------------------------------------------------------
def diff_manifests(old, new, limit=12):
    msgs = []
    for k in ("schema", "generator", "pack", "apple", "sources"):
        if old.get(k) != new.get(k):
            msgs.append("top-level %r differs: stored %r, rebuilt %r" % (k, old.get(k), new.get(k)))
    oldf = {e["id"]: e for e in old.get("files", [])}
    newf = {e["id"]: e for e in new.get("files", [])}
    for fid in sorted(set(oldf) | set(newf)):
        if fid not in oldf:
            msgs.append("%s: not in stored manifest" % fid)
        elif fid not in newf:
            msgs.append("%s: not rebuilt" % fid)
        elif oldf[fid] != newf[fid]:
            fields = [k for k in set(oldf[fid]) | set(newf[fid]) if oldf[fid].get(k) != newf[fid].get(k)]
            msgs.append("%s: fields differ: %s" % (fid, ", ".join(sorted(fields))))
    if len(msgs) > limit:
        msgs = msgs[:limit] + ["... (%d more differences)" % (len(msgs) - limit)]
    return msgs


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    for s in SETS:
        ap.add_argument("--" + s, help="%s input directory (<family>/<name>.mp3)" % s)
        ap.add_argument("--%s-ref" % s, dest=s + "_ref", help="%s Apple reference directory (reference.json, <family>/<name>.f32.gz)" % s)
    ap.add_argument("--manifest", help="stored manifest.json whose id list (and features) is rebuilt")
    ap.add_argument("--out", help="output directory for pack.bin and manifest.json")
    ap.add_argument("--update", action="store_true", help="accept (write) a rebuilt manifest that differs from the stored one")
    ap.add_argument("--verify-pack", nargs=2, metavar=("PACK", "MANIFEST"), help="check PACK against MANIFEST; needs no corpora")
    args = ap.parse_args(argv)
    try:
        if args.verify_pack:
            st = verify_pack(*args.verify_pack)
            print("OK: %d files (%d Apple refusals, %d files with a read error); pack and manifest are consistent"
                  % (st["files"], st["refused"], st["read_errors"]))
            return 0
        if not args.manifest or not args.out:
            ap.error("--manifest and --out are required (or use --verify-pack)")
        if not os.path.isfile(args.manifest):
            fail("stored manifest %s does not exist (the id list comes from it)" % args.manifest)
        stored = load_json(args.manifest)
        if stored.get("schema") != SCHEMA:
            fail("stored manifest schema is %r" % stored.get("schema"))
        ids_features = [(e["id"], e.get("features", [])) for e in stored["files"]]
        sets, apple = open_sets(args)
        manifest, pack = build(ids_features, sets, apple)
        problems = diff_manifests(stored, manifest)
        if problems and not args.update:
            sys.stderr.write("REFUSING to write: rebuilt manifest differs from the stored one (use --update to accept):\n")
            for m in problems:
                sys.stderr.write("  " + m + "\n")
            return 1
        write_outputs(manifest, pack, args.out)
        st = verify_pack(os.path.join(args.out, "pack.bin"), os.path.join(args.out, "manifest.json"))
        print("wrote %s/pack.bin (%d bytes, sha256 %s) and manifest.json: %d files%s"
              % (args.out, len(pack), manifest["pack"]["sha256"], st["files"],
                 " [UPDATED: %d differences accepted]" % len(problems) if problems else ""))
        return 0
    except FixtureError as exc:
        sys.stderr.write("make_fixtures: ERROR: %s\n" % exc)
        return 1


if __name__ == "__main__":
    sys.exit(main())
