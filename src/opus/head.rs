//! Ogg Opus header packets (RFC 7845) and the Opus TOC byte (RFC 6716 s3.1).

use super::OpusError;

pub const OPUS_HEAD_MAGIC: &[u8; 8] = b"OpusHead";
pub const OPUS_TAGS_MAGIC: &[u8; 8] = b"OpusTags";
/// RFC 7845 s6: demuxers should refuse audio packets above this per-stream size.
pub const MAX_AUDIO_PACKET: usize = 61_440;
/// Largest comment header accepted or written. RFC 7845 s5.2 permits refusing
/// anything above 125,829,120 bytes; this is deliberately tighter so a hostile
/// header cannot force a large allocation.
pub const MAX_COMMENT_HEADER: usize = 64 * 1024 * 1024;
/// More comments than this are refused: each costs a heap allocation.
pub const MAX_COMMENTS: usize = 1 << 20;
/// Samples per Opus packet are at most 120 ms at 48 kHz.
pub const MAX_PACKET_SAMPLES: u32 = 5760;

/// The identification header of a mapping-family-0 stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
    pub version: u8,
    pub channels: u8,
    pub pre_skip: u16,
    /// Informational: the sample rate of the audio before it was encoded.
    pub input_sample_rate: u32,
    /// Q7.8 dB, applied by the decoder.
    pub output_gain: i16,
}
impl OpusHead {
    pub fn parse(packet: &[u8]) -> Result<Self, OpusError> {
        if packet.len() < 8 || &packet[..8] != OPUS_HEAD_MAGIC {
            return Err(OpusError::NotOpus("the first packet is not an OpusHead"));
        }
        if packet.len() < 19 { return Err(OpusError::Invalid("the OpusHead packet is truncated")); }
        let version = packet[8];
        if version >= 16 { return Err(OpusError::Unsupported("OpusHead major version is newer than this reader")); }
        let channels = packet[9];
        if channels == 0 { return Err(OpusError::Invalid("OpusHead declares zero channels")); }
        let family = packet[18];
        if family != 0 { return Err(OpusError::Unsupported("only channel mapping family 0 (mono or stereo) is supported")); }
        if channels > 2 { return Err(OpusError::Unsupported("channel mapping family 0 allows only mono or stereo")); }
        Ok(Self {
            version, channels,
            pre_skip: u16::from_le_bytes([packet[10], packet[11]]),
            input_sample_rate: u32::from_le_bytes(packet[12..16].try_into().expect("four bytes")),
            output_gain: i16::from_le_bytes([packet[16], packet[17]]),
        })
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(19);
        out.extend_from_slice(OPUS_HEAD_MAGIC);
        out.push(self.version);
        out.push(self.channels);
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        out.extend_from_slice(&self.output_gain.to_le_bytes());
        out.push(0);
        out
    }
}

/// Duration of an Opus packet in 48 kHz samples, from its TOC byte(s). Returns
/// `None` for packets that RFC 6716 s3.4 declares malformed at this level
/// (empty, a code-3 packet without a frame count, zero frames, or more than
/// 120 ms in total).
pub fn packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame: u32 = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => {
            let count = u32::from(*packet.get(1)? & 0x3f);
            if count == 0 { return None; }
            count
        }
    };
    let total = frame.checked_mul(frames)?;
    (total <= MAX_PACKET_SAMPLES).then_some(total)
}

/// One `NAME=value` user comment, kept as raw bytes so it round-trips verbatim.
pub type Comment = Vec<u8>;

/// A parsed OpusTags packet. Comments are retained byte-for-byte and in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommentHeader {
    pub vendor: Vec<u8>,
    pub comments: Vec<Comment>,
    /// Bytes after the last comment (RFC 7845 s5.2): empty, or padding/binary data.
    pub trailing: Vec<u8>,
}
impl CommentHeader {
    /// Check the structure of an OpusTags packet without allocating for its
    /// contents; returns the number of user comments.
    pub fn validate(packet: &[u8]) -> Result<u32, OpusError> {
        Self::walk(packet, |_| {}).map(|(count, _)| count)
    }

    /// Walk the packet: the callback sees each comment's byte range. Returns the
    /// comment count and the offset where the user comments end.
    fn walk(packet: &[u8], mut each: impl FnMut(std::ops::Range<usize>)) -> Result<(u32, usize), OpusError> {
        if packet.len() > MAX_COMMENT_HEADER { return Err(OpusError::Limit("OpusTags header exceeds the supported size")); }
        if packet.len() < 16 || &packet[..8] != OPUS_TAGS_MAGIC {
            return Err(OpusError::Invalid("second packet is not an OpusTags header"));
        }
        let take_u32 = |cursor: &mut usize| -> Result<usize, OpusError> {
            let end = cursor.checked_add(4).filter(|end| *end <= packet.len()).ok_or(OpusError::Invalid("OpusTags header is truncated"))?;
            let value = u32::from_le_bytes(packet[*cursor..end].try_into().expect("four bytes"));
            *cursor = end;
            Ok(value as usize)
        };
        let mut cursor = 8usize;
        let vendor_len = take_u32(&mut cursor)?;
        if vendor_len > packet.len() - cursor { return Err(OpusError::Invalid("OpusTags vendor string is truncated")); }
        cursor += vendor_len;
        let count = take_u32(&mut cursor)?;
        // Each comment needs at least its four length bytes: a count that cannot
        // fit is rejected before anything is allocated for it.
        if count > (packet.len() - cursor) / 4 { return Err(OpusError::Invalid("OpusTags comment count exceeds the header size")); }
        if count > MAX_COMMENTS { return Err(OpusError::Limit("OpusTags header has too many comments")); }
        for _ in 0..count {
            let len = take_u32(&mut cursor)?;
            if len > packet.len() - cursor { return Err(OpusError::Invalid("OpusTags comment is truncated")); }
            each(cursor..cursor + len);
            cursor += len;
        }
        Ok((count as u32, cursor))
    }

    pub fn parse(packet: &[u8]) -> Result<Self, OpusError> {
        let mut comments = Vec::new();
        let (_, end) = Self::walk(packet, |range| comments.push(packet[range].to_vec()))?;
        let vendor_len = u32::from_le_bytes(packet[8..12].try_into().expect("four bytes")) as usize;
        Ok(Self { vendor: packet[12..12 + vendor_len].to_vec(), comments, trailing: packet[end..].to_vec() })
    }

    pub fn encode(&self) -> Result<Vec<u8>, OpusError> {
        let mut size = 8usize + 4 + self.vendor.len() + 4 + self.trailing.len();
        for comment in &self.comments { size = size.checked_add(4 + comment.len()).ok_or(OpusError::Limit("OpusTags header size overflows"))?; }
        if size > MAX_COMMENT_HEADER { return Err(OpusError::Limit("OpusTags header exceeds the supported size")); }
        let to_u32 = |n: usize| u32::try_from(n).map_err(|_| OpusError::Limit("OpusTags field exceeds 4 GiB"));
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(OPUS_TAGS_MAGIC);
        out.extend_from_slice(&to_u32(self.vendor.len())?.to_le_bytes());
        out.extend_from_slice(&self.vendor);
        out.extend_from_slice(&to_u32(self.comments.len())?.to_le_bytes());
        for comment in &self.comments {
            out.extend_from_slice(&to_u32(comment.len())?.to_le_bytes());
            out.extend_from_slice(comment);
        }
        out.extend_from_slice(&self.trailing);
        Ok(out)
    }

    /// Trailing data is carried forward only when RFC 7845 asks editors to
    /// preserve it (least-significant bit of its first byte set).
    pub fn preserved_trailing(&self) -> &[u8] {
        match self.trailing.first() { Some(b) if b & 1 == 1 => &self.trailing, _ => &[] }
    }
}

/// Case-insensitive match of the field name of a `NAME=value` comment.
pub fn comment_is(comment: &[u8], name: &str) -> bool {
    let n = name.as_bytes();
    comment.len() > n.len() && comment[n.len()] == b'=' && comment[..n.len()].eq_ignore_ascii_case(n)
}
/// Value part of a `NAME=value` comment, if it has the given name.
pub fn comment_value<'a>(comment: &'a [u8], name: &str) -> Option<&'a [u8]> {
    if comment.len() > name.len() && comment[name.len()] == b'=' && comment[..name.len()].eq_ignore_ascii_case(name.as_bytes()) {
        Some(&comment[name.len() + 1..])
    } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_durations() {
        // CELT FB 20 ms code 0, SILK NB 60 ms, hybrid FB 10 ms, code 3 with 6 x 20 ms.
        assert_eq!(packet_samples(&[0xfc]), Some(960));
        assert_eq!(packet_samples(&[0x18]), Some(2880));
        assert_eq!(packet_samples(&[0x70]), Some(480));
        assert_eq!(packet_samples(&[0xff, 6]), Some(5760));
        assert_eq!(packet_samples(&[0xff, 7]), None);
        assert_eq!(packet_samples(&[0xff, 0]), None);
        assert_eq!(packet_samples(&[0xff]), None);
        assert_eq!(packet_samples(&[]), None);
        assert_eq!(packet_samples(&[0xe1]), Some(240)); // two 2.5 ms? CELT config 28 -> 120 x 2
    }

    #[test]
    fn head_round_trip_and_refusals() {
        let head = OpusHead { version: 1, channels: 2, pre_skip: 312, input_sample_rate: 48_000, output_gain: -256 };
        let bytes = head.encode();
        assert_eq!(OpusHead::parse(&bytes).unwrap(), head);
        let mut family1 = bytes.clone(); family1[18] = 1;
        assert!(matches!(OpusHead::parse(&family1), Err(OpusError::Unsupported(_))));
        let mut three = bytes.clone(); three[9] = 3;
        assert!(matches!(OpusHead::parse(&three), Err(OpusError::Unsupported(_))));
        let mut zero = bytes.clone(); zero[9] = 0;
        assert!(matches!(OpusHead::parse(&zero), Err(OpusError::Invalid(_))));
        let mut future = bytes.clone(); future[8] = 16;
        assert!(matches!(OpusHead::parse(&future), Err(OpusError::Unsupported(_))));
        assert!(OpusHead::parse(&bytes[..18]).is_err());
        let mut minor = bytes.clone(); minor[8] = 15; minor.extend_from_slice(&[1, 2, 3]);
        assert_eq!(OpusHead::parse(&minor).unwrap().version, 15);
    }

    #[test]
    fn comment_header_round_trip_is_verbatim() {
        let header = CommentHeader { vendor: b"vendor".to_vec(),
            comments: vec![b"TITLE=x".to_vec(), vec![0xff, 0xfe, b'='], b"title=dup".to_vec(), Vec::new()], trailing: vec![1, 2, 3] };
        let bytes = header.encode().unwrap();
        assert_eq!(CommentHeader::parse(&bytes).unwrap(), header);
        assert_eq!(header.preserved_trailing(), &[1, 2, 3]);
        let padded = CommentHeader { trailing: vec![0; 40], ..header.clone() };
        assert!(padded.preserved_trailing().is_empty());
    }

    #[test]
    fn hostile_comment_headers_are_rejected_without_allocation() {
        let mut huge_vendor = b"OpusTags".to_vec();
        huge_vendor.extend_from_slice(&u32::MAX.to_le_bytes());
        huge_vendor.extend_from_slice(&[0; 8]);
        assert!(CommentHeader::parse(&huge_vendor).is_err());
        let mut huge_count = b"OpusTags".to_vec();
        huge_count.extend_from_slice(&0u32.to_le_bytes());
        huge_count.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(CommentHeader::parse(&huge_count).is_err());
        let mut huge_comment = b"OpusTags".to_vec();
        huge_comment.extend_from_slice(&0u32.to_le_bytes());
        huge_comment.extend_from_slice(&1u32.to_le_bytes());
        huge_comment.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(CommentHeader::parse(&huge_comment).is_err());
        assert!(CommentHeader::parse(b"OpusTag").is_err());
        assert!(CommentHeader::parse(&vec![0u8; MAX_COMMENT_HEADER + 1]).is_err());
    }

    #[test]
    fn comment_name_matching_is_case_insensitive() {
        assert!(comment_is(b"platinumnotes=abc", "PLATINUMNOTES"));
        assert!(!comment_is(b"PLATINUMNOTESX=abc", "PLATINUMNOTES"));
        assert_eq!(comment_value(b"Title=abc", "TITLE"), Some(&b"abc"[..]));
        assert_eq!(comment_value(b"TITLE", "TITLE"), None);
    }
}
