use platinum_engine::limiter::{LimiterContext, LimiterDsp, LimiterModel, LimiterValue};

fn render(
    dsp: &mut LimiterDsp,
    model: &mut LimiterModel,
    input: &[(f32, f32)],
    chunks: &[usize],
) -> Vec<(f32, f32)> {
    let mut result = Vec::with_capacity(input.len());
    let mut offset = 0;
    let mut block = 0;
    let mut context = LimiterContext::default();
    while offset < input.len() {
        let end = (offset + chunks[block % chunks.len()]).min(input.len());
        let mut left: Vec<f32> = input[offset..end].iter().map(|x| x.0).collect();
        let mut right: Vec<f32> = input[offset..end].iter().map(|x| x.1).collect();
        dsp.process_chunk(model, &mut context, &mut left, &mut right).unwrap();
        result.extend(left.into_iter().zip(right));
        offset = end;
        block += 1;
    }
    result
}

#[test]
fn native_window_delay_and_unlimited_impulse() {
    let mut model = LimiterModel::new();
    let mut dsp = LimiterDsp::new(48_000.0, &mut model).unwrap();
    assert_eq!(dsp.latency_frames(), 95);
    model.parameters.dry_latency_frames = dsp.latency_frames() as i32;
    let mut input = vec![(0.0, 0.0); 256];
    input[0] = (0.5, -0.25);
    let output = render(&mut dsp, &mut model, &input, &[64]);
    for (i, &(left, right)) in output.iter().enumerate() {
        let expected = if i == 95 { (0.5f32, -0.25f32) } else { (0.0, 0.0) };
        assert_eq!(left.to_bits(), expected.0.to_bits(), "left frame {i}");
        assert_eq!(right.to_bits(), expected.1.to_bits(), "right frame {i}");
    }
    let mut other_model = LimiterModel::new();
    assert_eq!(LimiterDsp::new(44_100.0, &mut other_model).unwrap().latency_frames(), 87);
    let mut other_model = LimiterModel::new();
    assert_eq!(LimiterDsp::new(96_000.0, &mut other_model).unwrap().latency_frames(), 191);
}

#[test]
fn linked_peak_changes_both_channels_and_swaps_symmetrically() {
    let mut first_model = LimiterModel::new();
    let mut first = LimiterDsp::new(48_000.0, &mut first_model).unwrap();
    let mut second_model = LimiterModel::new();
    let mut second = LimiterDsp::new(48_000.0, &mut second_model).unwrap();
    let mut a = vec![(0.0, 0.0); 256];
    let mut b = a.clone();
    a[0] = (2.0, 0.25);
    b[0] = (0.25, 2.0);
    let a = render(&mut first, &mut first_model, &a, &[64]);
    let b = render(&mut second, &mut second_model, &b, &[64]);
    assert!(a[95].0 > 0.0 && a[95].0 <= 1.000_001);
    assert!(a[95].1 > 0.0 && a[95].1 < 0.25);
    assert_eq!(a[95].1.to_bits(), (a[95].0 * 0.125).to_bits());
    for (left, right) in a.iter().zip(&b) {
        assert_eq!(left.0.to_bits(), right.1.to_bits());
        assert_eq!(left.1.to_bits(), right.0.to_bits());
    }
}

#[test]
fn input_gain_change_ramps_across_the_native_chunk() {
    let mut model = LimiterModel::new();
    let mut dsp = LimiterDsp::new(48_000.0, &mut model).unwrap();
    let mut context = LimiterContext::default();
    let mut left = [0.0; 64];
    let mut right = [0.0; 64];
    dsp.process_chunk(&mut model, &mut context, &mut left, &mut right).unwrap();
    model.parameters.input_gain = 2.0;
    let mut left = [0.25, 0.0, 0.0, 0.0];
    let mut right = [0.125, 0.0, 0.0, 0.0];
    dsp.process_chunk(&mut model, &mut context, &mut left, &mut right).unwrap();
    let mut delayed_left = [0.0; 64];
    let mut delayed_right = [0.0; 64];
    dsp.process_chunk(&mut model, &mut context, &mut delayed_left, &mut delayed_right).unwrap();
    let mut last_left = [0.0; 32];
    let mut last_right = [0.0; 32];
    dsp.process_chunk(&mut model, &mut context, &mut last_left, &mut last_right).unwrap();
    // Impulse entered at frame 64 and emerges 95 frames later, in the final chunk.
    assert_eq!(last_left[27].to_bits(), 0.3125f32.to_bits());
    assert_eq!(last_right[27].to_bits(), 0.15625f32.to_bits());
}

#[test]
fn inactive_path_preserves_the_controller_supplied_dry_delay() {
    let mut model = LimiterModel::new();
    model.parameters.enabled = 0;
    let mut dsp = LimiterDsp::new(48_000.0, &mut model).unwrap();
    model.parameters.dry_latency_frames = dsp.latency_frames() as i32;
    let mut input = vec![(0.0, 0.0); 256];
    input[0] = (0.5, -0.25);
    let output = render(&mut dsp, &mut model, &input, &[17, 31, 64, 3]);
    for (i, &(left, right)) in output.iter().enumerate() {
        let expected = if i == 95 { (0.5f32, -0.25f32) } else { (0.0, 0.0) };
        assert_eq!(left.to_bits(), expected.0.to_bits(), "left frame {i}");
        assert_eq!(right.to_bits(), expected.1.to_bits(), "right frame {i}");
    }
}

#[test]
fn disabling_runs_one_last_active_chunk_before_dry_mode() {
    let mut model = LimiterModel::new();
    model.parameters.output_gain = 0.5;
    let mut dsp = LimiterDsp::new(48_000.0, &mut model).unwrap();
    model.parameters.dry_latency_frames = dsp.latency_frames() as i32;
    let mut context = LimiterContext::default();
    let mut left = [0.0; 64];
    let mut right = [0.0; 64];
    left[0] = 0.5;
    right[0] = 0.25;
    dsp.process_chunk(&mut model, &mut context, &mut left, &mut right).unwrap();
    model.parameters.enabled = 0;
    let mut left = [0.0; 64];
    let mut right = [0.0; 64];
    dsp.process_chunk(&mut model, &mut context, &mut left, &mut right).unwrap();
    assert_eq!(left[31].to_bits(), 0.25f32.to_bits());
    assert_eq!(right[31].to_bits(), 0.125f32.to_bits());
}

#[test]
fn editor_and_model_keep_their_separate_parameter_conversions() {
    let mut model = LimiterModel::new();
    assert_eq!(model.decode_editor_parameter(0, 0.0), Some(LimiterValue::Enabled(0)));
    assert_eq!(model.decode_editor_parameter(0, 1.0), Some(LimiterValue::Enabled(1)));
    let input = model.decode_editor_parameter(1, 0.5).unwrap();
    assert_eq!(input, LimiterValue::InputGain(1.0));
    model.apply_editor_value(input);
    assert_eq!(model.parameters.input_gain.to_bits(), 1.0f32.to_bits());
    assert_eq!(model.decode_editor_parameter(5, 0.5), None);
}

#[test]
#[ignore = "requires PN_LIMITER_IMPULSE_CAPTURE from the original 48 kHz stereo wrapper"]
fn original_wrapper_impulse_matches_with_host_default_replay() {
    use platinum_engine::fp_mode::with_flush_to_zero;

    let path = std::env::var_os("PN_LIMITER_IMPULSE_CAPTURE").expect("native capture path");
    let reference = std::fs::read(path).expect("read native capture");
    let frames = 8192 + 48000;
    assert_eq!(reference.len(), frames * 2 * 4);
    let mut model = LimiterModel::new();
    let mut dsp = LimiterDsp::new(48_000.0, &mut model).unwrap();
    model.parameters.dry_latency_frames = dsp.latency_frames() as i32;
    // Observed original AU defaults, not the raw DSP constructor values.
    // Replaying the host's descriptor values changes ceiling/release by one ULP.
    let normalized = [0x3f800000, 0x3f000000, 0x3f000000, 0x3f44ec53, 0x3f012947];
    let writes: [_; 5] = std::array::from_fn(|id| {
        model.decode_editor_parameter(id as u32, f32::from_bits(normalized[id])).unwrap()
    });
    with_flush_to_zero(|| {
        for value in writes { model.apply_editor_value(value); }
        let mut remaining = 2400usize;
        let mut left = [0.0; 512];
        let mut right = [0.0; 512];
        while remaining != 0 {
            let n = remaining.min(512);
            let chunks = n.div_ceil(64);
            for part in 0..chunks {
                let start = part * n / chunks;
                let end = (part + 1) * n / chunks;
                let mut context = LimiterContext { silent: true, bypass: false };
                dsp.process_chunk(&mut model, &mut context, &mut left[start..end], &mut right[start..end]).unwrap();
            }
            remaining -= n;
        }
        let mut input = vec![(0.0, 0.0); frames];
        input[0] = (1.5, 0.25);
        input[513] = (-2.0, 0.0);
        input[1027] = (0.0, 1.25);
        let output = render(&mut dsp, &mut model, &input, &[64]);
        let mut mismatches = 0;
        for (frame, &(left, right)) in output.iter().enumerate() {
            for (channel, value) in [left, right].into_iter().enumerate() {
                let offset = (frame * 2 + channel) * 4;
                let expected = u32::from_le_bytes(reference[offset..offset + 4].try_into().unwrap());
                if value.to_bits() != expected {
                    if mismatches < 12 {
                        eprintln!("frame {frame} channel {channel}: native {expected:08x}, Rust {:08x}", value.to_bits());
                    }
                    mismatches += 1;
                }
            }
        }
        assert_eq!(mismatches, 0, "original wrapper sample words");
    });
}
