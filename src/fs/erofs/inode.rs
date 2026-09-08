use super::Superblock;
use super::compress;
use crate::core::{Error, Image, buf};
use std::io::Write;

pub const INODE_EXTENDED: u16 = 1;
pub const LAYOUT_PLAIN: u16 = 0;
pub const LAYOUT_INLINE: u16 = 2;
pub const LAYOUT_COMPRESSED_COMPACT: u16 = 3;

/// Upper bound for a single buffered file read (corrupt sizes must error,
/// never abort on a giant allocation). Streaming readers below stay in
/// the KiB range regardless of file size.
pub const MAX_FILE_BUFFER: u64 = 16 * 1024 * 1024 * 1024;
/// I/O quantum for streaming readers: bounded stack/heap buffers only.
pub const STREAM_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Inode {
    pub nid: u64,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub layout: u16,
    pub data_offset: u64,
    pub raw_blkaddr: u32,
    pub advise: u16,
    pub algorithm_type: u8,
    pub cluster_bits: u8,
    /// Inline xattr area size in bytes (0 when `i_xattr_icount == 0`).
    /// Needed to locate shared/inline attributes without re-reading.
    pub xattr_size: u64,
    /// On-disk inode body size: 32 (compact) or 64 (extended).
    pub inode_size: u64,
}

pub fn read(fs: &Superblock, image: &mut Image, nid: u64) -> Result<Inode, Error> {
    let offset = (fs.meta_blkaddr << fs.block_bits())
        .checked_add(nid << 5)
        .ok_or_else(|| Error::invalid("EROFS inode offset overflow"))?;
    let mut compact = [0u8; 64];
    image.read_at(offset, &mut compact)?;
    let format = buf::u16le(&compact, 0)?;
    let version = format & 1;
    let layout = (format >> 1) & 7;
    let inode_size = if version == INODE_EXTENDED { 64 } else { 32 };
    let xattr_count = buf::u16le(&compact, 2)? as u64;
    // Total inline xattr area: 8-byte base plus 4-byte units.
    // (i_xattr_icount counts 4-byte units after the first 8 header bytes.)
    let xattr_size = if xattr_count == 0 {
        0
    } else {
        8 + xattr_count
            .checked_mul(4)
            .ok_or_else(|| Error::invalid("EROFS xattr size overflow"))?
    };
    if xattr_size > 1024 * 1024 {
        return Err(Error::invalid("EROFS xattr area is too large"));
    }
    let size = if version == INODE_EXTENDED {
        buf::u64le(&compact, 8)?
    } else {
        buf::u32le(&compact, 8)? as u64
    };
    let uid = if version == INODE_EXTENDED {
        buf::u32le(&compact, 24)?
    } else {
        buf::u16le(&compact, 24)? as u32
    };
    let gid = if version == INODE_EXTENDED {
        buf::u32le(&compact, 28)?
    } else {
        buf::u16le(&compact, 26)? as u32
    };
    // Map header of compressed-compact inodes is 8-byte aligned, everything
    // else is 4-byte aligned after inode + xattrs.
    let data_offset = if layout == LAYOUT_COMPRESSED_COMPACT {
        (offset + inode_size + xattr_size + 7) & !7
    } else {
        (offset + inode_size + xattr_size + 3) & !3
    };
    let mut map_header = [0u8; 8];
    if layout == LAYOUT_COMPRESSED_COMPACT {
        image.read_at(data_offset, &mut map_header)?;
    }
    let map_advise = buf::u16le(&map_header, 4)?;
    Ok(Inode {
        nid,
        mode: buf::u16le(&compact, 4)?,
        uid,
        gid,
        size,
        layout,
        data_offset,
        raw_blkaddr: buf::u32le(&compact, 16)?,
        advise: map_advise,
        algorithm_type: map_header[6],
        cluster_bits: map_header[7] & 7,
        xattr_size,
        inode_size,
    })
}

pub fn read_data(fs: &Superblock, image: &mut Image, inode: &Inode) -> Result<Vec<u8>, Error> {
    // Cap single-file buffering: corrupt sizes must error, never abort the
    // process on a giant allocation. Real vendor files stay far below this.
    if inode.size > MAX_FILE_BUFFER {
        return Err(Error::invalid("EROFS file is too large"));
    }
    let size = usize::try_from(inode.size)
        .map_err(|_| Error::invalid("EROFS file is too large"))?;
    let mut data = vec![0u8; size];
    if size == 0 {
        return Ok(data);
    }
    match inode.layout {
        LAYOUT_PLAIN => {
            image.read_at(inode.raw_blkaddr as u64 * fs.block_size, &mut data)?;
        }
        LAYOUT_INLINE => {
            // Pure inline data (raw_blkaddr == 0) lives at data_offset;
            // otherwise full blocks come from raw_blkaddr and the tail
            // comes from data_offset (image-inspect inline fix).
            if inode.raw_blkaddr == 0 {
                image.read_at(inode.data_offset, &mut data)?;
            } else {
                let full = size / fs.block_size as usize;
                if full > 0 {
                    image.read_at(
                        inode.raw_blkaddr as u64 * fs.block_size,
                        &mut data[..full * fs.block_size as usize],
                    )?;
                }
                if full * (fs.block_size as usize) < size {
                    image.read_at(
                        inode.data_offset,
                        &mut data[full * fs.block_size as usize..],
                    )?;
                }
            }
        }
        LAYOUT_COMPRESSED_COMPACT => {
            compress::read_compact_data(fs, image, inode, &mut data)?;
        }
        other => {
            return Err(Error::invalid(format!(
                "unsupported EROFS data layout {other}"
            )));
        }
    }
    Ok(data)
}

/// Read `[offset, offset+buf.len())` of a file into `buf`, positions past
/// the end read as zero. Returns bytes placed (0 only at exact EOF with
/// an empty span). Offsets beyond the file size are an error, matching
/// `Image::read_at` semantics. Only covering pclusters are decoded.
pub fn read_range(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, Error> {
    if offset > inode.size {
        return Err(Error::invalid("EROFS read past end of file"));
    }
    let len = (inode.size - offset).min(buf.len() as u64) as usize;
    if len == 0 {
        return Ok(0);
    }
    match inode.layout {
        LAYOUT_PLAIN => {
            let base = (inode.raw_blkaddr as u64)
                .checked_mul(fs.block_size)
                .and_then(|base| base.checked_add(offset))
                .ok_or_else(|| Error::invalid("EROFS block offset overflow"))?;
            image.read_at(base, &mut buf[..len])?;
            Ok(len)
        }
        LAYOUT_INLINE => {
            // Inline files: full leading blocks at raw_blkaddr (unless the
            // file is pure-inline), tail bytes at data_offset.
            let bs = fs.block_size;
            let full = if inode.raw_blkaddr == 0 {
                0
            } else {
                (inode.size / bs) * bs
            };
            let mut done = 0usize;
            while done < len {
                let pos = offset + done as u64;
                let chunk = if pos < full {
                    let n = (full - pos).min((len - done) as u64) as usize;
                    image.read_at(
                        (inode.raw_blkaddr as u64) * bs + pos,
                        &mut buf[done..done + n],
                    )?;
                    n
                } else {
                    let tail_pos = pos - full;
                    let n = (inode.size - pos).min((len - done) as u64) as usize;
                    image.read_at(
                        inode
                            .data_offset
                            .checked_add(tail_pos)
                            .ok_or_else(|| Error::invalid("EROFS offset overflow"))?,
                        &mut buf[done..done + n],
                    )?;
                    n
                };
                done += chunk;
            }
            Ok(len)
        }
        LAYOUT_COMPRESSED_COMPACT => {
            let index = compress::compact_index(fs, image, inode)?;
            let mut done = 0usize;
            for seg in compress::segments(&index, inode)? {
                if seg.end <= offset || seg.start >= offset + len as u64 {
                    continue;
                }
                // Overlap of [offset, offset+len) with this segment.
                // Physical blocks are used as derived (kernel resolves the
                // same way from anchors; no cursor chaining needed).
                let seg_start = offset.max(seg.start);
                let seg_end = (offset + len as u64).min(seg.end);
                let out_start = (seg_start - offset) as usize;
                let out_end = (seg_end - offset) as usize;
                if seg.kind == 0 {
                    // Plain run: contiguous physical blocks, read the slice
                    // directly into the caller's buffer (no temp copy).
                    image.read_at(
                        seg.physical
                            .checked_mul(fs.block_size)
                            .and_then(|b| b.checked_add(seg_start - seg.start))
                            .ok_or_else(|| Error::invalid("EROFS block offset overflow"))?,
                        &mut buf[out_start..out_end],
                    )?;
                } else {
                    // Compressed pcluster: decode once (bounded by the
                    // MAX_PCLUSTER cap enforced in segments()), slice out.
                    let seg_len = (seg.end - seg.start) as usize;
                    let mut decoded = vec![0u8; seg_len];
                    let got = compress::read_segment(fs, image, &seg, &mut decoded)?;
                    if got != seg_len {
                        return Err(Error::invalid("short EROFS segment decode"));
                    }
                    let from = (seg_start - seg.start) as usize;
                    let to = (seg_end - seg.start) as usize;
                    buf[out_start..out_end].copy_from_slice(&decoded[from..to]);
                }
                done = done.max(out_end);
                if done >= len {
                    break;
                }
            }
            Ok(done)
        }
        other => Err(Error::invalid(format!(
            "unsupported EROFS data layout {other}"
        ))),
    }
}

/// Stream a whole file in 64 KiB quanta: the index is resolved once,
/// then plain runs stream through and each pcluster decodes once.
/// Peak RAM is one quantum plus a single pcluster, regardless of size.
pub fn read_data_streaming(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    out: &mut dyn Write,
) -> Result<(), Error> {
    match inode.layout {
        LAYOUT_PLAIN | LAYOUT_INLINE => {
            // Position-independent layouts stream without any index.
            let mut offset = 0u64;
            let mut buf = vec![0u8; STREAM_CHUNK];
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
        LAYOUT_COMPRESSED_COMPACT => {
            let index = compress::compact_index(fs, image, inode)?;
            let mut buf = vec![0u8; STREAM_CHUNK];
            let mut decoded: Vec<u8> = Vec::new();
            for seg in compress::segments(&index, inode)?.iter() {
                let seg_len = seg.end - seg.start;
                if seg.kind == 0 {
                    // Plain run: stream block-by-block through one quantum.
                    let mut pos = seg.start;
                    while pos < seg.end {
                        let n = (seg.end - pos).min(STREAM_CHUNK as u64) as usize;
                        image.read_at(
                            seg.physical
                                .checked_mul(fs.block_size)
                                .and_then(|b| b.checked_add(pos - seg.start))
                                .ok_or_else(|| {
                                    Error::invalid("EROFS block offset overflow")
                                })?,
                            &mut buf[..n],
                        )?;
                        out.write_all(&buf[..n])?;
                        pos += n as u64;
                    }
                } else {
                    // Compressed pcluster: decode once, then stream out.
                    decoded.clear();
                    decoded.resize(seg_len as usize, 0);
                    let got = compress::read_segment(fs, image, seg, &mut decoded)?;
                    if got != decoded.len() {
                        return Err(Error::invalid("short EROFS segment decode"));
                    }
                    let mut pos = 0usize;
                    while pos < decoded.len() {
                        let n = (decoded.len() - pos).min(STREAM_CHUNK);
                        out.write_all(&decoded[pos..pos + n])?;
                        pos += n;
                    }
                }
            }
            Ok(())
        }
        other => Err(Error::invalid(format!(
            "unsupported EROFS data layout {other}"
        ))),
    }
}

/// Bounded prefix read (content sniffing) without decoding the tail.
pub fn read_prefix(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    limit: u64,
) -> Result<Vec<u8>, Error> {
    let want = inode.size.min(limit) as usize;
    let mut out = vec![0u8; want];
    let mut done = 0usize;
    while done < want {
        let n = read_range(fs, image, inode, done as u64, &mut out[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    Ok(out)
}
