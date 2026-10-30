//! Output stage of Apple's decoder: `InterleaveConvert(src**, dst**, nch, frames, float?, nonInterleaved?, scale)` (0x2cd2f0).
//!
//! The synthesis planes are in the int16-scaled domain.  The float32 client multiplies by `scale = 2^-15` (`vDSP_vsmul`; an exact power of two) while
//! interleaving; mono and non-interleaved (planar) clients scale per plane (no dither, no clipping anywhere in the float path).
//!
//! The client-format mixer (`DownMixWithCoefficients`, in `decoder::file::mixer`) runs on the planes *before* this stage when the client has fewer
//! channels than the stream; as the scale is a power of two, mixing before or after it is bit-identical for non-subnormal data.

/// `FrameOutRecord+0x60` of the float32 client: 2^-15 (0x38000000).
pub const OUTPUT_SCALE: f32 = 1.0 / 32768.0;
