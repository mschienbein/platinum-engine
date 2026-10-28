//! Persistent Platinum Engine preferences and template management.
//!
//! This state belongs to Platinum Engine, not the original application's
//! preferences. Templates are stored as their effective, bit-exact JSON state;
//! native XML is used only for explicit import/export. A loaded snapshot is an
//! optimistic transaction: cooperating CLI writers lock the same permanent lock
//! file, compare the original bytes, and atomically publish a new revision.

use crate::file_workflow::{AudioFormat, OutputMode, OutputSettings};
use crate::templates::{self, Band, IgnoredAttribute, IgnoredElement, SerializationLoss, Template, TemplateCatalog, TemplateError};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::error::Error;
use std::ffi::{c_int, c_void};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const STATE_VERSION: u32 = 1;
const SETTINGS_FILE: &str = "settings.json";
const LOCK_FILE: &str = "settings.lock";

#[derive(Debug)]
pub enum SettingsError {
    Io { operation: &'static str, path: PathBuf, source: io::Error },
    /// Publication succeeded, but the named cleanup or durability operation failed.
    AfterPublish { operation: &'static str, path: PathBuf, source: io::Error },
    /// Export succeeded but its temporary hard link could not be removed.
    PublishedCleanup { destination: PathBuf, temporary: PathBuf, source: io::Error },
    Cleanup { operation: &'static str, path: PathBuf, cause: Box<SettingsError>, source: io::Error },
    Json(serde_json::Error),
    Template(TemplateError),
    Corrupt(String),
    InvalidField { field: String, reason: String },
    ConcurrentModification(PathBuf),
    WrongStore,
    TemplateNotFound(String),
    AmbiguousTemplateId(String),
    BuiltinImmutable(usize),
    UuidUnavailable,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, path, source } => write!(f, "{operation} {}: {source}", path.display()),
            Self::AfterPublish { operation, path, source } => write!(f, "{} was published, but {operation} failed: {source}", path.display()),
            Self::PublishedCleanup { destination, temporary, source } => write!(f, "{} was published, but removing temporary {} failed: {source}", destination.display(), temporary.display()),
            Self::Cleanup { operation, path, cause, source } => write!(f, "{cause}; additionally {operation} {} failed: {source}", path.display()),
            Self::Json(e) => write!(f, "settings JSON: {e}"),
            Self::Template(e) => write!(f, "{e}"),
            Self::Corrupt(message) => write!(f, "invalid settings state: {message}"),
            Self::InvalidField { field, reason } => write!(f, "invalid {field}: {reason}"),
            Self::ConcurrentModification(path) => write!(f, "settings changed since load: {}", path.display()),
            Self::WrongStore => write!(f, "settings snapshot belongs to a different store"),
            Self::TemplateNotFound(reference) => write!(f, "template not found: {reference}"),
            Self::AmbiguousTemplateId(id) => write!(f, "template ID {id:?} is ambiguous; use a manager index"),
            Self::BuiltinImmutable(index) => write!(f, "built-in template at index {index} cannot be edited or removed"),
            Self::UuidUnavailable => write!(f, "could not create a fresh UUID"),
        }
    }
}
impl Error for SettingsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::AfterPublish { source, .. } | Self::PublishedCleanup { source, .. } => Some(source),
            Self::Cleanup { cause, .. } => Some(cause.as_ref()),
            Self::Json(e) => Some(e),
            Self::Template(e) => Some(e),
            _ => None,
        }
    }
}
impl From<TemplateError> for SettingsError { fn from(e: TemplateError) -> Self { Self::Template(e) } }
impl From<serde_json::Error> for SettingsError { fn from(e: serde_json::Error) -> Self { Self::Json(e) } }
fn io_error(operation: &'static str, path: &Path, source: io::Error) -> SettingsError {
    SettingsError::Io { operation, path: path.to_owned(), source }
}
fn invalid(field: &str, reason: impl Into<String>) -> SettingsError {
    SettingsError::InvalidField { field: field.into(), reason: reason.into() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mp3Mode { Cbr320, Vbr192 }
impl From<Mp3Mode> for crate::mp3::Mp3Mode {
    fn from(value: Mp3Mode) -> Self {
        match value { Mp3Mode::Cbr320 => Self::Cbr320, Mp3Mode::Vbr192 => Self::Vbr192 }
    }
}

/// Explicitly chosen public libraries; no original-app fallback is performed.
/// Caller-pinned full-file SHA-256 values accompany the FLAC and LAME paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryPaths {
    pub flac: Option<PathBuf>,
    pub flac_sha256: Option<String>,
    pub lame: Option<PathBuf>,
    pub lame_sha256: Option<String>,
    pub tag_bridge: Option<PathBuf>,
}

/// Custom indices are zero-based within `custom_templates`, and carry the
/// imported ID (including null/empty) as a guard against a stale index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectedTemplate {
    Builtin { id: String },
    Custom { index: usize, id: Option<String> },
}

/// An index addresses the complete manager order: built-ins followed by custom.
/// IDs are exact and case-sensitive; an ambiguous ID is never guessed.
#[derive(Debug, Clone, Copy)]
pub enum TemplateRef<'a> { Index(usize), Id(&'a str) }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub output: OutputSettings,
    pub processor_max: usize,
    pub mp3_mode: Mp3Mode,
    pub libraries: LibraryPaths,
    pub selected_template: Option<SelectedTemplate>,
    pub custom_templates: Vec<Template>,
}

#[derive(Serialize)]
struct ConfigView<'a> {
    output: &'a OutputSettings,
    processor_max: usize,
    mp3_mode: Mp3Mode,
    libraries: &'a LibraryPaths,
    selected_template: &'a Option<SelectedTemplate>,
    effective_selected_template: SelectedTemplate,
}

fn cores(available_cores: usize) -> usize { available_cores.max(1) }
fn path_nonempty(path: &Path, field: &str) -> Result<(), SettingsError> {
    if path.as_os_str().is_empty() { Err(invalid(field, "path cannot be empty")) } else { Ok(()) }
}
fn decode<T: DeserializeOwned>(field: &str, value: Value) -> Result<T, SettingsError> {
    serde_json::from_value(value).map_err(|e| invalid(field, e.to_string()))
}

impl Settings {
    /// The reference defaults to MP3/CBR and the processor count. Its original
    /// destination is deliberately not reused: choose a separate new-output
    /// directory and create it explicitly before processing, if it does not exist.
    pub fn defaults(home: &Path, available_cores: usize) -> Self {
        Self {
            output: OutputSettings {
                mode: OutputMode::SpecificFolder { directory: home.join("Music/Platinum Engine Output"), mirror: false },
                format: AudioFormat::Mp3,
            },
            processor_max: cores(available_cores),
            mp3_mode: Mp3Mode::Cbr320,
            libraries: LibraryPaths::default(),
            selected_template: None,
            custom_templates: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), SettingsError> {
        match &self.output.mode {
            OutputMode::SpecificFolder { directory, .. } => path_nonempty(directory, "output.mode.specific_folder.directory")?,
            OutputMode::Replace { backup_directory } => {
                path_nonempty(backup_directory, "output.mode.replace.backup_directory")?;
                if self.output.format != AudioFormat::MatchInput {
                    return Err(invalid("output.format", "replacement requires match_input"));
                }
            }
            OutputMode::SameFolder => {}
        }
        for (field, path) in [("libraries.flac", &self.libraries.flac), ("libraries.lame", &self.libraries.lame), ("libraries.tag_bridge", &self.libraries.tag_bridge)] {
            if let Some(path) = path { path_nonempty(path, field)?; }
        }
        for (field, digest) in [("libraries.flac_sha256", &self.libraries.flac_sha256), ("libraries.lame_sha256", &self.libraries.lame_sha256)] {
            if let Some(digest) = digest {
                if digest.len() != 64 || !digest.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return Err(invalid(field, "expected 64 hexadecimal SHA-256 digits"));
                }
            }
        }
        if self.custom_templates.iter().any(|t| t.is_builtin) {
            return Err(SettingsError::Corrupt("custom_templates contains a built-in object".into()));
        }
        self.selected_template()?;
        Ok(())
    }

    /// Preferences only; templates themselves are available through `catalog`.
    pub fn config_show(&self) -> Result<Value, SettingsError> {
        Ok(serde_json::to_value(ConfigView {
            output: &self.output, processor_max: self.processor_max, mp3_mode: self.mp3_mode,
            libraries: &self.libraries, selected_template: &self.selected_template,
            effective_selected_template: self.effective_selection()?,
        })?)
    }
    pub fn config_get(&self, field: &str) -> Result<Value, SettingsError> {
        let value = match field {
            "output" => serde_json::to_value(&self.output)?,
            "output.mode" => serde_json::to_value(&self.output.mode)?,
            "output.format" => serde_json::to_value(self.output.format)?,
            "processor_max" => serde_json::to_value(self.processor_max)?,
            "mp3_mode" => serde_json::to_value(self.mp3_mode)?,
            "libraries" => serde_json::to_value(&self.libraries)?,
            "libraries.flac" => serde_json::to_value(&self.libraries.flac)?,
            "libraries.flac_sha256" => serde_json::to_value(&self.libraries.flac_sha256)?,
            "libraries.lame" => serde_json::to_value(&self.libraries.lame)?,
            "libraries.lame_sha256" => serde_json::to_value(&self.libraries.lame_sha256)?,
            "libraries.tag_bridge" => serde_json::to_value(&self.libraries.tag_bridge)?,
            "selected_template" => serde_json::to_value(&self.selected_template)?,
            "effective_selected_template" => serde_json::to_value(self.effective_selection()?)?,
            _ => return Err(invalid(field, "unknown configuration field")),
        };
        Ok(value)
    }
    /// Values are JSON values; output mode uses `OutputMode`'s existing serde
    /// representation. The candidate is validated before changing this object.
    pub fn config_set(&mut self, field: &str, value: Value, available_cores: usize) -> Result<(), SettingsError> {
        let mut next = self.clone();
        match field {
            "output" => {
                next.output = decode(field, value)?;
                if matches!(&next.output.mode, OutputMode::Replace { .. }) { next.output.format = AudioFormat::MatchInput; }
            }
            "output.mode" => {
                next.output.mode = decode(field, value)?;
                if matches!(&next.output.mode, OutputMode::Replace { .. }) { next.output.format = AudioFormat::MatchInput; }
            }
            "output.format" => next.output.format = decode(field, value)?,
            "processor_max" => {
                let n = if let Some(n) = value.as_u64() { n.min(cores(available_cores) as u64) as usize }
                    else if value.as_i64().is_some() { 1 }
                    else { return Err(invalid(field, "expected an integer")); };
                next.processor_max = n.clamp(1, cores(available_cores));
            }
            "mp3_mode" => next.mp3_mode = decode(field, value)?,
            "libraries" => next.libraries = decode(field, value)?,
            "libraries.flac" => next.libraries.flac = decode(field, value)?,
            "libraries.flac_sha256" => next.libraries.flac_sha256 = decode(field, value)?,
            "libraries.lame" => next.libraries.lame = decode(field, value)?,
            "libraries.lame_sha256" => next.libraries.lame_sha256 = decode(field, value)?,
            "libraries.tag_bridge" => next.libraries.tag_bridge = decode(field, value)?,
            "selected_template" => next.selected_template = decode(field, value)?,
            _ => return Err(invalid(field, "unknown configuration field")),
        }
        next.processor_max = next.processor_max.clamp(1, cores(available_cores));
        next.validate()?;
        *self = next;
        Ok(())
    }
    /// Reset all preferences (without deleting custom templates), or one field.
    pub fn config_reset(&mut self, field: Option<&str>, home: &Path, available_cores: usize) -> Result<(), SettingsError> {
        let defaults = Self::defaults(home, available_cores);
        if let Some(field) = field {
            self.config_set(field, defaults.config_get(field)?, available_cores)
        } else {
            let mut next = defaults;
            next.custom_templates = self.custom_templates.clone();
            next.validate()?;
            *self = next;
            Ok(())
        }
    }

    /// Built-ins are embedded and always precede custom templates. The native
    /// catalog's `selected()` resolves duplicate IDs to the first match; use
    /// this module's `selected_template()` for the exact index selection.
    pub fn catalog(&self) -> Result<TemplateCatalog, SettingsError> {
        let selected_id = match &self.selected_template {
            Some(SelectedTemplate::Builtin { id }) => Some(id.clone()),
            Some(SelectedTemplate::Custom { id, .. }) => id.clone(),
            None => None,
        };
        Ok(TemplateCatalog::new(self.custom_templates.clone(), selected_id)?)
    }
    fn resolve(catalog: &TemplateCatalog, reference: TemplateRef<'_>) -> Result<usize, SettingsError> {
        match reference {
            TemplateRef::Index(index) => {
                if index < catalog.templates.len() { Ok(index) } else { Err(SettingsError::TemplateNotFound(format!("index {index}"))) }
            }
            TemplateRef::Id(id) => {
                let mut matches = catalog.templates.iter().enumerate().filter(|(_, t)| t.unique_id.as_deref() == Some(id));
                let Some((index, _)) = matches.next() else { return Err(SettingsError::TemplateNotFound(format!("ID {id:?}"))); };
                if matches.next().is_some() { Err(SettingsError::AmbiguousTemplateId(id.into())) } else { Ok(index) }
            }
        }
    }
    fn builtin_count(&self, catalog: &TemplateCatalog) -> usize { catalog.templates.len() - self.custom_templates.len() }
    pub fn template(&self, reference: TemplateRef<'_>) -> Result<Template, SettingsError> {
        let catalog = self.catalog()?;
        Ok(catalog.templates[Self::resolve(&catalog, reference)?].clone())
    }
    pub fn selected_template(&self) -> Result<Template, SettingsError> {
        let catalog = self.catalog()?;
        let builtin_count = self.builtin_count(&catalog);
        let index = match &self.selected_template {
            None => 0,
            Some(SelectedTemplate::Builtin { id }) => catalog.templates[..builtin_count].iter().position(|t| t.unique_id.as_deref() == Some(id.as_str()))
                .ok_or_else(|| SettingsError::Corrupt(format!("selected built-in ID {id:?} is missing")))?,
            Some(SelectedTemplate::Custom { index, id }) => {
                let template = self.custom_templates.get(*index).ok_or_else(|| SettingsError::Corrupt("selected custom index is missing".into()))?;
                if &template.unique_id != id { return Err(SettingsError::Corrupt("selected custom index and ID disagree".into())); }
                builtin_count + *index
            }
        };
        catalog.templates.get(index).cloned().ok_or_else(|| SettingsError::Corrupt("no default template exists".into()))
    }
    /// Resolve the null/default selection to the actual built-in identity.
    pub fn effective_selection(&self) -> Result<SelectedTemplate, SettingsError> {
        let selected = self.selected_template()?;
        match &self.selected_template {
            Some(selection) => Ok(selection.clone()),
            None => {
                if !selected.is_builtin { return Err(SettingsError::Corrupt("default template is not built-in".into())); }
                let id = selected.unique_id.ok_or_else(|| SettingsError::Corrupt("default template has no ID".into()))?;
                Ok(SelectedTemplate::Builtin { id })
            }
        }
    }
    pub fn select_template(&mut self, reference: TemplateRef<'_>) -> Result<usize, SettingsError> {
        let catalog = self.catalog()?;
        let index = Self::resolve(&catalog, reference)?;
        let builtin_count = self.builtin_count(&catalog);
        self.selected_template = Some(if index < builtin_count {
            let id = catalog.templates[index].unique_id.clone().ok_or_else(|| SettingsError::Corrupt("built-in template has no ID".into()))?;
            SelectedTemplate::Builtin { id }
        } else {
            SelectedTemplate::Custom { index: index - builtin_count, id: catalog.templates[index].unique_id.clone() }
        });
        Ok(index)
    }
    /// GUI duplicate uses the native copy operation, including its name and
    /// limiter-release semantics; the original and its ID remain unchanged.
    pub fn clone_template(&mut self, reference: TemplateRef<'_>) -> Result<usize, SettingsError> {
        let catalog = self.catalog()?;
        let index = Self::resolve(&catalog, reference)?;
        let id = unique_template_uuid(&catalog)?;
        let duplicate = catalog.templates[index].duplicate(id)?;
        let new_index = catalog.templates.len();
        self.custom_templates.push(duplicate);
        Ok(new_index)
    }
    pub fn rename_template(&mut self, reference: TemplateRef<'_>, name: &str) -> Result<(), SettingsError> {
        self.edit_template(reference, "name", Value::String(name.to_owned()))
    }
    /// Edit effective fields, not XML literals or raw float-bit strings. Field
    /// names and value types are documented in research/CLI_CONTRACT.md.
    pub fn edit_template(&mut self, reference: TemplateRef<'_>, field: &str, value: Value) -> Result<(), SettingsError> {
        let catalog = self.catalog()?;
        let index = Self::resolve(&catalog, reference)?;
        let builtin_count = self.builtin_count(&catalog);
        if index < builtin_count { return Err(SettingsError::BuiltinImmutable(index)); }
        let custom_index = index - builtin_count;
        let mut candidate = self.custom_templates[custom_index].clone();
        edit_field(&mut candidate, field, &value)?;
        self.custom_templates[custom_index] = candidate;
        Ok(())
    }
    pub fn remove_template(&mut self, reference: TemplateRef<'_>) -> Result<Template, SettingsError> {
        let catalog = self.catalog()?;
        let index = Self::resolve(&catalog, reference)?;
        let builtin_count = self.builtin_count(&catalog);
        if index < builtin_count { return Err(SettingsError::BuiltinImmutable(index)); }
        let custom_index = index - builtin_count;
        let removed = self.custom_templates.remove(custom_index);
        if let Some(SelectedTemplate::Custom { index: selected, .. }) = &mut self.selected_template {
            if *selected == custom_index { self.selected_template = None; }
            else if *selected > custom_index { *selected -= 1; }
        }
        Ok(removed)
    }

    /// Import preserves original IDs, including duplicates and absent IDs, and
    /// selects the last appended object by index. Reports are returned before
    /// callers decide whether and how to persist the changed settings.
    pub fn import_templates(&mut self, xml: &str) -> Result<ImportReport, SettingsError> {
        let parsed = templates::parse_xml_report(xml)?;
        let mut catalog = self.catalog()?;
        let first = catalog.templates.len();
        let last = catalog.import_xml(xml)?;
        let roundtrip = templates::write_xml_report(&catalog.templates[first..])?;
        let indices: Vec<usize> = (first..catalog.templates.len()).collect();
        self.custom_templates.extend(catalog.templates.into_iter().skip(first));
        if let Some(last) = last { self.select_template(TemplateRef::Index(last))?; }
        Ok(ImportReport { indices, last_index: last, ignored_elements: parsed.ignored_elements,
            ignored_attributes: parsed.ignored_attributes, reserialization_losses: roundtrip.losses })
    }
    pub fn import_file(&mut self, path: &Path) -> Result<ImportReport, SettingsError> {
        let xml = fs::read_to_string(path).map_err(|e| io_error("read template XML", path, e))?;
        self.import_templates(&xml)
    }
    /// Native GUI export copies to a new identity before XML serialization.
    /// Both the copy's changes and subsequent XML round-trip loss are reported.
    pub fn export_template(&self, reference: TemplateRef<'_>) -> Result<ExportReport, SettingsError> {
        let catalog = self.catalog()?;
        let index = Self::resolve(&catalog, reference)?;
        let id = unique_template_uuid(&catalog)?;
        let source = &catalog.templates[index];
        let copy = source.clone_for_export(id.clone())?;
        let mut copy_losses = Vec::new();
        collect_differences("", &serde_json::to_value(source)?, &serde_json::to_value(&copy)?, &mut copy_losses);
        let written = templates::write_xml_report(&[copy])?;
        Ok(ExportReport { export_id: id, xml: written.xml, copy_losses, serialization_losses: written.losses })
    }
    /// Atomically publish a new XML file without replacing an existing path.
    pub fn export_file(&self, reference: TemplateRef<'_>, path: &Path) -> Result<ExportReport, SettingsError> {
        let report = self.export_template(reference)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let stage = write_stage(parent, "template", report.xml.as_bytes())?;
        if let Err(e) = fs::hard_link(&stage, path) {
            let error = io_error("publish template without replacement", path, e);
            return Err(cleanup_failed_stage(&stage, error));
        }
        if let Err(source) = fs::remove_file(&stage) {
            return Err(SettingsError::PublishedCleanup { destination: path.to_owned(), temporary: stage, source });
        }
        sync_directory(parent).map_err(|source| SettingsError::AfterPublish { operation: "sync export directory", path: path.to_owned(), source })?;
        Ok(report)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportReport {
    /// Manager indices; ignored-field indices remain relative to the XML input.
    pub indices: Vec<usize>,
    pub last_index: Option<usize>,
    #[serde(serialize_with = "serialize_ignored_elements")]
    pub ignored_elements: Vec<IgnoredElement>,
    #[serde(serialize_with = "serialize_ignored_attributes")]
    pub ignored_attributes: Vec<IgnoredAttribute>,
    /// Effective differences if the imported objects were written and reimported as native XML.
    pub reserialization_losses: Vec<SerializationLoss>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CopyLoss { pub field: String, pub before: Value, pub after: Value }
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportReport {
    pub export_id: String,
    pub xml: String,
    pub copy_losses: Vec<CopyLoss>,
    pub serialization_losses: Vec<SerializationLoss>,
}

fn serialize_ignored_elements<S: serde::Serializer>(values: &[IgnoredElement], serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    #[derive(Serialize)]
    struct View<'a> { template_index: Option<usize>, name: &'a str }
    let mut seq = serializer.serialize_seq(Some(values.len()))?;
    for value in values {
        seq.serialize_element(&View { template_index: value.template_index, name: &value.name })?;
    }
    seq.end()
}
fn serialize_ignored_attributes<S: serde::Serializer>(values: &[IgnoredAttribute], serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    #[derive(Serialize)]
    struct View<'a> { template_index: Option<usize>, element: &'a str, name: &'a str }
    let mut seq = serializer.serialize_seq(Some(values.len()))?;
    for value in values {
        seq.serialize_element(&View { template_index: value.template_index, element: &value.element, name: &value.name })?;
    }
    seq.end()
}

fn collect_differences(prefix: &str, before: &Value, after: &Value, out: &mut Vec<CopyLoss>) {
    match (before, after) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                if let Some(other) = b.get(key) {
                    let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                    collect_differences(&path, value, other, out);
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => for (i, (left, right)) in a.iter().zip(b).enumerate() {
            collect_differences(&format!("{prefix}[{i}]"), left, right, out);
        },
        _ if before != after => out.push(CopyLoss { field: prefix.into(), before: before.clone(), after: after.clone() }),
        _ => {}
    }
}

fn float_value(field: &str, value: &Value) -> Result<f32, SettingsError> {
    let n = value.as_f64().ok_or_else(|| invalid(field, "expected a finite JSON number"))?;
    let f = n as f32;
    if !n.is_finite() || !f.is_finite() { return Err(invalid(field, "number is outside the finite f32 range")); }
    Ok(f)
}
fn bool_value(field: &str, value: &Value) -> Result<bool, SettingsError> {
    value.as_bool().ok_or_else(|| invalid(field, "expected a boolean"))
}
fn i64_value(field: &str, value: &Value) -> Result<i64, SettingsError> {
    value.as_i64().ok_or_else(|| invalid(field, "expected a signed 64-bit integer"))
}
fn i32_value(field: &str, value: &Value) -> Result<i32, SettingsError> {
    i32::try_from(i64_value(field, value)?).map_err(|_| invalid(field, "expected a signed 32-bit integer"))
}
fn optional_text(field: &str, value: &Value) -> Result<Option<String>, SettingsError> {
    match value { Value::Null => Ok(None), Value::String(s) => Ok(Some(s.clone())), _ => Err(invalid(field, "expected a string or null")) }
}
fn indexed<'a>(field: &'a str, prefix: &str, suffix: bool) -> Option<(usize, &'a str)> {
    let rest = field.strip_prefix(prefix)?.strip_prefix('[')?;
    let (index, rest) = rest.split_once(']')?;
    let index = index.parse::<usize>().ok()?;
    if suffix { Some((index, rest.strip_prefix('.')?)) } else if rest.is_empty() { Some((index, "")) } else { None }
}
fn edit_band(band: &mut Band, field: &str, full_field: &str, value: &Value) -> Result<(), SettingsError> {
    let n = float_value(full_field, value)?;
    match field {
        "threshold" => band.threshold = n, "gain" => band.gain = n, "ratio" => band.ratio = n,
        "attack" => band.attack = n, "release" => band.release = n,
        _ => return Err(invalid(full_field, "unknown band field")),
    }
    Ok(())
}
fn edit_field(t: &mut Template, field: &str, value: &Value) -> Result<(), SettingsError> {
    match field {
        "name" => t.name = optional_text(field, value)?,
        "description" => t.description = optional_text(field, value)?,
        "is_default" => t.is_default = bool_value(field, value)?,
        "analysis_volume" => t.analysis_volume = float_value(field, value)?,
        "clip.enabled" => t.clip.enabled = bool_value(field, value)?,
        "clip.low" => t.clip.set_low(float_value(field, value)?),
        "clip.high" => t.clip.set_high(float_value(field, value)?),
        "clip.percentage" => t.clip.percentage = float_value(field, value)?,
        "clip.headroom" => t.clip.headroom = float_value(field, value)?,
        "pitch.enabled" => t.pitch.enabled = bool_value(field, value)?,
        "pitch.threshold_cents" => t.pitch.threshold_cents = i64_value(field, value)?,
        "pitch.option" => t.pitch.option = i32_value(field, value)?,
        "exciter_option" => t.exciter_option = i32_value(field, value)?,
        "expansion.enabled" => t.expansion.enabled = bool_value(field, value)?,
        "expansion.low" => t.expansion.set_low(float_value(field, value)?),
        "expansion.high" => t.expansion.set_high(float_value(field, value)?),
        "expansion.target_volume" => t.expansion.target_volume = float_value(field, value)?,
        "expansion.headroom" => t.expansion.headroom = float_value(field, value)?,
        "compression.enabled" => t.compression.enabled = bool_value(field, value)?,
        // Unlike clip and expansion, the native compression fields are independent.
        "compression.low" => t.compression.low = float_value(field, value)?,
        "compression.high" => t.compression.high = float_value(field, value)?,
        "compression.target_volume" => t.compression.target_volume = float_value(field, value)?,
        "output.limiter_threshold" => t.output.limiter_threshold = float_value(field, value)?,
        "output.margin" => t.output.margin = float_value(field, value)?,
        "output.character" => t.output.character = i64_value(field, value)?,
        "output.limiter_release" => t.output.limiter_release = float_value(field, value)?,
        _ => {
            if let Some((index, name)) = indexed(field, "expansion.bands", true) {
                let band = t.expansion.bands.get_mut(index).ok_or_else(|| invalid(field, "band index must be 0..3"))?;
                return edit_band(band, name, field, value);
            }
            if let Some((index, name)) = indexed(field, "compression.bands", true) {
                let band = t.compression.bands.get_mut(index).ok_or_else(|| invalid(field, "band index must be 0..3"))?;
                return edit_band(band, name, field, value);
            }
            if let Some((index, _)) = indexed(field, "expansion.crossovers_hz", false) {
                let n = float_value(field, value)?;
                match index { 0 => t.expansion.set_midpoint12(n), 1 => t.expansion.set_midpoint23(n), 2 => t.expansion.set_midpoint34(n), _ => return Err(invalid(field, "crossover index must be 0..2")) }
            } else if let Some((index, _)) = indexed(field, "compression.crossovers_hz", false) {
                let n = float_value(field, value)?;
                match index { 0 => t.compression.set_midpoint12(n), 1 => t.compression.set_midpoint23(n), 2 => t.compression.set_midpoint34(n), _ => return Err(invalid(field, "crossover index must be 0..2")) }
            } else { return Err(invalid(field, "unknown or immutable template field")); }
        }
    }
    Ok(())
}

// CFUUID is a public macOS primitive. Keep the returned aggregate layout equal
// to the sixteen individual uint8_t members in CFUUIDBytes.
#[repr(C)]
struct CFUUIDBytes {
    b0: u8, b1: u8, b2: u8, b3: u8, b4: u8, b5: u8, b6: u8, b7: u8,
    b8: u8, b9: u8, b10: u8, b11: u8, b12: u8, b13: u8, b14: u8, b15: u8,
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFUUIDCreate(allocator: *const c_void) -> *const c_void;
    fn CFUUIDGetUUIDBytes(uuid: *const c_void) -> CFUUIDBytes;
    fn CFRelease(value: *const c_void);
}
fn uuid() -> Result<String, SettingsError> {
    // SAFETY: a null allocator requests the system default; a successful create
    // returns a live CFUUID. Its bytes are copied before the balanced release.
    let object = unsafe { CFUUIDCreate(std::ptr::null()) };
    if object.is_null() { return Err(SettingsError::UuidUnavailable); }
    let b = unsafe { CFUUIDGetUUIDBytes(object) };
    unsafe { CFRelease(object) };
    Ok(format!("{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b.b0, b.b1, b.b2, b.b3, b.b4, b.b5, b.b6, b.b7, b.b8, b.b9, b.b10, b.b11, b.b12, b.b13, b.b14, b.b15))
}
fn unique_template_uuid(catalog: &TemplateCatalog) -> Result<String, SettingsError> {
    for _ in 0..16 {
        let id = uuid()?;
        if catalog.templates.iter().all(|t| t.unique_id.as_deref() != Some(id.as_str())) { return Ok(id); }
    }
    Err(SettingsError::UuidUnavailable)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile { version: u32, revision: u64, settings: Settings }

/// A loaded snapshot and its exact on-disk comparison token. Do not construct a
/// second store for the same path with a different snapshot and expect merging.
pub struct LoadedSettings {
    pub settings: Settings,
    expected: Option<Vec<u8>>,
    revision: u64,
    store_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SettingsStore { directory: PathBuf, home: PathBuf, available_cores: usize }
impl SettingsStore {
    /// `directory` is the complete state directory, which may be caller-selected
    /// for isolation. Neither this path nor defaults touch Platinum Notes state.
    pub fn new(directory: impl Into<PathBuf>, home: impl Into<PathBuf>, available_cores: usize) -> Self {
        Self { directory: directory.into(), home: home.into(), available_cores: cores(available_cores) }
    }
    pub fn in_home(home: impl Into<PathBuf>, available_cores: usize) -> Self {
        let home = home.into();
        Self::new(home.join("Library/Application Support/Platinum Engine"), home, available_cores)
    }
    pub fn directory(&self) -> &Path { &self.directory }
    pub fn settings_path(&self) -> PathBuf { self.directory.join(SETTINGS_FILE) }
    pub fn available_cores(&self) -> usize { self.available_cores }
    pub fn home(&self) -> &Path { &self.home }

    pub fn load(&self) -> Result<LoadedSettings, SettingsError> {
        self.create_directory()?;
        let _lock = StoreLock::acquire(&self.directory.join(LOCK_FILE))?;
        let path = self.settings_path();
        let bytes = read_state(&path)?;
        let (mut settings, revision) = match &bytes {
            None => (Settings::defaults(&self.home, self.available_cores), 0),
            Some(bytes) => {
                let state: StateFile = serde_json::from_slice(bytes).map_err(|e| SettingsError::Corrupt(format!("{}: {e}", path.display())))?;
                if state.version != STATE_VERSION { return Err(SettingsError::Corrupt(format!("unsupported version {}", state.version))); }
                if state.revision == 0 { return Err(SettingsError::Corrupt("zero revision".into())); }
                (state.settings, state.revision)
            }
        };
        settings.processor_max = settings.processor_max.clamp(1, self.available_cores);
        settings.validate().map_err(|e| SettingsError::Corrupt(e.to_string()))?;
        Ok(LoadedSettings { settings, expected: bytes, revision, store_path: path })
    }
    pub fn save(&self, loaded: &mut LoadedSettings) -> Result<(), SettingsError> {
        let path = self.settings_path();
        if loaded.store_path != path { return Err(SettingsError::WrongStore); }
        let mut settings = loaded.settings.clone();
        settings.processor_max = settings.processor_max.clamp(1, self.available_cores);
        settings.validate()?;
        self.create_directory()?;
        let _lock = StoreLock::acquire(&self.directory.join(LOCK_FILE))?;
        if read_state(&path)? != loaded.expected { return Err(SettingsError::ConcurrentModification(path)); }
        let revision = loaded.revision.checked_add(1).ok_or_else(|| SettingsError::Corrupt("revision counter exhausted".into()))?;
        let mut bytes = serde_json::to_vec_pretty(&StateFile { version: STATE_VERSION, revision, settings: settings.clone() })?;
        bytes.push(b'\n');
        let stage = write_stage(&self.directory, "settings", &bytes)?;
        if let Err(e) = fs::rename(&stage, &path) {
            return Err(cleanup_failed_stage(&stage, io_error("publish settings", &path, e)));
        }
        // The rename succeeded even if directory sync fails: keep the token in
        // agreement and report the publication explicitly to the caller.
        loaded.expected = Some(bytes);
        loaded.revision = revision;
        loaded.settings = settings;
        sync_directory(&self.directory).map_err(|source| SettingsError::AfterPublish { operation: "sync settings directory", path, source })?;
        Ok(())
    }
    fn create_directory(&self) -> Result<(), SettingsError> {
        if self.directory.as_os_str().is_empty() { return Err(invalid("state directory", "path cannot be empty")); }
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(&self.directory).map_err(|e| io_error("create settings directory", &self.directory, e))
    }
}

fn read_state(path: &Path) -> Result<Option<Vec<u8>>, SettingsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => fs::read(path).map(Some).map_err(|e| io_error("read settings", path, e)),
        Ok(_) => Err(SettingsError::Corrupt(format!("{} is not a regular file", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("inspect settings", path, e)),
    }
}

#[link(name = "System")]
unsafe extern "C" { fn flock(fd: c_int, operation: c_int) -> c_int; }
struct StoreLock { _file: File }
impl StoreLock {
    fn acquire(path: &Path) -> Result<Self, SettingsError> {
        let file = OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(path)
            .map_err(|e| io_error("open settings lock", path, e))?;
        loop {
            // SAFETY: fd remains owned by `file` for the whole locked scope;
            // closing it releases the advisory lock, including on early return.
            if unsafe { flock(file.as_raw_fd(), 2) } == 0 { return Ok(Self { _file: file }); }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted { return Err(io_error("lock settings", path, error)); }
        }
    }
}
fn cleanup_failed_stage(path: &Path, cause: SettingsError) -> SettingsError {
    match fs::remove_file(path) {
        Ok(()) => cause,
        Err(source) => SettingsError::Cleanup { operation: "remove temporary file", path: path.to_owned(), cause: Box::new(cause), source },
    }
}
pub(crate) fn write_stage(directory: &Path, kind: &str, bytes: &[u8]) -> Result<PathBuf, SettingsError> {
    for _ in 0..16 {
        let path = directory.join(format!(".platinum-{kind}-{}.tmp", uuid()?));
        let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path);
        let mut file = match file {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error("create temporary file", &path, e)),
        };
        if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            return Err(cleanup_failed_stage(&path, io_error("write temporary file", &path, e)));
        }
        drop(file);
        return Ok(path);
    }
    Err(SettingsError::UuidUnavailable)
}
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> { File::open(path)?.sync_all() }
