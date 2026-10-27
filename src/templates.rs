//! Platinum Notes 10.5.0.1256 template state and native XML interchange.
//!
//! The effective-state serde representation follows `research/templates/contract.json`:
//! binary32 values are `0x` plus eight hexadecimal bits and signed 64-bit values
//! are decimal strings. XML is intentionally lossy; it is not this representation.
//! See `research/CLI_CONTRACT.md` for interchange and verification boundaries.

use quick_xml::events::{BytesCData, BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

/// An explicit XML, unsupported-input, identity, or embedded-data failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    Xml(String),
    Unsupported(String),
    InvalidNumber { field: String, value: String },
    UnsupportedVersion(i32),
    InvalidIdentity(String),
    IndexOutOfBounds(usize),
    Builtins(String),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Xml(message) => write!(f, "template XML: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported template XML: {message}"),
            Self::InvalidNumber { field, value } => write!(f, "unsupported number in {field}: {value:?}"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported template version {version}"),
            Self::InvalidIdentity(message) => write!(f, "invalid new identity: {message}"),
            Self::IndexOutOfBounds(index) => write!(f, "template index {index} is out of bounds"),
            Self::Builtins(message) => write!(f, "embedded builtins: {message}"),
        }
    }
}
impl Error for TemplateError {}

mod float_bits {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{:08x}", value.to_bits()))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
        let value = String::deserialize(deserializer)?;
        parse(&value).map_err(D::Error::custom)
    }
    pub fn parse(value: &str) -> Result<f32, &'static str> {
        if value.len() != 10 || !value.starts_with("0x") || !value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit) {
            return Err("expected 0x followed by eight hexadecimal float32 bits");
        }
        u32::from_str_radix(&value[2..], 16)
            .map(f32::from_bits)
            .map_err(|_| "invalid float32 bits")
    }
}

mod float_array_bits {
    use super::float_bits;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

    pub fn serialize<S: Serializer>(values: &[f32; 3], serializer: S) -> Result<S::Ok, S::Error> {
        let bits = values.map(|v| format!("0x{:08x}", v.to_bits()));
        bits.serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[f32; 3], D::Error> {
        let values = <[String; 3]>::deserialize(deserializer)?;
        Ok([
            float_bits::parse(&values[0]).map_err(D::Error::custom)?,
            float_bits::parse(&values[1]).map_err(D::Error::custom)?,
            float_bits::parse(&values[2]).map_err(D::Error::custom)?,
        ])
    }
}

mod int64_string {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        let value = String::deserialize(deserializer)?;
        let digits = value.strip_prefix('-').unwrap_or(&value);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit())
            || (digits.len() > 1 && digits.starts_with('0'))
        {
            return Err(D::Error::custom("expected signed decimal i64 without leading zeros"));
        }
        value.parse::<i64>().map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Band {
    #[serde(with = "float_bits")]
    pub threshold: f32,
    #[serde(with = "float_bits")]
    pub gain: f32,
    #[serde(with = "float_bits")]
    pub ratio: f32,
    #[serde(with = "float_bits")]
    pub attack: f32,
    #[serde(with = "float_bits")]
    pub release: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clip {
    pub enabled: bool,
    #[serde(with = "float_bits")]
    pub low: f32,
    #[serde(with = "float_bits")]
    pub high: f32,
    #[serde(with = "float_bits")]
    pub percentage: f32,
    #[serde(with = "float_bits")]
    pub headroom: f32,
}
impl Clip {
    pub fn set_low(&mut self, value: f32) { set_low(&mut self.low, &mut self.high, value); }
    pub fn set_high(&mut self, value: f32) { set_high(&mut self.low, &mut self.high, value); }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pitch {
    pub enabled: bool,
    #[serde(with = "int64_string")]
    pub threshold_cents: i64,
    pub option: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expansion {
    pub enabled: bool,
    #[serde(with = "float_bits")]
    pub low: f32,
    #[serde(with = "float_bits")]
    pub high: f32,
    #[serde(with = "float_bits")]
    pub target_volume: f32,
    #[serde(with = "float_bits")]
    pub headroom: f32,
    pub bands: [Band; 4],
    #[serde(with = "float_array_bits")]
    pub crossovers_hz: [f32; 3],
}
impl Expansion {
    pub fn set_low(&mut self, value: f32) { set_low(&mut self.low, &mut self.high, value); }
    pub fn set_high(&mut self, value: f32) { set_high(&mut self.low, &mut self.high, value); }
    pub fn set_midpoint12(&mut self, value: f32) { set_midpoint12(&mut self.crossovers_hz, value); }
    pub fn set_midpoint23(&mut self, value: f32) { set_midpoint23(&mut self.crossovers_hz, value); }
    pub fn set_midpoint34(&mut self, value: f32) { set_midpoint34(&mut self.crossovers_hz, value); }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compression {
    pub enabled: bool,
    #[serde(with = "float_bits")]
    pub low: f32,
    #[serde(with = "float_bits")]
    pub high: f32,
    #[serde(with = "float_bits")]
    pub target_volume: f32,
    pub bands: [Band; 4],
    #[serde(with = "float_array_bits")]
    pub crossovers_hz: [f32; 3],
}
impl Compression {
    pub fn set_midpoint12(&mut self, value: f32) { set_midpoint12(&mut self.crossovers_hz, value); }
    pub fn set_midpoint23(&mut self, value: f32) { set_midpoint23(&mut self.crossovers_hz, value); }
    pub fn set_midpoint34(&mut self, value: f32) { set_midpoint34(&mut self.crossovers_hz, value); }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    #[serde(with = "float_bits")]
    pub limiter_threshold: f32,
    #[serde(with = "float_bits")]
    pub margin: f32,
    #[serde(with = "int64_string")]
    pub character: i64,
    /// Not serialized by native XML and not copied by the native clone operation.
    #[serde(with = "float_bits")]
    pub limiter_release: f32,
}
impl Default for Output {
    fn default() -> Self {
        Self { limiter_threshold: 0.0, margin: 0.0, character: 0, limiter_release: 3.0 }
    }
}

/// The fields are effective internal values, not XML literals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub unique_id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub is_builtin: bool,
    pub is_default: bool,
    #[serde(with = "float_bits")]
    pub analysis_volume: f32,
    pub clip: Clip,
    pub pitch: Pitch,
    pub exciter_option: i32,
    pub expansion: Expansion,
    pub compression: Compression,
    pub output: Output,
}

fn set_low(low: &mut f32, high: &mut f32, value: f32) {
    *low = value;
    if *high < value { *high = value; }
}
fn set_high(low: &mut f32, high: &mut f32, value: f32) {
    *high = value;
    if *low > value { *low = value; }
}
fn set_midpoint12(c: &mut [f32; 3], value: f32) {
    c[0] = value;
    if c[1] < value { set_midpoint23(c, value); }
}
fn set_midpoint23(c: &mut [f32; 3], value: f32) {
    c[1] = value;
    if c[0] > value { set_midpoint12(c, value); }
    if c[2] < c[1] { set_midpoint34(c, c[1]); }
}
fn set_midpoint34(c: &mut [f32; 3], value: f32) {
    c[2] = value;
    if c[1] > value { set_midpoint23(c, value); }
}
fn copy_crossovers(source: [f32; 3]) -> [f32; 3] {
    let mut copy = [0.0; 3];
    set_midpoint12(&mut copy, source[0]);
    set_midpoint23(&mut copy, source[1]);
    set_midpoint34(&mut copy, source[2]);
    copy
}

impl Template {
    pub fn editable(&self) -> bool { !self.is_builtin }

    // copyWithZone creates a UUID, copies through setters and resets default/release.
    fn native_copy(&self, new_id: impl Into<String>) -> Result<Self, TemplateError> {
        let new_id = new_id.into();
        if new_id.is_empty() || self.unique_id.as_deref() == Some(new_id.as_str()) {
            return Err(TemplateError::InvalidIdentity("supply a nonempty identity different from the source".into()));
        }
        let mut copy = self.clone();
        copy.unique_id = Some(new_id);
        copy.is_default = false;
        copy.output.limiter_release = 3.0;
        copy.clip.low = 0.0;
        copy.clip.high = 0.0;
        copy.clip.set_low(self.clip.low);
        copy.clip.set_high(self.clip.high);
        copy.expansion.low = 0.0;
        copy.expansion.high = 0.0;
        copy.expansion.set_low(self.expansion.low);
        copy.expansion.set_high(self.expansion.high);
        copy.expansion.crossovers_hz = copy_crossovers(self.expansion.crossovers_hz);
        copy.compression.crossovers_hz = copy_crossovers(self.compression.crossovers_hz);
        Ok(copy)
    }

    /// GUI duplicate: a fresh caller-provided ID, " copy" name, and a custom template.
    pub fn duplicate(&self, new_id: impl Into<String>) -> Result<Self, TemplateError> {
        let mut copy = self.native_copy(new_id)?;
        copy.name = Some(format!("{} copy", self.name.as_deref().unwrap_or("(null)")));
        copy.is_builtin = false;
        if self.is_builtin { copy.description = None; }
        Ok(copy)
    }

    /// GUI export copies first; unlike duplication it retains the original name and builtin flag.
    pub fn clone_for_export(&self, new_id: impl Into<String>) -> Result<Self, TemplateError> {
        self.native_copy(new_id)
    }

    pub fn export_xml(&self, new_id: impl Into<String>) -> Result<String, TemplateError> {
        write_xml(&[self.clone_for_export(new_id)?])
    }
}

/// Manager order is builtin first, then custom, without ID deduplication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateCatalog {
    pub templates: Vec<Template>,
    /// The persisted key, including an empty string if one was stored.
    pub selected_id: Option<String>,
}
impl TemplateCatalog {
    pub fn new(custom: Vec<Template>, selected_id: Option<String>) -> Result<Self, TemplateError> {
        let mut templates = builtins()?;
        templates.extend(custom);
        Ok(Self { templates, selected_id })
    }

    /// Fallback IDs are supplied by the caller because the native empty-file fallback
    /// creates three new constructor UUIDs, rather than shipping stable identities.
    pub fn from_loaded(
        mut builtin: Vec<Template>, custom: Vec<Template>, selected_id: Option<String>,
        fallback_ids: [String; 3],
    ) -> Result<Self, TemplateError> {
        builtin.extend(custom);
        if builtin.is_empty() {
            if fallback_ids.iter().any(|id| id.is_empty())
                || fallback_ids[0] == fallback_ids[1] || fallback_ids[0] == fallback_ids[2]
                || fallback_ids[1] == fallback_ids[2]
            {
                return Err(TemplateError::InvalidIdentity("fallback IDs must be nonempty and distinct".into()));
            }
            for (name, id) in ["One", "Two", "Three"].into_iter().zip(fallback_ids) {
                builtin.push(Template { name: Some(name.into()), unique_id: Some(id), ..Template::default() });
            }
        }
        Ok(Self { templates: builtin, selected_id })
    }

    pub fn default_template(&self) -> Option<&Template> { self.templates.first() }

    /// Unmatched nonempty IDs are removed, then the first loaded template is selected.
    pub fn selected(&mut self) -> Option<&Template> {
        if let Some(id) = self.selected_id.as_deref().filter(|id| !id.is_empty()) {
            if let Some(index) = self.templates.iter().position(|t| t.unique_id.as_deref() == Some(id)) {
                return self.templates.get(index);
            }
            self.selected_id = None;
        }
        self.default_template()
    }

    pub fn select(&mut self, index: usize) -> Result<(), TemplateError> {
        let template = self.templates.get(index).ok_or(TemplateError::IndexOutOfBounds(index))?;
        self.selected_id = template.unique_id.clone();
        Ok(())
    }

    /// Manager import appends every template, resets both flags, and returns the last index.
    /// As in the native manager, it does not deduplicate; UI selection is a separate step.
    pub fn import_xml(&mut self, xml: &str) -> Result<Option<usize>, TemplateError> {
        let mut imported = parse_xml(xml)?;
        for template in &mut imported {
            template.is_builtin = false;
            template.is_default = false;
        }
        if imported.is_empty() { return Ok(None); }
        self.templates.extend(imported);
        Ok(Some(self.templates.len() - 1))
    }

    /// Persist custom objects directly; IDs are not regenerated for a save.
    pub fn write_custom_xml(&self) -> Result<String, TemplateError> {
        write_template_refs(self.templates.iter().filter(|t| !t.is_builtin))
    }

    /// Remove an object by manager-array position, not by its possibly duplicated ID.
    pub fn remove(&mut self, index: usize) -> Result<Template, TemplateError> {
        if index >= self.templates.len() { return Err(TemplateError::IndexOutOfBounds(index)); }
        Ok(self.templates.remove(index))
    }
}

#[derive(Deserialize)]
struct BuiltinDocument { templates_in_load_order: Vec<BuiltinEntry> }
#[derive(Deserialize)]
struct BuiltinEntry { contract_version: u32, template: Template }

/// Shipped effective states derived from the original bundled XML, embedded with the crate.
pub fn builtins() -> Result<Vec<Template>, TemplateError> {
    let document: BuiltinDocument = serde_json::from_str(include_str!("../research/templates/builtins.json"))
        .map_err(|e| TemplateError::Builtins(e.to_string()))?;
    if document.templates_in_load_order.iter().any(|entry| entry.contract_version != 1) {
        return Err(TemplateError::Builtins("unsupported embedded contract version".into()));
    }
    Ok(document.templates_in_load_order.into_iter().map(|entry| entry.template).collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredElement {
    /// Index in the loaded template list; None denotes a direct child of the root.
    pub template_index: Option<usize>,
    pub name: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredAttribute {
    pub template_index: Option<usize>,
    pub element: String,
    pub name: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTemplates {
    pub templates: Vec<Template>,
    /// Unknown elements ignored by the native parser, including legacy exciter tags.
    pub ignored_elements: Vec<IgnoredElement>,
    /// All XML attributes not read by the native model.
    pub ignored_attributes: Vec<IgnoredAttribute>,
}

#[derive(Default)]
struct Node {
    name: String,
    attributes: Vec<(String, String)>,
    text: String,
    children: Vec<Node>,
}
impl Node {
    fn child(&self, name: &str) -> Option<&Node> { self.children.iter().rev().find(|node| node.name == name) }
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
    fn value(&self, name: &str) -> Option<&str> { self.child(name).map(|child| child.text.as_str()) }
    fn boolean(&self, name: &str) -> bool { self.value(name) == Some("true") }
    fn float(&self, name: &str) -> Result<f32, TemplateError> { parse_float(name, self.value(name)) }
    fn int(&self, name: &str) -> Result<i32, TemplateError> { parse_int(name, self.value(name)) }
}

fn xml_char(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}
fn xml_name_start(c: char) -> bool {
    matches!(c, 'A'..='Z' | '_' | 'a'..='z' | '\u{c0}'..='\u{d6}' |
        '\u{d8}'..='\u{f6}' | '\u{f8}'..='\u{2ff}' | '\u{370}'..='\u{37d}' |
        '\u{37f}'..='\u{1fff}' | '\u{200c}'..='\u{200d}' | '\u{2070}'..='\u{218f}' |
        '\u{2c00}'..='\u{2fef}' | '\u{3001}'..='\u{d7ff}' | '\u{f900}'..='\u{fdcf}' |
        '\u{fdf0}'..='\u{fffd}' | '\u{10000}'..='\u{effff}')
}
fn valid_xml_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(xml_name_start) && characters.all(|c| {
        xml_name_start(c) || matches!(c, '-' | '.' | '0'..='9' | '\u{b7}' | '\u{300}'..='\u{36f}' | '\u{203f}'..='\u{2040}')
    })
}
fn node_from_start(start: &BytesStart<'_>) -> Result<Node, TemplateError> {
    let name = start.name().as_ref().to_owned();
    if name.contains(':') { return Err(TemplateError::Unsupported("namespaced XML".into())); }
    if !valid_xml_name(&name) { return Err(TemplateError::Xml(format!("invalid element name {name:?}"))); }
    let mut node = Node { name, ..Node::default() };
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|e| TemplateError::Xml(e.to_string()))?;
        let key = attribute.key.as_ref();
        if key == "xmlns" || key.starts_with("xmlns:") || key.contains(':') {
            return Err(TemplateError::Unsupported("namespaced XML".into()));
        }
        if !valid_xml_name(key) { return Err(TemplateError::Xml(format!("invalid attribute name {key:?}"))); }
        if attribute.value.contains('<') { return Err(TemplateError::Xml("unescaped '<' in attribute".into())); }
        let value = attribute.normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| TemplateError::Xml(e.to_string()))?;
        if !value.chars().all(xml_char) { return Err(TemplateError::Xml("invalid XML attribute character".into())); }
        node.attributes.push((key.to_owned(), value.into_owned()));
    }
    Ok(node)
}
fn attach(node: Node, stack: &mut [Node], root: &mut Option<Node>) -> Result<(), TemplateError> {
    if let Some(parent) = stack.last_mut() {
        parent.text.push_str(&node.text);
        parent.children.push(node);
    } else if root.is_none() {
        *root = Some(node);
    } else {
        return Err(TemplateError::Xml("more than one root element".into()));
    }
    Ok(())
}
fn parse_document(xml: &str) -> Result<Node, TemplateError> {
    if !xml.chars().all(xml_char) { return Err(TemplateError::Xml("invalid XML character".into())); }
    let mut reader = Reader::from_str(xml.trim_start_matches('\u{feff}'));
    reader.config_mut().check_comments = true;
    let mut root = None;
    let mut stack = Vec::<Node>::new();
    let mut seen_decl = false;
    loop {
        let event = reader.read_event().map_err(|e| TemplateError::Xml(e.to_string()))?;
        match event {
            Event::Start(start) => stack.push(node_from_start(&start)?),
            Event::Empty(start) => attach(node_from_start(&start)?, &mut stack, &mut root)?,
            Event::End(_) => {
                let node = stack.pop().ok_or_else(|| TemplateError::Xml("unexpected closing element".into()))?;
                attach(node, &mut stack, &mut root)?;
            }
            Event::Text(text) => {
                if text.as_ref().contains("]]>") { return Err(TemplateError::Xml("']]>' outside CDATA".into())); }
                let value = text.xml10_content();
                if let Some(node) = stack.last_mut() { node.text.push_str(&value); }
                else if !value.trim().is_empty() { return Err(TemplateError::Xml("text outside root element".into())); }
            }
            Event::CData(text) => {
                let value = text.xml10_content();
                let node = stack.last_mut().ok_or_else(|| TemplateError::Xml("CDATA outside root element".into()))?;
                node.text.push_str(&value);
            }
            Event::GeneralRef(reference) => {
                let node = stack.last_mut().ok_or_else(|| TemplateError::Xml("entity outside root element".into()))?;
                if let Some(character) = reference.resolve_char_ref().map_err(|e| TemplateError::Xml(e.to_string()))? {
                    if !xml_char(character) { return Err(TemplateError::Xml("invalid XML character reference".into())); }
                    node.text.push(character);
                } else if let Some(value) = quick_xml::escape::resolve_xml_entity(reference.as_ref()) {
                    node.text.push_str(value);
                } else {
                    return Err(TemplateError::Unsupported(format!("unresolved entity &{};", reference.as_ref())));
                }
            }
            Event::Decl(decl) => {
                if seen_decl || root.is_some() || !stack.is_empty() {
                    return Err(TemplateError::Xml("misplaced XML declaration".into()));
                }
                seen_decl = true;
                let version = decl.version().map_err(|e| TemplateError::Xml(e.to_string()))?;
                if version != "1.0" { return Err(TemplateError::Unsupported(format!("XML version {version}"))); }
                if let Some(encoding) = decl.encoding() {
                    let encoding = encoding.map_err(|e| TemplateError::Xml(e.to_string()))?;
                    if !encoding.eq_ignore_ascii_case("utf-8") {
                        return Err(TemplateError::Unsupported(format!("XML encoding {encoding}")));
                    }
                }
            }
            Event::DocType(_) => return Err(TemplateError::Unsupported("DTD and external entities".into())),
            Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => break,
        }
    }
    if !stack.is_empty() { return Err(TemplateError::Xml("unclosed element".into())); }
    root.ok_or_else(|| TemplateError::Xml("missing root element".into()))
}

fn parse_float(name: &str, value: Option<&str>) -> Result<f32, TemplateError> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() { return Ok(0.0); }
    let parsed = value.parse::<f32>().map_err(|_| TemplateError::InvalidNumber { field: name.into(), value: value.into() })?;
    if !parsed.is_finite() {
        return Err(TemplateError::InvalidNumber { field: name.into(), value: value.into() });
    }
    Ok(parsed)
}
fn parse_int(name: &str, value: Option<&str>) -> Result<i32, TemplateError> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() { return Ok(0); }
    let (negative, digits) = if let Some(rest) = value.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = value.strip_prefix('+') {
        (false, rest)
    } else {
        (false, value)
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TemplateError::InvalidNumber { field: name.into(), value: value.into() });
    }
    // NSString.intValue saturates out-of-range decimal integers to the i32 bounds.
    let limit = if negative { 2_147_483_648u64 } else { 2_147_483_647u64 };
    let mut magnitude = 0u64;
    for digit in digits.bytes() {
        magnitude = magnitude.saturating_mul(10).saturating_add(u64::from(digit - b'0')).min(limit);
    }
    if negative {
        Ok((-(magnitude as i64)) as i32)
    } else {
        Ok(magnitude as i32)
    }
}
fn import_range(value: f32) -> f32 {
    let x = f64::from(value) + 1.0;
    if x >= 0.0 { 0.0 } else { x as f32 }
}
fn export_range(value: f32) -> f32 { if value < -1.0 { value - 1.0 } else { 0.0 } }
fn import_level(value: f32) -> f32 {
    let sum = f64::from(value) + 1.0;
    let x = if sum >= 0.0 { 0.0 } else { sum };
    let y = if x < -11.5 { x }
        else if x <= -9.0 { -11.5 + 2.0 * ((x + 11.5) / 2.5) }
        else if x <= -7.5 { -9.5 + 1.0 * ((x + 9.0) / 1.5) }
        else { -8.5 + 0.5 * ((x + 7.5) / 2.5) };
    y as f32
}
fn export_level(value: f32) -> f32 {
    if value >= -1.0 { return 0.0; }
    let y = f64::from(value);
    let x = if y < -11.5 { y }
        else if y <= -9.5 { -11.5 + 2.5 * ((y + 11.5) / 2.0) }
        else if y <= -8.5 { -9.0 + 1.5 * ((y + 9.5) / 1.0) }
        else { -7.5 + 2.5 * ((y + 8.5) / 0.5) };
    (x - 1.0) as f32
}
fn read_bands(node: &Node, prefix: &str) -> Result<([Band; 4], [f32; 3]), TemplateError> {
    let mut bands = [Band::default(); 4];
    for (index, band) in bands.iter_mut().enumerate() {
        let stem = format!("{prefix}{}", index + 1);
        band.threshold = node.float(&format!("{stem}Threshold"))?;
        band.gain = node.float(&format!("{stem}Gain"))?;
        band.ratio = node.float(&format!("{stem}Ratio"))?;
        band.attack = node.float(&format!("{stem}Attack"))?;
        band.release = node.float(&format!("{stem}Release"))?;
    }
    let mut crossovers = [0.0; 3];
    set_midpoint12(&mut crossovers, node.float(&format!("{prefix}Midpoint1"))?);
    set_midpoint23(&mut crossovers, node.float(&format!("{prefix}Midpoint2"))?);
    set_midpoint34(&mut crossovers, node.float(&format!("{prefix}Midpoint3"))?);
    Ok((bands, crossovers))
}
fn is_known_field(name: &str) -> bool {
    if matches!(name, "Name" | "Description" | "BuiltIn" | "Default" | "AnalysisVolume" |
        "ClippedPeaksEnabled" | "ClippedPeaksMin" | "ClippedPeaksMax" | "ClippedPeaksPercentage" |
        "ClippedPeaksHeadroom" | "PitchEnabled" | "PitchCents" | "PitchType" | "ExciterOption" |
        "ExpansionEnabled" | "ExpansionMin" | "ExpansionMax" | "ExpansionOutput" | "ExpansionHeadroom" |
        "CompressionEnabled" | "CompressionMin" | "CompressionMax" | "CompressionOutput" |
        "OutputVolume" | "OutputMargin" | "OutputCharacter") { return true; }
    for prefix in ["Expansion", "Compression"] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            if matches!(suffix, "Midpoint1" | "Midpoint2" | "Midpoint3") { return true; }
            if matches!(suffix.as_bytes().first().copied(), Some(b'1'..=b'4')) {
                let rest = &suffix[1..];
                if matches!(rest, "Threshold" | "Gain" | "Ratio" | "Attack" | "Release") { return true; }
            }
        }
    }
    false
}
fn parse_template(node: &Node) -> Result<Template, TemplateError> {
    let version = parse_int("version", node.attribute("version"))?;
    if version > 1 { return Err(TemplateError::UnsupportedVersion(version)); }
    let mut template = Template::default();
    template.unique_id = node.attribute("uniqueId").map(str::to_owned);
    template.name = node.value("Name").map(str::to_owned);
    template.description = node.value("Description").map(str::to_owned);
    template.is_builtin = node.boolean("BuiltIn");
    template.is_default = node.boolean("Default");
    template.analysis_volume = node.float("AnalysisVolume")?;
    template.clip.enabled = node.boolean("ClippedPeaksEnabled");
    template.clip.set_low(import_range(node.float("ClippedPeaksMin")?));
    template.clip.set_high(import_range(node.float("ClippedPeaksMax")?));
    template.clip.percentage = node.float("ClippedPeaksPercentage")?;
    template.clip.headroom = node.float("ClippedPeaksHeadroom")?;
    template.pitch.enabled = node.boolean("PitchEnabled");
    template.pitch.threshold_cents = i64::from(node.int("PitchCents")?);
    template.pitch.option = node.int("PitchType")?;
    template.exciter_option = node.int("ExciterOption")?;
    template.expansion.enabled = node.boolean("ExpansionEnabled");
    template.expansion.set_low(import_range(node.float("ExpansionMin")?));
    template.expansion.set_high(import_range(node.float("ExpansionMax")?));
    template.expansion.target_volume = node.float("ExpansionOutput")?;
    template.expansion.headroom = node.float("ExpansionHeadroom")?;
    (template.expansion.bands, template.expansion.crossovers_hz) = read_bands(node, "Expansion")?;
    template.compression.enabled = node.boolean("CompressionEnabled");
    template.compression.low = import_range(node.float("CompressionMin")?);
    template.compression.high = import_range(node.float("CompressionMax")?);
    template.compression.target_volume = node.float("CompressionOutput")?;
    (template.compression.bands, template.compression.crossovers_hz) = read_bands(node, "Compression")?;
    template.output.limiter_threshold = import_level(node.float("OutputVolume")?);
    template.output.margin = node.float("OutputMargin")?;
    template.output.character = i64::from(node.int("OutputCharacter")?);
    Ok(template)
}

/// Parse direct Template children. The root's spelling is not restricted by the native loader.
/// Unknown fields are ignored; use `parse_xml_report` to observe their names.
pub fn parse_xml(xml: &str) -> Result<Vec<Template>, TemplateError> {
    Ok(parse_xml_inner(xml, false)?.templates)
}
pub fn parse_xml_report(xml: &str) -> Result<ParsedTemplates, TemplateError> {
    parse_xml_inner(xml, true)
}
fn parse_xml_inner(xml: &str, report_ignored: bool) -> Result<ParsedTemplates, TemplateError> {
    let root = parse_document(xml)?;
    let mut result = ParsedTemplates { templates: Vec::new(), ignored_elements: Vec::new(), ignored_attributes: Vec::new() };
    if report_ignored {
        result.ignored_attributes.extend(root.attributes.iter().map(|(name, _)| IgnoredAttribute {
            template_index: None, element: root.name.clone(), name: name.clone(),
        }));
    }
    for node in &root.children {
        if node.name != "Template" {
            if report_ignored {
                result.ignored_elements.push(IgnoredElement { template_index: None, name: node.name.clone() });
                report_attributes(node, None, false, &mut result.ignored_attributes);
            }
            continue;
        }
        let index = result.templates.len();
        let template = parse_template(node)?;
        if report_ignored {
            report_attributes(node, Some(index), true, &mut result.ignored_attributes);
            result.ignored_elements.extend(node.children.iter().filter(|child| !is_known_field(&child.name)).map(|child| IgnoredElement {
                template_index: Some(index), name: child.name.clone(),
            }));
        }
        result.templates.push(template);
    }
    Ok(result)
}

fn report_attributes(root: &Node, template_index: Option<usize>, template_root: bool, output: &mut Vec<IgnoredAttribute>) {
    let mut stack = vec![(root, template_root)];
    while let Some((node, is_template_root)) = stack.pop() {
        output.extend(node.attributes.iter().filter(|(name, _)| {
            !is_template_root || (name != "version" && name != "uniqueId")
        }).map(|(name, _)| IgnoredAttribute {
            template_index, element: node.name.clone(), name: name.clone(),
        }));
        stack.extend(node.children.iter().rev().map(|child| (child, false)));
    }
}

fn writer_error(e: std::io::Error) -> TemplateError { TemplateError::Xml(e.to_string()) }
fn text_element(writer: &mut Writer<Vec<u8>>, name: &str, value: &str) -> Result<(), TemplateError> {
    if !value.chars().all(xml_char) { return Err(TemplateError::Xml(format!("invalid XML character in {name}"))); }
    writer.write_event(Event::Start(BytesStart::new(name))).map_err(writer_error)?;
    writer.write_event(Event::Text(BytesText::new(value))).map_err(writer_error)?;
    writer.write_event(Event::End(BytesEnd::new(name))).map_err(writer_error)?;
    Ok(())
}
fn description_element(writer: &mut Writer<Vec<u8>>, value: &str) -> Result<(), TemplateError> {
    if !value.chars().all(xml_char) { return Err(TemplateError::Xml("invalid XML character in Description".into())); }
    writer.write_event(Event::Start(BytesStart::new("Description"))).map_err(writer_error)?;
    let mut remaining = value;
    while let Some(index) = remaining.find("]]>") {
        writer.write_event(Event::CData(BytesCData::new(&remaining[..index + 2]))).map_err(writer_error)?;
        remaining = &remaining[index + 2..];
    }
    writer.write_event(Event::CData(BytesCData::new(remaining))).map_err(writer_error)?;
    writer.write_event(Event::End(BytesEnd::new("Description"))).map_err(writer_error)?;
    Ok(())
}
fn float_element(writer: &mut Writer<Vec<u8>>, name: &str, value: f32) -> Result<(), TemplateError> {
    if !value.is_finite() { return Err(TemplateError::Unsupported(format!("non-finite value in {name}"))); }
    text_element(writer, name, &format!("{:.1}", f64::from(value)))
}
fn bool_element(writer: &mut Writer<Vec<u8>>, name: &str, value: bool) -> Result<(), TemplateError> {
    text_element(writer, name, if value { "true" } else { "false" })
}
fn write_bands(writer: &mut Writer<Vec<u8>>, prefix: &str, bands: &[Band; 4], c: [f32; 3]) -> Result<(), TemplateError> {
    for (i, band) in bands.iter().enumerate() {
        let stem = format!("{prefix}{}", i + 1);
        for (field, value) in [("Threshold", band.threshold), ("Gain", band.gain), ("Ratio", band.ratio), ("Attack", band.attack), ("Release", band.release)] {
            float_element(writer, &format!("{stem}{field}"), value)?;
        }
    }
    for (i, value) in c.into_iter().enumerate() {
        float_element(writer, &format!("{prefix}Midpoint{}", i + 1), value)?;
    }
    Ok(())
}
fn write_one(writer: &mut Writer<Vec<u8>>, t: &Template) -> Result<(), TemplateError> {
    let id = t.unique_id.as_deref().unwrap_or("");
    if !id.chars().all(xml_char) { return Err(TemplateError::Xml("invalid XML character in uniqueId".into())); }
    let mut start = BytesStart::new("Template");
    start.push_attribute(("version", "1"));
    start.push_attribute(("uniqueId", id));
    writer.write_event(Event::Start(start)).map_err(writer_error)?;
    text_element(writer, "Name", t.name.as_deref().unwrap_or(""))?;
    description_element(writer, t.description.as_deref().unwrap_or(""))?;
    bool_element(writer, "BuiltIn", t.is_builtin)?;
    bool_element(writer, "Default", t.is_default)?;
    float_element(writer, "AnalysisVolume", t.analysis_volume)?;
    bool_element(writer, "ClippedPeaksEnabled", t.clip.enabled)?;
    for (name, value) in [("ClippedPeaksMin", t.clip.low), ("ClippedPeaksMax", t.clip.high)] {
        if !value.is_finite() { return Err(TemplateError::Unsupported(format!("non-finite value in {name}"))); }
        float_element(writer, name, export_range(value))?;
    }
    float_element(writer, "ClippedPeaksPercentage", t.clip.percentage)?;
    float_element(writer, "ClippedPeaksHeadroom", t.clip.headroom)?;
    bool_element(writer, "PitchEnabled", t.pitch.enabled)?;
    text_element(writer, "PitchCents", &t.pitch.threshold_cents.to_string())?;
    // This call zero-extends the i32 argument in w3 (0x1000258d0).
    text_element(writer, "PitchType", &(t.pitch.option as u32).to_string())?;
    text_element(writer, "ExciterOption", &t.exciter_option.to_string())?;
    bool_element(writer, "ExpansionEnabled", t.expansion.enabled)?;
    for (name, value) in [("ExpansionMin", t.expansion.low), ("ExpansionMax", t.expansion.high)] {
        if !value.is_finite() { return Err(TemplateError::Unsupported(format!("non-finite value in {name}"))); }
        float_element(writer, name, export_range(value))?;
    }
    float_element(writer, "ExpansionOutput", t.expansion.target_volume)?;
    float_element(writer, "ExpansionHeadroom", t.expansion.headroom)?;
    write_bands(writer, "Expansion", &t.expansion.bands, t.expansion.crossovers_hz)?;
    bool_element(writer, "CompressionEnabled", t.compression.enabled)?;
    for (name, value) in [("CompressionMin", t.compression.low), ("CompressionMax", t.compression.high)] {
        if !value.is_finite() { return Err(TemplateError::Unsupported(format!("non-finite value in {name}"))); }
        float_element(writer, name, export_range(value))?;
    }
    float_element(writer, "CompressionOutput", t.compression.target_volume)?;
    write_bands(writer, "Compression", &t.compression.bands, t.compression.crossovers_hz)?;
    if !t.output.limiter_threshold.is_finite() { return Err(TemplateError::Unsupported("non-finite OutputVolume".into())); }
    float_element(writer, "OutputVolume", export_level(t.output.limiter_threshold))?;
    float_element(writer, "OutputMargin", t.output.margin)?;
    text_element(writer, "OutputCharacter", &t.output.character.to_string())?;
    writer.write_event(Event::End(BytesEnd::new("Template"))).map_err(writer_error)?;
    Ok(())
}

/// Native-style XML writer: direct save, no identity change, one decimal per float.
/// XML cannot preserve nil strings, limiter release, exact decimal precision, or all endpoints.
pub fn write_xml(templates: &[Template]) -> Result<String, TemplateError> {
    write_template_refs(templates)
}

/// A change caused by the native XML representation and subsequent reimport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializationLoss {
    pub template_index: usize,
    pub field: String,
    /// Float values use IEEE-754 bits; other values use Rust debug notation.
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenTemplates {
    pub xml: String,
    pub losses: Vec<SerializationLoss>,
}

fn scalar_loss<T: PartialEq + fmt::Debug>(
    losses: &mut Vec<SerializationLoss>, index: usize, field: &str, before: &T, after: &T,
) {
    if before != after {
        losses.push(SerializationLoss {
            template_index: index, field: field.into(),
            before: format!("{before:?}"), after: format!("{after:?}"),
        });
    }
}

fn float_loss(
    losses: &mut Vec<SerializationLoss>, index: usize, field: &str, before: f32, after: f32,
) {
    if before.to_bits() != after.to_bits() {
        losses.push(SerializationLoss {
            template_index: index, field: field.into(),
            before: format!("0x{:08x}", before.to_bits()), after: format!("0x{:08x}", after.to_bits()),
        });
    }
}

fn dynamics_losses(
    losses: &mut Vec<SerializationLoss>, index: usize, prefix: &str,
    before_bands: &[Band; 4], after_bands: &[Band; 4], before_crossovers: [f32; 3],
    after_crossovers: [f32; 3],
) {
    for i in 0..4 {
        let a = before_bands[i];
        let b = after_bands[i];
        for (name, before, after) in [
            ("threshold", a.threshold, b.threshold), ("gain", a.gain, b.gain),
            ("ratio", a.ratio, b.ratio), ("attack", a.attack, b.attack),
            ("release", a.release, b.release),
        ] {
            if before.to_bits() != after.to_bits() {
                float_loss(losses, index, &format!("{prefix}.bands[{i}].{name}"), before, after);
            }
        }
    }
    for i in 0..3 {
        if before_crossovers[i].to_bits() != after_crossovers[i].to_bits() {
            float_loss(losses, index, &format!("{prefix}.crossovers_hz[{i}]"), before_crossovers[i], after_crossovers[i]);
        }
    }
}

/// Write the same native XML as `write_xml` and enumerate every effective field
/// changed by reimport. This reports native loss; it does not change the export.
pub fn write_xml_report(templates: &[Template]) -> Result<WrittenTemplates, TemplateError> {
    let xml = write_xml(templates)?;
    let reloaded = parse_xml(&xml)?;
    let mut losses = Vec::new();
    for (index, (a, b)) in templates.iter().zip(&reloaded).enumerate() {
        scalar_loss(&mut losses, index, "unique_id", &a.unique_id, &b.unique_id);
        scalar_loss(&mut losses, index, "name", &a.name, &b.name);
        scalar_loss(&mut losses, index, "description", &a.description, &b.description);
        scalar_loss(&mut losses, index, "is_builtin", &a.is_builtin, &b.is_builtin);
        scalar_loss(&mut losses, index, "is_default", &a.is_default, &b.is_default);
        float_loss(&mut losses, index, "analysis_volume", a.analysis_volume, b.analysis_volume);
        scalar_loss(&mut losses, index, "clip.enabled", &a.clip.enabled, &b.clip.enabled);
        float_loss(&mut losses, index, "clip.low", a.clip.low, b.clip.low);
        float_loss(&mut losses, index, "clip.high", a.clip.high, b.clip.high);
        float_loss(&mut losses, index, "clip.percentage", a.clip.percentage, b.clip.percentage);
        float_loss(&mut losses, index, "clip.headroom", a.clip.headroom, b.clip.headroom);
        scalar_loss(&mut losses, index, "pitch.enabled", &a.pitch.enabled, &b.pitch.enabled);
        scalar_loss(&mut losses, index, "pitch.threshold_cents", &a.pitch.threshold_cents, &b.pitch.threshold_cents);
        scalar_loss(&mut losses, index, "pitch.option", &a.pitch.option, &b.pitch.option);
        scalar_loss(&mut losses, index, "exciter_option", &a.exciter_option, &b.exciter_option);
        scalar_loss(&mut losses, index, "expansion.enabled", &a.expansion.enabled, &b.expansion.enabled);
        float_loss(&mut losses, index, "expansion.low", a.expansion.low, b.expansion.low);
        float_loss(&mut losses, index, "expansion.high", a.expansion.high, b.expansion.high);
        float_loss(&mut losses, index, "expansion.target_volume", a.expansion.target_volume, b.expansion.target_volume);
        float_loss(&mut losses, index, "expansion.headroom", a.expansion.headroom, b.expansion.headroom);
        dynamics_losses(&mut losses, index, "expansion", &a.expansion.bands, &b.expansion.bands, a.expansion.crossovers_hz, b.expansion.crossovers_hz);
        scalar_loss(&mut losses, index, "compression.enabled", &a.compression.enabled, &b.compression.enabled);
        float_loss(&mut losses, index, "compression.low", a.compression.low, b.compression.low);
        float_loss(&mut losses, index, "compression.high", a.compression.high, b.compression.high);
        float_loss(&mut losses, index, "compression.target_volume", a.compression.target_volume, b.compression.target_volume);
        dynamics_losses(&mut losses, index, "compression", &a.compression.bands, &b.compression.bands, a.compression.crossovers_hz, b.compression.crossovers_hz);
        float_loss(&mut losses, index, "output.limiter_threshold", a.output.limiter_threshold, b.output.limiter_threshold);
        float_loss(&mut losses, index, "output.margin", a.output.margin, b.output.margin);
        scalar_loss(&mut losses, index, "output.character", &a.output.character, &b.output.character);
        float_loss(&mut losses, index, "output.limiter_release", a.output.limiter_release, b.output.limiter_release);
    }
    Ok(WrittenTemplates { xml, losses })
}
fn write_template_refs<'a>(templates: impl IntoIterator<Item = &'a Template>) -> Result<String, TemplateError> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None))).map_err(writer_error)?;
    writer.write_event(Event::Start(BytesStart::new("Templates"))).map_err(writer_error)?;
    for template in templates { write_one(&mut writer, template)?; }
    writer.write_event(Event::End(BytesEnd::new("Templates"))).map_err(writer_error)?;
    String::from_utf8(writer.into_inner()).map_err(|e| TemplateError::Xml(e.to_string()))
}
