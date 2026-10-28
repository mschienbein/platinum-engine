//! Leaf regression cases and an opt-in comparison of two complete pitch-unit
//! capture sets. The candidate capture must be produced by an independently
//! implemented processor; this test does not supply a substitute processor.

use platinum_engine::pitch::{
    couple_stereo, fft_size_for_rate, interpolate::HermiteInterpolator, quantize_factor,
    uncouple_stereo, PitchMode, PitchProcessor,
};

#[test]
fn hermite_impulse_and_streaming_history() {
    let mut interpolation = HermiteInterpolator::new(2).unwrap();
    let mut left = [0.0; 4];
    let mut right = [0.0; 4];
    let count = interpolation.process_block(
        &[&[1.0, 0.0, 0.0, 0.0], &[0.0, -0.5, 0.0, 0.0]],
        &mut [&mut left, &mut right],
        1.0,
    ).unwrap();
    assert_eq!(count, 4);
    assert_eq!(left.map(f32::to_bits), [0.0f32, 0.0, 1.0, 0.0].map(f32::to_bits));
    assert_eq!(right.map(f32::to_bits), [0.0f32, 0.0, 0.0, -0.5].map(f32::to_bits));
}

#[test]
fn independently_recovered_factor_and_stereo_leaves() {
    let size = fft_size_for_rate(48_000.0).unwrap();
    let q = quantize_factor(size, 0.999, 1, 6.0).unwrap();
    assert_eq!((q.input_hop, q.output_hop), (512, 511));
    assert_eq!(q.factor.to_bits(), 0x3f7f_8000);
    let mut left = [0.5, -0.125];
    let mut right = [-0.25, 0.75];
    let expected_a = 0.5f32 * f32::from_bits(0x3f19_999a);
    let expected_b = -0.25f32 * f32::from_bits(0x3ecc_cccd);
    couple_stereo(&mut left, &mut right).unwrap();
    assert_eq!(left[0].to_bits(), (expected_a + expected_b).to_bits());
    assert_eq!(right[0].to_bits(), (expected_a - expected_b).to_bits());
    let expected_left = (left[0] + right[0]) / f32::from_bits(0x3f99_999a);
    let expected_right = (left[0] - right[0]) / f32::from_bits(0x3f4c_cccd);
    uncouple_stereo(&mut left, &mut right).unwrap();
    assert_eq!(left[0].to_bits(), expected_left.to_bits());
    assert_eq!(right[0].to_bits(), expected_right.to_bits());
}

/// Compare the independently recovered final interpolator with the native
/// `0x0e808` synthetic leaf oracle, including retained history, a zero-length
/// block, changing step, reset, and dissimilar stereo channels.
#[test]
#[ignore = "requires PITCH_HERMITE_ORACLE_OUTPUT captured by hermite-oracle.cpp"]
fn hermite_leaf_matches_native_component() {
    let path = std::env::var_os("PITCH_HERMITE_ORACLE_OUTPUT").expect("PITCH_HERMITE_ORACLE_OUTPUT");
    let events = read_events(std::path::Path::new(&path));
    let cases = [("unity", 2usize), ("half", 2), ("irregular", 5), ("changes-reset", 7), ("large", 2), ("stress", 4)];
    assert_eq!(events.len(), cases.iter().map(|(_, n)| n).sum::<usize>());
    let mut index = 0;
    for (name, count) in cases {
        let mut interpolation = HermiteInterpolator::new(2).unwrap();
        let mut offset = 0usize;
        for sequence in 0..count {
            let event = &events[index];
            index += 1;
            assert_eq!(event["case"].as_str(), Some(name));
            assert_eq!(event["sequence"].as_u64(), Some(sequence as u64));
            assert_eq!(event["offset"].as_u64(), Some(offset as u64));
            if event["reset"].as_bool().expect("reset flag") { interpolation.reset(); }
            let frames = event["frames"].as_u64().expect("input frames") as usize;
            let step = f32::from_bits(event["step_bits"].as_u64().expect("step bits") as u32);
            let input = |channel: usize| (offset..offset + frames).map(|frame| {
                if name == "stress" {
                    let values = [16_777_216.0f32, 1.0, -16_777_216.0, -3.0, 65_536.0, -65_536.0, 0.00390625, -0.0];
                    values[(frame * 7 + channel * 3) % values.len()]
                } else {
                    (((frame * 17 + channel * 29) % 101) as i32 - 50) as f32 / 128.0
                }
            }).collect::<Vec<_>>();
            let left = input(0);
            let right = input(1);
            let mut out_left = vec![0.0; 2 * frames + 8];
            let mut out_right = vec![0.0; 2 * frames + 8];
            let actual_count = interpolation.process_block(
                &[&left, &right], &mut [&mut out_left, &mut out_right], step
            ).unwrap();
            let expected_count = event["count"].as_u64().expect("output count") as usize;
            assert_eq!(actual_count, expected_count, "{name} block {sequence} count");
            for (field, samples) in [("left_bits", &out_left), ("right_bits", &out_right)] {
                let bits = event[field].as_array().expect("channel bits");
                assert_eq!(bits.len(), actual_count);
                for (frame, (sample, expected)) in samples.iter().zip(bits).enumerate() {
                    assert_eq!(u64::from(sample.to_bits()), expected.as_u64().unwrap(),
                        "{name} block {sequence} {field} frame {frame}");
                }
            }
            offset += frames;
        }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    #[link_name = "sin"]
    fn system_sin(value: f64) -> f64;
}
#[cfg(target_os = "linux")]
use platinum_engine::portable::math::sin as system_sin;

fn oracle_sample(signal: &str, rate: f64, source_frames: u64, frame: u64, channel: usize) -> f32 {
    if frame >= source_frames { return 0.0; }
    match signal {
        "impulse" => {
            if channel == 0 && frame == 0 { 0.75 }
            else if channel == 1 && frame == source_frames / 2 { -0.5 }
            else { 0.0 }
        }
        "tone" => {
            let angle = 6.283185307179586476925286766559f64 * 440.0 * frame as f64 / rate;
            let value = (0.25 * unsafe { system_sin(angle) }) as f32;
            value * if channel == 0 { 1.0 } else { 0.5 }
        }
        "pattern" => (((frame * 17 + channel as u64 * 29) % 101) as i32 - 50) as f32 / 128.0,
        "silence" => 0.0,
        _ => panic!("unexpected synthetic signal {signal}"),
    }
}

/// Exercise the Rust AU/Pro/Eff path against one independently captured
/// pitch-unit-oracle scenario. This compares actual generated samples, pulls,
/// timestamps, silence flags, Buffer Size and all output channel f32 bits.
#[test]
#[ignore = "requires PITCH_REFERENCE_PREFIX from the native pitch-unit oracle"]
fn synthesized_pitch_pipeline_matches_native_capture() {
    let prefix = std::env::var_os("PITCH_REFERENCE_PREFIX").expect("PITCH_REFERENCE_PREFIX");
    let events = read_events(&capture_path(&prefix, ".events.jsonl"));
    let frames = assert_complete_capture(&events);
    let config = &events[0];
    let rate = config["rate"].as_f64().expect("sample rate");
    let source_frames = config["input_frames"].as_u64().expect("source frames");
    let signal = config["signal"].as_str().expect("signal");
    let ratio = f32::from_bits(config["ratio_f32_bits"].as_u64().expect("ratio bits") as u32);
    let mode = match config["mode"].as_u64() {
        Some(0) => PitchMode::PreserveTempo,
        Some(1) => PitchMode::Turntable,
        _ => panic!("unsupported captured mode"),
    };
    let mut processor = PitchProcessor::new(rate as f32, 2, 512, mode, ratio).expect("selected pitch pipeline");
    let original = [
        std::fs::read(capture_path(&prefix, ".left.f32")).expect("original left PCM"),
        std::fs::read(capture_path(&prefix, ".right.f32")).expect("original right PCM"),
    ];
    for channel in &original { assert_eq!(channel.len(), frames * 4); }
    let mut expected_pulls = Vec::new();
    let mut pull_number = 0u64;
    let mut requested_frames = 0u64;
    for event in &events {
        match event["kind"].as_str() {
            Some("pull") => expected_pulls.push((
                event["number"].as_u64().unwrap(),
                event["start"].as_u64().unwrap(),
                event["frames"].as_u64().unwrap(),
            )),
            Some("event") => {
                let value = event["value"].as_f64().expect("event value");
                match event["event"].as_str().expect("event kind") {
                    "reset" => processor.reset(),
                    "ratio" => assert!(processor.set_ratio(value as f32).expect("supported event ratio")),
                    "mode" => assert!(processor.set_mode(if value == 0.0 { PitchMode::PreserveTempo } else { PitchMode::Turntable })),
                    other => panic!("unsupported event {other}"),
                }
            }
            Some("render") => {
                let offset = event["offset"].as_u64().unwrap() as usize;
                let count = event["frames"].as_u64().unwrap() as usize;
                let mut left = vec![0.0; count];
                let mut right = vec![0.0; count];
                let mut actual_pulls = Vec::new();
                let silent = processor.render(&mut [&mut left, &mut right], |timestamp, planes| {
                    assert!(timestamp.is_finite() && timestamp >= 0.0 && timestamp.fract() == 0.0);
                    let start = timestamp as u64;
                    let pulled = planes[0].len();
                    actual_pulls.push((pull_number, start, pulled as u64));
                    pull_number += 1;
                    requested_frames += pulled as u64;
                    for (channel, plane) in planes.iter_mut().enumerate() {
                        for (index, sample) in plane.iter_mut().enumerate() {
                            *sample = oracle_sample(signal, rate, source_frames, start + index as u64, channel);
                        }
                    }
                    Ok(pulled)
                }).expect("render selected pitch callback");
                assert_eq!(actual_pulls, expected_pulls, "pull sequence before render {offset}");
                expected_pulls.clear();
                assert_eq!(event["pulls"].as_u64(), Some(pull_number));
                assert_eq!(event["requested_frames"].as_u64(), Some(requested_frames));
                assert_eq!(event["flags"].as_u64(), Some(if silent { 0x10 } else { 0 }));
                let pending = event["pending"].as_f64().expect("finite Buffer Size") as f32;
                assert_eq!(processor.buffer_size().to_bits(), pending.to_bits(), "Buffer Size after render {offset}");
                for (channel, output) in [&left, &right].iter().enumerate() {
                    for (index, actual) in output.iter().enumerate() {
                        let byte = (offset + index) * 4;
                        let expected = u32::from_ne_bytes(original[channel][byte..byte + 4].try_into().unwrap());
                        assert_eq!(actual.to_bits(), expected, "channel {channel} frame {}", offset + index);
                    }
                }
            }
            Some("config" | "properties" | "end") => {}
            other => panic!("unsupported capture record {other:?}"),
        }
    }
    assert!(expected_pulls.is_empty(), "unmatched pulls at capture end");
}

fn capture_path(prefix: &std::ffi::OsStr, suffix: &str) -> std::path::PathBuf {
    let mut name = prefix.to_os_string();
    name.push(suffix);
    name.into()
}

fn read_events(path: &std::path::Path) -> Vec<serde_json::Value> {
    let contents = std::fs::read_to_string(path).expect("read capture event log");
    contents.lines().enumerate().map(|(index, line)| {
        serde_json::from_str(line).unwrap_or_else(|error| panic!("invalid JSON event {} in {}: {error}", index + 1, path.display()))
    }).collect()
}

fn assert_complete_capture(events: &[serde_json::Value]) -> usize {
    let config = events.first().expect("capture config");
    assert_eq!(config["kind"], "config");
    let expected = config["output_frames"].as_u64().expect("config output_frames") as usize;
    assert!(expected > 0);
    assert!(matches!(config["signal"].as_str(), Some("impulse" | "tone" | "pattern" | "silence")), "capture must use generated signals");
    let end = events.last().expect("capture end");
    assert_eq!(end["kind"], "end");
    assert_eq!(end["status"], 0, "failed render cannot be a reference");
    assert_eq!(end["uninitialize_status"], 0);
    assert_eq!(end["dispose_status"], 0);
    assert_eq!(end["rendered_frames"].as_u64(), Some(expected as u64));
    let mut rendered = 0u64;
    for event in events {
        if event["kind"] == "render" {
            assert_eq!(event["offset"].as_u64(), Some(rendered));
            assert_eq!(event["pending_status"], 0);
            rendered += event["frames"].as_u64().expect("render frames");
        } else if event["kind"] == "event" {
            assert_eq!(event["status"], 0);
        }
    }
    assert_eq!(rendered, expected as u64);
    expected
}

/// Compare exact f32 channel bits, pull offsets/counts, render partitions,
/// flags, mode/ratio/reset events and Buffer Size readback for one synthetic
/// oracle scenario. Run for each captured scenario by setting both prefixes.
/// Candidate logs must use the same JSONL schema as pitch-unit-oracle.cpp.
#[test]
#[ignore = "requires independent native and reconstructed pitch-unit capture prefixes"]
fn complete_pitch_capture_matches_original_bit_for_bit() {
    use std::os::unix::fs::MetadataExt;
    let reference = std::env::var_os("PITCH_REFERENCE_PREFIX").expect("PITCH_REFERENCE_PREFIX");
    let candidate = std::env::var_os("PITCH_CANDIDATE_PREFIX").expect("PITCH_CANDIDATE_PREFIX");
    let reference_events_path = capture_path(&reference, ".events.jsonl");
    let candidate_events_path = capture_path(&candidate, ".events.jsonl");
    let ref_metadata = std::fs::metadata(&reference_events_path).expect("original event metadata");
    let candidate_metadata = std::fs::metadata(&candidate_events_path).expect("candidate event metadata");
    assert_ne!((ref_metadata.dev(), ref_metadata.ino()), (candidate_metadata.dev(), candidate_metadata.ino()), "reference and candidate event files must be independent");
    let ref_events = read_events(&reference_events_path);
    let candidate_events = read_events(&candidate_events_path);
    let frames = assert_complete_capture(&ref_events);
    assert_complete_capture(&candidate_events);
    assert_eq!(ref_events.len(), candidate_events.len(), "event count");
    for (index, (expected, actual)) in ref_events.iter().zip(&candidate_events).enumerate() {
        assert_eq!(expected, actual, "event {index}");
    }
    for suffix in [".left.f32", ".right.f32"] {
        let original_path = capture_path(&reference, suffix);
        let candidate_path = capture_path(&candidate, suffix);
        let original_meta = std::fs::metadata(&original_path).expect("original audio metadata");
        let candidate_meta = std::fs::metadata(&candidate_path).expect("candidate audio metadata");
        assert_ne!((original_meta.dev(), original_meta.ino()), (candidate_meta.dev(), candidate_meta.ino()), "reference and candidate must be independent files");
        let original = std::fs::read(&original_path).expect("original f32 capture");
        let reconstructed = std::fs::read(&candidate_path).expect("candidate f32 capture");
        assert_eq!(original.len(), frames * 4, "original {suffix} byte count");
        assert_eq!(reconstructed.len(), frames * 4, "candidate {suffix} byte count");
        for (frame, (expected, actual)) in original.chunks_exact(4).zip(reconstructed.chunks_exact(4)).enumerate() {
            let expected = u32::from_ne_bytes(expected.try_into().unwrap());
            let actual = u32::from_ne_bytes(actual.try_into().unwrap());
            assert_eq!(expected, actual, "{suffix} frame {frame}: expected {expected:#010x}, got {actual:#010x}");
        }
    }
}
