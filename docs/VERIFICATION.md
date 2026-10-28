# Verification and evidence boundaries

The project uses complete-file identity as its reference-comparison criterion: audio, container headers, encoder output and metadata all count. Component tests and decoded PCM comparisons help locate differences; they do not substitute for a complete-file match.

## First pinned match

The recorded reference was Platinum Notes 10.5.0.1256 on macOS arm64. Its two same-session exports, A and B, matched each other, and the corrected Rust debug and release outputs matched them as complete files for this case:

| Setting | Recorded value |
| --- | --- |
| Input | Generated WAV; 16 seconds, 768,000 frames, stereo, 48 kHz, signed 16-bit PCM |
| Input SHA-256 | `2de421ff4f080693c78a24947ad856d9db8f86c5909f42252187307aa820f884` |
| Template and output | The Official Platinum Notes Template; WAV; specific output folder; no replacement |
| Output length | 3,076,786 bytes |
| Complete-file SHA-256 | `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b` |

No tags, headers or audio were normalized for that comparison. This demonstrates a match for **one input and one configuration**. It does not establish arbitrary-input, all-template, all-format, cross-process or cross-OS parity. In particular, original A and B were same-session repeats; they are not evidence of repeatability across every fresh application process.

### Historical correction

The first Rust output for this fixture did **not** match the original complete file. The captured PCM payload matched, but the input and output pitch values in the processing marker differed: the original reported `440.63037109375` Hz and Rust reported `440.6307373046875` Hz. The original file was 3,076,786 bytes; the earlier Rust file was 3,076,790 bytes. Investigation identified an off-by-one at the right edge of pitch-analysis whitening: the original replicates the mean/deviation from bin 247 where the earlier model used bin 246. Correcting that general boundary, without editing metadata or relaxing the comparison, produced the complete-file match above. The historical failure is not a pass.

The regular synthetic whitening regression checks all 256 output words plus the mean/deviation boundary, using measured native values. A controlled copy of the source with the old `N - 10` boundary restored failed at mean bin 247; the corrected source passed. This regression does not require the reference application or private fixture files.

A user-reported Big Boost export found in the earlier reference output directory matched a Rust Big Boost output for the previously generated input: 3,076,782 bytes, SHA-256 `cbf49914aa7000a5535aff29228d5097627b99946d0050f55e85c96b9e39a09d`. The input selection was not independently observed, no second original export was captured, and the marker says pitch correction was inactive. Treat this as supporting evidence, not proof of pitch-active or general template parity.

## Integrated crossover correction

The three controlled expansion-only, compression-only and combined-dynamics
cases originally differed by 8, 3 and 99 one-unit PCM24 samples. An owned native
component investigation established a crossover-control ramp over each host
event segment. The corrected graph now schedules normalized crossover controls
and advances them before each internal chunk. A regular bit-exact regression
checks all eight recorded ID-0 frequencies and first-low-section states for one
512-frame host call versus eight separate 64-frame calls.

The earlier integrated debug checkpoint, executable SHA-256
`dcebd44e72c8a2413f66db2aff2fc7ee9c141b60b123d64dd24041b35422b00b`,
matched both retained original files for those three cases and five controls:
gentle warmth, Big Boost, active pitch, Official baseline and custom pitch.
Its before/after hash receipts and outputs survive, but its executable bytes
were not separately archived before that target path was rebuilt.

The current source snapshot, including statistics and waveform changes, was
built and its executables archived before comparison. Debug SHA-256
`9575c5184067d0d55a367581a57bc43e3a1849877ed146f340e349f142830db9`
and optimized release SHA-256
`1ac812ef87257c5737105339eac19ce384c05504701debb47800430c021cf429`
each matched both retained native references in all eight cases. Every byte
and length counts, without normalization. Original locale, hidden settings,
timing and actual concurrency gaps remain: these are bounded file matches,
not strict contextual or universal parity. Further source changes require
new final-build verification.

The earlier `dcebd44…` checkpoint's explicit ambiguous-WAV-marker override matched both
native references for the tested empty and incomplete-JSON marker inputs.
Default processing still refused them, and a valid processed marker was
refused with or without the override. This is a user-selected safe-default
policy, not default-admission equivalence.

The archived optimized release additionally matched both preserved original
files in 71 selected format, metadata, DSP and boundary cases. The two explicit
ambiguous-marker cases are included. Execution was serial, with one input per
process and a configured jobs limit of 10; this is not evidence of ten active
workers. The reference sets overlap with the eight-case replay, so their counts
must not be added as unique coverage. Variable original outputs, timestamp
cases, path-sensitive workflows and safety refusals were retained as exclusions,
not counted as passes. Historical strict native-context gaps remain.

## JSON report precision correction

Calibration for new pitch-threshold cases exposed a reporting defect: JSON
inspection reparsed Foundation's marker numbers with default `serde_json`
float parsing, which could differ from the stored value by one binary64 unit.
Written files were unaffected; process reports and the encoded markers were
already exact. The dependency now enables exact float round-tripping, with a
regression that failed before and passes after the change. The corrected
optimized release, SHA-256
`8b8cfa128147ded271e6f1f6837f2d082ecd0ad004e6d1956a60a2505c1ee18e`,
again matched both native references in all eight dynamics/pitch cases and all
71 selected stable cases.

## Final release build

The final source adds `process --overwrite` and consumes the backup after a
verified restore; see the [CLI contract](../research/CLI_CONTRACT.md). Its
optimized release, SHA-256
`dd4b5fd05ac23bbf967e6688b71dee142ecdc1c3b399e42fe9f906ee9c7b9b98`
(LC_UUID `F3D8E57F-6B10-3CC7-A466-6B86E39E9197`), passed 317 regular tests with
92 ignored, the 20 public-library integration tests, the native
waveform-reducer replay and the Xing caller-byte oracle. Without normalization
it matched both native references in:

| Set | Matched |
|---|---|
| Strict full-context families c01–c03, c05–c07, f01–f03 | 9 of 9, no flags |
| c04 ALAC exports, given each recorded MP4 second | 2 of 2 |
| Retained dynamics/pitch cases | 8 of 8 |
| Selected format, metadata, DSP and boundary cases | 71 of 71 |
| MP4 exports, given each recorded second | 10 of 10 |
| E05 Xing without a recognized LAME tag | 1 of 1 |
| Nonzero-width dynamics gates and rounded pitch gates (D22–D24) | 15 of 15 |

The sets overlap. The strict reviewer is a copy of the frozen family toolkit
whose only change accepts the new `effective_config.overwrite` field and
requires it, `details.overwrite` and `replaced_existing_output` to be false.
The 71-case and eight-case harnesses likewise add `overwrite: false` to
historical profiles that predate the option. A CLI smoke on generated files
confirmed collision refusal by default, atomic `--overwrite` replacement with
the input untouched, source replacement with a verified backup, rejection of an
interdependent restore without changes, and backup consumption after restore.
Receipt: `local-artifacts/hash-parity/release-final-dd4b5fd0.json`.

The final binary also repeated the recorded candidate workflows. The W01 mixed
folder exited 1 with a partial failure for the Ogg file, and its three WAV
outputs matched native. The W02 same-folder and mirror-tree outputs matched
native; the mirror root matched native B, since native A has the known marker
variant. An existing destination was refused by default and replaced
identically with `--overwrite`. W03 output-permission failure published
nothing. SIGINT exited 130 without publication when sent during analysis and
during healing; with this faster binary, the original 5-second signal timing
lands in healing rather than analysis. The gauge fixture output matched the
native export, totals reached one song, eight dB and zero clips, and reset
behaved as before. CLI waveform bins equaled the composed native decoder and
reducer words for all four generated WAV fixtures. Receipt:
`local-artifacts/hash-parity/release-final-dd4b5fd0-supplement.json`.

## Additional native boundary cases

Two generated nonzero-width dynamics gates were imported natively. Native
GUI readback and the persisted native template IDs matched the prepared
templates. For three inputs measuring just below, at and just above
`-3.1` LUFS, the lower-inclusive `[-3.1, -1]` gate and upper-inclusive
`[-8, -3.1]` gate produced expected stage flags F/T/T and T/T/F. All six native A,
native B and Rust complete files were identical.

Nine generated pitch cases cover rounded corrections of 9, 10 and 11 cents for
negative detuning with preserved tempo and with turntable correction, plus
positive turntable correction. Native A, native B and Rust complete files were
identical in all nine. The threshold is a rounded measured correction, not an
exact physical cent boundary. Original locale, hidden settings and scheduling
remain unobserved.

## Strict full-context family captures

Earlier native captures lacked recorded locale, hidden settings, worker
concurrency or template state, so they were bounded matches rather than strict
passes. By user decision, one generated case per format family was recaptured
with full context and stands for its family. Each capture records:

- the native process instance (PID and start time before and after each render) and its arm64 architecture;
- the on-disk bundle version, executable SHA-256 and code signature;
- effective locale and timezone, and a privacy-redacted preferences snapshot;
- the effective template, derived from the native template store and checked against GUI readback;
- the exact admitted input and a polled single-item queue log;
- a candidate job trace recording each file job and every loaded library with its in-memory Mach-O UUID.

With a processor limit of 10, `parity-matrix compare` reported
`reviewed-complete-file-match-for-this-case` with no flags for nine families.
In each, two native exports from one process and the release candidate were
identical:

| Case | Template | Family |
|---|---|---|
| c01 | Official | 48 kHz stereo16 WAV → WAV |
| c02 | Official | 44.1 kHz stereo24 AIFF → match input |
| c03 | Official | 96 kHz stereo24 FLAC → MP3 CBR 320 (sample-rate fallback) |
| c05 | Official | 44.1 kHz AAC → MP3 VBR 192 |
| c06 | Official | MP3 CBR → match input |
| c07 | Official | 48 kHz mono24 WAV → FLAC |
| f01 | The Big Boost | Preserve-tempo pitch correction (+31 cents) with expansion, warmth and clip repair |
| f02 | Festival | 44.1 kHz stereo24 WAV → AIFF |
| f03 | Custom, imported natively | Turntable pitch (−34 cents), expansion and compression both active, hot warmth, clip repair |

For c04, 48 kHz stereo24 ALAC → match input, the two native exports differ only
in their MP4 header second. Given each export's recorded second, the candidate
reproduced both complete files. Evidence, abandoned attempts and attestation
limits are in `local-artifacts/hash-parity/family-fullctx-01/summary-v2.json`.
GUI polls are sampled, so the single-item queue and process continuity are
operator attestations. Loaded native images are identified from on-disk hashes
and signatures, not read from process memory.

## Batch pitch-marker variation

When several files are analyzed at once, native repeats can record slightly
different measured input pitch while the audio stays identical with correction
off. Constructing a tuning detector resets a global DSP dispatch table without
synchronization; the magnitude slot is at `0x1000ea720`. A concurrently analyzed
file can then compute one spectral window with the scalar or intermediate
magnitude routine. That routine reads the vDSP FFT's split buffer as interleaved
pairs.

Modeling one such window reproduced all five recorded native outlier words
exactly, and 80 end-to-end diagnostic replays matched. Every single-file and
one-worker native export matches the race-free value. By user decision, the
parity target is that race-free native result, which the CLI produces
deterministically; the batch outliers are documented native nondeterminism.
The historical dispatch state was not captured, and several windows can produce
each outlier. Receipts are listed under `batch-pitch-marker-race` in the private `local-artifacts/hash-parity/external-evidence-index.json`.


## Reproducing the candidate side

Follow [BUILD.md](BUILD.md), including the generated-fixture section. It pins the source archive checksums, Rust toolchain, native dependency versions, input digest, output configuration and expected output digest. The generator uses a new output path and generated audio, not recordings from a music library. Floating-point math runtimes can affect regenerated bytes: if the input digest does not match, it is not the pinned input. If the output digest differs, retain and investigate it rather than changing the comparison criterion.

The recorded native setup included a public TagLib 1.11.1 ABI-5 bridge, public FLAC 1.4.2 without Ogg, and public LAME 3.100. FLAC and LAME are not needed for the WAV fixture, but they were used in other bounded checks. Other host diagnostics recorded macOS 27.2 build 26B118 on arm64 and the native TagLib build used the macOS 27.0 SDK and CMake 3.31.6. Different toolchains, system frameworks, library builds, paths and metadata-relevant state may affect results; following the recipe is not a guarantee of identical output on another system.

To compare against your own locally obtained reference exports, preserve both unmodified files from the same input and effective settings, using the same reference version and output path. Preserve the first export before running the second. For example, after setting `A` and `B` to those two preserved files and following the build guide in the same shell:

```sh
C="$DEPS/fixture/output/reference_PN.wav"
wc -c "$A" "$B" "$C"
shasum -a 256 "$A" "$B" "$C"
cmp -s "$A" "$B" && cmp -s "$A" "$C" && printf 'Complete files match\n'
```

This optional reference exercise requires a separately obtained local reference application; the ordinary build and candidate run do not. Use only generated audio, do not alter the application or its signing, and preserve mismatches and context. A successful comparison on another configuration supports only that configuration.

For additional generated inputs, `node tools/parity-matrix.mjs --help` describes a case workflow that preserves original and candidate files and compares complete bytes. It records operator attestations and does not automate or authenticate the reference GUI. Strict reports now require concrete effective settings, process/render identity, independently evidenced stage states, actual concurrency evidence and ordered timing. Legacy receipts remain readable but incomplete where these facts were not captured; no historical unknown is retroactively filled. A candidate-only repeat is not an original comparison.

## Tests

From the repository root, the regular suite can be run without opting into reference-dependent tests:

```sh
cargo test --locked --all-targets
```

Tests marked `#[ignore]` are excluded from that command. Some need a separately built public bridge and codecs; others require native reference captures or separate oracle programs. Do not infer that ignored cases passed. After building and configuring the public libraries as described in [BUILD.md](BUILD.md), the following explicitly runs the ignored synthetic engine, codec and CLI tests that use those public libraries, without requiring the proprietary reference application:

```sh
PN_TAG_BRIDGE="$DEPS/libpn-tag-bridge.dylib" \
PLATINUM_TEST_TAG_BRIDGE="$DEPS/libpn-tag-bridge.dylib" \
PN_FLAC_LIB="$FLAC_LIB" PN_FLAC_SHA256="$FLAC_SHA" \
PN_LAME_LIB="$LAME_LIB" PN_LAME_SHA256="$LAME_SHA" \
cargo test --locked --test engine_reference --test engine_codec_reference --test cli --test statistics_cli -- --ignored
```

These integration tests use generated files and exercise their individual conditions; they do not establish complete original-app parity. Some regular tests also overlap with other reported runs, so do not add test counts as if all executions were distinct.

The final private development snapshot passed 317 regular tests and left 92 opt-in tests ignored. The curated public snapshot passed the same 317 regular tests with 90 ignored; it excludes the two private diagnostic targets `aac_32k_fail_closed.rs` and `aac_writer_warning_probe.rs`, each holding one ignored test. The public-dependency engine/codec/CLI/statistics run passed 20 selected tests on the final source, including explicit marker admission, no-clobber, replacement/restore and post-publication accounting cancellation paths.

A separate opt-in replay consumed three captured native waveform-component input/output sequences and matched every float word. Four generated PCM16 WAV decoder cases (mono/stereo at 44.1/48 kHz, requesting 44.1 kHz) also matched every returned float byte and read boundary. The actual release CLI's waveform bins match the composition of those captured decoder words and the independently tested reducer. This is not a direct full-file GUI waveform capture, and other formats remain unverified on that path. Four historical pitch checkpoint comparisons and 96 generated Ogg decoder comparisons passed against captured or signed original components; none establishes full application parity.

## Public source boundary

The public repository starts with independent history. It includes Rust sources, the public TagLib bridge source, synthetic tests, build instructions, typed preset parameters and the transformed human-readable template configuration required by the CLI. It excludes original serialized preset blobs, extracted compiled resources, screenshots, decompiler/disassembly exports, native libraries, audio captures and private research history. Optional reference tests may contain authored probe code or expect separately supplied captures; their presence does not mean those captures or proprietary components are distributed or needed by the production CLI. Static reconstruction and measured values are reference-derived, not a clean-room or licensing-clearance claim.

## Remaining limits

- The original pitch-correction splitter's output depends on its FFT scratch alignment. Real/imaginary scratch at 64-byte offset 0 gives one output profile; offsets 16, 32 and 48 give another, identical to each other. Every retained native pitch-active export in the five alignment-sensitive cases (D05, Big Boost, D04 and preserve-negative r10/r11; ten GUI exports) matches only the aligned-0 profile that Rust pins. Turntable cases do not distinguish the profiles. The splitter allocates through a 16-byte-aligned `malloc` callback, and 240 isolated processes repeating its allocation sequence never produced offset 0 for real/imaginary. The GUI therefore did not show plain-malloc behavior in any observed run, but its literal pointers and any callback override were not observed (`splitter-fft-alignment` in the private evidence index above).
- A Xing MP3 path without a recognized LAME tag makes the native updater read a reused caller byte. Static analysis shows that byte is part of the limiter AudioUnit handle and is nonzero on the in-process AudioToolbox creation path, so the CLI applies the nonzero branch. With that change the release CLI reproduced the retained original A/B/C export for the generated E05 case (644,959 bytes, SHA-256 `12b12b9ddff607fc9f82e38a25dd212518a6cbd462dc6aa3f44f79c7cf16971d`). The byte was not observed in the running app; see `local-artifacts/hash-parity/xing-nonzero-cutover-omp-01.json`.
- In one interleaved AAC fixture, original MIKAudio and direct AudioToolbox reads matched within each process but produced different bits across fresh processes with the same recorded conditions. Rust and direct reads also matched in a same-process diagnostic. By user decision, the decoding target is the native same-process result; the cross-process cause remains unknown.
- A separate direct-system AAC experiment compared untouched FPSR against clearing it before each read: 48 readers across 12 fresh processes all produced identical float streams, despite nonzero untouched FPSR values. It did not reproduce or explain the historical variability; the original app and MIKAudio wrapper were not used.
- AAC and ALAC outputs store one wall-clock second in all six `mvhd`/`tkhd`/`mdhd` creation and modification fields, so native repeats differ from each other. By user decision, an MP4 case passes when the CLI, given a native export's recorded second through the hidden `--controlled-mp4-time-1904` diagnostic option, reproduces that export's complete bytes. Release `1bd3b8164307a543d4f6c9f8cf9811d167f1cefa4c83e7e8f23fc012ab149ece` did so for all ten retained ALAC, AAC and ALAC-match-input exports (`local-artifacts/hash-parity/mp4-controlled-clock-omp-01.json`), and the final release repeated it. Normal processing keeps the AudioToolbox clock; the native clock-sampling phase is not observed.
- A generated 32 kHz AAC match-input case made the original publish an invalid 28-byte M4A after a warning. The CLI refuses to publish that output by design; the user selected this safety behavior rather than reproducing the invalid file.
- A replaced file's marker stores the absolute backup path, so replacement output depends on that path. The single native W04 replacement and the candidate's differed only in that string; every other decoded marker field and every non-marker chunk matched. No final-build replacement at the native backup path was compared.
- The original's normal picker disabled generated `.ogg` and `.oga`, Add Folders omitted them, and later selected drags and explicit-app file opens admitted neither. A supported WAV control was admitted by drag and processed through explicit-app handoff. These are bounded application-admission observations, not proof of decoder invocation. Lower-level original/Rust Ogg decoders matched 96 generated cases; that does not establish whole-application Ogg processing.
- Additional inputs, templates, correction/compression modes, rates, output formats and process contexts require their own repeatable, complete-file comparisons.

These limits also apply if individual component comparisons or tests pass. Keep backups and use separate output directories when processing files; see the [CLI contract](../research/CLI_CONTRACT.md) for replacement, restore and nil-marker behavior.
