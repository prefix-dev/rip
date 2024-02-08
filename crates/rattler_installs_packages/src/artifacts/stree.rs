use crate::resolve::PypiVersion;
use crate::types::ArtifactFromSource;
use crate::types::DirectUrlJson;
use crate::types::ReadPyProjectError;
use crate::types::{HasArtifactName, STreeFilename, SourceArtifactName};
use fs_err as fs;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Represents a source tree which can be a simple directory on filesystem
/// or something cloned from git
pub struct STree {
    /// Name of the source tree
    pub name: STreeFilename,

    /// Source tree location
    pub location: parking_lot::Mutex<PathBuf>,
}

impl STree {
    /// Get a lock on the inner data
    pub fn lock_data(&self) -> parking_lot::MutexGuard<PathBuf> {
        self.location.lock()
    }

    /// Copy source tree directory in specific location
    fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
        fs::create_dir_all(&dst)?;
        for entry in fs::read_dir(src.as_ref())? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                Self::copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
            } else {
                fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
            }
        }
        Ok(())
    }
}

impl HasArtifactName for STree {
    type Name = STreeFilename;

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

impl ArtifactFromSource for STree {
    fn try_get_bytes(&self) -> Result<Vec<u8>, std::io::Error> {
        let vec = vec![];
        let inner = self.lock_data();
        let mut dir_entry = fs::read_dir(inner.as_path())?;

        let next_entry = dir_entry.next();
        if let Some(Ok(root_folder)) = next_entry {
            let modified = root_folder.metadata()?.modified()?;
            let mut hasher = DefaultHasher::new();
            modified.hash(&mut hasher);
            let hash = hasher.finish().to_be_bytes().as_slice().to_owned();
            return Ok(hash);
        }

        Ok(vec)
    }

    fn distribution_name(&self) -> String {
        self.name.distribution.as_source_str().to_owned()
    }

    fn version(&self) -> PypiVersion {
        PypiVersion::Url(self.name.url.clone())
    }

    fn artifact_name(&self) -> SourceArtifactName {
        SourceArtifactName::STree(self.name.clone())
    }

    fn read_pyproject_toml(&self) -> Result<pyproject_toml::PyProjectToml, ReadPyProjectError> {
        let location = self.lock_data().join("pyproject.toml");

        if let Ok(bytes) = fs::read(location) {
            let source = String::from_utf8(bytes).map_err(|e| {
                ReadPyProjectError::PyProjectTomlParseError(format!(
                    "could not parse pyproject.toml (bad encoding): {}",
                    e
                ))
            })?;
            let project = pyproject_toml::PyProjectToml::new(&source).map_err(|e| {
                ReadPyProjectError::PyProjectTomlParseError(format!(
                    "could not parse pyproject.toml (bad toml): {}",
                    e
                ))
            })?;
            Ok(project)
        } else {
            Err(ReadPyProjectError::NoPyProjectTomlFound)
        }
    }
    /// move all files to a specific directory
    fn extract_to(&self, work_dir: &Path) -> std::io::Result<()> {
        let src = self.lock_data();
        Self::copy_dir_all(src.as_path(), work_dir)
    }
}
