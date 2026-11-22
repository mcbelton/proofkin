//! Small JSON files a profile keeps (shared folders, schedules, plugins) are written
//! by the agent, its web UI and the CLI, which may run at the same time in separate
//! processes. Writes go to a temporary file renamed into place, so nobody reads a
//! half-written file; read-modify-write updates hold a lock file, so nobody's change
//! is lost.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

/// How long a lock file may be held before it counts as left behind by a crash.
const STALE: Duration = Duration::from_secs(30);

/// Writes `bytes` to `path` atomically (a temporary file renamed over it).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("the file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(
        ".{name}.{}.tmp",
        data_encoding::HEXLOWER.encode(&iroh::SecretKey::generate().to_bytes()[..6])
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    let mut tries = 0;
    loop {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            // Windows: a reader holding the file without delete sharing; retry briefly.
            Err(_) if tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e).with_context(|| format!("writing {}", path.display()));
            }
        }
    }
}

/// An exclusive lock on a file (`<file>.lock`), released when dropped.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Waits up to 10 s for the lock on `target`; a lock older than 30 s was left
    /// behind by a crash and is taken over.
    pub fn acquire(target: &Path) -> Result<Self> {
        let mut name = target.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let path = target.with_file_name(name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Windows: a lock file still being deleted (its holder just let go, and another
        // handle has it open) can't be created again until it is gone, and says access
        // denied meanwhile; wait that out briefly, as for a held lock.
        let mut denied = 0;
        for _ in 0..200 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > STALE);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) if cfg!(windows) && in_transition(&e) && denied < 40 => {
                    denied += 1;
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("locking {}", target.display()));
                }
            }
        }
        bail!(
            "{} is being changed by another proofkin process; try again",
            target.display()
        )
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether creating a lock file failed only because the last one is being deleted
/// (Windows: access denied, a sharing violation or delete pending while its holder's
/// delete is under way).
fn in_transition(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ResourceBusy
    ) || e.raw_os_error() == Some(303)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_atomic_and_locks_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.json");
        write_atomic(&file, b"one").unwrap();
        write_atomic(&file, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());

        let lock = FileLock::acquire(&file).unwrap();
        let (trying, tried) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn({
            let file = file.clone();
            move || {
                trying.send(()).unwrap();
                let _second = FileLock::acquire(&file).unwrap();
                std::time::Instant::now()
            }
        });
        tried.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let released = std::time::Instant::now();
        drop(lock);
        // The second lock is taken only once the first is released, however late
        // its thread started.
        assert!(waiter.join().unwrap() >= released, "waited for the first");
        assert!(!dir.path().join("x.json.lock").exists(), "released");
    }

    /// A lock file that is being deleted while another handle still has it open (on
    /// Windows, access denied until it is gone) is waited for, not an error.
    #[test]
    fn a_lock_file_still_open_while_it_is_deleted_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.json");
        let lock = dir.path().join("x.json.lock");
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Read, write and delete sharing, so it can be deleted while it is open.
            open.share_mode(7);
        }
        let held = open.open(&lock).unwrap();
        std::fs::remove_file(&lock).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });
        let taken = FileLock::acquire(&file).unwrap();
        release.join().unwrap();
        drop(taken);
        assert!(!lock.exists(), "released");
    }
}
