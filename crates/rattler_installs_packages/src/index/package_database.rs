use crate::artifacts::{SDist, STree, Wheel};
use crate::index::file_store::FileStore;

use crate::index::html::{parse_package_names_html, parse_project_info_html};
use crate::index::http::{CacheMode, Http, HttpRequestError};
use crate::index::package_sources::PackageSources;
use crate::resolve::PypiVersion;
use crate::types::{ArtifactInfo, ArtifactType, ProjectInfo, STreeFilename, WheelCoreMetadata};

use crate::wheel_builder::{WheelBuildError, WheelBuilder, WheelCache};
use crate::{
    types::ArtifactFromBytes, types::InnerAsArtifactName, types::NormalizedPackageName,
    types::WheelFilename,
};
use async_http_range_reader::{AsyncHttpRangeReader, CheckSupportMethod};
use async_recursion::async_recursion;
use elsa::sync::FrozenMap;
use futures::{pin_mut, stream, StreamExt};
use indexmap::IndexMap;
use miette::{self, Diagnostic, IntoDiagnostic};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Method;

use reqwest::{header::CACHE_CONTROL, StatusCode};
use reqwest_middleware::ClientWithMiddleware;
use std::borrow::Borrow;

use std::path::PathBuf;

use itertools::Itertools;
use std::ops::Deref;
use std::sync::Arc;
use std::{fmt::Display, io::Read, path::Path};

use url::Url;

type VersionArtifacts = IndexMap<PypiVersion, Vec<Arc<ArtifactInfo>>>;

/// Cache of the available packages, artifacts and their metadata.
pub struct PackageDb {
    http: Http,

    sources: PackageSources,

    /// A file store that stores metadata by hashes
    metadata_cache: FileStore,

    /// A cache of package name to version to artifacts.
    artifacts: FrozenMap<NormalizedPackageName, Box<VersionArtifacts>>,

    /// Cache to locally built wheels
    local_wheel_cache: WheelCache,

    /// Reference to the cache directory for all caches
    cache_dir: PathBuf,
}

/// Type of request to get from the `available_artifacts` function.
pub enum ArtifactRequest {
    /// Get the available artifacts from the index.
    FromIndex(NormalizedPackageName),
    /// Get the artifact from a direct URL.
    DirectUrl {
        /// The name of the package
        name: NormalizedPackageName,
        /// The URL of the artifact
        url: Url,
        /// The wheel builder to use to build the artifact if its an SDist or STree
        wheel_builder: Arc<WheelBuilder>,
    },
}

pub(crate) struct DirectUrlArtifactResponse {
    pub(crate) artifact_info: Arc<ArtifactInfo>,
    pub(crate) artifact_versions: VersionArtifacts,
    pub(crate) metadata: (Vec<u8>, WheelCoreMetadata),
    pub(crate) artifact: ArtifactType,
}

impl PackageDb {
    /// Constructs a new [`PackageDb`] that reads information from the specified URLs.
    pub fn new(
        package_sources: PackageSources,
        client: ClientWithMiddleware,
        cache_dir: &Path,
    ) -> miette::Result<Self> {
        let http = Http::new(
            client,
            FileStore::new(&cache_dir.join("http")).into_diagnostic()?,
        );

        let metadata_cache = FileStore::new(&cache_dir.join("metadata")).into_diagnostic()?;
        let local_wheel_cache = WheelCache::new(cache_dir.join("local_wheels"));

        Ok(Self {
            http,
            sources: package_sources,
            metadata_cache,
            artifacts: Default::default(),
            local_wheel_cache,
            cache_dir: cache_dir.to_owned(),
        })
    }

    /// Returns the cache directory
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Returns the local wheel cache
    pub fn local_wheel_cache(&self) -> &WheelCache {
        &self.local_wheel_cache
    }

    /// Downloads and caches information about available artifacts of a package from the index.
    pub async fn available_artifacts<'wb>(
        &self,
        request: ArtifactRequest,
    ) -> miette::Result<&IndexMap<PypiVersion, Vec<Arc<ArtifactInfo>>>> {
        match request {
            ArtifactRequest::FromIndex(p) => {
                if let Some(cached) = self.artifacts.get(&p) {
                    return Ok(cached);
                }
                // Start downloading the information for each url.
                let http = self.http.clone();
                let index_urls = self.sources.index_url(&p);

                let urls = index_urls
                    .into_iter()
                    .map(|url| url.join(&format!("{}/", p.as_str())).expect("invalid url"))
                    .collect_vec();
                let request_iter = stream::iter(urls)
                    .map(|url| fetch_simple_api(&http, url))
                    .buffer_unordered(10)
                    .filter_map(|result| async { result.transpose() });

                pin_mut!(request_iter);

                // Add all the incoming results to the set of results
                let mut result = VersionArtifacts::default();
                while let Some(response) = request_iter.next().await {
                    for artifact in response?.files {
                        result
                            .entry(PypiVersion::Version {
                                version: artifact.filename.version().clone(),
                                package_allows_prerelease: artifact
                                    .filename
                                    .version()
                                    .any_prerelease(),
                            })
                            .or_default()
                            .push(Arc::new(artifact));
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
            ArtifactRequest::DirectUrl {
                name,
                url,
                wheel_builder,
            } => {
                self.get_artifact_by_direct_url(name, url, wheel_builder.deref())
                    .await
            }
        }
    }

    /// Returns the metadata from a set of artifacts. This function assumes that metadata is
    /// consistent for all artifacts of a single version.
    pub async fn get_metadata<'a, A: Borrow<ArtifactInfo>>(
        &self,
        artifacts: &'a [A],
        wheel_builder: Option<&WheelBuilder>,
    ) -> miette::Result<Option<(&'a A, WheelCoreMetadata)>> {
        // Check if we already have information about any of the artifacts cached.
        // Return if we do
        for artifact_info in artifacts.iter() {
            if let Some(metadata_bytes) = self.metadata_from_cache(artifact_info.borrow()).await {
                return Ok(Some((
                    artifact_info,
                    WheelCoreMetadata::try_from(metadata_bytes.as_slice()).into_diagnostic()?,
                )));
            }
        }

        // Apparently we dont have any metadata cached yet.
        // Next up check if we have downloaded any artifacts but do not have the metadata stored yet
        // In this case we can just return it
        let result = self.metadata_for_cached_artifacts(artifacts).await?;
        if result.is_some() {
            return Ok(result);
        }

        // We have exhausted all options to read the metadata from the cache. We'll have to hit the
        // network to get to the information.
        // Let's try to get information for any wheels that we have
        // first
        let result = self.get_metadata_wheels(artifacts, wheel_builder).await?;
        if result.is_some() {
            return Ok(result);
        }

        // No wheels found with metadata, try to get metadata from sdists
        // by building them or using the appropriate hooks
        if let Some(wheel_builder) = wheel_builder {
            let sdist = self.get_metadata_sdists(artifacts, wheel_builder).await?;
            if sdist.is_some() {
                return Ok(sdist);
            }

            let stree = self.get_metadata_stree(artifacts, wheel_builder).await?;
            if stree.is_some() {
                return Ok(stree);
            }
        }

        // Ok literally nothing seems to work, so we'll just return None
        Ok(None)
    }

    /// Opens the specified artifact info. Downloads the artifact data from the remote location if
    /// the information is not already cached.
    #[async_recursion]
    pub async fn get_wheel(
        &self,
        artifact_info: &ArtifactInfo,
        builder: Option<&'async_recursion WheelBuilder>,
    ) -> miette::Result<Wheel> {
        // TODO: add support for this currently there are not cached, they will be repeatedly downloaded between runs
        if artifact_info.is_direct_url {
            if let Some(builder) = builder {
                let response = super::direct_url::fetch_artifact_and_metadata_by_direct_url(
                    &self.http,
                    artifact_info.filename.distribution_name(),
                    artifact_info.url.clone(),
                    builder,
                )
                .await?;

                match response.artifact {
                    ArtifactType::Wheel(wheel) => return Ok(wheel),
                    ArtifactType::SDist(sdist) => {
                        return builder.build_wheel(&sdist).await.into_diagnostic()
                    }
                    ArtifactType::STree(stree) => {
                        return builder.build_wheel(&stree).await.into_diagnostic()
                    }
                }
            } else {
                miette::bail!("cannot build wheel without a wheel builder");
            }
        }

        // Try to build the wheel for this SDist if possible
        if artifact_info.is::<SDist>() {
            if let Some(builder) = builder {
                let sdist = self
                    .get_cached_artifact::<SDist>(artifact_info, CacheMode::Default)
                    .await?;

                return builder.build_wheel(&sdist).await.into_diagnostic();
            } else {
                miette::bail!("cannot build wheel without a wheel builder");
            }
        }

        // Otherwise just retrieve the wheel
        self.get_cached_artifact::<Wheel>(artifact_info, CacheMode::Default)
            .await
    }

    /// Get artifact directly from file, vcs, or url
    async fn get_artifact_by_direct_url<P: Into<NormalizedPackageName>>(
        &self,
        p: P,
        url: Url,
        wheel_builder: &WheelBuilder,
    ) -> miette::Result<&IndexMap<PypiVersion, Vec<Arc<ArtifactInfo>>>> {
        let p = p.into();

        if let Some(cached) = self.artifacts.get(&p) {
            return Ok(cached);
        }

        let response = super::direct_url::fetch_artifact_and_metadata_by_direct_url(
            &self.http,
            p.clone(),
            url,
            wheel_builder,
        )
        .await?;

        self.put_metadata_in_cache(&response.artifact_info, &response.metadata.0)
            .await?;

        Ok(self
            .artifacts
            .insert(p, Box::new(response.artifact_versions)))
    }

    /// Reads the metadata for the given artifact from the cache or return `None` if the metadata
    /// could not be found in the cache.
    async fn metadata_from_cache(&self, ai: &ArtifactInfo) -> Option<Vec<u8>> {
        let mut data = self.metadata_cache.get(&ai.hashes.as_ref()?).await?;
        let mut bytes = Vec::new();
        data.read_to_end(&mut bytes).ok()?;
        Some(bytes)
    }

    /// Writes the metadata for the given artifact into the cache. If the metadata already exists
    /// its not overwritten.
    async fn put_metadata_in_cache(&self, ai: &ArtifactInfo, blob: &[u8]) -> miette::Result<()> {
        if let Some(hash) = &ai.hashes {
            self.metadata_cache
                .get_or_set(&hash, |w| w.write_all(blob))
                .await
                .into_diagnostic()?;
        }
        Ok(())
    }

    /// Check if we already have one of the artifacts cached. Only do this if we have more than
    /// one artifact because otherwise, we'll do a request anyway if we dont have the file
    /// cached.
    async fn metadata_for_cached_artifacts<'a, A: Borrow<ArtifactInfo>>(
        &self,
        artifacts: &'a [A],
    ) -> miette::Result<Option<(&'a A, WheelCoreMetadata)>> {
        for artifact_info in artifacts.iter() {
            let artifact_info_ref = artifact_info.borrow();
            if artifact_info_ref.is::<Wheel>() && !artifact_info_ref.is_direct_url {
                let result = self
                    .get_cached_artifact::<Wheel>(artifact_info_ref, CacheMode::OnlyIfCached)
                    .await;
                match result {
                    Ok(artifact) => {
                        // Apparently the artifact has been downloaded, but its metadata has not been
                        // cached yet. Lets store it there.
                        let metadata = artifact.metadata();
                        match metadata {
                            Ok((blob, metadata)) => {
                                self.put_metadata_in_cache(artifact_info_ref, &blob).await?;
                                return Ok(Some((artifact_info, metadata)));
                            }
                            Err(err) => {
                                tracing::warn!(
                                    "Error reading metadata from artifact '{}' skipping ({:?})",
                                    artifact_info_ref.filename,
                                    err
                                );
                                continue;
                            }
                        }
                    }
                    Err(err) => match err.downcast_ref::<HttpRequestError>() {
                        Some(HttpRequestError::NotCached(_)) => continue,
                        _ => return Err(err),
                    },
                }
            }
            // We know that it is an sdist
            else if artifact_info_ref.is::<SDist>() && !artifact_info_ref.is_direct_url {
                let result = self
                    .get_cached_artifact::<SDist>(artifact_info_ref, CacheMode::OnlyIfCached)
                    .await;

                match result {
                    Ok(sdist) => {
                        // Save the pep643 metadata in the cache if it is available
                        let metadata = sdist.pep643_metadata().into_diagnostic()?;
                        if let Some((bytes, _)) = metadata {
                            self.put_metadata_in_cache(artifact_info_ref, &bytes)
                                .await?;
                        }
                    }
                    Err(err) => match err.downcast_ref::<HttpRequestError>() {
                        Some(HttpRequestError::NotCached(_)) => continue,
                        _ => return Err(err),
                    },
                }
            }
        }
        Ok(None)
    }

    async fn get_metadata_wheels<'a, A: Borrow<ArtifactInfo>>(
        &self,
        artifacts: &'a [A],
        wheel_builder: Option<&WheelBuilder>,
    ) -> miette::Result<Option<(&'a A, WheelCoreMetadata)>> {
        let wheels = artifacts
            .iter()
            .filter(|artifact_info| (*artifact_info).borrow().is::<Wheel>());

        // Get the information from the first artifact. We assume the metadata is consistent across
        // all matching artifacts
        for artifact_info in wheels {
            let ai = artifact_info.borrow();

            // Retrieve the metadata instead of the entire wheel
            // If the dist-info is available separately, we can use that instead
            if ai.dist_info_metadata.available {
                return Ok(Some(self.get_pep658_metadata(artifact_info).await?));
            }

            // Try to load the data by sparsely reading the artifact (if supported)
            if let Some(metadata) = self.get_lazy_metadata_wheel(ai).await? {
                return Ok(Some((artifact_info, metadata)));
            }

            let metadata = if ai.is_direct_url {
                if let Some(wheel_builder) = wheel_builder {
                    let response = super::direct_url::fetch_artifact_and_metadata_by_direct_url(
                        &self.http,
                        ai.filename.distribution_name(),
                        ai.url.clone(),
                        wheel_builder,
                    )
                    .await;
                    match response {
                        Err(err) => Err(miette::miette!(err.to_string())),
                        Ok(response) => Ok(response.metadata),
                    }
                } else {
                    miette::bail!("cannot build wheel without a wheel builder");
                }
            } else {
                // Otherwise download the entire artifact
                let artifact = self
                    .get_cached_artifact::<Wheel>(ai, CacheMode::Default)
                    .await?;
                artifact.metadata()
            };

            match metadata {
                Ok((blob, metadata)) => {
                    self.put_metadata_in_cache(ai, &blob).await?;
                    return Ok(Some((artifact_info, metadata)));
                }
                Err(err) => {
                    tracing::warn!(
                        "Error reading metadata from artifact '{}' skipping ({:?})",
                        ai.filename,
                        err
                    );
                    continue;
                }
            }
        }
        Ok(None)
    }

    async fn get_metadata_sdists<'a, A: Borrow<ArtifactInfo>>(
        &self,
        artifacts: &'a [A],
        wheel_builder: &WheelBuilder,
    ) -> miette::Result<Option<(&'a A, WheelCoreMetadata)>> {
        let sdists = artifacts
            .iter()
            .filter(|artifact_info| (*artifact_info).borrow().is::<SDist>());

        // Keep track of errors
        // only print these if we have not been able to find any metadata
        let mut errors = Vec::new();
        for ai in sdists {
            let artifact_info: &ArtifactInfo = ai.borrow();
            let metadata = if artifact_info.is_direct_url {
                let response = super::direct_url::fetch_artifact_and_metadata_by_direct_url(
                    &self.http,
                    artifact_info.filename.distribution_name(),
                    artifact_info.url.clone(),
                    wheel_builder,
                )
                .await;
                match response {
                    Err(err) => Err(WheelBuildError::Error(err.to_string())),
                    Ok(response) => Ok(response.metadata),
                }
            } else {
                let artifact = self
                    .get_cached_artifact::<SDist>(artifact_info, CacheMode::Default)
                    .await?;
                wheel_builder.get_sdist_metadata(&artifact).await
            };

            match metadata {
                Ok((blob, metadata)) => {
                    self.put_metadata_in_cache(artifact_info, &blob).await?;
                    return Ok(Some((ai, metadata)));
                }
                Err(err) => {
                    errors.push(format!(
                        "error while processing source distribution '{}': \n {}",
                        artifact_info.filename, err
                    ));
                    continue;
                }
            }
        }

        // Check if errors is empty and if not return an error
        if !errors.is_empty() {
            miette::bail!("{}", errors.join("\n"));
        }

        Ok(None)
    }

    async fn get_metadata_stree<'a, A: Borrow<ArtifactInfo>>(
        &self,
        artifacts: &'a [A],
        wheel_builder: &WheelBuilder,
    ) -> miette::Result<Option<(&'a A, WheelCoreMetadata)>> {
        let stree = artifacts
            .iter()
            .filter(|artifact_info| (*artifact_info).borrow().is::<STree>());

        // Keep track of errors
        // only print these if we have not been able to find any metadata
        let mut errors = Vec::new();
        for ai in stree {
            let artifact_info: &ArtifactInfo = ai.borrow();
            let stree_name = artifact_info
                .filename
                .as_inner::<STreeFilename>()
                .unwrap_or_else(|| {
                    panic!(
                        "the specified artifact '{}' does not refer to type requested to read",
                        artifact_info.filename
                    )
                });
            let response = super::direct_url::fetch_artifact_and_metadata_by_direct_url(
                &self.http,
                stree_name.distribution.clone(),
                artifact_info.url.clone(),
                wheel_builder,
            )
            .await;

            match response {
                Ok(direct_response) => {
                    let metadata_and_bytes = direct_response.metadata;
                    self.put_metadata_in_cache(artifact_info, &metadata_and_bytes.0)
                        .await?;
                    return Ok(Some((ai, metadata_and_bytes.1)));
                }
                Err(err) => {
                    errors.push(format!(
                        "error while processing source tree '{}': \n {}",
                        artifact_info.filename, err
                    ));
                    continue;
                }
            }
        }

        if !errors.is_empty() {
            miette::bail!("{}", errors.join("\n"));
        }

        Ok(None)
    }

    async fn get_lazy_metadata_wheel(
        &self,
        artifact_info: &ArtifactInfo,
    ) -> miette::Result<Option<WheelCoreMetadata>> {
        tracing::info!(url=%artifact_info.url, "lazy reading artifact");

        // Check if the artifact is the same type as the info.
        let name = WheelFilename::try_as(&artifact_info.filename)
            .expect("the specified artifact does not refer to type requested to read");

        if let Ok((mut reader, _)) = AsyncHttpRangeReader::new(
            self.http.client.clone(),
            artifact_info.url.clone(),
            CheckSupportMethod::Head,
        )
        .await
        {
            match Wheel::read_metadata_bytes(name, &mut reader).await {
                Ok((blob, metadata)) => {
                    self.put_metadata_in_cache(artifact_info, &blob).await?;
                    return Ok(Some(metadata));
                }
                Err(err) => {
                    tracing::warn!("failed to sparsely read wheel file: {err}, falling back to downloading the whole file");
                }
            }
        }

        Ok(None)
    }

    /// Retrieve the PEP658 metadata for the given artifact.
    /// This assumes that the metadata is available in the repository
    /// This can be checked with the ArtifactInfo
    async fn get_pep658_metadata<'a, A: Borrow<ArtifactInfo>>(
        &self,
        artifact_info: &'a A,
    ) -> miette::Result<(&'a A, WheelCoreMetadata)> {
        let ai = artifact_info.borrow();

        // Check if the artifact is the same type as the info.
        WheelFilename::try_as(&ai.filename)
            .expect("the specified artifact does not refer to type requested to read");

        // Turn into PEP658 compliant URL
        let mut url = ai.url.clone();
        url.set_path(&url.path().replace(".whl", ".whl.metadata"));

        let mut bytes = Vec::new();
        self.http
            .request(url, Method::GET, HeaderMap::default(), CacheMode::NoStore)
            .await?
            .into_body()
            .read_to_end(&mut bytes)
            .await
            .into_diagnostic()?;

        let metadata = WheelCoreMetadata::try_from(bytes.as_slice()).into_diagnostic()?;
        self.put_metadata_in_cache(ai, &bytes).await?;
        Ok((artifact_info, metadata))
    }

    /// Get all package names in the index.
    pub async fn get_package_names(&self) -> miette::Result<Vec<String>> {
        let index_url = self.sources.default_index_url();
        let response = self
            .http
            .request(
                index_url,
                Method::GET,
                HeaderMap::default(),
                CacheMode::Default,
            )
            .await?;

        let mut bytes = response.into_body().into_local().await.into_diagnostic()?;
        let mut source = String::new();
        bytes.read_to_string(&mut source).into_diagnostic()?;
        parse_package_names_html(&source)
    }

    /// Opens the specified artifact info. Depending on the specified `cache_mode`, downloads the
    /// artifact data from the remote location if the information is not already cached.
    async fn get_cached_artifact<A: ArtifactFromBytes>(
        &self,
        artifact_info: &ArtifactInfo,
        cache_mode: CacheMode,
    ) -> miette::Result<A> {
        // Check if the artifact is the same type as the info.
        let name = artifact_info
            .filename
            .as_inner::<A::Name>()
            .unwrap_or_else(|| {
                panic!(
                    "the specified artifact '{}' does not refer to type requested to read",
                    artifact_info.filename
                )
            });

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
            .into_local()
            .await
            .into_diagnostic()?;
        A::from_bytes(name.clone(), bytes)
    }
}

async fn fetch_simple_api(http: &Http, url: Url) -> miette::Result<Option<ProjectInfo>> {
    let mut headers = HeaderMap::new();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=0"));

    let response = match http
        .request(url.to_owned(), Method::GET, headers, CacheMode::Default)
        .await
    {
        Ok(response) => response,
        Err(err) => {
            if let HttpRequestError::HttpError(err) = &err {
                if err.status() == Some(StatusCode::NOT_FOUND) {
                    return Ok(None);
                }
            }
            return Err(err.into());
        }
    };

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
    use crate::types::PackageName;
    use reqwest::Client;
    use tempfile::TempDir;
    use tokio::task::JoinHandle;

    use crate::index::package_sources::PackageSourcesBuilder;
    use axum::response::{Html, IntoResponse};
    use axum::routing::get;
    use axum::Router;
    use insta::assert_debug_snapshot;
    use std::future::IntoFuture;
    use std::net::SocketAddr;
    use tower_http::add_extension::AddExtensionLayer;

    async fn get_index(
        axum::Extension(served_package): axum::Extension<String>,
    ) -> impl IntoResponse {
        // Return the HTML response with the list of packages
        let package_list = format!(
            r#"
            <a href="/{served_package}">{served_package}</a>
        "#
        );

        let html = format!("<html><body>{}</body></html>", package_list);
        Html(html)
    }

    async fn get_package(
        axum::Extension(served_package): axum::Extension<String>,
        axum::extract::Path(requested_package): axum::extract::Path<String>,
    ) -> impl IntoResponse {
        if served_package == requested_package {
            let wheel_name = format!("{}-1.0-py3-none-any.whl", served_package);
            let link_list = format!(
                r#"
                <a href="/files/{wheel_name}">{wheel_name}</a>
            "#
            );

            let html = format!("<html><body>{}</body></html>", link_list);
            Html(html).into_response()
        } else {
            axum::http::StatusCode::NOT_FOUND.into_response()
        }
    }

    async fn make_simple_server(
        package_name: &str,
    ) -> anyhow::Result<(Url, JoinHandle<Result<(), std::io::Error>>)> {
        let addr = SocketAddr::new([127, 0, 0, 1].into(), 0);
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        let address = listener.local_addr()?;

        let router = Router::new()
            .route("/simple", get(get_index))
            .route("/simple/:package/", get(get_package))
            .layer(AddExtensionLayer::new(package_name.to_string()));

        let server = axum::serve(listener, router).into_future();

        // Spawn the server.
        let join_handle = tokio::spawn(server);

        println!("Server started");
        let url = format!("http://{}/simple/", address).parse()?;
        Ok((url, join_handle))
    }

    fn make_package_db() -> (TempDir, PackageDb) {
        let url = Url::parse("https://pypi.org/simple/").unwrap();

        let cache_dir = TempDir::new().unwrap();
        let package_db = PackageDb::new(
            url.into(),
            ClientWithMiddleware::from(Client::new()),
            cache_dir.path(),
        )
        .unwrap();

        (cache_dir, package_db)
    }

    #[tokio::test]
    async fn test_available_packages() {
        let (_cache_dir, package_db) = make_package_db();
        let name = "scikit-learn".parse::<PackageName>().unwrap();

        // Get all the artifacts
        let artifacts = package_db
            .available_artifacts(ArtifactRequest::FromIndex(name.into()))
            .await
            .unwrap();

        // Get the first wheel artifact
        let artifact_info = artifacts
            .iter()
            .flat_map(|(_, artifacts)| artifacts.iter().cloned())
            .collect::<Vec<_>>();

        let (_artifact, _metadata) = package_db
            .get_metadata(&artifact_info, None)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_index_mapping() -> anyhow::Result<()> {
        // just a random UUID
        let package_name = "c99d774d1a5a4a7fa2c2820bae6688e7".to_string();

        let (test_index, _server) = make_simple_server(&package_name).await?;
        let pypi_index = Url::parse("https://pypi.org/simple/")?;

        let index_alias = "test-index".to_string();

        let package_name = package_name.parse::<PackageName>()?;
        let normalized_name = NormalizedPackageName::from(package_name);

        let cache_dir = TempDir::new()?;
        let sources = PackageSourcesBuilder::new(pypi_index)
            .with_index(&index_alias, &test_index)
            // Exists in pypi but not in our index
            .with_override("pytest".parse()?, &index_alias)
            // Doesn't exist in pypi (hopefully), should exist in our index
            .with_override(normalized_name.clone(), &index_alias)
            .build()
            .unwrap();

        let package_db = PackageDb::new(
            sources,
            ClientWithMiddleware::from(Client::new()),
            cache_dir.path(),
        )
        .unwrap();

        let pytest_name = "pytest".parse::<PackageName>()?;
        let pytest_result = package_db
            .available_artifacts(ArtifactRequest::FromIndex(pytest_name.into()))
            .await;

        // Should not fail because 404s are skipped
        assert!(
            pytest_result.is_ok(),
            "`pytest_result` not ok: {:?}",
            pytest_result
        );

        let test_package_result = package_db
            .available_artifacts(ArtifactRequest::FromIndex(normalized_name))
            .await
            .unwrap();

        assert_debug_snapshot!(test_package_result.keys(), @r###"
        [
            Version {
                version: Version {
                    epoch: 0,
                    release: [
                        1,
                        0,
                    ],
                    pre: None,
                    post: None,
                    dev: None,
                    local: None,
                },
                package_allows_prerelease: false,
            },
        ]
        "###);

        Ok(())
    }

    #[tokio::test]
    async fn test_pep658() {
        let (_cache_dir, package_db) = make_package_db();
        let name = "scikit-learn".parse::<PackageName>().unwrap();

        // Get all the artifacts
        let artifacts = package_db
            .available_artifacts(ArtifactRequest::FromIndex(name.into()))
            .await
            .unwrap();

        // Get the artifact with dist-info attribute
        let artifact_info = artifacts
            .iter()
            .flat_map(|(_, artifacts)| artifacts.iter())
            // This signifies that a PEP658 metadata file is available
            .find(|a| a.dist_info_metadata.available)
            .unwrap();

        let (_artifact, _metadata) = package_db.get_pep658_metadata(artifact_info).await.unwrap();
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
