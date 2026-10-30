//! ms: perceptual entropy kernels and the M/S stereo decision of the pure-Rust port of Apple's AudioToolbox
//! AAC-LC encoder (engine configuration).
//!
//! * [`pe`]: `_EvalPercepEntropyByLine` (fast + HQ), `calcChannelPE` (S0; `FRECPE`/`FRECPS` are `crate::portable::arm::{frecpe_f32, frecps_f32}`);
//! * [`mdct`]: `_getMsMDCT` (S1); [`candidate`]: the sequence that builds psyOut#2 from psyOut#1 with the `psy` module (S1);
//! * [`thresholds`]: correlation, `ldiff`, `_getMsThresholdsPrecise`, `_addLRThresholdOffsets`, `_modifyLRThresholds` (S2);
//! * [`stereo`]: `AdvanceMsStereo`: thresholds, PE(M/S), decision, global rule, copy-back, flag word (S2/S3);
//! * the CPE short-window grouping is [`crate::portable::aac::fb::grouping`];
//! * [`mask`]: `ms_mask_present`, `ms_used`, side-info estimate, intensity interplay (S5);
//! * [`tables`]: Apple's constant tables (generated).
#![forbid(unsafe_code)]
#![allow(clippy::all)]

pub mod candidate;
pub mod mask;
pub mod mdct;
pub mod pe;
pub mod stereo;
pub mod tables;
pub mod thresholds;

pub use pe::{calc_channel_pe, eval_percep_entropy_by_line, MsError};
pub use candidate::{build_ms_candidate, MsCandidateScratch};
pub use stereo::{MsConfig, MsTool};
