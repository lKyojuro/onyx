//! Safe, bounded on-demand file preview engine for Onyx.
//!
//! All files are treated strictly as untrusted input: no scripts, macros, or active content
//! are ever executed, and all reads, decodes, and scans are strictly bounded in size and time.

pub mod binary;
pub mod directory;
pub mod document;
pub mod image_preview;
pub mod text;

use onyx_core::{OnyxError, OnyxResult, to_extended_length_path};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Safety and performance limits governing preview generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewLimits {
    pub max_text_bytes: usize,
    pub max_image_bytes: u64,
    pub max_image_dim: u32,
    pub timeout_ms: u64,
    pub max_dir_scan: usize,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: 256 * 1024,
            max_image_bytes: 16 * 1024 * 1024,
            max_image_dim: 1024,
            timeout_ms: 2500,
            max_dir_scan: 2000,
        }
    }
}

/// Safe preview representation produced by [`PreviewEngine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewContent {
    Text {
        text: String,
        language: String,
        line_count: usize,
        truncated: bool,
    },
    Image {
        width: u32,
        height: u32,
        original_width: u32,
        original_height: u32,
        rgba: Vec<u8>,
        format: String,
    },
    DocumentMetadata {
        kind: String,
        summary_fields: Vec<(String, String)>,
    },
    DirectorySummary {
        item_count: usize,
        dir_count: usize,
        file_count: usize,
        immediate_size_bytes: u64,
        truncated: bool,
    },
    BinaryMetadata {
        mime_guess: String,
        size_bytes: u64,
        hex_header: String,
    },
    Unavailable {
        reason: String,
    },
}

/// Async, bounded, untrusted-input-safe preview engine.
#[derive(Debug, Clone, Default)]
pub struct PreviewEngine {
    limits: PreviewLimits,
}

impl PreviewEngine {
    /// Creates a preview engine with default [`PreviewLimits`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a preview engine with custom [`PreviewLimits`].
    #[must_use]
    pub fn with_limits(limits: PreviewLimits) -> Self {
        Self { limits }
    }

    /// Returns the active [`PreviewLimits`].
    #[must_use]
    pub fn limits(&self) -> &PreviewLimits {
        &self.limits
    }

    /// Loads a safe preview for `path` on a blocking worker pool within `limits.timeout_ms`.
    pub async fn load_preview(
        &self,
        path: PathBuf,
        cancel: CancellationToken,
    ) -> OnyxResult<PreviewContent> {
        if cancel.is_cancelled() {
            return Err(OnyxError::Cancelled);
        }

        let limits = self.limits.clone();
        let timeout_duration = Duration::from_millis(limits.timeout_ms.max(1));
        let worker_cancel = cancel.child_token();
        let worker_cancel_bg = worker_cancel.clone();

        let join_handle = tokio::task::spawn_blocking(move || {
            load_preview_blocking(&path, &limits, &worker_cancel_bg)
        });

        match tokio::time::timeout(timeout_duration, join_handle).await {
            Ok(Ok(result)) => result,
            Ok(Err(join_err)) => Err(OnyxError::Preview {
                message: format!("preview worker task failed: {join_err}"),
            }),
            Err(_) => {
                worker_cancel.cancel();
                if cancel.is_cancelled() {
                    Err(OnyxError::Cancelled)
                } else {
                    Ok(PreviewContent::Unavailable {
                        reason: format!("Preview timed out after {} ms", self.limits.timeout_ms),
                    })
                }
            }
        }
    }
}

fn load_preview_blocking(
    path: &Path,
    limits: &PreviewLimits,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let ext_path = to_extended_length_path(path);
    let meta = match std::fs::metadata(&ext_path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(OnyxError::NotFound {
                path: path.to_path_buf(),
            });
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(OnyxError::PermissionDenied {
                path: path.to_path_buf(),
            });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    };

    if meta.is_dir() {
        return directory::load_directory_summary(path, limits, cancel);
    }

    if !meta.is_file() {
        return Ok(PreviewContent::Unavailable {
            reason: "Unsupported special filesystem object".to_string(),
        });
    }

    let file_size = meta.len();

    // Known text/code extensions go straight to bounded text reader (zero script execution).
    if let Some(language) = text::detect_language(path) {
        return text::load_text_preview(&ext_path, language, limits, cancel);
    }

    // Read a small bounded header (8 KiB) for magic-byte sniffing.
    let header = read_header_sample(&ext_path, 8192, cancel)?;

    // 1. Known raster images (by extension or magic bytes)
    if image_preview::is_supported_image_extension(path)
        || image_preview::is_supported_image_magic(&header)
    {
        return image_preview::load_image_preview(&ext_path, file_size, limits, cancel);
    }

    // 2. Document / audio / video / archive containers via bounded header inspection
    if let Some(doc_preview) =
        document::try_load_document_metadata(&ext_path, &header, file_size, cancel)?
    {
        return Ok(doc_preview);
    }

    // 3. Heuristic plain text for unknown extensions (e.g. extensionless scripts/notes)
    if text::looks_like_text(&header) {
        return text::load_text_preview(&ext_path, "Plain Text", limits, cancel);
    }

    // 4. Safe binary metadata + hex header fallback
    Ok(binary::build_binary_metadata(path, &header, file_size))
}

fn read_header_sample(
    path: &Path,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> OnyxResult<Vec<u8>> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }
    let file = match File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(OnyxError::PermissionDenied {
                path: path.to_path_buf(),
            });
        }
        Err(err) => return Err(OnyxError::Io(err)),
    };
    let mut buf = Vec::with_capacity(max_bytes.min(8192));
    file.take(max_bytes as u64).read_to_end(&mut buf)?;
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }
    Ok(buf)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn text_preview_detects_languages_and_truncates_oversized_files() {
        let dir = TempDir::new().unwrap();
        let rs_path = dir.path().join("sample.rs");
        fs::write(&rs_path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        let engine = PreviewEngine::new();
        let content = engine
            .load_preview(rs_path, CancellationToken::new())
            .await
            .unwrap();
        match content {
            PreviewContent::Text {
                text,
                language,
                line_count,
                truncated,
            } => {
                assert_eq!(language, "Rust");
                assert_eq!(line_count, 3);
                assert!(!truncated);
                assert!(text.contains("fn main"));
            }
            other => panic!("expected Text preview, got {other:?}"),
        }

        // Verify language detection across required extensions
        let expected_langs = [
            ("a.toml", "TOML"),
            ("a.json", "JSON"),
            ("a.md", "Markdown"),
            ("a.ts", "TypeScript"),
            ("a.js", "JavaScript"),
            ("a.py", "Python"),
            ("a.c", "C"),
            ("a.cpp", "C++"),
            ("a.ps1", "PowerShell"),
            ("a.slint", "Slint"),
            ("a.yaml", "YAML"),
            ("a.xml", "XML"),
            ("a.html", "HTML"),
            ("a.css", "CSS"),
            ("a.sql", "SQL"),
            ("a.ini", "INI"),
        ];
        for (filename, expected) in expected_langs {
            assert_eq!(text::detect_language(Path::new(filename)), Some(expected));
        }

        // Oversized text truncation
        let small_limits = PreviewLimits {
            max_text_bytes: 64,
            ..PreviewLimits::default()
        };
        let limited_engine = PreviewEngine::with_limits(small_limits);
        let big_txt = dir.path().join("big.txt");
        fs::write(&big_txt, "line1234567890\n".repeat(20)).unwrap();

        let truncated_preview = limited_engine
            .load_preview(big_txt, CancellationToken::new())
            .await
            .unwrap();
        match truncated_preview {
            PreviewContent::Text {
                text, truncated, ..
            } => {
                assert!(truncated);
                assert!(text.len() <= 64);
            }
            other => panic!("expected truncated Text preview, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn png_image_decode_and_thumbnail_resize() {
        let dir = TempDir::new().unwrap();
        let img_path = dir.path().join("gradient.png");

        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(200, 100, |x, y| {
            Rgba([(x % 255) as u8, (y % 255) as u8, 128, 255])
        });
        img.save(&img_path).unwrap();

        let limits = PreviewLimits {
            max_image_dim: 64,
            ..PreviewLimits::default()
        };
        let engine = PreviewEngine::with_limits(limits);
        let preview = engine
            .load_preview(img_path, CancellationToken::new())
            .await
            .unwrap();

        match preview {
            PreviewContent::Image {
                width,
                height,
                original_width,
                original_height,
                rgba,
                format,
            } => {
                assert_eq!(original_width, 200);
                assert_eq!(original_height, 100);
                assert_eq!(width, 64);
                assert_eq!(height, 32);
                assert_eq!(format, "PNG");
                assert_eq!(rgba.len(), (width * height * 4) as usize);
            }
            other => panic!("expected Image preview, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pdf_header_and_media_containers_parse_safely() {
        let dir = TempDir::new().unwrap();

        // Synthetic minimal PDF with 2 pages and 1 Pages container
        let pdf_path = dir.path().join("sample.pdf");
        let pdf_bytes = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n\
            2 0 obj << /Type /Pages /Count 2 /Kids [3 0 R 4 0 R] >> endobj\n\
            3 0 obj << /Type /Page >> endobj\n\
            4 0 obj << /Type /Page >> endobj\n%%EOF\n";
        fs::write(&pdf_path, pdf_bytes).unwrap();

        let engine = PreviewEngine::new();
        let pdf_preview = engine
            .load_preview(pdf_path, CancellationToken::new())
            .await
            .unwrap();
        match pdf_preview {
            PreviewContent::DocumentMetadata {
                kind,
                summary_fields,
            } => {
                assert_eq!(kind, "PDF Document");
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "PDF Version" && v == "1.7")
                );
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Estimated Pages" && v == "2")
                );
            }
            other => panic!("expected PDF DocumentMetadata, got {other:?}"),
        }

        // Minimal WAV header
        let wav_path = dir.path().join("tone.wav");
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // Stereo
        wav.extend_from_slice(&44100u32.to_le_bytes());
        wav.extend_from_slice(&176400u32.to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&176400u32.to_le_bytes());
        fs::write(&wav_path, &wav).unwrap();

        let wav_preview = engine
            .load_preview(wav_path, CancellationToken::new())
            .await
            .unwrap();
        match wav_preview {
            PreviewContent::DocumentMetadata {
                kind,
                summary_fields,
            } => {
                assert_eq!(kind, "WAV Audio");
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Sample Rate" && v == "44100 Hz")
                );
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Duration" && v == "1.00 s")
                );
            }
            other => panic!("expected WAV DocumentMetadata, got {other:?}"),
        }

        // Minimal MP4 ftyp box
        let mp4_path = dir.path().join("clip.mp4");
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&24u32.to_be_bytes());
        mp4.extend_from_slice(b"ftyp");
        mp4.extend_from_slice(b"isom");
        mp4.extend_from_slice(&512u32.to_be_bytes());
        mp4.extend_from_slice(b"isommp42");
        fs::write(&mp4_path, &mp4).unwrap();

        let mp4_preview = engine
            .load_preview(mp4_path, CancellationToken::new())
            .await
            .unwrap();
        match mp4_preview {
            PreviewContent::DocumentMetadata {
                kind,
                summary_fields,
            } => {
                assert_eq!(kind, "MP4 / ISO Media");
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Major Brand" && v == "isom")
                );
            }
            other => panic!("expected MP4 DocumentMetadata, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn directory_summary_and_binary_fallback_and_cancellation() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(
            dir.path().join("a.bin"),
            [0x00, 0x01, 0x02, 0xFF, 0x7F, 0x45, 0x4C, 0x46],
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "hello").unwrap();

        let engine = PreviewEngine::new();

        // Directory summary
        let dir_preview = engine
            .load_preview(dir.path().to_path_buf(), CancellationToken::new())
            .await
            .unwrap();
        match dir_preview {
            PreviewContent::DirectorySummary {
                item_count,
                dir_count,
                file_count,
                immediate_size_bytes,
                truncated,
            } => {
                assert_eq!(item_count, 3);
                assert_eq!(dir_count, 1);
                assert_eq!(file_count, 2);
                assert_eq!(immediate_size_bytes, 13);
                assert!(!truncated);
            }
            other => panic!("expected DirectorySummary, got {other:?}"),
        }

        // Binary metadata fallback
        let bin_preview = engine
            .load_preview(dir.path().join("a.bin"), CancellationToken::new())
            .await
            .unwrap();
        match bin_preview {
            PreviewContent::BinaryMetadata {
                mime_guess,
                size_bytes,
                hex_header,
            } => {
                assert_eq!(mime_guess, "application/octet-stream");
                assert_eq!(size_bytes, 8);
                assert!(hex_header.contains("0000: 00 01 02 FF"));
            }
            other => panic!("expected BinaryMetadata, got {other:?}"),
        }

        // Pre-cancelled token returns Cancelled
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = engine
            .load_preview(dir.path().join("b.txt"), cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, OnyxError::Cancelled));
    }

    #[tokio::test]
    async fn extra_containers_and_limits_are_handled_safely() {
        let dir = TempDir::new().unwrap();
        let engine = PreviewEngine::new();

        // ID3v2.3 MP3 header with TIT2 frame
        let mp3_path = dir.path().join("track.mp3");
        let title_payload = b"\x03Onyx Theme";
        let frame_size = (title_payload.len() as u32).to_be_bytes();
        let tag_body_len = 10 + title_payload.len();
        let mut mp3 = Vec::new();
        mp3.extend_from_slice(b"ID3");
        mp3.extend_from_slice(&[3, 0, 0]);
        mp3.extend_from_slice(&[0, 0, 0, (tag_body_len as u8) & 0x7F]);
        mp3.extend_from_slice(b"TIT2");
        mp3.extend_from_slice(&frame_size);
        mp3.extend_from_slice(&[0, 0]);
        mp3.extend_from_slice(title_payload);
        fs::write(&mp3_path, &mp3).unwrap();

        match engine
            .load_preview(mp3_path, CancellationToken::new())
            .await
            .unwrap()
        {
            PreviewContent::DocumentMetadata {
                kind,
                summary_fields,
            } => {
                assert_eq!(kind, "MP3 Audio");
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Title" && v == "Onyx Theme")
                );
            }
            other => panic!("expected MP3 DocumentMetadata, got {other:?}"),
        }

        // FLAC header with STREAMINFO (44100 Hz, 2 ch, 16-bit, 44100 samples = 1.00 s)
        let flac_path = dir.path().join("lossless.flac");
        let mut flac = Vec::new();
        flac.extend_from_slice(b"fLaC");
        flac.extend_from_slice(&[0x80, 0x00, 0x00, 34]); // last block, STREAMINFO, len=34
        let mut si = [0u8; 34];
        // sample_rate=44100 (0x0AC44), channels-1=1 (0b001), bps-1=15 (0b01111), total_samples=44100
        si[10] = 0x0A;
        si[11] = 0xC4;
        si[12] = 0x42; // 0x4 << 4 | (1 << 1) | 0
        si[13] = 0xF0; // 0b1111 << 4 | 0
        si[14] = 0x00;
        si[15] = 0x00;
        si[16] = 0xAC;
        si[17] = 0x44;
        flac.extend_from_slice(&si);
        fs::write(&flac_path, &flac).unwrap();

        match engine
            .load_preview(flac_path, CancellationToken::new())
            .await
            .unwrap()
        {
            PreviewContent::DocumentMetadata {
                kind,
                summary_fields,
            } => {
                assert_eq!(kind, "FLAC Audio");
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Sample Rate" && v == "44100 Hz")
                );
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Channels" && v == "2")
                );
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Bit Depth" && v == "16-bit")
                );
            }
            other => panic!("expected FLAC DocumentMetadata, got {other:?}"),
        }

        // MKV / EBML header
        let mkv_path = dir.path().join("movie.mkv");
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x82, 0x88];
        mkv.extend_from_slice(b"matroska");
        fs::write(&mkv_path, &mkv).unwrap();

        match engine
            .load_preview(mkv_path, CancellationToken::new())
            .await
            .unwrap()
        {
            PreviewContent::DocumentMetadata {
                kind,
                summary_fields,
            } => {
                assert!(kind.contains("Matroska"));
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Container" && v == "EBML")
                );
            }
            other => panic!("expected MKV DocumentMetadata, got {other:?}"),
        }

        // Empty ZIP EOCD container
        let zip_path = dir.path().join("archive.zip");
        let mut eocd = [0u8; 22];
        eocd[0..4].copy_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
        eocd[10..12].copy_from_slice(&3u16.to_le_bytes());
        fs::write(&zip_path, eocd).unwrap();

        match engine
            .load_preview(zip_path, CancellationToken::new())
            .await
            .unwrap()
        {
            PreviewContent::DocumentMetadata {
                kind,
                summary_fields,
            } => {
                assert_eq!(kind, "ZIP Archive");
                assert!(
                    summary_fields
                        .iter()
                        .any(|(k, v)| k == "Entries" && v == "3")
                );
            }
            other => panic!("expected ZIP DocumentMetadata, got {other:?}"),
        }

        // Corrupt PNG degrades safely to Unavailable
        let bad_png = dir.path().join("corrupt.png");
        fs::write(&bad_png, b"\x89PNG\r\n\x1a\nnot_a_real_png").unwrap();
        match engine
            .load_preview(bad_png, CancellationToken::new())
            .await
            .unwrap()
        {
            PreviewContent::Unavailable { reason } => {
                assert!(!reason.is_empty());
            }
            other => panic!("expected Unavailable for corrupt PNG, got {other:?}"),
        }

        // Directory truncation limit
        let capped_dir = dir.path().join("many");
        fs::create_dir(&capped_dir).unwrap();
        for i in 0..5 {
            fs::write(capped_dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let capped_engine = PreviewEngine::with_limits(PreviewLimits {
            max_dir_scan: 3,
            ..PreviewLimits::default()
        });
        match capped_engine
            .load_preview(capped_dir, CancellationToken::new())
            .await
            .unwrap()
        {
            PreviewContent::DirectorySummary {
                item_count,
                truncated,
                ..
            } => {
                assert_eq!(item_count, 3);
                assert!(truncated);
            }
            other => panic!("expected truncated DirectorySummary, got {other:?}"),
        }
    }
}
