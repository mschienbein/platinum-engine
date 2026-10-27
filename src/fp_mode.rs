//! Scoped ARM64 FPCR.FZ behavior used by the original HeartCore host callback.
//!
//! Apply this to the complete synchronous process callback, including its event
//! handling and balanced DSP chunks. Parameter/editor writes occur outside it.

use std::marker::PhantomData;
use std::rc::Rc;

const FLUSH_TO_ZERO: u64 = 1 << 24;

#[inline]
fn read_fpcr() -> u64 {
    let value: u64;
    // The crate admits only the observed macOS arm64 reference target.
    unsafe {
        std::arch::asm!("mrs {value}, fpcr", value = out(reg) value, options(nostack, preserves_flags));
    }
    value
}

#[inline]
fn write_fpcr(value: u64) {
    // No nomem/readonly annotation: callers must not move memory-dependent
    // processing across this floating-point environment transition.
    unsafe {
        std::arch::asm!("msr fpcr, {value}", value = in(reg) value, options(nostack, preserves_flags));
    }
}

struct FlushToZeroGuard {
    previous_fz: u64,
    same_thread: PhantomData<Rc<()>>,
}

impl FlushToZeroGuard {
    fn enter() -> Self {
        let previous = read_fpcr();
        if previous & FLUSH_TO_ZERO == 0 {
            write_fpcr(previous | FLUSH_TO_ZERO);
        }
        Self {
            previous_fz: previous & FLUSH_TO_ZERO,
            same_thread: PhantomData,
        }
    }
}

impl Drop for FlushToZeroGuard {
    fn drop(&mut self) {
        let current = read_fpcr();
        let restored = (current & !FLUSH_TO_ZERO) | self.previous_fz;
        if restored != current {
            write_fpcr(restored);
        }
    }
}

/// Run one synchronous native-equivalent HeartCore callback with FPCR.FZ set.
///
/// Nesting and panic unwinding restore the caller's prior FZ bit while retaining
/// other current FPCR bits, matching the original host. Do not return a future
/// expecting its later execution to inherit this scope, or migrate work to
/// another thread. The guard itself is private and cannot cross threads.
#[inline(never)]
pub fn with_flush_to_zero<T>(body: impl FnOnce() -> T) -> T {
    let _guard = FlushToZeroGuard::enter();
    body()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    struct RestoreFpcr(u64);
    impl Drop for RestoreFpcr {
        fn drop(&mut self) {
            write_fpcr(self.0);
        }
    }

    #[test]
    fn subnormal_result_is_flushed_only_inside_scope() {
        let _restore = RestoreFpcr(read_fpcr());
        write_fpcr(read_fpcr() & !FLUSH_TO_ZERO);
        let normal = black_box(f32::MIN_POSITIVE);
        let half = black_box(0.5_f32);
        assert_eq!(black_box(normal * half).to_bits(), 0x0040_0000);
        let inside = with_flush_to_zero(|| black_box(normal) * black_box(half));
        assert_eq!(inside.to_bits(), 0);
        assert_eq!(black_box(normal * half).to_bits(), 0x0040_0000);
        assert_eq!(read_fpcr() & FLUSH_TO_ZERO, 0);
    }

    #[test]
    fn nested_scopes_restore_the_outer_mode() {
        let _restore = RestoreFpcr(read_fpcr());
        write_fpcr(read_fpcr() & !FLUSH_TO_ZERO);
        with_flush_to_zero(|| {
            assert_ne!(read_fpcr() & FLUSH_TO_ZERO, 0);
            with_flush_to_zero(|| assert_ne!(read_fpcr() & FLUSH_TO_ZERO, 0));
            assert_ne!(read_fpcr() & FLUSH_TO_ZERO, 0);
        });
        assert_eq!(read_fpcr() & FLUSH_TO_ZERO, 0);
    }

    #[test]
    fn unwind_restores_the_callers_fz_bit() {
        let _restore = RestoreFpcr(read_fpcr());
        write_fpcr(read_fpcr() & !FLUSH_TO_ZERO);
        let result = std::panic::catch_unwind(|| with_flush_to_zero(|| panic!("scope probe")));
        assert!(result.is_err());
        assert_eq!(read_fpcr() & FLUSH_TO_ZERO, 0);
    }

    #[test]
    fn caller_enabled_fz_remains_enabled() {
        let _restore = RestoreFpcr(read_fpcr());
        write_fpcr(read_fpcr() | FLUSH_TO_ZERO);
        with_flush_to_zero(|| assert_ne!(read_fpcr() & FLUSH_TO_ZERO, 0));
        assert_ne!(read_fpcr() & FLUSH_TO_ZERO, 0);
    }
}
