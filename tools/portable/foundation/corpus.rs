//! Raw reference corpus for cross-platform Foundation parity tests.
use crate::metadata::{Marker, MarkerText};
use std::io::{self, Read, Write};

pub const MAGIC: &[u8; 8] = b"PNFOUND1";
pub const COUNT: u64 = 100_000;

fn write_u32(w: &mut impl Write, n: u32) -> io::Result<()> { w.write_all(&n.to_le_bytes()) }
fn write_u64(w: &mut impl Write, n: u64) -> io::Result<()> { w.write_all(&n.to_le_bytes()) }
fn read_u8(r: &mut impl Read) -> io::Result<u8> { let mut b = [0; 1]; r.read_exact(&mut b)?; Ok(b[0]) }
fn read_u32(r: &mut impl Read) -> io::Result<u32> { let mut b = [0; 4]; r.read_exact(&mut b)?; Ok(u32::from_le_bytes(b)) }
fn read_u64(r: &mut impl Read) -> io::Result<u64> { let mut b = [0; 8]; r.read_exact(&mut b)?; Ok(u64::from_le_bytes(b)) }
fn invalid() -> io::Error { io::Error::new(io::ErrorKind::InvalidData, "invalid Foundation corpus") }
fn bytes(r: &mut impl Read, n: u32) -> io::Result<Vec<u8>> {
    if n > 16 * 1024 * 1024 { return Err(invalid()); }
    let mut b = vec![0; n as usize];
    r.read_exact(&mut b)?;
    Ok(b)
}
fn write_text(w: &mut impl Write, text: Option<&MarkerText>) -> io::Result<()> {
    match text {
        None => w.write_all(&[0]),
        Some(MarkerText::Utf8(s)) => {
            w.write_all(&[1])?;
            write_u32(w, u32::try_from(s.len()).map_err(|_| invalid())?)?;
            w.write_all(s.as_bytes())
        }
        Some(MarkerText::Utf16(s)) => {
            w.write_all(&[2])?;
            write_u32(w, u32::try_from(s.len()).map_err(|_| invalid())?)?;
            for ch in s { w.write_all(&ch.to_le_bytes())?; }
            Ok(())
        }
    }
}
fn read_text(r: &mut impl Read) -> io::Result<Option<MarkerText>> {
    match read_u8(r)? {
        0 => Ok(None),
        1 => {
            let len = read_u32(r)?;
            let text = String::from_utf8(bytes(r, len)?).map_err(|_| invalid())?;
            Ok(Some(MarkerText::Utf8(text)))
        }
        2 => {
            let n = read_u32(r)?;
            let data = bytes(r, n.checked_mul(2).ok_or_else(invalid)?)?;
            let units = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            Ok(Some(MarkerText::Utf16(units)))
        }
        _ => Err(invalid()),
    }
}
pub fn write_marker(w: &mut impl Write, m: &Marker) -> io::Result<()> {
    for text in [m.os_type.as_ref(), m.software_version.as_ref(), m.backup_file_name.as_ref(), m.volume_units.as_ref()] {
        write_text(w, text)?;
    }
    let flags = u8::from(m.is_processed) | (u8::from(m.applied_volume_adjustment) << 1)
        | (u8::from(m.applied_clip_repair) << 2) | (u8::from(m.applied_pitch_correction) << 3)
        | (u8::from(m.applied_warmth) << 4) | (u8::from(m.applied_expansion) << 5)
        | (u8::from(m.applied_compression) << 6);
    w.write_all(&[flags])?;
    for n in [m.volume_analysis_version, m.clip_repair_count, m.warmth_preset] { w.write_all(&n.to_le_bytes())?; }
    for n in [m.target_volume, m.input_volume, m.output_volume, m.input_pitch, m.output_pitch, m.speed_adjustment] { write_u64(w, n.to_bits())?; }
    Ok(())
}
pub fn read_marker(r: &mut impl Read) -> io::Result<Marker> {
    let os_type = read_text(r)?;
    let software_version = read_text(r)?;
    let backup_file_name = read_text(r)?;
    let volume_units = read_text(r)?;
    let flags = read_u8(r)?;
    if flags & 0x80 != 0 { return Err(invalid()); }
    let volume_analysis_version = read_u32(r)? as i32;
    let clip_repair_count = read_u32(r)? as i32;
    let warmth_preset = read_u32(r)? as i32;
    let target_volume = f64::from_bits(read_u64(r)?);
    let input_volume = f64::from_bits(read_u64(r)?);
    let output_volume = f64::from_bits(read_u64(r)?);
    let input_pitch = f64::from_bits(read_u64(r)?);
    let output_pitch = f64::from_bits(read_u64(r)?);
    let speed_adjustment = f64::from_bits(read_u64(r)?);
    Ok(Marker { os_type, software_version, backup_file_name, volume_units,
        is_processed: flags & 1 != 0, applied_volume_adjustment: flags & 2 != 0,
        applied_clip_repair: flags & 4 != 0, applied_pitch_correction: flags & 8 != 0,
        applied_warmth: flags & 16 != 0, applied_expansion: flags & 32 != 0,
        applied_compression: flags & 64 != 0, volume_analysis_version, clip_repair_count,
        warmth_preset, target_volume, input_volume, output_volume, input_pitch, output_pitch,
        speed_adjustment })
}
pub enum Expected { Nil, Bytes(Vec<u8>, Result<Marker, String>) }
pub fn write_case(w: &mut impl Write, input: &Marker, json: Option<&[u8]>, parsed: Option<Result<&Marker, String>>) -> io::Result<()> {
    write_marker(w, input)?;
    match json {
        None => w.write_all(&[0]),
        Some(json) => {
            w.write_all(&[1])?;
            write_u32(w, u32::try_from(json.len()).map_err(|_| invalid())?)?;
            w.write_all(json)?;
            match parsed.ok_or_else(invalid)? {
                Ok(marker) => { w.write_all(&[0])?; write_marker(w, marker) }
                Err(error) => { w.write_all(&[1])?; write_u32(w, u32::try_from(error.len()).map_err(|_| invalid())?)?; w.write_all(error.as_bytes()) }
            }
        }
    }
}
pub fn read_case(r: &mut impl Read) -> io::Result<(Marker, Expected)> {
    let input = read_marker(r)?;
    let expected = match read_u8(r)? {
        0 => Expected::Nil,
        1 => {
            let n = read_u32(r)?;
            let json = bytes(r, n)?;
            let parsed = match read_u8(r)? {
                0 => Ok(read_marker(r)?),
                1 => { let n = read_u32(r)?; Err(String::from_utf8(bytes(r, n)?).map_err(|_| invalid())?) },
                _ => return Err(invalid()),
            };
            Expected::Bytes(json, parsed)
        }
        _ => return Err(invalid()),
    };
    Ok((input, expected))
}
pub fn marker_equal(a: &Marker, b: &Marker) -> bool {
    a == b && [a.target_volume.to_bits(), a.input_volume.to_bits(), a.output_volume.to_bits(),
        a.input_pitch.to_bits(), a.output_pitch.to_bits(), a.speed_adjustment.to_bits()]
        == [b.target_volume.to_bits(), b.input_volume.to_bits(), b.output_volume.to_bits(),
        b.input_pitch.to_bits(), b.output_pitch.to_bits(), b.speed_adjustment.to_bits()]
}
