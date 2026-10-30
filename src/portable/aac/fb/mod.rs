//! fb: input path, filterbank, block switching, TNS and bandwidth of the pure-Rust port of Apple's AAC-LC encoder.
#![forbid(unsafe_code)]
#![allow(clippy::all)]

pub mod bandwidth;
pub mod blocksw;
pub mod frame;
pub mod gen_tables;
pub mod grouping;
pub mod input;
pub mod mdct;
pub mod tables;
pub mod tns;

pub use frame::{ChannelFb, FbConfig, FbState};
