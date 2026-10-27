use platinum_engine::controller::{
    detector_feed, final_frame_plan, final_sample_rate, graph_latency_frames, meter_feed,
    processing_sample_rate, AnalysisValues, FinalPlan, FrameAccounting, FrameError,
    HealingPlan, MeterFeed, Mp3CountInputs, RecordSpan, RecorderAccounting, RenderPass,
    HEAL_PADDING_FRAMES, RENDER_POST, RENDER_SILENCE,
};
use platinum_engine::loudness::GainAnalyzer;
use platinum_engine::multipass::{self, DynamicsBand, ShippedPreset};
use platinum_engine::templates::{Band, Template};

fn template() -> Template {
    let mut t = Template::default();
    t.exciter_option = -1;
    t.clip.enabled = true;
    t.clip.low = -20.0;
    t.clip.high = -10.0;
    t.clip.headroom = 2.5;
    t.clip.percentage = 97.25;
    t.expansion.enabled = true;
    t.expansion.low = -20.0;
    t.expansion.high = -10.0;
    t.expansion.bands = [Band { threshold: -20.0, gain: 2.0, ratio: 2.0, attack: 1.0, release: 1.0 }; 4];
    t.expansion.crossovers_hz = [95.0, 550.0, 5500.0];
    t.compression.enabled = true;
    t.compression.low = -10.0;
    t.compression.high = -20.0;
    t.compression.bands = [Band { threshold: -18.0, gain: 1.0, ratio: 1.5, attack: 3.0, release: 4.0 }; 4];
    t.compression.crossovers_hz = [100.0, 600.0, 6000.0];
    t.output.limiter_threshold = -12.5;
    t.output.margin = -0.6;
    t.output.limiter_release = 3.0;
    t
}

fn analysis(level: f32, tuning: f32) -> AnalysisValues {
    AnalysisValues { input_level_db: level, input_tuning_hz: tuning }
}

fn same_writes(actual: &[multipass::ParameterWrite], expected: &[multipass::ParameterWrite]) {
    assert_eq!(actual.len(), expected.len());
    for (a, b) in actual.iter().zip(expected) {
        assert_eq!((a.id, a.value.to_bits()), (b.id, b.value.to_bits()));
    }
}

#[test]
fn analysis_capture_and_inclusive_volume_gates() {
    // 0x100011e60, 0x100011fec, 0x10001210c and 0x100017410.
    let captured = AnalysisValues::from_measurements(-12.125_000_001, 0.999);
    assert_eq!(captured.input_level_db.to_bits(), (-12.125_f32).to_bits());
    assert_eq!(captured.input_tuning_hz, 440.0);
    assert_eq!(AnalysisValues::from_measurements(0.0, 1.0).input_tuning_hz, 1.0);
    assert!(AnalysisValues::from_measurements(0.0, f32::NAN).input_tuning_hz.is_nan());
    let mut t = template();
    for level in [-20.0, -10.0] {
        let plan = HealingPlan::from_analysis(&t, analysis(level, 440.0));
        assert_eq!(plan.clip.unwrap().threshold_percent, 97.25);
        assert!(plan.expansion.is_some());
        assert!(plan.compression.is_some());
    }
    for level in [-20.000_002_f32, -9.999_999_f32, f32::NAN, f32::NEG_INFINITY] {
        let plan = HealingPlan::from_analysis(&t, analysis(level, 440.0));
        assert!(plan.clip.is_none());
        assert!(plan.expansion.is_none());
        assert!(plan.compression.is_none());
    }
    // Clip uses stored bounds without sorting; dynamics uses FCSEL ordering.
    t.clip.low = -10.0;
    t.clip.high = -20.0;
    let plan = HealingPlan::from_analysis(&t, analysis(-15.0, 440.0));
    assert!(plan.clip.is_none());
    assert!(plan.compression.is_some());
    t.expansion.low = f32::NAN;
    assert!(HealingPlan::from_analysis(&t, analysis(-15.0, 440.0)).expansion.is_none());
}

#[test]
fn pitch_gate_rounding_readback_and_replacement_mode() {
    // 0x10001c788: f32 division, libSystem log2f, f32 multiply, FCVTAS X0.
    let mut t = template();
    t.pitch.enabled = true;
    t.pitch.option = 1;
    t.pitch.threshold_cents = 1200;
    let plan = HealingPlan::from_analysis(&t, analysis(-15.0, 220.0));
    let pitch = plan.pitch.unwrap();
    assert_eq!(pitch.correction_cents, 1200);
    assert_eq!(pitch.requested_ratio, 2.0);
    assert_eq!(pitch.requested_mode, 1.0);
    let timing = pitch.resolve(1.75, false);
    assert_eq!(timing.adjustment_readback, 1.75);
    assert_eq!(timing.tempo_factor, 1.75);
    assert_eq!(timing.forced_mode, None);
    let replacement = pitch.resolve(1.75, true);
    assert_eq!(replacement.tempo_factor, 1.0);
    assert_eq!(replacement.forced_mode, Some(0.0));
    assert_eq!(HealingPlan::from_analysis(&t, analysis(-15.0, 880.0)).pitch.unwrap().correction_cents, -1200);
    t.pitch.threshold_cents = 1201;
    assert!(HealingPlan::from_analysis(&t, analysis(-15.0, 220.0)).pitch.is_none());
    t.pitch.threshold_cents = 0;
    let nan = HealingPlan::from_analysis(&t, analysis(-15.0, f32::NAN)).pitch.unwrap();
    assert_eq!(nan.correction_cents, 0); // FCVTAS NaN -> 0
    assert_eq!(nan.requested_ratio, 1.0); // FCSEL GT rejects unordered
    assert!(HealingPlan::from_analysis(&t, analysis(-15.0, f32::INFINITY)).pitch.is_none()); // wrapping CNEG of i64::MIN
    t.pitch.option = -1;
    assert_eq!(HealingPlan::from_analysis(&t, analysis(-15.0, 440.0)).pitch.unwrap().requested_mode, u32::MAX as f32);
}

#[test]
fn stage_order_pre_normalization_and_existing_write_builders() {
    // 0x100012454..0x100012614 and 0x100013238.
    let mut t = template();
    t.exciter_option = 1;
    let plan = HealingPlan::from_analysis(&t, analysis(-20.0, 440.0));
    let warmth = plan.warmth.unwrap();
    assert_eq!(warmth.preset, ShippedPreset::Warmth1);
    assert_eq!(warmth.input_gain_db, 7.5); // -15 - (-20 - 2.5)
    assert_eq!(warmth.wrapper_flags, [true, true, true, false, false]);
    assert_eq!(warmth.writes[0].unwrap().id, 34);
    assert_eq!(warmth.writes[1].unwrap().id, 40);
    let expansion = plan.expansion.unwrap();
    let compression = plan.compression.unwrap();
    assert_eq!(expansion.input_gain_db, 0.0);
    assert_eq!(compression.input_gain_db, 0.0);
    assert!(expansion.reciprocal_ratio && expansion.correct_band_gain);
    assert!(!compression.reciprocal_ratio && compression.correct_band_gain);
    assert_eq!(expansion.wrapper_flags, [true, true, true, true, false]);
    let source = t.expansion.bands.map(|b| DynamicsBand { threshold_db: b.threshold, ratio: b.ratio, gain_db: b.gain });
    let expected = multipass::dynamics_parameter_writes(source, t.expansion.crossovers_hz, true, true, 0.0, 0.0);
    same_writes(&expansion.writes, &expected);
    assert!(plan.gain.is_none());
    assert_eq!(plan.analyzer_release_seconds, f64::from(3.0_f32 / 1000.0_f32));
    assert_eq!(plan.target_level_db, -12.5);

    t.exciter_option = -1;
    t.expansion.enabled = false;
    t.compression.enabled = false;
    t.clip.enabled = false;
    let plan = HealingPlan::from_analysis(&t, analysis(-17.0, 440.0));
    let gain = plan.gain.unwrap();
    assert_eq!(gain.preset, ShippedPreset::Gain);
    assert_eq!(gain.gain_db, 2.0);
    same_writes(&gain.writes, &multipass::gain_stage_parameter_writes(2.0));
    assert!(HealingPlan::from_analysis(&t, analysis(-15.0, 440.0)).gain.is_none());
    assert!(HealingPlan::from_analysis(&t, analysis(-15.02, 440.0)).gain.is_some());
    let beyond = -15.01_f32;
    let inside = f32::from_bits(beyond.to_bits() - 1);
    assert!(HealingPlan::from_analysis(&t, analysis(inside, 440.0)).gain.is_none());
    assert!(HealingPlan::from_analysis(&t, analysis(beyond, 440.0)).gain.is_some());
    assert!(HealingPlan::from_analysis(&t, analysis(f32::NAN, 440.0)).gain.is_none());
    t.exciter_option = 2;
    let hot = HealingPlan::from_analysis(&t, analysis(-15.0, 440.0)).warmth.unwrap();
    assert_eq!(hot.preset, ShippedPreset::Warmth2);
    assert!(hot.writes[0].is_none() && hot.writes[1].is_none());
}

#[test]
fn official_synthetic_control_uses_the_observed_stage_sequence() {
    let t = platinum_engine::templates::builtins().unwrap().remove(0);
    let plan = HealingPlan::from_analysis(&t, analysis(-10.0, 440.0));
    let clip = plan.clip.unwrap();
    assert_eq!(clip.headroom_db, 1.5);
    assert_eq!(clip.threshold_percent.to_bits(), 0x42c7_6666);
    assert!(plan.pitch.is_none());
    let warmth = plan.warmth.unwrap();
    assert_eq!(warmth.preset, ShippedPreset::Warmth1);
    assert_eq!(warmth.input_gain_db, -3.5);
    assert_eq!(warmth.writes[0].unwrap().id, 34);
    assert!(warmth.writes[1].is_none());
    let expansion = plan.expansion.unwrap();
    let b = [
        DynamicsBand { threshold_db: -22.0, gain_db: 0.0, ratio: f32::from_bits(0x3f8c_cccd) },
        DynamicsBand { threshold_db: -21.0, gain_db: 0.0, ratio: f32::from_bits(0x3f8c_cccd) },
        DynamicsBand { threshold_db: -24.0, gain_db: 0.0, ratio: f32::from_bits(0x3f99_999a) },
        DynamicsBand { threshold_db: -24.0, gain_db: 0.0, ratio: f32::from_bits(0x3f99_999a) },
    ];
    let expected = multipass::dynamics_parameter_writes(b, [95.0, 550.0, 5500.0], true, true, 0.0, 0.0);
    same_writes(&expansion.writes, &expected);
    assert!(plan.compression.is_none());
    assert!(plan.gain.is_none());
}

#[test]
fn final_limiter_headroom_conversion_and_silence() {
    // 0x100015af4..0x100015b30 and 0x1000164b8..0x1000165c8.
    let mut t = template();
    let disabled = GainAnalyzer::new(48_000.0, 1, 0.003, false).unwrap();
    let plan = FinalPlan::from_healed(&t, &disabled);
    assert_eq!(plan.healed_level_db, -200.0);
    assert_eq!(plan.effective_headroom_db, f64::from(-0.6_f32));
    assert_eq!(plan.analyzer_target_db, -12.5 - f64::from(-0.6_f32));
    assert_eq!(plan.analyzer_gain_db, 0.0);
    assert_eq!(plan.writes.map(|w| w.id), [1, 2, 3, 4]);
    t.output.margin = 25.0;
    let over = FinalPlan::from_healed(&t, &disabled);
    assert_eq!(over.writes[0].value, 1.0);
    assert_eq!(over.writes[1].value, 0.625);
    assert_eq!(over.writes[2].value, 0.0);
    t.output.margin = -25.0;
    let under = FinalPlan::from_healed(&t, &disabled);
    assert_eq!(under.writes[0].value, 0.0);
    assert_eq!(under.writes[1].value, 0.375);
    assert_eq!(under.writes[2].value, 0.0);
    t.output.margin = -0.6;
    t.output.limiter_threshold = -11.0;
    let zero = FinalPlan::from_healed(&t, &disabled);
    assert_eq!(zero.effective_headroom_db.to_bits(), (-0.0_f64).to_bits());
    t.output.margin = 1.0;
    t.output.limiter_threshold = -12.0;
    assert_eq!(FinalPlan::from_healed(&t, &disabled).effective_headroom_db, 0.0);
    t.output.limiter_threshold = f32::NAN;
    t.output.margin = -0.5;
    t.output.limiter_release = f32::NAN;
    let nan = FinalPlan::from_healed(&t, &disabled);
    assert_eq!(nan.effective_headroom_db, -0.5);
    assert_eq!(nan.writes[3].value, 1.0);
    t.output.limiter_release = 0.0;
    assert_eq!(FinalPlan::from_healed(&t, &disabled).writes[3].value, 0.0);

    t.output.limiter_threshold = -12.5;
    t.output.limiter_release = 3.0;
    let mut silent = GainAnalyzer::new(48_000.0, 1, 0.003, true).unwrap();
    silent.process_interleaved(&vec![0.0; 19_200]).unwrap();
    let result = FinalPlan::from_healed(&t, &silent);
    assert_eq!(result.healed_level_db, f32::NEG_INFINITY);
    assert_eq!(result.analyzer_gain_db, 0.0);
}

#[test]
fn sample_rate_and_latency_boundaries() {
    assert_eq!(processing_sample_rate(22_050.0, false), 44_100.0);
    assert_eq!(processing_sample_rate(22_051.0, false), 22_051.0);
    assert_eq!(processing_sample_rate(22_050.0, true), 22_050.0);
    assert_eq!(processing_sample_rate(96_000.0, true), 44_100.0);
    assert!(processing_sample_rate(f64::NAN, false).is_nan());
    assert_eq!(processing_sample_rate(f64::NAN, true), 44_100.0);
    assert_eq!(final_sample_rate(96_000.0, 1), 44_100.0);
    assert_eq!(final_sample_rate(96_000.0, 6), 96_000.0);
    assert_eq!(final_sample_rate(96_000.0, 0x6161_6320), 44_100.0);
    assert_eq!(graph_latency_frames(Some(4.0), &[Some(0.125)]), 1);
    assert_eq!(graph_latency_frames(Some(4.0), &[Some(-0.125)]), -1);
    assert_eq!(graph_latency_frames(Some(48_000.0), &[None, Some(0.001)]), 48);
    assert_eq!(graph_latency_frames(None, &[Some(1.0)]), 0);
}

#[test]
fn final_target_count_includes_mp3_header_and_delay_math() {
    // 0x100016604 FCVTAS W; 0x1000166e4 FCVTZS W; 0x100016810 FCVTAS X.
    let ordinary = final_frame_plan(101, 2.0, 7, None);
    assert_eq!(ordinary.adjusted_input_frames, 101);
    assert_eq!(ordinary.recorder_shift_frames, -7);
    assert_eq!(ordinary.recorder_target_frames, 51);
    assert_eq!(RecorderAccounting::for_final(&ordinary).shift_remaining(), -7);
    assert_eq!(RecorderAccounting::for_final(&ordinary).target_frames, 51);
    assert_eq!(RecorderAccounting::for_healing(2560).shift_remaining(), -2560);
    let mut mp3 = Mp3CountInputs {
        input_is_mp3: true,
        input_sample_rate: 48_000.0,
        output_sample_rate: 48_000.0,
        input_has_xing_header: true,
        input_has_info_header: false,
        decode_using_vbr_header: true,
        input_lame_info_latency: 1100,
        input_decoder_delay: 576,
    };
    let with_header = final_frame_plan(4800, 2.0, 7, Some(mp3));
    assert!(with_header.output_has_vbr_header);
    assert_eq!(with_header.adjusted_input_frames, 4800);
    assert_eq!(with_header.header_shift_frames, 236);
    assert_eq!(with_header.recorder_shift_frames, 229);
    assert_eq!(with_header.recorder_target_frames, 2636);
    mp3.input_has_xing_header = false;
    mp3.input_sample_rate = 44_100.0;
    let without_header = final_frame_plan(1000, 1.0, 7, Some(mp3));
    assert!(!without_header.output_has_vbr_header);
    assert_eq!(without_header.adjusted_input_frames, 471);
    assert_eq!(without_header.header_shift_frames, -529);
    assert_eq!(without_header.recorder_target_frames, -58);
    mp3.input_is_mp3 = false;
    mp3.input_decoder_delay = 0;
    let encoded = final_frame_plan(1000, 1.0, 0, Some(mp3));
    assert!(encoded.output_has_vbr_header);
    assert_eq!(encoded.adjusted_input_frames, 1000);
    assert_eq!(encoded.recorder_target_frames, 1000);
}

#[test]
fn meter_and_detector_use_distinct_render_count_rules() {
    // 0x10001c460 uses buffer byte size; 0x10001c514 uses render frames.
    assert_eq!(meter_feed(RENDER_POST | RENDER_SILENCE, 1, true, 2, 80),
        Some(MeterFeed::Interleaved { channels: 2, frames: 10 }));
    assert_eq!(meter_feed(RENDER_POST, 2, true, 1, 20),
        Some(MeterFeed::StereoPlanar { frames: 5 }));
    assert_eq!(meter_feed(RENDER_POST, 2, true, 1, 3), None);
    assert_eq!(meter_feed(0, 1, true, 2, 80), None);
    assert_eq!(detector_feed(RENDER_POST | RENDER_SILENCE, 1, true, 2, 80, 512), Some((2, 512)));
    assert_eq!(detector_feed(RENDER_POST, 1, true, 2, 0, 512), None);
    assert_eq!(detector_feed(RENDER_POST, 2, true, 1, 20, 4), Some((2, 4)));
}

#[test]
fn callback_accounting_requires_a_zero_read_and_observes_render_flags() {
    // 0x10001c184 tests EOF before adding the current requested frame count.
    let mut analysis = FrameAccounting::new(RenderPass::Analysis, None);
    assert_eq!(analysis.source_read(512, 512).unwrap().zero_filled_frames, 0);
    let short = analysis.source_read(512, 88).unwrap();
    assert_eq!(short.zero_filled_frames, 424);
    assert!(!short.done);
    let eof = analysis.source_read(512, 0).unwrap();
    assert!(eof.done && eof.signal_silence);
    assert_eq!(analysis.actual_input_frames, 600);
    assert_eq!(analysis.requested_input_frames, 1536);
    assert!(!analysis.finish_render(512, 0, false));
    assert!(analysis.finish_render(512, RENDER_SILENCE, false));
    assert_eq!(analysis.rendered_frames, 1024);
    assert_eq!(analysis.render_sample_time, 1024.0);
    let before = analysis;
    assert_eq!(analysis.source_read(4, 5), Err(FrameError::ReadExceedsRequest));
    assert_eq!(analysis, before);
    let mut inherited = FrameAccounting::with_prior_padding(RenderPass::Analysis, None, 2);
    assert!(!inherited.source_read(1, 0).unwrap().done);
    assert!(!inherited.source_read(1, 0).unwrap().done);
    assert!(inherited.source_read(1, 0).unwrap().done);
    assert_eq!(FrameAccounting::with_prior_padding(RenderPass::Final, None, 1024).padding_frames, 0);
    assert_eq!(FrameAccounting::with_prior_padding(RenderPass::Healing, None, 9).padding_frames, 1024);

    let mut heal = FrameAccounting::new(RenderPass::Healing, Some(RecorderAccounting::new(-10, 0)));
    assert_eq!(heal.padding_frames, HEAL_PADDING_FRAMES);
    assert!(!heal.source_read(128, 100).unwrap().done);
    assert_eq!(heal.recorder.as_mut().unwrap().record_render(RENDER_POST, true, 128), Some(RecordSpan { offset: 10, frames: 118 }));
    assert!(!heal.source_read(128, 0).unwrap().done);
    for _ in 0..8 {
        heal.recorder.as_mut().unwrap().record_render(RENDER_POST, true, 128);
    }
    assert_eq!(heal.recorder.unwrap().current_frames(), 1142);
    assert!(heal.source_read(128, 0).unwrap().signal_silence);
    assert!(heal.finish_render(512, 0, false));
}

#[test]
fn recorder_shift_target_and_silent_post_behavior() {
    // 0x10001c688 and 0x100018518; target <= 0 means unrestricted.
    let mut r = RecorderAccounting::new(-3, 5);
    assert_eq!(r.record_render(RENDER_POST | RENDER_SILENCE, true, 8), None);
    assert_eq!(r.record_render(RENDER_POST, false, 8), None);
    assert_eq!(r.record_render(RENDER_POST, true, 2), None);
    assert_eq!(r.shift_remaining(), -1);
    assert_eq!(r.record_render(RENDER_POST, true, 4), Some(RecordSpan { offset: 1, frames: 3 }));
    assert_eq!(r.record_render(RENDER_POST, true, 4), Some(RecordSpan { offset: 0, frames: 2 }));
    assert_eq!(r.current_frames(), 5);
    assert_eq!(r.record_render(RENDER_POST, true, 4), None);
    let mut positive = RecorderAccounting::new(2, 100);
    assert_eq!(positive.record_render(RENDER_POST, true, 10), None);
    assert_eq!(positive.shift_remaining(), 2);
    assert_eq!(positive.current_frames(), 0);
    let mut invalid_native = RecorderAccounting::new(i64::MIN, 0);
    assert_eq!(invalid_native.record_render(RENDER_POST, true, 4), Some(RecordSpan { offset: 1_u64 << 63, frames: 4 }));
    let mut target = FrameAccounting::new(RenderPass::Final, Some(RecorderAccounting::new(0, 3)));
    assert!(!target.source_read(4, 1).unwrap().done);
    target.recorder.as_mut().unwrap().record_render(RENDER_POST, true, 4);
    assert!(target.source_read(4, 0).unwrap().done);
    assert!(target.finish_render(512, 0, false));
}
