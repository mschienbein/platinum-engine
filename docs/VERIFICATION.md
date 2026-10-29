# Verification and evidence boundaries

The project uses complete-file identity as its reference-comparison criterion: audio, container headers, encoder output and metadata all count. Component tests and decoded PCM comparisons help locate differences; they do not substitute for a complete-file match.

## macOS reference tests and CI

The default macOS build uses that system's Accelerate and math libraries, as
the original application does. Tests asserting exact native values are tied to
the recorded Apple M4/SME and macOS reference environment; another Mac or OS
release can produce different bits without a build failure. The release battery
runs those default-mode reference tests on the reference Mac.

GitHub's macOS CI builds the default release and compiles its all-target tests
without running them. It then runs the `portable` all-target suite with
`--no-fail-fast`, excluding the `portable_math` `mac_reference::` module and
the `portable_vdsp` and `portable_vforce` `system::` modules, which compare
against the live host's math or Accelerate libraries. CI inventories test
names first and fails if those filters would exclude unrelated tests. These
reference-platform comparisons run on the M4 Mac in the release battery.
The portable math and vector implementations model the recorded M4 reference;
finite test coverage does not establish universal cross-platform parity.

## v0.2.0 changes and evidence

v0.2.0 adds one explicit opt-in and one reporting contract and pins one native
call context; it changes no processing numerics.

- **Opt-in non-identical paths.** `parity.allow_non_identical` (default off)
  enables, on Linux and `portable` builds only, an open-source deterministic
  MP3 decoder and the byte-for-byte copy of embedded cover art. These two paths
  are **not** claimed to match the macOS reference: the decoder equals
  AudioToolbox on 95.08 % of 46.1 million samples of a 389-file generated corpus
  with a 4-LSB maximum difference (identical on macOS arm64, Linux aarch64 and
  Linux x86_64), and a copied picture is not what the macOS bridge writes. Each
  affected item lists the path in `report.non_identical_paths`. With the setting
  off, Linux behavior is the v0.1.0 identical-or-refuse behavior; on a macOS
  default build it has no effect.
- **Error codes.** Every failed item, batch error and top-level error carries a
  stable `code`; the table is in the [CLI contract](../research/CLI_CONTRACT.md#error-codes).
- **Unchanged.** Exit statuses, the command line, no-clobber and atomic
  publication, ALAC, and AAC refusal on Linux (now with a code).
- **macOS AAC call context.** AudioToolbox's AAC encoder runs its 512-point DFTs
  with one of two kernels (SME or NEON) that it selects from the stack-pointer
  residue (`sp` modulo 64) at the call: `ExtAudioFileWrite` takes the SME kernel
  when the residue is 48, and the final drain inside `ExtAudioFileDispose` when
  it is 32. An intermediate v0.2.0 build whose engine frames had merely changed
  size produced different final AAC packets from v0.1.0 for 6 of 13 synthetic
  inputs (a control build of the v0.1.0 source with an unrelated 48-byte local
  reproduced the same bytes). Those two calls now go through a small aarch64
  trampoline (`call_with_sp_residue`) that fixes the residue at 0, so both run
  the NEON kernel, the one used by the v0.1.0 executable and by the four native
  AAC-to-AAC references (the controlled-clock exports described under "AAC
  decoder stack-context regression" below). The interleaved read list is a
  separate, 64-byte-aligned wrapper (`#[repr(align(64))]`) and does not use the
  trampoline. In scratch builds the 13 AAC outputs and the AAC waveform stayed
  equal to v0.1.0's under six frame-size perturbations and eight
  process-environment sizes, and the four native references still match. This is
  an observation of the tested macOS arm64 AudioToolbox, not a cross-version
  guarantee; the evidence is in `local-artifacts/release-v0.2.0/aac-layout-control-01/`.

The evidence for this release is kept separate by claim, and ignored tests are
never counted as passes:

1. *Default-off equivalence on macOS.* A generated input set (16/24-bit WAV at
   44.1 and 48 kHz, mono, clipped, noisy and silent material, AIFF, and FLAC,
   ALAC, AAC and MP3 inputs) was processed with the v0.1.0 macOS executable and
   the v0.2.0 executable using the same libraries, every output format, the
   Official template and a fixed MP4 header second. Output files and the JSON
   outcomes were compared after removing only the members v0.2.0 adds.
2. *Default-off equivalence on Linux.* The same refusals, with the same messages
   and now with codes, for MP3 and AAC input, AAC output, unreproduced
   sample-rate conversion and artwork that needs Apple image transcoding.
3. *Opt-in end to end on Linux.* Generated MP3 inputs and cover-art sources
   processed with the setting on; the reported `non_identical_paths`, the
   byte-for-byte picture comparison and the decoder's own tests.
4. *Error codes.* Unit tests for every typed mapping and stage label, CLI
   scenarios per reachable code, and a test that keeps the documented table,
   `ErrorCode::ALL` and the scenario ledger identical.
5. *Release builds and test counts* per platform, recorded at the source freeze
   and listed in the next section.

## v0.2.0 release evidence

Source freeze `freeze-02` has source-manifest SHA-256
`f6ff459965e75967095c9ccd9bfa05f8e73f0e8fdcbd6ed176d8731e1fdf3483`. Its
executables, all built from that source, are:

| Platform | Executable SHA-256 |
| --- | --- |
| macOS arm64 (privacy-remapped release build) | `75c2ac875f39bacac357544a704b821a489bc306935da85c0c5feff478d39260` |
| Linux aarch64 | `7f2fea4fd1fee5ee265fc63f809c868cea6179f3293ace4faaa1ac69b1e4a56d` |
| Linux x86_64 | `eb62ec42c73f233c94dd1e855655c1cc4548756acab743fe3b37abecf02275d0` |

The macOS executable was rebuilt independently from the exported public tree with
the recipe in [BUILD.md](BUILD.md) and matched byte for byte; both Linux
executables were likewise rebuilt from the public export in the native-library
images and matched. These are the executables in the release archives.

**Source-freeze tests** (offline, locked, all targets; ignored tests are never
counted as passes):

- macOS default build: **512** passed, **0** failed, **190** ignored across 49
  suites (516 passed if every `test result` line is counted). The opt-in lanes
  passed 38/38 public-dependency tests, 1/1 waveform native replay, 1/1 Xing
  unknown-byte oracle and 8/8 `error_codes` tests that need the macOS bridge.
- Linux aarch64: **469** passed, **0** failed, **126** ignored (49 suites), plus
  the library-gated lane (`error_codes`, `linux_opt_in`, `portable_art_copy`
  with `--ignored`) **28** passed, 0 failed (3 suites).
- Linux x86_64: **467** passed, **0** failed, **124** ignored (49 suites), plus
  the library-gated lane **28** passed, 0 failed (3 suites).

**macOS battery for `75c2ac87…`** (receipt `release-final-75c2ac87.json`, overall
status bounded-complete-file-match-with-documented-context-limits, 0 failures):
71/71 retained replay cases; 8/8 dynamics and pitch cases; 10/10 MP4 exports
including the four native AAC-to-AAC references; the Xing E05 case; 5/5
boundary cases; 11/11 strict family cases (byte-identical in a
relocation-aware review: the native and candidate output folders differ and
only that setting is substituted); 6/6 coverage extensions; 5/5 misnamed-Ogg
cases (u02; the FLAC case is a documented native failure with no output on both
sides); 4/4 failure inputs (f01; two matching native failures with no output);
the W04 exact-path workflow, the CLI workflow smoke, gauge and waveform
checks. The 13 synthetic AAC outputs equal v0.1.0's, and 120/120 output
digests and 131/131 statuses are identical to v0.1.0. Default-off comparisons
against the v0.1.0 macOS executable: 212 records, 0 stable differences.

**Linux battery** (receipt `battery-linux/receipt.json`; per architecture;
native libraries hashed before and after every run and matched by device and
inode in `/proc/<pid>/maps`: of 10,606 observed child processes, 8,488 mapped a
native library, every mapped file matched its pinned digest and none mapped an
unexpected one). With the setting **off**:

| Corpus | aarch64 | x86_64 |
| --- | --- | --- |
| Native references (132 cases) | 104 exact, 25 typed refusals, 3 matching native errors | 104 exact, 25 typed refusals, 3 matching native errors |
| Workflows | 16 matching | 16 matching |
| Differential stress (575 cases) | 535 exact, 40 sample-rate refusals | 535 exact, 40 sample-rate refusals |
| Pitch | 30/30 exact | 30/30 exact |
| Docker Desktop bind-mount subset (separate from the tmpfs main run) | 44 exact, 2 refusals, 1 native match, plus 16 workflows | not part of the x86_64 battery |

Every output hash is identical to the v0.1.0 receipts (0 differences in each
corpus). Default-off comparisons against the v0.1.0 Linux executables: 188
records on each architecture, 0 stable differences. With the setting **on**,
aarch64 and x86_64 agree on every scenario: MP3 160/160, cover art 157/157
(854 cells), error codes 165/165 covering 37/37 codes (31 observed through the
CLI, five not reachable from it, and cancellation exercised with SIGINT).

**Known differences and limits** (none changes a default-path result):

- A RIFF/WAVE file named `.mp3` is processed by content on Linux, with the
  setting on or off; the macOS default build refuses it with
  `invalid_input_file` because the Apple stack chooses by extension. This
  predates v0.2.0.
- `inspect` on a symlink returns `invalid_input_path` as the contract says on
  Linux; the macOS builds return `io_error`.
- W03 early cancellation depends on signal timing; when the first attempt does
  not land in the analysis stage it is reconciled with the separate one-signal
  probe, as in v0.1.0 (the original attempt is retained).
- With the setting off, an MP3 with junk bytes before its first frame and no
  ID3 tag is classified `unsupported_input_format` on Linux, while macOS decodes
  it. The open-source decoder claims only confirmed MP3 streams.
- The direct M4A-to-MP3 picture path is the reference path (not the opt-in
  copy); its ID3 picture has type 0, no description, and `image/png` for GIF,
  BMP and WebP bytes, as on macOS.
- AAC input and output stay refused on Linux; WAV to WAV drops the last-saved
  tag chunk, as the native application does; the x86_64 floating-point
  residual classes described in [LINUX.md](LINUX.md) are unchanged.
- Follow-ups for v0.3.0 include a stricter preflight for very large pictures
  converted to ID3 (about sixteen 16 MiB FLAC pictures can pass the current
  check and still exceed the ID3 size limit; the likely effect is a failed item,
  not a corrupt publication) and hardening flags for the bridge build.

All counts describe the recorded synthetic corpora and environments and are not
a universal parity proof.

## v0.1.0 source freeze

The regression-fixed source freeze has source-manifest SHA-256
`5b05c38952406b2e21e6c5e7397e29d3d9140d3d0963ad675569c5e7548f72d5`
and a private build receipt at
`portable-parity/full-build-v07-regression-fix-01/receipt.json`. In the v07
native-library images its offline locked all-target suite passed **400**, failed
**0**, ignored **97** on Linux aarch64 and passed **398**, failed **0**, ignored
**95** on Linux x86_64, each across 44 top-level suites. The respective Linux
executable SHA-256 values were
`6e19449bc9d492d27fa0068013b4e9d3aeaa7b4fe915a3abc6285ffbe00e289e`
and `1c34e59b586b0f6adfa394b858cbdd2d3abb3d675cd8711c3a6e443c7c7ddfff`.
The earlier source-path-remapped optimized macOS binary SHA-256 was
`82d8b48904f8d6128b7399330556ea13d130f01426d86041db1d974676d49b0d`.
These architecture-specific binary hashes and test results do not themselves
establish complete-file parity. The freeze's full-tree manifest records the
tree at that point, not later documentation or export-related edits.

### Shipped macOS identity-gated battery

The v0.1.0 macOS executable SHA-256 is
`198e0091a212e8af05e0934b1249fd698bfa2300f643e3b430d2b188d30913b8`
(LC_UUID `4D4554E4-7EE2-39F1-9681-CDF11CC1A5A2`). It was rebuilt from the
same compile-time sources with additional home, Cargo and Rustup path remaps;
the earlier `82d8b489…` build already remapped the source path. The binaries
are not byte-identical, so the new executable received its own battery rather
than inheriting a byte-equivalence claim. The build export differed from the
previously verified export only in release workflow and Linux documentation;
later CI and documentation changes are not represented by that build manifest.

The public-export default-mode all-target suite passed **440**, failed **0**,
and ignored **179** tests across 42 suites. A separate selected public-library
run passed 38 tests, and the waveform replay and Xing caller-byte oracle each
passed one. These runs overlap and ignored tests are not passes.

Without normalizing bytes, the new binary matched **71/71** selected stable
files, **8/8** dynamics/pitch cases, **10/10** MP4 exports at their recorded
header seconds, 15 boundary files, all six x01–x06 extension cases, the Xing
E05 case and W04 exact-path replacement/restore. Nine family cases matched
only in the documented **path-relocated context review**; the original strict
toolkit rejected the different output folders. The supplemental workflow,
gauge and waveform checks matched; timed W03 cancellation landed in healing
rather than the recorded native analysis stage, while an early cancellation
reached analysis. The native application was not launched or controlled.

The shipped bridge and LAME libraries use neutral install names and ad-hoc
signatures. Their SHA-256 values are
`14ca6f03b65a2cb0c4bc8bd06df540ae1707796f59f2d4f117a5d33447dfd79e`
and `2986f34cca6a4fbfb05934ac33b4bb919c8c47de1594e227b2f0fa4c930b377f`;
libFLAC remains
`d9b3a64df9eeab7556aad9877492ad07ba51e6459e3725f90b0a9203078ac6a5`.
The bridge and LAME section and non-metadata segment bytes were checked
against their prior libraries; changes are confined to install-name/load-command
layout and code-signature-related data. The CLI and these libraries were
checked for host-specific home-directory strings.

The private primary and supplement receipts are
`hash-parity/release-final-198e0091.json` and
`hash-parity/release-final-198e0091-supplement.json`, SHA-256
`566c93b7f848ede1f2846221218b1a587b2894b84206dc810f0a21e293f2c7a4`
and `35e1efaa9d7c3ab7911c6f07f61448bfa246625570468fbd4a27e327919029bb`.
These finite, overlapping synthetic cases do not prove universal parity.

### Earlier macOS identity-gated battery

The earlier `82d8b489…` binary (LC_UUID `5348303E-C012-3ECD-9023-D9A444C57F73`) passed
**440** top-level regular tests, with **0 failed** and **181 ignored**, in
44 suites. A separate selected public-dependency run passed 38 tests; the
native waveform replay and Xing caller-byte oracle each passed one. The
original Mac receipt aggregated four nested filtered subprocess summaries and
reported 444/48; the create-only count addendum corrects this to 440/44
without changing the original receipts, test exit or binary identity. Counts
overlap and ignored tests are not passes.

Without normalizing file bytes, the binary matched 71/71 selected stable cases,
8/8 dynamics/pitch cases, 10/10 MP4 exports using each recorded header second,
15 boundary files and all six x01–x06 extension cases. The retained Xing E05
case and W04 exact-backup-path replacement/restore comparison also matched.
Nine family cases matched in a **path-relocated context review**; both c04
ALAC exports matched separately at their recorded seconds. The unmodified
strict toolkit rejected the different output folders; the adapted review
validated both paths and substituted only that setting. It did not alter
native observations or output bytes. This is not the earlier `dd4b5fd0`
identical-path strict-context result. These sets overlap and cover only the
tested synthetic cases.

The supplemental workflow replay matched the recorded gauge/statistics and
waveform checks; timed W03 cancellation landed in healing rather than the
historical native analysis stage. The native application was not launched or
controlled during this battery. The primary, supplement and counts addendum
are retained under the private evidence root as
`hash-parity/release-final-82d8b489.json`,
`hash-parity/release-final-82d8b489-supplement.json`, and
`hash-parity/release-final-82d8b489-counts-addendum.json`, with SHA-256
`d3ea14d5064b02f9b390755f634f1dd0716706d9741ef2ff2938702939b93b1a`,
`33add854616ac356fc33759ce04fcc308540c92a83b4db3b8dde2eba22381588`, and
`7cb2eca91c0ffcdaacc58c7336d808c5d8f9619356429335180823641ab5a434`.

### Linux end-to-end battery

The regression-fixed source snapshot was tested on both Linux architectures.
Per architecture, 132 native-comparison cases comprised **104 exact complete
file matches**, **25 documented typed refusals**, and **3 matching native
error/no-output cases**. Each architecture matched all 16 recorded workflow
cases. Refusals and no-output outcomes are not counted as byte matches.

For a separate 575-case stress corpus, the macOS default build produced 575
outputs; macOS portable, Linux aarch64 and Linux x86_64 each had **535 exact
byte matches and 40 documented refusals**. All four platforms matched all
30 supplemental pitch cases. A separate Docker Desktop bind-mount subset
reported 44 byte matches, one native match, two documented refusals and 16
matching workflows; it does not replace the main Linux tmpfs battery.

There were no unexplained mismatches in these runs. Low-level and nonfinite
inputs were not instrumented, so successful files do not establish that the
x86 FTZ or NaN-sign boundary was exercised or repaired. The private index
`portable-parity/linux-battery-01/receipt-final-regression-01.json` has SHA-256
`c0d49ffb934e13074fc61c0d2eb0ad700973cb8821016a93ab7c4dd234dc498b` and
links the individual native, stress, pitch and bind receipts. These finite,
overlapping synthetic cases are not a universal parity proof.


## Historical v07-fixes source build

That pre-regression-fix freeze's original source-manifest SHA-256 is
`dc82b4a030c620c1d3b80a495078613c2736690dfbea251fbd8c5ca26fcceb55`.
In the v07 native-library images, its offline locked all-target suite passed
**398**, failed **0**, ignored **97** on Linux aarch64 and passed **396**,
failed **0**, ignored **95** on Linux x86_64; each had 44 top-level suites.
The Linux executable SHA-256 values were
`8954dd025e34395ceda24cabcfbbd3687677ab236199d6b96d097821e6775bf7`
and `e7b837ecb4ed4873ba23791a24d7786b3b45a09abeec75527bca2b364972a8dc`.
These architecture-specific binaries and passing tests are not themselves
complete-file end-to-end parity evidence.

The original build receipt counted three nested filtered test subprocess
summaries as additional x86 suites and passes (399/47); the separate
`top-level-test-counts.json` corrects that aggregation to 396/44 without
changing the recorded exit codes or executable hashes. The original receipt
is preserved. Both are under the private evidence root in
`portable-parity/full-build-v07-final-fixes-01/`. Its remapped macOS binary
SHA-256 is `3f2c19c4705ad892e6c03f202416ab25b714b4aaa1b0d6ae8765ed559431f8b6`.
Later documentation and export-related edits are not represented by the
original full-tree manifest; do not describe that manifest as the public
export's identity.


## Historical v07 frozen candidate

The pre-fix, pre-documentation candidate snapshot has source-manifest SHA-256
`855445719d235d4ca14cda8294bd1c8aef2283679e5ea0d967adbd70c8c9d186`.
The remapped optimized macOS executable SHA-256 was
`c9f19077c91fb975f9d375f4aae3c7024becce2416d3211e9ed971e7ad1830a7`
(LC_UUID `5AE44579-7B18-3355-83BD-AFBEAAEF5E9E`). Its frozen private
all-target suite passed **426**, failed **0**, and ignored **179** tests
across 44 suites. A separate selected public-dependency run passed 37 tests;
the opt-in native waveform replay and Xing caller-byte oracle each passed one.
These runs overlap and ignored tests are not counted as passes.

Without normalizing file bytes, this binary matched the retained references in
71/71 selected stable cases, 8/8 dynamics/pitch cases, 10/10 MP4 exports using
each recorded header second, and all six additional x01–x06 cases. It also
matched the retained Xing E05 case and the W04 exact-backup-path
replacement/restore comparison. Nine family cases matched in a **path-relocated
context review**; both c04 ALAC exports matched separately with their recorded
seconds. The unmodified strict toolkit rejected the relocated output folders:
the adapted review substitutes only that setting after validating both paths,
and does not normalize any audio bytes or change native observations. This is
not the earlier `dd4b5fd0` identical-path strict-context result. These case sets
overlap and establish only the tested synthetic cases.

The same source freeze built and ran the offline locked all-target suite on
Linux aarch64 (**386 passed, 0 failed, 96 ignored**) and x86_64 (**384 passed,
0 failed, 94 ignored**) in the v07 native-library images. Their executable
SHA-256 values were `6826ecbed9c463550c65d95c6179c397f7355e40fc3e7daa9487828be7c76005`
and `6ecd7c4e57b9acb1f8cda80da4d1c74447cd501e88c616574a1b6ef42c4094b4`
respectively. These are architecture-specific binaries and test results, not
cross-architecture binary identity or an end-to-end parity battery.

The macOS primary and supplement receipts are retained privately as
`local-artifacts/hash-parity/release-final-c9f19077.json` and
`local-artifacts/hash-parity/release-final-c9f19077-supplement.json`, SHA-256
`c98e64e8be4faeb9b954a198ce2e696506b59526e64f4085395daace21a8c3b4`
and `a175a21d0aaf6e9f96a94a579d7793c275e6b2b39c38bb9d082b3ab853609729`.
The Linux build receipt is `portable-parity/full-build-v07-registry-stage-01/receipt.json`
under the private evidence root. The frozen inventory predates the subsequent
documentation, notices, export list and package-notice wording changes; those
edits must not be represented as part of the original frozen manifest.


## Portable component evidence

These are component or codec comparisons from separately pinned snapshots, not
additional complete-application passes. Their sets overlap; do not add their
counts to the release battery. The Accelerate and math component reference
records Apple M4 with SME, macOS 27.2 build 26B123, round-to-nearest, the
specified FZ modes and the aligned-0 FFT profile.

| Component | Bounded observation |
| --- | --- |
| Math | 46 components × 131,072 inputs per FZ mode replayed on macOS, Linux aarch64 and Linux x86_64 without mismatches; Linux raw-bit sampling is not exhaustive. The math receipt also records separate exhaustive macOS binary32 sweeps. |
| ARM scalar helpers | The three-host oracle compared 100,000,000 random plus 32,768 special triples per precision/FZ with zero helper mismatches. A separate x86 vectorized-call-shape replay recorded 210 FMA NaN-payload mismatches, a documented residual rather than a pass. |
| vDSP | Eleven element routines, 686 lengths and both FZ modes were sampled without failures; a three-host non-FFT corpus matched 54,342,720 words per FZ mode. Profiles, lengths, strides and aliases are limited to those in the component receipt. |
| vForce | Mac `vvlogf`, `vvexpf` and `vvsincosf` sweeps covered all 2^32 binary32 inputs in specified body and tail classes, both FZ modes, without mismatches. Linux replay used finite general and boundary corpora, not exhaustive Linux sweeps. |
| FFT | The aligned-0 Mac corpus used 10,000 random and seven structured vectors per size, direction and FZ; each Linux architecture replayed 3,052 vectors/28,350,464 words with zero mismatches. SME complex transforms separately used 100,000 random plus 12 structured vectors per size/direction/FZ, with zero recorded word mismatches. |
| PCM and ALAC | Per Linux architecture, the audio snapshot recorded 9,540 reader cases/179,610,900 sample-bit comparisons, 2,424 PCM writer cases/90,494,042 file bytes, 864 ALAC reader cases and 1,589 ALAC writer cases without mismatches. An ALAC one-sample mono/16 sweep matched file lengths and hashes for all 73,951 integer rates 22,050–96,000; 2,240 fractional-rate cases comprised 2,095 matches and 145 expected native-loss refusals. Sample contents and fractional rates were not exhaustive. |
| GSM | The three-host optimized snapshot matched 117 packet cases, 42,258 blocks and 13,522,560 samples; its 12,288-mask reader corpus had zero mismatches and 179 reserved-mask cases were typed refusals. Later audio source replay coverage differs by platform as recorded in its inventory; the accepted mask subset is not universal. |
| FLAC/LAME | Per Linux architecture, codec-03 matched 540 FLAC and 320 MP3 retained cases, 7,488 FLAC and 2,880 MP3 expanded cases, plus a 2,988,408-record one-frame FLAC rate/channel/depth sweep. Codec-04 additionally matched 5,244 FLAC stress/recovered-PCM cases per architecture. These are codec-only results, not all possible streams or full pipeline outputs. |
| Bridge v07 | Each Linux architecture matched 604 transfers, 86 inspections and 232 source-status cases; 37 fault runs and nine WAV permission/umask cases had no reported failures. The macOS native-versus-portable matrix covered 690 cases, including 112 expected artwork refusals. Status/refusal evidence is not the same as a file-byte match for an accepted output. |
| MP3 postflight | The final-source gate matched ten stream-description/length fields for 17,120 unique files, including selected final-battery outputs, with zero mismatches. For v0.2.0 the `Info`-tag estimate was corrected for variable-bitrate output (`src/mp3.rs` SHA-256 `7eda250126953070837eca28fabb8fd1659078e4c5947b1f8cdd997b3fc05a7b`): the 17,120 prior rows were reconciled by SHA (17,080 files run) and 1,166 new Info-VBR, Xing and erased-identifier engine outputs were compared, all with zero mismatches (506 of those 1,166 mismatched before the fix; `local-artifacts/release-v0.2.0/mp3-info-vbr-01/REPORT.json`). This tests postflight fields, not complete-file encoding equivalence. |

The component reports retain their source pins, commands, historical failures
and exact scope in the private evidence inventory. The published source includes
only the reviewed compile-time coefficient table and two compact FFT test
fixtures, not the private corpus. A different Apple Silicon generation, OS,
Accelerate dispatch, allocator alignment or AudioToolbox stack context may
change the reference. On x86, ordinary FTZ tininess, default-NaN sign and NaN
selection including vectorized FMA order remain known residual classes; no
claim is made that the runtime detects every affected input. Linux aarch64 is
the stricter build. See [LINUX.md](LINUX.md) for refusals and loader limits.

Review rounds proceeded through ReviewMacDefault, ReviewPortable and
ReviewSecurity, fixes, ReReview, FinalReview and LastRegressionReview.
FinalReviewEngine's verdict for its reviewed snapshot was
`PASS_WITH_QUALIFICATIONS`, not an overall release or security sign-off. Its
conditional MPEG/MP4 classification finding was addressed by validating
atom sizes and structure; six decoder-admitted synthetic collision cases
matched the review baseline. LastRegressionReview's ACL and restrictive-umask
findings were addressed in FixWorkflow-regression-04 and the subsequent
frozen build. These observations remain bounded by their individual receipts.

FinalReviewEngine also recorded accepted limitations for intentionally
conflicting marker representations. The bridge selects the first relevant
GEOB or MP4 freeform item and prefers a FLAC Xiph marker over ID3; a later
processed marker may therefore be concealed. Sync-like junk or nonadjacent
ID3 tags may also hide a marker from prefix discovery. These are source-derived
scenarios, not executed proof that the exact crafted inputs are decodable or
that native behavior is identical. The CLI contract documents their scope.

The v07 review changed bridge handling so read/seek errors do not become a
source-container-unavailable status or cause a destination callback, and
verified WAV rewrite modes under nine mode/umask combinations per platform.
Other review rounds addressed transaction/overwrite and restore safety,
container sniffing, duplicate ID3 markers, descriptor pinning and postflight
validation. Their bounded checks and the earlier four-review history do not
imply that adversarial concurrent modifications are impossible: a library file
writable by another process is not made immutable by hashing or path pinning.
Native library initializers may run before a post-load hash detects a change.

Earlier unpatched Linux libFLAC builds differed on 24 retained cases; codec-03
matched those cases after the FP-order/math corrections. Codec-04 traced 12
later reported ARM battery differences to an obsolete pre-patch library and
replayed those complete files successfully with codec-03. This is a
build-provenance explanation for those cases, not proof of arbitrary FLAC
identity. ALAC's later hostile sample-table review also rejected eight compact
malformed fixtures on all three hosts before attempting an impossible index
allocation. The bridge v07 directory-as-MP3 source case now fails instead of
preserving the older bridge's accidental success; the CLI accepts regular
source files only.

An earlier `v0.0.0-dryrun` distribution exercise built packages for macOS
arm64 and both Linux architectures, verified the extracted package checksums,
and rebuilt both Linux native-library sets from the bundled sources with
matching library hashes. One 8,192-frame silent WAV produced the same bytes on
all three platforms. This was a pre-integration packaging experiment, not the
v0.1.0 release package, universal parity evidence or legal clearance; no
release or GHCR image was published by that exercise.

### Correction to f01–f03 process time

The original f01/f02/f03 observation files for native PID 99320 contain the
incorrect parenthetical UTC launch time `(2026-10-27T21:36:13Z)`. The correct
launch time is **2026-10-28T12:27:07Z**, corresponding to `lstart Wed Oct 28
07:27:07 2026 CDT`. This note corrects the annotation only; the original
observations are left unchanged and no missing context is inferred from it.

No new native GUI drag-and-drop admission was tested in this release cycle
without desktop-control approval. Picker and explicit open-route observations
were made. Earlier bounded Ogg/OGA drag observations are retained below; they
do not establish drag admission generally or imply a new test in this cycle.

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

That later archived source snapshot, including statistics and waveform changes, was
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

## Historical release build `dd4b5fd0`

The then-final source added `process --overwrite` and consumed the backup after a
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

The `dd4b5fd0` binary also repeated the recorded candidate workflows. The W01 mixed
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

## AAC decoder stack-context regression

Adding a source-tag status field to the process report changed the caller's
stack layout. On the tested macOS arm64 runtime, the same synthetic AAC input
then decoded to different float32 samples inside `ExtAudioFileRead`, before
any healing DSP. In a controlled debugger intervention, shifting only the stack
pointer by 48 bytes at that call changed 83 of the first 512 decoded samples;
the input, output buffers and FPCR were unchanged. Which AudioToolbox internals
cause this is unverified.

The planar reader now keeps its live `AudioBufferList` 64-byte aligned, which
fixes the stack alignment at the read call regardless of the callers, without
changing the list ABI or the sample buffers. The E04 and E10 AAC-to-WAV cases,
tagged AAC-to-MP3 and four controlled-clock AAC-to-AAC exports then matched
their native references as complete files, and previously matching format and
DSP controls were replayed unchanged. These are bounded observations, not a
guarantee for other compiler or macOS versions. Investigation receipts:
`local-artifacts/hash-parity/aac-regression-01/`.

An opt-in regression test checks exact decoded sample words for the pinned
generated `aac-repeat-01.m4a` fixture:

```sh
PN_AAC_REGRESSION_SOURCE="$PWD/local-artifacts/apple-codecs/aac-repeat-01.m4a" \
  cargo test --offline --locked --release --test apple_codec_reference \
  pinned_aac_planar_decoder_samples_match_release_context -- --ignored --exact
```

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

Tests marked `#[ignore]` are excluded from that command. Some need separately
built public libraries, additional public utilities, native captures or
separate oracle programs. Do not infer that ignored cases passed. Read each
test's prerequisites before opting in; running every ignored test is not a
self-contained public test suite. For example, after building a trusted macOS
v07 bridge as in [BUILD.md](BUILD.md), one generated-input bridge test can be
selected explicitly:

```sh
PN_TAG_BRIDGE="$DEPS/libpn-tag-bridge.dylib" \
cargo test --locked --test engine_reference \
  inspection_and_analysis_use_real_source_and_distinguish_malformed_markers \
  -- --ignored --exact
```

These integration tests use generated files and exercise their individual conditions; they do not establish complete original-app parity. Some regular tests also overlap with other reported runs, so do not add test counts as if all executions were distinct.

The earlier `dd4b5fd0` private development snapshot passed 317 regular tests and left 92 opt-in tests ignored. Its curated public snapshot passed 317 regular tests with 90 ignored; it excluded the two private diagnostic targets `aac_32k_fail_closed.rs` and `aac_writer_warning_probe.rs`, each holding one ignored test. Its public-dependency engine/codec/CLI/statistics run passed 20 selected tests, including marker admission, no-clobber, replacement/restore and post-publication accounting cancellation paths. These are historical counts, not results for a later source snapshot.

A separate opt-in replay consumed three captured native waveform-component input/output sequences and matched every float word. Four generated PCM16 WAV decoder cases (mono/stereo at 44.1/48 kHz, requesting 44.1 kHz) also matched every returned float byte and read boundary. The actual release CLI's waveform bins match the composition of those captured decoder words and the independently tested reducer. This is not a direct full-file GUI waveform capture, and other formats remain unverified on that path. Four historical pitch checkpoint comparisons and 96 generated Ogg decoder comparisons passed against captured or signed original components; none establishes full application parity.

## Public source boundary

The public repository starts with independent history. It includes Rust and portable sources, public TagLib bridge sources, synthetic tests, the reviewed FFT coefficient table and compact test fixtures, Linux build recipes, build instructions, typed preset parameters and the transformed human-readable template configuration required by the CLI. It excludes original serialized preset blobs, extracted compiled resources, screenshots, decompiler/disassembly exports, native libraries, audio captures and private research history. Optional reference tests may contain authored probe code or expect separately supplied captures; their presence does not mean those captures or proprietary components are distributed or needed by the production CLI. Static reconstruction and measured values are reference-derived, not a clean-room or licensing-clearance claim.

Some source comments and explicitly ignored diagnostics intentionally name
`local-artifacts` paths for private evidence. The ignore file and developer
dual-host harness also name that directory to exclude generated data and
constrain scratch outputs. These are historical locators or safety rules, not
distributed fixtures or prerequisites for the regular public suite. The
public files do not contain the private artifact directory.

## Remaining limits

- The original pitch-correction splitter's output depends on its FFT scratch alignment. Real/imaginary scratch at 64-byte offset 0 gives one output profile; offsets 16, 32 and 48 give another, identical to each other. Every retained native pitch-active export in the five alignment-sensitive cases (D05, Big Boost, D04 and preserve-negative r10/r11; ten GUI exports) matches only the aligned-0 profile that Rust pins. Turntable cases do not distinguish the profiles. The splitter allocates through a 16-byte-aligned `malloc` callback, and 240 isolated processes repeating its allocation sequence never produced offset 0 for real/imaginary. The GUI therefore did not show plain-malloc behavior in any observed run, but its literal pointers and any callback override were not observed (`splitter-fft-alignment` in the private evidence index above).
- A Xing MP3 path without a recognized LAME tag makes the native updater read a reused caller byte. Static analysis shows that byte is part of the limiter AudioUnit handle and is nonzero on the in-process AudioToolbox creation path, so the CLI applies the nonzero branch. With that change the release CLI reproduced the retained original A/B/C export for the generated E05 case (644,959 bytes, SHA-256 `12b12b9ddff607fc9f82e38a25dd212518a6cbd462dc6aa3f44f79c7cf16971d`). The byte was not observed in the running app; see `local-artifacts/hash-parity/xing-nonzero-cutover-omp-01.json`.
- In one interleaved AAC fixture, original MIKAudio and direct AudioToolbox reads matched within each process but produced different bits across fresh processes with the same recorded conditions. Rust and direct reads also matched in a same-process diagnostic. By user decision, the decoding target is the native same-process result; the cross-process cause remains unknown.
- A separate direct-system AAC experiment compared untouched FPSR against clearing it before each read: 48 readers across 12 fresh processes all produced identical float streams, despite nonzero untouched FPSR values. It did not reproduce or explain the historical variability; the original app and MIKAudio wrapper were not used.
- AAC and ALAC outputs store one wall-clock second in all six `mvhd`/`tkhd`/`mdhd` creation and modification fields, so native repeats differ from each other. By user decision, an MP4 case passes when the CLI, given a native export's recorded second through the hidden `--controlled-mp4-time-1904` diagnostic option, reproduces that export's complete bytes. Historical release `1bd3b8…` did so for all ten retained ALAC, AAC and ALAC-match-input exports (`local-artifacts/hash-parity/mp4-controlled-clock-omp-01.json`), and `dd4b5fd0…` repeated it. Normal processing keeps the AudioToolbox clock; the native clock-sampling phase is not observed.
- A generated 32 kHz AAC match-input case made the original publish an invalid 28-byte M4A after a warning. The CLI refuses to publish that output by design; the user selected this safety behavior rather than reproducing the invalid file.
- A replaced file's marker stores the absolute backup path, so replacement output depends on that path. The single native W04 replacement and an earlier candidate differed only in that string; every other decoded marker field and every non-marker chunk matched. The `dd4b5fd0` build did not compare replacement at the native backup path.
- The original's normal picker disabled generated `.ogg` and `.oga`, Add Folders omitted them, and later selected drags and explicit-app file opens admitted neither. A supported WAV control was admitted by drag and processed through explicit-app handoff. These are bounded application-admission observations, not proof of decoder invocation. Lower-level original/Rust Ogg decoders matched 96 generated cases; that does not establish whole-application Ogg processing.
- Additional inputs, templates, correction/compression modes, rates, output formats and process contexts require their own repeatable, complete-file comparisons.

These limits also apply if individual component comparisons or tests pass. Keep backups and use separate output directories when processing files; see the [CLI contract](../research/CLI_CONTRACT.md) for replacement, restore and nil-marker behavior.
