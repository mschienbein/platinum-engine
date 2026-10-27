use platinum_engine::metadata::{
    encode_marker_base64, rewrite_wav_marker, wav_pltn_chunk, Marker, MarkerError, MarkerText, ProcessingMarker,
    FLAC_XIPH_KEY, ID3_GEOB_DESCRIPTION, ID3_GEOB_MIME, MP4_FREEFORM_KEY,
};

fn sample() -> Marker {
    Marker::from_processing(ProcessingMarker {
        software_version: Some("10.5.0.1256"),
        backup_file_name: Some("/synthetic/input.wav"),
        applied_clip_repair: false,
        applied_pitch_correction: false,
        applied_warmth: false,
        expansion_applied: 0,
        compression_applied: 0,
        volume_analysis_version: 4,
        target_volume: -12.3_f32,
        input_volume: -15.2_f32,
        output_volume: -12.2_f32,
        clip_repair_count: 0,
        exciter_option: 0,
        input_pitch: 0.0,
        output_pitch: 0.0,
        speed_adjustment: 1.0,
    })
}

#[test]
fn marker_decode_preserves_values_and_declared_wav_padding() {
    let marker = sample();
    let mut payload = marker.encoded_bytes().unwrap().unwrap();
    if payload.len() & 1 != 0 { payload.push(0); }
    let decoded = Marker::from_encoded_bytes(&payload).unwrap();
    assert_eq!(decoded, marker);
    let backup = Some(MarkerText::from("/synthetic/日本語\0input.wav"));
    let marker = Marker { backup_file_name: backup.clone(), ..marker };
    let decoded = Marker::from_encoded_bytes(&marker.encoded_bytes().unwrap().unwrap()).unwrap();
    assert_eq!(decoded.backup_file_name, backup);
}

#[test]
fn marker_decode_uses_native_missing_field_and_scalar_conversions() {
    let payload = encode_marker_base64(br#"{"isProcessed":"YES","clipRepairCount":"-7","targetVolume":"-11.5","backupFileName":"","osType":null,"volumeUnits":"LUFS","unknown":42}"#).unwrap();
    let marker = Marker::from_encoded_bytes(&payload).unwrap();
    assert!(marker.is_processed);
    assert_eq!(marker.clip_repair_count, -7);
    assert_eq!(marker.target_volume, -11.5);
    assert_eq!(marker.backup_file_name, None);
    assert_eq!(marker.os_type, None);
    assert_eq!(marker.volume_units, Some("LUFS".into()));
    assert_eq!(marker.volume_analysis_version, 0);
    assert!(!marker.applied_pitch_correction);
    assert_eq!(marker.speed_adjustment.to_bits(), 0.0_f64.to_bits());
}

#[test]
fn present_invalid_marker_is_never_reported_as_absent_or_unprocessed() {
    for payload in [b"".as_slice(), b"==== \0", b"bm90LWpzb24="] {
        assert!(matches!(Marker::from_encoded_bytes(payload), Err(MarkerError::InvalidMarker(_))));
    }
    for json in [br#"{"isProcessed":null}"#.as_slice(), br#"{"backupFileName":123}"#, br#"{"targetVolume":[]}"#] {
        let payload = encode_marker_base64(json).unwrap();
        assert!(matches!(Marker::from_encoded_bytes(&payload), Err(MarkerError::InvalidField(_))));
    }
    let payload = encode_marker_base64(b"[]").unwrap();
    assert!(matches!(Marker::from_encoded_bytes(&payload), Err(MarkerError::InvalidMarker(_))));
}

#[test]
fn foundation_fixture_is_not_serde_serialization() {
    // Reference is a Foundation-only primitive on the pinned macOS host, not
    // a captured PN output. The whole dictionary's order is intentionally not
    // assumed to be universal across other OS versions or processes.
    let expected = include_str!("fixtures/foundation-marker.json");
    let marker = sample();
    let json = marker.json_bytes().unwrap().unwrap();
    assert_eq!(json, expected.as_bytes());
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 20);
    assert_eq!(value["isProcessed"], true);
    assert_eq!(value["appliedVolumeAdjustment"], true);
    assert_eq!(value["appliedCompression"], false);
    assert_eq!(value["volumeAnalysisVersion"], 4);
    assert_eq!(value["targetVolume"].as_f64(), Some(f64::from(-12.3_f32)));
    assert_eq!(value["inputVolume"].as_f64(), Some(f64::from(-15.2_f32)));
    assert_eq!(value["outputVolume"].as_f64(), Some(f64::from(-12.2_f32)));
    assert!(std::str::from_utf8(&json).unwrap().contains(r#"\/synthetic\/input.wav"#));
    let encoded = marker.encoded_bytes().unwrap().unwrap();
    assert_eq!(encoded, encode_marker_base64(&json).unwrap());
    assert_eq!(encoded.len(), 690);
    assert_eq!(encoded[72], b'\n');
    assert_eq!(encoded.last(), Some(&b'\n'));
}

#[test]
fn marker_construction_maps_controller_fields_without_rounding() {
    let mut marker = sample();
    assert_eq!(marker.volume_units, Some(MarkerText::from("LUFS")));
    assert_eq!(marker.warmth_preset, 0);
    let input = ProcessingMarker {
        software_version: None,
        backup_file_name: None,
        applied_clip_repair: true,
        applied_pitch_correction: true,
        applied_warmth: true,
        expansion_applied: 1,
        compression_applied: 2,
        volume_analysis_version: 3,
        target_volume: f32::from_bits(0x3f80_0001),
        input_volume: -0.0,
        output_volume: 6.75,
        clip_repair_count: -9,
        exciter_option: 2,
        input_pitch: -1.25,
        output_pitch: 3.5,
        speed_adjustment: 1.000_000_000_000_000_2,
    };
    marker = Marker::from_processing(input);
    assert_eq!(marker.os_type, Some(MarkerText::from("Mac")));
    assert_eq!(marker.volume_units, Some(MarkerText::from("dB")));
    assert_eq!(marker.warmth_preset, 2);
    assert!(marker.applied_clip_repair && marker.applied_pitch_correction && marker.applied_expansion);
    assert!(!marker.applied_compression);
    assert_eq!(marker.target_volume, f64::from(input.target_volume));
    assert_eq!(marker.input_volume.to_bits(), (-0.0_f64).to_bits());
    assert_eq!(marker.speed_adjustment, input.speed_adjustment);
    for (warmth, option, expected) in [(true, 0, 1), (true, 2, 2), (true, 1, 0), (false, 0, 0), (false, 2, 0)] {
        let m = Marker::from_processing(ProcessingMarker { applied_warmth: warmth, exciter_option: option, ..input });
        assert_eq!(m.warmth_preset, expected);
    }
    let data: serde_json::Value = serde_json::from_slice(&marker.json_bytes().unwrap().unwrap()).unwrap();
    assert_eq!(data["softwareVersion"], "");
    assert_eq!(data["backupFileName"], "");
    assert_eq!(data["clipRepairCount"], -9);
}

#[test]
fn nil_strings_and_embedded_nul_are_serialized_by_foundation() {
    let mut marker = sample();
    marker.os_type = None;
    marker.software_version = None;
    marker.backup_file_name = Some("a\0/\"\\\n日é😀".into());
    marker.volume_units = None;
    marker.input_pitch = -0.0;
    let json = marker.json_bytes().unwrap().unwrap();
    let text = std::str::from_utf8(&json).unwrap();
    assert!(text.contains(r#"a\u0000\/\"\\\n日é😀"#));
    assert!(text.contains(r#""inputPitch":-0"#));
    let data: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(data["osType"], "");
    assert_eq!(data["softwareVersion"], "");
    assert_eq!(data["volumeUnits"], "");
    assert_eq!(data["backupFileName"], "a\0/\"\\\n日é😀");
}

#[test]
fn foundation_utf16_preserves_code_units_and_rejects_unpaired_surrogates() {
    // Recorded separately against public Foundation, not the original app.
    let text = "music 😀\0/日本";
    let mut utf8 = sample();
    utf8.backup_file_name = Some(MarkerText::Utf8(text.to_owned()));
    let mut utf16 = sample();
    utf16.backup_file_name = Some(MarkerText::Utf16(text.encode_utf16().collect()));
    assert_eq!(utf16.json_bytes().unwrap(), utf8.json_bytes().unwrap());
    for units in [vec![0xd800], vec![0xdc00], vec![0xd800, 0x61]] {
        for field in 0..4 {
            let mut invalid = sample();
            match field {
                0 => invalid.os_type = Some(MarkerText::Utf16(units.clone())),
                1 => invalid.software_version = Some(MarkerText::Utf16(units.clone())),
                2 => invalid.backup_file_name = Some(MarkerText::Utf16(units.clone())),
                _ => invalid.volume_units = Some(MarkerText::Utf16(units.clone())),
            }
            assert!(invalid.json_bytes().unwrap().is_none());
            assert!(invalid.encoded_bytes().unwrap().is_none());
        }
    }
}

#[test]
fn nonfinite_values_produce_the_original_nil_outcome() {
    // The independently recorded Foundation-only edge probe shows all three
    // rejected by isValidJSONObject:, before dataWithJSONObject is called.
    for n in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for field in 0..6 {
            let mut marker = sample();
            match field {
                0 => marker.target_volume = n,
                1 => marker.input_volume = n,
                2 => marker.output_volume = n,
                3 => marker.input_pitch = n,
                4 => marker.output_pitch = n,
                _ => marker.speed_adjustment = n,
            }
            assert!(marker.json_bytes().unwrap().is_none());
            assert!(marker.encoded_bytes().unwrap().is_none());
        }
    }
}

#[test]
fn base64_wraps_only_complete_three_byte_groups_and_always_ends_with_lf() {
    assert!(matches!(encode_marker_base64(&[]), Err(MarkerError::UnobservedShortBase64 { length: 0 })));
    assert!(matches!(encode_marker_base64(&[0]), Err(MarkerError::UnobservedShortBase64 { length: 1 })));
    assert_eq!(encode_marker_base64(&[0, 1]).unwrap(), b"AAE=\n");
    assert_eq!(encode_marker_base64(b"abcdef").unwrap(), b"YWJjZGVm\n");
    let expected_53 = format!("{}=\n", "A".repeat(71));
    let expected_54 = format!("{}\n\n", "A".repeat(72));
    let expected_55 = format!("{}\nAA==\n", "A".repeat(72));
    assert_eq!(encode_marker_base64(&[0; 53]).unwrap(), expected_53.as_bytes());
    assert_eq!(encode_marker_base64(&[0; 54]).unwrap(), expected_54.as_bytes());
    assert_eq!(encode_marker_base64(&[0; 55]).unwrap(), expected_55.as_bytes());
}

#[test]
fn container_marker_identities_and_wav_declared_padding() {
    assert_eq!(ID3_GEOB_DESCRIPTION, "PlatinumNotes");
    assert_eq!(ID3_GEOB_MIME, "application/json");
    assert_eq!(MP4_FREEFORM_KEY, "----:com.mixedinkey.platinumnotes:data");
    assert_eq!(FLAC_XIPH_KEY, "PLATINUMNOTES");
    assert_eq!(wav_pltn_chunk(None).unwrap(), b"PLTN\x00\x00\x00\x00");
    assert_eq!(wav_pltn_chunk(Some(b"abc")).unwrap(), b"PLTN\x04\x00\x00\x00abc\x00");
    assert_eq!(wav_pltn_chunk(Some(b"abcd")).unwrap(), b"PLTN\x04\x00\x00\x00abcd");
}

fn input_chunk(id: &[u8; 4], data: &[u8], pad: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(id);
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
    if data.len() & 1 != 0 { bytes.push(pad); }
    bytes
}

fn input_wave(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    for chunk in chunks { bytes.extend_from_slice(chunk); }
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    bytes
}

#[test]
fn wav_rewrite_replaces_marker_normalizes_external_pads_and_discards_trailer() {
    // Parser/OnOther/OnLIST/OnPLTN/OnRIFFEnd static evidence, not a claimed
    // dynamic PN oracle. Short LISTs receive no callback; LIST contents are
    // otherwise copied as a single chunk.
    let mut input = input_wave(&[
        input_chunk(b"JUNK", &[1, 2, 3], 0xfa),
        input_chunk(b"PLTN", b"old\n", 0),
        input_chunk(b"LIST", b"BAD", 0xfc),
        input_chunk(b"LIST", b"INFOx", 0xfd),
        input_chunk(b"data", &[4, 5], 0),
        input_chunk(b"PLTN", b"old2", 0),
    ]);
    input.extend_from_slice(b"trailer outside RIFF");
    let mut output = Vec::new();
    rewrite_wav_marker(&mut std::io::Cursor::new(input), &mut output, Some(b"abc")).unwrap();
    let expected = [
        b"RIFF\x34\x00\x00\x00WAVE".as_slice(),
        b"JUNK\x03\x00\x00\x00\x01\x02\x03\x00",
        b"LIST\x05\x00\x00\x00INFOx\x00",
        b"data\x02\x00\x00\x00\x04\x05",
        b"PLTN\x04\x00\x00\x00abc\x00",
    ].concat();
    assert_eq!(output, expected);
}

#[test]
fn wav_nil_serialization_still_writes_an_empty_pltn() {
    let mut output = Vec::new();
    rewrite_wav_marker(&mut std::io::Cursor::new(input_wave(&[])), &mut output, None).unwrap();
    assert_eq!(output, b"RIFF\x0c\x00\x00\x00WAVEPLTN\x00\x00\x00\x00");
}

#[test]
fn wav_invalid_or_truncated_child_header_ends_the_parsed_prefix() {
    let valid = input_chunk(b"JUNK", b"ok", 0);
    let cases = [
        b"!BAD\x04\x00\x00\x00xxxxdata\x00\x00\x00\x00".to_vec(),
        b"data\x08\x00\x00\x00ab".to_vec(),
        b"data".to_vec(),
    ];
    for suffix in cases {
        let mut input = input_wave(&[valid.clone(), suffix]);
        // A parser scan uses the RIFF size, including the malformed suffix.
        let size = (input.len() - 8) as u32;
        input[4..8].copy_from_slice(&size.to_le_bytes());
        let mut output = Vec::new();
        rewrite_wav_marker(&mut std::io::Cursor::new(input), &mut output, Some(b"xy")).unwrap();
        let expected = [
            b"RIFF\x18\x00\x00\x00WAVE".as_slice(),
            b"JUNK\x02\x00\x00\x00ok",
            b"PLTN\x02\x00\x00\x00xy",
        ].concat();
        assert_eq!(output, expected);
    }
}

#[test]
fn wav_invalid_root_is_rejected_before_writing() {
    for bytes in [
        b"not wave".to_vec(),
        b"RIFX\x04\x00\x00\x00WAVE".to_vec(),
        b"RIFF\x05\x00\x00\x00WAVE".to_vec(),
        b"RIFF\x03\x00\x00\x00WAVE".to_vec(),
    ] {
        let mut output = b"untouched".to_vec();
        assert!(matches!(rewrite_wav_marker(&mut std::io::Cursor::new(bytes), &mut output, None), Err(MarkerError::InvalidWave(_))));
        assert_eq!(output, b"untouched");
    }
}
