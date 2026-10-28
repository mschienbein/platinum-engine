//! The selected PitchShiftUnit ARM64 real FFT and vector dispatch leaves.
//!
//! `0x20754(-1)` installs the generic table at `0x220c4`, then replaces its
//! entries with `0x20410`'s Accelerate backend. The real FFT wrappers are
//! `0x1ba00` and `0x1bb18`, not the unrelated tuning detector's FFT. Forward
//! output is N floats: N/2 real parts followed by N/2 imaginary parts, with
//! Nyquist stored in imaginary[0]. Forward applies 0.5; inverse is unscaled.
//!
//! These primitives intentionally do not change FPCR; the complete synchronous
//! host callback owns the surrounding flush-to-zero scope.

use std::ffi::c_void;
use std::mem::MaybeUninit;

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[repr(C)]
struct SplitComplex {
    real: *mut f32,
    imaginary: *mut f32,
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::vdsp::{SplitComplex, vDSP_create_fftsetup, vDSP_destroy_fftsetup, vDSP_ctoz,
    vDSP_ztoc, vDSP_fft_zrip, vDSP_vsmul, vDSP_vmul, vDSP_vadd, vDSP_vsub, vDSP_vdiv,
    vDSP_vmax, vDSP_vabs, vDSP_maxvi, vDSP_zvphas, vDSP_zvabs};

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn vDSP_create_fftsetup(log2n: usize, radix: i32) -> *mut c_void;
    fn vDSP_destroy_fftsetup(setup: *mut c_void);
    fn vDSP_ctoz(input: *const f32, input_stride: isize, output: *mut SplitComplex,
                 output_stride: isize, length: usize);
    fn vDSP_ztoc(input: *const SplitComplex, input_stride: isize, output: *mut f32,
                 output_stride: isize, length: usize);
    fn vDSP_fft_zrip(setup: *mut c_void, data: *mut SplitComplex, stride: isize,
                     log2n: usize, direction: i32);
    fn vDSP_vsmul(input: *const f32, input_stride: isize, scale: *const f32,
                  output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vmul(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vadd(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vsub(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vdiv(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vmax(a: *const f32, a_stride: isize, b: *const f32, b_stride: isize,
                 output: *mut f32, output_stride: isize, length: usize);
    fn vDSP_vabs(input: *const f32, input_stride: isize, output: *mut f32,
                 output_stride: isize, length: usize);
    fn vDSP_maxvi(a: *const f32, stride: isize, value: *mut f32, index: *mut usize, length: usize);
    fn vDSP_zvphas(input: *const SplitComplex, input_stride: isize, output: *mut f32,
                   output_stride: isize, length: usize);
    fn vDSP_zvabs(input: *const SplitComplex, input_stride: isize, output: *mut f32,
                  output_stride: isize, length: usize);
    fn vvsincosf(sine: *mut f32, cosine: *mut f32, input: *const f32, count: *const i32);
    fn vvlogf(output: *mut f32, input: *const f32, count: *const i32);
    fn vvexpf(output: *mut f32, input: *const f32, count: *const i32);
}

// Changing the original splitter FFT's scratch alignment changed sample bits
// in controlled reference runs. Pin the aligned-0 path seen in some captures
// instead of letting unrelated heap allocations choose the scratch alignment.
#[repr(C, align(64))]
struct ScratchBlock([f32; 16]);

struct FftScratch {
    storage: Box<[MaybeUninit<ScratchBlock>]>,
    len: usize,
    #[cfg(test)]
    offset: usize,
}

impl FftScratch {
    fn new(len: usize) -> Self {
        Self {
            storage: Box::<[ScratchBlock]>::new_uninit_slice(len.div_ceil(16)),
            len,
            #[cfg(test)]
            offset: 0,
        }
    }

    // The alternate offset is a controlled experiment, not an inferred GUI
    // allocation policy. Both planes must have room for all logical samples.
    #[cfg(test)]
    fn new_offset48(len: usize) -> Self {
        let offset = 12;
        Self {
            storage: Box::<[ScratchBlock]>::new_uninit_slice((len + offset).div_ceil(16)),
            len,
            offset,
        }
    }

    fn as_mut_ptr(&mut self) -> *mut f32 {
        let ptr = self.storage.as_mut_ptr().cast::<f32>();
        // The test-only allocation reserves `offset` additional floats.
        #[cfg(test)]
        let ptr = unsafe { ptr.add(self.offset) };
        ptr
    }

    fn as_ptr(&self) -> *const f32 {
        let ptr = self.storage.as_ptr().cast::<f32>();
        #[cfg(test)]
        let ptr = unsafe { ptr.add(self.offset) };
        ptr
    }

    fn copy_from_slice(&mut self, source: &[f32]) {
        assert_eq!(source.len(), self.len);
        // The caller's slice cannot alias this private allocation.
        unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), self.as_mut_ptr(), self.len) };
    }

    // The caller must have initialized every logical sample, by copying into
    // the scratch or by completing the corresponding vDSP output operation.
    unsafe fn copy_into(&self, destination: &mut [f32]) {
        assert_eq!(destination.len(), self.len);
        unsafe {
            std::ptr::copy_nonoverlapping(self.as_ptr(), destination.as_mut_ptr(), self.len);
        }
    }
}

/// One selected radix-2 FFT, including the native wrapper's reusable scratch.
pub struct RealFft {
    len: usize,
    log2_len: usize,
    setup: *mut c_void,
    real: FftScratch,
    imaginary: FftScratch,
    time: FftScratch,
}

impl RealFft {
    /// The selected wrapper requires a power of two, at least four floats.
    pub fn new(len: usize) -> Option<Self> {
        if len < 4 || !len.is_power_of_two() || len > i32::MAX as usize {
            return None;
        }
        let log2_len = len.trailing_zeros() as usize;
        let setup = unsafe { vDSP_create_fftsetup(log2_len, 0) };
        if setup.is_null() {
            return None;
        }
        Some(Self {
            len,
            log2_len,
            setup,
            real: FftScratch::new(len / 2),
            imaginary: FftScratch::new(len / 2),
            time: FftScratch::new(len),
        })
    }

    // Only the splitter's real/imaginary work planes were changed in the
    // native intervention. Keep the time plane and other FFTs untouched.
    #[cfg(test)]
    pub(super) fn use_offset48_for_test(&mut self) {
        self.real = FftScratch::new_offset48(self.len / 2);
        self.imaginary = FftScratch::new_offset48(self.len / 2);
        assert_eq!(self.real.as_ptr() as usize % 64, 48);
        assert_eq!(self.imaginary.as_ptr() as usize % 64, 48);
    }

    /// Forward FFT, in-place, with the selected packed-real layout and 0.5 scale.
    pub fn forward(&mut self, data: &mut [f32]) {
        assert_eq!(data.len(), self.len);
        let mut split = SplitComplex { real: self.real.as_mut_ptr(), imaginary: self.imaginary.as_mut_ptr() };
        // Scratch and input are disjoint; ctoz interprets the real buffer as
        // interleaved complex pairs with the original input stride of two.
        unsafe {
            vDSP_ctoz(data.as_ptr(), 2, &mut split, 1, self.len / 2);
            vDSP_fft_zrip(self.setup, &mut split, 1, self.log2_len, 1);
        }
        let (real, imaginary) = data.split_at_mut(self.len / 2);
        // ctoz initialized both complete planes before the in-place FFT.
        unsafe {
            self.real.copy_into(real);
            self.imaginary.copy_into(imaginary);
        }
        scale_in_place(data, 0.5);
    }

    /// Inverse FFT in-place. This does not apply a 1/N normalization.
    pub fn inverse(&mut self, data: &mut [f32]) {
        assert_eq!(data.len(), self.len);
        self.inverse_packed(data);
        // inverse_packed filled all len samples through ztoc.
        unsafe { self.time.copy_into(data) };
    }

    fn inverse_packed(&mut self, input: &[f32]) {
        let (real, imaginary) = input.split_at(self.len / 2);
        self.real.copy_from_slice(real);
        self.imaginary.copy_from_slice(imaginary);
        let mut split = SplitComplex { real: self.real.as_mut_ptr(), imaginary: self.imaginary.as_mut_ptr() };
        unsafe {
            vDSP_fft_zrip(self.setup, &mut split, 1, self.log2_len, -1);
            vDSP_ztoc(&split, 1, self.time.as_mut_ptr(), 2, self.len / 2);
        }
    }

    pub fn forward_from(&mut self, input: &[f32], output: &mut [f32]) {
        assert_eq!(output.len(), self.len);
        output.copy_from_slice(input);
        self.forward(output);
    }

    pub fn inverse_from(&mut self, input: &[f32], output: &mut [f32]) {
        assert_eq!(input.len(), self.len);
        assert_eq!(output.len(), self.len);
        self.inverse_packed(input);
        // inverse_packed filled all len samples through ztoc.
        unsafe { self.time.copy_into(output) };
    }
}

impl Drop for RealFft {
    fn drop(&mut self) {
        unsafe { vDSP_destroy_fftsetup(self.setup) };
    }
}

/// `0x1f53c`, the selected scalar multiply dispatch.
pub fn scale_in_place(data: &mut [f32], factor: f32) {
    unsafe { vDSP_vsmul(data.as_ptr(), 1, &factor, data.as_mut_ptr(), 1, data.len()) };
}

/// `0x1f5d0`, the selected transient helper's in-place absolute value.
pub fn abs_in_place(data: &mut [f32]) {
    unsafe { vDSP_vabs(data.as_ptr(), 1, data.as_mut_ptr(), 1, data.len()) };
}

/// `0x1f51c`, the selected vector multiply dispatch.
pub fn multiply_in_place(data: &mut [f32], other: &[f32]) {
    assert_eq!(data.len(), other.len());
    unsafe { vDSP_vmul(data.as_ptr(), 1, other.as_ptr(), 1, data.as_mut_ptr(), 1, data.len()) };
}

/// `0x1f4dc`, the selected vector add dispatch.
pub fn add_in_place(data: &mut [f32], other: &[f32]) {
    assert_eq!(data.len(), other.len());
    unsafe { vDSP_vadd(data.as_ptr(), 1, other.as_ptr(), 1, data.as_mut_ptr(), 1, data.len()) };
}

/// `0x1f4fc`: vDSP subtracts its first vector from its second.
pub fn subtract_in_place(data: &mut [f32], other: &[f32]) {
    assert_eq!(data.len(), other.len());
    unsafe { vDSP_vsub(other.as_ptr(), 1, data.as_ptr(), 1, data.as_mut_ptr(), 1, data.len()) };
}

/// `0x1f570`: vDSP divides its second vector by its first.
pub fn divide_in_place(data: &mut [f32], denominator: &[f32]) {
    assert_eq!(data.len(), denominator.len());
    unsafe { vDSP_vdiv(denominator.as_ptr(), 1, data.as_ptr(), 1, data.as_mut_ptr(), 1, data.len()) };
}

/// `0x1f4bc`.
pub fn maximum_in_place(data: &mut [f32], other: &[f32]) {
    assert_eq!(data.len(), other.len());
    unsafe { vDSP_vmax(data.as_ptr(), 1, other.as_ptr(), 1, data.as_mut_ptr(), 1, data.len()) };
}

/// The generic `0x20bac` entry survives the Accelerate table replacement.
pub fn add_scalar_in_place(data: &mut [f32], scalar: f32) {
    for value in data {
        *value += scalar;
    }
}

/// `0x203ec` forwards to vForce with the count passed by address.
pub fn sincos(input: &[f32], sine: &mut [f32], cosine: &mut [f32]) {
    assert_eq!(input.len(), sine.len());
    assert_eq!(input.len(), cosine.len());
    let count = i32::try_from(input.len()).expect("vForce input exceeds i32");
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    unsafe { vvsincosf(sine.as_mut_ptr(), cosine.as_mut_ptr(), input.as_ptr(), &count) };
    #[cfg(any(target_os = "linux", feature = "portable"))]
    { let _ = count; crate::portable::vforce::vvsincosf(sine, cosine, input); }
}

/// `0x1ffb8`: replace real zeroes permanently and temporarily clear Nyquist.
pub fn phase(packed: &mut [f32], output: &mut [f32]) {
    assert_eq!(packed.len(), output.len() * 2);
    assert!(!output.is_empty());
    let (real, imaginary) = packed.split_at_mut(output.len());
    let nyquist = imaginary[0];
    imaginary[0] = 0.0;
    for value in real.iter_mut() {
        if *value == 0.0 {
            *value = f32::from_bits(0x322b_cc77);
        }
    }
    let split = SplitComplex { real: real.as_mut_ptr(), imaginary: imaginary.as_mut_ptr() };
    unsafe { vDSP_zvphas(&split, 1, output.as_mut_ptr(), 1, output.len()) };
    imaginary[0] = nyquist;
}

/// `0x20220`: temporarily clear and then restore the packed Nyquist value.
pub fn magnitude(packed: &mut [f32], output: &mut [f32]) {
    assert_eq!(packed.len(), output.len() * 2);
    assert!(!output.is_empty());
    let (real, imaginary) = packed.split_at_mut(output.len());
    let nyquist = imaginary[0];
    imaginary[0] = 0.0;
    let split = SplitComplex { real: real.as_mut_ptr(), imaginary: imaginary.as_mut_ptr() };
    unsafe { vDSP_zvabs(&split, 1, output.as_mut_ptr(), 1, output.len()) };
    imaginary[0] = nyquist;
}

/// `0x20280`: conjugate the ordinary bins, but not packed DC or Nyquist.
pub fn conjugate_in_place(packed: &mut [f32]) {
    assert!(packed.len() >= 4 && packed.len() % 2 == 0);
    let half = packed.len() / 2;
    scale_in_place(&mut packed[half + 1..], -1.0);
}

/// `0x1fd88`'s split-complex multiply. The two packed real-only bins are
/// multiplied independently; ordinary bins use unfused multiply/add/subtract.
pub fn multiply_spectra_in_place(data: &mut [f32], other: &[f32]) {
    assert_eq!(data.len(), other.len());
    assert!(data.len() >= 4 && data.len() % 2 == 0);
    let half = data.len() / 2;
    data[0] *= other[0];
    data[half] *= other[half];
    for index in 1..half {
        let real = data[index];
        let imaginary = data[half + index];
        let real_other = other[index];
        let imaginary_other = other[half + index];
        data[index] = real * real_other - imaginary * imaginary_other;
        data[half + index] = real_other * imaginary + real * imaginary_other;
    }
}

pub(crate) fn max_value(data: &[f32]) -> (f32, usize) {
    let mut value = f32::from_bits(0xfcf0_bdc2);
    let mut index = 0usize;
    unsafe { vDSP_maxvi(data.as_ptr(), 1, &mut value, &mut index, data.len()) };
    (value, index)
}

/// `0x1f684`: ordered `< floor` clamp, then in-place vForce natural logarithm.
pub(crate) fn log_floor_in_place(data: &mut [f32], floor: f32) {
    for value in data.iter_mut() {
        if *value < floor {
            *value = floor;
        }
    }
    let count = i32::try_from(data.len()).expect("vForce input exceeds i32");
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    unsafe { vvlogf(data.as_mut_ptr(), data.as_ptr(), &count) };
    #[cfg(any(target_os = "linux", feature = "portable"))]
    { let _ = count; crate::portable::vforce::vvlogf_in_place(data); }
}

pub(crate) fn exp_in_place(data: &mut [f32]) {
    let count = i32::try_from(data.len()).expect("vForce input exceeds i32");
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    unsafe { vvexpf(data.as_mut_ptr(), data.as_ptr(), &count) };
    #[cfg(any(target_os = "linux", feature = "portable"))]
    { let _ = count; crate::portable::vforce::vvexpf_in_place(data); }
}

