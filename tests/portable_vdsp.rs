#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod system {

use platinum_engine::portable::vdsp::{self, SplitComplex};

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    #[link_name = "vDSP_create_fftsetup"]
    fn system_create_fftsetup(log2n: usize, radix: i32) -> *mut std::ffi::c_void;
    #[link_name = "vDSP_destroy_fftsetup"]
    fn system_destroy_fftsetup(setup: *mut std::ffi::c_void);
    #[link_name = "vDSP_fft_zrip"]
    fn system_fft_zrip(setup: *mut std::ffi::c_void, data: *mut SplitComplex,
                       stride: isize, log2n: usize, direction: i32);
    #[link_name = "vDSP_dotpr"]
    fn system_dotpr(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                    result: *mut f32, length: usize);
    #[link_name = "vDSP_rmsqv"]
    fn system_rmsqv(a: *const f32, stride: isize, result: *mut f32, length: usize);
    #[link_name = "vDSP_maxvi"]
    fn system_maxvi(a: *const f32, stride: isize, value: *mut f32, index: *mut usize, length: usize);
    #[link_name = "vDSP_ctoz"]
    fn system_ctoz(input: *const f32, input_stride: isize, output: *mut SplitComplex,
                   output_stride: isize, length: usize);
    #[link_name = "vDSP_ztoc"]
    fn system_ztoc(input: *const SplitComplex, input_stride: isize, output: *mut f32,
                   output_stride: isize, length: usize);
    #[link_name = "vDSP_vmul"]
    fn system_vmul(a: *const f32, ia: isize, b: *const f32, ib: isize, out: *mut f32, io: isize, n: usize);
    #[link_name = "vDSP_vadd"]
    fn system_vadd(a: *const f32, ia: isize, b: *const f32, ib: isize, out: *mut f32, io: isize, n: usize);
    #[link_name = "vDSP_vsub"]
    fn system_vsub(a: *const f32, ia: isize, b: *const f32, ib: isize, out: *mut f32, io: isize, n: usize);
    #[link_name = "vDSP_vdiv"]
    fn system_vdiv(a: *const f32, ia: isize, b: *const f32, ib: isize, out: *mut f32, io: isize, n: usize);
    #[link_name = "vDSP_vmax"]
    fn system_vmax(a: *const f32, ia: isize, b: *const f32, ib: isize, out: *mut f32, io: isize, n: usize);
    #[link_name = "vDSP_vsmul"]
    fn system_vsmul(a: *const f32, ia: isize, factor: *const f32, out: *mut f32, io: isize, n: usize);
    #[link_name = "vDSP_vabs"]
    fn system_vabs(a: *const f32, ia: isize, out: *mut f32, io: isize, n: usize);
    #[link_name = "vDSP_zvabs"]
    fn system_zvabs(input: *const SplitComplex, input_stride: isize,
                    output: *mut f32, output_stride: isize, length: usize);
    #[link_name = "vDSP_zvphas"]
    fn system_zvphas(input: *const SplitComplex, input_stride: isize,
                     output: *mut f32, output_stride: isize, length: usize);
}

struct Random(u64);
impl Random {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
    fn audio(&mut self) -> f32 {
        (self.next() as i32 as f32) * (1.0 / 2147483648.0)
    }
}

fn dot_vectors(cases: usize) -> (usize, usize, Vec<String>) {
    let mut random = Random(0x8db4_9bd1_72f3_2345);
    let mut a = [0.0; 256];
    let mut b = [0.0; 256];
    let mut tested = 0;
    let mut mismatches = 0;
    let mut examples = Vec::new();
    for len in (0..=159).chain([256]) {
        for case in 0..cases {
            for value in &mut a[..len] { *value = random.audio(); }
            for value in &mut b[..len] { *value = random.audio(); }
            let (mut expected, mut actual) = (0.0, 0.0);
            unsafe {
                system_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut expected, len);
                vdsp::vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut actual, len);
            }
            tested += 1;
            if expected.to_bits() != actual.to_bits() {
                mismatches += 1;
                if examples.len() < 12 {
                    examples.push(format!("len={len} case={case} reference={:08x} portable={:08x}", expected.to_bits(), actual.to_bits()));
                }
            }
        }
    }
    (tested, mismatches, examples)
}

#[test]
fn dotpr_sampled_unit_stride() {
    let (tested, mismatches, examples) = dot_vectors(64);
    assert_eq!(mismatches, 0, "{tested} vectors; examples: {examples:?}");
}

#[test]
fn dotpr_sampled_unit_stride_with_fz() {
    let (tested, mismatches, examples) =
        platinum_engine::fp_mode::with_flush_to_zero(|| dot_vectors(64));
    assert_eq!(mismatches, 0, "{tested} vectors; examples: {examples:?}");
}

#[test]
#[ignore = "diagnostic: arbitrary binary32 input classes"]
fn dotpr_arbitrary_bits_diagnostic() {
    let mut random = Random(0xd8b4_9bd1_72f3_2345);
    let mut a = [0.0; 256];
    let mut b = [0.0; 256];
    let mut count = 0usize;
    let mut mismatches = 0usize;
    for len in (1..=159).chain([256]) {
        for case in 0..512 {
            for v in &mut a[..len] { *v = f32::from_bits(random.next()); }
            for v in &mut b[..len] { *v = f32::from_bits(random.next()); }
            let (mut expected, mut actual) = (0.0, 0.0);
            unsafe {
                system_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut expected, len);
                vdsp::vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut actual, len);
            }
            count += 1;
            if expected.to_bits() != actual.to_bits() {
                mismatches += 1;
                if mismatches <= 8 {
                    eprintln!("len={len} case={case} expected={:08x} actual={:08x} a={:?} b={:?}",
                        expected.to_bits(), actual.to_bits(),
                        a[..len].iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                        b[..len].iter().map(|v| v.to_bits()).collect::<Vec<_>>());
                }
            }
        }
    }
    eprintln!("arbitrary binary32: {count} vectors, {mismatches} mismatches");
    assert_eq!(mismatches, 0);
}

#[test]
fn dot_nan_order_diagnostic() {
    let nan = [0x7f80_0001, 0x7fc0_0002, 0xff80_0003, 0xffc0_0004];
    for len in [1usize, 4, 6] {
        let index = if len == 6 { 4 } else { 0 };
        for a_bits in nan {
            for b_bits in nan {
                let mut a = [0.0f32; 6];
                let mut b = [1.0f32; 6];
                a[index] = f32::from_bits(a_bits);
                b[index] = f32::from_bits(b_bits);
                let (mut result, mut actual) = (0.0f32, 0.0f32);
                unsafe {
                    system_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut result, len);
                    vdsp::vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut actual, len);
                }
                assert_eq!(result.to_bits(), actual.to_bits(), "len={len} a={a_bits:08x} b={b_bits:08x}");
            }
        }
    }
    for first in nan {
        for second in nan {
            let mut a = [0.0f32; 16];
            let b = [1.0f32; 16];
            a[0] = f32::from_bits(first);
            a[8] = f32::from_bits(second);
            let (mut result, mut actual) = (0.0f32, 0.0f32);
            unsafe {
                system_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut result, 16);
                vdsp::vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut actual, 16);
            }
            assert_eq!(result.to_bits(), actual.to_bits(), "accum_first={first:08x} accum_second={second:08x}");
        }
    }
    for first in nan {
        for second in nan {
            let mut a = [0.0f32; 8];
            let b = [1.0f32; 8];
            a[0] = f32::from_bits(first);
            a[4] = f32::from_bits(second);
            let (mut result, mut actual) = (0.0f32, 0.0f32);
            unsafe {
                system_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut result, 8);
                vdsp::vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut actual, 8);
            }
            assert_eq!(result.to_bits(), actual.to_bits(), "fold_first={first:08x} fold_second={second:08x}");
        }
    }
    for (ax, bx) in [(f32::INFINITY, 0.0), (0.0, f32::INFINITY),
                     (f32::INFINITY, -1.0)] {
        let mut a = [0.0f32; 16];
        let mut b = [1.0f32; 16];
        a[0] = f32::from_bits(0x7fc0_1234);
        a[8] = ax;
        b[8] = bx;
        let (mut result, mut actual) = (0.0f32, 0.0f32);
        unsafe {
            system_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut result, 16);
            vdsp::vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut actual, 16);
        }
        assert_eq!(result.to_bits(), actual.to_bits(), "invalid product");
    }
}

#[test]
fn complex_layout_unit_stride() {
    let mut random = Random(0x917b_29c5_8213_47a1);
    for len in [0, 1, 3, 4, 7, 16, 256, 512, 2048, 16384] {
        let input: Vec<f32> = (0..2 * len).map(|_| f32::from_bits(random.next())).collect();
        let mut real = vec![0.0; len];
        let mut imag = vec![0.0; len];
        let mut reference_real = vec![0.0; len];
        let mut reference_imag = vec![0.0; len];
        let mut portable = SplitComplex { real: real.as_mut_ptr(), imaginary: imag.as_mut_ptr() };
        let mut reference = SplitComplex { real: reference_real.as_mut_ptr(), imaginary: reference_imag.as_mut_ptr() };
        unsafe {
            vdsp::vDSP_ctoz(input.as_ptr(), 2, &mut portable, 1, len);
            system_ctoz(input.as_ptr(), 2, &mut reference, 1, len);
        }
        assert_eq!(real.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), reference_real.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
        assert_eq!(imag.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), reference_imag.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
        let mut interleaved = vec![0.0; 2 * len];
        let mut reference_interleaved = vec![0.0; 2 * len];
        unsafe {
            vdsp::vDSP_ztoc(&portable, 1, interleaved.as_mut_ptr(), 2, len);
            system_ztoc(&reference, 1, reference_interleaved.as_mut_ptr(), 2, len);
        }
        assert_eq!(interleaved.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), reference_interleaved.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
    }
}

#[derive(Clone, Copy)]
#[repr(C, align(64))]
#[allow(dead_code)] // Storage is accessed as one flat, aligned raw allocation.
struct AlignedBlock([f32; 16]);

fn fft_wrapper_check() {
    let mut random = Random(0xc6a8_1294_6d31_50bb);
    for log2n in 9..=15 {
        let half = 1usize << (log2n - 1);
        let mut ref_real = vec![AlignedBlock([0.0; 16]); half / 16];
        let mut ref_imaginary = vec![AlignedBlock([0.0; 16]); half / 16];
        let mut own_real = vec![AlignedBlock([0.0; 16]); half / 16];
        let mut own_imaginary = vec![AlignedBlock([0.0; 16]); half / 16];
        let mut reference = SplitComplex { real: ref_real.as_mut_ptr().cast(), imaginary: ref_imaginary.as_mut_ptr().cast() };
        let mut portable = SplitComplex { real: own_real.as_mut_ptr().cast(), imaginary: own_imaginary.as_mut_ptr().cast() };
        let native_setup = unsafe { system_create_fftsetup(log2n, 0) };
        let own_setup = unsafe { vdsp::vDSP_create_fftsetup(log2n, 0) };
        assert!(!native_setup.is_null() && !own_setup.is_null());
        for direction in [1, -1] {
            for index in 0..half {
                let real = random.audio();
                let imaginary = random.audio();
                unsafe {
                    reference.real.add(index).write(real);
                    reference.imaginary.add(index).write(imaginary);
                    portable.real.add(index).write(real);
                    portable.imaginary.add(index).write(imaginary);
                }
            }
            unsafe {
                system_fft_zrip(native_setup, &mut reference, 1, log2n, direction);
                vdsp::vDSP_fft_zrip(own_setup, &mut portable, 1, log2n, direction);
            }
            for index in 0..half {
                unsafe {
                    assert_eq!(reference.real.add(index).read().to_bits(), portable.real.add(index).read().to_bits(),
                               "log2n={log2n} direction={direction} real index={index}");
                    assert_eq!(reference.imaginary.add(index).read().to_bits(), portable.imaginary.add(index).read().to_bits(),
                               "log2n={log2n} direction={direction} imaginary index={index}");
                }
            }
        }
        unsafe {
            system_destroy_fftsetup(native_setup);
            vdsp::vDSP_destroy_fftsetup(own_setup);
        }
    }
}

#[test]
fn fft_wrappers_sampled() {
    fft_wrapper_check();
    platinum_engine::fp_mode::with_flush_to_zero(fft_wrapper_check);
}

fn rms_diagnostic(raw: bool, cases: usize) -> (usize, Vec<String>) {
    let mut random = Random(0x5327_1495_b671_0893);
    let mut input = [0.0f32; 4096];
    let mut mismatches = 0;
    let mut examples = Vec::new();
    for case in 0..cases {
        for value in &mut input {
            *value = if raw { f32::from_bits(random.next()) } else { random.audio() };
        }
        let (mut expected, mut actual) = (0.0, 0.0);
        unsafe {
            system_rmsqv(input.as_ptr(), 1, &mut expected, input.len());
            vdsp::vDSP_rmsqv(input.as_ptr(), 1, &mut actual, input.len());
        }
        if expected.to_bits() != actual.to_bits() {
            mismatches += 1;
            if examples.len() < 6 {
                examples.push(format!("case={case} expected={:08x} actual={:08x}", expected.to_bits(), actual.to_bits()));
            }
        }
    }
    (mismatches, examples)
}

#[test]
#[ignore = "diagnostic: RMS input classes"]
fn rms_input_diagnostic() {
    for fz in [false, true] {
        for raw in [false, true] {
            let (mismatches, examples) = if fz {
                platinum_engine::fp_mode::with_flush_to_zero(|| rms_diagnostic(raw, 1024))
            } else {
                rms_diagnostic(raw, 1024)
            };
            eprintln!("fz={fz} arbitrary_bits={raw} vectors=1024 mismatches={mismatches} examples={examples:?}");
            assert_eq!(mismatches, 0, "fz={fz} arbitrary_bits={raw} examples={examples:?}");
        }
    }
}

fn maxvi_diagnostic() -> (usize, usize, Vec<String>) {
    let mut random = Random(0x9238_5429_1567_abcd);
    let mut tested = 0;
    let mut mismatches = 0;
    let mut examples = Vec::new();
    for len in (0..=70).chain([127, 128, 129, 255, 256, 257, 259, 260, 511, 512, 513, 1024, 4096, 16384]) {
        let mut input = vec![0.0f32; len];
        for case in 0..256 {
            for x in &mut input {
                *x = match case % 5 {
                    0 => f32::from_bits(random.next()),
                    1 => ((random.next() & 7) as i32 - 4) as f32,
                    2 => f32::from_bits([0xff80_0000, 0x7f80_0001, 0x7fc0_0002][random.next() as usize % 3]),
                    3 => f32::from_bits([0, 0x8000_0000, 1, 0x8000_0001, 0x007f_ffff][random.next() as usize % 5]),
                    _ => random.audio(),
                };
            }
            let (mut expected, mut actual) = (1.0f32, 1.0f32);
            let (mut expected_index, mut actual_index) = (usize::MAX, usize::MAX);
            unsafe {
                system_maxvi(input.as_ptr(), 1, &mut expected, &mut expected_index, len);
                vdsp::vDSP_maxvi(input.as_ptr(), 1, &mut actual, &mut actual_index, len);
            }
            tested += 1;
            if expected.to_bits() != actual.to_bits() || expected_index != actual_index {
                mismatches += 1;
                if examples.len() < 8 {
                    examples.push(format!("len={len} case={case} expected={:08x}/{expected_index} actual={:08x}/{actual_index} first={:?}",
                        expected.to_bits(), actual.to_bits(), input.iter().take(16).map(|x| format!("{:08x}", x.to_bits())).collect::<Vec<_>>()));
                }
            }
        }
    }
    (tested, mismatches, examples)
}

#[test]
#[ignore = "diagnostic: maximum tie and exceptional cases"]
fn maxvi_input_diagnostic() {
    for fz in [false, true] {
        let (tested, mismatches, examples) = if fz {
            platinum_engine::fp_mode::with_flush_to_zero(maxvi_diagnostic)
        } else {
            maxvi_diagnostic()
        };
        eprintln!("fz={fz} vectors={tested} mismatches={mismatches} examples={examples:?}");
        assert_eq!(mismatches, 0, "fz={fz} examples={examples:?}");
    }
}

fn magnitude_diagnostic(raw: bool) -> (usize, usize, Vec<String>) {
    let mut random = Random(0x8390_2745_b671_0893);
    let mut tested = 0;
    let mut mismatches = 0;
    let mut examples = Vec::new();
    for len in [1, 3, 4, 7, 16, 256, 512, 1024, 2048, 4096, 8192, 16384] {
        let mut re = vec![0.0; len];
        let mut im = vec![0.0; len];
        let mut expected = vec![0.0; len];
        let mut actual = vec![0.0; len];
        for _ in 0..32 {
            for v in &mut re { *v = if raw { f32::from_bits(random.next()) } else { random.audio() }; }
            for v in &mut im { *v = if raw { f32::from_bits(random.next()) } else { random.audio() }; }
            let input = SplitComplex { real: re.as_mut_ptr(), imaginary: im.as_mut_ptr() };
            unsafe {
                system_zvabs(&input, 1, expected.as_mut_ptr(), 1, len);
                vdsp::vDSP_zvabs(&input, 1, actual.as_mut_ptr(), 1, len);
            }
            for i in 0..len {
                tested += 1;
                if expected[i].to_bits() != actual[i].to_bits() {
                    mismatches += 1;
                    if examples.len() < 6 {
                        examples.push(format!("len={len} index={i} re={:08x} im={:08x} expected={:08x} actual={:08x}",
                            re[i].to_bits(), im[i].to_bits(), expected[i].to_bits(), actual[i].to_bits()));
                    }
                }
            }
        }
    }
    (tested, mismatches, examples)
}

#[test]
#[ignore = "diagnostic: magnitude input classes"]
fn magnitude_input_diagnostic() {
    for fz in [false, true] {
        for raw in [false, true] {
            let (tested, mismatches, examples) = if fz {
                platinum_engine::fp_mode::with_flush_to_zero(|| magnitude_diagnostic(raw))
            } else {
                magnitude_diagnostic(raw)
            };
            eprintln!("fz={fz} arbitrary_bits={raw} samples={tested} mismatches={mismatches} examples={examples:?}");
            assert_eq!(mismatches, 0, "fz={fz} arbitrary_bits={raw} examples={examples:?}");
        }
    }
}

fn phase_diagnostic(raw: bool) -> (usize, usize, Vec<String>) {
    let mut random = Random(0x82d2_1174_a619_c3a8);
    let mut tested = 0;
    let mut mismatches = 0;
    let mut examples = Vec::new();
    for len in [4, 16, 256, 512, 1024, 2048, 4096, 8192, 16384] {
        let mut re = vec![0.0; len];
        let mut im = vec![0.0; len];
        let mut expected = vec![0.0; len + 4];
        let mut actual = vec![0.0; len + 4];
        let expected_offset = ((16 - expected.as_ptr() as usize % 16) % 16) / 4;
        let actual_offset = ((16 - actual.as_ptr() as usize % 16) % 16) / 4;
        for _ in 0..64 {
            for v in &mut re { *v = if raw { f32::from_bits(random.next()) } else { random.audio() }; }
            for v in &mut im { *v = if raw { f32::from_bits(random.next()) } else { random.audio() }; }
            let input = SplitComplex { real: re.as_mut_ptr(), imaginary: im.as_mut_ptr() };
            unsafe {
                system_zvphas(&input, 1, expected.as_mut_ptr().add(expected_offset), 1, len);
                vdsp::vDSP_zvphas(&input, 1, actual.as_mut_ptr().add(actual_offset), 1, len);
            }
            for i in 0..len {
                tested += 1;
                if expected[expected_offset + i].to_bits() != actual[actual_offset + i].to_bits() {
                    mismatches += 1;
                    if examples.len() < 8 {
                        examples.push(format!("len={len} index={i} re={:08x} im={:08x} expected={:08x} actual={:08x}",
                            re[i].to_bits(), im[i].to_bits(), expected[expected_offset + i].to_bits(), actual[actual_offset + i].to_bits()));
                    }
                }
            }
        }
    }
    (tested, mismatches, examples)
}

#[test]
#[ignore = "diagnostic: phase input classes"]
fn phase_input_diagnostic() {
    for fz in [false, true] {
        for raw in [false, true] {
            let (tested, mismatches, examples) = if fz {
                platinum_engine::fp_mode::with_flush_to_zero(|| phase_diagnostic(raw))
            } else {
                phase_diagnostic(raw)
            };
            eprintln!("fz={fz} arbitrary_bits={raw} samples={tested} mismatches={mismatches} examples={examples:?}");
            assert_eq!(mismatches, 0, "fz={fz} arbitrary_bits={raw} examples={examples:?}");
        }
    }
}

fn phase_alignment_diagnostic() -> (usize, usize, Vec<String>) {
    let mut random = Random(0xd289_c974_f9a6_03b8);
    let mut tested = 0;
    let mut mismatches = 0;
    let mut examples = Vec::new();
    for len in 0..=40 {
        for offset in 0..4 {
            for stride in [1isize, 2] {
                let mut re = vec![0.0f32; len * stride as usize + 1];
                let mut im = vec![0.0f32; len * stride as usize + 1];
                let mut expected = vec![0.0f32; len * stride as usize + 8];
                let mut actual = vec![0.0f32; len * stride as usize + 8];
                let ref_base = ((16 - expected.as_ptr() as usize % 16) % 16) / 4 + offset;
                let own_base = ((16 - actual.as_ptr() as usize % 16) % 16) / 4 + offset;
                for case in 0..128 {
                    for v in &mut re { *v = if case % 2 == 0 { f32::from_bits(random.next()) } else { random.audio() }; }
                    for v in &mut im { *v = if case % 2 == 0 { f32::from_bits(random.next()) } else { random.audio() }; }
                    let input = SplitComplex { real: re.as_mut_ptr(), imaginary: im.as_mut_ptr() };
                    unsafe {
                        system_zvphas(&input, stride, expected.as_mut_ptr().add(ref_base), stride, len);
                        vdsp::vDSP_zvphas(&input, stride, actual.as_mut_ptr().add(own_base), stride, len);
                    }
                    for i in 0..len {
                        tested += 1;
                        let dst = i * stride as usize;
                        if expected[ref_base + dst].to_bits() != actual[own_base + dst].to_bits() {
                            mismatches += 1;
                            if examples.len() < 8 {
                                examples.push(format!("len={len} offset={offset} stride={stride} index={i} re={:08x} im={:08x} expected={:08x} actual={:08x}",
                                    re[dst].to_bits(), im[dst].to_bits(), expected[ref_base + dst].to_bits(), actual[own_base + dst].to_bits()));
                            }
                        }
                    }
                }
            }
        }
    }
    (tested, mismatches, examples)
}

#[test]
#[ignore = "diagnostic: phase alignment and scalar paths"]
fn phase_alignment_input_diagnostic() {
    for fz in [false, true] {
        let (tested, mismatches, examples) = if fz {
            platinum_engine::fp_mode::with_flush_to_zero(phase_alignment_diagnostic)
        } else {
            phase_alignment_diagnostic()
        };
        eprintln!("fz={fz} samples={tested} mismatches={mismatches} examples={examples:?}");
        assert_eq!(mismatches, 0, "fz={fz} examples={examples:?}");
    }
}

fn unary_diagnostic() -> (usize, usize, Vec<String>) {
    let mut random = Random(0xc80d_13c3_9e36_528a);
    let factors = [0, 0x8000_0000, 1, 0x3f80_0000, 0xbf80_0000,
                   0x7f80_0000, 0xff80_0000, 0x7f80_0001, 0xffc0_0002];
    let mut tested = 0;
    let mut mismatches = 0;
    let mut examples = Vec::new();
    for len in [0usize, 1, 2, 3, 4, 7, 16, 256, 512, 2048, 4096, 8192, 16384, 20480, 32768] {
        let mut input = vec![0.0f32; len];
        let mut expected = vec![0.0f32; len];
        let mut actual = vec![0.0f32; len];
        for case in 0..18 {
            for value in &mut input { *value = f32::from_bits(random.next()); }
            if len >= factors.len() {
                for (dst, bits) in input.iter_mut().zip(factors) { *dst = f32::from_bits(bits); }
            }
            let factor = f32::from_bits(factors[case % factors.len()]);
            for inplace in [false, true] {
                for is_abs in [false, true] {
                    expected.copy_from_slice(&input);
                    actual.copy_from_slice(&input);
                    unsafe {
                        let reference_input = if inplace { expected.as_ptr() } else { input.as_ptr() };
                        let portable_input = if inplace { actual.as_ptr() } else { input.as_ptr() };
                        if is_abs {
                            system_vabs(reference_input, 1, expected.as_mut_ptr(), 1, len);
                            vdsp::vDSP_vabs(portable_input, 1, actual.as_mut_ptr(), 1, len);
                        } else {
                            system_vsmul(reference_input, 1, &factor, expected.as_mut_ptr(), 1, len);
                            vdsp::vDSP_vsmul(portable_input, 1, &factor, actual.as_mut_ptr(), 1, len);
                        }
                    }
                    for i in 0..len {
                        tested += 1;
                        if expected[i].to_bits() != actual[i].to_bits() {
                            mismatches += 1;
                            if examples.len() < 8 {
                                examples.push(format!("len={len} index={i} abs={is_abs} inplace={inplace} x={:08x} factor={:08x} expected={:08x} actual={:08x}",
                                    input[i].to_bits(), factor.to_bits(), expected[i].to_bits(), actual[i].to_bits()));
                            }
                        }
                    }
                }
            }
        }
    }
    (tested, mismatches, examples)
}

#[test]
#[ignore = "diagnostic: unary input classes and aliasing"]
fn unary_input_diagnostic() {
    for fz in [false, true] {
        let (tested, mismatches, examples) = if fz {
            platinum_engine::fp_mode::with_flush_to_zero(unary_diagnostic)
        } else {
            unary_diagnostic()
        };
        eprintln!("fz={fz} samples={tested} mismatches={mismatches} examples={examples:?}");
        assert_eq!(mismatches, 0, "fz={fz} examples={examples:?}");
    }
}

fn large_mul_matrix() {
    let values = [0, 0x8000_0000, 1, 0x8000_0001, 0x007f_ffff, 0x0080_0000,
                  0x3f80_0000, 0xbf80_0000, 0x7f7f_ffff, 0x7f80_0000, 0xff80_0000,
                  0x7f80_0001, 0xffc0_0002];
    let input: Vec<f32> = (0..32768).map(|i| f32::from_bits(values[i % values.len()])).collect();
    for f in values {
        let factor = f32::from_bits(f);
        let mut reference = vec![0.0f32; input.len()];
        let mut actual = vec![0.0f32; input.len()];
        unsafe {
            system_vsmul(input.as_ptr(), 1, &factor, reference.as_mut_ptr(), 1, input.len());
            vdsp::vDSP_vsmul(input.as_ptr(), 1, &factor, actual.as_mut_ptr(), 1, input.len());
        }
        assert_eq!(reference.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                   actual.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), "factor={f:08x}");
    }
}

#[test]
#[ignore = "diagnostic: AMX scalar multiplication"]
fn large_mul_matrix_diagnostic() {
    eprintln!("fz=0");
    large_mul_matrix();
    eprintln!("fz=1");
    platinum_engine::fp_mode::with_flush_to_zero(large_mul_matrix);
}

type NativeBinary = unsafe extern "C" fn(*const f32, isize, *const f32, isize, *mut f32, isize, usize);
type PortableBinary = unsafe fn(*const f32, isize, *const f32, isize, *mut f32, isize, usize);

fn vector_binary_diagnostic() -> Vec<(String, usize, Vec<String>)> {
    let mut reports = Vec::new();
    let operations: [(&str, NativeBinary, PortableBinary); 5] = [
        ("vadd", system_vadd, vdsp::vDSP_vadd),
        ("vsub", system_vsub, vdsp::vDSP_vsub),
        ("vmul", system_vmul, vdsp::vDSP_vmul),
        ("vdiv", system_vdiv, vdsp::vDSP_vdiv),
        ("vmax", system_vmax, vdsp::vDSP_vmax),
    ];
    for (name, native, portable) in operations {
        let mut random = Random(0xa453_d917_8234_e13a);
        let mut mismatches = 0;
        let mut examples = Vec::new();
        for len in (1..=40).chain([256, 512, 16384, 20479, 20480, 32768]) {
            let mut a = vec![0.0; len];
            let mut b = vec![0.0; len];
            let mut expected = vec![0.0; len];
            let mut actual = vec![0.0; len];
            for case in 0..64 {
                for v in &mut a { *v = f32::from_bits(random.next()); }
                for v in &mut b { *v = f32::from_bits(random.next()); }
                unsafe {
                    native(a.as_ptr(), 1, b.as_ptr(), 1, expected.as_mut_ptr(), 1, len);
                    portable(a.as_ptr(), 1, b.as_ptr(), 1, actual.as_mut_ptr(), 1, len);
                }
                for index in 0..len {
                    if expected[index].to_bits() != actual[index].to_bits() {
                        mismatches += 1;
                        if examples.len() < 4 {
                            examples.push(format!("len={len} case={case} index={index} a={:08x} b={:08x} expected={:08x} actual={:08x}",
                                a[index].to_bits(), b[index].to_bits(), expected[index].to_bits(), actual[index].to_bits()));
                        }
                    }
                }
            }
        }
        reports.push((name.to_owned(), mismatches, examples));
    }
    reports
}

fn vector_special_diagnostic() {
    let values = [0, 0x8000_0000, 1, 0x8000_0001, 0x007f_ffff, 0x0080_0000,
                  0x3f80_0000, 0xbf80_0000, 0x7f80_0000, 0xff80_0000,
                  0x7f80_0001, 0x7fc0_0002, 0xff80_0003, 0xffc0_0004];
    let operations: [(&str, NativeBinary, PortableBinary); 5] = [
        ("vadd", system_vadd, vdsp::vDSP_vadd),
        ("vsub", system_vsub, vdsp::vDSP_vsub),
        ("vmul", system_vmul, vdsp::vDSP_vmul),
        ("vdiv", system_vdiv, vdsp::vDSP_vdiv),
        ("vmax", system_vmax, vdsp::vDSP_vmax),
    ];
    for (name, native, portable) in operations {
        let mut mismatches = 0;
        let mut examples = Vec::new();
        for len in [1usize, 4, 32] {
            for a_bits in values {
                for b_bits in values {
                    let a = [f32::from_bits(a_bits); 32];
                    let b = [f32::from_bits(b_bits); 32];
                    let mut expected = [0.0f32; 32];
                    let mut actual = [0.0f32; 32];
                    unsafe {
                        native(a.as_ptr(), 1, b.as_ptr(), 1, expected.as_mut_ptr(), 1, len);
                        portable(a.as_ptr(), 1, b.as_ptr(), 1, actual.as_mut_ptr(), 1, len);
                    }
                    if expected[0].to_bits() != actual[0].to_bits() {
                        mismatches += 1;
                        if examples.len() < 12 {
                            examples.push(format!("len={len} a={a_bits:08x} b={b_bits:08x} expected={:08x} actual={:08x}",
                                expected[0].to_bits(), actual[0].to_bits()));
                        }
                    }
                }
            }
        }
        eprintln!("op={name} mismatches={mismatches} examples={examples:?}");
        assert_eq!(mismatches, 0, "{name} examples={examples:?}");
    }
}

#[test]
fn vector_special_cases_diagnostic() {
    eprintln!("fz=0");
    vector_special_diagnostic();
    eprintln!("fz=1");
    platinum_engine::fp_mode::with_flush_to_zero(vector_special_diagnostic);
}

#[test]
#[ignore = "diagnostic: arbitrary binary32 input classes"]
fn vector_binary_arbitrary_bits_diagnostic() {
    for fz in [false, true] {
        let reports = if fz {
            platinum_engine::fp_mode::with_flush_to_zero(vector_binary_diagnostic)
        } else {
            vector_binary_diagnostic()
        };
        for (name, mismatches, examples) in reports {
            eprintln!("fz={fz} {name} mismatches={mismatches} examples={examples:?}");
            assert_eq!(mismatches, 0, "fz={fz} {name} examples={examples:?}");
        }
    }
}

#[test]
#[ignore = "long-running comparison; set VDSP_CASES, VDSP_LENGTH, VDSP_FZ and VDSP_RECEIPT"]
fn dotpr_receipt() {
    use std::io::Write;
    let cases: usize = std::env::var("VDSP_CASES").expect("VDSP_CASES").parse().unwrap();
    let len: usize = std::env::var("VDSP_LENGTH").expect("VDSP_LENGTH").parse().unwrap();
    assert!(len <= 256);
    let fz = std::env::var("VDSP_FZ").expect("VDSP_FZ") == "1";
    let path = std::env::var_os("VDSP_RECEIPT").expect("VDSP_RECEIPT");
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap();
    let exe = std::env::current_exe().unwrap();
    let fingerprint = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"]).arg(&exe).output().unwrap();
    assert!(fingerprint.status.success());
    let source_hash = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256", "src/portable/vdsp.rs"]).output().unwrap();
    assert!(source_hash.status.success());
    writeln!(out, "routine=vDSP_dotpr size={len} strides=1,1 fz={} seed=0x8db49bd172f32345 requested_vectors={cases} classes=raw-bits,audio,subnormal-mixed,raw-bits", u8::from(fz)).unwrap();
    writeln!(out, "macos=27.2 build=26B123 libvDSP_uuid=79FFEC1C-BD65-3324-907A-7FEFC636A526").unwrap();
    writeln!(out, "binary_sha256={}", String::from_utf8_lossy(&fingerprint.stdout).trim()).unwrap();
    writeln!(out, "source_sha256={}", String::from_utf8_lossy(&source_hash.stdout).trim()).unwrap();
    writeln!(out, "command={}", std::env::var("VDSP_COMMAND").unwrap_or_default()).unwrap();
    out.flush().unwrap();
    let start = std::time::Instant::now();
    let run = || {
        let mut fpcr = 0u64;
        unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) fpcr, options(nostack, preserves_flags)); }
        assert_eq!(fpcr & (1 << 24) != 0, fz, "unexpected FPCR.FZ");
        let mut random = Random(0x8db4_9bd1_72f3_2345);
        let mut a = [0.0f32; 256];
        let mut b = [0.0f32; 256];
        let mut mismatches = 0usize;
        let mut examples = Vec::new();
        for case in 0..cases {
            for value in &mut a[..len] {
                *value = match case % 4 {
                    0 | 3 => f32::from_bits(random.next()),
                    1 => random.audio(),
                    _ => f32::from_bits(random.next() & 0x807f_ffff),
                };
            }
            for value in &mut b[..len] {
                *value = if case % 4 == 1 || case % 4 == 2 { random.audio() } else { f32::from_bits(random.next()) };
            }
            let (mut expected, mut actual) = (0.0f32, 0.0f32);
            unsafe {
                system_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut expected, len);
                vdsp::vDSP_dotpr(a.as_ptr(), 1, b.as_ptr(), 1, &mut actual, len);
            }
            if expected.to_bits() != actual.to_bits() {
                mismatches += 1;
                if examples.len() < 12 {
                    examples.push(format!("case={case} reference={:08x} portable={:08x}", expected.to_bits(), actual.to_bits()));
                }
            }
        }
        (mismatches, examples)
    };
    let (mismatches, examples) = if fz { platinum_engine::fp_mode::with_flush_to_zero(run) } else { run() };
    writeln!(out, "tested_vectors={cases} mismatches={mismatches} elapsed_seconds={:.3}", start.elapsed().as_secs_f64()).unwrap();
    for example in examples { writeln!(out, "mismatch {example}").unwrap(); }
    out.flush().unwrap();
    assert_eq!(mismatches, 0);
}

include!("../tools/portable/vdsp/receipt.rs");
}

#[test]
fn portable_reference_cases() {
    use platinum_engine::portable::vdsp::{self, SplitComplex};

    let input = [1.0f32, -2.0, 3.0, 4.0];
    let mut re = [0.0f32; 2];
    let mut im = [0.0f32; 2];
    let mut split = SplitComplex { real: re.as_mut_ptr(), imaginary: im.as_mut_ptr() };
    let mut interleaved = [0.0f32; 4];
    unsafe {
        vdsp::vDSP_ctoz(input.as_ptr(), 2, &mut split, 1, 2);
        vdsp::vDSP_ztoc(&split, 1, interleaved.as_mut_ptr(), 2, 2);
    }
    assert_eq!(interleaved, input);

    let b = [4.0f32, 3.0, 2.0, 1.0];
    let mut dot = 0.0f32;
    unsafe { vdsp::vDSP_dotpr(input.as_ptr(), 1, b.as_ptr(), 1, &mut dot, 4); }
    assert_eq!(dot.to_bits(), 8.0f32.to_bits());

    let invalid = [1.0f32, f32::from_bits(0x7fc0_0001), 3.0, 2.0];
    let (mut maximum, mut index) = (0.0f32, usize::MAX);
    unsafe { vdsp::vDSP_maxvi(invalid.as_ptr(), 1, &mut maximum, &mut index, invalid.len()); }
    assert_eq!((maximum.to_bits(), index), (f32::NEG_INFINITY.to_bits(), 0));

    let zeros = [-0.0f32, 0.0];
    unsafe { vdsp::vDSP_maxvi(zeros.as_ptr(), 1, &mut maximum, &mut index, zeros.len()); }
    assert_eq!((maximum.to_bits(), index), (0x8000_0000, 0));
    let mut chunks = [0.0f32; 512];
    chunks[0] = 100.0;
    chunks[5] = f32::from_bits(0x7fc0_0001);
    chunks[256..].fill(1.0);
    unsafe { vdsp::vDSP_maxvi(chunks.as_ptr(), 1, &mut maximum, &mut index, chunks.len()); }
    assert_eq!((maximum.to_bits(), index), (1.0f32.to_bits(), 256));

    #[repr(align(16))]
    struct Aligned([f32; 4]);
    let mut phase_re = [1.0f32, -1.0, 1.0, -1.0];
    let mut phase_im = [0.0f32, 0.0, -0.0, -0.0];
    let phase_input = SplitComplex { real: phase_re.as_mut_ptr(), imaginary: phase_im.as_mut_ptr() };
    let mut output = Aligned([0.0; 4]);
    unsafe { vdsp::vDSP_zvphas(&phase_input, 1, output.0.as_mut_ptr(), 1, 4); }
    assert_eq!(output.0.map(f32::to_bits), [0x8000_0000, 0x4049_0fdb, 0x8000_0000, 0x4049_0fdb]);

    let frame = [1.0f32; 4096];
    let mut rms = 0.0f32;
    unsafe { vdsp::vDSP_rmsqv(frame.as_ptr(), 1, &mut rms, frame.len()); }
    assert_eq!(rms.to_bits(), 1.0f32.to_bits());

    let small = f32::from_bits(1);
    let long = vec![small; 32768];
    let mut product = vec![0.0f32; long.len()];
    let unity = 1.0f32;
    unsafe { vdsp::vDSP_vsmul(long.as_ptr(), 1, &unity, product.as_mut_ptr(), 1, long.len()); }
    assert!(product.iter().all(|x| x.to_bits() == 1));
}

#[test]
fn portable_fz_fused_boundary() {
    use platinum_engine::portable::vdsp::vDSP_dotpr;
    let got = platinum_engine::fp_mode::with_flush_to_zero(|| {
        let min = std::hint::black_box(f32::MIN_POSITIVE);
        let b = [1.0f32, min];
        let positive = [min, -min];
        let negative = [-min, min];
        let above = [min, min];
        let twice = f32::from_bits(0x0100_0000);
        let below = [twice, -f32::from_bits(0x0080_0001)];
        let below_b = [1.0f32, f32::from_bits(0x3f7f_ffff)];
        let exact = [twice, -min];
        let ones = [1.0f32, 1.0];
        let mut result = [0.0f32; 5];
        unsafe {
            vDSP_dotpr(positive.as_ptr(), 1, b.as_ptr(), 1, &mut result[0], 2);
            vDSP_dotpr(negative.as_ptr(), 1, b.as_ptr(), 1, &mut result[1], 2);
            vDSP_dotpr(above.as_ptr(), 1, b.as_ptr(), 1, &mut result[2], 2);
            vDSP_dotpr(below.as_ptr(), 1, below_b.as_ptr(), 1, &mut result[3], 2);
            vDSP_dotpr(exact.as_ptr(), 1, ones.as_ptr(), 1, &mut result[4], 2);
        }
        result.map(f32::to_bits)
    });
    assert_eq!(got, [0, 0x8000_0000, 0x0080_0000, 0, 0x0080_0000]);
}
