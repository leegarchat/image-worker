use super::Superblock;
use super::inode as inode_mod;
use crate::core::{Error, Image, buf};
use crate::fs::DirEntry;

#[derive(Debug, Clone)]
pub struct DirEntryRaw {
    pub nid: u64,
    pub file_type: u8,
    pub name: String,
}

pub fn read(fs: &Superblock, image: &mut Image, nid: u64) -> Result<Vec<DirEntryRaw>, Error> {
    let inode = inode_mod::read(fs, image, nid)?;
    if inode.mode & super::DIRECTORY == 0 {
        return Err(Error::invalid("path is not a directory"));
    }
    let data = inode_mod::read_data(fs, image, &inode)?;
    parse(&data, fs.block_size as usize)
}

/// Materialize one directory entry: a single inode read (plus an xattr
/// lookup when `want_context`). Keeps per-entry I/O identical to manual
/// reads; used by metadata listings while plain name listings keep the
/// cheaper raw path. Returns the entry plus the parsed inode (needed for
/// content sniffing without re-reading).
pub fn stat(
    fs: &Superblock,
    image: &mut Image,
    raw: &DirEntryRaw,
    want_context: bool,
) -> Result<(DirEntry, inode_mod::Inode), Error> {
    let inode = inode_mod::read(fs, image, raw.nid)?;
    let context = if want_context {
        super::xattr::selinux(fs, image, &inode)?
    } else {
        None
    };
    Ok((
        DirEntry {
            name: raw.name.clone(),
            file_type: raw.file_type,
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            size: inode.size,
            context,
        },
        inode,
    ))
}

/// Shared directory parser (also reused by the write engine).
pub fn parse(data: &[u8], block_size: usize) -> Result<Vec<DirEntryRaw>, Error> {
    let mut result = Vec::new();
    for (block_index, block) in data.chunks(block_size).enumerate() {
        if block.len() < 12 {
            continue;
        }
        let first_nameoff = buf::u16le(block, 8)? as usize;
        if first_nameoff == 0 {
            continue;
        }
        if first_nameoff < 12 || first_nameoff > block.len() {
            return Err(Error::invalid(format!(
                "invalid EROFS directory name offset in block {block_index}: {first_nameoff}"
            )));
        }
        let count = first_nameoff / 12;
        for index in 0..count {
            let offset = index * 12;
            if offset + 12 > block.len() {
                break;
            }
            let nid = buf::u64le(block, offset)?;
            let name_start = buf::u16le(block, offset + 8)? as usize;
            let name_end = if index + 1 < count {
                buf::u16le(block, offset + 20)? as usize
            } else {
                block.len()
            };
            if nid == 0 || name_start < first_nameoff || name_end < name_start || name_end > block.len()
            {
                continue;
            }
            result.push(DirEntryRaw {
                nid,
                file_type: block[offset + 10],
                name: String::from_utf8_lossy(&block[name_start..name_end])
                    .trim_end_matches('\0')
                    .to_owned(),
            });
        }
    }
    Ok(result)
}
