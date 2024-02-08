use crate::index::git_interop::{git_clone, GitSource, ParsedUrl};
use crate::index::package_database::DirectUrlArtifactResponse;
use crate::resolve::PypiVersion;
use crate::types::{
    ArtifactHashes, ArtifactInfo, ArtifactName, ArtifactType, DirectUrlJson, DirectUrlSource,
    DirectUrlVcs, DistInfoMetadata, HasArtifactName, NormalizedPackageName, Yanked,
};
use crate::wheel_builder::WheelBuilder;
use indexmap::IndexMap;
use miette::IntoDiagnostic;
use rattler_digest::{compute_bytes_digest, Sha256};
use std::sync::Arc;
use url::Url;

/// Get artifact by git reference
pub(crate) async fn get_artifacts_and_metadata<P: Into<NormalizedPackageName>>(
    p: P,
    url: Url,
    wheel_builder: &WheelBuilder,
) -> miette::Result<DirectUrlArtifactResponse> {
    let normalized_package_name = p.into();

    let parsed_url = ParsedUrl::new(&url)?;

    let git_source = GitSource {
        url: parsed_url.git_url,
        rev: parsed_url.revision,
    };

    let (mut location, git_rev) = git_clone(&git_source).into_diagnostic()?;

    if let Some(subdirectory) = parsed_url.subdirectory {
        location.push(&subdirectory);
        if !location.exists() {
            return Err(miette::miette!(
                "Requested subdirectory fragment {:?} can't be located at following url {:?}",
                subdirectory,
                url
            ));
        }
    };

    let (wheel_metadata, artifact) = super::file::get_stree_from_file_path(
        &normalized_package_name,
        url.clone(),
        Some(location),
        wheel_builder,
    )
    .await?;

    let requires_python = wheel_metadata.1.requires_python.clone();

    let dist_info_metadata = DistInfoMetadata {
        available: false,
        hashes: ArtifactHashes::default(),
    };

    let yanked = Yanked {
        yanked: false,
        reason: None,
    };

    let direct_url_json = DirectUrlJson {
        url: url.clone(),
        source: DirectUrlSource::Vcs {
            vcs: DirectUrlVcs::Git,
            requested_revision: git_source.rev,
            commit_id: git_rev.get_commit(),
        },
    };

    let project_hash = ArtifactHashes {
        sha256: Some(compute_bytes_digest::<Sha256>(url.as_str().as_bytes())),
    };

    let artifact_info = Arc::new(ArtifactInfo {
        filename: ArtifactName::STree(artifact.name().clone()),
        url: url.clone(),
        is_direct_url: true,
        hashes: Some(project_hash),
        requires_python,
        dist_info_metadata,
        yanked,
    });

    let mut result = IndexMap::default();
    result.insert(PypiVersion::Url(url.clone()), vec![artifact_info.clone()]);

    Ok(DirectUrlArtifactResponse {
        artifact_info,
        metadata: (wheel_metadata.0, wheel_metadata.1),
        artifact_versions: result,
        artifact: ArtifactType::STree(artifact),
        direct_url_json,
    })
}
