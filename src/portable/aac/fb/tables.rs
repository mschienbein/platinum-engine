//! Binary tables dumped from the AudioCodecs arm64e slice (SHA-256 `70e5d4a56b24d037b940328916df496a20267eb206f8e316216ae7bef69a1626`), embedded
//! verbatim (the filterbank is the same transform in both directions): the sine/KBD windows and the DCT-IV modulation coefficients differ
//! from correctly rounded `sin`/`cos` in 1 ulp at several hundred entries, so they must be taken from Apple's binary, never recomputed.

const fn load<const N: usize>(bytes: &[u8]) -> [f32; N] {
    assert!(bytes.len() == 4 * N);
    let mut out = [0.0f32; N];
    let mut i = 0;
    while i < N {
        out[i] = f32::from_bits(u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]]));
        i += 1;
    }
    out
}

/// `kSineWindow_1024` (0x660890): rising half of the long sine window, `sin(pi (i + 1/2) / 2048)` (Apple's rounding).
pub static SINE_LONG: [f32; 1024] = load(include_bytes!("tables/kSineWindow_1024.f32"));
/// `kSineWindow_128` (0x661890): rising half of the short sine window.
pub static SINE_SHORT: [f32; 128] = load(include_bytes!("tables/kSineWindow_128.f32"));
/// `kKBDWindow_1024` (0x662bf0): rising half of the long KBD window (alpha 4). Never selected by the engine configuration.
pub static KBD_LONG: [f32; 1024] = load(include_bytes!("tables/kKBDWindow_1024.f32"));
/// `kKBDWindow_128` (0x663bf0): rising half of the short KBD window (alpha 6). Never selected by the engine configuration.
pub static KBD_SHORT: [f32; 128] = load(include_bytes!("tables/kKBDWindow_128.f32"));
/// `kDCTIV_ModulationCoefs_1024` (0x65cae0): `c[2k] ~ cos(pi (8k+1) / 8192)`, `c[2k+1] ~ sin(...)`.
pub static MOD_LONG: [f32; 1024] = load(include_bytes!("tables/kDCTIV_ModulationCoefs_1024.f32"));
/// `kDCTIV_ModulationCoefs_128` (0x65dae0).
pub static MOD_SHORT: [f32; 128] = load(include_bytes!("tables/kDCTIV_ModulationCoefs_128.f32"));

/// Rising long window of window shape `shape` (0 = sine, 1 = KBD).
#[inline]
pub fn long_window(shape: u8) -> &'static [f32; 1024] {
    if shape == 0 { &SINE_LONG } else { &KBD_LONG }
}
/// Rising short window of window shape `shape`.
#[inline]
pub fn short_window(shape: u8) -> &'static [f32; 128] {
    if shape == 0 { &SINE_SHORT } else { &KBD_SHORT }
}

#[cfg(test)]
mod tests {
    use crate::portable::digest::sha256;

    /// The embedded dumps are the byte-identical files recorded with these digests.
    #[test]
    fn dumps_have_their_recorded_digests() {
        for (bytes, want) in [
            (include_bytes!("tables/kSineWindow_1024.f32").as_slice(), "1b44dedd53c577d9298b784de7c4ec346f939dbf1e0886de93aeb1d211c5587c"),
            (include_bytes!("tables/kSineWindow_128.f32").as_slice(), "4a2c07e75def06dce3b3b106d6f14682f189cb62d65bdaca87a44220191cc638"),
            (include_bytes!("tables/kKBDWindow_1024.f32").as_slice(), "83c778652bcf9e7dec5618645e60286e0d01afd16eed7fbe64a709cd77c96969"),
            (include_bytes!("tables/kKBDWindow_128.f32").as_slice(), "7f609d868306857e9542c8121182093e23763c8f50adb84ec474863f325b2014"),
            (include_bytes!("tables/kDCTIV_ModulationCoefs_1024.f32").as_slice(), "e4fdb42812d779949b002fb28f7b582ebaf70cd5ecd4896ca62d0711fb43c3fb"),
            (include_bytes!("tables/kDCTIV_ModulationCoefs_128.f32").as_slice(), "c3be54e2acbde891e081c0666f7b83db2229e08920e896c41ed98d9dc688886a"),
        ] {
            let got: String = sha256(bytes).iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(got, want);
        }
    }
}
