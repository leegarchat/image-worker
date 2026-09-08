//! Pure format converters: no file actions, just repacking.
//!
//! - Same filesystem  -> container-only conversion (raw <-> sparse),
//!   streaming, payload untouched.
//! - EROFS -> ext4    -> fresh ext4 rebuild from the EROFS tree (plain
//!   files stream block-by-block, compressed files decode once each).
//! - ext4  -> EROFS   -> fresh all-plain EROFS image (EROFS has no
//!   compressor here, so compressed inputs are stored decompressed).
//!
//! All streaming paths are O(1)/O(single file) in RAM.

use super::{
    FsTarget, Node, NodePayload, Source as ErofsSource, assign_nids_strided, collect_contexts,
    directory_bytes, mark_compressed, nodes, sort_tree, stream_compressed_file,
};
use super::ext4::{self, BlockSource};
use crate::core::{Error, image as core_image};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SYMLINK_MODE: u16 = 0xa000;

pub(crate) fn run(
    input: &Path,
    output: &Path,
    is_ext4: bool,
    target: FsTarget,
    sparse_output: bool,
    shared_blocks: Option<bool>,
    reserve_mb: u64,
    compress: crate::fs::erofs::compress::Algo,
) -> Result<(), Error> {
    match (is_ext4, target) {
        (true, FsTarget::Ext4) | (false, FsTarget::Erofs) => {
            let block_size = fs_block_size(input, is_ext4)?;
            core_image::convert_container(input, output, sparse_output, block_size)
        }
        (false, FsTarget::Ext4) => {
            erofs_to_ext4(input, output, sparse_output, shared_blocks.unwrap_or(false), reserve_mb)
        }
        (true, FsTarget::Erofs) => ext4_to_erofs(input, output, sparse_output, compress),
    }
}

/// Filesystem block size for sparse chunking (container conversion).
fn fs_block_size(input: &Path, is_ext4: bool) -> Result<usize, Error> {
    let mut image = core_image::Image::open(input)?;
    if is_ext4 {
        Ok(crate::fs::ext4::Superblock::read(&mut image)?.block_size as usize)
    } else {
        Ok(crate::fs::erofs::Superblock::read(&mut image)?.block_size as usize)
    }
}

// ── EROFS -> ext4 ────────────────────────────────────────────────────

/// Adapter serving EROFS tree content through the ext4 block interface.
/// Plain files stream straight from the source image (no RAM); compressed
/// clusters decode on demand into a single pcluster cache; per-file
/// segments resolve once and are reused across sequential block reads.
/// Peak RAM is one index (~24 B/cluster, freed on file switch) plus one
/// pcluster, never a whole file.
struct ErofsBlockSource<'a> {
    source: &'a mut ErofsSource,
    file_nid: u64,
    segments: Vec<crate::fs::erofs::compress::Segment>,
    seg_idx: usize,
    decoded_idx: usize,
    decoded: Vec<u8>,
}

impl BlockSource for ErofsBlockSource<'_> {
    fn read_node_block(
        &mut self,
        node: &Node,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<(), Error> {
        if let NodePayload::Memory(data) = &node.payload {
            return Ok(slice_serve(data, offset, buf));
        }
        if node.layout == 0 {
            let base = node.raw_block as u64 * self.source.block_size;
            self.source.reader.read_at(
                base.checked_add(offset as u64)
                    .ok_or_else(|| Error::invalid("EROFS block offset overflow"))?,
                buf,
            )?;
            return Ok(());
        }
        if node.layout == 2 {
            // Inline files need no index: serve positionally straight
            // through the shared range reader (also bounds-checks).
            let fs_sb = self.source.fs_superblock();
            let fi = crate::fs::erofs::inode::read(
                &fs_sb,
                &mut self.source.reader,
                node.original_nid,
            )?;
            let mut done = 0usize;
            while done < buf.len() {
                let n = crate::fs::erofs::inode::read_range(
                    &fs_sb,
                    &mut self.source.reader,
                    &fi,
                    (offset + done) as u64,
                    &mut buf[done..],
                )?;
                if n == 0 {
                    buf[done..].fill(0);
                    break;
                }
                done += n;
            }
            return Ok(());
        }
        if node.layout != 3 {
            return Err(Error::invalid(format!(
                "unsupported EROFS data layout {}",
                node.layout
            )));
        }
        // Compressed files: resolve-once segments, decode-on-demand.
        if self.file_nid != node.original_nid {
            let fs_sb = self.source.fs_superblock();
            let fi = crate::fs::erofs::inode::read(
                &fs_sb,
                &mut self.source.reader,
                node.original_nid,
            )?;
            let index = crate::fs::erofs::compress::compact_index(
                &fs_sb,
                &mut self.source.reader,
                &fi,
            )?;
            self.segments = crate::fs::erofs::compress::segments(&index, &fi)?;
            self.file_nid = node.original_nid;
            self.seg_idx = 0;
            self.decoded_idx = usize::MAX;
            self.decoded.clear();
        }
        let fs_sb = self.source.fs_superblock();
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done;
            while self.seg_idx < self.segments.len()
                && self.segments[self.seg_idx].end <= pos as u64
            {
                self.seg_idx += 1;
            }
            // Copy segment coordinates out first (borrow ends here) so
            // image reads below can borrow self mutably.
            let (kind, start, end, physical, blocks, algorithm) =
                match self.segments.get(self.seg_idx) {
                    None => {
                        buf[done..].fill(0);
                        return Ok(());
                    }
                    Some(seg) => (
                        seg.kind,
                        seg.start,
                        seg.end,
                        seg.physical,
                        seg.compressed_blocks,
                        seg.algorithm,
                    ),
                };
            if start > pos as u64 {
                // Gap before the segment: zeros (should not happen in
                // well-formed images, but never emit garbage).
                let n = ((start - pos as u64) as usize).min(buf.len() - done);
                buf[done..done + n].fill(0);
                done += n;
                continue;
            }
            let n = ((end - pos as u64) as usize).min(buf.len() - done);
            if kind == 0 {
                self.source.reader.read_at(
                    physical
                        .checked_mul(self.source.block_size)
                        .and_then(|b| b.checked_add(pos as u64 - start))
                        .ok_or_else(|| Error::invalid("EROFS block offset overflow"))?,
                    &mut buf[done..done + n],
                )?;
            } else {
                if self.decoded_idx != self.seg_idx {
                    let seg_len = (end - start) as usize;
                    self.decoded.clear();
                    self.decoded.resize(seg_len, 0);
                    let resolved = crate::fs::erofs::compress::Segment {
                        kind,
                        start,
                        end,
                        physical,
                        compressed_blocks: blocks,
                        algorithm,
                    };
                    let got = crate::fs::erofs::compress::read_segment(
                        &fs_sb,
                        &mut self.source.reader,
                        &resolved,
                        &mut self.decoded,
                    )?;
                    if got != seg_len {
                        return Err(Error::invalid("short EROFS segment decode"));
                    }
                    self.decoded_idx = self.seg_idx;
                }
                let from = (pos as u64 - start) as usize;
                buf[done..done + n].copy_from_slice(&self.decoded[from..from + n]);
            }
            done += n;
        }
        Ok(())
    }
}

fn slice_serve(data: &[u8], offset: usize, buf: &mut [u8]) {
    let end = (offset + buf.len()).min(data.len());
    if offset < data.len() {
        buf[..end - offset].copy_from_slice(&data[offset..end]);
        buf[end - offset..].fill(0);
    } else {
        buf.fill(0);
    }
}

/// ext4 has no inline-symlink concept for big targets: preload every
/// symlink target into memory (targets are tiny).
fn preload_symlinks(source: &mut ErofsSource, node: &mut Node) -> Result<(), Error> {
    if !node.is_dir()
        && node.mode & 0xf000 == SYMLINK_MODE
        && matches!(node.payload, NodePayload::None)
    {
        node.payload = NodePayload::Memory(source.data(node.original_nid)?.3);
    }
    for child in node.children.iter_mut() {
        preload_symlinks(source, child)?;
    }
    Ok(())
}

fn erofs_to_ext4(
    input: &Path,
    output: &Path,
    sparse_output: bool,
    shared_blocks: bool,
    reserve_mb: u64,
) -> Result<(), Error> {
    let mut source = ErofsSource::open(input)?;
    let block_size = source.block_size;
    if block_size < 1024 || block_size > 65536 {
        return Err(Error::invalid("unsupported block size for ext4 conversion"));
    }
    let mut root = source.tree(source.root_nid, String::new())?;
    preload_symlinks(&mut source, &mut root)?;
    let profile = crate::fs::ext4::Superblock {
        block_size,
        inode_size: 256,
        block_count: 0,
        blocks_per_group: 32768,
        inodes_per_group: 1024,
        group_descriptor_size: 32,
        compat: 0,
        incompat: 0x40 | 0x2, // EXTENTS | FILETYPE
        ro_compat: if shared_blocks { 0x4000 } else { 0 },
    };
    let mut adapter = ErofsBlockSource {
        source: &mut source,
        file_nid: u64::MAX,
        segments: Vec::new(),
        seg_idx: 0,
        decoded_idx: usize::MAX,
        decoded: Vec::new(),
    };
    ext4::run_converted(
        &profile,
        &mut adapter,
        &mut root,
        output,
        sparse_output,
        shared_blocks,
        reserve_mb,
    )
}

// ── ext4 -> EROFS ────────────────────────────────────────────────────

/// Fresh EROFS image from an ext4 tree. With `algo == None` every file
/// is stored all-plain; otherwise files stream through the
/// compressed-compact writer (parallel, bounded RAM).
fn ext4_to_erofs(
    input: &Path,
    output: &Path,
    sparse_output: bool,
    algo: crate::fs::erofs::compress::Algo,
) -> Result<(), Error> {
    let mut source = ext4::Source::open(input)?;
    let block_size = usize::try_from(source.profile.block_size)
        .map_err(|_| Error::invalid("invalid ext4 block size"))? as u64;
    let mut root = source.tree(2, String::new())?;
    // Special files (sockets, devices, fifos) have no EROFS encoding here.
    let skipped = prune_specials(&mut root);
    if skipped > 0 {
        eprintln!("warning: skipping {skipped} special files (sockets/devices/fifos)");
    }

    sort_tree(&mut root);
    if algo.is_some() {
        return ext4_to_erofs_compressed(source, root, output, sparse_output, algo, block_size);
    }
    // Shared SELinux table: contexts travel with the files that carry them.
    let contexts = collect_contexts(&root);
    let (xattr_table, xattr_ids) = if contexts.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        crate::fs::erofs::xattr::pack_shared_table(&contexts)
    };
    let block_usz = block_size as usize;
    let meta_blkaddr = 1u64; // block 0 stays reserved for the superblock
    let mut next = 1u64;
    assign_nids_strided(&mut root, &mut next, 1, meta_blkaddr, block_size);
    let mut list = Vec::new();
    nodes(&root, &mut list);
    let inode_blocks = (next as usize * 32).div_ceil(block_usz);
    let mut alloc_block = meta_blkaddr + inode_blocks as u64;
    let xattr_blkaddr = if xattr_table.is_empty() {
        0
    } else {
        let base = alloc_block;
        alloc_block += xattr_table.len().div_ceil(block_usz) as u64;
        base
    };

    // Directory data first (offsets must be known before inode planning).
    let mut dir_data: Vec<(u64, Vec<u8>, u64)> = Vec::new();
    for node in &list {
        if node.is_dir() {
            let data = directory_bytes(node, block_usz);
            if data.is_empty() {
                return Err(Error::invalid("directory does not fit into blocks"));
            }
            let blocks = data.len().div_ceil(block_usz) as u64;
            dir_data.push((node.nid, data, alloc_block));
            alloc_block += blocks;
        }
    }
    struct FileJob {
        nid: u64,
        destination: u64,
        size: u64,
    }
    let mut jobs: Vec<FileJob> = Vec::new();
    // (offset, 32-byte inode image)
    let mut inodes: Vec<(u64, Vec<u8>)> = Vec::new();
    for node in &list {
        let (size, data_block) = if node.is_dir() {
            let (data, start) = dir_data
                .iter()
                .find(|entry| entry.0 == node.nid)
                .map(|entry| (entry.1.len() as u64, entry.2))
                .ok_or_else(|| Error::invalid("directory data is missing"))?;
            (data, start)
        } else if node.data_size == 0 {
            (0, 0)
        } else {
            let start = alloc_block;
            alloc_block += node.data_size.div_ceil(block_size);
            (node.data_size, start)
        };
        let mut inode = vec![0u8; 32];
        inode[4..6].copy_from_slice(&node.mode.to_le_bytes());
        inode[6..8].copy_from_slice(&1u16.to_le_bytes());
        inode[8..12].copy_from_slice(&(size as u32).to_le_bytes());
        inode[16..20].copy_from_slice(&(data_block as u32).to_le_bytes());
        inode[20..24].copy_from_slice(&(node.nid as u32).to_le_bytes());
        inode[24..26].copy_from_slice(&(node.uid as u16).to_le_bytes());
        inode[26..28].copy_from_slice(&(node.gid as u16).to_le_bytes());
        if let Some(ctx) = node.context.as_deref().filter(|c| !c.is_empty()) {
            let id = xattr_ids
                .get(
                    contexts
                        .iter()
                        .position(|c| c == ctx)
                        .ok_or_else(|| Error::invalid("SELinux context missing from table"))?,
                )
                .copied()
                .ok_or_else(|| Error::invalid("SELinux context missing from table"))?;
            inode[2..4].copy_from_slice(&2u16.to_le_bytes());
            inode.extend_from_slice(&crate::fs::erofs::xattr::area_bytes(&[id]));
        }
        inodes.push((meta_blkaddr * block_size + node.nid * 32, inode));
        if !node.is_dir() && size > 0 {
            jobs.push(FileJob {
                nid: node.nid,
                destination: data_block * block_size,
                size,
            });
        }
    }
    let total_size = alloc_block * block_size;
    let build_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Materialize the raw image into a file (final path when possible),
    // then optionally sparsify / stream to stdout.
    enum RawTarget {
        Direct(PathBuf),
        Temp(PathBuf),
    }
    let to_stdout = output == Path::new("-");
    let raw_target = if !to_stdout && !sparse_output {
        RawTarget::Direct(output.to_path_buf())
    } else {
        let (_tmp, path) = core_image::spool_temp("image-worker-convert")?;
        drop(_tmp);
        RawTarget::Temp(path)
    };
    let raw_path = match &raw_target {
        RawTarget::Direct(p) | RawTarget::Temp(p) => p.clone(),
    };
    {
        let mut out = File::create(&raw_path)?;
        out.set_len(total_size)?;
        // Superblock (fresh, all-plain: no incompat features).
        let mut sb = [0u8; 128];
        sb[0..4].copy_from_slice(&0xe0f5_e1e2u32.to_le_bytes());
        sb[12] = block_size.trailing_zeros() as u8;
        sb[14..16].copy_from_slice(&(root.nid as u16).to_le_bytes());
        sb[16..24].copy_from_slice(&(list.len() as u64).to_le_bytes());
        sb[24..32].copy_from_slice(&build_time.to_le_bytes());
        sb[36..40].copy_from_slice(&(alloc_block as u32).to_le_bytes());
        sb[40..44].copy_from_slice(&(meta_blkaddr as u32).to_le_bytes());
        if !xattr_table.is_empty() {
            sb[44..48].copy_from_slice(&(xattr_blkaddr as u32).to_le_bytes());
        }
        out.seek(SeekFrom::Start(1024))?;
        out.write_all(&sb)?;
        if !xattr_table.is_empty() {
            out.seek(SeekFrom::Start(xattr_blkaddr * block_size))?;
            out.write_all(&xattr_table)?;
        }
        for (off, inode) in &inodes {
            out.seek(SeekFrom::Start(*off))?;
            out.write_all(inode)?;
        }
        for (_, data, start) in &dir_data {
            out.seek(SeekFrom::Start(*start * block_size))?;
            out.write_all(data)?;
        }
        // File payloads stream through 64 KiB chunks: O(1) RAM.
        let by_nid: std::collections::HashMap<u64, &Node> =
            list.iter().map(|n| (n.nid, *n)).collect();
        let mut buf = vec![0u8; 64 * 1024];
        for job in &jobs {
            let node = by_nid.get(&job.nid).copied().ok_or_else(|| {
                Error::invalid("file node vanished during conversion")
            })?;
            let mut remaining = job.size;
            let mut src_off = 0u64;
            let mut dst_off = job.destination;
            while remaining > 0 {
                let n = (remaining.min(buf.len() as u64)) as usize;
                source.read_node_block(node, src_off as usize, &mut buf[..n])?;
                out.seek(SeekFrom::Start(dst_off))?;
                out.write_all(&buf[..n])?;
                src_off += n as u64;
                dst_off += n as u64;
                remaining -= n as u64;
            }
        }
        out.flush()?;
    }
    match raw_target {
        RawTarget::Direct(_) => Ok(()),
        RawTarget::Temp(path) => {
            let result = if sparse_output {
                if to_stdout {
                    core_image::sparsify_file_to_stdout(&path, block_usz)
                } else {
                    // Re-encode into the final path, then drop the temp raw.
                    let mut raw = File::open(&path)?;
                    let (runs, blocks) = core_image::scan_sparse_runs(&mut raw, block_usz)?;
                    let mut out = File::create(output)?;
                    let mut header = [0u8; 28];
                    header[0..4].copy_from_slice(&core_image::SPARSE_MAGIC.to_le_bytes());
                    header[4..6].copy_from_slice(&1u16.to_le_bytes());
                    header[8..10].copy_from_slice(&28u16.to_le_bytes());
                    header[10..12].copy_from_slice(&12u16.to_le_bytes());
                    header[12..16].copy_from_slice(&(block_usz as u32).to_le_bytes());
                    header[16..20].copy_from_slice(&(blocks as u32).to_le_bytes());
                    header[20..24].copy_from_slice(&(runs.len() as u32).to_le_bytes());
                    out.write_all(&header)?;
                    core_image::emit_sparse_runs(&mut raw, block_usz, &runs, &mut out)?;
                    out.flush()?;
                    Ok(())
                }
            } else {
                // Raw to stdout.
                let mut raw = File::open(&path)?;
                let stdout = std::io::stdout();
                let mut stream = stdout.lock();
                let mut buf = vec![0u8; 1024 * 1024];
                loop {
                    let n = raw.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n])?;
                }
                stream.flush()?;
                Ok(())
            };
            let _ = std::fs::remove_file(&path);
            result
        }
    }
}

/// Drop special files (anything but regular/dir/symlink); EROFS output
/// cannot encode them. Returns the dropped count.
fn prune_specials(node: &mut Node) -> usize {
    let mut dropped = 0usize;
    node.children.retain(|child| {
        let keep = matches!(child.mode & 0xf000, 0x8000 | 0x4000 | 0xa000 | 0x0000);
        if !keep {
            dropped += 1;
        }
        keep
    });
    for child in node.children.iter_mut() {
        dropped += prune_specials(child);
    }
    dropped
}

// ── ext4 -> EROFS with compression ───────────────────────────────────
// Same fresh layout as `rebuild_compressed` (see super): metadata with
// reserved index space first, payloads streamed compactly through
// cluster-pair batches (rayon-parallel, order-preserving, ≤ ~512 KiB
// per batch), inodes/index/superblock seek-written as inputs finalize.

fn ext4_to_erofs_compressed(
    mut source: ext4::Source,
    mut root: Node,
    output: &Path,
    sparse_output: bool,
    algo: crate::fs::erofs::compress::Algo,
    block_size: u64,
) -> Result<(), Error> {
    use crate::fs::erofs::compress as cmp;
    sort_tree(&mut root);
    let block_usz = block_size as usize;
    mark_compressed(&mut root, block_size);
    let contexts = collect_contexts(&root);
    let (xattr_table, xattr_ids) = if contexts.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        crate::fs::erofs::xattr::pack_shared_table(&contexts)
    };
    let meta_blkaddr = 1u64;
    let mut next = 1u64;
    assign_nids_strided(&mut root, &mut next, 1, meta_blkaddr, block_size);
    let mut list = Vec::new();
    nodes(&root, &mut list);
    let inode_blocks = (next as usize * 32).div_ceil(block_usz);
    let mut alloc_block = meta_blkaddr + inode_blocks as u64;
    let xattr_blkaddr = if xattr_table.is_empty() {
        0
    } else {
        let base = alloc_block;
        alloc_block += xattr_table.len().div_ceil(block_usz) as u64;
        base
    };
    let mut dir_data: Vec<(u64, Vec<u8>, u64)> = Vec::new();
    for node in &list {
        if node.is_dir() {
            let data = directory_bytes(node, block_usz);
            if data.is_empty() {
                return Err(Error::invalid("directory does not fit into blocks"));
            }
            let blocks = data.len().div_ceil(block_usz) as u64;
            dir_data.push((node.nid, data, alloc_block));
            alloc_block += blocks;
        }
    }
    struct PendingFile {
        nid: u64,
        size: u64,
        clusters: u64,
        data_off: u64,
        index_off: u64,
    }
    let mut inodes: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut pending: Vec<PendingFile> = Vec::new();
    for node in &list {
        let inode_off = meta_blkaddr * block_size + node.nid * 32;
        let (size, data_block, is_compressed) = if node.is_dir() {
            let (len, start) = dir_data
                .iter()
                .find(|entry| entry.0 == node.nid)
                .map(|entry| (entry.1.len() as u64, entry.2))
                .ok_or_else(|| Error::invalid("directory data is missing"))?;
            (len, start as u32, false)
        } else if node.erofs_compact_clusters > 0 {
            (node.data_size, 0, true)
        } else {
            (node.data_size, 0, false)
        };
        let size32 = u32::try_from(size)
            .map_err(|_| Error::invalid("file is too large for a compact inode"))?;
        let mut inode = vec![0u8; 32];
        if is_compressed {
            inode[0..2].copy_from_slice(&6u16.to_le_bytes());
        }
        inode[4..6].copy_from_slice(&node.mode.to_le_bytes());
        inode[6..8].copy_from_slice(&1u16.to_le_bytes());
        inode[8..12].copy_from_slice(&size32.to_le_bytes());
        inode[16..20].copy_from_slice(&data_block.to_le_bytes());
        inode[20..24].copy_from_slice(&(node.nid as u32).to_le_bytes());
        inode[24..26].copy_from_slice(&(node.uid as u16).to_le_bytes());
        inode[26..28].copy_from_slice(&(node.gid as u16).to_le_bytes());
        let mut area = Vec::new();
        if let Some(ctx) = node.context.as_deref().filter(|c| !c.is_empty()) {
            let id = xattr_ids
                .get(
                    contexts
                        .iter()
                        .position(|c| c == ctx)
                        .ok_or_else(|| Error::invalid("SELinux context missing from table"))?,
                )
                .copied()
                .ok_or_else(|| Error::invalid("SELinux context missing from table"))?;
            inode[2..4].copy_from_slice(&2u16.to_le_bytes());
            area = crate::fs::erofs::xattr::area_bytes(&[id]);
            inode.extend_from_slice(&area);
        }
        inodes.push((inode_off, inode));
        if node.is_dir() || !is_compressed {
            continue;
        }
        let data_off = (inode_off + 32 + area.len() as u64 + 7) & !7;
        pending.push(PendingFile {
            nid: node.nid,
            size,
            clusters: node.erofs_compact_clusters,
            data_off,
            index_off: data_off + 8,
        });
    }
    let to_stdout = output == Path::new("-");
    enum RawTarget {
        Direct(PathBuf),
        Temp(PathBuf),
    }
    let raw_target = if !to_stdout && !sparse_output {
        RawTarget::Direct(output.to_path_buf())
    } else {
        let (_tmp, path) = core_image::spool_temp("image-worker-convert-cmp")?;
        drop(_tmp);
        RawTarget::Temp(path)
    };
    let raw_path = match &raw_target {
        RawTarget::Direct(p) | RawTarget::Temp(p) => p.clone(),
    };
    {
        let mut out = File::create(&raw_path)?;
        let mut sb = [0u8; 128];
        sb[0..4].copy_from_slice(&0xe0f5_e1e2u32.to_le_bytes());
        sb[12] = block_size.trailing_zeros() as u8;
        sb[14..16].copy_from_slice(&(root.nid as u16).to_le_bytes());
        sb[16..24].copy_from_slice(&(list.len() as u64).to_le_bytes());
        out.seek(SeekFrom::Start(1024))?;
        out.write_all(&sb)?;
        if !xattr_table.is_empty() {
            out.seek(SeekFrom::Start(xattr_blkaddr * block_size))?;
            out.write_all(&xattr_table)?;
        }
        for (off, inode) in &inodes {
            out.seek(SeekFrom::Start(*off))?;
            out.write_all(inode)?;
        }
        for (_, data, start) in &dir_data {
            out.seek(SeekFrom::Start(*start * block_size))?;
            out.write_all(data)?;
        }
        let by_nid: HashMap<u64, &Node> = list.iter().map(|n| (n.nid, *n)).collect();
        let mut cursor = alloc_block * block_size;
        for file in pending.iter_mut() {
            let node = by_nid.get(&file.nid).copied().ok_or_else(|| {
                Error::invalid("file node vanished during conversion")
            })?;
            out.seek(SeekFrom::Start(file.data_off))?;
            out.write_all(&cmp::map_header(algo))?;
            let mut index_off = file.index_off;
            // Cluster reader over the ext4 block source (holes read as
            // zeros, exactly like the plain converter's 64 KiB path).
            let mut reader = |idx: u64, buf: &mut [u8]| -> Result<(), Error> {
                let off = idx
                    .checked_mul(block_usz as u64)
                    .and_then(|base| base.checked_add(0))
                    .ok_or_else(|| Error::invalid("ext4 cluster offset overflow"))?;
                if matches!(node.payload, NodePayload::Memory(_)) {
                    let data = node.data();
                    let end = off as usize + buf.len();
                    if end > data.len() {
                        return Err(Error::invalid("file node data is too short"));
                    }
                    buf.copy_from_slice(&data[off as usize..end]);
                    return Ok(());
                }
                source.read_node_block(node, off as usize, buf)?;
                Ok(())
            };
            stream_compressed_file(
                &mut out,
                file.size,
                block_usz,
                algo,
                &mut cursor,
                &mut index_off,
                &mut reader,
            )?;
        }
        for file in &pending {
            let have = file.data_off + 8 + cmp::index_len(file.clusters) as u64;
            if file.index_off > have {
                return Err(Error::invalid("compressed index overran its reservation"));
            }
        }
        let total_blocks = cursor.div_ceil(block_size);
        let build_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.seek(SeekFrom::Start(1024))?;
        let mut sb = [0u8; 128];
        sb[0..4].copy_from_slice(&0xe0f5_e1e2u32.to_le_bytes());
        sb[12] = block_size.trailing_zeros() as u8;
        sb[14..16].copy_from_slice(&(root.nid as u16).to_le_bytes());
        sb[16..24].copy_from_slice(&(list.len() as u64).to_le_bytes());
        sb[24..32].copy_from_slice(&build_time.to_le_bytes());
        sb[36..40].copy_from_slice(&(total_blocks as u32).to_le_bytes());
        sb[40..44].copy_from_slice(&(meta_blkaddr as u32).to_le_bytes());
        if !xattr_table.is_empty() {
            sb[44..48].copy_from_slice(&(xattr_blkaddr as u32).to_le_bytes());
        }
        out.write_all(&sb)?;
        out.seek(SeekFrom::Start(cursor))?;
        out.set_len(cursor)?;
        out.flush()?;
    }
    match raw_target {
        RawTarget::Direct(_) => {
            if sparse_output {
                let mut raw = File::open(&raw_path)?;
                let (runs, blocks) = core_image::scan_sparse_runs(&mut raw, block_usz)?;
                let mut sparse = File::create(output)?;
                let mut header = [0u8; 28];
                header[0..4].copy_from_slice(&core_image::SPARSE_MAGIC.to_le_bytes());
                header[4..6].copy_from_slice(&1u16.to_le_bytes());
                header[8..10].copy_from_slice(&28u16.to_le_bytes());
                header[10..12].copy_from_slice(&12u16.to_le_bytes());
                header[12..16].copy_from_slice(&(block_usz as u32).to_le_bytes());
                header[16..20].copy_from_slice(&(blocks as u32).to_le_bytes());
                header[20..24].copy_from_slice(&(runs.len() as u32).to_le_bytes());
                sparse.write_all(&header)?;
                core_image::emit_sparse_runs(&mut raw, block_usz, &runs, &mut sparse)?;
                sparse.flush()?;
            }
            Ok(())
        }
        RawTarget::Temp(path) => {
            let result = if sparse_output {
                if to_stdout {
                    core_image::sparsify_file_to_stdout(&path, block_usz)
                } else {
                    let mut raw = File::open(&path)?;
                    let (runs, blocks) = core_image::scan_sparse_runs(&mut raw, block_usz)?;
                    let mut sparse = File::create(output)?;
                    let mut header = [0u8; 28];
                    header[0..4].copy_from_slice(&core_image::SPARSE_MAGIC.to_le_bytes());
                    header[4..6].copy_from_slice(&1u16.to_le_bytes());
                    header[8..10].copy_from_slice(&28u16.to_le_bytes());
                    header[10..12].copy_from_slice(&12u16.to_le_bytes());
                    header[12..16].copy_from_slice(&(block_usz as u32).to_le_bytes());
                    header[16..20].copy_from_slice(&(blocks as u32).to_le_bytes());
                    header[20..24].copy_from_slice(&(runs.len() as u32).to_le_bytes());
                    sparse.write_all(&header)?;
                    core_image::emit_sparse_runs(&mut raw, block_usz, &runs, &mut sparse)?;
                    sparse.flush()?;
                    Ok(())
                }
            } else {
                let mut raw = File::open(&path)?;
                let stdout = std::io::stdout();
                let mut stream = stdout.lock();
                let mut buf = vec![0u8; 1024 * 1024];
                loop {
                    let n = raw.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n])?;
                }
                stream.flush()?;
                Ok(())
            };
            let _ = std::fs::remove_file(&path);
            result
        }
    }
}
