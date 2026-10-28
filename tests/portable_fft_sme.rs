//! Bitwise regression against captured aligned Accelerate complex outputs.
use platinum_engine::portable::arm::{self, FpMode};
#[allow(clippy::disallowed_methods)]
#[path = "../src/portable/fft_sme.rs"]
mod fft_sme;

const SMOKE: &[u8; 49152] = include_bytes!("../tools/portable/fft_sme/smoke.bin");
// Captured from the same native complex wrappers in both FZ modes. Each
// direction/size/mode has cases 1, 5, 7, 8 and 11 from the parity harness.
const STRUCTURED: &[u8; 983040] = include_bytes!("../tools/portable/fft_sme/structured.bin");

fn expected(offset: usize, index: usize) -> u32 {
    let p = offset + index * 4;
    u32::from_le_bytes(SMOKE[p..p + 4].try_into().unwrap())
}

fn check_reference(mode: FpMode) {
    for (log2n, length, real_offset, imag_offset) in
        [(12, 2048, 0, 8192), (13, 4096, 16384, 32768)]
    {
        let mut real: Vec<f32> = (0..length)
            .map(|i| ((i % 41) as i32 - 20) as f32 * (1.0 / 32.0))
            .collect();
        let mut imag: Vec<f32> = (0..length)
            .map(|i| ((i % 47) as i32 - 23) as f32 * (1.0 / 32.0))
            .collect();
        fft_sme::complex(log2n, &mut real, &mut imag, mode);
        for i in 0..length {
            assert_eq!(real[i].to_bits(), expected(real_offset, i), "log2n={log2n} real[{i}]");
            assert_eq!(imag[i].to_bits(), expected(imag_offset, i), "log2n={log2n} imag[{i}]");
        }
    }
}

#[test]
fn captured_forward_complex_gradual() {
    check_reference(FpMode::IEEE);
}

#[test]
fn captured_forward_complex_flush() {
    platinum_engine::fp_mode::with_flush_to_zero(|| check_reference(FpMode::FZ));
}

#[test]
fn za_operations_use_default_nan_even_when_ambient_dn_is_clear() {
    let mut real = vec![0.0; 2048];
    let mut imag = vec![0.0; 2048];
    real[0] = f32::from_bits(0xffc0_1234);
    fft_sme::complex(12, &mut real, &mut imag, arm::FpMode::IEEE);
    assert!(real.iter().chain(&imag).any(|v| v.to_bits() == 0x7fc0_0000));
    assert!(!real.iter().chain(&imag).any(|v| v.is_nan() && v.to_bits() != 0x7fc0_0000));
}

fn structured_input(case: usize, index: usize, imaginary: bool) -> f32 {
    let b = usize::from(imaginary);
    match case {
        1 => if index == 0 && !imaginary { 1.0 } else { 0.0 },
        5 => f32::from_bits([0, 0x8000_0000, 1, 0x8000_0001, 0x007f_ffff, 0x807f_ffff,
                              0x0080_0000, 0x8080_0000][(index + b) % 8]),
        7 => f32::from_bits([0x7f80_0000, 0xff80_0000, 0, 0x3f80_0000][(index + b) % 4]),
        8 => f32::from_bits([0x7fc0_0001, 0xffc0_1234, 0x7f80_0001, 0xff80_0002,
                              0x3f80_0000, 0][(index + b) % 6]),
        11 => if index == 31 && imaginary { f32::from_bits(1) } else { f32::from_bits(0x0080_0000) },
        _ => unreachable!(),
    }
}

#[test]
fn captured_structured_both_directions_and_modes() {
    let mut offset = 0;
    for (log2n, length) in [(12, 2048), (13, 4096)] {
        for inverse in [false, true] {
            for flush in [false, true] {
                for case in [1, 5, 7, 8, 11] {
                    let mut real: Vec<f32> = (0..length).map(|i| structured_input(case, i, false)).collect();
                    let mut imag: Vec<f32> = (0..length).map(|i| structured_input(case, i, true)).collect();
                    let mode = if flush { FpMode::FZ } else { FpMode::IEEE };
                    let mut transform = || {
                        if inverse { fft_sme::complex(log2n, &mut imag, &mut real, mode); }
                        else { fft_sme::complex(log2n, &mut real, &mut imag, mode); }
                    };
                    if flush { platinum_engine::fp_mode::with_flush_to_zero(transform); }
                    else { transform(); }
                    for (plane, values) in [&real, &imag].into_iter().enumerate() {
                        for (index, value) in values.iter().enumerate() {
                            let p = offset + (plane * length + index) * 4;
                            let expected = u32::from_le_bytes(STRUCTURED[p..p + 4].try_into().unwrap());
                            assert_eq!(value.to_bits(), expected,
                                "log2n={log2n} inverse={inverse} flush={flush} case={case} plane={plane} index={index}");
                        }
                    }
                    offset += length * 8;
                }
            }
        }
    }
    assert_eq!(offset, STRUCTURED.len());
}
