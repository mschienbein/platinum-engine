//! Apple behaviour of the AAC writer that the engine cannot observe, replayed against the portable writer.
//!
//! `portable_aac_encode.rs` holds what the engine reaches: every accepted configuration, every content kind, every write
//! size, the `create` classification of every rate and channel count. The tests here pin AudioToolbox's behaviour in
//! corners the engine never enters, so that a deliberate difference is a decision in one place and not a surprise:
//!
//! * a writer that carries a warning (`AacBitRateNotSet`) is aborted by the engine before it writes anything; Apple still
//!   finishes an empty file, fails the first write with the converter's `OSStatus`, and poisons the writer (reproduced);
//! * fractional sample rates (44100.5, 44099.99999, 44100.00001, 47999.999) are accepted by AudioToolbox and give files with the
//!   nominal rate's container fields but different access units (Apple encodes with the exact rate); the engine's rates come
//!   from the input file's integer rate, or 44100. They are **not reproduced**: the portable writer refuses them at `create`
//!   with `CodecError::UnsupportedFormat`, and Apple's recorded files stay here as the data that justifies the refusal;
//! * with the caller's flush-to-zero mode set (FPCR.FZ / MXCSR FTZ+DAZ) Apple flushes subnormal *input* samples, so its file
//!   differs from the unflushed one for such input; the engine calls the writer outside its flush scopes. The portable
//!   encoder reproduces it (it flushes the subnormal samples of a push made while the mode is set).
//!
//! The fixtures and the recorder are the same as for the main test (`tests/fixtures/aac-encode/README.md`).

#[path = "support/aac_encode.rs"]
mod aac_encode;

use aac_encode::{Fixtures, NOT_REPRODUCED, cell_for, cells, flush_to_zero_replays, not_reproduced, run_cells, warned};

/// Beyond `create`: an empty file can still be finished, the first write fails with the converter's `OSStatus` (`'!dat'`), the
/// writer is poisoned after it, and its finish and abort report the failure while removing the workspace.
#[test]
fn a_writer_created_with_a_warning_behaves_like_apples_after_create() {
    let (count, bad) = run_cells(warned, |_| vec!["create", "empty_finish", "leftover_after_empty_finish", "post"]);
    assert!(count >= 30, "{count} cells");
    assert!(bad.is_empty(), "{} mismatches against the Apple reference:\n{}", bad.len(), bad.iter().take(30).cloned().collect::<Vec<_>>().join("\n"));
}

/// The "not reproduced" list as data. For every fractional rate Apple created a converter and finished the 1100-frame file of the
/// probe, and that file is not the one it wrote at the nominal rate (so encoding as the nominal rate, or at the exact rate with this
/// encoder's nominal-rate tables, would give a different file): the refusal at `create` is justified. Apple's *empty* file does not
/// depend on the fractional part (it is the nominal rate's file), recorded here as the reason the refusal is not narrower than
/// `create` can know: at `create` nobody knows whether a frame will be written.
#[test]
fn the_files_apple_wrote_for_fractional_rates_differ_from_the_nominal_rates_files() {
    let fractional: Vec<_> = cells().iter().filter(|c| not_reproduced(c)).collect();
    assert_eq!(fractional.len(), NOT_REPRODUCED.len() * 8, "{} cells in all", cells().len());
    let mut bad = Vec::new();
    for (label, nominal) in NOT_REPRODUCED {
        for channels in 1..=8u64 {
            let (fractional, integer) = (cell_for(label, channels), cell_for(nominal, channels));
            let what = format!("{label} Hz x {channels} ch against {nominal} Hz");
            let data = |cell: &serde_json::Value| cell["post"]["finish"].clone();
            let (a, b) = (data(fractional), data(integer));
            if a["result"] != "ok" || b["result"] != "ok" { bad.push(format!("{what}: a recorded finish failed: {a} / {b}")); continue; }
            if a["sha256"] == b["sha256"] { bad.push(format!("{what}: Apple's files are identical, the refusal would not be justified")); }
            if fractional["empty_finish"]["sha256"] != integer["empty_finish"]["sha256"] { bad.push(format!("{what}: the empty files differ")); }
            if fractional["empty_finish"]["length"] != integer["empty_finish"]["length"] { bad.push(format!("{what}: the empty files differ in length")); }
        }
    }
    assert!(bad.is_empty(), "{} problems in the recorded data:\n{}", bad.len(), bad.join("\n"));
}

/// Subnormal input is the one case where Apple's file depends on the caller's flush-to-zero mode; the fixture records both. The
/// portable encoder follows the same rule (subnormal input samples are flushed to zero when the mode is set at the push), so the
/// three items whose Apple file depends on the mode must equal Apple's flushed file when the whole encode runs in the mode.
#[test]
fn an_ambient_flush_to_zero_mode_is_followed_like_apple_for_subnormal_input() {
    let fixtures = Fixtures::load();
    let (checked, bad) = flush_to_zero_replays(|i| i.fz_sha256.as_deref() != Some(i.m4a_sha256.as_str()));
    assert_eq!(checked, fixtures.items.iter().filter(|i| i.fz_sha256.as_deref().is_some_and(|fz| fz != i.m4a_sha256)).count());
    assert!(checked >= 3, "{checked} items whose Apple file depends on the mode");
    assert!(bad.is_empty(), "{} items differ from Apple under flush-to-zero:\n{}", bad.len(), bad.join("\n"));
}
