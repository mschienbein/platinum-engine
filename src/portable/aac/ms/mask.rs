//! S5: what the bitstream and the bit estimator see of the M/S decision after the intensity stage.
//!
//! * the writer reads `psyOut(L).flags[idx] & 1` AFTER the intensity decision (`int` clears bits 0..2 of the L flag of every intensity band
//!   (`(f & ~7) | 2`) and sets bit 5 of the R flag): intensity bands carry `ms_used = 0`, even when the M/S stage flagged them.
//!   `MsTool::flags` keeps the pre-intensity decision and is not used downstream;
//! * `ms_mask_present` over the AU's `max_sfb` (which can be smaller than `l.max_sfb` of the M/S stage when the quantizer trims trailing zero bands):
//!   0 when no (group, sfb < max_sfb) is flagged (also when `max_sfb == 0`), 2 when all are flagged, else 1 followed by one `ms_used` bit per
//!   (group, sfb) (group-major).

/// `ms_mask_present` values.
pub const MS_MASK_NONE: u8 = 0;
pub const MS_MASK_PER_BAND: u8 = 1;
pub const MS_MASK_ALL: u8 = 2;

/// Classification of the L-slot flag words over `ngroups x max_sfb` bands (`flags` indexed `g * stride + b`):
/// returns `(all_set, none_set)`; with `max_sfb == 0` (or no groups) both are true.
pub fn classify_flags(flags: &[i32], ngroups: usize, stride: usize, max_sfb: usize) -> (bool, bool) {
    let mut all = true;
    let mut none = true;
    for g in 0..ngroups {
        for b in 0..max_sfb {
            if flags[g * stride + b] & 1 != 0 {
                none = false;
            } else {
                all = false;
            }
        }
    }
    (all, none)
}

/// `ms_mask_present` of the bitstream.  Precedence as observed: no band set -> 0 (also for `max_sfb == 0`); all bands set -> 2; else 1.
pub fn ms_mask_present(flags: &[i32], ngroups: usize, stride: usize, max_sfb: usize) -> u8 {
    let (all, none) = classify_flags(flags, ngroups, stride, max_sfb);
    if max_sfb == 0 || none {
        MS_MASK_NONE
    } else if all {
        MS_MASK_ALL
    } else {
        MS_MASK_PER_BAND
    }
}

/// The `ms_used[g][sfb]` bits that follow `ms_mask_present == 1` (group-major, `max_sfb` bits per group).
pub fn ms_used_bits(flags: &[i32], ngroups: usize, stride: usize, max_sfb: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(ngroups * max_sfb);
    for g in 0..ngroups {
        for b in 0..max_sfb {
            v.push((flags[g * stride + b] & 1) as u8);
        }
    }
    v
}
