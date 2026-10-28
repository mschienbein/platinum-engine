//! Static/numeric checks plus opt-in comparisons with the original native leaves.
//! The literals in `derived_tuning_cases` were computed with macOS libSystem
//! and vDSP from the documented assembly; they are not application captures.

use platinum_engine::pitch_detect::{PitchDetectionError, PitchDetector};

#[test]
fn derived_tuning_cases() {
    let detector = PitchDetector::new(44_100.0, 2).unwrap();
    let mut bins = [0.0f32; 256];
    assert_eq!(detector.tuning_hz_for_aggregate(&bins).to_bits(), 0x43dc_0000);
    bins[0] = 1.0;
    assert_eq!(detector.tuning_hz_for_aggregate(&bins).to_bits(), 0x43dc_0000);
    bins[0] = 0.0;
    bins[1] = 1.0;
    assert_eq!(detector.tuning_hz_for_aggregate(&bins).to_bits(), 0x43e0_46e6);
    bins[1] = 0.0;
    bins[2] = 1.0;
    assert_eq!(detector.tuning_hz_for_aggregate(&bins).to_bits(), 0x43d7_cdfc);
}

#[test]
fn silence_and_short_callbacks_keep_the_zero_aggregate() {
    for rate in [8_000.0, 11_025.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        let mut detector = PitchDetector::new(rate, 2).unwrap();
        for frames in [0usize, 1, 5, 16, 512, 16384, 511, 1, 0] {
            detector.process_interleaved(&vec![0.0; frames * 2]).unwrap();
        }
        assert_eq!(detector.tuning_hz().to_bits(), 0x43dc_0000);
        assert_eq!(detector.input_pitch_hz().to_bits(), 0x43dc_0000);
        assert_eq!(detector.pitch_classes(), [0.0; 12]);
    }
}

#[test]
fn callback_and_constructor_validation() {
    assert!(matches!(PitchDetector::new(0.0, 2), Err(PitchDetectionError::InvalidSampleRate)));
    assert!(matches!(PitchDetector::new(f32::NAN, 2), Err(PitchDetectionError::InvalidSampleRate)));
    assert!(matches!(PitchDetector::new(44_100.0, 0), Err(PitchDetectionError::InvalidChannelCount)));
    let mut detector = PitchDetector::new(48_000.0, 2).unwrap();
    assert_eq!(detector.process_interleaved(&[0.0]), Err(PitchDetectionError::IncompleteFrame));
    assert_eq!(detector.process_interleaved(&vec![0.0; 16385 * 2]),
               Err(PitchDetectionError::CallbackTooLarge));
}

#[test]
#[ignore = "requires freshly captured tuning-oracle stdout in PITCH_TUNING_ORACLE_OUTPUT"]
fn original_tuning_function_matches_synthetic_aggregates() {
    let path = std::env::var_os("PITCH_TUNING_ORACLE_OUTPUT").expect("oracle output path");
    let capture = std::fs::read_to_string(path).expect("read native oracle output");
    let detector = PitchDetector::new(44_100.0, 2).unwrap();
    let mut seen = 0;
    for line in capture.lines() {
        let mut fields = line.split_whitespace();
        let name = fields.next().expect("case name");
        let expected = u32::from_str_radix(fields.next().expect("float bits"), 16).unwrap();
        let mut bins = [0.0f32; 256];
        match name {
            "zero" => {},
            "bin0" => bins[0] = 1.0,
            "bin1" => bins[1] = 1.0,
            "bin2" => bins[2] = 1.0,
            "flat" => bins.fill(1.0),
            "nan" => bins[31] = f32::NAN,
            _ => panic!("unexpected oracle case: {name}"),
        }
        assert_eq!(detector.tuning_hz_for_aggregate(&bins).to_bits(), expected, "{name}");
        seen += 1;
    }
    assert_eq!(seen, 6, "oracle capture must contain every requested case");
}

#[test]
#[ignore = "requires PITCH_APP_REFERENCE_INPUT, the pinned generated WAV used for original captures A and B"]
fn original_app_tuning_matches_pinned_generated_input() {
    use platinum_engine::apple_codec::Reader;

    let path = std::env::var_os("PITCH_APP_REFERENCE_INPUT").expect("pinned generated input path");
    let digest = platinum_engine::mp3::file_sha256(&path).unwrap();
    assert_eq!(digest,
        "2de421ff4f080693c78a24947ad856d9db8f86c5909f42252187307aa820f884");
    let mut reader = Reader::open(path, None).unwrap();
    assert_eq!(reader.source_format().sample_rate, 48_000.0);
    assert_eq!(reader.source_format().channels_per_frame, 2);
    assert_eq!(reader.source_format().bits_per_channel, 16);
    let mut detector = PitchDetector::new(48_000.0, 2).unwrap();
    let mut block = [0.0f32; 1024];
    let mut frames = 0;
    loop {
        block.fill(0.0);
        let count = reader.read(&mut block).unwrap();
        detector.process_interleaved(&block).unwrap();
        frames += count;
        if count == 0 { break; }
    }
    reader.close().unwrap();
    assert_eq!(frames, 768_000);
    // Two byte-identical, uninstrumented original-app exports report this
    // value in inputPitch and outputPitch. This is a measured golden, not a
    // tolerance or an implementation correction fitted to this fixture.
    assert_eq!(detector.input_pitch_hz().to_bits(), 0x43dc_50b0);
}
