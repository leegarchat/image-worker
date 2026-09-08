use super::{EXT4_DIRECTORY, Superblock};
use super::inode;
use crate::core::{Error, Image, buf};
use crate::fs::DirEntry;

#[derive(Debug, Clone)]
pub struct DirEntryRaw {
    pub inode: u32,
    pub file_type: u8,
    pub name: String,
}

pub fn read(
    sb: &Superblock,
    image: &mut Image,
    inode_number: u32,
) -> Result<Vec<DirEntryRaw>, Error> {
    let inode = inode::read(sb, image, inode_number)?;
    if inode.mode & EXT4_DIRECTORY == 0 {
        return Err(Error::invalid("path is not a directory"));
    }
    let data = inode::read_data(sb, image, &inode)?;
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset + 8 <= data.len() {
        let bad = || Error::invalid("invalid directory record");
        let entry_inode = buf::u32le(&data, offset).map_err(|_| bad())?;
        let record_len = buf::u16le(&data, offset + 4).map_err(|_| bad())? as usize;
        let name_len = data[offset + 6] as usize;
        let file_type = data[offset + 7];
        if record_len < 8 || offset + record_len > data.len() {
            return Err(Error::invalid("invalid directory record"));
        }
        if entry_inode != 0 && 8 + name_len <= record_len {
            let name =
                String::from_utf8_lossy(&data[offset + 8..offset + 8 + name_len]).into_owned();
            entries.push(DirEntryRaw {
                inode: entry_inode,
                file_type,
                name,
            });
        }
        offset += record_len;
    }
    Ok(entries)
}

/// Materialize one directory entry (context rides in the inode struct,
/// so no extra I/O). Returns the entry plus the parsed inode (needed
/// for content sniffing without re-reading).
pub fn stat(
    sb: &Superblock,
    image: &mut Image,
    raw: &DirEntryRaw,
) -> Result<(DirEntry, inode::Ext4Inode), Error> {
    let inode = inode::read(sb, image, raw.inode)?;
    Ok((
        DirEntry {
            name: raw.name.clone(),
            file_type: raw.file_type,
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            size: inode.size,
            context: inode.context.clone(),
        },
        inode,
    ))
}
