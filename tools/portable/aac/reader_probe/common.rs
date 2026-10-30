// Differential probe of the engine's `Reader` (macOS): the same scenarios are run against the native `apple_codec::Reader` (`apple/`, real
// AudioToolbox) and the portable one (`portable/`, `--features portable`), and `compare.py` lists the differences. Used to establish the AAC
// routing, the statuses of broken files, `bitrate`, `file_length_frames` and the bytes behind short reads.
//
//   probe LIST OUT.tsv [scenario-substring]      LIST lines: `id<TAB>path` (the file name's extension is the type hint AudioFile sees)
//   OUT.tsv columns: id, scenario, status, frames returned, calls, FNV-1a of the WHOLE client buffer after every call (+ the returned count), detail
//   scenarios: `open` (source / client ASBD, file length, bitrate or the error), `ilv<N>[s|z]` interleaved reads of N frames, `pl<N>[s|z]` planar
//   stereo reads; `s` prefills the buffer with a NaN sentinel, `z` with zeros; a short read reports how many frames behind the returned count
//   stayed sentinel / zero / were written. After a failed read two more reads must answer Poisoned.
//
// Build: `cd apple && cargo build --release --offline`, `cd portable && cargo build --release --offline` (each is its own workspace).
// Shared body of the Apple and portable Reader probes. Expects `Reader` and `CodecError` in scope.
// usage: probe LIST OUT.tsv [scenario-filter]   (LIST lines: id<TAB>path)
use std::io::Write as _;

fn fnv(h: &mut u64, bytes: &[u8]) { for &b in bytes { *h ^= u64::from(b); *h = h.wrapping_mul(0x100_0000_01b3); } }
fn desc(e: &CodecError) -> String {
    match e {
        CodecError::Native { operation, status } => format!("native/{operation}/{status}"),
        other => format!("other/{}", other.to_string().replace('\t', " ").replace('\n', " ")),
    }
}
const SENT: u32 = 0x7fc0_5a5a;

fn probe_file(out: &mut impl std::io::Write, id: &str, path: &str, filter: &str) {
    let want = |s: &str| filter.is_empty() || s.contains(filter);
    let row = |out: &mut dyn std::io::Write, scn: &str, status: &str, frames: u64, calls: u64, hash: u64, detail: &str| {
        let _ = writeln!(out, "{id}\t{scn}\t{status}\t{frames}\t{calls}\t{hash:016x}\t{detail}");
    };
    // open + facts
    let mut fact_rate = 0.0;
    let mut ch = 0usize;
    if want("open") {
        match Reader::open(path, None) {
            Ok(r) => {
                let s = r.source_format();
                fact_rate = s.sample_rate;
                ch = s.channels_per_frame as usize;
                let len = r.file_length_frames().map(|v| v.to_string()).unwrap_or_else(|e| format!("ERR:{}", desc(&e)));
                let br = r.bitrate().map(|v| v.to_string()).unwrap_or_else(|e| format!("ERR:{}", desc(&e)));
                let c = r.client_format();
                let detail = format!("rate={};fmt={:08x};flags={};bpp={};fpp={};bpf={};ch={};bits={};len={};br={};crate={};cfmt={:08x};cflags={};cbpp={};cfpp={};cbpf={};cch={};cbits={}",
                    s.sample_rate, s.format_id, s.format_flags, s.bytes_per_packet, s.frames_per_packet, s.bytes_per_frame, s.channels_per_frame, s.bits_per_channel, len, br,
                    c.sample_rate, c.format_id, c.format_flags, c.bytes_per_packet, c.frames_per_packet, c.bytes_per_frame, c.channels_per_frame, c.bits_per_channel);
                row(out, "open", "ok", 0, 0, 0, &detail);
            }
            Err(e) => { row(out, "open", "err", 0, 0, 0, &desc(&e)); return; }
        }
    } else if let Ok(r) = Reader::open(path, None) { let s = r.source_format(); fact_rate = s.sample_rate; ch = s.channels_per_frame as usize; }
    if ch == 0 { return; }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    // interleaved reads: (name, block, prefill-sentinel)
    let mut scenarios: Vec<(String, usize, bool, bool)> = vec![
        ("ilv512s".into(), 512, true, false), ("ilv512z".into(), 512, false, false), ("ilv1025s".into(), 1025, true, false),
        ("ilv4096s".into(), 4096, true, false), ("ilv1024s".into(), 1024, true, false), ("pl512z".into(), 512, false, true), ("pl512s".into(), 512, true, true), ("pl4096s".into(), 4096, true, true),
        ("pl1025s".into(), 1025, true, true),
    ];
    if size < 400_000 { scenarios.push(("ilv7s".into(), 7, true, false)); scenarios.push(("ilv333s".into(), 333, true, false)); scenarios.push(("pl333s".into(), 333, true, true)); }
    if size < 30_000 { scenarios.push(("ilv1s".into(), 1, true, false)); scenarios.push(("pl1s".into(), 1, true, true)); }
    for (name, block, sent, planar) in scenarios {
        if !want(&name) { continue; }
        let mut r = match Reader::open(path, if planar { Some(fact_rate) } else { None }) { Ok(r) => r, Err(e) => { row(out, &name, "openerr", 0, 0, 0, &desc(&e)); continue; } };
        if planar {
            if let Err(e) = r.set_stereo_planar(fact_rate) { row(out, &name, "cfgerr", 0, 0, 0, &desc(&e)); continue; }
        }
        let fill = if sent { f32::from_bits(SENT) } else { 0.0 };
        let mut buf = vec![fill; block * ch];
        let (mut left, mut right) = (vec![fill; block], vec![fill; block]);
        let (mut frames, mut calls, mut hash) = (0u64, 0u64, 0xcbf2_9ce4_8422_2325u64);
        let mut status = "ok".to_string();
        let mut detail = String::new();
        let mut zero_returns = 0;
        let mut tail_note = String::new();
        loop {
            if calls > 4_000_000 { status = "toomanycalls".into(); break; }
            buf.fill(fill); left.fill(fill); right.fill(fill);
            let res = if planar { r.read_stereo_planar(&mut left, &mut right) } else { r.read(&mut buf) };
            calls += 1;
            match res {
                Ok(n) => {
                    let bytes: Vec<u8> = if planar { left.iter().chain(right.iter()).flat_map(|v| v.to_bits().to_le_bytes()).collect() } else { buf.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect() };
                    if n > 0 && n < block {
                        let (mut sent_f, mut zero_f, mut other_f) = (0, 0, 0);
                        for f in n..block {
                            let vals: Vec<f32> = if planar { vec![left[f], right[f]] } else { buf[f * ch..(f + 1) * ch].to_vec() };
                            if vals.iter().all(|v| v.to_bits() == SENT) { sent_f += 1 } else if vals.iter().all(|v| v.to_bits() == 0) { zero_f += 1 } else { other_f += 1 }
                        }
                        tail_note = format!("shortread n={n} tail sentinel={sent_f} zero={zero_f} data={other_f}");
                    }
                    fnv(&mut hash, &(n as u64).to_le_bytes());
                    fnv(&mut hash, &bytes);
                    if n == 0 { zero_returns += 1; if zero_returns >= 2 { break; } continue; }
                    frames += n as u64;
                }
                Err(e) => {
                    status = "err".into(); detail = desc(&e);
                    // poisoning: the next calls must fail too
                    let mut again = Vec::new();
                    for _ in 0..2 { let x = if planar { r.read_stereo_planar(&mut left, &mut right) } else { r.read(&mut buf) }; again.push(match x { Ok(n) => format!("ok{n}"), Err(e) => desc(&e) }); }
                    detail.push_str(&format!(" | after: {}", again.join(" , ")));
                    break;
                }
            }
        }
        if !tail_note.is_empty() { if !detail.is_empty() { detail.push_str(" | "); } detail.push_str(&tail_note); }
        row(out, &name, &status, frames, calls, hash, &detail);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let list = std::fs::read_to_string(&args[1]).expect("list");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[2]).expect("out"));
    let filter = args.get(3).cloned().unwrap_or_default();
    for line in list.lines() {
        let Some((id, path)) = line.split_once('\t') else { continue };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe_file(&mut out, id, path, &filter)));
        if r.is_err() { let _ = writeln!(out, "{id}\tpanic\tpanic\t0\t0\t0\t"); }
        let _ = out.flush();
    }
}
