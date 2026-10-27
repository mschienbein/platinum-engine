//! Native PSOLA oracle comparisons and source-independent boundary checks.
//! The ignored oracle test requires outputs produced by psola-oracle.cpp.

#[path = "../src/pitch/fft.rs"]
mod fft;
#[path = "../src/pitch/psola.rs"]
mod psola;

use psola::Psola;
use std::path::Path;

fn synthetic(frame: u64, channel: u64) -> f32 {
    let value = ((frame * 17 + channel * 29) % 101) as i32 - 50;
    let base = value as f32 / 128.0_f32;
    let amplitude = if (frame / 896) % 4 == 0 { 0.01_f32 } else { 0.6_f32 };
    base * amplitude
}

fn words(path: &Path) -> Vec<u32> {
    let bytes = std::fs::read(path).expect("native oracle artifact");
    assert_eq!(bytes.len() % 4, 0, "partial 32-bit word in {}", path.display());
    bytes.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect()
}

fn compare_words(label: &str, expected: &[u32], actual: &[u32]) {
    assert_eq!(expected.len(), actual.len(), "{label}: word count");
    if let Some((index, (want, got))) = expected.iter().zip(actual).enumerate().find(|(_, (a, b))| a != b) {
        panic!("{label}: word {index}: reference {want:#010x}, implementation {got:#010x}");
    }
}

#[test]
fn selected_rate_boundaries_seed_exact_silence_markers() {
    for (rate, base) in [(8000.0, 256), (31999.0, 256), (32000.0, 512), (63999.0, 512),
                         (64000.0, 1024), (127999.0, 1024), (128000.0, 2048),
                         (255999.0, 2048), (256000.0, 4096), (384000.0, 4096)] {
        let mut leaf = Psola::new(rate, 1, 512).expect("supported reference rate");
        let silence = vec![0.0_f32; 2 * base];
        leaf.analyse(&[&silence]);
        let events = leaf.reference_markers();
        assert_eq!(events.len(), 2, "rate {rate}");
        assert_eq!(events[0], [0; 6]);
        assert_eq!(events[1], [(base / 2) as u32, 0, 0, 0, 0, ((base / 2) as f32).to_bits()], "rate {rate}");
    }
}

#[test]
fn analysis_sums_opposed_channels_before_detection() {
    let mut leaf = Psola::new(48000.0, 2, 512).unwrap();
    let left: Vec<f32> = (0..2048).map(|i| ((i % 127) as f32 - 63.0) / 128.0).collect();
    let right: Vec<f32> = left.iter().map(|x| -*x).collect();
    leaf.analyse(&[&left, &right]);
    let events = leaf.reference_markers();
    assert_eq!(events.len(), 6);
    for (i, marker) in events.iter().enumerate().skip(1) {
        assert_eq!(*marker, [(256 * i) as u32, 0, 0, 0, 0, 256.0_f32.to_bits()]);
    }
}

#[test]
fn input_sizing_tracks_marker_availability_and_reset() {
    let mut leaf = Psola::new(48000.0, 1, 512).unwrap();
    assert_eq!(leaf.required_input(0, 0.0), Some(0));
    assert_eq!(leaf.required_input(512, 0.0), None);
    let silence = [0.0_f32; 1024];
    leaf.analyse(&[&silence]);
    assert_eq!(leaf.required_input(512, 0.0), Some(256));
    leaf.reset();
    assert_eq!(leaf.reference_markers(), vec![[0; 6]]);
    assert_eq!(leaf.required_input(512, 0.0), None);
}

#[test]
fn analysis_marker_history_is_independent_of_partitioning() {
    let left: Vec<f32> = (0..8192).map(|i| synthetic(i, 0)).collect();
    let right: Vec<f32> = (0..8192).map(|i| synthetic(i, 1)).collect();
    let mut whole = Psola::new(48000.0, 2, 512).unwrap();
    let mut split = Psola::new(48000.0, 2, 512).unwrap();
    whole.analyse(&[&left, &right]);
    let mut at = 0;
    let mut part = 0;
    let sizes = [1, 511, 29, 1027, 63, 256, 713];
    while at < left.len() {
        let end = (at + sizes[part % sizes.len()]).min(left.len());
        split.analyse(&[&left[at..end], &right[at..end]]);
        at = end;
        part += 1;
    }
    assert_eq!(whole.reference_markers(), split.reference_markers());
}

#[test]
fn aliased_processing_matches_separate_planar_buffers() {
    for channels in [1, 2] {
        let mut separate = Psola::new(48000.0, channels, 512).unwrap();
        let mut aliased = Psola::new(48000.0, channels, 512).unwrap();
        separate.set_stretch(511.0_f32 / 512.0_f32);
        aliased.set_stretch(511.0_f32 / 512.0_f32);
        for block in 0..24 {
            let left: Vec<f32> = (0..512).map(|i| synthetic((block * 512 + i) as u64, 0)).collect();
            let right: Vec<f32> = (0..512).map(|i| synthetic((block * 512 + i) as u64, 1)).collect();
            let hop = if block < 16 { 0 } else { 512 };
            let position = if block < 16 { 0.0 } else { ((block - 16) * 512) as f64 };
            let mut left_out = vec![0.0; hop];
            let mut right_out = vec![0.0; hop];
            let mut planes = if channels == 1 { vec![left.clone()] } else { vec![left.clone(), right.clone()] };
            if channels == 1 {
                separate.analyse(&[&left]);
                aliased.analyse(&[&planes[0]]);
                separate.process(&[&left], &mut [&mut left_out], position).unwrap();
            } else {
                separate.analyse(&[&left, &right]);
                aliased.analyse(&[&planes[0], &planes[1]]);
                separate.process(&[&left, &right], &mut [&mut left_out, &mut right_out], position).unwrap();
            }
            aliased.process_in_place(&mut planes, 512, hop, position).unwrap();
            assert_eq!(&planes[0][..hop], &left_out);
            if channels == 2 { assert_eq!(&planes[1][..hop], &right_out); }
            assert_eq!(aliased.reference_state(), separate.reference_state());
            assert_eq!(aliased.reference_markers(), separate.reference_markers());
        }
    }
}

#[test]
fn prime_does_not_reposition_or_overwrite_the_work_buffer() {
    let mut leaf = Psola::new(48000.0, 2, 512).unwrap();
    let make_planes = |block: usize| -> Vec<Vec<f32>> {
        (0..2).map(|channel| (0..512).map(|i|
            synthetic((block * 512 + i) as u64, channel)).collect()).collect()
    };
    for block in 0..16 {
        let mut planes = make_planes(block);
        leaf.analyse_prime(&[&planes[0], &planes[1]]);
        leaf.prime_in_place(&mut planes, 512).unwrap();
    }
    for block in 16..20 {
        let mut planes = make_planes(block);
        leaf.analyse(&[&planes[0], &planes[1]]);
        leaf.process_in_place(&mut planes, 512, 512, ((block - 16) * 512) as f64).unwrap();
    }
    let mut planes = make_planes(20);
    let original = planes.clone();
    leaf.analyse_prime(&[&planes[0], &planes[1]]);
    let position = leaf.reference_position();
    assert_ne!(position, 0.0_f64.to_bits());
    leaf.prime_in_place(&mut planes, 512).unwrap();
    assert_eq!(leaf.reference_position(), position);
    assert_eq!(planes, original);
}

#[test]
fn invalid_dimensions_are_not_accepted() {
    assert!(Psola::new(7999.0, 2, 512).is_none());
    assert!(Psola::new(384001.0, 2, 512).is_none());
    assert!(Psola::new(48000.0, 0, 512).is_none());
    assert!(Psola::new(48000.0, 3, 512).is_none());
}

#[test]
#[ignore = "requires synthetic original-component captures in PSOLA_REFERENCE_DIR"]
fn selected_and_forced_branch_outputs_match_native_psola() {
    let dir = std::env::var_os("PSOLA_REFERENCE_DIR").expect("set PSOLA_REFERENCE_DIR to the oracle artifact directory");
    let dir = Path::new(&dir);
    for case in ["selected", "interpolated", "transient", "both", "reset", "prime"] {
        let mut leaf = Psola::new(48000.0, 2, 512).unwrap();
        let table_reference = words(&dir.join(format!("{case}.tables.f32")));
        let (coefficients, differences, fade) = leaf.reference_tables();
        let table_actual: Vec<u32> = coefficients.iter().chain(differences).chain(fade).map(|v| v.to_bits()).collect();
        compare_words(&format!("{case} tables"), &table_reference, &table_actual);
        let interpolate = case == "interpolated" || case == "both";
        let transients = case == "transient" || case == "both";
        leaf.set_stretch(511.0_f32 / 512.0_f32);
        leaf.reference_branches(if interpolate { 0.98 } else { 1.0 }, interpolate, transients);
        let mut frame = 0_u64;
        let mut output_words = Vec::new();
        let mut needs = Vec::new();
        let mut states = Vec::new();
        let mut run = |leaf: &mut Psola, hop: usize, position: f64, prime: bool| {
            let left: Vec<f32> = (0..512).map(|i| synthetic(frame + i, 0)).collect();
            let right: Vec<f32> = (0..512).map(|i| synthetic(frame + i, 1)).collect();
            if prime {
                assert_eq!(hop, 0);
                leaf.analyse_prime(&[&left, &right]);
                // In this selected, transient-disabled case, ready >= zero.
                needs.push(0);
                let mut planes = vec![left, right];
                leaf.prime_in_place(&mut planes, 512).expect("native prime succeeded");
            } else {
                leaf.analyse(&[&left, &right]);
                needs.push(leaf.required_input(hop, position).map_or(-1_i64, |n| n as i64));
                let mut left_out = vec![0.0; hop];
                let mut right_out = vec![0.0; hop];
                leaf.process(&[&left, &right], &mut [&mut left_out, &mut right_out], position)
                    .expect("native oracle processing succeeded");
                output_words.extend(left_out.iter().chain(&right_out).map(|v| v.to_bits()));
            }
            let (cursor, ready, transient) = leaf.reference_state();
            states.push((cursor, ready, transient, leaf.reference_position()));
            frame += 512;
        };
        if case == "reset" {
            for _ in 0..4 { run(&mut leaf, 0, 0.0, false); }
            leaf.reset();
            leaf.set_stretch(511.0_f32 / 512.0_f32);
        }
        for _ in 0..16 { run(&mut leaf, 0, 0.0, case == "prime"); }
        for i in 0..8 { run(&mut leaf, 512, (i * 512) as f64, false); }
        if case == "prime" {
            for _ in 0..4 { run(&mut leaf, 0, 0.0, true); }
            for i in 8..12 { run(&mut leaf, 512, (i * 512) as f64, false); }
        }
        let log = std::fs::read_to_string(dir.join(format!("{case}.events.jsonl"))).expect("oracle event log");
        let native_events: Vec<serde_json::Value> = log.lines().map(|line| {
            serde_json::from_str(line).expect("oracle event JSON")
        }).collect();
        let native_needs: Vec<i64> = native_events.iter().map(|event|
            event["need"].as_i64().expect("oracle need")).collect();
        let native_states: Vec<(i32, i32, bool, u64)> = native_events.iter().map(|event| (
            event["cursor"].as_i64().expect("oracle cursor") as i32,
            event["ready"].as_i64().expect("oracle ready count") as i32,
            event["last_transient"].as_u64().expect("oracle transient state") != 0,
            event["position_bits"].as_u64().expect("oracle position bits")
        )).collect();
        assert_eq!(native_needs, needs, "{case}: required-input sequence");
        assert_eq!(native_states, states, "{case}: marker/queue/transient states");
        compare_words(&format!("{case} audio"), &words(&dir.join(format!("{case}.audio.f32"))), &output_words);
        let marker_reference = words(&dir.join(format!("{case}.markers.bin")));
        let markers = leaf.reference_markers();
        assert_eq!(marker_reference.first().copied(), Some(markers.len() as u32), "{case} marker count");
        let marker_actual: Vec<u32> = markers.into_iter().flatten().collect();
        compare_words(&format!("{case} markers"), &marker_reference[1..], &marker_actual);
    }
}
