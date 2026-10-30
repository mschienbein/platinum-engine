//! int: intensity stereo of Apple's AAC-LC encoder (`_aacEncodeFrame` phases A4/A7). Pure `f32` Rust, bit-exact against the AudioToolbox encoder (macOS 27.2).
#![forbid(unsafe_code)]
#![allow(clippy::all)]
pub mod analysis;
pub mod config;
pub mod decision;
pub mod input;
pub mod state;
pub mod tables;
mod tables_data;

pub use analysis::tns_ranges;
pub use config::{intensity_enabled, IntensityConfig};
pub use input::ChannelLines;
pub use state::IntensityChannel;
