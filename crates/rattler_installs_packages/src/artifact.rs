use crate::{artifact_name::InnerAsArtifactName, utils::ReadAndSeek, PackageDb};
use async_http_range_reader::AsyncHttpRangeReader;
use async_trait::async_trait;

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

/// Trait that represents an artifact that contains metadata.
#[async_trait]
pub trait MetadataArtifact: Artifact + Send {
    /// Associated type for the metadata of this artifact.
    type Metadata;

    /// Parses the metadata associated with an artifact.
    fn parse_metadata(bytes: &[u8]) -> miette::Result<Self::Metadata>;

    /// Parses the metadata from the artifact itself. Also returns the metadata bytes so we can
    /// cache it for later.
    async fn metadata(&self, package_db: &PackageDb) -> miette::Result<(Vec<u8>, Self::Metadata)>;

    /// Try to sparsely read the metadata
    async fn read_metadata_bytes(
        _name: &<Self as Artifact>::Name,
        _stream: &mut AsyncHttpRangeReader,
    ) -> miette::Result<(Vec<u8>, Self::Metadata)> {
        unimplemented!()
    }

    /// Returns true if the [`Self::read_metadata_bytes`] is supported.
    fn supports_sparse_metadata() -> bool {
        false
    }
}
