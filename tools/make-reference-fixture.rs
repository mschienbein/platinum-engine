use std::fs::OpenOptions;
use std::io::{BufWriter, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: make-reference-fixture NEW_OUTPUT.wav")?;
    let rate = 48_000_u32;
    let frames = rate * 16;
    let data_bytes = frames * 4;
    let file = OpenOptions::new().write(true).create_new(true).open(&path)?;
    let mut out = BufWriter::new(file);
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data_bytes).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16_u32.to_le_bytes())?;
    out.write_all(&1_u16.to_le_bytes())?;
    out.write_all(&2_u16.to_le_bytes())?;
    out.write_all(&rate.to_le_bytes())?;
    out.write_all(&(rate * 4).to_le_bytes())?;
    out.write_all(&4_u16.to_le_bytes())?;
    out.write_all(&16_u16.to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&data_bytes.to_le_bytes())?;
    let mut noise = 0x9e37_79b9_u32;
    let mut clipped_samples = 0_u64;
    for frame in 0..frames {
        noise ^= noise << 13;
        noise ^= noise >> 17;
        noise ^= noise << 5;
        let t = f64::from(frame) / f64::from(rate);
        let phase = std::f64::consts::TAU * 443.0 * t;
        let amplitude = if t < 0.5 || t >= 15.5 {
            0.0
        } else if t < 4.0 {
            0.35
        } else if t < 8.0 {
            1.35
        } else if t < 12.0 {
            0.05 + 0.75 * (t - 8.0) / 4.0
        } else {
            let pulse_frame = frame % (rate / 4);
            0.1 + 1.2 * (1.0 - f64::from(pulse_frame) / f64::from(rate / 4)).powi(6)
        };
        let random = (f64::from(noise) / f64::from(u32::MAX) - 0.5) * 0.04;
        let left = amplitude * (phase.sin() + 0.17 * (phase * 3.0).sin() + random);
        let right = amplitude * ((phase + 0.31).sin() + 0.13 * (phase * 2.0).sin() - random);
        let mut encoded = [0_u8; 4];
        for (channel, value) in [left, right].into_iter().enumerate() {
            if !(-1.0..=1.0).contains(&value) {
                clipped_samples += 1;
            }
            let sample = (value.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            encoded[channel * 2..channel * 2 + 2].copy_from_slice(&sample.to_le_bytes());
        }
        out.write_all(&encoded)?;
    }
    out.flush()?;
    println!("path={path}; frames={frames}; sample_rate={rate}; channels=2; bits=16; saturated_samples={clipped_samples}");
    Ok(())
}
