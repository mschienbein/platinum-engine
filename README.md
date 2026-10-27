# Platinum Engine

A Rust command-line audio processor targeting the behavior of Platinum Notes 10. It implements a headless processing and file-workflow engine rather than a graphical clone or a generic loudness-normalization substitute. Platinum Engine is not affiliated with the vendor.

## Current state

**One pinned fixture and configuration are byte-identical; complete all-input parity has not been established.** On macOS arm64, two Platinum Notes 10.5.0.1256 exports and corrected Rust debug/release outputs matched as complete files for a generated 16-second, 48 kHz, stereo 16-bit WAV, using the Official template and WAV output. Each file was 3,076,786 bytes with SHA-256 `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b`. No audio, container, or metadata normalization was used.

The CLI implements the three-pass pipeline, templates, settings, codecs, metadata and backup/restore workflows. Other templates, settings, rates, formats and process contexts remain open. The runtime uses Rust DSP, macOS system frameworks and separately built public libraries; it does not call the original application or its proprietary components. Those components are used only for optional reference comparisons, not ordinary operation.

### Measured progress

| Area | Verified result or remaining gate |
| --- | --- |
| Clip repair | 32 generated native cases matched every sample bit and captured event property. |
| Wrapper phase correction | 40 generated native cases matched coefficients, samples, latency and state transitions in debug and release builds. |
| Shipped multiband graphs | All four graphs match captured phase-off output/intermediates and long phase-enabled output, including current-template controls and native shared-buffer warmup. |
| Limiter | All 112,384 sample words in the captured 56,192-frame stereo impulse/tail case match after replaying the observed host defaults. |
| Pitch correction | Four original AU audio captures and both strict aligned-profile scheduler cases match. Controlled original-to-original comparisons establish scratch-alignment-dependent output. |
| Analysis | Original tuning, sinc-table, partitioned-resampler and harmonic-removal leaves pass. The whitening boundary has a regular synthetic native-golden regression; restoring the old boundary makes it fail. The original-app tuning and pinned complete-file comparisons pass. |
| Rust test runs | The public-preparation all-targets run passed 236 regular tests, with 78 opt-in tests ignored. The dependency-enabled engine/codec/CLI run passed all 21 tests; these counts overlap. |
| Codecs and tags | Public FLAC/LAME paths and native planar decoding have passing bounded comparisons. All 38 original metadata/container cases match complete bytes. The generated planar-stereo AAC input matches MIKAudio; the separate interleaved AAC diagnostic remains unresolved. |
| CLI smoke | Fresh debug and optimized release runs both match original exports A and B, including all WAV/header/processing-marker bytes. |
| File safety | Review corrections passed: retained preflight identities, metadata/resource-fork/ACL-preserving backups, collision reservations, decoder-independent restore, native nil markers, and synchronized cancellation/publication. Real CLI SIGINT exited 130 without publishing; replacement/restore preserved generated source bytes and metadata. |
| Full-file acceptance | **Pass for the pinned Official/WAV case:** 3,076,786 bytes; SHA-256 `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b`. Additional templates, modes, formats and process contexts remain open. |

**Known limits:** the original pitch-correction component varies with splitter FFT scratch alignment; Rust implements the observed aligned-0 profile, not every allocation context. A valid MP3 Xing header without a recognized LAME source header encounters an original uninitialized-byte-dependent path; the CLI refuses to guess that byte. A separate interleaved AAC diagnostic remains unresolved, although the captured planar-stereo processing-layout comparison matched. Ogg support is not established, and protected sources are rejected.

Historical comparison records retain earlier failures and the subsequent whitening-boundary correction. Component matches and test counts do not establish complete-file parity outside the pinned case.

## Scope

The implementation exposes volume and pitch analysis, clipping repair, pitch correction, warmth, multiband expansion/compression, output limiting, built-in/custom templates, encoding, tags and artwork. It also supports batch status and cancellation, processed-file detection, separate output folders, explicit replacement with backups, restore, and machine-readable analysis and waveform reports. Implemented controls are not a claim that every combination matches the reference.

The [CLI contract](research/CLI_CONTRACT.md) specifies the commands, settings, safety behavior and verification boundary. The reference comparison target is Platinum Notes 10.5.0.1256 for macOS arm64; no reference installation is required to run the CLI.

## Run the CLI

Build and usage are currently scoped to **macOS arm64**. You need the pinned Rust toolchain and Apple developer tools; processing also requires the ABI-5 bridge built against public TagLib 1.11.1. FLAC input or output requires an explicitly configured public libFLAC 1.4.2 and SHA-256 pin. MP3 output—including the default output format—requires explicitly configured LAME 3.100 and a SHA-256 pin. The libraries are not silently taken from the reference app.

```sh
cargo build --locked --bin platinum
target/debug/platinum --help
target/debug/platinum --state-dir "$PWD/local-artifacts/example-state" --json templates list
target/debug/platinum --state-dir "$PWD/local-artifacts/example-state" --json config show
```

The [build guide](docs/BUILD.md) contains source URLs and checksums, native build commands, configuration, and a generated-audio processing example. Use `--state-dir` to isolate settings and an existing, separate output directory for initial processing. Output files are not silently overwritten. Replacement requires explicit flags and an existing backup directory on every run; retain an independent backup as well. Restore requires the exact recorded backup path, and some nil-marker outputs cannot authorize CLI restore. See the [CLI contract](research/CLI_CONTRACT.md) before using replacement or restoration.

## Reference evidence and limits

The pinned result compares entire encoded files, including headers and processing markers; component-only tests are narrower diagnostics. The original pitch-correction alignment and MP3 header behavior described above can depend on runtime state, so original-to-original repeatability must be established for each new case rather than assumed. Original-component comparisons require a separately obtained local installation; they are not production dependencies.

The [verification guide](docs/VERIFICATION.md) summarizes the pinned case, the earlier mismatch and correction, test prerequisites, and the boundaries of the evidence without requiring raw reference captures.

The analyzer is identified from static evidence as K-weighted integrated LUFS-style measurement, not a demonstrated drum extractor. The controller follows analyze → heal/remeasure → final limit/encode. Neither these observations nor the presence of a command prove every reference behavior.

## Licensing

Project-owned contributions are available under the [Zero-Clause BSD license (0BSD)](LICENSE), to the extent their contributors can grant those rights. It permits use, modification and distribution, including commercially, without an attribution condition. Third-party dependencies have their own terms; consult their upstream packages and the [build guide](docs/BUILD.md) before distributing them. This license does not grant rights to the reference application or its artifacts.

This reconstruction was informed by static binary analysis and measured reference behavior; it is not a clean-room provenance claim. Compatibility names, identifiers and typed parameter values retain their reference-derived provenance. The public snapshot excludes original serialized preset blobs, raw decompiler/disassembly exports, compiled resources, screenshots and the private research history.

## Verification policy

Acceptance requires **SHA-256-identical complete output files** to Platinum Notes 10.5.0.1256 for the same source bytes and effective settings. Encoded audio, container/header bytes and metadata all count. First establish original-to-original repeatability with the reference build/runtime and metadata-relevant paths/state pinned. If the original is nondeterministic, retain and explain the differing bytes rather than quietly weakening the requirement. Per-stage PCM comparisons are diagnostics only. A command, flag, generic DSP substitute, or wrapper around the original proprietary engine is not a completed Rust port.
