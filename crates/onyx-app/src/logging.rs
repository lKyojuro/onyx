use onyx_config::ConfigStore;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use tracing_subscriber::EnvFilter;

const MAX_LOG_BYTES: usize = 1024 * 1024;
const LOG_NAMES: [&str; 3] = ["onyx.log", "onyx.1.log", "onyx.2.log"];
const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_DELETE: u32 = 4;

pub(crate) struct LogGuard {
    sender: SyncSender<Option<Vec<u8>>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(None);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn init() -> Option<LogGuard> {
    let directory = ConfigStore::default_location().dir().join("logs");
    let (sender, receiver) = mpsc::sync_channel::<Option<Vec<u8>>>(256);
    let worker = thread::Builder::new()
        .name("onyx-log".into())
        .spawn(move || {
            let mut file = RollingLog::open(directory).ok();
            while let Ok(Some(record)) = receiver.recv() {
                if let Some(log) = &mut file {
                    if log.write_record(&record).is_ok() {
                        continue;
                    }
                    file = None;
                }
                let _ = io::stderr().write_all(&record);
            }
            if let Some(log) = file {
                let _ = log.file.sync_all();
            }
        });

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false);
    match worker {
        Ok(worker) => {
            let output = sender.clone();
            subscriber
                .with_writer(move || QueuedLog(output.clone()))
                .init();
            Some(LogGuard {
                sender,
                worker: Some(worker),
            })
        }
        Err(_) => {
            subscriber.with_writer(io::stderr).init();
            None
        }
    }
}

struct QueuedLog(SyncSender<Option<Vec<u8>>>);

impl Write for QueuedLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let record = if bytes.len() <= MAX_LOG_BYTES {
            bytes.to_vec()
        } else {
            let prefix = &bytes[..MAX_LOG_BYTES - 1];
            let end =
                std::str::from_utf8(prefix).map_or_else(|error| error.valid_up_to(), str::len);
            let mut record = prefix[..end].to_vec();
            record.push(b'\n');
            record
        };
        // Do not block the UI when the worker or disk cannot keep up with log traffic.
        let _ = self.0.try_send(Some(record));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RollingLog {
    directory: PathBuf,
    file: File,
    len: u64,
}

impl RollingLog {
    fn open(directory: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&directory)?;
        let (file, len) = Self::open_active(&directory)?;
        Ok(Self {
            directory,
            file,
            len,
        })
    }

    fn open_active(directory: &Path) -> io::Result<(File, u64)> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            // Allow rotation while excluding other writers.
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(directory.join(LOG_NAMES[0]))?;
        let len = file.seek(SeekFrom::End(0))?;
        Ok((file, len))
    }

    fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.len.saturating_add(bytes.len() as u64) > MAX_LOG_BYTES as u64 {
            self.rotate()?;
        }
        self.file.write_all(bytes)?;
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.sync_all()?;
        for names in LOG_NAMES.windows(2).rev() {
            let source = self.directory.join(names[0]);
            let destination = self.directory.join(names[1]);
            match fs::rename(source, destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        (self.file, self.len) = Self::open_active(&self.directory)?;
        Ok(())
    }
}
