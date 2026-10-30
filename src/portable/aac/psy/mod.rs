//! psy: the psychoacoustic model of the pure-Rust port of Apple's AudioToolbox AAC-LC encoder (engine configuration).
//!
//! * [`param`]: `PamParam` (`P`): construction from the codec configuration and the per-frame `_adaptPamParam(B)` refresh;
//! * [`pam`]: `_pamMonaural` with its per-channel state `ThrChannel`, and the table-driven `dB()` / `2^x` approximations the later stages share;
//! * [`glue`]: the EncodeFrame glue (power spectra, threshold copy/weighting, `_AdvanceCodChannelPam`, `_AdvancePsyOut`,
//!   `_sortShortSpectrum`, `_calculateMaxSfb`) and the per-channel band configuration (`_CreatePsyParam`, ...);
//! * [`vdsp`]: the Accelerate primitives the model relies on (exact summation tree of `vDSP_sve`, `zvmags` head/body/tail rule, ...), also used by `int`;
//! * [`tables`]: Apple's constant tables (generated from the extracted binaries), also the `stic::logMantissa` / `stic::pow2table` tables of `ms`, `int`, `quant`.
#![forbid(unsafe_code)]
#![allow(clippy::all)]

pub mod glue;
pub mod pam;
pub mod param;
pub mod tables;
pub mod vdsp;

pub use glue::*;
pub use pam::{db, pow2_est, ThrChannel};
pub use param::{PamConfig, PamParam};
