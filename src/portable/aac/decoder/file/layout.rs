//! Channel layouts: `channelConfiguration` / PCE shape -> CoreAudio layout tag, channel count and output channel labels.
//!
//! Evidence: AudioToolbox was probed with every PCE shape of up to 3 front, 2 side and 3 back elements and 0..2 LFE; exactly 17 of those shapes open.
//!
//! The output channel order is the element-list order of the decoder (`decoder::bits` `CreateList`): SCE = 1 channel, CPE = 2 (left, right), LFE = 1,
//! PCE lists front, side, back, then the LFEs; `channelConfiguration` 14 is the only configuration whose list differs from the stream order
//! (`SCE0 CPE0 CPE1 LFE0 CPE2`).  `labels` is the CoreAudio label order of the tag.  For a PCE the decoder's output order is front, side,
//! back, LFE (probed: all 17 shapes, every stream channel found at output index = list index, LFE last), which equals `labels`
//! for every shape except 7.1_C (front C, L R, X X ; back Ls Rs ; LFE): there the positional stereo-mixdown matrix of the tag is nevertheless
//! applied to the list order (fitted matrix of an FFmpeg 7.1-wide stream = the `channelConfiguration` 14 matrix), see [`crate::portable::aac::decoder::file::mixer`].

use crate::portable::aac::decoder::file::asc::PceConfig;

pub const TAG_MONO: u32 = 0x0064_0001;
pub const TAG_STEREO: u32 = 0x0065_0002;
pub const TAG_AAC_3_0: u32 = 0x0072_0003;
pub const TAG_AAC_4_0: u32 = 0x0074_0004;
pub const TAG_QUADRAPHONIC: u32 = 0x006c_0004;
pub const TAG_AAC_5_0: u32 = 0x0078_0005;
pub const TAG_AAC_5_1: u32 = 0x007c_0006;
pub const TAG_AAC_6_0: u32 = 0x008d_0006;
pub const TAG_AAC_6_1: u32 = 0x008e_0007;
pub const TAG_AAC_7_0: u32 = 0x008f_0007;
/// `kAudioChannelLayoutTag_AAC_7_1` = `MPEG_7_1_B`: C Lc Rc L R Ls Rs LFE
pub const TAG_AAC_7_1: u32 = 0x007f_0008;
pub const TAG_AAC_7_1_B: u32 = 0x00b7_0008;
pub const TAG_AAC_7_1_C: u32 = 0x00b8_0008;
pub const TAG_AAC_OCTAGONAL: u32 = 0x0090_0008;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ch {
    C,
    L,
    R,
    Ls,
    Rs,
    Cs,
    Lfe,
    Lc,
    Rc,
    Rls,
    Rrs,
    Vhl,
    Vhr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub tag: u32,
    pub channels: u8,
    /// CoreAudio label order of the tag
    pub labels: &'static [Ch],
}

use Ch::*;
static MONO: Layout = Layout { tag: TAG_MONO, channels: 1, labels: &[C] };
static STEREO: Layout = Layout { tag: TAG_STEREO, channels: 2, labels: &[L, R] };
static AAC_3_0: Layout = Layout { tag: TAG_AAC_3_0, channels: 3, labels: &[C, L, R] };
static AAC_4_0: Layout = Layout { tag: TAG_AAC_4_0, channels: 4, labels: &[C, L, R, Cs] };
static QUAD: Layout = Layout { tag: TAG_QUADRAPHONIC, channels: 4, labels: &[L, R, Ls, Rs] };
static AAC_5_0: Layout = Layout { tag: TAG_AAC_5_0, channels: 5, labels: &[C, L, R, Ls, Rs] };
static AAC_5_1: Layout = Layout { tag: TAG_AAC_5_1, channels: 6, labels: &[C, L, R, Ls, Rs, Lfe] };
static AAC_6_0: Layout = Layout { tag: TAG_AAC_6_0, channels: 6, labels: &[C, L, R, Ls, Rs, Cs] };
static AAC_6_1: Layout = Layout { tag: TAG_AAC_6_1, channels: 7, labels: &[C, L, R, Ls, Rs, Cs, Lfe] };
static AAC_7_0: Layout = Layout { tag: TAG_AAC_7_0, channels: 7, labels: &[C, L, R, Ls, Rs, Rls, Rrs] };
static AAC_7_1: Layout = Layout { tag: TAG_AAC_7_1, channels: 8, labels: &[C, Lc, Rc, L, R, Ls, Rs, Lfe] };
static AAC_7_1_B: Layout = Layout { tag: TAG_AAC_7_1_B, channels: 8, labels: &[C, L, R, Ls, Rs, Rls, Rrs, Lfe] };
static AAC_7_1_C: Layout = Layout { tag: TAG_AAC_7_1_C, channels: 8, labels: &[C, L, R, Ls, Rs, Lfe, Vhl, Vhr] };
static OCTAGONAL: Layout = Layout { tag: TAG_AAC_OCTAGONAL, channels: 8, labels: &[C, L, R, Ls, Rs, Rls, Rrs, Cs] };

/// Layout of `channelConfiguration` 1..=7, 11, 12, 14.  Configurations 8, 9, 10, 13, 15 are refused by `ExtAudioFileOpenURL` (`'wht?'`);
/// 0 means "PCE" (see [`pce_layout`]).
pub fn config_layout(channel_config: u8) -> Option<&'static Layout> {
    Some(match channel_config {
        1 => &MONO,
        2 => &STEREO,
        3 => &AAC_3_0,
        4 => &AAC_4_0,
        5 => &AAC_5_0,
        6 => &AAC_5_1,
        7 => &AAC_7_1,
        11 => &AAC_6_1,
        12 => &AAC_7_1_B,
        14 => &AAC_7_1_C,
        _ => return None,
    })
}

/// Layout of a PCE: a whitelist of the 17 element shapes Apple opens (front | side | back lists as SCE=false / CPE=true, LFE count).  Tags,
/// `object_type`, assoc/cc lists, mixdown fields and the comment never change the layout (recorded; the `matrix_mixdown_idx` of an in-band PCE changes the stereo matrix of a 5.1 stream, see `mixer`); coupling channel elements
/// are accepted here and refused later at client-format time (`'!dat'`, see [`crate::portable::aac::decoder::file::ClientError::NoDecoder`]).
pub fn pce_layout(p: &PceConfig) -> Option<&'static Layout> {
    let f: Vec<bool> = p.front.iter().map(|e| e.is_cpe).collect();
    let s: Vec<bool> = p.side.iter().map(|e| e.is_cpe).collect();
    let b: Vec<bool> = p.back.iter().map(|e| e.is_cpe).collect();
    Some(match (f.as_slice(), s.as_slice(), b.as_slice(), p.lfe.len()) {
        ([false], [], [], 0) => &MONO,
        ([true], [], [], 0) | ([false, false], [], [], 0) => &STEREO,
        ([false, true], [], [], 0) => &AAC_3_0,
        ([true], [], [true], 0) => &QUAD,
        ([false, true], [], [false], 0) => &AAC_4_0,
        ([false, true], [], [true], 0) => &AAC_5_0,
        ([false, true], [], [true], 1) => &AAC_5_1,
        ([false, true], [true], [false], 0) | ([false, true], [], [true, false], 0) => &AAC_6_0,
        ([false, true], [true], [false], 1) | ([false, true], [], [true, false], 1) => &AAC_6_1,
        ([false, true], [true], [true], 0) => &AAC_7_0,
        ([false, true], [true], [true], 1) => &AAC_7_1,
        ([false, true], [true], [true, false], 0) => &OCTAGONAL,
        ([false, true], [], [true, true], 1) => &AAC_7_1_B,
        ([false, true, true], [], [true], 1) => &AAC_7_1_C,
        _ => return None,
    })
}

/// Layout for a CoreAudio tag (used by the mixer tables and tests).
pub fn layout_for_tag(tag: u32) -> Option<&'static Layout> {
    [
        &MONO,
        &STEREO,
        &AAC_3_0,
        &AAC_4_0,
        &QUAD,
        &AAC_5_0,
        &AAC_5_1,
        &AAC_6_0,
        &AAC_6_1,
        &AAC_7_0,
        &AAC_7_1,
        &AAC_7_1_B,
        &AAC_7_1_C,
        &OCTAGONAL,
    ]
    .into_iter()
    .find(|l| l.tag == tag)
}
