use super::{EXT4_64BIT, EXT4_EXTENT_MAGIC, Superblock};
use super::selinux::{read_selinux_context, read_selinux_from_block};
use crate::core::{Error, Image, buf};
use std::io::Write;

#[derive(Debug, Clone)]
pub struct Ext4Inode {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub flags: u32,
    pub block: [u8; 60],
    pub context: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Extent {
    pub logical_block: u64,
    pub physical_block: u64,
    pub length: u64,
    pub unwritten: bool,
}

pub fn read(sb: &Superblock, image: &mut Image, inode_number: u32) -> Result<Ext4Inode, Error> {
    if inode_number == 0 || inode_number > sb.inodes_per_group * sb.group_count() {
        return Err(Error::invalid(format!(
            "inode {inode_number} is out of range"
        )));
    }
    let group = (inode_number - 1) / sb.inodes_per_group;
    let index = (inode_number - 1) % sb.inodes_per_group;
    let descriptor_block = if sb.block_size == 1024 { 2 } else { 1 };
    let descriptor_offset =
        descriptor_block as u64 * sb.block_size + group as u64 * sb.group_descriptor_size as u64;
    let mut descriptor = vec![0u8; sb.group_descriptor_size as usize];
    image.read_at(descriptor_offset, &mut descriptor)?;
    let table_lo = buf::u32le(&descriptor, 8)? as u64;
    let table_hi = if sb.incompat & EXT4_64BIT != 0 && descriptor.len() >= 44 {
        buf::u32le(&descriptor, 40)? as u64
    } else {
        0
    };
    let table = table_lo | (table_hi << 32);
    let inode_offset = table
        .checked_mul(sb.block_size)
        .and_then(|o| o.checked_add(index as u64 * sb.inode_size as u64))
        .ok_or_else(|| Error::invalid("inode offset overflow"))?;
    let mut raw = vec![0u8; sb.inode_size as usize];
    image.read_at(inode_offset, &mut raw)?;
    let mut block = [0u8; 60];
    block.copy_from_slice(&raw[40..100]);
    let size_lo = buf::u32le(&raw, 4)? as u64;
    let size_hi = if raw.len() >= 112 {
        buf::u32le(&raw, 108)? as u64
    } else {
        0
    };
    let uid = buf::u16le(&raw, 2)? as u32
        | if raw.len() >= 122 {
            buf::u16le(&raw, 120)? as u32
        } else {
            0
        } << 16;
    let gid = buf::u16le(&raw, 24)? as u32
        | if raw.len() >= 124 {
            buf::u16le(&raw, 122)? as u32
        } else {
            0
        } << 16;
    let mut context = read_selinux_context(&raw);
    if context.is_none() {
        let file_acl_lo = buf::u32le(&raw, 104)? as u64;
        let file_acl_hi = if sb.incompat & EXT4_64BIT != 0 && raw.len() >= 132 {
            buf::u16le(&raw, 130)? as u64
        } else {
            0
        };
        let acl_block = file_acl_lo | (file_acl_hi << 32);
        if acl_block != 0 && acl_block < sb.block_count {
            let mut acl_buf = vec![0u8; sb.block_size as usize];
            image.read_at(acl_block * sb.block_size, &mut acl_buf)?;
            context = read_selinux_from_block(&acl_buf);
        }
    }

    Ok(Ext4Inode {
        mode: buf::u16le(&raw, 0)?,
        uid,
        gid,
        size: size_lo | (size_hi << 32),
        flags: buf::u32le(&raw, 32)?,
        block,
        context,
    })
}

pub fn collect_extents(
    sb: &Superblock,
    image: &mut Image,
    node: &[u8],
    depth: u16,
    entries: usize,
    extents: &mut Vec<Extent>,
) -> Result<(), Error> {
    // Depth cap: real trees are 2-3 levels; unbounded recursion on a
    // corrupt depth field would exhaust the stack.
    if depth > 16 {
        return Err(Error::invalid("invalid ext4 extent depth"));
    }
    if node.len() < 12 || u16::from_le_bytes([node[0], node[1]]) != EXT4_EXTENT_MAGIC {
        return Err(Error::invalid("invalid ext4 extent node"));
    }
    let available = ((node.len() - 12) / 12).min(entries);
    for index in 0..available {
        let off = 12 + index * 12;
        // Bounds are guaranteed by `available` + fixed 12-byte entries;
        // buf readers still guard against arithmetic edge cases.
        let bad = || Error::invalid("invalid ext4 extent node");
        let logical = u64::from(buf::u32le(node, off).map_err(|_| bad())?);
        if depth == 0 {
            let raw_len = buf::u16le(node, off + 4).map_err(|_| bad())?;
            let physical = (u64::from(buf::u16le(node, off + 6).map_err(|_| bad())?)) << 32
                | u64::from(buf::u32le(node, off + 8).map_err(|_| bad())?);
            extents.push(Extent {
                logical_block: logical,
                physical_block: physical,
                length: (raw_len & 0x7fff) as u64,
                unwritten: raw_len & 0x8000 != 0,
            });
        } else {
            let physical = (u64::from(buf::u16le(node, off + 8).map_err(|_| bad())?)) << 32
                | u64::from(buf::u32le(node, off + 4).map_err(|_| bad())?);
            let mut child = vec![0u8; sb.block_size as usize];
            image.read_at(physical * sb.block_size, &mut child)?;
            let child_entries = u16::from_le_bytes([child[2], child[3]]) as usize;
            let child_depth = u16::from_le_bytes([child[6], child[7]]);
            if child_depth >= depth {
                return Err(Error::invalid("invalid ext4 extent depth"));
            }
            collect_extents(sb, image, &child, child_depth, child_entries, extents)?;
        }
    }
    Ok(())
}

fn check_extent_inode(inode: &Ext4Inode) -> Result<(usize, u16), Error> {
    if inode.flags & 0x80000 == 0 {
        return Err(Error::invalid("only ext4 extent inodes are supported"));
    }
    let magic = u16::from_le_bytes([inode.block[0], inode.block[1]]);
    if magic != EXT4_EXTENT_MAGIC {
        return Err(Error::invalid("invalid ext4 extent header"));
    }
    let entries = u16::from_le_bytes([inode.block[2], inode.block[3]]) as usize;
    let depth = u16::from_le_bytes([inode.block[6], inode.block[7]]);
    Ok((entries, depth))
}

/// Full file read (holes / unwritten -> zero). Backed by zero-initialised vec.
pub fn read_data(sb: &Superblock, image: &mut Image, inode: &Ext4Inode) -> Result<Vec<u8>, Error> {
    // Cap single-file buffering: corrupt sizes must error, never abort the
    // process on a giant allocation. Real vendor files stay far below this.
    if inode.size > 16 * 1024 * 1024 * 1024 {
        return Err(Error::invalid("file is too large"));
    }
    read_prefix(sb, image, inode, inode.size)
}

/// Prefix read up to `limit` (used for `-F` content sniffing without
/// loading multi-GB files).
pub fn read_prefix(
    sb: &Superblock,
    image: &mut Image,
    inode: &Ext4Inode,
    limit: u64,
) -> Result<Vec<u8>, Error> {
    // Inline symlink targets live in the inode body, not in extents.
    if inode.mode & 0xf000 == 0xa000 && inode.size <= 60 {
        let size = usize::try_from(inode.size.min(limit))
            .map_err(|_| Error::invalid("file is too large"))?;
        return Ok(inode.block[..size].to_vec());
    };
    let size =
        usize::try_from(inode.size.min(limit)).map_err(|_| Error::invalid("file is too large"))?;
    let mut data = vec![0u8; size];
    if size == 0 {
        return Ok(data);
    }
    let (entries, depth) = check_extent_inode(inode)?;
    let mut extents = Vec::new();
    collect_extents(sb, image, &inode.block, depth, entries, &mut extents)?;
    for extent in extents {
        let dest = extent
            .logical_block
            .checked_mul(sb.block_size)
            .ok_or_else(|| Error::invalid("extent destination overflow"))?;
        if dest >= size as u64 || extent.unwritten {
            continue;
        }
        let bytes = extent
            .length
            .checked_mul(sb.block_size)
            .ok_or_else(|| Error::invalid("extent length overflow"))?
            .min(size as u64 - dest) as usize;
        let physical = extent
            .physical_block
            .checked_mul(sb.block_size)
            .ok_or_else(|| Error::invalid("extent physical offset overflow"))?;
        image.read_at(physical, &mut data[dest as usize..dest as usize + bytes])?;
    }
    Ok(data)
}

/// Streaming read (holes / unwritten -> zero, gaps -> zero). Used by `cat`
/// so large files never need a full in-memory copy.
pub fn write_data(
    sb: &Superblock,
    image: &mut Image,
    inode: &Ext4Inode,
    output: &mut dyn Write,
) -> Result<(), Error> {
    if inode.size == 0 {
        return Ok(());
    }
    // Inline symlink targets live in the inode body, not in extents.
    if inode.mode & 0xf000 == 0xa000 && inode.size <= 60 {
        let size = usize::try_from(inode.size)
            .map_err(|_| Error::invalid("file is too large"))?;
        output.write_all(&inode.block[..size])?;
        return Ok(());
    }
    let (entries, depth) = check_extent_inode(inode)?;
    let mut extents = Vec::new();
    collect_extents(sb, image, &inode.block, depth, entries, &mut extents)?;
    extents.sort_by_key(|e| e.logical_block);

    let mut cursor = 0u64;
    let zeros = [0u8; 8192];
    for extent in extents {
        let start = extent
            .logical_block
            .checked_mul(sb.block_size)
            .ok_or_else(|| Error::invalid("extent destination overflow"))?;
        if start > cursor {
            write_zeroes(output, &zeros, start - cursor)?;
        }
        let extent_bytes = extent
            .length
            .checked_mul(sb.block_size)
            .ok_or_else(|| Error::invalid("extent length overflow"))?;
        let end = start.saturating_add(extent_bytes).min(inode.size);
        if !extent.unwritten && start < inode.size && end > start {
            let physical = extent
                .physical_block
                .checked_mul(sb.block_size)
                .ok_or_else(|| Error::invalid("extent physical offset overflow"))?;
            let mut off = 0u64;
            let mut buf = [0u8; 65536];
            while start + off < end {
                let count = ((end - start - off) as usize).min(buf.len());
                image.read_at(physical + off, &mut buf[..count])?;
                output.write_all(&buf[..count])?;
                off += count as u64;
            }
        } else if end > cursor {
            write_zeroes(output, &zeros, end - cursor)?;
        }
        cursor = cursor.max(end);
        if cursor >= inode.size {
            break;
        }
    }
    if cursor < inode.size {
        write_zeroes(output, &zeros, inode.size - cursor)?;
    }
    Ok(())
}

fn write_zeroes(output: &mut dyn Write, zeros: &[u8], mut count: u64) -> Result<(), Error> {
    while count > 0 {
        let chunk = count.min(zeros.len() as u64) as usize;
        output.write_all(&zeros[..chunk])?;
        count -= chunk as u64;
    }
    Ok(())
}
