//! Closed analysis/synthesis-leaf tests; none assert a complete pitch processor.
//! The ignored test compares with a separately run, pinned native oracle.

use platinum_engine::pitch::fft::RealFft;
use platinum_engine::pitch::vocoder::{
    self, ChannelZeroPhase, FactorState, InverseFrame, PeakState, PhaseVocoder, PropagationState, SideAnalysis, SideInverse, Stage, StageTracking, TransientState,
};
use serde_json::Value;

#[test]
fn step_and_warmup_domains_match_the_selected_scheduler() {
    assert_eq!(vocoder::decode_step(-1, 2).unwrap().stage, Stage::PrimeInput);
    assert_eq!(vocoder::decode_step(-1, 2).unwrap().channel, 0);
    assert_eq!(vocoder::decode_step(-2, 2).unwrap().channel, 1);
    assert!(vocoder::decode_step(-2, 1).is_none());
    assert!(vocoder::decode_step(-512, 2).is_none());
    assert!(vocoder::decode_step(0, 0).is_none());
    assert!(vocoder::decode_step(10, 2).is_none());
    for channel in 0..2 {
        for (phase, expected) in [
            Stage::ShiftAndForward,
            Stage::MagnitudesAndPhases,
            Stage::Peaks,
            Stage::Propagate,
            Stage::InverseAndOverlap,
        ].into_iter().enumerate() {
            let decoded = vocoder::decode_step((channel * 5 + phase) as i32, 2).unwrap();
            assert_eq!(decoded.channel, channel);
            assert_eq!(decoded.stage, expected);
        }
    }
    let mut tracking = StageTracking::default();
    assert_eq!(tracking.begin(-1, 2).unwrap().stage, Stage::PrimeInput);
    assert_eq!(tracking.current, 0);
    tracking.finish(-1);
    assert_eq!(tracking.previous, 0x200);
    tracking.begin(2, 2).unwrap();
    assert_eq!(tracking.current, 2);
    assert_eq!(tracking.previous, 0x200);
    tracking.finish(2);
    tracking.begin(3, 2).unwrap();
    assert_eq!((tracking.current, tracking.previous), (3, 2));
    tracking.finish(3);
    assert_eq!(tracking.previous, 3);
    let mut history: Vec<f32> = (0..2048).map(|i| i as f32).collect();
    let input = vec![-1.0; 256];
    let mut position = 0;
    vocoder::shift_input(0, &mut history, &input, &mut position);
    assert_eq!(position, -256);
    assert_eq!(history[0], 256.0);
    assert_eq!(history[1791], 2047.0);
    assert!(history[1792..].iter().all(|x| *x == -1.0));
    vocoder::shift_input(1, &mut history, &input, &mut position);
    assert_eq!(position, -256, "channel 1 must not move shared transient position");
    position = -1024;
    vocoder::shift_input(0, &mut history, &input, &mut position);
    assert_eq!(position, -1024, "the -N/2 boundary is exclusive");
}

#[test]
fn window_retention_and_reset_are_distinct() {
    let n = 2048;
    let cosine = vec![0.0; n];
    let mut work = vec![-7.0; n];
    let mut window = vec![-9.0; n];
    assert!(!vocoder::update_analysis_window(&cosine, &mut work, &mut window, 1.0, 0.98, 256, false));
    assert!(work.iter().all(|x| *x == -7.0));
    assert!(window.iter().all(|x| *x == -9.0));
    assert!(vocoder::update_analysis_window(&cosine, &mut work, &mut window, 0.0, 1.0, 0, false));
    let base = f32::from_bits(0x3f09_d1f6);
    let square = base * base;
    let cube = square * base;
    let fourth = cube * base;
    assert!(work.iter().all(|x| x.to_bits() == base.to_bits()));
    assert!(window.iter().all(|x| x.to_bits() == fourth.to_bits()));
    assert!(vocoder::update_analysis_window(&cosine, &mut work, &mut window, 1.0, 1.0, 256, true));
    assert!(window.iter().all(|x| x.to_bits() == base.to_bits()));
}

#[test]
fn expected_phase_advance_uses_input_hop() {
    let mut expected = vec![0.0; 2048];
    vocoder::initialize_expected_advance(&mut expected, 256);
    assert_eq!(expected[0].to_bits(), 0);
    assert_eq!(expected[1].to_bits(), 0x3f49_0fdb);
    assert_eq!(expected[2].to_bits(), 0x3fc9_0fdb);
}

#[test]
fn initial_bin_estimates_apply_native_floor_and_bounds() {
    let advance = [0.0, -0.01, 2.356_194_5, 1.0e8, f32::NAN];
    let mut bins = [99; 5];
    vocoder::estimate_frequency_bins(2048, 256, advance.len(), &advance, &mut bins);
    assert_eq!(bins, [0, 1, 3, 1023, 4]);
}

#[test]
fn independent_peak_mode_maps_bins_to_themselves() {
    let half = 1024;
    let active = 1000;
    let advance = vec![0.0; half];
    let magnitude = vec![0.0; half + 1];
    let flags = vec![0; half];
    let mut peaks = vec![-1; half];
    let mut assignments = vec![-2; half];
    let mut scratch = vec![-3; half];
    let count = vocoder::identify_peaks(PeakState {
        frame_len: 2048, input_hop: 256, active_bins: active, independent_bins: true,
        instantaneous_advance: &advance, magnitude: &magnitude, previous_flags: &flags,
        peaks: &mut peaks, assignments: &mut assignments, scratch: &mut scratch,
    });
    assert_eq!(count, active);
    for i in 0..active {
        assert_eq!(peaks[i], i as i32);
        assert_eq!(assignments[i], i as i32);
    }
    assert_eq!(peaks[active], -1);
    assert_eq!(assignments[active], -2);
    assert_eq!(scratch[0], 1);
    assert_eq!(scratch[1], -3);
}

#[test]
fn factor_update_preserves_state_when_hops_do_not_change() {
    let n = 2048;
    let cosine = vec![0.0; n];
    let mut work = vec![0.0; n];
    let mut expected = vec![0.0; n];
    let mut analysis = vec![0.0; n];
    let mut synthesis = vec![0.0; n];
    let mut normalization = vec![0.0; n];
    let mut side = vec![0.0; n];
    let mut state = FactorState {
        previous_factor: 0.0, target_factor: 1.0, input_hop: 0, internal_hop: 0, output_hop: 0,
        frame_count: 0, relative_transient: -137, aligned_transient: 0, normalization_position: 0,
        peak_gain: 0.0, bypass_propagation: true, side_taper_enabled: true,
        cosine: &cosine, work: &mut work, expected_advance: &mut expected,
        analysis_window: &mut analysis, synthesis_window: &mut synthesis,
        normalization_window: &mut normalization, side_window: &mut side,
    };
    assert!(vocoder::update_factor_and_hops(&mut state, 1.0, 256, 256));
    assert_eq!(state.previous_factor, 1.0);
    assert_eq!(state.normalization_position, 1280);
    assert!(state.bypass_propagation);
    state.frame_count = 1;
    state.side_taper_enabled = false;
    assert!(vocoder::update_factor_and_hops(&mut state, 0.98, 251, 256));
    assert_eq!(state.previous_factor, 1.0, "the prior factor advances at the later channel transition");
    assert_eq!(state.target_factor, 0.98);
    assert!(!state.bypass_propagation);
    assert!(!vocoder::update_factor_and_hops(&mut state, 1.0, 251, 256));
    assert_eq!(state.target_factor, 0.98, "same-hop requests retain the stored target");
    assert!(!state.bypass_propagation);
}

fn bits_from_json(value: &Value, key: &str) -> Vec<u32> {
    value[key].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect()
}
fn assert_bits(name: &str, actual: &[f32], expected: Vec<u32>) {
    let actual: Vec<_> = actual.iter().map(|x| x.to_bits()).collect();
    assert_eq!(actual, expected, "{name}");
}
fn cosine_value(i: usize) -> f32 {
    ((i * 37 % 513) as i32 - 256) as f32 / 256.0
}
fn packed_value(i: usize, channel: usize, half: usize) -> f32 {
    if i == half {
        return if channel == 0 { -1.0 } else { 0.75 };
    }
    if i % 127 == 0 {
        return if i & 1 != 0 { -0.0 } else { 0.0 };
    }
    ((i * 17 + channel * 11) % 257) as f32 / 128.0 - 1.0
}
fn old_phase_value(i: usize, channel: usize) -> f32 {
    (((i + channel * 2) % 9) as i32 - 4) as f32 * 0.125
}

#[test]
#[ignore = "requires a fresh analysis-oracle JSONL capture in PITCH_VOCODER_ORACLE_OUTPUT"]
fn pinned_native_analysis_leaves_match() {
    let path = std::env::var_os("PITCH_VOCODER_ORACLE_OUTPUT").expect("oracle capture path");
    let capture = std::fs::read_to_string(path).expect("read oracle capture");
    let mut counts = [0usize; 13];
    let cosine: Vec<_> = (0..2048).map(cosine_value).collect();
    let mut work = vec![-7.0; 2048];
    let mut expected = vec![-6.0; 2048];
    let mut analysis = vec![-9.0; 2048];
    let mut synthesis = vec![-8.0; 2048];
    let mut normalization = vec![-5.0; 2048];
    let mut side = vec![-4.0; 2048];
    let mut factor_state = FactorState {
        previous_factor: 0.0, target_factor: 1.0, input_hop: 0, internal_hop: 0, output_hop: 0,
        frame_count: 0, relative_transient: -137, aligned_transient: 0, normalization_position: 0,
        peak_gain: 0.0, bypass_propagation: true, side_taper_enabled: true,
        cosine: &cosine, work: &mut work, expected_advance: &mut expected,
        analysis_window: &mut analysis, synthesis_window: &mut synthesis,
        normalization_window: &mut normalization, side_window: &mut side,
    };
    let mut peak_values = vec![0i32; 1024];
    let mut peak_assignments = vec![0i32; 1024];
    let mut peak_scratch = vec![0i32; 1024];
    let mut stream_lane = None;
    let mut stream: Option<PhaseVocoder> = None;
    for line in capture.lines() {
        let record: Value = serde_json::from_str(line).expect("oracle JSONL record");
        match record["kind"].as_str().unwrap() {
            "window" => {
                counts[0] += 1;
                let n = record["n"].as_u64().unwrap() as usize;
                let old = f32::from_bits(record["old"].as_u64().unwrap() as u32);
                let new = f32::from_bits(record["new"].as_u64().unwrap() as u32);
                let hop = record["hop"].as_u64().unwrap() as usize;
                let simple = record["simple"].as_bool().unwrap();
                let name = record["name"].as_str().unwrap();
                let cosine: Vec<_> = (0..n).map(cosine_value).collect();
                let mut work = vec![-7.0; n];
                let mut window = vec![-9.0; n];
                let changed = vocoder::update_analysis_window(&cosine, &mut work, &mut window, old, new, hop, simple);
                assert_eq!(changed, name != "retained", "{name}");
                assert_bits(name, &work, bits_from_json(&record, "work"));
                assert_bits(name, &window, bits_from_json(&record, "window"));
            }
            "wrap" => {
                counts[1] += 1;
                let input: Vec<_> = bits_from_json(&record, "input").into_iter().map(f32::from_bits).collect();
                let mut output = vec![0.0; input.len()];
                let mut inplace = input.clone();
                vocoder::wrap_phase(&input, &mut output);
                vocoder::wrap_phase_in_place(&mut inplace);
                assert_bits("phase wrap", &output, bits_from_json(&record, "output"));
                assert_bits("in-place phase wrap", &inplace, bits_from_json(&record, "inplace"));
            }
            "phase" => {
                counts[2] += 1;
                let n = record["n"].as_u64().unwrap() as usize;
                let half = n / 2;
                let mut packed0: Vec<_> = (0..n).map(|i| packed_value(i, 0, half)).collect();
                let mut packed1: Vec<_> = (0..n).map(|i| packed_value(i, 1, half)).collect();
                let mut prev0: Vec<_> = (0..=half).map(|i| old_phase_value(i, 0)).collect();
                let mut prev1: Vec<_> = (0..=half).map(|i| old_phase_value(i, 1)).collect();
                let mut mag0 = vec![0.0; half + 1];
                let mut mag1 = vec![0.0; half + 1];
                let mut scratch = vec![-77.0; half + 1];
                let expected: Vec<_> = (0..half).map(|i| (i % 41) as f32 * 0.125).collect();
                let mut delta = vec![-88.0; half];
                let mut history: Vec<_> = (0..32).map(|j| vec![10.0 + j as f32 * 0.25; half + 1]).collect();
                vocoder::extract_phase_and_magnitude(0, &mut packed0, &mut prev0, &mut mag0, &mut scratch,
                    Some(ChannelZeroPhase { expected_advance: &expected, instantaneous_advance: &mut delta, magnitude_history: &mut history }));
                vocoder::extract_phase_and_magnitude(1, &mut packed1, &mut prev1, &mut mag1, &mut scratch, None);
                for (key, actual) in [("packed0", &packed0), ("packed1", &packed1), ("prev0", &prev0), ("prev1", &prev1), ("mag0", &mag0), ("mag1", &mag1), ("scratch", &scratch), ("delta", &delta), ("history0", &history[0])] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
                assert_eq!(history[1][0].to_bits(), record["history1_first"].as_u64().unwrap() as u32);
                assert_eq!(history[31][0].to_bits(), record["history31_first"].as_u64().unwrap() as u32);
            }
            "forward" => {
                counts[3] += 1;
                let n = record["n"].as_u64().unwrap() as usize;
                let input0: Vec<_> = (0..n).map(|i| (((i * 29) % 257) as i32 - 128) as f32 / 256.0).collect();
                let input1: Vec<_> = (0..n).map(|i| (((i * 29 + 7) % 257) as i32 - 128) as f32 / 256.0).collect();
                let window: Vec<_> = (0..n).map(|i| (40 + i % 97) as f32 / 128.0).collect();
                let ramp: Vec<_> = (0..n).map(|i| {
                    let scaled = (2.0f32 / n as f32) * i as f32;
                    scaled + -1.0
                }).collect();
                let mut fft = RealFft::new(n).expect("FFT setup");
                let mut packed0 = vec![0.0; n];
                let mut packed1 = vec![0.0; n];
                let mut weighted = vec![-5.0; n];
                let mut scratch = vec![0.0; n];
                let mut capture = vec![0.0; n];
                vocoder::forward_analysis(&mut fft, 0, &input0, &window, &ramp, &mut packed0, &mut weighted, &mut scratch, &mut capture);
                assert_bits("weighted0", &weighted, bits_from_json(&record, "weighted0"));
                assert_bits("scratch0", &scratch, bits_from_json(&record, "scratch0"));
                assert_bits("capture0", &capture, bits_from_json(&record, "capture0"));
                vocoder::forward_analysis(&mut fft, 1, &input1, &window, &ramp, &mut packed1, &mut weighted, &mut scratch, &mut capture);
                for (key, actual) in [("packed0", &packed0), ("packed1", &packed1), ("weighted1", &weighted), ("scratch1", &scratch), ("capture1", &capture)] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
            }
            "inverse" => {
                counts[4] += 1;
                let n = record["n"].as_u64().unwrap() as usize;
                let hop = record["hop"].as_u64().unwrap() as usize;
                let bypass = record["bypass"].as_bool().unwrap();
                let mut position = record["position"].as_i64().unwrap() as i32;
                let input0: Vec<_> = (0..n).map(|i| (((i * 11) % 129) as i32 - 64) as f32 / 256.0).collect();
                let input1: Vec<_> = (0..n).map(|i| (((i * 11 + 3) % 129) as i32 - 64) as f32 / 256.0).collect();
                let mut synth: Vec<_> = (0..n).map(|i| (((i * 7) % 193) as i32 - 96) as f32 / 256.0).collect();
                let synth1: Vec<_> = (0..n).map(|i| (((i * 13 + 5) % 193) as i32 - 96) as f32 / 256.0).collect();
                let window: Vec<_> = (0..n).map(|i| (40 + i % 97) as f32 / 128.0).collect();
                let mut out0: Vec<_> = (0..n).map(|i| ((i % 7) as i32 - 3) as f32 / 128.0).collect();
                let mut out1 = out0.clone();
                let norm_window: Vec<_> = (0..n).map(|i| ((i % 11) as i32 - 3) as f32 / 256.0).collect();
                let mut history: Vec<_> = (0..n).map(|i| ((i % 9) as i32 - 3) as f32 / 256.0).collect();
                let mut gain = vec![0.25; n];
                let mut fft = RealFft::new(n).expect("FFT setup");
                vocoder::inverse_and_overlap(&mut fft, InverseFrame {
                    channel: 0, bypass_propagation: bypass, input_packed: &input0, synthesis_packed: &mut synth,
                    synthesis_window: &window, output_overlap: &mut out0, normalization_window: &norm_window,
                    normalization_history: &mut history, normalization_gain: &mut gain, normalization_position: &mut position,
                    output_hop: hop, internal_hop: hop,
                });
                assert_bits("synth0", &synth, bits_from_json(&record, "synth0"));
                assert_bits("out0", &out0, bits_from_json(&record, "out0"));
                assert_bits("history0", &history, bits_from_json(&record, "history0"));
                assert_bits("gain0", &gain, bits_from_json(&record, "gain0"));
                assert_eq!(position, record["pos0"].as_i64().unwrap() as i32);
                synth.copy_from_slice(&synth1);
                vocoder::inverse_and_overlap(&mut fft, InverseFrame {
                    channel: 1, bypass_propagation: bypass, input_packed: &input1, synthesis_packed: &mut synth,
                    synthesis_window: &window, output_overlap: &mut out1, normalization_window: &norm_window,
                    normalization_history: &mut history, normalization_gain: &mut gain, normalization_position: &mut position,
                    output_hop: hop, internal_hop: hop,
                });
                for (key, actual) in [("synth1", &synth), ("out1", &out1), ("history1", &history), ("gain1", &gain)] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
                assert_eq!(position, record["pos1"].as_i64().unwrap() as i32);
            }
            "factor" => {
                counts[5] += 1;
                let name = record["name"].as_str().unwrap();
                if name == "change" {
                    factor_state.frame_count = 1;
                    factor_state.side_taper_enabled = false;
                }
                let requested = f32::from_bits(record["request"].as_u64().unwrap() as u32);
                let input = record["input"].as_u64().unwrap() as usize;
                let output = record["output"].as_u64().unwrap() as usize;
                let rebuilt = vocoder::update_factor_and_hops(&mut factor_state, requested, input, output);
                assert_eq!(rebuilt, name != "same_hop");
                assert_eq!(factor_state.previous_factor.to_bits(), record["old"].as_u64().unwrap() as u32);
                assert_eq!(factor_state.target_factor.to_bits(), record["target"].as_u64().unwrap() as u32);
                assert_eq!(factor_state.input_hop, record["in_hop"].as_u64().unwrap() as usize);
                assert_eq!(factor_state.internal_hop, record["internal_hop"].as_u64().unwrap() as usize);
                assert_eq!(factor_state.output_hop, record["out_hop"].as_u64().unwrap() as usize);
                assert_eq!(factor_state.normalization_position, record["norm_pos"].as_i64().unwrap() as i32);
                assert_eq!(factor_state.aligned_transient, record["aligned"].as_i64().unwrap() as i32);
                assert_eq!(factor_state.peak_gain.to_bits(), record["gain"].as_u64().unwrap() as u32);
                assert_eq!(factor_state.bypass_propagation, record["bypass"].as_u64().unwrap() != 0);
                for (key, actual) in [
                    ("work", &*factor_state.work), ("expected", &*factor_state.expected_advance),
                    ("analysis", &*factor_state.analysis_window), ("synthesis", &*factor_state.synthesis_window),
                    ("normalization", &*factor_state.normalization_window), ("side", &*factor_state.side_window),
                ] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
            }
            "constructor" => {
                counts[6] += 1;
                let n = record["n"].as_u64().unwrap() as usize;
                let mut phase = vec![0.0; n];
                let mut sine = vec![0.0; n];
                let mut cosine = vec![0.0; n];
                let mut ramp = vec![0.0; n];
                vocoder::initialize_analysis_tables(&mut phase, &mut sine, &mut cosine, &mut ramp);
                for (key, actual) in [("phase", &phase), ("sine", &sine), ("cosine", &cosine), ("ramp", &ramp)] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
            }
            "side_analysis" => {
                counts[7] += 1;
                let n = record["n"].as_u64().unwrap() as usize;
                let hop = record["hop"].as_u64().unwrap() as usize;
                let input0: Vec<_> = (0..hop).map(|i| (((i * 7) % 129) as i32 - 64) as f32 / 256.0).collect();
                let input1: Vec<_> = (0..hop).map(|i| (((i * 11) % 129) as i32 - 64) as f32 / 256.0).collect();
                let mut hist0: Vec<_> = (0..n).map(|i| ((i % 13) as i32 - 6) as f32 / 256.0).collect();
                let mut hist1: Vec<_> = (0..n).map(|i| ((i % 17) as i32 - 8) as f32 / 256.0).collect();
                let window: Vec<_> = (0..n).map(|i| (40 + i % 97) as f32 / 128.0).collect();
                let mut packed = vec![0.0; n];
                let mut side_mask = vec![-5.0; n];
                let mut main_mask = vec![-4.0; n];
                let mut fft = RealFft::new(n).expect("FFT setup");
                vocoder::analyze_side(&mut fft, SideAnalysis {
                    channel: 0, input: &input0, history: &mut hist0, window: &window, packed: &mut packed,
                    side_mask: &mut side_mask, main_mask: &mut main_mask,
                });
                for (key, actual) in [("hist0", &hist0), ("packed0", &packed), ("side0", &side_mask), ("main0", &main_mask)] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
                vocoder::analyze_side(&mut fft, SideAnalysis {
                    channel: 1, input: &input1, history: &mut hist1, window: &window, packed: &mut packed,
                    side_mask: &mut side_mask, main_mask: &mut main_mask,
                });
                for (key, actual) in [("hist1", &hist1), ("packed1", &packed), ("side1", &side_mask), ("main1", &main_mask)] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
            }
            "side_output" => {
                counts[8] += 1;
                let n = record["n"].as_u64().unwrap() as usize;
                let hop = record["hop"].as_u64().unwrap() as usize;
                let bypass = record["bypass"].as_bool().unwrap();
                let input0: Vec<_> = (0..n).map(|i| (((i * 11) % 129) as i32 - 64) as f32 / 256.0).collect();
                let input1: Vec<_> = (0..n).map(|i| (((i * 11 + 3) % 129) as i32 - 64) as f32 / 256.0).collect();
                let mut synth: Vec<_> = (0..n).map(|i| (((i * 7) % 193) as i32 - 96) as f32 / 256.0).collect();
                let synth1: Vec<_> = (0..n).map(|i| (((i * 13 + 5) % 193) as i32 - 96) as f32 / 256.0).collect();
                let mut packed: Vec<_> = (0..n).map(|i| (((i * 5) % 129) as i32 - 64) as f32 / 256.0).collect();
                let packed1: Vec<_> = (0..n).map(|i| (((i * 9 + 7) % 129) as i32 - 64) as f32 / 256.0).collect();
                let mut main0: Vec<_> = (0..n).map(|i| ((i % 7) as i32 - 3) as f32 / 128.0).collect();
                let mut main1 = main0.clone();
                let mut side0: Vec<_> = (0..n).map(|i| ((i % 11) as i32 - 5) as f32 / 256.0).collect();
                let mut side1: Vec<_> = (0..n).map(|i| ((i % 13) as i32 - 6) as f32 / 256.0).collect();
                let mut history0: Vec<_> = (0..n).map(|i| ((i % 17) as i32 - 8) as f32 / 256.0).collect();
                let mut history1: Vec<_> = (0..n).map(|i| ((i % 19) as i32 - 9) as f32 / 256.0).collect();
                let window: Vec<_> = (0..n).map(|i| (40 + i % 97) as f32 / 128.0).collect();
                let mut side_window: Vec<_> = (0..n).map(|i| (20 + i % 43) as f32 / 64.0).collect();
                let norm_window: Vec<_> = (0..n).map(|i| ((i % 11) as i32 - 3) as f32 / 256.0).collect();
                let mut norm_history: Vec<_> = (0..n).map(|i| ((i % 9) as i32 - 3) as f32 / 256.0).collect();
                let mut norm_gain = vec![0.25; n];
                let mut position = (n / 2 + hop) as i32;
                let mut result0 = vec![0.0; hop];
                let mut result1 = vec![0.0; hop];
                let mut fft = RealFft::new(n).expect("FFT setup");
                vocoder::inverse_and_overlap(&mut fft, InverseFrame {
                    channel: 0, bypass_propagation: bypass, input_packed: &input0, synthesis_packed: &mut synth,
                    synthesis_window: &window, output_overlap: &mut main0, normalization_window: &norm_window,
                    normalization_history: &mut norm_history, normalization_gain: &mut norm_gain,
                    normalization_position: &mut position, output_hop: hop, internal_hop: hop,
                });
                vocoder::inverse_side_and_advance(&mut fft, SideInverse {
                    packed: &mut packed, window: &side_window, side_overlap: &mut side0, side_history: &mut history0,
                    output_overlap: &mut main0, bypass_propagation: bypass, output_hop: hop, internal_hop: hop,
                });
                vocoder::finish_output(0, 2, position, hop, &mut main0, &mut result0, &mut side_window);
                synth.copy_from_slice(&synth1);
                packed.copy_from_slice(&packed1);
                vocoder::inverse_and_overlap(&mut fft, InverseFrame {
                    channel: 1, bypass_propagation: bypass, input_packed: &input1, synthesis_packed: &mut synth,
                    synthesis_window: &window, output_overlap: &mut main1, normalization_window: &norm_window,
                    normalization_history: &mut norm_history, normalization_gain: &mut norm_gain,
                    normalization_position: &mut position, output_hop: hop, internal_hop: hop,
                });
                vocoder::inverse_side_and_advance(&mut fft, SideInverse {
                    packed: &mut packed, window: &side_window, side_overlap: &mut side1, side_history: &mut history1,
                    output_overlap: &mut main1, bypass_propagation: bypass, output_hop: hop, internal_hop: hop,
                });
                vocoder::finish_output(1, 2, position, hop, &mut main1, &mut result1, &mut side_window);
                assert_eq!(position, record["position"].as_i64().unwrap() as i32);
                for (key, actual) in [
                    ("result0", &result0), ("result1", &result1), ("main0", &main0), ("main1", &main1),
                    ("side0", &side0), ("side1", &side1), ("history0", &history0), ("history1", &history1),
                    ("norm_history", &norm_history), ("norm_gain", &norm_gain), ("side_window", &side_window),
                ] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
            }
            "peaks" => {
                counts[9] += 1;
                let n = record["n"].as_u64().unwrap() as usize;
                let hop = record["hop"].as_u64().unwrap() as usize;
                let active = record["active"].as_u64().unwrap() as usize;
                let frame = record["frame"].as_u64().unwrap() as usize;
                assert_eq!(counts[9], frame + 1, "peak history records must be in order");
                let independent = record["independent"].as_bool().unwrap();
                let half = n / 2;
                let advance: Vec<_> = (0..half).map(|i| {
                    let offset = (((i / 13 + frame) % 3) as i32 - 1) * 3;
                    (i as i32 + offset) as f32 * 0.75
                }).collect();
                let mut magnitude: Vec<_> = (0..half).map(|i| ((i * 17 + frame * 5) % 41 + 1) as f32 / 64.0).collect();
                magnitude.push(0.5);
                let flags: Vec<_> = (0..half).map(|i| {
                    if (i + frame * 3) % 37 == 0 { 2 }
                    else if (i + frame) % 61 == 0 { 3 }
                    else { 0 }
                }).collect();
                let count = vocoder::identify_peaks(PeakState {
                    frame_len: n, input_hop: hop, active_bins: active, independent_bins: independent,
                    instantaneous_advance: &advance, magnitude: &magnitude, previous_flags: &flags,
                    peaks: &mut peak_values, assignments: &mut peak_assignments, scratch: &mut peak_scratch,
                });
                assert_eq!(count, record["count"].as_u64().unwrap() as usize);
                for (key, actual) in [("peaks", &peak_values), ("assignments", &peak_assignments), ("scratch", &peak_scratch)] {
                    let expected: Vec<_> = record[key].as_array().unwrap().iter().map(|v| v.as_i64().unwrap() as i32).collect();
                    assert_eq!(actual, &expected, "{key} frame {frame}");
                }
            }
            "transient" => {
                counts[10] += 1;
                let c = record["case"].as_u64().unwrap() as usize;
                let n = 2048usize;
                let half = n / 2;
                let peak_count = 80usize;
                let peaks: Vec<_> = (0..half).map(|i| (1 + (i % peak_count) * 12) as i32).collect();
                let assignments: Vec<_> = (0..half).map(|i| (1 + (i / 12).min(peak_count - 1) * 12) as i32).collect();
                let mut magnitude: Vec<_> = (0..half).map(|i| ((i * 29 + c * 7) % 65 + 1) as f32 / 256.0).collect();
                magnitude.push(0.5);
                let mut weighted = vec![0.0; n];
                for i in 0..half {
                    weighted[i] = (((i * 13 + c * 5) % 41) as i32 - 20) as f32 / 8192.0;
                }
                let history: Vec<Vec<f32>> = (0..32).map(|j| {
                    (0..=half).map(|i| ((i * 7 + j * 3) % 31 + 1) as f32 / 512.0).collect()
                }).collect();
                let previous_flags: Vec<_> = (0..half).map(|i| if i % 37 == 0 { 1 } else if i % 53 == 0 { 2 } else { 0 }).collect();
                let mut flags = vec![-3; half];
                let mut difference = vec![-7.0; half];
                let mut flux = vec![-8.0; half];
                let mut energy = vec![-9.0; half];
                let mut capture: Vec<_> = (0..n).map(|i| (((i * 5) % 17) as i32 - 8) as f32 / 128.0).collect();
                capture[n - 5] = -2.0;
                let mut detected = 0;
                let mut bypass = false;
                vocoder::classify_transients(TransientState {
                    frame_len: n, input_hop: 256, frame_count: record["frame"].as_i64().unwrap() as i32,
                    previous_factor: f32::from_bits(record["previous"].as_u64().unwrap() as u32),
                    target_factor: f32::from_bits(record["target"].as_u64().unwrap() as u32),
                    independent_bins: record["independent"].as_bool().unwrap(),
                    relative_position: record["relative"].as_i64().unwrap() as i32,
                    aligned_position: record["aligned"].as_i64().unwrap() as i32,
                    external_position_enabled: record["external"].as_bool().unwrap(),
                    detected_position: &mut detected, bypass_propagation: &mut bypass,
                    peaks: &peaks, peak_count, assignments: &assignments, magnitude: &magnitude,
                    magnitude_history: &history, weighted_spectrum: &weighted, peak_difference: &mut difference,
                    spectral_flux: &mut flux, peak_energy: &mut energy, flags: &mut flags,
                    previous_flags: &previous_flags, transient_capture: &mut capture,
                });
                assert_eq!(detected, record["detected"].as_i64().unwrap() as i32);
                assert_eq!(bypass, record["bypass"].as_u64().unwrap() != 0);
                let expected_flags: Vec<_> = record["flags"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap() as i32).collect();
                assert_eq!(flags, expected_flags);
                for (key, actual) in [("difference", &difference), ("flux", &flux), ("energy", &energy), ("capture", &capture)] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
            }
            "propagation" => {
                counts[11] += 1;
                let c = record["case"].as_u64().unwrap() as usize;
                let frame = record["frame"].as_i64().unwrap() as i32;
                let prior_stage = record["prior"].as_i64().unwrap() as i32;
                let target = f32::from_bits(record["target"].as_u64().unwrap() as u32);
                let side_enabled = record["side"].as_bool().unwrap();
                let external = record["external"].as_bool().unwrap();
                let n = 2048usize;
                let half = n / 2;
                let count = 80usize;
                let active = 1000usize;
                let cutoff = 700usize;
                let packed0: Vec<_> = (0..n).map(|i| (((i * 11) % 129) as i32 - 64) as f32 / 256.0).collect();
                let packed1: Vec<_> = (0..n).map(|i| (((i * 11 + 3) % 129) as i32 - 64) as f32 / 256.0).collect();
                let phase0: Vec<_> = (0..=half).map(|i| ((i % 9) as i32 - 4) as f32 * 0.125).collect();
                let phase1: Vec<_> = (0..=half).map(|i| (((i + 2) % 9) as i32 - 4) as f32 * 0.125).collect();
                let mut prior0: Vec<_> = (0..=half).map(|i| ((i % 7) as i32 - 3) as f32 * 0.125).collect();
                let mut prior1: Vec<_> = (0..=half).map(|i| (((i + 1) % 7) as i32 - 3) as f32 * 0.125).collect();
                let mag0: Vec<_> = (0..=half).map(|i| ((i * 29 + c * 7) % 65 + 1) as f32 / 256.0).collect();
                let mag1: Vec<_> = (0..=half).map(|i| ((i * 19 + c * 3) % 65 + 1) as f32 / 256.0).collect();
                let mut held0: Vec<_> = (0..=half).map(|i| (i % 31 + 1) as f32 / 512.0).collect();
                let mut held1: Vec<_> = (0..=half).map(|i| (i % 29 + 1) as f32 / 512.0).collect();
                let expected: Vec<_> = (0..n).map(|i| (i % 41) as f32 * 0.125).collect();
                let mut instant: Vec<_> = (0..half).map(|i| (i % 19) as f32 * 0.0625).collect();
                let mut peak_phase = vec![-5.0; half];
                let mut synthesis_phase = vec![-6.0; half + 1];
                let mut rotation = vec![-7.0; half];
                let mut cosine = vec![-8.0; half];
                let mut sine = vec![-9.0; half];
                let mut angles = vec![-10.0; half];
                let mut sine_work = vec![-11.0; half];
                let mut cosine_work = vec![-12.0; half];
                let mut difference = vec![-7.0; half];
                let mut flux = vec![-0.25; half];
                let mut energy = vec![-9.0; half];
                let weighted: Vec<_> = (0..n).map(|i| (((i * 13 + c * 5) % 41) as i32 - 20) as f32 / 8192.0).collect();
                let mut capture: Vec<_> = (0..n).map(|i| (((i * 5) % 17) as i32 - 8) as f32 / 128.0).collect();
                capture[n - 5] = -2.0;
                let norm_window: Vec<_> = (0..n).map(|i| (128 + i % 17) as f32 / 256.0).collect();
                let mut side_mask = vec![0.0; n];
                side_mask[0] = 1.0; side_mask[1] = 1.0;
                let mut main_mask = vec![1.0; n];
                let peaks: Vec<_> = (0..half).map(|i| (1 + (i % count) * 12) as i32).collect();
                let assignments: Vec<_> = (0..half).map(|i| (1 + (i / 12).min(count - 1) * 12) as i32).collect();
                let mut flags = vec![0i32; half];
                for j in 0..count { flags[peaks[j] as usize] = (j % 4) as i32; }
                let mut previous_flags: Vec<_> = (0..half).map(|i| if i % 37 == 0 { 1 } else if i % 53 == 0 { 2 } else { 0 }).collect();
                let history: Vec<Vec<f32>> = (0..32).map(|j| {
                    (0..=half).map(|i| ((i * 7 + j * 3) % 31 + 1) as f32 / 512.0).collect()
                }).collect();
                let mut detected = 20;
                let mut bypass = false;
                let mut previous_factor = 1.0;
                if prior_stage < 3 {
                    vocoder::classify_transients(TransientState {
                        frame_len:n, input_hop:256, frame_count:frame, previous_factor, target_factor:target,
                        independent_bins:false, relative_position:32, aligned_position:32, external_position_enabled:external,
                        detected_position:&mut detected, bypass_propagation:&mut bypass, peaks:&peaks, peak_count:count,
                        assignments:&assignments, magnitude:&mag0, magnitude_history:&history, weighted_spectrum:&weighted,
                        peak_difference:&mut difference, spectral_flux:&mut flux, peak_energy:&mut energy,
                        flags:&mut flags, previous_flags:&previous_flags, transient_capture:&mut capture,
                    });
                }
                let mut out = vec![0.0; n];
                vocoder::propagate_phases(PropagationState {
                    frame_len:n, channel:0, channels:2, active_bins:active, side_cutoff:cutoff, side_enabled,
                    frame_count:frame, current_stage:3, previous_stage:prior_stage, previous_factor:&mut previous_factor,
                    target_factor:target, detected_position:&mut detected, peak_gain:1.0, peaks:&peaks, peak_count:count,
                    assignments:&assignments, flags:&mut flags, previous_flags:&mut previous_flags, current_phase:&phase0,
                    synthesis_history:&mut prior0, magnitude:&mag0, held_magnitude:&mut held0, packed:&packed0, output:&mut out,
                    expected_advance:&expected, instantaneous_advance:&mut instant, synthesis_peak_phase:&mut synthesis_phase,
                    rotation_phase:&mut rotation, rotation_cosine:&mut cosine, rotation_sine:&mut sine,
                    peak_phase:&mut peak_phase, sincos_angles:&mut angles, sincos_sine:&mut sine_work, sincos_cosine:&mut cosine_work,
                    spectral_flux:&flux, normalization_window:&norm_window, side_mask:&mut side_mask, main_mask:&mut main_mask,
                });
                let out0 = out.clone();
                vocoder::propagate_phases(PropagationState {
                    frame_len:n, channel:1, channels:2, active_bins:active, side_cutoff:cutoff, side_enabled,
                    frame_count:frame, current_stage:3, previous_stage:3, previous_factor:&mut previous_factor,
                    target_factor:target, detected_position:&mut detected, peak_gain:1.0, peaks:&peaks, peak_count:count,
                    assignments:&assignments, flags:&mut flags, previous_flags:&mut previous_flags, current_phase:&phase1,
                    synthesis_history:&mut prior1, magnitude:&mag1, held_magnitude:&mut held1, packed:&packed1, output:&mut out,
                    expected_advance:&expected, instantaneous_advance:&mut instant, synthesis_peak_phase:&mut synthesis_phase,
                    rotation_phase:&mut rotation, rotation_cosine:&mut cosine, rotation_sine:&mut sine,
                    peak_phase:&mut peak_phase, sincos_angles:&mut angles, sincos_sine:&mut sine_work, sincos_cosine:&mut cosine_work,
                    spectral_flux:&flux, normalization_window:&norm_window, side_mask:&mut side_mask, main_mask:&mut main_mask,
                });
                assert_eq!(previous_factor.to_bits(), record["previous_after"].as_u64().unwrap() as u32);
                assert_eq!(detected, record["detected"].as_i64().unwrap() as i32);
                assert_eq!(bypass, record["bypass"].as_u64().unwrap() != 0);
                for (key, actual) in [
                    ("out0",&out0), ("out1",&out), ("prior0",&prior0), ("prior1",&prior1), ("held0",&held0), ("held1",&held1),
                    ("instant",&instant), ("synthesis_phase",&synthesis_phase), ("rotation",&rotation), ("cosine",&cosine), ("sine",&sine),
                    ("peak_phase",&peak_phase), ("angles",&angles), ("sine_work",&sine_work), ("cosine_work",&cosine_work),
                    ("difference",&difference), ("flux",&flux), ("energy",&energy), ("capture",&capture), ("side_mask",&side_mask), ("main_mask",&main_mask),
                ] {
                    assert_bits(key, actual, bits_from_json(&record, key));
                }
                for (key, actual) in [("flags",&flags), ("previous_flags",&previous_flags)] {
                    let expected: Vec<_> = record[key].as_array().unwrap().iter().map(|v| v.as_i64().unwrap() as i32).collect();
                    assert_eq!(actual, &expected, "{key}");
                }
            }
            "stream" => {
                counts[12] += 1;
                let lane = record["lane"].as_u64().unwrap() as usize;
                let n = record["n"].as_u64().unwrap() as usize;
                let side_enabled = record["side"].as_bool().unwrap();
                let frame = record["frame"].as_u64().unwrap() as usize;
                let input_hop = record["input_hop"].as_u64().unwrap() as usize;
                let output_hop = record["output_hop"].as_u64().unwrap() as usize;
                if stream_lane != Some(lane) {
                    assert_eq!(frame, 0);
                    stream_lane = Some(lane);
                    stream = Some(PhaseVocoder::new(n, 2).expect("vocoder setup"));
                    if side_enabled {
                        let core = stream.as_mut().unwrap();
                        core.set_side_enabled(true);
                        core.set_side_taper_enabled(true);
                    }
                }
                let core = stream.as_mut().unwrap();
                if frame == 4 { core.set_factor_hops(0.98, input_hop, output_hop); }
                if frame == 6 { core.set_factor_hops(1.02, input_hop, output_hop); }
                if frame == 9 {
                    core.reset(input_hop, output_hop);
                    if side_enabled {
                        core.set_side_enabled(true);
                        core.set_side_taper_enabled(true);
                    }
                }
                if frame == 12 { core.set_factor_hops(1.0, input_hop, output_hop); }
                let mut input0 = vec![0.0; input_hop.max(output_hop)];
                let mut input1 = vec![0.0; input_hop.max(output_hop)];
                for i in 0..input_hop {
                    input0[i] = (((i * 7 + frame * 11) % 257) as i32 - 128) as f32 / 256.0;
                    input1[i] = (((i * 7 + frame * 11 + 17) % 257) as i32 - 128) as f32 / 256.0;
                }
                let mut side0: Vec<_> = (0..output_hop).map(|i| (((i * 3 + frame * 7) % 129) as i32 - 64) as f32 / 256.0).collect();
                let mut side1: Vec<_> = (0..output_hop).map(|i| (((i * 5 + frame * 7 + 3) % 129) as i32 - 64) as f32 / 256.0).collect();
                let mut result0 = vec![0.0; output_hop];
                let mut result1 = vec![0.0; output_hop];
                for operation in 0..10i32 {
                    if side_enabled {
                        let mut main: [&mut [f32]; 2] = [&mut input0, &mut input1];
                        let mut sides: [&mut [f32]; 2] = [&mut side0, &mut side1];
                        assert_eq!(core.process_step_in_place(operation, &mut main, Some(&mut sides)), output_hop);
                    } else {
                        let input: [&[f32]; 2] = [&input0[..input_hop], &input1[..input_hop]];
                        let mut output: [&mut [f32]; 2] = [&mut result0, &mut result1];
                        assert_eq!(core.process_step(operation, &input, None, &mut output), output_hop);
                    }
                }
                if side_enabled {
                    result0.copy_from_slice(&input0[..output_hop]);
                    result1.copy_from_slice(&input1[..output_hop]);
                }
                let state = core.state();
                assert_eq!(state.frame_count, record["frame_count"].as_i64().unwrap() as i32);
                assert_eq!(state.peak_count, record["peak_count"].as_u64().unwrap() as usize);
                assert_eq!(state.stages.current, record["stage"].as_i64().unwrap() as i32);
                assert_eq!(state.stages.previous, record["previous_stage"].as_i64().unwrap() as i32);
                assert_eq!(state.previous_factor.to_bits(), record["previous_factor"].as_u64().unwrap() as u32);
                assert_eq!(state.target_factor.to_bits(), record["target_factor"].as_u64().unwrap() as u32);
                assert_eq!(state.peak_gain.to_bits(), record["peak_gain"].as_u64().unwrap() as u32);
                assert_eq!(state.relative_position, record["relative"].as_i64().unwrap() as i32);
                assert_eq!(state.aligned_position, record["aligned"].as_i64().unwrap() as i32);
                assert_eq!(state.detected_position, record["detected"].as_i64().unwrap() as i32);
                assert_eq!(state.normalization_position, record["norm_pos"].as_i64().unwrap() as i32);
                assert_eq!(state.bypass_propagation, record["bypass"].as_u64().unwrap() != 0);
                assert_bits("stream left", &result0, bits_from_json(&record, "left"));
                assert_bits("stream right", &result1, bits_from_json(&record, "right"));
            }
            other => panic!("unexpected oracle case {other}"),
        }
    }
    assert_eq!(counts, [7, 1, 2, 1, 3, 4, 2, 1, 2, 4, 5, 3, 42], "capture must contain all native oracle cases");
}
