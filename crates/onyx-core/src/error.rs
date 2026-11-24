use std::path::PathBuf;
use thiserror::Error;

/// Unified typed error enum across all Onyx core engines.
#[derive(Debug, Error)]
pub enum OnyxError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("permission denied: {path}")]
    PermissionDenied { path: PathBuf },

    #[error("path not found: {path}")]
    NotFound { path: PathBuf },

    #[error("target already exists: {path}")]
    AlreadyExists { path: PathBuf },

    #[error("invalid path: {message}")]
    InvalidPath { message: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("file operation failed: {message}")]
    OperationFailed { message: String },

    #[error("recursive operation refused: {source_path} into {target_path}")]
    RecursiveOperation {
        source_path: PathBuf,
        target_path: PathBuf,
    },

    #[error("insufficient disk space: required {required} bytes, available {available} bytes")]
    InsufficientSpace { required: u64, available: u64 },

    #[error("database error: {message}")]
    Database { message: String },

    #[error("configuration error: {message}")]
    Config { message: String },

    #[error("Windows platform error: {message}")]
    Platform { message: String },

    #[error("preview error: {message}")]
    Preview { message: String },

    #[error("search error: {message}")]
    Search { message: String },
}

pub type OnyxResult<T> = Result<T, OnyxError>;
