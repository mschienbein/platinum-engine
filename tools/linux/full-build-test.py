#!/usr/bin/env python3
"""Snapshot the current tree and run offline Linux CLI builds and all-target tests.

All persistent writes are confined to two new caller-selected output directories.
The same immutable source snapshot and checksum-verified cached crates are used on
local arm64 Docker and on x86_64 Docker over SSH; no host packages are installed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tomllib
from typing import Any

REPO = Path(__file__).resolve().parents[2]
EXCLUDED = {".git", "target", "local-artifacts", "__pycache__", ".pytest_cache", ".DS_Store"}
RESULT = re.compile(r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; \d+ measured; 0 filtered out;", re.MULTILINE)
# The library-gated lane runs selected suites with --ignored, so other tests are filtered out.
GATED_RESULT = re.compile(r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; \d+ measured; \d+ filtered out;", re.MULTILINE)
REMOTE_BASE = "/home/mooki/platinum-linux-build/"
REMOTE_SHM_BASE = "/dev/shm/platinum-linux-build-"


def digest_file(path: Path) -> str:
    before = path.stat(follow_symlinks=False)
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for block in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(block)
    after = path.stat(follow_symlinks=False)
    def stamp(value: os.stat_result) -> tuple[int, int, int, int, int]:
        return value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns
    if stamp(before) != stamp(after):
        raise RuntimeError(f"file changed during snapshot inventory: {path}")
    return digest.hexdigest()


def inventory(root: Path, exclude_generated: bool) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    def fail(error: OSError) -> None:
        raise error
    for parent, directories, files in os.walk(root, followlinks=False, onerror=fail):
        parent_path = Path(parent)
        names = sorted(directories + files)
        directories[:] = sorted(name for name in directories if not (exclude_generated and name in EXCLUDED)
                                and not (parent_path / name).is_symlink())
        for name in names:
            if exclude_generated and name in EXCLUDED:
                continue
            path = parent_path / name
            relative = path.relative_to(root).as_posix()
            info = path.lstat()
            if stat.S_ISLNK(info.st_mode):
                records.append({"path": relative, "kind": "symlink", "target": os.readlink(path)})
            elif stat.S_ISDIR(info.st_mode):
                records.append({"path": relative, "kind": "directory", "mode": stat.S_IMODE(info.st_mode)})
            elif stat.S_ISREG(info.st_mode):
                records.append({"path": relative, "kind": "file", "mode": stat.S_IMODE(info.st_mode),
                                "size": info.st_size, "sha256": digest_file(path)})
            else:
                raise RuntimeError(f"unsupported special file in snapshot: {path}")
    return sorted(records, key=lambda row: row["path"])


def ignore_generated(_directory: str, names: list[str]) -> set[str]:
    return set(names).intersection(EXCLUDED)


def sparse_index_path(name: str) -> Path:
    name = name.lower()
    if not re.fullmatch(r"[a-z0-9_-]+", name):
        raise ValueError("invalid crate name in Cargo.lock")
    if len(name) == 1:
        return Path("1") / name
    if len(name) == 2:
        return Path("2") / name
    if len(name) == 3:
        return Path("3") / name[0] / name
    return Path(name[:2]) / name[2:4] / name


def prepare_registry(lock: Path, cargo_home: Path, destination: Path) -> list[str]:
    registries = list((cargo_home / "registry/index").glob("index.crates.io-*"))
    if len(registries) != 1:
        raise RuntimeError("expected exactly one cached crates.io sparse registry")
    index = registries[0]
    cache = cargo_home / "registry/cache" / index.name
    index_out = destination / "index" / index.name
    cache_out = destination / "cache" / index.name
    index_out.mkdir(parents=True)
    cache_out.mkdir(parents=True)
    shutil.copy2(index / "config.json", index_out / "config.json")
    missing: list[str] = []
    packages = tomllib.loads(lock.read_text())["package"]
    for package in packages:
        source = package.get("source")
        if source is None:
            continue
        if source != "registry+https://github.com/rust-lang/crates.io-index":
            raise RuntimeError(f"unsupported locked dependency source: {source}")
        name, version = package["name"], package["version"]
        metadata = Path(".cache") / sparse_index_path(name)
        target = index_out / metadata
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.exists():
            shutil.copy2(index / metadata, target)
        archive = cache / f"{name}-{version}.crate"
        if not archive.is_file():
            missing.append(f"{name} {version}")
            continue
        if archive.is_symlink() or digest_file(archive) != package["checksum"]:
            raise RuntimeError(f"cached crate archive fails its lockfile checksum: {name} {version}")
        shutil.copy2(archive, cache_out / archive.name)
    return missing


def freeze_tree(root: Path) -> None:
    def fail(error: OSError) -> None:
        raise error
    for parent, directories, files in os.walk(root, topdown=False, followlinks=False, onerror=fail):
        for name in files + directories:
            path = Path(parent) / name
            if not path.is_symlink():
                path.chmod(stat.S_IMODE(path.stat().st_mode) & ~0o222)
    root.chmod(stat.S_IMODE(root.stat().st_mode) & ~0o222)


def run(args: list[str], *, cwd: Path | None = None, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(args, cwd=cwd, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if check and result.returncode:
        raise RuntimeError(f"command failed ({result.returncode}): {args!r}\n{result.stderr}")
    return result


def counts(path: Path, pattern: re.Pattern[str] = RESULT) -> dict[str, int]:
    if not path.is_file():
        return {"suites": 0, "passed": 0, "failed": 0, "ignored": 0}
    rows = [tuple(map(int, match)) for match in pattern.findall(path.read_text(errors="replace"))]
    return {"suites": len(rows), "passed": sum(row[0] for row in rows),
            "failed": sum(row[1] for row in rows), "ignored": sum(row[2] for row in rows)}


def status(path: Path) -> int | None:
    if not path.is_file():
        return None
    return int(path.read_text().strip())


def log_hashes(directory: Path) -> dict[str, str]:
    names = ("build.stdout", "build.stderr", "test.stdout", "test.stderr", "ignored.stdout", "ignored.stderr")
    return {name: digest_file(directory / name) for name in names if (directory / name).is_file()}


def checked_remote_path(value: str) -> str:
    base = next((base for base in (REMOTE_BASE, REMOTE_SHM_BASE) if value.startswith(base)), None)
    if base is None:
        raise ValueError(f"remote output must be a new direct child of {REMOTE_BASE} or use {REMOTE_SHM_BASE}NAME")
    name = value[len(base):]
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name) or name in {".", ".."}:
        raise ValueError("remote output name must be a single safe path component")
    return value


def verify_snapshot(snapshot: Path, manifest: Path) -> None:
    expected = json.loads(manifest.read_text())
    actual = inventory(snapshot, False)
    if actual != expected:
        raise RuntimeError("snapshot content or metadata differs from the pinned manifest")


def main() -> int:
    if len(sys.argv) == 4 and sys.argv[1] == "--verify-snapshot":
        verify_snapshot(Path(sys.argv[2]), Path(sys.argv[3]))
        print("snapshot matches manifest")
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="new local output directory; parent must exist")
    parser.add_argument("remote_output", help="new /home/mooki/platinum-linux-build/NAME or /dev/shm/platinum-linux-build-NAME on ns2")
    parser.add_argument("--arm-image", default="platinum-native-arm64:clang-final")
    parser.add_argument("--x86-image", default="platinum-native-amd64:study")
    parser.add_argument("--ssh-host", default="ns2")
    args = parser.parse_args()
    remote = checked_remote_path(args.remote_output)
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.ssh_host):
        raise ValueError("invalid SSH host alias")
    for image in (args.arm_image, args.x86_image):
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/@-]*", image):
            raise ValueError("invalid Docker image reference")
    output = args.output.absolute()
    parent = output.parent.resolve(strict=True)
    output = parent / output.name
    if output.is_relative_to(REPO) and not output.is_relative_to(REPO / "local-artifacts"):
        raise ValueError("an output inside the repository must be under excluded local-artifacts")
    output.mkdir(mode=0o700)
    receipt: dict[str, Any] = {"schema": "platinum-linux-full-build-v1", "status": "error",
                               "remote_host": args.ssh_host, "remote_output": remote,
                               "arm_image": args.arm_image, "x86_image": args.x86_image,
                               "build_command": "cargo build --offline --locked --release --bin platinum --features portable",
                               "test_command": "cargo test --offline --locked --all-targets"}
    try:
        snapshot = output / "snapshot"
        shutil.copytree(REPO, snapshot, symlinks=True, ignore=ignore_generated)
        original = inventory(REPO, True)
        copied = inventory(snapshot, False)
        if original != copied:
            raise RuntimeError("repository changed during snapshot, or copied content differs")
        source_manifest = output / "source-manifest.json"
        source_manifest.write_text(json.dumps(copied, sort_keys=True, separators=(",", ":")) + "\n")
        receipt["source_manifest_sha256"] = digest_file(source_manifest)
        freeze_tree(snapshot)
        manifest = output / "snapshot-manifest.json"
        manifest.write_text(json.dumps(inventory(snapshot, False), sort_keys=True, separators=(",", ":")) + "\n")
        receipt["snapshot_manifest_sha256"] = digest_file(manifest)
        receipt["snapshot_entries"] = len(copied)
        arm_platform = run(["docker", "image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", args.arm_image]).stdout.strip()
        x86_platform = run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "docker", "image", "inspect",
                            "--format", "{{.Os}}/{{.Architecture}}", args.x86_image]).stdout.strip()
        local_machine = platform.machine()
        remote_machine = run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "uname", "-m"]).stdout.strip()
        if (local_machine not in {"arm64", "aarch64"} or arm_platform != "linux/arm64"
                or x86_platform != "linux/amd64" or remote_machine != "x86_64"):
            raise RuntimeError(f"unexpected image/host architectures: {local_machine}, {arm_platform}, {x86_platform}, {remote_machine}")
        receipt.update(arm_platform=arm_platform, x86_platform=x86_platform,
                       local_machine=local_machine, remote_machine=remote_machine)
        receipt["arm_image_id"] = run(["docker", "image", "inspect", "--format", "{{.Id}}", args.arm_image]).stdout.strip()
        receipt["x86_image_id"] = run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "docker", "image", "inspect",
                                      "--format", "{{.Id}}", args.x86_image]).stdout.strip()
        cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))).resolve(strict=True)
        if any(":" in str(path) for path in (snapshot, output, cargo_home)):
            raise ValueError("Docker bind paths cannot contain ':'")
        registry = output / "cargo-registry"
        receipt["missing_cached_archives"] = prepare_registry(snapshot / "Cargo.lock", cargo_home, registry)
        freeze_tree(registry)
        registry_manifest = output / "registry-manifest.json"
        registry_manifest.write_text(json.dumps(inventory(registry, False), sort_keys=True, separators=(",", ":")) + "\n")
        receipt["registry_manifest_sha256"] = digest_file(registry_manifest)
        # A new remote directory is mandatory; mkdir fails if it already exists.
        run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "mkdir", "--", remote])
        run(["rsync", "-a", "--", str(snapshot) + "/", f"{args.ssh_host}:{remote}/snapshot/"])
        run(["rsync", "-a", "--", str(registry) + "/", f"{args.ssh_host}:{remote}/cargo-registry/"])
        run(["rsync", "-a", "--", str(manifest), f"{args.ssh_host}:{remote}/snapshot-manifest.json"])
        run(["rsync", "-a", "--", str(registry_manifest), f"{args.ssh_host}:{remote}/registry-manifest.json"])
        run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "python3",
             f"{remote}/snapshot/tools/linux/full-build-test.py", "--verify-snapshot",
             f"{remote}/snapshot", f"{remote}/snapshot-manifest.json"])
        run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "python3",
             f"{remote}/snapshot/tools/linux/full-build-test.py", "--verify-snapshot",
             f"{remote}/cargo-registry", f"{remote}/registry-manifest.json"])
        local_env = os.environ.copy()
        local_env.pop("PN_LINUX_VENDOR_DIR", None)
        local_env.update(PN_LINUX_REGISTRY_DIR=str(registry), PN_LINUX_RUN_TESTS="1", PN_LINUX_TMP_TARGET="1")
        arm_dir = output / "arm64"
        arm = subprocess.run(["sh", str(snapshot / "tools/linux/build-cli.sh"), receipt["arm_image_id"], str(arm_dir)],
                             env=local_env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        (output / "arm-container.stdout").write_text(arm.stdout)
        (output / "arm-container.stderr").write_text(arm.stderr)
        remote_command = ["ssh", "-o", "BatchMode=yes", args.ssh_host, "env", "-u", "PN_LINUX_VENDOR_DIR",
                          f"PN_LINUX_REGISTRY_DIR={remote}/cargo-registry", "PN_LINUX_RUN_TESTS=1", "PN_LINUX_TMP_TARGET=1", "sh",
                          f"{remote}/snapshot/tools/linux/build-cli.sh", receipt["x86_image_id"], f"{remote}/x86_64"]
        x86 = run(remote_command, check=False)
        (output / "x86-container.stdout").write_text(x86.stdout)
        (output / "x86-container.stderr").write_text(x86.stderr)
        x86_logs = output / "x86_64"
        x86_logs.mkdir()
        for name in ("build.stdout", "build.stderr", "build.status", "test.stdout", "test.stderr", "test.status",
                     "ignored.stdout", "ignored.stderr", "ignored.status"):
            run(["rsync", "-a", "--", f"{args.ssh_host}:{remote}/x86_64/{name}", str(x86_logs / name)], check=False)
        arm_binary = arm_dir / "target/release/platinum"
        x86_hash = run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "sha256sum",
                        f"{remote}/x86_64/target/release/platinum"], check=False)
        x86_words = x86_hash.stdout.split()
        if x86_hash.returncode == 0 and x86_words:
            x86_target = x86_logs / "target" / "release"
            x86_target.mkdir(parents=True)
            run(["rsync", "-a", "--", f"{args.ssh_host}:{remote}/x86_64/target/release/platinum", str(x86_target / "platinum")])
            if digest_file(x86_target / "platinum") != x86_words[0]:
                raise RuntimeError("copied x86 binary does not match the remote build")
        receipt["arm64"] = {"container_exit": arm.returncode, "build_exit": status(arm_dir / "build.status"),
                            "test_exit": status(arm_dir / "test.status"), "counts": counts(arm_dir / "test.stdout"),
                            "ignored_lane_exit": status(arm_dir / "ignored.status"),
                            "ignored_lane_counts": counts(arm_dir / "ignored.stdout", GATED_RESULT),
                            "binary_sha256": digest_file(arm_binary) if arm_binary.is_file() else None,
                            "log_sha256": log_hashes(arm_dir)}
        receipt["x86_64"] = {"container_exit": x86.returncode, "build_exit": status(x86_logs / "build.status"),
                             "test_exit": status(x86_logs / "test.status"), "counts": counts(x86_logs / "test.stdout"),
                             "ignored_lane_exit": status(x86_logs / "ignored.status"),
                             "ignored_lane_counts": counts(x86_logs / "ignored.stdout", GATED_RESULT),
                             "binary_sha256": x86_words[0] if x86_hash.returncode == 0 and x86_words else None,
                             "log_sha256": log_hashes(x86_logs)}
        verify_snapshot(snapshot, manifest)
        verify_snapshot(registry, registry_manifest)
        run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "python3",
             f"{remote}/snapshot/tools/linux/full-build-test.py", "--verify-snapshot",
             f"{remote}/snapshot", f"{remote}/snapshot-manifest.json"])
        run(["ssh", "-o", "BatchMode=yes", args.ssh_host, "python3",
             f"{remote}/snapshot/tools/linux/full-build-test.py", "--verify-snapshot",
             f"{remote}/cargo-registry", f"{remote}/registry-manifest.json"])
        receipt["source_unchanged_through_build"] = inventory(REPO, True) == original
        receipt["status"] = "pass" if receipt["source_unchanged_through_build"] and all(
            receipt[arch][key] == 0 for arch in ("arm64", "x86_64")
            for key in ("container_exit", "build_exit", "test_exit", "ignored_lane_exit")) else "fail"
    except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
        receipt["error"] = str(error)
    (output / "receipt.json").write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    print(json.dumps(receipt, sort_keys=True))
    return 0 if receipt["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
