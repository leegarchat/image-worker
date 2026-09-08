//! Unified image container: raw or Android sparse.
//! Read path validates strictly (RAW/FILL/DONT_CARE/CRC32 sizes).
//! Write path can encode raw -> sparse.

use crate::core::Error;
use crate::core::buf;
use std::{
    env,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SPARSE_MAGIC: u32 = 0xed26_ff3a;
pub const CHUNK_RAW: u16 = 0xcac1;
pub const CHUNK_FILL: u16 = 0xcac2;
pub const CHUNK_DONT_CARE: u16 = 0xcac3;
pub const CHUNK_CRC32: u16 = 0xcac4;

/// RAII input source. Removes temp file (stdin spool) on drop.
pub(crate) struct InputSource {
    file: File,
    temporary_path: Option<PathBuf>,
}

impl Drop for InputSource {
    fn drop(&mut self) {
        if let Some(path) = &self.temporary_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl InputSource {
    fn open(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            file: File::open(path)?,
            temporary_path: None,
        })
    }

    fn from_stdin() -> Result<Self, Error> {
        let dir = env::var_os("TMPDIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::invalid(format!("system clock error: {e}")))?
            .as_nanos();
        let pid = std::process::id();
        for attempt in 0..100u32 {
            let candidate = dir.join(format!("image-worker-{pid}-{timestamp}-{attempt}.img"));
            match File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    let mut stdin = io::stdin().lock();
                    io::copy(&mut stdin, &mut file)?;
                    file.flush()?;
                    file.seek(SeekFrom::Start(0))?;
                    return Ok(Self {
                        file,
                        temporary_path: Some(candidate),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::invalid(
            "could not create a temporary file for stdin",
        ))
    }
}

#[derive(Debug, Clone)]
pub struct SparseChunk {
    pub kind: u16,
    pub logical_start: u64,
    pub logical_len: u64,
    pub file_offset: u64,
    pub fill_value: [u8; 4],
}

#[derive(Debug)]
pub struct SparseImage {
    pub block_size: u64,
    logical_size: u64,
    pub chunks: Vec<SparseChunk>,
}

impl SparseImage {
    pub fn parse(file: &mut File) -> Result<Self, Error> {
        let mut header = [0u8; 28];
        file.read_exact(&mut header)?;
        let bad = || Error::invalid("unsupported sparse header");
        let major = buf::u16le(&header, 4).map_err(|_| bad())?;
        let file_header_size = buf::u16le(&header, 8).map_err(|_| bad())? as u64;
        let chunk_header_size = buf::u16le(&header, 10).map_err(|_| bad())? as u64;
        let block_size = buf::u32le(&header, 12).map_err(|_| bad())? as u64;
        let total_blocks = buf::u32le(&header, 16).map_err(|_| bad())? as u64;
        let total_chunks = buf::u32le(&header, 20).map_err(|_| bad())?;

        if major != 1
            || file_header_size < 28
            || chunk_header_size < 12
            || chunk_header_size > 1024
            || block_size == 0
            || block_size > 64 * 1024 * 1024
        {
            return Err(Error::invalid("unsupported sparse header"));
        }
        file.seek(SeekFrom::Start(file_header_size))?;
        // No large pre-reserve: a corrupt chunk count must not balloon RAM.
        let mut chunks = Vec::new();
        let mut logical_start = 0u64;

        for index in 0..total_chunks {
            let mut chunk_header = vec![0u8; chunk_header_size as usize];
            file.read_exact(&mut chunk_header)?;
            let kind = buf::u16le(&chunk_header, 0)
                .map_err(|_| Error::invalid(format!("chunk {index} has invalid header")))?;
            let chunk_blocks = buf::u32le(&chunk_header, 4)
                .map_err(|_| Error::invalid(format!("chunk {index} has invalid header")))?
                as u64;
            let total_size = buf::u32le(&chunk_header, 8)
                .map_err(|_| Error::invalid(format!("chunk {index} has invalid header")))?
                as u64;
            let logical_len = chunk_blocks
                .checked_mul(block_size)
                .ok_or_else(|| Error::invalid(format!("chunk {index} length overflow")))?;
            // Zero-block chunks contribute nothing and would let a corrupt
            // chunk count spin without advancing: reject them.
            if chunk_blocks == 0 {
                return Err(Error::invalid(format!("chunk {index} has no blocks")));
            }
            let data_size = total_size
                .checked_sub(chunk_header_size)
                .ok_or_else(|| Error::invalid(format!("chunk {index} has invalid size")))?;
            let file_offset = file.stream_position()?;
            let fill_value = if kind == CHUNK_FILL {
                if data_size != 4 {
                    return Err(Error::invalid(format!(
                        "chunk {index} has invalid FILL size"
                    )));
                }
                let mut value = [0u8; 4];
                file.read_exact(&mut value)?;
                value
            } else {
                [0; 4]
            };

            match kind {
                CHUNK_RAW if data_size != logical_len => {
                    return Err(Error::invalid(format!(
                        "chunk {index} has invalid RAW size"
                    )));
                }
                CHUNK_DONT_CARE | CHUNK_CRC32
                    if data_size != 0 && !(kind == CHUNK_CRC32 && data_size == 4) =>
                {
                    return Err(Error::invalid(format!(
                        "chunk {index} has invalid size"
                    )));
                }
                CHUNK_RAW | CHUNK_DONT_CARE | CHUNK_CRC32 => {
                    file.seek(SeekFrom::Current(data_size as i64))?;
                }
                CHUNK_FILL => {}
                _ => {
                    return Err(Error::invalid(format!(
                        "chunk {index} type 0x{kind:04x} is unsupported"
                    )));
                }
            }
            chunks.push(SparseChunk {
                kind,
                logical_start,
                logical_len,
                file_offset,
                fill_value,
            });
            logical_start = logical_start
                .checked_add(logical_len)
                .ok_or_else(|| Error::invalid("logical image size overflow"))?;
        }
        let expected_size = total_blocks
            .checked_mul(block_size)
            .ok_or_else(|| Error::invalid("sparse image size overflow"))?;
        if logical_start != expected_size {
            return Err(Error::invalid("chunk blocks do not match header"));
        }
        Ok(Self {
            block_size,
            logical_size: expected_size,
            chunks,
        })
    }

    pub fn logical_size(&self) -> u64 {
        self.logical_size
    }

    pub fn read_at(&self, file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let mut position = offset;
        let mut written = 0usize;
        while written < buffer.len() {
            // chunks are ordered by logical_start; binary search is fine
            let idx = self
                .chunks
                .partition_point(|c| c.logical_start <= position);
            if idx == 0 {
                return Err(Error::invalid(format!(
                    "no sparse chunk for offset 0x{position:x}"
                )));
            }
            let chunk = &self.chunks[idx - 1];
            if position >= chunk.logical_start + chunk.logical_len {
                return Err(Error::invalid(format!(
                    "no sparse chunk for offset 0x{position:x}"
                )));
            }
            let available = (chunk.logical_start + chunk.logical_len - position) as usize;
            let count = available.min(buffer.len() - written);
            match chunk.kind {
                CHUNK_RAW => {
                    file.seek(SeekFrom::Start(
                        chunk.file_offset + position - chunk.logical_start,
                    ))?;
                    file.read_exact(&mut buffer[written..written + count])?;
                }
                CHUNK_FILL => {
                    for (index, byte) in
                        buffer[written..written + count].iter_mut().enumerate()
                    {
                        *byte = chunk.fill_value[(position as usize + index) % 4];
                    }
                }
                CHUNK_DONT_CARE | CHUNK_CRC32 => buffer[written..written + count].fill(0),
                _ => unreachable!(),
            }
            position += count as u64;
            written += count;
        }
        Ok(())
    }
}

pub enum Image {
    Raw {
        source: InputSource,
        size: u64,
    },
    Sparse {
        source: InputSource,
        sparse: SparseImage,
    },
}

impl Image {
    /// Open raw or sparse image. `"-"` spools stdin to a temp seekable file.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let mut source = if path == Path::new("-") {
            InputSource::from_stdin()?
        } else {
            InputSource::open(path)?
        };
        let file_size = source.file.metadata()?.len();
        let mut magic = [0u8; 4];
        // Empty file -> treat as raw of size 0 (read_at will bounds-check)
        if file_size >= 4 {
            source.file.read_exact(&mut magic)?;
            source.file.seek(SeekFrom::Start(0))?;
        }
        if file_size >= 4 && u32::from_le_bytes(magic) == SPARSE_MAGIC {
            let sparse = SparseImage::parse(&mut source.file)?;
            Ok(Self::Sparse { source, sparse })
        } else {
            Ok(Self::Raw {
                source,
                size: file_size,
            })
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            Self::Raw { size, .. } => *size,
            Self::Sparse { sparse, .. } => sparse.logical_size(),
        }
    }

    pub fn is_sparse(&self) -> bool {
        matches!(self, Self::Sparse { .. })
    }

    pub fn sparse_info(&self) -> Option<(u64, usize)> {
        match self {
            Self::Sparse { sparse, .. } => Some((sparse.block_size, sparse.chunks.len())),
            _ => None,
        }
    }

    pub fn container_name(&self) -> &'static str {
        if self.is_sparse() {
            "android-sparse"
        } else {
            "raw"
        }
    }

    /// Raw underlying file handle (container bytes). Only `Some` for raw
    /// images; sparse images must be accessed through `read_at`.
    pub fn raw_file_mut(&mut self) -> Option<&mut File> {
        match self {
            Self::Raw { source, .. } => Some(&mut source.file),
            Self::Sparse { .. } => None,
        }
    }

    /// Stream the whole container file byte-for-byte (headers included).
    pub fn copy_container_bytes(&mut self, out: &mut dyn Write) -> Result<(), Error> {
        let file = match self {
            Self::Raw { source, .. } => &mut source.file,
            Self::Sparse { source, .. } => &mut source.file,
        };
        file.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
        out.flush()?;
        Ok(())
    }

    pub fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| Error::invalid("read range overflow"))?;
        if end > self.size() {
            return Err(Error::invalid(format!(
                "read range 0x{offset:x}..0x{end:x} exceeds image"
            )));
        }
        match self {
            Self::Raw { source, .. } => {
                source.file.seek(SeekFrom::Start(offset))?;
                source.file.read_exact(buffer)?;
            }
            Self::Sparse { source, sparse } => {
                sparse.read_at(&mut source.file, offset, buffer)?;
            }
        }
        Ok(())
    }
}

/// Stream a raw file as sparse container to stdout (two passes, O(1) RAM).
pub fn sparsify_file_to_stdout(path: &Path, block_size: usize) -> Result<(), Error> {
    let mut raw = File::open(path)?;
    let (runs, blocks) = scan_sparse_runs(&mut raw, block_size)?;
    let stdout = io::stdout();
    let mut stream = stdout.lock();
    write_sparse_header(&mut stream, block_size, blocks, runs.len())?;
    emit_sparse_runs(&mut raw, block_size, &runs, &mut stream)?;
    stream.flush()?;
    Ok(())
}

// ── Streaming sparse encoder (O(1) RAM, file-backed) ─────────────────
// A raw block-aligned image is scanned block-by-block; only chunk
// descriptors (kind/start/count) live in RAM, payloads stream from disk.

/// Max blocks per single RAW chunk (payload stays below u32).
pub const SPARSE_MAX_RAW_RUN: u64 = 16384;

#[derive(Debug, Clone)]
pub struct SparseRun {
    pub kind: u16,
    pub start_block: u64,
    pub count: u32,
    pub fill: [u8; 4],
}

fn classify_block(block: &[u8]) -> (u16, [u8; 4]) {
    if block.iter().all(|b| *b == 0) {
        return (CHUNK_DONT_CARE, [0; 4]);
    }
    let mut fill = [0u8; 4];
    fill.copy_from_slice(&block[..4]);
    if block.chunks_exact(4).all(|p| p == &block[..4]) {
        return (CHUNK_FILL, fill);
    }
    (CHUNK_RAW, [0; 4])
}

/// Scan a raw block-aligned file, return merged chunk descriptors.
/// Only descriptors (kind/start/count) live in RAM; payloads stay on disk.
pub fn scan_sparse_runs(raw: &mut File, block_size: usize) -> Result<(Vec<SparseRun>, u64), Error> {
    if block_size == 0 {
        return Err(Error::invalid("invalid block size"));
    }
    let len = raw.metadata()?.len();
    if len % block_size as u64 != 0 {
        return Err(Error::invalid("image size is not block aligned"));
    }
    let blocks = len / block_size as u64;
    if blocks > u64::from(u32::MAX) {
        return Err(Error::invalid("image has too many blocks for sparse"));
    }
    let mut runs: Vec<SparseRun> = Vec::new();
    let mut buf = vec![0u8; block_size];
    let mut index = 0u64;
    while index < blocks {
        raw.seek(SeekFrom::Start(index * block_size as u64))?;
        raw.read_exact(&mut buf)?;
        let (kind, fill) = classify_block(&buf);
        let start = index;
        index += 1;
        if kind == CHUNK_RAW {
            while index < blocks
                && (index - start) < SPARSE_MAX_RAW_RUN
                && {
                    raw.seek(SeekFrom::Start(index * block_size as u64))?;
                    raw.read_exact(&mut buf)?;
                    classify_block(&buf).0 == CHUNK_RAW
                }
            {
                index += 1;
            }
        } else {
            while index < blocks {
                raw.seek(SeekFrom::Start(index * block_size as u64))?;
                raw.read_exact(&mut buf)?;
                let same = match kind {
                    CHUNK_DONT_CARE => buf.iter().all(|b| *b == 0),
                    _ => {
                        let (k, f) = classify_block(&buf);
                        k == CHUNK_FILL && f == fill
                    }
                };
                if !same {
                    break;
                }
                index += 1;
            }
        }
        let count = index - start;
        if count > u64::from(u32::MAX) {
            return Err(Error::invalid("sparse run is too long"));
        }
        runs.push(SparseRun {
            kind,
            start_block: start,
            count: count as u32,
            fill,
        });
    }
    if runs.len() > u32::MAX as usize {
        return Err(Error::invalid("too many sparse chunks"));
    }
    Ok((runs, blocks))
}

fn write_sparse_header(
    out: &mut impl Write,
    block_size: usize,
    blocks: u64,
    chunks: usize,
) -> Result<(), Error> {
    let mut header = [0u8; 28];
    header[0..4].copy_from_slice(&SPARSE_MAGIC.to_le_bytes());
    header[4..6].copy_from_slice(&1u16.to_le_bytes());
    header[8..10].copy_from_slice(&28u16.to_le_bytes());
    header[10..12].copy_from_slice(&12u16.to_le_bytes());
    header[12..16].copy_from_slice(&(block_size as u32).to_le_bytes());
    header[16..20].copy_from_slice(&(blocks as u32).to_le_bytes());
    header[20..24].copy_from_slice(&(chunks as u32).to_le_bytes());
    out.write_all(&header)?;
    Ok(())
}

/// Emit scanned runs: RAW payloads stream from the raw file in 256 KiB
/// quanta; FILL/DONT_CARE carry no payload.
pub fn emit_sparse_runs(
    raw: &mut File,
    block_size: usize,
    runs: &[SparseRun],
    out: &mut impl Write,
) -> Result<(), Error> {
    let mut copy_buf = vec![0u8; 256 * 1024];
    for run in runs {
        let payload = match run.kind {
            CHUNK_RAW => run.count as u64 * block_size as u64,
            CHUNK_FILL => 4,
            _ => 0,
        };
        if payload > u64::from(u32::MAX) {
            return Err(Error::invalid("sparse chunk payload is too large"));
        }
        let total = 12 + payload as u32;
        out.write_all(&run.kind.to_le_bytes())?;
        out.write_all(&0u16.to_le_bytes())?;
        out.write_all(&run.count.to_le_bytes())?;
        out.write_all(&total.to_le_bytes())?;
        match run.kind {
            CHUNK_RAW => {
                raw.seek(SeekFrom::Start(run.start_block * block_size as u64))?;
                let mut remaining = payload;
                while remaining > 0 {
                    let n = (remaining.min(copy_buf.len() as u64)) as usize;
                    raw.read_exact(&mut copy_buf[..n])?;
                    out.write_all(&copy_buf[..n])?;
                    remaining -= n as u64;
                }
            }
            CHUNK_FILL => out.write_all(&run.fill)?,
            _ => {}
        }
    }
    Ok(())
}

/// Convert a raw block-aligned file to a sparse container, streaming.
/// Output must support seeking (header counts are patched after the scan).
pub fn sparsify_file_to_seekable(
    raw: &mut File,
    block_size: usize,
    out: &mut (impl Write + Seek),
) -> Result<(), Error> {
    // Placeholder header; counts are unknown until the scan finishes.
    write_sparse_header(out, block_size, 0, 0)?;
    let (runs, blocks) = scan_sparse_runs(raw, block_size)?;
    emit_sparse_runs(raw, block_size, &runs, out)?;
    out.seek(SeekFrom::Start(0))?;
    write_sparse_header(out, block_size, blocks, runs.len())?;
    out.flush()?;
    Ok(())
}

/// Convert raw file at `path` into a sparse container in place
/// (sibling temp file + atomic rename). O(1) RAM.
pub fn sparsify_file_in_place(path: &Path, block_size: usize) -> Result<(), Error> {
    let tmp = path.with_extension("sparse_tmp");
    {
        let mut raw = File::open(path)?;
        let mut out = File::create(&tmp)?;
        sparsify_file_to_seekable(&mut raw, block_size, &mut out)?;
        out.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Create a uniquely-named temp file (`prefix-pid-nanos-attempt.img`).
/// Caller owns the path and must remove it when done.
pub fn spool_temp(prefix: &str) -> Result<(File, PathBuf), Error> {
    let dir = env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::invalid(format!("system clock error: {e}")))?
        .as_nanos();
    let pid = std::process::id();
    for attempt in 0..100u32 {
        let candidate = dir.join(format!("{prefix}-{pid}-{timestamp}-{attempt}.img"));
        match File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::invalid("could not create a temporary file"))
}

/// Convert only the container of an image (raw <-> Android sparse),
/// streaming, without touching the filesystem payload. `block_size` (the
/// sparse chunk granularity) must come from the filesystem superblock;
/// callers probe it before invoking this.
pub fn convert_container(
    input: &Path,
    output: &Path,
    want_sparse: bool,
    block_size: usize,
) -> Result<(), Error> {
    let mut image = Image::open(input)?;
    let to_stdout = output == Path::new("-");
    if image.is_sparse() == want_sparse {
        // Same container: plain byte copy.
        if to_stdout {
            let stdout = io::stdout();
            let mut stream = stdout.lock();
            image.copy_container_bytes(&mut stream)?;
        } else {
            let mut out = File::create(output)?;
            image.copy_container_bytes(&mut out)?;
        }
        return Ok(());
    }
    if image.is_sparse() {
        // Sparse -> raw: stream logical bytes.
        if to_stdout {
            let stdout = io::stdout();
            let mut stream = stdout.lock();
            stream_logical(&mut image, &mut stream)?;
        } else {
            let mut out = File::create(output)?;
            stream_logical(&mut image, &mut out)?;
        }
        return Ok(());
    }
    // Raw -> sparse: scan chunk descriptors, then emit (header first).
    let raw = image
        .raw_file_mut()
        .ok_or_else(|| Error::invalid("raw image has no backing file"))?;
    let (runs, blocks) = scan_sparse_runs(raw, block_size)?;
    if to_stdout {
        let stdout = io::stdout();
        let mut stream = stdout.lock();
        write_sparse_header(&mut stream, block_size, blocks, runs.len())?;
        let raw = image
            .raw_file_mut()
            .ok_or_else(|| Error::invalid("raw image has no backing file"))?;
        emit_sparse_runs(raw, block_size, &runs, &mut stream)?;
        stream.flush()?;
    } else {
        let mut out = File::create(output)?;
        write_sparse_header(&mut out, block_size, blocks, runs.len())?;
        let raw = image
            .raw_file_mut()
            .ok_or_else(|| Error::invalid("raw image has no backing file"))?;
        emit_sparse_runs(raw, block_size, &runs, &mut out)?;
        out.flush()?;
    }
    Ok(())
}

/// Stream the logical (de-sparsed) content of an image.
pub fn stream_logical(image: &mut Image, out: &mut dyn Write) -> Result<(), Error> {
    let total = image.size();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;
    while offset < total {
        let n = ((total - offset).min(buf.len() as u64)) as usize;
        image.read_at(offset, &mut buf[..n])?;
        out.write_all(&buf[..n])?;
        offset += n as u64;
    }
    out.flush()?;
    Ok(())
}
