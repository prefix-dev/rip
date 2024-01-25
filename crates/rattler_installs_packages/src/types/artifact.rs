use super::artifact_name::InnerAsArtifactName;
use crate::utils::ReadAndSeek;

/// Trait that represents an artifact type in the PyPI ecosystem.
pub trait Artifact: Sized {
    /// The name of the artifact which describes the artifact.
    ///
    /// Artifacts are describes by a string. [`super::artifact_name::ArtifactName`] describes the
    /// general format.
    type Name: Clone + InnerAsArtifactName;

    /// Construct a new artifact from the given bytes
    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self>;

    /// Returns the name of this instance
    fn name(&self) -> &Self::Name;
}
