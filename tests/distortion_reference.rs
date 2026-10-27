//! Structural tests are derived from the ARM64 transcription, not measured
//! native output. The ignored comparator consumes a separately produced native
//! `kernel-oracle.cpp` JSON capture and compares every coefficient/sample bit.

use platinum_engine::distortion::{dc_from_normalized, type_from_normalized, DistortionContext, DistortionError, DistortionProcessor, DistortionRouting, FloatDomain, Saturate, SaturateParameters};

fn parameters() -> SaturateParameters {
    SaturateParameters {
        drive: 1.25,
        bias: 0.0,
        spread: 0.0,
        dynamics: 0.4,
        mix: 0.5,
        remove_dc: true,
    }
}
#[test]
fn descriptor_domains_keep_ranges_and_integer_rounding() {
    for (p, physical) in [(0.0_f32, -1.0_f32), (0.5, 0.0), (1.0, 1.0)] {
        assert_eq!(FloatDomain::Bias.denormalize(p).to_bits(), physical.to_bits());
        assert_eq!(FloatDomain::Bias.normalize(physical).to_bits(), p.to_bits());
    }
    assert_eq!(type_from_normalized(0.0), 0);
    assert_eq!(type_from_normalized(0.5), 3);
    assert_eq!(type_from_normalized(1.0), 5);
    assert!(!dc_from_normalized(0.0));
    assert!(dc_from_normalized(0.5));
    assert!(FloatDomain::Drive.denormalize(-10.0) >= 0.5);
    assert!(FloatDomain::Drive.denormalize(10.0) <= 10.0);
    // Peer libSystem f32 mapper probe, not an editor-to-DSP event trace.
    for (physical, normalized) in [(0x3f40_d254, 0x3e0c_0e35), (0x3f5a_be70, 0x3e37_2bdd)] {
        let value = f32::from_bits(physical);
        let mapped = FloatDomain::Drive.normalize(value);
        assert_eq!(mapped.to_bits(), normalized);
        assert_eq!(FloatDomain::Drive.denormalize(mapped).to_bits(), physical);
    }
}


#[test]
fn oversampling_factor_is_bounded_ceiling_of_binary32_ratio() {
    for (rate, factor) in [(8_000.0, 16), (22_050.0, 7), (44_100.0, 4), (48_000.0, 4), (96_000.0, 2), (192_000.0, 1)] {
        assert_eq!(Saturate::new(rate, parameters()).unwrap().oversampling_factor(), factor);
    }
    assert!(matches!(Saturate::new(0.0, parameters()), Err(DistortionError::InvalidSampleRate)));
    assert!(matches!(Saturate::new(f32::NAN, parameters()), Err(DistortionError::InvalidSampleRate)));
}

#[test]
fn reset_recreates_initial_state_and_symmetric_channels_remain_symmetric() {
    let mut dsp = Saturate::new(48_000.0, parameters()).unwrap();
    let source: Vec<f32> = (0..96).map(|i| ((i * 17 % 101) as i32 - 50) as f32 / 128.0).collect();
    let mut left = source.clone();
    let mut right = source.clone();
    dsp.process_subblock(&mut left, &mut right).unwrap();
    assert_eq!(left.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), right.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
    let first = left.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    dsp.reset();
    let mut left = source.clone();
    let mut right = source;
    dsp.process_subblock(&mut left, &mut right).unwrap();
    assert_eq!(left.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), first);
}

#[test]
fn known_silent_active_dispatch_sleeps_then_reinitializes_ramps_on_wake() {
    let mut routed = Saturate::new(48_000.0, parameters()).unwrap();
    let mut direct = Saturate::new(48_000.0, parameters()).unwrap();
    // Slot 15 leaves its buffers untouched. Deliberately nonzero contents
    // isolate the context flag's routing semantics; callers must supply the
    // true upstream silence flag, not infer it from this diagnostic.
    let mut left = [0.25, -0.5];
    let mut right = [-0.125, 0.75];
    routed.process_sleep_aware_subblock(&mut left, &mut right, true, 0).unwrap();
    assert_eq!(left, [0.25, -0.5]);
    assert_eq!(right, [-0.125, 0.75]);
    let mut expected_left = left;
    let mut expected_right = right;
    routed.process_sleep_aware_subblock(&mut left, &mut right, false, 0).unwrap();
    direct.process_subblock(&mut expected_left, &mut expected_right).unwrap();
    assert_eq!(left.map(f32::to_bits), expected_left.map(f32::to_bits));
    assert_eq!(right.map(f32::to_bits), expected_right.map(f32::to_bits));
}

#[test]
fn stable_bypass_uses_the_native_dry_delay() {
    let mut dsp = DistortionProcessor::new(48_000.0, parameters()).unwrap();
    let routing = DistortionRouting { enabled: 0, host_enabled: 1, host_bypass: 0, dry_latency_frames: 2 };
    let mut context = DistortionContext { silent: false, bypass: false };
    let mut left = [0.25, 0.5, -0.5];
    let mut right = [-0.125, 0.75, 0.125];
    dsp.process_chunk(routing, &mut context, &mut left, &mut right).unwrap();
    assert_eq!(left, [0.0, 0.0, 0.25]);
    assert_eq!(right, [0.0, 0.0, -0.125]);
}

#[test]
fn stable_active_wrapper_reaches_slot16() {
    let mut wrapper = DistortionProcessor::new(48_000.0, parameters()).unwrap();
    let mut direct = Saturate::new(48_000.0, parameters()).unwrap();
    let routing = DistortionRouting { enabled: 1, host_enabled: 1, host_bypass: 0, dry_latency_frames: 0 };
    let mut context = DistortionContext { silent: false, bypass: false };
    let mut left = [0.25, -0.5];
    let mut right = [-0.125, 0.75];
    let mut expected_left = left;
    let mut expected_right = right;
    wrapper.process_chunk(routing, &mut context, &mut left, &mut right).unwrap();
    direct.process_subblock(&mut expected_left, &mut expected_right).unwrap();
    assert_eq!(left.map(f32::to_bits), expected_left.map(f32::to_bits));
    assert_eq!(right.map(f32::to_bits), expected_right.map(f32::to_bits));
}

#[test]
fn active_to_bypass_processes_the_transition_chunk_then_goes_dry() {
    let mut wrapper = DistortionProcessor::new(48_000.0, parameters()).unwrap();
    let mut direct = Saturate::new(48_000.0, parameters()).unwrap();
    let active = DistortionRouting { enabled: 1, host_enabled: 1, host_bypass: 0, dry_latency_frames: 0 };
    let disabled = DistortionRouting { enabled: 0, ..active };
    let mut context = DistortionContext { silent: false, bypass: false };
    for routing in [active, disabled] {
        let mut left = [0.25, -0.5];
        let mut right = [-0.125, 0.75];
        let mut expected_left = left;
        let mut expected_right = right;
        wrapper.process_chunk(routing, &mut context, &mut left, &mut right).unwrap();
        direct.process_subblock(&mut expected_left, &mut expected_right).unwrap();
        assert_eq!(left.map(f32::to_bits), expected_left.map(f32::to_bits));
        assert_eq!(right.map(f32::to_bits), expected_right.map(f32::to_bits));
    }
    let mut left = [0.25, -0.5];
    let mut right = [-0.125, 0.75];
    wrapper.process_chunk(disabled, &mut context, &mut left, &mut right).unwrap();
    assert_eq!(left, [0.25, -0.5]);
    assert_eq!(right, [-0.125, 0.75]);
}

#[test]
fn rejected_channel_length_does_not_change_stream_state() {
    let mut a = Saturate::new(44_100.0, parameters()).unwrap();
    let mut b = Saturate::new(44_100.0, parameters()).unwrap();
    assert_eq!(a.process_subblock(&mut [1.0], &mut []), Err(DistortionError::ChannelLengthMismatch));
    let mut a_left = [0.5, -0.25];
    let mut a_right = [0.125, -0.75];
    let mut b_left = a_left;
    let mut b_right = a_right;
    a.process_subblock(&mut a_left, &mut a_right).unwrap();
    b.process_subblock(&mut b_left, &mut b_right).unwrap();
    assert_eq!(a_left.map(f32::to_bits), b_left.map(f32::to_bits));
    assert_eq!(a_right.map(f32::to_bits), b_right.map(f32::to_bits));
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
#[ignore = "requires a separately captured native kernel-oracle JSON file"]
fn native_direct_slot16_coefficients_and_samples_match_bitwise() {
    use serde_json::Value;
    use platinum_engine::fp_mode::with_flush_to_zero;
    fn hex_u32(value: &Value) -> u32 {
        u32::from_str_radix(value.as_str().expect("hex f32"), 16).unwrap()
    }
    fn hex_u64(value: &Value) -> u64 {
        u64::from_str_radix(value.as_str().expect("hex f64"), 16).unwrap()
    }
    let path = std::env::var_os("PLATINUM_DISTORTION_ORACLE_JSON")
        .expect("set PLATINUM_DISTORTION_ORACLE_JSON to a native kernel-oracle capture");
    let reference: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(reference["scope"], "synthetic-direct-slot16");
    assert!(matches!(reference["sha256"].as_str(),
        Some("dcf7b668fd94812a41aedad2082fbe8581fd887f0331ade7d858ab53dbca0856") |
        Some("35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578")),
        "unrecognized original core identity");
    assert_eq!(reference["mode"].as_u64(), Some(1), "only Saturate is implemented");
    let values = reference["effective_bits"].as_array().unwrap();
    assert_eq!(values.len(), 5);
    let params = SaturateParameters {
        drive: f32::from_bits(hex_u32(&values[0])),
        bias: f32::from_bits(hex_u32(&values[1])),
        spread: f32::from_bits(hex_u32(&values[2])),
        dynamics: f32::from_bits(hex_u32(&values[3])),
        mix: f32::from_bits(hex_u32(&values[4])),
        remove_dc: reference["dc"].as_u64().unwrap() != 0,
    };
    for (i, domain) in [FloatDomain::Drive, FloatDomain::Bias, FloatDomain::Spread, FloatDomain::Dynamics, FloatDomain::Mix].into_iter().enumerate() {
        let normalized = domain.normalize(f32::from_bits(hex_u32(&values[i])));
        assert_eq!(normalized.to_bits(), hex_u32(&reference["descriptor_normalized_bits"][i]), "descriptor {i} normalization");
        assert_eq!(domain.denormalize(normalized).to_bits(), hex_u32(&reference["descriptor_roundtrip_bits"][i]), "descriptor {i} round trip");
    }
    let rate = reference["rate"].as_f64().unwrap() as f32;
    let frames = reference["frames"].as_u64().unwrap() as usize;
    let segment = reference["segment"].as_u64().unwrap() as usize;
    let change = if reference["change"].is_null() {
        None
    } else {
        let event = &reference["change"];
        let values = event["effective_bits"].as_array().unwrap();
        assert_eq!(values.len(), 5);
        Some((event["frame"].as_u64().unwrap() as usize, SaturateParameters {
            drive: f32::from_bits(hex_u32(&values[0])),
            bias: f32::from_bits(hex_u32(&values[1])),
            spread: f32::from_bits(hex_u32(&values[2])),
            dynamics: f32::from_bits(hex_u32(&values[3])),
            mix: f32::from_bits(hex_u32(&values[4])),
            remove_dc: event["dc"].as_u64().unwrap() != 0,
        }))
    };
    let mut dsp = Saturate::new(rate, params).unwrap();
    assert_eq!(dsp.oversampling_factor(), reference["factor"].as_u64().unwrap() as usize);
    for (stage, row) in reference["lowpass_bits"].as_array().unwrap().iter().enumerate() {
        for (index, expected) in row.as_array().unwrap().iter().enumerate() {
            assert_eq!(dsp.oversampling_coefficients()[stage].0[index].to_bits(), hex_u64(expected), "lowpass stage {stage}, coefficient {index}");
        }
    }
    for (index, expected) in reference["highpass_bits"].as_array().unwrap().iter().enumerate() {
        assert_eq!(dsp.dc_coefficients().0[index].to_bits(), hex_u64(expected), "highpass coefficient {index}");
    }
    let mut left: Vec<f32> = (0..frames).map(|i| ((i * 17 + 13) % 257) as i32 as f32 / 128.0 - 1.0).collect();
    let mut right: Vec<f32> = (0..frames).map(|i| ((i * 29 + 71) % 257) as i32 as f32 / 128.0 - 1.0).collect();
    with_flush_to_zero(|| {
        let mut offset = 0;
        while offset < frames {
            if let Some((frame, params)) = change {
                if offset == frame {
                    dsp.set_parameters(params);
                }
            }
            let mut n = (frames - offset).min(segment);
            if let Some((frame, _)) = change {
                if offset < frame {
                    n = n.min(frame - offset);
                }
            }
            let chunks = (n + 63) / 64;
            for k in 0..chunks {
                let start = offset + k * n / chunks;
                let end = offset + (k + 1) * n / chunks;
                dsp.process_subblock(&mut left[start..end], &mut right[start..end]).unwrap();
            }
            offset += n;
        }
    });
    for (channel, samples) in [("left_bits", &left), ("right_bits", &right)] {
        let expected = reference[channel].as_array().unwrap();
        assert_eq!(samples.len(), expected.len());
        for (frame, (sample, expected)) in samples.iter().zip(expected).enumerate() {
            assert_eq!(sample.to_bits(), hex_u32(expected), "{channel} frame {frame}");
        }
    }
}
