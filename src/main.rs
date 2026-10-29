use clap::{Args, Parser, Subcommand, ValueEnum, error::ErrorKind};
use platinum_engine::error_code::ErrorCode;
use platinum_engine::engine::{self, EngineError};
use platinum_engine::file_workflow::{self, AudioFormat, Cancellation, DiscoveredFile, OutputMode, RestoreStatus, SourceType, WorkflowError};
use platinum_engine::settings::{LoadedSettings, SelectedTemplate, Settings, SettingsError, SettingsStore, TemplateRef};
use platinum_engine::statistics::{StatisticsError, StatisticsStore};
use platinum_engine::templates::{Band, Template};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

#[derive(Debug, Parser)]
#[command(name = "platinum", version, about = "Process and inspect audio with Platinum Engine", long_about = "Platinum Engine is an independent Rust implementation. One pinned Official-template/WAV fixture matches Platinum Notes 10.5.0.1256 byte-for-byte. Parity for arbitrary inputs, settings and formats is not established.")]
struct Cli {
    /// Isolate persistent Platinum Engine state in this directory.
    #[arg(long, global = true, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,
    /// Emit one JSON outcome on stdout, including failures.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Process files or recursively discovered folders.
    Process(ProcessArgs),
    /// Analyze files without writing processed audio.
    Analyze(AnalyzeArgs),
    /// Inspect files, source metadata, and processing markers.
    Inspect(InspectArgs),
    /// Restore processed files from explicitly supplied recorded backups.
    Restore(RestoreArgs),
    /// List, select, edit, and interchange templates.
    Templates { #[command(subcommand)] command: TemplateCommand },
    /// Read or change persistent Engine preferences.
    Config { #[command(subcommand)] command: ConfigCommand },
    /// Show or explicitly reset cumulative Engine-owned processing gauges.
    Statistics { #[command(subcommand)] command: StatisticsCommand },
    /// Print the Engine version.
    Version,
    /// Print the compatibility target and implementation provenance.
    Provenance,
}

#[derive(Debug, Args)]
struct BatchArgs {
    /// Files or folders. Directory traversal does not follow symbolic links.
    #[arg(required = true, value_name = "INPUT")]
    inputs: Vec<PathBuf>,
    /// Number of workers; bounded by processor_max and available cores.
    #[arg(long, value_name = "N")]
    jobs: Option<NonZeroUsize>,
}

#[derive(Debug, Args)]
struct RunTemplate {
    /// Use this zero-based manager index for this invocation only.
    #[arg(long, conflicts_with = "template_id", value_name = "N")]
    template_index: Option<usize>,
    /// Use this exact, unambiguous template ID for this invocation only.
    #[arg(long, conflicts_with = "template_index", value_name = "ID")]
    template_id: Option<String>,
}

impl RunTemplate {
    fn reference(&self) -> Option<TemplateRef<'_>> {
        self.template_index.map(TemplateRef::Index).or_else(|| self.template_id.as_deref().map(TemplateRef::Id))
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FormatArg { Wav, Aiff, Alac, Flac, Mp3, Aac, Opus, MatchInput }
impl From<FormatArg> for AudioFormat {
    fn from(format: FormatArg) -> Self {
        match format {
            FormatArg::Wav => Self::Wav, FormatArg::Aiff => Self::Aiff,
            FormatArg::Alac => Self::Alac, FormatArg::Flac => Self::Flac,
            FormatArg::Mp3 => Self::Mp3, FormatArg::Aac => Self::Aac, FormatArg::Opus => Self::Opus,
            FormatArg::MatchInput => Self::MatchInput,
        }
    }
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mp3Arg { Cbr320, Vbr192 }

#[derive(Debug, Args)]
struct ProcessArgs {
    #[command(flatten)] batch: BatchArgs,
    #[command(flatten)] template: RunTemplate,
    /// Select the output codec for this invocation.
    #[arg(long, value_enum, value_name = "FORMAT")]
    format: Option<FormatArg>,
    /// Write new files under this existing directory.
    #[arg(long, value_name = "DIRECTORY", conflicts_with_all = ["same_folder", "replace_originals"])]
    output_dir: Option<PathBuf>,
    /// Write new _PN files next to their sources.
    #[arg(long, conflicts_with_all = ["output_dir", "replace_originals", "mirror", "no_mirror"])]
    same_folder: bool,
    /// Explicitly authorize replacement, with a verified retained backup.
    #[arg(long, requires = "backup_dir", conflicts_with_all = ["output_dir", "same_folder", "mirror", "no_mirror"])]
    replace_originals: bool,
    /// Atomically replace an existing output file; never overwrite an input.
    #[arg(long, conflicts_with = "replace_originals")]
    overwrite: bool,
    /// Existing directory for replacement backups; required on every replacement run.
    #[arg(long, value_name = "DIRECTORY", requires = "replace_originals")]
    backup_dir: Option<PathBuf>,
    /// Mirror selected input folder paths under a specific output directory.
    #[arg(long, conflicts_with_all = ["no_mirror", "same_folder", "replace_originals"])]
    mirror: bool,
    /// Disable folder mirroring for this invocation.
    #[arg(long, conflicts_with_all = ["mirror", "same_folder", "replace_originals"])]
    no_mirror: bool,
    /// Override the configured MP3 encoding mode for this invocation.
    #[arg(long, value_enum, value_name = "MODE")]
    mp3_mode: Option<Mp3Arg>,
    /// Allow a WAV PLTN with an empty payload or incomplete JSON for processing.
    /// This may reprocess an already processed file; inspection and restore stay strict.
    #[arg(long)]
    allow_ambiguous_marker: bool,
    /// Diagnostic only: write this second (since 1904) to the MP4 header time
    /// fields of AAC/ALAC outputs instead of the AudioToolbox wall clock.
    #[arg(long, hide = true, value_name = "SECONDS_SINCE_1904")]
    controlled_mp4_time_1904: Option<u32>,
}

#[derive(Debug, Args)]
struct AnalyzeArgs {
    #[command(flatten)] batch: BatchArgs,
    #[command(flatten)] template: RunTemplate,
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// Files to inspect.
    #[arg(required = true, value_name = "FILE")]
    files: Vec<PathBuf>,
    /// Number of workers; bounded by processor_max and available cores.
    #[arg(long, value_name = "N")]
    jobs: Option<NonZeroUsize>,
}

#[derive(Debug, Args)]
struct RestoreArgs {
    /// Processed files, in the same order as the --backup options.
    #[arg(required = true, value_name = "FILE")]
    files: Vec<PathBuf>,
    /// Absolute recorded backup for the corresponding FILE; consumed after verified restore.
    #[arg(long, required = true, action = clap::ArgAction::Append, value_name = "PATH")]
    backup: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct TemplateSelector {
    /// Zero-based index in manager order, built-ins first.
    #[arg(long, conflicts_with = "id", value_name = "N")]
    index: Option<usize>,
    /// Exact case-sensitive ID; duplicate IDs require --index.
    #[arg(long, conflicts_with = "index", value_name = "ID")]
    id: Option<String>,
}
impl TemplateSelector {
    fn reference(&self) -> Result<TemplateRef<'_>, AppError> {
        self.index.map(TemplateRef::Index).or_else(|| self.id.as_deref().map(TemplateRef::Id))
            .ok_or_else(|| AppError::usage("supply exactly one of --index or --id"))
    }
}

#[derive(Debug, Subcommand)]
enum TemplateCommand {
    /// List templates in manager order.
    List,
    /// Show both effective values and exact serialized state.
    Show { #[command(flatten)] selector: TemplateSelector },
    /// Persist the selected template.
    Select { #[command(flatten)] selector: TemplateSelector },
    /// Clone a built-in or custom template into an editable custom template.
    Clone { #[command(flatten)] selector: TemplateSelector },
    /// Edit an effective template field using a JSON value.
    Edit {
        #[command(flatten)] selector: TemplateSelector,
        field: String,
        #[arg(allow_hyphen_values = true, value_name = "JSON_VALUE")] value: String,
    },
    /// Rename a custom template.
    Rename { #[command(flatten)] selector: TemplateSelector, name: String },
    /// Remove a custom template.
    Remove { #[command(flatten)] selector: TemplateSelector },
    /// Import .pntemplate/XML, reporting ignored fields and round-trip losses.
    Import { file: PathBuf },
    /// Export a native-style copy without overwriting an existing file.
    Export { #[command(flatten)] selector: TemplateSelector, file: PathBuf },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show all preferences and the effective selection.
    Show,
    /// Read one configuration field.
    Get { field: String },
    /// Set a field to a JSON value.
    Set { field: String, #[arg(allow_hyphen_values = true, value_name = "JSON_VALUE")] value: String },
    /// Reset one field, or all preferences while preserving custom templates.
    Reset { field: Option<String> },
}

#[derive(Debug, Subcommand)]
enum StatisticsCommand {
    /// Show totals since the first recorded Engine report or the last reset.
    Show,
    /// Reset only the Engine gauges; requires explicit confirmation.
    Reset { #[arg(long, required = true)] confirm: bool },
}

#[derive(Debug)]
struct AppError { kind: &'static str, code: ErrorCode, message: String, publication: Option<&'static str>, cleanup: Option<Value> }
impl AppError {
    fn usage(message: impl Into<String>) -> Self { Self { kind: "usage", code: ErrorCode::Usage, message: message.into(), publication: None, cleanup: None } }
    fn settings(error: impl std::fmt::Display) -> Self { Self { kind: "settings", code: ErrorCode::SettingsError, message: error.to_string(), publication: None, cleanup: None } }
    fn io(message: impl Into<String>) -> Self { Self { kind: "io", code: ErrorCode::IoError, message: message.into(), publication: None, cleanup: None } }
    /// A filesystem-workflow failure outside any one item; its code is the workflow's.
    fn workflow(error: &WorkflowError) -> Self { Self { kind: "io", code: error.code(), message: error.to_string(), publication: None, cleanup: None } }
    fn statistics(error: StatisticsError) -> Self {
        let cancelled = error.is_cancelled();
        Self { kind: if cancelled { "cancelled" } else { "statistics" }, code: if cancelled { ErrorCode::Cancelled } else { ErrorCode::StatisticsError },
            cleanup: statistics_cleanup(&error), message: error.to_string(), publication: Some(error.publication_status()) }
    }
}

struct Outcome { value: Value, exit: i32 }
fn success(command: &str, data: Value) -> Outcome {
    Outcome { value: json!({"command": command, "status": "ok", "data": data}), exit: 0 }
}
fn json_value(text: &str) -> Result<Value, AppError> {
    serde_json::from_str(text).map_err(|e| AppError::usage(format!("expected a JSON value: {e}")))
}
fn as_value<T: Serialize>(data: T) -> Result<Value, AppError> {
    serde_json::to_value(data).map_err(|e| AppError::settings(format!("serialize result: {e}")))
}
fn save(store: &SettingsStore, loaded: &mut LoadedSettings) -> Result<(), AppError> {
    store.save(loaded).map_err(AppError::settings)
}

fn effective_float(value: f32) -> Value {
    if value.is_nan() { json!("NaN") }
    else if value == f32::INFINITY { json!("Infinity") }
    else if value == f32::NEG_INFINITY { json!("-Infinity") }
    else { json!(value) }
}
fn band_value(band: &Band) -> Value {
    json!({"threshold": effective_float(band.threshold), "gain": effective_float(band.gain),
        "ratio": effective_float(band.ratio), "attack": effective_float(band.attack), "release": effective_float(band.release)})
}
fn effective_template(t: &Template) -> Value {
    json!({
        "unique_id": t.unique_id, "name": t.name, "description": t.description,
        "is_builtin": t.is_builtin, "is_default": t.is_default, "analysis_volume": effective_float(t.analysis_volume),
        "clip": {"enabled": t.clip.enabled, "low": effective_float(t.clip.low), "high": effective_float(t.clip.high),
            "percentage": effective_float(t.clip.percentage), "headroom": effective_float(t.clip.headroom)},
        "pitch": {"enabled": t.pitch.enabled, "threshold_cents": t.pitch.threshold_cents, "option": t.pitch.option},
        "exciter_option": t.exciter_option,
        "expansion": {"enabled": t.expansion.enabled, "low": effective_float(t.expansion.low), "high": effective_float(t.expansion.high),
            "target_volume": effective_float(t.expansion.target_volume), "headroom": effective_float(t.expansion.headroom),
            "bands": t.expansion.bands.iter().map(band_value).collect::<Vec<_>>(),
            "crossovers_hz": t.expansion.crossovers_hz.map(effective_float)},
        "compression": {"enabled": t.compression.enabled, "low": effective_float(t.compression.low), "high": effective_float(t.compression.high),
            "target_volume": effective_float(t.compression.target_volume),
            "bands": t.compression.bands.iter().map(band_value).collect::<Vec<_>>(),
            "crossovers_hz": t.compression.crossovers_hz.map(effective_float)},
        "output": {"limiter_threshold": effective_float(t.output.limiter_threshold), "margin": effective_float(t.output.margin),
            "character": t.output.character, "limiter_release": effective_float(t.output.limiter_release)}
    })
}
fn template_details(t: &Template) -> Result<Value, AppError> {
    Ok(json!({"effective": effective_template(t), "serialized": as_value(t)?}))
}
fn list_templates(settings: &Settings) -> Result<Value, AppError> {
    let catalog = settings.catalog().map_err(AppError::settings)?;
    let builtin_count = catalog.templates.len() - settings.custom_templates.len();
    let selected = match settings.effective_selection().map_err(AppError::settings)? {
        SelectedTemplate::Builtin { id } => catalog.templates[..builtin_count].iter().position(|t| t.unique_id.as_deref() == Some(id.as_str()))
            .ok_or_else(|| AppError::settings("effective built-in selection is missing"))?,
        SelectedTemplate::Custom { index, .. } => builtin_count + index,
    };
    Ok(Value::Array(catalog.templates.iter().enumerate().map(|(index, t)| json!({
        "index": index, "id": t.unique_id, "name": t.name, "description": t.description,
        "builtin": t.is_builtin, "selected": index == selected
    })).collect()))
}
fn templates(command: TemplateCommand, store: &SettingsStore, loaded: &mut LoadedSettings) -> Result<Outcome, AppError> {
    let data = match command {
        TemplateCommand::List => list_templates(&loaded.settings)?,
        TemplateCommand::Show { selector } => template_details(&loaded.settings.template(selector.reference()?).map_err(AppError::settings)?)?,
        TemplateCommand::Select { selector } => {
            let index = loaded.settings.select_template(selector.reference()?).map_err(AppError::settings)?;
            save(store, loaded)?;
            json!({"index": index, "selection": loaded.settings.effective_selection().map_err(AppError::settings)?})
        }
        TemplateCommand::Clone { selector } => {
            let index = loaded.settings.clone_template(selector.reference()?).map_err(AppError::settings)?;
            save(store, loaded)?;
            json!({"index": index, "template": template_details(&loaded.settings.template(TemplateRef::Index(index)).map_err(AppError::settings)?)?})
        }
        TemplateCommand::Edit { selector, field, value } => {
            loaded.settings.edit_template(selector.reference()?, &field, json_value(&value)?).map_err(AppError::settings)?;
            let template = loaded.settings.template(selector.reference()?).map_err(AppError::settings)?;
            save(store, loaded)?;
            template_details(&template)?
        }
        TemplateCommand::Rename { selector, name } => {
            loaded.settings.rename_template(selector.reference()?, &name).map_err(AppError::settings)?;
            let template = loaded.settings.template(selector.reference()?).map_err(AppError::settings)?;
            save(store, loaded)?;
            template_details(&template)?
        }
        TemplateCommand::Remove { selector } => {
            let removed = loaded.settings.remove_template(selector.reference()?).map_err(AppError::settings)?;
            save(store, loaded)?;
            json!({"removed": template_details(&removed)?, "selection": loaded.settings.effective_selection().map_err(AppError::settings)?})
        }
        TemplateCommand::Import { file } => {
            let report = loaded.settings.import_file(&file).map_err(AppError::settings)?;
            save(store, loaded)?;
            as_value(report)?
        }
        TemplateCommand::Export { selector, file } => {
            let report = loaded.settings.export_file(selector.reference()?, &file).map_err(AppError::settings)?;
            json!({"path": path_value(&file), "report": report})
        }
    };
    Ok(success("templates", data))
}
fn config(command: ConfigCommand, store: &SettingsStore, loaded: &mut LoadedSettings) -> Result<Outcome, AppError> {
    let data = match command {
        ConfigCommand::Show => loaded.settings.config_show().map_err(AppError::settings)?,
        ConfigCommand::Get { field } => json!({"field": field, "value": loaded.settings.config_get(&field).map_err(AppError::settings)?}),
        ConfigCommand::Set { field, value } => {
            loaded.settings.config_set(&field, json_value(&value)?, store.available_cores()).map_err(AppError::settings)?;
            save(store, loaded)?;
            json!({"field": field, "value": loaded.settings.config_get(&field).map_err(AppError::settings)?})
        }
        ConfigCommand::Reset { field } => {
            loaded.settings.config_reset(field.as_deref(), store.home(), store.available_cores()).map_err(AppError::settings)?;
            save(store, loaded)?;
            loaded.settings.config_show().map_err(AppError::settings)?
        }
    };
    Ok(success("config", data))
}

fn statistics(command: StatisticsCommand, store: &StatisticsStore, cancel: &Cancellation) -> Result<Outcome, AppError> {
    let state = match command {
        StatisticsCommand::Show => store.show(cancel),
        StatisticsCommand::Reset { confirm: true } => store.reset(cancel),
        StatisticsCommand::Reset { confirm: false } => return Err(AppError::usage("statistics reset requires --confirm")),
    }.map_err(AppError::statistics)?;
    let mut data = as_value(state)?;
    data["baseline"] = json!("Engine-owned totals start at zero when statistics are absent or reset; no pre-upgrade or Platinum Notes totals are imported.");
    data["first_recorded_time"] = json!("Unix milliseconds from the recording host clock at the first recorded report after the baseline; null until then.");
    data["publication"] = json!("Audio and statistics are separate, not crash-atomic publications; crashes can leave a published output uncounted.");
    Ok(success("statistics", data))
}

// JSON strings cannot represent arbitrary Unix filenames. Preserve such paths
// as their exact bytes alongside a display rendering instead of panicking.
fn path_value(path: &Path) -> Value {
    match path.to_str() {
        Some(text) => Value::String(text.to_owned()),
        None => {
            use std::fmt::Write as _;
            let bytes = path.as_os_str().as_bytes();
            let mut hex = String::with_capacity(bytes.len() * 2);
            for byte in bytes { write!(&mut hex, "{byte:02x}").expect("writing to a string cannot fail"); }
            json!({"display": path.to_string_lossy(), "unix_bytes_hex": hex})
        }
    }
}

#[derive(Serialize)]
struct ItemResult {
    input: Value,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    statistics: Option<Value>,
}
impl ItemResult {
    fn ok(input: &Path, report: Value) -> Self { Self { input: path_value(input), status: "ok", report: Some(report), error: None, statistics: None } }
    fn failed(input: &Path, error: Value) -> Self { Self { input: path_value(input), status: "failed", report: None, error: Some(error), statistics: None } }
    fn not_started(input: &Path, code: ErrorCode, message: &str) -> Self {
        Self { input: path_value(input), status: "not_started", report: None, error: Some(json!({"code": code, "message": message})), statistics: None }
    }
    fn engine_error(input: &Path, error: EngineError) -> Self {
        Self { input: path_value(input), status: if error.cancelled { "cancelled" } else { "failed" }, report: None,
            error: Some(json!({"stage": error.stage, "code": error.code, "message": error.message, "cancelled": error.cancelled,
                "retained_backup": error.retained_backup.as_deref().map(path_value),
                "recovery_path": error.recovery_path.as_deref().map(path_value), "partial_publication": error.partial_publication})), statistics: None }
    }
    fn workflow_error(input: &Path, error: WorkflowError) -> Self {
        let cancelled = error.is_cancelled();
        Self { input: path_value(input), status: if cancelled { "cancelled" } else { "failed" },
            report: None, error: Some(workflow_error(error)), statistics: None }
    }
}

/// A cleanup that failed to remove or restore something is an I/O failure, whatever caused the cleanup.
fn cleanup_failure_code() -> ErrorCode { ErrorCode::IoError }

fn statistics_cleanup(error: &StatisticsError) -> Option<Value> {
    match error {
        StatisticsError::Cleanup { path, cause, source } => Some(json!({
            "code":cleanup_failure_code(),
            "stage_path":path_value(path), "cause_kind":if cause.is_cancelled() { "cancelled" } else { "error" },
            "cause":cause.to_string(), "message":source.to_string()
        })),
        StatisticsError::Storage(SettingsError::Cleanup { path, cause, source, .. }) => Some(json!({
            "code":cleanup_failure_code(),
            "stage_path":path_value(path), "cause_kind":"error",
            "cause":cause.to_string(), "message":source.to_string()
        })),
        _ => None,
    }
}

// `code` is `cancelled` when the accounting itself was cancelled and `statistics_error` otherwise;
// the audio is already published in both cases and the item status stays `ok`.
fn statistics_accounting(result: Result<(), StatisticsError>) -> Value {
    match result {
        Ok(()) => json!({"status":"recorded"}),
        Err(error) if error.is_cancelled() => json!({"status":"not_recorded", "reason":"cancelled", "code":ErrorCode::Cancelled,
            "message":error.to_string(), "publication":error.publication_status(), "audio_published":true}),
        Err(error) => {
            let mut value = json!({"status":"failed", "code":ErrorCode::StatisticsError, "message":error.to_string(),
                "publication":error.publication_status(), "audio_published":true});
            if let Some(cleanup) = statistics_cleanup(&error) { value["cleanup"] = cleanup; }
            value
        }
    }
}

// Audio has already been published when a ProcessReport is returned. Account
// before report serialization and never discard that result on a store error.
fn processed_report(input: &Path, report: engine::ProcessReport, store: &StatisticsStore, cancel: &Cancellation) -> ItemResult {
    let accounting = statistics_accounting(store.record(&report, cancel));
    match serde_json::to_value(&report) {
        Ok(value) => ItemResult { input: path_value(input), status: "ok", report: Some(value), error: None, statistics: Some(accounting) },
        Err(error) => ItemResult {
            input: path_value(input), status: "failed",
            report: Some(json!({"output_path":path_value(&report.output_path),
                "backup_path":report.backup_path.as_deref().map(path_value), "replaced_existing_output":report.replaced_existing_output,
                "clip_count":report.clip_count, "input_volume":effective_float(report.input_volume), "output_volume":effective_float(report.output_volume),
                "non_identical_paths":report.non_identical_paths})),
            error: Some({ let mut error = report_error(&error); error["partial_publication"] = json!(true); error }),
            statistics: Some(accounting),
        },
    }
}

/// A report that could not be serialized.
fn report_error(error: &serde_json::Error) -> Value {
    json!({"stage":"report", "code":ErrorCode::ReportSerializationFailed, "message":error.to_string()})
}

/// The top-level outcome of a command that failed outside any item.
fn error_outcome(kind: &str, code: ErrorCode, message: &str) -> Value {
    json!({"status":"error", "error":{"kind":kind, "code":code, "message":message}})
}

fn process_result(input: &Path, result: Result<engine::ProcessReport, EngineError>, store: &StatisticsStore, cancel: &Cancellation) -> ItemResult {
    match result {
        Ok(report) => processed_report(input, report, store, cancel),
        Err(error) => ItemResult::engine_error(input, error),
    }
}
fn workflow_error(error: WorkflowError) -> Value {
    let message = error.to_string();
    let code = error.code();
    match error {
        WorkflowError::Transaction { backup, recovery, rollback, cleanup, published, .. } => json!({
            "code": code, "message": message, "retained_backup": backup.as_deref().map(path_value), "retained_original": recovery.as_deref().map(path_value),
            // The string stays for compatibility; the code of a failed rollback is never `cancelled`.
            "rollback_error": rollback.as_ref().map(|e| e.to_string()),
            "rollback_error_code": rollback.as_ref().map(|e| match e.code() { ErrorCode::Cancelled => cleanup_failure_code(), other => other }),
            "partial_publication": published,
            "cleanup_errors": cleanup.into_iter().map(|e| json!({"code":cleanup_failure_code(), "operation":e.operation, "path":path_value(&e.path), "message":e.source.to_string()})).collect::<Vec<_>>()
        }),
        _ => json!({"code": code, "message": message}),
    }
}
fn restored_report(input: &Path, report: file_workflow::RestoreReport) -> ItemResult {
    ItemResult::ok(input, json!({
        "destination": path_value(&report.destination), "backup": path_value(&report.backup),
        "backup_removed": report.backup_removed,
        "backup_removal_error": report.backup_removal_error.as_ref().map(|error| json!({
            "code": cleanup_failure_code(), "operation": error.operation, "path": path_value(&report.backup), "message": error.message,
            "retained_path": error.retained_path.as_deref().map(path_value),
            "unexpected_path": error.unexpected_path.as_deref().map(path_value),
        })),
    }))
}
fn batch_outcome(command: &str, results: Vec<ItemResult>, cancel: &Cancellation, extras: Value, batch_error: Option<Value>) -> Outcome {
    let ok = results.iter().filter(|r| r.status == "ok").count();
    let failed = results.iter().any(|r| r.status == "failed" || r.status == "not_started"
        || r.statistics.as_ref().is_some_and(|s| s["status"] != "recorded")) || batch_error.is_some();
    let cancelled = cancel.is_cancelled() || results.iter().any(|r| r.status == "cancelled");
    let cleanup_failed = results.iter().any(|r| r.statistics.as_ref().is_some_and(|s| s.get("cleanup").is_some()));
    let (status, exit) = if cleanup_failed && ok > 0 { ("partial_failure", 1) }
        else if cleanup_failed { ("failed", 1) }
        else if cancelled { ("cancelled", 130) }
        else if failed && ok > 0 { ("partial_failure", 1) }
        else if failed || results.is_empty() { ("failed", 1) }
        else { ("ok", 0) };
    Outcome { value: json!({"command": command, "status": status, "results": results, "details": extras, "batch_error": batch_error}), exit }
}

// Every submitted index is either returned exactly once or remains not_started.
// The cancellation token is checked before and after taking an index; active
// engine calls receive that same token and own their transaction cleanup.
fn parallel_items<F>(indices: &[usize], jobs: usize, cancel: &Cancellation, operation: F) -> Vec<(usize, ItemResult)>
where F: Fn(usize) -> ItemResult + Sync {
    if indices.is_empty() { return Vec::new(); }
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    let mut results = Vec::with_capacity(indices.len());
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(indices.len()) {
            let tx = tx.clone();
            let next = &next;
            let operation = &operation;
            scope.spawn(move || loop {
                if cancel.is_cancelled() { break; }
                let slot = next.fetch_add(1, Ordering::Relaxed);
                if slot >= indices.len() || cancel.is_cancelled() { break; }
                let index = indices[slot];
                if tx.send((index, operation(index))).is_err() { break; }
            });
        }
        drop(tx);
        results.extend(rx);
    });
    results
}
fn worker_count(requested: Option<NonZeroUsize>, settings: &Settings, cores: usize) -> usize {
    let max = settings.processor_max.clamp(1, cores.max(1));
    requested.map_or(max, |n| n.get().min(max))
}
fn discovery(inputs: &[PathBuf], admitted: file_workflow::AdmittedFormats) -> (Vec<DiscoveredFile>, Vec<ItemResult>) {
    let mut files = BTreeMap::new();
    let mut errors = Vec::new();
    for input in inputs {
        match file_workflow::discover_inputs_with(std::slice::from_ref(input), admitted) {
            Ok(found) if found.is_empty() => errors.push(ItemResult::failed(input, json!({"stage":"discovery", "code":ErrorCode::NoInputFiles, "message":"no admitted audio files found"}))),
            Ok(found) => for file in found { files.entry(file.path.clone()).or_insert(file); },
            Err(error) => errors.push(ItemResult::failed(input, workflow_error(error))),
        }
    }
    (files.into_values().collect(), errors)
}
fn apply_run_template(settings: &mut Settings, selection: &RunTemplate) -> Result<(), AppError> {
    if let Some(reference) = selection.reference() { settings.select_template(reference).map_err(AppError::settings)?; }
    Ok(())
}
fn apply_process_options(settings: &mut Settings, args: &ProcessArgs, cores: usize) -> Result<(), AppError> {
    apply_run_template(settings, &args.template)?;
    let mut output = settings.output.clone();
    if args.replace_originals {
        output.mode = OutputMode::Replace { backup_directory: args.backup_dir.clone().ok_or_else(|| AppError::usage("--replace-originals requires --backup-dir"))? };
        output.format = AudioFormat::MatchInput;
    } else if let Some(directory) = &args.output_dir {
        output.mode = OutputMode::SpecificFolder { directory: directory.clone(), mirror: args.mirror };
    } else if args.same_folder {
        output.mode = OutputMode::SameFolder;
    } else if matches!(&output.mode, OutputMode::Replace { .. }) {
        return Err(AppError::usage("replacement is configured; explicitly pass --replace-originals --backup-dir DIRECTORY for each run, or choose another output mode"));
    }
    if args.mirror || args.no_mirror {
        match &mut output.mode {
            OutputMode::SpecificFolder { mirror, .. } => *mirror = args.mirror,
            _ => return Err(AppError::usage("--mirror/--no-mirror require a specific output folder")),
        }
    }
    if let Some(format) = args.format {
        let format = AudioFormat::from(format);
        if format == AudioFormat::Opus && !settings.formats.opus {
            return Err(AppError { kind: "usage", code: ErrorCode::OpusNotEnabled, publication: None, cleanup: None,
                message: "Opus output requires the formats.opus setting (platinum config set formats.opus true)".into() });
        }
        if args.replace_originals && format != AudioFormat::MatchInput {
            return Err(AppError::usage("replacement requires --format match-input"));
        }
        output.format = format;
    }
    settings.config_set("output", as_value(output)?, cores).map_err(AppError::settings)?;
    if let Some(mode) = args.mp3_mode {
        settings.config_set("mp3_mode", json!(match mode { Mp3Arg::Cbr320 => "cbr320", Mp3Arg::Vbr192 => "vbr192" }), cores).map_err(AppError::settings)?;
    }
    Ok(())
}
fn prepare_output(settings: &Settings, home: &Path) -> Result<(), AppError> {
    if let OutputMode::SpecificFolder { directory, .. } = &settings.output.mode {
        if *directory == home.join("Music/Platinum Engine Output") {
            std::fs::create_dir_all(directory).map_err(|e| AppError::io(format!("create default output directory {}: {e}", directory.display())))?;
        }
    }
    Ok(())
}

fn process(args: ProcessArgs, mut settings: Settings, home: &Path, cores: usize, cancel: &Cancellation, statistics: &StatisticsStore) -> Result<Outcome, AppError> {
    apply_process_options(&mut settings, &args, cores)?;
    let template = settings.selected_template().map_err(AppError::settings)?;
    let jobs = worker_count(args.batch.jobs, &settings, cores);
    let (files, mut errors) = discovery(&args.batch.inputs, settings.admitted_formats());
    let mut details = json!({"jobs": jobs, "requested_jobs": args.batch.jobs.map(NonZeroUsize::get),
        "effective_config": settings.config_show().map_err(AppError::settings)?, "effective_template": effective_template(&template),
        "overwrite": args.overwrite, "allow_ambiguous_marker": args.allow_ambiguous_marker, "input_marker_overrides": [],
        "controlled_mp4_time_1904": args.controlled_mp4_time_1904,
        "marker_override_warning": if args.allow_ambiguous_marker { Some("An ambiguous marker can belong to an already processed file.") } else { None }});
    details["effective_config"]["overwrite"] = json!(args.overwrite);
    if files.is_empty() { return Ok(batch_outcome("process", errors, cancel, details, None)); }
    if !cancel.is_cancelled() { prepare_output(&settings, home)?; }
    let mut results: Vec<Option<ItemResult>> = (0..files.len()).map(|_| None).collect();
    let mut prepared = Vec::new();
    let mut ready = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if cancel.is_cancelled() { break; }
        match engine::prepare_file(file, &settings, args.allow_ambiguous_marker) {
            Ok(item) => {
                let item = match args.controlled_mp4_time_1904 { Some(seconds) => item.with_controlled_mp4_time(seconds), None => item };
                prepared.push(item); ready.push(index);
            }
            Err(error) => results[index] = Some(ItemResult::engine_error(&file.path, error)),
        }
    }
    details["input_marker_overrides"] = Value::Array(prepared.iter().zip(&ready).filter_map(|(item, &index)| {
        item.input_marker_override().map(|kind| json!({"input": path_value(&files[index].path), "kind": kind}))
    }).collect());
    let overwrite_error = if args.overwrite && !errors.is_empty() {
        Some(json!({"code": ErrorCode::BatchPreflightFailed, "message": "cannot authorize overwrite because a selected input could not be discovered"}))
    } else if args.overwrite {
        let destinations: Vec<_> = prepared.iter().map(|item| item.output_plan().destination().to_owned()).collect();
        match file_workflow::OverwriteGuard::new(&files, &destinations) {
            Ok(guard) => {
                let guard = Arc::new(guard);
                prepared.iter_mut().find_map(|item| item.enable_overwrite(Arc::clone(&guard)).err()
                    .map(|error| json!({"stage": error.stage, "code": error.code, "message": error.message})))
            }
            Err(error) => Some(workflow_error(error)),
        }
    } else { None };
    let plans: Vec<_> = prepared.iter().map(|item| item.output_plan()).collect();
    let conflict = overwrite_error.or_else(|| file_workflow::validate_batch(&plans, &files).err().map(workflow_error));
    if conflict.is_none() && !cancel.is_cancelled() {
        let job_trace = JobTrace::from_env()?;
        let slots: Vec<_> = (0..ready.len()).collect();
        for (slot, result) in parallel_items(&slots, jobs, cancel, |slot| {
            let file = &files[ready[slot]];
            if let Some(trace) = &job_trace { trace.event("begin", slot, &file.path); }
            let processed = engine::process_file(&prepared[slot], &template, &settings, cancel);
            if let Some(trace) = &job_trace { trace.event("end", slot, &file.path); }
            process_result(&file.path, processed, statistics, cancel)
        }) { results[ready[slot]] = Some(result); }
    }
    errors.extend(results.into_iter().enumerate().map(|(i, result)| result.unwrap_or_else(|| {
        let (code, message) = if cancel.is_cancelled() { (ErrorCode::Cancelled, "cancelled before starting") } else { (ErrorCode::BatchPreflightFailed, "batch preflight failed") };
        ItemResult::not_started(&files[i].path, code, message)
    })));
    Ok(batch_outcome("process", errors, cancel, details, conflict))
}

/// Diagnostic only: `PLATINUM_DIAGNOSTIC_JOB_TRACE` names a new file that
/// receives one JSON line when each file job enters and leaves processing, and
/// records loaded images with Mach-O UUIDs on macOS or GNU build IDs on Linux.
struct JobTrace {
    file: std::sync::Mutex<std::fs::File>, origin: std::time::Instant,
    #[cfg(target_os = "linux")]
    images: std::sync::Mutex<std::collections::BTreeSet<(Vec<u8>, Option<String>)>>,
}
static JOB_TRACE: std::sync::OnceLock<JobTrace> = std::sync::OnceLock::new();
impl JobTrace {
    fn from_env() -> Result<Option<&'static Self>, AppError> {
        #[cfg(target_os = "macos")]
        unsafe extern "C" { fn _dyld_register_func_for_add_image(callback: extern "C" fn(*const u8, isize)); }
        let Some(path) = std::env::var_os("PLATINUM_DIAGNOSTIC_JOB_TRACE") else { return Ok(None) };
        if JOB_TRACE.get().is_some() { return Err(AppError::io("job trace already initialized")); }
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)
            .map_err(|e| AppError::io(format!("create job trace {}: {e}", Path::new(&path).display())))?;
        let trace = JOB_TRACE.get_or_init(|| Self {
            file: std::sync::Mutex::new(file), origin: std::time::Instant::now(),
            #[cfg(target_os = "linux")]
            images: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        });
        #[cfg(target_os = "macos")]
        // dyld replays already-loaded images through the process-lifetime callback.
        unsafe { _dyld_register_func_for_add_image(image_added) };
        #[cfg(target_os = "linux")]
        record_linux_images();
        Ok(Some(trace))
    }
    fn write(&self, mut line: Value) {
        use std::io::Write as _;
        let unix_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        line["thread"] = json!(format!("{:?}", std::thread::current().id()));
        line["unix_ns"] = json!(unix_ns.to_string());
        line["elapsed_ns"] = json!(self.origin.elapsed().as_nanos().to_string());
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(file, "{line}").and_then(|()| file.sync_data());
    }
    fn event(&self, event: &str, slot: usize, input: &Path) {
        #[cfg(target_os = "linux")]
        record_linux_images();
        self.write(json!({"event": event, "slot": slot, "input": path_value(input)}));
    }
}

#[cfg(target_os = "macos")]
extern "C" fn image_added(header: *const u8, _slide: isize) {
    #[repr(C)]
    struct DlInfo { fname: *const std::ffi::c_char, fbase: *mut std::ffi::c_void, sname: *const std::ffi::c_char, saddr: *mut std::ffi::c_void }
    unsafe extern "C" { fn dladdr(address: *const std::ffi::c_void, info: *mut DlInfo) -> i32; }
    const LC_UUID: u32 = 0x1b;
    let Some(trace) = JOB_TRACE.get() else { return };
    if header.is_null() { return; }
    // SAFETY: dyld passes the mapped 64-bit Mach-O header of a loaded image;
    // its load commands follow the 32-byte header for `ncmds` entries.
    let (path, uuid) = unsafe {
        let mut info = DlInfo { fname: std::ptr::null(), fbase: std::ptr::null_mut(), sname: std::ptr::null(), saddr: std::ptr::null_mut() };
        let path = if dladdr(header.cast(), &mut info) != 0 && !info.fname.is_null() {
            Some(std::ffi::CStr::from_ptr(info.fname).to_string_lossy().into_owned())
        } else { None };
        let read = |offset: usize| u32::from_ne_bytes(std::ptr::read_unaligned(header.add(offset).cast::<[u8; 4]>()));
        let (ncmds, mut offset, mut uuid) = (read(16), 32usize, None);
        for _ in 0..ncmds {
            let (cmd, size) = (read(offset), read(offset + 4) as usize);
            if cmd == LC_UUID {
                let bytes = std::ptr::read_unaligned(header.add(offset + 8).cast::<[u8; 16]>());
                uuid = Some(bytes.iter().map(|b| format!("{b:02X}")).collect::<String>());
                break;
            }
            if size < 8 { break; }
            offset += size;
        }
        (path, uuid)
    };
    if path.as_deref().is_some_and(|p| p.starts_with("/System/") || p.starts_with("/usr/lib/")) { return; }
    trace.write(json!({"event": "image", "path": path, "uuid": uuid}));
}

#[cfg(any(target_os = "linux", test))]
fn elf_gnu_build_id(bytes: &[u8]) -> Option<String> {
    let mut offset = 0usize;
    while let Some(header) = bytes.get(offset..offset.saturating_add(12)) {
        let namesz = u32::from_ne_bytes(header[0..4].try_into().ok()?) as usize;
        let descsz = u32::from_ne_bytes(header[4..8].try_into().ok()?) as usize;
        let kind = u32::from_ne_bytes(header[8..12].try_into().ok()?);
        let name_start = offset.checked_add(12)?;
        let desc_start = name_start.checked_add(namesz.checked_add(3)? & !3)?;
        let next = desc_start.checked_add(descsz.checked_add(3)? & !3)?;
        if next > bytes.len() { return None; }
        if kind == 3 && bytes.get(name_start..name_start.checked_add(namesz)?) == Some(b"GNU\0".as_slice())
            && descsz > 0 && descsz <= 128 {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut text = String::with_capacity(descsz * 2);
            for &byte in bytes.get(desc_start..desc_start.checked_add(descsz)?)? {
                text.push(HEX[usize::from(byte >> 4)] as char); text.push(HEX[usize::from(byte & 15)] as char);
            }
            return Some(text);
        }
        offset = next;
    }
    None
}

#[cfg(target_os = "linux")]
fn record_linux_images() {
    unsafe { libc::dl_iterate_phdr(Some(linux_image_added), std::ptr::null_mut()); }
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn linux_image_added(info: *mut libc::dl_phdr_info, size: usize, _data: *mut std::ffi::c_void) -> i32 {
    let _ = std::panic::catch_unwind(|| {
        let Some(trace) = JOB_TRACE.get() else { return };
        if info.is_null() || size < std::mem::size_of::<libc::dl_phdr_info>() { return; }
        let info = unsafe { &*info };
        if info.dlpi_phdr.is_null() || usize::from(info.dlpi_phnum) > 4096 { return; }
        let headers = unsafe { std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum)) };
        let base = info.dlpi_addr as usize;
        let mut build_id = None;
        for note in headers.iter().filter(|entry| entry.p_type == libc::PT_NOTE) {
            let Some(start) = base.checked_add(note.p_vaddr as usize) else { continue };
            let Some(end) = start.checked_add(note.p_memsz as usize) else { continue };
            if note.p_memsz == 0 || note.p_memsz > 1_048_576 || !headers.iter().any(|load| {
                if load.p_type != libc::PT_LOAD || load.p_flags & libc::PF_R == 0 { return false; }
                base.checked_add(load.p_vaddr as usize).and_then(|low| low.checked_add(load.p_memsz as usize).map(|high| (low, high)))
                    .is_some_and(|(low, high)| start >= low && end <= high)
            }) { continue; }
            let bytes = unsafe { std::slice::from_raw_parts(start as *const u8, end - start) };
            build_id = elf_gnu_build_id(bytes);
            if build_id.is_some() { break; }
        }
        let path = if info.dlpi_name.is_null() || unsafe { *info.dlpi_name } == 0 {
            std::env::current_exe().ok()
        } else {
            let bytes = unsafe { std::ffi::CStr::from_ptr(info.dlpi_name) }.to_bytes();
            Some(PathBuf::from(OsStr::from_bytes(bytes)))
        };
        let key = (path.as_ref().map_or_else(Vec::new, |p| p.as_os_str().as_bytes().to_vec()), build_id.clone());
        let mut images = trace.images.lock().unwrap_or_else(|error| error.into_inner());
        if images.insert(key) {
            drop(images);
            trace.write(json!({"event": "image", "path": path.as_deref().map(path_value), "build_id": build_id}));
        }
    });
    0
}
fn analyze(args: AnalyzeArgs, mut settings: Settings, cores: usize, cancel: &Cancellation) -> Result<Outcome, AppError> {
    apply_run_template(&mut settings, &args.template)?;
    let jobs = worker_count(args.batch.jobs, &settings, cores);
    let template = settings.selected_template().map_err(AppError::settings)?;
    let (files, mut errors) = discovery(&args.batch.inputs, settings.admitted_formats());
    let indices: Vec<usize> = (0..files.len()).collect();
    let mut results: Vec<Option<ItemResult>> = (0..files.len()).map(|_| None).collect();
    for (index, result) in parallel_items(&indices, jobs, cancel, |index| {
        let file = &files[index];
        match engine::analyze_file(&file.path, &settings, cancel) {
            Ok(report) => match serde_json::to_value(report) {
                Ok(report) => ItemResult::ok(&file.path, report),
                Err(error) => ItemResult::failed(&file.path, report_error(&error)),
            },
            Err(error) => ItemResult::engine_error(&file.path, error),
        }
    }) { results[index] = Some(result); }
    errors.extend(results.into_iter().enumerate().map(|(i, result)| result.unwrap_or_else(|| ItemResult::not_started(&files[i].path, ErrorCode::Cancelled, "cancelled before starting"))));
    Ok(batch_outcome("analyze", errors, cancel, json!({"jobs":jobs, "requested_jobs":args.batch.jobs.map(NonZeroUsize::get),
        "effective_template":effective_template(&template)}), None))
}
fn inspect(args: InspectArgs, settings: &Settings, cores: usize, cancel: &Cancellation) -> Outcome {
    let jobs = worker_count(args.jobs, settings, cores);
    let indices: Vec<usize> = (0..args.files.len()).collect();
    let mut results: Vec<Option<ItemResult>> = (0..args.files.len()).map(|_| None).collect();
    for (index, result) in parallel_items(&indices, jobs, cancel, |index| {
        let path = &args.files[index];
        match engine::inspect_file(path, settings) {
            Ok(report) => match serde_json::to_value(report) {
                Ok(report) => ItemResult::ok(path, report),
                Err(error) => ItemResult::failed(path, report_error(&error)),
            },
            Err(error) => ItemResult::engine_error(path, error),
        }
    }) { results[index] = Some(result); }
    let results = results.into_iter().enumerate().map(|(i, result)| result.unwrap_or_else(|| ItemResult::not_started(&args.files[i], ErrorCode::Cancelled, "cancelled before starting"))).collect();
    batch_outcome("inspect", results, cancel, json!({"jobs":jobs, "requested_jobs":args.jobs.map(NonZeroUsize::get)}), None)
}
fn restore(args: RestoreArgs, settings: &Settings, cancel: &Cancellation) -> Result<Outcome, AppError> {
    if args.files.len() != args.backup.len() { return Err(AppError::usage("supply one --backup PATH for each FILE, in the same order")); }
    let snapshot = file_workflow::RestoreBatchSnapshot::capture(&args.files, &args.backup).map_err(|error| {
        if matches!(&error, WorkflowError::RestoreInputConflict { .. } | WorkflowError::InvalidMarkerBackup) {
            AppError::usage(error.to_string())
        } else { AppError::workflow(&error) }
    })?;
    let mut results: Vec<Option<ItemResult>> = (0..args.files.len()).map(|_| None).collect();
    let mut prepared: Vec<Option<_>> = (0..args.files.len()).map(|_| None).collect();
    for (index, path) in args.files.iter().enumerate() {
        if cancel.is_cancelled() {
            results[index] = Some(ItemResult::not_started(path, ErrorCode::Cancelled, "cancelled before starting")); continue;
        }
        let marker = match engine::read_restore_marker(path, settings) {
            Ok((_, marker)) => marker,
            Err(error) => { results[index] = Some(ItemResult::engine_error(path, error)); continue; }
        };
        if let Err(error) = snapshot.target(index).verify(path) {
            results[index] = Some(ItemResult::workflow_error(path, error)); continue;
        }
        let status = if matches!(file_workflow::source_type(path), Some(SourceType::ProtectedM4p | SourceType::ProtectedWlMp3)) {
            RestoreStatus::Protected
        } else { RestoreStatus::Inactive };
        prepared[index] = Some((marker, status));
    }
    for members in snapshot.groups() {
        let ready: Vec<_> = members.iter().copied().filter(|&index| prepared[index].is_some()).collect();
        let allow_consumption = ready.len() == members.len();
        let requests: Vec<_> = ready.iter().map(|&index| {
            let (marker, status) = prepared[index].as_ref().unwrap();
            file_workflow::RestoreRequest { processed: &args.files[index], backup: &args.backup[index],
                marker: marker.as_ref(), status: *status, identity: snapshot.target(index), frozen_backup: snapshot.backup(index) }
        }).collect();
        for (index, outcome) in ready.into_iter().zip(file_workflow::restore_group(&requests, allow_consumption, cancel)) {
            results[index] = Some(match outcome {
                Ok(report) => restored_report(&args.files[index], report),
                Err(error) => ItemResult::workflow_error(&args.files[index], error),
            });
        }
    }
    let results = results.into_iter().enumerate().map(|(index, result)| result.unwrap_or_else(|| {
        ItemResult::not_started(&args.files[index], ErrorCode::Cancelled, "cancelled before restore")
    })).collect();
    Ok(batch_outcome("restore", results, cancel, json!({"backup_policy":"remove_after_verified_restore"}), None))
}

fn execute(cli: Cli, cancel: &Cancellation) -> Result<Outcome, AppError> {
    match cli.command {
        Command::Version => Ok(success("version", json!({"name":"platinum-engine", "version":env!("CARGO_PKG_VERSION")}))),
        Command::Provenance => Ok(success("provenance", json!({
            "name":"Platinum Engine", "version":env!("CARGO_PKG_VERSION"),
            "implementation":"independent Rust implementation; original Platinum Notes components are not called for processing",
            "reference_target":"Platinum Notes 10.5.0.1256",
            "complete_file_sha256_parity":"verified for one pinned Official-template/WAV fixture; not established for arbitrary inputs, settings or formats",
            "verified_fixture_sha256":"06dbf2b016bf3389aa997ce7dd9816a663d226bde5d38e2b4cb62c3abb0d0c1b"
        }))),
        command => {
            let home = std::env::var_os("HOME").filter(|s| !s.is_empty()).map(PathBuf::from)
                .ok_or_else(|| AppError::settings("HOME is not set; cannot resolve Engine defaults"))?;
            let cores = std::thread::available_parallelism().map(NonZeroUsize::get).unwrap_or(1);
            let (store, statistics_store) = match cli.state_dir {
                Some(path) => {
                    let store = SettingsStore::new(path, home, cores);
                    let statistics_store = StatisticsStore::new(store.directory());
                    (store, statistics_store)
                }
                None => {
                    #[cfg(target_os = "linux")]
                    let statistics_store = StatisticsStore::in_home(&home);
                    let store = SettingsStore::in_home(home, cores);
                    #[cfg(target_os = "macos")]
                    let statistics_store = StatisticsStore::new(store.directory());
                    (store, statistics_store)
                }
            };
            match command {
                Command::Statistics { command } => statistics(command, &statistics_store, cancel),
                command => {
                    let mut loaded = store.load().map_err(AppError::settings)?;
                    match command {
                        Command::Process(args) => process(args, loaded.settings, store.home(), cores, cancel, &statistics_store),
                        Command::Analyze(args) => analyze(args, loaded.settings, cores, cancel),
                        Command::Inspect(args) => Ok(inspect(args, &loaded.settings, cores, cancel)),
                        Command::Restore(args) => restore(args, &loaded.settings, cancel),
                        Command::Templates { command } => templates(command, &store, &mut loaded),
                        Command::Config { command } => config(command, &store, &mut loaded),
                        Command::Statistics { .. } | Command::Version | Command::Provenance => unreachable!(),
                    }
                }
            }
        }
    }
}
fn emit(value: &Value, machine: bool) {
    if machine { println!("{}", value); return; }
    if let Some(results) = value.get("results").and_then(Value::as_array) {
        println!("{}: {}", value["command"].as_str().unwrap_or("command"), value["status"].as_str().unwrap_or("unknown"));
        if let Some(details) = value.get("details") {
            println!("Run details:\n{}", serde_json::to_string_pretty(details).expect("JSON value serializes"));
        }
        if let Some(error) = value.get("batch_error").filter(|v| !v.is_null()) { eprintln!("batch preflight: {error}"); }
        for result in results {
            let path = result["input"].as_str().or_else(|| result["input"]["display"].as_str()).unwrap_or("<unknown path>");
            println!("{}: {}", path, result["status"].as_str().unwrap_or("unknown"));
            if let Some(report) = result.get("report") { println!("{}", serde_json::to_string_pretty(report).expect("JSON value serializes")); }
            if let Some(error) = result.get("error") { eprintln!("  {error}"); }
            if let Some(statistics) = result.get("statistics").filter(|v| v["status"] != "recorded") {
                eprintln!("  statistics: {statistics}");
            }
        }
    } else {
        println!("{}", serde_json::to_string_pretty(value).expect("JSON value serializes"));
    }
}
fn main() {
    let json_requested = std::env::args_os().any(|arg| arg == OsStr::new("--json"));
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if matches!(error.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            let _ = error.print();
            return;
        }
        Err(error) => {
            if json_requested { emit(&error_outcome("usage", ErrorCode::Usage, &error.to_string()), true); }
            else { let _ = error.print(); }
            std::process::exit(2);
        }
    };
    let machine = cli.json;
    let cancel = Arc::new(Cancellation::new());
    let signal_cancel = Arc::clone(&cancel);
    if let Err(error) = ctrlc::set_handler(move || signal_cancel.cancel()) {
        let value = error_outcome("signal", ErrorCode::SignalHandlerFailed, &format!("install Ctrl-C handler: {error}"));
        if machine { emit(&value, true); } else { eprintln!("{}", value["error"]["message"].as_str().unwrap_or("signal handler failed")); }
        std::process::exit(1);
    }
    match execute(cli, &cancel) {
        Ok(outcome) => { emit(&outcome.value, machine); std::process::exit(outcome.exit); }
        Err(error) => {
            let exit = if error.kind == "usage" { 2 } else if error.kind == "cancelled" { 130 } else { 1 };
            let mut value = error_outcome(error.kind, error.code, &error.message);
            if error.kind == "cancelled" { value["status"] = json!("cancelled"); }
            if let Some(publication) = error.publication { value["error"]["publication"] = json!(publication); }
            if let Some(cleanup) = error.cleanup { value["error"]["cleanup"] = cleanup; }
            if machine { emit(&value, true); } else { eprintln!("{}", value["error"]["message"].as_str().unwrap_or("operation failed")); }
            std::process::exit(exit);
        }
    }
}

#[cfg(test)]
mod statistics_outcome_tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    #[cfg_attr(target_os = "macos", link(name = "System"))]
    unsafe extern "C" { fn flock(fd: i32, operation: i32) -> i32; }

    #[test]
    fn elf_build_id_parser_checks_note_bounds_and_owner() {
        let mut note = Vec::new();
        note.extend_from_slice(&4u32.to_ne_bytes()); note.extend_from_slice(&3u32.to_ne_bytes());
        note.extend_from_slice(&3u32.to_ne_bytes()); note.extend_from_slice(b"GNU\0");
        note.extend_from_slice(&[0x01, 0xab, 0xff, 0]);
        assert_eq!(elf_gnu_build_id(&note).as_deref(), Some("01abff"));
        assert!(elf_gnu_build_id(&note[..note.len() - 1]).is_none());
        note[12] = b'X';
        assert!(elf_gnu_build_id(&note).is_none());
    }

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!("platinum-statistics-outcome-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create fixture: {error}"),
                }
            }
        }
        fn store(&self) -> StatisticsStore { StatisticsStore::new(&self.0) }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    fn report() -> engine::ProcessReport {
        engine::ProcessReport {
            input_path: "input.wav".into(), input_sample_rate: 48000.0, processing_sample_rate: 48000.0,
            output_path: "output.wav".into(), backup_path: None, replaced_existing_output: false, input_format: "WAV".into(), output_format: "WAV".into(),
            sample_rate: 48000.0, channels: 2, output_channels: 2, output_bit_depth: 16,
            encoded_length_frames: 1, input_frames: 1, healed_frames: 1, output_frames: 1,
            input_volume: 0.0, healed_volume: 0.0, output_volume: 0.6, input_pitch_hz: 440.0, output_pitch_hz: 440.0,
            tempo_factor: 1.0, clip_count: 3, applied_stages: Vec::new(),
            processing_marker_status: "present", input_marker_override: None, source_tags_unavailable: None, controlled_mp4_time_1904: None,
            non_identical_paths: Vec::new(), no_platinum_notes_reference: None, opus_output: None,
        }
    }

    #[test]
    fn restore_report_preserves_structured_backup_removal_failure() {
        let input = Path::new("/fixture/processed.wav");
        let report = file_workflow::RestoreReport {
            destination: input.to_owned(), backup: "/fixture/backup.wav".into(), backup_removed: false,
            backup_removal_error: Some(file_workflow::BackupRemovalError {
                operation: "remove verified backup", message: "permission denied".to_owned(),
                retained_path: Some("/fixture/backup.wav".into()), unexpected_path: None,
            }),
        };
        let item = restored_report(input, report);
        assert_eq!(item.status, "ok");
        let value = item.report.unwrap();
        assert_eq!(value["backup_removed"], false);
        assert_eq!(value["backup_removal_error"]["operation"], "remove verified backup");
        assert_eq!(value["backup_removal_error"]["path"], "/fixture/backup.wav");
        assert_eq!(value["backup_removal_error"]["retained_path"], "/fixture/backup.wav");
        assert!(value["backup_removal_error"]["unexpected_path"].is_null());
        assert_eq!(value["backup_removal_error"]["message"], "permission denied");
    }

    #[test]
    fn accounting_failure_retains_published_report_and_fails_batch() {
        let fixture = Fixture::new();
        let store = fixture.store();
        fs::write(store.statistics_path(), b"corrupt").unwrap();
        let item = processed_report(Path::new("input.wav"), report(), &store, &Cancellation::new());
        assert_eq!(item.status, "ok");
        assert_eq!(item.report.as_ref().unwrap()["output_path"], "output.wav");
        assert_eq!(item.statistics.as_ref().unwrap()["status"], "failed");
        assert_eq!(item.statistics.as_ref().unwrap()["publication"], "not_published");
        assert_eq!(item.statistics.as_ref().unwrap()["audio_published"], true);
        let outcome = batch_outcome("process", vec![item], &Cancellation::new(), json!({}), None);
        assert_eq!(outcome.exit, 1);
        assert_eq!(outcome.value["status"], "partial_failure");
        assert_eq!(outcome.value["results"][0]["report"]["clip_count"], 3);
        assert_eq!(fs::read(store.statistics_path()).unwrap(), b"corrupt");
    }

    #[test]
    fn uncertain_state_publication_is_not_reported_as_unpublished() {
        let error = StatisticsError::AfterPublish { path: "statistics.json".into(), source: io::Error::other("injected") };
        let accounting = statistics_accounting(Err(error));
        assert_eq!(accounting["publication"], "published_durability_uncertain");
        let mut item = ItemResult::ok(Path::new("input.wav"), json!({"output_path":"output.wav"}));
        item.statistics = Some(accounting);
        let outcome = batch_outcome("process", vec![item], &Cancellation::new(), json!({}), None);
        assert_eq!(outcome.exit, 1);
        assert_eq!(outcome.value["results"][0]["statistics"]["publication"], "published_durability_uncertain");
        let app_error = AppError::statistics(StatisticsError::AfterPublish {
            path: "statistics.json".into(), source: io::Error::other("injected"),
        });
        assert_eq!(app_error.publication, Some("published_durability_uncertain"));
    }

    #[test]
    fn cancellation_with_cleanup_failure_is_a_failure_with_retained_details() {
        let make_error = || StatisticsError::Cleanup {
            path: "staged.tmp".into(), cause: Box::new(StatisticsError::Cancelled),
            source: io::Error::other("injected cleanup failure"),
        };
        let accounting = statistics_accounting(Err(make_error()));
        assert_eq!(accounting["status"], "failed");
        assert_eq!(accounting["publication"], "not_published");
        assert_eq!(accounting["cleanup"]["stage_path"], "staged.tmp");
        assert_eq!(accounting["cleanup"]["cause_kind"], "cancelled");
        assert!(accounting["cleanup"]["cause"].is_string());
        let cancel = Cancellation::new();
        cancel.cancel();
        let mut item = ItemResult::ok(Path::new("input.wav"), json!({"output_path":"output.wav"}));
        item.statistics = Some(accounting);
        let outcome = batch_outcome("process", vec![item], &cancel, json!({}), None);
        assert_eq!(outcome.exit, 1);
        assert_eq!(outcome.value["status"], "partial_failure");
        assert_eq!(outcome.value["results"][0]["report"]["output_path"], "output.wav");
        let app_error = AppError::statistics(make_error());
        assert_eq!(app_error.kind, "statistics");
        assert_eq!(app_error.cleanup.unwrap()["stage_path"], "staged.tmp");
    }

    #[test]
    fn nested_stage_cleanup_failure_survives_cancellation_as_a_failure() {
        let make_error = || StatisticsError::Storage(SettingsError::Cleanup {
            operation: "remove temporary file", path: "retained-stage.tmp".into(),
            cause: Box::new(SettingsError::Io { operation: "write temporary file", path: "retained-stage.tmp".into(),
                source: io::Error::other("injected write failure") }),
            source: io::Error::other("injected cleanup failure"),
        });
        let accounting = statistics_accounting(Err(make_error()));
        assert_eq!(accounting["status"], "failed");
        assert_eq!(accounting["publication"], "not_published");
        assert_eq!(accounting["cleanup"]["stage_path"], "retained-stage.tmp");
        assert_eq!(accounting["cleanup"]["cause_kind"], "error");
        assert!(accounting["cleanup"]["cause"].is_string());
        let cancel = Cancellation::new();
        cancel.cancel();
        let mut item = ItemResult::ok(Path::new("input.wav"), json!({"output_path":"output.wav"}));
        item.statistics = Some(accounting);
        let outcome = batch_outcome("process", vec![item], &cancel, json!({}), None);
        assert_eq!(outcome.exit, 1);
        assert_eq!(outcome.value["status"], "partial_failure");
        assert_eq!(outcome.value["results"][0]["report"]["output_path"], "output.wav");
        let app_error = AppError::statistics(make_error());
        assert_eq!(app_error.kind, "statistics");
        assert_eq!(app_error.cleanup.unwrap()["stage_path"], "retained-stage.tmp");
    }

    #[test]
    fn cancellation_before_dispatch_does_not_record_but_after_publication_does() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let cancel = Cancellation::new();
        cancel.cancel();
        let started = AtomicBool::new(false);
        let items = parallel_items(&[0], 1, &cancel, |_| {
            started.store(true, Ordering::Relaxed);
            processed_report(Path::new("input.wav"), report(), &store, &cancel)
        });
        assert!(items.is_empty());
        assert!(!started.load(Ordering::Relaxed));
        assert_eq!(store.show(&Cancellation::new()).unwrap().processed_songs, 0);
        // Simulate the independent cancellation arriving after Engine returned
        // a published report: the returned report must still be counted.
        let item = processed_report(Path::new("input.wav"), report(), &store, &cancel);
        let outcome = batch_outcome("process", vec![item], &cancel, json!({}), None);
        assert_eq!(outcome.exit, 130);
        assert_eq!(outcome.value["results"][0]["statistics"]["status"], "recorded");
        assert_eq!(store.show(&Cancellation::new()).unwrap().processed_songs, 1);
    }

    #[test]
    fn failed_engine_result_is_not_counted_even_if_audio_was_partly_published() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let error = EngineError { stage: "publish output", code: ErrorCode::IoError, message: "injected failure".into(),
            cancelled: false, retained_backup: None, recovery_path: Some("/fixture/recovery".into()), partial_publication: true };
        let item = process_result(Path::new("input.wav"), Err(error), &store, &Cancellation::new());
        assert_eq!(item.status, "failed");
        let error = item.error.unwrap();
        assert_eq!(error["partial_publication"], true);
        assert_eq!(error["recovery_path"], "/fixture/recovery");
        assert!(item.statistics.is_none());
        assert_eq!(store.show(&Cancellation::new()).unwrap().processed_songs, 0);
    }

    #[test]
    fn restore_error_reports_rollback_recovery_and_cleanup_together() {
        let error = WorkflowError::Transaction {
            cause: Box::new(WorkflowError::BackupChanged("/fixture/backup".into())),
            backup: None, recovery: Some("/fixture/original".into()), rollback: None, published: true,
            cleanup: vec![file_workflow::CleanupFailure { operation: "release reservation",
                path: "/fixture/reservation".into(), source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied") }],
        };
        let value = workflow_error(error);
        assert_eq!(value["retained_original"], "/fixture/original");
        assert_eq!(value["partial_publication"], true);
        assert_eq!(value["cleanup_errors"][0]["path"], "/fixture/reservation");
    }

    #[test]
    fn report_serialization_failure_still_records_and_retains_output_path() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = Fixture::new();
        let store = fixture.store();
        let mut published = report();
        published.output_path = std::ffi::OsString::from_vec(b"output-\xff.wav".to_vec()).into();
        let item = process_result(Path::new("input.wav"), Ok(published), &store, &Cancellation::new());
        assert_eq!(item.status, "failed");
        assert_eq!(item.report.as_ref().unwrap()["output_path"]["unix_bytes_hex"], "6f75747075742dff2e776176");
        assert_eq!(item.report.as_ref().unwrap()["clip_count"], 3);
        assert_eq!(item.error.as_ref().unwrap()["partial_publication"], true);
        assert_eq!(item.statistics.as_ref().unwrap()["status"], "recorded");
        assert_eq!(store.show(&Cancellation::new()).unwrap().processed_songs, 1);
    }

    #[test]
    fn cancelled_contended_accounting_retains_published_report() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.reset(&Cancellation::new()).unwrap();
        let before = fs::read(store.statistics_path()).unwrap();
        let held = fs::OpenOptions::new().read(true).write(true).open(fixture.0.join("statistics.lock")).unwrap();
        // SAFETY: held owns a valid descriptor until it is dropped below.
        assert_eq!(unsafe { flock(held.as_raw_fd(), 2) }, 0);
        let cancel = Cancellation::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                started_tx.send(()).unwrap();
                result_tx.send(processed_report(Path::new("input.wav"), report(), &store, &cancel)).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            cancel.cancel();
            let received = result_rx.recv_timeout(Duration::from_secs(3));
            let unchanged = fs::read(store.statistics_path()).unwrap();
            drop(held);
            let item = received.expect("accounting did not stop while the lock remained held");
            assert_eq!(unchanged, before);
            assert_eq!(item.status, "ok");
            assert_eq!(item.report.as_ref().unwrap()["output_path"], "output.wav");
            assert_eq!(item.statistics.as_ref().unwrap()["status"], "not_recorded");
            assert_eq!(item.statistics.as_ref().unwrap()["reason"], "cancelled");
            assert_eq!(item.statistics.as_ref().unwrap()["publication"], "not_published");
            let outcome = batch_outcome("process", vec![item], &cancel, json!({}), None);
            assert_eq!(outcome.exit, 130);
            assert_eq!(outcome.value["results"][0]["statistics"]["audio_published"], true);
        });
    }
}

#[cfg(test)]
mod error_code_json_tests {
    use super::*;

    fn code(value: &Value) -> &str { value["code"].as_str().expect("every error object has a code") }

    #[test]
    fn every_item_error_shape_carries_a_code_next_to_its_message() {
        let started = serde_json::to_value(ItemResult::not_started(Path::new("a.wav"), ErrorCode::BatchPreflightFailed, "batch preflight failed")).unwrap();
        assert_eq!(started["status"], "not_started");
        assert_eq!((code(&started["error"]), started["error"]["message"].as_str()), ("batch_preflight_failed", Some("batch preflight failed")));
        let cancelled = serde_json::to_value(ItemResult::not_started(Path::new("a.wav"), ErrorCode::Cancelled, "cancelled before starting")).unwrap();
        assert_eq!(code(&cancelled["error"]), "cancelled");

        let engine = EngineError { stage: "open input", code: ErrorCode::UnsupportedInputCodec, message: "MP3 decoding is unavailable".into(),
            cancelled: false, retained_backup: None, recovery_path: None, partial_publication: false };
        let item = serde_json::to_value(ItemResult::engine_error(Path::new("a.mp3"), engine)).unwrap();
        assert_eq!(item["status"], "failed");
        assert_eq!(code(&item["error"]), "unsupported_input_codec");
        assert_eq!(item["error"]["stage"], "open input");

        let workflow = serde_json::to_value(ItemResult::workflow_error(Path::new("a.wav"), WorkflowError::DestinationExists("/x".into()))).unwrap();
        assert_eq!(code(&workflow["error"]), "output_exists");
        let transaction = workflow_error(WorkflowError::Transaction {
            cause: Box::new(WorkflowError::SourceChanged("/x".into())), backup: None, recovery: None, rollback: None, cleanup: Vec::new(), published: false,
        });
        assert_eq!(code(&transaction), "source_changed");

        let report = report_error(&serde_json::from_str::<Value>("{").unwrap_err());
        assert_eq!((code(&report), report["stage"].as_str()), ("report_serialization_failed", Some("report")));
        let discovery = serde_json::to_value(ItemResult::failed(Path::new("d"), json!({"stage":"discovery", "code":ErrorCode::NoInputFiles, "message":"m"}))).unwrap();
        assert_eq!(code(&discovery["error"]), "no_input_files");
    }

    #[test]
    fn top_level_errors_carry_kind_and_code() {
        let signal = error_outcome("signal", ErrorCode::SignalHandlerFailed, "install Ctrl-C handler: x");
        assert_eq!((signal["status"].as_str(), signal["error"]["kind"].as_str(), code(&signal["error"])), (Some("error"), Some("signal"), "signal_handler_failed"));
        let kinds = [
            (AppError::usage("u"), "usage", "usage"), (AppError::settings("s"), "settings", "settings_error"),
            (AppError::io("i"), "io", "io_error"),
            (AppError::workflow(&WorkflowError::AtomicRenameUnsupported { operation: "op", path: "/x".into(), source: std::io::Error::other("x") }), "io", "atomic_rename_unsupported"),
            (AppError::statistics(StatisticsError::Corrupt("c".into())), "statistics", "statistics_error"),
            (AppError::statistics(StatisticsError::Cancelled), "cancelled", "cancelled"),
        ];
        for (error, kind, expected) in kinds {
            assert_eq!(error.kind, kind);
            assert_eq!(error.code.as_str(), expected, "{}", error.message);
        }
    }

    #[test]
    fn every_failure_shaped_object_carries_a_code() {
        let io = || std::io::Error::other("x");
        let failed = statistics_accounting(Err(StatisticsError::Corrupt("c".into())));
        assert_eq!((failed["status"].as_str(), code(&failed)), (Some("failed"), "statistics_error"));
        let cancelled = statistics_accounting(Err(StatisticsError::Cancelled));
        assert_eq!((cancelled["status"].as_str(), code(&cancelled)), (Some("not_recorded"), "cancelled"));
        let cleanup_error = || StatisticsError::Cleanup { path: "/x".into(), cause: Box::new(StatisticsError::Cancelled), source: std::io::Error::other("x") };
        let with_cleanup = statistics_accounting(Err(cleanup_error()));
        assert_eq!(code(&with_cleanup), "statistics_error");
        assert_eq!(code(&with_cleanup["cleanup"]), "io_error");
        let top_level = AppError::statistics(cleanup_error());
        assert_eq!(code(top_level.cleanup.as_ref().expect("cleanup object")), "io_error");

        let rolled_back = |rollback: Option<WorkflowError>| workflow_error(WorkflowError::Transaction {
            cause: Box::new(WorkflowError::SourceChanged("/x".into())), backup: None, recovery: None,
            rollback: rollback.map(Box::new),
            cleanup: vec![file_workflow::CleanupFailure { operation: "remove", path: "/y".into(), source: io() }], published: true,
        });
        let none = rolled_back(None);
        assert!(none["rollback_error"].is_null() && none["rollback_error_code"].is_null());
        assert_eq!(code(&none["cleanup_errors"][0]), "io_error");
        let failed_rollback = rolled_back(Some(WorkflowError::SourceChanged("/z".into())));
        assert!(failed_rollback["rollback_error"].is_string(), "the string form stays for compatibility");
        assert_eq!(failed_rollback["rollback_error_code"], "source_changed");
        let cancelled_rollback = rolled_back(Some(WorkflowError::Cancelled));
        assert_eq!(cancelled_rollback["rollback_error_code"], "io_error", "a rollback that failed is never a clean cancel");

        let restored = serde_json::to_value(restored_report(Path::new("a.wav"), file_workflow::RestoreReport {
            destination: "/a.wav".into(), backup: "/b.wav".into(), backup_removed: false,
            backup_removal_error: Some(file_workflow::BackupRemovalError { operation: "sync", message: "m".into(), retained_path: None, unexpected_path: None }),
        })).unwrap();
        assert_eq!(restored["status"], "ok");
        assert_eq!(code(&restored["report"]["backup_removal_error"]), "io_error");
    }
}
