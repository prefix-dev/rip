use crate::artifacts::{SDist, Wheel};
use crate::index::http::Http;
use crate::index::{parse_hash, CacheMode};
use crate::resolve::PypiVersion;
use crate::types::{
    Artifact, ArtifactHashes, ArtifactInfo, ArtifactName, DistInfoMetadata, NormalizedPackageName,
    PackageName, SDistFilename, SDistFormat, WheelCoreMetadata, Yanked,
};
use crate::utils::ReadAndSeek;
use crate::wheel_builder::WheelBuilder;
use http::{HeaderMap, Method};
use indexmap::IndexMap;
use miette::IntoDiagnostic;
use pep440_rs::Version;
use rattler_digest::Sha256;
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

/// Get artifact by file URL
pub(crate) async fn get_artifacts_and_metadata<P: Into<NormalizedPackageName>>(
    http: &Http,
    p: P,
    url: Url,
    wheel_builder: &WheelBuilder,
) -> miette::Result<crate::index::package_database::DirectUrlArtifactResponse> {
    let str_name = url.path();
    let url_hash = url.fragment().and_then(parse_hash);

    let normalized_package_name = p.into();

    // Get the contents of the artifact
    let artifact_bytes = http
        .request(
            url.clone(),
            Method::GET,
            HeaderMap::default(),
            CacheMode::Default,
        )
        .await?;

    let mut bytes = artifact_bytes
        .into_body()
        .into_local()
        .await
        .into_diagnostic()?;

    let artifact_hash = {
        let mut bytes_for_hash = vec![];
        bytes.rewind().into_diagnostic()?;
        bytes.read_to_end(&mut bytes_for_hash).into_diagnostic()?;
        bytes.rewind().into_diagnostic()?;
        ArtifactHashes {
            sha256: Some(rattler_digest::compute_bytes_digest::<Sha256>(
                bytes_for_hash,
            )),
        }
    };

    if let Some(hash) = url_hash.clone() {
        assert_eq!(hash, artifact_hash);
    };

    let (filename, metadata_bytes, metadata) = if str_name.ends_with(".whl") {
        let wheel = Wheel::from_url_and_bytes(url.path(), &normalized_package_name, bytes)?;

        let filename = ArtifactName::Wheel(wheel.name().clone());
        let (data_bytes, metadata) = wheel.metadata()?;

        (filename, data_bytes, metadata)
    } else {
        let (wheel_metadata, filename) =
            get_sdist_from_bytes(&normalized_package_name, url.clone(), bytes, wheel_builder)
                .await?;

        (filename, wheel_metadata.0, wheel_metadata.1)
    };

    let artifact_info = Arc::new(ArtifactInfo {
        filename,
        url: url.clone(),
        hashes: Some(artifact_hash),
        requires_python: metadata.requires_python,
        dist_info_metadata: DistInfoMetadata::default(),
        yanked: Yanked::default(),
    });

    let mut result = IndexMap::default();
    result.insert(PypiVersion::Url(url.clone()), vec![artifact_info.clone()]);

    Ok(crate::index::package_database::DirectUrlArtifactResponse {
        artifact_info,
        metadata_bytes,
        artifact_versions: result,
    })
}

/// Return an sdist from http
async fn get_sdist_from_bytes(
    normalized_package_name: &NormalizedPackageName,
    url: Url,
    bytes: Box<dyn ReadAndSeek + Send>,
    wheel_builder: &WheelBuilder,
) -> miette::Result<((Vec<u8>, WheelCoreMetadata), ArtifactName)> {
    // it's probably an sdist
    let distribution = PackageName::from(normalized_package_name.clone());
    let version = Version::from_str("0.0.0").expect("0.0.0 version should always be parseable");
    let format = SDistFormat::get_extension(url.path()).into_diagnostic()?;

    let dummy_sdist_file_name = SDistFilename {
        distribution,
        version,
        format,
    };

    // when we receive a direct file or http url
    // we don't know the version for artifact until we extract the actual metadata
    // so we create a plain sdist object aka dummy
    // and populate it with correct metadata after calling `get_sdist_metadata`
    let dummy_sdist = SDist::new(dummy_sdist_file_name, Box::new(bytes))?;

    let wheel_metadata = wheel_builder
        .get_sdist_metadata(&dummy_sdist)
        .await
        .into_diagnostic()?;

    // construct a real sdist filename
    let sdist_filename = SDistFilename {
        distribution: wheel_metadata.1.name.clone(),
        version: wheel_metadata.1.version.clone(),
        format,
    };
    let filename = ArtifactName::SDist(sdist_filename.clone());

    Ok((wheel_metadata, filename))
}
