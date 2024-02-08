use crate::artifacts::{SDist, STree, Wheel};
use crate::index::package_database::DirectUrlArtifactResponse;
use crate::resolve::PypiVersion;
use crate::types::{
    ArtifactFromBytes, ArtifactHashes, ArtifactInfo, ArtifactType, DistInfoMetadata,
    NormalizedPackageName, PackageName, SDistFilename, SDistFormat, STreeFilename,
    WheelCoreMetadata, Yanked,
};
use crate::wheel_builder::{WheelBuildError, WheelBuilder};
use indexmap::IndexMap;
use miette::IntoDiagnostic;
use parking_lot::Mutex;
use pep440_rs::Version;
use rattler_digest::Sha256;
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

/// Return an sdist from file path
pub(crate) async fn get_sdist_from_file_path(
    normalized_package_name: &NormalizedPackageName,
    path: &PathBuf,
    wheel_builder: &WheelBuilder,
) -> miette::Result<((Vec<u8>, WheelCoreMetadata), SDist)> {
    let distribution = PackageName::from(normalized_package_name.clone());

    let path_str = if let Some(path_str) = path.as_os_str().to_str() {
        path_str
    } else {
        return Err(WheelBuildError::Error(format!(
            "Could not convert path in utf-8 str {}",
            path.to_string_lossy()
        )))
        .into_diagnostic();
    };
    let format = SDistFormat::get_extension(path_str).into_diagnostic()?;
    let dummy_version =
        Version::from_str("0.0.0").expect("0.0.0 version should always be parseable");

    let dummy_sdist_file_name = SDistFilename {
        distribution,
        version: dummy_version,
        format,
    };

    let file = File::open(path).into_diagnostic()?;

    let mut sdist = SDist::from_bytes(dummy_sdist_file_name, Box::new(file))?;

    let wheel_metadata = wheel_builder
        .get_sdist_metadata(&sdist)
        .await
        .into_diagnostic()?;

    // construct a real sdist filename
    let sdist_filename = SDistFilename {
        distribution: wheel_metadata.1.name.clone(),
        version: wheel_metadata.1.version.clone(),
        format,
    };

    sdist.name = sdist_filename;

    Ok((wheel_metadata, sdist))
}

/// Return an stree from file path
pub(crate) async fn get_stree_from_file_path(
    normalized_package_name: &NormalizedPackageName,
    url: Url,
    path: Option<PathBuf>,
    wheel_builder: &WheelBuilder,
) -> miette::Result<((Vec<u8>, WheelCoreMetadata), STree)> {
    let distribution = PackageName::from(normalized_package_name.clone());
    let path = match path {
        None => PathBuf::from_str(url.path()).into_diagnostic()?,
        Some(path) => path,
    };

    let dummy_version =
        Version::from_str("0.0.0").expect("0.0.0 version should always be parseable");

    let stree_file_name = STreeFilename {
        distribution,
        version: dummy_version,
        url: url.clone(),
    };

    let mut stree = STree {
        name: stree_file_name,
        location: Mutex::new(path),
    };

    let wheel_metadata = wheel_builder
        .get_sdist_metadata(&stree)
        .await
        .into_diagnostic()?;

    let stree_file_name = STreeFilename {
        distribution: wheel_metadata.1.name.clone(),
        version: wheel_metadata.1.version.clone(),
        url: url.clone(),
    };

    stree.name = stree_file_name;

    Ok((wheel_metadata, stree))
}

/// Get artifact by file URL
pub(crate) async fn get_artifacts_and_metadata<P: Into<NormalizedPackageName>>(
    p: P,
    url: Url,
    wheel_builder: &WheelBuilder,
) -> miette::Result<DirectUrlArtifactResponse> {
    let path = if let Ok(path) = url.to_file_path() {
        path
    } else {
        return Err(WheelBuildError::Error(format!(
            "Could not build wheel from path {}",
            url
        )))
        .into_diagnostic();
    };
    let str_name = url.path();

    let normalized_package_name = p.into();

    let (metadata_bytes, metadata, artifact) = if path.is_file() && str_name.ends_with(".whl") {
        let wheel = Wheel::from_path(&path, &normalized_package_name)
            .map_err(|e| WheelBuildError::Error(format!("Could not build wheel: {}", e)))
            .into_diagnostic()?;

        let (data_bytes, metadata) = wheel.metadata()?;
        (data_bytes, metadata, ArtifactType::Wheel(wheel))
    } else if path.is_file() {
        let (wheel_metadata, sdist) =
            get_sdist_from_file_path(&normalized_package_name, &path, wheel_builder).await?;
        (
            wheel_metadata.0,
            wheel_metadata.1,
            ArtifactType::SDist(sdist),
        )
    } else {
        let (wheel_metadata, stree) = get_stree_from_file_path(
            &normalized_package_name,
            url.clone(),
            Some(path),
            wheel_builder,
        )
        .await?;
        (
            wheel_metadata.0,
            wheel_metadata.1,
            ArtifactType::STree(stree),
        )
    };

    let artifact_hash = {
        ArtifactHashes {
            sha256: Some(rattler_digest::compute_bytes_digest::<Sha256>(
                metadata_bytes.clone(),
            )),
        }
    };

    let artifact_info = Arc::new(ArtifactInfo {
        filename: artifact.name(),
        url: url.clone(),
        is_direct_url: true,
        hashes: Some(artifact_hash),
        requires_python: metadata.requires_python.clone(),
        dist_info_metadata: DistInfoMetadata::default(),
        yanked: Yanked::default(),
    });

    let mut result = IndexMap::default();
    result.insert(PypiVersion::Url(url.clone()), vec![artifact_info.clone()]);

    Ok(DirectUrlArtifactResponse {
        artifact_info,
        metadata: (metadata_bytes, metadata),
        artifact_versions: result,
        artifact,
    })
}
