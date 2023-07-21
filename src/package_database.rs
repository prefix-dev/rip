use crate::artifact::MetadataArtifact;
use crate::{
    artifact::Artifact,
    artifact_name::InnerAsArtifactName,
    http::{CacheMode, Http},
    package_name::PackageName,
    project_info::{ArtifactInfo, ProjectInfo},
    FileStore,
};
use elsa::FrozenMap;
use futures::{pin_mut, stream, AsyncReadExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method};
use indexmap::IndexMap;
use miette::{self, IntoDiagnostic};
use pep440::Version;
use reqwest::{
    header::{ACCEPT, CACHE_CONTROL},
    Client, StatusCode,
};
use std::borrow::Borrow;
use std::fmt::Display;
use std::io::Read;
use std::path::PathBuf;
use url::Url;

pub struct PackageDb {
    http: Http,

    /// Index URLS to query
    index_urls: Vec<Url>,

    /// A file store that stores metadata by hashes
    metadata_cache: FileStore,

    /// A cache of package name to version to artifacts.
    artifacts: FrozenMap<PackageName, Box<IndexMap<Version, Vec<ArtifactInfo>>>>,
}

impl PackageDb {
    /// Constructs a new [`PackageDb`] that reads information from the specified URLs.
    pub fn new(client: Client, index_urls: &[Url], cache_dir: PathBuf) -> std::io::Result<Self> {
        Ok(Self {
            http: Http::new(
                client,
                FileStore::new(&cache_dir.join("http"))?,
                FileStore::new(&cache_dir.join("by-hash"))?,
            ),
            index_urls: index_urls.into(),
            metadata_cache: FileStore::new(&cache_dir.join("metadata"))?,
            artifacts: Default::default(),
        })
    }

    /// Downloads and caches information about available artifiacts of a package from the index.
    pub async fn available_artifacts(
        &self,
        p: &PackageName,
    ) -> miette::Result<&IndexMap<Version, Vec<ArtifactInfo>>> {
        if let Some(cached) = self.artifacts.get(p) {
            Ok(cached)
        } else {
            // Start downloading the information for each url.
            let http = self.http.clone();
            let request_iter = stream::iter(self.index_urls.iter())
                .map(|url| url.join(&format!("{}/", p.as_str())).expect("invalid url"))
                .map(|url| fetch_simple_api(&http, url))
                .buffer_unordered(10)
                .filter_map(|result| async { result.map_or_else(|e| Some(Err(e)), |v| v.map(Ok)) });

            pin_mut!(request_iter);

            // Add all the incoming results to the set of results
            let mut result: IndexMap<Version, Vec<ArtifactInfo>> = Default::default();
            while let Some(response) = request_iter.next().await {
                for artifact in response?.files {
                    result
                        .entry(artifact.filename.version().clone())
                        .or_default()
                        .push(artifact);
                }
            }

            // Sort the artifact infos by name, this is just to have a consistent order and make
            // the resolution output consistent.
            for artifact_infos in result.values_mut() {
                artifact_infos.sort_by(|a, b| a.filename.cmp(&b.filename));
            }

            // Sort in descending order by version
            result.sort_unstable_by(|v1, _, v2, _| v2.cmp(v1));

            Ok(self.artifacts.insert(p.clone(), Box::new(result)))
        }
    }

    /// Reads the metadata for the given artifact from the cache or return `None` if the metadata
    /// could not be found in the cache.
    fn metadata_from_cache(&self, ai: &ArtifactInfo) -> Option<Vec<u8>> {
        let mut data = self.metadata_cache.get(&ai.hashes.as_ref()?)?;
        let mut bytes = Vec::new();
        data.read_to_end(&mut bytes).ok()?;
        Some(bytes)
    }

    /// Writes the metadata for the given artifact into the cache. If the metadata already exists
    /// its not overwritten.
    fn put_metadata_in_cache(&self, ai: &ArtifactInfo, blob: &[u8]) -> miette::Result<()> {
        if let Some(hash) = &ai.hashes {
            self.metadata_cache
                .get_or_set(&hash, |w| Ok(w.write_all(blob)?))
                .into_diagnostic()?;
        }
        Ok(())
    }

    /// Returns the metadata from a set of artifacts. This function assumes that metadata is
    /// consistent for all artifacts of a single version.
    pub async fn get_metadata<'a, A: MetadataArtifact, I: Borrow<ArtifactInfo>>(
        &self,
        artifacts: &'a [I],
    ) -> miette::Result<(&'a ArtifactInfo, A::Metadata)> {
        // Find all the artifacts that match the artifact we are looking for
        let mut matching_artifacts = artifacts
            .iter()
            .map(|artifact_info| artifact_info.borrow())
            .filter(|artifact_info| artifact_info.is::<A>());

        // Check if we already have information about any of the artifacts cached.
        for artifact_info in artifacts.iter().map(|b| b.borrow()) {
            if let Some(metadata_bytes) = self.metadata_from_cache(artifact_info) {
                return Ok((artifact_info, A::parse_metadata(&metadata_bytes)?));
            }
        }

        // Check if we already have one of the artifacts cached.
        for artifact_info in matching_artifacts.clone() {
            let result = self
                .get_artifact_with_cache::<A>(artifact_info, CacheMode::OnlyIfCached)
                .await;
            match result {
                Ok(artifact) => {
                    // Apparently the artifact has been downloaded, but its metadata has not been
                    // cached yet. Lets store it there.
                    let (blob, metadata) = artifact.metadata().await?;
                    self.put_metadata_in_cache(artifact_info, &blob)?;
                    return Ok((artifact_info, metadata));
                }
                Err(err) => match err.downcast_ref::<NotCached>() {
                    Some(_) => continue,
                    None => return Err(err),
                },
            }
        }

        // We have exhausted all options to read the metadata from the cache. We'll have to hit the
        // network to get to the information.

        // TODO: PEP 658 support

        // Get the information from the first artifact. We assume the metadata is consistent across
        // all matching artifacts
        if let Some(artifact_info) = matching_artifacts.next() {
            let body = self
                .http
                .request(
                    artifact_info.url.clone(),
                    Method::GET,
                    HeaderMap::default(),
                    CacheMode::Default,
                )
                .await?
                .body()
                .force_seek()
                .await?;
            let artifact = A::new(
                artifact_info
                    .filename
                    .as_inner::<A::Name>()
                    .expect("this should never happen because we filter on matching artifacts only")
                    .clone(),
                body,
            )
            .await?;
            let (blob, metadata) = artifact.metadata().await?;
            self.put_metadata_in_cache(artifact_info, &blob)?;
            return Ok((artifact_info, metadata));
        }

        miette::bail!(
            "couldn't find any {} metadata for {:#?}",
            std::any::type_name::<A>(),
            artifacts
                .iter()
                .map(|artifact_info| artifact_info.borrow())
                .collect::<Vec<_>>()
        );
    }

    /// Opens the specified artifact info. Depending on the specified `cache_mode`, downloads the
    /// artifact data from the remote location if the information is not already cached.
    async fn get_artifact_with_cache<A: Artifact>(
        &self,
        artifact_info: &ArtifactInfo,
        cache_mode: CacheMode,
    ) -> miette::Result<A> {
        // Check if the artifact is the same type as the info.
        let name = A::Name::try_as(&artifact_info.filename)
            .expect("the specified artifact does not refer to type requested to read");

        // Get the contents of the artifact
        let artifact_bytes = self
            .http
            .request(
                artifact_info.url.clone(),
                Method::GET,
                HeaderMap::default(),
                cache_mode,
            )
            .await?;

        // Turn the response into a seekable response.
        let bytes = artifact_bytes.into_body().force_seek().await?;
        A::new(name.clone(), bytes).await
    }

    /// Opens the specified artifact info. Downloads the artifact data from the remote location if
    /// the information is not already cached.
    pub async fn get_artifact<A: Artifact>(
        &self,
        artifact_info: &ArtifactInfo,
    ) -> miette::Result<A> {
        self.get_artifact_with_cache(artifact_info, CacheMode::Default)
            .await
    }
}

async fn fetch_simple_api(http: &Http, url: Url) -> miette::Result<Option<ProjectInfo>> {
    let mut headers = HeaderMap::new();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=0"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.pypi.simple.v1+json"),
    );

    let response = http
        .request(url, Method::GET, headers, CacheMode::Default)
        .await?;

    // If the resource could not be found we simply return.
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    // Convert the information from json
    let mut bytes = Vec::new();
    response
        .into_body()
        .read_to_end(&mut bytes)
        .await
        .into_diagnostic()?;

    // Deserialize the json
    serde_json::from_slice(&bytes).map(Some).into_diagnostic()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::artifact::{MetadataArtifact, Wheel};
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_available_packages() {
        let cache_dir = TempDir::new().unwrap();
        let package_db = PackageDb::new(
            Client::new(),
            &[Url::parse("https://pypi.org/simple/").unwrap()],
            cache_dir.path().into(),
        )
        .unwrap();

        // Get all the artifacts
        let artifacts = package_db
            .available_artifacts(&"flask".parse().unwrap())
            .await
            .unwrap();

        // Get the first wheel artifact
        let artifact_info = artifacts
            .iter()
            .flat_map(|(_, artifacts)| artifacts.iter())
            .filter(|artifact| artifact.filename.as_wheel().is_some())
            .next()
            .unwrap();

        let artifact = package_db
            .get_artifact::<Wheel>(artifact_info)
            .await
            .unwrap();
        let (_, metadata) = artifact.metadata().await.unwrap();

        dbg!(metadata);
    }
}

#[derive(Debug)]
pub struct NotCached;

impl Display for NotCached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request not in cache, and cache_mode=OnlyIfCached")
    }
}

impl std::error::Error for NotCached {}
