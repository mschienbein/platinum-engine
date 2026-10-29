//! AudioCodec-level behaviour of Apple's `ACMP3Decoder` (AudioCodecs.component, subtype `.mp3`, Layer III),
//! everything that sits OUTSIDE `MP3DecoderWrapper_SpiritDSP::DecodeFrame`.
//!
//! This is a transcription of (addresses are file addresses of the arm64e slice of
//! `/System/Library/Components/AudioCodecs.component/Contents/MacOS/AudioCodecs`, macOS 27.2):
//!
//! | Rust                                   | Apple                                                               |
//! |----------------------------------------|---------------------------------------------------------------------|
//! | `Mp3Codec::new`                        | `ausdk::APFactory<AudioCodecLookup,ACMP3Decoder>::Construct` 0x56714 |
//! | `Mp3Codec::initialize`                 | `ACMP3Decoder::Initialize` 0x54c48 + `ACMP3Codec::Initialize` 0x545b0 |
//! | `Mp3Codec::set_current_input_format`   | `ACMP3Decoder::SetCurrentInputFormat` 0x5531c                       |
//! | `Mp3Codec::set_current_output_format`  | `ACMP3Decoder::SetCurrentOutputFormat` 0x55670                      |
//! | `Mp3Codec::set_magic_cookie`           | `ACMP3Codec::SetMagicCookie` 0x546ec                                |
//! | `Mp3Codec::append_input_data`          | `ACMP3Decoder::AppendInputData` 0x5597c, `MP3DecoderWrapper::Fill` 0x13fae0 |
//! | `Mp3Codec::produce_output_packets`     | `ACMP3Decoder::ProduceOutputPackets` 0x55b54                        |
//! | `Mp3Codec::reset` / `uninitialize`     | `ACMP3Decoder::Reset` 0x56488 / `Uninitialize` 0x56454              |
//! | `Wrapper`                              | `MP3DecoderWrapper_SpiritDSP` object (ring buffer, `Reset` 0x13d700) |
//!
//! The bit-exact decode of one frame (`DecodeFrame` 0x13d768, Spirit DSP) is behind the [`FrameDecoder`] trait.
//!
//! Output buffers are `&mut [i16]` (the codec only produces 16-bit signed interleaved PCM). Like Apple's, every
//! operation works IN PLACE: bytes of the caller's buffer that the codec does not write keep their value.
//!
//! Conventions: Apple throws `int`/`OSStatus` exceptions that `AudioCodecLookup` turns into the returned `OSStatus`;
//! here they are `Err(CodecError(code))`. In/out parameters (`io_bytes`, `io_packets`) are `&mut u32` because Apple's
//! error paths modify some of them before throwing (documented per method).

use core::fmt;

// ---------------------------------------------------------------------------------------------------------------------
// Constants

/// `'!stt'` kAudioCodecStateError (wrong initialized/uninitialized state).
pub const ERR_STATE: i32 = 0x2173_7474;
/// `'!dat'` kAudioCodecBadDataError.
pub const ERR_DATA: i32 = 0x2164_6174;
/// `'!buf'` kAudioCodecNotEnoughBufferSpaceError.
pub const ERR_BUF: i32 = 0x2162_7566;
/// `'!siz'` kAudioCodecBadPropertySizeError.
pub const ERR_SIZE: i32 = 0x2173_697a;
/// `'nope'` kAudioCodecIllegalOperationError (read-only property).
pub const ERR_NOPE: i32 = 0x6e6f_7065;
/// `'who?'` kAudioCodecUnknownPropertyError.
pub const ERR_WHO: i32 = 0x7768_6f3f;
/// Returned for exceptions that are not `int` (ProduceOutputPackets with a too small buffer).
pub const ERR_UNSPECIFIED: i32 = -1;
/// paramErr (negative `mStartOffset` in a packet description).
pub const ERR_PARAM: i32 = -50;

/// AudioCodecProduceOutputPacketStatus values.
pub const STATUS_FAILURE: u32 = 1;
pub const STATUS_SUCCESS: u32 = 2;
pub const STATUS_SUCCESS_HAS_MORE: u32 = 3;
pub const STATUS_NEEDS_MORE_INPUT: u32 = 4;

/// The `OSStatus` the `AudioCodecProduceOutputPackets` shim returns for an `outStatus`: `STATUS_FAILURE` becomes paramErr (-50)
/// (`outStatus`, `ioNumberPackets` = 0 and `ioOutputDataByteSize` = 0 are still written), every other status is noErr.
pub const fn produce_os_status(out_status: u32) -> i32 {
    if out_status == STATUS_FAILURE { ERR_PARAM } else { 0 }
}

/// `DecodeFrame` return codes (observed as the only ones: see NOTES.md).
pub const DECODE_OK: u32 = 0;
/// Insufficient input (also "no sync found"): nothing produced, `consumed` bytes of garbage may have been dropped.
pub const DECODE_NEED_MORE: u32 = 0x8101_0003;
/// The frame's PCM (`SpiritMP3GetLastFrameInfo` out bytes) does not fit the caller's capacity (LAB_0013dba0): ProduceOutputPackets fails.
pub const DECODE_OUT_TOO_SMALL: u32 = 0xc101_0003;
/// Concealed frame (uVar25 == 1 path, e.g. missing reservoir data): `*out_bytes == 0` and ProduceOutputPackets emits one zero packet.
pub const DECODE_SILENT: u32 = 0x4101_0002;
/// Decoder state corrupt: DecodeFrame reset itself.
pub const DECODE_RESET: u32 = 0x8101_0002;
/// Generic failure (null buffers / uVar25 in {5,6}).
pub const DECODE_FAIL: u32 = 0xc000_0001;

/// Size of the `MP3DecoderWrapper` input ring (`GetInputFree` = 0x2000 - used).
pub const RING_SIZE: usize = 0x2000;
/// Decoder delay in frames that `ACMP3Decoder` itself drops from the head of the stream and appends as zeros at EOF
/// (`+0x21c4`: 0x211 for Layer III, 0xf1 for Layer I/II).
pub const LAYER3_DECODER_DELAY: u32 = 0x211;
/// Layer I/II decoder delay (`0xf1`).
pub const LAYER12_DECODER_DELAY: u32 = 0xf1;

pub const FORMAT_MP1: u32 = u32::from_be_bytes(*b".mp1");
pub const FORMAT_MP2: u32 = u32::from_be_bytes(*b".mp2");
pub const FORMAT_MP3: u32 = u32::from_be_bytes(*b".mp3");
pub const FORMAT_LPCM: u32 = u32::from_be_bytes(*b"lpcm");
/// kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked.
pub const FLAGS_S16_PACKED: u32 = 0xc;

/// Layer III input format list (`kLayer3AvailableInputFormatList` 0x646ff8): (sample rate, base frames per packet).
pub const INPUT_RATES: [(f64, u32); 9] = [
    (8000.0, 576),
    (11025.0, 576),
    (12000.0, 576),
    (16000.0, 576),
    (22050.0, 576),
    (24000.0, 576),
    (32000.0, 1152),
    (44100.0, 1152),
    (48000.0, 1152),
];
/// Layer I input format list (`kLayer1AvailableInputFormatList` 0x647250).
pub const LAYER1_INPUT_RATES: [(f64, u32); 6] =
    [(16000.0, 384), (22050.0, 384), (24000.0, 384), (32000.0, 384), (44100.0, 384), (48000.0, 384)];
/// Layer II input format list (`kLayer2AvailableInputFormatList` 0x647160).
pub const LAYER2_INPUT_RATES: [(f64, u32); 6] =
    [(16000.0, 1152), (22050.0, 1152), (24000.0, 1152), (32000.0, 1152), (44100.0, 1152), (48000.0, 1152)];

/// The MPEG audio layer a component instance (`.mp1`, `.mp2`, `.mp3`) decodes. Only Layer III is verified by this project;
/// Layers I/II are included because `ACMP3Decoder` is the same class (`DecodeFrame` handles all three).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    I,
    II,
    III,
}

impl Layer {
    pub const fn format_id(self) -> u32 {
        match self {
            Layer::I => FORMAT_MP1,
            Layer::II => FORMAT_MP2,
            Layer::III => FORMAT_MP3,
        }
    }
    pub const fn decoder_delay(self) -> u32 {
        match self {
            Layer::III => LAYER3_DECODER_DELAY,
            _ => LAYER12_DECODER_DELAY,
        }
    }
    pub const fn input_rates(self) -> &'static [(f64, u32)] {
        match self {
            Layer::I => &LAYER1_INPUT_RATES,
            Layer::II => &LAYER2_INPUT_RATES,
            Layer::III => &INPUT_RATES,
        }
    }
    /// Frames per packet of the default input format (the 44100 Hz entry).
    const fn default_fpp(self) -> u32 {
        match self {
            Layer::I => 384,
            _ => 1152,
        }
    }
}

/// Output sample rates accepted by `SetCurrentOutputFormat` (constants at 0x923840.., 0x920ea0) plus 0.
const OUTPUT_RATES: [f64; 10] = [8000.0, 11025.0, 12000.0, 16000.0, 22050.0, 24000.0, 32000.0, 44100.0, 48000.0, 0.0];

// ---------------------------------------------------------------------------------------------------------------------
// Types

/// An `OSStatus` error code as returned by the AudioCodec API.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CodecError(pub i32);

impl fmt::Debug for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = (self.0 as u32).to_be_bytes();
        if b.iter().all(|c| (0x20..0x7f).contains(c)) {
            write!(f, "CodecError({} '{}')", self.0, String::from_utf8_lossy(&b))
        } else {
            write!(f, "CodecError({})", self.0)
        }
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for CodecError {}

/// `AudioStreamBasicDescription`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Asbd {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

impl Asbd {
    /// The `.mp3` input description AudioToolbox's converter passes to `AudioCodecInitialize`
    /// (`frames_per_packet` 1152 for MPEG-1, 576 for MPEG-2/2.5; `bytes_per_packet` 0 = variable).
    pub fn mp3(sample_rate: f64, channels: u32, frames_per_packet: u32, bytes_per_packet: u32) -> Asbd {
        Asbd {
            sample_rate,
            format_id: FORMAT_MP3,
            format_flags: 0,
            bytes_per_packet,
            frames_per_packet,
            bytes_per_frame: 0,
            channels_per_frame: channels,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// The 16-bit signed packed interleaved output description (the only output format the codec accepts).
    pub fn pcm_s16(sample_rate: f64, channels: u32) -> Asbd {
        Asbd {
            sample_rate,
            format_id: FORMAT_LPCM,
            format_flags: FLAGS_S16_PACKED,
            bytes_per_packet: 2 * channels,
            frames_per_packet: 1,
            bytes_per_frame: 2 * channels,
            channels_per_frame: channels,
            bits_per_channel: 16,
            reserved: 0,
        }
    }
}

/// `AudioStreamPacketDescription`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketDesc {
    pub start_offset: i64,
    pub variable_frames_in_packet: u32,
    pub data_byte_size: u32,
}

/// Result of one `DecodeFrame` call (see INTERFACE.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameResult {
    /// Exactly DecodeFrame's return value (`DECODE_*`).
    pub status: u32,
    /// Bytes DecodeFrame removes from the head of the ring (its local before the memmove at 0x13da98).
    pub consumed: usize,
    /// The `*param_4` DecodeFrame writes (PCM bytes of the decoded frame; 0 when it does not write it).
    pub out_bytes: u32,
}

/// The bit-exact `MP3DecoderWrapper_SpiritDSP::DecodeFrame` (0x13d768) + Spirit state.
pub trait FrameDecoder {
    /// Spirit-state half of `MP3DecoderWrapper_SpiritDSP::Reset` 0x13d700 (the wrapper clears `used` itself).
    fn reset(&mut self);
    /// `ring` is the WHOLE ring (stale bytes beyond `avail` included), `avail` = bytes in use (> 0),
    /// `out[..out_cap_bytes/2]` the caller's PCM buffer. On `DECODE_RESET` the implementation resets its state.
    fn decode_frame(&mut self, ring: &[u8; RING_SIZE], avail: usize, out: &mut [i16], out_cap_bytes: usize) -> FrameResult;
}

/// What one `produce_output_packets` call produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Produced {
    /// `outStatus` (`STATUS_*`).
    pub status: u32,
    /// `ioOutputDataByteSize` on return.
    pub bytes: u32,
    /// `ioNumberPackets` on return (PCM frames).
    pub packets: u32,
}

impl Produced {
    /// `OSStatus` of the API call (see [`produce_os_status`]).
    pub const fn os_status(&self) -> i32 {
        produce_os_status(self.status)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// MP3DecoderWrapper

struct Wrapper<D> {
    ring: Box<[u8; RING_SIZE]>,
    /// `this+8`.
    used: u32,
    dec: D,
}

impl<D: FrameDecoder + Default> Wrapper<D> {
    fn new() -> Self {
        // Apple's ring is uninitialized heap memory; zero is the only reproducible choice.
        let ring: Box<[u8; RING_SIZE]> = Box::new([0u8; RING_SIZE]);
        Wrapper { ring, used: 0, dec: D::default() }
    }

    /// `GetInputFree` 0x13fb68 (`int`).
    fn free(&self) -> i32 {
        RING_SIZE as i32 - self.used as i32
    }

    /// `MP3DecoderWrapper::Fill` 0x13fae0: returns the byte count appended, 0 when it did not fit.
    fn fill(&mut self, p: &[u8]) -> u32 {
        let n = p.len() as i32;
        if self.free() < n {
            return 0;
        }
        let u = self.used as usize;
        self.ring[u..u + p.len()].copy_from_slice(p);
        self.used += p.len() as u32;
        p.len() as u32
    }

    /// `MP3DecoderWrapper_SpiritDSP::Reset` 0x13d700.
    fn reset(&mut self) {
        self.dec.reset();
        self.used = 0;
    }

    /// `DecodeFrame(out, cap, &out_bytes, 0)`: returns (return code, `*param_4`). Applies the ring shift DecodeFrame does.
    fn decode_frame(&mut self, out: &mut [i16], out_cap: u32) -> (u32, u32) {
        if self.used == 0 {
            return (DECODE_NEED_MORE, 0);
        }
        let avail = self.used as usize;
        let cap = (out_cap as usize).min(out.len() * 2);
        let r = self.dec.decode_frame(&self.ring, avail, &mut out[..cap / 2], cap);
        // LAB_0013da98: `if consumed < used { if consumed != 0 { used -= consumed; memmove } } else { used = 0 }`
        if r.consumed < avail {
            if r.consumed != 0 {
                self.ring.copy_within(r.consumed..avail, 0);
                self.used = (avail - r.consumed) as u32;
            }
        } else {
            self.used = 0;
        }
        if r.status == DECODE_RESET {
            // `(**(vtable+0x10))(this)` = Reset: Spirit state cleared, used = 0.
            self.reset();
        }
        (r.status, r.out_bytes)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// ACMP3Decoder

/// `ACMP3Decoder` driven through the AudioCodec API (Layer III by default, see [`Layer`]).
pub struct Mp3Codec<D: FrameDecoder + Default> {
    layer: Layer,
    initialized: bool,
    in_fmt: Asbd,
    out_fmt: Asbd,
    cookie: [u8; 0x100],
    cookie_len: u32,
    /// `+0x1b0`: byte 0x22 of the `.mp3`/`ms\0U` cookie atom; 1 by default. >= 2 lets a "one more frame left" input produce `HAS_MORE`.
    cookie_flag: u32,
    /// `+0x21b4`: bytes appended minus bytes consumed by successful decodes (NOT corrected by failed/garbage decodes).
    used_input: u32,
    /// `+0x21c0`: frames still to drop from the head of the output (decoder delay + prime leading frames).
    skip_remaining: u32,
    /// `+0x21c4`: decoder delay (529).
    delay: u32,
    /// `+0x21c8`: `kAudioCodecPropertyPrimeInfo.leadingFrames`.
    prime_leading: u32,
    /// `+0x21cc`: zero frames still to emit after EOF.
    tail_remaining: u32,
    /// `+0x21d0`: an AppendInputData with 0 bytes or 0 packets was seen.
    eof_seen: bool,
    /// `+0x21b8` (None until the first Initialize; kept across Uninitialize).
    wrapper: Option<Wrapper<D>>,
}

impl<D: FrameDecoder + Default> Default for Mp3Codec<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: FrameDecoder + Default> Mp3Codec<D> {
    /// The freshly constructed, uninitialized component (defaults: 44100 Hz stereo, 1152 frames per packet, 16-bit PCM out).
    pub fn new() -> Self {
        Self::with_layer(Layer::III)
    }

    /// The component for `.mp1` / `.mp2` / `.mp3`.
    pub fn with_layer(layer: Layer) -> Self {
        let mut out_fmt = Asbd::pcm_s16(44100.0, 2);
        out_fmt.bytes_per_packet = 4;
        out_fmt.bytes_per_frame = 4;
        Mp3Codec {
            initialized: false,
            layer,
            in_fmt: Asbd { format_id: layer.format_id(), frames_per_packet: layer.default_fpp(), ..Asbd::mp3(44100.0, 2, 1152, 0) },
            out_fmt,
            cookie: [0; 0x100],
            cookie_len: 0,
            cookie_flag: 1,
            used_input: 0,
            skip_remaining: layer.decoder_delay(),
            delay: layer.decoder_delay(),
            prime_leading: 0,
            tail_remaining: layer.decoder_delay(),
            eof_seen: false,
            wrapper: None,
        }
    }

    /// Open the Layer III codec exactly like AudioToolbox's converter does for one stream:
    /// `Initialize(in, out)`, `Uninitialize`, `SetProperty('prim', {0,0})`, `Initialize(NULL, NULL)`, `Reset`
    /// (observed with lldb, see NOTES.md). `frames_per_packet` is 1152 (MPEG-1) or 576 (MPEG-2/2.5).
    pub fn open_like_converter(sample_rate: f64, channels: u32, frames_per_packet: u32) -> Result<Self, CodecError> {
        Self::open_like_converter_for(Layer::III, sample_rate, channels, frames_per_packet)
    }

    /// [`Self::open_like_converter`] for the component of `layer` (AudioFile reports `.mp1`/`.mp2`/`.mp3` as the data format ID
    /// from the first frame header, and AudioToolbox then instantiates that component).
    pub fn open_like_converter_for(layer: Layer, sample_rate: f64, channels: u32, frames_per_packet: u32) -> Result<Self, CodecError> {
        let mut c = Self::with_layer(layer);
        let input = Asbd { format_id: layer.format_id(), ..Asbd::mp3(sample_rate, channels, frames_per_packet, 0) };
        c.initialize(Some(&input), Some(&Asbd::pcm_s16(sample_rate, channels)), None)?;
        c.uninitialize();
        c.set_prime_info(0, 0)?;
        c.initialize(None, None, None)?;
        c.reset()?;
        Ok(c)
    }

    // ---- state / property accessors -------------------------------------------------------------------------------

    /// `kAudioCodecPropertyIsInitialized` 'init'.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// `kAudioCodecPropertyCurrentInputFormat` 'ifmt'.
    pub fn input_format(&self) -> Asbd {
        self.in_fmt
    }

    /// `kAudioCodecPropertyCurrentOutputFormat` 'ofmt'.
    pub fn output_format(&self) -> Asbd {
        self.out_fmt
    }

    /// `kAudioCodecPropertyPacketFrameSize` 'pakf' (`+0x44`: frames per packet of the INPUT format, 576 or 1152).
    pub fn packet_frame_size(&self) -> u32 {
        self.in_fmt.frames_per_packet
    }

    /// `kAudioCodecPropertyUsedInputBufferSize` 'ubuf' (`GetUsedInputBufferByteSize` 0x55e58).
    pub fn used_input_buffer_size(&self) -> u32 {
        self.used_input
    }

    /// `kAudioCodecPropertyInputBufferSize` 'tbuf' and `kAudioCodecPropertyMaximumPacketByteSize` 'pakb'.
    pub const fn input_buffer_size() -> u32 {
        RING_SIZE as u32
    }

    /// `kAudioCodecPropertyPrimeMethod` 'prmm' = kAudioCodecPrimeMethod_None.
    pub const fn prime_method() -> u32 {
        2
    }

    /// `kAudioCodecPropertyPrimeInfo` 'prim' (`{leadingFrames: +0x21c8, trailingFrames: 0}`).
    pub fn prime_info(&self) -> (u32, u32) {
        (self.prime_leading, 0)
    }

    /// `SetProperty('prim', 8 bytes)` (0x56314): only while uninitialized; `leading` frames are dropped in addition to the decoder delay.
    pub fn set_prime_info(&mut self, leading: u32, _trailing: u32) -> Result<(), CodecError> {
        if self.initialized {
            return Err(CodecError(ERR_STATE));
        }
        self.prime_leading = leading;
        self.skip_remaining = self.delay.wrapping_add(leading);
        self.tail_remaining = self.delay;
        Ok(())
    }

    /// `ACMP3Codec::GetMagicCookie` 0x54678: copies the stored cookie into `out`.
    pub fn magic_cookie(&self) -> &[u8] {
        &self.cookie[..self.cookie_len as usize]
    }

    // ---- formats ----------------------------------------------------------------------------------------------------

    /// `ACMP3Decoder::SetCurrentInputFormat` 0x5531c.
    pub fn set_current_input_format(&mut self, a: &Asbd) -> Result<(), CodecError> {
        if self.initialized {
            return Err(CodecError(ERR_STATE));
        }
        // List lookup by (sampleRate, formatID).
        let Some(&(_, base_fpp)) = self.layer.input_rates().iter().find(|(r, _)| *r == a.sample_rate && a.format_id == self.layer.format_id()) else {
            return Err(CodecError(ERR_DATA));
        };
        if a.frames_per_packet != 0 {
            let mult = a.frames_per_packet / base_fpp;
            let max_mult = if base_fpp == 0x240 { 8 } else { 4 };
            if a.frames_per_packet != mult * base_fpp || mult > max_mult {
                return Err(CodecError(ERR_DATA));
            }
        }
        if a.bytes_per_packet > 0x1200 || a.channels_per_frame > 2 {
            return Err(CodecError(ERR_DATA));
        }
        // ACBaseCodec::SetCurrentInputFormat(copy with framesPerPacket := list entry's) then the fix-ups.
        let mut f = *a;
        f.frames_per_packet = base_fpp;
        self.in_fmt = f;
        if f.channels_per_frame != 0 {
            self.out_fmt.channels_per_frame = f.channels_per_frame;
        } else {
            self.in_fmt.channels_per_frame = self.out_fmt.channels_per_frame;
        }
        self.out_fmt.sample_rate = self.in_fmt.sample_rate;
        self.in_fmt.bytes_per_frame = 0;
        self.in_fmt.format_flags = 0;
        self.in_fmt.bits_per_channel = 0;
        Ok(())
    }

    /// `ACMP3Decoder::SetCurrentOutputFormat` 0x55670.
    pub fn set_current_output_format(&mut self, a: &Asbd) -> Result<(), CodecError> {
        if self.initialized {
            return Err(CodecError(ERR_STATE));
        }
        // kAvailableOutputFormatList = { 0 Hz, lpcm, 0xc, 0, 1, 0, 0, 16 }
        if a.format_id != FORMAT_LPCM || a.format_flags != FLAGS_S16_PACKED || a.bits_per_channel != 16 {
            return Err(CodecError(ERR_DATA));
        }
        if !OUTPUT_RATES.contains(&a.sample_rate) || a.channels_per_frame >= 3 {
            return Err(CodecError(ERR_DATA));
        }
        self.out_fmt = *a;
        if self.out_fmt.sample_rate == 0.0 {
            self.out_fmt.sample_rate = self.in_fmt.sample_rate;
        }
        if self.out_fmt.channels_per_frame == 0 {
            self.out_fmt.channels_per_frame = self.in_fmt.channels_per_frame;
        }
        let bpf = (self.out_fmt.bits_per_channel >> 3).wrapping_mul(self.out_fmt.channels_per_frame);
        self.out_fmt.bytes_per_packet = bpf;
        self.out_fmt.frames_per_packet = 1;
        self.out_fmt.bytes_per_frame = bpf;
        self.out_fmt.reserved = 0;
        Ok(())
    }

    /// `ACMP3Codec::SetMagicCookie` 0x546ec.
    pub fn set_magic_cookie(&mut self, cookie: &[u8]) -> Result<(), CodecError> {
        if self.initialized {
            return Err(CodecError(ERR_STATE));
        }
        if cookie.len() > 0x100 {
            return Err(CodecError(ERR_SIZE));
        }
        self.cookie[..cookie.len()].copy_from_slice(cookie);
        self.cookie_len = cookie.len() as u32;
        // do { t = *(i32*)(p+4); if t == 'frma' { p += 12; size -= 12 } else if t is 'ms\0U' or '.mp3' { flag = p[0x22] } } while ...
        // (native little-endian loads of the atom type, exactly as Apple's code; reads stay inside the 0x100 byte cookie area).
        let mut p = 0usize;
        let mut size = cookie.len() as i32;
        let mut done = false;
        loop {
            if done {
                break;
            }
            let t = self.cookie_i32(p + 4);
            done = t != 0x6672_6d61;
            if t == 0x6672_6d61 {
                p += 12;
                size -= 12;
            } else if t == 0x6d73_0055 || t == 0x2e6d_7033 {
                self.cookie_flag = self.cookie.get(p + 0x22).copied().unwrap_or(0) as u32;
            }
            if size <= 0x2b {
                break;
            }
        }
        Ok(())
    }

    fn cookie_i32(&self, off: usize) -> i32 {
        let mut b = [0u8; 4];
        for (i, v) in b.iter_mut().enumerate() {
            *v = self.cookie.get(off + i).copied().unwrap_or(0);
        }
        i32::from_le_bytes(b)
    }

    // ---- state machine ----------------------------------------------------------------------------------------------

    /// `ACMP3Decoder::Initialize` 0x54c48 (`None` formats re-use the current ones; `cookie` may be `Some(&[])`).
    pub fn initialize(&mut self, input: Option<&Asbd>, output: Option<&Asbd>, cookie: Option<&[u8]>) -> Result<(), CodecError> {
        if self.initialized {
            return Err(CodecError(ERR_STATE));
        }
        let in_rate: i64;
        let out_rate: i64;
        match input {
            None => {
                let cur = self.in_fmt;
                self.set_current_input_format(&cur)?;
                in_rate = self.in_fmt.sample_rate as i64;
            }
            Some(a) => in_rate = a.sample_rate as i64,
        }
        match output {
            Some(b) => out_rate = b.sample_rate as i64,
            None => {
                if let Some(a) = input {
                    self.out_fmt.sample_rate = a.sample_rate;
                    self.out_fmt.channels_per_frame = a.channels_per_frame;
                }
                let cur = self.out_fmt;
                self.set_current_output_format(&cur)?;
                out_rate = self.out_fmt.sample_rate as i64;
            }
        }
        if in_rate != out_rate || in_rate > 48000 {
            return Err(CodecError(ERR_DATA));
        }
        // ACMP3Codec::Initialize 0x545b0
        if let Some(a) = input {
            self.set_current_input_format(a)?;
        }
        if let Some(b) = output {
            self.set_current_output_format(b)?;
        }
        if let Some(c) = cookie {
            self.set_magic_cookie(c)?;
        }
        self.initialized = true;
        // New MP3DecoderWrapper_SpiritDSP (fresh ring and Spirit state).
        self.wrapper = Some(Wrapper::new());
        if self.cookie_flag == 0 {
            self.cookie_flag = 1;
        }
        self.reset()
    }

    /// `ACMP3Decoder::Uninitialize` 0x56454. The wrapper object (ring, Spirit state) is kept.
    pub fn uninitialize(&mut self) {
        self.used_input = 0;
        self.initialized = false;
    }

    /// `ACMP3Decoder::Reset` 0x56488. (Apple crashes if the codec was never initialized; that is `Err(ERR_STATE)` here.)
    pub fn reset(&mut self) -> Result<(), CodecError> {
        let Some(w) = self.wrapper.as_mut() else {
            return Err(CodecError(ERR_STATE));
        };
        self.used_input = 0;
        self.skip_remaining = self.prime_leading.wrapping_add(self.delay);
        self.tail_remaining = self.delay;
        self.eof_seen = false;
        w.reset();
        Ok(())
    }

    /// `ACMP3Decoder::AppendInputData` 0x5597c (+ the `AudioCodecAppendInputData` shim).
    ///
    /// `data.len()` must be at least `*io_bytes`. With `pd == None` the codec needs a constant packet size
    /// (`bytes_per_packet != 0` in the input format, `*io_bytes == bytes_per_packet * *io_packets`) and appends exactly ONE packet
    /// (`bytes_per_packet` bytes) whatever `*io_packets` says. With packet descriptions only `pd[0]` is looked at (one packet per call).
    /// `*io_bytes == 0` or `*io_packets == 0` is the end-of-stream marker (sets the tail flag, consumes nothing).
    /// Like Apple, this does not check the initialized flag (only that an Initialize happened once).
    ///
    /// On `Err` the shim leaves `*io_packets` at its input value while `*io_bytes` keeps what the codec stored before throwing.
    pub fn append_input_data(
        &mut self,
        data: &[u8],
        io_bytes: &mut u32,
        io_packets: &mut u32,
        pd: Option<&[PacketDesc]>,
    ) -> Result<(), CodecError> {
        let packets_in = *io_packets;
        let r = self.append_input_data_inner(data, io_bytes, io_packets, pd);
        if r.is_err() {
            *io_packets = packets_in;
        }
        r
    }

    fn append_input_data_inner(
        &mut self,
        data: &[u8],
        io_bytes: &mut u32,
        io_packets: &mut u32,
        pd: Option<&[PacketDesc]>,
    ) -> Result<(), CodecError> {
        if *io_packets == 0 || *io_bytes == 0 {
            if self.wrapper.is_none() {
                return Err(CodecError(ERR_STATE));
            }
            self.eof_seen = true;
            *io_packets = 0;
            *io_bytes = 0;
            return Ok(());
        }
        if data.len() < *io_bytes as usize {
            return Err(CodecError(ERR_PARAM));
        }
        let bpp = self.in_fmt.bytes_per_packet;
        let Some(w) = self.wrapper.as_mut() else {
            return Err(CodecError(ERR_STATE));
        };
        let added: u32;
        match pd {
            None => {
                if bpp.wrapping_mul(*io_packets) != *io_bytes {
                    return Err(CodecError(ERR_DATA));
                }
                // Fill(data, this+0x40): the size register still holds bytes_per_packet (not *io_bytes).
                let n = w.fill(&data[..bpp as usize]);
                *io_packets = (n == bpp) as u32;
                *io_bytes = n;
                added = n;
            }
            Some(pds) => {
                let Some(d) = pds.first() else {
                    return Err(CodecError(ERR_PARAM));
                };
                let size = d.data_byte_size;
                if size == 0 || (*io_bytes as i64) < d.start_offset.wrapping_add(size as i64) {
                    *io_packets = 0;
                    *io_bytes = 0;
                    added = 0;
                } else {
                    *io_bytes = size;
                    if w.free() < size as i32 {
                        *io_bytes = 0;
                        *io_packets = 0;
                        return Err(CodecError(ERR_BUF));
                    }
                    if d.start_offset < 0 {
                        return Err(CodecError(ERR_PARAM));
                    }
                    let start = d.start_offset as usize;
                    let n = w.fill(&data[start..start + size as usize]);
                    *io_bytes = size.wrapping_add(d.start_offset as u32);
                    if n == size {
                        *io_packets = 1;
                    } else if n != 0 {
                        *io_bytes = n.wrapping_add(d.start_offset as u32);
                        *io_packets = 0;
                    } else {
                        *io_packets = 0;
                        *io_bytes = 0;
                    }
                    added = n;
                }
            }
        }
        self.used_input = self.used_input.wrapping_add(added);
        Ok(())
    }

    /// `ACMP3Decoder::ProduceOutputPackets` 0x55b54: decodes AT MOST ONE frame from the head of the ring into `out`.
    ///
    /// `*io_bytes` = capacity of `out` in bytes (at least `frames_per_packet * output bytes per frame`, else
    /// `Err(ERR_UNSPECIFIED)` with the parameters untouched), `*io_packets` = packets requested (only `== 0` matters).
    /// Returns `outStatus`; on return `*io_packets` = PCM frames produced, `*io_bytes` = bytes produced.
    pub fn produce_output_packets(&mut self, out: &mut [i16], io_bytes: &mut u32, io_packets: &mut u32) -> Result<u32, CodecError> {
        if !self.initialized {
            return Err(CodecError(ERR_STATE));
        }
        let bpf = self.out_fmt.bytes_per_frame;
        if *io_packets == 0 {
            *io_bytes = 0;
            return Ok(STATUS_NEEDS_MORE_INPUT);
        }
        let fpp = self.in_fmt.frames_per_packet;
        let full = bpf.wrapping_mul(fpp);
        if *io_bytes < full {
            return Err(CodecError(ERR_UNSPECIFIED));
        }
        if (out.len() as u64) * 2 < *io_bytes as u64 {
            return Err(CodecError(ERR_PARAM));
        }
        *io_packets = fpp;
        let Some(w) = self.wrapper.as_mut() else {
            return Err(CodecError(ERR_STATE));
        };
        let used_before = w.used;
        let (res, out_bytes) = w.decode_frame(out, *io_bytes);
        let used_after = w.used;
        let status: u32;
        if (res as i32) < 0 {
            if res & 0xffff_fffd == 0x8101_0001 {
                if self.eof_seen && self.tail_remaining != 0 {
                    let n = self.tail_remaining.min(*io_packets);
                    out[..(bpf.wrapping_mul(n) / 2) as usize].fill(0);
                    self.tail_remaining -= n;
                    *io_packets = n;
                    status = if self.tail_remaining != 0 { STATUS_SUCCESS_HAS_MORE } else { STATUS_SUCCESS };
                } else {
                    *io_packets = 0;
                    status = STATUS_NEEDS_MORE_INPUT;
                }
            } else {
                *io_packets = 0;
                status = STATUS_FAILURE;
            }
        } else {
            let consumed = used_before.wrapping_sub(used_after);
            self.used_input = self.used_input.wrapping_sub(consumed);
            if out_bytes == 0 && res != DECODE_SILENT {
                *io_packets = 0;
                status = STATUS_SUCCESS;
            } else {
                if out_bytes == 0 {
                    out[..(full / 2) as usize].fill(0);
                }
                // LAB_00055c64 (*io_packets == fpp != 0 here)
                let remaining = self.used_input;
                status = if remaining < consumed.wrapping_mul(2) && (remaining < consumed || self.cookie_flag < 2) {
                    STATUS_SUCCESS
                } else {
                    STATUS_SUCCESS_HAS_MORE
                };
                if self.skip_remaining != 0 {
                    let n = self.skip_remaining.min(fpp);
                    let bytes = bpf.wrapping_mul(n);
                    *io_packets -= n;
                    self.skip_remaining -= n;
                    if bytes <= full && full - bytes != 0 {
                        out.copy_within((bytes / 2) as usize..(full / 2) as usize, 0);
                    }
                }
            }
        }
        *io_bytes = bpf.wrapping_mul(*io_packets);
        Ok(status)
    }

    // ---- convenience for container code -------------------------------------------------------------------------------

    /// Append one packet exactly like AudioToolbox's converter: `AppendInputData(data, size, 1 packet, pd = {0, 0, size})`.
    /// Returns the consumed byte count reported by the codec (= `packet.len()`).
    pub fn append_packet(&mut self, packet: &[u8]) -> Result<u32, CodecError> {
        let mut bytes = packet.len() as u32;
        let mut packets = 1u32;
        let pd = [PacketDesc { start_offset: 0, variable_frames_in_packet: 0, data_byte_size: packet.len() as u32 }];
        self.append_input_data(packet, &mut bytes, &mut packets, Some(&pd))?;
        Ok(bytes)
    }

    /// The end-of-stream `AppendInputData(0 bytes, 0 packets)`.
    pub fn append_eof(&mut self) -> Result<(), CodecError> {
        let (mut b, mut p) = (0u32, 0u32);
        self.append_input_data(&[], &mut b, &mut p, None)
    }

    /// One `ProduceOutputPackets` with capacity `out.len() * 2` bytes and one packet requested (the converter passes
    /// `frames_per_packet * bytes_per_frame` = 4608 bytes for stereo MPEG-1).
    pub fn produce(&mut self, out: &mut [i16]) -> Result<Produced, CodecError> {
        let mut bytes = (out.len() * 2) as u32;
        let mut packets = 1u32;
        let status = self.produce_output_packets(out, &mut bytes, &mut packets)?;
        Ok(Produced { status, bytes, packets })
    }

    /// Decode one whole stream with the call pattern AudioToolbox's converter uses (observed with lldb on valid files):
    /// per packet `append_packet` + one `produce` (buffer `frames_per_packet * bytes_per_frame`), then one more `produce`
    /// (status 4), the end-of-stream `append_eof`, and one `produce` (the 529 zero frames of the decoder-delay tail).
    /// The codec stream is returned unaltered (INCLUDING the first-packet delay trim and the zero tail); AudioFile's priming /
    /// remainder trimming is applied above this layer. Produces with status `STATUS_FAILURE` contribute no samples and are counted in
    /// `failures` (what the real converter does with the -50 of a failed produce on damaged data is NOT modelled here).
    pub fn decode_stream<'a, I: IntoIterator<Item = &'a [u8]>>(&mut self, packets: I) -> Result<StreamDecode, CodecError> {
        let cap = (self.in_fmt.frames_per_packet * self.out_fmt.bytes_per_frame / 2) as usize;
        let mut buf = vec![0i16; cap];
        let mut r = StreamDecode { pcm: Vec::new(), failures: 0 };
        let take = |c: &mut Self, buf: &mut Vec<i16>, r: &mut StreamDecode| -> Result<(), CodecError> {
            let p = c.produce(buf)?;
            if p.status == STATUS_FAILURE {
                r.failures += 1;
            } else {
                r.pcm.extend_from_slice(&buf[..p.bytes as usize / 2]);
            }
            Ok(())
        };
        for pkt in packets {
            self.append_packet(pkt)?;
            take(self, &mut buf, &mut r)?;
        }
        take(self, &mut buf, &mut r)?;
        self.append_eof()?;
        take(self, &mut buf, &mut r)?;
        Ok(r)
    }
}

/// Result of [`Mp3Codec::decode_stream`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamDecode {
    /// Interleaved s16 samples of the codec stream.
    pub pcm: Vec<i16>,
    /// Number of `ProduceOutputPackets` calls that ended in `STATUS_FAILURE`.
    pub failures: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    thread_local! {
        static Q: RefCell<VecDeque<(FrameResult, i16)>> = const { RefCell::new(VecDeque::new()) };
    }

    /// Scripted `DecodeFrame`: pops (result, fill value); empty queue = "need more input".
    #[derive(Default)]
    struct Mock;

    impl FrameDecoder for Mock {
        fn reset(&mut self) {}
        fn decode_frame(&mut self, _ring: &[u8; RING_SIZE], _avail: usize, out: &mut [i16], _cap: usize) -> FrameResult {
            let (r, v) = Q.with(|q| q.borrow_mut().pop_front()).unwrap_or((FrameResult { status: DECODE_NEED_MORE, consumed: 0, out_bytes: 0 }, 0));
            out[..r.out_bytes as usize / 2].fill(v);
            r
        }
    }

    fn queue(status: u32, consumed: usize, out_bytes: u32, v: i16) {
        Q.with(|q| q.borrow_mut().push_back((FrameResult { status, consumed, out_bytes }, v)));
    }

    fn open() -> Mp3Codec<Mp3Mock> {
        Q.with(|q| q.borrow_mut().clear());
        Mp3Codec::open_like_converter(44100.0, 2, 1152).unwrap()
    }
    type Mp3Mock = Mock;

    #[test]
    fn first_packet_drops_decoder_delay_and_eof_emits_zero_tail() {
        let mut c = open();
        c.append_packet(&[0xff; 417]).unwrap();
        queue(DECODE_OK, 417, 4608, 7);
        let mut out = vec![0x5a5au16 as i16; 2304];
        let p = c.produce(&mut out).unwrap();
        assert_eq!((p.status, p.bytes, p.packets), (STATUS_SUCCESS, 2492, 623));
        assert_eq!(out[0], 7);
        assert_eq!(c.used_input_buffer_size(), 0);
        // no input: needs more
        let p = c.produce(&mut out).unwrap();
        assert_eq!((p.status, p.bytes, p.packets), (STATUS_NEEDS_MORE_INPUT, 0, 0));
        c.append_eof().unwrap();
        let p = c.produce(&mut out).unwrap();
        assert_eq!((p.status, p.bytes, p.packets), (STATUS_SUCCESS, 2116, 529));
        assert!(out[..1058].iter().all(|&s| s == 0));
        let p = c.produce(&mut out).unwrap();
        assert_eq!(p.status, STATUS_NEEDS_MORE_INPUT);
    }

    #[test]
    fn failure_statuses_map_to_param_err() {
        let mut c = open();
        c.append_packet(&[1; 100]).unwrap();
        queue(DECODE_OUT_TOO_SMALL, 100, 0, 0);
        let mut out = vec![0i16; 2304];
        let p = c.produce(&mut out).unwrap();
        assert_eq!((p.status, p.bytes, p.packets, p.os_status()), (STATUS_FAILURE, 0, 0, ERR_PARAM));
        // ubuf is NOT corrected by the failed decode (stale)
        assert_eq!(c.used_input_buffer_size(), 100);
        c.append_packet(&[1; 100]).unwrap();
        queue(DECODE_SILENT, 100, 0, 0);
        let p = c.produce(&mut out).unwrap();
        // a silent frame is a full zero packet (minus the 529 frame delay) and the first non-negative decode corrects ubuf by 100 only
        assert_eq!((p.status, p.bytes, p.packets), (STATUS_SUCCESS, 2492, 623));
        assert_eq!(c.used_input_buffer_size(), 100);
    }

    #[test]
    fn ring_full_is_buf_error_and_keeps_packets() {
        let mut c = open();
        for _ in 0..8 {
            c.append_packet(&[2; 1000]).unwrap();
        }
        let (mut b, mut n) = (1000u32, 1u32);
        let pd = [PacketDesc { start_offset: 0, variable_frames_in_packet: 0, data_byte_size: 1000 }];
        let e = c.append_input_data(&[2; 1000], &mut b, &mut n, Some(&pd)).unwrap_err();
        assert_eq!((e.0, b, n), (ERR_BUF, 0, 1));
        assert_eq!(c.used_input_buffer_size(), 8000);
        // 192 bytes still fit exactly
        c.append_packet(&[3; 192]).unwrap();
        assert_eq!(c.used_input_buffer_size(), 8192);
    }

    #[test]
    fn append_without_descriptions_takes_one_packet() {
        let mut c: Mp3Codec<Mock> = Mp3Codec::new();
        c.initialize(Some(&Asbd::mp3(44100.0, 2, 1152, 417)), Some(&Asbd::pcm_s16(44100.0, 2)), None).unwrap();
        let (mut b, mut n) = (834u32, 2u32);
        c.append_input_data(&[9; 834], &mut b, &mut n, None).unwrap();
        assert_eq!((b, n, c.used_input_buffer_size()), (417, 1, 417));
        let (mut b, mut n) = (835u32, 2u32);
        assert_eq!(c.append_input_data(&[9; 835], &mut b, &mut n, None).unwrap_err().0, ERR_DATA);
        // variable-size input format: no packet descriptions is an error
        let mut c: Mp3Codec<Mock> = Mp3Codec::open_like_converter(44100.0, 2, 1152).unwrap();
        let (mut b, mut n) = (417u32, 1u32);
        assert_eq!(c.append_input_data(&[9; 417], &mut b, &mut n, None).unwrap_err().0, ERR_DATA);
    }

    #[test]
    fn prime_info_extends_the_head_trim() {
        Q.with(|q| q.borrow_mut().clear());
        let mut c: Mp3Codec<Mock> = Mp3Codec::new();
        c.initialize(Some(&Asbd::mp3(44100.0, 2, 1152, 0)), Some(&Asbd::pcm_s16(44100.0, 2)), None).unwrap();
        c.uninitialize();
        c.set_prime_info(576, 0).unwrap();
        c.initialize(None, None, None).unwrap();
        let mut out = vec![0i16; 2304];
        // 529 + 576 = 1105 frames to drop: the first packet yields 47 frames, the second 1152
        c.append_packet(&[1; 10]).unwrap();
        queue(DECODE_OK, 10, 4608, 1);
        let p = c.produce(&mut out).unwrap();
        assert_eq!((p.packets, p.bytes), (47, 188));
        assert_eq!(c.set_prime_info(1, 1).unwrap_err().0, ERR_STATE);
    }

    #[test]
    fn format_validation() {
        let mut c: Mp3Codec<Mock> = Mp3Codec::new();
        let mp3 = |r, ch, fpp, bpp| Asbd::mp3(r, ch, fpp, bpp);
        let pcm = |r, ch| Asbd::pcm_s16(r, ch);
        assert_eq!(c.initialize(Some(&mp3(44100.0, 2, 1152, 0)), Some(&pcm(48000.0, 2)), None).unwrap_err().0, ERR_DATA);
        assert_eq!(c.initialize(Some(&mp3(96000.0, 2, 1152, 0)), Some(&pcm(96000.0, 2)), None).unwrap_err().0, ERR_DATA);
        assert_eq!(c.initialize(Some(&mp3(44100.0, 2, 576, 0)), Some(&pcm(44100.0, 2)), None).unwrap_err().0, ERR_DATA);
        assert_eq!(c.initialize(Some(&mp3(44100.0, 3, 1152, 0)), Some(&pcm(44100.0, 2)), None).unwrap_err().0, ERR_DATA);
        assert_eq!(c.initialize(Some(&mp3(44100.0, 2, 1152, 4609)), Some(&pcm(44100.0, 2)), None).unwrap_err().0, ERR_DATA);
        // 22.05 kHz accepts 1152 (normalised to 576), 4608 (8 x 576) and rejects 5760
        c.initialize(Some(&mp3(22050.0, 2, 1152, 0)), Some(&pcm(22050.0, 2)), None).unwrap();
        assert_eq!(c.packet_frame_size(), 576);
        assert_eq!(c.initialize(None, None, None).unwrap_err().0, ERR_STATE);
        c.uninitialize();
        assert_eq!(c.set_current_input_format(&mp3(22050.0, 2, 5760, 0)).unwrap_err().0, ERR_DATA);
        c.set_current_input_format(&mp3(22050.0, 2, 4608, 0)).unwrap();
        // input channels 0 inherit the CURRENT output channels (2), then the output description sets 1
        c.initialize(Some(&mp3(44100.0, 0, 0, 0)), Some(&pcm(44100.0, 1)), None).unwrap();
        assert_eq!((c.input_format().channels_per_frame, c.output_format().channels_per_frame, c.packet_frame_size()), (2, 1, 1152));
    }

    #[test]
    fn decode_stream_follows_the_converter_pattern() {
        Q.with(|q| q.borrow_mut().clear());
        let mut c: Mp3Codec<Mock> = Mp3Codec::open_like_converter(44100.0, 2, 1152).unwrap();
        for i in 1..=3i16 {
            queue(DECODE_OK, 100, 4608, i);
        }
        let p = [1u8; 100];
        let r = c.decode_stream([&p[..], &p[..], &p[..]]).unwrap();
        // 3 packets x 1152 frames - 529 head trim + 529 zero tail frames, stereo
        assert_eq!(r.pcm.len(), (3 * 1152) * 2);
        assert_eq!(r.failures, 0);
        assert!(r.pcm[..(1152 - 529) * 2].iter().all(|&s| s == 1));
        assert!(r.pcm[r.pcm.len() - 529 * 2..].iter().all(|&s| s == 0));
    }

    #[test]
    fn cookie_flag_enables_has_more() {
        // atom: size(4) type(4, native LE of '.mp3') body; byte 0x22 = flag = 2
        let mut cookie = vec![0u8; 0x30];
        cookie[4..8].copy_from_slice(&0x2e6d_7033u32.to_le_bytes());
        cookie[0x22] = 2;
        let mut c: Mp3Codec<Mock> = Mp3Codec::new();
        c.initialize(Some(&Asbd::mp3(44100.0, 2, 1152, 0)), Some(&Asbd::pcm_s16(44100.0, 2)), Some(&cookie)).unwrap();
        c.append_packet(&[1; 100]).unwrap();
        c.append_packet(&[1; 100]).unwrap();
        queue(DECODE_OK, 100, 4608, 1);
        let mut out = vec![0i16; 2304];
        // remaining (100) == consumed (100): HAS_MORE only because the flag is >= 2
        assert_eq!(c.produce(&mut out).unwrap().status, STATUS_SUCCESS_HAS_MORE);
    }
}
