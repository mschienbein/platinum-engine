//! Static wrapper-derived phase tests and an explicitly opt-in native oracle.
//! Native fixtures come from a separately supplied private phase oracle;
//! they are not bundled goldens and do not establish complete-file parity.

use platinum_engine::phase::{HOP, OVERLAP, PhaseCorrection, WINDOW};
use platinum_engine::portable::arm_scalar::ArmFloat;

const PARAMETERS: [f32; 4] = [0.2, 0.5, 0.8, 1.0];
const FRAMES: usize = WINDOW + 2 * HOP + 257;

fn flags(mask: u32) -> [bool; 5] {
    std::array::from_fn(|index| mask & (1 << index) != 0)
}

fn signal(count: usize, stage: usize, channel: usize) -> Vec<f32> {
    (0..count)
        .map(|i| (((i + stage * 13 + channel * 29) % 83) as f32 - 41.0) / 128.0)
        .collect()
}

fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|sample| sample.to_bits()).collect()
}

fn process_chunks(phase: &mut PhaseCorrection, channel: usize, samples: &mut [f32]) {
    for block in samples.chunks_mut(257) {
        phase.process_channel(channel, block);
    }
}

// A nonstreaming derivation of the native 0x14934/0x14dc0 window semantics.
// It intentionally emits only the first HOP samples of each completed window.
fn reference_stream(input: &[f32], coefficients: &[[f64; 6]]) -> Vec<f32> {
    let mut output = vec![0.0; input.len()];
    for start in (0..input.len()).step_by(HOP) {
        if start + WINDOW > input.len() {
            break;
        }
        let mut window = input[start..start + WINDOW].to_vec();
        for coeff in coefficients {
            let (mut z0, mut z1) = (0.0_f64, 0.0_f64);
            for sample in window.iter_mut().rev() {
                let x = f64::from(*sample);
                let y = coeff[3].arm_mul_add(x, z0);
                let term0 = y * -coeff[1];
                z0 = z1 + coeff[4].arm_mul_add(x, term0);
                let term1 = y * -coeff[2];
                z1 = coeff[5].arm_mul_add(x, term1);
                *sample = y as f32;
            }
        }
        let out_start = WINDOW + start;
        let n = HOP.min(output.len().saturating_sub(out_start));
        output[out_start..out_start + n].copy_from_slice(&window[..n]);
    }
    output
}

#[test]
fn bypass_preserves_bits_and_has_no_phase_latency() {
    for (rate, bands) in [
        (48_000.0, [false; 5]),
        (48_000.0, [false, true, false, false, false]),
        (0.0, [true; 5]),
        (-0.0, [true; 5]),
        (-48_000.0, [true; 5]),
    ] {
        let mut phase = PhaseCorrection::new(rate, bands, PARAMETERS, 0);
        let mut samples = [f32::from_bits(0x8000_0000), f32::from_bits(0x7fc0_1234), 0.25, -0.75];
        let before = bits(&samples);
        phase.process_channel(0, &mut samples);
        assert_eq!(bits(&samples), before);
        assert_eq!(phase.channel_count(), 1);
        assert_eq!(phase.section_count(), 0);
        assert_eq!(phase.latency_frames(), 0);
    }
}

#[test]
fn every_band_mask_selects_preceding_enabled_parameter() {
    for mask in 0..32 {
        let selected = flags(mask);
        let phase = PhaseCorrection::new(48_000.0, selected, PARAMETERS, 1);
        let enabled: Vec<usize> = (0..5).filter(|&index| selected[index]).collect();
        assert_eq!(phase.section_count(), enabled.len().saturating_sub(1), "mask {mask}");
        for (section, pair) in enabled.windows(2).enumerate() {
            let mut pair_flags = [false; 5];
            pair_flags[pair[0]] = true;
            pair_flags[pair[1]] = true;
            let single = PhaseCorrection::new(48_000.0, pair_flags, PARAMETERS, 1);
            assert_eq!(
                phase.section_coefficients(section).unwrap().map(f64::to_bits),
                single.section_coefficients(0).unwrap().map(f64::to_bits),
                "mask {mask}, section {section}"
            );
        }
        assert!(phase.section_coefficients(phase.section_count()).is_none());
    }
}

#[test]
fn impulse_appears_after_full_window_with_native_first_coefficient() {
    let mut phase = PhaseCorrection::new(48_000.0, flags(3), PARAMETERS, 1);
    let a2 = phase.section_coefficients(0).unwrap()[3] as f32;
    let mut samples = vec![0.0; WINDOW + 1];
    samples[0] = 1.0;
    phase.process_channel(0, &mut samples);
    assert_eq!(phase.latency_frames(), WINDOW);
    assert!(samples[..WINDOW].iter().all(|&sample| sample.to_bits() == 0));
    assert_eq!(samples[WINDOW].to_bits(), a2.to_bits());
}

#[test]
fn overlap_reverse_order_rounding_and_channel_partitioning() {
    assert_eq!((OVERLAP, HOP, WINDOW), (4096, 16384, 20480));
    let input = signal(FRAMES, 0, 0);
    let mut phase = PhaseCorrection::new(48_000.0, flags(0b10111), PARAMETERS, 2);
    let coefficients: Vec<[f64; 6]> = (0..phase.section_count())
        .map(|index| phase.section_coefficients(index).unwrap())
        .collect();
    let expected = reference_stream(&input, &coefficients);
    let mut whole = input.clone();
    let mut partitioned = input;
    phase.process_channel(0, &mut whole);
    process_chunks(&mut phase, 1, &mut partitioned);
    assert_eq!(bits(&whole), bits(&expected));
    assert_eq!(bits(&partitioned), bits(&expected));
}

#[test]
fn reset_triggers_and_noop_flag_write_follow_wrapper_state() {
    let mut a = PhaseCorrection::new(48_000.0, flags(15), PARAMETERS, 2);
    let mut b = PhaseCorrection::new(48_000.0, flags(15), PARAMETERS, 2);
    let mut first = signal(WINDOW + 17, 0, 0);
    let mut other = first.clone();
    a.process_channel(0, &mut first);
    b.process_channel(0, &mut other);
    assert!(!a.write_band(3, true));
    assert!(!a.write_band(5, true));
    assert!(!a.write_parameter(4, 0.5));
    assert!(!a.is_dirty());
    assert!(!a.prepare(48_000.0, 2));
    let mut next = signal(1000, 1, 0);
    let mut other_next = next.clone();
    a.process_channel(0, &mut next);
    b.process_channel(0, &mut other_next);
    assert_eq!(bits(&next), bits(&other_next));

    // Identical writes still discard queued audio, as do a successful rate
    // configuration and a preset load, even if their values are unchanged.
    assert!(a.write_parameter(0, PARAMETERS[0]));
    assert!(a.is_dirty());
    assert!(a.prepare(48_000.0, 2));
    let mut after = vec![0.0; WINDOW];
    a.process_channel(0, &mut after);
    assert!(after.iter().all(|&sample| sample.to_bits() == 0));
    a.sample_rate_reconfigured();
    assert!(a.prepare(48_000.0, 2));
    a.preset_loaded(PARAMETERS);
    assert!(a.prepare(48_000.0, 2));
    assert!(a.write_band(3, false));
    assert!(a.prepare(48_000.0, 2));
    assert_eq!(a.section_count(), 2);
    assert!(a.prepare(48_000.0, 1));
    assert_eq!(a.channel_count(), 1);
    assert!(a.prepare(48_000.0, 0) == false); // both counts clamp to one
    assert!(a.prepare(44_100.5, 1));
}

#[test]
fn rate_storage_and_unordered_branches_are_explicit() {
    let rate_a = 44_100.0001_f64 as f32;
    let rate_b = 44_100.0002_f64 as f32;
    assert_eq!(rate_a.to_bits(), rate_b.to_bits());
    let mut phase = PhaseCorrection::new(rate_a, flags(3), PARAMETERS, 1);
    assert!(!phase.prepare(rate_b, 1));
    assert!(phase.prepare(44_100.5, 1));
    phase.reconfigure(f32::NAN, flags(3), PARAMETERS);
    assert_eq!(phase.section_count(), 1); // FCMP/B.LS admits unordered
    assert_eq!(phase.latency_frames(), WINDOW);
    assert!(phase.prepare(f32::NAN, 1)); // FCMP/B.NE considers NaNs unequal
    phase.reconfigure(f32::INFINITY, flags(3), PARAMETERS);
    assert_eq!(phase.section_count(), 1);
    phase.reconfigure(0.0, flags(3), PARAMETERS);
    assert_eq!(phase.latency_frames(), 0);
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> u32 {
    let value = u32::from_le_bytes(bytes[*offset..*offset + 4].try_into().unwrap());
    *offset += 4;
    value
}

fn read_u64(bytes: &[u8], offset: &mut usize) -> u64 {
    let value = u64::from_le_bytes(bytes[*offset..*offset + 8].try_into().unwrap());
    *offset += 8;
    value
}

fn compare_oracle(name: &str, phase: &PhaseCorrection, getter_mask: u32, samples: &[f32]) {
    let directory = std::env::var_os("PLATINUM_PHASE_ORACLE_DIR")
        .expect("set PLATINUM_PHASE_ORACLE_DIR to a freshly generated native fixture directory");
    let bytes = std::fs::read(std::path::Path::new(&directory).join(format!("{name}.bin")))
        .unwrap_or_else(|error| panic!("cannot read native oracle {name}: {error}"));
    assert_eq!(&bytes[..4], b"PH01", "{name}");
    let mut offset = 4;
    assert_eq!(read_u32(&bytes, &mut offset) as usize, phase.section_count(), "{name} sections");
    assert_eq!(read_u32(&bytes, &mut offset), getter_mask, "{name} getter mask");
    assert_eq!(read_u32(&bytes, &mut offset) as usize, phase.latency_frames(), "{name} latency");
    assert_eq!(read_u32(&bytes, &mut offset) as usize, samples.len(), "{name} frames");
    for section in 0..phase.section_count() {
        for (coefficient, expected) in phase.section_coefficients(section).unwrap().into_iter().enumerate() {
            assert_eq!(read_u64(&bytes, &mut offset), expected.to_bits(), "{name} section {section} coefficient {coefficient}");
        }
    }
    assert_eq!(bytes.len(), offset + samples.len() * 4, "{name} length");
    for (frame, sample) in samples.iter().enumerate() {
        assert_eq!(read_u32(&bytes, &mut offset), sample.to_bits(), "{name} frame {frame}");
    }
}

fn oracle_case(name: &str, rate: f32, bands: [bool; 5], parameters: [f32; 4], getter_mask: u32) {
    let mut phase = PhaseCorrection::new(rate, bands, parameters, 1);
    let mut samples = signal(FRAMES, 0, 0);
    process_chunks(&mut phase, 0, &mut samples);
    compare_oracle(name, &phase, getter_mask, &samples);
}

/// Explicitly opted-in integration with the locally generated proprietary
/// reference data. Never calls or links the original from the Rust engine.
#[test]
#[ignore = "requires a freshly generated local KiloheartsUnit phase oracle"]
fn native_wrapper_coefficients_and_pcm_match() {
    for mask in 0..32 {
        oracle_case(&format!("flags-{mask}"), 48_000.0, flags(mask), PARAMETERS, 0);
    }
    for (name, rate) in [
        ("rate-zero", 0.0),
        ("rate-negative", -48_000.0),
        ("rate-fractional", 44_100.5),
        ("rate-infinity", f32::INFINITY),
        ("rate-nan", f32::from_bits(0x7fc0_1234)),
    ] {
        oracle_case(name, rate, [true; 5], PARAMETERS, 0);
    }
    oracle_case(
        "fallback", 48_000.0, [true; 5],
        [0.2, -0.0, f32::from_bits(0x7fc0_1234), 0.9], 0b1001,
    );
    oracle_case(
        "rate-zero-fallback", 0.0, [true; 5],
        [0.2, -0.0, f32::from_bits(0x7fc0_1234), 0.9], 0b1001,
    );
}

#[test]
#[ignore = "requires a freshly generated local KiloheartsUnit phase oracle"]
fn native_wrapper_invalidation_and_channel_windows_match() {
    let mut phase = PhaseCorrection::new(48_000.0, flags(15), PARAMETERS, 2);
    let mut output = Vec::new();
    for stage in 0..8 {
        match stage {
            0 => {}
            1 => {
                assert!(!phase.write_band(3, true));
                assert!(!phase.prepare(48_000.0, 2));
            }
            2 => {
                assert!(phase.write_band(3, false));
                assert!(phase.prepare(48_000.0, 2));
            }
            3 => assert!(phase.prepare(44_100.5, 2)),
            4 => assert!(phase.prepare(44_100.5, 1)),
            5 => assert!(phase.prepare(44_100.5, 2)),
            6 => {
                assert!(phase.write_parameter(1, PARAMETERS[1]));
                assert!(phase.prepare(44_100.5, 2));
            }
            7 => {
                phase.preset_loaded(PARAMETERS);
                assert!(phase.prepare(44_100.5, 2));
            }
            _ => unreachable!(),
        }
        let frames = match stage { 0 => 35_000, 1 => 2_049, _ => 22_000 };
        for channel in 0..if stage == 4 { 1 } else { 2 } {
            let mut block = signal(frames, stage, channel);
            process_chunks(&mut phase, channel, &mut block);
            output.extend_from_slice(&block);
        }
    }
    compare_oracle("events", &phase, 0, &output);
}

#[test]
fn every_rebuild_cause_discards_an_already_computed_window() {
    for event in 0..7 {
        let mut phase = PhaseCorrection::new(48_000.0, flags(15), PARAMETERS, 2);
        let mut filled = vec![0.5; WINDOW];
        phase.process_channel(0, &mut filled);
        match event {
            0 => { assert!(phase.write_parameter(1, PARAMETERS[1])); }
            1 => { assert!(phase.write_band(3, false)); }
            2 => phase.preset_loaded(PARAMETERS),
            3 => phase.sample_rate_reconfigured(),
            4 | 5 => {}
            6 => phase.reconfigure(48_000.0, flags(15), PARAMETERS),
            _ => unreachable!(),
        }
        if event != 6 {
            let rate = if event == 4 { 44_100.5 } else { 48_000.0 };
            let channels = if event == 5 { 1 } else { 2 };
            assert!(phase.prepare(rate, channels), "event {event}");
        }
        let mut after = vec![0.0; WINDOW];
        phase.process_channel(0, &mut after);
        assert!(after.iter().all(|sample| sample.to_bits() == 0), "event {event}");
    }
}
