//! Structural and opt-in reference comparisons for the four shipped graphs.
//! The captured-output comparisons must be run explicitly and do not imply
//! complete-file identity.

use std::io::{BufRead, BufReader};
use platinum_engine::dynamics::DynamicsField;
use platinum_engine::fp_mode::with_flush_to_zero;
use platinum_engine::multipass::{
    CrossoverFrequencyScale, LaneModelField, ShippedEffectParameters, ShippedPreset,
    TransientModelField, automation_slot_for_host_parameter,
};
use serde_json::Value;

#[test]
fn fixed_graph_latencies_keep_published_values_above_kernel_clamp() {
    use platinum_engine::multipass_graph::MultipassGraph;
    for (rate, dynamics, warmth) in [
        (8_000.0, 23, 26), (22_050.0, 64, 67), (44_100.0, 128, 131),
        (48_000.0, 139, 142), (96_000.0, 279, 282),
        (192_000.0, 557, 560), (384_000.0, 1115, 1118),
    ] {
        assert_eq!(MultipassGraph::new(rate, ShippedPreset::Gain).unwrap().latency_frames(), 0);
        assert_eq!(MultipassGraph::new(rate, ShippedPreset::Dynamics).unwrap().latency_frames(), dynamics);
        assert_eq!(MultipassGraph::new(rate, ShippedPreset::Warmth1).unwrap().latency_frames(), warmth);
        assert_eq!(MultipassGraph::new(rate, ShippedPreset::Warmth2).unwrap().latency_frames(), warmth);
    }
}

#[test]
fn fixed_graph_accepts_current_control_ids_and_rejects_invalid_chunks() {
    use platinum_engine::multipass_graph::{GraphContext, GraphError, MultipassGraph};
    assert!(matches!(MultipassGraph::new(0.0, ShippedPreset::Gain), Err(GraphError::InvalidSampleRate)));
    for preset in [ShippedPreset::Gain, ShippedPreset::Dynamics, ShippedPreset::Warmth1, ShippedPreset::Warmth2] {
        let mut graph = MultipassGraph::new(48_000.0, preset).unwrap();
        for id in [0, 1, 2, 3, 34, 40] {
            graph.set_parameter(id, 0.5).unwrap();
        }
        if preset == ShippedPreset::Dynamics {
            for id in (113..=148).chain(151..=154) {
                graph.set_parameter(id, 0.5).unwrap();
            }
        }
        assert_eq!(graph.set_parameter(200, 0.0), Err(GraphError::InvalidParameter(200)));
        let mut context = GraphContext { silent: false, bypass: false, segment_remaining: 1 };
        assert_eq!(graph.process_chunk(&mut context, &mut [], &mut []), Err(GraphError::InvalidBlock));
        assert_eq!(graph.process_chunk(&mut context, &mut [0.0; 65], &mut [0.0; 65]), Err(GraphError::InvalidBlock));
        assert_eq!(graph.process_chunk(&mut context, &mut [0.0; 1], &mut [0.0; 2]), Err(GraphError::InvalidBlock));
        context.segment_remaining = 0;
        assert_eq!(graph.process_chunk(&mut context, &mut [0.0; 1], &mut [0.0; 1]), Err(GraphError::InvalidBlock));
        context.segment_remaining = 513;
        assert_eq!(graph.process_chunk(&mut context, &mut [0.0; 1], &mut [0.0; 1]), Err(GraphError::InvalidBlock));
    }
}

fn bits(value: &Value) -> u32 {
    u32::from_str_radix(value.as_str().expect("expected hexadecimal f32 word"), 16)
        .expect("invalid hexadecimal f32 word")
}

#[test]
#[ignore = "requires four real-wrapper baseline trace captures"]
fn lane_descriptor_roundtrips_match_live_preset_models() {
    for (environment, preset) in [
        ("PLATINUM_GRAPH_GAIN_BASELINE", ShippedPreset::Gain),
        ("PLATINUM_GRAPH_DYNAMICS_BASELINE", ShippedPreset::Dynamics),
        ("PLATINUM_GRAPH_WARMTH1_BASELINE", ShippedPreset::Warmth1),
        ("PLATINUM_GRAPH_WARMTH2_BASELINE", ShippedPreset::Warmth2),
    ] {
        let path = std::env::var_os(environment).expect("missing baseline reference capture");
        let reader = BufReader::new(std::fs::File::open(path).expect("open reference capture"));
        let mut identity = false;
        let mut seen = false;
        for line in reader.lines() {
            let record: Value = serde_json::from_str(&line.expect("read capture")).expect("JSONL");
            if record["record"] == "identity" {
                assert_eq!(record["wrapper_sha256"], "e8e5b86093f3f781479422c300cfa4469c21bb8c302ff46c59b5de184ca44703");
                assert_eq!(record["core_sha256"], "35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578");
                assert_eq!(record["preset_sha256"], preset.snapshot().file_sha256);
                assert_eq!(record["rate"], 48_000);
                identity = true;
            }
            if record["phase"] != "trace_generated_0" || !record["lanes"].is_array() { continue; }
            let scale = CrossoverFrequencyScale::new();
            for index in 0..4 {
                let raw = preset.snapshot().crossover_hz[index];
                let expected = with_flush_to_zero(|| {
                    let editor = scale.from_normalized(scale.to_normalized(raw));
                    scale.from_normalized(scale.to_normalized(editor))
                });
                assert_eq!(expected.to_bits(), bits(&record["root_fields"][index + 1]),
                    "{environment} crossover {index}");
            }
            for (lane, raw) in preset.snapshot().lanes.iter().enumerate() {
                for (field, index, physical) in [
                    (LaneModelField::Gain, 1, raw.gain),
                    (LaneModelField::Pan, 2, raw.pan),
                    (LaneModelField::Mix, 3, raw.mix),
                    (LaneModelField::Post, 4, raw.post),
                ] {
                    let entry = &record["lanes"][lane]["parameters"][index];
                    let modulation = f32::from_bits(bits(&entry["modulation_bits"]));
                    let expected = with_flush_to_zero(|| {
                        let editor = field.model_event_value(physical, 0.0);
                        field.model_event_value(editor, modulation)
                    });
                    assert_eq!(expected.to_bits(), bits(&entry["bits"]),
                        "{environment} lane {lane} field {field:?}");
                }
            }
            seen = true;
        }
        assert!(identity && seen, "missing baseline identity or post-warmup state");
    }
}

fn field_and_index(event: u32) -> (DynamicsField, usize) {
    match event {
        0 => (DynamicsField::InputGain, 0),
        1 => (DynamicsField::OutputGain, 1),
        2 => (DynamicsField::LowThreshold, 2),
        3 => (DynamicsField::LowRatio, 3),
        4 => (DynamicsField::HighThreshold, 4),
        5 => (DynamicsField::HighRatio, 5),
        6 => (DynamicsField::Release, 7),
        7 => (DynamicsField::Mix, 9),
        422 => (DynamicsField::Attack, 6),
        423 => (DynamicsField::Knee, 8),
        _ => panic!("unexpected Dynamics automation event {event}"),
    }
}

#[test]
#[ignore = "requires a separately captured, real-wrapper AU Dynamics walk"]
fn dynamics_walk_compares_all_bound_descriptors_to_live_dsp_fields() {
    let path = std::env::var_os("PLATINUM_MULTIPASS_AU_WALK")
        .expect("set PLATINUM_MULTIPASS_AU_WALK to the reference JSONL capture");
    let reader = BufReader::new(std::fs::File::open(path).expect("cannot open reference capture"));
    let mut identity = false;
    let mut before = [None; 40];
    let mut first = [None; 40];
    let mut second = [false; 40];
    for line in reader.lines() {
        let line = line.expect("cannot read reference capture");
        let record: Value = serde_json::from_str(&line).expect("invalid reference JSONL");
        if record["record"] == "identity" {
            assert_eq!(record["wrapper_sha256"], "e8e5b86093f3f781479422c300cfa4469c21bb8c302ff46c59b5de184ca44703");
            assert_eq!(record["core_sha256"], "35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578");
            assert_eq!(record["preset_sha256"], "837ce80bc5b94515f802351cc9cce1b3212a0bb995b4999226cf54aee612d464");
            assert_eq!(record["rate"], 48_000);
            assert_eq!(record["mode"], "walk");
            assert_eq!(record["phase_bits"], "00000");
            identity = true;
        }
        if record["record"] == "status" || record["record"] == "render" {
            assert_eq!(record["status"], 0, "native operation failed: {record}");
        }
        let phase = record["phase"].as_str().unwrap_or("");
        if !matches!(phase, "walk_after_write" | "walk_after_block_1" | "walk_after_block_2")
            || !record["lanes"].is_array()
        {
            continue;
        }
        let id = record["write_id"].as_u64().expect("missing host id") as u32;
        let slot = automation_slot_for_host_parameter(id).expect("unexpected host parameter");
        assert!(slot < before.len(), "out-of-range slot {slot}");
        let path = ShippedPreset::Dynamics.automation_path(slot).expect("missing Dynamics path");
        let lane = slot % 4;
        let binding = record["slots"].as_array().expect("missing registry slots")
            .iter().find(|entry| entry["slot"].as_u64() == Some(slot as u64))
            .expect("missing observed registry binding");
        assert_eq!(binding["registry_owner_lane"], lane);
        assert_eq!(binding["direct_target_event_id"], path.event_id);
        assert_eq!(binding["registry_live"], true);
        let child = &record["lanes"][lane]["snapins"][0];
        assert_eq!(child["kind"], "ksot");
        assert_eq!(u16::from_str_radix(child["path"][0].as_str().unwrap(), 16).unwrap(), path.instance_code);
        let (field, index) = field_and_index(path.event_id);
        let observed = bits(&child["values"][index]["bits"]);
        match phase {
            "walk_after_write" => {
                assert!(before[slot].replace(observed).is_none(), "duplicate pre-render slot {slot}");
            }
            "walk_after_block_1" => {
                let previous = before[slot].expect("missing immediate post-write snapshot");
                assert_ne!(observed, previous, "host {id} did not reach the DSP field");
                let normalized = f32::from_bits(bits(&record["requested_bits"]));
                let expected = field.model_physical_from_normalized(normalized).to_bits();
                assert_eq!(observed, expected, "host {id}, event {}, lane {lane}: native {observed:08x}, descriptor {expected:08x}", path.event_id);
                assert!(first[slot].replace(observed).is_none(), "duplicate first-render slot {slot}");
            }
            "walk_after_block_2" => {
                assert_eq!(Some(observed), first[slot], "host {id} changed again in block 2");
                assert!(!second[slot], "duplicate second-render slot {slot}");
                second[slot] = true;
            }
            _ => unreachable!(),
        }
    }
    assert!(identity, "missing capture identity");
    assert!(before.into_iter().all(|v| v.is_some()), "capture omitted an immediate post-write snapshot");
    assert!(first.into_iter().all(|v| v.is_some()), "capture omitted a first-render snapshot");
    assert!(second.into_iter().all(|v| v), "capture omitted a second-render snapshot");
}

#[test]
#[ignore = "requires separate real-wrapper Warmth1 and Warmth2 captures"]
fn effect_descriptors_and_flags_match_observed_warmth_children() {
    for (environment, preset) in [
        ("PLATINUM_MULTIPASS_AU_WARMTH1", ShippedPreset::Warmth1),
        ("PLATINUM_MULTIPASS_AU_WARMTH2", ShippedPreset::Warmth2),
    ] {
        let path = std::env::var_os(environment).expect("missing reference capture path");
        let reader = BufReader::new(std::fs::File::open(path).expect("cannot open reference capture"));
        let mut identity = false;
        let mut checked = [0; 3];
        for line in reader.lines() {
            let record: Value = serde_json::from_str(&line.expect("cannot read reference"))
                .expect("invalid reference JSONL");
            if record["record"] == "identity" {
                assert_eq!(record["wrapper_sha256"], "e8e5b86093f3f781479422c300cfa4469c21bb8c302ff46c59b5de184ca44703");
                assert_eq!(record["core_sha256"], "35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578");
                assert_eq!(record["preset_sha256"], preset.snapshot().file_sha256);
                assert_eq!(record["rate"], 48_000);
                assert_eq!(record["phase_bits"], "00000");
                identity = true;
            }
            if record["record"] == "status" || record["record"] == "render" {
                assert_eq!(record["status"], 0, "native operation failed: {record}");
            }
            if record["phase"] != "after_first_generated" || !record["lanes"].is_array() {
                continue;
            }
            for (lane, snapins) in preset.snapshot().snapins.iter().enumerate() {
                for (ordinal, snapin) in snapins.iter().enumerate() {
                    let native = &record["lanes"][lane]["snapins"][ordinal];
                    assert_eq!(u16::from_str_radix(native["path"][0].as_str().unwrap(), 16).unwrap(), snapin.instance);
                    let enabled = match &snapin.parameters {
                        ShippedEffectParameters::Dynamics(raw) => {
                            assert_eq!(native["kind"], "ksot");
                            for (index, (field, physical)) in [
                                (DynamicsField::InputGain, raw.input_gain_db),
                                (DynamicsField::OutputGain, raw.output_gain_db),
                                (DynamicsField::LowThreshold, raw.low_threshold_db),
                                (DynamicsField::LowRatio, raw.low_ratio),
                                (DynamicsField::HighThreshold, raw.high_threshold_db),
                                (DynamicsField::HighRatio, raw.high_ratio),
                                (DynamicsField::Attack, raw.attack),
                                (DynamicsField::Release, raw.release),
                                (DynamicsField::Knee, raw.knee),
                                (DynamicsField::Mix, raw.mix),
                            ].into_iter().enumerate() {
                                let modulation = f32::from_bits(bits(&native["values"][index]["modulation_bits"]));
                                let expected = with_flush_to_zero(|| field.model_event_value(physical, modulation)).to_bits();
                                assert_eq!(bits(&native["values"][index]["bits"]), expected,
                                    "{environment}: lane {lane}, dynamics {}, field {field:?}", snapin.instance);
                            }
                            checked[0] += 1;
                            raw.enabled
                        }
                        ShippedEffectParameters::Distortion(raw) => {
                            assert_eq!(native["kind"], "ksdt");
                            for (index, physical) in [
                                (0, raw.drive), (1, raw.bias), (2, raw.spread),
                                (4, raw.dynamics), (5, raw.mix),
                            ] {
                                assert_eq!(bits(&native["values"][index]["modulation_bits"]), 0);
                                assert_eq!(bits(&native["values"][index]["bits"]), physical.to_bits(),
                                    "{environment}: lane {lane}, distortion {}, field {index}", snapin.instance);
                            }
                            assert_eq!(bits(&native["values"][3]["bits"]), 1, "expected Saturate mode");
                            assert_eq!(bits(&native["values"][6]["bits"]) & 1 != 0, raw.remove_dc);
                            checked[1] += 1;
                            raw.enabled
                        }
                        ShippedEffectParameters::TransientShaper(raw) => {
                            assert_eq!(native["kind"], "kstr");
                            for (index, (field, physical)) in [
                                (TransientModelField::Attack, raw.attack),
                                (TransientModelField::Pump, raw.pump),
                                (TransientModelField::Sustain, raw.sustain),
                                (TransientModelField::Speed, raw.speed),
                            ].into_iter().enumerate() {
                                let modulation = f32::from_bits(bits(&native["values"][index]["modulation_bits"]));
                                let expected = with_flush_to_zero(|| field.model_event_value(physical, modulation)).to_bits();
                                assert_eq!(bits(&native["values"][index]["bits"]), expected,
                                    "{environment}: lane {lane}, transient {}, field {field:?}", snapin.instance);
                            }
                            assert_eq!(bits(&native["values"][4]["bits"]) & 1 != 0, raw.clip);
                            checked[2] += 1;
                            raw.enabled
                        }
                    };
                    assert_eq!(bits(&native["generic_gate_words"][0]) & 1 != 0, enabled,
                        "{environment}: lane {lane}, effect {} enable", snapin.instance);
                }
            }
        }
        assert!(identity, "missing capture identity");
        assert_eq!(checked, if preset == ShippedPreset::Warmth1 { [1, 1, 2] } else { [1, 1, 3] });
    }
}

/// Compare the graph against the wrapper's `trace` mode, including the
/// wrapper's documented implicit warmup and native balanced subdivisions.
/// Phase is deliberately disabled in these captures. This is an opt-in
/// differential, not a claim that the comparison has passed.
#[test]
#[ignore = "requires real-wrapper graph trace JSONL captures"]
fn fixed_shipped_graphs_match_phaseoff_wrapper_traces() {
    use platinum_engine::multipass_graph::{GraphContext, MultipassGraph};
    let mut total_mismatched = 0_usize;
    for (environment, preset) in [
        ("PLATINUM_GRAPH_GAIN", ShippedPreset::Gain),
        ("PLATINUM_GRAPH_DYNAMICS", ShippedPreset::Dynamics),
        ("PLATINUM_GRAPH_WARMTH1", ShippedPreset::Warmth1),
        ("PLATINUM_GRAPH_WARMTH2", ShippedPreset::Warmth2),
    ] {
        let path = std::env::var_os(environment).expect("missing graph reference capture");
        let reader = BufReader::new(std::fs::File::open(path).expect("open graph reference capture"));
        let mut graph = MultipassGraph::new(48_000.0, preset).unwrap();
        let mut identity = false;
        let mut warmed = false;
        let mut renders = 0;
        let mut case_mismatched = 0_usize;
        for line in reader.lines() {
            let record: Value = serde_json::from_str(&line.expect("read reference line")).expect("JSONL");
            if record["record"] == "identity" {
                assert_eq!(record["wrapper_sha256"], "e8e5b86093f3f781479422c300cfa4469c21bb8c302ff46c59b5de184ca44703");
                assert_eq!(record["core_sha256"], "35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578");
                assert_eq!(record["preset_sha256"], preset.snapshot().file_sha256);
                assert_eq!(record["mode"], "trace");
                assert_eq!(record["phase_bits"], "00000");
                assert_eq!(record["rate"], 48_000);
                identity = true;
            } else if record["record"] == "status" {
                assert_eq!(record["status"], 0, "failed AU status");
            } else if record["record"] == "parameter" {
                assert_eq!(record["get_status"], 0);
                graph.set_parameter(record["id"].as_u64().unwrap() as u32,
                    f32::from_bits(bits(&record["requested_bits"]))).unwrap();
            } else if record["record"] == "render" {
                assert!(identity, "capture identity must precede render");
                assert_eq!(record["status"], 0);
                if !warmed {
                    let mut remaining = 2400_usize;
                    while remaining != 0 {
                        let block = remaining.min(512);
                        let mut left = vec![0.0_f32; block];
                        let mut right = vec![0.0_f32; block];
                        let chunks = block.div_ceil(64);
                        with_flush_to_zero(|| {
                            for k in 0..chunks {
                                let start = k * block / chunks;
                                let end = (k + 1) * block / chunks;
                                let mut context = GraphContext { silent: true, bypass: false, segment_remaining: block - start };
                                graph.process_chunk(&mut context, &mut left[start..end], &mut right[start..end]).unwrap();
                            }
                        });
                        remaining -= block;
                    }
                    warmed = true;
                }
                let frames = record["frames"].as_u64().unwrap() as usize;
                let time = record["sample_time"].as_u64().unwrap() as usize;
                let zero = record["input_zero"].as_bool().unwrap();
                let mut left: Vec<f32> = (0..frames).map(|i| if zero { 0.0 } else {
                    (((time + i) * 17 % 31) as i32 - 15) as f32 / 64.0
                }).collect();
                let mut right: Vec<f32> = (0..frames).map(|i| if zero { 0.0 } else {
                    (((time + i) * 11 % 29) as i32 - 14) as f32 / 64.0
                }).collect();
                let chunks = frames.div_ceil(64);
                with_flush_to_zero(|| {
                    for k in 0..chunks {
                        let start = k * frames / chunks;
                        let end = (k + 1) * frames / chunks;
                        let mut context = GraphContext { silent: zero, bypass: false, segment_remaining: frames - start };
                        graph.process_chunk(&mut context, &mut left[start..end], &mut right[start..end]).unwrap();
                    }
                });
                let mut block_mismatched = 0_usize;
                let mut first_difference = None;
                for (channel, actual) in [left.as_slice(), right.as_slice()].into_iter().enumerate() {
                    let expected = record["output_bits"][channel].as_array().expect("native output samples");
                    assert_eq!(actual.len(), expected.len());
                    for (i, (&sample, reference)) in actual.iter().zip(expected).enumerate() {
                        let rust = sample.to_bits();
                        let native = bits(reference);
                        if rust != native {
                            block_mismatched += 1;
                            if first_difference.is_none() {
                                first_difference = Some((channel, i, rust, native));
                            }
                        }
                    }
                }
                if let Some((channel, frame, rust, native)) = first_difference {
                    println!("{environment} {} first channel={channel} frame={frame} rust={rust:08x} native={native:08x} mismatched_samples={block_mismatched}", record["phase"]);
                } else {
                    println!("{environment} {} exact", record["phase"]);
                }
                case_mismatched += block_mismatched;
                renders += 1;
            }
        }
        assert!(identity && warmed);
        assert_eq!(renders, 20, "incomplete graph trace");
        println!("{environment} total_mismatched_samples={case_mismatched}");
        total_mismatched += case_mismatched;
    }
    assert_eq!(total_mismatched, 0, "one or more graph captures differ bitwise");
}

/// Compare the composed graph and wrapper-owned reverse-window phase stage.
/// The long capture crosses two phase windows rather than comparing only the
/// initial delayed zeros. It remains an opt-in reference test.
#[test]
#[ignore = "requires real-wrapper long phase trace JSONL captures"]
fn fixed_shipped_graphs_match_phaseon_wrapper_traces() {
    use platinum_engine::multipass_graph::{GraphContext, MultipassGraph};
    use platinum_engine::phase::PhaseCorrection;
    let mut total_mismatched = 0_usize;
    for (environment, preset) in [
        ("PLATINUM_PHASE_GRAPH_GAIN", ShippedPreset::Gain),
        ("PLATINUM_PHASE_GRAPH_DYNAMICS", ShippedPreset::Dynamics),
        ("PLATINUM_PHASE_GRAPH_WARMTH1", ShippedPreset::Warmth1),
        ("PLATINUM_PHASE_GRAPH_WARMTH2", ShippedPreset::Warmth2),
    ] {
        let path = std::env::var_os(environment).expect("missing long phase reference capture");
        let reader = BufReader::new(std::fs::File::open(path).expect("open long phase reference"));
        let mut graph = MultipassGraph::new(48_000.0, preset).unwrap();
        let mut identity = false;
        let mut flags = [false; 5];
        let mut parameters = [None; 4];
        let mut phase: Option<PhaseCorrection> = None;
        let mut renders = 0;
        let mut nonzero = false;
        let mut case_mismatched = 0_usize;
        for line in reader.lines() {
            let record: Value = serde_json::from_str(&line.expect("read reference line")).expect("JSONL");
            if record["record"] == "identity" {
                assert_eq!(record["wrapper_sha256"], "e8e5b86093f3f781479422c300cfa4469c21bb8c302ff46c59b5de184ca44703");
                assert_eq!(record["core_sha256"], "35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578");
                assert_eq!(record["preset_sha256"], preset.snapshot().file_sha256);
                assert_eq!(record["mode"], "trace-long");
                assert_eq!(record["rate"], 48_000);
                let text = record["phase_bits"].as_str().expect("missing phase flags");
                assert_eq!(text.len(), 5);
                for (index, byte) in text.bytes().enumerate() {
                    assert!(byte == b'0' || byte == b'1');
                    flags[index] = byte == b'1';
                }
                identity = true;
            } else if record["record"] == "status" {
                assert_eq!(record["status"], 0, "failed AU status");
            } else if record["record"] == "phase_parameter" {
                assert_eq!(record["get_status"], 0);
                let id = record["id"].as_u64().unwrap() as usize;
                assert!(id < 4 && parameters[id].is_none());
                parameters[id] = Some(f32::from_bits(bits(&record["get_bits"])));
            } else if record["record"] == "parameter" {
                assert_eq!(record["get_status"], 0);
                let id = record["id"].as_u64().unwrap() as usize;
                let value = f32::from_bits(bits(&record["requested_bits"]));
                graph.set_parameter(id as u32, value).unwrap();
                if id < 4 { parameters[id] = Some(value); }
            } else if record["record"] == "render" {
                assert!(identity, "capture identity must precede render");
                assert_eq!(record["status"], 0);
                if phase.is_none() {
                    let values = parameters.map(|value| value.expect("missing effective phase parameter"));
                    phase = Some(PhaseCorrection::new(48_000.0, flags, values, 2));
                    let mut remaining = 2400_usize;
                    while remaining != 0 {
                        let block = remaining.min(512);
                        let mut left = vec![0.0_f32; block];
                        let mut right = vec![0.0_f32; block];
                        let chunks = block.div_ceil(64);
                        with_flush_to_zero(|| {
                            for k in 0..chunks {
                                let start = k * block / chunks;
                                let end = (k + 1) * block / chunks;
                                let mut context = GraphContext { silent: true, bypass: false, segment_remaining: block - start };
                                graph.process_chunk(&mut context, &mut left[start..end], &mut right[start..end]).unwrap();
                            }
                        });
                        // Native 0x4d1c sends the same warmup pointer for both
                        // channels through host 0x6a0c. The latter applies
                        // its phase stage to channel 0 and then channel 1.
                        let correction = phase.as_mut().unwrap();
                        correction.process_channel(0, &mut left);
                        correction.process_channel(1, &mut left);
                        remaining -= block;
                    }
                }
                let frames = record["frames"].as_u64().unwrap() as usize;
                let time = record["sample_time"].as_u64().unwrap() as usize;
                let zero = record["input_zero"].as_bool().unwrap();
                let mut left: Vec<f32> = (0..frames).map(|i| if zero { 0.0 } else {
                    (((time + i) * 17 % 31) as i32 - 15) as f32 / 64.0
                }).collect();
                let mut right: Vec<f32> = (0..frames).map(|i| if zero { 0.0 } else {
                    (((time + i) * 11 % 29) as i32 - 14) as f32 / 64.0
                }).collect();
                let chunks = frames.div_ceil(64);
                with_flush_to_zero(|| {
                    for k in 0..chunks {
                        let start = k * frames / chunks;
                        let end = (k + 1) * frames / chunks;
                        let mut context = GraphContext { silent: zero, bypass: false, segment_remaining: frames - start };
                        graph.process_chunk(&mut context, &mut left[start..end], &mut right[start..end]).unwrap();
                    }
                });
                let correction = phase.as_mut().unwrap();
                correction.process_channel(0, &mut left);
                correction.process_channel(1, &mut right);
                let mut first_difference = None;
                let mut block_mismatched = 0_usize;
                for (channel, actual) in [left.as_slice(), right.as_slice()].into_iter().enumerate() {
                    let expected = record["output_bits"][channel].as_array().expect("native output samples");
                    assert_eq!(actual.len(), expected.len());
                    for (i, (&sample, reference)) in actual.iter().zip(expected).enumerate() {
                        let native = bits(reference);
                        nonzero |= native & 0x7fff_ffff != 0;
                        let rust = sample.to_bits();
                        if rust != native {
                            block_mismatched += 1;
                            if first_difference.is_none() {
                                first_difference = Some((channel, i, rust, native));
                            }
                        }
                    }
                }
                if let Some((channel, frame, rust, native)) = first_difference {
                    println!("{environment} {} first channel={channel} frame={frame} rust={rust:08x} native={native:08x} mismatched_samples={block_mismatched}", record["phase"]);
                }
                case_mismatched += block_mismatched;
                renders += 1;
            }
        }
        assert!(identity && phase.is_some());
        assert_eq!(renders, 100, "incomplete long phase trace");
        assert!(nonzero, "capture did not observe nonzero output");
        println!("{environment} total_mismatched_samples={case_mismatched}");
        total_mismatched += case_mismatched;
    }
    assert_eq!(total_mismatched, 0, "one or more composed phase captures differ bitwise");
}
