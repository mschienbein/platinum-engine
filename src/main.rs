use clap::{Args, Parser, Subcommand, ValueEnum, error::ErrorKind};
use platinum_engine::engine::{self, EngineError};
use platinum_engine::file_workflow::{self, AudioFormat, Cancellation, DiscoveredFile, OutputMode, RestoreStatus, SourceType, WorkflowError};
use platinum_engine::settings::{LoadedSettings, SelectedTemplate, Settings, SettingsStore, TemplateRef};
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
enum FormatArg { Wav, Aiff, Alac, Flac, Mp3, Aac, MatchInput }
impl From<FormatArg> for AudioFormat {
    fn from(format: FormatArg) -> Self {
        match format {
            FormatArg::Wav => Self::Wav, FormatArg::Aiff => Self::Aiff,
            FormatArg::Alac => Self::Alac, FormatArg::Flac => Self::Flac,
            FormatArg::Mp3 => Self::Mp3, FormatArg::Aac => Self::Aac,
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
    /// Absolute recorded backup for the corresponding FILE; repeat for each file.
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

#[derive(Debug)]
struct AppError { kind: &'static str, message: String }
impl AppError {
    fn usage(message: impl Into<String>) -> Self { Self { kind: "usage", message: message.into() } }
    fn settings(error: impl std::fmt::Display) -> Self { Self { kind: "settings", message: error.to_string() } }
    fn io(message: impl Into<String>) -> Self { Self { kind: "io", message: message.into() } }
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
}
impl ItemResult {
    fn ok(input: &Path, report: Value) -> Self { Self { input: path_value(input), status: "ok", report: Some(report), error: None } }
    fn failed(input: &Path, error: Value) -> Self { Self { input: path_value(input), status: "failed", report: None, error: Some(error) } }
    fn not_started(input: &Path, message: &str) -> Self {
        Self { input: path_value(input), status: "not_started", report: None, error: Some(json!({"message": message})) }
    }
    fn engine_error(input: &Path, error: EngineError) -> Self {
        Self { input: path_value(input), status: if error.cancelled { "cancelled" } else { "failed" }, report: None,
            error: Some(json!({"stage": error.stage, "message": error.message, "cancelled": error.cancelled,
                "retained_backup": error.retained_backup.as_deref().map(path_value), "partial_publication": error.partial_publication})) }
    }
    fn workflow_error(input: &Path, error: WorkflowError) -> Self {
        let cancelled = error.is_cancelled();
        Self { input: path_value(input), status: if cancelled { "cancelled" } else { "failed" },
            report: None, error: Some(workflow_error(error)) }
    }
}
fn workflow_error(error: WorkflowError) -> Value {
    let message = error.to_string();
    match error {
        WorkflowError::Transaction { backup, recovery, rollback, cleanup, published, .. } => json!({
            "message": message, "retained_backup": backup.as_deref().map(path_value), "retained_original": recovery.as_deref().map(path_value),
            "rollback_error": rollback.map(|e| e.to_string()), "partial_publication": published,
            "cleanup_errors": cleanup.into_iter().map(|e| json!({"operation":e.operation, "path":path_value(&e.path), "message":e.source.to_string()})).collect::<Vec<_>>()
        }),
        _ => json!({"message": message}),
    }
}
fn batch_outcome(command: &str, results: Vec<ItemResult>, cancel: &Cancellation, extras: Value, batch_error: Option<Value>) -> Outcome {
    let ok = results.iter().filter(|r| r.status == "ok").count();
    let failed = results.iter().any(|r| r.status == "failed" || r.status == "not_started") || batch_error.is_some();
    let cancelled = cancel.is_cancelled() || results.iter().any(|r| r.status == "cancelled");
    let (status, exit) = if cancelled { ("cancelled", 130) }
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
fn discovery(inputs: &[PathBuf]) -> (Vec<DiscoveredFile>, Vec<ItemResult>) {
    let mut files = BTreeMap::new();
    let mut errors = Vec::new();
    for input in inputs {
        match file_workflow::discover_inputs(std::slice::from_ref(input)) {
            Ok(found) if found.is_empty() => errors.push(ItemResult::failed(input, json!({"stage":"discovery", "message":"no admitted audio files found"}))),
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

fn process(args: ProcessArgs, mut settings: Settings, home: &Path, cores: usize, cancel: &Cancellation) -> Result<Outcome, AppError> {
    apply_process_options(&mut settings, &args, cores)?;
    let template = settings.selected_template().map_err(AppError::settings)?;
    let jobs = worker_count(args.batch.jobs, &settings, cores);
    let (files, mut errors) = discovery(&args.batch.inputs);
    let details = json!({"jobs": jobs, "requested_jobs": args.batch.jobs.map(NonZeroUsize::get),
        "effective_config": settings.config_show().map_err(AppError::settings)?, "effective_template": effective_template(&template)});
    if files.is_empty() { return Ok(batch_outcome("process", errors, cancel, details, None)); }
    if !cancel.is_cancelled() { prepare_output(&settings, home)?; }
    let mut results: Vec<Option<ItemResult>> = (0..files.len()).map(|_| None).collect();
    let mut prepared = Vec::new();
    let mut ready = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if cancel.is_cancelled() { break; }
        match engine::prepare_file(file, &settings) {
            Ok(item) => { prepared.push(item); ready.push(index); }
            Err(error) => results[index] = Some(ItemResult::engine_error(&file.path, error)),
        }
    }
    let plans: Vec<_> = prepared.iter().map(|item| item.output_plan()).collect();
    let conflict = file_workflow::validate_batch(&plans).err().map(workflow_error).or_else(|| {
        // Also protect selected inputs whose own preflight failed.
        plans.iter().find_map(|plan| files.iter().find(|file| plan.destination() == file.path.as_path() && plan.source() != file.path.as_path())
            .map(|file| json!({"message": format!("destination {} conflicts with selected input {}", plan.destination().display(), file.path.display())})))
    });
    if conflict.is_none() && !cancel.is_cancelled() {
        let slots: Vec<_> = (0..ready.len()).collect();
        for (slot, result) in parallel_items(&slots, jobs, cancel, |slot| {
            let file = &files[ready[slot]];
            match engine::process_file(&prepared[slot], &template, &settings, cancel) {
                Ok(report) => match serde_json::to_value(report) {
                    Ok(report) => ItemResult::ok(&file.path, report),
                    Err(error) => ItemResult::failed(&file.path, json!({"stage":"report", "message":error.to_string()})),
                },
                Err(error) => ItemResult::engine_error(&file.path, error),
            }
        }) { results[ready[slot]] = Some(result); }
    }
    errors.extend(results.into_iter().enumerate().map(|(i, result)| result.unwrap_or_else(|| {
        ItemResult::not_started(&files[i].path, if cancel.is_cancelled() { "cancelled before starting" } else { "batch preflight failed" })
    })));
    Ok(batch_outcome("process", errors, cancel, details, conflict))
}
fn analyze(args: AnalyzeArgs, mut settings: Settings, cores: usize, cancel: &Cancellation) -> Result<Outcome, AppError> {
    apply_run_template(&mut settings, &args.template)?;
    let jobs = worker_count(args.batch.jobs, &settings, cores);
    let template = settings.selected_template().map_err(AppError::settings)?;
    let (files, mut errors) = discovery(&args.batch.inputs);
    let indices: Vec<usize> = (0..files.len()).collect();
    let mut results: Vec<Option<ItemResult>> = (0..files.len()).map(|_| None).collect();
    for (index, result) in parallel_items(&indices, jobs, cancel, |index| {
        let file = &files[index];
        match engine::analyze_file(&file.path, &settings, cancel) {
            Ok(report) => match serde_json::to_value(report) {
                Ok(report) => ItemResult::ok(&file.path, report),
                Err(error) => ItemResult::failed(&file.path, json!({"stage":"report", "message":error.to_string()})),
            },
            Err(error) => ItemResult::engine_error(&file.path, error),
        }
    }) { results[index] = Some(result); }
    errors.extend(results.into_iter().enumerate().map(|(i, result)| result.unwrap_or_else(|| ItemResult::not_started(&files[i].path, "cancelled before starting"))));
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
                Err(error) => ItemResult::failed(path, json!({"stage":"report", "message":error.to_string()})),
            },
            Err(error) => ItemResult::engine_error(path, error),
        }
    }) { results[index] = Some(result); }
    let results = results.into_iter().enumerate().map(|(i, result)| result.unwrap_or_else(|| ItemResult::not_started(&args.files[i], "cancelled before starting"))).collect();
    batch_outcome("inspect", results, cancel, json!({"jobs":jobs, "requested_jobs":args.jobs.map(NonZeroUsize::get)}), None)
}
fn restore(args: RestoreArgs, settings: &Settings, cancel: &Cancellation) -> Result<Outcome, AppError> {
    if args.files.len() != args.backup.len() { return Err(AppError::usage("supply one --backup PATH for each FILE, in the same order")); }
    let mut results = Vec::with_capacity(args.files.len());
    for (path, backup) in args.files.iter().zip(&args.backup) {
        if cancel.is_cancelled() { results.push(ItemResult::not_started(path, "cancelled before starting")); continue; }
        let (identity, marker) = match engine::read_restore_marker(path, settings) {
            Ok(result) => result,
            Err(error) => { results.push(ItemResult::engine_error(path, error)); continue; }
        };
        if cancel.is_cancelled() { results.push(ItemResult::not_started(path, "cancelled before restore")); continue; }
        let status = if matches!(file_workflow::source_type(path), Some(SourceType::ProtectedM4p | SourceType::ProtectedWlMp3)) {
            RestoreStatus::Protected
        } else { RestoreStatus::Inactive };
        match file_workflow::restore(path, backup, marker.as_ref(), status, &identity, cancel) {
            Ok(report) => results.push(ItemResult::ok(path, json!({"destination": path_value(&report.destination), "backup": path_value(&report.backup)}))),
            Err(error) => results.push(ItemResult::workflow_error(path, error)),
        }
    }
    Ok(batch_outcome("restore", results, cancel, json!({"backup_policy":"retained_on_success"}), None))
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
            let store = match cli.state_dir { Some(path) => SettingsStore::new(path, home, cores), None => SettingsStore::in_home(home, cores) };
            let mut loaded = store.load().map_err(AppError::settings)?;
            match command {
                Command::Process(args) => process(args, loaded.settings, store.home(), cores, cancel),
                Command::Analyze(args) => analyze(args, loaded.settings, cores, cancel),
                Command::Inspect(args) => Ok(inspect(args, &loaded.settings, cores, cancel)),
                Command::Restore(args) => restore(args, &loaded.settings, cancel),
                Command::Templates { command } => templates(command, &store, &mut loaded),
                Command::Config { command } => config(command, &store, &mut loaded),
                Command::Version | Command::Provenance => unreachable!(),
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
            if json_requested { emit(&json!({"status":"error", "error":{"kind":"usage", "message":error.to_string()}}), true); }
            else { let _ = error.print(); }
            std::process::exit(2);
        }
    };
    let machine = cli.json;
    let cancel = Arc::new(Cancellation::new());
    let signal_cancel = Arc::clone(&cancel);
    if let Err(error) = ctrlc::set_handler(move || signal_cancel.cancel()) {
        let value = json!({"status":"error", "error":{"kind":"signal", "message":format!("install Ctrl-C handler: {error}")}});
        if machine { emit(&value, true); } else { eprintln!("{}", value["error"]["message"].as_str().unwrap_or("signal handler failed")); }
        std::process::exit(1);
    }
    match execute(cli, &cancel) {
        Ok(outcome) => { emit(&outcome.value, machine); std::process::exit(outcome.exit); }
        Err(error) => {
            let exit = if error.kind == "usage" { 2 } else { 1 };
            let value = json!({"status":"error", "error":{"kind":error.kind, "message":error.message}});
            if machine { emit(&value, true); } else { eprintln!("{}", value["error"]["message"].as_str().unwrap_or("operation failed")); }
            std::process::exit(exit);
        }
    }
}
