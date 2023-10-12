//! A module that defines the [`Filesystem`] trait and an implementation that works with the
//! physical filesystem [`RootedFilesystem]`.

use std::io::Read;
use std::path::{Path, PathBuf};

/// An abstraction over a filesystem.
pub trait Filesystem {
    /// Constructs a directory.
    fn mkdir(&mut self, path: &Path) -> std::io::Result<()>;

    /// Writes the content of a file to the filesystem.
    fn write_file(
        &mut self,
        path: &Path,
        data: &mut dyn Read,
        executable: bool,
    ) -> std::io::Result<()>;

    /// Creates a new symbolic link on the filesystem.
    ///
    /// The link path will be a symbolic link pointing to the original path.
    fn write_symlink(&mut self, original: &Path, link: &Path) -> std::io::Result<()>;
}

impl<FS: Filesystem> Filesystem for &mut FS {
    fn mkdir(&mut self, path: &Path) -> std::io::Result<()> {
        (*self).mkdir(path)
    }

    fn write_file(
        &mut self,
        path: &Path,
        data: &mut dyn Read,
        executable: bool,
    ) -> std::io::Result<()> {
        (*self).write_file(path, data, executable)
    }

    fn write_symlink(&mut self, source: &Path, target: &Path) -> std::io::Result<()> {
        (*self).write_symlink(source, target)
    }
}

/// A [`FileSystem`] implementation that writes its contents to a directory on the systems
/// filesystem.
pub struct RootedFilesystem {
    root: PathBuf,
}

impl<P: Into<PathBuf>> From<P> for RootedFilesystem {
    fn from(root: P) -> Self {
        Self { root: root.into() }
    }
}

impl Filesystem for RootedFilesystem {
    fn mkdir(&mut self, path: &Path) -> std::io::Result<()> {
        debug_assert!(path.is_relative());
        std::fs::create_dir_all(self.root.join(path))
    }

    fn write_file(
        &mut self,
        path: &Path,
        data: &mut dyn Read,
        _executable: bool,
    ) -> std::io::Result<()> {
        debug_assert!(path.is_relative());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true);
        #[cfg(unix)]
        if _executable {
            options.mode(0o777);
        } else {
            options.mode(0o666);
        }
        let full_path = self.root.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = options.open(full_path)?;
        std::io::copy(data, &mut file)?;
        Ok(())
    }

    fn write_symlink(&mut self, original: &Path, link: &Path) -> std::io::Result<()> {
        debug_assert!(link.is_relative());
        debug_assert!(original.is_relative());
        let full_path = self.root.join(link);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(original, &full_path)
        }
        #[cfg(not(unix))]
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks not supported on this platform",
        ))
    }
}
