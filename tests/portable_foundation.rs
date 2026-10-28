#![cfg(target_os = "macos")]
pub use platinum_engine::metadata;
#[allow(dead_code)]
#[path = "../tools/portable/foundation/corpus.rs"]
mod corpus;
use platinum_engine::metadata::{Marker, ProcessingMarker};

fn marker() -> Marker {
    Marker::from_processing(ProcessingMarker {
        software_version: Some("10.5.0.1256"), backup_file_name: Some("/synthetic/input.wav"),
        applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
        expansion_applied: 0, compression_applied: 0, volume_analysis_version: 4,
        target_volume: -12.3, input_volume: -15.2, output_volume: -12.2,
        clip_repair_count: 0, exciter_option: 0, input_pitch: 0.0, output_pitch: 0.0,
        speed_adjustment: 1.0,
    })
}

#[test]
fn corpus_codec_roundtrip() {
    use platinum_engine::metadata::{MarkerText, encode_marker_base64};
    let mut m = marker();
    m.backup_file_name = Some(MarkerText::Utf16(vec![0xd800]));
    m.target_volume = -0.0;
    m.input_pitch = f64::from_bits(1);
    let mut data = Vec::new();
    corpus::write_case(&mut data, &m, None, None).unwrap();
    let (result, expected) = corpus::read_case(&mut &data[..]).unwrap();
    assert!(corpus::marker_equal(&m, &result));
    assert!(matches!(expected, corpus::Expected::Nil));
    m.backup_file_name = Some("音楽/😀".into());
    let json = m.json_bytes().unwrap().unwrap();
    let native = Marker::from_encoded_bytes(&encode_marker_base64(&json).unwrap()).unwrap();
    data.clear();
    corpus::write_case(&mut data, &m, Some(&json), Some(Ok(&native))).unwrap();
    let (result, expected) = corpus::read_case(&mut &data[..]).unwrap();
    assert!(corpus::marker_equal(&m, &result));
    let corpus::Expected::Bytes(actual, Ok(decoded)) = expected else { panic!("missing oracle bytes"); };
    assert_eq!(actual, json);
    assert!(corpus::marker_equal(&native, &decoded));
}

// The caller names a new run; no existing evidence is overwritten.
fn receipt_dir() -> Option<std::path::PathBuf> {
    let run = std::env::var("PN_FOUNDATION_RUN").ok()?;
    assert!(!run.is_empty() && run.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')));
    Some(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("local-artifacts/portable-parity/foundation-01").join(run))
}

fn focused_receipt(name: &str, count: u64, seed: Option<&str>, duration: f64, mismatches: &[serde_json::Value]) {
    use std::io::Write;
    let Some(dir) = receipt_dir() else { return };
    let reference: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("receipt.json")).unwrap()).unwrap();
    let executable = std::env::current_exe().unwrap();
    let hash = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256", executable.to_str().unwrap()]).output().unwrap();
    assert!(hash.status.success());
    let hash = String::from_utf8(hash.stdout).unwrap().trim().to_owned();
    assert_eq!(reference["test_binary_sha256"].as_str(), Some(hash.as_str()), "focused test binary differs from marker receipt");
    let summary = serde_json::json!({
        "test": name, "cases": count, "seed": seed, "duration_seconds": duration,
        "mismatch_count": mismatches.len(), "mismatches": mismatches,
        "test_binary_sha256": hash,
        "reference_foundation": reference["reference_foundation"],
        "reference_corefoundation": reference["reference_corefoundation"],
        "macos": reference["macos"], "rustc": reference["rustc"],
    });
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join(format!("{name}.json"))).unwrap();
    serde_json::to_writer_pretty(&mut file, &summary).unwrap();
    writeln!(file).unwrap();
}

#[test]
fn parser_examples() {
    use platinum_engine::metadata::encode_marker_base64;
    let cases = [
        r#"{}"#, r#"{"inputPitch":-0}"#, r#"{"inputPitch":-0.0}"#,
        r#"{"inputPitch":9223372036854775807,"clipRepairCount":9223372036854775807}"#,
        r#"{"inputPitch":18446744073709551615,"clipRepairCount":18446744073709551615}"#,
        r#"{"inputPitch":1e999,"clipRepairCount":1e999}"#,
        r#"{"inputPitch":1e-999,"clipRepairCount":-1.9}"#,
        r#"{"inputPitch":"3.14foo","clipRepairCount":"999999999999999999999999999","isProcessed":"YES"}"#,
        r#"{"inputPitch":"  +0x1.8p+1","clipRepairCount":"  -42xyz","isProcessed":"-2"}"#,
        r#"{"osType":"\ud800"}"#, r#"{"osType":"\udc00"}"#,
        r#"{"osType":"\ud800\u0061"}"#, r#"{"osType":"a","osType":"b"}"#,
        r#"{"isProcessed":"true","appliedClipRepair":"false","appliedWarmth":"y","appliedCompression":"-1"}"#,
        r#"{"isProcessed":"yes","appliedClipRepair":"2","appliedWarmth":"0.1","appliedCompression":"-0"}"#,
        r#"{"clipRepairCount":2147483648.0,"warmthPreset":4294967295.0,"volumeAnalysisVersion":-2147483649.0}"#,
        r#"{"clipRepairCount":1e30,"warmthPreset":-1e30,"volumeAnalysisVersion":1.5}"#,
        r#"{"clipRepairCount":"2147483648","warmthPreset":"-2147483649","volumeAnalysisVersion":"1.5"}"#,
        r#"{"inputPitch":"NaN","outputPitch":"Inf","speedAdjustment":"-Infinity"}"#,
        r#"{"inputPitch":"1e","outputPitch":"1e+3foo","speedAdjustment":"12,34"}"#,
        r#"{"isProcessed":"   +1","appliedClipRepair":"  y","appliedWarmth":"yes","appliedCompression":"1e3"}"#,
        r#"{"isProcessed":"2true","appliedClipRepair":"  +0.2","appliedWarmth":"nan","appliedCompression":"\t t"}"#,
        r#"{"inputPitch":18446744073709551616,"clipRepairCount":18446744073709551616}"#,
        r#"{"inputPitch":null}"#, r#"{"inputPitch":true,"clipRepairCount":false}"#,
    ];
    for json in cases {
        let b = encode_marker_base64(json.as_bytes()).unwrap();
        let result = Marker::from_encoded_bytes(&b);
        let portable = platinum_engine::portable::foundation::marker_from_json(json.as_bytes());
        let sig = |value: &Result<Marker, platinum_engine::metadata::MarkerError>| {
            match value {
                Ok(m) => format!("{m:?} {:x?}", [m.target_volume.to_bits(), m.input_volume.to_bits(), m.output_volume.to_bits(), m.input_pitch.to_bits(), m.output_pitch.to_bits(), m.speed_adjustment.to_bits()]),
                Err(e) => format!("{e:?}"),
            }
        };
        assert_eq!(sig(&result), sig(&portable), "{json}");
    }
}

#[test]
fn portable_number_and_text_examples() {
    use platinum_engine::portable::foundation::marker_json_bytes;
    let mut m = marker();
    for x in [0.0, -0.0, 0.1, 1.2, 1.2345678901234567, 10.0, 1e-3, 1e-4,
        1e-5, 1e-6, 1e-7, 1e15, 1e16, 1e17, 1e23, f64::MIN_POSITIVE,
        f64::from_bits(1), f64::MAX, -f64::MAX, 437.76852416992188] {
        m.input_pitch = x;
        assert_eq!(marker_json_bytes(&m).unwrap(), m.json_bytes().unwrap(), "f64 bits {:016x}", x.to_bits());
    }
    m.backup_file_name = Some((0u8..=127).map(char::from).chain(['\u{0085}', '\u{2028}', '\u{2029}', '日', '😀']).collect::<String>().into());
    assert_eq!(marker_json_bytes(&m).unwrap(), m.json_bytes().unwrap());
}


#[test]
fn parser_encodings_match_foundation() {
    use platinum_engine::metadata::encode_marker_base64;
    use platinum_engine::portable::foundation::marker_from_json;
    let text = r#"{"osType":"日😀","inputPitch":0.00012803454592358321}"#;
    let mut cases = vec![text.as_bytes().to_vec(), [b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat()];
    for (big, bom) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut utf16 = Vec::new();
        let mut utf32 = Vec::new();
        if bom {
            utf16.extend_from_slice(if big { &[0xfe, 0xff] } else { &[0xff, 0xfe] });
            utf32.extend_from_slice(if big { &[0, 0, 0xfe, 0xff] } else { &[0xff, 0xfe, 0, 0] });
        }
        for x in text.encode_utf16() {
            utf16.extend_from_slice(&if big { x.to_be_bytes() } else { x.to_le_bytes() });
        }
        for x in text.chars() {
            let n = x as u32;
            utf32.extend_from_slice(&if big { n.to_be_bytes() } else { n.to_le_bytes() });
        }
        cases.push(utf16);
        cases.push(utf32);
    }
    for bytes in cases {
        let a = Marker::from_encoded_bytes(&encode_marker_base64(&bytes).unwrap());
        let b = marker_from_json(&bytes);
        match (a, b) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a, b, "encoding prefix {:x?}", &bytes[..4]);
                assert_eq!(a.input_pitch.to_bits(), b.input_pitch.to_bits());
            }
            (Err(a), Err(b)) => assert_eq!(format!("{a:?}"), format!("{b:?}")),
            (a, b) => panic!("encoding prefix {:x?}: {a:?} != {b:?}", &bytes[..4]),
        }
    }
}


#[test]
fn parser_unicode_digit_selectors() {
    use platinum_engine::metadata::encode_marker_base64;
    use platinum_engine::portable::foundation::marker_from_json;
    let starts = [
        0x30, 0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66,
        0x0be6, 0x0c66, 0x0ce6, 0x0d66, 0x0de6, 0x0e50, 0x0ed0, 0x0f20, 0x1040,
        0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90, 0x1b50, 0x1bb0,
        0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    ];
    for start in starts {
        for digit in 0..10 {
            let c = char::from_u32(start + digit).unwrap();
            for text in [format!("{c}"), format!("\t -{c}suffix"), format!("{c}.{c}e{c}x"),
                format!("0{c}"), format!("1{c}")] {
                let encoded = serde_json::to_string(&text).unwrap();
                let json = format!("{{\"inputPitch\":{encoded},\"clipRepairCount\":{encoded},\"isProcessed\":{encoded}}}");
                let a = Marker::from_encoded_bytes(&encode_marker_base64(json.as_bytes()).unwrap()).unwrap();
                let b = marker_from_json(json.as_bytes()).unwrap();
                assert_eq!(a, b, "NSString {text:?}");
                assert_eq!(a.input_pitch.to_bits(), b.input_pitch.to_bits(), "NSString {text:?}");
            }
        }
    }
    for text in ["𝟙𝟚", "²³", "Ⅻ", "一二", "- 4", "\n1", "0٢", "1٢3",
        "\u{200b}1", "-\u{200b}1", "\u{200b}yes", "\u{200b}0.5"] {
        let encoded = serde_json::to_string(text).unwrap();
        let json = format!("{{\"inputPitch\":{encoded},\"clipRepairCount\":{encoded},\"isProcessed\":{encoded}}}");
        let a = Marker::from_encoded_bytes(&encode_marker_base64(json.as_bytes()).unwrap()).unwrap();
        let b = marker_from_json(json.as_bytes()).unwrap();
        assert_eq!(a, b, "NSString {text:?}");
        assert_eq!(a.input_pitch.to_bits(), b.input_pitch.to_bits(), "NSString {text:?}");
    }
}

#[test]
fn uuid_format_matches_cf() {
    use std::ffi::c_void;
    #[repr(C)]
    struct CFUUIDBytes { bytes: [u8; 16] }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFUUIDCreate(allocator: *const c_void) -> *const c_void;
        fn CFUUIDGetUUIDBytes(uuid: *const c_void) -> CFUUIDBytes;
        fn CFRelease(value: *const c_void);
    }
    for _ in 0..1000 {
        let native = unsafe { CFUUIDCreate(std::ptr::null()) };
        assert!(!native.is_null());
        let bits = unsafe { CFUUIDGetUUIDBytes(native) }.bytes;
        unsafe { CFRelease(native) };
        assert_eq!(bits[6] & 0xf0, 0x40);
        assert_eq!(bits[8] & 0xc0, 0x80);
        let s = platinum_engine::portable::foundation::uuid().unwrap();
        let b = s.as_bytes();
        assert_eq!(b.len(), 36);
        for i in [8, 13, 18, 23] { assert_eq!(b[i], b'-'); }
        assert_eq!(b[14], b'4');
        assert!(b"89AB".contains(&b[19]));
        for (i, c) in b.iter().enumerate() {
            if ![8, 13, 18, 23].contains(&i) { assert!(c.is_ascii_digit() || (b'A'..=b'F').contains(c)); }
        }
    }
}

#[test]
fn parser_invalid_sample() {
    use platinum_engine::metadata::encode_marker_base64;
    use platinum_engine::portable::foundation::marker_from_json;
    let cases = [
        "", "  ", "null", "[]", "{}", "{ }", r#"{"x":1,}"#, r#"{,"x":1}"#,
        r#"{,}"#, r#"{"x":[,]}"#, r#"{"x":[1,]}"#, r#"{"x":[1,,]}"#, r#"{"x":{"a":1,}}"#,
        r#"{"x":{"a":[2,],}}"#,
        r#"{"x":01}"#, r#"{"x":1.}"#, r#"{"x":.1}"#, r#"{"x":-0}"#,
        r#"{"x":1e}"#, r#"{"x":1e+}"#, r#"{"x":1e-999}"#, r#"{"x":1e999}"#,
        r#"{"x":-1e999}"#, r#"{"osType":-1e999}"#, r#"{"inputPitch":-1e999}"#,
        r#"{"x":-252690638712564377130e-129}"#,
        r#"{"x":1.23456789012345678e145}"#, r#"{"x":1.23456789012345678e-112}"#,
        r#"{"inputPitch":1.23456789012345678e145}"#, r#"{"inputPitch":1.23456789012345678e-112}"#,
        r#"{"x":{},"y":[]}"#, r#"{"inputPitch":[1,2]}"#, r#"{"inputPitch":{"bad":2}}"#,
        r#"{"osType":null,"osType":1}"#, r#"{"osType":1,"osType":null}"#,
        r#"{"osType":"\ud800"}"#, r#"{"osType":"\udc00"}"#,
        r#"{"inputPitch":0,"osType":0}"#, r#"{"osType":0,"inputPitch":null}"#,
        r#"{"x":[1,{"q":1.23456789012345678e145}]}"#,
        r#"{"x":"\u0001"}"#, "{\"x\":\"a\nb\"}", r#"{"x":"\u0000"}"#,
        r#"{"x":true} garbage"#, r#"{"x":true}{}"#, r#"{"x":NaN}"#, r#"{"x":Infinity}"#,
    ];
    let mut failures = 0;
    for json in cases {
        let a = if json.len() < 2 { Marker::from_encoded_bytes(b"") }
                else { Marker::from_encoded_bytes(&encode_marker_base64(json.as_bytes()).unwrap()) };
        let b = marker_from_json(json.as_bytes());
        let s = |x: &Result<Marker, platinum_engine::metadata::MarkerError>| match x {
            Ok(m) => format!("{m:?}"),
            Err(e) => format!("{e:?}"),
        };
        if s(&a) != s(&b) {
            failures += 1;
            println!("mismatch {json:?}: {:?} != {:?}", s(&a), s(&b));
        }
    }
    assert_eq!(failures, 0);
}

#[test]
#[ignore]
fn exhaustive_unicode_serialization() {
    use platinum_engine::metadata::MarkerText;
    use platinum_engine::portable::foundation::marker_json_bytes;
    let started = std::time::Instant::now();
    let mut mismatches = Vec::new();
    let text: String = (0..=0x10ffff).filter_map(char::from_u32).collect();
    let mut m = marker();
    m.backup_file_name = Some(MarkerText::Utf8(text));
    if m.json_bytes().unwrap() != marker_json_bytes(&m).unwrap() {
        mismatches.push(serde_json::json!({"kind": "all_unicode_scalars"}));
    }
    for code in 0xd800..=0xdfff {
        m.backup_file_name = Some(MarkerText::Utf16(vec![code]));
        if m.json_bytes().unwrap() != marker_json_bytes(&m).unwrap() {
            mismatches.push(serde_json::json!({"unpaired_utf16": format!("{code:04x}")}));
        }
    }
    focused_receipt("unicode-1112064-scalars-plus-2048-isolated-surrogates", 1_114_112, None, started.elapsed().as_secs_f64(), &mismatches);
    assert!(mismatches.is_empty(), "{mismatches:?}");
}

#[test]
fn parser_depth_sample() {
    use platinum_engine::metadata::encode_marker_base64;
    use platinum_engine::portable::foundation::marker_from_json;
    for depth in [1usize, 64, 126, 127, 128, 129, 255, 256, 511, 512, 513, 1024] {
        let json = format!("{{\"x\":{}0{}}}", "[".repeat(depth), "]".repeat(depth));
        let a = Marker::from_encoded_bytes(&encode_marker_base64(json.as_bytes()).unwrap());
        let b = marker_from_json(json.as_bytes());
        assert_eq!(a.is_ok(), b.is_ok(), "depth {depth}: {a:?} != {b:?}");
    }
}

// This is intentionally an ignored, create-only receipt-producing parity test.
// It compares whole serialized bytes, then compares every parsed field and raw
// f64 bits against Foundation; failures are all retained, not sampled.
#[test]
#[ignore]
fn foundation_parity_receipt() {
    use platinum_engine::metadata::{MarkerError, MarkerText, encode_marker_base64};
    use platinum_engine::portable::foundation::{marker_from_json, marker_json_bytes};
    use std::ffi::{CStr, c_char, c_void};
    use std::fs::{self, OpenOptions};
    use std::io::{BufWriter, Write};
    use std::process::Command;
    use std::time::Instant;

    const COUNT: u64 = 1_200_000;
    const SEED: u64 = 0x621d_4fb9_aa72_1385;
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
    }
    fn text(r: u64) -> Option<MarkerText> {
        match r % 8 {
            0 => None,
            1 => Some(MarkerText::Utf8(String::new())),
            2 => Some(MarkerText::Utf8(format!("/sample/{r:016x}/x.wav"))),
            3 => Some(MarkerText::Utf8("a\0/\"\\\n\r\t\u{0001}".into())),
            4 => Some(MarkerText::Utf8("日本語é😀\u{2028}\u{2029}\u{ffff}".into())),
            5 => Some(MarkerText::Utf16("music 😀\0/日本".encode_utf16().collect())),
            6 => Some(MarkerText::Utf16(vec![])),
            _ => Some(MarkerText::Utf8("LUFS".into())),
        }
    }
    fn equal(a: &Result<Marker, MarkerError>, b: &Result<Marker, MarkerError>) -> bool {
        match (a, b) {
            (Ok(a), Ok(b)) => a == b && [
                a.target_volume.to_bits(), a.input_volume.to_bits(), a.output_volume.to_bits(),
                a.input_pitch.to_bits(), a.output_pitch.to_bits(), a.speed_adjustment.to_bits(),
            ] == [
                b.target_volume.to_bits(), b.input_volume.to_bits(), b.output_volume.to_bits(),
                b.input_pitch.to_bits(), b.output_pitch.to_bits(), b.speed_adjustment.to_bits(),
            ],
            (Err(a), Err(b)) => format!("{a:?}") == format!("{b:?}"),
            _ => false,
        }
    }
    fn command(program: &str, args: &[&str]) -> String {
        let output = Command::new(program).args(args).output().expect("receipt command");
        assert!(output.status.success(), "{program}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("UTF-8 receipt command output").trim().into()
    }
    unsafe extern "C" {
        fn _dyld_image_count() -> u32;
        fn _dyld_get_image_name(index: u32) -> *const c_char;
        fn _dyld_get_image_header(index: u32) -> *const c_void;
    }
    fn image_uuid(fragment: &str) -> serde_json::Value {
        for index in 0..unsafe { _dyld_image_count() } {
            let name = unsafe { _dyld_get_image_name(index) };
            if name.is_null() { continue; }
            let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
            if !name.contains(fragment) { continue; }
            let header = unsafe { _dyld_get_image_header(index) }.cast::<u8>();
            assert!(!header.is_null());
            let ncmds = unsafe { std::ptr::read_unaligned(header.add(16).cast::<u32>()) };
            let mut command = unsafe { header.add(32) };
            for _ in 0..ncmds {
                let kind = unsafe { std::ptr::read_unaligned(command.cast::<u32>()) };
                let length = unsafe { std::ptr::read_unaligned(command.add(4).cast::<u32>()) };
                assert!(length >= 8);
                if kind == 0x1b {
                    assert!(length >= 24);
                    let uuid = unsafe { std::slice::from_raw_parts(command.add(8), 16) }
                        .iter().map(|b| format!("{b:02x}")).collect::<String>();
                    return serde_json::json!({"path": name, "mach_o_uuid": uuid});
                }
                command = unsafe { command.add(length as usize) };
            }
        }
        panic!("missing image UUID for {fragment}");
    }

    let dir = receipt_dir().expect("set PN_FOUNDATION_RUN to a new run name");
    fs::create_dir_all(dir.parent().unwrap()).unwrap();
    fs::create_dir(&dir).expect("receipt directory must not already exist");
    let mismatch_path = dir.join("mismatches.jsonl");
    let mut log = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(&mismatch_path).unwrap());
    let corpus_path = dir.join("macos-oracle.bin");
    let mut corpus_file = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(&corpus_path).unwrap());
    corpus_file.write_all(corpus::MAGIC).unwrap();
    corpus_file.write_all(&corpus::COUNT.to_le_bytes()).unwrap();
    let started = Instant::now();
    let executable = std::env::current_exe().unwrap();
    let binary_sha256 = command("/usr/bin/shasum", &["-a", "256", executable.to_str().unwrap()]);
    let source_paths = ["-a", "256", "src/portable/foundation.rs", "tests/portable_foundation.rs", "tools/portable/foundation/corpus.rs", "tools/portable/foundation/src/main.rs", "tools/portable/foundation/Cargo.toml", "tools/portable/foundation/Cargo.lock", "src/metadata.rs", "src/settings.rs", "src/lib.rs", "Cargo.lock"];
    let source_sha256 = command("/usr/bin/shasum", &source_paths);
    let mut rng = Rng(SEED);
    let mut marker = marker();
    let mut emitted = 0u64;
    let mut nil = 0u64;
    let mut mismatched_serialization = 0u64;
    let mut mismatched_parsing = 0u64;
    let mut nonfinite_fields = 0u64;
    let mut subnormal_fields = 0u64;
    let mut negative_zero_fields = 0u64;
    let mut positive_zero_fields = 0u64;
    let mut invalid_utf16_markers = 0u64;
    for i in 0..COUNT {
        marker.os_type = text(rng.next());
        marker.software_version = text(rng.next());
        marker.backup_file_name = text(rng.next());
        marker.volume_units = text(rng.next());
        if i % 997 == 0 {
            marker.backup_file_name = Some(MarkerText::Utf16(vec![if i % 2 == 0 { 0xd800 } else { 0xdc00 }]));
            invalid_utf16_markers += 1;
        }
        marker.target_volume = f64::from(f32::from_bits(rng.next() as u32));
        marker.input_volume = f64::from(f32::from_bits(rng.next() as u32));
        marker.output_volume = f64::from_bits(rng.next());
        marker.input_pitch = f64::from(f32::from_bits(rng.next() as u32));
        marker.output_pitch = f64::from_bits(rng.next());
        marker.speed_adjustment = f64::from_bits(rng.next());
        match i % 512 {
            0 => {
                marker.target_volume = 0.0;
                marker.input_volume = -0.0;
                marker.output_volume = f64::from_bits(1);
                marker.input_pitch = f64::from(f32::from_bits(1));
                marker.output_pitch = -f64::from_bits(0x000f_ffff_ffff_ffff);
                marker.speed_adjustment = f64::MIN_POSITIVE;
            }
            1 => {
                marker.target_volume = f64::MAX;
                marker.input_volume = -f64::MAX;
                marker.output_volume = f64::from(f32::MAX);
                marker.input_pitch = 0.1;
                marker.output_pitch = 1e-5;
                marker.speed_adjustment = 1e23;
            }
            2 => {
                marker.target_volume = f64::INFINITY;
                marker.input_volume = f64::NEG_INFINITY;
                marker.output_volume = f64::NAN;
            }
            3 => {
                marker.target_volume = 1.0;
                marker.input_volume = 1e16;
                marker.output_volume = 1e17;
                marker.input_pitch = -12.0;
                marker.output_pitch = 9007199254740992.0;
                marker.speed_adjustment = -0.0;
            }
            _ => {}
        }
        marker.volume_analysis_version = rng.next() as i32;
        marker.clip_repair_count = rng.next() as i32;
        marker.warmth_preset = rng.next() as i32;
        let flags = rng.next();
        marker.is_processed = flags & 1 != 0;
        marker.applied_volume_adjustment = flags & 2 != 0;
        marker.applied_clip_repair = flags & 4 != 0;
        marker.applied_pitch_correction = flags & 8 != 0;
        marker.applied_warmth = flags & 16 != 0;
        marker.applied_expansion = flags & 32 != 0;
        marker.applied_compression = flags & 64 != 0;
        for x in [marker.target_volume, marker.input_volume, marker.output_volume, marker.input_pitch, marker.output_pitch, marker.speed_adjustment] {
            nonfinite_fields += u64::from(!x.is_finite());
            subnormal_fields += u64::from(x.is_subnormal());
            negative_zero_fields += u64::from(x.to_bits() == (-0.0f64).to_bits());
            positive_zero_fields += u64::from(x.to_bits() == 0);
        }
        let reference = marker.json_bytes().unwrap();
        let portable = marker_json_bytes(&marker).unwrap();
        if reference != portable {
            mismatched_serialization += 1;
            writeln!(log, "{}", serde_json::json!({"kind":"serialize", "index":i, "marker":format!("{marker:?}"), "reference":reference.as_deref().map(String::from_utf8_lossy), "portable":portable.as_deref().map(String::from_utf8_lossy)})).unwrap();
        }
        if let Some(json) = reference {
            emitted += 1;
            let native = Marker::from_encoded_bytes(&encode_marker_base64(&json).unwrap());
            if i < corpus::COUNT {
                corpus::write_case(&mut corpus_file, &marker, Some(&json),
                    Some(native.as_ref().map_err(|e| format!("{e:?}")))).unwrap();
            }
            let portable = marker_from_json(&json);
            if !equal(&native, &portable) {
                mismatched_parsing += 1;
                let bits = |m: &Result<Marker, MarkerError>| m.as_ref().ok().map(|m| [m.target_volume.to_bits(), m.input_volume.to_bits(), m.output_volume.to_bits(), m.input_pitch.to_bits(), m.output_pitch.to_bits(), m.speed_adjustment.to_bits()].map(|v| format!("{v:016x}")));
                writeln!(log, "{}", serde_json::json!({"kind":"parse", "index":i, "json":String::from_utf8_lossy(&json), "native":format!("{native:?}"), "portable":format!("{portable:?}"), "native_bits":bits(&native), "portable_bits":bits(&portable)})).unwrap();
            }
        } else {
            nil += 1;
            if i < corpus::COUNT { corpus::write_case(&mut corpus_file, &marker, None, None).unwrap(); }
        }
    }
    log.flush().unwrap();
    corpus_file.flush().unwrap();
    let corpus_sha256 = command("/usr/bin/shasum", &["-a", "256", corpus_path.to_str().unwrap()]);
    let source_sha256_after = command("/usr/bin/shasum", &source_paths);
    let sources_unchanged = source_sha256 == source_sha256_after;
    let summary = serde_json::json!({
        "comparison": "whole marker JSON bytes and Foundation marker parse raw f64 bits",
        "seed": format!("{SEED:016x}"),
        "prng": "xorshift64* 12,25,27; multiplier 2545f4914f6cdd1d",
        "generated_payloads": COUNT, "reference_emitted": emitted, "reference_nil": nil,
        "oracle_corpus_cases": corpus::COUNT, "oracle_corpus_sha256": corpus_sha256,
        "serialization_mismatches": mismatched_serialization, "parser_mismatches": mismatched_parsing,
        "field_classes": {"nonfinite": nonfinite_fields, "binary64_subnormal": subnormal_fields, "negative_zero": negative_zero_fields, "positive_zero": positive_zero_fields, "injected_unpaired_utf16_markers": invalid_utf16_markers},
        "float_domains": "three arbitrary binary64 bit patterns and three arbitrary binary32 bit patterns widened to binary64 per marker; periodic extrema, zeros, subnormals, nonfinites, integral values, and exponent thresholds",
        "strings": "nil, empty, randomized paths, controls and escapes, multilingual UTF-8, U+2028/U+2029/noncharacter, valid UTF-16 and injected isolated high/low surrogates",
        "integers": "three arbitrary signed i32 bit patterns per marker", "booleans": "seven arbitrary flags",
        "duration_seconds": started.elapsed().as_secs_f64(),
        "macos": command("/usr/bin/sw_vers", &[]),
        "kernel": command("/usr/bin/uname", &["-a"]),
        "rustc": command("rustc", &["-Vv"]),
        "reference_foundation": image_uuid("/Foundation.framework/"),
        "reference_corefoundation": image_uuid("/CoreFoundation.framework/"),
        "test_binary_sha256": binary_sha256, "source_sha256_at_start": source_sha256,
        "source_sha256_at_end": source_sha256_after, "sources_unchanged": sources_unchanged,
        "mismatches": mismatch_path.to_string_lossy(),
        "limitations": "macOS 27.2 arm64 runtime only; randomized not exhaustive floating domains; no end-to-end audio/container parity",
    });
    let mut file = OpenOptions::new().write(true).create_new(true).open(dir.join("receipt.json")).unwrap();
    serde_json::to_writer_pretty(&mut file, &summary).unwrap();
    writeln!(file).unwrap();
    println!("{summary}");
    assert!(sources_unchanged, "source changed during the parity run");
    assert!(emitted >= 1_000_000, "not enough serialized payloads");
    assert_eq!(mismatched_serialization, 0);
    assert_eq!(mismatched_parsing, 0);
}

#[test]
#[ignore]
fn parser_numeric_string_sample() {
    use platinum_engine::metadata::encode_marker_base64;
    use platinum_engine::portable::foundation::marker_from_json;
    let count: u64 = std::env::var("PN_FOUNDATION_COUNT").ok().and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let started = std::time::Instant::now();
    let mut state = 0x91c0_cfe2_78ba_1813u64;
    let mut next = || {
        state ^= state >> 12; state ^= state << 25; state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut mismatches = Vec::new();
    for i in 0..count {
        let mut text = String::new();
        if next() & 1 != 0 { text.push('-'); }
        for _ in 0..(1 + next() % 30) { text.push((b'0' + (next() % 10) as u8) as char); }
        if next() & 1 != 0 {
            text.push('.');
            for _ in 0..(next() % 30) { text.push((b'0' + (next() % 10) as u8) as char); }
        }
        if next() & 1 != 0 {
            text.push('e');
            if next() & 1 != 0 { text.push('-'); }
            for _ in 0..(1 + next() % 4) { text.push((b'0' + (next() % 10) as u8) as char); }
        }
        let encoded = serde_json::to_string(&text).unwrap();
        let json = format!("{{\"inputPitch\":{encoded},\"clipRepairCount\":{encoded},\"isProcessed\":{encoded}}}");
        let a = Marker::from_encoded_bytes(&encode_marker_base64(json.as_bytes()).unwrap()).unwrap();
        let b = marker_from_json(json.as_bytes()).unwrap();
        if a != b || a.input_pitch.to_bits() != b.input_pitch.to_bits() {
            mismatches.push(serde_json::json!({"index":i, "text":text, "reference":format!("{a:?}"), "portable":format!("{b:?}"), "reference_bits":format!("{:016x}", a.input_pitch.to_bits()), "portable_bits":format!("{:016x}", b.input_pitch.to_bits())}));
        }
    }
    println!("count={count} mismatches={}", mismatches.len());
    focused_receipt("numeric-strings", count, Some("91c0cfe278ba1813"), started.elapsed().as_secs_f64(), &mismatches);
    assert!(mismatches.is_empty());
}

#[test]
#[ignore]
fn parser_numeric_json_sample() {
    use platinum_engine::metadata::encode_marker_base64;
    use platinum_engine::portable::foundation::marker_from_json;
    let count: u64 = std::env::var("PN_FOUNDATION_COUNT").ok().and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let started = std::time::Instant::now();
    let mut state = 0x1f53_abc0_928e_7455u64;
    let mut next = || {
        state ^= state >> 12; state ^= state << 25; state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut mismatches = Vec::new();
    for i in 0..count {
        let mut text = String::new();
        if next() & 1 != 0 { text.push('-'); }
        text.push((b'1' + (next() % 9) as u8) as char);
        for _ in 0..(next() % 40) { text.push((b'0' + (next() % 10) as u8) as char); }
        if next() & 1 != 0 {
            text.push('.');
            for _ in 0..(1 + next() % 30) { text.push((b'0' + (next() % 10) as u8) as char); }
        }
        if next() & 1 != 0 {
            text.push('e');
            if next() & 1 != 0 { text.push('-'); }
            for _ in 0..(1 + next() % 3) { text.push((b'0' + (next() % 10) as u8) as char); }
        }
        let json = format!("{{\"inputPitch\":{text},\"clipRepairCount\":{text},\"isProcessed\":{text}}}");
        let a = Marker::from_encoded_bytes(&encode_marker_base64(json.as_bytes()).unwrap());
        let b = marker_from_json(json.as_bytes());
        let mismatch = match (&a, &b) {
            (Ok(a), Ok(b)) => a != b || a.input_pitch.to_bits() != b.input_pitch.to_bits(),
            (Err(a), Err(b)) => format!("{a:?}") != format!("{b:?}"),
            _ => true,
        };
        if mismatch {
            mismatches.push(serde_json::json!({"index":i, "text":text, "reference":format!("{a:?}"), "portable":format!("{b:?}")}));
        }
    }
    println!("count={count} mismatches={}", mismatches.len());
    focused_receipt("numeric-json", count, Some("1f53abc0928e7455"), started.elapsed().as_secs_f64(), &mismatches);
    assert!(mismatches.is_empty());
}
