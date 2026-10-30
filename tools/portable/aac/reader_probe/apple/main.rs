// Drives the engine's own `apple_codec::Reader` (macOS only); see ../common.rs.
#![allow(dead_code, warnings)]
#[path = "../../../../../src/apple_codec.rs"]
mod apple_codec;
use apple_codec::{CodecError, Reader};
include!("../common.rs");
