// Implementation comes from https://github.com/njsmith/posy/blob/main/src/kvstore.rs
// Licensed under MIT or Apache-2.0

use crate::types::ArtifactHashes;
use crate::utils::retry_interrupted;
use fs4::FileExt;
use std::{
    fs,
    fs::File,
    io,
    io::{Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
};

/// Types that implement this can be used as keys of the [`FileStore`].
pub trait CacheKey {
    /// Returns the path prefix that should be used to store the data for this key.
    fn key(&self) -> PathBuf;
}

impl<T: CacheKey + ?Sized> CacheKey for &T {
    fn key(&self) -> PathBuf {
        (*self).key()
    }
}

impl CacheKey for [u8] {
    fn key(&self) -> PathBuf {
        let hash = rattler_digest::compute_bytes_digest::<rattler_digest::Sha256>(self);
        bytes_to_path_suffix(hash.as_slice())
    }
}

// Some filesystems don't cope well with a single directory containing lots of files. So
// we disperse our files over multiple nested directories. This is the nesting depth, so
// "3" means our paths will look like:
//   ${BASE}/${CHAR}/${CHAR}/${CHAR}/${ENTRY}
// And our fanout is 64, so this would split our files over 64**3 = 262144 directories.
const DIR_NEST_DEPTH: usize = 3;

fn bytes_to_path_suffix(bytes: &[u8]) -> PathBuf {
    let mut path = PathBuf::new();
    let enc = data_encoding::BASE64URL_NOPAD.encode(bytes);
    for i in 0..DIR_NEST_DEPTH {
        path.push(&enc[i..i + 1]);
    }
    path.push(&enc[DIR_NEST_DEPTH..]);
    path
}

impl CacheKey for ArtifactHashes {
    fn key(&self) -> PathBuf {
        let mut path = PathBuf::new();
        if let Some(sha256) = &self.sha256 {
            path.push("sha256");
            path.push(bytes_to_path_suffix(sha256.as_slice()))
        } else {
            unreachable!("should never have an artifact hash without any hashes")
        }
        path
    }
}

#[derive(Debug)]
/// A cache that stores its data as cbor files on the filesystem.
pub struct FileStore {
    base: PathBuf,
    tmp: PathBuf,
}

impl FileStore {
    /// Constructs a new instance of a [`FileStore`] rooted at the given `base`.
    pub fn new(base: &Path) -> io::Result<Self> {
        // Ensure the directory exists
        fs::create_dir_all(base)?;

        // Get the canonical path now that we are sure the directory exists
        let base = base.canonicalize()?;

        // We use a temporary folder inside the base folder to ensure that they are on the same
        // filesystem.
        let tmp = base.join(".tmp");
        fs::create_dir_all(&tmp)?;

        Ok(Self { base, tmp })
    }

    /// Gets readable access to the data with the specified key. If no such entry exists the
    /// function `f` is called to populate the entry.
    pub fn get_or_set<K: CacheKey, F>(&self, key: &K, f: F) -> io::Result<impl Read + Seek>
    where
        F: FnOnce(&mut dyn Write) -> io::Result<()>,
    {
        let lock = self.lock(key)?;
        if let Some(reader) = lock.reader() {
            // We use `detach_unlocked` here because we are sure that if the file exists it also has
            // immutable content.
            Ok(reader.detach_unlocked())
        } else {
            let mut writer = lock.begin()?;
            f(&mut writer)?;
            Ok(writer.commit()?.detach_unlocked())
        }
    }

    /// Gets readable access to the data with the specified key. Returns `None` if no such key
    /// exists in the store.
    pub fn get<K: CacheKey>(&self, key: &K) -> Option<impl Read + Seek> {
        if let Some(lock) = self.lock_if_exists(key) {
            if let Some(reader) = lock.reader() {
                return Some(reader.detach_unlocked());
            }
        }
        None
    }

    /// Locks a certain file in the cache for exclusive access.
    pub fn lock<K: CacheKey>(&self, key: &K) -> io::Result<FileLock> {
        let path = self.base.join(key.key());
        let lock = lock(&path, LockMode::Lock)?;
        Ok(FileLock {
            tmp: self.tmp.clone(),
            _lock_file: lock,
            path,
        })
    }

    /// Locks a certain file in the cache for exclusive access if it exists only.
    ///
    /// This function exists to ensure that we don't create tons of directories just to check if an
    /// entry exists or not.
    pub fn lock_if_exists<K: CacheKey>(&self, key: &K) -> Option<FileLock> {
        let path = self.base.join(key.key());
        lock(&path, LockMode::IfExists).ok().map(|lock| FileLock {
            tmp: self.tmp.clone(),
            _lock_file: lock,
            path,
        })
    }
}

/// A [`LockedWriter`] is created from a [`FileLock`]. It holds a lifetime to the lock to ensure it
/// has exclusive write access to the the file itself.
///
/// Internally the writer writes to a temporary file that is persisted to the final location to
/// ensure that the final path is never corrupted.
pub struct LockedWriter<'a> {
    path: &'a Path,
    f: tempfile::NamedTempFile,
}

impl<'a> Write for LockedWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.f.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.f.flush()
    }
}

impl<'a> Seek for LockedWriter<'a> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.f.seek(pos)
    }
}

impl<'a> LockedWriter<'a> {
    /// Commit the content currently written to this instance. Returns a [`LockedReader`] which can
    /// be used to read from the file again.
    pub fn commit(self) -> io::Result<LockedReader<'a>> {
        self.f.as_file().sync_data()?;
        let mut file = self.f.persist(self.path)?;
        file.rewind()?;
        Ok(LockedReader {
            file,
            _data: Default::default(),
        })
    }
}

/// A [`LockedReader`] is created from a [`FileLock`]. It holds a lifetime to the lock to ensure the
/// lock is not dropped before the file itself.
pub struct LockedReader<'a> {
    file: File,
    _data: PhantomData<&'a ()>,
}

impl<'a> Read for LockedReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl<'a> Seek for LockedReader<'a> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl<'a> LockedReader<'a> {
    /// Returns access to the underlying file ignoring the lock file.
    pub fn detach_unlocked(self) -> File {
        self.file
    }
}

/// Holds a lock to a file in the [`FileStore`], can be used get a readable or writeable instance.
pub struct FileLock {
    /// The path to a directory that will contain temporary files.
    tmp: PathBuf,

    /// The lock-file. As long as this is kept open this instance has exclusive access to the file.
    _lock_file: File,

    /// The path of the file that is actually locked.
    path: PathBuf,
}

impl FileLock {
    /// Creates a reader to read the contents of the locked file. Returns `None` if the file could
    /// not be opened.
    pub fn reader(&self) -> Option<LockedReader> {
        Some(LockedReader {
            file: File::open(&self.path).ok()?,
            _data: Default::default(),
        })
    }

    /// Starts writing the contents of the file returning a writer. Call [`LockedWriter::commit`] to
    /// persist the data in the store.
    pub fn begin(&self) -> io::Result<LockedWriter> {
        Ok(LockedWriter {
            path: &self.path,
            f: tempfile::NamedTempFile::new_in(&self.tmp)?,
        })
    }

    /// Removes the file from the store.
    pub fn remove(self) -> io::Result<()> {
        fs::remove_file(self.path)?;
        Ok(())
    }
}

#[derive(Eq, PartialEq)]
enum LockMode {
    Lock,
    IfExists,
}

/// Create a `.lock` file for the file at the specified `path`. Only a single process has access to
/// the lock-file.
fn lock(path: &Path, mode: LockMode) -> io::Result<File> {
    // Determine the path of the lockfile
    let lock_path = path.with_extension(".lock");

    // On windows the file must be open as write to ensure it cannot be opened by another process.
    let mut open_options = fs::OpenOptions::new();
    open_options.write(true);

    // Only create the parent directories if the lock mode is set to `Lock`. In the other case we
    // don't care if the file doesn't exist.
    if mode == LockMode::Lock {
        let dir = lock_path
            .parent()
            .expect("expected the file to be rooted in some folder");
        std::fs::create_dir_all(dir)?;
        open_options.create(true);
    }

    // Open the lock file
    let lock = open_options.open(&lock_path)?;

    // Lock the file. On unix this is apparently a thin wrapper around flock(2) and it doesn't
    // properly handle EINTR so we keep retrying when that happens.

    retry_interrupted(|| lock.lock_exclusive())?;

    Ok(lock)
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[test]
    fn test_file_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path()).unwrap();

        let hello = b"Hello, world!".as_slice();

        let mut read_back = Vec::new();
        store
            .get_or_set(&hello, |w| w.write_all(hello))
            .unwrap()
            .read_to_end(&mut read_back)
            .unwrap();
        assert_eq!(read_back, hello);
    }

    #[tokio::test]
    async fn test_locking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let path2 = dir.path().to_path_buf();

        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();
        let notify3 = notify.clone();

        let one = tokio::spawn(async move {
            let lock = lock(&path, LockMode::Lock).unwrap();
            notify2.notify_one();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let two = tokio::spawn(async move {
            notify3.notified().await;
            tokio::task::spawn_blocking(move || lock(&path2, LockMode::IfExists))
                .await
                .unwrap()
                .unwrap();
        });

        let (a, b) = tokio::join!(one, two);
        a.unwrap();
        b.unwrap();
    }
}
