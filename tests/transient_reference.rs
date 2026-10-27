//! Direct DSP tests. The ignored differential test explicitly loads the pinned
//! local HeartCore copy as a comparison oracle; the Rust implementation does not.

use platinum_engine::transient::{TransientMeters, TransientParameters, TransientShaper};

fn parameters() -> TransientParameters {
    TransientParameters {
        attack: f32::from_bits(0x3d27_9140),
        pump: f32::from_bits(0x3da6_5512),
        sustain: 0.0,
        speed: f32::from_bits(0x3f60_6818),
        clip: false,
    }
}

fn signal(frame: usize, channel: usize) -> f32 {
    (((frame * 17 + channel * 53) % 257) as i32 - 128) as f32 / 128.0
}

fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|v| v.to_bits()).collect()
}

#[test]
fn neutral_model_values_preserve_finite_stereo_sample_bits() {
    let mut p = parameters();
    p.attack = 0.0;
    p.pump = 0.0;
    p.sustain = 0.0;
    let mut dsp = TransientShaper::new(48_000.0, p);
    let mut left: Vec<_> = (0..257).map(|i| signal(i, 0)).collect();
    let mut right: Vec<_> = (0..257).map(|i| signal(i, 1)).collect();
    left[1] = -0.0;
    right[3] = -0.0;
    let expected_left = bits(&left);
    let expected_right = bits(&right);
    dsp.process_chunk(&mut left, &mut right, None);
    assert_eq!(bits(&left), expected_left);
    assert_eq!(bits(&right), expected_right);
}

#[test]
fn stereo_detector_is_linked_and_symmetric() {
    let mut original = TransientShaper::new(44_100.0, parameters());
    let mut swapped = TransientShaper::new(44_100.0, parameters());
    let mut left: Vec<_> = (0..512).map(|i| signal(i, 0)).collect();
    let mut right: Vec<_> = (0..512).map(|i| signal(i, 1)).collect();
    let mut left_swapped = right.clone();
    let mut right_swapped = left.clone();
    let meters = original.process_chunk(&mut left, &mut right, None);
    let swapped_meters = swapped.process_chunk(&mut left_swapped, &mut right_swapped, None);
    assert_eq!(bits(&left), bits(&right_swapped));
    assert_eq!(bits(&right), bits(&left_swapped));
    assert_eq!(meters, swapped_meters);
}

#[test]
fn explicit_detector_uses_its_samples_and_not_output_channels() {
    let mut internal = TransientShaper::new(48_000.0, parameters());
    let mut external = TransientShaper::new(48_000.0, parameters());
    let mut muted_detector = TransientShaper::new(48_000.0, parameters());
    let mut left: Vec<_> = (0..512).map(|i| signal(i, 0)).collect();
    let mut right: Vec<_> = (0..512).map(|i| signal(i, 1)).collect();
    let side_left = left.clone();
    let side_right = right.clone();
    let mut left_external = left.clone();
    let mut right_external = right.clone();
    let mut left_muted = left.clone();
    let mut right_muted = right.clone();
    let zeros = vec![0.0; left.len()];
    internal.process_chunk(&mut left, &mut right, None);
    external.process_chunk(&mut left_external, &mut right_external, Some((&side_left, &side_right)));
    muted_detector.process_chunk(&mut left_muted, &mut right_muted, Some((&zeros, &zeros)));
    assert_eq!(bits(&left), bits(&left_external));
    assert_eq!(bits(&right), bits(&right_external));
    assert_ne!(bits(&left), bits(&left_muted));
}

#[test]
fn active_processing_is_continuous_across_chunks_and_reset_restores_state() {
    let p = parameters();
    let mut whole = TransientShaper::new(96_000.0, p);
    let mut parts = TransientShaper::new(96_000.0, p);
    let input_left: Vec<_> = (0..257).map(|i| signal(i, 0)).collect();
    let input_right: Vec<_> = (0..257).map(|i| signal(i, 1)).collect();
    let mut whole_left = input_left.clone();
    let mut whole_right = input_right.clone();
    let mut part_left = input_left.clone();
    let mut part_right = input_right.clone();
    whole.process_chunk(&mut whole_left, &mut whole_right, None);
    for range in [0..17, 17..49, 49..50, 50..113, 113..257] {
        parts.process_chunk(&mut part_left[range.clone()], &mut part_right[range], None);
    }
    assert_eq!(bits(&whole_left), bits(&part_left));
    assert_eq!(bits(&whole_right), bits(&part_right));
    parts.reset();
    let mut reset_left = input_left;
    let mut reset_right = input_right;
    parts.process_chunk(&mut reset_left, &mut reset_right, None);
    assert_eq!(bits(&reset_left), bits(&whole_left));
    assert_eq!(bits(&reset_right), bits(&whole_right));
}

#[test]
fn soft_clip_preserves_sign_and_has_the_native_fused_curve() {
    let mut p = parameters();
    p.attack = 0.0;
    p.pump = 0.0;
    p.sustain = 0.0;
    p.clip = true;
    let mut dsp = TransientShaper::new(48_000.0, p);
    let mut left = [0.0, -0.0, 0.5, 1.0];
    let mut right = [-1.0, -0.5, 0.25, 1.0];
    dsp.process_chunk(&mut left, &mut right, None);
    let excess = 1.0_f32.mul_add(10.0, -9.0);
    let curved = excess.mul_add(excess, excess);
    let expected = (curved * 0.1_f32) / (curved + 1.0) + 0.9_f32;
    assert_eq!(bits(&left), vec![0.0_f32.to_bits(), (-0.0_f32).to_bits(), 0.5_f32.to_bits(), expected.to_bits()]);
    assert_eq!(right[0].to_bits(), (-expected).to_bits());
    assert_eq!(right[1].to_bits(), (-0.5_f32).to_bits());
}

#[test]
fn sleeping_fast_path_is_not_replaced_with_active_zero_processing() {
    let p = parameters();
    let mut asleep = TransientShaper::new(48_000.0, p);
    let mut active_zeros = TransientShaper::new(48_000.0, p);
    let mut left = vec![1.0; 64];
    let mut right = vec![0.5; 64];
    let mut other_left = left.clone();
    let mut other_right = right.clone();
    asleep.process_chunk(&mut left, &mut right, None);
    active_zeros.process_chunk(&mut other_left, &mut other_right, None);
    for _ in 0..32 {
        asleep.advance_sleeping_chunk(64);
        let mut zero_left = [0.0; 64];
        let mut zero_right = [0.0; 64];
        active_zeros.process_chunk(&mut zero_left, &mut zero_right, None);
    }
    let mut wake_left = [0.125; 64];
    let mut wake_right = [-0.25; 64];
    let mut other_wake_left = wake_left;
    let mut other_wake_right = wake_right;
    asleep.process_chunk(&mut wake_left, &mut wake_right, None);
    active_zeros.process_chunk(&mut other_wake_left, &mut other_wake_right, None);
    assert_ne!(bits(&wake_left), bits(&other_wake_left));
}

// The remaining test deliberately calls private native DSP addresses in the
// SHA-256-pinned local binary. It is opt-in and checks this kernel, not the
// larger Multipass signal graph, plugin host, or encoded output.
mod native_oracle {
    use super::*;
    use libloading::Library;
    use platinum_engine::fp_mode::with_flush_to_zero;
    use std::path::Path;
    use std::process::Command;

    const SHA256: &str = "dcf7b668fd94812a41aedad2082fbe8581fd887f0331ade7d858ab53dbca0856";
    const UNIVERSAL_SHA256: &str = "35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578";
    const KSTR_EXPORT: usize = 0xc13b40;
    const MODEL_BYTES: usize = 0xc200;
    type Constructor = unsafe extern "C" fn(*mut u8) -> *mut u8;
    type Destructor = unsafe extern "C" fn(*mut u8) -> *mut u8;
    type Rate = unsafe extern "C" fn(*mut u8, f32);
    type Reset = unsafe extern "C" fn(*mut u8);
    type Process = unsafe extern "C" fn(*mut u8, *mut u8);

    fn store<T: Copy>(base: *mut u8, offset: usize, value: T) {
        unsafe { base.add(offset).cast::<T>().write_unaligned(value) }
    }
    fn load<T: Copy>(base: *const u8, offset: usize) -> T {
        unsafe { base.add(offset).cast::<T>().read_unaligned() }
    }

    struct Native {
        library: Library,
        dsp: Box<[u64; 24]>,
        model: Box<[u64]>,
        destructor: Destructor,
        rate: Rate,
        reset: Reset,
        process: Process,
        sleep: Process,
    }

    impl Native {
        unsafe fn new(path: &Path, rate: f32, params: TransientParameters) -> Self {
            let digest = Command::new("shasum").arg("-a").arg("256").arg(path).output().expect("shasum unavailable");
            assert!(digest.status.success());
            let hash_output = String::from_utf8(digest.stdout).unwrap();
            let actual = hash_output.split_whitespace().next();
            assert!(actual == Some(SHA256) || actual == Some(UNIVERSAL_SHA256), "unrecognized original core identity");
            let library = unsafe { Library::new(path) }.expect("load pinned HeartCore oracle");
            let entry = unsafe { library.get::<unsafe extern "C" fn()>(b"VSTPluginMain_kstr\0") }
                .expect("kstr export missing");
            let base = *entry as usize - KSTR_EXPORT;
            assert_eq!(unsafe { (base as *const u32).read_unaligned() }, 0xfeed_facf, "Mach-O64 image");
            assert_eq!(unsafe { ((base + 4) as *const u32).read_unaligned() }, 0x0100_000c, "arm64 image");
            let ctor: Constructor = unsafe { std::mem::transmute(base + 0xc09a3c) };
            let destructor: Destructor = unsafe { std::mem::transmute(base + 0xc09b28) };
            let set_rate: Rate = unsafe { std::mem::transmute(base + 0xc09c68) };
            let reset: Reset = unsafe { std::mem::transmute(base + 0xc09ba8) };
            let process: Process = unsafe { std::mem::transmute(base + 0xc0a004) };
            let sleep: Process = unsafe { std::mem::transmute(base + 0xc09df8) };
            let mut dsp = Box::new([0u64; 24]);
            let mut model = vec![0u64; MODEL_BYTES / 8].into_boxed_slice();
            let dsp_ptr = dsp.as_mut_ptr().cast::<u8>();
            let model_ptr = model.as_mut_ptr().cast::<u8>();
            unsafe { ctor(dsp_ptr); }
            store(dsp_ptr, 8, model_ptr);
            // Already-notified flags prevent the isolated fixture from calling
            // the absent UI/model notification graph.
            for offset in [0xc10b, 0xc14b, 0xc18b] { store(model_ptr, offset, 1u8); }
            let mut result = Self { library, dsp, model, destructor, rate: set_rate, reset, process, sleep };
            result.set_parameters(params);
            unsafe { (result.rate)(result.dsp_ptr(), rate); (result.reset)(result.dsp_ptr()); }
            result
        }

        fn dsp_ptr(&mut self) -> *mut u8 { self.dsp.as_mut_ptr().cast() }
        fn model_ptr(&mut self) -> *mut u8 { self.model.as_mut_ptr().cast() }
        fn set_parameters(&mut self, p: TransientParameters) {
            let model = self.model_ptr();
            store(model, 0xbf94, p.attack);
            store(model, 0xbfd4, p.pump);
            store(model, 0xc014, p.sustain);
            store(model, 0xc054, p.speed);
            store(model, 0xc094, u8::from(p.clip));
        }
        fn set_rate(&mut self, rate: f32) {
            unsafe { (self.rate)(self.dsp_ptr(), rate); }
        }
        fn reset(&mut self) {
            unsafe { (self.reset)(self.dsp_ptr()); }
        }
        fn meters(&self) -> TransientMeters {
            let p = self.model.as_ptr().cast::<u8>();
            TransientMeters { transient: load(p, 0xc0d4), sustain: load(p, 0xc114), pump: load(p, 0xc154) }
        }
        fn process(&mut self, left: &mut [f32], right: &mut [f32], count: usize, detector: Option<(&[f32], &[f32])>) -> TransientMeters {
            assert_eq!(left.len(), right.len());
            assert!(count <= left.len() && left.len() % 4 == 0);
            let mut context = [0u64; 14];
            let context_ptr = context.as_mut_ptr().cast::<u8>();
            store(context_ptr, 0, left.as_mut_ptr());
            store(context_ptr, 8, right.as_mut_ptr());
            store(context_ptr, 0x28, count as u32);
            let mut sidechain = [0usize; 3];
            let model = self.model_ptr();
            if let Some((l, r)) = detector {
                assert_eq!(l.len(), count);
                assert_eq!(r.len(), count);
                sidechain[0] = l.as_ptr() as usize;
                sidechain[1] = r.as_ptr() as usize;
                store(context_ptr, 0x18, sidechain.as_mut_ptr());
                store(model, 0xc1e0, 0i32);
            } else {
                store(model, 0xc1e0, -1i32);
            }
            unsafe { (self.process)(self.dsp_ptr(), context_ptr); }
            self.meters()
        }
        fn sleep(&mut self, count: u32) -> TransientMeters {
            let mut context = [0u64; 14];
            let context_ptr = context.as_mut_ptr().cast::<u8>();
            store(context_ptr, 0x28, count);
            unsafe { (self.sleep)(self.dsp_ptr(), context_ptr); }
            self.meters()
        }
    }
    impl Drop for Native {
        fn drop(&mut self) {
            unsafe { (self.destructor)(self.dsp_ptr()); }
            // Keep the image mapped until its destructor has returned.
            let _ = &self.library;
        }
    }

    fn compare_meters(actual: TransientMeters, expected: TransientMeters) {
        assert_eq!(actual.transient.to_bits(), expected.transient.to_bits(), "transient meter");
        assert_eq!(actual.sustain.to_bits(), expected.sustain.to_bits(), "sustain meter");
        assert_eq!(actual.pump.to_bits(), expected.pump.to_bits(), "pump meter");
    }

    fn compare_sleep(native: &mut Native, rust: &mut TransientShaper, count: u32) {
        with_flush_to_zero(|| {
            compare_meters(rust.advance_sleeping_chunk(count), native.sleep(count));
        });
    }

    fn compare_chunk(native: &mut Native, rust: &mut TransientShaper, start: usize, count: usize, use_sidechain: bool) {
        let padded = count.next_multiple_of(4);
        let mut native_left = vec![0.0; padded];
        let mut native_right = vec![0.0; padded];
        let mut side_left = vec![0.0; count];
        let mut side_right = vec![0.0; count];
        for i in 0..count {
            native_left[i] = signal(start + i, 0);
            native_right[i] = signal(start + i, 1);
            if (start + i) % 97 == 0 {
                native_left[i] = f32::from_bits(1);
                native_right[i] = f32::from_bits(0x8000_0001);
            }
            side_left[i] = signal((start + i) / 2, 1) * 0.5;
            side_right[i] = if (start + i) % 41 < 6 { 1.25 } else { -0.125 };
        }
        let mut rust_left = native_left[..count].to_vec();
        let mut rust_right = native_right[..count].to_vec();
        let side = if use_sidechain { Some((&side_left[..], &side_right[..])) } else { None };
        let (expected, actual) = with_flush_to_zero(|| {
            (
                native.process(&mut native_left, &mut native_right, count, side),
                rust.process_chunk(&mut rust_left, &mut rust_right, side),
            )
        });
        assert_eq!(bits(&rust_left), bits(&native_left[..count]), "left at frame {start}");
        assert_eq!(bits(&rust_right), bits(&native_right[..count]), "right at frame {start}");
        compare_meters(actual, expected);
    }

    #[test]
    #[ignore = "requires PLATINUM_TRANSIENT_ORACLE pointing to the pinned local HeartCore arm64 slice"]
    fn matches_direct_native_active_and_sleeping_dsp() {
        let path = std::env::var_os("PLATINUM_TRANSIENT_ORACLE").expect("set PLATINUM_TRANSIENT_ORACLE");
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            for sidechain in [false, true] {
                for clip in [false, true] {
                    let mut p = parameters();
                    p.clip = clip;
                    let mut native = unsafe { Native::new(Path::new(&path), rate, p) };
                    let mut rust = TransientShaper::new(rate, p);
                    // Initial silence can take the native sleep method before the
                    // first active chunk, retaining constructor time constants.
                    for count in [64, 17] { compare_sleep(&mut native, &mut rust, count); }
                    let mut start = 0;
                    for count in [1, 17, 32, 50, 33, 64] {
                        compare_chunk(&mut native, &mut rust, start, count, sidechain);
                        start += count;
                    }
                    p.attack = -0.25;
                    p.pump = 0.375;
                    p.sustain = -0.5;
                    p.speed = 0.5;
                    native.set_parameters(p);
                    rust.set_parameters(p);
                    compare_chunk(&mut native, &mut rust, start, 64, sidechain);
                    native.set_rate(88_200.0);
                    rust.set_sample_rate(88_200.0);
                    compare_chunk(&mut native, &mut rust, start + 64, 50, sidechain);
                    for count in [64, 31, 64] { compare_sleep(&mut native, &mut rust, count); }
                    compare_chunk(&mut native, &mut rust, start + 114, 33, sidechain);
                    for (offset, speed) in [(147, 0.0), (180, 1.0)] {
                        p.speed = speed;
                        native.set_parameters(p);
                        rust.set_parameters(p);
                        compare_chunk(&mut native, &mut rust, start + offset, 33, sidechain);
                    }
                    native.reset();
                    rust.reset();
                    compare_chunk(&mut native, &mut rust, start + 213, 64, sidechain);
                }
            }
        }
    }
}
