#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
)))]
compile_error!("supported targets are macOS arm64, Linux x86_64, and Linux arm64");

mod math;
mod native_library;
#[cfg(all(target_os = "macos", not(feature = "portable")))]
pub mod apple_codec;
#[cfg(any(target_os = "linux", feature = "portable"))]
pub use portable::audio as apple_codec;
pub mod audio_input;
pub mod clip;
pub mod controller;
pub mod distortion;
pub mod dynamics;
pub mod engine;
pub mod error_code;
pub mod file_workflow;
pub mod flac;
pub mod fp_mode;
pub mod limiter;
pub mod loudness;
pub mod metadata;
pub mod mp3;
pub mod multipass;
pub mod multipass_graph;
pub mod phase;
pub mod pitch;
pub mod pitch_detect;
#[doc(hidden)]
pub mod portable;
#[cfg(target_os = "linux")]
pub mod linux_libraries;
pub mod settings;
pub mod statistics;
pub mod tags;
pub mod templates;
pub mod transient;
#[cfg(any(target_os = "linux", feature = "portable"))]
pub(crate) mod wav_preserve;
