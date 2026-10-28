//! In-memory, compact malformed sample-table fixtures; no external files needed.
fn find(bytes: &[u8], kind: &[u8; 4]) -> usize {
    bytes.windows(4).position(|value| value == kind).expect("fixture atom")
}
fn get32(bytes: &[u8], index: usize) -> u32 {
    u32::from_be_bytes(bytes[index..index + 4].try_into().unwrap())
}
fn set32(bytes: &mut [u8], index: usize, value: u32) {
    bytes[index..index + 4].copy_from_slice(&value.to_be_bytes());
}
fn fix_offset(bytes: &mut [u8]) {
    let offset = find(bytes, b"mdat") + 4;
    let stco = find(bytes, b"stco");
    set32(bytes, stco + 12, offset as u32);
}
fn fixed_size(mut bytes: Vec<u8>, size: u32, count: u32) -> Vec<u8> {
    let stsz = find(&bytes, b"stsz");
    assert_eq!(get32(&bytes, stsz - 4), 24);
    set32(&mut bytes, stsz - 4, 20);
    set32(&mut bytes, stsz + 8, size);
    set32(&mut bytes, stsz + 12, count);
    bytes.drain(stsz + 16..stsz + 20);
    for kind in [b"moov", b"trak", b"mdia", b"minf", b"stbl"] {
        let size_at = find(&bytes, kind) - 4;
        let size = get32(&bytes, size_at);
        set32(&mut bytes, size_at, size - 4);
    }
    fix_offset(&mut bytes);
    bytes
}
/// Return one valid compact file, one valid fixed-size variant, and malformed
/// files whose reported stsz/stts/stsc/stco counts or offsets are hostile.
pub fn fixtures(mut bytes: Vec<u8>) -> (Vec<u8>, Vec<u8>, Vec<(&'static str, Vec<u8>)>) {
    let movie = get32(&bytes, 28) as usize;
    let free = 28 + movie;
    assert_eq!(&bytes[free + 4..free + 8], b"free");
    let free_size = get32(&bytes, free) as usize;
    bytes.drain(free..free + free_size);
    fix_offset(&mut bytes);
    let mdat = find(&bytes, b"mdat");
    let packet_size = get32(&bytes, mdat - 4) - 8;
    assert!(bytes.len() < 1024 && packet_size >= 3);
    let valid_fixed = fixed_size(bytes.clone(), packet_size, 1);

    let mut tiny = fixed_size(bytes.clone(), 3, 16_000_000);
    let stts = find(&tiny, b"stts");
    set32(&mut tiny, stts + 12, 16_000_000);
    let stsc = find(&tiny, b"stsc");
    set32(&mut tiny, stsc + 16, 16_000_000);
    let mdat = find(&tiny, b"mdat");
    set32(&mut tiny, mdat - 4, 11);
    tiny.truncate(mdat + 4 + 3);
    assert!(tiny.len() < 1024);
    let mut bad = vec![("fixed_count_16m_3byte_mdat", tiny)];
    for (name, kind, delta) in [
        ("stsz_count", b"stsz", 12), ("stts_count", b"stts", 8),
        ("stts_repeat", b"stts", 12), ("stsc_count", b"stsc", 8),
        ("stsc_samples", b"stsc", 16), ("stco_count", b"stco", 8),
    ] {
        let mut file = bytes.clone();
        let at = find(&file, kind) + delta;
        set32(&mut file, at, 16_000_000);
        bad.push((name, file));
    }
    let mut file = bytes.clone();
    let at = find(&file, b"stco") + 12;
    set32(&mut file, at, 1);
    bad.push(("stco_offset", file));
    (bytes, valid_fixed, bad)
}
