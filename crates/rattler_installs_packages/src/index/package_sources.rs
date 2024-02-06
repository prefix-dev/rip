use crate::types::NormalizedPackageName;
use miette::Diagnostic;
use std::collections::BTreeMap;
use thiserror::Error;
use url::Url;

struct PackageSource {
    alias: String,
    url: Url,
}

#[derive(Debug, Error, Diagnostic)]
pub enum PackageSourceError {
    #[error("duplicate index alias '{0}'")]
    DuplicateAlias(String),
    #[error("unknown index alias '{0}'")]
    UnknownAlias(String),
    #[error("duplicate package-source map entry '{0}'")]
    DuplicatePackageSource(NormalizedPackageName),
}

/// "Builder" pattern for creating a [`PackageSources`] instance
pub struct PackageSourcesBuilder {
    base_source: Url,
    extra_sources: Vec<PackageSource>,
    overrides: BTreeMap<NormalizedPackageName, String>,
}

impl PackageSourcesBuilder {
    /// Start building a new [`PackageSources`] instance, with the given base index URL
    /// This URL will be used for all packages by default
    pub fn new(base_index_url: Url) -> Self {
        Self {
            base_source: base_index_url,
            extra_sources: Default::default(),
            overrides: Default::default(),
        }
    }

    /// Add another index URL
    pub fn with_index(mut self, alias: &str, url: &Url) -> Self {
        self.extra_sources.push(PackageSource {
            alias: alias.to_string(),
            url: url.clone(),
        });
        self
    }

    /// Add an override for a specific package. This will cause the package to be installed
    /// from the given source and from that source only
    pub fn with_override(mut self, package: NormalizedPackageName, alias: &str) -> Self {
        self.overrides.insert(package, alias.to_string());
        self
    }

    /// Finalize the builder and create a `PackageSources` instance
    pub fn build(&self) -> Result<PackageSources, PackageSourceError> {
        let mut extra_sources_map = BTreeMap::new();
        self.extra_sources
            .iter()
            .enumerate()
            .map(|(i, source)| (source.alias.clone(), i))
            .try_for_each(|(alias, index)| {
                if extra_sources_map.insert(alias.clone(), index).is_some() {
                    return Err(PackageSourceError::DuplicateAlias(alias));
                }

                Ok(())
            })?;

        let mut artifact_to_index = BTreeMap::new();
        self.overrides.iter().try_for_each(|(package, source)| {
            let index = *extra_sources_map
                .get(source)
                .ok_or_else(|| PackageSourceError::UnknownAlias(source.clone()))?;

            if artifact_to_index.insert(package.clone(), index).is_some() {
                return Err(PackageSourceError::DuplicatePackageSource(package.clone()));
            }

            Ok(())
        })?;

        let index_url = self.base_source.clone();
        let extra_index_urls = self
            .extra_sources
            .iter()
            .map(|source| source.url.clone())
            .collect();

        Ok(PackageSources {
            index_urls: (index_url, extra_index_urls),
            artifact_to_index,
        })
    }
}

/// A collection of package sources and source overrides.
/// See [`PackageSourcesBuilder`] for creating an instance of this type.
pub struct PackageSources {
    index_urls: (Url, Vec<Url>),
    artifact_to_index: BTreeMap<NormalizedPackageName, usize>,
}

impl PackageSources {
    /// Get the index URL for a package
    pub fn index_url(&self, package: &NormalizedPackageName) -> Vec<&Url> {
        let maybe_index = self
            .artifact_to_index
            .get(package)
            .map(|&index| &self.index_urls.1[index]);

        if let Some(url) = maybe_index {
            vec![url]
        } else {
            std::iter::once(&self.index_urls.0)
                .chain(&self.index_urls.1)
                .collect()
        }
    }

    /// Get the default (fallback) index URL
    pub fn default_index_url(&self) -> Url {
        self.index_urls.0.clone()
    }
}

impl From<Url> for PackageSources {
    fn from(url: Url) -> Self {
        PackageSources {
            index_urls: (url, vec![]),
            artifact_to_index: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PackageName;
    use std::str::FromStr;
    use url::Url;

    #[test]
    fn test_from_url() {
        let url = Url::parse("https://example.com").unwrap();
        let sources = PackageSources::from(url.clone());
        assert_eq!(sources.default_index_url(), url);
    }

    #[test]
    fn test_package_sources_builder() {
        let base_url = Url::parse("https://example.com").unwrap();
        let foo_url = Url::parse("https://foo.com").unwrap();
        let bar_url = Url::parse("https://bar.com").unwrap();

        let name = |name: &str| NormalizedPackageName::from(PackageName::from_str(name).unwrap());

        let sources = PackageSourcesBuilder::new(base_url.clone())
            .with_index("foo", &foo_url)
            .with_index("bar", &bar_url)
            .with_override(name("pkg1"), "foo")
            .with_override(name("pkg2"), "bar")
            .build()
            .unwrap();

        assert_eq!(sources.index_url(&name("pkg1")), vec![&foo_url]);
        assert_eq!(sources.index_url(&name("pkg2")), vec![&bar_url]);
        assert_eq!(
            sources.index_url(&name("pkg3")),
            vec![&base_url, &foo_url, &bar_url]
        );
    }
}
