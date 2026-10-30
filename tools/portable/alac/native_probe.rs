//! Generate a deterministic, create-only AudioToolbox ALAC corpus on macOS.
//! Usage: native-probe NEW_OUTPUT_DIRECTORY
#![cfg(target_os = "macos")]
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[repr(C)]
struct Asbd { rate: f64, format: u32, flags: u32, packet_bytes: u32, packet_frames: u32,
    frame_bytes: u32, channels: u32, bits: u32, reserved: u32 }
#[repr(C)]
struct Buffer { channels: u32, bytes: u32, data: *mut c_void }
#[repr(C)]
struct BufferList { count: u32, buffer: Buffer }
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn ExtAudioFileCreateWithURL(url: *const c_void, file_type: u32, format: *const Asbd, layout: *const c_void, flags: u32, out: *mut *mut c_void) -> i32;
    fn ExtAudioFileSetProperty(file: *mut c_void, property: u32, size: u32, data: *const c_void) -> i32;
    fn ExtAudioFileWrite(file: *mut c_void, frames: u32, data: *const BufferList) -> i32;
    fn ExtAudioFileDispose(file: *mut c_void) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(allocator: *const c_void, bytes: *const u8, len: isize, directory: u8) -> *const c_void;
    fn CFRelease(value: *const c_void);
}
fn be(s: &[u8;4]) -> u32 { u32::from_be_bytes(*s) }
pub(crate) fn encode(path: &Path, rate: u32, channels: u32, depth: u32, input: &[f32]) -> Result<(), String> {
    encode_fractional(path, f64::from(rate), channels, depth, input)
}
pub(crate) fn encode_fractional(path: &Path, rate: f64, channels: u32, depth: u32, input: &[f32]) -> Result<(), String> {
    if path.exists() { return Err("output already exists".into()); }
    let path_bytes = path.as_os_str().as_bytes();
    let url = unsafe { CFURLCreateFromFileSystemRepresentation(std::ptr::null(), path_bytes.as_ptr(), path_bytes.len() as isize, 0) };
    if url.is_null() { return Err("URL allocation failed".into()); }
    let dst = Asbd { rate, format: be(b"alac"), flags: match depth {16=>1,20=>2,24=>3,32=>4,_=>unreachable!()}, packet_bytes: 0,
        packet_frames: 0, frame_bytes: 0, channels, bits: 0, reserved: 0 };
    let src = Asbd { rate, format: be(b"lpcm"), flags: 9, packet_bytes: channels*4,
        packet_frames: 1, frame_bytes: channels*4, channels, bits: 32, reserved: 0 };
    let mut file = std::ptr::null_mut();
    let status = unsafe { ExtAudioFileCreateWithURL(url, be(b"m4af"), &dst, std::ptr::null(), 0, &mut file) };
    unsafe { CFRelease(url); }
    if status != 0 || file.is_null() { return Err(format!("create status {status}")); }
    let configured = unsafe { ExtAudioFileSetProperty(file, be(b"cfmt"), std::mem::size_of::<Asbd>() as u32, (&src as *const Asbd).cast()) };
    let mut failure = if configured == 0 { None } else { Some(format!("configure status {configured}")) };
    if failure.is_none() {
        for chunk in input.chunks(channels as usize * 512) {
            let data = BufferList { count: 1, buffer: Buffer { channels, bytes: (chunk.len()*4) as u32, data: chunk.as_ptr().cast_mut().cast() } };
            let status = unsafe { ExtAudioFileWrite(file, (chunk.len() / channels as usize) as u32, &data) };
            if status != 0 { failure = Some(format!("write status {status}")); break; }
        }
    }
    let close = unsafe { ExtAudioFileDispose(file) };
    if close != 0 { return Err(format!("close status {close}, prior {failure:?}")); }
    match failure { Some(e) => Err(e), None => Ok(()) }
}
pub(crate) fn signal(kind: &str, channels: usize, frames: usize) -> Vec<f32> {
    const TONE: [i32;16] = [0,12540,23170,30274,32767,30274,23170,12540,0,-12540,-23170,-30274,-32768,-30274,-23170,-12540];
    let mut rng = 0x5eed_1234u32;
    let mut data = Vec::with_capacity(channels * frames);
    for frame in 0..frames { for channel in 0..channels {
        rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5;
        let x = match kind {
            "silence" => 0.0,
            "tone" => TONE[(frame + 3*channel) % 16] as f32 * (1.0/32768.0),
            "noise" => (rng as i32) as f32 * (1.0/2147483648.0),
            "fullscale" => [-1.0, 1.0, f32::from_bits(0x3f7fffff), -f32::from_bits(0x3f7fffff)][(frame + channel) % 4],
            "clipped" => [-2.0, -1.0001, 1.0001, 2.0, 0.0, 0.5, -0.5][(frame + channel) % 7],
            "transient" => if frame % 257 == 0 { if channel % 2 == 0 { 1.0 } else { -1.0 } } else if frame % 1024 == 1 { 0.001 } else { 0.0 },
            _ => unreachable!(),
        };
        data.push(x);
    }}
    data
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let dir = PathBuf::from(args.next().ok_or("new output directory required")?);
    if args.next().is_some() || !dir.is_absolute() { return Err("provide one absolute new output directory".into()); }
    fs::create_dir(&dir)?;
    let inputs = dir.join("inputs"); fs::create_dir(&inputs)?;
    let outputs = dir.join("outputs"); fs::create_dir(&outputs)?;
    let mut manifest = OpenOptions::new().write(true).create_new(true).open(dir.join("manifest.tsv"))?;
    writeln!(manifest, "model\tAnthropic claude-opus-5-5 xhigh")?;
    writeln!(manifest, "seed\t0x5eed1234")?;
    writeln!(manifest, "signal\tchannels\tframes\tdepth\trate\tinput\toutput\tstatus")?;
    let mut count = 0;
    let mut errors = 0;
    for kind in ["silence", "tone", "noise", "fullscale", "clipped", "transient"] {
        for channels in [1usize, 2, 6] {
            for frames in [0usize, 1, 4095, 4096, 4097, 65537] {
                let data = signal(kind, channels, frames);
                let input_name = format!("{kind}-{channels}-{frames}.f32le");
                let mut input = OpenOptions::new().write(true).create_new(true).open(inputs.join(&input_name))?;
                for sample in &data { input.write_all(&sample.to_bits().to_le_bytes())?; }
                input.sync_all()?;
                for depth in [16u32, 20, 24, 32] {
                    for rate in [22050u32, 32000, 44100, 48000, 88200, 96000] {
                        if ![22050, 44100, 96000].contains(&rate) && ![1, 4097].contains(&frames) { continue; }
                        let name = format!("{kind}-{channels}-{frames}-{depth}-{rate}.m4a");
                        let path = outputs.join(&name);
                        let result = encode(&path, rate, channels as u32, depth, &data);
                        let status = match result { Ok(()) => "ok".to_owned(), Err(e) => { errors += 1; e } };
                        writeln!(manifest, "{kind}\t{channels}\t{frames}\t{depth}\t{rate}\tinputs/{input_name}\toutputs/{name}\t{status}")?;
                        count += 1;
                    }
                }
            }
        }
    }
    manifest.sync_all()?;
    println!("cases={count} errors={errors} output={}", dir.display());
    if errors != 0 { std::process::exit(1); }
    Ok(())
}
