use super::{Action, Node, NodePayload, apply_action};
use crate::core::{Error, Image, buf, image as core_image};
use crate::fs::ext4 as fs_ext4;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: u16 = 0xef53;
const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_RESIZE_INODE: u32 = 0x10;
const RO_GDT_CSUM: u32 = 0x10;
const EXTENTS: u32 = 0x40;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_ENCRYPT: u32 = 0x10000;
const RO_METADATA_CSUM: u32 = 0x400;
const RO_SHARED_BLOCKS: u32 = 0x4000;
const INODE_EXTENTS: u32 = 0x80000;
const EXTENT_MAGIC: u16 = 0xf30a;
const SYMLINK: u16 = 0xa000;

pub(crate) struct FileWriter {
    file: File,
}
impl FileWriter {
    fn new(file: File) -> Self {
        Self { file }
    }
    fn put32(&mut self, offset: usize, value: u32) -> Result<(), Error> {
        self.file.seek(SeekFrom::Start(offset as u64))?;
        self.file.write_all(&value.to_le_bytes())?;
        Ok(())
    }
    fn put16(&mut self, offset: usize, value: u32) -> Result<(), Error> {
        self.file.seek(SeekFrom::Start(offset as u64))?;
        self.file.write_all(&(value as u16).to_le_bytes())?;
        Ok(())
    }
    fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<(), Error> {
        self.file.seek(SeekFrom::Start(offset as u64))?;
        self.file.write_all(data)?;
        Ok(())
    }
    fn or_byte(&mut self, offset: usize, value: u8) -> Result<(), Error> {
        let mut buf = [0u8; 1];
        self.file.seek(SeekFrom::Start(offset as u64))?;
        self.file.read_exact(&mut buf)?;
        buf[0] |= value;
        self.file.seek(SeekFrom::Start(offset as u64))?;
        self.file.write_all(&buf)?;
        Ok(())
    }
}

pub(crate) struct Source {
    reader: Image,
    pub(crate) profile: fs_ext4::Superblock,
    inodes_per_group: u32,
    group_count: u32,
}

impl Source {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let mut reader = Image::open(path)?;
        if !fs_ext4::probe(&mut reader)? {
            return Err(Error::Invalid("input is not ext4".into()));
        }
        let profile = fs_ext4::Superblock::read(&mut reader)?;
        if profile.inode_size < 128 || !profile.inode_size.is_multiple_of(4) {
            return Err(Error::Invalid("invalid ext4 inode size".into()));
        }
        if profile.compat & COMPAT_HAS_JOURNAL != 0 {
            return Err(Error::Invalid(
                "ext4 journal feature is unsupported by the standalone rebuild".into(),
            ));
        }
        if profile.incompat & !(EXTENTS | INCOMPAT_FILETYPE) != 0
            || profile.incompat & (INCOMPAT_64BIT | INCOMPAT_ENCRYPT) != 0
            || profile.ro_compat & RO_METADATA_CSUM != 0
        {
            return Err(Error::Invalid(format!(
                "unsupported ext4 features: compat=0x{:x} incompat=0x{:x} ro_compat=0x{:x}; metadata_csum, 64bit, encryption and unknown features are rejected",
                profile.compat, profile.incompat, profile.ro_compat
            )));
        }
        let mut sb = [0u8; 1024];
        reader.read_at(1024, &mut sb)?;
        let blocks_per_group = buf::u32le(&sb, 32)?;
        let group_count = profile.block_count.div_ceil(u64::from(blocks_per_group));
        if buf::u16le(&sb, 254)?.max(32) != 32 {
            return Err(Error::Invalid(
                "ext4 backend requires classic 32-byte group descriptors".into(),
            ));
        }
        Ok(Self {
            reader,
            profile,
            inodes_per_group: buf::u32le(&sb, 40)?,
            group_count: u32::try_from(group_count)
                .map_err(|_| Error::Invalid("ext4 group count is too large".into()))?,
        })
    }
    fn inode(&mut self, number: u32) -> Result<fs_ext4::inode::Ext4Inode, Error> {
        // Inode geometry/validation lives in crate::fs; the writer only
        // enforces its own supported subset (checked in open()).
        if number == 0 || number > self.inodes_per_group * self.group_count {
            return Err(Error::Invalid(format!(
                "ext4 inode {number} is out of range"
            )));
        }
        fs_ext4::inode::read(&self.profile, &mut self.reader, number)
    }
    fn data(&mut self, inode: &fs_ext4::inode::Ext4Inode) -> Result<Vec<u8>, Error> {
        // Single implementation lives in crate::fs (holes, inline
        // symlinks, extent walking with bounds checks).
        fs_ext4::inode::read_data(&self.profile, &mut self.reader, inode)
    }
    pub(crate) fn tree(&mut self, number: u32, name: String) -> Result<Node, Error> {
        self.tree_depth(number, name, 0)
    }

    /// Recursive worker with a depth cap: real trees are dozens deep;
    /// unbounded recursion on corrupt/cyclic data would exhaust the stack.
    fn tree_depth(&mut self, number: u32, name: String, depth: u32) -> Result<Node, Error> {
        if depth > 512 {
            return Err(Error::invalid("directory nesting is too deep"));
        }
        let inode = self.inode(number)?;
        let is_dir = inode.mode & 0xf000 == 0x4000;
        let has_extents = inode.block.len() >= 4 && buf::u16le(&inode.block, 0)? == EXTENT_MAGIC;
        let mut extents = Vec::new();
        if has_extents {
            let mut collected = Vec::new();
            fs_ext4::inode::collect_extents(
                &self.profile,
                &mut self.reader,
                &inode.block,
                buf::u16le(&inode.block, 6)?,
                usize::from(buf::u16le(&inode.block, 2)?),
                &mut collected,
            )?;
            extents = collected
                .into_iter()
                .map(|e| (e.logical_block, e.physical_block, e.length))
                .collect();
        }
        let (payload, data_size) = if is_dir {
            let data = self.data(&inode)?;
            let size = data.len() as u64;
            (NodePayload::Memory(data), size)
        } else if has_extents {
            (NodePayload::None, inode.size)
        } else {
            let data = self.data(&inode)?;
            let size = data.len() as u64;
            (NodePayload::Memory(data), size)
        };
        let mut node = Node {
            nid: u64::from(number),
            original_nid: u64::from(number),
            parent_nid: 0,
            name,
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            payload,
            data_size,
            children: Vec::new(),
            layout: 0,
            raw_block: 0,
            modified: false,
            content_changed: false,
            context: inode.context,
            context_explicit: false,
            erofs_compact_clusters: 0,
            extents,
        };
        if is_dir {
            // Shared directory parser (see crate::fs); skips "." / "..".
            for entry in fs_ext4::dir::read(&self.profile, &mut self.reader, number)? {
                if entry.name != "." && entry.name != ".." {
                        let mut child_node = self.tree_depth(entry.inode, entry.name, depth + 1)?;
                    child_node.parent_nid = u64::from(number);
                    node.children.push(child_node);
                }
            }
        }
        Ok(node)
    }
}

pub(crate) fn run(
    input: &Path,
    output: &Path,
    sparse: bool,
    shared_blocks: Option<bool>,
    reserve_mb: u64,
    compact: bool,
    actions: Vec<Action>,
) -> Result<(), Error> {
    let mut source = Source::open(input)?;
    let use_shared_blocks = shared_blocks.unwrap_or(
        source.profile.ro_compat & RO_SHARED_BLOCKS != 0
    );
    let mut root = source.tree(2, String::new())?;
    for action in actions {
        apply_action(&mut root, action)?;
    }
    rebuild(&mut source, &mut root, output, sparse, use_shared_blocks, reserve_mb, compact)
}
fn assign(node: &mut Node, nid: u64, parent: u64) {
    node.nid = nid;
    node.parent_nid = if parent == 0 { nid } else { parent };
    let mut next = if parent == 0 { 11 } else { nid + 1 };
    for child in &mut node.children {
        assign(child, next, nid);
        next += count(child);
    }
}
fn count(node: &Node) -> u64 {
    1 + node.children.iter().map(count).sum::<u64>()
}
fn collect<'a>(node: &'a Node, result: &mut Vec<&'a Node>) {
    result.push(node);
    for child in &node.children {
        collect(child, result);
    }
}
fn directory(node: &Node, block: usize) -> Result<Vec<u8>, Error> {
    let mut result = Vec::new();
    let mut previous_start = None;
    let mut entries = vec![
        (node.nid as u32, ".", 2u8),
        (node.parent_nid as u32, "..", 2u8),
    ];
    entries.extend(
        node.children
            .iter()
            .map(|child| (child.nid as u32, child.name.as_str(), file_type(child.mode))),
    );
    for (index, (inode, name, kind)) in entries.iter().enumerate() {
        let minimum = (8 + name.len()).div_ceil(4) * 4;
        let used = result.len() % block;
        if used != 0 && used + minimum > block {
            let padding = block - used;
            let start = previous_start.ok_or_else(|| {
                Error::Invalid("ext4 directory has an invalid first entry".into())
            })?;
            let length = buf::u16le(&result, start + 4)?;
            result[start + 4..start + 6]
                .copy_from_slice(&length.saturating_add(padding as u16).to_le_bytes());
            result.resize(result.len() + padding, 0);
        }
        let length = if index + 1 == entries.len() {
            block - result.len() % block
        } else {
            minimum
        };
        let start = result.len();
        result.resize(start + length, 0);
        result[start..start + 4].copy_from_slice(&inode.to_le_bytes());
        result[start + 4..start + 6].copy_from_slice(&(length as u16).to_le_bytes());
        result[start + 6] = name.len() as u8;
        result[start + 7] = *kind;
        result[start + 8..start + 8 + name.len()].copy_from_slice(name.as_bytes());
        previous_start = Some(start);
    }
    Ok(result)
}
fn file_type(mode: u16) -> u8 {
    match mode & 0xf000 {
        0x4000 => 2,
        0xa000 => 7,
        0x2000 => 5,
        0x6000 => 6,
        0x1000 => 3,
        0xc000 => 4,
        _ => 1,
    }
}
fn rebuild(
    source: &mut Source,
    root: &mut Node,
    output: &Path,
    sparse: bool,
    shared_blocks: bool,
    reserve_mb: u64,
    compact: bool,
) -> Result<(), Error> {
    let profile = source.profile.clone();
    rebuild_generic(
        source,
        &profile,
        root,
        output,
        sparse,
        shared_blocks,
        reserve_mb,
        true,
        compact,
    )
}

/// Converted-tree entry point (e.g. EROFS tree via an adapter): no input
/// size to preserve, so the plan defines the size from scratch.
pub(crate) fn run_converted(
    profile: &fs_ext4::Superblock,
    source: &mut dyn BlockSource,
    root: &mut Node,
    output: &Path,
    sparse: bool,
    shared_blocks: bool,
    reserve_mb: u64,
) -> Result<(), Error> {
    rebuild_generic(source, profile, root, output, sparse, shared_blocks, reserve_mb, false, false)
}

#[allow(clippy::too_many_arguments)]
fn rebuild_generic(
    source: &mut dyn BlockSource,
    profile: &fs_ext4::Superblock,
    root: &mut Node,
    output: &Path,
    sparse: bool,
    shared_blocks: bool,
    reserve_mb: u64,
    keep_input_size: bool,
    compact: bool,
) -> Result<(), Error> {
    assign(root, 2, 0);
    let block = usize::try_from(profile.block_size)
        .map_err(|_| Error::Invalid("invalid ext4 block size".into()))?;
    // Full vendor images (0% free is common) cannot fit a fresh copy-on-write
    // layout at the old size: estimate demand, grow by whole groups, retry.
    // With --reserve-mb the output additionally guarantees that much free
    // space on top of the packed content.
    let reserve_blocks = reserve_mb
        .saturating_mul(1024 * 1024)
        .div_ceil(profile.block_size);
    // Dedup-aware estimate only when it can shrink the plan (compact +
    // shared): otherwise the plain worst case is cheaper (no extra pass).
    let mut total_blocks = plan_total_blocks(
        profile,
        root,
        block,
        reserve_blocks,
        source,
        shared_blocks && compact,
    )?;
    // Never shrink real inputs unless compacting: flashing targets fixed
    // partitions, and the old blocks already exist. Conversions size from
    // scratch instead.
    if keep_input_size && !compact {
        total_blocks = total_blocks.max(profile.block_count);
    }
    if output == Path::new("-") {
        let (_tmp, tmp_path) = core_image::spool_temp("image-worker-ext4")?;
        let result = build_grown(source, profile, root, &tmp_path, shared_blocks, block, total_blocks);
        let stream_result = result.and_then(|raw_blocks| {
            stream_temp_to_stdout(&tmp_path, block, raw_blocks, sparse)
        });
        let _ = std::fs::remove_file(&tmp_path);
        return stream_result;
    }
    build_grown(source, profile, root, output, shared_blocks, block, total_blocks)?;
    if sparse {
        core_image::sparsify_file_in_place(output, block)?;
    }
    Ok(())
}

/// Grow-and-retry loop around the raw builder. The allocator is
/// deterministic, so a retry after growth converges.
fn build_grown(
    source: &mut dyn BlockSource,
    profile: &fs_ext4::Superblock,
    root: &mut Node,
    output: &Path,
    shared_blocks: bool,
    block: usize,
    mut total_blocks: u64,
) -> Result<u64, Error> {
    // Generous cap: 4x the input size (same-fs) or 2x the plan + slack
    // (conversions size from a zero seed), always within u32 blocks.
    const MAX_BLOCKS: u64 = u32::MAX as u64;
    let cap = profile
        .block_count
        .saturating_mul(4)
        .max(total_blocks.saturating_mul(2).saturating_add(65536))
        .min(MAX_BLOCKS);
    let bpg = u64::from(profile.blocks_per_group);
    let mut last_error = None;
    for _ in 0..16 {
        match rebuild_groups_to(source, profile, root, output, shared_blocks, block, total_blocks)
        {
            Ok(()) => return Ok(total_blocks),
            Err(Error::Invalid(message)) if message.contains("no free") && total_blocks < cap => {
                last_error = Some(message);
                total_blocks += bpg;
            }
            Err(other) => return Err(other),
        }
    }
    Err(Error::invalid(format!(
        "ext4 image cannot be rebuilt even after growth: {}",
        last_error.unwrap_or_else(|| "out of space".into())
    )))
}

/// Stream a raw temp image to stdout (raw or sparse container).
fn stream_temp_to_stdout(
    tmp: &Path,
    block: usize,
    _raw_blocks: u64,
    sparse: bool,
) -> Result<(), Error> {
    if sparse {
        return core_image::sparsify_file_to_stdout(tmp, block);
    }
    let mut file = File::open(tmp)?;
    let stdout = std::io::stdout();
    let mut stream = stdout.lock();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n])?;
    }
    stream.flush()?;
    Ok(())
}

/// Estimate the blocks a fresh layout needs (plain-layout worst case;
/// shared-blocks dedup and zero holes only reduce demand), then round up
/// to whole block groups plus a small margin.
///
/// With `dedup_estimate` (compact + shared blocks) file payloads are
/// hashed streaming (same zero-skip and full-block hashing as the
/// allocator) so deduplicated twins count once and the plan drops the
/// trailing empty groups. Hash collisions only undercount, which the
/// grow-and-retry loop absorbs; the estimate set is capped so planning
/// itself stays in the low MiB range.
fn plan_total_blocks(
    profile: &fs_ext4::Superblock,
    root: &Node,
    block: usize,
    reserve_blocks: u64,
    source: &mut dyn BlockSource,
    dedup_estimate: bool,
) -> Result<u64, Error> {
    let bpg = u64::from(profile.blocks_per_group);
    let mut all = Vec::new();
    collect(root, &mut all);
    // Exact data sizes (directories are serialized exactly).
    let mut data_blocks = 0u64;
    // Unique-block estimate state (only when dedup_estimate). A cap
    // abort falls back to the worst case inline (see below).
    let mut seen: HashSet<u64> = HashSet::new();
    let mut block_buf = vec![0u8; block];
    for (node_index, node) in all.iter().enumerate() {
        if node.is_dir() {
            data_blocks = data_blocks.saturating_add(
                (directory(node, block)?.len() as u64).div_ceil(block as u64),
            );
            continue;
        }
        if node.mode & 0xf000 == SYMLINK && node.data_len() <= 60 {
            continue;
        }
        if node.data_size == 0 {
            continue;
        }
        let need_blocks = node.data_size.div_ceil(block as u64);
        if !dedup_estimate {
            data_blocks = data_blocks.saturating_add(need_blocks);
            continue;
        }
        // Stream the file block by block exactly like the allocator:
        // zero blocks are holes, other blocks hash whole (tail padded).
        let mut unique_here = 0u64;
        let mut aborted = false;
        for logical in 0..need_blocks {
            let chunk = ((node.data_size - logical * block as u64).min(block as u64)) as usize;
            block_buf.fill(0);
            source.read_node_block(node, logical as usize * block, &mut block_buf[..chunk])?;
            if block_buf.iter().all(|v| *v == 0) {
                continue;
            }
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            block_buf.hash(&mut hasher);
            if seen.insert(hasher.finish()) {
                unique_here += 1;
            }
            // Bound planning RAM: past ~2M unique blocks (~16 MiB of set)
            // fall back to the worst case for this and all later files.
            if seen.len() > 2_000_000 {
                aborted = true;
                break;
            }
        }
        if aborted {
            // Keep the uniques counted so far, then worst-case the rest
            // (this file fully, the loop below handles the followers).
            data_blocks = data_blocks.saturating_add(unique_here);
            data_blocks = data_blocks.saturating_add(need_blocks);
            for later in all.iter().skip(node_index + 1) {
                if later.is_dir() {
                    data_blocks = data_blocks.saturating_add(
                        (directory(later, block)?.len() as u64).div_ceil(block as u64),
                    );
                } else if later.data_size > 0
                    && !(later.mode & 0xf000 == SYMLINK && later.data_len() <= 60)
                {
                    data_blocks = data_blocks.saturating_add(later.data_size.div_ceil(block as u64));
                }
            }
            break;
        }
        data_blocks = data_blocks.saturating_add(unique_here);
    }
    // Extent-tree blocks: fresh runs are long, so bound generously.
    let tree_blocks = all.len() as u64;
    // Inodes must fit as well (tiny files consume inodes, not blocks).
    let inodes_needed = all.len() as u64 + 16;
    // Fixed-point from below: the estimate must be able to shrink far
    // under the input size (compact/removals), so never seed from it.
    let mut groups = 1u64;
    for _ in 0..32 {
        let need = data_blocks + tree_blocks + groups * meta_blocks(groups, profile, block)
            + reserve_blocks;
        let by_space = need.div_ceil(bpg);
        let by_inodes =
            inodes_needed.div_ceil(u64::from(profile.inodes_per_group));
        let want = by_space.max(by_inodes).max(1);
        if want <= groups {
            break;
        }
        groups = want;
    }
    let mut total = groups * bpg;
    // Keep a small margin so the result is not 100.00% full, and always
    // honor an explicit reserve (loop: one group may be smaller than it).
    for _ in 0..1024 {
        let need = data_blocks + tree_blocks + groups * meta_blocks(groups, profile, block);
        if total.saturating_sub(need) >= (bpg / 4).max(reserve_blocks) {
            break;
        }
        groups += 1;
        total = groups * bpg;
    }
    Ok(total)
}

fn meta_blocks(groups: u64, profile: &fs_ext4::Superblock, block: usize) -> u64 {
    let descriptor_blocks = (groups * 32).div_ceil(block as u64);
    let table_blocks = (u64::from(profile.inodes_per_group) * u64::from(profile.inode_size))
        .div_ceil(block as u64);
    1 + descriptor_blocks + 2 + table_blocks
}

struct Placement {
    nid: u64,
    size: usize,
    extents: Vec<(u32, u32, u32)>,
    inline_symlink: bool,
    is_dir: bool,
}

/// Block-level content source for the rebuild: the native ext4 `Source`,
/// or an adapter over another filesystem (EROFS tree conversion).
/// Reads are always bounded by the callers to the file size; anything
/// past the end must read as zeros (holes).
pub(crate) trait BlockSource {
    fn read_node_block(
        &mut self,
        node: &Node,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<(), Error>;

    /// Stream one file's logical range into the output image.
    /// Provided method: 64 KiB chunks through `read_node_block`.
    fn copy_node_extents_to_image(
        &mut self,
        node: &Node,
        extents: &[(u32, u32, u32)],
        image: &mut FileWriter,
        block: usize,
    ) -> Result<(), Error> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut remaining = node.data_size;
        for &(logical, physical, length) in extents {
            let mut extent_bytes = (length as u64 * block as u64).min(remaining);
            let mut src_pos = logical as u64 * block as u64;
            let mut dst_pos = physical as u64 * block as u64;
            while extent_bytes > 0 {
                let chunk = (extent_bytes as usize).min(buf.len());
                self.read_node_block(node, src_pos as usize, &mut buf[..chunk])?;
                image.write_at(dst_pos as usize, &buf[..chunk])?;
                src_pos += chunk as u64;
                dst_pos += chunk as u64;
                extent_bytes -= chunk as u64;
                remaining = remaining.saturating_sub(chunk as u64);
            }
        }
        Ok(())
    }
}

fn rebuild_groups_to(
    source: &mut dyn BlockSource,
    profile: &fs_ext4::Superblock,
    root: &Node,
    output: &Path,
    shared_blocks: bool,
    block: usize,
    total_blocks: u64,
) -> Result<(), Error> {
    let blocks_per_group = u64::from(profile.blocks_per_group);
    let groups = total_blocks.div_ceil(blocks_per_group);
    let inodes_per_group = u64::from(profile.inodes_per_group);
    let inode_size = usize::from(profile.inode_size);
    let inode_table_blocks = (inodes_per_group as usize * inode_size).div_ceil(block);
    let descriptor_blocks = (groups as usize * 32).div_ceil(block);
    let total_blocks_usize = usize::try_from(total_blocks)
        .map_err(|_| Error::Invalid("ext4 image is too large".into()))?;
    let mut all = Vec::new();
    collect(root, &mut all);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)?;
    file.set_len(total_blocks * block as u64)?;
    let mut image = FileWriter::new(file);
    let mut used = vec![false; total_blocks_usize];
    let mut group_metadata = Vec::new();
    for group in 0..groups {
        let start = group * blocks_per_group;
        if start >= total_blocks {
            break;
        }
        let group_blocks = (total_blocks - start).min(blocks_per_group);
        let super_block = start;
        let gdt_start = start + 1;
        let bitmap = start + 1 + descriptor_blocks as u64;
        let inode_bitmap = bitmap + 1;
        let inode_table = inode_bitmap + 1;
        let end = inode_table + inode_table_blocks as u64;
        if end > start + group_blocks {
            return Err(Error::Invalid(
                "ext4 group has no room for inode table".into(),
            ));
        }
        for value in start..end {
            used[value as usize] = true;
        }
        group_metadata.push((super_block, gdt_start, bitmap, inode_bitmap, inode_table));
    }
    let mut next = 0u64;
    let mut placements: Vec<Placement> = Vec::with_capacity(all.len());
    let mut dir_data_map: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut shared_index: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut block_buf = vec![0u8; block];
    let mut verify_buf = vec![0u8; block];
    for node in &all {
        let is_dir = node.is_dir();
        let is_symlink_inline = node.mode & 0xf000 == SYMLINK && node.data_len() <= 60;
        let size = if is_dir {
            let d = directory(node, block)?;
            let len = d.len();
            dir_data_map.insert(node.nid, d);
            len
        } else {
            node.data_size as usize
        };
        if is_symlink_inline {
            placements.push(Placement {
                nid: node.nid,
                size,
                extents: Vec::new(),
                inline_symlink: true,
                is_dir: false,
            });
            continue;
        }
        let block_count = (size as u64).div_ceil(block as u64);
        let mut extents: Vec<(u32, u32, u32)> = Vec::new();
        let share_file = shared_blocks && !is_dir && size > 0;
        if share_file {
            for logical in 0..block_count {
                let chunk_len = (size - logical as usize * block).min(block);
                block_buf.fill(0);
                source.read_node_block(node, logical as usize * block, &mut block_buf[..chunk_len])?;
                const MAX_EXTENTS: usize = 4 * 340;
                if block_buf.iter().all(|v| *v == 0) && extents.len() < MAX_EXTENTS {
                    continue;
                }
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                block_buf.hash(&mut hasher);
                let hash = hasher.finish();
                let physical = find_or_insert_shared_block(
                    &mut shared_index,
                    hash,
                    &block_buf,
                    &mut verify_buf,
                    &mut image,
                    block,
                    &mut next,
                    &mut used,
                    extents.len() >= MAX_EXTENTS,
                )?;
                if let Some(last) = extents.last_mut() {
                    if u64::from(last.0 + last.2) == logical && last.1 + last.2 == physical {
                        last.2 += 1;
                        continue;
                    }
                }
                extents.push((logical as u32, physical, 1));
            }
        } else {
            let mut logical = 0u64;
            let mut remaining = block_count;
            while remaining != 0 {
                while next as usize >= used.len() || used[next as usize] {
                    next += 1;
                    if next as usize >= used.len() {
                        return Err(Error::Invalid("ext4 image has no free data blocks".into()));
                    }
                }
                let start = next;
                let mut length = 0u64;
                while length < remaining.min(32767)
                    && start + length < used.len() as u64
                    && !used[(start + length) as usize]
                {
                    length += 1;
                }
                if length == 0 {
                    return Err(Error::Invalid("ext4 image has no free data blocks".into()));
                }
                for value in start..start + length {
                    used[value as usize] = true;
                }
                extents.push((
                    u32::try_from(logical)
                        .map_err(|_| Error::Invalid("ext4 logical block overflow".into()))?,
                    u32::try_from(start)
                        .map_err(|_| Error::Invalid("ext4 block number overflow".into()))?,
                    u32::try_from(length)
                        .map_err(|_| Error::Invalid("ext4 extent length overflow".into()))?,
                ));
                logical += length;
                remaining -= length;
                next = start + length;
            }
        }
        placements.push(Placement {
            nid: node.nid,
            size,
            extents,
            inline_symlink: false,
            is_dir,
        });
    }
    let mut extent_trees = Vec::new();
    for p in &placements {
        if p.extents.len() > 4 * 340 {
            return Err(Error::Invalid(
                "ext4 file requires an extent tree deeper than one level".into(),
            ));
        }
        let tree_count = p.extents.len().saturating_sub(1).div_ceil(340);
        let mut blocks = Vec::new();
        for _ in 0..tree_count {
            while next as usize >= used.len() || used[next as usize] {
                next += 1;
                if next as usize >= used.len() {
                    return Err(Error::Invalid(
                        "ext4 compact image has no free metadata blocks".into(),
                    ));
                }
            }
            used[next as usize] = true;
            blocks.push(
                u32::try_from(next)
                    .map_err(|_| Error::Invalid("ext4 extent tree block overflow".into()))?,
            );
            next += 1;
        }
        extent_trees.push(blocks);
    }
    let total_inodes = groups * inodes_per_group;
    let used_blocks = used.iter().filter(|value| **value).count() as u64;
    let free_blocks = total_blocks.saturating_sub(used_blocks);
    let used_inodes = all.len() as u64 + 9;
    let free_inodes = total_inodes.saturating_sub(used_inodes);
    for (metadata_group, &(super_block, gdt_start, bitmap, inode_bitmap, _)) in
        group_metadata.iter().enumerate()
    {
        write_super(
            &mut image,
            &profile,
            total_blocks,
            total_inodes as u32,
            blocks_per_group as u32,
            inodes_per_group as u32,
            super_block,
            free_blocks as u32,
            free_inodes as u32,
            shared_blocks,
        )?;
        for group in 0..groups {
            let descriptor = gdt_start as usize * block + group as usize * 32;
            let (_, _, group_bitmap, group_inode_bitmap, group_inode_table) =
                group_metadata[group as usize];
            image.put32(descriptor, group_bitmap as u32)?;
            image.put32(descriptor + 4, group_inode_bitmap as u32)?;
            image.put32(descriptor + 8, group_inode_table as u32)?;
            let group_start = group * blocks_per_group;
            let group_end = (group_start + blocks_per_group).min(total_blocks);
            let block_free = (group_start..group_end)
                .filter(|value| !used[*value as usize])
                .count() as u32;
            let inode_used = all
                .iter()
                .filter(|node| (node.nid - 1) / inodes_per_group == u64::from(group))
                .count() as u32
                + if group == 0 { 9 } else { 0 };
            let directory_count = all
                .iter()
                .filter(|node| {
                    node.is_dir() && (node.nid - 1) / inodes_per_group == u64::from(group)
                })
                .count() as u32;
            image.put16(descriptor + 12, block_free)?;
            image.put16(
                descriptor + 14,
                (inodes_per_group as u32).saturating_sub(inode_used),
            )?;
            image.put16(descriptor + 16, directory_count)?;
        }
        let group_start = metadata_group as u64 * blocks_per_group;
        let group_end = (group_start + blocks_per_group).min(total_blocks);
        for value in group_start..group_end {
            if used[value as usize] {
                image.or_byte(
                    bitmap as usize * block + (value - group_start) as usize / 8,
                    1 << ((value - group_start) % 8),
                )?;
            }
        }
        for index in group_end - group_start..blocks_per_group {
            image.or_byte(bitmap as usize * block + index as usize / 8, 1 << (index % 8))?;
        }
        if metadata_group == 0 {
            for inode in 1..=10u64 {
                let index = inode - 1;
                image.or_byte(inode_bitmap as usize * block + index as usize / 8, 1 << (index % 8))?;
            }
        }
        for node in &all {
            let index = node.nid - 1;
            if index / inodes_per_group == metadata_group as u64 {
                let local = index % inodes_per_group;
                image.or_byte(inode_bitmap as usize * block + local as usize / 8, 1 << (local % 8))?;
            }
        }
        for index in inodes_per_group..(block as u64 * 8) {
            image.or_byte(inode_bitmap as usize * block + index as usize / 8, 1 << (index % 8))?;
        }
    }
    for (placement_index, p) in placements.iter().enumerate() {
        let node = all
            .iter()
            .find(|value| value.nid == p.nid)
            .ok_or_else(|| Error::invalid("file node vanished during rebuild"))?;
        let group = (p.nid - 1) / inodes_per_group;
        let index = (p.nid - 1) % inodes_per_group;
        let inode_table = group_metadata[group as usize].4;
        let offset = (inode_table as usize * block) + index as usize * inode_size;
        write_inode(
            &mut image,
            offset,
            node,
            p.size,
            &p.extents,
            &extent_trees[placement_index],
            block,
            inode_size,
        )?;
        write_extent_trees(&mut image, &p.extents, &extent_trees[placement_index], block)?;
        if p.inline_symlink {
            continue;
        }
        if p.is_dir {
            if let Some(dir_bytes) = dir_data_map.remove(&p.nid) {
                for &(logical, start, length) in &p.extents {
                    let src_start = logical as usize * block;
                    let src_size = (length as usize * block).min(dir_bytes.len().saturating_sub(src_start));
                    if src_size > 0 {
                        image.write_at(start as usize * block, &dir_bytes[src_start..src_start + src_size])?;
                    }
                }
            }
            continue;
        }
        if shared_blocks {
            continue;
        }
        source.copy_node_extents_to_image(node, &p.extents, &mut image, block)?;
    }
    Ok(())
}
impl BlockSource for Source {
    fn read_node_block(
        &mut self,
        node: &Node,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<(), Error> {
        if let NodePayload::Memory(data) = &node.payload {
            let end = (offset + buf.len()).min(data.len());
            if offset < data.len() {
                buf[..end - offset].copy_from_slice(&data[offset..end]);
                buf[end - offset..].fill(0);
            } else {
                buf.fill(0);
            }
            return Ok(());
        }
        let block_size = self.profile.block_size;
        let mut written = 0;
        while written < buf.len() {
            let current_logical = (offset + written) as u64;
            let block_idx = current_logical / block_size;
            let block_offset = current_logical % block_size;
            let extent = node.extents.iter().find(|e| {
                block_idx >= e.0 && block_idx < e.0 + e.2
            });
            let to_read = ((block_size - block_offset) as usize).min(buf.len() - written);
            if let Some(e) = extent {
                let physical_offset = (e.1 + (block_idx - e.0)) * block_size + block_offset;
                self.reader.read_at(physical_offset, &mut buf[written..written + to_read])?;
            } else {
                buf[written..written + to_read].fill(0);
            }
            written += to_read;
        }
        Ok(())
    }
}
fn find_or_insert_shared_block(
    shared_index: &mut HashMap<u64, Vec<u32>>,
    hash: u64,
    block_data: &[u8],
    verify_buf: &mut [u8],
    image: &mut FileWriter,
    block: usize,
    next: &mut u64,
    used: &mut [bool],
    force_new: bool,
) -> Result<u32, Error> {
    if !force_new {
        if let Some(candidates) = shared_index.get(&hash) {
            for &candidate in candidates {
                image.file.seek(SeekFrom::Start(candidate as u64 * block as u64))?;
                image.file.read_exact(verify_buf)?;
                if verify_buf == block_data {
                    return Ok(candidate);
                }
            }
        }
    }
    while *next as usize >= used.len() || used[*next as usize] {
        *next += 1;
        if *next as usize >= used.len() {
            return Err(Error::Invalid("ext4 compact image has no free data blocks".into()));
        }
    }
    used[*next as usize] = true;
    let physical = *next as u32;
    *next += 1;
    image.write_at(physical as usize * block, block_data)?;
    shared_index.entry(hash).or_default().push(physical);
    Ok(physical)
}
fn write_super(
    image: &mut FileWriter,
    profile: &fs_ext4::Superblock,
    blocks: u64,
    inodes: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    block_number: u64,
    free_blocks: u32,
    free_inodes: u32,
    shared_blocks: bool,
) -> Result<(), Error> {
    let p = block_number as usize * profile.block_size as usize
        + if block_number == 0 { 1024 } else { 0 };
    image.put32(p, inodes)?;
    image.put32(p + 4, blocks as u32)?;
    image.put32(p + 12, free_blocks)?;
    image.put32(p + 16, free_inodes)?;
    image.put32(p + 24, profile.block_size.trailing_zeros() - 10)?;
    image.put32(p + 28, profile.block_size.trailing_zeros() - 10)?;
    image.put32(p + 32, blocks_per_group)?;
    image.put32(p + 36, blocks_per_group)?;
    image.put32(p + 40, inodes_per_group)?;
    image.put16(p + 56, u32::from(MAGIC))?;
    image.put16(p + 88, profile.inode_size as u32)?;
    image.put32(p + 92, profile.compat & !COMPAT_RESIZE_INODE)?;
    image.put32(p + 96, EXTENTS | INCOMPAT_FILETYPE)?;
    let mut ro_compat = profile.ro_compat & !(RO_SHARED_BLOCKS | RO_GDT_CSUM | 1);
    if shared_blocks {
        ro_compat |= RO_SHARED_BLOCKS;
    }
    image.put32(p + 100, ro_compat)?;
    image.put32(p + 76, 1)?;
    image.put32(p + 84, 11)?;
    image.put16(p + 254, 32)?;
    Ok(())
}
fn write_inode(
    image: &mut FileWriter,
    p: usize,
    node: &Node,
    size: usize,
    extents: &[(u32, u32, u32)],
    tree_blocks: &[u32],
    block: usize,
    inode_size: usize,
) -> Result<(), Error> {
    image.put16(p, u32::from(node.mode))?;
    image.put16(p + 2, node.uid)?;
    image.put32(p + 4, size as u32)?;
    image.put16(p + 24, node.gid)?;
    let links = if node.is_dir() {
        2 + node.children.iter().filter(|child| child.is_dir()).count() as u16
    } else {
        1
    };
    image.put16(p + 26, u32::from(links))?;
    if node.mode & 0xf000 == SYMLINK && size <= 60 {
        image.put32(p + 28, 0)?;
        image.write_at(p + 40, node.data())?;
        if let Some(context) = &node.context {
            if let Err(error) = write_selinux_xattr(image, p, inode_size, context) {
                eprintln!(
                    "warning: dropping SELinux context of {}: {error}",
                    node.name
                );
            }
        }
        return Ok(());
    }
    image.put32(
        p + 28,
        (extents.iter().map(|extent| extent.2).sum::<u32>() + tree_blocks.len() as u32)
            * (block as u32 / 512),
    )?;
    image.put32(p + 32, INODE_EXTENTS)?;
    if tree_blocks.is_empty() {
        image.put16(p + 40, u32::from(EXTENT_MAGIC))?;
        image.put16(p + 42, extents.len() as u32)?;
        image.put16(p + 44, 4)?;
        for (index, &(logical, start, length)) in extents.iter().enumerate() {
            write_extent(image, p + 52 + index * 12, logical, start, length)?;
        }
    } else {        image.put16(p + 40, u32::from(EXTENT_MAGIC))?;
        image.put16(p + 42, tree_blocks.len() as u32)?;
        image.put16(p + 44, 4)?;
        image.put16(p + 46, 1)?;
        for (index, &tree_block) in tree_blocks.iter().enumerate() {
            let first = extents[index * 340].0;
            let entry = p + 52 + index * 12;
            image.put32(entry, first)?;
            image.put32(entry + 4, tree_block)?;
            image.put16(entry + 8, 0)?;
        }
    }
    // SELinux context: preserved from the source inode unless explicitly
    // overridden. Serialized inline; dropped with a warning if it cannot
    // fit into the inode extra area.
    if let Some(context) = &node.context {
        if let Err(error) = write_selinux_xattr(image, p, inode_size, context) {
            eprintln!(
                "warning: dropping SELinux context of {}: {error}",
                node.name
            );
        }
    }
    Ok(())
}

/// Serialize `security.selinux` as an inline xattr in the inode extra area.
///
/// Byte layout (must match kernel/e2fsck/mkfs, verified against
/// e2fsprogs `check_ea_in_inode` + `ext2_ext_attr_*` macros):
/// extra_isize=28, 4-byte magic at +156, one 16-byte entry at +160,
/// name "selinux" at +176, zero terminator entry where `NEXT()` lands,
/// and the NUL-terminated value packed backwards from the area end.
/// `e_value_offs` is relative to `header + 4` (the entry base), and entry
/// chaining uses `LEN = 16 + padded name`, so forward and backward
/// regions can never overlap.
fn write_selinux_xattr(
    image: &mut FileWriter,
    p: usize,
    inode_size: usize,
    context: &str,
) -> Result<(), Error> {
    const HEADER_OFF: usize = 156;
    // Region for entries/values: [HEADER_OFF+4, inode_size).
    let region = inode_size
        .checked_sub(HEADER_OFF + 4)
        .ok_or_else(|| Error::invalid("inode has no room for an inline SELinux xattr"))?;
    // Value with NUL terminator (Android convention), 4-padded.
    let value_len = context
        .len()
        .checked_add(1)
        .ok_or_else(|| Error::invalid("SELinux context is too long"))?;
    if context.is_empty() || value_len > 64 {
        return Err(Error::invalid("SELinux context is missing or too long"));
    }
    let value_padded = value_len.div_ceil(4) * 4;
    // Forward footprint: entry (16) + name (8) + terminator (4).
    const FORWARD: usize = 16 + 8 + 4;
    if FORWARD + value_padded > region {
        return Err(Error::invalid(
            "inode has no room for an inline SELinux xattr",
        ));
    }
    // Value sits at the region end; off is relative to header + 4.
    let value_off = (region - value_padded - 4) as u16;
    image.put16(p + 128, 28)?; // i_extra_isize
    image.put32(p + HEADER_OFF, 0xea02_0000)?; // ibody magic (4 bytes only)
    let entry = p + HEADER_OFF + 4;
    let mut entry_buf = [0u8; 16];
    entry_buf[0] = b"selinux".len() as u8;
    entry_buf[1] = 6; // name_index: security.*
    entry_buf[2..4].copy_from_slice(&value_off.to_le_bytes());
    entry_buf[4..8].copy_from_slice(&0u32.to_le_bytes()); // value in inode
    entry_buf[8..12].copy_from_slice(&(value_len as u32).to_le_bytes());
    entry_buf[12..16].copy_from_slice(&0u32.to_le_bytes()); // hash
    image.write_at(entry, &entry_buf)?;
    let mut name_buf = [0u8; 8];
    name_buf[..7].copy_from_slice(b"selinux");
    image.write_at(entry + 16, &name_buf)?;
    // Terminator entry (all zeros) exactly where NEXT() lands. The fresh
    // inode is already zeroed, but write it explicitly: NEXT() arithmetic
    // is what e2fsck follows, so its position must be deliberate.
    image.write_at(entry + 16 + 8, &[0u8; 4])?;
    let value_at = p + HEADER_OFF + 4 + value_off as usize;
    image.write_at(value_at, context.as_bytes())?;
    image.write_at(value_at + context.len(), &[0u8])?;
    let pad = value_padded - value_len;
    if pad > 0 {
        image.write_at(value_at + value_len, &vec![0u8; pad])?;
    }
    Ok(())
}
fn write_extent_trees(
    image: &mut FileWriter,
    extents: &[(u32, u32, u32)],
    tree_blocks: &[u32],
    block: usize,
) -> Result<(), Error> {
    for (tree_index, &tree_block) in tree_blocks.iter().enumerate() {
        let start = tree_index * 340;
        let end = (start + 340).min(extents.len());
        let offset = tree_block as usize * block;
        image.put16(offset, u32::from(EXTENT_MAGIC))?;
        image.put16(offset + 2, (end - start) as u32)?;
        image.put16(offset + 4, 340)?;
        image.put16(offset + 6, 0)?;
        for (index, &(logical, physical, length)) in extents[start..end].iter().enumerate() {
            write_extent(image, offset + 12 + index * 12, logical, physical, length)?;
        }
    }
    Ok(())
}
fn write_extent(image: &mut FileWriter, offset: usize, logical: u32, physical: u32, length: u32) -> Result<(), Error> {
    image.put32(offset, logical)?;
    image.put16(offset + 4, length)?;
    image.put16(offset + 6, 0)?;
    image.put32(offset + 8, physical)?;
    Ok(())
}
