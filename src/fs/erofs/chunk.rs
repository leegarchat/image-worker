//! EROFS chunk-based files (`EROFS_INODE_CHUNK_BASED`, data layout 4).
//!
//! On-disk contract (upstream `erofs_fs.h` + `erofs-utils lib/data.c`
//! `__erofs_map_blocks`, chunked branch):
//!   - the inode `i_u` word (offset 0x10) is a chunk summary: bits 0-4 are
//!     the chunk-size shift (`chunksize = blocksize << chunkbits`), bit 5
//!     (`INDEXES`) selects 8-byte chunk indexes over the 4-byte block map,
//!     bit 6 is 48-bit-addressing specific; bits 7+ must be zero.
//!   - the chunk table lives at `roundup(iloc + inode_size + xattr_size,
//!     entry_size)` with one entry per `ceil(i_size / chunksize)` chunk:
//!     a plain `u32` block address, or
//!     `(u16 hi, u16 device_id, u32 lo)` for the 8-byte index form.
//!   - an all-ones address (`EROFS_NULL_ADDR`) is a hole and reads as zero.
//!   - chunked files are never compressed; every chunk is whole blocks.
//!   - multi-device entries (`device_id != 0`, device table) cannot be
//!     resolved from a single image: they are a hard error, never silent
//!     wrong bytes.

use super::Superblock;
use super::inode::Inode;
use crate::core::{Error, Image};
use std::io::Write;

pub const LAYOUT_CHUNK_BASED: u16 = 4;
/// Bits 0-4 of the chunk summary: `chunksize = blocksize << chunkbits`.
pub const CHUNK_FORMAT_BLKBITS_MASK: u16 = 0x1f;
/// Bit 5: 8-byte chunk indexes instead of the 4-byte block map.
pub const CHUNK_FORMAT_INDEXES: u16 = 0x20;
/// Bit 6: 48-bit block addresses in 8-byte entries.
const CHUNK_FORMAT_48BIT: u16 = 0x40;
/// All valid summary bits (the kernel rejects anything else).
pub const CHUNK_FORMAT_ALL: u16 = 0x7f;
/// `EROFS_NULL_ADDR`: an all-ones 32-bit entry is a zero-filled hole.
const NULL_ADDR32: u32 = 0xffff_ffff;
/// `EROFS_FEATURE_INCOMPAT_DEVICE_TABLE`: unified addresses need the
/// on-disk device table, which a single-image parser cannot resolve.
const DEVICE_TABLE: u32 = 0x0000_0008;

/// Bytes per chunk-table entry for this inode.
fn entry_size(inode: &Inode) -> u64 {
    if inode.chunk_format & CHUNK_FORMAT_INDEXES != 0 {
        8
    } else {
        4
    }
}

fn check_supported(fs: &Superblock) -> Result<(), Error> {
    if fs.feature_incompat & DEVICE_TABLE != 0 {
        return Err(Error::invalid(
            "multi-device EROFS chunk mapping is not supported",
        ));
    }
    Ok(())
}

fn chunk_count(inode: &Inode) -> Result<u64, Error> {
    if inode.chunk_size == 0 {
        return Err(Error::invalid("EROFS chunk size is zero"));
    }
    Ok(inode.size.div_ceil(inode.chunk_size))
}

/// Resolve chunk `nr` to its physical byte offset in the image.
/// `None` means a hole (`EROFS_NULL_ADDR`): the chunk reads as zeros.
pub(crate) fn map_chunk(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    nr: u64,
) -> Result<Option<u64>, Error> {
    check_supported(fs)?;
    if nr >= chunk_count(inode)? {
        return Err(Error::invalid("EROFS chunk index out of range"));
    }
    let size = entry_size(inode);
    let pos = inode
        .data_offset
        .checked_add(
            nr.checked_mul(size)
                .ok_or_else(|| Error::invalid("EROFS chunk table overflow"))?,
        )
        .ok_or_else(|| Error::invalid("EROFS chunk table overflow"))?;
    if inode.chunk_format & CHUNK_FORMAT_INDEXES != 0 {
        let mut entry = [0u8; 8];
        image.read_at(pos, &mut entry)?;
        let hi = u16::from_le_bytes([entry[0], entry[1]]);
        let device = u16::from_le_bytes([entry[2], entry[3]]);
        let lo = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
        if device != 0 {
            return Err(Error::invalid(format!(
                "multi-device EROFS chunk {nr} (device {device}) is not supported"
            )));
        }
        let mask = if inode.chunk_format & CHUNK_FORMAT_48BIT != 0 {
            (1u64 << 48) - 1
        } else {
            (1u64 << 32) - 1
        };
        let block = (((hi as u64) << 32) | lo as u64) & mask;
        if (block ^ 0xffff_ffff_ffff_ffff) & mask == 0 {
            return Ok(None);
        }
        block
            .checked_mul(fs.block_size)
            .ok_or_else(|| Error::invalid("EROFS chunk block overflow"))
            .map(Some)
    } else {
        let mut entry = [0u8; 4];
        image.read_at(pos, &mut entry)?;
        let block = u32::from_le_bytes(entry);
        if block == NULL_ADDR32 {
            return Ok(None);
        }
        (block as u64)
            .checked_mul(fs.block_size)
            .ok_or_else(|| Error::invalid("EROFS chunk block overflow"))
            .map(Some)
    }
}

/// Whole-file read into `output` (`output.len()` must equal the file size;
/// holes decode as zeros). Mirrors `compress::read_compact_data`.
pub(crate) fn read_data(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    output: &mut [u8],
) -> Result<(), Error> {
    // Bounded single-chunk view: giant (corrupt) chunk sizes must error,
    // never balloon a conversion on 32-bit hosts.
    let chunk = usize::try_from(inode.chunk_size)
        .map_err(|_| Error::invalid("EROFS chunk is too large"))?;
    let mut offset = 0usize;
    let mut nr = 0u64;
    while offset < output.len() {
        let take = chunk.min(output.len() - offset);
        match map_chunk(fs, image, inode, nr)? {
            Some(physical) => image.read_at(physical, &mut output[offset..offset + take])?,
            None => output[offset..offset + take].fill(0),
        }
        offset += take;
        nr += 1;
    }
    Ok(())
}

/// Read `[offset, offset+buf.len())` of a chunked file into `buf`.
/// Returns bytes placed (at most `min(remaining, buf.len())`); holes read
/// as zeros. Mirrors the positional `read_range` contract of the other
/// layouts (no index is cached: one tiny table entry per chunk touched).
pub(crate) fn read_range(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, Error> {
    let total = (inode.size - offset).min(buf.len() as u64);
    let mut done = 0u64;
    while done < total {
        let pos = offset + done;
        let nr = pos
            .checked_div(inode.chunk_size)
            .ok_or_else(|| Error::invalid("EROFS chunk size is zero"))?;
        let intra = pos % inode.chunk_size;
        let take = (inode.chunk_size - intra).min(total - done);
        let take_usize = usize::try_from(take)
            .map_err(|_| Error::invalid("EROFS chunk span is too large"))?;
        let done_usize = usize::try_from(done)
            .map_err(|_| Error::invalid("EROFS chunk span is too large"))?;
        match map_chunk(fs, image, inode, nr)? {
            Some(physical) => image.read_at(
                physical
                    .checked_add(intra)
                    .ok_or_else(|| Error::invalid("EROFS chunk offset overflow"))?,
                &mut buf[done_usize..done_usize + take_usize],
            )?,
            None => buf[done_usize..done_usize + take_usize].fill(0),
        }
        done += take;
    }
    usize::try_from(total).map_err(|_| Error::invalid("EROFS chunk span is too large"))
}

/// Stream a whole chunked file in 64 KiB quanta through `read_range`:
/// peak RAM is one quantum regardless of file size.
pub(crate) fn stream(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    out: &mut dyn Write,
) -> Result<(), Error> {
    let mut offset = 0u64;
    let mut buf = vec![0u8; super::inode::STREAM_CHUNK];
    while offset < inode.size {
        let n = read_range(fs, image, inode, offset, &mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        offset += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::inode;
    use super::*;

    const BS: usize = 4096;

    /// Scratch image layout (block numbers):
    ///   1: metadata (two compact inodes, sparse nids like mkfs emits);
    ///   10/11: chunk payloads `A`/`B`.
    /// Nid 2: 4-byte entries `[10, HOLE]`, 4 KiB chunks, 4196-byte file.
    /// Nid 8: 8-byte INDEXES entries `[(0,0,11), (0xFFFF,0,ALL1)]` with
    /// the 48-bit flag: second chunk is a hole (all 48 bits set).
    fn scratch() -> (tempfile_guard::Guard, Superblock) {
        let blocks = 16usize;
        let mut img = vec![0u8; blocks * BS];
        let put = |img: &mut Vec<u8>, off: usize, data: &[u8]| {
            img[off..off + data.len()].copy_from_slice(data);
        };
        let body = |size: u32, format: u16| {
            let mut b = [0u8; 32];
            b[0..2].copy_from_slice(&format.to_le_bytes());
            b[4..6].copy_from_slice(&0x81a4u16.to_le_bytes());
            b[6..8].copy_from_slice(&1u16.to_le_bytes());
            b[8..12].copy_from_slice(&size.to_le_bytes());
            b
        };
        // Nid 2: layout 4, chunk summary 0 (4 KiB chunks, block map).
        let base = BS;
        put(&mut img, base + 2 * 32, &body(4196, 4 << 1));
        let t1 = (base + 2 * 32 + 32 + 3) & !3;
        img[t1..t1 + 8].copy_from_slice(&[10, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        // Nid 8: layout 4, summary INDEXES|48BIT.
        let mut b8 = body(8192, 4 << 1);
        b8[16..18].copy_from_slice(&(0x20u16 | 0x40u16).to_le_bytes());
        put(&mut img, base + 8 * 32, &b8);
        let t8 = (base + 8 * 32 + 32 + 7) & !7;
        // entry 0: (hi=0, dev=0, lo=11); entry 1: 48-bit hole.
        img[t8..t8 + 16].copy_from_slice(&[
            0, 0, 0, 0, 11, 0, 0, 0, //
            0xff, 0xff, 0, 0, 0xff, 0xff, 0xff, 0xff,
        ]);
        for (blk, byte) in [(10, b'A'), (11, b'B')] {
            img[blk * BS..(blk + 1) * BS].fill(byte);
        }
        // Superblock at 1024.
        let mut sb = [0u8; 128];
        sb[0..4].copy_from_slice(&0xe0f5_e1e2u32.to_le_bytes());
        sb[12] = 12;
        sb[14..16].copy_from_slice(&1u16.to_le_bytes());
        sb[16..24].copy_from_slice(&2u64.to_le_bytes());
        sb[36..40].copy_from_slice(&(blocks as u32).to_le_bytes());
        sb[40..44].copy_from_slice(&1u32.to_le_bytes());
        img[1024..1024 + 128].copy_from_slice(&sb);

        let guard = tempfile_guard::Guard::new(&img);
        let mut image = Image::open(guard.path()).expect("scratch image must open");
        let fs = Superblock::read(&mut image).expect("superblock must parse");
        // The hand-built table offsets must match the reader's own
        // alignment rule (round up to the entry size).
        let four = inode::read(&fs, &mut image, 2).expect("nid 2 must parse");
        assert_eq!(four.layout, LAYOUT_CHUNK_BASED);
        assert_eq!(four.chunk_format, 0);
        assert_eq!(four.chunk_size, 4096);
        assert_eq!(four.data_offset, t1 as u64);
        let eight = inode::read(&fs, &mut image, 8).expect("nid 8 must parse");
        assert_eq!(eight.layout, LAYOUT_CHUNK_BASED);
        assert_eq!(eight.chunk_format, 0x60);
        assert_eq!(eight.chunk_size, 4096);
        assert_eq!(eight.data_offset, t8 as u64);
        (guard, fs)
    }

    #[test]
    fn block_map_with_hole_reads_zero() {
        let (guard, fs) = scratch();
        let mut image = Image::open(guard.path()).unwrap();
        let four = inode::read(&fs, &mut image, 2).unwrap();
        assert_eq!(
            map_chunk(&fs, &mut image, &four, 0).unwrap(),
            Some(10u64 * 4096)
        );
        assert_eq!(map_chunk(&fs, &mut image, &four, 1).unwrap(), None);
        assert!(map_chunk(&fs, &mut image, &four, 2).is_err());

        let mut out = vec![0u8; 4196];
        read_data(&fs, &mut image, &four, &mut out).unwrap();
        assert!(out[..4096].iter().all(|b| *b == b'A'));
        assert!(out[4096..].iter().all(|b| *b == 0));

        // Range reads spanning the data/hole boundary.
        let mut span = vec![0u8; 200];
        let n = read_range(&fs, &mut image, &four, 4096 - 100, &mut span).unwrap();
        assert_eq!(n, 200);
        assert!(span[..100].iter().all(|b| *b == b'A'));
        assert!(span[100..].iter().all(|b| *b == 0));

        // Streaming matches the full read.
        let mut streamed = Vec::new();
        stream(&fs, &mut image, &four, &mut streamed).unwrap();
        assert_eq!(streamed, out);
    }

    #[test]
    fn index_entries_with_48bit_hole() {
        let (guard, fs) = scratch();
        let mut image = Image::open(guard.path()).unwrap();
        let eight = inode::read(&fs, &mut image, 8).unwrap();
        assert_eq!(
            map_chunk(&fs, &mut image, &eight, 0).unwrap(),
            Some(11u64 * 4096)
        );
        assert_eq!(map_chunk(&fs, &mut image, &eight, 1).unwrap(), None);

        let mut out = vec![0u8; 8192];
        read_data(&fs, &mut image, &eight, &mut out).unwrap();
        assert!(out[..4096].iter().all(|b| *b == b'B'));
        assert!(out[4096..].iter().all(|b| *b == 0));
    }

    /// Minimal file-backed temp image helper (no extra dependencies:
    /// stock `Image::open` needs a real path).
    mod tempfile_guard {
        use std::path::{Path, PathBuf};
        pub struct Guard {
            path: PathBuf,
        }
        impl Guard {
            pub fn new(data: &[u8]) -> Self {
                let path: PathBuf = std::env::temp_dir().join(format!(
                    "iw-chunk-test-{}-{}.img",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::write(&path, data).expect("temp image must be writable");
                Self { path }
            }
            pub fn path(&self) -> &Path {
                &self.path
            }
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}
