use super::solve::PreReleaseResolution;
use super::SDistResolution;
use crate::artifacts::SDist;
use crate::artifacts::Wheel;
use crate::index::PackageDb;
use crate::python_env::WheelTags;
use crate::resolve::{PinnedPackage, ResolveOptions};
use crate::types::{
    Artifact, ArtifactInfo, ArtifactName, Extra, NormalizedPackageName, PackageName,
};
use crate::wheel_builder::WheelBuilder;
use elsa::FrozenMap;
use itertools::Itertools;
use miette::{Diagnostic, IntoDiagnostic, MietteDiagnostic};
use parking_lot::Mutex;
use pep440_rs::{Operator, Version, VersionSpecifier, VersionSpecifiers};
use pep508_rs::{MarkerEnvironment, Requirement, VersionOrUrl};
use resolvo::{
    Candidates, Dependencies, DependencyProvider, KnownDependencies, NameId, Pool, SolvableId,
    SolverCache, VersionSet,
};
use serde::Deserialize;
use serde::Serialize;
use std::any::Any;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use thiserror::Error;
use tokio::runtime::Handle;
use tokio::task;
use url::Url;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
/// This is a wrapper around Specifiers that implements [`VersionSet`]
pub struct PypiVersionSet {
    /// The spec to match against
    spec: Option<VersionOrUrl>,
    /// If the VersionOrUrl is a Version specifier and any of the specifiers contains a
    /// prerelease, then pre-releases are allowed. For example,
    /// `jupyterlab==3.0.0a1` allows pre-releases, but `jupyterlab==3.0.0` does not.
    ///
    /// We pre-compute if any of the items in the specifiers contains a pre-release and store
    /// this as a boolean which is later used during matching.
    allows_prerelease: bool,
}

impl PypiVersionSet {
    /// Create a PyPiVersionSeet from VersionOrUrl specifier
    pub fn from_spec(spec: Option<VersionOrUrl>, prerelease_option: &PreReleaseResolution) -> Self {
        let allows_prerelease = match prerelease_option {
            PreReleaseResolution::Disallow => false,
            PreReleaseResolution::AllowIfNoOtherVersionsOrEnabled { .. } => match spec.as_ref() {
                Some(VersionOrUrl::VersionSpecifier(v)) => {
                    v.iter().any(|s| s.version().any_prerelease())
                }
                _ => false,
            },
            PreReleaseResolution::Allow => true,
        };

        Self {
            spec,
            allows_prerelease,
        }
    }
}

impl Display for PypiVersionSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.spec {
            None => write!(f, "*"),
            Some(VersionOrUrl::Url(url)) => write!(f, "{url}"),
            Some(VersionOrUrl::VersionSpecifier(spec)) => write!(f, "{spec}"),
        }
    }
}

/// This is a wrapper around [`Version`] that serves a version
/// within the [`PypiVersionSet`] version set.
#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum PypiVersion {
    /// Version of artifact
    Version {
        /// Version of Artifact
        version: Version,

        /// Given that the [`PreReleaseResolution`] is
        /// AllowIfNoOtherVersionsOrEnabled, this field is true if there are
        /// only pre-releases available for this package or if a spec explicitly
        /// enabled pre-releases for this package. For example, if the package
        /// `foo` has only versions `foo-1.0.0a1` and `foo-1.0.0a2` then this
        /// will be true. This allows us later to match against this version and
        /// allow the selection of pre-releases. Additionally, this is also true
        /// if any of the explicitly mentioned specs (by the user) contains a
        /// prerelease (for example c>0.0.0b0) contains the `b0` which signifies
        /// a pre-release.
        package_allows_prerelease: bool,
    },
    /// Direct reference for artifact
    Url(Url),
}

impl PypiVersion {
    /// Return if there are any prereleases for version
    pub fn any_prerelease(&self) -> bool {
        match self {
            PypiVersion::Url(_) => false,
            PypiVersion::Version { version, .. } => version.any_prerelease(),
        }
    }

    /// Return if pypi version is git url version
    pub fn is_git(&self) -> bool {
        match self {
            PypiVersion::Version { .. } => false,
            PypiVersion::Url(url) => url.scheme().contains("git"),
        }
    }
}

impl VersionSet for PypiVersionSet {
    type V = PypiVersion;

    fn contains(&self, v: &Self::V) -> bool {
        match (self.spec.as_ref(), v) {
            (Some(VersionOrUrl::Url(a)), PypiVersion::Url(b)) => a == b,
            (
                Some(VersionOrUrl::VersionSpecifier(spec)),
                PypiVersion::Version {
                    version,
                    package_allows_prerelease,
                },
            ) => {
                spec.contains(version)
                    // pre-releases are allowed only when the versionset allows them (jupyterlab==3.0.0a1)
                    // or there are no other versions available (foo-1.0.0a1, foo-1.0.0a2)
                    // or alternatively if the user has enabled all pre-releases or this specific (this is encoded in the allows_prerelease field)
                    && (self.allows_prerelease || *package_allows_prerelease || !version.any_prerelease())
            }
            (
                None,
                PypiVersion::Version {
                    version,
                    package_allows_prerelease,
                },
            ) => self.allows_prerelease || *package_allows_prerelease || !version.any_prerelease(),
            (None, PypiVersion::Url(_)) => true,
            _ => false,
        }
    }
}

impl Display for PypiVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PypiVersion::Version { version, .. } => write!(f, "{version}"),
            PypiVersion::Url(u) => write!(f, "{u}"),
        }
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
/// This can either be a base package name or with an extra
/// this is used to support optional dependencies
pub(crate) enum PypiPackageName {
    /// Regular dependency
    Base(NormalizedPackageName),
    /// Optional dependency
    Extra(NormalizedPackageName, Extra),
}

impl PypiPackageName {
    /// Returns the actual package (normalized) name without the extra
    pub fn base(&self) -> &NormalizedPackageName {
        match self {
            PypiPackageName::Base(normalized) => normalized,
            PypiPackageName::Extra(normalized, _) => normalized,
        }
    }

    /// Retrieves the extra if it is available
    pub fn extra(&self) -> Option<&Extra> {
        match self {
            PypiPackageName::Base(_) => None,
            PypiPackageName::Extra(_, e) => Some(e),
        }
    }
}

impl Display for PypiPackageName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PypiPackageName::Base(name) => write!(f, "{}", name),
            PypiPackageName::Extra(name, extra) => write!(f, "{}[{}]", name, extra.as_str()),
        }
    }
}

/// This is a [`DependencyProvider`] for PyPI packages
pub(crate) struct PypiDependencyProvider<'db, 'i> {
    pub pool: Pool<PypiVersionSet, PypiPackageName>,
    package_db: &'db PackageDb,
    wheel_builder: WheelBuilder<'db, 'i>,
    markers: &'i MarkerEnvironment,
    compatible_tags: Option<&'i WheelTags>,

    pub cached_artifacts: FrozenMap<SolvableId, Vec<&'db ArtifactInfo>>,

    favored_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
    locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
    pub name_to_url: FrozenMap<NormalizedPackageName, String>,

    options: &'i ResolveOptions,
    should_cancel_with_value: Mutex<Option<MetadataError>>,
}

impl<'db, 'i> PypiDependencyProvider<'db, 'i> {
    /// Creates a new PypiDependencyProvider
    /// for use with the [`resolvo`] crate
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Pool<PypiVersionSet, PypiPackageName>,
        package_db: &'db PackageDb,
        markers: &'i MarkerEnvironment,
        compatible_tags: Option<&'i WheelTags>,
        locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
        favored_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
        name_to_url: FrozenMap<NormalizedPackageName, String>,
        options: &'i ResolveOptions,
        env_variables: HashMap<String, String>,
    ) -> miette::Result<Self> {
        let wheel_builder =
            WheelBuilder::new(package_db, markers, compatible_tags, options, env_variables)
                .into_diagnostic()?;

        Ok(Self {
            pool,
            package_db,
            wheel_builder,
            markers,
            compatible_tags,
            cached_artifacts: Default::default(),
            favored_packages,
            locked_packages,
            name_to_url,
            options,
            should_cancel_with_value: Default::default(),
        })
    }

    fn filter_candidates<'a>(
        &self,
        artifacts: &'a [ArtifactInfo],
    ) -> Result<Vec<&'a ArtifactInfo>, &'static str> {
        // Filter only artifacts we can work with
        if artifacts.is_empty() {
            // If there are no wheel artifacts, we're just gonna skip it
            return Err("there are no packages available");
        }

        let mut artifacts = artifacts.iter().collect::<Vec<_>>();
        // Filter yanked artifacts
        artifacts.retain(|a| !a.yanked.yanked);

        if artifacts.is_empty() {
            return Err("it is yanked");
        }

        // This should keep only the wheels
        let mut wheels = if self.options.sdist_resolution.allow_wheels() {
            let wheels = artifacts
                .iter()
                .filter(|a| a.is::<Wheel>())
                .cloned()
                .collect::<Vec<_>>();

            if !self.options.sdist_resolution.allow_sdists() && wheels.is_empty() {
                return Err("there are no wheels available");
            }

            wheels
        } else {
            vec![]
        };

        // Extract sdists
        let mut sdists = if self.options.sdist_resolution.allow_sdists() {
            let mut sdists = artifacts
                .iter()
                .filter(|a| a.is::<SDist>() || a.filename.as_stree().is_some())
                .cloned()
                .collect::<Vec<_>>();

            if wheels.is_empty() && sdists.is_empty() {
                if self.options.sdist_resolution.allow_wheels() {
                    return Err("there are no wheels or sdists");
                } else {
                    return Err("there are no sdists");
                }
            }

            sdists.retain(|a| {
                a.filename
                    .as_sdist()
                    .is_some_and(|f| f.format.is_supported())
                    || a.filename.as_stree().is_some()
            });

            if wheels.is_empty() && sdists.is_empty() {
                return Err("none of the sdists formats are supported");
            }

            sdists
        } else {
            vec![]
        };

        // Filter based on compatibility
        if self.options.sdist_resolution.allow_wheels() {
            if let Some(compatible_tags) = self.compatible_tags {
                wheels.retain(|artifact| match &artifact.filename {
                    ArtifactName::Wheel(wheel_name) => wheel_name
                        .all_tags_iter()
                        .any(|t| compatible_tags.is_compatible(&t)),
                    ArtifactName::SDist(_) => false,
                    ArtifactName::STree(_) => false,
                });

                // Sort the artifacts from most compatible to least compatible, this ensures that we
                // check the most compatible artifacts for dependencies first.
                // this only needs to be done for wheels
                wheels.sort_by_cached_key(|a| {
                    -a.filename
                        .as_wheel()
                        .expect("only wheels are considered")
                        .all_tags_iter()
                        .filter_map(|tag| compatible_tags.compatibility(&tag))
                        .max()
                        .unwrap_or(0)
                });
            }

            if !self.options.sdist_resolution.allow_sdists() && wheels.is_empty() {
                return Err(
                    "none of the artifacts are compatible with the Python interpreter or glibc version",
                );
            }

            if wheels.is_empty() && sdists.is_empty() {
                return Err("none of the artifacts are compatible with the Python interpreter or glibc version and there are no supported sdists");
            }
        }

        // Append these together
        wheels.append(&mut sdists);
        let artifacts = wheels;

        if artifacts.is_empty() {
            return Err("there are no supported artifacts");
        }

        Ok(artifacts)
    }

    fn solvable_has_artifact_type<S: Artifact>(&self, solvable_id: SolvableId) -> bool {
        self.cached_artifacts
            .get(&solvable_id)
            .unwrap_or(&[])
            .iter()
            .any(|a| a.is::<S>())
    }
}

#[derive(Debug, Error, Diagnostic, Clone)]
pub(crate) enum MetadataError {
    #[error("Extraction of metadata in case of wheels or building in case of sdists returned no results for following artifacts:\n{0}")]
    NoMetadata(String),

    #[error("No metadata could be extracted for the following available artifacts:\n{artifacts}")]
    ExtractionFailure {
        artifacts: String,
        #[related]
        errors: Vec<MietteDiagnostic>,
    },
}

impl<'p> DependencyProvider<PypiVersionSet, PypiPackageName>
    for &'p PypiDependencyProvider<'_, '_>
{
    fn pool(&self) -> &Pool<PypiVersionSet, PypiPackageName> {
        &self.pool
    }

    fn should_cancel_with_value(&self) -> Option<Box<dyn Any>> {
        // Supply the error message
        self.should_cancel_with_value
            .lock()
            .as_ref()
            .map(|s| Box::new(s.clone()) as Box<dyn Any>)
    }

    fn sort_candidates(
        &self,
        solver: &SolverCache<PypiVersionSet, PypiPackageName, Self>,
        solvables: &mut [SolvableId],
    ) {
        solvables.sort_by(|&a, &b| {
            // First sort the solvables based on the artifact types we have available for them and
            // whether some of them are preferred. If one artifact type is preferred over another
            // we sort those versions above the others even if the versions themselves are lower.
            if matches!(self.options.sdist_resolution, SDistResolution::PreferWheels) {
                let a_has_wheels = self.solvable_has_artifact_type::<Wheel>(a);
                let b_has_wheels = self.solvable_has_artifact_type::<Wheel>(b);
                match (a_has_wheels, b_has_wheels) {
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    _ => {}
                }
            } else if matches!(self.options.sdist_resolution, SDistResolution::PreferSDists) {
                let a_has_sdists = self.solvable_has_artifact_type::<SDist>(a);
                let b_has_sdists = self.solvable_has_artifact_type::<SDist>(b);
                match (a_has_sdists, b_has_sdists) {
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    _ => {}
                }
            }

            let solvable_a = solver.pool().resolve_solvable(a);
            let solvable_b = solver.pool().resolve_solvable(b);

            match (&solvable_a.inner(), &solvable_b.inner()) {
                // Sort Urls alphabetically
                // TODO: Do better
                (PypiVersion::Url(a), PypiVersion::Url(b)) => a.cmp(b),

                // Prefer Urls over versions
                (PypiVersion::Url(_), PypiVersion::Version { .. }) => Ordering::Greater,
                (PypiVersion::Version { .. }, PypiVersion::Url(_)) => Ordering::Less,

                // Sort versions from highest to lowest
                (
                    PypiVersion::Version { version: a, .. },
                    PypiVersion::Version { version: b, .. },
                ) => b.cmp(a),
            }
        })
    }

    fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let package_name = self.pool.resolve_package_name(name);
        tracing::info!("collecting {}", package_name);

        // check if we have URL variant for this name
        let url_version = self.name_to_url.get(package_name.base());

        let result = if let Some(url) = url_version {
            task::block_in_place(move || {
                let url = Url::from_str(url).expect("cannot parse back url");
                Handle::current().block_on(self.package_db.get_artifact_by_direct_url(
                    package_name.base().clone(),
                    url,
                    &self.wheel_builder,
                ))
            })
        } else {
            // Get all the metadata for this package
            task::block_in_place(move || {
                Handle::current().block_on(
                    self.package_db
                        .available_artifacts(package_name.base().clone()),
                )
            })
        };

        let artifacts = match result {
            Ok(artifacts) => artifacts,
            Err(err) => {
                tracing::error!(
                    "failed to fetch artifacts of '{package_name}': {err:?}, skipping.."
                );
                return None;
            }
        };
        let mut candidates = Candidates::default();
        let locked_package = self.locked_packages.get(package_name.base());
        let favored_package = self.favored_packages.get(package_name.base());

        let should_package_allow_prerelease = match &self.options.pre_release_resolution {
            PreReleaseResolution::Disallow => false,
            PreReleaseResolution::AllowIfNoOtherVersionsOrEnabled { allow_names } => {
                if allow_names.contains(&package_name.base().to_string()) {
                    true
                } else {
                    // check if we _only_ have prereleases for this name (if yes, also allow them)
                    artifacts
                        .iter()
                        .all(|(version, _)| version.any_prerelease())
                }
            }
            PreReleaseResolution::Allow => true,
        };

        for (artifact_version, artifacts) in artifacts.iter() {
            // Skip this version if a locked or favored version exists for this version. It will be
            // added below.

            match artifact_version {
                PypiVersion::Url(url) => {
                    if locked_package.map(|p| &p.url) == Some(&Some(url.clone()))
                        || favored_package.map(|p| &p.url) == Some(&Some(url.clone()))
                    {
                        continue;
                    }
                }
                PypiVersion::Version { version, .. } => {
                    if locked_package.map(|p| &p.version) == Some(version)
                        || favored_package.map(|p| &p.version) == Some(version)
                    {
                        continue;
                    }
                }
            }

            // Add the solvable
            let internable_version = if let PypiVersion::Version { version, .. } = artifact_version
            {
                PypiVersion::Version {
                    version: version.to_owned(),
                    package_allows_prerelease: should_package_allow_prerelease,
                }
            } else {
                artifact_version.clone()
            };

            let solvable_id = self.pool.intern_solvable(name, internable_version);
            candidates.candidates.push(solvable_id);

            // Determine the candidates
            match self.filter_candidates(artifacts) {
                Ok(artifacts) => {
                    self.cached_artifacts.insert(solvable_id, artifacts);
                }
                Err(reason) => {
                    candidates
                        .excluded
                        .push((solvable_id, self.pool.intern_string(reason)));
                }
            }
        }

        // Add a locked dependency
        if let Some(locked) = self.locked_packages.get(package_name.base()) {
            let version = if let Some(url) = &locked.url {
                PypiVersion::Url(url.clone())
            } else {
                PypiVersion::Version {
                    version: locked.version.clone(),
                    package_allows_prerelease: locked.version.any_prerelease(),
                }
            };
            let solvable_id = self.pool.intern_solvable(name, version);
            candidates.candidates.push(solvable_id);
            candidates.locked = Some(solvable_id);
            self.cached_artifacts
                .insert(solvable_id, locked.artifacts.clone());
        }

        // Add a favored dependency
        if let Some(favored) = self.favored_packages.get(package_name.base()) {
            let version = if let Some(url) = &favored.url {
                PypiVersion::Url(url.clone())
            } else {
                PypiVersion::Version {
                    version: favored.version.clone(),
                    package_allows_prerelease: favored.version.any_prerelease(),
                }
            };
            let solvable_id = self.pool.intern_solvable(name, version);
            candidates.candidates.push(solvable_id);
            candidates.favored = Some(solvable_id);
            self.cached_artifacts
                .insert(solvable_id, favored.artifacts.clone());
        }

        Some(candidates)
    }

    fn get_dependencies(&self, solvable_id: SolvableId) -> Dependencies {
        let solvable = self.pool.resolve_solvable(solvable_id);
        let package_name = self.pool.resolve_package_name(solvable.name_id());
        let package_version = solvable.inner();

        tracing::info!(
            "obtaining dependency information from {}={}",
            package_name,
            package_version
        );

        let mut dependencies = KnownDependencies::default();

        // Add a dependency to the base dependency when we have an extra
        // So that we have a connection to the base package
        if let PypiPackageName::Extra(package_name, _) = package_name {
            let base_name_id = self
                .pool
                .lookup_package_name(&PypiPackageName::Base(package_name.clone()))
                .expect("base package not found while resolving extra");
            let specifiers = match package_version {
                PypiVersion::Version { version, .. } => {
                    VersionOrUrl::VersionSpecifier(VersionSpecifiers::from_iter([
                        VersionSpecifier::new(Operator::ExactEqual, version.clone(), false)
                            .expect("failed to construct equality version specifier"),
                    ]))
                }
                PypiVersion::Url(url_version) => VersionOrUrl::Url(url_version.clone()),
            };

            let version_set_id = self.pool.intern_version_set(
                base_name_id,
                PypiVersionSet::from_spec(Some(specifiers), &self.options.pre_release_resolution),
            );
            dependencies.requirements.push(version_set_id);
        }

        // Retrieve the artifacts that are applicable for this version
        let artifacts = self
            .cached_artifacts
            .get(&solvable_id)
            .expect("the artifacts must already have been cached");

        // If there are no artifacts we can have two cases
        if artifacts.is_empty() {
            // TODO: rework this so it makes more sense from an API perspective later, I think we should add the concept of installed_and_locked or something
            // It is locked the package data may be available externally
            // So it's fine if there are no artifacts, we can just assume this has been taken care of
            let locked_package = self.locked_packages.get(package_name.base());
            match package_version {
                PypiVersion::Url(url) => {
                    if locked_package.map(|p| &p.url) == Some(&Some(url.clone())) {
                        return Dependencies::Known(dependencies);
                    }
                }

                PypiVersion::Version { version, .. } => {
                    if locked_package.map(|p| &p.version) == Some(version) {
                        return Dependencies::Known(dependencies);
                    }
                }
            }

            // Otherwise, we do expect data, and it's not fine if there are no artifacts
            let error = self.pool.intern_string(format!(
                "there are no artifacts available for {}={}",
                package_name, package_version
            ));
            return Dependencies::Unknown(error);
        }

        let result = task::block_in_place(|| {
            // First try getting wheels
            Handle::current().block_on(
                self.package_db
                    .get_metadata(artifacts, Some(&self.wheel_builder)),
            )
        });

        let metadata = match result {
            // We have retrieved a value without error
            Ok(value) => {
                if let Some((_, metadata)) = value {
                    // Return the metadata
                    metadata
                } else {
                    let formatted_artifacts = artifacts
                        .iter()
                        .format_with("\n", |a, f| f(&format_args!("\t- {}", a.filename)))
                        .to_string();
                    // No results have been found with the methods we tried
                    *self.should_cancel_with_value.lock() =
                        Some(MetadataError::NoMetadata(formatted_artifacts));
                    return Dependencies::Unknown(self.pool.intern_string("".to_string()));
                }
            }
            // Errors have occurred during metadata extraction
            // This is almost always an sdist build failure
            Err(e) => {
                let formatted_artifacts = artifacts
                    .iter()
                    .format_with("\n", |a, f| f(&format_args!("\t- {}", a.filename)))
                    .to_string();
                *self.should_cancel_with_value.lock() = Some(MetadataError::ExtractionFailure {
                    artifacts: formatted_artifacts,
                    errors: vec![MietteDiagnostic::new(e.to_string()).with_help("Probably an error during processing of source distributions. Please check the error message above.")],
                });
                return Dependencies::Unknown(self.pool.intern_string("".to_string()));
            }
        };

        // Add constraints that restrict that the extra packages are set to the same version.
        if let PypiPackageName::Base(package_name) = package_name {
            // Add constraints on the extras of a package
            for extra in metadata.extras {
                let extra_name_id = self
                    .pool
                    .intern_package_name(PypiPackageName::Extra(package_name.clone(), extra));

                let specifiers = match package_version {
                    PypiVersion::Version { version, .. } => {
                        VersionOrUrl::VersionSpecifier(VersionSpecifiers::from_iter([
                            VersionSpecifier::new(Operator::ExactEqual, version.clone(), false)
                                .expect("failed to construct equality version specifier"),
                        ]))
                    }
                    PypiVersion::Url(url_version) => VersionOrUrl::Url(url_version.clone()),
                };
                let version_set_id = self.pool.intern_version_set(
                    extra_name_id,
                    PypiVersionSet::from_spec(
                        Some(specifiers),
                        &self.options.pre_release_resolution,
                    ),
                );
                dependencies.constrains.push(version_set_id);
            }
        }

        let extras = package_name
            .extra()
            .into_iter()
            .map(|e| e.as_str())
            .collect::<Vec<_>>();
        for requirement in metadata.requires_dist {
            // Evaluate environment markers
            if let Some(markers) = requirement.marker.as_ref() {
                if !markers.evaluate(self.markers, &extras) {
                    continue;
                }
            }

            // Add the dependency to the pool
            let Requirement {
                name,
                version_or_url,
                extras,
                ..
            } = requirement;
            let name = PackageName::from_str(&name).expect("invalid package name");
            let dependency_name_id = self
                .pool
                .intern_package_name(PypiPackageName::Base(name.clone().into()));

            let version_set_id = self.pool.intern_version_set(
                dependency_name_id,
                PypiVersionSet::from_spec(
                    version_or_url.clone(),
                    &self.options.pre_release_resolution,
                ),
            );

            if let Some(VersionOrUrl::Url(url)) = version_or_url.clone() {
                self.name_to_url
                    .insert(name.clone().into(), url.clone().as_str().to_owned());
            }

            dependencies.requirements.push(version_set_id);

            // Add a unique package for each extra/optional dependency
            for extra in extras.into_iter().flatten() {
                let extra = Extra::from_str(&extra).expect("invalid extra name");
                let dependency_name_id = self
                    .pool
                    .intern_package_name(PypiPackageName::Extra(name.clone().into(), extra));
                let version_set_id = self.pool.intern_version_set(
                    dependency_name_id,
                    PypiVersionSet::from_spec(
                        version_or_url.clone(),
                        &self.options.pre_release_resolution,
                    ),
                );
                dependencies.requirements.push(version_set_id);
            }
        }

        Dependencies::Known(dependencies)
    }
}
