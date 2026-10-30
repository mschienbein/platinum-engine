//! The portable AAC encoder against what AudioToolbox produced, replayed from the golden fixtures.
//!
//! `tests/fixtures/aac-encode/` holds generated inputs (a deterministic generator, no audio files: the same plain-Rust
//! source builds them in the recorder and here) and, for each, the macOS arm64 reference recorded through the engine's
//! own `apple_codec::Writer` (see its README): the SHA-256 and length of the complete `.m4a` after the MP4 clock is
//! normalised, access-unit diagnostics, and for the smaller items the file itself. `refusals.json` adds what
//! `Writer::create` answered for 364 combinations of sample rate and channel count (the exact `OSStatus`, or the
//! warnings the engine turns into "configure Apple encoder" failures), what a failed or abandoned writer leaves behind,
//! the recorder bit depths, and the misuse cases. Everything is compared bit for bit through the public portable
//! `Writer`; no Apple component is needed at run time, so the same test runs unchanged on macOS (`--features
//! portable`), Linux arm64 and Linux x86_64.

#[path = "support/aac_encode.rs"]
mod aac_encode;

use aac_encode::input::{self, KINDS, Source, access_units, summarize};
use aac_encode::{BLOCK, FIXED_CLOCK, Fixtures, Item, NOT_REPRODUCED, Scratch, Spec, cells, config, config_of, created, flush_to_zero_replays, hex, integer_rate, not_reproduced, refusals, replay, run_cells, warned};
use platinum_engine::portable::audio::{CodecError, OutputFormat, Writer, WriterConfig};
use platinum_engine::portable::digest::{Sha256, sha256};
use serde_json::Value;
use std::fs;

/// Every accepted `(rate, channels)`: AudioToolbox creates the converter and takes 256 kbit/s for exactly these.
fn accepted() -> Vec<(u32, usize)> {
    let mut all: Vec<(u32, usize)> = Vec::new();
    for rate in [44100, 48000] { all.extend((1..=8).map(|c| (rate, c))); }
    all.extend((3..=8).map(|c| (32000, c)));
    for rate in [24000, 22050] { all.extend((4..=8).map(|c| (rate, c))); }
    all.extend([(16000, 7), (16000, 8)]);
    all.sort_unstable();
    all
}

/// The groups whose items every default test replays; `long` items belong to the ignored test.
const GROUPS: [&str; 10] = ["empty", "grid", "stereo44", "mono44", "stereo48", "mono48", "lengths", "mid", "ten", "eightgroups"];

/// SHA-256 of the golden files, pinned here: regenerating `manifest.json` or `refusals.json` from the port alone would change them and fail the
/// pack test (`refs.bin` is pinned by the manifest).
const MANIFEST_SHA256: &str = "fe8004db1b1a67e1985a5bb22b69509c8a0f2d40ddcafd50cdc0fab18163cae2";
const REFUSALS_SHA256: &str = "261a61f747377a21c1a2826ef231a87e7ed3eedd6b81c3f8516fc5d817470a29";

#[test]
fn the_fixture_pack_is_intact_and_covers_what_it_claims() {
    let fixtures = Fixtures::load();
    let manifest = &fixtures.manifest;
    assert_eq!(hex(&sha256(&fixtures.pack)), manifest["pack"]["sha256"].as_str().unwrap(), "refs.bin does not match the manifest");
    assert_eq!(fixtures.pack.len() as u64, manifest["pack"]["bytes"].as_u64().unwrap());
    assert_eq!(manifest["reference"]["clock"].as_u64().unwrap(), u64::from(FIXED_CLOCK));
    assert!(fixtures.pack.len() <= 1_000_000, "the kept references stay small");
    let dir = aac_encode::fixture_dir();
    assert_eq!(hex(&sha256(&fs::read(dir.join("manifest.json")).unwrap())), MANIFEST_SHA256, "manifest.json is not the pinned golden file");
    assert_eq!(hex(&sha256(&fs::read(dir.join("refusals.json")).unwrap())), REFUSALS_SHA256, "refusals.json is not the pinned golden file");
    assert_eq!(fixtures.items.len(), 188);
    for (group, count) in [("empty", 34), ("grid", 34), ("stereo44", 33), ("mono44", 16), ("stereo48", 8), ("mono48", 3), ("lengths", 36), ("mid", 3), ("ten", 2), ("eightgroups", 12), ("long", 7)] {
        assert_eq!(fixtures.group(group).len(), count, "items of group {group}");
    }
    assert!(fixtures.items.windows(2).all(|pair| pair[0].id < pair[1].id), "the manifest is sorted by id");

    // the kept Apple files are what the manifest says, and the access-unit reader the diagnostics use understands them
    let (mut spans, mut kept) = (Vec::new(), 0usize);
    for item in &fixtures.items {
        let Some(reference) = &item.reference else { continue };
        spans.push((reference.offset, reference.length));
        assert!(reference.length <= 40_000, "{}: kept references are small", item.id);
        let file = fixtures.reference(item).unwrap();
        assert_eq!((hex(&sha256(&file)), file.len()), (item.m4a_sha256.clone(), item.m4a_length), "{}: kept reference", item.id);
        let summary = summarize(&file).unwrap_or_else(|e| panic!("{}: {e}", item.id));
        assert_eq!((summary.count, &summary.first_sha256, &summary.stream_sha256), (item.au_count, &item.first_au_sha256, &item.au_stream_sha256), "{}", item.id);
        assert_eq!((&summary.sizes, &summary.sha8), (&item.au_sizes, &item.au_sha8), "{}: access-unit table", item.id);
        let units = access_units(&file).unwrap();
        assert_eq!(units.len(), item.au_count);
        kept += 1;
    }
    spans.sort_unstable();
    let mut end = 0usize;
    for (offset, length) in spans {
        assert_eq!(offset, end, "the kept references do not tile refs.bin");
        end += length;
    }
    assert_eq!(end, fixtures.pack.len());
    assert_eq!(kept, 176, "kept references");

    // facts about Apple's files the manifest records: access-unit count, box order, and where moov goes
    for item in &fixtures.items {
        let frames = item.spec.frames;
        let units = if frames == 0 { 0 } else { (frames + 2112).div_ceil(1024) as usize };
        assert_eq!(item.au_count, units, "{}: {frames} frames need ceil((n + 2112) / 1024) access units", item.id);
        let tail = (item.spec.rate == 44100 && item.au_count > 13_508) || (item.spec.rate == 48000 && item.au_count > 14_540);
        let layout = if frames == 0 { "ftyp moov" } else if tail { "ftyp free mdat moov" } else { "ftyp moov free mdat" };
        assert_eq!(item.layout, layout, "{}: box order of Apple's file", item.id);
        assert!(item.m4a_length >= 557, "{}", item.id);
        assert_eq!(item.group == "long", item.seconds() >= 300.0, "{}: only the long items run to 5 minutes", item.id);
    }

    // coverage: every accepted configuration with content and as an empty file, every kind, the promised lengths
    let default: Vec<&Item> = fixtures.items.iter().filter(|i| i.group != "long").collect();
    assert_eq!(default.iter().filter(|i| i.spec.frames > 0).count(), 147, "encode items of the default groups");
    let mut grid: Vec<(u32, usize)> = fixtures.group("grid").iter().map(|i| (i.spec.rate, i.spec.channels)).collect();
    grid.sort_unstable();
    assert_eq!(grid, accepted(), "the grid covers every accepted configuration once");
    let mut empty: Vec<(u32, usize)> = fixtures.group("empty").iter().map(|i| (i.spec.rate, i.spec.channels)).collect();
    empty.sort_unstable();
    assert_eq!(empty, accepted());
    assert!(fixtures.group("empty").iter().all(|i| i.spec.frames == 0 && i.au_count == 0 && i.layout == "ftyp moov"));
    for kind in KINDS { assert!(fixtures.items.iter().any(|i| i.spec.kind == kind), "no item of kind {kind}"); }
    let lengths: Vec<u64> = default.iter().map(|i| i.spec.frames).collect();
    for n in [0, 1, 1023, 1024, 1025, 2112, 2113] { assert!(lengths.contains(&n), "no item of {n} frames"); }
    assert!(default.iter().any(|i| i.spec.frames == u64::from(i.spec.rate) * 3), "a 3 s item");
    assert_eq!(default.iter().filter(|i| (9.5..=10.5).contains(&i.seconds())).count(), 2, "10 s items");
    for (rate, channels, count) in [(44100, 1, 16), (44100, 2, 20), (48000, 1, 5), (48000, 2, 10)] {
        let kinds: std::collections::BTreeSet<&str> = default.iter().filter(|i| (i.spec.rate, i.spec.channels) == (rate, channels)).map(|i| i.spec.kind.as_str()).collect();
        assert_eq!(kinds.len(), count, "{rate} Hz {channels} ch has {} kinds", kinds.len());
    }
    let stereo44: std::collections::BTreeSet<&str> = default.iter().filter(|i| (i.spec.rate, i.spec.channels) == (44100, 2)).map(|i| i.spec.kind.as_str()).collect();
    assert_eq!(stereo44.len(), 20, "stereo 44.1 kHz has {} kinds", stereo44.len());
    assert!(fixtures.group("long").iter().any(|i| i.layout.ends_with("moov") && i.seconds() > 301.0), "an item with moov at the end");
    assert_eq!(fixtures.group("long").len(), 7);

    // the budget of the default tests: audio encoded per run, counting the write-size and flush-to-zero replays
    let write_sizes: Vec<&Item> = default.iter().copied().filter(|i| !i.write_sizes.is_empty()).collect();
    assert!(write_sizes.len() == 12 && write_sizes.iter().all(|i| i.write_sizes == [1, 7, 513, 4096, 0]));
    let seconds: f64 = default.iter().map(|i| i.seconds() * (1 + i.write_sizes.len() + usize::from(i.fz_sha256.is_some())) as f64).sum();
    assert!(seconds <= 62.0, "{seconds:.1} s of audio in the default tests");
    for group in GROUPS { assert!(!fixtures.group(group).is_empty(), "group {group}"); }
    assert!(fixtures.items.iter().all(|i| GROUPS.contains(&i.group.as_str()) || i.group == "long"), "every item belongs to a replayed group");
}

// ---- the generator ------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Stats { sha256: String, non_finite: u64, subnormal: u64, max_abs: f32, clipped: u64 }

fn stream_stats(spec: &Spec, block: usize) -> Stats {
    let mut source = Source::new(spec).unwrap();
    let mut hash = Sha256::new();
    let (mut buffer, mut bytes) = (Vec::new(), Vec::new());
    let mut stats = Stats::default();
    while source.next_block(block, &mut buffer) > 0 {
        bytes.clear();
        for &v in &buffer {
            bytes.extend_from_slice(&v.to_le_bytes());
            if !v.is_finite() { stats.non_finite += 1; continue; }
            if v != 0.0 && v.abs() < f32::MIN_POSITIVE { stats.subnormal += 1; }
            if v.abs() == 1.0 { stats.clipped += 1; }
            if v.abs() > stats.max_abs { stats.max_abs = v.abs(); }
        }
        hash.update(&bytes);
    }
    stats.sha256 = hex(&hash.finalize());
    stats
}

#[test]
fn generated_inputs_are_bit_identical_on_every_host() {
    let fixtures = Fixtures::load();
    let mut bad = Vec::new();
    for item in fixtures.items.iter().filter(|i| i.group != "long") {
        let stats = stream_stats(&item.spec, 4096);
        if stats.sha256 != item.input_sha256 { bad.push(format!("{}: input {} (expected {})", item.id, stats.sha256, item.input_sha256)); }
        // the kinds really do what they are for
        let what = match item.spec.kind.as_str() {
            "special" if stats.non_finite == 0 => Some("no NaN or infinity"),
            "subnormal" if stats.subnormal < item.spec.frames / 4 => Some("few subnormals"),
            "overrange" if stats.max_abs < item.spec.params[0] as f32 * 0.5 || stats.max_abs < 10.0 => Some("not over range"),
            "clipped" if stats.clipped < item.spec.frames / 10 => Some("not clipped"),
            "dc" | "square" | "nyquist" if stats.max_abs == 0.0 => Some("silent"),
            "silence" if stats.max_abs != 0.0 || stats.non_finite != 0 => Some("not silent"),
            "special" | "subnormal" | "overrange" | "clipped" | "dc" | "square" | "nyquist" | "silence" => None,
            _ if item.spec.frames > 4096 && stats.max_abs == 0.0 && item.spec.kind != "burst" => Some("silent"),
            _ => None,
        };
        if let Some(what) = what { bad.push(format!("{}: generated input is {what}", item.id)); }
        if item.spec.kind != "special" && stats.non_finite != 0 { bad.push(format!("{}: unexpected non-finite input", item.id)); }
    }
    assert!(bad.is_empty(), "{} generated inputs differ from the recorded ones:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn a_stream_does_not_depend_on_how_it_is_cut_into_blocks() {
    let fixtures = Fixtures::load();
    for id in ["st44_mix", "st44_castanets", "st44_special_basic", "grid_48000_8ch_lfe", "st44_sweep", "m44_subnormal_mixed", "grid_44100_3ch_perchan"] {
        let item = fixtures.get(id);
        for block in [1usize, 7, 512, 513, 100_000] {
            assert_eq!(stream_stats(&item.spec, block).sha256, item.input_sha256, "{id}: {block}-frame blocks");
        }
    }
    // a prefix of a longer item is the shorter item: the length only decides where the stream ends (sweeps excepted)
    let short = fixtures.get("st44_mix").spec.clone();
    let long = Spec { frames: short.frames * 2, ..short.clone() };
    let (a, b) = (input::render(&short).unwrap(), input::render(&long).unwrap());
    assert_eq!(a[..], b[..a.len()]);
}

// ---- whole files --------------------------------------------------------------------------------------------------------------

/// Replay every item of `group` with the engine's 512-frame writes and compare the complete file with Apple's.
fn check_group(group: &str) {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new(group);
    let items = fixtures.group(group);
    assert!(!items.is_empty(), "no items in group {group}");
    let bad: Vec<String> = items.iter().filter_map(|item| replay(fixtures, &scratch, item, BLOCK).err().map(|e| format!("{} ({} Hz, {} ch, {} frames, {}): {e}", item.id, item.spec.rate, item.spec.channels, item.spec.frames, item.spec.kind))).collect();
    let frames: u64 = items.iter().map(|i| i.spec.frames).sum();
    println!("{group}: {} items, {frames} frames", items.len());
    assert!(bad.is_empty(), "{} of {} items in {group} differ from the Apple reference:\n{}", bad.len(), items.len(), bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn every_accepted_rate_and_channel_count_is_bit_exact_with_apple() { check_group("grid"); }

#[test]
fn empty_input_gives_apples_ftyp_and_moov_only_file_for_every_accepted_configuration() { check_group("empty"); }

#[test]
fn stereo_44100_content_kinds_are_bit_exact_with_apple() { check_group("stereo44"); }

#[test]
fn mono_44100_content_kinds_are_bit_exact_with_apple() { check_group("mono44"); }

#[test]
fn rate_48000_content_kinds_are_bit_exact_with_apple() {
    check_group("stereo48");
    check_group("mono48");
}

#[test]
fn lengths_around_the_access_unit_boundaries_are_bit_exact_with_apple() { check_group("lengths"); }

#[test]
fn three_second_items_are_bit_exact_with_apple() { check_group("mid"); }

#[test]
fn ten_second_items_are_bit_exact_with_apple() { check_group("ten"); }

/// Short blocks of eight one-window groups: at the rates with 15 short bands (22.05, 24 and 16 kHz) that is 120 bands, the whole of the per-band
/// arrays, and the end of the last band (`start[120]`) is one past it. The portable encoder used to index out of bounds there.
#[test]
fn short_blocks_of_eight_one_window_groups_at_fifteen_band_rates_are_bit_exact_with_apple() {
    let fixtures = Fixtures::load();
    let items = fixtures.group("eightgroups");
    let mut configurations: Vec<(u32, usize)> = items.iter().map(|i| (i.spec.rate, i.spec.channels)).collect();
    configurations.sort_unstable();
    let expected: Vec<(u32, usize)> = accepted().into_iter().filter(|(rate, _)| matches!(rate, 16000 | 22050 | 24000)).collect();
    assert_eq!(configurations, expected, "one item for every accepted configuration with 15 short bands");
    assert!(items.iter().all(|i| i.spec.kind == "castanets" && i.au_count == 4 && i.spec.frames == 1280));
    check_group("eightgroups");
}

/// Apple's access units depend on the sample sequence only, not on how it reaches `ExtAudioFileWrite` (recorded for each of
/// these items at every size below on the Apple side too): the portable writer must give the same file for 1-frame,
/// 7-frame, 513-frame and 4096-frame writes and for one write of the whole item.
#[test]
fn write_sizes_other_than_512_give_the_same_file() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("sizes");
    let items: Vec<&Item> = fixtures.items.iter().filter(|i| !i.write_sizes.is_empty()).collect();
    assert_eq!(items.len(), 12);
    let mut bad = Vec::new();
    let mut replays = 0;
    for item in items {
        for &size in &item.write_sizes {
            replays += 1;
            if let Err(e) = replay(fixtures, &scratch, item, size) { bad.push(format!("{} with {size}-frame writes: {e}", item.id)); }
        }
    }
    println!("{replays} replays at other write sizes");
    assert!(bad.is_empty(), "{} replays differ from the Apple reference:\n{}", bad.len(), bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
}

/// The engine's DSP runs flushed (FPCR.FZ / MXCSR FTZ+DAZ); the writer must give Apple's file whatever mode its caller is in.
/// For input without subnormals Apple's file is the same flushed or not.
#[test]
fn an_ambient_flush_to_zero_mode_does_not_change_ordinary_input() {
    let (checked, bad) = flush_to_zero_replays(|i| i.fz_sha256.as_deref() == Some(i.m4a_sha256.as_str()));
    assert_eq!(checked, 4, "{checked} flush-to-zero items");
    assert!(bad.is_empty(), "{} items change under flush-to-zero:\n{}", bad.len(), bad.join("\n"));
}

// ---- Writer::create over rates and channel counts -----------------------------------------------------------------------------

#[test]
fn the_refusal_grid_covers_what_it_claims() {
    let document = refusals();
    let cells = cells();
    let rates = document["rates"].as_array().unwrap();
    let channels = document["channels"].as_array().unwrap();
    assert_eq!(cells.len(), rates.len() * channels.len());
    assert_eq!((rates.len(), channels.len()), (28, 13));
    let labels: Vec<&str> = rates.iter().map(|r| r["label"].as_str().unwrap()).collect();
    for label in ["0", "1", "7350", "8000", "11025", "12000", "16000", "22050", "24000", "32000", "44100", "44100.5", "48000", "64000", "88200", "96000", "176400", "192000", "384000", "nan", "inf", "-1"] {
        assert!(labels.contains(&label), "no rate {label}");
    }
    let channel_list: Vec<u64> = channels.iter().map(|c| c.as_u64().unwrap()).collect();
    assert_eq!(channel_list, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 16, 64]);
    // the accepted set, with and without a warning: exactly the 34 configurations the encoder serves
    let mut clean: Vec<(u32, usize)> = cells.iter().filter(|c| created(c) && !warned(c) && integer_rate(aac_encode::rate_of(c)))
        .map(|c| (aac_encode::rate_of(c) as u32, c["channels"].as_u64().unwrap() as usize)).collect();
    clean.sort_unstable();
    assert_eq!(clean, accepted());
    let warnings: Vec<&Value> = cells.iter().filter(|c| warned(c)).collect();
    assert!(warnings.len() == 38 && warnings.iter().all(|c| c["create"]["warnings"][0]["kind"] == "AacBitRateNotSet" && c["create"]["warnings"][0]["status"] == 0x2164_6174_i64), "'!dat' (0x21646174) on every warning");
    let operations: std::collections::BTreeSet<String> = cells.iter().filter(|c| !created(c)).map(|c| {
        let e = &c["create"]["error"];
        format!("{} {} {}", e["kind"].as_str().unwrap(), e["operation"].as_str().unwrap_or(e["message"].as_str().unwrap_or("")), e["status"])
    }).collect();
    assert_eq!(operations.len(), 4, "{operations:?}");
    assert!(cells.iter().all(|c| ["leftover", "leftover_after_abort", "leftover_after_drop", "leftover_after_empty_finish"].iter().all(|k| c.get(*k).is_none_or(|v| v.as_array().unwrap().is_empty()))),
        "Apple leaves nothing behind");
}

/// Every cell but the "not reproduced" rates (next test): the same classification as `ExtAudioFileCreateWithURL`/`SetProperty` gave
/// (the `OSStatus` or the engine's own input check), the same warnings, and nothing left behind after a failed create, an abort or an
/// abandoned writer; a cell without warnings also gets its write and finish.
#[test]
fn every_rate_and_channel_count_is_classified_like_apple_at_create() {
    let (count, bad) = run_cells(|cell| !not_reproduced(cell), |cell| {
        if !created(cell) { return vec!["create", "leftover"]; }
        let mut keys = vec!["create", "abort", "leftover_after_abort", "leftover_after_drop", "has_warnings"];
        if !warned(cell) { keys.extend(["empty_finish", "leftover_after_empty_finish", "post"]); }
        keys
    });
    println!("{count} cells");
    assert_eq!(count + NOT_REPRODUCED.len() * 8, cells().len(), "every cell is either replayed here or on the not-reproduced list");
    assert!(bad.is_empty(), "{} mismatches against the Apple reference:\n{}", bad.len(), bad.iter().take(30).cloned().collect::<Vec<_>>().join("\n"));
}

/// A rate that is not one of the nine AAC rates exactly is not reproduced: AudioToolbox serves it (44100.5, 44099.99999, 44100.00001
/// and 47999.999 all create and write a file that declares the nominal rate) but its access units follow the exact rate, so a file
/// encoded as the nominal rate would differ from Apple's. The writer refuses the rate at `create` with `CodecError::UnsupportedFormat`
/// and leaves nothing behind; `beyond_engine` keeps the evidence (Apple's files) that the refusal is justified.
#[test]
fn fractional_rates_are_refused_with_a_typed_error_and_leave_nothing() {
    let fractional: Vec<&Value> = cells().iter().filter(|c| not_reproduced(c)).collect();
    let labels: std::collections::BTreeSet<&str> = fractional.iter().map(|c| c["rate_label"].as_str().unwrap()).collect();
    assert_eq!(labels, NOT_REPRODUCED.iter().map(|(label, _)| *label).collect(), "the documented list is what Apple created without a warning at a fractional rate");
    assert_eq!(fractional.len(), NOT_REPRODUCED.len() * 8);
    assert!(fractional.iter().all(|c| (1..=8).contains(&c["channels"].as_u64().unwrap())));

    let scratch = Scratch::new("fractional");
    let mut bad = Vec::new();
    let mut refuse = |what: String, rate: f64, channels: u32, needle: &str| {
        let dir = scratch.dir();
        match Writer::create(dir.join("out.m4a"), config(rate, channels, 0)) {
            Err(CodecError::UnsupportedFormat(message)) if message.contains(needle) => {}
            Err(other) => bad.push(format!("{what}: refused with {other:?}, expected UnsupportedFormat ({needle})")),
            Ok(writer) => { bad.push(format!("{what}: created a writer")); let _ = writer.abort(); }
        }
        if !aac_encode::listing(&dir).as_array().unwrap().is_empty() { bad.push(format!("{what}: left {} behind", aac_encode::listing(&dir))); }
    };
    for cell in &fractional {
        refuse(format!("{} Hz x {} ch", cell["rate_label"], cell["channels"]), aac_encode::rate_of(cell), cell["channels"].as_u64().unwrap() as u32, "fractional");
    }
    // an integral rate within 10 Hz of an AAC rate is served the same way (measured on the Apple side; its files are not in the pack)
    for (rate, channels) in [(44090.0, 2), (44101.0, 8), (44110.0, 1), (47990.0, 2), (48010.0, 6), (32001.0, 3), (16010.0, 7), (24005.0, 4)] {
        refuse(format!("{rate} Hz x {channels} ch"), rate, channels, "exact sample rates");
    }
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.iter().take(30).cloned().collect::<Vec<_>>().join("\n"));
}

// ---- bit depth, create-time checks, paths and misuse ----------------------------------------------------------------------------

#[test]
fn the_recorder_bit_depth_is_validated_like_apple_and_never_changes_the_file() {
    let document = refusals();
    let data = aac_encode::probe_data(2);
    let scratch = Scratch::new("depth");
    let mut bad = Vec::new();
    let mut hashes = std::collections::BTreeSet::new();
    assert_eq!(document["depths"].as_array().unwrap().len(), 16, "recorded bit depths");
    for record in document["depths"].as_array().unwrap() {
        let depth = record["depth"].as_u64().unwrap() as u32;
        let dir = scratch.dir();
        let actual = aac_encode::finished_probe(&dir, config(44100.0, 2, depth), &[&data[..]]);
        bad.extend(aac_encode::mismatches(&format!("depth {depth}"), record, &actual, &["create", "writes", "finish", "sha256", "length", "leftover"]));
        if let Some(sha) = actual["sha256"].as_str() { hashes.insert(sha.to_owned()); }
        let _ = fs::remove_dir_all(&dir);
    }
    assert!(bad.is_empty(), "{} mismatches against the Apple reference:\n{}", bad.len(), bad.join("\n"));
    assert_eq!(hashes.len(), 1, "depths 0, 16, 24 and 32 give one file");
}

#[test]
fn when_several_create_checks_fail_the_same_one_wins_as_on_apple() {
    let scratch = Scratch::new("combined");
    let mut bad = Vec::new();
    assert_eq!(refusals()["combined"].as_array().unwrap().len(), 8, "recorded combined-failure cases");
    for record in refusals()["combined"].as_array().unwrap() {
        let dir = scratch.dir();
        let rate = aac_encode::rate_of(record);
        let actual = aac_encode::combined_case(&dir, rate, record["channels"].as_u64().unwrap() as u32, record["depth"].as_u64().unwrap() as u32, record["existing"].as_bool().unwrap());
        bad.extend(aac_encode::mismatches(record["case"].as_str().unwrap(), record, &actual, &["create", "leftover", "existing_unchanged", "leftover_after_drop"]));
    }
    assert!(bad.is_empty(), "{} mismatches against the Apple reference:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn unusable_destination_paths_are_refused_like_apple() {
    let scratch = Scratch::new("paths");
    let mut bad = Vec::new();
    assert_eq!(refusals()["paths"].as_array().unwrap().len(), 5, "recorded destination paths");
    for record in refusals()["paths"].as_array().unwrap() {
        let dir = scratch.dir();
        let actual = aac_encode::path_case(&dir, record["case"].as_str().unwrap());
        bad.extend(aac_encode::mismatches(record["case"].as_str().unwrap(), record, &actual, &["create", "leftover"]));
    }
    assert!(bad.is_empty(), "{} mismatches against the Apple reference:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn write_misuse_is_reported_like_apple_and_does_not_poison_the_writer() {
    let scratch = Scratch::new("misuse");
    let data = aac_encode::probe_data(2);
    let mut bad = Vec::new();
    let records = refusals()["misuse"].as_array().unwrap();
    assert_eq!(records.len(), 6, "recorded misuse cases");
    for record in records {
        let dir = scratch.dir();
        let actual = aac_encode::misuse_case(&dir, record["case"].as_str().unwrap(), &data);
        bad.extend(aac_encode::mismatches(record["case"].as_str().unwrap(), record, &actual, &["create", "writes", "finish", "sha256", "length", "leftover"]));
    }
    // an odd sample count is refused, an empty write is a no-op, and neither changes the file (Apple's own record of it)
    let sha = |case: &str| records.iter().find(|r| r["case"] == case).unwrap()["sha256"].clone();
    assert_eq!(sha("reference"), sha("empty_write"));
    assert_eq!(sha("reference"), sha("ragged_then_valid"));
    assert_eq!(sha("reference"), sha("two_writes"));
    assert!(bad.is_empty(), "{} mismatches against the Apple reference:\n{}", bad.len(), bad.join("\n"));
}

/// A writer that is aborted or dropped leaves nothing in its directory, one that finishes leaves exactly its target, on
/// every path a writer can end.
#[test]
fn writers_leave_nothing_behind() {
    let fixtures = Fixtures::load();
    let scratch = Scratch::new("cleanup");
    let mut bad = Vec::new();
    let mut check = |what: String, dir: &std::path::Path, want: &[&str]| {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        if names != want { bad.push(format!("{what}: {names:?}, expected {want:?}")); }
    };
    for id in ["grid_44100_2ch_corr", "grid_48000_6ch_lfe", "grid_16000_8ch_perchan", "st44_special_basic", "empty_44100_2ch"] {
        let item = fixtures.get(id);
        let data = input::render(&item.spec).unwrap();
        let per = item.spec.channels;
        let half = (data.len() / per / 2) * per;
        let target = |dir: &std::path::Path| dir.join("out.m4a");
        let cfg: WriterConfig = config_of(&item.spec);
        assert_eq!(cfg.format, OutputFormat::Aac);

        let dir = scratch.dir();
        let mut writer = Writer::create(target(&dir), cfg).map_err(|e| format!("{id}: create: {e:?}")).unwrap();
        writer.write(&data[..half]).unwrap();
        drop(writer);
        check(format!("{id}: dropped after a write"), &dir, &[]);

        let dir = scratch.dir();
        let mut writer = Writer::create(target(&dir), cfg).unwrap();
        writer.write(&data[..half]).unwrap();
        writer.abort().unwrap();
        check(format!("{id}: aborted after a write"), &dir, &[]);

        let dir = scratch.dir();
        let mut writer = Writer::create(target(&dir), cfg).unwrap();
        writer.write(&data).unwrap();
        writer.finish().unwrap();
        check(format!("{id}: finished"), &dir, &["out.m4a"]);

        // a target that already exists is never replaced
        let before = fs::read(target(&dir)).unwrap();
        assert!(Writer::create(target(&dir), cfg).is_err());
        check(format!("{id}: refused over an existing file"), &dir, &["out.m4a"]);
        assert_eq!(fs::read(target(&dir)).unwrap(), before);
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---- long items ---------------------------------------------------------------------------------------------------------------

/// Items of 5 minutes and more, replayed from their generators with bounded memory (512-frame writes streamed from the
/// generator): the moov box moves behind mdat once the sample tables outgrow the reservation, at 13,509 access units for
/// 44.1 kHz and 14,541 for 48 kHz, and the files stay bit exact. The release battery runs this one.
#[test]
#[ignore = "long: run by the release battery"]
fn long_items_are_bit_exact_with_apple_including_the_moov_at_the_end_layout() {
    let fixtures = Fixtures::load();
    let items = fixtures.group("long");
    assert_eq!(items.len(), 7);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failures = std::sync::Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).min(4);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let scratch = Scratch::new("long");
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let started = std::time::Instant::now();
                    let result = (|| -> Result<(), String> {
                        let stats = stream_stats(&item.spec, 4096);
                        if stats.sha256 != item.input_sha256 { return Err(format!("generated input {} (expected {})", stats.sha256, item.input_sha256)); }
                        replay(fixtures, &scratch, item, BLOCK)
                    })();
                    println!("{}: {:.1} s of audio in {:.1} s: {}", item.id, item.seconds(), started.elapsed().as_secs_f64(), if result.is_ok() { "bit exact" } else { "DIFFERENT" });
                    if let Err(e) = result { failures.lock().unwrap().push(format!("{}: {e}", item.id)); }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{} of {} long items differ from the Apple reference:\n{}", failures.len(), items.len(), failures.join("\n"));
}
