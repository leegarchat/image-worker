use super::Superblock;
use super::inode::Inode;
use crate::core::{Error, Image};
use std::io::Read;

const CLUSTER_TYPE_PLAIN: u8 = 0;
const CLUSTER_TYPE_HEAD1: u8 = 1;
const CLUSTER_TYPE_NONHEAD: u8 = 2;
const CLUSTER_TYPE_HEAD2: u8 = 3;

pub(crate) struct CompactEntry {
    pub(crate) lcn: u64,
    pub(crate) kind: u8,
    pub(crate) value: u32,
    pub(crate) pblk: u64,
}

/// Decoded index entry, shared by full and streaming readers.
pub(crate) struct CompactIndex {
    pub(crate) logical_block_size: u64,
    pub(crate) entries: Vec<CompactEntry>,
}

/// Upper bound for a single decompressed pcluster (real clusters are
/// ≤512 KiB at 4 KiB blocks; anything larger is corruption).
const MAX_PCLUSTER: usize = 64 * 1024 * 1024;

/// Compute the compact index. NOTE: entry encoding (2b vs 4b packing)
/// depends on the TOTAL cluster count, so the index is always computed
/// whole; prefix/range readers save only *decode* I/O, never index
/// correctness. Index reads are tiny (4B) and sequential-ish.
pub(crate) fn compact_index(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
) -> Result<CompactIndex, Error> {
    let logical_block_size = fs
        .block_size
        .checked_shl(inode.cluster_bits as u32)
        .ok_or_else(|| Error::invalid("invalid EROFS cluster size"))?;
    let total = inode.size.div_ceil(logical_block_size);
    let entries = compact_entries(fs, image, inode, total)?;
    Ok(CompactIndex {
        logical_block_size,
        entries,
    })
}

fn compact_entries(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    total: u64,
) -> Result<Vec<CompactEntry>, Error> {
    let index_base = inode.data_offset + 8;
    let compacted_2b = inode.advise & 1 != 0;
    let initial_4b = if compacted_2b {
        let aligned = (32 - (index_base % 32)) / 4;
        if aligned == 8 { 0 } else { aligned }
    } else {
        0
    };
    let middle_2b = if compacted_2b && initial_4b < total {
        (total - initial_4b) / 16 * 16
    } else {
        0
    };
    let mut entries: Vec<CompactEntry> = Vec::new();
    for lcn in 0..total {
        let (position, entry_size) = if lcn < initial_4b {
            (index_base + lcn * 4, 4)
        } else if lcn - initial_4b < middle_2b {
            (index_base + initial_4b * 4 + (lcn - initial_4b) * 2, 2)
        } else {
            (
                index_base + initial_4b * 4 + middle_2b * 2 + (lcn - initial_4b - middle_2b) * 4,
                4,
            )
        };
        let pack_size = if entry_size == 4 { 8 } else { 32 };
        let pack_base = position / pack_size * pack_size;
        let entry_index = (position - pack_base) / entry_size;
        let encoded_bits = if entry_size == 4 { 16 } else { 14 };
        let mut packed = [0u8; 4];
        image.read_at(pack_base + (entry_index * encoded_bits) / 8, &mut packed)?;
        let bit_shift = (entry_index * encoded_bits) % 8;
        let raw = u32::from_le_bytes(packed) >> bit_shift;
        let low_bits = fs
            .block_bits()
            .saturating_add(inode.cluster_bits as u32)
            .max(12);
        // Guard against absurd cluster configurations on corrupt images
        // (`1u32 << low_bits` would panic for low_bits >= 32).
        if low_bits > 31 {
            return Err(Error::invalid("invalid EROFS cluster configuration"));
        }
        let low_mask = (1u32 << low_bits) - 1;
        let value = raw & low_mask;
        let kind = ((raw >> low_bits) & 3) as u8;
        let mut trailer = [0u8; 4];
        image.read_at(pack_base + pack_size - 4, &mut trailer)?;
        let anchor = u32::from_le_bytes(trailer) as u64;
        let mut pblk = anchor;
        if kind != CLUSTER_TYPE_NONHEAD {
            let big_pcluster = inode.advise & 2 != 0;
            let pack_first_lcn = lcn
                .checked_sub(entry_index)
                .ok_or_else(|| Error::invalid("invalid EROFS compact index position"))?;
            let mut preceding = entry_index as i64;
            let mut blocks = if big_pcluster { 0 } else { 1 };
            while preceding > 0 {
                preceding -= 1;
                let previous_lcn = pack_first_lcn + preceding as u64;
                let previous = entries.get(previous_lcn as usize).ok_or_else(|| {
                    Error::invalid("invalid EROFS compact index")
                })?;
                if previous.kind == CLUSTER_TYPE_NONHEAD {
                    if big_pcluster && previous.value & 0x800 != 0 {
                        blocks += u64::from(previous.value & !0x800);
                        preceding -= 1;
                        continue;
                    } else {
                        preceding -= previous.value as i64;
                    }
                    continue;
                }
                if preceding >= 0 {
                    blocks += 1;
                }
            }
            pblk = pblk
                .checked_add(blocks)
                .ok_or_else(|| Error::invalid("EROFS physical block overflow"))?;
        }
        entries.push(CompactEntry {
            lcn,
            kind,
            value,
            pblk,
        });
    }
    Ok(entries)
}

pub fn read_compact_data(
    fs: &Superblock,
    image: &mut Image,
    inode: &Inode,
    output: &mut [u8],
) -> Result<(), Error> {
    let index = compact_index(fs, image, inode)?;
    let mut physical_cursor = None;
    for (seg_index, entry) in index.entries.iter().enumerate() {
        let logical_start = entry
            .lcn
            .checked_mul(index.logical_block_size)
            .and_then(|o| o.checked_add(entry.value as u64))
            .ok_or_else(|| Error::invalid("EROFS logical offset overflow"))?;
        let physical = physical_cursor.unwrap_or(entry.pblk);
        if entry.kind == CLUSTER_TYPE_PLAIN {
            if logical_start >= inode.size {
                continue;
            }
            let logical_end = index
                .entries
                .get(seg_index + 1)
                .map(|next| next.lcn * index.logical_block_size + next.value as u64)
                .unwrap_or(inode.size)
                .min(inode.size);
            let copy_size = logical_end.saturating_sub(logical_start);
            let copy_size = usize::try_from(copy_size)
                .map_err(|_| Error::invalid("EROFS extent is too large"))?;
            let output_offset = usize::try_from(logical_start)
                .map_err(|_| Error::invalid("EROFS output offset is too large"))?;
            image.read_at(
                physical * fs.block_size,
                &mut output[output_offset..output_offset + copy_size],
            )?;
            physical_cursor = Some(physical + 1);
            continue;
        }
        if entry.kind != CLUSTER_TYPE_HEAD1 && entry.kind != CLUSTER_TYPE_HEAD2 {
            continue;
        }
        let start = logical_start;
        let end = index.entries[seg_index + 1..]
            .iter()
            .find(|next| next.kind != CLUSTER_TYPE_NONHEAD)
            .map(|next| next.lcn * index.logical_block_size + next.value as u64)
            .unwrap_or(inode.size);
        if end <= start || end > inode.size {
            return Err(Error::invalid("invalid EROFS compressed extent"));
        }
        let decoded_size = usize::try_from(end - start)
            .map_err(|_| Error::invalid("EROFS extent is too large"))?;
        if decoded_size > MAX_PCLUSTER {
            return Err(Error::invalid("EROFS compressed extent is too large"));
        }
        let compressed_blocks = index.entries[seg_index + 1..]
            .iter()
            .take_while(|next| next.kind == CLUSTER_TYPE_NONHEAD)
            .find_map(|next| (next.value & 0x800 != 0).then_some(next.value & !0x800))
            .unwrap_or(1)
            .max(1);
        // Sanity: a pcluster spans a handful of blocks, never gigabytes.
        if compressed_blocks > 8192 {
            return Err(Error::invalid("EROFS compressed extent is too large"));
        }
        let physical_size = fs.block_size as usize * compressed_blocks as usize;
        // Bound the input read: real pcluster payloads are ~KiBs.
        if physical_size > 256 * 1024 * 1024 {
            return Err(Error::invalid("EROFS compressed extent is too large"));
        }
        let mut compressed = vec![0u8; physical_size];
        image.read_at(physical * fs.block_size, &mut compressed)?;
        let decoded = decompress(
            &compressed,
            decoded_size,
            match entry.kind {
                CLUSTER_TYPE_HEAD1 => inode.algorithm_type & 0x0f,
                CLUSTER_TYPE_HEAD2 => inode.algorithm_type >> 4,
                _ => 0,
            },
            fs.feature_incompat & 1 != 0,
        )?;
        let start =
            usize::try_from(start).map_err(|_| Error::invalid("EROFS output offset is too large"))?;
        output[start..start + decoded.len()].copy_from_slice(&decoded);
        physical_cursor = Some(physical + compressed_blocks as u64);
    }
    Ok(())
}

pub fn decompress(
    input: &[u8],
    output_size: usize,
    algorithm: u8,
    zero_padding: bool,
) -> Result<Vec<u8>, Error> {
    let input = if zero_padding {
        let start = input
            .iter()
            .position(|b| *b != 0)
            .ok_or_else(|| Error::invalid("EROFS compressed stream contains only padding"))?;
        &input[start..]
    } else {
        input
    };
    match algorithm {
        0 => lz4_decode(input, output_size),
        2 => {
            let data = miniz_oxide::inflate::decompress_to_vec_with_limit(input, output_size)
                .map_err(|e| Error::invalid(format!("invalid EROFS DEFLATE: {e:?}")))?;
            if data.len() != output_size {
                return Err(Error::invalid("EROFS DEFLATE output size mismatch"));
            }
            Ok(data)
        }
        3 => {
            let decoder = ruzstd::decoding::StreamingDecoder::new(std::io::Cursor::new(input))
                .map_err(|e| Error::invalid(format!("invalid EROFS ZSTD: {e:?}")))?;
            // Bounded: a corrupt stream must error, never balloon RAM.
            let mut data = Vec::new();
            decoder
                .take(output_size as u64 + 1)
                .read_to_end(&mut data)?;
            if data.len() != output_size {
                return Err(Error::invalid("EROFS ZSTD output size mismatch"));
            }
            Ok(data)
        }
        1 => Err(Error::invalid("EROFS LZMA is not supported by this build")),
        other => Err(Error::invalid(format!(
            "unsupported EROFS compression algorithm {other}"
        ))),
    }
}

/// One resolved output segment: either a PLAIN passthrough run or a HEAD
/// cluster to decode. `physical` is cursor-chained like the full reader.
pub(crate) struct Segment {
    pub(crate) kind: u8,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) physical: u64,
    pub(crate) compressed_blocks: u64,
    pub(crate) algorithm: u8,
}

/// Resolve the index into ordered output segments (skips NONHEAD fillers).
pub(crate) fn segments(
    index: &CompactIndex,
    inode: &Inode,
) -> Result<Vec<Segment>, Error> {
    let lbs = index.logical_block_size;
    let mut out = Vec::new();
    let mut physical_cursor = None;
    for (seg_index, entry) in index.entries.iter().enumerate() {
        let logical_start = entry
            .lcn
            .checked_mul(lbs)
            .and_then(|o| o.checked_add(entry.value as u64))
            .ok_or_else(|| Error::invalid("EROFS logical offset overflow"))?;
        let physical = physical_cursor.unwrap_or(entry.pblk);
        if entry.kind == CLUSTER_TYPE_PLAIN {
            if logical_start >= inode.size {
                continue;
            }
            let logical_end = index
                .entries
                .get(seg_index + 1)
                .map(|next| next.lcn * lbs + next.value as u64)
                .unwrap_or(inode.size)
                .min(inode.size);
            if logical_end > logical_start {
                out.push(Segment {
                    kind: CLUSTER_TYPE_PLAIN,
                    start: logical_start,
                    end: logical_end,
                    physical,
                    compressed_blocks: 0,
                    algorithm: 0,
                });
            }
            physical_cursor = Some(physical + 1);
            continue;
        }
        if entry.kind != CLUSTER_TYPE_HEAD1 && entry.kind != CLUSTER_TYPE_HEAD2 {
            continue;
        }
        let start = logical_start;
        let end = index.entries[seg_index + 1..]
            .iter()
            .find(|next| next.kind != CLUSTER_TYPE_NONHEAD)
            .map(|next| next.lcn * lbs + next.value as u64)
            .unwrap_or(inode.size);
        if end <= start || end > inode.size {
            return Err(Error::invalid("invalid EROFS compressed extent"));
        }
        if end - start > MAX_PCLUSTER as u64 {
            return Err(Error::invalid("EROFS compressed extent is too large"));
        }
        let compressed_blocks = index.entries[seg_index + 1..]
            .iter()
            .take_while(|next| next.kind == CLUSTER_TYPE_NONHEAD)
            .find_map(|next| (next.value & 0x800 != 0).then_some(next.value & !0x800))
            .unwrap_or(1)
            .max(1);
        // Sanity: a pcluster spans a handful of blocks, never gigabytes.
        if compressed_blocks > 8192 {
            return Err(Error::invalid("EROFS compressed extent is too large"));
        }
        out.push(Segment {
            kind: entry.kind,
            start,
            end,
            physical,
            compressed_blocks: compressed_blocks as u64,
            algorithm: match entry.kind {
                CLUSTER_TYPE_HEAD1 => inode.algorithm_type & 0x0f,
                _ => inode.algorithm_type >> 4,
            },
        });
        physical_cursor = Some(physical + compressed_blocks as u64);
    }
    Ok(out)
}

/// Decode (or passthrough-copy) one segment into `out`, filling up to the
/// segment end. Returns bytes written. Peak RAM is one pcluster + `out`.
pub(crate) fn read_segment(
    fs: &Superblock,
    image: &mut Image,
    seg: &Segment,
    out: &mut [u8],
) -> Result<usize, Error> {
    let want = ((seg.end - seg.start) as usize).min(out.len());
    if seg.kind == CLUSTER_TYPE_PLAIN {
        image.read_at(seg.physical * fs.block_size, &mut out[..want])?;
        return Ok(want);
    }
    let physical_size = fs.block_size as usize * seg.compressed_blocks as usize;
    if physical_size > 256 * 1024 * 1024 {
        return Err(Error::invalid("EROFS compressed extent is too large"));
    }
    let mut compressed = vec![0u8; physical_size];
    image.read_at(seg.physical * fs.block_size, &mut compressed)?;
    let decoded = decompress(
        &compressed,
        (seg.end - seg.start) as usize,
        seg.algorithm,
        fs.feature_incompat & 1 != 0,
    )?;
    let take = want.min(decoded.len());
    out[..take].copy_from_slice(&decoded[..take]);
    Ok(take)
}

pub fn lz4_decode(input: &[u8], output_size: usize) -> Result<Vec<u8>, Error> {    let mut output = Vec::with_capacity(output_size);
    let mut cursor = 0usize;
    while output.len() < output_size {
        let token = *input
            .get(cursor)
            .ok_or_else(|| Error::invalid("truncated EROFS LZ4 token"))?;
        cursor += 1;
        let mut literal_len = (token >> 4) as usize;
        if literal_len == 15 {
            loop {
                let value = *input
                    .get(cursor)
                    .ok_or_else(|| Error::invalid("truncated EROFS LZ4 literal length"))?
                    as usize;
                cursor += 1;
                literal_len += value;
                if value != 255 {
                    break;
                }
            }
        }
        let literal_end = cursor
            .checked_add(literal_len)
            .ok_or_else(|| Error::invalid("EROFS LZ4 literal overflow"))?;
        if literal_end > input.len() || output.len() + literal_len > output_size {
            return Err(Error::invalid("invalid EROFS LZ4 literals"));
        }
        output.extend_from_slice(&input[cursor..literal_end]);
        cursor = literal_end;
        if output.len() == output_size {
            break;
        }
        let offset = u16::from_le_bytes([
            *input
                .get(cursor)
                .ok_or_else(|| Error::invalid("truncated EROFS LZ4 offset"))?,
            *input
                .get(cursor + 1)
                .ok_or_else(|| Error::invalid("truncated EROFS LZ4 offset"))?,
        ]) as usize;
        cursor += 2;
        if offset == 0 || offset > output.len() {
            return Err(Error::invalid(format!(
                "invalid EROFS LZ4 match offset: offset={offset} output={} cursor={cursor}",
                output.len()
            )));
        }
        let mut match_len = (token & 0x0f) as usize + 4;
        if token & 0x0f == 15 {
            loop {
                let value = *input
                    .get(cursor)
                    .ok_or_else(|| Error::invalid("truncated EROFS LZ4 match length"))?
                    as usize;
                cursor += 1;
                match_len += value;
                if value != 255 {
                    break;
                }
            }
        }
        if output.len() + match_len > output_size {
            match_len = output_size - output.len();
        }
        let match_start = output.len() - offset;
        for index in 0..match_len {
            let value = output[match_start + index];
            output.push(value);
        }
    }
    Ok(output)
}

// ── Encode side (writer) ────────────────────────────────────────────
// Compact-cluster compressor for fresh images. Layout contract (matches
// the reader above and mkfs.erofs ground truth from vendor fixtures):
//   - `cluster_bits = 0`: one logical cluster is one filesystem block.
//   - pclusters are 1-2 consecutive *aligned* logical clusters (~4-8 KiB
//     of input) packed into a single 4 KiB physical block when the
//     compressed span is strictly smaller than the input span.
//   - index entries are all 4-byte style (`advise = 0`: no 2B packing, no
//     big pclusters), kinds are HEAD1(1) / PLAIN(0), all values are 0,
//     NONHEAD(2) fillers carry sequential 1..N-1 positions (mkfs style).
//   - every pcluster occupies exactly one physical block (1:1 for HEAD,
//     1:1 per cluster for PLAIN runs); anchors are `first_block - 1`.
// Incompressible spans fall back to PLAIN automatically, so enabling the
// compressor never grows an image beyond its all-plain size (plus ~2
// bytes of index per logical cluster in metadata).

/// On-disk writer layout for fresh compressed inodes.
pub(crate) const LAYOUT_COMPRESSED_COMPACT: u16 = 3;
/// Compressed-fragment map header advise: all-4B index, no big pclusters.
pub(crate) const MAP_ADVISE_PLAIN4B: u16 = 0;
/// Max logical clusters per pcluster (2 clusters = 8 KiB input max).
pub(crate) const MAX_CLUSTERS_PER_PCLUSTER: usize = 2;

/// Compression algorithm selected by `--compress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Algo {
    #[default]
    None,
    Lz4,
    Deflate,
}

impl Algo {
    pub(crate) fn parse(value: &str) -> Result<Self, Error> {
        match value.to_ascii_lowercase().as_str() {
            "none" | "off" | "no" => Ok(Self::None),
            "lz4" => Ok(Self::Lz4),
            "deflate" | "zlib" => Ok(Self::Deflate),
            other => Err(Error::invalid(format!(
                "invalid --compress value: {other} (use lz4|deflate|none)"
            ))),
        }
    }

    /// Low-nibble on-disk algorithm id (HEAD1; HEAD2 is never emitted and
    /// its nibble is written as 0, mkfs style).
    pub(crate) fn nibble(self) -> u8 {
        match self {
            Self::None | Self::Lz4 => 0,
            Self::Deflate => 2,
        }
    }

    pub(crate) fn is_some(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Compress one pcluster span. Returns `None` when compression does not
/// pay off (output must be strictly smaller than the input and fit into
/// one filesystem block); the caller then stores the span PLAIN.
pub(crate) fn compress_pcluster(algo: Algo, input: &[u8]) -> Option<Vec<u8>> {
    if input.is_empty() {
        return None;
    }
    let compressed = match algo {
        Algo::None => return None,
        // Raw LZ4 block format (no size header): exactly what the reader
        // above decodes, verified by round-trip tests.
        Algo::Lz4 => lz4_flex::block::compress(input),
        // Raw DEFLATE stream (no zlib wrapper): matches both the reader
        // (`decompress_to_vec_with_limit` = raw inflate) and the vendor
        // `deflate.img` fixture payloads.
        Algo::Deflate => miniz_oxide::deflate::compress_to_vec(input, 6),
    };
    if compressed.len() < input.len() {
        Some(compressed)
    } else {
        None
    }
}

/// One emitted pcluster: either a HEAD group (1-2 logical clusters in one
/// physical block) or a single PLAIN cluster (one physical block).
pub(crate) struct Pcluster {
    pub(crate) head: bool,
    pub(crate) n_clusters: u32,
    pub(crate) payload: Vec<u8>,
}

/// Group an ordered cluster run (each ≤ `block_size`, only the last may
/// be short) into pclusters. Pairing is positional (`(0,1)`, `(2,3)`, …)
/// so every pair is independent and compression runs in parallel via
/// rayon; results keep input order. Pure function, no I/O.
pub(crate) fn pack_clusters(algo: Algo, block_size: usize, clusters: &[Vec<u8>]) -> Vec<Pcluster> {
    use rayon::prelude::*;
    let pairs: Vec<&[Vec<u8>]> = clusters
        .chunks(MAX_CLUSTERS_PER_PCLUSTER)
        .collect();
    pairs
        .par_iter()
        .flat_map_iter(|pair| pack_pair(algo, block_size, pair))
        .collect()
}

/// Pack one aligned pair (1-2 clusters) without any cross-pair state.
fn pack_pair(algo: Algo, block_size: usize, pair: &[Vec<u8>]) -> Vec<Pcluster> {
    // Fast path: whole pair as one pcluster.
    let mut span = Vec::with_capacity(block_size * MAX_CLUSTERS_PER_PCLUSTER);
    for cluster in pair.iter() {
        span.extend_from_slice(cluster);
    }
    if span.len() <= block_size * MAX_CLUSTERS_PER_PCLUSTER {
        if let Some(compressed) = compress_pcluster(algo, &span) {
            if compressed.len() <= block_size {
                return vec![Pcluster {
                    head: true,
                    n_clusters: pair.len() as u32,
                    payload: compressed,
                }];
            }
        }
    }
    if pair.len() == 1 {
        return vec![Pcluster {
            head: false,
            n_clusters: 1,
            payload: span,
        }];
    }
    // Pair did not pay off: try each cluster alone, else PLAIN.
    let mut out = Vec::with_capacity(2);
    for cluster in pair.iter() {
        if let Some(compressed) = compress_pcluster(algo, cluster) {
            if compressed.len() <= block_size {
                out.push(Pcluster {
                    head: true,
                    n_clusters: 1,
                    payload: compressed,
                });
                continue;
            }
        }
        out.push(Pcluster {
            head: false,
            n_clusters: 1,
            payload: cluster.clone(),
        });
    }
    out
}

/// Index entries `(kind, value)` for an ordered pcluster list. HEAD
/// groups emit HEAD1 + sequential NONHEAD fillers, PLAIN clusters emit
/// one PLAIN entry each; every value is 0 except filler positions.
pub(crate) fn index_entries(pclusters: &[Pcluster]) -> Vec<(u8, u16)> {
    let mut entries = Vec::new();
    for pcl in pclusters {
        if pcl.head {
            entries.push((CLUSTER_TYPE_HEAD1, 0));
            for filler in 1..pcl.n_clusters {
                entries.push((CLUSTER_TYPE_NONHEAD, filler as u16));
            }
        } else {
            entries.push((CLUSTER_TYPE_PLAIN, 0));
        }
    }
    entries
}

/// Byte length of the all-4B compact index for `total` logical clusters:
/// entries pair up into 8-byte packs (2×u16 + u32 anchor).
pub(crate) fn index_len(total_clusters: u64) -> usize {
    total_clusters.div_ceil(2) as usize * 8
}

/// Encode an ordered pcluster run into the all-4B compact index and
/// count its payload blocks. `first_block` is the physical block of the
/// first pcluster; payload blocks must be consecutive. Every pack anchor
/// is `first_block_of_pack - 1` (non-big pclusters: a HEAD group of N
/// clusters takes 1 block, PLAIN entries take 1 block each).
/// Streaming-safe: callers split files on pair boundaries (even entry
/// counts), so packs never straddle the slice edges except for the final
/// odd tail, which is padded here.
pub(crate) fn encode_pclusters(pclusters: &[Pcluster], first_block: u64) -> (Vec<u8>, u64) {
    let entries = index_entries(pclusters);
    // Physical block per index entry: a HEAD group shares one block for
    // all its entries, PLAIN entries take one block each.
    let mut blocks: Vec<u64> = Vec::with_capacity(entries.len());
    let mut cursor = first_block;
    let mut payload_blocks = 0u64;
    for pcl in pclusters {
        if pcl.head {
            for _ in 0..pcl.n_clusters {
                blocks.push(cursor);
            }
            cursor += 1;
            payload_blocks += 1;
        } else {
            for _ in 0..pcl.n_clusters {
                blocks.push(cursor);
                cursor += 1;
                payload_blocks += 1;
            }
        }
    }
    let mut out = Vec::with_capacity(index_len(entries.len() as u64));
    for (pack_no, pack) in entries.chunks(2).enumerate() {
        for entry in pack.iter() {
            let encoded = (u32::from(entry.0) << 12) | u32::from(entry.1);
            out.extend_from_slice(&(encoded as u16).to_le_bytes());
        }
        if pack.len() == 1 {
            out.extend_from_slice(&0u16.to_le_bytes());
        }
        let anchor = blocks
            .get(pack_no * 2)
            .copied()
            .unwrap_or(first_block)
            .saturating_sub(1) as u32;
        out.extend_from_slice(&anchor.to_le_bytes());
    }
    (out, payload_blocks)
}

/// 8-byte compressed-fragment map header: reserved(4) + advise(2) +
/// algorithm byte + cluster bits.
pub(crate) fn map_header(algo: Algo) -> [u8; 8] {
    let mut header = [0u8; 8];
    header[4..6].copy_from_slice(&MAP_ADVISE_PLAIN4B.to_le_bytes());
    header[6] = algo.nibble();
    header[7] = 0;
    header
}

#[cfg(test)]
mod encode_tests {

    #[test]
    fn deflate_small_sizes_roundtrip() {
        // Regression: tiny single-cluster files (init .rc scripts) must
        // survive a padded-block deflate round trip, not decode as empty.
        let words = b"on boot\n    write /sys/test value\n# comment\n";
        for len in [50usize, 200, 1000, 3000, 4000, 4095, 4096, 5000, 8192] {
            let mut input = Vec::with_capacity(len);
            while input.len() < len {
                let take = (len - input.len()).min(words.len());
                input.extend_from_slice(&words[..take]);
            }
            let Some(compressed) = compress_pcluster(Algo::Deflate, &input) else {
                continue;
            };
            let mut block = vec![0u8; 4096];
            assert!(compressed.len() <= 4096, "len={len} clen={}", compressed.len());
            block[..compressed.len()].copy_from_slice(&compressed);
            let decoded = decompress(&block, input.len(), 2, false)
                .expect("deflate decode must succeed");
            assert_eq!(decoded, input, "mismatch len={len}");
        }
    }

    use super::*;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn lz4_pcluster_roundtrip() {
        let input = pattern(8192);
        let compressed = compress_pcluster(Algo::Lz4, &input).expect("must compress");
        assert!(compressed.len() < input.len());
        // Writer pads the payload block with zeros; the reader is fed the
        // whole padded block and must still decode exactly.
        let mut block = vec![0u8; 4096];
        block[..compressed.len()].copy_from_slice(&compressed);
        let decoded = decompress(&block, input.len(), 0, false).expect("lz4 decode");
        assert_eq!(decoded, input);
    }

    #[test]
    fn deflate_pcluster_roundtrip() {
        let input = pattern(8192);
        let compressed = compress_pcluster(Algo::Deflate, &input).expect("must compress");
        assert!(compressed.len() < input.len());
        let mut block = vec![0u8; 4096];
        block[..compressed.len()].copy_from_slice(&compressed);
        let decoded = decompress(&block, input.len(), 2, false).expect("deflate decode");
        assert_eq!(decoded, input);
    }

    #[test]
    fn incompressible_falls_back_to_plain() {
        // Pseudo-random bytes: neither algorithm may claim them.
        let mut input = vec![0u8; 4096];
        let mut state = 0x12345678u32;
        for byte in input.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *byte = (state >> 24) as u8;
        }
        assert!(compress_pcluster(Algo::Lz4, &input).is_none());
        assert!(compress_pcluster(Algo::Deflate, &input).is_none());
        assert!(compress_pcluster(Algo::None, &input).is_none());
    }

    #[test]
    fn pack_pairs_keep_order_and_fit_single_block() {
        let clusters = vec![pattern(4096), pattern(4096), vec![7u8; 100]];
        let pcl = pack_clusters(Algo::Lz4, 4096, &clusters);
        // First pair compresses jointly (HEAD of 2), tail stays separate.
        assert!(!pcl.is_empty());
        assert!(pcl.len() <= clusters.len());
        for p in &pcl {
            assert!(p.payload.len() <= 4096);
        }
    }

    #[test]
    fn index_anchors_match_consecutive_blocks() {
        // HEAD(2 clusters) + PLAIN: blocks P, P+1; packs [H,N] and [P,pad].
        let pcl = vec![
            Pcluster { head: true, n_clusters: 2, payload: vec![1u8; 100] },
            Pcluster { head: false, n_clusters: 1, payload: vec![2u8; 4096] },
        ];
        let (encoded, blocks) = encode_pclusters(&pcl, 100);
        assert_eq!(blocks, 2);
        assert_eq!(encoded.len(), 16);
        // Pack 0: HEAD + NONHEAD(1), anchor = 100 - 1.
        assert_eq!(&encoded[0..2], &((1u16 << 12) | 0).to_le_bytes());
        assert_eq!(&encoded[2..4], &((2u16 << 12) | 1).to_le_bytes());
        assert_eq!(&encoded[4..8], &99u32.to_le_bytes());
        // Pack 1: PLAIN + pad, anchor = 101 - 1.
        assert_eq!(&encoded[8..10], &0u16.to_le_bytes());
        assert_eq!(&encoded[10..12], &0u16.to_le_bytes());
        assert_eq!(&encoded[12..16], &100u32.to_le_bytes());
    }
}
