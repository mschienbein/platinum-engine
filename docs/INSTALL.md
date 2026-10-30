# Installation and upgrades

Use a release archive for macOS arm64 or Linux arm64/amd64, or the Linux container image. A Platinum Notes installation is not required. For everyday commands, see the [quick start](../README.md#quick-start); for a source build, see [BUILD.md](BUILD.md) or [LINUX.md](LINUX.md#build-from-source).

## Install and run

Release archives are named `platinum-engine-v0.4.0-macos-arm64.tar.gz`,
`platinum-engine-v0.4.0-linux-arm64.tar.gz`, and
`platinum-engine-v0.4.0-linux-amd64.tar.gz`. Obtain the appropriate archive and
the accompanying `SHA256SUMS` from the
[GitHub Releases page](https://github.com/mschienbein/platinum-engine/releases),
verify its digest using a trusted release checksum, and unpack
it in a new directory. The archive's `README.txt` gives commands to verify its
internal checksums and explicitly configure its library paths and codec
digests; it does not configure them automatically. Checksums alone do not
authenticate an untrusted download.

After downloading the selected archive and the release's `SHA256SUMS` into
the current directory, set `ARCHIVE` to its filename. This example refuses an
existing installation directory and verifies the package before using it:

```sh
(
  set -eu
  python3 - "$ARCHIVE" <<'PY'
import hashlib
from pathlib import Path
import sys
archive = Path(sys.argv[1])
suffix = "  " + archive.name
matches = [line[:-len(suffix)] for line in Path("SHA256SUMS").read_text().splitlines()
           if line.endswith(suffix)]
if len(matches) != 1:
    raise SystemExit("expected exactly one checksum for the selected archive")
digest = hashlib.sha256()
with archive.open("rb") as file:
    for chunk in iter(lambda: file.read(1024 * 1024), b""):
        digest.update(chunk)
if digest.hexdigest() != matches[0]:
    raise SystemExit("release archive checksum mismatch")
PY
  mkdir "$HOME/platinum-engine-v0.4.0"
  tar -xzf "$ARCHIVE" -C "$HOME/platinum-engine-v0.4.0"
  cd "$HOME/platinum-engine-v0.4.0"
  if [ "$(uname -s)" = Darwin ]; then shasum -a 256 -c SHA256SUMS
  else sha256sum -c SHA256SUMS; fi
  ./bin/platinum --help
  ./bin/platinum --state-dir "$HOME/platinum-state" --json templates list
)
```

### Opt in to cover art and WAV tags on Linux

A Linux caller that must keep embedded cover art and WAV tag chunks sets one
boolean in its state directory, once, after the libraries are configured (the
archive's `README.txt` shows how); the `process` command line does not change:

```sh
./bin/platinum --state-dir "$HOME/platinum-state" config set parity.allow_non_identical true
```

The default is `false`. With it on, cover art is copied byte for byte (also into
and out of a WAV) and the tag chunks of a WAV source that carry source data are
kept; items that took either path say so in `report.non_identical_paths`, and
their output is **not** guaranteed byte-identical to macOS. MP3 input and AAC
input and output need no setting: they are the exact ports of AudioToolbox's
MP3 reader and AAC encoder and decoder, and the setting does not touch them.
The setting has no effect on the macOS build. Details and limits:
[LINUX.md](LINUX.md#opt-in-cover-art-and-wav-tags).

### Upgrading from v0.1.0

Unpack v0.4.0 into a new directory next to v0.1.0; do not overwrite it. Library
configuration lives in the state directory, so after unpacking, point
`libraries.tag_bridge` at the v0.4.0 bridge (and, if you want the new
directory to be self-contained, `libraries.flac` and `libraries.lame` at its
copies). On Linux the bridge has had a new SHA-256 since v0.3.0 and the v0.1.0
bridge is refused by the v0.4.0 CLI, so this step is required there; the libFLAC
and LAME files are byte-identical to v0.1.0 and keep their SHA-256 pins. Without
the opt-in, Linux behaves as in v0.1.0 except that MP3 input is decoded exactly
(since v0.3.0) and AAC input and output are exact (since v0.4.0); every JSON
change is additive (error `code` members, `report.non_identical_paths`, the Opus
members when Opus is used).

### Upgrading from v0.2.0

Unpack v0.4.0 into a new directory next to v0.2.0 and repoint
`libraries.tag_bridge` at the v0.4.0 bridge: on Linux the bridge has had a new
SHA-256 since v0.3.0 and the v0.4.0 CLI refuses the v0.2.0 bridge (libFLAC and
LAME are unchanged; libopus is needed only for Opus). A state file that stores
`parity.allow_non_identical` stays valid, but the setting no longer gates MP3
input: MP3 is decoded exactly with or without it, the `mp3_decode` member of
`report.non_identical_paths` no longer exists, and samples decoded from MP3 can
differ from those the v0.2.0 opt-in decoder produced. A state file that stores
`formats` or `libraries.opus*` cannot be read by v0.2.0. Details:
[LINUX.md](LINUX.md#upgrading-from-v020).

### Upgrading from v0.3.0

Unpack v0.4.0 into a new directory next to v0.3.0. Library configuration lives
in the state directory and a state file stays valid. The Linux bridge, libFLAC,
LAME and libopus pins of v0.4.0 are unchanged since v0.3.0 (bridge `5c52c5f1…` aarch64 / `8c61f6bb…` x86_64, libFLAC `41b59a45…` aarch64 / `9420c96f…` x86_64, LAME `c780062f…` aarch64 / `cf59ae7f…` x86_64, libopus `c09146de…` aarch64 / `d09791af…` x86_64); the CLI refuses a
library that is not on its allowlist, as before. Three behaviours change on Linux
and `--features portable` builds. AAC input and output, which v0.3.0 refused
(`unsupported_input_codec` and `unsupported_output_codec`), are now exact and need
no setting; `unsupported_output_codec` is no longer produced, and a hand-built
AAC file that v0.3.0 refused as an unsupported codec can now be reported as
`invalid_input_file` or decoded, and an MP4 whose first sound entry is neither
`mp4a` nor `alac` nor a type AudioToolbox opens as another codec is now
`invalid_input_file` (`'wht?'`, as on macOS) where v0.3.0 answered
`unsupported_input_format`. ALAC output files can differ from v0.3.0's in two
cases, where v0.3.0 differed from AudioToolbox: the last packet of a stereo or
multichannel stream shorter than 288 frames, and files with packets that
AudioToolbox has to encode again (see the [ALAC release note](ENGINEERING.md#what-changed-in-v040)); an output that
v0.3.0 wrote can now be refused with `encode_failed` where the macOS writer
refuses it too. With `parity.allow_non_identical` on, a WAV
output whose source has no tempo, key or tags no longer keeps TagLib's empty
`ID3 ` chunk and no longer lists `wav_tag_preservation`: its bytes equal the
default output's. Consumers that required that member for such items should stop
doing so. On every build, a panic in one batch item's worker no longer ends the
run, but it can leave that item's staging files behind (`.pn-workflow-*`
directories and a `.<name>.pn-reservation` file next to the output): remove them
by hand before reprocessing the item. Details: [LINUX.md](LINUX.md#upgrading-from-v030).

### Container image

The multi-architecture container image is built by the release workflow from the
verified Linux archives and published as `ghcr.io/mschienbein/platinum-engine:v0.4.0`
(also `:latest`). The image runs as
an unprivileged user, does not auto-configure libraries, and needs durable state
and deliberately selected input/output mounts. In Docker Desktop on macOS, use
a container volume or tmpfs for state, and use the libraries already inside
the image, rather than bind-mounted libraries. See [LINUX.md](LINUX.md)
for complete setup, volume ownership, example commands and platform limits.

For source builds, see [BUILD.md](BUILD.md). Use `--state-dir` to isolate
settings and a separate output directory for initial processing. Existing
outputs are refused unless `--overwrite` is given. Replacement requires
explicit flags and an existing backup directory on every run; retain an
independent backup. Restore requires the exact recorded backup and consumes
that backup after a verified restore; some nil markers cannot authorize
restore. Read the [CLI contract](../research/CLI_CONTRACT.md) first.
