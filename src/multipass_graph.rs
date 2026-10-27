//! Fixed shipped Multipass graphs. Reference addresses below are offsets in the
//! pinned HeartCore arm64 image, never calls into that image. The caller owns
//! event-bounded host subdivision, FPCR.FZ, wrapper warmup and wrapper phase.

use crate::distortion::{DistortionContext, DistortionProcessor, DistortionRouting, FloatDomain, SaturateParameters};
use crate::dynamics::{controller_lookahead_argument, Dynamics, DynamicsField, DynamicsParameters};
use crate::limiter::DryDelay;
use crate::multipass::{
    active_bands, automation_slot_for_host_parameter, channels_are_silent, lane_gain_from_normalized,
    lane_gain_to_normalized, native_linear_modulation, native_macro_source, BandRouteMixer,
    CrossoverFrequencyScale, LaneDryWetMixer, LaneModelField, LanePanGain, LaneStereoDelay,
    MultipassBandSplitter, NativeSleepGate, RootDryAllpass, RootStereoGain, ShippedPreset,
    ShippedDistortionParameters, ShippedDynamicsParameters, ShippedEffectParameters,
    ShippedTransientParameters, SleepDispatch, TransientModelField,
};
use crate::transient::{TransientParameters, TransientShaper};

const MAX_FRAMES: usize = 64;
const QUIET_RATE: f32 = f32::from_bits(0x3580_0000);

/// Observed slot-9 input bytes (`+0x10` silence and `+0x40` bypass), plus the
/// host event-bounded segment length still remaining before this DSP chunk.
/// `segment_remaining` includes this chunk and must be supplied for every
/// call; it determines the native parameter-event ramp duration. `silent` is
/// updated by processing and is not permission to substitute input samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphContext {
    pub silent: bool,
    pub bypass: bool,
    pub segment_remaining: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphError {
    InvalidSampleRate,
    InvalidBlock,
    InvalidParameter(u32),
    EffectError,
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSampleRate => f.write_str("invalid Multipass sample rate"),
            Self::InvalidBlock => f.write_str("invalid stereo chunk or event-segment remainder"),
            Self::InvalidParameter(id) => write!(f, "unsupported Multipass host parameter {id}"),
            Self::EffectError => f.write_str("Multipass nested DSP failed"),
        }
    }
}
impl std::error::Error for GraphError {}

#[derive(Clone, Copy)]
struct Write { id: u32, value: f32 }

#[derive(Clone, Copy)]
struct AutomationRamp {
    current: f32,
    target: f32,
    remaining: usize,
}
impl AutomationRamp {
    fn new(current: f32) -> Self {
        Self { current, target: current, remaining: 0 }
    }

    fn schedule(&mut self, target: f32, segment_remaining: usize) {
        self.target = target;
        self.remaining = if self.current != target { segment_remaining } else { 0 };
    }

    /// `0x623db0..0x623dec`: separate f32 subtract, multiply, divide and
    /// add, with the remaining duration decremented only after this update.
    fn advance(&mut self, frames: usize) -> Option<f32> {
        if self.remaining == 0 { return None; }
        if self.remaining <= frames {
            self.current = self.target;
            self.remaining = 0;
        } else {
            let distance = self.target - self.current;
            let scaled = distance * frames as f32;
            self.current += scaled / self.remaining as f32;
            self.remaining -= frames;
        }
        Some(self.current)
    }
}

#[derive(Clone, Copy)]
struct LaneParameters {
    enabled: bool,
    gain: f32,
    pan: f32,
    mix: f32,
    post: f32,
    mute: bool,
    solo: bool,
}


fn dynamics_defaults() -> DynamicsParameters {
    // Model-constructor registration, before nested preset events; see
    // c24aec/c24f38 and au-state-oracle's after-preset-before-render record.
    DynamicsParameters {
        input_gain_db: 0.0, output_gain_db: 0.0, low_threshold_db: -30.0,
        low_ratio: 1.0, high_threshold_db: f32::from_bits(0xc19f_ffff),
        high_ratio: 1.0, attack: 1.0, release: 1.0, knee: 2.5, mix: 1.0,
    }
}

fn dynamics_parameters(raw: &ShippedDynamicsParameters) -> DynamicsParameters {
    let cv = |field: DynamicsField, value| field.model_event_value(value, 0.0);
    DynamicsParameters {
        input_gain_db: cv(DynamicsField::InputGain, raw.input_gain_db),
        output_gain_db: cv(DynamicsField::OutputGain, raw.output_gain_db),
        low_threshold_db: cv(DynamicsField::LowThreshold, raw.low_threshold_db),
        low_ratio: cv(DynamicsField::LowRatio, raw.low_ratio),
        high_threshold_db: cv(DynamicsField::HighThreshold, raw.high_threshold_db),
        high_ratio: cv(DynamicsField::HighRatio, raw.high_ratio),
        release: cv(DynamicsField::Release, raw.release),
        mix: cv(DynamicsField::Mix, raw.mix),
        attack: cv(DynamicsField::Attack, raw.attack),
        knee: cv(DynamicsField::Knee, raw.knee),
    }
}

fn transient_parameters(raw: &ShippedTransientParameters) -> TransientParameters {
    let cv = |field: TransientModelField, value| field.model_event_value(value, 0.0);
    TransientParameters {
        attack: cv(TransientModelField::Attack, raw.attack),
        pump: cv(TransientModelField::Pump, raw.pump),
        sustain: cv(TransientModelField::Sustain, raw.sustain),
        speed: cv(TransientModelField::Speed, raw.speed),
        clip: raw.clip,
    }
}

fn saturate_parameters(raw: &ShippedDistortionParameters) -> SaturateParameters {
    let cv = |domain: FloatDomain, value| domain.denormalize(domain.normalize(value));
    SaturateParameters {
        drive: cv(FloatDomain::Drive, raw.drive),
        bias: cv(FloatDomain::Bias, raw.bias),
        spread: cv(FloatDomain::Spread, raw.spread),
        dynamics: cv(FloatDomain::Dynamics, raw.dynamics),
        mix: cv(FloatDomain::Mix, raw.mix),
        remove_dc: raw.remove_dc,
    }
}

struct EffectGate {
    dry: [DryDelay; 2],
    sleep: NativeSleepGate,
    latency: i32,
    enabled: bool,
    previous_active: bool,
    synchronize_active: bool,
}
impl EffectGate {
    fn new(latency: i32, enabled: bool) -> Self {
        Self {
            dry: [DryDelay::new(), DryDelay::new()], sleep: NativeSleepGate::new(),
            latency, enabled, previous_active: true, synchronize_active: true,
        }
    }
    fn reset(&mut self) {
        self.dry[0].reset_counters(); self.dry[1].reset_counters();
        self.sleep.reset(); self.previous_active = true; self.synchronize_active = true;
    }
    fn begin(&mut self, bypass: bool, left: &mut [f32], right: &mut [f32]) -> (bool, bool, bool) {
        let frames = self.latency.max(0).min(10_112) as usize;
        self.dry[0].configure(frames); self.dry[1].configure(frames);
        let active = self.enabled && !bypass;
        if self.synchronize_active {
            self.synchronize_active = false;
            self.previous_active = active;
        }
        if !active && !self.previous_active {
            self.dry[0].process(left); self.dry[1].process(right);
            return (false, false, false);
        }
        let reset_before = active && !self.previous_active;
        if reset_before { self.reset(); }
        self.dry[0].remember(left); self.dry[1].remember(right);
        (true, active, reset_before)
    }
    fn finish(&mut self, active: bool, dispatched: bool, silent: &mut bool, left: &[f32], right: &[f32]) {
        if dispatched && !active { self.reset(); }
        self.previous_active = active;
        if *silent { *silent = channels_are_silent(left, right); }
        if !*silent {
            // Slot 9 resets its quiet counter without changing the sleeping bit.
            // A later non-silent dispatcher call performs its wake hook.
            self.sleep.clear_quiet_frames();
        } else if !active {
            self.sleep.mark_sleeping();
        }
    }
}

struct DynamicsEffect {
    parameters: DynamicsParameters,
    kernel: Dynamics,
    gate: EffectGate,
    initial_reset_pending: bool,
}
impl DynamicsEffect {
    fn new(rate: f32, raw: &ShippedDynamicsParameters) -> Result<Self, GraphError> {
        let latency = controller_lookahead_argument(rate);
        let kernel = Dynamics::new(rate, latency, dynamics_defaults()).map_err(|_| GraphError::EffectError)?;
        Ok(Self { parameters: dynamics_parameters(raw), kernel, gate: EffectGate::new(latency, raw.enabled), initial_reset_pending: true })
    }
    fn reset(&mut self) { self.gate.reset(); self.kernel.reset(self.parameters); }
    fn process(&mut self, silent: &mut bool, bypass: bool, left: &mut [f32], right: &mut [f32]) -> Result<(), GraphError> {
        // Nested master binding calls slot 7/17 with the model state after
        // the first eligible host-ramp update, before its first DSP chunk.
        if self.initial_reset_pending {
            self.kernel.reset(self.parameters);
            self.initial_reset_pending = false;
        }
        let (dispatch, active, reset_before) = self.gate.begin(bypass, left, right);
        if dispatch {
            if reset_before { self.kernel.reset(self.parameters); }
            match self.gate.sleep.select(*silent, self.gate.latency, left.len()) {
                SleepDispatch::Active { .. } => self.kernel.process_block(self.parameters, left, right).map_err(|_| GraphError::EffectError)?,
                SleepDispatch::Sleeping { .. } => (), // Dynamics slot 15 is empty (c22100).
            }
            if !active { self.kernel.reset(self.parameters); }
        }
        self.gate.finish(active, dispatch, silent, left, right);
        Ok(())
    }
    fn set_normalized(&mut self, field: DynamicsField, normalized: f32) {
        let v = field.model_physical_from_normalized(normalized.max(0.0).min(1.0));
        match field {
            DynamicsField::InputGain => self.parameters.input_gain_db = v,
            DynamicsField::OutputGain => self.parameters.output_gain_db = v,
            DynamicsField::LowThreshold => self.parameters.low_threshold_db = v,
            DynamicsField::LowRatio => self.parameters.low_ratio = v,
            DynamicsField::HighThreshold => self.parameters.high_threshold_db = v,
            DynamicsField::HighRatio => self.parameters.high_ratio = v,
            DynamicsField::Attack => self.parameters.attack = v,
            DynamicsField::Release => self.parameters.release = v,
            DynamicsField::Knee => self.parameters.knee = v,
            DynamicsField::Mix => self.parameters.mix = v,
        }
    }
}

struct TransientEffect {
    kernel: TransientShaper,
    gate: EffectGate,
}
impl TransientEffect {
    fn new(rate: f32, raw: &ShippedTransientParameters) -> Self {
        Self { kernel: TransientShaper::new(rate, transient_parameters(raw)), gate: EffectGate::new(0, raw.enabled) }
    }
    fn reset(&mut self) { self.gate.reset(); self.kernel.reset(); }
    fn process(&mut self, silent: &mut bool, bypass: bool, left: &mut [f32], right: &mut [f32]) {
        let (dispatch, active, reset_before) = self.gate.begin(bypass, left, right);
        if dispatch {
            if reset_before { self.kernel.reset(); }
            match self.gate.sleep.select(*silent, self.gate.latency, left.len()) {
                SleepDispatch::Active { .. } => { self.kernel.process_chunk(left, right, None); }
                SleepDispatch::Sleeping { .. } => { self.kernel.advance_sleeping_chunk(left.len() as u32); }
            }
            if !active { self.kernel.reset(); }
        }
        self.gate.finish(active, dispatch, silent, left, right);
    }
}

enum Effect {
    Dynamics(DynamicsEffect),
    Distortion { kernel: DistortionProcessor, enabled: bool },
    Transient(TransientEffect),
}
impl Effect {
    fn latency(&self) -> i32 {
        match self { Self::Dynamics(d) => d.gate.latency, Self::Distortion { .. } => 3, Self::Transient(_) => 0 }
    }
    fn reset(&mut self) {
        match self { Self::Dynamics(d) => d.reset(), Self::Distortion { kernel, .. } => kernel.reset(), Self::Transient(t) => t.reset() }
    }
    fn is_sleeping(&self) -> bool {
        match self {
            Self::Dynamics(d) => d.gate.sleep.is_sleeping(),
            Self::Distortion { kernel, .. } => kernel.is_sleeping(),
            Self::Transient(t) => t.gate.sleep.is_sleeping(),
        }
    }
    fn process(&mut self, silent: &mut bool, left: &mut [f32], right: &mut [f32]) -> Result<(), GraphError> {
        match self {
            Self::Dynamics(d) => d.process(silent, false, left, right),
            Self::Distortion { kernel, enabled } => {
                let mut context = DistortionContext { silent: *silent, bypass: false };
                kernel.process_chunk(DistortionRouting { enabled: i32::from(*enabled), host_enabled: 1, host_bypass: 0, dry_latency_frames: 3 }, &mut context, left, right).map_err(|_| GraphError::EffectError)?;
                *silent = context.silent; Ok(())
            }
            Self::Transient(t) => { t.process(silent, false, left, right); Ok(()) }
        }
    }
}

struct Lane {
    model: LaneParameters,
    effects: Vec<Effect>,
    first_delay: LaneStereoDelay,
    compensation_delay: LaneStereoDelay,
    mix: LaneDryWetMixer,
    gain: LanePanGain,
    wet: [[f32; MAX_FRAMES]; 2],
    active: bool,
    all_children_silent: bool,
}
impl Lane {
    fn list_latency(&self) -> i32 { self.effects.iter().map(Effect::latency).sum() }
    fn reset(&mut self) {
        if self.active {
            self.active = false;
            self.all_children_silent = false;
            self.gain.mark_initial();
            self.first_delay.reset_counters(); self.compensation_delay.reset_counters();
            for effect in &mut self.effects { effect.reset(); }
        }
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32], silent: &mut bool, compensation: i32, any_solo: bool) -> Result<(), GraphError> {
        let n = left.len();
        self.first_delay.configure(self.list_latency());
        self.compensation_delay.configure(compensation);
        if !self.model.enabled {
            self.first_delay.process(left, right, silent);
            self.compensation_delay.process(left, right, silent);
            return Ok(());
        }
        self.active = true;
        self.wet[0][..n].copy_from_slice(left); self.wet[1][..n].copy_from_slice(right);
        self.first_delay.process(left, right, silent);
        let mut wet_silent = channels_are_silent(&self.wet[0][..n], &self.wet[1][..n]);
        let [wet_left, wet_right] = &mut self.wet;
        for effect in &mut self.effects {
            effect.process(&mut wet_silent, &mut wet_left[..n], &mut wet_right[..n])?;
            *silent &= wet_silent;
        }
        self.all_children_silent = self.effects.iter().all(Effect::is_sleeping);
        self.mix.process(self.model.mix, [&mut *left, &mut *right], [&wet_left[..n], &wet_right[..n]]);
        self.compensation_delay.process(left, right, silent);
        self.gain.process(self.model.gain, self.model.pan, self.model.mute, self.model.solo, any_solo, [left, right]);
        Ok(())
    }
    fn keeps_graph_awake(&self) -> bool {
        !self.all_children_silent
            || self.first_delay.left_pending() > 0
            || self.compensation_delay.left_pending() > 0
    }
    /// Native `0x9d9784`: visit enabled nested effects on zeroed scratch
    /// with a silent context, then mark lane ramps initial for re-entry.
    fn process_silently(&mut self, frames: usize) -> Result<(), GraphError> {
        if self.model.enabled {
            self.wet[0].fill(0.0); self.wet[1].fill(0.0);
            let [left, right] = &mut self.wet;
            let mut silent = true;
            for effect in &mut self.effects {
                effect.process(&mut silent, &mut left[..frames], &mut right[..frames])?;
            }
            self.mix.mark_initial();
            self.gain.mark_initial();
        }
        Ok(())
    }
}

/// The four hash-pinned shipped Multipass topologies. Construction reserves all
/// audio scratch and delay storage. Each call is one positive HeartCore chunk;
/// `set_parameter` queues current-template writes in their caller order.
pub struct MultipassGraph {
    rate: f32,
    preset: ShippedPreset,
    crossover_scale: CrossoverFrequencyScale,
    crossover_hz: [f32; 4],
    lanes: [Lane; 7],
    splitter: MultipassBandSplitter,
    routers: [BandRouteMixer; 5],
    dry_allpass: RootDryAllpass,
    output_gain: RootStereoGain,
    output_mix: LaneDryWetMixer,
    root_dry: [DryDelay; 2],
    dry_reference: [DryDelay; 2],
    aux_delay: LaneStereoDelay,
    root_gate: NativeSleepGate,
    root_previous_active: bool,
    root_synchronize_active: bool,
    graph_silent: bool,
    graph_quiet_frames: i32,
    band_buffers: [[[f32; MAX_FRAMES]; 2]; 5],
    #[cfg(test)]
    band_inputs: [[[f32; MAX_FRAMES]; 2]; 5],
    aux: [[f32; MAX_FRAMES]; 2],
    dry: [[f32; MAX_FRAMES]; 2],
    automation: [AutomationRamp; 64],
    pending: Vec<Write>,
}

impl MultipassGraph {
    pub fn new(sample_rate: f32, preset: ShippedPreset) -> Result<Self, GraphError> {
        if !sample_rate.is_finite() || sample_rate <= QUIET_RATE { return Err(GraphError::InvalidSampleRate); }
        let snapshot = preset.snapshot();
        // These route destinations and depth words are the live descriptor
        // values in graph-trace-*-baseline-03, independently tied to the five
        // version-1058 route records and 0x6230d0. Source is macro0, loaded
        // through its own [0,1] descriptor, not an arbitrary effective lane.
        let macro_raw = snapshot.macro_value;
        let macro_value = macro_raw.max(0.0).min(1.0);
        let source = native_macro_source(macro_value, 0);
        let depths = [0x3f7f_15f2, 0x3f7f_ffc8, 0x3f7f_fff8, 0x3f7f_ffd8, 0xbe83_a8c8];
        let mut modulation = [[0.0_f32; 4]; 7];
        if preset != ShippedPreset::Dynamics {
            for (route, (lane, field)) in [(5, 2), (1, 2), (2, 2), (6, 2), (2, 0)].into_iter().enumerate() {
                modulation[lane][field] = native_linear_modulation([0.0; 2], [source; 2], f32::from_bits(depths[route]), 1.0)[0];
            }
        }
        let mut lane_results = Vec::with_capacity(7);
        for lane in 0..7 {
            let raw = snapshot.lanes[lane];
            // Wrapper `0x4a84` replays each exposed host descriptor after
            // loading the preset. Apply its editor roundtrip before the
            // queued DSP-model roundtrip and current modulation.
            let replay = |field: LaneModelField, physical: f32, modulation: f32| {
                let editor_physical = field.model_event_value(physical, 0.0);
                field.model_event_value(editor_physical, modulation)
            };
            let model = LaneParameters {
                enabled: raw.enabled,
                gain: replay(LaneModelField::Gain, raw.gain, modulation[lane][0]),
                pan: replay(LaneModelField::Pan, raw.pan, modulation[lane][1]),
                mix: replay(LaneModelField::Mix, raw.mix, modulation[lane][2]),
                post: replay(LaneModelField::Post, raw.post, modulation[lane][3]),
                mute: raw.mute, solo: raw.solo,
            };
            let mut effects = Vec::with_capacity(snapshot.snapins[lane].len());
            for snapin in snapshot.snapins[lane] {
                effects.push(match &snapin.parameters {
                    ShippedEffectParameters::Dynamics(raw) => Effect::Dynamics(DynamicsEffect::new(sample_rate, raw)?),
                    ShippedEffectParameters::Distortion(raw) => Effect::Distortion {
                        kernel: DistortionProcessor::new(sample_rate, saturate_parameters(raw)).map_err(|_| GraphError::EffectError)?,
                        enabled: raw.enabled,
                    },
                    ShippedEffectParameters::TransientShaper(raw) => Effect::Transient(TransientEffect::new(sample_rate, raw)),
                });
            }
            lane_results.push(Lane {
                model, effects, first_delay: LaneStereoDelay::new(), compensation_delay: LaneStereoDelay::new(),
                mix: LaneDryWetMixer::new(), gain: LanePanGain::new(), wet: [[0.0; MAX_FRAMES]; 2],
                active: false, all_children_silent: false,
            });
        }
        let lanes: [Lane; 7] = lane_results.try_into().map_err(|_| GraphError::EffectError)?;
        let crossover_scale = CrossoverFrequencyScale::new();
        let crossover_hz = snapshot.crossover_hz.map(|raw| {
            let editor_physical = crossover_scale.from_normalized(crossover_scale.to_normalized(raw));
            crossover_scale.from_normalized(crossover_scale.to_normalized(editor_physical))
        });
        // Initial editor source values precede host events and are not child
        // DSP fields; apply the same source descriptor clamp as before.
        let automation = std::array::from_fn(|slot| {
            AutomationRamp::new(snapshot.automation_values[slot].max(0.0).min(1.0))
        });
        Ok(Self {
            rate: sample_rate, preset, crossover_scale,
            crossover_hz, lanes, splitter: MultipassBandSplitter::new(sample_rate),
            routers: std::array::from_fn(|_| BandRouteMixer::new()), dry_allpass: RootDryAllpass::new(sample_rate),
            output_gain: RootStereoGain::new(), output_mix: LaneDryWetMixer::new(),
            root_dry: [DryDelay::new(), DryDelay::new()], dry_reference: [DryDelay::new(), DryDelay::new()],
            aux_delay: LaneStereoDelay::new(),
            root_gate: NativeSleepGate::new(), root_previous_active: true, root_synchronize_active: true,
            graph_silent: false, graph_quiet_frames: 0,
            band_buffers: [[[0.0; MAX_FRAMES]; 2]; 5], aux: [[0.0; MAX_FRAMES]; 2],
            #[cfg(test)]
            band_inputs: [[[0.0; MAX_FRAMES]; 2]; 5],
            dry: [[0.0; MAX_FRAMES]; 2], automation, pending: Vec::with_capacity(64),
        })
    }

    /// Queue one normalized template control write. The four split controls,
    /// lane-5/6 gain controls and Dynamics automation slots are supported.
    /// Other IDs have not been traced as current-template writes.
    pub fn set_parameter(&mut self, id: u32, normalized: f32) -> Result<(), GraphError> {
        if !(id <= 3 || id == 34 || id == 40 || automation_slot_for_host_parameter(id).is_some()) {
            return Err(GraphError::InvalidParameter(id));
        }
        self.pending.push(Write { id, value: normalized });
        Ok(())
    }

    fn deliver_writes(&mut self, segment_remaining: usize) {
        for write in self.pending.drain(..) {
            match write.id {
                0..=3 => {
                    let raw = self.crossover_scale.from_normalized(write.value);
                    self.crossover_hz[write.id as usize] = self.crossover_scale.from_normalized(self.crossover_scale.to_normalized(raw));
                }
                34 | 40 => {
                    let index = if write.id == 34 { 5 } else { 6 };
                    let raw = lane_gain_from_normalized(write.value);
                    self.lanes[index].model.gain = lane_gain_from_normalized(lane_gain_to_normalized(raw).max(0.0).min(1.0));
                }
                id => {
                    let slot = automation_slot_for_host_parameter(id).unwrap();
                    let normalized = write.value.max(0.0).min(1.0);
                    self.automation[slot].schedule(normalized, segment_remaining);
                }
            }
        }
    }

    /// `0x623c28` advances normalized parent values before `0x623f80`
    /// copies each changing automation slot to its bound child model.
    fn advance_automation(&mut self, frames: usize) {
        for slot in 0..self.automation.len() {
            if let Some(normalized) = self.automation[slot].advance(frames) {
                if let Some(path) = self.preset.automation_path(slot) {
                    let lane = slot % 4;
                    let field = match path.event_id {
                        0 => DynamicsField::InputGain, 1 => DynamicsField::OutputGain,
                        2 => DynamicsField::LowThreshold, 3 => DynamicsField::LowRatio,
                        4 => DynamicsField::HighThreshold, 5 => DynamicsField::HighRatio,
                        6 => DynamicsField::Release, 7 => DynamicsField::Mix,
                        422 => DynamicsField::Attack, 423 => DynamicsField::Knee,
                        _ => unreachable!("pinned automation path"),
                    };
                    match &mut self.lanes[lane].effects[0] {
                        Effect::Dynamics(effect) => effect.set_normalized(field, normalized),
                        _ => unreachable!("pinned Dynamics automation owner"),
                    }
                }
            }
        }
    }

    /// Reset processor histories but retain the current delivered model and
    /// pending host events, matching the native DSP reset boundary.
    pub fn reset(&mut self) {
        self.splitter.reset(self.rate); self.dry_allpass.reset(); self.output_gain.reset();
        for lane in &mut self.lanes { lane.reset(); }
        self.root_dry[0].reset_counters(); self.root_dry[1].reset_counters();
        self.dry_reference[0].reset_counters(); self.dry_reference[1].reset_counters();
        self.aux_delay.reset_counters(); self.root_gate.reset();
        self.root_previous_active = true; self.root_synchronize_active = true;
        self.graph_silent = false; self.graph_quiet_frames = 0;
    }

    /// Published root latency, excluding the separate wrapper phase stage.
    pub fn latency_frames(&self) -> usize {
        let max_band = self.lanes[..5].iter().map(Lane::list_latency).max().unwrap_or(0);
        (self.lanes[5].list_latency() + max_band + self.lanes[6].list_latency()).max(0) as usize
    }

    pub fn process_chunk(&mut self, context: &mut GraphContext, left: &mut [f32], right: &mut [f32]) -> Result<(), GraphError> {
        let n = left.len();
        if n == 0 || n > MAX_FRAMES || right.len() != n || context.segment_remaining < n || context.segment_remaining > 512 {
            return Err(GraphError::InvalidBlock);
        }
        self.deliver_writes(context.segment_remaining);
        self.advance_automation(n);
        let published_latency = self.latency_frames();
        let dry_latency = published_latency.min(10_112);
        self.root_dry[0].configure(dry_latency); self.root_dry[1].configure(dry_latency);
        let active = !context.bypass;
        if self.root_synchronize_active {
            self.root_synchronize_active = false;
            self.root_previous_active = active;
        }
        if !active && !self.root_previous_active {
            self.root_dry[0].process(left); self.root_dry[1].process(right);
        } else {
            if active && !self.root_previous_active { self.reset(); }
            self.root_dry[0].remember(left); self.root_dry[1].remember(right);
            // Multipass slot 20 is `0x9d40e8`, which returns zero even for
            // a silent host chunk. Its slot-16 method has a separate nested
            // graph-silence branch; do not use the context byte here.
            match self.root_gate.select(false, published_latency as i32, n) {
                SleepDispatch::Active { .. } => self.process_active(context, left, right)?,
                SleepDispatch::Sleeping { .. } => unreachable!("Multipass slot 20 returns false"),
            }
            if !active { self.reset(); }
        }
        self.root_previous_active = active;
        if context.silent { context.silent = channels_are_silent(left, right); }
        if !context.silent { self.root_gate.clear_quiet_frames(); }
        else if !active { self.root_gate.mark_sleeping(); }
        Ok(())
    }

    fn process_active(&mut self, context: &mut GraphContext, left: &mut [f32], right: &mut [f32]) -> Result<(), GraphError> {
        let n = left.len();
        let max_band = self.lanes[..5].iter().map(Lane::list_latency).max().unwrap_or(0);
        let root_latency = self.latency_frames() as i32;
        if !context.silent {
            self.graph_silent = false;
            self.graph_quiet_frames = self.graph_quiet_frames.max(root_latency);
        }
        self.dry[0][..n].copy_from_slice(left); self.dry[1][..n].copy_from_slice(right);
        self.dry_reference[0].configure(root_latency.max(0).min(10_112) as usize);
        self.dry_reference[1].configure(root_latency.max(0).min(10_112) as usize);
        self.dry_reference[0].process(&mut self.dry[0][..n]);
        self.dry_reference[1].process(&mut self.dry[1][..n]);
        if self.graph_silent {
            for lane in &mut self.lanes { lane.process_silently(n)?; }
        } else {
            self.process_routing(context, left, right, max_band)?;
            let [dry_left, dry_right] = &mut self.dry;
            let layout = active_bands(std::array::from_fn(|i| self.lanes[i].model.enabled));
            let mut frequencies = [0.0_f32; 4];
            for i in 0..layout.crossover_count { frequencies[i] = self.crossover_hz[layout.crossovers[i]]; }
            self.dry_allpass.process(&frequencies[..layout.crossover_count], &mut dry_left[..n], &mut dry_right[..n]);
        }
        let [dry_left, dry_right] = &mut self.dry;
        self.output_gain.process(f32::from_bits(0x3f7f_ffff), left, right);
        self.output_mix.process(0.0, [&mut *left, &mut *right], [&dry_left[..n], &dry_right[..n]]);
        if self.graph_quiet_frames >= 0 {
            self.graph_quiet_frames = self.graph_quiet_frames.wrapping_sub(n as i32);
        } else {
            context.silent = channels_are_silent(left, right);
            if context.silent {
                self.graph_silent = true;
            } else {
                self.graph_quiet_frames = self.graph_quiet_frames.max(root_latency);
            }
        }
        Ok(())
    }

    fn process_routing(&mut self, context: &mut GraphContext, left: &mut [f32], right: &mut [f32], max_band: i32) -> Result<(), GraphError> {
        let n = left.len();
        let layout = active_bands(std::array::from_fn(|i| self.lanes[i].model.enabled));
        for lane in 0..7 { if !self.lanes[lane].model.enabled { self.lanes[lane].reset(); } }
        // `0x9d4918` passes lane 5 a copied context; its silence-byte
        // changes are not copied back to the root context.
        let mut lane_silent = context.silent;
        self.lanes[5].process(left, right, &mut lane_silent, 0, false)?;
        self.note_lane_activity(5);
        if layout.band_count == 0 {
            let compensation = max_band - self.lanes[0].list_latency();
            self.lanes[0].process(left, right, &mut context.silent, compensation, false)?;
            self.aux[0][..n].fill(0.0); self.aux[1][..n].fill(0.0);
        } else {
            let mut frequencies = [0.0_f32; 4];
            for i in 0..layout.crossover_count { frequencies[i] = self.crossover_hz[layout.crossovers[i]]; }
            let [b0, b1, b2, b3, b4] = &mut self.band_buffers;
            let [l0, r0] = b0; let [l1, r1] = b1; let [l2, r2] = b2;
            let [l3, r3] = b3; let [l4, r4] = b4;
            let mut output = [
                [&mut l0[..n], &mut r0[..n]], [&mut l1[..n], &mut r1[..n]],
                [&mut l2[..n], &mut r2[..n]], [&mut l3[..n], &mut r3[..n]],
                [&mut l4[..n], &mut r4[..n]],
            ];
            self.splitter.process([&*left, &*right], &frequencies[..layout.crossover_count], &mut output[..layout.band_count]);
            #[cfg(test)]
            for compact in 0..layout.band_count {
                self.band_inputs[compact][0][..n].copy_from_slice(&self.band_buffers[compact][0][..n]);
                self.band_inputs[compact][1][..n].copy_from_slice(&self.band_buffers[compact][1][..n]);
            }
            let any_solo = layout.bands[..layout.band_count].iter().any(|&i| self.lanes[i].model.solo);
            for compact in 0..layout.band_count {
                let index = layout.bands[compact];
                let [l, r] = &mut self.band_buffers[compact];
                let mut band_silent = false;
                let compensation = max_band - self.lanes[index].list_latency();
                self.lanes[index].process(&mut l[..n], &mut r[..n], &mut band_silent, compensation, any_solo)?;
                self.note_lane_activity(index);
            }
            left.fill(0.0); right.fill(0.0); self.aux[0][..n].fill(0.0); self.aux[1][..n].fill(0.0);
            let [aux_left, aux_right] = &mut self.aux;
            for compact in 0..layout.band_count {
                let index = layout.bands[compact];
                let [l, r] = &self.band_buffers[compact];
                self.routers[index].process(self.lanes[index].model.post, [&l[..n], &r[..n]],
                    [&mut aux_left[..n], &mut aux_right[..n]], [&mut *left, &mut *right]);
            }
        }
        // Lane 6 likewise receives a fresh copied root context.
        let mut lane_silent = context.silent;
        self.lanes[6].process(left, right, &mut lane_silent, 0, false)?;
        self.note_lane_activity(6);
        self.aux_delay.configure(self.lanes[6].list_latency());
        let [aux_left, aux_right] = &mut self.aux;
        let mut aux_silent = false;
        self.aux_delay.process(&mut aux_left[..n], &mut aux_right[..n], &mut aux_silent);
        for i in 0..n { left[i] += aux_left[i]; right[i] += aux_right[i]; }
        Ok(())
    }

    fn note_lane_activity(&mut self, lane: usize) {
        if self.lanes[lane].keeps_graph_awake() {
            self.graph_quiet_frames = self.graph_quiet_frames.max(self.latency_frames() as i32);
        }
    }
}
#[cfg(test)]
mod graph_intermediate_reference {
    use super::*;
    use crate::fp_mode::with_flush_to_zero;
    use serde_json::Value;
    use std::io::{BufRead, BufReader};

    fn word(value: &Value) -> u32 {
        u32::from_str_radix(value.as_str().expect("hex word"), 16).expect("valid hex word")
    }

    #[test]
    fn semantic_automation_sources_initialize_and_ramp_before_child_delivery() {
        with_flush_to_zero(|| {
            let mut graph = MultipassGraph::new(48_000.0, ShippedPreset::Dynamics).unwrap();
            for (slot, ramp) in graph.automation.iter().enumerate() {
                let expected = match slot {
                    4..=7 => 0x3f80_0000,
                    18 | 22 | 24 | 34 | 35 => 0x3f00_0000,
                    38 => 0x3dcc_cccd,
                    _ => 0,
                };
                assert_eq!(ramp.current.to_bits(), expected, "automation slot {slot}");
                assert_eq!(ramp.target.to_bits(), expected, "automation target {slot}");
            }
            graph.set_parameter(117, 0.0).unwrap();
            graph.deliver_writes(512);
            graph.advance_automation(64);
            assert_eq!(graph.automation[4].current.to_bits(), 0x3f60_0000);
            let Effect::Dynamics(effect) = &graph.lanes[0].effects[0] else {
                panic!("expected Dynamics automation owner");
            };
            assert_eq!(effect.parameters.high_threshold_db.to_bits(), (-6.25_f32).to_bits());
            for preset in [ShippedPreset::Gain, ShippedPreset::Warmth1, ShippedPreset::Warmth2] {
                let graph = MultipassGraph::new(48_000.0, preset).unwrap();
                assert!(graph.automation.iter().all(|ramp| ramp.current.to_bits() == 0));
            }
        });
    }

    #[test]
    fn automation_first_chunk_uses_native_remaining_frame_arithmetic() {
        let mut high = AutomationRamp::new(0.0);
        high.schedule(f32::from_bits(0x3e15_8509), 512);
        let first = high.advance(64).unwrap();
        assert_eq!(first.to_bits(), 0x3c95_8509);
        let physical = DynamicsField::HighRatio.model_physical_from_normalized(first);
        assert_eq!((physical - 1.0).to_bits(), 0x3f5b_2214);

        let mut threshold = AutomationRamp::new(1.0);
        threshold.schedule(f32::from_bits(0x3f0f_5c29), 512);
        let first = threshold.advance(64).unwrap();
        assert_eq!(DynamicsField::HighThreshold.model_physical_from_normalized(first).to_bits(), 0xc030_0002);

        let mut low = AutomationRamp::new(0.0);
        low.schedule(0.5, 512);
        let first = low.advance(64).unwrap();
        let physical = DynamicsField::LowRatio.model_physical_from_normalized(first);
        assert_eq!((1.0 - physical).to_bits(), 0xc194_e19b);
    }

    #[test]
    #[ignore = "requires revision-05 zero-frame reference captures"]
    fn sleeping_ksot_reset_matches_zero_frame_probe() {
        let mut mismatch_count = 0;
        for (environment, preset) in [
            ("PLATINUM_ZERO_DYNAMICS", ShippedPreset::Dynamics),
            ("PLATINUM_ZERO_WARMTH1", ShippedPreset::Warmth1),
            ("PLATINUM_ZERO_WARMTH2", ShippedPreset::Warmth2),
        ] {
            let path = std::env::var_os(environment).expect("missing zero-frame probe");
            let reader = BufReader::new(std::fs::File::open(path).expect("open zero-frame probe"));
            let mut graph = MultipassGraph::new(48_000.0, preset).unwrap();
            let mut identity = false;
            let mut warmed = false;
            let mut compared = 0;
            for line in reader.lines() {
                let record: Value = serde_json::from_str(&line.expect("read zero-frame probe")).expect("JSONL");
                if record["record"] == "identity" {
                    assert_eq!(record["wrapper_sha256"], "e8e5b86093f3f781479422c300cfa4469c21bb8c302ff46c59b5de184ca44703");
                    assert_eq!(record["core_sha256"], "35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578");
                    assert_eq!(record["preset_sha256"], preset.snapshot().file_sha256);
                    assert_eq!(record["mode"], "probe-zero");
                    assert_eq!(record["phase_bits"], "00000");
                    assert_eq!(record["rate"], 48_000);
                    identity = true;
                } else if record["record"] == "parameter" {
                    graph.set_parameter(record["id"].as_u64().unwrap() as u32,
                        f32::from_bits(word(&record["requested_bits"]))).unwrap();
                } else if record["record"] == "render" && record["phase"] == "zero_frame_probe" {
                    assert_eq!(record["status"], 0);
                    assert_eq!(record["warmup_before"], false);
                    assert_eq!(record["warmup_after"], true);
                    let mut remaining = 2400_usize;
                    while remaining > 0 {
                        let block = remaining.min(512);
                        let chunks = block.div_ceil(64);
                        let mut left = [0.0_f32; 512];
                        let mut right = [0.0_f32; 512];
                        with_flush_to_zero(|| {
                            for k in 0..chunks {
                                let start = k * block / chunks;
                                let end = (k + 1) * block / chunks;
                                let mut context = GraphContext {
                                    silent: true, bypass: false, segment_remaining: block - start,
                                };
                                graph.process_chunk(&mut context, &mut left[start..end], &mut right[start..end]).unwrap();
                            }
                        });
                        remaining -= block;
                    }
                    warmed = true;
                } else if record["phase"] == "after_zero_frame_probe" && record["lanes"].is_array() {
                    assert!(warmed);
                    for lane in 0..7 {
                        for (index, effect) in graph.lanes[lane].effects.iter().enumerate() {
                            if let Effect::Dynamics(dynamics) = effect {
                                let native = record["lanes"][lane]["snapins"][index]["master"]["voices"][0]["kernel_words"]
                                    .as_array().expect("native Dynamics kernel state");
                                let rust = dynamics.kernel.debug_kernel_words();
                                assert_eq!(native.len(), rust.len());
                                for (position, (actual, expected)) in rust.into_iter().zip(native).enumerate() {
                                    let expected = word(expected);
                                    if actual != expected {
                                        println!("{environment} lane={lane} state[{position}] rust={actual:08x} native={expected:08x}");
                                        mismatch_count += 1;
                                    }
                                }
                                compared += 1;
                            }
                        }
                    }
                }
            }
            assert!(identity && warmed);
            assert_eq!(compared, if preset == ShippedPreset::Dynamics { 4 } else { 1 });
        }
        assert_eq!(mismatch_count, 0, "sleeping Dynamics reset state differs");
    }

    #[test]
    #[ignore = "requires revision-04 real-wrapper graph traces"]
    fn compare_last_chunk_intermediates() {
        let mut grand_total = 0_usize;
        for (env, preset) in [
            ("PLATINUM_GRAPH_GAIN", ShippedPreset::Gain),
            ("PLATINUM_GRAPH_DYNAMICS", ShippedPreset::Dynamics),
            ("PLATINUM_GRAPH_WARMTH1", ShippedPreset::Warmth1),
            ("PLATINUM_GRAPH_WARMTH2", ShippedPreset::Warmth2),
        ] {
            let path = std::env::var_os(env).expect("missing revision-04 graph trace");
            let reader = BufReader::new(std::fs::File::open(path).expect("open graph trace"));
            let mut graph = MultipassGraph::new(48_000.0, preset).unwrap();
            let mut identity = false;
            let mut warm = false;
            let mut last_frames = 0_usize;
            let mut last_phase = String::new();
            let mut seen = 0_usize;
            let mut count = 0_usize;
            for line in reader.lines() {
                let record: Value = serde_json::from_str(&line.expect("read graph trace")).expect("JSONL");
                if record["record"] == "identity" {
                    assert_eq!(record["wrapper_sha256"], "e8e5b86093f3f781479422c300cfa4469c21bb8c302ff46c59b5de184ca44703");
                    assert_eq!(record["core_sha256"], "35ec06ec68ce60bb316c15286e7ef3d8b452d1d04d115383c9eadbc07411a578");
                    assert_eq!(record["preset_sha256"], preset.snapshot().file_sha256);
                    assert_eq!(record["rate"], 48_000);
                    assert_eq!(record["phase_bits"], "00000");
                    assert_eq!(record["mode"], "trace");
                    identity = true;
                } else if record["record"] == "status" {
                    assert_eq!(record["status"], 0);
                } else if record["record"] == "parameter" {
                    graph.set_parameter(record["id"].as_u64().unwrap() as u32,
                        f32::from_bits(word(&record["requested_bits"]))).unwrap();
                } else if record["record"] == "render" {
                    assert!(identity);
                    assert_eq!(record["status"], 0);
                    if !warm {
                        let mut remaining = 2400_usize;
                        while remaining > 0 {
                            let block = remaining.min(512);
                            let mut left = [0.0_f32; 512];
                            let mut right = [0.0_f32; 512];
                            let chunks = block.div_ceil(64);
                            with_flush_to_zero(|| {
                                for k in 0..chunks {
                                    let start = k * block / chunks;
                                    let end = (k + 1) * block / chunks;
                                    let mut context = GraphContext { silent: true, bypass: false, segment_remaining: block - start };
                                    graph.process_chunk(&mut context, &mut left[start..end], &mut right[start..end]).unwrap();
                                }
                            });
                            remaining -= block;
                        }
                        warm = true;
                    }
                    let n = record["frames"].as_u64().unwrap() as usize;
                    let time = record["sample_time"].as_u64().unwrap() as usize;
                    let zero = record["input_zero"].as_bool().unwrap();
                    let mut left = [0.0_f32; 512];
                    let mut right = [0.0_f32; 512];
                    if !zero {
                        for i in 0..n {
                            left[i] = (((time + i) * 17 % 31) as i32 - 15) as f32 / 64.0;
                            right[i] = (((time + i) * 11 % 29) as i32 - 14) as f32 / 64.0;
                        }
                    }
                    let chunks = n.div_ceil(64);
                    with_flush_to_zero(|| {
                        for k in 0..chunks {
                            let start = k * n / chunks;
                            let end = (k + 1) * n / chunks;
                            let mut context = GraphContext { silent: zero, bypass: false, segment_remaining: n - start };
                            graph.process_chunk(&mut context, &mut left[start..end], &mut right[start..end]).unwrap();
                        }
                    });
                    last_frames = n - (chunks - 1) * n / chunks;
                    last_phase = record["phase"].as_str().unwrap().to_owned();
                } else if record["phase"].as_str() == Some(last_phase.as_str())
                    && record["root_graph"]["master"]["voices"].is_array()
                    && (last_phase.starts_with("trace_generated_") || last_phase.starts_with("trace_wake_"))
                {
                    let observed = &record["root_graph"]["master"]["voices"][0]["lanes"];
                    let layout = active_bands(std::array::from_fn(|i| graph.lanes[i].model.enabled));
                    for lane in 0..7 {
                        let mut compare = |kind: &str, actual: &[[f32; MAX_FRAMES]; 2]| {
                            let Some(expected) = observed[lane][kind].as_array() else { return; };
                            let mut first = None;
                            let mut mismatches = 0_usize;
                            for channel in 0..2 {
                                let channel_values = expected[channel].as_array().expect("native channel");
                                assert_eq!(channel_values.len(), last_frames);
                                for (frame, value) in channel_values.iter().enumerate() {
                                    let native = word(value);
                                    let rust = actual[channel][frame].to_bits();
                                    if rust != native {
                                        mismatches += 1;
                                        if first.is_none() { first = Some((channel, frame, rust, native)); }
                                    }
                                }
                            }
                            if let Some((channel, frame, rust, native)) = first {
                                println!("{env} {last_phase} lane={lane} {kind} channel={channel} frame={frame} rust={rust:08x} native={native:08x} mismatches={mismatches}");
                            } else {
                                println!("{env} {last_phase} lane={lane} {kind} exact");
                            }
                            count += mismatches;
                        };
                        compare("wet_bits", &graph.lanes[lane].wet);
                        if lane < 5 {
                            if let Some(compact) = layout.bands[..layout.band_count].iter().position(|&x| x == lane) {
                                compare("input_bits", &graph.band_inputs[compact]);
                                compare("band_bits", &graph.band_buffers[compact]);
                            }
                        }
                        drop(compare);
                        for (index, effect) in graph.lanes[lane].effects.iter().enumerate() {
                            if let Effect::Dynamics(dynamics) = effect {
                                let native = &record["lanes"][lane]["snapins"][index]["master"]["voices"][0]["kernel_words"];
                                if let Some(words) = native.as_array() {
                                    let rust = dynamics.kernel.debug_kernel_words();
                                    assert_eq!(words.len(), rust.len());
                                    for (position, (actual, expected)) in rust.into_iter().zip(words).enumerate() {
                                        let native_word = word(expected);
                                        if actual != native_word {
                                            println!("{env} {last_phase} lane={lane} dynamics_state[{position}] rust={actual:08x} native={native_word:08x}");
                                            count += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    seen += 1;
                }
            }
            assert!(identity && warm);
            assert_eq!(seen, 8, "missing generated/wake snapshot");
            println!("{env} intermediate_mismatched_samples={count}");
            grand_total += count;
        }
        assert_eq!(grand_total, 0, "one or more graph intermediate samples differ");
    }
}
