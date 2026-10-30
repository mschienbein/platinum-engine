//! quant: quantiser, scalefactor estimation, loop kernel, bit counters and rate state of the AAC-LC encoder port
//! (bit-exact port of Apple's AudioCodecs encoder for the engine configuration).
#![forbid(unsafe_code)]
#![allow(clippy::all)]

pub mod bitcount;
pub mod channel;
pub mod config;
pub mod fp;
pub mod frame;
pub mod kernel;
pub mod output;
pub mod prim;
pub mod rate;
pub mod tables_gen;

pub use config::QuantConfig;
pub use frame::Quantizer;
pub use kernel::{KernelConfig, PamParams};
pub use rate::{ElementSide, RateConfig};
