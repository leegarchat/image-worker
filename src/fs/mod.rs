//! Read-side filesystem backends over the shared [`crate::core::Image`].
//! ext4 logic mirrors image-inspect (incl. streaming writes, prefix sampling,
//! SELinux xattr); EROFS mirrors image-inspect erofs.rs (plain/inline/
//! compressed-compact + LZ4/DEFLATE/ZSTD).

pub mod erofs;
pub mod ext4;

use crate::core::{Error, Image};

/// Directory listing entry (read side).
pub struct DirEntry {
    pub name: String,
    /// Raw directory-entry file type byte (2 == directory convention).
    /// Traversal and markers follow this byte for image-inspect
    /// compatibility.
    pub file_type: u8,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub context: Option<String>,
}

/// Probe order: EROFS first (magic at 1024 differs), then ext4.
pub fn probe_fs(image: &mut Image) -> Result<FsKind, Error> {
    if erofs::probe(image)? {
        return Ok(FsKind::Erofs);
    }
    if ext4::probe(image)? {
        return Ok(FsKind::Ext4);
    }
    Err(Error::invalid("unsupported filesystem format"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Erofs,
    Ext4,
}
