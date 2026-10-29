// Drives the engine's own apple_codec::Reader exactly as the engine does for MP3 input (macOS only).
// Build: cargo build --release --offline  (this directory); `apple_probe info FILE` / `apple_probe dump FILE OUT.f32 planar|inter [BLOCK] [RATE]`.
#![allow(dead_code)]
#[path = "../../../../src/apple_codec.rs"]
mod apple_codec;
use apple_codec::Reader;
use std::io::Write;

fn arg(args: &[String], i: usize) -> &str { args.get(i).map(|s| s.as_str()).unwrap_or("") }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // apple_probe info <file>
    // apple_probe dump <file> <out.f32> <mode> [block] [rate]   mode = planar|inter
    match arg(&args, 1) {
        "info" => {
            let path = arg(&args, 2);
            match Reader::open(path, None) {
                Ok(r) => {
                    let s = r.source_format();
                    let frames = r.file_length_frames();
                    let br = r.bitrate();
                    println!("{{\"ok\":true,\"rate\":{},\"format_id\":{},\"flags\":{},\"bytes_per_packet\":{},\"frames_per_packet\":{},\"bytes_per_frame\":{},\"channels\":{},\"bits\":{},\"length_frames\":{},\"bitrate\":{}}}",
                        s.sample_rate, s.format_id, s.format_flags, s.bytes_per_packet, s.frames_per_packet, s.bytes_per_frame, s.channels_per_frame, s.bits_per_channel,
                        frames.map(|v| v.to_string()).unwrap_or_else(|e| format!("\"{e}\"")),
                        br.map(|v| v.to_string()).unwrap_or_else(|e| format!("\"{e}\"")));
                }
                Err(e) => println!("{{\"ok\":false,\"error\":{:?}}}", e.to_string()),
            }
        }
        "dump" => {
            let path = arg(&args, 2);
            let out = arg(&args, 3);
            let mode = arg(&args, 4);
            let block: usize = arg(&args, 5).parse().unwrap_or(512);
            let rate: Option<f64> = args.get(6).and_then(|s| s.parse().ok());
            let mut r = match Reader::open(path, rate) { Ok(r) => r, Err(e) => { println!("{{\"ok\":false,\"stage\":\"open\",\"error\":{:?}}}", e.to_string()); return; } };
            let s = r.source_format();
            let ch = s.channels_per_frame as usize;
            let mut sink = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
            let mut total = 0usize;
            let mut reads = 0usize;
            let mut err: Option<String> = None;
            let mut short_reads = 0usize;
            if mode == "planar" {
                let rr = rate.unwrap_or(s.sample_rate);
                if let Err(e) = r.set_stereo_planar(rr) { println!("{{\"ok\":false,\"stage\":\"planar\",\"error\":{:?}}}", e.to_string()); return; }
                let mut left = vec![0.0f32; block]; let mut right = vec![0.0f32; block];
                loop {
                    left.fill(0.0); right.fill(0.0);
                    match r.read_stereo_planar(&mut left, &mut right) {
                        Ok(0) => break,
                        Ok(n) => { reads += 1; if n < block { short_reads += 1; }
                            for i in 0..n { sink.write_all(&left[i].to_le_bytes()).unwrap(); sink.write_all(&right[i].to_le_bytes()).unwrap(); } total += n; }
                        Err(e) => { err = Some(e.to_string()); break; }
                    }
                }
            } else {
                let mut buf = vec![0.0f32; block * ch];
                loop {
                    match r.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => { reads += 1; if n < block { short_reads += 1; }
                            for v in &buf[..n * ch] { sink.write_all(&v.to_le_bytes()).unwrap(); } total += n; }
                        Err(e) => { err = Some(e.to_string()); break; }
                    }
                }
            }
            sink.flush().unwrap();
            println!("{{\"ok\":{},\"frames\":{},\"reads\":{},\"short_reads\":{},\"channels\":{},\"error\":{}}}", err.is_none(), total, reads, short_reads, ch,
                err.map(|e| format!("{e:?}")).unwrap_or("null".into()));
        }
        _ => eprintln!("usage"),
    }
}
