use platinum_engine::templates::{
    Band, Template, TemplateCatalog, TemplateError, builtins, parse_xml, parse_xml_report, write_xml, write_xml_report,
};
use serde_json::Value;

fn one(inner: &str) -> Template {
    let xml = format!("<Templates><Template version=\"1\" uniqueId=\"source\">{inner}</Template></Templates>");
    parse_xml(&xml).unwrap().remove(0)
}

#[test]
fn shipped_effective_states_match_complete_recovered_contract_and_original_xml() {
    let recovered: Value = serde_json::from_str(include_str!("../research/templates/builtins.json")).unwrap();
    let expected = recovered["templates_in_load_order"].as_array().unwrap();
    let states = builtins().unwrap();
    assert_eq!(states.len(), 3);
    for (state, entry) in states.iter().zip(expected) {
        assert_eq!(serde_json::to_value(state).unwrap(), entry["template"]);
    }
    assert_eq!(states[0].unique_id.as_deref(), Some("1D466ABE-2536-4ACC-A49D-D46C3F6DFF23"));
    assert_eq!(states[1].unique_id.as_deref(), Some("6C8F0DE3-5B5E-45B8-B284-07BA95F81103"));
    assert_eq!(states[2].unique_id.as_deref(), Some("7F9C4BAB-C063-46DB-970A-837E0EE27ED3"));
    assert_eq!(states[1].exciter_option, 0); // absent in Festival; legacy tags do not set it

    // A local licensed installation is optional for the rest of this suite.
    if let Ok(xml) = std::fs::read_to_string("/Applications/Platinum Notes 10.app/Contents/Resources/Templates.xml") {
        let parsed = parse_xml_report(&xml).unwrap();
        assert_eq!(parsed.templates.len(), expected.len());
        for (state, entry) in parsed.templates.iter().zip(expected) {
            assert_eq!(serde_json::to_value(state).unwrap(), entry["template"]);
        }
        assert!(parsed.ignored_elements.iter().any(|e| e.template_index == Some(1) && e.name == "ExciterPreset"));
    }
}

#[test]
fn missing_fields_last_child_text_and_legacy_exciter_match_parser() {
    let xml = r#"<Anything extra="root">
      <Wrapper><Template><Name>not direct</Name></Template></Wrapper>
      <Template extra="future"><Name note="unused">first</Name><Name/><Description>A&amp;B<![CDATA[<C>]]>&#33;</Description>
        <BuiltIn>TRUE</BuiltIn><Default>true</Default><PitchEnabled> true </PitchEnabled>
        <ExciterEnabled>true</ExciterEnabled><ExciterPreset>2</ExciterPreset>
        <UnknownSetting>not preserved</UnknownSetting></Template>
      <Template uniqueId=""><Name>  x &lt; y  </Name><ExciterOption>-1</ExciterOption></Template>
    </Anything>"#;
    let result = parse_xml_report(xml).unwrap();
    assert_eq!(result.templates.len(), 2);
    let first = &result.templates[0];
    assert_eq!(first.unique_id, None);
    assert_eq!(first.name.as_deref(), Some(""));
    assert_eq!(first.description.as_deref(), Some("A&B<C>!"));
    assert!(!first.is_builtin);
    assert!(first.is_default);
    assert!(!first.pitch.enabled);
    assert_eq!(first.exciter_option, 0);
    assert_eq!(first.output.limiter_threshold, -7.0);
    assert_eq!(first.output.limiter_release, 3.0);
    assert_eq!(first.expansion.bands, [Band::default(); 4]);
    assert_eq!(first.expansion.crossovers_hz, [0.0; 3]);
    assert_eq!(result.templates[1].unique_id.as_deref(), Some(""));
    assert_eq!(result.templates[1].name.as_deref(), Some("  x < y  "));
    assert_eq!(result.templates[1].description, None);
    assert_eq!(result.templates[1].exciter_option, -1);
    for name in ["ExciterEnabled", "ExciterPreset", "UnknownSetting"] {
        assert!(result.ignored_elements.iter().any(|e| e.template_index == Some(0) && e.name == name));
    }
    assert!(result.ignored_elements.iter().any(|e| e.template_index.is_none() && e.name == "Wrapper"));
    assert!(result.ignored_attributes.iter().any(|a| a.template_index.is_none() && a.element == "Anything" && a.name == "extra"));
    assert!(result.ignored_attributes.iter().any(|a| a.template_index == Some(0) && a.element == "Template" && a.name == "extra"));
    assert!(result.ignored_attributes.iter().any(|a| a.template_index == Some(0) && a.element == "Name" && a.name == "note"));
    assert!(!result.ignored_attributes.iter().any(|a| a.template_index == Some(1) && a.name == "uniqueId"));
}

#[test]
fn range_and_crossover_setters_are_ordered_not_sorted() {
    let t = one(r#"
      <ClippedPeaksMin>-2</ClippedPeaksMin><ClippedPeaksMax>-5</ClippedPeaksMax>
      <ExpansionMin>-2</ExpansionMin><ExpansionMax>-5</ExpansionMax>
      <CompressionMin>-2</CompressionMin><CompressionMax>-5</CompressionMax>
      <ExpansionMidpoint1>100</ExpansionMidpoint1><ExpansionMidpoint2>200</ExpansionMidpoint2><ExpansionMidpoint3>50</ExpansionMidpoint3>
      <CompressionMidpoint1>90</CompressionMidpoint1><CompressionMidpoint2>30</CompressionMidpoint2><CompressionMidpoint3>60</CompressionMidpoint3>
    "#);
    assert_eq!((t.clip.low, t.clip.high), (-4.0, -4.0));
    assert_eq!((t.expansion.low, t.expansion.high), (-4.0, -4.0));
    assert_eq!((t.compression.low, t.compression.high), (-1.0, -4.0));
    assert_eq!(t.expansion.crossovers_hz, [50.0, 50.0, 50.0]);
    assert_eq!(t.compression.crossovers_hz, [30.0, 30.0, 60.0]);
    let missing = one("<ExpansionMidpoint1>100</ExpansionMidpoint1><ExpansionMidpoint2>200</ExpansionMidpoint2>");
    assert_eq!(missing.expansion.crossovers_hz, [0.0; 3]);

    let mut expansion = t.expansion;
    expansion.set_midpoint12(100.0);
    assert_eq!(expansion.crossovers_hz, [100.0; 3]);
    expansion.set_midpoint34(25.0);
    assert_eq!(expansion.crossovers_hz, [25.0; 3]);
    expansion.set_low(-2.0);
    expansion.set_high(-8.0);
    assert_eq!((expansion.low, expansion.high), (-8.0, -8.0));
}

#[test]
fn output_volume_piecewise_boundaries_and_clamp() {
    // XML value is one lower than the piecewise input x. Exact half/quarter values
    // exercise all three breakpoints and the extrapolating final segment.
    for (xml, effective) in [
        (-13.5, -12.5), (-12.5, -11.5), (-11.25, -10.5),
        (-10.0, -9.5), (-9.25, -9.0), (-8.5, -8.5),
        (-7.25, -8.25), (-6.0, -8.0), (-1.0, -7.0), (1.0, -7.0),
    ] {
        let state = one(&format!("<OutputVolume>{xml}</OutputVolume>"));
        assert_eq!(state.output.limiter_threshold.to_bits(), (effective as f32).to_bits(), "XML {xml}");
    }
    let state = one("<OutputVolume>-12.5</OutputVolume>");
    let serialized = write_xml(&[state]).unwrap();
    assert!(serialized.contains("<OutputVolume>-12.5</OutputVolume>"));
    assert_eq!(parse_xml(&serialized).unwrap()[0].output.limiter_threshold, -11.5);
}

#[test]
fn native_export_is_lossy_for_endpoints_precision_and_non_xml_release() {
    let mut t = one("<ClippedPeaksMin>-2</ClippedPeaksMin><ClippedPeaksMax>0</ClippedPeaksMax><OutputVolume>-11.25</OutputVolume><AnalysisVolume>0.25</AnalysisVolume>");
    assert_eq!(t.clip.low, -1.0);
    assert_eq!(t.output.limiter_threshold, -10.5);
    t.output.limiter_release = 9.0;
    t.description = Some("A<&]]>B".into());
    t.name = Some("A & <title>".into());
    let report = write_xml_report(&[t]).unwrap();
    for field in ["clip.low", "analysis_volume", "output.limiter_threshold", "output.limiter_release"] {
        assert!(report.losses.iter().any(|loss| loss.template_index == 0 && loss.field == field), "missing loss for {field}");
    }
    assert!(!report.losses.iter().any(|loss| loss.field == "description"));
    let xml = report.xml;
    assert!(xml.contains("<ClippedPeaksMin>0.0</ClippedPeaksMin>"));
    assert!(xml.contains("<AnalysisVolume>0.2</AnalysisVolume>")); // Foundation %.1f ties to even
    assert!(xml.contains("<OutputVolume>-11.2</OutputVolume>"));
    assert!(xml.contains("<![CDATA[A<&]]]]><![CDATA[>B]]>"));
    let reloaded = parse_xml(&xml).unwrap().remove(0);
    assert_eq!(reloaded.unique_id.as_deref(), Some("source"));
    assert_eq!(reloaded.name.as_deref(), Some("A & <title>"));
    assert_eq!(reloaded.description.as_deref(), Some("A<&]]>B"));
    assert_eq!(reloaded.clip.low, 0.0);
    assert_eq!(reloaded.output.limiter_release, 3.0);
    assert_ne!(reloaded.output.limiter_threshold, -10.5);
    assert_eq!(reloaded.analysis_volume, 0.2_f32);
}

#[test]
fn missing_identity_and_text_become_empty_on_native_xml_write() {
    let t = parse_xml("<Templates><Template/></Templates>").unwrap().remove(0);
    assert!(t.unique_id.is_none() && t.name.is_none() && t.description.is_none());
    let xml = write_xml(&[t]).unwrap();
    assert!(xml.contains("uniqueId=\"\""));
    assert!(xml.contains("<Name></Name>"));
    assert!(xml.contains("<Description><![CDATA[]]></Description>"));
    let reloaded = parse_xml(&xml).unwrap().remove(0);
    assert_eq!(reloaded.unique_id.as_deref(), Some(""));
    assert_eq!(reloaded.name.as_deref(), Some(""));
    assert_eq!(reloaded.description.as_deref(), Some(""));
}

#[test]
fn all_disabled_band_settings_survive_native_serialization() {
    let mut t = one("<ExpansionEnabled>false</ExpansionEnabled><CompressionEnabled>false</CompressionEnabled><OutputVolume>-12.5</OutputVolume>");
    for i in 0..4 {
        let n = (i + 1) as f32;
        t.expansion.bands[i] = Band { threshold: -n, gain: n, ratio: n, attack: n * 10.0, release: n * 20.0 };
        t.compression.bands[i] = Band { threshold: -2.0 * n, gain: -n, ratio: n, attack: n * 30.0, release: n * 40.0 };
    }
    t.expansion.crossovers_hz = [95.0, 550.0, 5500.0];
    t.compression.crossovers_hz = [100.0, 600.0, 6000.0];
    t.expansion.target_volume = -14.0;
    t.expansion.headroom = 4.5;
    t.compression.target_volume = -9.0;
    let parsed = parse_xml(&write_xml(&[t.clone()]).unwrap()).unwrap().remove(0);
    assert!(!parsed.expansion.enabled && !parsed.compression.enabled);
    assert_eq!(parsed.expansion.bands, t.expansion.bands);
    assert_eq!(parsed.compression.bands, t.compression.bands);
    assert_eq!(parsed.expansion.crossovers_hz, t.expansion.crossovers_hz);
    assert_eq!(parsed.compression.crossovers_hz, t.compression.crossovers_hz);
    assert_eq!(parsed.expansion.target_volume, -14.0);
    assert_eq!(parsed.expansion.headroom, 4.5);
    assert_eq!(parsed.compression.target_volume, -9.0);
}

#[test]
fn identity_copy_export_and_import_are_distinct_operations() {
    let mut source = builtins().unwrap().remove(0);
    source.output.limiter_release = 8.0;
    source.clip.low = -2.0;
    source.clip.high = -8.0; // programmatically inconsistent: native copy uses setters
    source.expansion.crossovers_hz = [90.0, 30.0, 60.0];
    let duplicate = source.duplicate("duplicate-id").unwrap();
    assert_eq!(duplicate.unique_id.as_deref(), Some("duplicate-id"));
    assert_eq!(duplicate.name.as_deref(), Some("The Official Platinum Notes Template copy"));
    assert!(!duplicate.is_builtin && !duplicate.is_default && duplicate.editable());
    assert_eq!(duplicate.description, None);
    assert_eq!(duplicate.output.limiter_release, 3.0);
    assert_eq!((duplicate.clip.low, duplicate.clip.high), (-8.0, -8.0));
    assert_eq!(duplicate.expansion.crossovers_hz, [30.0, 30.0, 60.0]);

    let exported = source.clone_for_export("export-id").unwrap();
    assert!(exported.is_builtin && !exported.is_default);
    assert_eq!(exported.name, source.name);
    assert_eq!(exported.description, source.description);
    assert_eq!(exported.output.limiter_release, 3.0);
    let xml = source.export_xml("export-id").unwrap();
    assert!(xml.contains("uniqueId=\"export-id\""));
    let mut catalog = TemplateCatalog::new(vec![], None).unwrap();
    let index = catalog.import_xml(&xml).unwrap().unwrap();
    assert_eq!(catalog.templates[index].unique_id.as_deref(), Some("export-id"));
    assert!(!catalog.templates[index].is_builtin && !catalog.templates[index].is_default);
    assert!(source.duplicate(source.unique_id.clone().unwrap()).is_err());
    assert!(source.clone_for_export("").is_err());

    let custom_copy = duplicate.duplicate("another-id").unwrap();
    assert_eq!(custom_copy.description, duplicate.description);
    assert_eq!(custom_copy.name.as_deref(), Some("The Official Platinum Notes Template copy copy"));
}

#[test]
fn manager_selection_fallback_import_and_persistence_keep_array_order() {
    let mut first = one("<Name>first</Name><Default>false</Default>");
    first.unique_id = Some("same".into());
    let mut second = one("<Name>second</Name><Default>true</Default>");
    second.unique_id = Some("same".into());
    let mut catalog = TemplateCatalog::from_loaded(vec![first, second], vec![], Some("same".into()), ["a".into(), "b".into(), "c".into()]).unwrap();
    assert_eq!(catalog.selected().unwrap().name.as_deref(), Some("first"));
    catalog.select(1).unwrap();
    assert_eq!(catalog.selected().unwrap().name.as_deref(), Some("first")); // first ID wins
    catalog.remove(0).unwrap();
    assert_eq!(catalog.selected().unwrap().name.as_deref(), Some("second"));
    catalog.selected_id = Some("absent".into());
    assert_eq!(catalog.selected().unwrap().name.as_deref(), Some("second"));
    assert_eq!(catalog.selected_id, None);
    catalog.selected_id = Some(String::new());
    assert!(catalog.selected().is_some());
    assert_eq!(catalog.selected_id.as_deref(), Some(""));

    let xml = r#"<Templates><Template uniqueId="same"><Name>imported</Name><BuiltIn>true</BuiltIn><Default>true</Default></Template></Templates>"#;
    let last = catalog.import_xml(xml).unwrap().unwrap();
    assert!(!catalog.templates[last].is_builtin && !catalog.templates[last].is_default);
    catalog.select(last).unwrap();
    assert_eq!(catalog.selected().unwrap().name.as_deref(), Some("second"));
    let saved = parse_xml(&catalog.write_custom_xml().unwrap()).unwrap();
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[1].unique_id.as_deref(), Some("same"));

    let mut fallback = TemplateCatalog::from_loaded(vec![], vec![], None, ["one".into(), "two".into(), "three".into()]).unwrap();
    assert_eq!(fallback.templates.iter().map(|t| t.name.as_deref().unwrap()).collect::<Vec<_>>(), vec!["One", "Two", "Three"]);
    assert_eq!(fallback.selected().unwrap().unique_id.as_deref(), Some("one"));
    assert_eq!(fallback.templates[0].output.limiter_threshold, 0.0); // constructor, not XML default -7
    assert_eq!(fallback.templates[0].output.limiter_release, 3.0);
}

#[test]
fn serde_preserves_exact_float_bits_and_signed_long_fields() {
    let mut state = one("");
    state.analysis_volume = -0.0;
    state.clip.percentage = f32::from_bits(0x7fc01234);
    state.output.limiter_release = f32::from_bits(0xff800000);
    state.expansion.crossovers_hz = [-0.0, 1.0, f32::INFINITY];
    state.pitch.threshold_cents = i64::MIN;
    state.output.character = i64::MAX;
    let json = serde_json::to_value(&state).unwrap();
    assert_eq!(json["analysis_volume"], "0x80000000");
    assert_eq!(json["clip"]["percentage"], "0x7fc01234");
    assert_eq!(json["output"]["limiter_release"], "0xff800000");
    assert_eq!(json["pitch"]["threshold_cents"], i64::MIN.to_string());
    let decoded: Template = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(decoded.analysis_volume.to_bits(), state.analysis_volume.to_bits());
    assert_eq!(decoded.clip.percentage.to_bits(), state.clip.percentage.to_bits());
    assert_eq!(decoded.output.limiter_release.to_bits(), state.output.limiter_release.to_bits());
    assert_eq!(decoded.expansion.crossovers_hz.map(f32::to_bits), state.expansion.crossovers_hz.map(f32::to_bits));
    assert_eq!(decoded.pitch.threshold_cents, i64::MIN);
    let mut invalid = json;
    invalid["pitch"]["threshold_cents"] = Value::String("01".into());
    assert!(serde_json::from_value::<Template>(invalid).is_err());
}

#[test]
fn signed_integer_storage_and_native_pitchtype_zero_extension() {
    let t = one("<PitchCents>-2147483649</PitchCents><PitchType>-1</PitchType><ExciterOption>-2</ExciterOption><OutputCharacter>4294967295</OutputCharacter>");
    assert_eq!(t.pitch.threshold_cents, i32::MIN as i64);
    assert_eq!(t.output.character, i32::MAX as i64);
    let xml = write_xml(&[t]).unwrap();
    assert!(xml.contains("<PitchType>4294967295</PitchType>"));
    assert!(xml.contains("<ExciterOption>-2</ExciterOption>"));
    let reloaded = parse_xml(&xml).unwrap().remove(0);
    assert_eq!(reloaded.pitch.option, i32::MAX);
    assert_eq!(reloaded.exciter_option, -2);
}

#[test]
fn unsupported_and_malformed_inputs_return_explicit_errors() {
    assert!(matches!(parse_xml("<Templates><Template version=\"2\"/></Templates>"), Err(TemplateError::UnsupportedVersion(2))));
    assert!(matches!(parse_xml("<Templates><Template><PitchCents>12junk</PitchCents></Template></Templates>"), Err(TemplateError::InvalidNumber { .. })));
    assert!(matches!(parse_xml("<Templates><Template><OutputMargin>NaN</OutputMargin></Template></Templates>"), Err(TemplateError::InvalidNumber { .. })));
    assert!(matches!(parse_xml("<!DOCTYPE Templates><Templates/>"), Err(TemplateError::Unsupported(_))));
    assert!(matches!(parse_xml("<Templates xmlns=\"urn:other\"/>"), Err(TemplateError::Unsupported(_))));
    assert!(parse_xml("<Templates><Template></Templates>").is_err());
    assert!(parse_xml("<Templates/><Another/>").is_err());
    assert!(parse_xml("<Templates><1invalid/></Templates>").is_err());
    assert!(parse_xml("<Templates><Template uniqueId=\"bad<id\"/></Templates>").is_err());
    assert!(parse_xml("<Templates><Template><Name>bad]]>text</Name></Template></Templates>").is_err());
    assert!(parse_xml("<Templates><Template><Name>&unknown;</Name></Template></Templates>").is_err());
    let mut non_finite = one("");
    non_finite.output.margin = f32::INFINITY;
    assert!(matches!(write_xml(&[non_finite]), Err(TemplateError::Unsupported(_))));
}
