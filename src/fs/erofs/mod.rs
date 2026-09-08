pub mod compress;
pub mod dir;
pub mod inode;
pub mod xattr;

use crate::core::{Error, Image, buf};

pub const SUPER_OFFSET: u64 = 1024;
pub const MAGIC: u32 = 0xe0f5_e1e2;
pub const DIRECTORY: u16 = 0x4000;

#[derive(Debug, Clone)]
pub struct Superblock {
    pub block_size: u64,
    pub root_nid: u64,
    pub meta_blkaddr: u64,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub volume: [u8; 16],
    pub build_time: u64,
    pub build_nsec: u32,
    pub inodes: u64,
    pub xattr_blkaddr: u64,
}

pub fn probe(image: &mut Image) -> Result<bool, Error> {
    let mut magic = [0u8; 4];
    image.read_at(SUPER_OFFSET, &mut magic)?;
    Ok(u32::from_le_bytes(magic) == MAGIC)
}

impl Superblock {
    pub fn read(image: &mut Image) -> Result<Self, Error> {
        let mut data = [0u8; 128];
        image.read_at(SUPER_OFFSET, &mut data)?;
        if buf::u32le(&data, 0)? != MAGIC {
            return Err(Error::invalid("EROFS magic is missing"));
        }
        let blkszbits = buf::bytes(&data, 12, 1)?[0];
        let block_size = 1u64
            .checked_shl(blkszbits as u32)
            .ok_or_else(|| Error::invalid("invalid EROFS block size"))?;
        // Sanity cap: real images use 4 KiB; absurd sizes would only
        // cause giant allocations on corrupt inputs.
        if block_size < 512 || block_size > 1024 * 1024 || !block_size.is_power_of_two() {
            return Err(Error::invalid("invalid EROFS block size"));
        }
        let mut volume = [0u8; 16];
        volume.copy_from_slice(buf::bytes(&data, 64, 16)?);
        if buf::u32le(&data, 36)? == 0 {
            return Err(Error::invalid("EROFS block count is zero"));
        }
        Ok(Self {
            block_size,
            root_nid: buf::u16le(&data, 14)? as u64,
            meta_blkaddr: buf::u32le(&data, 40)? as u64,
            feature_compat: buf::u32le(&data, 8)?,
            feature_incompat: buf::u32le(&data, 80)?,
            feature_ro_compat: buf::u32le(&data, 100)?,
            volume,
            build_time: buf::u64le(&data, 24)?,
            build_nsec: buf::u32le(&data, 32)?,
            inodes: buf::u64le(&data, 16)?,
            xattr_blkaddr: buf::u32le(&data, 44)? as u64,
        })
    }

    pub fn block_bits(&self) -> u32 {
        self.block_size.trailing_zeros()
    }
}
