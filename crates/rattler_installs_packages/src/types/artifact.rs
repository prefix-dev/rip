use super::artifact_name::InnerAsArtifactName;
use crate::resolve::PypiVersion;
use crate::types::SourceArtifactName;
use crate::utils::ReadAndSeek;
use std::path::Path;

/// Trait to implement if it is a type that has an [`super::artifact_name::ArtifactName`]
/// this is then used by the [`crate::index::PackageDb`] to make a difference
/// between the different types of artifacts.
pub trait HasArtifactName {
    /// The name of the artifact which describes the artifact.
    ///
    /// Artifacts are describes by a string. [`super::artifact_name::ArtifactName`] describes the
    /// general format.
    type Name: Clone + InnerAsArtifactName;

    /// Returns the name of this instance
    fn name(&self) -> &Self::Name;
}

/// Trait that represents an artifact type in the PyPI ecosystem.
/// That is a single file like a wheel, sdist.
pub trait ArtifactFromBytes: HasArtifactName + Sized {
    /// Construct a new artifact from the given bytes
    fn from_bytes(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self>;
}

/// Error while reading pyproject.toml
#[derive(thiserror::Error, Debug)]
#[allow(missing_docs)]
pub enum ReadPyProjectError {
    #[error("IO error while reading pyproject.toml: {0}")]
    Io(#[from] std::io::Error),

    #[error("No pyproject.toml found in archive")]
    NoPyProjectTomlFound,

    #[error("Could not parse pyproject.toml")]
    PyProjectTomlParseError(String),
}

/// SDist or STree act as a SourceArtifact
/// so we can use it in methods where we expect sdist
/// to extract metadata
pub trait ArtifactFromSource: HasArtifactName + Sync {
    /// get bytes of an artifact
    /// that will we be used for hashing
    fn try_get_bytes(&self) -> Result<Vec<u8>, std::io::Error>;

    /// Distribution Name
    fn distribution_name(&self) -> String;

    /// Version ( URL or Version )
    fn version(&self) -> PypiVersion;

    /// Source artifact name
    fn artifact_name(&self) -> SourceArtifactName;

    /// Read the build system info from the pyproject.toml
    fn read_build_info(&self) -> Result<pyproject_toml::BuildSystem, ReadPyProjectError>;

    /// extract to a specific location
    /// for sdist we unpack it
    /// for stree we move it
    /// as example this method is used by install_build_files
    fn extract_to(&self, work_dir: &Path) -> std::io::Result<()>;
}
