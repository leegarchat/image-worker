//! EROFS write engine: patch (append), delta (overlay) and full (rebuild)
//! writers over one shared Node tree. All low-level parsing (superblock,
//! inodes, directories, decompression, xattrs) lives in `crate::fs`; this
//! module only plans and emits.
//!
//! Memory discipline: image-sourced bytes always stream (64 KiB quanta,
//! single-pcluster decodes, per-file compact indexes freed on file
//! switch). RAM holds metadata (tree, dir blobs, patches) plus at most
//! the operator-supplied delta (added/replaced file bytes, spooled to
//! `tail`/patches). Full-image buffering never happens; multi-GB inputs
//! stream in constant memory.

pub mod convert;
pub mod ext4;

use crate::core::{Error, Image, buf};
use crate::fs;
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

const MAGIC: u32 = 0xe0f5_e1e2;
const SPARSE_MAGIC: u32 = 0xed26_ff3a;

#[derive(Clone)]
pub(crate) enum NodePayload {
    None,
    InlineDir(Vec<u8>),
    Memory(Vec<u8>),
}

#[derive(Clone)]
pub(crate) struct Node {
    pub(crate) nid: u64,
    pub(crate) original_nid: u64,
    pub(crate) parent_nid: u64,
    pub(crate) name: String,
    pub(crate) mode: u16,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) payload: NodePayload,
    pub(crate) data_size: u64,
    pub(crate) children: Vec<Node>,
    pub(crate) layout: u16,
    pub(crate) raw_block: u32,
    pub(crate) modified: bool,
    /// Set when file/dir *content* was replaced (host bytes or image bytes),
    /// as opposed to metadata-only changes. Drives the overlay writer.
    pub(crate) content_changed: bool,
    /// SELinux context (`security.selinux`). For ext4 it is read from the
    /// source inode and serialized back on write; explicit `--context`
    /// overrides it. EROFS trees read it from shared/inline xattrs;
    /// EROFS output preserves it positionally (overlay) or via a fresh
    /// shared table (full rebuild / converter).
    pub(crate) context: Option<String>,
    /// True when `context` was explicitly requested (`--context`), as
    /// opposed to preserved from the source image. Explicit contexts need
    /// fresh table entries; preserved ones survive positionally.
    pub(crate) context_explicit: bool,
    pub(crate) extents: Vec<(u64, u64, u64)>,
    /// Compressed-compact planning: total 4 KiB logical clusters when this
    /// file is written with `--compress` (layout 3 + map header + index);
    /// 0 selects the plain layout. Directories are always 0.
    pub(crate) erofs_compact_clusters: u64,
}
impl Node {
    pub(crate) fn is_dir(&self) -> bool {
        self.mode & 0xf000 == 0x4000
    }
    pub(crate) fn data(&self) -> &[u8] {
        match &self.payload {
            NodePayload::InlineDir(d) => d,
            NodePayload::Memory(d) => d,
            NodePayload::None => &[],
        }
    }
    pub(crate) fn data_len(&self) -> usize {
        match &self.payload {
            NodePayload::InlineDir(d) => d.len(),
            NodePayload::Memory(d) => d.len(),
            NodePayload::None => self.data_size as usize,
        }
    }
}
pub(crate) struct Source {
    path: std::path::PathBuf,
    pub(crate) reader: Image,
    pub(crate) block_size: u64,
    meta_blkaddr: u64,
    pub(crate) root_nid: u64,
    volume: [u8; 16],
    feature_compat: u32,
    feature_incompat: u32,
    feature_ro_compat: u32,
    build_time: u64,
    build_nsec: u32,
    inodes: u64,
    xattr_blkaddr: u64,
}
impl Source {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let mut reader = Image::open(path)?;
        // Single source of truth for geometry (see crate::fs::erofs).
        let sb = fs::erofs::Superblock::read(&mut reader)?;
        let bits = sb.block_size.trailing_zeros();
        if !(12..=16).contains(&bits) {
            return Err(Error::Invalid("unsupported EROFS block size".into()));
        }
        Ok(Self {
            path: path.to_path_buf(),
            reader,
            block_size: sb.block_size,
            meta_blkaddr: sb.meta_blkaddr,
            root_nid: sb.root_nid,
            volume: sb.volume,
            feature_compat: sb.feature_compat,
            feature_incompat: sb.feature_incompat,
            feature_ro_compat: sb.feature_ro_compat,
            build_time: sb.build_time,
            build_nsec: sb.build_nsec,
            inodes: sb.inodes,
            xattr_blkaddr: sb.xattr_blkaddr,
        })
    }

    /// Shared read-side superblock view over this source (single source of
    /// truth for geometry; used for xattr lookups).
    pub(crate) fn fs_superblock(&self) -> fs::erofs::Superblock {
        fs::erofs::Superblock {
            block_size: self.block_size,
            root_nid: self.root_nid,
            meta_blkaddr: self.meta_blkaddr,
            feature_compat: self.feature_compat,
            feature_incompat: self.feature_incompat,
            feature_ro_compat: self.feature_ro_compat,
            volume: self.volume,
            build_time: self.build_time,
            build_nsec: self.build_nsec,
            inodes: self.inodes,
            xattr_blkaddr: self.xattr_blkaddr,
        }
    }
    pub(crate) fn data(&mut self, nid: u64) -> Result<(u16, u32, u32, Vec<u8>), Error> {
        // Single implementation lives in crate::fs (inode layout, inline
        // tails, compressed-compact decode); the write engine only adapts
        // the shape for its Node tree.
        let fs_sb = self.fs_superblock();
        let fi = fs::erofs::inode::read(&fs_sb, &mut self.reader, nid)?;
        let data = fs::erofs::inode::read_data(&fs_sb, &mut self.reader, &fi)?;
        Ok((fi.mode, fi.uid, fi.gid, data))
    }
    pub(crate) fn tree(&mut self, nid: u64, name: String) -> Result<Node, Error> {
        self.tree_depth(nid, name, 0)
    }

    /// Recursive worker with a depth cap: real trees are dozens deep;
    /// unbounded recursion on corrupt/cyclic data would exhaust the stack.
    fn tree_depth(&mut self, nid: u64, name: String, depth: u32) -> Result<Node, Error> {
        if depth > 512 {
            return Err(Error::invalid("directory nesting is too deep"));
        }
        let fs_sb = self.fs_superblock();
        let fi = fs::erofs::inode::read(&fs_sb, &mut self.reader, nid)?;
        let is_dir = fi.size > 0 && fi.mode & 0xf000 == 0x4000;
        let payload = if is_dir {
            NodePayload::InlineDir(fs::erofs::inode::read_data(&fs_sb, &mut self.reader, &fi)?)
        } else {
            NodePayload::None
        };
        let mut node = Node {
            nid,
            original_nid: nid,
            parent_nid: 0,
            name: name.clone(),
            mode: fi.mode,
            uid: fi.uid,
            gid: fi.gid,
            payload,
            data_size: fi.size,
            children: Vec::new(),
            layout: fi.layout,
            raw_block: fi.raw_blkaddr,
            modified: false,
            content_changed: false,
            context: None,
            context_explicit: false,
            erofs_compact_clusters: 0,
            extents: Vec::new(),
        };
        // SELinux context rides along for conversion (erofs->ext4) and for
        // reporting; EROFS output preserves it positionally via repatch.
        node.context = fs::erofs::xattr::selinux(&fs_sb, &mut self.reader, &fi)?;
        if is_dir {
            let data = match &node.payload {
                NodePayload::InlineDir(d) => d.clone(),
                _ => Vec::new(),
            };
            for entry in fs::erofs::dir::parse(&data, self.block_size as usize).map_err(
                |error| Error::Invalid(format!("directory inode {nid} ({name}): {error}")),
            )? {
                if entry.name != "." && entry.name != ".." {
                    node.children.push(self.tree_depth(entry.nid, entry.name, depth + 1)?);
                }
            }
        }
        Ok(node)
    }
}

/// Whether a fresh output inode needs an xattr area (shared selinux).
pub(crate) fn node_needs_xattr(node: &Node) -> bool {
    node.context.as_deref().is_some_and(|c| !c.is_empty())
}

/// 32-byte slots spanned by a fresh inode: 1 plain, 2 with a shared
/// xattr area (32B compact inode + 16B header/ID area = 48B), more for
/// compressed-compact files (map header + all-4B index follow the area).
pub(crate) fn inode_slots(node: &Node) -> u64 {
    if node.erofs_compact_clusters == 0 {
        return if node_needs_xattr(node) { 2 } else { 1 };
    }
    let xattr = if node_needs_xattr(node) { 16u64 } else { 0 };
    let index = crate::fs::erofs::compress::index_len(node.erofs_compact_clusters) as u64;
    // data_offset is 8-aligned past inode + xattrs, then map + index.
    let footprint = ((32 + xattr + 7) & !7) + 8 + index;
    footprint.div_ceil(32)
}

/// Strided NID assignment: inodes with xattrs reserve 2 slots (64 bytes),
/// because the 16-byte shared-xattr area follows the 32-byte inode body.
/// Dense `nid*32` packing would collide with the area bytes otherwise.
///
/// Slots overlapping the superblock (`[1024, 1152)`, present in every
/// image) are skipped: mkfs never assigns those NIDs either, and writing
/// inodes there would clobber the superblock (or be clobbered by it,
/// depending on patch order).
pub(crate) fn assign_nids_strided(
    node: &mut Node,
    next: &mut u64,
    parent_nid: u64,
    meta_blkaddr: u64,
    block_size: u64,
) {
    // Skip first: the footprint being assigned must clear the superblock.
    skip_superblock(next, meta_blkaddr, block_size, inode_slots(node));
    node.nid = *next;
    node.parent_nid = parent_nid;
    *next += inode_slots(node);
    for child in &mut node.children {
        assign_nids_strided(child, next, node.nid, meta_blkaddr, block_size);
    }
}

/// Advance `*next` until a `stride`-slot footprint starting there no
/// longer intersects the superblock.
fn skip_superblock(next: &mut u64, meta_blkaddr: u64, block_size: u64, stride: u64) {
    const SB_START: u64 = 1024;
    const SB_END: u64 = 1152;
    loop {
        let off = match meta_blkaddr
            .checked_mul(block_size)
            .and_then(|base| base.checked_add(next.checked_mul(32)?))
        {
            Some(off) => off,
            // On overflow there is nothing sane to assign; the writer
            // fails later with a proper bounds error.
            None => return,
        };
        let footprint = stride.max(1) * 32;
        let clear = off
            .checked_add(footprint)
            .is_none_or(|end| end <= SB_START || off >= SB_END);
        if clear {
            return;
        }
        *next += 1;
    }
}

/// Sorted unique non-empty SELinux contexts in a tree (shared-table keys).
pub(crate) fn collect_contexts(root: &Node) -> Vec<String> {
    fn walk(node: &Node, out: &mut Vec<String>) {
        if let Some(ctx) = node.context.as_deref() {
            if !ctx.is_empty() && !out.iter().any(|c| c == ctx) {
                out.push(ctx.to_owned());
            }
        }
        for child in &node.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}
fn nodes<'a>(node: &'a Node, result: &mut Vec<&'a Node>) {
    result.push(node);
    for child in &node.children {
        nodes(child, result);
    }
}
fn align(value: usize, alignment: usize) -> usize {
    value.div_ceil(alignment) * alignment
}
fn sort_tree(node: &mut Node) {
    node.children
        .sort_by(|left, right| left.name.cmp(&right.name));
    for child in &mut node.children {
        sort_tree(child);
    }
}
fn set_parents(node: &mut Node, parent_nid: u64) {
    node.parent_nid = parent_nid;
    for child in &mut node.children {
        set_parents(child, node.nid);
    }
}
fn directory_bytes(node: &Node, block_size: usize) -> Vec<u8> {
    let mut entries: Vec<(u64, String, u8)> = Vec::with_capacity(node.children.len() + 2);
    entries.push((node.nid, ".".into(), 2));
    entries.push((node.parent_nid, "..".into(), 2));
    entries.extend(node.children.iter().map(|child| {
        (
            child.nid,
            child.name.clone(),
            if child.is_dir() { 2 } else { 1 },
        )
    }));
    let mut output = Vec::new();
    let mut start = 0;
    while start < entries.len() {
        let mut end = start;
        let mut names = 0;
        while end < entries.len()
            && (end - start + 1) * 12 + names + entries[end].1.len() <= block_size
        {
            names += entries[end].1.len();
            end += 1;
        }
        if end == start {
            return Vec::new();
        }
        let first = (end - start) * 12;
        let mut block = vec![0u8; block_size];
        let mut name_offset = first;
        for (index, entry) in entries[start..end].iter().enumerate() {
            let offset = index * 12;
            block[offset..offset + 8].copy_from_slice(&entry.0.to_le_bytes());
            block[offset + 8..offset + 10].copy_from_slice(&(name_offset as u16).to_le_bytes());
            block[offset + 10] = entry.2;
            block[name_offset..name_offset + entry.1.len()].copy_from_slice(entry.1.as_bytes());
            name_offset += entry.1.len();
        }
        if end == entries.len() {
            output.extend_from_slice(&block[..name_offset]);
        } else {
            output.extend_from_slice(&block);
        }
        start = end;
    }
    output
}
fn find_directory<'a>(node: &'a mut Node, parts: &[&str]) -> Result<&'a mut Node, Error> {
    if parts.is_empty() {
        return Ok(node);
    }
    let child = node
        .children
        .iter_mut()
        .find(|child| child.name == parts[0] && child.is_dir())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "destination directory not found: /{}",
                parts.join("/")
            ))
        })?;
    find_directory(child, &parts[1..])
}
fn path_parts(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect()
}
fn find_path<'a>(node: &'a Node, parts: &[&str]) -> Option<&'a Node> {
    if parts.is_empty() {
        return Some(node);
    }
    node.children
        .iter()
        .find(|child| child.name == parts[0])
        .and_then(|child| find_path(child, &parts[1..]))
}
fn find_path_mut<'a>(node: &'a mut Node, parts: &[&str]) -> Option<&'a mut Node> {
    if parts.is_empty() {
        return Some(node);
    }
    node.children
        .iter_mut()
        .find(|child| child.name == parts[0])
        .and_then(|child| find_path_mut(child, &parts[1..]))
}
fn remove_path(node: &mut Node, parts: &[&str]) -> Result<Node, Error> {
    if parts.is_empty() {
        return Err(Error::Invalid("cannot remove root".into()));
    }
    let parent = find_directory(node, &parts[..parts.len() - 1])?;
    let name = parts[parts.len() - 1];
    let index = parent
        .children
        .iter()
        .position(|child| child.name == name)
        .ok_or_else(|| Error::Invalid(format!("path not found: /{}", parts.join("/"))))?;
    Ok(parent.children.remove(index))
}
fn clear_identity(node: &mut Node) {
    node.nid = 0;
    node.parent_nid = 0;
    for child in &mut node.children {
        clear_identity(child);
    }
}
fn apply_metadata(node: &mut Node, metadata: &Metadata) {
    if let Some(mode) = metadata.mode {
        node.mode = (node.mode & 0xf000) | (mode & 0x0fff);
        node.modified = true;
    }
    if let Some(uid) = metadata.uid {
        node.uid = uid;
        node.modified = true;
    }
    if let Some(gid) = metadata.gid {
        node.gid = gid;
        node.modified = true;
    }
    if let Some(context) = &metadata.context {
        node.context = Some(context.clone());
        node.context_explicit = true;
        node.modified = true;
    }
}
fn insert_path(
    root: &mut Node,
    destination: &str,
    mut node: Node,
    metadata: &Metadata,
    keep_identity: bool,
) -> Result<bool, Error> {
    let parts = path_parts(destination);
    if parts.is_empty() {
        return Err(Error::Invalid("destination must include a filename".into()));
    }
    let name = parts[parts.len() - 1].to_owned();
    let parent = find_directory(root, &parts[..parts.len() - 1])?;
    node.name = name.clone();
    apply_metadata(&mut node, metadata);
    if let Some(existing) = parent.children.iter_mut().find(|child| child.name == name) {
        if existing.is_dir() != node.is_dir() {
            return Err(Error::Invalid(
                "cannot replace file with directory or directory with file".into(),
            ));
        }
        if metadata.mode.is_none() { node.mode = existing.mode; }
        if metadata.uid.is_none() { node.uid = existing.uid; }
        if metadata.gid.is_none() { node.gid = existing.gid; }
        if metadata.context.is_none() {
            // Inherited (preserved) context is not explicit.
            node.context = existing.context.clone();
            node.context_explicit = false;
        }
        node.nid = existing.nid;
        // Image-sourced replacements keep their own origin so the overlay
        // writer can source shared/plain-ified bytes from it; host content
        // (original_nid == 0) inherits the target identity.
        if node.original_nid == 0 {
            node.original_nid = existing.original_nid;
        }
        node.parent_nid = existing.parent_nid;
        node.modified = true;
        node.content_changed = true;
        *existing = node;
        Ok(true)
    } else {
        // Moves preserve the inode identity so the overlay writer can keep
        // compressed/indexed data in place; the legacy rebuild renumbers
        // everything afterwards, so its output is unaffected.
        if !keep_identity {
            clear_identity(&mut node);
        }
        parent.children.push(node);
        Ok(false)
    }
}
fn host_node(path: &Path, name: String) -> Result<Node, Error> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path)?;
    let mode = metadata.permissions().mode() as u16;
    let mut node = Node {
        nid: 0,
        original_nid: 0,
        parent_nid: 0,
        name,
        mode,
        uid: 0,
        gid: 0,
        payload: NodePayload::None,
        data_size: 0,
        children: Vec::new(),
        layout: 0,
        raw_block: 0,
        modified: true,
        content_changed: false,
        context: None,
        context_explicit: false,
        erofs_compact_clusters: 0,
        extents: Vec::new(),
    };
    if node.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            node.children.push(host_node(
                &entry.path(),
                entry.file_name().to_string_lossy().into_owned(),
            )?);
        }
    } else {
        let data = std::fs::read(path)?;
        node.data_size = data.len() as u64;
        node.payload = NodePayload::Memory(data);
    }
    Ok(node)
}

fn stdin_node(name: String, mode: u16) -> Result<Node, Error> {
    let mut data = Vec::new();
    io::stdin().read_to_end(&mut data)?;
    Ok(Node {
        nid: 0,
        original_nid: 0,
        parent_nid: 0,
        name,
        mode,
        uid: 0,
        gid: 0,
        payload: NodePayload::Memory(data.clone()),
        data_size: data.len() as u64,
        children: Vec::new(),
        layout: 0,
        raw_block: 0,
        modified: true,
        content_changed: false,
        context: None,
        context_explicit: false,
        erofs_compact_clusters: 0,
        extents: Vec::new(),
    })
}
pub(crate) fn apply_action(root: &mut Node, action: Action) -> Result<bool, Error> {
    match action {
        Action::Add {
            source,
            destination,
            metadata,
        } => {
            let node = if source == "-" {
                let name = Path::new(&destination)
                    .file_name()
                    .ok_or_else(|| Error::Invalid("destination has no filename".into()))?
                    .to_string_lossy()
                    .into_owned();
                let mode = metadata.mode.unwrap_or(0o644);
                stdin_node(name, mode)?
            } else {
                let name = Path::new(&source)
                    .file_name()
                    .ok_or_else(|| Error::Invalid("source has no filename".into()))?
                    .to_string_lossy()
                    .into_owned();
                host_node(Path::new(&source), name)?
            };
            let is_replacement = insert_path(root, &destination, node, &metadata, false)?;
            Ok(is_replacement)
        }
        Action::Copy {
            source,
            destination,
            metadata,
        } => {
            let source_node = find_path(root, &path_parts(&source))
                .ok_or_else(|| Error::Invalid(format!("path not found: {source}")))?
                .clone();
            let _ = insert_path(root, &destination, source_node, &metadata, false)?;
            Ok(true)
        }
        Action::Move {
            source,
            destination,
            metadata,
        } => {
            let mut node = remove_path(root, &path_parts(&source))?;
            node.name = Path::new(&destination)
                .file_name()
                .ok_or_else(|| Error::Invalid("destination has no filename".into()))?
                .to_string_lossy()
                .into_owned();
            let _ = insert_path(root, &destination, node, &metadata, true)?;
            Ok(true)
        }
        Action::SetMode { path, mode } => {
            apply_metadata(
                find_path_mut(root, &path_parts(&path))
                    .ok_or_else(|| Error::Invalid(format!("path not found: {path}")))?,
                &Metadata {
                    mode: Some(mode),
                    ..Metadata::default()
                },
            );
            Ok(false)
        }
        Action::SetOwner { path, uid, gid } => {
            apply_metadata(
                find_path_mut(root, &path_parts(&path))
                    .ok_or_else(|| Error::Invalid(format!("path not found: {path}")))?,
                &Metadata {
                    uid: Some(uid),
                    gid: Some(gid),
                    ..Metadata::default()
                },
            );
            Ok(false)
        }
        Action::SetContext { path, context } => {
            apply_metadata(
                find_path_mut(root, &path_parts(&path))
                    .ok_or_else(|| Error::invalid(format!("path not found: {path}")))?,
                &Metadata {
                    context: Some(context),
                    ..Metadata::default()
                },
            );
            Ok(false)
        }
        Action::Remove { path, recursive } => {
            let parts = path_parts(&path);
            let target = find_path(root, &parts)
                .ok_or_else(|| Error::Invalid(format!("path not found: {path}")))?;
            if target.is_dir() && !target.children.is_empty() && !recursive {
                return Err(Error::Invalid(format!("directory not empty: {path}")));
            }
            remove_path(root, &parts)?;
            // Structural change: directory membership changed, so the
            // overlay/delta fast paths cannot shrink the image; callers
            // route removals through the full rebuild.
            Ok(true)
        }
    }
}
struct PatchedReader<'a> {
    source: &'a mut Image,
    original_size: u64,
    patches: Vec<(u64, Vec<u8>)>,
    tail_offset: u64,
    tail: Vec<u8>,
}

impl<'a> PatchedReader<'a> {
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<(), Error> {
        let len = output.len() as u64;
        let end = offset + len;

        if offset < self.original_size {
            let read_len = (self.original_size - offset).min(len) as usize;
            self.source.read_at(offset, &mut output[..read_len])?;
            if read_len < output.len() {
                output[read_len..].fill(0);
            }
        } else {
            output.fill(0);
        }

        for (p_off, p_data) in &self.patches {
            let p_end = *p_off + p_data.len() as u64;
            if end > *p_off && offset < p_end {
                let out_start = p_off.saturating_sub(offset) as usize;
                let p_start = offset.saturating_sub(*p_off) as usize;
                let copy_len = (p_data.len() - p_start).min(output.len() - out_start);
                output[out_start..out_start + copy_len]
                    .copy_from_slice(&p_data[p_start..p_start + copy_len]);
            }
        }

        let tail_end = self.tail_offset + self.tail.len() as u64;
        if end > self.tail_offset && offset < tail_end {
            let out_start = self.tail_offset.saturating_sub(offset) as usize;
            let t_start = offset.saturating_sub(self.tail_offset) as usize;
            let copy_len = (self.tail.len() - t_start).min(output.len() - out_start);
            output[out_start..out_start + copy_len]
                .copy_from_slice(&self.tail[t_start..t_start + copy_len]);
        }

        Ok(())
    }
}

fn write_patched_image(
    image: &mut PatchedReader,
    total_size: u64,
    block_size: usize,
    output: &Path,
    sparse_output: bool,
) -> Result<(), Error> {
    if output == Path::new("-") {
        let stdout = io::stdout();
        let mut stream = stdout.lock();
        if sparse_output {
            write_patched_sparse(image, total_size, block_size, &mut stream)?;
        } else {
            let mut buffer = vec![0u8; block_size];
            let blocks = total_size.div_ceil(block_size as u64) as usize;
            for i in 0..blocks {
                image.read_at((i * block_size) as u64, &mut buffer)?;
                stream.write_all(&buffer)?;
            }
        }
        stream.flush()?;
    } else {
        let mut file = File::create(output)?;
        if sparse_output {
            write_patched_sparse(image, total_size, block_size, &mut file)?;
        } else {
            let mut buffer = vec![0u8; block_size];
            let blocks = total_size.div_ceil(block_size as u64) as usize;
            for i in 0..blocks {
                image.read_at((i * block_size) as u64, &mut buffer)?;
                file.write_all(&buffer)?;
            }
        }
    }
    Ok(())
}

fn write_patched_sparse<W: Write>(
    image: &mut PatchedReader,
    total_size: u64,
    block_size: usize,
    output: &mut W,
) -> Result<(), Error> {
    if total_size % block_size as u64 != 0 {
        return Err(Error::Invalid("image size is not block aligned".into()));
    }
    let blocks = total_size.div_ceil(block_size as u64) as usize;
    let mut buffer = vec![0u8; block_size];
    let mut chunks: Vec<(u16, usize, usize)> = Vec::new();
    let mut index = 0;
    while index < blocks {
        image.read_at((index * block_size) as u64, &mut buffer)?;
        let kind = if buffer.iter().all(|byte| *byte == 0) {
            0xcac3
        } else if buffer.chunks_exact(4).all(|part| part == &buffer[..4]) {
            0xcac2
        } else {
            0xcac1
        };
        let start = index;
        index += 1;
        while index < blocks {
            image.read_at((index * block_size) as u64, &mut buffer)?;
            let same = match kind {
                0xcac3 => buffer.iter().all(|byte| *byte == 0),
                0xcac2 => {
                    buffer.chunks_exact(4).all(|part| part == &buffer[..4]) && buffer[..4] == buffer[..4]
                }
                _ => false,
            };
            if !same {
                break;
            }
            index += 1;
        }
        chunks.push((kind, start, index - start));
    }
    let mut header = [0u8; 28];
    header[0..4].copy_from_slice(&SPARSE_MAGIC.to_le_bytes());
    header[4..6].copy_from_slice(&1u16.to_le_bytes());
    header[8..10].copy_from_slice(&28u16.to_le_bytes());
    header[10..12].copy_from_slice(&12u16.to_le_bytes());
    header[12..16].copy_from_slice(&(block_size as u32).to_le_bytes());
    header[16..20].copy_from_slice(&(blocks as u32).to_le_bytes());
    header[20..24].copy_from_slice(&(chunks.len() as u32).to_le_bytes());
    output.write_all(&header)?;
    for (kind, start, count) in chunks {
        let payload = match kind {
            0xcac1 => count * block_size,
            0xcac2 => 4,
            _ => 0,
        };
        let total = 12 + payload;
        output.write_all(&kind.to_le_bytes())?;
        output.write_all(&0u16.to_le_bytes())?;
        output.write_all(&(count as u32).to_le_bytes())?;
        output.write_all(&(total as u32).to_le_bytes())?;
        match kind {
            0xcac1 => {
                let mut buf = vec![0u8; block_size];
                for i in 0..count {
                    image.read_at(((start + i) * block_size) as u64, &mut buf)?;
                    output.write_all(&buf)?;
                }
            }
            0xcac2 => {
                image.read_at((start * block_size) as u64, &mut buffer)?;
                output.write_all(&buffer[..4])?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn append_image(
    source: &mut Source,
    mut root: Node,
    output: &Path,
    sparse_output: bool,
) -> Result<(), Error> {
    sort_tree(&mut root);
    let root_nid = root.nid;
    set_parents(&mut root, root_nid);
    // Explicit contexts need the legacy rebuild (fresh shared table);
    // run() falls through to it on any append error.
    if root_has_explicit_context(&root) {
        return Err(Error::invalid(
            "explicit SELinux context needs a full rebuild",
        ));
    }
    let block_size = usize::try_from(source.block_size)
        .map_err(|_| Error::Invalid("invalid block size".into()))?;
    let original_size = source.reader.size();
    let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
    let (added_nid, parent_nid, added_data, added_mode, added_uid, added_gid, is_new) =
        if let Some(added) = find_new_node(&mut root) {
            added.nid = source.inodes;
            (
                added.nid,
                added.parent_nid,
                added.data().to_vec(),
                added.mode,
                added.uid,
                added.gid,
                true,
            )
        } else if let Some(existing) = find_modified(&root) {
            (
                existing.nid,
                existing.parent_nid,
                existing.data().to_vec(),
                existing.mode,
                existing.uid,
                existing.gid,
                false,
            )
        } else {
            return Err(Error::Invalid("no add or replacement action found".into()));
        };
    if is_new {
        let parent = find_node(&root, parent_nid)
            .ok_or_else(|| Error::Invalid("parent inode was not found".into()))?;
        let directory = directory_bytes(parent, block_size);
        if parent.layout == 0 && directory.len() > parent.data_len() {
            return Err(Error::Invalid(
                "destination directory has no free space".into(),
            ));
        }
        if parent.layout == 0 {
            let parent_data = parent.raw_block as u64 * block_size as u64;
            patches.push((parent_data, directory.clone()));
            let zero_len = parent.data_len() - directory.len();
            if zero_len > 0 {
                patches.push((parent_data + directory.len() as u64, vec![0u8; zero_len]));
            }
        } else {
            let dir_block = ((original_size + block_size as u64 - 1) / block_size as u64) as u32;
            let dir_offset = dir_block as u64 * block_size as u64;
            patches.push((dir_offset, directory.clone()));
            let parent_inode_offset = source.meta_blkaddr as u64 * block_size as u64 + parent.nid * 32;
            let mut parent_inode = vec![0u8; 32];
            parent_inode[0..2].copy_from_slice(&0u16.to_le_bytes());
            parent_inode[4..6].copy_from_slice(&parent.mode.to_le_bytes());
            parent_inode[6..8].copy_from_slice(&1u16.to_le_bytes());
            parent_inode[8..12].copy_from_slice(&(directory.len() as u32).to_le_bytes());
            parent_inode[16..20].copy_from_slice(&dir_block.to_le_bytes());
            parent_inode[20..24].copy_from_slice(&(parent.nid as u32).to_le_bytes());
            parent_inode[24..26].copy_from_slice(&(parent.uid as u16).to_le_bytes());
            parent_inode[26..28].copy_from_slice(&(parent.gid as u16).to_le_bytes());
            patches.push((parent_inode_offset, parent_inode));
            let new_original_size = dir_offset + directory.len() as u64;
            let new_tail_offset = ((new_original_size + block_size as u64 - 1) / block_size as u64) * block_size as u64;
            let data_block = ((new_tail_offset) / block_size as u64) as u32;
            let mut inode_patch = vec![0u8; 32];
            inode_patch[4..6].copy_from_slice(&added_mode.to_le_bytes());
            inode_patch[6..8].copy_from_slice(&1u16.to_le_bytes());
            inode_patch[8..12].copy_from_slice(&(added_data.len() as u32).to_le_bytes());
            inode_patch[16..20].copy_from_slice(&data_block.to_le_bytes());
            inode_patch[20..24].copy_from_slice(&(added_nid as u32).to_le_bytes());
            inode_patch[24..26].copy_from_slice(&(added_uid as u16).to_le_bytes());
            inode_patch[26..28].copy_from_slice(&(added_gid as u16).to_le_bytes());
            let inode_offset = source.meta_blkaddr as u64 * block_size as u64 + added_nid as u64 * 32;
            patches.push((inode_offset, inode_patch));
            let output_blocks = ((new_tail_offset + added_data.len() as u64 + block_size as u64 - 1) / block_size as u64) as u32;
            let mut sb_patch_compat = vec![0u8; 4];
            sb_patch_compat.copy_from_slice(&(source.feature_compat & !1).to_le_bytes());
            patches.push((1024 + 8, sb_patch_compat));
            let mut sb_patch_inodes = vec![0u8; 8];
            sb_patch_inodes.copy_from_slice(&(if is_new { source.inodes + 1 } else { source.inodes }).to_le_bytes());
            patches.push((1024 + 16, sb_patch_inodes));
            let mut sb_patch_blocks = vec![0u8; 4];
            sb_patch_blocks.copy_from_slice(&output_blocks.to_le_bytes());
            patches.push((1024 + 36, sb_patch_blocks));
            patches.push((1024 + 4, vec![0u8; 4]));
            let total_size = output_blocks as u64 * block_size as u64;
            let mut patched = PatchedReader {
                source: &mut source.reader,
                original_size,
                patches,
                tail_offset: new_tail_offset,
                tail: added_data,
            };
            return write_patched_image(&mut patched, total_size, block_size, output, sparse_output);
        }
    }
    let data_block = ((original_size + block_size as u64 - 1) / block_size as u64) as u32;
    let mut inode_patch = vec![0u8; 32];
    inode_patch[4..6].copy_from_slice(&added_mode.to_le_bytes());
    inode_patch[6..8].copy_from_slice(&1u16.to_le_bytes());
    inode_patch[8..12].copy_from_slice(&(added_data.len() as u32).to_le_bytes());
    inode_patch[16..20].copy_from_slice(&data_block.to_le_bytes());
    inode_patch[20..24].copy_from_slice(&(added_nid as u32).to_le_bytes());
    inode_patch[24..26].copy_from_slice(&(added_uid as u16).to_le_bytes());
    inode_patch[26..28].copy_from_slice(&(added_gid as u16).to_le_bytes());
    let inode_offset = source.meta_blkaddr as u64 * block_size as u64 + added_nid as u64 * 32;
    patches.push((inode_offset, inode_patch));
    let output_blocks = (((original_size + block_size as u64 - 1) / block_size as u64) as u32)
        + (added_data.len().div_ceil(block_size) as u32);
    let mut sb_patch_compat = vec![0u8; 4];
    sb_patch_compat.copy_from_slice(&(source.feature_compat & !1).to_le_bytes());
    patches.push((1024 + 8, sb_patch_compat));
    let mut sb_patch_inodes = vec![0u8; 8];
    sb_patch_inodes.copy_from_slice(&(if is_new { source.inodes + 1 } else { source.inodes }).to_le_bytes());
    patches.push((1024 + 16, sb_patch_inodes));
    let mut sb_patch_blocks = vec![0u8; 4];
    sb_patch_blocks.copy_from_slice(&output_blocks.to_le_bytes());
    patches.push((1024 + 36, sb_patch_blocks));
    patches.push((1024 + 4, vec![0u8; 4]));
    let tail_offset = ((original_size + block_size as u64 - 1) / block_size as u64) * block_size as u64;
    let total_size = output_blocks as u64 * block_size as u64;
    let mut patched = PatchedReader {
        source: &mut source.reader,
        original_size,
        patches,
        tail_offset,
        tail: added_data,
    };
    write_patched_image(&mut patched, total_size, block_size, output, sparse_output)
}
fn find_node(node: &Node, nid: u64) -> Option<&Node> {
    if node.nid == nid {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = find_node(child, nid) {
            return Some(found);
        }
    }
    None
}
fn find_new_node(node: &mut Node) -> Option<&mut Node> {
    if node.nid == 0 && !node.name.is_empty() {
        return Some(node);
    }
    for child in &mut node.children {
        if let Some(found) = find_new_node(child) {
            return Some(found);
        }
    }
    None
}
fn find_modified(node: &Node) -> Option<&Node> {
    if node.modified {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = find_modified(child) {
            return Some(found);
        }
    }
    None
}
fn patch_inode_metadata(
    source: &mut Source,
    root: &Node,
    output: &Path,
) -> Result<(), Error> {
    let mut out = File::create(output)?;
    let mut buf = vec![0u8; 64 * 1024];
    let total = source.reader.size();
    let mut remaining = total;
    let mut offset = 0u64;
    while remaining > 0 {
        let chunk = (remaining as usize).min(buf.len());
        source.reader.read_at(offset, &mut buf[..chunk])?;
        out.write_all(&buf[..chunk])?;
        offset += chunk as u64;
        remaining -= chunk as u64;
    }
    let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
    collect_metadata_patches(source, root, &mut patches)?;
    for (off, data) in patches {
        out.seek(SeekFrom::Start(off))?;
        out.write_all(&data)?;
    }
    Ok(())
}
fn collect_metadata_patches(
    source: &mut Source,
    node: &Node,
    patches: &mut Vec<(u64, Vec<u8>)>,
) -> Result<(), Error> {
    if node.modified {
        let offset = source.meta_blkaddr * source.block_size + node.nid * 32;
        // Field widths follow the on-disk format bit, read back here.
        let mut format = [0u8; 2];
        source.reader.read_at(offset, &mut format)?;
        if u16::from_le_bytes(format) & 1 != 0 {
            patches.push((offset + 4, node.mode.to_le_bytes().to_vec()));
            patches.push((offset + 24, node.uid.to_le_bytes().to_vec()));
            patches.push((offset + 28, node.gid.to_le_bytes().to_vec()));
        } else {
            patches.push((offset + 4, node.mode.to_le_bytes().to_vec()));
            patches.push((offset + 24, (node.uid as u16).to_le_bytes().to_vec()));
            patches.push((offset + 26, (node.gid as u16).to_le_bytes().to_vec()));
        }
    }
    for child in &node.children {
        collect_metadata_patches(source, child, patches)?;
    }
    Ok(())
}

// ── Overlay (delta) rebuild ──────────────────────────────────────────
// Unlike the legacy logical rebuild (which decompresses every compressed
// file and stores it back plain, doubling deflate images), the overlay
// keeps every untouched byte exactly where it is: compressed payloads,
// maps and indexes are never even read. Only changed/new directory blocks,
// patched inodes and appended file data are emitted, streaming, through
// PatchedReader. Peak RAM is bounded by the largest *touched* file, not
// by the image size. Anything the overlay cannot express (new host
// directories, directory copies, extended inodes) falls back to the
// legacy rebuild below.

const OVERLAY_FALLBACK: &str = "overlay unsupported; legacy rebuild required";

/// Fallback with a stderr trace (helps to see *why* the legacy path runs).
fn overlay_fallback(site: &str) -> Error {
    eprintln!("overlay fallback: {site}");
    Error::invalid(OVERLAY_FALLBACK)
}

/// Snapshot of directory membership (nid -> sorted child names), taken
/// before actions are applied, so rewritten directories can be detected.
fn snapshot_dirs(node: &Node, out: &mut Vec<(u64, Vec<String>)>) {
    if node.is_dir() {
        let mut names: Vec<String> = node.children.iter().map(|c| c.name.clone()).collect();
        names.sort();
        out.push((node.nid, names));
        for child in &node.children {
            snapshot_dirs(child, out);
        }
    }
}

/// Tail allocator: every appended blob (relocated dirs, new/changed file
/// data) is concatenated in allocation order, so the whole tail is one
/// contiguous Vec + base offset for PatchedReader.
struct TailCtx {
    block_size: usize,
    meta_blkaddr: u64,
    patches: Vec<(u64, Vec<u8>)>,
    tail: Vec<u8>,
    cursor: u64,
    next_nid: u64,
    new_inodes: u64,
    rewritten: HashSet<u64>,
}

impl TailCtx {
    fn meta_off(&self, nid: u64) -> u64 {
        self.meta_blkaddr * self.block_size as u64 + nid * 32
    }

    /// Append bytes block-aligned, return the byte offset.
    fn append(&mut self, data: &[u8]) -> u64 {
        let off = self.cursor;
        self.tail.extend_from_slice(data);
        let blocks = data.len().div_ceil(self.block_size) as u64;
        self.cursor += blocks * self.block_size as u64;
        let pad = self.cursor - off - data.len() as u64;
        self.tail.extend(std::iter::repeat(0).take(pad as usize));
        off
    }

    /// Fresh compact plain inode (same template as append_image), with an
    /// optional verbatim xattr area copy. Returns slots consumed (1 plain,
    /// more with xattrs) so the caller can stride NIDs accordingly.
    fn fresh_inode_patch(
        &mut self,
        nid: u64,
        mode: u16,
        size: u32,
        raw: u32,
        uid: u16,
        gid: u16,
        xattr_area: Option<&[u8]>,
    ) -> u64 {
        let mut inode = vec![0u8; 32];
        inode[4..6].copy_from_slice(&mode.to_le_bytes());
        inode[6..8].copy_from_slice(&1u16.to_le_bytes());
        inode[8..12].copy_from_slice(&size.to_le_bytes());
        inode[16..20].copy_from_slice(&raw.to_le_bytes());
        inode[20..24].copy_from_slice(&(nid as u32).to_le_bytes());
        inode[24..26].copy_from_slice(&uid.to_le_bytes());
        inode[26..28].copy_from_slice(&gid.to_le_bytes());
        let slots = if let Some(area) = xattr_area {
            // icount follows the (area-8)/4 on-disk convention.
            let icount = area.len().saturating_sub(8) / 4;
            inode[2..4].copy_from_slice(&(icount as u16).to_le_bytes());
            inode.extend_from_slice(area);
            ((32 + area.len() + 31) / 32) as u64
        } else {
            1
        };
        self.patches.push((self.meta_off(nid), inode));
        self.rewritten.insert(nid);
        slots
    }
}

/// Raw inline xattr area bytes of the original inode (empty when the inode
/// carries no xattrs). Used to carry preserved contexts onto fresh inodes.
fn source_xattr_area(source: &mut Source, nid: u64) -> Result<Vec<u8>, Error> {
    let off = source
        .meta_blkaddr
        .checked_mul(source.block_size)
        .and_then(|base| base.checked_add(nid.checked_mul(32)?))
        .ok_or_else(|| Error::invalid("EROFS inode offset overflow"))?;
    let mut header = [0u8; 64];
    source.reader.read_at(off, &mut header)?;
    let format = buf::u16le(&header, 0)?;
    let icount = buf::u16le(&header, 2)? as u64;
    // Shared area-size math lives in crate::fs (single formula).
    let area_len = crate::fs::erofs::xattr::area_size(icount)?;
    if area_len == 0 {
        return Ok(Vec::new());
    }
    let isize = if format & 1 == 1 { 64 } else { 32 };
    let area_len_usize = usize::try_from(area_len)
        .map_err(|_| Error::invalid("EROFS xattr area is too large"))?;
    let mut area = vec![0u8; area_len_usize];
    source.reader.read_at(
        off.checked_add(isize)
            .ok_or_else(|| Error::invalid("EROFS xattr offset overflow"))?,
        &mut area,
    )?;
    Ok(area)
}

/// Copy the original inode (32-byte compact or 64-byte extended) and patch
/// data fields, preserving xattr count, link count, timestamps and the
/// high UID/GID of extended inodes. Only the layout bits are switched to
/// plain: payload moves to freshly appended blocks.
fn repatch_inode(
    source: &mut Source,
    ctx: &mut TailCtx,
    nid: u64,
    mode: u16,
    size: u64,
    raw: u32,
    uid: u32,
    gid: u32,
) -> Result<(), Error> {
    let off = ctx.meta_off(nid);
    let mut orig = [0u8; 64];
    source.reader.read_at(off, &mut orig)?;
    let format = crate::core::buf::u16le(&orig, 0)?;
    let extended = format & 1 != 0;
    let mut inode = orig.to_vec();
    // Keep every format bit except the 3 layout bits -> plain (0).
    inode[0..2].copy_from_slice(&((format & !0xE) | (0 << 1)).to_le_bytes());
    inode[4..6].copy_from_slice(&mode.to_le_bytes());
    if extended {
        inode[8..16].copy_from_slice(&size.to_le_bytes());
        inode[16..20].copy_from_slice(&raw.to_le_bytes());
        inode[20..24].copy_from_slice(&(nid as u32).to_le_bytes());
        inode[24..28].copy_from_slice(&uid.to_le_bytes());
        inode[28..32].copy_from_slice(&gid.to_le_bytes());
        ctx.patches.push((off, inode));
    } else {
        let size32 = u32::try_from(size)
            .map_err(|_| Error::invalid("file is too large for a compact inode"))?;
        inode[8..12].copy_from_slice(&size32.to_le_bytes());
        inode[16..20].copy_from_slice(&raw.to_le_bytes());
        inode[20..24].copy_from_slice(&(nid as u32).to_le_bytes());
        inode[24..26].copy_from_slice(&(uid as u16).to_le_bytes());
        inode[26..28].copy_from_slice(&(gid as u16).to_le_bytes());
        ctx.patches.push((off, inode[..32].to_vec()));
    }
    ctx.rewritten.insert(nid);
    Ok(())
}

/// PHASE A: materialize brand-new files (nid == 0). Plain image copies
/// share the source blocks with zero copying; anything else is appended.
fn overlay_new_files(
    source: &mut Source,
    node: &mut Node,
    ctx: &mut TailCtx,
) -> Result<(), Error> {
    for child in node.children.iter_mut() {
        if child.nid == 0 {
            if child.is_dir() {
                // New host directories / directory copies: legacy path.
                return Err(overlay_fallback("new-dir"));
            }
            if child.context_explicit {
                // Fresh shared-table entries need the legacy layout.
                return Err(overlay_fallback("explicit-context"));
            }
            // Preserved contexts of image copies ride along verbatim
            // (shared IDs stay valid: the overlay never moves the table).
            let xattr_area = if child.original_nid == 0 {
                Vec::new()
            } else {
                source_xattr_area(source, child.original_nid)?
            };
            let (data, share_raw) = if child.original_nid == 0 {
                (Some(child.data().to_vec()), None)
            } else if child.layout == 0 {
                (None, Some(child.raw_block))
            } else {
                // Copy of a compressed/inline file: store decompressed plain.
                // Only the copied bytes bloat, never the whole image.
                (Some(source.data(child.original_nid)?.3), None)
            };
            let (raw, size) = match (&data, share_raw) {
                (Some(d), _) if !d.is_empty() => {
                    let off = ctx.append(d);
                    ((off / ctx.block_size as u64) as u32, d.len() as u32)
                }
                (None, Some(raw)) => (raw, child.data_size as u32),
                _ => (0, 0),
            };
            let fresh = ctx.next_nid;
            ctx.new_inodes += 1;
            let area = if xattr_area.is_empty() {
                None
            } else {
                Some(xattr_area.as_slice())
            };
            let slots = ctx.fresh_inode_patch(
                fresh,
                child.mode,
                size,
                raw,
                child.uid as u16,
                child.gid as u16,
                area,
            );
            ctx.next_nid += slots;
            child.nid = fresh;
            child.parent_nid = node.nid;
        }
        overlay_new_files(source, child, ctx)?;
    }
    Ok(())
}

/// PHASE B: replaced file content (host bytes or image bytes onto an
/// existing name). Untouched compressed files are never read.
fn overlay_replaced(
    source: &mut Source,
    node: &mut Node,
    ctx: &mut TailCtx,
) -> Result<(), Error> {
    if node.nid != 0 && node.content_changed {
        if node.is_dir() {
            return Err(overlay_fallback("replace-dir"));
        }
        if node.original_nid == 0 {
            // Host/stdin bytes replace whatever was there (any layout ->
            // plain, same as the append path).
            let data = node.data().to_vec();
            let (raw, size) = if data.is_empty() {
                (0, 0)
            } else {
                let off = ctx.append(&data);
                ((off / ctx.block_size as u64) as u32, data.len() as u32)
            };
            repatch_inode(source, ctx, node.nid, node.mode, size as u64, raw, node.uid, node.gid)?;
        } else if node.layout == 0 {
            // Image bytes onto an existing name: share the source blocks.
            repatch_inode(
                source,
                ctx,
                node.nid,
                node.mode,
                node.data_size,
                node.raw_block,
                node.uid,
                node.gid,
            )?;
        } else {
            // Compressed/inline source: plain-ify just this file.
            let data = source.data(node.original_nid)?.3;
            let off = ctx.append(&data);
            repatch_inode(
                source,
                ctx,
                node.nid,
                node.mode,
                data.len() as u64,
                (off / ctx.block_size as u64) as u32,
                node.uid,
                node.gid,
            )?;
        }
    }
    for child in node.children.iter_mut() {
        overlay_replaced(source, child, ctx)?;
    }
    Ok(())
}

/// PHASE C: rewrite directories whose membership changed (always relocate
/// to the tail: no "no free space" failure mode). Converts inline dirs to
/// plain, exactly like the append path does.
fn overlay_dirs(
    source: &mut Source,
    node: &mut Node,
    ctx: &mut TailCtx,
    before: &HashMap<u64, Vec<String>>,
) -> Result<(), Error> {
    if node.is_dir() && node.nid != 0 {
        let mut names: Vec<String> = node.children.iter().map(|c| c.name.clone()).collect();
        names.sort();
        match before.get(&node.nid) {
            None => return Err(overlay_fallback("dir-unknown")),
            Some(old) if *old != names => {
                let data = directory_bytes(node, ctx.block_size);
                if data.is_empty() {
                    return Err(overlay_fallback("dir-empty"));
                }
                let off = ctx.append(&data);
                repatch_inode(
                    source,
                    ctx,
                    node.nid,
                    node.mode,
                    data.len() as u64,
                    (off / ctx.block_size as u64) as u32,
                    node.uid,
                    node.gid,
                )?;
            }
            _ => {}
        }
    }
    for child in node.children.iter_mut() {
        overlay_dirs(source, child, ctx, before)?;
    }
    Ok(())
}

/// PHASE D: metadata-only changes (mode/uid/gid) on inodes that were not
/// already rewritten whole. Field widths follow the on-disk format bit.
fn overlay_meta(source: &mut Source, node: &Node, ctx: &mut TailCtx) -> Result<(), Error> {
    if node.nid != 0 && node.modified && !ctx.rewritten.contains(&node.nid) {
        let off = ctx.meta_off(node.nid);
        let mut format = [0u8; 2];
        source.reader.read_at(off, &mut format)?;
        ctx.patches.push((off + 4, node.mode.to_le_bytes().to_vec()));
        if u16::from_le_bytes(format) & 1 != 0 {
            ctx.patches.push((off + 24, node.uid.to_le_bytes().to_vec()));
            ctx.patches.push((off + 28, node.gid.to_le_bytes().to_vec()));
        } else {
            ctx.patches
                .push((off + 24, (node.uid as u16).to_le_bytes().to_vec()));
            ctx.patches
                .push((off + 26, (node.gid as u16).to_le_bytes().to_vec()));
        }
    }
    for child in &node.children {
        overlay_meta(source, child, ctx)?;
    }
    Ok(())
}

fn overlay_rebuild(
    source: &mut Source,
    root: &mut Node,
    output: &Path,
    sparse_output: bool,
    before: &[(u64, Vec<String>)],
) -> Result<(), Error> {
    sort_tree(root);
    let root_nid = root.nid;
    set_parents(root, root_nid);
    let block_size = usize::try_from(source.block_size)
        .map_err(|_| Error::invalid("invalid block size"))?;
    let original_size = source.reader.size();
    let tail_base = original_size.div_ceil(block_size as u64) * block_size as u64;
    let before_map: HashMap<u64, Vec<String>> =
        before.iter().cloned().collect();
    // Explicit SELinux contexts need fresh shared-table entries, which
    // only the legacy rebuild lays out: fall back instead of dropping them.
    // Preserved contexts need nothing and survive positionally.
    if root_has_explicit_context(&root) {
        return Err(Error::invalid(OVERLAY_FALLBACK));
    }
    let mut ctx = TailCtx {
        block_size,
        meta_blkaddr: source.meta_blkaddr,
        patches: Vec::new(),
        tail: Vec::new(),
        cursor: tail_base,
        next_nid: source.inodes,
        new_inodes: 0,
        rewritten: HashSet::new(),
    };
    overlay_new_files(source, root, &mut ctx)?;
    overlay_replaced(source, root, &mut ctx)?;
    overlay_dirs(source, root, &mut ctx, &before_map)?;
    overlay_meta(source, root, &mut ctx)?;

    // Superblock: same four patches as the append path.
    ctx.patches
        .push((1024 + 8, (source.feature_compat & !1).to_le_bytes().to_vec()));
    ctx.patches.push((
        1024 + 16,
        (source.inodes + ctx.new_inodes).to_le_bytes().to_vec(),
    ));
    let output_blocks = (ctx.cursor / block_size as u64) as u32;
    ctx.patches
        .push((1024 + 36, output_blocks.to_le_bytes().to_vec()));
    ctx.patches.push((1024 + 4, vec![0u8; 4]));
    let total_size = ctx.cursor;
    let mut patched = PatchedReader {
        source: &mut source.reader,
        original_size,
        patches: ctx.patches,
        tail_offset: tail_base,
        tail: ctx.tail,
    };
    write_patched_image(&mut patched, total_size, block_size, output, sparse_output)
}

/// True if any node carries an explicitly requested SELinux context
/// (`--context`). Preserved source contexts don't count: they need no
/// fresh table entries.
fn root_has_explicit_context(node: &Node) -> bool {
    node.context_explicit || node.children.iter().any(root_has_explicit_context)
}

/// True when the tree carries compressed-compact payloads (EROFS
/// sources built with compression). Drives the default `--compress lz4`
/// for rebuilds so they keep their size class.
fn tree_has_compressed(node: &Node) -> bool {
    use crate::fs::erofs::compress::LAYOUT_COMPRESSED_COMPACT;
    if node.layout == LAYOUT_COMPRESSED_COMPACT {
        return true;
    }
    node.children.iter().any(tree_has_compressed)
}

fn rebuild(
    mut source: Source,
    mut root: Node,
    output: &Path,
    sparse_output: bool,
    compress: crate::fs::erofs::compress::Algo,
) -> Result<(), Error> {
    sort_tree(&mut root);
    if compress.is_some() {
        return rebuild_compressed(source, root, output, sparse_output, compress);
    }
    // Shared SELinux table: one entry per unique context; inodes carrying
    // a context reserve a second 32-byte slot for the 16-byte area.
    let contexts = collect_contexts(&root);
    let (xattr_table, xattr_ids) = if contexts.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        crate::fs::erofs::xattr::pack_shared_table(&contexts)
    };
    let mut next = 1;
    assign_nids_strided(&mut root, &mut next, 1, source.meta_blkaddr, source.block_size);
    let mut list = Vec::new();
    nodes(&root, &mut list);
    let block_size = usize::try_from(source.block_size)
        .map_err(|_| Error::invalid("invalid block size"))?;
    let inode_blocks = align(next as usize * 32, block_size) / block_size;
    let meta_blkaddr = source.meta_blkaddr as u32;
    let mut alloc_block = meta_blkaddr + inode_blocks as u32;
    let xattr_blkaddr = if xattr_table.is_empty() {
        0
    } else {
        let base = alloc_block;
        alloc_block += xattr_table.len().div_ceil(block_size) as u32;
        base
    };
    let mut directory_data = Vec::new();
    for node in &list {
        if node.is_dir() {
            let data = directory_bytes(node, block_size);
            let allocated = data.len().div_ceil(block_size) as u32;
            directory_data.push((node.nid, data, alloc_block));
            alloc_block += allocated;
        }
    }
    let mut meta_patches: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut file_write_refs: Vec<FileWriteRef> = Vec::new();
    for node in &list {
        let offset = meta_blkaddr as u64 * block_size as u64 + node.nid as u64 * 32;
        let size = if node.is_dir() {
            directory_bytes(node, block_size).len()
        } else {
            node.data_size as usize
        };
        // NOTE: no block passthrough here by design. Referencing original
        // block numbers in a fresh layout lets later fresh writes stomp
        // them (proven data corruption); every file payload is copied
        // explicitly, streaming, to freshly allocated blocks.
        let data_block = if node.is_dir() {
            directory_data
                .iter()
                .find(|entry| entry.0 == node.nid)
                .map_or(0, |entry| entry.2)
        } else if size == 0 {
            0
        } else {
            let target = alloc_block;
            alloc_block += size.div_ceil(block_size) as u32;
            target
        };
        let mut inode = vec![0u8; 32];
        inode[0..2].copy_from_slice(&0u16.to_le_bytes());
        inode[4..6].copy_from_slice(&node.mode.to_le_bytes());
        inode[6..8].copy_from_slice(&1u16.to_le_bytes());
        inode[8..12].copy_from_slice(&(size as u32).to_le_bytes());
        inode[16..20].copy_from_slice(&data_block.to_le_bytes());
        inode[20..24].copy_from_slice(&(node.nid as u32).to_le_bytes());
        inode[24..26].copy_from_slice(&(node.uid as u16).to_le_bytes());
        inode[26..28].copy_from_slice(&(node.gid as u16).to_le_bytes());
        if let Some(ctx) = node.context.as_deref().filter(|c| !c.is_empty()) {
            // 16-byte shared-xattr area: 12-byte header + one entry ID.
            // icount=2 matches the (area-8)/4 on-disk convention.
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
        meta_patches.push((offset, inode));
        if node.is_dir() {
            if let Some((_, data, start)) = directory_data.iter().find(|entry| entry.0 == node.nid) {
                let destination = *start as u64 * block_size as u64;
                meta_patches.push((destination, data.clone()));
            }
        } else if size > 0 {
            let destination = data_block as u64 * block_size as u64;
            file_write_refs.push(FileWriteRef {
                destination,
                size: size as u64,
                nid: node.nid,
                original_nid: node.original_nid,
                modified: node.modified,
            });
        }
    }
    let total_size = alloc_block as u64 * block_size as u64;
    let sb = 1024u64;
    meta_patches.push((sb, MAGIC.to_le_bytes().to_vec()));
    meta_patches.push((sb + 8, (source.feature_compat & !1).to_le_bytes().to_vec()));
    meta_patches.push((sb + 12, vec![source.block_size.trailing_zeros() as u8]));
    meta_patches.push((sb + 14, (root.nid as u16).to_le_bytes().to_vec()));
    meta_patches.push((sb + 16, (list.len() as u64).to_le_bytes().to_vec()));
    meta_patches.push((sb + 24, source.build_time.to_le_bytes().to_vec()));
    meta_patches.push((sb + 32, source.build_nsec.to_le_bytes().to_vec()));
    meta_patches.push((sb + 36, alloc_block.to_le_bytes().to_vec()));
    meta_patches.push((sb + 40, meta_blkaddr.to_le_bytes()[..4].to_vec()));
    if !xattr_table.is_empty() {
        meta_patches.push((sb + 44, xattr_blkaddr.to_le_bytes().to_vec()));
        meta_patches.push((xattr_blkaddr as u64 * block_size as u64, xattr_table));
    }
    meta_patches.push((sb + 64, source.volume.to_vec()));
    meta_patches.push((sb + 80, source.feature_incompat.to_le_bytes().to_vec()));
    meta_patches.push((sb + 100, source.feature_ro_compat.to_le_bytes().to_vec()));
    if output == Path::new("-") {
        // Stream through a temp raw file (O(1) RAM) instead of buffering
        // every file payload in patches.
        let (_tmp, tmp_path) = crate::core::image::spool_temp("image-worker-rebuild")?;
        let result = emit_raw_file(
            &mut source,
            &root,
            meta_patches,
            &file_write_refs,
            total_size,
            block_size,
            &tmp_path,
        )
        .and_then(|_| {
            if sparse_output {
                crate::core::image::sparsify_file_to_stdout(&tmp_path, block_size)
            } else {
                stream_file_to_stdout(&tmp_path)
            }
        });
        let _ = std::fs::remove_file(&tmp_path);
        return result;
    } else {
        emit_raw_file(
            &mut source,
            &root,
            meta_patches,
            &file_write_refs,
            total_size,
            block_size,
            output,
        )?;
        if sparse_output {
            convert_raw_to_sparse(output, block_size)?;
        }
        Ok(())
    }
}

/// One file payload to materialize: host bytes are already in RAM
/// (user input), image files stream in 64 KiB quanta at emission.
struct FileWriteRef {
    destination: u64,
    size: u64,
    nid: u64,
    original_nid: u64,
    modified: bool,
}

// ── Compressed rebuild (EROFS, --compress lz4|deflate) ─────────────
// Fresh layout where every non-empty regular file is stored
// compressed-compact (1-2 aligned 4 KiB logical clusters per 4 KiB
// physical block, all-4B index, `advise = 0`). Incompressible spans
// fall back to PLAIN, so output never exceeds all-plain size.
//
// Layout: metadata (inodes + map headers + indexes + xattr table +
// directories) is reserved upfront — index *lengths* depend only on
// cluster counts — then file payloads stream compactly in cluster-pair
// batches (≤ ~512 KiB per batch, rayon-parallel compression, results
// written in order), recording each file's destination; inodes, index
// bytes and the superblock are seek-written as their inputs finalize.
// Peak RAM is one batch plus metadata, never a whole file or image.

/// Mark every non-empty regular file for compressed-compact output.
pub(crate) fn mark_compressed(node: &mut Node, block_size: u64) {
    if !node.is_dir() && node.data_size > 0 {
        node.erofs_compact_clusters = node.data_size.div_ceil(block_size);
    }
    for child in &mut node.children {
        mark_compressed(child, block_size);
    }
}

/// Cluster source for one compressed file: host bytes or image blocks.
enum ClusterSrc<'a> {
    Memory(&'a [u8]),
    Image(crate::fs::erofs::inode::Inode),
}

/// Build the per-cluster reader closure for an EROFS-backed file.
fn erofs_cluster_reader<'a>(
    source: &'a mut Source,
    src: &'a ClusterSrc<'a>,
    block_size: usize,
) -> impl FnMut(u64, &mut [u8]) -> Result<(), Error> + use<'a> {
    move |idx: u64, buf: &mut [u8]| {
        let off = idx
            .checked_mul(block_size as u64)
            .ok_or_else(|| Error::invalid("EROFS cluster offset overflow"))?;
        match src {
            ClusterSrc::Memory(data) => {
                let end = off as usize + buf.len();
                if end > data.len() {
                    return Err(Error::invalid("file node data is too short"));
                }
                buf.copy_from_slice(&data[off as usize..end]);
                Ok(())
            }
            ClusterSrc::Image(fi) => {
                let fs_sb = source.fs_superblock();
                let mut done = 0usize;
                while done < buf.len() {
                    let n = crate::fs::erofs::inode::read_range(
                        &fs_sb,
                        &mut source.reader,
                        fi,
                        off + done as u64,
                        &mut buf[done..],
                    )?;
                    if n == 0 {
                        return Err(Error::invalid("short EROFS source file read"));
                    }
                    done += n;
                }
                Ok(())
            }
        }
    }
}

/// Write one block of payload (data + zero pad to `block_size`).
pub(crate) fn write_padded_block(
    out: &mut File,
    dest: u64,
    data: &[u8],
    block_size: usize,
) -> Result<(), Error> {
    if data.len() > block_size {
        return Err(Error::invalid("compressed pcluster does not fit its block"));
    }
    out.seek(SeekFrom::Start(dest))?;
    out.write_all(data)?;
    let pad = block_size - data.len();
    if pad > 0 {
        out.write_all(&vec![0u8; pad])?;
    }
    Ok(())
}

/// Stream one file's payload compressed: cluster-pair batches
/// (rayon-parallel, order-preserving), payload blocks appended at
/// `*cursor`, index packs seek-written at `*index_off`. `read_cluster`
/// fills exactly one logical cluster (`min(block_size, tail)` bytes)
/// at absolute cluster index `lcn`. Returns the first payload block
/// (for the inode; 0 when the file is empty).
pub(crate) fn stream_compressed_file(
    out: &mut File,
    file_size: u64,
    block_size: usize,
    algo: crate::fs::erofs::compress::Algo,
    cursor: &mut u64,
    index_off: &mut u64,
    read_cluster: &mut dyn FnMut(u64, &mut [u8]) -> Result<(), Error>,
) -> Result<u64, Error> {
    use crate::fs::erofs::compress as cmp;
    let total = file_size.div_ceil(block_size as u64);
    if total == 0 {
        return Ok(0);
    }
    // Batches of whole pairs (even cluster counts) keep global pair
    // alignment, so index packs never straddle batch edges; the file
    // tail (possibly odd) is the only exception, handled by padding.
    // Pairing starts at cluster 0, so every batch starts even.
    let batch_pairs = ((512 * 1024) / block_size).clamp(1, 64);
    let batch_clusters = batch_pairs * cmp::MAX_CLUSTERS_PER_PCLUSTER;
    // Absolute index of `carry`, or of the next unread cluster.
    let mut lcn = 0u64;
    let mut carry: Option<Vec<u8>> = None;
    let mut first_block_out = 0u64;
    let mut wrote_any = false;
    while carry.is_some() || lcn < total {
        let mut clusters = Vec::new();
        if let Some(cluster) = carry.take() {
            clusters.push(cluster);
            lcn += 1;
        }
        let want = batch_clusters.saturating_sub(clusters.len());
        let rest = total.saturating_sub(lcn) as usize;
        let take = rest.min(want);
        for index in 0..take {
            let idx = lcn + index as u64;
            let off = idx
                .checked_mul(block_size as u64)
                .ok_or_else(|| Error::invalid("EROFS cluster offset overflow"))?;
            let len = (file_size - off).min(block_size as u64) as usize;
            let mut cluster = vec![0u8; len];
            read_cluster(idx, &mut cluster)?;
            clusters.push(cluster);
        }
        lcn += take as u64;
        if clusters.len() % 2 == 1 && lcn < total {
            carry = clusters.pop();
            lcn -= 1;
        }
        if clusters.is_empty() {
            break;
        }
        let pcl = cmp::pack_clusters(algo, block_size, &clusters);
        let block_base = *cursor / block_size as u64;
        let (index_bytes, _) = cmp::encode_pclusters(&pcl, block_base);
        out.seek(SeekFrom::Start(*index_off))?;
        out.write_all(&index_bytes)?;
        *index_off += index_bytes.len() as u64;
        if !wrote_any {
            first_block_out = block_base;
            wrote_any = true;
        }
        for p in &pcl {
            write_padded_block(out, *cursor, &p.payload, block_size)?;
            *cursor += block_size as u64;
        }
    }
    Ok(first_block_out)
}

fn rebuild_compressed(
    mut source: Source,
    mut root: Node,
    output: &Path,
    sparse_output: bool,
    algo: crate::fs::erofs::compress::Algo,
) -> Result<(), Error> {
    use crate::fs::erofs::compress as cmp;
    sort_tree(&mut root);
    let block_size = usize::try_from(source.block_size)
        .map_err(|_| Error::invalid("invalid block size"))?;
    let block_u64 = source.block_size;
    mark_compressed(&mut root, block_u64);
    let contexts = collect_contexts(&root);
    let (xattr_table, xattr_ids) = if contexts.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        crate::fs::erofs::xattr::pack_shared_table(&contexts)
    };
    let mut next = 1u64;
    assign_nids_strided(&mut root, &mut next, 1, source.meta_blkaddr, source.block_size);
    let mut list = Vec::new();
    nodes(&root, &mut list);
    let meta_blkaddr = source.meta_blkaddr;
    let inode_blocks = align(next as usize * 32, block_size) / block_size;
    let mut alloc_block = meta_blkaddr + inode_blocks as u64;
    let xattr_blkaddr = if xattr_table.is_empty() {
        0
    } else {
        let base = alloc_block;
        alloc_block += xattr_table.len().div_ceil(block_size) as u64;
        base
    };
    // Directory blobs (offsets known before any payload streams).
    let mut dir_data: Vec<(u64, Vec<u8>, u64)> = Vec::new();
    for node in &list {
        if node.is_dir() {
            let data = directory_bytes(node, block_size);
            if data.is_empty() {
                return Err(Error::invalid("directory does not fit into blocks"));
            }
            let blocks = data.len().div_ceil(block_size) as u64;
            dir_data.push((node.nid, data, alloc_block));
            alloc_block += blocks;
        }
    }
    // Inode images and per-file stream state (destinations filled while
    // payloads stream, then seek-written).
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
        let inode_off = meta_blkaddr * block_u64 + node.nid * 32;
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
    // Plain files (empty) and directory inodes carry their final bytes
    // already; compressed payload destinations resolve while streaming.
    let to_stdout = output == Path::new("-");
    enum RawTarget {
        Direct(std::path::PathBuf),
        Temp(std::path::PathBuf),
    }
    let raw_target = if !to_stdout && !sparse_output {
        RawTarget::Direct(output.to_path_buf())
    } else {
        let (_tmp, path) = crate::core::image::spool_temp("image-worker-cmp")?;
        drop(_tmp);
        RawTarget::Temp(path)
    };
    let raw_path = match &raw_target {
        RawTarget::Direct(p) | RawTarget::Temp(p) => p.clone(),
    };
    let final_blocks = {
        let mut out = File::create(&raw_path)?;
        // Metadata with known offsets first.
        let mut sb = [0u8; 128];
        sb[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        sb[12] = block_u64.trailing_zeros() as u8;
        sb[14..16].copy_from_slice(&(root.nid as u16).to_le_bytes());
        sb[16..24].copy_from_slice(&(list.len() as u64).to_le_bytes());
        // Blocks/xattr addresses finalize below; write a placeholder now
        // so the region exists, then seek-rewrite at the end.
        out.seek(SeekFrom::Start(1024))?;
        out.write_all(&sb)?;
        if !xattr_table.is_empty() {
            out.seek(SeekFrom::Start(xattr_blkaddr * block_u64))?;
            out.write_all(&xattr_table)?;
        }
        for (off, inode) in &inodes {
            // Compressed files get their map header now; the index bytes
            // stream in with the payload (offsets were reserved above).
            out.seek(SeekFrom::Start(*off))?;
            out.write_all(inode)?;
        }
        for (_, data, start) in &dir_data {
            out.seek(SeekFrom::Start(*start * block_u64))?;
            out.write_all(data)?;
        }
        // Payload phase: each compressed file streams compactly.
        let by_nid: HashMap<u64, &Node> =
            list.iter().map(|n| (n.nid, *n)).collect();
        let mut cursor = alloc_block * block_u64;
        for file in pending.iter_mut() {
            let node = by_nid.get(&file.nid).copied().ok_or_else(|| {
                Error::invalid("file node vanished during rebuild")
            })?;
            // Map header (8B) at the reserved data offset.
            out.seek(SeekFrom::Start(file.data_off))?;
            out.write_all(&cmp::map_header(algo))?;
            let src = match &node.payload {
                NodePayload::Memory(data) => ClusterSrc::Memory(data),
                NodePayload::InlineDir(_) => {
                    return Err(Error::invalid("directory reached file writer"));
                }
                NodePayload::None => {
                    let fs_sb = source.fs_superblock();
                    let fi = crate::fs::erofs::inode::read(
                        &fs_sb,
                        &mut source.reader,
                        node.original_nid,
                    )?;
                    ClusterSrc::Image(fi)
                }
            };
            let mut index_off = file.index_off;
            let mut reader = erofs_cluster_reader(&mut source, &src, block_size);
            stream_compressed_file(
                &mut out,
                file.size,
                block_size,
                algo,
                &mut cursor,
                &mut index_off,
                &mut reader,
            )?;
            file.index_off = index_off;
            // Inode data pointer for layout 3 lives in the map area, not
            // in the inode body (body u32 stays 0): nothing more to patch
            // except the size/nid, already written above.
        }
        // Index reservation check: every file must fit its packs.
        for file in &pending {
            let want = file.index_off;
            let have = file.data_off + 8 + cmp::index_len(file.clusters) as u64;
            if want > have {
                return Err(Error::invalid("compressed index overran its reservation"));
            }
        }
        let total_blocks = cursor.div_ceil(block_u64);
        // Finalize: superblock block count + xattr address + timestamps.
        let build_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.seek(SeekFrom::Start(1024))?;
        let mut sb = [0u8; 128];
        sb[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        sb[8..12].copy_from_slice(&(source.feature_compat & !1).to_le_bytes());
        sb[12] = block_u64.trailing_zeros() as u8;
        sb[14..16].copy_from_slice(&(root.nid as u16).to_le_bytes());
        sb[16..24].copy_from_slice(&(list.len() as u64).to_le_bytes());
        sb[24..32].copy_from_slice(&build_time.to_le_bytes());
        sb[36..40].copy_from_slice(&(total_blocks as u32).to_le_bytes());
        sb[40..44].copy_from_slice(&(meta_blkaddr as u32).to_le_bytes());
        if !xattr_table.is_empty() {
            sb[44..48].copy_from_slice(&(xattr_blkaddr as u32).to_le_bytes());
        }
        sb[64..80].copy_from_slice(&source.volume);
        // Compressed outputs clear incompat features: no zero-padding,
        // no big pclusters, no compr_cfgs table in this writer.
        sb[80..84].copy_from_slice(&0u32.to_le_bytes());
        sb[100..104].copy_from_slice(&source.feature_ro_compat.to_le_bytes());
        out.write_all(&sb)?;
        out.seek(SeekFrom::Start(cursor))?;
        out.set_len(cursor)?;
        out.flush()?;
        total_blocks
    };
    let _ = final_blocks;
    match raw_target {
        RawTarget::Direct(_) => {
            if sparse_output {
                convert_raw_to_sparse(&raw_path, block_size)?;
            }
            Ok(())
        }
        RawTarget::Temp(path) => {
            let result = if sparse_output {
                if to_stdout {
                    crate::core::image::sparsify_file_to_stdout(&path, block_size)
                } else {
                    let mut raw = File::open(&path)?;
                    let (runs, blocks) =
                        crate::core::image::scan_sparse_runs(&mut raw, block_size)?;
                    let mut sparse = File::create(output)?;
                    let mut header = [0u8; 28];
                    header[0..4].copy_from_slice(&SPARSE_MAGIC.to_le_bytes());
                    header[4..6].copy_from_slice(&1u16.to_le_bytes());
                    header[8..10].copy_from_slice(&28u16.to_le_bytes());
                    header[10..12].copy_from_slice(&12u16.to_le_bytes());
                    header[12..16].copy_from_slice(&(block_size as u32).to_le_bytes());
                    header[16..20].copy_from_slice(&(blocks as u32).to_le_bytes());
                    header[20..24].copy_from_slice(&(runs.len() as u32).to_le_bytes());
                    sparse.write_all(&header)?;
                    crate::core::image::emit_sparse_runs(&mut raw, block_size, &runs, &mut sparse)?;
                    sparse.flush()?;
                    Ok(())
                }
            } else {
                stream_file_to_stdout(&path)
            };
            let _ = std::fs::remove_file(&path);
            result
        }
    }
}

/// Stream one source-image file into the output at `dest` (64 KiB
/// quanta; compressed clusters decode on demand, never whole files).
fn stream_source_file(
    source: &mut Source,
    original_nid: u64,
    out: &mut File,
    dest: u64,
    size: u64,
) -> Result<(), Error> {
    let fs_sb = source.fs_superblock();
    let fi = crate::fs::erofs::inode::read(&fs_sb, &mut source.reader, original_nid)?;
    let mut buf = vec![0u8; crate::fs::erofs::inode::STREAM_CHUNK];
    let mut done = 0u64;
    while done < size.min(fi.size) {
        let n = crate::fs::erofs::inode::read_range(
            &fs_sb,
            &mut source.reader,
            &fi,
            done,
            &mut buf,
        )?;
        if n == 0 {
            break;
        }
        out.seek(SeekFrom::Start(dest + done))?;
        out.write_all(&buf[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// Stream a finished raw file to stdout (used for `-` output).
fn stream_file_to_stdout(path: &Path) -> Result<(), Error> {
    let mut file = File::open(path)?;
    let stdout = io::stdout();
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

/// Materialize the rebuilt raw image into `out_path`: stream the patched
/// base (meta only), then each file job — host bytes directly, image
/// files streamed in 64 KiB quanta. Only metadata lives in RAM.
fn emit_raw_file(
    source: &mut Source,
    root: &Node,
    meta_patches: Vec<(u64, Vec<u8>)>,
    file_write_refs: &[FileWriteRef],
    total_size: u64,
    block_size: usize,
    out_path: &Path,
) -> Result<(), Error> {
    let original_size = source.reader.size();
    let mut out = File::create(out_path)?;
    out.set_len(total_size)?;
    {
        let mut patched = PatchedReader {
            source: &mut source.reader,
            original_size,
            patches: meta_patches,
            tail_offset: original_size,
            tail: Vec::new(),
        };
        let mut buffer = vec![0u8; block_size];
        let blocks = total_size.div_ceil(block_size as u64) as usize;
        for i in 0..blocks {
            patched.read_at((i * block_size) as u64, &mut buffer)?;
            out.write_all(&buffer)?;
        }
    }
    for fwr in file_write_refs {
        if fwr.modified || fwr.original_nid == 0 {
            let data = find_node(root, fwr.nid)
                .ok_or_else(|| Error::invalid("file node vanished"))?
                .data();
            out.seek(SeekFrom::Start(fwr.destination))?;
            out.write_all(data)?;
        } else {
            stream_source_file(source, fwr.original_nid, &mut out, fwr.destination, fwr.size)?;
        }
    }
    out.flush()?;
    Ok(())
}

fn convert_raw_to_sparse(path: &Path, block_size: usize) -> Result<(), Error> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let blocks = (file_size / block_size as u64) as usize;
    let mut buffer = vec![0u8; block_size];
    let mut chunks: Vec<(u16, usize, usize)> = Vec::new();
    let mut index = 0;
    while index < blocks {
        file.seek(SeekFrom::Start((index * block_size) as u64))?;
        file.read_exact(&mut buffer)?;
        let kind = if buffer.iter().all(|byte| *byte == 0) {
            0xcac3
        } else if buffer.chunks_exact(4).all(|part| part == &buffer[..4]) {
            0xcac2
        } else {
            0xcac1
        };
        let start = index;
        index += 1;
        while index < blocks {
            file.seek(SeekFrom::Start((index * block_size) as u64))?;
            file.read_exact(&mut buffer)?;
            let same = match kind {
                0xcac3 => buffer.iter().all(|byte| *byte == 0),
                0xcac2 => buffer.chunks_exact(4).all(|part| part == &buffer[..4]),
                _ => false,
            };
            if !same {
                break;
            }
            index += 1;
        }
        chunks.push((kind, start, index - start));
    }
    let tmp_path = path.with_extension("sparse_tmp");
    {
        let mut sparse = File::create(&tmp_path)?;
        let mut header = [0u8; 28];
        header[0..4].copy_from_slice(&SPARSE_MAGIC.to_le_bytes());
        header[4..6].copy_from_slice(&1u16.to_le_bytes());
        header[8..10].copy_from_slice(&28u16.to_le_bytes());
        header[10..12].copy_from_slice(&12u16.to_le_bytes());
        header[12..16].copy_from_slice(&(block_size as u32).to_le_bytes());
        header[16..20].copy_from_slice(&(blocks as u32).to_le_bytes());
        header[20..24].copy_from_slice(&(chunks.len() as u32).to_le_bytes());
        sparse.write_all(&header)?;
        for (kind, start, count) in chunks {
            let payload = match kind {
                0xcac1 => count * block_size,
                0xcac2 => 4,
                _ => 0,
            };
            let total = 12 + payload;
            sparse.write_all(&kind.to_le_bytes())?;
            sparse.write_all(&0u16.to_le_bytes())?;
            sparse.write_all(&(count as u32).to_le_bytes())?;
            sparse.write_all(&(total as u32).to_le_bytes())?;
            match kind {
                0xcac1 => {
                    let mut buf = vec![0u8; block_size];
                    for i in 0..count {
                        file.seek(SeekFrom::Start(((start + i) * block_size) as u64))?;
                        file.read_exact(&mut buf)?;
                        sparse.write_all(&buf)?;
                    }
                }
                0xcac2 => {
                    file.seek(SeekFrom::Start((start * block_size) as u64))?;
                    file.read_exact(&mut buffer)?;
                    sparse.write_all(&buffer[..4])?;
                }
                _ => {}
            }
        }
    }
    drop(file);
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct Metadata {
    pub(crate) mode: Option<u16>,
    pub(crate) uid: Option<u32>,
    pub(crate) gid: Option<u32>,
    pub(crate) context: Option<String>,
}
pub(crate) enum Action {
    Add {
        source: String,
        destination: String,
        metadata: Metadata,
    },
    Copy {
        source: String,
        destination: String,
        metadata: Metadata,
    },
    Move {
        source: String,
        destination: String,
        metadata: Metadata,
    },
    Remove {
        path: String,
        recursive: bool,
    },
    SetContext {
        path: String,
        context: String,
    },
    SetMode {
        path: String,
        mode: u16,
    },
    SetOwner {
        path: String,
        uid: u32,
        gid: u32,
    },
}
struct Config {
    input: String,
    output: String,
    sparse_output: bool,
    shared_blocks: Option<bool>,
    /// Guaranteed free space in the rebuilt ext4 image, in MiB.
    reserve_mb: u64,
    /// Shrink ext4 output to the minimal block-group count (also implied
    /// by any `--rm` action). No-op for EROFS (always minimal).
    compact: bool,
    /// EROFS output compression (fresh compressed-compact files).
    compress: crate::fs::erofs::compress::Algo,
    /// True when `--compress` was passed explicitly (matters for target
    /// validation: implicit defaults never trigger "applies to X only").
    compress_explicit: bool,
    /// Pure format conversion target. Forbids file actions.
    convert_to: Option<FsTarget>,
    actions: Vec<Action>,
}

/// Filesystem selected by `--convert-to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsTarget {
    Erofs,
    Ext4,
}
fn parse_mode(value: &str) -> Result<u16, Error> {
    u16::from_str_radix(value.trim_start_matches("0o"), 8)
        .map_err(|_| Error::Invalid(format!("invalid mode: {value}")))
}
/// Parse a `--reserve-mb` size: bare number or K/M/G suffix, in MiB.
/// `64`, `64M` and `64m` all mean 64 MiB; `1G` = 1024 MiB.
fn parse_reserve(value: &str) -> Result<u64, Error> {
    let (number, factor) = match value.strip_suffix(['K', 'k']) {
        Some(n) => (n, 0u64),
        None => match value.strip_suffix(['M', 'm']) {
            Some(n) => (n, 1u64),
            None => match value.strip_suffix(['G', 'g']) {
                Some(n) => (n, 1024u64),
                None => (value, 1u64),
            },
        },
    };
    let number: u64 = number
        .parse()
        .map_err(|_| Error::invalid(format!("invalid reserve size: {value} (try 64M)")))?;
    // K rounds up to whole MiB so the reserve never silently becomes 0.
    Ok(if factor == 0 {
        number.div_ceil(1024)
    } else {
        number.saturating_mul(factor)
    })
}
fn parse_owner(value: &str) -> Result<(u32, u32), Error> {
    let (uid, gid) = value
        .split_once(':')
        .ok_or_else(|| Error::Invalid("owner must be uid:gid".into()))?;
    Ok((
        uid.parse()
            .map_err(|_| Error::Invalid("invalid uid".into()))?,
        gid.parse()
            .map_err(|_| Error::Invalid("invalid gid".into()))?,
    ))
}
fn action_metadata(action: &mut Action) -> Result<&mut Metadata, Error> {
    match action {
        Action::Add { metadata, .. }
        | Action::Copy { metadata, .. }
        | Action::Move { metadata, .. } => Ok(metadata),
        _ => Err(Error::invalid(
            "--mode/--uid/--gid/--context apply to a file action (--add/--cp/--mv) only",
        )),
    }
}
fn help() {
    println!(
        r#"image-worker write - edit images and convert between formats

USAGE
    image-worker write <input> <output> ACTION... [OPTIONS]
    image-worker write --input <input> --output <output> ACTION... [OPTIONS]
    image-worker write <input> <output> --convert-to <erofs|ext4> [OPTIONS]

INPUT/OUTPUT
    --input <path>          Input image; '-' means stdin (spooled to temp file).
    --output <path>         Output image; '-' means stdout.
    Positional <input> <output> and '- -' remain supported.

SUPPORTED FORMATS
    Containers (auto-detected on input, selected on output):
        raw                 Plain block image, directly mountable.
        Android sparse      simg container (CHUNK_RAW/FILL/DONT_CARE/CRC32).
                            --sparse-output selects it; default is raw.
                            A sparse container must be expanded (simg2img)
                            before a kernel mount.
    Filesystems on input (auto-detected):
        EROFS               plain, inline and compressed-compact layouts;
                            LZ4, DEFLATE and ZSTD payloads.
        ext4                extent-based files, classic 32-byte group
                            descriptors. Rejected: journal, 64bit,
                            metadata_csum, encryption, unknown features.
    Filesystems on output:
        EROFS               Fresh inodes are compact plain (+ inline tails
                            for dirs), or compressed-compact (LZ4/Deflate
                            pclusters, 4 KiB logical clusters packed 1-2
                            per 4 KiB block, all-4B index) when --compress
                            selects an algorithm. The overlay/delta path
                            keeps every untouched compressed payload
                            byte-identical in place (fast path, no growth);
                            the legacy full rebuild recompresses touched
                            files (parallel, bounded RAM) and stores
                            incompressible spans plain.
                            Inodes carrying a SELinux context reserve a
                            second 32-byte slot for a 16-byte shared-xattr
                            area; unique contexts pack into a shared table
                            (xattr_blkaddr in the superblock). Explicit
                            --context routes through the full rebuild.
        ext4                Full copy-on-write rebuild: multi-group extent
                            inodes (+1 level of extent tree), directories,
                            inline symlinks, modes, uid/gid and SELinux
                            contexts. Special device nodes are not supported.
                            The filesystem grows by whole block groups when
                            the input is full, and shrinks to the minimal
                            group count with --compact (implied by --rm).

ACTIONS (edit mode; forbidden with --convert-to)
    --add, -A <host> <dst>  Add or replace a host file at an image path.
                            Use '-' as source to read from stdin.
                            Replacing keeps the old mode/uid/gid/context
                            unless --mode/--uid/--gid/--context override them.
    --cp, -C <src> <dst>    Copy an image path (plain data shared by blocks).
    --mv, -M <src> <dst>    Move/rename an image path (keeps inode identity,
                            compressed data never moves).
    --rm <path>             Remove a file (or an empty directory).
    -R/-r <path>            Remove a path recursively (whole subtree).
    --compact               Shrink ext4 output to the minimal block-group
                            count (implied by any --rm). No-op for EROFS,
                            whose rebuilds are always minimal.
    --compress <a>          EROFS output compression: lz4 (fast, default for
                            ext4->erofs converts and for rebuilds of
                            compressed sources), deflate (denser, slower)
                            or none (plain). ext4 targets reject it.
    --set-mode <path> <m>   Set permission bits, for example 0755.
    --set-owner <path> <u:g> Set numeric uid and gid.
    --set-context <path> <c> Set SELinux context metadata.
    --mode/--uid/--gid/--context
                            Metadata for the preceding file action.
    --sparse-output         Write Android sparse output; default is raw.
    --shared-blocks <y|n>   Enable/disable shared blocks for ext4 output:
                            identical data blocks are deduplicated, zero
                            blocks become holes. Default: auto-detect from
                            the source image. ext4 only.
    --shader-blocks         Alias for --shared-blocks.
    --reserve-mb <size>     Guarantee at least this much free space in the
                            rebuilt ext4 image, e.g. 64, 64M, 1G, 512K
                            (bare number = MiB). ext4 only. Without it the
                            output is packed with a small block-group margin.

CONVERT MODE (no file actions; pure repacker)
    --convert-to <erofs|ext4>
                            Re-encode the image into another filesystem.
                            Same filesystem  -> container-only conversion
                            (payload untouched, streaming).
                            erofs -> ext4     -> fresh ext4 rebuild
                            (modes/uid/gid/sizes/names/SELinux contexts
                            preserved and verified; directories get fresh
                            sizes, lost+found is ext4-only).
                            ext4  -> erofs    -> fresh all-plain EROFS image
                            (inputs stream through 64 KiB quanta; SELinux
                            contexts pack into a shared xattr table, special
                            files are skipped with a warning; lost+found is
                            dropped).
                            --sparse-output/--shared-blocks/--reserve-mb
                            apply to ext4 targets as usual; --shared-blocks
                            and --reserve-mb are rejected for EROFS targets.

EXAMPLES
    image-worker write vendor.img result.img --add README.md /etc/README.md --mode 0644
    image-worker write --input - --output - --add README.md /etc/README.md > result.img
    image-worker write vendor.img result.img --add a /etc/a --cp /etc/a /system/a
    cat vendor.sparse.img | image-worker write - - --add README.md /etc/README.md --sparse-output > result.sparse.img

    # Read from stdin (pipe from other commands)
    image-worker read --cat vendor.img /etc/fstab.qcom | sed 's/foo/bar/' | \
        image-worker write vendor.img result.img --add - /etc/fstab.qcom --mode 0644

    # Reserve 256 MiB free in a packed ext4 image
    image-worker write ext4.img out.img --add f /etc/f --reserve-mb 256

    # Convert between formats (no file actions)
    image-worker write vendor_erofs.img vendor_ext4.img --convert-to ext4
    image-worker write vendor_ext4.img vendor_erofs.img --convert-to erofs
    image-worker write vendor.sparse.img vendor.raw.img --convert-to erofs
    image-worker write vendor.raw.img vendor.sparse.img --convert-to erofs --sparse-output

The complete action queue is parsed before image processing. Input stdin is
spooled to a temporary seekable file. Sparse output is a container and is not
directly mountable by the kernel."#
    );
}
fn parse_args(args: &[String]) -> Result<Config, Error> {
    let mut positional = Vec::new();
    let mut input = None;
    let mut output = None;
    let mut sparse_output = false;
    let mut shared_blocks = None;
    let mut reserve_mb: Option<u64> = None;
    let mut compact = false;
    let mut compress = crate::fs::erofs::compress::Algo::None;
    let mut compress_explicit = false;
    let mut convert_to: Option<FsTarget> = None;
    let mut actions = Vec::new();
    let mut pending = Metadata::default();
    // NOTE: ported repack parse_args started at index 1 to skip argv[0].
    // Here `run()` receives args without the program/subcommand name, so start at 0.
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                help();
                return Err(Error::Invalid("help requested".into()));
            }
            "--sparse-output" => sparse_output = true,
            "--compact" => compact = true,
            "--compress" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    Error::Invalid("--compress requires lz4, deflate or none".into())
                })?;
                compress = crate::fs::erofs::compress::Algo::parse(value)?;
                compress_explicit = true;
            }
            "--convert-to" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    Error::Invalid("--convert-to requires erofs or ext4".into())
                })?;
                convert_to = Some(match value.to_ascii_lowercase().as_str() {
                    "erofs" => FsTarget::Erofs,
                    "ext4" => FsTarget::Ext4,
                    other => {
                        return Err(Error::Invalid(format!(
                            "invalid --convert-to value: {other} (use erofs or ext4)"
                        )))
                    }
                });
            }
            "--reserve-mb" | "--reserve" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    Error::Invalid("--reserve-mb requires a size like 64M".into())
                })?;
                reserve_mb = Some(parse_reserve(value)?);
            }
            "--shared-blocks" | "--shader-blocks" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    Error::Invalid("--shared-blocks requires y or n".into())
                })?;
                match value.as_str() {
                    "y" | "yes" | "true" => shared_blocks = Some(true),
                    "n" | "no" | "false" => shared_blocks = Some(false),
                    other => return Err(Error::Invalid(format!(
                        "invalid --shared-blocks value: {other} (use y/n)"
                    ))),
                }
            }
            "--input" => {
                index += 1;
                input = Some(
                    args.get(index)
                        .ok_or_else(|| Error::Invalid("--input requires a path".into()))?
                        .clone(),
                );
            }
            "--output" => {
                index += 1;
                output = Some(
                    args.get(index)
                        .ok_or_else(|| Error::Invalid("--output requires a path".into()))?
                        .clone(),
                );
            }
            "--add" => {
                actions.push(Action::Add {
                    source: args
                        .get(index + 1)
                        .ok_or_else(|| {
                            Error::Invalid("--add requires source and destination".into())
                        })?
                        .clone(),
                    destination: args
                        .get(index + 2)
                        .ok_or_else(|| {
                            Error::Invalid("--add requires source and destination".into())
                        })?
                        .clone(),
                    metadata: pending.clone(),
                });
                pending = Metadata::default();
                index += 2;
            }
            "-A" | "--cp" | "-C" | "--mv" | "-M" => {
                let source = args
                    .get(index + 1)
                    .ok_or_else(|| Error::Invalid("action requires source and destination".into()))?
                    .clone();
                let destination = args
                    .get(index + 2)
                    .ok_or_else(|| Error::Invalid("action requires source and destination".into()))?
                    .clone();
                let action = match args[index].as_str() {
                    "-A" => Action::Add {
                        source,
                        destination,
                        metadata: pending.clone(),
                    },
                    "--cp" | "-C" => Action::Copy {
                        source,
                        destination,
                        metadata: pending.clone(),
                    },
                    _ => Action::Move {
                        source,
                        destination,
                        metadata: pending.clone(),
                    },
                };
                actions.push(action);
                pending = Metadata::default();
                index += 2;
            }
            "--set-context" => {
                actions.push(Action::SetContext {
                    path: args
                        .get(index + 1)
                        .ok_or_else(|| {
                            Error::Invalid("--set-context requires path and context".into())
                        })?
                        .clone(),
                    context: args
                        .get(index + 2)
                        .ok_or_else(|| {
                            Error::Invalid("--set-context requires path and context".into())
                        })?
                        .clone(),
                });
                index += 2;
            }
            "--set-mode" => {
                actions.push(Action::SetMode {
                    path: args
                        .get(index + 1)
                        .ok_or_else(|| Error::Invalid("--set-mode requires path and mode".into()))?
                        .clone(),
                    mode: parse_mode(args.get(index + 2).ok_or_else(|| {
                        Error::Invalid("--set-mode requires path and mode".into())
                    })?)?,
                });
                index += 2;
            }
            "--set-owner" => {
                let (uid, gid) = parse_owner(args.get(index + 2).ok_or_else(|| {
                    Error::Invalid("--set-owner requires path and uid:gid".into())
                })?)?;
                actions.push(Action::SetOwner {
                    path: args
                        .get(index + 1)
                        .ok_or_else(|| {
                            Error::Invalid("--set-owner requires path and uid:gid".into())
                        })?
                        .clone(),
                    uid,
                    gid,
                });
                index += 2;
            }
            "--rm" => {
                actions.push(Action::Remove {
                    path: args
                        .get(index + 1)
                        .ok_or_else(|| Error::Invalid("--rm requires a path".into()))?
                        .clone(),
                    recursive: false,
                });
                index += 1;
            }
            "-R" | "-r" => {
                actions.push(Action::Remove {
                    path: args
                        .get(index + 1)
                        .ok_or_else(|| Error::Invalid("-R requires a path".into()))?
                        .clone(),
                    recursive: true,
                });
                index += 1;
            }
            "--mode" => {
                pending.mode =
                    Some(parse_mode(args.get(index + 1).ok_or_else(|| {
                        Error::Invalid("--mode requires value".into())
                    })?)?);
                if let Some(action) = actions.last_mut() {
                    action_metadata(action)?.mode = pending.mode;
                    pending.mode = None;
                }
                index += 1;
            }
            "--uid" => {
                pending.uid = Some(
                    args.get(index + 1)
                        .ok_or_else(|| Error::Invalid("--uid requires value".into()))?
                        .parse()
                        .map_err(|_| Error::Invalid("invalid uid".into()))?,
                );
                if let Some(action) = actions.last_mut() {
                    action_metadata(action)?.uid = pending.uid;
                    pending.uid = None;
                }
                index += 1;
            }
            "--gid" => {
                pending.gid = Some(
                    args.get(index + 1)
                        .ok_or_else(|| Error::Invalid("--gid requires value".into()))?
                        .parse()
                        .map_err(|_| Error::Invalid("invalid gid".into()))?,
                );
                if let Some(action) = actions.last_mut() {
                    action_metadata(action)?.gid = pending.gid;
                    pending.gid = None;
                }
                index += 1;
            }
            "--context" => {
                pending.context = Some(
                    args.get(index + 1)
                        .ok_or_else(|| Error::Invalid("--context requires value".into()))?
                        .clone(),
                );
                if let Some(action) = actions.last_mut() {
                    action_metadata(action)?.context = pending.context.clone();
                    pending.context = None;
                }
                index += 1;
            }
            "-" => positional.push("-".to_owned()),
            value if value.starts_with('-') => {
                return Err(Error::Invalid(format!("unknown option: {value}")));
            }
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    if input.is_none() && positional.len() > 0 {
        input = Some(positional.remove(0));
    }
    if output.is_none() && positional.len() > 0 {
        output = Some(positional.remove(0));
    }
    if !positional.is_empty() || input.is_none() || output.is_none() {
        return Err(Error::Invalid(
            "invalid arguments; use --help for usage".into(),
        ));
    }
    // Pure converter mode: no file actions, no dangling per-action metadata.
    if convert_to.is_some()
        && (!actions.is_empty()
            || pending.mode.is_some()
            || pending.uid.is_some()
            || pending.gid.is_some()
            || pending.context.is_some())
    {
        return Err(Error::Invalid(
            "--convert-to does not take file actions; convert first, then edit".into(),
        ));
    }
    if convert_to.is_none() && actions.is_empty() {
        // Bare repack requests (dedupe and/or shrink without file edits)
        // are valid; anything else without actions is a usage error.
        if shared_blocks.is_none() && !compact {
            return Err(Error::Invalid(
                "invalid arguments; use --help for usage".into(),
            ));
        }
    }
    Ok(Config {
        input: input.ok_or_else(|| {
            Error::Invalid("invalid arguments; use --help for usage".into())
        })?,
        output: output.ok_or_else(|| {
            Error::Invalid("invalid arguments; use --help for usage".into())
        })?,
        sparse_output,
        shared_blocks,
        reserve_mb: reserve_mb.unwrap_or(0),
        compact,
        compress,
        compress_explicit,
        convert_to,
        actions,
    })
}
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let config = match parse_args(&args) {
        Ok(config) => config,
        Err(Error::Invalid(message)) if message == "help requested" => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let stdin_path = if config.input == "-" {
        let path = env::temp_dir().join(format!(
            "image-worker-write-{}-{}.input",
            std::process::id(),
            source_time()
        ));
        let mut file = File::create(&path)?;
        let mut stdin = io::stdin().lock();
        io::copy(&mut stdin, &mut file)?;
        Some(path)
    } else {
        None
    };
    let input_path = stdin_path
        .as_deref()
        .unwrap_or_else(|| Path::new(&config.input));
    let mut format_reader = Image::open(input_path)?;
    // Single shared probe (see crate::fs): EROFS first, then ext4.
    let is_ext4 = crate::fs::probe_fs(&mut format_reader)? == crate::fs::FsKind::Ext4;
    if let Some(target) = config.convert_to {
        if config.reserve_mb > 0 && target == FsTarget::Erofs {
            return Err(Error::invalid("--reserve-mb applies to ext4 output only").into());
        }
        if config.shared_blocks.is_some() && target == FsTarget::Erofs {
            return Err(Error::invalid("--shared-blocks applies to ext4 output only").into());
        }
        if config.compress_explicit && config.compress.is_some() && target == FsTarget::Ext4 {
            return Err(Error::invalid("--compress applies to EROFS output only").into());
        }
        // Default compression: lz4 when producing EROFS from ext4 (plain
        // sources have no payload worth keeping as-is).
        let compress = if target == FsTarget::Erofs
            && !config.compress_explicit
            && is_ext4
        {
            crate::fs::erofs::compress::Algo::Lz4
        } else {
            config.compress
        };
        let result = convert::run(
            input_path,
            Path::new(&config.output),
            is_ext4,
            target,
            config.sparse_output,
            config.shared_blocks,
            config.reserve_mb,
            compress,
        );
        if let Some(path) = stdin_path {
            let _ = std::fs::remove_file(path);
        }
        return result.map_err(|error| Box::new(error) as Box<dyn std::error::Error>);
    }
    if is_ext4 {
        if config.compress_explicit && config.compress.is_some() {
            return Err(Error::invalid("--compress applies to EROFS output only").into());
        }
        // Removals shrink the image: compact sizing is implied.
        let compact = config.compact
            || config
                .actions
                .iter()
                .any(|action| matches!(action, Action::Remove { .. }));
        ext4::run(
            input_path,
            Path::new(&config.output),
            config.sparse_output,
            config.shared_blocks,
            config.reserve_mb,
            compact,
            config.actions,
        )?;
        if let Some(path) = stdin_path {
            let _ = std::fs::remove_file(path);
        }
        return Ok(());
    }
    if config.reserve_mb > 0 {
        return Err(Error::invalid("--reserve-mb applies to ext4 output only").into());
    }
    let mut source = Source::open(input_path)?;
    let mut root = source.tree(source.root_nid, String::new())?;
    // Default compression for EROFS rebuilds: lz4 when the source already
    // carries compressed payloads (keeps the size class instead of
    // decompressing everything into plain blocks).
    let compress = if config.compress_explicit {
        config.compress
    } else if tree_has_compressed(&root) {
        crate::fs::erofs::compress::Algo::Lz4
    } else {
        crate::fs::erofs::compress::Algo::None
    };
    let single_add =
        config.actions.len() == 1 && matches!(config.actions.first(), Some(Action::Add { .. }));
    let mut has_file_actions = false;
    let mut has_remove = false;
    let mut metadata_only = true;
    for action in &config.actions {
        match action {
            Action::Add { .. } | Action::Copy { .. } | Action::Move { .. } => {
                has_file_actions = true;
                metadata_only = false;
            }
            Action::Remove { .. } => {
                has_file_actions = true;
                has_remove = true;
                metadata_only = false;
            }
            Action::SetMode { .. } | Action::SetOwner { .. } | Action::SetContext { .. } => {}
        }
    }
    // Directory membership snapshot for the overlay writer. Only needed for
    // multi-action queues (single adds go through append_image, pure
    // metadata through patch_inode_metadata).
    let before = if !single_add && has_file_actions {
        let mut snapshot = Vec::new();
        snapshot_dirs(&root, &mut snapshot);
        Some(snapshot)
    } else {
        None
    };
    let mut _structural = false;
    for action in config.actions {
        _structural |= apply_action(&mut root, action)?;
    }
    if metadata_only && !has_file_actions {
        if root_has_explicit_context(&root) {
            // The patch path cannot serialize fresh xattrs: full rebuild.
            let path = source.path.clone();
            rebuild(Source::open(&path)?, root, Path::new(&config.output), config.sparse_output, compress)?;
            if let Some(path) = stdin_path {
                let _ = std::fs::remove_file(path);
            }
            return Ok(());
        }
        patch_inode_metadata(&mut source, &root, Path::new(&config.output))?;
        if let Some(path) = stdin_path {
            let _ = std::fs::remove_file(path);
        }
        return Ok(());
    }
    // Removals must shrink the image and explicit compression must apply
    // to every file: both route straight to the full rebuild, skipping
    // the append/overlay fast paths (which only ever grow the tail).
    if has_remove || (config.compress_explicit && compress.is_some()) {
        let path = source.path.clone();
        rebuild(Source::open(&path)?, root, Path::new(&config.output), config.sparse_output, compress)?;
        if let Some(path) = stdin_path {
            let _ = std::fs::remove_file(path);
        }
        return Ok(());
    }
    let mut appended = false;
    if single_add {
        if append_image(
            &mut source,
            root.clone(),
            Path::new(&config.output),
            config.sparse_output,
        ).is_ok() {
            appended = true;
        }
    }
    if !appended {
        if single_add {
            // Append was attempted and failed; the tree is already mutated,
            // so no overlay snapshot exists: straight to legacy rebuild.
            let path = source.path.clone();
            rebuild(Source::open(&path)?, root, Path::new(&config.output), config.sparse_output, compress)?;
        } else {
            let before = before.as_ref().ok_or_else(|| {
                Error::invalid("internal error: missing overlay snapshot")
            })?;
            match overlay_rebuild(
                &mut source,
                &mut root,
                Path::new(&config.output),
                config.sparse_output,
                before,
            ) {
                Ok(()) => {}
                Err(Error::Invalid(message)) if message == OVERLAY_FALLBACK => {
                    let path = source.path.clone();
                    rebuild(Source::open(&path)?, root, Path::new(&config.output), config.sparse_output, compress)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    if let Some(path) = stdin_path {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn source_time() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}
