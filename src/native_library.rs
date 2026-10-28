//! Bind native library loading to the exact regular file that was hashed.
//!
//! Descriptors and handles are retained in a process-wide registry, including
//! failed post-load verifications: loaders may keep a DSO resident after
//! `dlclose`, so its descriptor path must never be reused for a different file.
//! Quarantined entries are never returned to a caller.

use libloading::Library;
use std::collections::HashMap;
use std::fmt;
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Debug)]
pub(crate) enum PinnedError {
    Io(io::Error),
    NotRegular,
    Changed,
    MappedIdentity,
    Quarantined,
    Load(String),
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    Digest,
}
impl fmt::Display for PinnedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "library identity I/O: {error}"),
            Self::NotRegular => f.write_str("library identity is not a regular file"),
            Self::Changed => f.write_str("library changed while verifying or loading it"),
            Self::MappedIdentity => f.write_str("loaded library image does not match the verified file descriptor"),
            Self::Quarantined => f.write_str("library was quarantined after a failed post-load verification"),
            Self::Load(error) => write!(f, "load pinned library: {error}"),
            #[cfg(all(target_os = "macos", not(feature = "portable")))]
            Self::Digest => f.write_str("library SHA-256 calculation failed"),
        }
    }
}
impl From<io::Error> for PinnedError {
    fn from(error: io::Error) -> Self { Self::Io(error) }
}

fn same_stamp(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino() && a.len() == b.len() && a.mode() == b.mode()
        && a.mtime() == b.mtime() && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime() && a.ctime_nsec() == b.ctime_nsec()
}

#[cfg(all(target_os = "macos", not(feature = "portable")))]
#[repr(C)]
struct Sha256Context { count: [u32; 2], hash: [u32; 8], buffer: [u32; 16] }
#[cfg(all(target_os = "macos", not(feature = "portable")))]
unsafe extern "C" {
    fn CC_SHA256_Init(ctx: *mut Sha256Context) -> i32;
    fn CC_SHA256_Update(ctx: *mut Sha256Context, data: *const std::ffi::c_void, len: u32) -> i32;
    fn CC_SHA256_Final(md: *mut u8, ctx: *mut Sha256Context) -> i32;
}

fn hash_pinned(file: &File) -> Result<[u8; 32], PinnedError> {
    let before = file.metadata()?;
    if !before.is_file() { return Err(PinnedError::NotRegular); }
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    let mut digest = {
        let mut context = Sha256Context { count: [0; 2], hash: [0; 8], buffer: [0; 16] };
        if unsafe { CC_SHA256_Init(&mut context) } != 1 { return Err(PinnedError::Digest); }
        context
    };
    #[cfg(any(target_os = "linux", feature = "portable"))]
    let mut digest = crate::portable::digest::Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    let mut offset = 0u64;
    loop {
        let count = file.read_at(&mut bytes, offset)?;
        if count == 0 { break; }
        #[cfg(all(target_os = "macos", not(feature = "portable")))]
        if unsafe { CC_SHA256_Update(&mut digest, bytes.as_ptr().cast(), count as u32) } != 1 { return Err(PinnedError::Digest); }
        #[cfg(any(target_os = "linux", feature = "portable"))]
        digest.update(&bytes[..count]);
        offset = offset.checked_add(count as u64).ok_or(PinnedError::Changed)?;
    }
    let after = file.metadata()?;
    if offset != before.len() || !same_stamp(&before, &after) { return Err(PinnedError::Changed); }
    #[cfg(all(target_os = "macos", not(feature = "portable")))]
    {
        let mut result = [0u8; 32];
        if unsafe { CC_SHA256_Final(result.as_mut_ptr(), &mut digest) } != 1 { return Err(PinnedError::Digest); }
        Ok(result)
    }
    #[cfg(any(target_os = "linux", feature = "portable"))]
    { Ok(digest.finalize()) }
}

type LibraryKey = (u64, u64, [u8; 32]);
struct RegisteredLibrary { _file: Arc<File>, library: Arc<Library>, verified: bool }
static REGISTRY: LazyLock<Mutex<HashMap<LibraryKey, RegisteredLibrary>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) struct PinnedLibrary { file: Arc<File>, digest: [u8; 32], key: LibraryKey }
impl PinnedLibrary {
    pub(crate) fn open(path: &Path) -> Result<Self, PinnedError> {
        let file = File::open(path)?;
        let digest = hash_pinned(&file)?;
        let metadata = file.metadata()?;
        let key = (metadata.dev(), metadata.ino(), digest);
        Ok(Self { file: Arc::new(file), digest, key })
    }
    pub(crate) fn digest(&self) -> [u8; 32] { self.digest }
    pub(crate) fn digest_matches(&self, expected: &str) -> bool {
        let hex = b"0123456789abcdef";
        expected.len() == 64 && self.digest.iter().enumerate().all(|(index, &byte)| {
            expected.as_bytes()[index * 2].to_ascii_lowercase() == hex[usize::from(byte >> 4)]
                && expected.as_bytes()[index * 2 + 1].to_ascii_lowercase() == hex[usize::from(byte & 15)]
        })
    }
    pub(crate) fn digest_hex(&self) -> String {
        use std::fmt::Write as _;
        let mut output = String::with_capacity(64);
        for byte in self.digest { write!(&mut output, "{byte:02x}").expect("write to string"); }
        output
    }
    /// The caller must trust the selected library's ABI and initializers.
    pub(crate) unsafe fn load(&self) -> Result<Arc<Library>, PinnedError> {
        unsafe { self.load_using(|path| Library::new(path).map_err(|error| PinnedError::Load(error.to_string())), hash_pinned) }
    }

    unsafe fn load_using(
        &self,
        open: impl FnOnce(&str) -> Result<Library, PinnedError>,
        verify: impl FnOnce(&File) -> Result<[u8; 32], PinnedError>,
    ) -> Result<Arc<Library>, PinnedError> {
        let mut registry = REGISTRY.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = registry.get_mut(&self.key) {
            if !entry.verified { return Err(PinnedError::Quarantined); }
            let checked = hash_pinned(&self.file).and_then(|digest| {
                if digest == self.digest { Ok(()) } else { Err(PinnedError::Changed) }
            });
            if checked.is_err() { entry.verified = false; }
            checked?;
            return Ok(Arc::clone(&entry.library));
        }
        #[cfg(target_os = "linux")]
        let path = format!("/proc/self/fd/{}", self.file.as_raw_fd());
        #[cfg(target_os = "macos")]
        let path = format!("/dev/fd/{}", self.file.as_raw_fd());
        let library = Arc::new(open(&path)?);
        let checked = verify(&self.file).and_then(|digest| {
            if digest == self.digest { Ok(()) } else { Err(PinnedError::Changed) }
        });
        registry.insert(self.key, RegisteredLibrary {
            _file: Arc::clone(&self.file), library: Arc::clone(&library), verified: checked.is_ok(),
        });
        checked?;
        Ok(library)
    }

    /// Verify an exported symbol belongs to the file backing this descriptor.
    pub(crate) fn verify_symbol(&self, address: *const std::ffi::c_void) -> Result<(), PinnedError> {
        let result = self.verify_symbol_inner(address);
        let mut registry = REGISTRY.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = registry.get_mut(&self.key) {
            if !entry.verified { return Err(PinnedError::Quarantined); }
            if result.is_err() { entry.verified = false; }
        }
        result
    }

    fn verify_symbol_inner(&self, address: *const std::ffi::c_void) -> Result<(), PinnedError> {
        let expected = self.file.metadata()?;
        #[cfg(target_os = "linux")]
        {
            use std::io::BufRead;
            let mut maps = io::BufReader::new(File::open("/proc/self/maps")?);
            let mut line = String::new();
            loop {
                line.clear();
                if maps.read_line(&mut line)? == 0 { break; }
                let mut fields = line.split_whitespace();
                let (Some(range), Some(_permissions), Some(_offset), Some(device), Some(inode)) =
                    (fields.next(), fields.next(), fields.next(), fields.next(), fields.next())
                    else { continue; };
                let Some((start, end)) = range.split_once('-') else { continue; };
                let (Ok(start), Ok(end)) = (usize::from_str_radix(start, 16), usize::from_str_radix(end, 16)) else { continue; };
                if !(start..end).contains(&(address as usize)) { continue; }
                let Some((major, minor)) = device.split_once(':') else { return Err(PinnedError::MappedIdentity); };
                let (Ok(major), Ok(minor), Ok(inode)) =
                    (u64::from_str_radix(major, 16), u64::from_str_radix(minor, 16), inode.parse::<u64>())
                    else { return Err(PinnedError::MappedIdentity); };
                if inode == expected.ino() && major == u64::from(libc::major(expected.dev()))
                    && minor == u64::from(libc::minor(expected.dev())) { return Ok(()); }
                return Err(PinnedError::MappedIdentity);
            }
            Err(PinnedError::MappedIdentity)
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            if unsafe { libc::dladdr(address, &mut info) } == 0 || info.dli_fname.is_null() {
                return Err(PinnedError::MappedIdentity);
            }
            let name = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_bytes();
            let path = Path::new(std::ffi::OsStr::from_bytes(name));
            let actual = std::fs::metadata(path)?;
            if (actual.dev(), actual.ino()) == (expected.dev(), expected.ino()) { Ok(()) }
            else { Err(PinnedError::MappedIdentity) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("platinum-library-pin-{}-{}-{}",
                std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn descriptor_digest_follows_the_original_inode_after_path_replacement() {
        let fixture = Fixture::new();
        let path = fixture.0.join("library");
        let held = fixture.0.join("held");
        fs::write(&path, b"original").unwrap();
        let pinned = PinnedLibrary::open(&path).unwrap();
        fs::rename(&path, &held).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert_eq!(hash_pinned(&pinned.file).unwrap(), pinned.digest());
        assert_ne!(PinnedLibrary::open(&path).unwrap().digest(), pinned.digest());
        fs::write(&held, b"modified").unwrap();
        assert_ne!(hash_pinned(&pinned.file).unwrap(), pinned.digest());
    }

    #[test]
    fn failed_post_load_checks_quarantine_the_descriptor_and_handle() {
        let fixture = Fixture::new();
        for fail_with_io in [false, true] {
            let path = fixture.0.join(format!("library-{fail_with_io}"));
            fs::write(&path, b"pinned library fixture").unwrap();
            let pinned = PinnedLibrary::open(&path).unwrap();
            let fd = pinned.file.as_raw_fd();
            let key = pinned.key;
            let result = unsafe {
                pinned.load_using(
                    |_| Ok(libloading::os::unix::Library::this().into()),
                    |_| if fail_with_io {
                        Err(PinnedError::Io(io::Error::new(io::ErrorKind::Other, "injected verification failure")))
                    } else {
                        let mut changed = pinned.digest;
                        changed[0] ^= 1;
                        Ok(changed)
                    },
                )
            };
            assert!(matches!(result, Err(PinnedError::Changed) | Err(PinnedError::Io(_))));
            drop(pinned);
            assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
            let again = PinnedLibrary::open(&path).unwrap();
            assert!(matches!(unsafe { again.load() }, Err(PinnedError::Quarantined)));
            let registry = REGISTRY.lock().unwrap();
            let entry = registry.get(&key).unwrap();
            assert!(!entry.verified);
            assert_eq!(entry._file.as_raw_fd(), fd);
        }
    }

    #[test]
    fn failed_mapped_symbol_check_quarantines_a_loaded_handle() {
        let fixture = Fixture::new();
        let path = fixture.0.join("library");
        fs::write(&path, b"pinned library fixture").unwrap();
        let pinned = PinnedLibrary::open(&path).unwrap();
        let fd = pinned.file.as_raw_fd();
        let handle = unsafe {
            pinned.load_using(|_| Ok(libloading::os::unix::Library::this().into()), |_| Ok(pinned.digest))
        }.unwrap();
        assert!(matches!(pinned.verify_symbol(std::ptr::null()), Err(PinnedError::MappedIdentity)));
        drop(handle);
        drop(pinned);
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        let again = PinnedLibrary::open(&path).unwrap();
        assert!(matches!(unsafe { again.load() }, Err(PinnedError::Quarantined)));
    }

    #[test]
    #[ignore = "requires PN_PINNED_LAME pointing to a trusted public LAME library"]
    fn real_loaded_image_is_quarantined_after_injected_post_load_failure() {
        let fixture = Fixture::new();
        let path = fixture.0.join("library");
        fs::copy(std::env::var_os("PN_PINNED_LAME").unwrap(), &path).unwrap();
        let pinned = PinnedLibrary::open(&path).unwrap();
        let fd = pinned.file.as_raw_fd();
        let key = pinned.key;
        let result = unsafe {
            pinned.load_using(
                |alias| Library::new(alias).map_err(|error| PinnedError::Load(error.to_string())),
                |_| Err(PinnedError::Changed),
            )
        };
        assert!(matches!(result, Err(PinnedError::Changed)));
        {
            let registry = REGISTRY.lock().unwrap();
            let entry = registry.get(&key).unwrap();
            assert!(!entry.verified);
            let symbol: libloading::Symbol<'_, unsafe extern "C" fn() -> *const std::ffi::c_char> =
                unsafe { entry.library.get(b"get_lame_version\0") }.unwrap();
            pinned.verify_symbol_inner(*symbol as *const () as *const std::ffi::c_void).unwrap();
        }
        drop(pinned);
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        let again = PinnedLibrary::open(&path).unwrap();
        assert!(matches!(unsafe { again.load() }, Err(PinnedError::Quarantined)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn configured_codec_pins_are_checked_before_dynamic_loading() {
        let fixture = Fixture::new();
        let path = fixture.0.join("not-a-library");
        fs::write(&path, b"not a dynamic library").unwrap();
        let wrong = "0".repeat(64);
        assert!(matches!(crate::flac::FlacLibrary::load(&path, &wrong),
            Err(crate::flac::FlacError::DigestMismatch { .. })));
        assert!(matches!(crate::mp3::LameLibrary::load(&path, &wrong),
            Err(crate::mp3::Mp3Error::DigestMismatch { .. })));
    }

    #[test]
    #[ignore = "requires PN_PINNED_LAME pointing to a trusted public LAME 3.100 library"]
    fn descriptor_load_uses_the_verified_inode_after_path_replacement() {
        let fixture = Fixture::new();
        let path = fixture.0.join("library");
        let held = fixture.0.join("held");
        fs::copy(std::env::var_os("PN_PINNED_LAME").unwrap(), &path).unwrap();
        let pinned = PinnedLibrary::open(&path).unwrap();
        fs::rename(&path, &held).unwrap();
        fs::write(&path, b"not a dynamic library").unwrap();
        let library = unsafe { pinned.load() }.unwrap();
        let version: libloading::Symbol<'_, unsafe extern "C" fn() -> *const std::ffi::c_char> =
            unsafe { library.get(b"get_lame_version\0") }.unwrap();
        let version = unsafe { std::ffi::CStr::from_ptr(version()) };
        assert_eq!(version.to_bytes(), b"3.100");
    }

    #[test]
    #[ignore = "requires PN_PINNED_FLAC and PN_PINNED_BRIDGE naming trusted native libraries"]
    fn registered_flac_then_bridge_cannot_reuse_the_resident_descriptor_name() {
        let flac_path = std::env::var_os("PN_PINNED_FLAC").unwrap();
        let bridge_path = std::env::var_os("PN_PINNED_BRIDGE").unwrap();
        let flac = PinnedLibrary::open(Path::new(&flac_path)).unwrap();
        let first = unsafe { flac.load() }.unwrap();
        let constructor: unsafe extern "C" fn() -> *mut std::ffi::c_void =
            unsafe { *first.get(b"FLAC__stream_encoder_new\0").unwrap() };
        flac.verify_symbol(constructor as *const () as *const std::ffi::c_void).unwrap();
        let same = PinnedLibrary::open(Path::new(&flac_path)).unwrap();
        let same_handle = unsafe { same.load() }.unwrap();
        assert!(Arc::ptr_eq(&first, &same_handle), "same inode and digest must reuse the handle");
        let resident_fd = REGISTRY.lock().unwrap().get(&flac.key).unwrap()._file.as_raw_fd();
        let transient_fd = same.file.as_raw_fd();
        drop(first);
        drop(same_handle);
        drop(same);
        drop(flac);
        assert_ne!(unsafe { libc::fcntl(resident_fd, libc::F_GETFD) }, -1, "resident descriptor was closed");

        let bridge = PinnedLibrary::open(Path::new(&bridge_path)).unwrap();
        assert_eq!(bridge.file.as_raw_fd(), transient_fd, "the transient descriptor number was not reused");
        assert_ne!(bridge.file.as_raw_fd(), resident_fd, "a live resident descriptor was reused");
        let handle = unsafe { bridge.load() }.unwrap();
        let abi: unsafe extern "C" fn() -> u32 = unsafe { *handle.get(b"pn_tag_bridge_abi\0").unwrap() };
        bridge.verify_symbol(abi as *const () as *const std::ffi::c_void).unwrap();
        let wrong = PinnedLibrary::open(Path::new(&flac_path)).unwrap();
        assert!(matches!(wrong.verify_symbol(abi as *const () as *const std::ffi::c_void), Err(PinnedError::MappedIdentity)));
        assert_eq!(unsafe { abi() }, 5);
    }
}
