use std::sync::Arc;

use crate::index::http::Http;
use crate::index::package_database::DirectUrlArtifactResponse;
use crate::types::NormalizedPackageName;
use crate::wheel_builder::WheelBuilder;
use url::Url;

pub(crate) mod file;
pub(crate) mod git;
pub(crate) mod http;

/// Get artifact directly from file, vcs, or url
pub(crate) async fn fetch_artifact_and_metadata_by_direct_url<P: Into<NormalizedPackageName>>(
    http: &Http,
    p: P,
    url: Url,
    wheel_builder: Arc<WheelBuilder>,
) -> miette::Result<DirectUrlArtifactResponse> {
    let p = p.into();

    let response = if url.scheme() == "file" {
        // This can result in a Wheel, Sdist or STree
        super::direct_url::file::get_artifacts_and_metadata(p.clone(), url, wheel_builder).await
    } else if url.scheme() == "https" {
        // This can be a Wheel or SDist artifact
        super::direct_url::http::get_artifacts_and_metadata(http, p.clone(), url, wheel_builder)
            .await
    } else if url.scheme() == "git+https" || url.scheme() == "git+file" {
        // This can be a STree artifact
        super::direct_url::git::get_artifacts_and_metadata(p.clone(), url, wheel_builder).await
    } else {
        Err(miette::miette!(
            "Usage of insecure protocol or unsupported scheme {:?}",
            url.scheme()
        ))
    }?;

    Ok(response)
}
