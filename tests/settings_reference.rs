use platinum_engine::file_workflow::{AudioFormat, OutputMode};
use platinum_engine::settings::{Mp3Mode, SelectedTemplate, Settings, SettingsError, SettingsStore, TemplateRef};
use platinum_engine::templates::parse_xml;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        for _ in 0..128 {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("platinum-settings-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create fixture {}: {e}", path.display()),
            }
        }
        panic!("cannot create isolated fixture")
    }
    fn store(&self, cores: usize) -> SettingsStore { SettingsStore::new(self.0.join("state"), self.0.clone(), cores) }
}
impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }

#[test]
fn defaults_config_paths_clamping_and_reset_are_explicit() {
    let f = Fixture::new();
    let default_directory = f.0.join("Library/Application Support/Platinum Engine");
    assert_eq!(SettingsStore::in_home(f.0.clone(), 8).directory(), default_directory.as_path());
    let store = f.store(8);
    let mut loaded = store.load().unwrap();
    let settings = &mut loaded.settings;
    assert_eq!(settings.output.format, AudioFormat::Mp3);
    assert_eq!(settings.mp3_mode, Mp3Mode::Cbr320);
    assert_eq!(settings.processor_max, 8);
    assert_eq!(settings.output.mode, OutputMode::SpecificFolder { directory: f.0.join("Music/Platinum Engine Output"), mirror: false });
    assert!(!f.0.join("Music/Platinum Engine Output").exists());
    assert!(settings.selected_template.is_none());
    assert_eq!(settings.selected_template().unwrap().name.as_deref(), Some("The Official Platinum Notes Template"));
    let shown = settings.config_show().unwrap();
    assert_eq!(shown["output"]["format"], "mp3");
    assert_eq!(shown["output"]["mode"]["specific_folder"]["mirror"], false);
    assert_eq!(shown["mp3_mode"], "cbr320");
    assert_eq!(shown["effective_selected_template"]["builtin"]["id"], "1D466ABE-2536-4ACC-A49D-D46C3F6DFF23");
    assert_eq!(settings.config_get("libraries.lame").unwrap(), Value::Null);
    assert!(shown.get("custom_templates").is_none());

    settings.config_set("processor_max", json!(-100), 8).unwrap();
    assert_eq!(settings.processor_max, 1);
    settings.config_set("processor_max", json!(5000), 8).unwrap();
    assert_eq!(settings.processor_max, 8);
    settings.config_set("processor_max", json!(0), 0).unwrap();
    assert_eq!(settings.processor_max, 1);
    settings.config_set("mp3_mode", json!("vbr192"), 8).unwrap();
    settings.config_set("libraries.flac", json!("/public/libFLAC.dylib"), 8).unwrap();
    settings.config_set("libraries.flac_sha256", json!("b".repeat(64)), 8).unwrap();
    settings.config_set("libraries.lame", json!("/public/libmp3lame.dylib"), 8).unwrap();
    settings.config_set("libraries.lame_sha256", json!("a".repeat(64)), 8).unwrap();
    settings.config_set("libraries.tag_bridge", json!("/public/tag-bridge.dylib"), 8).unwrap();
    let before = settings.config_show().unwrap();
    assert!(settings.config_set("libraries.lame_sha256", json!("bad"), 8).is_err());
    assert!(settings.config_set("libraries.flac_sha256", json!("bad"), 8).is_err());
    assert!(settings.config_set("processor_max", json!(2.5), 8).is_err());
    assert!(settings.config_set("libraries.flac", json!(""), 8).is_err());
    assert!(settings.config_set("unknown", json!(1), 8).is_err());
    assert_eq!(settings.config_show().unwrap(), before);
    settings.config_set("output.mode", json!({"replace":{"backup_directory":"/tmp/backup"}}), 8).unwrap();
    assert_eq!(settings.output.format, AudioFormat::MatchInput);
    assert!(settings.config_set("output.format", json!("wav"), 8).is_err());
    settings.config_reset(Some("output.mode"), &f.0, 8).unwrap();
    settings.config_reset(Some("output.format"), &f.0, 8).unwrap();
    assert_eq!(settings.output.format, AudioFormat::Mp3);
    let custom = settings.clone_template(TemplateRef::Index(0)).unwrap();
    settings.select_template(TemplateRef::Index(custom)).unwrap();
    settings.config_reset(None, &f.0, 8).unwrap();
    assert_eq!(settings.custom_templates.len(), 1);
    assert!(settings.selected_template.is_none());
    assert_eq!(settings.mp3_mode, Mp3Mode::Cbr320);
    assert_eq!(settings.processor_max, 8);
    assert_eq!(settings.libraries.flac, None);
    store.save(&mut loaded).unwrap();
}

#[test]
fn all_preferences_and_builtin_selection_persist() {
    let f = Fixture::new();
    let store = f.store(8);
    let mut loaded = store.load().unwrap();
    for (field, value) in [
        ("output.mode", json!({"specific_folder":{"directory":"/tmp/target","mirror":true}})),
        ("output.format", json!("flac")),
        ("processor_max", json!(3)),
        ("mp3_mode", json!("vbr192")),
        ("libraries.flac", json!("/opt/flac.dylib")),
        ("libraries.flac_sha256", json!("B".repeat(64))),
        ("libraries.lame", json!("/opt/lame.dylib")),
        ("libraries.lame_sha256", json!("a".repeat(64))),
        ("libraries.tag_bridge", json!("/opt/tag-bridge.dylib")),
    ] {
        loaded.settings.config_set(field, value, 8).unwrap();
    }
    loaded.settings.select_template(TemplateRef::Index(1)).unwrap();
    let expected = loaded.settings.config_show().unwrap();
    store.save(&mut loaded).unwrap();
    let actual = store.load().unwrap().settings;
    assert_eq!(actual.config_show().unwrap(), expected);
    assert_eq!(actual.selected_template().unwrap().name.as_deref(), Some("Festival"));
    assert_eq!(actual.output.format, AudioFormat::Flac);
    assert_eq!(actual.mp3_mode, Mp3Mode::Vbr192);
    assert_eq!(actual.libraries.flac_sha256.as_deref(), Some("B".repeat(64).as_str()));
}

#[test]
fn persistence_uses_effective_float_bits_and_stable_custom_identity() {
    let f = Fixture::new();
    let store = f.store(6);
    let mut loaded = store.load().unwrap();
    let index = loaded.settings.clone_template(TemplateRef::Index(0)).unwrap();
    loaded.settings.select_template(TemplateRef::Index(index)).unwrap();
    let t = &mut loaded.settings.custom_templates[0];
    let id = t.unique_id.clone();
    t.analysis_volume = -0.0;
    t.clip.percentage = f32::from_bits(0x7fc01234);
    t.output.limiter_release = f32::from_bits(0xff800000);
    t.expansion.crossovers_hz = [-0.0, 1.0, f32::INFINITY];
    t.pitch.threshold_cents = i64::MIN;
    store.save(&mut loaded).unwrap();
    let text = fs::read_to_string(store.settings_path()).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["version"], 1);
    assert_eq!(json["settings"]["custom_templates"][0]["analysis_volume"], "0x80000000");
    assert_eq!(json["settings"]["custom_templates"][0]["clip"]["percentage"], "0x7fc01234");
    assert_eq!(json["settings"]["custom_templates"][0]["output"]["limiter_release"], "0xff800000");
    assert_eq!(json["settings"]["custom_templates"][0]["pitch"]["threshold_cents"], i64::MIN.to_string());
    assert!(!text.contains("<Templates>"));
    let mut again = store.load().unwrap();
    let t = &again.settings.custom_templates[0];
    assert_eq!(t.unique_id, id);
    assert_eq!(t.analysis_volume.to_bits(), 0x80000000);
    assert_eq!(t.clip.percentage.to_bits(), 0x7fc01234);
    assert_eq!(t.output.limiter_release.to_bits(), 0xff800000);
    assert_eq!(t.expansion.crossovers_hz.map(f32::to_bits), [0x80000000, 1.0_f32.to_bits(), f32::INFINITY.to_bits()]);
    assert_eq!(t.pitch.threshold_cents, i64::MIN);
    assert_eq!(again.settings.selected_template().unwrap().unique_id, id);
    store.save(&mut again).unwrap();
    let last = store.load().unwrap();
    assert_eq!(last.settings.custom_templates[0].unique_id, id);
    assert_eq!(last.settings.custom_templates[0].clip.percentage.to_bits(), 0x7fc01234);
    let fewer_cores = f.store(2).load().unwrap();
    assert_eq!(fewer_cores.settings.processor_max, 2);
}

#[test]
fn stale_snapshots_and_external_corruption_never_overwrite() {
    let f = Fixture::new();
    let store = f.store(4);
    let mut first = store.load().unwrap();
    let mut stale = store.load().unwrap();
    first.settings.config_set("processor_max", json!(3), 4).unwrap();
    store.save(&mut first).unwrap();
    stale.settings.config_set("processor_max", json!(2), 4).unwrap();
    assert!(matches!(store.save(&mut stale), Err(SettingsError::ConcurrentModification(_))));
    assert_eq!(store.load().unwrap().settings.processor_max, 3);

    let mut same_data_stale = store.load().unwrap();
    let mut same_data_writer = store.load().unwrap();
    let old_bytes = fs::read(store.settings_path()).unwrap();
    store.save(&mut same_data_writer).unwrap();
    assert_ne!(fs::read(store.settings_path()).unwrap(), old_bytes); // revision changes even for identical values
    assert!(matches!(store.save(&mut same_data_stale), Err(SettingsError::ConcurrentModification(_))));

    let mut previous = store.load().unwrap();
    let invalid = b"{not json";
    fs::write(store.settings_path(), invalid).unwrap();
    assert!(matches!(store.load(), Err(SettingsError::Corrupt(_))));
    assert!(matches!(store.save(&mut previous), Err(SettingsError::ConcurrentModification(_))));
    assert_eq!(fs::read(store.settings_path()).unwrap(), invalid);
    let other = SettingsStore::new(f.0.join("other"), f.0.clone(), 4);
    assert!(matches!(other.save(&mut previous), Err(SettingsError::WrongStore)));
}

#[test]
fn real_simultaneous_cli_writers_have_one_winner() {
    let f = Fixture::new();
    let store = Arc::new(f.store(4));
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (1..=2).map(|number| {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let mut snapshot = store.load().unwrap();
            snapshot.settings.processor_max = number;
            barrier.wait();
            store.save(&mut snapshot)
        })
    }).collect();
    let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|r| matches!(r, Err(SettingsError::ConcurrentModification(_)))).count(), 1);
    assert!([1, 2].contains(&store.load().unwrap().settings.processor_max));
}

#[test]
fn template_edits_cover_all_fields_and_native_ordered_setters() {
    let mut settings = Settings::defaults(Path::new("/isolated-home"), 4);
    assert!(matches!(settings.edit_template(TemplateRef::Index(0), "clip.low", json!(-2)), Err(SettingsError::BuiltinImmutable(0))));
    assert!(matches!(settings.rename_template(TemplateRef::Index(0), "no"), Err(SettingsError::BuiltinImmutable(0))));
    assert!(matches!(settings.remove_template(TemplateRef::Index(0)), Err(SettingsError::BuiltinImmutable(0))));
    let index = settings.clone_template(TemplateRef::Index(0)).unwrap();
    let id = settings.template(TemplateRef::Index(index)).unwrap().unique_id.unwrap();
    assert_ne!(id, settings.template(TemplateRef::Index(0)).unwrap().unique_id.unwrap());
    assert_eq!(id.len(), 36);
    let edits = [
        ("name", json!("Custom")), ("description", json!("editable")), ("is_default", json!(true)),
        ("analysis_volume", json!(-12.375)), ("clip.enabled", json!(false)),
        ("clip.percentage", json!(88.125)), ("clip.headroom", json!(1.25)),
        ("pitch.enabled", json!(false)), ("pitch.threshold_cents", json!(-42)),
        ("pitch.option", json!(1)), ("exciter_option", json!(-1)),
        ("expansion.enabled", json!(false)), ("expansion.target_volume", json!(-8.75)),
        ("expansion.headroom", json!(0.75)), ("compression.enabled", json!(false)),
        ("compression.low", json!(5)), ("compression.high", json!(-5)),
        ("compression.target_volume", json!(-6.25)), ("output.limiter_threshold", json!(-9.25)),
        ("output.margin", json!(-0.125)), ("output.character", json!(7)),
        ("output.limiter_release", json!(9)),
    ];
    for (field, value) in edits { settings.edit_template(TemplateRef::Index(index), field, value).unwrap(); }
    settings.edit_template(TemplateRef::Index(index), "clip.low", json!(-2)).unwrap();
    settings.edit_template(TemplateRef::Index(index), "clip.high", json!(-8)).unwrap();
    settings.edit_template(TemplateRef::Index(index), "expansion.low", json!(-2)).unwrap();
    settings.edit_template(TemplateRef::Index(index), "expansion.high", json!(-8)).unwrap();
    for kind in ["expansion", "compression"] {
        settings.edit_template(TemplateRef::Index(index), &format!("{kind}.crossovers_hz[0]"), json!(7000)).unwrap();
        settings.edit_template(TemplateRef::Index(index), &format!("{kind}.crossovers_hz[2]"), json!(25)).unwrap();
        for band in 0..4 {
            for (field, value) in [("threshold", -20.25), ("gain", 1.125), ("ratio", 2.5), ("attack", 3.75), ("release", 4.5)] {
                settings.edit_template(TemplateRef::Index(index), &format!("{kind}.bands[{band}].{field}"), json!(value)).unwrap();
            }
        }
    }
    let t = settings.template(TemplateRef::Index(index)).unwrap();
    assert_eq!(t.name.as_deref(), Some("Custom"));
    assert_eq!(t.description.as_deref(), Some("editable"));
    assert_eq!(t.analysis_volume, -12.375);
    assert_eq!((t.clip.low, t.clip.high), (-8.0, -8.0));
    assert_eq!((t.expansion.low, t.expansion.high), (-8.0, -8.0));
    assert_eq!((t.compression.low, t.compression.high), (5.0, -5.0));
    assert_eq!(t.expansion.crossovers_hz, [25.0; 3]);
    assert_eq!(t.compression.crossovers_hz, [25.0; 3]);
    for band in t.expansion.bands.into_iter().chain(t.compression.bands) {
        assert_eq!((band.threshold, band.gain, band.ratio, band.attack, band.release), (-20.25, 1.125, 2.5, 3.75, 4.5));
    }
    assert_eq!(t.pitch.threshold_cents, -42);
    assert!(!t.clip.enabled && !t.pitch.enabled && !t.expansion.enabled && !t.compression.enabled);
    assert!(t.is_default);
    assert_eq!((t.clip.percentage, t.clip.headroom), (88.125, 1.25));
    assert_eq!((t.pitch.option, t.exciter_option), (1, -1));
    assert_eq!((t.expansion.target_volume, t.expansion.headroom, t.compression.target_volume), (-8.75, 0.75, -6.25));
    assert_eq!((t.output.limiter_threshold, t.output.margin, t.output.character), (-9.25, -0.125, 7));
    assert_eq!(t.output.limiter_release, 9.0);
    for field in ["unique_id", "is_builtin", "expansion.bands[4].gain", "compression.crossovers_hz[3]"] {
        assert!(settings.edit_template(TemplateRef::Index(index), field, json!(1)).is_err());
    }
    assert!(settings.edit_template(TemplateRef::Index(index), "pitch.option", json!(2.5)).is_err());
    assert!(settings.edit_template(TemplateRef::Index(index), "clip.low", json!("-2")).is_err());
    assert_eq!(settings.template(TemplateRef::Index(index)).unwrap().unique_id.as_deref(), Some(id.as_str()));
    let clone = settings.clone_template(TemplateRef::Index(index)).unwrap();
    let cloned = settings.template(TemplateRef::Index(clone)).unwrap();
    assert_eq!(cloned.name.as_deref(), Some("Custom copy"));
    assert_eq!(cloned.description.as_deref(), Some("editable"));
    assert_eq!(cloned.output.limiter_release, 3.0);
    assert!(!cloned.is_default);
    assert_ne!(cloned.unique_id.as_deref(), Some(id.as_str()));
    settings.rename_template(TemplateRef::Index(index), "renamed").unwrap();
    settings.edit_template(TemplateRef::Index(index), "description", Value::Null).unwrap();
    assert_eq!(settings.template(TemplateRef::Index(index)).unwrap().description, None);
}

#[test]
fn import_duplicate_and_missing_ids_use_index_selection_and_survive_changes() {
    let f = Fixture::new();
    let store = f.store(4);
    let mut loaded = store.load().unwrap();
    let xml = r#"<Templates extra="root"><Template uniqueId="same" other="ignored"><Name>one</Name><Unknown x="1">x</Unknown><BuiltIn>true</BuiltIn><Default>true</Default></Template><Template uniqueId="same"><Name>two</Name></Template><Template><Name>no id</Name></Template></Templates>"#;
    let input = f.0.join("input.pntemplate");
    fs::write(&input, xml).unwrap();
    let report = loaded.settings.import_file(&input).unwrap();
    assert_eq!(report.indices, [3, 4, 5]);
    assert_eq!(report.last_index, Some(5));
    assert!(report.ignored_elements.iter().any(|e| e.template_index == Some(0) && e.name == "Unknown"));
    assert!(report.ignored_attributes.iter().any(|a| a.element == "Templates" && a.name == "extra"));
    assert!(report.ignored_attributes.iter().any(|a| a.element == "Template" && a.name == "other"));
    assert!(report.ignored_attributes.iter().any(|a| a.element == "Unknown" && a.name == "x"));
    let report_json = serde_json::to_value(&report).unwrap();
    assert_eq!(report_json["ignored_elements"][0]["name"], "Unknown");
    assert!(report_json["ignored_attributes"].is_array());
    assert!(loaded.settings.custom_templates.iter().all(|t| !t.is_builtin && !t.is_default));
    assert!(matches!(loaded.settings.template(TemplateRef::Id("same")), Err(SettingsError::AmbiguousTemplateId(_))));
    assert!(matches!(loaded.settings.select_template(TemplateRef::Id("same")), Err(SettingsError::AmbiguousTemplateId(_))));
    assert_eq!(loaded.settings.selected_template().unwrap().name.as_deref(), Some("no id"));
    loaded.settings.select_template(TemplateRef::Index(4)).unwrap();
    assert_eq!(loaded.settings.selected_template().unwrap().name.as_deref(), Some("two"));
    store.save(&mut loaded).unwrap();
    let mut loaded = store.load().unwrap();
    assert_eq!(loaded.settings.selected_template().unwrap().name.as_deref(), Some("two"));
    assert_eq!(loaded.settings.custom_templates[0].unique_id.as_deref(), Some("same"));
    assert_eq!(loaded.settings.custom_templates[1].unique_id.as_deref(), Some("same"));
    assert!(loaded.settings.custom_templates[2].unique_id.is_none());
    loaded.settings.remove_template(TemplateRef::Index(3)).unwrap();
    assert!(matches!(&loaded.settings.selected_template, Some(SelectedTemplate::Custom { index: 0, id: Some(id) }) if id == "same"));
    assert_eq!(loaded.settings.selected_template().unwrap().name.as_deref(), Some("two"));
    loaded.settings.remove_template(TemplateRef::Index(3)).unwrap();
    assert!(loaded.settings.selected_template.is_none());
    assert_eq!(loaded.settings.selected_template().unwrap().name.as_deref(), Some("The Official Platinum Notes Template"));
    store.save(&mut loaded).unwrap();
    assert_eq!(store.load().unwrap().settings.custom_templates.len(), 1);
}

#[test]
fn builtin_id_collision_is_preserved_and_requires_an_index() {
    let mut settings = Settings::defaults(Path::new("/isolated-home"), 2);
    let id = settings.template(TemplateRef::Index(0)).unwrap().unique_id.unwrap();
    let xml = format!("<Templates><Template uniqueId=\"{id}\"><Name>collision</Name></Template></Templates>");
    let report = settings.import_templates(&xml).unwrap();
    assert_eq!(report.indices, [3]);
    assert_eq!(settings.selected_template().unwrap().name.as_deref(), Some("collision"));
    assert!(matches!(settings.select_template(TemplateRef::Id(&id)), Err(SettingsError::AmbiguousTemplateId(_))));
    settings.select_template(TemplateRef::Index(0)).unwrap();
    assert_eq!(settings.selected_template().unwrap().unique_id.as_deref(), Some(id.as_str()));
}

#[test]
fn export_reports_native_copy_and_xml_loss_and_never_overwrites() {
    let f = Fixture::new();
    let mut settings = Settings::defaults(&f.0, 4);
    let index = settings.clone_template(TemplateRef::Index(0)).unwrap();
    settings.edit_template(TemplateRef::Index(index), "analysis_volume", json!(0.25)).unwrap();
    settings.edit_template(TemplateRef::Index(index), "output.limiter_release", json!(9)).unwrap();
    let original = settings.template(TemplateRef::Index(index)).unwrap();
    let path = f.0.join("custom.pntemplate");
    let report = settings.export_file(TemplateRef::Index(index), &path).unwrap();
    assert_ne!(Some(report.export_id.as_str()), original.unique_id.as_deref());
    assert!(report.copy_losses.iter().any(|loss| loss.field == "unique_id"));
    assert!(report.copy_losses.iter().any(|loss| loss.field == "output.limiter_release"));
    assert!(report.serialization_losses.iter().any(|loss| loss.field == "analysis_volume"));
    let report_json = serde_json::to_value(&report).unwrap();
    assert_eq!(report_json["export_id"], report.export_id);
    assert!(report_json["serialization_losses"].is_array());
    assert_eq!(fs::read_to_string(&path).unwrap(), report.xml);
    let exported = parse_xml(&report.xml).unwrap().remove(0);
    assert_eq!(exported.unique_id.as_deref(), Some(report.export_id.as_str()));
    assert_eq!(exported.output.limiter_release, 3.0);
    assert_eq!(exported.analysis_volume, 0.2_f32);
    let retained = fs::read(&path).unwrap();
    assert!(settings.export_file(TemplateRef::Index(index), &path).is_err());
    assert_eq!(fs::read(&path).unwrap(), retained);
    assert_eq!(settings.template(TemplateRef::Index(index)).unwrap().output.limiter_release, 9.0);
    let another = settings.export_template(TemplateRef::Index(index)).unwrap();
    assert_ne!(another.export_id, report.export_id);
}

#[test]
fn invalid_persisted_state_is_not_replaced_with_defaults() {
    let f = Fixture::new();
    let store = f.store(4);
    let mut loaded = store.load().unwrap();
    store.save(&mut loaded).unwrap();
    let path = store.settings_path();
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["version"] = json!(2);
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(matches!(store.load(), Err(SettingsError::Corrupt(_))));
    document["version"] = json!(1);
    document["settings"]["selected_template"] = json!({"custom":{"index":0,"id":"missing"}});
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(matches!(store.load(), Err(SettingsError::Corrupt(_))));
    document["settings"]["selected_template"] = Value::Null;
    document["settings"]["unexpected"] = json!(true);
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(matches!(store.load(), Err(SettingsError::Corrupt(_))));
}
