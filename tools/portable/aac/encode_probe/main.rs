// Records what AudioToolbox's AAC encoder does through the engine's own macOS `apple_codec::Writer`, exactly as the
// engine drives it: the producer of the Apple reference in tests/fixtures/aac-encode (macOS arm64 only). Build:
// `cargo build --release --offline` in this directory; `tools/portable/aac/make_encode_fixtures.py` runs it.
//
//   encode_probe items SPEC.json OUT_DIR [--block N] [--fz] [--keep] [--only id,id]
//       encode every item of SPEC (a JSON array of generator specs, see tests/support/aac_encode_input.rs) with
//       512-frame writes (--block N: N frames, 0: one write; --fz: inside the engine's flush-to-zero scope), normalise
//       the MP4 clock and print what each file is. --keep leaves OUT_DIR/<id>.m4a behind.
//   encode_probe refusal WORK_DIR          the outcome of Writer::create for a grid of rates x channel counts
//   encode_probe create WORK_DIR RATE:CHANNELS...   only the outcome of Writer::create (and what it leaves behind) for the listed cells; RATE is any
//                                           f64 text ("3000000.5", "nan"), CHANNELS a u32: for re-probing a boundary
//   encode_probe misc WORK_DIR             bit depths, path and write misuse, and the post-create behaviour
//   encode_probe crosscheck CORPUS_DIR RUNS_DIR [--threads N]
//       an independently recorded set of Apple encodes: CORPUS_DIR/manifest.json lists items (id, file, rate,
//       channels) whose raw interleaved float32 inputs sit beside it; RUNS_DIR/<id>/ext.m4a (+ ext.aus) is the Apple
//       file. Encodes every input with the engine Writer and compares the files after normalising both clocks.
//   encode_probe layout LAYOUT_DIR         LAYOUT_DIR/{empty_<rate>_<n>ch,sil44_n<N>,sil48_n<N>}/ext.m4a (empty files and
//                                           silence at the moov-placement boundary) against the engine Writer
//
// Every command prints one JSON document. Output directories are created, never overwritten (create-only).
#![allow(dead_code)]
#[path = "../../../../tests/support/aac_encode_input.rs"]
mod input;

use input::{BLOCK, FIXED_CLOCK, Source, Spec, hex, summarize};
use platinum_engine::apple_codec::{CodecError, CodecWarning, OutputFormat, Writer, WriterConfig, set_mp4_header_times};
use platinum_engine::fp_mode::with_flush_to_zero;
use platinum_engine::portable::digest::sha256;
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

type Res<T> = Result<T, String>;

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}
fn has(args: &[String], name: &str) -> bool { args.iter().any(|a| a == name) }

fn config(rate: f64, channels: u32, depth: u32) -> WriterConfig { WriterConfig { format: OutputFormat::Aac, sample_rate: rate, channels, bit_depth: depth } }
fn config_of(spec: &Spec) -> WriterConfig { config(f64::from(spec.rate), spec.channels as u32, 0) }

/// The classification both sides compare: identical shape in tests/support/aac_encode.rs.
fn describe(error: &CodecError) -> Value {
    match error {
        CodecError::InvalidInput(message) => json!({"kind": "InvalidInput", "message": message}),
        CodecError::Native { operation, status } => json!({"kind": "Native", "operation": operation, "status": status}),
        CodecError::Io { operation, source, .. } => json!({"kind": "Io", "operation": operation, "io_kind": format!("{:?}", source.kind())}),
        CodecError::Cleanup { primary, .. } => json!({"kind": "Cleanup", "primary": describe(primary)}),
        CodecError::Poisoned => json!({"kind": "Poisoned"}),
        other => json!({"kind": "Other", "text": other.to_string()}),
    }
}
fn describe_warnings(warnings: &[CodecWarning]) -> Value {
    Value::Array(warnings.iter().map(|w| match w {
        CodecWarning::AacConverterUnavailable { status } => json!({"kind": "AacConverterUnavailable", "status": status}),
        CodecWarning::AacBitRateNotSet { status } => json!({"kind": "AacBitRateNotSet", "status": status}),
    }).collect())
}
fn result_of<T>(r: &Result<T, CodecError>) -> Value {
    match r { Ok(_) => json!({"result": "ok"}), Err(e) => json!({"result": "err", "error": describe(e)}) }
}

fn listing(dir: &Path) -> Value {
    let mut names: Vec<String> = fs::read_dir(dir).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    json!(names)
}

fn make_dir(path: &Path) -> Res<()> { fs::create_dir(path).map_err(|e| format!("create {}: {e}", path.display())) }

/// Feed `spec`'s stream (or the pre-rendered `data`) to `writer` in `block`-frame writes (0 = all at once).
fn feed(writer: &mut Writer, spec: &Spec, block: usize, data: Option<&[f32]>) -> Result<(), CodecError> {
    let channels = spec.channels;
    if let Some(data) = data {
        if block == 0 { return writer.write(data); }
        for chunk in data.chunks(block * channels) { writer.write(chunk)?; }
        return Ok(());
    }
    let mut source = Source::new(spec).expect("spec");
    let mut buffer = Vec::new();
    let step = if block == 0 { spec.frames.max(1) as usize } else { block };
    while source.next_block(step, &mut buffer) > 0 { writer.write(&buffer)?; }
    Ok(())
}

/// Create, write, finish, and normalise the clock: the engine's sequence for one file.
fn encode(path: &Path, spec: &Spec, block: usize, data: Option<&[f32]>) -> Res<()> {
    let mut writer = Writer::create(path, config_of(spec)).map_err(|e| format!("create {}: {e}", spec.kind))?;
    if !writer.warnings().is_empty() { return Err(format!("unexpected warnings {:?}", writer.warnings())); }
    feed(&mut writer, spec, block, data).map_err(|e| format!("write: {e}"))?;
    writer.finish().map_err(|e| format!("finish: {e}"))?;
    set_mp4_header_times(path, FIXED_CLOCK).map_err(|e| format!("clock: {e}"))?;
    Ok(())
}

fn au_json(file: &[u8]) -> Res<Value> {
    let s = summarize(file)?;
    Ok(json!({"au_count": s.count, "first_au_sha256": s.first_sha256, "au_stream_sha256": s.stream_sha256, "au_sizes": s.sizes, "au_sha8": s.sha8.join(" ")}))
}

fn cmd_items(args: &[String]) -> Res<Value> {
    let spec_path = args.first().ok_or("SPEC.json")?;
    let out = PathBuf::from(args.get(1).ok_or("OUT_DIR")?);
    let block: usize = opt(args, "--block").map(|v| v.parse().map_err(|_| "--block")).transpose()?.unwrap_or(BLOCK);
    let (fz, keep) = (has(args, "--fz"), has(args, "--keep"));
    let only: Option<Vec<&str>> = opt(args, "--only").map(|v| v.split(',').collect());
    make_dir(&out)?;
    let items: Value = serde_json::from_slice(&fs::read(spec_path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    for item in items.as_array().ok_or("spec must be an array")? {
        let id = item["id"].as_str().ok_or("id")?;
        if only.as_ref().is_some_and(|o| !o.contains(&id)) { continue; }
        let spec = Spec::from_json(item)?;
        let path = out.join(format!("{id}.m4a"));
        let input_sha256 = input::input_sha256(&spec)?;
        if fz {
            let data = input::render(&spec)?;
            with_flush_to_zero(|| encode(&path, &spec, block, Some(&data)))?;
        } else {
            encode(&path, &spec, block, None)?;
        }
        let bytes = fs::read(&path).map_err(|e| e.to_string())?;
        let mut record = json!({"id": id, "input_sha256": input_sha256, "frames": spec.frames, "m4a_sha256": hex(&sha256(&bytes)), "m4a_length": bytes.len()});
        for (k, v) in au_json(&bytes)?.as_object().unwrap() { record[k] = v.clone(); }
        record["layout"] = json!(input::layout(&bytes).iter().map(|b| b.0.clone()).collect::<Vec<_>>().join(" "));
        results.push(record);
        if !keep { fs::remove_file(&path).map_err(|e| e.to_string())?; }
    }
    Ok(Value::Array(results))
}

// ---- the refusal grid ---------------------------------------------------------------------------------------------------------

const WRITE_FRAMES: u64 = 1100;

fn probe_spec(rate: f64, channels: u32) -> Spec {
    Spec { kind: "white".into(), rate: rate as u32, channels: channels as usize, frames: WRITE_FRAMES, seed: 0x5eed_0001, params: [400, 0, 0, 0] }
}
/// 1100 frames of seeded white noise for any channel count 1..=8 (the rate is irrelevant to the stream).
fn probe_data(channels: u32) -> Vec<f32> {
    if channels == 0 || channels > 8 { return vec![0.25; WRITE_FRAMES as usize * channels.max(1) as usize]; }
    input::render(&probe_spec(48000.0, channels)).expect("probe spec")
}

/// What `Writer::create` does for `(rate, channels, depth)` and what follows, recorded with the engine's own call order.
fn cell(dir: &Path, rate: f64, channels: u32, depth: u32) -> Res<Value> {
    make_dir(dir)?;
    let target = dir.join("out.m4a");
    let cfg = config(rate, channels, depth);
    let data = probe_data(channels);
    let mut record = json!({});
    match Writer::create(&target, cfg) {
        Err(e) => {
            record["create"] = result_of::<()>(&Err(e));
            record["leftover"] = listing(dir);
        }
        Ok(writer) => {
            let warnings = describe_warnings(writer.warnings());
            let has_warnings = !writer.warnings().is_empty();
            record["create"] = json!({"result": "ok", "warnings": warnings});
            let aborted = writer.abort();
            record["abort"] = result_of(&aborted);
            record["leftover_after_abort"] = listing(dir);
            // an abandoned writer: dropped without finish or abort
            drop(Writer::create(&target, cfg));
            record["leftover_after_drop"] = listing(dir);
            // an empty file, finished at once
            let empty = Writer::create(&target, cfg).and_then(|w| w.finish());
            record["empty_finish"] = result_of(&empty);
            if empty.is_ok() {
                set_mp4_header_times(&target, FIXED_CLOCK).map_err(|e| e.to_string())?;
                let bytes = fs::read(&target).map_err(|e| e.to_string())?;
                record["empty_finish"]["sha256"] = json!(hex(&sha256(&bytes)));
                record["empty_finish"]["length"] = json!(bytes.len());
                fs::remove_file(&target).map_err(|e| e.to_string())?;
            }
            record["leftover_after_empty_finish"] = listing(dir);
            // write 1100 frames in 512-frame blocks, then finish
            let mut steps = json!({});
            match Writer::create(&target, cfg) {
                Err(e) => steps["create"] = result_of::<()>(&Err(e)),
                Ok(mut w) => {
                    let mut failure = None;
                    for chunk in data.chunks(BLOCK * channels.max(1) as usize) {
                        let r = w.write(chunk);
                        if let Err(e) = r { failure = Some(describe(&e)); break; }
                    }
                    steps["write"] = match &failure { None => json!({"result": "ok"}), Some(e) => json!({"result": "err", "error": e}) };
                    if failure.is_some() {
                        steps["write_again"] = result_of(&w.write(&data[..channels.max(1) as usize]));
                    }
                    let finished = w.finish();
                    steps["finish"] = result_of(&finished);
                    if finished.is_ok() {
                        set_mp4_header_times(&target, FIXED_CLOCK).map_err(|e| e.to_string())?;
                        let bytes = fs::read(&target).map_err(|e| e.to_string())?;
                        steps["finish"]["sha256"] = json!(hex(&sha256(&bytes)));
                        steps["finish"]["length"] = json!(bytes.len());
                        fs::remove_file(&target).map_err(|e| e.to_string())?;
                    }
                }
            }
            steps["leftover"] = listing(dir);
            // a writer that was written to and then aborted
            if let Ok(mut w) = Writer::create(&target, cfg) {
                let _ = w.write(&data[..channels.max(1) as usize * 600]);
                steps["abort_after_write"] = result_of(&w.abort());
                steps["leftover_after_abort_after_write"] = listing(dir);
            }
            record["post"] = steps;
            record["has_warnings"] = json!(has_warnings);
        }
    }
    fs::remove_dir_all(dir).map_err(|e| e.to_string())?;
    Ok(record)
}

fn rate_label(rate: f64) -> String {
    if rate.is_nan() { "nan".into() } else if rate.is_infinite() { if rate > 0.0 { "inf".into() } else { "-inf".into() } }
    else if rate.abs() >= 1e15 { format!("{rate:e}") } else { format!("{rate}") }
}

fn cmd_refusal(args: &[String]) -> Res<Value> {
    let root = PathBuf::from(args.first().ok_or("WORK_DIR")?);
    make_dir(&root)?;
    let mut rates = vec![0.0, 1.0, 7350.0, 8000.0, 11025.0, 12000.0, 16000.0, 22050.0, 24000.0, 32000.0, 44100.0, 44100.5, 48000.0, 64000.0,
        88200.0, 96000.0, 176400.0, 192000.0, 384000.0, f64::NAN, f64::INFINITY, -1.0];
    // a rate that is 44100 modulo 2^32, one that is 44100 to five decimals, and absurd magnitudes
    rates.extend([4_295_011_396.0, 44099.99999, 44100.00001, 47999.999, 2147483648.0, 1e300]);
    let mut channels: Vec<u32> = vec![0];
    channels.extend(1..=10);
    channels.extend([16, 64]);
    let mut cells = Vec::new();
    let mut index = 0usize;
    for &rate in &rates {
        for &ch in &channels {
            let mut record = cell(&root.join(format!("c{index}")), rate, ch, 0)?;
            record["rate_bits"] = json!(format!("{:#018x}", rate.to_bits()));
            record["rate_label"] = json!(rate_label(rate));
            record["channels"] = json!(ch);
            cells.push(record);
            index += 1;
        }
    }
    fs::remove_dir(&root).map_err(|e| e.to_string())?;
    Ok(json!({"rates": rates.iter().map(|r| json!({"bits": format!("{:#018x}", r.to_bits()), "label": rate_label(*r)})).collect::<Vec<_>>(), "channels": channels, "write_frames": WRITE_FRAMES, "cells": cells}))
}

/// `encode_probe create WORK_DIR RATE:CHANNELS...`: the outcome of `Writer::create` for each listed cell, and nothing after it.
fn cmd_create(args: &[String]) -> Res<Value> {
    let root = PathBuf::from(args.first().ok_or("WORK_DIR")?);
    make_dir(&root)?;
    let mut cells = Vec::new();
    for (index, text) in args[1..].iter().enumerate() {
        let (rate_text, channels_text) = text.split_once(':').ok_or_else(|| format!("{text}: expected RATE:CHANNELS"))?;
        let rate: f64 = rate_text.parse().map_err(|e| format!("{text}: rate: {e}"))?;
        let channels: u32 = channels_text.parse().map_err(|e| format!("{text}: channels: {e}"))?;
        let dir = root.join(format!("c{index}"));
        make_dir(&dir)?;
        let target = dir.join("out.m4a");
        let mut record = json!({"rate_bits": format!("{:#018x}", rate.to_bits()), "rate_label": rate_label(rate), "channels": channels});
        match Writer::create(&target, config(rate, channels, 0)) {
            Err(e) => record["create"] = result_of::<()>(&Err(e)),
            Ok(writer) => {
                record["create"] = json!({"result": "ok", "warnings": describe_warnings(writer.warnings())});
                record["abort"] = result_of(&writer.abort());
            }
        }
        record["leftover"] = listing(&dir);
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        cells.push(record);
    }
    fs::remove_dir(&root).map_err(|e| e.to_string())?;
    Ok(json!({"cells": cells}))
}

// ---- bit depths, path and write misuse -------------------------------------------------------------------------------------------

/// A finished 44.1 kHz stereo file of the probe stream, as (result, sha256, length).
fn finished_probe(dir: &Path, cfg: WriterConfig, blocks: &[&[f32]]) -> Res<Value> {
    let target = dir.join("out.m4a");
    let mut record = json!({});
    match Writer::create(&target, cfg) {
        Err(e) => record["create"] = result_of::<()>(&Err(e)),
        Ok(mut w) => {
            record["create"] = json!({"result": "ok", "warnings": describe_warnings(w.warnings())});
            let mut writes = Vec::new();
            for b in blocks { writes.push(result_of(&w.write(b))); }
            record["writes"] = Value::Array(writes);
            let finished = w.finish();
            record["finish"] = result_of(&finished);
            if finished.is_ok() {
                set_mp4_header_times(&target, FIXED_CLOCK).map_err(|e| e.to_string())?;
                let bytes = fs::read(&target).map_err(|e| e.to_string())?;
                record["sha256"] = json!(hex(&sha256(&bytes)));
                record["length"] = json!(bytes.len());
            }
        }
    }
    record["leftover"] = listing(dir);
    let _ = fs::remove_file(&target);
    Ok(record)
}

/// Which of several failing checks wins, with a pre-existing target or not.
fn combined_case(dir: &Path, rate: f64, channels: u32, depth: u32, existing: bool) -> Value {
    let target = dir.join("out.m4a");
    if existing { fs::write(&target, b"existing").expect("write the existing target"); }
    let r = Writer::create(&target, config(rate, channels, depth));
    let mut record = json!({"create": result_of(&r), "leftover": listing(dir)});
    if existing { record["existing_unchanged"] = json!(fs::read(&target).map(|b| b == b"existing").unwrap_or(false)); }
    drop(r);
    record["leftover_after_drop"] = listing(dir);
    record
}

/// Unusable destination paths.
fn path_case(dir: &Path, case: &str) -> Value {
    let cfg = config(44100.0, 2, 0);
    match case {
        "missing_parent" => json!({"create": result_of(&Writer::create(dir.join("no-such-dir").join("out.m4a"), cfg)), "leftover": listing(dir)}),
        "empty" => json!({"create": result_of(&Writer::create("", cfg))}),
        "nul" => json!({"create": result_of(&Writer::create("a\0b.m4a", cfg))}),
        "root" => json!({"create": result_of(&Writer::create("/", cfg))}),
        "directory" => {
            let as_dir = dir.join("is-a-dir");
            fs::create_dir(&as_dir).expect("create the directory");
            json!({"create": result_of(&Writer::create(&as_dir, cfg)), "leftover": listing(dir)})
        }
        other => panic!("unknown path case {other}"),
    }
}

/// Write misuse on a healthy 44.1 kHz stereo writer.
fn misuse_case(dir: &Path, case: &str, data: &[f32]) -> Res<Value> {
    let empty: &[f32] = &[];
    let ragged = &data[..3];
    let blocks: Vec<&[f32]> = match case {
        "reference" => vec![data],
        "empty_write" => vec![empty, data, empty],
        "ragged_then_valid" => vec![ragged, data],
        "ragged_only" => vec![ragged],
        "two_writes" => vec![&data[..1000], &data[1000..]],
        "no_write" => vec![],
        other => panic!("unknown misuse case {other}"),
    };
    finished_probe(dir, config(44100.0, 2, 0), &blocks)
}

const COMBINED: [(&str, f64, u32, u32, bool); 8] = [
    ("nan_rate_bad_depth", f64::NAN, 2, 7, false), ("zero_channels_bad_depth", 44100.0, 0, 7, false), ("zero_channels_nan_rate", f64::NAN, 0, 0, false),
    ("bad_depth_existing_target", 44100.0, 2, 7, true), ("nan_rate_existing_target", f64::NAN, 2, 0, true),
    ("rejected_rate_existing_target", 192000.0, 2, 0, true), ("rejected_rate_bad_depth", 192000.0, 2, 7, false), ("valid_existing_target", 44100.0, 2, 0, true),
];
const PATH_CASES: [&str; 5] = ["missing_parent", "empty", "nul", "root", "directory"];
const MISUSE_CASES: [&str; 6] = ["reference", "empty_write", "ragged_then_valid", "ragged_only", "two_writes", "no_write"];
const DEPTHS: [u32; 16] = [0, 8, 12, 15, 16, 17, 20, 23, 24, 25, 31, 32, 33, 48, 64, u32::MAX];

fn cmd_misc(args: &[String]) -> Res<Value> {
    let root = PathBuf::from(args.first().ok_or("WORK_DIR")?);
    make_dir(&root)?;
    let mut n = 0usize;
    let mut fresh = || -> Res<PathBuf> { n += 1; let dir = root.join(format!("m{n}")); make_dir(&dir)?; Ok(dir) };
    let data = probe_data(2);

    // the recorder bit depth: valid for AAC output as for the PCM formats, and without effect on the file
    let mut depths = Vec::new();
    for depth in DEPTHS {
        let dir = fresh()?;
        let mut record = finished_probe(&dir, config(44100.0, 2, depth), &[&data[..]])?;
        record["depth"] = json!(depth);
        depths.push(record);
    }
    let mut combined = Vec::new();
    for (case, rate, channels, depth, existing) in COMBINED {
        let dir = fresh()?;
        let mut record = combined_case(&dir, rate, channels, depth, existing);
        record["case"] = json!(case);
        record["rate_bits"] = json!(format!("{:#018x}", rate.to_bits()));
        record["channels"] = json!(channels);
        record["depth"] = json!(depth);
        record["existing"] = json!(existing);
        combined.push(record);
    }
    let mut paths = Vec::new();
    for case in PATH_CASES {
        let dir = fresh()?;
        let mut record = path_case(&dir, case);
        record["case"] = json!(case);
        paths.push(record);
    }
    let mut misuse = Vec::new();
    for case in MISUSE_CASES {
        let dir = fresh()?;
        let mut record = misuse_case(&dir, case, &data)?;
        record["case"] = json!(case);
        misuse.push(record);
    }
    fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
    Ok(json!({"write_frames": WRITE_FRAMES, "depths": depths, "combined": combined, "paths": paths, "misuse": misuse}))
}

// ---- an independent set of Apple files --------------------------------------------------------------------------------------------

/// Encode a raw interleaved float32 file with the engine Writer in 512-frame blocks, normalise the clock. `Ok(false)`: the
/// configuration carries a warning (AudioToolbox refuses 256 kbit/s there; the engine aborts such a writer), nothing was written.
fn encode_f32_file(input: &Path, rate: u32, channels: usize, out: &Path) -> Res<bool> {
    let mut writer = Writer::create(out, config(f64::from(rate), channels as u32, 0)).map_err(|e| format!("create: {e}"))?;
    if !writer.warnings().is_empty() { let _ = writer.abort(); return Ok(false); }
    let mut file = fs::File::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let mut bytes = vec![0u8; BLOCK * channels * 4];
    let mut samples = vec![0f32; BLOCK * channels];
    loop {
        let mut filled = 0;
        while filled < bytes.len() {
            let n = file.read(&mut bytes[filled..]).map_err(|e| e.to_string())?;
            if n == 0 { break; }
            filled += n;
        }
        if filled == 0 { break; }
        if filled % (channels * 4) != 0 { return Err("input is not whole frames".into()); }
        let count = filled / 4;
        for (slot, chunk) in samples[..count].iter_mut().zip(bytes[..filled].chunks_exact(4)) { *slot = f32::from_le_bytes(chunk.try_into().unwrap()); }
        writer.write(&samples[..count]).map_err(|e| format!("write: {e}"))?;
        if filled < bytes.len() { break; }
    }
    writer.finish().map_err(|e| format!("finish: {e}"))?;
    set_mp4_header_times(out, FIXED_CLOCK).map_err(|e| format!("clock: {e}"))?;
    Ok(true)
}

/// A copy of the independent Apple file with the clock normalised the same way.
fn normalised_copy(source: &Path, copy: &Path) -> Res<Vec<u8>> {
    fs::copy(source, copy).map_err(|e| format!("{}: {e}", source.display()))?;
    set_mp4_header_times(copy, FIXED_CLOCK).map_err(|e| format!("clock {}: {e}", source.display()))?;
    fs::read(copy).map_err(|e| e.to_string())
}

fn first_difference(a: &[u8], b: &[u8]) -> Value {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(at) => json!({"offset": at, "where": input::locate(a, at)}),
        None => json!({"offset": a.len().min(b.len()), "where": "length"}),
    }
}

fn cmd_crosscheck(args: &[String]) -> Res<Value> {
    let corpus = PathBuf::from(args.first().ok_or("CORPUS_DIR")?);
    let runs = PathBuf::from(args.get(1).ok_or("RUNS_DIR")?);
    let threads: usize = opt(args, "--threads").map(|v| v.parse().map_err(|_| "--threads")).transpose()?.unwrap_or(3);
    let scratch = std::env::temp_dir().join(format!("encode-probe-crosscheck-{}", std::process::id()));
    make_dir(&scratch)?;
    let manifest: Value = serde_json::from_slice(&fs::read(corpus.join("manifest.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let items = manifest.as_array().ok_or("manifest")?.clone();
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::<Value>::new());
    std::thread::scope(|scope| {
        for t in 0..threads {
            let (items, next, results, scratch, corpus, runs) = (&items, &next, &results, &scratch, &corpus, &runs);
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                let id = item["id"].as_str().unwrap();
                let reference = runs.join(id).join("ext.m4a");
                let mut record = json!({"id": id});
                if !reference.exists() {
                    record["status"] = json!("no-reference");
                } else {
                    let (rate, channels) = (item["rate"].as_u64().unwrap() as u32, item["channels"].as_u64().unwrap() as usize);
                    let work = scratch.join(format!("{t}-{i}"));
                    let outcome = (|| -> Res<Value> {
                        make_dir(&work)?;
                        let mine = work.join("mine.m4a");
                        if !encode_f32_file(&corpus.join(item["file"].as_str().unwrap()), rate, channels, &mine)? { return Ok(json!({"status": "refused-warning"})); }
                        let mine_bytes = fs::read(&mine).map_err(|e| e.to_string())?;
                        let theirs = normalised_copy(&reference, &work.join("reference.m4a"))?;
                        let mut out = json!({"status": if mine_bytes == theirs { "identical" } else { "DIFFERENT" }, "length": mine_bytes.len()});
                        if mine_bytes != theirs { out["first_difference"] = first_difference(&mine_bytes, &theirs); out["reference_length"] = json!(theirs.len()); }
                        // the independent set's own access-unit file
                        if let (Ok(aus), Ok(mine_au)) = (fs::read(runs.join(id).join("ext.aus")), summarize(&mine_bytes)) {
                            out["aus_equal"] = json!(hex(&sha256(&aus)) == mine_au.stream_sha256);
                        }
                        Ok(out)
                    })();
                    let _ = fs::remove_dir_all(&work);
                    match outcome {
                        Ok(v) => { for (k, x) in v.as_object().unwrap() { record[k] = x.clone(); } }
                        Err(e) => { record["status"] = json!("ERROR"); record["error"] = json!(e); }
                    }
                }
                results.lock().unwrap().push(record);
            });
        }
    });
    let mut all = results.into_inner().unwrap();
    all.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let count = |s: &str| all.iter().filter(|r| r["status"] == s).count();
    let aus_bad = all.iter().filter(|r| r["aus_equal"] == false).count();
    let aus_checked = all.iter().filter(|r| r.get("aus_equal").is_some()).count();
    let problems: Vec<&Value> = all.iter().filter(|r| r["status"] != "identical" && r["status"] != "no-reference" && r["status"] != "refused-warning").collect();
    let summary = json!({"items": all.len(), "identical": count("identical"), "different": count("DIFFERENT"), "errors": count("ERROR"), "refused_warning": count("refused-warning"),
        "no_reference": count("no-reference"), "aus_checked": aus_checked, "aus_mismatch": aus_bad, "problems": problems});
    fs::remove_dir_all(&scratch).map_err(|e| e.to_string())?;
    Ok(summary)
}

fn cmd_layout(args: &[String]) -> Res<Value> {
    let dir = PathBuf::from(args.first().ok_or("LAYOUT_DIR")?);
    let scratch = std::env::temp_dir().join(format!("encode-probe-layout-{}", std::process::id()));
    make_dir(&scratch)?;
    let mut records = Vec::new();
    let mut names: Vec<String> = fs::read_dir(&dir).map_err(|e| e.to_string())?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("empty_") || n.starts_with("sil")).collect();
    names.sort();
    for name in names {
        let reference = dir.join(&name).join("ext.m4a");
        let (rate, channels, frames): (u32, usize, u64) = if let Some(rest) = name.strip_prefix("empty_") {
            let mut parts = rest.split('_');
            (parts.next().unwrap().parse().unwrap(), parts.next().unwrap().trim_end_matches("ch").parse().unwrap(), 0)
        } else {
            // silence at n access units: the fewest frames that need n of them
            let (rate, n): (u32, u64) = if name.starts_with("sil44_n") { (44100, name[7..].parse().unwrap()) } else { (48000, name[7..].parse().unwrap()) };
            (rate, 1, (n - 1) * 1024 - 2112 + 1)
        };
        let spec = Spec { kind: "silence".into(), rate, channels, frames, seed: 1, params: [0; 4] };
        let mine = scratch.join(format!("{name}.m4a"));
        let mut warned = false;
        if frames == 0 {
            // an empty file is finished even where the converter refuses 256 kbit/s (the writer then carries a warning)
            let writer = Writer::create(&mine, config(f64::from(rate), channels as u32, 0)).map_err(|e| format!("create {name}: {e}"))?;
            warned = !writer.warnings().is_empty();
            writer.finish().map_err(|e| format!("finish {name}: {e}"))?;
            set_mp4_header_times(&mine, FIXED_CLOCK).map_err(|e| format!("clock {name}: {e}"))?;
        } else {
            encode(&mine, &spec, BLOCK, None)?;
        }
        let mine_bytes = fs::read(&mine).map_err(|e| e.to_string())?;
        let theirs = normalised_copy(&reference, &scratch.join(format!("{name}.reference.m4a")))?;
        let mut record = json!({"name": name, "rate": rate, "channels": channels, "frames": frames, "warned": warned, "status": if mine_bytes == theirs { "identical" } else { "DIFFERENT" },
            "length": mine_bytes.len(), "layout": input::layout(&mine_bytes).iter().map(|(k, a, b)| format!("{k}:{a}-{b}")).collect::<Vec<_>>()});
        if mine_bytes != theirs { record["first_difference"] = first_difference(&mine_bytes, &theirs); record["reference_length"] = json!(theirs.len()); }
        let _ = fs::remove_file(&mine);
        records.push(record);
    }
    let _ = fs::remove_dir_all(&scratch);
    let identical = records.iter().filter(|r| r["status"] == "identical").count();
    Ok(json!({"files": records.len(), "identical": identical, "records": records}))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("items") => cmd_items(&args[1..]),
        Some("refusal") => cmd_refusal(&args[1..]),
        Some("create") => cmd_create(&args[1..]),
        Some("misc") => cmd_misc(&args[1..]),
        Some("crosscheck") => cmd_crosscheck(&args[1..]),
        Some("layout") => cmd_layout(&args[1..]),
        _ => Err("usage: encode_probe items|refusal|create|misc|crosscheck|layout ...".into()),
    };
    match result {
        Ok(value) => println!("{}", serde_json::to_string(&value).unwrap()),
        Err(error) => { eprintln!("encode_probe: {error}"); std::process::exit(2); }
    }
}
