//! [`AacDecoder`]: `ExtAudioFileOpenURL` + client format + `ExtAudioFileRead` of an AAC-LC file, as AudioToolbox delivers it (macOS 27.2, arm64).
//!
//! Call semantics (the verified model of Apple's `ExtAudioFileRead`, see [`AacDecoder::read_interleaved`]):
//! * the stream a client reads is `decoded[skip ..]` with `skip = min(priming, T)`; the frames a call returns are the first
//!   `min(requested, stream frames left, valid frames left)` of it, whatever the read sizes are;
//! * a call asks the converter for the whole request, not only for the frames that are valid: the decoded frames behind the returned count (the
//!   padding of the last packets) are written into the client buffer too, up to the request or the end of the decodable frames;
//! * an access unit the decoder rejects yields 0 frames, resets the decoder (synthesis state, tag adoption) and counts towards the run of rejected
//!   units; the 4th consecutive rejection fails the call with `'bada'`; an oversize / out-of-file access unit fails it with -50;
//! * reads after the last frame return 0.

use crate::fp_mode::without_flush_to_zero;
use crate::portable::aac::decoder::au::{spectral_config, AuDecoder};
use crate::portable::aac::decoder::elements::element_sequence;
use crate::portable::aac::decoder::error::{DecodeError, ReadFailure, Unsupported};
use crate::portable::aac::decoder::file::asc::sfb_table_index;
use crate::portable::aac::decoder::file::{AacFile, ClientConverter, ClientError, FileType, Layout, OpenError, ReadError};
use crate::portable::aac::decoder::synth::output::OUTPUT_SCALE;
use crate::portable::aac::decoder::synth::FRAME;

/// The 4th consecutive access unit the decoder rejects fails the call that decodes it (`'bada'`).
const MAX_CONSECUTIVE_BAD_AUS: usize = 4;

/// `kExtAudioFileProperty_IOBufferSizeBytes` default: the converter pulls at most this many bytes of packets (by their upper bound) at once.
const IO_BUFFER_BYTES: usize = 65_536;

/// Frames of one `ExtAudioFileRead` call: how many the caller gets back and how many the call wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Read {
    /// The count `ExtAudioFileRead` returns (valid frames).
    pub frames: usize,
    /// Frames written into the client buffer, `>= frames`: the decoded padding behind the valid frames follows them.
    pub written: usize,
}

/// An opened AAC-LC file with a client format, read like `ExtAudioFileRead`.  Owns the file image; `Send`; instances are independent (no shared
/// mutable state), so any number can decode concurrently.
pub struct AacDecoder {
    file: AacFile,
    core: AuDecoder,
    layout: &'static Layout,
    conv: ClientConverter,
    /// Decoded frames still to discard (the priming).
    skip_left: u64,
    /// Valid frames not yet returned to the client (`u64::MAX`: the packet table declares no valid count).
    valid_left: u64,
    /// Next packet the converter pulls from the file, the packets it holds (pulled, not yet decoded), whether the file ended (latched), and the
    /// run of consecutive packets the decoder rejected.
    nxt: usize,
    held: usize,
    eof: bool,
    run: usize,
    /// Client planes of the current access unit (client scale), `conv.out_channels()` of them.
    stage: Vec<[f32; FRAME]>,
    /// Part of `stage` not written yet: `[off, end)`.
    off: usize,
    end: usize,
    reads: u64,
    /// a planar client format was refused: the converter is left without a client format and every read fails
    no_client_format: bool,
}

/// ADTS only: the first access unit must parse completely and agree with the header's `channel_configuration` (`ExtAudioFileOpenURL` fails `'fmt?'`
/// otherwise).  An SBR extension payload in it makes AudioToolbox treat the stream as HE-AAC: a payload without a header cannot start the SBR decoder
/// (`'fmt?'`, as for every payload this check cannot tell from a corrupt one), one with a header is refused.
fn adts_first_au_check(file: &AacFile) -> Result<(), DecodeError> {
    if file.file_type() != FileType::Adts {
        return Ok(());
    }
    let Ok(au) = file.packet(0) else { return Ok(()) };
    match element_sequence(au, sfb_table_index(file.sample_rate())) {
        Some(shape) => {
            file.check_adts_elements(&shape.elements)?;
            match shape.sbr {
                None => Ok(()),
                Some(false) => Err(OpenError::UnsupportedDataFormat.into()),
                Some(true) => Err(DecodeError::Unsupported(Unsupported::HeAac)),
            }
        }
        // an access unit that does not parse up to its `ID_END` fails the open (every corrupted first access unit that was tried; no intact one)
        None => Err(OpenError::UnsupportedDataFormat.into()),
    }
}

impl AacDecoder {
    /// Open `bytes` (the whole file image; `extension` = file-name extension without the dot, which AudioFile uses as a type hint) and set the
    /// client format "float32 interleaved at the source channel count".
    ///
    /// Errors follow Apple's order: container ([`DecodeError::Open`]), then the client format ([`DecodeError::Client`]); streams Apple decodes but
    /// this port does not are [`DecodeError::Unsupported`].
    pub fn open(bytes: Vec<u8>, extension: Option<&str>) -> Result<Self, DecodeError> {
        without_flush_to_zero(|| Self::open_inner(bytes, extension))
    }

    fn open_inner(bytes: Vec<u8>, extension: Option<&str>) -> Result<Self, DecodeError> {
        let file = AacFile::parse_with_hint(bytes, extension)?;
        adts_first_au_check(&file)?;
        let cfg = file.decoder_config().map_err(|e| DecodeError::client(&file, e))?;
        let layout = file.layout().ok_or(DecodeError::Client(ClientError::NotAacLc))?;
        let conv = ClientConverter::identity(usize::from(layout.channels));
        let spectral = spectral_config(&cfg).map_err(DecodeError::Config)?;
        let core = AuDecoder::new(spectral);
        if core.channels() != usize::from(layout.channels) {
            // the file layer reports another channel count than the element list produces (CAF `desc` vs ASC mismatch that Apple resolves by downmixing)
            return Err(DecodeError::Unsupported(Unsupported::ChannelCountMismatch));
        }
        let plan = file.trim_plan();
        let stage = vec![[0.0; FRAME]; conv.out_channels()];
        Ok(AacDecoder {
            core,
            layout,
            conv,
            skip_left: plan.skip_frames(),
            valid_left: if plan.valid_unknown { u64::MAX } else { plan.valid_frames },
            nxt: 0,
            held: 0,
            eof: false,
            run: 0,
            stage,
            off: 0,
            end: 0,
            reads: 0,
            no_client_format: false,
            file,
        })
    }

    /// `ExtAudioFileSetProperty(ClientDataFormat)` with two non-interleaved float32 planes (the engine's processing-AU read): the stream is mixed
    /// to stereo (identity for a stereo source, duplicated for mono).  Only allowed before the first read.
    pub fn set_stereo_planar(&mut self) -> Result<(), DecodeError> {
        let conv = match self.stereo_planar_converter() {
            Ok(conv) => conv,
            Err(error) => {
                self.note_refusal(&error);
                return Err(error);
            }
        };
        self.stage = vec![[0.0; FRAME]; conv.out_channels()];
        self.conv = conv;
        Ok(())
    }

    /// What [`set_stereo_planar`](Self::set_stereo_planar) would answer, without changing the client format (a refusal still costs the handle its client
    /// format, as it does on AudioToolbox).
    pub fn check_stereo_planar(&mut self) -> Result<(), DecodeError> {
        match self.stereo_planar_converter() {
            Ok(_) => Ok(()),
            Err(error) => {
                self.note_refusal(&error);
                Err(error)
            }
        }
    }

    /// AudioToolbox refused the client format (rather than this port): the handle keeps no usable client format.
    fn note_refusal(&mut self, error: &DecodeError) {
        self.no_client_format |= matches!(error, DecodeError::Client(e) if e.os_status().is_some());
    }

    fn stereo_planar_converter(&self) -> Result<ClientConverter, DecodeError> {
        if self.reads != 0 {
            return Err(DecodeError::Unsupported(Unsupported::ClientFormatChangeAfterRead));
        }
        ClientConverter::new(self.layout, 2).map_err(|e| DecodeError::client(&self.file, e))
    }

    // ---- file facts ---------------------------------------------------------------------------------------------------------------

    /// The opened container (`FileDataFormat`, layout tag, packet table facts, `trim_plan()`, ...).
    pub fn file(&self) -> &AacFile {
        &self.file
    }
    /// `kExtAudioFileProperty_FileLengthFrames`.
    pub fn file_length_frames(&self) -> u64 {
        self.file.file_length_frames()
    }
    /// Client channels (interleaved channels / planes).
    #[cfg(test)]
    pub fn client_channels(&self) -> usize {
        self.conv.out_channels()
    }

    // ---- reading ------------------------------------------------------------------------------------------------------------------
    /// `ExtAudioFileRead` into one interleaved buffer: up to `frames` frames, as many as `out` holds (`out.len() / client_channels()`).  Returns the valid
    /// frames delivered (0 = end of file) and the frames written (the decoded padding follows the valid frames; nothing behind `written` is touched).
    /// A request for no frame (or a buffer without room for one) returns 0 at once and changes nothing, as the readers of the other codecs do.
    pub fn read_interleaved(&mut self, frames: usize, out: &mut [f32]) -> Result<Read, ReadFailure> {
        let ch = self.stage.len();
        let frames = frames.min(out.len() / ch);
        if frames == 0 {
            return Ok(Read { frames: 0, written: 0 });
        }
        without_flush_to_zero(|| {
            self.read_loop(frames, |planes, off, take, done| {
                let dst = &mut out[done * ch..(done + take) * ch];
                match planes {
                    [a] => dst.copy_from_slice(&a[off..off + take]),
                    [a, b] => {
                        for ((d, &x), &y) in dst.chunks_exact_mut(2).zip(&a[off..off + take]).zip(&b[off..off + take]) {
                            d[0] = x;
                            d[1] = y;
                        }
                    }
                    _ => {
                        for t in 0..take {
                            for (c, p) in planes.iter().enumerate() {
                                dst[t * ch + c] = p[off + t];
                            }
                        }
                    }
                }
            })
        })
    }

    /// `ExtAudioFileRead` into two planes (after [`set_stereo_planar`](Self::set_stereo_planar), or from a stereo source): up to `frames` frames, as
    /// many as the shorter plane holds.  A request for no frame returns 0 at once and changes nothing.
    pub fn read_stereo_planar(&mut self, frames: usize, left: &mut [f32], right: &mut [f32]) -> Result<Read, ReadFailure> {
        if self.stage.len() != 2 {
            return Err(ReadError::NoClientFormat.into()); // the client format is not two planes
        }
        let frames = frames.min(left.len()).min(right.len());
        if frames == 0 {
            return Ok(Read { frames: 0, written: 0 });
        }
        without_flush_to_zero(|| {
            self.read_loop(frames, |planes, off, take, done| {
                left[done..done + take].copy_from_slice(&planes[0][off..off + take]);
                right[done..done + take].copy_from_slice(&planes[1][off..off + take]);
            })
        })
    }

    /// One `ExtAudioFileRead` of `want > 0` frames; `emit(planes, offset, count, already written)` stores the next `count` frames.
    fn read_loop(&mut self, want: usize, mut emit: impl FnMut(&[[f32; FRAME]], usize, usize, usize)) -> Result<Read, ReadFailure> {
        if self.file.multi_block_at() == Some(0) {
            return Err(ReadError::InvalidFile.into());
        }
        if self.no_client_format {
            return Err(ReadError::NoClientFormat.into());
        }
        if self.file.packet_count() == 0 {
            return if self.file.empty_stream() { Ok(Read { frames: 0, written: 0 }) } else { Err(ReadError::NoPackets.into()) };
        }
        if self.file.packet_size_upper_bound() == 0 {
            // every packet of the table is empty: the converter cannot tell the largest one (macOS 27.2: -66567, as for a table without packets)
            return Err(ReadError::NoPackets.into());
        }
        if self.file.frames_per_packet() == 0 {
            // a CAF whose `desc` says no frames per packet: the converter cannot be fed
            return Err(ReadError::BadData.into());
        }
        self.reads += 1;
        // the first call discards the priming: `want` frames at a time, decoded into the caller's buffer like any other frames
        while self.skip_left > 0 {
            let chunk = (want as u64).min(self.skip_left) as usize;
            let (got, failure) = self.request(chunk, true, &mut emit);
            if let Some(e) = failure {
                return Err(e);
            }
            self.skip_left -= got as u64;
            if got < chunk {
                self.skip_left = 0; // the priming is longer than the stream
            }
        }
        let (produced, failure) = self.request(want, false, &mut emit);
        if let Some(e) = failure {
            return Err(e);
        }
        let frames = (produced as u64).min(self.valid_left) as usize;
        self.valid_left = self.valid_left.saturating_sub(frames as u64);
        Ok(Read { frames, written: produced })
    }

    /// One request of the converter for `want` decoded frames; returns the frames written (also by a request that failed) and its failure.
    ///
    /// The planar stereo client of a source with 1, 3, 4 or 5 channels (a channel selection, no mixing) runs it in inner requests of at most
    /// 4096 frames and loses a failure that arrives after an inner request produced frames (the failed batch is skipped, the stream goes on);
    /// every other client passes the request on as it is.
    fn request(&mut self, want: usize, chunk: bool, emit: &mut impl FnMut(&[[f32; FRAME]], usize, usize, usize)) -> (usize, Option<ReadFailure>) {
        if self.conv.is_identity() || !matches!(self.layout.channels, 1 | 3 | 4 | 5) {
            return self.fill(want, chunk, want, emit);
        }
        let mut got = 0usize;
        while got < want {
            let inner = (want - got).min(4096);
            let base = got;
            let (produced, failure) = self.fill(inner, chunk, want, &mut |planes, off, take, done| emit(planes, off, take, base + done));
            got += produced;
            match failure {
                // what Apple shows of this error after an earlier failure is not reproduced
                Some(ReadFailure::Apple(ReadError::InvalidFile)) => return (got, Some(ReadFailure::NotReproduced(Unsupported::MultiBlockAdtsPlanar))),
                Some(e) if produced == 0 => return (got, Some(e)),
                // what the calls that follow a failure swallowed inside the priming skip see is not reproduced
                Some(_) if chunk => return (got, Some(ReadFailure::NotReproduced(Unsupported::PrimingFailureSwallowed))),
                Some(_) => continue,
                None if produced < inner => break,
                None => {}
            }
        }
        (got, None)
    }

    /// Packets per pull of a request of `want` frames: `ceil(want / 1024)` for the priming chunks, `floor(want / 1024) + 1` for a read, at most
    /// as many as fit the 64 KiB I/O buffer of the converter.
    fn batch(&self, want: usize, chunk: bool) -> usize {
        let wanted = if chunk { want.div_ceil(FRAME) } else { want / FRAME + 1 };
        let upper = self.file.packet_size_upper_bound() as usize;
        let cap = if upper == 0 { usize::MAX } else { (IO_BUFFER_BYTES / upper).max(1) };
        wanted.min(cap)
    }

    /// Writes the next `want` decoded frames of the stream (fewer at its end) with `emit`; packets are pulled `batch` at a time when the
    /// decoder has none left and decoded one at a time.  `outer` is the size of the whole request the batch size derives from.
    fn fill(&mut self, want: usize, chunk: bool, outer: usize, emit: &mut impl FnMut(&[[f32; FRAME]], usize, usize, usize)) -> (usize, Option<ReadFailure>) {
        let batch = self.batch(outer, chunk);
        let mut produced = 0usize;
        while produced < want {
            if self.off < self.end {
                let take = (want - produced).min(self.end - self.off);
                emit(&self.stage, self.off, take, produced);
                self.off += take;
                produced += take;
                continue;
            }
            if self.held == 0 {
                if self.eof {
                    break;
                }
                let (n, failure) = self.file.pull(self.nxt, batch);
                if let Some(e) = failure {
                    // the failed batch is consumed
                    self.nxt = (self.nxt + batch).min(self.file.packet_count());
                    return (produced, Some(e.into()));
                }
                if n == 0 {
                    self.eof = true;
                    break;
                }
                self.held = n;
                self.nxt += n;
            }
            let index = self.nxt - self.held;
            self.held -= 1;
            let au = match self.file.packet(index) {
                Ok(au) => au,
                Err(e) => return (produced, Some(e.into())),
            };
            let decoded = self.core.decode_block(au);
            if self.conv.applies_matrix_mixdown() && self.core.pce_unreproduced() {
                return (produced, Some(ReadFailure::NotReproduced(Unsupported::InBandPce)));
            }
            if decoded {
                self.run = 0;
                self.stage_block();
                self.off = 0;
                self.end = FRAME;
            } else {
                self.run += 1;
                if self.run >= MAX_CONSECUTIVE_BAD_AUS {
                    return (produced, Some(ReadError::BadData.into()));
                }
            }
        }
        (produced, None)
    }

    /// Time blocks (int16 domain) -> client planes: channel conversion first (Apple's `DownMixWithCoefficients` runs before the output stage), then
    /// `x * 2^-15`.
    fn stage_block(&mut self) {
        let src = self.core.block();
        if self.conv.is_identity() {
            for (o, s) in self.stage.iter_mut().zip(src) {
                for (d, &x) in o.iter_mut().zip(s) {
                    *d = x * OUTPUT_SCALE;
                }
            }
        } else {
            let ins: Vec<&[f32]> = src.iter().map(|p| p.as_slice()).collect();
            let (l, r) = self.stage.split_at_mut(1);
            self.conv.convert(&ins, &mut [&mut l[0][..], &mut r[0][..]], self.core.matrix_mixdown());
            for o in self.stage.iter_mut() {
                o.iter_mut().for_each(|x| *x *= OUTPUT_SCALE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fp_mode::with_flush_to_zero;
    use crate::portable::aac::decoder::testutil::*;

    /// Reads `file` to the end in `block`-frame calls; the returned frames, the frames written per call, and the error that ended it.
    fn read_all(file: &[u8], hint: Option<&str>, block: usize, planar: bool) -> (Vec<f32>, Vec<usize>, Option<ReadFailure>) {
        let mut dec = AacDecoder::open(file.to_vec(), hint).expect("opens");
        if planar {
            dec.set_stereo_planar().expect("planar");
        }
        let (mut out, mut written) = (Vec::new(), Vec::new());
        let (mut l, mut r) = (vec![0f32; block], vec![0f32; block]);
        let mut i = vec![0f32; block * dec.client_channels()];
        loop {
            let res = if planar { dec.read_stereo_planar(block, &mut l, &mut r) } else { dec.read_interleaved(block, &mut i) };
            match res {
                Ok(read) if read.frames == 0 => return (out, written, None),
                Ok(read) => {
                    written.push(read.written);
                    if planar {
                        out.extend((0..read.frames).flat_map(|k| [l[k], r[k]]));
                    } else {
                        out.extend_from_slice(&i[..read.frames * dec.client_channels()]);
                    }
                }
                Err(e) => return (out, written, Some(e)),
            }
        }
    }

    /// The 5.1 stream as ADTS (`channel_configuration` 6, 32 kHz) with unit 3 carrying `pce` in front of its first element, if any.
    fn five_one(pce: Option<Vec<u8>>) -> Vec<u8> {
        let mut aus = six_channel_aus();
        if let Some(unit) = pce {
            aus[3] = unit;
        }
        adts(&aus, 5, 6)
    }

    fn bits_of(samples: &[f32]) -> Vec<u32> {
        samples.iter().map(|x| x.to_bits()).collect()
    }

    /// An in-band PCE with a matrix mix-down index changes the stereo mix-down of a 5.1 stream (AudioToolbox, macOS 27.2: recorded by the `rule/pcemix-*` fixtures);
    /// one that is the stream's own layout is mixed, one that is not (a damaged stream's) is refused on the stereo read, and neither touches the interleaved read.
    #[test]
    fn an_in_band_pce_with_a_matrix_index_is_mixed_when_well_formed_and_refused_when_not() {
        let aus = six_channel_aus();
        let (plain_stereo, _, error) = read_all(&five_one(None), Some("aac"), 512, true);
        assert!(error.is_none() && plain_stereo.len() == 17 * 1024 * 2, "{} samples, {error:?}", plain_stereo.len());
        let (plain_interleaved, _, error) = read_all(&five_one(None), Some("aac"), 512, false);
        assert!(error.is_none());

        // the stream's own layout: front SCE + CPE, back CPE, one LFE
        let well_formed = with_pce(&aus[3], &[(false, 0), (true, 0)], 0, &[(true, 1)], 1, Some((3, 0)));
        let (stereo, _, error) = read_all(&five_one(Some(well_formed.clone())), Some("aac"), 512, true);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(stereo.len(), plain_stereo.len());
        let from = 3 * 1024 * 2;
        assert_eq!(bits_of(&stereo[..from]), bits_of(&plain_stereo[..from]), "the units before the PCE are mixed as before");
        assert_ne!(bits_of(&stereo[from..]), bits_of(&plain_stereo[from..]), "the index changes the mix-down from the unit it is in");
        // (the ISO matrix of index 3 is (L + c C) / (1 + c) without the surround: the left plane of that unit is that mix of the interleaved planes)
        let (interleaved, _, error) = read_all(&five_one(Some(well_formed)), Some("aac"), 512, false);
        assert!(error.is_none());
        assert_eq!(bits_of(&interleaved), bits_of(&plain_interleaved), "the interleaved read does not depend on the PCE");
        let c = std::f32::consts::FRAC_1_SQRT_2;
        let norm = 1.0 / (1.0 + c);
        for t in 0..1024 {
            let at = 3 * 1024 + t;
            let (centre, left) = (interleaved[at * 6], interleaved[at * 6 + 1]);
            let expected = norm * (left + c * centre);
            assert!((stereo[2 * at] - expected).abs() <= 1e-6 * (1.0 + expected.abs()), "left plane at frame {at}: {} against {expected}", stereo[2 * at]);
        }

        // lists that are not the stream's own: refused on the stereo read (the units before it are delivered), decoded on the interleaved read
        for (name, pce) in [
            ("1 front, 2 back", with_pce(&aus[3], &[(false, 0)], 0, &[(true, 1), (true, 2)], 1, Some((1, 0)))),
            ("a side element", with_pce(&aus[3], &[(false, 0), (true, 0)], 1, &[(true, 1)], 1, Some((0, 0)))),
            ("no lists at all", with_pce(&aus[3], &[], 0, &[], 0, Some((2, 1)))),
            ("CPE then SCE in front", with_pce(&aus[3], &[(true, 0), (false, 0)], 0, &[(true, 1)], 1, Some((3, 0)))),
        ] {
            let file = five_one(Some(pce));
            let (stereo, _, error) = read_all(&file, Some("aac"), 512, true);
            assert!(matches!(error, Some(ReadFailure::NotReproduced(Unsupported::InBandPce))), "{name}: {error:?}");
            assert_eq!(bits_of(&stereo), bits_of(&plain_stereo[..from]), "{name}: the units in front of the refusal are delivered as before");
            let (interleaved, _, error) = read_all(&file, Some("aac"), 512, false);
            assert!(error.is_none(), "{name}: {error:?}");
            assert_eq!(bits_of(&interleaved), bits_of(&plain_interleaved), "{name}");
        }
        // a PCE of any shape without an index never refuses
        let (stereo, _, error) = read_all(&five_one(Some(with_pce(&aus[3], &[(false, 0)], 3, &[], 0, None))), Some("aac"), 512, true);
        assert!(error.is_none() && bits_of(&stereo) == bits_of(&plain_stereo), "{error:?}");
    }

    #[test]
    fn the_delivered_stream_does_not_depend_on_the_read_size() {
        for (file, hint) in [(STEREO_M4A, "m4a"), (MONO_M4A, "m4a"), (STEREO_ADTS, "aac"), (STEREO_CAF, "caf")] {
            let (reference, _, error) = read_all(file, Some(hint), 512, false);
            assert!(error.is_none() && !reference.is_empty());
            for block in [1usize, 7, 333, 1024, 1025, 4096, 65_536] {
                let (samples, _, error) = read_all(file, Some(hint), block, false);
                assert!(error.is_none(), "{hint} block {block}");
                assert_eq!(samples.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), reference.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), "{hint} block {block}");
            }
        }
    }

    #[test]
    fn planar_stereo_equals_interleaved_for_a_stereo_source_and_duplicates_mono() {
        let (interleaved, _, _) = read_all(STEREO_M4A, Some("m4a"), 512, false);
        let (planar, _, _) = read_all(STEREO_M4A, Some("m4a"), 333, true);
        assert_eq!(interleaved.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), planar.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
        let (mono, _, _) = read_all(MONO_M4A, Some("m4a"), 512, false);
        let (duplicated, _, _) = read_all(MONO_M4A, Some("m4a"), 512, true);
        assert_eq!(duplicated.len(), 2 * mono.len());
        assert!(duplicated.chunks_exact(2).zip(&mono).all(|(pair, m)| pair[0].to_bits() == m.to_bits() && pair[1].to_bits() == m.to_bits()));
    }

    #[test]
    fn frame_counts_and_padding_follow_the_packet_table() {
        let dec = AacDecoder::open(STEREO_M4A.to_vec(), Some("m4a")).unwrap();
        assert_eq!(dec.file_length_frames(), 8820);
        assert_eq!((dec.file().packet_count(), dec.file().trim_plan().priming, dec.file().trim_plan().remainder), (11, 2112, 332));
        // the last read returns the valid frames and writes the decoded padding (the remainder) behind them
        let (samples, written, _) = read_all(STEREO_M4A, Some("m4a"), 512, false);
        assert_eq!(samples.len(), 8820 * 2);
        assert_eq!(written.len(), 18);
        assert_eq!((written[16], written[17]), (512, 116 + 332));
    }

    #[test]
    fn the_flush_to_zero_mode_of_the_caller_does_not_change_the_result() {
        let (plain, _, _) = read_all(STEREO_M4A, Some("m4a"), 512, false);
        let (guarded, _, _) = with_flush_to_zero(|| read_all(STEREO_M4A, Some("m4a"), 512, false));
        assert_eq!(plain.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), guarded.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
    }

    #[test]
    fn decoders_are_send_and_independent() {
        fn assert_send<T: Send>() {}
        assert_send::<AacDecoder>();
        let handles: Vec<_> = (0..4).map(|_| std::thread::spawn(|| read_all(STEREO_M4A, Some("m4a"), 512, false).0.len())).collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), 8820 * 2);
        }
    }

    #[test]
    fn an_oversize_access_unit_fails_the_call_that_needs_it() {
        let aus = access_units(STEREO_M4A);
        let refs: Vec<&[u8]> = aus.iter().map(|a| a.as_slice()).collect();
        let mut sizes: Vec<u32> = aus.iter().map(|a| a.len() as u32).collect();
        sizes[5] = 2049;
        let mut padded = aus.clone();
        padded[5].resize(2049, 0);
        let padded_refs: Vec<&[u8]> = padded.iter().map(|a| a.as_slice()).collect();
        let file = mp4_with(&padded_refs, &Mp4Parts { sizes, constant: None, asc: vec![0x12, 0x10], entry: *b"mp4a" });
        let (samples, _, error) = read_all(&file, Some("m4a"), 512, false);
        assert_eq!(error, Some(ReadFailure::Apple(ReadError::PacketTooLarge)));
        assert!(!samples.is_empty() && samples.len() < 8820 * 2);
        let _ = refs;
    }

    #[test]
    fn a_file_without_packets_fails_the_first_read() {
        let file = mp4(&[]);
        let mut dec = AacDecoder::open(file, Some("m4a")).unwrap();
        let mut out = [0f32; 1024];
        assert_eq!(dec.read_interleaved(512, &mut out), Err(ReadFailure::Apple(ReadError::NoPackets)));
        assert_eq!(dec.file().bitrate(), 0);
    }

    #[test]
    fn he_aac_and_low_delay_streams_are_refused_where_apple_decodes_them() {
        let aus = access_units(STEREO_M4A);
        let refs: Vec<&[u8]> = aus.iter().map(|a| a.as_slice()).collect();
        let open = |asc: &[u8]| {
            let file = mp4_with(&refs, &Mp4Parts { sizes: aus.iter().map(|a| a.len() as u32).collect(), constant: None, asc: asc.to_vec(), entry: *b"mp4a" });
            AacDecoder::open(file, Some("m4a")).err()
        };
        // AOT 2, 22.05 kHz core, stereo, explicit backward-compatible SBR signalling (extension type 0x2b7, AOT 5, sfi 4)
        assert_eq!(open(&[0x13, 0x90, 0x56, 0xe5, 0xa0]), Some(DecodeError::Unsupported(Unsupported::HeAac)));
        // AOT 23 (ER AAC LD) and 39 (ER AAC ELD)
        assert_eq!(open(&[0xb9, 0x90]), Some(DecodeError::Unsupported(Unsupported::LowDelayAac)));
        assert_eq!(open(&[0xf8, 0xe8, 0x40, 0x00]), Some(DecodeError::Unsupported(Unsupported::LowDelayAac)));
        // AOT 1 (Main) is refused by AudioToolbox itself at the client format: 'fmt?'
        assert!(matches!(open(&[0x0a, 0x10]), Some(DecodeError::Client(ClientError::NotAacLc))));
    }

    /// AudioToolbox's answer to a CAF whose `desc` channel count and `chan` layout tag disagree with the stream (rows: with / without `kuki`, `desc`
    /// channels 1, 2, 3, 6; columns: tags of 0, 1, 2, 3, 6, 8 channels). `o` opens, `L` fails -50 (the layout names more channels than allowed: more than
    /// two and not the `desc` count), `D` fails '!dat' (more `desc` channels than the ASC has), `R` is what the port refuses (the reference down-mixes).
    #[test]
    fn caf_channel_layouts_follow_audiotoolbox() {
        let tags = [0u32, (100 << 16) | 1, (101 << 16) | 2, (114 << 16) | 3, (124 << 16) | 6, (127 << 16) | 8];
        let grid = [
            (false, 1, "RRRLLL"), (false, 2, "oooLLL"), (false, 3, "DDDDDD"), (false, 6, "DDDDDD"),
            (true, 1, "oooLLL"), (true, 2, "oooLLL"), (true, 3, "ooooLL"), (true, 6, "oooLoL"),
        ];
        for (no_kuki, channels, row) in grid {
            for (tag, expected) in tags.iter().zip(row.chars()) {
                let result = AacDecoder::open(caf_variant(no_kuki, channels, *tag), Some("caf"));
                let got = match result {
                    Ok(_) => 'o',
                    Err(DecodeError::Client(ClientError::InvalidLayout)) => 'L',
                    Err(DecodeError::Client(ClientError::NoDecoder)) => 'D',
                    Err(DecodeError::Unsupported(Unsupported::ChannelCountMismatch)) => 'R',
                    Err(other) => panic!("{other}"),
                };
                assert_eq!(got, expected, "kuki dropped {no_kuki}, {channels} channels, tag {tag:#x}");
            }
        }
    }

    /// What AudioToolbox does with the `desc` chunk of a CAF whose other chunks are intact (recorded: bits and bytes per packet, frames per packet,
    /// channel counts, format flags).
    #[test]
    fn caf_desc_fields_decide_the_open_and_the_client_format() {
        let desc = |edit: &dyn Fn(&mut Vec<u8>)| caf_edited(|chunks| edit(&mut chunks.iter_mut().find(|c| &c.0 == b"desc").unwrap().1));
        let open = |bytes: Vec<u8>| AacDecoder::open(bytes, Some("caf")).err();
        let field = |at: usize, value: u32| desc(&|d: &mut Vec<u8>| d[at..at + 4].copy_from_slice(&value.to_be_bytes()));
        // mFormatFlags 12, mBytesPerPacket 16, mFramesPerPacket 20, mChannelsPerFrame 24, mBitsPerChannel 28
        assert!(open(field(12, 0xffff_ffff)).is_none(), "the flags are reported, not checked");
        assert!(AacDecoder::open(field(12, 5), None).unwrap().file().format_flags() == 5);
        assert!(matches!(open(field(28, 16)), Some(DecodeError::Client(ClientError::NotAacLc))));
        assert!(matches!(open(field(16, 100)), Some(DecodeError::Open(OpenError::InvalidChunk))), "a packet size behind a packet table");
        assert!(matches!(open(field(20, 2048)), Some(DecodeError::Client(ClientError::NotAacLc))));
        assert!(matches!(open(field(20, 999_999)), Some(DecodeError::Client(ClientError::NotAacLc))));
        assert!(matches!(open(field(20, 1_000_000)), Some(DecodeError::Client(ClientError::InvalidPacketFrames))));
        assert!(matches!(open(field(24, 0)), Some(DecodeError::Open(OpenError::UnsupportedDataFormat))));
        assert!(matches!(open(field(24, 9)), Some(DecodeError::Client(ClientError::UnsupportedChannelCount))));
        assert!(matches!(open(field(24, 3)), Some(DecodeError::Client(ClientError::NoDecoder))), "more channels than the stream has");
    }

    #[test]
    fn a_caf_without_frames_per_packet_opens_and_cannot_be_read() {
        let mut dec = AacDecoder::open(caf_edited(|chunks| chunks.iter_mut().find(|c| &c.0 == b"desc").unwrap().1[20..24].fill(0)), Some("caf")).unwrap();
        assert_eq!((dec.file().frames_per_packet(), dec.file_length_frames(), dec.file().packet_count()), (0, 8820, 11));
        let mut out = vec![0f32; 1024];
        for _ in 0..2 {
            assert!(matches!(dec.read_interleaved(512, &mut out), Err(ReadFailure::Apple(ReadError::BadData))));
        }
        // without a packet table nothing tells the size of the largest packet: the client format fails
        let no_table = caf_edited(|chunks| {
            chunks.iter_mut().find(|c| &c.0 == b"desc").unwrap().1[20..24].fill(0);
            chunks.retain(|c| &c.0 != b"pakt");
        });
        assert!(matches!(AacDecoder::open(no_table, Some("caf")).err(), Some(DecodeError::Client(ClientError::NoPacketTable))));
        // the list of a (size, frames) table: an even count of numbers serves half as many packets, an odd count any number
        let table = |numbers: usize, packets: i64| {
            caf_edited(|chunks| {
                chunks.iter_mut().find(|c| &c.0 == b"desc").unwrap().1[20..24].fill(0);
                let pakt = &mut chunks.iter_mut().find(|c| &c.0 == b"pakt").unwrap().1;
                pakt.truncate(24);
                pakt[0..8].copy_from_slice(&packets.to_be_bytes());
                pakt.extend(std::iter::repeat_n(6u8, numbers));
            })
        };
        for (numbers, packets, opens) in [(12, 6, true), (12, 7, false), (11, 11, true), (11, 13, true), (2, 2, false), (2, 1, true)] {
            let got = AacDecoder::open(table(numbers, packets), Some("caf")).err();
            assert!(if opens { got.is_none() } else { matches!(got, Some(DecodeError::Open(OpenError::InvalidChunk))) }, "{numbers} numbers, {packets} packets");
        }
        // the bit rate divides by the frames of the pairs the table announces: the packets' second numbers (6 each), not there for a short list
        let bitrate = |numbers, packets| {
            let f = AacDecoder::open(table(numbers, packets), Some("caf")).unwrap();
            f.file().bitrate_defined().then(|| f.file().bitrate())
        };
        for (numbers, packets, frames) in [(12, 6, 36u64), (11, 5, 30)] {
            let got = u64::from(bitrate(numbers, packets).unwrap());
            assert!(got.abs_diff(3009 * 8 * 44100 / frames) <= 1, "{got}");
        }
        assert_eq!(bitrate(11, 6), None);
        assert_eq!(bitrate(11, 11), None);
    }

    /// A CAF cut inside its chunk list, or with a `data` chunk that does not fit its packet table (recorded on macOS 27.2).
    #[test]
    fn caf_chunks_that_run_past_the_end_are_ignored_and_a_short_data_chunk_is_refused() {
        // desc 8..52, kuki 52..103, chan 103..127, info 127..202, pakt 202..259, free 259..4080, data 4080..
        let cut = |n: usize| AacDecoder::open(STEREO_CAF[..n].to_vec(), Some("caf"));
        for n in [52, 100, 115, 126, 150, 258] {
            let dec = cut(n).unwrap_or_else(|e| panic!("{n}: {e}"));
            assert_eq!((dec.file().packet_count(), dec.file_length_frames(), dec.file().bitrate()), (0, 0, 0), "cut at {n}");
        }
        for n in [7, 8, 20, 51] {
            assert!(matches!(cut(n).err(), Some(DecodeError::Open(OpenError::InvalidFile))), "cut at {n}");
        }
        for n in [259, 270, 1000, 4080, 4091] {
            assert!(matches!(cut(n).err(), Some(DecodeError::Open(OpenError::InvalidChunk))), "a packet table without data, cut at {n}");
        }
        for n in [4092, 4093, 4095] {
            assert_eq!(cut(n).unwrap().file().packet_count(), 0, "a data chunk without its edit count, cut at {n}");
        }
        for n in [4096, 4100, 5000, STEREO_CAF.len()] {
            assert_eq!(cut(n).unwrap().file().packet_count(), 11, "cut at {n}");
        }
        // a complete data chunk must hold the start of the last packet (2771 bytes in, behind the 4-byte edit count)
        let data_of = |size: usize| {
            AacDecoder::open(caf_edited(|chunks| {
                chunks.retain(|c| &c.0 != b"free");
                chunks.last_mut().unwrap().1.truncate(size);
            }), Some("caf"))
        };
        assert!(matches!(data_of(2774).err(), Some(DecodeError::Open(OpenError::InvalidChunk))));
        assert_eq!(data_of(0).unwrap().file().packet_count(), 0, "a data chunk that ends the file without its edit count");
        let short = data_of(2775).unwrap();
        assert_eq!((short.file().packet_count(), short.file().audio_data_bytes()), (11, 2771));
    }

    /// The packets of a complete `data` chunk that is shorter than the packet table: a request that needs the incomplete packet fails the read
    /// (-50), a larger one ends the stream at the last complete packet; the same with other chunks behind the data chunk (recorded on macOS 27.2).
    #[test]
    fn the_last_packets_of_a_short_data_chunk_fail_the_read_that_needs_them() {
        let with_data = |size: usize, trailer: bool| {
            caf_edited(|chunks| {
                chunks.retain(|c| &c.0 != b"free");
                chunks.last_mut().unwrap().1.truncate(size);
                if trailer {
                    chunks.push((*b"free", vec![0; 8]));
                }
            })
        };
        for (size, trailer) in [(2775, false), (3000, false), (3012, true), (3000, true)] {
            let file = with_data(size, trailer);
            let (samples, _, failed) = read_all(&file, Some("caf"), 1024, false);
            assert_eq!((samples.len() / 2, failed), (6144, Some(ReadFailure::Apple(ReadError::PacketBeyondEof))), "{size} {trailer}");
            let (samples, _, failed) = read_all(&file, Some("caf"), 4096, false);
            assert_eq!((samples.len() / 2, failed), (8128, None), "{size} {trailer}");
        }
    }

    /// A LOAS file whose first frame runs past the end of the file opens with no packets when the frame's StreamMuxConfig and payload length are
    /// there (10 bytes of the fixture), and fails `'fmt?'` when they are not (recorded on macOS 27.2).
    #[test]
    fn a_loas_stream_whose_first_frame_is_cut_opens_empty() {
        let open = |data: Vec<u8>| AacDecoder::open(data, Some("loas"));
        for n in [3, 5, 8, 9] {
            assert!(matches!(open(STEREO_LOAS[..n].to_vec()).err(), Some(DecodeError::Open(OpenError::UnsupportedDataFormat))), "{n} bytes");
        }
        let mut longer = STEREO_LOAS.to_vec();
        longer[1] |= 0x1f;
        longer[2] = 0xff;
        for data in [STEREO_LOAS[..10].to_vec(), STEREO_LOAS[..20].to_vec(), longer] {
            let mut dec = open(data).unwrap();
            assert_eq!((dec.file().packet_count(), dec.file_length_frames(), dec.file().bitrate()), (0, 0, 0));
            let mut out = vec![0f32; 1024];
            assert!(matches!(dec.read_interleaved(512, &mut out), Ok(read) if read.frames == 0));
        }
    }

    /// A CAF cookie that is no ES descriptor chain (or whose lengths do not hold) opens and fails the client format, like a cookie no decoder
    /// can be built from.
    #[test]
    fn an_unreadable_caf_cookie_fails_the_client_format() {
        let cookie = |edit: &dyn Fn(&mut Vec<u8>)| AacDecoder::open(caf_edited(|c| edit(&mut c.iter_mut().find(|c| &c.0 == b"kuki").unwrap().1)), Some("caf")).err();
        assert!(cookie(&|_| {}).is_none());
        // the tag, a length that runs past the chain, the ASC length, an ASC that is not one
        for (at, value) in [(0, 0x04), (4, 0xdd), (12, 0xeb), (30, 0x7f), (31, 0xed)] {
            assert!(matches!(cookie(&|k| k[at] = value), Some(DecodeError::Client(ClientError::NoDecoder))), "byte {at}");
        }
        assert!(matches!(cookie(&|k| k.clear()), Some(DecodeError::Client(ClientError::NoDecoder))));
    }

    #[test]
    fn adts_headers_after_the_first_end_the_stream_when_they_cannot_be_frames() {
        let frames = |edit: &dyn Fn(&mut [u8])| {
            let mut file = STEREO_ADTS.to_vec();
            let mut pos = 0;
            let mut index = 0;
            while pos + 7 <= file.len() {
                let len = (usize::from(file[pos + 3] & 3) << 11) | (usize::from(file[pos + 4]) << 3) | usize::from(file[pos + 5] >> 5);
                if index == 5 {
                    edit(&mut file[pos..pos + 7]);
                }
                index += 1;
                pos += len;
            }
            AacFile::parse_with_hint(file, Some("aac")).map(|f| f.packet_count())
        };
        assert_eq!(frames(&|_| {}), Ok(11));
        assert_eq!(frames(&|h| h[2] |= 0xc0), Ok(5), "profile 3");
        assert_eq!(frames(&|h| h[2] = (h[2] & 0xc3) | (13 << 2)), Ok(5), "sampling index 13");
        assert_eq!(frames(&|h| h[2] = (h[2] & 0xc3) | (12 << 2)), Ok(11), "sampling index 12 is a rate, only for the first frame an error");
        assert_eq!(frames(&|h| h[1] &= !1), Ok(5), "CRC protection behind unprotected frames");
        assert_eq!(frames(&|h| h[1] ^= 8), Ok(11), "the ID bit is free");
        // the first frame: the MPEG-2 header has no profile 3, the MPEG-4 one reads it as an object type that is not LC
        let first = |id: u8| {
            let mut file = STEREO_ADTS.to_vec();
            file[1] = 0xf1 | id;
            file[2] |= 0xc0;
            AacFile::parse_with_hint(file, Some("aac")).map(|_| ()).unwrap_err()
        };
        assert_eq!(first(8), OpenError::UnsupportedProfile);
        assert_eq!(first(0), OpenError::UnsupportedDataFormat);
    }

    /// The header size of an ADTS stream is its first frame's: a protected first frame makes every access unit start nine bytes in.
    #[test]
    fn adts_access_units_start_behind_the_header_size_of_the_first_frame() {
        let aus = access_units(STEREO_ADTS);
        let mut protected = Vec::new();
        for (i, au) in aus.iter().enumerate() {
            let mut frame = adts(std::slice::from_ref(au), 4, 2);
            if i == 0 {
                // CRC present: header of 9 bytes, frame length 2 longer
                let len = frame.len() + 2;
                frame[1] = 0xf0;
                frame[3] = (frame[3] & 0xfc) | (len >> 11) as u8;
                frame[4] = (len >> 3) as u8;
                frame[5] = (frame[5] & 0x1f) | ((len & 7) << 5) as u8;
                frame.splice(7..7, [0, 0]);
            }
            protected.extend(frame);
        }
        let file = AacFile::parse_with_hint(protected, Some("aac")).unwrap();
        assert_eq!(file.packet_count(), 11);
        assert_eq!(file.packet(0).unwrap(), &aus[0][..]);
        assert_eq!(file.packet(1).unwrap(), &aus[1][2..], "an unprotected frame behind a protected one is cut at the first frame's header size");
    }

    /// An SBR extension payload in the first access unit of an ADTS stream (recorded: any other position, type or later access unit is ignored).
    #[test]
    fn an_sbr_extension_in_the_first_adts_access_unit_is_refused_or_fails_fmt() {
        let aus = access_units(STEREO_ADTS);
        let stream = |fills: &[(usize, &str)]| {
            let aus: Vec<Vec<u8>> = aus.iter().enumerate().map(|(i, au)| match fills.iter().find(|f| f.0 == i) {
                Some((_, bits)) => with_fill_element(au, bits),
                None => au.clone(),
            }).collect();
            AacDecoder::open(adts(&aus, 4, 2), Some("aac")).err()
        };
        assert!(matches!(stream(&[(0, "1101100000000000")]), Some(DecodeError::Unsupported(Unsupported::HeAac))));
        assert!(matches!(stream(&[(0, "1110000000000010")]), Some(DecodeError::Unsupported(Unsupported::HeAac))), "with the CRC of type 14");
        assert!(matches!(stream(&[(0, "1101000000000000")]), Some(DecodeError::Open(OpenError::UnsupportedDataFormat))), "no header: it cannot start the SBR decoder");
        assert!(stream(&[(0, "1011000000000000")]).is_none());
        assert!(stream(&[(1, "1101100000000000"), (5, "1101100000000000")]).is_none());
    }

    #[test]
    fn a_refused_planar_client_format_leaves_the_handle_without_a_client_format() {
        // AAC_6_0 by program config element (front SCE + CPE, side CPE, back SCE): AudioToolbox has no stereo downmix for it, refuses the planar
        // format and then fails every read of the handle with -50
        let asc_bits = "00010 0100 0000 000  0000 01 0100 0010 0001 0001 00 000 0000 0 0 0  0 0000 1 0000  1 0001  0 0001  00  00000000";
        let asc: Vec<u8> = {
            let bits: Vec<char> = asc_bits.chars().filter(|c| !c.is_whitespace()).collect();
            bits.chunks(8).map(|byte| byte.iter().fold(0u8, |v, c| v << 1 | u8::from(*c == '1'))).collect()
        };
        let file = caf_edited(|chunks| {
            chunks.iter_mut().find(|c| &c.0 == b"desc").unwrap().1[24..28].copy_from_slice(&6u32.to_be_bytes());
            chunks.iter_mut().find(|c| &c.0 == b"kuki").unwrap().1 = esds(&asc)[12..].to_vec();
            chunks.retain(|c| &c.0 != b"chan");
        });
        let mut dec = AacDecoder::open(file, Some("caf")).unwrap();
        assert_eq!(dec.client_channels(), 6);
        assert!(matches!(dec.set_stereo_planar(), Err(DecodeError::Client(ClientError::NoDecoder))));
        let mut out = vec![0f32; 512 * dec.client_channels()];
        for _ in 0..2 {
            assert!(matches!(dec.read_interleaved(512, &mut out), Err(ReadFailure::Apple(ReadError::NoClientFormat))));
        }
        // a stereo source takes the planar format, and a refusal that is only this port's leaves the handle alone
        let mut stereo = AacDecoder::open(STEREO_CAF.to_vec(), Some("caf")).unwrap();
        stereo.set_stereo_planar().unwrap();
        assert!(stereo.read_stereo_planar(512, &mut [0.0; 512], &mut [0.0; 512]).is_ok());
    }

    #[test]
    fn open_errors_carry_apples_statuses() {
        let status = |bytes: Vec<u8>, hint: Option<&str>| match AacDecoder::open(bytes, hint) {
            Err(DecodeError::Open(e)) => e.os_status(),
            other => panic!("unexpected {:?}", other.err()),
        };
        assert_eq!(status(Vec::new(), None), i32::from_be_bytes(*b"wht?"));
        assert_eq!(status(vec![1, 2, 3, 4, 5, 6, 7, 8, 9], None), i32::from_be_bytes(*b"typ?"));
        assert_eq!(status(STEREO_M4A.to_vec(), Some("aac")), i32::from_be_bytes(*b"sync"));
        assert_eq!(status(STEREO_M4A.to_vec(), Some("loas")), i32::from_be_bytes(*b"fmt?"));
        assert_eq!(status(vec![0xff, 0xfb, 0x90, 0x00, 0, 0, 0, 0], Some("aac")), i32::from_be_bytes(*b"layr"));
    }

    /// A request for no frame (or a buffer that cannot hold one) used to loop for ever in the priming skip of a fresh decoder, and a buffer shorter
    /// than the request used to panic.  Now both return 0 at once and change nothing; a short buffer reads as many frames as it holds.
    #[test]
    fn requests_the_buffers_cannot_hold_are_clamped_not_panicked_on_or_looped() {
        for (file, hint) in [(STEREO_M4A, "m4a"), (MONO_M4A, "m4a"), (STEREO_ADTS, "aac"), (STEREO_CAF, "caf")] {
            let (reference, _, error) = read_all(file, Some(hint), 512, false);
            assert!(error.is_none() && !reference.is_empty());
            let mut dec = AacDecoder::open(file.to_vec(), Some(hint)).unwrap();
            let ch = dec.client_channels();
            let mut out = vec![0f32; 512 * ch];
            let none = Read { frames: 0, written: 0 };
            assert_eq!(dec.read_interleaved(0, &mut out).unwrap(), none, "{hint}");
            assert_eq!(dec.read_interleaved(512, &mut []).unwrap(), none, "{hint}");
            assert_eq!(dec.read_interleaved(512, &mut out[..ch - 1]).unwrap(), none, "{hint}: no room for a frame");
            // a buffer of 100 frames for a request of 512: 100 frames, and the stream goes on where it left off
            let first = dec.read_interleaved(512, &mut out[..100 * ch]).unwrap();
            assert_eq!((first.frames, first.written), (100, 100), "{hint}");
            let mut stream: Vec<f32> = out[..100 * ch].to_vec();
            loop {
                let read = dec.read_interleaved(512, &mut out).unwrap();
                if read.frames == 0 {
                    break;
                }
                stream.extend_from_slice(&out[..read.frames * ch]);
            }
            assert_eq!(stream.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), reference.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), "{hint}");

            let mut planar = AacDecoder::open(file.to_vec(), Some(hint)).unwrap();
            planar.set_stereo_planar().unwrap();
            let (mut l, mut r) = (vec![0f32; 512], vec![0f32; 512]);
            assert_eq!(planar.read_stereo_planar(0, &mut l, &mut r).unwrap(), none, "{hint}");
            assert_eq!(planar.read_stereo_planar(512, &mut l[..0], &mut r).unwrap(), none, "{hint}");
            assert_eq!(planar.read_stereo_planar(512, &mut l, &mut r[..7]).unwrap().frames, 7, "{hint}");
        }
    }

    #[test]
    fn a_planar_read_of_a_handle_that_is_not_two_planes_fails_like_a_missing_client_format() {
        let file = caf_edited(|chunks| {
            for (kind, body) in chunks.iter_mut() {
                if &kind[..] == b"desc" {
                    body[24..28].copy_from_slice(&6u32.to_be_bytes());
                }
            }
            chunks.retain(|c| &c.0 != b"chan");
        });
        // a channel count the stream does not have is refused or not reproduced; only a six-channel handle reaches the read
        if let Ok(mut dec) = AacDecoder::open(file, Some("caf")) {
            assert_eq!(dec.client_channels(), 6);
            assert!(matches!(dec.read_stereo_planar(512, &mut [0.0; 512], &mut [0.0; 512]), Err(ReadFailure::Apple(ReadError::NoClientFormat))));
        }
    }
}
