use crate::core::buf;

/// Extract `security.selinux` xattr from an ext4 inode body, if present.
///
/// Layout notes (must match kernel/e2fsck/mkfs, verified against e2fsprogs
/// 1.47.2 `check_ea_in_inode`): entries start right after the 4-byte magic
/// (`header + 4`), and `e_value_offs` is relative to that same base
/// (`header + 4`), NOT to the header start. Values are conventionally
/// packed backwards from the end of the extra area.
pub fn read_selinux_context(inode: &[u8]) -> Option<String> {
    let extra_size = buf::u16le(inode, 128).ok()? as usize;
    let start = 128usize.checked_add(extra_size)?;
    if start.checked_add(4)? > inode.len() {
        return None;
    }
    if buf::u32le(inode, start).ok()? != 0xea02_0000 {
        return None;
    }
    // Entry/value base: first byte after the 4-byte magic.
    let base = start.checked_add(4)?;
    let mut offset = base;
    while offset + 16 <= inode.len() {
        let name_length = inode[offset] as usize;
        if name_length == 0 {
            break;
        }
        let name_index = inode[offset + 1];
        let value_offset = buf::u16le(inode, offset + 2).ok()? as usize;
        let value_block = buf::u32le(inode, offset + 4).ok()?;
        let value_size = buf::u32le(inode, offset + 8).ok()? as usize;
        let name_start = offset + 16;
        let name_end = name_start.checked_add(name_length)?;
        if name_end > inode.len() {
            return None;
        }
        if name_index == 6 && &inode[name_start..name_end] == b"selinux" && value_block == 0 {
            let value_start = base.checked_add(value_offset)?;
            let value_end = value_start.checked_add(value_size)?;
            if value_end <= inode.len() {
                return Some(
                    String::from_utf8_lossy(&inode[value_start..value_end])
                        .trim_matches('\0')
                        .to_owned(),
                );
            }
        }
        offset = (name_end + 3) & !3;
    }
    None
}

pub fn read_selinux_from_block(block: &[u8]) -> Option<String> {
    if block.len() < 32 {
        return None;
    }
    // Проверка magic заголовка ext4_xattr_header (0xea02_0000)
    if buf::u32le(block, 0).ok()? != 0xea02_0000 {
        return None;
    }
    // Записи ext4_xattr_entry начинаются сразу за 32-байтным заголовком
    let mut offset = 32usize;
    while offset + 16 <= block.len() {
        let name_length = block[offset] as usize;
        if name_length == 0 {
            break;
        }
        let name_index = block[offset + 1];
        let value_offset = buf::u16le(block, offset + 2).ok()? as usize;
        let value_block = buf::u32le(block, offset + 4).ok()?;
        let value_size = buf::u32le(block, offset + 8).ok()? as usize;
        let name_start = offset + 16;
        let name_end = name_start.checked_add(name_length)?;
        if name_end > block.len() {
            return None;
        }
        // В блоке xattr смещение value_offset отсчитывается от начала самого блока
        if name_index == 6 && &block[name_start..name_end] == b"selinux" && value_block == 0 {
            let value_end = value_offset.checked_add(value_size)?;
            if value_end <= block.len() {
                return Some(
                    String::from_utf8_lossy(&block[value_offset..value_end])
                        .trim_matches('\0')
                        .to_owned(),
                );
            }
        }
        offset = (name_end + 3) & !3;
    }
    None
}
