//! Recovered pass decisions and render/recorder accounting for Platinum Notes
//! 10.5.0.1256. This module plans stages; it does not process audio. Instruction
//! anchors are recorded inline; `docs/VERIFICATION.md` describes the public
//! verification scope and remaining limits.

use crate::clip::ClipParameters;
use crate::loudness::GainAnalyzer;
use crate::math;
use crate::multipass::{self, DynamicsBand, ParameterWrite, ShippedPreset};
use crate::templates::{Band, Template};

#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" {
    #[link_name = "log2f"]
    fn native_log2f(value: f32) -> f32;
}
#[cfg(any(target_os = "linux", feature = "portable"))]
use crate::portable::math::log2f as native_log2f;

/// Stored analysis values, after the reference's float32 capture boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnalysisValues {
    pub input_level_db: f32,
    pub input_tuning_hz: f32,
}

impl AnalysisValues {
    /// Capture the primary meter's double and detector's float. The detector
    /// failure branch is `FCMP/B.PL` at 0x100017410: NaN is retained, not 440.
    pub fn from_measurements(input_level_db: f64, detector_tuning_hz: f32) -> Self {
        let input_tuning_hz = if detector_tuning_hz < 1.0 { 440.0 } else { detector_tuning_hz };
        Self { input_level_db: input_level_db as f32, input_tuning_hz }
    }
}

/// Requested pitch settings; parameter 0 must be read back before timing is
/// resolved. The original initializes the ratio and tempo factors to one when
/// the pitch node is absent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PitchPlan {
    pub correction_cents: i64,
    pub requested_ratio: f32,
    /// Mode initially written to parameter 1, converted from the option's low
    /// 32 bits as an unsigned integer, at 0x100026d9c.
    pub requested_mode: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PitchTiming {
    pub adjustment_readback: f32,
    /// The subsequent parameter-1 write made when replacing the original.
    pub forced_mode: Option<f32>,
    /// The reference stores an f32 factor promoted to f64 in processing info.
    pub tempo_factor: f64,
}

impl PitchPlan {
    /// Complete 0x100012e6c after a successful parameter-0 set/get. The
    /// replacement-mode setter is issued after the readback, just as native.
    pub fn resolve(&self, adjustment_readback: f32, replace_original: bool) -> PitchTiming {
        let tempo = if self.requested_mode == 1.0 && !replace_original {
            adjustment_readback
        } else {
            1.0
        };
        PitchTiming {
            adjustment_readback,
            forced_mode: replace_original.then_some(0.0),
            tempo_factor: f64::from(tempo),
        }
    }
}

/// Shipped preset and original ordered host writes, not snapin-level DSP state.
#[derive(Clone, Copy)]
pub struct WarmthPlan {
    pub mode: i32,
    pub preset: ShippedPreset,
    pub input_gain_db: f64,
    pub writes: [Option<ParameterWrite>; 2],
    pub wrapper_flags: [bool; 5],
}

#[derive(Clone, Copy)]
pub struct DynamicsPlan {
    pub preset: ShippedPreset,
    pub input_gain_db: f64,
    pub reciprocal_ratio: bool,
    pub correct_band_gain: bool,
    pub writes: [ParameterWrite; 41],
    pub wrapper_flags: [bool; 5],
}

#[derive(Clone, Copy)]
pub struct GainPlan {
    pub preset: ShippedPreset,
    pub gain_db: f32,
    pub writes: [ParameterWrite; 2],
}

/// Stages are connected in the field order: clip, pitch, warmth, expansion,
/// compression, gain, then the intermediate recorder and volume analyzer.
#[derive(Clone, Copy)]
pub struct HealingPlan {
    pub analysis: AnalysisValues,
    pub clip: Option<ClipParameters>,
    pub pitch: Option<PitchPlan>,
    pub warmth: Option<WarmthPlan>,
    pub expansion: Option<DynamicsPlan>,
    pub compression: Option<DynamicsPlan>,
    pub gain: Option<GainPlan>,
    pub target_level_db: f32,
    /// 0x100016c40 divides in f32 before passing the double to the analyzer.
    pub analyzer_release_seconds: f64,
}

fn correction_cents(input_hz: f32) -> i64 {
    let ratio = 440.0_f32 / input_hz;
    // The native helper calls libSystem log2f and uses FCVTAS X0,S0.
    let logarithm = unsafe { native_log2f(ratio) };
    (logarithm * 1200.0_f32).round() as i64
}

// FCMP/FCSEL MI selects the second operand on equality or unordered. FCMP/LS
// for the actual interval checks requires an ordered <= comparison.
fn sorted_volume_gate(level: f32, low: f32, high: f32) -> bool {
    let (minimum, maximum) = if low < high { (low, high) } else { (high, low) };
    minimum <= level && level <= maximum
}

fn bands(source: [Band; 4]) -> [DynamicsBand; 4] {
    source.map(|band| DynamicsBand {
        threshold_db: band.threshold,
        ratio: band.ratio,
        gain_db: band.gain,
    })
}

fn dynamics(source: [Band; 4], crossovers: [f32; 3], gain: f64, reciprocal: bool) -> DynamicsPlan {
    DynamicsPlan {
        preset: ShippedPreset::Dynamics,
        input_gain_db: gain,
        reciprocal_ratio: reciprocal,
        correct_band_gain: true,
        writes: multipass::dynamics_parameter_writes(bands(source), crossovers, reciprocal, true, gain, 0.0),
        wrapper_flags: [true, true, true, true, false],
    }
}

impl HealingPlan {
    /// The original evaluates every gate against the initial stored level,
    /// before applying clip headroom or any other stage (0x1000122b8..0x100012340).
    /// Clip bounds are used as stored; dynamics bounds are ordered with FCSEL.
    pub fn from_analysis(template: &Template, analysis: AnalysisValues) -> Self {
        let level = analysis.input_level_db;
        let clip_enabled = template.clip.enabled && template.clip.low <= level && level <= template.clip.high;
        let clip = clip_enabled.then_some(ClipParameters {
            headroom_db: template.clip.headroom,
            threshold_percent: template.clip.percentage,
        });
        let pitch = if template.pitch.enabled {
            let cents = correction_cents(analysis.input_tuning_hz);
            // CNEG is wrapping for i64::MIN, followed by signed comparison.
            if cents.wrapping_abs() >= template.pitch.threshold_cents {
                Some(PitchPlan {
                    correction_cents: cents,
                    requested_ratio: if analysis.input_tuning_hz > 1.0 {
                        440.0_f32 / analysis.input_tuning_hz
                    } else {
                        1.0
                    },
                    requested_mode: (template.pitch.option as u32) as f32,
                })
            } else {
                None
            }
        } else {
            None
        };
        let expansion_enabled = template.expansion.enabled
            && sorted_volume_gate(level, template.expansion.low, template.expansion.high);
        let compression_enabled = template.compression.enabled
            && sorted_volume_gate(level, template.compression.low, template.compression.high);

        let mut estimated = f64::from(level);
        if clip_enabled {
            estimated -= f64::from(template.clip.headroom);
        }
        let warmth = if template.exciter_option >= 0 {
            let input_gain_db = -15.0_f64 - estimated;
            estimated = -15.0;
            Some(WarmthPlan {
                mode: template.exciter_option,
                preset: if template.exciter_option < 2 { ShippedPreset::Warmth1 } else { ShippedPreset::Warmth2 },
                input_gain_db,
                writes: multipass::warmth_parameter_writes(input_gain_db, template.exciter_option),
                wrapper_flags: [true, true, true, false, false],
            })
        } else {
            None
        };
        let expansion = if expansion_enabled {
            let input_gain = -15.0_f64 - estimated;
            estimated = -15.0;
            Some(dynamics(template.expansion.bands, template.expansion.crossovers_hz, input_gain, true))
        } else {
            None
        };
        let compression = if compression_enabled {
            let input_gain = -15.0_f64 - estimated;
            estimated = -15.0;
            Some(dynamics(template.compression.bands, template.compression.crossovers_hz, input_gain, false))
        } else {
            None
        };
        let remaining = -15.0_f64 - estimated;
        // FABD/FCMP/B.LE skips insertion on equality and unordered NaN.
        let gain = if remaining.abs() > 0.01_f64 {
            let gain_db = remaining as f32;
            Some(GainPlan {
                preset: ShippedPreset::Gain,
                gain_db,
                writes: multipass::gain_stage_parameter_writes(gain_db),
            })
        } else {
            None
        };
        Self {
            analysis,
            clip,
            pitch,
            warmth,
            expansion,
            compression,
            gain,
            target_level_db: template.output.limiter_threshold,
            analyzer_release_seconds: f64::from(template.output.limiter_release / 1000.0_f32),
        }
    }
}

/// Final limiter controller values computed from the retained healing bank.
/// `writes` are normalized AudioUnit parameters 1..4, not physical controls.
#[derive(Clone, Copy)]
pub struct FinalPlan {
    pub healed_level_db: f32,
    pub target_level_db: f32,
    pub release_ms: f32,
    pub effective_headroom_db: f64,
    pub analyzer_target_db: f64,
    pub analyzer_gain_db: f64,
    pub writes: [ParameterWrite; 4],
}

// Native FMAX(x,+0), then FMINNM(result,1) at 0x10001154c.
fn controller_clamp(value: f64) -> f32 {
    if value.is_nan() { 1.0 }
    else if value <= 0.0 { 0.0 }
    else if value >= 1.0 { 1.0 }
    else { value as f32 }
}

fn min_propagating_zero(value: f64) -> f64 {
    if value.is_nan() || value < 0.0 || (value == 0.0 && value.is_sign_negative()) {
        value
    } else {
        0.0
    }
}

impl FinalPlan {
    /// The analyzer must still contain the healing pass with simulations enabled.
    /// It is queried before the reference resets/disables that bank.
    pub fn from_healed(template: &Template, analyzer: &GainAnalyzer) -> Self {
        let target = template.output.limiter_threshold;
        let mut headroom = f64::from(template.output.margin);
        if target > -12.5_f32 {
            let scale = ((f64::from(target) + 12.5_f64) / -1.5_f64) + 1.0_f64;
            headroom = min_propagating_zero(scale * headroom);
        }
        let analyzer_target = f64::from(target) - headroom;
        let gain = analyzer.gain_for_target(analyzer_target);
        let mut value = (headroom + gain) as f32;
        let mut ceiling = -(headroom as f32).abs();
        let mut extra = 0.0_f64;
        if value > 20.0_f32 {
            let excess = value + -20.0_f32;
            extra = f64::from(excess) + 0.0_f64;
            ceiling -= excess;
            value = excess + value;
        }
        if value < -20.0_f32 {
            let excess = value + 20.0_f32;
            extra += f64::from(excess);
            ceiling -= excess;
            value += excess;
        }
        let parameter_1 = f64::from((value + 20.0_f32) / 40.0_f32);
        let parameter_2 = (extra + 20.0_f64) / 40.0_f64;
        let parameter_3 = f64::from((ceiling + 20.0_f32) / 26.0_f32);
        let parameter_4 = math::ln(f64::from(template.output.limiter_release)) / 6.214608098422191_f64;
        Self {
            healed_level_db: analyzer.integrated_loudness() as f32,
            target_level_db: target,
            release_ms: template.output.limiter_release,
            effective_headroom_db: headroom,
            analyzer_target_db: analyzer_target,
            analyzer_gain_db: gain,
            writes: [
                ParameterWrite { id: 1, value: controller_clamp(parameter_1) },
                ParameterWrite { id: 2, value: controller_clamp(parameter_2) },
                ParameterWrite { id: 3, value: controller_clamp(parameter_3) },
                ParameterWrite { id: 4, value: controller_clamp(parameter_4) },
            ],
        }
    }
}

/// Initial processing rate selected at 0x100016e48..0x100016ffc. The non-MP3
/// `B.HI` also retains unordered NaN; later graph setup may reject that rate.
pub fn processing_sample_rate(input_rate: f64, input_is_mp3: bool) -> f64 {
    if input_is_mp3 {
        if [22050.0, 32000.0, 44100.0, 48000.0].contains(&input_rate) { input_rate } else { 44100.0 }
    } else if input_rate > 22050.0 || input_rate.is_nan() {
        input_rate
    } else {
        44100.0
    }
}

/// Final graph sample-rate selection at 0x100015fb0. The native second branch
/// compares the output code to the fourcc `aac `, not recorder code 6.
pub fn final_sample_rate(processing_rate: f64, output_format_code: i64) -> f64 {
    let restricted = output_format_code == 1 || output_format_code == 0x6161_6320;
    if restricted && ![22050.0, 32000.0, 44100.0, 48000.0].contains(&processing_rate) {
        44100.0
    } else {
        processing_rate
    }
}

/// Sum successful latency properties in graph order and round only once. A
/// failed last-node/output-rate query returns zero. Individual failed node
/// properties contribute +0. The function returns the low i32 bits of FCVTAS X0.
pub fn graph_latency_frames(output_rate: Option<f64>, node_latencies: &[Option<f64>]) -> i32 {
    let Some(rate) = output_rate else { return 0 };
    let mut seconds = 0.0_f64;
    for latency in node_latencies {
        seconds += latency.unwrap_or(0.0);
    }
    (seconds * rate).round() as i64 as i32
}

/// Extra facts used only when the chosen output recorder is MP3.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mp3CountInputs {
    pub input_is_mp3: bool,
    pub input_sample_rate: f64,
    pub output_sample_rate: f64,
    pub input_has_xing_header: bool,
    pub input_has_info_header: bool,
    pub decode_using_vbr_header: bool,
    pub input_lame_info_latency: i32,
    pub input_decoder_delay: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalFramePlan {
    pub adjusted_input_frames: i64,
    pub output_has_vbr_header: bool,
    /// The MP3 header/decoder adjustment, before graph latency subtraction.
    pub header_shift_frames: i64,
    pub recorder_shift_frames: i64,
    pub recorder_target_frames: i64,
}

/// Final recorder accounting from 0x100016360..0x10001681c. `input_frames`
/// is the actual count read by initial analysis, not a container's advertised
/// duration or the healed file's length. MP3 adjustments are in input-frame
/// coordinates as observed; no unobserved rate-conversion factor is added.
pub fn final_frame_plan(input_frames: i64, tempo_factor: f64, graph_latency: i32, mp3: Option<Mp3CountInputs>) -> FinalFramePlan {
    let mut adjusted = input_frames;
    let mut header_shift = 0_i64;
    let mut output_has_vbr_header = false;
    if let Some(mp3) = mp3 {
        let input_rate = mp3.input_sample_rate as i32;
        let output_rate = match mp3.output_sample_rate as i32 { n if n > 0 => n, _ => 44100 };
        let has_header = mp3.input_has_xing_header || mp3.input_has_info_header;
        let mut trim = 0_i32;
        if mp3.input_is_mp3 {
            output_has_vbr_header = has_header;
            if !(has_header && mp3.decode_using_vbr_header) {
                trim = ((f64::from(input_rate) * 576.0_f64) / f64::from(output_rate)).round() as i32;
            }
            if has_header {
                header_shift = i64::from(mp3.input_lame_info_latency) - 576;
            }
        } else {
            output_has_vbr_header = true;
        }
        let mut delay_limit = if output_has_vbr_header { 1728_i32 } else { 576_i32 };
        if input_rate > 0 {
            delay_limit = ((f64::from(input_rate) * f64::from(delay_limit)) / f64::from(output_rate)) as i32;
        }
        if delay_limit > 0 {
            let delay = (f64::from(mp3.input_decoder_delay) / tempo_factor).round() as i32;
            if delay > 0 {
                header_shift = header_shift.wrapping_sub(i64::from(delay.min(delay_limit)));
            }
        }
        adjusted = input_frames.wrapping_sub(i64::from(trim.min(576))).max(1);
    }
    let graph_shift = i64::from(graph_latency.wrapping_neg());
    let target = ((adjusted as f64) / tempo_factor).round() as i64;
    FinalFramePlan {
        adjusted_input_frames: adjusted,
        output_has_vbr_header,
        header_shift_frames: header_shift,
        recorder_shift_frames: header_shift.wrapping_add(graph_shift),
        recorder_target_frames: header_shift.wrapping_add(target),
    }
}

pub const RENDER_BLOCK_FRAMES: u32 = 512;
pub const HEAL_PADDING_FRAMES: i64 = 1024;
pub const RENDER_POST: u32 = 0x08;
pub const RENDER_SILENCE: u32 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPass { Analysis, Healing, Final }

/// The two dispatches in volume callback 0x10001c460. Multi-buffer audio uses
/// only the first two planes and the analyzer's distinct scalar stereo path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeterFeed {
    Interleaved { channels: u32, frames: u32 },
    StereoPlanar { frames: u32 },
}

/// Determine the volume callback's dispatch from the first buffer descriptor.
/// It checks POST, not the silence bit. The caller remains responsible for
/// validating actual buffer extents and a second plane before reading them.
pub fn meter_feed(
    action_flags: u32,
    buffer_count: u32,
    first_buffer_present: bool,
    first_buffer_channels: u32,
    first_buffer_bytes: u32,
) -> Option<MeterFeed> {
    if action_flags & RENDER_POST == 0 || !first_buffer_present || buffer_count == 0 {
        return None;
    }
    let samples = first_buffer_bytes >> 2;
    if buffer_count == 1 {
        let frames = if (first_buffer_channels as i32) > 0 {
            samples / first_buffer_channels
        } else {
            samples
        };
        Some(MeterFeed::Interleaved { channels: first_buffer_channels, frames })
    } else if (buffer_count as i32) >= 2 && first_buffer_bytes >= 4 {
        Some(MeterFeed::StereoPlanar { frames: samples.max(1) })
    } else {
        None
    }
}

/// 0x10001c514 checks POST, a non-null first buffer, and nonzero first-buffer
/// byte size; unlike the meter it passes the render callback's frame argument.
/// Its channel-plane count is the wrapping product of first-buffer channels
/// and buffer count. Layout validity is not established by these predicates.
pub fn detector_feed(
    action_flags: u32,
    buffer_count: u32,
    first_buffer_present: bool,
    first_buffer_channels: u32,
    first_buffer_bytes: u32,
    render_frames: u32,
) -> Option<(u32, u32)> {
    if action_flags & RENDER_POST == 0 || !first_buffer_present || first_buffer_bytes == 0 {
        None
    } else {
        Some((first_buffer_channels.wrapping_mul(buffer_count), render_frames))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRead {
    pub real_frames: u32,
    pub zero_filled_frames: u32,
    /// Set by the input callback only on a zero-frame read satisfying its
    /// recorder/processed-count termination check.
    pub signal_silence: bool,
    pub done: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError { ReadExceedsRequest }

/// A call to the recorder's internal writer, expressed in rendered-frame
/// coordinates. `offset` or `frames` can exceed the buffer for pathological
/// native integer state; callers must not dereference such an invalid span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordSpan { pub offset: u64, pub frames: u32 }

/// State of 0x100018518. A positive initial shift is intentionally not
/// converted into prepended silence: the observed native branch emits nothing
/// and leaves the positive shift unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecorderAccounting {
    pub target_frames: i64,
    current_frames: i64,
    shift_remaining: i64,
}

impl RecorderAccounting {
    pub fn new(shift_frames: i64, target_frames: i64) -> Self {
        Self { target_frames, current_frames: 0, shift_remaining: shift_frames }
    }
    /// Intermediate recorder setup at 0x100012c44..0x100012c54: negate the
    /// i32 graph latency before sign-extending to the recorder's i64 shift.
    pub fn for_healing(graph_latency: i32) -> Self {
        Self::new(i64::from(graph_latency.wrapping_neg()), 0)
    }
    /// Apply the final path's header-adjusted shift and target.
    pub fn for_final(plan: &FinalFramePlan) -> Self {
        Self::new(plan.recorder_shift_frames, plan.recorder_target_frames)
    }
    pub fn current_frames(&self) -> i64 { self.current_frames }
    pub fn shift_remaining(&self) -> i64 { self.shift_remaining }

    /// Mirrors the post-render callback's `(flags & 0x18) == 8` gate. Supplying
    /// no buffers corresponds to the recorder's immediate return.
    pub fn record_render(&mut self, action_flags: u32, has_buffers: bool, frames: u32) -> Option<RecordSpan> {
        if action_flags & (RENDER_POST | RENDER_SILENCE) != RENDER_POST || !has_buffers {
            return None;
        }
        if self.shift_remaining > 0 {
            return None;
        }
        let mut offset = 0_u64;
        let mut count = frames;
        let crossing = self.shift_remaining < 0;
        if crossing {
            let discard = self.shift_remaining.wrapping_neg();
            if i64::from(frames) <= discard {
                self.shift_remaining = self.shift_remaining.wrapping_add(i64::from(frames));
                return None;
            }
            offset = discard as u64;
            count = frames.wrapping_add(self.shift_remaining as u32);
            self.shift_remaining = 0;
        }
        if self.target_frames > 0 && self.current_frames.wrapping_add(i64::from(count)) > self.target_frames {
            if !crossing && self.current_frames >= self.target_frames {
                return None;
            }
            count = self.target_frames.wrapping_sub(self.current_frames) as u32;
        }
        self.current_frames = self.current_frames.wrapping_add(i64::from(count));
        Some(RecordSpan { offset, frames: count })
    }
}

/// Source pulls and graph renders are deliberately separate: a pitch unit may
/// pull ahead, and silence inside the source is not EOF. Construct a fresh
/// instance for each openInputFile call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameAccounting {
    pub pass: RenderPass,
    pub recorder: Option<RecorderAccounting>,
    pub padding_frames: i64,
    pub actual_input_frames: i64,
    pub requested_input_frames: i64,
    pub rendered_frames: i64,
    pub render_sample_time: f64,
    pub done: bool,
}

impl FrameAccounting {
    /// Use for a fresh enhancer, or after a final pass set padding to zero.
    pub fn new(pass: RenderPass, recorder: Option<RecorderAccounting>) -> Self {
        Self::with_prior_padding(pass, recorder, 0)
    }

    /// Opening an input clears counters and the recorder, but not padding.
    /// The analysis pass inherits it; healing sets 1024 and final sets zero.
    pub fn with_prior_padding(pass: RenderPass, recorder: Option<RecorderAccounting>, prior_padding: i64) -> Self {
        let padding_frames = match pass {
            RenderPass::Analysis => prior_padding,
            RenderPass::Healing => HEAL_PADDING_FRAMES,
            RenderPass::Final => 0,
        };
        Self {
            pass,
            recorder,
            padding_frames,
            actual_input_frames: 0,
            requested_input_frames: 0,
            rendered_frames: 0,
            render_sample_time: 0.0,
            done: false,
        }
    }

    /// Apply a successful MIKAudioFileRead result. The caller zeroes the whole
    /// requested buffer before the read, including the unused part of a short
    /// read. EOF termination is tested only when `read_frames == 0`, before
    /// adding this request to the processed counter (0x10001c184).
    pub fn source_read(&mut self, requested: u32, read_frames: u32) -> Result<SourceRead, FrameError> {
        if read_frames > requested { return Err(FrameError::ReadExceedsRequest); }
        self.actual_input_frames = self.actual_input_frames.wrapping_add(i64::from(read_frames));
        let mut silence = false;
        if read_frames == 0 {
            let threshold = self.actual_input_frames.wrapping_add(self.padding_frames);
            let finished = match self.recorder {
                None => self.requested_input_frames >= threshold,
                Some(recorder) if recorder.target_frames <= 0 => recorder.current_frames >= threshold,
                Some(recorder) => recorder.current_frames >= recorder.target_frames,
            };
            if finished {
                self.done = true;
                silence = true;
            }
        }
        self.requested_input_frames = self.requested_input_frames.wrapping_add(i64::from(requested));
        Ok(SourceRead {
            real_frames: read_frames,
            zero_filled_frames: requested - read_frames,
            signal_silence: silence,
            done: self.done,
        })
    }

    /// Called after a successful graph render and its recorder callback. Analysis
    /// requires both done and the render's output-silence flag; healing/final
    /// stop on done alone. Cancellation stops any pass after the current render.
    pub fn finish_render(&mut self, frames: u32, output_action_flags: u32, cancelled: bool) -> bool {
        self.rendered_frames = self.rendered_frames.wrapping_add(i64::from(frames));
        self.render_sample_time += f64::from(frames);
        cancelled || (self.done && (self.pass != RenderPass::Analysis || output_action_flags & RENDER_SILENCE != 0))
    }
}
