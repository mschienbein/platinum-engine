//! Persistent Platinum Engine preferences and template management.
//!
//! This state belongs to Platinum Engine, not the original application's
//! preferences. Templates are stored as their effective, bit-exact JSON state;
//! native XML is used only for explicit import/export. A loaded snapshot is an
//! optimistic transaction: cooperating CLI writers lock the same permanent lock
//! file, compare the original bytes, and atomically publish a new revision.

use crate::file_workflow::{AudioFormat, OutputMode, OutputSettings};
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::foundation::uuid;
use crate::templates::{self, Band, IgnoredAttribute, IgnoredElement, SerializationLoss, Template, TemplateCatalog, TemplateError};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::error::Error;
#[cfg(all(target_os = "macos", not(feature = "portable")))]
use std::ffi::c_void;
use std::ffi::{CStr, CString, c_int};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
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

/// Opt-ins for paths that are not proven byte-identical to the macOS arm64
/// reference. Everything under `parity.*` defaults to off; the default
/// behaviour of every build is exactly the identical-or-refuse behaviour.
/// Settings that enable formats the original application does not accept live
/// in a separate namespace, never here.
///
/// `allow_non_identical` enables, on Linux and `portable` builds only, the
/// open-source MP3 decoder and the byte-for-byte copy of embedded cover art.
/// macOS default builds always use Apple's decoder and image pipeline, so the
/// setting is stored but has no effect there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParitySettings {
    pub allow_non_identical: bool,
}
impl ParitySettings {
    fn is_default(&self) -> bool { *self == Self::default() }
}

/// Whether this build can take a non-identical path at all.
pub const NON_IDENTICAL_PATHS_AVAILABLE: bool = cfg!(any(target_os = "linux", feature = "portable"));

/// Read-only statement of what `parity.allow_non_identical` does in this build,
/// shown by `config show` so that a stored but inert opt-in is never mistaken
/// for an active one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ParityEffect {
    pub effective: bool,
    pub note: &'static str,
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
    /// Omitted from the state file while default so a settings file that never
    /// used the opt-in stays readable by earlier releases.
    #[serde(default, skip_serializing_if = "ParitySettings::is_default")]
    pub parity: ParitySettings,
    pub selected_template: Option<SelectedTemplate>,
    pub custom_templates: Vec<Template>,
}

#[derive(Serialize)]
struct ConfigView<'a> {
    output: &'a OutputSettings,
    processor_max: usize,
    mp3_mode: Mp3Mode,
    libraries: &'a LibraryPaths,
    parity: ParitySettings,
    parity_effect: ParityEffect,
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
            parity: ParitySettings::default(),
            selected_template: None,
            custom_templates: Vec::new(),
        }
    }

    /// Whether non-identical paths (open-source MP3 decoding, unchanged
    /// cover-art copy) are active: the opt-in is set and this build has them.
    pub fn non_identical_enabled(&self) -> bool {
        NON_IDENTICAL_PATHS_AVAILABLE && self.parity.allow_non_identical
    }
    pub fn parity_effect(&self) -> ParityEffect {
        match (NON_IDENTICAL_PATHS_AVAILABLE, self.parity.allow_non_identical) {
            (false, _) => ParityEffect { effective: false,
                note: "no effect in this macOS default build: Apple's decoder and image pipeline are always used, so this setting is stored but ignored" },
            (true, false) => ParityEffect { effective: false,
                note: "disabled: identical-or-refuse behaviour; MP3 input is refused and artwork that would need Apple image conversion is refused, each with a typed error code" },
            (true, true) => ParityEffect { effective: true,
                note: "enabled: MP3 input is decoded with the open-source decoder, embedded cover art is copied unchanged and the tag chunks of a WAV source are kept; items using these paths list them in report.non_identical_paths and are not guaranteed byte-identical to macOS" },
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
            libraries: &self.libraries, parity: self.parity, parity_effect: self.parity_effect(),
            selected_template: &self.selected_template,
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
            "parity" => serde_json::to_value(self.parity)?,
            "parity.allow_non_identical" => serde_json::to_value(self.parity.allow_non_identical)?,
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
            "parity" => next.parity = decode(field, value)?,
            "parity.allow_non_identical" => next.parity.allow_non_identical = decode(field, value)?,
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
        self.export_file_impl(reference, path, #[cfg(test)] None)
    }
    fn export_file_impl(&self, reference: TemplateRef<'_>, path: &Path,
        #[cfg(test)] after_stage: Option<Box<dyn FnOnce(&Path)>>) -> Result<ExportReport, SettingsError> {
        let report = self.export_template(reference)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let name = path.file_name().ok_or_else(|| io_error("publish template", path,
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no filename")))?;
        let destination = parent.join(name);
        let work = crate::file_workflow::Workspace::create(parent, "template")
            .map_err(|e| io_error("create private template workspace", path, io::Error::other(e)))?;
        let publish = (|| {
            let mut file = work.create_stage()
                .map_err(|e| io_error("create private template stage", path, io::Error::other(e)))?;
            file.write_all(report.xml.as_bytes()).and_then(|_| file.sync_all())
                .map_err(|e| io_error("write private template stage", path, e))?;
            #[cfg(test)] if let Some(hook) = after_stage { hook(work.directory_path()); }
            work.link_stage(&destination)
                .map_err(|e| io_error("publish template without replacement", path, io::Error::other(e)))
        })();
        if let Err(cause) = publish {
            let cleanup = work.cleanup(false);
            return if cleanup.is_empty() { Err(cause) } else {
                Err(SettingsError::Cleanup { operation: "remove private template workspace",
                    path: cleanup[0].path.clone(), cause: Box::new(cause), source: io::Error::other(format!("{cleanup:?}")) })
            };
        }
        let actual = work.actual_destination(&destination).unwrap_or_else(|| path.to_owned());
        let post = work.validate_parent(&destination).map_err(io::Error::other).and_then(|_| work.sync_parent());
        let cleanup = work.cleanup(false);
        if let Err(source) = post {
            let source = if cleanup.is_empty() { source } else { io::Error::other(format!("{source}; cleanup: {cleanup:?}")) };
            return Err(SettingsError::AfterPublish { operation: "validate and sync export directory", path: actual, source });
        }
        if !cleanup.is_empty() {
            return Err(SettingsError::PublishedCleanup { destination: actual,
                temporary: cleanup[0].path.clone(), source: io::Error::other(format!("{cleanup:?}")) });
        }
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
#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[repr(C)]
struct CFUUIDBytes {
    b0: u8, b1: u8, b2: u8, b3: u8, b4: u8, b5: u8, b6: u8, b7: u8,
    b8: u8, b9: u8, b10: u8, b11: u8, b12: u8, b13: u8, b14: u8, b15: u8,
}
#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFUUIDCreate(allocator: *const c_void) -> *const c_void;
    fn CFUUIDGetUUIDBytes(uuid: *const c_void) -> CFUUIDBytes;
    fn CFRelease(value: *const c_void);
}
#[cfg(all(target_os = "macos", not(feature = "portable")))]
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


#[cfg(target_os = "linux")]
pub(crate) fn xdg_directory(variable: &str, fallback: PathBuf) -> PathBuf {
    std::env::var_os(variable).map(PathBuf::from).filter(|path| path.is_absolute()).unwrap_or(fallback)
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
    expected_stamp: Option<StateStamp>,
    directory_identity: (u64, u64),
    lock_identity: (u64, u64),
    _directory_pin: File,
    _lock_pin: File,
    _state_pin: Option<File>,
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
        #[cfg(target_os = "macos")]
        let directory = home.join("Library/Application Support/Platinum Engine");
        #[cfg(target_os = "linux")]
        let directory = xdg_directory("XDG_CONFIG_HOME", home.join(".config")).join("platinum-engine");
        Self::new(directory, home, available_cores)
    }
    pub fn directory(&self) -> &Path { &self.directory }
    pub fn settings_path(&self) -> PathBuf { self.directory.join(SETTINGS_FILE) }
    pub fn available_cores(&self) -> usize { self.available_cores }
    pub fn home(&self) -> &Path { &self.home }

    pub fn load(&self) -> Result<LoadedSettings, SettingsError> {
        let directory = SecureSettingsDirectory::open(&self.directory)?;
        let lock = StoreLock::acquire(&directory)?;
        let path = self.settings_path();
        let (bytes, expected_stamp, state_pin) = read_state(&directory, &lock)?;
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
        let directory_pin = directory.file.try_clone().map_err(|e| io_error("pin settings directory", &self.directory, e))?;
        // A distinct open description pins the lock inode without retaining its flock.
        let lock_pin = directory.open_entry(c"settings.lock", libc::O_RDONLY, 0)
            .map_err(|e| io_error("pin settings lock", &self.directory.join(LOCK_FILE), e))?;
        let lock_meta = secure_file_metadata(&lock_pin, &self.directory.join(LOCK_FILE), false)?;
        if (lock_meta.dev(), lock_meta.ino()) != lock.identity { return Err(SettingsError::ConcurrentModification(self.directory.join(LOCK_FILE))); }
        Ok(LoadedSettings { settings, expected: bytes, expected_stamp, directory_identity: directory.identity,
            lock_identity: lock.identity, _directory_pin: directory_pin, _lock_pin: lock_pin, _state_pin: state_pin, revision, store_path: path })
    }
    pub fn save(&self, loaded: &mut LoadedSettings) -> Result<(), SettingsError> {
        let path = self.settings_path();
        if loaded.store_path != path { return Err(SettingsError::WrongStore); }
        let mut settings = loaded.settings.clone();
        settings.processor_max = settings.processor_max.clamp(1, self.available_cores);
        settings.validate()?;
        let directory = SecureSettingsDirectory::open(&self.directory)?;
        if directory.identity != loaded.directory_identity { return Err(SettingsError::ConcurrentModification(self.directory.clone())); }
        let lock = StoreLock::acquire(&directory)?;
        if lock.identity != loaded.lock_identity { return Err(SettingsError::ConcurrentModification(self.directory.join(LOCK_FILE))); }
        let (current, current_stamp, _current_pin) = read_state(&directory, &lock)?;
        if current != loaded.expected || current_stamp != loaded.expected_stamp { return Err(SettingsError::ConcurrentModification(path)); }
        let revision = loaded.revision.checked_add(1).ok_or_else(|| SettingsError::Corrupt("revision counter exhausted".into()))?;
        let mut bytes = serde_json::to_vec_pretty(&StateFile { version: STATE_VERSION, revision, settings: settings.clone() })?;
        bytes.push(b'\n');
        let stage = directory.write_stage(&bytes)?;
        let new_pin = match stage.file.try_clone() {
            Ok(file) => file,
            Err(e) => return Err(directory.cleanup_stage(&stage, io_error("pin temporary settings", &path, e))),
        };
        let publish = (|| {
            directory.validate()?;
            lock.validate(&directory)?;
            let (_, current_stamp, _current_pin) = read_state(&directory, &lock)?;
            if current_stamp != loaded.expected_stamp { return Err(SettingsError::ConcurrentModification(path.clone())); }
            directory.publish(&stage, current_stamp.is_some())
        })();
        if let Err(error) = publish { return Err(directory.cleanup_stage(&stage, error)); }
        // Rename succeeded: retain the new comparison token even if sync fails.
        loaded.expected = Some(bytes);
        loaded.revision = revision;
        loaded.settings = settings;
        let metadata = stage.file.metadata().map_err(|source| SettingsError::AfterPublish {
            operation: "inspect published settings", path: path.clone(), source,
        })?;
        loaded.expected_stamp = Some(StateStamp::new(&metadata));
        loaded._state_pin = Some(new_pin);
        directory.file.sync_all().map_err(|source| SettingsError::AfterPublish { operation: "sync settings directory", path, source })?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateStamp { dev: u64, ino: u64, len: u64, mtime: i64, mtime_nsec: i64, ctime: i64, ctime_nsec: i64, mode: u32, uid: u32 }
impl StateStamp {
    fn new(meta: &fs::Metadata) -> Self {
        Self { dev: meta.dev(), ino: meta.ino(), len: meta.len(), mtime: meta.mtime(), mtime_nsec: meta.mtime_nsec(),
            ctime: meta.ctime(), ctime_nsec: meta.ctime_nsec(), mode: meta.mode(), uid: meta.uid() }
    }
}

pub(crate) fn validate_owner_mode(path: &Path, meta: &fs::Metadata, uid: u32, directory: bool) -> Result<(), SettingsError> {
    let kind_ok = if directory { meta.file_type().is_dir() } else { meta.file_type().is_file() && meta.nlink() == 1 };
    if !kind_ok || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(SettingsError::Corrupt(format!("{} must be a non-symlink {} owned by the current user and not group/world writable",
            path.display(), if directory { "directory" } else { "singly linked regular file" })));
    }
    Ok(())
}
fn secure_metadata(path: &Path, directory: bool, mut read: impl FnMut() -> io::Result<fs::Metadata>) -> Result<fs::Metadata, SettingsError> {
    let mut meta = read().map_err(|e| io_error("inspect secure file", path, e))?;
    let first = (meta.dev(), meta.ino(), meta.mode(), meta.nlink(), meta.len(),
        meta.mtime(), meta.mtime_nsec(), meta.ctime(), meta.ctime_nsec());
    for attempt in 0..3 {
        match validate_owner_mode(path, &meta, unsafe { libc::geteuid() }, directory) {
            Ok(()) => return Ok(meta),
            // Never retry a bad kind, link count or writable permission.
            Err(error) if attempt == 2 || meta.mode() & 0o022 != 0
                || (directory && !meta.file_type().is_dir())
                || (!directory && (!meta.file_type().is_file() || meta.nlink() != 1)) => return Err(error),
            Err(_) => {
                let next = read().map_err(|e| io_error("reinspect secure file", path, e))?;
                let observed = (next.dev(), next.ino(), next.mode(), next.nlink(), next.len(),
                    next.mtime(), next.mtime_nsec(), next.ctime(), next.ctime_nsec());
                if observed != first { return Err(SettingsError::ConcurrentModification(path.to_owned())); }
                meta = next;
            }
        }
    }
    unreachable!()
}
/// Recheck a held descriptor when a filesystem transiently reports an
/// unmapped owner; a persistently wrong owner is always refused.
pub(crate) fn secure_file_metadata(file: &File, path: &Path, directory: bool) -> Result<fs::Metadata, SettingsError> {
    secure_metadata(path, directory, || file.metadata())
}
pub(crate) fn secure_path_metadata(path: &Path, directory: bool) -> Result<fs::Metadata, SettingsError> {
    secure_metadata(path, directory, || fs::symlink_metadata(path))
}
pub(crate) fn validate_private_acl(file: &File, path: &Path) -> Result<(), SettingsError> {
    crate::file_workflow::require_no_extended_acl(file, path).map_err(|error| SettingsError::Corrupt(error.to_string()))
}

pub(crate) struct SecureSettingsDirectory { file: File, path: PathBuf, identity: (u64, u64) }
struct SettingsStage { file: File, name: CString, identity: (u64, u64) }
impl SecureSettingsDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self, SettingsError> {
        if path.as_os_str().is_empty() { return Err(invalid("state directory", "path cannot be empty")); }
        // A trailing slash or "/." otherwise makes O_NOFOLLOW apply to a
        // synthetic final component and can dereference the directory symlink.
        let normalized = path.components().collect::<PathBuf>();
        let path = normalized.as_path();
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
            .map_err(|e| io_error("create settings directory", path, e))?;
        let file = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path).map_err(|e| io_error("open non-symlink settings directory", path, e))?;
        let meta = secure_file_metadata(&file, path, true)?;
        let dir = Self { file, path: path.to_owned(), identity: (meta.dev(), meta.ino()) };
        dir.validate()?;
        Ok(dir)
    }
    pub(crate) fn validate(&self) -> Result<(), SettingsError> {
        let opened = secure_file_metadata(&self.file, &self.path, true)?;
        validate_private_acl(&self.file, &self.path)?;
        let named = secure_path_metadata(&self.path, true)?;
        if (opened.dev(), opened.ino()) != self.identity || (named.dev(), named.ino()) != self.identity {
            return Err(SettingsError::ConcurrentModification(self.path.clone()));
        }
        Ok(())
    }
    pub(crate) fn alias(&self) -> Result<PathBuf, SettingsError> {
        #[cfg(target_os = "macos")]
        let alias = PathBuf::from(format!("/.vol/{}/{}", self.identity.0, self.identity.1));
        #[cfg(target_os = "linux")]
        let alias = PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()));
        let meta = fs::metadata(&alias).map_err(|e| io_error("resolve pinned state directory", &self.path, e))?;
        if (meta.dev(), meta.ino()) != self.identity {
            return Err(SettingsError::ConcurrentModification(self.path.clone()));
        }
        Ok(alias)
    }
    fn open_entry(&self, name: &CStr, flags: c_int, mode: libc::mode_t) -> Result<File, io::Error> {
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC, mode as libc::c_uint) };
        if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { File::from_raw_fd(fd) }) }
    }
    fn validate_entry(&self, name: &CStr, expected: (u64, u64), file: &File) -> Result<(), SettingsError> {
        let path = self.path.join(name.to_string_lossy().as_ref());
        let meta = secure_file_metadata(file, &path, false)?;
        validate_private_acl(file, &path)?;
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatat(self.file.as_raw_fd(), name.as_ptr(), named.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io_error("inspect non-symlink settings entry", &path, io::Error::last_os_error()));
        }
        let named = unsafe { named.assume_init() };
        if (meta.dev(), meta.ino()) != expected || named.st_mode & libc::S_IFMT != libc::S_IFREG
            || (named.st_dev as u64, named.st_ino as u64) != expected {
            return Err(SettingsError::ConcurrentModification(path));
        }
        Ok(())
    }
    fn write_stage(&self, bytes: &[u8]) -> Result<SettingsStage, SettingsError> {
        for _ in 0..16 {
            let name = CString::new(format!(".platinum-settings-{}.tmp", uuid()?)).unwrap();
            let path = self.path.join(name.to_string_lossy().as_ref());
            let file = match self.open_entry(&name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) {
                Ok(file) => file, Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io_error("create temporary settings", &path, e)),
            };
            let meta = match file.metadata() {
                Ok(meta) => meta,
                Err(error) => return Err(SettingsError::Cleanup { operation: "remove unverified temporary settings",
                    path: path.clone(), cause: Box::new(io_error("inspect temporary settings", &path, error)),
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "stage identity unavailable; retained") }),
            };
            let stage = SettingsStage { file, name, identity: (meta.dev(), meta.ino()) };
            if let Err(error) = ensure_stage_read_write(&stage.file, meta.mode()) {
                return Err(self.cleanup_stage(&stage, io_error("set temporary settings permissions", &path, error)));
            }
            if let Err(e) = (&stage.file).write_all(bytes).and_then(|_| stage.file.sync_all()) {
                return Err(self.cleanup_stage(&stage, io_error("write temporary settings", &path, e)));
            }
            return Ok(stage);
        }
        Err(SettingsError::UuidUnavailable)
    }
    fn cleanup_stage(&self, stage: &SettingsStage, cause: SettingsError) -> SettingsError {
        let path = self.path.join(stage.name.to_string_lossy().as_ref());
        let result = self.validate_entry(&stage.name, stage.identity, &stage.file).map_err(io::Error::other).and_then(|_| {
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), stage.name.as_ptr(), 0) } == 0 { Ok(()) }
            else { Err(io::Error::last_os_error()) }
        });
        match result { Ok(()) => cause, Err(source) => SettingsError::Cleanup {
            operation: "remove temporary settings", path, cause: Box::new(cause), source,
        } }
    }
    fn publish(&self, stage: &SettingsStage, replacing: bool) -> Result<(), SettingsError> {
        self.validate_entry(&stage.name, stage.identity, &stage.file)?;
        let fd = self.file.as_raw_fd();
        let status = if replacing {
            unsafe { libc::renameat(fd, stage.name.as_ptr(), fd, c"settings.json".as_ptr()) }
        } else {
            #[cfg(target_os = "macos")]
            { unsafe { libc::renameatx_np(fd, stage.name.as_ptr(), fd, c"settings.json".as_ptr(), libc::RENAME_EXCL) } }
            #[cfg(target_os = "linux")]
            { unsafe { libc::renameat2(fd, stage.name.as_ptr(), fd, c"settings.json".as_ptr(), libc::RENAME_NOREPLACE) } }
        };
        if status == 0 { return Ok(()); }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::AlreadyExists { Err(SettingsError::ConcurrentModification(self.path.join(SETTINGS_FILE))) }
        else { Err(io_error("publish settings", &self.path.join(SETTINGS_FILE), error)) }
    }
}

fn read_state(directory: &SecureSettingsDirectory, lock: &StoreLock) -> Result<(Option<Vec<u8>>, Option<StateStamp>, Option<File>), SettingsError> {
    directory.validate()?;
    lock.validate(directory)?;
    let path = directory.path.join(SETTINGS_FILE);
    let mut file = match directory.open_entry(c"settings.json", libc::O_RDONLY, 0) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            directory.validate()?;
            lock.validate(directory)?;
            return Ok((None, None, None));
        }
        Err(e) => return Err(io_error("open non-symlink settings", &path, e)),
    };
    let before = secure_file_metadata(&file, &path, false)?;
    validate_private_acl(&file, &path)?;
    let stamp = StateStamp::new(&before);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| io_error("read settings", &path, e))?;
    let after = secure_file_metadata(&file, &path, false)?;
    validate_private_acl(&file, &path)?;
    if StateStamp::new(&after) != stamp { return Err(SettingsError::ConcurrentModification(path)); }
    directory.validate_entry(c"settings.json", (stamp.dev, stamp.ino), &file)?;
    directory.validate()?;
    lock.validate(directory)?;
    Ok((Some(bytes), Some(stamp), Some(file)))
}

#[cfg_attr(target_os = "macos", link(name = "System"))]
unsafe extern "C" { fn flock(fd: c_int, operation: c_int) -> c_int; }
struct StoreLock { file: File, identity: (u64, u64) }
impl StoreLock {
    fn acquire(directory: &SecureSettingsDirectory) -> Result<Self, SettingsError> {
        directory.validate()?;
        let path = directory.path.join(LOCK_FILE);
        let open_lock = || directory.open_entry(c"settings.lock", libc::O_RDWR | libc::O_CREAT, 0o600);
        let file = match open_lock() {
            Ok(file) => file,
            // Darwin can return ENOENT while another writer creates this lock.
            // Revalidate the pinned directory before one bounded retry.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                directory.validate()?;
                open_lock().map_err(|e| io_error("open non-symlink settings lock", &path, e))?
            }
            Err(e) => return Err(io_error("open non-symlink settings lock", &path, e)),
        };
        let meta = secure_file_metadata(&file, &path, false)?;
        validate_private_acl(&file, &path)?;
        let lock = Self { file, identity: (meta.dev(), meta.ino()) };
        loop {
            lock.validate(directory)?;
            if unsafe { flock(lock.file.as_raw_fd(), 2) } == 0 {
                lock.validate(directory)?;
                return Ok(lock);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted { return Err(io_error("lock settings", &path, error)); }
        }
    }
    fn validate(&self, directory: &SecureSettingsDirectory) -> Result<(), SettingsError> {
        directory.validate()?;
        let path = directory.path.join(LOCK_FILE);
        let meta = secure_file_metadata(&self.file, &path, false)?;
        validate_private_acl(&self.file, &path)?;
        if (meta.dev(), meta.ino()) != self.identity { return Err(SettingsError::ConcurrentModification(path)); }
        directory.validate_entry(c"settings.lock", self.identity, &self.file)
    }
}
fn ensure_stage_read_write(file: &File, mode: u32) -> io::Result<()> {
    if mode & 0o777 == 0o600 { return Ok(()); }
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 { return Err(io::Error::last_os_error()); }
    if file.metadata()?.mode() & 0o777 != 0o600 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "could not establish owner read/write permissions"));
    }
    Ok(())
}

fn cleanup_failed_stage(path: &Path, file: &File, cause: SettingsError) -> SettingsError {
    let result = file.metadata().and_then(|held| {
        let named = fs::symlink_metadata(path)?;
        if !held.file_type().is_file() || !named.file_type().is_file()
            || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "temporary file was replaced; retained"));
        }
        fs::remove_file(path)
    });
    match result {
        Ok(()) => cause,
        Err(source) => SettingsError::Cleanup { operation: "remove temporary file", path: path.to_owned(), cause: Box::new(cause), source },
    }
}
pub(crate) fn write_stage(directory: &Path, kind: &str, bytes: &[u8]) -> Result<(PathBuf, File), SettingsError> {
    for _ in 0..16 {
        let path = directory.join(format!(".platinum-{kind}-{}.tmp", uuid()?));
        let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path);
        let mut file = match file {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error("create temporary file", &path, e)),
        };
        let mode = match file.metadata() {
            Ok(meta) => meta.mode(),
            Err(error) => return Err(cleanup_failed_stage(&path, &file, io_error("inspect temporary file", &path, error))),
        };
        if let Err(error) = ensure_stage_read_write(&file, mode) {
            return Err(cleanup_failed_stage(&path, &file, io_error("set temporary file permissions", &path, error)));
        }
        if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            return Err(cleanup_failed_stage(&path, &file, io_error("write temporary file", &path, e)));
        }
        return Ok((path, file));
    }
    Err(SettingsError::UuidUnavailable)
}
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> { File::open(path)?.sync_all() }

#[cfg(test)]
mod security_tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("pn-settings-stage-{}-{}", std::process::id(), uuid().unwrap()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }

    #[test]
    fn template_export_does_not_follow_a_substituted_workspace() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        let shared = fixture.0.join("shared"); let victim = fixture.0.join("victim");
        fs::create_dir(&shared).unwrap(); fs::create_dir(&victim).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o770)).unwrap();
        fs::write(victim.join("output.template"), b"victim").unwrap();
        let moved = shared.join("moved");
        let moved_hook = moved.clone(); let victim_hook = victim.clone();
        let settings = Settings::defaults(&fixture.0, 1);
        let destination = shared.join("export.pntemplate");
        let error = settings.export_file_impl(TemplateRef::Index(0), &destination, Some(Box::new(move |workspace| {
            fs::rename(workspace, &moved_hook).unwrap();
            symlink(&victim_hook, workspace).unwrap();
        }))).unwrap_err();
        assert!(matches!(error, SettingsError::PublishedCleanup { .. }));
        assert_eq!(fs::read(victim.join("output.template")).unwrap(), b"victim");
        assert!(fs::read_to_string(destination).unwrap().contains("<"));
        assert!(!moved.join("output.template").exists());
    }

    #[test]
    fn restrictive_umask_settings_and_statistics_stages_remain_readable() {
        use std::os::unix::process::CommandExt;
        const CHILD: &str = "PN_TEST_SETTINGS_UMASK_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = PathBuf::from(path);
            let directory = SecureSettingsDirectory::open(&path).unwrap();
            let stage = directory.write_stage(b"discard").unwrap();
            assert_eq!(stage.file.metadata().unwrap().mode() & 0o777, 0o600);
            let stage_name = path.join(stage.name.to_string_lossy().as_ref());
            assert!(matches!(directory.cleanup_stage(&stage, SettingsError::Corrupt("injected".into())), SettingsError::Corrupt(_)));
            assert!(!stage_name.exists());
            let store = SettingsStore::new(&path, &path, 1);
            let mut loaded = store.load().unwrap();
            loaded.settings.mp3_mode = Mp3Mode::Vbr192;
            store.save(&mut loaded).unwrap();
            assert_eq!(fs::metadata(store.settings_path()).unwrap().mode() & 0o777, 0o600);
            assert_eq!(store.load().unwrap().settings.mp3_mode, Mp3Mode::Vbr192);
            let statistics = crate::statistics::StatisticsStore::new(&path);
            statistics.reset(&crate::file_workflow::Cancellation::new()).unwrap();
            assert_eq!(fs::metadata(statistics.statistics_path()).unwrap().mode() & 0o777, 0o600);
            assert_eq!(statistics.show(&crate::file_workflow::Cancellation::new()).unwrap(), crate::statistics::Statistics::default());
            return;
        }
        let fixture = Fixture::new();
        for name in ["settings.lock", "statistics.lock"] {
            OpenOptions::new().write(true).create_new(true).mode(0o600).open(fixture.0.join(name)).unwrap();
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.arg("--exact").arg("settings::security_tests::restrictive_umask_settings_and_statistics_stages_remain_readable")
            .arg("--nocapture").env(CHILD, &fixture.0);
        unsafe { child.pre_exec(|| { libc::umask(0o400); Ok(()) }); }
        assert!(child.status().unwrap().success());
    }

    #[test]
    fn owner_validation_rejects_a_different_effective_uid() {
        let path = std::env::temp_dir().join(format!("pn-settings-owner-{}-{}", std::process::id(), uuid().unwrap()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let file_path = path.join("state");
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file_path).unwrap();
        for (entry, is_directory) in [(&path, true), (&file_path, false)] {
            let meta = fs::symlink_metadata(entry).unwrap();
            assert!(validate_owner_mode(entry, &meta, meta.uid(), is_directory).is_ok());
            assert!(validate_owner_mode(entry, &meta, meta.uid().wrapping_add(1), is_directory).is_err());
        }
        fs::remove_file(file_path).unwrap();
        fs::remove_dir(path).unwrap();
    }
}
