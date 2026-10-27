# Platinum Engine CLI contract

Status: the Rust CLI implements this command surface on macOS arm64. Two Platinum Notes 10.5.0.1256 reference exports and Rust debug/release outputs matched complete bytes for one generated 16-second, 48 kHz, stereo 16-bit input using the Official template and WAV output. The common SHA-256 was `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b`. Commands and options do not establish equivalence for other inputs, settings or execution contexts.

## Product boundary

The project targets the current application's audio and file-workflow capabilities in a headless engine, not a recreation of its windows. It does not claim complete feature parity. The original scratch-alignment-dependent pitch behavior, one uninitialized-byte-dependent MP3 header path, and an interleaved AAC diagnostic remain unresolved beyond the observed profiles. Ogg support is not established.

Production processing uses Rust DSP, macOS AudioToolbox/Accelerate and explicitly configured public native libraries. It does not call the original application or its proprietary components; those are reference-only and optional. macOS arm64 is the current supported build/runtime scope, not a portability claim. The [build guide](../docs/BUILD.md) covers the bridge, optional codecs and isolated configuration.

## Commands and capability mapping

| Command family | Required behavior |
| --- | --- |
| `process INPUT...` | File and folder batches; selected built-in/custom template; original conditional processing stages; format/output/backup policy; bounded concurrent jobs; cancellation; truthful per-file results. |
| `analyze INPUT...` | First-pass level and pitch analysis, processed-marker inspection and waveform data. Clip count and dynamics may be reported from an existing marker, not measured as new repair/dynamics results; unavailable values are explicit. No encoded replacement output. |
| `templates list/show/select` | Enumerate built-in and user templates, show effective and serialized settings, select by stable identity, and preserve original fallback precedence. |
| `templates clone/edit/rename/remove` | All editable processing fields, name and description; original built-in mutability rules; persisted custom-template identity. |
| `templates import/export` | Interoperate with the observed `.pntemplate`/XML contract, including missing-field and conversion rules. Unknown fields and lossy conversions must be reported. |
| `inspect FILE...` | Format/rate/channels/bit depth, tags/artwork, processing marker, original/output/backup paths and status. |
| `restore FILE...` | Restore original audio from its recorded backup using an explicit, verified source/destination pair; report partial failure instead of pretending success. |
| `config show/get/set/reset` | Persistent non-account preferences, validation, and explicit effective values. |
| `help`, `version`, source/provenance information | Document supported operations and the reference compatibility target without impersonating the original vendor. |

## Command syntax

`platinum [--state-dir DIRECTORY] [--json] COMMAND ...` uses its own state,
not Platinum Notes' preferences. The global options can appear alongside a
subcommand. `--help`, `COMMAND --help`, `--version`, `version`, and
`provenance` describe the interface and target; no vendor activation or
account is involved. `--state-dir` is the complete state directory, useful for
isolated runs. `HOME` supplies the default output path even with `--state-dir`.

```text
platinum process INPUT... [--jobs N] [--template-index N | --template-id ID]
    [--format wav|aiff|alac|flac|mp3|aac|match-input]
    [--output-dir DIR | --same-folder | --replace-originals --backup-dir DIR]
    [--mirror | --no-mirror] [--mp3-mode cbr320|vbr192]
platinum analyze INPUT... [--jobs N] [--template-index N | --template-id ID]
platinum inspect FILE... [--jobs N]
platinum restore FILE... --backup ABSOLUTE_PATH [--backup ABSOLUTE_PATH ...]
platinum templates list
platinum templates show|select|clone|remove (--index N | --id ID)
platinum templates edit (--index N | --id ID) FIELD JSON_VALUE
platinum templates rename (--index N | --id ID) NAME
platinum templates import FILE
platinum templates export (--index N | --id ID) FILE
platinum config show
platinum config get FIELD
platinum config set FIELD JSON_VALUE
platinum config reset [FIELD]
```

Template manager indices are zero-based with built-ins first; IDs are exact,
case-sensitive, and must be unambiguous. `show` reports effective values and
the exact serialized bit representation. Non-finite effective floats are
shown as `"NaN"`, `"Infinity"`, or `"-Infinity"`; their exact bits remain in
the serialized view. `edit` accepts all fields and value types listed below, e.g.
`platinum templates edit --index 3 'expansion.bands[1].threshold' -12.5`.
JSON strings need shell quoting, e.g. `platinum config set output.format '"wav"'`;
booleans, numbers, `null`, arrays and objects are also JSON values.
The output settings, codec library locations and digests, and selected
template are configurable using the field paths below. Import and export
report ignored fields and losses; export never replaces an existing file.

Process flags override the loaded settings for that invocation only. The
dedicated default output folder is created if needed; any other selected
`--output-dir` must already exist. `--mirror` and `--no-mirror` apply to a
specific output folder. Replacement always
requires **both** `--replace-originals` and an explicit existing `--backup-dir`
on every invocation, even if replacement is persisted in the configuration.
Replacement forces match-input; a conflicting explicit format is rejected.
Each backup is copied and verified before publication and retained afterward.
Other outputs never intentionally overwrite an existing destination. Batch
destination and selected-source conflicts are checked before starting workers.
Each worker uses the exact preflight plan and identity; a source changed after
preflight is rejected rather than inspected and planned again.
Restore pairs each file with the correspondingly ordered explicit `--backup`
argument and verifies it against the marker read from that file. The source
identity captured before marker inspection must still match under the restore
reservation. Marker-only restore requires the tag bridge, not an audio decoder;
the backup is not consumed, and supplied backup paths must be absolute.

For a first run, use a disposable copy of an input and an existing separate
output directory. After following the build guide, set `INPUT` and `OUTPUT_DIR`
to your chosen paths; `PN` and `PN_STATE` are the variables from that guide:

```sh
"$PN" --state-dir "$PN_STATE" --json process "$INPUT" \
  --template-index 0 --format wav --output-dir "$OUTPUT_DIR"
```

Replacement changes the source file. Retain an independent backup first, set
`BACKUP_DIR` to an existing backup directory, and use the explicit flags
instead of `--output-dir`:

```sh
"$PN" --state-dir "$PN_STATE" --json process "$INPUT" \
  --replace-originals --backup-dir "$BACKUP_DIR"
```

Read the per-file JSON result and retain its `output_path`, `backup_path` and
`processing_marker_status`; do not guess backup names or assume that a failed
transaction left no published output. When the marker contains the recorded
backup, set `PROCESSED_PATH` and `BACKUP_PATH` to those reported paths and
restore using the corresponding **absolute** backup path:

```sh
"$PN" --state-dir "$PN_STATE" --json restore "$PROCESSED_PATH" \
  --backup "$BACKUP_PATH"
```

Restore replaces the processed file with a verified copy of the backup and
retains the backup. A separate-output file or a nil marker without a recorded
backup cannot authorize this restore; keep the independent copy and reported
backup for recovery instead.

JSON processing reports expose `processing_marker_status`. `present` means the
stored marker was decoded and checked against the actual processing result.
If Foundation rejects non-finite marker measurements, the native nil behavior
is instead reported as `nil_payload_absent` (ID3, MP4 and FLAC) or
`nil_payload_empty_wav_chunk` (a zero-length WAV `PLTN` chunk). The numerical
measurements remain explicit strings such as `"-Infinity"`, not invented finite
values. A nil result does not record the backup in a readable marker; retain
the reported backup path. Strict marker inspection treats a present empty WAV
payload as malformed because it cannot distinguish that chunk from malformed
input, and marker-authorized restore cannot use a nil marker.

Jobs default to `processor_max`, clamped to available cores; `--jobs` is also
clamped to that maximum. Ctrl-C stops starting queued work and signals active
processing/analysis and restore. Cancellation and publication share a commit
boundary: cancellation winning before commit prevents publication, while an
already-entered commit or rollback completes before cancellation takes effect.
An already-running inspection finishes its current operation. JSON outcomes include per-file statuses and real reports or
errors, with transaction recovery details where available. A non-Unicode
command path is represented as an object containing display text and exact
`unix_bytes_hex`. Exit status is 0 for success, 1 for failure or partial
failure, 2 for command/usage errors, and 130 for cancellation. The status of
each file remains visible when outcomes are mixed.

## Processing controls

Keep the command surface compact. Expose the controls visible in the screenshots, with complete four-band settings in the template model. Preserve internal settings required by the original algorithms and imported templates without inventing a separate switch for every archived control.

- Clipping: enablement, measured-volume gate and threshold percentage. The kernel's headroom remains part of the compatible template/engine state; it is not visible as a separate control in the supplied screenshot.
- Pitch: enablement, cents threshold and both modes: pitch change preserving tempo, or tempo-linked turntable-style change.
- Warmth: no warmth, gentle warmth, hot vacuum tube. Effective option/preset mapping comes from the parser and wrapper, not prose labels alone.
- Expansion and compression independently: enablement and volume gate; four bands with threshold, gain, ratio, attack and release; three crossover midpoints. Additional internal template values stay documented separately.
- Final output: the visible average-level target. Margin and character values are internal compatibility state until their current exposure is established; ten archived labels do not establish ten distinct effects.
- Built-ins: Official, Festival and The Big Boost, interpreted through the actual loading/conversion rules.

The selected analyzer has been recovered statically as K-weighted integrated LUFS-style measurement with 400 ms windows, 100 ms hops, absolute/relative gates and a bank of simulated limiter gains. That is stronger evidence than the older vendor FAQ's “drums only” wording, but it is not a standards-conformance or output-equivalence test. A generic `loudnorm` substitution is not established equivalent. XML values are not necessarily effective internal values.

## Settings and template API

`SettingsStore::in_home` uses `~/Library/Application Support/Platinum Engine/`
(not Platinum Notes' preferences); `SettingsStore::new` accepts the complete
state-directory path plus home and available-core count for isolated use.
`load` returns a snapshot and `save` publishes that snapshot's settings. Missing
state gets defaults; malformed, incompatible, or internally inconsistent state
is an error. Saved custom templates contain the effective JSON representation,
including exact `0x`-encoded binary32 bits, not native XML. The permanent
`settings.lock` coordinates CLI writers: save checks the original bytes under
the lock and publishes an atomic rename with a fresh revision, so stale
snapshots cannot silently overwrite another cooperating writer. This does not
guarantee exclusion against an external writer that ignores the advisory lock.
`AfterPublish` or `PublishedCleanup` means publication already succeeded but a
later directory sync or temporary-file cleanup failed; the latter reports the
remaining temporary path.

Defaults are MP3, `cbr320`, the available processor count, and the first
built-in template. In contrast to destructive replacement, the default output
is a specific folder, `~/Music/Platinum Engine Output`, without mirroring. The
CLI creates that default folder if needed; the settings API itself does not.
Other selected output directories must already exist. Default MP3 output also
requires the separately configured LAME library and digest. The MP3 and
processor-count defaults are evidence-backed; the separate output path is a
deliberate Engine safety choice.

`config_show`/`config_get`/`config_set`/`config_reset` use these exact paths:

| Path | JSON value / semantics |
| --- | --- |
| `output` | `{"mode":...,"format":...}` using `file_workflow::OutputSettings`. |
| `output.mode` | `"same_folder"`, `{"specific_folder":{"directory":"...","mirror":false}}`, or `{"replace":{"backup_directory":"..."}}`. Choosing replacement forces `match_input`. |
| `output.format` | `"wav"`, `"aiff"`, `"alac"`, `"flac"`, `"mp3"`, `"aac"`, or `"match_input"`. An incompatible replacement format is rejected. |
| `processor_max` | Integer clamped to `1..=max(1, available_cores)` on set, load, and save. |
| `mp3_mode` | `"cbr320"` or `"vbr192"`. |
| `libraries` | Object with `flac`, `flac_sha256`, `lame`, `lame_sha256`, `tag_bridge`; each is a string or `null`. Individual paths such as `libraries.flac_sha256` are also writable. A non-null digest is 64 hex digits; FLAC input/output and LAME output loading require caller-pinned digests. The ABI-5 tag bridge is required for processing, analysis, inspection and marker-only restore. Paths and digests are caller-supplied; settings alone does not load the libraries or establish their provenance. |
| `selected_template` | `null`, `{"builtin":{"id":"..."}}`, or `{"custom":{"index":0,"id":"..."}}`; custom IDs can be `null` and must match the indexed object. Prefer `select_template` for validated selection. |
| `effective_selected_template` | Read-only resolved selection; even when `selected_template` is `null`, this shows the first built-in's identity. |

Resetting all preferences preserves custom templates; resetting one field uses
its default subject to the same validation. The selected custom index in the
JSON is zero-based *within custom templates*. For template commands,
`TemplateRef::Index` is instead the zero-based manager order (embedded
built-ins followed by imported/duplicated customs). `TemplateRef::Id` is exact
and case-sensitive; if an ID occurs more than once, it fails and an index must
be used. Imported IDs, including duplicates, empty strings, and absent IDs,
are preserved. Selection by manager index persists the intended object even for
those cases. Removing a preceding custom adjusts the selected index; removing
the selected object returns to the first built-in fallback. This index behavior
is intentionally unambiguous where the original manager's ID lookup would
choose the first collision. Built-ins cannot be edited, renamed, or removed;
cloning one creates an editable custom with a fresh UUID. Saving never changes
an existing custom ID.

`edit_template` accepts a JSON string or `null` for `name` and `description`,
booleans for `is_default` and `.enabled`, signed integers for `pitch.threshold_cents`,
`pitch.option`, `exciter_option`, and `output.character`, and finite JSON
numbers for all float fields. Numeric values are effective internal values,
not native XML values or raw bit strings. The writable field names are:

- `name`, `description`, `is_default`, `analysis_volume`;
- `clip.enabled`, `clip.low`, `clip.high`, `clip.percentage`, `clip.headroom`;
- `pitch.enabled`, `pitch.threshold_cents`, `pitch.option`, `exciter_option`;
- `expansion.enabled`, `expansion.low`, `expansion.high`,
  `expansion.target_volume`, `expansion.headroom`;
- `compression.enabled`, `compression.low`, `compression.high`,
  `compression.target_volume`;
- `{expansion,compression}.bands[0..3].{threshold,gain,ratio,attack,release}`
  and `{expansion,compression}.crossovers_hz[0..2]` (substitute each concrete
  name/index);
- `output.limiter_threshold`, `output.margin`, `output.character`,
  `output.limiter_release`.

Identity and the built-in flag are not editable. The `is_default` flag does not
change manager fallback precedence. Clip and expansion
endpoints and both crossover arrays use the native coupled setters in the
order edits are requested; compression endpoints are independent. XML import
returns ignored element/attribute names and prospective native reserialization
losses, and selects the last imported object by index. Export uses the native
copy operation and a fresh UUID, then returns separate copy-difference and
XML-round-trip loss reports. File export creates a new destination atomically
without replacing an existing file. Neither these reports nor CLI settings
establish encoded-file parity.

## Files and encoders

The CLI accepts `wav`, `aiff`, `alac`, `flac`, `mp3`, `aac`, and
`match-input` output selections. AAC and ALAC use an M4A container. Input
discovery recognizes WAV, AIFF/AIF, MP3, M4A and FLAC; protected M4P/WL.MP3
and unverified Ogg sources are rejected. Actual decoding and encoding still
depend on the source and the host codecs, and these options do not establish
byte parity. The interleaved AAC diagnostic remains unresolved.

MP3 output exposes `cbr320` and `vbr192`. The implementation derives bit depth,
sample-rate adaptation and channel handling from the source and selected
format, and transfers tags and artwork. Those paths have bounded comparisons,
not a complete original-file parity claim.

Destination policy covers a selected directory, the input directory, mirrored input folders and replacement mode. Replacement forces match-input, retains the configured backup workflow, and exposes restoration. Ordinary outputs use the observed `_PN` naming convention. Already-processed tags and suffix checks must not disappear in the CLI.

## Safety and truthful outcomes

The reference has potentially destructive output-collision and replacement/error behavior. Those observations must be recorded rather than silently copied into a new default:

- Refuse accidental output clobber by default; destructive replacement must be explicit.
- Preserve backups before replacing originals and report rollback failures.
- Do not report success solely because a late reference status flag says processed if an earlier move/replace operation failed.
- Keep synthetic oracle files and user music separate. Verification never processes a real library by default.
- No vendor activation bypass, credential copying, automatic telemetry or hidden remote side effects.

These are explicit proposed safety differences, not claims of byte-for-byte replication of every original defect.

## Verification boundary

Acceptance requires byte-identical complete output files for the same source bytes and effective settings, checked with SHA-256 against Platinum Notes 10.5.0.1256. The encoded audio, container structure, encoder headers and metadata all count. No perceptual tolerance, PCM-only comparison, normalized-tag comparison, or generic replacement satisfies this requirement. Pin the reference build, architecture, codec/runtime versions, paths and metadata-relevant state for each case.

The first pinned generated Official/WAV case satisfied that criterion:
original A and B and corrected Rust debug/release output were identical in
all 3,076,786 bytes. This does not extend to arbitrary files or settings.

Reference comparisons must cover silence, short clips, representative unclipped/clipped synthetic signals, each mode, parameter edges, state changes, channels, supported sample rates, block partitioning, end-of-stream delay compensation and long-stream counters. Encoders and file workflows require independent metadata, path, collision, permission, backup, cancellation and error tests.

Before asserting this gate is reproducible, process the identical source repeatedly in the original under the same recorded state. If its complete-file hashes differ, preserve the outputs and identify the differing bytes and cause. Report that conflict explicitly; do not silently weaken the requested acceptance criterion. Per-stage PCM hashes and semantic tag comparisons are diagnostic tools, not alternate pass criteria.

A completed Rust implementation must not hide an unrecovered stage behind a no-op, a generic replacement, or a call into the original proprietary binary.

## Vendor-connected and presentation features

The current Personalize screenshots show only Output Location, Output File Format and Processor Load. Archived Audio Player, Privacy, License and Debug/checkpoint controls remain research evidence, not mandatory new CLI commands. File handoff, restore and other non-tab workflows still need their active code paths and relevant menus accounted for. The supplied screenshots leave the template selector/gear menus and lower multiband scroll regions unobserved; those are explicit coverage limits, not proof of additional features or permission to drop existing ones.
