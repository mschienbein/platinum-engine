#![cfg(target_os = "linux")]

use platinum_engine::flac::{FlacError, FlacLibrary};
use platinum_engine::linux_libraries::{LibraryKind, LibraryPolicyError};
use platinum_engine::mp3::{self, LameLibrary, Mp3Error};
use platinum_engine::tags::{TagBridge, TagError};
use std::ffi::{CStr, c_void};
use std::path::PathBuf;

#[link(name = "m")]
unsafe extern "C" { #[link_name = "cos"] fn system_cos(value: f64) -> f64; }

fn system_library() -> PathBuf {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    assert_ne!(unsafe { libc::dladdr(system_cos as *const () as *const c_void, &mut info) }, 0);
    assert!(!info.dli_fname.is_null());
    let bytes = unsafe { CStr::from_ptr(info.dli_fname) }.to_bytes();
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes)).canonicalize().unwrap()
}

fn check_refusal(error: LibraryPolicyError, kind: LibraryKind, self_pin: &str) {
    match error {
        LibraryPolicyError::Unapproved { library, actual, expected } => {
            assert_eq!(library, kind);
            assert_eq!(expected, kind.expected_sha256());
            let actual: String = actual.iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(actual, self_pin);
            assert_ne!(actual, expected);
        }
        other => panic!("expected unapproved-library policy error, got {other}")
    }
}

#[test]
fn wrong_but_self_pinned_shared_library_is_rejected_before_loading() {
    let path = system_library();
    let self_pin = mp3::file_sha256(&path).unwrap();
    match LameLibrary::load(&path, &self_pin).err().unwrap() {
        Mp3Error::LibraryPolicy(error) => check_refusal(error, LibraryKind::Lame, &self_pin),
        error => panic!("unexpected LAME error: {error}"),
    }
    match FlacLibrary::load(&path, &self_pin).err().unwrap() {
        FlacError::LibraryPolicy(error) => check_refusal(error, LibraryKind::Flac, &self_pin),
        error => panic!("unexpected FLAC error: {error}"),
    }
    match unsafe { TagBridge::load(&path) }.err().unwrap() {
        TagError::LibraryPolicy(error) => check_refusal(error, LibraryKind::TagBridge, &self_pin),
        error => panic!("unexpected bridge error: {error}"),
    }
}

#[test]
#[ignore = "requires PN_TAG_BRIDGE, PN_FLAC_LIB and PN_LAME_LIB from the approved image"]
fn approved_libraries_load_through_distinct_pinned_descriptors() {
    let bridge = PathBuf::from(std::env::var_os("PN_TAG_BRIDGE").unwrap());
    let flac = PathBuf::from(std::env::var_os("PN_FLAC_LIB").unwrap());
    let lame = PathBuf::from(std::env::var_os("PN_LAME_LIB").unwrap());
    let pin = mp3::file_sha256(&lame).unwrap();
    let flac_pin = mp3::file_sha256(&flac).unwrap();
    assert!(matches!(FlacLibrary::load(&flac, &"0".repeat(64)), Err(FlacError::DigestMismatch { .. })));
    assert!(matches!(LameLibrary::load(&lame, &"0".repeat(64)), Err(Mp3Error::DigestMismatch { .. })));
    let first_bridge = unsafe { TagBridge::load(&bridge) }.unwrap();
    let actual: String = first_bridge.bridge_sha256().iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(actual, mp3::file_sha256(&bridge).unwrap());
    let _second_bridge = unsafe { TagBridge::load(&bridge) }.unwrap();
    let _first_flac = FlacLibrary::load(&flac, &flac_pin).unwrap();
    let _second_flac = FlacLibrary::load(&flac, &flac_pin).unwrap();
    let _first_lame = LameLibrary::load(&lame, &pin).unwrap();
    let _second_lame = LameLibrary::load(&lame, &pin).unwrap();
}
