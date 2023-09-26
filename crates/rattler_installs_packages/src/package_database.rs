use crate::artifact::MetadataArtifact;
use crate::html::parse_project_info_html;
use crate::http::HttpRequestError;
use crate::{
    artifact::Artifact,
    artifact_name::InnerAsArtifactName,
    html,
    http::{CacheMode, Http},
    project_info::{ArtifactInfo, ProjectInfo},
    FileStore, NormalizedPackageName,
};
use elsa::FrozenMap;
use futures::{pin_mut, stream, StreamExt};
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue, Method};
use indexmap::IndexMap;
use miette::{self, Diagnostic, IntoDiagnostic};
use pep440::Version;
use reqwest::{header::CACHE_CONTROL, Client, StatusCode};
use std::borrow::Borrow;
use std::fmt::Display;
use std::io::{Read};
use std::path::PathBuf;
use url::Url;

pub struct PackageDb {
    http: Http,

    /// Index URLS to query
    index_urls: Vec<Url>,

    /// A file store that stores metadata by hashes
    metadata_cache: FileStore,

    /// A cache of package name to version to artifacts.
    artifacts: FrozenMap<NormalizedPackageName, Box<IndexMap<Version, Vec<ArtifactInfo>>>>,
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
    pub async fn available_artifacts<P: Into<NormalizedPackageName>>(
        &self,
        p: P,
    ) -> miette::Result<&IndexMap<Version, Vec<ArtifactInfo>>> {
        let p = p.into();
        if let Some(cached) = self.artifacts.get(&p) {
            Ok(cached)
        } else {
            // Start downloading the information for each url.
            let http = self.http.clone();
            let request_iter = stream::iter(self.index_urls.iter())
                .map(|url| url.join(&format!("{}/", p.as_str())).expect("invalid url"))
                .map(|url| fetch_simple_api(&http, url))
                .buffer_unordered(10)
                .filter_map(|result| async { result.transpose() });

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
                .get_or_set(&hash, |w| w.write_all(blob))
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
                    let (blob, metadata) = artifact.metadata()?;
                    self.put_metadata_in_cache(artifact_info, &blob)?;
                    return Ok((artifact_info, metadata));
                }
                Err(err) => match err.downcast_ref::<HttpRequestError>() {
                    Some(HttpRequestError::NotCached(_)) => continue,
                    _ => return Err(err),
                },
            }
        }

        // We have exhausted all options to read the metadata from the cache. We'll have to hit the
        // network to get to the information.

        // Get the information from the first artifact. We assume the metadata is consistent across
        // all matching artifacts
        if let Some(artifact_info) = matching_artifacts.next() {

            // Retrieve the metadata instead of the entire wheel
            // If the dist-info is available separately, we can use that instead
            if artifact_info.dist_info_metadata.available {

                // Turn into PEP658 compliant URL
                let mut url = artifact_info.url.clone();
                url.set_path(&url.path().replace(".whl", ".whl.metadata"));

                let mut bytes = Vec::new();
                self.http
                    .request(
                        url,
                        Method::GET,
                        HeaderMap::default(),
                        CacheMode::Default,
                    ).await?
                    .into_body()
                    .read_to_end(&mut bytes).await.into_diagnostic()?;

                let metadata = A::parse_metadata(&bytes)?;
                self.put_metadata_in_cache(artifact_info, &bytes)?;
                return Ok((artifact_info, metadata));
            }

            let body = self
                .http
                .request(
                    artifact_info.url.clone(),
                    Method::GET,
                    HeaderMap::default(),
                    CacheMode::Default,
                )
                .await?
                .into_body()
                .force_local()
                .await
                .into_diagnostic()?;
            let artifact = A::new(
                artifact_info
                    .filename
                    .as_inner::<A::Name>()
                    .expect("this should never happen because we filter on matching artifacts only")
                    .clone(),
                body,
            )?;
            let (blob, metadata) = artifact.metadata()?;
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

    /// Get all package names in the index.
    pub async fn get_package_names(&self) -> miette::Result<Vec<String>> {
        let index_url = self.index_urls.first();
        if let Some(url) = index_url {
            let response = self
                .http
                .request(
                    url.clone(),
                    Method::GET,
                    HeaderMap::default(),
                    CacheMode::Default,
                )
                .await?;

            let mut bytes = response.into_body().force_local().await.into_diagnostic()?;
            let mut source = String::new();
            bytes.read_to_string(&mut source).into_diagnostic()?;
            html::parse_package_names_html(&source)
        } else {
            Ok(vec![])
        }
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
        let bytes = artifact_bytes
            .into_body()
            .force_local()
            .await
            .into_diagnostic()?;
        A::new(name.clone(), bytes)
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

    let response = http
        .request(url, Method::GET, headers, CacheMode::Default)
        .await?;

    // If the resource could not be found we simply return.
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("text/html")
        .to_owned();

    let url = response.extensions().get::<Url>().unwrap().to_owned();

    // Convert the information from html
    let mut bytes = Vec::new();
    response
        .into_body()
        .read_to_end(&mut bytes)
        .await
        .into_diagnostic()?;

    let content_type: mime::Mime = content_type.parse().into_diagnostic()?;
    match (
        content_type.type_().as_str(),
        content_type.subtype().as_str(),
    ) {
        ("text", "html") => {
            parse_project_info_html(&url, std::str::from_utf8(&bytes).into_diagnostic()?).map(Some)
        }
        _ => miette::bail!(
            "simple API page expected Content-Type: text/html, but got {}",
            &content_type
        ),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::artifact::Wheel;
    use tempfile::TempDir;
    use crate::PackageName;

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
            .available_artifacts("scikit-learn".parse::<PackageName>().unwrap())
            .await
            .unwrap();

        // Get the first wheel artifact
        let artifact_info = artifacts
            .iter()
            .flat_map(|(_, artifacts)| artifacts.iter())
            .collect::<Vec<_>>();

        let (_artifact, _metadata) = package_db
            .get_metadata::<Wheel, _>(&artifact_info)
            .await
            .unwrap();
    }
}

#[derive(Debug, Diagnostic)]
pub struct NotCached;

impl Display for NotCached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request not in cache, and cache_mode=OnlyIfCached")
    }
}

impl std::error::Error for NotCached {}
