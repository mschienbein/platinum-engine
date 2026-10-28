//! Foundation behavior used by the marker and settings code.
//!
//! The marker serializer models the macOS 27.2 arm64 Foundation runtime used
//! by the reference, including its enumeration of the fixed 20-key dictionary.
//! Foundation does not promise this order on other OS releases. No Objective-C,
//! CoreFoundation, or platform math routines are called by this module.
//! Engine marker and settings call sites run outside the DSP's
//! `with_flush_to_zero` scopes; this module does not change the FP mode.

use crate::metadata::{Marker, MarkerError, MarkerText};
use std::fmt::Write as _;

// NSDictionary enumeration for the original, fixed constructor order, observed
// on the reference runtime. This is not a general NSDictionary implementation.
const ORDER: [usize; 20] = [
    15, 16, 9, 2, 14, 12, 3, 11, 10, 13, 17, 19, 6, 7, 18, 1, 4, 5, 8, 0,
];
const KEYS: [&str; 20] = [
    "osType", "softwareVersion", "backupFileName", "isProcessed",
    "appliedVolumeAdjustment", "appliedClipRepair", "appliedPitchCorrection",
    "appliedWarmth", "appliedExpansion", "appliedCompression", "volumeAnalysisVersion",
    "volumeUnits", "targetVolume", "inputVolume", "outputVolume", "clipRepairCount",
    "warmthPreset", "inputPitch", "outputPitch", "speedAdjustment",
];

enum Field<'a> {
    Text(Option<&'a MarkerText>),
    Bool(bool),
    Int(i32),
    Double(f64),
}

// A finite f64 with 17 significant digits fits in 25 scientific-format bytes.
struct ScientificBuffer { bytes: [u8; 32], len: usize }
impl std::fmt::Write for ScientificBuffer {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let end = self.len.checked_add(value.len()).filter(|&n| n <= self.bytes.len()).ok_or(std::fmt::Error)?;
        self.bytes[self.len..end].copy_from_slice(value.as_bytes());
        self.len = end;
        Ok(())
    }
}

// NSNumber numberWithDouble: is formatted as 17-significant-digit %g on the
// pinned runtime. Rust's decimal formatter rounds the exact binary value; the
// remaining work applies %g's exponent selection, zero trimming and padding.
fn append_double(out: &mut String, value: f64) {
    let mut buf = ScientificBuffer { bytes: [0; 32], len: 0 };
    write!(&mut buf, "{value:.16e}").expect("finite f64 scientific format fits");
    let scientific = std::str::from_utf8(&buf.bytes[..buf.len]).expect("numeric format is ASCII");
    let (mantissa, exponent) = scientific.split_once('e').expect("scientific exponent");
    let exponent: i32 = exponent.parse().expect("scientific exponent integer");
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let mut digits = [0u8; 17];
    let mut count = 0;
    for b in mantissa.bytes().filter(|&b| b != b'.') {
        digits[count] = b;
        count += 1;
    }
    while count > 1 && digits[count - 1] == b'0' { count -= 1; }
    if negative { out.push('-'); }
    if !(-4..17).contains(&exponent) {
        out.push(digits[0] as char);
        if count > 1 {
            out.push('.');
            for &b in &digits[1..count] { out.push(b as char); }
        }
        let _ = write!(out, "e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs());
    } else {
        let point = exponent + 1;
        if point <= 0 {
            out.push_str("0.");
            for _ in point..0 { out.push('0'); }
            for &b in &digits[..count] { out.push(b as char); }
        } else {
            let point = point as usize;
            for i in 0..point {
                out.push(if i < count { digits[i] as char } else { '0' });
            }
            if count > point {
                out.push('.');
                for &b in &digits[point..count] { out.push(b as char); }
            }
        }
    }
}

fn append_character(out: &mut String, ch: char) {
    match ch {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '/' => out.push_str("\\/"),
        '\u{08}' => out.push_str("\\b"),
        '\u{0c}' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{00}'..='\u{1f}' => { let _ = write!(out, "\\u{:04x}", ch as u32); }
        _ => out.push(ch),
    }
}

fn append_text(out: &mut String, text: Option<&MarkerText>) -> bool {
    out.push('"');
    match text {
        None => {},
        Some(MarkerText::Utf8(text)) => {
            for ch in text.chars() { append_character(out, ch); }
        }
        Some(MarkerText::Utf16(units)) => {
            for ch in char::decode_utf16(units.iter().copied()) {
                match ch {
                    Ok(ch) => append_character(out, ch),
                    Err(_) => return false,
                }
            }
        }
    }
    out.push('"');
    true
}

/// Serialize the fixed 20-field marker as macOS 27.2 arm64 Foundation does.
/// All finite f64 and i32 values and UTF-8/UTF-16 strings are accepted;
/// nonfinite numbers and unpaired UTF-16 surrogates return `Ok(None)`.
/// This does not model dictionaries with arbitrary keys or another OS version.
pub fn marker_json_bytes(marker: &Marker) -> Result<Option<Vec<u8>>, MarkerError> {
    let fields = [
        Field::Text(marker.os_type.as_ref()), Field::Text(marker.software_version.as_ref()),
        Field::Text(marker.backup_file_name.as_ref()), Field::Bool(marker.is_processed),
        Field::Bool(marker.applied_volume_adjustment), Field::Bool(marker.applied_clip_repair),
        Field::Bool(marker.applied_pitch_correction), Field::Bool(marker.applied_warmth),
        Field::Bool(marker.applied_expansion), Field::Bool(marker.applied_compression),
        Field::Int(marker.volume_analysis_version), Field::Text(marker.volume_units.as_ref()),
        Field::Double(marker.target_volume), Field::Double(marker.input_volume),
        Field::Double(marker.output_volume), Field::Int(marker.clip_repair_count),
        Field::Int(marker.warmth_preset), Field::Double(marker.input_pitch),
        Field::Double(marker.output_pitch), Field::Double(marker.speed_adjustment),
    ];
    let mut out = String::with_capacity(512);
    out.push('{');
    for (position, &index) in ORDER.iter().enumerate() {
        if position != 0 { out.push(','); }
        out.push('"');
        out.push_str(KEYS[index]);
        out.push_str("\":");
        match &fields[index] {
            Field::Text(text) => if !append_text(&mut out, *text) { return Ok(None); },
            Field::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Field::Int(value) => { let _ = write!(&mut out, "{value}"); },
            Field::Double(value) => {
                if !value.is_finite() { return Ok(None); }
                append_double(&mut out, *value);
            }
        }
    }
    out.push('}');
    Ok(Some(out.into_bytes()))
}

// NSJSONSerialization detects UTF-8/16/32. Its LE UTF-32 BOM is treated as
// an LE UTF-16 BOM on the reference runtime, unlike its BE UTF-32 BOM.
fn json_utf8(bytes: &[u8]) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    let (data, unit, big) = if bytes.starts_with(&[0x00, 0x00, 0xfe, 0xff]) {
        (&bytes[4..], 4, true)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (&bytes[2..], 2, true)
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        (&bytes[2..], 2, false)
    } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        (&bytes[3..], 1, false)
    } else if bytes.len() >= 4 && bytes[..3] == [0, 0, 0] {
        (bytes, 4, true)
    } else if bytes.len() >= 4 && bytes[1..4] == [0, 0, 0] {
        (bytes, 4, false)
    } else if bytes.len() >= 4 && bytes[0] == 0 && bytes[2] == 0 {
        (bytes, 2, true)
    } else if bytes.len() >= 4 && bytes[1] == 0 && bytes[3] == 0 {
        (bytes, 2, false)
    } else {
        (bytes, 1, false)
    };
    match unit {
        1 => std::str::from_utf8(data).ok().map(Cow::Borrowed),
        2 => {
            let chunks = data.chunks_exact(2);
            if !chunks.remainder().is_empty() { return None; }
            let units = chunks.map(|c| {
                if big { u16::from_be_bytes([c[0], c[1]]) }
                else { u16::from_le_bytes([c[0], c[1]]) }
            });
            let mut result = String::with_capacity(data.len() / 2);
            for ch in char::decode_utf16(units) { result.push(ch.ok()?); }
            Some(Cow::Owned(result))
        }
        4 => {
            let mut chunks = data.chunks_exact(4);
            let mut result = String::with_capacity(data.len() / 4);
            for c in chunks.by_ref() {
                let bytes = [c[0], c[1], c[2], c[3]];
                let value = if big { u32::from_be_bytes(bytes) } else { u32::from_le_bytes(bytes) };
                result.push(char::from_u32(value)?);
            }
            if !chunks.remainder().is_empty() { return None; }
            Some(Cow::Owned(result))
        }
        _ => unreachable!(),
    }
}

fn whitespace(bytes: &[u8], cursor: &mut usize) {
    while bytes.get(*cursor).is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')) { *cursor += 1; }
}

// The reference JSON reader accepts a single trailing comma after a value.
// Normalize only commas outside strings, leaving all other syntax validation
// to the JSON parser. The input can be malformed at this point.
fn remove_trailing_commas(json: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let bytes = json.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    let mut previous = 0u8;
    let mut out: Option<String> = None;
    let mut copied = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped { escaped = false; }
            else if b == b'\\' { escaped = true; }
            else if b == b'"' { in_string = false; previous = b'"'; }
            continue;
        }
        if b == b'"' { in_string = true; continue; }
        if b == b',' && !matches!(previous, b'{' | b'[' | b',' | b':' | 0) {
            let mut j = i + 1;
            whitespace(bytes, &mut j);
            if matches!(bytes.get(j), Some(b'}' | b']')) {
                let result = out.get_or_insert_with(|| String::with_capacity(json.len()));
                result.push_str(&json[copied..i]);
                copied = i + 1;
                continue;
            }
        }
        if !matches!(b, b' ' | b'\t' | b'\r' | b'\n') { previous = b; }
    }
    if let Some(mut out) = out {
        out.push_str(&json[copied..]);
        Cow::Owned(out)
    } else {
        Cow::Borrowed(json)
    }
}

// Called only after validate_json has validated the entire JSON document.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    loop {
        match bytes[i] {
            b'\"' => return i + 1,
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
}

fn value_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut depth = 0;
    loop {
        match bytes[i] {
            b'\"' => i = string_end(bytes, i),
            b'{' | b'[' => { depth += 1; i += 1; }
            b'}' | b']' if depth > 0 => { depth -= 1; i += 1; }
            b',' | b'}' if depth == 0 => return i,
            _ => i += 1,
        }
    }
}

// NSString's numeric selectors skip the whitespace character set, not the
// newline character set. intValue additionally skips whitespace after a sign.
fn string_space(ch: char) -> bool {
    matches!(ch, '\t' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200b}' |
        '\u{202f}' | '\u{205f}' | '\u{3000}')
}

// The reference NSString scanner recognizes decimal digits in the BMP, but
// not supplementary-plane digits represented by UTF-16 surrogate pairs.
fn string_digit(ch: char) -> Option<u8> {
    let code = ch as u32;
    if (b'0' as u32..=b'9' as u32).contains(&code) { return Some((code - b'0' as u32) as u8); }
    const STARTS: [u32; 36] = [
        0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6,
        0x0c66, 0x0ce6, 0x0d66, 0x0de6, 0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x1090,
        0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90, 0x1b50, 0x1bb0, 0x1c40,
        0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    ];
    let i = STARTS.partition_point(|&start| start <= code);
    if i > 0 && code - STARTS[i - 1] < 10 { Some((code - STARTS[i - 1]) as u8) } else { None }
}

fn normalized_digits(text: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let Some((start, _)) = text.char_indices().find(|&(_, ch)| !ch.is_ascii() && string_digit(ch).is_some()) else {
        return Cow::Borrowed(text);
    };
    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..start]);
    for ch in text[start..].chars() {
        if let Some(digit) = string_digit(ch) { result.push((b'0' + digit) as char); }
        else { result.push(ch); }
    }
    Cow::Owned(result)
}

fn string_integer(text: &str) -> i32 {
    let text = text.trim_start_matches(string_space);
    let mut bytes = text.bytes();
    let first = bytes.next();
    let (negative, digits) = match first {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let digits = digits.trim_start_matches(string_space);
    let limit = if negative { 2_147_483_648u64 } else { 2_147_483_647u64 };
    let mut value = 0u64;
    for digit in digits.chars().map_while(string_digit) {
        value = value.saturating_mul(10).saturating_add(u64::from(digit)).min(limit);
    }
    if negative { (-(value as i64)) as i32 } else { value as i32 }
}

fn string_double(text: &str) -> f64 {
    let text = normalized_digits(text.trim_start_matches(string_space));
    let bytes = text.as_bytes();
    let mut i = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut digits = 0;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) { i += 1; digits += 1; }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) { i += 1; digits += 1; }
    }
    if digits == 0 { return 0.0; }
    let mut end = i;
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) { i += 1; }
        let exponent_start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) { i += 1; }
        if i != exponent_start { end = i; }
    }
    text[..end].parse().unwrap_or(0.0)
}

fn string_boolean(text: &str) -> bool {
    let text = text.trim_start_matches(string_space);
    if matches!(text.as_bytes().first(), Some(b'y' | b'Y' | b't' | b'T')) { return true; }
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    digits.bytes().take_while(u8::is_ascii_digit).any(|b| b != b'0')
}

// JSONSerialization chooses an NSDecimalNumber for a floating-point token with
// more than 17 written mantissa digits (the leading zero in 0.xxx is excluded),
// and for an integer outside the native 64-bit integer range.
fn uses_decimal(value: &serde_json::Number, raw: &str) -> bool {
    let mantissa = raw.trim_start_matches('-').split(['e', 'E']).next().unwrap();
    if raw.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
        let digits = mantissa.bytes().filter(u8::is_ascii_digit).count();
        digits - usize::from(mantissa.starts_with("0.")) > 17
    } else {
        value.as_i64().is_none() && value.as_u64().is_none()
    }
}

// The reference JSON parser rejects positive floating overflow but accepts
// negative infinity when it chose NSNumber rather than NSDecimalNumber.
fn negative_json_infinity(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.first() != Some(&b'-') { return false; }
    let mut i = 1;
    match bytes.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            i += 1;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) { i += 1; }
        }
        _ => return false,
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) { i += 1; }
        if start == i { return false; }
    }
    let mantissa = &raw[1..i];
    let digits = mantissa.bytes().filter(u8::is_ascii_digit).count()
        - usize::from(mantissa.starts_with("0."));
    if digits > 17 || !matches!(bytes.get(i), Some(b'e' | b'E')) { return false; }
    i += 1;
    if matches!(bytes.get(i), Some(b'+' | b'-')) { i += 1; }
    let start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) { i += 1; }
    start != i && i == bytes.len() && raw.parse::<f64>().is_ok_and(|v| v == f64::NEG_INFINITY)
}

// NSDecimal's 128-bit coefficient is compacted by removing trailing zeroes.
// The runtime converts successive base-65536 limbs, then scales one decade at a
// time, so this deliberately does not use a correctly-rounded decimal parser.
// Multiplication by 65536 is exact in this domain: fusing the subsequent add
// gives the same result, without requiring an FMA instruction or platform libm.
fn decimal_double(raw: &str) -> Option<f64> {
    let negative = raw.starts_with('-');
    let raw = raw.trim_start_matches('-');
    let (mantissa, exponent) = match raw.find(['e', 'E']) {
        Some(i) => {
            let exp = &raw[i + 1..];
            let parsed = exp.parse::<i64>().unwrap_or(if exp.starts_with('-') { i64::MIN } else { i64::MAX });
            (&raw[..i], parsed)
        }
        None => (raw, 0),
    };
    let fraction = mantissa.split_once('.').map_or(0, |(_, b)| b.len());
    let mut coefficient = 0u128;
    let mut omitted = 0i64;
    let mut overflow = false;
    for digit in mantissa.bytes().filter(u8::is_ascii_digit) {
        if !overflow {
            if let Some(next) = coefficient.checked_mul(10).and_then(|x| x.checked_add(u128::from(digit - b'0'))) {
                coefficient = next;
                continue;
            }
            overflow = true;
        }
        omitted = omitted.saturating_add(1);
    }
    let mut scale = exponent.saturating_sub(i64::try_from(fraction).unwrap_or(i64::MAX)).saturating_add(omitted);
    if !(-128..=127).contains(&scale) { return None; }
    if coefficient == 0 { return Some(0.0); }
    while coefficient % 10 == 0 {
        coefficient /= 10;
        scale = scale.saturating_add(1);
    }
    if !(-128..=127).contains(&scale) { return None; }
    let mut value = 0.0f64;
    let length = ((128 - coefficient.leading_zeros() as usize) + 15) / 16;
    for i in (0..length).rev() {
        let limb = ((coefficient >> (16 * i)) & 0xffff) as u16;
        value = value * 65536.0 + f64::from(limb);
    }
    if scale < 0 { for _ in scale..0 { value /= 10.0; } }
    else { for _ in 0..scale { value *= 10.0; } }
    Some(if negative { -value } else { value })
}

fn number_double(value: &serde_json::Number, raw: &str) -> Result<f64, MarkerError> {
    if uses_decimal(value, raw) {
        decimal_double(raw).ok_or(MarkerError::InvalidMarker("expected a JSON object"))
    } else if raw == "-0" {
        Ok(0.0)
    } else {
        Ok(value.as_f64().expect("validated JSON number"))
    }
}

fn number_integer(value: &serde_json::Number, raw: &str) -> Result<i32, MarkerError> {
    if uses_decimal(value, raw) { return Ok(number_double(value, raw)? as i32); }
    if let Some(n) = value.as_i64() { return Ok(n as i32); }
    if let Some(n) = value.as_u64() { return Ok(n as i32); }
    let n = value.as_f64().expect("validated JSON number");
    // A non-decimal NSNumber's intValue converts through a 64-bit value.
    Ok((n as i64) as i32)
}

// Foundation permits 512 nested containers, while serde_json's default limit is
// 128. Validate structure iteratively; serde still validates each scalar token.
// This also validates numbers inside ignored fields and duplicate keys.
fn validate_json(json: &str) -> bool {
    #[derive(Clone, Copy)]
    enum State { ObjectFirst, ObjectKey, ObjectColon, ObjectValue, ObjectAfter, ArrayFirst, ArrayValue, ArrayAfter }
    let bytes = json.as_bytes();
    let mut i = 0;
    whitespace(bytes, &mut i);
    if bytes.get(i) != Some(&b'{') { return false; }
    i += 1;
    let mut stack = [State::ObjectFirst; 512];
    let mut depth = 1usize;
    while depth != 0 {
        let state = stack[depth - 1];
        whitespace(bytes, &mut i);
        let Some(&b) = bytes.get(i) else { return false };
        match state {
            State::ObjectFirst | State::ObjectKey => {
                if matches!(state, State::ObjectFirst) && b == b'}' {
                    depth -= 1; i += 1; continue;
                }
                if b != b'"' { return false; }
                let start = i;
                i += 1;
                let mut escaped = false;
                let mut terminated = false;
                while let Some(&b) = bytes.get(i) {
                    i += 1;
                    if escaped { escaped = false; }
                    else if b == b'\\' { escaped = true; }
                    else if b == b'"' { terminated = true; break; }
                }
                if !terminated || serde_json::from_slice::<String>(&bytes[start..i]).is_err() { return false; }
                stack[depth - 1] = State::ObjectColon;
            }
            State::ObjectColon => {
                if b != b':' { return false; }
                i += 1;
                stack[depth - 1] = State::ObjectValue;
            }
            State::ObjectAfter => match b {
                b',' => { i += 1; stack[depth - 1] = State::ObjectKey; }
                b'}' => { i += 1; depth -= 1; }
                _ => return false,
            },
            State::ArrayAfter => match b {
                b',' => { i += 1; stack[depth - 1] = State::ArrayValue; }
                b']' => { i += 1; depth -= 1; }
                _ => return false,
            },
            State::ObjectValue | State::ArrayFirst | State::ArrayValue => {
                if matches!(state, State::ArrayFirst) && b == b']' {
                    depth -= 1; i += 1; continue;
                }
                stack[depth - 1] = if matches!(state, State::ObjectValue) { State::ObjectAfter } else { State::ArrayAfter };
                match b {
                    b'{' | b'[' => {
                        if depth == stack.len() { return false; }
                        stack[depth] = if b == b'{' { State::ObjectFirst } else { State::ArrayFirst };
                        depth += 1;
                        i += 1;
                    }
                    b'"' => {
                        let start = i;
                        i += 1;
                        let mut escaped = false;
                        let mut terminated = false;
                        while let Some(&b) = bytes.get(i) {
                            i += 1;
                            if escaped { escaped = false; }
                            else if b == b'\\' { escaped = true; }
                            else if b == b'"' { terminated = true; break; }
                        }
                        if !terminated || serde_json::from_slice::<String>(&bytes[start..i]).is_err() { return false; }
                    }
                    b'-' | b'0'..=b'9' => {
                        let start = i;
                        while i < bytes.len() && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') { i += 1; }
                        let raw = &json[start..i];
                        match serde_json::from_str::<serde_json::Number>(raw) {
                            Ok(number) => if uses_decimal(&number, raw) && decimal_double(raw).is_none() { return false; },
                            Err(_) => if !negative_json_infinity(raw) { return false; },
                        }
                    }
                    b't' if bytes[i..].starts_with(b"true") => i += 4,
                    b'f' if bytes[i..].starts_with(b"false") => i += 5,
                    b'n' if bytes[i..].starts_with(b"null") => i += 4,
                    _ => return false,
                }
            }
        }
    }
    whitespace(bytes, &mut i);
    i == bytes.len()
}


/// Parse marker JSON using the numeric and missing-field behavior observed on
/// macOS 27.2 arm64. Unknown fields are ignored and duplicate fields use their
/// first occurrence. Supports Foundation's UTF-8/16/32 detection and the scalar
/// NSString/NSNumber conversions used by marker fields. The differential tests
/// cover generated marker JSON and sampled foreign/invalid JSON, not every
/// possible malformed or adversarial document.
pub fn marker_from_json(bytes: &[u8]) -> Result<Marker, MarkerError> {
    use serde_json::Value;
    if bytes.is_empty() { return Err(MarkerError::InvalidMarker("empty decoded payload")); }
    let invalid = || MarkerError::InvalidMarker("expected a JSON object");
    let decoded = json_utf8(bytes).ok_or_else(invalid)?;
    let json = remove_trailing_commas(&decoded);
    if !validate_json(&json) { return Err(invalid()); }
    let source = json.as_bytes();
    let mut cursor = 0;
    whitespace(source, &mut cursor);
    cursor += 1; // validated top-level opening brace
    let mut values: [Option<(Value, &str, bool)>; 20] = std::array::from_fn(|_| None);
    loop {
        whitespace(source, &mut cursor);
        if source[cursor] == b'}' { break; }
        let key_end = string_end(source, cursor);
        let key: String = serde_json::from_slice(&source[cursor..key_end]).map_err(|_| invalid())?;
        cursor = key_end;
        whitespace(source, &mut cursor);
        cursor += 1; // validated colon
        whitespace(source, &mut cursor);
        let end = value_end(source, cursor);
        if let Some(index) = KEYS.iter().position(|name| *name == key) {
            if values[index].is_none() {
                let raw = json[cursor..end].trim_end_matches(char::is_whitespace);
                let mut negative_infinity = false;
                let value: Value = match raw.as_bytes()[0] {
                    b'[' => Value::Array(Vec::new()),
                    b'{' => Value::Object(serde_json::Map::new()),
                    _ => match serde_json::from_str(raw) {
                        Ok(value) => value,
                        Err(_) if negative_json_infinity(raw) => {
                            negative_infinity = true;
                            Value::Null
                        }
                        Err(_) => return Err(invalid()),
                    },
                };
                values[index] = Some((value, raw, negative_infinity));
            }
        }
        cursor = end;
        if source[cursor] == b',' { cursor += 1; } else { break; }
    }
    type Entry<'a> = Option<(Value, &'a str, bool)>;
    let [v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15, v16, v17, v18, v19] = values;
    let text = |entry: Entry<'_>, key: &'static str| -> Result<Option<MarkerText>, MarkerError> {
        match entry {
            None | Some((Value::Null, _, false)) => Ok(None),
            Some((Value::String(s), _, false)) if s.is_empty() => Ok(None),
            Some((Value::String(s), _, false)) => Ok(Some(MarkerText::Utf8(s))),
            _ => Err(MarkerError::InvalidField(key)),
        }
    };
    let boolean = |entry: Entry<'_>, key: &'static str| -> Result<bool, MarkerError> {
        match entry {
            None => Ok(false),
            Some((_, _, true)) => Ok(true),
            Some((Value::Bool(b), _, false)) => Ok(b),
            Some((Value::Number(n), raw, false)) => Ok(number_double(&n, raw)? != 0.0),
            Some((Value::String(s), _, false)) => Ok(string_boolean(&s)),
            _ => Err(MarkerError::InvalidField(key)),
        }
    };
    let integer = |entry: Entry<'_>, key: &'static str| -> Result<i32, MarkerError> {
        match entry {
            None => Ok(0),
            Some((_, _, true)) => Ok(0),
            Some((Value::Bool(b), _, false)) => Ok(i32::from(b)),
            Some((Value::Number(n), raw, false)) => number_integer(&n, raw),
            Some((Value::String(s), _, false)) => Ok(string_integer(&s)),
            _ => Err(MarkerError::InvalidField(key)),
        }
    };
    let double = |entry: Entry<'_>, key: &'static str| -> Result<f64, MarkerError> {
        match entry {
            None => Ok(0.0),
            Some((_, _, true)) => Ok(f64::NEG_INFINITY),
            Some((Value::Bool(b), _, false)) => Ok(if b { 1.0 } else { 0.0 }),
            Some((Value::Number(n), raw, false)) => number_double(&n, raw),
            Some((Value::String(s), _, false)) => Ok(string_double(&s)),
            _ => Err(MarkerError::InvalidField(key)),
        }
    };
    Ok(Marker {
        os_type: text(v0, KEYS[0])?, software_version: text(v1, KEYS[1])?, backup_file_name: text(v2, KEYS[2])?,
        is_processed: boolean(v3, KEYS[3])?, applied_volume_adjustment: boolean(v4, KEYS[4])?,
        applied_clip_repair: boolean(v5, KEYS[5])?, applied_pitch_correction: boolean(v6, KEYS[6])?,
        applied_warmth: boolean(v7, KEYS[7])?, applied_expansion: boolean(v8, KEYS[8])?, applied_compression: boolean(v9, KEYS[9])?,
        volume_analysis_version: integer(v10, KEYS[10])?, volume_units: text(v11, KEYS[11])?,
        target_volume: double(v12, KEYS[12])?, input_volume: double(v13, KEYS[13])?, output_volume: double(v14, KEYS[14])?,
        clip_repair_count: integer(v15, KEYS[15])?, warmth_preset: integer(v16, KEYS[16])?,
        input_pitch: double(v17, KEYS[17])?, output_pitch: double(v18, KEYS[18])?, speed_adjustment: double(v19, KEYS[19])?,
    })
}

/// Generate a fresh random UUID in the uppercase representation used by
/// settings. Its version and variant match CFUUIDCreate on the reference host;
/// the random value itself is inherently not repeatable across runs.
pub fn uuid() -> Result<String, crate::settings::SettingsError> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let mut entropy = std::fs::File::open("/dev/urandom")
        .map_err(|_| crate::settings::SettingsError::UuidUnavailable)?;
    entropy.read_exact(&mut bytes).map_err(|_| crate::settings::SettingsError::UuidUnavailable)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut result = String::with_capacity(36);
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for (i, b) in bytes.into_iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) { result.push('-'); }
        result.push(HEX[(b >> 4) as usize] as char);
        result.push(HEX[(b & 0x0f) as usize] as char);
    }
    Ok(result)
}
