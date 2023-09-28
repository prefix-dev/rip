use crate::artifact_name::{InnerAsArtifactName, WheelName};
use crate::core_metadata::WheelCoreMetadata;
use crate::package_name::PackageName;
use crate::utils::ReadAndSeek;
use async_trait::async_trait;
use miette::IntoDiagnostic;
use parking_lot::Mutex;
use pep440::Version;
use std::collections::HashSet;
use std::io::Read;
use std::str::FromStr;
use zip::ZipArchive;

/// Trait that represents an artifact type in the PyPi ecosystem.
/// Currently implemented for [`Wheel`] files.
#[async_trait]
pub trait Artifact: Sized {
    /// The name of the artifact which describes the artifact.
    ///
    /// Artifacts are describes by a string. [`super::artifact_name::ArtifactName`] describes the
    /// general format.
    type Name: Clone + InnerAsArtifactName;

    /// Construct a new artifact from the given bytes
    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self>;

    /// Returns the name of this instance
    fn name(&self) -> &Self::Name;
}

/// Wheel file in the PyPi ecosystem.
/// See the [Reference Page](https://packaging.python.org/en/latest/specifications/binary-distribution-format/#binary-distribution-format)
/// for more information.
pub struct Wheel {
    name: WheelName,
    archive: Mutex<ZipArchive<Box<dyn ReadAndSeek + Send>>>,
}

#[async_trait]
impl Artifact for Wheel {
    type Name = WheelName;

    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        Ok(Self {
            name,
            archive: Mutex::new(ZipArchive::new(bytes).into_diagnostic()?),
        })
    }

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

/// Trait that represents an artifact that contains metadata.
/// Currently implemented for [`Wheel`] files.
#[async_trait]
pub trait MetadataArtifact: Artifact {
    /// Associated type for the metadata of this artifact.
    type Metadata;

    /// Parses the metadata associated with an artifact.
    fn parse_metadata(bytes: &[u8]) -> miette::Result<Self::Metadata>;

    /// Parses the metadata from the artifact itself. Also returns the metadata bytes so we can
    /// cache it for later.
    fn metadata(&self) -> miette::Result<(Vec<u8>, Self::Metadata)>;
}

impl Wheel {
    /// A wheel file always contains a special directory that contains the metadata of the package.
    /// This function returns the name of that directory.
    fn find_special_wheel_dir<'a>(
        top_level_names: impl IntoIterator<Item = &'a str>,
        name: &PackageName,
        version: &Version,
        suffix: &str,
    ) -> miette::Result<Option<&'a str>> {
        // Find all directories that end in the suffix
        let mut candidates = top_level_names.into_iter().filter(|dir_name| {
            let Some(candidate) = dir_name.strip_suffix(suffix) else {
                return false;
            };
            let Some((candidate_name, candidate_version)) = candidate.rsplit_once('-') else {
                return false;
            };

            let Ok(candidate_name) = PackageName::from_str(candidate_name) else {
                return false;
            };
            let Some(candidate_version) = Version::parse(candidate_version) else {
                return false;
            };

            &candidate_name == name && &candidate_version == version
        });

        // Get the first candidate
        let candidate = match candidates.next() {
            Some(candidate) => candidate,
            None => return Ok(None),
        };

        // Error out if there are multiple directories
        if candidates.next().is_some() {
            miette::bail!("found multiple {suffix}/ directories in wheel");
        }

        Ok(Some(candidate))
    }
}

#[async_trait]
impl MetadataArtifact for Wheel {
    type Metadata = WheelCoreMetadata;

    fn parse_metadata(bytes: &[u8]) -> miette::Result<Self::Metadata> {
        WheelCoreMetadata::try_from(bytes)
    }

    fn metadata(&self) -> miette::Result<(Vec<u8>, Self::Metadata)> {
        let mut archive = self.archive.lock();

        // Determine the top level filenames in the wheel
        let top_level_names = archive
            .file_names()
            .map(|filename| {
                filename
                    .split_once(['/', '\\'])
                    .map_or_else(|| filename, |(base, _)| base)
            })
            .collect::<HashSet<_>>();

        // Determine the name of the dist-info directory
        let dist_info = Wheel::find_special_wheel_dir(
            top_level_names.iter().copied(),
            &self.name.distribution,
            &self.name.version,
            ".dist-info",
        )?
        .ok_or_else(|| miette::miette!(".dist-info/ missing"))?
        .to_owned();

        // // Determine the name of the data directory
        // let data = Wheel::find_special_wheel_dir(
        //     top_level_names,
        //     &self.name.distribution,
        //     &self.name.version,
        //     ".data",
        // )?
        // .map_or_else(
        //     || format!("{}.data", dist_info.strip_suffix(".dist-info").unwrap()),
        //     ToOwned::to_owned,
        // );

        // let wheel_path = format!("{dist_info}/WHEEL");
        // let wheel_metadata = read_entry_to_end(&mut archive, &wheel_path).await?;
        //
        // // TODO: Verify integrity of wheel metadata

        let metadata_path = format!("{dist_info}/METADATA");
        let metadata_bytes = read_entry_to_end(&mut archive, &metadata_path)?;
        let metadata = Self::parse_metadata(&metadata_bytes)?;

        Ok((metadata_bytes, metadata))
    }
}

/// Helper method to read a particular file from a zip archive.
fn read_entry_to_end<R: ReadAndSeek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> miette::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .map_err(|_| miette::miette!("could not find {name} in wheel file"))?
        .read_to_end(&mut bytes)
        .into_diagnostic()?;

    Ok(bytes)
}
