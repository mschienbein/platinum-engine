//! `AacEncoder`: the access-unit loop of `AACEncoder::EncodeFrame` + `_aacEncodeFrame` + `WriteBitstream` + `AACEncodeOutput` for the engine
//! configuration, built from the stage modules in Apple's call order:
//!
//! ```text
//! per AU (1024 client frames per channel):
//!   fb       clamp/scale, history ring, block switching, banks A/B, TNS                          (Front::analyse)
//!   psy      PAM per channel -> short grouping -> AdvanceCodChannelPam -> AdvancePsyOut(#1)      (Front::analyse)
//!   [CPE]    M/S candidates + PE + M/S decision, intensity analysis/decision                     (stereo stage)
//!   psy      _sortShortSpectrum + _calculateMaxSfb + pair unification, TNS clear                 (Front::finalize)
//!   quant    side-info budget -> _AdvanceLoopKernel                                              (Quantizer::quantize_frame)
//!   bits     access unit (PCE in AU 0 of 4/7 channel streams)                                    (write_access_unit)
//!   quant    AACEncodeOutput statistics -> bias B -> psy `_adaptPamParam(B)`                     (Quantizer::end_au)
//! ```
//!
//! The access units depend only on the sample sequence: how it is cut into pushes is irrelevant. They are queued as they are produced and leave the
//! encoder through [`AacEncoder::drain_access_units`], so a caller that drains after every push keeps the memory bounded by one push, however long
//! the stream is.

use std::collections::VecDeque;

use crate::portable::aac::bits::{grouping_bits, write_access_unit};
use crate::portable::aac::encoder::config::{ConfigError, EngineConfig};
use crate::portable::aac::encoder::front::Front;
use crate::portable::aac::fb::input::InputFramer;
use crate::portable::aac::int::{tns_ranges, ChannelLines, IntensityChannel};
use crate::portable::aac::ms::mask::{ms_mask_present, ms_used_bits, MS_MASK_PER_BAND};
use crate::portable::aac::ms::{build_ms_candidate, calc_channel_pe, MsCandidateScratch, MsConfig, MsError, MsTool};
use crate::portable::aac::psy::calculate_sfb_center_freqs;
use crate::portable::aac::quant::output::tns_bits_of;
use crate::portable::aac::quant::{ElementSide, Quantizer};
use crate::portable::aac::types::{AccessUnit, ChannelStream, Element, ElementKind, IcsInfo, PsyOut};

/// A failure of the access-unit loop of a created encoder.
#[derive(Debug)]
pub enum EncodeError {
    /// The bitstream writer could not serialise an access unit.
    Write(crate::portable::aac::bits::WriteError),
    /// The M/S stage refused the frame (PE argument error: the real encoder fails the frame too).
    Ms(MsError),
    /// The access unit the writer produced is not as long as the quantiser's bit accounting says: the rate-control state would no longer follow the
    /// original's, so the stream is not continued.
    BudgetMismatch { au_index: u64, written: u32, budgeted: u32 },
    /// An earlier block failed (or the stream failed while finishing): the encoder produces nothing more, [`AacEncoder::finish`] included.
    Failed,
    /// Input length is not a whole number of frames.
    PartialFrame,
    /// Samples were pushed after [`AacEncoder::finish`].
    Finished,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Write(e) => write!(f, "bitstream writer: {e}"),
            EncodeError::Ms(e) => write!(f, "M/S stage: {e}"),
            EncodeError::BudgetMismatch { au_index, written, budgeted } => write!(f, "access unit {au_index} is {written} bytes long but its bit accounting says {budgeted}"),
            EncodeError::Failed => write!(f, "the encoder failed earlier and cannot continue"),
            EncodeError::PartialFrame => write!(f, "interleaved input is not a whole number of frames"),
            EncodeError::Finished => write!(f, "the encoder has already finished its stream"),
        }
    }
}
impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EncodeError::Write(e) => Some(e),
            _ => None,
        }
    }
}
impl From<MsError> for EncodeError {
    fn from(e: MsError) -> Self {
        EncodeError::Ms(e)
    }
}
impl From<crate::portable::aac::bits::WriteError> for EncodeError {
    fn from(e: crate::portable::aac::bits::WriteError) -> Self {
        EncodeError::Write(e)
    }
}

/// Per-CPE stereo state: the M/S tool (stateless between frames), the intensity channel (`IC`, cross-frame state) and the psyOut#2 slots.
struct CpeWork {
    ms: MsTool,
    ic: IntensityChannel,
    scratch: MsCandidateScratch,
    m: PsyOut,
    s: PsyOut,
}

/// Streaming encoder: feed interleaved client float32, drain access units. It is `Send`.
pub struct AacEncoder {
    cfg: EngineConfig,
    front: Front,
    quant: Quantizer,
    /// stereo state of every element (`None` for SCE/LFE).
    cpe: Vec<Option<CpeWork>>,
    /// `None` once the stream is finished.
    framer: Option<InputFramer>,
    /// finished access units not yet drained.
    aus: VecDeque<Vec<u8>>,
    /// client frames pushed so far (the iTunSMPB valid-frame count of the file).
    frames: u64,
    /// a block failed: the stream is not continued (a later `finish` must not report success for a short stream).
    failed: bool,
    /// test hook: the access unit whose block fails
    #[cfg(test)]
    fail_at_au: Option<u64>,
}

impl AacEncoder {
    /// An encoder for `rate` Hz x `channels` channels at 256 kbit/s; `Err` unless [`accepts`](crate::portable::aac::encoder::accepts) takes the format.
    ///
    /// The set-up arithmetic (psychoacoustic and quantiser tables, DFT set-ups) runs with flush-to-zero cleared like the encoding, whatever the mode of the caller.
    pub fn new(rate: u32, channels: u32) -> Result<Self, ConfigError> {
        crate::fp_mode::without_flush_to_zero(|| Self::build(rate, channels))
    }

    fn build(rate: u32, channels: u32) -> Result<Self, ConfigError> {
        let cfg = EngineConfig::new(rate, channels)?;
        let quant = Quantizer::new(cfg.quant.clone());
        let front = Front::new(&cfg, quant.rate.b)?;
        let sfbc = calculate_sfb_center_freqs(rate).map_err(|_| ConfigError::Stage("sfb centre frequencies"))?;
        let cpe = cfg
            .elements
            .iter()
            .enumerate()
            .map(|(e, el)| {
                if el.kind != ElementKind::Cpe {
                    return None;
                }
                let ms_cfg = MsConfig::new(
                    rate,
                    cfg.per_channel_bitrate,
                    cfg.quality,
                    &front.pam.quiet_long_pow,
                    &front.pam.quiet_short_pow,
                    sfbc.first_ge_18k_long,
                    sfbc.first_ge_18k_short,
                    &sfbc.long,
                    &sfbc.short,
                );
                Some(CpeWork {
                    ms: MsTool::new(ms_cfg),
                    ic: IntensityChannel::new(cfg.intensity[e].clone().expect("CPE intensity configuration")),
                    scratch: MsCandidateScratch::new(),
                    m: PsyOut::new(),
                    s: PsyOut::new(),
                })
            })
            .collect();
        let framer = Some(InputFramer::new(channels as usize));
        Ok(Self {
            cfg,
            front,
            quant,
            cpe,
            framer,
            aus: VecDeque::new(),
            frames: 0,
            failed: false,
            #[cfg(test)]
            fail_at_au: None,
        })
    }

    /// Append interleaved float32 client frames (any number of whole frames); every complete 1024-frame block is encoded immediately and its access
    /// unit queued.
    ///
    /// The encoder itself runs with flush-to-zero cleared (`FPCR.FZ` = 0 / MXCSR FTZ|DAZ = 0): FZ = 1 changes coded spectra, so the engine's own
    /// [`with_flush_to_zero`](crate::fp_mode::with_flush_to_zero) scope must not reach the arithmetic. The effect of the caller's mode on
    /// AudioToolbox that is reproduced is the one on the *input*: with the mode set while the samples are written, its input path flushes subnormal samples
    /// to zero (the sign is kept). That is reproduced here: when the mode is set at the time of the call, the subnormal samples of this push are
    /// replaced by +-0 before they are encoded; with the mode clear (the engine writes outside its flush scopes) the samples are used as they are and
    /// nothing is copied. A stream whose pushes are made in different modes is flushed push by push. This is the verified part of the model, not all of
    /// it: with the mode set, AudioToolbox's own arithmetic also changes its result for input made of tiny normal numbers (a constant level of
    /// about 2^-126 to 4e-37, a sine of 1e-33 to 1e-30, subnormals mixed with one near-minimum normal sample, infinities or NaN mixed with
    /// subnormals), which is not reproduced; the smallest recorded case is 3,008 frames of mono DC at -2^-126 written with the mode set (18 such
    /// cases in 24,000 random ones written inside a flush scope). The engine never writes inside a flush scope, where the stream is exact.
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Result<(), EncodeError> {
        // a failed or finished stream accepts nothing, an empty push included; that is checked before the shape of the input
        self.check_open()?;
        if !samples.len().is_multiple_of(self.cfg.channels) {
            return Err(EncodeError::PartialFrame);
        }
        let flush = crate::fp_mode::flush_to_zero_enabled();
        crate::fp_mode::without_flush_to_zero(|| self.push_interleaved_inner(samples, flush))
    }

    fn check_open(&self) -> Result<(), EncodeError> {
        if self.failed {
            Err(EncodeError::Failed)
        } else if self.framer.is_none() {
            Err(EncodeError::Finished)
        } else {
            Ok(())
        }
    }

    fn push_interleaved_inner(&mut self, samples: &[f32], flush: bool) -> Result<(), EncodeError> {
        let ch = self.cfg.channels;
        // Feed the framer in bounded slices: `InputFramer` buffers per channel in a `Vec` that is drained from the front, so pushing a whole file
        // first would make the block pops quadratic.
        const SLICE_FRAMES: usize = 16 * 1024;
        let mut flushed = Vec::new();
        for part in samples.chunks(SLICE_FRAMES * ch) {
            let part = if flush {
                flush_subnormals(part, &mut flushed);
                &flushed[..]
            } else {
                part
            };
            self.framer.as_mut().ok_or(EncodeError::Finished)?.push_interleaved(part);
            self.frames += (part.len() / ch) as u64;
            while let Some(block) = self.framer.as_mut().and_then(InputFramer::pop_block) {
                self.encode_block(&block)?;
            }
        }
        Ok(())
    }

    /// End of stream: encode the zero-padded partial block and the flush blocks (`au_count(frames)` access units in total; none when no frame was
    /// written). Finishing a finished encoder does nothing; finishing one whose stream failed returns [`EncodeError::Failed`], also when it is the
    /// finish itself that failed (the stream is short then and must not be taken for complete).
    pub fn finish(&mut self) -> Result<(), EncodeError> {
        if self.failed {
            return Err(EncodeError::Failed);
        }
        crate::fp_mode::without_flush_to_zero(|| {
            let Some(framer) = self.framer.take() else { return Ok(()) };
            if self.frames == 0 {
                // ExtAudioFile with no frame written produces no access units at all
                return Ok(());
            }
            for block in framer.finish() {
                self.encode_block(&block)?;
            }
            Ok(())
        })
    }

    /// Client frames pushed so far.
    #[cfg(test)]
    pub fn frames_pushed(&self) -> u64 {
        self.frames
    }

    /// Access units produced and not yet drained.
    #[cfg(test)]
    pub fn pending_access_units(&self) -> usize {
        self.aus.len()
    }

    /// Hand every queued access unit, oldest first, to `sink` and drop it from the queue. A failing `sink` stops the drain and returns its error; the
    /// access unit it failed on and the later ones stay queued.
    pub fn drain_access_units<E>(&mut self, mut sink: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        while let Some(au) = self.aus.front() {
            sink(au)?;
            self.aus.pop_front();
        }
        Ok(())
    }

    /// Take every queued access unit.
    #[cfg(test)]
    pub fn take_access_units(&mut self) -> Vec<Vec<u8>> {
        self.aus.drain(..).collect()
    }

    /// One `EncodeFrame`: `blk[ch]` = 1024 client floats of channel `ch`. A failure poisons the encoder.
    fn encode_block(&mut self, blk: &[Vec<f32>]) -> Result<(), EncodeError> {
        let result = self.encode_block_inner(blk);
        self.failed |= result.is_err();
        result
    }

    fn encode_block_inner(&mut self, blk: &[Vec<f32>]) -> Result<(), EncodeError> {
        let refs: Vec<&[f32]> = blk.iter().map(|v| &v[..]).collect();
        let mut ff = self.front.analyse(&refs);
        let au_index = ff.au_index;
        #[cfg(test)]
        if self.fail_at_au == Some(au_index) {
            return Err(EncodeError::BudgetMismatch { au_index, written: 0, budgeted: 0 });
        }
        // E12-E13: M/S candidate set (psyOut#2) of every CPE, built from the UNSORTED L/R spectra
        self.build_ms_candidates(&mut ff);
        // E14: sort short spectra, max_sfb, pair unification, TNS clear
        self.front.finalize(&mut ff);
        // _aacEncodeFrame: calcChannelPE, intensity analysis, M/S decision, intensity decision
        self.stereo_stage(&mut ff)?;

        // ---- _aacEncodeFrame: budget block + loop kernel
        let cfg = &self.cfg;
        let mut sides: Vec<ElementSide> = Vec::with_capacity(cfg.elements.len());
        for el in &cfg.elements {
            let c0 = &ff.channels[el.first_channel].psy;
            let (ws1, tns1) = if el.kind == ElementKind::Cpe {
                let c1 = &ff.channels[el.first_channel + 1].psy;
                (c1.wseq, tns_bits_of(c1))
            } else {
                (0, 0)
            };
            let (mixed, nbands) = if el.kind == ElementKind::Cpe { ms_mask_shape(&ff, el.first_channel) } else { (false, 0) };
            sides.push(ElementSide { kind: el.kind, common_window: el.kind == ElementKind::Cpe, ms_mask_mixed: mixed, ms_mask_nbands: nbands, ws: [c0.wseq, ws1], tns_bits: [tns_bits_of(c0), tns1] });
        }
        let psy_refs: Vec<&PsyOut> = ff.channels.iter().map(|c| &c.psy).collect();
        let pce_bits = if au_index == 0 { cfg.pce_bits() } else { 0 };
        let qf = self.quant.quantize_frame(&psy_refs, &sides, pce_bits);

        // ---- WriteBitstream
        let au = self.assemble(&ff, au_index);
        let bytes = write_access_unit(&au, cfg.sf_index)?;
        if bytes.len() as u32 != qf.au_bytes {
            return Err(EncodeError::BudgetMismatch { au_index, written: bytes.len() as u32, budgeted: qf.au_bytes });
        }

        // ---- AACEncodeOutput: statistics, bias feedback -> _adaptPamParam
        let step = self.quant.end_au(&qf);
        if step.stepped {
            self.front.adapt(step.b);
        }
        self.aus.push_back(bytes);
        Ok(())
    }

    /// Syntax structure of the AU from the quantised channels.
    fn assemble(&self, ff: &crate::portable::aac::encoder::front::FrontFrame, au_index: u64) -> AccessUnit {
        let cfg = &self.cfg;
        let mut elements = Vec::with_capacity(cfg.elements.len());
        for el in &cfg.elements {
            let c0 = el.first_channel;
            let psy0 = &ff.channels[c0].psy;
            let info = ics_info(psy0);
            let mut channels: Vec<ChannelStream> = Vec::with_capacity(el.nch());
            for k in 0..el.nch() {
                let c = c0 + k;
                let psy = &ff.channels[c].psy;
                let mut cs = self.quant.channel_stream(c, psy);
                // `tns_data_present` follows the TNS object's `active` byte
                if !psy.tns.as_ref().is_some_and(|t| t.active) {
                    cs.tns = None;
                }
                channels.push(cs);
            }
            let (ms_present, ms_used) = if el.kind == ElementKind::Cpe { ms_mask(ff, c0) } else { (0, Vec::new()) };
            elements.push(Element {
                kind: el.kind,
                tag: el.tag,
                common_window: el.kind == ElementKind::Cpe,
                ics: [info, if el.kind == ElementKind::Cpe { info } else { IcsInfo::default() }],
                ms_mask_present: ms_present,
                ms_used,
                channels,
            });
        }
        AccessUnit { pce: if au_index == 0 { cfg.pce.clone() } else { None }, elements }
    }

    /// E12-E13 of `EncodeFrame`: the M/S candidate psyOut#2 of every CPE (`_getMsMDCT`, `_AdvanceCodChannelPam` x2, `_AdvancePsyOut` x2,
    /// `_ProcessPsyOutMS` x2), built from psyOut#1 as `_AdvancePsyOut` left it (spectra unsorted).
    fn build_ms_candidates(&mut self, ff: &mut crate::portable::aac::encoder::front::FrontFrame) {
        let p1ac = self.front.pam.p1ac;
        for (e, el) in self.cfg.elements.iter().enumerate() {
            let Some(w) = self.cpe[e].as_mut() else { continue };
            let (a, b) = (el.first_channel, el.first_channel + 1);
            let (ca, cb) = (&ff.channels[a], &ff.channels[b]);
            build_ms_candidate(&ca.psy, &cb.psy, &self.front.psy_ch[a], p1ac, ca.psy.field_5bc, ca.psy.wseq, &ca.grouping, &mut w.m, &mut w.s, &mut w.scratch);
        }
    }

    /// M/S + intensity stereo for every CPE (A1/A4/A6/A7 of `_aacEncodeFrame`).
    fn stereo_stage(&mut self, ff: &mut crate::portable::aac::encoder::front::FrontFrame) -> Result<(), EncodeError> {
        let quality = self.cfg.quality;
        for (e, el) in self.cfg.elements.iter().enumerate() {
            let Some(w) = self.cpe[e].as_mut() else { continue };
            let (a, b) = (el.first_channel, el.first_channel + 1);
            let (lo, hi) = ff.channels.split_at_mut(b);
            let (ca, cb) = (&mut lo[a], &mut hi[0]);
            // A1 calcChannelPE
            calc_channel_pe(&mut ca.psy, quality)?;
            calc_channel_pe(&mut cb.psy, quality)?;
            // per-line power arrays hanging off the psyOuts (E4/E5): `psyOut+8`, `+0x10[w]`, `+0x9c0`, `+0x9d0`
            let zero = [0.0f32; 1024];
            let lines = |c: &crate::portable::aac::encoder::front::ChannelFrame| {
                ChannelLines::from_psy_spectra(&c.fb.psy_long, c.fb.psy_short.as_ref().unwrap_or(&zero), c.fb.wseq == 2, c.psy.is_short)
            };
            let (la, lb) = (lines(ca), lines(cb));
            // A4 intensity analysis (before M/S, on the original L/R)
            w.ic.analyze(&ca.psy, &cb.psy, &la, &lb, tns_ranges(&ca.psy), tns_ranges(&cb.psy));
            // A6 M/S
            let short = ca.psy.wseq == 2;
            let (pl, pr) = if short { (&la.winen, &lb.winen) } else { (&la.en, &lb.en) };
            w.ms.advance_ms_stereo(&mut ca.psy, &mut cb.psy, &mut w.m, &mut w.s, pl, pr)?;
            // A7 intensity decision + application (after M/S)
            w.ic.decide_and_apply(&mut ca.psy, &mut cb.psy, &la, &lb);
        }
        Ok(())
    }
}

/// `ics_info` of a channel / the shared one of a CPE (window shape 0, `max_sfb`, `scale_factor_grouping`).
fn ics_info(psy: &PsyOut) -> IcsInfo {
    let short = psy.wseq == 2;
    IcsInfo {
        window_sequence: psy.wseq as u8,
        window_shape: 0,
        max_sfb: psy.max_sfb as u8,
        scale_factor_grouping: if short { grouping_bits(&psy.group_len[..psy.ngroups as usize]) } else { 0 },
    }
}

/// `ms_mask_present` (0 none, 1 per-band, 2 all) and the per-(group, band) mask of a CPE: read from the M/S flag bit of psyOut#1 channel 0 AFTER the
/// intensity stage (intensity-coded bands carry `ms_used == 0`), over the AU's `max_sfb` (`crate::portable::aac::ms::mask`).
fn ms_mask(ff: &crate::portable::aac::encoder::front::FrontFrame, c0: usize) -> (u8, Vec<Vec<bool>>) {
    let p = &ff.channels[c0].psy;
    let (stride, max_sfb, ng) = (p.nsfb_per_group as usize, p.max_sfb as usize, p.ngroups as usize);
    let present = ms_mask_present(&p.flags, ng, stride, max_sfb);
    if present != MS_MASK_PER_BAND {
        return (present, Vec::new());
    }
    let bits = ms_used_bits(&p.flags, ng, stride, max_sfb);
    (present, (0..ng).map(|g| bits[g * max_sfb..(g + 1) * max_sfb].iter().map(|&x| x != 0).collect()).collect())
}

/// `(ms_mask_present == 1, sum over groups of max_sfb)` of a CPE for the budget block.
fn ms_mask_shape(ff: &crate::portable::aac::encoder::front::FrontFrame, c0: usize) -> (bool, i32) {
    let p = &ff.channels[c0].psy;
    let present = ms_mask_present(&p.flags, p.ngroups as usize, p.nsfb_per_group as usize, p.max_sfb as usize);
    (present == MS_MASK_PER_BAND, (p.ngroups * p.max_sfb) as i32)
}

/// `samples` with every subnormal replaced by a zero of the same sign, in `out` (what FPCR.FZ / MXCSR DAZ make of subnormal operands). Integer
/// arithmetic on the bit pattern only: the result does not depend on the flush mode of the thread, and NaNs, infinities and normal numbers pass.
fn flush_subnormals(samples: &[f32], out: &mut Vec<f32>) {
    out.clear();
    out.extend(samples.iter().map(|&x| {
        let bits = x.to_bits();
        if bits & 0x7f80_0000 == 0 { f32::from_bits(bits & 0x8000_0000) } else { x }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::aac::fb::input::au_count;

    /// Deterministic interleaved test signal: a tone per channel plus a little noise.
    fn signal(frames: usize, channels: usize) -> Vec<f32> {
        let mut s = 0x2545_f491u32;
        (0..frames * channels)
            .map(|i| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let (frame, c) = (i / channels, i % channels);
                0.25 * (frame as f32 * (0.02 + 0.003 * c as f32)).sin() + 0.01 * ((s >> 9) as f32 / 8_388_608.0 - 1.0)
            })
            .collect()
    }

    fn encode(rate: u32, channels: u32, pcm: &[f32], piece_frames: usize) -> Vec<Vec<u8>> {
        let mut enc = AacEncoder::new(rate, channels).unwrap();
        for piece in pcm.chunks(piece_frames * channels as usize) {
            enc.push_interleaved(piece).unwrap();
        }
        enc.finish().unwrap();
        enc.take_access_units()
    }

    #[test]
    fn the_encoder_can_move_between_threads() {
        fn send<T: Send>() {}
        send::<AacEncoder>();
        let mut enc = AacEncoder::new(44100, 2).unwrap();
        let pcm = signal(3000, 2);
        let units = std::thread::spawn(move || {
            enc.push_interleaved(&pcm).unwrap();
            enc.finish().unwrap();
            enc.take_access_units().len()
        })
        .join()
        .unwrap();
        assert_eq!(units as u64, au_count(3000));
    }

    #[test]
    fn the_unit_count_follows_the_frame_count_and_pushes_of_any_size_agree() {
        let pcm = signal(5000, 2);
        let whole = encode(44100, 2, &pcm, 5000);
        assert_eq!(whole.len() as u64, au_count(5000));
        for piece in [1usize, 7, 512, 1025, 4999] {
            assert_eq!(encode(44100, 2, &pcm, piece), whole, "pushes of {piece} frames");
        }
    }

    #[test]
    fn no_frame_gives_no_access_unit() {
        let mut enc = AacEncoder::new(48000, 6).unwrap();
        assert_eq!(enc.frames_pushed(), 0);
        enc.push_interleaved(&[]).unwrap();
        enc.finish().unwrap();
        assert_eq!(enc.pending_access_units(), 0);
        assert!(enc.take_access_units().is_empty());
    }

    #[test]
    fn draining_after_every_push_keeps_the_queue_short() {
        let pcm = signal(40_000, 2);
        let mut enc = AacEncoder::new(44100, 2).unwrap();
        let (mut drained, mut peak) = (Vec::new(), 0);
        for piece in pcm.chunks(512 * 2) {
            enc.push_interleaved(piece).unwrap();
            peak = peak.max(enc.pending_access_units());
            enc.drain_access_units(|au| -> Result<(), ()> {
                drained.push(au.to_vec());
                Ok(())
            })
            .unwrap();
            assert_eq!(enc.pending_access_units(), 0);
        }
        enc.finish().unwrap();
        enc.drain_access_units(|au| -> Result<(), ()> {
            drained.push(au.to_vec());
            Ok(())
        })
        .unwrap();
        assert!(peak <= 1, "a 512-frame push completes at most one 1024-frame block");
        assert_eq!(drained, encode(44100, 2, &pcm, 512));
    }

    #[test]
    fn a_failing_sink_leaves_its_unit_and_the_later_ones_queued() {
        let pcm = signal(6000, 1);
        let mut enc = AacEncoder::new(44100, 1).unwrap();
        enc.push_interleaved(&pcm).unwrap();
        enc.finish().unwrap();
        let total = enc.pending_access_units();
        assert!(total >= 4);
        let mut seen = 0;
        let result = enc.drain_access_units(|_| {
            seen += 1;
            if seen == 3 { Err("full") } else { Ok(()) }
        });
        assert_eq!(result, Err("full"));
        assert_eq!(enc.pending_access_units(), total - 2, "two units went out, the third stays");
        let rest = enc.take_access_units();
        assert_eq!(rest.len(), total - 2);
    }

    #[test]
    fn misuse_is_a_typed_error_not_a_panic() {
        let mut enc = AacEncoder::new(44100, 2).unwrap();
        assert!(matches!(enc.push_interleaved(&[0.0; 3]), Err(EncodeError::PartialFrame)));
        assert_eq!(enc.frames_pushed(), 0, "a refused push changes nothing");
        enc.push_interleaved(&[0.0; 4]).unwrap();
        enc.finish().unwrap();
        enc.finish().unwrap();
        assert!(matches!(enc.push_interleaved(&[0.0; 4]), Err(EncodeError::Finished)));
        assert_eq!(enc.frames_pushed(), 2);
        // a finished stream refuses every push, an empty one and a partial frame included, with the same error
        assert!(matches!(enc.push_interleaved(&[]), Err(EncodeError::Finished)));
        assert!(matches!(enc.push_interleaved(&[0.0; 3]), Err(EncodeError::Finished)));
        for error in [EncodeError::Finished, EncodeError::PartialFrame, EncodeError::Failed, EncodeError::BudgetMismatch { au_index: 3, written: 5, budgeted: 6 }] {
            assert!(!error.to_string().is_empty());
        }
    }

    /// A failed block ends the stream: a retry of `finish` after a failed `finish` must not report a complete stream, and nothing can be pushed.
    #[test]
    fn a_failed_block_poisons_the_encoder_and_a_retried_finish_is_an_error() {
        let pcm = signal(2500, 2); // two complete blocks are pushed, the rest and the flush blocks come with `finish`: au_count(2500) = 5 access units
        assert_eq!(au_count(2500), 5);
        // fails while pushing (access unit 1)
        let mut enc = AacEncoder::new(44100, 2).unwrap();
        enc.fail_at_au = Some(1);
        assert!(matches!(enc.push_interleaved(&pcm), Err(EncodeError::BudgetMismatch { au_index: 1, .. })));
        assert!(matches!(enc.push_interleaved(&[]), Err(EncodeError::Failed)));
        assert!(matches!(enc.push_interleaved(&[0.0; 3]), Err(EncodeError::Failed)));
        assert!(matches!(enc.finish(), Err(EncodeError::Failed)));
        // fails in the flush blocks of `finish`; the second `finish` is an error and the stream stays short
        let mut enc = AacEncoder::new(44100, 2).unwrap();
        enc.fail_at_au = Some(4);
        enc.push_interleaved(&pcm).unwrap();
        assert!(matches!(enc.finish(), Err(EncodeError::BudgetMismatch { au_index: 4, .. })));
        assert!(enc.pending_access_units() < 5);
        assert!(matches!(enc.finish(), Err(EncodeError::Failed)));
        assert!(matches!(enc.push_interleaved(&[]), Err(EncodeError::Failed)));
        // the good path is untouched by the hook being absent
        let mut enc = AacEncoder::new(44100, 2).unwrap();
        enc.push_interleaved(&pcm).unwrap();
        enc.finish().unwrap();
        assert_eq!(enc.pending_access_units(), 5);
    }

    #[test]
    fn the_set_up_runs_in_the_callers_flush_mode_without_changing_it() {
        let pcm = signal(3000, 2);
        let plain = encode(44100, 2, &pcm, 512);
        crate::fp_mode::with_flush_to_zero(|| {
            let mut enc = AacEncoder::new(44100, 2).unwrap();
            assert!(crate::fp_mode::flush_to_zero_enabled(), "the caller's mode is still set after the set-up");
            for piece in pcm.chunks(512 * 2) {
                enc.push_interleaved(piece).unwrap();
            }
            enc.finish().unwrap();
            assert_eq!(enc.take_access_units(), plain);
        });
    }

    #[test]
    fn the_encoder_refuses_what_audiotoolbox_refuses() {
        assert!(matches!(AacEncoder::new(44100, 0), Err(ConfigError::Unsupported { .. })));
        assert!(matches!(AacEncoder::new(44100, 9), Err(ConfigError::Unsupported { .. })));
        assert!(matches!(AacEncoder::new(32000, 2), Err(ConfigError::Refused { .. })));
        assert!(matches!(AacEncoder::new(44101, 2), Err(ConfigError::NotReproduced { .. })));
        assert!(matches!(AacEncoder::new(7350, 2), Err(ConfigError::Unsupported { .. })));
    }

    #[test]
    fn the_constructor_agrees_with_accepts_on_the_whole_domain_and_never_panics() {
        use crate::portable::aac::encoder::accepts;
        let rates = [0, 1, 7350, 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44099, 44100, 44101, 48000, 64000, 88200, 96000, 192_000, u32::MAX];
        for rate in rates {
            for channels in 0..=64 {
                match (AacEncoder::new(rate, channels), accepts(rate, channels)) {
                    (Ok(_), Ok(())) => {}
                    (Err(got), Err(want)) => assert_eq!(got, want, "{rate} Hz, {channels} channels"),
                    (got, want) => panic!("{rate} Hz, {channels} channels: constructor {:?}, accepts {want:?}", got.err()),
                }
            }
        }
    }

    #[test]
    fn subnormals_become_zeros_of_the_same_sign_and_nothing_else_changes() {
        let input = [
            f32::from_bits(1), f32::from_bits(0x8000_0001), f32::from_bits(0x007f_ffff), f32::from_bits(0x807f_ffff), // subnormals
            0.0, -0.0, f32::MIN_POSITIVE, -f32::MIN_POSITIVE, 1.0e-30, -0.5, f32::INFINITY, f32::NEG_INFINITY, f32::from_bits(0x7fc0_0001), f32::from_bits(0xffa0_0000),
        ];
        let mut out = Vec::new();
        for mode in [false, true] {
            // the helper is integer code: the thread's flush mode cannot change it
            let run = |out: &mut Vec<f32>| flush_subnormals(&input, out);
            if mode { crate::fp_mode::with_flush_to_zero(|| run(&mut out)) } else { run(&mut out) }
            let bits: Vec<u32> = out.iter().map(|x| x.to_bits()).collect();
            let mut want: Vec<u32> = input.iter().map(|x| x.to_bits()).collect();
            want[..4].copy_from_slice(&[0, 0x8000_0000, 0, 0x8000_0000]);
            assert_eq!(bits, want, "flush mode {mode}");
        }
    }

    #[test]
    fn ordinary_input_does_not_depend_on_the_flush_to_zero_mode_of_the_caller() {
        let pcm = signal(3000, 2);
        let plain = encode(44100, 2, &pcm, 512);
        let flushed = crate::fp_mode::with_flush_to_zero(|| encode(44100, 2, &pcm, 512));
        assert_eq!(plain, flushed);
    }

    /// The content of the fixtures' `subnormal` items: cycles of 4096 frames, 2048 random subnormal patterns, 1024 of 1e-30 noise, 1024 of a 1e-3 tone.
    fn subnormal_signal(frames: usize, channels: usize) -> Vec<f32> {
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32
        };
        let mut out = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            for _ in 0..channels {
                let phase = frame % 4096;
                out.push(match phase {
                    0..2048 => f32::from_bits((next() & 0x007f_ffff) | (next() & 0x8000_0000)),
                    2048..3072 => 1.0e-30 * ((next() >> 8) as f32 / 8_388_608.0 - 1.0),
                    _ => 1.0e-3 * (phase as f32 * 0.05).sin(),
                });
            }
        }
        out
    }

    #[test]
    fn a_caller_in_flush_mode_gets_apples_flushed_input() {
        let raw = subnormal_signal(12_288, 2);
        assert!(raw.iter().any(|x| x.is_subnormal()));
        let mut flushed = Vec::new();
        flush_subnormals(&raw, &mut flushed);
        assert!(!flushed.iter().any(|x| x.is_subnormal()));
        let unflushed_file = encode(44100, 2, &raw, 512);
        let in_mode = crate::fp_mode::with_flush_to_zero(|| encode(44100, 2, &raw, 512));
        // the mode is what changes the stream, and it changes it exactly as pre-flushed input does, whatever the push size
        assert_ne!(in_mode, unflushed_file, "subnormal input is flushed when the caller's mode is set");
        assert_eq!(in_mode, encode(44100, 2, &flushed, 512));
        assert_eq!(crate::fp_mode::with_flush_to_zero(|| encode(44100, 2, &raw, 7)), in_mode);
        assert_eq!(crate::fp_mode::with_flush_to_zero(|| encode(44100, 2, &raw, 20_000)), in_mode);
        // clear mode: the samples are used as they are
        assert_eq!(encode(44100, 2, &flushed, 512), in_mode);
        assert_eq!(encode(44100, 2, &raw, 1025), unflushed_file);
    }

    #[test]
    fn the_mode_is_read_at_each_push_and_restored_after_it() {
        let raw = subnormal_signal(8192, 2);
        let mut flushed = Vec::new();
        flush_subnormals(&raw, &mut flushed);
        let (head, tail) = raw.split_at(4096 * 2);
        let mut enc = AacEncoder::new(44100, 2).unwrap();
        crate::fp_mode::with_flush_to_zero(|| {
            enc.push_interleaved(head).unwrap();
            assert!(crate::fp_mode::flush_to_zero_enabled(), "the caller's mode is still set after the push");
        });
        enc.push_interleaved(tail).unwrap();
        assert!(!crate::fp_mode::flush_to_zero_enabled());
        enc.finish().unwrap();
        // the first push was made in flush mode, the second was not
        let mut mixed = flushed[..4096 * 2].to_vec();
        mixed.extend_from_slice(tail);
        assert_eq!(enc.take_access_units(), encode(44100, 2, &mixed, 4096));
    }
}
