use crate::artifacts::{SDist, STree, SourceArtifact, Wheel};
use crate::index::file_store::FileStore;
use crate::index::html::{parse_package_names_html, parse_project_info_html};
use crate::index::http::{CacheMode, Http, HttpRequestError};
use crate::resolve::PypiVersion;
use crate::types::{
    ArtifactHashes, ArtifactInfo, ArtifactName, DistInfoMetadata, PackageName, ProjectInfo,
    SDistFilename, SDistFormat, STreeFilename, WheelCoreMetadata, Yanked,
};
use crate::wheel_builder::{WheelBuildError, WheelBuilder, WheelCache};
use crate::{
    types::Artifact, types::InnerAsArtifactName, types::NormalizedPackageName, types::Version,
    types::WheelFilename,
};
use async_http_range_reader::{AsyncHttpRangeReader, CheckSupportMethod};
use async_recursion::async_recursion;
use elsa::sync::FrozenMap;
use futures::{pin_mut, stream, StreamExt};
use http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, Method};
use indexmap::IndexMap;
use miette::{self, Diagnostic, IntoDiagnostic};
use parking_lot::Mutex;
use rattler_build::recipe::parser::{GitRev, GitSource, GitUrl};
use rattler_build::source::git_source::git_src;
use rattler_digest::{compute_bytes_digest, Sha256};
use reqwest::{header::CACHE_CONTROL, Client, StatusCode};
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::{fmt::Display, io::Read, path::Path};
use tempfile::tempdir;
use url::Url;

use super::parse_hash;

/// Cache of the available packages, artifacts and their metadata.
pub struct PackageDb {
    http: Http,

    /// Index URLS to query
    index_urls: Vec<Url>,

    /// A file store that stores metadata by hashes
    metadata_cache: FileStore,

    /// A cache of package name to version to artifacts.
    artifacts: FrozenMap<NormalizedPackageName, Box<IndexMap<PypiVersion, Vec<ArtifactInfo>>>>,

    /// Cache to locally built wheels
    local_wheel_cache: WheelCache,

    /// Reference to the cache directory for all caches
    cache_dir: PathBuf,
}

impl PackageDb {
    /// Constructs a new [`PackageDb`] that reads information from the specified URLs.
    pub fn new(client: Client, index_urls: &[Url], cache_dir: &Path) -> std::io::Result<Self> {
        Ok(Self {
            http: Http::new(client, FileStore::new(&cache_dir.join("http"))?),
            index_urls: index_urls.into(),
            metadata_cache: FileStore::new(&cache_dir.join("metadata"))?,
            artifacts: Default::default(),
            local_wheel_cache: WheelCache::new(cache_dir.join("local_wheels")),
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

    /// Downloads and caches information about available artifiacts of a package from the index.
    pub async fn available_artifacts<P: Into<NormalizedPackageName>>(
        &self,
        p: P,
    ) -> miette::Result<&IndexMap<PypiVersion, Vec<ArtifactInfo>>> {
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
            let mut result: IndexMap<PypiVersion, Vec<ArtifactInfo>> = Default::default();
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

    /// Get artifact by file URL
    pub async fn get_file_artifact<'a, 'i, P: Into<NormalizedPackageName>>(
        &self,
        p: P,
        url: Url,
        wheel_builder: &WheelBuilder<'a, 'i>,
    ) -> miette::Result<&IndexMap<PypiVersion, Vec<ArtifactInfo>>> {
        let str_name = url.path();
        let path = Path::new(str_name);

        let normalized_package_name = p.into();

        let dummy_version =
            Version::from_str("0.0.0").expect("0.0.0 version should always be parseable");

        let (data_bytes, metadata, artifact_name) = if path.is_file() && str_name.ends_with(".whl") {
            let wheel = Wheel::from_path(path, &normalized_package_name).
            map_err(|e| WheelBuildError::Error(format!("Could not build wheel: {}", e))).into_diagnostic()?;
            
            let (data_bytes, metadata) = wheel.metadata()?;
            (data_bytes, metadata, ArtifactName::Wheel(wheel.name().clone()), )

        } else if path.is_file() {
            let distribution = PackageName::from(normalized_package_name.clone());

            let format = SDistFormat::get_extension(str_name).into_diagnostic()?;

            let dummy_sdist_file_name = SDistFilename {
                distribution,
                version: dummy_version,
                format,
            };
            let bytes = fs::File::open(path).into_diagnostic()?;

            let dummy_sdist = SDist::new(dummy_sdist_file_name, Box::new(bytes))?;

            let (data_bytes, metadata) = wheel_builder
                .get_sdist_metadata(&dummy_sdist)
                .await
                .into_diagnostic()?;

            // construct a real sdist filename
            let sdist_filename = SDistFilename {
                distribution: metadata.name.clone(),
                version: metadata.version.clone(),
                format,
            };

            let filename = ArtifactName::SDist(sdist_filename.clone());

            (data_bytes, metadata, filename)
        } else {
            let distribution = PackageName::from(normalized_package_name.clone());

            let stree_file_name = STreeFilename {
                distribution,
                version: url.clone(),
            };

            let stree = STree {
                name: stree_file_name,
                location: Mutex::new(path.to_path_buf()),
            };

            let (data_bytes, metadata) = wheel_builder
                .get_sdist_metadata(&stree)
                .await
                .into_diagnostic()?;

            let stree_file_name = STreeFilename {
                distribution: metadata.name.clone(),
                version: url.clone(),
            };

            (data_bytes, metadata, ArtifactName::STree(stree_file_name))
        };

        // TODO: extract hash from url if it's present
        let project_hash = ArtifactHashes {
            sha256: Some(compute_bytes_digest::<Sha256>(url.as_str().as_bytes())),
        };

        // let's try to build an artifact info
        let artifact_info = ArtifactInfo {
            filename: artifact_name,
            url: url.clone(),
            hashes: Some(project_hash),
            requires_python: metadata.requires_python,
            dist_info_metadata: DistInfoMetadata::default(),
            yanked: Yanked::default(),
        };

        let mut result: IndexMap<PypiVersion, Vec<ArtifactInfo>> = Default::default();

        result
            .entry(PypiVersion::Url(url))
            .or_default()
            .push(artifact_info.clone());

        self.put_metadata_in_cache(&artifact_info, &data_bytes)
            .await?;

        Ok(self
            .artifacts
            .insert(normalized_package_name.clone(), Box::new(result)))
    }

    /// Get artifact by file URL
    pub async fn get_http_artifact<'a, 'i, P: Into<NormalizedPackageName>>(
        &self,
        p: P,
        url: Url,
        wheel_builder: &WheelBuilder<'a, 'i>,
    ) -> miette::Result<&IndexMap<PypiVersion, Vec<ArtifactInfo>>> {
        let str_name = url.path();
        let url_hash = url.fragment().and_then(parse_hash);

        let normalized_package_name = p.into();

        // Get the contents of the artifact
        let artifact_bytes = self
            .http
            .request(
                url.clone(),
                Method::GET,
                HeaderMap::default(),
                CacheMode::Default,
            )
            .await?;

        let bytes = artifact_bytes
            .into_body()
            .into_local()
            .await
            .into_diagnostic()?;

        let (filename, data_bytes, metadata) = match str_name.ends_with(".whl") {
            true => {
                // it's wheel
                let url_path = PathBuf::from_str(url.path()).unwrap();
                let file_name = url_path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .ok_or_else(|| {
                        miette::miette!("path {:?} does not contain a wheel filename", url_path)
                    })?;
                let wheel_filename =
                    WheelFilename::from_filename(file_name, &normalized_package_name)
                        .into_diagnostic()?;
                let wheel = Wheel::new(wheel_filename.clone(), Box::new(bytes))?;
                let filename = ArtifactName::Wheel(wheel_filename);
                let (data_bytes, metadata) = wheel.metadata()?;
                (filename, data_bytes, metadata)
            }
            false => {
                // it's probably an sdist
                let distribution = PackageName::from(normalized_package_name.clone());
                let version =
                    Version::from_str("0.0.0").expect("0.0.0 version should always be parseable");
                let format = SDistFormat::get_extension(str_name).into_diagnostic()?;

                let dummy_sdist_file_name = SDistFilename {
                    distribution,
                    version,
                    format,
                };

                let dummy_sdist = SDist::new(dummy_sdist_file_name, Box::new(bytes))?;

                // verify hash of url fragment with sdist
                let sdist_hash = dummy_sdist.get_wheel_key().into_diagnostic()?;

                if let Some(hash) = url_hash.clone() {
                    let hash_str = format!("{:?}", hash);
                    assert_eq!(hash_str, sdist_hash.to_string())
                }

                let (data_bytes, metadata) = wheel_builder
                    .get_sdist_metadata(&dummy_sdist)
                    .await
                    .into_diagnostic()?;

                // construct a real sdist filename
                let sdist_filename = SDistFilename {
                    distribution: metadata.name.clone(),
                    version: metadata.version.clone(),
                    format,
                };
                let filename = ArtifactName::SDist(sdist_filename.clone());
                (filename, data_bytes, metadata)
            }
        };

        let requires_python = metadata.requires_python;

        let dist_info_metadata = DistInfoMetadata {
            available: false,
            hashes: ArtifactHashes::default(),
        };

        let yanked = Yanked {
            yanked: false,
            reason: None,
        };

        let artifact_info = ArtifactInfo {
            filename,
            url: url.clone(),
            hashes: url_hash,
            requires_python,
            dist_info_metadata,
            yanked,
        };

        let mut result: IndexMap<PypiVersion, Vec<ArtifactInfo>> = Default::default();
        result
            .entry(PypiVersion::Url(url))
            .or_default()
            .push(artifact_info.clone());

        self.put_metadata_in_cache(&artifact_info, &data_bytes)
            .await?;

        Ok(self
            .artifacts
            .insert(normalized_package_name.clone(), Box::new(result)))
    }

    /// Get artifact by git reference
    pub async fn get_git_artifact<'a, 'i, P: Into<NormalizedPackageName>>(
        &self,
        p: P,
        url: Url,
        wheel_builder: &WheelBuilder<'a, 'i>,
    ) -> miette::Result<&IndexMap<PypiVersion, Vec<ArtifactInfo>>> {
        let str_name = url.as_str();

        let new_str_url = str_name.replace("git+https", "https");

        let checkout_url = Url::from_str(new_str_url.as_str()).unwrap();

        let normalized_package_name = p.into();

        let git_source = GitSource {
            url: GitUrl::Url(checkout_url.clone()),
            rev: GitRev::Head,
            depth: None,
            patches: vec![],
            target_directory: None,
            lfs: false,
        };

        let temp_dir = tempdir().unwrap();

        let cache_dir = temp_dir.path().join("rip-git-cache");
        let clone_dir = temp_dir.path().join("rip-clone-dir");

        let (location, _rev) =
            git_src(&git_source, cache_dir.as_ref(), clone_dir.as_ref()).unwrap();

        let distribution = PackageName::from(normalized_package_name.clone());

        let stree_partial_file_name = STreeFilename {
            distribution,
            version: url.clone(),
        };

        let stree = STree {
            name: stree_partial_file_name,
            location: Mutex::new(location.to_path_buf()),
        };

        let (data_bytes, metadata) = wheel_builder
            .get_sdist_metadata(&stree)
            .await
            .into_diagnostic()?;

        let stree_partial_file_name = STreeFilename {
            distribution: metadata.name.clone(),
            version: url.clone(),
        };

        let filename = ArtifactName::STree(stree_partial_file_name.clone());

        let requires_python = metadata.requires_python;

        let dist_info_metadata = DistInfoMetadata {
            available: false,
            hashes: ArtifactHashes::default(),
        };

        let yanked = Yanked {
            yanked: false,
            reason: None,
        };

        let project_hash = ArtifactHashes {
            sha256: Some(compute_bytes_digest::<Sha256>(url.as_str().as_bytes())),
        };

        let artifact_info = ArtifactInfo {
            filename,
            url: url.clone(),
            hashes: Some(project_hash),
            requires_python,
            dist_info_metadata,
            yanked,
        };

        let mut result: IndexMap<PypiVersion, Vec<ArtifactInfo>> = Default::default();
        result
            .entry(PypiVersion::Url(url))
            .or_default()
            .push(artifact_info.clone());

        self.put_metadata_in_cache(&artifact_info, &data_bytes)
            .await?;

        Ok(self
            .artifacts
            .insert(normalized_package_name.clone(), Box::new(result)))
    }

    /// Get artifact directly from file, vcs, or url
    pub async fn get_artifact_by_url<'a, 'i, P: Into<NormalizedPackageName>>(
        &self,
        p: P,
        url: Url,
        wheel_builder: &WheelBuilder<'a, 'i>,
    ) -> miette::Result<&IndexMap<PypiVersion, Vec<ArtifactInfo>>> {
        //conver to str
        let p = p.into();

        if let Some(cached) = self.artifacts.get(&p) {
            Ok(cached)
        } else if url.scheme() == "file" {
            self.get_file_artifact(p, url, wheel_builder).await
        } else if url.scheme() == "https" {
            self.get_http_artifact(p, url, wheel_builder).await
        } else if url.scheme() == "git+https" {
            self.get_git_artifact(p, url, wheel_builder).await
        } else {
            // make it a better error
            Err(miette::miette!(
                "Usage of unsecure protocol or unsupported scheme {:?}",
                url.scheme()
            ))
        }
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
    async fn metadata_for_cached_artifacts<'a>(
        &self,
        artifacts: &[&'a ArtifactInfo],
    ) -> miette::Result<Option<(&'a ArtifactInfo, WheelCoreMetadata)>> {
        for artifact_info in artifacts.iter() {
            if artifact_info.is::<Wheel>() {
                let result = self
                    .get_artifact_with_cache::<Wheel>(artifact_info, CacheMode::OnlyIfCached)
                    .await;
                match result {
                    Ok(artifact) => {
                        // Apparently the artifact has been downloaded, but its metadata has not been
                        // cached yet. Lets store it there.
                        let metadata = artifact.metadata();
                        match metadata {
                            Ok((blob, metadata)) => {
                                self.put_metadata_in_cache(artifact_info, &blob).await?;
                                return Ok(Some((artifact_info, metadata)));
                            }
                            Err(err) => {
                                tracing::warn!(
                                    "Error reading metadata from artifact '{}' skipping ({:?})",
                                    artifact_info.filename,
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
            else {
                let result = self
                    .get_artifact_with_cache::<SDist>(artifact_info, CacheMode::OnlyIfCached)
                    .await;

                match result {
                    Ok(sdist) => {
                        // Save the pep643 metadata in the cache if it is available
                        let metadata = sdist.pep643_metadata().into_diagnostic()?;
                        if let Some((bytes, _)) = metadata {
                            self.put_metadata_in_cache(artifact_info, &bytes).await?;
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

    async fn get_metadata_wheels<'a>(
        &self,
        artifacts: &[&'a ArtifactInfo],
    ) -> miette::Result<Option<(&'a ArtifactInfo, WheelCoreMetadata)>> {
        let wheels = artifacts
            .iter()
            .copied()
            .filter(|artifact_info| artifact_info.is::<Wheel>());

        // Get the information from the first artifact. We assume the metadata is consistent across
        // all matching artifacts
        for artifact_info in wheels {
            // Retrieve the metadata instead of the entire wheel
            // If the dist-info is available separately, we can use that instead
            if artifact_info.dist_info_metadata.available {
                return Ok(Some(self.get_pep658_metadata(artifact_info).await?));
            }

            // Try to load the data by sparsely reading the artifact (if supported)
            if let Some(metadata) = self.get_lazy_metadata_wheel(artifact_info).await? {
                return Ok(Some((artifact_info, metadata)));
            }

            // Otherwise download the entire artifact
            let artifact = self
                .get_artifact_with_cache::<Wheel>(artifact_info, CacheMode::Default)
                .await?;
            let metadata = artifact.metadata();
            match metadata {
                Ok((blob, metadata)) => {
                    self.put_metadata_in_cache(artifact_info, &blob).await?;
                    return Ok(Some((artifact_info, metadata)));
                }
                Err(err) => {
                    tracing::warn!(
                        "Error reading metadata from artifact '{}' skipping ({:?})",
                        artifact_info.filename,
                        err
                    );
                    continue;
                }
            }
        }
        Ok(None)
    }

    async fn get_metadata_sdists<'a, 'i>(
        &self,
        artifacts: &[&'a ArtifactInfo],
        wheel_builder: &WheelBuilder<'a, 'i>,
    ) -> miette::Result<Option<(&'a ArtifactInfo, WheelCoreMetadata)>> {
        let sdists = artifacts
            .iter()
            .copied()
            .filter(|artifact_info| artifact_info.is::<SDist>());

        for artifact_info in sdists {
            let artifact = self
                .get_artifact_with_cache::<SDist>(artifact_info, CacheMode::Default)
                .await?;
            let metadata = wheel_builder.get_sdist_metadata(&artifact).await;
            match metadata {
                Ok((blob, metadata)) => {
                    self.put_metadata_in_cache(artifact_info, &blob).await?;
                    return Ok(Some((artifact_info, metadata)));
                }
                Err(err) => {
                    tracing::warn!(
                        "Error reading metadata from artifact '{}' skipping ({})",
                        artifact_info.filename,
                        err
                    );
                    continue;
                }
            }
        }
        Ok(None)
    }

    /// Returns the metadata from a set of artifacts. This function assumes that metadata is
    /// consistent for all artifacts of a single version.
    pub async fn get_metadata<'a, 'i>(
        &self,
        artifacts: &[&'a ArtifactInfo],
        wheel_builder: Option<&WheelBuilder<'a, 'i>>,
    ) -> miette::Result<Option<(&'a ArtifactInfo, WheelCoreMetadata)>> {
        // Check if we already have information about any of the artifacts cached.
        // Return if we do
        for artifact_info in artifacts.iter().copied() {
            if let Some(metadata_bytes) = self.metadata_from_cache(artifact_info).await {
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
        let result = self.get_metadata_wheels(artifacts).await?;
        if result.is_some() {
            return Ok(result);
        }

        // No wheels found with metadata, try to get metadata from sdists
        // by building them or using the appropriate hooks
        if let Some(wheel_builder) = wheel_builder {
            let result = self.get_metadata_sdists(artifacts, wheel_builder).await?;
            if result.is_some() {
                return Ok(result);
            }
        }

        // Ok literally nothing seems to work, so we'll just return None
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

        if let Ok(mut reader) = AsyncHttpRangeReader::new(
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
    async fn get_pep658_metadata<'a>(
        &self,
        artifact_info: &'a ArtifactInfo,
    ) -> miette::Result<(&'a ArtifactInfo, WheelCoreMetadata)> {
        // Check if the artifact is the same type as the info.
        WheelFilename::try_as(&artifact_info.filename)
            .expect("the specified artifact does not refer to type requested to read");

        // Turn into PEP658 compliant URL
        let mut url = artifact_info.url.clone();
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
        self.put_metadata_in_cache(artifact_info, &bytes).await?;
        Ok((artifact_info, metadata))
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

            let mut bytes = response.into_body().into_local().await.into_diagnostic()?;
            let mut source = String::new();
            bytes.read_to_string(&mut source).into_diagnostic()?;
            parse_package_names_html(&source)
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
        let name = A::Name::try_as(&artifact_info.filename).unwrap_or_else(|| {
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
        A::new(name.clone(), bytes)
    }

    /// Opens the specified artifact info. Downloads the artifact data from the remote location if
    /// the information is not already cached.
    #[async_recursion]
    pub async fn get_wheel<'db, 'i>(
        &self,
        artifact_info: &ArtifactInfo,
        builder: Option<&'async_recursion WheelBuilder<'db, 'i>>,
    ) -> miette::Result<Wheel> {
        // Try to build the wheel for this SDist if possible
        if artifact_info.is::<SDist>() {
            if let Some(builder) = builder {
                let sdist = self
                    .get_artifact_with_cache::<SDist>(artifact_info, CacheMode::Default)
                    .await?;

                return builder.build_wheel(&sdist).await.into_diagnostic();
            } else {
                miette::bail!("cannot build wheel without a wheel builder");
            }
        }

        // Otherwise just retrieve the wheel
        self.get_artifact_with_cache::<Wheel>(artifact_info, CacheMode::Default)
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
    use crate::types::PackageName;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_available_packages() {
        let cache_dir = TempDir::new().unwrap();
        let package_db = PackageDb::new(
            Client::new(),
            &[Url::parse("https://pypi.org/simple/").unwrap()],
            cache_dir.path(),
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
            .get_metadata(&artifact_info, None)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_pep658() {
        let cache_dir = TempDir::new().unwrap();
        let package_db = PackageDb::new(
            Client::new(),
            &[Url::parse("https://pypi.org/simple/").unwrap()],
            cache_dir.path(),
        )
        .unwrap();

        // Get all the artifacts
        let artifacts = package_db
            .available_artifacts("numpy".parse::<PackageName>().unwrap())
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
