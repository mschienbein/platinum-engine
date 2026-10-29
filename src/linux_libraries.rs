//! Exact, architecture-specific identities of the tested Linux native libraries.

use crate::native_library::{PinnedError, PinnedLibrary};
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryKind { Flac, Lame, TagBridge }
impl LibraryKind {
    fn label(self) -> &'static str {
        match self { Self::Flac => "libFLAC", Self::Lame => "LAME", Self::TagBridge => "TagLib bridge" }
    }
    pub fn expected_sha256(self) -> &'static str {
        #[cfg(target_arch = "aarch64")]
        { match self {
            Self::Flac => "41b59a452fcd7245a0609a924b8cfcd7873e7b740945d49cd25c34321ec01e72",
            Self::Lame => "c780062f806c3b04ab00b777efd9f842f00800724f7183815374fdabd89c9d87",
            Self::TagBridge => "8708af52d83fc7fdb6a487d72abc90ff4bd9f9b94e313436cc6fb68cc9db2aa0",
        } }
        #[cfg(target_arch = "x86_64")]
        { match self {
            Self::Flac => "9420c96f562aec7734f09469ed07def4444471cf90d2c3fd70bc257e847b8755",
            Self::Lame => "cf59ae7fc6445a2b1d24465f423865e971e966b780c3f82d268cd397dbef675b",
            Self::TagBridge => "ea86881cb6615d7cf8614dfb285e8670d978de621957b15611b8bd6d5b0e1ad5",
        } }
    }
}

#[derive(Debug)]
pub enum LibraryPolicyError {
    Identity(String),
    Unapproved { library: LibraryKind, actual: [u8; 32], expected: &'static str },
}
impl fmt::Display for LibraryPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity(error) => f.write_str(error),
            Self::Unapproved { library, actual, expected } => {
                write!(f, "unapproved Linux {} SHA-256 ", library.label())?;
                for byte in actual { write!(f, "{byte:02x}")?; }
                write!(f, "; expected {expected}")
            }
        }
    }
}
impl std::error::Error for LibraryPolicyError {}
impl From<PinnedError> for LibraryPolicyError {
    fn from(error: PinnedError) -> Self { Self::Identity(error.to_string()) }
}

pub(crate) fn open_approved(path: &Path, kind: LibraryKind) -> Result<PinnedLibrary, LibraryPolicyError> {
    let pinned = PinnedLibrary::open(path)?;
    let actual = pinned.digest();
    let expected = kind.expected_sha256();
    if !pinned.digest_matches(expected) {
        return Err(LibraryPolicyError::Unapproved { library: kind, actual, expected });
    }
    Ok(pinned)
}
