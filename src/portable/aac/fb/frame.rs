//! Frame-level driver of the filterbank area: the part of `AACEncoder::EncodeFrame` (0x12ed60) from the input clamp to the end of the TNS stage
//! (steps 1-4):
//!
//! 1. clamp/scale the client floats ([`crate::portable::aac::fb::input::clamp_and_scale`]), 2. `_ComputeBlockMode` (history ring + block switching + CPE join),
//! 3. per channel the two transform banks (A: psy short transforms, B: coding transform, short-frame data path),
//! 4. `_processTnsChannel` per channel and the psy-copy pass.
//!
//! The psychoacoustic model, bandwidth lowpass, M/S, quantiser etc. follow in the other modules (`psy`, `ms`, `int`, `quant`).

use crate::portable::aac::fb::bandwidth::{engine_bandwidth, ChannelBandwidth};
use crate::portable::aac::fb::blocksw::{BlockSwitch, BsLayout, TrdeCfg};
use crate::portable::aac::fb::input::{clamp_and_scale, FRAME_LEN};
use crate::portable::aac::fb::mdct::{FilterBank, SEQ_EIGHT_SHORT, SEQ_ONLY_LONG};
use crate::portable::aac::fb::tns::{self, TnsConfig, TnsResult};
use crate::portable::aac::types::ElementKind;

/// Static configuration of one channel.
#[derive(Clone, Debug)]
pub struct ChannelCfg {
    /// `chan+0x131 != 0`: CPE channel with its own bank-A object (always-on psy short transform); SCE/LFE channels share bank B for A.
    pub own_bank_a: bool,
    /// TNS configuration (`None` for LFE channels: no TnsChannel object).
    pub tns: Option<TnsConfig>,
    /// Bandwidth lowpass (cutoff lines, taper).
    pub bandwidth: ChannelBandwidth,
}

/// Configuration of the whole area for one encoder instance.
#[derive(Clone, Debug)]
pub struct FbConfig {
    pub channels: usize,
    /// `aac+0xe0` = `'cdqu'` after the `srcq` map (0x40 -> 64).
    pub quality: u32,
    pub layout: BsLayout,
    pub channel: Vec<ChannelCfg>,
}

impl FbConfig {
    /// Engine configuration (`ExtAudioFile` m4af, `'brat'` `brat`, `'cdqu'` `quality`, `'acbf'` 1; 1..8 channels): the layout, the bandwidth table and the TNS
    /// parameters of the stream's format. The block-switching parameters (TRDE block) come from the psychoacoustic model's parameters
    /// ([`crate::portable::aac::psy::PamParam::trde`]) and are handed to [`FbState::new`].
    pub fn engine(sample_rate: u32, channels: usize, brat: u32, quality: u32) -> Self {
        let layout = BsLayout::engine(channels);
        let kinds: Vec<ElementKind> = match channels {
            1 => vec![ElementKind::Sce],
            2 => vec![ElementKind::Cpe; 2],
            3 => vec![ElementKind::Sce, ElementKind::Cpe, ElementKind::Cpe],
            4 => vec![ElementKind::Cpe; 4],
            5 => [vec![ElementKind::Sce], vec![ElementKind::Cpe; 4]].concat(),
            6 => [vec![ElementKind::Sce], vec![ElementKind::Cpe; 4], vec![ElementKind::Lfe]].concat(),
            7 => [vec![ElementKind::Sce], vec![ElementKind::Cpe; 4], vec![ElementKind::Sce, ElementKind::Lfe]].concat(),
            8 => [vec![ElementKind::Sce], vec![ElementKind::Cpe; 6], vec![ElementKind::Lfe]].concat(),
            _ => panic!("unsupported channel count {channels}"),
        };
        let n_fbw = match channels {
            6..=8 => channels - 1,
            _ => channels,
        } as u32;
        let per_ch = brat / n_fbw;
        let channel = (0..channels)
            .map(|c| {
                let kind = kinds[c];
                let cpe_of_3 = channels == 3 && kind == ElementKind::Cpe;
                let bandwidth = engine_bandwidth(sample_rate, channels, cpe_of_3, brat);
                let tns = if kind == ElementKind::Lfe { None } else { TnsConfig::new(sample_rate, per_ch, if channels == 1 { 0 } else { 1 }) };
                ChannelCfg { own_bank_a: kind == ElementKind::Cpe, tns, bandwidth }
            })
            .collect();
        Self { channels, quality, layout, channel }
    }
}

/// Output of one channel for one frame (= the access unit being coded).
#[derive(Clone, Debug)]
pub struct ChannelFb {
    /// Window sequence of the AU after the CPE join (0 LONG, 1 START, 2 EIGHT_SHORT, 3 STOP).
    pub wseq: u8,
    /// `chan+0x140`: the coding spectrum after TNS, **before** the bandwidth lowpass (ISO scale `X`; EIGHT_SHORT: 8 x 128 in window order = bank A's output).
    pub coded: [f32; 1024],
    /// `chan+0x148`: psy long-window spectrum `0.5 X` (after TNS for long/start/stop frames; for EIGHT_SHORT frames `0.5 *` the long-window transform of the
    /// time block run only to advance bank B, unfiltered).
    pub psy_long: [f32; 1024],
    /// `chan+0x1d8`: psy short-window spectra `0.5 *` bank A output (8 x 128; after TNS for EIGHT_SHORT frames). `None` when bank A did not run
    /// (SCE/LFE channel in a non-short frame without pre-echo flag), exactly as in Apple's code.
    pub psy_short: Option<[f32; 1024]>,
    /// `chan+0x240`: pre-echo mask of the block switch (the psy model reads it).
    pub pre_echo_mask: u16,
    /// TnsChannel object (`None` for channels without TNS object, i.e. LFE).
    pub tns: Option<TnsResult>,
}

/// Output of one `process_frame`.
#[derive(Clone, Debug)]
pub struct FrameFb {
    /// Index of the access unit (= number of earlier `process_frame` calls).
    pub au_index: u64,
    pub channels: Vec<ChannelFb>,
}

/// Per-stream state of the filterbank area (history rings, detector, both transform banks).
pub struct FbState {
    pub cfg: FbConfig,
    bs: BlockSwitch,
    bank_a: FilterBank,
    bank_b: FilterBank,
    frames: u64,
}

impl FbState {
    /// `trde`: the block-switching parameters (the TRDE block of the psychoacoustic model's parameters).
    pub fn new(cfg: &FbConfig, trde: TrdeCfg) -> Self {
        let n = cfg.channels;
        Self {
            cfg: cfg.clone(),
            bs: BlockSwitch::new(n, trde, cfg.quality, cfg.layout.clone()),
            bank_a: FilterBank::new(n),
            bank_b: FilterBank::new(n),
            frames: 0,
        }
    }

    /// One `EncodeFrame` worth of input: `input[ch]` = 1024 client float32 samples of channel `ch` (unscaled; scaling and clamping happen here).
    /// Call `j` (0-based) returns the coded block `[1024 j - 1088, 1024 j - 64)` of AU `j` with the window sequence decided for it.
    pub fn process_frame(&mut self, input: &[&[f32]]) -> FrameFb {
        let nch = self.cfg.channels;
        assert_eq!(input.len(), nch);
        let mut scaled = vec![[0.0f32; FRAME_LEN]; nch];
        for c in 0..nch {
            assert_eq!(input[c].len(), FRAME_LEN);
            for i in 0..FRAME_LEN {
                scaled[c][i] = clamp_and_scale(input[c][i]);
            }
        }
        let refs: Vec<&[f32]> = scaled.iter().map(|b| &b[..]).collect();
        let mut delayed = vec![[0.0f32; FRAME_LEN]; nch];
        let modes = self.bs.compute_block_mode(&refs, &mut delayed);
        let mut out = Vec::with_capacity(nch);
        for c in 0..nch {
            out.push(self.transform_channel(c, modes[c], &delayed[c]));
        }
        // TNS (stage 4): analysis + filtering of the coding spectrum, then the psy copies
        for c in 0..nch {
            let ch = &mut out[c];
            if let Some(tc) = &self.cfg.channel[c].tns {
                let bw_line = self.cfg.channel[c].bandwidth.bw_line(ch.wseq);
                let res = tns::process_channel(tc, self.cfg.quality, &mut ch.coded, ch.wseq, bw_line);
                if res.active {
                    if ch.wseq == SEQ_EIGHT_SHORT {
                        let ps = ch.psy_short.as_mut().expect("EIGHT_SHORT frames always run bank A");
                        tns::apply_to_psy_copy(tc, &res, &mut ps[..]);
                    } else {
                        tns::apply_to_psy_copy(tc, &res, &mut ch.psy_long[..]);
                    }
                }
                ch.tns = Some(res);
            }
        }
        let au_index = self.frames;
        self.frames += 1;
        FrameFb { au_index, channels: out }
    }

    /// Step 3 of EncodeFrame for one channel (0x12f984..0x12fd78): the A/B bank dispatch.
    fn transform_channel(&mut self, c: usize, mode: u8, delayed: &[f32; 1024]) -> ChannelFb {
        let cc = &self.cfg.channel[c];
        let shape = 0u8; // chan+0x23c: cleared at every START, only assigned for quality 0x60
        let short = mode == SEQ_EIGHT_SHORT;
        let pre_echo = self.bs.chans[c].pre_echo_mask;
        let run_a = cc.own_bank_a || short || pre_echo != 0;
        let mut x27 = *delayed; // chan+0x1d8 scratch
        if run_a {
            if cc.own_bank_a {
                self.bank_a.time_to_frequency_in_place(c, shape, SEQ_EIGHT_SHORT, &mut x27);
            } else {
                // same object as B: bracketed by GetState / SetState / SetWindowShape (PrepareTransform(seq 2) overwrites the overlap state)
                let st = self.bank_b.get_state(c);
                self.bank_b.time_to_frequency_in_place(c, shape, SEQ_EIGHT_SHORT, &mut x27);
                self.bank_b.set_state(c, &st);
                self.bank_b.set_window_shape(c, shape);
            }
        }
        let mut coded;
        let mut psy_long = [0.0f32; 1024];
        let mut psy_short = None;
        if short {
            coded = x27; // A's unscaled output becomes the coded spectrum
            let mut half = x27;
            for v in half.iter_mut() {
                *v *= 0.5;
            }
            psy_short = Some(half);
            // B runs seq 0 on a backup of the time block only to advance its overlap state; its spectrum feeds the psy model
            let mut backup = *delayed;
            self.bank_b.time_to_frequency_in_place(c, shape, SEQ_ONLY_LONG, &mut backup);
            for i in 0..1024 {
                psy_long[i] = backup[i] * 0.5;
            }
        } else {
            if run_a {
                let mut half = x27;
                for v in half.iter_mut() {
                    *v *= 0.5;
                }
                psy_short = Some(half);
            }
            coded = *delayed;
            self.bank_b.time_to_frequency_in_place(c, shape, mode, &mut coded);
            for i in 0..1024 {
                psy_long[i] = coded[i] * 0.5;
            }
        }
        ChannelFb { wseq: mode, coded, psy_long, psy_short, pre_echo_mask: pre_echo, tns: None }
    }
}
