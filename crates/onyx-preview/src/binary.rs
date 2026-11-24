use crate::PreviewContent;
use std::fmt::Write;
use std::path::Path;

/// Number of header bytes included in the hex dump preview.
pub(crate) const HEX_HEADER_BYTES: usize = 256;

/// Builds a [`PreviewContent::BinaryMetadata`] preview from a bounded header sample.
#[must_use]
pub(crate) fn build_binary_metadata(path: &Path, header: &[u8], size_bytes: u64) -> PreviewContent {
    let mime_guess = guess_mime(path, header).to_string();
    let sample_len = header.len().min(HEX_HEADER_BYTES);
    let hex_header = format_hex_dump(&header[..sample_len]);

    PreviewContent::BinaryMetadata {
        mime_guess,
        size_bytes,
        hex_header,
    }
}

/// Guesses a MIME type from magic bytes first, falling back to file extension.
#[must_use]
pub(crate) fn guess_mime(path: &Path, header: &[u8]) -> &'static str {
    if header.starts_with(b"MZ") {
        return "application/vnd.microsoft.portable-executable";
    }
    if header.starts_with(b"\x7FELF") {
        return "application/x-elf";
    }
    if header.starts_with(b"SQLite format 3\0") {
        return "application/vnd.sqlite3";
    }
    if header.starts_with(b"\0asm") {
        return "application/wasm";
    }
    if header.starts_with(&[0x1F, 0x8B]) {
        return "application/gzip";
    }
    if header.starts_with(b"7z\xBC\xAF\x27\x1C") {
        return "application/x-7z-compressed";
    }
    if header.starts_with(b"Rar!\x1A\x07") {
        return "application/vnd.rar";
    }
    if header.starts_with(b"BZh") {
        return "application/x-bzip2";
    }
    if header.starts_with(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]) {
        return "application/x-xz";
    }
    if header.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return "application/zstd";
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    match ext.as_str() {
        "exe" | "dll" | "sys" => "application/vnd.microsoft.portable-executable",
        "msi" => "application/x-msi",
        "db" | "sqlite" | "sqlite3" => "application/vnd.sqlite3",
        "wasm" => "application/wasm",
        "gz" => "application/gzip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "tar" => "application/x-tar",
        "iso" => "application/x-iso9660-image",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Formats a byte slice as a compact canonical 16-byte-per-row hex + ASCII dump.
fn format_hex_dump(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    for (row_idx, chunk) in bytes.chunks(16).enumerate() {
        if row_idx > 0 {
            out.push('\n');
        }
        let offset = row_idx * 16;
        let _ = write!(out, "{offset:04X}: ");
        for i in 0..16 {
            if let Some(&b) = chunk.get(i) {
                let _ = write!(out, "{b:02X} ");
            } else {
                out.push_str("   ");
            }
        }
        out.push(' ');
        for &b in chunk {
            if (0x20..=0x7E).contains(&b) {
                out.push(b as char);
            } else {
                out.push('.');
            }
        }
    }
    out
}
