use super::artifact_name::InnerAsArtifactName;
use crate::utils::ReadAndSeek;

/// Trait to implement if it is a type that has an [`super::artifact_name::ArtifactName`]
/// this is then used by the [`crate::index::package_database::PackageDatabase`] to make a difference
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
pub trait Artifact: HasArtifactName + Sized {
    /// Construct a new artifact from the given bytes
    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self>;
}
