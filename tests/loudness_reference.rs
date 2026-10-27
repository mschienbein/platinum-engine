use platinum_engine::loudness::{GainAnalyzer, LoudnessError, LoudnessMeter};

const FS: f64 = 48_000.0;
const HOP: usize = 4_800;
const WINDOW: usize = 4 * HOP;

fn alternating(frames: usize) -> Vec<f32> {
    (0..frames).map(|i| if i % 2 == 0 { 0.25 } else { -0.25 }).collect()
}

fn place_in_channel(input: &[f32], channels: usize, channel: usize) -> Vec<f32> {
    let mut interleaved = vec![0.0; input.len() * channels];
    for (frame, &sample) in input.iter().enumerate() {
        interleaved[frame * channels + channel] = sample;
    }
    interleaved
}

#[test]
fn validates_configuration_and_complete_frames_before_mutating() {
    assert!(matches!(LoudnessMeter::new(FS, 0), Err(LoudnessError::InvalidChannelCount)));
    assert!(matches!(LoudnessMeter::new(FS, 65), Err(LoudnessError::InvalidChannelCount)));
    assert!(matches!(LoudnessMeter::new(f64::NAN, 1), Err(LoudnessError::InvalidSampleRate)));
    assert!(matches!(LoudnessMeter::new(15.99, 1), Err(LoudnessError::InvalidSampleRate)));
    // The native 16/17 Hz configuration writes eight frames to a six-frame
    // ring; this API rejects it rather than reproducing memory corruption.
    assert!(matches!(LoudnessMeter::new(16.0, 1), Err(LoudnessError::InvalidSampleRate)));
    assert!(matches!(LoudnessMeter::new(17.99, 1), Err(LoudnessError::InvalidSampleRate)));
    assert!(LoudnessMeter::new(18.0, 1).is_ok());
    assert!(matches!(LoudnessMeter::new(2_822_401.0, 1), Err(LoudnessError::InvalidSampleRate)));

    let mut meter = LoudnessMeter::new(FS, 2).unwrap();
    assert_eq!(meter.integrated_loudness(), -200.0);
    meter.process_interleaved(&[]).unwrap();
    assert_eq!(meter.integrated_loudness(), -200.0);
    assert_eq!(meter.process_interleaved(&[0.5]), Err(LoudnessError::IncompleteFrame));
    assert_eq!(meter.integrated_loudness(), -200.0);

    let mut analyzer = GainAnalyzer::new(FS, 2, 0.023, true).unwrap();
    assert_eq!(analyzer.process_interleaved(&[0.5]), Err(LoudnessError::IncompleteFrame));
    assert_eq!(analyzer.integrated_loudness(), -200.0);
}

#[test]
fn first_complete_window_and_trailing_partial_window() {
    let samples = alternating(WINDOW + HOP - 1);
    let mut meter = LoudnessMeter::new(FS, 1).unwrap();
    meter.process_interleaved(&samples[..WINDOW - 1]).unwrap();
    assert_eq!(meter.integrated_loudness(), f64::NEG_INFINITY);
    meter.process_interleaved(&samples[WINDOW - 1..WINDOW]).unwrap();
    let first = meter.integrated_loudness();
    assert!(first.is_finite());
    meter.process_interleaved(&samples[WINDOW..]).unwrap();
    assert_eq!(meter.integrated_loudness().to_bits(), first.to_bits());
    meter.process_interleaved(&[-0.25]).unwrap();
    assert!(meter.integrated_loudness().is_finite());
}

#[test]
fn channel_energy_weights_are_summed_not_averaged() {
    let signal = alternating(WINDOW);
    let mut mono = LoudnessMeter::new(FS, 1).unwrap();
    mono.process_interleaved(&signal).unwrap();
    let mono_loudness = mono.integrated_loudness();

    let mut stereo = Vec::with_capacity(signal.len() * 2);
    for &sample in &signal {
        stereo.extend_from_slice(&[sample, sample]);
    }
    let mut duplicated = LoudnessMeter::new(FS, 2).unwrap();
    duplicated.process_interleaved(&stereo).unwrap();
    let stereo_delta = duplicated.integrated_loudness() - mono_loudness;
    assert!((stereo_delta - 10.0 * 2.0f64.log10()).abs() < 1e-11);

    let mut four = LoudnessMeter::new(FS, 4).unwrap();
    four.process_interleaved(&place_in_channel(&signal, 4, 2)).unwrap();
    let surround_delta = four.integrated_loudness() - mono_loudness;
    assert!((surround_delta - 10.0 * 1.41f64.log10()).abs() < 1e-11);

    let mut six = LoudnessMeter::new(FS, 6).unwrap();
    six.process_interleaved(&place_in_channel(&signal, 6, 3)).unwrap();
    assert_eq!(six.integrated_loudness(), f64::NEG_INFINITY);
    let mut seven = LoudnessMeter::new(FS, 7).unwrap();
    seven.process_interleaved(&place_in_channel(&signal, 7, 6)).unwrap();
    assert_eq!(seven.integrated_loudness(), f64::NEG_INFINITY);
}

#[test]
fn interleaved_partitioning_preserves_normal_signal_results() {
    let mono = alternating(WINDOW + 3 * HOP + 77);
    let mut stereo = Vec::with_capacity(2 * mono.len());
    for (i, &sample) in mono.iter().enumerate() {
        stereo.extend_from_slice(&[sample, if i % 5 == 0 { sample * 0.5 } else { -sample }]);
    }
    let mut single = LoudnessMeter::new(FS, 2).unwrap();
    single.process_interleaved(&stereo).unwrap();
    let mut partitioned = LoudnessMeter::new(FS, 2).unwrap();
    let mut frame = 0;
    let partitions = [1usize, 1023, 1024, 100, 4800, 17, 2051];
    let mut i = 0;
    while frame < mono.len() {
        let end = (frame + partitions[i % partitions.len()]).min(mono.len());
        partitioned.process_interleaved(&stereo[frame * 2..end * 2]).unwrap();
        frame = end;
        i += 1;
    }
    assert_eq!(partitioned.integrated_loudness().to_bits(), single.integrated_loudness().to_bits());
}

#[test]
fn gain_analyzer_uses_bank_only_when_enabled_and_direct_attenuation() {
    let samples = alternating(WINDOW + HOP);
    let mut disabled = GainAnalyzer::new(FS, 1, 0.023, false).unwrap();
    disabled.process_interleaved(&samples).unwrap();
    let baseline = disabled.integrated_loudness();
    assert!(baseline.is_finite());
    assert_eq!(disabled.gain_for_target(baseline - 3.0).to_bits(), 0.0f64.to_bits());

    let mut enabled = GainAnalyzer::new(FS, 1, 0.023, true).unwrap();
    enabled.process_interleaved(&samples).unwrap();
    assert_eq!(enabled.integrated_loudness().to_bits(), baseline.to_bits());
    let target = baseline - 3.0;
    assert_eq!(enabled.gain_for_target(target).to_bits(), (target - baseline).to_bits());
    assert!(enabled.gain_for_target(baseline + 2.0).is_finite());
}

#[test]
fn silent_simulations_and_nonpositive_release_follow_reference_branches() {
    let mut default_release = GainAnalyzer::new(FS, 1, 0.0, true).unwrap();
    let mut explicit_release = GainAnalyzer::new(FS, 1, 0.023, true).unwrap();
    let signal = alternating(WINDOW + HOP);
    default_release.process_interleaved(&signal).unwrap();
    explicit_release.process_interleaved(&signal).unwrap();
    let target = default_release.integrated_loudness() + 6.0;
    assert_eq!(default_release.gain_for_target(target).to_bits(), explicit_release.gain_for_target(target).to_bits());

    let mut silent = GainAnalyzer::new(FS, 1, 0.023, true).unwrap();
    silent.process_interleaved(&vec![0.0; WINDOW]).unwrap();
    assert_eq!(silent.integrated_loudness(), f64::NEG_INFINITY);
    assert_eq!(silent.gain_for_target(-11.5).to_bits(), 0.0f64.to_bits());
    assert_eq!(silent.gain_for_target(f64::NAN).to_bits(), 0.0f64.to_bits());
}
