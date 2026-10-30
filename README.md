# Platinum Engine

**Batch audio processing for the command line.** Analyze loudness and pitch, repair clipping, and apply template-based dynamics and pitch correction on macOS and Linux.

Platinum Engine is an independent Rust implementation of Platinum Notes 10.5's processing and file workflows. It runs without the original application and provides both terminal output and JSON for scripts and integrations. It is not affiliated with the Platinum Notes vendor.

[Download](https://github.com/mschienbein/platinum-engine/releases/latest) · [Installation](docs/INSTALL.md) · [CLI reference](research/CLI_CONTRACT.md) · [Release notes](docs/ENGINEERING.md#what-changed-in-v040)

## What it does

- **Analyze and process audio:** loudness and pitch analysis, clipping repair, pitch correction, warmth, multiband expansion/compression and output limiting.
- **Work with familiar formats:** WAV, AIFF, FLAC, ALAC, MP3 and AAC-LC. Keep the input format or choose an output format; Opus is available as a separate opt-in.
- **Process files or folders:** built-in and custom templates, parallel jobs and mirrored output folders.
- **Keep control of your files:** separate outputs, collision protection, processed-file detection, and explicit replacement/backup/restore workflows.
- **Automate the workflow:** per-file JSON results, stable error codes, cancellation and processing statistics.

Format, sample-rate and channel support have platform-specific limits; see [compatibility](#compatibility-and-safety) before processing a library.

## Install

Download the archive for your platform and its `SHA256SUMS` from [GitHub Releases](https://github.com/mschienbein/platinum-engine/releases/latest).

| Platform | Release archive |
| --- | --- |
| macOS — Apple Silicon | `platinum-engine-v0.4.0-macos-arm64.tar.gz` |
| Linux — ARM64 | `platinum-engine-v0.4.0-linux-arm64.tar.gz` |
| Linux — x86-64 | `platinum-engine-v0.4.0-linux-amd64.tar.gz` |

Verify the checksums, unpack into a new directory, then follow the archive's `README.txt` to configure the bundled libraries and their SHA-256 pins. **Library setup is explicit, not automatic.** The [installation guide](docs/INSTALL.md) includes checksum commands and upgrade instructions.

Prefer a container? Use `ghcr.io/mschienbein/platinum-engine:v0.4.0` on Linux amd64 or arm64. Follow the [container setup](docs/LINUX.md#container-image) for library configuration, persistent state and input/output mounts.

## Quick start

Run these commands from the unpacked release directory **after configuring its libraries**, using the same state directory you configured during installation:

```sh
export PN_STATE="$HOME/platinum-state"

# List the built-in templates.
./bin/platinum --state-dir "$PN_STATE" templates list

# Process one file with the Official template, keeping its input format.
mkdir -p "./processed"
./bin/platinum --state-dir "$PN_STATE" process "track.wav" \
  --template-index 0 --format match-input --output-dir "./processed"
```

This writes `processed/track_PN.wav` and leaves `track.wav` unchanged. Existing output files are refused unless you explicitly allow overwriting.

To process a folder with four workers:

```sh
./bin/platinum --state-dir "$PN_STATE" process "./music" \
  --template-index 0 --format match-input --output-dir "./processed" --jobs 4
```

To analyze a file without processing it, or inspect its format and processing marker:

```sh
./bin/platinum --state-dir "$PN_STATE" --json analyze "track.wav"
./bin/platinum --state-dir "$PN_STATE" --json inspect "track.wav"
```

Use `./bin/platinum --help` and `./bin/platinum process --help` for available options. The [CLI reference](research/CLI_CONTRACT.md#commands-and-capability-mapping) documents templates, settings, JSON results and replacement/restore behavior.

## Compatibility and safety

Platinum Engine targets complete-file byte identity with a measured macOS Platinum Notes 10.5 reference—not just similar audio. That is a bounded compatibility target, **not a guarantee for every file, setting, CPU or OS version**. The [verification guide](docs/VERIFICATION.md) records what has been checked and what remains different.

- **Linux:** unsupported sample-rate conversions and codec/container variants are refused rather than approximated. Known x86-64 floating-point edge cases are not all detected and refused; see [Linux limits](docs/LINUX.md#floating-point-and-reference-limits).
- **AAC:** AAC-LC input and output are supported without an opt-in from v0.4.0. Other profiles, some rates/channel layouts and damaged-file shapes remain restricted. See [AAC support](docs/LINUX.md#aac-input-and-output) and the [input compatibility details](docs/AAC_INPUT.md).
- **Opus:** off by default and outside the Platinum Notes compatibility target. See [enabling Opus](docs/LINUX.md#opt-in-opus-input-and-output).
- **File safety:** start with a separate output folder and keep independent backups. Processing markers are not a security boundary for crafted files. Read the [replacement and restore rules](research/CLI_CONTRACT.md) before using them on originals.

### Keeping cover art and WAV tags on Linux

If preserving embedded artwork and source WAV tag data matters more than matching the reference's metadata behavior, enable this setting in your state directory:

```sh
./bin/platinum --state-dir "$PN_STATE" config set parity.allow_non_identical true
```

The default is `false`. Outputs that use these paths declare them in `report.non_identical_paths` and are not claimed byte-identical to macOS. This does not enable unsupported codecs or sample-rate conversion, does not gate MP3 or AAC, and has no effect on the default macOS build. See the [metadata policy](docs/LINUX.md#opt-in-cover-art-and-wav-tags).

## Documentation

- [Installation and upgrades](docs/INSTALL.md) — packages, checksums, state directories and migration notes.
- [CLI reference](research/CLI_CONTRACT.md) — commands, templates, JSON, error codes and file-safety rules.
- [Linux and containers](docs/LINUX.md) — setup, supported paths and platform limitations.
- [Build from source](docs/BUILD.md) — macOS build instructions, dependencies and tests; [Linux builds](docs/LINUX.md#build-from-source).
- [Verification](docs/VERIFICATION.md) — reference comparisons, evidence and known differences.
- [Engineering notes and release history](docs/ENGINEERING.md) — detailed changes, historical results and implementation provenance.

## License and provenance

Project-owned contributions are available under the [MIT license](LICENSE), subject to third-party terms. This is a reverse-engineered compatibility implementation, not a clean-room provenance claim. See [NOTICE](NOTICE), [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) and the [provenance details](docs/ENGINEERING.md#licensing) for the codec implementations, reference-derived tables and bundled libraries.

AAC patent licensing may apply in some jurisdictions. The project grants no patent rights or rights to the reference application and does not claim legal clearance.
