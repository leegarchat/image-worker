pub mod dir;
pub mod inode;
pub mod selinux;

use crate::core::{Error, Image, buf};

pub const EXT4_MAGIC: u16 = 0xef53;
pub const EXT4_EXTENTS: u32 = 0x0040;
pub const EXT4_64BIT: u32 = 0x0080;
pub const EXT4_METADATA_CSUM: u32 = 0x0400;
pub const EXT4_SHARED_BLOCKS: u32 = 0x4000;
pub const EXT4_EXTENT_MAGIC: u16 = 0xf30a;
pub const EXT4_DIRECTORY: u16 = 0x4000;
pub const EXT4_ROOT_INODE: u32 = 2;

#[derive(Debug, Clone)]
pub struct Superblock {
    pub block_size: u64,
    pub inode_size: u16,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub group_descriptor_size: u16,
    pub block_count: u64,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
}

pub fn probe(image: &mut Image) -> Result<bool, Error> {
    let mut data = [0u8; 1024];
    image.read_at(1024, &mut data)?;
    Ok(u16::from_le_bytes([data[56], data[57]]) == EXT4_MAGIC)
}

impl Superblock {
    pub fn read(image: &mut Image) -> Result<Self, Error> {
        let mut data = [0u8; 1024];
        image.read_at(1024, &mut data)?;
        if u16::from_le_bytes([data[56], data[57]]) != EXT4_MAGIC {
            return Err(Error::invalid("ext4 magic is missing"));
        }
        let log_block_size = buf::u32le(&data, 24)?;
        let block_size = 1024u64
            .checked_shl(log_block_size)
            .ok_or_else(|| Error::invalid("invalid ext4 block size"))?;
        // Sanity cap: the kernel max is 64 KiB; absurd sizes would only
        // cause giant allocations on corrupt inputs.
        if block_size < 1024 || block_size > 1024 * 1024 {
            return Err(Error::invalid("invalid ext4 block size"));
        }
        let blocks_lo = buf::u32le(&data, 4)? as u64;
        // NOTE: 64bit flag lives in incompat (offset 96), not ro_compat (100).
        // image-inspect historically read offset 100 here; fixed to 96.
        let blocks_hi = if buf::u32le(&data, 96)? & EXT4_64BIT != 0 {
            buf::u32le(&data, 336)? as u64
        } else {
            0
        };
        let descriptor_size = buf::u16le(&data, 254)?.max(32);
        Ok(Self {
            block_size,
            inode_size: buf::u16le(&data, 88)?,
            blocks_per_group: buf::u32le(&data, 32)?,
            inodes_per_group: buf::u32le(&data, 40)?,
            group_descriptor_size: descriptor_size,
            block_count: blocks_lo | (blocks_hi << 32),
            compat: buf::u32le(&data, 92)?,
            incompat: buf::u32le(&data, 96)?,
            ro_compat: buf::u32le(&data, 100)?,
        })
    }

    pub fn group_count(&self) -> u32 {
        self.block_count.div_ceil(self.blocks_per_group as u64) as u32
    }
}
