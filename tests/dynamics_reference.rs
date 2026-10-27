//! Instruction-derived active-slot cases, not recorded native golden audio.
//! Original-unit and complete-file parity remain separate acceptance gates.

use platinum_engine::dynamics::{controller_lookahead_argument, Dynamics, DynamicsError, DynamicsField, DynamicsParameters};

fn neutral() -> DynamicsParameters {
    DynamicsParameters {
        input_gain_db: 0.0,
        output_gain_db: 0.0,
        low_threshold_db: -50.0,
        low_ratio: 1.0,
        high_threshold_db: 0.0,
        high_ratio: 1.0,
        attack: 1.0,
        release: 1.0,
        knee: 0.0,
        mix: 1.0,
    }
}

fn bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|v| v.to_bits()).collect()
}

#[test]
fn model_descriptors_keep_native_event_ids_and_reversed_log_domains() {
    assert_eq!(DynamicsField::Mix.event_id(), 7);
    assert_eq!(DynamicsField::Attack.event_id(), 422);
    assert_eq!(DynamicsField::Knee.event_id(), 423);
    assert_eq!(DynamicsField::InputGain.model_physical_from_normalized(0.5).to_bits(), 0.0_f32.to_bits());
    assert_eq!(DynamicsField::LowThreshold.model_physical_from_normalized(0.5).to_bits(), (-25.0_f32).to_bits());
    assert_eq!(DynamicsField::Knee.model_physical_from_normalized(0.25).to_bits(), 6.25_f32.to_bits());
    assert_eq!(DynamicsField::Attack.model_physical_from_normalized(0.5).to_bits(), 1.0_f32.to_bits());
    assert_eq!(DynamicsField::LowRatio.model_physical_from_normalized(0.5).to_bits(), 1.0_f32.to_bits());
    assert_eq!(DynamicsField::HighRatio.model_physical_from_normalized(-1.0).to_bits(), 2.0_f32.to_bits());
    assert_eq!(DynamicsField::HighRatio.model_physical_from_normalized(2.0).to_bits(), 0x3d08_8889);

    let field = DynamicsField::Release;
    let clamped_high = field.model_event_value(-1.0, 2.0);
    assert_eq!(clamped_high.to_bits(), field.model_physical_from_normalized(1.0).to_bits());
    // The initial -20 dB model store does not survive this f32 round trip.
    assert_eq!(DynamicsField::HighThreshold.model_event_value(-20.0, 0.0).to_bits(), 0xc19f_ffff);
    let mut parameters = neutral();
    parameters.apply_model_event(DynamicsField::LowThreshold, -25.0, 0.0);
    assert_eq!(parameters.low_threshold_db.to_bits(), (-25.0_f32).to_bits());
}

#[test]
fn controller_lookahead_is_rounded_before_the_dsp_clamp() {
    let p = neutral();
    for (rate, expected) in [(44_100.0, 128), (48_000.0, 139), (96_000.0, 279), (192_000.0, 557), (384_000.0, 1115)] {
        assert_eq!(controller_lookahead_argument(rate), expected);
        let dsp = Dynamics::new(rate, controller_lookahead_argument(rate), p).unwrap();
        assert_eq!(dsp.lookahead_frames(), expected.min(896) as usize);
    }
}

#[test]
fn neutral_linked_kernel_and_two_frame_delay_have_exact_vectors() {
    let p = neutral();
    let mut dsp = Dynamics::new(48_000.0, 2, p).unwrap();
    let mut left = [0.25, -0.5, 0.75, -1.0, 0.125, 0.0];
    let mut right = [-0.25, 0.5, -0.75, 1.0, -0.125, 0.0];
    dsp.process_block(p, &mut left, &mut right).unwrap();
    assert_eq!(bits(&left), bits(&[0.0, 0.0, 0.25, -0.5, 0.75, -1.0]));
    assert_eq!(bits(&right), bits(&[0.0, 0.0, -0.25, 0.5, -0.75, 1.0]));
    assert_eq!(dsp.lookahead_frames(), 2);
}

#[test]
fn lookahead_clamp_and_wrap_preserve_fifo_across_chunks() {
    let p = neutral();
    assert_eq!(Dynamics::new(44_100.0, -5, p).unwrap().lookahead_frames(), 0);
    let mut dsp = Dynamics::new(48_000.0, 10_000, p).unwrap();
    assert_eq!(dsp.lookahead_frames(), 896);
    let total = 2304;
    let input: Vec<f32> = (0..total).map(|i| ((i % 127) as f32 - 63.0) / 128.0).collect();
    let mut left = input.clone();
    let mut right = input.clone();
    for (l, r) in left.chunks_mut(64).zip(right.chunks_mut(64)) {
        dsp.process_block(p, l, r).unwrap();
    }
    let mut expected = vec![0.0; total];
    expected[896..].copy_from_slice(&input[..total - 896]);
    assert_eq!(bits(&left), bits(&expected));
    assert_eq!(bits(&right), bits(&expected));
}

#[test]
fn gain_change_uses_previous_target_and_native_one_based_ramp() {
    let p = neutral();
    let mut dsp = Dynamics::new(48_000.0, 0, p).unwrap();
    let mut l = [1.0];
    let mut r = [1.0];
    dsp.process_block(p, &mut l, &mut r).unwrap();
    let next = DynamicsParameters { input_gain_db: 20.0, ..p };
    let mut l = [1.0; 4];
    let mut r = [1.0; 4];
    dsp.process_block(next, &mut l, &mut r).unwrap();
    assert_eq!(bits(&l), bits(&[3.25, 5.5, 7.75, 10.0]));
    assert_eq!(bits(&r), bits(&[3.25, 5.5, 7.75, 10.0]));
}

#[test]
fn reset_quadratic_knee_boundaries_have_exact_gains() {
    let initial = f32::from_bits(0xc288_5461);
    for (distance, expected) in [(-2.0, 0.0_f32), (0.0, 0.5), (2.0, 2.0)] {
        let p = DynamicsParameters {
            low_threshold_db: initial + distance,
            low_ratio: 0.0,
            high_threshold_db: 0.0,
            high_ratio: 1.0,
            knee: 2.0,
            ..neutral()
        };
        let dsp = Dynamics::new(48_000.0, 0, p).unwrap();
        assert_eq!(dsp.meters().gain_db.to_bits(), expected.to_bits());
    }
}

#[test]
fn one_channel_drives_shared_stereo_detector() {
    let p = DynamicsParameters {
        low_threshold_db: -100.0,
        high_threshold_db: -30.0,
        high_ratio: 0.5,
        ..neutral()
    };
    let mut baseline = Dynamics::new(48_000.0, 0, p).unwrap();
    let mut linked = Dynamics::new(48_000.0, 0, p).unwrap();
    let mut mirror = Dynamics::new(48_000.0, 0, p).unwrap();
    let mut base_right = [0.0; 64];
    let mut pulse_right = [0.0; 64];
    for _ in 0..16 {
        let mut base_l = [0.125; 64];
        base_right.fill(0.125);
        let mut pulse_l = [1.0; 64];
        pulse_right.fill(0.125);
        let mut mirrored_l = [0.125; 64];
        let mut mirrored_r = [1.0; 64];
        baseline.process_block(p, &mut base_l, &mut base_right).unwrap();
        linked.process_block(p, &mut pulse_l, &mut pulse_right).unwrap();
        mirror.process_block(p, &mut mirrored_l, &mut mirrored_r).unwrap();
        assert_eq!(bits(&pulse_l), bits(&mirrored_r));
        assert_eq!(bits(&pulse_right), bits(&mirrored_l));
    }
    assert!(pulse_right[63] < base_right[63]);
    assert_eq!(linked.meters().detector_db.to_bits(), mirror.meters().detector_db.to_bits());
}

#[test]
fn reset_clears_delay_history_and_invalid_block_does_not_advance() {
    let p = neutral();
    let mut dsp = Dynamics::new(48_000.0, 3, p).unwrap();
    let mut l = [0.5; 4];
    let mut r = [0.25; 4];
    dsp.process_block(p, &mut l, &mut r).unwrap();
    dsp.reset(p);
    let before = dsp.meters();
    let mut oversized_l = [0.5; 65];
    let mut oversized_r = [0.25; 65];
    assert_eq!(dsp.process_block(p, &mut oversized_l, &mut oversized_r), Err(DynamicsError::InvalidBlock));
    assert_eq!(dsp.meters(), before);
    let mut l = [0.5; 4];
    let mut r = [0.25; 4];
    dsp.process_block(p, &mut l, &mut r).unwrap();
    assert_eq!(bits(&l), bits(&[0.0, 0.0, 0.0, 0.5]));
    assert_eq!(bits(&r), bits(&[0.0, 0.0, 0.0, 0.25]));
}

#[test]
fn invalid_sample_rates_are_rejected() {
    for rate in [0.0, -1.0, f32::INFINITY, f32::NAN] {
        assert!(matches!(Dynamics::new(rate, 0, neutral()), Err(DynamicsError::InvalidSampleRate)));
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
struct RestoreFpcr(u64);

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl Drop for RestoreFpcr {
    fn drop(&mut self) {
        unsafe { std::arch::asm!("msr fpcr, {value}", value = in(reg) self.0) };
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn host_fpcr() -> RestoreFpcr {
    let previous: u64;
    unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) previous) };
    let active = previous | (1 << 24);
    unsafe { std::arch::asm!("msr fpcr, {value}", value = in(reg) active) };
    RestoreFpcr(previous)
}

/// Run only with a freshly generated direct-kernel reference capture. This is
/// deliberately ignored by default rather than silently passing without one.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
#[ignore = "requires DYNAMICS_ORACLE_JSONL from the pinned native probe"]
fn compare_direct_native_kernel_vectors() {
    let _restore = host_fpcr();

    let path = std::env::var("DYNAMICS_ORACLE_JSONL").expect("set DYNAMICS_ORACLE_JSONL to the reference probe output");
    let capture = std::fs::read_to_string(path).expect("read native capture");
    let records: Vec<serde_json::Value> = capture.lines()
        .map(|line| serde_json::from_str(line).expect("valid native JSON record"))
        .collect();
    assert_eq!(records.len(), 4);
    let compare = |name: &str, left: &[f32], right: &[f32], dsp: &Dynamics| {
        let record = records.iter().find(|r| r["case"].as_str() == Some(name)).expect("missing native case");
        let expected_l: Vec<String> = serde_json::from_value(record["left_bits"].clone()).unwrap();
        let expected_r: Vec<String> = serde_json::from_value(record["right_bits"].clone()).unwrap();
        let hexadecimal = |v: &[f32]| v.iter().map(|x| format!("{:08x}", x.to_bits())).collect::<Vec<_>>();
        assert_eq!(hexadecimal(left), expected_l, "{name} left");
        assert_eq!(hexadecimal(right), expected_r, "{name} right");
        assert_eq!(format!("{:08x}", dsp.meters().detector_db.to_bits()), record["meter_db_bits"].as_str().unwrap(), "{name} detector");
        assert_eq!(format!("{:08x}", dsp.meters().gain_db.to_bits()), record["gain_db_bits"].as_str().unwrap(), "{name} gain");
    };

    let p = neutral();
    let mut dsp = Dynamics::new(48_000.0, 2, p).unwrap();
    let mut left = [0.25, -0.5, 0.75, -1.0, 0.125, 0.0];
    let mut right = [-0.25, 0.5, -0.75, 1.0, -0.125, 0.0];
    dsp.process_block(p, &mut left, &mut right).unwrap();
    compare("delay-neutral", &left, &right, &dsp);

    let mut dsp = Dynamics::new(48_000.0, 0, p).unwrap();
    dsp.process_block(p, &mut [1.0], &mut [1.0]).unwrap();
    let changed = DynamicsParameters { input_gain_db: 20.0, ..p };
    let mut left = [1.0; 4];
    let mut right = [1.0; 4];
    dsp.process_block(changed, &mut left, &mut right).unwrap();
    compare("gain-ramp", &left, &right, &dsp);

    let compression = DynamicsParameters {
        low_threshold_db: -100.0, high_threshold_db: -30.0, high_ratio: 0.5, ..p
    };
    let mut dsp = Dynamics::new(48_000.0, 0, compression).unwrap();
    let mut left = [1.0; 64];
    let mut right = [0.125; 64];
    for _ in 0..16 {
        left.fill(1.0);
        right.fill(0.125);
        dsp.process_block(compression, &mut left, &mut right).unwrap();
    }
    compare("linked-compression-final", &left[48..], &right[48..], &dsp);

    let mut knee = DynamicsParameters {
        input_gain_db: 3.0, output_gain_db: -2.0, low_threshold_db: -27.0,
        low_ratio: 0.5, high_threshold_db: -9.0, high_ratio: 1.5,
        attack: 8.0, release: 15.0, knee: 2.5, mix: 0.75,
    };
    let mut dsp = Dynamics::new(44_100.0, 7, knee).unwrap();
    let mut left = [0.0; 31];
    let mut right = [0.0; 31];
    for block in 0..25 {
        for i in 0..31 {
            let n = block * 31 + i;
            left[i] = (((n * 17) % 257) as i32 - 128) as f32 / 128.0;
            right[i] = (((n * 29 + 7) % 257) as i32 - 128) as f32 / 128.0;
        }
        if block == 12 {
            knee.knee = 8.0;
            knee.release = 4.0;
            knee.low_threshold_db = -35.0;
            knee.mix = 0.4;
        }
        dsp.process_block(knee, &mut left, &mut right).unwrap();
    }
    compare("knee-update-final", &left, &right, &dsp);
}

/// Direct model-event differential, separate from the audio-slot comparison.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
#[ignore = "requires DYNAMICS_MODEL_ORACLE_JSONL from the pinned model probe"]
fn compare_direct_native_model_events() {
    let _restore = host_fpcr();
    let path = std::env::var("DYNAMICS_MODEL_ORACLE_JSONL").expect("set DYNAMICS_MODEL_ORACLE_JSONL to native model capture");
    let capture = std::fs::read_to_string(path).expect("read native model capture");
    let mut seen = [0_usize; 10];
    for line in capture.lines() {
        let record: serde_json::Value = serde_json::from_str(line).expect("valid native model JSON");
        let id = record["id"].as_u64().expect("event ID") as u32;
        let field = match id {
            0 => DynamicsField::InputGain,
            1 => DynamicsField::OutputGain,
            2 => DynamicsField::LowThreshold,
            3 => DynamicsField::LowRatio,
            4 => DynamicsField::HighThreshold,
            5 => DynamicsField::HighRatio,
            6 => DynamicsField::Release,
            7 => DynamicsField::Mix,
            422 => DynamicsField::Attack,
            423 => DynamicsField::Knee,
            _ => panic!("unexpected event ID {id}"),
        };
        let index = match id { 0..=7 => id as usize, 422 => 8, 423 => 9, _ => unreachable!() };
        seen[index] += 1;
        let word = |key: &str| u32::from_str_radix(record[key].as_str().expect("hex word"), 16).expect("valid f32 bits");
        let raw = f32::from_bits(word("raw"));
        let modulation = f32::from_bits(word("modulation"));
        assert_eq!(field.model_event_value(raw, modulation).to_bits(), word("value"),
                   "event {id}, raw={:08x}, modulation={:08x}", raw.to_bits(), modulation.to_bits());
    }
    assert_eq!(seen, [30; 10], "each native field must have 30 cases");
}
