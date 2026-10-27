//! Consumer-facing clip vectors. The delay baseline was measured on the original
//! Audio Unit; repaired-waveform and event expectations are statically derived
//! from the ARM64 instructions, not yet independent native golden recordings.

use platinum_engine::clip::{ClipError, ClipParameters, ClipRepair};

const RATE: f64 = 48_000.0;
const DELAY: usize = 2560;
const THRESHOLD: f32 = 99.7;

fn parameters(headroom_db: f32) -> ClipParameters {
    ClipParameters { headroom_db, threshold_percent: THRESHOLD }
}

fn render(input: &[f32], params: ClipParameters, schedule: &[usize]) -> (Vec<f32>, ClipRepair) {
    let mut clip = ClipRepair::new(RATE, params).unwrap();
    let mut output = vec![0.0; input.len()];
    let mut offset = 0;
    let mut block = 0;
    while offset < input.len() {
        let end = (offset + schedule[block % schedule.len()]).min(input.len());
        clip.process(&input[offset..end], &mut output[offset..end]).unwrap();
        offset = end;
        block += 1;
    }
    (output, clip)
}

fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|v| v.to_bits()).collect()
}

#[test]
fn measured_zero_headroom_delay_matches_fixed_and_irregular_blocks() {
    // Measured native trace: 8192 frames, 2560 zeros, 48 kHz.
    for channel in 0..2_usize {
        let mut input = vec![0.0; 8192 + DELAY];
        for (frame, value) in input[..8192].iter_mut().enumerate() {
            *value = (((frame * 17 + channel * 29) % 101) as i32 - 50) as f32 / 128.0;
        }
        let mut expected = vec![0.0; input.len()];
        expected[DELAY..].copy_from_slice(&input[..8192]);
        let fixed = render(&input, parameters(0.0), &[512]);
        let irregular = render(&input, parameters(0.0), &[17, 31, 257, 512, 3]);
        assert_eq!(bits(&fixed.0), bits(&expected));
        assert_eq!(bits(&irregular.0), bits(&expected));
        assert_eq!(fixed.1.latency_frames(), DELAY);
        assert_eq!(fixed.1.event_count(), 0);
        assert!(fixed.1.event_times().is_empty());
        assert!(irregular.1.event_positions().is_empty());
    }
}

#[test]
fn measured_default_gain_has_the_recorded_naive_reference_differences() {
    // The native oracle's comparator uses double pow and double multiplication,
    // then casts once to f32; those mismatch statistics are measured, not a
    // claim that the naive comparator is itself the reference DSP.
    let mut input = vec![0.0; 8192 + DELAY];
    for (frame, value) in input[..8192].iter_mut().enumerate() {
        *value = (((frame * 17) % 101) as i32 - 50) as f32 / 128.0;
    }
    let (output, clip) = render(&input, parameters(1.5), &[512]);
    let naive_gain = 10.0_f64.powf(-1.5_f64 / 20.0_f64);
    let mut mismatches = 0;
    let mut first = None;
    let mut maximum_error = 0.0_f64;
    for (frame, &actual) in output.iter().enumerate() {
        let source = if frame < DELAY { 0.0 } else { input[frame - DELAY] };
        let expected = (f64::from(source) * naive_gain) as f32;
        if actual.to_bits() != expected.to_bits() {
            mismatches += 1;
            first.get_or_insert(frame);
        }
        maximum_error = maximum_error.max((f64::from(actual) - f64::from(expected)).abs());
    }
    assert_eq!(mismatches, 486);
    assert_eq!(first, Some(2574));
    assert_eq!(maximum_error, 1.4901161193847656e-8);
    assert_eq!(clip.event_count(), 0);
}

#[test]
fn bypass_copies_negative_zero_and_nan_payloads_without_detection() {
    let mut input = vec![0.0; DELAY + 5];
    input[..5].copy_from_slice(&[f32::from_bits(0x8000_0000), f32::from_bits(0x7fc0_1234), 1.0, 1.0, 1.0]);
    let (output, clip) = render(&input, parameters(0.0), &[3, 1, 257]);
    assert_eq!(bits(&output[DELAY..]), bits(&input[..5]));
    assert_eq!(clip.event_count(), 0);
}

#[test]
fn invalid_configuration_and_buffers_do_not_advance_the_stream() {
    assert!(matches!(ClipRepair::new(0.0, parameters(1.5)), Err(ClipError::InvalidSampleRate)));
    assert!(matches!(ClipRepair::new(f64::NAN, parameters(1.5)), Err(ClipError::InvalidSampleRate)));
    assert!(matches!(ClipRepair::new(RATE, parameters(f32::INFINITY)), Err(ClipError::InvalidHeadroom)));
    assert!(matches!(ClipRepair::new(RATE, ClipParameters { headroom_db: 1.5, threshold_percent: f32::NAN }), Err(ClipError::InvalidThreshold)));
    let mut clip = ClipRepair::new(RATE, parameters(0.0)).unwrap();
    let mut too_short = [23.0_f32; 1];
    assert_eq!(clip.process(&[1.0, 2.0], &mut too_short), Err(ClipError::BufferLengthMismatch { input: 2, output: 1 }));
    assert_eq!(too_short, [23.0]);
    clip.process(&[], &mut []).unwrap();
    let mut output = vec![0.0; DELAY + 1];
    let mut input = vec![0.0; DELAY + 1];
    input[0] = -0.25;
    clip.process(&input, &mut output).unwrap();
    assert_eq!(output[DELAY].to_bits(), (-0.25_f32).to_bits());
}

#[test]
fn three_sample_plateaus_use_asymmetric_rails_and_report_events() {
    for (sample, rail_bits) in [(1.0, 0x3f7f_3765), (-1.0, 0xbf7f_3965)] {
        let p = 3000;
        let mut input = vec![0.0; 8192];
        input[0] = 0.5; // Recover the block's float32 gain without a second pow implementation.
        input[p..p + 3].fill(sample);
        let (output, clip) = render(&input, parameters(1.5), &[17, 31, 257, 512, 3]);
        let gain = output[DELAY] * 2.0;
        let scaled_rail = f32::from_bits(rail_bits) * gain + 0.0_f32;
        assert_eq!(bits(&output[p + DELAY..p + DELAY + 3]), vec![scaled_rail.to_bits(); 3]);
        assert_eq!(clip.event_count(), 1);
        assert_eq!(clip.event_times(), &[3001.0 / RATE]);
        assert_eq!(clip.event_positions(), vec![3001]);
    }
}

#[test]
fn one_and_two_sample_plateaus_do_not_create_events() {
    for sample in [1.0, -1.0] {
        for length in [1, 2] {
            let mut input = vec![0.0; 8192];
            input[3000..3000 + length].fill(sample);
            let (output, clip) = render(&input, parameters(1.5), &[512]);
            assert_eq!(clip.event_count(), 0);
            assert!(clip.event_times().is_empty());
            assert_ne!(output[3000 + DELAY], 0.0);
        }
    }
}

#[test]
fn strict_and_loose_rail_equality_uses_ordered_crossings() {
    for (sign, strict, adjacent, loose) in [
        (1.0, 0x3f7f_3765, 0x3f7f_3764, 0x3f66_6280),
        (-1.0, 0xbf7f_3965, 0xbf7f_3964, 0xbf66_6480),
    ] {
        for (value, expected) in [(strict, 1), (adjacent, 0)] {
            let mut input = vec![0.0; 8192];
            input[3000..3003].fill(f32::from_bits(value));
            let (_, clip) = render(&input, parameters(1.5), &[512]);
            assert_eq!(clip.event_count(), expected);
        }
        let mut input = vec![0.0; 8192];
        input[3000..3003].fill(sign);
        input[3003] = f32::from_bits(loose);
        let (_, clip) = render(&input, parameters(1.5), &[257, 3, 512]);
        assert_eq!(clip.event_count(), 1);
        assert_eq!(clip.event_times(), &[3001.0 / RATE]);
    }
}

#[test]
fn lookahead_boundary_and_position_round_trip_are_preserved() {
    for sample in [1.0, -1.0] {
        for (length, expected) in [(1023, 1), (1024, 0)] {
            let mut input = vec![0.0; 8192];
            input[3000..3000 + length].fill(sample);
            let (_, clip) = render(&input, parameters(1.5), &[512]);
            assert_eq!(clip.event_count(), expected);
            if length == 1023 {
                assert_eq!(clip.event_times(), &[3511.0 / RATE]);
                assert_eq!(clip.event_positions(), vec![3510]);
            }
        }
    }
}

#[test]
fn holdoff_and_ring_wrap_survive_block_partitions() {
    let mut input = vec![0.0; 9000];
    input[3000..3003].fill(1.0);
    input[3003..3006].fill(-1.0);
    input[4094..4098].fill(1.0);
    let fixed = render(&input, parameters(1.5), &[512]);
    let irregular = render(&input, parameters(1.5), &[17, 31, 257, 512, 3]);
    assert_eq!(bits(&fixed.0), bits(&irregular.0));
    assert_eq!(fixed.1.event_count(), 2);
    assert_eq!(fixed.1.event_times(), &[3001.0 / RATE, 4096.0 / RATE]);
    assert_eq!(irregular.1.event_times(), fixed.1.event_times());
}

#[test]
fn channels_are_independent_and_simultaneous_events_are_not_deduplicated() {
    let mut input = vec![0.0; 8192];
    input[3000..3003].fill(1.0);
    let (_, left) = render(&input, parameters(1.5), &[512]);
    let (_, right) = render(&input, parameters(1.5), &[17, 31, 257, 512, 3]);
    let mut times = left.event_times().to_vec();
    times.extend_from_slice(right.event_times());
    times.sort_by(f64::total_cmp);
    assert_eq!(times, vec![3001.0 / RATE, 3001.0 / RATE]);
    assert_eq!(left.event_positions(), vec![3001]);
    assert_eq!(right.event_positions(), vec![3001]);
}

#[test]
#[ignore = "requires the separately executed original Audio Unit oracle and CLIP_ORACLE_DIR"]
fn native_oracle_sample_and_property_bits() {
    use std::fs;
    use std::path::PathBuf;

    let dir = PathBuf::from(std::env::var_os("CLIP_ORACLE_DIR").expect("set CLIP_ORACLE_DIR to the fresh oracle output directory"));
    let mut files: Vec<_> = fs::read_dir(&dir).expect("read oracle output directory")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 32, "the native oracle must finish all 32 fixtures");

    for path in files {
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let name = json["case"].as_str().unwrap();
        let rate = json["sample_rate"].as_f64().unwrap();
        let params = ClipParameters {
            headroom_db: json["headroom"].as_f64().unwrap() as f32,
            threshold_percent: json["threshold"].as_f64().unwrap() as f32,
        };
        let schedule: Vec<usize> = json["schedule"].as_array().unwrap().iter()
            .map(|value| value.as_u64().unwrap() as usize).collect();
        let inputs = json["input_bits"].as_array().unwrap();
        let outputs = json["output_bits"].as_array().unwrap();
        assert_eq!(inputs.len(), outputs.len(), "{name}: channel count");
        let mut times = Vec::new();
        let mut positions = Vec::new();
        let mut count = 0_u32;
        for (channel, (input, expected)) in inputs.iter().zip(outputs).enumerate() {
            let samples: Vec<f32> = input.as_array().unwrap().iter()
                .map(|value| f32::from_bits(value.as_u64().unwrap() as u32)).collect();
            let expected = expected.as_array().unwrap();
            assert_eq!(samples.len(), expected.len(), "{name}: channel {channel} frame count");
            let mut clip = ClipRepair::new(rate, params).unwrap();
            let mut output = vec![0.0; samples.len()];
            let mut offset = 0;
            let mut block = 0;
            while offset < samples.len() {
                let end = (offset + schedule[block % schedule.len()]).min(samples.len());
                clip.process(&samples[offset..end], &mut output[offset..end]).unwrap();
                offset = end;
                block += 1;
            }
            for (frame, (actual, reference)) in output.iter().zip(expected).enumerate() {
                assert_eq!(actual.to_bits(), reference.as_u64().unwrap() as u32,
                    "{name}: channel {channel}, frame {frame}");
            }
            count = count.wrapping_add(clip.event_count());
            times.extend_from_slice(clip.event_times());
            positions.extend(clip.event_positions());
        }
        times.sort_by(f64::total_cmp);
        positions.sort_unstable();
        let time_bits: Vec<u64> = times.iter().map(|value| value.to_bits()).collect();
        let expected_times: Vec<u64> = json["time_bits"].as_array().unwrap().iter()
            .map(|value| value.as_u64().unwrap()).collect();
        let expected_positions: Vec<u64> = json["positions"].as_array().unwrap().iter()
            .map(|value| value.as_u64().unwrap()).collect();
        assert_eq!(count as u64, json["event_count"].as_u64().unwrap(), "{name}: event count");
        assert_eq!(time_bits, expected_times, "{name}: event time bits");
        assert_eq!(positions, expected_positions, "{name}: event positions");
    }
}
