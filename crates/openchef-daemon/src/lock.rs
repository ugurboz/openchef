//! The single-instance lock (`~/.openchef/daemon.lock`, §5.3 step 1).

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::Path;

use crate::Error;

/// An OS file lock, so the kernel releases it when the daemon dies, however it dies.
#[derive(Debug)]
pub struct Lock {
    file: Option<File>,
}

impl Lock {
    /// Takes the lock at `path` without waiting.
    ///
    /// # Errors
    ///
    /// [`Error::Running`] when another daemon holds it; I/O failures.
    pub fn acquire(path: &Path) -> Result<Self, Error> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::io(format!("create {}", dir.display()), e))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| Error::io("open lock", e))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::Running),
            Err(TryLockError::Error(e)) => return Err(Error::io("lock", e)),
        }
        // The pid is informational (diagnostics); the lock itself is the authority.
        let wrote = file
            .set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| writeln!(file, "{}", std::process::id()));
        drop(wrote);
        Ok(Self { file: Some(file) })
    }

    /// Drops the lock. Idempotent.
    ///
    /// # Errors
    ///
    /// When the OS refuses to unlock.
    pub fn release(&mut self) -> Result<(), Error> {
        match self.file.take() {
            Some(f) => f.unlock().map_err(|e| Error::io("unlock", e)),
            None => Ok(()),
        }
    }
}
