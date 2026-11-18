//! Presentation helpers. Byte-identical semantics to image-inspect.

pub const SAMPLE_SIZE: usize = 4096;

pub fn file_type(mode: u16) -> &'static str {
    match mode & 0xf000 {
        0x4000 => "directory",
        0xa000 => "symlink",
        0x2000 => "character",
        0x6000 => "block",
        0x1000 => "fifo",
        0xc000 => "socket",
        _ => "file",
    }
}

pub fn numeric_permissions(mode: u16) -> String {
    format!("{:04o}", mode & 0o7777)
}

pub fn symbolic_permissions(mode: u16) -> String {
    let mut output = String::with_capacity(10);
    output.push(match mode & 0xf000 {
        0x4000 => 'd',
        0xa000 => 'l',
        _ => '-',
    });
    for (shift, special, special_char) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        output.push(if mode & (0o4 << shift) != 0 { 'r' } else { '-' });
        output.push(if mode & (0o2 << shift) != 0 { 'w' } else { '-' });
        output.push(if mode & special != 0 {
            if mode & (0o1 << shift) != 0 {
                special_char
            } else {
                special_char.to_ascii_uppercase()
            }
        } else if mode & (0o1 << shift) != 0 {
            'x'
        } else {
            '-'
        });
    }
    output
}

pub fn human_size(size: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = size as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", size)
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

/// Full signature detection, identical to image-inspect `file_type::detect`.
pub fn detect_content_type(data: &[u8]) -> String {
    if data.starts_with(b"\x7fELF") {
        return match data.get(4) {
            Some(1) => "ELF 32-bit executable".into(),
            Some(2) => "ELF 64-bit executable".into(),
            _ => "ELF executable".into(),
        };
    }
    if data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06") {
        return "ZIP archive (possibly APK/JAR)".into();
    }
    if data.starts_with(b"dex\n") {
        return "Dalvik DEX bytecode".into();
    }
    if data.starts_with(b"ANDROID!") {
        return "Android boot image".into();
    }
    if data.starts_with(b"SQLite format 3\0") {
        return "SQLite database".into();
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "PNG image".into();
    }
    if data.starts_with(b"\xff\xd8\xff") {
        return "JPEG image".into();
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return "GIF image".into();
    }
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        return "WebP image".into();
    }
    if data.starts_with(b"%PDF-") {
        return "PDF document".into();
    }
    if data.starts_with(&[0x1f, 0x8b]) {
        return "gzip compressed data".into();
    }
    if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return "Zstandard compressed data".into();
    }
    if data.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        return "XZ compressed data".into();
    }
    if data.starts_with(b"BZh") {
        return "bzip2 compressed data".into();
    }
    if data.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        return "LZ4 compressed data".into();
    }
    if data.len() >= 1082 && u16::from_le_bytes([data[1080], data[1081]]) == 0xef53 {
        return "ext4 filesystem image".into();
    }
    if data.starts_with(b"{\n") || data.starts_with(b"{\r\n") {
        return "JSON text".into();
    }
    if data.starts_with(b"<?xml") || data.starts_with(b"<xml") {
        return "XML text".into();
    }
    if is_text(data) {
        "UTF-8 text".into()
    } else {
        "binary data".into()
    }
}

pub fn sample_size() -> usize {
    SAMPLE_SIZE
}

fn is_text(data: &[u8]) -> bool {
    if data.contains(&0) || std::str::from_utf8(data).is_err() {
        return false;
    }
    data.iter()
        .all(|byte| matches!(*byte, b'\t' | b'\n' | b'\r' | 0x0c | 0x20..=0x7e) || *byte >= 0x80)
}

/// Glob with `*` and `?`, identical to image-inspect.
pub fn matches_pattern(name: &str, pattern: &str) -> bool {
    let name = name.as_bytes();
    let pattern = pattern.as_bytes();
    let (mut name_index, mut pattern_index) = (0usize, 0usize);
    let mut star = None;
    let mut star_match = 0usize;
    while name_index < name.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == name[name_index])
        {
            name_index += 1;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_match = name_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_match += 1;
            name_index = star_match;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::detect_content_type;

    #[test]
    fn detects_common_binary_signatures() {
        assert_eq!(detect_content_type(b"\x7fELF\x02"), "ELF 64-bit executable");
        assert_eq!(
            detect_content_type(b"PK\x03\x04"),
            "ZIP archive (possibly APK/JAR)"
        );
        assert_eq!(
            detect_content_type(b"\x89PNG\r\n\x1a\n"),
            "PNG image"
        );
    }

    #[test]
    fn classifies_text_and_binary() {
        assert_eq!(
            detect_content_type(b"fstab.zuma\n/system /system ext4 ro\n"),
            "UTF-8 text"
        );
        assert_eq!(detect_content_type(b"\x00\xff\x01"), "binary data");
    }
}
