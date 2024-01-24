use super::artifact_name::InnerAsArtifactName;
use crate::utils::ReadAndSeek;

/// NamedArtifact
// pub trait NamedArtifact {
//     /// The name of the artifact which describes the artifact.
//     ///
//     /// Artifacts are describes by a string. [`super::artifact_name::ArtifactName`] describes the
//     /// general format.
//     type Name: Clone + InnerAsArtifactName;

//     /// Returns the name of this instance
//     fn name(&self) -> &Self::Name;
// }

/// Trait that represents an artifact type in the PyPI ecosystem.
pub trait Artifact: Sized {
    /// The name of the artifact which describes the artifact.
    ///
    /// Artifacts are describes by a string. [`super::artifact_name::ArtifactName`] describes the
    /// general format.
    type Name: Clone + InnerAsArtifactName;

    /// Returns the name of this instance
    fn name(&self) -> &Self::Name;
    /// Construct a new artifact from the given bytes
    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self>;
}
