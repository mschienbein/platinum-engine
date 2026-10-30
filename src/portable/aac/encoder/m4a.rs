//! File-level driver of the encoder: the `AudioSpecificConfig` of a stream's format and the `.m4a` that AudioToolbox's ExtAudioFile (`m4af`) writes
//! around its access units, byte for byte ([`write_file`], on top of [`crate::portable::aac::mp4`]).
//!
//! The file is produced from the sizes of the access units and a `Read` of their bytes, so a caller that spools the units to disk as the encoder
//! emits them ([`AacEncoder::drain_access_units`](crate::portable::aac::encoder::AacEncoder::drain_access_units)) never holds the stream in memory.

use std::io::{Read, Write};

use crate::portable::aac::bits::write_pce_asc_bits;
use crate::portable::aac::encoder::config::{layout_pce, ENGINE_BRAT};
use crate::portable::aac::fb::input::au_count;
use crate::portable::aac::mp4::{asc_simple, asc_with_pce, channel_configuration, default_bitrate, write_empty_m4a, write_m4a, FileParams, Mp4Error, WriteError};

/// `AudioSpecificConfig` of the file (`esds`) for `(rate, channels)`: `channelConfiguration` per [`channel_configuration`]; 4 channels carry the PCE inside the ASC
/// (`channelConfiguration` 0), 7 channels signal chcfg 11 and carry their PCE in-band in AU 0 only. `None` for a rate without a sampling-frequency index or a
/// channel count outside 1..=8.
pub fn audio_specific_config(rate: u32, channels: u32) -> Option<Vec<u8>> {
    if channel_configuration(channels)? == 0 {
        let pce = layout_pce(rate, channels as usize)?;
        let (bytes, nbits) = write_pce_asc_bits(&pce);
        asc_with_pce(rate, &bytes, nbits)
    } else {
        asc_simple(rate, channels)
    }
}

/// Why `(rate, channels)` has no file layout: a channel count outside 1..=8, else the rate.
fn unsupported(rate: u32, channels: u32) -> Mp4Error {
    if channel_configuration(channels).is_none() { Mp4Error::UnsupportedChannels(channels) } else { Mp4Error::UnsupportedRate(rate) }
}

/// Write the complete `.m4a` of a stream of `frames` client frames at `rate` Hz x `channels` channels whose `au_sizes.len()` access units (`au_sizes` bytes each,
/// read back to back from `mdat`) the encoder emitted; `clock` is the close-time second since 1904-01-01 stamped into mvhd/tkhd/mdhd.
///
/// Zero frames written is the file ExtAudioFile leaves when it never saw a frame: `ftyp` + `moov` with empty tables, the esds bitrate being Apple's
/// default `'brat'` of the format ([`default_bitrate`]), not 256000. It exists for every format AudioToolbox creates a converter for, including the
/// ones that refuse 256 kbit/s (no `'brat'` is ever applied to an empty file). A stream with frames needs exactly `au_count(frames)` access units.
pub fn write_file<W: Write, R: Read>(out: &mut W, mdat: &mut R, au_sizes: &[u32], rate: u32, channels: u32, frames: u64, clock: u32) -> Result<(), WriteError> {
    let asc = audio_specific_config(rate, channels).ok_or_else(|| unsupported(rate, channels))?;
    let mut params = FileParams { sample_rate: rate, input_frames: frames, asc: &asc, clock, avg_bitrate: ENGINE_BRAT };
    let inconsistent = Mp4Error::InconsistentFrameCount { frames, access_units: au_sizes.len() };
    if frames == 0 {
        if !au_sizes.is_empty() {
            return Err(inconsistent.into());
        }
        params.avg_bitrate = default_bitrate(rate, channels).ok_or_else(|| unsupported(rate, channels))?;
        return write_empty_m4a(out, &params);
    }
    if au_sizes.len() as u64 != au_count(frames) {
        return Err(inconsistent.into());
    }
    write_m4a(out, mdat, au_sizes, &params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::aac::encoder::AacEncoder;
    use crate::portable::aac::encoder::config::NOMINAL_RATES;

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn find(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).position(|w| w == needle).expect("pattern present")
    }

    #[test]
    fn the_asc_follows_the_channel_configuration() {
        assert_eq!(audio_specific_config(44100, 2).unwrap(), [0x12, 0x10]);
        assert_eq!(audio_specific_config(48000, 5).unwrap(), [0x11, 0xA8]);
        assert_eq!(audio_specific_config(44100, 7).unwrap(), [0x12, 0x58], "7 channels signal chcfg 11");
        // 4 channels: chcfg 0 followed by the PCE
        let four = audio_specific_config(44100, 4).unwrap();
        assert_eq!(four[..2], [0x12, 0x00]);
        assert!(four.len() > 2);
        assert!(audio_specific_config(44100, 0).is_none() && audio_specific_config(44100, 9).is_none());
        assert!(audio_specific_config(7350, 2).is_some() && audio_specific_config(44101, 2).is_none());
    }

    #[test]
    fn an_empty_stream_is_ftyp_and_moov_with_apples_default_bitrate_for_every_format() {
        for rate in NOMINAL_RATES {
            for channels in 1..=8u32 {
                let mut file = Vec::new();
                write_file(&mut file, &mut std::io::empty(), &[], rate, channels, 0, 7).unwrap();
                assert!(!file.windows(4).any(|w| w == b"mdat" || w == b"free" || w == b"udta"), "{rate} Hz x {channels}");
                assert_eq!(&file[4..8], b"ftyp");
                let e = find(&file, b"esds") + 4 + 4 + 5 + 3 + 5 + 2 + 3;
                let want = default_bitrate(rate, channels).unwrap();
                assert_eq!((be32(&file, e), be32(&file, e + 4)), (want, want), "{rate} Hz x {channels}");
                assert_eq!(be32(&file, find(&file, b"mvhd") + 16), rate);
            }
        }
    }

    #[test]
    fn a_stream_needs_exactly_the_access_units_its_frame_count_implies() {
        let (rate, channels, frames) = (44100u32, 2u32, 5000usize);
        let pcm: Vec<f32> = (0..frames * channels as usize).map(|i| 0.2 * (i as f32 * 0.01).sin()).collect();
        let mut enc = AacEncoder::new(rate, channels).unwrap();
        enc.push_interleaved(&pcm).unwrap();
        enc.finish().unwrap();
        let units = enc.take_access_units();
        let sizes: Vec<u32> = units.iter().map(|u| u.len() as u32).collect();
        let bytes: Vec<u8> = units.concat();
        let mut file = Vec::new();
        write_file(&mut file, &mut &bytes[..], &sizes, rate, channels, frames as u64, 9).unwrap();
        // ftyp, moov, free, mdat: the media data holds the access units back to back
        let mdat = find(&file, b"mdat") + 4;
        assert_eq!(&file[mdat..mdat + bytes.len()], &bytes[..]);
        assert_eq!(be32(&file, find(&file, b"esds") + 4 + 4 + 5 + 3 + 5 + 2 + 3 + 4), 256_000, "the average bitrate is the 'brat' of the stream");
        for (given_frames, given_sizes) in [(frames as u64, &sizes[1..]), (frames as u64 + 2000, &sizes[..]), (0, &sizes[..])] {
            let err = write_file(&mut Vec::new(), &mut &bytes[..], given_sizes, rate, channels, given_frames, 9).unwrap_err();
            assert!(matches!(err, WriteError::Layout(Mp4Error::InconsistentFrameCount { .. })), "{given_frames}: {err:?}");
        }
        let err = write_file(&mut Vec::new(), &mut &bytes[..], &sizes, 44101, channels, frames as u64, 9).unwrap_err();
        assert!(matches!(err, WriteError::Layout(Mp4Error::UnsupportedRate(44101))), "{err:?}");
    }

    #[test]
    fn an_invalid_channel_count_is_not_reported_as_a_rate_problem() {
        for channels in [0u32, 9, 64] {
            let err = write_file(&mut Vec::new(), &mut std::io::empty(), &[], 44100, channels, 0, 9).unwrap_err();
            assert!(matches!(err, WriteError::Layout(Mp4Error::UnsupportedChannels(c)) if c == channels), "{channels}: {err:?}");
            assert!(!err.to_string().is_empty());
        }
        let err = write_file(&mut Vec::new(), &mut std::io::empty(), &[], 44101, 2, 0, 9).unwrap_err();
        assert!(matches!(err, WriteError::Layout(Mp4Error::UnsupportedRate(44101))), "{err:?}");
    }
}
