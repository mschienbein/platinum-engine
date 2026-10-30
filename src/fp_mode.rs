//! Scoped floating-point flush behavior used by synchronous DSP callbacks.
//!
//! ARM64 uses FPCR.FZ as the original host does. x86_64 uses MXCSR FTZ and
//! DAZ; ARM detects tininess before rounding, while x86 flushes after rounding,
//! so boundary values are not claimed bit-identical across architectures.
//! Apply the scope to the complete callback, including events and DSP chunks.
//!
//! [`without_flush_to_zero`] is the opposite scope: code whose reference
//! results are defined with gradual underflow (the AAC encoder and decoder
//! reproductions in `portable::aac`) clears the flush controls for its
//! duration, whatever the surrounding callback set.

use std::marker::PhantomData;
use std::rc::Rc;

#[cfg(target_arch = "aarch64")]
const FLUSH_TO_ZERO: u64 = 1 << 24;
#[cfg(target_arch = "x86_64")]
const FLUSH_TO_ZERO: u64 = (1 << 15) | (1 << 6); // MXCSR FTZ | DAZ

#[cfg(target_arch = "aarch64")]
#[inline]
fn read_control() -> u64 {
    let value: u64;
    unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) value, options(nostack, preserves_flags)); }
    value
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn write_control(value: u64) {
    // No nomem/readonly annotation, so loads and stores are not moved across this transition. Pure arithmetic on values that are already in registers is
    // not ordered by it: the optimiser may schedule such an operation on either side. Code whose result must see the mode takes its operands from memory and
    // delivers its results to memory (the codec stages do), or uses an asm instruction (the tests below do).
    unsafe { std::arch::asm!("msr fpcr, {value}", value = in(reg) value, options(nostack, preserves_flags)); }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn read_control() -> u64 {
    let mut value = 0u32;
    unsafe { std::arch::asm!("stmxcsr [{address}]", address = in(reg) &mut value, options(nostack, preserves_flags)); }
    u64::from(value)
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn write_control(value: u64) {
    let value = value as u32;
    unsafe { std::arch::asm!("ldmxcsr [{address}]", address = in(reg) &value, options(nostack, preserves_flags)); }
}

struct FlushToZeroGuard {
    previous_fz: u64,
    same_thread: PhantomData<Rc<()>>,
}

impl FlushToZeroGuard {
    fn enter() -> Self {
        Self::enter_with(true)
    }

    /// Set the flush controls to `flush` for the guard's lifetime.
    fn enter_with(flush: bool) -> Self {
        let previous = read_control();
        let wanted = if flush { previous | FLUSH_TO_ZERO } else { previous & !FLUSH_TO_ZERO };
        if wanted != previous {
            write_control(wanted);
        }
        Self {
            previous_fz: previous & FLUSH_TO_ZERO,
            same_thread: PhantomData,
        }
    }
}

impl Drop for FlushToZeroGuard {
    fn drop(&mut self) {
        let current = read_control();
        let restored = (current & !FLUSH_TO_ZERO) | self.previous_fz;
        if restored != current {
            write_control(restored);
        }
    }
}

/// Run one synchronous callback with the architecture's flush controls set.
///
/// Nesting and panic unwinding restore the caller's prior flush bits while
/// retaining other current control bits. Do not return a future expecting its
/// later execution to inherit this scope, or migrate work to another thread.
/// The guard itself is private and cannot cross threads.
#[inline(never)]
pub fn with_flush_to_zero<T>(body: impl FnOnce() -> T) -> T {
    let _guard = FlushToZeroGuard::enter();
    body()
}

/// Run one synchronous callback with the architecture's flush controls cleared.
///
/// The mirror of [`with_flush_to_zero`]: values that would be flushed are
/// computed with gradual underflow inside the scope. Nesting and panic
/// unwinding restore the caller's prior flush bits while retaining other
/// current control bits; nothing is written when the controls are already
/// clear. Do not return a future expecting its later execution to inherit this
/// scope, or migrate work to another thread.
#[inline(never)]
pub fn without_flush_to_zero<T>(body: impl FnOnce() -> T) -> T {
    let _guard = FlushToZeroGuard::enter_with(false);
    body()
}

/// Whether any flush control (FPCR.FZ, or MXCSR FTZ/DAZ) is currently set on this thread.
#[inline]
pub fn flush_to_zero_enabled() -> bool {
    read_control() & FLUSH_TO_ZERO != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    struct RestoreControl(u64);
    impl Drop for RestoreControl {
        fn drop(&mut self) {
            write_control(self.0);
        }
    }

    /// `a * b` as one `asm!` instruction. An ordinary multiplication is a pure operation that the optimiser may schedule on either side of the
    /// control-register writes of a scope (it did, in release builds); the `asm!` blocks are ordered with each other.
    #[cfg(target_arch = "aarch64")]
    fn multiply(a: f32, b: f32) -> f32 {
        let product: f32;
        unsafe {
            std::arch::asm!("fmul {product:s}, {a:s}, {b:s}", product = lateout(vreg) product, a = in(vreg) a, b = in(vreg) b, options(nostack, preserves_flags));
        }
        product
    }

    #[cfg(target_arch = "x86_64")]
    fn multiply(a: f32, b: f32) -> f32 {
        let mut product = a;
        unsafe {
            std::arch::asm!("mulss {product}, {b}", product = inout(xmm_reg) product, b = in(xmm_reg) b, options(nostack, preserves_flags));
        }
        product
    }

    #[test]
    fn subnormal_result_is_flushed_only_inside_scope() {
        let _restore = RestoreControl(read_control());
        write_control(read_control() & !FLUSH_TO_ZERO);
        let normal = black_box(f32::MIN_POSITIVE);
        let half = black_box(0.5_f32);
        assert_eq!(multiply(normal, half).to_bits(), 0x0040_0000);
        let inside = with_flush_to_zero(|| multiply(normal, half));
        assert_eq!(inside.to_bits(), 0);
        assert_eq!(multiply(normal, half).to_bits(), 0x0040_0000);
        assert_eq!(read_control() & FLUSH_TO_ZERO, 0);
    }

    #[test]
    fn nested_scopes_restore_the_outer_mode() {
        let _restore = RestoreControl(read_control());
        write_control(read_control() & !FLUSH_TO_ZERO);
        with_flush_to_zero(|| {
            assert_eq!(read_control() & FLUSH_TO_ZERO, FLUSH_TO_ZERO);
            with_flush_to_zero(|| assert_eq!(read_control() & FLUSH_TO_ZERO, FLUSH_TO_ZERO));
            assert_eq!(read_control() & FLUSH_TO_ZERO, FLUSH_TO_ZERO);
        });
        assert_eq!(read_control() & FLUSH_TO_ZERO, 0);
    }

    #[test]
    fn unwind_restores_the_callers_fz_bit() {
        let _restore = RestoreControl(read_control());
        write_control(read_control() & !FLUSH_TO_ZERO);
        let result = std::panic::catch_unwind(|| with_flush_to_zero(|| panic!("scope probe")));
        assert!(result.is_err());
        assert_eq!(read_control() & FLUSH_TO_ZERO, 0);
    }

    #[test]
    fn caller_enabled_fz_remains_enabled() {
        let _restore = RestoreControl(read_control());
        write_control(read_control() | FLUSH_TO_ZERO);
        with_flush_to_zero(|| assert_eq!(read_control() & FLUSH_TO_ZERO, FLUSH_TO_ZERO));
        assert_eq!(read_control() & FLUSH_TO_ZERO, FLUSH_TO_ZERO);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_enables_and_restores_ftz_and_daz_independently() {
        let _restore = RestoreControl(read_control());
        for prior in [1 << 15, 1 << 6] {
            write_control((read_control() & !FLUSH_TO_ZERO) | prior);
            with_flush_to_zero(|| assert_eq!(read_control() & FLUSH_TO_ZERO, FLUSH_TO_ZERO));
            assert_eq!(read_control() & FLUSH_TO_ZERO, prior);
        }
    }

    #[test]
    fn subnormal_result_is_kept_only_inside_cleared_scope() {
        let _restore = RestoreControl(read_control());
        write_control(read_control() | FLUSH_TO_ZERO);
        let normal = black_box(f32::MIN_POSITIVE);
        let half = black_box(0.5_f32);
        assert_eq!(multiply(normal, half).to_bits(), 0);
        let inside = without_flush_to_zero(|| multiply(normal, half));
        assert_eq!(inside.to_bits(), 0x0040_0000);
        assert_eq!(multiply(normal, half).to_bits(), 0);
        assert!(flush_to_zero_enabled());
    }

    #[test]
    fn cleared_scope_nests_inside_the_flush_scope_and_restores_it() {
        let _restore = RestoreControl(read_control());
        write_control(read_control() & !FLUSH_TO_ZERO);
        with_flush_to_zero(|| {
            assert!(flush_to_zero_enabled());
            without_flush_to_zero(|| {
                assert!(!flush_to_zero_enabled());
                with_flush_to_zero(|| assert!(flush_to_zero_enabled()));
                assert!(!flush_to_zero_enabled());
            });
            assert_eq!(read_control() & FLUSH_TO_ZERO, FLUSH_TO_ZERO);
        });
        assert!(!flush_to_zero_enabled());
    }

    #[test]
    fn cleared_scope_restores_the_callers_fz_bit_on_unwind() {
        let _restore = RestoreControl(read_control());
        write_control(read_control() | FLUSH_TO_ZERO);
        let result = std::panic::catch_unwind(|| without_flush_to_zero(|| panic!("scope probe")));
        assert!(result.is_err());
        assert_eq!(read_control() & FLUSH_TO_ZERO, FLUSH_TO_ZERO);
    }

    #[test]
    fn cleared_scope_leaves_clear_controls_untouched() {
        let _restore = RestoreControl(read_control());
        write_control(read_control() & !FLUSH_TO_ZERO);
        let before = read_control();
        without_flush_to_zero(|| assert_eq!(read_control(), before));
        assert_eq!(read_control(), before);
    }
}
