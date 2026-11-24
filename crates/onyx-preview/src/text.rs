use crate::{PreviewContent, PreviewLimits};
use onyx_core::{OnyxError, OnyxResult};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Detects a human-readable syntax/language label from a file path's extension or well-known name.
#[must_use]
pub fn detect_language(path: &Path) -> Option<&'static str> {
    if let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
    {
        let lang = match ext.as_str() {
            "rs" => "Rust",
            "toml" => "TOML",
            "json" | "jsonc" | "jsonl" => "JSON",
            "md" | "markdown" | "mdx" => "Markdown",
            "ts" | "tsx" | "mts" | "cts" => "TypeScript",
            "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
            "py" | "pyw" | "pyi" => "Python",
            "c" | "h" => "C",
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "C++",
            "ps1" | "psm1" | "psd1" => "PowerShell",
            "slint" => "Slint",
            "yaml" | "yml" => "YAML",
            "xml" | "svg" | "xaml" | "csproj" | "plist" | "manifest" => "XML",
            "html" | "htm" | "xhtml" => "HTML",
            "css" | "scss" | "sass" | "less" => "CSS",
            "sql" => "SQL",
            "ini" | "cfg" | "conf" | "env" | "properties" => "INI",
            "txt" | "log" | "csv" | "tsv" | "sh" | "bash" | "zsh" | "bat" | "cmd" | "gitignore"
            | "gitattributes" | "editorconfig" | "lock" => "Plain Text",
            _ => return None,
        };
        return Some(lang);
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase);

    match file_name.as_deref() {
        Some(
            "readme" | "license" | "copying" | "authors" | "changelog" | "makefile" | "dockerfile"
            | "cargo.lock",
        ) => Some("Plain Text"),
        _ => None,
    }
}

/// Checks if a byte buffer looks like text (no NUL bytes and predominantly printable/whitespace).
#[must_use]
pub(crate) fn looks_like_text(sample: &[u8]) -> bool {
    if sample.is_empty() {
        return true;
    }
    let inspect_len = sample.len().min(8192);
    let slice = &sample[..inspect_len];
    if slice.contains(&0) {
        return false;
    }

    let mut suspicious = 0usize;
    for &b in slice {
        let is_ok = b == b'\n'
            || b == b'\r'
            || b == b'\t'
            || b == 0x0C
            || b == 0x1B
            || (0x20..=0x7E).contains(&b)
            || b >= 0x80;
        if !is_ok {
            suspicious += 1;
        }
    }

    // Allow up to 10% non-printable control bytes before classifying as binary.
    suspicious * 10 <= inspect_len
}

/// Reads bounded UTF-8/lossy text from `path` according to `limits.max_text_bytes`.
pub(crate) fn load_text_preview(
    path: &Path,
    language: &str,
    limits: &PreviewLimits,
    cancel: &CancellationToken,
) -> OnyxResult<PreviewContent> {
    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let file = File::open(path)?;
    let max_bytes = limits.max_text_bytes;
    let read_cap = max_bytes.saturating_add(1);
    let mut buf = Vec::with_capacity(read_cap.min(64 * 1024));
    file.take(read_cap as u64).read_to_end(&mut buf)?;

    if cancel.is_cancelled() {
        return Err(OnyxError::Cancelled);
    }

    let truncated = buf.len() > max_bytes;
    if truncated {
        buf.truncate(max_bytes);
        trim_incomplete_utf8_tail(&mut buf);
    }

    Ok(decode_text_buffer(buf, language, truncated))
}

/// Decodes an already-read byte buffer into [`PreviewContent::Text`].
pub(crate) fn decode_text_buffer(
    mut buf: Vec<u8>,
    language: &str,
    truncated: bool,
) -> PreviewContent {
    // Strip UTF-8 BOM if present at start.
    if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
        buf.drain(..3);
    }

    let text = match String::from_utf8(buf) {
        Ok(valid) => valid,
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    };

    let line_count = if text.is_empty() {
        0
    } else {
        text.lines().count()
    };

    PreviewContent::Text {
        text,
        language: language.to_string(),
        line_count,
        truncated,
    }
}

/// Removes up to 3 trailing bytes if truncation split a multi-byte UTF-8 sequence.
fn trim_incomplete_utf8_tail(buf: &mut Vec<u8>) {
    let len = buf.len();
    if len == 0 {
        return;
    }

    // Walk back at most 3 continuation bytes (0b10xx_xxxx) to inspect the leading byte.
    let mut idx = len - 1;
    let mut cont_count = 0usize;
    while cont_count < 3 && (buf[idx] & 0xC0) == 0x80 {
        cont_count += 1;
        if idx == 0 {
            return;
        }
        idx -= 1;
    }

    let lead = buf[idx];
    let expected_len = if lead & 0x80 == 0 {
        1
    } else if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else if lead & 0xF8 == 0xF0 {
        4
    } else {
        // Invalid byte anyway; let lossy decoder handle it.
        return;
    };

    let actual_len = len - idx;
    if actual_len < expected_len {
        buf.truncate(idx);
    }
}
