//! Bitwise regressions for portable vForce, including mixed-lane reduction.
use platinum_engine::portable::vforce;

#[test]
fn fixed_vector_regressions() {
    let input = [f32::from_bits(0x4009_4abc), 0.0, -0.0];
    let mut output = [0.0; 3];
    vforce::vvlogf(&mut output, &input);
    assert_eq!(output.map(f32::to_bits), [0x3f43_62d1, 0xff80_0000, 0xff80_0000]);
    let mut exp = [f32::from_bits(0x4004_6f43), 0.0, -0.0];
    vforce::vvexpf_in_place(&mut exp);
    assert_eq!(exp.map(f32::to_bits), [0x40fd_6a21, 0x3f80_0000, 0x3f80_0000]);

    let mut sine = [0.0; 2];
    let mut cosine = [0.0; 2];
    vforce::vvsincosf(&mut sine[..1], &mut cosine[..1], &[0.0]);
    assert_eq!((sine[0].to_bits(), cosine[0].to_bits()), (0, 0x3f7f_ffff));
    vforce::vvsincosf(&mut sine, &mut cosine, &[0.0, f32::MAX]);
    assert_eq!((sine[0].to_bits(), cosine[0].to_bits()), (0, 0x3f80_0000));
    vforce::vvsincosf(&mut sine, &mut cosine, &[f32::INFINITY, f32::NEG_INFINITY]);
    assert_eq!(sine.map(f32::to_bits), [0xffc0_0000, 0x7fc0_0000]);
    assert_eq!(cosine.map(f32::to_bits), [0xffc0_0000, 0x7fc0_0000]);
}

#[test]
fn empty_slices_and_length_contracts() {
    vforce::vvsincosf(&mut [], &mut [], &[]);
    vforce::vvlogf(&mut [], &[]);
    vforce::vvexpf(&mut [], &[]);
    vforce::vvlogf_in_place(&mut []);
    vforce::vvexpf_in_place(&mut []);
    assert!(std::panic::catch_unwind(|| vforce::vvlogf(&mut [0.0], &[])).is_err());
    assert!(std::panic::catch_unwind(|| vforce::vvexpf(&mut [], &[0.0])).is_err());
    assert!(std::panic::catch_unwind(|| vforce::vvsincosf(&mut [0.0], &mut [], &[0.0])).is_err());
}

#[test]
fn large_values_select_only_their_own_sixteen_lane_group() {
    let mut input = [0.0; 17];
    input[15] = f32::MAX;
    let mut sine = [0.0; 17];
    let mut cosine = [0.0; 17];
    vforce::vvsincosf(&mut sine, &mut cosine, &input);
    assert_eq!(cosine[0].to_bits(), 0x3f80_0000);
    assert_eq!(cosine[14].to_bits(), 0x3f80_0000);
    assert_eq!(cosine[16].to_bits(), 0x3f7f_ffff);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod system {
    use super::vforce;
    use platinum_engine::fp_mode::with_flush_to_zero;

    #[link(name = "Accelerate", kind = "framework")]
    unsafe extern "C" {
        #[link_name = "vvsincosf"] fn system_sincos(s: *mut f32, c: *mut f32, x: *const f32, n: *const i32);
        #[link_name = "vvlogf"] fn system_log(out: *mut f32, x: *const f32, n: *const i32);
        #[link_name = "vvexpf"] fn system_exp(out: *mut f32, x: *const f32, n: *const i32);
    }

    fn next(state: &mut u64) -> u32 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) as u32
    }

    fn compare() {
        let mut state = 0x8d85_b019_a8ec_7231;
        let special = [0, 0x8000_0000, 1, 0x8000_0001, 0x007f_ffff,
            0x0080_0000, 0x7f80_0000, 0xff80_0000, 0x7f80_0001,
            0xff80_0001, 0x7fc1_2345, 0xffc1_2345, 0x4974_2400, 0x4974_2401];
        for len in (0..=33).chain([63, 64, 65, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768]) {
            for offset in 0..16 {
                let mut xs = vec![0.0; len + 16];
                for (i, x) in xs[offset..offset + len].iter_mut().enumerate() {
                    let r = next(&mut state);
                    *x = f32::from_bits(match i % 4 {
                        0 => special[r as usize % special.len()],
                        1 => r,
                        2 => ((r as i32 as f64 / 2_147_483_648.0 * 16.0) as f32).to_bits(),
                        _ => r & 0x807f_ffff,
                    });
                }
                let x = &xs[offset..offset + len];
                let mut sine = vec![0.0; len]; let mut cosine = vec![0.0; len];
                let mut expected_sine = vec![0.0; len]; let mut expected_cosine = vec![0.0; len];
                let n = len as i32;
                unsafe { system_sincos(expected_sine.as_mut_ptr(), expected_cosine.as_mut_ptr(), x.as_ptr(), &n); }
                vforce::vvsincosf(&mut sine, &mut cosine, x);
                for i in 0..len {
                    assert_eq!((sine[i].to_bits(), cosine[i].to_bits()),
                        (expected_sine[i].to_bits(), expected_cosine[i].to_bits()), "sincos len={len} offset={offset} i={i} x={:08x}", x[i].to_bits());
                }
                for is_log in [true, false] {
                    let mut expected = x.to_vec();
                    let mut actual = expected.clone();
                    unsafe {
                        if is_log { system_log(expected.as_mut_ptr(), expected.as_ptr(), &n); }
                        else { system_exp(expected.as_mut_ptr(), expected.as_ptr(), &n); }
                    }
                    if is_log { vforce::vvlogf_in_place(&mut actual); }
                    else { vforce::vvexpf_in_place(&mut actual); }
                    for i in 0..len {
                        assert_eq!(actual[i].to_bits(), expected[i].to_bits(),
                            "{} len={len} offset={offset} i={i} x={:08x}", if is_log { "log" } else { "exp" }, x[i].to_bits());
                    }
                }
            }
        }
    }

    #[test] fn sampled_native_vectors() { compare(); }
    #[test] fn sampled_native_vectors_with_flush() { with_flush_to_zero(compare); }
}
