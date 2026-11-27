//! Native Windows transfer primitives for the blocking file operations engine.

use crate::to_wide_null;
use onyx_core::{OnyxError, OnyxResult, normalize_display_path, to_extended_length_path};
use std::collections::HashMap;
use std::ffi::{OsStr, c_void};
use std::fs::{self, File, OpenOptions};
use std::mem::size_of;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::ptr;
use std::sync::{LazyLock, Mutex};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_INVALID_PARAMETER,
    ERROR_NOT_SUPPORTED, ERROR_REQUEST_ABORTED, FILETIME, HANDLE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    COPY_FILE_FAIL_IF_EXISTS, COPY_FILE_NO_BUFFERING, COPYFILE_FLAGS,
    COPYFILE2_CALLBACK_CHUNK_FINISHED, COPYFILE2_CALLBACK_STREAM_FINISHED,
    COPYFILE2_EXTENDED_PARAMETERS, COPYFILE2_MESSAGE, COPYFILE2_MESSAGE_ACTION,
    COPYFILE2_PROGRESS_CANCEL, COPYFILE2_PROGRESS_CONTINUE, CopyFile2, CreateFileW,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_END_OF_FILE_INFO,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileEndOfFileInfo, GetDiskFreeSpaceW, GetDriveTypeW, GetFileTime, GetVolumeInformationW,
    OPEN_EXISTING, SetFileInformationByHandle, SetFileTime,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY,
    StorageDeviceSeekPenaltyProperty,
};
use windows::Win32::System::SystemServices::FILE_SUPPORTS_BLOCK_REFCOUNTING;
use windows::core::PCWSTR;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageMediaKind {
    Ssd,
    Hdd,
    Unknown,
}

static MEDIA_CACHE: LazyLock<Mutex<HashMap<char, StorageMediaKind>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: The guard owns the handle returned by a successful CreateFileW.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn drive_letter(path: &Path) -> Option<char> {
    let display = normalize_display_path(path);
    match display.as_bytes() {
        [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic() => {
            Some(char::from(letter.to_ascii_uppercase()))
        }
        _ => None,
    }
}

fn drive_root(letter: char) -> Vec<u16> {
    to_wide_null(OsStr::new(&format!("{letter}:\\")))
}

#[must_use]
pub fn detect_storage_media(path: &Path) -> StorageMediaKind {
    let Some(letter) = drive_letter(path) else {
        return StorageMediaKind::Unknown;
    };

    let mut cache = MEDIA_CACHE.lock().unwrap_or_else(|err| err.into_inner());
    *cache
        .entry(letter)
        .or_insert_with(|| query_storage_media(letter))
}

fn query_storage_media(letter: char) -> StorageMediaKind {
    let root = drive_root(letter);
    // SAFETY: root is a null-terminated drive root. Only fixed/removable local media are probed.
    if !matches!(unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }, 2 | 3) {
        return StorageMediaKind::Unknown;
    }

    let device = to_wide_null(OsStr::new(&format!(r"\\.\{letter}:")));
    // SAFETY: device is a null-terminated volume device path. Optional pointers are null.
    let Ok(handle) = (unsafe {
        CreateFileW(
            PCWSTR(device.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }) else {
        return StorageMediaKind::Unknown;
    };
    let handle = HandleGuard(handle);
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut descriptor = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned = 0;
    let descriptor_size = size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32;
    // SAFETY: The volume handle is open and each stack buffer is valid for the declared size.
    let result = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(ptr::from_ref(&query).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(ptr::from_mut(&mut descriptor).cast()),
            descriptor_size,
            Some(&raw mut returned),
            None,
        )
    };
    if result.is_err() || returned < descriptor_size {
        StorageMediaKind::Unknown
    } else if descriptor.IncursSeekPenalty {
        StorageMediaKind::Hdd
    } else {
        StorageMediaKind::Ssd
    }
}

#[must_use]
pub fn is_path_on_ssd(path: &Path) -> Option<bool> {
    match detect_storage_media(path) {
        StorageMediaKind::Ssd => Some(true),
        StorageMediaKind::Hdd => Some(false),
        StorageMediaKind::Unknown => None,
    }
}

#[must_use]
pub fn recommended_transfer_workers(src: &Path, dst: &Path) -> usize {
    match (detect_storage_media(src), detect_storage_media(dst)) {
        (StorageMediaKind::Hdd, _) | (_, StorageMediaKind::Hdd) => 1,
        (StorageMediaKind::Ssd, StorageMediaKind::Ssd) => {
            std::thread::available_parallelism().map_or(4, |workers| workers.get().clamp(4, 8))
        }
        _ => 4,
    }
}

fn clone_cluster_size(src: &Path, dst: &Path) -> Option<u64> {
    let letter = drive_letter(src)?;
    if drive_letter(dst) != Some(letter) {
        return None;
    }
    let root = drive_root(letter);
    let root = PCWSTR(root.as_ptr());
    let mut flags = 0;
    let mut sectors = 0;
    let mut bytes = 0;
    // SAFETY: root points to a live null-terminated buffer. The output pointers are writable.
    unsafe {
        GetVolumeInformationW(root, None, None, None, Some(&raw mut flags), None).ok()?;
        if flags & FILE_SUPPORTS_BLOCK_REFCOUNTING == 0 || !matches!(GetDriveTypeW(root), 2 | 3) {
            return None;
        }
        GetDiskFreeSpaceW(
            root,
            Some(&raw mut sectors),
            Some(&raw mut bytes),
            None,
            None,
        )
        .ok()?;
    }
    let cluster = u64::from(sectors) * u64::from(bytes);
    (cluster > 0 && cluster <= 1 << 30).then_some(cluster)
}

fn file_handle(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

fn file_io_error(error: windows::core::Error) -> OnyxError {
    match WIN32_ERROR::from_error(&error) {
        Some(code) => std::io::Error::from_raw_os_error(code.0 as i32).into(),
        None => std::io::Error::other(error).into(),
    }
}

/// Sets the file's logical length without moving its current cursor.
pub fn preallocate_file(file: &File, size: u64) -> OnyxResult<()> {
    let end = FILE_END_OF_FILE_INFO {
        EndOfFile: i64::try_from(size)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?,
    };
    // SAFETY: file owns a live handle, and end is readable for the given size.
    unsafe {
        SetFileInformationByHandle(
            file_handle(file),
            FileEndOfFileInfo,
            ptr::from_ref(&end).cast(),
            size_of::<FILE_END_OF_FILE_INFO>() as u32,
        )
    }
    .map_err(file_io_error)
}

/// Attempts a block clone into a fresh destination. Unsupported transfers can fall back to a copy.
pub fn try_clone_file_refs(src: &Path, dst: &Path) -> OnyxResult<bool> {
    let Some(cluster) = clone_cluster_size(src, dst) else {
        return Ok(false);
    };
    let source = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(src)?;
    let meta = source.metadata()?;
    if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Ok(false);
    }
    let destination = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .attributes(FILE_ATTRIBUTE_NORMAL.0)
        .open(dst)
    {
        Ok(file) => file,
        // Never truncate an existing file: it might be another name for the source.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(err) => return Err(err.into()),
    };

    let cloned = clone_extents(&source, &destination, meta.len(), cluster);
    drop(destination);
    if !cloned {
        match fs::remove_file(dst) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(cloned)
}

fn clone_extents(source: &File, destination: &File, size: u64, cluster: u64) -> bool {
    let aligned_size = size.div_ceil(cluster) * cluster;
    let Ok(aligned_size) = i64::try_from(aligned_size) else {
        return false;
    };
    let max_chunk = ((1 << 30) / cluster * cluster) as i64;
    // ReFS requires the aligned clone range to fit in the destination EOF.
    if preallocate_file(destination, aligned_size as u64).is_err() {
        return false;
    }
    let mut offset = 0;
    while offset < aligned_size {
        let bytes = (aligned_size - offset).min(max_chunk);
        let extent = DUPLICATE_EXTENTS_DATA {
            FileHandle: file_handle(source),
            SourceFileOffset: offset,
            TargetFileOffset: offset,
            ByteCount: bytes,
        };
        let mut returned = 0;
        // SAFETY: Both files own live handles. extent is readable and returned is writable.
        let result = unsafe {
            DeviceIoControl(
                file_handle(destination),
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                Some(ptr::from_ref(&extent).cast()),
                size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                None,
                0,
                Some(&raw mut returned),
                None,
            )
        };
        if result.is_err() {
            return false;
        }
        offset += bytes;
    }

    (aligned_size as u64 == size || preallocate_file(destination, size).is_ok())
        && copy_file_times(source, destination)
}

fn copy_file_times(source: &File, destination: &File) -> bool {
    let mut created = FILETIME::default();
    let mut accessed = FILETIME::default();
    let mut modified = FILETIME::default();
    // SAFETY: Both files own live handles. Each timestamp pointer addresses a valid FILETIME.
    unsafe {
        GetFileTime(
            file_handle(source),
            Some(&raw mut created),
            Some(&raw mut accessed),
            Some(&raw mut modified),
        )
        .and_then(|()| {
            SetFileTime(
                file_handle(destination),
                Some(&raw const created),
                Some(&raw const accessed),
                Some(&raw const modified),
            )
        })
        .is_ok()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyFileOptions {
    pub no_buffering: bool,
    pub overwrite: bool,
}

fn copy_path(path: &Path) -> OnyxResult<Vec<u16>> {
    // Resolving relative paths also handles a short relative path under a long current directory.
    let path = std::path::absolute(path)?;
    let wide = to_wide_null(to_extended_length_path(&path).as_os_str());
    if wide[..wide.len() - 1].contains(&0) {
        return Err(OnyxError::InvalidPath {
            message: "File path contains a NUL character".into(),
        });
    }
    Ok(wide)
}

struct CopyContext<F, C> {
    on_progress: F,
    is_cancelled: C,
    panicked: bool,
}

unsafe extern "system" fn copyfile2_callback<F, C>(
    message: *const COPYFILE2_MESSAGE,
    context: *const c_void,
) -> COPYFILE2_MESSAGE_ACTION
where
    F: FnMut(u64, u64),
    C: Fn() -> bool,
{
    // SAFETY: copy_file_fast supplies a live, exclusively borrowed context for the synchronous callback.
    let context = unsafe { &mut *context.cast_mut().cast::<CopyContext<F, C>>() };
    let action = catch_unwind(AssertUnwindSafe(|| {
        if context.panicked || (context.is_cancelled)() {
            return COPYFILE2_PROGRESS_CANCEL;
        }
        // SAFETY: CopyFile2 supplies a readable message for the duration of this callback.
        let message = unsafe { &*message };
        let counts = match message.Type {
            COPYFILE2_CALLBACK_CHUNK_FINISHED => {
                // SAFETY: The message discriminator selects ChunkFinished.
                let chunk = unsafe { message.Info.ChunkFinished };
                Some((chunk.uliTotalBytesTransferred, chunk.uliTotalFileSize))
            }
            COPYFILE2_CALLBACK_STREAM_FINISHED => {
                // SAFETY: The message discriminator selects StreamFinished.
                let stream = unsafe { message.Info.StreamFinished };
                Some((stream.uliTotalBytesTransferred, stream.uliTotalFileSize))
            }
            _ => None,
        };
        if let Some((done, total)) = counts {
            (context.on_progress)(done, total);
            if (context.is_cancelled)() {
                return COPYFILE2_PROGRESS_CANCEL;
            }
        }
        COPYFILE2_PROGRESS_CONTINUE
    }));
    match action {
        Ok(action) => action,
        Err(payload) => {
            // A panic payload can itself have a panicking destructor. Keep it out of the ABI.
            std::mem::forget(payload);
            context.panicked = true;
            COPYFILE2_PROGRESS_CANCEL
        }
    }
}

/// Copies a file with native progress and cancellation.
pub fn copy_file_fast<F, C>(
    src: &Path,
    dst: &Path,
    options: CopyFileOptions,
    is_cancelled: C,
    on_progress: F,
) -> OnyxResult<()>
where
    F: FnMut(u64, u64),
    C: Fn() -> bool,
{
    let src_w = copy_path(src)?;
    let dst_w = copy_path(dst)?;
    let mut context = CopyContext {
        on_progress,
        is_cancelled,
        panicked: false,
    };
    let mut no_buffering = options.no_buffering;

    loop {
        if (context.is_cancelled)() {
            return Err(OnyxError::Cancelled);
        }
        let mut flags = COPYFILE_FLAGS(0);
        if no_buffering {
            flags |= COPY_FILE_NO_BUFFERING;
        }
        if !options.overwrite {
            flags |= COPY_FILE_FAIL_IF_EXISTS;
        }
        let parameters = COPYFILE2_EXTENDED_PARAMETERS {
            dwSize: size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
            dwCopyFlags: flags,
            pfCancel: ptr::null_mut(),
            pProgressRoutine: Some(copyfile2_callback::<F, C>),
            pvCallbackContext: ptr::from_mut(&mut context).cast(),
        };
        // SAFETY: Both paths are null-terminated and the callback context and parameters
        // remain alive and exclusively available for the entire synchronous call.
        let result = unsafe {
            CopyFile2(
                PCWSTR(src_w.as_ptr()),
                PCWSTR(dst_w.as_ptr()),
                Some(&raw const parameters),
            )
        };
        if context.panicked {
            return Err(OnyxError::OperationFailed {
                message: "Copy progress callback panicked".into(),
            });
        }
        let Err(error) = result else {
            return Ok(());
        };
        let code = WIN32_ERROR::from_error(&error);
        // COPYFILE2_PROGRESS_CANCEL tells Windows to remove its own partial destination.
        if code == Some(ERROR_REQUEST_ABORTED) || (context.is_cancelled)() {
            return Err(OnyxError::Cancelled);
        }
        if no_buffering && matches!(code, Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED)) {
            no_buffering = false;
            continue;
        }
        if matches!(code, Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS)) {
            return Err(OnyxError::AlreadyExists {
                path: dst.to_path_buf(),
            });
        }
        return Err(file_io_error(error));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tempfile::TempDir;

    #[test]
    fn media_detection_and_worker_policy() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        let media = detect_storage_media(path);
        assert_eq!(detect_storage_media(path), media);
        let relative = Path::new("relative");
        let unc = Path::new(r"\\server\share\file");
        assert_eq!(detect_storage_media(relative), StorageMediaKind::Unknown);
        assert_eq!(detect_storage_media(unc), StorageMediaKind::Unknown);
        assert_eq!(drive_letter(Path::new(r"c:relative")), None);
        assert_eq!(drive_letter(Path::new(r"\\?\c:\test")), Some('C'));
        match media {
            StorageMediaKind::Ssd => {
                assert_eq!(is_path_on_ssd(path), Some(true));
                assert!((4..=8).contains(&recommended_transfer_workers(path, path)));
                assert_eq!(recommended_transfer_workers(path, unc), 4);
            }
            StorageMediaKind::Hdd => {
                assert_eq!(is_path_on_ssd(path), Some(false));
                assert_eq!(recommended_transfer_workers(path, unc), 1);
            }
            StorageMediaKind::Unknown => {
                assert_eq!(is_path_on_ssd(path), None);
                assert_eq!(recommended_transfer_workers(path, path), 4);
            }
        }
    }

    #[test]
    fn preallocates_without_moving_the_cursor() {
        use std::io::{Seek, Write};

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preallocated.bin");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"initial").unwrap();
        preallocate_file(&file, 65_536).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 65_536);
        assert_eq!(file.stream_position().unwrap(), 7);
        file.write_all(b"!").unwrap();
        assert_eq!(&fs::read(path).unwrap()[..8], b"initial!");
    }

    #[test]
    fn copy_file_fast_small_reports_progress_and_honors_conflicts() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.bin");
        let dst = dir.path().join("destination.bin");
        fs::write(&src, b"data").unwrap();

        let mut progress = None;
        copy_file_fast(
            &src,
            &dst,
            CopyFileOptions::default(),
            || false,
            |done, total| {
                progress = Some((done, total));
            },
        )
        .unwrap();
        assert_eq!(progress, Some((4, 4)));
        assert_eq!(fs::read(&dst).unwrap(), b"data");

        fs::write(&src, b"replacement").unwrap();
        let result = copy_file_fast(&src, &dst, CopyFileOptions::default(), || false, |_, _| {});
        assert!(matches!(result, Err(OnyxError::AlreadyExists { .. })));
        assert_eq!(fs::read(&dst).unwrap(), b"data");

        let result = copy_file_fast(&src, &dst, CopyFileOptions::default(), || true, |_, _| {});
        assert!(matches!(result, Err(OnyxError::Cancelled)));
        assert_eq!(fs::read(&dst).unwrap(), b"data");
    }

    #[test]
    fn copy_file_fast_five_mib_unbuffered() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.bin");
        let dst = dir.path().join("destination.bin");
        let contents = vec![0x7d; 5 * 1024 * 1024];
        fs::write(&src, &contents).unwrap();
        fs::write(&dst, b"old").unwrap();

        copy_file_fast(
            &src,
            &dst,
            CopyFileOptions {
                no_buffering: true,
                overwrite: true,
            },
            || false,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(fs::read(dst).unwrap(), contents);
    }

    #[test]
    fn copy_file_fast_cancels_in_flight_and_removes_destination() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.bin");
        let dst = dir.path().join("destination.bin");
        File::create(&src)
            .unwrap()
            .set_len(17 * 1024 * 1024)
            .unwrap();
        let saw_progress = Cell::new(false);

        let result = copy_file_fast(
            &src,
            &dst,
            CopyFileOptions::default(),
            || saw_progress.get(),
            |done, _| saw_progress.set(done > 0),
        );
        assert!(saw_progress.get());
        assert!(matches!(result, Err(OnyxError::Cancelled)));
        assert!(!dst.exists());
        assert!(src.is_file());
    }

    #[test]
    fn clone_preserves_contents_size_and_timestamp_or_falls_back_cleanly() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.bin");
        let dst = dir.path().join("destination.bin");
        for size in [0, 65_536, 65_539] {
            let contents = vec![0x5a; size];
            fs::write(&src, &contents).unwrap();
            let modified = fs::metadata(&src).unwrap().modified().unwrap();
            if try_clone_file_refs(&src, &dst).unwrap() {
                assert_eq!(fs::read(&dst).unwrap(), contents);
                assert_eq!(fs::metadata(&dst).unwrap().modified().unwrap(), modified);
                fs::remove_file(&dst).unwrap();
            } else {
                assert!(!dst.exists());
                // An aligned ordinary file on a block-cloning volume should succeed.
                assert!(size == 65_539 || clone_cluster_size(&src, &dst).is_none());
            }
            assert_eq!(fs::read(&src).unwrap(), contents);
        }
        fs::write(&dst, b"keep").unwrap();
        assert!(!try_clone_file_refs(&src, &dst).unwrap());
        assert_eq!(fs::read(&dst).unwrap(), b"keep");
        assert!(!try_clone_file_refs(&src, &src).unwrap());
        assert_eq!(fs::metadata(&src).unwrap().len(), 65_539);
    }
}
