use crate::package_name::PackageName;
use crate::project_info::ArtifactInfo;
use elsa::FrozenMap;
use indexmap::IndexMap;
use pep440::Version;
use reqwest::Client;
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

    // pub fn available_artifacts(
    //     &self,
    //     p: &PackageName,
    // ) -> Result<&IndexMap<Version, Vec<ArtifactInfo>>> {
    //     if let Some(cached) = self.artifacts.get(p) {
    //         Ok(cached)
    //     } else {
    //         let mut result: IndexMap<Version, Vec<ArtifactInfo>> = Default::default();
    //     }
    // }
}
