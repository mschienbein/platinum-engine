//! decoder::file: container / read-semantics layer in front of the AAC-LC decoder core, bit-exact to `ExtAudioFile` for the container variants it
//! was recorded against.
//!
//! An [`AacFile`] owns the whole file image (inputs of up to 2 GiB, the limit of `Reader::open`; the LOAS access units are repacked inside that
//! buffer), so a file is decoded from memory.
//!
//! ```text
//! AacFile::parse_with_hint(bytes, ext) == ExtAudioFileOpenURL           (OpenError: 'dta?' 'typ?' 'wht?' 'fmt?' 'sync' 'rate' '!dat')
//! file.decoder_config()                == ClientDataFormat acceptance   (ClientError: 'fmt?' '!dat')  -> DecoderConfig for decoder::bits
//! file.trim_plan()                     == FileLengthFrames / PacketTableInfo / priming / valid / remainder
//! file.packet(i), file.pull(..)        == AudioFileReadPacketData and the converter's packet requests (ReadError -50 / -66567)
//! AacDecoder (decoder::decoder)        == ExtAudioFileRead              (priming trim, V cut, planar / stereo downmix client)
//! ```
#![forbid(unsafe_code)]

mod adts;
pub mod asc;
mod bits;
mod caf;
pub mod error;
pub mod layout;
mod loas;
pub mod mixer;
pub mod mp4;
pub mod trim;
#[cfg(test)]
mod container_rules;

pub(crate) use adts::MAX_TAG_BLOCKS;
pub use asc::{Asc, DecoderConfig, PceConfig};
pub use error::{ClientError, OpenError, ReadError, Unspecified};
pub use layout::Layout;
pub use mixer::ClientConverter;
pub use mp4::FileType;
pub use trim::{TrimPlan, TrimSource};

use asc::{parse_asc, sfb_table_index, SAMPLE_RATES};

/// `mFormatID` of the file data format Apple reports.  Only [`FormatId::Aac`] is decoded by this module; every other id opens in Apple but
/// fails at `ClientDataFormat` or at the first read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatId {
    /// `'aac '` AAC-LC (ASC audioObjectType 2, no SBR/PS signalling)
    Aac,
    /// `'aacm'` Main (1), `'aacr'` SSR (3), `'aact'` LTP (4)
    AacMain,
    AacSsr,
    AacLtp,
    /// `'aacl'` LD (23), `'aace'` ELD (39)
    AacLd,
    AacEld,
    /// `'aach'` HE-AAC v1 (SBR signalled after an LC ASC), `'aacp'` HE-AAC v2 (PS)
    AacHe,
    AacHeV2,
    /// `'usac'` (42)
    Usac,
    /// `'.mp1'` / `'.mp2'` (MPEG-4 audio object types 32 / 33)
    Mpeg1Layer1,
    Mpeg1Layer2,
}

impl FormatId {
    pub fn fourcc(&self) -> [u8; 4] {
        *match self {
            FormatId::Aac => b"aac ",
            FormatId::AacMain => b"aacm",
            FormatId::AacSsr => b"aacr",
            FormatId::AacLtp => b"aact",
            FormatId::AacLd => b"aacl",
            FormatId::AacEld => b"aace",
            FormatId::AacHe => b"aach",
            FormatId::AacHeV2 => b"aacp",
            FormatId::Usac => b"usac",
            FormatId::Mpeg1Layer1 => b".mp1",
            FormatId::Mpeg1Layer2 => b".mp2",
        }
    }
    /// `mFramesPerPacket` Apple reports.
    pub fn frames_per_packet(&self) -> u32 {
        match self {
            FormatId::Aac | FormatId::AacMain | FormatId::AacSsr | FormatId::AacLtp => 1024,
            FormatId::AacLd => 512,
            FormatId::AacEld => 480,
            FormatId::AacHe | FormatId::AacHeV2 => 2048,
            FormatId::Usac => 768,
            FormatId::Mpeg1Layer1 => 384,
            FormatId::Mpeg1Layer2 => 1152,
        }
    }
}

/// Maximum access-unit size per output channel (`kAudioConverterPropertyMaximumInputPacketSize = 1024 * channels`).
pub const AU_BYTES_PER_CHANNEL: usize = 1024;

/// An opened AAC file; it owns the byte image of the file (LOAS: the repacked access units).
pub struct AacFile {
    data: Vec<u8>,
    file_type: FileType,
    format: FormatId,
    fpp: u32,
    sample_rate: u32,
    /// `mSampleRate` of the file data format exactly (a CAF `desc` may hold a fractional rate)
    rate_hz: f64,
    /// channels of the file data format
    channels: u32,
    /// channels the decoder produces (differs from `channels` only for CAF `desc` mismatches)
    decoder_channels: u32,
    layout: Option<&'static Layout>,
    asc: Asc,
    /// ADTS channel_configuration actually in force (an SCE stream under cfg 2 is mono)
    channel_config: u8,
    pce: Option<PceConfig>,
    packets: Vec<(u64, u32)>,
    /// MP4: the first `unlocated` packets cannot be located (the sample table has no sample-to-chunk map): every read of one fails `'pck?'`
    unlocated: usize,
    /// LOAS: indices (ascending) of the packets that cannot be read as access units: the read that reaches one fails `'fmt?'`
    failing: Vec<usize>,
    plan: TrimPlan,
    audio_data_bytes: u64,
    max_packet_size: u32,
    /// ADTS: index of the first frame that announces more than one raw_data_block (`packets` holds the frames before it; the properties see none)
    multi_block_at: Option<usize>,
    /// packets the AudioFile properties see (`packets` for everything but a multi-block ADTS file, which reports none)
    table_packets: u64,
    /// CAF: leading packets that lie completely inside the file (the data may be cut inside the packet table's range)
    complete_packets: usize,
    /// container-level refusal of the client data format (CAF desc/chan mismatches)
    extra_client_error: Option<ClientError>,
    /// `mFormatFlags` of the file data format (CAF `desc`; 0 elsewhere)
    format_flags: u32,
    /// packet bytes behind this offset are not audio data (a complete CAF `data` chunk followed by other chunks)
    byte_limit: u64,
    /// no packets, and a read ends at once instead of failing (a LOAS stream whose first frame runs past the end of the file)
    empty_stream: bool,
    /// CAF `desc` without frames per packet: the frames of the packet table (`None`: the table does not hold them)
    pair_frames: Option<u64>,
}

struct Classified {
    format: FormatId,
    /// `mFramesPerPacket` of the file data format
    fpp: u32,
    /// frames per packet the packet table / `T` is computed with (HE-AAC: the core rate, 1024)
    table_fpp: u32,
    rate: u32,
    channels: u32,
    layout: Option<&'static Layout>,
    channel_config: u8,
    pce: Option<PceConfig>,
    /// what the client format fails with although the file opens (SBR / PS signalling that no client format goes with)
    client_error: Option<ClientError>,
}

/// What the configuration (`ASC`) makes of the file.  SBR / PS signalling counts for `audioObjectType` 2 only, and is the same whether it was written
/// hierarchically (`audioObjectType` 5 / 29 first) or as the backward-compatible extension: AudioToolbox answered alike for all 576 combinations of
/// the two forms, core rates, extension rates and the two channel configurations tried (macOS 27.2).  The rules:
/// * the extension rate is the core rate or twice it (exact Hz), else the open fails `'wht?'`;
/// * SBR: the channel configurations 3 and 5 open and fail the client format `'fmt?'`, the others (1, 2, 4, 6, 7, 11, 12, 14) are HE-AAC;
/// * PS: channel configuration 1 is HE-AAC v2 (stereo) when the rate doubles and fails the client format `'fmt?'` when it does not, the configurations
///   2, 6, 7, 11, 12 and 14 open as plain mono AAC-LC at the core rate, the others (3, 4, 5, 8, 9, 10, 13, 15) fail the client format `'!dat'`.
fn classify(asc: &Asc) -> Result<Classified, OpenError> {
    use Unspecified::*;
    let fail = |u| OpenError::Unspecified(u);
    let he = if asc.aot == 2 { asc.he } else { None };
    let mut client_error = None;
    if let Some(h) = he {
        let (core, ext) = (u64::from(asc.sample_rate), u64::from(h.ext_rate));
        if ext != core && ext != 2 * core {
            return Err(fail(BadSbrRate));
        }
        let cfg = asc.channel_config;
        if h.ps {
            match cfg {
                1 => {
                    if ext == core {
                        client_error = Some(ClientError::FormatMismatch);
                    }
                }
                2 | 6 | 7 | 11 | 12 | 14 => {
                    let mut plain = asc.clone();
                    plain.he = None;
                    plain.channel_config = 1;
                    plain.pce = None;
                    return classify(&plain);
                }
                3 | 4 | 5 | 8 | 9 | 10 | 13 | 15 => {
                    let layout = layout::layout_for_tag(layout::TAG_STEREO);
                    return Ok(Classified { format: FormatId::AacHeV2, fpp: 2048, table_fpp: 1024, rate: h.ext_rate, channels: 2, layout, channel_config: cfg, pce: None, client_error: Some(ClientError::NoDecoder) });
                }
                _ => {}
            }
        } else if matches!(cfg, 3 | 5) {
            client_error = Some(ClientError::FormatMismatch);
        }
    }
    let format = match asc.aot {
        2 => match he {
            Some(h) if h.ps => FormatId::AacHeV2,
            Some(_) => FormatId::AacHe,
            None => FormatId::Aac,
        },
        1 => FormatId::AacMain,
        3 => FormatId::AacSsr,
        4 => FormatId::AacLtp,
        23 => FormatId::AacLd,
        39 => FormatId::AacEld,
        42 => FormatId::Usac,
        32 => FormatId::Mpeg1Layer1,
        33 => FormatId::Mpeg1Layer2,
        _ => return Err(fail(UnsupportedAot)),
    };
    let rate = match he {
        Some(h) if h.ext_rate != 0 => h.ext_rate,
        _ => asc.sample_rate,
    };
    if asc.sfi_raw == 15 && (asc.sample_rate == 0 || asc.sample_rate > 3_000_000) {
        return Err(fail(BadSampleRate));
    }
    let cfg = asc.channel_config;
    let (layout, pce) = if cfg == 0 {
        match (&asc.pce, asc.aot) {
            (Some(p), 1 | 2 | 3 | 4 | 23) => {
                // the PCE's sampling_frequency_index must be the SFB-table index of the stream rate (recorded on macOS 27.2)
                if sfb_table_index(asc.sample_rate) != Some(p.sfi) {
                    return Err(fail(UnmappedPceLayout));
                }
                (Some(layout::pce_layout(p).ok_or(fail(UnmappedPceLayout))?), Some(p.clone()))
            }
            (None, 1 | 2 | 3 | 4 | 23) => return Err(fail(BadChannelConfig)),
            _ => (None, None),
        }
    } else {
        match layout::config_layout(cfg) {
            Some(l) => (Some(l), None),
            None if matches!(asc.aot, 1 | 2 | 3 | 4 | 23) => return Err(fail(BadChannelConfig)),
            None => (None, None),
        }
    };
    let mut channels = layout.map_or(0, |l| u32::from(l.channels));
    if format == FormatId::AacHeV2 {
        channels = 2;
    }
    let mut layout = match format {
        FormatId::Aac | FormatId::AacLd | FormatId::AacEld | FormatId::AacHe | FormatId::AacHeV2 => layout,
        _ => None,
    };
    let mut fpp = format.frames_per_packet();
    let mut table_fpp = fpp;
    match format {
        // AAC-LC with frameLengthFlag: Apple reports 960 frames per packet and no layout (dependsOnCoreCoder with a valid
        // 14-bit delay is an ordinary LC stream)
        FormatId::Aac if asc.frame_length_flag => {
            fpp = 960;
            table_fpp = 960;
            layout = None;
        }
        FormatId::AacEld => {
            fpp = if asc.frame_length_flag { 480 } else { 512 };
            table_fpp = fpp;
        }
        // HE-AAC: packets are 1024 core frames, the file data format says 2048 output frames, V is reported at the output rate
        FormatId::AacHe | FormatId::AacHeV2 => table_fpp = 1024,
        _ => {}
    }
    if format == FormatId::AacHeV2 {
        layout = layout::layout_for_tag(layout::TAG_STEREO);
    }
    Ok(Classified { format, fpp, table_fpp, rate, channels, layout, channel_config: cfg, pce, client_error })
}

/// Priming Apple assumes when the file has no priming metadata (`None`: no packet table at all).  Observed: `'aac '` and
/// HE-AAC 2112; Main/SSR/LTP 0; LD one packet; ELD half a packet; MPEG-1 audio and USAC have no packet table.
fn default_priming(format: FormatId, table_fpp: u32) -> Option<u64> {
    match format {
        FormatId::Aac | FormatId::AacHe | FormatId::AacHeV2 => Some(trim::DEFAULT_PRIMING),
        FormatId::AacMain | FormatId::AacSsr | FormatId::AacLtp => Some(0),
        FormatId::AacLd => Some(u64::from(table_fpp)),
        FormatId::AacEld => Some(u64::from(table_fpp / 2)),
        FormatId::Usac | FormatId::Mpeg1Layer1 | FormatId::Mpeg1Layer2 => None,
    }
}

fn mp4_trim(t: &mp4::Mp4Track, format: FormatId, packets: u64, fpp: u32, rate: u32) -> TrimPlan {
    let total = packets * u64::from(fpp);
    if let (true, Some(e)) = (t.roll_gate, t.first_edit) {
        let media = e.media_time as u64;
        let skip = if t.media_ts == 0 { media } else { (u128::from(media) * u128::from(rate) / u128::from(t.media_ts)).min(u128::from(u64::MAX)) as u64 }.min(total);
        let room = total - skip;
        let v = if e.segment_duration == 0 {
            room
        } else if t.movie_ts == 0 {
            e.segment_duration.min(room)
        } else {
            ((u128::from(e.segment_duration) * u128::from(rate) / u128::from(t.movie_ts)).min(u128::from(u64::MAX)) as u64).min(room)
        };
        // QuickTime (`MooV`) files report no remainder
        let remainder = if t.file_type == FileType::Mov { 0 } else { room - v };
        return TrimPlan {
            source: TrimSource::EditList,
            packets,
            frames_per_packet: fpp,
            decoded_frames: total,
            priming: skip,
            remainder,
            valid_frames: v,
            has_packet_table: true,
            valid_unknown: false,
        };
    }
    if let Some(i) = t.itun {
        return TrimPlan {
            source: TrimSource::ITunSmpb,
            packets,
            frames_per_packet: fpp,
            decoded_frames: total,
            priming: u64::from(i.priming),
            remainder: u64::from(i.remainder),
            valid_frames: i.valid_frames,
            has_packet_table: true,
            valid_unknown: false,
        };
    }
    plan_default(format, packets, fpp)
}

fn plan_default(format: FormatId, packets: u64, fpp: u32) -> TrimPlan {
    match default_priming(format, fpp) {
        Some(p) => TrimPlan::default_with(packets, fpp, p),
        None => TrimPlan::untrimmed(packets, fpp),
    }
}

/// The `kAudioFilePropertyBitRate` rule (see [`AacFile::bitrate`]): `bytes * 8 / (frames / rate)` in `f64`, cut to `u32` (saturating).  The double
/// rounding of the duration matters: 7686 bytes over 14 packets of 1024 frames at 48 kHz is exactly 205875 bit/s as an integer ratio, and
/// `205874.99999999997` as AudioToolbox computes it.
pub(crate) fn bitrate_of(audio_data_bytes: u64, frames: u64, rate: u32) -> u32 {
    if frames == 0 || rate == 0 {
        return 0;
    }
    let duration = frames as f64 / f64::from(rate);
    (audio_data_bytes as f64 * 8.0 / duration) as u32
}

impl AacFile {

    /// Opens the file image `data` with the file-name extension (without dot) Apple would see as its type hint.  The hint only matters for files that
    /// are not recognised by content: `aac`/`adts`/`loas`/`latm` then fail `'sync'` instead of `'typ?'`.
    pub fn parse_with_hint(data: Vec<u8>, extension: Option<&str>) -> Result<AacFile, OpenError> {
        // the generic open check: nothing behind the leading tag blocks is as good as an empty file
        if data.is_empty() || adts::skip_tags(&data) >= data.len() {
            return Err(OpenError::EmptyFile);
        }
        let file_len = data.len() as u64;
        let ext = extension.map(|e| e.to_ascii_lowercase());
        // an ADTS / LOAS name hint is authoritative (MP4 bytes named .aac fail 'sync')
        match ext.as_deref() {
            Some("aac") | Some("adts") => {
                return Self::from_adts(data, file_len);
            }
            Some("loas") | Some("latm") => {
                return if loas::looks_like_loas(&data) { Self::from_loas(data, file_len) } else { Err(OpenError::UnsupportedDataFormat) };
            }
            _ => {}
        }
        if caf::looks_like_caf(&data) {
            return Self::from_caf(data, file_len);
        }
        if data.len() >= 8 && &data[4..8] == b"ftyp" {
            return Self::from_mp4(data, file_len);
        }
        if adts::looks_like_adts(&data) {
            return Self::from_adts(data, file_len);
        }
        if loas::looks_like_loas(&data) {
            return Self::from_loas(data, file_len);
        }
        Err(OpenError::UnsupportedFileType)
    }

    fn from_mp4(data: Vec<u8>, file_len: u64) -> Result<Self, OpenError> {
        let t = mp4::parse_mp4(&data)?;
        let c = classify(&t.asc)?;
        let fpp = c.table_fpp;
        let total_bytes: u64 = t.samples.iter().map(|s| u64::from(s.1)).sum();
        if file_len < total_bytes {
            return Err(OpenError::Unspecified(Unspecified::FileShorterThanAudio));
        }
        let plan = mp4_trim(&t, c.format, t.samples.len() as u64, fpp, c.rate);
        let max = t.samples.iter().map(|s| s.1).max().unwrap_or(0);
        let table_packets = t.samples.len() as u64;
        Ok(AacFile {
            data,
            file_type: t.file_type,
            format: c.format,
            fpp: c.fpp,
            sample_rate: c.rate,
            rate_hz: f64::from(c.rate),
            channels: c.channels,
            decoder_channels: c.channels,
            layout: c.layout,
            asc: t.asc,
            channel_config: c.channel_config,
            pce: c.pce,
            packets: t.samples,
            unlocated: t.unlocated,
            failing: Vec::new(),
            plan,
            audio_data_bytes: t.data_bytes,
            max_packet_size: max,
            multi_block_at: None,
            table_packets,
            complete_packets: usize::MAX,
            extra_client_error: c.client_error,
            format_flags: 0,
            byte_limit: u64::MAX,
            empty_stream: false,
            pair_frames: None,
        })
    }

    fn from_caf(data: Vec<u8>, file_len: u64) -> Result<Self, OpenError> {
        let c = caf::parse_caf(&data)?;
        // `alac` is here for a cookie that is an AAC one: AudioToolbox builds no decoder for the pair
        let (mut format, alac_with_aac_cookie) = match &c.format_id {
            b"aac " => (FormatId::Aac, false),
            b"aach" => (FormatId::AacHe, false),
            b"aacp" => (FormatId::AacHeV2, false),
            b"aacl" => (FormatId::AacLd, false),
            b"aace" => (FormatId::AacEld, false),
            b"alac" if c.asc_bytes.is_some() => (FormatId::Aac, true),
            _ => return Err(OpenError::UnsupportedDataFormat),
        };
        // decoder configuration: the cookie's ASC, or (no `kuki`) one synthesised from `desc`; a rate that is not a number opens like 0
        if c.sample_rate == 0.0 || c.sample_rate.is_nan() {
            return Err(OpenError::ZeroSampleRate);
        }
        let rate = c.sample_rate as u32;
        let mut extra_client_error = None;
        // a cookie no decoder can be built from opens, and fails the client format
        let cookie = c.asc_bytes.as_ref().and_then(|b| {
            let asc = parse_asc(b).ok()?;
            let k = classify(&asc).ok()?;
            Some((asc, k))
        });
        if c.bad_cookie || (c.asc_bytes.is_some() && cookie.is_none()) {
            extra_client_error = Some(ClientError::NoDecoder);
        }
        let (asc, mut k) = match cookie {
            Some((asc, k)) => (asc, Some(k)),
            None => {
                let cfg = match c.channels {
                    1..=6 => c.channels as u8,
                    7 => 11,
                    8 => 7,
                    _ => 0,
                };
                let asc = Asc {
                    aot: 2,
                    sfi_raw: sfb_table_index(rate).unwrap_or(15),
                    sample_rate: rate,
                    channel_config: cfg,
                    frame_length_flag: false,
                    depends_on_core: false,
                    extension_flag: false,
                    pce: None,
                    he: None,
                    bits_consumed: 16,
                };
                if c.channels > 8 {
                    extra_client_error = Some(ClientError::UnsupportedChannelCount);
                    (asc, None)
                } else {
                    let k = classify(&asc)?;
                    (asc, Some(k))
                }
            }
        };
        let fpp = c.frames_per_packet;
        // `aac ` over a cookie that signals SBR or PS: AudioToolbox decodes it (macOS 27.2, whatever the `desc` rate and frames per packet say: all the
        // HE-AAC cookies tried), as something that is not the AAC-LC core's decode with either rate's tables, so it is the HE-AAC format of the cookie
        // and the HE-AAC refusal (the checks that name the plain format are for the plain format)
        if format == FormatId::Aac && !alac_with_aac_cookie {
            if let Some(he @ (FormatId::AacHe | FormatId::AacHeV2)) = k.as_ref().map(|k| k.format) {
                format = he;
            }
        }
        // the format `desc` names against the one the cookie describes: HE-AAC over a plain AAC cookie has no decoder, the other families no format
        match (format, k.as_ref().map(|k| k.format)) {
            _ if alac_with_aac_cookie => extra_client_error = Some(ClientError::NoDecoder),
            (FormatId::AacHe, Some(FormatId::Aac)) if c.asc_bytes.is_some() => extra_client_error = Some(ClientError::NoDecoder),
            // `aacp` without a cookie opens for 1 or 2 channels at 2048 frames per packet (macOS 27.2: and decodes HE-AAC v2 data; any other shape fails `'fmt?'`)
            (FormatId::AacHeV2, _) if c.asc_bytes.is_none() && fpp == 2048 && c.channels <= 2 => {}
            (FormatId::AacHeV2 | FormatId::AacLd | FormatId::AacEld, found) if found != Some(format) => extra_client_error = Some(ClientError::FormatMismatch),
            _ => {}
        }
        let (layout, decoder_channels, channel_config, pce) = match k.as_mut() {
            Some(k) => (k.layout, k.channels, k.channel_config, k.pce.take()),
            None => (None, c.channels, asc.channel_config, None),
        };
        // the refusals of the client format in the order AudioToolbox meets them: the data format, the decoder (more `desc` channels than the
        // stream has), the channel layout (a tag of more than two channels must name the `desc` channel count), then what is not reproduced
        // (a `desc` frames-per-packet of 0 opens: the first read fails)
        if fpp >= 1_000_000 {
            extra_client_error = extra_client_error.or(Some(ClientError::InvalidPacketFrames));
        }
        if format == FormatId::Aac && ((fpp != 1024 && fpp != 0) || c.bytes_per_packet != 0 || c.bits_per_channel != 0) {
            extra_client_error = extra_client_error.or(Some(ClientError::NotAacLc));
        }
        if fpp == 0 && (c.packets.is_empty() || c.table_numbers < 2) {
            extra_client_error = extra_client_error.or(Some(ClientError::NoPacketTable));
        }
        if c.channels > 8 {
            extra_client_error = extra_client_error.or(Some(ClientError::UnsupportedChannelCount));
        }
        if layout.is_some_and(|l| c.channels > u32::from(l.channels)) {
            extra_client_error = extra_client_error.or(Some(ClientError::NoDecoder));
        }
        let file_layout_tag = c.chan_tag.unwrap_or(0);
        let tag_channels = file_layout_tag & 0xFFFF;
        if tag_channels > 2 && tag_channels != c.channels {
            extra_client_error = extra_client_error.or(Some(ClientError::InvalidLayout));
        }
        if format == FormatId::Aac && layout.is_some_and(|l| c.channels < u32::from(l.channels)) {
            extra_client_error = extra_client_error.or(Some(ClientError::UnverifiedConversion));
        }
        // a `desc` rate that is not exactly the stream's (even by 0.0001 Hz), or not a whole number of Hz, takes another path in AudioToolbox
        // (the converter's limits come first: a rate it cannot produce fails as for an ASC that names it)
        if format == FormatId::Aac && (c.sample_rate != f64::from(k.as_ref().map_or(rate, |k| k.rate)) || c.sample_rate.fract() != 0.0) {
            extra_client_error = extra_client_error.or(Some(if rate >= 3_000_000 {
                ClientError::InvalidRate
            } else if rate == 7350 || rate > 110_851 {
                ClientError::UnsupportedRate
            } else {
                ClientError::RateMismatch
            }));
        }
        extra_client_error = extra_client_error.or(k.as_ref().and_then(|k| k.client_error));
        if c.layered_unreproduced {
            extra_client_error = extra_client_error.or(Some(ClientError::LayeredFormat));
        }
        let n = c.packets.len() as u64;
        let total = n * u64::from(fpp);
        let avail = c.packets.iter().take_while(|p| p.0 + u64::from(p.1) <= file_len.min(c.data_end.unwrap_or(u64::MAX))).count();
        let plan = TrimPlan {
            source: TrimSource::CafPacketTable,
            packets: n,
            frames_per_packet: fpp,
            decoded_frames: total,
            priming: c.priming.max(0) as u64,
            remainder: c.remainder.max(0) as u64,
            valid_frames: trim::through_a_double(c.valid_frames.max(0) as u64),
            has_packet_table: true,
            valid_unknown: c.has_pakt && c.valid_frames == 0,
        };
        let max = c.packets.iter().map(|p| p.1).max().unwrap_or(0);
        Ok(AacFile {
            data,
            file_type: FileType::Caf,
            format,
            fpp,
            sample_rate: rate,
            rate_hz: c.sample_rate,
            channels: c.channels,
            decoder_channels,
            layout,
            asc,
            channel_config,
            pce,
            packets: c.packets,
            unlocated: 0,
            failing: Vec::new(),
            plan,
            audio_data_bytes: c.data_bytes,
            max_packet_size: max,
            multi_block_at: None,
            table_packets: n,
            complete_packets: avail,
            extra_client_error,
            format_flags: c.format_flags,
            byte_limit: c.data_end.unwrap_or(u64::MAX),
            empty_stream: false,
            pair_frames: c.pair_frames,
        })
    }

    fn from_adts(data: Vec<u8>, file_len: u64) -> Result<Self, OpenError> {
        let s = adts::parse_adts(&data)?;
        let mut cfg = s.channel_config;
        if cfg == 2 && s.first_element == Some(0) {
            cfg = 1;
        }
        let asc = asc::Asc {
            aot: 2,
            sfi_raw: s.sfi,
            sample_rate: SAMPLE_RATES[s.sfi as usize],
            channel_config: cfg,
            frame_length_flag: false,
            depends_on_core: false,
            extension_flag: false,
            pce: s.pce.clone(),
            he: None,
            bits_consumed: 16,
        };
        let k = classify(&asc)?;
        let packets = s.frames;
        // a multi-block file answers the property queries as if it had no packets, although its reads deliver the frames before the block
        let table_packets = if s.multi_block_at.is_some() { 0 } else { packets.len() as u64 };
        let mut plan = TrimPlan::untrimmed(table_packets, 1024);
        plan.valid_unknown = s.multi_block_at.is_some(); // the reads are not limited by the (empty) table's length
        Ok(AacFile {
            data,
            file_type: FileType::Adts,
            format: k.format,
            fpp: k.fpp,
            sample_rate: k.rate,
            rate_hz: f64::from(k.rate),
            channels: k.channels,
            decoder_channels: k.channels,
            layout: k.layout,
            asc,
            channel_config: cfg,
            pce: k.pce,
            packets,
            unlocated: 0,
            failing: Vec::new(),
            plan,
            audio_data_bytes: file_len - s.start as u64,
            max_packet_size: if s.multi_block_at.is_some() { 0 } else { s.max_au },
            multi_block_at: s.multi_block_at,
            table_packets,
            complete_packets: usize::MAX,
            extra_client_error: None,
            format_flags: 0,
            byte_limit: u64::MAX,
            empty_stream: false,
            pair_frames: None,
        })
    }

    fn from_loas(data: Vec<u8>, file_len: u64) -> Result<Self, OpenError> {
        let s = loas::parse_loas(data)?;
        let k = classify(&s.asc)?;
        let packets = s.frames;
        let table_packets = packets.len() as u64;
        let packets_empty = packets.is_empty();
        let plan = TrimPlan::untrimmed(table_packets, k.table_fpp);
        Ok(AacFile {
            data: s.payload,
            file_type: FileType::Loas,
            format: k.format,
            fpp: k.fpp,
            sample_rate: k.rate,
            rate_hz: f64::from(k.rate),
            channels: k.channels,
            decoder_channels: k.channels,
            layout: k.layout,
            asc: s.asc,
            channel_config: k.channel_config,
            pce: k.pce,
            packets,
            unlocated: 0,
            failing: s.failing,
            plan,
            audio_data_bytes: file_len - s.start as u64,
            max_packet_size: s.max_frame,
            multi_block_at: None,
            table_packets,
            complete_packets: usize::MAX,
            extra_client_error: k.client_error,
            format_flags: 0,
            byte_limit: u64::MAX,
            empty_stream: packets_empty,
            pair_frames: None,
        })
    }

    fn bytes(&self) -> &[u8] {
        &self.data
    }

    // ---- file data format -------------------------------------------------------------------------------------------------------------

    /// `kAudioFilePropertyFileFormat`.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }
    /// `mFormatID` of the file data format.
    pub fn format_id(&self) -> FormatId {
        self.format
    }
    /// `mSampleRate` (Hz) of the file data format exactly (a CAF `desc` may hold a fractional rate).
    pub fn sample_rate_hz(&self) -> f64 {
        self.rate_hz
    }
    /// `mSampleRate` (Hz) of the file data format: the ASC rate (explicit-rate aware); HE-AAC: the SBR output rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    /// `mChannelsPerFrame` of the file data format.
    pub fn channels(&self) -> u32 {
        self.channels
    }
    /// `mFramesPerPacket`.
    pub fn frames_per_packet(&self) -> u32 {
        self.fpp
    }
    /// `mFormatFlags` of the file data format.
    pub fn format_flags(&self) -> u32 {
        self.format_flags
    }
    /// CoreAudio layout tag and channel labels (`None` for non-LC formats whose tag Apple reports as 0).
    pub fn layout(&self) -> Option<&'static Layout> {
        self.layout
    }

    // ---- packets ----------------------------------------------------------------------------------------------------------------------

    /// A stream without packets whose reads end at once (a LOAS file whose first frame is cut) rather than fail.
    pub fn empty_stream(&self) -> bool {
        self.empty_stream
    }
    /// Packets the reads can take (`kAudioFilePropertyAudioDataPacketCount` unless the file is a multi-block ADTS one, whose properties see none).
    pub fn packet_count(&self) -> usize {
        self.packets.len()
    }
    /// `kAudioFilePropertyAudioDataByteCount`: sum of the sample sizes (MP4, CAF) or the file size (ADTS, LOAS).
    #[cfg(test)]
    pub fn audio_data_bytes(&self) -> u64 {
        self.audio_data_bytes
    }
    /// `kAudioFilePropertyMaximumPacketSize`.
    #[cfg(test)]
    pub fn max_packet_size(&self) -> u32 {
        self.max_packet_size
    }
    /// Declared (offset, size) of packet `i` in the file image (LOAS: in the repacked payload).
    #[cfg(test)]
    pub fn packet_extent(&self, i: usize) -> Option<(u64, u32)> {
        self.packets.get(i).copied()
    }
    /// Access unit `i` as `AudioFileReadPacketData` + the converter's input check would hand it to the decoder.
    pub fn packet(&self, i: usize) -> Result<&[u8], ReadError> {
        let (off, len) = *self.packets.get(i).ok_or(ReadError::PacketBeyondEof)?;
        if i < self.unlocated {
            return Err(ReadError::InvalidPacketOffset);
        }
        if !self.failing.is_empty() && self.failing.binary_search(&i).is_ok() {
            return Err(ReadError::BadFrame);
        }
        if len as usize > AU_BYTES_PER_CHANNEL * self.decoder_channels.max(1) as usize {
            return Err(ReadError::PacketTooLarge);
        }
        let b = self.bytes();
        let end = off.checked_add(u64::from(len)).ok_or(ReadError::PacketBeyondEof)?;
        if end > (b.len() as u64).min(self.byte_limit) {
            return Err(ReadError::PacketBeyondEof);
        }
        Ok(&b[off as usize..end as usize])
    }
    /// ADTS only: the open-time consistency check between the header's channel_configuration and the FIRST access unit needs the element
    /// sequence (`0` SCE, `1` CPE, `3` LFE, in stream order; PCE/DSE/FIL excluded) that only the bit parser (`decoder::bits`) can produce; `parse`
    /// itself only compares the first element id.  Apple accepts exactly: cfg 1 `[SCE]`; cfg 2 `[CPE]` or `[SCE]` (reported as mono);
    /// cfg 3 `[SCE CPE]`; 4 `[SCE CPE SCE]`; 5 `[SCE CPE CPE]`; 6 `[SCE CPE CPE LFE]`; 7 `[SCE CPE CPE CPE LFE]`; everything else `'fmt?'`.
    /// Other containers (and ADTS with channel_configuration 0) always return `Ok`.
    pub fn check_adts_elements(&self, element_ids: &[u8]) -> Result<(), OpenError> {
        if self.file_type != FileType::Adts || self.pce.is_some() {
            return Ok(());
        }
        let expect: &[u8] = match self.channel_config {
            1 => &[0],
            2 => {
                if element_ids == [0] {
                    return Ok(());
                }
                &[1]
            }
            3 => &[0, 1],
            4 => &[0, 1, 0],
            5 => &[0, 1, 1],
            6 => &[0, 1, 1, 3],
            7 => &[0, 1, 1, 1, 3],
            _ => return Err(OpenError::UnsupportedDataFormat),
        };
        if element_ids == expect { Ok(()) } else { Err(OpenError::UnsupportedDataFormat) }
    }
    /// `kAudioFilePropertyPacketSizeUpperBound`: the largest declared packet size (MP4, CAF: oversize and cut ones included), 768 bytes per channel
    /// for the framed streams (ADTS, LOAS), whatever their frames hold.
    pub fn packet_size_upper_bound(&self) -> u32 {
        match self.file_type {
            FileType::Adts | FileType::Loas => 768 * self.decoder_channels.max(1),
            _ => self.max_packet_size,
        }
    }
    /// ADTS: index of the first multi-block frame; reads that reach it fail `'dta?'`.
    pub fn multi_block_at(&self) -> Option<usize> {
        self.multi_block_at
    }

    /// What one request of the converter for `count` packets starting at packet `start` returns: the number of packets and the failure of
    /// the batch (reading a batch fails as a whole when it holds a packet the converter cannot take).  The container decides:
    /// * MP4 and a CAF whose data holds every packet: `min(count, packets left)` packets; an oversize packet, one that lies (partly) outside the
    ///   file, or one that cannot be located fails the batch;
    /// * a CAF cut inside the range of its packet table: a request that ends inside the table returns `count` packets and fails when one of them
    ///   is incomplete; a request that reaches past the table returns only the complete packets, none is the (silent, final) end of the stream;
    /// * ADTS with a multi-block frame: the frame table is scanned `start + count` frames far (one more for `count >= 2`), `'dta?'` as soon as
    ///   that reaches the block.
    pub fn pull(&self, start: usize, count: usize) -> (usize, Option<ReadError>) {
        let total = self.packets.len();
        if let Some(block) = self.multi_block_at {
            if start + count + usize::from(count >= 2) > block {
                return (0, Some(ReadError::InvalidFile));
            }
        }
        let checked = |n: usize| (n, (start..start + n).find_map(|i| self.packet(i).err()));
        if self.file_type == FileType::Caf && self.complete_packets < total {
            if start >= total {
                return (0, None);
            }
            if start + count <= total {
                return checked(count); // an incomplete packet in the range fails the batch
            }
            return checked(self.complete_packets.saturating_sub(start).min(total - start));
        }
        checked(count.min(total.saturating_sub(start)))
    }

    // ---- frame accounting -------------------------------------------------------------------------------------------------------------

    /// Priming / valid / remainder, `FileLengthFrames`, delivered frame count.
    pub fn trim_plan(&self) -> TrimPlan {
        self.plan
    }
    /// `kAudioFilePropertyBitRate`: the audio data bytes over the duration of the packets,
    /// `audio_data_bytes * 8 / (packets * frames_per_packet / rate)` in double precision cut to `u32`, saturating at `u32::MAX` (the property is a
    /// `u32`: AudioToolbox answers 4294967295 for a packet table that claims more); 0 for a file without packets.
    pub fn bitrate(&self) -> u32 {
        let frames = if self.fpp == 0 { self.pair_frames.unwrap_or(0) } else { self.table_packets * u64::from(self.fpp) };
        bitrate_of(self.audio_data_bytes, frames, self.sample_rate)
    }
    /// Whether the bit rate property answers: it fails (`'dta?'`) for a CAF whose `desc` has no frames per packet and whose packet table holds no
    /// frames to divide by.
    pub fn bitrate_defined(&self) -> bool {
        self.fpp != 0 || self.pair_frames.is_some_and(|f| f != 0)
    }
    /// `kExtAudioFileProperty_FileLengthFrames`.
    pub fn file_length_frames(&self) -> u64 {
        let v = self.plan.file_length_frames();
        if matches!(self.format, FormatId::AacHe | FormatId::AacHeV2) { v.saturating_mul(2) } else { v }
    }

    // ---- client side ------------------------------------------------------------------------------------------------------------------

    /// `kExtAudioFileProperty_ClientDataFormat` acceptance for the AAC-LC decoder + the configuration `decoder::bits` is created with.
    pub fn decoder_config(&self) -> Result<DecoderConfig, ClientError> {
        if let Some(e) = self.extra_client_error {
            return Err(e);
        }
        if self.format != FormatId::Aac {
            return Err(ClientError::NotAacLc);
        }
        if self.asc.frame_length_flag {
            return Err(ClientError::FrameLength960);
        }
        if self.sample_rate >= 3_000_000 {
            return Err(ClientError::InvalidRate);
        }
        if self.sample_rate == 7350 || self.sample_rate > 110_851 {
            return Err(ClientError::UnsupportedRate);
        }
        if self.pce.as_ref().is_some_and(|p| !p.cc.is_empty()) {
            return Err(ClientError::NoDecoder);
        }
        Ok(DecoderConfig {
            sample_rate: self.sample_rate,
            sfi: sfb_table_index(self.sample_rate),
            channel_config: self.channel_config,
            channels: self.decoder_channels as u8,
            pce: self.pce.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::aac::decoder::testutil::*;

    /// `(audio data bytes, packets, frames per packet, rate, bitrate AudioToolbox reported)`: a recorded selection of MP4 files (every case
    /// whose quotient is an exact integer, the largest fractional parts, both extremes, one file per rate and container, the empty file), and the
    /// saturation observed with oversized packets.
    const BITRATES: &[(u64, u64, u32, u32, u32)] = &[
    (0, 0, 1024, 44100, 0), // m4af aac
    (18, 3, 1024, 44100, 2067), // m4af aac
    (152, 3, 1024, 48000, 19000), // m4af aac
    (223, 3, 1024, 48000, 27875), // m4af aac
    (295, 3, 1024, 48000, 36875), // m4af aac
    (336, 3, 1024, 48000, 42000), // m4af aac
    (403, 3, 1024, 48000, 50375), // m4af aac
    (416, 3, 1024, 48000, 52000), // m4af aac
    (436, 3, 1024, 48000, 54500), // m4af aac
    (489, 3, 1024, 48000, 61125), // m4af aac
    (503, 3, 1024, 48000, 62875), // m4af aac
    (532, 3, 1024, 48000, 66500), // m4af aac
    (736, 3, 1024, 44100, 84525), // m4af aac
    (806, 3, 1024, 48000, 100750), // m4af aac
    (893, 3, 1024, 48000, 111625), // m4af aac
    (921, 3, 1024, 48000, 115125), // m4af aac
    (976, 3, 1024, 48000, 122000), // m4af aac
    (1003, 5, 1024, 48000, 75225), // m4af aac
    (1040, 4, 1024, 48000, 97500), // m4af aac
    (1050, 175, 1024, 44100, 2067), // m4af aac
    (1557, 3, 1024, 48000, 194625), // m4af aac
    (1569, 3, 1024, 48000, 196125), // m4af aac
    (1622, 3, 1024, 48000, 202750), // m4af aac
    (2296, 5, 1024, 48000, 172200), // m4af aac
    (2402, 6, 1024, 48000, 150125), // m4af aac
    (3936, 7, 1024, 44100, 193725), // m4af aac
    (5870, 15, 1024, 48000, 146750), // m4af aac
    (8094, 18, 1024, 48000, 168625), // m4af aac
    (9538, 9, 1024, 22050, 182563), // m4af aac
    (12597, 24, 1024, 44101, 180839), // m4af aac
    (14426, 23, 1024, 44100, 216095), // m4af aac
    (14943, 24, 1024, 50000, 243212), // m4af aac
    (17948, 21, 1024, 48000, 320500), // m4af aac
    (18977, 283, 1024, 96000, 50292), // m4af aac
    (20659, 27, 1024, 8000, 47821), // m4af aac
    (23732, 48, 1024, 44100, 170341), // m4af aac
    (24896, 66, 1024, 22050, 64980), // adts aac
    (25406, 30, 1024, 48000, 317575), // m4af aac
    (25836, 50, 1024, 16000, 64590), // m4af aac
    (25912, 38, 1024, 12000, 63927), // m4af aac
    (25977, 35, 1024, 11025, 63927), // m4af aac
    (28222, 40, 1024, 64000, 352775), // m4af aac
    (28653, 45, 1024, 48000, 238775), // m4af aac
    (30815, 432, 1024, 88200, 49151), // m4af aac
    (36058, 131, 1024, 44100, 94832), // MooV aac
    (37376, 48, 1024, 44100, 268275), // m4af aac
    (41825, 37, 1024, 7350, 64909), // m4af aac
    (49747, 132, 1024, 44100, 129843), // mp4f aac
    (50549, 132, 1024, 44100, 131937), // loas aac
    (50671, 132, 1024, 44100, 132255), // adts aac
    (60571, 314, 1024, 64000, 96450), // m4af aac
    (61530, 138, 1024, 44100, 153615), // m4af aac
    (72512, 175, 1024, 44100, 142758), // m4af aac
    (76834, 38, 1024, 16000, 252743), // m4af aac
    (94436, 48, 1024, 44100, 677836), // m4af aac
    (94688, 147, 1024, 44100, 221925), // m4af aac
    (101342, 48, 1024, 96000, 1583468), // m4af aac
    (128113, 97, 1024, 24000, 247641), // m4af aac
    (129952, 175, 1024, 44100, 255843), // m4af aac
    (139702, 174, 1024, 44100, 276618), // m4af aac
    (179187, 273, 1024, 48000, 246135), // m4af aac
    (195010, 190, 1024, 32000, 256592), // m4af aac
    (244849, 378, 1024, 48000, 242905), // m4af aac
    (392196, 519, 1024, 44100, 260354), // m4af aac
        // the exact ratio is 205875: the duration is rounded first
        (7_686, 14, 1024, 48000, 205_874),
        (12_400_000, 1, 1024, 44100, 4_272_187_500),
        (12_466_000, 1, 1024, 44100, 4_294_926_562),
        (12_470_000, 1, 1024, 44100, u32::MAX),
        (40_000_000, 1, 1024, 44100, u32::MAX),
    ];

    #[test]
    fn the_bitrate_rule_reproduces_audiotoolbox() {
        for &(bytes, packets, fpp, rate, expected) in BITRATES {
            assert_eq!(bitrate_of(bytes, packets * u64::from(fpp), rate), expected, "{bytes} bytes in {packets} packets of {fpp} frames at {rate} Hz");
        }
    }

    fn open(file: Vec<u8>, hint: Option<&str>) -> Result<AacFile, OpenError> {
        AacFile::parse_with_hint(file, hint)
    }

    #[test]
    fn the_fixtures_report_what_audiotoolbox_reports() {
        let f = open(STEREO_M4A.to_vec(), Some("m4a")).unwrap();
        assert_eq!((f.file_type(), f.format_id(), f.sample_rate(), f.channels(), f.frames_per_packet()), (FileType::M4a, FormatId::Aac, 44100, 2, 1024));
        assert_eq!((f.file_length_frames(), f.packet_count(), f.audio_data_bytes(), f.max_packet_size(), f.bitrate()), (8820, 11, 3009, 404, 94244));
        let adts = open(STEREO_ADTS.to_vec(), Some("aac")).unwrap();
        assert_eq!((adts.file_type(), adts.file_length_frames(), adts.bitrate()), (FileType::Adts, 11264, 96656));
        let caf = open(STEREO_CAF.to_vec(), None).unwrap();
        assert_eq!((caf.file_type(), caf.file_length_frames(), caf.bitrate()), (FileType::Caf, 8820, 94244));
    }

    #[test]
    fn leading_id3_and_id3v1_blocks_are_skipped_by_adts_only() {
        let tag = |size: usize| {
            let mut t = b"ID3\x04\x00\x00".to_vec();
            t.extend_from_slice(&[(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
            t.resize(10 + size, 0);
            t
        };
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, 0);
        let plain = open(STEREO_ADTS.to_vec(), None).unwrap();
        for prefix in [tag(300), tag(0), [tag(50), tag(70)].concat(), v1.clone(), [tag(40), v1.clone()].concat(), [v1.clone(), tag(40)].concat()] {
            let f = open([prefix.clone(), STEREO_ADTS.to_vec()].concat(), None).unwrap();
            assert_eq!((f.packet_count(), f.file_length_frames()), (plain.packet_count(), plain.file_length_frames()));
            assert_eq!(f.audio_data_bytes(), STEREO_ADTS.len() as u64);
        }
        // a tag with a footer flag is not skipped (nor is a version byte 0xff a tag)
        assert_eq!(open([tag(40), vec![0; 10], STEREO_ADTS.to_vec()].concat(), Some("aac")).err(), Some(OpenError::NoSync));
        assert_eq!(open([b"ID3\xff\x00\x00\0\0\0\0\0\0".to_vec(), STEREO_ADTS.to_vec()].concat(), Some("aac")).err(), Some(OpenError::NoSync));
        // nothing left behind the tags: an empty file
        assert_eq!(open(tag(10), Some("aac")).err(), Some(OpenError::EmptyFile));
    }

    #[test]
    fn name_hints_commit_to_the_adts_and_loas_types() {
        for hint in ["aac", "adts", "AAC"] {
            assert_eq!(open(STEREO_M4A.to_vec(), Some(hint)).err(), Some(OpenError::NoSync));
            assert!(open(STEREO_ADTS.to_vec(), Some(hint)).is_ok());
        }
        for hint in ["loas", "latm"] {
            assert_eq!(open(STEREO_ADTS.to_vec(), Some(hint)).err(), Some(OpenError::UnsupportedDataFormat));
        }
        // an MPEG audio sync word under an ADTS name
        assert_eq!(open(vec![0xff, 0xfb, 0x90, 0x00, 0, 0, 0, 0], Some("aac")).err(), Some(OpenError::WrongLayer));
    }

    #[test]
    fn adts_frame_lengths_that_lie_end_the_scan() {
        let aus = access_units(STEREO_M4A);
        let good = adts(&aus, 4, 2);
        assert_eq!(open(good.clone(), None).unwrap().packet_count(), 11);
        // the last frame announces more bytes than the file holds: it is dropped
        assert_eq!(open(good[..good.len() - 1].to_vec(), None).unwrap().packet_count(), 10);
        // a frame length below the header size ends the scan there
        let mut short = good.clone();
        let second = 7 + aus[0].len();
        short[second + 3] &= !3;
        short[second + 4] = 0;
        short[second + 5] &= 0x1f;
        assert_eq!(open(short, None).unwrap().packet_count(), 1);
        // a single frame that is cut: nothing to read at all
        assert_eq!(open(good[..8].to_vec(), Some("aac")).err(), Some(OpenError::Unspecified(Unspecified::NoFrames)));
        // junk after the last frame is ignored
        assert_eq!(open([good, vec![0xde; 40]].concat(), None).unwrap().packet_count(), 11);
    }

    #[test]
    fn mp4_atoms_that_lie_are_refused_with_apples_statuses() {
        let aus = access_units(STEREO_M4A);
        let refs: Vec<&[u8]> = aus.iter().map(|a| a.as_slice()).collect();
        let sizes: Vec<u32> = aus.iter().map(|a| a.len() as u32).collect();
        let good = mp4_with(&refs, &Mp4Parts { sizes: sizes.clone(), constant: None, asc: vec![0x12, 0x10], entry: *b"mp4a" });
        assert!(open(good.clone(), None).is_ok());
        let moov_at = good.windows(4).position(|w| w == b"moov").unwrap() - 4;
        // a moov atom that runs past the end of the file is read as far as the file goes; a cut that leaves no sound track is no file
        let mut clamped = good.clone();
        clamped[moov_at..moov_at + 4].copy_from_slice(&0x00ff_ffffu32.to_be_bytes());
        assert_eq!(open(clamped, None).unwrap().packet_count(), 11);
        let mut short_moov = good.clone();
        let trak_at = short_moov.windows(4).position(|w| w == b"trak").unwrap() - 4;
        short_moov[moov_at..moov_at + 4].copy_from_slice(&((trak_at + 20 - moov_at) as u32).to_be_bytes());
        assert_eq!(open(short_moov, None).err(), Some(OpenError::InvalidFile));
        // a 64-bit atom size of u64::MAX
        let mut huge = good.clone();
        huge[moov_at..moov_at + 4].copy_from_slice(&1u32.to_be_bytes());
        huge.splice(moov_at + 8..moov_at + 8, u64::MAX.to_be_bytes());
        assert_eq!(open(huge, None).err(), Some(OpenError::InvalidFile));
        // atom sizes below their header, truncations anywhere
        for cut in (0..good.len()).step_by(37) {
            let _ = open(good[..cut].to_vec(), None);
        }
        let mut tiny = good.clone();
        tiny[moov_at..moov_at + 4].copy_from_slice(&3u32.to_be_bytes());
        assert!(open(tiny, None).is_err());
        // the file is shorter than the samples it declares
        let mut padded = sizes.clone();
        padded[0] = 1_000_000;
        let lie = mp4_with(&refs, &Mp4Parts { sizes: padded, constant: None, asc: vec![0x12, 0x10], entry: *b"mp4a" });
        assert_eq!(open(lie, None).err(), Some(OpenError::Unspecified(Unspecified::FileShorterThanAudio)));
    }

    /// Sample-table and header damage the recorded files show (macOS 27.2): the first run covers the first chunk whatever its `first_chunk` says,
    /// samples the runs do not reach stay in the table but cannot be read, a map that cannot hold its entries locates nothing, and `mdhd` and `esds`
    /// must be well formed.
    #[test]
    fn mp4_tables_and_headers_follow_the_recorded_rules() {
        let patched = |kind: &[u8; 4], at: usize, bytes: &[u8]| {
            let mut file = STEREO_M4A.to_vec();
            let atom = file.windows(4).position(|w| w == kind).unwrap() + 4; // the body
            file[atom + at..atom + at + bytes.len()].copy_from_slice(bytes);
            file
        };
        // stsc body: version/flags, entry count, then (first chunk, samples per chunk, description) per run
        let first_chunk = open(patched(b"stsc", 8, &[0x01, 0x00, 0x00, 0x01]), None).unwrap();
        assert_eq!((first_chunk.packet_count(), first_chunk.packet(10).is_ok()), (11, true));
        let short = open(patched(b"stsc", 12, &[0, 0, 0, 10]), None).unwrap();
        assert_eq!((short.packet_count(), short.bitrate(), short.audio_data_bytes(), short.trim_plan().packets), (11, 94_244, 3009, 11));
        // the packet no chunk reaches is read from the start of the file: not an access unit (the last one: the stream ends silently, see the replay)
        assert_eq!(short.packet_extent(10).map(|e| e.0), Some(mp4::UNPLACED_OFFSET));
        let empty = open(patched(b"stsc", 4, &[0x01, 0, 0, 1]), None).unwrap();
        assert_eq!((empty.packet_count(), empty.packet(0).err()), (11, Some(ReadError::InvalidPacketOffset)));
        // mdhd: version 0 or 1, and the fields of the version
        for version in [1u8, 2, 0x80] {
            assert_eq!(open(patched(b"mdhd", 0, &[version]), None).err(), Some(OpenError::Unspecified(Unspecified::BadStructure)), "version {version}");
        }
        // esds: the lengths of the descriptors must hold
        for (at, value) in [(8, 0xddu8), (16, 0xeb), (34, 0x7f)] {
            assert_eq!(open(patched(b"esds", at, &[value]), None).err(), Some(OpenError::Unspecified(Unspecified::BadEsds)), "byte {at}");
        }
    }

    #[test]
    fn stsz_bombs_are_refused_without_allocating_their_claim() {
        let aus = access_units(STEREO_M4A);
        let refs: Vec<&[u8]> = aus.iter().map(|a| a.as_slice()).collect();
        let parts = |constant: Option<(u32, u32)>, sizes: Vec<u32>| Mp4Parts { sizes, constant, asc: vec![0x12, 0x10], entry: *b"mp4a" };
        // a count larger than the table that follows
        let mut file = mp4_with(&refs, &parts(None, aus.iter().map(|a| a.len() as u32).collect()));
        let stsz = file.windows(4).position(|w| w == b"stsz").unwrap() + 4;
        file[stsz + 8..stsz + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(open(file, None).err(), Some(OpenError::Unspecified(Unspecified::BadStructure)));
        // a constant sample size times a count that exceeds the file
        let file = mp4_with(&refs, &parts(Some((4096, u32::MAX)), Vec::new()));
        assert_eq!(open(file, None).err(), Some(OpenError::Unspecified(Unspecified::BadStructure)));
        // a count that the file could hold but the decoder does not admit
        let big = vec![0u8; mp4::MAX_PACKETS + 1];
        let file = mp4_with(&[&big], &parts(Some((1, mp4::MAX_PACKETS as u32 + 1)), Vec::new()));
        assert_eq!(open(file, None).err(), Some(OpenError::PacketTableTooLarge));
    }
}
