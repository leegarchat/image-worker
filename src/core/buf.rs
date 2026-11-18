//! Bounds-checked little-endian readers for on-disk structures.
//!
//! Every accessor returns `Error::Invalid` instead of panicking, so
//! corrupted images produce clean errors from any layer (read, write,
//! convert). Use these instead of `uXX::from_le_bytes(slice.try_into()
//! .unwrap())` on untrusted data.

use super::Error;

/// Read a `u16` at `offset`, failing cleanly when out of bounds.
pub fn u16le(data: &[u8], offset: usize) -> Result<u16, Error> {
    data.get(offset..offset + 2)
        .and_then(|v| v.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| Error::invalid(format!("truncated structure: missing u16 at {offset}")))
}

/// Read a `u32` at `offset`, failing cleanly when out of bounds.
pub fn u32le(data: &[u8], offset: usize) -> Result<u32, Error> {
    data.get(offset..offset + 4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| Error::invalid(format!("truncated structure: missing u32 at {offset}")))
}

/// Read a `u64` at `offset`, failing cleanly when out of bounds.
pub fn u64le(data: &[u8], offset: usize) -> Result<u64, Error> {
    data.get(offset..offset + 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| Error::invalid(format!("truncated structure: missing u64 at {offset}")))
}

/// Borrow `len` bytes at `offset`, failing cleanly when out of bounds.
pub fn bytes(data: &[u8], offset: usize, len: usize) -> Result<&[u8], Error> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| Error::invalid("byte range overflow"))?;
    data.get(offset..end)
        .ok_or_else(|| Error::invalid(format!("truncated structure: missing {len} bytes at {offset}")))
}
