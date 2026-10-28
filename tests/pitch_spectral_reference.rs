//! Selected FFT/splitter checks. The native-oracle comparison is opt-in and
//! requires the separately generated, identity-guarded synthetic capture.
use platinum_engine::pitch::fft::{self, RealFft};
use platinum_engine::pitch::spectral::FftSplitter;

fn pattern(index: usize, channel: usize) -> f32 {
    (((index * 17 + channel * 29) % 101) as i32 - 50) as f32 / 128.0
}


#[test]
fn phase_mutates_real_zeroes_but_restores_nyquist() {
    let mut packed = [0.0_f32, -0.0, 1.0, -2.0, -3.0, 0.25, -0.5, 1.0];
    let mut output = [0.0; 4];
    fft::phase(&mut packed, &mut output);
    assert_eq!(packed[0].to_bits(), 0x322b_cc77);
    assert_eq!(packed[1].to_bits(), 0x322b_cc77);
    assert_eq!(packed[2], 1.0);
    assert_eq!(packed[3], -2.0);
    assert_eq!(packed[4].to_bits(), (-3.0_f32).to_bits());
    let before = packed.map(f32::to_bits);
    fft::magnitude(&mut packed, &mut output);
    assert_eq!(packed.map(f32::to_bits), before);
}

#[test]
fn transient_absolute_vector_handles_signed_zero_and_infinity() {
    let mut values = [-0.0_f32, -1.5, 2.0, f32::NEG_INFINITY];
    fft::abs_in_place(&mut values);
    assert_eq!(values.map(f32::to_bits),
               [0.0_f32, 1.5, 2.0, f32::INFINITY].map(f32::to_bits));
}

fn stream(splitter: &mut FftSplitter, input: &[Vec<f32>; 2], chunks: &[usize]) -> [Vec<u32>; 4] {
    let mut result = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let mut offset = 0;
    for &count in chunks {
        assert!(offset + count <= input[0].len());
        let expected = splitter.output_frames_for(count);
        let mut full0 = vec![0.0; expected];
        let mut full1 = vec![0.0; expected];
        let mut high0 = vec![0.0; expected];
        let mut high1 = vec![0.0; expected];
        let planes = [&input[0][offset..offset + count], &input[1][offset..offset + count]];
        let mut full = [&mut full0[..], &mut full1[..]];
        let mut high = [&mut high0[..], &mut high1[..]];
        assert_eq!(splitter.process(&planes, &mut full, &mut high), expected);
        for (dst, src) in result.iter_mut().zip([full0, full1, high0, high1]) {
            dst.extend(src.into_iter().map(f32::to_bits));
        }
        offset += count;
    }
    assert_eq!(offset, input[0].len());
    result
}

#[test]
fn splitter_partition_and_reset_are_stateful() {
    let n = 1024;
    let frames = n * 2 + 31;
    let input = [
        (0..frames).map(|i| pattern(i, 0)).collect::<Vec<_>>(),
        (0..frames).map(|i| pattern(i, 1)).collect::<Vec<_>>(),
    ];
    let mut whole = FftSplitter::new(48_000, n, 2, 1500.0).unwrap();
    let mut split = FftSplitter::new(48_000, n, 2, 1500.0).unwrap();
    let expected = stream(&mut whole, &input, &[frames]);
    let observed = stream(&mut split, &input, &[0, 1, 511, 37, 600, frames - 1149]);
    assert_eq!(observed, expected);
    split.set_envelope_ratio(0.75);
    split.reset();
    assert_eq!(stream(&mut split, &input, &[frames]), expected);
}

fn words<'a>(fields: impl Iterator<Item = &'a str>) -> Vec<u32> {
    fields.map(|word| u32::from_str_radix(word, 16).expect("hex f32 word")).collect()
}

fn chunk_for(call: usize, half: usize) -> usize {
    [0, 1, half - 1, half, 7, half / 3, half, 3, half / 2 + 1][call % 9]
}

struct SplitCapture {
    counts: Vec<usize>,
    full: [Vec<u32>; 2],
    high: [Vec<u32>; 2],
}

fn expected_split(rate: u32, n: usize, mode: usize) -> SplitCapture {
    let mut splitter = FftSplitter::new(rate, n, 2, 1500.0).unwrap();
    if mode == 1 { splitter.set_envelope_ratio(0.75); }
    let total = 3 * n + 7;
    let half = n / 2;
    let mut capture = SplitCapture { counts: Vec::new(), full: [Vec::new(), Vec::new()], high: [Vec::new(), Vec::new()] };
    let (mut consumed, mut call) = (0, 0);
    while consumed < total {
        if mode == 2 && call == 4 { splitter.set_envelope_ratio(0.8); }
        if mode == 2 && call == 8 { splitter.reset(); }
        if mode == 2 && call == 9 { splitter.set_envelope_ratio(1.25); }
        let count = chunk_for(call, half).min(total - consumed);
        let input = [0, 1].map(|channel| (0..count).map(|i| {
            let at = consumed + i;
            if mode == 3 {
                if channel == 0 && at == 0 { 0.75 } else if channel == 1 && at == half { -0.5 } else { 0.0 }
            } else { pattern(at, channel) }
        }).collect::<Vec<_>>());
        let max_output = n * 4;
        let mut full0 = vec![0.0; max_output];
        let mut full1 = vec![0.0; max_output];
        let mut high0 = vec![0.0; max_output];
        let mut high1 = vec![0.0; max_output];
        let mut full = [&mut full0[..], &mut full1[..]];
        let mut high = [&mut high0[..], &mut high1[..]];
        let emitted = splitter.process(&[&input[0], &input[1]], &mut full, &mut high);
        capture.counts.push(emitted);
        for (channel, (f, h)) in [(full0, high0), (full1, high1)].into_iter().enumerate() {
            capture.full[channel].extend(f[..emitted].iter().map(|v| v.to_bits()));
            capture.high[channel].extend(h[..emitted].iter().map(|v| v.to_bits()));
        }
        consumed += count;
        call += 1;
    }
    capture
}

// This native-oracle comparator verifies the complete ARM FPCR register.
#[cfg(target_arch = "aarch64")]
#[test]
#[ignore = "requires PITCH_SPECTRAL_ORACLE_OUTPUT from the identity-guarded native oracle"]
fn original_fft_and_splitter_match_generated_signals() {
    let path = std::env::var_os("PITCH_SPECTRAL_ORACLE_OUTPUT").expect("oracle capture path");
    let text = std::fs::read_to_string(path).expect("read oracle capture");
    let mut lines = text.lines();
    let header: Vec<_> = lines.next().expect("header").split_whitespace().collect();
    assert_eq!(&header[..3], &["backend", "10", "fpcr"]);
    let original_fpcr = u64::from_str_radix(header[3], 16).unwrap();
    let current_fpcr: u64;
    unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) current_fpcr, options(nostack, preserves_flags)) };
    assert_eq!(original_fpcr, current_fpcr, "oracle/test FPCR mismatch");
    let mut fft_rows = 0;
    for n in [512, 1024, 2048] {
        let mut fft = RealFft::new(n).unwrap();
        for signal in 0..3 {
            let mut data: Vec<f32> = (0..n).map(|i| match signal {
                0 => if i == 0 { 1.0 } else { 0.0 },
                1 => if i % 2 == 0 { 1.0 } else { -1.0 },
                _ => pattern(i, 0),
            }).collect();
            fft.forward(&mut data);
            let mut line = lines.next().expect("forward row").split_whitespace();
            assert_eq!(line.next(), Some("fft_forward"));
            assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), n);
            assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), signal);
            assert_eq!(data.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), words(line));
            fft.inverse(&mut data);
            let mut line = lines.next().expect("inverse row").split_whitespace();
            assert_eq!(line.next(), Some("fft_inverse"));
            assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), n);
            assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), signal);
            assert_eq!(data.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), words(line));
            fft_rows += 2;
        }
    }
    assert_eq!(fft_rows, 18);
    let re = [0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 1, 0x8000_0001,
              0x7fc1_2345, 0x7f80_0000, 0xff80_0000, 0x3d80_0000, 0xbd80_0000,
              0x0080_0000, 0x8080_0000, 0x4000_0000, 0xc000_0000, 0x3f00_0000];
    let mut packed = vec![0.0_f32; 32];
    for i in 0..16 { packed[i] = f32::from_bits(re[i]); packed[16 + i] = pattern(i, 1); }
    packed[16] = -3.0;
    let mut polar = vec![0.0; 16];
    fft::phase(&mut packed, &mut polar);
    for (name, data) in [("phase_output", &polar), ("phase_mutated", &packed)] {
        let mut line = lines.next().expect(name).split_whitespace();
        assert_eq!(line.next(), Some(name)); assert_eq!(line.next(), Some("32")); assert_eq!(line.next(), Some("0"));
        assert_eq!(data.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), words(line), "{name}");
    }
    fft::magnitude(&mut packed, &mut polar);
    for (name, data) in [("magnitude_output", &polar), ("magnitude_mutated", &packed)] {
        let mut line = lines.next().expect(name).split_whitespace();
        assert_eq!(line.next(), Some(name)); assert_eq!(line.next(), Some("32")); assert_eq!(line.next(), Some("0"));
        assert_eq!(data.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), words(line), "{name}");
    }
    let mut mismatches = Vec::new();
    for (rate, n) in [(8000,512), (31999,512), (32000,1024), (44100,1024), (48000,1024),
                      (63999,1024), (64000,2048), (96000,2048), (127999,2048),
                      (128000,4096), (256000,8192), (384000,8192)] {
        for mode in 0..=3 {
            let generated = expected_split(rate, n, mode);
            let mut line = lines.next().expect("split counts").split_whitespace();
            assert_eq!(line.next(), Some("split_counts"));
            assert_eq!(line.next().unwrap().parse::<u32>().unwrap(), rate);
            assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), n);
            assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), mode);
            assert_eq!(generated.counts, line.map(|v| v.parse::<usize>().unwrap()).collect::<Vec<_>>(), "counts {rate} {n} {mode}");
            for channel in 0..2 {
                for (name, data) in [("split_full", &generated.full[channel]), ("split_high", &generated.high[channel])] {
                    let mut line = lines.next().expect(name).split_whitespace();
                    assert_eq!(line.next(), Some(name));
                    assert_eq!(line.next().unwrap().parse::<u32>().unwrap(), rate);
                    assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), n);
                    assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), mode);
                    assert_eq!(line.next().unwrap().parse::<usize>().unwrap(), channel);
                    let reference = words(line);
                    assert_eq!(data.len(), reference.len(), "{name} {rate} {n} {mode} channel {channel}");
                    let different = data.iter().zip(&reference).filter(|(a, b)| a != b).count();
                    if different != 0 {
                        let first = data.iter().zip(&reference).position(|(a, b)| a != b).unwrap();
                        mismatches.push(format!("{name} rate={rate} fft={n} mode={mode} channel={channel}: \
                            {different}/{} samples differ, first={first} rust={:08x} native={:08x}",
                            data.len(), data[first], reference[first]));
                    }
                }
            }
        }
    }
    assert!(lines.next().is_none(), "unexpected oracle rows");
    assert!(mismatches.is_empty(), "native sample mismatches:\n{}", mismatches.join("\n"));
}
