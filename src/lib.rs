#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("The current bit-parity reference target is macOS arm64; other targets are not yet validated.");

mod math;
pub mod apple_codec;
pub mod audio_input;
pub mod clip;
pub mod controller;
pub mod distortion;
pub mod dynamics;
pub mod engine;
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
pub mod settings;
pub mod statistics;
pub mod tags;
pub mod templates;
pub mod transient;
