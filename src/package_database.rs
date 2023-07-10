use crate::package_name::PackageName;
use crate::project_info::{ArtifactInfo, ProjectInfo};
use elsa::FrozenMap;
use futures::{pin_mut, stream, StreamExt};
use indexmap::IndexMap;
use pep440::Version;
use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use url::Url;

pub struct PackageDb {
    /// The `reqwest` client to download data with
    client: Client,

    /// Index URLS to query
    index_urls: Vec<Url>,

    /// A cache of package name to version to artifacts.
    artifacts: FrozenMap<PackageName, Box<IndexMap<Version, Vec<ArtifactInfo>>>>,
}

impl PackageDb {
    /// Constructs a new [`PackageDb`] that reads information from the specified URLs.
    pub fn new(client: Client, index_urls: &[Url]) -> std::io::Result<Self> {
        Ok(Self {
            client,
            index_urls: index_urls.into(),
            artifacts: Default::default(),
        })
    }

    /// Downloads and caches information about available artifiacts of a package from the index.
    pub async fn available_artifacts(
        &self,
        p: &PackageName,
    ) -> reqwest::Result<&IndexMap<Version, Vec<ArtifactInfo>>> {
        if let Some(cached) = self.artifacts.get(p) {
            Ok(cached)
        } else {
            // Start downloading the information for each url.
            let client = self.client.clone();
            let request_iter = stream::iter(self.index_urls.iter())
                .map(|url| url.join(&format!("{}/", p.as_str())).expect("invalid url"))
                .map(|url| fetch_simple_api(client.clone(), url))
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
}

async fn fetch_simple_api(client: Client, url: Url) -> reqwest::Result<Option<ProjectInfo>> {
    dbg!(&url);

    let request = client
        .get(url)
        .header(CACHE_CONTROL, "max-age=0")
        .header(ACCEPT, "application/vnd.pypi.simple.v1+json")
        .send()
        .await?;

    // If the resource could not be found we simply return.
    if request.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    // Convert the information from json
    request.error_for_status()?.json().await.map(Some)
}

#[cfg(test)]
mod test {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_available_packages() {
        let cache_dir = TempDir::new().unwrap();
        let package_db = PackageDb::new(
            Client::new(),
            &[Url::parse("https://pypi.org/simple/").unwrap()],
        )
        .unwrap();

        let artifacts = package_db
            .available_artifacts(&"flask".parse().unwrap())
            .await
            .unwrap();

        dbg!(artifacts);
    }
}
