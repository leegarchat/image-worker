//! EROFS extended attributes (xattr).
//!
//! On-disk layout, reverse-engineered from mkfs.erofs vendor images and
//! validated exhaustively (exact area tiling + shared-entry resolution,
//! zero failures over all fixtures, raw and sparse):
//!
//! - `i_xattr_icount == 0` → no xattr area, nothing follows the inode.
//! - Otherwise the inline area is `8 + icount*4` bytes right after the
//!   inode body (32 compact / 64 extended):
//!   `[w0: u32][shared_count: u32][reserved: u32]`
//!   then `shared_count` × u32 shared entry IDs,
//!   then inline entries, each
//!   `{u8 name_len, u8 name_index, u16 value_size} + name + value`,
//!   4-byte padded, tiling exactly to the area end.
//! - `w0` is an opaque marker (`0x00000000` on some images,
//!   `0xFFFFF7FF`/`0xDFFFEFFF` on others); all bootable images are
//!   accepted, the writer emits `0`.
//! - Shared entry ID `k` lives at `xattr_blkaddr*block_size + k*4` and
//!   parses with the same entry layout (`selinux` = name index 6).
//!
//! Memory discipline: only the (small, capped) xattr area and single
//! entries are ever buffered; file payloads are never touched here.

use super::Superblock;
use super::inode::Inode;
use crate::core::{Error, Image, buf};

/// xattr name index for `security.*` (matches `EROFS_XATTR_INDEX_SECURITY`).
pub const INDEX_SECURITY: u8 = 6;
/// Hard cap for a single xattr value (generous; real values are tiny).
const MAX_VALUE: usize = 1024 * 1024;
/// Hard cap for an inline xattr area (generous; real areas are < 1 KiB).
const MAX_AREA: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Xattr {
    pub index: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl Xattr {
    pub fn is_selinux(&self) -> bool {
        self.index == INDEX_SECURITY && self.name == b"selinux"
    }

    pub fn selinux_value(&self) -> String {
        String::from_utf8_lossy(&self.value)
            .trim_matches('\0')
            .to_owned()
    }
}

/// Inline area size for a given `i_xattr_icount` (0 when count is 0).
pub fn area_size(icount: u64) -> Result<u64, Error> {
    if icount == 0 {
        return Ok(0);
    }
    let size = 8u64
        .checked_add(
            icount
                .checked_mul(4)
                .ok_or_else(|| Error::invalid("EROFS xattr size overflow"))?,
        )
        .ok_or_else(|| Error::invalid("EROFS xattr size overflow"))?;
    if size > MAX_AREA {
        return Err(Error::invalid("EROFS xattr area is too large"));
    }
    Ok(size)
}

/// Parse one entry at `data[offset..]`; returns the entry and the offset
/// just past its 4-byte padding. `end` bounds the readable region.
fn parse_entry(data: &[u8], offset: usize, end: usize) -> Result<(Xattr, usize), Error> {
    let header = buf::bytes(data, offset, 4).map_err(|_| Error::invalid("truncated EROFS xattr entry"))?;
    let name_len = header[0] as usize;
    let name_index = header[1];
    let value_size = u16::from_le_bytes([header[2], header[3]]) as usize;
    if value_size > MAX_VALUE {
        return Err(Error::invalid("EROFS xattr value is too large"));
    }
    let name_off = offset.checked_add(4).ok_or_else(|| Error::invalid("EROFS xattr offset overflow"))?;
    let value_off = name_off.checked_add(name_len).ok_or_else(|| Error::invalid("EROFS xattr offset overflow"))?;
    let value_end = value_off.checked_add(value_size).ok_or_else(|| Error::invalid("EROFS xattr offset overflow"))?;
    if value_end > end {
        return Err(Error::invalid("truncated EROFS xattr entry"));
    }
    let name = buf::bytes(data, name_off, name_len)?.to_vec();
    let value = buf::bytes(data, value_off, value_size)?.to_vec();
    let next = (value_end + 3) & !3;
    if next > end {
        return Err(Error::invalid("truncated EROFS xattr entry"));
    }
    Ok((Xattr { index: name_index, name, value }, next))
}

/// Read a shared entry by ID. The entry must be structurally valid.
fn read_shared(fs: &Superblock, image: &mut Image, id: u32) -> Result<Xattr, Error> {
    let base = fs
        .xattr_blkaddr
        .checked_mul(fs.block_size)
        .ok_or_else(|| Error::invalid("EROFS xattr block overflow"))?;
    let entry_off = base
        .checked_add(id as u64 * 4)
        .ok_or_else(|| Error::invalid("EROFS xattr id overflow"))?;
    let mut header = [0u8; 4];
    image.read_at(entry_off, &mut header)?;
    let name_len = header[0] as usize;
    if name_len == 0 || header[1] == 0 {
        return Err(Error::invalid("invalid EROFS shared xattr entry"));
    }
    let value_size = u16::from_le_bytes([header[2], header[3]]) as usize;
    if value_size > MAX_VALUE {
        return Err(Error::invalid("EROFS xattr value is too large"));
    }
    let total = 4usize
        .checked_add(name_len)
        .and_then(|v| v.checked_add(value_size))
        .ok_or_else(|| Error::invalid("EROFS xattr size overflow"))?;
    let total_padded = (total + 3) & !3;
    let mut raw = vec![0u8; total_padded];
    image.read_at(entry_off, &mut raw)?;
    let (entry, consumed) = parse_entry(&raw, 0, raw.len())?;
    if consumed != raw.len() {
        return Err(Error::invalid("invalid EROFS shared xattr entry"));
    }
    Ok(entry)
}

/// Read all xattrs of an inode: shared entries first (kernel order),
/// then inline entries. The inline section must tile exactly.
pub fn read_all(fs: &Superblock, image: &mut Image, inode: &Inode) -> Result<Vec<Xattr>, Error> {
    read_at(fs, image, inode.nid, inode.inode_size, inode.xattr_size)
}

/// First `security.selinux` value of an inode, if any.
pub fn selinux(fs: &Superblock, image: &mut Image, inode: &Inode) -> Result<Option<String>, Error> {
    for attr in read_all(fs, image, inode)? {
        if attr.is_selinux() {
            return Ok(Some(attr.selinux_value()));
        }
    }
    Ok(None)
}

/// Core reader over explicit geometry (offset = meta + nid*32).
fn read_at(
    fs: &Superblock,
    image: &mut Image,
    nid: u64,
    inode_size: u64,
    xattr_size: u64,
) -> Result<Vec<Xattr>, Error> {
    let mut out = Vec::new();
    if xattr_size == 0 {
        return Ok(out);
    }
    let inode_off = (fs.meta_blkaddr << fs.block_bits())
        .checked_add(nid << 5)
        .ok_or_else(|| Error::invalid("EROFS inode offset overflow"))?;
    let area_off = inode_off
        .checked_add(inode_size)
        .ok_or_else(|| Error::invalid("EROFS xattr offset overflow"))?;
    let area_len = usize::try_from(xattr_size)
        .map_err(|_| Error::invalid("EROFS xattr area is too large"))?;
    if area_len > MAX_AREA as usize {
        return Err(Error::invalid("EROFS xattr area is too large"));
    }
    let mut area = vec![0u8; area_len];
    image.read_at(area_off, &mut area)?;
    if area.len() < 12 {
        return Err(Error::invalid("truncated EROFS xattr area"));
    }
    // [w0: u32 opaque][shared_count: u32][reserved: u32].
    let shared_count = buf::u32le(&area, 4)? as usize;
    let ids_end = 12usize
        .checked_add(
            shared_count
                .checked_mul(4)
                .ok_or_else(|| Error::invalid("EROFS xattr count overflow"))?,
        )
        .ok_or_else(|| Error::invalid("EROFS xattr count overflow"))?;
    if ids_end > area.len() {
        return Err(Error::invalid("truncated EROFS xattr area"));
    }
    for index in 0..shared_count {
        let id = buf::u32le(&area, 12 + index * 4)?;
        out.push(read_shared(fs, image, id)?);
    }
    let mut offset = ids_end;
    while offset < area.len() {
        let (entry, next) = parse_entry(&area, offset, area.len())?;
        out.push(entry);
        if next <= offset {
            return Err(Error::invalid("invalid EROFS xattr entry"));
        }
        offset = next;
    }
    Ok(out)
}

/// Encode one entry (`{len, index, size} + name + value`, 4-padded).
pub fn encode_entry(index: u8, name: &[u8], value: &[u8], out: &mut Vec<u8>) {
    out.push(name.len() as u8);
    out.push(index);
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(value);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// Pack unique contexts into a shared xattr table.
/// Returns `(table_bytes, ids)` with `ids[i]` = entry offset/4 of
/// `contexts[i]`. Table entries are 4-aligned by construction.
pub fn pack_shared_table(contexts: &[String]) -> (Vec<u8>, Vec<u32>) {
    let mut table = Vec::new();
    let mut ids = Vec::with_capacity(contexts.len());
    for context in contexts {
        ids.push((table.len() / 4) as u32);
        encode_entry(INDEX_SECURITY, b"selinux", context.as_bytes(), &mut table);
    }
    (table, ids)
}

/// Inline area bytes for `shared_ids` (12-byte header + IDs, no inline
/// entries). `w0` is emitted as 0 (accepted by all observed images).
pub fn area_bytes(shared_ids: &[u32]) -> Vec<u8> {
    let mut area = Vec::with_capacity(12 + shared_ids.len() * 4);
    area.extend_from_slice(&0u32.to_le_bytes());
    area.extend_from_slice(&(shared_ids.len() as u32).to_le_bytes());
    area.extend_from_slice(&0u32.to_le_bytes());
    for id in shared_ids {
        area.extend_from_slice(&id.to_le_bytes());
    }
    area
}
