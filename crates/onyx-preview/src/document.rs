use crate::PreviewContent;
use onyx_core::{OnyxError, OnyxResult, format_bytes};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Maximum bytes read from a PDF when estimating page count safely.
const MAX_PDF_SCAN_BYTES: u64 = 512 * 1024;
/// Maximum header bytes read for media/archive container inspection.
const MAX_HEADER_SCAN_BYTES: u64 = 64 * 1024;
/// Maximum tail window scanned for a ZIP End-of-Central-Directory record.
const MAX_ZIP_EOCD_WINDOW: u64 = 65_557;

/// Attempts safe, bounded header-only metadata extraction for known document, media, and archive containers.
///
/// Returns `Ok(Some(PreviewContent::DocumentMetadata { .. }))` if the file matches a supported
/// container format, or `Ok(None)` if another preview handler should inspect the file.
pub(crate) fn try_load_document_metadata(
    path: &Path,
    header: &[u8],
    file_size: u64,
    cancel: &CancellationToken,
) -> OnyxResult<Option<PreviewContent>> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    // PDF (%PDF-x.y)
    if find_subslice(header, b"%PDF-").is_some() || ext == "pdf" {
        return Ok(Some(inspect_pdf(path, file_size, cancel)?));
    }

    // WAV / RIFF
    if (header.len() >= 12 && &header[0..4] == b"RIFF" && &header[8..12] == b"WAVE") || ext == "wav"
    {
        return Ok(Some(inspect_wav(path, file_size, cancel)?));
    }

    // FLAC
    if header.starts_with(b"fLaC") || ext == "flac" {
        return Ok(Some(inspect_flac(path, file_size, cancel)?));
    }

    // MP3 / ID3
    if header.starts_with(b"ID3") || ext == "mp3" {
        return Ok(Some(inspect_mp3(path, file_size, cancel)?));
    }

    // MP4 / ISO Base Media (ftyp)
    if (header.len() >= 12 && &header[4..8] == b"ftyp")
        || matches!(ext.as_str(), "mp4" | "m4a" | "m4v" | "mov")
    {
        return Ok(Some(inspect_mp4(path, file_size, cancel)?));
    }

    // MKV / WebM (EBML)
    if header.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) || matches!(ext.as_str(), "mkv" | "webm") {
        return Ok(Some(inspect_mkv(path, file_size, cancel)?));
    }

    // ZIP / PK (including OOXML containers safely without macro execution)
    if header.starts_with(&[0x50, 0x4B, 0x03, 0x04])
        || header.starts_with(&[0x50, 0x4B, 0x05, 0x06])
        || header.starts_with(&[0x50, 0x4B, 0x07, 0x08])
        || matches!(
            ext.as_str(),
            "zip" | "docx" | "xlsx" | "pptx" | "jar" | "epub"
        )
    {
        return Ok(Some(inspect_zip(path, &ext, file_size, cancel)?));
    }

    Ok(None)
}

fn read_bounded(path: &Path, cap: u64, cancel: &CancellationToken) -> OnyxResult<Vec<u8>> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }
    let file = File::open(path)?;
    let mut buf = Vec::new();
    file.take(cap).read_to_end(&mut buf)?;
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }
    Ok(buf)
}

fn inspect_pdf(
    path: &Path,
    file_size: u64,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    let data = read_bounded(path, MAX_PDF_SCAN_BYTES, cancel)?;
    let mut fields = Vec::new();

    let version = find_subslice(&data[..data.len().min(1024)], b"%PDF-")
        .map(|pos| {
            let start = pos + 5;
            let end = data[start..]
                .iter()
                .position(|&b| b == b'\r' || b == b'\n' || b == b' ')
                .map_or(data.len().min(start + 8), |rel| {
                    (start + rel).min(start + 8)
                });
            String::from_utf8_lossy(&data[start..end])
                .trim()
                .to_string()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());

    let page_count = estimate_pdf_pages(&data);
    let partial_scan = file_size > MAX_PDF_SCAN_BYTES;
    let encrypted = find_subslice(&data, b"/Encrypt").is_some();

    fields.push(("Format".to_string(), "PDF".to_string()));
    fields.push(("PDF Version".to_string(), version));
    if page_count > 0 {
        let label = if partial_scan {
            format!("{page_count}+ (header scan)")
        } else {
            page_count.to_string()
        };
        fields.push(("Estimated Pages".to_string(), label));
    } else {
        fields.push(("Estimated Pages".to_string(), "Unknown".to_string()));
    }
    fields.push((
        "Encrypted".to_string(),
        if encrypted { "Yes" } else { "No" }.to_string(),
    ));
    fields.push(("Size".to_string(), format_bytes(file_size)));

    Ok(PreviewContent::DocumentMetadata {
        kind: "PDF Document".to_string(),
        summary_fields: fields,
    })
}

/// Counts `/Type /Page` markers while excluding `/Type /Pages` container dictionaries.
fn estimate_pdf_pages(data: &[u8]) -> usize {
    let needle = b"/Type";
    let mut count = 0usize;
    let mut idx = 0usize;

    while let Some(rel) = find_subslice(&data[idx..], needle) {
        let mut cursor = idx + rel + needle.len();
        while cursor < data.len() && data[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if data[cursor..].starts_with(b"/Page") {
            let after = cursor + 5;
            let next_is_ident = data
                .get(after)
                .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_');
            if !next_is_ident {
                count += 1;
            }
        }
        idx = cursor.max(idx + rel + 1);
    }

    count
}

fn inspect_mp3(
    path: &Path,
    file_size: u64,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    let data = read_bounded(path, MAX_HEADER_SCAN_BYTES, cancel)?;
    let mut fields = vec![("Format".to_string(), "MP3 Audio".to_string())];

    if data.len() >= 10 && &data[0..3] == b"ID3" {
        let major = data[3];
        let minor = data[4];
        let tag_size = ((u32::from(data[6] & 0x7F)) << 21)
            | ((u32::from(data[7] & 0x7F)) << 14)
            | ((u32::from(data[8] & 0x7F)) << 7)
            | u32::from(data[9] & 0x7F);

        fields.push(("Tag".to_string(), format!("ID3v2.{major}.{minor}")));
        fields.push(("Tag Size".to_string(), format_bytes(u64::from(tag_size))));

        // Parse simple ID3v2.3 / ID3v2.4 text frames within our bounded buffer.
        if matches!(major, 3 | 4) {
            let limit = data.len().min(10usize.saturating_add(tag_size as usize));
            let mut pos = 10usize;
            let mut frames_parsed = 0usize;

            while pos + 10 <= limit && frames_parsed < 64 {
                let frame_id = &data[pos..pos + 4];
                if frame_id == [0, 0, 0, 0] || !frame_id.iter().all(|b| b.is_ascii_alphanumeric()) {
                    break;
                }
                let frame_size = if major == 4 {
                    ((u32::from(data[pos + 4] & 0x7F)) << 21)
                        | ((u32::from(data[pos + 5] & 0x7F)) << 14)
                        | ((u32::from(data[pos + 6] & 0x7F)) << 7)
                        | u32::from(data[pos + 7] & 0x7F)
                } else {
                    u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                } as usize;

                let payload_start = pos + 10;
                if frame_size == 0 || payload_start.saturating_add(frame_size) > limit {
                    break;
                }

                let payload = &data[payload_start..payload_start + frame_size];
                let field_label = match frame_id {
                    b"TIT2" => Some("Title"),
                    b"TPE1" => Some("Artist"),
                    b"TALB" => Some("Album"),
                    b"TYER" | b"TDRC" => Some("Year"),
                    _ => None,
                };
                if let Some(label) = field_label
                    && let Some(text) = decode_id3_text_frame(payload)
                {
                    fields.push((label.to_string(), text));
                }

                pos = payload_start + frame_size;
                frames_parsed += 1;
            }
        }
    } else if data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0 {
        fields.push(("Header".to_string(), "MPEG Audio Frame Sync".to_string()));
    }

    fields.push(("Size".to_string(), format_bytes(file_size)));

    Ok(PreviewContent::DocumentMetadata {
        kind: "MP3 Audio".to_string(),
        summary_fields: fields,
    })
}

fn decode_id3_text_frame(payload: &[u8]) -> Option<String> {
    if payload.len() < 2 {
        return None;
    }
    let encoding = payload[0];
    let body = &payload[1..];
    let decoded = match encoding {
        0 | 3 => String::from_utf8_lossy(body)
            .trim_end_matches('\0')
            .trim()
            .to_string(),
        _ => return None,
    };
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn inspect_wav(
    path: &Path,
    file_size: u64,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    let data = read_bounded(path, MAX_HEADER_SCAN_BYTES, cancel)?;
    let mut fields = vec![("Format".to_string(), "WAV (RIFF)".to_string())];

    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        let mut pos = 12usize;
        let mut byte_rate: Option<u32> = None;
        let mut data_bytes: Option<u32> = None;

        while pos + 8 <= data.len() {
            let chunk_id = &data[pos..pos + 4];
            let chunk_size =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
            let body_start = pos + 8;

            if chunk_id == b"fmt " && body_start + 16 <= data.len() {
                let audio_fmt = u16::from_le_bytes([data[body_start], data[body_start + 1]]);
                let channels = u16::from_le_bytes([data[body_start + 2], data[body_start + 3]]);
                let sample_rate = u32::from_le_bytes([
                    data[body_start + 4],
                    data[body_start + 5],
                    data[body_start + 6],
                    data[body_start + 7],
                ]);
                let br = u32::from_le_bytes([
                    data[body_start + 8],
                    data[body_start + 9],
                    data[body_start + 10],
                    data[body_start + 11],
                ]);
                let bits_per_sample =
                    u16::from_le_bytes([data[body_start + 14], data[body_start + 15]]);

                let codec = match audio_fmt {
                    1 => "PCM",
                    3 => "IEEE Float",
                    _ => "Other",
                };
                fields.push(("Codec".to_string(), codec.to_string()));
                fields.push(("Channels".to_string(), channels.to_string()));
                fields.push(("Sample Rate".to_string(), format!("{sample_rate} Hz")));
                fields.push(("Bit Depth".to_string(), format!("{bits_per_sample}-bit")));
                byte_rate = Some(br);
            } else if chunk_id == b"data" {
                data_bytes = Some(chunk_size);
                break;
            }

            let padded = (chunk_size as usize).saturating_add((chunk_size as usize) & 1);
            let Some(next_pos) = body_start.checked_add(padded) else {
                break;
            };
            if next_pos <= pos {
                break;
            }
            pos = next_pos;
        }

        if let (Some(br), Some(db)) = (byte_rate, data_bytes)
            && br > 0
        {
            let secs = f64::from(db) / f64::from(br);
            fields.push(("Duration".to_string(), format!("{secs:.2} s")));
        }
    }

    fields.push(("Size".to_string(), format_bytes(file_size)));

    Ok(PreviewContent::DocumentMetadata {
        kind: "WAV Audio".to_string(),
        summary_fields: fields,
    })
}

fn inspect_flac(
    path: &Path,
    file_size: u64,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    let data = read_bounded(path, MAX_HEADER_SCAN_BYTES, cancel)?;
    let mut fields = vec![("Format".to_string(), "FLAC Audio".to_string())];

    if data.len() >= 42 && &data[0..4] == b"fLaC" {
        let block_type = data[4] & 0x7F;
        let block_len =
            (usize::from(data[5]) << 16) | (usize::from(data[6]) << 8) | usize::from(data[7]);
        if block_type == 0 && block_len >= 34 && data.len() >= 8 + 34 {
            let si = &data[8..42];
            // STREAMINFO bytes 10..18 pack sample_rate(20), channels-1(3), bps-1(5), total_samples(36)
            let sample_rate =
                (u32::from(si[10]) << 12) | (u32::from(si[11]) << 4) | (u32::from(si[12]) >> 4);
            let channels = ((si[12] >> 1) & 0x07) + 1;
            let bits_per_sample = (((si[12] & 0x01) << 4) | (si[13] >> 4)) + 1;
            let total_samples = (u64::from(si[13] & 0x0F) << 32)
                | (u64::from(si[14]) << 24)
                | (u64::from(si[15]) << 16)
                | (u64::from(si[16]) << 8)
                | u64::from(si[17]);

            fields.push(("Channels".to_string(), channels.to_string()));
            fields.push(("Sample Rate".to_string(), format!("{sample_rate} Hz")));
            fields.push(("Bit Depth".to_string(), format!("{bits_per_sample}-bit")));
            if sample_rate > 0 && total_samples > 0 {
                let secs = total_samples as f64 / f64::from(sample_rate);
                fields.push(("Duration".to_string(), format!("{secs:.2} s")));
            }
        }
    }

    fields.push(("Size".to_string(), format_bytes(file_size)));

    Ok(PreviewContent::DocumentMetadata {
        kind: "FLAC Audio".to_string(),
        summary_fields: fields,
    })
}

fn inspect_mp4(
    path: &Path,
    file_size: u64,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    let data = read_bounded(path, MAX_HEADER_SCAN_BYTES, cancel)?;
    let mut fields = vec![("Format".to_string(), "MP4 / ISO Base Media".to_string())];

    if data.len() >= 16 && &data[4..8] == b"ftyp" {
        let box_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let major_brand = String::from_utf8_lossy(&data[8..12]).trim().to_string();
        let minor_version = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
        fields.push(("Major Brand".to_string(), major_brand));
        fields.push(("Minor Version".to_string(), minor_version.to_string()));

        let end = box_size.min(data.len()).min(128);
        if end >= 20 {
            let mut compat = Vec::new();
            let mut pos = 16;
            while pos + 4 <= end {
                let brand = String::from_utf8_lossy(&data[pos..pos + 4])
                    .trim()
                    .to_string();
                if !brand.is_empty() && brand.is_ascii() {
                    compat.push(brand);
                }
                pos += 4;
            }
            if !compat.is_empty() {
                fields.push(("Compatible Brands".to_string(), compat.join(", ")));
            }
        }
    }

    fields.push(("Size".to_string(), format_bytes(file_size)));

    Ok(PreviewContent::DocumentMetadata {
        kind: "MP4 / ISO Media".to_string(),
        summary_fields: fields,
    })
}

fn inspect_mkv(
    path: &Path,
    file_size: u64,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    let data = read_bounded(path, 4096, cancel)?;
    let mut fields = vec![("Container".to_string(), "EBML".to_string())];

    let doc_type = if find_subslice(&data, b"webm").is_some() {
        "WebM"
    } else if find_subslice(&data, b"matroska").is_some() {
        "Matroska (MKV)"
    } else {
        "EBML Media"
    };

    fields.push(("DocType".to_string(), doc_type.to_string()));
    fields.push(("Size".to_string(), format_bytes(file_size)));

    Ok(PreviewContent::DocumentMetadata {
        kind: format!("{doc_type} Media"),
        summary_fields: fields,
    })
}

fn inspect_zip(
    path: &Path,
    ext: &str,
    file_size: u64,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let kind = match ext {
        "docx" => "Word Document (OOXML)",
        "xlsx" => "Excel Workbook (OOXML)",
        "pptx" => "PowerPoint Presentation (OOXML)",
        "jar" => "Java Archive (JAR)",
        "epub" => "EPUB E-Book",
        _ => "ZIP Archive",
    };

    let mut fields = vec![("Container".to_string(), "ZIP (PK)".to_string())];

    // Inspect End of Central Directory (EOCD) near end of file for an exact entry count.
    if let Ok(mut file) = File::open(path) {
        let window = file_size.min(MAX_ZIP_EOCD_WINDOW);
        let start = file_size.saturating_sub(window);
        if file.seek(SeekFrom::Start(start)).is_ok() {
            let mut tail = Vec::new();
            if file.take(window).read_to_end(&mut tail).is_ok()
                && let Some(eocd_pos) = find_zip_eocd(&tail)
                && eocd_pos + 22 <= tail.len()
            {
                let total_entries = u16::from_le_bytes([tail[eocd_pos + 10], tail[eocd_pos + 11]]);
                fields.push(("Entries".to_string(), total_entries.to_string()));
            }
        }
    }

    // Also inspect the first local file header name if present.
    let head = read_bounded(path, 1024, cancel)?;
    if head.len() >= 30 && &head[0..4] == b"PK\x03\x04" {
        let name_len = usize::from(u16::from_le_bytes([head[26], head[27]]));
        if name_len > 0 && 30 + name_len <= head.len() {
            let first_entry = String::from_utf8_lossy(&head[30..30 + name_len]).into_owned();
            fields.push(("First Entry".to_string(), first_entry));
        }
    }

    fields.push(("Size".to_string(), format_bytes(file_size)));

    Ok(PreviewContent::DocumentMetadata {
        kind: kind.to_string(),
        summary_fields: fields,
    })
}

fn find_zip_eocd(tail: &[u8]) -> Option<usize> {
    if tail.len() < 22 {
        return None;
    }
    let sig = [0x50, 0x4B, 0x05, 0x06];
    let mut i = tail.len() - 22;
    loop {
        if tail[i..i + 4] == sig {
            return Some(i);
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
