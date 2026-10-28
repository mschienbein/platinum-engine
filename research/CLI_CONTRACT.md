# Platinum Engine CLI contract

Status: the Rust CLI implements this command surface on macOS arm64 and Linux aarch64/x86_64, subject to the restrictions below. Two Platinum Notes 10.5.0.1256 reference exports and Rust debug/release outputs matched complete bytes for one generated 16-second, 48 kHz, stereo 16-bit Official-template WAV case, SHA-256 `06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b`. Commands and options do not establish equivalence for other inputs, settings or execution contexts.

## Product boundary

The project targets the current application's audio and file-workflow capabilities in a headless engine, not a recreation of its windows. It does not claim complete feature parity. Pitch scratch alignment, a native-state-dependent MP3 header path, process-sensitive AAC decoding and MP4 timestamps remain bounded or unresolved. Static reference evidence excludes Ogg extensions from its normal input filter despite vendor advertising.

Production processing uses Rust DSP and explicitly configured public native libraries; macOS uses AudioToolbox/Accelerate, while Linux uses portable implementations. It does not call the original application or its proprietary components; those are reference-only and optional. The [build guide](../docs/BUILD.md) and [Linux guide](../docs/LINUX.md) cover libraries, installation and platform boundaries.

### Linux capability and error boundary

Linux refuses MP3 and AAC decoding, AAC encoding, unequal-rate sample-rate
conversion, artwork conversion that requires Apple's transcoder, unverified
codec and container variants (including unverified GSM masks), and ALAC packet
conditions known to lose native audio. Portable ALAC output admits only finite
22,050–96,000 Hz rates, 1–8 channels and 16/20/24/32-bit depths in its supported
layout. Processing that requires an unavailable conversion is refused before
publication; analysis of non-MP3 inputs at or below 22,050 Hz is refused. A
non-44.1 kHz waveform is unavailable rather than approximated; an otherwise
supported analysis can still succeed.

The Rust codec layer uses `UnsupportedCodec`, `UnsupportedFormat` and
`UnsupportedSampleRateConversion` variants; artwork uses
`ArtworkTranscodeUnavailable`. Linux bridge/FLAC/LAME loads also enforce the
architecture-specific `LibraryPolicyError::Unapproved` digest in
`src/linux_libraries.rs`, plus user-configured FLAC/LAME hashes. Configuring
the digest of an unapproved library does not bypass that policy. CLI per-item
errors serialize `stage`, a human-readable `message`, `cancelled`, recovery
paths and `partial_publication`; the underlying Rust enum is not exposed as a
stable machine-readable error code. See [LINUX.md](../docs/LINUX.md) for
approved identities and residual x86 floating-point differences.

The loader retains a descriptor and handle for each library identity, checks
the digest after loading and verifies a mapped entry point. Failed post-load
hash or mapped-symbol checks quarantine the descriptor and handle for the
process lifetime and prevent reuse of that identity. This does not
prevent a writer with permission on the same inode from changing it while it is
being loaded; an initializer may already have run before the post-load check.
Use libraries on trusted storage not writable by concurrent adversaries.
Docker Desktop has reported identity mismatches for a bind-mounted bridge;
use in-image libraries and container-volume/tmpfs state.

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
| `statistics show`, `statistics reset --confirm` | Engine-owned cumulative processed-song, repaired-peak and rounded-decibel gauges; explicit reset separate from preferences and vendor state. |
| `help`, `version`, source/provenance information | Document supported operations and the reference compatibility target without impersonating the original vendor. |

## Command syntax

`platinum [--state-dir DIRECTORY] [--json] COMMAND ...` uses its own state,
not Platinum Notes' preferences. The global options can appear alongside a
subcommand. `--help`, `COMMAND --help`, `--version`, `version`, and
`provenance` describe the interface and target; no vendor activation or
account is involved. `--state-dir` is the complete state directory, useful for
isolated runs. `HOME` supplies the default output path even with `--state-dir`.
Existing settings and statistics directories must be non-symlink directories
owned by the effective user, with neither group nor world write permission.
Their state and lock files must be owned by that user, singly linked regular
files, also without group/world write permission; symlinks are refused.
On macOS, extended ACLs on these private directories or files are refused
because their effective access cannot be established from mode bits alone.
Directory and file identities are pinned and checked across operations; an
unsafe or replaced object is reported rather than followed or overwritten.

```text
platinum process INPUT... [--jobs N] [--template-index N | --template-id ID]
    [--format wav|aiff|alac|flac|mp3|aac|match-input]
    [--output-dir DIR | --same-folder | --replace-originals --backup-dir DIR]
    [--mirror | --no-mirror] [--mp3-mode cbr320|vbr192]
    [--overwrite] [--allow-ambiguous-marker]
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
platinum statistics show
platinum statistics reset --confirm
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
Template exports stage in a private descriptor-pinned workspace in the destination
directory, publish without replacement, and sync that directory. A failure after
publication reports that the export exists and whether cleanup or sync failed.

Process flags override the loaded settings for that invocation only. The
dedicated default output folder is created if needed; any other selected
`--output-dir` must already exist. `--mirror` and `--no-mirror` apply to a
specific output folder. Replacement always
requires **both** `--replace-originals` and an explicit existing `--backup-dir`
on every invocation, even if replacement is persisted in the configuration.
Replacement forces match-input; a conflicting explicit format is rejected.
Each backup is copied and verified, and its directory is synced before any
source replacement. After publication the output directory is synced before
the displaced original is removed; a sync failure reports the retained
backup and recovery path rather than discarding the displaced file.
New separate outputs also sync the pinned output directory after linking; a
sync failure is reported as published with durability uncertain.
On macOS, metadata verification excludes only the kernel-managed
`com.apple.provenance` extended attribute: a newly created copy receives the
creator's provenance rather than a reproducible copy of the source value.
The attribute is not removed or forged. Other attributes, including
`com.apple.macl` and `com.apple.quarantine`, remain subject to comparison, as
do file bytes, identity, permissions, ACLs and other preserved metadata.
After a metadata copy, differing copyable extended attributes are reapplied
from the source and unexpected attributes removed before strict verification;
any failed reapplication aborts the transaction.
On Linux, copy verification covers bytes, owner/group, mode, access and
modification times, and extended attributes (including POSIX ACL attributes).
Linux birth time and filesystem flags are not copied or compared across
different inodes; when available, they remain part of same-inode identity
checks. Linux atomic swap and no-replace publication require kernel and
filesystem support for the corresponding `renameat2` flags; unsupported
operations fail rather than falling back to a non-atomic replacement.
Separate outputs refuse an existing destination by default. `--overwrite`
explicitly permits atomic replacement of a regular, non-symlink output file;
it is invocation-only and cannot be combined with `--replace-originals`. It
never authorizes overwriting a selected input, a hard link to a selected input,
a directory, a symlink, or another batch destination. Batch destination and
selected-source conflicts are checked before starting workers, including
selected files whose own preflight failed. APFS names use canonical Unicode
decomposition; HFS+/HFSX use the filesystem's Unicode 3.2 decomposition and
excluded ranges. Other and unknown filesystems use byte comparison, applying
case folding when lookup is case-insensitive or sensitivity is indeterminate.
On Linux, `FS_CASEFOLD_FL` absence is treated as case-sensitive only when its
query succeeds; casefolded or unknown directories use Rust Unicode lowercase
mappings without normalization, with ASCII folding for invalid UTF-8 bytes.
Kernel casefold equivalences this approximation misses can still fail at
no-replace or identity-checked publication rather than clobbering output.
Each destination's absent/present identity is captured at batch preflight and
must still match at staging and commit. Existing outputs
are atomically swapped with the completed stage, then the displaced entry is
identity-, metadata-, and SHA-256-checked; an unexpected entry is swapped back
instead of discarded. A rollback's displaced stage is checked against its
pinned identity, metadata, and digest and retained even when unchanged, since a
writer may still hold an open descriptor. Failed process results report its
`error.recovery_path`; the retained file is never automatically deleted.
Each worker uses the exact preflight plan and identity; a source changed after
preflight is rejected rather than inspected and planned again.
Process JSON includes `details.overwrite`,
`details.effective_config.overwrite` and per-item
`report.replaced_existing_output` (false for new output or source replacement).
MP3 staging files are private (0600 subject to umask). New MP3 outputs receive
the mode of an ordinary 0666 creation, including inherited Linux default ACLs;
explicitly overwritten outputs preserve the existing output's owner, group, mode
and effective ACL, or refuse if those permissions cannot be reproduced. Workflow
and CAF staging directories and created stage files are pinned by descriptor,
including when their original directory entry is moved or replaced. If the
platform cannot resolve a safe descriptor/inode alias for a path-only codec,
processing refuses. A partial or replaced stage whose ownership is unproven is
retained and reported. A renamed output parent is detected before publication
and rechecked afterward; errors report its current pinned location when known.
New private workspaces are validated without changing their permissions or ACLs.
On macOS, an inherited extended ACL causes refusal. Initialization failures
clean up a pinned workspace or report any directory that cannot be safely removed.
Reservation cleanup uses a private quarantine allocated before the reservation
exists; it first moves the entry into that quarantine and verifies
its pinned identity; unexpected entries are returned or retained, never deleted.
These checks detect ordinary concurrent writers; a same-user process that
deliberately modifies the private workspace during a transaction can defeat
them.
Workspace creation compares the observed directory identity with the
no-follow opened descriptor but cannot atomically prove the `mkdir` result
against a rename before the first observation. Unsuitable restrictive umasks
can leave an unpinnable directory whose path is reported rather than removing
an unverified entry.

Before reading markers or publishing any file, restore snapshots every target
and backup, freezes each backup's identity and SHA-256, and rejects the whole
request if any target aliases any backup by path or inode. Restore then pairs
each file with the correspondingly ordered explicit `--backup` argument and
verifies it against the marker read from that file. The source identity captured
before marker inspection must still match under the restore reservation.
Marker-only restore requires the tag bridge, not an audio decoder; supplied
backup paths must be absolute. After publication, the restored bytes and
metadata are verified; the restored file receives `F_FULLFSYNC` on macOS or
`fsync` on Linux, and its parent directory is synced before consuming the backup.
Backup consumption
honours the same path reservations as source-replacement jobs. The verified
backup is exclusively moved into a private sibling quarantine, rechecked, then
unlinked. Requests sharing a backup path or inode are restored as a group:
every member must succeed before any authorized backup path is consumed; if
any member fails or cannot be inspected, the backup is retained. The frozen
backup is rechecked before every member and before consumption. Original
processed inodes are retained until the group completes; if a frozen backup
changes mid-group, published members are rolled back where safe, otherwise
the recovery path and rollback failure are reported.

If a sync, identity check, reservation, quarantine, or removal fails,
restoration remains successful and the report contains `backup_removed: false`
plus a structured `backup_removal_error` with `operation`, `path`, `message`,
`retained_path`, and `unexpected_path`. `retained_path` is null if the verified
backup's location is unknown; `unexpected_path` separately identifies an
unverified entry retained after a race. On normal removal success
`backup_removed` is true and `backup_removal_error` is null; a quarantine
directory cleanup warning can accompany `backup_removed: true`.
`details.backup_policy` is `remove_after_verified_restore`.

`--allow-ambiguous-marker` is a process-only, nonpersistent override for one
WAV `PLTN` whose payload is empty or whose canonical Base64 decodes to an
incomplete JSON object. It can reprocess an already-processed file with a
damaged or empty marker; the default remains refusal. The report names each
applied override. Valid processed markers, `_PN` suffixes, duplicate `PLTN`
chunks and suspicious trailing marker bytes remain refused. An ambiguous
override also refuses any bytes beyond the declared RIFF. Analysis, inspection
and restore keep their strict marker rules. This is an explicit user-selected
safety policy, not default admission parity with the original.

The engine sniffs the source container before deciding marker state, even when
the extension-selected tag reader succeeds. Leading ID3v2 headers are validated
and skipped, including their synchsafe lengths and optional footers, before the
following container is classified. Additional nonempty ID3v2 tags are refused
because the bridge does not inspect their marker frames; an ID3-prefixed Ogg
source is also refused. An unknown signature is refused even if a permissive
extension-selected reader accepts it. MP4 detection walks bounded top-level
atoms, validates their declared sizes and a movie child, and requires both
`moov` and `mdat`; leading `free`, `wide` and `skip` atoms are permitted.
An atom name in an MPEG frame alone is not evidence of an MP4 container.
If the direct header does not identify a container, the weaker MPEG scan is
used only for an MP3-selected extension or after a valid ID3 prefix. It scans
at most 64 KiB from the post-prefix position for three complete consecutive
frames with matching version, layer and sample rate; bitrate and padding may
vary. Recognized WAV, AIFF, FLAC and Ogg headers take precedence over
incidental embedded MPEG frame bytes.

When the detected PN-writable family differs from the extension, its marker
is read through the bridge; a read error is a refusal, and a processed or
ambiguous marker follows the normal safeguards. Tag inspection and copying
still use the extension-selected family. If that reader returns ABI-5
source-unavailable status 3, no source tags are copied. A positively identified
Ogg source without an ID3 prefix may continue.

For a matching WAV or AIFF/AIFC whose tag container is unavailable, a bounded
physical chunk walk may establish marker absence. It requires format and audio
chunks, clamps a truncated final audio payload to the file, and refuses partial
headers, truncated non-audio chunks, trailing bytes and unwalkable or excessive
chunks. Any `PLTN`, `ID3 ` or `id3 ` chunk is conservatively refused even with
the ambiguous-marker override. Other matching PN-writable families are refused
on status 3. When admission succeeds without source tags, inspection reports
`tags: null` and `tags_unavailable` while reporting the marker state actually
established; processing reports `source_tags_unavailable` and writes the output
marker. Destination-container, file-open and I/O failures are not treated as
absent tags. Restore still requires an inspectable authorizing marker. Older
ABI-5 bridges report status 3 conditions as generic errors.

AIFF/AIFC marker reads also walk the physically present chunks and refuse more
than one `ID3 ` or `id3 ` chunk, even if the first has no marker. This applies
to inspection, processing and restore; the ambiguous-marker override does not
authorize duplicate AIFF tags.

This physical AIFF-chunk safeguard does not inspect every conflicting marker
within a single ID3 tag or duplicate MP4 freeform items, and FLAC's Xiph
marker takes precedence over an ID3 GEOB. A valid unprocessed first marker
could therefore hide a later processed marker. The leading-ID3 counter covers
adjacent tags; sync-like junk before a tag, or tags separated by junk, could
also hide one. These deliberately crafted scenarios are accepted limitations
from source review; the reviewer did not execute the exact end-to-end examples
or establish that their inputs decode. Do not treat this as proof of equivalent
native admission or as complete processed-marker protection for untrusted,
intentionally modified files.

The WAV marker rewrite creates its exclusive sibling with no broader mode than
the staging file, restores that file's mode after writing, and then renames it;
a restrictive umask cannot silently change the final mode. On failure, cleanup
unlinks only a dev/inode-verified sibling in a non-shared, ACL-checked parent.
Otherwise it does not unlink and reports the cleanup failure and sibling path.

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

Restore replaces the processed file with a verified copy of the backup and,
after verifying the published copy, consumes that backup. Check `backup_removed`
and `backup_removal_error` in the per-file report. A separate-output file or a
nil marker without a recorded backup cannot authorize restore; keep the
independent copy and reported backup for recovery instead.

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

`ProcessReport.encoded_length_frames` is the encoded container's advertised
frame length, distinct from the recorder's `input_frames`, `healed_frames`
and `output_frames`. `source_tags_unavailable` is emitted only when a source
tag container could not be inspected or copied; it is omitted otherwise.
`InspectionReport.tags_unavailable` is likewise omitted when absent, while
`tags` can be null and `processed`/`processing_state` describe the independently
established marker/suffix state. `AnalysisReport.waveform` may be null with
`waveform_unavailable` explaining why; that reason is omitted when absent.
`original_waveform` and `original_waveform_unavailable` describe the recorded
backup's waveform or error, and the latter may be null. The diagnostic-only
`controlled_mp4_time_1904` field is the supplied second or null; it does not
claim the original clock's sampling phase.

On the portable path, MP3 postflight inspects the encoded output without
enabling MP3 input decoding. For a CBR `Info` header, its frame estimate uses
the observed AudioToolbox binary64 operation order and rounds the estimated
packet count upward before accounting for encoder delay and padding; a `Xing`
header uses its validated declared packet count. Thus encoded length is not
necessarily the recorder's submitted-frame count.

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

On a render failure or cancellation, the live writer is explicitly aborted.
If writer disposal/abort also fails, its error is appended to the original
error message and the outcome is a failure rather than a clean cancellation.
The transaction reports retained backup/recovery paths and any partial
publication as applicable; callers must inspect those fields.

## Cumulative gauges

Successful CLI processing records one song, its repaired-peak count, and its
binary32 absolute input/output-volume difference rounded to the nearest integer
with ties away from zero. The counters use native signed-64 wrapping additions.
The instruction-derived nontrapping conversion maps NaN to zero and positive
overflow to `i64::MAX`; those edge cases are not observed GUI parity claims.
The original's status-4 error path can increment despite failed publication;
the Engine instead counts only returned successful processing reports.

`statistics.json` and `statistics.lock` live in the Engine state directory,
separate from `settings.json` and all Platinum Notes preferences. An absent file
starts a new baseline; no historical or vendor totals are imported. The first
recorded time is the recording host's Unix-millisecond clock, not necessarily
the first audio-processing time. Explicit reset clears gauges and that date;
configuration reset and audio restore do not change the gauges.

Cooperating processes serialize read/modify/write and reset under the permanent
lock. First publication is no-replace; existing-state updates use identity checks
and atomic rename. Malformed state is preserved and reported, not reset silently.
Settings and statistics stages keep their creation descriptors through commit
and cleanup, and are made owner-readable and writable (0600) before publication.
Cancellation prevents a reset if it wins the shared publication boundary.
After audio is published, accounting is attempted immediately; a cancelled wait
on a contended statistics lock reports that the audio exists but was not counted.
Cleanup failures remain failures rather than clean cancellations.

Audio and statistics are separate commits, not crash-atomic exactly-once
accounting. An accounting error never removes or relabels successful audio.
The per-file result retains its report and a separate statistics outcome; a
post-rename sync error reports published-but-durability-uncertain and must not
be blindly retried.


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

On macOS, `SettingsStore::in_home` uses
`~/Library/Application Support/Platinum Engine/` (not Platinum Notes'
preferences). On Linux, it uses `$XDG_CONFIG_HOME/platinum-engine/` when
`XDG_CONFIG_HOME` is absolute, otherwise `~/.config/platinum-engine/`.
Linux statistics use `$XDG_STATE_HOME/platinum-engine/` when absolute,
otherwise `~/.local/state/platinum-engine/`. An explicit CLI `--state-dir`
places both settings and statistics in that directory on either platform.
`SettingsStore::new` accepts the complete state-directory path plus home and
available-core count for isolated use.
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

On Linux, the FLAC, LAME and TagLib bridge libraries must additionally match
the exact architecture-specific SHA-256 identities in `src/linux_libraries.rs`.
An unapproved library is refused even if the caller supplies its own matching
digest; the typed policy error names the actual and expected identities. FLAC
decoding uses the same approved, inode-pinned handle as encoding. On both
platforms, codecs and bridge are hashed before and after loading through a
retained file descriptor; the FLAC and LAME configured pins are checked on
that descriptor before `dlopen`. `TagBridge::bridge_sha256()` reports the
observed bridge digest. The loader verifies the resolved entry point's mapped
file identity and caches each verified library handle and descriptor for the
process lifetime, preventing descriptor-name reuse when a dynamic loader
retains a previously closed image. The loader requires `/dev/fd` on macOS or
`/proc/self/fd` on Linux and fails if the descriptor path is unavailable.
The macOS default build retains its caller-selected codec and bridge policy.

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
and `.ogg` sources are rejected. The reference's recovered normal extension
filter excludes `.ogg` and `.oga`; on macOS AudioToolbox can decode Ogg content
under a permitted extension, with source tags unavailable if TagLib cannot
interpret that named container. Actual decoding and encoding depend on the
source and host codecs, and these options do not establish byte parity.

With `PLATINUM_DIAGNOSTIC_JOB_TRACE`, macOS records loaded-image callbacks
with Mach-O UUIDs. Linux snapshots loaded ELF images at initialization and at
each job boundary, recording a GNU build ID when present. This snapshot is not
a loader callback: images loaded and unloaded between snapshots can be missed.
The x86-64 DSP flush scope enables both MXCSR flush-to-zero and
denormals-are-zero; its prior bits are restored afterward. ARM and x86 differ
in when they detect underflow, so boundary values are not claimed to be
cross-architecture byte-identical.

`analyze` obtains waveform data in a separate read pass requesting 44.1 kHz
interleaved float32. Each nonempty read of at most 512 frames produces one
pair of sequential float32 mean-absolute amplitudes, using the last two
channels or duplicating mono. A short read remains its own bin; it is not
zero-padded. JSON reports both the requested rate and the decoder's actual
rate, frame positions and actual bin lengths. The original FLAC rate setter
is a no-op, so FLAC waveform data retains its source rate. Analysis of FLAC
with more than two channels currently fails closed: the native interleaved
path can leave part of the returned buffer uninitialized, and deterministic
waveform values for that suffix have not been established. No analysis
report is returned for that unsupported path.
On Linux and in the macOS `portable` feature, a non-MP3 source at or below
22,050 Hz is refused because the reference processing-rate conversion has not
been reproduced. For other non-44.1 kHz sources, analysis can proceed but the
primary waveform is `null` with `waveform_unavailable` explaining the missing
conversion. MP3/AAC decoding, AAC encoding and output sample-rate conversion
fail before publication. Portable FLAC and MP3 output can use caller-pinned
encoders; sampled patched Linux builds are documented separately and are not
a proof of whole-pipeline parity. MP3 output is inspected without enabling MP3
input decoding; the postflight validates MPEG frame structure and computes the
gapless length using the observed AudioToolbox reporting rules. Portable pitch
and warmth no longer have their provisional refusals; unsupported FFT
shapes still fail when the portable setup cannot be created. The verified
vector and scalar component comparisons do not by themselves establish
whole-file or whole-graph parity.

For a processed file, the primary waveform describes that file. An original
waveform is available only when a readable backup is explicitly identified
by its processing marker; otherwise its unavailability is reported. Native
component captures verify the reducer for three input sequences, not every
decoder, resampler, file or GUI cache behavior.
A separate four-case generated PCM16 WAV comparison at 44.1/48 kHz (mono/stereo)
matched every native decoder float byte and read boundary; the release CLI's
resulting bins also match.

MP3 output exposes `cbr320` and `vbr192`. The implementation derives bit depth,
sample-rate adaptation and channel handling from the source and selected
format, and transfers tags and artwork. Those paths have bounded comparisons,
not a complete original-file parity claim.
MP3 conversion accepts 1–64 source channels through the source-to-stereo PCM16
converter; LAME receives mono for a mono source and stereo otherwise. A
generated six-channel 24-bit WAV matched both pinned native MP3 captures.
After macOS ALAC finalization, the private output is decoded and its valid
frame count checked against submitted frames before publication. Valid empty
ALAC containers remain accepted; a silently truncated stream is refused.

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
